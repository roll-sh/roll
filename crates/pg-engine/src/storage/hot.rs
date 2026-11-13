//! Hot relation ranges: small key ranges read whole again and again while their keys are
//! rewritten all the time (a sequential scan of a small table whose rows every transaction
//! updates: pgbench's branches and tellers, counters, job queues).
//!
//! In the LSM each version of such a key is an entry of its own, spread over the memtables
//! and files, so a scan merges every source and seeks past each key's older versions. A
//! range scanned whole often enough is kept here as well: each key's versions as a chain,
//! newest first, trimmed on every commit to what a snapshot at or after the store's gc
//! horizon can read (PostgreSQL prunes a HOT chain on its page the same way). A scan of the
//! range reads the chains instead of merging the sources. The LSM stays the store of record
//! (it gets every write as before; the versions a trim drops leave its memtables too); this
//! is a second copy of a few small ranges, rebuilt from the LSM when dropped.
//!
//! A range is one relation's: the keys under an 8-byte prefix (`keys::index_prefix`, a
//! table's rows or one index's entries). Every write goes through the commit lock, where
//! `apply` adds it to the chains of a kept range before the commit is visible; a commit
//! written as an SST of its own (`Store::ingest_locked`) drops every range instead.

use super::sst::{key_ts, user_key};
use crate::error::Result;
use bytes::Bytes;
use parking_lot::{Mutex, RwLock};
use std::collections::BTreeMap;
// (foldhash, not SipHash: `note_scan` hashes a prefix on every scan of a relation range not
// kept, an index lookup's included.)
use hashbrown::HashMap;
use std::ops::Bound;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// A relation's key prefix: table and index (`keys::index_prefix`).
pub(super) const PREFIX: usize = 8;
pub(super) type Prefix = [u8; PREFIX];

/// Scans in a range before it is kept.
const BUILD_AFTER: i32 = 8;
/// A range refused (too big) is tried again after this many more scans, doubling with each
/// refusal (up to 256 times as many).
const RETRY_AFTER: i32 = 4096;
/// Most keys / bytes a kept range may hold (more: dropped, and tried again later).
const MAX_KEYS: usize = 2048;
const MAX_BYTES: usize = 1 << 20;
/// Most deleted keys a build reads before it refuses the range.
const MAX_DEAD: usize = 4 * MAX_KEYS;
/// Most ranges kept per store.
const MAX_RANGES: usize = 64;
/// Versions written to a kept range with no scan of it since: it is dropped.
const IDLE_WRITES: u32 = 8192;

/// A kept range: each key's versions (commit ts, value; None a deletion), oldest first: a
/// write appends (no shift of the chain under the commit lock), a read looks from the end.
#[derive(Default)]
struct Chains {
    keys: BTreeMap<Bytes, Vec<(u64, Option<Bytes>)>>,
    bytes: usize,
    /// Versions written since the last scan.
    idle: u32,
}

impl Chains {
    /// Add a version of `key` at `ts` (newer than every version it has) and trim the chain
    /// to what snapshots at or after `horizon` read: the versions after it and the newest
    /// one at or before it. `dropped(key, ts)` for each version trimmed off.
    fn push(
        &mut self,
        key: &[u8],
        ts: u64,
        v: Option<Bytes>,
        horizon: u64,
        dropped: &mut impl FnMut(&[u8], u64),
    ) {
        self.bytes += v.as_ref().map_or(0, Bytes::len) + 16;
        let chain = match self.keys.get_mut(key) {
            Some(c) => c,
            None => {
                self.bytes += key.len() + 32;
                self.keys.entry(Bytes::copy_from_slice(key)).or_default()
            }
        };
        // (A commit writing a key twice: its last write is the version, as in the memtable.)
        match chain.last_mut() {
            Some(last) if last.0 == ts => {
                self.bytes -= last.1.as_ref().map_or(0, Bytes::len) + 16;
                last.1 = v;
            }
            _ => chain.push((ts, v)),
        }
        // (The newest version at or before the horizon, and those after it, are kept.)
        if let Some(keep) = chain.iter().rposition(|(t, _)| *t <= horizon)
            && keep > 0
        {
            for (t, v) in chain.drain(..keep) {
                self.bytes -= v.as_ref().map_or(0, Bytes::len) + 16;
                dropped(key, t);
            }
        }
        // A deletion every snapshot sees: the key is gone for all of them.
        if let [(t, None)] = chain.as_slice()
            && *t <= horizon
        {
            self.keys.remove(key);
            self.bytes -= key.len() + 32 + 16;
        }
    }

    /// The rows visible at `ts` among keys in [start, end) (the keys shared, not copied).
    fn visible(&self, start: &[u8], end: &[u8], ts: u64) -> Vec<(Bytes, Bytes)> {
        let range = (Bound::Included(start), Bound::Excluded(end));
        self.keys
            .range::<[u8], _>(range)
            .filter_map(|(k, chain)| {
                let (_, v) = newest(chain, ts)?;
                Some((k.clone(), v.clone()?))
            })
            .collect()
    }

    fn too_big(&self) -> bool {
        self.keys.len() > MAX_KEYS || self.bytes > MAX_BYTES
    }
}

/// A chain's newest version at or before `ts`.
#[inline]
fn newest(chain: &[(u64, Option<Bytes>)], ts: u64) -> Option<&(u64, Option<Bytes>)> {
    chain.iter().rev().find(|(t, _)| *t <= ts)
}

#[derive(Default)]
pub(super) struct Hot {
    /// Any range kept (commits skip `apply`'s lookups otherwise).
    pub(super) any: AtomicBool,
    /// The kept ranges' prefixes as bits (`bit`), written under `ranges`' write lock after
    /// every change: a read of a prefix whose bit is clear answers from the store without
    /// `ranges`' lock, which every worker's point read took (a shared line's
    /// compare-exchange). A reader seeing a bit late only reads the store, which holds every
    /// version a kept range does for any live snapshot.
    mask: PrefixBits,
    ranges: RwLock<HashMap<Prefix, Arc<Mutex<Chains>>>>,
    /// Scans in ranges not kept (negative: refused, counting up to a retry), and how often
    /// each was refused.
    seen: Mutex<HashMap<Prefix, (i32, u32)>>,
}

const MASK_WORDS: usize = 4;

/// A prefix's word and bit in a `PrefixBits`.
#[inline]
fn bit(p: &Prefix) -> (usize, u64) {
    let h = (u64::from_ne_bytes(*p).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as usize;
    (h / 64 % MASK_WORDS, 1 << (h % 64))
}

/// Relation prefixes as bits, read without a lock: a prefix whose bit is clear is not in the
/// set (a set bit: it may be; 256 bits, so a few prefixes rarely share one).
#[derive(Default)]
pub(super) struct PrefixBits([AtomicU64; MASK_WORDS]);

impl PrefixBits {
    /// Whether the relation of key `k` (its first `PREFIX` bytes) may be in the set: true for
    /// a key shorter than a prefix.
    #[inline]
    pub(super) fn may_hold(&self, k: &[u8]) -> bool {
        let Some(p) = k.get(..PREFIX).and_then(|p| Prefix::try_from(p).ok()) else { return true };
        let (w, b) = bit(&p);
        self.0[w].load(Ordering::Acquire) & b != 0
    }

    /// Add the relation of key `k` (none for a key shorter than a prefix).
    #[inline]
    pub(super) fn add(&self, k: &[u8]) {
        if let Some(p) = k.get(..PREFIX).and_then(|p| Prefix::try_from(p).ok()) {
            let (w, b) = bit(&p);
            if self.0[w].load(Ordering::Relaxed) & b == 0 {
                self.0[w].fetch_or(b, Ordering::Release);
            }
        }
    }

    /// The set made `prefixes`, whatever it held.
    fn set<'a>(&self, prefixes: impl Iterator<Item = &'a Prefix>) {
        let mut m = [0u64; MASK_WORDS];
        for p in prefixes {
            let (w, b) = bit(p);
            m[w] |= b;
        }
        for (a, v) in self.0.iter().zip(m) {
            a.store(v, Ordering::Release);
        }
    }
}

/// The prefix of a scan of [start, end) that reads one whole relation's range.
fn whole(start: &[u8], end: &[u8]) -> Option<Prefix> {
    let p: Prefix = start.try_into().ok()?;
    let last = p.iter().rposition(|b| *b != 0xFF)?;
    (end.len() == last + 1 && end[..last] == p[..last] && end[last] == p[last] + 1).then_some(p)
}

/// The relation range [start, end) lies in, if it lies in one.
pub(super) fn covering(start: &[u8], end: &[u8]) -> Option<Prefix> {
    let p: Prefix = start.get(..PREFIX)?.try_into().ok()?;
    let inside = end.get(..PREFIX) == Some(&p[..]) || whole(&p, end).is_some();
    inside.then_some(p)
}

impl Hot {
    /// Whether `p`'s range may be kept (its bit set: `mask`).
    #[inline]
    fn may_keep(&self, p: &Prefix) -> bool {
        self.mask.may_hold(p)
    }

    /// `mask` made again from `ranges` (under its write lock, after a change).
    fn publish(&self, ranges: &HashMap<Prefix, Arc<Mutex<Chains>>>) {
        self.mask.set(ranges.keys());
    }

    /// The rows of [start, end) visible at `ts`, when a kept range holds them.
    pub(super) fn scan(&self, start: &[u8], end: &[u8], ts: u64) -> Option<Vec<(Bytes, Bytes)>> {
        if !self.any.load(Ordering::Acquire) {
            return None;
        }
        let p = covering(start, end)?;
        if !self.may_keep(&p) {
            return None;
        }
        let range = self.ranges.read().get(&p)?.clone();
        let mut c = range.lock();
        c.idle = 0;
        Some(c.visible(start, end, ts))
    }

    /// Count a scan in `p`'s range, which isn't kept (a sequential scan of the relation, an
    /// index scan of a few of its keys): true when it is time to keep it.
    pub(super) fn note_scan(&self, p: Prefix) -> bool {
        let mut seen = self.seen.lock();
        if seen.len() > 64 * MAX_RANGES {
            seen.clear();
        }
        let (n, _) = seen.entry(p).or_insert((0, 0));
        *n += 1;
        if *n >= BUILD_AFTER {
            *n = 0;
            return true;
        }
        false
    }

    /// `get_version` of `key` at `ts` when a kept range holds a version of it (Some: the
    /// answer); None: the store answers (a key the range holds no version of at `ts` may
    /// be a deletion every snapshot sees that the chain dropped).
    pub(super) fn get(&self, key: &[u8], ts: u64) -> Option<Option<(u64, Option<Bytes>)>> {
        if !self.any.load(Ordering::Acquire) {
            return None;
        }
        let p: Prefix = key.get(..PREFIX)?.try_into().ok()?;
        if !self.may_keep(&p) {
            return None;
        }
        let ranges = self.ranges.read();
        let mut c = ranges.get(&p)?.lock();
        c.idle = 0;
        c.keys.get(key).and_then(|chain| newest(chain, ts)).map(|v| Some(v.clone()))
    }

    /// Keep `p`'s range, built from `entries`: every stored version in it, in storage order
    /// (by key, newest first). Called under the commit lock, so no commit lands meanwhile.
    pub(super) fn build(
        &self,
        p: Prefix,
        entries: &mut dyn Iterator<Item = Result<(Vec<u8>, Option<Bytes>)>>,
        horizon: u64,
    ) -> Result<()> {
        let mut c = Chains::default();
        // (Entries come newest first per key: a key's are collected, then pushed oldest
        // first.)
        let flush = |c: &mut Chains, key: &[u8], versions: &mut Vec<(u64, Option<Bytes>)>| {
            for (t, v) in versions.drain(..).rev() {
                c.push(key, t, v, horizon, &mut |_, _| {});
            }
        };
        let mut key: Vec<u8> = Vec::new();
        let mut versions: Vec<(u64, Option<Bytes>)> = Vec::new();
        let mut held = 0usize;
        // (Deleted keys read: a queue's head of 100,000 deleted keys holds no chain, and was
        // read whole under the commit lock to find the range too big.)
        let mut dead = 0usize;
        for e in entries {
            let (k, v) = e?;
            let uk = user_key(&k);
            if uk != key.as_slice() {
                // (A key's newest version first.)
                if versions.first().is_some_and(|v| v.1.is_none()) {
                    dead += 1;
                }
                flush(&mut c, &key, &mut versions);
                held = c.bytes;
                if c.too_big() || dead > MAX_DEAD {
                    self.refuse(p);
                    return Ok(());
                }
                key.clear();
                key.extend_from_slice(uk);
            }
            // (A value copied out of its file block, which it would keep in memory.)
            let v = v.map(|v| Bytes::copy_from_slice(&v));
            held += v.as_ref().map_or(0, Bytes::len) + 16;
            if held > 4 * MAX_BYTES || versions.len() >= MAX_KEYS {
                // (A key with versions piled up behind an old snapshot, or big values.)
                self.refuse(p);
                return Ok(());
            }
            versions.push((key_ts(&k), v));
        }
        flush(&mut c, &key, &mut versions);
        if c.too_big() {
            self.refuse(p);
            return Ok(());
        }
        let mut ranges = self.ranges.write();
        if ranges.len() >= MAX_RANGES {
            // The one written to longest without a scan goes.
            let idlest = ranges.iter().max_by_key(|(_, r)| r.lock().idle).map(|(p, _)| *p);
            if let Some(q) = idlest {
                ranges.remove(&q);
            }
        }
        ranges.insert(p, Arc::new(Mutex::new(c)));
        self.publish(&ranges);
        self.any.store(true, Ordering::Release);
        Ok(())
    }

    fn refuse(&self, p: Prefix) {
        let mut seen = self.seen.lock();
        let (n, times) = seen.entry(p).or_insert((0, 0));
        *n = BUILD_AFTER - (RETRY_AFTER << (*times).min(8));
        *times += 1;
    }

    /// A commit's writes at `ts` (under the commit lock, before it is visible): added to the
    /// kept ranges they fall in, whose chains are trimmed to `horizon()`; `dropped(key, ts)`
    /// for each version trimmed off. Ranges grown too big or written long without a scan go.
    pub(super) fn apply(
        &self,
        ts: u64,
        writes: &[(Vec<u8>, Option<Bytes>)],
        horizon: impl FnOnce() -> u64,
        mut dropped: impl FnMut(&[u8], u64),
    ) {
        if !self.any.load(Ordering::Acquire) {
            return;
        }
        let mut horizon = Some(horizon);
        let mut h = 0;
        let mut gone = Vec::new();
        {
            let ranges = self.ranges.read();
            let done = |cur: Option<(Prefix, Option<parking_lot::MutexGuard<'_, Chains>>)>, gone: &mut Vec<Prefix>| {
                if let Some((q, Some(c))) = cur
                    && (c.too_big() || c.idle > IDLE_WRITES)
                {
                    gone.push(q);
                }
            };
            let mut cur: Option<(Prefix, Option<parking_lot::MutexGuard<'_, Chains>>)> = None;
            for (k, v) in writes {
                let Some(p) = k.get(..PREFIX).and_then(|p| Prefix::try_from(p).ok()) else {
                    continue;
                };
                if cur.as_ref().is_none_or(|(q, _)| *q != p) {
                    done(cur.take(), &mut gone);
                    cur = Some((p, ranges.get(&p).map(|r| r.lock())));
                }
                let Some((_, Some(c))) = &mut cur else { continue };
                if let Some(f) = horizon.take() {
                    h = f();
                }
                c.idle += 1;
                c.push(k, ts, v.clone(), h, &mut dropped);
            }
            done(cur, &mut gone);
        }
        if !gone.is_empty() {
            let mut ranges = self.ranges.write();
            for p in gone {
                ranges.remove(&p);
                self.refuse(p);
            }
            self.publish(&ranges);
            self.any.store(!ranges.is_empty(), Ordering::Release);
        }
    }

    /// Drop every kept range (a commit that bypasses `apply`).
    pub(super) fn clear(&self) {
        if self.any.swap(false, Ordering::AcqRel) {
            let mut ranges = self.ranges.write();
            ranges.clear();
            self.publish(&ranges);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_and_covering_ranges() {
        let p = [0, 0, 0, 7, 0, 0, 0, 0];
        assert_eq!(whole(&p, &[0, 0, 0, 7, 0, 0, 0, 1]), Some(p));
        assert_eq!(whole(&p, &[0, 0, 0, 8]), None);
        let q = [0, 0, 0, 7, 0, 0, 0, 0xFF];
        assert_eq!(whole(&q, &[0, 0, 0, 7, 0, 0, 1]), Some(q));
        assert_eq!(whole(&q, &[0, 0, 0, 8]), None);
        assert_eq!(whole(&[0xFF; 8], &[0xFF; 9]), None);
        assert_eq!(covering(&[0, 0, 0, 7, 0, 0, 0, 0, 5], &[0, 0, 0, 7, 0, 0, 0, 0, 9]), Some(p));
        assert_eq!(covering(&[0, 0, 0, 7, 0, 0, 0, 0, 5], &[0, 0, 0, 7, 0, 0, 0, 1]), Some(p));
        assert_eq!(covering(&[0, 0, 0, 7, 0, 0, 0, 0, 5], &[0, 0, 0, 7, 0, 0, 0, 2]), None);
        assert_eq!(covering(&[0, 0, 0, 7], &[0, 0, 0, 8]), None);
    }
}
