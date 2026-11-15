//! Character properties with the JDK's semantics.
//!
//! Lucene's analysis components classify characters through `java.lang.Character`
//! (`isLetter`, `isWhitespace`, `toLowerCase(int)` …), which follow the Unicode version of the JDK
//! Elasticsearch bundles (Unicode 17 for 9.5.5). Rust's `char` methods use a different Unicode
//! version and different definitions (`is_alphabetic` is the Alphabetic property, not the letter
//! categories; `to_lowercase` applies full rather than simple case mappings), so tokens would
//! differ on exactly the inputs where compatibility is hardest to debug. These functions read
//! tables generated from the UCD (`tools/gen_unicode.py`): two-level lookups for the hot
//! properties (general category, case mappings) and sorted ranges for the rare ones.
//!
//! The JFlex grammars of `standard`, `uax_url_email` and `classic` were compiled against older
//! Unicode versions (12.1 and 3.0); their property ranges live here too, for [`crate::dfa`].

#[allow(dead_code)]
pub(crate) mod tables;

/// Unicode blocks and scripts under the JDK's names.
pub(crate) mod blocks;

/// Case foldings from the UCD.
pub(crate) mod case_folding;

use tables::*;

/// `java.lang.Character` general category values (`Character.getType`).
#[allow(dead_code)] // the full `Character.getType` table
pub(crate) mod gc {
    pub const UNASSIGNED: u8 = 0;
    pub const UPPERCASE_LETTER: u8 = 1;
    pub const LOWERCASE_LETTER: u8 = 2;
    pub const TITLECASE_LETTER: u8 = 3;
    pub const MODIFIER_LETTER: u8 = 4;
    pub const OTHER_LETTER: u8 = 5;
    pub const NON_SPACING_MARK: u8 = 6;
    pub const ENCLOSING_MARK: u8 = 7;
    pub const COMBINING_SPACING_MARK: u8 = 8;
    pub const DECIMAL_DIGIT_NUMBER: u8 = 9;
    pub const LETTER_NUMBER: u8 = 10;
    pub const OTHER_NUMBER: u8 = 11;
    pub const SPACE_SEPARATOR: u8 = 12;
    pub const LINE_SEPARATOR: u8 = 13;
    pub const PARAGRAPH_SEPARATOR: u8 = 14;
    pub const CONTROL: u8 = 15;
    pub const FORMAT: u8 = 16;
    pub const PRIVATE_USE: u8 = 18;
    pub const SURROGATE: u8 = 19;
    pub const DASH_PUNCTUATION: u8 = 20;
    pub const START_PUNCTUATION: u8 = 21;
    pub const END_PUNCTUATION: u8 = 22;
    pub const CONNECTOR_PUNCTUATION: u8 = 23;
    pub const OTHER_PUNCTUATION: u8 = 24;
    pub const MATH_SYMBOL: u8 = 25;
    pub const CURRENCY_SYMBOL: u8 = 26;
    pub const MODIFIER_SYMBOL: u8 = 27;
    pub const OTHER_SYMBOL: u8 = 28;
    pub const INITIAL_QUOTE_PUNCTUATION: u8 = 29;
    pub const FINAL_QUOTE_PUNCTUATION: u8 = 30;
}

/// `Character.getType(cp)`.
#[inline]
pub(crate) fn get_type(c: char) -> u8 {
    let cp = c as usize;
    GC_BLOCKS[GC_INDEX[cp >> 8] as usize][cp & 0xFF]
}

/// `Character.isLetter(cp)`.
#[inline]
pub(crate) fn is_letter(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphabetic();
    }
    matches!(get_type(c), 1..=5)
}

/// `Character.isDigit(cp)`.
#[inline]
pub(crate) fn is_digit(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_digit();
    }
    get_type(c) == gc::DECIMAL_DIGIT_NUMBER
}

/// `Character.isLetterOrDigit(cp)`.
#[inline]
pub(crate) fn is_letter_or_digit(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphanumeric();
    }
    matches!(get_type(c), 1..=5 | gc::DECIMAL_DIGIT_NUMBER)
}

/// The value of a decimal digit (`Character.getNumericValue` for Nd characters): Nd ranges are
/// runs of complete 0–9 sequences, so the value is the distance from the range start modulo 10.
pub(crate) fn digit_value(c: char) -> Option<u32> {
    if c.is_ascii_digit() {
        return Some(c as u32 - '0' as u32);
    }
    if !is_digit(c) {
        return None;
    }
    let cp = c as u32;
    let mut start = cp;
    while start > 0 && char::from_u32(start - 1).is_some_and(is_digit) {
        start -= 1;
    }
    Some((cp - start) % 10)
}

/// `Character.isWhitespace(cp)`: space separators except the no-break spaces, plus the ASCII
/// controls `\t \n \u000B \f \r` and the information separators `\u001C`–`\u001F`.
#[inline]
pub(crate) fn is_whitespace(c: char) -> bool {
    let cp = c as u32;
    if cp < 0x80 {
        return cp == 0x20 || (0x09..=0x0D).contains(&cp) || (0x1C..=0x1F).contains(&cp);
    }
    match get_type(c) {
        gc::SPACE_SEPARATOR | gc::LINE_SEPARATOR | gc::PARAGRAPH_SEPARATOR => {
            cp != 0x00A0 && cp != 0x2007 && cp != 0x202F
        }
        _ => false,
    }
}

/// `Character.isSpaceChar(cp)`.
#[inline]
pub(crate) fn is_space_char(c: char) -> bool {
    matches!(
        get_type(c),
        gc::SPACE_SEPARATOR | gc::LINE_SEPARATOR | gc::PARAGRAPH_SEPARATOR
    )
}

fn in_ranges(ranges: &[(u32, u32)], cp: u32) -> bool {
    ranges
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                std::cmp::Ordering::Less
            } else if lo > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// `Character.isLowerCase(cp)`: Ll or Other_Lowercase.
pub(crate) fn is_lowercase(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_lowercase();
    }
    get_type(c) == gc::LOWERCASE_LETTER || in_ranges(OTHER_LOWERCASE, c as u32)
}

/// `Character.isUpperCase(cp)`: Lu or Other_Uppercase.
pub(crate) fn is_uppercase(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_uppercase();
    }
    get_type(c) == gc::UPPERCASE_LETTER || in_ranges(OTHER_UPPERCASE, c as u32)
}

/// `Character.isAlphabetic(cp)`.
pub(crate) fn is_alphabetic(c: char) -> bool {
    in_ranges(ALPHABETIC, c as u32)
}

/// `Character.isIdeographic(cp)`.
#[allow(dead_code)]
pub(crate) fn is_ideographic(c: char) -> bool {
    in_ranges(IDEOGRAPHIC, c as u32)
}

/// `Character.toLowerCase(cp)`: the simple (one to one) lowercase mapping.
#[inline]
pub(crate) fn to_lower(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let cp = c as usize;
    let id = LOWER_BLOCKS[LOWER_INDEX[cp >> 8] as usize][cp & 0xFF];
    if id == 0 {
        return c;
    }
    // Safe by construction: the deltas come from UnicodeData, which only maps to scalar values.
    char::from_u32((cp as i64 + LOWER_DELTAS[id as usize] as i64) as u32).unwrap_or(c)
}

/// `Character.toUpperCase(cp)`: the simple uppercase mapping (`ß` stays `ß`).
#[inline]
pub(crate) fn to_upper(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_uppercase();
    }
    let cp = c as usize;
    let id = UPPER_BLOCKS[UPPER_INDEX[cp >> 8] as usize][cp & 0xFF];
    if id == 0 {
        return c;
    }
    char::from_u32((cp as i64 + UPPER_DELTAS[id as usize] as i64) as u32).unwrap_or(c)
}

/// Appends `s` lowercased code point by code point, like Lucene's `LowerCaseFilter`.
pub(crate) fn push_lower(out: &mut String, s: &str) {
    if s.is_ascii() {
        out.reserve(s.len());
        for b in s.bytes() {
            out.push(b.to_ascii_lowercase() as char);
        }
        return;
    }
    for c in s.chars() {
        out.push(to_lower(c));
    }
}

/// Lowercases `s` in place (`Character.toLowerCase` per code point). Returns whether anything
/// changed. ASCII is rewritten without reallocating.
pub(crate) fn lower_in_place(s: &mut String, scratch: &mut String) -> bool {
    if s.is_ascii() {
        if !s.bytes().any(|b| b.is_ascii_uppercase()) {
            return false;
        }
        s.make_ascii_lowercase();
        return true;
    }
    if !s.chars().any(|c| to_lower(c) != c) {
        return false;
    }
    scratch.clear();
    push_lower(scratch, s);
    std::mem::swap(s, scratch);
    true
}

/// Number of UTF-16 code units of `s` (Java's `String.length()`).
#[inline]
pub(crate) fn utf16_len(s: &str) -> usize {
    if s.is_ascii() {
        return s.len();
    }
    s.chars().map(char::len_utf16).sum()
}

fn java_name_lookup(table: &[(&str, u16)], name: &str) -> Option<usize> {
    // `forName` upper-cases with full case mappings (so `ı` matches `I`), then looks the name up.
    let upper = name.to_uppercase();
    table
        .binary_search_by(|(n, _)| (*n).cmp(upper.as_str()))
        .ok()
        .map(|i| table[i].1 as usize)
}

/// The code points of a block by any name `Character.UnicodeBlock.forName` accepts.
pub(crate) fn unicode_block(name: &str) -> Option<&'static [(u32, u32)]> {
    java_name_lookup(blocks::BLOCK_NAMES, name).map(|i| blocks::BLOCK_RANGES[i])
}

/// The code points of a script by any name `Character.UnicodeScript.forName` accepts.
pub(crate) fn unicode_script(name: &str) -> Option<&'static [(u32, u32)]> {
    java_name_lookup(blocks::SCRIPT_NAMES, name).map(|i| blocks::SCRIPT_RANGES[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_semantics() {
        assert!(
            is_letter('a')
                && is_letter('é')
                && is_letter('中')
                && !is_letter('1')
                && !is_letter('_')
        );
        // U+10940 SIDETIC LETTER N01 is new in Unicode 17; the JDK in Elasticsearch 9.5.5 knows it.
        assert!(is_letter('\u{10940}'));
        assert!(!is_letter('\u{16EE}')); // RUNIC ARLAUG SYMBOL: Nl, alphabetic but not a letter
        assert!(is_alphabetic('\u{16EE}'));
        assert!(is_digit('٣') && !is_digit('½'));
        assert!(is_whitespace(' ') && is_whitespace('\u{2003}') && is_whitespace('\u{1F}'));
        assert!(!is_whitespace('\u{A0}') && !is_whitespace('\u{202F}') && !is_whitespace('\u{85}'));
        assert_eq!(to_lower('İ'), 'i');
        assert_eq!(to_lower('Σ'), 'σ');
        assert_eq!(to_upper('ß'), 'ß');
        assert_eq!(to_upper('ǆ'), 'Ǆ');
        assert_eq!(to_lower('ǅ'), 'ǆ');
        assert_eq!(to_lower('\u{1E9E}'), 'ß');
        assert_eq!(to_lower('😀'), '😀');
        assert!(is_lowercase('ª') && is_uppercase('Ⓐ') && !is_lowercase('A'));
        assert_eq!(get_type('\u{E000}'), gc::PRIVATE_USE);
        assert_eq!(get_type('\u{378}'), gc::UNASSIGNED);
        assert_eq!(utf16_len("a😀é"), 4);
    }

    #[test]
    fn lowercase_in_place() {
        let mut s = String::from("HeLLo");
        let mut scratch = String::new();
        assert!(lower_in_place(&mut s, &mut scratch));
        assert_eq!(s, "hello");
        assert!(!lower_in_place(&mut s, &mut scratch));
        let mut s = String::from("ÀÉİ");
        assert!(lower_in_place(&mut s, &mut scratch));
        assert_eq!(s, "àéi");
    }
}
