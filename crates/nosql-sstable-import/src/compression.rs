//! `CompressionInfo.db` and chunk decompression (`org.apache.cassandra.io.compress.
//! CompressionMetadata`, `LZ4Compressor`, `SnappyCompressor`, `DeflateCompressor`,
//! `ZstdCompressor`; ScyllaDB's `LZ4WithDictsCompressor` / `ZstdWithDictsCompressor` keep their
//! dictionary in the options as `.dictionary.NNNNNNNN` parts, per its documentation).
//!
//! ```text
//! utf compressor  be32 option-count (utf key, utf value)*  be32 chunk-length
//! [be32 max-compressed-length (na+)]  be64 data-length  be32 chunk-count  be64 offset*
//! ```
//!
//! The header is parsed whole; chunk offsets are read on demand (a large SSTable has millions).

use crate::input::{In, SliceIn};
use anyhow::{Context, Result, bail};
use bytes::Bytes;

/// Largest chunk accepted (Cassandra's `chunk_length_in_kb` is a power of two, at most 2 GiB in
/// theory; real tables use 4–64 KiB).
pub const MAX_CHUNK: u32 = 64 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Codec {
    Noop,
    Lz4,
    Snappy,
    Deflate,
    Zstd,
    Lz4Dict(Bytes),
    ZstdDict(Bytes),
}

#[derive(Clone, Debug)]
pub struct CompressionInfo {
    pub class: String,
    pub codec: Codec,
    pub chunk_length: u32,
    /// Chunks stored at or above this length are uncompressed (`na`+).
    pub max_compressed_length: u32,
    /// Uncompressed length of the data file.
    pub data_length: u64,
    pub chunks: u64,
    /// Where the offsets start in CompressionInfo.db.
    pub offsets_at: u64,
}

impl CompressionInfo {
    /// Parse the header from the start of CompressionInfo.db (`head` holds at least the header;
    /// `file_len` is the file's size, to check the offset table fits).
    pub fn parse(head: Bytes, file_len: u64, has_max_compressed_length: bool) -> Result<CompressionInfo> {
        let mut r = SliceIn::new(head);
        let class = r.utf().context("CompressionInfo.db")?;
        let n = r.be32()?;
        let n = r.count(n as u64, 4)?;
        let mut dict: Vec<(String, Bytes)> = Vec::new();
        for _ in 0..n {
            let k = r.utf()?;
            let v = r.short_bytes()?;
            if k.starts_with(".dictionary.") {
                dict.push((k, v));
            }
        }
        let chunk_length = r.be32()?;
        if chunk_length == 0 || chunk_length > MAX_CHUNK {
            bail!("corrupt CompressionInfo.db: chunk length {chunk_length}");
        }
        let max_compressed_length = if has_max_compressed_length { r.be32()? } else { u32::MAX };
        let data_length = r.be64()?;
        let chunks = r.be32()? as u64;
        let offsets_at = r.pos() as u64;
        if offsets_at + chunks * 8 > file_len {
            bail!("corrupt CompressionInfo.db: {chunks} chunk offsets in {file_len} bytes");
        }
        if chunks.saturating_mul(chunk_length as u64) < data_length || (chunks > 0 && (chunks - 1) * chunk_length as u64 >= data_length.max(1)) {
            bail!("corrupt CompressionInfo.db: {chunks} chunks of {chunk_length} for {data_length} bytes");
        }
        dict.sort_by(|a, b| a.0.cmp(&b.0));
        let dict = || -> Bytes { Bytes::from(dict.iter().flat_map(|(_, v)| v.iter().copied()).collect::<Vec<u8>>()) };
        let short = class.rsplit('.').next().unwrap_or(&class);
        let codec = match short {
            "LZ4Compressor" => Codec::Lz4,
            "SnappyCompressor" => Codec::Snappy,
            "DeflateCompressor" => Codec::Deflate,
            "ZstdCompressor" => Codec::Zstd,
            "NoopCompressor" => Codec::Noop,
            "LZ4WithDictsCompressor" => {
                let d = dict();
                if d.is_empty() { Codec::Lz4 } else { Codec::Lz4Dict(d) }
            }
            "ZstdWithDictsCompressor" => {
                let d = dict();
                if d.is_empty() { Codec::Zstd } else { Codec::ZstdDict(d) }
            }
            other => bail!("unsupported sstable compressor {other}"),
        };
        Ok(CompressionInfo { class, codec, chunk_length, max_compressed_length, data_length, chunks, offsets_at })
    }

    /// Bytes of CompressionInfo.db before the offsets, at most (options hold dictionaries of a
    /// few hundred KiB at most).
    pub const HEAD_MAX: u64 = 4 << 20;
}

impl Codec {
    /// Decompress one chunk into `out` (cleared first); `max` is the chunk length, the most
    /// any chunk may expand to.
    pub fn decompress(&self, data: &[u8], max: usize, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        match self {
            Codec::Noop => out.extend_from_slice(data),
            Codec::Lz4 | Codec::Lz4Dict(_) => {
                if data.len() < 4 {
                    bail!("corrupt chunk: lz4 header");
                }
                let n = u32::from_le_bytes(data[..4].try_into().expect("4")) as usize;
                if n > max {
                    bail!("corrupt chunk: lz4 length {n} over the chunk length {max}");
                }
                out.resize(n, 0);
                let got = match self {
                    Codec::Lz4Dict(d) => lz4_flex::block::decompress_into_with_dict(&data[4..], out, d),
                    _ => lz4_flex::block::decompress_into(&data[4..], out),
                }
                .map_err(|e| anyhow::anyhow!("corrupt chunk: lz4: {e}"))?;
                if got != n {
                    bail!("corrupt chunk: lz4 gave {got} bytes, expected {n}");
                }
            }
            Codec::Snappy => {
                let n = snap::raw::decompress_len(data).map_err(|e| anyhow::anyhow!("corrupt chunk: snappy: {e}"))?;
                if n > max {
                    bail!("corrupt chunk: snappy length {n} over the chunk length {max}");
                }
                out.resize(n, 0);
                let got = snap::raw::Decoder::new().decompress(data, out).map_err(|e| anyhow::anyhow!("corrupt chunk: snappy: {e}"))?;
                out.truncate(got);
            }
            Codec::Deflate => {
                use std::io::Read;
                let mut d = flate2::read::ZlibDecoder::new(data).take(max as u64 + 1);
                d.read_to_end(out).map_err(|e| anyhow::anyhow!("corrupt chunk: deflate: {e}"))?;
                if out.len() > max {
                    bail!("corrupt chunk: deflate output over the chunk length {max}");
                }
            }
            Codec::Zstd | Codec::ZstdDict(_) => {
                let mut d = match self {
                    Codec::ZstdDict(dict) => zstd::bulk::Decompressor::with_dictionary(dict)?,
                    _ => zstd::bulk::Decompressor::new()?,
                };
                out.reserve(max);
                d.decompress_to_buffer(data, out).map_err(|e| anyhow::anyhow!("corrupt chunk: zstd: {e}"))?;
                if out.len() > max {
                    bail!("corrupt chunk: zstd output over the chunk length {max}");
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn info_bytes(class: &str, options: &[(&str, &[u8])], chunk: u32, max: Option<u32>, len: u64, offsets: &[u64]) -> Vec<u8> {
        let mut o = Vec::new();
        o.extend_from_slice(&(class.len() as u16).to_be_bytes());
        o.extend_from_slice(class.as_bytes());
        o.extend_from_slice(&(options.len() as u32).to_be_bytes());
        for (k, v) in options {
            o.extend_from_slice(&(k.len() as u16).to_be_bytes());
            o.extend_from_slice(k.as_bytes());
            o.extend_from_slice(&(v.len() as u16).to_be_bytes());
            o.extend_from_slice(v);
        }
        o.extend_from_slice(&chunk.to_be_bytes());
        if let Some(m) = max {
            o.extend_from_slice(&m.to_be_bytes());
        }
        o.extend_from_slice(&len.to_be_bytes());
        o.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        for x in offsets {
            o.extend_from_slice(&x.to_be_bytes());
        }
        o
    }

    #[test]
    fn parses_cassandra_and_scylla_headers() {
        // Bytes of the reference images' files (nb: LZ4 16 KiB; ScyllaDB mt: LZ4 4 KiB, no
        // max compressed length).
        let nb = info_bytes("LZ4Compressor", &[], 16384, Some(i32::MAX as u32), 44, &[0]);
        let i = CompressionInfo::parse(Bytes::from(nb.clone()), nb.len() as u64, true).unwrap();
        assert_eq!((i.codec.clone(), i.chunk_length, i.data_length, i.chunks), (Codec::Lz4, 16384, 44, 1));
        let mt = info_bytes("LZ4Compressor", &[("crc_check_chance", b"1.0")], 4096, None, 41, &[0]);
        let i = CompressionInfo::parse(Bytes::from(mt.clone()), mt.len() as u64, false).unwrap();
        assert_eq!((i.max_compressed_length, i.chunks), (u32::MAX, 1));
        let d = info_bytes("ZstdWithDictsCompressor", &[(".dictionary.00000001", b"cd"), (".dictionary.00000000", b"ab")], 4096, None, 1, &[0]);
        assert_eq!(CompressionInfo::parse(Bytes::from(d.clone()), d.len() as u64, false).unwrap().codec, Codec::ZstdDict(Bytes::from_static(b"abcd")));
    }

    #[test]
    fn bad_headers_are_errors() {
        let ok = info_bytes("LZ4Compressor", &[], 4096, None, 10_000, &[0, 10, 20]);
        assert!(CompressionInfo::parse(Bytes::from(ok.clone()), ok.len() as u64 - 1, false).is_err(), "offsets past the end");
        let few = info_bytes("LZ4Compressor", &[], 4096, None, 100_000, &[0]);
        assert!(CompressionInfo::parse(Bytes::from(few.clone()), few.len() as u64, false).is_err(), "too few chunks");
        let zero = info_bytes("LZ4Compressor", &[], 0, None, 0, &[]);
        assert!(CompressionInfo::parse(Bytes::from(zero.clone()), zero.len() as u64, false).is_err());
        let other = info_bytes("com.example.Brotli", &[], 4096, None, 1, &[0]);
        assert!(CompressionInfo::parse(Bytes::from(other.clone()), other.len() as u64, false).unwrap_err().to_string().contains("unsupported"));
    }

    #[test]
    fn codecs_decompress_and_bound_output() {
        let raw: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let mut lz = (raw.len() as u32).to_le_bytes().to_vec();
        lz.extend_from_slice(&lz4_flex::block::compress(&raw));
        let snappy = snap::raw::Encoder::new().compress_vec(&raw).unwrap();
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut z, &raw).unwrap();
        let deflate = z.finish().unwrap();
        let zs = zstd::bulk::compress(&raw, 3).unwrap();
        let mut out = Vec::new();
        for (codec, data) in [(Codec::Lz4, &lz), (Codec::Snappy, &snappy), (Codec::Deflate, &deflate), (Codec::Zstd, &zs), (Codec::Noop, &raw)] {
            codec.decompress(data, 32768, &mut out).unwrap();
            assert_eq!(out, raw, "{codec:?}");
            assert!(codec.decompress(data, 1000, &mut out).is_err() || codec == Codec::Noop, "{codec:?} past the chunk length");
            let mut bad = data.clone();
            let n = bad.len();
            bad.truncate(n / 2);
            assert!(codec.decompress(&bad, 32768, &mut out).is_err() || codec == Codec::Noop, "{codec:?} truncated");
        }
        let dict = b"some dictionary content".repeat(10);
        let mut lzd = (raw.len() as u32).to_le_bytes().to_vec();
        lzd.extend_from_slice(&lz4_flex::block::compress_with_dict(&raw, &dict));
        Codec::Lz4Dict(Bytes::from(dict.clone())).decompress(&lzd, 32768, &mut out).unwrap();
        assert_eq!(out, raw);
        let zd = zstd::bulk::Compressor::with_dictionary(3, &dict).unwrap().compress(&raw).unwrap();
        Codec::ZstdDict(Bytes::from(dict)).decompress(&zd, 32768, &mut out).unwrap();
        assert_eq!(out, raw);
    }
}
