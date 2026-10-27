//! The operator class's view of the indexed type (`HnswTypeInfo`, `HnswSupport`): the
//! distance (support function 1) and norm (2) by their C symbols, the type's dimensions,
//! normalization and value check (3: `hnsw_halfvec_support`, `hnsw_bit_support`,
//! `hnsw_sparsevec_support`; none for vector), and the size the value takes as a varlena
//! (which decides how element tuples fill pages).

use super::super::dist;
use crate::Result;
use crate::error::Error;
use crate::ext::index_am::AmIndex;
use crate::ext::types::{HALFVEC, SPARSEVEC, VECTOR};
use crate::types::Value;

/// HNSW_MAX_DIM.
pub(super) const MAX_DIM: i32 = 2000;
/// HNSW_MAX_NNZ.
const MAX_NNZ: usize = 1000;
/// SPARSEVEC_MAX_DIM.
const SPARSEVEC_MAX_DIM: i32 = 1_000_000_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    Vector,
    Halfvec,
    Sparsevec,
    Bit,
}

/// A byte-level distance: the float8 the SQL function returns.
pub(super) type DistFn = fn(&[u8], &[u8]) -> f64;

/// HnswTypeInfo + HnswSupport of an index's (only) column.
#[derive(Clone, Copy)]
pub(super) struct Support {
    pub kind: Kind,
    pub dist: DistFn,
    /// The norm function (FUNCTION 2), when the class normalizes (cosine).
    pub norm: Option<fn(&Value) -> f64>,
}

/// `BitHammingDistance` over two equally long bit strings' bytes (pad bits clear).
fn hamming(a: &[u8], b: &[u8]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (x ^ y).count_ones() as u64).sum::<u64>() as f64
}

/// `BitJaccardDistance`.
fn jaccard(a: &[u8], b: &[u8]) -> f64 {
    let (mut ab, mut aa, mut bb) = (0u64, 0u64, 0u64);
    for (x, y) in a.iter().zip(b) {
        ab += (x & y).count_ones() as u64;
        aa += x.count_ones() as u64;
        bb += y.count_ones() as u64;
    }
    if ab == 0 { 1.0 } else { 1.0 - (ab as f64 / (aa + bb - ab) as f64) }
}

/// The byte-level kernel of distance function `symbol`.
fn distance(symbol: &str) -> Option<DistFn> {
    Some(match symbol {
        "vector_l2_squared_distance" => dist::vector_l2_squared,
        "l2_distance" => dist::vector_l2,
        "vector_negative_inner_product" => dist::vector_negative_ip,
        "inner_product" => dist::vector_ip,
        "cosine_distance" => dist::vector_cosine,
        "vector_spherical_distance" => dist::vector_spherical,
        "l1_distance" => dist::vector_l1,
        "halfvec_l2_squared_distance" => dist::halfvec_l2_squared,
        "halfvec_l2_distance" => dist::halfvec_l2,
        "halfvec_negative_inner_product" => dist::halfvec_negative_ip,
        "halfvec_inner_product" => dist::halfvec_ip,
        "halfvec_cosine_distance" => dist::halfvec_cosine,
        "halfvec_spherical_distance" => dist::halfvec_spherical,
        "halfvec_l1_distance" => dist::halfvec_l1,
        "sparsevec_l2_squared_distance" => dist::sparsevec_l2_squared,
        "sparsevec_l2_distance" => dist::sparsevec_l2,
        "sparsevec_negative_inner_product" => dist::sparsevec_negative_ip,
        "sparsevec_inner_product" => dist::sparsevec_ip,
        "sparsevec_cosine_distance" => dist::sparsevec_cosine,
        "sparsevec_l1_distance" => dist::sparsevec_l1,
        "hamming_distance" => hamming,
        "jaccard_distance" => jaccard,
        _ => return None,
    })
}

impl Support {
    /// HnswInitSupport + HnswGetTypeInfo of index `ix`.
    pub fn of(ix: &AmIndex) -> Result<Support> {
        let col = ix.columns.first().ok_or_else(|| crate::error::internal("hnsw index without a column"))?;
        let kind = match col.support(3) {
            None => Kind::Vector,
            Some("hnsw_halfvec_support") => Kind::Halfvec,
            Some("hnsw_bit_support") => Kind::Bit,
            Some("hnsw_sparsevec_support") => Kind::Sparsevec,
            Some(s) => return Err(crate::error::internal(format!("hnsw: unknown type support function {s}"))),
        };
        let sym = col.support(1).ok_or_else(|| crate::error::internal("missing support function 1 for hnsw index"))?;
        let dist = distance(sym).ok_or_else(|| crate::error::internal(format!("hnsw: unsupported distance function {sym}")))?;
        let norm = match col.support(2) {
            Some(s) => Some(dist::norm(s).ok_or_else(|| crate::error::internal(format!("hnsw: unsupported norm function {s}")))?),
            None => None,
        };
        Ok(Support { kind, dist, norm })
    }

    /// maxDimensions.
    pub fn max_dims(&self) -> i32 {
        match self.kind {
            Kind::Vector => MAX_DIM,
            Kind::Halfvec => MAX_DIM * 2,
            Kind::Bit => MAX_DIM * 32,
            Kind::Sparsevec => SPARSEVEC_MAX_DIM,
        }
    }

    /// The `dimensions` function of the type (vector_dims, bitlength, ...).
    pub fn dims(&self, v: &Value) -> i32 {
        match (self.kind, v) {
            (Kind::Bit, Value::Bits(n, _)) => *n as i32,
            (Kind::Vector, Value::Ext(_, b)) => (b.len() / 4) as i32,
            (Kind::Halfvec, Value::Ext(_, b)) => (b.len() / 2) as i32,
            (Kind::Sparsevec, Value::Ext(_, b)) if b.len() >= 4 => i32::from_le_bytes(b[..4].try_into().unwrap()),
            _ => 0,
        }
    }

    /// The stored bytes of a value: a vector type's own, a bit string's with its pad bits
    /// clear (the distances read whole bytes).
    pub fn bytes(&self, v: &Value) -> Vec<u8> {
        match v {
            Value::Ext(_, b) => b.to_vec(),
            Value::Bits(n, b) => {
                let mut b = b[..(*n as usize).div_ceil(8).min(b.len())].to_vec();
                if n % 8 != 0
                    && let Some(last) = b.last_mut()
                {
                    *last &= 0xffu8 << (8 - n % 8);
                }
                b
            }
            _ => Vec::new(),
        }
    }

    /// The value of stored bytes (`dims`: the index's dimensions, a bit string's length).
    pub fn value(&self, b: &[u8], dims: u32) -> Value {
        match self.kind {
            Kind::Vector => Value::Ext(VECTOR, b.into()),
            Kind::Halfvec => Value::Ext(HALFVEC, b.into()),
            Kind::Sparsevec => Value::Ext(SPARSEVEC, b.into()),
            Kind::Bit => Value::Bits(dims, b.into()),
        }
    }

    /// VARSIZE_ANY of the value as PostgreSQL holds it, from its stored bytes.
    pub fn varsize(&self, b: &[u8]) -> usize {
        match self.kind {
            // vl_len_, dim, unused
            Kind::Vector | Kind::Halfvec => 8 + b.len(),
            // vl_len_, dim, nnz, unused (the engine's bytes start at dim)
            Kind::Sparsevec => 8 + b.len(),
            // vl_len_, bit_len
            Kind::Bit => 8 + b.len(),
        }
    }

    /// HnswFormIndexValue: the value as the index holds it (checked, normalized), or None
    /// when the class normalizes and its norm is zero (not indexed).
    pub fn form(&self, v: &Value) -> Result<Option<Value>> {
        if self.kind == Kind::Sparsevec
            && let Value::Ext(_, b) = v
            && b.len() >= 8
            && u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize > MAX_NNZ
        {
            return Err(Error::new("54000", format!("sparsevec cannot have more than {MAX_NNZ} non-zero elements for hnsw index")));
        }
        let Some(norm) = self.norm else {
            return Ok(Some(v.clone()));
        };
        // (HnswCheckNorm: > 0, so a NaN norm is not indexed either.)
        if norm(v).partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
            return Ok(None);
        }
        Ok(Some(dist::l2_normalize(v)))
    }

    /// The scan's query (GetScanValue): normalized when the class normalizes.
    pub fn query(&self, v: &Value) -> Value {
        if self.norm.is_some() { dist::l2_normalize(v) } else { v.clone() }
    }

    /// HnswCheckDim.
    pub fn check_dim(&self, expected: i32, v: &Value) -> Result<()> {
        let dim = self.dims(v);
        if dim != expected {
            return Err(Error::new("22000", format!("expected {expected} dimensions, not {dim}")));
        }
        Ok(())
    }
}
