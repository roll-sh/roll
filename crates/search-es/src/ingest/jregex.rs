//! Java regular expressions for ingest processors (`gsub`, `split`, `kv`).
//!
//! Elasticsearch compiles these with `java.util.regex.Pattern`, whose semantics differ from the
//! `regex` crate's in ways that change documents silently: `\d`, `\w`, `\s` and `\b` are ASCII,
//! `.` stops at every Java line terminator, and Java has lookaround and backreferences. The Java
//! engine roll has lives in `search-analysis` (its `pattern_replace` char filter runs
//! `Matcher.replaceAll`), and its public surface is the analysis chain, so patterns run through a
//! one-off normalizer made of that char filter:
//!
//! - [`JavaReplace`] is `Matcher.replaceAll(replacement)` with Java's `$n`/`${name}`/`\`
//!   replacement syntax and errors.
//! - [`JavaPattern`] finds `Matcher.find()`'s matches by replacing each with itself wrapped in
//!   two Unicode noncharacters that don't occur in the input (`\u{FDD0}$0\u{FDD1}`), then reading
//!   the match bounds back out of the result. On top of that it implements `Pattern.split(input,
//!   limit)`: no leading empty string for a zero-width match at the start, trailing empty
//!   strings removed when `limit == 0`, at most `limit` parts when it is positive.
//!
//! Noncharacters are reserved for internal use and never appear in interchange text; an input
//! that does contain the default pair falls back to another pair, built on first use.

use std::sync::{Arc, Mutex};

use search_analysis::{Analysis, IndexOptions, Normalizer};

use crate::error::{EsError, EsResult};

/// The noncharacters U+FDD0..U+FDEF, paired up as match markers.
const MARKERS: [(char, char); 16] = {
    let mut out = [('\u{FDD0}', '\u{FDD1}'); 16];
    let mut i = 0;
    while i < 16 {
        out[i] = (char::from_u32(0xFDD0 + 2 * i as u32).unwrap(), char::from_u32(0xFDD1 + 2 * i as u32).unwrap());
        i += 1;
    }
    out
};

/// A normalizer whose only component is `pattern_replace(pattern, replacement)`.
fn replacer(pattern: &str, replacement: &str) -> Result<Normalizer, search_analysis::AnalysisError> {
    // The char filter requires a pattern; `(?:)` is the empty pattern's equivalent.
    let pattern = if pattern.is_empty() { "(?:)" } else { pattern };
    let settings = serde_json::json!({
        "char_filter": { "p": { "type": "pattern_replace", "pattern": pattern, "replacement": replacement } },
        "normalizer": { "n": { "type": "custom", "char_filter": ["p"] } },
    });
    let analysis = Analysis::from_settings(&settings, &IndexOptions::default())?;
    analysis.normalizer("n").ok_or_else(|| search_analysis::AnalysisError::illegal("pattern normalizer missing"))
}

/// The normalizer's output, or its analysis-time error (a bad group reference).
fn run(n: &Normalizer, text: &str) -> EsResult<String> {
    let mut out: Option<String> = None;
    n.analyzer()
        .try_analyze(text, &mut |t| {
            if out.is_none() {
                out = Some(t.term.to_string());
            }
        })
        .map_err(crate::analysis::es_error)?;
    Ok(out.unwrap_or_default())
}

/// Elasticsearch's text for a pattern that fails to compile: Java's `PatternSyntaxException`
/// message (description, then the pattern and a caret on following lines).
pub fn syntax_message(e: &search_analysis::AnalysisError) -> String {
    e.reason.clone()
}

/// `Matcher.replaceAll(replacement)` for one pattern and replacement.
pub struct JavaReplace {
    normalizer: Normalizer,
}

impl std::fmt::Debug for JavaReplace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JavaReplace")
    }
}

impl JavaReplace {
    pub fn new(pattern: &str, replacement: &str) -> Result<JavaReplace, search_analysis::AnalysisError> {
        Ok(JavaReplace { normalizer: replacer(pattern, replacement)? })
    }

    pub fn replace_all(&self, text: &str) -> EsResult<String> {
        run(&self.normalizer, text)
    }
}

/// A compiled Java pattern that reports its matches (`Matcher.find()` iteration).
pub struct JavaPattern {
    source: String,
    /// One normalizer per marker pair, built when an input needs it (index 0 at compile time).
    marked: Mutex<Vec<Option<Arc<Normalizer>>>>,
}

impl std::fmt::Debug for JavaPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JavaPattern({:?})", self.source)
    }
}

impl JavaPattern {
    pub fn compile(pattern: &str) -> Result<JavaPattern, search_analysis::AnalysisError> {
        let (a, b) = MARKERS[0];
        let first = replacer(pattern, &format!("{a}$0{b}"))?;
        let mut marked = vec![None; MARKERS.len()];
        marked[0] = Some(Arc::new(first));
        Ok(JavaPattern { source: pattern.to_string(), marked: Mutex::new(marked) })
    }

    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// The byte ranges of `Matcher.find()`'s successive matches in `text`.
    pub fn find_all(&self, text: &str, out: &mut Vec<(usize, usize)>) -> EsResult<()> {
        out.clear();
        let Some(i) = MARKERS.iter().position(|(a, b)| !text.contains(*a) && !text.contains(*b)) else {
            return Err(EsError::illegal_argument("input contains every regex match marker"));
        };
        let (open, close) = MARKERS[i];
        let normalizer = {
            let mut g = self.marked.lock().unwrap_or_else(|e| e.into_inner());
            match &g[i] {
                Some(n) => n.clone(),
                None => {
                    let n = Arc::new(replacer(&self.source, &format!("{open}$0{close}")).map_err(crate::analysis::es_error)?);
                    g[i] = Some(n.clone());
                    n
                }
            }
        };
        let marked = run(&normalizer, text)?;
        // `marked` is `text` with every match wrapped in the markers; map back to `text` offsets.
        let mut pos = 0usize; // byte offset in `text`
        let mut start = None;
        for c in marked.chars() {
            if c == open {
                start = Some(pos);
            } else if c == close {
                if let Some(s) = start.take() {
                    out.push((s, pos));
                }
            } else {
                pos += c.len_utf8();
            }
        }
        Ok(())
    }

    /// `Pattern.split(text, limit)` (and `String.split(regex, limit)`).
    pub fn split(&self, text: &str, limit: i32) -> EsResult<Vec<String>> {
        let mut matches = Vec::new();
        self.find_all(text, &mut matches)?;
        Ok(split_at(text, &matches, limit))
    }
}

/// `Pattern.split`'s rules over the matches `Matcher.find()` reported.
fn split_at(text: &str, matches: &[(usize, usize)], limit: i32) -> Vec<String> {
    let mut parts: Vec<&str> = Vec::new();
    let mut index = 0usize;
    let limited = limit > 0;
    for &(s, e) in matches {
        if limited && parts.len() as i64 >= i64::from(limit) - 1 {
            break;
        }
        // No leading empty substring for a zero-width match at the beginning.
        if index == 0 && s == 0 && s == e {
            continue;
        }
        parts.push(&text[index..s]);
        index = e;
    }
    if index == 0 && parts.is_empty() {
        return vec![text.to_string()];
    }
    parts.push(&text[index..]);
    if limit == 0 {
        while parts.last().is_some_and(|p| p.is_empty()) {
            parts.pop();
        }
    }
    parts.into_iter().map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(p: &str, s: &str, limit: i32) -> Vec<String> {
        JavaPattern::compile(p).unwrap().split(s, limit).unwrap()
    }

    fn gsub(p: &str, s: &str, r: &str) -> String {
        JavaReplace::new(p, r).unwrap().replace_all(s).unwrap()
    }

    // Expected values are Java's (`String.split`, `replaceAll`), as Elasticsearch 9.5.5's
    // `_ingest/pipeline/_simulate` returns them.
    #[test]
    fn java_classes_are_ascii() {
        assert_eq!(gsub("\\d", "1٣２", "#"), "#٣２");
        assert_eq!(gsub("\\w+", "héllo wörld", "W"), "WéW WöW");
        assert_eq!(gsub("\\s", "a\u{a0}b c", "_"), "a\u{a0}b_c");
        assert_eq!(gsub("(a)\\1", "aab", "X"), "Xb");
        assert_eq!(gsub("(?<x>b)", "abc", "[${x}]"), "a[b]c");
        assert_eq!(gsub("x*", "abc", "-"), "-a-b-c-");
        assert_eq!(gsub("x*", "", "-"), "-");
        assert_eq!(gsub("a", "", "-"), "");
    }

    #[test]
    fn bad_replacement_fails_when_something_matches() {
        let r = JavaReplace::new("a", "$5").unwrap();
        assert_eq!(r.replace_all("bcd").unwrap(), "bcd");
        let e = r.replace_all("abc").unwrap_err();
        assert_eq!(e.reason(), "No group 5");
    }

    #[test]
    fn splits_like_java() {
        assert_eq!(split("(?=b)", "abab", 0), ["a", "ba", "b"]);
        assert_eq!(split("", "ab,", 0), ["a", "b", ","]);
        assert_eq!(split("", "ab,", -1), ["a", "b", ",", ""]);
        assert_eq!(split("x*", "abc", 0), ["a", "b", "c"]);
        assert_eq!(split(",", "a,b,,", 0), ["a", "b"]);
        assert_eq!(split(",", "a,b,,", -1), ["a", "b", "", ""]);
        assert_eq!(split(",", ",a", 0), ["", "a"]);
        assert_eq!(split(",", "", 0), [""]);
        assert_eq!(split(",", ",,", 0), Vec::<String>::new());
        assert_eq!(split("=", "a=b=c", 2), ["a", "b=c"]);
        assert_eq!(split("=", "a", 2), ["a"]);
        assert_eq!(split("=", "=b", 2), ["", "b"]);
        assert_eq!(split("\\s", "a b\u{2003}c", 0), ["a", "b\u{2003}c"]);
        assert_eq!(split("é", "aébéc", 0), ["a", "b", "c"]);
    }

    #[test]
    fn inputs_containing_markers() {
        let p = JavaPattern::compile(",").unwrap();
        let s = "a\u{FDD0},b\u{FDD1}";
        assert_eq!(p.split(s, 0).unwrap(), ["a\u{FDD0}", "b\u{FDD1}"]);
    }

    #[test]
    fn syntax_errors_are_javas() {
        let e = JavaPattern::compile("(").unwrap_err();
        assert_eq!(e.kind, "pattern_syntax_exception");
        assert_eq!(syntax_message(&e), "Unclosed group near index 1\n(");
    }

    #[test]
    fn long_inputs() {
        let s = "ab,".repeat(50_000);
        assert_eq!(split(",", &s, 0).len(), 50_000);
        assert_eq!(gsub(",", &s, "").len(), 100_000);
    }
}
