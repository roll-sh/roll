//! What the pgvector types share: musl's `strtof` / `strtol` (which the input functions
//! call, so their prefixes and range errors are the reference's), IEEE binary16 conversion
//! (`halfutils.h`), order-preserving key parts, the array and aggregate-state arguments,
//! the binary message reader, musl's `acos`, and the table of C functions.

use crate::Result;
use crate::error::{Error, code};
use crate::types::array::ArrayValue;
use crate::types::{DataType, Value};
use std::cmp::Ordering;
use std::sync::Arc;

/// The C functions of one source file, by symbol: `symbol` (the engine's natives) and
/// `plain` (the same functions without a call context, for tests).
macro_rules! natives {
    ($($name:literal => $f:path),* $(,)?) => {
        pub(super) fn symbol(sym: &str) -> Option<crate::ext::native::Native> {
            Some(crate::ext::native::Native::scalar(match sym {
                $($name => {
                    fn w(a: &[Value], _: &crate::sql::func::FnCtx) -> Result<Value> {
                        $f(a)
                    }
                    w
                })*
                _ => return None,
            }))
        }

        #[cfg(test)]
        pub(super) fn plain(sym: &str) -> Option<fn(&[Value]) -> Result<Value>> {
            Some(match sym {
                $($name => $f,)*
                _ => return None,
            })
        }
    };
}
pub(super) use natives;

// ── errors ──

pub(super) fn data_exception(msg: impl Into<String>) -> Error {
    Error::new(code::DATA_EXCEPTION, msg)
}

pub(super) fn limit_exceeded(msg: impl Into<String>) -> Error {
    Error::new(code::PROGRAM_LIMIT_EXCEEDED, msg)
}

/// `float_overflow_error`.
pub(super) fn overflow() -> Error {
    Error::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow")
}

/// `float_underflow_error`.
pub(super) fn underflow() -> Error {
    Error::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: underflow")
}

/// `CheckExpectedDim`.
pub(super) fn check_expected_dim(typmod: i32, dim: i32) -> Result<()> {
    if typmod != -1 && typmod != dim {
        return Err(data_exception(format!("expected {typmod} dimensions, not {dim}")));
    }
    Ok(())
}

/// `CheckDim` of type `name` with at most `max` dimensions.
pub(super) fn check_dim(dim: i64, name: &str, max: i64) -> Result<()> {
    if dim < 1 {
        return Err(data_exception(format!("{name} must have at least 1 dimension")));
    }
    if dim > max {
        return Err(limit_exceeded(format!("{name} cannot have more than {max} dimensions")));
    }
    Ok(())
}

/// `CheckElement` (NaN, then infinity) of type `name`.
pub(super) fn check_element(v: f32, name: &str) -> Result<()> {
    if v.is_nan() {
        return Err(data_exception(format!("NaN not allowed in {name}")));
    }
    if v.is_infinite() {
        return Err(data_exception(format!("infinite value not allowed in {name}")));
    }
    Ok(())
}

/// `invalid input syntax for type <name>: "<lit>"`, with a detail or not.
pub(super) fn bad_input(name: &str, lit: &str, detail: Option<&str>) -> Error {
    let e = Error::new(code::INVALID_TEXT_REPRESENTATION, format!("invalid input syntax for type {name}: \"{lit}\""));
    match detail {
        Some(d) => e.detail(d),
        None => e,
    }
}

// ── arguments ──

/// A value's bytes (pgvector's types are `Value::Ext`).
pub(super) fn bytes(v: &Value) -> &[u8] {
    match v {
        Value::Ext(_, b) => b,
        _ => &[],
    }
}

pub(super) fn int(v: &Value) -> i32 {
    v.as_i64().unwrap_or(0) as i32
}

pub(super) fn text(v: &Value) -> &str {
    crate::sql::func::text_arg(v)
}

pub(super) fn f32_at(b: &[u8], i: usize) -> f32 {
    f32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap())
}

pub(super) fn i32_at(b: &[u8], i: usize) -> i32 {
    i32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap())
}

pub(super) fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[2 * i], b[2 * i + 1]])
}

/// The elements of an array argument of a cast to one of the types (`array_to_vector`
/// and the others): 1-D, without NULLs.
pub(super) fn array_items(v: &Value) -> Result<&ArrayValue> {
    let Value::Array(a) = v else {
        return Err(crate::error::internal("array argument expected"));
    };
    if a.dims.len() > 1 {
        return Err(data_exception("array must be 1-D"));
    }
    if a.items.iter().any(Value::is_null) {
        return Err(Error::new(code::NULL_VALUE_NOT_ALLOWED, "array must not contain nulls"));
    }
    Ok(a)
}

/// One element of an int4[] / float8[] / float4[] / numeric[] as the C code's `float` (an
/// int4 or float8 converted, a numeric through `numeric_float4`).
pub(super) fn element_f32(elem: &DataType, v: &Value) -> Result<f32> {
    Ok(match (elem, v) {
        (DataType::Int4, Value::Int4(i)) => *i as f32,
        (DataType::Float8, Value::Float8(f)) => *f as f32,
        (DataType::Float4, Value::Float4(f)) => *f,
        (DataType::Numeric(_), Value::Numeric(_)) => {
            let ctx = crate::types::text::FormatCtx::default();
            match crate::types::cast::cast_value(v.clone(), elem, &DataType::Float4, &ctx, true)? {
                Value::Float4(f) => f,
                _ => return Err(crate::error::internal("numeric_float4 failed")),
            }
        }
        _ => return Err(data_exception("unsupported array type")),
    })
}

/// A float8[] aggregate state (`CheckStateArray`): its values.
pub(super) fn state(v: &Value, caller: &str) -> Result<Vec<f64>> {
    let bad = || crate::error::internal(format!("{caller}: expected state array"));
    let Value::Array(a) = v else { return Err(bad()) };
    if a.dims.len() != 1 || a.items.is_empty() || !matches!(a.elem, DataType::Float8) {
        return Err(bad());
    }
    a.items
        .iter()
        .map(|x| match x {
            Value::Float8(f) => Ok(*f),
            _ => Err(bad()),
        })
        .collect()
}

pub(super) fn float8_array(v: Vec<f64>) -> Value {
    let items = v.into_iter().map(Value::Float8).collect();
    Value::Array(Arc::new(ArrayValue::from_vec(DataType::Float8, items)))
}

pub(super) fn float4_array(v: impl Iterator<Item = f32>) -> Value {
    let items = v.map(Value::Float4).collect();
    Value::Array(Arc::new(ArrayValue::from_vec(DataType::Float4, items)))
}

// ── type modifiers ──

/// `*_typmod_in` over the modifiers (`ArrayGetIntegerTypmods`, then pgvector's checks).
pub(super) fn typmod_in(mods: &[i64], name: &str, max: i64) -> Result<i32> {
    let bad = |m: String| Error::new(code::INVALID_PARAMETER_VALUE, m);
    if mods.len() != 1 {
        return Err(bad("invalid type modifier".into()));
    }
    let t = mods[0];
    if t < i32::MIN as i64 || t > i32::MAX as i64 {
        return Err(Error::new(code::NUMERIC_VALUE_OUT_OF_RANGE, format!("value \"{t}\" is out of range for type integer")));
    }
    if t < 1 {
        return Err(bad(format!("dimensions for type {name} must be at least 1")));
    }
    if t > max {
        return Err(bad(format!("dimensions for type {name} cannot exceed {max}")));
    }
    Ok(t as i32)
}

/// The SQL-callable `*_typmod_in(cstring[])`.
pub(super) fn typmod_in_sql(a: &Value, name: &str, max: i64) -> Result<Value> {
    let Value::Array(arr) = a else {
        return Err(crate::error::internal("typmod array must be type cstring[]"));
    };
    if arr.dims.len() != 1 {
        return Err(Error::new(code::ARRAY_SUBSCRIPT_ERROR, "typmod array must be one-dimensional"));
    }
    let mut mods = Vec::with_capacity(arr.items.len());
    for x in &arr.items {
        if x.is_null() {
            return Err(Error::new(code::NULL_VALUE_NOT_ALLOWED, "typmod array must not contain nulls"));
        }
        mods.push(crate::types::text::parse_int(text(x), "integer", i32::MIN as i64, i32::MAX as i64)?);
    }
    typmod_in(&mods, name, max).map(Value::Int4)
}

// ── text ──

/// `scanner_isspace` (and the C locale's `isspace`).
pub(super) fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// `float_to_shortest_decimal_bufn` (whatever extra_float_digits): the fewest significant
/// digits strictly between the midpoints to the neighbouring floats (Ryu's interval, its
/// bounds excluded as PostgreSQL's build has them), the closest of those.
pub(super) fn fmt_f32(v: f32) -> String {
    if !v.is_finite() {
        return crate::types::text::format_float4(v, 1);
    }
    let b = v.to_bits();
    let (eb, frac) = ((b >> 23) & 0xff, (b & 0x7f_ffff) as u64);
    let (m, e) = if eb == 0 { (frac, -149) } else { (frac | 0x80_0000, eb as i32 - 150) };
    let fp = Fp { m, e, asym: frac == 0 && eb > 1, digits: 9, max_fixed: 6 };
    let a = v.abs();
    shortest(v.is_sign_negative(), &fp, &|p| match p {
        None => format!("{a:e}"),
        Some(p) => format!("{a:.p$e}"),
    })
}

/// `double_to_shortest_decimal_bufn` (float8out with extra_float_digits > 0), as
/// `fmt_f32`. (The engine's float8out differs for powers of two.)
#[cfg(test)]
pub(super) fn fmt_f64(v: f64) -> String {
    if !v.is_finite() {
        return crate::types::text::format_float8(v, 1);
    }
    let b = v.to_bits();
    let (eb, frac) = ((b >> 52) & 0x7ff, b & 0xf_ffff_ffff_ffff);
    let (m, e) = if eb == 0 { (frac, -1074) } else { (frac | 1 << 52, eb as i32 - 1075) };
    let fp = Fp { m, e, asym: frac == 0 && eb > 1, digits: 17, max_fixed: 15 };
    let a = v.abs();
    shortest(v.is_sign_negative(), &fp, &|p| match p {
        None => format!("{a:e}"),
        Some(p) => format!("{a:.p$e}"),
    })
}

/// A positive float: m × 2^e, whether the gap below it is half the gap above (a power of
/// two), the most digits it needs, and the decimal exponents written without one.
struct Fp {
    m: u64,
    e: i32,
    asym: bool,
    digits: usize,
    max_fixed: i32,
}

impl Fp {
    /// Its interval's bounds as (odd, power of two): the upper, then the lower.
    fn bounds(&self) -> ((u64, i32), (u64, i32)) {
        let (m, e) = (self.m, self.e);
        let lower = if self.asym { (4 * m - 1, e - 2) } else { (2 * m - 1, e - 1) };
        ((2 * m + 1, e - 1), lower)
    }

    fn inside(&self, d: u64, e: i32) -> bool {
        let (hi, lo) = self.bounds();
        cmp_exact(d, e, lo.0, lo.1).is_gt() && cmp_exact(d, e, hi.0, hi.1).is_lt()
    }
}

/// The shortest text of a float: Rust's shortest digits (`fmt(None)`, `{:e}`) are Ryu's
/// except that Rust takes a bound when the mantissa is even; then a search from one digit
/// up, of the nearest decimal (`fmt(Some(digits - 1))`) and the one on the other side
/// (where the interval is wider).
fn shortest(neg: bool, fp: &Fp, fmt: &dyn Fn(Option<usize>) -> String) -> String {
    if fp.m == 0 {
        return if neg { "-0" } else { "0" }.into();
    }
    let (d, e) = decimal(&fmt(None));
    // (A bound below 2^-14 (2^-25) is an odd multiple of it with more than 9 (17) digits.)
    let small = if fp.digits == 9 { -14 } else { -25 };
    let (hi, lo) = fp.bounds();
    let on_bound =
        fp.m & 1 == 0 && lo.1 >= small && (cmp_exact(d, e, hi.0, hi.1).is_eq() || cmp_exact(d, e, lo.0, lo.1).is_eq());
    if !on_bound {
        // A float halfway between two shortest decimals: Ryu takes the even one, Rust the
        // one above. (The float then has one digit more, so it is an odd multiple of 2^-14
        // (2^-25) or more.)
        let odd_e = fp.e + fp.m.trailing_zeros() as i32;
        let tie = |n: u64| odd_e >= small && cmp_exact(n, e, fp.m, fp.e + 1).is_eq();
        let pair = if d > 0 && tie(2 * d - 1) {
            Some(d - 1)
        } else if tie(2 * d + 1) {
            Some(d + 1)
        } else {
            None
        };
        if let Some(o) = pair
            && o % 2 == 0
            && fp.inside(o, e)
        {
            return layout(neg, o, e, fp.max_fixed);
        }
        return layout(neg, d, e, fp.max_fixed);
    }
    for k in 1..=fp.digits {
        let (d, e) = decimal(&fmt(Some(k - 1)));
        if fp.inside(d, e) {
            return layout(neg, d, e, fp.max_fixed);
        }
        let other = if cmp_exact(d, e, fp.m, fp.e).is_lt() { d + 1 } else { d - 1 };
        if fp.inside(other, e) {
            return layout(neg, other, e, fp.max_fixed);
        }
    }
    let (d, e) = decimal(&fmt(Some(fp.digits - 1)));
    layout(neg, d, e, fp.max_fixed)
}

/// Digits and the exponent of their last one, from Rust's `{:e}` text.
fn decimal(s: &str) -> (u64, i32) {
    let (m, x) = s.split_once('e').unwrap();
    let digits: String = m.chars().filter(char::is_ascii_digit).collect();
    let x: i32 = x.parse().unwrap();
    (digits.parse().unwrap(), x - (digits.len() as i32 - 1))
}

/// `d` × 10^`e` against `n` × 2^`p`, exactly.
fn cmp_exact(d: u64, e: i32, n: u64, p: i32) -> Ordering {
    use num_bigint::BigUint;
    let (mut l, mut r) = (BigUint::from(d), BigUint::from(n));
    if e >= 0 {
        l *= BigUint::from(10u32).pow(e as u32);
    } else {
        r *= BigUint::from(10u32).pow((-e) as u32);
    }
    if p >= 0 {
        r <<= p as usize;
    } else {
        l <<= (-p) as usize;
    }
    l.cmp(&r)
}

/// PostgreSQL's layout of digits `d` × 10^`e`: exponential below 1e-4 and from
/// 10^`max_fixed`.
fn layout(neg: bool, d: u64, e: i32, max_fixed: i32) -> String {
    let ds = d.to_string();
    let ds = ds.trim_end_matches('0');
    let e = e + (d.to_string().len() - ds.len()) as i32;
    let x = e + ds.len() as i32 - 1;
    let mut out = String::with_capacity(16);
    if neg {
        out.push('-');
    }
    if !(-4..max_fixed).contains(&x) {
        out.push_str(&ds[..1]);
        if ds.len() > 1 {
            out.push('.');
            out.push_str(&ds[1..]);
        }
        out.push_str(&format!("e{}{:02}", if x < 0 { '-' } else { '+' }, x.abs()));
    } else if x < 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-x - 1) as usize));
        out.push_str(ds);
    } else if ds.len() as i32 <= x + 1 {
        out.push_str(ds);
        out.extend(std::iter::repeat_n('0', (x + 1) as usize - ds.len()));
    } else {
        out.push_str(&ds[..x as usize + 1]);
        out.push('.');
        out.push_str(&ds[x as usize + 1..]);
    }
    out
}

/// What musl's `strtof` read at the start of a string.
pub(super) struct Strtof {
    pub value: f32,
    /// The bytes it consumed (`stringEnd - pt`).
    pub len: usize,
    /// It set errno to ERANGE.
    pub erange: bool,
}

/// musl's `strtof` (`__floatscan`): leading white space, a sign, then `inf` / `infinity`,
/// `nan` / `nan(chars)`, a hexadecimal float (`0x1.8p3`) or a decimal one, correctly
/// rounded. `None` when it converts nothing.
pub(super) fn strtof(s: &[u8]) -> Option<Strtof> {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut i = 0;
    while is_space(at(i)) {
        i += 1;
    }
    let neg = at(i) == b'-';
    if matches!(at(i), b'+' | b'-') {
        i += 1;
    }
    let signed = |v: f32| if neg { -v } else { v };
    let done = |value: f32, len: usize, erange: bool| Some(Strtof { value, len, erange });
    let k = (0..8).take_while(|&k| at(i + k) | 32 == b"infinity"[k]).count();
    if k >= 3 {
        return done(signed(f32::INFINITY), i + if k == 8 { 8 } else { 3 }, false);
    }
    if k > 0 {
        return None;
    }
    let n = (0..3).take_while(|&k| at(i + k) | 32 == b"nan"[k]).count();
    if n == 3 {
        let mut j = i + 3;
        if at(j) == b'(' {
            let mut e = j + 1;
            while at(e).is_ascii_alphanumeric() || at(e) == b'_' {
                e += 1;
            }
            if at(e) == b')' {
                j = e + 1;
            }
        }
        return done(f32::NAN, j, false);
    }
    if n > 0 {
        return None;
    }
    if at(i) == b'0' && at(i + 1) | 32 == b'x' {
        return Some(match hexfloat(s, i + 2, neg) {
            Some((value, end, erange)) => Strtof { value, len: end, erange },
            // "0x" without digits: the "0".
            None => Strtof { value: signed(0.0), len: i + 1, erange: false },
        });
    }
    // A decimal float: digits with at most one radix point, then an exponent if one is
    // complete.
    let start = i;
    let (mut gotdig, mut gotrad, mut nonzero) = (false, false, false);
    while at(i).is_ascii_digit() || (at(i) == b'.' && !gotrad) {
        if at(i) == b'.' {
            gotrad = true;
        } else {
            gotdig = true;
            nonzero |= at(i) != b'0';
        }
        i += 1;
    }
    if !gotdig {
        return None;
    }
    if at(i) | 32 == b'e'
        && let Some((_, end)) = scanexp(s, i + 1)
    {
        i = end;
    }
    let text = std::str::from_utf8(&s[start..i]).ok()?;
    let v: f32 = text.parse().ok()?;
    // (musl sets ERANGE on overflow and on underflow to zero.)
    let erange = v.is_infinite() || (v == 0.0 && nonzero);
    done(signed(v), i, erange)
}

/// The exponent of a float (`scanexp`) at `i`: its value (saturated) and end, if it has
/// digits.
fn scanexp(s: &[u8], mut i: usize) -> Option<(i64, usize)> {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let neg = at(i) == b'-';
    if matches!(at(i), b'+' | b'-') {
        i += 1;
    }
    if !at(i).is_ascii_digit() {
        return None;
    }
    let mut y: i64 = 0;
    while at(i).is_ascii_digit() {
        y = y.saturating_mul(10).saturating_add((at(i) - b'0') as i64).min(i64::MAX / 100);
        i += 1;
    }
    Some((if neg { -y } else { y }, i))
}

/// musl's `hexfloat` after the `0x` at `i`: the value, the end, and whether it set ERANGE
/// (an exponent far out of range, or a result of zero; not an overflow by rounding);
/// `None` without digits.
fn hexfloat(s: &[u8], mut i: usize, neg: bool) -> Option<(f32, usize, bool)> {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let signed = |v: f32| if neg { -v } else { v };
    let (mut gotdig, mut gotrad) = (false, false);
    // rp: the radix point's position, in hex digits from the first significant one.
    let (mut rp, mut dc): (i64, i64) = (0, 0);
    while at(i) == b'0' {
        gotdig = true;
        i += 1;
    }
    if at(i) == b'.' {
        gotrad = true;
        i += 1;
        while at(i) == b'0' {
            gotdig = true;
            rp -= 1;
            i += 1;
        }
    }
    // The first 32 significant digits, and whether a later one isn't zero.
    let (mut m, mut sticky) = (0u128, false);
    loop {
        let c = at(i);
        if c == b'.' {
            if gotrad {
                break;
            }
            rp = dc;
            gotrad = true;
        } else if let Some(d) = (c as char).to_digit(16) {
            gotdig = true;
            if dc < 32 {
                m = m * 16 + d as u128;
            } else if d != 0 {
                sticky = true;
            }
            dc += 1;
        } else {
            break;
        }
        i += 1;
    }
    if !gotdig {
        return None;
    }
    if !gotrad {
        rp = dc;
    }
    let mut p = 0;
    if at(i) | 32 == b'p'
        && let Some((e, end)) = scanexp(s, i + 1)
    {
        p = e;
        i = end;
    }
    if m == 0 {
        return Some((signed(0.0), i, false));
    }
    // musl's range checks are on the exponent of its leading 32 bits.
    let e2 = p.saturating_add(4 * rp - 32);
    if e2 > 149 {
        return Some((signed(f32::INFINITY), i, true));
    }
    if e2 < -149 - 128 {
        return Some((signed(0.0), i, true));
    }
    let e = p + 4 * (rp - dc.min(32));
    // Below half the smallest subnormal (no bit of precision left), musl rounds its leading
    // 32 bits to a multiple of 2^32 (through a bias that is subtracted again, so a zero is
    // +0 and ERANGE), then converts what is left to float (a signed zero, no ERANGE).
    let nb = 128 - m.leading_zeros() as i64;
    let e2 = e + nb - 32;
    if 32 + e2 + 149 <= 0 {
        let (x, rest) = if nb > 32 { (m >> (nb - 32), m & ((1 << (nb - 32)) - 1) != 0) } else { (m << (32 - nb), false) };
        if x < 1 << 31 || (x == 1 << 31 && !rest && !sticky) {
            return Some((0.0, i, true));
        }
        return Some((signed(2f64.powi((32 + e2) as i32) as f32), i, false));
    }
    Some((signed(round_f32(m, e, sticky)), i, false))
}

/// `m` * 2^`e` (plus something below m's last bit when `sticky`), rounded to the nearest
/// float, ties to even.
fn round_f32(m: u128, e: i64, sticky: bool) -> f32 {
    let nbits = 128 - m.leading_zeros() as i64;
    let top = e + nbits - 1;
    if top > 127 {
        return f32::INFINITY;
    }
    let lsb = (top - 23).max(-149);
    let shift = lsb - e;
    let q: u128 = if shift <= 0 {
        m << (-shift)
    } else if shift >= 128 {
        0
    } else {
        let q = m >> shift;
        let rem = m & ((1u128 << shift) - 1);
        let half = 1u128 << (shift - 1);
        if rem > half || (rem == half && (sticky || q & 1 == 1)) { q + 1 } else { q }
    };
    ((q as f64) * 2f64.powi(lsb as i32)) as f32
}

/// musl's `strtol` (base 10): its value (saturated to the range of `long`) and the bytes
/// consumed; `None` without digits.
pub(super) fn strtol(s: &[u8]) -> Option<(i64, usize)> {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut i = 0;
    while is_space(at(i)) {
        i += 1;
    }
    let neg = at(i) == b'-';
    if matches!(at(i), b'+' | b'-') {
        i += 1;
    }
    if !at(i).is_ascii_digit() {
        return None;
    }
    let mut v: i128 = 0;
    while at(i).is_ascii_digit() {
        v = (v * 10 + (at(i) - b'0') as i128).min(1 << 64);
        i += 1;
    }
    let v = if neg { -v } else { v };
    Some((v.clamp(i64::MIN as i128, i64::MAX as i128) as i64, i))
}

// ── binary16 (halfutils.h, the build without F16C or _Float16) ──

/// `HalfToFloat4` (exact).
pub(crate) fn half_to_f32(h: u16) -> f32 {
    let sign = ((h & 0x8000) as u32) << 16;
    let exp = ((h >> 10) & 0x1f) as u32;
    let man = (h & 0x3ff) as u32;
    let bits = match exp {
        31 if man == 0 => sign | 0x7f80_0000,
        31 => sign | 0x7fc0_0000 | man << 13,
        0 if man == 0 => sign,
        0 => {
            let v = man as f32 * (1.0 / 16_777_216.0);
            return if sign != 0 { -v } else { v };
        }
        _ => sign | (exp + 112) << 23 | man << 13,
    };
    f32::from_bits(bits)
}

/// `Float4ToHalfUnchecked`: to the nearest half, ties to even (an overflow is infinite).
pub(crate) fn f32_to_half(num: f32) -> u16 {
    let bin = num.to_bits();
    let mut exponent = ((bin & 0x7f80_0000) >> 23) as i32;
    let mut mantissa = (bin & 0x007f_ffff) as i32;
    let mut result = ((bin & 0x8000_0000) >> 16) as u16;
    if num.is_infinite() {
        result |= 0x7c00;
    } else if num.is_nan() {
        result |= 0x7e00 | (mantissa >> 13) as u16;
    } else if exponent > 98 {
        exponent -= 127;
        let mut s = mantissa & 0xfff;
        // Subnormal
        if exponent < -14 {
            let diff = -exponent - 14;
            mantissa >>= diff;
            mantissa += 1 << (23 - diff);
            s |= mantissa & 0xfff;
        }
        let mut m = mantissa >> 13;
        let gr = (mantissa >> 12) % 4;
        if gr == 3 || (gr == 1 && s != 0) {
            m += 1;
        }
        if m == 1024 {
            m = 0;
            exponent += 1;
        }
        if exponent > 15 {
            result |= 0x7c00;
        } else {
            if exponent >= -14 {
                result |= ((exponent + 15) << 10) as u16;
            }
            result |= m as u16;
        }
    }
    result
}

pub(super) fn half_is_nan(h: u16) -> bool {
    h & 0x7c00 == 0x7c00 && h & 0x7fff != 0x7c00
}

pub(super) fn half_is_inf(h: u16) -> bool {
    h & 0x7fff == 0x7c00
}

pub(super) fn half_is_zero(h: u16) -> bool {
    h & 0x7fff == 0
}

/// `Float4ToHalf`: an error when a finite float overflows the half's range.
pub(super) fn f32_to_half_checked(num: f32) -> Result<u16> {
    let h = f32_to_half(num);
    if half_is_inf(h) && !num.is_infinite() {
        return Err(Error::new(
            code::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("\"{}\" is out of range for type halfvec", fmt_f32(num)),
        ));
    }
    Ok(h)
}

// ── keys ──

/// A float's order-preserving key (the zeros as one).
pub(super) fn f32_key(v: f32) -> [u8; 4] {
    let bits = if v == 0.0 { 0 } else { v.to_bits() };
    let k = if bits >> 31 == 1 { !bits } else { bits | 0x8000_0000 };
    k.to_be_bytes()
}

// ── binary messages ──

/// A binary value being received (`pq_getmsgint`, `pq_getmsgfloat4`).
pub(super) struct Msg<'a>(pub &'a [u8]);

impl Msg<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        if self.0.len() < N {
            return Err(Error::new(code::PROTOCOL_VIOLATION, "insufficient data left in message"));
        }
        let (h, t) = self.0.split_at(N);
        self.0 = t;
        Ok(h.try_into().unwrap())
    }

    pub fn i16(&mut self) -> Result<i16> {
        self.take().map(i16::from_be_bytes)
    }

    pub fn u16(&mut self) -> Result<u16> {
        self.take().map(u16::from_be_bytes)
    }

    pub fn i32(&mut self) -> Result<i32> {
        self.take().map(i32::from_be_bytes)
    }

    pub fn f32(&mut self) -> Result<f32> {
        self.take().map(f32::from_be_bytes)
    }
}
