//! The term dictionary of an inverted field.
//!
//! # Layout
//!
//! Terms are stored in blocks of 32 (`TermsMeta` section). A block starts with the terms'
//! metadata — absolute postings pointers and a bit width per column, then one fixed-width row
//! per term (doc freq, total term freq minus doc freq, doc pointer delta — or the doc itself for
//! single-doc terms — and positions/payloads pointer deltas) — followed by the terms
//! front-coded, each as (bytes shared with the previous term, suffix), the first in full; both
//! lengths usually fit one header byte. An array of block offsets makes `ord → metadata` O(1).
//!
//! The index (`TermsIndex` section) has one key per block: the shortest prefix of the block's
//! first term that sorts after the previous block's last term (empty for block 0). The block
//! that may hold a term `t` is the one with the largest key `<= t`. Keys are stored as a sorted
//! array of their first 8 bytes as big-endian integers, plus the full keys for ties: the floor
//! search is a binary search over integers that stays in cache (8 bytes per 32 terms), and
//! only blocks whose keys share those 8 bytes with `t` compare bytes (like Lucene's BlockTree
//! index, which uses an FST over the same keys; integers compare faster than FST arcs decode).
//!
//! # Lookups
//!
//! - `seek_exact`: floor search, then a scan of at most 32 front-coded entries that compares
//!   only the bytes each entry changes (Lucene's `scanToTermLeaf` trick), then one metadata row.
//!   Fields with a Bloom filter answer most misses before that ([`crate::bloom`]).
//! - Iteration decodes entries sequentially: a few nanoseconds per term, metadata on demand.
//! - Automaton intersection (prefix, wildcard, regexp, fuzzy) runs the automaton over each
//!   term incrementally, reusing the states of the prefix shared with the previous term. When
//!   a prefix leads to a dead state, it asks the automaton for the next string that could still
//!   match ([`Automaton::next_live_byte`], backtracking up the prefix) and seeks to it, so whole
//!   runs of non-matching terms — or the rest of the dictionary — are skipped. The automaton's
//!   literal prefix (`foo` in `foo*bar`) positions the first seek.
//!
//! The metadata header also carries the field statistics BM25 needs (`doc_count`,
//! `sum_doc_freq`, `sum_total_term_freq`) and the min/max terms.

use std::cmp::Ordering;
use std::ops::Bound;

use crate::automaton::{Automaton, literal_prefix, next_candidate};
use crate::bitpack::direct;
use crate::bloom::BloomFilter;
use crate::container::{ContainerWriter, SectionKind};
use crate::error::{Result, corrupt, invalid};
use crate::info::IndexOptions;
use crate::io::{In, Out, read_vu32, read_vu64};
use crate::postings::{PostingsCursor, PostingsData, PostingsFlags, TermState};
use crate::slice::Slice;
use crate::spill::{Spill, SpillOptions};

const TERMS_PER_BLOCK: u64 = 32;

/// Field-level statistics of an inverted field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TermsStats {
    /// Number of distinct terms.
    pub num_terms: u64,
    /// Sum of doc freqs over all terms.
    pub sum_doc_freq: u64,
    /// Sum of total term freqs (equals `sum_doc_freq` without frequencies).
    pub sum_total_term_freq: u64,
    /// Number of docs with at least one term in the field.
    pub doc_count: u32,
}

/// Builds the index and blocks of one field.
pub(crate) struct TermsWriter {
    /// Index keys: first 8 bytes as integers, and the full keys.
    key_prefixes: Vec<u64>,
    key_offsets: Vec<u32>,
    key_bytes: Vec<u8>,
    /// Finished blocks.
    meta: Spill,
    block: Vec<u8>,
    block_terms: Vec<u8>,
    block_offsets: Vec<u64>,
    has_freqs: bool,
    has_pos: bool,
    has_pay: bool,
    count: u64,
    stats: TermsStats,
    first: Vec<u8>,
    last: Vec<u8>,
    /// Last term of the previous block (for the next block's index key).
    prev_block_last: Vec<u8>,
    // States of the current (unflushed) block.
    pending: Vec<TermState>,
}

impl TermsWriter {
    #[cfg(test)]
    pub(crate) fn new(opts: IndexOptions, payloads: bool) -> TermsWriter {
        TermsWriter::with_spill(opts, payloads, &SpillOptions::in_memory()).expect("in-memory writer")
    }

    /// A writer whose index and blocks spill to disk beyond the budget of `spill`.
    pub(crate) fn with_spill(opts: IndexOptions, payloads: bool, spill: &SpillOptions) -> Result<TermsWriter> {
        Ok(TermsWriter {
            key_prefixes: Vec::new(),
            key_offsets: vec![0],
            key_bytes: Vec::new(),
            meta: Spill::new(spill),
            block: Vec::new(),
            block_terms: Vec::new(),
            block_offsets: Vec::new(),
            has_freqs: opts.has_freqs(),
            has_pos: opts.has_positions(),
            has_pay: opts.has_offsets() || (payloads && opts.has_positions()),
            count: 0,
            stats: TermsStats::default(),
            first: Vec::new(),
            last: Vec::new(),
            prev_block_last: Vec::new(),
            pending: Vec::with_capacity(TERMS_PER_BLOCK as usize),
        })
    }

    /// Adds a term (strictly greater than the previous one) with its postings state.
    pub(crate) fn add(&mut self, term: &[u8], st: &TermState) -> Result<()> {
        if self.count > 0 && term <= self.last.as_slice() {
            return Err(invalid("terms must be added in strictly increasing byte order"));
        }
        if self.pending.is_empty() {
            // A new block: its index key is the shortest prefix of its first term that sorts
            // after the previous block's last term.
            let key_len = if self.block_offsets.is_empty() {
                0
            } else {
                common_prefix(&self.prev_block_last, term) + 1
            };
            let key = &term[..key_len.min(term.len())];
            self.key_prefixes.push(key_prefix(key));
            self.key_bytes.extend_from_slice(key);
            if self.key_bytes.len() > u32::MAX as usize {
                return Err(invalid("term dictionary index too large"));
            }
            self.key_offsets.push(self.key_bytes.len() as u32);
            put_entry(&mut self.block_terms, 0, term);
        } else {
            let shared = common_prefix(&self.last, term);
            put_entry(&mut self.block_terms, shared, &term[shared..]);
        }
        self.pending.push(*st);
        if self.count == 0 {
            self.first = term.to_vec();
        }
        self.last.clear();
        self.last.extend_from_slice(term);
        self.count += 1;
        self.stats.sum_doc_freq += st.doc_freq as u64;
        self.stats.sum_total_term_freq += st.total_term_freq;
        if self.pending.len() == TERMS_PER_BLOCK as usize {
            self.flush_block()?;
        }
        Ok(())
    }

    /// Writes the pending block: absolute base pointers, one width per column and one
    /// fixed-width row per term (df, ttf - df, doc pointer delta or singleton doc, positions and
    /// payloads pointer deltas), then the front-coded terms.
    fn flush_block(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.block_offsets.push(self.meta.len());
        self.block.clear();
        let first = self.pending[0];
        let cols = |st: &TermState| -> [u64; 5] {
            [
                st.doc_freq as u64,
                st.total_term_freq - st.doc_freq as u64,
                if st.doc_freq == 1 { st.singleton as u64 } else { st.doc_fp - first.doc_fp },
                st.pos_fp - first.pos_fp,
                st.pay_fp - first.pay_fp,
            ]
        };
        let used = [true, self.has_freqs, true, self.has_pos, self.has_pay];
        let mut widths = [0u32; 5];
        for st in &self.pending {
            for (c, v) in cols(st).iter().enumerate() {
                widths[c] = widths[c].max(direct::bits_for(*v));
            }
        }
        self.block.put_vu64(first.doc_fp);
        self.block.put_vu64(first.pos_fp);
        self.block.put_vu64(first.pay_fp);
        for c in 0..5 {
            if !used[c] {
                widths[c] = 0;
            }
            self.block.put_u8(widths[c] as u8);
        }
        let mut bw = BitWriter::default();
        for st in &self.pending {
            for (c, v) in cols(st).iter().enumerate() {
                bw.put(*v, widths[c]);
            }
        }
        bw.finish(&mut self.block);
        self.block.put_vu32(self.block_terms.len() as u32);
        self.block.extend_from_slice(&self.block_terms);
        self.block_terms.clear();
        self.pending.clear();
        self.prev_block_last.clear();
        self.prev_block_last.extend_from_slice(&self.last);
        self.meta.append(&self.block)
    }

    /// Writes the `TermsIndex` and `TermsMeta` sections of `field`.
    pub(crate) fn finish_into(self, doc_count: u32, w: &mut ContainerWriter<'_>, field: u32) -> Result<()> {
        let (index, header, mut meta) = self.finish_parts(doc_count)?;
        w.section(SectionKind::TermsIndex, field, &index)?;
        w.begin(SectionKind::TermsMeta, field)?;
        w.write(&header)?;
        meta.copy_to(w)?;
        w.end()
    }

    /// Returns the index section, the metadata header (stats, min/max term, block offsets) and
    /// the blocks, which follow the header in the `TermsMeta` section.
    fn finish_parts(mut self, doc_count: u32) -> Result<(Vec<u8>, Vec<u8>, Spill)> {
        self.flush_block()?;
        let n = self.key_prefixes.len();
        let mut index = Vec::with_capacity(n * 12 + 4 + self.key_bytes.len());
        for p in &self.key_prefixes {
            index.put_u64(*p);
        }
        for o in &self.key_offsets {
            index.put_u32(*o);
        }
        index.extend_from_slice(&self.key_bytes);
        let mut out = Vec::with_capacity(self.block_offsets.len() * 8 + 64);
        out.put_vu64(self.count);
        out.put_vu64(self.stats.sum_doc_freq);
        out.put_vu64(self.stats.sum_total_term_freq);
        out.put_vu32(doc_count);
        out.put_lbytes(&self.first);
        out.put_lbytes(&self.last);
        out.put_vu64(self.block_offsets.len() as u64);
        out.put_vu64(self.meta.len());
        for o in &self.block_offsets {
            out.put_u64(*o);
        }
        Ok((index, out, self.meta))
    }

    /// Returns `(index bytes, meta section bytes)` of an in-memory writer.
    #[cfg(test)]
    pub(crate) fn finish(self, doc_count: u32) -> Result<(Vec<u8>, Vec<u8>)> {
        let (index, mut header, meta) = self.finish_parts(doc_count)?;
        header.extend_from_slice(meta.in_memory().expect("in-memory writer"));
        Ok((index, header))
    }
}

/// Appends a front-coded entry: one byte `shared << 4 | suffix length` when both are below 15
/// (most entries), a nibble of 15 meaning "15 plus a varint that follows", then the suffix.
#[inline]
fn put_entry(out: &mut Vec<u8>, shared: usize, suffix: &[u8]) {
    let nib = |v: usize| v.min(15) as u8;
    out.push(nib(shared) << 4 | nib(suffix.len()));
    if shared >= 15 {
        out.put_vu32((shared - 15) as u32);
    }
    if suffix.len() >= 15 {
        out.put_vu32((suffix.len() - 15) as u32);
    }
    out.extend_from_slice(suffix);
}

/// Reads an entry header written by [`put_entry`]: `(shared, suffix length)`.
#[inline]
fn read_entry(data: &[u8], p: &mut usize) -> (usize, usize) {
    let b = data.get(*p).copied().unwrap_or(0);
    *p += 1;
    let mut shared = (b >> 4) as usize;
    if shared == 15 {
        shared += read_vu32(data, p) as usize;
    }
    let mut len = (b & 15) as usize;
    if len == 15 {
        len += read_vu32(data, p) as usize;
    }
    (shared, len)
}

/// The first 8 bytes of `key`, zero-padded, as a big-endian integer: integer order agrees with
/// byte order whenever the prefixes differ.
#[inline]
fn key_prefix(key: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    let n = key.len().min(8);
    b[..n].copy_from_slice(&key[..n]);
    u64::from_be_bytes(b)
}

#[inline]
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// Reads one field's term dictionary. Cheap to clone.
#[derive(Clone)]
pub struct TermsReader {
    /// Index: `num_blocks` key prefixes (u64), `num_blocks + 1` key offsets (u32), key bytes.
    key_prefixes: Slice,
    key_offsets: Slice,
    key_bytes: Slice,
    blocks: Slice,
    meta: Slice,
    num_blocks: u64,
    stats: TermsStats,
    min_term: Vec<u8>,
    max_term: Vec<u8>,
    has_freqs: bool,
    has_pos: bool,
    postings: PostingsData,
    bloom: Option<BloomFilter>,
}

impl std::fmt::Debug for TermsReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TermsReader").field("stats", &self.stats).finish()
    }
}

impl TermsReader {
    pub(crate) fn open(index_bytes: Slice, meta: Slice, postings: PostingsData) -> Result<TermsReader> {
        let mut r = In::new(&meta);
        let num_terms = r.vu64()?;
        let sum_doc_freq = r.vu64()?;
        let sum_total_term_freq = r.vu64()?;
        let doc_count = r.vu32()?;
        let min_term = r.lbytes()?.to_vec();
        let max_term = r.lbytes()?.to_vec();
        let num_blocks = r.vu64()?;
        let meta_len = r.vu64()?;
        if num_blocks != num_terms.div_ceil(TERMS_PER_BLOCK) {
            return Err(corrupt("term dictionary block count mismatch"));
        }
        let nb = num_blocks as usize;
        let fixed = nb.checked_mul(12).and_then(|x| x.checked_add(4)).ok_or_else(|| corrupt("overflow"))?;
        if index_bytes.len() < fixed {
            return Err(corrupt("term index truncated"));
        }
        let key_prefixes = index_bytes.slice(0..nb * 8);
        let key_offsets = index_bytes.slice(nb * 8..fixed);
        let key_bytes = index_bytes.slice(fixed..index_bytes.len());
        let last = u32::from_le_bytes(key_offsets[nb * 4..nb * 4 + 4].try_into().unwrap());
        if last as usize != key_bytes.len() {
            return Err(corrupt("term index keys truncated"));
        }
        let blocks_at = r.pos() as u64;
        let blocks = meta
            .try_slice(blocks_at, num_blocks.checked_mul(8).ok_or_else(|| corrupt("overflow"))?)
            .ok_or_else(|| corrupt("term block offsets truncated"))?;
        let meta_stream = meta
            .try_slice(blocks_at + num_blocks * 8, meta_len)
            .ok_or_else(|| corrupt("term metadata truncated"))?;
        let opts = postings.opts;
        Ok(TermsReader {
            key_prefixes,
            key_offsets,
            key_bytes,
            blocks,
            meta: meta_stream,
            num_blocks,
            stats: TermsStats { num_terms, sum_doc_freq, sum_total_term_freq, doc_count },
            min_term,
            max_term,
            has_freqs: opts.has_freqs(),
            has_pos: opts.has_positions(),
            postings,
            bloom: None,
        })
    }

    pub fn stats(&self) -> &TermsStats {
        &self.stats
    }

    /// Smallest term (empty if the field has no terms).
    pub fn min_term(&self) -> &[u8] {
        &self.min_term
    }

    /// Largest term.
    pub fn max_term(&self) -> &[u8] {
        &self.max_term
    }

    pub fn has_freqs(&self) -> bool {
        self.has_freqs
    }

    pub fn has_positions(&self) -> bool {
        self.has_pos
    }

    pub fn has_offsets(&self) -> bool {
        self.postings.opts.has_offsets()
    }

    pub fn has_payloads(&self) -> bool {
        self.postings.payloads
    }

    /// Exact lookup. Fields with a Bloom filter answer most misses without touching the index.
    pub fn seek_exact(&self, term: &[u8]) -> Option<TermState> {
        if let Some(b) = &self.bloom
            && !b.may_contain(term)
        {
            return None;
        }
        self.lookup(term)
    }

    /// [`seek_exact`](Self::seek_exact) with the term's [`hash64`](crate::bloom::hash64)
    /// computed once by a caller probing many segments.
    pub fn seek_exact_hashed(&self, term: &[u8], hash: u64) -> Option<TermState> {
        if let Some(b) = &self.bloom
            && !b.may_contain_hash(hash)
        {
            return None;
        }
        self.lookup(term)
    }

    fn lookup(&self, term: &[u8]) -> Option<TermState> {
        if self.num_blocks == 0 || term < self.min_term.as_slice() || term > self.max_term.as_slice() {
            return None;
        }
        let b = self.floor_block(term);
        let ord = self.scan_block_exact(b, term)?;
        Some(self.term_state(ord))
    }

    /// The field's Bloom filter, if it has one.
    pub fn bloom(&self) -> Option<&BloomFilter> {
        self.bloom.as_ref()
    }

    pub(crate) fn set_bloom(&mut self, b: BloomFilter) {
        self.bloom = Some(b);
    }

    /// Metadata of the term with ordinal `ord` (`ord < num_terms`).
    pub fn term_state(&self, ord: u64) -> TermState {
        let mut dec = MetaDecoder::default();
        dec.seek(self, ord);
        dec.state
    }

    /// Postings of a term.
    pub fn postings(&self, st: &TermState, flags: PostingsFlags) -> PostingsCursor {
        PostingsCursor::new(&self.postings, st, flags)
    }

    /// Repositions `cursor` on a term's postings, reusing its buffers (no allocation).
    pub fn postings_reset(&self, st: &TermState, flags: PostingsFlags, cursor: &mut PostingsCursor) {
        cursor.reset(&self.postings, st, flags);
    }

    /// All terms in order.
    pub fn iter(&self) -> TermsIter<'_> {
        TermsIter::new(self, None, Bound::Unbounded, Bound::Unbounded)
    }

    /// Terms within bounds, in order. `seek_ceil(t)` is `range(Included(t), Unbounded)`.
    pub fn range(&self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> TermsIter<'_> {
        TermsIter::new(self, None, lower, upper)
    }

    /// Iterator positioned before the smallest term `>= term`.
    pub fn seek_ceil(&self, term: &[u8]) -> TermsIter<'_> {
        self.range(Bound::Included(term), Bound::Unbounded)
    }

    /// Terms accepted by `aut`, in order.
    pub fn intersect<'a>(&'a self, aut: &'a dyn Automaton) -> TermsIter<'a> {
        TermsIter::new(self, Some(aut), Bound::Unbounded, Bound::Unbounded)
    }

    /// Terms accepted by `aut` within bounds.
    pub fn intersect_range<'a>(
        &'a self,
        aut: &'a dyn Automaton,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
    ) -> TermsIter<'a> {
        TermsIter::new(self, Some(aut), lower, upper)
    }

    fn block_offset(&self, block: u64) -> usize {
        let at = block as usize * 8;
        self.blocks
            .get(at..at + 8)
            .map_or(usize::MAX, |b| u64::from_le_bytes(b.try_into().unwrap()) as usize)
    }

    /// Parses block `b`'s metadata header; returns it and the `(start, end)` of the block's
    /// term entries.
    fn block_header(&self, b: u64) -> (MetaBlock, usize, usize) {
        let m = &self.meta[..];
        let mut p = self.block_offset(b).min(m.len());
        let mut h = MetaBlock {
            doc_fp: read_vu64(m, &mut p),
            pos_fp: read_vu64(m, &mut p),
            pay_fp: read_vu64(m, &mut p),
            ..MetaBlock::default()
        };
        for c in 0..5 {
            h.widths[c] = (m.get(p).copied().unwrap_or(0) as u32).min(64);
            p += 1;
        }
        h.row_bits = h.widths.iter().map(|&w| w as u64).sum();
        h.rows_len = read_vu32(m, &mut p) as usize;
        h.rows = p;
        let mut t = p.saturating_add(h.rows_len).saturating_add(8).min(m.len());
        let len = read_vu32(m, &mut t) as usize;
        let t = t.min(m.len());
        (h, t, t.saturating_add(len).min(m.len()))
    }

    /// `(start, end)` of block `b`'s term entries in `meta`.
    fn block_terms(&self, b: u64) -> (usize, usize) {
        let (_, s, e) = self.block_header(b);
        (s, e)
    }

    /// Full index key of block `b`.
    fn key(&self, b: usize) -> &[u8] {
        let o = |i: usize| u32::from_le_bytes(self.key_offsets[i * 4..i * 4 + 4].try_into().unwrap()) as usize;
        let (s, e) = (o(b).min(self.key_bytes.len()), o(b + 1).min(self.key_bytes.len()));
        &self.key_bytes[s..e.max(s)]
    }

    #[inline]
    fn prefix_at(&self, b: usize) -> u64 {
        u64::from_le_bytes(self.key_prefixes[b * 8..b * 8 + 8].try_into().unwrap())
    }

    /// The block whose index key is the largest `<= term` (the only block that can hold it).
    fn floor_block(&self, term: &[u8]) -> u64 {
        let n = self.num_blocks as usize;
        let tp = key_prefix(term);
        // Blocks whose 8-byte prefix is below `term`'s have smaller keys, above it larger ones;
        // only equal prefixes need the full keys.
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.prefix_at(mid) <= tp {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let le = lo; // blocks [0, le) have prefix <= tp
        if le == 0 {
            return 0;
        }
        if self.prefix_at(le - 1) < tp || self.key(le - 1) <= term {
            return (le - 1) as u64;
        }
        // Ties on the prefix: binary search the full keys among them.
        let (mut a, mut b) = (0usize, le);
        while a < b {
            let mid = (a + b) / 2;
            if self.prefix_at(mid) < tp {
                a = mid + 1;
            } else {
                b = mid;
            }
        }
        let (mut x, mut y) = (a, le);
        while x < y {
            let mid = (x + y) / 2;
            if self.key(mid) <= term {
                x = mid + 1;
            } else {
                y = mid;
            }
        }
        x.saturating_sub(1) as u64
    }

    /// Ordinal of `term` if block `b` holds it. Compares only the bytes each front-coded entry
    /// changes: `matched` is how much of `term` the previous entry shares.
    fn scan_block_exact(&self, b: u64, term: &[u8]) -> Option<u64> {
        let (mut p, end) = self.block_terms(b);
        let data = &self.meta[..end];
        let mut matched = 0usize;
        let mut i = 0u64;
        while p < end {
            let (shared, len) = read_entry(data, &mut p);
            let suffix = data.get(p..p.saturating_add(len))?;
            p += len;
            if i > 0 {
                match shared.cmp(&matched) {
                    // Diverges earlier than the previous entry did, upward: past `term`.
                    Ordering::Less => return None,
                    // Shares more with the previous entry, which was below `term`: still below.
                    Ordering::Greater => {
                        i += 1;
                        continue;
                    }
                    Ordering::Equal => {}
                }
            } else if shared != 0 {
                return None;
            }
            // Compare the suffix with the rest of `term`.
            let rest = term.get(matched..).unwrap_or(&[]);
            let k = common_prefix(suffix, rest);
            match (k == suffix.len(), k == rest.len()) {
                (true, true) => return Some(b * TERMS_PER_BLOCK + i),
                (true, false) => {}          // entry is a proper prefix of `term`: below
                (false, true) => return None, // `term` is a proper prefix of entry: above
                (false, false) if suffix[k] > rest[k] => return None,
                (false, false) => {}
            }
            matched += k;
            i += 1;
        }
        None
    }
}

/// Appends values of arbitrary widths (≤ 64 bits) to a little-endian bit stream.
#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    acc: u128,
    filled: u32,
}

impl BitWriter {
    fn put(&mut self, v: u64, bits: u32) {
        if bits == 0 {
            return;
        }
        self.acc |= ((v & if bits == 64 { u64::MAX } else { (1 << bits) - 1 }) as u128) << self.filled;
        self.filled += bits;
        while self.filled >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.filled -= 8;
        }
    }
    fn finish(mut self, dst: &mut Vec<u8>) {
        if self.filled > 0 {
            self.out.push(self.acc as u8);
        }
        dst.put_vu32(self.out.len() as u32);
        dst.extend_from_slice(&self.out);
        dst.extend_from_slice(&[0u8; 8]);
    }
}

/// Reads `bits` (≤ 64) at bit offset `bit`.
#[inline]
fn read_bits(data: &[u8], bit: u64, bits: u32) -> u64 {
    if bits == 0 {
        return 0;
    }
    let byte = (bit >> 3) as usize;
    let shift = (bit & 7) as u32;
    let word = match data.get(byte..byte.saturating_add(16)) {
        Some(b) => u128::from_le_bytes(b.try_into().unwrap()),
        None => {
            // Near the end (rows are followed by 8 padding bytes) or corrupt: zero-fill.
            let mut tmp = [0u8; 16];
            if let Some(rest) = data.get(byte..) {
                tmp[..rest.len().min(16)].copy_from_slice(&rest[..rest.len().min(16)]);
            }
            u128::from_le_bytes(tmp)
        }
    };
    let v = word >> shift;
    (v as u64) & if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 }
}

/// Parsed metadata header of one block.
#[derive(Clone, Copy, Default)]
struct MetaBlock {
    doc_fp: u64,
    pos_fp: u64,
    pay_fp: u64,
    widths: [u32; 5],
    row_bits: u64,
    rows: usize,
    rows_len: usize,
}

/// Term metadata lookups, caching the last block header.
#[derive(Default)]
struct MetaDecoder {
    block: Option<u64>,
    header: MetaBlock,
    /// Ordinal decoded in `state`, if any.
    ord: Option<u64>,
    state: TermState,
}

impl MetaDecoder {
    fn seek(&mut self, r: &TermsReader, ord: u64) {
        if self.ord == Some(ord) {
            return;
        }
        let block = ord / TERMS_PER_BLOCK;
        if block >= r.num_blocks {
            self.state = TermState::default();
            self.ord = None;
            return;
        }
        if self.block != Some(block) {
            self.header = r.block_header(block).0;
            self.block = Some(block);
        }
        let h = &self.header;
        let m = &r.meta[..];
        let data = &m[h.rows.min(m.len())..(h.rows.saturating_add(h.rows_len).saturating_add(8)).min(m.len())];
        let mut bit = (ord % TERMS_PER_BLOCK) * h.row_bits;
        let mut col = [0u64; 5];
        for (c, v) in col.iter_mut().enumerate() {
            *v = read_bits(data, bit, h.widths[c]);
            bit += h.widths[c] as u64;
        }
        let df = col[0] as u32;
        self.state = TermState {
            ord,
            doc_freq: df,
            total_term_freq: df as u64 + col[1],
            doc_fp: if df == 1 { h.doc_fp } else { h.doc_fp.wrapping_add(col[2]) },
            pos_fp: h.pos_fp.wrapping_add(col[3]),
            pay_fp: h.pay_fp.wrapping_add(col[4]),
            singleton: if df == 1 { col[2] as u32 } else { 0 },
        };
        self.ord = Some(ord);
    }
}

/// An ordered term iterator (lending: the term is valid until the next call), optionally
/// restricted to an automaton and bounds.
pub struct TermsIter<'a> {
    reader: &'a TermsReader,
    aut: Option<&'a dyn Automaton>,
    upper: Bound<Vec<u8>>,
    /// Block of the current term, the byte offset of the next entry and the end of the block's
    /// entries.
    block: u64,
    pos: usize,
    end: usize,
    /// Index in the block of the current term (`u64::MAX` before the first of the block).
    idx: u64,
    term: Vec<u8>,
    /// Bytes the current term shares with the previously decoded one.
    shared: usize,
    /// The current term was positioned by a seek and not returned yet.
    pending: bool,
    started: bool,
    valid: bool,
    done: bool,
    /// `states[i]`: automaton state after `term[..i]` (only the live prefix is kept).
    states: Vec<u32>,
    dead_run: u32,
    scratch: Vec<u8>,
    dec: MetaDecoder,
}

/// Dead terms walked sequentially before seeking past their prefix: the next entries usually
/// sit in the same block, and decoding one is far cheaper than a seek.
const DEAD_RUN_BEFORE_SEEK: u32 = 4;

impl<'a> TermsIter<'a> {
    fn new(
        reader: &'a TermsReader,
        aut: Option<&'a dyn Automaton>,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
    ) -> TermsIter<'a> {
        // An automaton that accepts everything needs no per-term work.
        let aut = aut.filter(|a| !a.will_always_match(a.start()));
        let mut it = TermsIter {
            reader,
            aut,
            upper: match upper {
                Bound::Included(u) => Bound::Included(u.to_vec()),
                Bound::Excluded(u) => Bound::Excluded(u.to_vec()),
                Bound::Unbounded => Bound::Unbounded,
            },
            block: 0,
            pos: 0,
            end: 0,
            idx: u64::MAX,
            term: Vec::new(),
            shared: 0,
            pending: false,
            started: false,
            valid: false,
            done: reader.num_blocks == 0,
            states: Vec::new(),
            dead_run: 0,
            scratch: Vec::new(),
            dec: MetaDecoder::default(),
        };
        if let Some(a) = aut {
            if !a.can_match(a.start()) {
                it.done = true;
                return it;
            }
            it.states.push(a.start());
        }
        // Start at the larger of the lower bound and the automaton's literal prefix.
        let mut start: Option<(Vec<u8>, bool)> = match lower {
            Bound::Included(t) => Some((t.to_vec(), false)),
            Bound::Excluded(t) => Some((t.to_vec(), true)),
            Bound::Unbounded => None,
        };
        if let Some(a) = aut {
            let lit = literal_prefix(a);
            if !lit.is_empty() && start.as_ref().is_none_or(|(s, _)| s.as_slice() < lit.as_slice()) {
                start = Some((lit, false));
            }
        }
        if !it.done
            && let Some((t, exclusive)) = start
        {
            it.seek_to(&t);
            if exclusive && it.pending && it.term == t {
                it.pending = false;
            }
        }
        it
    }

    /// Loads block `b` with its first entry decoded as the current term (skipping blocks with
    /// no entries, which only corrupt data has); false and done past the last block.
    fn load_block(&mut self, mut b: u64) -> bool {
        while b < self.reader.num_blocks {
            let (start, end) = self.reader.block_terms(b);
            self.block = b;
            self.pos = start;
            self.end = end;
            self.idx = u64::MAX;
            if self.decode_next() {
                return true;
            }
            b += 1;
        }
        self.done = true;
        false
    }

    /// Decodes the next entry of the current block into `term`.
    #[inline]
    fn decode_next(&mut self) -> bool {
        if self.pos >= self.end || self.idx.wrapping_add(1) >= TERMS_PER_BLOCK {
            return false;
        }
        let data = &self.reader.meta[..self.end];
        let (shared, len) = read_entry(data, &mut self.pos);
        let at = self.pos.min(self.end);
        let suffix = &data[at..at.saturating_add(len).min(self.end)];
        self.pos = at + suffix.len();
        if self.idx == u64::MAX {
            // A block's first term is stored whole: share what it has in common with the
            // previous one so automaton states can be reused across blocks.
            self.shared = if self.started { common_prefix(&self.term, suffix) } else { 0 };
            self.term.clear();
            self.term.extend_from_slice(suffix);
        } else {
            let shared = shared.min(self.term.len());
            self.shared = shared;
            self.term.truncate(shared);
            self.term.extend_from_slice(suffix);
        }
        self.idx = self.idx.wrapping_add(1);
        self.started = true;
        true
    }

    /// Moves to the next term in order; false at the end.
    #[inline]
    fn step(&mut self) -> bool {
        if self.done {
            return false;
        }
        if !self.started {
            return self.load_block(0);
        }
        if self.decode_next() {
            return true;
        }
        let next = self.block + 1;
        self.load_block(next)
    }

    /// Positions on the smallest term `>= target` (left pending for the next `next`).
    fn seek_to(&mut self, target: &[u8]) {
        let r = self.reader;
        if r.num_blocks == 0 {
            self.done = true;
            return;
        }
        let b = if target <= r.min_term.as_slice() { 0 } else { r.floor_block(target) };
        // Inside the current block and ahead of the current term: keep decoding from here.
        let here = self.started && !self.done && b == self.block && self.term.as_slice() < target;
        if !here {
            self.started = false;
            if !self.load_block(b) {
                return;
            }
        } else if !self.step() {
            return;
        }
        self.shared = 0;
        loop {
            if self.term.as_slice() >= target {
                self.pending = true;
                return;
            }
            if !self.step() {
                return;
            }
        }
    }

    /// Advances to the next term (accepted by the automaton and within bounds, if any).
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<&[u8]> {
        loop {
            if self.pending {
                self.pending = false;
                if self.done {
                    break;
                }
            } else if !self.step() {
                break;
            }
            match &self.upper {
                Bound::Included(u) if self.term.as_slice() > u.as_slice() => break,
                Bound::Excluded(u) if self.term.as_slice() >= u.as_slice() => break,
                _ => {}
            }
            let Some(aut) = self.aut else {
                self.valid = true;
                return Some(&self.term);
            };
            match self.run_automaton(aut) {
                Run::Match => {
                    self.dead_run = 0;
                    self.valid = true;
                    return Some(&self.term);
                }
                Run::NoMatch => {}
                Run::Dead(k) => {
                    self.dead_run += 1;
                    if self.dead_run >= DEAD_RUN_BEFORE_SEEK || self.pos >= self.end {
                        self.dead_run = 0;
                        // Every term starting with term[..=k] is rejected: jump to the next
                        // string the automaton could still accept.
                        if !next_candidate(aut, &self.states, &self.term, k, &mut self.scratch) {
                            break;
                        }
                        let target = std::mem::take(&mut self.scratch);
                        let before = self.ord();
                        self.seek_to(&target);
                        self.scratch = target;
                        self.states.truncate(1);
                        // The target sorts after the current term, so a sound dictionary always
                        // seeks forward; going back means corrupt blocks, which must not loop.
                        if !self.done && self.ord() <= before {
                            break;
                        }
                    }
                }
            }
        }
        self.done = true;
        self.valid = false;
        None
    }

    /// Feeds the current term to the automaton, reusing the states of its shared prefix.
    #[inline]
    fn run_automaton(&mut self, aut: &dyn Automaton) -> Run {
        let keep = self.shared.min(self.states.len() - 1) + 1;
        self.states.truncate(keep);
        loop {
            let depth = self.states.len() - 1;
            let s = self.states[depth];
            if aut.will_always_match(s) {
                return Run::Match;
            }
            if depth == self.term.len() {
                return if aut.is_match(s) { Run::Match } else { Run::NoMatch };
            }
            let n = aut.accept(s, self.term[depth]);
            if !aut.can_match(n) {
                return Run::Dead(depth);
            }
            self.states.push(n);
        }
    }

    /// The current term.
    pub fn term(&self) -> &[u8] {
        &self.term
    }

    /// Ordinal of the current term.
    pub fn ord(&self) -> u64 {
        self.block * TERMS_PER_BLOCK + self.idx.min(TERMS_PER_BLOCK - 1)
    }

    /// Metadata of the current term.
    pub fn state(&mut self) -> TermState {
        if !self.valid {
            return TermState::default();
        }
        let ord = self.ord();
        self.dec.seek(self.reader, ord);
        self.dec.state
    }

    pub fn doc_freq(&mut self) -> u32 {
        self.state().doc_freq
    }

    pub fn total_term_freq(&mut self) -> u64 {
        self.state().total_term_freq
    }

    /// Postings of the current term.
    pub fn postings(&mut self, flags: PostingsFlags) -> PostingsCursor {
        let st = self.state();
        self.reader.postings(&st, flags)
    }

    /// Repositions `cursor` on the current term's postings, reusing its buffers.
    pub fn postings_reset(&mut self, flags: PostingsFlags, cursor: &mut PostingsCursor) {
        let st = self.state();
        self.reader.postings_reset(&st, flags, cursor);
    }
}

enum Run {
    Match,
    NoMatch,
    /// `term[depth]` leads from the live state `states[depth]` to a dead one.
    Dead(usize),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NO_MORE_DOCS;
    use crate::automaton::{AlwaysMatch, Dfa, Prefix, literal_prefix, next_candidate};
    use crate::postings::PostingsWriter;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    fn build(terms: &[(&str, Vec<u32>)], opts: IndexOptions) -> TermsReader {
        let mut pw = PostingsWriter::new(opts, false, None);
        let mut tw = TermsWriter::new(opts, false);
        for (t, docs) in terms {
            pw.start_term();
            for &d in docs {
                pw.start_doc(d, 2);
                for p in 0..2 {
                    pw.add_position(p, 0, 0, &[]);
                }
                pw.finish_doc();
            }
            let st = pw.finish_term();
            tw.add(t.as_bytes(), &st).unwrap();
        }
        let (fst, meta) = tw.finish(10).unwrap();
        let data = PostingsData {
            docs: Slice::from(pw.docs),
            pos: Slice::from(pw.pos),
            pay: Slice::from(pw.pay),
            opts,
            payloads: false,
        };
        TermsReader::open(Slice::from(fst), Slice::from(meta), data).unwrap()
    }

    fn collect(mut it: TermsIter<'_>) -> Vec<String> {
        let mut v = Vec::new();
        while let Some(t) = it.next() {
            v.push(String::from_utf8(t.to_vec()).unwrap());
        }
        v
    }

    #[test]
    fn seek_iterate_intersect() {
        let mut terms: Vec<(String, Vec<u32>)> = Vec::new();
        for i in 0..1000u32 {
            let docs: Vec<u32> = (0..(i % 7 + 1)).map(|k| k * 3 + i % 3).collect();
            terms.push((format!("t{i:04}"), docs));
        }
        terms.push(("zebra".into(), vec![1, 5]));
        let refs: Vec<(&str, Vec<u32>)> =
            terms.iter().map(|(t, d)| (t.as_str(), d.clone())).collect();
        let r = build(&refs, IndexOptions::DocsAndFreqsAndPositions);
        assert_eq!(r.stats().num_terms, 1001);
        assert_eq!(r.min_term(), b"t0000");
        assert_eq!(r.max_term(), b"zebra");
        assert_eq!(
            r.stats().sum_doc_freq,
            terms.iter().map(|t| t.1.len() as u64).sum::<u64>()
        );
        assert_eq!(r.stats().sum_total_term_freq, r.stats().sum_doc_freq * 2);
        for (i, (t, docs)) in terms.iter().enumerate() {
            let st = r.seek_exact(t.as_bytes()).unwrap();
            assert_eq!(st.ord, i as u64);
            assert_eq!(st.doc_freq as usize, docs.len());
            let mut c = r.postings(&st, PostingsFlags::POSITIONS);
            for &d in docs {
                assert_eq!(c.next(), d);
                assert_eq!(c.freq(), 2);
                assert_eq!(c.next_position(), 0);
                assert_eq!(c.next_position(), 1);
            }
            assert_eq!(c.next(), NO_MORE_DOCS);
        }
        assert!(r.seek_exact(b"t").is_none());
        // Ordered iteration with metadata.
        let mut it = r.iter();
        let mut n = 0;
        while let Some(t) = it.next() {
            assert_eq!(t, terms[n].0.as_bytes());
            assert_eq!(it.ord(), n as u64);
            assert_eq!(it.state().doc_freq as usize, terms[n].1.len());
            n += 1;
        }
        assert_eq!(n, terms.len());
        // seek_ceil
        let mut it = r.seek_ceil(b"t0500x");
        assert_eq!(it.next(), Some(&b"t0501"[..]));
        assert_eq!(it.state().ord, 501);
        let mut it = r.seek_ceil(b"zz");
        assert!(it.next().is_none());
        // ranges
        let v = collect(r.range(Bound::Excluded(b"t0997"), Bound::Included(b"zebra")));
        assert_eq!(v, ["t0998", "t0999", "zebra"]);
        // automata
        let p = Prefix::new("t099");
        assert_eq!(collect(r.intersect(&p)).len(), 10);
        let d = Dfa::wildcard("t0?00", false, 10_000).unwrap();
        assert_eq!(
            collect(r.intersect(&d)),
            [
                "t0000", "t0100", "t0200", "t0300", "t0400", "t0500", "t0600", "t0700", "t0800",
                "t0900"
            ]
        );
        let f = Dfa::levenshtein("zebar", 1, 0, true);
        assert_eq!(collect(r.intersect(&f)), ["zebra"]);
        let f = Dfa::levenshtein("zebar", 1, 0, false);
        assert!(collect(r.intersect(&f)).is_empty());
        // Random-order state lookups agree with sequential ones.
        for ord in [1000u64, 0, 33, 32, 31, 999, 64] {
            assert_eq!(r.term_state(ord).ord, ord);
        }
    }

    #[test]
    #[ignore]
    fn bench_seek_latency() {
        let n = 1_000_000u32;
        let mut pw = PostingsWriter::new(IndexOptions::DocsAndFreqs, false, None);
        let mut tw = TermsWriter::new(IndexOptions::DocsAndFreqs, false);
        let terms: Vec<String> = (0..n).map(|i| format!("term{:08x}", i.wrapping_mul(2654435761))).collect();
        let mut sorted = terms.clone();
        sorted.sort();
        for t in &sorted {
            pw.start_term();
            pw.start_doc(1, 1);
            pw.finish_doc();
            pw.start_doc(7, 2);
            pw.finish_doc();
            let st = pw.finish_term();
            tw.add(t.as_bytes(), &st).unwrap();
        }
        let (fst, meta) = tw.finish(10).unwrap();
        let data = PostingsData {
            docs: Slice::from(pw.docs),
            pos: Slice::empty(),
            pay: Slice::empty(),
            opts: IndexOptions::DocsAndFreqs,
            payloads: false,
        };
        println!("terms index {} bytes, blocks+meta {} bytes for {n} terms", fst.len(), meta.len());
        let r = TermsReader::open(Slice::from(fst), Slice::from(meta), data).unwrap();
        let t0 = std::time::Instant::now();
        let mut found = 0;
        for t in terms.iter().take(200_000) {
            found += r.seek_exact(t.as_bytes()).is_some() as u32;
        }
        let e = t0.elapsed();
        println!("seek_exact: {:.0} ns/op ({found})", e.as_nanos() as f64 / 200_000.0);
        let misses: Vec<String> = (0..200_000u32).map(|i| format!("term{:08x}", i.wrapping_mul(2246822519) ^ 0x5bd1e995)).collect();
        let t0 = std::time::Instant::now();
        let mut found = 0;
        for t in &misses {
            found += r.seek_exact(t.as_bytes()).is_some() as u32;
        }
        println!("seek_exact (mostly misses): {:.0} ns/op ({found} found)", t0.elapsed().as_nanos() as f64 / 200_000.0);
        let mut bb = crate::bloom::BloomBuilder::new(n as u64);
        for t in &sorted {
            bb.add(t.as_bytes());
        }
        let mut rb = r.clone();
        rb.set_bloom(crate::bloom::BloomFilter::open(Slice::from(bb.finish().unwrap())).unwrap());
        let t0 = std::time::Instant::now();
        let mut found = 0;
        for t in &misses {
            found += rb.seek_exact(t.as_bytes()).is_some() as u32;
        }
        println!("seek_exact with bloom (mostly misses): {:.0} ns/op ({found} found)", t0.elapsed().as_nanos() as f64 / 200_000.0);
        let t0 = std::time::Instant::now();
        let mut found = 0;
        for t in terms.iter().take(200_000) {
            found += rb.seek_exact(t.as_bytes()).is_some() as u32;
        }
        println!("seek_exact with bloom (hits): {:.0} ns/op ({found})", t0.elapsed().as_nanos() as f64 / 200_000.0);
        let t0 = std::time::Instant::now();
        let mut s = 0u64;
        for t in terms.iter().take(200_000) {
            s += r.floor_block(t.as_bytes());
        }
        println!("floor_block only: {:.0} ns/op ({s})", t0.elapsed().as_nanos() as f64 / 200_000.0);
        let t0 = std::time::Instant::now();
        let mut s = 0u64;
        for i in 0..200_000u64 {
            s += r.term_state((i * 7919) % n as u64).doc_fp;
        }
        println!("term_state only: {:.0} ns/op ({s})", t0.elapsed().as_nanos() as f64 / 200_000.0);
        let t0 = std::time::Instant::now();
        let mut it = r.iter();
        let mut c = 0u64;
        while it.next().is_some() {
            c += it.state().doc_freq as u64;
        }
        println!("iterate+state: {:.0} ns/term ({c})", t0.elapsed().as_nanos() as f64 / n as f64);
        let t0 = std::time::Instant::now();
        let mut it = r.iter();
        let mut c = 0u64;
        while let Some(t) = it.next() {
            c += t.len() as u64;
        }
        println!("iterate only: {:.0} ns/term ({c})", t0.elapsed().as_nanos() as f64 / n as f64);
        let p = crate::automaton::Prefix::new(&b"term1"[..]);
        let t0 = std::time::Instant::now();
        let mut it = r.intersect(&p);
        let mut c = 0u64;
        while it.next().is_some() {
            c += 1;
        }
        println!("prefix intersect: {:.0} ns/term ({c} terms)", t0.elapsed().as_nanos() as f64 / c.max(1) as f64);
    }

    fn build_set(terms: &std::collections::BTreeSet<Vec<u8>>) -> TermsReader {
        let opts = IndexOptions::Docs;
        let mut pw = PostingsWriter::new(opts, false, None);
        let mut tw = TermsWriter::new(opts, false);
        for (i, t) in terms.iter().enumerate() {
            pw.start_term();
            pw.start_doc(i as u32 % 50, 1);
            pw.finish_doc();
            if i % 3 == 0 {
                pw.start_doc(60 + i as u32 % 7, 1);
                pw.finish_doc();
            }
            let st = pw.finish_term();
            tw.add(t, &st).unwrap();
        }
        let (fst, meta) = tw.finish(100).unwrap();
        let data = PostingsData {
            docs: Slice::from(pw.docs),
            pos: Slice::empty(),
            pay: Slice::empty(),
            opts,
            payloads: false,
        };
        TermsReader::open(Slice::from(fst), Slice::from(meta), data).unwrap()
    }

    fn random_term(rng: &mut StdRng) -> Vec<u8> {
        let alphabet: &[u8] = match rng.gen_range(0..4) {
            0 => b"ab",
            1 => b"abcxyz",
            2 => &[0, 1, 0x7f, 0x80, 0xfe, 0xff],
            _ => b"abcdefghijklmnopqrstuvwxyz0123456789",
        };
        let len = if rng.gen_bool(0.3) { rng.gen_range(0..4) } else { rng.gen_range(0..24) };
        (0..len).map(|_| alphabet[rng.gen_range(0..alphabet.len())]).collect()
    }

    fn collect_bytes(mut it: TermsIter<'_>) -> Vec<(Vec<u8>, u64)> {
        let mut v = Vec::new();
        while let Some(t) = it.next() {
            let t = t.to_vec();
            v.push((t, it.ord()));
            assert_eq!(it.state().ord, it.ord());
        }
        assert!(it.next().is_none(), "iterators stay exhausted");
        v
    }

    /// The block dictionary against a `BTreeSet` model: exact and ceiling seeks, ordered
    /// iteration, every bound combination and automaton intersections (prefix, wildcard,
    /// regexp, fuzzy) on random term sets around the block size, with binary and empty terms.
    #[test]
    fn block_dictionary_matches_model() {
        use std::collections::BTreeSet;
        let mut rng = StdRng::seed_from_u64(11);
        for (size, long_prefix) in [(0usize, false), (1, false), (2, false), (31, false), (32, false), (33, false), (64, true), (65, false), (700, true), (4000, false), (4000, true)] {
            let mut set = BTreeSet::new();
            while set.len() < size {
                let mut t = random_term(&mut rng);
                // URL-like keys share more than the index's 8-byte integer prefixes.
                if long_prefix && rng.gen_bool(0.8) {
                    let mut u = b"https://example.org/".to_vec();
                    u.extend_from_slice(&t);
                    t = u;
                }
                set.insert(t);
            }
            let r = build_set(&set);
            let all: Vec<Vec<u8>> = set.iter().cloned().collect();
            assert_eq!(r.stats().num_terms as usize, size);
            assert_eq!(collect_bytes(r.iter()).into_iter().map(|x| x.0).collect::<Vec<_>>(), all);
            for (i, t) in all.iter().enumerate() {
                let st = r.seek_exact(t).unwrap_or_else(|| panic!("{t:?} missing (size {size})"));
                assert_eq!(st.ord, i as u64);
                assert_eq!(r.term_state(i as u64), st);
            }
            let mut probes: Vec<Vec<u8>> = (0..300).map(|_| random_term(&mut rng)).collect();
            probes.extend(all.iter().take(50).map(|t| {
                let mut t = t.clone();
                t.push(0);
                t
            }));
            probes.push(Vec::new());
            probes.push(vec![0xff; 30]);
            for p in &probes {
                let want = all.binary_search(p).ok();
                assert_eq!(r.seek_exact(p).map(|s| s.ord as usize), want, "seek_exact {p:?}");
                let ceil = all.partition_point(|t| t < p);
                let mut it = r.seek_ceil(p);
                assert_eq!(it.next().map(|t| t.to_vec()), all.get(ceil).cloned(), "seek_ceil {p:?}");
                if ceil < all.len() {
                    assert_eq!(it.ord(), ceil as u64);
                }
            }
            for _ in 0..40 {
                let a = &probes[rng.gen_range(0..probes.len())];
                let b = &probes[rng.gen_range(0..probes.len())];
                let bounds = [
                    (Bound::Included(&a[..]), Bound::Excluded(&b[..])),
                    (Bound::Excluded(&a[..]), Bound::Included(&b[..])),
                    (Bound::Unbounded, Bound::Included(&b[..])),
                    (Bound::Excluded(&a[..]), Bound::Unbounded),
                ];
                for (lo, hi) in bounds {
                    let inside = |t: &[u8]| {
                        (match lo {
                            Bound::Included(l) => t >= l,
                            Bound::Excluded(l) => t > l,
                            Bound::Unbounded => true,
                        }) && (match hi {
                            Bound::Included(h) => t <= h,
                            Bound::Excluded(h) => t < h,
                            Bound::Unbounded => true,
                        })
                    };
                    let want: Vec<Vec<u8>> = all.iter().filter(|t| inside(t)).cloned().collect();
                    let got: Vec<Vec<u8>> = collect_bytes(r.range(lo, hi)).into_iter().map(|x| x.0).collect();
                    assert_eq!(got, want, "range {lo:?} {hi:?}");
                }
            }
            // Automata.
            let mut auts: Vec<Box<dyn Automaton>> = vec![
                Box::new(Prefix::new(&b"a"[..])),
                Box::new(Prefix::new(&b"abc"[..])),
                Box::new(Prefix::new(&[0xffu8][..])),
                Box::new(Prefix::new(&b""[..])),
                Box::new(Dfa::wildcard("a*b", false, 10_000).unwrap()),
                Box::new(Dfa::wildcard("?c*", false, 10_000).unwrap()),
                Box::new(Dfa::wildcard("*z", false, 10_000).unwrap()),
                Box::new(crate::automaton::regexp::compile("[a-c]+x?", crate::automaton::regexp::ALL, false, 10_000).unwrap()),
                Box::new(crate::automaton::regexp::compile("(ab)*", crate::automaton::regexp::ALL, false, 10_000).unwrap()),
                Box::new(crate::automaton::regexp::compile("abc.*", crate::automaton::regexp::ALL, false, 10_000).unwrap()),
            ];
            for t in all.iter().filter(|t| std::str::from_utf8(t).is_ok() && !t.is_empty()).take(4) {
                let s = std::str::from_utf8(t).unwrap();
                auts.push(Box::new(Dfa::levenshtein(s, 1, 0, true)));
                auts.push(Box::new(Dfa::levenshtein(s, 2, 1, false)));
            }
            for aut in &auts {
                let want: Vec<(Vec<u8>, u64)> = all
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| aut.matches(t))
                    .map(|(i, t)| (t.clone(), i as u64))
                    .collect();
                assert_eq!(collect_bytes(r.intersect(aut.as_ref())), want);
                let lo = &probes[rng.gen_range(0..probes.len())];
                let hi = &probes[rng.gen_range(0..probes.len())];
                let want: Vec<(Vec<u8>, u64)> = want
                    .into_iter()
                    .filter(|(t, _)| t.as_slice() >= lo.as_slice() && t.as_slice() < hi.as_slice())
                    .collect();
                assert_eq!(collect_bytes(r.intersect_range(aut.as_ref(), Bound::Included(lo), Bound::Excluded(hi))), want);
            }
        }
    }

    #[test]
    fn literal_prefixes_and_candidates() {
        assert_eq!(literal_prefix(&Prefix::new(&b"foo"[..])), b"foo");
        assert_eq!(literal_prefix(&Dfa::wildcard("foo*bar", false, 10_000).unwrap()), b"foo");
        assert_eq!(literal_prefix(&Dfa::wildcard("*bar", false, 10_000).unwrap()), b"");
        assert_eq!(literal_prefix(&AlwaysMatch), b"");
        let exact = crate::automaton::regexp::compile("hello", crate::automaton::regexp::ALL, false, 10_000).unwrap();
        assert_eq!(literal_prefix(&exact), b"hello");
        // "fop" dies at 'p' after "fo"; nothing after "fo" or "f" can match "foo": done.
        let p = Prefix::new(&b"foo"[..]);
        let mut out = Vec::new();
        assert!(!next_candidate(&p, &[0, 1, 2], b"fop", 2, &mut out));
        // "fa" dies at 'a' after "f": the next candidate is "fo".
        assert!(next_candidate(&p, &[0, 1], b"fa", 1, &mut out));
        assert_eq!(out, b"fo");
    }

    /// A cursor reset onto other terms (with other flags) reads exactly what a fresh one does.
    #[test]
    fn reset_cursors_match_fresh_ones() {
        let mut terms: Vec<(String, Vec<u32>)> = Vec::new();
        for i in 0..300u32 {
            let docs: Vec<u32> = (0..(i * 7 % 400 + 1)).map(|k| k * 3 + i % 3).collect();
            terms.push((format!("t{i:04}"), docs));
        }
        let refs: Vec<(&str, Vec<u32>)> = terms.iter().map(|(t, d)| (t.as_str(), d.clone())).collect();
        let r = build(&refs, IndexOptions::DocsAndFreqsAndPositions);
        let other = build(&refs[..50], IndexOptions::DocsAndFreqs);
        let read = |c: &mut PostingsCursor| {
            let mut v = Vec::new();
            loop {
                let d = c.next();
                if d == NO_MORE_DOCS {
                    break;
                }
                let f = c.freq();
                let p = if f > 0 && c.next_position() != u32::MAX { 1 } else { 0 };
                v.push((d, f, p));
            }
            v
        };
        let mut reused = PostingsCursor::empty();
        let mut it = r.iter();
        let mut n = 0;
        while it.next().is_some() {
            let flags = if n % 2 == 0 { PostingsFlags::POSITIONS } else { PostingsFlags::FREQS };
            let mut fresh = it.postings(flags);
            it.postings_reset(flags, &mut reused);
            // Interleave impacts to dirty the recycled buffers.
            reused.advance_shallow(0);
            let _ = reused.impacts(0).len();
            assert_eq!(read(&mut reused), read(&mut fresh), "term {n}");
            if n % 10 == 0 {
                let st = other.term_state((n / 10) as u64 % 50);
                other.postings_reset(&st, PostingsFlags::FREQS, &mut reused);
                assert_eq!(read(&mut reused), read(&mut other.postings(&st, PostingsFlags::FREQS)));
            }
            n += 1;
        }
        assert_eq!(n, 300);
    }

    #[test]
    fn rejects_unsorted_terms() {
        let mut tw = TermsWriter::new(IndexOptions::Docs, false);
        let st = TermState {
            doc_freq: 1,
            total_term_freq: 1,
            ..Default::default()
        };
        tw.add(b"b", &st).unwrap();
        assert!(tw.add(b"a", &st).is_err());
        assert!(tw.add(b"b", &st).is_err());
    }

    #[test]
    fn empty_field() {
        let r = build(&[], IndexOptions::Docs);
        assert_eq!(r.stats().num_terms, 0);
        assert!(r.iter().next().is_none());
        assert!(r.seek_exact(b"").is_none());
    }
}
