//! Doc values: column-stride per-document values for sorting, aggregations and scripts.
//!
//! Five types, as in Lucene: numeric (one `i64`), binary (one byte string), sorted (one byte
//! string from a per-segment dictionary, addressed by ordinal), sorted set (a sorted, deduplicated
//! set of ordinals) and sorted numeric (a sorted list of `i64`s, duplicates kept).
//!
//! Every section starts with the docs that have a value as a [`DocSet`]; values are addressed by
//! the doc's rank in that set, so dense fields index values by doc id directly and sparse fields
//! pay one rank lookup. Values use the encodings of [`crate::packed`]; multi-valued types add a
//! monotonic address array (`addresses[i]..addresses[i + 1]` are doc `i`'s values) that is
//! omitted when every doc has exactly one value.
//!
//! Dictionaries ([`TermDict`]) store sorted unique terms in blocks of 16: the first term of a
//! block in full, the rest as (shared prefix length, suffix). `ord → term` decodes at most 15
//! entries (about 100 ns for a random ordinal); `term → ord` binary-searches the blocks' first
//! terms then scans one block.
//!
//! Numeric, sorted, sorted-set and sorted-numeric fields also carry a skip index
//! ([`DocValuesSkipper`]): per 4 096 docs-with-values the doc range, doc count and min/max
//! value (ordinal), with coarser levels aggregating 8, 64, ... intervals. Range filters use it to
//! skip intervals that cannot match and to accept intervals that all match without reading
//! values.

use crate::NO_MORE_DOCS;
use crate::automaton::Automaton;
use crate::docset::{DocSet, DocSetCursor, write_docset};
use crate::error::{Result, corrupt};
use crate::io::{In, Out};
use crate::packed::{MonotonicValues, NumericValues, write_monotonic, write_monotonic_iter, write_numeric_with};
use crate::slice::Slice;

const T_NUMERIC: u8 = 1;
const T_BINARY: u8 = 2;
const T_SORTED: u8 = 3;
const T_SORTED_SET: u8 = 4;
const T_SORTED_NUMERIC: u8 = 5;

const DICT_BLOCK_SHIFT: u32 = 4;
const DICT_BLOCK: u64 = 1 << DICT_BLOCK_SHIFT;

/// A growable list of byte strings in one buffer (no per-item allocation).
#[derive(Clone, Debug, Default)]
pub struct ByteList {
    offsets: Vec<u64>,
    data: Vec<u8>,
}

impl ByteList {
    pub fn new() -> ByteList {
        ByteList {
            offsets: vec![0],
            data: Vec::new(),
        }
    }
    pub fn push(&mut self, b: &[u8]) {
        if self.offsets.is_empty() {
            self.offsets.push(0);
        }
        self.data.extend_from_slice(b);
        self.offsets.push(self.data.len() as u64);
    }
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn get(&self, i: usize) -> &[u8] {
        &self.data[self.offsets[i] as usize..self.offsets[i + 1] as usize]
    }
    pub fn clear(&mut self) {
        self.offsets.clear();
        self.offsets.push(0);
        self.data.clear();
    }
    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        (0..self.len()).map(move |i| self.get(i))
    }
}

// ---------------------------------------------------------------------------------------------
// Term dictionary of sorted / sorted-set values
// ---------------------------------------------------------------------------------------------

/// Sorted, unique terms addressed by ordinal: what the dictionary writer reads (twice: sizes,
/// then bytes), so a merge's dictionary streams from its inputs instead of being copied.
pub(crate) trait TermSeq {
    fn count(&self) -> u64;
    fn term(&mut self, ord: u64) -> &[u8];
}

impl TermSeq for ByteList {
    fn count(&self) -> u64 {
        self.len() as u64
    }
    fn term(&mut self, ord: u64) -> &[u8] {
        self.get(ord as usize)
    }
}

/// Writes a [`TermSeq`] as a [`TermDict`]. Fails with the first ordinal whose term is not
/// strictly greater than the previous one (nothing useful is written then).
pub(crate) fn write_term_dict<O: Out + ?Sized>(terms: &mut dyn TermSeq, out: &mut O) -> std::result::Result<(), u64> {
    write_term_list(terms, true, out)
}

/// [`write_term_dict`], optionally allowing equal neighbours (suggest keys repeat).
pub(crate) fn write_term_list<O: Out + ?Sized>(
    terms: &mut dyn TermSeq,
    strict: bool,
    out: &mut O,
) -> std::result::Result<(), u64> {
    let count = terms.count();
    // Pass 1: validate and size, recording each block's data offset.
    let mut offsets = Vec::with_capacity(count.div_ceil(DICT_BLOCK) as usize);
    let mut prev: Vec<u8> = Vec::new();
    let mut max_len = 0usize;
    let mut data_len = 0u64;
    for ord in 0..count {
        let t = terms.term(ord);
        if ord > 0 && (prev.as_slice() > t || (strict && prev.as_slice() == t)) {
            return Err(ord);
        }
        if ord.is_multiple_of(DICT_BLOCK) {
            offsets.push(data_len);
            data_len += (crate::io::vlen(t.len() as u64) + t.len()) as u64;
        } else {
            let p = common_prefix(&prev, t);
            let suffix = t.len() - p;
            data_len += (crate::io::vlen(p as u64) + crate::io::vlen(suffix as u64) + suffix) as u64;
        }
        max_len = max_len.max(t.len());
        prev.clear();
        prev.extend_from_slice(t);
    }
    out.put_vu64(count);
    out.put_vu32(max_len as u32);
    out.put_vu64(data_len);
    for o in &offsets {
        out.put_u64(*o);
    }
    // Pass 2: the entries.
    prev.clear();
    for ord in 0..count {
        let t = terms.term(ord);
        if ord.is_multiple_of(DICT_BLOCK) {
            out.put_lbytes(t);
        } else {
            let p = common_prefix(&prev, t);
            out.put_vu32(p as u32);
            out.put_lbytes(&t[p..]);
        }
        prev.clear();
        prev.extend_from_slice(t);
    }
    Ok(())
}

#[inline]
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// The value dictionary of a sorted or sorted-set field. Cheap to clone.
#[derive(Clone, Debug)]
pub struct TermDict {
    count: u64,
    max_len: usize,
    blocks: Slice,
    data: Slice,
}

impl TermDict {
    pub(crate) fn read(buf: &Slice, pos: &mut usize) -> Result<TermDict> {
        let mut r = In::at(buf, *pos);
        let count = r.vu64()?;
        let max_len = r.vu32()? as usize;
        let data_len = r.vu64()?;
        let nblocks = count.div_ceil(DICT_BLOCK);
        if nblocks > r.remaining() as u64 / 8 {
            return Err(corrupt("term dictionary truncated"));
        }
        let blocks = buf
            .try_slice(r.pos() as u64, nblocks * 8)
            .ok_or_else(|| corrupt("term dict"))?;
        r.skip(nblocks as usize * 8)?;
        let data = buf
            .try_slice(r.pos() as u64, data_len)
            .ok_or_else(|| corrupt("term dict data"))?;
        r.skip(data_len as usize)?;
        for b in 0..nblocks as usize {
            let o = u64::from_le_bytes(blocks[b * 8..b * 8 + 8].try_into().unwrap());
            if o >= data_len {
                return Err(corrupt("term dict block offset out of range"));
            }
        }
        *pos = r.pos();
        Ok(TermDict {
            count,
            max_len,
            blocks,
            data,
        })
    }

    /// Number of terms (the value count).
    pub fn len(&self) -> u64 {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Length of the longest term.
    pub fn max_term_len(&self) -> usize {
        self.max_len
    }

    fn block_offset(&self, b: u64) -> usize {
        let at = b as usize * 8;
        u64::from_le_bytes(self.blocks[at..at + 8].try_into().unwrap()) as usize
    }

    fn block_first(&self, b: u64) -> (&[u8], usize) {
        let mut r = In::at(&self.data, self.block_offset(b));
        let t = r.lbytes().unwrap_or(&[]);
        (t, r.pos())
    }

    /// Decodes the term with ordinal `ord` into `out`.
    pub fn term(&self, ord: u64, out: &mut Vec<u8>) {
        out.clear();
        if ord >= self.count {
            return;
        }
        let b = ord >> DICT_BLOCK_SHIFT;
        let (first, mut p) = self.block_first(b);
        out.extend_from_slice(first);
        for _ in 0..(ord & (DICT_BLOCK - 1)) {
            next_entry(&self.data, &mut p, out);
        }
    }

    /// `Ok(ord)` if `term` is present, else `Err(insertion point)` (the ordinal of the first
    /// larger term, possibly `len()`).
    pub fn seek(&self, term: &[u8]) -> std::result::Result<u64, u64> {
        let nblocks = self.count.div_ceil(DICT_BLOCK);
        if nblocks == 0 {
            return Err(0);
        }
        // Last block whose first term is <= term.
        let (mut lo, mut hi) = (0u64, nblocks);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.block_first(mid).0 <= term {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            return Err(0);
        }
        let b = lo - 1;
        let (first, mut p) = self.block_first(b);
        let mut cur = first.to_vec();
        let mut ord = b << DICT_BLOCK_SHIFT;
        let end = ((b + 1) << DICT_BLOCK_SHIFT).min(self.count);
        loop {
            match cur.as_slice().cmp(term) {
                std::cmp::Ordering::Equal => return Ok(ord),
                std::cmp::Ordering::Greater => return Err(ord),
                std::cmp::Ordering::Less => {}
            }
            ord += 1;
            if ord >= end {
                return Err(ord);
            }
            next_entry(&self.data, &mut p, &mut cur);
        }
    }

    /// The first ordinal whose term is `>= key` (`len()` if none); also correct when equal terms
    /// repeat (suggest keys).
    pub fn lower_bound(&self, key: &[u8]) -> u64 {
        let nblocks = self.count.div_ceil(DICT_BLOCK);
        // Last block whose first term is < key: the answer is in it or starts the next one.
        let (mut lo, mut hi) = (0u64, nblocks);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.block_first(mid).0 < key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            return 0;
        }
        let b = lo - 1;
        let (first, mut p) = self.block_first(b);
        let mut cur = first.to_vec();
        let mut ord = b << DICT_BLOCK_SHIFT;
        let end = ((b + 1) << DICT_BLOCK_SHIFT).min(self.count);
        loop {
            if cur.as_slice() >= key {
                return ord;
            }
            ord += 1;
            if ord >= end {
                return ord;
            }
            next_entry(&self.data, &mut p, &mut cur);
        }
    }

    /// Iterates terms in order starting at ordinal `ord`.
    pub fn iter_from(&self, ord: u64) -> TermDictIter<'_> {
        TermDictIter {
            dict: self,
            next_ord: ord,
            pos: 0,
            cur: Vec::new(),
            shared: 0,
            primed: false,
        }
    }
}

/// Decodes one prefix-coded entry into `cur`; returns how many leading bytes it kept.
#[inline]
fn next_entry(data: &[u8], p: &mut usize, cur: &mut Vec<u8>) -> usize {
    let mut r = In::at(data, *p);
    let prefix = (r.vu32().unwrap_or(0) as usize).min(cur.len());
    let suffix = r.lbytes().unwrap_or(&[]);
    cur.truncate(prefix);
    cur.extend_from_slice(suffix);
    *p = r.pos();
    prefix
}

/// Ordered iterator over a [`TermDict`].
pub struct TermDictIter<'a> {
    dict: &'a TermDict,
    next_ord: u64,
    pos: usize,
    cur: Vec<u8>,
    /// Bytes the current term shares with the previous one returned (0 after a reposition).
    shared: usize,
    primed: bool,
}

impl TermDictIter<'_> {
    /// The next term, or `None` at the end.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<&[u8]> {
        let ord = self.next_ord;
        if ord >= self.dict.count {
            return None;
        }
        let in_block = ord & (DICT_BLOCK - 1);
        if !self.primed {
            let (first, mut p) = self.dict.block_first(ord >> DICT_BLOCK_SHIFT);
            self.cur.clear();
            self.cur.extend_from_slice(first);
            for _ in 0..in_block {
                next_entry(&self.dict.data, &mut p, &mut self.cur);
            }
            self.pos = p;
            self.shared = 0;
            self.primed = true;
        } else if in_block == 0 {
            let (first, p) = self.dict.block_first(ord >> DICT_BLOCK_SHIFT);
            self.shared = self.cur.iter().zip(first).take_while(|(a, b)| a == b).count();
            self.cur.truncate(self.shared);
            self.cur.extend_from_slice(&first[self.shared..]);
            self.pos = p;
        } else {
            self.shared = next_entry(&self.dict.data, &mut self.pos, &mut self.cur);
        }
        self.next_ord += 1;
        Some(&self.cur)
    }

    /// Ordinal of the term last returned by `next`.
    pub fn ord(&self) -> u64 {
        self.next_ord.wrapping_sub(1)
    }

    /// The term last returned by `next`.
    pub fn term(&self) -> &[u8] {
        &self.cur
    }

    /// Bytes the term last returned shares with the one before it (0 after a reposition).
    pub(crate) fn shared(&self) -> usize {
        self.shared
    }
}

impl TermDict {
    /// Terms accepted by `aut`, in order (terms aggregations' `include`/`exclude` patterns,
    /// keyword `regexp`/`wildcard` over doc values). The walk reuses the automaton states of the
    /// prefix shared with the previous term, and when a prefix leads to a dead state it seeks
    /// past every term starting with that prefix instead of decoding them one by one.
    pub fn intersect<'a>(&'a self, aut: &'a dyn Automaton) -> TermDictIntersect<'a> {
        // Start at the automaton's literal prefix (`foo` of `foo*`).
        let lit = crate::automaton::literal_prefix(aut);
        let first = if lit.is_empty() { 0 } else { self.seek(&lit).unwrap_or_else(|e| e) };
        TermDictIntersect {
            it: self.iter_from(first),
            aut,
            states: vec![aut.start()],
            dead_runs: 0,
            skip: Vec::new(),
            done: self.is_empty() || !aut.can_match(aut.start()),
        }
    }
}

/// Iterator returned by [`TermDict::intersect`].
pub struct TermDictIntersect<'a> {
    it: TermDictIter<'a>,
    aut: &'a dyn Automaton,
    /// `states[i]` is the state after the first `i` bytes of the current term; only states of
    /// the prefix shared with the next term survive.
    states: Vec<u32>,
    dead_runs: u32,
    skip: Vec<u8>,
    done: bool,
}

impl TermDictIntersect<'_> {
    /// The ordinal of the next accepted term.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<u64> {
        while !self.done {
            if self.it.next().is_none() {
                self.done = true;
                break;
            }
            let term = &self.it.cur;
            self.states.truncate(self.it.shared.min(self.states.len() - 1) + 1);
            // Extend the states over the rest of the term, stopping at a dead state or one from
            // which everything matches.
            let mut dead = None;
            let mut always = false;
            loop {
                let depth = self.states.len() - 1;
                let s = self.states[depth];
                if !self.aut.can_match(s) {
                    dead = Some(depth);
                    break;
                }
                if self.aut.will_always_match(s) {
                    always = true;
                    break;
                }
                if depth == term.len() {
                    break;
                }
                self.states.push(self.aut.accept(s, term[depth]));
            }
            let depth = self.states.len() - 1;
            if always || (dead.is_none() && self.aut.is_match(self.states[depth])) {
                self.dead_runs = 0;
                return Some(self.it.ord());
            }
            // `states[dead]` is the dead state: every term starting with term[..dead] is
            // rejected. After a few cheap sequential steps, seek to the next string the
            // automaton could still accept; when there is none, no later term can match.
            let Some(dead) = dead else { continue };
            if dead == 0 {
                self.done = true;
                break;
            }
            self.dead_runs += 1;
            if self.dead_runs >= 4 {
                self.dead_runs = 0;
                let term = &self.it.cur;
                let live = &self.states[..dead];
                if !crate::automaton::next_candidate(self.aut, live, term, dead - 1, &mut self.skip) {
                    self.done = true;
                    break;
                }
                let ord = self.it.dict.seek(&self.skip).unwrap_or_else(|e| e);
                if ord > self.it.next_ord {
                    let dict = self.it.dict;
                    self.it = dict.iter_from(ord);
                    self.states.truncate(1);
                }
            }
        }
        None
    }

    /// The term of the ordinal last returned by [`next`](Self::next).
    pub fn term(&self) -> &[u8] {
        &self.it.cur
    }
}

// ---------------------------------------------------------------------------------------------
// Skip index
// ---------------------------------------------------------------------------------------------

/// Docs-with-values per level-0 interval.
pub const SKIP_INTERVAL: usize = 4096;
const SKIP_ENTRY: usize = 28;
const SKIP_FANOUT_SHIFT: u32 = 3;

#[derive(Clone, Copy, Debug)]
struct SkipEntry {
    min_doc: u32,
    max_doc: u32,
    count: u32,
    min: i64,
    max: i64,
}

fn put_skip<O: Out + ?Sized>(out: &mut O, e: &SkipEntry) {
    out.put_u32(e.min_doc);
    out.put_u32(e.max_doc);
    out.put_u32(e.count);
    out.put_i64(e.min);
    out.put_i64(e.max);
}

/// Writes a skip index for docs (ascending) whose per-doc value range is `range(i)`.
pub(crate) fn write_skipper<O: Out + ?Sized>(docs: &[u32], range: impl Fn(usize) -> (i64, i64), out: &mut O) {
    let mut level: Vec<SkipEntry> = Vec::new();
    for (c, chunk) in docs.chunks(SKIP_INTERVAL).enumerate() {
        let base = c * SKIP_INTERVAL;
        let (mut min, mut max) = (i64::MAX, i64::MIN);
        for i in 0..chunk.len() {
            let (a, b) = range(base + i);
            min = min.min(a);
            max = max.max(b);
        }
        level.push(SkipEntry {
            min_doc: chunk[0],
            max_doc: *chunk.last().unwrap(),
            count: chunk.len() as u32,
            min,
            max,
        });
    }
    let mut levels = vec![level];
    while levels.last().unwrap().len() > 1 && levels.len() < 8 {
        let prev = levels.last().unwrap();
        let next: Vec<SkipEntry> = prev
            .chunks(1 << SKIP_FANOUT_SHIFT)
            .map(|g| SkipEntry {
                min_doc: g[0].min_doc,
                max_doc: g.last().unwrap().max_doc,
                count: g.iter().map(|e| e.count).sum(),
                min: g.iter().map(|e| e.min).min().unwrap(),
                max: g.iter().map(|e| e.max).max().unwrap(),
            })
            .collect();
        levels.push(next);
    }
    if levels[0].is_empty() {
        out.put_u8(0);
        return;
    }
    out.put_u8(levels.len() as u8);
    for l in &levels {
        out.put_vu32(l.len() as u32);
    }
    for l in &levels {
        for e in l {
            put_skip(out, e);
        }
    }
}

#[derive(Clone, Debug)]
struct SkipIndex {
    data: Slice,
    /// (byte offset, entry count) per level.
    levels: Vec<(usize, u32)>,
}

fn read_skip_index(buf: &Slice, pos: &mut usize) -> Result<Option<SkipIndex>> {
    let mut r = In::at(buf, *pos);
    let n = r.u8()? as usize;
    if n == 0 {
        *pos = r.pos();
        return Ok(None);
    }
    if n > 8 {
        return Err(corrupt("bad skip index level count"));
    }
    let mut counts = Vec::with_capacity(n);
    for _ in 0..n {
        counts.push(r.vu32()?);
    }
    let total: usize = counts.iter().map(|&c| c as usize).sum();
    if total > r.remaining() / SKIP_ENTRY {
        return Err(corrupt("skip index truncated"));
    }
    let data = buf
        .try_slice(r.pos() as u64, (total * SKIP_ENTRY) as u64)
        .ok_or_else(|| corrupt("skip index"))?;
    r.skip(total * SKIP_ENTRY)?;
    let mut levels = Vec::with_capacity(n);
    let mut off = 0usize;
    for c in counts {
        levels.push((off, c));
        off += c as usize * SKIP_ENTRY;
    }
    *pos = r.pos();
    Ok(Some(SkipIndex { data, levels }))
}

impl SkipIndex {
    fn entry(&self, level: usize, i: u32) -> SkipEntry {
        let (off, _) = self.levels[level];
        let at = off + i as usize * SKIP_ENTRY;
        let e = &self.data[at..at + SKIP_ENTRY];
        SkipEntry {
            min_doc: u32::from_le_bytes(e[0..4].try_into().unwrap()),
            max_doc: u32::from_le_bytes(e[4..8].try_into().unwrap()),
            count: u32::from_le_bytes(e[8..12].try_into().unwrap()),
            min: i64::from_le_bytes(e[12..20].try_into().unwrap()),
            max: i64::from_le_bytes(e[20..28].try_into().unwrap()),
        }
    }
}

/// Cursor over a doc values skip index (Lucene's `DocValuesSkipper`).
///
/// After [`advance(target)`](Self::advance), level 0 is the first interval whose last doc is
/// `>= target`; level `k` is the group of `8^k` level-0 intervals containing it. Doc ids are
/// exact: `min_doc_id`/`max_doc_id` are the first and last docs with a value in the interval,
/// so `doc_count(l) == max_doc_id(l) - min_doc_id(l) + 1` means every doc in between has a value.
#[derive(Clone, Debug)]
pub struct DocValuesSkipper {
    index: SkipIndex,
    cur: [Option<SkipEntry>; 8],
    idx0: u32,
}

impl DocValuesSkipper {
    fn new(index: SkipIndex) -> DocValuesSkipper {
        DocValuesSkipper {
            index,
            cur: [None; 8],
            idx0: u32::MAX,
        }
    }

    /// Positions on the first interval containing a doc `>= target`.
    pub fn advance(&mut self, target: u32) {
        let (_, n0) = self.index.levels[0];
        // Binary search for the first level-0 entry with max_doc >= target.
        let (mut lo, mut hi) = (if self.idx0 == u32::MAX { 0 } else { self.idx0 }, n0);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.index.entry(0, mid).max_doc < target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        self.idx0 = lo;
        for l in 0..8 {
            self.cur[l] = if l < self.index.levels.len() && lo < n0 {
                let i = lo >> (SKIP_FANOUT_SHIFT * l as u32);
                (i < self.index.levels[l].1).then(|| self.index.entry(l, i))
            } else {
                None
            };
        }
    }

    /// Number of levels available at the current position (0 when exhausted).
    pub fn num_levels(&self) -> usize {
        self.cur.iter().take_while(|e| e.is_some()).count()
    }

    /// First doc with a value in the level's interval; [`NO_MORE_DOCS`] when exhausted.
    pub fn min_doc_id(&self, level: usize) -> u32 {
        self.cur[level].map_or(NO_MORE_DOCS, |e| e.min_doc)
    }

    pub fn max_doc_id(&self, level: usize) -> u32 {
        self.cur[level].map_or(NO_MORE_DOCS, |e| e.max_doc)
    }

    pub fn min_value(&self, level: usize) -> i64 {
        self.cur[level].map_or(i64::MAX, |e| e.min)
    }

    pub fn max_value(&self, level: usize) -> i64 {
        self.cur[level].map_or(i64::MIN, |e| e.max)
    }

    pub fn doc_count(&self, level: usize) -> u32 {
        self.cur[level].map_or(0, |e| e.count)
    }

    fn top(&self) -> SkipEntry {
        let l = self.index.levels.len() - 1;
        self.index.entry(l, 0)
    }

    /// Smallest value of the whole field.
    pub fn global_min_value(&self) -> i64 {
        self.top().min
    }

    pub fn global_max_value(&self) -> i64 {
        self.top().max
    }

    pub fn global_doc_count(&self) -> u32 {
        self.top().count
    }
}

// ---------------------------------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
pub(crate) fn write_numeric_dv(docs: &[u32], values: &[i64], max_doc: u32, out: &mut Vec<u8>) {
    write_numeric_dv_with(docs, |i| values[i], max_doc, out);
}

/// Numeric doc values of `docs` (ascending); `value(i)` is the value of `docs[i]`.
pub(crate) fn write_numeric_dv_with<O: Out + ?Sized>(docs: &[u32], value: impl Fn(usize) -> i64, max_doc: u32, out: &mut O) {
    out.put_u8(T_NUMERIC);
    write_docset(docs, max_doc, out);
    write_numeric_with(docs.len(), &value, out);
    write_skipper(docs, |i| (value(i), value(i)), out);
}

#[cfg(test)]
pub(crate) fn write_binary_dv(docs: &[u32], values: &ByteList, max_doc: u32, out: &mut Vec<u8>) {
    write_binary_dv_with(docs, |i| values.get(i), max_doc, out);
}

/// Binary doc values of `docs` (ascending); `value(i)` is the value of `docs[i]`.
pub(crate) fn write_binary_dv_with<'v, O: Out + ?Sized>(
    docs: &[u32],
    value: impl Fn(usize) -> &'v [u8],
    max_doc: u32,
    out: &mut O,
) {
    out.put_u8(T_BINARY);
    write_docset(docs, max_doc, out);
    let n = docs.len();
    let (mut min, mut max, mut total) = (usize::MAX, 0usize, 0u64);
    for i in 0..n {
        let l = value(i).len();
        min = min.min(l);
        max = max.max(l);
        total += l as u64;
    }
    let min = if n == 0 { 0 } else { min };
    out.put_vu32(min as u32);
    out.put_vu32(max as u32);
    if min != max {
        let offsets = || {
            std::iter::once(0).chain((0..n).scan(0u64, |at, i| {
                *at += value(i).len() as u64;
                Some(*at)
            }))
        };
        write_monotonic_iter(n + 1, offsets, out);
    }
    out.put_vu64(total);
    for i in 0..n {
        out.put_bytes(value(i));
    }
}

#[cfg(test)]
pub(crate) fn write_sorted_dv(docs: &[u32], ords: &[u64], dict: &ByteList, max_doc: u32, out: &mut Vec<u8>) {
    write_sorted_dv_with(docs, |i| ords[i], &mut dict.clone(), max_doc, out).unwrap();
}

/// Sorted doc values of `docs` (ascending); `ord(i)` is the ordinal of `docs[i]`. Fails like
/// [`write_term_dict`].
pub(crate) fn write_sorted_dv_with<O: Out + ?Sized>(
    docs: &[u32],
    ord: impl Fn(usize) -> u64,
    dict: &mut dyn TermSeq,
    max_doc: u32,
    out: &mut O,
) -> std::result::Result<(), u64> {
    out.put_u8(T_SORTED);
    write_docset(docs, max_doc, out);
    write_numeric_with(docs.len(), |i| ord(i) as i64, out);
    write_term_dict(dict, out)?;
    write_skipper(docs, |i| (ord(i) as i64, ord(i) as i64), out);
    Ok(())
}

#[cfg(test)]
pub(crate) fn write_sorted_set_dv(
    docs: &[u32],
    addresses: &[u64],
    ords: &[u64],
    dict: &ByteList,
    max_doc: u32,
    out: &mut Vec<u8>,
) {
    write_sorted_set_dv_with(docs, addresses, ords, &mut dict.clone(), max_doc, out).unwrap();
}

/// `addresses` has `docs.len() + 1` entries; doc `i`'s ords are `ords[addresses[i]..addresses[i+1]]`
/// (sorted, unique). Fails like [`write_term_dict`].
pub(crate) fn write_sorted_set_dv_with<O: Out + ?Sized>(
    docs: &[u32],
    addresses: &[u64],
    ords: &[u64],
    dict: &mut dyn TermSeq,
    max_doc: u32,
    out: &mut O,
) -> std::result::Result<(), u64> {
    out.put_u8(T_SORTED_SET);
    write_docset(docs, max_doc, out);
    let singleton = ords.len() == docs.len();
    out.put_u8(singleton as u8);
    if !singleton {
        write_monotonic(addresses, out);
    }
    write_numeric_with(ords.len(), |i| ords[i] as i64, out);
    write_term_dict(dict, out)?;
    write_skipper(
        docs,
        |i| (ords[addresses[i] as usize] as i64, ords[addresses[i + 1] as usize - 1] as i64),
        out,
    );
    Ok(())
}

pub(crate) fn write_sorted_numeric_dv<O: Out + ?Sized>(
    docs: &[u32],
    addresses: &[u64],
    values: &[i64],
    max_doc: u32,
    out: &mut O,
) {
    out.put_u8(T_SORTED_NUMERIC);
    write_docset(docs, max_doc, out);
    let singleton = values.len() == docs.len();
    out.put_u8(singleton as u8);
    if !singleton {
        write_monotonic(addresses, out);
    }
    write_numeric_with(values.len(), |i| values[i], out);
    write_skipper(
        docs,
        |i| {
            (
                values[addresses[i] as usize],
                values[addresses[i + 1] as usize - 1],
            )
        },
        out,
    );
}

// ---------------------------------------------------------------------------------------------
// Readers
// ---------------------------------------------------------------------------------------------

fn open_head(data: &Slice, want: u8, max_doc: u32) -> Result<(DocSet, usize)> {
    let mut pos = 0usize;
    let t = *data.first().ok_or_else(|| corrupt("empty doc values"))?;
    if t != want {
        return Err(corrupt(format!("doc values type {t}, expected {want}")));
    }
    pos += 1;
    let docs = DocSet::read(data, &mut pos)?;
    if docs.max_doc() != max_doc {
        return Err(corrupt("doc values max_doc mismatch"));
    }
    Ok((docs, pos))
}

/// Appends to `found` the docs of `docs` (ascending) that are in `set`, and to `slots` their
/// ranks; returns where this batch starts in both.
#[inline]
fn batch_ranks<T: Copy>(set: &DocSet, docs: &[u32], found: &mut Vec<u32>, slots: &mut Vec<T>, rank: impl Fn(u32) -> T) -> usize {
    debug_assert!(docs.windows(2).all(|w| w[0] < w[1]), "batch docs must be ascending");
    let start = slots.len();
    if set.is_all() {
        let n = docs.partition_point(|&d| d < set.max_doc());
        found.extend_from_slice(&docs[..n]);
        slots.extend(docs[..n].iter().map(|&d| rank(d)));
    } else if !set.is_empty() {
        for &d in docs {
            if let Some(r) = set.rank(d) {
                found.push(d);
                slots.push(rank(r));
            }
        }
    }
    start
}

/// Appends the values of the docs of `docs` (ascending) that have any: the doc to `found`, its
/// value count to `counts` and the values to `values` (flattened). Shared by the multi-valued
/// types, whose values are a flattened [`NumericValues`] with optional per-doc addresses.
#[allow(clippy::too_many_arguments)]
#[inline]
fn multi_batch<T: Copy>(
    set: &DocSet,
    addresses: Option<&MonotonicValues>,
    flat: &NumericValues,
    docs: &[u32],
    found: &mut Vec<u32>,
    counts: &mut Vec<u32>,
    values: &mut Vec<T>,
    index: impl Fn(T) -> u64 + Copy,
    from_index: impl Fn(u64) -> T,
    value: impl Fn(i64) -> T,
) {
    let start = values.len();
    match addresses {
        None => {
            batch_ranks(set, docs, found, values, |r| from_index(r as u64));
            counts.resize(found.len(), 1);
        }
        Some(a) => {
            // Ranks go into `counts` first, then each becomes its value count while its
            // value indices are appended.
            let c0 = batch_ranks(set, docs, found, counts, |r| r);
            for c in counts[c0..].iter_mut() {
                let r = *c as u64;
                let (s, e) = (a.get(r), a.get(r + 1));
                *c = e.saturating_sub(s) as u32;
                values.extend((s..e.max(s)).map(&from_index));
            }
        }
    }
    flat.gather(&mut values[start..], index, value);
}

/// Single `i64` per doc.
#[derive(Clone, Debug)]
pub struct NumericDocValues {
    docs: DocSet,
    values: NumericValues,
    skip: Option<SkipIndex>,
}

impl NumericDocValues {
    pub(crate) fn open(data: Slice, max_doc: u32) -> Result<NumericDocValues> {
        let (docs, mut pos) = open_head(&data, T_NUMERIC, max_doc)?;
        let values = NumericValues::read(&data, &mut pos)?;
        if values.len() != docs.len() as u64 {
            return Err(corrupt("numeric doc values count mismatch"));
        }
        let skip = read_skip_index(&data, &mut pos)?;
        Ok(NumericDocValues { docs, values, skip })
    }

    /// Value of `doc`.
    #[inline]
    pub fn get(&self, doc: u32) -> Option<i64> {
        if self.docs.is_all() {
            return (doc < self.docs.max_doc()).then(|| self.values.get(doc as u64));
        }
        self.docs.rank(doc).map(|i| self.values.get(i as u64))
    }

    /// Value of the `index`-th doc of [`docs`](Self::docs).
    #[inline]
    pub fn value_at(&self, index: u32) -> i64 {
        self.values.get(index as u64)
    }

    /// Docs with a value.
    pub fn docs(&self) -> &DocSet {
        &self.docs
    }

    /// Iterates docs with values; read values with `value_at(cursor.index())`.
    pub fn cursor(&self) -> DocSetCursor {
        self.docs.cursor()
    }

    pub fn skipper(&self) -> Option<DocValuesSkipper> {
        self.skip.clone().map(DocValuesSkipper::new)
    }

    /// Batch lookup for collectors: for each doc of `docs` (ascending) that has a value,
    /// appends the doc to `found` and its value to `values`. Dense fields need no rank lookups
    /// and the value encoding is dispatched once per batch.
    pub fn get_batch(&self, docs: &[u32], found: &mut Vec<u32>, values: &mut Vec<i64>) {
        let start = batch_ranks(&self.docs, docs, found, values, |r| r as i64);
        self.values.gather(&mut values[start..], |i| i as u64, |v| v);
    }

    /// Values of the docs with value number `start..start + out.len()` (in doc order), decoded
    /// sequentially: aggregations over every doc. Slots past the last value are set to 0.
    pub fn values_range(&self, start: u32, out: &mut [i64]) {
        self.values.get_range(start as u64, out);
    }
}

/// One byte string per doc.
#[derive(Clone, Debug)]
pub struct BinaryDocValues {
    docs: DocSet,
    fixed: Option<usize>,
    offsets: Option<MonotonicValues>,
    data: Slice,
}

impl BinaryDocValues {
    pub(crate) fn open(data: Slice, max_doc: u32) -> Result<BinaryDocValues> {
        let (docs, mut pos) = open_head(&data, T_BINARY, max_doc)?;
        let mut r = In::at(&data, pos);
        let min = r.vu32()? as usize;
        let max = r.vu32()? as usize;
        pos = r.pos();
        let (fixed, offsets) = if min == max {
            (Some(min), None)
        } else {
            let o = MonotonicValues::read(&data, &mut pos)?;
            if o.len() != docs.len() as u64 + 1 {
                return Err(corrupt("binary offsets count mismatch"));
            }
            (None, Some(o))
        };
        let mut r = In::at(&data, pos);
        let n = r.vlen(r.remaining())?;
        let bytes = data
            .try_slice(r.pos() as u64, n as u64)
            .ok_or_else(|| corrupt("binary data"))?;
        if let Some(f) = fixed
            && f.checked_mul(docs.len() as usize) != Some(n)
        {
            return Err(corrupt("binary fixed length mismatch"));
        }
        Ok(BinaryDocValues {
            docs,
            fixed,
            offsets,
            data: bytes,
        })
    }

    /// The value of the `index`-th doc with a value.
    #[inline]
    pub fn value_at(&self, index: u32) -> &[u8] {
        let (s, e) = match (self.fixed, &self.offsets) {
            (Some(f), _) => (index as usize * f, index as usize * f + f),
            (None, Some(o)) => (
                o.get(index as u64) as usize,
                o.get(index as u64 + 1) as usize,
            ),
            _ => (0, 0),
        };
        self.data.get(s..e).unwrap_or(&[])
    }

    #[inline]
    pub fn get(&self, doc: u32) -> Option<&[u8]> {
        self.docs.rank(doc).map(|i| self.value_at(i))
    }

    pub fn docs(&self) -> &DocSet {
        &self.docs
    }
}

/// One ordinal per doc into a sorted dictionary.
#[derive(Clone, Debug)]
pub struct SortedDocValues {
    docs: DocSet,
    ords: NumericValues,
    dict: TermDict,
    skip: Option<SkipIndex>,
}

impl SortedDocValues {
    pub(crate) fn open(data: Slice, max_doc: u32) -> Result<SortedDocValues> {
        let (docs, mut pos) = open_head(&data, T_SORTED, max_doc)?;
        let ords = NumericValues::read(&data, &mut pos)?;
        let dict = TermDict::read(&data, &mut pos)?;
        if ords.len() != docs.len() as u64 {
            return Err(corrupt("sorted ords count mismatch"));
        }
        let skip = read_skip_index(&data, &mut pos)?;
        Ok(SortedDocValues {
            docs,
            ords,
            dict,
            skip,
        })
    }

    #[inline]
    pub fn ord(&self, doc: u32) -> Option<u64> {
        self.docs.rank(doc).map(|i| self.ords.get(i as u64) as u64)
    }

    #[inline]
    pub fn ord_at(&self, index: u32) -> u64 {
        self.ords.get(index as u64) as u64
    }

    /// Batch lookup: for each doc of `docs` (ascending) with a value, appends the doc to
    /// `found` and its ordinal to `ords`.
    pub fn ords_batch(&self, docs: &[u32], found: &mut Vec<u32>, ords: &mut Vec<u64>) {
        let start = batch_ranks(&self.docs, docs, found, ords, |r| r as u64);
        self.ords.gather(&mut ords[start..], |i| i, |v| v as u64);
    }

    /// Ordinals of the docs with value number `start..start + out.len()`, sequentially.
    pub fn ords_range(&self, start: u32, out: &mut [i64]) {
        self.ords.get_range(start as u64, out);
    }

    pub fn docs(&self) -> &DocSet {
        &self.docs
    }

    /// Number of distinct values.
    pub fn value_count(&self) -> u64 {
        self.dict.len()
    }

    pub fn lookup_ord(&self, ord: u64, out: &mut Vec<u8>) {
        self.dict.term(ord, out)
    }

    pub fn lookup_term(&self, term: &[u8]) -> std::result::Result<u64, u64> {
        self.dict.seek(term)
    }

    pub fn dict(&self) -> &TermDict {
        &self.dict
    }

    pub fn skipper(&self) -> Option<DocValuesSkipper> {
        self.skip.clone().map(DocValuesSkipper::new)
    }
}

/// Sorted unique ordinals per doc.
#[derive(Clone, Debug)]
pub struct SortedSetDocValues {
    docs: DocSet,
    addresses: Option<MonotonicValues>,
    ords: NumericValues,
    dict: TermDict,
    skip: Option<SkipIndex>,
}

/// Ordinals (or values) of one doc.
#[derive(Clone)]
pub struct OrdsIter<'a> {
    values: &'a NumericValues,
    next: u64,
    end: u64,
}

impl Iterator for OrdsIter<'_> {
    type Item = u64;
    #[inline]
    fn next(&mut self) -> Option<u64> {
        if self.next < self.end {
            let v = self.values.get(self.next) as u64;
            self.next += 1;
            Some(v)
        } else {
            None
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = (self.end - self.next) as usize;
        (n, Some(n))
    }
}

impl ExactSizeIterator for OrdsIter<'_> {}

impl SortedSetDocValues {
    pub(crate) fn open(data: Slice, max_doc: u32) -> Result<SortedSetDocValues> {
        let (docs, mut pos) = open_head(&data, T_SORTED_SET, max_doc)?;
        let singleton = *data
            .get(pos)
            .ok_or_else(|| corrupt("sorted set truncated"))?
            != 0;
        pos += 1;
        let addresses = if singleton {
            None
        } else {
            Some(MonotonicValues::read(&data, &mut pos)?)
        };
        let ords = NumericValues::read(&data, &mut pos)?;
        let dict = TermDict::read(&data, &mut pos)?;
        match &addresses {
            None if ords.len() != docs.len() as u64 => {
                return Err(corrupt("sorted set count mismatch"));
            }
            Some(a)
                if a.len() != docs.len() as u64 + 1 || a.get(docs.len() as u64) != ords.len() =>
            {
                return Err(corrupt("sorted set addresses mismatch"));
            }
            _ => {}
        }
        let skip = read_skip_index(&data, &mut pos)?;
        Ok(SortedSetDocValues {
            docs,
            addresses,
            ords,
            dict,
            skip,
        })
    }

    /// Range of the `index`-th doc's ordinals in the flattened ordinal array.
    #[inline]
    fn range(&self, index: u32) -> (u64, u64) {
        match &self.addresses {
            None => (index as u64, index as u64 + 1),
            Some(a) => (a.get(index as u64), a.get(index as u64 + 1)),
        }
    }

    /// Ordinals of `doc` (empty if it has none).
    #[inline]
    pub fn ords(&self, doc: u32) -> OrdsIter<'_> {
        match self.docs.rank(doc) {
            Some(i) => self.ords_at(i),
            None => OrdsIter {
                values: &self.ords,
                next: 0,
                end: 0,
            },
        }
    }

    /// Ordinals of the `index`-th doc with values.
    #[inline]
    pub fn ords_at(&self, index: u32) -> OrdsIter<'_> {
        let (s, e) = self.range(index);
        OrdsIter {
            values: &self.ords,
            next: s,
            end: e,
        }
    }

    /// Batch lookup: for each doc of `docs` (ascending) with ordinals, appends the doc to
    /// `found`, its ordinal count to `counts` and its ordinals (ascending) to `ords`.
    pub fn ords_batch(&self, docs: &[u32], found: &mut Vec<u32>, counts: &mut Vec<u32>, ords: &mut Vec<u64>) {
        multi_batch(&self.docs, self.addresses.as_ref(), &self.ords, docs, found, counts, ords, |i| i, |i| i, |v| v as u64);
    }

    /// Whether every doc has at most one value (the field could be read as sorted).
    pub fn is_single_valued(&self) -> bool {
        self.addresses.is_none()
    }

    pub fn docs(&self) -> &DocSet {
        &self.docs
    }

    pub fn value_count(&self) -> u64 {
        self.dict.len()
    }

    pub fn lookup_ord(&self, ord: u64, out: &mut Vec<u8>) {
        self.dict.term(ord, out)
    }

    pub fn lookup_term(&self, term: &[u8]) -> std::result::Result<u64, u64> {
        self.dict.seek(term)
    }

    pub fn dict(&self) -> &TermDict {
        &self.dict
    }

    pub fn skipper(&self) -> Option<DocValuesSkipper> {
        self.skip.clone().map(DocValuesSkipper::new)
    }
}

/// Sorted `i64` lists per doc (duplicates kept).
#[derive(Clone, Debug)]
pub struct SortedNumericDocValues {
    docs: DocSet,
    addresses: Option<MonotonicValues>,
    values: NumericValues,
    skip: Option<SkipIndex>,
}

/// Values of one doc.
#[derive(Clone)]
pub struct ValuesIter<'a> {
    values: &'a NumericValues,
    next: u64,
    end: u64,
}

impl Iterator for ValuesIter<'_> {
    type Item = i64;
    #[inline]
    fn next(&mut self) -> Option<i64> {
        if self.next < self.end {
            let v = self.values.get(self.next);
            self.next += 1;
            Some(v)
        } else {
            None
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = (self.end - self.next) as usize;
        (n, Some(n))
    }
}

impl ExactSizeIterator for ValuesIter<'_> {}

impl SortedNumericDocValues {
    pub(crate) fn open(data: Slice, max_doc: u32) -> Result<SortedNumericDocValues> {
        let (docs, mut pos) = open_head(&data, T_SORTED_NUMERIC, max_doc)?;
        let singleton = *data
            .get(pos)
            .ok_or_else(|| corrupt("sorted numeric truncated"))?
            != 0;
        pos += 1;
        let addresses = if singleton {
            None
        } else {
            Some(MonotonicValues::read(&data, &mut pos)?)
        };
        let values = NumericValues::read(&data, &mut pos)?;
        match &addresses {
            None if values.len() != docs.len() as u64 => {
                return Err(corrupt("sorted numeric count mismatch"));
            }
            Some(a)
                if a.len() != docs.len() as u64 + 1 || a.get(docs.len() as u64) != values.len() =>
            {
                return Err(corrupt("sorted numeric addresses mismatch"));
            }
            _ => {}
        }
        let skip = read_skip_index(&data, &mut pos)?;
        Ok(SortedNumericDocValues {
            docs,
            addresses,
            values,
            skip,
        })
    }

    #[inline]
    fn range(&self, index: u32) -> (u64, u64) {
        match &self.addresses {
            None => (index as u64, index as u64 + 1),
            Some(a) => (a.get(index as u64), a.get(index as u64 + 1)),
        }
    }

    /// Values of `doc` in ascending order (empty if none).
    #[inline]
    pub fn values(&self, doc: u32) -> ValuesIter<'_> {
        match self.docs.rank(doc) {
            Some(i) => self.values_at(i),
            None => ValuesIter {
                values: &self.values,
                next: 0,
                end: 0,
            },
        }
    }

    #[inline]
    pub fn values_at(&self, index: u32) -> ValuesIter<'_> {
        let (s, e) = self.range(index);
        ValuesIter {
            values: &self.values,
            next: s,
            end: e,
        }
    }

    pub fn is_single_valued(&self) -> bool {
        self.addresses.is_none()
    }

    /// The values of a single-valued field — one per doc with a value, in doc order (index =
    /// the doc's rank in [`docs`](Self::docs)) — for batch decoding with
    /// [`NumericValues::get_range`](crate::packed::NumericValues::get_range).
    pub fn singleton_values(&self) -> Option<&NumericValues> {
        self.addresses.is_none().then_some(&self.values)
    }

    pub fn docs(&self) -> &DocSet {
        &self.docs
    }

    pub fn skipper(&self) -> Option<DocValuesSkipper> {
        self.skip.clone().map(DocValuesSkipper::new)
    }

    /// Batch lookup: for each doc of `docs` (ascending) with values, appends the doc to
    /// `found`, its value count to `counts` and its values (ascending) to `values`.
    pub fn get_batch(&self, docs: &[u32], found: &mut Vec<u32>, counts: &mut Vec<u32>, values: &mut Vec<i64>) {
        multi_batch(&self.docs, self.addresses.as_ref(), &self.values, docs, found, counts, values, |i| i as u64, |i| i as i64, |v| v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    fn sparse_docs(rng: &mut StdRng, max_doc: u32, p: f64) -> Vec<u32> {
        (0..max_doc).filter(|_| rng.gen_bool(p)).collect()
    }

    #[test]
    fn numeric_round_trip_and_skipper() {
        let mut rng = StdRng::seed_from_u64(1);
        for &(max_doc, p) in &[
            (0u32, 1.0),
            (1, 1.0),
            (127, 0.5),
            (129, 1.0),
            (50_000, 0.3),
            (70_000, 1.0),
        ] {
            let docs = sparse_docs(&mut rng, max_doc, p);
            let values: Vec<i64> = docs
                .iter()
                .map(|&d| d as i64 * 3 - 1000 + rng.gen_range(0..5))
                .collect();
            let mut out = Vec::new();
            write_numeric_dv(&docs, &values, max_doc, &mut out);
            let dv = NumericDocValues::open(Slice::from(out), max_doc).unwrap();
            for d in 0..max_doc {
                let want = docs.binary_search(&d).ok().map(|i| values[i]);
                assert_eq!(dv.get(d), want);
            }
            let mut c = dv.cursor();
            let mut n = 0;
            while c.next() != NO_MORE_DOCS {
                assert_eq!(dv.value_at(c.index()), values[n]);
                n += 1;
            }
            assert_eq!(n, docs.len());
            match dv.skipper() {
                None => assert!(docs.is_empty()),
                Some(mut s) => {
                    assert_eq!(s.global_doc_count() as usize, docs.len());
                    assert_eq!(s.global_min_value(), *values.iter().min().unwrap());
                    assert_eq!(s.global_max_value(), *values.iter().max().unwrap());
                    for target in (0..max_doc + 10).step_by(997) {
                        s.advance(target);
                        let first = docs.iter().position(|&d| d >= target);
                        match first {
                            None => assert_eq!(s.num_levels(), 0),
                            Some(i) => {
                                for l in 0..s.num_levels() {
                                    assert!(
                                        s.min_doc_id(l) <= docs[i] && s.max_doc_id(l) >= docs[i]
                                    );
                                    let lo = docs.binary_search(&s.min_doc_id(l)).unwrap();
                                    let hi = docs.binary_search(&s.max_doc_id(l)).unwrap();
                                    assert_eq!(s.doc_count(l) as usize, hi - lo + 1);
                                    assert_eq!(
                                        s.min_value(l),
                                        *values[lo..=hi].iter().min().unwrap()
                                    );
                                    assert_eq!(
                                        s.max_value(l),
                                        *values[lo..=hi].iter().max().unwrap()
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// `cargo test --release -p search-codec --lib bench_doc_values -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_doc_values() {
        let mut rng = StdRng::seed_from_u64(9);
        let max_doc = 4_000_000u32;
        for (name, p) in [("dense", 1.0), ("sparse 30%", 0.3)] {
            let docs = sparse_docs(&mut rng, max_doc, p);
            // Timestamps-like values: increasing with jitter, in milliseconds.
            let values: Vec<i64> = docs.iter().map(|&d| 1_700_000_000_000 + d as i64 * 1000 + rng.gen_range(0..1000)).collect();
            let mut out = Vec::new();
            write_numeric_dv(&docs, &values, max_doc, &mut out);
            let dv = NumericDocValues::open(Slice::from(out), max_doc).unwrap();
            let t = std::time::Instant::now();
            let mut sum = 0i64;
            let mut c = dv.cursor();
            while c.next() != NO_MORE_DOCS {
                sum = sum.wrapping_add(dv.value_at(c.index()));
            }
            let e = t.elapsed();
            println!("numeric {name}: iterate {:.1} ns/value ({sum})", e.as_nanos() as f64 / docs.len() as f64);
            let t = std::time::Instant::now();
            let mut sum = 0i64;
            for d in (0..max_doc).step_by(7) {
                sum = sum.wrapping_add(dv.get(d).unwrap_or(0));
            }
            let e = t.elapsed();
            println!("numeric {name}: get(doc) every 7th doc {:.1} ns/doc ({sum})", e.as_nanos() as f64 / (max_doc / 7) as f64);
            let batch: Vec<u32> = (0..max_doc).step_by(7).collect();
            let (mut found, mut vals) = (Vec::with_capacity(128), Vec::with_capacity(128));
            let t = std::time::Instant::now();
            let mut sum = 0i64;
            for chunk in batch.chunks(128) {
                found.clear();
                vals.clear();
                dv.get_batch(chunk, &mut found, &mut vals);
                sum = vals.iter().fold(sum, |a, &v| a.wrapping_add(v));
            }
            let e = t.elapsed();
            println!("numeric {name}: get_batch(128) every 7th doc {:.1} ns/doc ({sum})", e.as_nanos() as f64 / batch.len() as f64);
            let mut buf = vec![0i64; 4096];
            let t = std::time::Instant::now();
            let mut sum = 0i64;
            let mut at = 0u32;
            while (at as usize) < docs.len() {
                let n = (docs.len() - at as usize).min(buf.len());
                dv.values_range(at, &mut buf[..n]);
                sum = buf[..n].iter().fold(sum, |a, &v| a.wrapping_add(v));
                at += n as u32;
            }
            let e = t.elapsed();
            println!("numeric {name}: values_range(4096) {:.2} ns/value ({sum})", e.as_nanos() as f64 / docs.len() as f64);
        }
        let (dict, terms) = dict_of(50_000);
        let docs: Vec<u32> = (0..max_doc).collect();
        let ords: Vec<u64> = docs.iter().map(|_| rng.gen_range(0..terms.len() as u64)).collect();
        let mut out = Vec::new();
        write_sorted_dv(&docs, &ords, &dict, max_doc, &mut out);
        let dv = SortedDocValues::open(Slice::from(out), max_doc).unwrap();
        let t = std::time::Instant::now();
        let mut counts = vec![0u32; terms.len()];
        for d in 0..max_doc {
            if let Some(o) = dv.ord(d) {
                counts[o as usize] += 1;
            }
        }
        println!("sorted ord(doc) terms-agg loop: {:.1} ns/doc", t.elapsed().as_nanos() as f64 / max_doc as f64);
        let t = std::time::Instant::now();
        let mut buf = Vec::new();
        let mut len = 0;
        for o in (0..terms.len() as u64).step_by(3) {
            dv.lookup_ord(o, &mut buf);
            len += buf.len();
        }
        println!("lookup_ord: {:.1} ns/term ({len})", t.elapsed().as_nanos() as f64 / (terms.len() / 3) as f64);
    }

    #[test]
    fn binary_round_trip() {
        let mut rng = StdRng::seed_from_u64(2);
        for fixed in [true, false] {
            let max_doc = 3000;
            let docs = sparse_docs(&mut rng, max_doc, 0.6);
            let mut list = ByteList::new();
            let mut vals = Vec::new();
            for _ in &docs {
                let n = if fixed { 4 } else { rng.gen_range(0..20) };
                let v: Vec<u8> = (0..n).map(|_| rng.r#gen()).collect();
                list.push(&v);
                vals.push(v);
            }
            let mut out = Vec::new();
            write_binary_dv(&docs, &list, max_doc, &mut out);
            let dv = BinaryDocValues::open(Slice::from(out), max_doc).unwrap();
            for d in 0..max_doc {
                let want = docs.binary_search(&d).ok().map(|i| vals[i].as_slice());
                assert_eq!(dv.get(d), want);
            }
        }
    }

    fn dict_of(n: usize) -> (ByteList, Vec<Vec<u8>>) {
        let mut terms: Vec<Vec<u8>> = (0..n)
            .map(|i| format!("term-{:06}-{}", i * 7, i % 3).into_bytes())
            .collect();
        terms.sort();
        terms.dedup();
        let mut l = ByteList::new();
        for t in &terms {
            l.push(t);
        }
        (l, terms)
    }

    #[test]
    fn term_dict_lookups() {
        for n in [0usize, 1, 15, 16, 17, 63, 64, 65, 1000] {
            let (list, terms) = dict_of(n);
            let mut out = Vec::new();
            write_term_dict(&mut list.clone(), &mut out).unwrap();
            let d = TermDict::read(&Slice::from(out), &mut 0).unwrap();
            assert_eq!(d.len() as usize, terms.len());
            let mut buf = Vec::new();
            for (i, t) in terms.iter().enumerate() {
                d.term(i as u64, &mut buf);
                assert_eq!(&buf, t);
                assert_eq!(d.seek(t), Ok(i as u64));
                let mut probe = t.clone();
                probe.push(0);
                assert_eq!(d.seek(&probe), Err(i as u64 + 1));
            }
            assert_eq!(d.seek(b""), Err(0));
            assert_eq!(d.seek(b"zzzz"), Err(terms.len() as u64));
            for start in [0u64, 1, 15, 16, 17, 63, 64, 65, 500] {
                let mut it = d.iter_from(start);
                let mut o = start;
                while let Some(t) = it.next() {
                    assert_eq!(t, &terms[o as usize][..]);
                    assert_eq!(it.ord(), o);
                    o += 1;
                }
                assert_eq!(o.max(start), (terms.len() as u64).max(start));
            }
        }
    }

    #[test]
    fn term_dict_intersection_matches_brute_force() {
        use crate::automaton::{AlwaysMatch, Dfa, Prefix, regexp};
        let mut rng = StdRng::seed_from_u64(12);
        let alphabet = [b'a', b'b', b'c', 0xC3, 0xA9, 0xFF];
        for n in [0usize, 1, 5, 64, 65, 3000] {
            let mut terms: Vec<Vec<u8>> = (0..n)
                .map(|_| (0..rng.gen_range(0..7)).map(|_| alphabet[rng.gen_range(0..alphabet.len())]).collect())
                .collect();
            terms.sort();
            terms.dedup();
            let mut list = ByteList::new();
            for t in &terms {
                list.push(t);
            }
            let mut out = Vec::new();
            write_term_dict(&mut list.clone(), &mut out).unwrap();
            let d = TermDict::read(&Slice::from(out), &mut 0).unwrap();
            let auts: Vec<Box<dyn Automaton>> = vec![
                Box::new(AlwaysMatch),
                Box::new(Prefix::new(&b"ab"[..])),
                Box::new(Prefix::new(&b""[..])),
                Box::new(Prefix::new(&[0xFFu8, 0xFF][..])),
                Box::new(regexp::compile("a.*c", regexp::ALL, false, 10_000).unwrap()),
                Box::new(regexp::compile("(b|c)+", regexp::ALL, false, 10_000).unwrap()),
                Box::new(regexp::compile("é.?", regexp::ALL, false, 10_000).unwrap()),
                Box::new(Dfa::levenshtein("abca", 1, 1, true)),
            ];
            for aut in &auts {
                let want: Vec<u64> = terms
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| aut.matches(t))
                    .map(|(i, _)| i as u64)
                    .collect();
                let mut it = d.intersect(aut.as_ref());
                let mut got = Vec::new();
                while let Some(o) = it.next() {
                    assert_eq!(it.term(), &terms[o as usize][..]);
                    got.push(o);
                }
                assert_eq!(got, want, "n={n}");
                assert_eq!(it.next(), None);
            }
        }
    }

    #[test]
    fn sorted_and_sorted_set_round_trip() {
        let mut rng = StdRng::seed_from_u64(3);
        let (dict, terms) = dict_of(500);
        let max_doc = 20_000;
        let docs = sparse_docs(&mut rng, max_doc, 0.7);
        // sorted
        let ords: Vec<u64> = docs
            .iter()
            .map(|_| rng.gen_range(0..terms.len() as u64))
            .collect();
        let mut out = Vec::new();
        write_sorted_dv(&docs, &ords, &dict, max_doc, &mut out);
        let dv = SortedDocValues::open(Slice::from(out), max_doc).unwrap();
        let mut buf = Vec::new();
        for (i, &d) in docs.iter().enumerate() {
            assert_eq!(dv.ord(d), Some(ords[i]));
            dv.lookup_ord(ords[i], &mut buf);
            assert_eq!(buf, terms[ords[i] as usize]);
        }
        assert_eq!(dv.value_count(), terms.len() as u64);
        // sorted set, multi-valued and singleton
        for single in [false, true] {
            let mut addresses = vec![0u64];
            let mut flat = Vec::new();
            let mut per_doc = Vec::new();
            for _ in &docs {
                let k = if single { 1 } else { rng.gen_range(1..5) };
                let mut s: Vec<u64> = (0..k)
                    .map(|_| rng.gen_range(0..terms.len() as u64))
                    .collect();
                s.sort();
                s.dedup();
                flat.extend_from_slice(&s);
                addresses.push(flat.len() as u64);
                per_doc.push(s);
            }
            let mut out = Vec::new();
            write_sorted_set_dv(&docs, &addresses, &flat, &dict, max_doc, &mut out);
            let dv = SortedSetDocValues::open(Slice::from(out), max_doc).unwrap();
            assert_eq!(dv.is_single_valued(), single);
            for d in 0..max_doc {
                let want = docs
                    .binary_search(&d)
                    .ok()
                    .map(|i| per_doc[i].clone())
                    .unwrap_or_default();
                assert_eq!(dv.ords(d).collect::<Vec<_>>(), want);
            }
            let mut s = dv.skipper().unwrap();
            s.advance(0);
            assert_eq!(s.min_doc_id(0), docs[0]);
        }
    }

    #[test]
    fn sorted_numeric_round_trip() {
        let mut rng = StdRng::seed_from_u64(4);
        let max_doc = 10_000;
        let docs = sparse_docs(&mut rng, max_doc, 0.5);
        let mut addresses = vec![0u64];
        let mut flat = Vec::new();
        let mut per_doc = Vec::new();
        for _ in &docs {
            let k = rng.gen_range(1..4);
            let mut s: Vec<i64> = (0..k).map(|_| rng.gen_range(-50..50)).collect();
            s.sort();
            flat.extend_from_slice(&s);
            addresses.push(flat.len() as u64);
            per_doc.push(s);
        }
        let mut out = Vec::new();
        write_sorted_numeric_dv(&docs, &addresses, &flat, max_doc, &mut out);
        let dv = SortedNumericDocValues::open(Slice::from(out), max_doc).unwrap();
        for d in 0..max_doc {
            let want = docs
                .binary_search(&d)
                .ok()
                .map(|i| per_doc[i].clone())
                .unwrap_or_default();
            assert_eq!(dv.values(d).collect::<Vec<_>>(), want);
        }
        assert!(dv.singleton_values().is_none());
        // Single-valued: the values in doc order, batch-decodable.
        let ones: Vec<i64> = (0..docs.len()).map(|_| rng.gen_range(-1_000_000..1_000_000)).collect();
        let addresses: Vec<u64> = (0..=docs.len() as u64).collect();
        let mut out = Vec::new();
        write_sorted_numeric_dv(&docs, &addresses, &ones, max_doc, &mut out);
        let dv = SortedNumericDocValues::open(Slice::from(out), max_doc).unwrap();
        let vals = dv.singleton_values().expect("single-valued");
        let mut buf = vec![0i64; docs.len()];
        vals.get_range(0, &mut buf);
        assert_eq!(buf, ones);
        for (i, &d) in docs.iter().enumerate() {
            assert_eq!(dv.docs().rank(d), Some(i as u32));
            assert_eq!(dv.values(d).collect::<Vec<_>>(), vec![ones[i]]);
        }
    }

    /// Random ascending batches of docs (some without values, some past max_doc).
    fn batches(rng: &mut StdRng, max_doc: u32) -> Vec<Vec<u32>> {
        let mut out = vec![Vec::new(), (0..max_doc + 3).collect()];
        for _ in 0..20 {
            let p = [0.01, 0.3, 0.9][rng.gen_range(0..3)];
            out.push((0..max_doc + 3).filter(|_| rng.gen_bool(p)).collect());
        }
        out
    }

    /// Every encoding (const, table, gcd, blocks, all bit widths incl. 8/16/32/64) on dense and
    /// sparse fields: batch lookups and range decoding agree with per-doc lookups.
    #[test]
    fn batch_lookups_match_single_lookups() {
        let mut rng = StdRng::seed_from_u64(77);
        type Gen = fn(&mut StdRng, u32) -> i64;
        let gens: [(&str, Gen); 9] = [
            ("const", |_, _| 42),
            ("table", |r, _| [-5i64, 7, 1 << 40, 99][r.gen_range(0..4)]),
            ("gcd", |r, _| r.gen_range(0..100_000) * 1000 - 7),
            ("w8", |r, _| r.gen_range(0..256)),
            ("w16", |r, _| r.gen_range(0..65_536) - 300),
            ("w32", |r, _| r.gen_range(0..1i64 << 32)),
            ("w64", |r, _| r.r#gen()),
            ("w57", |r, _| r.gen_range(0..1i64 << 57)),
            ("blocks", |r, d| d as i64 * 1_000_000 + r.gen_range(0..1000)),
        ];
        for (name, g) in gens {
            for (max_doc, p) in [(0u32, 1.0), (1, 1.0), (300, 0.5), (40_000, 1.0), (40_000, 0.2)] {
                let docs = sparse_docs(&mut rng, max_doc, p);
                let values: Vec<i64> = docs.iter().map(|&d| g(&mut rng, d)).collect();
                let mut out = Vec::new();
                write_numeric_dv(&docs, &values, max_doc, &mut out);
                let dv = NumericDocValues::open(Slice::from(out), max_doc).unwrap();
                for b in batches(&mut rng, max_doc) {
                    let (mut found, mut got) = (vec![7u32], vec![-1i64]);
                    dv.get_batch(&b, &mut found, &mut got);
                    let want: Vec<(u32, i64)> = b.iter().filter_map(|&d| dv.get(d).map(|v| (d, v))).collect();
                    assert_eq!(found[1..].len(), want.len(), "{name} {max_doc} {p}");
                    assert!(found[1..].iter().zip(&got[1..]).map(|(&d, &v)| (d, v)).eq(want.iter().copied()), "{name}");
                    assert_eq!((found[0], got[0]), (7, -1), "batch output is appended");
                }
                for (start, len) in [(0usize, values.len()), (1, 5), (values.len() / 2, 20_000), (values.len(), 3)] {
                    let start = start.min(values.len());
                    let mut buf = vec![9i64; len];
                    dv.values_range(start as u32, &mut buf);
                    for (k, v) in buf.iter().enumerate() {
                        assert_eq!(*v, values.get(start + k).copied().unwrap_or(0), "{name} range {start}+{k}");
                    }
                }
            }
        }
    }

    #[test]
    fn multi_valued_batches_match_single_lookups() {
        let mut rng = StdRng::seed_from_u64(78);
        let (dict, terms) = dict_of(300);
        for (max_doc, p, single) in [(5_000u32, 0.6, false), (5_000, 1.0, false), (5_000, 0.6, true), (5_000, 1.0, true), (0, 1.0, false)] {
            let docs = sparse_docs(&mut rng, max_doc, p);
            let mut addresses = vec![0u64];
            let (mut flat_ords, mut flat_vals) = (Vec::new(), Vec::new());
            for _ in &docs {
                let k = if single { 1 } else { rng.gen_range(1..5) };
                let mut o: Vec<u64> = (0..k).map(|_| rng.gen_range(0..terms.len() as u64)).collect();
                o.sort();
                o.dedup();
                let mut v: Vec<i64> = (0..o.len()).map(|_| rng.gen_range(-1000..1000)).collect();
                v.sort();
                flat_ords.extend_from_slice(&o);
                flat_vals.extend_from_slice(&v);
                addresses.push(flat_ords.len() as u64);
            }
            let mut out = Vec::new();
            write_sorted_set_dv(&docs, &addresses, &flat_ords, &dict, max_doc, &mut out);
            let ss = SortedSetDocValues::open(Slice::from(out), max_doc).unwrap();
            let mut out = Vec::new();
            write_sorted_numeric_dv(&docs, &addresses, &flat_vals, max_doc, &mut out);
            let sn = SortedNumericDocValues::open(Slice::from(out), max_doc).unwrap();
            assert_eq!(ss.is_single_valued(), single || docs.is_empty() || addresses.windows(2).all(|w| w[1] - w[0] == 1));
            for b in batches(&mut rng, max_doc) {
                let (mut found, mut counts, mut ords) = (Vec::new(), Vec::new(), Vec::new());
                ss.ords_batch(&b, &mut found, &mut counts, &mut ords);
                let mut at = 0usize;
                let mut seen = 0usize;
                for &d in &b {
                    let want: Vec<u64> = ss.ords(d).collect();
                    if want.is_empty() {
                        continue;
                    }
                    assert_eq!(found[seen], d);
                    assert_eq!(counts[seen] as usize, want.len());
                    assert_eq!(&ords[at..at + want.len()], &want[..]);
                    at += want.len();
                    seen += 1;
                }
                assert_eq!((seen, at), (found.len(), ords.len()));
                let (mut found, mut counts, mut vals) = (Vec::new(), Vec::new(), Vec::new());
                sn.get_batch(&b, &mut found, &mut counts, &mut vals);
                let want: Vec<i64> = b.iter().flat_map(|&d| sn.values(d)).collect();
                assert_eq!(vals, want);
                assert_eq!(counts.iter().sum::<u32>() as usize, want.len());
                assert_eq!(found.len(), counts.len());
            }
        }
        // Sorted: ordinals by batch and by range.
        let docs = sparse_docs(&mut rng, 3000, 0.7);
        let ords: Vec<u64> = docs.iter().map(|_| rng.gen_range(0..terms.len() as u64)).collect();
        let mut out = Vec::new();
        write_sorted_dv(&docs, &ords, &dict, 3000, &mut out);
        let dv = SortedDocValues::open(Slice::from(out), 3000).unwrap();
        for b in batches(&mut rng, 3000) {
            let (mut found, mut got) = (Vec::new(), Vec::new());
            dv.ords_batch(&b, &mut found, &mut got);
            let want: Vec<(u32, u64)> = b.iter().filter_map(|&d| dv.ord(d).map(|o| (d, o))).collect();
            assert!(found.iter().copied().zip(got.iter().copied()).eq(want.iter().copied()));
        }
        let mut buf = vec![0i64; ords.len()];
        dv.ords_range(0, &mut buf);
        assert!(buf.iter().map(|&o| o as u64).eq(ords.iter().copied()));
    }

}
