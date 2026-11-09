//! Persistence: commit log shipping and durability modes, manifests and history, crash
//! recovery, branches, point-in-time restore, garbage collection.

use super::*;
use crate::cache::CacheConfig;
use crate::db::{Database, DbConfig};
use crate::log::{Durability, LogConfig};
use crate::manifest;
use crate::store::{Store, StoreConfig};
use std::time::Duration;

const T1: TableId = TableId(0x1111);
const T2: TableId = TableId(0x2222);
const NOW: i64 = 2_000_000;

async fn store(objects: &crate::Objects, node: &str, clock: &Arc<TestClock>) -> Arc<Store> {
    Store::start(
        objects.clone(),
        StoreConfig {
            node: node.into(),
            cache: CacheConfig { memory_bytes: 4 << 20, ..Default::default() },
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

async fn put(db: &Database, t: TableId, k: u32, c: u32, v: &str, ts: i64) {
    db.get_table(t).unwrap().apply(PartitionUpdate::new(key(k)).with_row(row(c, ts, v))).await.unwrap();
}

async fn get(db: &Database, t: TableId, k: u32) -> Vec<(u32, String)> {
    let table = db.get_table(t).unwrap();
    match read(&table, key(k), NOW).await {
        Some(p) => p.rows.iter().map(|r| (u32::from_be_bytes(r.clustering.get(0).unwrap()[..].try_into().unwrap()), value(r, 1).unwrap_or_default())).collect(),
        None => Vec::new(),
    }
}

async fn attach(db: &Database) {
    db.table(T1, TableOptions::default()).await.unwrap();
    db.table(T2, TableOptions::default()).await.unwrap();
}

#[tokio::test]
async fn write_flush_unload_reopen() {
    let objects = crate::test_objects("persist-reopen");
    let clock = TestClock::new(1_000_000);
    let s = store(&objects, "n1", &clock).await;
    let db = s.open("d1", DbConfig::default()).await.unwrap();
    attach(&db).await;
    put(&db, T1, 1, 1, "a", 1).await;
    put(&db, T2, 1, 1, "other", 1).await;
    db.get_table(T1).unwrap().flush().await.unwrap();
    put(&db, T1, 1, 2, "b", 1).await;
    assert!(s.unload("d1").await.unwrap());
    assert!(s.get("d1").is_none());
    let m = manifest::read_manifest(&objects, "d1").await.unwrap().unwrap();
    assert!(m.node.is_none(), "clean unload");
    assert_eq!(m.tables[&T1].sstables.len(), 2);
    let db = s.open("d1", DbConfig::default()).await.unwrap();
    attach(&db).await;
    assert_eq!(get(&db, T1, 1).await, vec![(1, "a".into()), (2, "b".into())]);
    assert_eq!(get(&db, T2, 1).await, vec![(1, "other".into())]);
    // Nothing to replay: everything was flushed at unload.
    assert!(db.get_table(T1).unwrap().memtables_empty());
}

#[tokio::test]
async fn crash_recovery_replays_shipped_frames() {
    let objects = crate::test_objects("persist-crash");
    let clock = TestClock::new(1_000_000);
    let s = store(&objects, "n1", &clock).await;
    let db = s.open("d1", DbConfig::default()).await.unwrap();
    attach(&db).await;
    put(&db, T1, 1, 1, "flushed", 1).await;
    db.get_table(T1).unwrap().flush().await.unwrap();
    put(&db, T1, 1, 2, "shipped", 1).await;
    put(&db, T2, 7, 1, "shipped-too", 1).await;
    s.shipper.flush().await.unwrap();
    // Not yet shipped when the node dies (async durability: acknowledged, then lost).
    put(&db, T1, 1, 3, "lost", 1).await;
    drop(db);
    drop(s);
    // The same node restarts: the manifest doesn't list the last segment, the scan finds it.
    let s = store(&objects, "n1", &clock).await;
    let db = s.open("d1", DbConfig::default()).await.unwrap();
    attach(&db).await;
    assert_eq!(get(&db, T1, 1).await, vec![(1, "flushed".into()), (2, "shipped".into())]);
    assert_eq!(get(&db, T2, 7).await, vec![(1, "shipped-too".into())]);
    // New writes continue after the replayed positions.
    put(&db, T1, 1, 4, "after", 1).await;
    s.shipper.flush().await.unwrap();
    drop(db);
    drop(s);
    // Another node takes over after a second crash.
    let s2 = store(&objects, "n2", &clock).await;
    let db = s2.open("d1", DbConfig::default()).await.unwrap();
    attach(&db).await;
    assert_eq!(get(&db, T1, 1).await, vec![(1, "flushed".into()), (2, "shipped".into()), (4, "after".into())]);
    s2.unload("d1").await.unwrap();
    let db = s2.open("d1", DbConfig::default()).await.unwrap();
    attach(&db).await;
    assert_eq!(get(&db, T1, 1).await.len(), 3);
}

#[tokio::test]
async fn sync_durability_waits_for_the_segment() {
    let objects = crate::test_objects("persist-sync");
    let clock = TestClock::new(1_000_000);
    let s = store(&objects, "n1", &clock).await;
    let db = s.open("d1", DbConfig { durability: Durability::Sync, ..Default::default() }).await.unwrap();
    attach(&db).await;
    // The ship interval is an hour: a sync write kicks the shipper and returns once stored.
    tokio::time::timeout(Duration::from_secs(10), put(&db, T1, 1, 1, "durable", 1)).await.expect("sync write completes");
    assert!(!objects.list("nosql/log/n1/").await.unwrap().is_empty());
    drop(db);
    drop(s);
    let s = store(&objects, "n1", &clock).await;
    let db = s.open("d1", DbConfig::default()).await.unwrap();
    attach(&db).await;
    assert_eq!(get(&db, T1, 1).await, vec![(1, "durable".into())]);
}

#[tokio::test]
async fn shipper_retries_until_stored() {
    let objects = crate::test_objects("persist-retry");
    let clock = TestClock::new(1_000_000);
    let s = store(&objects, "n1", &clock).await;
    let db = s.open("d1", DbConfig { durability: Durability::Sync, ..Default::default() }).await.unwrap();
    attach(&db).await;
    s.shipper.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    let db2 = db.clone();
    let w = tokio::spawn(async move { put(&db2, T1, 1, 1, "x", 1).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!w.is_finished(), "a sync write isn't acknowledged before its segment is stored");
    s.shipper.fail.store(false, std::sync::atomic::Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(10), w).await.unwrap().unwrap();
}

#[tokio::test]
async fn frames_of_unattached_tables_survive() {
    let objects = crate::test_objects("persist-pending");
    let clock = TestClock::new(1_000_000);
    let s = store(&objects, "n1", &clock).await;
    let db = s.open("d1", DbConfig::default()).await.unwrap();
    attach(&db).await;
    put(&db, T1, 1, 1, "one", 1).await;
    put(&db, T2, 1, 1, "two", 1).await;
    s.shipper.flush().await.unwrap();
    drop(db);
    drop(s);
    // Reopen, attach only T1, unload: T2's frames must stay replayable.
    let s = store(&objects, "n1", &clock).await;
    let db = s.open("d1", DbConfig::default()).await.unwrap();
    db.table(T1, TableOptions::default()).await.unwrap();
    s.unload("d1").await.unwrap();
    let db = s.open("d1", DbConfig::default()).await.unwrap();
    attach(&db).await;
    assert_eq!(get(&db, T1, 1).await, vec![(1, "one".into())]);
    assert_eq!(get(&db, T2, 1).await, vec![(1, "two".into())]);
}

#[tokio::test]
async fn batches_truncate_and_drop() {
    let objects = crate::test_objects("persist-ddl");
    let clock = TestClock::new(1_000_000);
    let s = store(&objects, "n1", &clock).await;
    let db = s.open("d1", DbConfig::default()).await.unwrap();
    attach(&db).await;
    db.apply_batch(vec![(T1, PartitionUpdate::new(key(1)).with_row(row(1, 1, "b1"))), (T2, PartitionUpdate::new(key(1)).with_row(row(1, 1, "b2")))]).await.unwrap();
    assert_eq!(get(&db, T1, 1).await.len(), 1);
    assert_eq!(get(&db, T2, 1).await.len(), 1);
    db.flush().await.unwrap();
    put(&db, T1, 2, 1, "unflushed", 1).await;
    s.shipper.flush().await.unwrap();
    db.truncate(T1).await.unwrap();
    assert!(get(&db, T1, 1).await.is_empty());
    assert!(get(&db, T1, 2).await.is_empty());
    put(&db, T1, 3, 1, "after-truncate", 1).await;
    s.shipper.flush().await.unwrap();
    db.drop_table(T2).await.unwrap();
    assert!(db.table(T2, TableOptions::default()).await.is_err());
    drop(db);
    drop(s);
    // After a crash, truncated frames don't come back, later ones do; the drop sticks.
    let s = store(&objects, "n1", &clock).await;
    let db = s.open("d1", DbConfig::default()).await.unwrap();
    db.table(T1, TableOptions::default()).await.unwrap();
    assert!(get(&db, T1, 1).await.is_empty());
    assert!(get(&db, T1, 2).await.is_empty());
    assert_eq!(get(&db, T1, 3).await, vec![(1, "after-truncate".into())]);
    assert!(db.table(T2, TableOptions::default()).await.is_err());
    let m = db.manifest();
    assert!(m.tables[&T1].truncated_ms.is_some());
    assert!(m.dropped.contains(&T2));
}

#[tokio::test]
async fn branches_are_zero_copy_and_isolated() {
    let objects = crate::test_objects("persist-branch");
    let clock = TestClock::new(1_000_000);
    let s = store(&objects, "n1", &clock).await;
    let db = s.open("parent", DbConfig::default()).await.unwrap();
    attach(&db).await;
    put(&db, T1, 1, 1, "in-sstable", 1).await;
    db.flush().await.unwrap();
    put(&db, T1, 1, 2, "in-log", 1).await;
    put(&db, T2, 5, 5, "t2", 1).await;
    let before: Vec<(String, u64)> = objects.list("nosql/db/").await.unwrap();
    s.branch("parent", "child", None).await.unwrap();
    let after: Vec<(String, u64)> = objects.list("nosql/db/").await.unwrap();
    let new: Vec<&(String, u64)> = after.iter().filter(|o| !before.contains(o)).collect();
    assert!(new.iter().all(|(k, _)| k.ends_with(".json")), "only manifests written: {new:?}");
    assert!(new.iter().any(|(k, _)| k == "nosql/db/child/manifest.json"));
    let child_m = manifest::read_manifest(&objects, "child").await.unwrap().unwrap();
    assert!(child_m.tables[&T1].sstables[0].key.starts_with("nosql/db/parent/sst/"), "references the parent's SSTables");
    let child = s.open("child", DbConfig::default()).await.unwrap();
    attach(&child).await;
    assert_eq!(get(&child, T1, 1).await, vec![(1, "in-sstable".into()), (2, "in-log".into())]);
    assert_eq!(get(&child, T2, 5).await, vec![(5, "t2".into())]);
    // Isolation both ways.
    put(&child, T1, 1, 3, "child-only", 1).await;
    put(&db, T1, 1, 4, "parent-only", 1).await;
    assert_eq!(get(&child, T1, 1).await.len(), 3);
    assert!(!get(&child, T1, 1).await.iter().any(|(_, v)| v == "parent-only"));
    assert!(!get(&db, T1, 1).await.iter().any(|(_, v)| v == "child-only"));
    // The child's own flushes go under its own prefix; base frames are done with after it.
    child.flush().await.unwrap();
    let m = child.manifest();
    assert!(m.tables[&T1].base_done);
    assert!(m.tables[&T1].sstables.iter().any(|s| s.key.starts_with("nosql/db/child/sst/")));
    // A branch of the branch, after the child unloads.
    s.unload("child").await.unwrap();
    s.branch("child", "grandchild", None).await.unwrap();
    let g = s.open("grandchild", DbConfig::default()).await.unwrap();
    attach(&g).await;
    assert_eq!(get(&g, T1, 1).await, vec![(1, "in-sstable".into()), (2, "in-log".into()), (3, "child-only".into())]);
    assert_eq!(get(&g, T2, 5).await.len(), 1);
    assert!(s.branch("parent", "child", None).await.is_err(), "existing id");
}

#[tokio::test]
async fn point_in_time_branch_and_restore() {
    let objects = crate::test_objects("persist-pitr");
    let clock = TestClock::new(1_000_000);
    let s = store(&objects, "n1", &clock).await;
    let db = s.open("d1", DbConfig::default()).await.unwrap();
    attach(&db).await;
    clock.set_s(1_000);
    put(&db, T1, 1, 1, "t1000", 1).await;
    db.flush().await.unwrap();
    clock.set_s(2_000);
    put(&db, T1, 1, 2, "t2000", 2).await;
    s.shipper.flush().await.unwrap();
    clock.set_s(3_000);
    put(&db, T1, 1, 3, "t3000", 3).await;
    db.flush().await.unwrap();
    clock.set_s(4_000);
    put(&db, T1, 1, 1, "t4000-overwrite", 4).await;
    s.shipper.flush().await.unwrap();
    clock.set_s(5_000);
    // As a new database, at 2500 s: an old SSTable version plus frames up to then.
    s.restore_at("d1", 2_500_000, Some("d1-at-2500")).await.unwrap();
    let r = s.open("d1-at-2500", DbConfig::default()).await.unwrap();
    attach(&r).await;
    assert_eq!(get(&r, T1, 1).await, vec![(1, "t1000".into()), (2, "t2000".into())]);
    // In place, at 3500 s; the replaced state is archived.
    let archive = s.restore_at("d1", 3_500_000, None).await.unwrap().unwrap();
    assert!(s.get("d1").is_none(), "unloaded by the restore");
    let db = s.open("d1", DbConfig::default()).await.unwrap();
    attach(&db).await;
    assert_eq!(get(&db, T1, 1).await, vec![(1, "t1000".into()), (2, "t2000".into()), (3, "t3000".into())]);
    // The restored database keeps writing; a crash replays only the new timeline.
    clock.set_s(6_000);
    put(&db, T1, 1, 9, "new-timeline", 9).await;
    s.shipper.flush().await.unwrap();
    drop(db);
    drop(s);
    let s = store(&objects, "n1", &clock).await;
    let db = s.open("d1", DbConfig::default()).await.unwrap();
    attach(&db).await;
    assert_eq!(get(&db, T1, 1).await.iter().map(|(c, _)| *c).collect::<Vec<_>>(), vec![1, 2, 3, 9]);
    assert_eq!(get(&db, T1, 1).await[0].1, "t1000");
    // The archive still has the replaced state.
    s.branch(&archive, "undo", None).await.unwrap();
    let u = s.open("undo", DbConfig::default()).await.unwrap();
    attach(&u).await;
    assert_eq!(get(&u, T1, 1).await, vec![(1, "t4000-overwrite".into()), (2, "t2000".into()), (3, "t3000".into())]);
    // Too old.
    assert!(s.branch("d1", "ancient", Some(10)).await.is_err());
}

#[tokio::test]
async fn garbage_collection_respects_references() {
    let objects = crate::test_objects("persist-gc");
    let clock = TestClock::new(1_000_000);
    let s = store(&objects, "n1", &clock).await;
    let db = s.open("p", DbConfig { history_retention_ms: 0, ..Default::default() }).await.unwrap();
    db.table(T1, TableOptions::default()).await.unwrap();
    for i in 0..4 {
        put(&db, T1, i, 1, "x", 1).await;
        db.flush().await.unwrap();
    }
    s.shipper.flush().await.unwrap();
    s.branch("p", "b", None).await.unwrap();
    let branch_ssts: Vec<String> = manifest::read_manifest(&objects, "b").await.unwrap().unwrap().tables[&T1].sstables.iter().map(|s| s.key.clone()).collect();
    assert_eq!(branch_ssts.len(), 4);
    // The parent compacts: its current manifest no longer references the four inputs.
    db.get_table(T1).unwrap().compact_major().await.unwrap();
    assert_eq!(db.get_table(T1).unwrap().state().sstables.len(), 1);
    clock.set_s(2_000);
    db.expire_history().await.unwrap();
    db.write_manifest(false).await.unwrap();
    // An orphan: uploaded, never referenced.
    objects.put("nosql/db/p/sst/00000000-0000-0000-0000-000000001111/orphan.data", Bytes::from_static(b"x")).await.unwrap();
    let st = s.collect_garbage(Duration::ZERO).await.unwrap();
    assert!(st.manifests >= 2);
    assert_eq!(st.deleted, 1, "only the orphan goes: {st:?}");
    assert!(objects.get("nosql/db/p/sst/00000000-0000-0000-0000-000000001111/orphan.data").await.unwrap().is_none());
    for k in &branch_ssts {
        assert!(objects.exists(&format!("{k}.data")).await.unwrap(), "referenced by the branch");
    }
    // The branch still reads.
    let b = s.open("b", DbConfig::default()).await.unwrap();
    b.table(T1, TableOptions::default()).await.unwrap();
    assert_eq!(get(&b, T1, 3).await.len(), 1);
    s.unload("b").await.unwrap();
    // Delete the branch: what only it referenced becomes collectable; the parent still reads.
    s.delete_database("b").await.unwrap();
    s.collect_garbage(Duration::ZERO).await.unwrap();
    s.collect_garbage(Duration::ZERO).await.unwrap();
    for k in &branch_ssts {
        assert!(!objects.exists(&format!("{k}.data")).await.unwrap(), "collected after the branch went");
    }
    for i in 0..4 {
        assert_eq!(get(&db, T1, i).await.len(), 1);
    }
    s.unload("p").await.unwrap();
    let db = s.open("p", DbConfig::default()).await.unwrap();
    db.table(T1, TableOptions::default()).await.unwrap();
    for i in 0..4 {
        assert_eq!(get(&db, T1, i).await.len(), 1);
    }
    // min_age protects young unreferenced objects.
    objects.put("nosql/db/p/sst/00000000-0000-0000-0000-000000001111/young.data", Bytes::from_static(b"x")).await.unwrap();
    s.collect_garbage(Duration::from_secs(3600)).await.unwrap();
    s.collect_garbage(Duration::from_secs(3600)).await.unwrap();
    assert!(objects.exists("nosql/db/p/sst/00000000-0000-0000-0000-000000001111/young.data").await.unwrap());
}

#[tokio::test]
async fn history_versions_expire() {
    let objects = crate::test_objects("persist-history");
    let clock = TestClock::new(1_000_000);
    let s = store(&objects, "n1", &clock).await;
    let db = s.open("d", DbConfig { history_retention_ms: 10_000, ..Default::default() }).await.unwrap();
    db.table(T1, TableOptions::default()).await.unwrap();
    for i in 0..5 {
        clock.set_s(1_000 + i * 100);
        put(&db, T1, i as u32, 1, "x", 1).await;
        db.flush().await.unwrap();
    }
    let n = manifest::list_versions(&objects, "d").await.unwrap().len();
    assert!(n >= 5);
    clock.set_s(1_000 + 4 * 100 + 5);
    let deleted = db.expire_history().await.unwrap();
    // Kept: the newest version before the horizon and the one after it.
    assert_eq!(deleted, n - 2);
    assert_eq!(manifest::list_versions(&objects, "d").await.unwrap().len(), 2);
    assert!(db.manifest().history_floor > 0);
}

/// Random writes, flushes, shipping, unloads, crashes and compactions against two tables,
/// checked after every reopen against the model of what was durable.
#[tokio::test]
async fn random_crashes_lose_only_unshipped_writes() {
    use super::model::{Model, random_update, view_of};
    use crate::reconcile::Reconcile;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};
    for seed in 0..4u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        let objects = crate::test_objects("persist-random");
        let clock = TestClock::new(1_000_000);
        let mut s = store(&objects, "n1", &clock).await;
        let mut db = s.open("d", DbConfig::default()).await.unwrap();
        attach(&db).await;
        let mut models = [Model::default(), Model::default()];
        let mut durable = [0usize, 0usize];
        let mut ts = 100;
        for op in 0..300 {
            match rng.gen_range(0..30) {
                0 => {
                    db.flush().await.unwrap();
                    durable = [models[0].updates.len(), models[1].updates.len()];
                }
                1 | 2 => {
                    s.shipper.flush().await.unwrap();
                    durable = [models[0].updates.len(), models[1].updates.len()];
                }
                3 => {
                    // Crash: whatever wasn't shipped is lost.
                    drop(db);
                    drop(s);
                    for i in 0..2 {
                        models[i].updates.truncate(durable[i]);
                    }
                    s = store(&objects, "n1", &clock).await;
                    db = s.open("d", DbConfig::default()).await.unwrap();
                    attach(&db).await;
                }
                4 => {
                    s.unload("d").await.unwrap();
                    durable = [models[0].updates.len(), models[1].updates.len()];
                    db = s.open("d", DbConfig::default()).await.unwrap();
                    attach(&db).await;
                }
                5 => {
                    let t = if rng.gen_bool(0.5) { T1 } else { T2 };
                    db.get_table(t).unwrap().compact_major().await.unwrap();
                }
                _ => {
                    ts += 1;
                    let u = random_update(&mut rng, ts, NOW);
                    let i = rng.gen_range(0..2);
                    models[i].updates.push(u.clone());
                    db.get_table([T1, T2][i]).unwrap().apply(u).await.unwrap();
                }
            }
            if op % 50 == 49 || op == 299 {
                for (i, t) in [T1, T2].into_iter().enumerate() {
                    let got = view_of(scan_all(&db.get_table(t).unwrap(), NOW).await);
                    assert_eq!(got, models[i].eval(NOW, Reconcile::Scylla), "seed {seed} op {op} table {i}");
                }
            }
        }
    }
}

#[tokio::test]
async fn snapshots_keep_objects_and_corrupt_manifests_fail() {
    let objects = crate::test_objects("persist-snap");
    let clock = TestClock::new(1_000_000);
    let s = store(&objects, "n1", &clock).await;
    let db = s.open("d", DbConfig { history_retention_ms: 0, ..Default::default() }).await.unwrap();
    db.table(T1, TableOptions::default()).await.unwrap();
    for i in 0..2 {
        put(&db, T1, i, 1, "x", 1).await;
        db.flush().await.unwrap();
    }
    db.snapshot("before-compaction").await.unwrap();
    let snap_ssts: Vec<String> = db.manifest().tables[&T1].sstables.iter().map(|s| s.key.clone()).collect();
    db.get_table(T1).unwrap().compact_major().await.unwrap();
    clock.set_s(5_000);
    db.expire_history().await.unwrap();
    db.write_manifest(false).await.unwrap();
    s.collect_garbage(Duration::ZERO).await.unwrap();
    for k in &snap_ssts {
        assert!(objects.exists(&format!("{k}.data")).await.unwrap(), "kept by the snapshot");
    }
    db.clear_snapshot("before-compaction").await.unwrap();
    s.collect_garbage(Duration::ZERO).await.unwrap();
    for k in &snap_ssts {
        assert!(!objects.exists(&format!("{k}.data")).await.unwrap(), "collected once the snapshot is gone");
    }
    // A corrupt manifest is an error, for opening and for GC (which must not guess).
    s.unload("d").await.unwrap();
    objects.put(&manifest::manifest_key("d"), Bytes::from_static(b"{not json")).await.unwrap();
    assert!(s.open("d", DbConfig::default()).await.is_err());
    assert!(s.collect_garbage(Duration::ZERO).await.is_err());
}

#[tokio::test]
async fn manifest_versions_by_time() {
    let objects = crate::test_objects("persist-versions");
    let clock = TestClock::new(10_000);
    let s = store(&objects, "n1", &clock).await;
    let db = s.open("d", DbConfig::default()).await.unwrap();
    // A new database's first manifest is a numbered version: its empty state at creation.
    assert_eq!(manifest::list_versions(&objects, "d").await.unwrap().len(), 1);
    db.table(T1, TableOptions::default()).await.unwrap();
    for i in 1..=5 {
        clock.set_s(i * 100);
        put(&db, T1, i as u32, 1, "x", 1).await;
        db.flush().await.unwrap();
    }
    let current = db.manifest();
    let v = manifest::manifest_at(&objects, "d", &current, 350_000).await.unwrap().unwrap();
    assert_eq!(v.written_ms, 300_000);
    assert_eq!(v.tables[&T1].sstables.len(), 3);
    assert!(manifest::manifest_at(&objects, "d", &current, 5_000).await.unwrap().is_none());
    let empty = manifest::manifest_at(&objects, "d", &current, 50_000).await.unwrap().unwrap();
    assert_eq!(empty.written_ms, 10_000);
    assert!(empty.tables.values().all(|t| t.sstables.is_empty()), "{:?}", empty.tables);
    // Reopening a database that has a manifest adds no version.
    s.unload("d").await.unwrap();
    let versions = manifest::list_versions(&objects, "d").await.unwrap().len();
    s.open("d", DbConfig::default()).await.unwrap();
    assert_eq!(manifest::list_versions(&objects, "d").await.unwrap().len(), versions);
    assert_eq!(manifest::manifest_at(&objects, "d", &current, i64::MAX).await.unwrap().unwrap(), current);
}

/// Writers racing flushes and compactions on a multi-threaded runtime: every acknowledged
/// write is readable, and after a crash every shipped one is recovered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writes_flushes_and_compactions() {
    let objects = crate::test_objects("persist-concurrent");
    let clock = TestClock::new(1_000_000);
    let s = store(&objects, "n1", &clock).await;
    let db = s.open("d", DbConfig::default()).await.unwrap();
    let t = db.table(T1, TableOptions { block_size: 512, ..Default::default() }).await.unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let bg = {
        let (t, stop) = (t.clone(), stop.clone());
        tokio::spawn(async move {
            let mut n = 0;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                t.flush().await.unwrap();
                if n % 3 == 2 {
                    t.compact_major().await.unwrap();
                }
                n += 1;
                tokio::task::yield_now().await;
            }
        })
    };
    let mut writers = Vec::new();
    for w in 0..4u32 {
        let t = t.clone();
        writers.push(tokio::spawn(async move {
            for i in 0..3000u32 {
                t.apply(PartitionUpdate::new(key(i % 200)).with_row(row(w * 10_000 + i, 1, "v"))).await.unwrap();
            }
        }));
    }
    for w in writers {
        w.await.unwrap();
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    bg.await.unwrap();
    let total: usize = scan_all(&t, NOW).await.iter().map(|p| p.rows.len()).sum();
    assert_eq!(total, 12_000);
    s.shipper.flush().await.unwrap();
    drop(t);
    drop(db);
    drop(s);
    let s = store(&objects, "n1", &clock).await;
    let db = s.open("d", DbConfig::default()).await.unwrap();
    let t = db.table(T1, TableOptions::default()).await.unwrap();
    let total: usize = scan_all(&t, NOW).await.iter().map(|p| p.rows.len()).sum();
    assert_eq!(total, 12_000);
}
