//! `UnicodeSet` patterns (`unicode_set_filter`, transform filters), parsed by ICU4X's
//! `unicodeset_parse` (ICU's syntax: nested sets, `&` and `-`, `[:prop:]` and `\p{prop}` with
//! loose property names) and queried over UTF-16 as ICU's `UnicodeSet.span` does.

use icu_collections::codepointinvlist::CodePointInversionList;

/// A set of code points (strings in a pattern are ignored: the filters only test code points).
#[derive(Debug)]
pub(crate) struct USet {
    set: CodePointInversionList<'static>,
}

impl USet {
    /// Parses a whole pattern, with ICU's error text for a bad one.
    pub(crate) fn parse(pattern: &str) -> Result<USet, String> {
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
