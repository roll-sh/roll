//! Norms: one byte per document and field, Lucene's `SmallFloat.intToByte4` encoding of the
//! field length, so BM25 length normalization (and therefore scores) match Elasticsearch to the
//! bit.
//!
//! Layout: a kind byte (empty / constant / bytes), a [`DocSet`] of the docs that have a norm,
//! and for the bytes kind one byte per doc of the set. When every doc has the field the set is
//! "all", so `get(doc)` is a single indexed load; sparse fields pay one rank lookup.

use crate::docset::{DocSet, write_docset_bits};
use crate::error::{Result, corrupt};
use crate::io::{In, Out};
use crate::slice::Slice;

/// Lucene `SmallFloat.longToInt4`: a 4-bit mantissa, 4-bit exponent float for non-negative
/// integers; exact below 8.
pub fn long_to_int4(i: u64) -> u32 {
    let num_bits = 64 - i.leading_zeros();
    if num_bits < 4 {
        i as u32
    } else {
        let shift = num_bits - 4;
        let encoded = ((i >> shift) as u32) & 0x07;
        encoded | ((shift + 1) << 3)
    }
}

/// Inverse of [`long_to_int4`].
pub fn int4_to_long(i: u32) -> u64 {
    let bits = (i & 0x07) as u64;
    let shift = (i >> 3) as i32 - 1;
    if shift == -1 {
        bits
    } else {
        (bits | 0x08) << shift
    }
}

const MAX_INT4: u32 = 231; // long_to_int4(i32::MAX)
const NUM_FREE_VALUES: u32 = 255 - MAX_INT4;

/// Lucene `SmallFloat.intToByte4`: lengths below 24 are exact, larger ones keep 4 significant
/// bits. Returns the unsigned byte (Lucene's signed byte reinterpreted).
pub fn int_to_byte4(i: u32) -> u8 {
    let i = i.min(i32::MAX as u32);
    if i < NUM_FREE_VALUES {
        i as u8
    } else {
        (NUM_FREE_VALUES + long_to_int4((i - NUM_FREE_VALUES) as u64)) as u8
    }
}

/// Lucene `SmallFloat.byte4ToInt`: the length a norm byte stands for.
pub fn byte4_to_int(b: u8) -> u32 {
    let i = b as u32;
    if i < NUM_FREE_VALUES {
        i
    } else {
        NUM_FREE_VALUES + int4_to_long(i - NUM_FREE_VALUES) as u32
    }
}

const EMPTY: u8 = 0;
const CONSTANT: u8 = 1;
const BYTES: u8 = 2;

/// Writes the norms of one field: the docs whose bit is set in `has` (covering `max_doc` bits),
/// with `dense[doc]` as their norm byte.
pub(crate) fn write_norms<O: Out + ?Sized>(has: &[u64], dense: &[u8], max_doc: u32, out: &mut O) {
    let mut first = None;
    let mut constant = true;
    for_each_set(has, max_doc, |d| {
        let v = dense[d as usize];
        match first {
            None => first = Some(v),
            Some(f) => constant &= f == v,
        }
    });
    let Some(first) = first else {
        out.put_u8(EMPTY);
        return;
    };
    out.put_u8(if constant { CONSTANT } else { BYTES });
    let count = write_docset_bits(has, max_doc, out);
    if constant {
        out.put_u8(first);
    } else if count == max_doc {
        out.put_bytes(&dense[..max_doc as usize]);
    } else {
        let mut buf = Vec::with_capacity(4096);
        for_each_set(has, max_doc, |d| {
            buf.push(dense[d as usize]);
            if buf.len() == 4096 {
                out.put_bytes(&buf);
                buf.clear();
            }
        });
        out.put_bytes(&buf);
    }
}

fn for_each_set(bits: &[u64], max_doc: u32, mut f: impl FnMut(u32)) {
    for (i, &w) in bits.iter().enumerate().take((max_doc as usize).div_ceil(64)) {
        let mut w = w;
        while w != 0 {
            f(i as u32 * 64 + w.trailing_zeros());
            w &= w - 1;
        }
    }
}

#[derive(Clone, Debug)]
enum Repr {
    Empty,
    Constant(u8),
    Bytes(Slice),
}

/// Per-field norms reader. Cheap to clone.
#[derive(Clone, Debug)]
pub struct NormsReader {
    docs: DocSet,
    repr: Repr,
}

impl NormsReader {
    pub(crate) fn open(data: Slice, max_doc: u32) -> Result<NormsReader> {
        let mut r = In::new(&data);
        let kind = r.u8()?;
        if kind == EMPTY {
            return Ok(NormsReader {
                docs: DocSet::empty(max_doc),
                repr: Repr::Empty,
            });
        }
        let mut pos = r.pos();
        let docs = DocSet::read(&data, &mut pos)?;
        if docs.max_doc() != max_doc {
            return Err(corrupt("norms doc set max_doc mismatch"));
        }
        let repr = match kind {
            CONSTANT => Repr::Constant(*data.get(pos).ok_or_else(|| corrupt("norms truncated"))?),
            BYTES => Repr::Bytes(
                data.try_slice(pos as u64, docs.len() as u64)
                    .ok_or_else(|| corrupt("norms truncated"))?,
            ),
            k => return Err(corrupt(format!("bad norms kind {k}"))),
        };
        Ok(NormsReader { docs, repr })
    }

    /// The norm of `doc`, if it has one.
    #[inline]
    pub fn get(&self, doc: u32) -> Option<u8> {
        match &self.repr {
            Repr::Empty => None,
            Repr::Constant(v) => self.docs.contains(doc).then_some(*v),
            Repr::Bytes(b) => {
                if self.docs.is_all() {
                    b.get(doc as usize).copied()
                } else {
                    self.docs.rank(doc).map(|i| b[i as usize])
                }
            }
        }
    }

    /// The norm of `doc`, or `missing` (Lucene's scorers use 1 for docs without a norm).
    #[inline]
    pub fn get_or(&self, doc: u32, missing: u8) -> u8 {
        self.get(doc).unwrap_or(missing)
    }

    /// Docs that have a norm.
    pub fn docs(&self) -> &DocSet {
        &self.docs
    }

    /// The dense norm bytes when every doc has a norm (`bytes[doc]`), for vectorized scoring.
    pub fn dense_bytes(&self) -> Option<&[u8]> {
        match &self.repr {
            Repr::Bytes(b) if self.docs.is_all() => Some(b),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_float_matches_lucene() {
        assert_eq!(long_to_int4(i32::MAX as u64), MAX_INT4);
        for i in 0..24 {
            assert_eq!(int_to_byte4(i), i as u8);
            assert_eq!(byte4_to_int(i as u8), i);
        }
        // Values computed with Lucene's SmallFloat.
        assert_eq!(int_to_byte4(1000), 87);
        assert_eq!(byte4_to_int(87), 984);
        assert_eq!(int_to_byte4(25), 25);
        assert_eq!(int_to_byte4(i32::MAX as u32), 255);
        assert_eq!(byte4_to_int(255), 24 + int4_to_long(231) as u32);
        // Monotone and lossy-from-below.
        let mut prev = 0u8;
        for len in 0..100_000u32 {
            let b = int_to_byte4(len);
            assert!(b >= prev);
            assert!(byte4_to_int(b) <= len);
            assert_eq!(int_to_byte4(byte4_to_int(b)), b);
            prev = b;
        }
    }

    #[test]
    fn norms_round_trip() {
        for (docs, vals, max_doc) in [
            (vec![], vec![], 10u32),
            (vec![0, 1, 2], vec![5, 5, 5], 3),
            (vec![1, 7], vec![3, 9], 10),
            (
                (0..1000).collect::<Vec<u32>>(),
                (0..1000).map(|i| (i % 256) as u8).collect::<Vec<u8>>(),
                1000,
            ),
        ] {
            let mut has = vec![0u64; (max_doc as usize).div_ceil(64)];
            let mut dense = vec![0u8; max_doc as usize];
            for (&d, &v) in docs.iter().zip(&vals) {
                has[d as usize >> 6] |= 1 << (d & 63);
                dense[d as usize] = v;
            }
            let mut out = Vec::new();
            write_norms(&has, &dense, max_doc, &mut out);
            let r = NormsReader::open(Slice::from(out), max_doc).unwrap();
            for d in 0..max_doc {
                let want = docs.iter().position(|&x| x == d).map(|i| vals[i]);
                assert_eq!(r.get(d), want);
            }
        }
    }
}
