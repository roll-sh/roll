//! Resetting a cluster in place to another cluster's indices (a branch back to a copy of it: a
//! preview's restore point).
//!
//! The cluster keeps its name and everything that isn't its indices' data: the caller passes the
//! state it takes (its own, with the other cluster's index metadata). Each shard of the other
//! cluster becomes the next generation of the same shard here, written through the manifests'
//! compare-and-swap path like any commit point (fenced by ownership, older generations kept for
//! point-in-time branches by retention), naming the same immutable segments, live docs and WAL
//! ranges, as a branch does ([`Store::copied_manifest`]); shards only this cluster has get a
//! tombstone; the state comes last. Nothing is copied, and garbage collection never sees a
//! segment unreferenced: the other cluster's commit points reference it throughout.
//!
//! [`Store::begin_reset`] only records what to do (the reset's commit point); the cluster's next
//! load does it ([`Store::load_cluster`]), on its owner, after its claim, before anything is
//! read, and a load after a crash midway does the rest. Every step can be done twice: a shard
//! whose newest commit point already came from this reset is skipped.

use crate::keys::{self, ShardId, ShardVersion, Version};
use crate::manifest::ClusterState;
use crate::wal::Listings;
use crate::{Put, ShardTrack, Store, branch::Copied, now_ms};
use anyhow::{Context, bail};
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};

/// What a reset in progress applies.
#[derive(Serialize, Deserialize)]
struct Intent {
    from: String,
    /// When it was asked for: it marks the commit points it writes (`BranchBase::branched_ms`)
    /// and cuts the WAL ranges they take.
    at_ms: u64,
    /// The commit points of `from` it applies, as they were then (`from` may be written to
    /// meanwhile): `(index uuid, shard, version)`.
    shards: Vec<(String, u32, Version)>,
}

impl Store {
    /// Reset `target` in place to `from`'s indices, with cluster state `state`: records the
    /// reset, which [`Store::load_cluster`] then carries out when `target` next loads (call it
    /// with `target` unloaded everywhere). Once this returns, the reset happens: no load reads
    /// `target` without it.
    pub async fn begin_reset(&self, target: &str, from: &str, state: Bytes) -> anyhow::Result<()> {
        keys::validate_cluster(target)?;
        keys::validate_cluster(from)?;
        anyhow::ensure!(target != from, "a cluster can't be reset to itself");
        if !self.cluster_exists(target).await? {
            bail!("no such cluster [{target}]");
        }
        let mut shards = Vec::new();
        for (shard, versions) in self.list_shard_histories(from).await? {
            if let Some(top) = versions.last().filter(|v| !v.deleted) {
                shards.push((shard.index_uuid, shard.shard, top.version));
            }
        }
        let intent = Intent { from: from.to_string(), at_ms: now_ms(), shards };
        // The state first: the intent is what says a reset is due.
        self.objects().put(&keys::reset_state_key(target), Bytes::from(ClusterState::new(state).encode(3)?)).await?;
        self.objects().put(&keys::reset_key(target), Bytes::from(serde_json::to_vec(&intent)?)).await?;
        Ok(())
    }

    /// The latest cluster state for loading the cluster: [`Store::read_cluster`], after carrying
    /// out a reset that is due ([`Store::begin_reset`]). One listing when none is.
    pub async fn load_cluster(&self, cluster: &str) -> anyhow::Result<Option<ClusterState>> {
        keys::validate_cluster(cluster)?;
        let reset = keys::reset_key(cluster);
        if self.objects().list(&keys::state_prefix(cluster)).await?.iter().any(|(k, _)| *k == reset) {
            self.finish_reset(cluster).await?;
        }
        self.read_cluster(cluster).await
    }

    async fn finish_reset(&self, target: &str) -> anyhow::Result<()> {
        let Some(raw) = self.objects().get(&keys::reset_key(target)).await? else { return Ok(()) };
        let intent: Intent = serde_json::from_slice(&raw).with_context(|| format!("the reset of cluster [{target}] is damaged"))?;
        let state = self.objects().get(&keys::reset_state_key(target)).await?.with_context(|| format!("the state of the reset of cluster [{target}] is missing"))?;
        let state = ClusterState::decode(&state).with_context(|| format!("the state of the reset of cluster [{target}] is damaged"))?;
        let from = intent.from.as_str();
        // `from`'s operations not flushed yet are read from object storage, whichever node wrote
        // them; this node's are shipped now.
        self.wal().sync_remote().await?;
        let cur = self.wal().current_file();
        let listings = &Listings::default();
        let copied = Copied { at_ms: None, branched_ms: intent.at_ms, cut: intent.at_ms, cur };
        let wanted: Vec<(ShardId, ShardVersion)> =
            intent.shards.iter().map(|(uuid, n, version)| (ShardId::new(uuid.clone(), *n), ShardVersion { version: *version, deleted: false })).collect();
        let intent = &intent;
        futures::stream::iter(wanted.clone().into_iter().map(|(shard, v)| async move {
            let shard = &shard;
            let track = self.shard_track(target, shard);
            let mut t = track.lock().await;
            if let Some(top) = self.read_manifest(target, shard).await?
                && top.base.as_ref().is_some_and(|b| b.cluster == from && b.branched_ms == intent.at_ms)
            {
                return anyhow::Ok(());
            }
            let base = match self.get_manifest(from, shard, v).await? {
                Some(m) => m,
                // Gone since (retention after `from` was written to): its latest stands in.
                None => {
                    tracing::warn!(target, from, %shard, "a commit point a reset applies is gone; applying the newest");
                    self.read_manifest(from, shard).await?.with_context(|| format!("shard {shard} of cluster [{from}] is gone"))?
                }
            };
            let sources = base.wal.clone();
            let m = self.copied_manifest(from, base, sources, copied, listings).await?;
            // What this node remembers of the shard's old history (its WAL, a deletion) is no
            // longer the shard's.
            self.wal().forget_shard(target, shard);
            *t = ShardTrack::default();
            self.put_manifest(&mut t, target, shard, m, Put::Revive).await?;
            Ok(())
        }))
        .buffer_unordered(16)
        .try_collect::<Vec<()>>()
        .await?;
        for shard in self.list_shards(target).await? {
            if !wanted.iter().any(|(s, _)| *s == shard) {
                self.delete_shard(target, &shard).await?;
            }
        }
        self.write_cluster(target, &ClusterState::new(state.json)).await?;
        self.objects().delete(&keys::reset_key(target)).await?;
        if let Err(e) = self.objects().delete(&keys::reset_state_key(target)).await {
            tracing::warn!(target, error = %e, "deleting a finished reset's state failed");
        }
        Ok(())
    }
}
