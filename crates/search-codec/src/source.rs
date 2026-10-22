//! What [`write_segment`](crate::write_segment) pulls a segment from.
//!
//! Both the indexing buffer (in `search-index`) and merges implement [`SegmentSource`]. The
//! codec pulls, field by field: sorted terms with their postings, norms, doc values, points and
//! vectors, then every document's serialized stored fields. Pull-style sources keep the writer in
//! control of memory and ordering; sources only need forward iteration (plus random access to
//! stored documents and dictionary values).
//!
//! Requirements on sources (checked by the writer, which fails with `InvalidArgument`):
//! - terms in strictly increasing byte order; postings in strictly increasing doc order;
//! - norms, doc values, points and vectors in any doc order (doc order is cheapest: the writer
//!   then skips sorting), each doc at most once per field (points: any number of values);
//! - sorted / sorted-set dictionaries strictly increasing; ordinals in range.
//!   Sorted-set ordinals and sorted-numeric values of a doc may come in any order (the writer
//!   sorts them, and deduplicates set ordinals).
//!
//! [`DocMap`] renumbers documents (dropping deleted ones and/or permuting them), and
//! [`RemappedSource`] applies one to any source: each term's postings are buffered and replayed
//! in the new order, every per-doc value streams through renumbered (the writer sorts it in
//! place). Index sorting at flush and reordered merges (index sort or BP reordering) are both
//! "a source + a DocMap".

use std::cell::RefCell;
use std::collections::BTreeMap;

use crate::NO_MORE_DOCS;
use crate::error::{Result, invalid};
use crate::info::{FieldInfo, IndexSort};
use crate::spill::{SpillOptions, SpillSlice, SpillVec};
use crate::suggest::{SuggestEntry, SuggestSource};
use crate::vectors::VectorsReader;

/// Postings of one term, in increasing doc order.
pub trait PostingsSource {
    /// Next doc, or [`NO_MORE_DOCS`].
    fn next_doc(&mut self) -> u32;
    /// Frequency of the current doc (ignored for fields without frequencies).
    fn freq(&mut self) -> u32;
    /// Next position of the current doc (call `freq()` times; fields with positions only).
    fn next_position(&mut self) -> u32;
    /// Offsets of the last position.
    fn offsets(&self) -> (u32, u32) {
        (0, 0)
    }
    /// Payload of the last position.
    fn payload(&self) -> &[u8] {
        &[]
    }
}

/// Terms of one field in increasing byte order.
pub trait TermsSource {
    /// Advances to the next term.
    fn next_term(&mut self) -> Result<Option<&[u8]>>;
    /// Postings of the current term.
    fn postings(&mut self) -> Result<&mut dyn PostingsSource>;
}

/// `(doc, bytes)` pairs: binary doc values, points (any doc order) and vectors.
pub trait BytesSource {
    fn next(&mut self) -> Option<(u32, &[u8])>;
}

/// A sorted (single-valued) doc values field.
pub trait SortedSource {
    /// Number of distinct values.
    fn value_count(&self) -> u64;
    /// The value with ordinal `ord` (values are in strictly increasing order).
    fn value(&mut self, ord: u64) -> &[u8];
    /// Next `(doc, ord)`.
    fn next_doc(&mut self) -> Option<(u32, u64)>;
}

/// A sorted-set doc values field.
pub trait SortedSetSource {
    fn value_count(&self) -> u64;
    fn value(&mut self, ord: u64) -> &[u8];
    /// Next `(doc, ords)`.
    fn next_doc(&mut self) -> Option<(u32, &[u64])>;
}

/// A sorted-numeric doc values field.
pub trait SortedNumericSource {
    /// Next `(doc, values)`.
    fn next_doc(&mut self) -> Option<(u32, &[i64])>;
}

/// Norms of a field: `(doc, norm byte)`, any doc order.
pub type NormsIter<'a> = Box<dyn Iterator<Item = (u32, u8)> + 'a>;

/// The doc values of one field.
pub enum DocValuesSource<'a> {
    Numeric(Box<dyn Iterator<Item = (u32, i64)> + 'a>),
    Binary(Box<dyn BytesSource + 'a>),
    Sorted(Box<dyn SortedSource + 'a>),
    SortedSet(Box<dyn SortedSetSource + 'a>),
    SortedNumeric(Box<dyn SortedNumericSource + 'a>),
}

/// A segment to write.
pub trait SegmentSource {
    fn segment_id(&self) -> [u8; 16];
    fn max_doc(&self) -> u32;
    /// Fields of the segment (any order).
    fn field_infos(&self) -> Vec<FieldInfo>;
    /// Index sort to apply / record.
    fn index_sort(&self) -> Option<IndexSort> {
        None
    }
    /// Whether docs are already in index-sort order (the writer then does not sort).
    fn is_sorted(&self) -> bool {
        false
    }
    fn attributes(&self) -> BTreeMap<String, String> {
        BTreeMap::new()
    }
    fn terms(&self, field: u32) -> Result<Option<Box<dyn TermsSource + '_>>>;
    /// Norm bytes (`SmallFloat` encoded lengths), any doc order.
    fn norms(&self, field: u32) -> Result<Option<NormsIter<'_>>>;
    fn doc_values(&self, field: u32) -> Result<Option<DocValuesSource<'_>>>;
    /// Packed point values (any order).
    fn points(&self, field: u32) -> Result<Option<Box<dyn BytesSource + '_>>>;
    /// Raw vectors, any doc order (storage encoding of the field's element type).
    fn vectors(&self, field: u32) -> Result<Option<Box<dyn BytesSource + '_>>>;
    /// Appends `doc`'s serialized stored fields (see [`crate::stored`]) to `out`.
    fn stored(&self, doc: u32, out: &mut Vec<u8>) -> Result<()>;
    /// Whether [`stored`](Self::stored) is as cheap in random doc order as in sequence (an
    /// in-memory buffer). When it is not (compressed segments: the default), a reordering
    /// [`RemappedSource`] copies the documents once in their own order instead of reading
    /// them scattered.
    fn stored_random_access_is_cheap(&self) -> bool {
        false
    }
    /// Merges: an existing HNSW graph of `field` the writer may start from instead of
    /// inserting every vector again (Lucene's incremental graph merge). Returns the reader that
    /// holds the graph and, for each of its ordinals, the doc that vector becomes in this
    /// source. Only offer a graph none of whose vectors are dropped.
    fn vector_graph_seed(&self, _field: u32) -> Option<(VectorsReader, Vec<u32>)> {
        None
    }
    /// Suggest (completion) entries of a field marked [`FieldInfo::suggest`], any order.
    fn suggest(&self, _field: u32) -> Result<Option<Box<dyn SuggestSource + '_>>> {
        Ok(None)
    }
}

/// Marker for a deleted doc in a [`DocMap`].
pub const DELETED: u32 = u32::MAX;

/// A renumbering of docs: old ids to new ids, with deleted docs dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocMap {
    old_to_new: Vec<u32>,
    new_to_old: Vec<u32>,
}

impl DocMap {
    /// From the new order: `new_to_old[new] = old`. Old docs not listed are deleted.
    pub fn from_new_to_old(new_to_old: Vec<u32>, old_max_doc: u32) -> Result<DocMap> {
        let mut old_to_new = vec![DELETED; old_max_doc as usize];
        for (new, &old) in new_to_old.iter().enumerate() {
            let slot = old_to_new
                .get_mut(old as usize)
                .ok_or_else(|| invalid(format!("doc map: old doc {old} out of range")))?;
            if *slot != DELETED {
                return Err(invalid(format!("doc map: old doc {old} mapped twice")));
            }
            *slot = new as u32;
        }
        Ok(DocMap {
            old_to_new,
            new_to_old,
        })
    }

    /// Keeps doc order, dropping docs for which `live` is false.
    pub fn compact(old_max_doc: u32, live: impl Fn(u32) -> bool) -> DocMap {
        let mut old_to_new = vec![DELETED; old_max_doc as usize];
        let mut new_to_old = Vec::with_capacity(old_max_doc as usize);
        for d in 0..old_max_doc {
            if live(d) {
                old_to_new[d as usize] = new_to_old.len() as u32;
                new_to_old.push(d);
            }
        }
        DocMap {
            old_to_new,
            new_to_old,
        }
    }

    #[inline]
    pub fn old_to_new(&self, old: u32) -> Option<u32> {
        match self.old_to_new.get(old as usize) {
            Some(&n) if n != DELETED => Some(n),
            _ => None,
        }
    }

    #[inline]
    pub fn new_to_old(&self, new: u32) -> u32 {
        self.new_to_old[new as usize]
    }

    pub fn old_max_doc(&self) -> u32 {
        self.old_to_new.len() as u32
    }

    pub fn new_max_doc(&self) -> u32 {
        self.new_to_old.len() as u32
    }

    /// Whether the map changes nothing.
    pub fn is_identity(&self) -> bool {
        self.old_to_new.len() == self.new_to_old.len()
            && self
                .new_to_old
                .iter()
                .enumerate()
                .all(|(i, &o)| i as u32 == o)
    }

    /// Whether new order follows old order (only deletions).
    pub fn is_monotonic(&self) -> bool {
        self.new_to_old.windows(2).all(|w| w[0] < w[1])
    }
}

/// Buffered postings of one term, replayed in a new doc order.
#[derive(Default)]
pub(crate) struct BufferedPostings {
    // (new doc, freq, first position index)
    docs: Vec<(u32, u32, u32)>,
    positions: Vec<(u32, u32, u32, u32, u32)>, // pos, start, end, payload start, payload len
    payloads: Vec<u8>,
    cur: usize,
    pos_idx: usize,
    started: bool,
}

impl BufferedPostings {
    pub(crate) fn clear(&mut self) {
        self.docs.clear();
        self.positions.clear();
        self.payloads.clear();
        self.cur = 0;
        self.pos_idx = 0;
        self.started = false;
    }

    /// Reads all of `src`, mapping docs through `map` (dropping deleted ones), then sorts.
    pub(crate) fn fill(
        &mut self,
        src: &mut dyn PostingsSource,
        map: &dyn Fn(u32) -> Option<u32>,
        freqs: bool,
        positions: bool,
    ) {
        self.clear();
        loop {
            let d = src.next_doc();
            if d == NO_MORE_DOCS {
                break;
            }
            let Some(nd) = map(d) else { continue };
            let f = if freqs { src.freq() } else { 1 };
            let first = self.positions.len() as u32;
            if positions {
                for _ in 0..f {
                    let p = src.next_position();
                    let (s, e) = src.offsets();
                    let pl = src.payload();
                    let ps = self.payloads.len() as u32;
                    self.payloads.extend_from_slice(pl);
                    self.positions.push((p, s, e, ps, pl.len() as u32));
                }
            }
            self.docs.push((nd, f, first));
        }
        if !self.docs.windows(2).all(|w| w[0].0 < w[1].0) {
            self.docs.sort_unstable_by_key(|d| d.0);
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }
}

impl PostingsSource for BufferedPostings {
    fn next_doc(&mut self) -> u32 {
        if self.started {
            self.cur += 1;
        } else {
            self.started = true;
        }
        match self.docs.get(self.cur) {
            Some(&(d, _, first)) => {
                self.pos_idx = first as usize;
                d
            }
            None => NO_MORE_DOCS,
        }
    }
    fn freq(&mut self) -> u32 {
        self.docs.get(self.cur).map_or(1, |d| d.1)
    }
    fn next_position(&mut self) -> u32 {
        let p = self.positions.get(self.pos_idx).map_or(0, |p| p.0);
        self.pos_idx += 1;
        p
    }
    fn offsets(&self) -> (u32, u32) {
        self.positions
            .get(self.pos_idx.wrapping_sub(1))
            .map_or((0, 0), |p| (p.1, p.2))
    }
    fn payload(&self) -> &[u8] {
        match self.positions.get(self.pos_idx.wrapping_sub(1)) {
            Some(p) => &self.payloads[p.3 as usize..(p.3 + p.4) as usize],
            None => &[],
        }
    }
}

/// A source with its docs renumbered by a [`DocMap`].
pub struct RemappedSource<'a> {
    inner: &'a dyn SegmentSource,
    map: DocMap,
    sort: Option<IndexSort>,
    fields: Vec<FieldInfo>,
    spill: SpillOptions,
    /// Stored documents in old order, when the new order jumps around too much to read them
    /// from the inner source one by one (see [`SegmentSource::stored`] below).
    stored_cache: RefCell<Option<StoredCache>>,
    scattered: bool,
}

/// Every kept document's stored bytes, read sequentially from the inner source: `ends[old]` is
/// where old doc `old`'s bytes end in `bytes` (deleted docs are empty).
struct StoredCache {
    bytes: SpillSlice<u8>,
    ends: SpillSlice<u64>,
}

/// Whether a permutation is scattered enough that reading stored documents in its order would
/// decompress the same chunks over and over: fewer than 32 docs per run of consecutive old docs.
fn scattered(new_to_old: &[u32]) -> bool {
    let jumps = new_to_old.windows(2).filter(|w| w[1] != w[0].wrapping_add(1)).count();
    jumps.saturating_mul(32) > new_to_old.len()
}

impl<'a> RemappedSource<'a> {
    /// `map.old_max_doc()` must equal `inner.max_doc()`. `sort` is recorded as the new order's
    /// index sort (it marks the output as sorted).
    pub fn new(
        inner: &'a dyn SegmentSource,
        map: DocMap,
        sort: Option<IndexSort>,
    ) -> Result<RemappedSource<'a>> {
        if map.old_max_doc() != inner.max_doc() {
            return Err(invalid("doc map does not match the source's max_doc"));
        }
        let fields = inner.field_infos();
        let scattered = !inner.stored_random_access_is_cheap() && scattered(&map.new_to_old);
        Ok(RemappedSource {
            inner,
            map,
            sort,
            fields,
            spill: SpillOptions::default(),
            stored_cache: RefCell::new(None),
            scattered,
        })
    }

    /// Where the stored-fields copy of a scattered reordering spills (see
    /// [`WriteOptions::spill`](crate::WriteOptions)).
    pub fn set_spill(&mut self, spill: SpillOptions) {
        self.spill = spill;
    }

    fn build_stored_cache(&self) -> Result<StoredCache> {
        let mut bytes = SpillVec::<u8>::new(&self.spill);
        let mut ends = SpillVec::<u64>::new(&self.spill);
        let mut buf = Vec::new();
        for old in 0..self.map.old_max_doc() {
            if self.map.old_to_new(old).is_some() {
                buf.clear();
                self.inner.stored(old, &mut buf)?;
                bytes.extend_from_slice(&buf)?;
            }
            ends.push(bytes.len() as u64)?;
        }
        Ok(StoredCache { bytes: bytes.finish()?, ends: ends.finish()? })
    }

    pub fn doc_map(&self) -> &DocMap {
        &self.map
    }

    fn field(&self, number: u32) -> Option<&FieldInfo> {
        self.fields.iter().find(|f| f.number == number)
    }
}

struct RemappedTerms<'s> {
    inner: Box<dyn TermsSource + 's>,
    map: &'s DocMap,
    term: Vec<u8>,
    buf: BufferedPostings,
    freqs: bool,
    positions: bool,
}

impl TermsSource for RemappedTerms<'_> {
    fn next_term(&mut self) -> Result<Option<&[u8]>> {
        loop {
            match self.inner.next_term()? {
                None => return Ok(None),
                Some(t) => {
                    self.term.clear();
                    self.term.extend_from_slice(t);
                }
            }
            let map = self.map;
            let p = self.inner.postings()?;
            self.buf
                .fill(p, &|d| map.old_to_new(d), self.freqs, self.positions);
            if !self.buf.is_empty() {
                return Ok(Some(&self.term));
            }
        }
    }
    fn postings(&mut self) -> Result<&mut dyn PostingsSource> {
        Ok(&mut self.buf)
    }
}

/// Bytes per doc (binary doc values, vectors, points) renumbered on the fly. Copied (a value
/// is small): the lending signature cannot hand out the inner borrow from inside the loop.
struct RemappedBytes<'s> {
    inner: Box<dyn BytesSource + 's>,
    map: &'s DocMap,
    buf: Vec<u8>,
}

impl BytesSource for RemappedBytes<'_> {
    fn next(&mut self) -> Option<(u32, &[u8])> {
        loop {
            let (d, b) = self.inner.next()?;
            if let Some(nd) = self.map.old_to_new(d) {
                self.buf.clear();
                self.buf.extend_from_slice(b);
                return Some((nd, &self.buf));
            }
        }
    }
}

/// Suggest entries renumbered on the fly (copied: see [`RemappedBytes`]).
struct RemappedSuggest<'s> {
    inner: Box<dyn SuggestSource + 's>,
    map: &'s DocMap,
    key: Vec<u8>,
    payload: Vec<u8>,
}

impl SuggestSource for RemappedSuggest<'_> {
    fn next(&mut self) -> Option<SuggestEntry<'_>> {
        loop {
            let e = self.inner.next()?;
            if let Some(nd) = self.map.old_to_new(e.doc) {
                let weight = e.weight;
                self.key.clear();
                self.key.extend_from_slice(e.key);
                self.payload.clear();
                self.payload.extend_from_slice(e.payload);
                return Some(SuggestEntry { key: &self.key, weight, doc: nd, payload: &self.payload });
            }
        }
    }
}

struct RemappedSorted<'s> {
    inner: Box<dyn SortedSource + 's>,
    map: &'s DocMap,
}

impl SortedSource for RemappedSorted<'_> {
    fn value_count(&self) -> u64 {
        self.inner.value_count()
    }
    fn value(&mut self, ord: u64) -> &[u8] {
        self.inner.value(ord)
    }
    fn next_doc(&mut self) -> Option<(u32, u64)> {
        loop {
            let (d, o) = self.inner.next_doc()?;
            if let Some(nd) = self.map.old_to_new(d) {
                return Some((nd, o));
            }
        }
    }
}

struct RemappedSortedSet<'s> {
    inner: Box<dyn SortedSetSource + 's>,
    map: &'s DocMap,
    buf: Vec<u64>,
}

impl SortedSetSource for RemappedSortedSet<'_> {
    fn value_count(&self) -> u64 {
        self.inner.value_count()
    }
    fn value(&mut self, ord: u64) -> &[u8] {
        self.inner.value(ord)
    }
    fn next_doc(&mut self) -> Option<(u32, &[u64])> {
        loop {
            let (d, os) = self.inner.next_doc()?;
            if let Some(nd) = self.map.old_to_new(d) {
                self.buf.clear();
                self.buf.extend_from_slice(os);
                return Some((nd, &self.buf));
            }
        }
    }
}

struct RemappedSortedNumeric<'s> {
    inner: Box<dyn SortedNumericSource + 's>,
    map: &'s DocMap,
    buf: Vec<i64>,
}

impl SortedNumericSource for RemappedSortedNumeric<'_> {
    fn next_doc(&mut self) -> Option<(u32, &[i64])> {
        loop {
            let (d, vs) = self.inner.next_doc()?;
            if let Some(nd) = self.map.old_to_new(d) {
                self.buf.clear();
                self.buf.extend_from_slice(vs);
                return Some((nd, &self.buf));
            }
        }
    }
}

impl SegmentSource for RemappedSource<'_> {
    fn segment_id(&self) -> [u8; 16] {
        self.inner.segment_id()
    }
    fn max_doc(&self) -> u32 {
        self.map.new_max_doc()
    }
    fn field_infos(&self) -> Vec<FieldInfo> {
        self.fields.clone()
    }
    fn index_sort(&self) -> Option<IndexSort> {
        self.sort.clone().or_else(|| self.inner.index_sort())
    }
    fn is_sorted(&self) -> bool {
        self.sort.is_some() || (self.inner.is_sorted() && self.map.is_monotonic())
    }
    fn attributes(&self) -> BTreeMap<String, String> {
        self.inner.attributes()
    }
    fn terms(&self, field: u32) -> Result<Option<Box<dyn TermsSource + '_>>> {
        let Some(info) = self.field(field) else {
            return Ok(None);
        };
        let (freqs, positions) = (info.index.has_freqs(), info.index.has_positions());
        Ok(self.inner.terms(field)?.map(|inner| {
            Box::new(RemappedTerms {
                inner,
                map: &self.map,
                term: Vec::new(),
                buf: BufferedPostings::default(),
                freqs,
                positions,
            }) as Box<dyn TermsSource + '_>
        }))
    }
    // Per-doc values stream through in the inner order with renumbered docs: the writer accepts
    // any doc order and sorts in place, so a reordered merge holds no second copy of a field.
    fn norms(&self, field: u32) -> Result<Option<NormsIter<'_>>> {
        let map = &self.map;
        Ok(self
            .inner
            .norms(field)?
            .map(|it| Box::new(it.filter_map(move |(d, n)| map.old_to_new(d).map(|nd| (nd, n)))) as NormsIter<'_>))
    }
    fn doc_values(&self, field: u32) -> Result<Option<DocValuesSource<'_>>> {
        let Some(dv) = self.inner.doc_values(field)? else {
            return Ok(None);
        };
        let map = &self.map;
        Ok(Some(match dv {
            DocValuesSource::Numeric(it) => {
                DocValuesSource::Numeric(Box::new(it.filter_map(move |(d, x)| map.old_to_new(d).map(|nd| (nd, x)))))
            }
            DocValuesSource::Binary(inner) => {
                DocValuesSource::Binary(Box::new(RemappedBytes { inner, map, buf: Vec::new() }))
            }
            DocValuesSource::Sorted(inner) => DocValuesSource::Sorted(Box::new(RemappedSorted { inner, map })),
            DocValuesSource::SortedSet(inner) => {
                DocValuesSource::SortedSet(Box::new(RemappedSortedSet { inner, map, buf: Vec::new() }))
            }
            DocValuesSource::SortedNumeric(inner) => {
                DocValuesSource::SortedNumeric(Box::new(RemappedSortedNumeric { inner, map, buf: Vec::new() }))
            }
        }))
    }
    fn points(&self, field: u32) -> Result<Option<Box<dyn BytesSource + '_>>> {
        let map = &self.map;
        Ok(self
            .inner
            .points(field)?
            .map(|inner| Box::new(RemappedBytes { inner, map, buf: Vec::new() }) as Box<dyn BytesSource>))
    }
    fn vectors(&self, field: u32) -> Result<Option<Box<dyn BytesSource + '_>>> {
        let map = &self.map;
        Ok(self
            .inner
            .vectors(field)?
            .map(|inner| Box::new(RemappedBytes { inner, map, buf: Vec::new() }) as Box<dyn BytesSource>))
    }
    // Reordered merges read stored documents in the new order. When that order is scattered,
    // reading each one from the inner source would decompress a whole chunk per document; copy
    // them once in old order instead (page cache past the spill budget) and serve from there.
    fn stored(&self, doc: u32, out: &mut Vec<u8>) -> Result<()> {
        if doc >= self.map.new_max_doc() {
            return Err(invalid(format!("doc {doc} >= max_doc {}", self.map.new_max_doc())));
        }
        let old = self.map.new_to_old(doc);
        let mut cache = self.stored_cache.borrow_mut();
        if cache.is_none() {
            if !self.scattered {
                drop(cache);
                return self.inner.stored(old, out);
            }
            *cache = Some(self.build_stored_cache()?);
        }
        let c = cache.as_ref().unwrap();
        let start = if old == 0 { 0 } else { c.ends[old as usize - 1] as usize };
        out.extend_from_slice(&c.bytes[start..c.ends[old as usize] as usize]);
        Ok(())
    }
    fn suggest(&self, field: u32) -> Result<Option<Box<dyn SuggestSource + '_>>> {
        let map = &self.map;
        Ok(self.inner.suggest(field)?.map(|inner| {
            Box::new(RemappedSuggest { inner, map, key: Vec::new(), payload: Vec::new() }) as Box<dyn SuggestSource>
        }))
    }
    fn vector_graph_seed(&self, field: u32) -> Option<(VectorsReader, Vec<u32>)> {
        let (r, docs) = self.inner.vector_graph_seed(field)?;
        let docs = docs
            .into_iter()
            .map(|d| self.map.old_to_new(d))
            .collect::<Option<Vec<u32>>>()?;
        Some((r, docs))
    }
}
