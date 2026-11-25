//! A restore's rename (`rename_pattern` with `rename_replacement`), applied as Elasticsearch's
//! `RestoreService.renameIndex` applies it: Java's `Matcher.replaceAll`, whose replacement
//! syntax isn't the `regex` crate's. `$n` takes as many digits as still name a group of the
//! pattern (`$12` with two groups is group 1, then `2`), `${name}` a named group, and `\x` is
//! `x` itself. A replacement naming a group the pattern lacks fails, as in Java, once a name
//! matches.

use crate::error::{EsError, EsResult};

pub struct Rename {
    re: regex::Regex,
    /// The replacement, parsed; Java reports a bad one only when it's used.
    replacement: Result<Vec<Piece>, EsError>,
}

enum Piece {
    Text(String),
    Group(usize),
}

impl Rename {
    /// The rename a request asks for: none unless both are given (`RestoreService` renames
    /// only then).
    pub fn of(pattern: Option<&str>, replacement: Option<&str>) -> EsResult<Option<Rename>> {
        let (Some(p), Some(r)) = (pattern, replacement) else { return Ok(None) };
        let re = regex::Regex::new(p).map_err(|e| EsError::illegal_argument(e.to_string()))?;
        let replacement = parse(&re, r);
        Ok(Some(Rename { re, replacement }))
    }

    /// `name` renamed: unchanged when the pattern doesn't match it.
    pub fn apply(&self, name: &str) -> EsResult<String> {
        if !self.re.is_match(name) {
            return Ok(name.to_string());
        }
        let pieces = self.replacement.as_ref().map_err(Clone::clone)?;
        let mut out = String::with_capacity(name.len());
        let mut last = 0;
        for caps in self.re.captures_iter(name) {
            let whole = caps.get(0).expect("group 0");
            out.push_str(&name[last..whole.start()]);
            for p in pieces {
                match p {
                    Piece::Text(t) => out.push_str(t),
                    // A group that took no part in the match adds nothing.
                    Piece::Group(g) => out.push_str(caps.get(*g).map_or("", |m| m.as_str())),
                }
            }
            last = whole.end();
        }
        out.push_str(&name[last..]);
        Ok(out)
    }
}

/// `Matcher.appendExpandedReplacement`, with its errors (an `IllegalArgumentException` is a
/// 400; `start(n)`'s `IndexOutOfBoundsException` for a group past the pattern's, a 500).
fn parse(re: &regex::Regex, replacement: &str) -> Result<Vec<Piece>, EsError> {
    let groups = re.captures_len() - 1;
    let mut pieces = Vec::new();
    let mut text = String::new();
    let mut chars = replacement.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(n) => text.push(n),
                None => return Err(EsError::illegal_argument("character to be escaped is missing")),
            },
            '$' => {
                let group = match chars.next() {
                    None => return Err(EsError::illegal_argument("Illegal group reference: group index is missing")),
                    Some('{') => {
                        let mut name = String::new();
                        while let Some(&n) = chars.peek().filter(|n| n.is_ascii_alphanumeric()) {
                            name.push(n);
                            chars.next();
                        }
                        if name.is_empty() {
                            return Err(EsError::illegal_argument("named capturing group has 0 length name"));
                        }
                        if chars.next() != Some('}') {
                            return Err(EsError::illegal_argument("named capturing group is missing trailing '}'"));
                        }
                        if name.starts_with(|c: char| c.is_ascii_digit()) {
                            return Err(EsError::illegal_argument(format!("capturing group name {{{name}}} starts with digit character")));
                        }
                        re.capture_names().position(|n| n == Some(name.as_str())).ok_or_else(|| EsError::illegal_argument(format!("No group with name {{{name}}}")))?
                    }
                    Some(d) => {
                        let Some(mut group) = d.to_digit(10).map(|d| d as usize) else { return Err(EsError::illegal_argument("Illegal group reference")) };
                        while let Some(next) = chars.peek().and_then(|c| c.to_digit(10)).map(|d| group * 10 + d as usize).filter(|n| *n <= groups) {
                            group = next;
                            chars.next();
                        }
                        if group > groups {
                            return Err(EsError::new(500, "index_out_of_bounds_exception", format!("No group {group}")));
                        }
                        group
                    }
                };
                if !text.is_empty() {
                    pieces.push(Piece::Text(std::mem::take(&mut text)));
                }
                pieces.push(Piece::Group(group));
            }
            c => text.push(c),
        }
    }
    if !text.is_empty() {
        pieces.push(Piece::Text(text));
    }
    Ok(pieces)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rename(p: &str, r: &str, name: &str) -> Result<String, String> {
        Rename::of(Some(p), Some(r)).unwrap().unwrap().apply(name).map_err(|e| format!("{} {}", e.status, e.reason()))
    }

    /// Java's replacement syntax, as `RestoreService.renameIndex` applies it.
    #[test]
    fn renames_as_javas_replace_all() {
        assert_eq!(rename("(a)(b)", "x$12", "abc").unwrap(), "xa2c");
        assert_eq!(rename("(.+)-(.+)", "$2_$1", "logs-web").unwrap(), "web_logs");
        assert_eq!(rename("(?P<what>.+)-1", "${what}-2", "events-1").unwrap(), "events-2");
        assert_eq!(rename("(.+)", r"\$1-\\", "i").unwrap(), r"$1-\");
        assert_eq!(rename("x", "$9", "abc").unwrap(), "abc", "no match: the replacement isn't read");
        assert_eq!(rename("(a)|(b)", "[$2]", "ab").unwrap(), "[][b]");
        assert_eq!(rename("(a)", "$2", "a").unwrap_err(), "500 No group 2");
        assert_eq!(rename("(a)", "${b}", "a").unwrap_err(), "400 No group with name {b}");
        assert_eq!(rename("(a)", "$x", "a").unwrap_err(), "400 Illegal group reference");
        assert_eq!(rename("(a)", "a\\", "a").unwrap_err(), "400 character to be escaped is missing");
        assert!(Rename::of(Some("(a)"), None).unwrap().is_none() && Rename::of(None, Some("b")).unwrap().is_none());
    }
}
