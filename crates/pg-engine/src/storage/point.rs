//! Point reads: the newest version of a key visible at a snapshot, read source by source
//! (memtables, then files newest first) until one holds a version.
//!
//! A `PointReader` keeps one view of the sources and, per file source, the block it read
//! last, between calls. A key that falls in a held block (ascending lookups: a nested
//! loop's inner side, an index's row fetches, a batch of them) costs a seek in that block:
//! no file search, bloom check, block index search or block cache lookup.

use super::sst::{BlockPos, SstReader, key_ts, user_key};
use super::{Frozen, Memtable, View};
use crate::error::Result;
use bytes::Bytes;
use std::ops::Bound;
use std::sync::Arc;

/// A version found: (commit ts, value; None a tombstone).
pub type Version = (u64, Option<Bytes>);

/// A block a source read last, and the seek key that found it: every key from that one to
/// the block's last key is in it, so a seek there needs nothing but the block. (No block:
/// none held; its buffers are kept for the next.)
#[derive(Default)]
pub(super) struct Held {
    block: Option<Arc<super::sst::Block>>,
    from: Vec<u8>,
    /// Where the last read in it stopped (the next of ascending reads starts there).
    pos: BlockPos,
}

impl Held {
    /// The block held, when `key` falls in it.
    fn holding(&mut self, key: &[u8]) -> Option<(&super::sst::Block, &mut BlockPos)> {
        let b = self.block.as_deref()?;
        (self.from.as_slice() <= key && b.last_key().is_some_and(|l| key <= l)).then_some((b, &mut self.pos))
    }

    /// Hold `block`, which `seek` found at `pos`.
    fn keep(&mut self, block: Arc<super::sst::Block>, seek: &[u8], pos: &BlockPos) {
        self.block = Some(block);
        self.from.clear();
        self.from.extend_from_slice(seek);
        self.pos.clone_from(pos);
    }
}

/// Single-key reads at one snapshot over the sources as they were when it was made (see the
/// module's comment). Made after the snapshot's commits are visible (`Store::point_reader`).
pub struct PointReader {
    view: Arc<View>,
    ts: u64,
    /// Check the last level's bloom filters too (keys that may not exist; keys an index
    /// named are in it when no newer source has them, so its filters would only cost).
    bloom_last: bool,
    bufs: Bufs,
}

/// A reader's buffers: a reader leaves them to its thread's next one (`SPARE`), so a point
/// read per statement (a unique key's probe and its row's fetch) makes none of them again.
#[derive(Default)]
struct Bufs {
    /// Per file source, by its place: the frozen sources, L0's files, then L1.
    held: Vec<Held>,
    pos: BlockPos,
    seek: Vec<u8>,
    /// `first_raw`'s candidate key.
    cand: Vec<u8>,
}

thread_local! {
    static SPARE: std::cell::RefCell<Bufs> = std::cell::RefCell::new(Bufs::default());
}

impl Drop for PointReader {
    fn drop(&mut self) {
        // (The blocks held are this reader's view's: they go with it.)
        let mut bufs = std::mem::take(&mut self.bufs);
        bufs.held.iter_mut().for_each(|h| h.block = None);
        // (Not as the thread ends: the buffers go then.)
        let _ = SPARE.try_with(|s| {
            if let Ok(mut s) = s.try_borrow_mut() {
                *s = bufs;
            }
        });
    }
}

impl PointReader {
    pub(super) fn new(view: Arc<View>, ts: u64, bloom_last: bool) -> PointReader {
        let mut bufs = SPARE.try_with(|s| s.try_borrow_mut().map(|mut s| std::mem::take(&mut *s)).unwrap_or_default()).unwrap_or_default();
        bufs.held.resize_with(view.imm.len() + view.l0.len() + 1, Held::default);
        PointReader { view, ts, bloom_last, bufs }
    }

    pub fn ts(&self) -> u64 {
        self.ts
    }

    /// Room for a storage key in the buffers every read writes into, made at the first
    /// read (a key grown into them entry by entry reallocated each a few times).
    fn room(&mut self) {
        let b = &mut self.bufs;
        if b.seek.capacity() == 0 {
            b.seek.reserve(KEY_ROOM);
            b.cand.reserve(KEY_ROOM);
            b.pos.reserve_key(KEY_ROOM);
        }
    }

    pub(super) fn view(&self) -> &Arc<View> {
        &self.view
    }


    /// The newest version of `key` visible at the reader's snapshot, range deletions applied;
    /// values as stored (out-of-line ones not put back).
    pub fn get_raw(&mut self, key: &[u8]) -> Result<Option<Version>> {
        self.room();
        let b = &mut self.bufs;
        b.seek.clear();
        b.seek.extend_from_slice(key);
        b.seek.extend_from_slice(&(!self.ts).to_be_bytes());
        let hit = newest(&self.view, key, &b.seek, self.bloom_last, &mut b.held, &mut b.pos)?;
        Ok(deleted_by_range(&self.view, key, hit, self.ts))
    }

    /// The first row visible at the reader's snapshot with a user key in [start, end) (an
    /// empty `end`: unbounded), values as stored: an equality lookup's one row, read with no
    /// merging scan (no cursor, source list or allocation per lookup). Ok(None): there is
    /// none. Err-free `Unknown` (`First::Unknown`): more than a few keys in the range had no
    /// visible version (deleted ones): a scan is cheaper.
    pub fn first_raw(&mut self, start: &[u8], end: &[u8]) -> Result<First> {
        self.room();
        if let Some(first) = self.first_in_l1(start, end)? {
            return Ok(first);
        }
        let Bufs { held, pos, seek: from, cand, .. } = &mut self.bufs;
        from.clear();
        from.extend_from_slice(start);
        let mut tries = 0;
        Ok(loop {
            if !smallest_key(&self.view, from, end, held, pos, cand)? {
                break First::None;
            }
            if !end.is_empty() && cand.as_slice() >= end {
                break First::None;
            }
            if cand.as_slice() < start {
                // (A key `start` extends: its versions sort after `start`. No key starts with
                // it, so none lies between `start` and past its versions.)
                from.clear();
                from.extend_from_slice(cand);
                from.extend_from_slice(&[0xFF; 9]);
                continue;
            }
            from.clear();
            from.extend_from_slice(cand);
            from.extend_from_slice(&(!self.ts).to_be_bytes());
            let hit = newest(&self.view, cand, from, true, held, pos)?;
            if let Some((_, Some(v))) = deleted_by_range(&self.view, cand, hit, self.ts) {
                break First::Row(cand.clone(), v);
            }
            tries += 1;
            if tries == FIRST_TRIES {
                break First::Unknown;
            }
            // (Past every version of this key: no other user key starts with it.)
            from.truncate(cand.len());
            from.extend_from_slice(&[0xFF; 9]);
        })
    }

    /// `first_raw` by one seek, when L1 is the only source that may hold a key of
    /// [start, end) (no frozen source, no L0 file, a memtable without keys of the range's
    /// relation, no range deletion): its first entry at or after `start` is the newest version
    /// of the smallest key there, the answer when the snapshot sees it and it holds a row.
    /// None: not decided by it (a newer version, a deletion: the search above). (`first_raw`
    /// sought that entry, then sought the key's newest version again through `newest`.)
    fn first_in_l1(&mut self, start: &[u8], end: &[u8]) -> Result<Option<First>> {
        let view = &*self.view;
        if !view.imm.is_empty() || !view.l0.is_empty() || view.l1.is_empty() || !view.rdels.is_empty() || (!view.mem.is_empty() && view.mem.may_hold_range(start, end)) {
            return Ok(None);
        }
        let ts = self.ts;
        let Bufs { held, pos, .. } = &mut self.bufs;
        // (No entry at or after `start`: no row, as `smallest_key` finding none. L1's slot is
        // the first.)
        let mut first = Some(First::None);
        file_first(&view.l1, held.get_mut(0), start, pos, &mut |block, pos| {
            let k = pos.key();
            let uk = user_key(k);
            first = if !end.is_empty() && uk >= end {
                Some(First::None)
            } else if uk < start || key_ts(k) > ts {
                None
            } else {
                block.value(pos).map(|v| First::Row(uk.to_vec(), v))
            };
        })?;
        Ok(first)
    }
}

impl PointReader {
    /// The rows visible at the reader's snapshot under strictly ascending `keys`, each in
    /// its key's place (None: no row; out-of-line values put back when `resolve`): `get_raw`
    /// of each key, the L1 reads made a step at a time for the whole batch. An index's row fetches are mostly in a block each, and one read's steps (the
    /// file's block index, the block cache, the block's restarts, its entries) missed the
    /// CPU's cache one after another; here every key's block is found, then the blocks are
    /// taken from the cache, then each seek's restart is found, each step asking for the
    /// cache lines the next reads, so the misses of different keys overlap. A key in the
    /// block the key before it went to, or in the block the reader holds from its last read,
    /// is sought on from there, without a search.
    pub fn get_sorted<K: AsRef<[u8]>>(&mut self, keys: &[K], resolve: bool) -> Result<Vec<Option<Bytes>>> {
        debug_assert!(keys.windows(2).all(|w| w[0].as_ref() < w[1].as_ref()), "keys strictly ascending");
        self.room();
        let ts = self.ts;
        let mut out = vec![None; keys.len()];
        let PointReader { view, bufs, .. } = self;
        let Bufs { held, pos, seek, .. } = bufs;
        let l1 = view.l1.as_slice();
        let slot = view.imm.len() + view.l0.len();
        let newer = !view.mem.is_empty() || !view.imm.is_empty() || !view.l0.is_empty();
        // Each key: its seek key (in `seek`), a newer source's version, or where in L1 it is.
        seek.clear();
        // (Its own, not in the buffers every point read moves and drops.)
        let mut sorted: Vec<Step> = Vec::with_capacity(keys.len());
        let mut f = 0;
        // (The block the last key searched for went to.)
        let mut last: Option<(usize, usize)> = None;
        for (i, k) in keys.iter().enumerate() {
            let k = k.as_ref();
            let at = seek.len();
            seek.extend_from_slice(k);
            seek.extend_from_slice(&(!ts).to_be_bytes());
            let s = at..seek.len();
            let sk = &seek[s.clone()];
            if newer && let Some(hit) = newest_above_l1(view, k, sk, held, pos)? {
                sorted.push(Step { i, seek: s, l1: L1::No, hit: Some(hit) });
                continue;
            }
            let l1_at = if let Some((lf, lb)) = last.filter(|&(lf, lb)| l1[lf].block_holds(lb, sk)) {
                L1::Block(lf, lb)
            } else if held.get_mut(slot).is_some_and(|h| h.holding(sk).is_some()) {
                L1::Held
            } else {
                while f < l1.len() && user_key(&l1[f].meta.last_key) < k {
                    f += 1;
                }
                match l1.get(f) {
                    Some(sst) if k >= user_key(&sst.meta.first_key) => {
                        let b = sst.find_block(sk);
                        if b < sst.block_count() {
                            last = Some((f, b));
                            L1::Block(f, b)
                        } else {
                            L1::No
                        }
                    }
                    _ => L1::No,
                }
            };
            sorted.push(Step { i, seek: s, l1: l1_at, hit: None });
        }
        // The blocks, from the cache (or the file), their restarts asked for.
        let mut blocks: Vec<(usize, usize, Arc<super::sst::Block>)> = Vec::new();
        for st in sorted.iter() {
            let L1::Block(f, b) = st.l1 else { continue };
            if blocks.last().is_some_and(|l| (l.0, l.1) == (f, b)) {
                continue;
            }
            let block = l1[f].block(b)?;
            block.prefetch_restarts();
            blocks.push((f, b, block));
        }
        // The restart the first seek of each block starts at, its entries asked for (the
        // next keys in the block are sought on from the one before).
        let mut plan: Vec<(usize, usize)> = Vec::with_capacity(sorted.len());
        let (mut bi, mut prev) = (0, None);
        for st in sorted.iter() {
            let L1::Block(f, b) = st.l1 else { continue };
            while (blocks[bi].0, blocks[bi].1) != (f, b) {
                bi += 1;
            }
            if prev == Some(bi) {
                plan.push((bi, usize::MAX));
                continue;
            }
            let block = &blocks[bi].2;
            let r = block.restart_for(&seek[st.seek.clone()]);
            block.prefetch_restart(r);
            plan.push((bi, r));
            prev = Some(bi);
        }
        // The reads, in key order.
        let (mut pi, mut pos_in) = (0, None);
        // (The last block read and the seek key its search found it by: the reader holds it.)
        let mut keep: Option<(usize, std::ops::Range<usize>)> = None;
        // (Rows found in the batch's blocks, their values asked for and read after every seek:
        // a row's first byte was a miss of its own, one row after another.)
        let mut found: Vec<(usize, usize, (u32, u32))> = Vec::with_capacity(sorted.len());
        for st in sorted.iter() {
            let key = keys[st.i].as_ref();
            let sk = &seek[st.seek.clone()];
            let (block, bpos) = match st.l1 {
                L1::No => {
                    if let Some((_, Some(v))) = st.hit.as_ref().and_then(|h| deleted_by_range(view, key, Some(h.clone()), ts)) {
                        out[st.i] = Some(owned(view, key, v, resolve, ts)?);
                    }
                    continue;
                }
                L1::Held => {
                    let (block, hpos) = held[slot].holding(sk).expect("a held block");
                    block.seek_on(sk, hpos);
                    (block, hpos)
                }
                L1::Block(..) => {
                    let (b, r) = plan[pi];
                    pi += 1;
                    let block = &*blocks[b].2;
                    match r {
                        usize::MAX if pos_in == Some(b) => block.seek_on(sk, pos),
                        usize::MAX => block.seek(sk, pos),
                        r => {
                            block.seek_from(r, sk, pos);
                            keep = Some((b, st.seek.clone()));
                        }
                    }
                    pos_in = Some(b);
                    (block, &mut *pos)
                }
            };
            if bpos.index() >= block.len() {
                // (Past the block's last entry, which `find_block` doesn't leave: as `file_get`
                // reads it.)
                pos_in = None;
                if let Some((_, Some(v))) = deleted_by_range(view, key, file_get(l1, false, None, key, sk, pos)?, ts) {
                    out[st.i] = Some(owned(view, key, v, resolve, ts)?);
                }
                continue;
            }
            let k = bpos.key();
            if user_key(k) != key {
                continue;
            }
            let vts = key_ts(k);
            if !view.rdels.is_empty() && view.rdels.covering(key, vts, ts).is_some() {
                continue;
            }
            if let L1::Block(..) = st.l1
                && let Some(span) = block.value_span(bpos)
            {
                found.push((st.i, plan[pi - 1].0, span));
                continue;
            }
            match block.value_ref(bpos) {
                None => {}
                Some(v) if resolve && super::toast::has_external(v) => {
                    let v = block.value(bpos).expect("a value");
                    out[st.i] = Some(super::toast::resolve(view, key, v, ts)?);
                }
                Some(_) => out[st.i] = block.value(bpos),
            }
        }
        for (i, b, span) in found {
            let block = &*blocks[b].2;
            out[i] = Some(match resolve && super::toast::has_external(block.span_ref(span)) {
                true => super::toast::resolve(view, keys[i].as_ref(), block.span(span), ts)?,
                false => block.span(span),
            });
        }
        // (Held for the reader's next reads: a nested loop's next outer row's keys are often
        // in it.)
        if let Some((b, s)) = keep
            && pos_in == Some(b)
            && let Some(h) = held.get_mut(slot)
        {
            h.keep(blocks[b].2.clone(), &seek[s], pos);
        }
        Ok(out)
    }
}

/// A key of `PointReader::get_sorted`: its place in the keys, its seek key's bytes,
/// where in L1 it is, or the version a source newer than L1 holds.
struct Step {
    i: usize,
    seek: std::ops::Range<usize>,
    l1: L1,
    hit: Option<Version>,
}

/// Where in L1 a key of `PointReader::get_sorted` is: in no file, in the block the
/// reader holds, or in a file's block.
enum L1 {
    No,
    Held,
    Block(usize, usize),
}

/// A row read owned under `key`, its out-of-line values put back when `resolve`.
fn owned(view: &View, key: &[u8], v: Bytes, resolve: bool, ts: u64) -> Result<Bytes> {
    if resolve && super::toast::has_external(&v) { super::toast::resolve(view, key, v, ts) } else { Ok(v) }
}

/// The bytes a point read's key buffers are made with: a table's or an index's storage key
/// and its version fit (longer ones grow them).
const KEY_ROOM: usize = 64;

/// `PointReader::first_raw`'s answer.
#[derive(Debug, PartialEq)]
pub enum First {
    /// The row (its user key, its value as stored).
    Row(Vec<u8>, Bytes),
    None,
    /// Not found in a few tries: scan the range.
    Unknown,
}

/// Keys without a visible version `first_raw` steps over before it gives up.
const FIRST_TRIES: u32 = 4;

/// The smallest user key at or after `from` that any source of `view` holds a version of,
/// into `out` (false: none).
fn smallest_key(view: &View, from: &[u8], end: &[u8], held: &mut [Held], pos: &mut BlockPos, out: &mut Vec<u8>) -> Result<bool> {
    let mut found = false;
    let mut take = |k: &[u8]| {
        let k = user_key(k);
        if !found || k < out.as_slice() {
            out.clear();
            out.extend_from_slice(k);
            found = true;
        }
    };
    // (A key past `end` is no answer: a memtable without keys of the range's relation isn't
    // read.)
    for m in view.memtables().filter(|m| m.may_hold_range(from, end)) {
        if let Some(e) = m.map.range::<[u8], _>((Bound::Included(from), Bound::Unbounded)).next() {
            take(e.key());
        }
    }
    // (Slots as `newest` numbers them: every frozen source has one, a memtable's unused.)
    let files = view
        .imm
        .iter()
        .map(|f| match f {
            Frozen::Sst(s) => Some(std::slice::from_ref(s)),
            Frozen::Mem(_) => None,
        })
        .chain(view.l0.iter().map(|s| Some(std::slice::from_ref(s))))
        .chain(std::iter::once(Some(view.l1.as_slice())));
    for (slot, ssts) in files.enumerate() {
        if let Some(ssts) = ssts.filter(|s| !s.is_empty()) {
            file_first(ssts, held.get_mut(slot), from, pos, &mut |_, p| take(p.key()))?;
        }
    }
    Ok(found)
}

/// The first entry at or after `from` in non-overlapping files in key order, to `f` (its
/// block and position; not called: none), through the block `held` keeps when `from` falls in
/// it.
fn file_first(
    ssts: &[Arc<SstReader>],
    mut held: Option<&mut Held>,
    from: &[u8],
    pos: &mut BlockPos,
    f: &mut dyn FnMut(&super::sst::Block, &BlockPos),
) -> Result<()> {
    if let Some((block, hpos)) = held.as_deref_mut().and_then(|h| h.holding(from)) {
        // (From where the last read in the block stopped.)
        block.seek_on(from, hpos);
        if hpos.index() < block.len() {
            f(block, hpos);
        }
        return Ok(());
    }
    let first_file = ssts.partition_point(|s| s.meta.last_key.as_slice() < from);
    for sst in &ssts[first_file.min(ssts.len())..] {
        let first = sst.find_block(from);
        for b in first..sst.block_count() {
            let block = sst.block(b)?;
            block.seek(from, pos);
            if pos.index() >= block.len() {
                continue;
            }
            f(&block, pos);
            if b == first
                && let Some(slot) = held
            {
                slot.keep(block, from, pos);
            }
            return Ok(());
        }
    }
    Ok(())
}

/// `hit` with the range deletions of `view` applied: a version one hides reads as the
/// deletion's tombstone.
pub(super) fn deleted_by_range(view: &View, key: &[u8], hit: Option<Version>, ts: u64) -> Option<Version> {
    match hit {
        Some((vts, _)) if !view.rdels.is_empty() => match view.rdels.covering(key, vts, ts) {
            Some(r) => Some((r.ts, None)),
            None => hit,
        },
        _ => hit,
    }
}

/// The newest version of `key` at or before the version `seek` names (`key | !ts`), before
/// range deletions. `held` keeps each file source's last block (an empty slice: none kept).
pub(super) fn newest(
    view: &View,
    key: &[u8],
    seek: &[u8],
    bloom_last: bool,
    held: &mut [Held],
    pos: &mut BlockPos,
) -> Result<Option<Version>> {
    if let Some(hit) = mem_get(&view.mem, key, seek) {
        return Ok(Some(hit));
    }
    let mut slot = 0;
    for f in &view.imm {
        let hit = match f {
            Frozen::Mem(m) => mem_get(m, key, seek),
            Frozen::Sst(s) => file_get(std::slice::from_ref(s), true, held.get_mut(slot), key, seek, pos)?,
        };
        if hit.is_some() {
            return Ok(hit);
        }
        slot += 1;
    }
    for s in &view.l0 {
        if let Some(hit) = file_get(std::slice::from_ref(s), true, held.get_mut(slot), key, seek, pos)? {
            return Ok(Some(hit));
        }
        slot += 1;
    }
    if view.l1.is_empty() {
        return Ok(None);
    }
    file_get(&view.l1, bloom_last, held.get_mut(slot), key, seek, pos)
}

/// `newest`'s version from the sources newer than L1 (None: none of them has one).
fn newest_above_l1(view: &View, key: &[u8], seek: &[u8], held: &mut [Held], pos: &mut BlockPos) -> Result<Option<Version>> {
    if let Some(hit) = mem_get(&view.mem, key, seek) {
        return Ok(Some(hit));
    }
    let mut slot = 0;
    for f in &view.imm {
        let hit = match f {
            Frozen::Mem(m) => mem_get(m, key, seek),
            Frozen::Sst(s) => file_get(std::slice::from_ref(s), true, held.get_mut(slot), key, seek, pos)?,
        };
        if hit.is_some() {
            return Ok(hit);
        }
        slot += 1;
    }
    for s in &view.l0 {
        if let Some(hit) = file_get(std::slice::from_ref(s), true, held.get_mut(slot), key, seek, pos)? {
            return Ok(Some(hit));
        }
        slot += 1;
    }
    Ok(None)
}

fn mem_get(m: &Memtable, key: &[u8], seek: &[u8]) -> Option<Version> {
    if m.is_empty() || !m.may_hold(key) {
        return None;
    }
    m.map
        .range::<[u8], _>((Bound::Included(seek), Bound::Unbounded))
        .next()
        .filter(|e| user_key(e.key()) == key)
        .map(|e| (key_ts(e.key()), e.value().clone()))
}

/// The version `seek` finds of `key` in non-overlapping files in key order (one file, or
/// L1), through the block `held` keeps when the key falls in it.
fn file_get(
    ssts: &[Arc<SstReader>],
    bloom: bool,
    mut held: Option<&mut Held>,
    key: &[u8],
    seek: &[u8],
    pos: &mut BlockPos,
) -> Result<Option<Version>> {
    let found = |block: &super::sst::Block, pos: &mut BlockPos| {
        block.seek(seek, pos);
        if pos.index() >= block.len() {
            return None;
        }
        let k = pos.key();
        Some((user_key(k) == key).then(|| (key_ts(k), block.value(pos))))
    };
    if let Some((block, hpos)) = held.as_deref_mut().and_then(|h| h.holding(seek)) {
        // (From where the last read in the block stopped.)
        block.seek_on(seek, hpos);
        if hpos.index() < block.len() {
            let k = hpos.key();
            return Ok((user_key(k) == key).then(|| (key_ts(k), block.value(hpos))));
        }
    }
    let f = match ssts.len() {
        1 => 0,
        _ => ssts.partition_point(|s| user_key(&s.meta.last_key) < key),
    };
    let Some(sst) = ssts.get(f) else { return Ok(None) };
    if key < user_key(&sst.meta.first_key) || key > user_key(&sst.meta.last_key) || bloom && !sst.may_contain(key) {
        return Ok(None);
    }
    let first = sst.find_block(seek);
    for b in first..sst.block_count() {
        let block = sst.block(b)?;
        let Some(hit) = found(&block, pos) else { continue };
        // (Kept only as `find_block` found it: then `seek` is a lower bound of its keys'.)
        if b == first
            && let Some(slot) = held.as_deref_mut()
        {
            slot.keep(block, seek, pos);
        }
        return Ok(hit);
    }
    Ok(None)
}
