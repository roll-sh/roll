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
/// the block's last key is in it, so a seek there needs nothing but the block.
pub(super) struct Held {
    block: Arc<super::sst::Block>,
    from: Vec<u8>,
    /// Where the last read in it stopped (the next of ascending reads starts there).
    pos: BlockPos,
}

/// Single-key reads at one snapshot over the sources as they were when it was made (see the
/// module's comment). Made after the snapshot's commits are visible (`Store::point_reader`).
pub struct PointReader {
    view: Arc<View>,
    ts: u64,
    /// Check the last level's bloom filters too (keys that may not exist; keys an index
    /// named are in it when no newer source has them, so its filters would only cost).
    bloom_last: bool,
    /// Per file source, by its place: the frozen sources, L0's files, then L1.
    held: Vec<Option<Held>>,
    pos: BlockPos,
    seek: Vec<u8>,
}

impl PointReader {
    pub(super) fn new(view: Arc<View>, ts: u64, bloom_last: bool) -> PointReader {
        let n = view.imm.len() + view.l0.len() + 1;
        PointReader { view, ts, bloom_last, held: (0..n).map(|_| None).collect(), pos: BlockPos::default(), seek: Vec::new() }
    }

    pub fn ts(&self) -> u64 {
        self.ts
    }

    pub(super) fn view(&self) -> &Arc<View> {
        &self.view
    }

    /// The newest version of `key` visible at the reader's snapshot, range deletions applied;
    /// values as stored (out-of-line ones not put back).
    pub fn get_raw(&mut self, key: &[u8]) -> Result<Option<Version>> {
        self.seek.clear();
        self.seek.extend_from_slice(key);
        self.seek.extend_from_slice(&(!self.ts).to_be_bytes());
        let hit = newest(&self.view, key, &self.seek, self.bloom_last, &mut self.held, &mut self.pos)?;
        Ok(deleted_by_range(&self.view, key, hit, self.ts))
    }

    /// The first row visible at the reader's snapshot with a user key in [start, end) (an
    /// empty `end`: unbounded), values as stored: an equality lookup's one row, read with no
    /// merging scan (no cursor, source list or allocation per lookup). Ok(None): there is
    /// none. Err-free `Unknown` (`First::Unknown`): more than a few keys in the range had no
    /// visible version (deleted ones): a scan is cheaper.
    pub fn first_raw(&mut self, start: &[u8], end: &[u8]) -> Result<First> {
        let mut from = std::mem::take(&mut self.seek);
        from.clear();
        from.extend_from_slice(start);
        let mut cand = Vec::new();
        let mut tries = 0;
        let out = loop {
            if !smallest_key(&self.view, &from, &mut self.held, &mut self.pos, &mut cand)? {
                break First::None;
            }
            if !end.is_empty() && cand.as_slice() >= end {
                break First::None;
            }
            if cand.as_slice() < start {
                // (A key `start` extends: its versions sort after `start`. No key starts with
                // it, so none lies between `start` and past its versions.)
                from.clear();
                from.extend_from_slice(&cand);
                from.extend_from_slice(&[0xFF; 9]);
                continue;
            }
            from.clear();
            from.extend_from_slice(&cand);
            from.extend_from_slice(&(!self.ts).to_be_bytes());
            let hit = newest(&self.view, &cand, &from, true, &mut self.held, &mut self.pos)?;
            if let Some((_, Some(v))) = deleted_by_range(&self.view, &cand, hit, self.ts) {
                break First::Row(cand, v);
            }
            tries += 1;
            if tries == FIRST_TRIES {
                break First::Unknown;
            }
            // (Past every version of this key: no other user key starts with it.)
            from.truncate(cand.len());
            from.extend_from_slice(&[0xFF; 9]);
        };
        self.seek = from;
        Ok(out)
    }
}

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
fn smallest_key(view: &View, from: &[u8], held: &mut [Option<Held>], pos: &mut BlockPos, out: &mut Vec<u8>) -> Result<bool> {
    let mut found = false;
    let mut take = |k: &[u8]| {
        let k = user_key(k);
        if !found || k < out.as_slice() {
            out.clear();
            out.extend_from_slice(k);
            found = true;
        }
    };
    for m in view.memtables() {
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
            file_first(ssts, held.get_mut(slot), from, pos, &mut take)?;
        }
    }
    Ok(found)
}

/// The first storage key at or after `from` in non-overlapping files in key order, to `f`
/// (not called: none), through the block `held` keeps when `from` falls in it.
fn file_first(ssts: &[Arc<SstReader>], mut held: Option<&mut Option<Held>>, from: &[u8], pos: &mut BlockPos, f: &mut dyn FnMut(&[u8])) -> Result<()> {
    if let Some(Some(h)) = held.as_deref_mut()
        && h.from.as_slice() <= from
        && h.block.last_key().is_some_and(|l| from <= l)
    {
        // (From where the last read in the block stopped.)
        h.block.seek_on(from, &mut h.pos);
        if h.pos.index() < h.block.len() {
            f(h.pos.key());
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
            f(pos.key());
            if b == first
                && let Some(slot) = held
            {
                *slot = Some(Held { block, from: from.to_vec(), pos: pos.clone() });
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
    held: &mut [Option<Held>],
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

fn mem_get(m: &Memtable, key: &[u8], seek: &[u8]) -> Option<Version> {
    if m.is_empty() {
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
    mut held: Option<&mut Option<Held>>,
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
    if let Some(Some(h)) = held.as_deref_mut()
        && h.from.as_slice() <= seek
        && h.block.last_key().is_some_and(|l| seek <= l)
    {
        // (From where the last read in the block stopped.)
        h.block.seek_on(seek, &mut h.pos);
        if h.pos.index() < h.block.len() {
            let k = h.pos.key();
            return Ok((user_key(k) == key).then(|| (key_ts(k), h.block.value(&h.pos))));
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
            match slot {
                Some(h) => {
                    h.block = block;
                    h.from.clear();
                    h.from.extend_from_slice(seek);
                    h.pos.clone_from(pos);
                }
                None => *slot = Some(Held { block, from: seek.to_vec(), pos: pos.clone() }),
            }
        }
        return Ok(hit);
    }
    Ok(None)
}
