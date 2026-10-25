//! seg (`contrib/seg`): float4 intervals that remember their significant digits and
//! whether a boundary is exact, `<`, `>` or `~`. A value's bytes are the SEG struct's:
//! lower and upper (float4, little-endian), l_sigd, u_sigd, l_ext, u_ext.

mod funcs;
mod parse;
mod restore;

use super::native::Native;
use super::types::{ExtType, SEG};
use crate::types::Value;
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Seg {
    pub lower: f32,
    pub upper: f32,
    pub l_sigd: u8,
    pub u_sigd: u8,
    pub l_ext: u8,
    pub u_ext: u8,
}

impl Seg {
    pub fn to_bytes(self) -> Vec<u8> {
        let mut b = Vec::with_capacity(12);
        b.extend_from_slice(&self.lower.to_le_bytes());
        b.extend_from_slice(&self.upper.to_le_bytes());
        b.extend_from_slice(&[self.l_sigd, self.u_sigd, self.l_ext, self.u_ext]);
        b
    }

    pub fn from_bytes(b: &[u8]) -> Seg {
        let f = |i: usize| b.get(i..i + 4).map_or(0.0, |x| f32::from_le_bytes(x.try_into().unwrap()));
        let c = |i: usize| b.get(i).copied().unwrap_or(0);
        Seg { lower: f(0), upper: f(4), l_sigd: c(8), u_sigd: c(9), l_ext: c(10), u_ext: c(11) }
    }

    pub fn value(self) -> Value {
        Value::Ext(SEG, self.to_bytes().into())
    }
}

pub(crate) fn arg(v: &Value) -> Seg {
    match v {
        Value::Ext(_, b) => Seg::from_bytes(b),
        _ => Seg::from_bytes(&[]),
    }
}

/// seg_out.
fn output(b: &[u8], _: &crate::types::text::FormatCtx) -> String {
    let s = Seg::from_bytes(b);
    let ext = |e: u8| matches!(e, b'>' | b'<' | b'~');
    let mut out = String::new();
    if ext(s.l_ext) {
        out.push(s.l_ext as char);
    }
    if s.lower == s.upper && s.l_ext == s.u_ext {
        // Built by seg_in off a single point.
        out.push_str(&restore::restore(s.lower, s.l_sigd as i32));
    } else {
        if s.l_ext != b'-' {
            out.push_str(&restore::restore(s.lower, s.l_sigd as i32));
            out.push(' ');
        }
        out.push_str("..");
        if s.u_ext != b'-' {
            out.push(' ');
            if ext(s.u_ext) {
                out.push(s.u_ext as char);
            }
            out.push_str(&restore::restore(s.upper, s.u_sigd as i32));
        }
    }
    out
}

/// An order-preserving key under `seg_cmp`: each boundary's position, then its kind and
/// significant digits in the order seg_cmp weighs them.
fn key(b: &[u8], out: &mut Vec<u8>) {
    let s = Seg::from_bytes(b);
    let float = |v: f32, out: &mut Vec<u8>| {
        let v = if v == 0.0 { 0.0 } else { v };
        let bits = v.to_bits();
        let k = if bits >> 31 == 1 { !bits } else { bits | (1 << 31) };
        out.extend_from_slice(&k.to_be_bytes());
    };
    let lower_rank = |e: u8| match e {
        b'-' => 0,
        b'<' => 1,
        b'>' => 3,
        _ => 2,
    };
    let upper_rank = |e: u8| match e {
        b'<' => 0,
        b'>' => 2,
        b'-' => 3,
        _ => 1,
    };
    float(s.lower, out);
    out.push(lower_rank(s.l_ext));
    out.push(s.l_sigd);
    out.push(u8::from(s.l_ext != b'~'));
    float(s.upper, out);
    out.push(upper_rank(s.u_ext));
    out.push(255 - s.u_sigd);
    out.push(u8::from(s.u_ext == b'~'));
}

fn canon(b: &[u8]) -> Cow<'_, [u8]> {
    let mut k = Vec::new();
    key(b, &mut k);
    Cow::Owned(k)
}

pub(crate) const TYPE: ExtType = ExtType {
    kind: SEG,
    extension: "seg",
    name: "seg",
    input: |s| Ok(parse::seg_in(s)?.to_bytes()),
    output,
    cmp: |a, b| funcs::seg_cmp(&Seg::from_bytes(a), &Seg::from_bytes(b)),
    canon,
    key,
    to_builtin: None,
    from_builtin: None,
    hash: false,
};

pub(super) fn symbol(sym: &str) -> Option<Native> {
    funcs::symbol(sym)
}
