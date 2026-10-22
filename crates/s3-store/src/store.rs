//! The store: buckets, objects, uploads, trash, keys and usage over the metadata database and
//! the blob backend. Every write is durable (in object storage) when its method returns.

use crate::backend::{Backend, BlobAttrs, ByteStream, Uploaded};
use crate::error::{Result, StoreError};
use crate::ids;
use crate::log::{LogConfig, LogStats, MetaLog};
use crate::meter::Meter;
use crate::model::*;
use crate::rows::{self, Row};
use crate::validate;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Unix milliseconds now (injectable for tests).
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64))
}

#[derive(Clone)]
pub struct StoreConfig {
    /// Local working directory (the metadata database cache).
    pub dir: PathBuf,
    /// The region recorded on new buckets; the same one the S3 API reports.
    pub region: String,
    pub node: String,
    pub recover_days_default: u32,
    /// Unreferenced blobs wait this long before they're deleted from object storage.
    pub quarantine: Duration,
    /// Blobs in object storage without metadata are deleted once this old …
    pub orphan_min_age: Duration,
    /// … and only if younger than this: older ones mean metadata was lost, so they're only
    /// reported.
    pub orphan_max_age: Duration,
    /// Multipart uploads are aborted after this many days (unless a lifecycle rule says less).
    pub upload_expiry_days: u32,
    /// S3's 5 MiB minimum for all parts but the last.
    pub min_part_size: u64,
    pub commit_ms: u64,
    pub snapshot_commits: u64,
    pub snapshot_bytes: u64,
    /// Pages (4 KiB) a snapshot copies at a time; writes pause for one step.
    pub snapshot_step_pages: i32,
    /// How often an idle node checks it's still the metadata log's writer.
    pub fence_check: Duration,
    /// Background jobs; `None` = only when called.
    pub meter_interval: Option<Duration>,
    pub sweep_interval: Option<Duration>,
    pub lifecycle_interval: Option<Duration>,
    pub orphan_interval: Option<Duration>,
    pub scrub_interval: Option<Duration>,
    pub clock: Clock,
    /// Block public access everywhere (`ROLL_S3_BLOCK_PUBLIC_ACCESS`): all four S3 Block Public
    /// Access settings apply to every bucket, on top of the bucket's own.
    pub block_public_access: bool,
    /// SSE-KMS on or off (`ROLL_S3_KMS`); `None`: whatever the backend can pass through (real
    /// AWS S3 without a custom endpoint).
    pub kms: Option<bool>,
    /// Copies that need a new blob (encryption changes) go in one backend copy up to this size
    /// (S3's 5 GiB CopyObject limit), part by part beyond it …
    pub max_single_copy: u64,
    /// … in parts of this size (at least `size / 10 000`).
    pub copy_part_size: u64,
}

impl StoreConfig {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            region: "us-east-1".into(),
            node: "local".into(),
            recover_days_default: 7,
            quarantine: Duration::from_secs(86_400),
            orphan_min_age: Duration::from_secs(86_400),
            orphan_max_age: Duration::from_secs(7 * 86_400),
            upload_expiry_days: 7,
            min_part_size: 5 << 20,
            commit_ms: 20,
            snapshot_commits: 10_000,
            snapshot_bytes: 64 << 20,
            snapshot_step_pages: 256,
            fence_check: Duration::from_secs(5),
            meter_interval: Some(Duration::from_secs(60)),
            sweep_interval: Some(Duration::from_secs(3600)),
            lifecycle_interval: Some(Duration::from_secs(86_400)),
            orphan_interval: Some(Duration::from_secs(86_400)),
            scrub_interval: Some(Duration::from_secs(7 * 86_400)),
            clock: system_clock(),
            block_public_access: false,
            kms: None,
            max_single_copy: 5 << 30,
            copy_part_size: 512 << 20,
        }
    }

    /// No background jobs (tests run them by hand).
    pub fn manual(mut self) -> Self {
        self.meter_interval = None;
        self.sweep_interval = None;
        self.lifecycle_interval = None;
        self.orphan_interval = None;
        self.scrub_interval = None;
        self
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct StoreStats {
    pub buckets: u64,
    pub objects: u64,
    pub bytes: u64,
    pub trash_bytes: u64,
    pub blobs: u64,
    pub blob_bytes: u64,
    pub log: LogStats,
    pub scrub: ScrubReport,
    /// Blobs in object storage without metadata, too old to delete automatically.
    pub unexpected_orphans: u64,
}

pub struct Store {
    pub cfg: StoreConfig,
    pub(crate) log: Arc<MetaLog>,
    pub(crate) backend: Arc<dyn Backend>,
    pub meter: Meter,
    /// Blobs being uploaded: not in the metadata yet, but not orphans.
    pub(crate) inflight: Mutex<HashSet<String>>,
    /// One CompleteMultipartUpload at a time per upload (a client's retry waits for the first).
    pub(crate) completing: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// When each key's `last_used_at` was last written.
    touched: Mutex<HashMap<String, i64>>,
    pub(crate) last_flush: Mutex<i64>,
    pub(crate) scrub: Mutex<ScrubReport>,
    pub(crate) unexpected_orphans: std::sync::atomic::AtomicU64,
    pub(crate) events: crate::events::EventState,
}

/// A bucket's row.
#[derive(Clone, Debug)]
pub(crate) struct BucketRow {
    pub id: String,
    pub name: String,
    pub region: String,
    pub created_at: i64,
    pub public: bool,
    pub recover_days: u32,
    pub cors: Vec<CorsRule>,
    pub lifecycle: Vec<LifecycleRule>,
    pub parent_id: Option<String>,
    pub parent_at: Option<i64>,
    pub objects: u64,
    pub bytes: u64,
    pub trash_bytes: u64,
    pub versioning: Option<String>,
    pub object_lock: Option<ObjectLockConfig>,
    pub versions: u64,
    pub version_bytes: u64,
}

const BUCKET_COLS: &str = "id, name, region, created_at, public, recover_days, cors, lifecycle, parent_id, parent_at, objects, bytes, trash_bytes, versioning, object_lock, versions, version_bytes";

fn bucket_row(r: &rusqlite::Row) -> rusqlite::Result<BucketRow> {
    let parse = |i: usize| -> rusqlite::Result<Option<String>> { r.get(i) };
    let cors: Option<String> = parse(6)?;
    let lifecycle: Option<String> = parse(7)?;
    let object_lock: Option<String> = parse(14)?;
    let bad = |i: usize, e: serde_json::Error| rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e));
    Ok(BucketRow {
        id: r.get(0)?,
        name: r.get(1)?,
        region: r.get(2)?,
        created_at: r.get(3)?,
        public: r.get::<_, i64>(4)? != 0,
        recover_days: r.get::<_, i64>(5)? as u32,
        cors: cors.map(|s| serde_json::from_str(&s)).transpose().map_err(|e| bad(6, e))?.unwrap_or_default(),
        lifecycle: lifecycle.map(|s| serde_json::from_str(&s)).transpose().map_err(|e| bad(7, e))?.unwrap_or_default(),
        parent_id: r.get(8)?,
        parent_at: r.get(9)?,
        objects: r.get::<_, i64>(10)?.max(0) as u64,
        bytes: r.get::<_, i64>(11)?.max(0) as u64,
        trash_bytes: r.get::<_, i64>(12)?.max(0) as u64,
        versioning: r.get(13)?,
        object_lock: object_lock.map(|s| serde_json::from_str(&s)).transpose().map_err(|e| bad(14, e))?,
        versions: r.get::<_, i64>(15)?.max(0) as u64,
        version_bytes: r.get::<_, i64>(16)?.max(0) as u64,
    })
}

pub(crate) fn find_bucket(c: &Connection, name: &str) -> Result<BucketRow> {
    c.prepare_cached(&format!("SELECT {BUCKET_COLS} FROM buckets WHERE name = ?1"))?
        .query_row([name], bucket_row)
        .optional()?
        .ok_or_else(|| StoreError::NoSuchBucket(name.to_string()))
}

/// S3's rule for deleting a bucket: no objects, no delete markers and no noncurrent versions in
/// its view.
fn bucket_empty(c: &Connection, bucket_id: &str) -> Result<bool> {
    if c.prepare_cached("SELECT 1 FROM versions WHERE bucket_id = ?1 LIMIT 1")?.exists([bucket_id])? {
        return Ok(false);
    }
    let chain = rows::chain(c, bucket_id)?;
    Ok(rows::Merger::current(c, &chain, "", "", true).next_entry()?.is_none())
}

/// Whether any of the bucket's rows (current, noncurrent or trashed) is under Object Lock.
pub(crate) fn holds_locked(c: &Connection, bucket_id: &str, now: i64) -> Result<bool> {
    const LOCKED: &str = "bucket_id = ?1 AND (legal_hold = 1 OR (lock_mode IS NOT NULL AND lock_until > ?2))";
    Ok(c.prepare_cached(&format!(
        "SELECT 1 FROM objects WHERE {LOCKED} UNION ALL SELECT 1 FROM versions WHERE {LOCKED} UNION ALL SELECT 1 FROM trash WHERE {LOCKED} LIMIT 1"
    ))?
    .exists(params![bucket_id, now])?)
}

pub(crate) fn find_bucket_id(c: &Connection, id: &str) -> Result<Option<BucketRow>> {
    Ok(c.prepare_cached(&format!("SELECT {BUCKET_COLS} FROM buckets WHERE id = ?1"))?.query_row([id], bucket_row).optional()?)
}

pub(crate) fn bucket_view(c: &Connection, b: BucketRow) -> Result<Bucket> {
    let parent = match (&b.parent_id, b.parent_at) {
        (Some(pid), Some(at)) => {
            let name: String = c.query_row("SELECT name FROM buckets WHERE id = ?1", [pid], |r| r.get(0)).optional()?.unwrap_or_default();
            Some(ParentRef { id: pid.clone(), name, at })
        }
        _ => None,
    };
    let branches = c
        .prepare_cached("SELECT name FROM buckets WHERE parent_id = ?1 ORDER BY name")?
        .query_map([&b.id], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(Bucket {
        id: b.id,
        name: b.name,
        region: b.region,
        created_at: b.created_at,
        public: b.public,
        recover_days: b.recover_days,
        cors: b.cors,
        lifecycle: b.lifecycle,
        parent,
        branches,
        objects: b.objects,
        bytes: b.bytes,
        trash_bytes: b.trash_bytes,
        versioning: b.versioning,
        object_lock: b.object_lock,
        versions: b.versions,
        version_bytes: b.version_bytes,
    })
}

fn key_row(r: &rusqlite::Row) -> rusqlite::Result<AccessKey> {
    let perm: String = r.get(3)?;
    let buckets: Option<String> = r.get(4)?;
    Ok(AccessKey {
        id: r.get(0)?,
        secret: r.get(1)?,
        name: r.get(2)?,
        permission: Permission::parse(&perm).unwrap_or(Permission::Read),
        buckets: buckets.and_then(|s| serde_json::from_str(&s).ok()),
        created_at: r.get(5)?,
        last_used_at: r.get(6)?,
        session: None,
    })
}

const KEY_COLS: &str = "id, secret, name, permission, buckets, created_at, last_used_at";

impl Store {
    /// Rebuild the metadata from object storage, take over writing it, and start the
    /// background jobs.
    pub async fn open(cfg: StoreConfig, backend: Arc<dyn Backend>) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&cfg.dir)?;
        let log_cfg = LogConfig {
            prefix: "s3/meta".into(),
            dir: cfg.dir.clone(),
            node: cfg.node.clone(),
            commit_ms: cfg.commit_ms,
            snapshot_commits: cfg.snapshot_commits,
            snapshot_bytes: cfg.snapshot_bytes,
            snapshot_step_pages: cfg.snapshot_step_pages,
            fence_check_ms: cfg.fence_check.as_millis() as u64,
        };
        let log = MetaLog::open(backend.clone(), log_cfg).await?;
        let now = (cfg.clock)();
        let scrub = log.db.read(|c| {
            let v: Option<String> = c.query_row("SELECT v FROM state WHERE k = 'scrub'", [], |r| r.get(0)).optional()?;
            Ok(v.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default())
        })?;
        let store = Arc::new(Self {
            cfg,
            log,
            backend,
            meter: Meter::default(),
            inflight: Mutex::new(HashSet::new()),
            completing: Mutex::new(HashMap::new()),
            touched: Mutex::new(HashMap::new()),
            last_flush: Mutex::new(now),
            scrub: Mutex::new(scrub),
            unexpected_orphans: Default::default(),
            events: Default::default(),
        });
        store.ensure_owner().await?;
        store.load_event_state()?;
        store.start_jobs();
        Ok(store)
    }

    pub fn now(&self) -> i64 {
        (self.cfg.clock)()
    }

    pub fn backend(&self) -> &Arc<dyn Backend> {
        &self.backend
    }

    pub fn describe(&self) -> String {
        self.backend.describe()
    }

    /// Flush the meter, commit everything and stop. Every call after this fails.
    pub async fn close(&self) {
        if let Err(e) = self.flush_meter() {
            tracing::warn!(error = %e, "flushing storage usage at close failed");
        }
        self.log.close().await;
    }

    /// Take a metadata snapshot now (normally they happen as the log grows).
    pub async fn snapshot_now(&self) -> Result<u64> {
        self.log.snapshot_now().await
    }

    pub(crate) fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.log.check_open()?;
        self.log.db.read(f)
    }

    /// A durable write: returns once it's in object storage.
    pub(crate) async fn write<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.log.check_open()?;
        let (out, batch) = self.log.db.write(|tx| f(tx))?;
        self.log.wait(batch).await?;
        Ok(out)
    }

    /// A write that may be lost in a crash (metering, key usage times): committed with the
    /// next group, without waiting.
    pub(crate) fn write_relaxed<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.log.check_open()?;
        let (out, _) = self.log.db.write(|tx| f(tx))?;
        self.log.kick();
        Ok(out)
    }

    pub fn stats(&self) -> Result<StoreStats> {
        let (buckets, objects, bytes, trash_bytes, blobs, blob_bytes) = self.read(|c| {
            Ok(c.query_row(
                "SELECT (SELECT count(*) FROM buckets), (SELECT coalesce(sum(objects), 0) FROM buckets), (SELECT coalesce(sum(bytes), 0) FROM buckets),
                        (SELECT coalesce(sum(trash_bytes), 0) FROM buckets), (SELECT count(*) FROM blobs), (SELECT coalesce(sum(size), 0) FROM blobs)",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?)),
            )?)
        })?;
        Ok(StoreStats {
            buckets: buckets as u64,
            objects: objects as u64,
            bytes: bytes as u64,
            trash_bytes: trash_bytes as u64,
            blobs: blobs as u64,
            blob_bytes: blob_bytes as u64,
            log: self.log.stats(),
            scrub: self.scrub.lock().unwrap().clone(),
            unexpected_orphans: self.unexpected_orphans.load(std::sync::atomic::Ordering::Relaxed),
        })
    }

    // ---- Buckets ----

    pub async fn create_bucket(&self, name: &str, opts: BucketOptions) -> Result<Bucket> {
        validate::bucket_name(name)?;
        let recover_days = opts.recover_days.unwrap_or(self.cfg.recover_days_default);
        validate::recover_days(recover_days)?;
        let now = self.now();
        let id = ids::bucket_id();
        let region = self.cfg.region.clone();
        let public = opts.public.unwrap_or(false);
        if public && self.cfg.block_public_access {
            return Err(StoreError::InvalidArgument(format!("bucket {name} can't be made public: public access is blocked for this deployment")));
        }
        // Object Lock needs versioning, so it turns it on.
        let locked = opts.object_lock.unwrap_or(false);
        let (versioning, lock) = if locked { (Some("Enabled"), Some(rows::json(&ObjectLockConfig::default())?)) } else { (None, None) };
        self.write(|c| {
            if c.query_row("SELECT 1 FROM buckets WHERE name = ?1", [name], |_| Ok(())).optional()?.is_some() {
                return Err(StoreError::BucketExists(name.to_string()));
            }
            c.execute(
                &format!("INSERT INTO buckets ({BUCKET_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, NULL, NULL, 0, 0, 0, ?7, ?8, 0, 0)"),
                params![id, name, region, now, public as i64, recover_days as i64, versioning, lock],
            )?;
            Ok(())
        })
        .await?;
        self.bucket(name)
    }

    pub fn bucket(&self, name: &str) -> Result<Bucket> {
        self.read(|c| bucket_view(c, find_bucket(c, name)?))
    }

    pub fn bucket_by_id(&self, id: &str) -> Result<Option<Bucket>> {
        self.read(|c| find_bucket_id(c, id)?.map(|b| bucket_view(c, b)).transpose())
    }

    pub fn buckets(&self) -> Result<Vec<Bucket>> {
        self.read(|c| {
            let rows = c.prepare(&format!("SELECT {BUCKET_COLS} FROM buckets ORDER BY name"))?.query_map([], bucket_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().map(|b| bucket_view(c, b)).collect()
        })
    }

    pub async fn update_bucket(&self, name: &str, mut u: BucketUpdate) -> Result<Bucket> {
        if let Some(d) = u.recover_days {
            validate::recover_days(d)?;
        }
        if let Some(r) = &u.cors {
            validate::cors(r)?;
        }
        if let Some(r) = u.lifecycle.as_mut() {
            validate::lifecycle(r)?;
        }
        if let Some(v) = &u.versioning
            && v != "Enabled"
            && v != "Suspended"
        {
            return Err(StoreError::InvalidArgument("versioning is Enabled or Suspended".into()));
        }
        self.write(|c| {
            let b = find_bucket(c, name)?;
            if let Some(v) = &u.versioning {
                if v == "Suspended" && b.object_lock.is_some() {
                    return Err(StoreError::InvalidBucketState("An Object Lock configuration is present on this bucket, so the versioning state cannot be changed.".into()));
                }
                c.execute("UPDATE buckets SET versioning = ?2 WHERE id = ?1", params![b.id, v])?;
            }
            if let Some(p) = u.public {
                if p {
                    crate::config::check_may_be_public(c, &b.id, name, self.cfg.block_public_access)?;
                }
                c.execute("UPDATE buckets SET public = ?2 WHERE id = ?1", params![b.id, p as i64])?;
            }
            if let Some(d) = u.recover_days {
                c.execute("UPDATE buckets SET recover_days = ?2 WHERE id = ?1", params![b.id, d as i64])?;
            }
            if let Some(r) = &u.cors {
                let v = if r.is_empty() { None } else { Some(rows::json(r)?) };
                c.execute("UPDATE buckets SET cors = ?2 WHERE id = ?1", params![b.id, v])?;
            }
            if let Some(r) = &u.lifecycle {
                let v = if r.is_empty() { None } else { Some(rows::json(r)?) };
                c.execute("UPDATE buckets SET lifecycle = ?2 WHERE id = ?1", params![b.id, v])?;
            }
            Ok(())
        })
        .await?;
        self.bucket(name)
    }

    /// A new bucket that sees `parent`'s objects as of now (copy-on-write; nothing is copied).
    pub async fn branch_bucket(&self, parent: &str, name: &str) -> Result<Bucket> {
        validate::bucket_name(name)?;
        let now = self.now();
        let id = ids::bucket_id();
        self.write(|c| {
            let p = find_bucket(c, parent)?;
            if c.query_row("SELECT 1 FROM buckets WHERE name = ?1", [name], |_| Ok(())).optional()?.is_some() {
                return Err(StoreError::BucketExists(name.to_string()));
            }
            c.execute(
                &format!(
                    "INSERT INTO buckets ({BUCKET_COLS}) SELECT ?1, ?2, region, ?3, public, recover_days, cors, lifecycle, id, ?3, 0, 0, 0, versioning, object_lock, 0, 0 FROM buckets WHERE id = ?4"
                ),
                params![id, name, now, p.id],
            )?;
            crate::config::copy_bucket_config(c, &p.id, &id)?;
            Ok(())
        })
        .await?;
        self.bucket(name)
    }

    /// Delete a bucket. Without `force` its view must be empty (S3's rule); with it, every
    /// object (and the trash) goes. Buckets with branches can't be deleted.
    pub async fn delete_bucket(&self, name: &str, force: bool) -> Result<()> {
        let b = self.read(|c| find_bucket(c, name))?;
        if !self.read(|c| rows::children(c, &b.id))?.is_empty() {
            return Err(StoreError::BucketHasBranches(name.to_string()));
        }
        if !force && !self.read(|c| bucket_empty(c, &b.id))? {
            return Err(StoreError::BucketNotEmpty(name.to_string()));
        }
        // Versions under Object Lock (retention or legal hold) are never deleted, so neither is
        // their bucket. Checked up front, and again for every row deleted below.
        let now = self.now();
        if self.read(|c| holds_locked(c, &b.id, now))? {
            return Err(StoreError::ObjectLocked(format!("bucket {name} holds versions under Object Lock")));
        }
        // Uploads first (each aborts its backend upload), then rows in batches, then the bucket.
        loop {
            let uploads: Vec<String> = self.read(|c| {
                Ok(c.prepare("SELECT id FROM uploads WHERE bucket_id = ?1 LIMIT 1000")?.query_map([&b.id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
            })?;
            if uploads.is_empty() {
                break;
            }
            for u in uploads {
                match self.abort_upload(&u).await {
                    Ok(()) | Err(StoreError::NoSuchUpload(_)) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        // Without `force` everything goes in one transaction that checks the view is empty
        // first: a write may have landed since the check above, and it must not be deleted
        // along with the bucket. (Its own rows are then only tombstones, which are small.)
        let batch: i64 = if force { 1000 } else { i64::MAX };
        let locked = || StoreError::ObjectLocked(format!("bucket {name} holds versions under Object Lock"));
        loop {
            let done = self
                .write(|c| {
                    if !rows::children(c, &b.id)?.is_empty() {
                        return Err(StoreError::BucketHasBranches(name.to_string()));
                    }
                    if !force && !bucket_empty(c, &b.id)? {
                        return Err(StoreError::BucketNotEmpty(name.to_string()));
                    }
                    let own = c
                        .prepare(&format!("SELECT {} FROM objects WHERE bucket_id = ?1 LIMIT ?2", rows::COLS))?
                        .query_map(params![&b.id, batch], |r| rows::row_at(r, 0))?
                        .collect::<rusqlite::Result<Vec<Row>>>()?;
                    for r in &own {
                        if r.lock.locked(now) {
                            return Err(locked());
                        }
                        rows::delete_own_row(c, &b.id, &r.key)?;
                        if let Some(blob) = &r.blob_id {
                            rows::add_ref(c, blob, -1, now)?;
                        }
                    }
                    let versions = c
                        .prepare(&format!("SELECT {} FROM versions WHERE bucket_id = ?1 LIMIT ?2", rows::COLS))?
                        .query_map(params![&b.id, batch], |r| rows::row_at(r, 0))?
                        .collect::<rusqlite::Result<Vec<Row>>>()?;
                    for r in &versions {
                        if r.lock.locked(now) {
                            return Err(locked());
                        }
                        rows::version_delete(c, &b.id, r)?;
                        if let Some(blob) = &r.blob_id {
                            rows::add_ref(c, blob, -1, now)?;
                        }
                    }
                    let trash = c
                        .prepare("SELECT key, deleted_at, blob_id, size, legal_hold, lock_mode, lock_until FROM trash WHERE bucket_id = ?1 LIMIT ?2")?
                        .query_map(params![&b.id, batch], |r| {
                            let lock = ObjectLock { legal_hold: r.get::<_, Option<i64>>(4)?.map(|v| v != 0), mode: r.get(5)?, retain_until: r.get(6)? };
                            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, lock))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    for (key, at, blob, size, lock) in &trash {
                        if lock.locked(now) {
                            return Err(locked());
                        }
                        rows::trash_delete(c, &b.id, key, *at, *size as u64)?;
                        rows::add_ref(c, blob, -1, now)?;
                    }
                    if (own.is_empty() && versions.is_empty() && trash.is_empty()) || !force {
                        let uploads: i64 = c.query_row("SELECT count(*) FROM uploads WHERE bucket_id = ?1", [&b.id], |r| r.get(0))?;
                        if uploads > 0 {
                            return Err(StoreError::Unavailable("uploads started while the bucket was being deleted; retry".into()));
                        }
                        crate::config::drop_bucket_config(c, &b.id)?;
                        c.execute("DELETE FROM buckets WHERE id = ?1", [&b.id])?;
                        return Ok(true);
                    }
                    Ok(false)
                })
                .await?;
            if done {
                return Ok(());
            }
        }
    }

    // ---- Keys ----

    /// A new access key; the secret is only ever returned here.
    pub async fn create_key(&self, name: &str, permission: Permission, buckets: Option<Vec<String>>) -> Result<AccessKey> {
        if name.is_empty() || name.len() > 128 {
            return Err(StoreError::InvalidArgument("key names are 1-128 characters".into()));
        }
        let key = AccessKey { id: ids::access_key_id(), secret: ids::secret_key(), name: name.to_string(), permission, buckets, created_at: self.now(), last_used_at: None, session: None };
        let k = key.clone();
        self.write(move |c| {
            let buckets = k.buckets.as_ref().map(rows::json).transpose()?;
            c.execute(
                &format!("INSERT INTO keys ({KEY_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)"),
                params![k.id, k.secret, k.name, k.permission.as_str(), buckets, k.created_at],
            )?;
            Ok(())
        })
        .await?;
        Ok(key)
    }

    /// Make sure an admin key with this id and secret exists (`ROLL_S3_ACCESS_KEY`).
    pub async fn ensure_key(&self, id: &str, secret: &str, name: &str) -> Result<AccessKey> {
        if id.len() < 3 || id.len() > 128 || secret.len() < 8 {
            return Err(StoreError::InvalidArgument("access key ids are 3-128 characters, secrets at least 8".into()));
        }
        let now = self.now();
        self.write(|c| {
            c.execute(
                &format!(
                    "INSERT INTO keys ({KEY_COLS}) VALUES (?1, ?2, ?3, 'admin', NULL, ?4, NULL)
                     ON CONFLICT (id) DO UPDATE SET secret = excluded.secret, permission = 'admin', buckets = NULL"
                ),
                params![id, secret, name, now],
            )?;
            Ok(())
        })
        .await?;
        self.access_key(id)?.ok_or_else(|| StoreError::internal("key vanished"))
    }

    pub fn keys(&self) -> Result<Vec<AccessKey>> {
        self.read(|c| Ok(c.prepare(&format!("SELECT {KEY_COLS} FROM keys ORDER BY created_at, id"))?.query_map([], key_row)?.collect::<rusqlite::Result<Vec<_>>>()?))
    }

    /// The key (with its secret) for request signing.
    pub fn access_key(&self, id: &str) -> Result<Option<AccessKey>> {
        self.read(|c| Ok(c.prepare_cached(&format!("SELECT {KEY_COLS} FROM keys WHERE id = ?1"))?.query_row([id], key_row).optional()?))
    }

    pub async fn delete_key(&self, id: &str) -> Result<()> {
        let n = self.write(|c| Ok(c.execute("DELETE FROM keys WHERE id = ?1", [id])?)).await?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("no access key {id}")));
        }
        Ok(())
    }

    /// Note that a key was used (written at most hourly per key, not waited for).
    pub fn touch_key(&self, id: &str) {
        let now = self.now();
        {
            let mut t = self.touched.lock().unwrap();
            if t.get(id).is_some_and(|at| now - at < 3_600_000) {
                return;
            }
            t.insert(id.to_string(), now);
        }
        if let Err(e) = self.write_relaxed(|c| Ok(c.execute("UPDATE keys SET last_used_at = ?2 WHERE id = ?1", params![id, now])?)) {
            tracing::debug!(error = %e, "recording key use failed");
        }
    }

    // ---- Usage ----

    /// Move the meter's counters into `usage`, and add stored bytes × elapsed time for every
    /// bucket.
    pub fn flush_meter(&self) -> Result<()> {
        let now = self.now();
        let taken = self.meter.take();
        let elapsed = {
            let mut last = self.last_flush.lock().unwrap();
            let e = (now - *last).max(0);
            *last = now;
            e
        };
        let day = ids::day_of(now);
        let result = self.write_relaxed(|c| {
            let sizes: Vec<(String, i64)> =
                c.prepare("SELECT id, bytes + trash_bytes + version_bytes FROM buckets")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let known: HashSet<&str> = sizes.iter().map(|(id, _)| id.as_str()).collect();
            let mut st = c.prepare_cached(
                "INSERT INTO usage (bucket_id, day, class_a, class_b, bytes_in, egress_internet, egress_internal, byte_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (bucket_id, day) DO UPDATE SET class_a = class_a + excluded.class_a, class_b = class_b + excluded.class_b,
                   bytes_in = bytes_in + excluded.bytes_in, egress_internet = egress_internet + excluded.egress_internet,
                   egress_internal = egress_internal + excluded.egress_internal, byte_ms = byte_ms + excluded.byte_ms",
            )?;
            for (id, bytes) in &sizes {
                let t = taken.get(id).cloned().unwrap_or_default();
                let byte_ms = *bytes as f64 * elapsed as f64;
                if byte_ms == 0.0 && t == Default::default() {
                    continue;
                }
                st.execute(params![id, day, t.class_a as i64, t.class_b as i64, t.bytes_in as i64, t.egress_internet as i64, t.egress_internal as i64, byte_ms])?;
            }
            // Requests to buckets deleted meanwhile still count.
            for (id, t) in &taken {
                if !known.contains(id.as_str()) {
                    st.execute(params![id, day, t.class_a as i64, t.class_b as i64, t.bytes_in as i64, t.egress_internet as i64, t.egress_internal as i64, 0.0])?;
                }
            }
            Ok(())
        });
        if let Err(e) = result {
            self.meter.restore(taken);
            return Err(e);
        }
        Ok(())
    }

    /// Metered days from `since_day` (`YYYY-MM-DD`) on, of one bucket or all.
    pub fn usage(&self, bucket_id: Option<&str>, since_day: &str) -> Result<Vec<UsageDay>> {
        self.read(|c| {
            let map = |r: &rusqlite::Row| {
                Ok(UsageDay {
                    bucket_id: r.get(0)?,
                    day: r.get(1)?,
                    class_a: r.get::<_, i64>(2)? as u64,
                    class_b: r.get::<_, i64>(3)? as u64,
                    bytes_in: r.get::<_, i64>(4)? as u64,
                    egress_internet: r.get::<_, i64>(5)? as u64,
                    egress_internal: r.get::<_, i64>(6)? as u64,
                    byte_ms: r.get(7)?,
                })
            };
            const Q: &str = "SELECT bucket_id, day, class_a, class_b, bytes_in, egress_internet, egress_internal, byte_ms FROM usage";
            let rows = match bucket_id {
                Some(b) => c.prepare(&format!("{Q} WHERE bucket_id = ?1 AND day >= ?2 ORDER BY day"))?.query_map(params![b, since_day], map)?.collect::<rusqlite::Result<Vec<_>>>()?,
                None => c.prepare(&format!("{Q} WHERE day >= ?1 ORDER BY day, bucket_id"))?.query_map([since_day], map)?.collect::<rusqlite::Result<Vec<_>>>()?,
            };
            Ok(rows)
        })
    }

    // ---- Blobs ----

    /// A fresh blob id, registered as in flight until [`Self::release_blob`] (commits do it).
    pub fn new_blob(&self) -> String {
        let id = ids::blob_id();
        self.inflight.lock().unwrap().insert(id.clone());
        id
    }

    /// The blob is committed (or discarded): it's no longer in flight.
    pub fn release_blob(&self, blob: &str) {
        self.inflight.lock().unwrap().remove(blob);
    }

    /// Mark `blob` in flight until the guard drops, whichever way the caller exits (an error,
    /// or its future dropped by a client that went away). Releasing never deletes anything:
    /// a blob that didn't get committed is left to the orphan scan.
    pub(crate) fn inflight_guard(&self, blob: &str) -> Inflight<'_> {
        self.inflight.lock().unwrap().insert(blob.to_string());
        Inflight { store: self, blob: blob.to_string() }
    }

    /// Whether the metadata knows `blob` (committed, in the trash, or quarantined).
    pub(crate) fn blob_committed(&self, blob: &str) -> Result<bool> {
        self.log.db.read(|c| Ok(c.query_row("SELECT 1 FROM blobs WHERE id = ?1", [blob], |_| Ok(())).optional()?.is_some()))
    }

    /// Upload a new object's bytes (the blob from [`Self::new_blob`]); commit it with
    /// [`Self::commit_put`].
    pub async fn put_blob(&self, blob: &str, bucket_id: &str, key: &str, body: ByteStream, len: u64, content_type: Option<&str>) -> Result<Uploaded> {
        self.put_blob_sse(blob, bucket_id, key, body, len, content_type, None).await
    }

    /// [`Self::put_blob`] with server-side encryption; commit it with
    /// [`Self::commit_put_sse`].
    #[allow(clippy::too_many_arguments)]
    pub async fn put_blob_sse(&self, blob: &str, bucket_id: &str, key: &str, body: ByteStream, len: u64, content_type: Option<&str>, sse: Option<&crate::backend::BlobSse>) -> Result<Uploaded> {
        if let Err(e) = self.check_sse_supported(sse) {
            self.release_blob(blob);
            return Err(e);
        }
        let attrs = BlobAttrs { bucket_id: bucket_id.to_string(), key: key.to_string(), content_type: content_type.map(str::to_string), sse: sse.cloned() };
        match self.backend.put(&ids::blob_key(blob), body, len, &attrs).await {
            Ok(u) => Ok(u),
            Err(e) => {
                self.release_blob(blob);
                Err(e.into())
            }
        }
    }

    /// Delete an uploaded blob that was never committed. A blob the metadata knows (an object,
    /// the trash, quarantine or an upload references it) is never deleted here, whatever the
    /// caller thinks: it's left to the sweeper and the orphan scan.
    pub async fn discard_blob(&self, blob: &str) {
        let known = self.log.db.read(|c| {
            Ok(c.query_row("SELECT 1 FROM blobs WHERE id = ?1 UNION ALL SELECT 1 FROM uploads WHERE blob_id = ?1 LIMIT 1", [blob], |_| Ok(())).optional()?.is_some())
        });
        if !matches!(known, Ok(false)) {
            tracing::error!(blob, "not discarding a blob the metadata references");
            self.release_blob(blob);
            return;
        }
        if let Err(e) = self.backend.delete(&ids::blob_key(blob)).await {
            tracing::warn!(error = %e, blob, "discarding an uncommitted blob failed; the orphan scan will");
        }
        self.release_blob(blob);
    }

    /// An object's bytes (or `range` of them, clamped to its size), and how many there are.
    pub async fn open_blob(&self, meta: &ObjectMeta, range: Option<std::ops::Range<u64>>) -> Result<(u64, ByteStream)> {
        self.open_blob_sse(meta, range, None).await
    }

    /// [`Self::open_blob`] with the request's SSE-C key: an SSE-C object can only be read with
    /// its own key (`InvalidArgument` otherwise).
    pub async fn open_blob_sse(&self, meta: &ObjectMeta, range: Option<std::ops::Range<u64>>, sse: Option<&crate::backend::BlobSse>) -> Result<(u64, ByteStream)> {
        let key = crate::sse::key_for(self.blob_encryption(&meta.blob_id)?.as_ref(), sse, &meta.key)?;
        match self.backend.get_blob(&ids::blob_key(&meta.blob_id), range, key.as_ref()).await? {
            Some(x) => Ok(x),
            None => {
                tracing::error!(blob = meta.blob_id, key = meta.key, "blob missing from object storage");
                Err(StoreError::internal(format!("the data of {} is missing from object storage", meta.key)))
            }
        }
    }
}

/// See [`Store::inflight_guard`].
pub(crate) struct Inflight<'a> {
    store: &'a Store,
    blob: String,
}

impl Drop for Inflight<'_> {
    fn drop(&mut self) {
        self.store.release_blob(&self.blob);
    }
}

/// Run `f` every `every` while the store is alive.
pub(crate) fn every<F, Fut>(store: &Arc<Store>, every: Option<Duration>, what: &'static str, f: F)
where
    F: Fn(Arc<Store>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send,
{
    let Some(every) = every else { return };
    let weak = Arc::downgrade(store);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            let Some(store) = weak.upgrade() else { return };
            if store.log.check_open().is_err() {
                return;
            }
            if let Err(e) = f(store).await {
                tracing::warn!(error = %e, job = what, "storage background job failed");
            }
        }
    });
}
