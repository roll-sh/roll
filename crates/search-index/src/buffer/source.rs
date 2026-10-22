//! A frozen [`Buffer`] as a codec [`SegmentSource`]: the refresh writes it with
//! `write_segment` (wrapped in a `RemappedSource` when the index is sorted).
//!
//! Terms are emitted in byte order by sorting the field's term ids once; postings decode the
//! pool streams directly. Sorted and sorted-set doc values build their dictionary order once
//! (term ids sorted by bytes, plus the inverse map id → ordinal) and stream `(doc, ord)` pairs
//! without copying values.

use std::collections::BTreeMap;

use search_codec::source::{
    BytesSource, DocValuesSource, NormsIter, PostingsSource, SegmentSource, SortedNumericSource, SortedSetSource,
    SortedSource, TermsSource,
};
use search_codec::suggest::{SuggestEntry, SuggestSource};
use search_codec::{FieldInfo, NO_MORE_DOCS, Result as CResult};

use super::pool::{BytePool, SliceReader};
use super::{Buffer, DvBuf, FieldBuf, Packed, SuggestBuf};

/// Postings of one term of a buffer.
pub struct BufPostings<'a> {
    docs: SliceReader<'a>,
    pos: Option<SliceReader<'a>>,
    freqs: bool,
    offsets: bool,
    payloads: bool,
    pending: (u32, u32),
    pending_done: bool,
    doc: u32,
    freq: u32,
    last_pos: u32,
    last_off: u32,
    start: u32,
    end: u32,
    payload: Vec<u8>,
}

impl<'a> BufPostings<'a> {
    pub(crate) fn new(fb: &'a FieldBuf, pool: &'a BytePool, id: u32) -> BufPostings<'a> {
        let (ds, dw) = fb.doc_stream(id);
        let (freqs, _positions, offsets, payloads) = fb.flags();
        BufPostings {
            docs: pool.reader(ds, dw),
            pos: fb.pos_stream(id).map(|(s, w)| pool.reader(s, w)),
            freqs,
            offsets,
            payloads,
            pending: fb.pending(id),
            pending_done: false,
            doc: 0,
            freq: 0,
            last_pos: 0,
            last_off: 0,
            start: 0,
            end: 0,
            payload: Vec::new(),
        }
    }
}

impl PostingsSource for BufPostings<'_> {
    fn next_doc(&mut self) -> u32 {
        if !self.docs.eof() {
            if self.freqs {
                let code = self.docs.read_vu32();
                self.doc += code >> 1;
                self.freq = if code & 1 != 0 { 1 } else { self.docs.read_vu32() };
            } else {
                self.doc += self.docs.read_vu32();
                self.freq = 1;
            }
        } else if !self.pending_done {
            self.pending_done = true;
            self.doc = self.pending.0;
            self.freq = self.pending.1;
        } else {
            return NO_MORE_DOCS;
        }
        self.last_pos = 0;
        self.last_off = 0;
        self.doc
    }

    fn freq(&mut self) -> u32 {
        self.freq
    }

    fn next_position(&mut self) -> u32 {
        let Some(r) = self.pos.as_mut() else { return 0 };
        let code = r.read_vu32();
        let delta = if self.payloads { code >> 1 } else { code };
        self.payload.clear();
        if self.payloads && code & 1 != 0 {
            let n = r.read_vu32() as usize;
            r.read_into(&mut self.payload, n);
        }
        self.last_pos += delta;
        if self.offsets {
            self.start = self.last_off + r.read_vu32();
            self.end = self.start + r.read_vu32();
            self.last_off = self.start;
        }
        self.last_pos
    }

    fn offsets(&self) -> (u32, u32) {
        (self.start, self.end)
    }

    fn payload(&self) -> &[u8] {
        &self.payload
    }
}

struct BufTerms<'a> {
    fb: &'a FieldBuf,
    pool: &'a BytePool,
    ids: Vec<u32>,
    next: usize,
    cur: Option<BufPostings<'a>>,
}

impl TermsSource for BufTerms<'_> {
    fn next_term(&mut self) -> CResult<Option<&[u8]>> {
        if self.next >= self.ids.len() {
            return Ok(None);
        }
        let id = self.ids[self.next];
        self.next += 1;
        self.cur = Some(BufPostings::new(self.fb, self.pool, id));
        Ok(Some(self.fb.terms.get(id)))
    }

    fn postings(&mut self) -> CResult<&mut dyn PostingsSource> {
        match self.cur.as_mut() {
            Some(p) => Ok(p),
            None => Err(search_codec::Error::InvalidArgument("postings before first term".into())),
        }
    }
}

struct PackedSource<'a> {
    p: &'a Packed,
    i: usize,
}

impl BytesSource for PackedSource<'_> {
    fn next(&mut self) -> Option<(u32, &[u8])> {
        let d = *self.p.docs.get(self.i)?;
        let w = self.p.width;
        let b = &self.p.bytes[self.i * w..(self.i + 1) * w];
        self.i += 1;
        Some((d, b))
    }
}

struct BinarySource<'a> {
    docs: &'a [u32],
    ends: &'a [u32],
    bytes: &'a [u8],
    i: usize,
}

impl BytesSource for BinarySource<'_> {
    fn next(&mut self) -> Option<(u32, &[u8])> {
        let d = *self.docs.get(self.i)?;
        let s = if self.i == 0 { 0 } else { self.ends[self.i - 1] as usize };
        let e = self.ends[self.i] as usize;
        self.i += 1;
        Some((d, &self.bytes[s..e]))
    }
}

/// Dictionary order of an interned doc values field.
fn dict_order(hash: &super::termhash::TermHash) -> (Vec<u32>, Vec<u32>) {
    let sorted = hash.sorted_ids();
    let mut ord_of = vec![0u32; sorted.len()];
    for (o, &id) in sorted.iter().enumerate() {
        ord_of[id as usize] = o as u32;
    }
    (sorted, ord_of)
}

struct SortedSrc<'a> {
    hash: &'a super::termhash::TermHash,
    sorted: Vec<u32>,
    ord_of: Vec<u32>,
    docs: &'a [u32],
    ids: &'a [u32],
    i: usize,
}

impl SortedSource for SortedSrc<'_> {
    fn value_count(&self) -> u64 {
        self.sorted.len() as u64
    }
    fn value(&mut self, ord: u64) -> &[u8] {
        self.hash.get(self.sorted[ord as usize])
    }
    fn next_doc(&mut self) -> Option<(u32, u64)> {
        let d = *self.docs.get(self.i)?;
        let o = self.ord_of[self.ids[self.i] as usize] as u64;
        self.i += 1;
        Some((d, o))
    }
}

struct SortedSetSrc<'a> {
    hash: &'a super::termhash::TermHash,
    sorted: Vec<u32>,
    ord_of: Vec<u32>,
    docs: &'a [u32],
    starts: &'a [u32],
    ids: &'a [u32],
    i: usize,
    ords: Vec<u64>,
}

impl SortedSetSource for SortedSetSrc<'_> {
    fn value_count(&self) -> u64 {
        self.sorted.len() as u64
    }
    fn value(&mut self, ord: u64) -> &[u8] {
        self.hash.get(self.sorted[ord as usize])
    }
    fn next_doc(&mut self) -> Option<(u32, &[u64])> {
        let d = *self.docs.get(self.i)?;
        let s = self.starts[self.i] as usize;
        let e = self.starts.get(self.i + 1).map_or(self.ids.len(), |&x| x as usize);
        self.ords.clear();
        for &id in &self.ids[s..e] {
            self.ords.push(self.ord_of[id as usize] as u64);
        }
        self.i += 1;
        Some((d, &self.ords))
    }
}

struct SortedNumSrc<'a> {
    docs: &'a [u32],
    starts: &'a [u32],
    vals: &'a [i64],
    i: usize,
}

impl SortedNumericSource for SortedNumSrc<'_> {
    fn next_doc(&mut self) -> Option<(u32, &[i64])> {
        let d = *self.docs.get(self.i)?;
        let s = self.starts[self.i] as usize;
        let e = self.starts.get(self.i + 1).map_or(self.vals.len(), |&x| x as usize);
        self.i += 1;
        Some((d, &self.vals[s..e]))
    }
}

struct SuggestSrc<'a> {
    s: &'a SuggestBuf,
    i: usize,
}

impl SuggestSource for SuggestSrc<'_> {
    fn next(&mut self) -> Option<SuggestEntry<'_>> {
        let i = self.i;
        let doc = *self.s.docs.get(i)?;
        self.i += 1;
        let ks = if i == 0 { 0 } else { self.s.key_ends[i - 1] as usize };
        let ps = if i == 0 { 0 } else { self.s.payload_ends[i - 1] as usize };
        Some(SuggestEntry {
            key: &self.s.keys[ks..self.s.key_ends[i] as usize],
            weight: self.s.weights[i],
            doc,
            payload: &self.s.payloads[ps..self.s.payload_ends[i] as usize],
        })
    }
}

/// A buffer viewed as a segment source with a given id.
pub struct BufferSource<'a> {
    pub buf: &'a Buffer,
    pub id: [u8; 16],
    pub attributes: BTreeMap<String, String>,
}

impl BufferSource<'_> {
    fn fb(&self, field: u32) -> Option<&FieldBuf> {
        self.buf.field(field)
    }
}

impl SegmentSource for BufferSource<'_> {
    fn segment_id(&self) -> [u8; 16] {
        self.id
    }

    fn max_doc(&self) -> u32 {
        self.buf.max_doc
    }

    fn field_infos(&self) -> Vec<FieldInfo> {
        self.buf
            .fields
            .iter()
            .flatten()
            .filter(|f| f.has_any())
            .map(|f| {
                let mut fi = f.schema.field_info();
                if f.terms.is_empty() {
                    fi.index = search_codec::IndexOptions::None;
                    fi.norms = false;
                    fi.payloads = false;
                }
                if f.points.docs.is_empty() {
                    fi.points = None;
                }
                if f.vectors.docs.is_empty() {
                    fi.vectors = None;
                }
                fi
            })
            .collect()
    }

    fn attributes(&self) -> BTreeMap<String, String> {
        self.attributes.clone()
    }

    fn terms(&self, field: u32) -> CResult<Option<Box<dyn TermsSource + '_>>> {
        let Some(fb) = self.fb(field) else { return Ok(None) };
        if fb.terms.is_empty() {
            return Ok(None);
        }
        Ok(Some(Box::new(BufTerms { fb, pool: &self.buf.pool, ids: fb.terms.sorted_ids(), next: 0, cur: None })))
    }

    fn norms(&self, field: u32) -> CResult<Option<NormsIter<'_>>> {
        let Some(fb) = self.fb(field) else { return Ok(None) };
        if fb.norm_docs.is_empty() || fb.terms.is_empty() {
            return Ok(None);
        }
        Ok(Some(Box::new(fb.norm_docs.iter().copied().zip(fb.norms.iter().copied()))))
    }

    fn doc_values(&self, field: u32) -> CResult<Option<DocValuesSource<'_>>> {
        let Some(fb) = self.fb(field) else { return Ok(None) };
        Ok(match fb.dv() {
            DvBuf::None => None,
            DvBuf::Numeric { docs, vals } => {
                if docs.is_empty() {
                    None
                } else {
                    Some(DocValuesSource::Numeric(Box::new(docs.iter().copied().zip(vals.iter().copied()))))
                }
            }
            DvBuf::Binary { docs, ends, bytes } => {
                if docs.is_empty() {
                    None
                } else {
                    Some(DocValuesSource::Binary(Box::new(BinarySource { docs, ends, bytes, i: 0 })))
                }
            }
            DvBuf::Sorted { hash, docs, ids } => {
                if docs.is_empty() {
                    None
                } else {
                    let (sorted, ord_of) = dict_order(hash);
                    Some(DocValuesSource::Sorted(Box::new(SortedSrc { hash, sorted, ord_of, docs, ids, i: 0 })))
                }
            }
            DvBuf::SortedSet { hash, docs, starts, ids } => {
                if docs.is_empty() {
                    None
                } else {
                    let (sorted, ord_of) = dict_order(hash);
                    Some(DocValuesSource::SortedSet(Box::new(SortedSetSrc {
                        hash,
                        sorted,
                        ord_of,
                        docs,
                        starts,
                        ids,
                        i: 0,
                        ords: Vec::new(),
                    })))
                }
            }
            DvBuf::SortedNumeric { docs, starts, vals } => {
                if docs.is_empty() {
                    None
                } else {
                    Some(DocValuesSource::SortedNumeric(Box::new(SortedNumSrc { docs, starts, vals, i: 0 })))
                }
            }
        })
    }

    fn points(&self, field: u32) -> CResult<Option<Box<dyn BytesSource + '_>>> {
        let Some(fb) = self.fb(field) else { return Ok(None) };
        if fb.points.docs.is_empty() {
            return Ok(None);
        }
        Ok(Some(Box::new(PackedSource { p: &fb.points, i: 0 })))
    }

    fn vectors(&self, field: u32) -> CResult<Option<Box<dyn BytesSource + '_>>> {
        let Some(fb) = self.fb(field) else { return Ok(None) };
        if fb.vectors.docs.is_empty() {
            return Ok(None);
        }
        Ok(Some(Box::new(PackedSource { p: &fb.vectors, i: 0 })))
    }

    fn stored(&self, doc: u32, out: &mut Vec<u8>) -> CResult<()> {
        self.buf.stored(doc, out);
        Ok(())
    }

    fn stored_random_access_is_cheap(&self) -> bool {
        true
    }

    fn suggest(&self, field: u32) -> CResult<Option<Box<dyn SuggestSource + '_>>> {
        let Some(fb) = self.fb(field) else { return Ok(None) };
        if fb.suggest.docs.is_empty() {
            return Ok(None);
        }
        Ok(Some(Box::new(SuggestSrc { s: &fb.suggest, i: 0 })))
    }
}
