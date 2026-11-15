//! Pins: commit points kept apart from any shard's history (snapshot contents).
//!
//! A snapshot must keep an index's data as of the moment it was taken while the index goes on:
//! merges replace its segments, deletes write new live-docs generations, the index may be
//! deleted. A pin is a copy of one shard's commit point (segments with their live-docs
//! generations, the checkpoint and user data, no WAL ranges) under the cluster's `p/` prefix.
//! Garbage collection keeps everything a pin names exactly as it does for a retained manifest,
//! so taking a snapshot copies no data (as Elasticsearch's incremental snapshots reuse the files
//! a repository already holds) and deleting one is deleting its pins.
//!
//! Pins are not part of any shard's history: waking a cluster never lists or reads them, so a
//! cluster with many snapshots wakes as fast as one without. They are written once and never
//! rewritten (the collector caches what a key references), branches copy them (a few hundred
//! bytes each; the segments stay shared), and they go with their cluster.

use crate::keys::{self, ShardId};
use crate::manifest::ShardManifest;
use crate::{Store, now_ms};
use anyhow::{Context, bail};
use bytes::Bytes;
use futures::StreamExt;

impl Store {
    /// Pin `m` (a shard's commit point, as [`Store::read_manifest`] or [`Store::write_manifest`]
    /// returned it) as `pin`. Its segments and live docs must be uploaded. WAL ranges and the
    /// branch base are dropped: a pin is exactly its segments. Writing a pin that exists is a
    /// no-op when it names the same segments and an error otherwise. Fails with
    /// [`crate::Fenced`] once a newer owner claimed the cluster. Returns the size of the pin
    /// object (snapshot status reports it as the shard's commit point file).
    pub async fn write_pin(&self, cluster: &str, pin: &ShardId, m: &ShardManifest) -> anyhow::Result<u64> {
        keys::validate_cluster(cluster)?;
        keys::validate_shard(pin)?;
        let epoch = self.fence_check(cluster, self.claimed_epoch(cluster).await?)?;
        let key = keys::pin_key(cluster, pin);
        if let Some(raw) = self.inner.objects.get(&key).await.with_context(|| format!("reading pin {key}"))? {
            let existing = ShardManifest::from_json(&raw).with_context(|| format!("pin {key} is damaged"))?;
            if existing.segments == m.segments {
                return Ok(raw.len() as u64);
            }
            bail!("pin {pin} of cluster [{cluster}] already exists with other contents");
        }
        let mut m = ShardManifest { epoch, written_ms: now_ms(), wal: Vec::new(), wal_head: None, base: None, ..m.clone() };
        m.user_data.retain(|k, _| !k.starts_with(crate::manifest::INSTANCE_DATA_PREFIX));
        let body = Bytes::from(m.to_json());
        let len = body.len() as u64;
        self.inner.objects.put(&key, body).await.with_context(|| format!("writing pin {key}"))?;
        Ok(len)
    }

    /// A pin, if it exists.
    pub async fn read_pin(&self, cluster: &str, pin: &ShardId) -> anyhow::Result<Option<ShardManifest>> {
        keys::validate_cluster(cluster)?;
        keys::validate_shard(pin)?;
        let key = keys::pin_key(cluster, pin);
        match self.inner.objects.get(&key).await.with_context(|| format!("reading pin {key}"))? {
            Some(raw) => Ok(Some(ShardManifest::from_json(&raw).with_context(|| format!("pin {key} is damaged"))?)),
            None => Ok(None),
        }
    }

    /// Every pin of a cluster, sorted.
    pub async fn list_pins(&self, cluster: &str) -> anyhow::Result<Vec<ShardId>> {
        keys::validate_cluster(cluster)?;
        let mut out: Vec<ShardId> = self
            .inner
            .objects
            .list(&keys::pins_prefix(cluster))
            .await?
            .iter()
            .filter_map(|(k, _)| match keys::parse_cluster_key(k) {
                Some(keys::ClusterObject::Pin { cluster: c, name, shard }) if c == cluster => Some(ShardId::new(name, shard)),
                _ => None,
            })
            .collect();
        out.sort_unstable();
        Ok(out)
    }

    /// Deletes pins (missing ones are fine). Their objects go with garbage collection once no
    /// manifest or other pin names them.
    pub async fn delete_pins(&self, cluster: &str, pins: &[ShardId]) -> anyhow::Result<()> {
        keys::validate_cluster(cluster)?;
        for p in pins {
            keys::validate_shard(p)?;
        }
        if pins.is_empty() {
            return Ok(());
        }
        self.fence_check(cluster, self.claimed_epoch(cluster).await?)?;
        let objects = &self.inner.objects;
        // Owned keys: futures borrowing the caller's slice aren't provably `Send` in a spawned
        // request future.
        let doomed: Vec<String> = pins.iter().map(|p| keys::pin_key(cluster, p)).collect();
        let results: Vec<anyhow::Result<()>> = futures::stream::iter(doomed.into_iter().map(|k| async move { objects.delete(&k).await }))
            .buffer_unordered(16)
            .collect()
            .await;
        results.into_iter().collect()
    }

    /// Copies `parent`'s pins to `child` (branching): those written at or before `at_ms` when
    /// given (a pin deleted since is gone for good: the branch then restores that snapshot's
    /// shards empty).
    pub(crate) async fn copy_pins(&self, parent: &str, child: &str, at_ms: Option<u64>) -> anyhow::Result<()> {
        let pins = self.list_pins(parent).await?;
        let objects = &self.inner.objects;
        let results: Vec<anyhow::Result<()>> = futures::stream::iter(pins.into_iter().map(|p| async move {
            let Some(raw) = objects.get(&keys::pin_key(parent, &p)).await? else { return Ok(()) };
            if let Some(at) = at_ms {
                let m = ShardManifest::from_json(&raw).with_context(|| format!("pin {p} of cluster [{parent}] is damaged"))?;
                if m.written_ms > at {
                    return Ok(());
                }
            }
            objects.put(&keys::pin_key(child, &p), raw).await
        }))
        .buffer_unordered(16)
        .collect()
        .await;
        results.into_iter().collect()
    }
}
