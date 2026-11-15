//! Range fields (`integer_range`, `long_range`, `float_range`, `double_range`, `date_range`,
//! `ip_range`) as aggregations read them.
//!
//! A range field's doc values are one binary value per doc holding all of the doc's ranges in
//! Elasticsearch's `BinaryRangeUtil` encoding, so that `cardinality` (which hashes the whole
//! value) counts exactly like Elasticsearch:
//!
//! - a VInt range count, then each range's `from` and `to`;
//! - `long`, `integer` and `date` bounds: a variable-length, order-preserving encoding (a header
//!   byte with the sign bit, four bits of extra-byte count and three value bits, then big-endian
//!   value bytes; negative values are stored as the complement of `-1 - v`);
//! - `float` / `double` bounds: Lucene's sortable int / long, big-endian (4 / 8 bytes);
//! - `ip` bounds: 16-byte `InetAddressPoint` encodings.
//!
//! Numeric ranges are sorted by `(from, to)` before encoding; ip ranges keep their order.
//! Bounds are inclusive: exclusive bounds are adjusted (`nextUp` / `nextDown`) before
//! encoding, and a missing bound is the type's minimum or maximum. [`encode_long_ranges`] and
//! its siblings build the value (`search-es` indexes with them); [`decode_f64`] and
//! [`decode_i64`] read it back for `histogram` and `date_histogram`.

use std::cmp::Ordering;

use crate::error::{AggError, Result};
use crate::request::java_double_compare;

/// The type of a range field (`RangeType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeType {
    Integer,
    Long,
    Float,
    Double,
    Date,
    Ip,
}

impl RangeType {
    /// The mapping type name (`RangeType.name`).
    pub fn name(self) -> &'static str {
        match self {
            RangeType::Integer => "integer_range",
            RangeType::Long => "long_range",
            RangeType::Float => "float_range",
            RangeType::Double => "double_range",
            RangeType::Date => "date_range",
            RangeType::Ip => "ip_range",
        }
    }

    /// `RangeType.isNumeric()`: every type but `ip`.
    pub fn is_numeric(self) -> bool {
        self != RangeType::Ip
    }
}

fn write_vint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// `BinaryRangeUtil.encodeLong`: the variable-length, order-preserving long encoding.
pub fn encode_long(out: &mut Vec<u8>, v: i64) {
    let (sign, mag) = if v < 0 { (0u8, (-1 - v) as u64) } else { (1u8, v as u64) };
    let bits = 64 - mag.leading_zeros() as usize;
    let extra = (bits + 7 - 3) / 8;
    let start = out.len();
    out.resize(start + 1 + extra, 0);
    let enc = &mut out[start..];
    let mut m = mag;
    for i in (0..=extra).rev() {
        enc[i] = m as u8;
        m >>= 8;
    }
    if sign == 0 {
        for b in enc.iter_mut() {
            *b = !*b;
        }
        enc[0] &= 0x07;
        enc[0] |= ((15 - extra) as u8) << 3;
    } else {
        enc[0] &= 0x07;
        enc[0] |= 0x80 | ((extra as u8) << 3);
    }
}

/// The encoded length of the long starting at `b[0]` (`LengthType.VARIABLE`).
#[inline]
fn long_len(token: u8) -> usize {
    let mut n = ((token >> 3) & 0x0f) as usize;
    if token & 0x80 == 0 {
        n = 0x0f - n;
    }
    1 + n
}

/// `BinaryRangeUtil.decodeLong`.
#[inline]
fn decode_long(b: &[u8]) -> i64 {
    let mut v: i64 = if b[0] & 0x80 == 0 { -8 | b[0] as i8 as i64 } else { (b[0] & 7) as i64 };
    for &x in &b[1..] {
        v = (v << 8).wrapping_add(x as i64);
    }
    v
}

fn sortable_double(v: f64) -> u64 {
    let bits = v.to_bits() as i64;
    (bits ^ ((bits >> 63) & 0x7fff_ffff_ffff_ffff)) as u64 ^ (1 << 63)
}

fn sortable_float(v: f32) -> u32 {
    let bits = v.to_bits() as i32;
    (bits ^ ((bits >> 31) & 0x7fff_ffff)) as u32 ^ (1 << 31)
}

fn unsortable_double(s: u64) -> f64 {
    let bits = (s ^ (1 << 63)) as i64;
    f64::from_bits((bits ^ ((bits >> 63) & 0x7fff_ffff_ffff_ffff)) as u64)
}

fn unsortable_float(s: u32) -> f32 {
    let bits = (s ^ (1 << 31)) as i32;
    f32::from_bits((bits ^ ((bits >> 31) & 0x7fff_ffff)) as u32)
}

/// `encodeLongRanges` (also `integer_range` and `date_range`): sorted by `(from, to)`.
pub fn encode_long_ranges(ranges: &[(i64, i64)]) -> Vec<u8> {
    let mut sorted = ranges.to_vec();
    sorted.sort();
    let mut out = Vec::with_capacity(5 + 18 * sorted.len());
    write_vint(&mut out, sorted.len() as u32);
    for (from, to) in sorted {
        encode_long(&mut out, from);
        encode_long(&mut out, to);
    }
    out
}

/// `encodeDoubleRanges`: sorted by `(from, to)` with `Double.compare`.
pub fn encode_double_ranges(ranges: &[(f64, f64)]) -> Vec<u8> {
    let mut sorted = ranges.to_vec();
    sorted.sort_by(|a, b| java_double_compare(a.0, b.0).then_with(|| java_double_compare(a.1, b.1)));
    let mut out = Vec::with_capacity(5 + 16 * sorted.len());
    write_vint(&mut out, sorted.len() as u32);
    for (from, to) in sorted {
        out.extend_from_slice(&sortable_double(from).to_be_bytes());
        out.extend_from_slice(&sortable_double(to).to_be_bytes());
    }
    out
}

/// `encodeFloatRanges`: sorted by `(from, to)`.
pub fn encode_float_ranges(ranges: &[(f32, f32)]) -> Vec<u8> {
    let mut sorted = ranges.to_vec();
    sorted.sort_by(|a, b| java_double_compare(a.0 as f64, b.0 as f64).then_with(|| java_double_compare(a.1 as f64, b.1 as f64)));
    let mut out = Vec::with_capacity(5 + 8 * sorted.len());
    write_vint(&mut out, sorted.len() as u32);
    for (from, to) in sorted {
        out.extend_from_slice(&sortable_float(from).to_be_bytes());
        out.extend_from_slice(&sortable_float(to).to_be_bytes());
    }
    out
}

/// `encodeIPRanges`: 16-byte `InetAddressPoint` bounds, in the given order.
pub fn encode_ip_ranges(ranges: &[([u8; 16], [u8; 16])]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + 32 * ranges.len());
    write_vint(&mut out, ranges.len() as u32);
    for (from, to) in ranges {
        out.extend_from_slice(from);
        out.extend_from_slice(to);
    }
    out
}

fn corrupt() -> AggError {
    AggError::new(500, "illegal_state_exception", "corrupt range doc value")
}

/// Reads the VInt count; bounded by the bytes present (a range takes at least 2 bytes).
fn read_count(b: &[u8]) -> Result<(usize, usize)> {
    let mut v: u32 = 0;
    for (i, &x) in b.iter().enumerate().take(5) {
        v |= ((x & 0x7f) as u32) << (7 * i);
        if x & 0x80 == 0 {
            let n = v as usize;
            if n > (b.len() - i - 1) / 2 {
                return Err(corrupt());
            }
            return Ok((n, i + 1));
        }
    }
    Err(corrupt())
}

/// Decodes a numeric range value into `(from, to)` doubles (`RangeType.doubleValue`), in
/// encoded order. `out` is cleared first.
pub fn decode_f64(ty: RangeType, b: &[u8], out: &mut Vec<(f64, f64)>) -> Result<()> {
    out.clear();
    let (n, mut pos) = read_count(b)?;
    for _ in 0..n {
        let mut bound = || -> Result<f64> {
            let v = match ty {
                RangeType::Long | RangeType::Integer | RangeType::Date => {
                    let len = long_len(*b.get(pos).ok_or_else(corrupt)?);
                    let s = b.get(pos..pos + len).ok_or_else(corrupt)?;
                    pos += len;
                    decode_long(s) as f64
                }
                RangeType::Double => {
                    let s = b.get(pos..pos + 8).ok_or_else(corrupt)?;
                    pos += 8;
                    unsortable_double(u64::from_be_bytes(s.try_into().unwrap_or_default()))
                }
                RangeType::Float => {
                    let s = b.get(pos..pos + 4).ok_or_else(corrupt)?;
                    pos += 4;
                    unsortable_float(u32::from_be_bytes(s.try_into().unwrap_or_default())) as f64
                }
                RangeType::Ip => return Err(corrupt()),
            };
            Ok(v)
        };
        let from = bound()?;
        let to = bound()?;
        out.push((from, to));
    }
    Ok(())
}

/// Decodes a `long` / `integer` / `date` range value into `(from, to)` longs.
pub fn decode_i64(b: &[u8], out: &mut Vec<(i64, i64)>) -> Result<()> {
    out.clear();
    let (n, mut pos) = read_count(b)?;
    for _ in 0..n {
        let mut bound = || -> Result<i64> {
            let len = long_len(*b.get(pos).ok_or_else(corrupt)?);
            let s = b.get(pos..pos + len).ok_or_else(corrupt)?;
            pos += len;
            Ok(decode_long(s))
        };
        let from = bound()?;
        let to = bound()?;
        out.push((from, to));
    }
    Ok(())
}

/// Orders encoded longs like their values (what the encoding guarantees).
pub fn compare_encoded_longs(a: i64, b: i64) -> Ordering {
    let (mut x, mut y) = (Vec::new(), Vec::new());
    encode_long(&mut x, a);
    encode_long(&mut y, b);
    x.cmp(&y)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `BinaryRangeUtil.encodeLong` outputs, worked through its algorithm by hand.
    #[test]
    fn long_encoding_matches_elasticsearch() {
        let cases: [(i64, &[u8]); 9] = [
            (0, &[0x80]),
            (1, &[0x81]),
            (7, &[0x87]),
            (8, &[0x88, 0x08]),
            (-1, &[0x7f]),
            (-8, &[0x78]),
            (-9, &[0x77, 0xf7]),
            (300, &[0x89, 0x2c]),
            (i64::MAX, &[0xc0, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]),
        ];
        for (v, want) in cases {
            let mut out = Vec::new();
            encode_long(&mut out, v);
            assert_eq!(out, want, "{v}");
            assert_eq!(long_len(out[0]), out.len());
            assert_eq!(decode_long(&out), v);
        }
        let mut out = Vec::new();
        encode_long(&mut out, i64::MIN);
        assert_eq!(out, [0x3f, 0x80, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(decode_long(&out), i64::MIN);
    }

    #[test]
    fn long_encoding_preserves_order() {
        let mut vals: Vec<i64> = vec![i64::MIN, -1 << 40, -70000, -256, -9, -8, -1, 0, 1, 7, 8, 255, 256, 1 << 33, i64::MAX];
        let mut rng = 0x1234_5678_9abc_def0u64;
        for _ in 0..2000 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            vals.push((rng as i64) >> (rng % 60));
        }
        for w in vals.windows(2) {
            assert_eq!(compare_encoded_longs(w[0], w[1]), w[0].cmp(&w[1]), "{} {}", w[0], w[1]);
            let mut b = Vec::new();
            encode_long(&mut b, w[0]);
            assert_eq!(decode_long(&b), w[0]);
            assert_eq!(long_len(b[0]), b.len());
        }
    }

    #[test]
    fn ranges_round_trip_sorted() {
        let enc = encode_long_ranges(&[(20, 22), (3, 9), (3, 4)]);
        let mut out = Vec::new();
        decode_i64(&enc, &mut out).unwrap();
        assert_eq!(out, [(3, 4), (3, 9), (20, 22)]);
        let mut f = Vec::new();
        decode_f64(RangeType::Long, &enc, &mut f).unwrap();
        assert_eq!(f, [(3.0, 4.0), (3.0, 9.0), (20.0, 22.0)]);

        let enc = encode_double_ranges(&[(1.5, f64::INFINITY), (f64::NEG_INFINITY, -0.0), (-2.0, 3.25)]);
        assert_eq!(enc.len(), 1 + 3 * 16);
        decode_f64(RangeType::Double, &enc, &mut f).unwrap();
        assert_eq!(f, [(f64::NEG_INFINITY, -0.0), (-2.0, 3.25), (1.5, f64::INFINITY)]);
        assert!(f[0].1.is_sign_negative());

        let enc = encode_float_ranges(&[(0.1, 0.2), (-1.0, 5.5)]);
        decode_f64(RangeType::Float, &enc, &mut f).unwrap();
        assert_eq!(f, [(-1.0, 5.5), (0.1f32 as f64, 0.2f32 as f64)]);

        assert!(decode_f64(RangeType::Long, &[5, 0x80], &mut f).is_err());
        assert!(decode_i64(&[1, 0x88], &mut out).is_err());
        assert!(decode_i64(&[0xff, 0xff, 0xff, 0xff, 0x0f], &mut out).is_err());
        decode_i64(&[0], &mut out).unwrap();
        assert!(out.is_empty());
    }
}
