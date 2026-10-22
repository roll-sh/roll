//! `DocSet`: an immutable set of doc ids with O(1) rank, used wherever a structure only covers
//! some documents (sparse norms, doc values, vectors).
//!
//! The doc id space is cut into blocks of 65 536 docs (like Lucene's `IndexedDISI`), and each
//! block picks the smaller representation: absent, all docs, a sorted `u16` list (fewer than
//! 4 096 docs), or an 8 KiB bitset with a rank table (one `u16` per 512 bits). A fixed-size block
//! table records each block's kind, data offset and the number of set docs before it, so
//! `rank(doc)` (the index of `doc` among the set's docs, which is how values are addressed) is a
//! table lookup plus a short binary search or one popcount over at most 8 words — no forward-only
//! iteration state is needed for random access. A set covering every doc (or none) stores no
//! blocks at all.
//!
//! [`DocSetCursor`] adds stateful `next`/`advance` for iterating doc values in doc order.

use crate::NO_MORE_DOCS;
use crate::error::{Result, corrupt};
use crate::io::{In, Out};
use crate::slice::Slice;

const BLOCK_SHIFT: u32 = 16;
const BLOCK_DOCS: u32 = 1 << BLOCK_SHIFT;
const DENSE_MIN: u32 = 4096;
const WORDS: usize = (BLOCK_DOCS / 64) as usize;
const RANK_EVERY_WORDS: usize = 8;
const DENSE_BYTES: usize = WORDS * 8 + (WORDS / RANK_EVERY_WORDS) * 2;
const ENTRY: usize = 16;

const KIND_EMPTY: u32 = 0;
const KIND_ALL: u32 = 1;
const KIND_SPARSE: u32 = 2;
const KIND_DENSE: u32 = 3;

/// Writes the set of `docs` (strictly ascending, all `< max_doc`). Returns the doc count.
pub fn write_docset<O: Out + ?Sized>(docs: &[u32], max_doc: u32, out: &mut O) -> u32 {
    debug_assert!(docs.windows(2).all(|w| w[0] < w[1]));
    let nblocks = max_doc.div_ceil(BLOCK_DOCS) as usize;
    // Block b covers docs[bounds[b]..bounds[b + 1]].
    let mut bounds = Vec::with_capacity(nblocks + 1);
    bounds.push(0usize);
    for b in 0..nblocks {
        let end_doc = ((b as u64 + 1) << BLOCK_SHIFT).min(max_doc as u64) as u32;
        let at = *bounds.last().unwrap();
        bounds.push(at + docs[at..].partition_point(|&d| d < end_doc));
    }
    let counts: Vec<u32> = bounds.windows(2).map(|w| (w[1] - w[0]) as u32).collect();
    write_blocks(max_doc, docs.len() as u32, &counts, out, |b, kind, out| {
        let base = (b as u32) << BLOCK_SHIFT;
        let block_docs = &docs[bounds[b]..bounds[b + 1]];
        if kind == KIND_SPARSE {
            for &d in block_docs {
                out.put_u16((d - base) as u16);
            }
        } else {
            let mut words = [0u64; WORDS];
            for &d in block_docs {
                let o = (d - base) as usize;
                words[o >> 6] |= 1 << (o & 63);
            }
            write_dense(&words, out);
        }
    })
}

/// [`write_docset`] from a bitset (`bits` covers `max_doc` bits; bits past it must be clear).
pub(crate) fn write_docset_bits<O: Out + ?Sized>(bits: &[u64], max_doc: u32, out: &mut O) -> u32 {
    let nblocks = max_doc.div_ceil(BLOCK_DOCS) as usize;
    let block_words = |b: usize| {
        let lo = (b * WORDS).min(bits.len());
        &bits[lo..((b + 1) * WORDS).min(bits.len())]
    };
    let counts: Vec<u32> = (0..nblocks)
        .map(|b| block_words(b).iter().map(|w| w.count_ones()).sum())
        .collect();
    let count = counts.iter().sum();
    write_blocks(max_doc, count, &counts, out, |b, kind, out| {
        let ws = block_words(b);
        if kind == KIND_SPARSE {
            for (i, &w) in ws.iter().enumerate() {
                let mut w = w;
                while w != 0 {
                    out.put_u16((i * 64 + w.trailing_zeros() as usize) as u16);
                    w &= w - 1;
                }
            }
        } else {
            let mut words = [0u64; WORDS];
            words[..ws.len()].copy_from_slice(ws);
            write_dense(&words, out);
        }
    })
}

/// A dense block: the bitset, then the number of set bits before every 8th word.
fn write_dense<O: Out + ?Sized>(words: &[u64; WORDS], out: &mut O) {
    let mut buf = [0u8; DENSE_BYTES];
    for (i, w) in words.iter().enumerate() {
        buf[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    let mut acc = 0u32;
    for g in 0..WORDS / RANK_EVERY_WORDS {
        let at = WORDS * 8 + g * 2;
        buf[at..at + 2].copy_from_slice(&(acc as u16).to_le_bytes());
        for w in &words[g * RANK_EVERY_WORDS..(g + 1) * RANK_EVERY_WORDS] {
            acc += w.count_ones();
        }
    }
    out.put_bytes(&buf);
}

fn block_kind(b: usize, n: u32, max_doc: u32) -> u32 {
    let base = (b as u32) << BLOCK_SHIFT;
    let end_doc = base.saturating_add(BLOCK_DOCS).min(max_doc);
    if n == 0 {
        KIND_EMPTY
    } else if n == end_doc - base {
        KIND_ALL
    } else if n < DENSE_MIN {
        KIND_SPARSE
    } else {
        KIND_DENSE
    }
}

/// The layout shared by both writers: header, block table (computed up front so the whole set
/// is written append-only), then each sparse or dense block's data from `data`.
fn write_blocks<O: Out + ?Sized>(
    max_doc: u32,
    count: u32,
    counts: &[u32],
    out: &mut O,
    mut data: impl FnMut(usize, u32, &mut O),
) -> u32 {
    out.put_u32(max_doc);
    out.put_u32(count);
    if count == 0 {
        out.put_u8(0);
        return 0;
    }
    if count == max_doc {
        out.put_u8(1);
        return count;
    }
    out.put_u8(2);
    let (mut start, mut offset) = (0u32, 0u32);
    for (b, &n) in counts.iter().enumerate() {
        let kind = block_kind(b, n, max_doc);
        out.put_u32(start);
        out.put_u32(offset);
        out.put_u32(n);
        out.put_u32(kind);
        start += n;
        offset += match kind {
            KIND_SPARSE => 2 * n,
            KIND_DENSE => DENSE_BYTES as u32,
            _ => 0,
        };
    }
    for (b, &n) in counts.iter().enumerate() {
        let kind = block_kind(b, n, max_doc);
        if kind == KIND_SPARSE || kind == KIND_DENSE {
            data(b, kind, out);
        }
    }
    count
}

#[derive(Clone, Debug)]
enum Repr {
    Empty,
    All,
    Blocks {
        table: Slice,
        data: Slice,
        nblocks: u32,
    },
}

/// A read-only doc set over a [`Slice`].
#[derive(Clone, Debug)]
pub struct DocSet {
    max_doc: u32,
    count: u32,
    repr: Repr,
}

#[derive(Clone, Copy)]
struct Block {
    rank: u32,
    offset: u32,
    count: u32,
    kind: u32,
}

impl DocSet {
    /// A set holding every doc of `max_doc`.
    pub fn all(max_doc: u32) -> DocSet {
        DocSet {
            max_doc,
            count: max_doc,
            repr: if max_doc == 0 { Repr::Empty } else { Repr::All },
        }
    }

    /// An empty set.
    pub fn empty(max_doc: u32) -> DocSet {
        DocSet {
            max_doc,
            count: 0,
            repr: Repr::Empty,
        }
    }

    /// Parses a set written by [`write_docset`] at `*pos` of `buf`, advancing `pos` past it.
    pub fn read(buf: &Slice, pos: &mut usize) -> Result<DocSet> {
        let mut r = In::at(buf, *pos);
        let max_doc = r.u32()?;
        let count = r.u32()?;
        if count > max_doc {
            return Err(corrupt("doc set count exceeds max_doc"));
        }
        let repr = match r.u8()? {
            0 => Repr::Empty,
            1 => Repr::All,
            2 => {
                let mut p = r.pos();
                let nblocks = max_doc.div_ceil(BLOCK_DOCS);
                let table_len = nblocks as usize * ENTRY;
                let table = buf
                    .try_slice(p as u64, table_len as u64)
                    .ok_or_else(|| corrupt("doc set table truncated"))?;
                p += table_len;
                // Validate the table and find the data length.
                let mut data_len = 0usize;
                let mut expect_rank = 0u32;
                for b in 0..nblocks as usize {
                    let e = &table[b * ENTRY..];
                    let rank = u32::from_le_bytes(e[0..4].try_into().unwrap());
                    let offset = u32::from_le_bytes(e[4..8].try_into().unwrap()) as usize;
                    let n = u32::from_le_bytes(e[8..12].try_into().unwrap());
                    let kind = u32::from_le_bytes(e[12..16].try_into().unwrap());
                    let size = match kind {
                        KIND_EMPTY | KIND_ALL => 0,
                        KIND_SPARSE => n as usize * 2,
                        KIND_DENSE => DENSE_BYTES,
                        _ => return Err(corrupt("bad doc set block kind")),
                    };
                    if rank != expect_rank || n > BLOCK_DOCS {
                        return Err(corrupt("bad doc set block table"));
                    }
                    if size > 0 {
                        if offset != data_len {
                            return Err(corrupt("bad doc set block offset"));
                        }
                        data_len += size;
                    }
                    expect_rank += n;
                }
                if expect_rank != count {
                    return Err(corrupt("doc set count mismatch"));
                }
                let data = buf
                    .try_slice(p as u64, data_len as u64)
                    .ok_or_else(|| corrupt("doc set data truncated"))?;
                r.set_pos(p + data_len);
                Repr::Blocks {
                    table,
                    data,
                    nblocks,
                }
            }
            k => return Err(corrupt(format!("bad doc set kind {k}"))),
        };
        *pos = r.pos();
        Ok(DocSet {
            max_doc,
            count,
            repr,
        })
    }

    pub fn max_doc(&self) -> u32 {
        self.max_doc
    }

    /// Number of docs in the set.
    pub fn len(&self) -> u32 {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Whether every doc is in the set (values can be addressed by doc id directly).
    pub fn is_all(&self) -> bool {
        matches!(self.repr, Repr::All)
    }

    #[inline]
    fn block(&self, b: u32) -> Block {
        match &self.repr {
            Repr::Blocks { table, .. } => {
                let e = &table[b as usize * ENTRY..b as usize * ENTRY + ENTRY];
                Block {
                    rank: u32::from_le_bytes(e[0..4].try_into().unwrap()),
                    offset: u32::from_le_bytes(e[4..8].try_into().unwrap()),
                    count: u32::from_le_bytes(e[8..12].try_into().unwrap()),
                    kind: u32::from_le_bytes(e[12..16].try_into().unwrap()),
                }
            }
            _ => unreachable!(),
        }
    }

    #[inline]
    fn data(&self) -> &[u8] {
        match &self.repr {
            Repr::Blocks { data, .. } => data,
            _ => &[],
        }
    }

    /// Index of `doc` among the set's docs, if present.
    #[inline]
    pub fn rank(&self, doc: u32) -> Option<u32> {
        if doc >= self.max_doc {
            return None;
        }
        match &self.repr {
            Repr::Empty => None,
            Repr::All => Some(doc),
            Repr::Blocks { .. } => {
                let blk = self.block(doc >> BLOCK_SHIFT);
                let o = doc & (BLOCK_DOCS - 1);
                match blk.kind {
                    KIND_ALL => Some(blk.rank + o),
                    KIND_SPARSE => {
                        let list = &self.data()[blk.offset as usize..][..blk.count as usize * 2];
                        sparse_search(list, o as u16)
                            .ok()
                            .map(|i| blk.rank + i as u32)
                    }
                    KIND_DENSE => {
                        let d = &self.data()[blk.offset as usize..];
                        let w = (o >> 6) as usize;
                        let word = u64::from_le_bytes(d[w * 8..w * 8 + 8].try_into().unwrap());
                        let bit = 1u64 << (o & 63);
                        if word & bit == 0 {
                            return None;
                        }
                        Some(blk.rank + dense_rank(d, w) + (word & (bit - 1)).count_ones())
                    }
                    _ => None,
                }
            }
        }
    }

    #[inline]
    pub fn contains(&self, doc: u32) -> bool {
        self.rank(doc).is_some()
    }

    /// First doc `>= target` in the set, or [`NO_MORE_DOCS`].
    pub fn next_from(&self, target: u32) -> u32 {
        if target >= self.max_doc {
            return NO_MORE_DOCS;
        }
        match &self.repr {
            Repr::Empty => NO_MORE_DOCS,
            Repr::All => target,
            Repr::Blocks { nblocks, .. } => {
                let mut b = target >> BLOCK_SHIFT;
                let mut o = target & (BLOCK_DOCS - 1);
                while b < *nblocks {
                    if let Some(d) = self.next_in_block(b, o) {
                        return d;
                    }
                    b += 1;
                    o = 0;
                }
                NO_MORE_DOCS
            }
        }
    }

    fn next_in_block(&self, b: u32, o: u32) -> Option<u32> {
        let blk = self.block(b);
        let base = b << BLOCK_SHIFT;
        match blk.kind {
            KIND_ALL => {
                let end = (base + BLOCK_DOCS).min(self.max_doc);
                (base + o < end).then_some(base + o)
            }
            KIND_SPARSE => {
                let list = &self.data()[blk.offset as usize..][..blk.count as usize * 2];
                let i = match sparse_search(list, o as u16) {
                    Ok(i) | Err(i) => i,
                };
                (i < blk.count as usize).then(|| base + sparse_at(list, i) as u32)
            }
            KIND_DENSE => {
                let d = &self.data()[blk.offset as usize..];
                let mut w = (o >> 6) as usize;
                let mut word = read_word(d, w) & (u64::MAX << (o & 63));
                loop {
                    if word != 0 {
                        return Some(base + (w as u32) * 64 + word.trailing_zeros());
                    }
                    w += 1;
                    if w == WORDS {
                        return None;
                    }
                    word = read_word(d, w);
                }
            }
            _ => None,
        }
    }

    /// The doc at index `index` (inverse of [`rank`](Self::rank)).
    pub fn select(&self, index: u32) -> Option<u32> {
        if index >= self.count {
            return None;
        }
        match &self.repr {
            Repr::Empty => None,
            Repr::All => Some(index),
            Repr::Blocks { nblocks, .. } => {
                // Binary search the block whose rank range holds `index`.
                let (mut lo, mut hi) = (0u32, *nblocks);
                while hi - lo > 1 {
                    let mid = (lo + hi) / 2;
                    if self.block(mid).rank <= index {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                // Skip empty blocks that share the rank.
                let mut b = lo;
                let mut blk = self.block(b);
                while index >= blk.rank + blk.count {
                    b += 1;
                    blk = self.block(b);
                }
                let base = b << BLOCK_SHIFT;
                let k = index - blk.rank;
                match blk.kind {
                    KIND_ALL => Some(base + k),
                    KIND_SPARSE => {
                        let list = &self.data()[blk.offset as usize..];
                        Some(base + sparse_at(list, k as usize) as u32)
                    }
                    KIND_DENSE => {
                        let d = &self.data()[blk.offset as usize..];
                        let groups = WORDS / RANK_EVERY_WORDS;
                        let ranks = &d[WORDS * 8..];
                        let mut g = 0;
                        while g + 1 < groups
                            && u16::from_le_bytes(
                                ranks[(g + 1) * 2..(g + 1) * 2 + 2].try_into().unwrap(),
                            ) as u32
                                <= k
                        {
                            g += 1;
                        }
                        let mut left =
                            k - u16::from_le_bytes(ranks[g * 2..g * 2 + 2].try_into().unwrap())
                                as u32;
                        let mut w = g * RANK_EVERY_WORDS;
                        loop {
                            let mut word = read_word(d, w);
                            let c = word.count_ones();
                            if left < c {
                                for _ in 0..left {
                                    word &= word - 1;
                                }
                                return Some(base + w as u32 * 64 + word.trailing_zeros());
                            }
                            left -= c;
                            w += 1;
                        }
                    }
                    _ => None,
                }
            }
        }
    }

    /// A forward cursor over the set.
    pub fn cursor(&self) -> DocSetCursor {
        DocSetCursor::new(self.clone())
    }

    /// All docs, materialized (tests and merges).
    pub fn to_vec(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.count as usize);
        let mut c = self.cursor();
        while c.next() != NO_MORE_DOCS {
            out.push(c.doc());
        }
        out
    }
}

#[inline]
fn read_word(d: &[u8], w: usize) -> u64 {
    u64::from_le_bytes(d[w * 8..w * 8 + 8].try_into().unwrap())
}

#[inline]
fn dense_rank(d: &[u8], w: usize) -> u32 {
    let g = w / RANK_EVERY_WORDS;
    let ranks = &d[WORDS * 8..];
    let mut r = u16::from_le_bytes(ranks[g * 2..g * 2 + 2].try_into().unwrap()) as u32;
    for i in g * RANK_EVERY_WORDS..w {
        r += read_word(d, i).count_ones();
    }
    r
}

#[inline]
fn sparse_at(list: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([list[2 * i], list[2 * i + 1]])
}

#[inline]
fn sparse_search(list: &[u8], target: u16) -> std::result::Result<usize, usize> {
    let (mut lo, mut hi) = (0usize, list.len() / 2);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let v = sparse_at(list, mid);
        if v < target {
            lo = mid + 1;
        } else if v > target {
            hi = mid;
        } else {
            return Ok(mid);
        }
    }
    Err(lo)
}

/// A forward iterator over a [`DocSet`] that also tracks each doc's index (rank).
///
/// It keeps its position inside the current block (list index, or bitset word and remaining
/// bits), so `next` costs a few instructions instead of a fresh lookup plus rank computation.
#[derive(Clone, Debug)]
pub struct DocSetCursor {
    set: DocSet,
    doc: u32,
    index: u32,
    /// Current block number (`u32::MAX`: none loaded).
    block: u32,
    kind: u32,
    base: u32,
    offset: u32,
    count: u32,
    /// Sparse: index in the block's list. Dense: current word.
    pos: u32,
    /// Dense: bits of the current word above the current doc.
    word: u64,
}

impl DocSetCursor {
    fn new(set: DocSet) -> DocSetCursor {
        DocSetCursor {
            set,
            doc: u32::MAX,
            index: u32::MAX,
            block: u32::MAX,
            kind: KIND_EMPTY,
            base: 0,
            offset: 0,
            count: 0,
            pos: 0,
            word: 0,
        }
    }

    /// Current doc; `u32::MAX` before the first `next`, [`NO_MORE_DOCS`] at the end.
    #[inline]
    pub fn doc(&self) -> u32 {
        self.doc
    }

    /// Index (rank) of the current doc.
    #[inline]
    pub fn index(&self) -> u32 {
        self.index
    }

    #[inline]
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u32 {
        if self.doc == NO_MORE_DOCS {
            return NO_MORE_DOCS;
        }
        if self.set.is_all() {
            let target = self.doc.wrapping_add(1);
            self.doc = if target < self.set.max_doc { target } else { NO_MORE_DOCS };
            self.index = self.doc;
            return self.doc;
        }
        if self.block != u32::MAX {
            match self.kind {
                KIND_ALL => {
                    let end = (self.base + BLOCK_DOCS).min(self.set.max_doc);
                    if self.doc + 1 < end {
                        self.doc += 1;
                        self.index += 1;
                        return self.doc;
                    }
                }
                KIND_SPARSE => {
                    if self.pos + 1 < self.count {
                        self.pos += 1;
                        let list = &self.set.data()[self.offset as usize..];
                        self.doc = self.base + sparse_at(list, self.pos as usize) as u32;
                        self.index += 1;
                        return self.doc;
                    }
                }
                KIND_DENSE => {
                    if let Some(d) = self.dense_next() {
                        self.doc = d;
                        self.index += 1;
                        return d;
                    }
                }
                _ => {}
            }
        }
        let next_block = if self.block == u32::MAX { 0 } else { self.block + 1 };
        self.first_from_block(next_block)
    }

    /// Next set bit of the current dense block after the current doc.
    #[inline]
    fn dense_next(&mut self) -> Option<u32> {
        loop {
            if self.word != 0 {
                let d = self.base + self.pos * 64 + self.word.trailing_zeros();
                self.word &= self.word - 1;
                return Some(d);
            }
            self.pos += 1;
            if self.pos as usize >= WORDS {
                return None;
            }
            self.word = read_word(&self.set.data()[self.offset as usize..], self.pos as usize);
        }
    }

    fn load(&mut self, b: u32) -> Block {
        let blk = self.set.block(b);
        self.block = b;
        self.kind = blk.kind;
        self.base = b << BLOCK_SHIFT;
        self.offset = blk.offset;
        self.count = blk.count;
        blk
    }

    /// Positions on the first doc of the first non-empty block at or after `b`.
    fn first_from_block(&mut self, mut b: u32) -> u32 {
        let Repr::Blocks { nblocks, .. } = self.set.repr else {
            self.doc = NO_MORE_DOCS;
            return NO_MORE_DOCS;
        };
        while b < nblocks {
            let blk = self.load(b);
            if blk.kind != KIND_EMPTY && blk.count > 0 {
                self.index = blk.rank;
                self.doc = match blk.kind {
                    KIND_ALL => self.base,
                    KIND_SPARSE => {
                        self.pos = 0;
                        self.base + sparse_at(&self.set.data()[blk.offset as usize..], 0) as u32
                    }
                    _ => {
                        self.pos = 0;
                        self.word = read_word(&self.set.data()[blk.offset as usize..], 0);
                        match self.dense_next() {
                            Some(d) => d,
                            None => {
                                b += 1;
                                continue;
                            }
                        }
                    }
                };
                return self.doc;
            }
            b += 1;
        }
        self.block = u32::MAX;
        self.doc = NO_MORE_DOCS;
        NO_MORE_DOCS
    }

    /// Moves to the first doc `>= target` (never backwards).
    pub fn advance(&mut self, target: u32) -> u32 {
        if self.doc != u32::MAX && (self.doc == NO_MORE_DOCS || self.doc >= target) {
            return self.doc;
        }
        if target >= self.set.max_doc {
            self.doc = NO_MORE_DOCS;
            return NO_MORE_DOCS;
        }
        if self.set.is_all() {
            self.doc = target;
            self.index = target;
            return target;
        }
        let Repr::Blocks { .. } = self.set.repr else {
            self.doc = NO_MORE_DOCS;
            return NO_MORE_DOCS;
        };
        let b = target >> BLOCK_SHIFT;
        let o = target & (BLOCK_DOCS - 1);
        let same_block = self.block == b && self.doc != u32::MAX;
        let blk = if same_block { self.set.block(b) } else { self.load(b) };
        match blk.kind {
            KIND_ALL => {
                self.doc = target;
                self.index = blk.rank + o;
                return target;
            }
            KIND_SPARSE => {
                let list = &self.set.data()[blk.offset as usize..][..blk.count as usize * 2];
                let from = if same_block { self.pos as usize } else { 0 };
                let i = from + match sparse_search(&list[from * 2..], o as u16) {
                    Ok(i) | Err(i) => i,
                };
                if i < blk.count as usize {
                    self.pos = i as u32;
                    self.doc = self.base + sparse_at(list, i) as u32;
                    self.index = blk.rank + i as u32;
                    return self.doc;
                }
            }
            KIND_DENSE => {
                let d = &self.set.data()[blk.offset as usize..];
                let mut w = (o >> 6) as usize;
                let mut word = read_word(d, w) & (u64::MAX << (o & 63));
                while word == 0 && w + 1 < WORDS {
                    w += 1;
                    word = read_word(d, w);
                }
                if word != 0 {
                    let bit = word.trailing_zeros();
                    let below = read_word(d, w) & ((1u64 << bit) - 1);
                    self.index = blk.rank + dense_rank(d, w) + below.count_ones();
                    self.pos = w as u32;
                    self.word = word & (word - 1);
                    self.doc = self.base + w as u32 * 64 + bit;
                    return self.doc;
                }
            }
            _ => {}
        }
        self.first_from_block(b + 1)
    }

    /// Positions on `target` if it is in the set; `false` leaves the cursor where it was.
    #[inline]
    pub fn advance_exact(&mut self, target: u32) -> bool {
        match self.set.rank(target) {
            Some(i) => {
                // Re-seat the block state so a following `next` continues after `target`.
                self.doc = u32::MAX;
                self.block = u32::MAX;
                let d = self.advance(target);
                debug_assert_eq!(d, target);
                self.index = i;
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    fn check(docs: &[u32], max_doc: u32) {
        let mut buf = vec![9u8];
        write_docset(docs, max_doc, &mut buf);
        buf.push(0xEE);
        let s = Slice::from(buf);
        let mut pos = 1;
        let set = DocSet::read(&s, &mut pos).unwrap();
        assert_eq!(s[pos], 0xEE);
        assert_eq!(set.len() as usize, docs.len());
        assert_eq!(set.to_vec(), docs);
        let mut it = docs.iter().enumerate().peekable();
        let step = (max_doc / 5000).max(1);
        let mut d = 0u32;
        while d < max_doc {
            while it.peek().is_some_and(|(_, x)| **x < d) {
                it.next();
            }
            let expect_next = it.peek().map(|(_, x)| **x).unwrap_or(NO_MORE_DOCS);
            assert_eq!(set.next_from(d), expect_next, "next_from {d}");
            let expect_rank = it.peek().filter(|(_, x)| **x == d).map(|(i, _)| *i as u32);
            assert_eq!(set.rank(d), expect_rank, "rank {d}");
            d += step;
        }
        for (i, &x) in docs.iter().enumerate() {
            assert_eq!(set.rank(x), Some(i as u32));
            assert_eq!(set.select(i as u32), Some(x));
        }
        assert_eq!(set.select(docs.len() as u32), None);
        assert_eq!(set.next_from(max_doc), NO_MORE_DOCS);
        // Cursor advance.
        let mut c = set.cursor();
        for (i, &x) in docs.iter().enumerate().step_by(7) {
            assert_eq!(c.advance(x), x);
            assert_eq!(c.index(), i as u32);
        }
        // Mixed next / advance / advance_exact against the model.
        let mut rng = StdRng::seed_from_u64(docs.len() as u64 ^ max_doc as u64);
        let mut c = set.cursor();
        let mut cur: Option<usize> = None; // model position
        loop {
            let op = rng.gen_range(0..10);
            let got;
            if op < 6 {
                got = c.next();
                cur = Some(cur.map_or(0, |i| i + 1));
            } else if op < 9 {
                let from = cur.map_or(0, |i| docs.get(i).copied().unwrap_or(max_doc));
                let target = from + rng.gen_range(0..3000);
                got = c.advance(target);
                let at_or_after = |i: usize| docs.get(i).is_some_and(|&d| d >= target);
                if !cur.is_some_and(at_or_after) {
                    cur = Some(docs.partition_point(|&d| d < target));
                }
            } else {
                let target = rng.gen_range(0..max_doc.max(1));
                if c.advance_exact(target) {
                    let i = docs.binary_search(&target).unwrap();
                    assert_eq!(c.index(), i as u32);
                    cur = Some(i);
                } else {
                    assert!(docs.binary_search(&target).is_err());
                }
                continue;
            }
            match cur.and_then(|i| docs.get(i)) {
                Some(&d) => {
                    assert_eq!(got, d);
                    assert_eq!(c.index() as usize, cur.unwrap());
                }
                None => {
                    assert_eq!(got, NO_MORE_DOCS);
                    break;
                }
            }
        }
    }

    #[test]
    fn edge_sizes() {
        check(&[], 0);
        check(&[], 10);
        check(&[0], 1);
        check(&[5], 10);
        check(&(0..10).collect::<Vec<_>>(), 10);
        check(&(0..65536).collect::<Vec<_>>(), 200_000);
        check(&(65535..65537).collect::<Vec<_>>(), 65537);
    }

    #[test]
    fn random_densities() {
        let mut rng = StdRng::seed_from_u64(42);
        for &(max_doc, p) in &[
            (1000u32, 0.5f64),
            (200_000, 0.001),
            (200_000, 0.03),
            (200_000, 0.07),
            (200_000, 0.5),
            (200_000, 0.999),
            (131_072, 1.0),
        ] {
            let docs: Vec<u32> = (0..max_doc).filter(|_| rng.gen_bool(p)).collect();
            check(&docs, max_doc);
        }
    }

    #[test]
    fn corrupt_tables_are_rejected() {
        let docs: Vec<u32> = (0..100_000).step_by(3).collect();
        let mut buf = Vec::new();
        write_docset(&docs, 100_000, &mut buf);
        let mut bad = buf.clone();
        bad[13] ^= 1; // first block entry rank
        assert!(DocSet::read(&Slice::from(bad), &mut 0).is_err());
        for n in [0, 5, 20, buf.len() - 1] {
            assert!(DocSet::read(&Slice::from(buf[..n].to_vec()), &mut 0).is_err());
        }
    }
}
