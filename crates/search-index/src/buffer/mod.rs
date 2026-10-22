//! The in-memory indexing buffer: one writable segment (Lucene's `DocumentsWriterPerThread`).
//!
//! A shard keeps a small pool of buffers; an indexing thread borrows one, inverts a document
//! into it and returns it, so threads index concurrently without sharing hot data. A buffer is
//! frozen by a refresh (or by the memory limit) and written through the codec as a segment: it
//! implements [`SegmentSource`](search_codec::SegmentSource) directly (see `source.rs`).
//!
//! Layout, chosen so a document costs no allocation once the buffer has warmed up:
//! - per field, a [`TermHash`] gives each distinct term a dense id; postings of a term are two
//!   byte streams in a shared [`BytePool`] (doc deltas + freqs; position deltas, offsets,
//!   payloads) plus a few per-term integers kept as parallel vectors (struct of arrays);
//! - norms, doc values, points, vectors and completion entries are appended to flat per-field
//!   vectors in doc order (sorted and sorted-set values are interned through a `TermHash`, so
//!   the dictionary is built once at flush);
//! - stored fields are serialized with the codec's stored-field encoding into a chunked arena.
//!
//! Inversion follows Lucene's `IndexingChain` exactly (position/offset bookkeeping, the gap
//! after each analyzed value, `FieldInvertState` length/overlap/unique counts, the immense term
//! and bad-offset errors), because norms and positions decide scores and phrase matches.
//! A document that fails midway keeps its doc id and is marked deleted (Lucene's non-aborting
//! failure); for a nested block the whole block is deleted.

pub mod pool;
pub mod source;
pub mod termhash;

use std::sync::Arc;

use bytes::Bytes;
use search_codec::stored::{OwnedStoredValue, StoredValue, encode_field};
use search_codec::{DocValuesType, IndexOptions};

use crate::doc::{DocValue, IndexableDoc, IndexableField};
use crate::error::{Error, Result};
use crate::schema::{
    FieldSchema, ID_FIELD, PRIMARY_TERM_FIELD, SEQ_NO_FIELD, Schema, TERM_VECTORS_STORED_BIT, VERSION_FIELD,
};
use crate::similarity::InvertState;
use crate::termvectors;
use crate::util::bitset::FixedBitSet;
use pool::BytePool;
use termhash::TermHash;

/// Longest term Lucene accepts (`IndexWriter.MAX_TERM_LENGTH`).
pub const MAX_TERM_LENGTH: usize = 32766;
/// Largest position (`IndexWriter.MAX_POSITION`).
pub const MAX_POSITION: i64 = i32::MAX as i64 - 128;

/// Engine metadata of a root document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocMeta {
    pub seq_no: i64,
    pub version: i64,
    pub primary_term: i64,
}

/// Per-term postings state, struct of arrays indexed by term id.
#[derive(Default)]
struct Postings {
    doc_start: Vec<u32>,
    doc_w: Vec<u32>,
    pos_start: Vec<u32>,
    pos_w: Vec<u32>,
    /// Doc of the pending (last) doc of the term.
    last_doc: Vec<u32>,
    /// Delta of the pending doc from the one before it.
    last_code: Vec<u32>,
    /// Frequency in the pending doc.
    freq: Vec<u32>,
    last_pos: Vec<u32>,
    last_off: Vec<u32>,
}

impl Postings {
    fn ram_bytes(&self) -> usize {
        (self.doc_start.capacity()
            + self.doc_w.capacity()
            + self.pos_start.capacity()
            + self.pos_w.capacity()
            + self.last_doc.capacity()
            + self.last_code.capacity()
            + self.freq.capacity()
            + self.last_pos.capacity()
            + self.last_off.capacity())
            * 4
    }
}

/// Doc values of one field.
pub(crate) enum DvBuf {
    None,
    Numeric { docs: Vec<u32>, vals: Vec<i64> },
    Binary { docs: Vec<u32>, ends: Vec<u32>, bytes: Vec<u8> },
    Sorted { hash: TermHash, docs: Vec<u32>, ids: Vec<u32> },
    SortedSet { hash: TermHash, docs: Vec<u32>, starts: Vec<u32>, ids: Vec<u32> },
    SortedNumeric { docs: Vec<u32>, starts: Vec<u32>, vals: Vec<i64> },
}

impl DvBuf {
    fn ram_bytes(&self) -> usize {
        match self {
            DvBuf::None => 0,
            DvBuf::Numeric { docs, vals } => docs.capacity() * 4 + vals.capacity() * 8,
            DvBuf::Binary { docs, ends, bytes } => docs.capacity() * 4 + ends.capacity() * 4 + bytes.capacity(),
            DvBuf::Sorted { hash, docs, ids } => hash.ram_bytes() + docs.capacity() * 4 + ids.capacity() * 4,
            DvBuf::SortedSet { hash, docs, starts, ids } => {
                hash.ram_bytes() + (docs.capacity() + starts.capacity() + ids.capacity()) * 4
            }
            DvBuf::SortedNumeric { docs, starts, vals } => {
                (docs.capacity() + starts.capacity()) * 4 + vals.capacity() * 8
            }
        }
    }
}

/// Fixed-width per-doc values (points, vectors).
#[derive(Default)]
pub(crate) struct Packed {
    pub docs: Vec<u32>,
    pub bytes: Vec<u8>,
    pub width: usize,
}

/// Completion entries of one field.
#[derive(Default)]
pub(crate) struct SuggestBuf {
    pub docs: Vec<u32>,
    pub weights: Vec<u64>,
    pub key_ends: Vec<u32>,
    pub keys: Vec<u8>,
    pub payload_ends: Vec<u32>,
    pub payloads: Vec<u8>,
}

/// One term vector occurrence collected while inverting a doc.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TvOcc {
    pub term: u32,
    pub pos: u32,
    pub start: u32,
    pub end: u32,
    pub pay_start: u32,
    pub pay_len: u32,
}

/// Everything the buffer holds for one field.
pub(crate) struct FieldBuf {
    pub schema: FieldSchema,
    freqs: bool,
    positions: bool,
    offsets: bool,
    payloads: bool,
    pub terms: TermHash,
    post: Postings,
    /// Norms: docs and their bytes, in doc order.
    pub norm_docs: Vec<u32>,
    pub norms: Vec<u8>,
    dv: DvBuf,
    pub points: Packed,
    pub vectors: Packed,
    pub suggest: SuggestBuf,
    // Per-doc inversion state (Lucene FieldInvertState).
    inv_position: i64,
    inv_last_position: i64,
    inv_offset: i64,
    inv_last_start: i64,
    inv: InvertState,
    in_doc: bool,
    tv_occ: Vec<TvOcc>,
    tv_payloads: Vec<u8>,
    cached_ram: usize,
}

impl FieldBuf {
    fn new(schema: FieldSchema) -> FieldBuf {
        let io = schema.index;
        let dv = match schema.doc_values {
            DocValuesType::None => DvBuf::None,
            DocValuesType::Numeric => DvBuf::Numeric { docs: Vec::new(), vals: Vec::new() },
            DocValuesType::Binary => DvBuf::Binary { docs: Vec::new(), ends: Vec::new(), bytes: Vec::new() },
            DocValuesType::Sorted => DvBuf::Sorted { hash: TermHash::new(), docs: Vec::new(), ids: Vec::new() },
            DocValuesType::SortedSet => {
                DvBuf::SortedSet { hash: TermHash::new(), docs: Vec::new(), starts: Vec::new(), ids: Vec::new() }
            }
            DocValuesType::SortedNumeric => {
                DvBuf::SortedNumeric { docs: Vec::new(), starts: Vec::new(), vals: Vec::new() }
            }
        };
        let points = Packed { width: schema.points.map_or(0, |p| p.packed_len()), ..Default::default() };
        let vectors = Packed { width: schema.vectors.map_or(0, |v| v.vector_bytes()), ..Default::default() };
        FieldBuf {
            freqs: io.has_freqs(),
            positions: io.has_positions(),
            offsets: io.has_offsets(),
            payloads: schema.payloads && io.has_positions(),
            schema,
            terms: TermHash::new(),
            post: Postings::default(),
            norm_docs: Vec::new(),
            norms: Vec::new(),
            dv,
            points,
            vectors,
            suggest: SuggestBuf::default(),
            inv_position: -1,
            inv_last_position: 0,
            inv_offset: 0,
            inv_last_start: 0,
            inv: InvertState::default(),
            in_doc: false,
            tv_occ: Vec::new(),
            tv_payloads: Vec::new(),
            cached_ram: 0,
        }
    }

    fn ram_bytes(&self) -> usize {
        self.terms.ram_bytes()
            + self.post.ram_bytes()
            + self.norm_docs.capacity() * 5
            + self.dv.ram_bytes()
            + self.points.docs.capacity() * 4
            + self.points.bytes.capacity()
            + self.vectors.docs.capacity() * 4
            + self.vectors.bytes.capacity()
            + self.suggest.docs.capacity() * 24
            + self.suggest.keys.capacity()
            + self.suggest.payloads.capacity()
            + self.tv_occ.capacity() * 24
            + 512
    }


    /// Whether the field has any data in the buffer.
    fn has_data(&self) -> bool {
        !self.terms.is_empty()
            || !self.norm_docs.is_empty()
            || !matches!(self.dv, DvBuf::None)
            || !self.points.docs.is_empty()
            || !self.vectors.docs.is_empty()
            || !self.suggest.docs.is_empty()
    }
}

/// Stored documents: serialized bytes in fixed-size chunks (a doc may straddle chunks), so a
/// large buffer never reallocates and copies its whole stored data.
#[derive(Default)]
pub(crate) struct StoredArena {
    chunks: Vec<Vec<u8>>,
    ends: Vec<u64>,
    len: u64,
}

const STORED_CHUNK: usize = 1 << 20;

impl StoredArena {
    fn push_doc(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        while !rest.is_empty() {
            if self.chunks.last().is_none_or(|c| c.len() == STORED_CHUNK) {
                self.chunks.push(Vec::with_capacity(STORED_CHUNK));
            }
            let c = self.chunks.last_mut().unwrap();
            let n = (STORED_CHUNK - c.len()).min(rest.len());
            c.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
        }
        self.len += bytes.len() as u64;
        self.ends.push(self.len);
    }

    pub fn doc(&self, doc: u32, out: &mut Vec<u8>) {
        let d = doc as usize;
        if d >= self.ends.len() {
            return;
        }
        let start = if d == 0 { 0 } else { self.ends[d - 1] };
        let end = self.ends[d];
        let mut pos = start;
        while pos < end {
            let c = (pos / STORED_CHUNK as u64) as usize;
            let off = (pos % STORED_CHUNK as u64) as usize;
            let n = ((end - pos) as usize).min(STORED_CHUNK - off);
            out.extend_from_slice(&self.chunks[c][off..off + n]);
            pos += n as u64;
        }
    }

    fn ram_bytes(&self) -> usize {
        self.chunks.len() * STORED_CHUNK + self.ends.capacity() * 8
    }
}

/// One writable in-memory segment.
pub struct Buffer {
    pub(crate) uid: u64,
    pub(crate) schema: Arc<Schema>,
    pub(crate) max_doc: u32,
    pub(crate) fields: Vec<Option<Box<FieldBuf>>>,
    pub(crate) pool: BytePool,
    pub(crate) stored: StoredArena,
    /// Docs that failed while being indexed (deleted from the start).
    pub(crate) aborted: Vec<u32>,
    /// Docs that are roots of their block (carry `_primary_term`); nested children are not.
    pub(crate) roots: FixedBitSet,
    pub(crate) has_nested: bool,
    pub(crate) max_seq_no: i64,
    /// Field numbers touched by the doc being indexed.
    touched: Vec<u32>,
    stored_scratch: Vec<u8>,
    tv_scratch: Vec<u8>,
    ram: usize,
}

impl Buffer {
    pub fn new(uid: u64, schema: Arc<Schema>) -> Buffer {
        Buffer {
            uid,
            schema,
            max_doc: 0,
            fields: Vec::new(),
            pool: BytePool::new(),
            stored: StoredArena::default(),
            aborted: Vec::new(),
            roots: FixedBitSet::new(0),
            has_nested: false,
            max_seq_no: -1,
            touched: Vec::new(),
            stored_scratch: Vec::new(),
            tv_scratch: Vec::new(),
            ram: 0,
        }
    }

    pub fn uid(&self) -> u64 {
        self.uid
    }

    pub fn max_doc(&self) -> u32 {
        self.max_doc
    }

    /// Updates the schema (fields added by mappings; existing field buffers keep their options).
    pub fn set_schema(&mut self, schema: Arc<Schema>) {
        self.schema = schema;
    }

    /// Approximate heap bytes held.
    pub fn ram_bytes(&self) -> usize {
        self.ram
    }

    fn recompute_ram(&mut self) {
        let mut r = self.pool.ram_bytes() + self.stored.ram_bytes() + self.roots.ram_bytes() + self.aborted.capacity() * 4;
        for f in self.fields.iter().flatten() {
            r += f.cached_ram;
        }
        self.ram = r + 4096;
    }

    /// Serialized stored fields of `doc` (appended to `out`).
    pub fn stored(&self, doc: u32, out: &mut Vec<u8>) {
        self.stored.doc(doc, out)
    }

    pub(crate) fn field(&self, number: u32) -> Option<&FieldBuf> {
        self.fields.get(number as usize).and_then(|f| f.as_deref())
    }

    /// Whether `doc` is a root doc (not a nested child).
    pub fn is_root(&self, doc: u32) -> bool {
        self.roots.get(doc)
    }

    fn field_buf(&mut self, number: u32) -> Result<&mut FieldBuf> {
        let n = number as usize;
        if self.fields.len() <= n {
            self.fields.resize_with(n + 1, || None);
        }
        if self.fields[n].is_none() {
            let fs = self
                .schema
                .field(number)
                .ok_or_else(|| Error::illegal(format!("unknown field number {number}")))?
                .clone();
            self.fields[n] = Some(Box::new(FieldBuf::new(fs)));
        }
        Ok(self.fields[n].as_deref_mut().unwrap())
    }

    /// Indexes a document and its nested children as one block (children first); returns the
    /// doc id of the root. `meta` goes on the root only. On failure every doc of the block that
    /// was started is marked deleted and the error is returned.
    pub fn add_block(&mut self, doc: &IndexableDoc, meta: DocMeta) -> Result<u32> {
        let first = self.max_doc;
        let total = doc.doc_count() as u64;
        if self.max_doc as u64 + total > search_codec::MAX_DOCS as u64 {
            return Err(Error::illegal(format!(
                "number of documents in the index cannot exceed {}",
                search_codec::MAX_DOCS
            )));
        }
        let r = self.add_rec(doc, meta, true);
        if r.is_err() {
            for d in first..self.max_doc {
                self.aborted.push(d);
            }
        }
        if !doc.nested.is_empty() {
            self.has_nested = true;
        }
        if meta.seq_no > self.max_seq_no {
            self.max_seq_no = meta.seq_no;
        }
        self.recompute_ram();
        r
    }

    fn add_rec(&mut self, doc: &IndexableDoc, meta: DocMeta, root: bool) -> Result<u32> {
        for c in &doc.nested {
            self.add_rec(c, meta, false)?;
        }
        self.add_one(doc, root.then_some(meta))
    }

    fn add_one(&mut self, doc: &IndexableDoc, meta: Option<DocMeta>) -> Result<u32> {
        let d = self.max_doc;
        self.max_doc += 1;
        if self.roots.len() < self.max_doc {
            let mut words = std::mem::take(&mut self.roots).words().to_vec();
            let len = (self.max_doc as usize).next_power_of_two().max(1024) as u32;
            words.resize((len as usize).div_ceil(64), 0);
            self.roots = FixedBitSet::from_words(words, len);
        }
        self.touched.clear();
        self.stored_scratch.clear();
        let mut result = Ok(());
        if let Some(m) = meta {
            self.roots.set(d);
            result = self.add_meta(d, &doc.id, m);
        }
        if result.is_ok() {
            for f in &doc.fields {
                if let Err(e) = self.add_field(d, f) {
                    result = Err(e);
                    break;
                }
            }
        }
        // Stored fields: always written so doc ids and stored docs stay aligned.
        for s in &doc.stored {
            encode_field(&mut self.stored_scratch, s.field, stored_ref(&s.value));
        }
        let fin = self.finish_doc(d);
        let mut scratch = std::mem::take(&mut self.stored_scratch);
        self.stored.push_doc(&scratch);
        scratch.clear();
        self.stored_scratch = scratch;
        result?;
        fin?;
        Ok(d)
    }

    fn add_meta(&mut self, d: u32, id: &Bytes, m: DocMeta) -> Result<()> {
        if id.is_empty() {
            return Err(Error::illegal("if _id is specified it must not be empty"));
        }
        self.add_term(d, ID_FIELD, id)?;
        encode_field(&mut self.stored_scratch, ID_FIELD, StoredValue::Bytes(id));
        self.add_point(d, SEQ_NO_FIELD, &search_codec::points::encode::i64_bytes(m.seq_no))?;
        self.add_doc_value(d, SEQ_NO_FIELD, &DocValue::Numeric(m.seq_no))?;
        self.add_doc_value(d, VERSION_FIELD, &DocValue::Numeric(m.version))?;
        self.add_doc_value(d, PRIMARY_TERM_FIELD, &DocValue::Numeric(m.primary_term))?;
        Ok(())
    }

    fn touch(&mut self, number: u32) -> Result<&mut FieldBuf> {
        let fb = self.field_buf(number)?;
        if !fb.in_doc {
            fb.in_doc = true;
            fb.inv_position = -1;
            fb.inv_last_position = 0;
            fb.inv_offset = 0;
            fb.inv_last_start = 0;
            fb.inv = InvertState { docs_only: !fb.freqs, ..Default::default() };
            self.touched.push(number);
        }
        Ok(self.fields[number as usize].as_deref_mut().unwrap())
    }

    fn add_field(&mut self, d: u32, f: &IndexableField) -> Result<()> {
        match f {
            IndexableField::Text { field, analyzer, values } => {
                let refs: Vec<&str> = values.iter().map(|s| s.as_str()).collect();
                self.add_text(d, *field, analyzer, &refs)
            }
            IndexableField::Tokens { field, tokens } => {
                let number = *field;
                check_indexed(self.touch(number)?)?;
                let pool = &mut self.pool;
                let fb = self.fields[number as usize].as_deref_mut().unwrap();
                for t in tokens {
                    if t.freq != 1 && fb.positions {
                        return Err(Error::illegal(format!(
                            "field \"{}\": cannot index positions while using custom TermFrequencyAttribute",
                            fb.schema.name
                        )));
                    }
                    let pos = fb.inv_position + t.position_increment as i64;
                    let start = fb.inv_offset + t.start_offset as i64;
                    let end = fb.inv_offset + t.end_offset as i64;
                    invert_token(
                        pool,
                        fb,
                        d,
                        &t.term,
                        t.position_increment,
                        pos,
                        start,
                        end,
                        t.payload.as_deref(),
                        t.freq.max(1),
                        false,
                    )?;
                }
                if let Some(last) = tokens.last() {
                    fb.inv_offset += last.end_offset as i64;
                }
                Ok(())
            }
            IndexableField::Term { field, term } => self.add_term(d, *field, term),
            IndexableField::Point { field, packed } => self.add_point(d, *field, packed),
            IndexableField::DocValue { field, value } => self.add_doc_value(d, *field, value),
            IndexableField::Vector { field, value } => {
                let fb = self.touch(*field)?;
                let Some(spec) = fb.schema.vectors else {
                    return Err(Error::illegal(format!("field [{}] does not index vectors", fb.schema.name)));
                };
                if value.len() != spec.vector_bytes() {
                    return Err(Error::illegal(format!(
                        "vector of field [{}] has {} bytes, expected {}",
                        fb.schema.name,
                        value.len(),
                        spec.vector_bytes()
                    )));
                }
                if fb.vectors.docs.last() == Some(&d) {
                    return Err(Error::illegal(format!(
                        "VectorValuesField \"{}\" appears more than once in this document (only one value is allowed per field)",
                        fb.schema.name
                    )));
                }
                fb.vectors.docs.push(d);
                fb.vectors.bytes.extend_from_slice(value);
                Ok(())
            }
            IndexableField::Suggest { field, key, weight, payload } => {
                let fb = self.touch(*field)?;
                if !fb.schema.suggest {
                    return Err(Error::illegal(format!("field [{}] is not a completion field", fb.schema.name)));
                }
                if *weight > i64::MAX as u64 {
                    return Err(Error::illegal(format!("weight must be <= {}", i64::MAX)));
                }
                let s = &mut fb.suggest;
                s.docs.push(d);
                s.weights.push(*weight);
                s.keys.extend_from_slice(key);
                s.key_ends.push(s.keys.len() as u32);
                s.payloads.extend_from_slice(payload);
                s.payload_ends.push(s.payloads.len() as u32);
                Ok(())
            }
        }
    }

    fn add_text(&mut self, d: u32, number: u32, analyzer: &search_analysis::Analyzer, values: &[&str]) -> Result<()> {
        check_indexed(self.touch(number)?)?;
        let pool = &mut self.pool;
        let fb = self.fields[number as usize].as_deref_mut().unwrap();
        let base_pos = fb.inv_position + 1;
        let base_off = fb.inv_offset;
        let mut err: Option<Error> = None;
        let mut last_abs: i64 = -1;
        let mut any = false;
        let r = analyzer.try_analyze_values(values, &mut |t| {
            if err.is_some() {
                return;
            }
            any = true;
            let pos = base_pos + t.position as i64;
            last_abs = t.position as i64;
            if let Err(e) = invert_token(
                pool,
                fb,
                d,
                t.term.as_bytes(),
                t.position_increment,
                pos,
                base_off + t.start_offset as i64,
                base_off + t.end_offset as i64,
                t.payload,
                1,
                false,
            ) {
                err = Some(e);
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        r?;
        // State after the values: Lucene adds the final increment (0 here), the final offset
        // and the gap after each value.
        let gap = analyzer.position_increment_gap() as i64;
        let ogap = analyzer.offset_gap() as i64;
        let mut off = 0i64;
        for v in values {
            off += v.encode_utf16().count() as i64 + ogap;
        }
        if any {
            fb.inv_position = base_pos + last_abs + gap;
        } else {
            fb.inv_position += gap * values.len() as i64;
        }
        fb.inv_offset = base_off + off;
        Ok(())
    }

    fn add_term(&mut self, d: u32, number: u32, term: &[u8]) -> Result<()> {
        self.touch(number)?;
        let pool = &mut self.pool;
        let fb = self.fields[number as usize].as_deref_mut().unwrap();
        check_indexed(fb)?;
        if term.len() > MAX_TERM_LENGTH {
            return Err(immense_term(&fb.schema.name, term));
        }
        // Lucene's invertTerm: one more position, length counted twice (as Lucene does).
        let pos = fb.inv_position + 1;
        let start = fb.inv_offset;
        let end = fb.inv_offset;
        invert_token(pool, fb, d, term, 1, pos, start, end, None, 1, true)?;
        fb.inv.length += 1;
        Ok(())
    }

    fn add_point(&mut self, d: u32, number: u32, packed: &[u8]) -> Result<()> {
        let fb = self.touch(number)?;
        let Some(spec) = fb.schema.points else {
            return Err(Error::illegal(format!("field [{}] does not index points", fb.schema.name)));
        };
        if packed.len() != spec.packed_len() {
            return Err(Error::illegal(format!(
                "point of field [{}] has {} bytes, expected {}",
                fb.schema.name,
                packed.len(),
                spec.packed_len()
            )));
        }
        fb.points.docs.push(d);
        fb.points.bytes.extend_from_slice(packed);
        Ok(())
    }

    fn add_doc_value(&mut self, d: u32, number: u32, v: &DocValue) -> Result<()> {
        let fb = self.touch(number)?;
        let name = &fb.schema.name;
        let twice = || {
            Error::illegal(format!(
                "DocValuesField \"{name}\" appears more than once in this document (only one value is allowed per field)"
            ))
        };
        let too_large = || {
            Error::illegal(format!("DocValuesField \"{name}\" is too large, must be <= {MAX_TERM_LENGTH}"))
        };
        match (&mut fb.dv, v) {
            (DvBuf::Numeric { docs, vals }, DocValue::Numeric(x)) => {
                if docs.last() == Some(&d) {
                    return Err(twice());
                }
                docs.push(d);
                vals.push(*x);
            }
            (DvBuf::Binary { docs, ends, bytes }, DocValue::Binary(b)) => {
                if docs.last() == Some(&d) {
                    return Err(twice());
                }
                docs.push(d);
                bytes.extend_from_slice(b);
                ends.push(bytes.len() as u32);
            }
            (DvBuf::Sorted { hash, docs, ids }, DocValue::Sorted(b)) => {
                if docs.last() == Some(&d) {
                    return Err(twice());
                }
                if b.len() > MAX_TERM_LENGTH {
                    return Err(too_large());
                }
                docs.push(d);
                ids.push(hash.add(b).0);
            }
            (DvBuf::SortedSet { hash, docs, starts, ids }, DocValue::SortedSet(b)) => {
                if b.len() > MAX_TERM_LENGTH {
                    return Err(too_large());
                }
                if docs.last() != Some(&d) {
                    docs.push(d);
                    starts.push(ids.len() as u32);
                }
                ids.push(hash.add(b).0);
            }
            (DvBuf::SortedNumeric { docs, starts, vals }, DocValue::SortedNumeric(x)) => {
                if docs.last() != Some(&d) {
                    docs.push(d);
                    starts.push(vals.len() as u32);
                }
                vals.push(*x);
            }
            (have, _) => {
                let want = match v {
                    DocValue::Numeric(_) => "NUMERIC",
                    DocValue::Binary(_) => "BINARY",
                    DocValue::Sorted(_) => "SORTED",
                    DocValue::SortedSet(_) => "SORTED_SET",
                    DocValue::SortedNumeric(_) => "SORTED_NUMERIC",
                };
                let cur = match have {
                    DvBuf::None => "NONE",
                    DvBuf::Numeric { .. } => "NUMERIC",
                    DvBuf::Binary { .. } => "BINARY",
                    DvBuf::Sorted { .. } => "SORTED",
                    DvBuf::SortedSet { .. } => "SORTED_SET",
                    DvBuf::SortedNumeric { .. } => "SORTED_NUMERIC",
                };
                return Err(Error::illegal(format!(
                    "cannot change DocValues type from {cur} to {want} for field \"{name}\""
                )));
            }
        }
        Ok(())
    }

    /// Writes norms and term vectors of the fields the doc touched.
    fn finish_doc(&mut self, d: u32) -> Result<()> {
        let touched = std::mem::take(&mut self.touched);
        let mut tv_err = Ok(());
        for &n in &touched {
            let fb = self.fields[n as usize].as_deref_mut().unwrap();
            fb.in_doc = false;
            if fb.schema.norms && fb.schema.index.is_indexed() && fb.inv_position >= 0 {
                let norm = if fb.inv.length == 0 {
                    0
                } else {
                    self.schema.similarity(n).compute_norm(&fb.inv)
                };
                fb.norm_docs.push(d);
                fb.norms.push(norm);
            }
            if let Some(tv) = fb.schema.term_vectors
                && !fb.tv_occ.is_empty()
            {
                self.tv_scratch.clear();
                let r = termvectors::encode(&mut self.tv_scratch, tv, &fb.terms, &mut fb.tv_occ, &fb.tv_payloads);
                if r.is_ok() {
                    encode_field(&mut self.stored_scratch, n | TERM_VECTORS_STORED_BIT, StoredValue::Bytes(&self.tv_scratch));
                } else {
                    tv_err = r;
                }
                fb.tv_occ.clear();
                fb.tv_payloads.clear();
            }
            fb.cached_ram = fb.ram_bytes();
        }
        self.touched = touched;
        tv_err
    }

    /// Unique terms of a field in this buffer.
    pub fn num_terms(&self, field: u32) -> usize {
        self.field(field).map_or(0, |f| f.terms.len())
    }
}

fn stored_ref(v: &OwnedStoredValue) -> StoredValue<'_> {
    match v {
        OwnedStoredValue::Str(s) => StoredValue::Str(s),
        OwnedStoredValue::Bytes(b) => StoredValue::Bytes(b),
        OwnedStoredValue::Int(x) => StoredValue::Int(*x),
        OwnedStoredValue::Long(x) => StoredValue::Long(*x),
        OwnedStoredValue::Float(x) => StoredValue::Float(*x),
        OwnedStoredValue::Double(x) => StoredValue::Double(*x),
    }
}

fn check_indexed(fb: &FieldBuf) -> Result<()> {
    if fb.schema.index == IndexOptions::None {
        return Err(Error::illegal(format!("field [{}] is not indexed", fb.schema.name)));
    }
    Ok(())
}

/// `Arrays.toString(byte[30])` of a term prefix (Java signed bytes).
/// Elasticsearch's rejection of an un-analyzed value too long to index (`KeywordFieldMapper`,
/// at parse time: single spaces, unlike Lucene's analyzed-text message).
fn immense_term(field: &str, term: &[u8]) -> Error {
    Error::illegal(format!(
        "Document contains at least one immense term in field=\"{field}\" (whose UTF8 encoding is longer than the max length {MAX_TERM_LENGTH}), all of which were skipped. Please correct the analyzer to not produce such terms. The prefix of the first immense term is: '{}...'",
        java_byte_prefix(term)
    ))
}

/// The checks Elasticsearch makes while parsing a document, before the engine gives it a
/// sequence number: an immense un-analyzed term (`IndexableField::Term`), in the doc or a
/// nested child. A doc failing them uses no sequence number.
pub(crate) fn precheck(schema: &Schema, doc: &IndexableDoc) -> Result<()> {
    for f in &doc.fields {
        if let IndexableField::Term { field, term } = f
            && term.len() > MAX_TERM_LENGTH
        {
            return Err(immense_term(&schema.name(*field), term));
        }
    }
    doc.nested.iter().try_for_each(|c| precheck(schema, c))
}

fn java_byte_prefix(term: &[u8]) -> String {
    let mut s = String::from("[");
    for (i, &b) in term.iter().take(30).enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&(b as i8).to_string());
    }
    s.push(']');
    s
}

/// Adds one token occurrence: Lucene's position/offset checks, the term hash and the
/// postings streams. `pos` and `start`/`end` are absolute within the doc's field.
#[allow(clippy::too_many_arguments)]
fn invert_token(
    pool: &mut BytePool,
    fb: &mut FieldBuf,
    d: u32,
    term: &[u8],
    pos_inc: u32,
    pos: i64,
    start: i64,
    end: i64,
    payload: Option<&[u8]>,
    freq: u32,
    binary: bool,
) -> Result<()> {
    let name = fb.schema.name.as_str();
    if pos < fb.inv_last_position {
        if pos_inc == 0 {
            return Err(Error::illegal(format!("first position increment must be > 0 (got 0) for field '{name}'")));
        }
        return Err(Error::illegal(format!(
            "position overflowed Integer.MAX_VALUE (got posIncr={pos_inc} lastPosition={} position={pos}) for field '{name}'",
            fb.inv_last_position
        )));
    } else if pos > MAX_POSITION {
        return Err(Error::illegal(format!(
            "position {pos} is too large for field '{name}': max allowed position is {MAX_POSITION}"
        )));
    }
    if !binary && (start < fb.inv_last_start || end < start) {
        return Err(Error::illegal(format!(
            "startOffset must be non-negative, and endOffset must be >= startOffset, and offsets must not go backwards startOffset={start},endOffset={end},lastStartOffset={} for field '{name}'",
            fb.inv_last_start
        )));
    }
    // Single terms (`binary`) were checked by the caller with their own message.
    if !binary && term.len() > MAX_TERM_LENGTH {
        return Err(Error::illegal(format!(
            "Document contains at least one immense term in field=\"{name}\" (whose UTF8 encoding is longer than the max length {MAX_TERM_LENGTH}), all of which were skipped.  Please correct the analyzer to not produce such terms.  The prefix of the first immense term is: '{}...', original message: bytes can be at most {MAX_TERM_LENGTH} in length; got {}",
            java_byte_prefix(term),
            term.len()
        )));
    }
    let Some(length) = fb.inv.length.checked_add(freq) else {
        return Err(Error::illegal(format!("too many tokens for field \"{name}\"")));
    };
    fb.inv.length = length;
    fb.inv_position = pos;
    fb.inv_last_position = pos;
    if pos_inc == 0 {
        fb.inv.num_overlap += 1;
    }
    fb.inv_last_start = start;
    let pos = pos as u32;
    let (start, end) = (start.max(0) as u32, end.max(0) as u32);
    let (id, new) = fb.terms.add(term);
    let p = &mut fb.post;
    if new {
        let ds = pool.new_stream();
        p.doc_start.push(ds);
        p.doc_w.push(ds);
        if fb.positions {
            let ps = pool.new_stream();
            p.pos_start.push(ps);
            p.pos_w.push(ps);
            p.last_pos.push(0);
            if fb.offsets {
                p.last_off.push(0);
            }
        }
        p.last_doc.push(d);
        p.last_code.push(d);
        p.freq.push(0);
        fb.inv.unique_terms += 1;
    } else if p.last_doc[id as usize] != d {
        let i = id as usize;
        let code = p.last_code[i];
        let mut w = p.doc_w[i];
        if fb.freqs {
            let f = p.freq[i];
            if f == 1 {
                pool.write_vu32(&mut w, code << 1 | 1);
            } else {
                pool.write_vu32(&mut w, code << 1);
                pool.write_vu32(&mut w, f);
            }
        } else {
            pool.write_vu32(&mut w, code);
        }
        p.doc_w[i] = w;
        p.last_code[i] = d - p.last_doc[i];
        p.last_doc[i] = d;
        p.freq[i] = 0;
        if fb.positions {
            p.last_pos[i] = 0;
            if fb.offsets {
                p.last_off[i] = 0;
            }
        }
        fb.inv.unique_terms += 1;
    }
    let i = id as usize;
    p.freq[i] = p.freq[i].saturating_add(freq);
    if fb.positions {
        let mut w = p.pos_w[i];
        let delta = pos - p.last_pos[i];
        p.last_pos[i] = pos;
        match payload {
            Some(pl) if fb.payloads && !pl.is_empty() => {
                pool.write_vu32(&mut w, delta << 1 | 1);
                pool.write_vu32(&mut w, pl.len() as u32);
                pool.write_bytes(&mut w, pl);
            }
            _ if fb.payloads => pool.write_vu32(&mut w, delta << 1),
            _ => pool.write_vu32(&mut w, delta),
        }
        if fb.offsets {
            pool.write_vu32(&mut w, start - p.last_off[i]);
            pool.write_vu32(&mut w, end - start);
            p.last_off[i] = start;
        }
        p.pos_w[i] = w;
    }
    if fb.schema.term_vectors.is_some() {
        let ps = fb.tv_payloads.len() as u32;
        let pl = payload.unwrap_or(&[]);
        fb.tv_payloads.extend_from_slice(pl);
        fb.tv_occ.push(TvOcc { term: id, pos, start, end, pay_start: ps, pay_len: pl.len() as u32 });
    }
    Ok(())
}

impl FieldBuf {
    pub(crate) fn doc_stream(&self, id: u32) -> (u32, u32) {
        let i = id as usize;
        (self.post.doc_start[i], self.post.doc_w[i])
    }
    pub(crate) fn pos_stream(&self, id: u32) -> Option<(u32, u32)> {
        let i = id as usize;
        self.positions.then(|| (self.post.pos_start[i], self.post.pos_w[i]))
    }
    pub(crate) fn pending(&self, id: u32) -> (u32, u32) {
        let i = id as usize;
        (self.post.last_doc[i], self.post.freq[i])
    }
    pub(crate) fn flags(&self) -> (bool, bool, bool, bool) {
        (self.freqs, self.positions, self.offsets, self.payloads)
    }
    pub(crate) fn dv(&self) -> &DvBuf {
        &self.dv
    }
    pub(crate) fn has_any(&self) -> bool {
        self.has_data()
    }
}
