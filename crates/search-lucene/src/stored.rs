//! Stored fields (`Lucene90CompressingStoredFieldsReader`): documents in compressed chunks in
//! `.fdt`, read here in order, chunk after chunk, as a restore wants them. `.fdm` gives the
//! chunk size (the fields index in `.fdx` is for random access and isn't read).
//!
//! A chunk is `docBase`, a token (`chunkDocs << 2 | sliced`), each document's field count and
//! length (`StoredFieldsInts`), then the compressed documents, whole or, when `sliced`, in
//! pieces of the chunk size each compressed on their own. Three compressions:
//!
//! - `Lucene90StoredFieldsFastData`: `LZ4WithPresetDictCompressionMode` (a dictionary, then
//!   blocks that can refer back into it);
//! - `Lucene90StoredFieldsHighData`: `DeflateWithPresetDictCompressionMode` (raw DEFLATE, the
//!   blocks inflated with the dictionary as preset history);
//! - `ZstdStoredFields814`: Elasticsearch's `best_compression` since 8.14, as observed in
//!   what Elasticsearch writes: a vInt compressed length, then one zstd frame of the piece.
//!
//! Reading a chunk is two steps, so a caller streaming the file never decompresses twice:
//! [`measure`] walks only the header and the compressed lengths to tell how many bytes the
//! chunk takes (or that more are needed), then [`ChunkHeader::decompress`] inflates it into a
//! buffer of exactly the size the header declared, which the caller caps first.

use crate::codec;
use crate::input::Input;
use crate::{Error, Id, Result, corrupt};

/// How a `.fdt` compresses its chunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Lz4,
    Deflate,
    Zstd,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Lz4 => "Lucene90StoredFieldsFastData",
            Mode::Deflate => "Lucene90StoredFieldsHighData",
            Mode::Zstd => "ZstdStoredFields814",
        }
    }
}

/// The most documents a chunk may hold: every writer flushes a chunk far sooner (Lucene at
/// 1,024 or 4,096 documents), and the per-document arrays are sized from it.
pub const MAX_CHUNK_DOCS: u32 = 1 << 16;

/// `.fdt`'s header: the mode and where the chunks start.
pub fn read_fdt_header(head: &[u8], id: &Id) -> Result<(Mode, usize)> {
    let mode = match codec::header_name(head) {
        Some("Lucene90StoredFieldsFastData") => Mode::Lz4,
        Some("Lucene90StoredFieldsHighData") => Mode::Deflate,
        Some("ZstdStoredFields814") => Mode::Zstd,
        Some(other @ ("Lucene87StoredFieldsFastData" | "Lucene87StoredFieldsHighData" | "Lucene50StoredFieldsFastData" | "Lucene50StoredFieldsHighData")) => {
            return Err(Error::Unsupported(format!("stored fields format [{other}] (Lucene 8) isn't read")));
        }
        Some(other) => return Err(Error::Unsupported(format!("unknown stored fields format [{other}]"))),
        None => return corrupt("the stored fields have no codec header"),
    };
    let mut inp = Input::new(head);
    codec::check_index_header(&mut inp, mode.name(), 1, 1, id, "")?;
    Ok((mode, inp.pos()))
}

/// `.fdm`: the chunk size (footer checked first).
pub fn read_chunk_size(fdm: &[u8], id: &Id) -> Result<u32> {
    codec::check_footer(fdm)?;
    let mut inp = Input::new(codec::body(fdm)?);
    codec::check_index_header(&mut inp, "Lucene90FieldsIndexMeta", 0, 1, id, "")?;
    let chunk_size = inp.vint()?;
    if chunk_size <= 0 {
        return corrupt(format!("invalid chunk size [{chunk_size}]"));
    }
    Ok(chunk_size as u32)
}

/// A chunk's header and where its compressed bytes are.
#[derive(Clone, Debug, PartialEq)]
pub struct ChunkHeader {
    pub doc_base: u32,
    pub sliced: bool,
    /// Each document's stored field count.
    pub field_counts: Vec<u32>,
    /// Each document's offset in the decompressed chunk; one more than there are documents.
    pub offsets: Vec<usize>,
    /// Header bytes, then compressed bytes: the chunk is `header_len + compressed_len` long.
    pub header_len: usize,
    pub compressed_len: usize,
}

impl ChunkHeader {
    pub fn docs(&self) -> u32 {
        self.field_counts.len() as u32
    }

    /// The decompressed size.
    pub fn total(&self) -> usize {
        self.offsets.last().copied().unwrap_or(0)
    }

    pub fn len(&self) -> usize {
        self.header_len + self.compressed_len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Decompresses the chunk (`chunk`: its [`ChunkHeader::len`] bytes) into `out`, which ends
    /// up [`ChunkHeader::total`] bytes long.
    pub fn decompress(&self, chunk: &[u8], mode: Mode, chunk_size: u32, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        let total = self.total();
        let mut inp = Input::new(chunk.get(self.header_len..self.len()).ok_or_else(|| Error::Corrupt("a chunk shorter than its header said".into()))?);
        let piece = if self.sliced { chunk_size as usize } else { total.max(1) };
        while out.len() < total {
            let n = piece.min(total - out.len());
            decompress_piece(&mut inp, mode, n, out)?;
        }
        if inp.remaining() != 0 {
            return corrupt(format!("[{}] bytes left over after a chunk", inp.remaining()));
        }
        Ok(())
    }

    /// Document `i`'s bytes in the decompressed chunk.
    pub fn doc<'a>(&self, decompressed: &'a [u8], i: usize) -> &'a [u8] {
        &decompressed[self.offsets[i]..self.offsets[i + 1]]
    }
}

/// `StoredFieldsInts.readInts`: `count` values of 0 (one vInt for all), 8, 16 or 32 bits,
/// blocks of 128 transposed into longs.
fn read_ints(inp: &mut Input, count: usize, out: &mut Vec<u32>) -> Result<()> {
    let bpv = inp.u8()?;
    out.clear();
    match bpv {
        0 => {
            let v = inp.vint()? as u32;
            out.resize(count, v);
            return Ok(());
        }
        8 | 16 | 32 => {}
        b => return corrupt(format!("Unsupported number of bits per value: {b}")),
    }
    let width = bpv as usize / 8;
    if count.saturating_mul(width) > inp.remaining() {
        return Err(Error::Corrupt(NEED_MORE.into()));
    }
    out.resize(count, 0);
    let per_long = 8 / width;
    let mut k = 0;
    // Whole blocks of 128: each long holds `per_long` values `block/per_long` apart.
    while k + 128 <= count {
        let longs = 128 / per_long;
        for i in 0..longs {
            let l = inp.i64()? as u64;
            for j in 0..per_long {
                let shift = (per_long - 1 - j) * bpv as usize;
                let mask = if bpv == 32 { 0xFFFF_FFFF } else { (1u64 << bpv) - 1 };
                out[k + i + j * longs] = ((l >> shift) & mask) as u32;
            }
        }
        k += 128;
    }
    while k < count {
        out[k] = match bpv {
            8 => inp.u8()? as u32,
            16 => inp.u16()? as u32,
            _ => inp.i32()? as u32,
        };
        k += 1;
    }
    Ok(())
}

/// What [`measure`] answers when `buf` ends before the chunk does.
const NEED_MORE: &str = "\u{0}need more";

fn need_more(e: &Error) -> bool {
    matches!(e, Error::Corrupt(m) if m == NEED_MORE || m.starts_with("read past the end"))
}

/// Reads the header of the chunk `buf` starts with and walks its compressed lengths: `None`
/// when `buf` ends before the chunk does. `max_docs` is the segment's documents left (the
/// chunk must fit them) and `max_total` the most the chunk may decompress to.
pub fn measure(buf: &[u8], mode: Mode, chunk_size: u32, max_docs: u32, max_total: usize) -> Result<Option<ChunkHeader>> {
    match measure_inner(buf, mode, chunk_size, max_docs, max_total) {
        Ok(h) => Ok(Some(h)),
        Err(e) if need_more(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

fn measure_inner(buf: &[u8], mode: Mode, chunk_size: u32, max_docs: u32, max_total: usize) -> Result<ChunkHeader> {
    let mut inp = Input::new(buf);
    let doc_base = inp.vint()?;
    let token = inp.vint()? as u32;
    let docs = token >> 2;
    let sliced = token & 1 != 0;
    if doc_base < 0 || docs == 0 || docs > max_docs || docs > MAX_CHUNK_DOCS {
        return corrupt(format!("Corrupted: docBase={doc_base}, chunkDocs={docs}, numDocs left={max_docs}"));
    }
    let mut field_counts = Vec::new();
    let mut lengths = Vec::new();
    if docs == 1 {
        field_counts.push(inp.vint()? as u32);
        lengths.push(inp.vint()? as u32);
    } else {
        read_ints(&mut inp, docs as usize, &mut field_counts)?;
        read_ints(&mut inp, docs as usize, &mut lengths)?;
    }
    let mut offsets = Vec::with_capacity(docs as usize + 1);
    let mut total = 0usize;
    offsets.push(0);
    for (i, &len) in lengths.iter().enumerate() {
        if (len == 0) != (field_counts[i] == 0) {
            return corrupt(format!("length={len}, numStoredFields={}", field_counts[i]));
        }
        total = total.checked_add(len as usize).filter(|t| *t <= max_total).ok_or_else(|| {
            Error::Unsupported(format!("a stored fields chunk decompresses to more than [{max_total}] bytes"))
        })?;
        offsets.push(total);
    }
    let header_len = inp.pos();
    let piece = if sliced { chunk_size as usize } else { total.max(1) };
    let mut done = 0;
    while done < total {
        let n = piece.min(total - done);
        skip_piece(&mut inp, mode, n)?;
        done += n;
    }
    Ok(ChunkHeader { doc_base: doc_base as u32, sliced, field_counts, offsets, header_len, compressed_len: inp.pos() - header_len })
}

/// Walks the compressed bytes of one piece of `len` decompressed bytes without inflating them.
fn skip_piece(inp: &mut Input, mode: Mode, len: usize) -> Result<()> {
    match mode {
        Mode::Zstd => {
            let n = inp.vuint()? as usize;
            inp.skip(n)
        }
        Mode::Lz4 => {
            let (dict, block) = (inp.vuint()? as usize, inp.vuint()? as usize);
            let dict_compressed = inp.vuint()? as usize;
            let mut compressed = dict_compressed;
            for _ in 0..blocks(len, dict, block)? {
                compressed = compressed.saturating_add(inp.vuint()? as usize);
            }
            inp.skip(compressed)
        }
        Mode::Deflate => {
            let (dict, block) = (inp.vuint()? as usize, inp.vuint()? as usize);
            let n = inp.vuint()? as usize;
            inp.skip(n)?;
            for _ in 0..blocks(len, dict, block)? {
                let n = inp.vuint()? as usize;
                inp.skip(n)?;
            }
            Ok(())
        }
    }
}

/// Blocks after the dictionary in a piece of `len` bytes.
fn blocks(len: usize, dict: usize, block: usize) -> Result<usize> {
    if dict > len {
        return corrupt(format!("dictionary of [{dict}] bytes in a piece of [{len}]"));
    }
    if len == dict {
        return Ok(0);
    }
    if block == 0 {
        return corrupt("a block length of 0");
    }
    Ok((len - dict).div_ceil(block))
}

/// Decompresses one piece of `len` bytes, appending it to `out`.
fn decompress_piece(inp: &mut Input, mode: Mode, len: usize, out: &mut Vec<u8>) -> Result<()> {
    match mode {
        Mode::Zstd => {
            let n = inp.vuint()? as usize;
            let frame = inp.bytes(n)?;
            let start = out.len();
            out.resize(start + len, 0);
            let mut d = zstd::bulk::Decompressor::new().map_err(|e| Error::Corrupt(format!("zstd: {e}")))?;
            let got = d.decompress_to_buffer(frame, &mut out[start..]).map_err(|e| Error::Corrupt(format!("zstd: {e}")))?;
            if got != len {
                return corrupt(format!("a zstd piece decompressed to [{got}] bytes, not [{len}]"));
            }
            Ok(())
        }
        Mode::Lz4 => {
            let (dict, block) = (inp.vuint()? as usize, inp.vuint()? as usize);
            let _dict_compressed = inp.vuint()?;
            let n = blocks(len, dict, block)?;
            let mut lengths = Vec::with_capacity(n.min(inp.remaining()));
            for _ in 0..n {
                lengths.push(inp.vuint()? as usize);
            }
            // The dictionary, then each block decompressed after it, so matches can reach it.
            let mut buf = vec![0u8; dict + block.min(len - dict)];
            crate::lz4::decompress(inp, &mut buf[..dict], 0)?;
            out.extend_from_slice(&buf[..dict]);
            let mut done = dict;
            for (i, compressed) in lengths.into_iter().enumerate() {
                let m = block.min(len - done);
                let start = inp.pos();
                crate::lz4::decompress(inp, &mut buf[..dict + m], dict)?;
                if inp.pos() - start != compressed {
                    return corrupt(format!("LZ4 block [{i}] took [{}] bytes, not the [{compressed}] recorded", inp.pos() - start));
                }
                out.extend_from_slice(&buf[dict..dict + m]);
                done += m;
            }
            Ok(())
        }
        Mode::Deflate => {
            let (dict, block) = (inp.vuint()? as usize, inp.vuint()? as usize);
            let n = blocks(len, dict, block)?;
            let start = out.len();
            out.resize(start + len, 0);
            let piece = &mut out[start..];
            let compressed = inp.vuint()? as usize;
            inflate(inp.bytes(compressed)?, &mut piece[..dict], 0)?;
            let mut done = dict;
            for _ in 0..n {
                let m = block.min(len - done);
                let compressed = inp.vuint()? as usize;
                // Each block is its own stream with the dictionary as history.
                let mut window = Vec::with_capacity(dict + m);
                window.extend_from_slice(&piece[..dict]);
                window.resize(dict + m, 0);
                inflate(inp.bytes(compressed)?, &mut window, dict)?;
                piece[done..done + m].copy_from_slice(&window[dict..]);
                done += m;
            }
            Ok(())
        }
    }
}

/// Inflates raw DEFLATE into exactly `out[start..]`, `out[..start]` being history it may refer
/// to (a preset dictionary).
fn inflate(input: &[u8], out: &mut [u8], start: usize) -> Result<()> {
    use miniz_oxide::inflate::TINFLStatus;
    use miniz_oxide::inflate::core::{DecompressorOxide, decompress, inflate_flags};
    if out.len() == start {
        return Ok(());
    }
    let mut d = Box::new(DecompressorOxide::new());
    let (status, _, written) = decompress(&mut d, input, out, start, inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF);
    match status {
        TINFLStatus::Done if written == out.len() - start => Ok(()),
        TINFLStatus::Done => corrupt(format!("a DEFLATE block inflated to [{written}] bytes, not [{}]", out.len() - start)),
        TINFLStatus::HasMoreOutput => corrupt("a DEFLATE block inflates to more than its length"),
        other => corrupt(format!("DEFLATE: {other:?}")),
    }
}

/// One stored value (`Lucene90CompressingStoredFieldsWriter`'s types).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stored<'a> {
    Str(&'a str),
    Bytes(&'a [u8]),
    Int(i32),
    Float(f32),
    Long(i64),
    Double(f64),
}

/// A document's stored values with their field numbers, in order.
pub fn read_doc(bytes: &[u8], field_count: u32) -> Result<Vec<(u32, Stored<'_>)>> {
    let mut inp = Input::new(bytes);
    // A value takes at least two bytes.
    if field_count as usize > bytes.len() {
        return corrupt(format!("[{field_count}] stored fields in [{}] bytes", bytes.len()));
    }
    let mut out = Vec::with_capacity(field_count as usize);
    for _ in 0..field_count {
        let info = inp.vlong()? as u64;
        let number = (info >> 3) as u32;
        let value = match info & 7 {
            0 => Stored::Str(inp.string()?),
            1 => {
                let n = inp.vuint()? as usize;
                Stored::Bytes(inp.bytes(n)?)
            }
            2 => Stored::Int(inp.zint()?),
            3 => Stored::Float(read_zfloat(&mut inp)?),
            4 => Stored::Long(read_tlong(&mut inp)?),
            5 => Stored::Double(read_zdouble(&mut inp)?),
            t => return corrupt(format!("Unknown type flag: {t:x}")),
        };
        out.push((number, value));
    }
    if inp.remaining() != 0 {
        return corrupt(format!("[{}] bytes left after a document's stored fields", inp.remaining()));
    }
    Ok(out)
}

fn read_zfloat(inp: &mut Input) -> Result<f32> {
    let b = inp.u8()? as u32;
    Ok(if b == 0xFF {
        f32::from_bits(inp.i32()? as u32)
    } else if b & 0x80 != 0 {
        (b & 0x7F) as f32 - 1.0
    } else {
        let s = inp.u16()? as u32;
        let l = inp.u8()? as u32;
        f32::from_bits(b << 24 | s << 8 | l)
    })
}

fn read_zdouble(inp: &mut Input) -> Result<f64> {
    let b = inp.u8()? as u64;
    Ok(if b == 0xFF {
        f64::from_bits(inp.i64()? as u64)
    } else if b == 0xFE {
        f32::from_bits(inp.i32()? as u32) as f64
    } else if b & 0x80 != 0 {
        (b & 0x7F) as f64 - 1.0
    } else {
        let i = inp.i32()? as u32 as u64;
        let s = inp.u16()? as u64;
        let l = inp.u8()? as u64;
        f64::from_bits(b << 56 | i << 24 | s << 8 | l)
    })
}

fn read_tlong(inp: &mut Input) -> Result<i64> {
    let header = inp.u8()? as u64;
    let mut bits = header & 0x1F;
    if header & 0x20 != 0 {
        bits |= (inp.vlong()? as u64) << 5;
    }
    let l = ((bits >> 1) as i64) ^ -((bits & 1) as i64);
    Ok(match header & 0xC0 {
        0x40 => l.wrapping_mul(1000),
        0x80 => l.wrapping_mul(3_600_000),
        0xC0 => l.wrapping_mul(86_400_000),
        _ => l,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ints_transposed_in_blocks() {
        // 130 bytes of 8-bit values: the first 128 as 16 longs, value k + i + j*16 in byte j.
        let mut buf = vec![8u8];
        for i in 0..16u64 {
            let mut l = 0u64;
            for j in 0..8u64 {
                l |= (i + j * 16) << ((7 - j) * 8);
            }
            buf.extend_from_slice(&l.to_le_bytes());
        }
        buf.extend_from_slice(&[128, 129]);
        let mut out = Vec::new();
        read_ints(&mut Input::new(&buf), 130, &mut out).unwrap();
        assert_eq!(out, (0..130).collect::<Vec<u32>>());
    }

    #[test]
    fn a_huge_declared_chunk_needs_bytes_not_memory() {
        // 65,536 documents of 0x7fffffff bytes each, declared in a dozen bytes.
        let buf = [0, 0x80, 0x80, 0x10, 0, 1, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0x07];
        let e = measure(&buf, Mode::Lz4, 1 << 14, 1 << 20, 64 << 20).unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
        // More documents than the segment has left.
        assert!(measure(&[0, 0x10, 1, 1], Mode::Lz4, 1 << 14, 2, 1 << 20).is_err());
        // A chunk that's only started needs more bytes.
        assert_eq!(measure(&[0, 0x08, 8], Mode::Lz4, 1 << 14, 10, 1 << 20).unwrap(), None);
    }
}
