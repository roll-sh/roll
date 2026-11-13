//! Case filters: `lowercase` (with the `greek`, `irish` and `turkish` variants) and `uppercase`.
//!
//! Lucene maps case code point by code point with `Character.toLowerCase`/`toUpperCase`, the
//! simple one-to-one mappings (`ß` stays `ß`, `İ` becomes `i`), never the locale-sensitive or
//! length-changing full mappings; [`crate::unicode`] provides exactly those.

use crate::stream::{Filter, TokenStream};
use crate::token::Tok;
use crate::unicode::{self, gc};

/// `LowerCaseFilter`.
#[derive(Default)]
pub(crate) struct Lower {
    scratch: String,
}

impl Filter for Lower {
    #[inline]
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if !input.next(t) {
            return false;
        }
        unicode::lower_in_place(&mut t.term, &mut self.scratch);
        true
    }
}

/// `UpperCaseFilter`.
#[derive(Default)]
pub(crate) struct Upper {
    scratch: String,
}

impl Filter for Upper {
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if !input.next(t) {
            return false;
        }
        if t.term.is_ascii() {
            t.term.make_ascii_uppercase();
        } else {
            self.scratch.clear();
            self.scratch.extend(t.term.chars().map(unicode::to_upper));
            std::mem::swap(&mut t.term, &mut self.scratch);
        }
        true
    }
}

/// `GreekLowerCaseFilter`: lowercase, final sigma to sigma, accents (tonos, dialytika) removed.
#[derive(Default)]
pub(crate) struct GreekLower {
    scratch: String,
}

fn greek_lower(c: char) -> char {
    match c {
        '\u{03C2}' => '\u{03C3}',
        '\u{0386}' | '\u{03AC}' => '\u{03B1}',
        '\u{0388}' | '\u{03AD}' => '\u{03B5}',
        '\u{0389}' | '\u{03AE}' => '\u{03B7}',
        '\u{038A}' | '\u{03AA}' | '\u{03AF}' | '\u{03CA}' | '\u{0390}' => '\u{03B9}',
        '\u{038E}' | '\u{03AB}' | '\u{03CD}' | '\u{03CB}' | '\u{03B0}' => '\u{03C5}',
        '\u{038C}' | '\u{03CC}' => '\u{03BF}',
        '\u{038F}' | '\u{03CE}' => '\u{03C9}',
        '\u{03A2}' => '\u{03C2}',
        c => unicode::to_lower(c),
    }
}

impl Filter for GreekLower {
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if !input.next(t) {
            return false;
        }
        self.scratch.clear();
        self.scratch.extend(t.term.chars().map(greek_lower));
        std::mem::swap(&mut t.term, &mut self.scratch);
        true
    }
}

/// `IrishLowerCaseFilter`: `nAthair` becomes `n-athair` (the prefix before an uppercase vowel is
/// kept apart), then lowercase per UTF-16 unit (so supplementary characters are left alone).
#[derive(Default)]
pub(crate) struct IrishLower {
    scratch: String,
}

fn is_upper_vowel(c: char) -> bool {
    matches!(
        c,
        'A' | 'E' | 'I' | 'O' | 'U' | '\u{c1}' | '\u{c9}' | '\u{cd}' | '\u{d3}' | '\u{da}'
    )
}

impl Filter for IrishLower {
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if !input.next(t) {
            return false;
        }
        self.scratch.clear();
        let mut chars = t.term.chars();
        let first = chars.next();
        let second = chars.clone().next();
        let mut rest_start = 0;
        if let (Some(f), Some(s)) = (first, second)
            && (f == 'n' || f == 't')
            && is_upper_vowel(s)
        {
            self.scratch.push(f);
            self.scratch.push('-');
            rest_start = 1;
        }
        for c in t.term[rest_start..].chars() {
            self.scratch.push(if (c as u32) < 0x10000 {
                unicode::to_lower(c)
            } else {
                c
            });
        }
        std::mem::swap(&mut t.term, &mut self.scratch);
        true
    }
}

/// `TurkishLowerCaseFilter`: `I` lowercases to dotless `ı` unless followed (across non-spacing
/// marks) by a combining dot above, which is then removed and gives `i`.
#[derive(Default)]
pub(crate) struct TurkishLower {
    scratch: String,
    chars: Vec<char>,
}

const CAPITAL_I: char = 'I';
const COMBINING_DOT_ABOVE: char = '\u{0307}';

impl Filter for TurkishLower {
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if !input.next(t) {
            return false;
        }
        self.chars.clear();
        self.chars.extend(t.term.chars());
        self.scratch.clear();
        let s = &self.chars;
        let mut i_or_after = false;
        let mut i = 0;
        while i < s.len() {
            let ch = s[i];
            i_or_after =
                ch == CAPITAL_I || (i_or_after && unicode::get_type(ch) == gc::NON_SPACING_MARK);
            if i_or_after {
                if ch == COMBINING_DOT_ABOVE {
                    i += 1;
                    continue;
                }
                if ch == CAPITAL_I {
                    let before_dot = s[i + 1..]
                        .iter()
                        .take_while(|&&c| unicode::get_type(c) == gc::NON_SPACING_MARK)
                        .any(|&c| c == COMBINING_DOT_ABOVE);
                    if before_dot {
                        self.scratch.push('i');
                    } else {
                        self.scratch.push('\u{0131}');
                        i_or_after = false;
                    }
                    i += 1;
                    continue;
                }
            }
            self.scratch.push(unicode::to_lower(ch));
            i += 1;
        }
        std::mem::swap(&mut t.term, &mut self.scratch);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::tests::terms;

    #[test]
    fn cases() {
        assert_eq!(
            terms(vec![Box::new(Lower::default())], "HeLLo ÉTÉ İstanbul"),
            vec!["hello", "été", "istanbul"]
        );
        assert_eq!(
            terms(vec![Box::new(Upper::default())], "straße déjà"),
            vec!["STRAßE", "DÉJÀ"]
        );
        assert_eq!(
            terms(vec![Box::new(GreekLower::default())], "ΆΛΦΑ λόγος"),
            vec!["αλφα", "λογοσ"]
        );
        assert_eq!(
            terms(vec![Box::new(IrishLower::default())], "nAthair tUISCE Bean"),
            vec!["n-athair", "t-uisce", "bean"]
        );
        assert_eq!(
            terms(
                vec![Box::new(TurkishLower::default())],
                "ISPARTA I\u{0307}stanbul"
            ),
            vec!["ısparta", "istanbul"]
        );
    }
}
