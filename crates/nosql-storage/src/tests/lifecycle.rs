//! Lifecycle edge cases: the local disk cache across node restarts, unloading while a
//! compaction runs, failed object writes during DDL, flushes and unloads (fail points), and
//! garbage collection's two-phase deletion and log references.

use super::*;
use crate::cache::CacheConfig;
use crate::db::{Database, DbConfig};
use crate::failpoint;
use crate::log::LogConfig;
use crate::manifest;
use crate::store::{Store, StoreConfig};
use std::path::{Path, PathBuf};
use std::time::Duration;

const T1: TableId = TableId(0x1111);
const T2: TableId = TableId(0x2222);
const NOW: i64 = 2_000_000;

async fn store_with(objects: &crate::Objects, clock: &Arc<TestClock>, cache: CacheConfig) -> Arc<Store> {
    Store::start(
        objects.clone(),
        StoreConfig {
            node: "n1".into(),
            cache,
            log: LogConfig { flush_interval: Duration::from_secs(3600), ..Default::default() },
            memtable_budget_bytes: 64 << 20,
            maintenance_interval: Duration::from_secs(3600),
            manifest_interval: Duration::from_secs(3600),
            auto_compaction: false,
            clock: Some(clock.clock()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

async fn store(objects: &crate::Objects, clock: &Arc<TestClock>) -> Arc<Store> {
    store_with(objects, clock, CacheConfig { memory_bytes: 4 << 20, ..Default::default() }).await
}

fn small_blocks() -> TableOptions {
    TableOptions { block_size: 512, ..Default::default() }
}

async fn fill(db: &Database, t: TableId, keys: std::ops::Range<u32>, v: &str) {
    let table = db.get_table(t).unwrap();
    for k in keys {
        table.apply(PartitionUpdate::new(key(k)).with_row(row(1, 1, v))).await.unwrap();
    }
}

/// Every partition's single value, by key order of the scan.
async fn values(db: &Database, t: TableId) -> Vec<String> {
    let mut v: Vec<String> = scan_all(&db.get_table(t).unwrap(), NOW).await.iter().flat_map(|p| p.rows.iter().map(|r| value(r, 1).unwrap_or_default())).collect();
    v.sort();
    v
}

fn files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir).map(|d| d.map(|e| e.unwrap().path()).collect()).unwrap_or_default()
}

fn temp_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("roll-nosql-{name}-{}", rand::random::<u64>()))
}

#[tokio::test]
async fn disk_cache_starts_empty_after_a_restart_and_refetches_lost_files() {
    let objects = crate::test_objects("life-disk-cache");
    let clock = TestClock::new(1_000_000);
    let dir = temp_dir("disk-cache");
    // A memory tier too small for any block: every hit is a disk hit.
    let cache = CacheConfig { memory_bytes: 8, disk_dir: Some(dir.clone()), disk_bytes: 64 << 20, max_get_bytes: 4 << 10 };
    let s = store_with(&objects, &clock, cache.clone()).await;
    let db = s.open("d", DbConfig::default()).await.unwrap();
    db.table(T1, small_blocks()).await.unwrap();
    fill(&db, T1, 0..300, "cached value").await;
    db.flush().await.unwrap();
    let want = values(&db, T1).await;
    assert_eq!(want.len(), 300);
    assert!(!files(&dir).is_empty(), "blocks written to the disk tier");
    values(&db, T1).await;
    assert!(s.env.cache.metrics.disk_hits.load(std::sync::atomic::Ordering::Relaxed) > 0);
    // Files deleted underneath the cache: refetched from object storage, same answer.
    let gets = s.env.cache.metrics.gets.load(std::sync::atomic::Ordering::Relaxed);
    for f in files(&dir) {
        std::fs::remove_file(f).unwrap();
    }
    assert_eq!(values(&db, T1).await, want);
    assert!(s.env.cache.metrics.gets.load(std::sync::atomic::Ordering::Relaxed) > gets);
    // Truncated files (a crash mid-write) are corrupt blocks: dropped and refetched.
    for f in files(&dir) {
        let b = std::fs::read(&f).unwrap();
        std::fs::write(&f, &b[..b.len() / 2]).unwrap();
    }
    assert_eq!(values(&db, T1).await, want);
    assert!(s.env.cache.metrics.corrupt_disk_blocks.load(std::sync::atomic::Ordering::Relaxed) > 0);
    s.shipper.flush().await.unwrap();
    drop(db);
    drop(s);
    // A restart on the same directory: SSTable uids are per process, so stale files could
    // name another SSTable's blocks. The directory starts empty and refills.
    std::fs::write(dir.join("0-0"), b"stale").unwrap();
    let s = store_with(&objects, &clock, cache).await;
    assert!(files(&dir).is_empty(), "wiped at start: {:?}", files(&dir));
    let db = s.open("d", DbConfig::default()).await.unwrap();
    db.table(T1, small_blocks()).await.unwrap();
    assert_eq!(values(&db, T1).await, want);
    assert!(!files(&dir).is_empty());
    // Unloading forgets the database's blocks, files included.
    s.unload("d").await.unwrap();
    assert!(files(&dir).is_empty(), "{:?}", files(&dir));
    assert_eq!(s.env.cache.disk_bytes(), 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unload_during_a_compaction_keeps_a_consistent_manifest() {
    for (round, delay_ms) in [0u64, 1, 5, 20, 60].into_iter().enumerate() {
        let objects = crate::test_objects("life-unload-compaction");
        let clock = TestClock::new(1_000_000);
        let s = store(&objects, &clock).await;
        let db = s.open("d", DbConfig::default()).await.unwrap();
        db.table(T1, small_blocks()).await.unwrap();
        for i in 0..6u32 {
            fill(&db, T1, i * 400..(i + 1) * 400, &format!("v{i}")).await;
            db.flush().await.unwrap();
        }
        let t = db.get_table(T1).unwrap();
        let inputs: std::collections::BTreeSet<String> = t.state().sstables.iter().map(|s| s.key.clone()).collect();
        let compaction = tokio::spawn(async move { t.compact_major().await });
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        assert!(s.unload("d").await.unwrap());
        compaction.await.unwrap().unwrap();
        // The manifest lists either the six inputs or only outputs, all of them stored.
        let m = manifest::read_manifest(&objects, "d").await.unwrap().unwrap();
        assert!(m.node.is_none(), "clean unload");
        let ssts = &m.tables[&T1].sstables;
        let listed: std::collections::BTreeSet<String> = ssts.iter().map(|s| s.key.clone()).collect();
        assert!(listed == inputs || listed.is_disjoint(&inputs), "round {round}: {listed:?}");
        for sst in ssts {
            assert!(objects.exists(&format!("{}.data", sst.key)).await.unwrap(), "round {round}: {} is stored", sst.key);
            assert!(objects.exists(&format!("{}.index", sst.key)).await.unwrap());
        }
        // Reopened, nothing is lost or duplicated, and nothing is left to replay.
        let db = s.open("d", DbConfig::default()).await.unwrap();
        db.table(T1, small_blocks()).await.unwrap();
        let v = values(&db, T1).await;
        assert_eq!(v.len(), 2400, "round {round}");
        assert!(db.get_table(T1).unwrap().memtables_empty());
        // Garbage collection keeps what the manifest references.
        s.collect_garbage(Duration::ZERO).await.unwrap();
        s.collect_garbage(Duration::ZERO).await.unwrap();
        s.unload("d").await.unwrap();
        let db = s.open("d", DbConfig::default()).await.unwrap();
        db.table(T1, small_blocks()).await.unwrap();
        assert_eq!(values(&db, T1).await, v, "round {round}");
    }
}

#[tokio::test]
async fn failed_sstable_uploads_keep_data_and_the_database() {
    let objects = crate::test_objects("life-upload-fail");
    let clock = TestClock::new(1_000_000);
    let s = store(&objects, &clock).await;
    let db = s.open("upfail", DbConfig::default()).await.unwrap();
    db.table(T1, small_blocks()).await.unwrap();
    fill(&db, T1, 0..100, "a").await;
    db.flush().await.unwrap();
    fill(&db, T1, 100..150, "a").await;
    db.flush().await.unwrap();
    fill(&db, T1, 150..200, "b").await;
    let armed = "nosql/db/upfail/sst/";
    failpoint::arm(armed);
    // A flush that can't upload keeps the memtable readable.
    assert!(db.get_table(T1).unwrap().flush().await.is_err());
    assert_eq!(values(&db, T1).await.len(), 200);
    // A compaction that can't upload leaves its inputs in place.
    fill(&db, T1, 200..250, "c").await;
    assert!(db.get_table(T1).unwrap().compact_major().await.is_err());
    assert_eq!(db.get_table(T1).unwrap().state().sstables.len(), 2);
    // An unload that can't flush fails and leaves the database loaded and writable.
    assert!(s.unload("upfail").await.is_err());
    assert!(s.get("upfail").is_some());
    fill(&db, T1, 250..260, "d").await;
    assert_eq!(values(&db, T1).await.len(), 260);
    failpoint::disarm(armed);
    // Retried: everything lands.
    assert!(s.unload("upfail").await.unwrap());
    let m = manifest::read_manifest(&objects, "upfail").await.unwrap().unwrap();
    assert!(m.node.is_none());
    let db = s.open("upfail", DbConfig::default()).await.unwrap();
    db.table(T1, small_blocks()).await.unwrap();
    assert_eq!(values(&db, T1).await.len(), 260);
    assert!(db.get_table(T1).unwrap().memtables_empty());
}

#[tokio::test]
async fn failed_manifest_writes_are_retried_and_ddl_survives_a_crash() {
    let objects = crate::test_objects("life-manifest-fail");
    let clock = TestClock::new(1_000_000);
    let s = store(&objects, &clock).await;
    let db = s.open("mfail", DbConfig::default()).await.unwrap();
    db.table(T1, TableOptions::default()).await.unwrap();
    db.table(T2, TableOptions::default()).await.unwrap();
    fill(&db, T1, 0..50, "t1").await;
    fill(&db, T2, 0..50, "t2").await;
    db.flush().await.unwrap();
    s.shipper.flush().await.unwrap();
    let armed = manifest::manifest_key("mfail");
    failpoint::arm(&armed);
    // TRUNCATE and DROP TABLE apply in memory, but their manifest isn't stored.
    assert!(db.truncate(T1).await.is_err());
    assert!(db.drop_table(T2).await.is_err());
    assert!(values(&db, T1).await.is_empty());
    assert!(db.get_table(T2).is_none());
    assert!(db.is_dirty(), "a failed write leaves the manifest to rewrite");
    // The lazy writer fails while storage does, and stays dirty.
    assert!(s.write_manifests(false).await.is_err());
    assert!(db.is_dirty());
    failpoint::disarm(&armed);
    // Storage is back: the lazy writer stores it, then the node crashes.
    s.write_manifests(false).await.unwrap();
    assert!(!db.is_dirty());
    drop(db);
    drop(s);
    let s = store(&objects, &clock).await;
    let db = s.open("mfail", DbConfig::default()).await.unwrap();
    db.table(T1, TableOptions::default()).await.unwrap();
    assert!(values(&db, T1).await.is_empty(), "truncated data stays gone");
    assert!(db.table(T2, TableOptions::default()).await.unwrap_err().to_string().contains("dropped"));
    // A failed unload's manifest write is retried by the next one.
    fill(&db, T1, 0..10, "again").await;
    failpoint::arm(&armed);
    assert!(s.unload("mfail").await.is_err());
    assert!(s.get("mfail").is_some());
    fill(&db, T1, 10..20, "again").await;
    failpoint::disarm(&armed);
    assert!(s.unload("mfail").await.unwrap());
    let db = s.open("mfail", DbConfig::default()).await.unwrap();
    db.table(T1, TableOptions::default()).await.unwrap();
    assert_eq!(values(&db, T1).await.len(), 20);
}

#[tokio::test]
async fn garbage_collection_ages_candidates_and_keeps_unlisted_log_segments() {
    let objects = crate::test_objects("life-gc");
    let clock = TestClock::new(1_000_000);
    let s = store(&objects, &clock).await;
    let db = s.open("g", DbConfig { history_retention_ms: 0, ..Default::default() }).await.unwrap();
    db.table(T1, TableOptions::default()).await.unwrap();
    // Two-phase deletion: found unreferenced, kept until it has stayed so for min_age.
    let orphan = "nosql/db/g/sst/00000000-0000-0000-0000-000000001111/orphan.data";
    objects.put(orphan, Bytes::from_static(b"x")).await.unwrap();
    let min_age = Duration::from_secs(60);
    let st = s.collect_garbage(min_age).await.unwrap();
    assert_eq!((st.unreferenced, st.deleted), (1, 0), "{st:?}");
    clock.set_s(1_000 + 30);
    assert_eq!(s.collect_garbage(min_age).await.unwrap().deleted, 0);
    assert!(objects.exists(orphan).await.unwrap());
    clock.set_s(1_000 + 61);
    let st = s.collect_garbage(min_age).await.unwrap();
    assert_eq!((st.deleted, st.deleted_bytes), (1, 1), "{st:?}");
    assert!(!objects.exists(orphan).await.unwrap());
    // Shipped segments the manifest doesn't list yet (written after it) are live: a crash
    // now must still replay them.
    fill(&db, T1, 0..5, "unlisted").await;
    s.shipper.flush().await.unwrap();
    let segs = objects.list("nosql/log/n1/").await.unwrap();
    assert!(!segs.is_empty());
    clock.set_s(10_000);
    s.collect_garbage(Duration::ZERO).await.unwrap();
    s.collect_garbage(Duration::ZERO).await.unwrap();
    for (k, _) in &segs {
        assert!(objects.exists(k).await.unwrap(), "{k} is still needed");
    }
    drop(db);
    drop(s);
    let s = store(&objects, &clock).await;
    let db = s.open("g", DbConfig::default()).await.unwrap();
    db.table(T1, TableOptions::default()).await.unwrap();
    assert_eq!(values(&db, T1).await.len(), 5);
    // A deleted database's manifests are all removed; expiring an unknown one is a no-op.
    s.unload("g").await.unwrap();
    assert!(s.delete_database("g").await.unwrap() >= 2);
    assert!(manifest::read_manifest(&objects, "g").await.unwrap().is_none());
    assert_eq!(crate::gc::expire_versions(&objects, "nope", i64::MAX).await.unwrap(), (0, None));
    // A corrupt pending-state file starts the aging over instead of failing.
    objects.put("nosql/gc/pending.json", Bytes::from_static(b"{oops")).await.unwrap();
    s.collect_garbage(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn bulk_ingest_swaps_atomically_and_survives_a_crash() {
    use crate::ingest::Ingested;
    let objects = crate::test_objects("life-ingest");
    let clock = TestClock::new(1_000_000);
    let s = store(&objects, &clock).await;
    let db = s.open("ing", DbConfig { history_retention_ms: 0, ..Default::default() }).await.unwrap();
    let t1 = db.table(T1, TableOptions::default()).await.unwrap();
    let t2 = db.table(T2, TableOptions::default()).await.unwrap();
    fill(&db, T1, 0..10, "old").await;
    fill(&db, T2, 0..5, "kept").await;
    db.flush().await.unwrap();
    fill(&db, T1, 10..20, "old-unflushed").await;
    // Partitions in token order; one given as an update in any order, with a duplicate row
    // (reconciled: the newer write wins), one row by row.
    let mut keys = [1u32, 100];
    keys.sort_by_key(|k| key(*k).token);
    let mut w = t1.ingest_writer(2).unwrap();
    for k in keys {
        if k == 1 {
            w.write_update(PartitionUpdate::new(key(1)).with_row(row(2, 5, "b")).with_row(row(1, 5, "a")).with_row(row(2, 9, "b-newer"))).unwrap();
        } else {
            w.begin_partition(&key(100), DeletionTime::LIVE, None).unwrap();
            w.add_row(row(1, 1, "direct")).unwrap();
            w.end_partition().unwrap();
        }
    }
    assert_eq!(w.partitions(), 2);
    let sst1 = t1.ingest_upload(w).await.unwrap().unwrap();
    // An empty writer uploads nothing.
    assert!(t1.ingest_upload(t1.ingest_writer(1).unwrap()).await.unwrap().is_none());
    let mut w = t2.ingest_writer(1).unwrap();
    w.write_update(PartitionUpdate::new(key(50)).with_row(row(1, 1, "added"))).unwrap();
    let sst2 = t2.ingest_upload(w).await.unwrap().unwrap();
    // Staged: garbage collection keeps uploads that aren't part of a table yet.
    db.stage_ingest("import-1", &[(T1, vec![sst1.clone()]), (T2, vec![sst2.clone()])]).await.unwrap();
    s.collect_garbage(Duration::ZERO).await.unwrap();
    s.collect_garbage(Duration::ZERO).await.unwrap();
    assert!(objects.exists(&format!("{}.data", sst1.key)).await.unwrap());
    // Not attached: refused, nothing changes.
    assert!(db.ingest(vec![Ingested { table: TableId(0x9999), sstables: vec![], replace: false }]).await.is_err());
    let versions = manifest::list_versions(&objects, "ing").await.unwrap().len();
    db.ingest(vec![Ingested { table: T1, sstables: vec![sst1.clone()], replace: true }, Ingested { table: T2, sstables: vec![sst2], replace: false }]).await.unwrap();
    assert_eq!(manifest::list_versions(&objects, "ing").await.unwrap().len(), versions + 1, "one version for the whole ingest");
    let t1_want = vec!["a".to_string(), "b-newer".into(), "direct".into()];
    assert_eq!(values(&db, T1).await, t1_want, "replaced: flushed and unflushed rows gone");
    assert_eq!(values(&db, T2).await.len(), 6, "appended");
    db.clear_snapshot("import-1").await.unwrap();
    s.collect_garbage(Duration::ZERO).await.unwrap();
    s.collect_garbage(Duration::ZERO).await.unwrap();
    assert!(objects.exists(&format!("{}.data", sst1.key)).await.unwrap(), "referenced by the manifest now");
    // A crash: the replaced table's earlier frames are never replayed again.
    s.shipper.flush().await.unwrap();
    drop((t1, t2, db));
    drop(s);
    let s = store(&objects, &clock).await;
    let db = s.open("ing", DbConfig::default()).await.unwrap();
    db.table(T1, TableOptions::default()).await.unwrap();
    db.table(T2, TableOptions::default()).await.unwrap();
    assert_eq!(values(&db, T1).await, t1_want);
    assert_eq!(values(&db, T2).await.len(), 6);
}

#[tokio::test]
async fn maintenance_flushes_memtables_by_flush_period() {
    let objects = crate::test_objects("life-flush-period");
    let clock = TestClock::new(1_000_000);
    let s = store(&objects, &clock).await;
    let db = s.open("fp", DbConfig::default()).await.unwrap();
    db.table(T1, TableOptions { memtable_flush_period_ms: 1, ..Default::default() }).await.unwrap();
    db.table(T2, TableOptions::default()).await.unwrap();
    fill(&db, T1, 0..3, "x").await;
    fill(&db, T2, 0..3, "y").await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    s.maintain().await.unwrap();
    assert!(db.get_table(T1).unwrap().memtables_empty(), "the period elapsed");
    assert_eq!(db.get_table(T1).unwrap().state().sstables.len(), 1);
    assert!(!db.get_table(T2).unwrap().memtables_empty(), "no period: left alone");
    // Nothing new: no empty SSTables.
    tokio::time::sleep(Duration::from_millis(10)).await;
    s.maintain().await.unwrap();
    assert_eq!(db.get_table(T1).unwrap().state().sstables.len(), 1);
}
