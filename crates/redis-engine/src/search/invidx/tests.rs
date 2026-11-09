//! Vectors from `redisearch_rs` (`qint`, `varint`, `inverted_index` codec and index tests).
// Numeric headers are grouped as value_type_delta bits.
#![allow(clippy::unusual_byte_groupings)]

use super::codec::num_value_kind;
use super::flags::*;
use super::varint::*;
use super::*;

const MAX7: u64 = 72_057_594_037_927_935;

#[test]
fn qint_examples() {
    let mut out = Vec::new();
    assert_eq!(qint_encode(&mut out, &[0xFF, 0x0FF0]), 4);
    assert_eq!(out, [0b0000_0100, 0xFF, 0xF0, 0x0F]);
    let mut p = 0;
    assert_eq!(qint_decode::<2>(&out, &mut p), Some([0xFF, 0x0FF0]));
    assert_eq!(p, 4);

    let mut out = Vec::new();
    let v = [0xFF, 0x0FF0, 0xFF00F0, 0xFF0000FF];
    assert_eq!(qint_encode(&mut out, &v), 11);
    assert_eq!(out, [0b1110_0100, 0xFF, 0xF0, 0x0F, 0xF0, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF]);
    let mut p = 0;
    assert_eq!(qint_decode::<4>(&out, &mut p), Some(v));
    assert_eq!(qint_decode::<4>(&out[..10], &mut 0), None);
}

#[test]
fn varint_vectors() {
    let cases: &[(u32, &[u8])] = &[
        (0, &[0x00]),
        (1, &[0x01]),
        (127, &[0x7F]),
        (128, &[0x80, 0x00]),
        (129, &[0x80, 0x01]),
        (255, &[0x80, 0x7F]),
        (256, &[0x81, 0x00]),
        (16383, &[0xFE, 0x7F]),
        (16384, &[0xFF, 0x00]),
        (16511, &[0xFF, 0x7F]),
        (16512, &[0x80, 0x80, 0x00]),
        (2097151, &[0xFE, 0xFE, 0x7F]),
        (2097152, &[0xFE, 0xFF, 0x00]),
        (268435455, &[0xFE, 0xFE, 0xFE, 0x7F]),
        (268435456, &[0xFE, 0xFE, 0xFF, 0x00]),
        (u32::MAX, &[0x8E, 0xFE, 0xFE, 0xFE, 0x7F]),
    ];
    for &(v, bytes) in cases {
        let mut out = Vec::new();
        assert_eq!(varint_encode(&mut out, v as u128), bytes.len());
        assert_eq!(out, bytes, "{v}");
        let mut p = 0;
        assert_eq!(varint_decode_u32(&out, &mut p), Some(v));
        assert_eq!(p, bytes.len());
    }
    let mut out = Vec::new();
    assert_eq!(varint_encode(&mut out, u128::MAX), 19);
    assert_eq!(varint_decode(&out, &mut 0), Some(u128::MAX));
    assert_eq!(varint_decode(&[0x80], &mut 0), None);
}

#[test]
fn offsets_writer_roundtrip() {
    let mut w = OffsetsWriter::new();
    for v in [10, 20, 30, 40, 50] {
        assert_eq!(w.write(v), 1);
    }
    assert_eq!(w.bytes(), [10, 10, 10, 10, 10]);
    assert_eq!(w.count(), 5);
    for vals in [&[100u32, 90, 80, 70, 60][..], &[u32::MAX - 10, 5], &[100, 100, 100], &[u32::MAX, u32::MAX], &[1000, 10, 2000, 20, 3000, 30]] {
        let mut w = OffsetsWriter::new();
        vals.iter().for_each(|&v| {
            w.write(v);
        });
        assert_eq!(w.count(), vals.len());
        assert_eq!(offsets(w.bytes()).collect::<Vec<_>>(), vals);
    }
    w.reset();
    assert!(w.is_empty());
    assert_eq!(w.count(), 0);
    w.write(200);
    assert_eq!(w.bytes(), [0x80, 0x48]);
}

/// Encodes a record with `delta` and checks the bytes and the decode from `doc_id - delta`.
fn check(enc: Encoding, delta: u32, freq: u32, field_mask: u128, offs: &[u8], expected: &[u8]) {
    let doc_id = 4294967296u64;
    let rec = Record::term(doc_id, freq, field_mask, offs);
    let mut out = Vec::new();
    assert_eq!(enc.encode(&mut out, delta as u64, &rec), expected.len(), "{enc:?} {delta}");
    assert_eq!(out, expected, "{enc:?} {delta}");
    let mut p = 0;
    let d = enc.decode(&out, &mut p, doc_id - delta as u64).unwrap();
    assert_eq!(p, out.len());
    assert_eq!(d.doc_id, doc_id);
    let base = enc.base_record();
    let stores = |f: u32| enc_flags(enc) & f != 0;
    assert_eq!(d.freq, if stores(STORE_FREQS) { freq } else { base.freq }, "{enc:?}");
    assert_eq!(d.field_mask, if stores(STORE_FIELD_FLAGS) { field_mask } else { 0 }, "{enc:?}");
    assert_eq!(d.offsets, if stores(STORE_TERM_OFFSETS) { offs } else { &[] }, "{enc:?}");
}

fn enc_flags(enc: Encoding) -> u32 {
    (0..=0xFFu32).find(|&f| f & !STORAGE_MASK == 0 && Encoding::from_flags(f, false, false) == Some(enc)).unwrap_or(0)
}

#[test]
fn encode_doc_ids_only() {
    for (delta, exp) in [
        (0u32, &[0u8][..]),
        (10, &[10]),
        (256, &[129, 0]),
        (65536, &[130, 255, 0]),
        (u16::MAX as u32, &[130, 254, 127]),
        (u32::MAX, &[142, 254, 254, 254, 127]),
    ] {
        check(Encoding::DocIdsOnly, delta, 1, 0, &[], exp);
    }
}

#[test]
fn encode_raw_doc_ids_only() {
    for (delta, exp) in [
        (0u32, [0u8, 0, 0, 0]),
        (10, [10, 0, 0, 0]),
        (256, [0, 1, 0, 0]),
        (65536, [0, 0, 1, 0]),
        (u16::MAX as u32, [255, 255, 0, 0]),
        (u32::MAX, [255, 255, 255, 255]),
    ] {
        let mut out = Vec::new();
        Encoding::RawDocIdsOnly.encode(&mut out, delta as u64, &Record::doc(5));
        assert_eq!(out, exp);
        assert_eq!(Encoding::RawDocIdsOnly.decode(&out, &mut 0, 7).unwrap().doc_id, 7 + delta as u64);
    }
}

#[test]
fn encode_fields_only() {
    let m32 = u32::MAX as u128;
    for (delta, fm, exp) in [
        (0u32, 1u128, &[0u8, 0, 1][..]),
        (10, m32, &[12, 10, 255, 255, 255, 255]),
        (256, 1, &[1, 0, 1, 1]),
        (65536, 1, &[2, 0, 0, 1, 1]),
        (u16::MAX as u32, 1, &[1, 255, 255, 1]),
        (u32::MAX, 1, &[3, 255, 255, 255, 255, 1]),
        (u32::MAX, m32, &[15, 255, 255, 255, 255, 255, 255, 255, 255]),
    ] {
        check(Encoding::FieldsOnly, delta, 1, fm, &[], exp);
    }
    let mut wide_max = vec![142, 254, 254, 254, 127, 130];
    wide_max.extend([254; 17]);
    wide_max.push(127);
    for (delta, fm, exp) in [
        (0u32, 1u128, &[0u8, 1][..]),
        (10, m32, &[10, 142, 254, 254, 254, 127]),
        (256, 1, &[129, 0, 1]),
        (65536, 1, &[130, 255, 0, 1]),
        (u16::MAX as u32, 1, &[130, 254, 127, 1]),
        (u32::MAX, 1, &[142, 254, 254, 254, 127, 1]),
        (u32::MAX, m32, &[142, 254, 254, 254, 127, 142, 254, 254, 254, 127]),
        (u32::MAX, u128::MAX, &wide_max),
    ] {
        check(Encoding::FieldsOnlyWide, delta, 1, fm, &[], exp);
    }
}

#[test]
fn encode_freqs_fields() {
    let m32 = u32::MAX as u128;
    for (delta, freq, fm, exp) in [
        (0u32, 1u32, 1u128, &[0u8, 0, 1, 1][..]),
        (10, 5, m32, &[48, 10, 5, 255, 255, 255, 255]),
        (256, 1, 1, &[1, 0, 1, 1, 1]),
        (65536, 1, 1, &[2, 0, 0, 1, 1, 1]),
        (u16::MAX as u32, 1, 1, &[1, 255, 255, 1, 1]),
        (u32::MAX, 1, 1, &[3, 255, 255, 255, 255, 1, 1]),
        (u32::MAX, u32::MAX, m32, &[63, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255]),
    ] {
        check(Encoding::FreqsFields, delta, freq, fm, &[], exp);
    }
    let mut wide_max = vec![15, 255, 255, 255, 255, 255, 255, 255, 255, 130];
    wide_max.extend([254; 17]);
    wide_max.push(127);
    for (delta, freq, fm, exp) in [
        (0u32, 1u32, 1u128, &[0u8, 0, 1, 1][..]),
        (10, 5, m32, &[0, 10, 5, 142, 254, 254, 254, 127]),
        (256, 1, 1, &[1, 0, 1, 1, 1]),
        (65536, 1, 1, &[2, 0, 0, 1, 1, 1]),
        (u16::MAX as u32, 1, 1, &[1, 255, 255, 1, 1]),
        (u32::MAX, 1, 1, &[3, 255, 255, 255, 255, 1, 1]),
        (u32::MAX, u32::MAX, m32, &[15, 255, 255, 255, 255, 255, 255, 255, 255, 142, 254, 254, 254, 127]),
        (u32::MAX, u32::MAX, u128::MAX, &wide_max),
    ] {
        check(Encoding::FreqsFieldsWide, delta, freq, fm, &[], exp);
    }
}

#[test]
fn encode_freqs_only() {
    for (freq, delta, exp) in [
        (0u32, 0u32, &[0u8, 0, 0][..]),
        (0, 1, &[0, 1, 0]),
        (2, 0, &[0, 0, 2]),
        (2, 1, &[0, 1, 2]),
        (256, 0, &[4, 0, 0, 1]),
        (256, 256, &[5, 0, 1, 0, 1]),
        (2, 65536, &[2, 0, 0, 1, 2]),
        (65536, 65536, &[10, 0, 0, 1, 0, 0, 1]),
        (2, u32::MAX, &[3, 255, 255, 255, 255, 2]),
        (u32::MAX, u32::MAX, &[15, 255, 255, 255, 255, 255, 255, 255, 255]),
    ] {
        check(Encoding::FreqsOnly, delta, freq, 0, &[], exp);
    }
}

#[test]
fn encode_offsets_variants() {
    let m32 = u32::MAX as u128;
    let o3: &[u8] = &[1, 2, 3];
    let o4: &[u8] = &[1, 2, 3, 4];
    for (delta, offs, exp) in [
        (0u32, o3, &[0u8, 0, 3, 1, 2, 3][..]),
        (10, o4, &[0, 10, 4, 1, 2, 3, 4]),
        (256, o3, &[1, 0, 1, 3, 1, 2, 3]),
        (65536, o3, &[2, 0, 0, 1, 3, 1, 2, 3]),
        (u16::MAX as u32, o3, &[1, 255, 255, 3, 1, 2, 3]),
        (u32::MAX, o3, &[3, 255, 255, 255, 255, 3, 1, 2, 3]),
    ] {
        check(Encoding::OffsetsOnly, delta, 1, 0, offs, exp);
    }
    for (delta, freq, offs, exp) in [
        (0u32, 1u32, o3, &[0u8, 0, 1, 3, 1, 2, 3][..]),
        (10, 2, o4, &[0, 10, 2, 4, 1, 2, 3, 4]),
        (256, 3, o3, &[1, 0, 1, 3, 3, 1, 2, 3]),
        (65536, 4, o3, &[2, 0, 0, 1, 4, 3, 1, 2, 3]),
        (u16::MAX as u32, 5, o3, &[1, 255, 255, 5, 3, 1, 2, 3]),
        (u32::MAX, 6, o3, &[3, 255, 255, 255, 255, 6, 3, 1, 2, 3]),
    ] {
        check(Encoding::FreqsOffsets, delta, freq, 0, offs, exp);
    }
    for (delta, fm, offs, exp) in [
        (0u32, 1u128, o3, &[0u8, 0, 1, 3, 1, 2, 3][..]),
        (10, m32, o4, &[12, 10, 255, 255, 255, 255, 4, 1, 2, 3, 4]),
        (256, 1, o3, &[1, 0, 1, 1, 3, 1, 2, 3]),
        (65536, 1, o3, &[2, 0, 0, 1, 1, 3, 1, 2, 3]),
        (u16::MAX as u32, 1, o3, &[1, 255, 255, 1, 3, 1, 2, 3]),
        (u32::MAX, 1, o3, &[3, 255, 255, 255, 255, 1, 3, 1, 2, 3]),
    ] {
        check(Encoding::FieldsOffsets, delta, 1, fm, offs, exp);
    }
    let hundred = [1u8; 100];
    let mut big = vec![3, 255, 255, 255, 255, 100, 142, 254, 254, 254, 127];
    big.extend([1; 100]);
    let mut huge = vec![3, 255, 255, 255, 255, 100, 130];
    huge.extend([254; 17]);
    huge.push(127);
    huge.extend([1; 100]);
    for (delta, fm, offs, exp) in [
        (0u32, 1u128, o3, &[0u8, 0, 3, 1, 1, 2, 3][..]),
        (10, m32, o4, &[0, 10, 4, 142, 254, 254, 254, 127, 1, 2, 3, 4]),
        (256, 1, o3, &[1, 0, 1, 3, 1, 1, 2, 3]),
        (65536, 1, o3, &[2, 0, 0, 1, 3, 1, 1, 2, 3]),
        (u16::MAX as u32, 1, o3, &[1, 255, 255, 3, 1, 1, 2, 3]),
        (u32::MAX, 1, o3, &[3, 255, 255, 255, 255, 3, 1, 1, 2, 3]),
        (u32::MAX, m32, &hundred, &big),
        (u32::MAX, u128::MAX, &hundred, &huge),
    ] {
        check(Encoding::FieldsOffsetsWide, delta, 1, fm, offs, exp);
    }
}

#[test]
fn encode_full() {
    let m32 = u32::MAX as u128;
    let o3: &[u8] = &[1, 2, 3];
    let o4: &[u8] = &[1, 2, 3, 4];
    let hundred = [1u8; 100];
    let mut max = vec![63];
    max.extend([255; 12]);
    max.push(100);
    max.extend([1; 100]);
    for (delta, freq, fm, offs, exp) in [
        (0u32, 1u32, 1u128, o3, &[0u8, 0, 1, 1, 3, 1, 2, 3][..]),
        (10, 5, m32, o4, &[48, 10, 5, 255, 255, 255, 255, 4, 1, 2, 3, 4]),
        (256, 1, 1, o3, &[1, 0, 1, 1, 1, 3, 1, 2, 3]),
        (65536, 1, 1, o3, &[2, 0, 0, 1, 1, 1, 3, 1, 2, 3]),
        (u16::MAX as u32, 1, 1, o3, &[1, 255, 255, 1, 1, 3, 1, 2, 3]),
        (u32::MAX, 1, 1, o3, &[3, 255, 255, 255, 255, 1, 1, 3, 1, 2, 3]),
        (u32::MAX, u32::MAX, m32, &hundred, &max),
    ] {
        check(Encoding::Full, delta, freq, fm, offs, exp);
    }
    let mut big = vec![15, 255, 255, 255, 255, 255, 255, 255, 255, 100, 142, 254, 254, 254, 127];
    big.extend([1; 100]);
    let mut huge = vec![15, 255, 255, 255, 255, 255, 255, 255, 255, 100, 130];
    huge.extend([254; 17]);
    huge.push(127);
    huge.extend([1; 100]);
    for (delta, freq, fm, offs, exp) in [
        (0u32, 1u32, 1u128, o3, &[0u8, 0, 1, 3, 1, 1, 2, 3][..]),
        (10, 5, m32, o4, &[0, 10, 5, 4, 142, 254, 254, 254, 127, 1, 2, 3, 4]),
        (256, 1, 1, o3, &[1, 0, 1, 1, 3, 1, 1, 2, 3]),
        (65536, 1, 1, o3, &[2, 0, 0, 1, 1, 3, 1, 1, 2, 3]),
        (u16::MAX as u32, 1, 1, o3, &[1, 255, 255, 1, 3, 1, 1, 2, 3]),
        (u32::MAX, 1, 1, o3, &[3, 255, 255, 255, 255, 1, 3, 1, 1, 2, 3]),
        (u32::MAX, u32::MAX, m32, &hundred, &big),
        (u32::MAX, u32::MAX, u128::MAX, &hundred, &huge),
    ] {
        check(Encoding::FullWide, delta, freq, fm, offs, exp);
    }
}

#[test]
fn decode_truncated() {
    assert!(Encoding::Full.decode(&[0, 0], &mut 0, 100).is_none());
    assert!(Encoding::Full.decode(&[], &mut 0, 100).is_none());
    // the offsets vector claims 3 bytes but has 2
    let mut p = 0;
    assert!(Encoding::Full.decode(&[0, 0, 1, 1, 3, 1, 2], &mut p, 100).is_none());
    assert_eq!(p, 0);
    assert!(Encoding::Numeric.decode(&[], &mut 0, 0).is_none());
}

#[test]
fn full_records_sequence() {
    // test_seek_full: six records from base 10.
    let buf = [
        0, 0, 1, 10, 3, 1, 2, 3, 0, 10, 2, 12, 4, 1, 2, 3, 4, 0, 10, 3, 13, 4, 5, 6, 7, 8, 0, 5, 1, 10, 2, 10, 11, 0, 20, 9, 4, 2,
        20, 21, 0, 5, 1, 4, 2, 20, 21,
    ];
    let mut p = 0;
    let mut base = 10;
    let mut got = Vec::new();
    while p < buf.len() {
        let r = Encoding::Full.decode(&buf, &mut p, base).unwrap();
        base = r.doc_id;
        got.push((r.doc_id, r.freq, r.field_mask, r.offsets.to_vec()));
    }
    assert_eq!(got[2], (30, 3, 13, vec![5, 6, 7, 8]));
    assert_eq!(got[4], (55, 9, 4, vec![20, 21]));
    assert_eq!(got.len(), 6);
    assert_eq!(got[5].0, 60);
}

fn check_numeric(delta: u64, value: f64, expected: &[u8]) {
    let mut out = Vec::new();
    let rec = Record::numeric(u64::MAX, value);
    assert!(Encoding::Numeric.delta_fits(delta));
    assert_eq!(Encoding::Numeric.encode(&mut out, delta, &rec), expected.len(), "{value}");
    assert_eq!(out, expected, "{value}");
    let mut p = 0;
    let d = Encoding::Numeric.decode(&out, &mut p, u64::MAX - delta).unwrap();
    assert_eq!(p, out.len());
    assert_eq!(d.doc_id, u64::MAX);
    assert_eq!(d.numeric.to_bits(), if value == 0.0 { 0 } else { value.to_bits() }, "{value}");
    assert_eq!((d.freq, d.field_mask), (1, u128::MAX));
}

#[test]
fn numeric_vectors() {
    let d7 = [255u8; 7];
    let with = |h: u8, d: &[u8], v: &[u8]| [&[h][..], d, v].concat();
    check_numeric(0, 2.0, &[0b010_00_000]);
    check_numeric(2, 7.0, &[0b111_00_001, 2]);
    check_numeric(MAX7, 0.0, &with(0b000_00_111, &d7, &[]));
    check_numeric(0, 0.0, &[0]);
    check_numeric(0, -0.0, &[0]);
    check_numeric(1, 16.0, &[0b000_10_001, 1, 16]);
    check_numeric(0, 256.0, &[0b001_10_000, 0, 1]);
    check_numeric(MAX7, u64::MAX as f64, &with(0b111_10_111, &d7, &[255; 8]));
    check_numeric(0, -1.0, &[0b000_11_000, 1]);
    check_numeric(1, -16.0, &[0b000_11_001, 1, 16]);
    check_numeric(0, -256.0, &[0b001_11_000, 0, 1]);
    check_numeric(MAX7, -(u64::MAX as f64), &with(0b111_11_111, &d7, &[255; 8]));
    check_numeric(0, 3.125, &[0b000_01_000, 0, 0, 72, 64]);
    check_numeric(MAX7, 3.125, &with(0b000_01_111, &d7, &[0, 0, 72, 64]));
    check_numeric(0, -3.125, &[0b010_01_000, 0, 0, 72, 64]);
    check_numeric(MAX7, -3.125, &with(0b010_01_111, &d7, &[0, 0, 72, 64]));
    check_numeric(0, f64::INFINITY, &[0b001_01_000]);
    check_numeric(MAX7, f64::INFINITY, &with(0b001_01_111, &d7, &[]));
    check_numeric(0, f64::NEG_INFINITY, &[0b011_01_000]);
    check_numeric(MAX7, f64::NEG_INFINITY, &with(0b011_01_111, &d7, &[]));
    let f = [203, 161, 69, 182, 243, 253, 8, 64];
    check_numeric(0, 3.124, &with(0b100_01_000, &[], &f));
    check_numeric(MAX7, 3.124, &with(0b100_01_111, &d7, &f));
    check_numeric(0, -3.124, &with(0b110_01_000, &[], &f));
    check_numeric(MAX7, -3.124, &with(0b110_01_111, &d7, &f));
}

#[test]
fn numeric_float_compression() {
    let mut out = Vec::new();
    Encoding::NumericFloatCompression.encode(&mut out, 0, &Record::numeric(0, 3.124));
    assert_eq!(out, [0b000_01_000, 158, 239, 71, 64]);
    let d = Encoding::Numeric.decode(&out, &mut 0, 0).unwrap();
    assert!((d.numeric - 3.124).abs() < 0.01);
    assert_eq!(num_value_kind(f64::NAN, false), "f64");
    assert_eq!(num_value_kind(1e20, false), "f64");
    assert_eq!(num_value_kind(8.0, false), "int+");
    assert_eq!(num_value_kind(-3.0, false), "int-");
    assert_eq!(num_value_kind(0.5, false), "f32");
}

#[test]
fn numeric_delta_limits() {
    assert!(Encoding::Numeric.delta_fits(u32::MAX as u64 + 1));
    assert!(Encoding::Numeric.delta_fits((1 << 56) - 1));
    assert!(!Encoding::Numeric.delta_fits(1 << 56));
    assert!(Encoding::Full.delta_fits(u32::MAX as u64));
    assert!(!Encoding::Full.delta_fits(u32::MAX as u64 + 1));
}

#[test]
fn encoder_choice() {
    use Encoding::*;
    let cases = [
        (STORE_FREQS | STORE_TERM_OFFSETS | STORE_FIELD_FLAGS, Full),
        (STORE_FREQS | STORE_TERM_OFFSETS | STORE_FIELD_FLAGS | WIDE_SCHEMA, FullWide),
        (STORE_FREQS | STORE_FIELD_FLAGS, FreqsFields),
        (STORE_FREQS | STORE_FIELD_FLAGS | WIDE_SCHEMA, FreqsFieldsWide),
        (STORE_FREQS, FreqsOnly),
        (STORE_FIELD_FLAGS, FieldsOnly),
        (STORE_FIELD_FLAGS | WIDE_SCHEMA, FieldsOnlyWide),
        (STORE_FIELD_FLAGS | STORE_TERM_OFFSETS, FieldsOffsets),
        (STORE_FIELD_FLAGS | STORE_TERM_OFFSETS | WIDE_SCHEMA, FieldsOffsetsWide),
        (STORE_TERM_OFFSETS, OffsetsOnly),
        (STORE_FREQS | STORE_TERM_OFFSETS, FreqsOffsets),
        (DOC_IDS_ONLY, DocIdsOnly),
        (STORE_NUMERIC, Numeric),
    ];
    for (f, e) in cases {
        // flags outside the storage mask (multi value, byte offsets, phonetics...) don't matter
        assert_eq!(Encoding::from_flags(f | HAS_MULTI_VALUE | STORE_BYTE_OFFSETS | 0x400, false, false), Some(e));
    }
    assert_eq!(Encoding::from_flags(DOC_IDS_ONLY, true, false), Some(RawDocIdsOnly));
    assert_eq!(Encoding::from_flags(STORE_NUMERIC, true, true), Some(NumericFloatCompression));
    assert_eq!(Encoding::from_flags(STORE_FREQS | STORE_TERM_OFFSETS | STORE_FIELD_FLAGS, true, true), Some(Full));
    assert_eq!(Encoding::from_flags(WIDE_SCHEMA, false, false), None);
    assert_eq!(Encoding::from_flags(STORE_NUMERIC | STORE_FREQS, false, false), None);
    assert_eq!(Encoding::from_flags(STORE_FREQS | WIDE_SCHEMA, false, false), None);
}

#[test]
fn initial_sizes() {
    assert_eq!(new_inverted_index(DOC_IDS_ONLY, false, false).unwrap().1, 24);
    assert_eq!(new_inverted_index(STORE_NUMERIC, false, false).unwrap().1, 32);
    assert_eq!(new_inverted_index(0x13, false, false).unwrap().1, 40);
    assert_eq!(new_inverted_index(STORE_FREQS, false, false).unwrap().1, 24);
    assert_eq!(new_inverted_index(STORE_FREQS | STORE_TERM_OFFSETS, false, false).unwrap().1, 24);
    assert_eq!(new_inverted_index(STORE_FIELD_FLAGS | WIDE_SCHEMA, false, false).unwrap().1, 40);
    assert!(new_inverted_index(WIDE_SCHEMA, false, false).is_none());
}

#[test]
fn growth_sequence() {
    let mut seq = vec![0];
    for _ in 0..10 {
        let c = *seq.last().unwrap();
        seq.push(grown_capacity(c, c + 1));
    }
    assert_eq!(seq, [0, 1, 2, 3, 4, 5, 7, 9, 11, 14, 17]);
    assert_eq!(grown_capacity(4, 15), 17);
    assert_eq!(grown_capacity(0, 4), 4);
    assert_eq!(grown_capacity(10 << 20, (10 << 20) + 1), (11 << 20));
}

#[test]
fn adding_records_mem_growth() {
    // `adding_records`, with the raw encoder's 4-byte records standing in for the dummy encoder
    let mut ii = InvertedIndex::new(DOC_IDS_ONLY, true, false).unwrap();
    assert_eq!(ii.memory_usage(), 24);
    let out = ii.add_record(&Record::doc(10));
    assert_eq!(out, AddOutcome { mem_growth: 8 + 48 + 4, blocks_added: 1 });
    assert_eq!(ii.memory_usage(), 24 + 60);
    assert_eq!(ii.blocks()[0].data(), [0, 0, 0, 0]);
    let out = ii.add_record(&Record::doc(11));
    assert_eq!(out, AddOutcome { mem_growth: 5, blocks_added: 0 });
    let b = &ii.blocks()[0];
    assert_eq!(b.data(), [0, 0, 0, 0, 1, 0, 0, 0]);
    assert_eq!((b.num_entries(), b.first_doc_id(), b.last_doc_id(), b.capacity()), (2, 10, 11, 9));
    assert_eq!(ii.unique_docs(), 2);
    assert_eq!(ii.memory_usage(), 24 + 65);

    // a delta past u32 (from the block's first doc) starts a block
    let big = u32::MAX as u64 + 11;
    let out = ii.add_record(&Record::doc(big));
    assert_eq!(out, AddOutcome { mem_growth: 48 + 4, blocks_added: 1 });
    assert_eq!((ii.blocks()[1].first_doc_id(), ii.blocks()[1].data()), (big, &[0u8, 0, 0, 0][..]));
    assert_eq!(ii.records().iter().map(|r| r.doc_id).collect::<Vec<_>>(), [10, 11, big]);
}

#[test]
fn duplicates() {
    let mut ii = InvertedIndex::new(DOC_IDS_ONLY, false, false).unwrap();
    ii.add_record(&Record::doc(10));
    let out = ii.add_record(&Record::doc(10));
    assert_eq!(out, AddOutcome::default());
    assert_eq!(ii.blocks()[0].data(), [0]);
    assert_eq!((ii.unique_docs(), ii.flags()), (1, DOC_IDS_ONLY));

    let mut ii = InvertedIndex::new(STORE_NUMERIC, false, false).unwrap();
    assert_eq!(ii.add_record(&Record::numeric(10, 1.0)).mem_growth, 56 + 1);
    assert_eq!(ii.add_record(&Record::numeric(10, 2.0)).mem_growth, 1);
    assert_eq!(ii.blocks()[0].data(), [0b001_00_000, 0b010_00_000]);
    assert_eq!((ii.unique_docs(), ii.number_of_entries()), (1, 2));
    assert_eq!(ii.flags(), STORE_NUMERIC | HAS_MULTI_VALUE);
    assert_eq!(ii.memory_usage(), 32 + 56 + 2);
    let vals: Vec<_> = ii.records().iter().map(|r| (r.doc_id, r.numeric)).collect();
    assert_eq!(vals, [(10, 1.0), (10, 2.0)]);

    // the field mask is tracked even for a dropped duplicate
    let mut ii = InvertedIndex::new(0x13, false, false).unwrap();
    ii.add_record(&Record::term(3, 1, 0b1, &[1]));
    assert_eq!(ii.add_record(&Record::term(3, 1, 0b100, &[1])), AddOutcome::default());
    assert_eq!(ii.field_mask(), 0b101);
    assert_eq!(ii.number_of_entries(), 0);
}

#[test]
fn full_index_accounting() {
    let mut ii = InvertedIndex::new(STORE_FREQS | STORE_TERM_OFFSETS | STORE_FIELD_FLAGS | STORE_BYTE_OFFSETS, false, false).unwrap();
    assert_eq!(ii.encoding(), Encoding::Full);
    let mut w = OffsetsWriter::new();
    w.write(1);
    // qint [0, 1, 1, 1] + [1]: 6 bytes, capacity 7
    assert_eq!(ii.add_record(&Record::term(1, 1, 1, w.bytes())).mem_growth, 56 + 7);
    w.reset();
    w.write(1);
    w.write(5);
    // qint [1, 2, 3, 2] + [1, 4]: 7 more bytes, capacity 14
    assert_eq!(ii.add_record(&Record::term(2, 2, 3, w.bytes())).mem_growth, 7);
    assert_eq!(ii.blocks()[0].data(), [0, 0, 1, 1, 1, 1, 0, 1, 2, 3, 2, 1, 4]);
    assert_eq!(ii.memory_usage(), 40 + 56 + 14);
    let recs = ii.records();
    assert_eq!(recs[1].doc_id, 2);
    assert_eq!(recs[1].freq, 2);
    assert_eq!(recs[1].field_mask, 3);
    assert_eq!(offsets(recs[1].offsets).collect::<Vec<_>>(), [1, 5]);
    assert_eq!(ii.field_mask(), 3);

    // 100 entries per block
    for d in 3..=100 {
        ii.add_record(&Record::term(d, 1, 1, &[1]));
    }
    assert_eq!(ii.number_of_blocks(), 1);
    let before = ii.memory_usage();
    let out = ii.add_record(&Record::term(101, 1, 1, &[1]));
    assert_eq!(out, AddOutcome { mem_growth: 48 + 7, blocks_added: 1 });
    assert_eq!(ii.memory_usage(), before + 55);
    assert_eq!(ii.blocks()[1].data(), [0, 0, 1, 1, 1, 1]);
}

#[test]
fn block_entries_and_overflow() {
    let mut ii = InvertedIndex::new(DOC_IDS_ONLY, false, false).unwrap();
    for d in 1..=1000 {
        ii.add_record(&Record::doc(d));
    }
    assert_eq!(ii.number_of_blocks(), 1);
    assert_eq!(ii.add_record(&Record::doc(1001)), AddOutcome { mem_growth: 48 + 1, blocks_added: 1 });
    assert_eq!(ii.add_record(&Record::doc(1002)).blocks_added, 0);

    // `test_inverted_index_usage`: deltas of exactly u32::MAX still fit
    let mut ii = InvertedIndex::new(DOC_IDS_ONLY, false, false).unwrap();
    for i in 0..1_002u64 {
        ii.add_record(&Record::doc(i * u32::MAX as u64));
    }
    assert_eq!((ii.unique_docs(), ii.number_of_blocks()), (1_002, 2));

    // a delta overflow while the last block has room: the old block goes back, a new one starts
    let mut ii = InvertedIndex::new(DOC_IDS_ONLY, false, false).unwrap();
    ii.add_record(&Record::doc(10));
    let out = ii.add_record(&Record::doc(u32::MAX as u64 + 11));
    assert_eq!(out, AddOutcome { mem_growth: 48 + 1, blocks_added: 1 });
    assert_eq!(ii.memory_usage(), 24 + 8 + 96 + 2);
}

#[test]
fn reader_walk_and_seek() {
    let mut ii = InvertedIndex::new(DOC_IDS_ONLY, false, false).unwrap();
    for id in 0..3_200 {
        ii.add_record(&Record::doc(id));
    }
    assert_eq!(ii.number_of_blocks(), 4);
    let mut r = ii.reader();
    for expected in 0..1_100 {
        assert_eq!(r.next_record().unwrap().doc_id, expected);
    }
    for expected in 1_805..2_200 {
        assert_eq!(r.seek(expected).unwrap().doc_id, expected);
    }
    assert!(r.skip_to(3_100));
    for expected in 3_000..3_200 {
        assert_eq!(r.next_record().unwrap().doc_id, expected);
    }
    assert!(r.next_record().is_none());
    r.reset();
    assert_eq!(r.seek(2_999).unwrap().doc_id, 2_999);
    assert!(r.seek(3_200).is_none());

    // raw doc ids decode relative to each block's first doc
    let mut ii = InvertedIndex::new(DOC_IDS_ONLY, true, false).unwrap();
    let ids: Vec<u64> = (0..2_500).map(|i| i * 3 + 7).collect();
    ids.iter().for_each(|&d| {
        ii.add_record(&Record::doc(d));
    });
    assert_eq!(ii.records().iter().map(|r| r.doc_id).collect::<Vec<_>>(), ids);
    assert_eq!(ii.reader().seek(1_500).unwrap().doc_id, 1_501);

    let empty = InvertedIndex::new(STORE_FREQS, false, false).unwrap();
    assert!(empty.reader().next().is_none());
    assert!(empty.reader().seek(1).is_none());
}

#[test]
fn wide_and_freqs_indexes_roundtrip() {
    for flags in [0x93u32, 0x92, 0x82, 0x83, 0x10, 0x11, 0x01, 0x02, 0x03, 0x12] {
        let mut ii = InvertedIndex::new(flags, false, false).unwrap();
        let mut w = OffsetsWriter::new();
        let mut expect = Vec::new();
        for d in (1..500u64).map(|d| d * 7) {
            w.reset();
            w.write(d as u32);
            w.write(d as u32 + 3);
            let fm = if flags & WIDE_SCHEMA != 0 { 1u128 << (d % 100) } else { 1 << (d % 30) };
            ii.add_record(&Record::term(d, d as u32 % 9 + 1, fm, w.bytes()));
            expect.push((d, d as u32 % 9 + 1, fm, w.bytes().to_vec()));
        }
        let base = ii.encoding().base_record();
        for (r, (d, f, fm, o)) in ii.records().iter().zip(&expect) {
            assert_eq!(r.doc_id, *d);
            assert_eq!(r.freq, if flags & STORE_FREQS != 0 { *f } else { base.freq });
            assert_eq!(r.field_mask, if flags & STORE_FIELD_FLAGS != 0 { *fm } else { 0 });
            assert_eq!(r.offsets, if flags & STORE_TERM_OFFSETS != 0 { &o[..] } else { &[] });
        }
        assert_eq!(ii.records().len(), expect.len());
        let blocks_mem: usize = ii.blocks().iter().map(|b| b.capacity()).sum();
        assert_eq!(ii.memory_usage(), ii.encoding().base_size() + 8 + 48 * 5 + blocks_mem);
    }
}
