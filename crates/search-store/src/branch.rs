//! Branching: a new cluster whose commit points share its parent's objects.
//!
//! A branch copies the parent's cluster state and writes, for every parent shard, a first
//! manifest naming the same segments and live-docs generations. Nothing else is copied: segment
//! and live-docs objects are immutable and keyed uniquely, so when either side later deletes
//! documents it writes a *new* live-docs generation and the other side keeps reading its own.
//! (Copying live docs would buy nothing over sharing them under this scheme.)
//!
//! Operations the parent hasn't flushed yet are not lost: the branch's manifests carry the
//! parent's WAL ranges, frozen at the branch moment and cut at its time (`until_ms`), so the
//! branch's first open replays them into its own segments. A point-in-time branch picks the
//! newest retained parent commit point written at or before the time and replays the parent's
//! WAL from there up to the time; commit points older than retention allows are an error.
//!
//! The branch becomes visible (its cluster state is written) only after all its manifests are,
//! so a crash midway leaves no half-made cluster; a retry clears the leftovers.

use crate::keys::{self, ShardId};
use crate::manifest::{BranchBase, ClusterState, ShardManifest, successor_cap};
use crate::wal::{Listings, WalSource};
use crate::{Store, now_ms};
use anyhow::bail;
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use std::sync::atomic::Ordering::Relaxed;

impl Store {
    /// Create cluster `child` from `parent`'s current state, or its state at `at_ms` (Unix ms).
    /// Flush the parent's shards first when it is loaded (on any node), so the branch starts
    /// from commit points rather than replaying a long WAL; this node's WAL is shipped here.
    pub async fn branch(&self, parent: &str, child: &str, at_ms: Option<u64>) -> anyhow::Result<()> {
        keys::validate_cluster(parent)?;
        keys::validate_cluster(child)?;
        anyhow::ensure!(parent != child, "a cluster can't be branched into itself");
        if self.cluster_exists(child).await? {
            bail!("cluster [{child}] already exists");
        }
        let now = now_ms();
        let cut = at_ms.map_or(now, |t| t.min(now));
        // The parent's recent operations must be readable from object storage by whichever
        // node opens the branch.
        self.wal().sync_remote().await?;

        let history = self.read_cluster_history(parent).await?;
        let state = match at_ms {
            None => history.last().cloned(),
            Some(_) => history.iter().rev().find(|s| s.written_ms <= cut).cloned(),
        };
        let Some(state) = state else {
            if history.is_empty() {
                bail!("no such cluster [{parent}]");
            }
            bail!("cluster [{parent}] has no retained state at or before {cut}");
        };

        // Leftovers of an earlier attempt that died before writing the state.
        self.objects().delete_prefix(&keys::cluster_prefix(child)).await?;
        let cur = self.wal().current_file();
        let epoch = self.epoch(child);
        let listings = Listings::default();
        let point = at_ms.map(|_| cut);
        // Branching from now starts from the latest commit points, read in one go.
        let shards: Vec<(ShardId, Option<ShardManifest>)> = match point {
            None => self.read_latest_manifests(parent).await?.into_iter().map(|(s, m)| (s, Some(m))).collect(),
            // Deleted shards too: the point may be before the deletion.
            Some(_) => self.list_shards_with_history(parent).await?.into_iter().map(|s| (s, None)).collect(),
        };
        let listings = &listings;
        // Shards are independent: write their manifests concurrently (a cluster may have
        // hundreds, and each costs a few object-store round trips).
        futures::stream::iter(shards.into_iter().map(|(shard, latest)| async move {
            let point = match latest {
                Some(m) => {
                    let w = m.wal.clone();
                    Some((m, w))
                }
                None => self.branch_point(parent, &shard, cut, listings).await?,
            };
            let Some((base, sources)) = point else { return anyhow::Ok(()) };
            let mut m = ShardManifest {
                generation: 1,
                epoch,
                written_ms: now,
                wal: sources,
                wal_head: None,
                base: Some(BranchBase { cluster: parent.to_string(), generation: base.generation, at_ms, branched_ms: now }),
                ..base
            };
            for s in &mut m.wal {
                s.until_ms = Some(s.until_ms.map_or(cut, |u| u.min(cut)));
                if s.to.is_none() && s.node == self.node() {
                    // This node's range, live now or from an earlier run (a point-in-time range
                    // can span runs): everything up to `cut` was shipped above, in files before
                    // the current one.
                    s.to = Some(cur - 1);
                }
            }
            self.wal().freeze(&mut m.wal, listings).await?;
            m.wal.retain(|s| s.to.is_none_or(|to| to >= s.from) && s.max_seq_no.is_none_or(|x| x > m.local_checkpoint));
            self.objects().put(&keys::manifest_key(child, &shard, keys::Version::fresh(epoch, 1)), Bytes::from(m.to_json())).await?;
            Ok(())
        }))
        .buffer_unordered(16)
        .try_collect::<Vec<()>>()
        .await?;
        self.write_cluster(child, &ClusterState::new(state.json.clone())).await?;
        self.inner.counters.branches.fetch_add(1, Relaxed);
        Ok(())
    }

    /// The parent commit point a point-in-time branch of `shard` starts from (the newest at or
    /// before `at`) and the WAL ranges that may hold the parent's operations after it. `None`
    /// when the shard didn't exist yet at `at`, or was already deleted.
    async fn branch_point(&self, parent: &str, shard: &ShardId, at: u64, listings: &Listings) -> anyhow::Result<Option<(ShardManifest, Vec<WalSource>)>> {
        let history = self.read_history(parent, shard).await?;
        let Some(i) = history.iter().rposition(|(m, _)| m.written_ms <= at) else {
            return match history.first() {
                // Its very first commit point (written when the shard was created) is later.
                Some((first, _)) if first.generation == 1 => Ok(None),
                Some(_) => bail!("shard {shard} of cluster [{parent}] has no retained commit point at or before {at}"),
                None => Ok(None),
            };
        };
        if history[i].1 {
            // Deleted at or before `at`.
            return Ok(None);
        }
        let all: Vec<ShardManifest> = history.into_iter().map(|(m, _)| m).collect();
        // Its WAL is kept for point-in-time use only while its successor is inside the window.
        let window = self.config().manifest_retain_ms;
        if let Some(next) = all.get(i + 1)
            && next.written_ms.saturating_add(window) <= now_ms()
        {
            bail!("point in time {at} is older than the {window} ms point-in-time window of shard {shard} of cluster [{parent}]");
        }
        // Operations between that commit point and `at` may have been written on another node
        // (the shard moved): later commit points name those ranges, so take them all. A range
        // that was still open when its commit point was written ends where the next commit
        // point says (see `successor_cap`), so files the shard's writers never used in between
        // (a crashed owner's late uploads among them) stay out.
        let mut ranges = Vec::new();
        for (k, m) in all[i..].iter().enumerate() {
            let next = all.get(i + k + 1);
            for s in &m.wal {
                let mut s = s.clone();
                if s.to.is_none() {
                    s.to = next.and_then(|n| successor_cap(&n.wal, n.wal_head.as_ref(), &s.node));
                }
                ranges.push(s);
            }
        }
        // Freeze what is still open before merging: each range as written belongs to one run of
        // its node, which is what ending it at the node's next run relies on, while a merged
        // range may span runs (a node that restarted in between) and would lose the later ones.
        self.wal().freeze(&mut ranges, listings).await?;
        Ok(Some((all[i].clone(), merge_sources(ranges))))
    }
}

/// Turn the WAL ranges of successive commit points (in history order) into a list replay can
/// apply in order, reading every file once. A node's files are written in order, so a range
/// that repeats files an earlier range of the same node already covers keeps only its new
/// files; a range continuing the one just before it (same node, no other node's range in
/// between) is folded into it. Ranges are never joined across another node's range, or across
/// a gap of files the shard's writers didn't use: applying them in history order is what makes
/// a seq_no reused after a crash resolve to its latest operation.
fn merge_sources(sources: Vec<WalSource>) -> Vec<WalSource> {
    let mut out: Vec<WalSource> = Vec::new();
    for s in sources {
        let mut s = WalSource { max_seq_no: None, ..s };
        // Files of this node an earlier range already reads.
        let covered = out.iter().filter(|o| o.node == s.node && o.cluster == s.cluster).map(|o| o.to).max_by(|a, b| match (a, b) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, _) => std::cmp::Ordering::Greater,
            (_, None) => std::cmp::Ordering::Less,
            (Some(a), Some(b)) => a.cmp(b),
        });
        match covered {
            Some(None) => continue,
            Some(Some(c)) if s.to.is_some_and(|to| to <= c) => continue,
            Some(Some(c)) => s.from = s.from.max(c + 1),
            None => {}
        }
        if let Some(o) = out.last_mut()
            && o.node == s.node
            && o.cluster == s.cluster
            && o.to.is_some_and(|to| s.from <= to.saturating_add(1))
        {
            o.to = s.to;
            o.exact &= s.exact;
            o.until_ms = match (o.until_ms, s.until_ms) {
                (Some(a), Some(b)) => Some(a.max(b)),
                _ => None,
            };
            // Each skip run lies inside the range that froze it, which no other range of the
            // same node overlaps with a different view: the union is every view.
            for r in s.skip {
                if !o.skip.contains(&r) {
                    o.skip.push(r);
                }
            }
            continue;
        }
        out.push(s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(node: &str, from: u64, to: Option<u64>) -> WalSource {
        WalSource { to, max_seq_no: Some(3), ..WalSource::open(node, "p", from) }
    }

    #[test]
    fn merging_reads_each_file_once_in_history_order() {
        let got = |m: &[WalSource]| m.iter().map(|s| (s.node.clone(), s.from, s.to)).collect::<Vec<_>>();
        let r = |n: &str, f: u64, t: Option<u64>| (n.to_string(), f, t);
        // Successive commit points of one writer: overlapping and touching ranges fold.
        let m = merge_sources(vec![src("a", 3, Some(7)), src("a", 5, Some(9)), src("a", 10, Some(12)), src("a", 20, Some(30))]);
        assert_eq!(got(&m), vec![r("a", 3, Some(12)), r("a", 20, Some(30))], "files 13..=19 aren't the shard's");
        assert!(m.iter().all(|s| s.max_seq_no.is_none()));
        // The shard moved a -> b -> a: three ranges in that order, nothing read twice.
        let m = merge_sources(vec![src("a", 11, Some(22)), src("a", 22, Some(22)), src("b", 7, Some(7)), src("a", 20, Some(25)), src("a", 26, None)]);
        assert_eq!(got(&m), vec![r("a", 11, Some(22)), r("b", 7, Some(7)), r("a", 23, None)]);
        // A range inside one already read disappears; an open earlier range covers the rest.
        let m = merge_sources(vec![src("a", 5, None), src("b", 1, Some(2)), src("a", 9, Some(12))]);
        assert_eq!(got(&m), vec![r("a", 5, None), r("b", 1, Some(2))]);
        let skipped = WalSource { skip: vec![[6, 6]], ..src("a", 5, Some(9)) };
        let m = merge_sources(vec![skipped.clone(), WalSource { skip: vec![[25, 26]], ..src("a", 9, Some(30)) }, skipped]);
        assert_eq!(got(&m), vec![r("a", 5, Some(30))]);
        assert_eq!(m[0].skip, vec![[6, 6], [25, 26]]);
        assert!(m[0].skips(6) && m[0].skips(26) && !m[0].skips(7));
        // Folded ranges are exact only if every part was.
        let exact = |s: WalSource| WalSource { exact: true, ..s };
        let m = merge_sources(vec![exact(src("a", 1, Some(4))), exact(src("a", 5, Some(9)))]);
        assert!(m.len() == 1 && m[0].exact);
        let m = merge_sources(vec![exact(src("a", 1, Some(4))), src("a", 5, Some(9))]);
        assert!(m.len() == 1 && !m[0].exact);
        let m = merge_sources(vec![src("a", 5, Some(9)), WalSource { cluster: "q".into(), ..src("a", 5, Some(9)) }]);
        assert_eq!(m.len(), 2, "another cluster's records are another range");
    }
}
