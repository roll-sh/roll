mod common;

use bytes::Bytes;
use common::*;
use s3_store::*;

#[tokio::test]
async fn trash_expires_then_blobs_after_quarantine() {
    let (backend, clock, s) = env("sweep").await;
    s.create_bucket("swp", BucketOptions { recover_days: Some(1), ..Default::default() }).await.unwrap();
    put(&s, "swp", "k", b"v1").await;
    put(&s, "swp", "k", b"v2").await;
    clock.advance(DAY / 2);
    let st = s.sweep().await.unwrap();
    assert_eq!((st.trash_expired, st.blobs_deleted), (0, 0));
    clock.advance(DAY);
    let st = s.sweep().await.unwrap();
    assert_eq!((st.trash_expired, st.blobs_deleted), (1, 0), "quarantine still holds the blob");
    assert_eq!(blobs(&backend).await.len(), 2);
    clock.advance(DAY + 1000);
    let st = s.sweep().await.unwrap();
    assert_eq!(st.blobs_deleted, 1);
    assert_eq!(blobs(&backend).await.len(), 1);
    assert_eq!(read(&s, "swp", "k").await.unwrap(), b"v2");
}

#[tokio::test]
async fn stale_uploads_are_aborted() {
    let (_b, clock, s) = env("stale").await;
    s.create_bucket("stl", BucketOptions::default()).await.unwrap();
    let rule = LifecycleRule { prefix: "tmp/".into(), enabled: true, abort_multipart_days: Some(1), ..Default::default() };
    s.update_bucket("stl", BucketUpdate { lifecycle: Some(vec![rule]), ..Default::default() }).await.unwrap();
    let a = s.create_upload("stl", "tmp/x", ObjectHeaders::default(), None, None).await.unwrap();
    let b = s.create_upload("stl", "keep/x", ObjectHeaders::default(), None, None).await.unwrap();
    clock.advance(2 * DAY);
    assert_eq!(s.sweep().await.unwrap().uploads_aborted, 1);
    assert!(s.upload("stl", &a.id).is_err());
    assert!(s.upload("stl", &b.id).is_ok());
    clock.advance(6 * DAY);
    assert_eq!(s.sweep().await.unwrap().uploads_aborted, 1, "the 7-day default");
}

#[tokio::test]
async fn lifecycle_expires_to_trash() {
    let (_b, clock, s) = env("lifecycle").await;
    s.create_bucket("lfc", BucketOptions::default()).await.unwrap();
    let rule = LifecycleRule { prefix: "logs/".into(), enabled: true, expire_days: Some(3), ..Default::default() };
    s.update_bucket("lfc", BucketUpdate { lifecycle: Some(vec![rule]), ..Default::default() }).await.unwrap();
    put(&s, "lfc", "logs/old", b"x").await;
    put(&s, "lfc", "keep", b"x").await;
    clock.advance(2 * DAY);
    put(&s, "lfc", "logs/new", b"x").await;
    assert_eq!(s.run_lifecycle().await.unwrap(), 0);
    clock.advance(DAY + 1000);
    assert_eq!(s.run_lifecycle().await.unwrap(), 1);
    assert_eq!(keys(&s, "lfc"), ["keep", "logs/new"]);
    assert_eq!(s.list_trash("lfc", "", None, 10).unwrap().0.len(), 1, "recoverable");
}

#[tokio::test]
async fn orphan_scan_only_deletes_unacknowledged_blobs() {
    let (backend, clock, s) = env("orphans").await;
    s.create_bucket("orp", BucketOptions::default()).await.unwrap();
    put(&s, "orp", "live", b"live").await;
    // A blob uploaded but never committed (a crash before the commit).
    let orphan = "zzorphanorphanorphanorphan";
    backend.put(&blob_key(orphan), bytes_body(Bytes::from_static(b"o")), 1, &BlobAttrs::default()).await.unwrap();
    // One being uploaded right now.
    let b = s.bucket("orp").unwrap();
    let inflight = s.new_blob();
    s.put_blob(&inflight, &b.id, "pending", bytes_body(Bytes::from_static(b"p")), 1, None).await.unwrap();
    // An open multipart upload.
    s.create_upload("orp", "mp", ObjectHeaders::default(), None, None).await.unwrap();

    let st = s.orphan_scan().await.unwrap();
    assert_eq!((st.deleted, st.candidates), (0, 1), "first recorded");
    clock.advance(DAY / 2);
    assert_eq!(s.orphan_scan().await.unwrap().deleted, 0, "then deleted a day later");
    clock.advance(DAY);
    let st = s.orphan_scan().await.unwrap();
    assert_eq!(st.deleted, 1);
    let left = blobs(&backend).await;
    assert!(!left.contains(&blob_key(orphan)));
    assert!(left.contains(&blob_key(&inflight)));
    assert_eq!(read(&s, "orp", "live").await.unwrap(), b"live");

    // Old blobs without metadata mean lost metadata: kept and reported.
    let ancient = "zyancientancientancientanc";
    backend.put(&blob_key(ancient), bytes_body(Bytes::from_static(b"a")), 1, &BlobAttrs::default()).await.unwrap();
    s.release_blob(&inflight);
    clock.advance(30 * DAY);
    let st = s.orphan_scan().await.unwrap();
    assert_eq!(st.deleted, 0);
    assert_eq!(s.stats().unwrap().unexpected_orphans, 2);
    assert!(blobs(&backend).await.contains(&blob_key(ancient)));
}

#[tokio::test]
async fn scrub_reports_missing_and_wrong_blobs() {
    let (backend, clock, s) = env("scrub").await;
    s.create_bucket("scr", BucketOptions::default()).await.unwrap();
    for i in 0..5 {
        put(&s, "scr", &format!("k{i}"), b"data").await;
    }
    clock.advance(120_000);
    let r = s.scrub().await.unwrap();
    assert_eq!((r.checked, r.missing, r.mismatched), (5, 0, 0));
    let m0 = s.get_object("scr", "k0").unwrap();
    let m1 = s.get_object("scr", "k1").unwrap();
    backend.delete(&blob_key(&m0.blob_id)).await.unwrap();
    backend.put(&blob_key(&m1.blob_id), bytes_body(Bytes::from_static(b"no")), 2, &BlobAttrs::default()).await.unwrap();
    let r = s.scrub().await.unwrap();
    assert_eq!((r.checked, r.missing, r.mismatched), (5, 1, 1));
    assert_eq!(s.stats().unwrap().scrub.missing, 1);
    // Never deletes metadata.
    assert_eq!(keys(&s, "scr").len(), 5);
}

#[tokio::test]
async fn a_stale_node_never_deletes_the_new_writers_blobs() {
    let (backend, clock, old) = env("stale-gc").await;
    old.create_bucket("stg", BucketOptions::default()).await.unwrap();
    put(&old, "stg", "a", b"a").await;
    // Another node takes over; the old one hasn't noticed (it has written nothing since).
    let new = open(&backend, &clock).await;
    put(&new, "stg", "b", b"b").await;
    clock.advance(3 * DAY);
    assert!(matches!(old.sweep().await, Err(StoreError::Unavailable(_))), "the first job notices");
    for _ in 0..3 {
        assert!(matches!(old.orphan_scan().await, Err(StoreError::Unavailable(_))));
        assert!(old.sweep().await.is_err());
        clock.advance(2 * DAY);
    }
    assert_eq!(read(&new, "stg", "b").await.unwrap(), b"b");
    assert_eq!(new.orphan_scan().await.unwrap().candidates, 0);
}

fn rule(prefix: &str, enabled: bool, expire: Option<u32>, abort: Option<u32>) -> LifecycleRule {
    LifecycleRule { id: String::new(), prefix: prefix.into(), enabled, expire_days: expire, abort_multipart_days: abort }
}

#[tokio::test]
async fn upload_expiry_follows_enabled_rules_capped_at_the_default() {
    let (backend, clock, s) = env("abortrules").await;
    s.create_bucket("abr", BucketOptions::default()).await.unwrap();
    let rules = vec![rule("off/", false, None, Some(1)), rule("long/", true, None, Some(30)), rule("", true, Some(9), None)];
    s.update_bucket("abr", BucketUpdate { lifecycle: Some(rules), ..Default::default() }).await.unwrap();
    let off = s.create_upload("abr", "off/x", ObjectHeaders::default(), None, None).await.unwrap();
    let long = s.create_upload("abr", "long/x", ObjectHeaders::default(), None, None).await.unwrap();
    clock.advance(2 * DAY);
    assert_eq!(s.sweep().await.unwrap().uploads_aborted, 0, "a disabled rule does nothing");
    clock.advance(5 * DAY + 1000);
    assert_eq!(s.sweep().await.unwrap().uploads_aborted, 2, "nothing outlives the 7-day default");
    for u in [off, long] {
        assert!(matches!(s.upload("abr", &u.id), Err(StoreError::NoSuchUpload(_))));
    }
    assert_eq!(s.sweep().await.unwrap().uploads_aborted, 0);
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn trash_expiry_follows_the_buckets_current_window() {
    let (backend, clock, s) = env("trashwindow").await;
    s.create_bucket("twn", BucketOptions { recover_days: Some(30), ..Default::default() }).await.unwrap();
    put(&s, "twn", "k", b"1").await;
    put(&s, "twn", "k", b"2").await;
    clock.advance(3 * DAY);
    assert_eq!(s.sweep().await.unwrap().trash_expired, 0);
    s.update_bucket("twn", BucketUpdate { recover_days: Some(2), ..Default::default() }).await.unwrap();
    assert_eq!(s.sweep().await.unwrap().trash_expired, 1);
    assert_eq!(s.bucket("twn").unwrap().trash_bytes, 0);
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn lifecycle_rule_details() {
    let (backend, clock, s) = env("lcdetails").await;
    s.create_bucket("lcd", BucketOptions { recover_days: Some(0), ..Default::default() }).await.unwrap();
    let rules = vec![rule("off/", false, Some(1), None), rule("abort/", true, None, Some(1)), rule("gone/", true, Some(1), None)];
    s.update_bucket("lcd", BucketUpdate { lifecycle: Some(rules), ..Default::default() }).await.unwrap();
    for k in ["off/a", "abort/a", "gone/a", "gone/b", "gonex"] {
        put(&s, "lcd", k, b"x").await;
    }
    s.branch_bucket("lcd", "lcd-dev").await.unwrap();
    // The parent's rule goes; the branch keeps its copy.
    s.update_bucket("lcd", BucketUpdate { lifecycle: Some(vec![]), ..Default::default() }).await.unwrap();
    clock.advance(2 * DAY);
    assert_eq!(s.run_lifecycle().await.unwrap(), 2);
    assert_eq!(keys(&s, "lcd-dev"), ["abort/a", "gonex", "off/a"]);
    assert_eq!(keys(&s, "lcd").len(), 5, "the parent is untouched");
    assert!(s.list_trash("lcd-dev", "", None, 10).unwrap().0.is_empty(), "no trash with recover_days 0");
    assert_consistent(&s, &backend).await;
    clock.advance(2 * DAY);
    assert_eq!(s.sweep().await.unwrap().blobs_deleted, 0, "the parent still references them");
    assert_eq!(s.run_lifecycle().await.unwrap(), 0);
}

#[tokio::test]
async fn orphan_scan_edge_cases() {
    let (backend, clock, s) = env("orphanedge").await;
    s.create_bucket("oed", BucketOptions { recover_days: Some(1), ..Default::default() }).await.unwrap();
    put(&s, "oed", "trashed", b"t").await;
    s.delete_object("oed", "trashed", None).await.unwrap();
    s.update_bucket("oed", BucketUpdate { recover_days: Some(0), ..Default::default() }).await.unwrap();
    put(&s, "oed", "quarantined", b"q").await;
    s.delete_object("oed", "quarantined", None).await.unwrap();
    // Not blob keys: never touched, whatever their age.
    for k in ["s3/blobs/ab/short", "s3/blobs/readme.txt", "s3/blobs/zz/ZZUPPERCASEUPPERCASEUPPERC"] {
        backend.put_small(k, Bytes::from_static(b"?")).await.unwrap();
    }
    // A blob whose client was slow: recorded as unknown, then committed before the second scan.
    let b = s.bucket("oed").unwrap();
    let slow = s.new_blob();
    let up = s.put_blob(&slow, &b.id, "slow", bytes_body(Bytes::from_static(b"slow")), 4, None).await.unwrap();
    s.release_blob(&slow);
    // One that vanishes after it's recorded, and comes back.
    let vanishing = "zxvanishedvanishedvanished";
    backend.put(&blob_key(vanishing), bytes_body(Bytes::from_static(b"v")), 1, &BlobAttrs::default()).await.unwrap();
    let st = s.orphan_scan().await.unwrap();
    assert_eq!(st.candidates, 2, "only the slow and the vanishing blob");
    let obj = NewObject { blob_id: slow.clone(), size: up.size, crc32c: up.crc32c, etag: md5_hex(b"slow"), headers: ObjectHeaders::default(), checksum: None };
    s.commit_put("oed", "slow", obj, &WriteCondition::none()).await.unwrap();
    backend.delete(&blob_key(vanishing)).await.unwrap();
    s.orphan_scan().await.unwrap();
    backend.put(&blob_key(vanishing), bytes_body(Bytes::from_static(b"v")), 1, &BlobAttrs::default()).await.unwrap();
    clock.advance(DAY + 60_000);
    let st = s.orphan_scan().await.unwrap();
    assert_eq!(st.deleted, 0, "the committed one is known now, and the vanished one starts over");
    assert_eq!(st.candidates, 1);
    assert_eq!(read(&s, "oed", "slow").await.unwrap(), b"slow");
    clock.advance(DAY + 60_000);
    assert_eq!(s.orphan_scan().await.unwrap().deleted, 1);
    let left = blobs(&backend).await;
    assert!(!left.contains(&blob_key(vanishing)));
    for k in ["s3/blobs/ab/short", "s3/blobs/readme.txt", "s3/blobs/zz/ZZUPPERCASEUPPERCASEUPPERC"] {
        assert!(left.contains(&k.to_string()), "{k}");
    }
    // The trash and quarantine kept theirs until the sweeper's turn.
    assert_consistent(&s, &backend).await;
    assert_eq!(s.stats().unwrap().blobs, 3);
    // Unexpected orphans are counted per scan, not forever.
    clock.advance(30 * DAY);
    let ancient = "zyancientancientancientanc";
    backend.put(&blob_key(ancient), bytes_body(Bytes::from_static(b"a")), 1, &BlobAttrs::default()).await.unwrap();
    s.orphan_scan().await.unwrap();
    assert_eq!(s.stats().unwrap().unexpected_orphans, 1);
    assert!(blobs(&backend).await.contains(&blob_key(ancient)), "kept");
    backend.delete(&blob_key(ancient)).await.unwrap();
    s.orphan_scan().await.unwrap();
    assert_eq!(s.stats().unwrap().unexpected_orphans, 0);
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn scrub_details() {
    let (backend, clock, s) = env("scrubdetail").await;
    s.create_bucket("scd", BucketOptions { recover_days: Some(0), ..Default::default() }).await.unwrap();
    let m = put(&s, "scd", "k", b"data").await;
    let r = s.scrub().await.unwrap();
    assert_eq!(r.checked, 0, "blobs newer than a minute are skipped (their upload may still be settling)");
    put(&s, "scd", "q", b"quarantined").await;
    s.delete_object("scd", "q", None).await.unwrap();
    clock.advance(120_000);
    backend.delete(&blob_key(&m.blob_id)).await.unwrap();
    let r = s.scrub().await.unwrap();
    assert_eq!((r.checked, r.missing, r.mismatched), (2, 1, 0), "quarantined blobs are checked too");
    assert_eq!(r.examples, std::slice::from_ref(&m.blob_id));
    assert!(r.last_at.is_some());
    // Never deletes anything, and the report survives a restart.
    assert_eq!(s.get_object("scd", "k").unwrap().blob_id, m.blob_id);
    assert_eq!(s.stats().unwrap().blobs, 2);
    s.create_bucket("barrier", BucketOptions::default()).await.unwrap();
    let s2 = open(&backend, &clock).await;
    assert_eq!(s2.stats().unwrap().scrub, r);
}
