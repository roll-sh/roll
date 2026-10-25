//! cube (`contrib/cube`): n-dimensional boxes and points of float8 coordinates. A value's
//! bytes are the NDBOX's: the header (dimensions, and bit 31 for a point, whose upper-right
//! corner isn't stored) and the coordinates, little-endian.

mod funcs;
mod parse;

use super::native::Native;
use super::types::{CUBE, ExtType};
use crate::types::Value;
use std::borrow::Cow;

pub(crate) const CUBE_MAX_DIM: usize = 100;
const POINT_BIT: u32 = 0x8000_0000;
const DIM_MASK: u32 = 0x7fff_ffff;

/// An NDBOX: `x` holds the lower-left coordinates, then (unless `point`) the upper-right.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Ndbox {
    pub point: bool,
    pub dim: usize,
    pub x: Vec<f64>,
}

/// C's `Min` / `Max` macros (NaN-sensitive as they are).
pub(crate) fn cmin(a: f64, b: f64) -> f64 {
    if a < b { a } else { b }
}
pub(crate) fn cmax(a: f64, b: f64) -> f64 {
    if a > b { a } else { b }
}

impl Ndbox {
    pub fn ll(&self, i: usize) -> f64 {
        self.x[i]
    }
    pub fn ur(&self, i: usize) -> f64 {
        if self.point { self.x[i] } else { self.x[i + self.dim] }
    }

    /// A box of `dim` dimensions from its corners (2 * dim coordinates), a point when
    /// they coincide (`cube_is_point_internal`; the point flag set, as the C code does).
    pub fn boxed(dim: usize, mut x: Vec<f64>) -> Ndbox {
        let point = (0..dim).all(|i| x[i] == x[i + dim]);
        if point {
            x.truncate(dim);
        }
        Ndbox { point, dim, x }
    }

    pub fn point(x: Vec<f64>) -> Ndbox {
        Ndbox { point: true, dim: x.len(), x }
    }

    pub fn is_point(&self) -> bool {
        self.point || (0..self.dim).all(|i| self.ll(i) == self.ur(i))
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + 8 * self.x.len());
        let header = self.dim as u32 | if self.point { POINT_BIT } else { 0 };
        out.extend_from_slice(&header.to_le_bytes());
        for v in &self.x {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    pub fn from_bytes(b: &[u8]) -> Ndbox {
        let header = b.get(..4).map_or(0, |h| u32::from_le_bytes(h.try_into().unwrap()));
        let dim = (header & DIM_MASK) as usize;
        let point = header & POINT_BIT != 0;
        let x = (4..b.len().saturating_sub(7))
            .step_by(8)
            .map(|i| f64::from_le_bytes(b[i..i + 8].try_into().unwrap()))
            .collect();
        Ndbox { point, dim, x }
    }

    pub fn value(&self) -> Value {
        Value::Ext(CUBE, self.to_bytes().into())
    }
}

/// A cube argument.
pub(crate) fn arg(v: &Value) -> Ndbox {
    match v {
        Value::Ext(_, b) => Ndbox::from_bytes(b),
        _ => Ndbox::point(Vec::new()),
    }
}

fn output(b: &[u8], ctx: &crate::types::text::FormatCtx) -> String {
    let c = Ndbox::from_bytes(b);
    let f = |v: f64| crate::types::text::format_float8(v, ctx.extra_float_digits);
    let side = |get: &dyn Fn(usize) -> f64| (0..c.dim).map(|i| f(get(i))).collect::<Vec<_>>().join(", ");
    let mut s = format!("({})", side(&|i| c.ll(i)));
    if !c.is_point() {
        s.push_str(&format!(",({})", side(&|i| c.ur(i))));
    }
    s
}

/// An index key consistent with `cube_cmp`'s equality, ordered as it is among cubes of one
/// dimension count (cubes of different counts never compare equal; their keys are grouped
/// by the count, which isn't `cube_cmp`'s order across counts).
fn key(b: &[u8], out: &mut Vec<u8>) {
    let c = Ndbox::from_bytes(b);
    out.extend_from_slice(&(c.dim as u32).to_be_bytes());
    let mut put = |v: f64| {
        let v = if v == 0.0 { 0.0 } else { v };
        let bits = v.to_bits();
        let k = if bits >> 63 == 1 { !bits } else { bits | (1 << 63) };
        out.extend_from_slice(&k.to_be_bytes());
    };
    for i in 0..c.dim {
        put(cmin(c.ll(i), c.ur(i)));
    }
    for i in 0..c.dim {
        put(cmax(c.ll(i), c.ur(i)));
    }
}

/// Equal cubes in one form: normalized corners as a box.
fn canon(b: &[u8]) -> Cow<'_, [u8]> {
    let mut k = Vec::new();
    key(b, &mut k);
    Cow::Owned(k)
}

pub(crate) const TYPE: ExtType = ExtType {
    kind: CUBE,
    extension: "cube",
    name: "cube",
    input: |s| Ok(parse::cube_in(s)?.to_bytes()),
    output,
    cmp: |a, b| funcs::cube_cmp(&Ndbox::from_bytes(a), &Ndbox::from_bytes(b)),
    canon,
    key,
    to_builtin: None,
    from_builtin: None,
    hash: false,
};

pub(super) fn symbol(sym: &str) -> Option<Native> {
    funcs::symbol(sym)
}

/// earthdistance's C function.
pub(super) fn earth_symbol(sym: &str) -> Option<Native> {
    (sym == "geo_distance").then_some(Native::scalar(funcs::geo_distance))
}
