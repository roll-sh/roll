//! Per-envelope body compression for v3/v4 (v5 compresses whole segments instead, see
//! [`crate::segment`]).
//!
//! - lz4: a 4-byte big-endian uncompressed length, then an lz4 block — Cassandra's
//!   `Compressor.LZ4Compressor` (big-endian on purpose, unlike its storage compressor).
//! - snappy: a raw snappy block (no framing format).
//!
//! Decompression never allocates more than a compressed body can expand to (lz4 at most
//! 255:1, snappy at most ~22:1), nor more than the caller's limit.

use bytes::{BufMut, Bytes, BytesMut};

use crate::error::DecodeError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Compression {
    Lz4,
    Snappy,
}

impl Compression {
    /// The STARTUP `COMPRESSION` value, case-insensitively.
    pub fn from_name(s: &str) -> Option<Self> {
        if s.eq_ignore_ascii_case("lz4") {
            Some(Compression::Lz4)
        } else if s.eq_ignore_ascii_case("snappy") {
            Some(Compression::Snappy)
        } else {
            None
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Compression::Lz4 => "lz4",
            Compression::Snappy => "snappy",
        }
    }

    /// Compresses an envelope body.
    pub fn compress(self, body: &[u8]) -> Bytes {
        match self {
            Compression::Lz4 => {
                let mut out = BytesMut::with_capacity(4 + lz4_flex::block::get_maximum_output_size(body.len()));
                out.put_u32(body.len() as u32);
                let start = out.len();
                out.resize(start + lz4_flex::block::get_maximum_output_size(body.len()), 0);
                let n = lz4_flex::block::compress_into(body, &mut out[start..]).expect("output sized for the worst case");
                out.truncate(start + n);
                out.freeze()
            }
            Compression::Snappy => {
                let mut out = vec![0u8; snap::raw::max_compress_len(body.len())];
                let n = snap::raw::Encoder::new().compress(body, &mut out).expect("output sized for the worst case");
                out.truncate(n);
                Bytes::from(out)
            }
        }
    }

    /// Decompresses an envelope body of at most `max` bytes.
    pub fn decompress(self, body: &[u8], max: usize) -> Result<Bytes, DecodeError> {
        match self {
            Compression::Lz4 => {
                if body.len() < 4 {
                    return Err(DecodeError::Compression("Compressed lengths mismatch".into()));
                }
                let n = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
                let input = &body[4..];
                if n > max || n > input.len().saturating_mul(255).saturating_add(16) {
                    return Err(DecodeError::Compression(format!("lz4 body claims {n} uncompressed bytes")));
                }
                let mut out = vec![0u8; n];
                match lz4_flex::block::decompress_into(input, &mut out) {
                    Ok(got) if got == n => Ok(Bytes::from(out)),
                    _ => Err(DecodeError::Compression("Compressed lengths mismatch".into())),
                }
            }
            Compression::Snappy => {
                let bad = || DecodeError::Compression("Provided frame does not appear to be Snappy compressed".into());
                let n = snap::raw::decompress_len(body).map_err(|_| bad())?;
                if n > max || n > body.len().saturating_mul(32).saturating_add(16) {
                    return Err(bad());
                }
                let mut out = vec![0u8; n];
                let got = snap::raw::Decoder::new().decompress(body, &mut out).map_err(|_| bad())?;
                out.truncate(got);
                Ok(Bytes::from(out))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_bodies_match_the_references() {
        // READY after STARTUP {COMPRESSION: lz4|snappy}, captured from both references.
        assert_eq!(&Compression::Lz4.compress(b"")[..], b"\x00\x00\x00\x00\x00");
        assert_eq!(&Compression::Snappy.compress(b"")[..], b"\x00");
    }

    #[test]
    fn round_trip() {
        let body: Vec<u8> = (0..10_000u32).flat_map(|i| (i % 97).to_be_bytes()).collect();
        for c in [Compression::Lz4, Compression::Snappy] {
            let z = c.compress(&body);
            assert!(z.len() < body.len());
            assert_eq!(&c.decompress(&z, 1 << 20).unwrap()[..], &body[..]);
            assert!(c.decompress(&z, 100).is_err(), "limit");
            for cut in [0, 1, 3, 5, z.len() / 2, z.len() - 1] {
                assert!(c.decompress(&z[..cut], 1 << 20).is_err(), "{c:?} cut {cut}");
            }
        }
    }

    #[test]
    fn bombs_are_refused_before_allocating() {
        // lz4 claiming 4 GiB from a 5-byte body.
        assert!(Compression::Lz4.decompress(b"\xff\xff\xff\xff\x00", usize::MAX).is_err());
        // snappy varint claiming ~4 GiB.
        assert!(Compression::Snappy.decompress(b"\xff\xff\xff\xff\x0f\x00", usize::MAX).is_err());
        assert!(Compression::Snappy.decompress(b"garbage!", 1 << 20).is_err());
    }

    #[test]
    fn names() {
        assert_eq!(Compression::from_name("LZ4"), Some(Compression::Lz4));
        assert_eq!(Compression::from_name("Snappy"), Some(Compression::Snappy));
        assert_eq!(Compression::from_name("zstd"), None);
    }
}
