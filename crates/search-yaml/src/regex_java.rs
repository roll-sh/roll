//! Java `java.util.regex` patterns on Rust's `regex` crate.
//!
//! The suites write Java patterns: `match: { $body: "/^ green \s+ \d+ $/" }` is compiled with
//! `Pattern.COMMENTS` and searched with `find()`, `catch: /reason/` and the warning regexes
//! without flags. The two dialects agree on everything the suites use except a few Java-only
//! escapes, which are rewritten here:
//!
//! - `\Q...\E` literal quoting becomes escaped literals;
//! - `$` and `\Z` match at the end of input *or before a final line terminator* in Java (without
//!   `MULTILINE`), so `/^green$/` finds `"green\n"`; both become `(?:<terminator>?\z)`;
//! - `\d`, `\s`, `\w` (and their negations) are ASCII-only in Java, Unicode in Rust: they become
//!   explicit ASCII classes;
//! - `\h` (horizontal whitespace) becomes its character class;
//! - `.` excludes every Java line terminator (`\n`, `\r`, U+0085, U+2028, U+2029) unless
//!   `(?s)` is on, where Rust's excludes only `\n`: it becomes the explicit class;
//! - possessive quantifiers (`a*+`) become greedy ones (same result for the patterns in use).
//!
//! Compiled patterns are cached per worker: the same few hundred patterns run thousands of times.

use std::collections::HashMap;

use regex::{Regex, RegexBuilder};

/// Translates a Java pattern into Rust syntax.
pub fn translate(java: &str) -> String {
    let mut out = String::with_capacity(java.len() + 8);
    let chars: Vec<char> = java.chars().collect();
    let mut i = 0;
    let mut in_class = 0usize;
    // With an inline MULTILINE flag `$` keeps its per-line meaning (Rust's `(?m)$` is close).
    let multiline = has_inline_flag(java, 'm');
    // With DOTALL `.` matches everything in both dialects, and Rust honours the flag itself.
    let dotall = has_inline_flag(java, 's');
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            let n = chars[i + 1];
            match n {
                'Q' => {
                    i += 2;
                    let mut lit = String::new();
                    while i < chars.len() {
                        if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] == 'E' {
                            i += 2;
                            break;
                        }
                        lit.push(chars[i]);
                        i += 1;
                    }
                    for ch in lit.chars() {
                        // Spaces must be escaped too: the pattern may be compiled in verbose mode.
                        if ch.is_whitespace() || ch == '#' || regex_syntax_meta(ch) {
                            out.push('\\');
                        }
                        out.push(ch);
                    }
                    continue;
                }
                'Z' if in_class == 0 => {
                    out.push_str(END_BUT_FOR_TERMINATOR);
                    i += 2;
                    continue;
                }
                'd' | 'D' | 's' | 'S' | 'w' | 'W' => {
                    let (set, negated) = match n {
                        'd' => (DIGIT, false),
                        'D' => (DIGIT, true),
                        's' => (SPACE, false),
                        'S' => (SPACE, true),
                        'w' => (WORD, false),
                        _ => (WORD, true),
                    };
                    match (in_class > 0, negated) {
                        (true, false) => out.push_str(set),
                        // Rust allows nested classes: `[a[^0-9]]`.
                        _ => {
                            out.push_str(if negated { "[^" } else { "[" });
                            out.push_str(set);
                            out.push(']');
                        }
                    }
                    i += 2;
                    continue;
                }
                'h' => {
                    let class = "\\t \\x{A0}\\x{1680}\\x{180E}\\x{2000}-\\x{200A}\\x{202F}\\x{205F}\\x{3000}";
                    if in_class > 0 {
                        out.push_str(class);
                    } else {
                        out.push('[');
                        out.push_str(class);
                        out.push(']');
                    }
                    i += 2;
                    continue;
                }
                _ => {
                    // Java lets any non-letter be escaped to stand for itself. Rust gives `\<`
                    // and `\>` a meaning (word boundaries) and rejects escapes of characters
                    // that aren't its metacharacters, so those become plain literals.
                    if n.is_ascii_alphanumeric() || n.is_whitespace() || regex_syntax_meta(n) || n == '#' {
                        out.push(c);
                    }
                    out.push(n);
                    i += 2;
                    continue;
                }
            }
        }
        match c {
            '$' if in_class == 0 && !multiline => {
                out.push_str(END_BUT_FOR_TERMINATOR);
                i += 1;
                continue;
            }
            '.' if in_class == 0 && !dotall => {
                out.push_str(ANY_BUT_TERMINATOR);
                i += 1;
                continue;
            }
            '[' => in_class += 1,
            ']' if in_class > 0 => in_class -= 1,
            '+' if in_class == 0 && i > 0 && matches!(chars[i - 1], '*' | '+' | '?' | '}') && !escaped_at(&chars, i - 1) => {
                // Possessive quantifier: drop the `+`.
                i += 1;
                continue;
            }
            _ => {}
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Java's `$`/`\Z` without `MULTILINE`: the end of input, or just before a line terminator
/// that ends the input (`\n`, `\r\n`, `\r`, U+0085, U+2028, U+2029).
const END_BUT_FOR_TERMINATOR: &str = "(?:(?:\\r\\n|[\\n\\r\\x{85}\\x{2028}\\x{2029}])?\\z)";
/// Java's `.` without `DOTALL`: anything but a line terminator.
const ANY_BUT_TERMINATOR: &str = "[^\\n\\r\\x{85}\\x{2028}\\x{2029}]";
/// Java's ASCII `\d`, `\s` (`[ \t\n\x0B\f\r]`) and `\w`, written to survive verbose mode.
const DIGIT: &str = "0-9";
const SPACE: &str = "\\x20\\t\\n\\x0B\\x0C\\r";
const WORD: &str = "a-zA-Z0-9_";

/// Whether an inline flag group (`(?m)`, `(?im:...)`) turns `flag` on.
fn has_inline_flag(pattern: &str, flag: char) -> bool {
    let mut rest = pattern;
    while let Some(i) = rest.find("(?") {
        let after = &rest[i + 2..];
        let flags: String = after.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        let on = flags.split('-').next().unwrap_or("");
        if on.contains(flag) && after[flags.len()..].starts_with([')', ':']) {
            return true;
        }
        rest = after;
    }
    false
}

fn escaped_at(chars: &[char], idx: usize) -> bool {
    let mut n = 0;
    let mut j = idx;
    while j > 0 && chars[j - 1] == '\\' {
        n += 1;
        j -= 1;
    }
    n % 2 == 1
}

fn regex_syntax_meta(c: char) -> bool {
    matches!(c, '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '-' | '&' | '~')
}

/// A cache of compiled patterns keyed by (pattern, verbose).
#[derive(Default)]
pub struct RegexCache {
    map: HashMap<(String, bool), Result<Regex, String>>,
}

impl RegexCache {
    /// Compiles (or fetches) a Java pattern; `comments` is `Pattern.COMMENTS`.
    pub fn get(&mut self, pattern: &str, comments: bool) -> Result<&Regex, String> {
        let key = (pattern.to_owned(), comments);
        let entry = self.map.entry(key).or_insert_with(|| {
            RegexBuilder::new(&translate(pattern))
                .ignore_whitespace(comments)
                .size_limit(64 << 20)
                .build()
                .map_err(|e| format!("invalid regex [{pattern}]: {e}"))
        });
        entry.as_ref().map_err(Clone::clone)
    }

    /// `Pattern.compile(pattern, flags).matcher(text).find()`.
    pub fn find(&mut self, pattern: &str, comments: bool, text: &str) -> Result<bool, String> {
        Ok(self.get(pattern, comments)?.is_match(text))
    }

    /// `Pattern.compile(pattern).matcher(text).matches()`: the whole text must match.
    pub fn full_match(&mut self, pattern: &str, text: &str) -> Result<bool, String> {
        let anchored = format!("\\A(?:{pattern})\\z");
        Ok(self.get(&anchored, false)?.is_match(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbose_find_like_java() {
        let mut c = RegexCache::default();
        let cat = "/^  epoch  \\s+  timestamp \\s+ count \\s+ \\n  \\d+ \\s+ \\d{2}:\\d{2}:\\d{2} \\s+ \\d+ \\s+ \\n $/";
        let pat = &cat[1..cat.len() - 1];
        assert!(c.find(pat, true, "epoch      timestamp count \n1700000000 10:11:12  0     \n").unwrap());
        assert!(!c.find(pat, true, "epoch timestamp\n").unwrap());
        // find(), not matches(): a substring is enough.
        assert!(c.find("not_found", true, "index_not_found_exception").unwrap());
        // Comments run to the end of the line.
        assert!(c.find("^ a # comment\n b $", true, "ab").unwrap());
        // Whitespace inside classes is ignored in verbose mode, as in Java.
        assert!(c.find("^[ a]+$", true, "aaa").unwrap());
        assert!(!c.find("^[ a]+$", true, "a a").unwrap());
    }

    #[test]
    fn java_only_syntax() {
        let mut c = RegexCache::default();
        assert!(c.find("\\Qa.b*c\\E", false, "xa.b*cx").unwrap());
        assert!(!c.find("\\Qa.b\\E", false, "axb").unwrap());
        assert!(c.find("\\Qa b\\E", true, "a b").unwrap());
        assert!(c.find("abc\\Z", false, "abc\n").unwrap());
        assert!(!c.find("abc\\Z", false, "abc\nx").unwrap());
        // `$` allows one final line terminator, but not two, and is literal in a class.
        assert!(c.find("^green$", false, "green\n").unwrap());
        assert!(c.find("^green$", false, "green\r\n").unwrap());
        assert!(!c.find("^green$", false, "green\n\n").unwrap());
        assert!(c.find("^ \\$[$] $", true, "$$").unwrap());
        assert!(c.find("(?m)^a$", false, "a\nb").unwrap());
        // ASCII classes, also inside and negated inside character classes.
        assert!(!c.find("^\\d+$", false, "١٢").unwrap());
        assert!(c.find("^\\d+$", false, "12").unwrap());
        assert!(!c.find("^\\w$", false, "é").unwrap());
        assert!(c.find("^[\\w.]+$", true, "a.b_1").unwrap());
        assert!(c.find("^[x\\S]+$", true, "xé").unwrap());
        assert!(!c.find("\\s", false, "\u{a0}").unwrap());
        assert!(c.find("a \\s b", true, "a\tb").unwrap());
        assert!(c.find("^\\S+$", false, "é").unwrap());
        // Escaped punctuation is literal, including `\<`/`\>` (word boundaries in Rust).
        assert!(c.find("path \\[the_terms\\>unknown\\]\\;", false, "path [the_terms>unknown];").unwrap());
        assert!(c.find("\\<a\\> \\\"q\\\" \\/x\\/ \\@\\% \\_ \\é", false, "<a> \"q\" /x/ @% _ é").unwrap());
        assert!(c.find("a\\ b \\# c", true, "a b#c").unwrap());
        assert!(c.find("a*+b", false, "aaab").unwrap());
        assert!(c.find("x\\hy", false, "x\ty").unwrap());
        assert!(c.full_match("[a-z]+", "abc").unwrap());
        assert!(!c.full_match("[a-z]+", "abc1").unwrap());
        assert!(c.find("(", false, "x").is_err());
    }

    #[test]
    fn dot_excludes_every_java_line_terminator() {
        let mut c = RegexCache::default();
        for t in ["a\nb", "a\rb", "a\u{85}b", "a\u{2028}b", "a\u{2029}b"] {
            assert!(!c.find("^a.b$", false, t).unwrap(), "{t:?}");
            assert!(!c.find("a.b", true, t).unwrap(), "{t:?}");
            assert!(!c.full_match("a.b", t).unwrap(), "{t:?}");
            // DOTALL: anything.
            assert!(c.find("(?s)^a.b$", false, t).unwrap(), "{t:?}");
        }
        assert!(c.find("^a.b$", false, "a\tb").unwrap());
        assert!(c.find("^a.b$", false, "aéb").unwrap());
        assert!(c.find("^a.*+$", false, "abc").unwrap());
        // Literal in a class or escaped.
        assert!(c.find("^a[.]b$", false, "a.b").unwrap());
        assert!(!c.find("^a[.]b$", false, "axb").unwrap());
        assert!(!c.find("^a\\.b$", false, "axb").unwrap());
    }
}
