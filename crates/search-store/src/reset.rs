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
use crate::manifest::{BranchBase, ClusterState, ShardManifest};
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

/// The [`ShardManifest::user_data`] key marking a commit point a reset wrote (its `at_ms`). An
/// instance key: a branch of the cluster doesn't inherit it.
const RESET_MARK: &str = "roll.instance.reset";

fn tops(h: BTreeMap<ShardId, Vec<ShardVersion>>) -> BTreeMap<ShardId, ShardVersion> {
    h.into_iter().filter_map(|(s, vs)| vs.last().copied().map(|v| (s, v))).collect()
}

impl Store {
    /// Reset `target` in place to `from`'s indices: reads `from` and records the reset (`merge`
    /// makes the state `target` takes from `target`'s state and `from`'s), which
    /// [`Store::load_cluster`] then carries out when `target` next loads. Call it with `target`
    /// unloaded everywhere; on a fleet (`claim`), under its lease: it claims `target` before
    /// writing anything. Once this returns, the reset happens at the next load, or is dropped
    /// there as stale ([`Store::reset_outcome`]). A `from` that never stored anything (a copy
    /// made while the cluster was empty) resets it to no indices (`merge` gets `None`); the
    /// caller keeps `from` from being deleted meanwhile. Refused ([`Refused`]) when `from` is
    /// half deleted (commit points without a state), keeps changing, or either has a reset
    /// pending; [`crate::Fenced`] when another node claimed `target` meanwhile.
    ///
    /// The reset's commit points keep `target`'s own lineage (the branch base its commit points
    /// had), so what `target` holds of its parent's data counts as shared as before
    /// ([`Store::cluster_usage`]), however long `from` lives.
    /// `parent`: the cluster `target` was branched from (its catalog's), whose data it shares: the
    /// lineage its reset commit points carry even when no live shard of it still does (its
    /// indices all deleted or made since, or an empty restore point put back before).
    pub async fn begin_reset(
        &self,
        target: &str,
        from: &str,
        parent: Option<&str>,
        claim: bool,
        merge: impl FnOnce(&[u8], Option<&[u8]>) -> anyhow::Result<Vec<u8>>,
    ) -> anyhow::Result<ResetTicket> {
        keys::validate_cluster(target)?;
        keys::validate_cluster(from)?;
        anyhow::ensure!(target != from, "a cluster can't be reset to itself");
        for c in [target, from] {
            if self.reset_pending(c).await? {
                return Err(Refused(format!("cluster [{c}] has a reset pending; it applies when it next loads")).into());
            }
        }
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
        // A delete removes the state first, then the commit points: both or neither, never one.
        if from_state.is_none() && from_tops.values().any(|v| !v.deleted) {
            return Err(Refused(format!("cluster [{from}] is being deleted")).into());
        }
        // An older owner's writes are refused from here on.
        let epoch = if claim { Some(self.claim_next(target).await?) } else { None };
        // The state the merge reads is exactly the one the intent records.
        let state_version = self.list_versions(&keys::state_prefix(target), ".json.zst").await?.last().copied();
        let current = match state_version {
            Some(v) => self.get_state(target, v).await?,
            None => None,
        };
        let (Some(state_version), Some(current)) = (state_version, current) else {
            return Err(Refused(format!("cluster [{target}] has never stored anything, or its state changed while it was read")).into());
        };
        let target_tops = tops(self.list_shard_histories(target).await?);
        // Its own lineage, which the reset's commit points keep: its parent's, as a live shard
        // carries it or, with none left that does, as the catalog names it.
        let live = self.read_latest_manifests(target).await?.into_iter().find_map(|(_, m)| m.base);
        let lineage = match (live, parent) {
            (Some(b), Some(p)) if b.cluster == p => Some(b),
            (_, Some(p)) => Some(BranchBase { cluster: p.to_string(), generation: 0, at_ms: None, branched_ms: now_ms() }),
            (live, None) => live,
        };
        // Its operations not flushed yet are read from object storage, whichever node wrote
        // them; this node's are shipped now.
        self.wal().sync_remote().await?;
        let at = now_ms();
        let copied = Copied { at_ms: None, branched_ms: at, cut: at, cur: self.wal().current_file() };
        let listings = &Listings::default();
        let lineage = &lineage;
        let copies: Vec<(ShardId, ShardManifest)> = futures::stream::iter(from_tops.into_iter().filter(|(_, v)| !v.deleted).map(|(shard, v)| async move {
            let base = self.get_manifest(from, &shard, v).await?.ok_or_else(|| Refused(format!("shard {shard} of cluster [{from}] changed while it was read; retry")))?;
            let sources = base.wal.clone();
            let mut m = self.copied_manifest(from, base, sources, copied, listings).await?;
            m.base = lineage.clone();
            m.user_data.insert(RESET_MARK.to_string(), at.to_string());
            anyhow::Ok((shard, m))
        }))
        .buffered(16)
        .try_collect()
        .await?;
        let state = merge(&current.json, from_state.as_ref().map(|s| &s.json[..]))?;
        let intent = Intent {
            from: from.to_string(),
            at_ms: at,
            epoch,
            state: state_version,
            tops: target_tops.into_iter().map(|(s, v)| (s.index_uuid, s.shard, v)).collect(),
            shards: copies.iter().map(|(s, _)| (s.index_uuid.clone(), s.shard)).collect(),
        };
        if let Err(e) = self.write_reset(target, at, copies, state, &intent).await {
            // What it wrote before failing applies to nothing.
            self.discard_reset(target, at).await;
            return Err(e);
        }
        // Another node claimed it meanwhile and may serve it without the intent: given up here,
        // so it never applies over what that owner writes.
        if let Err(e) = self.check_owner(target).await {
            self.discard_reset(target, at).await;
            return Err(e);
        }
        Ok(ResetTicket { at_ms: at })
    }

    /// Best effort: the reset recorded at `at` will never be carried out, and leaves nothing
    /// behind. Only its own objects: another node's reset of the cluster (a newer owner's) is
    /// left alone.
    async fn discard_reset(&self, target: &str, at: u64) {
        if let Ok(Some(raw)) = self.objects().get(&keys::reset_key(target)).await
            && serde_json::from_slice::<Intent>(&raw).is_ok_and(|i| i.at_ms == at)
        {
            let _ = self.objects().delete(&keys::reset_key(target)).await;
        }
        let _ = self.objects().delete_prefix(&keys::reset_dir(target, at)).await;
    }

    /// Its copies and state under its own `reset/<at>/`, then the intent, created only where
    /// none is: two resets never overwrite each other's.
    async fn write_reset(&self, target: &str, at: u64, copies: Vec<(ShardId, ShardManifest)>, state: Vec<u8>, intent: &Intent) -> anyhow::Result<()> {
        futures::stream::iter(copies.into_iter().map(|(shard, m)| async move { self.objects().put(&keys::reset_copy_key(target, at, &shard), Bytes::from(m.to_json())).await }))
            .buffer_unordered(16)
            .try_collect::<Vec<()>>()
            .await?;
        self.objects().put(&keys::reset_state_key(target, at), Bytes::from(ClusterState::new(state).encode(3)?)).await?;
        if self.objects().put_new(&keys::reset_key(target), Bytes::from(serde_json::to_vec(intent)?)).await?.is_none() {
            return Err(Refused(format!("cluster [{target}] has a reset pending; it applies when it next loads")).into());
        }
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

    /// Whether the reset `cluster` has pending already wrote part of what it applies (a load
    /// that failed midway): giving it up then would keep a mix of both.
    /// Started is anything moved since it was recorded: a shard revived or deleted, or the state.
    pub async fn reset_started(&self, cluster: &str) -> anyhow::Result<bool> {
        keys::validate_cluster(cluster)?;
        let Some(raw) = self.objects().get(&keys::reset_key(cluster)).await? else { return Ok(false) };
        let Ok(intent) = serde_json::from_slice::<Intent>(&raw) else { return Ok(false) };
        if self.list_versions(&keys::state_prefix(cluster), ".json.zst").await?.last().copied() != Some(intent.state) {
            return Ok(true);
        }
        let then: BTreeMap<ShardId, ShardVersion> = intent.tops.iter().map(|(u, n, v)| (ShardId::new(u.clone(), *n), *v)).collect();
        let now = tops(self.list_shard_histories(cluster).await?);
        // A shard removed outright (no history kept, so no tombstone) counts too.
        Ok(now.iter().any(|(s, v)| then.get(s) != Some(v)) || then.keys().any(|s| !now.contains_key(s)))
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
        tracing::info!(cluster = target, from = intent.from, "carrying out a reset");
        let dropped = |why: &str| self.end_reset(target, at, ResetOutcome::Dropped(why.to_string()));
        let state = match self.objects().get(&keys::reset_state_key(target, at)).await? {
            Some(raw) => match ClusterState::decode(&raw) {
                Ok(s) => s,
                Err(_) => return dropped("its state is damaged").await,
            },
            None => return dropped("its state is missing").await,
        };
        let mark = at.to_string();
        let ours = |m: &ShardManifest| m.user_data.get(RESET_MARK) == Some(&mark);
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
            let m = match self.objects().get(&keys::reset_copy_key(target, at, shard)).await? {
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
        // Its own objects (all of `reset/` when its intent was unreadable: whose they were is
        // unknown).
        let dir = if at_ms == 0 { keys::reset_copies_prefix(target) } else { keys::reset_dir(target, at_ms) };
        if let Err(e) = self.objects().delete_prefix(&dir).await {
            tracing::warn!(cluster = target, error = %e, "deleting a finished reset's leftovers failed (garbage collection removes its copies)");
        }
        Ok(())
    }
}
