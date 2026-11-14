//! Range deletions: TRUNCATE and DROP TABLE delete a relation's whole key range at once
//! instead of a tombstone per row and index entry.
//!
//! A transaction deletes a range by writing a marker, `PREFIX | start` = `end`, with its
//! other writes, so the deletion commits (and is replayed after a crash) atomically with them.
//! The store keeps the committed markers as `RangeDel`s in its view: a version of a key in
//! [start, end) written before the marker's commit ts is gone for a reader at that ts or
//! later (older snapshots still read it). Compaction drops what they cover once no snapshot
//! can see it, whole files without reading them (`Store::collect_ranges`), and a marker
//! nothing older is left under is deleted (retired).

use super::sst::{SstMeta, SstReader, key_ts, user_key};
use super::{Cursor, Frozen, Merge, Store, View, write_manifest};
use crate::error::Result;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Where markers are kept (a system prefix of their own: 0x20 two-phase state, 0x22 size
/// estimates, 0x23 / 0x24 models kept across a close, `exec::sizing::kept`).
pub const PREFIX: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 0x2a];

/// A `RangeDel::ts` that hides every version: the ranges a transaction deleted itself, which
/// its reads of the store leave out (`Store::scan_excluding`).
pub const ALL: u64 = u64::MAX;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeDel {
    pub start: Vec<u8>,
    pub end: Vec<u8>,
    /// The commit ts of the deletion (in a transaction's spilled writes, its journal place).
    pub ts: u64,
}

impl RangeDel {
    pub fn contains(&self, uk: &[u8]) -> bool {
        self.start.as_slice() <= uk && uk < self.end.as_slice()
    }

    /// Whether it overlaps [start, end) (an empty `end`: unbounded).
    pub fn overlaps(&self, start: &[u8], end: &[u8]) -> bool {
        (end.is_empty() || self.start.as_slice() < end) && start < self.end.as_slice()
    }

    /// Whether it hides a version written at `vts` from a reader at `read_ts`.
    pub fn hides(&self, vts: u64, read_ts: u64) -> bool {
        vts < self.ts && (self.ts <= read_ts || self.ts == ALL)
    }
}

/// The marker key of a deletion of [start, ..).
pub fn marker_key(start: &[u8]) -> Vec<u8> {
    let mut k = PREFIX.to_vec();
    k.extend_from_slice(start);
    k
}

pub fn is_marker(key: &[u8]) -> bool {
    key.len() > PREFIX.len() && key.starts_with(PREFIX)
}

/// A store's committed range deletions (its view's), sorted by start, so a read finds the
/// ones holding a key by a binary search instead of reading the whole list: every row
/// fetch asks, and replication deletes a range per batch of confirmed records (agent 158),
/// so the list can be long until compaction retires its markers. Reads as a slice.
#[derive(Clone, Debug, Default)]
pub struct RangeDels {
    list: Vec<RangeDel>,
    /// `reach[i]`: the largest `end` among `list[..=i]` (a search stops at the first entry
    /// whose prefix ends at or before the key).
    reach: Vec<Vec<u8>>,
}

impl RangeDels {
    pub fn new(mut list: Vec<RangeDel>) -> RangeDels {
        list.sort_by(|a, b| a.start.cmp(&b.start).then(a.ts.cmp(&b.ts)));
        let mut reach: Vec<Vec<u8>> = Vec::with_capacity(list.len());
        for r in &list {
            let e = match reach.last() {
                Some(m) if *m >= r.end => m.clone(),
                _ => r.end.clone(),
            };
            reach.push(e);
        }
        RangeDels { list, reach }
    }

    /// These and `more`.
    pub fn with(&self, more: impl Iterator<Item = RangeDel>) -> RangeDels {
        RangeDels::new(self.list.iter().cloned().chain(more).collect())
    }

    /// Whether some deletion's range holds `uk`.
    pub fn holds(&self, uk: &[u8]) -> bool {
        self.holding(uk).next().is_some()
    }

    /// The deletions whose range holds `uk`, last first.
    fn holding<'a>(&'a self, uk: &'a [u8]) -> impl Iterator<Item = &'a RangeDel> + 'a {
        let n = self.list.partition_point(|r| r.start.as_slice() <= uk);
        (0..n)
            .rev()
            .take_while(move |&i| self.reach[i].as_slice() > uk)
            .map(move |i| &self.list[i])
            .filter(move |r| r.contains(uk))
    }

    /// The newest deletion that hides version `vts` of `uk` from a reader at `read_ts`.
    pub fn covering<'a>(&'a self, uk: &'a [u8], vts: u64, read_ts: u64) -> Option<&'a RangeDel> {
        self.holding(uk).filter(|r| r.hides(vts, read_ts)).max_by_key(|r| r.ts)
    }

    /// The deletions that can hide something in [start, end) from a reader at `ts` (an
    /// empty `end`: unbounded): of those starting before `end`, the ones reaching past
    /// `start`.
    pub fn relevant(&self, start: &[u8], end: &[u8], ts: u64) -> Vec<RangeDel> {
        if self.list.is_empty() {
            return Vec::new();
        }
        let n = match end.is_empty() {
            true => self.list.len(),
            false => self.list.partition_point(|r| r.start.as_slice() < end),
        };
        let mut out: Vec<RangeDel> = (0..n)
            .rev()
            .take_while(|&i| self.reach[i].as_slice() > start)
            .map(|i| &self.list[i])
            .filter(|r| (r.ts <= ts || r.ts == ALL) && r.overlaps(start, end))
            .cloned()
            .collect();
        out.reverse();
        out
    }
}

impl std::ops::Deref for RangeDels {
    type Target = [RangeDel];
    fn deref(&self) -> &[RangeDel] {
        &self.list
    }
}

/// `list` after a commit at `ts` of writes including these markers (value Some(end): a
/// deletion; None: a retired marker, which takes every deletion of that start with it).
/// None: no marker among them.
pub fn noted<'a>(
    list: &[RangeDel],
    markers: impl Iterator<Item = (&'a [u8], Option<&'a [u8]>)>,
    ts: u64,
) -> Option<RangeDels> {
    let mut out: Option<Vec<RangeDel>> = None;
    for (k, v) in markers {
        if !is_marker(k) {
            continue;
        }
        let l = out.get_or_insert_with(|| list.to_vec());
        let start = &k[PREFIX.len()..];
        match v {
            Some(end) => l.push(RangeDel {
                start: start.to_vec(),
                end: end.to_vec(),
                ts,
            }),
            None => l.retain(|r| r.start.as_slice() != start),
        }
    }
    out.map(RangeDels::new)
}

/// Whether a file's user keys all lie in `r`'s range, every version older than its ts.
fn covers_file(r: &RangeDel, m: &SstMeta) -> bool {
    r.start.as_slice() <= user_key(&m.first_key) && user_key(&m.last_key) < r.end.as_slice() && m.max_ts < r.ts
}

/// Whether a file may hold a version `r` hides.
fn touches_file(r: &RangeDel, m: &SstMeta) -> bool {
    user_key(&m.first_key) < r.end.as_slice() && r.start.as_slice() <= user_key(&m.last_key) && m.min_ts < r.ts
}

/// Whether `f` holds a version one of `live` hides (read in their ranges, up to the first).
fn holds_covered(f: &Arc<SstReader>, live: &[RangeDel]) -> Result<bool> {
    for r in live.iter().filter(|r| touches_file(r, &f.meta)) {
        let mut merge = Merge::new(vec![Cursor::level(vec![f.clone()], &r.start, &r.end)], r.end.clone());
        while let Some(i) = merge.min()? {
            let (k, _) = merge.sources[i].head().expect("a head entry");
            if key_ts(k) < r.ts {
                return Ok(true);
            }
            merge.sources[i].advance();
        }
    }
    Ok(false)
}

impl Store {
    /// The range deletions' share of the background work (after compaction): files whose
    /// every entry a deletion no snapshot is older than covers leave the manifest unread;
    /// L1 files it covers in part are written again without those entries; then a marker
    /// with nothing older left under it is retired (deleted), so the list stays short.
    pub(super) fn collect_ranges(self: &Arc<Self>) -> Result<()> {
        let live = {
            let _g = self.bg.lock();
            if self.closed.load(Ordering::Acquire) {
                return Ok(());
            }
            let view = self.view();
            // (Durable ones only: a file goes on the strength of the marker.)
            let horizon = self.gc_horizon().min(self.visible_ts());
            let live: Vec<RangeDel> = view.rdels.iter().filter(|r| r.ts <= horizon).cloned().collect();
            // (Discarded ranges hide every version: `discard`. Only whole files go for them.)
            let discarded: Vec<RangeDel> =
                self.discard.ranges().into_iter().map(|(start, end)| RangeDel { start, end, ts: ALL }).collect();
            if live.is_empty() && discarded.is_empty() {
                return Ok(());
            }
            let whole = |m: &SstMeta| live.iter().chain(&discarded).any(|r| covers_file(r, m));
            let gone: HashSet<u64> = view
                .l0
                .iter()
                .chain(view.l1.iter())
                .filter(|s| whole(&s.meta))
                .map(|s| s.meta.id)
                .collect();
            let mut replaced: HashMap<u64, Vec<Arc<SstReader>>> = HashMap::new();
            for f in &view.l1 {
                if !gone.contains(&f.meta.id) && holds_covered(f, &live)? {
                    replaced.insert(f.meta.id, self.rewrite_without(f, &live, horizon)?);
                }
            }
            if !gone.is_empty() || !replaced.is_empty() {
                let mut l1: Vec<Arc<SstReader>> = Vec::new();
                for f in &view.l1 {
                    match replaced.get(&f.meta.id) {
                        Some(out) => l1.extend(out.iter().cloned()),
                        None if !gone.contains(&f.meta.id) => l1.push(f.clone()),
                        None => {}
                    }
                }
                let mut m = self.manifest.lock();
                m.l0.retain(|x| !gone.contains(&x.id));
                // (Only compaction and this change L1, under `bg`: the view's is the manifest's.)
                m.l1 = l1.iter().map(|s| s.meta.clone()).collect();
                m.next_id = self.next_id.load(Ordering::Relaxed);
                write_manifest(&self.dir, &m)?;
                let mut v = self.view.write();
                *v = Arc::new(View {
                    mem: v.mem.clone(),
                    imm: v.imm.clone(),
                    l0: v.l0.iter().filter(|s| !gone.contains(&s.meta.id)).cloned().collect(),
                    l1,
                    rdels: v.rdels.clone(),
                });
                drop(v);
                drop(m);
                for s in view.l0.iter().chain(view.l1.iter()) {
                    if gone.contains(&s.meta.id) || replaced.contains_key(&s.meta.id) {
                        s.mark_obsolete();
                    }
                }
                tracing::debug!(dir = %self.dir.display(), dropped = gone.len(), rewritten = replaced.len(), "collected deleted ranges");
            }
            live
        };
        // Markers with nothing older left under them in any file (the memtables hold only
        // commits past `flushed_ts`) are retired.
        let flushed = self.manifest.lock().flushed_ts;
        let view = self.view();
        for r in live.iter().filter(|r| r.ts <= flushed) {
            let mut files = view
                .imm
                .iter()
                .filter_map(|f| match f {
                    Frozen::Sst(s) => Some(s),
                    Frozen::Mem(_) => None,
                })
                .chain(view.l0.iter())
                .chain(view.l1.iter());
            let mut held = false;
            for s in &mut files {
                if holds_covered(s, std::slice::from_ref(r))? {
                    held = true;
                    break;
                }
            }
            if held {
                continue;
            }
            let key = marker_key(&r.start);
            let at = r.ts;
            // (Unless a newer deletion of the range committed meanwhile: then it stays.)
            let _ = self.commit(vec![(key.clone(), None)], |s| {
                if s.latest_version_ts(&key)? == Some(at) {
                    Ok(())
                } else {
                    Err(crate::error::internal("range deletion renewed"))
                }
            });
        }
        Ok(())
    }

    /// `f` written again without the entries `live` covers (none left: no file).
    pub(super) fn rewrite_without(&self, f: &Arc<SstReader>, live: &[RangeDel], horizon: u64) -> Result<Vec<Arc<SstReader>>> {
        let live = RangeDels::new(live.to_vec());
        let mut merge = Merge::new(vec![Cursor::level_uncached(vec![f.clone()])], Vec::new());
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut w = self.sst_writer(id, true)?;
        while let Some(i) = merge.min()? {
            let (k, v) = merge.sources[i].head().expect("a head entry");
            if live.covering(user_key(k), key_ts(k), horizon).is_none()
                && let Err(e) = w.add(k, v)
            {
                w.abandon();
                return Err(e);
            }
            merge.sources[i].advance();
        }
        if w.entries() == 0 {
            w.abandon();
            return Ok(Vec::new());
        }
        let meta = w.finish()?;
        Ok(vec![Arc::new(SstReader::open(&self.dir, meta, self.opts.cache.clone())?)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sorted list answers as a search over every deletion does.
    #[test]
    fn sorted_lookups_match_a_linear_search() {
        let mut rng = 0x1234_5678_u64;
        let mut next = |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        for round in 0..200 {
            let list: Vec<RangeDel> = (0..next(40))
                .map(|_| {
                    let a = next(100) as u8;
                    let b = a + 1 + next(if round % 2 == 0 { 3 } else { 60 }) as u8;
                    let ts = if next(10) == 0 { ALL } else { next(50) };
                    RangeDel { start: vec![a], end: vec![b], ts }
                })
                .collect();
            let sorted = RangeDels::new(list.clone());
            let order = |a: &RangeDel, b: &RangeDel| (&a.start, a.ts, &a.end).cmp(&(&b.start, b.ts, &b.end));
            for _ in 0..100 {
                let k = [next(170) as u8];
                let (vts, rts) = (next(50), next(60));
                let want = list.iter().filter(|r| r.contains(&k) && r.hides(vts, rts)).map(|r| r.ts).max();
                assert_eq!(sorted.covering(&k, vts, rts).map(|r| r.ts), want);
                assert_eq!(sorted.holds(&k), list.iter().any(|r| r.contains(&k)));
                let s = [next(170) as u8];
                let e = if next(4) == 0 { vec![] } else { vec![next(170) as u8] };
                let mut want: Vec<_> =
                    list.iter().filter(|r| (r.ts <= rts || r.ts == ALL) && r.overlaps(&s, &e)).cloned().collect();
                let mut got = sorted.relevant(&s, &e, rts);
                want.sort_by(order);
                got.sort_by(order);
                assert_eq!(got, want);
            }
        }
    }
}
