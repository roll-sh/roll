//! `UnicodeSet` patterns (`unicode_set_filter`, transform filters), parsed by ICU4X's
//! `unicodeset_parse` (ICU's syntax: nested sets, `&` and `-`, `[:prop:]` and `\p{prop}` with
//! loose property names) and queried over UTF-16 as ICU's `UnicodeSet.span` does.

use icu_collections::codepointinvlist::CodePointInversionList;

/// A set of code points (strings in a pattern are ignored: the filters only test code points).
#[derive(Debug)]
pub(crate) struct USet {
    set: CodePointInversionList<'static>,
}

/// ICU's `UnicodeSet.MAX_DEPTH`: sets nest at most 101 deep (ICU4X's parser recurses once per
/// level with no limit of its own, so a deeper pattern would overflow the stack).
const MAX_DEPTH: usize = 101;

/// The longest pattern parsed (ICU has no limit; this keeps a hostile pattern's parse short).
pub(crate) const MAX_PATTERN_LEN: usize = 64 * 1024;

/// ICU's error for a pattern nested deeper than `MAX_DEPTH`, found before ICU4X sees it: `[`
/// opens a level unless escaped with `\` or opening a `[:property:]` (ICU4J counts brackets in
/// quotes too).
fn check_depth(pattern: &str) -> Result<(), String> {
    let b = pattern.as_bytes();
    let mut depth = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'[' => {
                // `[:name:]` is a property (its name holds no `[`), not a level.
                let prop = pattern[i..].strip_prefix("[:").and_then(|r| r.find(":]").filter(|&e| !r[..e].contains('[')));
                if let Some(e) = prop {
                    i += e + 3;
                } else {
                    depth += 1;
                    if depth > MAX_DEPTH {
                        return Err(format!("Error: Pattern nested too deeply at \"{}|{}\"", &pattern[..i], &pattern[i..]));
                    }
                }
            }
            b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    Ok(())
}

impl USet {
    /// Parses a whole pattern, with ICU's error text for a bad one. Every pattern from settings
    /// comes through here.
    pub(crate) fn parse(pattern: &str) -> Result<USet, String> {
        if pattern.len() > MAX_PATTERN_LEN {
            return Err(format!("Error: Pattern longer than {MAX_PATTERN_LEN} bytes"));
        }
        check_depth(pattern)?;
        let trimmed = pattern.trim_end_matches(|c: char| c.is_whitespace());
        match icu_properties::unicodeset_parse::parse(trimmed) {
            Ok((set, consumed)) if consumed == trimmed.len() => Ok(USet { set: set.code_points().clone() }),
            Ok((_, consumed)) => Err(format!("Extra chars in variable value {}", &trimmed[consumed..])),
            // ICU's `Error: <reason> at "<parsed>|<rest>"` (the reasons ICU gives for the
            // same faults).
            Err(e) => {
                use icu_properties::unicodeset_parse::ParseErrorKind as K;
                let at = match e.offset() {
                    Some(o) => {
                        let end = trimmed[o..].chars().next().map_or(o, |c| o + c.len_utf8());
                        end.min(pattern.len())
                    }
                    None => pattern.len(),
                };
                let reason = match e.kind() {
                    K::Eof => "Missing ']'",
                    K::UnknownProperty => "Invalid property pattern",
                    K::UnknownVariable | K::UnexpectedVariable => "Undefined variable",
                    K::InvalidEscape => "Invalid escape",
                    _ => "Set expected",
                };
                Err(format!("Error: {reason} at \"{}|{}\"", &pattern[..at], &pattern[at..]))
            }
        }
    }

    pub(crate) fn contains(&self, c: u32) -> bool {
        self.set.contains32(c)
    }

    /// `UnicodeSet.span(s, start, contained ? SIMPLE : NOT_CONTAINED)` over UTF-16: the end of
    /// the run of code points that are (or aren't) in the set.
    pub(crate) fn span(&self, s: &[u16], start: usize, contained: bool) -> usize {
        let mut i = start;
        while i < s.len() {
            let c = s[i] as u32;
            let (cp, n) = if (0xd800..0xdc00).contains(&c) && i + 1 < s.len() && (0xdc00..0xe000).contains(&(s[i + 1] as u32)) {
                (0x10000 + ((c - 0xd800) << 10) + (s[i + 1] as u32 - 0xdc00), 2)
            } else {
                (c, 1)
            };
            if self.contains(cp) != contained {
                break;
            }
            i += n;
        }
        i
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nest(n: usize) -> String {
        format!("{}a{}", "[".repeat(n), "]".repeat(n))
    }

    #[test]
    fn deep_sets_are_refused_before_icu4x_parses_them() {
        assert!(USet::parse(&nest(MAX_DEPTH)).unwrap().contains('a' as u32));
        let e = USet::parse(&nest(600)).unwrap_err();
        assert!(e.starts_with(&format!("Error: Pattern nested too deeply at \"{}|[", "[".repeat(MAX_DEPTH))), "{e}");
        // Escaped brackets and properties are not levels.
        assert!(USet::parse(&format!("[{}]", "\\[".repeat(600))).is_ok());
        assert!(USet::parse(&format!("{}[:L:]{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH))).is_ok());
        assert!(USet::parse(&format!("[{}]", "a".repeat(MAX_PATTERN_LEN))).is_err());
    }
}
