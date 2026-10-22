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
