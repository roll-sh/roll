//! ivfflat's per-type support (`IvfflatTypeInfo` in `ivfutils.c`: vector, halfvec, bit) and
//! the operator class's support functions, resolved to the native kernels by their C names.

use crate::Result;
use crate::error::{Error, code};
use crate::ext::index_am::{AmColumn, AmIndex};
use crate::ext::types::{HALFVEC, VECTOR};
use crate::ext::vector::common::{f32_to_half, half_to_f32};
use crate::ext::vector::dist;
use crate::types::Value;
use std::sync::Arc;

/// A support function computing a distance (FUNCTION 1 and 3).
pub(super) type Dist = fn(&Value, &Value) -> f64;
/// A support function computing a norm (FUNCTION 2 and 4).
pub(super) type Norm = fn(&Value) -> f64;

/// The indexed type, as IvfflatGetTypeInfo tells it from support function 5.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    Vector,
    Halfvec,
    Bit,
}

impl Kind {
    /// `IvfflatGetTypeInfo`.
    pub(super) fn of(col: &AmColumn) -> Kind {
        match col.support(5) {
            Some("ivfflat_halfvec_support") => Kind::Halfvec,
            Some("ivfflat_bit_support") => Kind::Bit,
            _ => Kind::Vector,
        }
    }

    pub(super) fn tag(self) -> u8 {
        self as u8
    }

    pub(super) fn from_tag(t: u8) -> Kind {
        match t {
            1 => Kind::Halfvec,
            2 => Kind::Bit,
            _ => Kind::Vector,
        }
    }

    /// `maxDimensions`.
    pub(super) fn max_dimensions(self) -> i32 {
        match self {
            Kind::Vector => 2000,
            Kind::Halfvec => 4000,
            Kind::Bit => 64000,
        }
    }

    /// `dimensions`: vector_dims, halfvec_vector_dims, bitlength.
    pub(super) fn dimensions(self, v: &Value) -> i32 {
        match (self, v) {
            (Kind::Vector, Value::Ext(_, b)) => (b.len() / 4) as i32,
            (Kind::Halfvec, Value::Ext(_, b)) => (b.len() / 2) as i32,
            (Kind::Bit, Value::Bits(n, _)) => *n as i32,
            _ => -1,
        }
    }

    /// `itemSize`: VECTOR_SIZE, HALFVEC_SIZE, VARBITTOTALLEN.
    pub(super) fn item_size(self, dims: i32) -> usize {
        let d = dims.max(0) as usize;
        match self {
            Kind::Vector => 8 + 4 * d,
            Kind::Halfvec => 8 + 2 * d,
            Kind::Bit => 8 + d.div_ceil(8),
        }
    }

    /// VARSIZE of a value of the type (`item_size` of its own dimensions).
    pub(super) fn varsize(self, v: &Value) -> usize {
        self.item_size(self.dimensions(v))
    }

    /// `updateCenter`: a center of `x` (VectorUpdateCenter, HalfvecUpdateCenter,
    /// BitUpdateCenter).
    pub(super) fn center(self, x: &[f32]) -> Value {
        match self {
            Kind::Vector => Value::Ext(VECTOR, x.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>().into()),
            Kind::Halfvec => Value::Ext(HALFVEC, x.iter().flat_map(|f| f32_to_half(*f).to_le_bytes()).collect::<Vec<u8>>().into()),
            Kind::Bit => {
                let mut nx = vec![0u8; x.len().div_ceil(8)];
                for (i, f) in x.iter().enumerate() {
                    nx[i / 8] |= ((*f > 0.5) as u8) << (7 - (i % 8));
                }
                Value::Bits(x.len() as u32, nx.into())
            }
        }
    }

    /// `sumCenter`: `x` plus the value's elements (VectorSumCenter, HalfvecSumCenter,
    /// BitSumCenter), after their dimension check.
    pub(super) fn sum_center(self, v: &Value, x: &mut [f32]) -> Result<()> {
        check_expected_dim(x.len() as i32, self.dimensions(v))?;
        match (self, v) {
            (Kind::Vector, Value::Ext(_, b)) => {
                // Auto-vectorized
                for (xi, c) in x.iter_mut().zip(b.as_chunks::<4>().0) {
                    *xi += f32::from_le_bytes(*c);
                }
            }
            (Kind::Halfvec, Value::Ext(_, b)) => {
                for (xi, c) in x.iter_mut().zip(b.as_chunks::<2>().0) {
                    *xi += half_to_f32(u16::from_le_bytes(*c));
                }
            }
            (Kind::Bit, Value::Bits(_, b)) => {
                for (i, xi) in x.iter_mut().enumerate() {
                    *xi += ((b[i / 8] >> (7 - (i % 8))) & 0x01) as f32;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// `normalize`: l2_normalize, halfvec_l2_normalize (bit has none: never called).
    pub(super) fn normalize(self, v: &Value) -> Value {
        dist::l2_normalize(v)
    }
}

/// `IvfflatCheckNorm`: whether the value has a norm above zero (not NaN).
pub(super) fn check_norm(norm: Norm, v: &Value) -> bool {
    norm(v) > 0.0
}

/// `IvfflatCheckExpectedDim`.
pub(super) fn check_expected_dim(expected: i32, dim: i32) -> Result<()> {
    if dim != expected {
        return Err(Error::new(code::DATA_EXCEPTION, format!("expected {expected} dimensions, not {dim}")));
    }
    Ok(())
}

/// The operator class's support functions of the index's column.
#[derive(Clone, Copy)]
pub(super) struct Procs {
    pub kind: Kind,
    /// FUNCTION 1 (IVFFLAT_DISTANCE_PROC): lists and entries against values and queries.
    pub distance: Dist,
    /// FUNCTION 2 (IVFFLAT_NORM_PROC): values and queries are normalized when present.
    pub norm: Option<Norm>,
    /// FUNCTION 3 (IVFFLAT_KMEANS_DISTANCE_PROC).
    pub kmeans_distance: Dist,
    /// FUNCTION 4 (IVFFLAT_KMEANS_NORM_PROC): spherical k-means when present.
    pub kmeans_norm: Option<Norm>,
}

impl Procs {
    /// `index_getprocinfo` / `IvfflatOptionalProcInfo` for each, as native kernels.
    pub(super) fn of(ix: &AmIndex) -> Result<Procs> {
        let col = ix.columns.first().ok_or_else(|| crate::error::internal("ivfflat index without a column"))?;
        let symbol = |n: i16| -> Result<Option<&str>> {
            let Some(s) = col.support.iter().find(|s| s.0 == n) else {
                return Ok(None);
            };
            if s.1 != "vector" {
                return Err(crate::error::unsupported(format!("support function {n} of ivfflat index \"{}\" outside pgvector", ix.name)));
            }
            Ok(Some(s.2.as_str()))
        };
        let required = |n: i16| -> Result<Dist> {
            let Some(s) = symbol(n)? else {
                return Err(crate::error::internal(format!("missing support function {n} for attribute 1 of index \"{}\"", ix.name)));
            };
            distance(s).ok_or_else(|| crate::error::unsupported(format!("ivfflat support function {s}")))
        };
        let optional = |n: i16| -> Result<Option<Norm>> {
            match symbol(n)? {
                None => Ok(None),
                Some(s) => dist::norm(s).map(Some).ok_or_else(|| crate::error::unsupported(format!("ivfflat support function {s}"))),
            }
        };
        Ok(Procs {
            kind: Kind::of(col),
            distance: required(1)?,
            norm: optional(2)?,
            kmeans_distance: required(3)?,
            kmeans_norm: optional(4)?,
        })
    }
}

/// The distance function of C name `symbol`: `dist`'s, but for `hamming_distance`, computed
/// here without copying the strings (the same count).
fn distance(symbol: &str) -> Option<Dist> {
    if symbol == "hamming_distance" {
        return Some(hamming);
    }
    dist::distance(symbol)
}

/// `BitHammingDistance` (as `dist::hamming`: each string's bytes, its pad bits cleared, over
/// the shorter).
fn hamming(a: &Value, b: &Value) -> f64 {
    let (Value::Bits(na, a), Value::Bits(nb, b)) = (a, b) else {
        return 0.0;
    };
    let bytes = |n: u32, b: &Arc<[u8]>| (n as usize).div_ceil(8).min(b.len());
    let (la, lb) = (bytes(*na, a), bytes(*nb, b));
    let n = la.min(lb);
    if n == 0 {
        return 0.0;
    }
    let mut d = 0u64;
    // The bytes before the last are whole in both.
    let full = n - 1;
    let (ca, ra) = a[..full].as_chunks::<8>();
    let (cb, rb) = b[..full].as_chunks::<8>();
    for (x, y) in ca.iter().zip(cb) {
        d += (u64::from_ne_bytes(*x) ^ u64::from_ne_bytes(*y)).count_ones() as u64;
    }
    for (x, y) in ra.iter().zip(rb) {
        d += (x ^ y).count_ones() as u64;
    }
    let last = |n: u32, l: usize, b: &[u8]| {
        let mut v = b[full];
        if full == l - 1 && !n.is_multiple_of(8) {
            v &= 0xffu8 << (8 - n % 8);
        }
        v
    };
    d += (last(*na, la, a) ^ last(*nb, lb, b)).count_ones() as u64;
    d as f64
}
