//! The record encoders of `inverted_index::codec`, one per storage layout, and the choice
//! `NewInvertedIndex_Ex` makes between them from an index's flags.

use super::Record;
use super::flags::*;
use super::varint::{qint_decode, qint_encode, varint_decode, varint_decode_u32, varint_encode};

/// The opaque `InvertedIndex` variants: which encoder writes the records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Encoding {
    Full,
    FullWide,
    FreqsFields,
    FreqsFieldsWide,
    FreqsOnly,
    FieldsOnly,
    FieldsOnlyWide,
    FieldsOffsets,
    FieldsOffsetsWide,
    OffsetsOnly,
    FreqsOffsets,
    DocIdsOnly,
    RawDocIdsOnly,
    Numeric,
    NumericFloatCompression,
}

/// What the wrapper around the typed index counts besides its blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tracking {
    /// Bare `InvertedIndex<E>`.
    None,
    /// `FieldMaskTrackingIndex`: the union of every record's field mask.
    FieldMask,
    /// `EntriesTrackingIndex`: every record added, duplicates included.
    Entries,
}

const FULL: u32 = STORE_FREQS | STORE_TERM_OFFSETS | STORE_FIELD_FLAGS;
const FULL_WIDE: u32 = FULL | WIDE_SCHEMA;
const FREQS_FIELDS: u32 = STORE_FREQS | STORE_FIELD_FLAGS;
const FREQS_FIELDS_WIDE: u32 = FREQS_FIELDS | WIDE_SCHEMA;
const FIELDS_ONLY: u32 = STORE_FIELD_FLAGS;
const FIELDS_ONLY_WIDE: u32 = STORE_FIELD_FLAGS | WIDE_SCHEMA;
const FIELDS_OFFSETS: u32 = STORE_FIELD_FLAGS | STORE_TERM_OFFSETS;
const FIELDS_OFFSETS_WIDE: u32 = FIELDS_OFFSETS | WIDE_SCHEMA;
const FREQS_OFFSETS: u32 = STORE_FREQS | STORE_TERM_OFFSETS;

/// `size_of::<InvertedIndex<E>>()`: the blocks `ThinVec` pointer (8), `n_unique_docs`, `flags`,
/// `gc_marker` and `unique_id` (4 each).
pub const INVERTED_INDEX_SIZE: usize = 24;

impl Encoding {
    /// `NewInvertedIndex_Ex`: the encoder for `flags & INDEX_STORAGE_MASK`;
    /// `raw_doc_id_encoding` is `RSGlobalConfig.invertedIndexRawDocidEncoding` and
    /// `compress_floats` `RSGlobalConfig.numericCompress`. `None` where the reference panics.
    pub const fn from_flags(flags: u32, raw_doc_id_encoding: bool, compress_floats: bool) -> Option<Self> {
        Some(match flags & STORAGE_MASK {
            FULL => Self::Full,
            FULL_WIDE => Self::FullWide,
            FREQS_FIELDS => Self::FreqsFields,
            FREQS_FIELDS_WIDE => Self::FreqsFieldsWide,
            STORE_FREQS => Self::FreqsOnly,
            FIELDS_ONLY => Self::FieldsOnly,
            FIELDS_ONLY_WIDE => Self::FieldsOnlyWide,
            FIELDS_OFFSETS => Self::FieldsOffsets,
            FIELDS_OFFSETS_WIDE => Self::FieldsOffsetsWide,
            STORE_TERM_OFFSETS => Self::OffsetsOnly,
            FREQS_OFFSETS => Self::FreqsOffsets,
            DOC_IDS_ONLY if raw_doc_id_encoding => Self::RawDocIdsOnly,
            DOC_IDS_ONLY => Self::DocIdsOnly,
            STORE_NUMERIC if compress_floats => Self::NumericFloatCompression,
            STORE_NUMERIC => Self::Numeric,
            _ => return None,
        })
    }

    /// `Encoder::ALLOW_DUPLICATES`: only numeric indexes keep repeated doc ids (multi-value JSON).
    pub const fn allow_duplicates(self) -> bool {
        matches!(self, Self::Numeric | Self::NumericFloatCompression)
    }

    /// `Encoder::RECOMMENDED_BLOCK_ENTRIES`.
    pub const fn recommended_block_entries(self) -> u16 {
        match self {
            Self::DocIdsOnly | Self::RawDocIdsOnly => 1000,
            _ => 100,
        }
    }

    /// `Encoder::delta_base` is the block's first doc id rather than its last.
    pub const fn delta_from_first(self) -> bool {
        matches!(self, Self::RawDocIdsOnly)
    }

    /// `IdDelta::from_u64`: u32 deltas, or `NumericDelta`'s 7 bytes.
    pub const fn delta_fits(self, delta: u64) -> bool {
        match self {
            Self::Numeric | Self::NumericFloatCompression => delta >> 56 == 0,
            _ => delta <= u32::MAX as u64,
        }
    }

    /// The opaque variant's wrapper.
    pub const fn tracking(self) -> Tracking {
        match self {
            Self::Full
            | Self::FullWide
            | Self::FreqsFields
            | Self::FreqsFieldsWide
            | Self::FieldsOnly
            | Self::FieldsOnlyWide
            | Self::FieldsOffsets
            | Self::FieldsOffsetsWide => Tracking::FieldMask,
            Self::Numeric | Self::NumericFloatCompression => Tracking::Entries,
            _ => Tracking::None,
        }
    }

    /// `memory_usage` of an empty index (what `NewInvertedIndex` reports): the typed index plus
    /// the 16-byte field mask (`FieldMaskTrackingIndex`) or 8-byte entry count
    /// (`EntriesTrackingIndex`).
    pub const fn base_size(self) -> usize {
        INVERTED_INDEX_SIZE
            + match self.tracking() {
                Tracking::None => 0,
                Tracking::FieldMask => 16,
                Tracking::Entries => 8,
            }
    }

    /// `Encoder::encode`: appends `rec` with its doc id `delta` (already checked with
    /// `delta_fits`) to `out`, returning the bytes written. The narrow encoders take the low 32
    /// bits of the field mask (the reference refuses wider masks).
    pub fn encode(self, out: &mut Vec<u8>, delta: u64, rec: &Record<'_>) -> usize {
        let start = out.len();
        let d = delta as u32;
        let fm = rec.field_mask as u32;
        let offs = rec.offsets;
        let olen = offs.len() as u32;
        match self {
            Self::DocIdsOnly => {
                varint_encode(out, d as u128);
            }
            Self::RawDocIdsOnly => out.extend_from_slice(&d.to_le_bytes()),
            Self::FieldsOnly => {
                qint_encode(out, &[d, fm]);
            }
            Self::FieldsOnlyWide => {
                varint_encode(out, d as u128);
                varint_encode(out, rec.field_mask);
            }
            Self::FreqsFields => {
                qint_encode(out, &[d, rec.freq, fm]);
            }
            Self::FreqsFieldsWide => {
                qint_encode(out, &[d, rec.freq]);
                varint_encode(out, rec.field_mask);
            }
            Self::FreqsOnly => {
                qint_encode(out, &[d, rec.freq]);
            }
            Self::FieldsOffsets => {
                qint_encode(out, &[d, fm, olen]);
                out.extend_from_slice(offs);
            }
            Self::FieldsOffsetsWide => {
                qint_encode(out, &[d, olen]);
                varint_encode(out, rec.field_mask);
                out.extend_from_slice(offs);
            }
            Self::OffsetsOnly => {
                qint_encode(out, &[d, olen]);
                out.extend_from_slice(offs);
            }
            Self::FreqsOffsets => {
                qint_encode(out, &[d, rec.freq, olen]);
                out.extend_from_slice(offs);
            }
            Self::Full => {
                qint_encode(out, &[d, rec.freq, fm, olen]);
                out.extend_from_slice(offs);
            }
            Self::FullWide => {
                qint_encode(out, &[d, rec.freq, olen]);
                varint_encode(out, rec.field_mask);
                out.extend_from_slice(offs);
            }
            Self::Numeric => numeric_encode(out, delta, rec.numeric, false),
            Self::NumericFloatCompression => numeric_encode(out, delta, rec.numeric, true),
        }
        out.len() - start
    }

    /// `Decoder::base_result`: what a decoded record holds in the parts its encoder doesn't
    /// store (terms: freq 1, no fields; `FreqsOnly`'s virtual result: no fields; numeric: freq
    /// 1, every field).
    pub const fn base_record(self) -> Record<'static> {
        match self {
            Self::FreqsOnly => Record { doc_id: 0, freq: 0, field_mask: 0, offsets: &[], numeric: 0.0 },
            Self::Numeric | Self::NumericFloatCompression => {
                Record { doc_id: 0, freq: 1, field_mask: u128::MAX, offsets: &[], numeric: 0.0 }
            }
            _ => Record { doc_id: 0, freq: 1, field_mask: 0, offsets: &[], numeric: 0.0 },
        }
    }

    /// `Decoder::decode`: the record at `data[*pos..]` whose delta is relative to `base`,
    /// advancing `pos`; `None` when the data is truncated.
    pub fn decode<'a>(self, data: &'a [u8], pos: &mut usize, base: u64) -> Option<Record<'a>> {
        let mut rec = self.base_record();
        let mut p = *pos;
        let delta: u64 = match self {
            Self::DocIdsOnly => varint_decode_u32(data, &mut p)? as u64,
            Self::RawDocIdsOnly => {
                let b = data.get(p..p + 4)?;
                p += 4;
                u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64
            }
            Self::FieldsOnly => {
                let [d, fm] = qint_decode::<2>(data, &mut p)?;
                rec.field_mask = fm as u128;
                d as u64
            }
            Self::FieldsOnlyWide => {
                let d = varint_decode_u32(data, &mut p)?;
                rec.field_mask = varint_decode(data, &mut p)?;
                d as u64
            }
            Self::FreqsFields => {
                let [d, freq, fm] = qint_decode::<3>(data, &mut p)?;
                rec.freq = freq;
                rec.field_mask = fm as u128;
                d as u64
            }
            Self::FreqsFieldsWide => {
                let [d, freq] = qint_decode::<2>(data, &mut p)?;
                rec.freq = freq;
                rec.field_mask = varint_decode(data, &mut p)?;
                d as u64
            }
            Self::FreqsOnly => {
                let [d, freq] = qint_decode::<2>(data, &mut p)?;
                rec.freq = freq;
                d as u64
            }
            Self::FieldsOffsets => {
                let [d, fm, olen] = qint_decode::<3>(data, &mut p)?;
                rec.field_mask = fm as u128;
                rec.offsets = take(data, &mut p, olen)?;
                d as u64
            }
            Self::FieldsOffsetsWide => {
                let [d, olen] = qint_decode::<2>(data, &mut p)?;
                rec.field_mask = varint_decode(data, &mut p)?;
                rec.offsets = take(data, &mut p, olen)?;
                d as u64
            }
            Self::OffsetsOnly => {
                let [d, olen] = qint_decode::<2>(data, &mut p)?;
                rec.offsets = take(data, &mut p, olen)?;
                d as u64
            }
            Self::FreqsOffsets => {
                let [d, freq, olen] = qint_decode::<3>(data, &mut p)?;
                rec.freq = freq;
                rec.offsets = take(data, &mut p, olen)?;
                d as u64
            }
            Self::Full => {
                let [d, freq, fm, olen] = qint_decode::<4>(data, &mut p)?;
                rec.freq = freq;
                rec.field_mask = fm as u128;
                rec.offsets = take(data, &mut p, olen)?;
                d as u64
            }
            Self::FullWide => {
                let [d, freq, olen] = qint_decode::<3>(data, &mut p)?;
                rec.freq = freq;
                rec.field_mask = varint_decode(data, &mut p)?;
                rec.offsets = take(data, &mut p, olen)?;
                d as u64
            }
            Self::Numeric | Self::NumericFloatCompression => {
                let (d, v) = numeric_decode(data, &mut p)?;
                rec.numeric = v;
                d
            }
        };
        rec.doc_id = base.wrapping_add(delta);
        *pos = p;
        Some(rec)
    }
}

/// `decode_term_record_offsets`: the `len`-byte offsets vector.
fn take<'a>(data: &'a [u8], p: &mut usize, len: u32) -> Option<&'a [u8]> {
    let s = data.get(*p..*p + len as usize)?;
    *p += len as usize;
    Some(s)
}

const TINY_TYPE: u8 = 0b00;
const FLOAT_TYPE: u8 = 0b01;
const INT_POS_TYPE: u8 = 0b10;
const INT_NEG_TYPE: u8 = 0b11;
const FLOAT32_POSITIVE: u8 = 0b000;
const FLOAT32_NEGATIVE: u8 = 0b010;
const FLOAT64_POSITIVE: u8 = 0b100;
const FLOAT64_NEGATIVE: u8 = 0b110;
const FLOAT_INFINITE: u8 = 0b001;
const FLOAT_NEGATIVE_INFINITE: u8 = 0b011;

/// `NumericFloatCompression::FLOAT_COMPRESSION_THRESHOLD`.
const FLOAT_COMPRESSION_THRESHOLD: f64 = 0.01;

/// The `Value` a number is stored as.
#[derive(Clone, Copy, Debug, PartialEq)]
enum NumValue {
    Tiny(u8),
    IntPos(u64),
    IntNeg(u64),
    F32Pos(f32),
    F32Neg(f32),
    F64Pos(f64),
    F64Neg(f64),
    Inf,
    NegInf,
}

/// `Value::from`: integral magnitudes (the saturating cast round-trips) as tiny 0..=7 or
/// sign-and-magnitude integers, infinities bare, other floats as f32 when exact (or within 0.01
/// when compressing), else f64. NaN lands on f64 with its sign.
fn num_value(value: f64, compress_floats: bool) -> NumValue {
    let abs = value.abs();
    let u = abs as u64;
    if u as f64 == abs {
        if (u & 0b111) as f64 == value {
            NumValue::Tiny(u as u8)
        } else if value.is_sign_negative() {
            NumValue::IntNeg(u)
        } else {
            NumValue::IntPos(u)
        }
    } else if value == f64::INFINITY {
        NumValue::Inf
    } else if value == f64::NEG_INFINITY {
        NumValue::NegInf
    } else {
        let f = abs as f32;
        let small = f as f64 == abs || (compress_floats && (abs - f as f64).abs() < FLOAT_COMPRESSION_THRESHOLD);
        match (small, value.is_sign_positive()) {
            (true, true) => NumValue::F32Pos(f),
            (true, false) => NumValue::F32Neg(f),
            (false, true) => NumValue::F64Pos(abs),
            (false, false) => NumValue::F64Neg(abs),
        }
    }
}

/// Little-endian bytes of `v` without the high zero bytes (none for 0).
fn trimmed(v: u64) -> ([u8; 8], usize) {
    (v.to_le_bytes(), (64 - v.leading_zeros() as usize).div_ceil(8))
}

/// `numeric::encode`: a header byte (delta byte count in bits 0-2, type in 3-4, type-specific
/// bits 5-7), the delta's trimmed little-endian bytes, then the value's bytes.
fn numeric_encode(out: &mut Vec<u8>, delta: u64, value: f64, compress_floats: bool) {
    let (db, dn) = trimmed(delta);
    let v = num_value(value, compress_floats);
    let (typ, upper) = match v {
        NumValue::Tiny(t) => (TINY_TYPE, t),
        NumValue::IntPos(i) => (INT_POS_TYPE, (trimmed(i).1 - 1) as u8),
        NumValue::IntNeg(i) => (INT_NEG_TYPE, (trimmed(i).1 - 1) as u8),
        NumValue::F32Pos(_) => (FLOAT_TYPE, FLOAT32_POSITIVE),
        NumValue::F32Neg(_) => (FLOAT_TYPE, FLOAT32_NEGATIVE),
        NumValue::F64Pos(_) => (FLOAT_TYPE, FLOAT64_POSITIVE),
        NumValue::F64Neg(_) => (FLOAT_TYPE, FLOAT64_NEGATIVE),
        NumValue::Inf => (FLOAT_TYPE, FLOAT_INFINITE),
        NumValue::NegInf => (FLOAT_TYPE, FLOAT_NEGATIVE_INFINITE),
    };
    out.push((dn as u8 & 0b111) | (typ << 3) | ((upper & 0b111) << 5));
    out.extend_from_slice(&db[..dn]);
    match v {
        NumValue::IntPos(i) | NumValue::IntNeg(i) => {
            let (b, n) = trimmed(i);
            out.extend_from_slice(&b[..n]);
        }
        NumValue::F32Pos(f) | NumValue::F32Neg(f) => out.extend_from_slice(&f.to_le_bytes()),
        NumValue::F64Pos(f) | NumValue::F64Neg(f) => out.extend_from_slice(&f.to_le_bytes()),
        NumValue::Tiny(_) | NumValue::Inf | NumValue::NegInf => {}
    }
}

/// Little-endian unsigned integer of `n` bytes.
fn read_le(data: &[u8], p: &mut usize, n: usize) -> Option<u64> {
    let s = data.get(*p..*p + n)?;
    *p += n;
    let mut b = [0u8; 8];
    b[..n].copy_from_slice(s);
    Some(u64::from_le_bytes(b))
}

/// `numeric::decode`: `(delta, value)`.
fn numeric_decode(data: &[u8], p: &mut usize) -> Option<(u64, f64)> {
    let header = *data.get(*p)?;
    *p += 1;
    let dn = (header & 0b111) as usize;
    let upper = header >> 5;
    let delta = read_le(data, p, dn)?;
    let v = match (header >> 3) & 0b11 {
        TINY_TYPE => upper as f64,
        INT_POS_TYPE => read_le(data, p, upper as usize + 1)? as f64,
        INT_NEG_TYPE => (read_le(data, p, upper as usize + 1)? as f64).copysign(-1.0),
        _ => match upper {
            FLOAT32_POSITIVE => f32::from_bits(read_le(data, p, 4)? as u32) as f64,
            FLOAT32_NEGATIVE => f32::from_bits(read_le(data, p, 4)? as u32).copysign(-1.0) as f64,
            FLOAT64_POSITIVE => f64::from_bits(read_le(data, p, 8)?),
            FLOAT64_NEGATIVE => f64::from_bits(read_le(data, p, 8)?).copysign(-1.0),
            0b001 | 0b101 => f64::INFINITY,
            _ => f64::NEG_INFINITY,
        },
    };
    Some((delta, v))
}

#[cfg(test)]
pub(super) fn num_value_kind(value: f64, compress: bool) -> &'static str {
    match num_value(value, compress) {
        NumValue::Tiny(_) => "tiny",
        NumValue::IntPos(_) => "int+",
        NumValue::IntNeg(_) => "int-",
        NumValue::F32Pos(_) | NumValue::F32Neg(_) => "f32",
        NumValue::F64Pos(_) | NumValue::F64Neg(_) => "f64",
        NumValue::Inf | NumValue::NegInf => "inf",
    }
}
