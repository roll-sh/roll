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
use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
/// Most ranges kept per store.
const MAX_RANGES: usize = 64;
/// Versions written to a kept range with no scan of it since: it is dropped.
const IDLE_WRITES: u32 = 8192;

/// A kept range: each key's versions (commit ts, value; None a deletion), newest first.
#[derive(Default)]
struct Chains {
    keys: BTreeMap<Vec<u8>, Vec<(u64, Option<Bytes>)>>,
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
                self.keys.entry(key.to_vec()).or_default()
            }
        };
        // (A commit writing a key twice: its last write is the version, as in the memtable.)
        match chain.first_mut() {
            Some(first) if first.0 == ts => {
                self.bytes -= first.1.as_ref().map_or(0, Bytes::len) + 16;
                first.1 = v;
            }
            _ => chain.insert(0, (ts, v)),
        }
        if let Some(keep) = chain.iter().position(|(t, _)| *t <= horizon) {
            for (t, v) in chain.drain(keep + 1..) {
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

    /// The rows visible at `ts` among keys in [start, end).
    fn visible(&self, start: &[u8], end: &[u8], ts: u64) -> Vec<(Vec<u8>, Bytes)> {
        let range = (Bound::Included(start), Bound::Excluded(end));
        self.keys
            .range::<[u8], _>(range)
            .filter_map(|(k, chain)| {
                let (_, v) = chain.iter().find(|(t, _)| *t <= ts)?;
                Some((k.clone(), v.clone()?))
            })
            .collect()
    }

    fn too_big(&self) -> bool {
        self.keys.len() > MAX_KEYS || self.bytes > MAX_BYTES
    }
}

#[derive(Default)]
pub(super) struct Hot {
    /// Any range kept (commits skip `apply`'s lookups otherwise).
    pub(super) any: AtomicBool,
    ranges: RwLock<HashMap<Prefix, Arc<Mutex<Chains>>>>,
    /// Scans in ranges not kept (negative: refused, counting up to a retry), and how often
    /// each was refused.
    seen: Mutex<HashMap<Prefix, (i32, u32)>>,
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
    /// The rows of [start, end) visible at `ts`, when a kept range holds them.
    pub(super) fn scan(&self, start: &[u8], end: &[u8], ts: u64) -> Option<Vec<(Vec<u8>, Bytes)>> {
        if !self.any.load(Ordering::Acquire) {
            return None;
        }
        let p = covering(start, end)?;
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

    /// `get_version` of each of `keys` (ascending) at `ts` that a kept range holds: Some(the
    /// answer) for those, None for the others.
    #[allow(clippy::type_complexity)]
    pub(super) fn get_many(&self, keys: &[&[u8]], ts: u64) -> Option<Vec<Option<Option<(u64, Option<Bytes>)>>>> {
        if !self.any.load(Ordering::Acquire) {
            return None;
        }
        let ranges = self.ranges.read();
        let mut out = Vec::with_capacity(keys.len());
        let mut cur: Option<(Prefix, Option<parking_lot::MutexGuard<'_, Chains>>)> = None;
        for k in keys {
            let Some(p) = k.get(..PREFIX).and_then(|p| Prefix::try_from(p).ok()) else {
                out.push(None);
                continue;
            };
            if cur.as_ref().is_none_or(|(q, _)| *q != p) {
                cur = Some((p, ranges.get(&p).map(|r| r.lock())));
            }
            out.push(match &mut cur {
                Some((_, Some(c))) => {
                    c.idle = 0;
                    // (A key the range holds no version of at `ts`: the store answers, with
                    // the deletion every snapshot sees that the chain dropped.)
                    c.keys.get(*k).and_then(|chain| chain.iter().find(|(t, _)| *t <= ts)).map(|v| Some(v.clone()))
                }
                _ => None,
            });
        }
        Some(out)
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
        for e in entries {
            let (k, v) = e?;
            let uk = user_key(&k);
            if uk != key.as_slice() {
                flush(&mut c, &key, &mut versions);
                held = c.bytes;
                if c.too_big() {
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
            self.any.store(!ranges.is_empty(), Ordering::Release);
        }
    }

    /// Drop every kept range (a commit that bypasses `apply`).
    pub(super) fn clear(&self) {
        if self.any.swap(false, Ordering::AcqRel) {
            self.ranges.write().clear();
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
