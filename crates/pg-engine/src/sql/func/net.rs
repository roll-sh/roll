//! inet/cidr and macaddr/macaddr8 functions and operators (Postgres's network.c, mac.c, mac8.c).

use super::{FnCtx, Registry};
use crate::bail;
use crate::error::Result;
use crate::types::text::{abbrev_cidr, format_inet, inet_host, inet_network};
use crate::types::value::bitncmp;
use crate::types::{DataType as T, Value};
use std::cmp::Ordering;

fn inet(v: &Value) -> (&[u8], u8) {
    match v {
        Value::Inet(a, m) => (a, *m),
        _ => (&[], 0),
    }
}

fn max_bits(a: &[u8]) -> u8 {
    (a.len() * 8) as u8
}

fn inet_value(a: Vec<u8>, bits: u8) -> Value {
    Value::Inet(a.into(), bits)
}

/// The mask itself: `bits` leading ones over the address width.
fn mask_bytes(len: usize, bits: u8) -> Vec<u8> {
    inet_network(&vec![0xff; len], bits)
}

fn int(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

fn host(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Text(inet_host(inet(&a[0]).0).into()))
}

fn masklen(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(inet(&a[0]).1 as i32))
}

fn family(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(if inet(&a[0]).0.len() == 4 { 4 } else { 6 }))
}

fn netmask(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (addr, bits) = inet(&a[0]);
    Ok(inet_value(mask_bytes(addr.len(), bits), max_bits(addr)))
}

fn hostmask(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (addr, bits) = inet(&a[0]);
    Ok(inet_value(
        mask_bytes(addr.len(), bits).iter().map(|b| !b).collect(),
        max_bits(addr),
    ))
}

fn network(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (addr, bits) = inet(&a[0]);
    Ok(inet_value(inet_network(addr, bits), bits))
}

fn broadcast(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (addr, bits) = inet(&a[0]);
    let m = mask_bytes(addr.len(), bits);
    Ok(inet_value(
        addr.iter().zip(&m).map(|(x, m)| x | !m).collect(),
        bits,
    ))
}

/// `text(inet)`: always with the /width.
fn network_show(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (addr, bits) = inet(&a[0]);
    Ok(Value::Text(format_inet(addr, bits, true).into()))
}

fn inet_abbrev(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (addr, bits) = inet(&a[0]);
    Ok(Value::Text(format_inet(addr, bits, false).into()))
}

fn cidr_abbrev(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (addr, bits) = inet(&a[0]);
    Ok(Value::Text(abbrev_cidr(addr, bits).into()))
}

fn checked_masklen(addr: &[u8], n: i64) -> Result<u8> {
    let n = if n == -1 { max_bits(addr) as i64 } else { n };
    if n < 0 || n > max_bits(addr) as i64 {
        bail!(INVALID_PARAMETER_VALUE, "invalid mask length: {n}");
    }
    Ok(n as u8)
}

fn inet_set_masklen(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (addr, _) = inet(&a[0]);
    Ok(inet_value(
        addr.to_vec(),
        checked_masklen(addr, int(&a[1]))?,
    ))
}

fn cidr_set_masklen(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (addr, _) = inet(&a[0]);
    let bits = checked_masklen(addr, int(&a[1]))?;
    Ok(inet_value(inet_network(addr, bits), bits))
}

fn inet_same_family(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(inet(&a[0]).0.len() == inet(&a[1]).0.len()))
}

/// The number of leading bits two addresses share, up to `n` (`bitncommon`).
fn bitncommon(a: &[u8], b: &[u8], n: u8) -> u8 {
    (0..n)
        .find(|&i| bitncmp(a, b, i as usize + 1) != Ordering::Equal)
        .unwrap_or(n)
}

fn inet_merge(a: &[Value], _: &FnCtx) -> Result<Value> {
    let ((x, mx), (y, my)) = (inet(&a[0]), inet(&a[1]));
    if x.len() != y.len() {
        bail!(
            INVALID_PARAMETER_VALUE,
            "cannot merge addresses from different families"
        );
    }
    let bits = bitncommon(x, y, mx.min(my));
    Ok(inet_value(inet_network(x, bits), bits))
}

/// `network_sub` and friends: same family, mask lengths compared by `ord`, prefixes equal.
fn contains(a: &[Value], ord: fn(u8, u8) -> bool, over_left: bool) -> Value {
    let ((x, mx), (y, my)) = (inet(&a[0]), inet(&a[1]));
    let n = if over_left { mx } else { my };
    Value::Bool(x.len() == y.len() && ord(mx, my) && bitncmp(x, y, n as usize) == Ordering::Equal)
}

fn network_sub(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(contains(a, |l, r| l > r, false))
}

fn network_subeq(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(contains(a, |l, r| l >= r, false))
}

fn network_sup(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(contains(a, |l, r| l < r, true))
}

fn network_supeq(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(contains(a, |l, r| l <= r, true))
}

fn network_overlap(a: &[Value], _: &FnCtx) -> Result<Value> {
    let ((x, mx), (y, my)) = (inet(&a[0]), inet(&a[1]));
    Ok(Value::Bool(
        x.len() == y.len() && bitncmp(x, y, mx.min(my) as usize) == Ordering::Equal,
    ))
}

fn inetnot(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (addr, bits) = inet(&a[0]);
    Ok(inet_value(addr.iter().map(|b| !b).collect(), bits))
}

fn bitwise(a: &[Value], what: &str, f: fn(u8, u8) -> u8) -> Result<Value> {
    let ((x, mx), (y, my)) = (inet(&a[0]), inet(&a[1]));
    if x.len() != y.len() {
        bail!(
            INVALID_PARAMETER_VALUE,
            "cannot {what} inet values of different sizes"
        );
    }
    Ok(inet_value(
        x.iter().zip(y).map(|(p, q)| f(*p, *q)).collect(),
        mx.max(my),
    ))
}

fn inetand(a: &[Value], _: &FnCtx) -> Result<Value> {
    bitwise(a, "AND", |p, q| p & q)
}

fn inetor(a: &[Value], _: &FnCtx) -> Result<Value> {
    bitwise(a, "OR", |p, q| p | q)
}

/// `internal_inetpl`: byte-wise addition with the carry, overflow out of the address an error.
fn inetpl(v: &Value, mut addend: i64) -> Result<Value> {
    let (addr, bits) = inet(v);
    let mut out = addr.to_vec();
    let mut carry: i64 = 0;
    for b in out.iter_mut().rev() {
        carry += *b as i64 + (addend & 0xff);
        *b = (carry & 0xff) as u8;
        carry >>= 8;
        addend &= !0xff;
        addend /= 0x100;
    }
    if !((addend == 0 && carry == 0) || (addend == -1 && carry == 1)) {
        bail!(NUMERIC_VALUE_OUT_OF_RANGE, "result is out of range");
    }
    Ok(inet_value(out, bits))
}

fn inet_pl_int8(a: &[Value], _: &FnCtx) -> Result<Value> {
    inetpl(&a[0], int(&a[1]))
}

fn int8_pl_inet(a: &[Value], _: &FnCtx) -> Result<Value> {
    inetpl(&a[1], int(&a[0]))
}

fn inet_mi_int8(a: &[Value], _: &FnCtx) -> Result<Value> {
    inetpl(&a[0], int(&a[1]).wrapping_neg())
}

/// `inetmi`: the difference as int8, two's complement over the whole address.
fn inetmi(a: &[Value], _: &FnCtx) -> Result<Value> {
    let ((x, _), (y, _)) = (inet(&a[0]), inet(&a[1]));
    if x.len() != y.len() {
        bail!(
            INVALID_PARAMETER_VALUE,
            "cannot subtract inet values of different sizes"
        );
    }
    let (mut res, mut carry) = (0i64, 1i32);
    for (byte, (p, q)) in x.iter().rev().zip(y.iter().rev()).enumerate() {
        carry += *p as i32 + (!*q) as i32;
        let lobyte = carry & 0xff;
        if byte < 8 {
            res |= (lobyte as i64) << (byte * 8);
        } else if if res < 0 { lobyte != 0xff } else { lobyte != 0 } {
            bail!(NUMERIC_VALUE_OUT_OF_RANGE, "result is out of range");
        }
        carry >>= 8;
    }
    if carry == 0 && x.len() < 8 {
        res |= -1i64 << (x.len() * 8);
    }
    Ok(Value::Int8(res))
}

// ── macaddr / macaddr8 ──

fn mac(v: &Value) -> [u8; 6] {
    match v {
        Value::Macaddr(m) => *m,
        _ => [0; 6],
    }
}

fn mac8(v: &Value) -> [u8; 8] {
    match v {
        Value::Macaddr8(m) => *m,
        _ => [0; 8],
    }
}

fn macaddr_trunc(a: &[Value], _: &FnCtx) -> Result<Value> {
    let m = mac(&a[0]);
    Ok(Value::Macaddr([m[0], m[1], m[2], 0, 0, 0]))
}

fn macaddr_not(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Macaddr(mac(&a[0]).map(|b| !b)))
}

fn macaddr_and(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (x, y) = (mac(&a[0]), mac(&a[1]));
    Ok(Value::Macaddr(std::array::from_fn(|i| x[i] & y[i])))
}

fn macaddr_or(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (x, y) = (mac(&a[0]), mac(&a[1]));
    Ok(Value::Macaddr(std::array::from_fn(|i| x[i] | y[i])))
}

fn macaddr8_trunc(a: &[Value], _: &FnCtx) -> Result<Value> {
    let m = mac8(&a[0]);
    Ok(Value::Macaddr8([m[0], m[1], m[2], 0, 0, 0, 0, 0]))
}

fn macaddr8_not(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Macaddr8(mac8(&a[0]).map(|b| !b)))
}

fn macaddr8_and(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (x, y) = (mac8(&a[0]), mac8(&a[1]));
    Ok(Value::Macaddr8(std::array::from_fn(|i| x[i] & y[i])))
}

fn macaddr8_or(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (x, y) = (mac8(&a[0]), mac8(&a[1]));
    Ok(Value::Macaddr8(std::array::from_fn(|i| x[i] | y[i])))
}

/// Sets the 7th bit (universal/local) of the first byte: a modified EUI-64 for IPv6.
fn macaddr8_set7bit(a: &[Value], _: &FnCtx) -> Result<Value> {
    let mut m = mac8(&a[0]);
    m[0] |= 0x02;
    Ok(Value::Macaddr8(m))
}

pub fn register(r: &mut Registry) {
    use T::{Bool, Cidr, Inet, Int4, Int8, Macaddr, Macaddr8, Text};
    r.func("host", vec![Inet], Text, host);
    r.func("masklen", vec![Inet], Int4, masklen);
    r.func("family", vec![Inet], Int4, family);
    r.func("netmask", vec![Inet], Inet, netmask);
    r.func("hostmask", vec![Inet], Inet, hostmask);
    r.func("network", vec![Inet], Cidr, network);
    r.func("broadcast", vec![Inet], Inet, broadcast);
    r.func("text", vec![Inet], Text, network_show);
    r.func("abbrev", vec![Inet], Text, inet_abbrev);
    r.func("abbrev", vec![Cidr], Text, cidr_abbrev);
    r.func("set_masklen", vec![Inet, Int4], Inet, inet_set_masklen);
    r.func("set_masklen", vec![Cidr, Int4], Cidr, cidr_set_masklen);
    r.func("cidr", vec![Inet], Cidr, network);
    r.func("inet_same_family", vec![Inet, Inet], Bool, inet_same_family);
    r.func("inet_merge", vec![Inet, Inet], Cidr, inet_merge);
    for (op, name, f) in [
        ("<<", "network_sub", network_sub as super::FnImpl),
        ("<<=", "network_subeq", network_subeq),
        (">>", "network_sup", network_sup),
        (">>=", "network_supeq", network_supeq),
        ("&&", "network_overlap", network_overlap),
    ] {
        r.func(name, vec![Inet, Inet], Bool, f);
        r.binop(op, Inet, Inet, Bool, f);
    }
    r.func("inetnot", vec![Inet], Inet, inetnot);
    r.prefix_op("~", Inet, Inet, inetnot);
    r.func("inetand", vec![Inet, Inet], Inet, inetand);
    r.binop("&", Inet, Inet, Inet, inetand);
    r.func("inetor", vec![Inet, Inet], Inet, inetor);
    r.binop("|", Inet, Inet, Inet, inetor);
    r.func("inetpl", vec![Inet, Int8], Inet, inet_pl_int8);
    r.binop("+", Inet, Int8, Inet, inet_pl_int8);
    r.func("int8pl_inet", vec![Int8, Inet], Inet, int8_pl_inet);
    r.binop("+", Int8, Inet, Inet, int8_pl_inet);
    r.func("inetmi_int8", vec![Inet, Int8], Inet, inet_mi_int8);
    r.binop("-", Inet, Int8, Inet, inet_mi_int8);
    r.func("inetmi", vec![Inet, Inet], Int8, inetmi);
    r.binop("-", Inet, Inet, Int8, inetmi);

    r.func("trunc", vec![Macaddr], Macaddr, macaddr_trunc);
    r.func("macaddr_not", vec![Macaddr], Macaddr, macaddr_not);
    r.prefix_op("~", Macaddr, Macaddr, macaddr_not);
    r.func("macaddr_and", vec![Macaddr, Macaddr], Macaddr, macaddr_and);
    r.binop("&", Macaddr, Macaddr, Macaddr, macaddr_and);
    r.func("macaddr_or", vec![Macaddr, Macaddr], Macaddr, macaddr_or);
    r.binop("|", Macaddr, Macaddr, Macaddr, macaddr_or);

    super::ops::register_cmp(r, Macaddr8, Macaddr8);
    r.func("trunc", vec![Macaddr8], Macaddr8, macaddr8_trunc);
    r.func("macaddr8_not", vec![Macaddr8], Macaddr8, macaddr8_not);
    r.prefix_op("~", Macaddr8, Macaddr8, macaddr8_not);
    r.func(
        "macaddr8_and",
        vec![Macaddr8, Macaddr8],
        Macaddr8,
        macaddr8_and,
    );
    r.binop("&", Macaddr8, Macaddr8, Macaddr8, macaddr8_and);
    r.func(
        "macaddr8_or",
        vec![Macaddr8, Macaddr8],
        Macaddr8,
        macaddr8_or,
    );
    r.binop("|", Macaddr8, Macaddr8, Macaddr8, macaddr8_or);
    r.func(
        "macaddr8_set7bit",
        vec![Macaddr8],
        Macaddr8,
        macaddr8_set7bit,
    );
}
