//! A port of Postgres's `formatting.c`: to_char for timestamps, intervals and numbers,
//! to_timestamp / to_date (`do_to_timestamp`) and to_number (`NUM_processor`).
//!
//! The session's lc_time / lc_numeric are taken to be C, so the TM (localized) forms use the
//! English names and the locale symbols are the C defaults.

use crate::error::{Error, Result, code};
use crate::types::datetime::{
    self as dt, DTK_DATE_M, DtCtx, DtErr, MONTHS, POSTGRES_EPOCH_JDATE, TimeZone, Tm,
    date2j, dt_parse_error, dtk_m,
};
use crate::types::numeric::Numeric;
use crate::{bail, err};

const DCH_MAX_ITEM_SIZ: usize = 12;
const NUM_MAX_ITEM_SIZ: usize = 8;
const MAX_ROMAN_LEN: usize = 15;

const MONTHS_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DAYS_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const ADBC_STRINGS: [&str; 4] = ["ad", "bc", "AD", "BC"];
const ADBC_STRINGS_LONG: [&str; 4] = ["a.d.", "b.c.", "A.D.", "B.C."];
const AMPM_STRINGS: [&str; 4] = ["am", "pm", "AM", "PM"];
const AMPM_STRINGS_LONG: [&str; 4] = ["a.m.", "p.m.", "A.M.", "P.M."];
const RM_MONTHS_UPPER: [&str; 12] = [
    "XII", "XI", "X", "IX", "VIII", "VII", "VI", "V", "IV", "III", "II", "I",
];
const RM_MONTHS_LOWER: [&str; 12] = [
    "xii", "xi", "x", "ix", "viii", "vii", "vi", "v", "iv", "iii", "ii", "i",
];
const RM1: [&str; 9] = ["I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX"];
const RM10: [&str; 9] = ["X", "XX", "XXX", "XL", "L", "LX", "LXX", "LXXX", "XC"];
const RM100: [&str; 9] = ["C", "CC", "CCC", "CD", "D", "DC", "DCC", "DCCC", "CM"];

// ── keywords ──

#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum K {
    A_D,
    A_M,
    AD,
    AM,
    B_C,
    BC,
    CC,
    DAY,
    DDD,
    DD,
    DY,
    Day,
    Dy,
    D,
    FF1,
    FF2,
    FF3,
    FF4,
    FF5,
    FF6,
    FX,
    HH24,
    HH12,
    HH,
    IDDD,
    ID,
    IW,
    IYYY,
    IYY,
    IY,
    I,
    J,
    MI,
    MM,
    MONTH,
    MON,
    MS,
    Month,
    Mon,
    OF,
    P_M,
    PM,
    Q,
    RM,
    SSSS,
    SS,
    TZH,
    TZM,
    TZ,
    US,
    WW,
    W,
    Y_YYY,
    YYYY,
    YYY,
    YY,
    Y,
    a_d,
    a_m,
    ad,
    am,
    b_c,
    bc,
    day,
    dy,
    month,
    mon,
    p_m,
    pm,
    rm,
    tz,
    // number keywords
    NComma,
    NDec,
    N0,
    N9,
    NB,
    NC,
    ND,
    NE,
    NFM,
    NG,
    NL,
    NMI,
    NPL,
    NPR,
    NRN,
    NSG,
    NSP,
    NS,
    NTH,
    NV,
    Nrn,
    Nth,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    None,
    Gregorian,
    IsoWeek,
}

#[derive(Debug)]
struct KeyWord {
    name: &'static str,
    id: K,
    is_digit: bool,
    mode: Mode,
}

const fn kw(name: &'static str, id: K, is_digit: bool, mode: Mode) -> KeyWord {
    KeyWord {
        name,
        id,
        is_digit,
        mode,
    }
}

use Mode::{Gregorian as G, IsoWeek as IW, None as NO};

static DCH_KEYWORDS: &[KeyWord] = &[
    kw("A.D.", K::A_D, false, NO),
    kw("A.M.", K::A_M, false, NO),
    kw("AD", K::AD, false, NO),
    kw("AM", K::AM, false, NO),
    kw("B.C.", K::B_C, false, NO),
    kw("BC", K::BC, false, NO),
    kw("CC", K::CC, true, NO),
    kw("DAY", K::DAY, false, NO),
    kw("DDD", K::DDD, true, G),
    kw("DD", K::DD, true, G),
    kw("DY", K::DY, false, NO),
    kw("Day", K::Day, false, NO),
    kw("Dy", K::Dy, false, NO),
    kw("D", K::D, true, G),
    kw("FF1", K::FF1, true, NO),
    kw("FF2", K::FF2, true, NO),
    kw("FF3", K::FF3, true, NO),
    kw("FF4", K::FF4, true, NO),
    kw("FF5", K::FF5, true, NO),
    kw("FF6", K::FF6, true, NO),
    kw("FX", K::FX, false, NO),
    kw("HH24", K::HH24, true, NO),
    kw("HH12", K::HH12, true, NO),
    kw("HH", K::HH, true, NO),
    kw("IDDD", K::IDDD, true, IW),
    kw("ID", K::ID, true, IW),
    kw("IW", K::IW, true, IW),
    kw("IYYY", K::IYYY, true, IW),
    kw("IYY", K::IYY, true, IW),
    kw("IY", K::IY, true, IW),
    kw("I", K::I, true, IW),
    kw("J", K::J, true, NO),
    kw("MI", K::MI, true, NO),
    kw("MM", K::MM, true, G),
    kw("MONTH", K::MONTH, false, G),
    kw("MON", K::MON, false, G),
    kw("MS", K::MS, true, NO),
    kw("Month", K::Month, false, G),
    kw("Mon", K::Mon, false, G),
    kw("OF", K::OF, false, NO),
    kw("P.M.", K::P_M, false, NO),
    kw("PM", K::PM, false, NO),
    kw("Q", K::Q, true, NO),
    kw("RM", K::RM, false, G),
    kw("SSSSS", K::SSSS, true, NO),
    kw("SSSS", K::SSSS, true, NO),
    kw("SS", K::SS, true, NO),
    kw("TZH", K::TZH, false, NO),
    kw("TZM", K::TZM, true, NO),
    kw("TZ", K::TZ, false, NO),
    kw("US", K::US, true, NO),
    kw("WW", K::WW, true, G),
    kw("W", K::W, true, G),
    kw("Y,YYY", K::Y_YYY, true, G),
    kw("YYYY", K::YYYY, true, G),
    kw("YYY", K::YYY, true, G),
    kw("YY", K::YY, true, G),
    kw("Y", K::Y, true, G),
    kw("a.d.", K::a_d, false, NO),
    kw("a.m.", K::a_m, false, NO),
    kw("ad", K::ad, false, NO),
    kw("am", K::am, false, NO),
    kw("b.c.", K::b_c, false, NO),
    kw("bc", K::bc, false, NO),
    kw("cc", K::CC, true, NO),
    kw("day", K::day, false, NO),
    kw("ddd", K::DDD, true, G),
    kw("dd", K::DD, true, G),
    kw("dy", K::dy, false, NO),
    kw("d", K::D, true, G),
    kw("ff1", K::FF1, true, NO),
    kw("ff2", K::FF2, true, NO),
    kw("ff3", K::FF3, true, NO),
    kw("ff4", K::FF4, true, NO),
    kw("ff5", K::FF5, true, NO),
    kw("ff6", K::FF6, true, NO),
    kw("fx", K::FX, false, NO),
    kw("hh24", K::HH24, true, NO),
    kw("hh12", K::HH12, true, NO),
    kw("hh", K::HH, true, NO),
    kw("iddd", K::IDDD, true, IW),
    kw("id", K::ID, true, IW),
    kw("iw", K::IW, true, IW),
    kw("iyyy", K::IYYY, true, IW),
    kw("iyy", K::IYY, true, IW),
    kw("iy", K::IY, true, IW),
    kw("i", K::I, true, IW),
    kw("j", K::J, true, NO),
    kw("mi", K::MI, true, NO),
    kw("mm", K::MM, true, G),
    kw("month", K::month, false, G),
    kw("mon", K::mon, false, G),
    kw("ms", K::MS, true, NO),
    kw("of", K::OF, false, NO),
    kw("p.m.", K::p_m, false, NO),
    kw("pm", K::pm, false, NO),
    kw("q", K::Q, true, NO),
    kw("rm", K::rm, false, G),
    kw("sssss", K::SSSS, true, NO),
    kw("ssss", K::SSSS, true, NO),
    kw("ss", K::SS, true, NO),
    kw("tzh", K::TZH, false, NO),
    kw("tzm", K::TZM, true, NO),
    kw("tz", K::tz, false, NO),
    kw("us", K::US, true, NO),
    kw("ww", K::WW, true, G),
    kw("w", K::W, true, G),
    kw("y,yyy", K::Y_YYY, true, G),
    kw("yyyy", K::YYYY, true, G),
    kw("yyy", K::YYY, true, G),
    kw("yy", K::YY, true, G),
    kw("y", K::Y, true, G),
];

static NUM_KEYWORDS: &[KeyWord] = &[
    kw(",", K::NComma, false, NO),
    kw(".", K::NDec, false, NO),
    kw("0", K::N0, false, NO),
    kw("9", K::N9, false, NO),
    kw("B", K::NB, false, NO),
    kw("C", K::NC, false, NO),
    kw("D", K::ND, false, NO),
    kw("EEEE", K::NE, false, NO),
    kw("FM", K::NFM, false, NO),
    kw("G", K::NG, false, NO),
    kw("L", K::NL, false, NO),
    kw("MI", K::NMI, false, NO),
    kw("PL", K::NPL, false, NO),
    kw("PR", K::NPR, false, NO),
    kw("RN", K::NRN, false, NO),
    kw("SG", K::NSG, false, NO),
    kw("SP", K::NSP, false, NO),
    kw("S", K::NS, false, NO),
    kw("TH", K::NTH, false, NO),
    kw("V", K::NV, false, NO),
    kw("b", K::NB, false, NO),
    kw("c", K::NC, false, NO),
    kw("d", K::ND, false, NO),
    kw("eeee", K::NE, false, NO),
    kw("fm", K::NFM, false, NO),
    kw("g", K::NG, false, NO),
    kw("l", K::NL, false, NO),
    kw("mi", K::NMI, false, NO),
    kw("pl", K::NPL, false, NO),
    kw("pr", K::NPR, false, NO),
    kw("rn", K::Nrn, false, NO),
    kw("sg", K::NSG, false, NO),
    kw("sp", K::NSP, false, NO),
    kw("s", K::NS, false, NO),
    kw("th", K::Nth, false, NO),
    kw("v", K::NV, false, NO),
];

/// index_seq_search: the keyword at the start of `s`.
fn index_seq_search(s: &[u8], kws: &'static [KeyWord]) -> Option<&'static KeyWord> {
    let c = *s.first()?;
    if c <= b' ' || c >= b'~' {
        return None;
    }
    let start = kws.iter().position(|k| k.name.as_bytes()[0] == c)?;
    let mut i = start;
    loop {
        let k = &kws[i];
        if s.starts_with(k.name.as_bytes()) {
            return Some(k);
        }
        i += 1;
        if i >= kws.len() || kws[i].name.as_bytes()[0] != c {
            return None;
        }
    }
}

const DCH_S_FM: u8 = 0x01;
const DCH_S_TH: u8 = 0x02;
const DCH_S_TH_LOWER: u8 = 0x04;
const DCH_S_SP: u8 = 0x08;
const DCH_S_TM: u8 = 0x10;

fn s_fm(s: u8) -> bool {
    s & DCH_S_FM != 0
}
fn s_thth(s: u8) -> bool {
    s & (DCH_S_TH | DCH_S_TH_LOWER) != 0
}
fn s_tm(s: u8) -> bool {
    s & DCH_S_TM != 0
}
fn th_upper(s: u8) -> bool {
    s & DCH_S_TH != 0
}

#[derive(Clone, Debug)]
enum Node {
    Action { key: &'static KeyWord, suffix: u8 },
    Char(String),
    Separator(String),
    Space(String),
}

impl Node {
    fn character(&self) -> &str {
        match self {
            Node::Char(c) | Node::Separator(c) | Node::Space(c) => c,
            Node::Action { .. } => "",
        }
    }
}

/// pg_mblen for UTF-8 text.
fn mblen(s: &[u8], i: usize) -> usize {
    match s.get(i) {
        None => 0,
        Some(&b) if b < 0x80 => 1,
        Some(&b) if b >= 0xF0 => 4,
        Some(&b) if b >= 0xE0 => 3,
        Some(&b) if b >= 0xC0 => 2,
        _ => 1,
    }
    .min(s.len() - i)
}

fn is_separator_char(c: u8) -> bool {
    c > 0x20 && c < 0x7F && !c.is_ascii_alphanumeric()
}

fn c_isspace(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

// ── NUMDesc ──

const NUM_F_DECIMAL: u32 = 1 << 1;
const NUM_F_LDECIMAL: u32 = 1 << 2;
const NUM_F_ZERO: u32 = 1 << 3;
const NUM_F_BLANK: u32 = 1 << 4;
const NUM_F_FILLMODE: u32 = 1 << 5;
const NUM_F_LSIGN: u32 = 1 << 6;
const NUM_F_BRACKET: u32 = 1 << 7;
const NUM_F_MINUS: u32 = 1 << 8;
const NUM_F_PLUS: u32 = 1 << 9;
const NUM_F_ROMAN: u32 = 1 << 10;
const NUM_F_MULTI: u32 = 1 << 11;
const NUM_F_PLUS_POST: u32 = 1 << 12;
const NUM_F_MINUS_POST: u32 = 1 << 13;
const NUM_F_EEEE: u32 = 1 << 14;

const NUM_LSIGN_PRE: i32 = -1;
const NUM_LSIGN_POST: i32 = 1;
const NUM_LSIGN_NONE: i32 = 0;

#[derive(Clone, Copy, Debug, Default)]
struct NumDesc {
    pre: i32,
    post: i32,
    lsign: i32,
    flag: u32,
    pre_lsign_num: i32,
    multi: i32,
    zero_start: i32,
    zero_end: i32,
    need_locale: bool,
}

impl NumDesc {
    fn is(&self, f: u32) -> bool {
        self.flag & f != 0
    }
}

fn syntax(msg: &str) -> Error {
    Error::new(code::SYNTAX_ERROR, msg.to_string())
}

/// NUMDesc_prepare.
fn num_desc_prepare(num: &mut NumDesc, id: K) -> Result<()> {
    if num.is(NUM_F_EEEE) && id != K::NE {
        return Err(syntax("\"EEEE\" must be the last pattern used"));
    }
    match id {
        K::N9 => {
            if num.is(NUM_F_BRACKET) {
                return Err(syntax("\"9\" must be ahead of \"PR\""));
            }
            if num.is(NUM_F_MULTI) {
                num.multi += 1;
            } else if num.is(NUM_F_DECIMAL) {
                num.post += 1;
            } else {
                num.pre += 1;
            }
        }
        K::N0 => {
            if num.is(NUM_F_BRACKET) {
                return Err(syntax("\"0\" must be ahead of \"PR\""));
            }
            if !num.is(NUM_F_ZERO) && !num.is(NUM_F_DECIMAL) {
                num.flag |= NUM_F_ZERO;
                num.zero_start = num.pre + 1;
            }
            if !num.is(NUM_F_DECIMAL) {
                num.pre += 1;
            } else {
                num.post += 1;
            }
            num.zero_end = num.pre + num.post;
        }
        K::NB => {
            if num.pre == 0 && num.post == 0 && !num.is(NUM_F_ZERO) {
                num.flag |= NUM_F_BLANK;
            }
        }
        K::ND | K::NDec => {
            if id == K::ND {
                num.flag |= NUM_F_LDECIMAL;
                num.need_locale = true;
            }
            if num.is(NUM_F_DECIMAL) {
                return Err(syntax("multiple decimal points"));
            }
            if num.is(NUM_F_MULTI) {
                return Err(syntax("cannot use \"V\" and decimal point together"));
            }
            num.flag |= NUM_F_DECIMAL;
        }
        K::NFM => num.flag |= NUM_F_FILLMODE,
        K::NS => {
            if num.is(NUM_F_LSIGN) {
                return Err(syntax("cannot use \"S\" twice"));
            }
            if num.is(NUM_F_PLUS) || num.is(NUM_F_MINUS) || num.is(NUM_F_BRACKET) {
                return Err(syntax(
                    "cannot use \"S\" and \"PL\"/\"MI\"/\"SG\"/\"PR\" together",
                ));
            }
            if !num.is(NUM_F_DECIMAL) {
                num.lsign = NUM_LSIGN_PRE;
                num.pre_lsign_num = num.pre;
                num.need_locale = true;
                num.flag |= NUM_F_LSIGN;
            } else if num.lsign == NUM_LSIGN_NONE {
                num.lsign = NUM_LSIGN_POST;
                num.need_locale = true;
                num.flag |= NUM_F_LSIGN;
            }
        }
        K::NMI => {
            if num.is(NUM_F_LSIGN) {
                return Err(syntax("cannot use \"S\" and \"MI\" together"));
            }
            num.flag |= NUM_F_MINUS;
            if num.is(NUM_F_DECIMAL) {
                num.flag |= NUM_F_MINUS_POST;
            }
        }
        K::NPL => {
            if num.is(NUM_F_LSIGN) {
                return Err(syntax("cannot use \"S\" and \"PL\" together"));
            }
            num.flag |= NUM_F_PLUS;
            if num.is(NUM_F_DECIMAL) {
                num.flag |= NUM_F_PLUS_POST;
            }
        }
        K::NSG => {
            if num.is(NUM_F_LSIGN) {
                return Err(syntax("cannot use \"S\" and \"SG\" together"));
            }
            num.flag |= NUM_F_MINUS | NUM_F_PLUS;
        }
        K::NPR => {
            if num.is(NUM_F_LSIGN) || num.is(NUM_F_PLUS) || num.is(NUM_F_MINUS) {
                return Err(syntax(
                    "cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together",
                ));
            }
            num.flag |= NUM_F_BRACKET;
        }
        K::Nrn | K::NRN => {
            if num.is(NUM_F_ROMAN) {
                return Err(syntax("cannot use \"RN\" twice"));
            }
            num.flag |= NUM_F_ROMAN;
        }
        K::NL | K::NG => num.need_locale = true,
        K::NV => {
            if num.is(NUM_F_DECIMAL) {
                return Err(syntax("cannot use \"V\" and decimal point together"));
            }
            num.flag |= NUM_F_MULTI;
        }
        K::NE => {
            if num.is(NUM_F_EEEE) {
                return Err(syntax("cannot use \"EEEE\" twice"));
            }
            if num.is(NUM_F_BLANK)
                || num.is(NUM_F_FILLMODE)
                || num.is(NUM_F_LSIGN)
                || num.is(NUM_F_BRACKET)
                || num.is(NUM_F_MINUS)
                || num.is(NUM_F_PLUS)
                || num.is(NUM_F_ROMAN)
                || num.is(NUM_F_MULTI)
            {
                return Err(syntax("\"EEEE\" is incompatible with other formats").detail(
                    "\"EEEE\" may only be used together with digit and decimal point patterns.",
                ));
            }
            num.flag |= NUM_F_EEEE;
        }
        _ => {}
    }
    if num.is(NUM_F_ROMAN) && (num.flag & !(NUM_F_ROMAN | NUM_F_FILLMODE)) != 0 {
        return Err(syntax("\"RN\" is incompatible with other formats")
            .detail("\"RN\" may only be used together with \"FM\"."));
    }
    Ok(())
}

/// parse_format for the DCH (`num` = None) or NUM keyword set.
fn parse_format(fmt: &str, std: bool, mut num: Option<&mut NumDesc>) -> Result<Vec<Node>> {
    let s = fmt.as_bytes();
    let dch = num.is_none();
    let kws = if dch { DCH_KEYWORDS } else { NUM_KEYWORDS };
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let mut suffix = 0u8;
        if dch {
            for (name, id) in [
                ("FM", DCH_S_FM),
                ("fm", DCH_S_FM),
                ("TM", DCH_S_TM),
                ("tm", DCH_S_TM),
            ] {
                if s[i..].starts_with(name.as_bytes()) {
                    suffix |= id;
                    i += 2;
                    break;
                }
            }
        }
        if i < s.len()
            && let Some(key) = index_seq_search(&s[i..], kws)
        {
            i += key.name.len();
            if let Some(n) = num.as_deref_mut() {
                num_desc_prepare(n, key.id)?;
            }
            if dch && i < s.len() {
                for (name, id) in [
                    ("TH", DCH_S_TH),
                    ("th", DCH_S_TH_LOWER),
                    ("SP", DCH_S_SP),
                ] {
                    if s[i..].starts_with(name.as_bytes()) {
                        suffix |= id;
                        i += 2;
                        break;
                    }
                }
            }
            out.push(Node::Action { key, suffix });
        } else if i < s.len() {
            if std && s[i] != b'"' {
                if !b"-./,':; ".contains(&s[i]) {
                    let l = mblen(s, i);
                    bail!(
                        INVALID_DATETIME_FORMAT,
                        "invalid datetime format separator: \"{}\"",
                        String::from_utf8_lossy(&s[i..i + l])
                    );
                }
                let ch = (s[i] as char).to_string();
                out.push(if s[i] == b' ' {
                    Node::Space(ch)
                } else {
                    Node::Separator(ch)
                });
                i += 1;
            } else if s[i] == b'"' {
                i += 1;
                while i < s.len() {
                    if s[i] == b'"' {
                        i += 1;
                        break;
                    }
                    if s[i] == b'\\' && i + 1 < s.len() {
                        i += 1;
                    }
                    let l = mblen(s, i);
                    out.push(Node::Char(String::from_utf8_lossy(&s[i..i + l]).into_owned()));
                    i += l;
                }
            } else {
                if s[i] == b'\\' && s.get(i + 1) == Some(&b'"') {
                    i += 1;
                }
                let l = mblen(s, i);
                let ch = String::from_utf8_lossy(&s[i..i + l]).into_owned();
                out.push(if dch && is_separator_char(s[i]) {
                    Node::Separator(ch)
                } else if c_isspace(s[i]) {
                    Node::Space(ch)
                } else {
                    Node::Char(ch)
                });
                i += l;
            }
        }
    }
    Ok(out)
}

/// get_th: ordinal suffix for a digit string.
fn get_th(num: &str, upper: bool) -> Result<&'static str> {
    let b = num.as_bytes();
    let Some(&last) = b.last() else {
        return Err(err!(INVALID_TEXT_REPRESENTATION, "\"\" is not a number"));
    };
    if !last.is_ascii_digit() {
        bail!(INVALID_TEXT_REPRESENTATION, "\"{num}\" is not a number");
    }
    let last = if b.len() > 1 && b[b.len() - 2] == b'1' {
        0
    } else {
        last
    };
    Ok(match (last, upper) {
        (b'1', true) => "ST",
        (b'1', false) => "st",
        (b'2', true) => "ND",
        (b'2', false) => "nd",
        (b'3', true) => "RD",
        (b'3', false) => "rd",
        (_, true) => "TH",
        (_, false) => "th",
    })
}

// ── to_char (date/time) ──

/// struct fmt_tm (+ fsec and tzn).
#[derive(Clone, Debug, Default)]
pub struct TmToChar {
    pub sec: i32,
    pub min: i32,
    pub hour: i64,
    pub mday: i32,
    pub mon: i32,
    pub year: i32,
    pub wday: i32,
    pub yday: i32,
    pub gmtoff: i64,
    pub fsec: i32,
    pub tzn: Option<String>,
}

fn adjust_year(year: i32, is_interval: bool) -> i32 {
    if is_interval {
        year
    } else if year <= 0 {
        -(year - 1)
    } else {
        year
    }
}

fn invalid_for_interval(is_interval: bool) -> Result<()> {
    if is_interval {
        return Err(err!(
            INVALID_DATETIME_FORMAT,
            "invalid format specification for an interval value"
        )
        .hint("Intervals are not tied to specific calendar dates."));
    }
    Ok(())
}

/// `%0*d`.
fn pad(v: i64, width: usize) -> String {
    format!("{v:0width$}")
}

/// DCH_to_char.
fn dch_to_char(nodes: &[Node], is_interval: bool, tm: &TmToChar) -> Result<String> {
    let mut out = String::new();
    for n in nodes {
        let Node::Action { key, suffix } = n else {
            out.push_str(n.character());
            continue;
        };
        let suffix = *suffix;
        let fm = s_fm(suffix);
        let mut s = String::new();
        let mut numeric = true;
        let w2 = |v: i64| if fm { 0 } else if v >= 0 { 2 } else { 3 };
        match key.id {
            K::A_M | K::P_M => {
                s.push_str(if tm.hour % 24 >= 12 { "P.M." } else { "A.M." });
                numeric = false;
            }
            K::AM | K::PM => {
                s.push_str(if tm.hour % 24 >= 12 { "PM" } else { "AM" });
                numeric = false;
            }
            K::a_m | K::p_m => {
                s.push_str(if tm.hour % 24 >= 12 { "p.m." } else { "a.m." });
                numeric = false;
            }
            K::am | K::pm => {
                s.push_str(if tm.hour % 24 >= 12 { "pm" } else { "am" });
                numeric = false;
            }
            K::HH | K::HH12 => {
                let v = if tm.hour % 12 == 0 { 12 } else { tm.hour % 12 };
                s = pad(v, w2(tm.hour));
            }
            K::HH24 => s = pad(tm.hour, w2(tm.hour)),
            K::MI => s = pad(tm.min as i64, w2(tm.min as i64)),
            K::SS => s = pad(tm.sec as i64, w2(tm.sec as i64)),
            K::FF1 => s = format!("{:01}", tm.fsec / 100_000),
            K::FF2 => s = format!("{:02}", tm.fsec / 10_000),
            K::FF3 | K::MS => s = format!("{:03}", tm.fsec / 1000),
            K::FF4 => s = format!("{:04}", tm.fsec / 100),
            K::FF5 => s = format!("{:05}", tm.fsec / 10),
            K::FF6 | K::US => s = format!("{:06}", tm.fsec),
            K::SSSS => s = (tm.hour * 3600 + tm.min as i64 * 60 + tm.sec as i64).to_string(),
            K::tz | K::TZ => {
                invalid_for_interval(is_interval)?;
                numeric = false;
                if let Some(z) = &tm.tzn {
                    if z.len() > key.name.len() * DCH_MAX_ITEM_SIZ {
                        bail!(DATETIME_FIELD_OVERFLOW, "time zone format value too long");
                    }
                    if key.id == K::tz {
                        s.push_str(&z.to_ascii_lowercase());
                    } else {
                        s.push_str(z);
                    }
                }
            }
            K::TZH => {
                invalid_for_interval(is_interval)?;
                numeric = false;
                s = format!(
                    "{}{:02}",
                    if tm.gmtoff >= 0 { '+' } else { '-' },
                    (tm.gmtoff as i32).abs() / 3600
                );
            }
            K::TZM => {
                invalid_for_interval(is_interval)?;
                numeric = false;
                s = format!("{:02}", ((tm.gmtoff as i32).abs() % 3600) / 60);
            }
            K::OF => {
                invalid_for_interval(is_interval)?;
                numeric = false;
                let a = (tm.gmtoff as i32).abs();
                s = format!(
                    "{}{:0w$}",
                    if tm.gmtoff >= 0 { '+' } else { '-' },
                    a / 3600,
                    w = if fm { 0 } else { 2 }
                );
                if a % 3600 != 0 {
                    s.push_str(&format!(":{:02}", (a % 3600) / 60));
                }
            }
            K::A_D | K::B_C => {
                invalid_for_interval(is_interval)?;
                numeric = false;
                s.push_str(if tm.year <= 0 { "B.C." } else { "A.D." });
            }
            K::AD | K::BC => {
                invalid_for_interval(is_interval)?;
                numeric = false;
                s.push_str(if tm.year <= 0 { "BC" } else { "AD" });
            }
            K::a_d | K::b_c => {
                invalid_for_interval(is_interval)?;
                numeric = false;
                s.push_str(if tm.year <= 0 { "b.c." } else { "a.d." });
            }
            K::ad | K::bc => {
                invalid_for_interval(is_interval)?;
                numeric = false;
                s.push_str(if tm.year <= 0 { "bc" } else { "ad" });
            }
            K::MONTH | K::Month | K::month => {
                invalid_for_interval(is_interval)?;
                numeric = false;
                if tm.mon == 0 {
                    continue;
                }
                let name = MONTHS_FULL[(tm.mon - 1) as usize];
                let name = match key.id {
                    K::MONTH => name.to_ascii_uppercase(),
                    K::month => name.to_ascii_lowercase(),
                    _ => name.to_string(),
                };
                s = if s_tm(suffix) || fm {
                    name
                } else {
                    format!("{name:<9}")
                };
            }
            K::MON | K::Mon | K::mon => {
                invalid_for_interval(is_interval)?;
                numeric = false;
                if tm.mon == 0 {
                    continue;
                }
                let name = MONTHS[(tm.mon - 1) as usize];
                s = match key.id {
                    K::MON => name.to_ascii_uppercase(),
                    K::mon => name.to_ascii_lowercase(),
                    _ => name.to_string(),
                };
            }
            K::MM => s = pad(tm.mon as i64, w2(tm.mon as i64)),
            K::DAY | K::Day | K::day => {
                invalid_for_interval(is_interval)?;
                numeric = false;
                let name = dt::DAYS[tm.wday.rem_euclid(7) as usize];
                let name = match key.id {
                    K::DAY => name.to_ascii_uppercase(),
                    K::day => name.to_ascii_lowercase(),
                    _ => name.to_string(),
                };
                s = if s_tm(suffix) || fm {
                    name
                } else {
                    format!("{name:<9}")
                };
            }
            K::DY | K::Dy | K::dy => {
                invalid_for_interval(is_interval)?;
                numeric = false;
                let name = DAYS_SHORT[tm.wday.rem_euclid(7) as usize];
                s = match key.id {
                    K::DY => name.to_ascii_uppercase(),
                    K::dy => name.to_ascii_lowercase(),
                    _ => name.to_string(),
                };
            }
            K::DDD | K::IDDD => {
                let v = if key.id == K::DDD {
                    tm.yday
                } else {
                    dt::date2isoyearday(tm.year, tm.mon, tm.mday)
                };
                s = pad(v as i64, if fm { 0 } else { 3 });
            }
            K::DD => s = pad(tm.mday as i64, if fm { 0 } else { 2 }),
            K::D => {
                invalid_for_interval(is_interval)?;
                s = (tm.wday + 1).to_string();
            }
            K::ID => {
                invalid_for_interval(is_interval)?;
                s = (if tm.wday == 0 { 7 } else { tm.wday }).to_string();
            }
            K::WW => s = pad(((tm.yday - 1) / 7 + 1) as i64, if fm { 0 } else { 2 }),
            K::IW => s = pad(
                dt::date2isoweek(tm.year, tm.mon, tm.mday) as i64,
                if fm { 0 } else { 2 },
            ),
            K::Q => {
                if tm.mon == 0 {
                    continue;
                }
                s = ((tm.mon - 1) / 3 + 1).to_string();
            }
            K::CC => {
                let i = if is_interval {
                    tm.year / 100
                } else if tm.year > 0 {
                    (tm.year - 1) / 100 + 1
                } else {
                    tm.year / 100 - 1
                };
                s = if (-99..=99).contains(&i) {
                    pad(i as i64, if fm { 0 } else if i >= 0 { 2 } else { 3 })
                } else {
                    i.to_string()
                };
            }
            K::Y_YYY => {
                let y = adjust_year(tm.year, is_interval);
                let i = y / 1000;
                s = format!("{},{:03}", i, y - i * 1000);
            }
            K::YYYY | K::IYYY | K::YYY | K::IYY | K::YY | K::IY | K::Y | K::I => {
                let iso = matches!(key.id, K::IYYY | K::IYY | K::IY | K::I);
                let y = if iso {
                    adjust_year(dt::date2isoyear(tm.year, tm.mon, tm.mday), is_interval)
                } else {
                    adjust_year(tm.year, is_interval)
                };
                let adj = adjust_year(tm.year, is_interval);
                let (v, w) = match key.id {
                    K::YYYY | K::IYYY => (y, if adj >= 0 { 4 } else { 5 }),
                    K::YYY | K::IYY => (y % 1000, if adj >= 0 { 3 } else { 4 }),
                    K::YY | K::IY => (y % 100, if adj >= 0 { 2 } else { 3 }),
                    _ => (y % 10, 1),
                };
                s = if matches!(key.id, K::Y | K::I) {
                    format!("{v:1}")
                } else {
                    pad(v as i64, if fm { 0 } else { w })
                };
            }
            K::RM | K::rm => {
                numeric = false;
                if tm.mon == 0 && tm.year == 0 {
                    continue;
                }
                let months = if key.id == K::RM {
                    &RM_MONTHS_UPPER
                } else {
                    &RM_MONTHS_LOWER
                };
                let mon = if tm.mon == 0 {
                    if tm.year >= 0 { 0 } else { 11 }
                } else if tm.mon < 0 {
                    -(tm.mon + 1)
                } else {
                    12 - tm.mon
                };
                let m = months[mon.clamp(0, 11) as usize];
                s = if fm { m.to_string() } else { format!("{m:<4}") };
            }
            K::W => s = ((tm.mday - 1) / 7 + 1).to_string(),
            K::J => s = date2j(tm.year, tm.mon, tm.mday).to_string(),
            K::FX => continue,
            _ => continue,
        }
        if numeric && s_thth(suffix) {
            let th = get_th(&s, th_upper(suffix))?;
            s.push_str(th);
        }
        out.push_str(&s);
    }
    Ok(out)
}

fn dch_nodes(fmt: &str) -> Result<Vec<Node>> {
    parse_format(fmt, false, None)
}

/// timestamp_to_char / timestamptz_to_char (`tz` for timestamptz). None = NULL.
pub fn timestamp_to_char(ts: i64, fmt: &str, tz: Option<&TimeZone>) -> Result<Option<String>> {
    if fmt.is_empty() || !dt::ts_is_finite(ts) {
        return Ok(None);
    }
    let Some((tt, fsec)) = dt::timestamp2tm(ts, tz) else {
        bail!(DATETIME_FIELD_OVERFLOW, "timestamp out of range");
    };
    let thisdate = date2j(tt.year, tt.mon, tt.mday);
    let tm = TmToChar {
        sec: tt.sec,
        min: tt.min,
        hour: tt.hour as i64,
        mday: tt.mday,
        mon: tt.mon,
        year: tt.year,
        wday: (thisdate + 1) % 7,
        yday: thisdate - date2j(tt.year, 1, 1) + 1,
        gmtoff: tt.gmtoff,
        fsec,
        tzn: if tz.is_some() { tt.zone.clone() } else { None },
    };
    Ok(Some(dch_to_char(&dch_nodes(fmt)?, false, &tm)?))
}

/// interval_to_char. None = NULL.
pub fn interval_to_char(iv: &crate::types::value::Interval, fmt: &str) -> Result<Option<String>> {
    if fmt.is_empty() || iv.is_infinite() {
        return Ok(None);
    }
    let itm = dt::interval2itm(iv);
    let tm = TmToChar {
        sec: itm.sec,
        min: itm.min,
        hour: itm.hour,
        mday: itm.mday,
        mon: itm.mon,
        year: itm.year,
        wday: 0,
        // (int arithmetic, which Postgres compiles with -fwrapv: 178000000 years wrap.)
        yday: itm.year.wrapping_mul(12).wrapping_add(itm.mon).wrapping_mul(30).wrapping_add(itm.mday),
        gmtoff: 0,
        fsec: itm.usec,
        tzn: None,
    };
    Ok(Some(dch_to_char(&dch_nodes(fmt)?, true, &tm)?))
}

// ── from_char (to_timestamp / to_date) ──

#[derive(Default)]
struct TmFromChar {
    mode: Option<Mode>,
    hh: i32,
    pm: i32,
    mi: i32,
    ss: i32,
    ssss: i32,
    d: i32,
    dd: i32,
    ddd: i32,
    mm: i32,
    ms: i32,
    year: i32,
    bc: i32,
    ww: i32,
    w: i32,
    cc: i32,
    j: i32,
    us: i32,
    yysz: i32,
    clock12: bool,
    tzsign: i32,
    tzh: i32,
    tzm: i32,
    ff: i32,
    has_tz: bool,
    gmtoffset: i32,
    tzp: Option<TimeZone>,
    abbrev: Option<String>,
}

fn from_char_set_mode(t: &mut TmFromChar, mode: Mode) -> Result<()> {
    if mode != Mode::None {
        match t.mode {
            None => t.mode = Some(mode),
            Some(m) if m != mode => {
                return Err(err!(
                    INVALID_DATETIME_FORMAT,
                    "invalid combination of date conventions"
                )
                .hint(
                    "Do not mix Gregorian and ISO week date conventions in a formatting template.",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn from_char_set_int(dest: &mut i32, value: i32, key: &KeyWord) -> Result<()> {
    if *dest != 0 && *dest != value {
        return Err(err!(
            INVALID_DATETIME_FORMAT,
            "conflicting values for \"{}\" field in formatting string",
            key.name
        )
        .detail("This value contradicts a previous setting for the same field type."));
    }
    *dest = value;
    Ok(())
}

/// is_next_separator.
fn is_next_separator(nodes: &[Node], idx: usize) -> bool {
    let Some(n) = nodes.get(idx) else {
        return false;
    };
    if let Node::Action { suffix, .. } = n
        && s_thth(*suffix)
    {
        return true;
    }
    match nodes.get(idx + 1) {
        None => true,
        Some(Node::Action { key, .. }) => !key.is_digit,
        Some(n) => {
            let c = n.character().as_bytes();
            !(c.len() == 1 && c[0].is_ascii_digit())
        }
    }
}

/// strtol from `i`: (value, end, overflow).
fn c_strtol(s: &[u8], i: usize) -> (i64, usize, bool) {
    let mut p = i;
    while p < s.len() && c_isspace(s[p]) {
        p += 1;
    }
    let mut neg = false;
    if p < s.len() && (s[p] == b'+' || s[p] == b'-') {
        neg = s[p] == b'-';
        p += 1;
    }
    let st = p;
    let mut v: i128 = 0;
    let mut over = false;
    while p < s.len() && s[p].is_ascii_digit() {
        if !over {
            v = v * 10 + (s[p] - b'0') as i128;
            if v > 1 << 64 {
                over = true;
            }
        }
        p += 1;
    }
    if p == st {
        return (0, i, false);
    }
    let v = if neg { -v } else { v };
    if over || v > i64::MAX as i128 || v < i64::MIN as i128 {
        return (if neg { i64::MIN } else { i64::MAX }, p, true);
    }
    (v as i64, p, false)
}

/// from_char_parse_int_len: returns the number of bytes consumed (leading spaces included).
fn from_char_parse_int_len(
    dest: Option<&mut i32>,
    s: &[u8],
    src: &mut usize,
    len: usize,
    nodes: &[Node],
    idx: usize,
) -> Result<usize> {
    let Node::Action { key, suffix } = &nodes[idx] else {
        return Ok(0);
    };
    let init = *src;
    while *src < s.len() && c_isspace(s[*src]) {
        *src += 1;
    }
    let used_avail = s.len() - *src;
    let copy_end = (*src + len).min(s.len());
    let copy = &s[*src..copy_end];
    let copy_str = String::from_utf8_lossy(copy).into_owned();
    let (result, over);
    if s_fm(*suffix) || is_next_separator(nodes, idx) {
        let (v, end, o) = c_strtol(s, init);
        result = v;
        over = o;
        *src = end;
    } else {
        if used_avail < len {
            return Err(err!(
                INVALID_DATETIME_FORMAT,
                "source string too short for \"{}\" formatting field",
                key.name
            )
            .detail(format!(
                "Field requires {len} characters, but only {used_avail} remain."
            ))
            .hint("If your source string is not fixed-width, try using the \"FM\" modifier."));
        }
        let (v, end, o) = c_strtol(copy, 0);
        let used = end;
        if used > 0 && used < len {
            return Err(err!(
                INVALID_DATETIME_FORMAT,
                "invalid value \"{copy_str}\" for \"{}\"",
                key.name
            )
            .detail(format!(
                "Field requires {len} characters, but only {used} could be parsed."
            ))
            .hint("If your source string is not fixed-width, try using the \"FM\" modifier."));
        }
        result = v;
        over = o;
        *src += used;
    }
    if *src == init {
        return Err(err!(
            INVALID_DATETIME_FORMAT,
            "invalid value \"{copy_str}\" for \"{}\"",
            key.name
        )
        .detail("Value must be an integer."));
    }
    if over || result < i32::MIN as i64 || result > i32::MAX as i64 {
        return Err(err!(
            DATETIME_FIELD_OVERFLOW,
            "value for \"{}\" in source string is out of range",
            key.name
        )
        .detail("Value must be in the range -2147483648 to 2147483647."));
    }
    if let Some(d) = dest {
        from_char_set_int(d, result as i32, key)?;
    }
    Ok(*src - init)
}

/// seq_search_ascii: (index, match length).
fn seq_search_ascii(name: &[u8], array: &[&str]) -> Option<(usize, usize)> {
    let first = name.first()?.to_ascii_lowercase();
    for (i, a) in array.iter().enumerate() {
        let a = a.as_bytes();
        if a[0].to_ascii_lowercase() != first {
            continue;
        }
        let mut k = 1;
        loop {
            if k == a.len() {
                return Some((i, k));
            }
            if k >= name.len() {
                break;
            }
            if !a[k].eq_ignore_ascii_case(&name[k]) {
                break;
            }
            k += 1;
        }
    }
    None
}

fn from_char_seq_search(s: &[u8], src: &mut usize, array: &[&str], key: &KeyWord) -> Result<usize> {
    match seq_search_ascii(&s[*src..], array) {
        Some((i, len)) => {
            *src += len;
            Ok(i)
        }
        None => {
            let rest = &s[*src..];
            let end = rest
                .iter()
                .position(|&c| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0c))
                .unwrap_or(rest.len());
            Err(err!(
                INVALID_DATETIME_FORMAT,
                "invalid value \"{}\" for \"{}\"",
                String::from_utf8_lossy(&rest[..end]),
                key.name
            )
            .detail("The given value did not match any of the allowed values for this field."))
        }
    }
}

fn adjust_partial_year_to_2020(year: i32) -> i32 {
    if year < 70 {
        year + 2000
    } else if year < 100 {
        year + 1900
    } else if year < 520 {
        year + 2000
    } else if year < 1000 {
        year + 1000
    } else {
        year
    }
}

fn skip_thth(s: &[u8], src: &mut usize, suffix: u8) {
    if s_thth(suffix) {
        for _ in 0..2 {
            if *src < s.len() {
                *src += mblen(s, *src);
            }
        }
    }
}

/// DCH_from_char.
fn dch_from_char(nodes: &[Node], input: &str, out: &mut TmFromChar, std: bool, cx: &DtCtx) -> Result<()> {
    let s = input.as_bytes();
    let mut fx_mode = std;
    let mut extra_skip: i32 = 0;
    let mut p = 0usize;
    let mut idx = 0usize;
    while idx < nodes.len() && p < s.len() {
        let n = &nodes[idx];
        let is_fx = matches!(n, Node::Action { key, .. } if key.id == K::FX);
        if !fx_mode && !is_fx && (matches!(n, Node::Action { .. }) || idx == 0) {
            while p < s.len() && c_isspace(s[p]) {
                p += 1;
                extra_skip += 1;
            }
        }
        match n {
            Node::Space(c) | Node::Separator(c) => {
                if std {
                    if p < s.len() && s[p] == c.as_bytes()[0] {
                        p += 1;
                    } else {
                        bail!(
                            INVALID_DATETIME_FORMAT,
                            "unmatched format separator \"{}\"",
                            c
                        );
                    }
                } else if !fx_mode {
                    extra_skip -= 1;
                    if p < s.len() && (c_isspace(s[p]) || is_separator_char(s[p])) {
                        p += 1;
                        extra_skip += 1;
                    }
                } else {
                    p += mblen(s, p);
                }
                idx += 1;
                continue;
            }
            Node::Char(c) => {
                if !fx_mode {
                    if extra_skip > 0 {
                        extra_skip -= 1;
                    } else {
                        p += mblen(s, p);
                    }
                } else {
                    let chlen = mblen(s, p);
                    if std && &s[p..p + chlen] != c.as_bytes() {
                        bail!(
                            INVALID_DATETIME_FORMAT,
                            "unmatched format character \"{}\"",
                            c
                        );
                    }
                    p += chlen;
                }
                idx += 1;
                continue;
            }
            Node::Action { .. } => {}
        }
        let Node::Action { key, suffix } = n else {
            unreachable!()
        };
        let suffix = *suffix;
        from_char_set_mode(out, key.mode)?;
        match key.id {
            K::FX => fx_mode = true,
            K::A_M | K::P_M | K::a_m | K::p_m => {
                let v = from_char_seq_search(s, &mut p, &AMPM_STRINGS_LONG, key)?;
                from_char_set_int(&mut out.pm, (v % 2) as i32, key)?;
                out.clock12 = true;
            }
            K::AM | K::PM | K::am | K::pm => {
                let v = from_char_seq_search(s, &mut p, &AMPM_STRINGS, key)?;
                from_char_set_int(&mut out.pm, (v % 2) as i32, key)?;
                out.clock12 = true;
            }
            K::HH | K::HH12 => {
                from_char_parse_int_len(Some(&mut out.hh), s, &mut p, 2, nodes, idx)?;
                out.clock12 = true;
                skip_thth(s, &mut p, suffix);
            }
            K::HH24 => {
                from_char_parse_int_len(Some(&mut out.hh), s, &mut p, 2, nodes, idx)?;
                skip_thth(s, &mut p, suffix);
            }
            K::MI => {
                from_char_parse_int_len(Some(&mut out.mi), s, &mut p, key.name.len(), nodes, idx)?;
                skip_thth(s, &mut p, suffix);
            }
            K::SS => {
                from_char_parse_int_len(Some(&mut out.ss), s, &mut p, key.name.len(), nodes, idx)?;
                skip_thth(s, &mut p, suffix);
            }
            K::MS => {
                let len = from_char_parse_int_len(Some(&mut out.ms), s, &mut p, 3, nodes, idx)?;
                out.ms *= match len {
                    1 => 100,
                    2 => 10,
                    _ => 1,
                };
                skip_thth(s, &mut p, suffix);
            }
            K::FF1 | K::FF2 | K::FF3 | K::FF4 | K::FF5 | K::FF6 | K::US => {
                let len = if key.id == K::US {
                    6
                } else {
                    out.ff = match key.id {
                        K::FF1 => 1,
                        K::FF2 => 2,
                        K::FF3 => 3,
                        K::FF4 => 4,
                        K::FF5 => 5,
                        _ => 6,
                    };
                    out.ff as usize
                };
                let used = from_char_parse_int_len(Some(&mut out.us), s, &mut p, len, nodes, idx)?;
                out.us *= match used {
                    1 => 100_000,
                    2 => 10_000,
                    3 => 1000,
                    4 => 100,
                    5 => 10,
                    _ => 1,
                };
                skip_thth(s, &mut p, suffix);
            }
            K::SSSS => {
                from_char_parse_int_len(Some(&mut out.ssss), s, &mut p, key.name.len(), nodes, idx)?;
                skip_thth(s, &mut p, suffix);
            }
            K::tz | K::TZ | K::OF => {
                let mut as_of = key.id == K::OF;
                if !as_of {
                    let rest = std::str::from_utf8(&s[p..]).unwrap_or("");
                    match dt::decode_timezone_abbrev_prefix(rest, cx) {
                        Some((tzlen, off, z)) => {
                            out.has_tz = true;
                            out.gmtoffset = off;
                            if z.is_some() {
                                out.abbrev = Some(rest[..tzlen].to_string());
                            }
                            out.tzp = z;
                            out.tzsign = 0;
                            p += tzlen;
                        }
                        None => {
                            if p < s.len() && s[p].is_ascii_alphabetic() {
                                return Err(err!(
                                    INVALID_DATETIME_FORMAT,
                                    "invalid value \"{}\" for \"{}\"",
                                    rest,
                                    key.name
                                )
                                .detail("Time zone abbreviation is not recognized."));
                            }
                            as_of = true;
                        }
                    }
                }
                if as_of {
                    if p < s.len() && (s[p] == b'+' || s[p] == b'-' || s[p] == b' ') {
                        out.tzsign = if s[p] == b'-' { -1 } else { 1 };
                        p += 1;
                    } else if extra_skip > 0 && p > 0 && s[p - 1] == b'-' {
                        out.tzsign = -1;
                    } else {
                        out.tzsign = 1;
                    }
                    from_char_parse_int_len(Some(&mut out.tzh), s, &mut p, 2, nodes, idx)?;
                    if p < s.len() && s[p] == b':' {
                        p += 1;
                        from_char_parse_int_len(Some(&mut out.tzm), s, &mut p, 2, nodes, idx)?;
                    }
                }
            }
            K::TZH => {
                if p < s.len() && (s[p] == b'+' || s[p] == b'-' || s[p] == b' ') {
                    out.tzsign = if s[p] == b'-' { -1 } else { 1 };
                    p += 1;
                } else if extra_skip > 0 && p > 0 && s[p - 1] == b'-' {
                    out.tzsign = -1;
                } else {
                    out.tzsign = 1;
                }
                from_char_parse_int_len(Some(&mut out.tzh), s, &mut p, 2, nodes, idx)?;
            }
            K::TZM => {
                if out.tzsign == 0 {
                    out.tzsign = 1;
                }
                from_char_parse_int_len(Some(&mut out.tzm), s, &mut p, 2, nodes, idx)?;
            }
            K::A_D | K::B_C | K::a_d | K::b_c => {
                let v = from_char_seq_search(s, &mut p, &ADBC_STRINGS_LONG, key)?;
                from_char_set_int(&mut out.bc, (v % 2) as i32, key)?;
            }
            K::AD | K::BC | K::ad | K::bc => {
                let v = from_char_seq_search(s, &mut p, &ADBC_STRINGS, key)?;
                from_char_set_int(&mut out.bc, (v % 2) as i32, key)?;
            }
            K::MONTH | K::Month | K::month => {
                let v = from_char_seq_search(s, &mut p, &MONTHS_FULL, key)?;
                from_char_set_int(&mut out.mm, v as i32 + 1, key)?;
            }
            K::MON | K::Mon | K::mon => {
                let v = from_char_seq_search(s, &mut p, &MONTHS, key)?;
                from_char_set_int(&mut out.mm, v as i32 + 1, key)?;
            }
            K::MM => {
                from_char_parse_int_len(Some(&mut out.mm), s, &mut p, key.name.len(), nodes, idx)?;
                skip_thth(s, &mut p, suffix);
            }
            K::DAY | K::Day | K::day => {
                let v = from_char_seq_search(s, &mut p, &dt::DAYS, key)?;
                from_char_set_int(&mut out.d, v as i32, key)?;
                out.d += 1;
            }
            K::DY | K::Dy | K::dy => {
                let v = from_char_seq_search(s, &mut p, &DAYS_SHORT, key)?;
                from_char_set_int(&mut out.d, v as i32, key)?;
                out.d += 1;
            }
            K::DDD => {
                from_char_parse_int_len(Some(&mut out.ddd), s, &mut p, key.name.len(), nodes, idx)?;
                skip_thth(s, &mut p, suffix);
            }
            K::IDDD => {
                from_char_parse_int_len(Some(&mut out.ddd), s, &mut p, 3, nodes, idx)?;
                skip_thth(s, &mut p, suffix);
            }
            K::DD => {
                from_char_parse_int_len(Some(&mut out.dd), s, &mut p, key.name.len(), nodes, idx)?;
                skip_thth(s, &mut p, suffix);
            }
            K::D => {
                from_char_parse_int_len(Some(&mut out.d), s, &mut p, key.name.len(), nodes, idx)?;
                skip_thth(s, &mut p, suffix);
            }
            K::ID => {
                from_char_parse_int_len(Some(&mut out.d), s, &mut p, 1, nodes, idx)?;
                out.d += 1;
                if out.d > 7 {
                    out.d = 1;
                }
                skip_thth(s, &mut p, suffix);
            }
            K::WW | K::IW => {
                from_char_parse_int_len(Some(&mut out.ww), s, &mut p, key.name.len(), nodes, idx)?;
                skip_thth(s, &mut p, suffix);
            }
            K::Q => {
                from_char_parse_int_len(None, s, &mut p, key.name.len(), nodes, idx)?;
                skip_thth(s, &mut p, suffix);
            }
            K::CC => {
                from_char_parse_int_len(Some(&mut out.cc), s, &mut p, key.name.len(), nodes, idx)?;
                skip_thth(s, &mut p, suffix);
            }
            K::Y_YYY => {
                // sscanf("%d,%03d%n")
                let (mil, e1, _) = c_strtol(s, p);
                let ok1 = e1 > p;
                let mut matched = 0;
                let mut years = 0i64;
                let mut nch = 0usize;
                if ok1 {
                    matched = 1;
                    if s.get(e1) == Some(&b',') {
                        // %03d: up to 3 chars (whitespace skipped, sign allowed).
                        let mut q = e1 + 1;
                        while q < s.len() && c_isspace(s[q]) {
                            q += 1;
                        }
                        let st = q;
                        let mut lim = st + 3;
                        if q < s.len() && (s[q] == b'+' || s[q] == b'-') {
                            q += 1;
                        }
                        lim = lim.min(s.len());
                        while q < lim && s[q].is_ascii_digit() {
                            q += 1;
                        }
                        let txt = std::str::from_utf8(&s[st..q]).unwrap_or("");
                        if let Ok(v) = txt.parse::<i64>() {
                            years = v;
                            matched = 2;
                            nch = q - p;
                        }
                    }
                }
                if matched < 2 {
                    bail!(
                        INVALID_DATETIME_FORMAT,
                        "invalid value \"{}\" for \"Y,YYY\"",
                        String::from_utf8_lossy(&s[p..])
                    );
                }
                let total = (mil as i32)
                    .checked_mul(1000)
                    .and_then(|m| (years as i32).checked_add(m))
                    .filter(|_| mil >= i32::MIN as i64 && mil <= i32::MAX as i64)
                    .ok_or_else(|| {
                        err!(
                            DATETIME_FIELD_OVERFLOW,
                            "value for \"Y,YYY\" in source string is out of range"
                        )
                    })?;
                from_char_set_int(&mut out.year, total, key)?;
                out.yysz = 4;
                p += nch;
                skip_thth(s, &mut p, suffix);
            }
            K::YYYY | K::IYYY => {
                from_char_parse_int_len(Some(&mut out.year), s, &mut p, key.name.len(), nodes, idx)?;
                out.yysz = 4;
                skip_thth(s, &mut p, suffix);
            }
            K::YYY | K::IYY | K::YY | K::IY | K::Y | K::I => {
                let len = from_char_parse_int_len(Some(&mut out.year), s, &mut p, key.name.len(), nodes, idx)?;
                if len < 4 {
                    out.year = adjust_partial_year_to_2020(out.year);
                }
                out.yysz = match key.id {
                    K::YYY | K::IYY => 3,
                    K::YY | K::IY => 2,
                    _ => 1,
                };
                skip_thth(s, &mut p, suffix);
            }
            K::RM | K::rm => {
                let v = from_char_seq_search(s, &mut p, &RM_MONTHS_LOWER, key)?;
                from_char_set_int(&mut out.mm, 12 - v as i32, key)?;
            }
            K::W => {
                from_char_parse_int_len(Some(&mut out.w), s, &mut p, key.name.len(), nodes, idx)?;
                skip_thth(s, &mut p, suffix);
            }
            K::J => {
                from_char_parse_int_len(Some(&mut out.j), s, &mut p, key.name.len(), nodes, idx)?;
                skip_thth(s, &mut p, suffix);
            }
            _ => {}
        }
        if !fx_mode {
            extra_skip = 0;
            while p < s.len() && c_isspace(s[p]) {
                p += 1;
                extra_skip += 1;
            }
        }
        idx += 1;
    }
    if std {
        if idx < nodes.len() {
            bail!(
                INVALID_DATETIME_FORMAT,
                "input string is too short for datetime format"
            );
        }
        while p < s.len() && c_isspace(s[p]) {
            p += 1;
        }
        if p < s.len() {
            bail!(
                INVALID_DATETIME_FORMAT,
                "trailing characters remain in input string after datetime format"
            );
        }
    }
    Ok(())
}

const DCH_DATED: u32 = 0x01;
const DCH_TIMED: u32 = 0x02;
const DCH_ZONED: u32 = 0x04;

/// DCH_datetime_type.
fn dch_datetime_type(nodes: &[Node]) -> u32 {
    let mut flags = 0;
    for n in nodes {
        let Node::Action { key, .. } = n else { continue };
        flags |= match key.id {
            K::FX => 0,
            K::A_M | K::P_M | K::a_m | K::p_m | K::AM | K::PM | K::am | K::pm | K::HH
            | K::HH12 | K::HH24 | K::MI | K::SS | K::MS | K::US | K::FF1 | K::FF2 | K::FF3
            | K::FF4 | K::FF5 | K::FF6 | K::SSSS => DCH_TIMED,
            K::tz | K::TZ | K::OF | K::TZH | K::TZM => DCH_ZONED,
            _ => DCH_DATED,
        };
    }
    flags
}

/// What do_to_timestamp produces.
pub struct ToTimestamp {
    pub tm: Tm,
    pub fsec: i32,
    /// GMT offset in seconds west, when the input had one.
    pub tz: Option<i32>,
    pub fprec: i32,
    pub flags: u32,
}

/// do_to_timestamp.
fn do_to_timestamp(input: &str, fmt: &str, std: bool, cx: &DtCtx) -> Result<ToTimestamp> {
    let mut tmfc = TmFromChar::default();
    let mut tm = Tm {
        mday: 1,
        mon: 1,
        ..Default::default()
    };
    let mut fsec: i32 = 0;
    let mut tz: Option<i32> = None;
    let mut fmask = 0;
    let mut flags = 0;
    let ferr = |e: DtErr| dt_parse_error(e, input, "timestamp");
    if !fmt.is_empty() {
        let nodes = parse_format(fmt, std, None)?;
        dch_from_char(&nodes, input, &mut tmfc, std, cx)?;
        flags = dch_datetime_type(&nodes);
    }
    if tmfc.ssss != 0 {
        let mut x = tmfc.ssss;
        tm.hour = x / 3600;
        x %= 3600;
        tm.min = x / 60;
        x %= 60;
        tm.sec = x;
    }
    if tmfc.ss != 0 {
        tm.sec = tmfc.ss;
    }
    if tmfc.mi != 0 {
        tm.min = tmfc.mi;
    }
    if tmfc.hh != 0 {
        tm.hour = tmfc.hh;
    }
    if tmfc.clock12 {
        if tm.hour < 1 || tm.hour > 12 {
            return Err(err!(
                INVALID_DATETIME_FORMAT,
                "hour \"{}\" is invalid for the 12-hour clock",
                tm.hour
            )
            .hint("Use the 24-hour clock, or give an hour between 1 and 12."));
        }
        if tmfc.pm != 0 && tm.hour < 12 {
            tm.hour += 12;
        } else if tmfc.pm == 0 && tm.hour == 12 {
            tm.hour = 0;
        }
    }
    if tmfc.year != 0 {
        if tmfc.cc != 0 && tmfc.yysz <= 2 {
            if tmfc.bc != 0 {
                tmfc.cc = -tmfc.cc;
            }
            tm.year = tmfc.year % 100;
            if tm.year != 0 {
                if tmfc.cc >= 0 {
                    tm.year = (tmfc.cc - 1)
                        .checked_mul(100)
                        .and_then(|t| tm.year.checked_add(t))
                        .ok_or_else(|| ferr(DtErr::FieldOverflow))?;
                } else {
                    tm.year = (tmfc.cc + 1)
                        .checked_mul(100)
                        .and_then(|t| t.checked_sub(tm.year))
                        .and_then(|t| t.checked_add(1))
                        .ok_or_else(|| ferr(DtErr::FieldOverflow))?;
                }
            } else {
                tm.year = tmfc.cc * 100 + if tmfc.cc >= 0 { 0 } else { 1 };
            }
        } else {
            tm.year = tmfc.year;
            if tmfc.bc != 0 {
                tm.year = -tm.year;
            }
            if tm.year < 0 {
                tm.year += 1;
            }
        }
        fmask |= dtk_m(dt::YEAR);
    } else if tmfc.cc != 0 {
        if tmfc.bc != 0 {
            tmfc.cc = -tmfc.cc;
        }
        tm.year = if tmfc.cc >= 0 {
            (tmfc.cc - 1)
                .checked_mul(100)
                .and_then(|t| t.checked_add(1))
                .ok_or_else(|| ferr(DtErr::FieldOverflow))?
        } else {
            tmfc.cc
                .checked_mul(100)
                .and_then(|t| t.checked_add(1))
                .ok_or_else(|| ferr(DtErr::FieldOverflow))?
        };
        fmask |= dtk_m(dt::YEAR);
    }
    if tmfc.j != 0 {
        (tm.year, tm.mon, tm.mday) = dt::j2date(tmfc.j);
        fmask |= DTK_DATE_M;
    }
    if tmfc.ww != 0 {
        if tmfc.mode == Some(Mode::IsoWeek) {
            (tm.year, tm.mon, tm.mday) = if tmfc.d != 0 {
                dt::isoweekdate2date(tmfc.ww, tmfc.d, tm.year)
            } else {
                dt::isoweek2date(tmfc.ww, tm.year)
            };
            fmask |= DTK_DATE_M;
        } else {
            tmfc.ddd = tmfc
                .ww
                .checked_sub(1)
                .and_then(|v| v.checked_mul(7))
                .and_then(|v| v.checked_add(1))
                .ok_or_else(|| ferr(DtErr::FieldOverflow))?;
        }
    }
    if tmfc.w != 0 {
        tmfc.dd = tmfc
            .w
            .checked_sub(1)
            .and_then(|v| v.checked_mul(7))
            .and_then(|v| v.checked_add(1))
            .ok_or_else(|| ferr(DtErr::FieldOverflow))?;
    }
    if tmfc.dd != 0 {
        tm.mday = tmfc.dd;
        fmask |= dtk_m(dt::DAY);
    }
    if tmfc.mm != 0 {
        tm.mon = tmfc.mm;
        fmask |= dtk_m(dt::MONTH);
    }
    if tmfc.ddd != 0 && (tm.mon <= 1 || tm.mday <= 1) {
        if tm.year == 0 && tmfc.bc == 0 {
            bail!(
                INVALID_DATETIME_FORMAT,
                "cannot calculate day of year without year information"
            );
        }
        if tmfc.mode == Some(Mode::IsoWeek) {
            let j0 = dt::isoweek2j(tm.year, 1) - 1;
            (tm.year, tm.mon, tm.mday) = dt::j2date(j0 + tmfc.ddd);
            fmask |= DTK_DATE_M;
        } else {
            const YSUM: [[i32; 13]; 2] = [
                [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 365],
                [0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335, 366],
            ];
            let y = &YSUM[dt::isleap(tm.year) as usize];
            let mut i = 1;
            while i <= 12 {
                if tmfc.ddd <= y[i] {
                    break;
                }
                i += 1;
            }
            let i = i.min(12);
            if tm.mon <= 1 {
                tm.mon = i as i32;
            }
            if tm.mday <= 1 {
                tm.mday = tmfc.ddd - y[i - 1];
            }
            fmask |= dtk_m(dt::MONTH) | dtk_m(dt::DAY);
        }
    }
    if tmfc.ms != 0 {
        fsec = tmfc
            .ms
            .checked_mul(1000)
            .and_then(|v| fsec.checked_add(v))
            .ok_or_else(|| ferr(DtErr::FieldOverflow))?;
    }
    if tmfc.us != 0 {
        fsec = fsec.wrapping_add(tmfc.us);
    }
    if fmask != 0 && dt::validate_date(fmask, true, false, false, &mut tm).is_err() {
        return Err(ferr(DtErr::FieldOverflow));
    }
    if !(0..24).contains(&tm.hour)
        || !(0..60).contains(&tm.min)
        || !(0..60).contains(&tm.sec)
        || !(0..1_000_000).contains(&fsec)
    {
        return Err(ferr(DtErr::FieldOverflow));
    }
    if tmfc.tzsign != 0 {
        if tmfc.tzh < 0 || tmfc.tzh > 15 || tmfc.tzm < 0 || tmfc.tzm >= 60 {
            return Err(ferr(DtErr::TzdispOverflow));
        }
        let mut g = (tmfc.tzh * 60 + tmfc.tzm) * 60;
        if tmfc.tzsign > 0 {
            g = -g;
        }
        tz = Some(g);
    } else if tmfc.has_tz {
        tz = Some(match (&tmfc.tzp, &tmfc.abbrev) {
            (Some(z), Some(a)) => dt::determine_tz_abbrev_offset(&tm, a.to_ascii_lowercase().as_bytes(), z).0,
            _ => -tmfc.gmtoffset,
        });
    }
    Ok(ToTimestamp {
        tm,
        fsec,
        tz,
        fprec: tmfc.ff,
        flags,
    })
}

/// to_timestamp(text, text).
pub fn to_timestamp(input: &str, fmt: &str, cx: &DtCtx) -> Result<i64> {
    let r = do_to_timestamp(input, fmt, false, cx)?;
    let tz = match r.tz {
        Some(t) => t,
        None => dt::determine_tz_offset(&r.tm, cx.tz).0,
    };
    let ts = dt::tm2timestamp(&r.tm, r.fsec, Some(tz))
        .ok_or_else(|| err!(DATETIME_FIELD_OVERFLOW, "timestamp out of range"))?;
    Ok(if r.fprec != 0 {
        dt::round_micros(ts, r.fprec)
    } else {
        ts
    })
}

/// to_date(text, text).
pub fn to_date(input: &str, fmt: &str, cx: &DtCtx) -> Result<i32> {
    let r = do_to_timestamp(input, fmt, false, cx)?;
    let tm = r.tm;
    if !dt::is_valid_julian(tm.year, tm.mon, tm.mday) {
        bail!(DATETIME_FIELD_OVERFLOW, "date out of range: \"{input}\"");
    }
    let d = date2j(tm.year, tm.mon, tm.mday) - POSTGRES_EPOCH_JDATE;
    if !dt::is_valid_date(d) {
        bail!(DATETIME_FIELD_OVERFLOW, "date out of range: \"{input}\"");
    }
    Ok(d)
}

/// The typed result of parse_datetime (jsonpath's `.datetime(template)`).
pub enum ParsedDatetime {
    Date(i32),
    Time(i64),
    TimeTz(i64, i32),
    Timestamp(i64),
    TimestampTz(i64, i32),
}

/// parse_datetime: the type follows the template's date/time/zone fields. `tz` west.
pub fn parse_datetime(input: &str, fmt: &str, strict: bool, cx: &DtCtx) -> Result<ParsedDatetime> {
    let r = do_to_timestamp(input, fmt, strict, cx)?;
    let typmod = if r.fprec != 0 { r.fprec } else { -1 };
    let flags = r.flags;
    if flags & DCH_DATED != 0 {
        if flags & DCH_TIMED != 0 {
            if flags & DCH_ZONED != 0 {
                let Some(tz) = r.tz else {
                    bail!(
                        INVALID_DATETIME_FORMAT,
                        "missing time zone in input string for type timestamptz"
                    );
                };
                let ts = dt::tm2timestamp(&r.tm, r.fsec, Some(tz))
                    .ok_or_else(|| err!(DATETIME_FIELD_OVERFLOW, "timestamptz out of range"))?;
                return Ok(ParsedDatetime::TimestampTz(dt::round_micros(ts, typmod), tz));
            }
            let ts = dt::tm2timestamp(&r.tm, r.fsec, None)
                .ok_or_else(|| err!(DATETIME_FIELD_OVERFLOW, "timestamp out of range"))?;
            return Ok(ParsedDatetime::Timestamp(dt::round_micros(ts, typmod)));
        }
        if flags & DCH_ZONED != 0 {
            bail!(
                INVALID_DATETIME_FORMAT,
                "datetime format is zoned but not timed"
            );
        }
        let tm = r.tm;
        if !dt::is_valid_julian(tm.year, tm.mon, tm.mday) {
            bail!(DATETIME_FIELD_OVERFLOW, "date out of range: \"{input}\"");
        }
        let d = date2j(tm.year, tm.mon, tm.mday) - POSTGRES_EPOCH_JDATE;
        if !dt::is_valid_date(d) {
            bail!(DATETIME_FIELD_OVERFLOW, "date out of range: \"{input}\"");
        }
        return Ok(ParsedDatetime::Date(d));
    }
    if flags & DCH_TIMED != 0 {
        let t = dt::time2t(r.tm.hour, r.tm.min, r.tm.sec, r.fsec);
        if flags & DCH_ZONED != 0 {
            let Some(tz) = r.tz else {
                bail!(
                    INVALID_DATETIME_FORMAT,
                    "missing time zone in input string for type timetz"
                );
            };
            return Ok(ParsedDatetime::TimeTz(dt::round_micros(t, typmod), tz));
        }
        return Ok(ParsedDatetime::Time(dt::round_micros(t, typmod)));
    }
    bail!(
        INVALID_DATETIME_FORMAT,
        "datetime format is not dated and not timed"
    )
}

/// datetime_format_has_tz.
pub fn datetime_format_has_tz(fmt: &str) -> bool {
    parse_format(fmt, false, None)
        .map(|n| dch_datetime_type(&n) & DCH_ZONED != 0)
        .unwrap_or(false)
}

// ── numbers ──

/// int_to_roman: upper-case, unpadded; out of range gives 15 '#'.
fn int_to_roman(number: i32) -> String {
    if !(1..=3999).contains(&number) {
        return "#".repeat(MAX_ROMAN_LEN);
    }
    let s = number.to_string();
    let mut len = s.len();
    let mut out = String::new();
    for c in s.bytes() {
        let num = c as i32 - (b'0' as i32 + 1);
        if num >= 0 {
            let num = num as usize;
            match len {
                4 => {
                    for _ in 0..=num {
                        out.push('M');
                    }
                }
                3 => out.push_str(RM100[num]),
                2 => out.push_str(RM10[num]),
                1 => out.push_str(RM1[num]),
                _ => {}
            }
        }
        len -= 1;
    }
    out
}

fn roman_val(c: u8) -> i32 {
    match c {
        b'I' => 1,
        b'V' => 5,
        b'X' => 10,
        b'L' => 50,
        b'C' => 100,
        b'D' => 500,
        b'M' => 1000,
        _ => 0,
    }
}

fn valid_sub_comb(c: u8, n: u8) -> bool {
    (c == b'I' && (n == b'V' || n == b'X'))
        || (c == b'X' && (n == b'L' || n == b'C'))
        || (c == b'C' && (n == b'D' || n == b'M'))
}

/// roman_to_int: (value or -1, bytes consumed).
fn roman_to_int(input: &[u8], start: usize) -> (i32, usize) {
    let mut p = start;
    while p < input.len() && c_isspace(input[p]) {
        p += 1;
    }
    let mut chars = Vec::new();
    let mut vals = Vec::new();
    while chars.len() < MAX_ROMAN_LEN && p < input.len() {
        let c = input[p].to_ascii_uppercase();
        let v = roman_val(c);
        if v == 0 {
            break;
        }
        chars.push(c);
        vals.push(v);
        p += 1;
    }
    let len = chars.len();
    if len == 0 {
        return (-1, p);
    }
    let (mut result, mut repeat, mut vc, mut lc, mut dc) = (0, 1, 0, 0, 0);
    let mut sub = false;
    let mut last_sub = 0;
    let mut i = 0;
    while i < len {
        let (c, v) = (chars[i], vals[i]);
        if sub && v >= last_sub {
            return (-1, p);
        }
        if (vc > 0 && v >= 5) || (lc > 0 && v >= 50) || (dc > 0 && v >= 500) {
            return (-1, p);
        }
        match c {
            b'V' => vc += 1,
            b'L' => lc += 1,
            b'D' => dc += 1,
            _ => {}
        }
        if i < len - 1 {
            let (nc, nv) = (chars[i + 1], vals[i + 1]);
            if v < nv {
                if !valid_sub_comb(c, nc) || repeat > 1 {
                    return (-1, p);
                }
                if (vc > 0 && nv >= 5) || (lc > 0 && nv >= 50) || (dc > 0 && nv >= 500) {
                    return (-1, p);
                }
                match nc {
                    b'V' => vc += 1,
                    b'L' => lc += 1,
                    b'D' => dc += 1,
                    _ => {}
                }
                i += 1;
                repeat = 1;
                sub = true;
                last_sub = v;
                result += nv - v;
            } else {
                if c == nc {
                    repeat += 1;
                    if repeat > 3 {
                        return (-1, p);
                    }
                } else {
                    repeat = 1;
                }
                result += v;
            }
        } else {
            result += v;
        }
        i += 1;
    }
    (result, p)
}

/// NUM_processor state.
struct NumProc<'a> {
    num: &'a mut NumDesc,
    sign: u8,
    sign_wrote: bool,
    num_count: i32,
    num_in: bool,
    num_curr: i32,
    out_pre_spaces: i32,
    read_dec: bool,
    read_post: i32,
    read_pre: i32,
    /// The digits (to_char) or the collected number (to_number, `number[0]` = sign).
    number: Vec<u8>,
    number_p: usize,
    /// Output (to_char) or input (to_number).
    inout: Vec<u8>,
    inout_p: usize,
    last_relevant: Option<usize>,
    decimal: &'static str,
}

impl NumProc<'_> {
    fn num_at(&self, i: usize) -> u8 {
        self.number.get(i).copied().unwrap_or(0)
    }

    fn put(&mut self, c: u8) {
        if self.inout_p < self.inout.len() {
            self.inout[self.inout_p] = c;
        } else {
            self.inout.resize(self.inout_p, b' ');
            self.inout.push(c);
        }
    }

    fn put_str(&mut self, s: &[u8]) {
        for (k, &c) in s.iter().enumerate() {
            let at = self.inout_p + k;
            if at < self.inout.len() {
                self.inout[at] = c;
            } else {
                self.inout.resize(at, b' ');
                self.inout.push(c);
            }
        }
    }

    fn add_symbol(&mut self, sym: &str) {
        let b = sym.as_bytes();
        let b = &b[..b.len().min(NUM_MAX_ITEM_SIZ)];
        self.put_str(b);
        self.inout_p += b.len();
    }

    fn is_predec_space(&self) -> bool {
        !self.num.is(NUM_F_ZERO)
            && self.number_p == 0
            && self.num_at(0) == b'0'
            && self.num.post != 0
    }

    fn last_relevant_is_dot(&self) -> bool {
        self.last_relevant
            .is_some_and(|i| self.num_at(i) == b'.')
    }

    /// NUM_numpart_to_char.
    fn numpart_to_char(&mut self, id: K) {
        if self.num.is(NUM_F_ROMAN) {
            return;
        }
        self.num_in = false;
        if !self.sign_wrote
            && (self.num_curr >= self.out_pre_spaces
                || (self.num.is(NUM_F_ZERO) && self.num.zero_start == self.num_curr))
            && (!self.is_predec_space() || self.last_relevant_is_dot())
        {
            if self.num.is(NUM_F_LSIGN) {
                if self.num.lsign == NUM_LSIGN_PRE {
                    self.add_symbol(if self.sign == b'-' { "-" } else { "+" });
                    self.sign_wrote = true;
                }
            } else if self.num.is(NUM_F_BRACKET) {
                self.put(if self.sign == b'+' { b' ' } else { b'<' });
                self.inout_p += 1;
                self.sign_wrote = true;
            } else if self.sign == b'+' {
                if !self.num.is(NUM_F_FILLMODE) {
                    self.put(b' ');
                    self.inout_p += 1;
                }
                self.sign_wrote = true;
            } else if self.sign == b'-' {
                self.put(b'-');
                self.inout_p += 1;
                self.sign_wrote = true;
            }
        }
        if matches!(id, K::N9 | K::N0 | K::ND | K::NDec) {
            if self.num_curr < self.out_pre_spaces
                && (self.num.zero_start > self.num_curr || !self.num.is(NUM_F_ZERO))
            {
                if !self.num.is(NUM_F_FILLMODE) {
                    self.put(b' ');
                    self.inout_p += 1;
                }
            } else if self.num.is(NUM_F_ZERO)
                && self.num_curr < self.out_pre_spaces
                && self.num.zero_start <= self.num_curr
            {
                self.put(b'0');
                self.inout_p += 1;
                self.num_in = true;
            } else {
                if self.num_at(self.number_p) == b'.' {
                    if !self.last_relevant_is_dot()
                        || (self.num.is(NUM_F_FILLMODE) && self.last_relevant_is_dot())
                    {
                        self.add_symbol(self.decimal);
                    }
                } else if self.last_relevant.is_some_and(|lr| self.number_p > lr) && id != K::N0
                {
                } else if self.is_predec_space() {
                    if !self.num.is(NUM_F_FILLMODE) {
                        self.put(b' ');
                        self.inout_p += 1;
                    } else if self.last_relevant_is_dot() {
                        self.put(b'0');
                        self.inout_p += 1;
                    }
                } else {
                    let c = self.num_at(self.number_p);
                    self.put(c);
                    self.inout_p += 1;
                    self.num_in = true;
                }
                if self.num_at(self.number_p) != 0 {
                    self.number_p += 1;
                }
            }
            let mut end = self.num_count
                + if self.out_pre_spaces != 0 { 1 } else { 0 }
                + if self.num.is(NUM_F_DECIMAL) { 1 } else { 0 };
            if self.last_relevant.is_some_and(|lr| lr == self.number_p) {
                end = self.num_curr;
            }
            if self.num_curr + 1 == end {
                if self.sign_wrote && self.num.is(NUM_F_BRACKET) {
                    self.put(if self.sign == b'+' { b' ' } else { b'>' });
                    self.inout_p += 1;
                } else if self.num.is(NUM_F_LSIGN) && self.num.lsign == NUM_LSIGN_POST {
                    self.add_symbol(if self.sign == b'-' { "-" } else { "+" });
                }
            }
        }
        self.num_curr += 1;
    }

    fn overload(&self) -> bool {
        self.inout_p >= self.inout.len()
    }

    fn amount(&self, s: usize) -> bool {
        self.inout_p + s <= self.inout.len()
    }

    fn in_at(&self, i: usize) -> u8 {
        self.inout.get(i).copied().unwrap_or(0)
    }

    fn set_number(&mut self, i: usize, c: u8) {
        if i < self.number.len() {
            self.number[i] = c;
        } else {
            self.number.resize(i, b'0');
            self.number.push(c);
        }
    }

    /// NUM_numpart_from_char.
    fn numpart_from_char(&mut self, id: K) {
        let mut isread = false;
        if self.overload() {
            return;
        }
        if self.in_at(self.inout_p) == b' ' {
            self.inout_p += 1;
        }
        if self.overload() {
            return;
        }
        if self.num_at(0) == b' '
            && matches!(id, K::N0 | K::N9)
            && self.read_pre + self.read_post == 0
        {
            if self.num.is(NUM_F_LSIGN) && self.num.lsign == NUM_LSIGN_PRE {
                if self.amount(1) && self.in_at(self.inout_p) == b'-' {
                    self.inout_p += 1;
                    self.number[0] = b'-';
                } else if self.amount(1) && self.in_at(self.inout_p) == b'+' {
                    self.inout_p += 1;
                    self.number[0] = b'+';
                }
            } else {
                let c = self.in_at(self.inout_p);
                if c == b'-' || (self.num.is(NUM_F_BRACKET) && c == b'<') {
                    self.number[0] = b'-';
                    self.inout_p += 1;
                } else if c == b'+' {
                    self.number[0] = b'+';
                    self.inout_p += 1;
                }
            }
        }
        if self.overload() {
            return;
        }
        let c = self.in_at(self.inout_p);
        if c.is_ascii_digit() {
            if self.read_dec && self.read_post == self.num.post {
                return;
            }
            let at = self.number_p;
            self.set_number(at, c);
            self.number_p += 1;
            if self.read_dec {
                self.read_post += 1;
            } else {
                self.read_pre += 1;
            }
            isread = true;
        } else if self.num.is(NUM_F_DECIMAL) && !self.read_dec {
            let x = self.decimal.len();
            if x > 0
                && self.amount(x)
                && self.inout[self.inout_p..].starts_with(self.decimal.as_bytes())
            {
                self.inout_p += x - 1;
                let at = self.number_p;
                self.set_number(at, b'.');
                self.number_p += 1;
                self.read_dec = true;
                isread = true;
            }
        }
        if self.overload() {
            return;
        }
        if self.num_at(0) == b' ' && self.read_pre + self.read_post > 0 {
            if self.num.is(NUM_F_LSIGN)
                && isread
                && self.inout_p + 1 < self.inout.len()
                && !self.in_at(self.inout_p + 1).is_ascii_digit()
            {
                let tmp = self.inout_p;
                self.inout_p += 1;
                if self.amount(1) && self.in_at(self.inout_p) == b'-' {
                    self.number[0] = b'-';
                } else if self.amount(1) && self.in_at(self.inout_p) == b'+' {
                    self.number[0] = b'+';
                }
                if self.num_at(0) == b' ' {
                    self.inout_p = tmp;
                }
            } else if !isread
                && !self.num.is(NUM_F_LSIGN)
                && (self.num.is(NUM_F_PLUS) || self.num.is(NUM_F_MINUS))
            {
                let c = self.in_at(self.inout_p);
                if c == b'-' || c == b'+' {
                    self.number[0] = c;
                }
            }
        }
    }

    /// NUM_eat_non_data_chars.
    fn eat_non_data(&mut self, n: usize) {
        for _ in 0..n {
            if self.overload() {
                break;
            }
            if b"0123456789.,+-".contains(&self.in_at(self.inout_p)) {
                break;
            }
            self.inout_p += mblen(&self.inout, self.inout_p).max(1);
        }
    }
}

/// get_last_relevant_decnum: index of the last relevant char after the point.
fn get_last_relevant_decnum(num: &[u8]) -> Option<usize> {
    let p = num.iter().position(|&c| c == b'.')?;
    let mut result = p;
    for (i, &c) in num.iter().enumerate().skip(p + 1) {
        if c != b'0' {
            result = i;
        }
    }
    Some(result)
}

/// NUM_processor. For to_char returns the output; for to_number the number string.
fn num_processor(
    nodes: &[Node],
    num: &mut NumDesc,
    inout: &[u8],
    number: &[u8],
    out_pre_spaces: i32,
    sign: u8,
    is_to_char: bool,
) -> Result<String> {
    if num.zero_start != 0 {
        num.zero_start -= 1;
    }
    if num.is(NUM_F_EEEE) {
        if !is_to_char {
            bail!(FEATURE_NOT_SUPPORTED, "\"EEEE\" not supported for input");
        }
        return Ok(String::from_utf8_lossy(number).into_owned());
    }
    let decimal = ".";
    let mut np = NumProc {
        num,
        sign: 0,
        sign_wrote: false,
        num_count: 0,
        num_in: false,
        num_curr: 0,
        out_pre_spaces: 0,
        read_dec: false,
        read_post: 0,
        read_pre: 0,
        number: number.to_vec(),
        number_p: 0,
        inout: if is_to_char { Vec::new() } else { inout.to_vec() },
        inout_p: 0,
        last_relevant: None,
        decimal,
    };
    if is_to_char {
        np.sign = sign;
        if np.num.is(NUM_F_PLUS) || np.num.is(NUM_F_MINUS) {
            np.sign_wrote = !(np.num.is(NUM_F_PLUS) && !np.num.is(NUM_F_MINUS));
        } else {
            if np.sign != b'-' && np.num.is(NUM_F_FILLMODE) {
                np.num.flag &= !NUM_F_BRACKET;
            }
            np.sign_wrote =
                np.sign == b'+' && np.num.is(NUM_F_FILLMODE) && !np.num.is(NUM_F_LSIGN);
            if np.num.lsign == NUM_LSIGN_PRE && np.num.pre == np.num.pre_lsign_num {
                np.num.lsign = NUM_LSIGN_POST;
            }
        }
    }
    np.num_count = np.num.post + np.num.pre - 1;
    if is_to_char {
        np.out_pre_spaces = out_pre_spaces;
        if np.num.is(NUM_F_FILLMODE) && np.num.is(NUM_F_DECIMAL) {
            np.last_relevant = get_last_relevant_decnum(&np.number);
            if let Some(lr) = np.last_relevant
                && np.num.zero_end > np.out_pre_spaces
            {
                let mut last_zero_pos = np.number.len() as i32 - 1;
                last_zero_pos = last_zero_pos.min(np.num.zero_end - np.out_pre_spaces);
                if (lr as i32) < last_zero_pos {
                    np.last_relevant = Some(last_zero_pos as usize);
                }
            }
        }
        if !np.sign_wrote && np.out_pre_spaces == 0 {
            np.num_count += 1;
        }
    } else {
        np.out_pre_spaces = 0;
        np.number = vec![b' '];
    }
    np.num_in = false;
    np.num_curr = 0;
    np.number_p = if is_to_char { 0 } else { 1 };
    for n in nodes {
        if !is_to_char && np.overload() {
            break;
        }
        let Node::Action { key, .. } = n else {
            if is_to_char {
                let c = n.character().as_bytes().to_vec();
                np.put_str(&c);
                np.inout_p += c.len();
            } else {
                np.inout_p += mblen(&np.inout, np.inout_p).max(1);
            }
            continue;
        };
        match key.id {
            K::N9 | K::N0 | K::NDec | K::ND => {
                if is_to_char {
                    np.numpart_to_char(key.id);
                    continue;
                }
                np.numpart_from_char(key.id);
            }
            K::NComma => {
                if is_to_char {
                    if !np.num_in {
                        if np.num.is(NUM_F_FILLMODE) {
                            continue;
                        }
                        np.put(b' ');
                    } else {
                        np.put(b',');
                    }
                } else {
                    if !np.num_in && np.num.is(NUM_F_FILLMODE) {
                        continue;
                    }
                    if np.in_at(np.inout_p) != b',' {
                        continue;
                    }
                }
            }
            K::NG => {
                let pattern = ",";
                if is_to_char {
                    if !np.num_in {
                        if np.num.is(NUM_F_FILLMODE) {
                            continue;
                        }
                        np.put(b' ');
                    } else {
                        np.put_str(pattern.as_bytes());
                        np.inout_p += pattern.len() - 1;
                    }
                } else {
                    if !np.num_in && np.num.is(NUM_F_FILLMODE) {
                        continue;
                    }
                    if np.amount(pattern.len())
                        && np.inout[np.inout_p..].starts_with(pattern.as_bytes())
                    {
                        np.inout_p += pattern.len() - 1;
                    } else {
                        continue;
                    }
                }
            }
            K::NL => {
                let pattern = " ";
                if is_to_char {
                    np.put_str(pattern.as_bytes());
                    np.inout_p += pattern.len() - 1;
                } else {
                    np.eat_non_data(1);
                    continue;
                }
            }
            K::NRN | K::Nrn => {
                if is_to_char {
                    let mut r = String::from_utf8_lossy(&np.number[np.number_p..]).into_owned();
                    if key.id == K::Nrn {
                        r = r.to_ascii_lowercase();
                    }
                    let s = if np.num.is(NUM_F_FILLMODE) {
                        r
                    } else {
                        format!("{r:>15}")
                    };
                    np.put_str(s.as_bytes());
                    np.inout_p += s.len().max(1) - 1;
                } else {
                    let (v, end) = roman_to_int(&np.inout, np.inout_p);
                    np.inout_p = end;
                    if v < 0 {
                        bail!(INVALID_TEXT_REPRESENTATION, "invalid Roman numeral");
                    }
                    let digits = v.to_string();
                    for c in digits.bytes() {
                        let at = np.number_p;
                        np.set_number(at, c);
                        np.number_p += 1;
                    }
                    np.num.pre = digits.len() as i32;
                    np.num.post = 0;
                    continue;
                }
            }
            K::Nth | K::NTH => {
                if np.num.is(NUM_F_ROMAN)
                    || np.num_at(0) == b'#'
                    || np.sign == b'-'
                    || np.num.is(NUM_F_DECIMAL)
                {
                    continue;
                }
                if is_to_char {
                    let digits = String::from_utf8_lossy(&np.number).into_owned();
                    let th = get_th(&digits, key.id == K::NTH)?;
                    np.put_str(th.as_bytes());
                    np.inout_p += 1;
                } else {
                    np.eat_non_data(2);
                    continue;
                }
            }
            K::NMI => {
                if is_to_char {
                    if np.sign == b'-' {
                        np.put(b'-');
                    } else if np.num.is(NUM_F_FILLMODE) {
                        continue;
                    } else {
                        np.put(b' ');
                    }
                } else if np.in_at(np.inout_p) == b'-' {
                    np.number[0] = b'-';
                } else {
                    np.eat_non_data(1);
                    continue;
                }
            }
            K::NPL => {
                if is_to_char {
                    if np.sign == b'+' {
                        np.put(b'+');
                    } else if np.num.is(NUM_F_FILLMODE) {
                        continue;
                    } else {
                        np.put(b' ');
                    }
                } else if np.in_at(np.inout_p) == b'+' {
                    np.number[0] = b'+';
                } else {
                    np.eat_non_data(1);
                    continue;
                }
            }
            K::NSG => {
                if is_to_char {
                    let s = np.sign;
                    np.put(s);
                } else {
                    let c = np.in_at(np.inout_p);
                    if c == b'-' || c == b'+' {
                        np.number[0] = c;
                    } else {
                        np.eat_non_data(1);
                        continue;
                    }
                }
            }
            _ => continue,
        }
        np.inout_p += 1;
    }
    if is_to_char {
        // A digit position past the end of the number string writes its NUL, which ends
        // the C string there.
        let end = np.inout_p.min(np.inout.len());
        let end = np.inout[..end].iter().position(|&b| b == 0).unwrap_or(end);
        np.inout.truncate(end);
        return Ok(String::from_utf8_lossy(&np.inout).into_owned());
    }
    let mut out = np.number.clone();
    let cut = if np.number_p > 0 && np.num_at(np.number_p - 1) == b'.' {
        np.number_p - 1
    } else {
        np.number_p
    };
    out.truncate(cut.min(out.len()));
    np.num.post = np.read_post;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn num_prepare(fmt: &str) -> Result<(Vec<Node>, NumDesc)> {
    let mut num = NumDesc::default();
    let nodes = parse_format(fmt, false, Some(&mut num))?;
    Ok((nodes, num))
}

fn fill_hash(pre: i32, post: i32) -> Vec<u8> {
    let mut v = vec![b'#'; (pre + post + 1).max(0) as usize];
    if (pre as usize) < v.len() {
        v[pre as usize] = b'.';
    }
    v
}

fn fill_hash_eeee(pre: i32, post: i32) -> Vec<u8> {
    let mut v = vec![b'#'; (pre + post + 6).max(1) as usize];
    v[0] = b' ';
    if ((pre + 1) as usize) < v.len() {
        v[(pre + 1) as usize] = b'.';
    }
    v
}

/// C's `%+.*e`.
fn c_fmt_e_plus(v: f64, prec: usize) -> String {
    let s = format!("{:+.*e}", prec, v);
    let (m, e) = s.split_once('e').unwrap_or((&s, "0"));
    let x: i32 = e.parse().unwrap_or(0);
    format!("{m}e{}{:02}", if x < 0 { '-' } else { '+' }, x.abs())
}

/// `%.*f` through Postgres's own snprintf, which spells non-finite values "NaN" and
/// "[-]Infinity".
fn c_fmt_f(v: f64, prec: usize) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-Infinity" } else { "Infinity" }.into();
    }
    format!("{v:.prec$}")
}

/// The shared tail: pad / overflow from the digits string.
fn num_digits_prep(num: &mut NumDesc, orgnum: &str) -> (Vec<u8>, i32, u8) {
    let (sign, digits) = match orgnum.strip_prefix('-') {
        Some(d) => (b'-', d),
        None => (b'+', orgnum),
    };
    let pre_len = digits.find('.').unwrap_or(digits.len()) as i32;
    let mut out_pre_spaces = 0;
    let mut numstr = digits.as_bytes().to_vec();
    if pre_len < num.pre {
        out_pre_spaces = num.pre - pre_len;
    } else if pre_len > num.pre {
        numstr = fill_hash(num.pre, num.post);
    }
    (numstr, out_pre_spaces, sign)
}

/// numeric_out_sci.
pub fn numeric_out_sci(n: &Numeric, scale: i32) -> String {
    let rscale = scale.max(0) as u32;
    match n {
        Numeric::NaN => "NaN".into(),
        Numeric::PosInf => "Infinity".into(),
        Numeric::NegInf => "-Infinity".into(),
        Numeric::Finite { int, scale: s } => {
            use num_traits::{Signed, Zero};
            let exponent: i64 = if int.is_zero() {
                0
            } else {
                int.abs().to_string().len() as i64 - 1 - *s as i64
            };
            // significand = value / 10^exponent, rounded to rscale digits.
            let sig = if exponent >= 0 {
                Numeric::Finite {
                    int: int.clone(),
                    scale: s + exponent as u32,
                }
            } else {
                let mul = num_bigint::BigInt::from(10).pow((-exponent) as u32);
                Numeric::Finite {
                    int: int * mul,
                    scale: *s,
                }
            };
            let sig = sig.with_scale(rscale);
            format!(
                "{}e{}{:02}",
                sig,
                if exponent < 0 { '-' } else { '+' },
                exponent.abs()
            )
        }
    }
}

/// numeric_to_char / int4_to_char / int8_to_char / float4_to_char / float8_to_char.
pub enum NumArg {
    Numeric(Numeric),
    Int(i64, bool),
    Float4(f32),
    Float8(f64),
}

/// to_char for numbers.
pub fn number_to_char(v: &NumArg, fmt: &str) -> Result<String> {
    if fmt.is_empty() {
        return Ok(String::new());
    }
    let (nodes, mut num) = num_prepare(fmt)?;
    let mut out_pre_spaces = 0;
    let mut sign = 0u8;
    let numstr: Vec<u8>;
    if num.is(NUM_F_ROMAN) {
        let iv: i32 = match v {
            Numeric_(n) => match n.to_i64_round() {
                Some(x) if x >= i32::MIN as i64 && x <= i32::MAX as i64 => x as i32,
                _ => i32::MAX,
            },
            NumArg::Int(x, _) => {
                if *x <= i32::MAX as i64 && *x >= i32::MIN as i64 {
                    *x as i32
                } else {
                    i32::MAX
                }
            }
            NumArg::Float4(f) => {
                let r = (*f as f64).round_ties_even();
                if !r.is_nan() && r >= i32::MIN as f64 && r < -(i32::MIN as f64) {
                    r as i32
                } else {
                    i32::MAX
                }
            }
            NumArg::Float8(f) => {
                let r = f.round_ties_even();
                if !r.is_nan() && r >= i32::MIN as f64 && r < -(i32::MIN as f64) {
                    r as i32
                } else {
                    i32::MAX
                }
            }
        };
        numstr = int_to_roman(iv).into_bytes();
    } else if num.is(NUM_F_EEEE) {
        numstr = match v {
            Numeric_(n) => {
                let org = numeric_out_sci(n, num.post);
                if org == "NaN" || org == "Infinity" || org == "-Infinity" {
                    fill_hash_eeee(num.pre, num.post)
                } else if !org.starts_with('-') {
                    format!(" {org}").into_bytes()
                } else {
                    org.into_bytes()
                }
            }
            NumArg::Int(x, is4) => {
                if *is4 {
                    let mut s = c_fmt_e_plus(*x as f64, num.post.max(0) as usize);
                    if s.starts_with('+') {
                        s.replace_range(0..1, " ");
                    }
                    s.into_bytes()
                } else {
                    let org = numeric_out_sci(&Numeric::from_i64(*x), num.post);
                    if !org.starts_with('-') {
                        format!(" {org}").into_bytes()
                    } else {
                        org.into_bytes()
                    }
                }
            }
            NumArg::Float4(_) | NumArg::Float8(_) => {
                let f = match v {
                    NumArg::Float4(f) => *f as f64,
                    NumArg::Float8(f) => *f,
                    _ => 0.0,
                };
                if f.is_nan() || f.is_infinite() {
                    fill_hash_eeee(num.pre, num.post)
                } else {
                    let mut s = c_fmt_e_plus(f, num.post.max(0) as usize);
                    if s.starts_with('+') {
                        s.replace_range(0..1, " ");
                    }
                    s.into_bytes()
                }
            }
        };
    } else {
        match v {
            Numeric_(n) => {
                let mut val = n.clone();
                if num.is(NUM_F_MULTI) {
                    val = val.mul(&Numeric::from_i64(10).power(&Numeric::from_i64(num.multi as i64))?);
                    num.pre += num.multi;
                }
                let org = val.round(num.post as i64).to_string();
                let (s, ops, sg) = num_digits_prep(&mut num, &org);
                numstr = s;
                out_pre_spaces = ops;
                sign = sg;
            }
            NumArg::Int(x, is4) => {
                let mut value = *x as i128;
                if num.is(NUM_F_MULTI) {
                    let multi = 10f64.powi(num.multi);
                    let m = multi.round_ties_even();
                    if *is4 {
                        if !(m >= i32::MIN as f64 && m < -(i32::MIN as f64)) {
                            bail!(NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range");
                        }
                        value *= m as i128;
                        if value > i32::MAX as i128 || value < i32::MIN as i128 {
                            bail!(NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range");
                        }
                    } else {
                        if !(m >= i64::MIN as f64 && m < -(i64::MIN as f64)) {
                            bail!(NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range");
                        }
                        value *= m as i128;
                        if value > i64::MAX as i128 || value < i64::MIN as i128 {
                            bail!(NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range");
                        }
                    }
                    num.pre += num.multi;
                }
                let org = value.to_string();
                let (sg, digits) = match org.strip_prefix('-') {
                    Some(d) => (b'-', d.to_string()),
                    None => (b'+', org.clone()),
                };
                sign = sg;
                let pre_len = digits.len() as i32;
                let mut s = digits.clone();
                if num.post > 0 {
                    s.push('.');
                    s.push_str(&"0".repeat(num.post as usize));
                }
                let mut ns = s.into_bytes();
                if pre_len < num.pre {
                    out_pre_spaces = num.pre - pre_len;
                } else if pre_len > num.pre {
                    ns = fill_hash(num.pre, num.post);
                }
                numstr = ns;
            }
            NumArg::Float4(_) | NumArg::Float8(_) => {
                let (is4, value) = match v {
                    NumArg::Float4(f) => (true, *f as f64),
                    NumArg::Float8(f) => (false, *f),
                    _ => (false, 0.0),
                };
                let mut val = value;
                if num.is(NUM_F_MULTI) {
                    let multi = 10f64.powi(num.multi);
                    val = if is4 {
                        ((value as f32) * (multi as f32)) as f64
                    } else {
                        value * multi
                    };
                    num.pre += num.multi;
                }
                let dig = if is4 { 6 } else { 15 };
                let org0 = c_fmt_f(val.abs(), 0);
                let pre_len0 = org0.len() as i32;
                if pre_len0 >= dig {
                    num.post = 0;
                } else if pre_len0 + num.post > dig {
                    num.post = dig - pre_len0;
                }
                let org = if is4 {
                    c_fmt_f((val as f32) as f64, num.post as usize)
                } else {
                    c_fmt_f(val, num.post as usize)
                };
                let (s, ops, sg) = num_digits_prep(&mut num, &org);
                numstr = s;
                out_pre_spaces = ops;
                sign = sg;
            }
        }
    }
    num_processor(&nodes, &mut num, &[], &numstr, out_pre_spaces, sign, true)
}

use NumArg::Numeric as Numeric_;

/// numeric_to_number (to_number). None = NULL.
pub fn to_number(value: &str, fmt: &str) -> Result<Option<Numeric>> {
    if fmt.is_empty() {
        return Ok(None);
    }
    let (nodes, mut num) = num_prepare(fmt)?;
    let numstr = num_processor(&nodes, &mut num, value.as_bytes(), &[], 0, 0, false)?;
    let scale = num.post;
    let precision = num.pre + num.multi + scale;
    let typmod = ((precision << 16) | scale) + 4;
    let n = Numeric::parse_pg(&numstr)?.apply_typmod(typmod)?;
    if num.is(NUM_F_MULTI) {
        let x = Numeric::from_i64(10).power(&Numeric::from_i64(-(num.multi as i64)))?;
        return Ok(Some(n.mul(&x)));
    }
    Ok(Some(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        let f = |v: i64, fmt: &str| number_to_char(&NumArg::Int(v, true), fmt).unwrap();
        assert_eq!(f(123, "999"), " 123");
        assert_eq!(f(-123, "999"), "-123");
        assert_eq!(f(5, "FM999th"), "5th");
        assert_eq!(f(1999, "RN"), "        MCMXCIX");
        let n = |s: &str, fmt: &str| {
            number_to_char(&NumArg::Numeric(s.parse().unwrap()), fmt).unwrap()
        };
        assert_eq!(n("-1234.5", "9,999.99"), "-1,234.50");
        assert_eq!(n("0.5", "9.99"), "  .50");
        assert_eq!(n("12345", "9.99EEEE"), " 1.23e+04");
        assert_eq!(
            to_number("12,454.8-", "99G999D9S").unwrap().unwrap().to_string(),
            "-12454.8"
        );
    }
}
