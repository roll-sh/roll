//! Postings: doc ids, frequencies, positions, offsets and payloads, with inline skip data that
//! carries impacts for block-max WAND / MaxScore.
//!
//! # Streams
//!
//! Each field has three sections. `Docs` holds doc ids and frequencies, `Positions` the position
//! deltas, `Payloads` the offsets and payloads. A term's postings start at the file pointers in
//! its [`TermState`]; a term with a single doc stores that doc in the term state ("pulsing") and
//! nothing in `Docs`.
//!
//! # Doc stream of one term
//!
//! ```text
//! ( [L1 header] if a full group of 32 blocks starts here
//!   [L0 header] [docs] [freq-1: PFOR] )*                one per full block of 128 docs
//! [tail header] [tail docs: varints]                     df % 128 docs, if any
//!
//! L0 header: last-doc delta, data length, impacts, positions pointer (fp, absolute index),
//!            payloads pointer
//! L1 header: last-doc delta of the 32-block group, group length, group impacts
//! tail:      impacts, positions pointer, payloads pointer; then per doc
//!            (delta << 1 | freq == 1) [freq if != 1]
//! ```
//!
//! A full block's docs are FOR-packed deltas, or — like Lucene 10.1 — a bit set over the
//! block's doc range when that is no larger than FOR one bit wider (dense terms), or nothing
//! at all for 128 consecutive docs. Dense blocks advance with a few bit operations and fill
//! `into_bitset` a word at a time without decoding; `next()` decodes them on first use.
//!
//! Skip data is inline (as in Lucene 10.1): `advance` reads the 8–20 byte header of a block and
//! jumps over the block's data when the block ends before the target, and jumps over whole groups
//! of 32 blocks with the L1 header, so a long jump touches O(groups + 32) headers and decodes one
//! block. Headers carry the positions pointer of their first doc, so skipping never needs to
//! decode positions of skipped docs.
//!
//! Impacts are the Pareto frontier of `(freq, norm)` pairs of a block (higher freq and lower
//! norm score better under BM25), stored as deltas. Unlike Lucene, the tail of a term also stores
//! impacts, so medium-frequency terms get tight score bounds instead of a dummy maximum.
//!
//! # Position streams
//!
//! Positions (delta within the doc) are PFOR blocks of 128 independent of doc blocks, with a
//! varint tail. Offsets (start delta within the doc, length) and payload lengths are PFOR blocks
//! aligned with the position blocks in the payloads stream, followed by the payload bytes.
//!
//! # Decoding
//!
//! Doc blocks decode with SIMD bit unpacking plus a SIMD prefix sum; frequencies are decoded
//! lazily, only when `freq()` is called for a doc of the block, so filters and conjunction
//! leaders never pay for them. Advancing inside a decoded block counts the docs below the
//! target with a branch-free (vectorized) loop.

use crate::NO_MORE_DOCS;
use crate::bitpack::{self, BLOCK, for_decode, for_encode, pfor_decode, pfor_encode, pfor_skip};
use crate::info::IndexOptions;
use crate::io::{Out, read_vu32, read_vu64};
use crate::slice::Slice;

const GROUP: u32 = 32;

/// Index of the first doc `>= target` in the ascending `docs[from..]` (`docs.len()` when none;
/// Lucene's `VectorUtil.findNextGEQ`). Advances mostly move a few docs, so this hops over
/// chunks of 8 with one compare each and counts inside the final chunk branch-free (the count
/// stays in `u32` lanes so it vectorizes), instead of scanning the whole block.
#[inline]
pub(crate) fn find_next_geq(docs: &[u32], from: usize, target: u32) -> usize {
    let mut i = from;
    while i + 8 <= docs.len() {
        if docs[i + 7] < target {
            i += 8;
            continue;
        }
        let chunk: &[u32; 8] = docs[i..i + 8].try_into().unwrap();
        let below: u32 = chunk.iter().map(|&d| (d < target) as u32).sum();
        return i + below as usize;
    }
    while i < docs.len() && docs[i] < target {
        i += 1;
    }
    i
}

/// Doc encoding byte of a full block: FOR widths are `0..=32`; these mark the dense forms.
const DOCS_BITSET: u8 = 0x40; // | (words - 1), then the words
const DOCS_CONSECUTIVE: u8 = 0x80;

/// Benchmarks compare against FOR-only blocks.
#[cfg(test)]
static FORCE_FOR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Per-term metadata pointing into the postings streams.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TermState {
    /// Ordinal of the term in its field's dictionary.
    pub ord: u64,
    pub doc_freq: u32,
    /// Sum of frequencies; equals `doc_freq` for fields without frequencies.
    pub total_term_freq: u64,
    pub doc_fp: u64,
    pub pos_fp: u64,
    pub pay_fp: u64,
    /// The doc of a term with `doc_freq == 1`.
    pub singleton: u32,
}

/// What a postings cursor should decode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PostingsFlags(u8);

impl PostingsFlags {
    pub const DOCS: PostingsFlags = PostingsFlags(0);
    pub const FREQS: PostingsFlags = PostingsFlags(1);
    pub const POSITIONS: PostingsFlags = PostingsFlags(1 | 2);
    pub const OFFSETS: PostingsFlags = PostingsFlags(1 | 2 | 4);
    pub const PAYLOADS: PostingsFlags = PostingsFlags(1 | 2 | 8);
    pub const ALL: PostingsFlags = PostingsFlags(15);

    pub fn freqs(self) -> bool {
        self.0 & 1 != 0
    }
    pub fn positions(self) -> bool {
        self.0 & 2 != 0
    }
    pub fn offsets(self) -> bool {
        self.0 & 4 != 0
    }
    pub fn payloads(self) -> bool {
        self.0 & 8 != 0
    }
}

impl std::ops::BitOr for PostingsFlags {
    type Output = PostingsFlags;
    fn bitor(self, o: PostingsFlags) -> PostingsFlags {
        PostingsFlags(self.0 | o.0)
    }
}

/// A competitive `(freq, norm)` pair: no doc in the covered range has a higher frequency with a
/// norm at most `norm` (norms compare as unsigned bytes; smaller means a shorter field).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Impact {
    pub freq: u32,
    pub norm: u8,
}

/// Norm recorded in impacts of fields without norms (Lucene uses 1 as well).
pub const NO_NORM: u8 = 1;

/// Accumulates the Pareto frontier of `(freq, norm)` pairs.
#[derive(Clone)]
pub(crate) struct ImpactAcc {
    max_freq: Box<[u32; 256]>,
    lo: usize,
    hi: usize,
}

impl ImpactAcc {
    pub(crate) fn new() -> ImpactAcc {
        ImpactAcc {
            max_freq: Box::new([0; 256]),
            lo: 256,
            hi: 0,
        }
    }

    #[inline]
    pub(crate) fn add(&mut self, freq: u32, norm: u8) {
        let n = norm as usize;
        if freq > self.max_freq[n] {
            self.max_freq[n] = freq;
        }
        self.lo = self.lo.min(n);
        self.hi = self.hi.max(n + 1);
    }

    pub(crate) fn merge(&mut self, other: &ImpactAcc) {
        for n in other.lo..other.hi {
            if other.max_freq[n] > self.max_freq[n] {
                self.max_freq[n] = other.max_freq[n];
            }
        }
        self.lo = self.lo.min(other.lo);
        self.hi = self.hi.max(other.hi);
    }

    pub(crate) fn clear(&mut self) {
        if self.lo < self.hi {
            self.max_freq[self.lo..self.hi].fill(0);
        }
        self.lo = 256;
        self.hi = 0;
    }

    /// Serializes the frontier: count, then `(freq delta - 1, norm delta [- 1])` pairs.
    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        let mut tmp = [Impact::default(); 256];
        let mut n = 0;
        let mut best = 0u32;
        for norm in self.lo..self.hi {
            let f = self.max_freq[norm];
            if f > best {
                best = f;
                tmp[n] = Impact {
                    freq: f,
                    norm: norm as u8,
                };
                n += 1;
            }
        }
        // The body is at most 1 + 256 * (5 + 2) bytes; encode it on the stack, then append it
        // with its length prefix.
        let mut body = [0u8; 2048];
        let mut len = 0usize;
        let mut put = |v: u32| {
            let mut v = v;
            while v >= 0x80 {
                body[len] = (v as u8) | 0x80;
                len += 1;
                v >>= 7;
            }
            body[len] = v as u8;
            len += 1;
        };
        put(n as u32);
        let (mut pf, mut pn) = (0u32, 0u32);
        for (i, imp) in tmp[..n].iter().enumerate() {
            put(imp.freq - pf - 1);
            put(if i == 0 { imp.norm as u32 } else { imp.norm as u32 - pn - 1 });
            pf = imp.freq;
            pn = imp.norm as u32;
        }
        out.put_vu32(len as u32);
        out.put_bytes(&body[..len]);
    }
}

/// Decodes impacts written by [`ImpactAcc::write`] (the body, without its length prefix).
pub(crate) fn decode_impacts(body: &[u8], out: &mut Vec<Impact>) {
    out.clear();
    let mut p = 0;
    let n = read_vu32(body, &mut p).min(256);
    let (mut f, mut nm) = (0u32, 0u32);
    for i in 0..n {
        f = f.wrapping_add(read_vu32(body, &mut p)).wrapping_add(1);
        let d = read_vu32(body, &mut p);
        nm = if i == 0 {
            d
        } else {
            nm.wrapping_add(d).wrapping_add(1)
        };
        out.push(Impact {
            freq: f,
            norm: nm.min(255) as u8,
        });
    }
}

// ---------------------------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------------------------

struct BlockRec {
    end: usize,
    last_doc: u32,
}

struct GroupRec {
    last_doc: u32,
    impacts: Vec<u8>,
}

/// Writes the postings of one field, term by term.
pub(crate) struct PostingsWriter<'n> {
    has_freqs: bool,
    has_pos: bool,
    has_offsets: bool,
    has_payloads: bool,
    norms: Option<&'n [u8]>,
    /// Unflushed tails of the three streams; `*_base` bytes before them were already handed
    /// to the caller with [`take_flushed`](Self::take_flushed).
    pub docs: Vec<u8>,
    pub pos: Vec<u8>,
    pub pay: Vec<u8>,
    docs_base: u64,
    pos_base: u64,
    pay_base: u64,

    // Current term.
    term_buf: Vec<u8>,
    scratch: Vec<u8>,
    blocks: Vec<BlockRec>,
    groups: Vec<GroupRec>,
    doc_buf: [u32; BLOCK],
    freq_buf: [u32; BLOCK],
    doc_raw: [u32; BLOCK],
    n: usize,
    df: u32,
    ttf: u64,
    last_doc: u32,
    block_prev_last: u32,
    block_acc: ImpactAcc,
    group_acc: ImpactAcc,
    term_acc: ImpactAcc,
    cur_freq: u32,
    first_doc: u32,

    // Positions of the current term.
    term_pos_start: u64,
    term_pay_start: u64,
    pos_buf: [u32; BLOCK],
    off_start_buf: [u32; BLOCK],
    off_len_buf: [u32; BLOCK],
    pay_len_buf: [u32; BLOCK],
    pay_bytes: Vec<u8>,
    pos_n: usize,
    pos_count: u64,
    last_pos: u32,
    last_start: u32,
    // Positions pointer of the current doc block's first doc.
    blk_pos_fp: u64,
    blk_pos_count: u64,
    blk_pay_fp: u64,
}

impl<'n> PostingsWriter<'n> {
    pub(crate) fn new(opts: IndexOptions, payloads: bool, norms: Option<&'n [u8]>) -> Self {
        PostingsWriter {
            has_freqs: opts.has_freqs(),
            has_pos: opts.has_positions(),
            has_offsets: opts.has_offsets(),
            has_payloads: payloads && opts.has_positions(),
            norms,
            docs: Vec::new(),
            pos: Vec::new(),
            pay: Vec::new(),
            docs_base: 0,
            pos_base: 0,
            pay_base: 0,
            term_buf: Vec::new(),
            scratch: Vec::new(),
            blocks: Vec::new(),
            groups: Vec::new(),
            doc_buf: [0; BLOCK],
            freq_buf: [0; BLOCK],
            doc_raw: [0; BLOCK],
            n: 0,
            df: 0,
            ttf: 0,
            last_doc: 0,
            block_prev_last: 0,
            block_acc: ImpactAcc::new(),
            group_acc: ImpactAcc::new(),
            term_acc: ImpactAcc::new(),
            cur_freq: 0,
            first_doc: 0,
            term_pos_start: 0,
            term_pay_start: 0,
            pos_buf: [0; BLOCK],
            off_start_buf: [0; BLOCK],
            off_len_buf: [0; BLOCK],
            pay_len_buf: [0; BLOCK],
            pay_bytes: Vec::new(),
            pos_n: 0,
            pos_count: 0,
            last_pos: 0,
            last_start: 0,
            blk_pos_fp: 0,
            blk_pos_count: 0,
            blk_pay_fp: 0,
        }
    }

    fn pos_len(&self) -> u64 {
        self.pos_base + self.pos.len() as u64
    }

    fn pay_len(&self) -> u64 {
        self.pay_base + self.pay.len() as u64
    }

    /// Hands the buffered bytes of the three streams to `sink` (doc, positions, payloads
    /// stream) and clears them, between terms, once they exceed `threshold` bytes together.
    /// File pointers in later term states stay absolute.
    pub(crate) fn take_flushed(
        &mut self,
        threshold: usize,
        sink: &mut dyn FnMut(usize, &[u8]) -> crate::error::Result<()>,
    ) -> crate::error::Result<()> {
        if self.docs.len() + self.pos.len() + self.pay.len() < threshold {
            return Ok(());
        }
        for (i, (buf, base)) in [
            (&mut self.docs, &mut self.docs_base),
            (&mut self.pos, &mut self.pos_base),
            (&mut self.pay, &mut self.pay_base),
        ]
        .into_iter()
        .enumerate()
        {
            if !buf.is_empty() {
                sink(i, buf)?;
                *base += buf.len() as u64;
                buf.clear();
            }
        }
        Ok(())
    }

    pub(crate) fn start_term(&mut self) {
        self.term_buf.clear();
        self.blocks.clear();
        self.groups.clear();
        self.n = 0;
        self.df = 0;
        self.ttf = 0;
        self.last_doc = 0;
        self.block_prev_last = 0;
        self.block_acc.clear();
        self.group_acc.clear();
        self.term_acc.clear();
        self.term_pos_start = self.pos_len();
        self.term_pay_start = self.pay_len();
        self.pos_n = 0;
        self.pos_count = 0;
        self.pay_bytes.clear();
    }

    fn norm(&self, doc: u32) -> u8 {
        match self.norms {
            Some(n) => n.get(doc as usize).copied().unwrap_or(0),
            None => NO_NORM,
        }
    }

    /// Starts a doc; `freq` is ignored (treated as 1) for fields without frequencies.
    pub(crate) fn start_doc(&mut self, doc: u32, freq: u32) {
        debug_assert!(
            self.df == 0 || doc > self.last_doc,
            "docs must be strictly increasing"
        );
        let freq = if self.has_freqs { freq.max(1) } else { 1 };
        if self.n == 0 {
            self.blk_pos_fp = self.pos_len() - self.term_pos_start;
            self.blk_pos_count = self.pos_count;
            self.blk_pay_fp = self.pay_len() - self.term_pay_start;
        }
        if self.df == 0 {
            self.first_doc = doc;
        }
        let base = if self.n == 0 {
            self.block_prev_last
        } else {
            self.last_doc
        };
        self.doc_buf[self.n] = doc - base;
        self.doc_raw[self.n] = doc;
        self.freq_buf[self.n] = freq;
        self.n += 1;
        self.df += 1;
        self.ttf += freq as u64;
        self.last_doc = doc;
        self.cur_freq = freq;
        let norm = self.norm(doc);
        self.block_acc.add(freq, norm);
        self.last_pos = 0;
        self.last_start = 0;
    }

    pub(crate) fn add_position(&mut self, position: u32, start: u32, end: u32, payload: &[u8]) {
        if !self.has_pos {
            return;
        }
        let i = self.pos_n;
        self.pos_buf[i] = position.wrapping_sub(self.last_pos);
        self.last_pos = position;
        if self.has_offsets {
            self.off_start_buf[i] = start.wrapping_sub(self.last_start);
            self.off_len_buf[i] = end.wrapping_sub(start);
            self.last_start = start;
        }
        if self.has_payloads {
            self.pay_len_buf[i] = payload.len() as u32;
            self.pay_bytes.extend_from_slice(payload);
        }
        self.pos_n += 1;
        self.pos_count += 1;
        if self.pos_n == BLOCK {
            self.flush_pos_block();
        }
    }

    fn flush_pos_block(&mut self) {
        pfor_encode(&self.pos_buf, &mut self.pos);
        if self.has_payloads {
            pfor_encode(&self.pay_len_buf, &mut self.pay);
            self.pay.put_vu32(self.pay_bytes.len() as u32);
            self.pay.extend_from_slice(&self.pay_bytes);
            self.pay_bytes.clear();
        }
        if self.has_offsets {
            pfor_encode(&self.off_start_buf, &mut self.pay);
            pfor_encode(&self.off_len_buf, &mut self.pay);
        }
        self.pos_n = 0;
    }

    pub(crate) fn finish_doc(&mut self) {
        if self.n == BLOCK {
            self.flush_doc_block();
        }
    }

    /// The positions pointer of the current doc block's first doc.
    fn pos_pointer(&self) -> Option<[u64; 3]> {
        self.has_pos.then_some([self.blk_pos_fp, self.blk_pos_count, self.blk_pay_fp])
    }

    fn put_pos_pointer(&self, p: Option<[u64; 3]>, out: &mut Vec<u8>) {
        if let Some([fp, count, pay]) = p {
            out.put_vu64(fp);
            out.put_vu64(count);
            if self.has_offsets || self.has_payloads {
                out.put_vu64(pay);
            }
        }
    }

    fn flush_doc_block(&mut self) {
        let mut data = std::mem::take(&mut self.scratch);
        data.clear();
        // Docs as FOR-packed deltas, or as a bit set (one bit per doc id in the block's range)
        // when that is no larger than FOR one bit wider, like Lucene 10.1: dense blocks then
        // advance and fill bit sets without decoding. A run of 128 consecutive docs stores
        // nothing.
        let start = if self.blocks.is_empty() { 0 } else { self.block_prev_last + 1 };
        let range = (self.last_doc - start) as usize + 1;
        let bpv = bitpack::max_bits(&self.doc_buf) as usize;
        #[cfg(test)]
        let range = if FORCE_FOR.load(std::sync::atomic::Ordering::Relaxed) { usize::MAX / 2 } else { range };
        if range == BLOCK {
            data.push(DOCS_CONSECUTIVE);
        } else if (bpv + 1).min(32) * BLOCK <= range {
            for_encode(&self.doc_buf, &mut data);
        } else {
            let nwords = range.div_ceil(64);
            let mut words = [0u64; 64];
            for &d in &self.doc_raw {
                let b = (d - start) as usize;
                words[b >> 6] |= 1 << (b & 63);
            }
            data.push(DOCS_BITSET | (nwords - 1) as u8);
            for w in &words[..nwords] {
                data.extend_from_slice(&w.to_le_bytes());
            }
        }
        if self.has_freqs {
            let mut f = self.freq_buf;
            for v in f.iter_mut() {
                *v -= 1;
            }
            pfor_encode(&f, &mut data);
        }
        let mut tb = std::mem::take(&mut self.term_buf);
        tb.put_vu32(self.last_doc - self.block_prev_last);
        tb.put_vu32(data.len() as u32);
        if self.has_freqs {
            self.block_acc.write(&mut tb);
        }
        self.put_pos_pointer(self.pos_pointer(), &mut tb);
        tb.extend_from_slice(&data);
        self.term_buf = tb;
        self.scratch = data;
        self.blocks.push(BlockRec {
            end: self.term_buf.len(),
            last_doc: self.last_doc,
        });
        self.group_acc.merge(&self.block_acc);
        self.block_acc.clear();
        if self.blocks.len().is_multiple_of(GROUP as usize) {
            let mut imp = Vec::new();
            if self.has_freqs {
                self.group_acc.write(&mut imp);
            }
            self.groups.push(GroupRec {
                last_doc: self.last_doc,
                impacts: imp,
            });
            self.term_acc.merge(&self.group_acc);
            self.group_acc.clear();
        }
        self.block_prev_last = self.last_doc;
        self.n = 0;
    }

    /// Finishes the current term and returns its state (with `ord` 0).
    pub(crate) fn finish_term(&mut self) -> TermState {
        let mut st = TermState {
            ord: 0,
            doc_freq: self.df,
            total_term_freq: if self.has_freqs {
                self.ttf
            } else {
                self.df as u64
            },
            doc_fp: self.docs_base + self.docs.len() as u64,
            pos_fp: self.term_pos_start,
            pay_fp: self.term_pay_start,
            singleton: 0,
        };
        if self.df == 0 {
            return st;
        }
        // Positions tail.
        if self.has_pos && self.pos_n > 0 {
            for i in 0..self.pos_n {
                self.pos.put_vu32(self.pos_buf[i]);
            }
            if self.has_payloads || self.has_offsets {
                let mut pb = 0usize;
                for i in 0..self.pos_n {
                    if self.has_payloads {
                        let l = self.pay_len_buf[i] as usize;
                        self.pay.put_vu32(l as u32);
                        self.pay.extend_from_slice(&self.pay_bytes[pb..pb + l]);
                        pb += l;
                    }
                    if self.has_offsets {
                        self.pay.put_vu32(self.off_start_buf[i]);
                        self.pay.put_vu32(self.off_len_buf[i]);
                    }
                }
            }
            self.pos_n = 0;
        }
        if self.df == 1 {
            st.singleton = self.first_doc;
            return st;
        }
        // Assemble the doc stream: full blocks with L1 headers in front of complete groups.
        let full = self.blocks.len();
        let mut start = 0usize;
        let mut prev_last = 0u32;
        for (i, b) in self.blocks.iter().enumerate() {
            if i % GROUP as usize == 0 && i + GROUP as usize <= full {
                let g = &self.groups[i / GROUP as usize];
                let group_end = self.blocks[i + GROUP as usize - 1].end;
                self.docs.put_vu32(g.last_doc - prev_last);
                self.docs.put_vu64((group_end - start) as u64);
                self.docs.extend_from_slice(&g.impacts);
            }
            self.docs.extend_from_slice(&self.term_buf[start..b.end]);
            start = b.end;
            prev_last = b.last_doc;
        }
        if self.n > 0 {
            // Tail header + varint docs.
            let mut docs = std::mem::take(&mut self.docs);
            if self.has_freqs {
                self.block_acc.write(&mut docs);
            }
            self.put_pos_pointer(self.pos_pointer(), &mut docs);
            self.docs = docs;
            for i in 0..self.n {
                let delta = self.doc_buf[i];
                if self.has_freqs {
                    let f = self.freq_buf[i];
                    self.docs.put_vu32((delta << 1) | (f == 1) as u32);
                    if f != 1 {
                        self.docs.put_vu32(f);
                    }
                } else {
                    self.docs.put_vu32(delta);
                }
            }
            self.n = 0;
        }
        st
    }
}

// ---------------------------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------------------------

/// The streams of one field plus its options; cheap to clone.
#[derive(Clone, Debug)]
pub struct PostingsData {
    pub(crate) docs: Slice,
    pub(crate) pos: Slice,
    pub(crate) pay: Slice,
    pub(crate) opts: IndexOptions,
    pub(crate) payloads: bool,
}

#[derive(Clone, Copy, Default)]
struct PosPointer {
    fp: u64,
    count: u64,
    pay_fp: u64,
}

/// Position-decoding state, allocated only when positions are requested.
struct PosState {
    pos: Slice,
    pay: Slice,
    want_offsets: bool,
    want_payloads: bool,
    has_offsets: bool,
    has_payloads: bool,
    term_pos: usize,
    term_pay: usize,
    full_blocks: u64,
    ttf: u64,
    // Buffer of position deltas covering absolute indices [buf_start, buf_start + len).
    buf: [u32; BLOCK],
    off_start: [u32; BLOCK],
    off_len: [u32; BLOCK],
    pay_len: [u32; BLOCK],
    pay_starts: [u32; BLOCK + 1],
    pay_bytes: Vec<u8>,
    len: usize,
    idx: usize,
    loaded: bool,
    buf_block: u64,
    next_fp: usize,
    next_pay_fp: usize,
    // Hint: pointer of the current doc block's first doc.
    hint_block: u64,
    hint_fp: usize,
    hint_pay_fp: usize,
    // Per-doc state.
    doc_count: u64,
    started: bool,
    position: u32,
    last_start: u32,
    start_offset: u32,
    end_offset: u32,
    payload: (usize, usize),
}

impl PosState {
    fn set_hint(&mut self, p: PosPointer) {
        self.hint_block = p.count / BLOCK as u64;
        self.hint_fp = self.term_pos + p.fp as usize;
        self.hint_pay_fp = self.term_pay + p.pay_fp as usize;
    }

    fn load_block(&mut self, block: u64, fp: usize, pay_fp: usize) {
        let mut p = fp;
        let mut pp = pay_fp;
        if block < self.full_blocks {
            pfor_decode(&self.pos, &mut p, &mut self.buf);
            self.len = BLOCK;
            if self.has_payloads {
                if self.want_payloads {
                    pfor_decode(&self.pay, &mut pp, &mut self.pay_len);
                    let n = read_vu32(&self.pay, &mut pp) as usize;
                    let end = (pp + n).min(self.pay.len());
                    self.pay_bytes.clear();
                    self.pay_bytes
                        .extend_from_slice(&self.pay[pp.min(end)..end]);
                    pp += n;
                    let mut acc = 0u32;
                    for i in 0..BLOCK {
                        self.pay_starts[i] = acc;
                        acc = acc.wrapping_add(self.pay_len[i]);
                    }
                    self.pay_starts[BLOCK] = acc;
                } else {
                    pfor_skip(&self.pay, &mut pp);
                    let n = read_vu32(&self.pay, &mut pp) as usize;
                    pp += n;
                }
            }
            if self.has_offsets {
                if self.want_offsets {
                    pfor_decode(&self.pay, &mut pp, &mut self.off_start);
                    pfor_decode(&self.pay, &mut pp, &mut self.off_len);
                } else {
                    pfor_skip(&self.pay, &mut pp);
                    pfor_skip(&self.pay, &mut pp);
                }
            }
        } else {
            let n = (self.ttf - self.full_blocks * BLOCK as u64).min(BLOCK as u64) as usize;
            for i in 0..n {
                self.buf[i] = read_vu32(&self.pos, &mut p);
            }
            self.len = n;
            if self.has_payloads || self.has_offsets {
                self.pay_bytes.clear();
                let mut acc = 0u32;
                for i in 0..n {
                    if self.has_payloads {
                        let l = read_vu32(&self.pay, &mut pp) as usize;
                        let end = (pp + l).min(self.pay.len());
                        self.pay_starts[i] = acc;
                        self.pay_len[i] = (end - pp.min(end)) as u32;
                        if self.want_payloads {
                            self.pay_bytes
                                .extend_from_slice(&self.pay[pp.min(end)..end]);
                        }
                        acc = acc.wrapping_add(self.pay_len[i]);
                        pp += l;
                    }
                    if self.has_offsets {
                        self.off_start[i] = read_vu32(&self.pay, &mut pp);
                        self.off_len[i] = read_vu32(&self.pay, &mut pp);
                    }
                }
                self.pay_starts[n] = acc;
            }
        }
        self.loaded = true;
        self.buf_block = block;
        self.next_fp = p;
        self.next_pay_fp = pp;
    }

    fn skip_block(&self, block: u64, fp: &mut usize, pay_fp: &mut usize) {
        debug_assert!(block < self.full_blocks);
        pfor_skip(&self.pos, fp);
        if self.has_payloads {
            pfor_skip(&self.pay, pay_fp);
            let n = read_vu32(&self.pay, pay_fp) as usize;
            *pay_fp += n;
        }
        if self.has_offsets {
            pfor_skip(&self.pay, pay_fp);
            pfor_skip(&self.pay, pay_fp);
        }
    }

    /// Positions the buffer on absolute position index `target`.
    fn seek(&mut self, target: u64) {
        let tb = target / BLOCK as u64;
        if self.loaded && tb == self.buf_block {
            self.idx = (target - tb * BLOCK as u64) as usize;
            return;
        }
        // Start from the closest known block start at or before `tb`.
        let (mut b, mut fp, mut pfp) =
            if self.loaded && self.buf_block < tb && self.buf_block + 1 > self.hint_block {
                (self.buf_block + 1, self.next_fp, self.next_pay_fp)
            } else {
                (self.hint_block, self.hint_fp, self.hint_pay_fp)
            };
        if b > tb {
            // Hint is past the target (cannot happen for well-formed data); restart the term.
            b = 0;
            fp = self.term_pos;
            pfp = self.term_pay;
        }
        while b < tb && b < self.full_blocks {
            self.skip_block(b, &mut fp, &mut pfp);
            b += 1;
        }
        self.load_block(b, fp, pfp);
        self.idx = (target - b * BLOCK as u64) as usize;
    }

    #[inline]
    fn next_position(&mut self) -> u32 {
        if !self.started {
            self.seek(self.doc_count);
            self.started = true;
            self.position = 0;
            self.last_start = 0;
        } else if self.idx >= self.len {
            let b = self.buf_block + 1;
            let (fp, pfp) = (self.next_fp, self.next_pay_fp);
            self.load_block(b, fp, pfp);
            self.idx = 0;
        }
        let i = self.idx.min(BLOCK - 1);
        self.position = self.position.wrapping_add(self.buf[i]);
        if self.has_offsets && self.want_offsets {
            self.start_offset = self.last_start.wrapping_add(self.off_start[i]);
            self.end_offset = self.start_offset.wrapping_add(self.off_len[i]);
            self.last_start = self.start_offset;
        }
        if self.has_payloads && self.want_payloads {
            let s = self.pay_starts[i] as usize;
            let e = self.pay_starts[i + 1] as usize;
            self.payload = (s.min(self.pay_bytes.len()), e.min(self.pay_bytes.len()));
        }
        self.idx += 1;
        self.position
    }
}

/// Header-walk state over a term's doc stream: the next block to load is `next_block`, whose
/// headers start at `fp`.
#[derive(Clone, Copy, Default)]
struct Skip {
    next_block: u32,
    fp: usize,
    prev_last: u32,
    /// Header of block `next_block`, parsed but not decoded.
    pending: Option<PendingBlock>,
    /// The current L1 group (set when its header is parsed).
    group_last: u32,
    group_impacts: (usize, usize),
    group_end_block: u32,
}

#[derive(Clone, Copy, Default)]
struct PendingBlock {
    last: u32,
    data_fp: usize,
    data_len: usize,
    impacts: (usize, usize),
    pointer: PosPointer,
}

/// The read-only part of a cursor: the doc stream and what it contains.
struct DocStream {
    docs: Slice,
    num_full: u32,
    has_freqs: bool,
    has_pos: bool,
    has_pay: bool,
}

impl DocStream {
    #[inline]
    fn read_pos_pointer(&self, p: &mut usize) -> PosPointer {
        if !self.has_pos {
            return PosPointer::default();
        }
        let fp = read_vu64(&self.docs, p);
        let count = read_vu64(&self.docs, p);
        let pay_fp = if self.has_pay { read_vu64(&self.docs, p) } else { 0 };
        PosPointer { fp, count, pay_fp }
    }

    #[inline]
    fn read_impacts_range(&self, p: &mut usize) -> (usize, usize) {
        if !self.has_freqs {
            return (0, 0);
        }
        let len = read_vu32(&self.docs, p) as usize;
        let r = (*p, len);
        *p += len;
        r
    }

    #[inline]
    fn group_starts(&self, block: u32) -> bool {
        block.is_multiple_of(GROUP) && block + GROUP <= self.num_full
    }

    /// Parses the header(s) of block `s.next_block` into `s.pending`.
    fn parse(&self, s: &mut Skip) {
        if s.pending.is_some() {
            return;
        }
        let mut p = s.fp;
        if self.group_starts(s.next_block) {
            let delta = read_vu32(&self.docs, &mut p);
            let _len = read_vu64(&self.docs, &mut p);
            s.group_impacts = self.read_impacts_range(&mut p);
            s.group_last = s.prev_last.wrapping_add(delta);
            s.group_end_block = s.next_block + GROUP;
            s.fp = p;
        }
        let delta = read_vu32(&self.docs, &mut p);
        let data_len = read_vu32(&self.docs, &mut p) as usize;
        let impacts = self.read_impacts_range(&mut p);
        let pointer = self.read_pos_pointer(&mut p);
        s.pending = Some(PendingBlock {
            last: s.prev_last.wrapping_add(delta),
            data_fp: p,
            data_len,
            impacts,
            pointer,
        });
    }

    /// Skips blocks (and whole groups) ending before `target`, leaving the first block that
    /// may contain it pending. Returns false when only the tail (or nothing) remains.
    fn skip_to(&self, s: &mut Skip, target: u32) -> bool {
        loop {
            if s.next_block >= self.num_full {
                return false;
            }
            if let Some(pb) = s.pending {
                if pb.last >= target {
                    return true;
                }
                s.fp = pb.data_fp + pb.data_len;
                s.prev_last = pb.last;
                s.next_block += 1;
                s.pending = None;
                continue;
            }
            if self.group_starts(s.next_block) {
                let mut p = s.fp;
                let delta = read_vu32(&self.docs, &mut p);
                let len = read_vu64(&self.docs, &mut p) as usize;
                let _ = self.read_impacts_range(&mut p);
                let last = s.prev_last.wrapping_add(delta);
                if last < target {
                    s.fp = p + len;
                    s.prev_last = last;
                    s.next_block += GROUP;
                    continue;
                }
            }
            self.parse(s);
        }
    }

    fn block_view(&self, s: &Skip, block: u32, last: u32, impacts: (usize, usize)) -> View {
        let in_group = s.group_end_block >= GROUP
            && block < s.group_end_block
            && block >= s.group_end_block - GROUP;
        View {
            block_last: last,
            block_impacts: impacts,
            group_last: s.group_last,
            group_impacts: s.group_impacts,
            levels: if in_group { 2 } else { 1 },
            special: 0,
        }
    }
}

/// Positions of the set bits of every byte, lowest first (unused slots 0).
static BYTE_BITS: [[u8; 8]; 256] = {
    let mut t = [[0u8; 8]; 256];
    let mut b = 0;
    while b < 256 {
        let mut k = 0;
        let mut bit = 0;
        while bit < 8 {
            if b >> bit & 1 != 0 {
                t[b][k] = bit as u8;
                k += 1;
            }
            bit += 1;
        }
        b += 1;
    }
    t
};

/// A dense block's docs as a bit set: doc `start + i` for every set bit `i`. Exactly 128 bits
/// are set (checked when loaded).
#[derive(Clone)]
struct BitsBlock {
    start: u32,
    nwords: usize,
    words: [u64; 64],
    /// Set bits before each word: the in-block index of its first doc.
    rank: [u8; 64],
    /// Word of the current doc, and that word's bits above the current doc.
    word: usize,
    rem: u64,
}

impl Default for BitsBlock {
    fn default() -> BitsBlock {
        BitsBlock { start: 0, nwords: 0, words: [0; 64], rank: [0; 64], word: 0, rem: 0 }
    }
}

impl BitsBlock {
    /// Loads the block's words from `data`; false when they are not a valid 128-doc block
    /// ending at `last`.
    fn load(&mut self, data: &[u8], start: u32, nwords: usize, last: u32) -> bool {
        let Some(raw) = data.get(..nwords * 8) else { return false };
        self.start = start;
        self.nwords = nwords;
        let mut count = 0u32;
        for (i, c) in raw.chunks_exact(8).enumerate() {
            let w = u64::from_le_bytes(c.try_into().unwrap());
            self.words[i] = w;
            self.rank[i] = count.min(255) as u8;
            count += w.count_ones();
        }
        let top = self.words[nwords - 1];
        count == BLOCK as u32
            && top != 0
            && start.checked_add((nwords as u32 - 1) * 64 + 63 - top.leading_zeros()) == Some(last)
    }

    /// 128 consecutive docs from `start`.
    fn consecutive(&mut self, start: u32) {
        self.start = start;
        self.nwords = 2;
        self.words[0] = u64::MAX;
        self.words[1] = u64::MAX;
        self.rank[0] = 0;
        self.rank[1] = 64;
    }

    /// The first doc (the cursor's state moves past it).
    #[inline]
    fn first(&mut self) -> u32 {
        self.word = 0;
        self.rem = self.words[0];
        self.take()
    }

    /// Every doc of the block, in order (128 of them): a byte at a time, each byte's set bit
    /// positions coming from a table and written as eight lanes (branch-free, vectorizes).
    #[inline]
    fn decode_into(&self, out: &mut [u32; BLOCK]) {
        let mut k = 0usize;
        for (i, &w) in self.words[..self.nwords].iter().enumerate() {
            if w == 0 {
                continue;
            }
            let mut base = self.start.wrapping_add(i as u32 * 64);
            for byte in w.to_le_bytes() {
                let n = byte.count_ones() as usize;
                let pos = &BYTE_BITS[byte as usize];
                if k + 8 <= BLOCK {
                    // All eight lanes fit: write them all (the unused ones are overwritten next).
                    let dst: &mut [u32; 8] = (&mut out[k..k + 8]).try_into().unwrap();
                    for j in 0..8 {
                        dst[j] = base.wrapping_add(pos[j] as u32);
                    }
                } else {
                    for j in 0..n.min(BLOCK - k) {
                        out[k + j] = base.wrapping_add(pos[j] as u32);
                    }
                }
                k += n;
                base = base.wrapping_add(8);
            }
        }
    }

    /// The next doc after the current one; one must exist.
    #[inline]
    fn take(&mut self) -> u32 {
        while self.rem == 0 && self.word + 1 < self.nwords {
            self.word += 1;
            self.rem = self.words[self.word];
        }
        let t = self.rem.trailing_zeros();
        self.rem &= self.rem.wrapping_sub(1);
        self.start + self.word as u32 * 64 + t
    }

    /// The first doc `>= target` (which must be within the block) and its in-block index.
    #[inline]
    fn seek(&mut self, target: u32) -> (u32, usize) {
        let rel = (target - self.start) as usize;
        let mut w = (rel >> 6).min(self.nwords - 1);
        let mut word = self.words[w] & (u64::MAX << (rel & 63));
        while word == 0 && w + 1 < self.nwords {
            w += 1;
            word = self.words[w];
        }
        let t = word.trailing_zeros();
        let below = self.words[w] & 1u64.checked_shl(t).unwrap_or(0).wrapping_sub(1);
        self.word = w;
        self.rem = word & word.wrapping_sub(1);
        (self.start + w as u32 * 64 + t, self.rank[w] as usize + below.count_ones() as usize)
    }

    /// ORs the docs `from..to` (in-block bit positions) into `out`, where bit `x` of `out` is
    /// doc `x + offset`.
    fn or_into(&self, from: usize, to: usize, out: &mut [u64], offset: u32) {
        let mut i = from;
        while i < to {
            let (w, sh) = (i >> 6, i & 63);
            let n = (64 - sh).min(to - i);
            let mask = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
            let chunk = (self.words[w] >> sh) & mask;
            if chunk != 0 {
                let dst = self.start as usize + i - offset as usize;
                let (dw, ds) = (dst >> 6, dst & 63);
                out[dw] |= chunk << ds;
                if ds + n > 64 {
                    out[dw + 1] |= chunk >> (64 - ds);
                }
            }
            i += n;
        }
    }
}

/// Iterates one term's postings: docs, freqs, positions, offsets, payloads and impacts.
///
/// Cursors follow Lucene's `PostingsEnum`/`ImpactsEnum` protocol: `doc()` is `u32::MAX` before
/// the first `next`/`advance` and [`NO_MORE_DOCS`] at the end; `advance` never moves backwards;
/// `advance_shallow` only moves the impacts view (a later `next()` still returns the next doc).
pub struct PostingsCursor {
    ds: DocStream,
    df: u32,
    ttf: u64,
    singleton: Option<u32>,
    term_doc_start: usize,

    doc: u32,
    docs_buf: [u32; BLOCK],
    freq_buf: [u32; BLOCK],
    buf_len: usize,
    buf_idx: usize,
    buf_last: u32,
    /// The current block is dense: its docs live in `bits`, not `docs_buf`.
    dense: bool,
    bits: BitsBlock,
    freq_fp: usize,
    freqs_decoded: bool,

    /// Iteration state (block after the decoded one).
    it: Skip,
    /// State moved ahead by `advance_shallow`, adopted by the next `advance` when safe.
    sh: Option<Skip>,
    view: View,
    cur_view: View,
    in_tail: bool,
    impacts: [Vec<Impact>; 2],

    pos: Option<Box<PosState>>,
    /// Buffer index of the doc whose first position `pos.doc_count` points at. Moving the
    /// cursor only marks positions stale; the frequencies of skipped docs are summed when a
    /// position is actually read (Lucene's pending positions), so advancing a positions
    /// cursor never decodes frequencies of blocks whose positions nobody reads.
    pos_doc_idx: usize,
}

/// Parts of a cursor kept across [`PostingsCursor::reset`].
struct Recycled {
    impacts: [Vec<Impact>; 2],
    pos: Option<Box<PosState>>,
    docs: Slice,
}

#[derive(Clone, Copy, Default)]
struct View {
    block_last: u32,
    block_impacts: (usize, usize),
    group_last: u32,
    group_impacts: (usize, usize),
    levels: u8,
    /// 0: stored impacts, 1: single doc, 2: nothing left.
    special: u8,
}

impl PostingsCursor {
    /// Opens a cursor over a term; `flags` beyond what the field indexes are ignored.
    pub fn new(data: &PostingsData, st: &TermState, flags: PostingsFlags) -> PostingsCursor {
        PostingsCursor::build(data, st, flags, None)
    }

    /// Repositions this cursor on another term (of any field), keeping its buffers: queries
    /// that visit many terms (multi-term rewrites, merges) then allocate nothing per term and
    /// do not touch the segment's shared reference count for terms of the same field.
    pub fn reset(&mut self, data: &PostingsData, st: &TermState, flags: PostingsFlags) {
        let parts = Recycled {
            impacts: std::mem::take(&mut self.impacts),
            pos: self.pos.take(),
            docs: std::mem::take(&mut self.ds.docs),
        };
        *self = PostingsCursor::build(data, st, flags, Some(parts));
    }

    fn build(data: &PostingsData, st: &TermState, flags: PostingsFlags, recycled: Option<Recycled>) -> PostingsCursor {
        let (mut impacts, mut old_pos, old_docs) = match recycled {
            Some(r) => (r.impacts, r.pos, Some(r.docs)),
            None => ([Vec::new(), Vec::new()], None, None),
        };
        impacts.iter_mut().for_each(Vec::clear);
        let docs = match old_docs {
            Some(d) if d.same_view(&data.docs) => d,
            _ => data.docs.clone(),
        };
        let has_freqs = data.opts.has_freqs();
        let has_pos = data.opts.has_positions();
        let has_offsets = data.opts.has_offsets();
        let has_payloads = data.payloads && has_pos;
        let want_pos = flags.positions() && has_pos;
        let pos = want_pos.then(|| {
            let (pos_slice, pay_slice, pay_bytes) = match old_pos.take() {
                Some(mut b) => {
                    let pos = std::mem::take(&mut b.pos);
                    let pay = std::mem::take(&mut b.pay);
                    let mut bytes = std::mem::take(&mut b.pay_bytes);
                    bytes.clear();
                    (
                        if pos.same_view(&data.pos) { pos } else { data.pos.clone() },
                        if pay.same_view(&data.pay) { pay } else { data.pay.clone() },
                        bytes,
                    )
                }
                None => (data.pos.clone(), data.pay.clone(), Vec::new()),
            };
            Box::new(PosState {
                pos: pos_slice,
                pay: pay_slice,
                want_offsets: flags.offsets() && has_offsets,
                want_payloads: flags.payloads() && has_payloads,
                has_offsets,
                has_payloads,
                term_pos: st.pos_fp as usize,
                term_pay: st.pay_fp as usize,
                full_blocks: st.total_term_freq / BLOCK as u64,
                ttf: st.total_term_freq,
                buf: [0; BLOCK],
                off_start: [0; BLOCK],
                off_len: [0; BLOCK],
                pay_len: [0; BLOCK],
                pay_starts: [0; BLOCK + 1],
                pay_bytes,
                len: 0,
                idx: 0,
                loaded: false,
                buf_block: 0,
                next_fp: st.pos_fp as usize,
                next_pay_fp: st.pay_fp as usize,
                hint_block: 0,
                hint_fp: st.pos_fp as usize,
                hint_pay_fp: st.pay_fp as usize,
                doc_count: 0,
                started: false,
                position: 0,
                last_start: 0,
                start_offset: 0,
                end_offset: 0,
                payload: (0, 0),
            })
        });
        let df = st.doc_freq;
        PostingsCursor {
            ds: DocStream {
                docs,
                num_full: if df > 1 { df / BLOCK as u32 } else { 0 },
                has_freqs,
                has_pos,
                has_pay: has_offsets || has_payloads,
            },
            df,
            ttf: st.total_term_freq,
            singleton: (df == 1).then_some(st.singleton),
            term_doc_start: st.doc_fp as usize,
            doc: u32::MAX,
            docs_buf: [0; BLOCK],
            freq_buf: [1; BLOCK],
            buf_len: 0,
            buf_idx: 0,
            buf_last: 0,
            dense: false,
            bits: BitsBlock::default(),
            freq_fp: 0,
            freqs_decoded: true,
            pos_doc_idx: 0,
            it: Skip { fp: st.doc_fp as usize, ..Skip::default() },
            sh: None,
            view: View::default(),
            cur_view: View::default(),
            in_tail: false,
            impacts,
            pos,
        }
    }

    /// An exhausted cursor (for terms that do not exist).
    pub fn empty() -> PostingsCursor {
        let data = PostingsData {
            docs: Slice::empty(),
            pos: Slice::empty(),
            pay: Slice::empty(),
            opts: IndexOptions::Docs,
            payloads: false,
        };
        let mut c = PostingsCursor::new(&data, &TermState::default(), PostingsFlags::DOCS);
        c.doc = NO_MORE_DOCS;
        c
    }

    /// Current doc: `u32::MAX` before the first `next`/`advance`, then a doc id, then
    /// [`NO_MORE_DOCS`].
    #[inline]
    pub fn doc(&self) -> u32 {
        self.doc
    }

    /// Doc frequency of the term (an upper bound of the docs this cursor returns).
    #[inline]
    pub fn cost(&self) -> u64 {
        self.df as u64
    }

    pub fn doc_freq(&self) -> u32 {
        self.df
    }

    pub fn total_term_freq(&self) -> u64 {
        self.ttf
    }

    /// Decodes the next block (full, tail or singleton) into the buffers.
    fn load_next_block(&mut self) -> bool {
        self.pos_doc_idx = 0;
        if self.it.next_block < self.ds.num_full {
            self.ds.parse(&mut self.it);
            let pb = self.it.pending.take().unwrap_or_default();
            let mut p = pb.data_fp;
            let head = self.ds.docs.get(p).copied().unwrap_or(0);
            let start = if self.it.next_block == 0 { 0 } else { self.it.prev_last.wrapping_add(1) };
            // Corrupt (unverified) data may not decode to a block ending where its header says,
            // or claim a block that does not move past the previous one by its 128 docs (which
            // would let a corrupt doc count spin the cursor); the cursor then stops.
            if (self.it.next_block > 0 && start == 0) || start.checked_add(BLOCK as u32 - 1).is_none_or(|m| pb.last < m) {
                return false;
            }
            if head == DOCS_CONSECUTIVE {
                p += 1;
                self.bits.consecutive(start);
                self.dense = true;
                if start.checked_add(BLOCK as u32 - 1) != Some(pb.last) {
                    return false;
                }
            } else if head & 0xC0 == DOCS_BITSET {
                p += 1;
                let nwords = (head & 63) as usize + 1;
                let data = self.ds.docs.get(p..).unwrap_or(&[]);
                self.dense = true;
                if !self.bits.load(data, start, nwords, pb.last) {
                    return false;
                }
                p += nwords * 8;
            } else {
                self.dense = false;
                for_decode(&self.ds.docs, &mut p, &mut self.docs_buf);
                bitpack::prefix_sum(&mut self.docs_buf, self.it.prev_last);
                if self.docs_buf[BLOCK - 1] != pb.last {
                    return false;
                }
            }
            self.buf_len = BLOCK;
            if self.ds.has_freqs {
                self.freq_fp = p;
                self.freqs_decoded = false;
            }
            self.cur_view = self.ds.block_view(&self.it, self.it.next_block, pb.last, pb.impacts);
            self.view = self.cur_view;
            self.it.fp = pb.data_fp + pb.data_len;
            self.it.prev_last = pb.last;
            self.it.next_block += 1;
            self.buf_last = pb.last;
            if self.sh.is_some_and(|s| s.next_block <= self.it.next_block) {
                self.sh = None;
            }
            if let Some(ps) = self.pos.as_deref_mut() {
                ps.set_hint(pb.pointer);
                ps.doc_count = pb.pointer.count;
            }
            true
        } else if !self.in_tail && self.df > 1 && !self.df.is_multiple_of(BLOCK as u32) {
            self.in_tail = true;
            self.sh = None;
            let mut p = self.it.fp;
            let impacts = self.ds.read_impacts_range(&mut p);
            self.cur_view = View {
                block_last: NO_MORE_DOCS,
                block_impacts: impacts,
                levels: 1,
                ..View::default()
            };
            self.view = self.cur_view;
            let pointer = self.ds.read_pos_pointer(&mut p);
            let n = (self.df % BLOCK as u32) as usize;
            self.dense = false;
            let mut d = self.it.prev_last;
            let docs = &self.ds.docs;
            for i in 0..n {
                let v = read_vu32(docs, &mut p);
                if self.ds.has_freqs {
                    d = d.wrapping_add(v >> 1);
                    self.freq_buf[i] = if v & 1 != 0 { 1 } else { read_vu32(docs, &mut p) };
                } else {
                    d = d.wrapping_add(v);
                }
                self.docs_buf[i] = d;
            }
            self.freqs_decoded = true;
            self.buf_len = n;
            self.buf_last = d;
            if let Some(ps) = self.pos.as_deref_mut() {
                ps.set_hint(pointer);
                ps.doc_count = pointer.count;
            }
            true
        } else if let Some(s) = self.singleton.take() {
            self.dense = false;
            self.docs_buf[0] = s;
            self.freq_buf[0] = self.ttf.min(u32::MAX as u64) as u32;
            self.freqs_decoded = true;
            self.buf_len = 1;
            self.buf_last = s;
            self.cur_view = View {
                block_last: NO_MORE_DOCS,
                levels: 1,
                special: 1,
                ..View::default()
            };
            self.view = self.cur_view;
            self.in_tail = true;
            if let Some(ps) = self.pos.as_deref_mut() {
                ps.doc_count = 0;
            }
            true
        } else {
            false
        }
    }

    #[inline]
    fn ensure_freqs(&mut self) {
        if !self.freqs_decoded {
            let mut p = self.freq_fp;
            pfor_decode(&self.ds.docs, &mut p, &mut self.freq_buf);
            for f in self.freq_buf.iter_mut() {
                *f = f.wrapping_add(1);
            }
            self.freqs_decoded = true;
        }
    }

    /// Marks positions stale after moving within the buffer (see `pos_doc_idx`).
    #[inline]
    fn pos_advance_docs(&mut self, _from: usize, _to: usize) {
        if let Some(ps) = self.pos.as_deref_mut() {
            ps.started = false;
        }
    }

    /// Advances to the next doc (the step inside a decoded block is inlined into callers).
    #[inline]
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u32 {
        if self.doc != u32::MAX && self.doc != NO_MORE_DOCS && !self.dense && self.buf_idx + 1 < self.buf_len {
            if self.pos.is_some() {
                self.pos_advance_docs(self.buf_idx, self.buf_idx + 1);
            }
            self.buf_idx += 1;
            self.doc = self.docs_buf[self.buf_idx];
            return self.doc;
        }
        self.next_slow()
    }

    #[inline(never)]
    fn next_slow(&mut self) -> u32 {
        if self.doc == NO_MORE_DOCS {
            return NO_MORE_DOCS;
        }
        let started = self.doc != u32::MAX;
        if started && self.buf_idx + 1 < self.buf_len {
            if self.dense {
                // Sequential iteration: decode the bit set once (advance and into_bitset keep
                // using the bits until then).
                self.bits.decode_into(&mut self.docs_buf);
                self.dense = false;
            }
            if self.pos.is_some() {
                self.pos_advance_docs(self.buf_idx, self.buf_idx + 1);
            }
            self.buf_idx += 1;
            self.doc = self.docs_buf[self.buf_idx];
            return self.doc;
        }
        if !self.load_next_block() {
            self.doc = NO_MORE_DOCS;
            return NO_MORE_DOCS;
        }
        if let Some(ps) = self.pos.as_deref_mut() {
            ps.started = false;
        }
        self.buf_idx = 0;
        self.doc = if self.dense { self.bits.first() } else { self.docs_buf[0] };
        self.doc
    }

    /// Advances to the first doc `>= target` (never backwards).
    ///
    /// The common cases — already there, or the target inside the decoded block — are inlined
    /// into callers; moving to another block takes the out-of-line path.
    #[inline]
    pub fn advance(&mut self, target: u32) -> u32 {
        let d = self.doc;
        if d != u32::MAX {
            // NO_MORE_DOCS is above every target.
            if d >= target {
                return d;
            }
            if target <= self.buf_last && self.buf_len > 0 {
                let idx = if self.dense {
                    let (doc, idx) = self.bits.seek(target.max(self.bits.start));
                    if self.pos.is_some() && idx > self.buf_idx {
                        self.pos_advance_docs(self.buf_idx, idx);
                    }
                    self.buf_idx = idx;
                    self.doc = doc;
                    return doc;
                } else {
                    find_next_geq(&self.docs_buf[..self.buf_len], self.buf_idx, target)
                };
                if idx < self.buf_len {
                    if self.pos.is_some() && idx > self.buf_idx {
                        self.pos_advance_docs(self.buf_idx, idx);
                    }
                    self.buf_idx = idx;
                    self.doc = self.docs_buf[idx];
                    return self.doc;
                }
            }
        }
        self.advance_slow(target)
    }

    #[inline(never)]
    fn advance_slow(&mut self, target: u32) -> u32 {
        if self.doc == NO_MORE_DOCS {
            return NO_MORE_DOCS;
        }
        let started = self.doc != u32::MAX;
        if started && self.doc >= target {
            return self.doc;
        }
        let start_idx;
        if started && self.buf_len > 0 && target <= self.buf_last {
            start_idx = self.buf_idx;
        } else {
            // Adopt the shallow state when it is ahead and every doc it skipped is < target.
            if let Some(s) = self.sh.take()
                && s.next_block > self.it.next_block
                && s.prev_last < target
            {
                self.it = s;
            }
            self.ds.skip_to(&mut self.it, target);
            if !self.load_next_block() {
                self.doc = NO_MORE_DOCS;
                return NO_MORE_DOCS;
            }
            if let Some(ps) = self.pos.as_deref_mut() {
                ps.started = false;
            }
            if target > self.buf_last {
                // Only possible in the tail or singleton: nothing left at or above target.
                self.doc = NO_MORE_DOCS;
                return NO_MORE_DOCS;
            }
            start_idx = 0;
            self.buf_idx = 0;
        }
        if self.dense {
            // target is within this block and above the current doc (or the block is new).
            let (d, idx) = self.bits.seek(target.max(self.bits.start));
            if self.pos.is_some() && idx > self.buf_idx {
                self.pos_advance_docs(self.buf_idx, idx);
            }
            self.buf_idx = idx;
            self.doc = d;
            return d;
        }
        let idx = find_next_geq(&self.docs_buf[..self.buf_len], start_idx, target);
        if idx >= self.buf_len || self.docs_buf[idx] < target {
            // Only reachable with corrupt (unverified) data: stop rather than misbehave.
            self.doc = NO_MORE_DOCS;
            return NO_MORE_DOCS;
        }
        if self.pos.is_some() && idx > self.buf_idx {
            self.pos_advance_docs(self.buf_idx, idx);
        }
        self.buf_idx = idx;
        self.doc = self.docs_buf[idx];
        self.doc
    }

    /// Copies the current doc and the following docs below `upto` that are in the same decoded
    /// block into `docs`/`freqs` (frequencies are 1 without them), then moves the cursor to the
    /// next doc after the batch (Lucene's `nextPostings`). Returns the batch size: 0 when the
    /// cursor is unpositioned, exhausted or at a doc `>= upto`. Batches let scorers score a
    /// block at a time instead of a call per doc.
    pub fn next_postings(&mut self, upto: u32, docs: &mut [u32; BLOCK], freqs: &mut [u32; BLOCK]) -> usize {
        let d = self.doc;
        if d == u32::MAX || d == NO_MORE_DOCS || d >= upto {
            return 0;
        }
        if self.pos.is_some() {
            // Position bookkeeping is per doc.
            let mut n = 0;
            let mut d = d;
            while d < upto && n < BLOCK {
                docs[n] = d;
                freqs[n] = self.freq();
                n += 1;
                d = self.next();
            }
            return n;
        }
        if self.dense {
            self.bits.decode_into(&mut self.docs_buf);
            self.dense = false;
        }
        let start = self.buf_idx;
        let end = find_next_geq(&self.docs_buf[..self.buf_len], start, upto);
        let n = end - start;
        docs[..n].copy_from_slice(&self.docs_buf[start..end]);
        if self.ds.has_freqs {
            self.ensure_freqs();
            freqs[..n].copy_from_slice(&self.freq_buf[start..end]);
        } else {
            freqs[..n].fill(1);
        }
        if end < self.buf_len {
            self.buf_idx = end;
            self.doc = self.docs_buf[end];
        } else {
            self.buf_idx = self.buf_len - 1;
            self.doc = self.docs_buf[self.buf_idx];
            self.next();
        }
        n
    }

    /// Sets bit `doc - offset` in `bits` for the current doc and every following doc below
    /// `upto`, leaving the cursor on the first doc `>= upto` (Lucene's `intoBitSet`). The cursor
    /// must be positioned (`next`/`advance` called). Whole decoded blocks are consumed without
    /// per-doc calls.
    pub fn into_bitset(&mut self, upto: u32, bits: &mut [u64], offset: u32) {
        if self.pos.is_some() {
            while self.doc < upto {
                let o = (self.doc - offset) as usize;
                bits[o >> 6] |= 1 << (o & 63);
                self.next();
            }
            return;
        }
        while self.doc < upto && self.doc != NO_MORE_DOCS {
            if self.dense {
                // Whole words at a time.
                let b = &self.bits;
                let end = upto.min(self.buf_last.saturating_add(1));
                b.or_into((self.doc - b.start) as usize, (end - b.start) as usize, bits, offset);
                if upto <= self.buf_last {
                    let (d, idx) = self.bits.seek(upto);
                    self.buf_idx = idx;
                    self.doc = d;
                    return;
                }
                self.buf_idx = self.buf_len - 1;
                self.doc = self.buf_last;
                self.next();
                continue;
            }
            let mut i = self.buf_idx;
            while i < self.buf_len {
                let d = self.docs_buf[i];
                if d >= upto {
                    self.buf_idx = i;
                    self.doc = d;
                    return;
                }
                let o = (d - offset) as usize;
                bits[o >> 6] |= 1 << (o & 63);
                i += 1;
            }
            self.buf_idx = self.buf_len.saturating_sub(1);
            self.doc = self.docs_buf[self.buf_idx];
            self.next();
        }
    }

    /// The current block's docs as a bit set when it is stored dense and the cursor got there
    /// with `advance` (`next` switches to the decoded list): `(start, words, last)` where bit
    /// `i` of `words` is doc `start + i` and `last` is the block's last doc. Conjunctions of
    /// frequent terms AND these words instead of leapfrogging doc by doc; moving within the
    /// block with `advance` is then a cheap seek.
    #[inline]
    pub fn dense_block(&self) -> Option<(u32, &[u64], u32)> {
        if self.dense && self.doc != u32::MAX && self.doc != NO_MORE_DOCS {
            Some((self.bits.start, &self.bits.words[..self.bits.nwords], self.buf_last))
        } else {
            None
        }
    }

    /// The end (exclusive) of the run of consecutive docs starting at the current doc, as far
    /// as the current block knows (Lucene's `docIDRunEnd`): every doc in `doc()..end` matches.
    /// At least `doc() + 1`; the cursor does not move.
    pub fn doc_id_run_end(&self) -> u32 {
        let d = self.doc;
        if d == u32::MAX || d == NO_MORE_DOCS {
            return d.saturating_add(1);
        }
        if self.dense {
            let b = &self.bits;
            let mut p = (d - b.start) as usize;
            loop {
                let (w, sh) = (p >> 6, p & 63);
                if w >= b.nwords {
                    break;
                }
                let ones = (!(b.words[w] >> sh)).trailing_zeros() as usize;
                p += ones;
                if sh + ones < 64 {
                    break;
                }
            }
            return b.start + p as u32;
        }
        let mut i = self.buf_idx;
        while i + 1 < self.buf_len && self.docs_buf[i + 1] == self.docs_buf[i].wrapping_add(1) {
            i += 1;
        }
        self.docs_buf[i].wrapping_add(1).max(d + 1)
    }

    /// Frequency of the current doc (1 for fields without frequencies).
    #[inline]
    pub fn freq(&mut self) -> u32 {
        if !self.ds.has_freqs {
            return 1;
        }
        self.ensure_freqs();
        self.freq_buf[self.buf_idx]
    }

    /// Next position of the current doc; call at most `freq()` times per doc.
    #[inline]
    pub fn next_position(&mut self) -> u32 {
        // Fast path (phrase matching reads positions one at a time): a started doc whose next
        // position delta is buffered, with no offsets or payloads to decode alongside.
        if let Some(ps) = self.pos.as_deref_mut()
            && ps.started
            && ps.idx < ps.len
            && !ps.want_offsets
            && !ps.want_payloads
        {
            ps.position = ps.position.wrapping_add(ps.buf[ps.idx & (BLOCK - 1)]);
            ps.idx += 1;
            return ps.position;
        }
        self.next_position_slow()
    }

    #[inline(never)]
    fn next_position_slow(&mut self) -> u32 {
        let Some(started) = self.pos.as_deref().map(|p| p.started) else { return 0 };
        if !started && self.buf_idx > self.pos_doc_idx {
            // First position of this doc: skip the positions of the docs passed since.
            self.ensure_freqs();
            let sum: u64 = self.freq_buf[self.pos_doc_idx..self.buf_idx].iter().map(|&f| f as u64).sum();
            self.pos.as_deref_mut().unwrap().doc_count += sum;
            self.pos_doc_idx = self.buf_idx;
        }
        self.pos.as_deref_mut().unwrap().next_position()
    }

    /// Start offset of the last position (offsets must have been requested and indexed).
    pub fn start_offset(&self) -> u32 {
        self.pos.as_deref().map_or(0, |p| p.start_offset)
    }

    pub fn end_offset(&self) -> u32 {
        self.pos.as_deref().map_or(0, |p| p.end_offset)
    }

    /// Payload of the last position (empty when none).
    pub fn payload(&self) -> &[u8] {
        match self.pos.as_deref() {
            Some(p) if p.want_payloads => &p.pay_bytes[p.payload.0..p.payload.1],
            _ => &[],
        }
    }

    /// Moves the impacts view to the block holding `target` without decoding docs or moving
    /// the doc iterator.
    pub fn advance_shallow(&mut self, target: u32) {
        let started = self.doc != u32::MAX && self.doc != NO_MORE_DOCS;
        if (started && target <= self.buf_last) || self.in_tail {
            self.view = self.cur_view;
            return;
        }
        if self.doc == NO_MORE_DOCS {
            self.view = View { block_last: NO_MORE_DOCS, levels: 1, special: 2, ..View::default() };
            return;
        }
        if self.df == 1 {
            self.view = View { block_last: NO_MORE_DOCS, levels: 1, special: 1, ..View::default() };
            return;
        }
        let mut s = match self.sh {
            Some(s) if s.prev_last < target => s,
            _ => self.it,
        };
        if self.ds.skip_to(&mut s, target) {
            let pb = s.pending.unwrap_or_default();
            self.view = self.ds.block_view(&s, s.next_block, pb.last, pb.impacts);
        } else if !self.df.is_multiple_of(BLOCK as u32) {
            let mut p = s.fp;
            let impacts = self.ds.read_impacts_range(&mut p);
            self.view = View {
                block_last: NO_MORE_DOCS,
                block_impacts: impacts,
                levels: 1,
                ..View::default()
            };
        } else {
            self.view = View { block_last: NO_MORE_DOCS, levels: 1, special: 2, ..View::default() };
        }
        self.sh = Some(s);
    }

    /// Number of impact levels for the current position (see [`impacts`](Self::impacts)).
    pub fn impact_levels(&mut self) -> usize {
        self.ensure_impacts_state();
        self.view.levels as usize
    }

    /// Last doc covered by impact level `level` (inclusive); [`NO_MORE_DOCS`] for the last
    /// region of the term.
    pub fn impact_doc_upto(&mut self, level: usize) -> u32 {
        self.ensure_impacts_state();
        if level == 0 || self.view.levels < 2 {
            self.view.block_last
        } else {
            self.view.group_last
        }
    }

    /// Competitive `(freq, norm)` pairs of level `level`, sorted by increasing freq and norm.
    /// Empty when the cursor is exhausted.
    pub fn impacts(&mut self, level: usize) -> &[Impact] {
        self.ensure_impacts_state();
        let level = if self.view.levels < 2 { 0 } else { level.min(1) };
        let mut v = std::mem::take(&mut self.impacts[level]);
        v.clear();
        match self.view.special {
            2 => {}
            1 => v.push(Impact {
                freq: if self.ds.has_freqs { self.ttf.min(u32::MAX as u64) as u32 } else { 1 },
                norm: 0,
            }),
            _ if !self.ds.has_freqs => v.push(Impact { freq: 1, norm: NO_NORM }),
            _ => {
                let (s, l) = if level == 0 { self.view.block_impacts } else { self.view.group_impacts };
                let docs = &self.ds.docs;
                let end = (s + l).min(docs.len());
                decode_impacts(&docs[s.min(end)..end], &mut v);
            }
        }
        self.impacts[level] = v;
        &self.impacts[level]
    }

    /// Before the first doc, describe the first block.
    fn ensure_impacts_state(&mut self) {
        if self.doc == u32::MAX && self.view.levels == 0 {
            self.advance_shallow(0);
        }
    }

    /// Upper bound of any doc's frequency in this term (`ttf - df + 1`), without I/O.
    pub fn max_freq_bound(&self) -> u32 {
        (self.ttf - self.df as u64 + 1).min(u32::MAX as u64) as u32
    }

    /// Offset of this term's postings in the doc stream (diagnostics).
    pub fn term_doc_fp(&self) -> usize {
        self.term_doc_start
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    #[test]
    fn find_next_geq_matches_linear_scan() {
        let mut rng = StdRng::seed_from_u64(3);
        for _ in 0..2000 {
            let n = rng.gen_range(0..=BLOCK);
            let mut d = rng.gen_range(0..5u32);
            let docs: Vec<u32> = (0..n)
                .map(|_| {
                    d += rng.gen_range(1..6);
                    d
                })
                .collect();
            let from = rng.gen_range(0..=n);
            let target = rng.gen_range(0..d + 8);
            let want = (from..n).find(|&i| docs[i] >= target).unwrap_or(n);
            assert_eq!(find_next_geq(&docs, from, target), want, "{docs:?} from {from} target {target}");
        }
    }

    #[derive(Clone, Debug)]
    struct Posting {
        doc: u32,
        positions: Vec<(u32, u32, u32, Vec<u8>)>,
        freq: u32,
    }

    fn gen_term(rng: &mut StdRng, df: usize, max_doc: u32, with_pos: bool) -> Vec<Posting> {
        let mut docs: Vec<u32> = Vec::with_capacity(df);
        if df as u32 * 2 > max_doc {
            let mut all: Vec<u32> = (0..max_doc).collect();
            for i in 0..df {
                let j = rng.gen_range(i..all.len());
                all.swap(i, j);
            }
            docs.extend_from_slice(&all[..df]);
        } else {
            let mut set = std::collections::BTreeSet::new();
            while set.len() < df {
                set.insert(rng.gen_range(0..max_doc));
            }
            docs.extend(set);
        }
        docs.sort_unstable();
        docs.iter()
            .map(|&doc| {
                let freq = if rng.gen_bool(0.7) {
                    1
                } else {
                    rng.gen_range(1..40)
                };
                let mut positions = Vec::new();
                if with_pos {
                    let mut p = 0u32;
                    let mut off = 0u32;
                    for _ in 0..freq {
                        p += rng.gen_range(0..20);
                        off += rng.gen_range(0..30);
                        let len = rng.gen_range(0..12);
                        let pl: Vec<u8> = if rng.gen_bool(0.3) {
                            (0..rng.gen_range(0..5)).map(|_| rng.r#gen()).collect()
                        } else {
                            Vec::new()
                        };
                        positions.push((p, off, off + len, pl));
                    }
                }
                Posting {
                    doc,
                    positions,
                    freq,
                }
            })
            .collect()
    }

    fn write_terms(
        opts: IndexOptions,
        payloads: bool,
        norms: &[u8],
        terms: &[Vec<Posting>],
    ) -> (PostingsData, Vec<TermState>) {
        let mut w = PostingsWriter::new(
            opts,
            payloads,
            if norms.is_empty() { None } else { Some(norms) },
        );
        let mut states = Vec::new();
        for t in terms {
            w.start_term();
            for p in t {
                w.start_doc(p.doc, p.freq);
                for (pos, s, e, pl) in &p.positions {
                    w.add_position(*pos, *s, *e, pl);
                }
                w.finish_doc();
            }
            states.push(w.finish_term());
        }
        let data = PostingsData {
            docs: Slice::from(std::mem::take(&mut w.docs)),
            pos: Slice::from(std::mem::take(&mut w.pos)),
            pay: Slice::from(std::mem::take(&mut w.pay)),
            opts,
            payloads,
        };
        (data, states)
    }

    fn check_iteration(data: &PostingsData, st: &TermState, t: &[Posting], flags: PostingsFlags) {
        let mut c = PostingsCursor::new(data, st, flags);
        assert_eq!(c.doc(), u32::MAX);
        for p in t {
            assert_eq!(c.next(), p.doc);
            if data.opts.has_freqs() && flags.freqs() {
                assert_eq!(c.freq(), p.freq, "freq doc {}", p.doc);
            }
            if flags.positions() && data.opts.has_positions() {
                for (pos, s, e, pl) in &p.positions {
                    assert_eq!(c.next_position(), *pos);
                    if flags.offsets() && data.opts.has_offsets() {
                        assert_eq!((c.start_offset(), c.end_offset()), (*s, *e));
                    }
                    if flags.payloads() && data.payloads {
                        assert_eq!(c.payload(), &pl[..]);
                    }
                }
            }
        }
        assert_eq!(c.next(), NO_MORE_DOCS);
        assert_eq!(c.next(), NO_MORE_DOCS);
    }

    fn check_advance(
        rng: &mut StdRng,
        data: &PostingsData,
        st: &TermState,
        t: &[Posting],
        flags: PostingsFlags,
    ) {
        let mut c = PostingsCursor::new(data, st, flags);
        let mut cur = 0u32;
        let max = t.last().map_or(0, |p| p.doc) + 3;
        loop {
            let jump = match rng.gen_range(0..4) {
                0 => 1,
                1 => rng.gen_range(1..300),
                2 => rng.gen_range(1..20_000),
                _ => rng.gen_range(1..3),
            };
            let target = cur + jump;
            let expect = t.iter().position(|p| p.doc >= target);
            let got = c.advance(target);
            match expect {
                None => {
                    assert_eq!(got, NO_MORE_DOCS, "advance {target}");
                    break;
                }
                Some(i) => {
                    assert_eq!(got, t[i].doc, "advance {target}");
                    if data.opts.has_freqs() && flags.freqs() && rng.gen_bool(0.5) {
                        assert_eq!(c.freq(), t[i].freq);
                    }
                    if flags.positions() && data.opts.has_positions() && rng.gen_bool(0.5) {
                        let k = rng.gen_range(0..=t[i].positions.len());
                        for (pos, s, e, pl) in &t[i].positions[..k] {
                            assert_eq!(c.next_position(), *pos, "doc {}", t[i].doc);
                            if flags.offsets() && data.opts.has_offsets() {
                                assert_eq!((c.start_offset(), c.end_offset()), (*s, *e));
                            }
                            if flags.payloads() && data.payloads {
                                assert_eq!(c.payload(), &pl[..]);
                            }
                        }
                    }
                    cur = got;
                    if rng.gen_bool(0.3) {
                        let n = c.next();
                        let e = t.get(i + 1).map_or(NO_MORE_DOCS, |p| p.doc);
                        assert_eq!(n, e);
                        if n == NO_MORE_DOCS {
                            break;
                        }
                        cur = n;
                    }
                }
            }
            if cur > max {
                break;
            }
        }
    }

    #[test]
    fn round_trips_all_options_and_sizes() {
        let mut rng = StdRng::seed_from_u64(99);
        let max_doc = 300_000u32;
        let norms: Vec<u8> = (0..max_doc).map(|_| rng.r#gen()).collect();
        let dfs = [
            1usize,
            2,
            127,
            128,
            129,
            255,
            256,
            257,
            4095,
            4096,
            4097,
            8192 + 129,
            40_000,
            299_000,
        ];
        for opts in [
            IndexOptions::Docs,
            IndexOptions::DocsAndFreqs,
            IndexOptions::DocsAndFreqsAndPositions,
            IndexOptions::DocsAndFreqsAndPositionsAndOffsets,
        ] {
            for payloads in [false, true] {
                if payloads && !opts.has_positions() {
                    continue;
                }
                let with_pos = opts.has_positions();
                let terms: Vec<Vec<Posting>> = dfs
                    .iter()
                    .map(|&df| {
                        let df = if with_pos { df.min(20_000) } else { df };
                        let mut t = gen_term(&mut rng, df, max_doc, with_pos);
                        if !opts.has_freqs() {
                            t.iter_mut().for_each(|p| p.freq = 1);
                        }
                        t
                    })
                    .collect();
                let (data, states) = write_terms(opts, payloads, &norms, &terms);
                for (t, st) in terms.iter().zip(&states) {
                    assert_eq!(st.doc_freq as usize, t.len());
                    for flags in [
                        PostingsFlags::DOCS,
                        PostingsFlags::FREQS,
                        PostingsFlags::ALL,
                    ] {
                        check_iteration(&data, st, t, flags);
                        check_advance(&mut rng, &data, st, t, flags);
                    }
                }
            }
        }
    }

    fn brute_impacts(t: &[Posting], norms: &[u8], lo: u32, hi: u32) -> Vec<Impact> {
        let mut acc = ImpactAcc::new();
        for p in t.iter().filter(|p| p.doc >= lo && p.doc <= hi) {
            acc.add(p.freq, norms[p.doc as usize]);
        }
        let mut buf = Vec::new();
        acc.write(&mut buf);
        let mut pos = 0;
        let len = read_vu32(&buf, &mut pos) as usize;
        let mut out = Vec::new();
        decode_impacts(&buf[pos..pos + len], &mut out);
        out
    }

    #[test]
    fn next_postings_batches_match_iteration() {
        let mut rng = StdRng::seed_from_u64(11);
        for (opts, with_pos) in [
            (IndexOptions::Docs, false),
            (IndexOptions::DocsAndFreqs, false),
            (IndexOptions::DocsAndFreqsAndPositions, true),
        ] {
            let max_doc = 20_000u32;
            // Sparse, medium, dense (bit set blocks) and consecutive terms, plus a singleton.
            let terms = vec![
                gen_term(&mut rng, 1, max_doc, with_pos),
                gen_term(&mut rng, 90, max_doc, with_pos),
                gen_term(&mut rng, 128 * 9 + 5, max_doc, with_pos),
                gen_term(&mut rng, 15_000, max_doc, with_pos),
                gen_term(&mut rng, max_doc as usize, max_doc, with_pos),
            ];
            let (data, states) = write_terms(opts, false, &[], &terms);
            let flags = if with_pos { PostingsFlags::POSITIONS } else { PostingsFlags::FREQS };
            for (t, st) in terms.iter().zip(&states) {
                let has_freqs = opts != IndexOptions::Docs;
                let want: Vec<(u32, u32)> = t.iter().map(|p| (p.doc, if has_freqs { p.freq } else { 1 })).collect();
                let mut c = PostingsCursor::new(&data, st, flags);
                let (mut docs, mut freqs) = ([0u32; BLOCK], [0u32; BLOCK]);
                let mut got = Vec::new();
                let mut skipped: Vec<(u32, u32)> = Vec::new();
                assert_eq!(c.next_postings(u32::MAX, &mut docs, &mut freqs), 0, "unpositioned");
                c.next();
                let mut upto = 0u32;
                while c.doc() != NO_MORE_DOCS {
                    upto = upto.max(c.doc()) + rng.gen_range(0..400);
                    if rng.gen_bool(0.2) {
                        // Interleave advances: the docs in [doc, target) are skipped.
                        let before = c.doc();
                        let target = before + rng.gen_range(1..300);
                        c.advance(target);
                        skipped.push((before, target));
                        continue;
                    }
                    let n = c.next_postings(upto, &mut docs, &mut freqs);
                    assert!(n > 0 || c.doc() >= upto);
                    for i in 0..n {
                        assert!(docs[i] < upto && docs[i] < c.doc());
                        got.push((docs[i], freqs[i]));
                    }
                }
                let want: Vec<(u32, u32)> =
                    want.into_iter().filter(|&(d, _)| !skipped.iter().any(|&(a, b)| d >= a && d < b)).collect();
                assert_eq!(got, want, "{opts:?} df {}", t.len());
            }
        }
    }

    #[test]
    fn impacts_bound_every_block() {
        let mut rng = StdRng::seed_from_u64(5);
        let max_doc = 2_000_000u32;
        let norms: Vec<u8> = (0..max_doc).map(|_| rng.r#gen()).collect();
        let terms = vec![
            gen_term(&mut rng, 70, max_doc, false),
            gen_term(&mut rng, 128 * 40 + 77, max_doc, false),
            gen_term(&mut rng, 128 * 64, max_doc, false),
        ];
        let (data, states) = write_terms(IndexOptions::DocsAndFreqs, false, &norms, &terms);
        for (t, st) in terms.iter().zip(&states) {
            let mut c = PostingsCursor::new(&data, st, PostingsFlags::FREQS);
            // Before iteration: level 0 describes the first block.
            let first_upto = c.impact_doc_upto(0);
            let first = c.impacts(0).to_vec();
            assert_eq!(first, brute_impacts(t, &norms, 0, first_upto));
            let mut target = 0u32;
            let mut seen_levels = 0;
            while target < max_doc {
                c.advance_shallow(target);
                let levels = c.impact_levels();
                seen_levels = seen_levels.max(levels);
                let mut lo = target;
                for l in 0..levels {
                    let upto = c.impact_doc_upto(l);
                    assert!(upto >= target);
                    let got = c.impacts(l).to_vec();
                    let want = brute_impacts(t, &norms, lo.min(target), upto);
                    // Stored impacts cover the whole block/group, so they dominate the brute
                    // force over [target, upto].
                    for w in &want {
                        assert!(
                            got.iter().any(|g| g.freq >= w.freq && g.norm <= w.norm),
                            "level {l} target {target} missing {w:?} in {got:?}"
                        );
                    }
                    lo = 0;
                }
                let d = c.advance(target);
                if d == NO_MORE_DOCS {
                    break;
                }
                target = d + rng.gen_range(1..5000);
            }
            if t.len() >= 128 * 32 {
                assert_eq!(seen_levels, 2);
            }
        }
    }

    #[test]
    fn shallow_then_advance_is_consistent() {
        let mut rng = StdRng::seed_from_u64(77);
        let max_doc = 1_000_000;
        let terms = vec![gen_term(&mut rng, 50_000, max_doc, true)];
        let (data, states) =
            write_terms(IndexOptions::DocsAndFreqsAndPositions, false, &[], &terms);
        let t = &terms[0];
        let mut c = PostingsCursor::new(&data, &states[0], PostingsFlags::POSITIONS);
        let mut target = 0;
        loop {
            c.advance_shallow(target + 1000);
            c.advance_shallow(target + 50_000);
            let got = c.advance(target + 50_000);
            let want = t.iter().find(|p| p.doc >= target + 50_000);
            match want {
                None => {
                    assert_eq!(got, NO_MORE_DOCS);
                    break;
                }
                Some(p) => {
                    assert_eq!(got, p.doc);
                    assert_eq!(c.freq(), p.freq);
                    for (pos, ..) in &p.positions {
                        assert_eq!(c.next_position(), *pos);
                    }
                    target = got;
                }
            }
        }
    }

    #[test]
    fn next_after_shallow_does_not_skip() {
        let mut rng = StdRng::seed_from_u64(78);
        let terms = vec![gen_term(&mut rng, 30_000, 400_000, true)];
        let (data, states) = write_terms(IndexOptions::DocsAndFreqsAndPositions, false, &[], &terms);
        let t = &terms[0];
        let mut c = PostingsCursor::new(&data, &states[0], PostingsFlags::POSITIONS);
        let mut i = 0;
        while i < t.len() {
            if rng.gen_bool(0.05) {
                c.advance_shallow(t[i].doc + rng.gen_range(0..200_000));
                let _ = c.impacts(0);
            }
            let d = if rng.gen_bool(0.2) { c.advance(t[i].doc) } else { c.next() };
            assert_eq!(d, t[i].doc, "doc #{i}");
            if rng.gen_bool(0.3) {
                assert_eq!(c.freq(), t[i].freq);
                for (pos, ..) in &t[i].positions {
                    assert_eq!(c.next_position(), *pos);
                }
            }
            i += 1;
        }
        assert_eq!(c.next(), NO_MORE_DOCS);
    }

    #[test]
    fn dense_block_bits_are_the_block_docs() {
        let mut rng = StdRng::seed_from_u64(57);
        let docs: Vec<u32> = (0..60_000u32).filter(|_| rng.gen_bool(0.4)).collect();
        let t: Vec<Posting> = docs.iter().map(|&doc| Posting { doc, positions: Vec::new(), freq: 1 }).collect();
        let (data, states) = write_terms(IndexOptions::DocsAndFreqs, false, &[], &[t]);
        let mut c = PostingsCursor::new(&data, &states[0], PostingsFlags::DOCS);
        assert!(c.dense_block().is_none());
        let mut seen = 0;
        let mut target = 0u32;
        while c.advance(target) != NO_MORE_DOCS {
            let d = c.doc();
            if let Some((start, words, last)) = c.dense_block() {
                seen += 1;
                let from_bits: Vec<u32> = (0..words.len() as u32 * 64)
                    .filter(|&i| words[(i >> 6) as usize] >> (i & 63) & 1 != 0)
                    .map(|i| start + i)
                    .collect();
                assert_eq!(from_bits.len(), 128);
                assert_eq!(*from_bits.last().unwrap(), last);
                let lo = docs.binary_search(&from_bits[0]).unwrap();
                assert_eq!(from_bits, docs[lo..lo + 128]);
                assert!(from_bits.contains(&d));
            }
            target = d + rng.gen_range(1..300);
        }
        assert!(seen > 10);
        assert!(c.dense_block().is_none());
    }

    #[test]
    fn into_bitset_matches_iteration() {
        let mut rng = StdRng::seed_from_u64(31);
        let max_doc = 100_000u32;
        let terms: Vec<Vec<Posting>> = [1usize, 50, 128, 300, 20_000].iter().map(|&df| gen_term(&mut rng, df, max_doc, false)).collect();
        let (data, states) = write_terms(IndexOptions::DocsAndFreqs, false, &[], &terms);
        for (t, st) in terms.iter().zip(&states) {
            let mut c = PostingsCursor::new(&data, st, PostingsFlags::DOCS);
            let start = c.advance(1000);
            let mut bits = vec![0u64; (max_doc as usize).div_ceil(64)];
            c.into_bitset(60_000, &mut bits, 0);
            let want: Vec<u32> = t.iter().map(|p| p.doc).filter(|&d| d >= start && d < 60_000).collect();
            let got: Vec<u32> = (0..max_doc).filter(|&d| bits[(d >> 6) as usize] & (1 << (d & 63)) != 0).collect();
            assert_eq!(got, want);
            let next = t.iter().map(|p| p.doc).find(|&d| d >= 60_000).unwrap_or(NO_MORE_DOCS);
            assert_eq!(c.doc(), next);
            if next != NO_MORE_DOCS {
                let after = t.iter().map(|p| p.doc).find(|&d| d > next).unwrap_or(NO_MORE_DOCS);
                assert_eq!(c.next(), after);
            }
        }
    }

    /// Dense blocks (consecutive runs and bit sets, Lucene 10.1 style) mixed with FOR blocks:
    /// iteration, advance, positions and `into_bitset` agree with the model, and dense terms
    /// get smaller.
    #[test]
    fn dense_blocks_round_trip() {
        let mut rng = StdRng::seed_from_u64(41);
        let max_doc = 200_000u32;
        let mut shapes: Vec<Vec<u32>> = vec![
            (0..5000).collect(),                                // consecutive from doc 0
            (7..20_000).step_by(2).collect(),                   // every 2nd doc
            (100..50_000).step_by(3).collect(),                 // every 3rd doc
            (0..max_doc).filter(|_| rng.gen_bool(0.5)).collect(), // random half
        ];
        // Alternating dense runs and sparse stretches.
        let mut mixed = Vec::new();
        let mut d = 3u32;
        while d < max_doc - 1000 {
            if rng.gen_bool(0.5) {
                mixed.extend(d..d + 300);
                d += 300;
            } else {
                d += rng.gen_range(1..400);
                mixed.push(d);
                d += 1;
            }
        }
        shapes.push(mixed);
        for with_pos in [false, true] {
            let opts = if with_pos { IndexOptions::DocsAndFreqsAndPositions } else { IndexOptions::DocsAndFreqs };
            let terms: Vec<Vec<Posting>> = shapes
                .iter()
                .map(|docs| {
                    docs.iter()
                        .map(|&doc| {
                            let freq = rng.gen_range(1..4);
                            let positions = if with_pos { (0..freq).map(|i| (i * 3, 0, 0, Vec::new())).collect() } else { Vec::new() };
                            Posting { doc, positions, freq }
                        })
                        .collect()
                })
                .collect();
            let (data, states) = write_terms(opts, false, &[], &terms);
            // Runs of consecutive docs, as far as the block knows.
            for (t, st) in terms.iter().zip(&states) {
                let docs: Vec<u32> = t.iter().map(|p| p.doc).collect();
                let mut c = PostingsCursor::new(&data, st, PostingsFlags::DOCS);
                let mut i = 0;
                while c.next() != NO_MORE_DOCS {
                    let end = c.doc_id_run_end();
                    let mut j = i;
                    while j + 1 < docs.len() && docs[j + 1] == docs[j] + 1 {
                        j += 1;
                    }
                    assert!(end > c.doc() && end <= docs[j] + 1, "doc {} end {end} true end {}", c.doc(), docs[j] + 1);
                    if rng.gen_bool(0.01) {
                        for d in c.doc()..end {
                            assert!(docs.binary_search(&d).is_ok());
                        }
                    }
                    i += 1;
                }
            }
            for (t, st) in terms.iter().zip(&states) {
                for flags in [PostingsFlags::DOCS, PostingsFlags::FREQS, PostingsFlags::ALL] {
                    check_iteration(&data, st, t, flags);
                    check_advance(&mut rng, &data, st, t, flags);
                }
                for start in [0u32, 1, 4999, 77_777] {
                    for upto in [start + 1, start + 64, start + 129, start + 5000, max_doc] {
                        let mut c = PostingsCursor::new(&data, st, PostingsFlags::DOCS);
                        let first = c.advance(start);
                        if first == NO_MORE_DOCS {
                            continue;
                        }
                        let mut bits = vec![0u64; (max_doc as usize).div_ceil(64) + 1];
                        c.into_bitset(upto, &mut bits, start & !63);
                        let want: Vec<u32> = t.iter().map(|p| p.doc).filter(|&d| d >= first && d < upto).collect();
                        let got: Vec<u32> = (0..max_doc)
                            .filter(|&d| d >= start & !63)
                            .filter(|&d| bits[((d - (start & !63)) >> 6) as usize] >> ((d - (start & !63)) & 63) & 1 != 0)
                            .collect();
                        assert_eq!(got, want, "start {start} upto {upto}");
                        assert_eq!(c.doc(), t.iter().map(|p| p.doc).find(|&d| d >= upto).unwrap_or(NO_MORE_DOCS));
                    }
                }
            }
        }
        // Dense terms are stored compactly: 128 consecutive docs cost no doc bytes.
        let consecutive: Vec<Posting> = (0..128 * 64).map(|doc| Posting { doc, positions: Vec::new(), freq: 1 }).collect();
        let (data, _) = write_terms(IndexOptions::Docs, false, &[], &[consecutive]);
        assert!(data.docs.len() < 64 * 12, "{} bytes", data.docs.len());
    }

    #[test]
    fn empty_cursor() {
        let mut c = PostingsCursor::empty();
        assert_eq!(c.doc(), NO_MORE_DOCS);
        assert_eq!(c.next(), NO_MORE_DOCS);
        assert_eq!(c.advance(5), NO_MORE_DOCS);
    }

    #[test]
    #[ignore]
    fn bench_postings_decode() {
        let mut rng = StdRng::seed_from_u64(1);
        let max_doc = 10_000_000u32;
        let terms = vec![gen_term(&mut rng, 2_000_000, max_doc, false)];
        let (data, states) = write_terms(IndexOptions::DocsAndFreqs, false, &[], &terms);
        println!(
            "bytes per posting: {:.3}",
            data.docs.len() as f64 / 2_000_000.0
        );
        let t = std::time::Instant::now();
        let mut n = 0u64;
        for _ in 0..20 {
            let mut c = PostingsCursor::new(&data, &states[0], PostingsFlags::DOCS);
            while c.next() != NO_MORE_DOCS {
                n += 1;
            }
        }
        let e = t.elapsed();
        println!("next(): {:.0} M docs/s", n as f64 / e.as_secs_f64() / 1e6);
        let t = std::time::Instant::now();
        let mut n = 0u64;
        for _ in 0..20 {
            let mut c = PostingsCursor::new(&data, &states[0], PostingsFlags::FREQS);
            while c.next() != NO_MORE_DOCS {
                n += c.freq() as u64;
            }
        }
        let e = t.elapsed();
        println!(
            "next()+freq(): {:.0} M docs/s ({n})",
            40_000_000.0 / e.as_secs_f64() / 1e6
        );
        let t = std::time::Instant::now();
        let mut n = 0u64;
        for _ in 0..20 {
            let mut c = PostingsCursor::new(&data, &states[0], PostingsFlags::DOCS);
            let mut d = 0;
            while d != NO_MORE_DOCS {
                d = c.advance(d.saturating_add(1000));
                n += 1;
            }
        }
        let e = t.elapsed();
        println!(
            "advance(+1000): {:.0} ns/op",
            e.as_nanos() as f64 / n as f64
        );
        // Positions: every doc, every position (phrase-query style).
        let terms = vec![gen_term(&mut rng, 200_000, 1_000_000, true)];
        let (data, states) = write_terms(IndexOptions::DocsAndFreqsAndPositions, false, &[], &terms);
        let total: u64 = terms[0].iter().map(|p| p.freq as u64).sum();
        let t = std::time::Instant::now();
        let mut acc = 0u64;
        for _ in 0..10 {
            let mut c = PostingsCursor::new(&data, &states[0], PostingsFlags::POSITIONS);
            while c.next() != NO_MORE_DOCS {
                for _ in 0..c.freq() {
                    acc += c.next_position() as u64;
                }
            }
        }
        let e = t.elapsed();
        println!(
            "next()+freq()+all positions: {:.1} ns/position ({acc})",
            e.as_nanos() as f64 / (10 * total) as f64
        );
        // Sparse position reads: advance, then read only the first position.
        let t = std::time::Instant::now();
        let mut n = 0u64;
        for _ in 0..10 {
            let mut c = PostingsCursor::new(&data, &states[0], PostingsFlags::POSITIONS);
            let mut d = 0;
            while d != NO_MORE_DOCS {
                d = c.advance(d.saturating_add(50));
                if d != NO_MORE_DOCS {
                    acc += c.next_position() as u64;
                    n += 1;
                }
            }
        }
        println!("advance(+50)+first position: {:.0} ns/op ({acc})", t.elapsed().as_nanos() as f64 / n as f64);
        // Dense terms (half and a fifth of all docs), FOR-only blocks vs bit set blocks.
        let max_doc = 4_000_000u32;
        let dense = [
            ("50%", gen_term(&mut rng, 2_000_000, max_doc, false)),
            ("20%", gen_term(&mut rng, 800_000, max_doc, false)),
        ];
        for force in [true, false] {
            FORCE_FOR.store(force, std::sync::atomic::Ordering::Relaxed);
            for (name, t) in &dense {
                let (data, states) = write_terms(IndexOptions::DocsAndFreqs, false, &[], std::slice::from_ref(t));
                let st = &states[0];
                let label = if force { "FOR" } else { "dense" };
                // Best of 5 (the machine is shared).
                let best = |f: &mut dyn FnMut() -> u64| {
                    (0..5)
                        .map(|_| {
                            let t0 = std::time::Instant::now();
                            let n = f();
                            (t0.elapsed().as_secs_f64(), n)
                        })
                        .min_by(|a, b| a.0.total_cmp(&b.0))
                        .unwrap()
                };
                let (secs, n) = best(&mut || {
                    let mut n = 0u64;
                    let mut c = PostingsCursor::new(&data, st, PostingsFlags::DOCS);
                    while c.next() != NO_MORE_DOCS {
                        n += 1;
                    }
                    n
                });
                let next = n as f64 / secs / 1e6;
                let (secs, k) = best(&mut || {
                    let mut k = 0u64;
                    let mut c = PostingsCursor::new(&data, st, PostingsFlags::DOCS);
                    let mut d = 0;
                    while d != NO_MORE_DOCS {
                        d = c.advance(d.saturating_add(17));
                        k += 1;
                    }
                    k
                });
                let adv = secs * 1e9 / k as f64;
                let mut bits = vec![0u64; (max_doc as usize).div_ceil(64)];
                let (secs, _) = best(&mut || {
                    let mut c = PostingsCursor::new(&data, st, PostingsFlags::DOCS);
                    c.next();
                    c.into_bitset(NO_MORE_DOCS, &mut bits, 0);
                    0
                });
                let ib = t.len() as f64 / secs / 1e6;
                println!(
                    "{label} {name}: {:.3} bytes/doc, next {next:.0} M docs/s, advance(+17) {adv:.1} ns, into_bitset {ib:.0} M docs/s",
                    data.docs.len() as f64 / t.len() as f64
                );
            }
        }
        FORCE_FOR.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}
