//! isn (`contrib/isn/isn.c`): EAN13, UPC, ISBN, ISMN and ISSN numbers. A value is the
//! number shifted left one bit, its low bit set when it was entered with a wrong check digit
//! (weak mode); it is kept as 8 big-endian bytes and seen as an int8 by the btint8cmp /
//! int8eq / hashint8 functions the operators are made of.
//!
//! The string handling is a port of isn.c's, buffer for buffer: the hyphenation writes in
//! place into the buffers it reads, as the C does.

mod tables;

use tables::Table;

/// isn.c's `enum isn_type`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    Invalid,
    Any,
    Ean13,
    Isbn,
    Ismn,
    Issn,
    Upc,
}

fn name(k: Kind) -> &'static str {
    match k {
        Kind::Invalid | Kind::Any => "EAN13/UPC/ISxN",
        Kind::Ean13 => "EAN13",
        Kind::Isbn => "ISBN",
        Kind::Ismn => "ISMN",
        Kind::Issn => "ISSN",
        Kind::Upc => "UPC",
    }
}

const MAXEAN13LEN: usize = 18;

/// A C buffer: bytes past the written ones read as NUL.
struct Buf(Vec<u8>);

impl Buf {
    fn new(n: usize) -> Buf {
        Buf(vec![0; n])
    }
    fn at(&self, i: usize) -> u8 {
        self.0.get(i).copied().unwrap_or(0)
    }
    fn set(&mut self, i: usize, c: u8) {
        if i >= self.0.len() {
            self.0.resize(i + 1, 0);
        }
        self.0[i] = c;
    }
    /// The NUL-terminated string at `i`.
    fn cstr(&self, i: usize) -> String {
        let mut e = i;
        while self.at(e) != 0 {
            e += 1;
        }
        String::from_utf8_lossy(&self.0[i.min(self.0.len())..e.min(self.0.len())]).into_owned()
    }
    fn starts(&self, i: usize, p: &str) -> bool {
        p.bytes().enumerate().all(|(k, c)| self.at(i + k) == c)
    }
}

fn byte_at(s: &str, i: usize) -> u8 {
    s.as_bytes().get(i).copied().unwrap_or(0)
}

/// `dehyphenate`: the digits of `b[i..]` to `b[o..]`.
fn dehyphenate(b: &mut Buf, mut o: usize, mut i: usize) -> u32 {
    let mut ret = 0;
    while b.at(i) != 0 {
        if b.at(i).is_ascii_digit() {
            let c = b.at(i);
            b.set(o, c);
            o += 1;
            ret += 1;
        }
        i += 1;
    }
    b.set(o, 0);
    ret
}

/// `hyphenate`: `b[i..]` (digits) to `b[o..]` hyphenated by `table`'s range for it, then a
/// hyphen and the next character; the length written (0: no range has it).
fn hyphenate(b: &mut Buf, o: usize, i: usize, table: Option<&Table>) -> u32 {
    let mut ret = 0u32;
    let Some(t) = table else {
        let (mut o, mut i) = (o, i);
        while b.at(i) != 0 {
            let c = b.at(i);
            b.set(o, c);
            o += 1;
            i += 1;
            ret += 1;
        }
        b.set(o, 0);
        return ret + 1;
    };
    let d = b.at(i).wrapping_sub(b'0') as usize;
    if d > 9 {
        return 0;
    }
    let mut lower = t.index[d][0];
    let mut upper = lower.wrapping_add(t.index[d][1]);
    lower = lower.wrapping_sub(1);
    let mut step = upper.wrapping_sub(lower) / 2;
    if step == 0 {
        return 0;
    }
    let mut search = lower.wrapping_add(step);
    let bound = |s: u32, k: usize| -> &str { t.range.get(s as usize).map_or("", |r| r[k].as_str()) };
    let mut first = i;
    let (mut in1, mut in2) = (false, false);
    let (mut p1, mut p2) = (0usize, 0usize);
    loop {
        let (a1, a2) = (byte_at(bound(search, 0), p1), byte_at(bound(search, 1), p2));
        let f = b.at(first);
        if (in1 || f >= a1) && (in2 || f <= a2) {
            if f > a1 {
                in1 = true;
            }
            if f < a2 {
                in2 = true;
            }
            if in1 && in2 {
                break;
            }
            first += 1;
            p1 += 1;
            p2 += 1;
            if !(byte_at(bound(search, 0), p1) != 0 && byte_at(bound(search, 1), p2) != 0 && b.at(first) != 0) {
                break;
            }
            if !byte_at(bound(search, 0), p1).is_ascii_digit() {
                p1 += 1;
                p2 += 1;
            }
        } else {
            if f < a1 && !in1 {
                upper = search;
            } else {
                lower = search;
            }
            step = upper.wrapping_sub(lower) / 2;
            search = lower.wrapping_add(step);
            first = i;
            in1 = false;
            in2 = false;
            p1 = 0;
            p2 = 0;
        }
        if step == 0 {
            break;
        }
    }
    if step == 0 {
        return ret;
    }
    let (mut x, mut y) = (o, i);
    let pat = bound(search, 0).to_string();
    for c in pat.bytes() {
        if b.at(y) == 0 {
            break;
        }
        if c != b'-' {
            let v = b.at(y);
            b.set(x, v);
            y += 1;
        } else {
            b.set(x, b'-');
        }
        x += 1;
        ret += 1;
    }
    b.set(x, b'-');
    let v = b.at(y);
    b.set(x + 1, v);
    ret + 1
}

/// `weight_checkdig`: the ISBN / ISSN check digit (10: 'X') of the first `size - 1` digits.
fn weight_checkdig(b: &Buf, mut i: usize, mut size: u32) -> u32 {
    let mut w = 0u32;
    while b.at(i) != 0 && size > 1 {
        if b.at(i).is_ascii_digit() {
            w += size * (b.at(i) - b'0') as u32;
            size -= 1;
        }
        i += 1;
    }
    w %= 11;
    if w != 0 { 11 - w } else { 0 }
}

/// `checkdig`: the EAN13 / UPC / ISMN check digit of the first `size - 1` digits.
fn checkdig(b: &Buf, mut i: usize, mut size: u32) -> u32 {
    let (mut check, mut check3, mut pos) = (0u32, 0u32, 0u32);
    if b.at(i) == b'M' {
        check3 = 3;
        pos = 1;
    }
    while b.at(i) != 0 && size > 1 {
        if b.at(i).is_ascii_digit() {
            if pos % 2 == 1 {
                check3 += (b.at(i) - b'0') as u32;
            } else {
                check += (b.at(i) - b'0') as u32;
            }
            pos += 1;
            size -= 1;
        }
        i += 1;
    }
    check = (check + 3 * check3) % 10;
    if check != 0 { 10 - check } else { 0 }
}

/// `str2ean`: the digits at `i` as a number, shifted for the flag.
fn str2ean(b: &Buf, mut i: usize) -> u64 {
    let mut ean = 0u64;
    while b.at(i) != 0 {
        if b.at(i).is_ascii_digit() {
            ean = ean.wrapping_mul(10).wrapping_add((b.at(i) - b'0') as u64);
        }
        i += 1;
    }
    ean << 1
}

use crate::Result;
use crate::error::{Error, code};

fn text_err(msg: String) -> Error {
    Error::new(code::INVALID_TEXT_REPRESENTATION, msg)
}

/// The 13 digits of `ean >> 1`, zero-filled (ean2isn's buffer).
fn digits13(mut ean: u64) -> String {
    let mut d = Vec::new();
    let mut search = 0;
    loop {
        d.push(b'0' + (ean % 10) as u8);
        ean /= 10;
        // (`while (ean && search++ < 12)`.)
        if ean == 0 {
            break;
        }
        search += 1;
        if search > 12 {
            break;
        }
    }
    while d.len() < 13 {
        d.push(b'0');
    }
    d.reverse();
    String::from_utf8(d).unwrap()
}

/// `ean2isn`: the value as `accept` (its type checked by prefix).
fn ean2isn(ean: u64, accept: Kind) -> Result<u64> {
    let n = ean >> 1;
    if n > 9_999_999_999_999 {
        return Err(Error::new(
            code::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("value \"{n}\" is out of range for {} type", name(Kind::Invalid)),
        ));
    }
    let buf = digits13(n);
    let t = if buf.starts_with("978") {
        Kind::Isbn
    } else if buf.starts_with("977") {
        Kind::Issn
    } else if buf.starts_with("9790") {
        Kind::Ismn
    } else if buf.starts_with("979") {
        Kind::Isbn
    } else if buf.starts_with('0') {
        Kind::Upc
    } else {
        Kind::Ean13
    };
    if accept != Kind::Any && accept != Kind::Ean13 && accept != t {
        return Err(text_err(if t != Kind::Ean13 {
            format!("cannot cast EAN13({}) to {} for number: \"{buf}\"", name(t), name(accept))
        } else {
            format!("cannot cast {} to {} for number: \"{buf}\"", name(t), name(accept))
        }));
    }
    Ok(ean)
}

/// `ean2string`: the number hyphenated as its type is (`short`: in the type's short form).
fn ean2string(ean: u64, short: bool) -> Result<String> {
    let valid = if ean & 1 != 0 { b'!' } else { 0 };
    let mut n = ean >> 1;
    if n > 9_999_999_999_999 {
        return Err(Error::new(
            code::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("value \"{n}\" is out of range for {} type", name(Kind::Invalid)),
        ));
    }
    let mut b = Buf::new(MAXEAN13LEN + 1);
    let mut aux = MAXEAN13LEN;
    b.set(aux, 0);
    aux -= 1;
    b.set(aux, valid);
    let mut search = 0;
    loop {
        aux -= 1;
        b.set(aux, b'0' + (n % 10) as u8);
        n /= 10;
        if search == 0 {
            aux -= 1;
            b.set(aux, b'-');
        }
        if n == 0 {
            break;
        }
        search += 1;
        if search > 13 {
            break;
        }
    }
    while search < 13 {
        search += 1;
        aux -= 1;
        b.set(aux, b'0');
    }
    let mut kind = Kind::Invalid;
    let s = hyphenate(&mut b, 0, 3, Some(tables::ean13()));
    if s == 0 {
        hyphenate(&mut b, 0, 3, None);
    } else {
        let s = s as usize;
        let eq = strncmp_eq;
        let table: Option<&Table> = if eq(&b, "978-", s) {
            kind = Kind::Isbn;
            Some(tables::isbn())
        } else if eq(&b, "977-", s) {
            kind = Kind::Issn;
            Some(tables::issn())
        } else if eq(&b, "979-0", s + 1) {
            kind = Kind::Ismn;
            Some(tables::ismn())
        } else if eq(&b, "979-", s) {
            kind = Kind::Isbn;
            Some(tables::isbn_new())
        } else if b.at(0) == b'0' {
            kind = Kind::Upc;
            Some(tables::upc())
        } else {
            kind = Kind::Ean13;
            None
        };
        let d = s;
        if hyphenate(&mut b, d, d + 2, table) == 0 {
            hyphenate(&mut b, d, d + 2, None);
        }
    }
    if short {
        match kind {
            Kind::Isbn => ean2isbn(&mut b),
            Kind::Ismn => {
                hyphenate(&mut b, 0, 4, None);
                b.set(0, b'M');
            }
            Kind::Issn => {
                hyphenate(&mut b, 0, 4, None);
                let c = weight_checkdig(&b, 0, 8);
                b.set(8, if c == 10 { b'X' } else { b'0' + c as u8 });
                b.set(9, 0);
            }
            Kind::Upc => {
                dehyphenate(&mut b, 0, 1);
                b.set(12, 0);
            }
            _ => {}
        }
    }
    Ok(b.cstr(0))
}

/// `ean2ISBN`: a 978- number in its ten-digit form, with its own check digit.
fn ean2isbn(b: &mut Buf) {
    if b.starts(0, "978-") {
        hyphenate(b, 0, 4, None);
        let c = weight_checkdig(b, 0, 10);
        let mut aux = 0;
        while b.at(aux) != 0 {
            aux += 1;
        }
        loop {
            aux -= 1;
            if b.at(aux).is_ascii_digit() {
                break;
            }
        }
        b.set(aux, if c == 10 { b'X' } else { b'0' + c as u8 });
    }
}

/// `strncmp(p, b, n) == 0`.
fn strncmp_eq(b: &Buf, p: &str, n: usize) -> bool {
    for k in 0..n {
        let (x, y) = (b.at(k), byte_at(p, k));
        if x != y {
            return false;
        }
        if x == 0 {
            return true;
        }
    }
    true
}

/// `string2ean`: a number of type `accept` as written (`weak`: isn.weak is on).
fn string2ean(s: &str, accept: Kind, weak: bool) -> Result<u64> {
    let str_b = s.as_bytes();
    let at = |i: usize| str_b.get(i).copied().unwrap_or(0);
    let mut b = Buf(b"                \0".to_vec());
    let mut a1 = 3usize;
    let mut a2 = 0usize;
    let mut t = Kind::Invalid;
    let mut rcheck: u32 = u32::MAX;
    let mut length = 0u32;
    let (mut magic, mut valid) = (false, true);
    let invalid = || text_err(format!("invalid input syntax for {} number: \"{s}\"", name(accept)));
    let toobig = || {
        Error::new(code::NUMERIC_VALUE_OUT_OF_RANGE, format!("value \"{s}\" is out of range for {} type", name(accept)))
    };
    while at(a2) != 0 && length <= 13 {
        let c = at(a2);
        let last = at(a2 + 1) == b'!' || at(a2 + 1) == 0;
        let mut digit = c.is_ascii_digit();
        if c == b'?' && last {
            magic = true;
            digit = true;
        }
        let xdig = digit || c == b'X' || c == b'x';
        if length == 0 && (c == b'M' || c == b'm') {
            if t != Kind::Invalid {
                return Err(invalid());
            }
            t = Kind::Ismn;
            b.set(a1, b'M');
            a1 += 1;
            length += 1;
        } else if length == 7 && xdig && last {
            if t != Kind::Invalid {
                return Err(invalid());
            }
            t = Kind::Issn;
            b.set(a1, c.to_ascii_uppercase());
            a1 += 1;
            length += 1;
        } else if length == 9 && xdig && last {
            if t != Kind::Invalid && t != Kind::Ismn {
                return Err(invalid());
            }
            if t == Kind::Invalid {
                t = Kind::Isbn;
            }
            b.set(a1, c.to_ascii_uppercase());
            a1 += 1;
            length += 1;
        } else if length == 11 && digit && last {
            if t != Kind::Invalid {
                return Err(invalid());
            }
            t = Kind::Upc;
            b.set(a1, c);
            a1 += 1;
            length += 1;
        } else if c == b'-' || c == b' ' {
        } else if c == b'!' && at(a2 + 1) == 0 {
            if !magic {
                valid = false;
            }
            magic = true;
        } else if !digit {
            return Err(invalid());
        } else {
            b.set(a1, c);
            a1 += 1;
            length += 1;
            if length > 13 {
                return Err(toobig());
            }
        }
        a2 += 1;
    }
    b.set(a1, 0);
    let digit_of = |c: u8| (c as u32).wrapping_sub(b'0' as u32);
    let check: u32;
    match length {
        13 => {
            if t != Kind::Invalid {
                return Err(invalid());
            }
            t = Kind::Ean13;
            check = digit_of(b.at(15));
        }
        12 => {
            if t != Kind::Upc {
                return Err(invalid());
            }
            check = digit_of(b.at(14));
        }
        10 => {
            if t != Kind::Isbn && t != Kind::Ismn {
                return Err(invalid());
            }
            check = if b.at(12) == b'X' { 10 } else { digit_of(b.at(12)) };
        }
        8 => {
            if t != Kind::Invalid && t != Kind::Issn {
                return Err(invalid());
            }
            t = Kind::Issn;
            check = if b.at(10) == b'X' { 10 } else { digit_of(b.at(10)) };
        }
        _ => return Err(invalid()),
    }
    if t == Kind::Invalid {
        return Err(invalid());
    }
    let wrong = |t: Kind| text_err(format!("cannot cast {} to {} for number: \"{s}\"", name(t), name(accept)));
    if (accept == Kind::Ean13 && t != accept) || (accept != Kind::Any && t != Kind::Ean13 && t != accept) {
        return Err(wrong(t));
    }
    match t {
        Kind::Ean13 => {
            rcheck = checkdig(&b, 3, 13);
            valid = valid && (rcheck == check || magic);
            if b.at(3) == b'0' {
                t = Kind::Upc;
            } else if b.starts(3, "977") {
                t = Kind::Issn;
            } else if b.starts(3, "978") {
                t = Kind::Isbn;
            } else if b.starts(3, "9790") {
                t = Kind::Ismn;
            } else if b.starts(3, "979") {
                t = Kind::Isbn;
            }
            if accept != Kind::Ean13 && accept != Kind::Any && t != accept {
                return Err(wrong(t));
            }
        }
        Kind::Ismn => {
            for (k, c) in b"9790".iter().enumerate() {
                b.set(k, *c);
            }
            rcheck = checkdig(&b, 0, 13);
            valid = valid && (rcheck == check || magic);
        }
        Kind::Isbn => {
            for (k, c) in b"978".iter().enumerate() {
                b.set(k, *c);
            }
            rcheck = weight_checkdig(&b, 3, 10);
            valid = valid && (rcheck == check || magic);
        }
        Kind::Issn => {
            b.set(10, b'0');
            b.set(11, b'0');
            for (k, c) in b"977".iter().enumerate() {
                b.set(k, *c);
            }
            rcheck = weight_checkdig(&b, 3, 8);
            valid = valid && (rcheck == check || magic);
        }
        Kind::Upc => {
            b.set(2, b'0');
            rcheck = checkdig(&b, 2, 13);
            valid = valid && (rcheck == check || magic);
        }
        _ => {}
    }
    // Fix the check digit.
    let mut x = 0;
    while b.at(x) != 0 && b.at(x) <= b' ' {
        x += 1;
    }
    let c = checkdig(&b, x, 13);
    b.set(x + 12, b'0' + c as u8);
    b.set(x + 13, 0);
    if !valid && !magic {
        if weak {
            return Ok(str2ean(&b, x) | 1);
        }
        return Err(if rcheck == u32::MAX {
            text_err(format!("invalid {} number: \"{s}\"", name(accept)))
        } else {
            let ch = if rcheck == 10 { 'X' } else { (b'0' + rcheck as u8) as char };
            text_err(format!("invalid check digit for {} number: \"{s}\", should be {ch}", name(accept)))
        });
    }
    Ok(str2ean(&b, x) | if valid { 0 } else { 1 })
}

use super::native::Native;
use super::types::ExtType;
use crate::sql::func::FnCtx;
use crate::types::{DataType, Value};
use std::borrow::Cow;

/// The native types' kinds (`crate::ext::types`), in the order the script makes them.
pub(super) const EAN13: u16 = 11;
const ISBN13: u16 = 12;
const ISMN13: u16 = 13;
const ISSN13: u16 = 14;
const ISBN: u16 = 15;
const ISMN: u16 = 16;
const ISSN: u16 = 17;
const UPC: u16 = 18;

/// What a type accepts on input, and whether it prints in the short form (isn_out).
fn type_of(kind: u16) -> (Kind, bool) {
    match kind {
        EAN13 => (Kind::Ean13, false),
        ISBN13 => (Kind::Isbn, false),
        ISMN13 => (Kind::Ismn, false),
        ISSN13 => (Kind::Issn, false),
        ISBN => (Kind::Isbn, true),
        ISMN => (Kind::Ismn, true),
        ISSN => (Kind::Issn, true),
        _ => (Kind::Upc, true),
    }
}

fn val(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    let n = b.len().min(8);
    a[8 - n..].copy_from_slice(&b[..n]);
    u64::from_be_bytes(a)
}

fn ext(kind: u16, v: u64) -> Value {
    Value::Ext(kind, v.to_be_bytes().to_vec().into())
}

/// isn.weak, as the session has it.
fn weak() -> bool {
    crate::session::current::with_current(|ctx| {
        ctx.sess.settings.borrow().get_raw("isn.weak").is_some_and(|v| matches!(v.as_str(), "on" | "true" | "yes" | "1"))
    })
    .unwrap_or(false)
}

macro_rules! isn_type {
    ($c:ident, $kind:expr, $name:expr) => {
        pub(super) const $c: ExtType = ExtType {
            kind: $kind,
            extension: "isn",
            name: $name,
            input: |s| Ok(string2ean(s, type_of($kind).0, weak())?.to_be_bytes().to_vec()),
            output: |b, _| ean2string(val(b), type_of($kind).1).unwrap_or_default(),
            cmp: |a, b| val(a).cmp(&val(b)),
            canon: |a| Cow::Borrowed(a),
            key: |a, out| out.extend_from_slice(a),
            to_builtin: Some(|b| Value::Int8(val(b) as i64)),
            from_builtin: Some(|v| v.as_i64().map(|i| (i as u64).to_be_bytes().to_vec())),
            hash: true,
        };
    };
}

isn_type!(TYPE_EAN13, EAN13, "ean13");
isn_type!(TYPE_ISBN13, ISBN13, "isbn13");
isn_type!(TYPE_ISMN13, ISMN13, "ismn13");
isn_type!(TYPE_ISSN13, ISSN13, "issn13");
isn_type!(TYPE_ISBN, ISBN, "isbn");
isn_type!(TYPE_ISMN, ISMN, "ismn");
isn_type!(TYPE_ISSN, ISSN, "issn");
isn_type!(TYPE_UPC, UPC, "upc");

/// The value of an argument of one of the types (an int8 given as one too).
fn arg(v: &Value) -> u64 {
    match v {
        Value::Ext(_, b) => val(b),
        v => v.as_i64().unwrap_or(0) as u64,
    }
}

/// The kind of the call's result type (or of its first argument).
fn ret_kind(a: &[Value], c: &FnCtx) -> u16 {
    match c.ret {
        DataType::Base(b) if b.ext != 0 => b.ext,
        _ => match a.first() {
            Some(Value::Ext(k, _)) => *k,
            _ => EAN13,
        },
    }
}

fn input(a: &[Value], c: &FnCtx, accept: Kind) -> Result<Value> {
    let s = crate::sql::func::text_arg(&a[0]);
    Ok(ext(ret_kind(a, c), string2ean(s, accept, weak())?))
}

fn cast(a: &[Value], c: &FnCtx, accept: Kind) -> Result<Value> {
    Ok(ext(ret_kind(a, c), ean2isn(arg(&a[0]), accept)?))
}

/// `isn.weak` set as `accept_weak_input` sets it (set_config_option, session).
fn accept_weak_input(a: &[Value], c: &FnCtx) -> Result<Value> {
    let on = matches!(a[0], Value::Bool(true));
    c.exec.sess.settings.borrow_mut().set("isn.weak", Some(if on { "on" } else { "off" }), false, true)?;
    Ok(Value::Bool(weak_now(c)))
}

fn weak_now(c: &FnCtx) -> bool {
    c.exec.sess.settings.borrow().get_raw("isn.weak").is_some_and(|v| matches!(v.as_str(), "on" | "true" | "yes" | "1"))
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    Some(Native::scalar(match sym {
        "isn_out" => |a, _| Ok(Value::text(ean2string(arg(&a[0]), true)?)),
        "ean13_out" => |a, _| Ok(Value::text(ean2string(arg(&a[0]), false)?)),
        "ean13_in" => |a, c| input(a, c, Kind::Ean13),
        "isbn_in" => |a, c| input(a, c, Kind::Isbn),
        "ismn_in" => |a, c| input(a, c, Kind::Ismn),
        "issn_in" => |a, c| input(a, c, Kind::Issn),
        "upc_in" => |a, c| input(a, c, Kind::Upc),
        "isbn_cast_from_ean13" => |a, c| cast(a, c, Kind::Isbn),
        "ismn_cast_from_ean13" => |a, c| cast(a, c, Kind::Ismn),
        "issn_cast_from_ean13" => |a, c| cast(a, c, Kind::Issn),
        "upc_cast_from_ean13" => |a, c| cast(a, c, Kind::Upc),
        "is_valid" => |a, _| Ok(Value::Bool(arg(&a[0]) & 1 == 0)),
        "make_valid" => |a, c| Ok(ext(ret_kind(a, c), arg(&a[0]) & !1)),
        "accept_weak_input" => accept_weak_input,
        "weak_input_status" => |_, c| Ok(Value::Bool(weak_now(c))),
        _ => return None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isn_vectors() {
        let e = |s: &str, k: Kind| string2ean(s, k, false);
        assert_eq!(ean2string(e("9780123456786", Kind::Ean13).unwrap(), false).unwrap(), "978-0-12-345678-6");
        assert_eq!(ean2string(e("123456789X", Kind::Isbn).unwrap(), true).unwrap(), "1-234-56789-X");
        assert_eq!(ean2string(e("M-1234-5678-5", Kind::Ismn).unwrap(), true).unwrap(), "M-1234-5678-5");
        assert_eq!(ean2string(e("12345679", Kind::Issn).unwrap(), true).unwrap(), "1234-5679");
        assert!(e("1234567890", Kind::Isbn).is_err());
        assert_eq!(ean2string(e("0123456789012", Kind::Upc).unwrap(), true).unwrap(), "123456789012");
    }
}
