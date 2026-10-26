//! Java regular expressions for ingest processors (`gsub`, `split`, `kv`).
//!
//! Elasticsearch compiles these with `java.util.regex.Pattern`, whose semantics differ from the
//! `regex` crate's in ways that change documents silently: `\d`, `\w`, `\s` and `\b` are ASCII,
//! `.` stops at every Java line terminator, and Java has lookaround and backreferences. The Java
//! engine lives in `search-analysis` ([`search_analysis::jregex`]); this module adapts it to the
//! ingest processors:
//!
//! - [`JavaReplace`] is `Matcher.replaceAll(replacement)` with Java's `$n`/`${name}`/`\`
//!   replacement syntax and errors (a bad replacement fails only once something matched).
//! - [`JavaPattern`] iterates `Matcher.find()`'s matches and implements `Pattern.split(input,
//!   limit)`: no leading empty string for a zero-width match at the start, trailing empty
//!   strings removed when `limit == 0`, at most `limit` parts when it is positive.
//!
//! Compiled patterns are shared by every document a pipeline processes, so the per-match scratch
//! (`Captures`) is made per call: one small allocation per value, none per match.

use search_analysis::jregex::{self as engine, Finder, JavaRegex, Replacement};

use crate::error::EsResult;

/// Elasticsearch's text for a pattern that fails to compile: Java's `PatternSyntaxException`
/// message (description, then the pattern and a caret on following lines).
pub fn syntax_message(e: &search_analysis::AnalysisError) -> String {
    e.reason.clone()
}

/// `Matcher.replaceAll(replacement)` for one pattern and replacement.
pub struct JavaReplace {
    re: JavaRegex,
    rep: Replacement,
}

impl std::fmt::Debug for JavaReplace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JavaReplace({:?})", self.re.as_str())
    }
}

impl JavaReplace {
    pub fn new(pattern: &str, replacement: &str) -> Result<JavaReplace, search_analysis::AnalysisError> {
        let re = engine::compile(pattern, 0)?;
        let rep = Replacement::parse(replacement, &re);
        Ok(JavaReplace { re, rep })
    }

    pub fn replace_all(&self, text: &str) -> EsResult<String> {
        let mut caps = self.re.captures();
        let mut out = String::with_capacity(text.len());
        self.rep.replace(&self.re, text, true, &mut caps, &mut out).map_err(crate::analysis::es_error)?;
        Ok(out)
    }
}

/// A compiled Java pattern that reports its matches (`Matcher.find()` iteration).
pub struct JavaPattern {
    re: JavaRegex,
}

impl std::fmt::Debug for JavaPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JavaPattern({:?})", self.re.as_str())
    }
}

impl JavaPattern {
    pub fn compile(pattern: &str) -> Result<JavaPattern, search_analysis::AnalysisError> {
        Ok(JavaPattern { re: engine::compile(pattern, 0)? })
    }

    pub fn as_str(&self) -> &str {
        self.re.as_str()
    }

    /// The byte ranges of `Matcher.find()`'s successive matches in `text`.
    pub fn find_all(&self, text: &str, out: &mut Vec<(usize, usize)>) -> EsResult<()> {
        out.clear();
        let mut caps = self.re.captures();
        let mut finder = Finder::new();
        while let Some(m) = finder.next(&self.re, text, &mut caps, false) {
            out.push(m);
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
    fn noncharacters_are_ordinary_text() {
        let p = JavaPattern::compile(",").unwrap();
        let s = "a\u{FDD0},b\u{FDD1}";
        assert_eq!(p.split(s, 0).unwrap(), ["a\u{FDD0}", "b\u{FDD1}"]);
        assert_eq!(gsub("\u{FDD0}", "x\u{FDD0}y", "-"), "x-y");
    }

    #[test]
    fn empty_pattern_matches_everywhere() {
        assert_eq!(gsub("", "ab", "-"), "-a-b-");
        assert_eq!(split("", "", 0), [""]);
    }

    #[test]
    fn unfinished_range_is_javas_error() {
        // Java: `Pattern.compile("[a-")` (no caret line: the index is the pattern's length).
        let e = JavaReplace::new("[a-", "").unwrap_err();
        assert_eq!(syntax_message(&e), "Illegal character range near index 3\n[a-");
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
