//! `MergeSource`: N segments (with their live docs) as one [`SegmentSource`].
//!
//! Docs are concatenated in input order with deleted docs dropped, so every per-term postings
//! list and per-field value stream (points included, leaf by leaf) is produced by chaining the
//! inputs' cursors through their doc maps — no buffering. A doc map is the input's first new doc
//! plus a rank over its live-docs bitset (a running count per 64 docs), half a bit per doc
//! instead of a `u32`. Terms are k-way merged across inputs, reusing postings cursors from term
//! to term; a term whose docs are all deleted is dropped. Sorted and sorted-set dictionaries are
//! merged into one (keeping only values still used by a live doc) with a per-input ordinal map,
//! like Lucene's `OrdinalMap`.
//!
//! To merge into a different order (index sorting, BP reordering) wrap the merge source in a
//! [`RemappedSource`](crate::source::RemappedSource) with the desired [`DocMap`](crate::DocMap);
//! [`write_segment`](crate::write_segment) does that automatically for index sorts.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::NO_MORE_DOCS;
use crate::docvalues::TermDict;
use crate::spill::{SpillOptions, SpillSlice, SpillVec};
use crate::error::{Result, invalid};
use crate::info::{FieldInfo, IndexOptions, IndexSort};
use crate::livedocs::LiveDocs;
use crate::postings::{PostingsCursor, PostingsFlags};
use crate::reader::{DocValuesReader, SegmentReader};
use crate::source::{
    BytesSource, DELETED, DocValuesSource, PostingsSource, SegmentSource, SortedNumericSource,
    SortedSetSource, SortedSource, TermsSource,
};
use crate::stored::StoredReader;
use crate::suggest::{SuggestEntry, SuggestSource};
use crate::terms::TermsIter;

/// One segment to merge.
#[derive(Clone, Debug)]
pub struct MergeInput {
    pub reader: SegmentReader,
    /// `None` when no doc is deleted.
    pub live: Option<Arc<LiveDocs>>,
}

/// Several segments as one source.
pub struct MergeSource {
    inputs: Vec<MergeInput>,
    maps: Vec<InputMap>,
    max_doc: u32,
    /// Last `source_of` answer `(new doc, input, old doc)`, so sequential calls step instead of
    /// searching.
    last_source: std::cell::Cell<(u32, u32, u32)>,
    fields: Vec<FieldInfo>,
    id: [u8; 16],
    sort: Option<IndexSort>,
    attributes: BTreeMap<String, String>,
    stored: RefCell<Vec<Option<StoredReader>>>,
    spill: SpillOptions,
}

impl MergeSource {
    /// Prepares a merge of `inputs` into a segment with id `id`.
    pub fn new(inputs: Vec<MergeInput>, id: [u8; 16]) -> Result<MergeSource> {
        let mut maps = Vec::with_capacity(inputs.len());
        let mut base = 0u64;
        for inp in &inputs {
            let max_doc = inp.reader.max_doc();
            if let Some(l) = &inp.live
                && l.max_doc() != max_doc
            {
                return Err(invalid("live docs do not match the segment"));
            }
            let m = InputMap::new(base as u32, max_doc, inp.live.clone());
            base += m.live as u64;
            if base > crate::MAX_DOCS as u64 {
                return Err(invalid("merged segment would have too many docs"));
            }
            maps.push(m);
        }
        let fields = merge_field_infos(&inputs)?;
        let sort = {
            let sorts: Vec<Option<IndexSort>> = inputs
                .iter()
                .map(|i| i.reader.info().sort.clone())
                .collect();
            if sorts.windows(2).all(|w| w[0] == w[1]) {
                sorts.first().cloned().flatten()
            } else {
                None
            }
        };
        let stored = RefCell::new((0..inputs.len()).map(|_| None).collect());
        Ok(MergeSource {
            inputs,
            maps,
            max_doc: base as u32,
            last_source: std::cell::Cell::new((u32::MAX, 0, 0)),
            fields,
            id,
            sort,
            attributes: BTreeMap::new(),
            stored,
            spill: SpillOptions::default(),
        })
    }

    /// Where large merge-time arrays spill (ordinal maps of sorted doc values); pass the same
    /// options as [`WriteOptions::spill`](crate::WriteOptions).
    pub fn set_spill(&mut self, spill: SpillOptions) {
        self.spill = spill;
    }

    /// Sets attributes recorded in the merged segment.
    pub fn set_attributes(&mut self, attributes: BTreeMap<String, String>) {
        self.attributes = attributes;
    }

    /// Overrides the index sort (e.g. `None` to keep concatenation order).
    pub fn set_index_sort(&mut self, sort: Option<IndexSort>) {
        self.sort = sort;
    }

    /// New doc id of `(input, old doc)`.
    pub fn map_doc(&self, input: usize, doc: u32) -> Option<u32> {
        let m = self.maps.get(input)?;
        if doc >= m.max_doc {
            return None;
        }
        match m.get(doc) {
            DELETED => None,
            d => Some(d),
        }
    }

    /// Input and old doc of a new doc id (`new_doc < max_doc`).
    pub fn source_of(&self, new_doc: u32) -> (usize, u32) {
        let (last_new, last_input, last_old) = self.last_source.get();
        let i = last_input as usize;
        let answer = if last_new != u32::MAX && new_doc == last_new + 1 && new_doc < self.maps[i].base + self.maps[i].live {
            // Sequential access (stored fields): the next live doc of the same input.
            (i, self.maps[i].next_live(last_old + 1))
        } else {
            let i = self.maps.partition_point(|m| m.base <= new_doc).saturating_sub(1);
            // Skip inputs left empty by deletions that share the base.
            let i = (i..self.maps.len()).find(|&k| new_doc < self.maps[k].base + self.maps[k].live).unwrap_or(i);
            (i, self.maps[i].select(new_doc - self.maps[i].base))
        };
        self.last_source.set((new_doc, answer.0 as u32, answer.1));
        answer
    }

    fn field(&self, number: u32) -> Option<&FieldInfo> {
        self.fields.iter().find(|f| f.number == number)
    }
}

/// Old doc → new doc of one input: its first new doc plus the rank among its live docs, from
/// the live-docs bitset and a running count per 64-doc word (half a bit per doc instead of a
/// `u32` per doc, so huge merges do not hold doc-sized maps).
pub(crate) struct InputMap {
    base: u32,
    max_doc: u32,
    live: u32,
    deletes: Option<(Arc<LiveDocs>, Vec<u32>)>,
}

impl InputMap {
    fn new(base: u32, max_doc: u32, live: Option<Arc<LiveDocs>>) -> InputMap {
        match live {
            Some(l) if l.num_deleted() > 0 => {
                let mut cum = Vec::with_capacity(l.words().len());
                let mut n = 0u32;
                for (w, &word) in l.words().iter().enumerate() {
                    cum.push(n);
                    // Bits past max_doc never count.
                    let valid = (max_doc as u64).saturating_sub(w as u64 * 64).min(64) as u32;
                    let mask = if valid == 64 { u64::MAX } else { (1u64 << valid) - 1 };
                    n += (word & mask).count_ones();
                }
                InputMap { base, max_doc, live: n, deletes: Some((l, cum)) }
            }
            _ => InputMap { base, max_doc, live: max_doc, deletes: None },
        }
    }

    /// New doc of live doc `d`, or `DELETED` (also for `d >= max_doc`, which only corrupt
    /// inputs produce).
    #[inline]
    pub(crate) fn get(&self, d: u32) -> u32 {
        if d >= self.max_doc {
            return DELETED;
        }
        match &self.deletes {
            None => self.base + d,
            Some((l, cum)) => {
                let w = d as usize >> 6;
                let (Some(&word), Some(&c)) = (l.words().get(w), cum.get(w)) else {
                    return DELETED;
                };
                if word >> (d & 63) & 1 == 0 {
                    return DELETED;
                }
                self.base + c + (word & ((1u64 << (d & 63)) - 1)).count_ones()
            }
        }
    }

    /// The `k`-th live doc (`k < live`).
    fn select(&self, k: u32) -> u32 {
        let Some((l, cum)) = &self.deletes else { return k };
        let w = cum.partition_point(|&c| c <= k).saturating_sub(1);
        let mut word = l.words()[w];
        for _ in 0..k - cum[w] {
            word &= word.wrapping_sub(1);
        }
        w as u32 * 64 + word.trailing_zeros()
    }

    /// The first live doc at or after `from`.
    fn next_live(&self, from: u32) -> u32 {
        match &self.deletes {
            None => from,
            Some((l, _)) => l.next_live(from),
        }
    }
}

fn merge_field_infos(inputs: &[MergeInput]) -> Result<Vec<FieldInfo>> {
    let mut by_num: BTreeMap<u32, FieldInfo> = BTreeMap::new();
    for inp in inputs {
        for f in &inp.reader.info().fields {
            match by_num.get_mut(&f.number) {
                None => {
                    by_num.insert(f.number, f.clone());
                }
                Some(m) => {
                    if m.name != f.name {
                        return Err(invalid(format!(
                            "field {} named [{}] and [{}]",
                            f.number, m.name, f.name
                        )));
                    }
                    // Index options: the common denominator every input can provide.
                    if f.index != IndexOptions::None {
                        m.index = if m.index == IndexOptions::None {
                            f.index
                        } else {
                            m.index.min(f.index)
                        };
                    }
                    m.norms |= f.norms;
                    m.payloads &= f.payloads || f.index == IndexOptions::None;
                    if f.doc_values != crate::info::DocValuesType::None {
                        if m.doc_values == crate::info::DocValuesType::None {
                            m.doc_values = f.doc_values;
                        } else if m.doc_values != f.doc_values {
                            return Err(invalid(format!(
                                "field [{}] has conflicting doc values types",
                                f.name
                            )));
                        }
                    }
                    m.doc_values_skip_index |= f.doc_values_skip_index;
                    m.bloom_filter |= f.bloom_filter;
                    m.suggest |= f.suggest;
                    match (&m.points, &f.points) {
                        (None, Some(p)) => m.points = Some(*p),
                        (Some(a), Some(b)) if a != b => {
                            return Err(invalid(format!(
                                "field [{}] has conflicting point shapes",
                                f.name
                            )));
                        }
                        _ => {}
                    }
                    match (&m.vectors, &f.vectors) {
                        (None, Some(v)) => m.vectors = Some(*v),
                        (Some(a), Some(b))
                            if a.dims != b.dims
                                || a.element != b.element
                                || a.similarity != b.similarity =>
                        {
                            return Err(invalid(format!(
                                "field [{}] has conflicting vector shapes",
                                f.name
                            )));
                        }
                        _ => {}
                    }
                    for (k, v) in &f.attributes {
                        m.attributes.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }
        }
    }
    Ok(by_num.into_values().collect())
}

// ---------------------------------------------------------------------------------------------
// Terms
// ---------------------------------------------------------------------------------------------

struct ConcatPostings<'s> {
    parts: Vec<(PostingsCursor, &'s InputMap)>,
    /// Cursors of earlier terms, reset onto the next ones (no allocation per term).
    pool: Vec<PostingsCursor>,
    part: usize,
    peeked: Option<u32>,
}

impl ConcatPostings<'_> {
    fn advance(&mut self) -> u32 {
        while self.part < self.parts.len() {
            let (c, map) = &mut self.parts[self.part];
            loop {
                let d = c.next();
                if d == NO_MORE_DOCS {
                    break;
                }
                let nd = map.get(d);
                if nd != DELETED {
                    return nd;
                }
            }
            self.part += 1;
        }
        NO_MORE_DOCS
    }
}

impl PostingsSource for ConcatPostings<'_> {
    fn next_doc(&mut self) -> u32 {
        match self.peeked.take() {
            Some(d) => d,
            None => self.advance(),
        }
    }
    fn freq(&mut self) -> u32 {
        self.parts.get_mut(self.part).map_or(1, |p| p.0.freq())
    }
    fn next_position(&mut self) -> u32 {
        self.parts
            .get_mut(self.part)
            .map_or(0, |p| p.0.next_position())
    }
    fn offsets(&self) -> (u32, u32) {
        self.parts
            .get(self.part)
            .map_or((0, 0), |p| (p.0.start_offset(), p.0.end_offset()))
    }
    fn payload(&self) -> &[u8] {
        self.parts.get(self.part).map_or(&[], |p| p.0.payload())
    }
}

struct MergedTerms<'s> {
    src: &'s MergeSource,
    iters: Vec<(usize, TermsIter<'s>)>,
    heads: Vec<Option<Vec<u8>>>,
    advance: Vec<usize>,
    term: Vec<u8>,
    flags: PostingsFlags,
    postings: ConcatPostings<'s>,
}

impl TermsSource for MergedTerms<'_> {
    fn next_term(&mut self) -> Result<Option<&[u8]>> {
        loop {
            for &k in &self.advance {
                let (_, it) = &mut self.iters[k];
                let head = &mut self.heads[k];
                match it.next() {
                    Some(t) => {
                        let h = head.get_or_insert_with(Vec::new);
                        h.clear();
                        h.extend_from_slice(t);
                    }
                    None => *head = None,
                }
            }
            self.advance.clear();
            let Some(min_k) = (0..self.heads.len())
                .filter(|&k| self.heads[k].is_some())
                .min_by(|&a, &b| self.heads[a].cmp(&self.heads[b]))
            else {
                return Ok(None);
            };
            self.term.clear();
            self.term.extend_from_slice(self.heads[min_k].as_deref().unwrap_or_default());
            let ConcatPostings { parts, pool, .. } = &mut self.postings;
            pool.extend(parts.drain(..).map(|(c, _)| c));
            for k in 0..self.iters.len() {
                if self.heads[k].as_deref() == Some(self.term.as_slice()) {
                    self.advance.push(k);
                    let (input, it) = &mut self.iters[k];
                    let mut cursor = self.postings.pool.pop().unwrap_or_else(PostingsCursor::empty);
                    it.postings_reset(self.flags, &mut cursor);
                    self.postings.parts.push((cursor, &self.src.maps[*input]));
                }
            }
            self.postings.part = 0;
            self.postings.peeked = None;
            let first = self.postings.advance();
            if first != NO_MORE_DOCS {
                self.postings.peeked = Some(first);
                return Ok(Some(&self.term));
            }
        }
    }
    fn postings(&mut self) -> Result<&mut dyn PostingsSource> {
        Ok(&mut self.postings)
    }
}

// ---------------------------------------------------------------------------------------------
// Doc values
// ---------------------------------------------------------------------------------------------

/// A k-way merge of the inputs' value dictionaries (skipping values no live doc uses), yielding
/// the merged dictionary in order. It runs once to build the ordinal maps and again, lazily, to
/// serve [`SortedSource::value`] while the writer reads the dictionary: merged values are never
/// copied, so a merge of huge keyword dictionaries holds no second dictionary in memory.
struct DictMerge<'s> {
    heads: Vec<Head<'s>>,
    /// The last merged term returned, and how many have been returned.
    cur: Vec<u8>,
    pos: u64,
    /// `(head index, input ordinal)` of the inputs holding the last merged term.
    matched: Vec<(usize, u64)>,
}

struct Head<'s> {
    dict: &'s TermDict,
    it: crate::docvalues::TermDictIter<'s>,
    /// Ordinals used by live docs (bitset); `None` when every doc is live.
    used: Option<Vec<u64>>,
    term: Vec<u8>,
    ord: u64,
    live: bool,
}

impl<'s> Head<'s> {
    fn step(&mut self) {
        loop {
            // The ordinal `next` is about to return.
            let ord = self.it.ord().wrapping_add(1);
            match self.it.next() {
                None => {
                    self.live = false;
                    return;
                }
                Some(t) => {
                    if self.used.as_ref().is_none_or(|u| u.get(ord as usize >> 6).is_some_and(|w| w >> (ord & 63) & 1 != 0)) {
                        self.term.clear();
                        self.term.extend_from_slice(t);
                        self.ord = ord;
                        self.live = true;
                        return;
                    }
                }
            }
        }
    }
}

impl<'s> DictMerge<'s> {
    fn new(dicts: Vec<(&'s TermDict, Option<Vec<u64>>)>) -> DictMerge<'s> {
        let mut m = DictMerge {
            heads: dicts
                .into_iter()
                .map(|(dict, used)| Head { dict, it: dict.iter_from(0), used, term: Vec::new(), ord: 0, live: false })
                .collect(),
            cur: Vec::new(),
            pos: 0,
            matched: Vec::new(),
        };
        m.restart();
        m
    }

    fn restart(&mut self) {
        for h in &mut self.heads {
            h.it = h.dict.iter_from(0);
            h.step();
        }
        self.pos = 0;
        self.cur.clear();
        self.matched.clear();
    }

    /// The next merged term (`self.cur`); false at the end.
    fn next(&mut self) -> bool {
        self.matched.clear();
        let Some(k) = (0..self.heads.len())
            .filter(|&k| self.heads[k].live)
            .min_by(|&a, &b| self.heads[a].term.cmp(&self.heads[b].term))
        else {
            return false;
        };
        self.cur.clear();
        self.cur.extend_from_slice(&self.heads[k].term);
        for (i, h) in self.heads.iter_mut().enumerate() {
            if h.live && h.term == self.cur {
                self.matched.push((i, h.ord));
                h.step();
            }
        }
        self.pos += 1;
        true
    }

    /// The merged term with ordinal `ord` (`ord < count`); sequential calls cost one merge step.
    fn value(&mut self, ord: u64) -> &[u8] {
        if ord + 1 < self.pos {
            self.restart();
        }
        while self.pos <= ord {
            if !self.next() {
                self.cur.clear();
                break;
            }
        }
        &self.cur
    }
}

/// Input ordinal → merged ordinal of one input (`u32` entries unless the merged dictionary could
/// exceed them), spill-backed.
enum OrdMap {
    U32(SpillSlice<u32>),
    U64(SpillSlice<u64>),
}

impl OrdMap {
    #[inline]
    fn get(&self, ord: u64) -> u64 {
        match self {
            OrdMap::U32(m) => m.get(ord as usize).map_or(u64::MAX, |&g| g as u64),
            OrdMap::U64(m) => m.get(ord as usize).copied().unwrap_or(u64::MAX),
        }
    }
}

/// Builds the merged dictionary's ordinal maps; returns them (indexed by input) with the merge
/// state positioned for value reads and the merged value count.
fn merge_dicts<'s>(
    dicts: Vec<(usize, &'s TermDict, Option<Vec<u64>>)>,
    n_inputs: usize,
    spill: &SpillOptions,
) -> Result<(Vec<Option<OrdMap>>, DictMerge<'s>, u64)> {
    let total: u64 = dicts.iter().map(|d| d.1.len()).sum();
    let wide = total >= u32::MAX as u64;
    let inputs: Vec<usize> = dicts.iter().map(|d| d.0).collect();
    let lens: Vec<u64> = dicts.iter().map(|d| d.1.len()).collect();
    let mut dm = DictMerge::new(dicts.into_iter().map(|(_, d, u)| (d, u)).collect());
    enum Building {
        U32(SpillVec<u32>),
        U64(SpillVec<u64>),
    }
    impl Building {
        fn push(&mut self, v: u64) -> Result<()> {
            match self {
                Building::U32(b) => b.push(v as u32),
                Building::U64(b) => b.push(v),
            }
        }
        fn len(&self) -> u64 {
            match self {
                Building::U32(b) => b.len() as u64,
                Building::U64(b) => b.len() as u64,
            }
        }
    }
    let unused = if wide { u64::MAX } else { u32::MAX as u64 };
    let mut building: Vec<Building> = (0..inputs.len())
        .map(|_| if wide { Building::U64(SpillVec::new(spill)) } else { Building::U32(SpillVec::new(spill)) })
        .collect();
    let mut count = 0u64;
    while dm.next() {
        for &(h, ord) in &dm.matched {
            let b = &mut building[h];
            while b.len() < ord {
                b.push(unused)?;
            }
            b.push(count)?;
        }
        count += 1;
    }
    let mut maps: Vec<Option<OrdMap>> = (0..n_inputs).map(|_| None).collect();
    for ((b, input), len) in building.into_iter().zip(inputs).zip(lens) {
        let mut b = b;
        while b.len() < len {
            b.push(unused)?;
        }
        maps[input] = Some(match b {
            Building::U32(v) => OrdMap::U32(v.finish()?),
            Building::U64(v) => OrdMap::U64(v.finish()?),
        });
    }
    dm.restart();
    Ok((maps, dm, count))
}

struct MergedSorted<'s> {
    src: &'s MergeSource,
    maps: Vec<Option<OrdMap>>,
    dict: DictMerge<'s>,
    count: u64,
    readers: Vec<Option<&'s crate::docvalues::SortedDocValues>>,
    input: usize,
    cursor: Option<crate::docset::DocSetCursor>,
}

impl SortedSource for MergedSorted<'_> {
    fn value_count(&self) -> u64 {
        self.count
    }
    fn value(&mut self, ord: u64) -> &[u8] {
        self.dict.value(ord)
    }
    fn next_doc(&mut self) -> Option<(u32, u64)> {
        loop {
            if self.input >= self.readers.len() {
                return None;
            }
            let (Some(r), Some(map)) = (self.readers[self.input], self.maps[self.input].as_ref()) else {
                self.input += 1;
                continue;
            };
            let c = self.cursor.get_or_insert_with(|| r.docs().cursor());
            let d = c.next();
            if d == NO_MORE_DOCS {
                self.cursor = None;
                self.input += 1;
                continue;
            }
            let nd = self.src.maps[self.input].get(d);
            if nd == DELETED {
                continue;
            }
            return Some((nd, map.get(r.ord_at(c.index()))));
        }
    }
}

struct MergedSortedSet<'s> {
    src: &'s MergeSource,
    maps: Vec<Option<OrdMap>>,
    dict: DictMerge<'s>,
    count: u64,
    readers: Vec<Option<&'s crate::docvalues::SortedSetDocValues>>,
    input: usize,
    cursor: Option<crate::docset::DocSetCursor>,
    buf: Vec<u64>,
}

impl SortedSetSource for MergedSortedSet<'_> {
    fn value_count(&self) -> u64 {
        self.count
    }
    fn value(&mut self, ord: u64) -> &[u8] {
        self.dict.value(ord)
    }
    fn next_doc(&mut self) -> Option<(u32, &[u64])> {
        loop {
            if self.input >= self.readers.len() {
                return None;
            }
            let (Some(r), Some(map)) = (self.readers[self.input], self.maps[self.input].as_ref()) else {
                self.input += 1;
                continue;
            };
            let c = self.cursor.get_or_insert_with(|| r.docs().cursor());
            let d = c.next();
            if d == NO_MORE_DOCS {
                self.cursor = None;
                self.input += 1;
                continue;
            }
            let nd = self.src.maps[self.input].get(d);
            if nd == DELETED {
                continue;
            }
            self.buf.clear();
            self.buf.extend(r.ords_at(c.index()).map(|o| map.get(o)));
            return Some((nd, &self.buf));
        }
    }
}

struct MergedSortedNumeric<'s> {
    src: &'s MergeSource,
    readers: Vec<Option<&'s crate::docvalues::SortedNumericDocValues>>,
    input: usize,
    cursor: Option<crate::docset::DocSetCursor>,
    buf: Vec<i64>,
}

impl SortedNumericSource for MergedSortedNumeric<'_> {
    fn next_doc(&mut self) -> Option<(u32, &[i64])> {
        loop {
            if self.input >= self.readers.len() {
                return None;
            }
            let Some(r) = self.readers[self.input] else {
                self.input += 1;
                continue;
            };
            let c = self.cursor.get_or_insert_with(|| r.docs().cursor());
            let d = c.next();
            if d == NO_MORE_DOCS {
                self.cursor = None;
                self.input += 1;
                continue;
            }
            let nd = self.src.maps[self.input].get(d);
            if nd == DELETED {
                continue;
            }
            self.buf.clear();
            self.buf.extend(r.values_at(c.index()));
            return Some((nd, &self.buf));
        }
    }
}

struct MergedBinary<'s> {
    src: &'s MergeSource,
    readers: Vec<Option<&'s crate::docvalues::BinaryDocValues>>,
    input: usize,
    cursor: Option<crate::docset::DocSetCursor>,
}

impl BytesSource for MergedBinary<'_> {
    fn next(&mut self) -> Option<(u32, &[u8])> {
        loop {
            if self.input >= self.readers.len() {
                return None;
            }
            let Some(r) = self.readers[self.input] else {
                self.input += 1;
                continue;
            };
            let c = self.cursor.get_or_insert_with(|| r.docs().cursor());
            let d = c.next();
            if d == NO_MORE_DOCS {
                self.cursor = None;
                self.input += 1;
                continue;
            }
            let nd = self.src.maps[self.input].get(d);
            if nd == DELETED {
                continue;
            }
            let idx = c.index();
            return Some((nd, r.value_at(idx)));
        }
    }
}

struct MergedVectors<'s> {
    src: &'s MergeSource,
    readers: Vec<Option<&'s crate::vectors::VectorsReader>>,
    input: usize,
    ord: u32,
}

impl BytesSource for MergedVectors<'_> {
    fn next(&mut self) -> Option<(u32, &[u8])> {
        loop {
            if self.input >= self.readers.len() {
                return None;
            }
            let Some(r) = self.readers[self.input] else {
                self.input += 1;
                continue;
            };
            if self.ord >= r.len() {
                self.ord = 0;
                self.input += 1;
                continue;
            }
            let ord = self.ord;
            self.ord += 1;
            let nd = self.src.maps[self.input].get(r.doc(ord));
            if nd == DELETED {
                continue;
            }
            return Some((nd, r.vector(ord)));
        }
    }
}

/// Points of every input, streamed leaf by leaf (points may come in any order).
struct MergedPoints<'s> {
    src: &'s MergeSource,
    iters: Vec<(usize, crate::points::PointValuesIter<'s>)>,
    at: usize,
}

impl BytesSource for MergedPoints<'_> {
    fn next(&mut self) -> Option<(u32, &[u8])> {
        // Position on the next live point first, then borrow it (a lending iterator cannot
        // return a value from inside the search loop).
        let new_doc = loop {
            let (input, it) = self.iters.get_mut(self.at)?;
            match it.peek_doc() {
                None => self.at += 1,
                Some(d) => {
                    let m = self.src.maps[*input].get(d);
                    if m != DELETED {
                        break m;
                    }
                    it.skip();
                }
            }
        };
        let (_, v) = self.iters[self.at].1.next()?;
        Some((new_doc, v))
    }
}

/// Suggest entries of every input, renumbered, deleted docs dropped (the writer sorts them).
struct MergedSuggest<'s> {
    src: &'s MergeSource,
    readers: Vec<Option<&'s crate::suggest::SuggestReader>>,
    input: usize,
    keys: Option<crate::docvalues::TermDictIter<'s>>,
}

impl SuggestSource for MergedSuggest<'_> {
    fn next(&mut self) -> Option<SuggestEntry<'_>> {
        loop {
            let r = match self.readers.get(self.input) {
                None => return None,
                Some(None) => {
                    self.input += 1;
                    continue;
                }
                Some(Some(r)) => *r,
            };
            let it = self.keys.get_or_insert_with(|| r.keys().iter_from(0));
            if it.next().is_none() {
                self.keys = None;
                self.input += 1;
                continue;
            }
            let i = it.ord();
            let nd = self.src.maps[self.input].get(r.doc(i));
            if nd == DELETED {
                continue;
            }
            let key = self.keys.as_ref().map_or(&[][..], |it| it.term());
            return Some(SuggestEntry { key, weight: r.weight(i), doc: nd, payload: r.payload(i) });
        }
    }
}

/// Marks the ordinals used by live docs as a bitset (`None` when every doc is live).
fn used_ords<'a>(
    live: Option<&LiveDocs>,
    docs: &crate::docset::DocSet,
    count: u64,
    ords_of: &dyn Fn(u32) -> Box<dyn Iterator<Item = u64> + 'a>,
) -> Option<Vec<u64>> {
    let live = live?;
    let mut used = vec![0u64; (count as usize).div_ceil(64)];
    let mut c = docs.cursor();
    while c.next() != NO_MORE_DOCS {
        if live.is_live(c.doc()) {
            for o in ords_of(c.index()) {
                if let Some(w) = used.get_mut(o as usize >> 6) {
                    *w |= 1 << (o & 63);
                }
            }
        }
    }
    Some(used)
}

impl SegmentSource for MergeSource {
    fn segment_id(&self) -> [u8; 16] {
        self.id
    }
    fn max_doc(&self) -> u32 {
        self.max_doc
    }
    fn field_infos(&self) -> Vec<FieldInfo> {
        self.fields.clone()
    }
    fn index_sort(&self) -> Option<IndexSort> {
        self.sort.clone()
    }
    fn is_sorted(&self) -> bool {
        // Concatenating sorted segments is not sorted (unless there is one input).
        self.sort.is_some() && self.inputs.len() == 1
    }
    fn attributes(&self) -> BTreeMap<String, String> {
        self.attributes.clone()
    }

    fn terms(&self, field: u32) -> Result<Option<Box<dyn TermsSource + '_>>> {
        let Some(info) = self.field(field) else {
            return Ok(None);
        };
        let mut iters = Vec::new();
        for (i, inp) in self.inputs.iter().enumerate() {
            if let Some(t) = inp.reader.terms(field) {
                iters.push((i, t.iter()));
            }
        }
        if iters.is_empty() {
            return Ok(None);
        }
        let mut flags = PostingsFlags::DOCS;
        if info.index.has_freqs() {
            flags = flags | PostingsFlags::FREQS;
        }
        if info.index.has_positions() {
            flags = flags | PostingsFlags::POSITIONS;
        }
        if info.index.has_offsets() {
            flags = flags | PostingsFlags::OFFSETS;
        }
        if info.payloads {
            flags = flags | PostingsFlags::PAYLOADS;
        }
        let n = iters.len();
        Ok(Some(Box::new(MergedTerms {
            src: self,
            iters,
            heads: vec![None; n],
            advance: (0..n).collect(),
            term: Vec::new(),
            flags,
            postings: ConcatPostings {
                parts: Vec::new(),
                pool: Vec::new(),
                part: 0,
                peeked: None,
            },
        })))
    }

    fn norms(&self, field: u32) -> Result<Option<crate::source::NormsIter<'_>>> {
        if !self.inputs.iter().any(|i| i.reader.norms(field).is_some()) {
            return Ok(None);
        }
        let it = self.inputs.iter().enumerate().flat_map(move |(i, inp)| {
            let norms = inp.reader.norms(field);
            let map = &self.maps[i];
            let mut cursor = norms.map(|n| n.docs().cursor());
            std::iter::from_fn(move || {
                let (c, n) = (cursor.as_mut()?, norms?);
                loop {
                    let d = c.next();
                    if d == NO_MORE_DOCS {
                        return None;
                    }
                    let nd = map.get(d);
                    if nd != DELETED {
                        return Some((nd, n.get(d).unwrap_or(0)));
                    }
                }
            })
        });
        Ok(Some(Box::new(it)))
    }

    fn doc_values(&self, field: u32) -> Result<Option<DocValuesSource<'_>>> {
        let readers: Vec<Option<&DocValuesReader>> = self
            .inputs
            .iter()
            .map(|i| i.reader.doc_values(field))
            .collect();
        if readers.iter().all(|r| r.is_none()) {
            return Ok(None);
        }
        let Some(info) = self.field(field) else {
            return Ok(None);
        };
        use crate::info::DocValuesType as T;
        Ok(Some(match info.doc_values {
            T::Numeric => {
                let it = self.inputs.iter().enumerate().flat_map(move |(i, inp)| {
                    let dv = inp.reader.numeric(field);
                    let map = &self.maps[i];
                    let mut cursor = dv.map(|n| n.cursor());
                    std::iter::from_fn(move || {
                        let (c, n) = (cursor.as_mut()?, dv?);
                        loop {
                            let d = c.next();
                            if d == NO_MORE_DOCS {
                                return None;
                            }
                            let nd = map.get(d);
                            if nd != DELETED {
                                return Some((nd, n.value_at(c.index())));
                            }
                        }
                    })
                });
                DocValuesSource::Numeric(Box::new(it))
            }
            T::Binary => DocValuesSource::Binary(Box::new(MergedBinary {
                src: self,
                readers: self.inputs.iter().map(|i| i.reader.binary(field)).collect(),
                input: 0,
                cursor: None,
            })),
            T::Sorted => {
                let readers: Vec<_> = self.inputs.iter().map(|i| i.reader.sorted(field)).collect();
                let mut dicts = Vec::new();
                for (i, r) in readers.iter().enumerate() {
                    if let Some(r) = r {
                        let used = used_ords(
                            self.inputs[i].live.as_deref(),
                            r.docs(),
                            r.value_count(),
                            &|idx| Box::new(std::iter::once(r.ord_at(idx))),
                        );
                        dicts.push((i, r.dict(), used));
                    }
                }
                let (maps, dict, count) = merge_dicts(dicts, readers.len(), &self.spill)?;
                DocValuesSource::Sorted(Box::new(MergedSorted {
                    src: self,
                    maps,
                    dict,
                    count,
                    readers,
                    input: 0,
                    cursor: None,
                }))
            }
            T::SortedSet => {
                let readers: Vec<_> = self
                    .inputs
                    .iter()
                    .map(|i| i.reader.sorted_set(field))
                    .collect();
                let mut dicts = Vec::new();
                for (i, r) in readers.iter().enumerate() {
                    if let Some(r) = r {
                        let used = used_ords(
                            self.inputs[i].live.as_deref(),
                            r.docs(),
                            r.value_count(),
                            &|idx| Box::new(r.ords_at(idx)),
                        );
                        dicts.push((i, r.dict(), used));
                    }
                }
                let (maps, dict, count) = merge_dicts(dicts, readers.len(), &self.spill)?;
                DocValuesSource::SortedSet(Box::new(MergedSortedSet {
                    src: self,
                    maps,
                    dict,
                    count,
                    readers,
                    input: 0,
                    cursor: None,
                    buf: Vec::new(),
                }))
            }
            T::SortedNumeric => DocValuesSource::SortedNumeric(Box::new(MergedSortedNumeric {
                src: self,
                readers: self
                    .inputs
                    .iter()
                    .map(|i| i.reader.sorted_numeric(field))
                    .collect(),
                input: 0,
                cursor: None,
                buf: Vec::new(),
            })),
            T::None => return Ok(None),
        }))
    }

    fn points(&self, field: u32) -> Result<Option<Box<dyn BytesSource + '_>>> {
        if !self.inputs.iter().any(|i| i.reader.points(field).is_some()) {
            return Ok(None);
        }
        let iters = self
            .inputs
            .iter()
            .enumerate()
            .filter_map(|(i, inp)| inp.reader.points(field).map(|p| (i, p.values_iter())))
            .collect();
        Ok(Some(Box::new(MergedPoints { src: self, iters, at: 0 })))
    }

    fn vectors(&self, field: u32) -> Result<Option<Box<dyn BytesSource + '_>>> {
        let readers: Vec<_> = self
            .inputs
            .iter()
            .map(|i| i.reader.vectors(field))
            .collect();
        if readers.iter().all(|r| r.is_none()) {
            return Ok(None);
        }
        Ok(Some(Box::new(MergedVectors {
            src: self,
            readers,
            input: 0,
            ord: 0,
        })))
    }

    fn suggest(&self, field: u32) -> Result<Option<Box<dyn SuggestSource + '_>>> {
        let readers: Vec<_> = self.inputs.iter().map(|i| i.reader.suggest(field)).collect();
        if readers.iter().all(|r| r.is_none()) {
            return Ok(None);
        }
        Ok(Some(Box::new(MergedSuggest { src: self, readers, input: 0, keys: None })))
    }

    fn vector_graph_seed(&self, field: u32) -> Option<(crate::vectors::VectorsReader, Vec<u32>)> {
        // The largest input whose vectors all survive (Lucene's incremental merge).
        let (i, r) = self
            .inputs
            .iter()
            .enumerate()
            .filter_map(|(i, inp)| inp.reader.vectors(field).map(|r| (i, r)))
            .filter(|(_, r)| r.has_graph() && !r.is_empty())
            .filter(|(i, r)| {
                let map = &self.maps[*i];
                (0..r.len()).all(|o| map.get(r.doc(o)) != DELETED)
            })
            .max_by_key(|(i, r)| (r.len(), std::cmp::Reverse(*i)))?;
        let map = &self.maps[i];
        let docs = (0..r.len()).map(|o| map.get(r.doc(o))).collect();
        Some((r.clone(), docs))
    }

    fn stored(&self, doc: u32, out: &mut Vec<u8>) -> Result<()> {
        if doc >= self.max_doc {
            return Err(invalid(format!("doc {doc} >= max_doc {}", self.max_doc)));
        }
        let (i, old) = self.source_of(doc);
        let mut readers = self.stored.borrow_mut();
        let r = readers[i].get_or_insert_with(|| self.inputs[i].reader.stored());
        out.extend_from_slice(r.raw(old)?);
        Ok(())
    }
}
