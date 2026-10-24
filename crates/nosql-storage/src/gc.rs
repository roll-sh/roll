//! Garbage collection of objects no manifest references, and expiry of manifest history.
//!
//! References come from every current manifest, every retained version, every named snapshot
//! (and so every branch, restore archive and base log they hold). A node's segments after a
//! manifest's `after_seq` stay alive too (frames a lazily written manifest doesn't list yet).
//! Deletion is two-phase, as in the Redis product: an object must be found unreferenced by
//! passes at least `min_age` apart, so references appearing during a pass (an SSTable uploaded
//! before its manifest, a branch being created) keep it.

use crate::manifest::{self, Manifest};
use anyhow::{Context, Result};
use bytes::Bytes;
use futures::StreamExt;
use pg_storage::Objects;
use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

const GC_STATE: &str = "nosql/gc/pending.json";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GcStats {
    pub manifests: usize,
    /// Unreferenced objects found (deleted once they've stayed so for `min_age`).
    pub unreferenced: usize,
    pub deleted: usize,
    pub deleted_bytes: u64,
}

fn is_manifest(key: &str) -> bool {
    key.ends_with("/manifest.json") || ((key.contains("/manifests/") || key.contains("/snapshots/")) && key.ends_with(".json"))
}

/// Delete segments and SSTables that nothing references. Run from one place per bucket.
pub async fn collect_garbage(objects: &Objects, min_age: Duration, now_ms: i64) -> Result<GcStats> {
    // Candidates first: anything a manifest written after this refers to is either listed
    // here and marked below, or newer and not a candidate.
    let mut candidates = objects.list("nosql/log/").await?;
    let db_objects = objects.list("nosql/db/").await?;
    candidates.extend(db_objects.iter().filter(|(k, _)| k.contains("/sst/")).cloned());
    let mut pending: HashMap<String, i64> = match objects.get(GC_STATE).await? {
        Some(raw) => serde_json::from_slice(&raw).unwrap_or_default(),
        None => HashMap::new(),
    };
    let mut st = GcStats::default();
    let mut keep = BTreeSet::new();
    let mut after: HashMap<String, u64> = HashMap::new();
    // Manifests listed after the candidates (and re-listed, in case of new ones).
    for (key, _) in objects.list("nosql/db/").await? {
        if !is_manifest(&key) {
            continue;
        }
        let Some(raw) = objects.get(&key).await? else { continue };
        let m: Manifest = serde_json::from_slice(&raw).with_context(|| format!("corrupt manifest {key}"))?;
        m.referenced(&mut keep);
        if let Some(node) = &m.node {
            let a = after.entry(node.clone()).or_insert(u64::MAX);
            *a = (*a).min(m.after_seq);
        }
        st.manifests += 1;
    }
    let live = |key: &str| {
        keep.contains(key)
            || key.strip_prefix("nosql/log/").and_then(|k| k.split_once('/')).is_some_and(|(node, _)| {
                after.get(node).is_some_and(|a| manifest::segment_seq(key).is_some_and(|s| s > *a))
            })
    };
    let cutoff = now_ms - min_age.as_millis() as i64;
    let mut next = HashMap::new();
    let mut doomed = Vec::new();
    for (key, size) in candidates {
        if live(&key) {
            continue;
        }
        st.unreferenced += 1;
        let since = pending.remove(&key).unwrap_or(now_ms);
        if since <= cutoff {
            doomed.push((key, size));
        } else {
            next.insert(key, since);
        }
    }
    let results = futures::stream::iter(doomed.into_iter().map(|(key, size)| async move { objects.delete(&key).await.map(|()| size) })).buffer_unordered(16).collect::<Vec<_>>().await;
    for r in results {
        st.deleted_bytes += r?;
        st.deleted += 1;
    }
    objects.put(GC_STATE, Bytes::from(serde_json::to_vec(&next)?)).await?;
    Ok(st)
}

/// Delete a database's manifest versions written before `horizon_ms`, except the newest of
/// them. Returns how many were deleted and the oldest kept version's smallest flushed position
/// (frames above it may still be replayed by a restore).
pub async fn expire_versions(objects: &Objects, db: &str, horizon_ms: i64) -> Result<(usize, Option<u64>)> {
    let versions = manifest::list_versions(objects, db).await?;
    if versions.is_empty() {
        return Ok((0, None));
    }
    // First version written at or after the horizon (versions are written in order).
    let (mut lo, mut hi) = (0usize, versions.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        match manifest::read_json(objects, &versions[mid].1).await? {
            Some(m) if m.written_ms >= horizon_ms => hi = mid,
            _ => lo = mid + 1,
        }
    }
    let keep_from = lo.saturating_sub(1);
    let mut deleted = 0;
    for (_, key) in &versions[..keep_from] {
        objects.delete(key).await?;
        deleted += 1;
    }
    let floor = manifest::read_json(objects, &versions[keep_from].1).await?.map(|m| m.min_flushed());
    Ok((deleted, floor))
}

/// Delete a database's manifests (current, versions, snapshots). Its objects become
/// collectable once no branch references them.
pub async fn delete_database(objects: &Objects, db: &str) -> Result<usize> {
    let mut n = 0;
    for (key, _) in objects.list(&format!("nosql/db/{db}/")).await? {
        if is_manifest(&key) {
            objects.delete(&key).await?;
            n += 1;
        }
    }
    Ok(n)
}
