//! Metadata durability: everything acknowledged survives losing the node (its local disk
//! included), a second writer fences the first, and snapshots bound the log.

mod common;

use bytes::Bytes;
use common::*;
use s3_store::*;
use std::sync::Arc;

/// Everything observable about a store, for before/after comparisons.
async fn state(s: &Store) -> Vec<String> {
    let mut out = Vec::new();
    for b in s.buckets().unwrap() {
        out.push(format!("bucket {} public={} recover={} parent={:?} objects={} bytes={} trash={} cors={:?}", b.name, b.public, b.recover_days, b.parent.map(|p| p.name), b.objects, b.bytes, b.trash_bytes, b.cors));
        for k in keys(s, &b.name) {
            let m = s.get_object(&b.name, &k).unwrap();
            let data = read(s, &b.name, &k).await.unwrap();
            out.push(format!("  {k} {} {} {:?} {:?}", m.etag, m.size, m.headers, data));
        }
        for t in s.list_trash(&b.name, "", None, 1000).unwrap().0 {
            out.push(format!("  trash {} {} {}", t.key, t.etag, t.deleted_at));
        }
        for u in s.list_uploads(&b.name, &UploadsQuery { max_uploads: 1000, ..Default::default() }).unwrap().uploads {
            out.push(format!("  upload {} {}", u.key, u.id));
        }
    }
    for k in s.keys().unwrap() {
        out.push(format!("key {} {} {:?}", k.id, k.name, k.permission));
    }
    out
}

async fn workload(s: &Store) {
    s.create_bucket("alpha", BucketOptions::default()).await.unwrap();
    s.create_bucket("beta", BucketOptions { public: Some(true), recover_days: Some(0) }).await.unwrap();
    for i in 0..20 {
        put(s, "alpha", &format!("dir/{i:02}"), format!("value {i}").as_bytes()).await;
    }
    put(s, "alpha", "dir/03", b"overwritten").await;
    s.delete_object("alpha", "dir/04", None).await.unwrap();
    s.copy_object("alpha", "dir/05", "beta", "copied", &CopyOptions::default()).await.unwrap();
    s.branch_bucket("alpha", "alpha-preview").await.unwrap();
    put(s, "alpha-preview", "dir/06", b"branch").await;
    put(s, "alpha", "dir/07", b"parent").await;
    let up = s.create_upload("beta", "multi", ObjectHeaders::default(), None, None).await.unwrap();
    let one = vec![b'1'; 5 << 20];
    let d = s.put_part_data(&up, 1, bytes_body(Bytes::from(one.clone())), one.len() as u64).await.unwrap();
    s.commit_part(&up, 1, &d, &md5_hex(&one), None).await.unwrap();
    s.create_upload("beta", "open", ObjectHeaders::default(), None, None).await.unwrap();
    let d2 = s.put_part_data(&up, 2, bytes_body(Bytes::from_static(b"two")), 3).await.unwrap();
    s.commit_part(&up, 2, &d2, &md5_hex(b"two"), None).await.unwrap();
    s.complete_upload("beta", &up.id, &[(1, md5_hex(&one)), (2, md5_hex(b"two"))], None, &WriteCondition::none()).await.unwrap();
    let cors = vec![CorsRule { allowed_origins: vec!["https://x.dev".into()], allowed_methods: vec!["GET".into(), "PUT".into()], ..Default::default() }];
    s.update_bucket("beta", BucketUpdate { cors: Some(cors), ..Default::default() }).await.unwrap();
    s.create_key("deploy", Permission::Write, None).await.unwrap();
    s.put_tags("alpha", "dir/01", [("a".to_string(), "b".to_string())].into()).await.unwrap();
}

#[tokio::test]
async fn recovers_everything_from_object_storage_alone() {
    let (backend, clock, s) = env("recover").await;
    workload(&s).await;
    let before = state(&s).await;
    // The node dies (no clean close); a new one starts with an empty local disk.
    let s2 = open(&backend, &clock).await;
    assert_eq!(state(&s2).await, before);
    assert_consistent(&s2, &backend).await;
    // And it keeps working.
    put(&s2, "alpha", "after", b"x").await;
    let s3 = open(&backend, &clock).await;
    assert_eq!(read(&s3, "alpha", "after").await.unwrap(), b"x");
    drop(s);
}

#[tokio::test]
async fn a_new_writer_fences_the_old_one() {
    let (backend, clock, old) = env("fence").await;
    old.create_bucket("fence", BucketOptions::default()).await.unwrap();
    put(&old, "fence", "before", b"1").await;
    let new = open(&backend, &clock).await;
    // The old node's next write can't commit; it stops serving entirely.
    let r = put_with(&old, "fence", "lost", b"2", &WriteCondition::none()).await;
    assert!(matches!(r, Err(StoreError::Unavailable(_))), "{r:?}");
    assert!(matches!(old.get_object("fence", "before"), Err(StoreError::Unavailable(_))));
    assert!(matches!(old.create_bucket("other", BucketOptions::default()).await, Err(StoreError::Unavailable(_))));
    // The new one has everything acknowledged, and nothing that wasn't.
    assert_eq!(read(&new, "fence", "before").await.unwrap(), b"1");
    assert!(read(&new, "fence", "lost").await.is_none());
    put(&new, "fence", "next", b"3").await;
    let third = open(&backend, &clock).await;
    assert_eq!(keys(&third, "fence"), ["before", "next"]);
}

#[tokio::test]
async fn snapshots_bound_the_log() {
    let backend = test_backend("snap").await;
    let clock = TestClock::default();
    let mut cfg = config(&clock);
    cfg.snapshot_commits = 1_000_000;
    let s = Store::open(cfg, backend.clone()).await.unwrap();
    workload(&s).await;
    let first = s.snapshot_now().await.unwrap();
    put(&s, "alpha", "between", b"x").await;
    let second = s.snapshot_now().await.unwrap();
    assert!(second > first);
    put(&s, "alpha", "after-snapshots", b"y").await;
    // Entries up to the older snapshot are deleted; the newer one's are kept for the fallback.
    let wal = list_all(&backend, "s3/meta/wal/").await;
    assert!(wal.iter().all(|k| seq(k) > first), "{wal:?}");
    let snaps = list_all(&backend, "s3/meta/snap/").await;
    assert_eq!(snaps.len(), 2);
    let before = state(&s).await;
    let s2 = open(&backend, &clock).await;
    assert_eq!(state(&s2).await, before);
    assert_eq!(s.stats().unwrap().log.pruned, first);

    // A third snapshot drops the first.
    put(&s2, "alpha", "x", b"x").await;
    let third = s2.snapshot_now().await.unwrap();
    let snaps = list_all(&backend, "s3/meta/snap/").await;
    assert_eq!(snaps.iter().map(|k| seq(k)).collect::<Vec<_>>().len(), 2);
    assert!(snaps.iter().all(|k| seq(k) > first));
    assert!(third > second);
}

#[tokio::test]
async fn a_corrupt_snapshot_falls_back_to_the_previous_one() {
    let (backend, clock, s) = env("badsnap").await;
    workload(&s).await;
    s.snapshot_now().await.unwrap();
    put(&s, "alpha", "between", b"x").await;
    let newest = s.snapshot_now().await.unwrap();
    put(&s, "alpha", "after", b"y").await;
    let before = state(&s).await;
    backend.put_small(&format!("s3/meta/snap/{newest:020}.db.zst"), Bytes::from_static(b"garbage")).await.unwrap();
    let s2 = open(&backend, &clock).await;
    assert_eq!(state(&s2).await, before);
}

#[tokio::test]
async fn a_corrupt_log_entry_stops_recovery() {
    let (backend, clock, s) = env("badlog").await;
    s.create_bucket("bad", BucketOptions::default()).await.unwrap();
    put(&s, "bad", "k", b"v").await;
    let wal = list_all(&backend, "s3/meta/wal/").await;
    let victim = &wal[wal.len() / 2];
    let mut bytes = backend.get_bytes(victim).await.unwrap().unwrap().to_vec();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    backend.put_small(victim, Bytes::from(bytes)).await.unwrap();
    let r = Store::open(config(&clock), backend.clone()).await;
    let err = r.err().expect("must refuse to start").to_string();
    assert!(err.contains("corrupt"), "{err}");
}

#[tokio::test]
async fn closed_stores_refuse_work_and_reopen_cleanly() {
    let (backend, clock, s) = env("close").await;
    s.create_bucket("cls", BucketOptions::default()).await.unwrap();
    put(&s, "cls", "k", b"v").await;
    s.close().await;
    assert!(matches!(s.bucket("cls"), Err(StoreError::Unavailable(_))));
    let s2: Arc<Store> = open(&backend, &clock).await;
    assert_eq!(read(&s2, "cls", "k").await.unwrap(), b"v");
}

fn seq(key: &str) -> u64 {
    key.rsplit('/').next().unwrap().split('.').next().unwrap().parse().unwrap()
}

async fn list_all(backend: &Arc<dyn Backend>, prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let (page, more) = backend.list_page(prefix, after.as_deref(), 1000).await.unwrap();
        after = page.last().map(|l| l.key.clone()).or(after);
        out.extend(page.into_iter().map(|l| l.key));
        if !more {
            return out;
        }
    }
}

#[tokio::test]
async fn concurrent_writes_group_commit_and_snapshot_while_writing() {
    let backend = test_backend("busy").await;
    let clock = TestClock::default();
    let mut cfg = config(&clock);
    cfg.commit_ms = 10;
    cfg.snapshot_step_pages = 1;
    cfg.snapshot_commits = 25;
    let s = Store::open(cfg, backend.clone()).await.unwrap();
    s.create_bucket("busy", BucketOptions::default()).await.unwrap();
    // Enough metadata for a snapshot to take many steps.
    let mut h = ObjectHeaders::default();
    h.metadata.insert("pad".into(), "x".repeat(1500));
    let tasks: Vec<_> = (0..8)
        .map(|t| {
            let s = s.clone();
            let h = h.clone();
            tokio::spawn(async move {
                for i in 0..40 {
                    let key = format!("t{t}/{i:03}");
                    let b = s.bucket("busy").unwrap();
                    let blob = s.new_blob();
                    let data = key.as_bytes().to_vec();
                    let up = s.put_blob(&blob, &b.id, &key, bytes_body(Bytes::from(data.clone())), data.len() as u64, None).await.unwrap();
                    let obj = NewObject { blob_id: blob, size: up.size, crc32c: up.crc32c, etag: md5_hex(&data), headers: h.clone(), checksum: None };
                    s.commit_put("busy", &key, obj, &WriteCondition::none()).await.unwrap();
                    if i % 7 == 0 {
                        s.delete_object("busy", &key, None).await.unwrap();
                    }
                }
            })
        })
        .collect();
    let snapper = {
        let s = s.clone();
        tokio::spawn(async move {
            for _ in 0..5 {
                s.snapshot_now().await.unwrap();
            }
        })
    };
    for t in tasks {
        t.await.unwrap();
    }
    snapper.await.unwrap();
    let st = s.stats().unwrap();
    assert!(st.log.commits < 8 * 40, "writes are grouped: {} commits", st.log.commits);
    assert!(st.log.snapshots >= 2);
    let before = state(&s).await;
    assert_eq!(keys(&s, "busy").len(), 8 * (40 - 6));
    let s2 = open(&backend, &clock).await;
    assert_eq!(state(&s2).await, before);
    assert_consistent(&s2, &backend).await;
}

#[tokio::test]
async fn an_idle_deposed_node_stops_serving() {
    let backend = test_backend("idle-fence").await;
    let clock = TestClock::default();
    let mut cfg = config(&clock);
    cfg.fence_check = std::time::Duration::from_millis(50);
    let old = Store::open(cfg, backend.clone()).await.unwrap();
    old.create_bucket("idl", BucketOptions::default()).await.unwrap();
    let _new = open(&backend, &clock).await;
    for _ in 0..100 {
        if old.bucket("idl").is_err() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(matches!(old.bucket("idl"), Err(StoreError::Unavailable(_))), "stale reads stop without a write");
}

/// Every metadata row, table by table: what a rebuild must reproduce exactly (refcounts and
/// quarantine times included, or the sweeper would delete on the wrong schedule).
fn dump(s: &Store) -> Vec<String> {
    let c = meta_db(s);
    let mut out = Vec::new();
    for t in ["buckets", "objects", "trash", "blobs", "uploads", "parts", "keys", "usage", "orphans", "state"] {
        let mut st = c.prepare(&format!("SELECT * FROM {t}")).unwrap();
        let n = st.column_count();
        let mut rows: Vec<String> = st
            .query_map([], |r| Ok((0..n).map(|i| format!("{:?}", r.get::<_, rusqlite::types::Value>(i).unwrap())).collect::<Vec<_>>().join("|")))
            .unwrap()
            .map(|r| format!("{t}: {}", r.unwrap()))
            .collect();
        rows.sort();
        out.extend(rows);
    }
    out
}

#[tokio::test]
async fn recovery_rebuilds_the_metadata_exactly() {
    let backend = test_backend("exact").await;
    let clock = TestClock::default();
    let s = open(&backend, &clock).await;
    workload(&s).await;
    s.snapshot_now().await.unwrap();
    // Quarantine, trash, an orphan candidate, a scrub report, usage and key times.
    s.delete_object("beta", "copied", None).await.unwrap();
    s.delete_object("alpha", "dir/08", None).await.unwrap();
    s.restore("alpha", "dir/03", s.list_trash("alpha", "dir/03", None, 1).unwrap().0[0].deleted_at).await.unwrap();
    backend.put(&blob_key("zzorphanorphanorphanorphan"), bytes_body(Bytes::from_static(b"o")), 1, &BlobAttrs::default()).await.unwrap();
    s.orphan_scan().await.unwrap();
    s.sweep().await.unwrap();
    clock.advance(120_000);
    s.scrub().await.unwrap();
    let b = s.bucket("alpha").unwrap();
    s.meter.request(&b.id, Class::A);
    s.flush_meter().unwrap();
    let k = s.create_key("used", Permission::Read, None).await.unwrap();
    s.touch_key(&k.id);
    s.create_bucket("last", BucketOptions::default()).await.unwrap();
    assert_consistent(&s, &backend).await;
    let before = dump(&s);
    let s2 = open(&backend, &clock).await;
    let after = dump(&s2);
    assert_eq!(after, before);
    assert_consistent(&s2, &backend).await;
    // And without the snapshot: replaying the whole log gives the same.
    for l in s3_store::backend::list_all(&*backend, "s3/meta/snap/").await.unwrap() {
        backend.delete(&l.key).await.unwrap();
    }
    let wal = list_all(&backend, "s3/meta/wal/").await;
    assert_eq!(seq(&wal[0]), 1, "one snapshot never prunes");
    let s3 = open(&backend, &clock).await;
    assert_eq!(dump(&s3), before);
}
