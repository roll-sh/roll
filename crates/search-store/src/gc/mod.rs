//! Garbage collection of segments, live docs and WAL objects.
//!
//! An object is live while any retained manifest of any cluster references it: segments and
//! live-docs generations by key, WAL files through the `(node, from..=to)` ranges in manifests.
//! Everything else under `search/seg/` and `search/wal/` is garbage, but deletion is two-phase:
//! an object must be found unreferenced by passes at least `grace` apart (first sightings are
//! remembered in `search/gc/pending.json`). That covers references that appear while a pass runs
//! or that aren't written yet — a segment uploaded moments before the manifest naming it, a
//! branch being made — as long as `grace` exceeds the longest such window (an hour is plenty).
//!
//! Every retained manifest keeps its segments and live docs (a reader may still hold it). WAL
//! ranges are needed by the latest manifest of each shard (crash recovery) and by older ones only
//! for point-in-time branches inside `manifest_retain_ms`: an older manifest's range counts while
//! its successor is younger than the window, and only up to the WAL file the successor was
//! written at (anything later is covered by the successor itself).
//!
//! A latest commit point's range that is still open-ended reaches only to the end of the run (the
//! process) that wrote it: when a node restarts it writes a run marker
//! (`search/wal/<node>/<first file>.run`), and a range opened by an earlier run ends before the
//! next run's first file. Without that, a shard left attached by a crash would keep every WAL
//! object its node ever writes afterwards until the shard is opened again. Markers no open range
//! needs any more (and never the newest of a node) are deleted.
//!
//! Deleted shards: inside the point-in-time window `Store::delete_shard` writes a tombstone
//! generation instead of deleting the shard's commit points, so a branch to a moment before the
//! deletion still finds them. Once the tombstone is older than the window, the collector
//! deletes the shard's manifests (the tombstone last); their objects go with later passes.
//!
//! Candidates are listed *before* manifests are read, so anything a manifest written during the
//! pass refers to is either already marked live or too new to be a candidate. Manifests are
//! immutable (a key is never rewritten), so their marks are cached between passes and a pass
//! only fetches manifests it hasn't seen; the cache is also persisted ([`snapshot`]), so a
//! restarted collector doesn't read the whole bucket's manifests again.
//!
//! Run it from one place per bucket.

use crate::keys::{self, ClusterObject, LiveGen, SegObject, SegmentId};
use crate::manifest::{ShardManifest, WalHead, successor_cap};
use crate::wal::WalSource;
use crate::{Store, now_ms};
use bytes::Bytes;
use futures::StreamExt;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

mod snapshot;

/// The snapshot of marks is rewritten once this share (1/n) of its entries changed, or once it is
/// this old and anything changed.
const SNAPSHOT_CHURN: usize = 16;
const SNAPSHOT_EVERY_MS: u64 = 3_600_000;

/// What a garbage collection pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct GcStats {
    pub manifests: usize,
    /// Manifests read from object storage this pass (the others were known already).
    pub manifests_read: usize,
    /// Segment, live-docs and WAL objects listed.
    pub candidates: usize,
    /// Of those, found unreferenced this pass (deleted once they stay so for the grace period).
    pub unreferenced: usize,
    pub deleted: usize,
    pub deleted_bytes: u64,
    /// Manifests of deleted shards removed once their tombstone left the point-in-time window.
    pub manifests_deleted: usize,
    /// WAL run markers no open range needed any more.
    pub run_markers_deleted: usize,
}

/// The collector's memory between passes (one per [`Store`]).
#[derive(Default)]
pub(crate) struct Memo {
    /// Marks of every manifest key seen by the last pass.
    marks: HashMap<String, Arc<Marks>>,
    /// The persisted snapshot was read (or found missing) by this process.
    loaded: bool,
    /// Entries added or dropped since the snapshot was last written.
    dirty: usize,
    /// When this process last wrote the snapshot (0: never).
    written_ms: u64,
}

/// `(cluster, index uuid, shard)` of a manifest key.
type ShardOf<'a> = (&'a str, &'a str, u32);

/// One generation of a shard's history as the collector sees it.
struct Gen<'a> {
    version: keys::Version,
    deleted: bool,
    key: &'a str,
    marks: &'a Marks,
}

/// What one manifest references. Kept compact (ids, not key strings): the cache holds one per
/// retained manifest in the bucket.
pub(crate) struct Marks {
    segments: Box<[(SegmentId, Option<LiveGen>)]>,
    wal: Box<[WalSource]>,
    head: Option<WalHead>,
    written_ms: u64,
}

impl Marks {
    fn of(m: ShardManifest) -> Self {
        Marks {
            segments: m.segments.iter().map(|s| (s.id, s.live)).collect(),
            wal: m.wal.into_boxed_slice(),
            head: m.wal_head,
            written_ms: m.written_ms,
        }
    }
}

/// Sort inclusive ranges and merge overlapping or adjacent ones.
fn merge_ranges(mut r: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    r.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(r.len());
    for (from, to) in r {
        match out.last_mut() {
            Some(last) if from <= last.1.saturating_add(1) => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    out
}

/// Whether `seq` is in one of the merged ranges.
fn in_ranges(r: &[(u64, u64)], seq: u64) -> bool {
    let i = r.partition_point(|&(from, _)| from <= seq);
    i > 0 && seq <= r[i - 1].1
}

impl Store {
    /// One garbage collection pass (see the module docs).
    pub async fn collect_garbage(&self, grace: Duration) -> anyhow::Result<GcStats> {
        let objects = self.objects();
        let now = now_ms();
        let window = self.config().manifest_retain_ms;
        let wal_objects = self.wal_objects();
        let mut candidates = objects.list(keys::SEG_PREFIX).await?;
        candidates.extend(wal_objects.list(keys::WAL_PREFIX).await?);
        // First files of every node's runs, sorted.
        let mut runs: HashMap<String, Vec<u64>> = HashMap::new();
        for (k, _) in &candidates {
            if let Some((node, first)) = keys::parse_run_key(k) {
                runs.entry(node.to_string()).or_default().push(first);
            }
        }
        for r in runs.values_mut() {
            r.sort_unstable();
        }
        // Only keys this store names are ever deleted.
        candidates.retain(|(k, _)| keys::parse_seg_key(k).is_some() || keys::parse_wal_key(k).is_some());
        let mut pending: HashMap<String, u64> = match objects.get(keys::GC_STATE).await? {
            Some(raw) => serde_json::from_slice(&raw).unwrap_or_default(),
            None => HashMap::new(),
        };

        let manifest_keys: Vec<String> = objects
            .list(keys::CLUSTER_PREFIX)
            .await?
            .into_iter()
            .map(|(k, _)| k)
            .filter(|k| matches!(keys::parse_cluster_key(k), Some(ClusterObject::Manifest { .. })))
            .collect();
        self.load_marks_snapshot().await;
        let missing: Vec<String> = {
            let memo = self.inner.gc.lock();
            manifest_keys.iter().filter(|k| !memo.marks.contains_key(*k)).cloned().collect()
        };
        let fetched = futures::stream::iter(missing.into_iter().map(|key| async move {
            let raw = objects.get(&key).await?;
            let marks = match raw {
                Some(raw) => Some(Arc::new(Marks::of(ShardManifest::from_json(&raw).map_err(|e| anyhow::anyhow!("{key}: {e}"))?))),
                // Deleted since listing (retention, a deleted cluster).
                None => None,
            };
            anyhow::Ok((key, marks))
        }))
        .buffer_unordered(16)
        .collect::<Vec<_>>()
        .await;
        let mut st = GcStats::default();
        let mut keep_segments: HashSet<SegmentId> = HashSet::new();
        let mut keep_live: HashSet<(SegmentId, LiveGen)> = HashSet::new();
        let mut ranges: HashMap<String, Vec<(u64, u64)>> = HashMap::new();
        // Where retained manifests' open-ended ranges start, per node (which run markers matter).
        let mut open_from: HashMap<String, Vec<u64>> = HashMap::new();
        let mut expired_shards: Vec<Vec<String>> = Vec::new();
        {
            let mut guard = self.inner.gc.lock();
            let memo = &mut *guard;
            for r in fetched {
                // An unreadable manifest might reference anything: don't delete on a guess.
                let (key, marks) = r?;
                st.manifests_read += 1;
                if let Some(m) = marks {
                    memo.marks.insert(key, m);
                    memo.dirty += 1;
                }
            }
            let listed: HashSet<&String> = manifest_keys.iter().collect();
            let before = memo.marks.len();
            memo.marks.retain(|k, _| listed.contains(k));
            memo.dirty += before - memo.marks.len();
            let cache = &memo.marks;
            // Per shard, oldest version first: an older manifest's open-ended WAL range is
            // capped by its successor, so retained history doesn't pin a node's WAL forever.
            let mut shards: HashMap<ShardOf<'_>, Vec<Gen<'_>>> = HashMap::new();
            for (key, m) in cache.iter() {
                if let Some(ClusterObject::Manifest { cluster, index_uuid, shard, version, deleted }) = keys::parse_cluster_key(key) {
                    shards.entry((cluster, index_uuid, shard)).or_default().push(Gen { version, deleted, key, marks: m });
                }
            }
            for gens in shards.values_mut() {
                gens.sort_unstable_by_key(|g| (g.version, g.deleted));
                // A deleted shard whose tombstone left the window: nothing may branch to before
                // the deletion any more, so its history goes (the tombstone last) and marks
                // nothing.
                if let Some(last) = gens.last()
                    && last.deleted
                    && last.marks.written_ms.saturating_add(window) <= now
                {
                    expired_shards.push(gens.iter().map(|g| g.key.to_string()).collect());
                    continue;
                }
                for (i, g) in gens.iter().enumerate() {
                    let m = g.marks;
                    st.manifests += 1;
                    for &(id, live) in m.segments.iter() {
                        keep_segments.insert(id);
                        if let Some(g) = live {
                            keep_live.insert((id, g));
                        }
                    }
                    let next = gens.get(i + 1).map(|g| g.marks);
                    // An older commit point's WAL only serves point-in-time branches to moments
                    // before its successor; outside the window nobody may ask for those.
                    // (`<=`: with no window, never; the branch side refuses the same moments.)
                    if next.is_some_and(|n| n.written_ms.saturating_add(window) <= now) {
                        continue;
                    }
                    for s in m.wal.iter() {
                        let to = match s.to {
                            Some(to) => to,
                            None => {
                                open_from.entry(s.node.clone()).or_default().push(s.from);
                                // Bounded by what the successor says and by the end of the run that
                                // opened the range (the node restarted since).
                                let cap = next.and_then(|n| successor_cap(&n.wal, n.head.as_ref(), &s.node));
                                let run = runs.get(&s.node).and_then(|r| crate::wal::run_end(r, s.from));
                                cap.unwrap_or(u64::MAX).min(run.unwrap_or(u64::MAX))
                            }
                        };
                        if to >= s.from {
                            ranges.entry(s.node.clone()).or_default().push((s.from, to));
                        }
                    }
                }
            }
        }
        // Every attached shard names an open range on its node: merge them so a lookup is a
        // binary search, not a scan of every shard's range.
        for r in ranges.values_mut() {
            *r = merge_ranges(std::mem::take(r));
        }
        let live = |key: &str| match keys::parse_seg_key(key) {
            Some(SegObject::Segment(id)) => keep_segments.contains(&id),
            Some(SegObject::LiveDocs(id, g)) => keep_live.contains(&(id, g)),
            None => keys::parse_wal_key(key).is_some_and(|(node, seq)| ranges.get(node).is_some_and(|r| in_ranges(r, seq))),
        };

        st.candidates = candidates.len();
        let cutoff = now.saturating_sub(grace.as_millis() as u64);
        let mut next = HashMap::new();
        let mut doomed = Vec::new();
        for (key, size) in candidates {
            if live(&key) {
                continue;
            }
            st.unreferenced += 1;
            let since = pending.remove(&key).unwrap_or(now);
            if since <= cutoff {
                doomed.push((key, size));
            } else {
                next.insert(key, since);
            }
        }
        let results = futures::stream::iter(doomed.into_iter().map(|(key, size)| async move {
            let store = if key.starts_with(keys::WAL_PREFIX) { wal_objects } else { objects };
            store.delete(&key).await.map(|()| (key, size))
        }))
            .buffer_unordered(16)
            .collect::<Vec<_>>()
            .await;
        let mut first_err = None;
        // Deleted shards' expired histories: older generations first, so the tombstone (still
        // saying "deleted") is the last to go.
        let expired = futures::stream::iter(expired_shards.into_iter().map(|keys| async move {
            let mut n = 0;
            for k in keys {
                objects.delete(&k).await?;
                n += 1;
            }
            anyhow::Ok(n)
        }))
        .buffer_unordered(16)
        .collect::<Vec<_>>()
        .await;
        for r in expired {
            match r {
                Ok(n) => st.manifests_deleted += n,
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        // Run markers no retained open range starts under (the newest of each node stays: the
        // node may be running it).
        let mut stale_markers = Vec::new();
        for (node, starts) in &runs {
            let froms = open_from.get(node).map_or(&[][..], |v| v.as_slice());
            for (i, &first) in starts.iter().enumerate().take(starts.len().saturating_sub(1)) {
                let prev = if i == 0 { 0 } else { starts[i - 1] };
                // Marker `first` ends the ranges opened by the run before it.
                if !froms.iter().any(|&f| f >= prev && f < first) {
                    stale_markers.push(keys::run_key(node, first));
                }
            }
        }
        let marker_results =
            futures::stream::iter(stale_markers.iter().map(|k| wal_objects.delete(k))).buffer_unordered(16).collect::<Vec<_>>().await;
        for r in marker_results {
            match r {
                Ok(()) => st.run_markers_deleted += 1,
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        for r in results {
            match r {
                Ok((key, size)) => {
                    st.deleted += 1;
                    st.deleted_bytes += size;
                    if let Some(name) = key.strip_prefix(keys::SEG_PREFIX) {
                        self.inner.cache.remove(name);
                    }
                }
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        objects.put(keys::GC_STATE, Bytes::from(serde_json::to_vec(&next)?)).await?;
        if let Err(e) = self.write_marks_snapshot(now).await {
            // Only a restart's head start is at stake.
            tracing::warn!(error = %e, "writing the garbage collector's marks snapshot failed");
        }
        let c = &self.inner.counters;
        c.gc_runs.fetch_add(1, Relaxed);
        c.gc_deleted.fetch_add(st.deleted as u64, Relaxed);
        c.gc_deleted_bytes.fetch_add(st.deleted_bytes, Relaxed);
        if let Some(e) = first_err {
            return Err(e.context("deleting garbage"));
        }
        Ok(st)
    }
}

impl Store {
    /// Seed the collector's memory from the persisted snapshot, once per process.
    async fn load_marks_snapshot(&self) {
        if self.inner.gc.lock().loaded {
            return;
        }
        let loaded = match self.objects().get(keys::GC_MARKS).await {
            Ok(Some(raw)) => match tokio::task::spawn_blocking(move || snapshot::decode(&raw)).await {
                Ok(Ok(entries)) => entries,
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "ignoring the garbage collector's marks snapshot");
                    Vec::new()
                }
                Err(e) => {
                    tracing::warn!(error = %e, "decoding the garbage collector's marks snapshot failed");
                    Vec::new()
                }
            },
            Ok(None) => Vec::new(),
            // Transient: try again next pass.
            Err(e) => {
                tracing::warn!(error = %e, "reading the garbage collector's marks snapshot failed");
                return;
            }
        };
        let mut memo = self.inner.gc.lock();
        if !memo.loaded {
            memo.loaded = true;
            for (k, m) in loaded {
                memo.marks.entry(k).or_insert(m);
            }
        }
    }

    /// Persist the collector's memory when enough of it changed (see [`snapshot`]).
    async fn write_marks_snapshot(&self, now: u64) -> anyhow::Result<()> {
        let (mut entries, dirty) = {
            let memo = self.inner.gc.lock();
            let due = memo.written_ms == 0 || memo.dirty * SNAPSHOT_CHURN >= memo.marks.len() || now.saturating_sub(memo.written_ms) >= SNAPSHOT_EVERY_MS;
            if memo.dirty == 0 || !due {
                return Ok(());
            }
            (memo.marks.iter().map(|(k, m)| (k.clone(), m.clone())).collect::<Vec<_>>(), memo.dirty)
        };
        let data = tokio::task::spawn_blocking(move || {
            // Sorted keys share long prefixes: they compress to almost nothing.
            entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            snapshot::encode(&entries, 3)
        })
        .await??;
        self.objects().put(keys::GC_MARKS, Bytes::from(data)).await?;
        let mut memo = self.inner.gc.lock();
        memo.dirty = memo.dirty.saturating_sub(dirty);
        memo.written_ms = now.max(1);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_merge_and_answer_membership() {
        let r = merge_ranges(vec![(10, 12), (1, 3), (4, 5), (20, u64::MAX), (11, 15), (30, 40)]);
        assert_eq!(r, vec![(1, 5), (10, 15), (20, u64::MAX)]);
        for (seq, want) in [(0, false), (1, true), (5, true), (6, false), (9, false), (10, true), (15, true), (16, false), (20, true), (u64::MAX, true)] {
            assert_eq!(in_ranges(&r, seq), want, "{seq}");
        }
        assert!(!in_ranges(&[], 3));
        assert_eq!(merge_ranges(vec![(5, 5), (5, 5)]), vec![(5, 5)]);
    }
}
