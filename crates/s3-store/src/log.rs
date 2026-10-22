//! The metadata log: what makes metadata as durable as the blobs. Modeled on the tasks log
//! store (`crates/tasks/src/store/log.rs`), for one database.
//!
//! ```text
//! s3/meta/wal/<seq, 20 digits>           one group commit (a SQLite patchset), create-only
//! s3/meta/snap/<seq, 20 digits>.db.zst   the database after entry <seq>
//! s3/meta/pruned                         entries up to here may be gone
//! ```
//!
//! - A write returns once the entry holding it is in object storage. Entries group everything
//!   written while the previous one uploaded and start at most every `commit_ms`.
//! - Entries are written create-only. A node writes a takeover entry at the next sequence number
//!   before serving, so a previous writer's next commit fails and it fences itself: no more
//!   writes, and reads fail too, until it restarts and rebuilds from object storage.
//! - Each entry ends in a CRC32C of everything before it; a corrupt entry stops recovery
//!   (nothing is skipped silently).
//! - Snapshots are copied a few pages at a time, cut at an exact entry, compressed, uploaded,
//!   then downloaded again and integrity-checked before they count. The newest two verified
//!   ones are kept; entries up to the older of the two are deleted.

use crate::backend::Backend;
use crate::db::{self, Db};
use crate::error::{Result, StoreError};
use crate::schema;
use bytes::Bytes;
use futures::StreamExt;
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};
use tracing::{info, warn};

const MAGIC: &[u8] = b"RS3LOG1\n";

#[derive(Clone, Debug)]
pub struct LogConfig {
    /// Key prefix in the backend (`s3/meta`).
    pub prefix: String,
    /// Local working directory (the database cache and snapshot copies).
    pub dir: PathBuf,
    /// Written into entries, for the record.
    pub node: String,
    /// Minimum time between the starts of two commits: caps the PUT rate at 1000 / `commit_ms`
    /// per second whatever the traffic; durable writes wait up to this long.
    pub commit_ms: u64,
    /// Snapshot after this many entries …
    pub snapshot_commits: u64,
    /// … or this many bytes of log.
    pub snapshot_bytes: u64,
    /// Pages (4 KiB) copied per snapshot step; writes pause for one step at a time.
    pub snapshot_step_pages: i32,
    /// While idle, check this often whether another node took over the log (a HEAD of the
    /// next entry), so a deposed node stops serving stale reads without having to write.
    pub fence_check_ms: u64,
}

impl LogConfig {
    pub fn new(dir: PathBuf) -> Self {
        Self { prefix: "s3/meta".into(), dir, node: "local".into(), commit_ms: 20, snapshot_commits: 10_000, snapshot_bytes: 64 << 20, snapshot_step_pages: 256, fence_check_ms: 5_000 }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LogStats {
    /// The last entry written.
    pub seq: u64,
    pub commits: u64,
    pub commit_bytes: u64,
    pub snapshots: u64,
    pub last_snapshot_seq: u64,
    pub pruned: u64,
    pub db_bytes: u64,
    pub fenced: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct Durable {
    batch: u64,
    fenced: Option<String>,
}

pub(crate) struct MetaLog {
    pub(crate) db: Arc<Db>,
    backend: Arc<dyn Backend>,
    cfg: LogConfig,
    durable: watch::Sender<Durable>,
    kick: Notify,
    stopping: AtomicBool,
    force_snapshot: AtomicBool,
    /// Newest verified snapshot.
    snapshots: watch::Sender<u64>,
    /// Verified snapshots in object storage, and other snapshot objects to delete.
    snaps: Mutex<(BTreeSet<u64>, BTreeSet<u64>)>,
    /// Entries in object storage.
    wal: Mutex<BTreeSet<u64>>,
    stats: Mutex<LogStats>,
}

fn wal_key(prefix: &str, seq: u64) -> String {
    format!("{prefix}/wal/{seq:020}")
}

fn snap_key(prefix: &str, seq: u64) -> String {
    format!("{prefix}/snap/{seq:020}.db.zst")
}

fn pruned_key(prefix: &str) -> String {
    format!("{prefix}/pruned")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn encode(seq: u64, kind: &str, node: &str, patch: &[u8]) -> Vec<u8> {
    let meta = serde_json::to_vec(&serde_json::json!({ "seq": seq, "kind": kind, "node": node, "at": now_ms(), "schema": schema::LATEST })).unwrap_or_default();
    let mut out = Vec::with_capacity(MAGIC.len() + 8 + meta.len() + patch.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(meta.len() as u32).to_be_bytes());
    out.extend_from_slice(&meta);
    out.extend_from_slice(patch);
    let crc = crc32c::crc32c(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

pub(crate) struct Entry {
    pub(crate) seq: u64,
    pub(crate) schema: u32,
    pub(crate) patch: Vec<u8>,
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Entry> {
    let bad = |why: &str| StoreError::internal(format!("corrupt metadata log entry: {why}"));
    if bytes.len() < MAGIC.len() + 8 || !bytes.starts_with(MAGIC) {
        return Err(bad("not a roll storage log entry"));
    }
    let (body, crc) = bytes.split_at(bytes.len() - 4);
    if crc32c::crc32c(body) != u32::from_be_bytes(crc.try_into().unwrap()) {
        return Err(bad("checksum mismatch"));
    }
    let rest = &body[MAGIC.len()..];
    let n = u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
    if rest.len() < 4 + n {
        return Err(bad("truncated"));
    }
    let meta: serde_json::Value = serde_json::from_slice(&rest[4..4 + n]).map_err(|_| bad("bad header"))?;
    Ok(Entry {
        seq: meta["seq"].as_u64().ok_or_else(|| bad("no seq"))?,
        schema: meta["schema"].as_u64().unwrap_or(1) as u32,
        patch: rest[4 + n..].to_vec(),
    })
}

fn seq_of(key: &str) -> Option<u64> {
    key.rsplit('/').next()?.split('.').next()?.parse().ok()
}

fn compress_file(path: &std::path::Path) -> Result<Vec<u8>> {
    let input = std::fs::File::open(path)?;
    let mut enc = zstd::stream::Encoder::new(Vec::new(), 3)?;
    enc.include_checksum(true)?;
    let mut reader = std::io::BufReader::new(input);
    std::io::copy(&mut reader, &mut enc)?;
    Ok(enc.finish()?)
}

fn decompress_to(data: &[u8], path: &std::path::Path) -> Result<()> {
    let _ = std::fs::remove_file(path);
    let mut out = std::fs::File::create(path)?;
    zstd::stream::copy_decode(data, &mut out)?;
    out.sync_all()?;
    Ok(())
}

fn remove_db_files(path: &std::path::Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

/// GET with a few retries: an error reading object storage is taken for a transient one
/// (corruption shows up when the bytes are checked). A missing object is `Ok(None)`.
async fn get_retry(backend: &dyn Backend, key: &str) -> Result<Option<Bytes>> {
    let mut delay = 100u64;
    let mut attempt = 0;
    loop {
        match backend.get_bytes(key).await {
            Ok(b) => return Ok(b),
            Err(e) if attempt < 4 => {
                warn!(error = %e, key, attempt, "reading metadata from object storage failed; retrying");
                tokio::time::sleep(Duration::from_millis(delay)).await;
                delay *= 2;
                attempt += 1;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Fencing relies on create-only writes (`If-None-Match: *`). An S3-compatible service that
/// ignores the header would let two writers overwrite each other's log entries, so check it
/// does before writing anything.
async fn probe_create_only(backend: &dyn Backend, prefix: &str) -> Result<()> {
    let key = format!("{prefix}/probe/{}", crate::ids::blob_id());
    let first = backend.put_create_only(&key, Bytes::from_static(b"1")).await;
    let second = match &first {
        Ok(true) => backend.put_create_only(&key, Bytes::from_static(b"2")).await,
        _ => Ok(true),
    };
    if let Err(e) = backend.delete(&key).await {
        warn!(error = %e, key, "deleting the create-only probe failed");
    }
    match (first?, second?) {
        (true, false) => Ok(()),
        _ => Err(StoreError::internal(format!(
            "{} doesn't honour create-only writes (If-None-Match: *); refusing to start, as another node could overwrite the metadata log",
            backend.describe()
        ))),
    }
}

impl MetaLog {
    /// Rebuild the database from object storage, take over as the log's writer, and start
    /// committing.
    pub(crate) async fn open(backend: Arc<dyn Backend>, cfg: LogConfig) -> Result<Arc<Self>> {
        let db_path = cfg.dir.join("meta.db");
        let snap_dir = cfg.dir.join("snapshots");
        remove_db_files(&db_path);
        let _ = std::fs::remove_dir_all(&snap_dir);
        std::fs::create_dir_all(&snap_dir)?;
        let prefix = cfg.prefix.clone();

        let pruned = match backend.get_bytes(&pruned_key(&prefix)).await? {
            Some(b) => serde_json::from_slice::<serde_json::Value>(&b)?["through"].as_u64().unwrap_or(0),
            None => 0,
        };

        probe_create_only(&*backend, &prefix).await?;

        let wal_listed = crate::backend::list_all(&*backend, &format!("{prefix}/wal/")).await?;
        let mut all_wal: BTreeSet<u64> = wal_listed.iter().filter_map(|l| seq_of(&l.key)).collect();
        // Whether every entry after `seq` is still there (a snapshot older than the pruned log
        // may need entries that are gone).
        let replayable = |seq: u64| match all_wal.last() {
            Some(&max) if max > seq => all_wal.range(seq + 1..).count() as u64 == max - seq,
            _ => true,
        };

        // The newest snapshot that reads back intact and has the log after it; older ones are
        // the fallbacks.
        let listed = crate::backend::list_all(&*backend, &format!("{prefix}/snap/")).await?;
        let mut snap_seqs: Vec<u64> = listed.iter().filter_map(|l| seq_of(&l.key)).collect();
        snap_seqs.sort_unstable_by(|a, b| b.cmp(a));
        let mut base = None;
        let mut bad = BTreeSet::new();
        let mut unread = None;
        for &seq in &snap_seqs {
            if !replayable(seq) {
                continue;
            }
            let data = match get_retry(&*backend, &snap_key(&prefix, seq)).await {
                Ok(Some(d)) => d,
                Ok(None) => continue,
                Err(e) => {
                    warn!(error = %e, seq, "can't read a metadata snapshot; trying an older one");
                    unread = Some(e);
                    continue;
                }
            };
            let path = db_path.clone();
            let checked = tokio::task::spawn_blocking(move || -> Result<()> {
                decompress_to(&data, &path)?;
                db::check_file(&path)?;
                Ok(())
            })
            .await
            .map_err(StoreError::internal)?;
            match checked {
                Ok(()) => {
                    base = Some(seq);
                    break;
                }
                Err(e) => {
                    warn!(error = %e, seq, "corrupt metadata snapshot; trying an older one");
                    bad.insert(seq);
                    remove_db_files(&db_path);
                }
            }
        }
        let base = match base {
            Some(b) => b,
            None if pruned == 0 && replayable(0) => 0,
            None if pruned == 0 => {
                let missing = (1..).find(|s| !all_wal.contains(s)).unwrap_or(0);
                return Err(StoreError::internal(format!("metadata log gap: entry {missing} is missing")));
            }
            None => {
                let why = unread.map_or_else(String::new, |e| format!(" (last read error: {e})"));
                return Err(StoreError::internal(format!("metadata: no readable snapshot has the log after it, and log entries up to {pruned} are pruned{why}")));
            }
        };
        // Verified ones at or below the base; the rest (corrupt, or newer but unread) are deleted
        // once a newer snapshot is verified.
        let good: BTreeSet<u64> = snap_seqs.iter().copied().filter(|s| *s <= base && !bad.contains(s)).collect();
        bad.extend(snap_seqs.iter().copied().filter(|s| *s > base));
        let db = Arc::new(Db::open(&db_path)?);
        if db.schema_version()? > schema::LATEST {
            return Err(StoreError::internal("metadata was written by a newer version of roll; upgrade"));
        }

        // The log after the snapshot, in order, without gaps.
        let needed: Vec<u64> = all_wal.iter().copied().filter(|s| *s > base).collect();
        for (i, s) in needed.iter().enumerate() {
            let want = base + 1 + i as u64;
            if *s != want {
                return Err(StoreError::internal(format!("metadata log gap: expected entry {want}, found {s}")));
            }
        }
        let mut entries = futures::stream::iter(needed.clone())
            .map(|s| {
                let backend = backend.clone();
                let key = wal_key(&prefix, s);
                async move { Ok::<_, StoreError>((s, get_retry(&*backend, &key).await?.ok_or_else(|| StoreError::internal(format!("log entry {s} vanished")))?)) }
            })
            .buffered(32);
        let mut last = base;
        let mut log_bytes = 0u64;
        while let Some(e) = entries.next().await {
            let (seq, bytes) = e?;
            let entry = decode(&bytes)?;
            if entry.seq != seq {
                return Err(StoreError::internal(format!("log entry {seq} claims to be {}", entry.seq)));
            }
            Self::replay(&db, &entry).map_err(|e| StoreError::internal(format!("metadata log entry {seq} doesn't apply: {e}")))?;
            log_bytes += bytes.len() as u64;
            last = seq;
        }
        drop(entries);

        // Take over: claim the next entry. Anything a previous writer slipped in first gets
        // replayed, and from here on its commits fail.
        let mut seq = last + 1;
        loop {
            let marker = encode(seq, "takeover", &cfg.node, &[]);
            if backend.put_create_only(&wal_key(&prefix, seq), Bytes::from(marker)).await? {
                break;
            }
            let bytes = get_retry(&*backend, &wal_key(&prefix, seq)).await?.ok_or_else(|| StoreError::internal("log entry vanished"))?;
            let entry = decode(&bytes)?;
            if entry.seq != seq {
                return Err(StoreError::internal(format!("log entry {seq} claims to be {}", entry.seq)));
            }
            Self::replay(&db, &entry).map_err(|e| StoreError::internal(format!("metadata log entry {seq} doesn't apply: {e}")))?;
            all_wal.insert(seq);
            seq += 1;
        }
        all_wal.insert(seq);
        db.migrate_to(schema::LATEST)?;
        db.record_changes()?;
        info!(snapshot = base, replayed = seq - 1 - base, seq, "storage metadata open");

        let log = Arc::new(Self {
            db,
            backend,
            durable: watch::channel(Durable::default()).0,
            kick: Notify::new(),
            stopping: AtomicBool::new(false),
            force_snapshot: AtomicBool::new(false),
            snapshots: watch::channel(base).0,
            snaps: Mutex::new((good, bad)),
            wal: Mutex::new(all_wal),
            stats: Mutex::new(LogStats { seq, pruned, last_snapshot_seq: base, ..Default::default() }),
            cfg,
        });
        let watcher = log.clone();
        tokio::spawn(async move {
            use futures::FutureExt;
            let run = std::panic::AssertUnwindSafe(committer(watcher.clone(), seq + 1, seq - base, log_bytes));
            if run.catch_unwind().await.is_err() {
                // Waiting writes get Unavailable, and reads stop, instead of hanging on a log
                // nobody writes.
                watcher.fence("the metadata committer crashed".into());
            }
        });
        Ok(log)
    }

    fn replay(db: &Db, entry: &Entry) -> Result<()> {
        if entry.schema > schema::LATEST {
            return Err(StoreError::internal("metadata log written by a newer version of roll; upgrade"));
        }
        if db.schema_version()? < entry.schema {
            db.migrate_to(entry.schema)?;
        }
        db.apply(&entry.patch)
    }

    pub(crate) fn check_open(&self) -> Result<()> {
        match self.durable.borrow().fenced.clone() {
            Some(reason) => Err(StoreError::Unavailable(reason)),
            None => Ok(()),
        }
    }

    /// Something was written: commit it soon (relaxed writes stop here).
    pub(crate) fn kick(&self) {
        self.kick.notify_one();
    }

    /// Wait until `batch` is in object storage.
    pub(crate) async fn wait(&self, batch: u64) -> Result<()> {
        self.kick.notify_one();
        let mut rx = self.durable.subscribe();
        loop {
            {
                let d = rx.borrow_and_update();
                if let Some(reason) = &d.fenced {
                    return Err(StoreError::Unavailable(reason.clone()));
                }
                if d.batch >= batch {
                    return Ok(());
                }
            }
            rx.changed().await.map_err(|_| StoreError::Unavailable("metadata log closed".into()))?;
        }
    }

    fn fence(&self, reason: String) {
        if reason != "closed" {
            warn!(%reason, "storage metadata log fenced");
        }
        self.stats.lock().unwrap().fenced = Some(reason.clone());
        self.durable.send_modify(|d| d.fenced = Some(reason));
    }

    /// Commit everything written so far and stop. Writes (and reads) after this fail.
    pub(crate) async fn close(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.kick.notify_one();
        let mut rx = self.durable.subscribe();
        let _ = tokio::time::timeout(Duration::from_secs(60), async {
            while rx.borrow_and_update().fenced.is_none() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
    }

    /// Take a snapshot now and wait until it's uploaded and verified. Returns its entry.
    pub(crate) async fn snapshot_now(&self) -> Result<u64> {
        self.check_open()?;
        let target = self.stats.lock().unwrap().seq;
        let mut rx = self.snapshots.subscribe();
        if *rx.borrow_and_update() >= target {
            return Ok(*rx.borrow());
        }
        self.force_snapshot.store(true, Ordering::SeqCst);
        self.kick.notify_one();
        loop {
            let v = *rx.borrow_and_update();
            if v >= target {
                return Ok(v);
            }
            self.check_open()?;
            tokio::select! {
                r = rx.changed() => r.map_err(|_| StoreError::Unavailable("metadata log closed".into()))?,
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
    }

    pub(crate) fn stats(&self) -> LogStats {
        let mut s = self.stats.lock().unwrap().clone();
        s.db_bytes = self.db.size_bytes();
        s
    }

    /// Upload one entry, retrying transient errors. `Ok(false)`: the slot was taken.
    async fn put_entry(&self, seq: u64, body: Vec<u8>) -> Result<bool> {
        let key = wal_key(&self.cfg.prefix, seq);
        let body = Bytes::from(body);
        let started = Instant::now();
        let mut delay = 50u64;
        loop {
            match self.backend.put_create_only(&key, body.clone()).await {
                Ok(true) => return Ok(true),
                // The slot is taken: by another writer, or by this very entry (an earlier
                // attempt of ours, or the client library's own retry, whose response was lost).
                // Entries carry their writer and a timestamp, so equal bytes mean it's ours.
                Ok(false) => {
                    let found = get_retry(&*self.backend, &key).await?;
                    return Ok(found.as_deref() == Some(&body[..]));
                }
                Err(e) if started.elapsed() < Duration::from_secs(30) => {
                    warn!(error = %e, seq, "metadata commit failed; retrying");
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    delay = (delay * 2).min(2_000);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Compress, upload and verify a snapshot copy, then prune what it makes unnecessary.
    async fn upload_snapshot(self: Arc<Self>, file: PathBuf, seq: u64) -> Result<()> {
        let f = file.clone();
        let data = tokio::task::spawn_blocking(move || compress_file(&f)).await.map_err(StoreError::internal)??;
        let _ = std::fs::remove_file(&file);
        let key = snap_key(&self.cfg.prefix, seq);
        self.backend.put_small(&key, Bytes::from(data)).await?;
        // Only a snapshot read back intact counts.
        let back = self.backend.get_bytes(&key).await?.ok_or_else(|| StoreError::internal("snapshot vanished after upload"))?;
        let check = self.cfg.dir.join("snapshots").join(format!("verify-{seq}.db"));
        let c2 = check.clone();
        let verified = tokio::task::spawn_blocking(move || -> Result<()> {
            decompress_to(&back, &c2)?;
            db::check_file(&c2)?;
            Ok(())
        })
        .await
        .map_err(StoreError::internal)?;
        remove_db_files(&check);
        if let Err(e) = verified {
            // Never counted; deleted now if possible, else once a later snapshot is verified.
            self.snaps.lock().unwrap().1.insert(seq);
            if self.backend.delete(&key).await.is_ok() {
                self.snaps.lock().unwrap().1.remove(&seq);
            }
            return Err(e);
        }
        let (prev, stale) = {
            let mut snaps = self.snaps.lock().unwrap();
            snaps.0.insert(seq);
            let keep: Vec<u64> = snaps.0.iter().rev().take(2).copied().collect();
            let prev = (keep.len() == 2).then(|| keep[1]);
            let mut stale: Vec<u64> = snaps.0.iter().copied().filter(|s| prev.is_some_and(|p| *s < p)).collect();
            stale.extend(snaps.1.iter().copied().filter(|s| *s < seq));
            (prev, stale)
        };
        {
            let mut st = self.stats.lock().unwrap();
            st.snapshots += 1;
            st.last_snapshot_seq = seq;
        }
        info!(seq, "storage metadata snapshot");
        let pruned = match prev {
            Some(prev) => self.prune(prev, stale).await,
            None => Ok(()),
        };
        self.snapshots.send_modify(|v| *v = (*v).max(seq));
        pruned
    }

    /// Entries up to `through` (the older of the two kept snapshots) and older snapshots go.
    async fn prune(&self, through: u64, stale_snaps: Vec<u64>) -> Result<()> {
        let prefix = &self.cfg.prefix;
        self.backend.put_small(&pruned_key(prefix), Bytes::from(serde_json::to_vec(&serde_json::json!({ "through": through }))?)).await?;
        self.stats.lock().unwrap().pruned = through;
        let old: Vec<u64> = self.wal.lock().unwrap().iter().copied().filter(|s| *s <= through).collect();
        let deletes = futures::stream::iter(old.clone())
            .map(|s| {
                let key = wal_key(prefix, s);
                async move { self.backend.delete(&key).await }
            })
            .buffer_unordered(16)
            .collect::<Vec<_>>()
            .await;
        for r in deletes {
            r?;
        }
        self.wal.lock().unwrap().retain(|s| *s > through);
        for s in stale_snaps {
            self.backend.delete(&snap_key(prefix, s)).await?;
            let mut snaps = self.snaps.lock().unwrap();
            snaps.0.remove(&s);
            snaps.1.remove(&s);
        }
        Ok(())
    }
}

/// The single writer of the log: groups writes into entries, takes snapshots.
async fn committer(log: Arc<MetaLog>, mut next_seq: u64, mut since_entries: u64, mut since_bytes: u64) {
    let cfg = log.cfg.clone();
    let gap = Duration::from_millis(cfg.commit_ms);
    let mut last_start = Instant::now() - gap;
    let mut copying: Option<PathBuf> = None;
    let mut snapshot_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut last_check = Instant::now();
    loop {
        if last_check.elapsed() >= Duration::from_millis(cfg.fence_check_ms) {
            last_check = Instant::now();
            match log.backend.head(&wal_key(&cfg.prefix, next_seq)).await {
                Ok(Some(_)) => {
                    log.fence(format!("another node took over the metadata log at entry {next_seq}"));
                    break;
                }
                Ok(None) => {}
                Err(e) => warn!(error = %e, "checking the metadata log's writer failed"),
            }
        }
        let stopping = log.stopping.load(Ordering::SeqCst);
        if stopping && copying.take().is_some() {
            log.db.snapshot_abort();
        }
        let snapshot_due = !stopping
            && copying.is_none()
            && snapshot_task.as_ref().is_none_or(|t| t.is_finished())
            && (log.force_snapshot.load(Ordering::SeqCst) || since_entries >= cfg.snapshot_commits || since_bytes >= cfg.snapshot_bytes);
        if copying.is_none() && !snapshot_due && !stopping && !log.db.is_dirty() {
            tokio::select! {
                _ = log.kick.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(500.min(cfg.fence_check_ms))) => {}
            }
            continue;
        }

        // Copy a snapshot a few milliseconds at a time; between steps, commit at the usual pace.
        let mut snap_cut = None;
        if copying.is_some() {
            let budget = Instant::now();
            loop {
                match log.db.snapshot_step(cfg.snapshot_step_pages) {
                    Ok(Some(c)) => {
                        snap_cut = Some(c);
                        break;
                    }
                    Ok(None) if budget.elapsed() < Duration::from_millis(4) => {}
                    Ok(None) => break,
                    Err(e) => {
                        warn!(error = %e, "metadata snapshot failed");
                        log.db.snapshot_abort();
                        copying = None;
                        break;
                    }
                }
            }
            if snap_cut.is_none() && copying.is_some() && !(log.db.is_dirty() && last_start.elapsed() >= gap) {
                tokio::select! {
                    _ = log.kick.notified() => {}
                    _ = tokio::time::sleep(Duration::from_millis(2)) => {}
                }
                continue;
            }
        } else if log.db.is_dirty() || stopping {
            let since = last_start.elapsed();
            if since < gap && !stopping {
                tokio::time::sleep(gap - since).await;
            }
        }
        last_start = Instant::now();

        let finished_snapshot = snap_cut.is_some();
        let cut = match snap_cut.take() {
            Some(c) => Ok(c),
            None => log.db.cut(),
        };
        let (patch, batch) = match cut {
            Ok(c) => c,
            Err(e) => {
                log.fence(format!("can't read metadata changes: {e}"));
                break;
            }
        };
        let mut written = None;
        if patch.is_empty() {
            log.durable.send_modify(|d| d.batch = d.batch.max(batch));
        } else {
            let body = encode(next_seq, "commit", &cfg.node, &patch);
            let len = body.len() as u64;
            match log.put_entry(next_seq, body).await {
                Ok(true) => {
                    {
                        let mut s = log.stats.lock().unwrap();
                        s.seq = next_seq;
                        s.commits += 1;
                        s.commit_bytes += len;
                    }
                    log.wal.lock().unwrap().insert(next_seq);
                    log.durable.send_modify(|d| d.batch = d.batch.max(batch));
                    written = Some(next_seq);
                    next_seq += 1;
                    last_check = Instant::now();
                    since_entries += 1;
                    since_bytes += len;
                }
                Ok(false) => {
                    log.fence(format!("another node took over the metadata log at entry {next_seq}"));
                    break;
                }
                Err(e) => {
                    log.fence(format!("can't write the metadata log: {e}"));
                    break;
                }
            }
        }

        if finished_snapshot && let Some(file) = copying.take() {
            // The copy is the database after this entry (or the last one, if nothing changed).
            let at = written.unwrap_or(next_seq - 1);
            since_entries = 0;
            since_bytes = 0;
            let log2 = log.clone();
            snapshot_task = Some(tokio::spawn(async move {
                if let Err(e) = log2.upload_snapshot(file, at).await {
                    warn!(error = %e, seq = at, "metadata snapshot upload failed");
                }
            }));
        }

        if snapshot_due && copying.is_none() && !finished_snapshot {
            log.force_snapshot.store(false, Ordering::SeqCst);
            let file = cfg.dir.join("snapshots").join(format!("{next_seq}.db"));
            match log.db.snapshot_begin(&file) {
                Ok(()) => copying = Some(file),
                Err(e) => warn!(error = %e, "can't start a metadata snapshot"),
            }
        }

        if log.stopping.load(Ordering::SeqCst) && !log.db.is_dirty() && copying.is_none() {
            if let Some(t) = snapshot_task.take() {
                let _ = t.await;
            }
            log.fence("closed".into());
            break;
        }
    }
}
