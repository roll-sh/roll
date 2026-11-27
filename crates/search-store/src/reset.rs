//! Resetting a cluster in place to another cluster's indices (a branch back to a copy of it: a
//! preview's restore point).
//!
//! The cluster keeps its name and everything that isn't its indices' data: the caller merges the
//! state it takes (its own, with the other cluster's index metadata). Each shard of the other
//! cluster becomes the next generation of the same shard here, written through the manifests'
//! compare-and-swap path like any commit point (fenced by ownership, older generations kept for
//! point-in-time branches by retention), naming the same immutable segments, live docs and WAL
//! ranges, as a branch does ([`Store::copied_manifest`]); shards only this cluster has get a
//! tombstone; the state comes last. Nothing is copied but commit points.
//!
//! [`Store::begin_reset`] reads the other cluster once (its state and commit points, again until
//! nothing moved in between) and writes everything the reset applies under the cluster it resets:
//! the commit points (`reset/`, which garbage collection keeps as it keeps a latest commit point,
//! WAL ranges included), the merged state, then the intent, the reset's commit point. From then
//! on the reset needs nothing of the other cluster, which may be deleted. The cluster's next load
//! carries it out ([`Store::load_cluster`]), on its owner, after its claim, before anything reads
//! it, and a load after a crash midway does the rest: every step can be done twice (a shard whose
//! newest commit point already came from this reset is skipped). A reset that can't be carried
//! out (its copies gone or damaged) or that is stale (the cluster was written after it was asked
//! for: an owner that loaded it without seeing the intent) is dropped, and the cluster stays as
//! it is; a load never fails on one for good.

use crate::keys::{self, ShardId, Version};
use crate::manifest::{ClusterState, ShardManifest};
use crate::wal::Listings;
use crate::{Put, Refused, ShardTrack, Store, branch::Copied, now_ms};
use anyhow::bail;
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};

/// What a reset in progress applies.
#[derive(Serialize, Deserialize)]
struct Intent {
    from: String,
    /// When it was asked for: it marks the commit points it writes (`BranchBase::branched_ms`),
    /// cuts the WAL ranges they take, and makes it stale once the cluster is written after it.
    at_ms: u64,
    /// The lease epoch it was asked under (0 on one node), for the record.
    epoch: i64,
    /// Its shards, each copied under `reset/`.
    shards: Vec<(String, u32)>,
}

/// Attempts at reading `from` with nothing moving in between.
const CONSISTENT_READS: usize = 3;

impl Store {
    /// Reset `target` in place to `from`'s indices: reads `from` and records the reset (`merge`
    /// makes the state `target` takes from `target`'s state and `from`'s, `None` for one never
    /// stored), which [`Store::load_cluster`] then carries out when `target` next loads. Call it
    /// with `target` unloaded everywhere and under its lease (`epoch`) on a fleet, and check the
    /// lease is still held after it. Once this returns, the reset happens: no load reads `target`
    /// without it (or it is dropped as stale).
    pub async fn begin_reset(&self, target: &str, from: &str, epoch: i64, merge: impl FnOnce(&[u8], Option<&[u8]>) -> anyhow::Result<Vec<u8>>) -> anyhow::Result<()> {
        keys::validate_cluster(target)?;
        keys::validate_cluster(from)?;
        anyhow::ensure!(target != from, "a cluster can't be reset to itself");
        if self.reset_pending(from).await? {
            return Err(Refused(format!("cluster [{from}] has a reset of its own in progress; it applies when it next loads")).into());
        }
        let Some(current) = self.read_cluster(target).await? else { bail!("cluster [{target}] has never stored anything") };
        // `from` as one moment: its commit points and state, read again until neither moved.
        let tops = |h: std::collections::BTreeMap<ShardId, Vec<keys::ShardVersion>>| -> Vec<(ShardId, Version)> {
            h.into_iter().filter_map(|(s, vs)| vs.last().filter(|v| !v.deleted).map(|v| (s, v.version))).collect()
        };
        let mut read = None;
        for _ in 0..CONSISTENT_READS {
            let before = tops(self.list_shard_histories(from).await?);
            let state = self.read_cluster(from).await?;
            let after = tops(self.list_shard_histories(from).await?);
            if before == after && state.as_ref().map(|s| (s.epoch, s.generation)) == self.read_cluster(from).await?.map(|s| (s.epoch, s.generation)) {
                read = Some((before, state));
                break;
            }
        }
        let Some((shards, from_state)) = read else { return Err(Refused(format!("cluster [{from}] kept changing while it was read; retry")).into()) };
        // Its operations not flushed yet are read from object storage, whichever node wrote
        // them; this node's are shipped now.
        self.wal().sync_remote().await?;
        let at = now_ms();
        let copied = Copied { at_ms: None, branched_ms: at, cut: at, cur: self.wal().current_file() };
        let listings = &Listings::default();
        let copies: Vec<(ShardId, ShardManifest)> = futures::stream::iter(shards.into_iter().map(|(shard, v)| async move {
            let base = self
                .get_manifest(from, &shard, keys::ShardVersion { version: v, deleted: false })
                .await?
                .ok_or_else(|| Refused(format!("shard {shard} of cluster [{from}] changed while it was read; retry")))?;
            let sources = base.wal.clone();
            anyhow::Ok((shard, self.copied_manifest(from, base, sources, copied, listings).await?))
        }))
        .buffered(16)
        .try_collect()
        .await?;
        let state = merge(&current.json, from_state.as_ref().map(|s| &s.json[..]))?;
        // Leftovers of a reset that never got its intent written.
        self.objects().delete_prefix(&keys::reset_copies_prefix(target)).await?;
        let names: Vec<(String, u32)> = copies.iter().map(|(s, _)| (s.index_uuid.clone(), s.shard)).collect();
        futures::stream::iter(copies.into_iter().map(|(shard, m)| async move { self.objects().put(&keys::reset_copy_key(target, &shard), Bytes::from(m.to_json())).await }))
            .buffer_unordered(16)
            .try_collect::<Vec<()>>()
            .await?;
        self.objects().put(&keys::reset_state_key(target), Bytes::from(ClusterState::new(state).encode(3)?)).await?;
        let intent = Intent { from: from.to_string(), at_ms: at, epoch, shards: names };
        self.objects().put(&keys::reset_key(target), Bytes::from(serde_json::to_vec(&intent)?)).await?;
        // An owner that claimed it meanwhile reads it without the intent: the next load drops it.
        self.check_owner(target).await?;
        Ok(())
    }

    /// Whether `cluster` has a reset waiting for its next load ([`Store::begin_reset`]).
    pub async fn reset_pending(&self, cluster: &str) -> anyhow::Result<bool> {
        keys::validate_cluster(cluster)?;
        Ok(self.objects().get(&keys::reset_key(cluster)).await?.is_some())
    }

    /// The latest cluster state for loading the cluster, after carrying out a reset that is due
    /// ([`Store::begin_reset`]): one listing of its states, which shows the reset too.
    pub async fn load_cluster(&self, cluster: &str) -> anyhow::Result<Option<ClusterState>> {
        keys::validate_cluster(cluster)?;
        let prefix = keys::state_prefix(cluster);
        let listed = self.objects().list(&prefix).await?;
        let reset = keys::reset_key(cluster);
        if listed.iter().any(|(k, _)| *k == reset) {
            self.finish_reset(cluster).await?;
            return self.read_cluster(cluster).await;
        }
        let top = listed.iter().filter(|(k, _)| k.strip_prefix(&prefix).is_some_and(|f| !f.contains('/'))).filter_map(|(k, _)| keys::parse_version(k, ".json.zst")).max();
        let Some(top) = top else { return Ok(None) };
        match self.get_state(cluster, top).await? {
            Some(s) => Ok(Some(s)),
            // Gone since the listing: look again.
            None => self.read_cluster(cluster).await,
        }
    }

    async fn finish_reset(&self, target: &str) -> anyhow::Result<()> {
        let Some(raw) = self.objects().get(&keys::reset_key(target)).await? else { return Ok(()) };
        let Ok(intent) = serde_json::from_slice::<Intent>(&raw) else { return self.drop_reset(target, "its intent is damaged").await };
        let state = match self.objects().get(&keys::reset_state_key(target)).await? {
            Some(raw) => match ClusterState::decode(&raw) {
                Ok(s) => s,
                Err(_) => return self.drop_reset(target, "its state is damaged").await,
            },
            None => return self.drop_reset(target, "its state is missing").await,
        };
        let ours = |m: &ShardManifest| m.base.as_ref().is_some_and(|b| b.cluster == intent.from && b.branched_ms == intent.at_ms);
        // Written after it was asked for, other than by this reset: an owner served the cluster
        // without it. What it wrote stands.
        if let Some(latest) = self.read_cluster(target).await?
            && latest.written_ms > intent.at_ms
        {
            if latest.json == state.json {
                // Everything was applied; only the leftovers remain.
                return self.clear_reset(target).await;
            }
            return self.drop_reset(target, "the cluster's state was written after it").await;
        }
        let existing = self.read_latest_manifests(target).await?;
        if existing.iter().any(|(_, m)| !ours(m) && m.written_ms > intent.at_ms) {
            return self.drop_reset(target, "the cluster's commit points were written after it").await;
        }
        let mut copies = Vec::with_capacity(intent.shards.len());
        for (uuid, n) in &intent.shards {
            let shard = ShardId::new(uuid.clone(), *n);
            let m = match self.objects().get(&keys::reset_copy_key(target, &shard)).await? {
                Some(raw) => match ShardManifest::from_json(&raw) {
                    Ok(m) => m,
                    Err(_) => return self.drop_reset(target, &format!("its copy of shard {shard} is damaged")).await,
                },
                None => return self.drop_reset(target, &format!("its copy of shard {shard} is missing")).await,
            };
            copies.push((shard, m));
        }
        let done: Vec<ShardId> = existing.iter().filter(|(_, m)| ours(m)).map(|(s, _)| s.clone()).collect();
        let todo: Vec<(ShardId, ShardManifest)> = copies.iter().filter(|(s, _)| !done.contains(s)).cloned().collect();
        futures::stream::iter(todo.into_iter().map(|(shard, m)| async move {
            let shard = &shard;
            let track = self.shard_track(target, shard);
            let mut t = track.lock().await;
            // What this node remembers of the shard's old history (its WAL, a deletion) is no
            // longer the shard's.
            self.wal().forget_shard(target, shard);
            *t = ShardTrack::default();
            self.put_manifest(&mut t, target, shard, m, Put::Revive).await?;
            anyhow::Ok(())
        }))
        .buffer_unordered(16)
        .try_collect::<Vec<()>>()
        .await?;
        for (shard, _) in &existing {
            if !copies.iter().any(|(s, _)| s == shard) {
                self.delete_shard(target, shard).await?;
            }
        }
        self.write_cluster(target, &ClusterState::new(state.json)).await?;
        self.clear_reset(target).await
    }

    /// Gives up a reset that can't be carried out: the cluster stays as it is.
    async fn drop_reset(&self, target: &str, why: &str) -> anyhow::Result<()> {
        tracing::warn!(cluster = target, why, "a reset was dropped; the cluster stays as it was");
        self.clear_reset(target).await
    }

    /// The intent first (it says a reset is due), then what it applied.
    async fn clear_reset(&self, target: &str) -> anyhow::Result<()> {
        self.objects().delete(&keys::reset_key(target)).await?;
        for r in [self.objects().delete(&keys::reset_state_key(target)).await, self.objects().delete_prefix(&keys::reset_copies_prefix(target)).await.map(|_| ())] {
            if let Err(e) = r {
                tracing::warn!(cluster = target, error = %e, "deleting a finished reset's leftovers failed");
            }
        }
        Ok(())
    }
}
