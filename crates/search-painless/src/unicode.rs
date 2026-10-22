//! Unicode character data for `java.lang.Character`: general categories (`getType` and the
//! predicates Java defines through them), binary properties, digit values, title case and an
//! approximation of bidirectional classes. Tables come from `regex-syntax`'s Unicode data and
//! are built once.

use std::sync::OnceLock;

use crate::lib_lang::int;
use crate::lookup::Builder;
use crate::types::cls;
use crate::value::{ArrayData, JArray, Obj, Value};
use crate::vm::{Throw, Thrown, Vm, VR};

/// Java's general category constants (`Character.UPPERCASE_LETTER`, …) by property name.
const CATEGORIES: &[(&str, u8)] = &[
    ("Lu", 1),
    ("Ll", 2),
    ("Lt", 3),
    ("Lm", 4),
    ("Lo", 5),
    ("Mn", 6),
    ("Me", 7),
    ("Mc", 8),
    ("Nd", 9),
    ("Nl", 10),
    ("No", 11),
    ("Zs", 12),
    ("Zl", 13),
    ("Zp", 14),
    ("Cc", 15),
    ("Cf", 16),
    ("Co", 18),
    ("Cs", 19),
    ("Pd", 20),
    ("Ps", 21),
    ("Pe", 22),
    ("Pc", 23),
    ("Po", 24),
    ("Sm", 25),
    ("Sc", 26),
    ("Sk", 27),
    ("So", 28),
    ("Pi", 29),
    ("Pf", 30),
];

fn ranges(expr: &str) -> Vec<(u32, u32)> {
    let Ok(hir) = regex_syntax::ParserBuilder::new().build().parse(expr) else { return Vec::new() };
    match hir.kind() {
        regex_syntax::hir::HirKind::Class(regex_syntax::hir::Class::Unicode(c)) => c.ranges().iter().map(|r| (r.start() as u32, r.end() as u32)).collect(),
        // a property with a single code point (Zl, Zp) parses as a literal
        regex_syntax::hir::HirKind::Literal(l) => match std::str::from_utf8(&l.0).ok().and_then(|s| s.chars().next()) {
            Some(c) => vec![(c as u32, c as u32)],
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

struct Table(Vec<(u32, u32, u8)>);

impl Table {
    fn lookup(&self, cp: u32) -> Option<u8> {
        let v = &self.0;
        let i = v.partition_point(|r| r.1 < cp);
        v.get(i).filter(|r| r.0 <= cp).map(|r| r.2)
    }
}

fn gencat_table() -> &'static Table {
    static T: OnceLock<Table> = OnceLock::new();
    T.get_or_init(|| {
        let mut v: Vec<(u32, u32, u8)> = Vec::new();
        for (name, id) in CATEGORIES {
            for (a, b) in ranges(&format!(r"\p{{gc={name}}}")) {
                v.push((a, b, *id));
            }
        }
        // surrogates are not chars in regex-syntax's tables
        v.push((0xD800, 0xDFFF, 19));
        v.sort_by_key(|r| r.0);
        Table(v)
    })
}

fn prop_table(name: &'static str) -> &'static Table {
    static TABLES: OnceLock<std::sync::Mutex<Vec<(&'static str, &'static Table)>>> = OnceLock::new();
    let m = TABLES.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut g = m.lock().unwrap();
    if let Some((_, t)) = g.iter().find(|(n, _)| *n == name) {
        return t;
    }
    let t: &'static Table = Box::leak(Box::new(Table(ranges(&format!(r"\p{{{name}}}")).into_iter().map(|(a, b)| (a, b, 1)).collect())));
    g.push((name, t));
    t
}

fn has_prop(name: &'static str, cp: u32) -> bool {
    prop_table(name).lookup(cp).is_some()
}

/// `Character.getType(cp)`.
pub fn gencat(cp: i32) -> u8 {
    if !(0..=0x10FFFF).contains(&cp) {
        return 0;
    }
    gencat_table().lookup(cp as u32).unwrap_or(0)
}

fn is_letter(cp: i32) -> bool {
    matches!(gencat(cp), 1..=5)
}

/// `Character.digit(cp, radix)`.
pub fn digit(cp: i32, radix: i32) -> i32 {
    if !(2..=36).contains(&radix) {
        return -1;
    }
    let v = digit_value(cp);
    if v >= 0 && v < radix {
        v
    } else {
        -1
    }
}

fn digit_value(cp: i32) -> i32 {
    match cp {
        0x30..=0x39 => return cp - 0x30,
        0x41..=0x5A => return cp - 0x41 + 10,
        0x61..=0x7A => return cp - 0x61 + 10,
        0xFF21..=0xFF3A => return cp - 0xFF21 + 10,
        0xFF41..=0xFF5A => return cp - 0xFF41 + 10,
        _ => {}
    }
    if gencat(cp) == 9 {
        // decimal digits come in aligned runs of ten
        let t = gencat_table();
        let i = t.0.partition_point(|r| r.1 < cp as u32);
        if let Some(r) = t.0.get(i) {
            return ((cp as u32 - r.0) % 10) as i32;
        }
    }
    -1
}

/// `Character.getNumericValue(cp)`: decimal digits and Latin letters (10–35); other numeric
/// characters report -2 like Java does for values that are not non-negative integers.
pub fn numeric_value(cp: i32) -> i32 {
    let v = digit_value(cp);
    if v >= 0 {
        return v;
    }
    match gencat(cp) {
        10 | 11 => match cp {
            0x2160..=0x216B => cp - 0x2160 + 1,
            0x2170..=0x217B => cp - 0x2170 + 1,
            0x216C | 0x217C => 50,
            0x216D | 0x217D => 100,
            0x216E | 0x217E => 500,
            0x216F | 0x217F | 0x2180 => 1000,
            0x2181 => 5000,
            0x2182 => 10000,
            0x3007 => 0,
            0x3021..=0x3029 => cp - 0x3020,
            0x3038 => 10,
            0x3039 => 20,
            0x303A => 30,
            0x16EE..=0x16F0 => cp - 0x16EE + 17,
            0x2460..=0x2473 => cp - 0x2460 + 1,
            0x2474..=0x2487 => cp - 0x2474 + 1,
            0x2488..=0x249B => cp - 0x2488 + 1,
            0x00B2 | 0x00B3 => cp - 0x00B0,
            0x00B9 => 1,
            0x2070 => 0,
            0x2074..=0x2079 => cp - 0x2070,
            0x2080..=0x2089 => cp - 0x2080,
            _ => -2,
        },
        _ => -1,
    }
}

fn is_java_whitespace(cp: i32) -> bool {
    match cp {
        0x00A0 | 0x2007 | 0x202F => false,
        0x09..=0x0D | 0x1C..=0x1F => true,
        _ => matches!(gencat(cp), 12..=14),
    }
}

fn is_identifier_ignorable(cp: i32) -> bool {
    matches!(cp, 0..=8 | 0x0E..=0x1B | 0x7F..=0x9F) || gencat(cp) == 16
}

/// `Character.toTitleCase`: the digraphs have their own title-case forms.
pub fn title_case(cp: i32) -> i32 {
    match cp {
        0x01C4..=0x01C6 => 0x01C5,
        0x01C7..=0x01C9 => 0x01C8,
        0x01CA..=0x01CC => 0x01CB,
        0x01F1..=0x01F3 => 0x01F2,
        _ => {
            let Some(c) = char::from_u32(cp as u32) else { return cp };
            if gencat(cp) == 3 {
                return cp;
            }
            let mut up = c.to_uppercase();
            match (up.next(), up.next()) {
                (Some(u), None) => u as i32,
                _ => cp,
            }
        }
    }
}

/// `Character.getDirectionality`: exact for Latin-1, approximated from categories and scripts
/// elsewhere.
pub fn directionality(cp: i32) -> i8 {
    match cp {
        0x30..=0x39 | 0xB2 | 0xB3 | 0xB9 => return 3,
        0x2B | 0x2D => return 4,
        0x23..=0x25 | 0xA2..=0xA5 | 0xB0 | 0xB1 => return 5,
        0x2C | 0x2E | 0x2F | 0x3A | 0xA0 => return 7,
        0x09 | 0x0B | 0x1F => return 11,
        0x0A | 0x0D | 0x1C..=0x1E | 0x85 | 0x2029 => return 10,
        0x0C | 0x20 => return 12,
        0x00..=0x08 | 0x0E..=0x1B | 0x7F..=0x84 | 0x86..=0x9F | 0xAD => return 9,
        0x41..=0x5A | 0x61..=0x7A | 0xAA | 0xB5 | 0xBA | 0xC0..=0xD6 | 0xD8..=0xF6 | 0xF8..=0xFF => return 0,
        0x21 | 0x22 | 0x26..=0x2A | 0x3B..=0x40 | 0x5B..=0x60 | 0x7B..=0x7E | 0xA1 | 0xA6..=0xA9 | 0xAB | 0xAC | 0xAE | 0xAF | 0xB4 | 0xB6..=0xB8 | 0xBB..=0xBF | 0xD7 | 0xF7 => return 13,
        0x202A => return 14,
        0x202D => return 15,
        0x202B => return 16,
        0x202E => return 17,
        0x202C => return 18,
        0x0660..=0x0669 | 0x066B | 0x066C => return 6,
        0x2028 => return 12,
        0x2E2F => return 13,
        0x24B6..=0x24E9 => return 0,
        0x06F0..=0x06F9 => return 3,
        _ => {}
    }
    let cat = gencat(cp);
    if cat == 0 {
        return -1;
    }
    if (0x0590..=0x05FF).contains(&cp) || (0x07C0..=0x085F).contains(&cp) || (0xFB1D..=0xFB4F).contains(&cp) || (0x10800..=0x10FFF).contains(&cp) {
        return if matches!(cat, 6 | 7) { 8 } else { 1 };
    }
    if (0x0600..=0x07BF).contains(&cp) || (0x0860..=0x08FF).contains(&cp) || (0xFB50..=0xFDFF).contains(&cp) || (0xFE70..=0xFEFF).contains(&cp) {
        return if matches!(cat, 6 | 7) { 8 } else { 2 };
    }
    match cat {
        6 | 7 => 8,
        9 => 3,
        12 => 12,
        13 => 12,
        14 => 10,
        15 => 9,
        16 => 9,
        26 => 5,
        1..=5 | 8 | 10 | 18 | 19 => 0,
        _ => 13,
    }
}

// ------------------------------------------------------------------------------ natives

fn cp_arg(v: &Value) -> i32 {
    match v {
        Value::Char(c) => *c as i32,
        other => int(other),
    }
}

macro_rules! pred {
    ($name:ident, |$c:ident| $e:expr) => {
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            let $c = cp_arg(&a[0]);
            Ok(Value::Bool($e))
        }
    };
}

pred!(c_is_letter, |c| is_letter(c));
pred!(c_is_digit, |c| gencat(c) == 9);
pred!(c_is_letter_or_digit, |c| is_letter(c) || gencat(c) == 9);
pred!(c_is_alphabetic, |c| (0..=0x10FFFF).contains(&c) && has_prop("Alphabetic", c as u32));
pred!(c_is_ideographic, |c| (0..=0x10FFFF).contains(&c) && has_prop("Ideographic", c as u32));
pred!(c_is_upper, |c| (0..=0x10FFFF).contains(&c) && has_prop("Uppercase", c as u32));
pred!(c_is_lower, |c| (0..=0x10FFFF).contains(&c) && has_prop("Lowercase", c as u32));
pred!(c_is_title, |c| gencat(c) == 3);
pred!(c_is_space_char, |c| matches!(gencat(c), 12..=14));
pred!(c_is_whitespace, |c| is_java_whitespace(c));
pred!(c_is_defined, |c| gencat(c) != 0);
pred!(c_is_mirrored, |c| (0..=0x10FFFF).contains(&c) && has_prop("Bidi_Mirrored", c as u32));
pred!(c_is_id_ignorable, |c| is_identifier_ignorable(c));
pred!(c_is_java_id_start, |c| java_identifier_start(c));
pred!(c_is_java_id_part, |c| java_identifier_part(c));

/// `Character.isJavaIdentifierStart(int)`.
pub fn java_identifier_start(c: i32) -> bool {
    is_letter(c) || matches!(gencat(c), 10 | 23 | 26)
}

/// `Character.isJavaIdentifierPart(int)`.
pub fn java_identifier_part(c: i32) -> bool {
    is_letter(c) || matches!(gencat(c), 6 | 8 | 9 | 10 | 23 | 26) || is_identifier_ignorable(c)
}
pred!(c_is_unicode_id_start, |c| c == 0x2E2F || ((0..=0x10FFFF).contains(&c) && has_prop("ID_Start", c as u32)));
pred!(c_is_unicode_id_part, |c| c == 0x2E2F || is_identifier_ignorable(c) || ((0..=0x10FFFF).contains(&c) && has_prop("ID_Continue", c as u32)));

fn c_get_type(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(gencat(cp_arg(&a[0])) as i32))
}
fn c_digit(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(digit(cp_arg(&a[0]), int(&a[1]))))
}
fn c_numeric(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(numeric_value(cp_arg(&a[0]))))
}
fn c_directionality(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Byte(directionality(cp_arg(&a[0]))))
}
fn c_to_title(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = cp_arg(&a[0]);
    Ok(match &a[0] {
        Value::Char(_) => Value::Char(title_case(c) as u16),
        _ => Value::Int(title_case(c)),
    })
}

fn char_array(v: &Value) -> Result<Vec<u16>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Array(arr) => match &*arr.data.borrow() {
                ArrayData::Char(c) => Ok(c.clone()),
                _ => Err(crate::vm::cce("not a char[]")),
            },
            _ => Err(crate::vm::cce("not a char[]")),
        },
        _ => Err(crate::vm::npe("Cannot read the array length because \"a\" is null")),
    }
}

fn ioobe(msg: Option<String>) -> Thrown {
    Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, msg)
}

fn is_high(u: u16) -> bool {
    (0xD800..0xDC00).contains(&u)
}
fn is_low(u: u16) -> bool {
    (0xDC00..0xE000).contains(&u)
}
fn pair(h: u16, l: u16) -> i32 {
    0x10000 + (((h as i32) - 0xD800) << 10) + (l as i32 - 0xDC00)
}

fn c_code_point_at_arr(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = char_array(&a[0])?;
    let (i, limit) = (int(&a[1]), int(&a[2]));
    if i < 0 || i >= limit || limit < 0 || limit as usize > s.len() {
        return Err(ioobe(None).with_frames(&["java.base/java.lang.Character.codePointAt(Character.java:9843)"]));
    }
    let (i, limit) = (i as usize, limit as usize);
    let c = s[i];
    if is_high(c) && i + 1 < limit && is_low(s[i + 1]) {
        return Ok(Value::Int(pair(c, s[i + 1])));
    }
    Ok(Value::Int(c as i32))
}

fn c_code_point_before_arr(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = char_array(&a[0])?;
    let (i, start) = (int(&a[1]), int(&a[2]));
    if i <= start || start < 0 || i as usize > s.len() {
        return Err(ioobe(None));
    }
    let i = i as usize;
    let c = s[i - 1];
    if is_low(c) && i - 1 > start as usize && is_high(s[i - 2]) {
        return Ok(Value::Int(pair(s[i - 2], c)));
    }
    Ok(Value::Int(c as i32))
}

fn units(v: &Value) -> Result<Vec<u16>, Thrown> {
    Ok(crate::lib_lang::char_seq(v)?.encode_utf16().collect())
}

fn c_code_point_before_cs(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = units(&a[0])?;
    let i = int(&a[1]);
    if i < 1 || i as usize > s.len() {
        let mut t = Throw::new(cls::STRING_INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Index {} out of bounds for length {}", i - 1, s.len())));
        t.frames = [
            "jdk.internal.util.Preconditions$1.apply(Preconditions.java:55)",
            "jdk.internal.util.Preconditions$1.apply(Preconditions.java:52)",
            "jdk.internal.util.Preconditions$4.apply(Preconditions.java:213)",
            "jdk.internal.util.Preconditions$4.apply(Preconditions.java:210)",
            "jdk.internal.util.Preconditions.outOfBounds(Preconditions.java:98)",
            "jdk.internal.util.Preconditions.outOfBoundsCheckIndex(Preconditions.java:106)",
            "jdk.internal.util.Preconditions.checkIndex(Preconditions.java:302)",
            "java.lang.String.checkIndex(String.java:5279)",
            "java.lang.StringLatin1.charAt(StringLatin1.java:58)",
            "java.lang.String.charAt(String.java:1843)",
            "java.lang.Character.codePointBefore(Character.java:9882)",
        ]
        .iter()
        .map(|f| format!("java.base/{f}"))
        .collect();
        return Err(t);
    }
    let i = i as usize;
    let c = s[i - 1];
    if is_low(c) && i >= 2 && is_high(s[i - 2]) {
        return Ok(Value::Int(pair(s[i - 2], c)));
    }
    Ok(Value::Int(c as i32))
}

fn c_code_point_count_cs(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = units(&a[0])?;
    let (b, e) = (int(&a[1]), int(&a[2]));
    if b < 0 || e > s.len() as i32 || b > e {
        return Err(ioobe(None));
    }
    let mut n = 0;
    let mut i = b as usize;
    while i < e as usize {
        if is_high(s[i]) && i + 1 < e as usize && is_low(s[i + 1]) {
            i += 2;
        } else {
            i += 1;
        }
        n += 1;
    }
    Ok(Value::Int(n))
}

fn offset_by(s: &[u16], start: usize, end: usize, index: i32, offset: i32) -> Result<i32, Thrown> {
    let mut x = index;
    if offset >= 0 {
        for _ in 0..offset {
            if x as usize >= end {
                return Err(ioobe(None));
            }
            if is_high(s[x as usize]) && (x as usize) + 1 < end && is_low(s[x as usize + 1]) {
                x += 1;
            }
            x += 1;
        }
    } else {
        for _ in 0..-offset {
            if x as usize <= start {
                return Err(ioobe(None));
            }
            x -= 1;
            if is_low(s[x as usize]) && x as usize > start && is_high(s[x as usize - 1]) {
                x -= 1;
            }
        }
    }
    Ok(x)
}

fn c_offset_cs(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = units(&a[0])?;
    let (index, offset) = (int(&a[1]), int(&a[2]));
    if index < 0 || index as usize > s.len() {
        return Err(ioobe(None));
    }
    Ok(Value::Int(offset_by(&s, 0, s.len(), index, offset)?))
}

fn c_offset_arr(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = char_array(&a[0])?;
    let (start, count, index, offset) = (int(&a[1]), int(&a[2]), int(&a[3]), int(&a[4]));
    if count > s.len() as i32 - start || start < 0 || count < 0 || index < start || index > start + count {
        return Err(ioobe(None));
    }
    Ok(Value::Int(offset_by(&s, start as usize, (start + count) as usize, index, offset)?))
}

fn c_to_chars3(_: &mut Vm, a: &mut [Value]) -> VR {
    let cp = int(&a[0]);
    let idx = int(&a[2]);
    if !(0..=0x10FFFF).contains(&cp) {
        return Err(crate::vm::iae(format!("Not a valid Unicode code point: 0x{:X}", cp as u32)).with_frames(&["java.base/java.lang.Character.toChars(Character.java:10060)"]));
    }
    let Value::Obj(o) = &a[1] else { return Err(crate::vm::npe("dst")) };
    let Obj::Array(arr) = &**o else { return Err(crate::vm::cce("not a char[]")) };
    let mut data = arr.data.borrow_mut();
    let ArrayData::Char(c) = &mut *data else { return Err(crate::vm::cce("not a char[]")) };
    let mut buf = [0u16; 2];
    let enc: &[u16] = if cp >= 0x10000 {
        char::from_u32(cp as u32).map(|ch| &*ch.encode_utf16(&mut buf)).unwrap_or(&[])
    } else {
        buf[0] = cp as u16;
        &buf[..1]
    };
    if idx < 0 || idx as usize + enc.len() > c.len() {
        return Err(Throw::new(cls::ARRAY_INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Index {} out of bounds for length {}", idx as usize + enc.len() - 1, c.len()))));
    }
    for (k, u) in enc.iter().enumerate() {
        c[idx as usize + k] = *u;
    }
    Ok(Value::Int(enc.len() as i32))
}

pub fn register(b: &mut Builder) {
    use cls::*;
    macro_rules! byte_consts {
        ($($name:literal = $v:literal,)*) => {$(
            b.sf(CHARACTER, concat!("byte ", $name), |_, _| Ok(Value::Byte($v)));
        )*};
    }
    byte_consts! {
        "UNASSIGNED" = 0, "UPPERCASE_LETTER" = 1, "LOWERCASE_LETTER" = 2, "TITLECASE_LETTER" = 3, "MODIFIER_LETTER" = 4,
        "OTHER_LETTER" = 5, "NON_SPACING_MARK" = 6, "ENCLOSING_MARK" = 7, "COMBINING_SPACING_MARK" = 8,
        "DECIMAL_DIGIT_NUMBER" = 9, "LETTER_NUMBER" = 10, "OTHER_NUMBER" = 11, "SPACE_SEPARATOR" = 12,
        "LINE_SEPARATOR" = 13, "PARAGRAPH_SEPARATOR" = 14, "CONTROL" = 15, "FORMAT" = 16, "PRIVATE_USE" = 18,
        "SURROGATE" = 19, "DASH_PUNCTUATION" = 20, "START_PUNCTUATION" = 21, "END_PUNCTUATION" = 22,
        "CONNECTOR_PUNCTUATION" = 23, "OTHER_PUNCTUATION" = 24, "MATH_SYMBOL" = 25, "CURRENCY_SYMBOL" = 26,
        "MODIFIER_SYMBOL" = 27, "OTHER_SYMBOL" = 28, "INITIAL_QUOTE_PUNCTUATION" = 29, "FINAL_QUOTE_PUNCTUATION" = 30,
        "DIRECTIONALITY_UNDEFINED" = -1, "DIRECTIONALITY_LEFT_TO_RIGHT" = 0, "DIRECTIONALITY_RIGHT_TO_LEFT" = 1,
        "DIRECTIONALITY_RIGHT_TO_LEFT_ARABIC" = 2, "DIRECTIONALITY_EUROPEAN_NUMBER" = 3,
        "DIRECTIONALITY_EUROPEAN_NUMBER_SEPARATOR" = 4, "DIRECTIONALITY_EUROPEAN_NUMBER_TERMINATOR" = 5,
        "DIRECTIONALITY_ARABIC_NUMBER" = 6, "DIRECTIONALITY_COMMON_NUMBER_SEPARATOR" = 7,
        "DIRECTIONALITY_NONSPACING_MARK" = 8, "DIRECTIONALITY_BOUNDARY_NEUTRAL" = 9,
        "DIRECTIONALITY_PARAGRAPH_SEPARATOR" = 10, "DIRECTIONALITY_SEGMENT_SEPARATOR" = 11,
        "DIRECTIONALITY_WHITESPACE" = 12, "DIRECTIONALITY_OTHER_NEUTRALS" = 13,
        "DIRECTIONALITY_LEFT_TO_RIGHT_EMBEDDING" = 14, "DIRECTIONALITY_LEFT_TO_RIGHT_OVERRIDE" = 15,
        "DIRECTIONALITY_RIGHT_TO_LEFT_EMBEDDING" = 16, "DIRECTIONALITY_RIGHT_TO_LEFT_OVERRIDE" = 17,
        "DIRECTIONALITY_POP_DIRECTIONAL_FORMAT" = 18,
    }
    for (sig, f) in [
        ("boolean isLetter(int)", c_is_letter as crate::lookup::Native),
        ("boolean isDigit(int)", c_is_digit),
        ("boolean isLetterOrDigit(int)", c_is_letter_or_digit),
        ("boolean isAlphabetic(int)", c_is_alphabetic),
        ("boolean isIdeographic(int)", c_is_ideographic),
        ("boolean isUpperCase(int)", c_is_upper),
        ("boolean isLowerCase(int)", c_is_lower),
        ("boolean isTitleCase(int)", c_is_title),
        ("boolean isSpaceChar(int)", c_is_space_char),
        ("boolean isWhitespace(int)", c_is_whitespace),
        ("boolean isDefined(int)", c_is_defined),
        ("boolean isMirrored(int)", c_is_mirrored),
        ("boolean isIdentifierIgnorable(int)", c_is_id_ignorable),
        ("boolean isJavaIdentifierStart(int)", c_is_java_id_start),
        ("boolean isJavaIdentifierPart(int)", c_is_java_id_part),
        ("boolean isUnicodeIdentifierStart(int)", c_is_unicode_id_start),
        ("boolean isUnicodeIdentifierPart(int)", c_is_unicode_id_part),
        ("int getType(int)", c_get_type),
        ("int digit(int,int)", c_digit),
        ("int getNumericValue(int)", c_numeric),
        ("byte getDirectionality(int)", c_directionality),
        ("char toTitleCase(char)", c_to_title),
        ("int codePointAt(char[],int,int)", c_code_point_at_arr),
        ("int codePointBefore(char[],int,int)", c_code_point_before_arr),
        ("int codePointBefore(CharSequence,int)", c_code_point_before_cs),
        ("int codePointCount(CharSequence,int,int)", c_code_point_count_cs),
        ("int offsetByCodePoints(char[],int,int,int,int)", c_offset_arr),
        ("int offsetByCodePoints(CharSequence,int,int)", c_offset_cs),
        ("int toChars(int,char[],int)", c_to_chars3),
    ] {
        b.s(CHARACTER, sig, f);
    }
    let _ = JArray::new;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories() {
        assert_eq!(gencat('A' as i32), 1);
        assert_eq!(gencat('a' as i32), 2);
        assert_eq!(gencat('ǅ' as i32), 3);
        assert_eq!(gencat('٣' as i32), 9);
        assert_eq!(gencat(' ' as i32), 12);
        assert_eq!(gencat(0x0378), 0);
        assert_eq!(gencat(0xE000), 18);
        assert_eq!(digit('٣' as i32, 10), 3);
        assert_eq!(digit('Ｚ' as i32, 36), 35);
        assert_eq!(title_case('ǆ' as i32), 'ǅ' as i32);
        assert!(has_prop("Uppercase", 0x2160));
    }
}
