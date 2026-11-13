//! Character folding: `asciifolding`, `scandinavian_folding` and `scandinavian_normalization`.

use std::sync::Arc;

use super::ascii_folding_table::FOLDINGS;
use crate::component::{FilterFactory, SynonymRole};
use crate::stream::{Filter, TokenStream};
use crate::token::Tok;

/// The ASCII replacement of `c` (`ASCIIFoldingFilter.foldToASCII`), if any.
#[inline]
pub(crate) fn fold_char(c: char) -> Option<&'static str> {
    let cp = c as u32;
    if !(0x80..=0xFFFF).contains(&cp) {
        return None;
    }
    FOLDINGS
        .binary_search_by_key(&(cp as u16), |&(k, _)| k)
        .ok()
        .map(|i| FOLDINGS[i].1)
}

/// Appends the folded form of `s` to `out`; returns whether anything changed.
pub(crate) fn fold_into(s: &str, out: &mut String) -> bool {
    let mut changed = false;
    for c in s.chars() {
        match fold_char(c) {
            Some(r) => {
                out.push_str(r);
                changed = true;
            }
            None => out.push(c),
        }
    }
    changed
}

/// `ASCIIFoldingFilter`; with `preserve_original` the folded token is followed by the original
/// at the same position.
pub(crate) struct AsciiFolding {
    preserve: bool,
    pending: Tok,
    has_pending: bool,
    scratch: String,
}

impl AsciiFolding {
    pub fn new(preserve: bool) -> Self {
        AsciiFolding {
            preserve,
            pending: Tok::default(),
            has_pending: false,
            scratch: String::new(),
        }
    }
}

impl Filter for AsciiFolding {
    fn reset(&mut self) {
        self.has_pending = false;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if self.has_pending {
            self.has_pending = false;
            t.clone_from(&self.pending);
            t.pos_inc = 0;
            return true;
        }
        if !input.next(t) {
            return false;
        }
        if t.term.is_ascii() {
            return true;
        }
        self.scratch.clear();
        let changed = fold_into(&t.term, &mut self.scratch);
        if self.preserve && changed {
            self.pending.clone_from(t);
            self.has_pending = true;
        }
        std::mem::swap(&mut t.term, &mut self.scratch);
        true
    }
}

struct AsciiFoldingFactory {
    preserve: bool,
}

impl FilterFactory for AsciiFoldingFactory {
    fn create(&self) -> Box<dyn Filter> {
        Box::new(AsciiFolding::new(self.preserve))
    }
    fn normalizing(&self) -> bool {
        true
    }
    fn create_normalizer(&self) -> Box<dyn Filter> {
        Box::new(AsciiFolding::new(false))
    }
    fn synonym_role(&self) -> SynonymRole {
        if self.preserve {
            SynonymRole::Replace(ascii_folding_factory(false))
        } else {
            SynonymRole::Same
        }
    }
}

pub(crate) fn ascii_folding_factory(preserve: bool) -> Arc<dyn FilterFactory> {
    Arc::new(AsciiFoldingFactory { preserve })
}

/// `ScandinavianFoldingFilter`: å/ä/æ → a, ö/ø → o, and aa/ae/ao/oe/oo → a/o.
pub(crate) fn scandinavian_folding(t: &mut Tok, buf: &mut Vec<char>) {
    if t.term.is_ascii()
        && !t
            .term
            .bytes()
            .any(|b| matches!(b, b'a' | b'A' | b'o' | b'O'))
    {
        return;
    }
    buf.clear();
    buf.extend(t.term.chars());
    let mut i = 0;
    while i < buf.len() {
        match buf[i] {
            '\u{e5}' | '\u{e4}' | '\u{e6}' => buf[i] = 'a',
            '\u{c5}' | '\u{c4}' | '\u{c6}' => buf[i] = 'A',
            '\u{f8}' | '\u{f6}' => buf[i] = 'o',
            '\u{d8}' | '\u{d6}' => buf[i] = 'O',
            c if i + 1 < buf.len() => {
                let n = buf[i + 1];
                if ((c == 'a' || c == 'A') && matches!(n, 'a' | 'A' | 'e' | 'E' | 'o' | 'O'))
                    || ((c == 'o' || c == 'O') && matches!(n, 'e' | 'E' | 'o' | 'O'))
                {
                    buf.remove(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    t.term.clear();
    t.term.extend(buf.iter());
}

/// `ScandinavianNormalizationFilter`: interchangeable Scandinavian letters to æ/ø/å forms.
pub(crate) fn scandinavian_normalization(t: &mut Tok, buf: &mut Vec<char>) {
    if t.term.is_ascii()
        && !t
            .term
            .bytes()
            .any(|b| matches!(b, b'a' | b'A' | b'o' | b'O'))
    {
        return;
    }
    buf.clear();
    buf.extend(t.term.chars());
    let mut i = 0;
    while i < buf.len() {
        match buf[i] {
            '\u{e4}' => buf[i] = '\u{e6}',
            '\u{c4}' => buf[i] = '\u{c6}',
            '\u{f6}' => buf[i] = '\u{f8}',
            '\u{d6}' => buf[i] = '\u{d8}',
            c if i + 1 < buf.len() => {
                let n = buf[i + 1];
                let aa_ao = matches!(n, 'a' | 'A' | 'o' | 'O');
                if c == 'a' && aa_ao {
                    buf.remove(i + 1);
                    buf[i] = '\u{e5}';
                } else if c == 'A' && aa_ao {
                    buf.remove(i + 1);
                    buf[i] = '\u{c5}';
                } else if c == 'a' && matches!(n, 'e' | 'E') {
                    buf.remove(i + 1);
                    buf[i] = '\u{e6}';
                } else if c == 'A' && matches!(n, 'e' | 'E') {
                    buf.remove(i + 1);
                    buf[i] = '\u{c6}';
                } else if c == 'o' && matches!(n, 'e' | 'E' | 'o' | 'O') {
                    buf.remove(i + 1);
                    buf[i] = '\u{f8}';
                } else if c == 'O' && matches!(n, 'e' | 'E' | 'o' | 'O') {
                    buf.remove(i + 1);
                    buf[i] = '\u{d8}';
                }
            }
            _ => {}
        }
        i += 1;
    }
    t.term.clear();
    t.term.extend(buf.iter());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::Map;
    use crate::filter::tests::{run_filters, terms};

    #[test]
    fn ascii_folding() {
        assert_eq!(
            terms(
                vec![Box::new(AsciiFolding::new(false))],
                "açaí Æsir ⒜ “q” ﬁ"
            ),
            vec!["acai", "AEsir", "(a)", "\"q\"", "fi"]
        );
        let out = run_filters(vec![Box::new(AsciiFolding::new(true))], "déjà vu");
        assert_eq!(
            out.iter().map(|t| (t.0.as_str(), t.1)).collect::<Vec<_>>(),
            vec![("deja", 1), ("déjà", 0), ("vu", 1)]
        );
    }

    #[test]
    fn scandinavian() {
        let mut b = Vec::new();
        assert_eq!(
            terms(
                vec![Box::new(Map(move |t: &mut Tok| scandinavian_folding(
                    t, &mut b
                )))],
                "räksmörgås Blåbærsyltetøy aao"
            ),
            vec!["raksmorgas", "Blabarsyltetoy", "ao"]
        );
        let mut b = Vec::new();
        assert_eq!(
            terms(
                vec![Box::new(Map(
                    move |t: &mut Tok| scandinavian_normalization(t, &mut b)
                ))],
                "räksmörgås aa ae oe"
            ),
            vec!["ræksmørgås", "å", "æ", "ø"]
        );
    }
}
