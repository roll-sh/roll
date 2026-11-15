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
//! Documents are grouped into chunks of about 2 KiB (at most 128 docs; a larger document is a
//! chunk of its own) compressed with zstd. Reading a document decompresses its whole chunk, so
//! the chunk size is the cost of a random fetch (zstd, unlike LZ4, cannot stop at the document's
//! end inside a block; independent small frames are what bound the work). A dictionary built
//! from a sample of the segment's documents is stored once (`StoredDict`) and used by every
//! chunk, which is what makes small chunks compress almost as well as large ones: merges train
//! it, refreshes finalize sampled documents into one ([`StoredOptions::train`]). Segments
//! written with other chunk sizes (16 KiB before round 6) read the same way. A fixed-width
//! index of `(first doc, offset)` per chunk, after the chunks, is binary searched to find a
//! doc's chunk:
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
//! chunk once. Readers are cheap to create: the zstd decompression context (about 100 KiB of
//! tables and buffers) is borrowed from a small process-wide pool for the duration of one chunk
//! decompression, so a fetch phase that opens a reader per hit allocates and faults in no
//! context memory per hit.

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
    /// Use a shared zstd dictionary built from sampled documents when the segment has enough
    /// data.
    pub dictionary: bool,
    /// Train the dictionary (zstd's cover algorithm: tens of ms per segment, the best ratio;
    /// merges) instead of finalizing sampled documents into one (a few ms, within 1-4% of the
    /// trained ratio at 2 KiB chunks; refreshes).
    pub train: bool,
}

impl Default for StoredOptions {
    fn default() -> StoredOptions {
        StoredOptions {
            level: 3,
            chunk_bytes: CHUNK_BYTES,
            chunk_docs: 128,
            dictionary: true,
            train: true,
        }
    }
}

impl StoredOptions {
    /// `index.codec: best_compression`: zstd level 6 in 16 KiB chunks. On 200k geonames /
    /// http_logs documents 59.3 / 16.7 B per document against the default's 78.7 / 23.4 (-25%,
    /// -29%), at 20 µs per random document instead of 3-4 µs and a third of the write speed.
    pub fn best_compression() -> StoredOptions {
        StoredOptions { level: 6, chunk_bytes: 16 * 1024, ..StoredOptions::default() }
    }
}

/// Default uncompressed bytes per chunk. Reading a document decompresses its whole chunk, so a
/// chunk is the unit of random-access cost; the dictionary keeps small chunks compact. Measured
/// on 200k geonames / http_logs documents with a 16 KiB dictionary: 16 KiB chunks 5.30x / 7.95x
/// at 14.8 / 12.5 µs per random document, 2 KiB chunks 4.49x / 6.73x at 3.0 / 2.2 µs.
pub const CHUNK_BYTES: usize = 2 * 1024;

/// Dictionary size and the sampled bytes below which no dictionary is built.
const DICT_SIZE: usize = 16 * 1024;
const DICT_MIN_SAMPLE_BYTES: usize = 64 * 1024;
const DICT_MAX_SAMPLE_BYTES: usize = 1 << 20;
/// A sampled dictionary's statistics read at most about this many sampled bytes.
const SAMPLED_DICT_STATS_BYTES: usize = 256 * 1024;
/// Average sampled document size above which no sampled dictionary is built.
const SAMPLED_DICT_MAX_DOC_BYTES: usize = 4 * 1024;

/// A dictionary for a segment's stored fields from a sample of its documents (`None` if too
/// little data): trained, or the sampled documents themselves finalized with zstd's entropy
/// tables for them (a raw-content dictionary without the tables decodes 2 KiB chunks no
/// faster than no dictionary).
pub(crate) fn build_dictionary(samples: &[Vec<u8>], train: bool, level: i32) -> Option<Vec<u8>> {
    let total: usize = samples.iter().map(|s| s.len()).sum();
    if total < DICT_MIN_SAMPLE_BYTES || samples.len() < 16 {
        return None;
    }
    if train {
        return zstd::dict::from_samples(samples, DICT_SIZE).ok();
    }
    // Documents that fill a chunk on their own gain little from shared content, and finalizing
    // statistics over them is slow (pmc articles of 38 KB: 0.4 s, ratio 2.47 against 2.68
    // without a dictionary).
    if total / samples.len() > SAMPLED_DICT_MAX_DOC_BYTES {
        return None;
    }
    // The statistics come from an evenly spaced subset (their cost grows with the bytes), the
    // content from evenly spaced documents up to the dictionary size.
    let every = total.div_ceil(SAMPLED_DICT_STATS_BYTES).max(1);
    let stats: Vec<&[u8]> = samples.iter().step_by(every).map(Vec::as_slice).collect();
    let mut content = Vec::with_capacity(DICT_SIZE);
    let step = total.div_ceil(DICT_SIZE).max(1);
    for s in samples.iter().step_by(step) {
        if content.len() + s.len() > DICT_SIZE {
            break;
        }
        content.extend_from_slice(s);
    }
    let flat = stats.concat();
    let sizes: Vec<usize> = stats.iter().map(|s| s.len()).collect();
    let mut out = vec![0u8; DICT_SIZE + 4096];
    let params = zstd::zstd_safe::zstd_sys::ZDICT_params_t { compressionLevel: level, notificationLevel: 0, dictID: 0 };
    // SAFETY: every pointer comes with its buffer's length; `sizes` sums to `flat.len()`; zstd
    // writes at most `out.len()` bytes and reads only the given ranges.
    let n = unsafe {
        zstd::zstd_safe::zstd_sys::ZDICT_finalizeDictionary(
            out.as_mut_ptr().cast(),
            out.len(),
            content.as_ptr().cast(),
            content.len(),
            flat.as_ptr().cast(),
            sizes.as_ptr(),
            sizes.len() as u32,
            params,
        )
    };
    // SAFETY: a pure function of the returned code.
    if unsafe { zstd::zstd_safe::zstd_sys::ZDICT_isError(n) } != 0 {
        return None;
    }
    out.truncate(n);
    Some(out)
}

/// The documents a dictionary is built from: up to 2048 evenly spaced slots, read in
/// bit-reversed slot order until [`DICT_MAX_SAMPLE_BYTES`], so a sample cut short by large
/// documents still spans the whole segment instead of its first documents.
pub(crate) fn sample_docs(max_doc: u32, mut read: impl FnMut(u32, &mut Vec<u8>) -> Result<()>) -> Result<Vec<Vec<u8>>> {
    let n = max_doc.min(2048);
    let step = max_doc.checked_div(n).unwrap_or(1).max(1);
    let bits = n.next_power_of_two().trailing_zeros();
    let mut samples = Vec::new();
    let mut total = 0usize;
    let mut doc = Vec::new();
    for i in 0..n.next_power_of_two() {
        let slot = if bits == 0 { 0 } else { i.reverse_bits() >> (32 - bits) };
        if slot >= n {
            continue;
        }
        doc.clear();
        read(slot * step, &mut doc)?;
        total += doc.len();
        samples.push(doc.clone());
        if total > DICT_MAX_SAMPLE_BYTES {
            break;
        }
    }
    Ok(samples)
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

    /// A reader with its own chunk cache (decompression contexts come from a shared pool).
    pub fn reader(&self) -> StoredReader {
        StoredReader {
            fields: self.clone(),
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

/// Decompression contexts kept for reuse: about one per core is enough (a context is held
/// only while one chunk decompresses), and the cap bounds what an idle process keeps.
static DCTX_POOL: std::sync::Mutex<Vec<zstd::zstd_safe::DCtx<'static>>> = std::sync::Mutex::new(Vec::new());
const DCTX_POOL_MAX: usize = 8;

/// Runs `f` with a pooled decompression context.
fn with_dctx<R>(f: impl FnOnce(&mut zstd::zstd_safe::DCtx<'static>) -> R) -> R {
    let pool = || DCTX_POOL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut dctx = pool().pop().unwrap_or_else(zstd::zstd_safe::DCtx::create);
    let r = f(&mut dctx);
    let mut pool = pool();
    if pool.len() < DCTX_POOL_MAX {
        pool.push(dctx);
    }
    r
}

/// Random access to documents, caching the last decompressed chunk.
pub struct StoredReader {
    fields: StoredFields,
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
        let buf = &mut self.buf;
        let n = with_dctx(|dctx| match &f.dict {
            Some(d) => dctx.decompress_using_ddict(buf, comp, d.0.as_ddict()),
            None => dctx.decompress(buf, comp),
        })
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

    /// Whether `doc` is in the chunk this reader has decompressed (reading it costs no
    /// decompression). Lets a pool of readers hand out the one that holds a doc.
    pub fn holds(&self, doc: u32) -> bool {
        if self.chunk == u32::MAX || doc >= self.fields.num_docs || self.chunk_ok().is_none() {
            return false;
        }
        let first = self.fields.chunk_first(self.chunk);
        let end = if self.chunk + 1 < self.fields.nchunks { self.fields.chunk_first(self.chunk + 1) } else { self.fields.num_docs };
        first <= doc && doc < end
    }

    fn chunk_ok(&self) -> Option<()> {
        (self.chunk < self.fields.nchunks).then_some(())
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
        let samples = sample_docs(n, |d, out| {
            out.extend_from_slice(&docs[d as usize]);
            Ok(())
        })
        .unwrap();
        let dict_bytes = if opts.dictionary {
            build_dictionary(&samples, opts.train, opts.level)
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
    fn holds_reports_the_decompressed_chunk() {
        let (f, docs) = build(3000, StoredOptions::default());
        let mut r = f.reader();
        assert!(!r.holds(0), "nothing decompressed yet");
        for &d in &[0u32, 1, 1500, 2999] {
            r.raw(d).unwrap();
            assert!(r.holds(d));
            // exactly the docs of the same chunk: reading them decompresses nothing new
            let held: Vec<u32> = (0..3000).filter(|&x| r.holds(x)).collect();
            assert!(held.contains(&d) && held.len() < 3000);
            assert_eq!(held.last().unwrap() - held[0] + 1, held.len() as u32, "a contiguous range");
            for &x in &held {
                assert_eq!(r.raw(x).unwrap(), &docs[x as usize][..]);
                assert!(r.holds(d), "same chunk still cached");
            }
        }
        assert!(!r.holds(3000));
    }

    #[test]
    fn round_trips_with_and_without_dictionary() {
        for (dictionary, train) in [(false, true), (true, true), (true, false)] {
            for n in [0u32, 1, 127, 128, 129, 3000] {
                let (f, docs) = build(
                    n,
                    StoredOptions {
                        dictionary,
                        train,
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
    fn samples_span_the_segment_when_cut_short() {
        // Small documents: every evenly spaced slot, each once.
        let mut seen = Vec::new();
        let all = sample_docs(10_000, |d, out| {
            seen.push(d);
            out.extend_from_slice(&[0; 100]);
            Ok(())
        })
        .unwrap();
        assert_eq!(all.len(), 2048);
        seen.sort_unstable();
        assert_eq!(seen, (0..2048).map(|i| i * 4).collect::<Vec<u32>>());
        // Large documents stop the sample at the byte budget, spread over the whole segment.
        let mut seen = Vec::new();
        let few = sample_docs(10_000, |d, out| {
            seen.push(d);
            out.resize(40_000, 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(few.len(), DICT_MAX_SAMPLE_BYTES / 40_000 + 1);
        assert!(seen.iter().any(|&d| d < 1_000) && seen.iter().any(|&d| d > 7_000), "{seen:?}");
        let mut distinct = seen.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), seen.len());
        for max_doc in [0u32, 1, 2, 3, 2047, 2049] {
            let mut n = 0;
            sample_docs(max_doc, |d, _| {
                assert!(d < max_doc);
                n += 1;
                Ok(())
            })
            .unwrap();
            assert_eq!(n, max_doc.min(2048));
        }
    }

    #[test]
    fn finalized_dictionary_compresses_like_a_trained_one() {
        let opts = |dictionary, train| StoredOptions { dictionary, train, ..Default::default() };
        let (none, _) = build(5000, opts(false, true));
        let (trained, _) = build(5000, opts(true, true));
        let (sampled, docs) = build(5000, opts(true, false));
        assert!(sampled.dict.is_some());
        let (n, t, s) = (none.chunks.len() as f64, trained.chunks.len() as f64, sampled.chunks.len() as f64);
        assert!(s < n * 0.9, "sampled {s} vs none {n}");
        assert!(s < t * 1.15, "sampled {s} vs trained {t}");
        let mut r = sampled.reader();
        for d in (0..5000).rev().step_by(7) {
            assert_eq!(r.raw(d).unwrap(), &docs[d as usize][..]);
        }
        // Too little data, or documents that fill chunks alone: no sampled dictionary.
        let docs = |len: usize, n: usize| std::iter::repeat_n(vec![1u8; len], n).collect::<Vec<_>>();
        assert!(build_dictionary(&docs(100, 100), false, 3).is_none());
        assert!(build_dictionary(&docs(8192, 64), false, 3).is_none());
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

    /// `cargo test --release -p search-codec --lib bench_stored -- --ignored --nocapture`;
    /// `ROLL_STORED_CORPORA=a.json,b.json` (one JSON document per line, e.g. the first 200k
    /// lines of Rally's geonames or http_logs corpora) measures those instead of the synthetic
    /// documents.
    #[test]
    #[ignore]
    fn bench_stored() {
        let mut rng = StdRng::seed_from_u64(9);
        let corpora: Vec<(String, Vec<Vec<u8>>)> = match std::env::var("ROLL_STORED_CORPORA") {
            Ok(paths) => paths
                .split(',')
                .map(|p| {
                    let text = std::fs::read(p).unwrap();
                    let docs = text
                        .split(|b| *b == b'\n')
                        .filter(|l| !l.is_empty())
                        .take(200_000)
                        .enumerate()
                        .map(|(i, l)| {
                            let mut w = StoredDocWriter::new();
                            w.add(0, StoredValue::Bytes(i.to_string().as_bytes()));
                            w.add(8, StoredValue::Bytes(l));
                            w.into_bytes()
                        })
                        .collect();
                    (p.to_string(), docs)
                })
                .collect(),
            Err(_) => {
                let words = ["alpha", "beta", "gamma", "delta", "search", "engine", "rust", "lucene", "shard", "segment"];
                let docs = (0..100_000u32)
                    .map(|i| {
                        let mut w = StoredDocWriter::new();
                        w.add(0, StoredValue::Str(&format!("id-{i}")));
                        let body: Vec<&str> = (0..rng.gen_range(50..250)).map(|_| words[rng.gen_range(0..words.len())]).collect();
                        let src = format!(r#"{{"title":"doc {i}","price":{},"tags":["a","b"],"body":"{}"}}"#, rng.gen_range(0..1000), body.join(" "));
                        w.add(1, StoredValue::Bytes(src.as_bytes()));
                        w.into_bytes()
                    })
                    .collect();
                vec![("synthetic".to_string(), docs)]
            }
        };
        for (corpus, docs) in &corpora {
            let n = docs.len() as u32;
            let raw: usize = docs.iter().map(|d| d.len()).sum();
            println!("{corpus}: {n} docs, {:.0} B/doc", raw as f64 / n as f64);
            let chunk = |chunk_bytes, train| StoredOptions { chunk_bytes, train, ..Default::default() };
            for (name, opts) in [
                ("zstd3, trained dict, 16K chunks", chunk(16 * 1024, true)),
                ("zstd3, trained dict, 4K chunks", chunk(4 * 1024, true)),
                ("zstd3, trained dict, 2K chunks", chunk(2 * 1024, true)),
                ("zstd3, trained dict, 1K chunks", chunk(1024, true)),
                ("best_compression (zstd6, trained dict, 16K chunks)", StoredOptions::best_compression()),
                ("zstd3, sampled dict, 2K chunks", chunk(2 * 1024, false)),
                ("zstd3, no dict, 16K chunks", StoredOptions { dictionary: false, chunk_bytes: 16 * 1024, ..Default::default() }),
                ("zstd3, no dict, 2K chunks", StoredOptions { dictionary: false, ..Default::default() }),
            ] {
                let samples = sample_docs(n, |d, out| {
                    out.extend_from_slice(&docs[d as usize]);
                    Ok(())
                })
                .unwrap();
                let t = std::time::Instant::now();
                let dict = if opts.dictionary { build_dictionary(&samples, opts.train, opts.level) } else { None };
                let dict_time = t.elapsed();
                let enc = dict.as_ref().map(|d| zstd::dict::EncoderDictionary::copy(d, opts.level));
                let t = std::time::Instant::now();
                let mut w = StoredWriter::new(opts.clone(), enc.as_ref()).unwrap();
                let mut bytes = Vec::new();
                for d in docs {
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
                    "  {name}: {:.1} B/doc (ratio {:.2}), dictionary {dict_time:?}, write {:.0} MB/s, random doc {random:?}, sequential doc {seq:?} ({sum})",
                    bytes.len() as f64 / n as f64,
                    raw as f64 / bytes.len() as f64,
                    raw as f64 / write.as_secs_f64() / 1e6
                );
            }
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
