//! Normalizers and stemmers for Arabic-script and Indic languages, and Indonesian: Lucene's
//! `ArabicNormalizer`/`ArabicStemmer`, `PersianNormalizer`/`PersianStemmer`,
//! `SoraniNormalizer`/`SoraniStemmer`, `HindiNormalizer`/`HindiStemmer`,
//! `BengaliNormalizer`/`BengaliStemmer`, `IndicNormalizer` and `IndonesianStemmer`.
//!
//! Same conventions as [`super::light`]: edits on a `char` buffer with a length. Loops that
//! delete the current character and step back (`i--` in Java) use a signed index.
#![allow(clippy::if_same_then_else)] // ported suffix chains keep Lucene's rule order

use super::light::{delete, delete_n, ends_with, starts_with};
use crate::unicode::{self, gc};

// ---------------------------------------------------------------------------------------------
// Arabic, Persian, Sorani

const ALEF: char = '\u{0627}';
const YEH: char = '\u{064A}';
const HEH: char = '\u{0647}';

fn is_harakat(c: char) -> bool {
    matches!(
        c,
        '\u{0640}'
            | '\u{064B}'
            | '\u{064C}'
            | '\u{064D}'
            | '\u{064E}'
            | '\u{064F}'
            | '\u{0650}'
            | '\u{0651}'
            | '\u{0652}'
    )
}

/// `ArabicNormalizer`.
pub(crate) fn arabic_normalize(s: &mut [char], mut len: usize) -> usize {
    let mut i = 0;
    while i < len {
        match s[i] {
            '\u{0622}' | '\u{0623}' | '\u{0625}' => s[i] = ALEF,
            '\u{0649}' => s[i] = YEH,
            '\u{0629}' => s[i] = HEH,
            c if is_harakat(c) => {
                len = delete(s, i, len);
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    len
}

const AR_PREFIXES: &[&str] = &[
    "\u{0627}\u{0644}",
    "\u{0648}\u{0627}\u{0644}",
    "\u{0628}\u{0627}\u{0644}",
    "\u{0643}\u{0627}\u{0644}",
    "\u{0641}\u{0627}\u{0644}",
    "\u{0644}\u{0644}",
    "\u{0648}",
];

const AR_SUFFIXES: &[&str] = &[
    "\u{0647}\u{0627}",
    "\u{0627}\u{0646}",
    "\u{0627}\u{062A}",
    "\u{0648}\u{0646}",
    "\u{064A}\u{0646}",
    "\u{064A}\u{0647}",
    "\u{064A}\u{0629}",
    "\u{0647}",
    "\u{0629}",
    "\u{064A}",
];

/// `ArabicStemmer` (light stemming: prefixes then suffixes).
pub(crate) fn arabic_stem(s: &mut [char], mut len: usize) -> usize {
    for p in AR_PREFIXES {
        let plen = p.chars().count();
        let ok = if plen == 1 && len < 4 {
            false
        } else {
            len >= plen + 2 && starts_with(s, len, p)
        };
        if ok {
            len = delete_n(s, 0, len, plen);
            break;
        }
    }
    for sfx in AR_SUFFIXES {
        let slen = sfx.chars().count();
        if len >= slen + 2 && ends_with(s, len, sfx) {
            len = delete_n(s, len - slen, len, slen);
        }
    }
    len
}

/// `PersianNormalizer`.
pub(crate) fn persian_normalize(s: &mut [char], mut len: usize) -> usize {
    let mut i = 0;
    while i < len {
        match s[i] {
            '\u{06CC}' | '\u{06D2}' => s[i] = YEH,
            '\u{06A9}' => s[i] = '\u{0643}',
            '\u{06C0}' | '\u{06C1}' => s[i] = HEH,
            '\u{0654}' => {
                len = delete(s, i, len);
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    len
}

const FA_SUFFIXES: &[&str] = &[
    "\u{0627}\u{062A}",
    "\u{0627}\u{0646}",
    "\u{062A}\u{0631}\u{064A}\u{0646}",
    "\u{062A}\u{0631}",
    "\u{064A}\u{064A}",
    "\u{064A}",
    "\u{0647}\u{0627}",
    "\u{200C}",
];

/// `PersianStemmer`.
pub(crate) fn persian_stem(s: &mut [char], mut len: usize) -> usize {
    for sfx in FA_SUFFIXES {
        let slen = sfx.chars().count();
        if len >= slen + 2 && ends_with(s, len, sfx) {
            len = delete_n(s, len - slen, len, slen);
        }
    }
    len
}

/// `SoraniNormalizer`.
pub(crate) fn sorani_normalize(s: &mut [char], mut len: usize) -> usize {
    const FARSI_YEH: char = '\u{06CC}';
    const KEHEH: char = '\u{06A9}';
    const AE: char = '\u{06D5}';
    const RREH: char = '\u{0695}';
    let mut i: isize = 0;
    while (i as usize) < len {
        let u = i as usize;
        match s[u] {
            '\u{064A}' | '\u{0649}' => s[u] = FARSI_YEH,
            '\u{0643}' => s[u] = KEHEH,
            '\u{200C}' => {
                if u > 0 && s[u - 1] == HEH {
                    s[u - 1] = AE;
                }
                len = delete(s, u, len);
                i -= 1;
            }
            HEH => {
                if u == len - 1 {
                    s[u] = AE;
                }
            }
            '\u{0629}' => s[u] = AE,
            '\u{06BE}' => s[u] = HEH,
            '\u{0631}' => {
                if u == 0 {
                    s[u] = RREH;
                }
            }
            '\u{0692}' => s[u] = RREH,
            c if is_harakat(c) => {
                len = delete(s, u, len);
                i -= 1;
            }
            c => {
                if unicode::get_type(c) == gc::FORMAT {
                    len = delete(s, u, len);
                    i -= 1;
                }
            }
        }
        i += 1;
    }
    len
}

/// `SoraniStemmer`.
pub(crate) fn sorani_stem(s: &mut [char], mut len: usize) -> usize {
    let e = |s: &[char], len: usize, x: &str| ends_with(s, len, x);
    if len > 5 && e(s, len, "دا") {
        len -= 2;
    } else if len > 4 && e(s, len, "نا") {
        len -= 1;
    } else if len > 6 && e(s, len, "ەوە") {
        len -= 3;
    }
    if len > 6 && (e(s, len, "مان") || e(s, len, "یان") || e(s, len, "تان")) {
        len -= 3;
    }
    if len > 6 && e(s, len, "ێکی") {
        return len - 3;
    } else if len > 7 && e(s, len, "یەکی") {
        return len - 4;
    }
    if len > 5 && e(s, len, "ێک") {
        return len - 2;
    } else if len > 6 && e(s, len, "یەک") {
        return len - 3;
    } else if len > 6 && e(s, len, "ەکە") {
        return len - 3;
    } else if len > 5 && e(s, len, "کە") {
        return len - 2;
    } else if len > 7 && e(s, len, "ەکان") {
        return len - 4;
    } else if len > 6 && e(s, len, "کان") {
        return len - 3;
    } else if len > 7 && e(s, len, "یانی") {
        return len - 4;
    } else if len > 6 && e(s, len, "انی") {
        return len - 3;
    } else if len > 6 && e(s, len, "یان") {
        return len - 3;
    } else if len > 5 && e(s, len, "ان") {
        return len - 2;
    } else if len > 7 && e(s, len, "یانە") {
        return len - 4;
    } else if len > 6 && e(s, len, "انە") {
        return len - 3;
    } else if len > 5 && (e(s, len, "ایە") || e(s, len, "ەیە")) {
        return len - 2;
    } else if len > 4 && e(s, len, "ە") {
        return len - 1;
    } else if len > 4 && e(s, len, "ی") {
        return len - 1;
    }
    len
}

// ---------------------------------------------------------------------------------------------
// Hindi, Bengali, Indic

/// `HindiNormalizer`.
pub(crate) fn hindi_normalize(s: &mut [char], mut len: usize) -> usize {
    let mut i = 0;
    while i < len {
        let c = s[i];
        let to = match c {
            '\u{0928}' => {
                if i + 1 < len && s[i + 1] == '\u{094D}' {
                    s[i] = '\u{0902}';
                    len = delete(s, i + 1, len);
                }
                None
            }
            '\u{0901}' => Some('\u{0902}'),
            '\u{093C}' | '\u{200D}' | '\u{200C}' | '\u{094D}' => {
                len = delete(s, i, len);
                continue;
            }
            '\u{0929}' => Some('\u{0928}'),
            '\u{0931}' => Some('\u{0930}'),
            '\u{0934}' => Some('\u{0933}'),
            '\u{0958}' => Some('\u{0915}'),
            '\u{0959}' => Some('\u{0916}'),
            '\u{095A}' => Some('\u{0917}'),
            '\u{095B}' => Some('\u{091C}'),
            '\u{095C}' => Some('\u{0921}'),
            '\u{095D}' => Some('\u{0922}'),
            '\u{095E}' => Some('\u{092B}'),
            '\u{095F}' => Some('\u{092F}'),
            '\u{0945}' | '\u{0946}' => Some('\u{0947}'),
            '\u{0949}' | '\u{094A}' => Some('\u{094B}'),
            '\u{090D}' | '\u{090E}' => Some('\u{090F}'),
            '\u{0911}' | '\u{0912}' => Some('\u{0913}'),
            '\u{0972}' => Some('\u{0905}'),
            '\u{0906}' => Some('\u{0905}'),
            '\u{0908}' => Some('\u{0907}'),
            '\u{090A}' => Some('\u{0909}'),
            '\u{0960}' => Some('\u{090B}'),
            '\u{0961}' => Some('\u{090C}'),
            '\u{0910}' => Some('\u{090F}'),
            '\u{0914}' => Some('\u{0913}'),
            '\u{0940}' => Some('\u{093F}'),
            '\u{0942}' => Some('\u{0941}'),
            '\u{0944}' => Some('\u{0943}'),
            '\u{0963}' => Some('\u{0962}'),
            '\u{0948}' => Some('\u{0947}'),
            '\u{094C}' => Some('\u{094B}'),
            _ => None,
        };
        if let Some(t) = to {
            s[i] = t;
        }
        i += 1;
    }
    len
}

/// Suffix tiers of the Hindi and Bengali stemmers: (length must exceed, characters removed,
/// suffixes), extracted from Lucene's sources code point for code point (some suffixes use
/// precomposed nukta letters, so their lengths differ from the removal counts).
type Tiers = &'static [(usize, usize, &'static [&'static str])];

fn tiered(s: &[char], len: usize, tiers: Tiers) -> usize {
    for &(min, remove, suffixes) in tiers {
        if len > min && suffixes.iter().any(|x| ends_with(s, len, x)) {
            return len - remove;
        }
    }
    len
}

const HI_TIERS: Tiers = &[
    (
        6,
        5,
        &[
            "\u{093E}\u{090F}\u{0902}\u{0917}\u{0940}",
            "\u{093E}\u{090F}\u{0902}\u{0917}\u{0947}",
            "\u{093E}\u{090A}\u{0902}\u{0917}\u{0940}",
            "\u{093E}\u{090A}\u{0902}\u{0917}\u{093E}",
            "\u{093E}\u{0907}\u{092F}\u{093E}\u{0901}",
            "\u{093E}\u{0907}\u{092F}\u{094B}\u{0902}",
            "\u{093E}\u{0907}\u{092F}\u{093E}\u{0902}",
        ],
    ),
    (
        5,
        4,
        &[
            "\u{093E}\u{090F}\u{0917}\u{0940}",
            "\u{093E}\u{090F}\u{0917}\u{093E}",
            "\u{093E}\u{0913}\u{0917}\u{0940}",
            "\u{093E}\u{0913}\u{0917}\u{0947}",
            "\u{090F}\u{0902}\u{0917}\u{0940}",
            "\u{0947}\u{0902}\u{0917}\u{0940}",
            "\u{090F}\u{0902}\u{0917}\u{0947}",
            "\u{0947}\u{0902}\u{0917}\u{0947}",
            "\u{0942}\u{0902}\u{0917}\u{0940}",
            "\u{0942}\u{0902}\u{0917}\u{093E}",
            "\u{093E}\u{0924}\u{0940}\u{0902}",
            "\u{0928}\u{093E}\u{0913}\u{0902}",
            "\u{0928}\u{093E}\u{090F}\u{0902}",
            "\u{0924}\u{093E}\u{0913}\u{0902}",
            "\u{0924}\u{093E}\u{090F}\u{0902}",
            "\u{093F}\u{092F}\u{093E}\u{0901}",
            "\u{093F}\u{092F}\u{094B}\u{0902}",
            "\u{093F}\u{092F}\u{093E}\u{0902}",
        ],
    ),
    (
        4,
        3,
        &[
            "\u{093E}\u{0915}\u{0930}",
            "\u{093E}\u{0907}\u{090F}",
            "\u{093E}\u{0908}\u{0902}",
            "\u{093E}\u{092F}\u{093E}",
            "\u{0947}\u{0917}\u{0940}",
            "\u{0947}\u{0917}\u{093E}",
            "\u{094B}\u{0917}\u{0940}",
            "\u{094B}\u{0917}\u{0947}",
            "\u{093E}\u{0928}\u{0947}",
            "\u{093E}\u{0928}\u{093E}",
            "\u{093E}\u{0924}\u{0947}",
            "\u{093E}\u{0924}\u{0940}",
            "\u{093E}\u{0924}\u{093E}",
            "\u{0924}\u{0940}\u{0902}",
            "\u{093E}\u{0913}\u{0902}",
            "\u{093E}\u{090F}\u{0902}",
            "\u{0941}\u{0913}\u{0902}",
            "\u{0941}\u{090F}\u{0902}",
            "\u{0941}\u{0906}\u{0902}",
        ],
    ),
    (
        3,
        2,
        &[
            "\u{0915}\u{0930}",
            "\u{093E}\u{0913}",
            "\u{093F}\u{090F}",
            "\u{093E}\u{0908}",
            "\u{093E}\u{090F}",
            "\u{0928}\u{0947}",
            "\u{0928}\u{0940}",
            "\u{0928}\u{093E}",
            "\u{0924}\u{0947}",
            "\u{0940}\u{0902}",
            "\u{0924}\u{0940}",
            "\u{0924}\u{093E}",
            "\u{093E}\u{0901}",
            "\u{093E}\u{0902}",
            "\u{094B}\u{0902}",
            "\u{0947}\u{0902}",
        ],
    ),
    (
        2,
        1,
        &[
            "\u{094B}", "\u{0947}", "\u{0942}", "\u{0941}", "\u{0940}", "\u{093F}", "\u{093E}",
        ],
    ),
];

/// `HindiStemmer`.
pub(crate) fn hindi_stem(s: &mut [char], len: usize) -> usize {
    tiered(s, len, HI_TIERS)
}

const BN_TIERS: Tiers = &[
    (
        9,
        8,
        &[
            "\u{09BF}\u{09DF}\u{09BE}\u{099B}\u{09BF}\u{09B2}\u{09BE}\u{09AE}",
            "\u{09BF}\u{09A4}\u{09C7}\u{099B}\u{09BF}\u{09B2}\u{09BE}\u{09AE}",
            "\u{09BF}\u{09A4}\u{09C7}\u{099B}\u{09BF}\u{09B2}\u{09C7}\u{09A8}",
            "\u{0987}\u{09A4}\u{09C7}\u{099B}\u{09BF}\u{09B2}\u{09C7}\u{09A8}",
            "\u{09BF}\u{09DF}\u{09BE}\u{099B}\u{09BF}\u{09B2}\u{09C7}\u{09A8}",
            "\u{0987}\u{09DF}\u{09BE}\u{099B}\u{09BF}\u{09B2}\u{09C7}\u{09A8}",
        ],
    ),
    (
        8,
        7,
        &[
            "\u{09BF}\u{09A4}\u{09C7}\u{099B}\u{09BF}\u{09B2}\u{09BF}",
            "\u{09BF}\u{09A4}\u{09C7}\u{099B}\u{09BF}\u{09B2}\u{09C7}",
            "\u{09BF}\u{09DF}\u{09BE}\u{099B}\u{09BF}\u{09B2}\u{09BE}",
            "\u{09BF}\u{09DF}\u{09BE}\u{099B}\u{09BF}\u{09B2}\u{09C7}",
            "\u{09BF}\u{09A4}\u{09C7}\u{099B}\u{09BF}\u{09B2}\u{09BE}",
            "\u{09BF}\u{09DF}\u{09BE}\u{099B}\u{09BF}\u{09B2}\u{09BF}",
            "\u{09DF}\u{09C7}\u{09A6}\u{09C7}\u{09B0}\u{0995}\u{09C7}",
        ],
    ),
    (
        7,
        6,
        &[
            "\u{09BF}\u{09A4}\u{09C7}\u{099B}\u{09BF}\u{09B8}",
            "\u{09BF}\u{09A4}\u{09C7}\u{099B}\u{09C7}\u{09A8}",
            "\u{09BF}\u{09DF}\u{09BE}\u{099B}\u{09BF}\u{09B8}",
            "\u{09BF}\u{09DF}\u{09BE}\u{099B}\u{09C7}\u{09A8}",
            "\u{09C7}\u{099B}\u{09BF}\u{09B2}\u{09BE}\u{09AE}",
            "\u{09C7}\u{099B}\u{09BF}\u{09B2}\u{09C7}\u{09A8}",
            "\u{09C7}\u{09A6}\u{09C7}\u{09B0}\u{0995}\u{09C7}",
        ],
    ),
    (
        6,
        5,
        &[
            "\u{09BF}\u{09A4}\u{09C7}\u{099B}\u{09BF}",
            "\u{09BF}\u{09A4}\u{09C7}\u{099B}\u{09BE}",
            "\u{09BF}\u{09A4}\u{09C7}\u{099B}\u{09C7}",
            "\u{099B}\u{09BF}\u{09B2}\u{09BE}\u{09AE}",
            "\u{099B}\u{09BF}\u{09B2}\u{09C7}\u{09A8}",
            "\u{09BF}\u{09DF}\u{09BE}\u{099B}\u{09BF}",
            "\u{09BF}\u{09DF}\u{09BE}\u{099B}\u{09BE}",
            "\u{09BF}\u{09DF}\u{09BE}\u{099B}\u{09C7}",
            "\u{09C7}\u{099B}\u{09BF}\u{09B2}\u{09C7}",
            "\u{09C7}\u{099B}\u{09BF}\u{09B2}\u{09BE}",
            "\u{09DF}\u{09C7}\u{09A6}\u{09C7}\u{09B0}",
            "\u{09A6}\u{09C7}\u{09B0}\u{0995}\u{09C7}",
        ],
    ),
    (
        5,
        4,
        &[
            "\u{09BF}\u{09B2}\u{09BE}\u{09AE}",
            "\u{09BF}\u{09B2}\u{09C7}\u{09A8}",
            "\u{09BF}\u{09A4}\u{09BE}\u{09AE}",
            "\u{09BF}\u{09A4}\u{09C7}\u{09A8}",
            "\u{09BF}\u{09AC}\u{09C7}\u{09A8}",
            "\u{099B}\u{09BF}\u{09B2}\u{09BF}",
            "\u{099B}\u{09BF}\u{09B2}\u{09C7}",
            "\u{099B}\u{09BF}\u{09B2}\u{09BE}",
            "\u{09A4}\u{09C7}\u{099B}\u{09C7}",
            "\u{09BF}\u{09A4}\u{09C7}\u{099B}",
            "\u{0996}\u{09BE}\u{09A8}\u{09BE}",
            "\u{0996}\u{09BE}\u{09A8}\u{09BF}",
            "\u{0997}\u{09C1}\u{09B2}\u{09CB}",
            "\u{0997}\u{09C1}\u{09B2}\u{09BF}",
            "\u{09DF}\u{09C7}\u{09B0}\u{09BE}",
            "\u{09C7}\u{09A6}\u{09C7}\u{09B0}",
        ],
    ),
    (
        4,
        3,
        &[
            "\u{09B2}\u{09BE}\u{09AE}",
            "\u{09BF}\u{09B2}\u{09BF}",
            "\u{0987}\u{09B2}\u{09BF}",
            "\u{09BF}\u{09B2}\u{09C7}",
            "\u{0987}\u{09B2}\u{09C7}",
            "\u{09B2}\u{09C7}\u{09A8}",
            "\u{09BF}\u{09B2}\u{09BE}",
            "\u{0987}\u{09B2}\u{09BE}",
            "\u{09A4}\u{09BE}\u{09AE}",
            "\u{09BF}\u{09A4}\u{09BF}",
            "\u{0987}\u{09A4}\u{09BF}",
            "\u{09BF}\u{09A4}\u{09C7}",
            "\u{0987}\u{09A4}\u{09C7}",
            "\u{09A4}\u{09C7}\u{09A8}",
            "\u{09BF}\u{09A4}\u{09BE}",
            "\u{09BF}\u{09AC}\u{09BE}",
            "\u{0987}\u{09AC}\u{09BE}",
            "\u{09BF}\u{09AC}\u{09BF}",
            "\u{0987}\u{09AC}\u{09BF}",
            "\u{09AC}\u{09C7}\u{09A8}",
            "\u{09BF}\u{09AC}\u{09C7}",
            "\u{0987}\u{09AC}\u{09C7}",
            "\u{099B}\u{09C7}\u{09A8}",
            "\u{09DF}\u{09CB}\u{09A8}",
            "\u{09DF}\u{09C7}\u{09B0}",
            "\u{09C7}\u{09B0}\u{09BE}",
            "\u{09A6}\u{09C7}\u{09B0}",
        ],
    ),
    (
        3,
        2,
        &[
            "\u{09BF}\u{09B8}",
            "\u{09C7}\u{09A8}",
            "\u{09B2}\u{09BF}",
            "\u{09B2}\u{09C7}",
            "\u{09B2}\u{09BE}",
            "\u{09A4}\u{09BF}",
            "\u{09A4}\u{09C7}",
            "\u{09A4}\u{09BE}",
            "\u{09AC}\u{09BF}",
            "\u{09AC}\u{09C7}",
            "\u{09AC}\u{09BE}",
            "\u{099B}\u{09BF}",
            "\u{099B}\u{09BE}",
            "\u{099B}\u{09C7}",
            "\u{09C1}\u{09A8}",
            "\u{09C1}\u{0995}",
            "\u{099F}\u{09BE}",
            "\u{099F}\u{09BF}",
            "\u{09A8}\u{09BF}",
            "\u{09C7}\u{09B0}",
            "\u{09A4}\u{09C7}",
            "\u{09B0}\u{09BE}",
            "\u{0995}\u{09C7}",
        ],
    ),
    (
        2,
        1,
        &[
            "\u{09BF}", "\u{09C0}", "\u{09BE}", "\u{09CB}", "\u{09C7}", "\u{09AC}", "\u{09A4}",
        ],
    ),
];

/// `BengaliStemmer`.
pub(crate) fn bengali_stem(s: &mut [char], len: usize) -> usize {
    tiered(s, len, BN_TIERS)
}

/// `BengaliNormalizer`.
pub(crate) fn bengali_normalize(s: &mut [char], mut len: usize) -> usize {
    let mut i: isize = 0;
    while (i as usize) < len {
        let u = i as usize;
        match s[u] {
            '\u{0981}' => {
                len = delete(s, u, len);
                i -= 1;
            }
            '\u{09C0}' => s[u] = '\u{09BF}',
            '\u{09C2}' => s[u] = '\u{09C1}',
            '\u{0995}' => {
                if u + 2 < len && s[u + 1] == '\u{09CD}' && s[u + 2] == '\u{09BF}' {
                    if u == 0 {
                        s[u] = '\u{0996}';
                        len = delete(s, u + 2, len);
                        len = delete(s, u + 1, len);
                    } else {
                        s[u + 1] = '\u{0996}';
                        len = delete(s, u + 2, len);
                    }
                }
            }
            '\u{0999}' => s[u] = '\u{0982}',
            '\u{09AF}' => {
                if i - 2 == 0 && s[u - 1] == '\u{09CD}' {
                    s[u - 1] = '\u{09C7}';
                    if u + 1 < len && s[u + 1] == '\u{09BE}' {
                        len = delete(s, u + 1, len);
                    }
                    len = delete(s, u, len);
                    i -= 1;
                } else if i >= 1 && s[u - 1] == '\u{09CD}' {
                    len = delete(s, u, len);
                    len = delete(s, u - 1, len);
                    i -= 2;
                }
            }
            '\u{09AC}' => {
                if (i >= 1 && s[u - 1] != '\u{09CD}') || i == 0 {
                } else if i - 2 == 0 {
                    len = delete(s, u, len);
                    len = delete(s, u - 1, len);
                    i -= 2;
                } else if i - 5 >= 0 && s[u - 3] == '\u{09CD}' {
                    len = delete(s, u, len);
                    len = delete(s, u - 1, len);
                    i -= 2;
                } else if i - 2 >= 0 {
                    s[u - 1] = s[u - 2];
                    len = delete(s, u, len);
                    i -= 1;
                }
            }
            '\u{0983}' => {
                if u == len - 1 {
                    if len <= 3 {
                        s[u] = '\u{09B9}';
                    } else {
                        len = delete(s, u, len);
                    }
                } else {
                    s[u] = s[u + 1];
                }
            }
            '\u{09B6}' | '\u{09B7}' => s[u] = '\u{09B8}',
            '\u{09A3}' => s[u] = '\u{09A8}',
            '\u{09DC}' | '\u{09DD}' => s[u] = '\u{09B0}',
            '\u{09CE}' => s[u] = '\u{09A4}',
            _ => {}
        }
        i += 1;
    }
    len
}

const DEVANAGARI: u16 = 1;
const BENGALI: u16 = 2;
const GURMUKHI: u16 = 4;
const GUJARATI: u16 = 8;
const ORIYA: u16 = 16;
const TAMIL: u16 = 32;
const TELUGU: u16 = 64;
const KANNADA: u16 = 128;
const MALAYALAM: u16 = 256;

/// `IndicNormalizer.decompositions`: (first, second, third or -1 / 0xFF for ZWJ, composed,
/// scripts).
const DECOMPOSITIONS: &[(i32, i32, i32, i32, u16)] = &[
    (0x05, 0x3E, 0x45, 0x11, DEVANAGARI | GUJARATI),
    (0x05, 0x3E, 0x46, 0x12, DEVANAGARI),
    (0x05, 0x3E, 0x47, 0x13, DEVANAGARI | GUJARATI),
    (0x05, 0x3E, 0x48, 0x14, DEVANAGARI | GUJARATI),
    (
        0x05,
        0x3E,
        -1,
        0x06,
        DEVANAGARI | BENGALI | GURMUKHI | GUJARATI | ORIYA,
    ),
    (0x05, 0x45, -1, 0x72, DEVANAGARI),
    (0x05, 0x45, -1, 0x0D, GUJARATI),
    (0x05, 0x46, -1, 0x04, DEVANAGARI),
    (0x05, 0x47, -1, 0x0F, GUJARATI),
    (0x05, 0x48, -1, 0x10, GURMUKHI | GUJARATI),
    (0x05, 0x49, -1, 0x11, DEVANAGARI | GUJARATI),
    (0x05, 0x4A, -1, 0x12, DEVANAGARI),
    (0x05, 0x4B, -1, 0x13, DEVANAGARI | GUJARATI),
    (0x05, 0x4C, -1, 0x14, DEVANAGARI | GURMUKHI | GUJARATI),
    (0x06, 0x45, -1, 0x11, DEVANAGARI | GUJARATI),
    (0x06, 0x46, -1, 0x12, DEVANAGARI),
    (0x06, 0x47, -1, 0x13, DEVANAGARI | GUJARATI),
    (0x06, 0x48, -1, 0x14, DEVANAGARI | GUJARATI),
    (0x07, 0x57, -1, 0x08, MALAYALAM),
    (0x09, 0x41, -1, 0x0A, DEVANAGARI),
    (0x09, 0x57, -1, 0x0A, TAMIL | MALAYALAM),
    (0x0E, 0x46, -1, 0x10, MALAYALAM),
    (0x0F, 0x45, -1, 0x0D, DEVANAGARI),
    (0x0F, 0x46, -1, 0x0E, DEVANAGARI),
    (0x0F, 0x47, -1, 0x10, DEVANAGARI),
    (0x0F, 0x57, -1, 0x10, ORIYA),
    (0x12, 0x3E, -1, 0x13, MALAYALAM),
    (0x12, 0x4C, -1, 0x14, TELUGU | KANNADA),
    (0x12, 0x55, -1, 0x13, TELUGU),
    (0x12, 0x57, -1, 0x14, TAMIL | MALAYALAM),
    (0x13, 0x57, -1, 0x14, ORIYA),
    (0x15, 0x3C, -1, 0x58, DEVANAGARI),
    (0x16, 0x3C, -1, 0x59, DEVANAGARI | GURMUKHI),
    (0x17, 0x3C, -1, 0x5A, DEVANAGARI | GURMUKHI),
    (0x1C, 0x3C, -1, 0x5B, DEVANAGARI | GURMUKHI),
    (0x21, 0x3C, -1, 0x5C, DEVANAGARI | BENGALI | ORIYA),
    (0x22, 0x3C, -1, 0x5D, DEVANAGARI | BENGALI | ORIYA),
    (0x23, 0x4D, 0xFF, 0x7A, MALAYALAM),
    (0x24, 0x4D, 0xFF, 0x4E, BENGALI),
    (0x28, 0x3C, -1, 0x29, DEVANAGARI),
    (0x28, 0x4D, 0xFF, 0x7B, MALAYALAM),
    (0x2B, 0x3C, -1, 0x5E, DEVANAGARI | GURMUKHI),
    (0x2F, 0x3C, -1, 0x5F, DEVANAGARI | BENGALI),
    (0x2C, 0x41, 0x41, 0x0B, TELUGU),
    (0x30, 0x3C, -1, 0x31, DEVANAGARI),
    (0x30, 0x4D, 0xFF, 0x7C, MALAYALAM),
    (0x32, 0x4D, 0xFF, 0x7D, MALAYALAM),
    (0x33, 0x3C, -1, 0x34, DEVANAGARI),
    (0x33, 0x4D, 0xFF, 0x7E, MALAYALAM),
    (0x35, 0x41, -1, 0x2E, TELUGU),
    (0x3E, 0x45, -1, 0x49, DEVANAGARI | GUJARATI),
    (0x3E, 0x46, -1, 0x4A, DEVANAGARI),
    (0x3E, 0x47, -1, 0x4B, DEVANAGARI | GUJARATI),
    (0x3E, 0x48, -1, 0x4C, DEVANAGARI | GUJARATI),
    (0x3F, 0x55, -1, 0x40, KANNADA),
    (0x41, 0x41, -1, 0x42, GURMUKHI),
    (0x46, 0x3E, -1, 0x4A, TAMIL | MALAYALAM),
    (0x46, 0x42, 0x55, 0x4B, KANNADA),
    (0x46, 0x42, -1, 0x4A, KANNADA),
    (0x46, 0x46, -1, 0x48, MALAYALAM),
    (0x46, 0x55, -1, 0x47, TELUGU | KANNADA),
    (0x46, 0x56, -1, 0x48, TELUGU | KANNADA),
    (0x46, 0x57, -1, 0x4C, TAMIL | MALAYALAM),
    (0x47, 0x3E, -1, 0x4B, BENGALI | ORIYA | TAMIL | MALAYALAM),
    (0x47, 0x57, -1, 0x4C, BENGALI | ORIYA),
    (0x4A, 0x55, -1, 0x4B, KANNADA),
    (0x72, 0x3F, -1, 0x07, GURMUKHI),
    (0x72, 0x40, -1, 0x08, GURMUKHI),
    (0x72, 0x47, -1, 0x0F, GURMUKHI),
    (0x73, 0x41, -1, 0x09, GURMUKHI),
    (0x73, 0x42, -1, 0x0A, GURMUKHI),
    (0x73, 0x4B, -1, 0x13, GURMUKHI),
];

/// The Indic block of `c` (Devanagari … Malayalam: 0x0900–0x0D7F, 128 code points each).
fn indic_block(c: char) -> Option<(u16, u32)> {
    let cp = c as u32;
    if !(0x0900..0x0D80).contains(&cp) {
        return None;
    }
    let idx = (cp - 0x0900) / 0x80;
    Some((1 << idx, 0x0900 + idx * 0x80))
}

/// `IndicNormalizer`: composes decomposed vowel signs and nukta forms per script.
pub(crate) fn indic_normalize(s: &mut [char], mut len: usize) -> usize {
    let mut i = 0;
    while i < len {
        if let Some((flag, base)) = indic_block(s[i]) {
            let ch0 = (s[i] as u32 - base) as i32;
            if DECOMPOSITIONS.iter().any(|d| d.0 == ch0 && d.4 & flag != 0) {
                len = indic_compose(ch0, flag, base, s, i, len);
            }
        }
        i += 1;
    }
    len
}

fn indic_compose(
    ch0: i32,
    flag: u16,
    base: u32,
    s: &mut [char],
    pos: usize,
    mut len: usize,
) -> usize {
    if pos + 1 >= len {
        return len;
    }
    let ch1 = s[pos + 1] as i32 - base as i32;
    if indic_block(s[pos + 1]).map(|b| b.0) != Some(flag) {
        return len;
    }
    let mut ch2 = -1;
    if pos + 2 < len {
        ch2 = s[pos + 2] as i32 - base as i32;
        if s[pos + 2] == '\u{200D}' {
            ch2 = 0xFF;
        } else if indic_block(s[pos + 2]).map(|b| b.0) != Some(flag) {
            ch2 = -1;
        }
    }
    for d in DECOMPOSITIONS {
        if d.0 == ch0 && d.4 & flag != 0 && d.1 == ch1 && (d.2 < 0 || d.2 == ch2) {
            s[pos] = char::from_u32(base + d.3 as u32).unwrap_or(s[pos]);
            len = delete(s, pos + 1, len);
            if d.2 >= 0 {
                len = delete(s, pos + 1, len);
            }
            return len;
        }
    }
    len
}

// ---------------------------------------------------------------------------------------------
// Indonesian

const REMOVED_KE: u32 = 1;
const REMOVED_PENG: u32 = 2;
const REMOVED_DI: u32 = 4;
const REMOVED_MENG: u32 = 8;
const REMOVED_TER: u32 = 16;
const REMOVED_BER: u32 = 32;
const REMOVED_PE: u32 = 64;

/// `IndonesianStemmer` (with derivational stemming, as `IndonesianStemFilter` uses it).
pub(crate) struct Indonesian {
    syllables: i32,
    flags: u32,
}

fn id_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')
}

impl Indonesian {
    pub fn stem(s: &mut [char], mut len: usize) -> usize {
        let mut st = Indonesian {
            syllables: s[..len].iter().filter(|&&c| id_vowel(c)).count() as i32,
            flags: 0,
        };
        if st.syllables > 2 {
            len = st.remove_particle(s, len);
        }
        if st.syllables > 2 {
            len = st.remove_possessive(s, len);
        }
        st.derivational(s, len)
    }

    fn derivational(&mut self, s: &mut [char], mut len: usize) -> usize {
        let old = len;
        if self.syllables > 2 {
            len = self.remove_first_order_prefix(s, len);
        }
        if old != len {
            let old = len;
            if self.syllables > 2 {
                len = self.remove_suffix(s, len);
            }
            if old != len && self.syllables > 2 {
                len = self.remove_second_order_prefix(s, len);
            }
        } else {
            if self.syllables > 2 {
                len = self.remove_second_order_prefix(s, len);
            }
            if self.syllables > 2 {
                len = self.remove_suffix(s, len);
            }
        }
        len
    }

    fn remove_particle(&mut self, s: &[char], len: usize) -> usize {
        if ends_with(s, len, "kah") || ends_with(s, len, "lah") || ends_with(s, len, "pun") {
            self.syllables -= 1;
            return len - 3;
        }
        len
    }

    fn remove_possessive(&mut self, s: &[char], len: usize) -> usize {
        if ends_with(s, len, "ku") || ends_with(s, len, "mu") {
            self.syllables -= 1;
            return len - 2;
        }
        if ends_with(s, len, "nya") {
            self.syllables -= 1;
            return len - 3;
        }
        len
    }

    fn cut(&mut self, flag: u32, s: &mut [char], len: usize, n: usize) -> usize {
        self.flags |= flag;
        self.syllables -= 1;
        delete_n(s, 0, len, n)
    }

    fn remove_first_order_prefix(&mut self, s: &mut [char], len: usize) -> usize {
        if starts_with(s, len, "meng") {
            return self.cut(REMOVED_MENG, s, len, 4);
        }
        if starts_with(s, len, "meny") && len > 4 && id_vowel(s[4]) {
            s[3] = 's';
            return self.cut(REMOVED_MENG, s, len, 3);
        }
        if starts_with(s, len, "men") || starts_with(s, len, "mem") {
            return self.cut(REMOVED_MENG, s, len, 3);
        }
        if starts_with(s, len, "me") {
            return self.cut(REMOVED_MENG, s, len, 2);
        }
        if starts_with(s, len, "peng") {
            return self.cut(REMOVED_PENG, s, len, 4);
        }
        if starts_with(s, len, "peny") && len > 4 && id_vowel(s[4]) {
            s[3] = 's';
            return self.cut(REMOVED_PENG, s, len, 3);
        }
        if starts_with(s, len, "peny") {
            return self.cut(REMOVED_PENG, s, len, 4);
        }
        if starts_with(s, len, "pen") && len > 3 && id_vowel(s[3]) {
            s[2] = 't';
            return self.cut(REMOVED_PENG, s, len, 2);
        }
        if starts_with(s, len, "pen") || starts_with(s, len, "pem") {
            return self.cut(REMOVED_PENG, s, len, 3);
        }
        if starts_with(s, len, "di") {
            return self.cut(REMOVED_DI, s, len, 2);
        }
        if starts_with(s, len, "ter") {
            return self.cut(REMOVED_TER, s, len, 3);
        }
        if starts_with(s, len, "ke") {
            return self.cut(REMOVED_KE, s, len, 2);
        }
        len
    }

    fn remove_second_order_prefix(&mut self, s: &mut [char], len: usize) -> usize {
        if starts_with(s, len, "ber") {
            return self.cut(REMOVED_BER, s, len, 3);
        }
        if len == 7 && starts_with(s, len, "belajar") {
            return self.cut(REMOVED_BER, s, len, 3);
        }
        if starts_with(s, len, "be") && len > 4 && !id_vowel(s[2]) && s[3] == 'e' && s[4] == 'r' {
            return self.cut(REMOVED_BER, s, len, 2);
        }
        if starts_with(s, len, "per") {
            return self.cut(0, s, len, 3);
        }
        if len == 7 && starts_with(s, len, "pelajar") {
            return self.cut(0, s, len, 3);
        }
        if starts_with(s, len, "pe") {
            return self.cut(REMOVED_PE, s, len, 2);
        }
        len
    }

    fn remove_suffix(&mut self, s: &[char], len: usize) -> usize {
        let f = self.flags;
        if ends_with(s, len, "kan") && f & (REMOVED_KE | REMOVED_PENG | REMOVED_PE) == 0 {
            self.syllables -= 1;
            return len - 3;
        }
        if ends_with(s, len, "an") && f & (REMOVED_DI | REMOVED_MENG | REMOVED_TER) == 0 {
            self.syllables -= 1;
            return len - 2;
        }
        if ends_with(s, len, "i")
            && !ends_with(s, len, "si")
            && f & (REMOVED_BER | REMOVED_KE | REMOVED_PENG) == 0
        {
            self.syllables -= 1;
            return len - 1;
        }
        len
    }
}
