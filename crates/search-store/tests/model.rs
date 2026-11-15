//! Randomized lifecycle test against a model: shards move between nodes, nodes crash and
//! restart, shards flush, detach and recover, and garbage collection runs with no grace period
//! between steps. After every recovery the shard's operations (segment contents plus WAL
//! replay) must hold every acknowledged operation with its exact payload, nothing that was
//! never appended, and no gap in seq_nos.
//!
//! With `sync_objects` an acknowledged operation is in object storage, so a shard may recover on
//! any node after a crash. Without it, acknowledged operations are only on the crashed node's
//! disk until shipped, so a crashed node's shards recover on that node (as a restarted
//! Elasticsearch node replays its own translog) and move only after a clean detach.

mod common;

use common::*;
use search_store::{ClusterState, SegmentId, SegmentRef, ShardId, ShardManifest, Store, StoreConfig, WalRecord, WalShard, test_objects};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

struct Rng(u64);

impl Rng {
    /// Scrambled (splitmix64) so nearby seeds give unrelated runs.
    fn new(seed: u64) -> Rng {
        let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        Rng((z ^ (z >> 31)) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// A shard open on a node: its engine state is just the operations it holds.
struct Loaded {
    node: usize,
    wal: WalShard,
    ops: BTreeMap<i64, Vec<u8>>,
    manifest: ShardManifest,
}

struct ShardModel {
    id: ShardId,
    acked: BTreeMap<i64, Vec<u8>>,
    ever: HashSet<(i64, Vec<u8>)>,
    loaded: Option<Loaded>,
    /// The node it must recover on (it crashed there with acknowledged operations not shipped).
    pinned_to: Option<usize>,
    incarnation: u64,
}

const CLUSTER: &str = "model";

fn encode_ops(ops: &BTreeMap<i64, Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    for (seq, p) in ops {
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&(p.len() as u32).to_le_bytes());
        out.extend_from_slice(p);
    }
    out
}

fn decode_ops(mut b: &[u8]) -> BTreeMap<i64, Vec<u8>> {
    let mut out = BTreeMap::new();
    while !b.is_empty() {
        let seq = i64::from_le_bytes(b[..8].try_into().unwrap());
        let len = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
        out.insert(seq, b[12..12 + len].to_vec());
        b = &b[12 + len..];
    }
    out
}

fn config(node: usize, sync: bool, pit: bool) -> StoreConfig {
    let mut cfg = fast_config(&format!("m{node}"));
    cfg.wal.sync_objects = sync;
    if pit {
        cfg.manifest_retain_ms = 3_600_000;
    }
    cfg.wal.file_bytes = 3000;
    cfg.manifest_retain = 2;
    cfg
}

struct World {
    objects: search_store::Objects,
    dirs: Vec<PathBuf>,
    nodes: Vec<Option<Store>>,
    sync: bool,
    /// Point-in-time branches: snapshots of what each shard held at a moment.
    pit: bool,
    snapshots: Vec<Snapshot>,
    rng: Rng,
    shards: Vec<ShardModel>,
    tally: Tally,
}

type Ops = BTreeMap<i64, Vec<u8>>;
type Appended = HashSet<(i64, Vec<u8>)>;

/// Each shard's acknowledged and ever-appended operations at `at_ms`.
struct Snapshot {
    at_ms: u64,
    shards: Vec<(Ops, Appended)>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

/// What a run did, so a run that silently did nothing fails.
#[derive(Debug, Default)]
struct Tally {
    recoveries: u64,
    moves: u64,
    replayed: u64,
    crashes: u64,
    branches: u64,
    pit_branches: u64,
    gc_deleted: usize,
}

impl World {
    async fn node(&mut self, n: usize) -> Store {
        if self.nodes[n].is_none() {
            self.nodes[n] = Some(open(&self.objects, &self.dirs[n], config(n, self.sync, self.pit)).await);
        }
        self.nodes[n].clone().unwrap()
    }

    async fn append(&mut self, i: usize, wait: bool) {
        let n = 1 + self.rng.below(12) as i64;
        let tag = self.rng.next();
        let sm = &mut self.shards[i];
        let Some(l) = sm.loaded.as_mut() else { return };
        let next = l.ops.keys().next_back().map_or(0, |s| s + 1);
        let ops: Vec<(i64, Vec<u8>)> =
            (next..next + n).map(|seq| (seq, format!("{}:{seq}:{}:{tag:x}", sm.id.index_uuid, sm.incarnation).into_bytes())).collect();
        let lsn = l.wal.append_many(&ops).unwrap();
        for (s, p) in &ops {
            sm.ever.insert((*s, p.clone()));
            l.ops.insert(*s, p.clone());
        }
        if wait {
            let store = self.nodes[l.node].clone().unwrap();
            store.wal().wait_ack(lsn).await.unwrap();
            sm.acked.extend(ops);
        }
    }

    async fn flush(&mut self, i: usize) {
        let sm = &mut self.shards[i];
        let Some(l) = sm.loaded.as_mut() else { return };
        let store = self.nodes[l.node].clone().unwrap();
        let mut segments = Vec::new();
        if !l.ops.is_empty() {
            let id = SegmentId::random();
            let data = encode_ops(&l.ops);
            std::fs::write(store.segment_path(&id), &data).unwrap();
            store.upload_segment(&store.segment_path(&id)).await.unwrap();
            segments.push(SegmentRef { id, bytes: data.len() as u64, max_doc: l.ops.len() as u32, del_count: 0, live: None });
        }
        let max = l.ops.keys().next_back().copied().unwrap_or(-1);
        let m = ShardManifest { segments, max_seq_no: max, local_checkpoint: max, ..l.manifest.clone() };
        l.manifest = store.write_manifest(CLUSTER, &sm.id, &m).await.unwrap();
    }

    async fn detach(&mut self, i: usize, flush: bool) {
        if flush {
            self.flush(i).await;
        }
        let sm = &mut self.shards[i];
        let Some(l) = sm.loaded.take() else { return };
        let store = self.nodes[l.node].clone().unwrap();
        let unflushed = l.wal.stats().uncommitted_ops > 0;
        store.detach_shard(l.wal).await.unwrap();
        if unflushed && !self.sync {
            // Detaching shipped them: they can be replayed anywhere now.
            assert!(store.wal().remote_lsn() >= store.wal().last_lsn());
        }
    }

    async fn crash(&mut self, n: usize) {
        let Some(store) = self.nodes[n].take() else { return };
        self.tally.crashes += 1;
        store.wal().abandon().await;
        for sm in &mut self.shards {
            if sm.loaded.as_ref().is_some_and(|l| l.node == n) {
                sm.loaded = None;
                if !self.sync {
                    sm.pinned_to = Some(n);
                }
            }
        }
    }

    async fn close(&mut self, n: usize) {
        for i in 0..self.shards.len() {
            if self.shards[i].loaded.as_ref().is_some_and(|l| l.node == n) {
                let flush = self.rng.chance(50);
                self.detach(i, flush).await;
            }
        }
        if let Some(store) = self.nodes[n].take() {
            store.close().await.unwrap();
        }
    }

    async fn recover(&mut self, i: usize, n: usize) {
        let n = self.shards[i].pinned_to.unwrap_or(n);
        if self.shards[i].loaded.is_some() {
            return;
        }
        let store = self.node(n).await;
        let sm = &mut self.shards[i];
        let mut rec = store.recover_shard(CLUSTER, &sm.id).await.unwrap();
        let mut ops = BTreeMap::new();
        if let Some(m) = &rec.manifest {
            for s in &m.segments {
                let data = store.open_segment(&s.id).await.unwrap();
                ops.extend(decode_ops(&data));
            }
            assert!(ops.keys().next_back().is_none_or(|&max| max == m.max_seq_no), "segments end at the commit point");
        }
        let mut replayed = Vec::new();
        store
            .replay(&mut rec, &mut |r: &WalRecord<'_>| {
                assert_eq!((r.cluster, r.index_uuid, r.shard), (CLUSTER, sm.id.index_uuid.as_str(), sm.id.shard));
                replayed.push((r.seq_no, r.payload.to_vec()));
                Ok(())
            })
            .await
            .unwrap();
        // Later appends of a seq_no win (an unacknowledged op lost in a crash is reused).
        for (s, p) in replayed {
            ops.insert(s, p);
        }
        for (s, p) in &sm.acked {
            assert_eq!(ops.get(s), Some(p), "{}: acknowledged op {s} lost or changed after recovering on node {n}", sm.id);
        }
        for (s, p) in &ops {
            assert!(sm.ever.contains(&(*s, p.clone())), "{}: op {s} was never appended", sm.id);
        }
        if let Some(&max) = ops.keys().next_back() {
            assert_eq!(ops.len() as i64, max + 1, "{}: gap in seq_nos {:?}", sm.id, ops.keys().collect::<Vec<_>>());
        }
        self.tally.recoveries += 1;
        self.tally.replayed += rec.stats.records;
        if rec.manifest.as_ref().and_then(|m| m.wal_head.as_ref()).is_some_and(|h| h.node != store.node()) {
            self.tally.moves += 1;
        }
        let (wal, manifest) = store.attach_shard(rec).await.unwrap();
        // Unacknowledged operations lost in a crash get their seq_nos reused, with payloads
        // of a new incarnation.
        sm.incarnation += 1;
        sm.pinned_to = None;
        sm.loaded = Some(Loaded { node: n, wal, ops, manifest });
    }

    /// Branch the cluster, check that each child shard starts from the parent's operations,
    /// write to the child, delete it, and collect garbage: the parent must not notice (its
    /// later recoveries check that nothing of the child leaks in and nothing of its own went).
    /// Remember what every shard holds now, on a millisecond no operation shares.
    async fn snapshot(&mut self) {
        tokio::time::sleep(Duration::from_millis(3)).await;
        let at_ms = now_ms();
        tokio::time::sleep(Duration::from_millis(3)).await;
        let shards = self.shards.iter().map(|s| (s.acked.clone(), s.ever.clone())).collect();
        self.snapshots.push(Snapshot { at_ms, shards });
    }

    async fn branch_check(&mut self) {
        // Only the node writing the parent ships its WAL tail when branching: unload the
        // parent's shards everywhere else first (as search-server flushes before branching).
        let first = self.shards.iter().find_map(|s| s.loaded.as_ref().map(|l| l.node));
        let n = first.unwrap_or_else(|| self.rng.below(3) as usize);
        for i in 0..self.shards.len() {
            if self.shards[i].loaded.as_ref().is_some_and(|l| l.node != n) {
                let flush = self.rng.chance(50);
                self.detach(i, flush).await;
            }
        }
        let store = self.node(n).await;
        self.tally.branches += 1;
        let child = format!("{CLUSTER}-b{}", self.tally.branches);
        // A point-in-time branch half the time when there are snapshots.
        let at = if self.snapshots.is_empty() || self.rng.chance(30) { None } else { Some(self.rng.below(self.snapshots.len() as u64) as usize) };
        store.branch(CLUSTER, &child, at.map(|k| self.snapshots[k].at_ms)).await.unwrap();
        if at.is_some() {
            self.tally.pit_branches += 1;
        }
        for i in 0..self.shards.len() {
            let m = self.rng.below(3) as usize;
            let m = if !self.sync && self.shards[i].pinned_to.is_some() { self.shards[i].pinned_to.unwrap() } else { m };
            let other = self.node(m).await;
            let sm = &self.shards[i];
            let mut rec = other.recover_shard(&child, &sm.id).await.unwrap();
            let mut ops = BTreeMap::new();
            for seg in rec.manifest.iter().flat_map(|m| m.segments.iter()) {
                ops.extend(decode_ops(&other.open_segment(&seg.id).await.unwrap()));
            }
            let mut replayed = Vec::new();
            other
                .replay(&mut rec, &mut |r: &WalRecord<'_>| {
                    assert_eq!(r.cluster, CLUSTER, "a branch replays its parent's records");
                    replayed.push((r.seq_no, r.payload.to_vec()));
                    Ok(())
                })
                .await
                .unwrap();
            ops.extend(replayed);
            match (&sm.loaded, at) {
                (_, Some(k)) => {
                    let (acked, ever) = &self.snapshots[k].shards[i];
                    if self.sync && acked.iter().any(|(s, p)| ops.get(s) != Some(p)) {
                        eprintln!("PIT at {} for {}", self.snapshots[k].at_ms, sm.id);
                        for m in other.read_manifests(CLUSTER, &sm.id).await.unwrap() {
                            eprintln!("  parent gen {} written {} ckpt {} head {:?} segs {} wal {:?}", m.generation, m.written_ms, m.local_checkpoint, m.wal_head, m.segments.len(), m.wal);
                        }
                        let cm = other.read_manifest(&child, &sm.id).await.unwrap();
                        eprintln!("  child {:?}", cm.map(|m| (m.base, m.local_checkpoint, m.wal)));
                        eprintln!("  child ops {:?}", ops.keys().collect::<Vec<_>>());
                    }
                    if self.sync {
                        for (s, p) in acked {
                            assert_eq!(ops.get(s), Some(p), "{}: point-in-time branch lost acknowledged op {s}", sm.id);
                        }
                    }
                    for (s, p) in &ops {
                        assert!(ever.contains(&(*s, p.clone())), "{}: point-in-time branch has op {s}, appended after the time", sm.id);
                    }
                }
                (Some(l), None) => assert_eq!(ops, l.ops, "{}: the branch starts from the parent's operations", sm.id),
                (None, None) => {
                    if sm.pinned_to.is_none() {
                        for (s, p) in &sm.acked {
                            assert_eq!(ops.get(s), Some(p), "{}: branch lost acknowledged op {s}", sm.id);
                        }
                    }
                    for (s, p) in &ops {
                        assert!(sm.ever.contains(&(*s, p.clone())), "{}: branch has op {s} never appended", sm.id);
                    }
                }
            }
            if let Some(&max) = ops.keys().next_back() {
                assert_eq!(ops.len() as i64, max + 1, "{}: gap in the branch's seq_nos", sm.id);
            }
            let (wal, base) = other.attach_shard(rec).await.unwrap();
            // Child writes: reuse the parent's next seq_nos with different payloads.
            let next = ops.keys().next_back().map_or(0, |s| s + 1);
            let child_ops: Vec<(i64, Vec<u8>)> = (next..next + 5).map(|q| (q, format!("child:{q}").into_bytes())).collect();
            let lsn = wal.append_many(&child_ops).unwrap();
            other.wal().wait_ack(lsn).await.unwrap();
            if self.rng.chance(50) {
                ops.extend(child_ops);
                let id = SegmentId::random();
                let data = encode_ops(&ops);
                std::fs::write(other.segment_path(&id), &data).unwrap();
                other.upload_segment(&other.segment_path(&id)).await.unwrap();
                let max = *ops.keys().next_back().unwrap();
                let seg = SegmentRef { id, bytes: data.len() as u64, max_doc: ops.len() as u32, del_count: 0, live: None };
                other.write_manifest(&child, &sm.id, &ShardManifest { segments: vec![seg], max_seq_no: max, local_checkpoint: max, ..base }).await.unwrap();
            }
            other.detach_shard(wal).await.unwrap();
        }
        let k = self.rng.below(3) as usize;
        let killer = self.node(k).await;
        killer.delete_cluster(&child).await.unwrap();
        assert!(!killer.cluster_exists(&child).await.unwrap());
        for s in self.nodes.iter().flatten() {
            s.release_cluster(&child);
        }
        self.gc().await;
    }

    async fn gc(&mut self) {
        let open: Vec<usize> = (0..self.nodes.len()).filter(|&n| self.nodes[n].is_some()).collect();
        if open.is_empty() {
            return;
        }
        let n = open[self.rng.below(open.len() as u64) as usize];
        let store = self.nodes[n].clone().unwrap();
        // Ship what nodes have locally first: GC is only safe for objects older than the
        // grace period, and with no grace that means nothing may be in flight.
        for s in self.nodes.iter().flatten() {
            s.wal().sync_remote().await.unwrap();
        }
        self.tally.gc_deleted += store.collect_garbage(Duration::ZERO).await.unwrap().deleted;
    }
}

async fn run_model(sync: bool, pit: bool, seed: u64, steps: usize) {
    let name = match (sync, pit) {
        (true, true) => "model-pit",
        (true, false) => "model-sync",
        _ => "model-local",
    };
    eprintln!("{name}: seed {seed:#x}");
    let objects = test_objects(name);
    let base = temp_dir(name);
    let mut w = World {
        objects,
        dirs: (0..3).map(|n| base.join(format!("node{n}"))).collect(),
        nodes: vec![None, None, None],
        sync,
        pit,
        snapshots: Vec::new(),
        rng: Rng::new(seed),
        tally: Tally::default(),
        shards: (0..3)
            .map(|s| ShardModel { id: shard(&format!("idxM{s}"), s), acked: BTreeMap::new(), ever: HashSet::new(), loaded: None, pinned_to: None, incarnation: 0 })
            .collect(),
    };
    w.node(0).await.write_cluster(CLUSTER, &ClusterState::new("{\"model\":true}")).await.unwrap();
    for step in 0..steps {
        let i = w.rng.below(w.shards.len() as u64) as usize;
        let n = w.rng.below(3) as usize;
        match w.rng.below(100) {
            0..=34 => {
                if w.shards[i].loaded.is_none() {
                    w.recover(i, n).await;
                }
                let wait = w.rng.chance(85);
                w.append(i, wait).await;
            }
            35..=49 => w.flush(i).await,
            50..=59 => {
                let flush = w.rng.chance(50);
                w.detach(i, flush).await;
            }
            60..=74 => w.recover(i, n).await,
            75..=82 => w.crash(n).await,
            83..=89 => w.close(n).await,
            90..=94 => w.gc().await,
            95..=96 if pit => w.snapshot().await,
            _ => w.branch_check().await,
        }
        if pit && step % 50 == 10 {
            w.snapshot().await;
        }
        if pit && step % 50 == 40 {
            w.branch_check().await;
        }
        if step % 50 == 49 {
            // Everything recovers somewhere and checks out.
            for i in 0..w.shards.len() {
                let n = w.rng.below(3) as usize;
                w.recover(i, n).await;
            }
        }
    }
    // A final crash of everything, then recovery of every shard.
    for n in 0..3 {
        w.crash(n).await;
    }
    for i in 0..w.shards.len() {
        let n = w.rng.below(3) as usize;
        w.recover(i, n).await;
        assert!(w.shards[i].loaded.is_some());
    }
    for n in 0..3 {
        w.close(n).await;
    }
    // Runs add up (many seeds in one process): drop this one's data.
    if let Some(dir) = w.objects.description.strip_prefix("file://") {
        let _ = std::fs::remove_dir_all(dir);
    }
    let _ = std::fs::remove_dir_all(&base);
    let t = &w.tally;
    println!("{name} seed {seed:x}: {t:?}, acked {:?}", w.shards.iter().map(|s| s.acked.len()).collect::<Vec<_>>());
    assert!(t.recoveries > 10 && t.moves > 2 && t.replayed > 0 && t.crashes > 2 && t.branches > 0 && t.gc_deleted > 0, "{t:?}");
    assert!(!pit || t.pit_branches > 0, "{t:?}");
}

/// Fixed seeds, plus `ROLL_MODEL_RUNS` random ones (printed, to reproduce a failure).
fn seeds(fixed: &[u64]) -> Vec<u64> {
    if let Some(seed) = std::env::var("ROLL_MODEL_SEED").ok().and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok()) {
        return vec![seed];
    }
    let mut out = fixed.to_vec();
    let extra: u64 = std::env::var("ROLL_MODEL_RUNS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut r = Rng::new(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64);
    out.extend((0..extra).map(|_| r.next()));
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_model_with_sync_objects() {
    for seed in seeds(&[0x5eed_0001, 0xdead_beef, 0x1234_5678_9abc]) {
        run_model(true, false, seed, 300).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_model_with_local_acks() {
    for seed in seeds(&[0x5eed_0002, 0xfeed_f00d, 0x0bad_cafe]) {
        run_model(false, false, seed, 300).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_model_with_point_in_time_branches() {
    for seed in seeds(&[0x5eed_0003, 0x7177_0001, 0xabcd_ef01]) {
        run_model(true, true, seed, 300).await;
    }
}
