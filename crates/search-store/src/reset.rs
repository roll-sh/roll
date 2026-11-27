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
//! the commit points (`reset/`, which garbage collection keeps while the reset is pending, WAL
//! ranges included), the merged state, then the intent, the reset's commit point. On a fleet it
//! claims the cluster first, so an older owner's writes are refused from then on. From then on
//! the reset needs nothing of the other cluster, which may be deleted. The cluster's next load
//! carries it out ([`Store::load_cluster`]), on its owner, after its claim, before anything reads
//! it, and a load after a crash midway does the rest: every step can be done twice (a shard whose
//! newest commit point already came from this reset is skipped).
//!
//! The intent records the version of the cluster's state and of each shard's newest commit point
//! when it was written. A reset finds the cluster written since by anything but itself (an owner
//! that loaded it without seeing the intent) stale; no clock is compared. A stale reset, or one
//! that can't be carried out (its copies or state gone or damaged), is dropped, and the cluster
//! stays as it is: a load never fails on one for good. Either way the outcome is recorded
//! ([`Store::reset_outcome`]), so the caller can tell a reset applied from one dropped.

use crate::keys::{self, ShardId, ShardVersion, Version};
use crate::manifest::{ClusterState, ShardManifest};
use crate::wal::Listings;
use crate::{Put, Refused, ShardTrack, Store, branch::Copied, now_ms};
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What a reset in progress applies.
#[derive(Serialize, Deserialize)]
struct Intent {
    from: String,
    /// When it was asked for: it marks the commit points it writes (`BranchBase::branched_ms`)
    /// and cuts the WAL ranges they take. Never compared with another node's clock.
    at_ms: u64,
    /// The store epoch it claimed the cluster under (a fleet), for the record.
    epoch: Option<u64>,
    /// The version of the cluster's state when it was written.
    state: Version,
    /// Each shard's newest generation then (a tombstone included).
    tops: Vec<(String, u32, ShardVersion)>,
    /// Its shards, each copied under `reset/`.
    shards: Vec<(String, u32)>,
}

/// What became of a reset ([`Store::reset_outcome`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResetOutcome {
    Applied,
    /// Dropped, and why: the cluster stayed as it was.
    Dropped(String),
    /// Not carried out yet: the cluster's next load does it.
    Pending,
}

#[derive(Serialize, Deserialize)]
struct Outcome {
    at_ms: u64,
    outcome: ResetOutcome,
}

/// A reset [`Store::begin_reset`] recorded: what [`Store::reset_outcome`] asks about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResetTicket {
    pub at_ms: u64,
}

/// Attempts at reading `from` with nothing moving in between.
const CONSISTENT_READS: usize = 3;

fn tops(h: BTreeMap<ShardId, Vec<ShardVersion>>) -> BTreeMap<ShardId, ShardVersion> {
    h.into_iter().filter_map(|(s, vs)| vs.last().copied().map(|v| (s, v))).collect()
}

impl Store {
    /// Reset `target` in place to `from`'s indices: reads `from` and records the reset (`merge`
    /// makes the state `target` takes from `target`'s state and `from`'s), which
    /// [`Store::load_cluster`] then carries out when `target` next loads. Call it with `target`
    /// unloaded everywhere; on a fleet (`claim`), under its lease: it claims `target` before
    /// writing anything. Once this returns, the reset happens at the next load, or is dropped
    /// there as stale ([`Store::reset_outcome`]). Refused ([`Refused`]) when `from` stored nothing
    /// (or is being deleted), keeps changing, or either has a reset pending.
    pub async fn begin_reset(&self, target: &str, from: &str, claim: bool, merge: impl FnOnce(&[u8], &[u8]) -> anyhow::Result<Vec<u8>>) -> anyhow::Result<ResetTicket> {
        keys::validate_cluster(target)?;
        keys::validate_cluster(from)?;
        anyhow::ensure!(target != from, "a cluster can't be reset to itself");
        for c in [target, from] {
            if self.reset_pending(c).await? {
                return Err(Refused(format!("cluster [{c}] has a reset pending; it applies when it next loads")).into());
            }
        }
        let Some(current) = self.read_cluster(target).await? else { return Err(Refused(format!("cluster [{target}] has never stored anything")).into()) };
        // `from` as one moment: its commit points and state, read again until neither moved.
        let mut read = None;
        for _ in 0..CONSISTENT_READS {
            let before = tops(self.list_shard_histories(from).await?);
            let state = self.read_cluster(from).await?;
            let after = tops(self.list_shard_histories(from).await?);
            if before == after && state.as_ref().map(|s| (s.epoch, s.generation)) == self.read_cluster(from).await?.map(|s| (s.epoch, s.generation)) {
                read = Some((after, state));
                break;
            }
        }
        let Some((from_tops, from_state)) = read else { return Err(Refused(format!("cluster [{from}] kept changing while it was read; retry")).into()) };
        // Gone mid-read (a delete removes the state first) or never written: nothing to reset to.
        let Some(from_state) = from_state else { return Err(Refused(format!("cluster [{from}] has no stored state (never written, or being deleted)")).into()) };
        // An older owner's writes are refused from here on.
        let epoch = if claim { Some(self.claim_next(target).await?) } else { None };
        let state_version = self.list_versions(&keys::state_prefix(target), ".json.zst").await?.last().copied();
        let Some(state_version) = state_version else { return Err(Refused(format!("cluster [{target}] has never stored anything")).into()) };
        let target_tops = tops(self.list_shard_histories(target).await?);
        // Its operations not flushed yet are read from object storage, whichever node wrote
        // them; this node's are shipped now.
        self.wal().sync_remote().await?;
        let at = now_ms();
        let copied = Copied { at_ms: None, branched_ms: at, cut: at, cur: self.wal().current_file() };
        let listings = &Listings::default();
        let copies: Vec<(ShardId, ShardManifest)> = futures::stream::iter(from_tops.into_iter().filter(|(_, v)| !v.deleted).map(|(shard, v)| async move {
            let base = self.get_manifest(from, &shard, v).await?.ok_or_else(|| Refused(format!("shard {shard} of cluster [{from}] changed while it was read; retry")))?;
            let sources = base.wal.clone();
            anyhow::Ok((shard, self.copied_manifest(from, base, sources, copied, listings).await?))
        }))
        .buffered(16)
        .try_collect()
        .await?;
        let state = merge(&current.json, &from_state.json)?;
        let intent = Intent {
            from: from.to_string(),
            at_ms: at,
            epoch,
            state: state_version,
            tops: target_tops.into_iter().map(|(s, v)| (s.index_uuid, s.shard, v)).collect(),
            shards: copies.iter().map(|(s, _)| (s.index_uuid.clone(), s.shard)).collect(),
        };
        if let Err(e) = self.write_reset(target, copies, state, &intent).await {
            // What it wrote before failing applies to nothing: garbage collection would keep it.
            let _ = self.objects().delete_prefix(&keys::reset_copies_prefix(target)).await;
            let _ = self.objects().delete(&keys::reset_state_key(target)).await;
            return Err(e);
        }
        // Claimed: a newer owner since would fence this, and its load would find the intent.
        self.check_owner(target).await?;
        Ok(ResetTicket { at_ms: at })
    }

    async fn write_reset(&self, target: &str, copies: Vec<(ShardId, ShardManifest)>, state: Vec<u8>, intent: &Intent) -> anyhow::Result<()> {
        // Leftovers of a reset that never got its intent written, and an earlier outcome.
        self.objects().delete_prefix(&keys::reset_copies_prefix(target)).await?;
        self.objects().delete(&keys::reset_outcome_key(target)).await?;
        futures::stream::iter(copies.into_iter().map(|(shard, m)| async move { self.objects().put(&keys::reset_copy_key(target, &shard), Bytes::from(m.to_json())).await }))
            .buffer_unordered(16)
            .try_collect::<Vec<()>>()
            .await?;
        self.objects().put(&keys::reset_state_key(target), Bytes::from(ClusterState::new(state).encode(3)?)).await?;
        self.objects().put(&keys::reset_key(target), Bytes::from(serde_json::to_vec(intent)?)).await?;
        Ok(())
    }

    /// Whether `cluster` has a reset waiting for its next load ([`Store::begin_reset`]).
    pub async fn reset_pending(&self, cluster: &str) -> anyhow::Result<bool> {
        keys::validate_cluster(cluster)?;
        Ok(self.objects().get(&keys::reset_key(cluster)).await?.is_some())
    }

    /// What became of the reset `ticket` names: applied, dropped (and why), or still pending.
    pub async fn reset_outcome(&self, cluster: &str, ticket: ResetTicket) -> anyhow::Result<ResetOutcome> {
        keys::validate_cluster(cluster)?;
        if let Some(raw) = self.objects().get(&keys::reset_outcome_key(cluster)).await?
            && let Ok(o) = serde_json::from_slice::<Outcome>(&raw)
            && o.at_ms == ticket.at_ms
        {
            return Ok(o.outcome);
        }
        Ok(if self.reset_pending(cluster).await? { ResetOutcome::Pending } else { ResetOutcome::Dropped("its outcome wasn't recorded".into()) })
    }

    /// Gives up the reset `cluster` has pending (its source went away before it was recorded,
    /// or a delete can't wait for it): the cluster stays as it is.
    pub async fn cancel_reset(&self, cluster: &str, why: &str) -> anyhow::Result<()> {
        keys::validate_cluster(cluster)?;
        let at = match self.objects().get(&keys::reset_key(cluster)).await? {
            Some(raw) => serde_json::from_slice::<Intent>(&raw).map(|i| i.at_ms).unwrap_or(0),
            None => return Ok(()),
        };
        self.end_reset(cluster, at, ResetOutcome::Dropped(why.to_string())).await
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
        let Ok(intent) = serde_json::from_slice::<Intent>(&raw) else { return self.end_reset(target, 0, ResetOutcome::Dropped("its intent is damaged".into())).await };
        let at = intent.at_ms;
        let dropped = |why: &str| self.end_reset(target, at, ResetOutcome::Dropped(why.to_string()));
        let state = match self.objects().get(&keys::reset_state_key(target)).await? {
            Some(raw) => match ClusterState::decode(&raw) {
                Ok(s) => s,
                Err(_) => return dropped("its state is damaged").await,
            },
            None => return dropped("its state is missing").await,
        };
        let ours = |m: &ShardManifest| m.base.as_ref().is_some_and(|b| b.cluster == intent.from && b.branched_ms == at);
        let copied: Vec<ShardId> = intent.shards.iter().map(|(u, n)| ShardId::new(u.clone(), *n)).collect();
        // Written since by anything but this reset: an owner served the cluster without it, and
        // what it wrote stands.
        let state_now = self.list_versions(&keys::state_prefix(target), ".json.zst").await?.last().copied();
        if state_now != Some(intent.state) {
            if let Some(latest) = self.read_cluster(target).await?
                && latest.json == state.json
            {
                // Everything was applied (the state comes last); only the leftovers remain.
                return self.end_reset(target, at, ResetOutcome::Applied).await;
            }
            return dropped("the cluster's state was written after it was recorded").await;
        }
        let then: BTreeMap<ShardId, ShardVersion> = intent.tops.iter().map(|(u, n, v)| (ShardId::new(u.clone(), *n), *v)).collect();
        let now = tops(self.list_shard_histories(target).await?);
        let mut done: Vec<ShardId> = Vec::new();
        for (shard, top) in &now {
            if then.get(shard) == Some(top) {
                continue;
            }
            // This reset's own writes: a copy applied, or the tombstone of a shard it doesn't keep.
            let own = match top.deleted {
                true => !copied.contains(shard),
                false => self.get_manifest(target, shard, *top).await?.is_some_and(|m| ours(&m)),
            };
            if !own {
                return dropped(&format!("shard {shard} of the cluster was written after it was recorded")).await;
            }
            if !top.deleted {
                done.push(shard.clone());
            }
        }
        let mut copies = Vec::with_capacity(copied.len());
        for shard in copied.iter().filter(|s| !done.contains(s)) {
            let m = match self.objects().get(&keys::reset_copy_key(target, shard)).await? {
                Some(raw) => match ShardManifest::from_json(&raw) {
                    Ok(m) => m,
                    Err(_) => return dropped(&format!("its copy of shard {shard} is damaged")).await,
                },
                None => return dropped(&format!("its copy of shard {shard} is missing")).await,
            };
            copies.push((shard.clone(), m));
        }
        futures::stream::iter(copies.into_iter().map(|(shard, m)| async move {
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
        for (shard, top) in &now {
            if !top.deleted && !copied.contains(shard) {
                self.delete_shard(target, shard).await?;
            }
        }
        self.write_cluster(target, &ClusterState::new(state.json)).await?;
        self.end_reset(target, at, ResetOutcome::Applied).await
    }

    /// Records what became of the reset, then removes its intent (which says a reset is due),
    /// then what it applied.
    async fn end_reset(&self, target: &str, at_ms: u64, outcome: ResetOutcome) -> anyhow::Result<()> {
        if let ResetOutcome::Dropped(why) = &outcome {
            tracing::warn!(cluster = target, why, "a reset was dropped; the cluster stays as it was");
        }
        self.objects().put(&keys::reset_outcome_key(target), Bytes::from(serde_json::to_vec(&Outcome { at_ms, outcome })?)).await?;
        self.objects().delete(&keys::reset_key(target)).await?;
        for r in [self.objects().delete(&keys::reset_state_key(target)).await, self.objects().delete_prefix(&keys::reset_copies_prefix(target)).await.map(|_| ())] {
            if let Err(e) = r {
                tracing::warn!(cluster = target, error = %e, "deleting a finished reset's leftovers failed (garbage collection removes them)");
            }
        }
        Ok(())
    }
}
