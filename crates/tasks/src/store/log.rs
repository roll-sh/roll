//! The log store: roll tasks' own database. No database service to pay for, only S3.
//!
//! One leader node owns a cell's data: one SQLite file per tenant ([`SqliteStore`]), all made
//! durable together by shipping their net row changes (SQLite session patchsets) to object
//! storage as one append-only log, one object per group commit:
//!
//! ```text
//! <prefix>/wal/00000000000000000042                  commit 42: changes of any tenants
//! <prefix>/snap/<tenant>/00000000000000000040.db.zst  a tenant's database after commit 40
//! <prefix>/meta/pruned                                log entries up to here are gone
//! ```
//!
//! - **Durability.** A write returns once the commit holding it is in S3 (multi-AZ). Commits
//!   group everything written while the previous one uploaded, by every tenant of the cell, and
//!   start at most every `commit_ms`: the S3 bill follows the commit rate, which is capped,
//!   not the message rate or the number of tenants.
//! - **Relaxed writes.** Work the system can redo (delivery outcomes, leases, workflow steps:
//!   anything inside [`super::relaxed`]) doesn't wait. It rides along with the next commit, or
//!   with a lazy one at most `relaxed_ms` later. Losing it in a crash means a redelivery, which
//!   at-least-once delivery allows. Recovered state is always an exact prefix of history.
//! - **Recovery.** A node rebuilds every tenant from its newest snapshot plus the log after it.
//! - **Fencing.** Commits are written create-only (`If-None-Match: *`). A new leader writes a
//!   takeover entry at the next sequence number before serving, so a deposed leader's next
//!   commit fails, and it stops. Every commit it acknowledged before that sits below the takeover
//!   entry and was replayed. Two leaders can never both commit.
//! - **Snapshots.** A tenant is copied a few pages at a time while serving, cut at an exact
//!   commit, compressed and uploaded: when its changes add up, or when the log grows long and
//!   it's one of the tenants holding it back. Log entries every tenant has a snapshot past are
//!   deleted, one round late, so each tenant's previous snapshot stays usable as a fallback.

use super::sqlite::SqliteStore;
use super::*;
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutPayload};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};
use tracing::{info, warn};

const MAGIC: &[u8] = b"RTLOG2\n";

/// The tenant a single-tenant deployment uses (and `/v1/tasks` on a node).
pub const DEFAULT_TENANT: &str = "default";

#[derive(Clone)]
pub struct LogConfig {
    pub objects: Arc<dyn ObjectStore>,
    /// Key prefix inside the bucket, e.g. `tasks`.
    pub prefix: String,
    /// Local working directory for the databases.
    pub dir: PathBuf,
    /// Due-index shards of the default tenant …
    pub shards: u32,
    /// … and of every other tenant (one worker per cell: a couple of lanes is plenty).
    pub tenant_shards: u32,
    /// Written into takeover entries, for the record.
    pub node: String,
    /// Minimum time between the starts of two commits. Caps the S3 PUT rate at 1000 /
    /// `commit_ms` per second whatever the traffic; durable writes wait up to this long.
    pub commit_ms: u64,
    /// Relaxed writes are committed at most this long after they happen.
    pub relaxed_ms: u64,
    /// Snapshot a tenant once its changes since the last snapshot reach this many bytes (or a
    /// quarter of its size, if larger), and compact once the log holds this many bytes …
    pub snapshot_bytes: u64,
    /// … or this many entries (bounds recovery time).
    pub snapshot_commits: u64,
    /// Pages copied per snapshot step (4 KiB each); a tenant pauses for one step at a time.
    pub snapshot_step_pages: i32,
    /// Page cache per tenant database.
    pub tenant_cache_kib: i64,
}

impl LogConfig {
    pub fn new(objects: Arc<dyn ObjectStore>, prefix: impl Into<String>, dir: PathBuf) -> Self {
        Self {
            objects,
            prefix: prefix.into(),
            dir,
            shards: 4,
            tenant_shards: 2,
            node: crate::ids::new_id("node"),
            commit_ms: 100,
            relaxed_ms: 5_000,
            snapshot_bytes: 64 << 20,
            snapshot_commits: 20_000,
            snapshot_step_pages: 256,
            tenant_cache_kib: 512,
        }
    }
}

/// Where the log lives, from a URL: `s3://bucket/prefix`, `file:///dir` or `memory://`.
/// S3 settings come from the usual `AWS_*` environment (`AWS_ENDPOINT_URL` for S3-compatible
/// stores); `s3express://bucket--usw2-az1--x-s3/prefix` uses an Express One Zone directory bucket.
pub fn objects_from_url(url: &str) -> Result<(Arc<dyn ObjectStore>, String)> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| anyhow::anyhow!("log url needs a scheme: {url}"))?;
    let (root, prefix) = match rest.split_once('/') {
        Some((root, prefix)) => (root, prefix.trim_matches('/')),
        None => (rest, ""),
    };
    let prefix = if prefix.is_empty() { "tasks".to_string() } else { prefix.to_string() };
    Ok(match scheme {
        "memory" => (Arc::new(object_store::memory::InMemory::new()), prefix),
        "file" => {
            std::fs::create_dir_all(rest)?;
            (Arc::new(object_store::local::LocalFileSystem::new_with_prefix(rest)?), "tasks".into())
        }
        "s3" | "s3express" => {
            let mut b = object_store::aws::AmazonS3Builder::from_env().with_bucket_name(root);
            if scheme == "s3express" {
                b = b.with_s3_express(true);
            }
            if let Ok(e) = std::env::var("AWS_ENDPOINT_URL_S3").or_else(|_| std::env::var("AWS_ENDPOINT_URL")) {
                b = b.with_endpoint(&e).with_allow_http(e.starts_with("http://")).with_virtual_hosted_style_request(false);
            }
            (Arc::new(b.build()?), prefix)
        }
        other => anyhow::bail!("unsupported log url scheme {other:?}"),
    })
}

/// Tenant ids: 1–63 of `a-z`, `0-9` and inner `-`.
pub fn valid_tenant_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 63
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-')
        && !id.ends_with('-')
}

/// A write this node can no longer make durable: another node leads the log now (or this one
/// closed, or the tenant was deleted). Nothing of it was committed, so it's safe to retry on the
/// current leader.
#[derive(Debug, thiserror::Error)]
#[error("log store unavailable: {0}")]
pub struct Unavailable(pub String);

/// `create_tenant` with an id that's taken.
#[derive(Debug, thiserror::Error)]
#[error("tenant {0} already exists")]
pub struct TenantExists(pub String);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Durable {
    /// Every batch up to this one is in the log.
    batch: u64,
    /// Set once this node lost the log to another leader (or can't write it): no more commits.
    fenced: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct LogStats {
    /// The last committed log entry.
    pub seq: u64,
    /// Log objects written (each a PUT) since start.
    pub commits: u64,
    pub commit_bytes: u64,
    pub snapshots: u64,
    pub last_snapshot_seq: u64,
    /// Entries up to here are deleted.
    pub pruned: u64,
    pub tenants: usize,
    pub db_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Changes = 0,
    Create = 1,
    Drop = 2,
}

struct Section {
    tenant: String,
    op: Op,
    data: Vec<u8>,
}

fn wal_path(prefix: &str, seq: u64) -> ObjPath {
    ObjPath::from(format!("{prefix}/wal/{seq:020}"))
}

fn snap_path(prefix: &str, tenant: &str, seq: u64) -> ObjPath {
    ObjPath::from(format!("{prefix}/snap/{tenant}/{seq:020}.db.zst"))
}

fn pruned_path(prefix: &str) -> ObjPath {
    ObjPath::from(format!("{prefix}/meta/pruned"))
}

fn seq_of(p: &ObjPath) -> Option<u64> {
    p.filename()?.split('.').next()?.parse().ok()
}

/// `(tenant, seq)` of a snapshot path.
fn snap_of(p: &ObjPath) -> Option<(String, u64)> {
    let parts: Vec<_> = p.parts().collect();
    let tenant = parts.get(parts.len().checked_sub(2)?)?.as_ref().to_string();
    Some((tenant, seq_of(p)?))
}

fn encode(seq: u64, kind: &str, node: &str, sections: &[Section]) -> Vec<u8> {
    let meta = serde_json::to_vec(&serde_json::json!({ "seq": seq, "kind": kind, "node": node, "at": now_ms() })).unwrap_or_default();
    let size: usize = sections.iter().map(|s| 7 + s.tenant.len() + s.data.len()).sum();
    let mut out = Vec::with_capacity(MAGIC.len() + 4 + meta.len() + size);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(meta.len() as u32).to_be_bytes());
    out.extend_from_slice(&meta);
    for s in sections {
        out.extend_from_slice(&(s.tenant.len() as u16).to_be_bytes());
        out.extend_from_slice(s.tenant.as_bytes());
        out.push(s.op as u8);
        out.extend_from_slice(&(s.data.len() as u32).to_be_bytes());
        out.extend_from_slice(&s.data);
    }
    out
}

fn decode(entry: &[u8]) -> Result<Vec<Section>> {
    fn take<'a>(b: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
        anyhow::ensure!(b.len() >= n, "truncated log entry");
        let (head, rest) = b.split_at(n);
        *b = rest;
        Ok(head)
    }
    let mut b = entry.strip_prefix(MAGIC).ok_or_else(|| anyhow::anyhow!("not a roll tasks log entry"))?;
    let n = u32::from_be_bytes(take(&mut b, 4)?.try_into()?) as usize;
    take(&mut b, n)?;
    let mut out = Vec::new();
    while !b.is_empty() {
        let n = u16::from_be_bytes(take(&mut b, 2)?.try_into()?) as usize;
        let tenant = std::str::from_utf8(take(&mut b, n)?)?.to_string();
        let op = match take(&mut b, 1)?[0] {
            0 => Op::Changes,
            1 => Op::Create,
            2 => Op::Drop,
            other => anyhow::bail!("unknown log section {other}"),
        };
        let n = u32::from_be_bytes(take(&mut b, 4)?.try_into()?) as usize;
        out.push(Section { tenant, op, data: take(&mut b, n)?.to_vec() });
    }
    Ok(out)
}

struct Tenant {
    id: String,
    db: SqliteStore,
    /// Written since the last cut.
    dirty: AtomicBool,
    dropped: AtomicBool,
    uploading: AtomicBool,
    /// Newest uploaded snapshot and the one before it (0: none).
    snap: AtomicU64,
    prev_snap: AtomicU64,
    /// The last log entry with a section of this tenant.
    last_section: AtomicU64,
    /// Bytes logged since the last snapshot.
    since_snapshot: AtomicU64,
    /// This tenant's share of object writes not yet metered, in millionths: a commit is split
    /// between the tenants in it, a snapshot is its tenant's.
    storage_micros: AtomicU64,
}

impl Tenant {
    fn new(id: String, db: SqliteStore, snap: u64, prev_snap: u64, last_section: u64) -> Self {
        Self {
            id,
            db,
            dirty: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
            uploading: AtomicBool::new(false),
            snap: AtomicU64::new(snap),
            prev_snap: AtomicU64::new(prev_snap),
            last_section: AtomicU64::new(last_section),
            since_snapshot: AtomicU64::new(0),
            storage_micros: AtomicU64::new(0),
        }
    }

    fn take_storage_writes(&self) -> u64 {
        let whole = self.storage_micros.load(Ordering::Relaxed) / 1_000_000;
        self.storage_micros.fetch_sub(whole * 1_000_000, Ordering::Relaxed);
        whole
    }

    /// Log entries this tenant still needs: everything after its newest snapshot, if it
    /// changed since.
    fn needs_after(&self) -> u64 {
        let snap = self.snap.load(Ordering::SeqCst);
        if self.last_section.load(Ordering::SeqCst) > snap { snap } else { u64::MAX }
    }
}

fn db_path(dir: &Path, tenant: &str) -> PathBuf {
    dir.join("tenants").join(format!("{tenant}.db"))
}

fn remove_db_files(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

fn open_tenant_db(cfg: &LogConfig, tenant: &str, fresh: bool) -> Result<SqliteStore> {
    let path = db_path(&cfg.dir, tenant);
    if fresh {
        remove_db_files(&path);
    }
    let shards = if tenant == DEFAULT_TENANT { cfg.shards } else { cfg.tenant_shards };
    let db = SqliteStore::open_unsynced(&path, shards)?;
    db.set_cache_kib(cfg.tenant_cache_kib)?;
    Ok(db)
}

/// A cell's log: its tenants, the committer, snapshots and pruning.
pub struct Log {
    cfg: LogConfig,
    tenants: RwLock<BTreeMap<String, Arc<Tenant>>>,
    /// Tenant creations and deletions for the next commit. Locked while the committer takes
    /// them together with the tenant list, so a new tenant's changes never precede its creation.
    ops: Mutex<Vec<(String, Op)>>,
    /// State that only means something on this node (worker heartbeats: the leader is the
    /// cell's only worker). Never logged, so an idle cell writes nothing.
    local: SqliteStore,
    /// Log entries after these must stay (deleted tenants whose snapshots are being removed).
    holds: Mutex<BTreeMap<u64, u64>>,
    next_hold: AtomicU64,
    reevaluate: AtomicBool,
    /// Something was written since the committer last looked.
    changed: AtomicBool,
    /// The batch currently gathering writes.
    open_batch: AtomicU64,
    durable: watch::Sender<Durable>,
    /// Something was written (strict or relaxed).
    kick: Notify,
    /// A strict write is waiting.
    urgent: AtomicBool,
    urgent_kick: Notify,
    stopping: AtomicBool,
    pruned: AtomicU64,
    /// Object writes not yet handed to metering (the whole cell's).
    metered: AtomicU64,
    stats: Mutex<LogStats>,
}

/// One tenant's view of the log: a [`Store`].
pub struct LogStore {
    log: Arc<Log>,
    t: Arc<Tenant>,
}

/// Rebuilding tenants from snapshots and the log.
struct Replay {
    cfg: LogConfig,
    tenants: BTreeMap<String, (SqliteStore, u64, u64, u64)>, // db, snapshot, previous snapshot, last section
    gone: BTreeSet<String>,
}

impl Replay {
    fn apply(&mut self, seq: u64, sections: Vec<Section>) -> Result<()> {
        for s in sections {
            let base = self.tenants.get(&s.tenant).map(|t| t.1).unwrap_or(0);
            if seq <= base {
                // Already in the tenant's snapshot.
                continue;
            }
            match s.op {
                Op::Create => {
                    if self.tenants.remove(&s.tenant).is_some() {
                        remove_db_files(&db_path(&self.cfg.dir, &s.tenant));
                    }
                    let db = open_tenant_db(&self.cfg, &s.tenant, true)?;
                    self.gone.remove(&s.tenant);
                    self.tenants.insert(s.tenant, (db, 0, 0, seq));
                }
                Op::Drop => {
                    if self.tenants.remove(&s.tenant).is_some() {
                        remove_db_files(&db_path(&self.cfg.dir, &s.tenant));
                    }
                    self.gone.insert(s.tenant);
                }
                Op::Changes => match self.tenants.get_mut(&s.tenant) {
                    Some(t) => {
                        t.0.apply_changes(&s.data)?;
                        t.3 = seq;
                    }
                    // A deleted tenant whose snapshots are already gone.
                    None => tracing::debug!(tenant = s.tenant, seq, "changes of a deleted tenant"),
                },
            }
        }
        Ok(())
    }
}

impl Log {
    /// Rebuild every tenant from the log, take over as its writer, and start committing.
    pub async fn open(cfg: LogConfig) -> Result<Arc<Self>> {
        let _ = std::fs::remove_dir_all(cfg.dir.join("tenants"));
        let _ = std::fs::remove_dir_all(cfg.dir.join("snapshots"));
        std::fs::create_dir_all(cfg.dir.join("tenants"))?;
        std::fs::create_dir_all(cfg.dir.join("snapshots"))?;
        let objects = cfg.objects.clone();
        let prefix = cfg.prefix.clone();

        let pruned = match objects.get(&pruned_path(&prefix)).await {
            Ok(r) => serde_json::from_slice::<Value>(&r.bytes().await?)?["through"].as_u64().unwrap_or(0),
            Err(object_store::Error::NotFound { .. }) => 0,
            Err(e) => return Err(e.into()),
        };

        // Each tenant's newest readable snapshot (the one before it is the fallback).
        let mut snaps: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        let listed: Vec<_> = objects.list(Some(&ObjPath::from(format!("{prefix}/snap")))).try_collect().await?;
        for m in listed {
            if let Some((tenant, seq)) = snap_of(&m.location) {
                snaps.entry(tenant).or_default().push(seq);
            }
        }
        let loaded: Vec<Result<(String, SqliteStore, u64, u64)>> = futures::stream::iter(snaps.clone())
            .map(|(tenant, mut seqs)| {
                let (cfg, objects, prefix) = (cfg.clone(), objects.clone(), prefix.clone());
                async move {
                    seqs.sort_unstable_by(|a, b| b.cmp(a));
                    let path = db_path(&cfg.dir, &tenant);
                    for (i, seq) in seqs.iter().take(2).enumerate() {
                        match download_snapshot(&*objects, &snap_path(&prefix, &tenant, *seq), &path).await {
                            Ok(()) => {
                                let db = open_tenant_db(&cfg, &tenant, false)?;
                                let prev = if i == 0 { seqs.get(1).copied().unwrap_or(0) } else { 0 };
                                return Ok((tenant, db, *seq, prev));
                            }
                            Err(e) => {
                                warn!(error = %e, tenant, seq, "unreadable snapshot; trying the previous one");
                                remove_db_files(&path);
                            }
                        }
                    }
                    anyhow::bail!("tenant {tenant}: no readable snapshot")
                }
            })
            .buffer_unordered(16)
            .collect()
            .await;
        let mut replay = Replay { cfg: cfg.clone(), tenants: BTreeMap::new(), gone: BTreeSet::new() };
        for r in loaded {
            let (tenant, db, snap, prev) = r?;
            replay.tenants.insert(tenant, (db, snap, prev, snap));
        }

        // The log after the pruned point, in order, without gaps.
        let mut seqs: Vec<u64> = objects
            .list_with_offset(Some(&ObjPath::from(format!("{prefix}/wal"))), &wal_path(&prefix, pruned))
            .map_ok(|m| seq_of(&m.location))
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .flatten()
            .filter(|s| *s > pruned)
            .collect();
        seqs.sort_unstable();
        for (i, s) in seqs.iter().enumerate() {
            let want = pruned + 1 + i as u64;
            anyhow::ensure!(*s == want, "log gap: expected entry {want}, found {s}");
        }
        let mut wal = VecDeque::new();
        let mut entries = futures::stream::iter(seqs.clone())
            .map(|s| {
                let (objects, prefix) = (objects.clone(), prefix.clone());
                async move { Ok::<_, anyhow::Error>((s, objects.get(&wal_path(&prefix, s)).await?.bytes().await?)) }
            })
            .buffered(64);
        let mut last = pruned;
        while let Some(entry) = entries.next().await {
            let (seq, bytes) = entry?;
            replay.apply(seq, decode(&bytes)?)?;
            wal.push_back((seq, bytes.len() as u64));
            last = seq;
        }

        // Take over: claim the next entry. Anything a previous writer slipped in first gets
        // replayed, and from here on its commits fail.
        let mut seq = last + 1;
        loop {
            let marker = encode(seq, "takeover", &cfg.node, &[]);
            match objects.put_opts(&wal_path(&prefix, seq), PutPayload::from(marker), PutMode::Create.into()).await {
                Ok(_) => break,
                Err(object_store::Error::AlreadyExists { .. }) => {
                    let bytes = objects.get(&wal_path(&prefix, seq)).await?.bytes().await?;
                    replay.apply(seq, decode(&bytes)?)?;
                    wal.push_back((seq, bytes.len() as u64));
                    seq += 1;
                }
                Err(e) => return Err(e.into()),
            }
        }
        wal.push_back((seq, 0));

        // Snapshots of deleted tenants are garbage now.
        let stale: Vec<String> = snaps.keys().filter(|t| !replay.tenants.contains_key(*t)).cloned().collect();
        if !stale.is_empty() {
            let (objects, prefix) = (objects.clone(), prefix.clone());
            tokio::spawn(async move {
                for t in stale {
                    if let Err(e) = delete_prefix(&*objects, &format!("{prefix}/snap/{t}")).await {
                        warn!(error = %e, tenant = t, "removing a deleted tenant's snapshots");
                    }
                }
            });
        }

        let mut tenants = BTreeMap::new();
        for (id, (db, snap, prev, last_section)) in replay.tenants {
            db.record_changes()?;
            tenants.insert(id.clone(), Arc::new(Tenant::new(id, db, snap, prev, last_section)));
        }
        info!(tenants = tenants.len(), pruned, replayed = seq - 1 - pruned, seq, "log store open");
        let log = Arc::new(Self {
            local: SqliteStore::memory(1)?,
            tenants: RwLock::new(tenants),
            ops: Mutex::new(Vec::new()),
            holds: Mutex::new(BTreeMap::new()),
            next_hold: AtomicU64::new(0),
            reevaluate: AtomicBool::new(false),
            changed: AtomicBool::new(false),
            open_batch: AtomicU64::new(1),
            durable: watch::channel(Durable::default()).0,
            kick: Notify::new(),
            urgent: AtomicBool::new(false),
            urgent_kick: Notify::new(),
            stopping: AtomicBool::new(false),
            pruned: AtomicU64::new(pruned),
            metered: AtomicU64::new(0),
            stats: Mutex::new(LogStats { seq, pruned, ..Default::default() }),
            cfg,
        });
        tokio::spawn(committer(log.clone(), seq + 1, wal));
        Ok(log)
    }

    pub fn config(&self) -> &LogConfig {
        &self.cfg
    }

    pub fn tenant(self: &Arc<Self>, id: &str) -> Option<Arc<LogStore>> {
        let t = self.tenants.read().unwrap().get(id).cloned()?;
        Some(Arc::new(LogStore { log: self.clone(), t }))
    }

    pub fn tenant_ids(&self) -> Vec<String> {
        self.tenants.read().unwrap().keys().cloned().collect()
    }

    /// Create an empty tenant; returns once its creation is durable.
    pub async fn create_tenant(self: &Arc<Self>, id: &str) -> Result<Arc<LogStore>> {
        anyhow::ensure!(valid_tenant_id(id), "tenant ids are 1-63 of a-z, 0-9 and inner '-'");
        self.check_open()?;
        let t = {
            let mut ops = self.ops.lock().unwrap();
            let mut tenants = self.tenants.write().unwrap();
            // Checked before the files are touched: opening fresh clears them.
            if tenants.contains_key(id) {
                return Err(TenantExists(id.to_string()).into());
            }
            let db = open_tenant_db(&self.cfg, id, true)?;
            db.record_changes()?;
            let t = Arc::new(Tenant::new(id.to_string(), db, 0, 0, 0));
            tenants.insert(id.to_string(), t.clone());
            ops.push((id.to_string(), Op::Create));
            t
        };
        self.written(None).await?;
        Ok(Arc::new(LogStore { log: self.clone(), t }))
    }

    /// Delete a tenant and its data; `false` if there was none. Stop its engine first.
    pub async fn drop_tenant(self: &Arc<Self>, id: &str) -> Result<bool> {
        self.check_open()?;
        // Keep the deletion in the log until the snapshots are gone, so a crash in between
        // still deletes it on recovery.
        let hold = self.next_hold.fetch_add(1, Ordering::SeqCst);
        let at = self.stats.lock().unwrap().seq;
        self.holds.lock().unwrap().insert(hold, at);
        let result = async {
            let t = {
                let mut ops = self.ops.lock().unwrap();
                let Some(t) = self.tenants.write().unwrap().remove(id) else { return Ok(false) };
                t.dropped.store(true, Ordering::SeqCst);
                ops.push((id.to_string(), Op::Drop));
                t
            };
            self.written(None).await?;
            delete_prefix(&*self.cfg.objects, &format!("{}/snap/{id}", self.cfg.prefix)).await?;
            drop(t);
            remove_db_files(&db_path(&self.cfg.dir, id));
            Ok(true)
        }
        .await;
        self.holds.lock().unwrap().remove(&hold);
        self.reevaluate.store(true, Ordering::SeqCst);
        result
    }

    pub fn stats(&self) -> LogStats {
        let mut s = self.stats.lock().unwrap().clone();
        let tenants = self.tenants.read().unwrap();
        s.tenants = tenants.len();
        s.db_bytes = tenants.values().map(|t| t.db.size_bytes()).sum();
        s.pruned = self.pruned.load(Ordering::SeqCst);
        s
    }

    /// The whole cell's object-storage writes since the last call (commits, snapshots, prune
    /// markers).
    pub fn take_storage_writes(&self) -> u64 {
        self.metered.swap(0, Ordering::Relaxed)
    }

    /// Resolves (with the reason) once this node can no longer commit.
    pub async fn fenced(&self) -> String {
        let mut rx = self.durable.subscribe();
        loop {
            if let Some(reason) = rx.borrow_and_update().fenced.clone() {
                return reason;
            }
            if rx.changed().await.is_err() {
                return "log store closed".into();
            }
        }
    }

    /// Commit everything written so far and stop. Writes after this fail.
    pub async fn close(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.urgent.store(true, Ordering::SeqCst);
        self.urgent_kick.notify_one();
        self.kick.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(60), self.fenced()).await;
    }

    fn check_open(&self) -> Result<()> {
        match self.durable.borrow().fenced.clone() {
            Some(reason) => Err(Unavailable(reason).into()),
            None => Ok(()),
        }
    }

    /// After a write: wait until it's in the log (or, when relaxed, just schedule it).
    async fn written(&self, t: Option<&Tenant>) -> Result<()> {
        self.check_open()?;
        if let Some(t) = t {
            if t.dropped.load(Ordering::SeqCst) {
                return Err(Unavailable(format!("tenant {} was deleted", t.id)).into());
            }
            t.dirty.store(true, Ordering::SeqCst);
        }
        self.changed.store(true, Ordering::SeqCst);
        if is_relaxed() {
            self.kick.notify_one();
            return Ok(());
        }
        let batch = self.open_batch.load(Ordering::SeqCst);
        self.urgent.store(true, Ordering::SeqCst);
        self.urgent_kick.notify_one();
        self.kick.notify_one();
        let mut rx = self.durable.subscribe();
        loop {
            {
                let d = rx.borrow_and_update();
                if let Some(reason) = &d.fenced {
                    return Err(Unavailable(reason.clone()).into());
                }
                if d.batch >= batch {
                    return Ok(());
                }
            }
            rx.changed().await.map_err(|_| anyhow::anyhow!("log store closed"))?;
        }
    }

    fn fence(&self, reason: String) {
        if reason != "closed" {
            warn!(%reason, "log store fenced");
        }
        self.durable.send_modify(|d| d.fenced = Some(reason));
    }

    /// Upload one entry, retrying transient errors. `Ok(false)`: the slot was taken (fenced).
    async fn put_entry(&self, seq: u64, body: Vec<u8>) -> Result<bool> {
        let path = wal_path(&self.cfg.prefix, seq);
        let payload = PutPayload::from(Bytes::from(body));
        let mut delay = 50u64;
        let started = Instant::now();
        let mut retried = false;
        loop {
            match self.cfg.objects.put_opts(&path, payload.clone(), PutMode::Create.into()).await {
                Ok(_) => return Ok(true),
                // A retry can find the entry an earlier, seemingly failed attempt wrote.
                Err(object_store::Error::AlreadyExists { .. }) if retried => {
                    let found = self.cfg.objects.get(&path).await?.bytes().await?;
                    return Ok(found.iter().eq(payload.iter().flat_map(|b| b.iter())));
                }
                Err(object_store::Error::AlreadyExists { .. }) => return Ok(false),
                Err(e) if started.elapsed() < Duration::from_secs(60) => {
                    retried = true;
                    warn!(error = %e, seq, "log commit failed; retrying");
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    delay = (delay * 2).min(2_000);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Commit the sections as entry `seq`. `Ok(Some(bytes))` if an entry was written.
    async fn commit(&self, seq: u64, sections: &[Section], batch: u64) -> std::result::Result<Option<u64>, String> {
        if sections.is_empty() {
            self.durable.send_modify(|d| d.batch = d.batch.max(batch));
            return Ok(None);
        }
        let body = encode(seq, "commit", &self.cfg.node, sections);
        let len = body.len() as u64;
        match self.put_entry(seq, body).await {
            Ok(true) => {
                self.metered.fetch_add(1, Ordering::Relaxed);
                {
                    let mut s = self.stats.lock().unwrap();
                    s.seq = seq;
                    s.commits += 1;
                    s.commit_bytes += len;
                }
                self.durable.send_modify(|d| d.batch = d.batch.max(batch));
                Ok(Some(len))
            }
            Ok(false) => Err(format!("another node took over the log at entry {seq}")),
            Err(e) => Err(format!("can't write the log: {e}")),
        }
    }

    /// Entries every tenant (and every hold) can do without.
    fn prune_point(&self, last: u64) -> u64 {
        let mut p = last;
        for t in self.tenants.read().unwrap().values() {
            p = p.min(t.needs_after());
        }
        for h in self.holds.lock().unwrap().values() {
            p = p.min(*h);
        }
        p
    }

    /// The tenant to snapshot next, if any: a tenant with many changes, or (when the log is
    /// long) the one holding it back the most.
    fn pick_snapshot(&self, wal_entries: usize, wal_bytes: u64) -> Option<Arc<Tenant>> {
        let tenants = self.tenants.read().unwrap();
        let free = |t: &&Arc<Tenant>| !t.uploading.load(Ordering::SeqCst);
        let big = tenants.values().filter(free).find(|t| {
            let since = t.since_snapshot.load(Ordering::Relaxed);
            since >= self.cfg.snapshot_bytes && since >= t.db.size_bytes() / 4
        });
        if big.is_some() {
            return big.cloned();
        }
        if wal_entries as u64 > self.cfg.snapshot_commits || wal_bytes > self.cfg.snapshot_bytes {
            return tenants.values().filter(free).filter(|t| t.needs_after() != u64::MAX).min_by_key(|t| t.needs_after()).cloned();
        }
        None
    }
}

impl LogStore {
    /// A single-tenant log store: the cell's default tenant (created if new).
    pub async fn open(cfg: LogConfig) -> Result<Arc<Self>> {
        let log = Log::open(cfg).await?;
        match log.tenant(DEFAULT_TENANT) {
            Some(t) => Ok(t),
            None => log.create_tenant(DEFAULT_TENANT).await,
        }
    }

    pub fn log(&self) -> &Arc<Log> {
        &self.log
    }

    pub fn tenant_id(&self) -> &str {
        &self.t.id
    }

    pub fn stats(&self) -> LogStats {
        self.log.stats()
    }

    pub async fn fenced(&self) -> String {
        self.log.fenced().await
    }

    pub async fn close(&self) {
        self.log.close().await
    }

    async fn written(&self) -> Result<()> {
        self.log.written(Some(&self.t)).await
    }

    fn local_kind(&self, kind: &str) -> String {
        format!("{kind}@{}", self.t.id)
    }
}

async fn download_snapshot(objects: &dyn ObjectStore, from: &ObjPath, to: &Path) -> Result<()> {
    let zst = to.with_extension("db.zst");
    {
        let mut file = tokio::fs::File::create(&zst).await?;
        let mut stream = objects.get(from).await?.into_stream();
        while let Some(chunk) = stream.next().await {
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk?).await?;
        }
        tokio::io::AsyncWriteExt::flush(&mut file).await?;
    }
    let (zst2, to2) = (zst.clone(), to.to_path_buf());
    tokio::task::spawn_blocking(move || -> Result<()> {
        let input = std::fs::File::open(&zst2)?;
        let mut output = std::fs::File::create(&to2)?;
        zstd::stream::copy_decode(input, &mut output)?;
        Ok(())
    })
    .await??;
    let _ = tokio::fs::remove_file(&zst).await;
    Ok(())
}

/// Compress and upload a snapshot file; returns the requests it took.
async fn upload_snapshot(objects: Arc<dyn ObjectStore>, file: PathBuf, to: ObjPath) -> Result<(u64, u64)> {
    let zst = file.with_extension("db.zst");
    let zst2 = zst.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let input = std::fs::File::open(&file)?;
        let output = std::fs::File::create(&zst2)?;
        zstd::stream::copy_encode(input, output, 3)?;
        let _ = std::fs::remove_file(&file);
        Ok(())
    })
    .await??;
    let size = tokio::fs::metadata(&zst).await?.len();
    let mut writer = object_store::buffered::BufWriter::new(objects, to);
    let mut input = tokio::fs::File::open(&zst).await?;
    tokio::io::copy(&mut input, &mut writer).await?;
    tokio::io::AsyncWriteExt::shutdown(&mut writer).await?;
    let _ = tokio::fs::remove_file(&zst).await;
    // Small files go up in one PUT; larger ones as a multipart upload.
    let requests = if size < (10 << 20) { 1 } else { 2 + size.div_ceil(10 << 20) };
    Ok((size, requests))
}

async fn delete_prefix(objects: &dyn ObjectStore, prefix: &str) -> Result<()> {
    let paths = objects.list(Some(&ObjPath::from(prefix))).map_ok(|m| m.location).boxed();
    objects.delete_stream(paths).try_collect::<Vec<_>>().await?;
    Ok(())
}

/// Delete log entries up to `through`, recording the point first.
async fn prune(log: Arc<Log>, through: u64) -> Result<()> {
    let from = log.pruned.load(Ordering::SeqCst) + 1;
    if through < from {
        return Ok(());
    }
    let marker = serde_json::to_vec(&serde_json::json!({ "through": through, "at": now_ms() }))?;
    log.cfg.objects.put(&pruned_path(&log.cfg.prefix), PutPayload::from(marker)).await?;
    log.metered.fetch_add(1, Ordering::Relaxed);
    let prefix = log.cfg.prefix.clone();
    let paths = futures::stream::iter((from..=through).map(move |s| Ok(wal_path(&prefix, s)))).boxed();
    let deleted: Vec<_> = log.cfg.objects.delete_stream(paths).collect().await;
    for d in deleted {
        if let Err(e) = d
            && !matches!(e, object_store::Error::NotFound { .. })
        {
            return Err(e.into());
        }
    }
    log.pruned.store(through, Ordering::SeqCst);
    Ok(())
}

async fn upload_tenant_snapshot(log: Arc<Log>, t: Arc<Tenant>, file: PathBuf, at: u64) {
    let prefix = log.cfg.prefix.clone();
    match upload_snapshot(log.cfg.objects.clone(), file, snap_path(&prefix, &t.id, at)).await {
        Ok((size, requests)) => {
            log.metered.fetch_add(requests, Ordering::Relaxed);
            t.storage_micros.fetch_add(requests * 1_000_000, Ordering::Relaxed);
            tracing::debug!(tenant = t.id, seq = at, bytes = size, "snapshot uploaded");
            let old = t.prev_snap.swap(t.snap.swap(at, Ordering::SeqCst), Ordering::SeqCst);
            // Keep two per tenant: the newest and its fallback.
            if old > 0 {
                let _ = log.cfg.objects.delete(&snap_path(&prefix, &t.id, old)).await;
            }
            if t.dropped.load(Ordering::SeqCst) {
                let _ = delete_prefix(&*log.cfg.objects, &format!("{prefix}/snap/{}", t.id)).await;
            }
            let mut st = log.stats.lock().unwrap();
            st.snapshots += 1;
            st.last_snapshot_seq = st.last_snapshot_seq.max(at);
        }
        Err(e) => warn!(error = %e, tenant = t.id, "snapshot upload failed"),
    }
    t.uploading.store(false, Ordering::SeqCst);
}

/// Wait for a durable write, or let relaxed ones gather for up to `relaxed_ms`.
async fn wait_for_work(log: &Log) {
    let relaxed = Duration::from_millis(log.cfg.relaxed_ms);
    loop {
        if log.urgent.load(Ordering::SeqCst) || log.stopping.load(Ordering::SeqCst) {
            return;
        }
        if log.changed.load(Ordering::SeqCst) {
            tokio::select! {
                _ = log.urgent_kick.notified() => {}
                _ = tokio::time::sleep(relaxed) => {}
            }
            return;
        }
        tokio::select! {
            _ = log.kick.notified() => {}
            _ = log.urgent_kick.notified() => {}
            _ = tokio::time::sleep(relaxed) => {}
        }
    }
}

/// The single writer of the log: groups writes into commits, takes snapshots, prunes.
async fn committer(log: Arc<Log>, mut next_seq: u64, mut wal: VecDeque<(u64, u64)>) {
    let cfg = log.cfg.clone();
    let gap = Duration::from_millis(cfg.commit_ms);
    let mut wal_bytes: u64 = wal.iter().map(|(_, b)| b).sum();
    let mut copying: Option<(Arc<Tenant>, PathBuf)> = None;
    let mut uploads = tokio::task::JoinSet::new();
    let mut pruning: Option<tokio::task::JoinHandle<()>> = None;
    // Pruning runs one evaluation behind, so every tenant's previous snapshot stays usable.
    let mut candidate = log.pruned.load(Ordering::SeqCst);
    let mut last_eval = Instant::now();
    let mut last_start = Instant::now() - gap;
    loop {
        let stopping = log.stopping.load(Ordering::SeqCst);
        if stopping && let Some((t, _)) = copying.take() {
            t.db.snapshot_abort();
            t.uploading.store(false, Ordering::SeqCst);
        }

        // Copy a snapshot a few milliseconds at a time (its tenant pauses for one step at a
        // time); between steps, commit only what someone waits for, at the usual pace.
        let mut copied = None;
        if let Some((t, _)) = &copying {
            let budget = Instant::now();
            let mut step = t.db.snapshot_step(cfg.snapshot_step_pages);
            while matches!(step, Ok(None)) && budget.elapsed() < Duration::from_millis(4) {
                step = t.db.snapshot_step(cfg.snapshot_step_pages);
            }
            match step {
                Ok(Some(changes)) => copied = Some(changes),
                Ok(None) => {
                    if !(log.urgent.load(Ordering::SeqCst) && last_start.elapsed() >= gap) {
                        tokio::select! {
                            _ = log.urgent_kick.notified() => {}
                            _ = tokio::time::sleep(Duration::from_millis(2)) => {}
                        }
                        continue;
                    }
                }
                Err(e) => {
                    warn!(error = %e, tenant = t.id, "snapshot failed");
                    t.db.snapshot_abort();
                    t.uploading.store(false, Ordering::SeqCst);
                    copying = None;
                }
            }
        } else if !stopping {
            wait_for_work(&log).await;
            let since = last_start.elapsed();
            if since < gap && !log.stopping.load(Ordering::SeqCst) {
                tokio::time::sleep(gap - since).await;
            }
        }
        last_start = Instant::now();
        log.urgent.store(false, Ordering::SeqCst);
        log.changed.store(false, Ordering::SeqCst);

        // Every cut gets a fresh batch number first, so a writer that reads the old number
        // after writing is covered by this cut.
        let batch = log.open_batch.fetch_add(1, Ordering::SeqCst);
        let mut sections = Vec::new();
        let tenants: Vec<Arc<Tenant>> = {
            let mut ops = log.ops.lock().unwrap();
            for (tenant, op) in ops.drain(..) {
                sections.push(Section { tenant, op, data: Vec::new() });
            }
            log.tenants.read().unwrap().values().cloned().collect()
        };
        let mut snapshot = None;
        if let Some(changes) = copied
            && let Some((t, file)) = copying.take()
        {
            if !changes.is_empty() {
                sections.push(Section { tenant: t.id.clone(), op: Op::Changes, data: changes });
            }
            snapshot = Some((t, file));
        }
        let mut failed = None;
        for t in &tenants {
            if t.dirty.swap(false, Ordering::SeqCst) {
                match t.db.cut() {
                    Ok(c) if !c.is_empty() => sections.push(Section { tenant: t.id.clone(), op: Op::Changes, data: c }),
                    Ok(_) => {}
                    Err(e) => failed = Some(format!("can't read changes of tenant {}: {e}", t.id)),
                }
            }
        }
        if let Some(reason) = failed {
            log.fence(reason);
            break;
        }

        let written = match log.commit(next_seq, &sections, batch).await {
            Ok(Some(len)) => {
                let seq = next_seq;
                next_seq += 1;
                wal.push_back((seq, len));
                wal_bytes += len;
                let index = log.tenants.read().unwrap();
                let involved: BTreeSet<&str> = sections.iter().map(|s| s.tenant.as_str()).collect();
                let share = 1_000_000 / involved.len().max(1) as u64;
                for id in involved {
                    if let Some(t) = index.get(id) {
                        t.storage_micros.fetch_add(share, Ordering::Relaxed);
                    }
                }
                for s in &sections {
                    if s.op != Op::Drop
                        && let Some(t) = index.get(&s.tenant)
                    {
                        t.last_section.fetch_max(seq, Ordering::SeqCst);
                        t.since_snapshot.fetch_add(s.data.len() as u64, Ordering::Relaxed);
                    }
                }
                Some(seq)
            }
            Ok(None) => None,
            Err(reason) => {
                log.fence(reason);
                break;
            }
        };

        if let Some((t, file)) = snapshot {
            // The copy is the tenant after this entry (or the last one, if nothing was written).
            let at = written.unwrap_or(next_seq - 1);
            t.since_snapshot.store(0, Ordering::Relaxed);
            if t.dropped.load(Ordering::SeqCst) {
                let _ = std::fs::remove_file(&file);
                t.uploading.store(false, Ordering::SeqCst);
            } else {
                uploads.spawn(upload_tenant_snapshot(log.clone(), t, file, at));
            }
        }

        // After uploads land (or holds go), entries can go: those every tenant had a
        // snapshot past at the previous evaluation.
        let mut finished = false;
        while uploads.try_join_next().is_some() {
            finished = true;
        }
        let idle_eval = copying.is_none() && uploads.is_empty() && candidate > log.pruned.load(Ordering::SeqCst) && last_eval.elapsed() > Duration::from_secs(1);
        if finished || idle_eval || log.reevaluate.swap(false, Ordering::SeqCst) {
            last_eval = Instant::now();
            let target = candidate;
            candidate = log.prune_point(next_seq - 1);
            if target > log.pruned.load(Ordering::SeqCst) && pruning.as_ref().is_none_or(|p| p.is_finished()) {
                let log2 = log.clone();
                pruning = Some(tokio::spawn(async move {
                    if let Err(e) = prune(log2, target).await {
                        warn!(error = %e, "pruning the log failed");
                    }
                }));
            }
        }
        let pruned = log.pruned.load(Ordering::SeqCst);
        while let Some((s, b)) = wal.front().copied()
            && s <= pruned
        {
            wal.pop_front();
            wal_bytes -= b;
        }

        if copying.is_none()
            && uploads.len() < 4
            && !log.stopping.load(Ordering::SeqCst)
            && let Some(t) = log.pick_snapshot(wal.len(), wal_bytes)
        {
            let file = cfg.dir.join("snapshots").join(format!("{}-{next_seq}.db", t.id));
            t.uploading.store(true, Ordering::SeqCst);
            match t.db.snapshot_begin(&file) {
                Ok(()) => copying = Some((t, file)),
                Err(e) => {
                    warn!(error = %e, tenant = t.id, "can't start a snapshot");
                    t.uploading.store(false, Ordering::SeqCst);
                }
            }
        }

        if log.stopping.load(Ordering::SeqCst) && !log.changed.load(Ordering::SeqCst) && log.ops.lock().unwrap().is_empty() {
            while uploads.join_next().await.is_some() {}
            if let Some(p) = pruning.take() {
                let _ = p.await;
            }
            log.fence("closed".into());
            break;
        }
    }
    if let Some((t, _)) = copying {
        t.db.snapshot_abort();
    }
}

#[async_trait]
impl Store for LogStore {
    fn describe(&self) -> String {
        format!("log {}/{} tenant {}", self.log.cfg.objects, self.log.cfg.prefix, self.t.id)
    }

    fn shards(&self) -> u32 {
        self.t.db.shards()
    }

    fn take_storage_writes(&self) -> u64 {
        self.t.take_storage_writes()
    }

    async fn msg_insert(&self, m: &Message, body: &[u8]) -> Result<()> {
        self.t.db.msg_insert(m, body).await?;
        self.written().await
    }

    async fn msg_get(&self, id: &str) -> Result<Option<Message>> {
        self.t.db.msg_get(id).await
    }

    async fn msg_body(&self, id: &str) -> Result<Option<Vec<u8>>> {
        self.t.db.msg_body(id).await
    }

    async fn msg_update(&self, m: &Message, expect: u64) -> Result<bool> {
        let ok = self.t.db.msg_update(m, expect).await?;
        if ok {
            self.written().await?;
        }
        Ok(ok)
    }

    async fn msg_delete(&self, id: &str) -> Result<()> {
        self.t.db.msg_delete(id).await?;
        self.written().await
    }

    async fn msg_due(&self, lane: &str, now: Ms, limit: usize) -> Result<Due> {
        self.t.db.msg_due(lane, now, limit).await
    }

    async fn msg_group_head(&self, group: &str) -> Result<Option<String>> {
        self.t.db.msg_group_head(group).await
    }

    async fn msg_list(&self, q: &MessageQuery) -> Result<Page<Message>> {
        self.t.db.msg_list(q).await
    }

    async fn lane_depth(&self, lane: &str, now: Ms) -> Result<(u64, u64, u64)> {
        self.t.db.lane_depth(lane, now).await
    }

    async fn attempt_add(&self, a: &Attempt) -> Result<()> {
        self.t.db.attempt_add(a).await?;
        self.written().await
    }

    async fn attempts(&self, message_id: &str) -> Result<Vec<Attempt>> {
        self.t.db.attempts(message_id).await
    }

    async fn kv_get(&self, key: &str) -> Result<Option<Versioned<Value>>> {
        self.t.db.kv_get(key).await
    }

    async fn kv_put(&self, key: &str, value: &Value, expect: Expect, expires_at: Option<Ms>) -> Result<bool> {
        let ok = self.t.db.kv_put(key, value, expect, expires_at).await?;
        if ok {
            self.written().await?;
        }
        Ok(ok)
    }

    async fn kv_delete(&self, key: &str, expect: Expect) -> Result<bool> {
        let ok = self.t.db.kv_delete(key, expect).await?;
        if ok {
            self.written().await?;
        }
        Ok(ok)
    }

    async fn doc_get(&self, kind: &str, id: &str) -> Result<Option<Versioned<Value>>> {
        if kind == crate::engine::KIND_NODE {
            return self.log.local.doc_get(&self.local_kind(kind), id).await;
        }
        self.t.db.doc_get(kind, id).await
    }

    async fn doc_put(&self, kind: &str, id: &str, value: &Value, expect: Expect) -> Result<bool> {
        if kind == crate::engine::KIND_NODE {
            return self.log.local.doc_put(&self.local_kind(kind), id, value, expect).await;
        }
        let ok = self.t.db.doc_put(kind, id, value, expect).await?;
        if ok {
            self.written().await?;
        }
        Ok(ok)
    }

    async fn doc_delete(&self, kind: &str, id: &str) -> Result<bool> {
        if kind == crate::engine::KIND_NODE {
            return self.log.local.doc_delete(&self.local_kind(kind), id).await;
        }
        let ok = self.t.db.doc_delete(kind, id).await?;
        if ok {
            self.written().await?;
        }
        Ok(ok)
    }

    async fn doc_list(&self, kind: &str) -> Result<Vec<Versioned<Value>>> {
        if kind == crate::engine::KIND_NODE {
            return self.log.local.doc_list(&self.local_kind(kind)).await;
        }
        self.t.db.doc_list(kind).await
    }

    async fn run_get(&self, id: &str) -> Result<Option<Run>> {
        self.t.db.run_get(id).await
    }

    async fn run_put(&self, run: &Run, expect: Expect) -> Result<bool> {
        let ok = self.t.db.run_put(run, expect).await?;
        if ok {
            self.written().await?;
        }
        Ok(ok)
    }

    async fn run_list(&self, q: &RunQuery) -> Result<Page<Run>> {
        self.t.db.run_list(q).await
    }

    async fn steps(&self, run_id: &str) -> Result<Vec<Step>> {
        self.t.db.steps(run_id).await
    }

    async fn step_put(&self, step: &Step) -> Result<()> {
        self.t.db.step_put(step).await?;
        self.written().await
    }

    async fn event_put(&self, e: &Event) -> Result<()> {
        self.t.db.event_put(e).await?;
        self.written().await
    }

    async fn event_get(&self, id: &str) -> Result<Option<Event>> {
        self.t.db.event_get(id).await
    }

    async fn event_list(&self, q: &EventQuery) -> Result<Page<Event>> {
        self.t.db.event_list(q).await
    }

    async fn wait_put(&self, w: &Wait) -> Result<()> {
        self.t.db.wait_put(w).await?;
        self.written().await
    }

    async fn wait_delete(&self, w: &Wait) -> Result<()> {
        self.t.db.wait_delete(w).await?;
        self.written().await
    }

    async fn waits(&self, event: &str, key: Option<&str>) -> Result<Vec<Wait>> {
        self.t.db.waits(event, key).await
    }

    async fn usage_add(&self, day: i64, u: &Usage) -> Result<()> {
        self.t.db.usage_add(day, u).await?;
        // Metering can be redone; it never holds anything up.
        relaxed(self.written()).await
    }

    async fn usage(&self, from_day: i64) -> Result<Vec<(i64, Usage)>> {
        self.t.db.usage(from_day).await
    }

    async fn sweep(&self, now: Ms) -> Result<u64> {
        let n = self.t.db.sweep(now).await?;
        if n > 0 {
            relaxed(self.written()).await?;
        }
        Ok(n)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;
    use serde_json::json;

    fn config(objects: Arc<dyn ObjectStore>, dir: &std::path::Path) -> LogConfig {
        let mut c = LogConfig::new(objects, "t", dir.to_path_buf());
        c.shards = 4;
        c.commit_ms = 1;
        c.relaxed_ms = 50;
        c
    }

    async fn eventually(f: impl Fn() -> bool) {
        for _ in 0..200 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("condition never held");
    }

    async fn wal_seqs(objects: &dyn ObjectStore) -> Vec<u64> {
        let mut v: Vec<u64> = objects.list(Some(&ObjPath::from("t/wal"))).map_ok(|m| seq_of(&m.location).unwrap()).try_collect().await.unwrap();
        v.sort_unstable();
        v
    }

    async fn put(s: &LogStore, kind: &str, id: &str, n: i64) {
        assert!(s.doc_put(kind, id, &json!({ "n": n }), Expect::Any).await.unwrap());
    }

    async fn get(s: &LogStore, kind: &str, id: &str) -> Option<i64> {
        s.doc_get(kind, id).await.unwrap().map(|v| v.value["n"].as_i64().unwrap())
    }

    #[tokio::test]
    async fn conformance() {
        let dir = tempfile::tempdir().unwrap();
        let store = LogStore::open(config(Arc::new(InMemory::new()), dir.path())).await.unwrap();
        super::super::conformance::run(store.clone()).await;
        store.close().await;
    }

    #[tokio::test]
    async fn conformance_on_a_local_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (objects, _) = objects_from_url(&format!("file://{}", dir.path().join("bucket").display())).unwrap();
        let store = LogStore::open(config(objects, &dir.path().join("work"))).await.unwrap();
        super::super::conformance::run(store.clone()).await;
        store.close().await;
    }

    #[tokio::test]
    async fn everything_acknowledged_survives_a_restart() {
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let a = LogStore::open(config(objects.clone(), d1.path())).await.unwrap();
        for i in 0..20 {
            put(&a, "q", &format!("k{i}"), i).await;
        }
        assert!(a.doc_delete("q", "k3").await.unwrap());
        // No close: a crash right after the last acknowledgement.
        drop(a);

        let b = LogStore::open(config(objects.clone(), d2.path())).await.unwrap();
        for i in 0..20 {
            assert_eq!(get(&b, "q", &format!("k{i}")).await, (i != 3).then_some(i as i64));
        }
        b.close().await;
    }

    #[tokio::test]
    async fn concurrent_writes_share_commits() {
        let dir = tempfile::tempdir().unwrap();
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut c = config(objects.clone(), dir.path());
        c.commit_ms = 20;
        let s = LogStore::open(c).await.unwrap();
        let writes = (0..200).map(|i| {
            let s = s.clone();
            async move { put(&s, "q", &format!("k{i}"), i).await }
        });
        futures::future::join_all(writes).await;
        let commits = s.stats().commits;
        assert!((1..20).contains(&commits), "200 concurrent writes took {commits} commits");
        s.close().await;
    }

    #[tokio::test]
    async fn relaxed_writes_commit_soon_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut c = config(objects.clone(), dir.path());
        c.relaxed_ms = 300;
        let s = LogStore::open(c).await.unwrap();
        let base = s.stats().commits;
        let started = std::time::Instant::now();
        relaxed(async {
            for i in 0..50 {
                put(&s, "r", &format!("k{i}"), i).await;
            }
        })
        .await;
        assert!(started.elapsed() < Duration::from_millis(250), "relaxed writes waited");
        assert_eq!(s.stats().commits, base);
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(s.stats().commits, base + 1, "relaxed writes go out together");

        let d2 = tempfile::tempdir().unwrap();
        drop(s);
        let b = LogStore::open(config(objects, d2.path())).await.unwrap();
        assert_eq!(get(&b, "r", "k49").await, Some(49));
        b.close().await;
    }

    #[tokio::test]
    async fn a_new_leader_fences_the_old_one() {
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let a = LogStore::open(config(objects.clone(), d1.path())).await.unwrap();
        put(&a, "q", "before", 1).await;

        let b = LogStore::open(config(objects.clone(), d2.path())).await.unwrap();
        assert_eq!(get(&b, "q", "before").await, Some(1));

        // The old leader can't acknowledge anything any more, and says so.
        let err = a.doc_put("q", "after", &json!({ "n": 2 }), Expect::Any).await.unwrap_err();
        assert!(err.to_string().contains("unavailable"), "{err}");
        assert!(tokio::time::timeout(Duration::from_secs(1), a.fenced()).await.unwrap().contains("took over"));
        assert!(a.doc_put("q", "later", &json!({ "n": 3 }), Expect::Any).await.is_err());

        // The new one carries on, and what the old one tried never shows up.
        put(&b, "q", "new", 4).await;
        let d3 = tempfile::tempdir().unwrap();
        b.close().await;
        let c = LogStore::open(config(objects, d3.path())).await.unwrap();
        assert_eq!(get(&c, "q", "after").await, None);
        assert_eq!(get(&c, "q", "new").await, Some(4));
        c.close().await;
    }

    #[tokio::test]
    async fn snapshots_bound_the_log_and_restore_everything() {
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let dir = tempfile::tempdir().unwrap();
        let mut c = config(objects.clone(), dir.path());
        c.snapshot_commits = 10;
        c.snapshot_step_pages = 1;
        let s = LogStore::open(c.clone()).await.unwrap();
        // Bodies big enough that the copy takes several steps while writes go on.
        let body = "x".repeat(3000);
        let mut n = 0;
        while s.stats().snapshots < 3 {
            assert!(n < 5_000, "no snapshots after {n} writes");
            s.doc_put("big", &format!("k{n}"), &json!({ "n": n, "pad": body }), Expect::Any).await.unwrap();
            n += 1;
        }
        let stats = s.stats();
        s.close().await;

        // Only the last two snapshots and the log after the older one remain.
        let snaps: Vec<_> = objects.list(Some(&ObjPath::from("t/snap/default"))).try_collect().await.unwrap();
        assert!(snaps.len() <= 3, "{} snapshots kept", snaps.len());
        let wal = wal_seqs(&*objects).await;
        assert!(wal[0] > 1, "old log entries were pruned");
        assert!((wal.len() as u64) < stats.seq, "log is bounded: {} entries for {} commits", wal.len(), stats.seq);

        let d2 = tempfile::tempdir().unwrap();
        let b = LogStore::open(config(objects, d2.path())).await.unwrap();
        for i in 0..n {
            assert_eq!(get(&b, "big", &format!("k{i}")).await, Some(i));
        }
        b.close().await;
    }

    #[tokio::test]
    async fn an_unreadable_snapshot_falls_back_to_the_previous_one() {
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let dir = tempfile::tempdir().unwrap();
        let mut c = config(objects.clone(), dir.path());
        c.snapshot_commits = 5;
        let s = LogStore::open(c).await.unwrap();
        for i in 0..40 {
            put(&s, "q", &format!("k{i}"), i).await;
        }
        eventually(|| s.stats().snapshots >= 2).await;
        s.close().await;

        let mut snaps: Vec<u64> = objects.list(Some(&ObjPath::from("t/snap/default"))).map_ok(|m| seq_of(&m.location).unwrap()).try_collect().await.unwrap();
        snaps.sort_unstable();
        assert_eq!(snaps.len(), 2, "the newest snapshot and its fallback are kept");
        let newest = *snaps.last().unwrap();
        objects.put(&snap_path("t", DEFAULT_TENANT, newest), PutPayload::from_static(b"garbage")).await.unwrap();

        let d2 = tempfile::tempdir().unwrap();
        let b = LogStore::open(config(objects, d2.path())).await.unwrap();
        for i in 0..40 {
            assert_eq!(get(&b, "q", &format!("k{i}")).await, Some(i));
        }
        b.close().await;
    }

    #[tokio::test]
    async fn a_hole_in_the_log_refuses_to_open() {
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let dir = tempfile::tempdir().unwrap();
        let s = LogStore::open(config(objects.clone(), dir.path())).await.unwrap();
        for i in 0..5 {
            put(&s, "q", &format!("k{i}"), i).await;
        }
        s.close().await;
        objects.delete(&wal_path("t", 3)).await.unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let err = LogStore::open(config(objects, d2.path())).await.err().expect("must not open past a gap");
        assert!(err.to_string().contains("gap"), "{err}");
    }

    #[tokio::test]
    async fn closing_flushes_relaxed_writes() {
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let dir = tempfile::tempdir().unwrap();
        let mut c = config(objects.clone(), dir.path());
        c.relaxed_ms = 60_000;
        let s = LogStore::open(c).await.unwrap();
        relaxed(put(&s, "q", "lazy", 7)).await;
        s.close().await;
        assert!(s.doc_put("q", "x", &json!({}), Expect::Any).await.is_err(), "closed stores refuse writes");
        let d2 = tempfile::tempdir().unwrap();
        let b = LogStore::open(config(objects, d2.path())).await.unwrap();
        assert_eq!(get(&b, "q", "lazy").await, Some(7));
        b.close().await;
    }

    #[test]
    fn entries_round_trip() {
        let sections = vec![
            Section { tenant: "acme".into(), op: Op::Create, data: vec![] },
            Section { tenant: "acme".into(), op: Op::Changes, data: b"changes".to_vec() },
            Section { tenant: "b".into(), op: Op::Drop, data: vec![] },
        ];
        let e = encode(9, "commit", "n1", &sections);
        let back = decode(&e).unwrap();
        assert_eq!(back.len(), 3);
        assert_eq!((back[1].tenant.as_str(), back[1].op, back[1].data.as_slice()), ("acme", Op::Changes, &b"changes"[..]));
        assert_eq!((back[2].tenant.as_str(), back[2].op), ("b", Op::Drop));
        assert!(decode(&encode(1, "takeover", "n", &[])).unwrap().is_empty());
        assert!(decode(b"nope").is_err());
        assert!(decode(&e[..e.len() - 3]).is_err());
        assert_eq!(seq_of(&wal_path("t", 7)), Some(7));
        assert_eq!(snap_of(&snap_path("t", "acme", 42)), Some(("acme".into(), 42)));
        assert!(valid_tenant_id("acme-2") && !valid_tenant_id("-a") && !valid_tenant_id("A") && !valid_tenant_id("") && !valid_tenant_id(&"a".repeat(64)));
    }

    async fn open_log(objects: Arc<dyn ObjectStore>, dir: &std::path::Path, f: impl FnOnce(&mut LogConfig)) -> Arc<Log> {
        let mut c = config(objects, dir);
        f(&mut c);
        Log::open(c).await.unwrap()
    }

    #[tokio::test]
    async fn tenants_are_isolated_share_commits_and_survive_a_restart() {
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let d1 = tempfile::tempdir().unwrap();
        let log = open_log(objects.clone(), d1.path(), |c| c.commit_ms = 30).await;
        let mut stores = Vec::new();
        for i in 0..20 {
            stores.push(log.create_tenant(&format!("t{i}")).await.unwrap());
        }
        assert!(log.create_tenant("t3").await.is_err(), "ids are unique");
        assert!(log.create_tenant("Bad_Id").await.is_err());
        let before = log.stats().commits;
        // Every tenant writes at once: they share commits.
        let writes = stores.iter().enumerate().flat_map(|(i, s)| {
            (0..5).map(move |k| {
                let s = s.clone();
                async move { put(&s, "q", &format!("k{k}"), (i * 100 + k) as i64).await }
            })
        });
        futures::future::join_all(writes).await;
        let commits = log.stats().commits - before;
        assert!(commits < 20, "100 writes from 20 tenants took {commits} commits");
        // Same keys, different tenants, different values.
        assert_eq!(get(&stores[3], "q", "k2").await, Some(302));
        assert_eq!(get(&stores[7], "q", "k2").await, Some(702));
        assert_eq!(stores[5].doc_list("q").await.unwrap().len(), 5);
        drop(stores);

        let d2 = tempfile::tempdir().unwrap();
        let again = open_log(objects, d2.path(), |_| {}).await;
        assert_eq!(again.tenant_ids().len(), 20);
        for i in 0..20 {
            let t = again.tenant(&format!("t{i}")).unwrap();
            assert_eq!(get(&t, "q", "k4").await, Some((i * 100 + 4) as i64));
        }
        again.close().await;
    }

    #[tokio::test]
    async fn deleted_tenants_stay_deleted_and_ids_can_be_reused() {
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let d1 = tempfile::tempdir().unwrap();
        let log = open_log(objects.clone(), d1.path(), |c| c.snapshot_commits = 4).await;
        let keep = log.create_tenant("keep").await.unwrap();
        let gone = log.create_tenant("gone").await.unwrap();
        for i in 0..30 {
            put(&gone, "q", &format!("k{i}"), i).await;
            put(&keep, "q", &format!("k{i}"), i).await;
        }
        eventually(|| log.stats().snapshots >= 2).await;
        // Taking an existing id fails, and leaves that tenant's files alone.
        let inode = || std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(db_path(d1.path(), "keep")).unwrap());
        let before = inode();
        assert!(log.create_tenant("keep").await.is_err());
        assert_eq!(inode(), before, "the live database file was replaced");
        assert_eq!(get(&keep, "q", "k29").await, Some(29));
        put(&keep, "q", "after", 1).await;
        assert!(log.drop_tenant("gone").await.unwrap());
        assert!(!log.drop_tenant("gone").await.unwrap());
        assert!(gone.doc_put("q", "late", &json!({}), Expect::Any).await.is_err(), "writes to a deleted tenant fail");
        let left: Vec<_> = objects.list(Some(&ObjPath::from("t/snap/gone"))).try_collect().await.unwrap();
        assert!(left.is_empty(), "its snapshots are deleted");
        let reborn = log.create_tenant("gone").await.unwrap();
        assert_eq!(get(&reborn, "q", "k1").await, None, "a new tenant with an old id starts empty");
        put(&reborn, "q", "fresh", 1).await;

        let d2 = tempfile::tempdir().unwrap();
        drop((keep, gone, reborn));
        let again = open_log(objects, d2.path(), |_| {}).await;
        let reborn = again.tenant("gone").unwrap();
        assert_eq!(get(&reborn, "q", "k1").await, None);
        assert_eq!(get(&reborn, "q", "fresh").await, Some(1));
        assert_eq!(get(&again.tenant("keep").unwrap(), "q", "k29").await, Some(29));
        again.close().await;
    }

    #[tokio::test]
    async fn many_tenants_keep_the_log_short_and_restore_fully() {
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let d1 = tempfile::tempdir().unwrap();
        let log = open_log(objects.clone(), d1.path(), |c| c.snapshot_commits = 20).await;
        let mut stores = Vec::new();
        for i in 0..12 {
            stores.push(log.create_tenant(&format!("t{i}")).await.unwrap());
        }
        for round in 0..20 {
            for (i, s) in stores.iter().enumerate() {
                put(s, "q", &format!("k{round}"), (i * 1000 + round) as i64).await;
            }
        }
        // Compaction catches up once writes stop.
        eventually(|| log.stats().pruned > 150).await;
        let stats = log.stats();
        let wal = wal_seqs(&*objects).await;
        assert!((wal.len() as u64) < stats.seq / 2, "log is bounded: {} entries of {}", wal.len(), stats.seq);
        log.close().await;

        let d2 = tempfile::tempdir().unwrap();
        drop(stores);
        let again = open_log(objects, d2.path(), |_| {}).await;
        for i in 0..12 {
            let t = again.tenant(&format!("t{i}")).unwrap();
            for round in 0..20 {
                assert_eq!(get(&t, "q", &format!("k{round}")).await, Some((i * 1000 + round) as i64));
            }
        }
        again.close().await;
    }

    #[tokio::test]
    async fn tenants_pay_their_share_of_shared_commits() {
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let d1 = tempfile::tempdir().unwrap();
        let log = open_log(objects, d1.path(), |c| c.commit_ms = 50).await;
        let a = log.create_tenant("a").await.unwrap();
        let b = log.create_tenant("b").await.unwrap();
        let _ = (a.take_storage_writes(), b.take_storage_writes());
        for i in 0..10 {
            futures::future::join(put(&a, "q", &format!("k{i}"), i), put(&b, "q", &format!("k{i}"), i)).await;
        }
        let (wa, wb) = (a.take_storage_writes(), b.take_storage_writes());
        // Ten shared commits: five each, give or take one for timing.
        assert!((4..=6).contains(&wa) && (4..=6).contains(&wb), "a {wa}, b {wb}");
        log.close().await;
    }
}
