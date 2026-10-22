//! Regression tests for the reliability review: races that could lose an acknowledged object,
//! fencing that misfires, and recovery fallbacks. A wrapping backend injects the faults.

mod common;

use async_trait::async_trait;
use bytes::Bytes;
use common::*;
use s3_store::*;
use std::collections::HashSet;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A backend that misbehaves on demand.
struct Faulty {
    inner: Arc<dyn Backend>,
    /// Create-only writes overwrite and report "created" (a service ignoring If-None-Match).
    ignore_create_only: AtomicBool,
    /// This many create-only writes of log entries land, but report "exists" (the response was
    /// lost and the client library's retry got a 412).
    lose_create_response: AtomicU32,
    /// Reads of keys containing the pattern fail this many times.
    fail_gets: Mutex<Option<(String, u32)>>,
    /// Log entry writes wait this long.
    wal_delay_ms: AtomicU64,
    /// Completing a multipart upload waits this long; completing it again succeeds, as on S3.
    complete_delay_ms: AtomicU64,
    completed: Mutex<HashSet<String>>,
    /// Aborting a multipart upload waits this long.
    abort_delay_ms: AtomicU64,
    /// The next log entry write finds its slot taken by another writer's (valid, empty) entry.
    steal_wal: AtomicBool,
    /// Log entry writes fail this many times (`u32::MAX`: for good).
    fail_wal: AtomicU32,
    /// Log entry writes panic.
    panic_wal: AtomicBool,
    /// Reads of snapshots return garbage this many times.
    corrupt_snap_reads: AtomicU32,
}

impl Faulty {
    fn new(inner: Arc<dyn Backend>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            ignore_create_only: AtomicBool::new(false),
            lose_create_response: AtomicU32::new(0),
            fail_gets: Mutex::new(None),
            wal_delay_ms: AtomicU64::new(0),
            complete_delay_ms: AtomicU64::new(0),
            completed: Mutex::new(HashSet::new()),
            abort_delay_ms: AtomicU64::new(0),
            steal_wal: AtomicBool::new(false),
            fail_wal: AtomicU32::new(0),
            panic_wal: AtomicBool::new(false),
            corrupt_snap_reads: AtomicU32::new(0),
        })
    }

    fn fail_gets(&self, pattern: &str, times: u32) {
        *self.fail_gets.lock().unwrap() = Some((pattern.to_string(), times));
    }
}

async fn sleep_ms(ms: u64) {
    if ms > 0 {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
}

#[async_trait]
impl Backend for Faulty {
    fn describe(&self) -> String {
        format!("faulty {}", self.inner.describe())
    }

    async fn put(&self, key: &str, body: ByteStream, len: u64, attrs: &BlobAttrs) -> anyhow::Result<Uploaded> {
        self.inner.put(key, body, len, attrs).await
    }

    async fn put_small(&self, key: &str, data: Bytes) -> anyhow::Result<()> {
        self.inner.put_small(key, data).await
    }

    async fn put_create_only(&self, key: &str, data: Bytes) -> anyhow::Result<bool> {
        if key.contains("/wal/") {
            sleep_ms(self.wal_delay_ms.load(Ordering::SeqCst)).await;
            assert!(!self.panic_wal.load(Ordering::SeqCst), "injected panic writing {key}");
            if self.fail_wal.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n > 0).then(|| if n == u32::MAX { n } else { n - 1 })).is_ok() {
                anyhow::bail!("injected failure writing {key}");
            }
            if self.steal_wal.swap(false, Ordering::SeqCst) {
                let seq: u64 = key.rsplit('/').next().unwrap().parse().unwrap();
                self.inner.put_create_only(key, entry(seq, 1, &[])).await?;
            }
        }
        if self.ignore_create_only.load(Ordering::SeqCst) {
            self.inner.put_small(key, data).await?;
            return Ok(true);
        }
        if key.contains("/wal/") && self.lose_create_response.load(Ordering::SeqCst) > 0 {
            self.lose_create_response.fetch_sub(1, Ordering::SeqCst);
            self.inner.put_create_only(key, data).await?;
            return Ok(false);
        }
        self.inner.put_create_only(key, data).await
    }

    async fn get(&self, key: &str, range: Option<Range<u64>>) -> anyhow::Result<Option<(u64, ByteStream)>> {
        {
            let mut f = self.fail_gets.lock().unwrap();
            if let Some((pattern, n)) = f.as_mut()
                && *n > 0
                && key.contains(pattern.as_str())
            {
                *n -= 1;
                anyhow::bail!("injected read failure of {key}");
            }
        }
        if key.contains("/snap/") && self.corrupt_snap_reads.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok() {
            return Ok(Some((7, bytes_body(Bytes::from_static(b"garbage")))));
        }
        self.inner.get(key, range).await
    }

    async fn head(&self, key: &str) -> anyhow::Result<Option<Listed>> {
        self.inner.head(key).await
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.inner.delete(key).await
    }

    async fn list_page(&self, prefix: &str, start_after: Option<&str>, max: usize) -> anyhow::Result<(Vec<Listed>, bool)> {
        self.inner.list_page(prefix, start_after, max).await
    }

    async fn create_multipart(&self, key: &str, attrs: &BlobAttrs) -> anyhow::Result<String> {
        self.inner.create_multipart(key, attrs).await
    }

    async fn upload_part(&self, key: &str, upload_id: &str, part: u32, body: ByteStream, len: u64) -> anyhow::Result<UploadedPart> {
        self.inner.upload_part(key, upload_id, part, body, len).await
    }

    async fn upload_part_copy(&self, key: &str, upload_id: &str, part: u32, src_key: &str, range: Option<Range<u64>>) -> anyhow::Result<UploadedPart> {
        self.inner.upload_part_copy(key, upload_id, part, src_key, range).await
    }

    async fn complete_multipart(&self, key: &str, upload_id: &str, parts: &[(u32, String)]) -> anyhow::Result<()> {
        sleep_ms(self.complete_delay_ms.load(Ordering::SeqCst)).await;
        if self.completed.lock().unwrap().contains(upload_id) {
            return Ok(());
        }
        self.inner.complete_multipart(key, upload_id, parts).await?;
        self.completed.lock().unwrap().insert(upload_id.to_string());
        Ok(())
    }

    async fn abort_multipart(&self, key: &str, upload_id: &str) -> anyhow::Result<()> {
        sleep_ms(self.abort_delay_ms.load(Ordering::SeqCst)).await;
        self.inner.abort_multipart(key, upload_id).await
    }
}

async fn faulty_env(name: &str) -> (Arc<Faulty>, Arc<dyn Backend>, TestClock, Arc<Store>) {
    let faulty = Faulty::new(test_backend(name).await);
    let backend: Arc<dyn Backend> = faulty.clone();
    let clock = TestClock::default();
    let store = open(&backend, &clock).await;
    (faulty, backend, clock, store)
}

async fn part(s: &Store, up: &Upload, n: u32, data: &[u8]) -> Part {
    let d = s.put_part_data(up, n, bytes_body(Bytes::copy_from_slice(data)), data.len() as u64).await.unwrap();
    s.commit_part(up, n, &d, &md5_hex(data), None).await.unwrap()
}

/// Run every GC job as days pass: the orphan scan records a candidate on one pass and deletes
/// it a day later (a blob already older than a week is only reported), then the sweeper
/// outlives the trash and the quarantine.
async fn collect_everything(s: &Store, clock: &TestClock) {
    for days in [2, 2, 10] {
        clock.advance(days * DAY);
        s.sweep().await.unwrap();
        s.orphan_scan().await.unwrap();
    }
}

fn wal_key(seq: u64) -> String {
    format!("s3/meta/wal/{seq:020}")
}

async fn wal_seqs(backend: &Arc<dyn Backend>) -> Vec<u64> {
    let mut out: Vec<u64> = s3_store::backend::list_all(&**backend, "s3/meta/wal/")
        .await
        .unwrap()
        .iter()
        .map(|l| l.key.rsplit('/').next().unwrap().parse().unwrap())
        .collect();
    out.sort_unstable();
    out
}

/// A log entry as the store writes them: magic, header, patchset, CRC32C of all that.
fn entry(seq: u64, schema: u32, patch: &[u8]) -> Bytes {
    let meta = serde_json::to_vec(&serde_json::json!({ "seq": seq, "kind": "commit", "node": "test", "at": 0, "schema": schema })).unwrap();
    let mut out = b"RS3LOG1\n".to_vec();
    out.extend_from_slice(&(meta.len() as u32).to_be_bytes());
    out.extend_from_slice(&meta);
    out.extend_from_slice(patch);
    let crc = crc32c::crc32c(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out.into()
}

fn refuses(r: Result<Arc<Store>>, wanted: &str) {
    let err = r.err().expect("must refuse to start").to_string();
    assert!(err.contains(wanted), "{err}");
}

// ---- Multipart completion ----

#[tokio::test]
async fn a_retried_complete_returns_the_committed_object() {
    let (faulty, backend, clock, s) = faulty_env("retrycomplete").await;
    s.create_bucket("mpu", BucketOptions::default()).await.unwrap();
    let up = s.create_upload("mpu", "obj", ObjectHeaders::default(), None, None).await.unwrap();
    // Real S3 wants 5 MiB for every part but the last.
    let first = [&b"hello "[..], &vec![b'.'; 5 << 20]].concat();
    let p1 = part(&s, &up, 1, &first).await;
    let p2 = part(&s, &up, 2, b"world").await;
    let list = [(1, p1.etag.clone()), (2, p2.etag.clone())];
    // The first attempt is slow; the client retries while it's still running.
    faulty.complete_delay_ms.store(200, Ordering::SeqCst);
    let none = WriteCondition::none();
    let (a, b) = tokio::join!(s.complete_upload("mpu", &up.id, &list, None, &none), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        s.complete_upload("mpu", &up.id, &list, None, &none).await
    });
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!((a.etag.as_str(), a.blob_id.as_str()), (b.etag.as_str(), b.blob_id.as_str()));
    // Once it's over, the upload is gone for good.
    assert!(matches!(s.complete_upload("mpu", &up.id, &list, None, &none).await, Err(StoreError::NoSuchUpload(_))));
    collect_everything(&s, &clock).await;
    assert!(read(&s, "mpu", "obj").await.unwrap() == [&first[..], b"world"].concat());
    assert!(blobs(&backend).await.contains(&blob_key(&a.blob_id)));
}

#[tokio::test]
async fn a_complete_after_the_key_moved_on_is_no_such_upload() {
    let (faulty, _backend, _clock, s) = faulty_env("movedon").await;
    s.create_bucket("mpu", BucketOptions::default()).await.unwrap();
    let up = s.create_upload("mpu", "obj", ObjectHeaders::default(), None, None).await.unwrap();
    let p1 = part(&s, &up, 1, b"one").await;
    faulty.complete_delay_ms.store(200, Ordering::SeqCst);
    let none = WriteCondition::none();
    let list = [(1, p1.etag.clone())];
    let (a, b) = tokio::join!(s.complete_upload("mpu", &up.id, &list, None, &none), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Queued behind the first; by the time it runs the key has a newer version.
        let r = s.complete_upload("mpu", &up.id, &list, None, &none);
        tokio::pin!(r);
        tokio::select! {
            r = &mut r => r,
            _ = tokio::time::sleep(Duration::from_secs(10)) => panic!("hung"),
        }
    });
    // Whichever ran second either saw its own result or a newer version.
    assert!(a.is_ok());
    assert!(b.is_ok() || matches!(b, Err(StoreError::NoSuchUpload(_))));
    put(&s, "mpu", "obj", b"newer").await;
    assert!(matches!(s.complete_upload("mpu", &up.id, &list, None, &none).await, Err(StoreError::NoSuchUpload(_))));
    assert_eq!(read(&s, "mpu", "obj").await.unwrap(), b"newer");
}

#[tokio::test]
async fn a_failed_complete_condition_keeps_the_upload() {
    let (_faulty, _backend, _clock, s) = faulty_env("condcomplete").await;
    s.create_bucket("mpu", BucketOptions::default()).await.unwrap();
    put(&s, "mpu", "obj", b"exists").await;
    let up = s.create_upload("mpu", "obj", ObjectHeaders::default(), None, None).await.unwrap();
    let p1 = part(&s, &up, 1, b"one").await;
    let list = [(1, p1.etag.clone())];
    let only_new = WriteCondition { if_match: None, if_none_match: Some("*".into()) };
    assert!(matches!(s.complete_upload("mpu", &up.id, &list, None, &only_new).await, Err(StoreError::PreconditionFailed)));
    // Nothing was assembled: the upload can still be completed.
    s.complete_upload("mpu", &up.id, &list, None, &WriteCondition::none()).await.unwrap();
    assert_eq!(read(&s, "mpu", "obj").await.unwrap(), b"one");
}

// ---- Bucket deletion ----

#[tokio::test]
async fn a_put_during_a_bucket_delete_survives() {
    let (faulty, _backend, _clock, s) = faulty_env("delrace").await;
    s.create_bucket("racy", BucketOptions::default()).await.unwrap();
    // An open upload makes the delete abort it first (slowly), after its emptiness check.
    s.create_upload("racy", "pending", ObjectHeaders::default(), None, None).await.unwrap();
    faulty.abort_delay_ms.store(300, Ordering::SeqCst);
    let (deleted, ()) = tokio::join!(s.delete_bucket("racy", false), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        put(&s, "racy", "landed", b"acknowledged").await;
    });
    assert!(matches!(deleted, Err(StoreError::BucketNotEmpty(_))), "{deleted:?}");
    assert_eq!(read(&s, "racy", "landed").await.unwrap(), b"acknowledged");
}

#[tokio::test]
async fn a_non_force_delete_of_a_branch_is_all_or_nothing() {
    let (_faulty, _backend, _clock, s) = faulty_env("delbranch").await;
    s.create_bucket("main", BucketOptions::default()).await.unwrap();
    for i in 0..5 {
        put(&s, "main", &format!("k{i}"), b"x").await;
    }
    s.branch_bucket("main", "brn").await.unwrap();
    for i in 0..4 {
        s.delete_object("brn", &format!("k{i}"), None).await.unwrap();
    }
    // One inherited key is still visible: the delete must not touch the tombstones.
    assert!(matches!(s.delete_bucket("brn", false).await, Err(StoreError::BucketNotEmpty(_))));
    assert_eq!(keys(&s, "brn"), ["k4"]);
    s.delete_object("brn", "k4", None).await.unwrap();
    s.delete_bucket("brn", false).await.unwrap();
    assert_eq!(keys(&s, "main").len(), 5);
}

// ---- Lifecycle ----

#[tokio::test]
async fn lifecycle_never_expires_a_version_written_after_the_cutoff() {
    let (_faulty, _backend, clock, s) = faulty_env("lcrace").await;
    s.create_bucket("lcy", BucketOptions::default()).await.unwrap();
    let rule = LifecycleRule { id: "old".into(), prefix: "logs/".into(), enabled: true, expire_days: Some(1), abort_multipart_days: None };
    s.update_bucket("lcy", BucketUpdate { lifecycle: Some(vec![rule]), ..Default::default() }).await.unwrap();
    put(&s, "lcy", "logs/a", b"old").await;
    put(&s, "lcy", "logs/b", b"old").await;
    clock.advance(2 * DAY);
    put(&s, "lcy", "logs/b", b"fresh").await;
    assert_eq!(s.run_lifecycle().await.unwrap(), 1);
    assert_eq!(keys(&s, "lcy"), ["logs/b"]);
    assert_eq!(read(&s, "lcy", "logs/b").await.unwrap(), b"fresh");
}

// ---- Fencing ----

#[tokio::test]
async fn a_lost_commit_response_doesnt_fence() {
    let (faulty, backend, clock, s) = faulty_env("lostresp").await;
    s.create_bucket("lrs", BucketOptions::default()).await.unwrap();
    faulty.lose_create_response.store(2, Ordering::SeqCst);
    put(&s, "lrs", "a", b"1").await;
    put(&s, "lrs", "b", b"2").await;
    assert_eq!(faulty.lose_create_response.load(Ordering::SeqCst), 0, "both commits went through the lossy path");
    assert!(s.stats().unwrap().log.fenced.is_none());
    put(&s, "lrs", "c", b"3").await;
    let s2 = open(&backend, &clock).await;
    assert_eq!(keys(&s2, "lrs"), ["a", "b", "c"]);
}

#[tokio::test]
async fn a_taken_log_slot_with_other_bytes_fences() {
    let (_faulty, backend, _clock, s) = faulty_env("takenslot").await;
    s.create_bucket("tsl", BucketOptions::default()).await.unwrap();
    let next = wal_seqs(&backend).await.last().unwrap() + 1;
    // Another writer got there first.
    assert!(backend.put_create_only(&wal_key(next), entry(next, 1, &[])).await.unwrap());
    let r = put_with(&s, "tsl", "k", b"v", &WriteCondition::none()).await;
    assert!(matches!(r, Err(StoreError::Unavailable(_))), "{r:?}");
    assert!(matches!(s.bucket("tsl"), Err(StoreError::Unavailable(_))), "reads stop too");
}

#[tokio::test]
async fn object_storage_ignoring_create_only_is_refused() {
    let faulty = Faulty::new(test_backend("nocreateonly").await);
    faulty.ignore_create_only.store(true, Ordering::SeqCst);
    let backend: Arc<dyn Backend> = faulty;
    refuses(Store::open(config(&TestClock::default()), backend.clone()).await, "create-only");
    // And the probe cleans up after itself.
    assert!(s3_store::backend::list_all(&*backend, "s3/meta/probe/").await.unwrap().is_empty());
}

// ---- Recovery ----

#[tokio::test]
async fn recovery_walks_past_two_corrupt_snapshots() {
    let (_faulty, backend, clock, s) = faulty_env("twobad").await;
    s.create_bucket("rbk", BucketOptions::default()).await.unwrap();
    put(&s, "rbk", "before", b"1").await;
    let good = s.snapshot_now().await.unwrap();
    for i in 0..4 {
        put(&s, "rbk", &format!("after{i}"), b"2").await;
    }
    let last = *wal_seqs(&backend).await.last().unwrap();
    assert!(last >= good + 3);
    // Bit rot in two newer snapshots (the log after them is intact).
    let bad = [good + 1, good + 2];
    for seq in bad {
        backend.put_small(&format!("s3/meta/snap/{seq:020}.db.zst"), Bytes::from_static(b"rotten")).await.unwrap();
    }
    let s2 = open(&backend, &clock).await;
    assert_eq!(s2.stats().unwrap().log.last_snapshot_seq, good);
    assert_eq!(keys(&s2, "rbk"), ["after0", "after1", "after2", "after3", "before"]);
    // The next verified snapshot clears them away.
    s2.snapshot_now().await.unwrap();
    let left: Vec<String> = s3_store::backend::list_all(&*backend, "s3/meta/snap/").await.unwrap().into_iter().map(|l| l.key).collect();
    for seq in bad {
        assert!(!left.iter().any(|k| k.contains(&format!("{seq:020}"))), "{left:?}");
    }
}

#[tokio::test]
async fn transient_read_errors_are_retried_not_taken_for_corruption() {
    let (faulty, backend, clock, s) = faulty_env("transient").await;
    s.create_bucket("trn", BucketOptions::default()).await.unwrap();
    put(&s, "trn", "a", b"1").await;
    let snap = s.snapshot_now().await.unwrap();
    put(&s, "trn", "b", b"2").await;
    faulty.fail_gets("/snap/", 2);
    let s2 = open(&backend, &clock).await;
    assert_eq!(s2.stats().unwrap().log.last_snapshot_seq, snap, "the newest snapshot was used");
    assert_eq!(keys(&s2, "trn"), ["a", "b"]);
    // Log entries too.
    faulty.fail_gets("/wal/", 2);
    let s3 = open(&backend, &clock).await;
    assert_eq!(keys(&s3, "trn"), ["a", "b"]);
}

#[tokio::test]
async fn an_unreadable_newest_snapshot_falls_back_to_the_older_one() {
    let (faulty, backend, clock, s) = faulty_env("unreadable").await;
    s.create_bucket("urb", BucketOptions::default()).await.unwrap();
    put(&s, "urb", "a", b"1").await;
    let older = s.snapshot_now().await.unwrap();
    put(&s, "urb", "b", b"2").await;
    let newest = s.snapshot_now().await.unwrap();
    put(&s, "urb", "c", b"3").await;
    faulty.fail_gets(&format!("{newest:020}.db.zst"), 1000);
    let s2 = open(&backend, &clock).await;
    assert_eq!(s2.stats().unwrap().log.last_snapshot_seq, older);
    assert_eq!(keys(&s2, "urb"), ["a", "b", "c"]);
}

#[tokio::test]
async fn a_missing_log_entry_stops_recovery() {
    let (_faulty, backend, clock, s) = faulty_env("gap").await;
    s.create_bucket("gap", BucketOptions::default()).await.unwrap();
    for i in 0..3 {
        put(&s, "gap", &format!("k{i}"), b"x").await;
    }
    let seqs = wal_seqs(&backend).await;
    backend.delete(&wal_key(seqs[seqs.len() / 2])).await.unwrap();
    refuses(Store::open(config(&clock), backend.clone()).await, "gap");
}

#[tokio::test]
async fn a_missing_log_entry_after_a_snapshot_stops_recovery() {
    let (_faulty, backend, clock, s) = faulty_env("gapsnap").await;
    s.create_bucket("gps", BucketOptions::default()).await.unwrap();
    put(&s, "gps", "a", b"x").await;
    let snap = s.snapshot_now().await.unwrap();
    for i in 0..3 {
        put(&s, "gps", &format!("k{i}"), b"x").await;
    }
    let after: Vec<u64> = wal_seqs(&backend).await.into_iter().filter(|q| *q > snap + 1).collect();
    backend.delete(&wal_key(after[0])).await.unwrap();
    // The snapshot can't be brought up to date, and an older state is not an answer.
    refuses(Store::open(config(&clock), backend.clone()).await, "gap");
}

#[tokio::test]
async fn a_log_entry_from_a_newer_version_stops_recovery() {
    let (_faulty, backend, clock, s) = faulty_env("newer").await;
    s.create_bucket("nvr", BucketOptions::default()).await.unwrap();
    let next = wal_seqs(&backend).await.last().unwrap() + 1;
    backend.put_small(&wal_key(next), entry(next, 9_999, &[])).await.unwrap();
    refuses(Store::open(config(&clock), backend.clone()).await, "newer version");
}

#[tokio::test]
async fn a_log_entry_under_the_wrong_number_stops_recovery() {
    let (_faulty, backend, clock, s) = faulty_env("wrongseq").await;
    s.create_bucket("wsq", BucketOptions::default()).await.unwrap();
    let next = wal_seqs(&backend).await.last().unwrap() + 1;
    backend.put_small(&wal_key(next), entry(next + 7, 1, &[])).await.unwrap();
    refuses(Store::open(config(&clock), backend.clone()).await, "claims to be");
}

#[tokio::test]
async fn a_change_that_doesnt_apply_stops_recovery() {
    let (_faulty, backend, clock, s) = faulty_env("noapply").await;
    s.create_bucket("nap", BucketOptions::default()).await.unwrap();
    // A patchset deleting an access key the metadata never had.
    let db = rusqlite::Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE keys (id TEXT PRIMARY KEY, secret TEXT NOT NULL, name TEXT NOT NULL, permission TEXT NOT NULL, buckets TEXT, created_at INTEGER NOT NULL, last_used_at INTEGER);
         INSERT INTO keys VALUES ('RKGHOST', 's', 'ghost', 'read', NULL, 0, NULL);",
    )
    .unwrap();
    let mut session = rusqlite::session::Session::new(&db).unwrap();
    session.attach(None::<&str>).unwrap();
    db.execute("DELETE FROM keys WHERE id = 'RKGHOST'", []).unwrap();
    let mut patch = Vec::new();
    session.patchset_strm(&mut patch).unwrap();
    assert!(!patch.is_empty());
    let next = wal_seqs(&backend).await.last().unwrap() + 1;
    backend.put_small(&wal_key(next), entry(next, 1, &patch)).await.unwrap();
    refuses(Store::open(config(&clock), backend.clone()).await, "doesn't apply");
}

// ---- GC safety around requests that go away ----

#[tokio::test]
async fn an_abandoned_upload_is_collected_but_a_dropped_commit_is_kept() {
    let (faulty, backend, clock, s) = faulty_env("dropped").await;
    s.create_bucket("drp", BucketOptions::default()).await.unwrap();
    let b = s.bucket("drp").unwrap();

    // A client that uploaded and went away before committing: never acknowledged.
    let abandoned = s.new_blob();
    s.put_blob(&abandoned, &b.id, "gone", bytes_body(Bytes::from_static(b"never")), 5, None).await.unwrap();
    s.release_blob(&abandoned);

    // A client that went away while its commit was on its way to the log: it landed.
    faulty.wal_delay_ms.store(300, Ordering::SeqCst);
    let dropped = tokio::time::timeout(Duration::from_millis(50), put(&s, "drp", "landed", b"kept")).await;
    assert!(dropped.is_err(), "the commit was still waiting");
    tokio::time::sleep(Duration::from_millis(600)).await;
    faulty.wal_delay_ms.store(0, Ordering::SeqCst);
    let landed = s.get_object("drp", "landed").unwrap();

    collect_everything(&s, &clock).await;
    let left = blobs(&backend).await;
    assert!(!left.contains(&blob_key(&abandoned)), "the abandoned blob is collected");
    assert!(left.contains(&blob_key(&landed.blob_id)), "the committed one is kept");
    assert_eq!(read(&s, "drp", "landed").await.unwrap(), b"kept");
    let s2 = open(&backend, &clock).await;
    assert_eq!(read(&s2, "drp", "landed").await.unwrap(), b"kept");
}

#[tokio::test]
async fn a_blob_whose_commit_failed_definitely_is_deleted_at_once() {
    let (_faulty, backend, _clock, s) = faulty_env("definite").await;
    s.create_bucket("dfn", BucketOptions::default()).await.unwrap();
    put(&s, "dfn", "k", b"first").await;
    let before = blobs(&backend).await;
    let only_new = WriteCondition { if_match: None, if_none_match: Some("*".into()) };
    assert!(matches!(put_with(&s, "dfn", "k", b"second", &only_new).await, Err(StoreError::PreconditionFailed)));
    assert_eq!(blobs(&backend).await, before);
}

// ---- Failed commits ----

#[tokio::test]
async fn a_failed_complete_on_a_deposed_node_keeps_the_blob() {
    let (faulty, backend, clock, a) = faulty_env("deposedcomplete").await;
    a.create_bucket("mpu", BucketOptions::default()).await.unwrap();
    let up = a.create_upload("mpu", "obj", ObjectHeaders::default(), None, None).await.unwrap();
    let p1 = part(&a, &up, 1, b"assembled").await;
    let list = [(1, p1.etag.clone())];
    let only_new = WriteCondition { if_match: None, if_none_match: Some("*".into()) };
    // While the parts are assembled, a put takes the key (so the commit fails its condition)
    // and another node takes over the log (so dropping the upload can't commit either).
    faulty.complete_delay_ms.store(500, Ordering::SeqCst);
    let (r, b) = tokio::join!(a.complete_upload("mpu", &up.id, &list, None, &only_new), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        put(&a, "mpu", "obj", b"racing put").await;
        open(&backend, &clock).await
    });
    faulty.complete_delay_ms.store(0, Ordering::SeqCst);
    assert!(matches!(r, Err(StoreError::PreconditionFailed)), "{r:?}");
    // The new writer still has the upload, so the blob it references must still be there.
    b.upload("mpu", &up.id).unwrap();
    assert!(blobs(&backend).await.contains(&blob_key(&up.blob_id)), "the deposed node deleted a blob the metadata references");
    // The racing object goes away and the client retries the Complete on the new writer.
    b.delete_object("mpu", "obj", None).await.unwrap();
    let m = b.complete_upload("mpu", &up.id, &list, None, &only_new).await.unwrap();
    assert_eq!(m.blob_id, up.blob_id);
    collect_everything(&b, &clock).await;
    assert_eq!(read(&b, "mpu", "obj").await.unwrap(), b"assembled");
    assert_consistent(&b, &backend).await;
}

#[tokio::test]
async fn a_group_commit_that_loses_its_slot_fails_every_write_in_it() {
    let (faulty, backend, clock, s) = faulty_env("groupfail").await;
    s.create_bucket("grp", BucketOptions::default()).await.unwrap();
    put(&s, "grp", "before", b"1").await;
    let b = s.bucket("grp").unwrap();
    let mut objs = Vec::new();
    for i in 0..8 {
        let blob = s.new_blob();
        let up = s.put_blob(&blob, &b.id, "k", bytes_body(Bytes::from_static(b"v")), 1, None).await.unwrap();
        objs.push((format!("k{i}"), NewObject { blob_id: blob, size: up.size, crc32c: up.crc32c, etag: md5_hex(b"v"), headers: ObjectHeaders::default(), checksum: None }));
    }
    faulty.wal_delay_ms.store(100, Ordering::SeqCst);
    faulty.steal_wal.store(true, Ordering::SeqCst);
    let tasks: Vec<_> = objs
        .into_iter()
        .map(|(k, o)| {
            let s = s.clone();
            tokio::spawn(async move { s.commit_put("grp", &k, o, &WriteCondition::none()).await })
        })
        .collect();
    for t in tasks {
        let r = tokio::time::timeout(Duration::from_secs(10), t).await.expect("hung").unwrap();
        assert!(matches!(r, Err(StoreError::Unavailable(_))), "{r:?}");
    }
    assert!(s.stats().is_err(), "fenced");
    // Whether those commits landed isn't known here, so their blobs are kept.
    assert_eq!(blobs(&backend).await.len(), 9);
    faulty.wal_delay_ms.store(0, Ordering::SeqCst);
    let s2 = open(&backend, &clock).await;
    assert_eq!(keys(&s2, "grp"), ["before"], "nothing unacknowledged appears");
    assert_consistent(&s2, &backend).await;
    collect_everything(&s2, &clock).await;
    assert_eq!(blobs(&backend).await.len(), 1, "the new writer's orphan scan collects them");
    assert_eq!(read(&s2, "grp", "before").await.unwrap(), b"1");
}

#[tokio::test]
async fn transient_commit_failures_are_retried_without_fencing() {
    let (faulty, backend, clock, s) = faulty_env("walretry").await;
    s.create_bucket("wrt", BucketOptions::default()).await.unwrap();
    faulty.fail_wal.store(3, Ordering::SeqCst);
    put(&s, "wrt", "k", b"v").await;
    assert_eq!(faulty.fail_wal.load(Ordering::SeqCst), 0);
    assert!(s.stats().unwrap().log.fenced.is_none());
    let s2 = open(&backend, &clock).await;
    assert_eq!(keys(&s2, "wrt"), ["k"]);
}

#[tokio::test]
async fn object_storage_refusing_commits_fences_the_node() {
    let (faulty, backend, clock, s) = faulty_env("walfail").await;
    let bkt = s.create_bucket("wfl", BucketOptions::default()).await.unwrap();
    // The blob is uploaded first, so the put's commit is in the pipeline before the delete's
    // (however slow the backend is).
    let blob = s.new_blob();
    let up = s.put_blob(&blob, &bkt.id, "a", bytes_body(Bytes::from_static(b"1")), 1, None).await.unwrap();
    let obj = NewObject { blob_id: blob, size: up.size, crc32c: up.crc32c, etag: md5_hex(b"1"), headers: ObjectHeaders::default(), checksum: None };
    faulty.fail_wal.store(u32::MAX, Ordering::SeqCst);
    let started = std::time::Instant::now();
    let none = WriteCondition::none();
    let (a, b) = tokio::join!(s.commit_put("wfl", "a", obj, &none), async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        s.delete_bucket("wfl", true).await
    });
    assert!(matches!(a, Err(StoreError::Unavailable(_))), "{a:?}");
    assert!(matches!(b, Err(StoreError::Unavailable(_))), "{b:?}");
    assert!(started.elapsed() > Duration::from_secs(20), "retried for a while first");
    assert!(matches!(s.bucket("wfl"), Err(StoreError::Unavailable(_))));
    faulty.fail_wal.store(0, Ordering::SeqCst);
    let s2 = open(&backend, &clock).await;
    assert!(keys(&s2, "wfl").is_empty());
}

#[tokio::test]
async fn a_committer_panic_fences_instead_of_hanging() {
    let (faulty, backend, clock, s) = faulty_env("panic").await;
    s.create_bucket("pnc", BucketOptions::default()).await.unwrap();
    put(&s, "pnc", "before", b"1").await;
    faulty.panic_wal.store(true, Ordering::SeqCst);
    let r = tokio::time::timeout(Duration::from_secs(10), put_with(&s, "pnc", "lost", b"2", &WriteCondition::none())).await.expect("a write hung on a dead committer");
    assert!(matches!(r, Err(StoreError::Unavailable(_))), "{r:?}");
    assert!(matches!(s.bucket("pnc"), Err(StoreError::Unavailable(_))), "reads stop too");
    assert!(matches!(s.create_bucket("other", BucketOptions::default()).await, Err(StoreError::Unavailable(_))));
    faulty.panic_wal.store(false, Ordering::SeqCst);
    let s2 = open(&backend, &clock).await;
    assert_eq!(keys(&s2, "pnc"), ["before"]);
}

#[tokio::test]
async fn a_takeover_racing_another_writer_replays_its_entry() {
    let (faulty, backend, clock, a) = faulty_env("takeoverrace").await;
    a.create_bucket("tko", BucketOptions::default()).await.unwrap();
    put(&a, "tko", "k", b"v").await;
    let last = *wal_seqs(&backend).await.last().unwrap();
    // Between the new node's listing and its takeover, someone else writes the next entry.
    faulty.steal_wal.store(true, Ordering::SeqCst);
    let b = open(&backend, &clock).await;
    assert_eq!(b.stats().unwrap().log.seq, last + 2, "replayed the entry in its slot, then claimed the next");
    assert_eq!(read(&b, "tko", "k").await.unwrap(), b"v");
    put(&b, "tko", "after", b"x").await;
    assert!(matches!(put_with(&a, "tko", "stale", b"x", &WriteCondition::none()).await, Err(StoreError::Unavailable(_))));
    let c = open(&backend, &clock).await;
    assert_eq!(keys(&c, "tko"), ["after", "k"]);
}

// ---- Snapshots ----

fn snap_seqs(listed: Vec<Listed>) -> Vec<u64> {
    listed.iter().map(|l| l.key.rsplit('/').next().unwrap().split('.').next().unwrap().parse().unwrap()).collect()
}

#[tokio::test]
async fn a_snapshot_failing_its_check_never_counts_or_prunes() {
    let (faulty, backend, clock, s) = faulty_env("badverify").await;
    s.create_bucket("bvf", BucketOptions::default()).await.unwrap();
    put(&s, "bvf", "a", b"1").await;
    let first = s.snapshot_now().await.unwrap();
    put(&s, "bvf", "b", b"2").await;
    let second = s.snapshot_now().await.unwrap();
    put(&s, "bvf", "c", b"3").await;
    let wal_before = wal_seqs(&backend).await;
    // The upload reads back corrupt.
    faulty.corrupt_snap_reads.store(1, Ordering::SeqCst);
    let r = tokio::time::timeout(Duration::from_secs(10), s.snapshot_now()).await.expect("snapshot_now hung on a failed snapshot");
    assert!(r.is_err(), "{r:?}");
    assert_eq!(faulty.corrupt_snap_reads.load(Ordering::SeqCst), 0);
    // Deleted, never counted, and nothing more pruned.
    assert_eq!(snap_seqs(s3_store::backend::list_all(&*backend, "s3/meta/snap/").await.unwrap()), [first, second]);
    let st = s.stats().unwrap().log;
    assert_eq!((st.pruned, st.last_snapshot_seq), (first, second));
    let wal_after = wal_seqs(&backend).await;
    assert!(wal_before.iter().all(|q| wal_after.contains(q)), "no log entry deleted");
    let s2 = open(&backend, &clock).await;
    assert_eq!(s2.stats().unwrap().log.last_snapshot_seq, second);
    assert_eq!(keys(&s2, "bvf"), ["a", "b", "c"]);
    put(&s2, "bvf", "d", b"4").await;
    assert!(s2.snapshot_now().await.unwrap() > second, "the next one works");
}

#[tokio::test]
async fn without_a_usable_snapshot_a_pruned_log_refuses_to_start() {
    let (_faulty, backend, clock, s) = faulty_env("allbad").await;
    s.create_bucket("alb", BucketOptions::default()).await.unwrap();
    put(&s, "alb", "a", b"1").await;
    let first = s.snapshot_now().await.unwrap();
    put(&s, "alb", "b", b"2").await;
    let second = s.snapshot_now().await.unwrap();
    put(&s, "alb", "c", b"3").await;
    assert_eq!(s.stats().unwrap().log.pruned, first);
    for seq in [first, second] {
        backend.put_small(&format!("s3/meta/snap/{seq:020}.db.zst"), Bytes::from_static(b"rotten")).await.unwrap();
    }
    // Replaying what's left of the log from nothing would silently lose data.
    refuses(Store::open(config(&clock), backend.clone()).await, "pruned");
}

// ---- Multipart races ----

#[tokio::test]
async fn an_abort_during_a_slow_complete_never_acknowledges() {
    let (faulty, backend, clock, s) = faulty_env("abortcomplete").await;
    s.create_bucket("mpu", BucketOptions::default()).await.unwrap();
    let up = s.create_upload("mpu", "obj", ObjectHeaders::default(), None, None).await.unwrap();
    let p1 = part(&s, &up, 1, b"one").await;
    // The backend assembles the object; the abort's metadata commit lands before the Complete's.
    faulty.complete_delay_ms.store(300, Ordering::SeqCst);
    faulty.abort_delay_ms.store(600, Ordering::SeqCst);
    let (list, none) = ([(1, p1.etag.clone())], WriteCondition::none());
    let (r, aborted) = tokio::join!(s.complete_upload("mpu", &up.id, &list, None, &none), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        s.abort_bucket_upload("mpu", &up.id).await
    });
    aborted.unwrap();
    assert!(matches!(r, Err(StoreError::NoSuchUpload(_))), "{r:?}");
    assert!(read(&s, "mpu", "obj").await.is_none());
    assert_consistent(&s, &backend).await;
    collect_everything(&s, &clock).await;
    assert!(!blobs(&backend).await.contains(&blob_key(&up.blob_id)), "the assembled blob is collected");
}

#[tokio::test]
async fn a_complete_losing_its_condition_after_assembly_drops_the_upload_and_blob() {
    let (faulty, backend, _clock, s) = faulty_env("lostcond").await;
    s.create_bucket("mpu", BucketOptions::default()).await.unwrap();
    let up = s.create_upload("mpu", "obj", ObjectHeaders::default(), None, None).await.unwrap();
    let p1 = part(&s, &up, 1, b"one").await;
    faulty.complete_delay_ms.store(300, Ordering::SeqCst);
    let only_new = WriteCondition { if_match: None, if_none_match: Some("*".into()) };
    let list = [(1, p1.etag.clone())];
    let (r, ()) = tokio::join!(s.complete_upload("mpu", &up.id, &list, None, &only_new), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        put(&s, "mpu", "obj", b"winner").await;
    });
    assert!(matches!(r, Err(StoreError::PreconditionFailed)), "{r:?}");
    assert!(matches!(s.upload("mpu", &up.id), Err(StoreError::NoSuchUpload(_))), "assembled, so it can't be retried");
    assert!(!blobs(&backend).await.contains(&blob_key(&up.blob_id)), "never acknowledged: deleted at once");
    assert_eq!(read(&s, "mpu", "obj").await.unwrap(), b"winner");
    assert_consistent(&s, &backend).await;
}
