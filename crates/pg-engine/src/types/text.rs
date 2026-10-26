//! Text input/output for every type, byte-for-byte compatible with Postgres's output functions.

use super::array::{format_array, parse_array};
use super::datetime::{self, Clock, TimeZone};
use super::json::{parse_jsonb, validate_json};
use super::numeric::Numeric;
use super::value::Interval;
use super::{DataType, Value};
use crate::error::{Error, Result, code};
use crate::{bail, err};
use std::sync::Arc;

/// Name lookups for the reg* types (`'users'::regclass`, `oid::regtype`).
pub trait NameResolver: Send + Sync {
    fn rel_name(&self, oid: u32) -> Option<String>;
    fn rel_oid(&self, name: &str) -> Result<u32>;
    fn type_name(&self, oid: u32) -> Option<String>;
    fn type_oid(&self, name: &str) -> Result<u32>;
    fn proc_name(&self, oid: u32) -> Option<String>;
    fn proc_oid(&self, name: &str) -> Result<u32>;
    /// regprocedure input: `name(argtypes)`.
    fn procedure_oid(&self, name: &str) -> Result<u32> {
        self.proc_oid(name)
    }
    fn nsp_name(&self, oid: u32) -> Option<String>;
    fn nsp_oid(&self, name: &str) -> Result<u32>;
    fn role_name(&self, oid: u32) -> Option<String>;
    fn role_oid(&self, name: &str) -> Result<u32>;
    /// pg_attribute.atttypid of field `i` of the row type `rowtype`, when it is a domain
    /// (what record_send / record_recv put on the wire for it).
    fn field_domain(&self, _rowtype: u32, _i: usize) -> Option<u32> {
        None
    }
    fn enum_labels(&self, _oid: u32) -> Option<Vec<String>> {
        None
    }
    /// The version of the catalog names resolve through (None: not a catalog's).
    fn catalog_version(&self) -> Option<u64> {
        None
    }
    /// A user-defined operator as regoper (`with_args`: regoperator) shows it.
    fn oper_display(&self, _oid: u32, _with_args: bool) -> Option<String> {
        None
    }
    /// User-defined operators by (possibly qualified) name: (oid, left type oid, right type oid).
    fn opers_named(&self, _name: &str) -> Vec<(u32, Option<u32>, u32)> {
        Vec::new()
    }
    /// A function as regprocedure shows it: `name(integer,text)`.
    fn proc_signature(&self, _oid: u32) -> Option<String> {
        None
    }
    /// regconfig / regdictionary input and output.
    fn ts_oid(&self, kind: crate::catalog::tsearch::TsKind, name: &str) -> Result<u32> {
        crate::catalog::Catalog::default().ts_lookup_text(kind, name, &[])
    }
    fn ts_name(&self, kind: crate::catalog::tsearch::TsKind, oid: u32) -> Option<String> {
        crate::catalog::Catalog::default().ts_display(kind, oid, &[])
    }
    /// regcollation input: a (possibly qualified) collation name's parts.
    fn collation_oid(&self, parts: &[String]) -> Option<u32> {
        match parts {
            [n] => crate::catalog::system::collation_oid(n),
            [s, n] if s == "pg_catalog" => crate::catalog::system::collation_oid(n),
            _ => None,
        }
    }
    /// regcollation output.
    fn collation_name(&self, oid: u32) -> Option<String> {
        crate::catalog::system::collation_name(oid).map(crate::catalog::quote_ident)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntervalStyle {
    Postgres,
    Iso8601,
    SqlStandard,
    PostgresVerbose,
}

/// Session settings that affect text I/O.
#[derive(Clone)]
pub struct FormatCtx {
    pub tz: TimeZone,
    pub interval_style: IntervalStyle,
    pub date_style: datetime::DateStyle,
    pub extra_float_digits: i32,
    pub bytea_hex: bool,
    pub clock: Clock,
    pub names: Option<Arc<dyn NameResolver>>,
    /// `xmloption = document`.
    pub xml_document: bool,
    /// `xmlbinary = hex`.
    pub xml_binary_hex: bool,
    /// `timezone_abbreviations` (Default, Australia or India).
    pub tz_abbrevs: &'static str,
    /// The generation of the session settings this was made from (`Settings::generation`;
    /// 0: not made from a session's), which caches of what parsing under it gives compare.
    pub settings: u64,
    /// client_encoding: what binary send / receive functions' text is in.
    pub client_encoding: i32,
}

impl FormatCtx {
    /// What date/time input decoding needs.
    pub fn dt_ctx(&self) -> datetime::DtCtx<'_> {
        datetime::DtCtx {
            tz: &self.tz,
            now: self.clock.now,
            order: self.date_style.order,
            interval_style: self.interval_style,
            abbrevs: self.tz_abbrevs,
        }
    }
}

impl Default for FormatCtx {
    fn default() -> Self {
        FormatCtx {
            tz: TimeZone::default(),
            interval_style: IntervalStyle::Postgres,
            date_style: datetime::DateStyle::default(),
            extra_float_digits: 1,
            bytea_hex: true,
            clock: Clock { now: 0 },
            names: None,
            xml_document: false,
            xml_binary_hex: false,
            tz_abbrevs: "Default",
            settings: 0,
            client_encoding: super::mbconv::UTF8,
        }
    }
}

impl std::fmt::Debug for FormatCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FormatCtx").field("tz", &self.tz).finish()
    }
}

// ── floats ──

/// Shortest round-trip digits and decimal exponent: 1.5e-5 → ("15", -5), as Ryu (which
/// Postgres uses) finds them: the shortest decimals strictly between the midpoints to the
/// neighbouring floats (one exactly on a midpoint isn't taken, though it reads back as this
/// float), the closest of them, ties to even.
fn shortest_digits(v: f64, single: bool) -> (String, i32) {
    let split = |s: String| {
        let (mant, exp) = s.split_once('e').unwrap();
        let exp: i32 = exp.parse().unwrap();
        let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
        (digits, exp)
    };
    let fmt = |p: Option<usize>| match (single, p) {
        (true, None) => format!("{:e}", v as f32),
        (false, None) => format!("{v:e}"),
        (true, Some(p)) => format!("{:.*e}", p, v as f32),
        (false, Some(p)) => format!("{:.*e}", p, v),
    };
    let roundtrips = |d: &str, e: i32| {
        let text = format!("{}.{}e{e}", &d[..1], if d.len() > 1 { &d[1..] } else { "0" });
        if single {
            text.parse::<f32>().is_ok_and(|x| x == v as f32)
        } else {
            text.parse::<f64>().is_ok_and(|x| x == v)
        }
    };
    let (digits, exp) = split(fmt(None));
    let max = if single { 9 } else { 17 };
    for n in digits.len()..=max {
        // The closest decimal of n digits (ties to even).
        let (d, e) = split(fmt(Some(n - 1)));
        let d = d.trim_end_matches('0');
        let d = if d.is_empty() { "0" } else { d };
        if roundtrips(d, e) && !is_midpoint(v, single, d, e) {
            return (d.to_string(), e);
        }
    }
    (digits, exp)
}

/// Whether `digits` × 10^(exp - digits + 1) is exactly the midpoint between `v` (positive)
/// and one of its neighbouring floats.
fn is_midpoint(v: f64, single: bool, digits: &str, exp: i32) -> bool {
    use num_bigint::BigUint;
    let (m, e, min_mant, min_e): (u64, i32, u64, i32) = if single {
        let b = (v as f32).to_bits();
        let (eb, frac) = ((b >> 23) & 0xff, (b & 0x7f_ffff) as u64);
        if eb == 0 {
            (frac, -149, 1 << 23, -149)
        } else {
            (frac | 0x80_0000, eb as i32 - 150, 1 << 23, -149)
        }
    } else {
        let b = v.to_bits();
        let (eb, frac) = ((b >> 52) & 0x7ff, b & 0xf_ffff_ffff_ffff);
        if eb == 0 {
            (frac, -1074, 1 << 52, -1074)
        } else {
            (frac | 0x10_0000_0000_0000, eb as i32 - 1075, 1 << 52, -1074)
        }
    };
    // (An odd multiple of a negative power of two ends in 5.)
    if e < 2 && !digits.ends_with('5') {
        return false;
    }
    let Ok(n) = digits.parse::<BigUint>() else {
        return false;
    };
    let k = exp - (digits.len() as i32 - 1);
    // n * 10^k == odd * 2^p ?
    let equals = |odd: u64, p: i32| {
        let ten = BigUint::from(10u32);
        let two = BigUint::from(2u32);
        let mut lhs = n.clone();
        let mut rhs = BigUint::from(odd);
        if k >= 0 {
            lhs *= ten.pow(k as u32);
        } else {
            rhs *= ten.pow((-k) as u32);
        }
        if p >= 0 {
            rhs *= two.pow(p as u32);
        } else {
            lhs *= two.pow((-p) as u32);
        }
        lhs == rhs
    };
    // Up: (2m + 1) * 2^(e-1); down: (2m - 1) * 2^(e-1), or (4m - 1) * 2^(e-2) where the
    // spacing below halves (a power of two above the subnormals).
    if equals(2 * m + 1, e - 1) {
        return true;
    }
    if m == min_mant && e > min_e {
        equals(4 * m - 1, e - 2)
    } else {
        m > 0 && equals(2 * m - 1, e - 1)
    }
}

fn format_float_shortest(v: f64, single: bool) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    if v == 0.0 {
        return if v.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    let neg = v < 0.0;
    let (digits, exp) = shortest_digits(v.abs(), single);
    let max_fixed = if single { 6 } else { 15 };
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if exp < -4 || exp >= max_fixed {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", exp.abs()));
    } else if exp < 0 {
        out.push_str("0.");
        for _ in 0..(-exp - 1) {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        let int_len = exp as usize + 1;
        if digits.len() <= int_len {
            out.push_str(&digits);
            for _ in 0..int_len - digits.len() {
                out.push('0');
            }
        } else {
            out.push_str(&digits[..int_len]);
            out.push('.');
            out.push_str(&digits[int_len..]);
        }
    }
    out
}

/// `%.{prec}g` as C's printf formats it.
fn format_g(v: f64, prec: usize) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    if v == 0.0 {
        return if v.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    let prec = prec.max(1);
    let s = format!("{:.*e}", prec - 1, v);
    let (mant, exp) = s.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    if exp < -4 || exp >= prec as i32 {
        let mant = trim_frac_zeros(mant);
        format!("{mant}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs())
    } else {
        let decimals = (prec as i32 - 1 - exp).max(0) as usize;
        trim_frac_zeros(&format!("{v:.decimals$}")).to_string()
    }
}

fn trim_frac_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

pub fn format_float8(v: f64, extra: i32) -> String {
    if extra > 0 {
        format_float_shortest(v, false)
    } else {
        format_g(v, (15 + extra).max(1) as usize)
    }
}

pub fn format_float4(v: f32, extra: i32) -> String {
    if extra > 0 {
        format_float_shortest(v as f64, true)
    } else {
        format_g(v as f64, (6 + extra).max(1) as usize)
    }
}

pub fn parse_float8(s: &str) -> Result<f64> {
    parse_float(s, "double precision")
}

pub fn parse_float4(s: &str) -> Result<f32> {
    let v = parse_float(s, "real")?;
    // strtof rounds once, straight from the decimal string (going through f64 can round
    // twice).
    let f = if v.is_finite() {
        s.trim().parse::<f32>().unwrap_or(v as f32)
    } else {
        v as f32
    };
    if (f.is_infinite() && v.is_finite()) || (f == 0.0 && v != 0.0) {
        bail!(
            NUMERIC_VALUE_OUT_OF_RANGE,
            "\"{}\" is out of range for type real",
            s.trim()
        );
    }
    Ok(f)
}

fn parse_float(s: &str, tname: &str) -> Result<f64> {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    let v = match lower.as_str() {
        "nan" | "+nan" | "-nan" => return Ok(f64::NAN),
        "infinity" | "+infinity" | "inf" | "+inf" => return Ok(f64::INFINITY),
        "-infinity" | "-inf" => return Ok(f64::NEG_INFINITY),
        _ => {
            let ok = !t.is_empty()
                && t.bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
                && t.bytes().any(|b| b.is_ascii_digit());
            if !ok {
                bail!(
                    INVALID_TEXT_REPRESENTATION,
                    "invalid input syntax for type {tname}: \"{s}\""
                );
            }
            t.parse::<f64>().map_err(|_| {
                err!(
                    INVALID_TEXT_REPRESENTATION,
                    "invalid input syntax for type {tname}: \"{s}\""
                )
            })?
        }
    };
    if v.is_infinite() {
        bail!(
            NUMERIC_VALUE_OUT_OF_RANGE,
            "\"{t}\" is out of range for type {tname}"
        );
    }
    if v == 0.0 && t.bytes().any(|b| (b'1'..=b'9').contains(&b)) {
        // Underflow: a nonzero mantissa that rounded to zero.
        let mant = t.split(['e', 'E']).next().unwrap_or("");
        if mant.bytes().any(|b| (b'1'..=b'9').contains(&b)) {
            bail!(
                NUMERIC_VALUE_OUT_OF_RANGE,
                "\"{t}\" is out of range for type {tname}"
            );
        }
    }
    Ok(v)
}

// ── integers ──

pub fn int_type_name(ty: &DataType) -> &'static str {
    match ty {
        DataType::Int2 => "smallint",
        DataType::Int4 => "integer",
        _ => "bigint",
    }
}

/// Parse an integer literal with Postgres's rules (whitespace, sign, `_` separators, 0x/0o/0b).
pub fn parse_int(s: &str, tname: &str, min: i64, max: i64) -> Result<i64> {
    let bad = || {
        err!(
            INVALID_TEXT_REPRESENTATION,
            "invalid input syntax for type {tname}: \"{s}\""
        )
    };
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let lower = body.to_ascii_lowercase();
    let (radix, digits) = if let Some(d) = lower.strip_prefix("0x") {
        (16, d)
    } else if let Some(d) = lower.strip_prefix("0o") {
        (8, d)
    } else if let Some(d) = lower.strip_prefix("0b") {
        (2, d)
    } else {
        (10, lower.as_str())
    };
    // An underscore must sit between digits; after a 0x/0o/0b prefix it may come first.
    if digits.is_empty()
        || (radix == 10 && digits.starts_with('_'))
        || digits.ends_with('_')
        || digits.contains("__")
    {
        return Err(bad());
    }
    let mut v: i128 = 0;
    for c in digits.chars() {
        if c == '_' {
            continue;
        }
        let d = c.to_digit(radix).ok_or_else(bad)? as i128;
        v = v * radix as i128 + d;
        if v > (max as i128) + 1 {
            bail!(
                NUMERIC_VALUE_OUT_OF_RANGE,
                "value \"{s}\" is out of range for type {tname}"
            );
        }
    }
    let v = if neg { -v } else { v };
    if v < min as i128 || v > max as i128 {
        bail!(
            NUMERIC_VALUE_OUT_OF_RANGE,
            "value \"{s}\" is out of range for type {tname}"
        );
    }
    Ok(v as i64)
}

/// `uint32in_subr` (oid, xid, cid input): C's `strtoul(s, .., 0)`, so a `0x` prefix is hex and
/// a leading `0` octal; a minus sign negates (`-1` is 4294967295); only whitespace may follow.
pub fn parse_uint32(s: &str, tname: &str) -> Result<u32> {
    let bad = || {
        err!(
            INVALID_TEXT_REPRESENTATION,
            "invalid input syntax for type {tname}: \"{s}\""
        )
    };
    let range = || {
        err!(
            NUMERIC_VALUE_OUT_OF_RANGE,
            "value \"{s}\" is out of range for type {tname}"
        )
    };
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && (b[i] == b' ' || (b'\t'..=b'\r').contains(&b[i])) {
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
    let radix: u32 = if b.get(i) == Some(&b'0')
        && matches!(b.get(i + 1), Some(b'x' | b'X'))
        && b.get(i + 2).is_some_and(|c| c.is_ascii_hexdigit())
    {
        i += 2;
        16
    } else if b.get(i) == Some(&b'0') {
        8
    } else {
        10
    };
    let start = i;
    let mut v: u64 = 0;
    let mut overflow = false;
    while let Some(d) = b.get(i).and_then(|c| (*c as char).to_digit(radix)) {
        match v.checked_mul(radix as u64).and_then(|x| x.checked_add(d as u64)) {
            Some(x) => v = x,
            None => overflow = true,
        }
        i += 1;
    }
    if i == start {
        return Err(bad());
    }
    if overflow {
        return Err(range());
    }
    if !b[i..].iter().all(|c| *c == b' ' || (b'\t'..=b'\r').contains(c)) {
        return Err(bad());
    }
    let cvt = if neg { v.wrapping_neg() } else { v };
    let r = cvt as u32;
    if cvt != r as u64 && cvt != r as i32 as i64 as u64 {
        return Err(range());
    }
    Ok(r)
}

/// tidin: `(block,offset)` read as C does (strtoul / strtol in base 10, so a sign and leading
/// spaces pass, and `-1` is block 4294967295); text after the `)` is ignored.
pub fn parse_tid(s: &str) -> Result<Value> {
    let bad = || err!(INVALID_TEXT_REPRESENTATION, "invalid input syntax for type tid: \"{s}\"");
    let b = s.as_bytes();
    let mut coord = [0usize; 2];
    let mut i = 0;
    for (p, c) in b.iter().enumerate() {
        if i >= 2 || *c == b')' {
            break;
        }
        if *c == b',' || (*c == b'(' && i == 0) {
            coord[i] = p + 1;
            i += 1;
        }
    }
    if i < 2 {
        return Err(bad());
    }
    // strtol-style: spaces, a sign, decimal digits; returns (value, end).
    let num = |mut p: usize| -> Option<(i128, usize)> {
        while p < b.len() && (b[p] == b' ' || (b'\t'..=b'\r').contains(&b[p])) {
            p += 1;
        }
        let neg = match b.get(p) {
            Some(b'-') => {
                p += 1;
                true
            }
            Some(b'+') => {
                p += 1;
                false
            }
            _ => false,
        };
        let start = p;
        let mut v: i128 = 0;
        while p < b.len() && b[p].is_ascii_digit() {
            v = (v * 10 + (b[p] - b'0') as i128).min(i128::MAX / 100);
            p += 1;
        }
        (p > start).then_some((if neg { -v } else { v }, p))
    };
    let (blk, end) = num(coord[0]).ok_or_else(bad)?;
    if b.get(end) != Some(&b',') || blk.unsigned_abs() > u64::MAX as u128 {
        return Err(bad());
    }
    // strtoul: a negative value wraps; it must fit 32 bits either way.
    let cvt = if blk < 0 { blk as i64 as u64 } else { blk as u64 };
    let block = cvt as u32;
    if cvt != block as u64 && cvt != block as i32 as i64 as u64 {
        return Err(bad());
    }
    let (off, end) = num(coord[1]).ok_or_else(bad)?;
    if b.get(end) != Some(&b')') || !(0..=u16::MAX as i128).contains(&off) {
        return Err(bad());
    }
    Ok(Value::Tid(block, off as u16))
}

pub fn parse_bool(s: &str) -> Result<bool> {
    let t = s.trim().to_ascii_lowercase();
    let is_prefix = |full: &str, min: usize| t.len() >= min && full.starts_with(t.as_str());
    if is_prefix("true", 1) || is_prefix("yes", 1) || t == "on" || t == "1" {
        return Ok(true);
    }
    if is_prefix("false", 1) || is_prefix("no", 1) || is_prefix("off", 2) || t == "0" {
        return Ok(false);
    }
    bail!(
        INVALID_TEXT_REPRESENTATION,
        "invalid input syntax for type boolean: \"{s}\""
    )
}

// ── bytea ──

pub fn format_bytea(b: &[u8], hex: bool) -> String {
    if hex {
        let mut s = String::with_capacity(2 + b.len() * 2);
        s.push_str("\\x");
        for byte in b {
            s.push_str(&format!("{byte:02x}"));
        }
        s
    } else {
        let mut s = String::new();
        for &byte in b {
            match byte {
                b'\\' => s.push_str("\\\\"),
                0x20..=0x7e => s.push(byte as char),
                _ => s.push_str(&format!("\\{byte:03o}")),
            }
        }
        s
    }
}

pub fn parse_bytea(s: &str) -> Result<Vec<u8>> {
    if let Some(hex) = s.strip_prefix("\\x") {
        let mut out = Vec::with_capacity(hex.len() / 2);
        // Whitespace may separate byte pairs but not split one (hex_decode).
        let mut it = hex.chars();
        while let Some(h) = it.next() {
            if matches!(h, ' ' | '\t' | '\n' | '\r') {
                continue;
            }
            let hv = h.to_digit(16).ok_or_else(|| {
                err!(
                    INVALID_PARAMETER_VALUE,
                    "invalid hexadecimal digit: \"{h}\""
                )
            })?;
            let Some(l) = it.next() else {
                bail!(
                    INVALID_PARAMETER_VALUE,
                    "invalid hexadecimal data: odd number of digits"
                );
            };
            let lv = l.to_digit(16).ok_or_else(|| {
                err!(
                    INVALID_PARAMETER_VALUE,
                    "invalid hexadecimal digit: \"{l}\""
                )
            })?;
            out.push((hv * 16 + lv) as u8);
        }
        return Ok(out);
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            if i + 1 < b.len() && b[i + 1] == b'\\' {
                out.push(b'\\');
                i += 2;
            } else if i + 3 < b.len()
                && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
                && b[i + 1] <= b'3'
            {
                let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
                out.push(v);
                i += 4;
            } else {
                bail!(
                    INVALID_TEXT_REPRESENTATION,
                    "invalid input syntax for type bytea"
                );
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Ok(out)
}

// ── uuid ──

pub fn parse_uuid(s: &str) -> Result<u128> {
    let bad = || {
        err!(
            INVALID_TEXT_REPRESENTATION,
            "invalid input syntax for type uuid: \"{s}\""
        )
    };
    let mut t = s.trim();
    if t.starts_with('{') {
        t = t
            .strip_prefix('{')
            .and_then(|x| x.strip_suffix('}'))
            .ok_or_else(bad)?;
    }
    let mut v: u128 = 0;
    let mut n = 0;
    let mut since_hyphen = 0;
    let mut prev_hyphen = false;
    for (i, c) in t.chars().enumerate() {
        if c == '-' {
            // Hyphens only after a group of 4 digits.
            if i == 0 || prev_hyphen || since_hyphen % 4 != 0 {
                return Err(bad());
            }
            prev_hyphen = true;
            continue;
        }
        prev_hyphen = false;
        let d = c.to_digit(16).ok_or_else(bad)?;
        v = (v << 4) | d as u128;
        n += 1;
        since_hyphen += 1;
    }
    if n != 32 || prev_hyphen {
        return Err(bad());
    }
    Ok(v)
}

pub fn format_uuid(u: u128) -> String {
    let h = format!("{u:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

// ── character types ──

/// charin / text_char: `\ooo` (exactly four bytes, three octal digits) is that byte; otherwise
/// the first byte, or 0 for an empty string.
pub fn parse_char(s: &str) -> u8 {
    match s.as_bytes() {
        [b'\\', a @ b'0'..=b'7', b @ b'0'..=b'7', c @ b'0'..=b'7'] => {
            ((a - b'0') << 6).wrapping_add((b - b'0') << 3).wrapping_add(c - b'0')
        }
        b => b.first().copied().unwrap_or(0),
    }
}

/// Length in characters, for varchar/bpchar typmods.
pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Enforce varchar(n)/char(n) on input or explicit cast. `explicit` truncates silently, as
/// `'abcdef'::varchar(3)` does; otherwise exceeding the length is an error unless the extra
/// characters are spaces.
pub fn apply_char_typmod(s: &str, ty: &DataType, explicit: bool) -> Result<Arc<str>> {
    match ty {
        DataType::Varchar(m) if *m >= 4 => {
            let max = (*m - 4) as usize;
            if char_len(s) <= max {
                return Ok(s.into());
            }
            let cut: String = s.chars().take(max).collect();
            if explicit || s.chars().skip(max).all(|c| c == ' ') {
                return Ok(cut.into());
            }
            bail!(
                STRING_DATA_RIGHT_TRUNCATION,
                "value too long for type character varying({max})"
            )
        }
        DataType::Bpchar(m) if *m >= 4 => {
            let len = (*m - 4) as usize;
            let n = char_len(s);
            if n == len {
                return Ok(s.into());
            }
            if n < len {
                let mut out = s.to_string();
                out.extend(std::iter::repeat_n(' ', len - n));
                return Ok(out.into());
            }
            let cut: String = s.chars().take(len).collect();
            if explicit || s.chars().skip(len).all(|c| c == ' ') {
                return Ok(cut.into());
            }
            bail!(
                STRING_DATA_RIGHT_TRUNCATION,
                "value too long for type character({len})"
            )
        }
        DataType::Name => {
            // NAMEDATALEN - 1 bytes.
            if s.len() <= 63 {
                return Ok(s.into());
            }
            let mut end = 63;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            Ok(s[..end].into())
        }
        _ => Ok(s.into()),
    }
}

/// bpchar comparisons and most functions ignore trailing spaces.
pub fn bpchar_trim(s: &str) -> &str {
    s.trim_end_matches(' ')
}

// ── bit strings ──

pub fn parse_bits(s: &str, ty: &DataType) -> Result<Value> {
    let t = s;
    let (bits, hex) = if let Some(h) = t.strip_prefix(['x', 'X']) {
        (h, true)
    } else {
        (t.strip_prefix(['b', 'B']).unwrap_or(t), false)
    };
    let mut out: Vec<bool> = Vec::new();
    for c in bits.chars() {
        if hex {
            let d = c.to_digit(16).ok_or_else(|| {
                err!(
                    INVALID_TEXT_REPRESENTATION,
                    "\"{c}\" is not a valid hexadecimal digit"
                )
            })?;
            for i in (0..4).rev() {
                out.push(d >> i & 1 == 1);
            }
        } else {
            match c {
                '0' => out.push(false),
                '1' => out.push(true),
                _ => bail!(
                    INVALID_TEXT_REPRESENTATION,
                    "\"{c}\" is not a valid binary digit"
                ),
            }
        }
    }
    let typmod = ty.typmod();
    match ty {
        DataType::Bit(_) if typmod > 0 && out.len() != typmod as usize => {
            bail!(
                STRING_DATA_LENGTH_MISMATCH,
                "bit string length {} does not match type bit({typmod})",
                out.len()
            )
        }
        DataType::Varbit(_) if typmod > 0 && out.len() > typmod as usize => {
            bail!(
                STRING_DATA_RIGHT_TRUNCATION,
                "bit string too long for type bit varying({typmod})"
            )
        }
        _ => {}
    }
    Ok(bits_value(&out))
}

pub fn bits_value(bits: &[bool]) -> Value {
    let mut bytes = vec![0u8; bits.len().div_ceil(8)];
    for (i, b) in bits.iter().enumerate() {
        if *b {
            bytes[i / 8] |= 0x80 >> (i % 8);
        }
    }
    Value::Bits(bits.len() as u32, bytes.into())
}

pub fn bits_of(len: u32, bytes: &[u8]) -> Vec<bool> {
    (0..len as usize)
        .map(|i| bytes[i / 8] & (0x80 >> (i % 8)) != 0)
        .collect()
}

// ── network (Postgres's inet_net_pton.c / inet_cidr_ntop.c / inet_net_ntop.c) ──

/// inet/cidr input: Postgres's own parsers, so classful cidr ("10.1/16", "192.168"),
/// leading-zero rules and every rejection match exactly.
pub fn parse_inet(s: &str, cidr: bool) -> Result<Value> {
    let tname = if cidr { "cidr" } else { "inet" };
    let bad = || {
        err!(
            INVALID_TEXT_REPRESENTATION,
            "invalid input syntax for type {tname}: \"{s}\""
        )
    };
    let src = s.as_bytes();
    let v6 = src.contains(&b':');
    let mut addr = Vec::with_capacity(16);
    let bits = if v6 {
        addr.resize(16, 0);
        inet_pton6(src, &mut addr)
    } else if cidr {
        cidr_pton4(src, &mut addr)
    } else {
        net_pton4(src, &mut addr)
    }
    .ok_or_else(bad)?;
    addr.resize(if v6 { 16 } else { 4 }, 0);
    if bits as usize > addr.len() * 8 {
        return Err(bad());
    }
    let bits = bits as u8;
    if cidr && inet_network(&addr, bits) != addr {
        return Err(
            err!(INVALID_TEXT_REPRESENTATION, "invalid cidr value: \"{s}\"")
                .detail("Value has bits set to right of mask."),
        );
    }
    Ok(Value::Inet(addr.into(), bits))
}

/// Decimal digits from `src[*i..]` into a number; None past `max`.
fn digits(src: &[u8], i: &mut usize, max: u32) -> Option<u32> {
    let mut v = 0u32;
    while *i < src.len() && src[*i].is_ascii_digit() {
        v = v * 10 + (src[*i] - b'0') as u32;
        if v > max {
            return None;
        }
        *i += 1;
    }
    Some(v)
}

/// `/bits` after an IPv4 mantissa: digits only, at most 32, nothing after.
fn v4_bits(src: &[u8], i: &mut usize, wrote: bool) -> Option<Option<u32>> {
    if *i < src.len() && src[*i] == b'/' && src.get(*i + 1).is_some_and(u8::is_ascii_digit) && wrote
    {
        *i += 1;
        let mut bits = 0u32;
        while *i < src.len() && src[*i].is_ascii_digit() {
            bits = bits
                .saturating_mul(10)
                .saturating_add((src[*i] - b'0') as u32);
            *i += 1;
        }
        if *i != src.len() || bits > 32 {
            return None;
        }
        return Some(Some(bits));
    }
    if *i != src.len() {
        return None;
    }
    Some(None)
}

/// `inet_net_pton_ipv4`: dotted decimal plus an optional /bits; without one, all four octets.
fn net_pton4(src: &[u8], dst: &mut Vec<u8>) -> Option<u32> {
    let mut i = 0;
    while i < src.len() && src[i].is_ascii_digit() {
        let v = digits(src, &mut i, 255)?;
        if dst.len() == 4 {
            return None;
        }
        dst.push(v as u8);
        if i == src.len() || src[i] == b'/' {
            break;
        }
        if src[i] != b'.' {
            return None;
        }
        i += 1;
    }
    let bits = match v4_bits(src, &mut i, !dst.is_empty())? {
        Some(b) => b,
        None if dst.len() == 4 => 32,
        None => return None,
    };
    if dst.is_empty() || (bits / 8) as usize > dst.len() {
        return None;
    }
    Some(bits)
}

/// `inet_cidr_pton_ipv4`: also hex ("0x0a01"), and a classful width when no /bits is given.
fn cidr_pton4(src: &[u8], dst: &mut Vec<u8>) -> Option<u32> {
    let mut i = 0;
    if src.len() > 2
        && src[0] == b'0'
        && matches!(src[1], b'x' | b'X')
        && src[2].is_ascii_hexdigit()
    {
        i = 2;
        let mut nibbles = Vec::new();
        while i < src.len() && src[i].is_ascii_hexdigit() {
            nibbles.push((src[i] as char).to_digit(16).unwrap() as u8);
            i += 1;
        }
        for pair in nibbles.chunks(2) {
            if dst.len() == 4 {
                return None;
            }
            dst.push(pair[0] << 4 | pair.get(1).copied().unwrap_or(0));
        }
    } else if src.first().is_some_and(u8::is_ascii_digit) {
        loop {
            let v = digits(src, &mut i, 255)?;
            if dst.len() == 4 {
                return None;
            }
            dst.push(v as u8);
            if i == src.len() || src[i] == b'/' {
                break;
            }
            if src[i] != b'.' || !src.get(i + 1).is_some_and(u8::is_ascii_digit) {
                return None;
            }
            i += 1;
        }
    } else {
        return None;
    }
    let bits = match v4_bits(src, &mut i, !dst.is_empty())? {
        Some(b) => b,
        None => {
            let first = dst[0];
            let mut b = match first {
                240.. => 32,
                224.. => 8,
                192.. => 24,
                128.. => 16,
                _ => 8,
            };
            if b < dst.len() as u32 * 8 {
                b = dst.len() as u32 * 8;
            }
            if b == 8 && first == 224 {
                b = 4;
            }
            b
        }
    };
    while bits > dst.len() as u32 * 8 {
        if dst.len() == 4 {
            return None;
        }
        dst.push(0);
    }
    Some(bits)
}

/// `getbits`: 0..=128, no leading zeros.
fn v6_bits(src: &[u8]) -> Option<u32> {
    if src.is_empty() || (src.len() > 1 && src[0] == b'0') || !src.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut i = 0;
    digits(src, &mut i, 128)
}

/// `getv4`: the dotted tail of an IPv6 address (no leading zeros, maybe a /bits).
fn v6_tail4(src: &[u8], dst: &mut [u8], bits: &mut Option<u32>) -> bool {
    let (mut val, mut n, mut k) = (0u32, 0, 0);
    let mut i = 0;
    while i < src.len() {
        let ch = src[i];
        i += 1;
        if ch.is_ascii_digit() {
            if n != 0 && val == 0 {
                return false;
            }
            n += 1;
            val = val * 10 + (ch - b'0') as u32;
            if val > 255 {
                return false;
            }
            continue;
        }
        if ch == b'.' || ch == b'/' {
            if k > 3 {
                return false;
            }
            dst[k] = val as u8;
            k += 1;
            if ch == b'/' {
                *bits = v6_bits(&src[i..]);
                return bits.is_some();
            }
            (val, n) = (0, 0);
            continue;
        }
        return false;
    }
    if n == 0 || k > 3 {
        return false;
    }
    dst[k] = val as u8;
    true
}

/// `inet_cidr_pton_ipv6`: returns the width (128 when none is given).
fn inet_pton6(src: &[u8], dst: &mut [u8]) -> Option<u32> {
    let mut tmp = [0u8; 16];
    let (mut tp, mut colonp) = (0usize, None::<usize>);
    let mut i = 0;
    if src.first() == Some(&b':') {
        if src.get(1) != Some(&b':') {
            return None;
        }
        i = 1;
    }
    let mut curtok = i;
    let (mut saw_xdigit, mut val, mut ndigits) = (false, 0u32, 0);
    let mut bits = None;
    while i < src.len() {
        let ch = src[i];
        i += 1;
        if let Some(d) = (ch as char).to_digit(16) {
            val = val << 4 | d;
            ndigits += 1;
            if ndigits > 4 {
                return None;
            }
            saw_xdigit = true;
            continue;
        }
        if ch == b':' {
            curtok = i;
            if !saw_xdigit {
                if colonp.is_some() {
                    return None;
                }
                colonp = Some(tp);
                continue;
            } else if i == src.len() {
                return None;
            }
            if tp + 2 > 16 {
                return None;
            }
            tmp[tp] = (val >> 8) as u8;
            tmp[tp + 1] = val as u8;
            tp += 2;
            (saw_xdigit, ndigits, val) = (false, 0, 0);
            continue;
        }
        if ch == b'.' && tp + 4 <= 16 && v6_tail4(&src[curtok..], &mut tmp[tp..tp + 4], &mut bits) {
            tp += 4;
            saw_xdigit = false;
            break;
        }
        if ch == b'/'
            && let Some(b) = v6_bits(&src[i..])
        {
            bits = Some(b);
            break;
        }
        return None;
    }
    if saw_xdigit {
        if tp + 2 > 16 {
            return None;
        }
        tmp[tp] = (val >> 8) as u8;
        tmp[tp + 1] = val as u8;
        tp += 2;
    }
    if let Some(c) = colonp {
        if tp == 16 {
            return None;
        }
        let n = tp - c;
        for k in 1..=n {
            tmp[16 - k] = tmp[c + n - k];
            tmp[c + n - k] = 0;
        }
        tp = 16;
    }
    if tp != 16 {
        return None;
    }
    dst.copy_from_slice(&tmp);
    Some(bits.unwrap_or(128))
}

/// The address with every bit right of `mask` cleared (`network()`, the inet-to-cidr cast).
pub fn inet_network(bytes: &[u8], mask: u8) -> Vec<u8> {
    bytes
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let keep = (mask as usize).saturating_sub(i * 8).min(8);
            if keep == 0 {
                0
            } else {
                b & (0xffu8 << (8 - keep))
            }
        })
        .collect()
}

/// `inet_net_ntop`: the full address, then `/bits` unless it is the whole width (or `cidr`).
pub fn format_inet(bytes: &[u8], mask: u8, cidr: bool) -> String {
    let mut s = inet_host(bytes);
    if cidr || mask as usize != bytes.len() * 8 {
        s.push_str(&format!("/{mask}"));
    }
    s
}

/// The address alone (`host()`).
pub fn inet_host(bytes: &[u8]) -> String {
    if bytes.len() == 4 {
        return format!("{}.{}.{}.{}", bytes[0], bytes[1], bytes[2], bytes[3]);
    }
    let words: Vec<u16> = bytes
        .chunks(2)
        .map(|c| u16::from(c[0]) << 8 | u16::from(c[1]))
        .collect();
    // The longest run of zero words (the first on a tie), when at least two long.
    let (mut best, mut cur) = (None::<(usize, usize)>, None::<(usize, usize)>);
    for (i, w) in words.iter().enumerate() {
        if *w == 0 {
            cur = Some(cur.map_or((i, 1), |(b, l)| (b, l + 1)));
        } else if let Some(c) = cur.take()
            && best.is_none_or(|b| c.1 > b.1)
        {
            best = Some(c);
        }
    }
    if let Some(c) = cur
        && best.is_none_or(|b| c.1 > b.1)
    {
        best = Some(c);
    }
    let best = best.filter(|b| b.1 >= 2);
    let mut s = String::new();
    for (i, w) in words.iter().enumerate() {
        if let Some((b, l)) = best
            && (b..b + l).contains(&i)
        {
            if i == b {
                s.push(':');
            }
            continue;
        }
        if i != 0 {
            s.push(':');
        }
        // An encapsulated IPv4 address: ::a.b.c.d or ::ffff:a.b.c.d.
        if i == 6 && best.is_some_and(|(b, l)| b == 0 && (l == 6 || (l == 5 && words[5] == 0xffff)))
        {
            s.push_str(&format!(
                "{}.{}.{}.{}",
                bytes[12], bytes[13], bytes[14], bytes[15]
            ));
            return s;
        }
        s.push_str(&format!("{w:x}"));
    }
    if best.is_some_and(|(b, l)| b + l == 8) {
        s.push(':');
    }
    s
}

/// `inet_cidr_ntop`: `abbrev(cidr)`, only the octets/words the mask covers.
pub fn abbrev_cidr(bytes: &[u8], bits: u8) -> String {
    let bits = bits as usize;
    if bytes.len() == 4 {
        let mut s = String::new();
        if bits == 0 {
            s.push('0');
        }
        for (k, b) in bytes.iter().take(bits / 8).enumerate() {
            if k > 0 {
                s.push('.');
            }
            s.push_str(&b.to_string());
        }
        if !bits.is_multiple_of(8) {
            if !s.is_empty() {
                s.push('.');
            }
            s.push_str(&(bytes[bits / 8] & (0xffu8 << (8 - bits % 8))).to_string());
        }
        return format!("{s}/{bits}");
    }
    let mut out = String::new();
    if bits == 0 {
        out.push_str("::");
        return format!("{out}/0");
    }
    let s = inet_network(bytes, bits as u8);
    let words = (bits.div_ceil(16)).max(2);
    // The longest zero run (as Postgres finds it, reset only when a longer run ends).
    let (mut zero_s, mut zero_l, mut tmp_s, mut tmp_l) = (0, 0, 0, 0);
    for i in 0..words {
        if s[i * 2] | s[i * 2 + 1] == 0 {
            if tmp_l == 0 {
                tmp_s = i;
            }
            tmp_l += 1;
        } else if tmp_l != 0 && zero_l < tmp_l {
            (zero_s, zero_l, tmp_l) = (tmp_s, tmp_l, 0);
        }
    }
    if tmp_l != 0 && zero_l < tmp_l {
        (zero_s, zero_l) = (tmp_s, tmp_l);
    }
    let is_ipv4 = zero_l != words
        && zero_s == 0
        && (zero_l == 6
            || (zero_l == 5 && s[10] == 0xff && s[11] == 0xff)
            || (zero_l == 7 && s[14] != 0 && s[15] != 1));
    for p in 0..words {
        if zero_l != 0 && p >= zero_s && p < zero_s + zero_l {
            if p == zero_s {
                out.push(':');
            }
            if p == words - 1 {
                out.push(':');
            }
            continue;
        }
        if is_ipv4 && p > 5 {
            out.push(if p == 6 { ':' } else { '.' });
            out.push_str(&s[p * 2].to_string());
            if p != 7 || bits > 120 {
                out.push('.');
                out.push_str(&s[p * 2 + 1].to_string());
            }
        } else {
            if !out.is_empty() {
                out.push(':');
            }
            out.push_str(&format!(
                "{:x}",
                u16::from(s[p * 2]) << 8 | u16::from(s[p * 2 + 1])
            ));
        }
    }
    format!("{out}/{bits}")
}

/// `sscanf("%x")` / `"%2x"` (`width`): leading space, a sign, an optional 0x, hex digits.
fn scan_hex(s: &[u8], i: &mut usize, width: Option<usize>) -> Option<u64> {
    while *i < s.len() && s[*i].is_ascii_whitespace() {
        *i += 1;
    }
    let limit = width.map_or(usize::MAX, |w| *i + w);
    let neg = match s.get(*i) {
        Some(b'-') if *i < limit => {
            *i += 1;
            true
        }
        Some(b'+') if *i < limit => {
            *i += 1;
            false
        }
        _ => false,
    };
    let hex_prefix = s.get(*i) == Some(&b'0') && matches!(s.get(*i + 1), Some(b'x' | b'X'));
    if hex_prefix && *i + 2 < limit && s.get(*i + 2).is_some_and(u8::is_ascii_hexdigit) {
        *i += 2;
    }
    let start = *i;
    let mut v: u64 = 0;
    while *i < limit && *i < s.len() && s[*i].is_ascii_hexdigit() {
        v = v
            .saturating_mul(16)
            .saturating_add((s[*i] as char).to_digit(16).unwrap() as u64);
        *i += 1;
    }
    if *i == start {
        return None;
    }
    Some(if neg {
        (v as u32).wrapping_neg() as u64
    } else {
        v
    })
}

/// macaddr input: the `sscanf` formats Postgres tries in turn.
pub fn parse_macaddr(s: &str) -> Result<[u8; 6]> {
    const FORMATS: [&str; 7] = [
        "%x:%x:%x:%x:%x:%x",
        "%x-%x-%x-%x-%x-%x",
        "%2x%2x%2x:%2x%2x%2x",
        "%2x%2x%2x-%2x%2x%2x",
        "%2x%2x.%2x%2x.%2x%2x",
        "%2x%2x-%2x%2x-%2x%2x",
        "%2x%2x%2x%2x%2x%2x",
    ];
    let b = s.as_bytes();
    'format: for f in FORMATS {
        let (f, mut i, mut k, mut n) = (f.as_bytes(), 0, 0, 0);
        let mut out = [0u64; 6];
        while k < f.len() {
            if f[k] == b'%' {
                let width = (f[k + 1] == b'2').then_some(2);
                k += if width.is_some() { 3 } else { 2 };
                let Some(v) = scan_hex(b, &mut i, width) else {
                    continue 'format;
                };
                out[n] = v;
                n += 1;
            } else {
                if b.get(i) != Some(&f[k]) {
                    continue 'format;
                }
                (i, k) = (i + 1, k + 1);
            }
        }
        // "%1s": anything but trailing space is a seventh conversion, so no match.
        if b[i..].iter().any(|c| !c.is_ascii_whitespace()) {
            continue;
        }
        if out.iter().any(|v| *v > 255) {
            bail!(
                NUMERIC_VALUE_OUT_OF_RANGE,
                "invalid octet value in \"macaddr\" value: \"{s}\""
            );
        }
        return Ok(out.map(|v| v as u8));
    }
    bail!(
        INVALID_TEXT_REPRESENTATION,
        "invalid input syntax for type macaddr: \"{s}\""
    )
}

/// macaddr8 input: hex pairs with one consistent spacer; 6 bytes get FF:FE inserted.
pub fn parse_macaddr8(s: &str) -> Result<[u8; 8]> {
    let bad = || {
        err!(
            INVALID_TEXT_REPRESENTATION,
            "invalid input syntax for type macaddr8: \"{s}\""
        )
    };
    let p = s.as_bytes();
    let mut i = p.iter().take_while(|c| c.is_ascii_whitespace()).count();
    let (mut out, mut spacer) = (Vec::with_capacity(8), None);
    while i + 1 < p.len() {
        if out.len() == 8 {
            return Err(bad());
        }
        let hex = |c: u8| (c as char).to_digit(16);
        let (Some(h), Some(l)) = (hex(p[i]), hex(p[i + 1])) else {
            return Err(bad());
        };
        out.push((h << 4 | l) as u8);
        i += 2;
        if let Some(c @ (b':' | b'-' | b'.')) = p.get(i).copied() {
            if *spacer.get_or_insert(c) != c {
                return Err(bad());
            }
            i += 1;
        }
        if (out.len() == 6 || out.len() == 8) && p.get(i).is_some_and(u8::is_ascii_whitespace) {
            if p[i..].iter().any(|c| !c.is_ascii_whitespace()) {
                return Err(bad());
            }
            i = p.len();
        }
    }
    match out[..] {
        [a, b, c, d, e, f] => Ok([a, b, c, 0xff, 0xfe, d, e, f]),
        [..] if out.len() == 8 => Ok(out.try_into().unwrap()),
        _ => Err(bad()),
    }
}

pub fn parse_money(s: &str) -> Result<i64> {
    let t: String = s
        .trim()
        .chars()
        .filter(|c| !matches!(c, '$' | ',' | ' '))
        .collect();
    let (neg, t) = if let Some(x) = t.strip_prefix('-') {
        (true, x.to_string())
    } else if t.starts_with('(') && t.ends_with(')') {
        (true, t[1..t.len() - 1].to_string())
    } else {
        (false, t)
    };
    let n: Numeric = t.parse().map_err(|_| {
        err!(
            INVALID_TEXT_REPRESENTATION,
            "invalid input syntax for type money: \"{s}\""
        )
    })?;
    let n = if neg { n.neg() } else { n };
    n.mul(&Numeric::from_i64(100))
        .round(0)
        .to_i64_round()
        .ok_or_else(|| {
            err!(
                NUMERIC_VALUE_OUT_OF_RANGE,
                "value \"{s}\" is out of range for type money"
            )
        })
}

pub fn format_money(cents: i64) -> String {
    let neg = cents < 0;
    let a = cents.unsigned_abs();
    let whole = (a / 100).to_string();
    let mut grouped = String::new();
    for (i, c) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{}${}.{:02}", if neg { "-" } else { "" }, grouped, a % 100)
}

pub fn parse_lsn(s: &str) -> Result<u64> {
    let bad = || {
        err!(
            INVALID_TEXT_REPRESENTATION,
            "invalid input syntax for type pg_lsn: \"{s}\""
        )
    };
    // pg_lsn_in_internal: 1 to 8 hex digits, '/', 1 to 8 hex digits, nothing else (not even
    // whitespace).
    let (hi, lo) = s.split_once('/').ok_or_else(bad)?;
    let hex = |p: &str| (1..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_hexdigit());
    if !hex(hi) || !hex(lo) {
        return Err(bad());
    }
    let hi = u32::from_str_radix(hi, 16).map_err(|_| bad())?;
    let lo = u32::from_str_radix(lo, 16).map_err(|_| bad())?;
    Ok(((hi as u64) << 32) | lo as u64)
}

// ── dispatch ──

/// Text → value for a type. `explicit` (a `::` cast) truncates char types instead of failing.
pub fn parse_value(s: &str, ty: &DataType, ctx: &FormatCtx) -> Result<Value> {
    parse_value_ex(s, ty, ctx, false)
}

pub fn parse_value_ex(s: &str, ty: &DataType, ctx: &FormatCtx, explicit: bool) -> Result<Value> {
    use DataType as T;
    Ok(match ty {
        T::Bool => Value::Bool(parse_bool(s)?),
        T::Int2 => Value::Int2(parse_int(s, "smallint", i16::MIN as i64, i16::MAX as i64)? as i16),
        T::Int4 => Value::Int4(parse_int(s, "integer", i32::MIN as i64, i32::MAX as i64)? as i32),
        T::Int8 => Value::Int8(parse_int(s, "bigint", i64::MIN, i64::MAX)?),
        T::Oid | T::Xid | T::Cid => Value::Oid(parse_uint32(
            s,
            match ty {
                T::Oid => "oid",
                T::Cid => "cid",
                _ => "xid",
            },
        )?),
        T::Float4 => Value::Float4(parse_float4(s)?),
        T::Float8 => Value::Float8(parse_float8(s)?),
        T::Numeric(m) => Value::numeric(Numeric::parse_pg(s)?.apply_typmod(*m)?),
        T::Text | T::Unknown | T::Cstring => Value::text(s),
        T::Varchar(_) | T::Bpchar(_) | T::Name => Value::Text(apply_char_typmod(s, ty, explicit)?),
        T::Char => Value::Char(parse_char(s)),
        T::Bytea => Value::bytea(parse_bytea(s)?),
        T::Date => Value::Date(datetime::parse_date_cx(s, &ctx.dt_ctx())?),
        T::Time(m) => Value::Time(datetime::round_micros(
            datetime::parse_time_cx(s, &ctx.dt_ctx())?,
            *m,
        )),
        T::Timetz(m) => {
            let (t, z) = datetime::parse_timetz_cx(s, &ctx.dt_ctx())?;
            Value::Timetz(datetime::round_micros(t, *m), z)
        }
        T::Timestamp(m) => Value::Timestamp(datetime::round_micros(
            datetime::parse_timestamp_cx(s, &ctx.dt_ctx())?,
            *m,
        )),
        T::Timestamptz(m) => Value::Timestamptz(datetime::round_micros(
            datetime::parse_timestamptz_cx(s, &ctx.dt_ctx())?,
            *m,
        )),
        T::Interval(m) => {
            Value::Interval(datetime::parse_interval_ex(s, ctx.interval_style, *m)?)
        }
        T::Uuid => Value::Uuid(parse_uuid(s)?),
        T::Json => {
            validate_json(s)?;
            Value::Json(s.into())
        }
        // Checked, then kept as written (xml_in).
        T::Xml => {
            let opt = if ctx.xml_document {
                super::xml::XmlOption::Document
            } else {
                super::xml::XmlOption::Content
            };
            super::xml::validate(s, opt, true)?;
            Value::Text(s.into())
        }
        T::Refcursor => Value::Text(s.into()),
        T::Jsonb => Value::Jsonb(Arc::new(parse_jsonb(s)?)),
        T::JsonPath => Value::Text(Arc::from(super::jsonpath::normalize(s)?)),
        T::Array(elem) => Value::Array(Arc::new(parse_array(s, elem, ctx)?)),
        T::Regclass => Value::Oid(resolve_reg(s, ctx, |n, x| n.rel_oid(x))?),
        T::Regtype => Value::Oid(resolve_reg(s, ctx, |n, x| n.type_oid(x))?),
        T::Regproc => Value::Oid(resolve_reg(s, ctx, |n, x| n.proc_oid(x))?),
        T::Regprocedure => Value::Oid(resolve_reg(s, ctx, |n, x| n.procedure_oid(x))?),
        T::Regnamespace => Value::Oid(resolve_reg(s, ctx, |n, x| n.nsp_oid(x))?),
        T::Regrole => Value::Oid(resolve_reg(s, ctx, |n, x| n.role_oid(x))?),
        T::Regconfig => Value::Oid(resolve_ts(s, ctx, crate::catalog::tsearch::TsKind::Config)?),
        T::Regdictionary => Value::Oid(resolve_ts(s, ctx, crate::catalog::tsearch::TsKind::Dict)?),
        T::TsVector => Value::TsVector(Arc::new(super::tsearch::parse_tsvector(s)?)),
        T::TsQuery => Value::TsQuery(Arc::new(super::tsearch::parse_tsquery_text(
            s,
            &mut super::tsearch::queue_notice,
        )?)),
        T::Regoper => Value::Oid(parse_regoper(s, ctx)?),
        T::Regoperator => Value::Oid(parse_regoperator(s, ctx)?),
        T::Regcollation => Value::Oid(parse_regcollation(s, ctx)?),
        T::Bit(_) | T::Varbit(_) => parse_bits(s, ty)?,
        T::Inet => parse_inet(s, false)?,
        T::Cidr => parse_inet(s, true)?,
        T::Macaddr => Value::Macaddr(parse_macaddr(s)?),
        T::Macaddr8 => Value::Macaddr8(parse_macaddr8(s)?),
        T::Point => geo_value(super::geo::Geo::Point(super::geo::parse_point(s)?)),
        T::Lseg => geo_value(super::geo::Geo::Lseg(super::geo::parse_lseg(s)?)),
        T::Path => geo_value(super::geo::Geo::Path(super::geo::parse_path(s)?)),
        T::GBox => geo_value(super::geo::Geo::Box(super::geo::parse_box(s)?)),
        T::Polygon => geo_value(super::geo::Geo::Polygon(super::geo::parse_polygon(s)?)),
        T::Line => geo_value(super::geo::Geo::Line(super::geo::parse_line(s)?)),
        T::Circle => geo_value(super::geo::Geo::Circle(super::geo::parse_circle(s)?)),
        T::Money => Value::Money(parse_money(s)?),
        T::PgLsn => Value::PgLsn(parse_lsn(s)?),
        T::Xid8 => Value::Xid8(super::snapshot::parse_xid8(s)?),
        // txid_snapshot_in is pg_snapshot_in, errors included.
        T::PgSnapshot | T::TxidSnapshot => Value::Snapshot(Arc::new(
            super::snapshot::Snapshot::parse(s, "pg_snapshot")?,
        )),
        T::Int2Vector | T::OidVector => {
            let elem = if matches!(ty, T::Int2Vector) {
                T::Int2
            } else {
                T::Oid
            };
            let items = parse_vector(s, matches!(ty, T::Int2Vector))?;
            Value::Array(Arc::new(super::array::ArrayValue {
                dims: if items.is_empty() {
                    vec![]
                } else {
                    vec![(items.len(), 0)]
                },
                elem,
                items,
            }))
        }
        T::Tid => parse_tid(s)?,
        T::Void => Value::Null,
        T::Record => bail!(
            FEATURE_NOT_SUPPORTED,
            "input of anonymous composite types is not implemented"
        ),
        T::Row(fields) => parse_record(s, fields, ctx)?,
        T::Range(rt) => super::range::value(super::range::parse(s, rt, ctx)?),
        T::Multirange(rt) => super::range::multi_value(super::range::parse_multi(s, rt, ctx)?),
        T::Enum(oid, name, _) => {
            if let Some(labels) = ctx.names.as_ref().and_then(|n| n.enum_labels(*oid))
                && !labels.iter().any(|l| l == s)
            {
                bail!(
                    INVALID_TEXT_REPRESENTATION,
                    "invalid input value for enum {name}: \"{s}\""
                );
            }
            crate::sql::func::system::check_safe_enum(*oid, s, name)?;
            Value::text(s)
        }
        T::AnyEnum => bail!(
            INDETERMINATE_DATATYPE,
            "could not determine polymorphic type because input has type unknown"
        ),
        T::Aclitem => match &ctx.names {
            Some(n) => {
                let unquote = |q: String| {
                    if q.starts_with('"') {
                        q[1..q.len() - 1].replace("\"\"", "\"")
                    } else {
                        q
                    }
                };
                let (item, _) = crate::catalog::acl::parse_item(s, &|r| {
                    n.role_oid(&crate::catalog::quote_ident(r)).ok()
                })?;
                Value::text(crate::catalog::acl::format_item(&item, &|oid| {
                    n.role_name(oid).map(unquote)
                }))
            }
            None => Value::text(s),
        },
        T::Trigger => Value::text(s),
        // A made base type reads its values as its input function's built-in type does; a
        // shell's input function (shell_in) refuses them.
        T::Base(b) if b.ext != 0 => crate::ext::types::input(b.ext, s)?,
        T::Base(b) => match &b.repr {
            Some(r) => parse_value_ex(s, r, ctx, explicit)?,
            None => bail!(
                FEATURE_NOT_SUPPORTED,
                "cannot accept a value of a shell type"
            ),
        },
        T::PgNodeTree | T::PgNdistinct | T::PgDependencies | T::PgMcvList => bail!(
            FEATURE_NOT_SUPPORTED,
            "cannot accept a value of type {}",
            ty.typname()
        ),
        // gtsvectorin and the BRIN summaries' input functions (the multi-minmax one names
        // its type without the pg_ prefix).
        T::Gtsvector | T::PgBrinBloomSummary => bail!(
            FEATURE_NOT_SUPPORTED,
            "cannot accept a value of type {}",
            ty.typname()
        ),
        T::PgBrinMinmaxMultiSummary => bail!(
            FEATURE_NOT_SUPPORTED,
            "cannot accept a value of type brin_minmax_multi_summary"
        ),
        T::AnyElement
        | T::AnyArray
        | T::AnyNonArray
        | T::AnyCompatible
        | T::AnyCompatibleNonArray
        | T::AnyCompatibleArray
        | T::Any
        | T::AnyRange
        | T::AnyMultirange
        | T::AnyCompatibleRange
        | T::AnyCompatibleMultirange => {
            bail!(
                INDETERMINATE_DATATYPE,
                "could not determine polymorphic type because input has type unknown"
            )
        }
    })
}

/// Composite input: `(a,"b c",)`; empty fields are NULL (record_in).
pub fn parse_record(s: &str, fields: &[(String, DataType)], ctx: &FormatCtx) -> Result<Value> {
    let bad = |detail: &str| {
        err!(
            INVALID_TEXT_REPRESENTATION,
            "malformed record literal: \"{s}\""
        )
        .detail(detail.to_string())
    };
    let chars: Vec<char> = s.chars().collect();
    let mut p = 0;
    while p < chars.len() && chars[p].is_ascii_whitespace() {
        p += 1;
    }
    if chars.get(p) != Some(&'(') {
        return Err(bad("Missing left parenthesis."));
    }
    p += 1;
    let mut vals = Vec::with_capacity(fields.len());
    for (i, (_, ty)) in fields.iter().enumerate() {
        if i > 0 {
            if chars.get(p) == Some(&',') {
                p += 1;
            } else {
                return Err(bad("Too few columns."));
            }
        }
        // Completely empty input means NULL.
        if matches!(chars.get(p), Some(',') | Some(')')) {
            vals.push(Value::Null);
            continue;
        }
        let mut buf = String::new();
        let mut inquote = false;
        loop {
            match chars.get(p) {
                Some(',') | Some(')') if !inquote => break,
                None => return Err(bad("Unexpected end of input.")),
                Some(&ch) => {
                    p += 1;
                    if ch == '\\' {
                        match chars.get(p) {
                            Some(&c) => {
                                buf.push(c);
                                p += 1;
                            }
                            None => return Err(bad("Unexpected end of input.")),
                        }
                    } else if ch == '"' {
                        if !inquote {
                            inquote = true;
                        } else if chars.get(p) == Some(&'"') {
                            buf.push('"');
                            p += 1;
                        } else {
                            inquote = false;
                        }
                    } else {
                        buf.push(ch);
                    }
                }
            }
        }
        vals.push(parse_value(&buf, ty, ctx)?);
    }
    if chars.get(p) != Some(&')') {
        return Err(bad("Too many columns."));
    }
    p += 1;
    while p < chars.len() && chars[p].is_ascii_whitespace() {
        p += 1;
    }
    if p < chars.len() {
        return Err(bad("Junk after right parenthesis."));
    }
    Ok(Value::Record(Arc::new(vals)))
}

fn resolve_reg(
    s: &str,
    ctx: &FormatCtx,
    f: impl Fn(&dyn NameResolver, &str) -> Result<u32>,
) -> Result<u32> {
    let t = s.trim();
    if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
        return t.parse().map_err(|_| {
            err!(
                INVALID_TEXT_REPRESENTATION,
                "invalid input syntax for type oid: \"{s}\""
            )
        });
    }
    if t == "-" {
        return Ok(0);
    }
    match &ctx.names {
        Some(n) => f(n.as_ref(), t),
        None => Err(Error::new(
            code::UNDEFINED_OBJECT,
            format!("\"{t}\" does not exist"),
        )),
    }
}

/// regconfigin / regdictionaryin: a number is an oid; otherwise the name, through the path.
fn resolve_ts(s: &str, ctx: &FormatCtx, kind: crate::catalog::tsearch::TsKind) -> Result<u32> {
    let t = s.trim();
    if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
        return t.parse().map_err(|_| {
            err!(
                INVALID_TEXT_REPRESENTATION,
                "invalid input syntax for type oid: \"{s}\""
            )
        });
    }
    match &ctx.names {
        Some(n) => n.ts_oid(kind, s),
        None => crate::catalog::Catalog::default().ts_lookup_text(kind, s, &[]),
    }
}

fn ts_name(o: u32, ctx: &FormatCtx, kind: crate::catalog::tsearch::TsKind) -> String {
    let name = match &ctx.names {
        Some(n) => n.ts_name(kind, o),
        None => crate::catalog::Catalog::default().ts_display(kind, o, &[]),
    };
    name.unwrap_or_else(|| o.to_string())
}

/// An operator name, optionally `pg_catalog.`-qualified: (schema ok, name).
fn oper_name(s: &str) -> (bool, &str) {
    match s.rsplit_once('.') {
        Some((sch, n)) if !n.is_empty() => (sch.trim().trim_matches('"') == "pg_catalog", n.trim()),
        _ => (true, s),
    }
}

/// regoperin: an operator by name, which must be unambiguous.
fn parse_regoper(s: &str, ctx: &FormatCtx) -> Result<u32> {
    let t = s.trim();
    if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
        return t.parse().map_err(|_| {
            err!(
                INVALID_TEXT_REPRESENTATION,
                "invalid input syntax for type oid: \"{s}\""
            )
        });
    }
    let (ok, name) = oper_name(t);
    let mut ops: Vec<u32> = if ok {
        crate::sql::func::operators_named(name)
            .iter()
            .map(|o| o.oid)
            .collect()
    } else {
        Vec::new()
    };
    if let Some(n) = &ctx.names {
        ops.extend(n.opers_named(t).iter().map(|o| o.0));
    }
    match ops.len() {
        0 => Err(Error::new(
            code::UNDEFINED_FUNCTION,
            format!("operator does not exist: {s}"),
        )),
        1 => Ok(ops[0]),
        _ => Err(Error::new(
            code::AMBIGUOUS_FUNCTION,
            format!("more than one operator named {name}"),
        )),
    }
}

/// regoperatorin: `name(left,right)`, NONE for a prefix operator's missing left.
fn parse_regoperator(s: &str, ctx: &FormatCtx) -> Result<u32> {
    let t = s.trim();
    if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
        return t.parse().map_err(|_| {
            err!(
                INVALID_TEXT_REPRESENTATION,
                "invalid input syntax for type oid: \"{s}\""
            )
        });
    }
    let Some((name, rest)) = t.split_once('(') else {
        return Err(Error::new(
            code::INVALID_TEXT_REPRESENTATION,
            "expected a left parenthesis",
        ));
    };
    let Some(args) = rest.trim_end().strip_suffix(')') else {
        return Err(Error::new(
            code::INVALID_TEXT_REPRESENTATION,
            "expected a right parenthesis",
        ));
    };
    let args: Vec<&str> = args.split(',').map(str::trim).collect();
    if args.len() == 1 {
        return Err(Error::new(code::UNDEFINED_PARAMETER, "missing argument")
            .hint("Use NONE to denote the missing argument of a unary operator."));
    }
    if args.len() != 2 {
        return Err(Error::new(code::TOO_MANY_ARGUMENTS, "too many arguments")
            .hint("Provide two argument types for operator."));
    }
    let ty = |a: &str| -> Result<Option<u32>> {
        if a.eq_ignore_ascii_case("none") {
            return Ok(None);
        }
        match &ctx.names {
            Some(n) => n.type_oid(a).map(Some),
            None => Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("type \"{a}\" does not exist"),
            )),
        }
    };
    let (left, right) = (ty(args[0])?, ty(args[1])?);
    let (ok, name) = oper_name(name.trim());
    let mut found = if ok {
        crate::sql::func::operators_named(name)
            .into_iter()
            .find(|o| o.left.as_ref().map(DataType::oid) == left && Some(o.right.oid()) == right)
            .map(|o| o.oid)
    } else {
        None
    };
    if found.is_none()
        && let Some(n) = &ctx.names
    {
        found = n
            .opers_named(t.split_once('(').map_or(t, |p| p.0).trim())
            .iter()
            .find(|o| o.1 == left && Some(o.2) == right)
            .map(|o| o.0);
    }
    found.ok_or_else(|| {
        Error::new(
            code::UNDEFINED_FUNCTION,
            format!("operator does not exist: {s}"),
        )
    })
}

/// regcollationin: a collation by (possibly quoted, possibly qualified) name.
fn parse_regcollation(s: &str, ctx: &FormatCtx) -> Result<u32> {
    let t = s.trim();
    if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
        return t.parse().map_err(|_| {
            err!(
                INVALID_TEXT_REPRESENTATION,
                "invalid input syntax for type oid: \"{s}\""
            )
        });
    }
    if t == "-" {
        return Ok(0);
    }
    let parts = crate::catalog::names::parse_qualified(t)?;
    let found = match &ctx.names {
        Some(n) => n.collation_oid(&parts),
        None => match parts.as_slice() {
            [n] => crate::catalog::system::collation_oid(n),
            [s, n] if s == "pg_catalog" => crate::catalog::system::collation_oid(n),
            _ => None,
        },
    };
    match found {
        Some(o) => Ok(o),
        _ => Err(Error::new(
            code::UNDEFINED_OBJECT,
            format!(
                "collation \"{}\" for encoding \"UTF8\" does not exist",
                parts.join(".")
            ),
        )),
    }
}

fn geo_value(g: super::geo::Geo) -> Value {
    Value::Geo(Arc::new(g))
}

/// `record_out`: the fields, quoted where they need it.
fn format_record(fields: &[Value], out: impl Fn(usize, &Value) -> String) -> String {
    let mut s = String::from("(");
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        if f.is_null() {
            continue;
        }
        let t = out(i, f);
        let quote = t.is_empty()
            || t.chars()
                .any(|c| matches!(c, '"' | '\\' | '(' | ')' | ',') || c.is_ascii_whitespace());
        if quote {
            s.push('"');
            for c in t.chars() {
                if c == '"' || c == '\\' {
                    s.push(c);
                }
                s.push(c);
            }
            s.push('"');
        } else {
            s.push_str(&t);
        }
    }
    s.push(')');
    s
}

/// Value → text, as the type's output function prints it.
pub fn format_value(v: &Value, ty: &DataType, ctx: &FormatCtx) -> String {
    use DataType as T;
    match (v, ty) {
        (Value::Null, _) => String::new(),
        (Value::Ext(k, b), _) => crate::ext::types::output(*k, b, ctx),
        // Fields of a known row type print by their types (a regclass as its name).
        (Value::Record(fields), T::Row(rt)) if fields.len() == rt.fields.len() => {
            // (A table's row with dropped columns, as a trigger's OLD / NEW: they don't print.)
            let dropped = |n: &str| n.starts_with("........pg.dropped.");
            if rt.fields.iter().any(|f| dropped(&f.0)) {
                let (vals, tys): (Vec<Value>, Vec<&DataType>) = fields
                    .iter()
                    .zip(&rt.fields)
                    .filter(|(_, f)| !dropped(&f.0))
                    .map(|(v, f)| (v.clone(), &f.1))
                    .unzip();
                return format_record(&vals, |i, f| format_value(f, tys[i], ctx));
            }
            format_record(fields, |i, f| format_value(f, &rt.fields[i].1, ctx))
        }
        (Value::Bytea(b), T::PgNdistinct) => super::extstats::format_ndistinct(b),
        (Value::Bytea(b), T::PgDependencies) => super::extstats::format_dependencies(b),
        (Value::Oid(o), T::Regclass) => reg_name(*o, ctx, |n, o| n.rel_name(o)),
        (Value::Oid(o), T::Regtype) => reg_name(*o, ctx, |n, o| n.type_name(o)),
        (Value::Oid(o), T::Regproc) => reg_name(*o, ctx, |n, o| n.proc_name(o)),
        (Value::Oid(o), T::Regprocedure) => reg_name(*o, ctx, |n, o| {
            n.proc_signature(o).or_else(|| n.proc_name(o))
        }),
        (Value::Oid(o), T::Regnamespace) => reg_name(*o, ctx, |n, o| n.nsp_name(o)),
        (Value::Oid(o), T::Regrole) => reg_name(*o, ctx, |n, o| n.role_name(o)),
        (Value::Oid(o), T::Regconfig) => ts_name(*o, ctx, crate::catalog::tsearch::TsKind::Config),
        (Value::Oid(o), T::Regdictionary) => {
            ts_name(*o, ctx, crate::catalog::tsearch::TsKind::Dict)
        }
        (Value::Oid(o), T::Regoper) => match crate::sql::func::operator_by_oid(*o) {
            // Qualified when the name alone is ambiguous.
            Some(op) if crate::sql::func::operators_named(op.name).len() == 1 => {
                op.name.to_string()
            }
            Some(op) => format!("pg_catalog.{}", op.name),
            None => ctx
                .names
                .as_ref()
                .and_then(|n| n.oper_display(*o, false))
                .unwrap_or_else(|| o.to_string()),
        },
        (Value::Oid(o), T::Regoperator) => match crate::sql::func::operator_by_oid(*o) {
            Some(op) => format!(
                "{}({},{})",
                op.name,
                op.left.as_ref().map_or("NONE".into(), |t| t.display()),
                op.right.display()
            ),
            None => ctx
                .names
                .as_ref()
                .and_then(|n| n.oper_display(*o, true))
                .unwrap_or_else(|| o.to_string()),
        },
        (Value::Oid(0), T::Regcollation) => "-".into(),
        (Value::Oid(o), T::Regcollation) => match ctx
            .names
            .as_ref()
            .and_then(|n| n.collation_name(*o))
            .or_else(|| crate::catalog::system::collation_name(*o).map(crate::catalog::quote_ident))
        {
            Some(n) => n,
            None => o.to_string(),
        },
        (Value::Text(s), T::Xml) => super::xml::xml_out(s),
        (Value::Bits(len, bytes), _) => bits_of(*len, bytes)
            .iter()
            .map(|b| if *b { '1' } else { '0' })
            .collect(),
        (Value::Inet(b, m), T::Cidr) => format_inet(b, *m, true),
        (v, T::Base(b)) if b.repr.is_some() => format_value(v, b.repr.as_ref().unwrap(), ctx),
        (Value::Array(a), T::Int2Vector | T::OidVector) => a
            .items
            .iter()
            .map(|x| format_value(x, &a.elem, ctx))
            .collect::<Vec<_>>()
            .join(" "),
        _ => format_plain(v, ctx),
    }
}

fn reg_name(
    o: u32,
    ctx: &FormatCtx,
    f: impl Fn(&dyn NameResolver, u32) -> Option<String>,
) -> String {
    if o == 0 {
        return "-".into();
    }
    ctx.names
        .as_ref()
        .and_then(|n| f(n.as_ref(), o))
        .unwrap_or_else(|| o.to_string())
}

/// Output that depends only on the value's representation.
pub fn format_plain(v: &Value, ctx: &FormatCtx) -> String {
    match v {
        Value::Null => String::new(),
        Value::Ext(k, b) => crate::ext::types::output(*k, b, ctx),
        Value::Bool(b) => if *b { "t" } else { "f" }.into(),
        Value::Int2(i) => i.to_string(),
        Value::Int4(i) => i.to_string(),
        Value::Int8(i) => i.to_string(),
        Value::Oid(o) => o.to_string(),
        Value::Float4(f) => format_float4(*f, ctx.extra_float_digits),
        Value::Float8(f) => format_float8(*f, ctx.extra_float_digits),
        Value::Numeric(n) => n.to_string(),
        Value::Text(s) | Value::Json(s) => s.to_string(),
        Value::Char(c) => {
            if *c == 0 {
                String::new()
            } else if c.is_ascii() {
                (*c as char).to_string()
            } else {
                format!("\\{c:03o}")
            }
        }
        Value::Bytea(b) => format_bytea(b, ctx.bytea_hex),
        Value::Date(d) => datetime::format_date_styled(*d, ctx.date_style),
        Value::Time(t) => datetime::format_time_of_day(*t),
        Value::Timetz(t, z) => datetime::format_timetz(*t, *z),
        Value::Timestamp(t) => datetime::format_timestamp_styled(*t, ctx.date_style),
        Value::Timestamptz(t) => datetime::format_timestamptz_styled(*t, &ctx.tz, ctx.date_style),
        Value::Interval(i) => format_interval(i, ctx),
        Value::Uuid(u) => format_uuid(*u),
        Value::Jsonb(j) => j.to_string(),
        Value::Array(a) => format_array(a, ctx),
        Value::Record(fields) => format_record(fields, |_, f| format_plain(f, ctx)),
        Value::Bits(len, bytes) => bits_of(*len, bytes)
            .iter()
            .map(|b| if *b { '1' } else { '0' })
            .collect(),
        Value::Inet(b, m) => format_inet(b, *m, false),
        Value::Macaddr(m) => m
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":"),
        Value::Macaddr8(m) => m
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":"),
        Value::Geo(g) => super::geo::format(g, ctx.extra_float_digits),
        Value::TsVector(v) => super::tsearch::format_tsvector(v),
        Value::TsQuery(q) => super::tsearch::format_tsquery(q),
        Value::Money(c) => format_money(*c),
        Value::PgLsn(l) => format!("{:X}/{:X}", l >> 32, l & 0xffff_ffff),
        Value::Xid8(x) => x.to_string(),
        Value::Snapshot(s) => s.format(),
        Value::Tid(b, o) => format!("({b},{o})"),
        Value::Range(r) => super::range::format(r, ctx),
        Value::Multirange(m) => super::range::format_multi(m, ctx),
    }
}

pub fn format_interval(i: &Interval, ctx: &FormatCtx) -> String {
    datetime::format_interval_styled(i, ctx.interval_style)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats() {
        assert_eq!(format_float8(1e20, 1), "1e+20");
        assert_eq!(format_float8(1.5e-5, 1), "1.5e-05");
        assert_eq!(format_float8(0.1, 1), "0.1");
        assert_eq!(
            format_float8(123456789012345678.0, 1),
            "1.2345678901234568e+17"
        );
        assert_eq!(format_float8(1e15, 1), "1e+15");
        assert_eq!(format_float8(1e14, 1), "100000000000000");
        assert_eq!(format_float8(1e-4, 1), "0.0001");
        assert_eq!(format_float4(1e-5, 1), "1e-05");
        assert_eq!(format_float4(100000.0, 1), "100000");
        assert_eq!(format_float4(1000000.0, 1), "1e+06");
        assert_eq!(format_float4(0.1, 1), "0.1");
        assert_eq!(format_float8(-2.5, 1), "-2.5");
        assert_eq!(format_float8(f64::NAN, 1), "NaN");
        assert_eq!(format_float8(0.1 + 0.2, 1), "0.30000000000000004");
        assert_eq!(format_float8(0.1 + 0.2, 0), "0.3");
        assert_eq!(format_float8(1e20, 0), "1e+20");
    }

    #[test]
    fn ints() {
        assert_eq!(
            parse_int(" 42 ", "integer", i32::MIN as i64, i32::MAX as i64).unwrap(),
            42
        );
        assert_eq!(
            parse_int("1_000", "integer", i32::MIN as i64, i32::MAX as i64).unwrap(),
            1000
        );
        assert_eq!(
            parse_int("0x1F", "integer", i32::MIN as i64, i32::MAX as i64).unwrap(),
            31
        );
        assert_eq!(
            parse_int("-2147483648", "integer", i32::MIN as i64, i32::MAX as i64).unwrap(),
            -2147483648
        );
        let e = parse_int("2147483648", "integer", i32::MIN as i64, i32::MAX as i64).unwrap_err();
        assert_eq!(
            e.message,
            "value \"2147483648\" is out of range for type integer"
        );
        let e = parse_int("4x", "integer", i32::MIN as i64, i32::MAX as i64).unwrap_err();
        assert_eq!(e.message, "invalid input syntax for type integer: \"4x\"");
    }

    #[test]
    fn misc() {
        assert!(parse_bool("yes").unwrap());
        assert!(!parse_bool(" of").unwrap());
        assert!(parse_bool("o").is_err());
        assert_eq!(
            format_bytea(&parse_bytea("\\x0aff").unwrap(), true),
            "\\x0aff"
        );
        assert_eq!(parse_bytea("a\\\\b\\001").unwrap(), b"a\\b\x01");
        let u = parse_uuid("{A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11}").unwrap();
        assert_eq!(format_uuid(u), "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11");
        assert_eq!(
            format_uuid(parse_uuid("a0eebc999c0b4ef8bb6d6bb9bd380a11").unwrap()),
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"
        );
        assert!(parse_uuid("a0eebc99-9c0b4ef8-bb6d6bb9-bd380a11-").is_err());
        assert_eq!(
            &*apply_char_typmod("ab", &DataType::Bpchar(4 + 4), false).unwrap(),
            "ab  "
        );
        assert!(apply_char_typmod("abcdef", &DataType::Varchar(3 + 4), false).is_err());
        assert_eq!(
            &*apply_char_typmod("abcdef", &DataType::Varchar(3 + 4), true).unwrap(),
            "abc"
        );
        assert_eq!(format_money(-123456789), "-$1,234,567.89");
    }

    #[test]
    fn arrays() {
        let ctx = FormatCtx::default();
        let a = parse_value("{1,2,NULL}", &DataType::array_of(DataType::Int4), &ctx).unwrap();
        assert_eq!(format_plain(&a, &ctx), "{1,2,NULL}");
        let a = parse_value(
            r#"{"a b",c,"",NULL,"\"q\"","null"}"#,
            &DataType::array_of(DataType::Text),
            &ctx,
        )
        .unwrap();
        assert_eq!(
            format_plain(&a, &ctx),
            r#"{"a b",c,"",NULL,"\"q\"","null"}"#
        );
        let a = parse_value("{{1,2},{3,4}}", &DataType::array_of(DataType::Int4), &ctx).unwrap();
        assert_eq!(format_plain(&a, &ctx), "{{1,2},{3,4}}");
        let a = parse_value("[0:1]={7,8}", &DataType::array_of(DataType::Int4), &ctx).unwrap();
        assert_eq!(format_plain(&a, &ctx), "[0:1]={7,8}");
        assert!(parse_value("{{1},{2,3}}", &DataType::array_of(DataType::Int4), &ctx).is_err());
        assert!(parse_value("{1,2", &DataType::array_of(DataType::Int4), &ctx).is_err());
        let a = parse_value("{}", &DataType::array_of(DataType::Int4), &ctx).unwrap();
        assert_eq!(format_plain(&a, &ctx), "{}");
    }
}

/// int2vectorin / oidvectorin: numbers separated by spaces. An int2vector's bad element is
/// reported with the rest of the input from it; an oid's number may run straight into the
/// next item (uint32in_subr stops where the digits do).
fn parse_vector(s: &str, int2: bool) -> Result<Vec<Value>> {
    let tyname = if int2 { "smallint" } else { "oid" };
    let b = s.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            return Ok(out);
        }
        let rest = &s[i..];
        let mut j = i;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let digits = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j == digits {
            bail!(
                INVALID_TEXT_REPRESENTATION,
                "invalid input syntax for type {tyname}: \"{rest}\""
            );
        }
        let n: i128 = s[i..j].parse().unwrap_or(i128::MAX);
        if int2 {
            if !(i16::MIN as i128..=i16::MAX as i128).contains(&n) {
                bail!(
                    NUMERIC_VALUE_OUT_OF_RANGE,
                    "value \"{rest}\" is out of range for type smallint"
                );
            }
            if j < b.len() && b[j] != b' ' {
                bail!(
                    INVALID_TEXT_REPRESENTATION,
                    "invalid input syntax for type smallint: \"{rest}\""
                );
            }
            out.push(Value::Int2(n as i16));
        } else {
            // strtoul: a negative number wraps (within the 32-bit range).
            if n > u32::MAX as i128 || n < -(u32::MAX as i128) {
                bail!(
                    NUMERIC_VALUE_OUT_OF_RANGE,
                    "value \"{rest}\" is out of range for type oid"
                );
            }
            out.push(Value::Oid(n.rem_euclid(1 << 32) as u32));
        }
        i = j;
    }
}
