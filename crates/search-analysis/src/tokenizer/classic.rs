//! `classic`: Lucene's `ClassicTokenizer` (the pre-3.1 `StandardTokenizer` grammar), which
//! recognizes acronyms, company names, emails, host names and serial numbers with Unicode 3.0
//! letter and digit classes.
//!
//! Unlike `standard`, its JFlex buffer grows, so a token longer than `max_token_length` is not
//! split but dropped, leaving a position gap (`ClassicTokenizer.incrementToken`).

use std::sync::OnceLock;

use crate::dfa::{self, Dfa, Re, alt, lit, plus, seq, set, star};
use crate::stream::Tokenizer;
use crate::token::Tok;
use crate::unicode::tables::{GC3_LETTER, GC3_ND};

pub(crate) const ALPHANUM: &str = "<ALPHANUM>";
pub(crate) const APOSTROPHE: &str = "<APOSTROPHE>";
pub(crate) const ACRONYM: &str = "<ACRONYM>";
pub(crate) const COMPANY: &str = "<COMPANY>";
pub(crate) const EMAIL: &str = "<EMAIL>";
pub(crate) const HOST: &str = "<HOST>";
pub(crate) const NUM: &str = "<NUM>";
pub(crate) const CJ: &str = "<CJ>";

/// Rule types; the last rule (`ACRONYM_DEP`) reports as `<HOST>` with its trailing dot removed.
const RULE_TYPES: [&str; 9] = [
    ALPHANUM, APOSTROPHE, ACRONYM, COMPANY, EMAIL, HOST, NUM, CJ, HOST,
];
const ACRONYM_DEP: usize = 8;

fn dfa_classic() -> &'static Dfa {
    static DFA: OnceLock<Dfa> = OnceLock::new();
    DFA.get_or_init(|| {
        let cj = dfa::ranges(vec![
            (0x3100, 0x312f),
            (0x3040, 0x309F),
            (0x30A0, 0x30FF),
            (0x31F0, 0x31FF),
            (0x3300, 0x337f),
            (0x3400, 0x4dbf),
            (0x4e00, 0x9fff),
            (0xf900, 0xfaff),
            (0xff65, 0xff9f),
        ]);
        let letter = dfa::difference(GC3_LETTER, &cj);
        let thai = vec![(0x0E00, 0x0E59)];
        let digit = GC3_ND.to_vec();
        let alnum_set = dfa::union(&[&letter, &thai, &digit]);
        let letter_digit = dfa::union(&[&letter, &digit]);
        let alphanum = || plus(set(&alnum_set));
        let alpha = || plus(set(&letter));
        let p = || set(&dfa::chars("_-/.,"));
        let has_digit = || {
            seq(vec![
                star(set(&letter_digit)),
                set(&digit),
                star(set(&letter_digit)),
            ])
        };
        let dot = || lit(".");

        let apostrophe = seq(vec![alpha(), plus(seq(vec![lit("'"), alpha()]))]);
        let acronym = seq(vec![
            set(&letter),
            dot(),
            plus(seq(vec![set(&letter), dot()])),
        ]);
        let acronym_dep = seq(vec![alphanum(), dot(), plus(seq(vec![alphanum(), dot()]))]);
        let company = seq(vec![alpha(), set(&dfa::chars("&@")), alpha()]);
        let email = seq(vec![
            alphanum(),
            star(seq(vec![set(&dfa::chars(".-_")), alphanum()])),
            lit("@"),
            alphanum(),
            plus(seq(vec![set(&dfa::chars(".-")), alphanum()])),
        ]);
        let host = seq(vec![alphanum(), plus(seq(vec![dot(), alphanum()]))]);
        let num = alt(vec![
            seq(vec![alphanum(), p(), has_digit()]),
            seq(vec![has_digit(), p(), alphanum()]),
            seq(vec![
                alphanum(),
                plus(seq(vec![p(), has_digit(), p(), alphanum()])),
            ]),
            seq(vec![
                has_digit(),
                plus(seq(vec![p(), alphanum(), p(), has_digit()])),
            ]),
            seq(vec![
                alphanum(),
                p(),
                has_digit(),
                plus(seq(vec![p(), alphanum(), p(), has_digit()])),
            ]),
            seq(vec![
                has_digit(),
                p(),
                alphanum(),
                plus(seq(vec![p(), has_digit(), p(), alphanum()])),
            ]),
        ]);
        let rules: Vec<Re> = vec![
            alphanum(),
            apostrophe,
            acronym,
            company,
            email,
            host,
            num,
            set(&cj),
            acronym_dep,
        ];
        Dfa::new(&rules)
    })
}

/// `ClassicTokenizer`.
pub(crate) struct ClassicTokenizer {
    dfa: &'static Dfa,
    max_len: usize,
    pos: usize,
    upos: usize,
    skipped: u32,
    exhausted: bool,
}

impl ClassicTokenizer {
    pub fn new(max_len: usize) -> ClassicTokenizer {
        ClassicTokenizer {
            dfa: dfa_classic(),
            max_len,
            pos: 0,
            upos: 0,
            skipped: 0,
            exhausted: false,
        }
    }
}

impl Tokenizer for ClassicTokenizer {
    fn reset(&mut self, _text: &str) {
        self.pos = 0;
        self.upos = 0;
        self.skipped = 0;
        self.exhausted = false;
    }

    fn next(&mut self, text: &str, t: &mut Tok) -> bool {
        let mut skipped = 0;
        while self.pos < text.len() {
            if let Some((rule, end, ulen)) = self.dfa.longest_match(text, self.pos, usize::MAX) {
                let (start_b, start_u) = (self.pos, self.upos);
                self.pos = end;
                self.upos += ulen;
                if ulen > self.max_len {
                    skipped += 1;
                    continue;
                }
                t.pos_inc = skipped + 1;
                t.start = start_u as u32;
                t.end = (start_u + ulen) as u32;
                if rule == ACRONYM_DEP {
                    t.term.push_str(&text[start_b..end - 1]);
                } else {
                    t.term.push_str(&text[start_b..end]);
                }
                t.kind = RULE_TYPES[rule];
                return true;
            }
            let c = text[self.pos..].chars().next().unwrap_or(' ');
            self.pos += c.len_utf8();
            self.upos += c.len_utf16();
        }
        self.skipped = skipped;
        self.exhausted = true;
        false
    }

    /// The scanner's position: the end of the last token until the text has been read to the
    /// end (a `limit` filter can end the stream early).
    fn end(&mut self, text: &str, t: &mut Tok) {
        let end = if self.exhausted {
            crate::unicode::utf16_len(text)
        } else {
            self.upos
        } as u32;
        t.start = end;
        t.end = end;
        t.pos_inc += self.skipped;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(text: &str) -> Vec<(String, &'static str)> {
        let mut tk = ClassicTokenizer::new(255);
        tk.reset(text);
        let mut out = Vec::new();
        let mut t = Tok::default();
        loop {
            t.clear();
            if !tk.next(text, &mut t) {
                break;
            }
            out.push((t.term.clone(), t.kind));
        }
        out
    }

    #[test]
    fn classic_types() {
        let t =
            tokens("The I.B.M. AT&T O'Reilly's john@example.com www.example.com 1.2.3 B2B-2 中文");
        assert_eq!(
            t,
            vec![
                ("The".to_string(), ALPHANUM),
                ("I.B.M.".to_string(), ACRONYM),
                ("AT&T".to_string(), COMPANY),
                ("O'Reilly's".to_string(), APOSTROPHE),
                ("john@example.com".to_string(), EMAIL),
                ("www.example.com".to_string(), HOST),
                ("1.2.3".to_string(), HOST),
                ("B2B-2".to_string(), NUM),
                ("中".to_string(), CJ),
                ("文".to_string(), CJ),
            ]
        );
        // ACRONYM_DEP: alphanumerics with dots, reported as a host without its last dot.
        assert_eq!(tokens("ab1.cd."), vec![("ab1.cd".to_string(), HOST)]);
    }
}
