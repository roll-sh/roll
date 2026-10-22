//! Stored fields: the original values (`_source`, `_id`, stored fields) of every document.
//!
//! # Document encoding
//!
//! A document is a sequence of `(field << 3 | type)` varints followed by the value: UTF-8 string
//! or bytes (length-prefixed), int/long (zigzag varint), float/double (little-endian). The
//! indexing buffer serializes documents in this form once ([`StoredDocWriter`]), so flushing and
//! merging copy bytes instead of re-encoding values.
//!
//! # Chunks
//!
//! Documents are grouped into chunks of about 16 KiB (at most 128 docs) compressed with zstd. A
//! dictionary trained on a sample of the segment's documents is stored once (`StoredDict`) and
//! used by every chunk, which is what makes small chunks compress almost as well as large ones —
//! small chunks keep random access (the fetch phase) cheap. A fixed-width index of
//! `(first doc, offset)` per chunk, after the chunks, is binary searched to find a doc's chunk:
//!
//! ```text
//! chunk*  (doc count varint, raw length varint, compressed length varint, zstd frame)
//! index   (first doc u32, chunk offset u64) per chunk
//! trailer num_docs u32 | chunk count u32 | index offset u64
//! ```
//!
//! Chunks are streamed into the segment as they fill (only the index stays in memory), so
//! writing the stored fields of a large merge needs no more memory than a chunk.
//!
//! # Reading
//!
//! [`StoredReader`] is a per-thread cursor: it keeps the last decompressed chunk, so fetching
//! hits that are close in doc order (the common case after sorting by doc id) decompresses each
//! chunk once.

use std::sync::Arc;

use crate::error::{Result, corrupt};
use crate::io::{ByteSink, In, Out};
use crate::slice::Slice;

/// A stored value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StoredValue<'a> {
    Str(&'a str),
    Bytes(&'a [u8]),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
}

/// An owned stored value.
#[derive(Clone, Debug, PartialEq)]
pub enum OwnedStoredValue {
    Str(String),
    Bytes(Vec<u8>),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
}

impl StoredValue<'_> {
    pub fn to_owned(&self) -> OwnedStoredValue {
        match *self {
            StoredValue::Str(s) => OwnedStoredValue::Str(s.to_string()),
            StoredValue::Bytes(b) => OwnedStoredValue::Bytes(b.to_vec()),
            StoredValue::Int(v) => OwnedStoredValue::Int(v),
            StoredValue::Long(v) => OwnedStoredValue::Long(v),
            StoredValue::Float(v) => OwnedStoredValue::Float(v),
            StoredValue::Double(v) => OwnedStoredValue::Double(v),
        }
    }
}

/// What a visitor wants next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisitStatus {
    /// Decode and deliver this field.
    Yes,
    /// Skip this field.
    No,
    /// Stop visiting this document.
    Stop,
}

/// Receives a document's stored fields.
pub trait StoredVisitor {
    fn needs_field(&mut self, _field: u32) -> VisitStatus {
        VisitStatus::Yes
    }
    fn value(&mut self, field: u32, value: StoredValue<'_>);
}

/// Collects every field.
impl StoredVisitor for Vec<(u32, OwnedStoredValue)> {
    fn value(&mut self, field: u32, value: StoredValue<'_>) {
        self.push((field, value.to_owned()));
    }
}

const TY_STR: u64 = 0;
const TY_BYTES: u64 = 1;
const TY_INT: u64 = 2;
const TY_LONG: u64 = 3;
const TY_FLOAT: u64 = 4;
const TY_DOUBLE: u64 = 5;

/// Serializes a document's stored fields.
#[derive(Clone, Debug, Default)]
pub struct StoredDocWriter {
    buf: Vec<u8>,
}

impl StoredDocWriter {
    pub fn new() -> StoredDocWriter {
        StoredDocWriter::default()
    }

    pub fn add(&mut self, field: u32, value: StoredValue<'_>) {
        encode_field(&mut self.buf, field, value);
    }

    /// The serialized document.
    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// Appends one field to a serialized document.
pub fn encode_field(out: &mut Vec<u8>, field: u32, value: StoredValue<'_>) {
    let key = |t: u64| ((field as u64) << 3) | t;
    match value {
        StoredValue::Str(s) => {
            out.put_vu64(key(TY_STR));
            out.put_lbytes(s.as_bytes());
        }
        StoredValue::Bytes(b) => {
            out.put_vu64(key(TY_BYTES));
            out.put_lbytes(b);
        }
        StoredValue::Int(v) => {
            out.put_vu64(key(TY_INT));
            out.put_vi64(v as i64);
        }
        StoredValue::Long(v) => {
            out.put_vu64(key(TY_LONG));
            out.put_vi64(v);
        }
        StoredValue::Float(v) => {
            out.put_vu64(key(TY_FLOAT));
            out.put_f32(v);
        }
        StoredValue::Double(v) => {
            out.put_vu64(key(TY_DOUBLE));
            out.put_f64(v);
        }
    }
}

/// Decodes a serialized document into a visitor.
pub fn decode_doc(bytes: &[u8], visitor: &mut dyn StoredVisitor) -> Result<()> {
    let mut r = In::new(bytes);
    while !r.is_empty() {
        let key = r.vu64()?;
        let field = u32::try_from(key >> 3).map_err(|_| corrupt("stored field number"))?;
        let ty = key & 7;
        let status = visitor.needs_field(field);
        if status == VisitStatus::Stop {
            return Ok(());
        }
        let deliver = status == VisitStatus::Yes;
        match ty {
            TY_STR => {
                let b = r.lbytes()?;
                if deliver {
                    let s = std::str::from_utf8(b)
                        .map_err(|_| corrupt("stored string is not utf-8"))?;
                    visitor.value(field, StoredValue::Str(s));
                }
            }
            TY_BYTES => {
                let b = r.lbytes()?;
                if deliver {
                    visitor.value(field, StoredValue::Bytes(b));
                }
            }
            TY_INT => {
                let v = r.vi64()?;
                if deliver {
                    visitor.value(field, StoredValue::Int(v as i32));
                }
            }
            TY_LONG => {
                let v = r.vi64()?;
                if deliver {
                    visitor.value(field, StoredValue::Long(v));
                }
            }
            TY_FLOAT => {
                let v = r.f32()?;
                if deliver {
                    visitor.value(field, StoredValue::Float(v));
                }
            }
            TY_DOUBLE => {
                let v = r.f64()?;
                if deliver {
                    visitor.value(field, StoredValue::Double(v));
                }
            }
            t => return Err(corrupt(format!("bad stored value type {t}"))),
        }
    }
    Ok(())
}

/// Stored fields compression options.
#[derive(Clone, Debug)]
pub struct StoredOptions {
    /// zstd level (3 is zstd's default; 1 for indexing speed, up to 19 for `best_compression`).
    pub level: i32,
    /// Target uncompressed bytes per chunk.
    pub chunk_bytes: usize,
    /// Maximum docs per chunk.
    pub chunk_docs: usize,
    /// Train and use a shared zstd dictionary when the segment has enough data.
    pub dictionary: bool,
}

impl Default for StoredOptions {
    fn default() -> StoredOptions {
        StoredOptions {
            level: 3,
            chunk_bytes: 16 * 1024,
            chunk_docs: 128,
            dictionary: true,
        }
    }
}

/// Dictionary size and the sampled bytes below which no dictionary is trained.
const DICT_SIZE: usize = 16 * 1024;
const DICT_MIN_SAMPLE_BYTES: usize = 64 * 1024;
const DICT_MAX_SAMPLE_BYTES: usize = 1 << 20;

/// Trains a dictionary from a sample of documents (`None` if too little data).
pub(crate) fn train_dictionary(samples: &[Vec<u8>]) -> Option<Vec<u8>> {
    let total: usize = samples.iter().map(|s| s.len()).sum();
    if total < DICT_MIN_SAMPLE_BYTES || samples.len() < 16 {
        return None;
    }
    zstd::dict::from_samples(samples, DICT_SIZE).ok()
}

/// Sample doc ids for dictionary training: evenly spaced, capped by count.
pub(crate) fn sample_docs(max_doc: u32) -> impl Iterator<Item = u32> {
    let n = max_doc.min(2048);
    let step = max_doc.checked_div(n).unwrap_or(1).max(1);
    (0..n).map(move |i| i * step)
}

pub(crate) fn max_sample_bytes() -> usize {
    DICT_MAX_SAMPLE_BYTES
}

/// Accumulates documents into compressed chunks.
pub(crate) struct StoredWriter<'d> {
    opts: StoredOptions,
    compressor: zstd::bulk::Compressor<'d>,
    /// Bytes handed to the sink so far.
    written: u64,
    header: Vec<u8>,
    raw: Vec<u8>,
    index: Vec<(u32, u64)>,
    chunk: Vec<u8>,
    lens: Vec<u32>,
    chunk_first: u32,
    next_doc: u32,
}

impl<'d> StoredWriter<'d> {
    pub(crate) fn new(
        opts: StoredOptions,
        dict: Option<&'d zstd::dict::EncoderDictionary<'static>>,
    ) -> Result<Self> {
        let compressor = match dict {
            Some(d) => zstd::bulk::Compressor::with_prepared_dictionary(d)?,
            None => zstd::bulk::Compressor::new(opts.level)?,
        };
        Ok(StoredWriter {
            opts,
            compressor,
            written: 0,
            header: Vec::new(),
            raw: Vec::new(),
            index: Vec::new(),
            chunk: Vec::new(),
            lens: Vec::new(),
            chunk_first: 0,
            next_doc: 0,
        })
    }

    /// Adds the next document (docs arrive in order 0, 1, 2, ...); full chunks go to `sink`.
    pub(crate) fn add(&mut self, doc_bytes: &[u8], sink: &mut dyn ByteSink) -> Result<()> {
        if self.lens.is_empty() {
            self.chunk_first = self.next_doc;
        }
        self.chunk.extend_from_slice(doc_bytes);
        self.lens.push(doc_bytes.len() as u32);
        self.next_doc += 1;
        if self.chunk.len() >= self.opts.chunk_bytes || self.lens.len() >= self.opts.chunk_docs {
            self.flush(sink)?;
        }
        Ok(())
    }

    fn flush(&mut self, sink: &mut dyn ByteSink) -> Result<()> {
        if self.lens.is_empty() {
            return Ok(());
        }
        self.index.push((self.chunk_first, self.written));
        self.raw.clear();
        for l in &self.lens {
            self.raw.put_vu32(*l);
        }
        self.raw.extend_from_slice(&self.chunk);
        let compressed = self.compressor.compress(&self.raw)?;
        self.header.clear();
        self.header.put_vu32(self.lens.len() as u32);
        self.header.put_vu32(self.raw.len() as u32);
        self.header.put_vu32(compressed.len() as u32);
        sink.put_all(&self.header)?;
        sink.put_all(&compressed)?;
        self.written += (self.header.len() + compressed.len()) as u64;
        self.chunk.clear();
        self.lens.clear();
        Ok(())
    }

    /// Flushes the last chunk and writes the index and trailer.
    pub(crate) fn finish(mut self, sink: &mut dyn ByteSink) -> Result<()> {
        self.flush(sink)?;
        let mut s = Vec::with_capacity(self.index.len() * 12 + 16);
        for (d, o) in &self.index {
            s.put_u32(*d);
            s.put_u64(*o);
        }
        s.put_u32(self.next_doc);
        s.put_u32(self.index.len() as u32);
        s.put_u64(self.written);
        sink.put_all(&s)
    }
}

/// A prepared decompression dictionary shared by all readers of a segment.
struct SharedDict(zstd::dict::DecoderDictionary<'static>);

// SAFETY: a zstd DDict is immutable after creation and documented as safe to use from several
// decompression contexts concurrently; we only ever pass it by shared reference.
unsafe impl Sync for SharedDict {}

/// The stored fields of a segment; cheap to clone, create a [`StoredReader`] per thread.
#[derive(Clone)]
pub struct StoredFields {
    num_docs: u32,
    index: Slice,
    chunks: Slice,
    nchunks: u32,
    dict: Option<Arc<SharedDict>>,
}

impl std::fmt::Debug for StoredFields {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredFields")
            .field("num_docs", &self.num_docs)
            .field("chunks", &self.nchunks)
            .finish()
    }
}

/// Range, relative to the start of a `section_len`-byte stored-fields section, that opening
/// the section reads (the chunk index and the 16-byte trailer), from the section's last 16
/// bytes. The chunks before it are only read to load documents, so a segment that is still
/// downloading can open once this range (and not the documents) is in.
pub fn open_range(section_len: u64, trailer: &[u8]) -> Result<std::ops::Range<u64>> {
    if section_len < 16 || trailer.len() != 16 {
        return Err(corrupt("stored fields truncated"));
    }
    let nchunks = u32::from_le_bytes(trailer[4..8].try_into().unwrap()) as u64;
    let index_at = u64::from_le_bytes(trailer[8..16].try_into().unwrap());
    if index_at.checked_add(nchunks * 12 + 16) != Some(section_len) {
        return Err(corrupt("stored index truncated"));
    }
    Ok(index_at..section_len)
}

impl StoredFields {
    pub(crate) fn open(data: Slice, dict: Option<Slice>) -> Result<StoredFields> {
        if data.len() < 16 {
            return Err(corrupt("stored fields truncated"));
        }
        let mut r = In::new(&data[data.len() - 16..]);
        let num_docs = r.u32()?;
        let nchunks = r.u32()?;
        let index_at = r.u64()?;
        if index_at
            .checked_add(nchunks as u64 * 12 + 16)
            .is_none_or(|end| end != data.len() as u64)
        {
            return Err(corrupt("stored index truncated"));
        }
        let index = data
            .try_slice(index_at, nchunks as u64 * 12)
            .ok_or_else(|| corrupt("stored index"))?;
        let chunks = data.slice(0..index_at as usize);
        let mut prev = None;
        for i in 0..nchunks as usize {
            let d = u32::from_le_bytes(index[i * 12..i * 12 + 4].try_into().unwrap());
            let o = u64::from_le_bytes(index[i * 12 + 4..i * 12 + 12].try_into().unwrap());
            if prev.is_some_and(|p| p >= d) || d >= num_docs || o >= chunks.len() as u64 {
                return Err(corrupt("bad stored chunk index"));
            }
            prev = Some(d);
        }
        let dict = match dict {
            Some(d) if !d.is_empty() => Some(Arc::new(SharedDict(
                zstd::dict::DecoderDictionary::try_copy(&d)
                    .map_err(|e| corrupt(format!("stored dictionary: {e}")))?,
            ))),
            _ => None,
        };
        Ok(StoredFields {
            num_docs,
            index,
            chunks,
            nchunks,
            dict,
        })
    }

    pub fn num_docs(&self) -> u32 {
        self.num_docs
    }

    /// A reader with its own decompression context and chunk cache.
    pub fn reader(&self) -> StoredReader {
        StoredReader {
            fields: self.clone(),
            dctx: zstd::zstd_safe::DCtx::create(),
            chunk: u32::MAX,
            buf: Vec::new(),
            starts: Vec::new(),
        }
    }

    fn chunk_of(&self, doc: u32) -> u32 {
        let (mut lo, mut hi) = (0u32, self.nchunks);
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if self.chunk_first(mid) <= doc {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }

    fn chunk_first(&self, c: u32) -> u32 {
        let at = c as usize * 12;
        u32::from_le_bytes(self.index[at..at + 4].try_into().unwrap())
    }

    fn chunk_offset(&self, c: u32) -> usize {
        let at = c as usize * 12 + 4;
        u64::from_le_bytes(self.index[at..at + 8].try_into().unwrap()) as usize
    }
}

/// Random access to documents, caching the last decompressed chunk.
pub struct StoredReader {
    fields: StoredFields,
    dctx: zstd::zstd_safe::DCtx<'static>,
    chunk: u32,
    buf: Vec<u8>,
    /// Start of each doc of the cached chunk within `buf`, plus the end.
    starts: Vec<usize>,
}

impl StoredReader {
    fn load(&mut self, c: u32) -> Result<()> {
        if self.chunk == c {
            return Ok(());
        }
        self.chunk = u32::MAX;
        let f = &self.fields;
        let mut r = In::at(&f.chunks, f.chunk_offset(c));
        let ndocs = r.vu32()? as usize;
        let raw_len = r.vlen(1 << 31)?;
        let comp = r.lbytes()?;
        // The compressed bytes bound how much a chunk can expand (zstd's maximum ratio is far
        // below 2^15), so a corrupt length cannot trigger a huge allocation.
        if raw_len > comp.len().saturating_mul(1 << 15).max(1 << 16) {
            return Err(corrupt("stored chunk length implausible"));
        }
        self.buf.clear();
        self.buf.reserve(raw_len);
        let n = match &f.dict {
            Some(d) => self
                .dctx
                .decompress_using_ddict(&mut self.buf, comp, d.0.as_ddict()),
            None => self.dctx.decompress(&mut self.buf, comp),
        }
        .map_err(|code| {
            corrupt(format!(
                "stored chunk decompression failed ({})",
                zstd::zstd_safe::get_error_name(code)
            ))
        })?;
        if n != raw_len {
            return Err(corrupt("stored chunk length mismatch"));
        }
        let expected_docs = if c + 1 < f.nchunks {
            f.chunk_first(c + 1)
        } else {
            f.num_docs
        } - f.chunk_first(c);
        if ndocs != expected_docs as usize {
            return Err(corrupt("stored chunk doc count mismatch"));
        }
        // Doc lengths precede the docs: read them into `starts`, then turn them into offsets.
        let mut lr = In::new(&self.buf);
        self.starts.clear();
        for _ in 0..ndocs {
            self.starts.push(lr.vu32()? as usize);
        }
        let mut at = lr.pos();
        for s in self.starts.iter_mut() {
            let len = *s;
            *s = at;
            at = at.saturating_add(len);
        }
        if at != self.buf.len() {
            return Err(corrupt("stored chunk lengths mismatch"));
        }
        self.starts.push(at);
        self.chunk = c;
        Ok(())
    }

    fn doc_range(&mut self, doc: u32) -> Result<(usize, usize)> {
        if doc >= self.fields.num_docs {
            return Err(crate::error::invalid(format!("doc {doc} out of range")));
        }
        let c = self.fields.chunk_of(doc);
        self.load(c)?;
        let i = (doc - self.fields.chunk_first(c)) as usize;
        Ok((self.starts[i], self.starts[i + 1]))
    }

    /// The serialized bytes of `doc` (see [`decode_doc`]).
    pub fn raw(&mut self, doc: u32) -> Result<&[u8]> {
        let (s, e) = self.doc_range(doc)?;
        Ok(&self.buf[s..e])
    }

    /// Delivers `doc`'s fields to `visitor`.
    pub fn visit(&mut self, doc: u32, visitor: &mut dyn StoredVisitor) -> Result<()> {
        let (s, e) = self.doc_range(doc)?;
        decode_doc(&self.buf[s..e], visitor)
    }

    /// All fields of `doc`.
    pub fn document(&mut self, doc: u32) -> Result<Vec<(u32, OwnedStoredValue)>> {
        let mut v = Vec::new();
        self.visit(doc, &mut v)?;
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    fn doc(rng: &mut StdRng, i: u32) -> Vec<u8> {
        let mut w = StoredDocWriter::new();
        w.add(0, StoredValue::Str(&format!("id-{i}")));
        let src = format!(
            r#"{{"title":"document number {i}","body":"the quick brown fox jumps over the lazy dog {}","n":{}}}"#,
            rng.r#gen::<u32>(),
            i
        );
        w.add(1, StoredValue::Bytes(src.as_bytes()));
        w.add(2, StoredValue::Int(-(i as i32)));
        w.add(3, StoredValue::Long(i as i64 * 1_000_000_007));
        w.add(4, StoredValue::Float(i as f32 * 0.5));
        w.add(5, StoredValue::Double(-(i as f64) / 3.0));
        w.into_bytes()
    }

    fn build(n: u32, opts: StoredOptions) -> (StoredFields, Vec<Vec<u8>>) {
        let mut rng = StdRng::seed_from_u64(n as u64);
        let docs: Vec<Vec<u8>> = (0..n).map(|i| doc(&mut rng, i)).collect();
        let samples: Vec<Vec<u8>> = sample_docs(n).map(|d| docs[d as usize].clone()).collect();
        let dict_bytes = if opts.dictionary {
            train_dictionary(&samples)
        } else {
            None
        };
        let enc = dict_bytes
            .as_ref()
            .map(|d| zstd::dict::EncoderDictionary::copy(d, opts.level));
        let mut w = StoredWriter::new(opts, enc.as_ref()).unwrap();
        let mut bytes = Vec::new();
        for d in &docs {
            w.add(d, &mut bytes).unwrap();
        }
        w.finish(&mut bytes).unwrap();
        let f = StoredFields::open(Slice::from(bytes), dict_bytes.map(Slice::from)).unwrap();
        (f, docs)
    }

    #[test]
    fn open_range_covers_what_open_reads() {
        for n in [0u32, 1, 300, 3000] {
            let mut rng = StdRng::seed_from_u64(n as u64);
            let mut w = StoredWriter::new(StoredOptions::default(), None).unwrap();
            let mut bytes = Vec::new();
            for i in 0..n {
                w.add(&doc(&mut rng, i), &mut bytes).unwrap();
            }
            w.finish(&mut bytes).unwrap();
            let len = bytes.len() as u64;
            let r = open_range(len, &bytes[bytes.len() - 16..]).unwrap();
            assert_eq!(r.end, len);
            // Zeroing everything before the range leaves the section openable.
            let mut z = bytes.clone();
            z[..r.start as usize].fill(0);
            let f = StoredFields::open(Slice::from(z), None).unwrap();
            assert_eq!(f.num_docs(), n);
            assert!(open_range(len - 1, &bytes[bytes.len() - 16..]).is_err());
            assert!(open_range(len, &bytes[..15]).is_err());
        }
    }

    #[test]
    fn round_trips_with_and_without_dictionary() {
        for dictionary in [false, true] {
            for n in [0u32, 1, 127, 128, 129, 3000] {
                let (f, docs) = build(
                    n,
                    StoredOptions {
                        dictionary,
                        ..Default::default()
                    },
                );
                assert_eq!(f.num_docs(), n);
                let mut r = f.reader();
                let mut rng = StdRng::seed_from_u64(1);
                for _ in 0..200.min(n * 2) {
                    let d = rng.gen_range(0..n);
                    assert_eq!(r.raw(d).unwrap(), &docs[d as usize][..]);
                }
                for d in 0..n {
                    let fields = r.document(d).unwrap();
                    assert_eq!(fields.len(), 6);
                    assert_eq!(fields[0], (0, OwnedStoredValue::Str(format!("id-{d}"))));
                    assert_eq!(fields[2], (2, OwnedStoredValue::Int(-(d as i32))));
                }
                assert!(r.raw(n).is_err());
            }
        }
    }

    #[test]
    fn dictionary_improves_compression() {
        // Small chunks (fast random access) are where a shared dictionary pays.
        let (plain, _) = build(
            5000,
            StoredOptions {
                dictionary: false,
                chunk_docs: 8,
                ..Default::default()
            },
        );
        let (dict, _) = build(
            5000,
            StoredOptions {
                dictionary: true,
                chunk_docs: 8,
                ..Default::default()
            },
        );
        assert!(dict.dict.is_some());
        assert!(
            dict.chunks.len() < plain.chunks.len(),
            "{} vs {}",
            dict.chunks.len(),
            plain.chunks.len()
        );
    }

    #[test]
    fn visitor_controls() {
        struct OnlyTwo(Vec<u32>);
        impl StoredVisitor for OnlyTwo {
            fn needs_field(&mut self, f: u32) -> VisitStatus {
                match f {
                    2 => VisitStatus::Yes,
                    4 => VisitStatus::Stop,
                    _ => VisitStatus::No,
                }
            }
            fn value(&mut self, f: u32, _: StoredValue<'_>) {
                self.0.push(f);
            }
        }
        let (f, _) = build(10, StoredOptions::default());
        let mut v = OnlyTwo(Vec::new());
        f.reader().visit(3, &mut v).unwrap();
        assert_eq!(v.0, [2]);
    }

    /// `cargo test --release -p search-codec --lib bench_stored -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_stored() {
        let mut rng = StdRng::seed_from_u64(9);
        let words = ["alpha", "beta", "gamma", "delta", "search", "engine", "rust", "lucene", "shard", "segment"];
        let n = 100_000u32;
        let docs: Vec<Vec<u8>> = (0..n)
            .map(|i| {
                let mut w = StoredDocWriter::new();
                w.add(0, StoredValue::Str(&format!("id-{i}")));
                let body: Vec<&str> = (0..rng.gen_range(50..250)).map(|_| words[rng.gen_range(0..words.len())]).collect();
                let src = format!(r#"{{"title":"doc {i}","price":{},"tags":["a","b"],"body":"{}"}}"#, rng.gen_range(0..1000), body.join(" "));
                w.add(1, StoredValue::Bytes(src.as_bytes()));
                w.into_bytes()
            })
            .collect();
        let raw: usize = docs.iter().map(|d| d.len()).sum();
        for (name, opts) in [
            ("zstd3+dict 16K", StoredOptions::default()),
            ("zstd1+dict 16K", StoredOptions { level: 1, ..Default::default() }),
            ("zstd3 no dict 16K", StoredOptions { dictionary: false, ..Default::default() }),
        ] {
            let samples: Vec<Vec<u8>> = sample_docs(n).map(|d| docs[d as usize].clone()).collect();
            let dict = if opts.dictionary { train_dictionary(&samples) } else { None };
            let enc = dict.as_ref().map(|d| zstd::dict::EncoderDictionary::copy(d, opts.level));
            let t = std::time::Instant::now();
            let mut w = StoredWriter::new(opts.clone(), enc.as_ref()).unwrap();
            let mut bytes = Vec::new();
            for d in &docs {
                w.add(d, &mut bytes).unwrap();
            }
            w.finish(&mut bytes).unwrap();
            let write = t.elapsed();
            let f = StoredFields::open(Slice::from(bytes.clone()), dict.map(Slice::from)).unwrap();
            let mut r = f.reader();
            let t = std::time::Instant::now();
            let mut sum = 0usize;
            let k = 20_000;
            for _ in 0..k {
                sum += r.raw(rng.gen_range(0..n)).unwrap().len();
            }
            let random = t.elapsed() / k;
            let t = std::time::Instant::now();
            for d in 0..n {
                sum += r.raw(d).unwrap().len();
            }
            let seq = t.elapsed() / n;
            println!(
                "{name}: ratio {:.2}, write {:.0} MB/s, random doc {random:?}, sequential doc {seq:?} ({sum})",
                raw as f64 / bytes.len() as f64,
                raw as f64 / write.as_secs_f64() / 1e6
            );
        }
    }

    #[test]
    fn corrupt_chunk_is_error() {
        let mut rng = StdRng::seed_from_u64(3);
        let mut w = StoredWriter::new(StoredOptions::default(), None).unwrap();
        let mut bytes = Vec::new();
        for i in 0..10 {
            w.add(&doc(&mut rng, i), &mut bytes).unwrap();
        }
        w.finish(&mut bytes).unwrap();
        let good = bytes.clone();
        // Inside the compressed frame.
        bytes[20] ^= 0xFF;
        let f = StoredFields::open(Slice::from(bytes), None).unwrap();
        assert!(f.reader().raw(0).is_err());
        // A trailer pointing anywhere but at the index is caught at open.
        for at in [good.len() - 1, good.len() - 8, good.len() - 12] {
            let mut bad = good.clone();
            bad[at] ^= 0x41;
            assert!(StoredFields::open(Slice::from(bad), None).is_err(), "byte {at}");
        }
        assert!(StoredFields::open(Slice::from(good[..10].to_vec()), None).is_err());
    }
}
