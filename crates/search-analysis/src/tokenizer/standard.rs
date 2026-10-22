//! `standard`: Lucene's `StandardTokenizer`, the UAX#29 word break rules as Lucene's JFlex grammar
//! states them (Unicode 12.1 properties, the TR#51 emoji sequences, Southeast Asian runs kept
//! whole, every Han and Hiragana character its own token).
//!
//! The grammar is compiled by [`crate::dfa`] and scanned with JFlex's discipline: longest match,
//! earlier rule on ties, anything unmatched skipped one code point at a time. Lucene sizes the
//! scanner buffer to `max_token_length` and disables buffer growth, so a match can never be longer
//! than that many UTF-16 units: a longer word comes out in `max_token_length` pieces, each
//! re-scanned from where the last stopped. The window passed to `longest_match` reproduces that.

use std::sync::OnceLock;

use crate::dfa::{self, Dfa, Re, alt, opt, plus, repeat, seq, set, star};
use crate::stream::Tokenizer;
use crate::token::Tok;
use crate::unicode::tables::*;

pub(crate) const ALPHANUM: &str = "<ALPHANUM>";
pub(crate) const NUM: &str = "<NUM>";
pub(crate) const SOUTHEAST_ASIAN: &str = "<SOUTHEAST_ASIAN>";
pub(crate) const IDEOGRAPHIC: &str = "<IDEOGRAPHIC>";
pub(crate) const HIRAGANA: &str = "<HIRAGANA>";
pub(crate) const KATAKANA: &str = "<KATAKANA>";
pub(crate) const HANGUL: &str = "<HANGUL>";
pub(crate) const EMOJI: &str = "<EMOJI>";

pub(crate) const MAX_TOKEN_LENGTH_LIMIT: usize = 1024 * 1024;

/// The UAX#29 macros shared by `standard` and `uax_url_email`.
pub(crate) struct Uax29 {
    pub emoji: Re,
    pub numeric: Re,
    pub hangul: Re,
    pub katakana: Re,
    pub word: Re,
    pub southeast_asian: Re,
    pub ideographic: Re,
    pub hiragana: Re,
}

fn v(r: &[(u32, u32)]) -> Vec<(u32, u32)> {
    r.to_vec()
}

pub(crate) fn uax29() -> Uax29 {
    let format = v(WB12_FORMAT);
    let extend = v(WB12_EXTEND);
    let zwj = v(WB12_ZWJ);
    let fez = dfa::union(&[&format, &extend, &zwj]);
    // [\p{WB:Format}\p{WB:Extend}\p{WB:ZWJ}]*
    let ext_fmt_zwj = || star(set(&fez));
    let fez_sans_pres = dfa::difference(&fez, &dfa::chars("\u{FE0E}\u{FE0F}"));
    let ext_fmt_zwj_sans_pres_sel = || star(set(&fez_sans_pres));
    let ex = |s: &[(u32, u32)]| seq(vec![set(s), ext_fmt_zwj()]);

    let keycap_base = dfa::chars("0123456789#*");
    let keycap_base_ex = seq(vec![set(&keycap_base), ext_fmt_zwj_sans_pres_sel()]);
    let keycap_ex = seq(vec![
        set(&dfa::chars("\u{20E3}")),
        ext_fmt_zwj_sans_pres_sel(),
    ]);
    let accidental = dfa::chars("©®™\u{3030}\u{303D}");
    let ri = v(WB12_REGIONAL_INDICATOR);
    let emoji_rkam = dfa::union(&[&ri, &keycap_base, &accidental, EMOJI12_EMOJI_MODIFIER]);
    let emoji_sans_rkam = dfa::difference(EMOJI12_EMOJI, &emoji_rkam);
    let emoji_char = dfa::union(&[EMOJI12_EXTENDED_PICTOGRAPHIC, &emoji_sans_rkam]);
    let emoji_char_ex = seq(vec![set(&emoji_char), ext_fmt_zwj_sans_pres_sel()]);
    let modifier_base_ex = seq(vec![
        set(EMOJI12_EMOJI_MODIFIER_BASE),
        ext_fmt_zwj_sans_pres_sel(),
    ]);
    let modifier_ex = seq(vec![
        set(EMOJI12_EMOJI_MODIFIER),
        ext_fmt_zwj_sans_pres_sel(),
    ]);
    let pres_sel = dfa::chars("\u{FE0F}");
    // ( \p{WB:ZWJ}* {EmojiCharEx} {EmojiPresentationSelector}? )
    //   | ( ( \p{WB:ZWJ}* {EmojiModifierBaseEx} )? {EmojiModifierEx} )
    let char_or_seq = || {
        alt(vec![
            seq(vec![
                star(set(&zwj)),
                emoji_char_ex.clone(),
                opt(set(&pres_sel)),
            ]),
            seq(vec![
                opt(seq(vec![star(set(&zwj)), modifier_base_ex.clone()])),
                modifier_ex.clone(),
            ]),
        ])
    };
    let tag_spec = vec![(0xE0020, 0xE007E)];
    let tag_term = vec![(0xE007F, 0xE007F)];
    let emoji = alt(vec![
        seq(vec![
            char_or_seq(),
            alt(vec![
                star(seq(vec![set(&zwj), char_or_seq()])),
                seq(vec![plus(set(&tag_spec)), set(&tag_term)]),
            ]),
        ]),
        seq(vec![keycap_base_ex, opt(set(&pres_sel)), keycap_ex]),
        repeat(ex(&ri), 2, Some(2)),
    ]);

    let aletter_hebrew = dfa::union(&[WB12_ALETTER, WB12_HEBREW_LETTER]);
    let hangul_ex = || ex(&dfa::intersect(SCRIPT12_HANGUL, &aletter_hebrew));
    let ahletter_ex = || ex(&aletter_hebrew);
    let numeric_ex = || ex(WB12_NUMERIC);
    let katakana_ex = || ex(WB12_KATAKANA);
    let mid_letter_ex = || {
        ex(&dfa::union(&[
            WB12_MIDLETTER,
            WB12_MIDNUMLET,
            WB12_SINGLE_QUOTE,
        ]))
    };
    let mid_numeric_ex = || {
        ex(&dfa::union(&[
            WB12_MIDNUM,
            WB12_MIDNUMLET,
            WB12_SINGLE_QUOTE,
        ]))
    };
    let extend_num_let_ex = || ex(WB12_EXTENDNUMLET);
    let single_quote_ex = || ex(WB12_SINGLE_QUOTE);
    let double_quote_ex = || ex(WB12_DOUBLE_QUOTE);
    let hebrew_letter_ex = || ex(WB12_HEBREW_LETTER);

    // {ExtendNumLetEx}* {NumericEx} ( ( {ExtendNumLetEx}* | {MidNumericEx} ) {NumericEx} )* {ExtendNumLetEx}*
    let numeric = seq(vec![
        star(extend_num_let_ex()),
        numeric_ex(),
        star(seq(vec![
            alt(vec![star(extend_num_let_ex()), mid_numeric_ex()]),
            numeric_ex(),
        ])),
        star(extend_num_let_ex()),
    ]);

    // ( {KatakanaEx} ( {ExtendNumLetEx}* {KatakanaEx} )*
    // | ( {HebrewLetterEx} ( {SingleQuoteEx} | {DoubleQuoteEx} {HebrewLetterEx} )
    //   | {NumericEx} ( ( {ExtendNumLetEx}* | {MidNumericEx} ) {NumericEx} )*
    //   | {AHLetterEx} ( ( {ExtendNumLetEx}* | {MidLetterEx} ) {AHLetterEx} )*
    //   )+
    // )
    let word_inner = || {
        alt(vec![
            seq(vec![
                katakana_ex(),
                star(seq(vec![star(extend_num_let_ex()), katakana_ex()])),
            ]),
            plus(alt(vec![
                seq(vec![
                    hebrew_letter_ex(),
                    alt(vec![
                        single_quote_ex(),
                        seq(vec![double_quote_ex(), hebrew_letter_ex()]),
                    ]),
                ]),
                seq(vec![
                    numeric_ex(),
                    star(seq(vec![
                        alt(vec![star(extend_num_let_ex()), mid_numeric_ex()]),
                        numeric_ex(),
                    ])),
                ]),
                seq(vec![
                    ahletter_ex(),
                    star(seq(vec![
                        alt(vec![star(extend_num_let_ex()), mid_letter_ex()]),
                        ahletter_ex(),
                    ])),
                ]),
            ])),
        ])
    };
    let word = seq(vec![
        star(extend_num_let_ex()),
        word_inner(),
        star(seq(vec![plus(extend_num_let_ex()), word_inner()])),
        star(extend_num_let_ex()),
    ]);

    Uax29 {
        emoji,
        numeric,
        hangul: plus(hangul_ex()),
        katakana: plus(katakana_ex()),
        word,
        southeast_asian: plus(ex(LB12_SA)),
        ideographic: ex(SCRIPT12_HAN),
        hiragana: ex(SCRIPT12_HIRAGANA),
    }
}

/// Token types by rule index of [`dfa_standard`].
const RULE_TYPES: [&str; 8] = [
    EMOJI,
    NUM,
    HANGUL,
    KATAKANA,
    ALPHANUM,
    SOUTHEAST_ASIAN,
    IDEOGRAPHIC,
    HIRAGANA,
];

fn dfa_standard() -> &'static Dfa {
    static DFA: OnceLock<Dfa> = OnceLock::new();
    DFA.get_or_init(|| {
        let u = uax29();
        Dfa::new(&[
            u.emoji,
            u.numeric,
            u.hangul,
            u.katakana,
            u.word,
            u.southeast_asian,
            u.ideographic,
            u.hiragana,
        ])
    })
}

/// `StandardTokenizer`.
pub(crate) struct StandardTokenizer {
    dfa: &'static Dfa,
    buf: super::jflex::JflexBuffer,
    /// Byte position and UTF-16 position of the scanner.
    pos: usize,
    upos: usize,
    exhausted: bool,
}

impl StandardTokenizer {
    pub fn new(max_len: usize) -> StandardTokenizer {
        StandardTokenizer {
            dfa: dfa_standard(),
            buf: super::jflex::JflexBuffer::new(max_len),
            pos: 0,
            upos: 0,
            exhausted: false,
        }
    }
}

impl Tokenizer for StandardTokenizer {
    fn reset(&mut self, _text: &str) {
        self.pos = 0;
        self.upos = 0;
        self.exhausted = false;
        self.buf.reset();
    }

    /// The scanner's position (`yychar + yylength`): the end of the last token until the text
    /// has been read to the end (a `limit` filter can end the stream early).
    fn end(&mut self, text: &str, t: &mut Tok) {
        let end = if self.exhausted {
            crate::unicode::utf16_len(text)
        } else {
            self.upos
        } as u32;
        t.start = end;
        t.end = end;
    }

    #[inline]
    fn next(&mut self, text: &str, t: &mut Tok) -> bool {
        let bytes = text.as_bytes();
        let dfa = self.dfa;
        while self.pos < bytes.len() {
            match self.buf.scan(
                text,
                self.pos,
                self.upos,
                |s, c| dfa.step(s, c),
                |s| dfa.accepts(s),
            ) {
                super::jflex::Scan::Token(rule, end, ulen) => {
                    t.term.push_str(&text[self.pos..end]);
                    t.start = self.upos as u32;
                    t.end = (self.upos + ulen) as u32;
                    t.kind = RULE_TYPES[rule];
                    self.pos = end;
                    self.upos += ulen;
                    return true;
                }
                super::jflex::Scan::Eof => {
                    self.exhausted = true;
                    return false;
                }
                super::jflex::Scan::NoMatch => {}
            }
            // `[^]`: skip one code point.
            let b = bytes[self.pos];
            if b < 0x80 {
                self.pos += 1;
                self.upos += 1;
            } else {
                let c = text[self.pos..].chars().next().unwrap_or(' ');
                self.pos += c.len_utf8();
                self.upos += c.len_utf16();
            }
        }
        self.exhausted = true;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(text: &str, max: usize) -> Vec<(String, &'static str, u32, u32)> {
        let mut tk = StandardTokenizer::new(max);
        tk.reset(text);
        let mut out = Vec::new();
        let mut t = Tok::default();
        loop {
            t.clear();
            if !tk.next(text, &mut t) {
                break;
            }
            out.push((t.term.clone(), t.kind, t.start, t.end));
        }
        out
    }

    fn terms(text: &str) -> Vec<String> {
        tokens(text, 255).into_iter().map(|t| t.0).collect()
    }

    #[test]
    fn basic_words_and_numbers() {
        assert_eq!(
            terms("The 2 QUICK Brown-Foxes jumped over the lazy dog's bone."),
            vec![
                "The", "2", "QUICK", "Brown", "Foxes", "jumped", "over", "the", "lazy", "dog's",
                "bone"
            ]
        );
        let t = tokens("3.14 1,000 v1.2 a_b", 255);
        assert_eq!(t[0], ("3.14".into(), NUM, 0, 4));
        assert_eq!(t[1], ("1,000".into(), NUM, 5, 10));
        assert_eq!(t[2], ("v1.2".into(), ALPHANUM, 11, 15));
        assert_eq!(t[3], ("a_b".into(), ALPHANUM, 16, 19));
    }

    #[test]
    fn cjk_emoji_and_offsets() {
        let t = tokens("我是中国人 ひらがな カタカナ 한국어", 255);
        let kinds: Vec<_> = t.iter().map(|t| t.1).collect();
        assert_eq!(&kinds[..5], &[IDEOGRAPHIC; 5]);
        assert_eq!(t[5].1, HIRAGANA);
        assert_eq!(t.iter().filter(|t| t.1 == HIRAGANA).count(), 4);
        assert!(t.iter().any(|t| t.0 == "カタカナ" && t.1 == KATAKANA));
        assert!(t.iter().any(|t| t.0 == "한국어" && t.1 == HANGUL));
        let t = tokens("a 😀 👍🏽 🇭🇺 b", 255);
        assert_eq!(t[1], ("😀".into(), EMOJI, 2, 4));
        assert_eq!(t[2], ("👍🏽".into(), EMOJI, 5, 9));
        assert_eq!(t[3], ("🇭🇺".into(), EMOJI, 10, 14));
        assert_eq!(t[4], ("b".into(), ALPHANUM, 15, 16));
        assert_eq!(tokens("ภาษาไทย", 255)[0].1, SOUTHEAST_ASIAN);
    }

    #[test]
    fn max_token_length_splits() {
        let t = tokens("abcdefghij", 4);
        assert_eq!(
            t.iter().map(|t| t.0.as_str()).collect::<Vec<_>>(),
            vec!["abcd", "efgh", "ij"]
        );
        assert_eq!((t[1].2, t[1].3), (4, 8));
        // A supplementary character that cannot fit the one-slot buffer ends the stream, and one
        // that would complete a full buffer is held back.
        assert!(tokens("😀 a", 1).is_empty());
        assert_eq!(tokens("a 😀 b", 1).len(), 1);
        assert_eq!(
            tokens("ab😀c", 3)
                .iter()
                .map(|t| t.0.as_str())
                .collect::<Vec<_>>(),
            vec!["ab", "😀", "c"]
        );
    }
}
