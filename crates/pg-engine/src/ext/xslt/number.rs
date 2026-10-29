//! Number formatting for the native XSLT 1.0 processor: a byte-for-byte port of libxslt
//! 1.1.43 `numbers.c` (`format-number()` via `xsltFormatNumberConversion`, and `xsl:number`
//! via `[SECRET]` + `xsltNumberFormatInsertNumbers`).
//!
//! The port works on UTF-8 bytes the way the C code does, including its quirks: decimal-format
//! characters are compared by their first character only (except the grouping separator in the
//! integer part, which is matched as a whole string), the integer and fraction digits use the
//! first *byte* of `zero-digit`, and fixed-size scratch buffers truncate huge outputs.
#![allow(dead_code)]

use crate::types::xml::chvalid;

/// `xsl:decimal-format` (`xsltDecimalFormat`). `Default` gives `xsltNewDecimalFormat`'s values.
/// libxslt stores attribute values as given, so any of these may be empty or longer than one
/// character.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DecimalFormat {
    pub decimal_point: String,
    pub grouping: String,
    pub infinity: String,
    pub minus_sign: String,
    pub nan: String,
    pub percent: String,
    pub permille: String,
    pub zero_digit: String,
    pub digit: String,
    pub pattern_separator: String,
}

impl Default for DecimalFormat {
    fn default() -> Self {
        DecimalFormat {
            decimal_point: ".".into(),
            grouping: ",".into(),
            infinity: "Infinity".into(),
            minus_sign: "-".into(),
            nan: "NaN".into(),
            percent: "%".into(),
            permille: "\u{2030}".into(),
            zero_digit: "0".into(),
            digit: "#".into(),
            pattern_separator: ";".into(),
        }
    }
}

/// Byte at `i`, or 0 past the end (C strings are NUL-terminated; XML text has no NUL).
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// `xmlUTF8Strsize(s + i, 1)`: byte length of the character at `i`, 0 at the end.
fn char_size(s: &[u8], i: usize) -> usize {
    let mut ch = at(s, i) as u32;
    if ch == 0 {
        return 0;
    }
    let mut n = 1;
    if ch & 0x80 != 0 {
        loop {
            ch <<= 1;
            if ch & 0x80 == 0 || at(s, i + n) == 0 {
                break;
            }
            n += 1;
        }
    }
    n
}

/// `xmlStrncmp(a, b, len) == 0` on NUL-terminated views.
fn strn_eq(a: &[u8], b: &[u8], len: usize) -> bool {
    for i in 0..len {
        let (x, y) = (at(a, i), at(b, i));
        if x != y {
            return false;
        }
        if y == 0 {
            return true;
        }
    }
    true
}

/// `xsltUTF8Charcmp(s + i, sym) == 0`: the character at `i` equals the first character of `sym`.
fn char_is(s: &[u8], i: usize, sym: &str) -> bool {
    let len = char_size(s, i);
    len >= 1 && strn_eq(&s[i..], sym.as_bytes(), len)
}

/// `xsltGetUTF8CharZ` / `xsltGetUTF8Char` on a valid string: (code point, byte length);
/// (0, 0) at the end.
fn get_char(s: &[u8], i: usize) -> (i32, usize) {
    match std::str::from_utf8(&s[i.min(s.len())..]).ok().and_then(|t| t.chars().next()) {
        Some(c) => (c as i32, c.len_utf8()),
        None => (0, 0),
    }
}

/// `xsltCopyCharMultiByte`.
fn encode_char(val: i32) -> Vec<u8> {
    if val < 0 {
        return Vec::new();
    }
    let v = val as u32;
    if v < 0x80 {
        return vec![v as u8];
    }
    let (mut out, mut bits): (Vec<u8>, i32) = if v < 0x800 {
        (vec![((v >> 6) | 0xC0) as u8], 0)
    } else if v < 0x10000 {
        (vec![((v >> 12) | 0xE0) as u8], 6)
    } else if v < 0x110000 {
        (vec![((v >> 18) | 0xF0) as u8], 12)
    } else {
        return Vec::new();
    };
    while bits >= 0 {
        out.push((((v >> bits) & 0x3F) | 0x80) as u8);
        bits -= 6;
    }
    out
}

/// `xsltIsLetterDigit`.
fn is_letter_digit(c: i32) -> bool {
    c > 0 && {
        let c = c as u32;
        chvalid::is_base_char(c) || chvalid::is_ideographic(c) || chvalid::is_digit(c)
    }
}

/// `xsltIsDigitZero`: the zeros of XML 1.0's Digit class.
fn is_digit_zero(c: i32) -> bool {
    matches!(
        c,
        0x30 | 0x660
            | 0x6F0
            | 0x966
            | 0x9E6
            | 0xA66
            | 0xAE6
            | 0xB66
            | 0xC66
            | 0xCE6
            | 0xD66
            | 0xE50
            | 0xED0
            | 0xF20
    )
}

/// Grouping for decimal output: `(digitsPerGroup, groupingCharacter, groupingCharacterLen)`.
#[derive(Clone, Copy)]
struct Grouping {
    per_group: i32,
    ch: i32,
    len: usize,
}

const NO_GROUPING: Grouping = Grouping { per_group: 0, ch: 0, len: 0 };

/// `xsltNumberFormatDecimal`: digits built backwards in a 500-byte scratch buffer; overflow
/// keeps the low-order part (libxslt only prints an error).
fn format_decimal(out: &mut Vec<u8>, mut number: f64, digit_zero: i32, width: i32, g: Grouping) {
    const SIZE: usize = 500;
    let mut temp = [0u8; SIZE];
    let mut p = SIZE - 1;
    let mut i: i32 = 0;
    while p > 0 {
        if i >= width && number.abs() < 1.0 {
            break;
        }
        if i > 0 && g.ch != 0 && g.per_group > 0 && i % g.per_group == 0 {
            if p < g.len {
                break;
            }
            p -= g.len;
            let enc = encode_char(g.ch);
            let n = enc.len().min(SIZE - p);
            temp[p..p + n].copy_from_slice(&enc[..n]);
        }
        let val = digit_zero.wrapping_add((number % 10.0) as i32);
        if val < 0x80 {
            if p == 0 {
                break;
            }
            p -= 1;
            temp[p] = val as u8;
        } else {
            let enc = encode_char(val);
            if p < enc.len() {
                break;
            }
            p -= enc.len();
            temp[p..p + enc.len()].copy_from_slice(&enc);
        }
        number /= 10.0;
        i += 1;
    }
    // xmlBufferCat stops at the first NUL (a zero digit when zero-digit is "").
    let s = &temp[p..SIZE - 1];
    let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
    out.extend_from_slice(&s[..end]);
}

/// `xsltNumberFormatAlpha`: bijective base 26, at most 64 letters; below 1 falls back to "1".
fn format_alpha(out: &mut Vec<u8>, mut number: f64, upper: bool, g: Grouping) {
    if number < 1.0 {
        format_decimal(out, number, '0' as i32, 1, g);
        return;
    }
    let list = if upper { b"ABCDEFGHIJKLMNOPQRSTUVWXYZ" } else { b"abcdefghijklmnopqrstuvwxyz" };
    let mut rev = Vec::new();
    for _ in 1..65 {
        number -= 1.0;
        rev.push(list[(number % 26.0) as i32 as usize]);
        number /= 26.0;
        if number < 1.0 {
            break;
        }
    }
    out.extend(rev.iter().rev());
}

/// `xsltNumberFormatRoman`: 1..=5000, otherwise the "1" format.
fn format_roman(out: &mut Vec<u8>, mut number: f64, upper: bool, g: Grouping) {
    if !(1.0..=5000.0).contains(&number) {
        format_decimal(out, number, '0' as i32, 1, g);
        return;
    }
    let mut put = |s: &str| {
        if upper {
            out.extend(s.bytes().map(|b| b.to_ascii_uppercase()))
        } else {
            out.extend_from_slice(s.as_bytes())
        }
    };
    let steps: [(f64, &str, bool); 13] = [
        (1000.0, "m", true),
        (900.0, "cm", false),
        (500.0, "d", true),
        (400.0, "cd", false),
        (100.0, "c", true),
        (90.0, "xc", false),
        (50.0, "l", true),
        (40.0, "xl", false),
        (10.0, "x", true),
        (9.0, "ix", false),
        (5.0, "v", true),
        (4.0, "iv", false),
        (1.0, "i", true),
    ];
    for (v, s, repeat) in steps {
        if repeat {
            while number >= v {
                put(s);
                number -= v;
            }
        } else if number >= v {
            put(s);
            number -= v;
        }
    }
}

/// One format token: `token` is 'A', 'a', 'I', 'i' or a Unicode digit zero.
struct Token {
    separator: Option<Vec<u8>>,
    token: i32,
    width: i32,
}

const MAX_TOKENS: usize = 1024;

struct Format {
    start: Option<Vec<u8>>,
    tokens: Vec<Token>,
    end: Option<Vec<u8>>,
}

/// `[SECRET]`.
fn parse_format(format: &[u8]) -> Format {
    let mut f = Format { start: None, tokens: Vec::new(), end: None };
    let mut ix = 0;
    loop {
        let (val, len) = get_char(format, ix);
        if is_letter_digit(val) || at(format, ix) == 0 {
            break;
        }
        ix += len;
    }
    if ix > 0 {
        f.start = Some(format[..ix].to_vec());
    }
    while f.tokens.len() < MAX_TOKENS {
        if at(format, ix) == 0 {
            break;
        }
        let separator = if f.tokens.is_empty() { None } else { f.end.take() };
        let (mut val, mut len) = get_char(format, ix);
        let (token, width);
        if is_digit_zero(val - 1) || is_digit_zero(val) {
            let mut w = 1;
            while is_digit_zero(val) {
                w += 1;
                ix += len;
                (val, len) = get_char(format, ix);
            }
            if is_digit_zero(val - 1) {
                token = val - 1;
                width = w;
                ix += len;
                (val, len) = get_char(format, ix);
            } else {
                token = '0' as i32;
                width = 1;
            }
        } else if matches!(val, 0x41 | 0x61 | 0x49 | 0x69) {
            token = val;
            width = 0;
            ix += len;
            (val, len) = get_char(format, ix);
        } else {
            token = '0' as i32;
            width = 1;
        }
        f.tokens.push(Token { separator, token, width });
        while is_letter_digit(val) {
            ix += len;
            (val, len) = get_char(format, ix);
        }
        let j = ix;
        while !is_letter_digit(val) && val != 0 {
            ix += len;
            (val, len) = get_char(format, ix);
        }
        if ix > j {
            f.end = Some(format[j..ix].to_vec());
        }
    }
    f
}

/// `sscanf(s, "%d", &n)` with glibc semantics; `None` when nothing converts.
fn scan_int(s: &str) -> Option<i32> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let neg = match b.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let digits = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    // strtol saturates at the long range, then the value is truncated to int.
    let mut v: i64 = 0;
    for &c in &b[i..i + digits] {
        let d = (c - b'0') as i64;
        v = match v.checked_mul(10).and_then(|v| if neg { v.checked_sub(d) } else { v.checked_add(d) }) {
            Some(v) => v,
            None if neg => i64::MIN,
            None => i64::MAX,
        };
    }
    Some(v as i32)
}

/// `xsl:number` output: `xsltNumberFormatInsertNumbers` with the tokens of `format` (the
/// evaluated `format` attribute; libxslt uses "" when it is absent, which formats as "1").
///
/// `numbers` are in output order: the outermost level first for `level="multiple"`, or the
/// single counted value, or the number from the `value` attribute (`number(expr)` as an f64,
/// unrounded). Each number is rounded with `floor(n + 0.5)`; negative results (including
/// -Infinity) become 0 (libxslt reports "xsl-number : negative value" and goes on), NaN prints
/// "NaN", +Infinity prints "Infinity", 'a'/'A' below 1 and 'i'/'I' outside 1..=5000 fall back
/// to the "1" format. libxslt never calls this with no numbers (an unmatched `level="single"`
/// or `"multiple"`, or a failed `value` expression, inserts nothing, not even the prefix), so
/// an empty slice gives "".
///
/// `grouping_separator` / `grouping_size` are the raw attributes: only the first character
/// of the separator is used, the size is read with `sscanf("%d")`, and grouping is on only
/// when both are present. `letter-value` has no effect in libxslt (a compile-time warning
/// only), so it is not a parameter.
pub(crate) fn format_numbers(
    numbers: &[f64],
    format: &str,
    grouping_separator: Option<&str>,
    grouping_size: Option<&str>,
) -> String {
    if numbers.is_empty() {
        return String::new();
    }
    let mut g = NO_GROUPING;
    if let Some(sep) = grouping_separator {
        let (c, len) = get_char(sep.as_bytes(), 0);
        if len > 0 {
            g.ch = c;
            g.len = len;
        }
    }
    match grouping_size {
        Some(size) => g.per_group = scan_int(size).unwrap_or(0),
        None => g.ch = 0,
    }

    let f = parse_format(format.as_bytes());
    let default_token = Token { separator: Some(b".".to_vec()), token: '0' as i32, width: 1 };
    let mut out = Vec::new();
    if let Some(s) = &f.start {
        out.extend_from_slice(s);
    }
    for (i, &n) in numbers.iter().enumerate() {
        let mut number = (n + 0.5).floor();
        if number < 0.0 {
            number = 0.0;
        }
        let token = f.tokens.get(i).or(f.tokens.last()).unwrap_or(&default_token);
        if i > 0 {
            out.extend_from_slice(token.separator.as_deref().unwrap_or(b"."));
        }
        if number == f64::INFINITY {
            out.extend_from_slice(b"Infinity");
        } else if number.is_nan() {
            out.extend_from_slice(b"NaN");
        } else {
            match token.token {
                0x41 => format_alpha(&mut out, number, true, g),
                0x61 => format_alpha(&mut out, number, false, g),
                0x49 => format_roman(&mut out, number, true, g),
                0x69 => format_roman(&mut out, number, false, g),
                t if is_digit_zero(t) => format_decimal(&mut out, number, t, token.width, g),
                _ => {}
            }
        }
    }
    if let Some(e) = &f.end {
        out.extend_from_slice(e);
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Default)]
struct Info {
    integer_hash: i32,
    integer_digits: i32,
    frac_digits: i32,
    frac_hash: i32,
    group: i32,
    multiplier: i32,
    add_decimal: bool,
    is_multiplier_set: bool,
}

/// `IS_SPECIAL`.
fn is_special(df: &DecimalFormat, s: &[u8], i: usize) -> bool {
    char_is(s, i, &df.zero_digit)
        || char_is(s, i, &df.digit)
        || char_is(s, i, &df.decimal_point)
        || char_is(s, i, &df.grouping)
        || char_is(s, i, &df.pattern_separator)
}

/// `xsltFormatNumberPreSuffix`: the byte length of the prefix/suffix at `*p` without its
/// quote characters, or `None` for an error.
fn pre_suffix(df: &DecimalFormat, s: &[u8], p: &mut usize, info: &mut Info) -> Option<usize> {
    let mut count = 0;
    loop {
        if at(s, *p) == 0 {
            return Some(count);
        }
        if at(s, *p) == b'\'' {
            *p += 1;
            if at(s, *p) == 0 {
                return None;
            }
        } else if is_special(df, s, *p) {
            return Some(count);
        } else if char_is(s, *p, &df.percent) {
            if info.is_multiplier_set {
                return None;
            }
            info.multiplier = 100;
            info.is_multiplier_set = true;
        } else if char_is(s, *p, &df.permille) {
            if info.is_multiplier_set {
                return None;
            }
            info.multiplier = 1000;
            info.is_multiplier_set = true;
        }
        let len = char_size(s, *p);
        if len < 1 {
            return None;
        }
        count += len;
        *p += len;
    }
}

/// `xmlUTF8Strloc(s, sym)` followed by `xmlUTF8Strpos(s, j + 1)`: the byte offset just after
/// the first occurrence of `sym`'s first character.
fn after_separator(s: &[u8], sym: &str) -> Option<usize> {
    let size = char_size(sym.as_bytes(), 0);
    let mut i = 0;
    while at(s, i) != 0 {
        if strn_eq(&s[i..], sym.as_bytes(), size) {
            return Some(i + char_size(s, i));
        }
        i += char_size(s, i);
    }
    None
}

/// The parsed pattern: prefix/suffix as (offset, byte length without quotes).
struct Pattern {
    info: Info,
    prefix: (usize, usize),
    suffix: (usize, usize),
    default_sign: bool,
}

/// The pattern-parsing half of `xsltFormatNumberConversion`; `None` where it sets
/// `found_error`.
fn parse_pattern(df: &DecimalFormat, s: &[u8], negative: bool) -> Option<Pattern> {
    let mut info = Info { group: -1, multiplier: 1, ..Info::default() };
    let mut p = 0;
    let prefix = (0, pre_suffix(df, s, &mut p, &mut info)?);

    let mut delayed = 0;
    let mut len = 0;
    let grouping = df.grouping.as_bytes();
    while at(s, p) != 0 && !char_is(s, p, &df.decimal_point) && !char_is(s, p, &df.pattern_separator)
    {
        if delayed != 0 {
            info.multiplier = delayed;
            info.is_multiplier_set = true;
            delayed = 0;
        }
        if char_is(s, p, &df.digit) {
            if info.integer_digits > 0 {
                return None;
            }
            info.integer_hash += 1;
            if info.group >= 0 {
                info.group += 1;
            }
        } else if char_is(s, p, &df.zero_digit) {
            info.integer_digits += 1;
            if info.group >= 0 {
                info.group += 1;
            }
        } else if !grouping.is_empty() && strn_eq(&s[p..], grouping, grouping.len()) {
            info.group = 0;
            p += grouping.len();
            continue;
        } else if char_is(s, p, &df.percent) {
            if info.is_multiplier_set {
                return None;
            }
            delayed = 100;
        } else if char_is(s, p, &df.permille) {
            if info.is_multiplier_set {
                return None;
            }
            delayed = 1000;
        } else {
            break;
        }
        len = char_size(s, p);
        if len < 1 {
            return None;
        }
        p += len;
    }

    if at(s, p) != 0 && char_is(s, p, &df.decimal_point) {
        info.add_decimal = true;
        len = char_size(s, p);
        if len < 1 {
            return None;
        }
        p += len;
    }

    while at(s, p) != 0 {
        if char_is(s, p, &df.zero_digit) {
            if info.frac_hash != 0 {
                return None;
            }
            info.frac_digits += 1;
        } else if char_is(s, p, &df.digit) {
            info.frac_hash += 1;
        } else if char_is(s, p, &df.percent) || char_is(s, p, &df.permille) {
            if info.is_multiplier_set {
                return None;
            }
            delayed = if char_is(s, p, &df.percent) { 100 } else { 1000 };
            len = char_size(s, p);
            if len < 1 {
                return None;
            }
            p += len;
            continue;
        } else if !char_is(s, p, &df.grouping) {
            break;
        }
        len = char_size(s, p);
        if len < 1 {
            return None;
        }
        p += len;
        if delayed != 0 {
            info.multiplier = delayed;
            delayed = 0;
            info.is_multiplier_set = true;
        }
    }

    if delayed != 0 {
        p -= len;
    }

    let suffix_start = p;
    let suffix_len = pre_suffix(df, s, &mut p, &mut info)?;
    if at(s, p) != 0 && !char_is(s, p, &df.pattern_separator) {
        return None;
    }
    let mut pat = Pattern { info, prefix, suffix: (suffix_start, suffix_len), default_sign: false };
    if !negative {
        return Some(pat);
    }

    let Some(mut p) = after_separator(s, &df.pattern_separator) else {
        pat.default_sign = true;
        return Some(pat);
    };
    let info = &mut pat.info;
    info.is_multiplier_set = false;
    let nprefix = (p, pre_suffix(df, s, &mut p, info)?);
    let mut delayed = false;
    while at(s, p) != 0 {
        if char_is(s, p, &df.percent) || char_is(s, p, &df.permille) {
            if info.is_multiplier_set {
                return None;
            }
            info.is_multiplier_set = true;
            delayed = true;
        } else if is_special(df, s, p) {
            delayed = false;
        } else {
            break;
        }
        len = char_size(s, p);
        if len < 1 {
            return None;
        }
        p += len;
    }
    if delayed {
        info.is_multiplier_set = false;
        p -= len;
    }
    let mut nsuffix = (p, 0);
    if at(s, p) != 0 {
        nsuffix.1 = pre_suffix(df, s, &mut p, info)?;
    }
    if at(s, p) != 0 {
        return None;
    }
    // Java's rule: a negative prefix/suffix equal to the positive one is discarded (and,
    // as libxslt leaves it, no minus sign is added either).
    let differs = |a: (usize, usize), b: (usize, usize)| {
        a.1 != b.1 || (a.1 > 0 && !strn_eq(&s[a.0..], &s[b.0..], b.1))
    };
    if differs(nprefix, pat.prefix) || differs(nsuffix, pat.suffix) {
        pat.prefix = nprefix;
        pat.suffix = nsuffix;
    }
    Some(pat)
}

/// Copies a prefix/suffix, dropping each quote character that escapes the next one.
fn put_affix(out: &mut Vec<u8>, s: &[u8], (mut p, length): (usize, usize)) {
    let mut j = 0;
    while j < length {
        if at(s, p) == b'\'' {
            p += 1;
        }
        let len = char_size(s, p);
        if len == 0 {
            break;
        }
        out.extend_from_slice(&s[p..p + len]);
        p += len;
        j += len;
    }
}

/// `format-number(number, pattern)` with decimal format `df`: `xsltFormatNumberConversion`
/// as `xsltFormatNumberFunction` calls it.
///
/// libxslt never fails here: an invalid pattern (or an empty one) only prints a message
/// ("error in format string '...', using default") and formats with the fallback pattern
/// `#0.0###` (minus sign from `df`, no prefix/suffix, no grouping), so this returns a plain
/// string. The only error `xsltFormatNumberConversion` can return is `XPATH_MEMORY_ERROR`.
pub(crate) fn format_number(df: &DecimalFormat, pattern: &str, number: f64) -> String {
    if number.is_nan() {
        return df.nan.clone();
    }
    let s = pattern.as_bytes();
    let pat = parse_pattern(df, s, number < 0.0).unwrap_or(Pattern {
        info: Info {
            integer_digits: 1,
            frac_digits: 1,
            frac_hash: 4,
            group: -1,
            multiplier: 1,
            add_decimal: true,
            ..Info::default()
        },
        prefix: (0, 0),
        suffix: (0, 0),
        default_sign: number < 0.0,
    });
    let mut info = pat.info;

    let mut number = number * info.multiplier as f64;
    if number.is_infinite() {
        let mut r = String::new();
        if number < 0.0 {
            r.push_str(&df.minus_sign);
        }
        r.push_str(&df.infinity);
        return r;
    }

    let mut out = Vec::new();
    if pat.default_sign {
        let m = df.minus_sign.as_bytes();
        out.extend_from_slice(&m[..char_size(m, 0)]);
    }
    put_affix(&mut out, s, pat.prefix);

    number = number.abs();
    let mut exp10 = info.frac_digits + info.frac_hash;
    if exp10 > 308 {
        if info.frac_digits > 308 {
            info.frac_digits = 308;
            info.frac_hash = 0;
        } else {
            info.frac_hash = 308 - info.frac_digits;
        }
        exp10 = 308;
    }
    let scale = 10f64.powf(exp10 as f64);
    number += 0.5 / scale;
    number -= number % (1.0 / scale);

    let zero = df.zero_digit.as_bytes();
    let zero0 = at(zero, 0) as i32;
    let g = if df.grouping.is_empty() {
        Grouping { per_group: info.group, ch: ',' as i32, len: 1 }
    } else {
        let (ch, len) = get_char(df.grouping.as_bytes(), 0);
        Grouping { per_group: info.group, ch, len }
    };
    format_decimal(&mut out, number.floor(), zero0, info.integer_digits, g);

    if info.integer_digits + info.integer_hash + info.frac_digits == 0 && info.frac_hash > 0 {
        info.frac_digits += 1;
        info.frac_hash -= 1;
    }
    if number.floor() == 0.0 && info.integer_digits + info.frac_digits == 0 {
        out.extend_from_slice(&zero[..char_size(zero, 0)]);
    }
    let dp = df.decimal_point.as_bytes();
    let dp = &dp[..char_size(dp, 0)];
    if info.frac_digits + info.frac_hash == 0 {
        if info.add_decimal {
            out.extend_from_slice(dp);
        }
    } else {
        number -= number.floor();
        if number != 0.0 || info.frac_digits != 0 {
            out.extend_from_slice(dp);
            number = (scale * number + 0.5).floor();
            let mut j = info.frac_hash;
            while j > 0 {
                if number % 10.0 >= 1.0 {
                    break;
                }
                number /= 10.0;
                j -= 1;
            }
            format_decimal(&mut out, number.floor(), zero0, info.frac_digits + j, NO_GROUPING);
        }
    }
    put_affix(&mut out, s, pat.suffix);
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// libxml2's `xmlXPathCompNumber` (plus unary minus and `1 div 0`-style specials), so the
    /// test inputs are the exact doubles the reference saw.
    fn xpath_num(src: &str) -> f64 {
        match src {
            "NaN" => return f64::NAN,
            "Inf" => return f64::INFINITY,
            "-Inf" => return f64::NEG_INFINITY,
            _ => {}
        }
        if let Some(rest) = src.strip_prefix('-') {
            return -xpath_num(rest);
        }
        let b = src.as_bytes();
        let mut i = 0;
        let mut ret = 0.0f64;
        while i < b.len() && b[i].is_ascii_digit() {
            ret = ret * 10.0 + (b[i] - b'0') as f64;
            i += 1;
        }
        if i < b.len() && b[i] == b'.' {
            i += 1;
            let mut frac = 0;
            let mut fraction = 0.0f64;
            while i < b.len() && b[i] == b'0' {
                frac += 1;
                i += 1;
            }
            let max = frac + 20;
            while i < b.len() && b[i].is_ascii_digit() && frac < max {
                fraction = fraction * 10.0 + (b[i] - b'0') as f64;
                frac += 1;
                i += 1;
            }
            fraction /= 10f64.powf(frac as f64);
            ret += fraction;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
        if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
            i += 1;
            let mut neg = false;
            if b.get(i) == Some(&b'-') {
                neg = true;
                i += 1;
            } else if b.get(i) == Some(&b'+') {
                i += 1;
            }
            let mut e = 0i32;
            while i < b.len() && b[i].is_ascii_digit() {
                if e < 1_000_000 {
                    e = e * 10 + (b[i] - b'0') as i32;
                }
                i += 1;
            }
            if neg {
                e = -e;
            }
            ret *= 10f64.powf(e as f64);
        }
        assert_eq!(i, b.len(), "bad test number {src}");
        ret
    }

    /// Builds a decimal format from `key=value` pairs separated by `\u{1}`.
    fn decimal_format(spec: &str) -> DecimalFormat {
        let mut df = DecimalFormat::default();
        for kv in spec.split('\u{1}').filter(|s| !s.is_empty()) {
            let (k, v) = kv.split_once('=').unwrap();
            let v = v.to_string();
            match k {
                "decimal-separator" => df.decimal_point = v,
                "grouping-separator" => df.grouping = v,
                "infinity" => df.infinity = v,
                "minus-sign" => df.minus_sign = v,
                "NaN" => df.nan = v,
                "percent" => df.percent = v,
                "per-mille" => df.permille = v,
                "zero-digit" => df.zero_digit = v,
                "digit" => df.digit = v,
                "pattern-separator" => df.pattern_separator = v,
                _ => panic!("unknown decimal-format attribute {k}"),
            }
        }
        df
    }

    /// `(decimal-format spec, number, pattern, reference output)` from PostgreSQL 18.6 +
    /// libxslt 1.1.43 `xslt_process`.
    const FORMAT_NUMBER: &[(&str, &str, &str, &str)] = &[
        // @@FORMAT_NUMBER@@
        ("", "0", "#,##0.00", "0.00"),
        ("", "1", "#,##0.00", "1.00"),
        ("", "-1", "#,##0.00", "-1.00"),
        ("", "1234.5", "#,##0.00", "1,234.50"),
        ("", "-1234.5", "#,##0.00", "-1,234.50"),
        ("", "0.125", "#,##0.00", "0.13"),
        ("", "2.5", "#,##0.00", "2.50"),
        ("", "-0.5", "#,##0.00", "-0.50"),
        ("", "1e15", "#,##0.00", "1,000,000,000,000,000.00"),
        ("", "0.0000001", "#,##0.00", "0.00"),
        ("", "NaN", "#,##0.00", "NaN"),
        ("", "Inf", "#,##0.00", "Infinity"),
        ("", "-Inf", "#,##0.00", "-Infinity"),
        ("", "1234567.891", "#,##0.00", "1,234,567.89"),
        ("", "0.005", "#,##0.00", "0.01"),
        ("", "0", "0", "0"),
        ("", "1", "0", "1"),
        ("", "-1", "0", "-1"),
        ("", "1234.5", "0", "1235"),
        ("", "-1234.5", "0", "-1235"),
        ("", "0.125", "0", "0"),
        ("", "2.5", "0", "3"),
        ("", "-0.5", "0", "-1"),
        ("", "1e15", "0", "1000000000000000"),
        ("", "0.0000001", "0", "0"),
        ("", "NaN", "0", "NaN"),
        ("", "Inf", "0", "Infinity"),
        ("", "-Inf", "0", "-Infinity"),
        ("", "1234567.891", "0", "1234568"),
        ("", "0.005", "0", "0"),
        ("", "0", "#", "0"),
        ("", "1", "#", "1"),
        ("", "-1", "#", "-1"),
        ("", "1234.5", "#", "1235"),
        ("", "-1234.5", "#", "-1235"),
        ("", "0.125", "#", "0"),
        ("", "2.5", "#", "3"),
        ("", "-0.5", "#", "-1"),
        ("", "1e15", "#", "1000000000000000"),
        ("", "0.0000001", "#", "0"),
        ("", "NaN", "#", "NaN"),
        ("", "Inf", "#", "Infinity"),
        ("", "-Inf", "#", "-Infinity"),
        ("", "1234567.891", "#", "1234568"),
        ("", "0.005", "#", "0"),
        ("", "0", "#.##", "0"),
        ("", "1", "#.##", "1"),
        ("", "-1", "#.##", "-1"),
        ("", "1234.5", "#.##", "1234.5"),
        ("", "-1234.5", "#.##", "-1234.5"),
        ("", "0.125", "#.##", "0.13"),
        ("", "2.5", "#.##", "2.5"),
        ("", "-0.5", "#.##", "-0.5"),
        ("", "1e15", "#.##", "1000000000000000"),
        ("", "0.0000001", "#.##", "0"),
        ("", "NaN", "#.##", "NaN"),
        ("", "Inf", "#.##", "Infinity"),
        ("", "-Inf", "#.##", "-Infinity"),
        ("", "1234567.891", "#.##", "1234567.89"),
        ("", "0.005", "#.##", "0.01"),
        ("", "0", "0.###", "0"),
        ("", "1", "0.###", "1"),
        ("", "-1", "0.###", "-1"),
        ("", "1234.5", "0.###", "1234.5"),
        ("", "-1234.5", "0.###", "-1234.5"),
        ("", "0.125", "0.###", "0.125"),
        ("", "2.5", "0.###", "2.5"),
        ("", "-0.5", "0.###", "-0.5"),
        ("", "1e15", "0.###", "1000000000000000"),
        ("", "0.0000001", "0.###", "0"),
        ("", "NaN", "0.###", "NaN"),
        ("", "Inf", "0.###", "Infinity"),
        ("", "-Inf", "0.###", "-Infinity"),
        ("", "1234567.891", "0.###", "1234567.891"),
        ("", "0.005", "0.###", "0.005"),
        ("", "0", "00000", "00000"),
        ("", "1", "00000", "00001"),
        ("", "-1", "00000", "-00001"),
        ("", "1234.5", "00000", "01235"),
        ("", "-1234.5", "00000", "-01235"),
        ("", "0.125", "00000", "00000"),
        ("", "2.5", "00000", "00003"),
        ("", "-0.5", "00000", "-00001"),
        ("", "1e15", "00000", "1000000000000000"),
        ("", "0.0000001", "00000", "00000"),
        ("", "NaN", "00000", "NaN"),
        ("", "Inf", "00000", "Infinity"),
        ("", "-Inf", "00000", "-Infinity"),
        ("", "1234567.891", "00000", "1234568"),
        ("", "0.005", "00000", "00000"),
        ("", "0", ".##", ".0"),
        ("", "1", ".##", "1.0"),
        ("", "-1", ".##", "-1.0"),
        ("", "1234.5", ".##", "1234.5"),
        ("", "-1234.5", ".##", "-1234.5"),
        ("", "0.125", ".##", ".13"),
        ("", "2.5", ".##", "2.5"),
        ("", "-0.5", ".##", "-.5"),
        ("", "1e15", ".##", "1000000000000000.0"),
        ("", "0.0000001", ".##", ".0"),
        ("", "NaN", ".##", "NaN"),
        ("", "Inf", ".##", "Infinity"),
        ("", "-Inf", ".##", "-Infinity"),
        ("", "1234567.891", ".##", "1234567.89"),
        ("", "0.005", ".##", ".01"),
        ("", "0", "#%", "0%"),
        ("", "1", "#%", "100%"),
        ("", "-1", "#%", "-100%"),
        ("", "1234.5", "#%", "123450%"),
        ("", "-1234.5", "#%", "-123450%"),
        ("", "0.125", "#%", "13%"),
        ("", "2.5", "#%", "250%"),
        ("", "-0.5", "#%", "-50%"),
        ("", "1e15", "#%", "100000000000000000%"),
        ("", "0.0000001", "#%", "0%"),
        ("", "NaN", "#%", "NaN"),
        ("", "Inf", "#%", "Infinity"),
        ("", "-Inf", "#%", "-Infinity"),
        ("", "1234567.891", "#%", "123456789%"),
        ("", "0.005", "#%", "1%"),
        ("", "0", "#;(#)", "0"),
        ("", "1", "#;(#)", "1"),
        ("", "-1", "#;(#)", "(1)"),
        ("", "1234.5", "#;(#)", "1235"),
        ("", "-1234.5", "#;(#)", "(1235)"),
        ("", "0.125", "#;(#)", "0"),
        ("", "2.5", "#;(#)", "3"),
        ("", "-0.5", "#;(#)", "(1)"),
        ("", "1e15", "#;(#)", "1000000000000000"),
        ("", "0.0000001", "#;(#)", "0"),
        ("", "NaN", "#;(#)", "NaN"),
        ("", "Inf", "#;(#)", "Infinity"),
        ("", "-Inf", "#;(#)", "-Infinity"),
        ("", "1234567.891", "#;(#)", "1234568"),
        ("", "0.005", "#;(#)", "0"),
        ("", "0", "0.00E0", "0.0"),
        ("", "1", "0.00E0", "1.0"),
        ("", "-1", "0.00E0", "-1.0"),
        ("", "1234.5", "0.00E0", "1234.5"),
        ("", "-1234.5", "0.00E0", "-1234.5"),
        ("", "0.125", "0.00E0", "0.125"),
        ("", "2.5", "0.00E0", "2.5"),
        ("", "-0.5", "0.00E0", "-0.5"),
        ("", "1e15", "0.00E0", "1000000000000000.0"),
        ("", "0.0000001", "0.00E0", "0.0"),
        ("", "NaN", "0.00E0", "NaN"),
        ("", "Inf", "0.00E0", "Infinity"),
        ("", "-Inf", "0.00E0", "-Infinity"),
        ("", "1234567.891", "0.00E0", "1234567.891"),
        ("", "0.005", "0.00E0", "0.005"),
        ("", "0.125", "0.00", "0.13"),
        ("", "0.375", "0.00", "0.37"),
        ("", "2.675", "0.00", "2.67"),
        ("", "1.005", "0.00", "1.00"),
        ("", "999.995", "#,##0.00", "999.99"),
        ("", "0.5", "#", "1"),
        ("", "1.5", "#", "2"),
        ("", "-0.5", "0", "-1"),
        ("", "-0.001", "#", "-0"),
        ("", "-0.001", "0.00", "-0.00"),
        ("", "1e15", "#,###.##", "1,000,000,000,000,000"),
        ("", "1e-7", "0.000000", "0.000000"),
        ("", "1e-7", "#.#######", "0.0000001"),
        ("", "1e-7", "0.0000000", "0.0000001"),
        ("", "1e20", "#", "100000000000000000000"),
        ("", "1e20", "#,###", "100,000,000,000,000,000,000"),
        ("", "123456789012345678", "#", "123456789012345706"),
        ("", "1e300", "#", "1000000000000000808202880644066688264042266026266448262686660886824482642480402642604824608020806846480802642482840004220882624002648422282404882802048204824486000440820482268208860020480222000642044446660880460482426820042064082406040426248002428044264824840642466020628608860000884488266822640406660"),
        ("", "1e300", "#,#", "8,2,6,2,6,8,6,6,6,0,8,8,6,8,2,4,4,8,2,6,4,2,4,8,0,4,0,2,6,4,2,6,0,4,8,2,4,6,0,8,0,2,0,8,0,6,8,4,6,4,8,0,8,0,2,6,4,2,4,8,2,8,4,0,0,0,4,2,2,0,8,8,2,6,2,4,0,0,2,6,4,8,4,2,2,2,8,2,4,0,4,8,8,2,8,0,2,0,4,8,2,0,4,8,2,4,4,8,6,0,0,0,4,4,0,8,2,0,4,8,2,2,6,8,2,0,8,8,6,0,0,2,0,4,8,0,2,2,2,0,0,0,6,4,2,0,4,4,4,4,6,6,6,0,8,8,0,4,6,0,4,8,2,4,2,6,8,2,0,0,4,2,0,6,4,0,8,2,4,0,6,0,4,0,4,2,6,2,4,8,0,0,2,4,2,8,0,4,4,2,6,4,8,2,4,8,4,0,6,4,2,4,6,6,0,2,0,6,2,8,6,0,8,8,6,0,0,0,0,8,8,4,4,8,8,2,6,6,8,2,2,6,4,0,4,0,6,6,6,0"),
        ("", "0.1", "0.00000000000000000000", "0.10000000000000000000"),
        ("", "0.1", "#.####################", "0.1"),
        ("", "1", "0.00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000", "1.00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"),
        ("", "0.25", "0%", "25%"),
        ("", "0.25", "%0", "%25"),
        ("", "0.25", "#.#%", "25%"),
        ("", "0.0025", "#.##\u{2030}", "2.5\u{2030}"),
        ("", "0.0025", "\u{2030}#", "\u{2030}3"),
        ("", "0.0025", "#\u{2030}", "3\u{2030}"),
        ("", "5", "##%%", "5.0"),
        ("", "5", "%#%", "5.0"),
        ("", "5", "%#\u{2030}", "5.0"),
        ("", "1234.5", "#0#", "1234.5"),
        ("", "1234.5", "0#", "1234.5"),
        ("", "1234.5", "#.0#0", "1234.5"),
        ("", "1234.5", "", "1235"),
        ("", "-1234.5", "", "-1235"),
        ("", "-1234.5", "#0#", "-1234.5"),
        ("", "1.5", "#.", "2."),
        ("", "1.5", ".", "2."),
        ("", "0", ".", "0."),
        ("", "0", "#.", "0."),
        ("", "0.5", ".#", ".5"),
        ("", "0.5", "#.#", "0.5"),
        ("", "7", "'#'#", "##7"),
        ("", "7", "#'%'", "7.0"),
        ("", "7", "''#", "'7"),
        ("", "7", "'", "7.0"),
        ("", "7", "#'", "7.0"),
        ("", "7", "abc#xyz", "abc7xyz"),
        ("", "-7", "abc#xyz", "-abc7xyz"),
        ("", "-7", "#;-#", "-7"),
        ("", "-7", "#;#", "7"),
        ("", "-7", "#;", "7"),
        ("", "7", ";#", "7"),
        ("", "-7", ";#", "7"),
        ("", "-7", "0.00;[0.00]", "[7.00]"),
        ("", "-7", "#;x#y", "x7y"),
        ("", "-7", "a#b;a#b", "a7b"),
        ("", "-7", "a#b;a#c", "a7c"),
        ("", "-7", "#;##%", "700%"),
        ("", "-0.07", "#%;(#%)", "(7%)"),
        ("", "-0.07", "#;(#)%", "(7)%"),
        ("", "-7", "#;(#);", "-7.0"),
        ("", "-7", "#;(#)x", "(7)x"),
        ("", "-7", "#;'-'#", "-#7"),
        ("", "7", "#,##0.00 \u{20ac}", "7.00 \u{20ac}"),
        ("", "-7", "\u{a4} #,##0.00", "-\u{a4} 7.00"),
        ("", "1234567", "#,##,###", "1,234,567"),
        ("", "1234567", ",###", "1,234,567"),
        ("", "1234567", "#,", "1234567"),
        ("", "1234567", "#,##0,", "1234567"),
        ("", "1234567", "0,0", "1,2,3,4,5,6,7"),
        ("", "1234567.5", "#,##0.0,0", "1,234,567.50"),
        ("", "12", "000,000", "000,012"),
        ("", "0.5", "#,##0.#", "0.5"),
        ("", "7", "#.#;", "7"),
        ("", "-Inf", "#;(#)", "-Infinity"),
        ("", "-Inf", "x#", "-Infinity"),
        ("", "Inf", "#%", "Infinity"),
        ("", "1e308", "#%", "Infinity"),
        ("", "-1e308", "#\u{2030}", "-Infinity"),
        ("", "-0", "#", "0"),
        ("", "3.14159", "#.00#", "3.142"),
        ("", "3.14159", "00.0000##", "03.14159"),
        ("", "99.5", "#", "100"),
        ("", "100.5", "0", "101"),
        ("", "0.05", "#.#", "0.1"),
        ("", "0.95", "#.#", "0.9"),
        ("", "0.96", "0.#", "1"),
        ("", "9.96", "#.#", "10"),
        ("", "0.0001", "#.###", "0"),
        ("", "0.0001", "#.##0", "0.0001"),
        ("", "1.1", "#.000#", "1.100"),
        ("", "7", "#\u{e9}", "7\u{e9}"),
        ("", "7", "\u{e9}#", "\u{e9}7"),
        ("", "-7", "#;\u{e9}#", "\u{e9}7"),
        ("decimal-separator=,\u{1}grouping-separator=.", "1234567.891", "#.##0,00", "1.234.567,89"),
        ("decimal-separator=,\u{1}grouping-separator=.", "-1234.5", "#.##0,###", "-1.234,5"),
        ("decimal-separator=,\u{1}grouping-separator=.", "0.5", ",##", ",5"),
        ("decimal-separator=,\u{1}grouping-separator=.", "1234.5", "#,##0.00", "1234,5"),
        ("zero-digit=\u{660}", "1234.5", "#,##\u{660}.\u{660}\u{660}", "\u{da},\u{db}\u{dc}\u{dd}.\u{de}\u{d9}"),
        ("zero-digit=\u{660}", "0.25", "#.##", "\u{660}.\u{db}\u{de}"),
        ("zero-digit=\u{660}", "7", "\u{660}\u{660}\u{660}", "\u{d9}\u{d9}\u{e0}"),
        ("zero-digit=a", "1234.5", "#,##a.aa", "b,cde.fa"),
        ("zero-digit=a", "0", "#", "a"),
        ("zero-digit=a", "0.5", ".a", ".f"),
        ("infinity=\u{221e}\u{1}NaN=nan!", "Inf", "#", "\u{221e}"),
        ("infinity=\u{221e}\u{1}NaN=nan!", "-Inf", "#", "-\u{221e}"),
        ("infinity=\u{221e}\u{1}NaN=nan!", "NaN", "#", "nan!"),
        ("infinity=\u{221e}\u{1}NaN=nan!", "-Inf", "#;(#)", "-\u{221e}"),
        ("minus-sign=~", "-5", "#", "~5"),
        ("minus-sign=~", "-Inf", "#", "~Infinity"),
        ("minus-sign=~", "-5", "#;-#", "-5"),
        ("minus-sign=~", "-5", "#0#", "~5.0"),
        ("minus-sign=--", "-5", "#", "-5"),
        ("minus-sign=--", "-Inf", "#", "--Infinity"),
        ("minus-sign=", "-5", "#", "5"),
        ("minus-sign=", "-Inf", "#", "Infinity"),
        ("percent=p\u{1}per-mille=m", "0.25", "#p", "25p"),
        ("percent=p\u{1}per-mille=m", "0.25", "#m", "250m"),
        ("percent=p\u{1}per-mille=m", "0.25", "#%", "0%"),
        ("percent=p\u{1}per-mille=m", "0.25", "p#", "p25"),
        ("digit=x\u{1}pattern-separator=|", "-1234.5", "x,xx0.0x|(x)", "(1,234.5)"),
        ("digit=x\u{1}pattern-separator=|", "1234.5", "#,##0", "1234.5"),
        ("digit=x\u{1}pattern-separator=|", "-5", "x;x|<x>", "-5.0"),
        ("grouping-separator= ", "1234567", "# ##0", "1 234 567"),
        ("grouping-separator= ", "1234567", "#,##0", "1234567.0"),
        ("grouping-separator=\u{b7}", "1234567", "#\u{b7}##0", "1\u{b7}234\u{b7}567"),
        ("grouping-separator=ab", "1234567", "#ab##0", "1a234a567"),
        ("grouping-separator=ab", "1234567", "#a##0", "1234567.0"),
        ("grouping-separator=ab", "-1234567", "#ab##0;(#)", "(1a234a567)"),
        ("decimal-separator=ab", "1.5", "#ab#", "1a5"),
        ("decimal-separator=ab", "1.5", "#a#", "1a5"),
        ("pattern-separator=\u{a7}", "-5", "#\u{a7}(#)", "(5)"),
        ("pattern-separator=\u{a7}", "-5", "#;(#)", "-5.0"),
        ("NaN=", "NaN", "#", ""),
        ("infinity=", "-Inf", "#", "-"),
        ("grouping-separator=", "1234567", "#,##0", "1234567.0"),
        ("grouping-separator=", "1234", "#0", "1234"),
    ];

    /// `(numbers, format, grouping-separator, grouping-size, reference output)`; numbers are
    /// `,`-separated `value` literals; `\u{0}` marks an absent attribute.
    const NUMBER: &[(&str, &str, &str, &str, &str)] = &[
        // @@NUMBER@@
        ("0", "1", "\u{0}", "\u{0}", "0"),
        ("1", "1", "\u{0}", "\u{0}", "1"),
        ("7", "1", "\u{0}", "\u{0}", "7"),
        ("-3", "1", "\u{0}", "\u{0}", "0"),
        ("0.4", "1", "\u{0}", "\u{0}", "0"),
        ("0.5", "1", "\u{0}", "\u{0}", "1"),
        ("1.5", "1", "\u{0}", "\u{0}", "2"),
        ("2.5", "1", "\u{0}", "\u{0}", "3"),
        ("-0.4", "1", "\u{0}", "\u{0}", "0"),
        ("-0.6", "1", "\u{0}", "\u{0}", "0"),
        ("NaN", "1", "\u{0}", "\u{0}", "NaN"),
        ("Inf", "1", "\u{0}", "\u{0}", "Infinity"),
        ("-Inf", "1", "\u{0}", "\u{0}", "0"),
        ("26", "1", "\u{0}", "\u{0}", "26"),
        ("27", "1", "\u{0}", "\u{0}", "27"),
        ("52", "1", "\u{0}", "\u{0}", "52"),
        ("53", "1", "\u{0}", "\u{0}", "53"),
        ("702", "1", "\u{0}", "\u{0}", "702"),
        ("703", "1", "\u{0}", "\u{0}", "703"),
        ("18278", "1", "\u{0}", "\u{0}", "18278"),
        ("18279", "1", "\u{0}", "\u{0}", "18279"),
        ("1e10", "1", "\u{0}", "\u{0}", "10000000000"),
        ("3999", "1", "\u{0}", "\u{0}", "3999"),
        ("4000", "1", "\u{0}", "\u{0}", "4000"),
        ("4999", "1", "\u{0}", "\u{0}", "4999"),
        ("5000", "1", "\u{0}", "\u{0}", "5000"),
        ("5000.4", "1", "\u{0}", "\u{0}", "5000"),
        ("5000.5", "1", "\u{0}", "\u{0}", "5001"),
        ("5001", "1", "\u{0}", "\u{0}", "5001"),
        ("1e20", "1", "\u{0}", "\u{0}", "100000000000000000000"),
        ("1e300", "1", "\u{0}", "\u{0}", "1000000000000000808202880644066688264042266026266448262686660886824482642480402642604824608020806846480802642482840004220882624002648422282404882802048204824486000440820482268208860020480222000642044446660880460482426820042064082406040426248002428044264824840642466020628608860000884488266822640406660"),
        ("1234567", "1", "\u{0}", "\u{0}", "1234567"),
        ("100", "1", "\u{0}", "\u{0}", "100"),
        ("0", "01", "\u{0}", "\u{0}", "00"),
        ("1", "01", "\u{0}", "\u{0}", "01"),
        ("7", "01", "\u{0}", "\u{0}", "07"),
        ("-3", "01", "\u{0}", "\u{0}", "00"),
        ("0.4", "01", "\u{0}", "\u{0}", "00"),
        ("0.5", "01", "\u{0}", "\u{0}", "01"),
        ("1.5", "01", "\u{0}", "\u{0}", "02"),
        ("2.5", "01", "\u{0}", "\u{0}", "03"),
        ("-0.4", "01", "\u{0}", "\u{0}", "00"),
        ("-0.6", "01", "\u{0}", "\u{0}", "00"),
        ("NaN", "01", "\u{0}", "\u{0}", "NaN"),
        ("Inf", "01", "\u{0}", "\u{0}", "Infinity"),
        ("-Inf", "01", "\u{0}", "\u{0}", "00"),
        ("26", "01", "\u{0}", "\u{0}", "26"),
        ("27", "01", "\u{0}", "\u{0}", "27"),
        ("52", "01", "\u{0}", "\u{0}", "52"),
        ("53", "01", "\u{0}", "\u{0}", "53"),
        ("702", "01", "\u{0}", "\u{0}", "702"),
        ("703", "01", "\u{0}", "\u{0}", "703"),
        ("18278", "01", "\u{0}", "\u{0}", "18278"),
        ("18279", "01", "\u{0}", "\u{0}", "18279"),
        ("1e10", "01", "\u{0}", "\u{0}", "10000000000"),
        ("3999", "01", "\u{0}", "\u{0}", "3999"),
        ("4000", "01", "\u{0}", "\u{0}", "4000"),
        ("4999", "01", "\u{0}", "\u{0}", "4999"),
        ("5000", "01", "\u{0}", "\u{0}", "5000"),
        ("5000.4", "01", "\u{0}", "\u{0}", "5000"),
        ("5000.5", "01", "\u{0}", "\u{0}", "5001"),
        ("5001", "01", "\u{0}", "\u{0}", "5001"),
        ("1e20", "01", "\u{0}", "\u{0}", "100000000000000000000"),
        ("1e300", "01", "\u{0}", "\u{0}", "1000000000000000808202880644066688264042266026266448262686660886824482642480402642604824608020806846480802642482840004220882624002648422282404882802048204824486000440820482268208860020480222000642044446660880460482426820042064082406040426248002428044264824840642466020628608860000884488266822640406660"),
        ("1234567", "01", "\u{0}", "\u{0}", "1234567"),
        ("100", "01", "\u{0}", "\u{0}", "100"),
        ("0", "001", "\u{0}", "\u{0}", "000"),
        ("1", "001", "\u{0}", "\u{0}", "001"),
        ("7", "001", "\u{0}", "\u{0}", "007"),
        ("-3", "001", "\u{0}", "\u{0}", "000"),
        ("0.4", "001", "\u{0}", "\u{0}", "000"),
        ("0.5", "001", "\u{0}", "\u{0}", "001"),
        ("1.5", "001", "\u{0}", "\u{0}", "002"),
        ("2.5", "001", "\u{0}", "\u{0}", "003"),
        ("-0.4", "001", "\u{0}", "\u{0}", "000"),
        ("-0.6", "001", "\u{0}", "\u{0}", "000"),
        ("NaN", "001", "\u{0}", "\u{0}", "NaN"),
        ("Inf", "001", "\u{0}", "\u{0}", "Infinity"),
        ("-Inf", "001", "\u{0}", "\u{0}", "000"),
        ("26", "001", "\u{0}", "\u{0}", "026"),
        ("27", "001", "\u{0}", "\u{0}", "027"),
        ("52", "001", "\u{0}", "\u{0}", "052"),
        ("53", "001", "\u{0}", "\u{0}", "053"),
        ("702", "001", "\u{0}", "\u{0}", "702"),
        ("703", "001", "\u{0}", "\u{0}", "703"),
        ("18278", "001", "\u{0}", "\u{0}", "18278"),
        ("18279", "001", "\u{0}", "\u{0}", "18279"),
        ("1e10", "001", "\u{0}", "\u{0}", "10000000000"),
        ("3999", "001", "\u{0}", "\u{0}", "3999"),
        ("4000", "001", "\u{0}", "\u{0}", "4000"),
        ("4999", "001", "\u{0}", "\u{0}", "4999"),
        ("5000", "001", "\u{0}", "\u{0}", "5000"),
        ("5000.4", "001", "\u{0}", "\u{0}", "5000"),
        ("5000.5", "001", "\u{0}", "\u{0}", "5001"),
        ("5001", "001", "\u{0}", "\u{0}", "5001"),
        ("1e20", "001", "\u{0}", "\u{0}", "100000000000000000000"),
        ("1e300", "001", "\u{0}", "\u{0}", "1000000000000000808202880644066688264042266026266448262686660886824482642480402642604824608020806846480802642482840004220882624002648422282404882802048204824486000440820482268208860020480222000642044446660880460482426820042064082406040426248002428044264824840642466020628608860000884488266822640406660"),
        ("1234567", "001", "\u{0}", "\u{0}", "1234567"),
        ("100", "001", "\u{0}", "\u{0}", "100"),
        ("0", "a", "\u{0}", "\u{0}", "0"),
        ("1", "a", "\u{0}", "\u{0}", "a"),
        ("7", "a", "\u{0}", "\u{0}", "g"),
        ("-3", "a", "\u{0}", "\u{0}", "0"),
        ("0.4", "a", "\u{0}", "\u{0}", "0"),
        ("0.5", "a", "\u{0}", "\u{0}", "a"),
        ("1.5", "a", "\u{0}", "\u{0}", "b"),
        ("2.5", "a", "\u{0}", "\u{0}", "c"),
        ("-0.4", "a", "\u{0}", "\u{0}", "0"),
        ("-0.6", "a", "\u{0}", "\u{0}", "0"),
        ("NaN", "a", "\u{0}", "\u{0}", "NaN"),
        ("Inf", "a", "\u{0}", "\u{0}", "Infinity"),
        ("-Inf", "a", "\u{0}", "\u{0}", "0"),
        ("26", "a", "\u{0}", "\u{0}", "z"),
        ("27", "a", "\u{0}", "\u{0}", "aa"),
        ("52", "a", "\u{0}", "\u{0}", "az"),
        ("53", "a", "\u{0}", "\u{0}", "ba"),
        ("702", "a", "\u{0}", "\u{0}", "zz"),
        ("703", "a", "\u{0}", "\u{0}", "aaa"),
        ("18278", "a", "\u{0}", "\u{0}", "zzz"),
        ("18279", "a", "\u{0}", "\u{0}", "aaaa"),
        ("1e10", "a", "\u{0}", "\u{0}", "afipyqjp"),
        ("3999", "a", "\u{0}", "\u{0}", "ewu"),
        ("4000", "a", "\u{0}", "\u{0}", "ewv"),
        ("4999", "a", "\u{0}", "\u{0}", "gjg"),
        ("5000", "a", "\u{0}", "\u{0}", "gjh"),
        ("5000.4", "a", "\u{0}", "\u{0}", "gjh"),
        ("5000.5", "a", "\u{0}", "\u{0}", "gji"),
        ("5001", "a", "\u{0}", "\u{0}", "gji"),
        ("1e20", "a", "\u{0}", "\u{0}", "angwjirsmasvagw"),
        ("1e300", "a", "\u{0}", "\u{0}", "miwiaguwssuiqeimiuggukkcuwkycwiwcqeemqmaumwamccmewamysgiqkigqsog"),
        ("1234567", "a", "\u{0}", "\u{0}", "brfgi"),
        ("100", "a", "\u{0}", "\u{0}", "cv"),
        ("0", "A", "\u{0}", "\u{0}", "0"),
        ("1", "A", "\u{0}", "\u{0}", "A"),
        ("7", "A", "\u{0}", "\u{0}", "G"),
        ("-3", "A", "\u{0}", "\u{0}", "0"),
        ("0.4", "A", "\u{0}", "\u{0}", "0"),
        ("0.5", "A", "\u{0}", "\u{0}", "A"),
        ("1.5", "A", "\u{0}", "\u{0}", "B"),
        ("2.5", "A", "\u{0}", "\u{0}", "C"),
        ("-0.4", "A", "\u{0}", "\u{0}", "0"),
        ("-0.6", "A", "\u{0}", "\u{0}", "0"),
        ("NaN", "A", "\u{0}", "\u{0}", "NaN"),
        ("Inf", "A", "\u{0}", "\u{0}", "Infinity"),
        ("-Inf", "A", "\u{0}", "\u{0}", "0"),
        ("26", "A", "\u{0}", "\u{0}", "Z"),
        ("27", "A", "\u{0}", "\u{0}", "AA"),
        ("52", "A", "\u{0}", "\u{0}", "AZ"),
        ("53", "A", "\u{0}", "\u{0}", "BA"),
        ("702", "A", "\u{0}", "\u{0}", "ZZ"),
        ("703", "A", "\u{0}", "\u{0}", "AAA"),
        ("18278", "A", "\u{0}", "\u{0}", "ZZZ"),
        ("18279", "A", "\u{0}", "\u{0}", "AAAA"),
        ("1e10", "A", "\u{0}", "\u{0}", "AFIPYQJP"),
        ("3999", "A", "\u{0}", "\u{0}", "EWU"),
        ("4000", "A", "\u{0}", "\u{0}", "EWV"),
        ("4999", "A", "\u{0}", "\u{0}", "GJG"),
        ("5000", "A", "\u{0}", "\u{0}", "GJH"),
        ("5000.4", "A", "\u{0}", "\u{0}", "GJH"),
        ("5000.5", "A", "\u{0}", "\u{0}", "GJI"),
        ("5001", "A", "\u{0}", "\u{0}", "GJI"),
        ("1e20", "A", "\u{0}", "\u{0}", "ANGWJIRSMASVAGW"),
        ("1e300", "A", "\u{0}", "\u{0}", "MIWIAGUWSSUIQEIMIUGGUKKCUWKYCWIWCQEEMQMAUMWAMCCMEWAMYSGIQKIGQSOG"),
        ("1234567", "A", "\u{0}", "\u{0}", "BRFGI"),
        ("100", "A", "\u{0}", "\u{0}", "CV"),
        ("0", "i", "\u{0}", "\u{0}", "0"),
        ("1", "i", "\u{0}", "\u{0}", "i"),
        ("7", "i", "\u{0}", "\u{0}", "vii"),
        ("-3", "i", "\u{0}", "\u{0}", "0"),
        ("0.4", "i", "\u{0}", "\u{0}", "0"),
        ("0.5", "i", "\u{0}", "\u{0}", "i"),
        ("1.5", "i", "\u{0}", "\u{0}", "ii"),
        ("2.5", "i", "\u{0}", "\u{0}", "iii"),
        ("-0.4", "i", "\u{0}", "\u{0}", "0"),
        ("-0.6", "i", "\u{0}", "\u{0}", "0"),
        ("NaN", "i", "\u{0}", "\u{0}", "NaN"),
        ("Inf", "i", "\u{0}", "\u{0}", "Infinity"),
        ("-Inf", "i", "\u{0}", "\u{0}", "0"),
        ("26", "i", "\u{0}", "\u{0}", "xxvi"),
        ("27", "i", "\u{0}", "\u{0}", "xxvii"),
        ("52", "i", "\u{0}", "\u{0}", "lii"),
        ("53", "i", "\u{0}", "\u{0}", "liii"),
        ("702", "i", "\u{0}", "\u{0}", "dccii"),
        ("703", "i", "\u{0}", "\u{0}", "dcciii"),
        ("18278", "i", "\u{0}", "\u{0}", "18278"),
        ("18279", "i", "\u{0}", "\u{0}", "18279"),
        ("1e10", "i", "\u{0}", "\u{0}", "10000000000"),
        ("3999", "i", "\u{0}", "\u{0}", "mmmcmxcix"),
        ("4000", "i", "\u{0}", "\u{0}", "mmmm"),
        ("4999", "i", "\u{0}", "\u{0}", "mmmmcmxcix"),
        ("5000", "i", "\u{0}", "\u{0}", "mmmmm"),
        ("5000.4", "i", "\u{0}", "\u{0}", "mmmmm"),
        ("5000.5", "i", "\u{0}", "\u{0}", "5001"),
        ("5001", "i", "\u{0}", "\u{0}", "5001"),
        ("1e20", "i", "\u{0}", "\u{0}", "100000000000000000000"),
        ("1e300", "i", "\u{0}", "\u{0}", "1000000000000000808202880644066688264042266026266448262686660886824482642480402642604824608020806846480802642482840004220882624002648422282404882802048204824486000440820482268208860020480222000642044446660880460482426820042064082406040426248002428044264824840642466020628608860000884488266822640406660"),
        ("1234567", "i", "\u{0}", "\u{0}", "1234567"),
        ("100", "i", "\u{0}", "\u{0}", "c"),
        ("0", "I", "\u{0}", "\u{0}", "0"),
        ("1", "I", "\u{0}", "\u{0}", "I"),
        ("7", "I", "\u{0}", "\u{0}", "VII"),
        ("-3", "I", "\u{0}", "\u{0}", "0"),
        ("0.4", "I", "\u{0}", "\u{0}", "0"),
        ("0.5", "I", "\u{0}", "\u{0}", "I"),
        ("1.5", "I", "\u{0}", "\u{0}", "II"),
        ("2.5", "I", "\u{0}", "\u{0}", "III"),
        ("-0.4", "I", "\u{0}", "\u{0}", "0"),
        ("-0.6", "I", "\u{0}", "\u{0}", "0"),
        ("NaN", "I", "\u{0}", "\u{0}", "NaN"),
        ("Inf", "I", "\u{0}", "\u{0}", "Infinity"),
        ("-Inf", "I", "\u{0}", "\u{0}", "0"),
        ("26", "I", "\u{0}", "\u{0}", "XXVI"),
        ("27", "I", "\u{0}", "\u{0}", "XXVII"),
        ("52", "I", "\u{0}", "\u{0}", "LII"),
        ("53", "I", "\u{0}", "\u{0}", "LIII"),
        ("702", "I", "\u{0}", "\u{0}", "DCCII"),
        ("703", "I", "\u{0}", "\u{0}", "DCCIII"),
        ("18278", "I", "\u{0}", "\u{0}", "18278"),
        ("18279", "I", "\u{0}", "\u{0}", "18279"),
        ("1e10", "I", "\u{0}", "\u{0}", "10000000000"),
        ("3999", "I", "\u{0}", "\u{0}", "MMMCMXCIX"),
        ("4000", "I", "\u{0}", "\u{0}", "MMMM"),
        ("4999", "I", "\u{0}", "\u{0}", "MMMMCMXCIX"),
        ("5000", "I", "\u{0}", "\u{0}", "MMMMM"),
        ("5000.4", "I", "\u{0}", "\u{0}", "MMMMM"),
        ("5000.5", "I", "\u{0}", "\u{0}", "5001"),
        ("5001", "I", "\u{0}", "\u{0}", "5001"),
        ("1e20", "I", "\u{0}", "\u{0}", "100000000000000000000"),
        ("1e300", "I", "\u{0}", "\u{0}", "1000000000000000808202880644066688264042266026266448262686660886824482642480402642604824608020806846480802642482840004220882624002648422282404882802048204824486000440820482268208860020480222000642044446660880460482426820042064082406040426248002428044264824840642466020628608860000884488266822640406660"),
        ("1234567", "I", "\u{0}", "\u{0}", "1234567"),
        ("100", "I", "\u{0}", "\u{0}", "C"),
        ("0", "x", "\u{0}", "\u{0}", "0"),
        ("1", "x", "\u{0}", "\u{0}", "1"),
        ("7", "x", "\u{0}", "\u{0}", "7"),
        ("-3", "x", "\u{0}", "\u{0}", "0"),
        ("0.4", "x", "\u{0}", "\u{0}", "0"),
        ("0.5", "x", "\u{0}", "\u{0}", "1"),
        ("1.5", "x", "\u{0}", "\u{0}", "2"),
        ("2.5", "x", "\u{0}", "\u{0}", "3"),
        ("-0.4", "x", "\u{0}", "\u{0}", "0"),
        ("-0.6", "x", "\u{0}", "\u{0}", "0"),
        ("NaN", "x", "\u{0}", "\u{0}", "NaN"),
        ("Inf", "x", "\u{0}", "\u{0}", "Infinity"),
        ("-Inf", "x", "\u{0}", "\u{0}", "0"),
        ("26", "x", "\u{0}", "\u{0}", "26"),
        ("27", "x", "\u{0}", "\u{0}", "27"),
        ("52", "x", "\u{0}", "\u{0}", "52"),
        ("53", "x", "\u{0}", "\u{0}", "53"),
        ("702", "x", "\u{0}", "\u{0}", "702"),
        ("703", "x", "\u{0}", "\u{0}", "703"),
        ("18278", "x", "\u{0}", "\u{0}", "18278"),
        ("18279", "x", "\u{0}", "\u{0}", "18279"),
        ("1e10", "x", "\u{0}", "\u{0}", "10000000000"),
        ("3999", "x", "\u{0}", "\u{0}", "3999"),
        ("4000", "x", "\u{0}", "\u{0}", "4000"),
        ("4999", "x", "\u{0}", "\u{0}", "4999"),
        ("5000", "x", "\u{0}", "\u{0}", "5000"),
        ("5000.4", "x", "\u{0}", "\u{0}", "5000"),
        ("5000.5", "x", "\u{0}", "\u{0}", "5001"),
        ("5001", "x", "\u{0}", "\u{0}", "5001"),
        ("1e20", "x", "\u{0}", "\u{0}", "100000000000000000000"),
        ("1e300", "x", "\u{0}", "\u{0}", "1000000000000000808202880644066688264042266026266448262686660886824482642480402642604824608020806846480802642482840004220882624002648422282404882802048204824486000440820482268208860020480222000642044446660880460482426820042064082406040426248002428044264824840642466020628608860000884488266822640406660"),
        ("1234567", "x", "\u{0}", "\u{0}", "1234567"),
        ("100", "x", "\u{0}", "\u{0}", "100"),
        ("0", "(1)", "\u{0}", "\u{0}", "(0)"),
        ("1", "(1)", "\u{0}", "\u{0}", "(1)"),
        ("7", "(1)", "\u{0}", "\u{0}", "(7)"),
        ("-3", "(1)", "\u{0}", "\u{0}", "(0)"),
        ("0.4", "(1)", "\u{0}", "\u{0}", "(0)"),
        ("0.5", "(1)", "\u{0}", "\u{0}", "(1)"),
        ("1.5", "(1)", "\u{0}", "\u{0}", "(2)"),
        ("2.5", "(1)", "\u{0}", "\u{0}", "(3)"),
        ("-0.4", "(1)", "\u{0}", "\u{0}", "(0)"),
        ("-0.6", "(1)", "\u{0}", "\u{0}", "(0)"),
        ("NaN", "(1)", "\u{0}", "\u{0}", "(NaN)"),
        ("Inf", "(1)", "\u{0}", "\u{0}", "(Infinity)"),
        ("-Inf", "(1)", "\u{0}", "\u{0}", "(0)"),
        ("26", "(1)", "\u{0}", "\u{0}", "(26)"),
        ("27", "(1)", "\u{0}", "\u{0}", "(27)"),
        ("52", "(1)", "\u{0}", "\u{0}", "(52)"),
        ("53", "(1)", "\u{0}", "\u{0}", "(53)"),
        ("702", "(1)", "\u{0}", "\u{0}", "(702)"),
        ("703", "(1)", "\u{0}", "\u{0}", "(703)"),
        ("18278", "(1)", "\u{0}", "\u{0}", "(18278)"),
        ("18279", "(1)", "\u{0}", "\u{0}", "(18279)"),
        ("1e10", "(1)", "\u{0}", "\u{0}", "(10000000000)"),
        ("3999", "(1)", "\u{0}", "\u{0}", "(3999)"),
        ("4000", "(1)", "\u{0}", "\u{0}", "(4000)"),
        ("4999", "(1)", "\u{0}", "\u{0}", "(4999)"),
        ("5000", "(1)", "\u{0}", "\u{0}", "(5000)"),
        ("5000.4", "(1)", "\u{0}", "\u{0}", "(5000)"),
        ("5000.5", "(1)", "\u{0}", "\u{0}", "(5001)"),
        ("5001", "(1)", "\u{0}", "\u{0}", "(5001)"),
        ("1e20", "(1)", "\u{0}", "\u{0}", "(100000000000000000000)"),
        ("1e300", "(1)", "\u{0}", "\u{0}", "(1000000000000000808202880644066688264042266026266448262686660886824482642480402642604824608020806846480802642482840004220882624002648422282404882802048204824486000440820482268208860020480222000642044446660880460482426820042064082406040426248002428044264824840642466020628608860000884488266822640406660)"),
        ("1234567", "(1)", "\u{0}", "\u{0}", "(1234567)"),
        ("100", "(1)", "\u{0}", "\u{0}", "(100)"),
        ("0", "", "\u{0}", "\u{0}", "0"),
        ("1", "", "\u{0}", "\u{0}", "1"),
        ("7", "", "\u{0}", "\u{0}", "7"),
        ("-3", "", "\u{0}", "\u{0}", "0"),
        ("0.4", "", "\u{0}", "\u{0}", "0"),
        ("0.5", "", "\u{0}", "\u{0}", "1"),
        ("1.5", "", "\u{0}", "\u{0}", "2"),
        ("2.5", "", "\u{0}", "\u{0}", "3"),
        ("-0.4", "", "\u{0}", "\u{0}", "0"),
        ("-0.6", "", "\u{0}", "\u{0}", "0"),
        ("NaN", "", "\u{0}", "\u{0}", "NaN"),
        ("Inf", "", "\u{0}", "\u{0}", "Infinity"),
        ("-Inf", "", "\u{0}", "\u{0}", "0"),
        ("26", "", "\u{0}", "\u{0}", "26"),
        ("27", "", "\u{0}", "\u{0}", "27"),
        ("52", "", "\u{0}", "\u{0}", "52"),
        ("53", "", "\u{0}", "\u{0}", "53"),
        ("702", "", "\u{0}", "\u{0}", "702"),
        ("703", "", "\u{0}", "\u{0}", "703"),
        ("18278", "", "\u{0}", "\u{0}", "18278"),
        ("18279", "", "\u{0}", "\u{0}", "18279"),
        ("1e10", "", "\u{0}", "\u{0}", "10000000000"),
        ("3999", "", "\u{0}", "\u{0}", "3999"),
        ("4000", "", "\u{0}", "\u{0}", "4000"),
        ("4999", "", "\u{0}", "\u{0}", "4999"),
        ("5000", "", "\u{0}", "\u{0}", "5000"),
        ("5000.4", "", "\u{0}", "\u{0}", "5000"),
        ("5000.5", "", "\u{0}", "\u{0}", "5001"),
        ("5001", "", "\u{0}", "\u{0}", "5001"),
        ("1e20", "", "\u{0}", "\u{0}", "100000000000000000000"),
        ("1e300", "", "\u{0}", "\u{0}", "1000000000000000808202880644066688264042266026266448262686660886824482642480402642604824608020806846480802642482840004220882624002648422282404882802048204824486000440820482268208860020480222000642044446660880460482426820042064082406040426248002428044264824840642466020628608860000884488266822640406660"),
        ("1234567", "", "\u{0}", "\u{0}", "1234567"),
        ("100", "", "\u{0}", "\u{0}", "100"),
        ("0", "Z", "\u{0}", "\u{0}", "0"),
        ("7", "Z", "\u{0}", "\u{0}", "7"),
        ("12", "Z", "\u{0}", "\u{0}", "12"),
        ("1234", "Z", "\u{0}", "\u{0}", "1234"),
        ("0", "\u{30a2}", "\u{0}", "\u{0}", "0"),
        ("7", "\u{30a2}", "\u{0}", "\u{0}", "7"),
        ("12", "\u{30a2}", "\u{0}", "\u{0}", "12"),
        ("1234", "\u{30a2}", "\u{0}", "\u{0}", "1234"),
        ("0", "\u{661}", "\u{0}", "\u{0}", "\u{660}"),
        ("7", "\u{661}", "\u{0}", "\u{0}", "\u{667}"),
        ("12", "\u{661}", "\u{0}", "\u{0}", "\u{661}\u{662}"),
        ("1234", "\u{661}", "\u{0}", "\u{0}", "\u{661}\u{662}\u{663}\u{664}"),
        ("0", "\u{660}\u{661}", "\u{0}", "\u{0}", "\u{660}\u{660}"),
        ("7", "\u{660}\u{661}", "\u{0}", "\u{0}", "\u{660}\u{667}"),
        ("12", "\u{660}\u{661}", "\u{0}", "\u{0}", "\u{661}\u{662}"),
        ("1234", "\u{660}\u{661}", "\u{0}", "\u{0}", "\u{661}\u{662}\u{663}\u{664}"),
        ("0", "\u{967}", "\u{0}", "\u{0}", "\u{966}"),
        ("7", "\u{967}", "\u{0}", "\u{0}", "\u{96d}"),
        ("12", "\u{967}", "\u{0}", "\u{0}", "\u{967}\u{968}"),
        ("1234", "\u{967}", "\u{0}", "\u{0}", "\u{967}\u{968}\u{969}\u{96a}"),
        ("0", "[a]", "\u{0}", "\u{0}", "[0]"),
        ("7", "[a]", "\u{0}", "\u{0}", "[g]"),
        ("12", "[a]", "\u{0}", "\u{0}", "[l]"),
        ("1234", "[a]", "\u{0}", "\u{0}", "[aul]"),
        ("0", "1.", "\u{0}", "\u{0}", "0."),
        ("7", "1.", "\u{0}", "\u{0}", "7."),
        ("12", "1.", "\u{0}", "\u{0}", "12."),
        ("1234", "1.", "\u{0}", "\u{0}", "1234."),
        ("0", "#1#", "\u{0}", "\u{0}", "#0#"),
        ("7", "#1#", "\u{0}", "\u{0}", "#7#"),
        ("12", "#1#", "\u{0}", "\u{0}", "#12#"),
        ("1234", "#1#", "\u{0}", "\u{0}", "#1234#"),
        ("0", ".", "\u{0}", "\u{0}", ".0"),
        ("7", ".", "\u{0}", "\u{0}", ".7"),
        ("12", ".", "\u{0}", "\u{0}", ".12"),
        ("1234", ".", "\u{0}", "\u{0}", ".1234"),
        ("0", "0", "\u{0}", "\u{0}", "0"),
        ("7", "0", "\u{0}", "\u{0}", "7"),
        ("12", "0", "\u{0}", "\u{0}", "12"),
        ("1234", "0", "\u{0}", "\u{0}", "1234"),
        ("0", "00", "\u{0}", "\u{0}", "0"),
        ("7", "00", "\u{0}", "\u{0}", "7"),
        ("12", "00", "\u{0}", "\u{0}", "12"),
        ("1234", "00", "\u{0}", "\u{0}", "1234"),
        ("0", "ab", "\u{0}", "\u{0}", "0"),
        ("7", "ab", "\u{0}", "\u{0}", "g"),
        ("12", "ab", "\u{0}", "\u{0}", "l"),
        ("1234", "ab", "\u{0}", "\u{0}", "aul"),
        ("0", "iv", "\u{0}", "\u{0}", "0"),
        ("7", "iv", "\u{0}", "\u{0}", "vii"),
        ("12", "iv", "\u{0}", "\u{0}", "xii"),
        ("1234", "iv", "\u{0}", "\u{0}", "mccxxxiv"),
        ("0", "\u{2160}", "\u{0}", "\u{0}", "\u{2160}0"),
        ("7", "\u{2160}", "\u{0}", "\u{0}", "\u{2160}7"),
        ("12", "\u{2160}", "\u{0}", "\u{0}", "\u{2160}12"),
        ("1234", "\u{2160}", "\u{0}", "\u{0}", "\u{2160}1234"),
        ("0", "a1", "\u{0}", "\u{0}", "0"),
        ("7", "a1", "\u{0}", "\u{0}", "g"),
        ("12", "a1", "\u{0}", "\u{0}", "l"),
        ("1234", "a1", "\u{0}", "\u{0}", "aul"),
        ("0", "1a", "\u{0}", "\u{0}", "0"),
        ("7", "1a", "\u{0}", "\u{0}", "7"),
        ("12", "1a", "\u{0}", "\u{0}", "12"),
        ("1234", "1a", "\u{0}", "\u{0}", "1234"),
        ("0", "\u{e9}1", "\u{0}", "\u{0}", "0"),
        ("7", "\u{e9}1", "\u{0}", "\u{0}", "7"),
        ("12", "\u{e9}1", "\u{0}", "\u{0}", "12"),
        ("1234", "\u{e9}1", "\u{0}", "\u{0}", "1234"),
        ("0", "-", "\u{0}", "\u{0}", "-0"),
        ("7", "-", "\u{0}", "\u{0}", "-7"),
        ("12", "-", "\u{0}", "\u{0}", "-12"),
        ("1234", "-", "\u{0}", "\u{0}", "-1234"),
        ("0", "--", "\u{0}", "\u{0}", "--0"),
        ("7", "--", "\u{0}", "\u{0}", "--7"),
        ("12", "--", "\u{0}", "\u{0}", "--12"),
        ("1234", "--", "\u{0}", "\u{0}", "--1234"),
        ("0", " 1 ", "\u{0}", "\u{0}", " 0 "),
        ("7", " 1 ", "\u{0}", "\u{0}", " 7 "),
        ("12", " 1 ", "\u{0}", "\u{0}", " 12 "),
        ("1234", " 1 ", "\u{0}", "\u{0}", " 1234 "),
        ("0", "\u{e51}", "\u{0}", "\u{0}", "\u{e50}"),
        ("7", "\u{e51}", "\u{0}", "\u{0}", "\u{e57}"),
        ("12", "\u{e51}", "\u{0}", "\u{0}", "\u{e51}\u{e52}"),
        ("1234", "\u{e51}", "\u{0}", "\u{0}", "\u{e51}\u{e52}\u{e53}\u{e54}"),
        ("0", "\u{ff11}", "\u{0}", "\u{0}", "\u{ff11}0"),
        ("7", "\u{ff11}", "\u{0}", "\u{0}", "\u{ff11}7"),
        ("12", "\u{ff11}", "\u{0}", "\u{0}", "\u{ff11}12"),
        ("1234", "\u{ff11}", "\u{0}", "\u{0}", "\u{ff11}1234"),
        ("0", "\u{1d7ce}", "\u{0}", "\u{0}", "\u{1d7ce}0"),
        ("7", "\u{1d7ce}", "\u{0}", "\u{0}", "\u{1d7ce}7"),
        ("12", "\u{1d7ce}", "\u{0}", "\u{0}", "\u{1d7ce}12"),
        ("1234", "\u{1d7ce}", "\u{0}", "\u{0}", "\u{1d7ce}1234"),
        ("0", "0001x", "\u{0}", "\u{0}", "0000"),
        ("7", "0001x", "\u{0}", "\u{0}", "0007"),
        ("12", "0001x", "\u{0}", "\u{0}", "0012"),
        ("1234", "0001x", "\u{0}", "\u{0}", "1234"),
        ("0", "\u{660}\u{660}", "\u{0}", "\u{0}", "0"),
        ("7", "\u{660}\u{660}", "\u{0}", "\u{0}", "7"),
        ("12", "\u{660}\u{660}", "\u{0}", "\u{0}", "12"),
        ("1234", "\u{660}\u{660}", "\u{0}", "\u{0}", "1234"),
        ("1234567", "1", ",", "3", "1,234,567"),
        ("1234567", "0001", ",", "3", "1,234,567"),
        ("1234567", "\u{661}", ",", "3", "\u{661},\u{662}\u{663}\u{664},\u{665}\u{666}\u{667}"),
        ("0", "a", ",", "3", "0"),
        ("1e6", "i", ",", "3", "1,000,000"),
        ("12", "00001", ",", "3", "00,012"),
        ("1234567", "1", " ", "2", "1 23 45 67"),
        ("1234567", "0001", " ", "2", "1 23 45 67"),
        ("1234567", "\u{661}", " ", "2", "\u{661} \u{662}\u{663} \u{664}\u{665} \u{666}\u{667}"),
        ("0", "a", " ", "2", "0"),
        ("1e6", "i", " ", "2", "1 00 00 00"),
        ("12", "00001", " ", "2", "0 00 12"),
        ("1234567", "1", "", "3", "1234567"),
        ("1234567", "0001", "", "3", "1234567"),
        ("1234567", "\u{661}", "", "3", "\u{661}\u{662}\u{663}\u{664}\u{665}\u{666}\u{667}"),
        ("0", "a", "", "3", "0"),
        ("1e6", "i", "", "3", "1000000"),
        ("12", "00001", "", "3", "00012"),
        ("1234567", "1", ",", "\u{0}", "1234567"),
        ("1234567", "0001", ",", "\u{0}", "1234567"),
        ("1234567", "\u{661}", ",", "\u{0}", "\u{661}\u{662}\u{663}\u{664}\u{665}\u{666}\u{667}"),
        ("0", "a", ",", "\u{0}", "0"),
        ("1e6", "i", ",", "\u{0}", "1000000"),
        ("12", "00001", ",", "\u{0}", "00012"),
        ("1234567", "1", "\u{0}", "3", "1234567"),
        ("1234567", "0001", "\u{0}", "3", "1234567"),
        ("1234567", "\u{661}", "\u{0}", "3", "\u{661}\u{662}\u{663}\u{664}\u{665}\u{666}\u{667}"),
        ("0", "a", "\u{0}", "3", "0"),
        ("1e6", "i", "\u{0}", "3", "1000000"),
        ("12", "00001", "\u{0}", "3", "00012"),
        ("1234567", "1", ",", "0", "1234567"),
        ("1234567", "0001", ",", "0", "1234567"),
        ("1234567", "\u{661}", ",", "0", "\u{661}\u{662}\u{663}\u{664}\u{665}\u{666}\u{667}"),
        ("0", "a", ",", "0", "0"),
        ("1e6", "i", ",", "0", "1000000"),
        ("12", "00001", ",", "0", "00012"),
        ("1234567", "1", ",", "-1", "1234567"),
        ("1234567", "0001", ",", "-1", "1234567"),
        ("1234567", "\u{661}", ",", "-1", "\u{661}\u{662}\u{663}\u{664}\u{665}\u{666}\u{667}"),
        ("0", "a", ",", "-1", "0"),
        ("1e6", "i", ",", "-1", "1000000"),
        ("12", "00001", ",", "-1", "00012"),
        ("1234567", "1", ",", "x", "1234567"),
        ("1234567", "0001", ",", "x", "1234567"),
        ("1234567", "\u{661}", ",", "x", "\u{661}\u{662}\u{663}\u{664}\u{665}\u{666}\u{667}"),
        ("0", "a", ",", "x", "0"),
        ("1e6", "i", ",", "x", "1000000"),
        ("12", "00001", ",", "x", "00012"),
        ("1234567", "1", ",", " 2", "1,23,45,67"),
        ("1234567", "0001", ",", " 2", "1,23,45,67"),
        ("1234567", "\u{661}", ",", " 2", "\u{661},\u{662}\u{663},\u{664}\u{665},\u{666}\u{667}"),
        ("0", "a", ",", " 2", "0"),
        ("1e6", "i", ",", " 2", "1,00,00,00"),
        ("12", "00001", ",", " 2", "0,00,12"),
        ("1234567", "1", "\u{b7}", "3", "1\u{b7}234\u{b7}567"),
        ("1234567", "0001", "\u{b7}", "3", "1\u{b7}234\u{b7}567"),
        ("1234567", "\u{661}", "\u{b7}", "3", "\u{661}\u{b7}\u{662}\u{663}\u{664}\u{b7}\u{665}\u{666}\u{667}"),
        ("0", "a", "\u{b7}", "3", "0"),
        ("1e6", "i", "\u{b7}", "3", "1\u{b7}000\u{b7}000"),
        ("12", "00001", "\u{b7}", "3", "00\u{b7}012"),
        ("1234567", "1", "ab", "1", "1a2a3a4a5a6a7"),
        ("1234567", "0001", "ab", "1", "1a2a3a4a5a6a7"),
        ("1234567", "\u{661}", "ab", "1", "\u{661}a\u{662}a\u{663}a\u{664}a\u{665}a\u{666}a\u{667}"),
        ("0", "a", "ab", "1", "0"),
        ("1e6", "i", "ab", "1", "1a0a0a0a0a0a0"),
        ("12", "00001", "ab", "1", "0a0a0a1a2"),
        ("1234567", "1", ",", "4294967297", "1,2,3,4,5,6,7"),
        ("1234567", "0001", ",", "4294967297", "1,2,3,4,5,6,7"),
        ("1234567", "\u{661}", ",", "4294967297", "\u{661},\u{662},\u{663},\u{664},\u{665},\u{666},\u{667}"),
        ("0", "a", ",", "4294967297", "0"),
        ("1e6", "i", ",", "4294967297", "1,0,0,0,0,0,0"),
        ("12", "00001", ",", "4294967297", "0,0,0,1,2"),
        ("1234567", "1", ",", "+2z", "1,23,45,67"),
        ("1234567", "0001", ",", "+2z", "1,23,45,67"),
        ("1234567", "\u{661}", ",", "+2z", "\u{661},\u{662}\u{663},\u{664}\u{665},\u{666}\u{667}"),
        ("0", "a", ",", "+2z", "0"),
        ("1e6", "i", ",", "+2z", "1,00,00,00"),
        ("12", "00001", ",", "+2z", "0,00,12"),
        ("1,2,3", "1.1.1", "\u{0}", "\u{0}", "1.2.3"),
        ("1,2,3", "1", "\u{0}", "\u{0}", "1.2.3"),
        ("1,2,3", "1.a.i", "\u{0}", "\u{0}", "1.b.iii"),
        ("1,2,3", "(1) ", "\u{0}", "\u{0}", "(1.2.3) "),
        ("1,2,3", "1-a)", "\u{0}", "\u{0}", "1-b-c)"),
        ("1,2,3", "A.1.a.i", "\u{0}", "\u{0}", "A.2.c"),
        ("1,2,3", "", "\u{0}", "\u{0}", "1.2.3"),
        ("1,2,3", "x", "\u{0}", "\u{0}", "1.2.3"),
        ("1,2,3", "01.1", "\u{0}", "\u{0}", "01.2.3"),
        ("1,2,3", "a", "\u{0}", "\u{0}", "a.b.c"),
        ("1,2,3", "1.", "\u{0}", "\u{0}", "1.2.3."),
        ("1,2,3", "[1]", "\u{0}", "\u{0}", "[1.2.3]"),
        ("1,2,3", "i.", "\u{0}", "\u{0}", "i.ii.iii."),
        ("1,2,3", "1..a", "\u{0}", "\u{0}", "1..b..c"),
        ("3,27,4,1", "1.A", "\u{0}", "\u{0}", "3.AA.D.A"),
        ("2,5", "1.\u{661}", "\u{0}", "\u{0}", "2.\u{665}"),
        ("12,1", "#1-", "\u{0}", "\u{0}", "#12.1-"),
    ];

    fn opt(s: &str) -> Option<&str> {
        (s != "\u{0}").then_some(s)
    }

    #[test]
    fn format_number_matches_libxslt() {
        let mut bad = Vec::new();
        for &(spec, n, pattern, want) in FORMAT_NUMBER {
            let got = format_number(&decimal_format(spec), pattern, xpath_num(n));
            if got != want {
                bad.push(format!("format-number({n}, {pattern:?}) [{spec:?}]: got {got:?}, want {want:?}"));
            }
        }
        assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.join("\n"));
    }

    #[test]
    fn xsl_number_matches_libxslt() {
        let mut bad = Vec::new();
        for &(nums, format, sep, size, want) in NUMBER {
            let numbers: Vec<f64> = nums.split(',').map(xpath_num).collect();
            let got = format_numbers(&numbers, format, opt(sep), opt(size));
            if got != want {
                bad.push(format!(
                    "number({nums}, {format:?}, {sep:?}, {size:?}): got {got:?}, want {want:?}"
                ));
            }
        }
        assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.join("\n"));
    }

    #[test]
    fn helpers() {
        assert_eq!(scan_int(" 3x"), Some(3));
        assert_eq!(scan_int("-2"), Some(-2));
        assert_eq!(scan_int("x"), None);
        assert_eq!(scan_int("4294967299"), Some(3));
        assert_eq!(scan_int("99999999999999999999"), Some(-1));
        assert_eq!(format_numbers(&[], "(1)", None, None), "");
    }
}
