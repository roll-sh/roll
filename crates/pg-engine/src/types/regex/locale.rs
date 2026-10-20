//! Character classification and case mapping for the regex engine (regc_pg_locale.c), plus the
//! character vectors classes and ranges are built from (regc_cvec.c, regc_locale.c's tables).

use super::unicode_tables::{ALPHABETIC, CATEGORIES, LOWERCASE, UPPERCASE, WHITE_SPACE};
use super::{Chr, MAX_SIMPLE_CHR};
use crate::types::collation::probes;
use std::sync::OnceLock;

/// How a collation classifies characters (regc_pg_locale.c's PG_Locale_Strategy, for the
/// collations the engine has).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Strategy {
    /// PG_REGEX_STRATEGY_C: a collation whose ctype is C (C, POSIX, ucs_basic); ASCII only.
    C,
    /// PG_REGEX_STRATEGY_BUILTIN: the builtin provider's Unicode semantics; `posix` unless the
    /// locale maps case fully (C.UTF-8 vs PG_UNICODE_FAST).
    Builtin { posix: bool },
    /// PG_REGEX_STRATEGY_LIBC_WIDE: a libc locale's <wctype.h> functions (musl's).
    Libc,
    /// PG_REGEX_STRATEGY_ICU: ICU's uchar.h functions.
    Icu,
}

/// The default collation: the builtin provider's C.UTF-8 locale.
pub const DEFAULT_STRATEGY: Strategy = Strategy::Builtin { posix: true };

// Hard-wired character properties for C locale.
const PG_ISDIGIT: u8 = 0x01;
const PG_ISALPHA: u8 = 0x02;
const PG_ISALNUM: u8 = PG_ISDIGIT | PG_ISALPHA;
const PG_ISUPPER: u8 = 0x04;
const PG_ISLOWER: u8 = 0x08;
const PG_ISGRAPH: u8 = 0x10;
const PG_ISPRINT: u8 = 0x20;
const PG_ISPUNCT: u8 = 0x40;
const PG_ISSPACE: u8 = 0x80;

fn c_props(c: Chr) -> u8 {
    const P: u8 = PG_ISGRAPH | PG_ISPRINT | PG_ISPUNCT;
    match c {
        0x09..=0x0D => PG_ISSPACE,
        0x20 => PG_ISPRINT | PG_ISSPACE,
        0x30..=0x39 => PG_ISDIGIT | PG_ISGRAPH | PG_ISPRINT,
        0x41..=0x5A => PG_ISALPHA | PG_ISUPPER | PG_ISGRAPH | PG_ISPRINT,
        0x61..=0x7A => PG_ISALPHA | PG_ISLOWER | PG_ISGRAPH | PG_ISPRINT,
        0x21..=0x7E => P,
        _ => 0,
    }
}

// Unicode general categories (common/unicode_category.h).
const CAT_UNASSIGNED: u8 = 0;
const CAT_DECIMAL_NUMBER: u8 = 9;
const CAT_SPACE_SEPARATOR: u8 = 12;
const CAT_CONTROL: u8 = 15;
const CAT_SURROGATE: u8 = 18;

/// Whether a category is punctuation (Pd Ps Pe Pc Po Pi Pf) or a symbol (Sm Sc Sk So).
fn cat_is_punct(cat: u8) -> bool {
    matches!(cat, 19..=23 | 28 | 29)
}
fn cat_is_symbol(cat: u8) -> bool {
    matches!(cat, 24..=27)
}

/// `unicode_category`.
fn category(c: Chr) -> u8 {
    let (mut lo, mut hi) = (0usize, CATEGORIES.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        let (first, last, cat) = CATEGORIES[mid];
        if c < first {
            hi = mid;
        } else if c > last {
            lo = mid + 1;
        } else {
            return cat;
        }
    }
    CAT_UNASSIGNED
}

fn in_ranges(tbl: &[(u32, u32)], c: Chr) -> bool {
    let (mut lo, mut hi) = (0usize, tbl.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        let (first, last) = tbl[mid];
        if c < first {
            hi = mid;
        } else if c > last {
            lo = mid + 1;
        } else {
            return true;
        }
    }
    false
}

pub(crate) fn u_isdigit(c: Chr, posix: bool) -> bool {
    if posix {
        (0x30..=0x39).contains(&c)
    } else {
        category(c) == CAT_DECIMAL_NUMBER
    }
}
pub(crate) fn u_isalpha(c: Chr) -> bool {
    in_ranges(&ALPHABETIC, c)
}
fn u_isspace(c: Chr) -> bool {
    in_ranges(&WHITE_SPACE, c)
}
fn u_isblank(c: Chr) -> bool {
    c == 0x09 || category(c) == CAT_SPACE_SEPARATOR
}
fn u_isgraph(c: Chr) -> bool {
    let cat = category(c);
    !(cat == CAT_CONTROL || cat == CAT_SURROGATE || cat == CAT_UNASSIGNED || u_isspace(c))
}
fn u_isprint(c: Chr) -> bool {
    category(c) != CAT_CONTROL && (u_isgraph(c) || u_isblank(c))
}
fn u_ispunct(c: Chr, posix: bool) -> bool {
    if posix {
        if u_isalpha(c) {
            return false;
        }
        let cat = category(c);
        cat_is_punct(cat) || cat_is_symbol(cat)
    } else {
        cat_is_punct(category(c))
    }
}

/// The character-class probes of regc_pg_locale.c.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    Digit,
    Alpha,
    Alnum,
    Word,
    Upper,
    Lower,
    Graph,
    Print,
    Punct,
    Space,
}

impl Strategy {
    pub fn probe(self, p: Probe, c: Chr) -> bool {
        match p {
            Probe::Digit => self.isdigit(c),
            Probe::Alpha => self.isalpha(c),
            Probe::Alnum => self.isalnum(c),
            Probe::Word => self.isword(c),
            Probe::Upper => self.isupper(c),
            Probe::Lower => self.islower(c),
            Probe::Graph => self.isgraph(c),
            Probe::Print => self.isprint(c),
            Probe::Punct => self.ispunct(c),
            Probe::Space => self.isspace(c),
        }
    }
    fn cprop(c: Chr, bit: u8) -> bool {
        c <= 127 && c_props(c) & bit != 0
    }
    pub fn isdigit(self, c: Chr) -> bool {
        match self {
            Strategy::C => Self::cprop(c, PG_ISDIGIT),
            Strategy::Builtin { posix } => u_isdigit(c, posix),
            Strategy::Libc => probes::libc_isdigit(c),
            Strategy::Icu => probes::icu_isdigit(c),
        }
    }
    pub fn isalpha(self, c: Chr) -> bool {
        match self {
            Strategy::C => Self::cprop(c, PG_ISALPHA),
            Strategy::Builtin { .. } => u_isalpha(c),
            Strategy::Libc => probes::libc_isalpha(c),
            Strategy::Icu => probes::icu_isalpha(c),
        }
    }
    pub fn isalnum(self, c: Chr) -> bool {
        match self {
            Strategy::C => Self::cprop(c, PG_ISALNUM),
            Strategy::Builtin { posix } => u_isalpha(c) || u_isdigit(c, posix),
            Strategy::Libc => probes::libc_isalnum(c),
            Strategy::Icu => probes::icu_isalnum(c),
        }
    }
    /// Word characters: alnum plus underscore.
    pub fn isword(self, c: Chr) -> bool {
        c == '_' as Chr || self.isalnum(c)
    }
    pub fn isupper(self, c: Chr) -> bool {
        match self {
            Strategy::C => Self::cprop(c, PG_ISUPPER),
            Strategy::Builtin { .. } => in_ranges(&UPPERCASE, c),
            Strategy::Libc => probes::libc_isupper(c),
            Strategy::Icu => probes::icu_isupper(c),
        }
    }
    pub fn islower(self, c: Chr) -> bool {
        match self {
            Strategy::C => Self::cprop(c, PG_ISLOWER),
            Strategy::Builtin { .. } => in_ranges(&LOWERCASE, c),
            Strategy::Libc => probes::libc_islower(c),
            Strategy::Icu => probes::icu_islower(c),
        }
    }
    pub fn isgraph(self, c: Chr) -> bool {
        match self {
            Strategy::C => Self::cprop(c, PG_ISGRAPH),
            Strategy::Builtin { .. } => u_isgraph(c),
            Strategy::Libc => probes::libc_isgraph(c),
            Strategy::Icu => probes::icu_isgraph(c),
        }
    }
    pub fn isprint(self, c: Chr) -> bool {
        match self {
            Strategy::C => Self::cprop(c, PG_ISPRINT),
            Strategy::Builtin { .. } => u_isprint(c),
            Strategy::Libc => probes::libc_isprint(c),
            Strategy::Icu => probes::icu_isprint(c),
        }
    }
    pub fn ispunct(self, c: Chr) -> bool {
        match self {
            Strategy::C => Self::cprop(c, PG_ISPUNCT),
            Strategy::Builtin { posix } => u_ispunct(c, posix),
            Strategy::Libc => probes::libc_ispunct(c),
            Strategy::Icu => probes::icu_ispunct(c),
        }
    }
    pub fn isspace(self, c: Chr) -> bool {
        match self {
            Strategy::C => Self::cprop(c, PG_ISSPACE),
            Strategy::Builtin { .. } => u_isspace(c),
            Strategy::Libc => probes::libc_isspace(c),
            Strategy::Icu => probes::icu_isspace(c),
        }
    }
    /// `pg_wc_toupper`: the simple uppercase mapping.
    pub fn toupper(self, c: Chr) -> Chr {
        match self {
            Strategy::C => {
                if c <= 127 {
                    (c as u8).to_ascii_uppercase() as Chr
                } else {
                    c
                }
            }
            Strategy::Builtin { .. } => crate::types::collation::case::upper_simple(c),
            Strategy::Libc => probes::libc_toupper(c),
            Strategy::Icu => probes::icu_toupper(c),
        }
    }
    /// `pg_wc_tolower`: the simple lowercase mapping.
    pub fn tolower(self, c: Chr) -> Chr {
        match self {
            Strategy::C => {
                if c <= 127 {
                    (c as u8).to_ascii_lowercase() as Chr
                } else {
                    c
                }
            }
            Strategy::Builtin { .. } => crate::types::collation::case::lower_simple(c),
            Strategy::Libc => probes::libc_tolower(c),
            Strategy::Icu => probes::icu_tolower(c),
        }
    }
}

/// A set of characters (`struct cvec`): single characters, inclusive ranges, and for a
/// locale-dependent class whose members above MAX_SIMPLE_CHR are found at run time, its class
/// code (else -1).
#[derive(Clone, Debug, Default)]
pub struct Cvec {
    pub chrs: Vec<Chr>,
    pub ranges: Vec<(Chr, Chr)>,
    pub cclasscode: i32,
}

impl Cvec {
    pub fn new() -> Cvec {
        Cvec {
            chrs: Vec::new(),
            ranges: Vec::new(),
            cclasscode: -1,
        }
    }
}

/// `pg_ctype_get_cache`: every character up to MAX_SIMPLE_CHR (127 in C) satisfying the probe,
/// cached per strategy.
pub fn ctype_cvec(strategy: Strategy, probe: Probe, cclasscode: i32) -> &'static Cvec {
    const NPROBES: usize = 10;
    type Cache = [OnceLock<Cvec>; NPROBES];
    static C_CACHE: Cache = [const { OnceLock::new() }; NPROBES];
    static POSIX_CACHE: Cache = [const { OnceLock::new() }; NPROBES];
    static FULL_CACHE: Cache = [const { OnceLock::new() }; NPROBES];
    static LIBC_CACHE: Cache = [const { OnceLock::new() }; NPROBES];
    static ICU_CACHE: Cache = [const { OnceLock::new() }; NPROBES];
    let cache = match strategy {
        Strategy::C => &C_CACHE,
        Strategy::Builtin { posix: true } => &POSIX_CACHE,
        Strategy::Builtin { posix: false } => &FULL_CACHE,
        Strategy::Libc => &LIBC_CACHE,
        Strategy::Icu => &ICU_CACHE,
    };
    cache[probe as usize].get_or_init(|| {
        let mut cv = Cvec::new();
        cv.cclasscode = cclasscode;
        let max_chr = match strategy {
            Strategy::C => {
                cv.cclasscode = -1;
                127
            }
            Strategy::Builtin { .. } | Strategy::Libc | Strategy::Icu => MAX_SIMPLE_CHR,
        };
        let mut nmatches: Chr = 0;
        let mut cur: Chr = 0;
        let store = |cv: &mut Cvec, first: Chr, n: Chr| {
            if n > 1 {
                cv.ranges.push((first, first + n - 1));
            } else {
                cv.chrs.push(first);
            }
        };
        while cur <= max_chr {
            if strategy.probe(probe, cur) {
                nmatches += 1;
            } else if nmatches > 0 {
                store(&mut cv, cur - nmatches, nmatches);
                nmatches = 0;
            }
            cur += 1;
        }
        if nmatches > 0 {
            store(&mut cv, cur - nmatches, nmatches);
        }
        cv
    })
}

/// regc_locale.c's ASCII character-name table, for `[.name.]`.
pub const CNAMES: &[(&str, u8)] = &[
    ("NUL", 0),
    ("SOH", 1),
    ("STX", 2),
    ("ETX", 3),
    ("EOT", 4),
    ("ENQ", 5),
    ("ACK", 6),
    ("BEL", 7),
    ("alert", 7),
    ("BS", 8),
    ("backspace", 8),
    ("HT", 9),
    ("tab", 9),
    ("LF", 10),
    ("newline", 10),
    ("VT", 11),
    ("vertical-tab", 11),
    ("FF", 12),
    ("form-feed", 12),
    ("CR", 13),
    ("carriage-return", 13),
    ("SO", 14),
    ("SI", 15),
    ("DLE", 16),
    ("DC1", 17),
    ("DC2", 18),
    ("DC3", 19),
    ("DC4", 20),
    ("NAK", 21),
    ("SYN", 22),
    ("ETB", 23),
    ("CAN", 24),
    ("EM", 25),
    ("SUB", 26),
    ("ESC", 27),
    ("IS4", 28),
    ("FS", 28),
    ("IS3", 29),
    ("GS", 29),
    ("IS2", 30),
    ("RS", 30),
    ("IS1", 31),
    ("US", 31),
    ("space", b' '),
    ("exclamation-mark", b'!'),
    ("quotation-mark", b'"'),
    ("number-sign", b'#'),
    ("dollar-sign", b'$'),
    ("percent-sign", b'%'),
    ("ampersand", b'&'),
    ("apostrophe", b'\''),
    ("left-parenthesis", b'('),
    ("right-parenthesis", b')'),
    ("asterisk", b'*'),
    ("plus-sign", b'+'),
    ("comma", b','),
    ("hyphen", b'-'),
    ("hyphen-minus", b'-'),
    ("period", b'.'),
    ("full-stop", b'.'),
    ("slash", b'/'),
    ("solidus", b'/'),
    ("zero", b'0'),
    ("one", b'1'),
    ("two", b'2'),
    ("three", b'3'),
    ("four", b'4'),
    ("five", b'5'),
    ("six", b'6'),
    ("seven", b'7'),
    ("eight", b'8'),
    ("nine", b'9'),
    ("colon", b':'),
    ("semicolon", b';'),
    ("less-than-sign", b'<'),
    ("equals-sign", b'='),
    ("greater-than-sign", b'>'),
    ("question-mark", b'?'),
    ("commercial-at", b'@'),
    ("left-square-bracket", b'['),
    ("backslash", b'\\'),
    ("reverse-solidus", b'\\'),
    ("right-square-bracket", b']'),
    ("circumflex", b'^'),
    ("circumflex-accent", b'^'),
    ("underscore", b'_'),
    ("low-line", b'_'),
    ("grave-accent", b'`'),
    ("left-brace", b'{'),
    ("left-curly-bracket", b'{'),
    ("vertical-line", b'|'),
    ("right-brace", b'}'),
    ("right-curly-bracket", b'}'),
    ("tilde", b'~'),
    ("DEL", 127),
];

/// The class names, in `enum char_classes` order.
pub const CLASS_NAMES: [&str; 14] = [
    "alnum", "alpha", "ascii", "blank", "cntrl", "digit", "graph", "lower", "print", "punct",
    "space", "upper", "xdigit", "word",
];

/// Whether a pattern's characters spell `name` (pg_char_and_wchar_strncmp over the length).
pub fn chrs_eq(chrs: &[Chr], name: &str) -> bool {
    chrs.len() == name.len() && name.bytes().zip(chrs).all(|(b, &c)| b as Chr == c)
}
