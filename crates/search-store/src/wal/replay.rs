//! Replaying a shard's operations from WAL sources.
//!
//! A shard manifest lists the WAL ranges ([`WalSource`]) that may hold operations newer than its
//! commit point: the node that writes the shard now, and nodes (or earlier runs of this node)
//! that wrote it before a crash moved it. Replay reads each range in file order, from the local
//! file when this node still has it (fast restarts) and from `search/wal/<node>/<seq>.zst`
//! otherwise, and hands the shard's records with `seq_no` past the checkpoint to a callback.
//!
//! Loading is a pipeline: up to [`PREFETCH`] files are fetched ahead of the one being handed
//! over, and each object's block for this shard is decompressed on the blocking pool as soon as
//! it arrives, so neither object-store round trips nor zstd stall the async workers or the
//! callback. Only this shard's block of an object is ever downloaded and decompressed (see
//! [`super::object`]): a suffix read of [`TAIL`] bytes gets small objects whole and large ones'
//! directories, and a ranged read gets the block. Recovering one tenant from a node whose WAL
//! files are shared with a thousand others costs what that tenant wrote, not what the node did.
//!
//! Ranges that are still open-ended but no longer written (another node, an earlier run) are
//! first frozen at the newest file that exists, or at the end of the run that wrote them when a
//! later run of the node has started (its marker, `search/wal/<node>/<first file>.run`, is in
//! the same listing): a crashed run's range never reaches into files of the node's later runs.
//! That fixes the shard's history before this node starts appending to it again. Freezing
//! another node's range also records which files of the range were missing
//! ([`WalSource::skip`]): a deposed owner finishing a slow upload later must not change what a
//! later replay of the same range sees, and any *other* file of the range that is missing at a
//! later replay was lost ([`WalSource::exact`]), which fails the replay instead of silently
//! leaving a hole in the shard's history.

use super::object::{Entry, TRAILER, Trailer, WalObject};
use super::record::{self, Records, WalRecord};
use super::{Wal, WalSource};
use crate::keys::{self, ShardId};
use futures::StreamExt;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

/// Files fetched ahead of the one being decoded (cold-start latency is mostly GET round trips).
const PREFETCH: usize = 4;
/// Compressed blocks at least this big are inflated on the blocking pool.
const INLINE_INFLATE: u32 = 64 << 10;
/// The first read of a WAL object takes this much of its end: all of a small object (files
/// sealed by age under light load are small), else at least the trailer and usually the
/// directory.
pub(crate) const TAIL: u64 = 64 << 10;

/// What a replay did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct ReplayStats {
    /// Records handed to the callback.
    pub records: u64,
    pub bytes: u64,
    /// WAL files read.
    pub files: u64,
    /// Highest seq_no replayed (`after_seq_no` when nothing was).
    pub max_seq_no: i64,
}

enum Loaded {
    /// A local file's intact bytes (header included), memory-mapped: concurrent replays after
    /// a restart share the page cache instead of each holding copies of 64 MiB files.
    Raw(memmap2::Mmap),
    /// This shard's records from a shipped object (if it has any past the checkpoint), and
    /// whether the object holds anything appended after the source's cut.
    Block { records: Option<Vec<u8>>, past_cut: bool },
}

/// Listings of nodes' WAL objects, shared by the freezes and replays of one operation (a
/// branch freezes every shard's ranges against the same listing).
#[derive(Default)]
pub(crate) struct Listings(parking_lot::Mutex<HashMap<String, Arc<NodeListing>>>);

/// One node's WAL objects and run markers, both sorted.
#[derive(Default)]
pub(crate) struct NodeListing {
    pub(crate) files: Vec<u64>,
    pub(crate) runs: Vec<u64>,
}

impl NodeListing {
    /// The last file the run that wrote file `from` can have written, once a later run started.
    pub(crate) fn run_end(&self, from: u64) -> Option<u64> {
        run_end(&self.runs, from)
    }
}

/// The last file of the run holding `from`, given the sorted first files of a node's runs: one
/// before the next run's first file (`None` while no later run is known).
pub(crate) fn run_end(runs: &[u64], from: u64) -> Option<u64> {
    let i = runs.partition_point(|&r| r <= from);
    runs.get(i).map(|&next| next - 1)
}

/// Which shard and which part of its history a load is for.
struct Want {
    cluster: String,
    shard: ShardId,
    after_seq_no: i64,
    until_ms: Option<u64>,
}

/// Runs of consecutive numbers in `from..=to` that aren't in `present` (sorted).
fn gaps(from: u64, to: u64, present: &[u64]) -> Vec<[u64; 2]> {
    let mut out = Vec::new();
    let mut next = from;
    for &p in present.iter().filter(|&&p| (from..=to).contains(&p)) {
        if p > next {
            out.push([next, p - 1]);
        }
        next = p + 1;
    }
    if next <= to {
        out.push([next, to]);
    }
    out
}

/// An exact range must have every file it doesn't skip (`present`: those it has).
fn check_complete(src: &WalSource, shard: &ShardId, present: &BTreeSet<u64>) -> anyhow::Result<()> {
    let to = src.to.unwrap_or(src.from);
    let mut skip = src.skip.clone();
    skip.sort_unstable();
    let present: Vec<u64> = present.iter().copied().collect();
    let missing: Vec<[u64; 2]> = gaps(src.from, to, &present).into_iter().flat_map(|[a, b]| subtract(a, b, &skip)).collect();
    if missing.is_empty() {
        return Ok(());
    }
    let list = missing.iter().map(|[a, b]| if a == b { a.to_string() } else { format!("{a}..={b}") }).collect::<Vec<_>>().join(", ");
    anyhow::bail!(
        "WAL files {list} of node [{}] are missing from the range {}..={to} holding operations of shard {shard} of cluster [{}]: acknowledged operations would be lost",
        src.node,
        src.from,
        src.cluster
    )
}

/// The runs of `a..=b` outside every (sorted) inclusive run of `skip`.
fn subtract(a: u64, b: u64, skip: &[[u64; 2]]) -> Vec<[u64; 2]> {
    let mut out = Vec::new();
    let mut next = a;
    for &[s, e] in skip {
        if e < next || s > b {
            continue;
        }
        if s > next {
            out.push([next, s - 1]);
        }
        if e >= b {
            return out;
        }
        next = next.max(e + 1);
    }
    if next <= b {
        out.push([next, b]);
    }
    out
}

/// What replay knows about a shipped WAL object: its directory, and its bytes when it was small
/// enough to come whole with the first read.
pub(crate) struct ObjectView {
    entries: Vec<Entry>,
    whole: Option<bytes::Bytes>,
    size: u64,
}

impl ObjectView {
    /// Memory it holds, for the shared cache's budget.
    fn cost(&self) -> usize {
        self.whole.as_ref().map_or(0, |b| b.len()) + self.entries.iter().map(|e| 96 + e.cluster.len() + e.index_uuid.len()).sum::<usize>()
    }
}

fn missing(key: &str) -> anyhow::Error {
    anyhow::anyhow!("WAL object {key} is missing")
}

/// Read a WAL object's directory (and the whole object when it is small).
async fn fetch_view(objects: &pg_storage::Objects, key: &str, seq: u64, cache: &ReplayCache) -> anyhow::Result<ObjectView> {
    let (tail, size) = objects.get_suffix(key, TAIL).await?.ok_or_else(|| missing(key))?;
    cache.fetched_bytes.fetch_add(tail.len() as u64, Relaxed);
    if tail.len() as u64 == size {
        let obj = WalObject::parse(&tail).map_err(|e| anyhow::anyhow!("WAL object {key}: {e}"))?;
        anyhow::ensure!(obj.header.seq == seq, "WAL object {key} claims to be file {}", obj.header.seq);
        return Ok(ObjectView { entries: obj.entries, whole: Some(tail), size });
    }
    anyhow::ensure!(tail.len() >= TRAILER, "WAL object {key}: short read of its tail");
    let t = Trailer::decode(&tail[tail.len() - TRAILER..], size).map_err(|e| anyhow::anyhow!("WAL object {key}: {e}"))?;
    let tail_start = size - tail.len() as u64;
    let dir = if t.dir_offset >= tail_start {
        tail.slice((t.dir_offset - tail_start) as usize..tail.len() - TRAILER)
    } else {
        // A directory bigger than the tail read (a file with very many shards).
        cache.fetches.fetch_add(1, Relaxed);
        let (dir, now) = objects.get_range(key, t.dir_offset..size - TRAILER as u64).await?.ok_or_else(|| missing(key))?;
        anyhow::ensure!(now == size, "WAL object {key} changed size while being read");
        cache.fetched_bytes.fetch_add(dir.len() as u64, Relaxed);
        dir
    };
    let entries = t.directory(&dir).map_err(|e| anyhow::anyhow!("WAL object {key}: {e}"))?;
    Ok(ObjectView { entries, whole: None, size })
}

/// Pick this shard's block out of a shipped object (reading just that block when the object
/// didn't come whole) and decompress it.
async fn load_object(view: Arc<ObjectView>, objects: &pg_storage::Objects, key: &str, want: &Want, cache: &ReplayCache) -> anyhow::Result<Loaded> {
    // Times only grow: once a block ends past the cut, later files start past it.
    let past_cut = want.until_ms.is_some_and(|u| view.entries.iter().any(|e| e.last_ms > u));
    let entry = view.entries.iter().find(|e| {
        e.shard == want.shard.shard
            && e.index_uuid == want.shard.index_uuid
            && e.cluster == want.cluster
            && e.max_seq_no > want.after_seq_no
            && want.until_ms.is_none_or(|u| e.first_ms <= u)
    });
    let Some(e) = entry.cloned() else { return Ok(Loaded::Block { records: None, past_cut }) };
    let records = match &view.whole {
        Some(data) if e.len < INLINE_INFLATE => WalObject::block(data, &e),
        Some(data) => {
            let data = data.clone();
            tokio::task::spawn_blocking(move || WalObject::block(&data, &e)).await?
        }
        None => {
            anyhow::ensure!(e.len > 0, "WAL object {key} has an empty block");
            cache.block_fetches.fetch_add(1, Relaxed);
            let (block, now) = objects.get_range(key, e.offset..e.offset + e.len as u64).await?.ok_or_else(|| missing(key))?;
            anyhow::ensure!(now == view.size, "WAL object {key} changed size while being read");
            cache.fetched_bytes.fetch_add(block.len() as u64, Relaxed);
            if e.len < INLINE_INFLATE {
                WalObject::inflate_block(&block, &e)
            } else {
                tokio::task::spawn_blocking(move || WalObject::inflate_block(&block, &e)).await?
            }
        }
    }
    .map_err(|err| anyhow::anyhow!("WAL object {key}: {err}"))?;
    Ok(Loaded::Block { records: Some(records), past_cut })
}

/// Map the first `len` (intact, synced) bytes of a local WAL file and ask the kernel to read
/// them ahead, so iterating doesn't stall on page faults one page at a time.
fn map_local(path: &std::path::Path, len: u64) -> std::io::Result<memmap2::Mmap> {
    let file = std::fs::File::open(path)?;
    // SAFETY: WAL files are only appended to while the process runs (truncation happens only
    // at startup, before any replay), and the map covers bytes already written and synced, so
    // the mapped range never changes underneath. Deleting the file keeps the inode alive.
    let map = unsafe { memmap2::MmapOptions::new().len(len as usize).map(&file)? };
    let _ = map.advise(memmap2::Advice::WillNeed);
    Ok(map)
}

impl Wal {
    /// Is this the range this process is writing to right now?
    pub(crate) fn is_live(&self, src: &WalSource) -> bool {
        src.to.is_none() && src.node == self.sh.node && src.from >= self.sh.first_file
    }

    /// `node`'s WAL objects and run markers (listed once per [`Listings`]).
    async fn listed(&self, node: &str, cache: &Listings) -> anyhow::Result<Arc<NodeListing>> {
        if let Some(v) = cache.0.lock().get(node) {
            return Ok(v.clone());
        }
        let mut v = NodeListing::default();
        for (k, _) in self.sh.objects.list(&keys::wal_prefix(node)).await? {
            if let Some((n, seq)) = keys::parse_wal_key(&k) {
                if n == node {
                    v.files.push(seq);
                }
            } else if let Some((n, seq)) = keys::parse_run_key(&k)
                && n == node
            {
                v.runs.push(seq);
            }
        }
        v.files.sort_unstable();
        v.runs.sort_unstable();
        let v = Arc::new(v);
        // A concurrent lister may have won: keep the first, so every user of the cache sees one
        // snapshot per node.
        Ok(cache.0.lock().entry(node.to_string()).or_insert(v).clone())
    }

    /// Cap open-ended sources that this process doesn't write at the newest file that exists.
    pub(crate) async fn freeze(&self, sources: &mut [WalSource], listings: &Listings) -> anyhow::Result<()> {
        for src in sources.iter_mut().filter(|s| s.to.is_none()) {
            if self.is_live(src) {
                continue;
            }
            let listed = self.listed(&src.node, listings).await?;
            let run_end = listed.run_end(src.from);
            if src.node == self.sh.node {
                // An earlier run of this node: everything it wrote is below our first file (and
                // below the next run's, when runs came in between), and is either shipped already
                // or local and queued (this node never loses files it still has), so nothing can
                // show up later that a replay now wouldn't see. Not exact: a crash can leave
                // numbers unused, and unshipped local files aren't in the listing.
                let to = run_end.unwrap_or(u64::MAX).min(self.sh.first_file - 1);
                src.to = Some(to.max(src.from.saturating_sub(1)));
                continue;
            }
            let newest = listed.files.last().copied().unwrap_or(0);
            let to = newest.min(run_end.unwrap_or(u64::MAX)).max(src.from.saturating_sub(1));
            src.to = Some(to);
            src.exact = true;
            if to >= src.from {
                src.skip.extend(gaps(src.from, to, &listed.files));
            }
        }
        Ok(())
    }

    /// Replay the records of `shard` with `seq_no > after_seq_no` from `sources`, in order.
    /// Open-ended ranges this process doesn't write are first capped at the newest file that
    /// exists, and sources get their `max_seq_no` filled in.
    pub async fn replay(
        &self,
        sources: &mut [WalSource],
        shard: &ShardId,
        after_seq_no: i64,
        f: &mut (dyn FnMut(&WalRecord<'_>) -> anyhow::Result<()> + Send),
    ) -> anyhow::Result<ReplayStats> {
        // Snapshot this node's local files before listing any object: a local file is deleted
        // only after it shipped, so whatever the snapshot misses the listing has. (The other
        // order loses a file that ships and is trimmed in between.)
        let locals: Vec<HashMap<u64, u64>> = sources
            .iter()
            .map(|src| match src.to {
                Some(to) if to < src.from => HashMap::new(),
                to if src.node == self.sh.node => self.sh.local_range(src.from, to.unwrap_or(u64::MAX)).into_iter().collect(),
                _ => HashMap::new(),
            })
            .collect();
        let cache = Listings::default();
        self.freeze(sources, &cache).await?;
        let _active = self.sh.replay_cache.begin();
        let mut stats = ReplayStats { max_seq_no: after_seq_no, ..Default::default() };
        for (src, local) in sources.iter_mut().zip(locals) {
            let to = src.to.unwrap_or(u64::MAX);
            if to < src.from {
                src.max_seq_no = Some(src.max_seq_no.unwrap_or(after_seq_no).max(after_seq_no));
                continue;
            }
            let mut seqs: BTreeSet<u64> = self.listed(&src.node, &cache).await?.files.iter().copied().filter(|s| (src.from..=to).contains(s)).collect();
            let local: HashMap<u64, u64> = local.into_iter().filter(|(s, _)| *s <= to).collect();
            seqs.extend(local.keys().copied());
            seqs.retain(|s| !src.skips(*s));
            if src.exact {
                check_complete(src, shard, &seqs)?;
            }
            let max = self.replay_source(src, shard, after_seq_no, seqs, &local, &mut stats, f).await?;
            src.max_seq_no = Some(src.max_seq_no.unwrap_or(after_seq_no).max(max));
            stats.max_seq_no = stats.max_seq_no.max(max);
        }
        self.sh.counters.replayed_records.fetch_add(stats.records, Relaxed);
        self.sh.counters.replayed_files.fetch_add(stats.files, Relaxed);
        Ok(stats)
    }

    #[allow(clippy::too_many_arguments)]
    async fn replay_source(
        &self,
        src: &WalSource,
        shard: &ShardId,
        after_seq_no: i64,
        seqs: BTreeSet<u64>,
        local: &HashMap<u64, u64>,
        stats: &mut ReplayStats,
        f: &mut (dyn FnMut(&WalRecord<'_>) -> anyhow::Result<()> + Send),
    ) -> anyhow::Result<i64> {
        let sh = &self.sh;
        let node = src.node.as_str();
        let want = Want { cluster: src.cluster.clone(), shard: shard.clone(), after_seq_no, until_ms: src.until_ms };
        let want = &want;
        let mut loads = futures::stream::iter(seqs.into_iter().map(|seq| {
            let local_len = local.get(&seq).copied();
            async move {
                if let Some(len) = local_len {
                    match map_local(&sh.path(seq), len) {
                        Ok(map) => return anyhow::Ok((seq, Loaded::Raw(map))),
                        // Shipped and trimmed meanwhile: fall back to the object.
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                let key = keys::wal_key(node, seq);
                let view = sh.replay_cache.get(&sh.objects, &key, seq).await?;
                Ok((seq, load_object(view, &sh.objects, &key, want, &sh.replay_cache).await?))
            }
        }))
        .buffered(PREFETCH);
        let mut max = after_seq_no;
        let wanted = |r: &WalRecord<'_>| r.shard == shard.shard && r.index_uuid == shard.index_uuid && r.cluster == src.cluster;
        let mut stop = false;
        while let Some(loaded) = loads.next().await {
            let (seq, loaded) = loaded?;
            stats.files += 1;
            let mut visit = |r: &WalRecord<'_>| -> anyhow::Result<bool> {
                if src.until_ms.is_some_and(|u| r.at_ms > u) {
                    // Append times only grow on a node: everything after is later too.
                    return Ok(false);
                }
                if wanted(r) && r.seq_no > after_seq_no {
                    f(r)?;
                    stats.records += 1;
                    stats.bytes += r.payload.len() as u64;
                    max = max.max(r.seq_no);
                }
                Ok(true)
            };
            match loaded {
                Loaded::Raw(raw) => {
                    let h = record::FileHeader::decode(&raw).ok_or_else(|| anyhow::anyhow!("local WAL file {seq} has a bad header"))?;
                    anyhow::ensure!(h.seq == seq, "local WAL file {seq} claims to be file {}", h.seq);
                    for r in Records::new(&raw[record::FILE_HEADER..]) {
                        if !visit(&r)? {
                            stop = true;
                            break;
                        }
                    }
                }
                Loaded::Block { records, past_cut } => {
                    if let Some(block) = records {
                        let mut it = Records::new(&block);
                        for r in it.by_ref() {
                            if !visit(&r)? {
                                stop = true;
                                break;
                            }
                        }
                        anyhow::ensure!(stop || it.valid_len() == block.len(), "WAL object {node}/{seq} is corrupt");
                    }
                    stop |= past_cut;
                }
            }
            if stop {
                break;
            }
        }
        Ok(max)
    }
}

type Shared = Option<Result<Arc<ObjectView>, String>>;

#[derive(Default)]
struct CacheState {
    map: HashMap<String, (Arc<ObjectView>, u64)>,
    used: usize,
    tick: u64,
    inflight: HashMap<String, tokio::sync::watch::Receiver<Shared>>,
    /// Replays running now: the cache lives only while some do, so it costs nothing idle.
    active: usize,
}

/// WAL object directories (and small objects whole) read during replay, shared by the replays
/// running at the same time (after a node failure every shard it wrote recovers from the same
/// files). Concurrent reads of one object are merged; the cache empties when the last replay
/// finishes.
pub(crate) struct ReplayCache {
    budget: usize,
    st: parking_lot::Mutex<CacheState>,
    /// Directory (or whole small object) reads, and reads served by another replay's.
    pub(crate) fetches: std::sync::atomic::AtomicU64,
    pub(crate) hits: std::sync::atomic::AtomicU64,
    /// Ranged reads of single blocks, and bytes downloaded by replay in all.
    pub(crate) block_fetches: std::sync::atomic::AtomicU64,
    pub(crate) fetched_bytes: std::sync::atomic::AtomicU64,
}

/// Removes an in-flight marker even when the fetching replay is cancelled.
struct Lead<'a> {
    cache: &'a ReplayCache,
    key: &'a str,
}

impl Drop for Lead<'_> {
    fn drop(&mut self) {
        self.cache.st.lock().inflight.remove(self.key);
    }
}

/// Keeps a replay counted as active.
pub(crate) struct Active<'a>(&'a ReplayCache);

impl Drop for Active<'_> {
    fn drop(&mut self) {
        let mut st = self.0.st.lock();
        st.active -= 1;
        if st.active == 0 {
            st.map.clear();
            st.used = 0;
        }
    }
}

impl ReplayCache {
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            budget,
            st: parking_lot::Mutex::new(CacheState::default()),
            fetches: Default::default(),
            hits: Default::default(),
            block_fetches: Default::default(),
            fetched_bytes: Default::default(),
        }
    }

    /// Bytes held now.
    pub(crate) fn used(&self) -> usize {
        self.st.lock().used
    }

    pub(crate) fn begin(&self) -> Active<'_> {
        self.st.lock().active += 1;
        Active(self)
    }

    pub(crate) async fn get(&self, objects: &pg_storage::Objects, key: &str, seq: u64) -> anyhow::Result<Arc<ObjectView>> {
        loop {
            let (mut rx, tx) = {
                let mut st = self.st.lock();
                st.tick += 1;
                let tick = st.tick;
                if let Some((b, t)) = st.map.get_mut(key) {
                    *t = tick;
                    self.hits.fetch_add(1, Relaxed);
                    return Ok(b.clone());
                }
                match st.inflight.get(key) {
                    Some(rx) => {
                        self.hits.fetch_add(1, Relaxed);
                        (rx.clone(), None)
                    }
                    None => {
                        let (tx, rx) = tokio::sync::watch::channel(None);
                        st.inflight.insert(key.to_string(), rx.clone());
                        (rx, Some(tx))
                    }
                }
            };
            let Some(tx) = tx else {
                match rx.wait_for(|v| v.is_some()).await {
                    Ok(v) => return v.clone().expect("completed").map_err(|e| anyhow::anyhow!(e)),
                    // The fetching replay was cancelled: try again.
                    Err(_) => continue,
                }
            };
            let _lead = Lead { cache: self, key };
            self.fetches.fetch_add(1, Relaxed);
            let r = fetch_view(objects, key, seq, self).await.map(Arc::new);
            if let Ok(v) = &r {
                let cost = v.cost();
                let mut st = self.st.lock();
                if st.active > 0 && cost <= self.budget {
                    while st.used + cost > self.budget {
                        let Some(oldest) = st.map.iter().min_by_key(|(_, (_, t))| *t).map(|(k, _)| k.clone()) else { break };
                        let (old, _) = st.map.remove(&oldest).expect("present");
                        st.used -= old.cost();
                    }
                    st.used += cost;
                    let tick = st.tick;
                    st.map.insert(key.to_string(), (v.clone(), tick));
                }
            }
            tx.send_replace(Some(r.as_ref().map(|b| b.clone()).map_err(|e| format!("{e:#}"))));
            return r;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::gaps;

    #[test]
    fn skipped_runs_are_subtracted() {
        use super::subtract;
        assert_eq!(subtract(5, 9, &[]), vec![[5, 9]]);
        assert_eq!(subtract(5, 9, &[[1, 4], [10, 12]]), vec![[5, 9]]);
        assert_eq!(subtract(5, 9, &[[5, 9]]), Vec::<[u64; 2]>::new());
        assert_eq!(subtract(5, 9, &[[1, 6], [8, 8]]), vec![[7, 7], [9, 9]]);
        assert_eq!(subtract(5, 9, &[[6, 6], [6, 7]]), vec![[5, 5], [8, 9]]);
        assert_eq!(subtract(0, u64::MAX, &[[0, u64::MAX]]), Vec::<[u64; 2]>::new());
    }

    #[test]
    fn a_crashed_run_ends_where_the_next_one_starts() {
        use super::run_end;
        assert_eq!(run_end(&[], 5), None);
        assert_eq!(run_end(&[1], 5), None, "the latest run may still be writing");
        assert_eq!(run_end(&[1, 9, 20], 5), Some(8));
        assert_eq!(run_end(&[1, 9, 20], 9), Some(19), "a run's own first file");
        assert_eq!(run_end(&[1, 9, 20], 20), None);
    }

    #[test]
    fn gaps_are_runs_of_missing_files() {
        assert_eq!(gaps(5, 9, &[5, 6, 7, 8, 9]), Vec::<[u64; 2]>::new());
        assert_eq!(gaps(5, 9, &[1, 6, 9, 12]), vec![[5, 5], [7, 8]]);
        assert_eq!(gaps(5, 9, &[]), vec![[5, 9]]);
        assert_eq!(gaps(5, 9, &[5, 7]), vec![[6, 6], [8, 9]]);
    }
}
