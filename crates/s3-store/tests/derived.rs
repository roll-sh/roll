//! Derived images (image transformations): the cache by source blob and transformation, its
//! references and garbage collection, expiry, and metering by unique source per month.

mod common;

use bytes::Bytes;
use common::*;
use futures::StreamExt;
use s3_store::*;
use std::sync::Arc;

/// Upload and record a derived image of `source`; `(entry, newly recorded)`.
async fn make(s: &Store, bucket: &str, source: &str, params: &str, data: &[u8]) -> (Derived, bool) {
    let b = s.bucket(bucket).unwrap();
    let blob = s.new_blob();
    let up = s.put_derived_blob(&blob, &b.id, source, params, Bytes::copy_from_slice(data), "image/webp", None).await.unwrap();
    let new = NewDerived { source_blob: source.into(), params: params.into(), blob_id: blob.clone(), size: up.size, crc32c: up.crc32c, content_type: "image/webp".into() };
    let out = s.commit_derived(&new, None).await.unwrap();
    if !out.1 {
        s.discard_blob(&blob).await;
    }
    out
}

async fn bytes_of(s: &Store, d: &Derived) -> Vec<u8> {
    let (_, mut body) = s.open_derived(d).await.unwrap();
    let mut out = Vec::new();
    while let Some(c) = body.next().await {
        out.extend_from_slice(&c.unwrap());
    }
    out
}

async fn has_blob(backend: &Arc<dyn Backend>, id: &str) -> bool {
    blobs(backend).await.contains(&blob_key(id))
}

#[tokio::test]
async fn caches_by_source_blob_and_params() {
    let (backend, _clock, s) = env("derived-cache").await;
    s.create_bucket("imgs", BucketOptions::default()).await.unwrap();
    let src = put(&s, "imgs", "cat.jpg", b"jpeg bytes").await;
    assert_eq!(s.derived(&src.blob_id, "p1").unwrap(), None);
    let (d, new) = make(&s, "imgs", &src.blob_id, "p1", b"webp bytes").await;
    assert!(new);
    assert_eq!((d.size, d.content_type.as_str()), (10, "image/webp"));
    assert_eq!(s.derived(&src.blob_id, "p1").unwrap().as_ref(), Some(&d));
    assert_eq!(bytes_of(&s, &d).await, b"webp bytes");
    assert_eq!(blob_row(&s, &d.blob_id).map(|r| r.2), Some(1), "the derived row is the blob's one reference");
    // A second request that made the same image finds the first one's; its own blob goes.
    let (again, new) = make(&s, "imgs", &src.blob_id, "p1", b"webp bytes").await;
    assert!(!new);
    assert_eq!(again, d);
    assert_eq!(blobs(&backend).await.len(), 2, "the source and one derived image");
    // Another transformation is another entry.
    make(&s, "imgs", &src.blob_id, "p2", b"avif").await;
    assert_eq!(s.derived_of(&src.blob_id).unwrap().len(), 2);
    // Copies share the source blob, so they share its derived images.
    let copy = s.copy_object("imgs", "cat.jpg", "imgs", "copy.jpg", &CopyOptions::default()).await.unwrap();
    assert_eq!(copy.blob_id, src.blob_id);
    assert!(s.derived(&copy.blob_id, "p1").unwrap().is_some());
    // Unknown sources get nothing.
    let blob = s.new_blob();
    let new = NewDerived { source_blob: "nosuchblob".into(), params: "p".into(), blob_id: blob.clone(), size: 1, crc32c: 0, content_type: "image/png".into() };
    assert!(matches!(s.commit_derived(&new, None).await, Err(StoreError::NotFound(_))));
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn an_overwrite_is_a_new_source() {
    let (backend, _clock, s) = env("derived-overwrite").await;
    s.create_bucket("imgs", BucketOptions::default()).await.unwrap();
    let v1 = put(&s, "imgs", "a.png", b"one").await;
    make(&s, "imgs", &v1.blob_id, "p", b"small one").await;
    let v2 = put(&s, "imgs", "a.png", b"two").await;
    assert_ne!(v1.blob_id, v2.blob_id);
    assert_eq!(s.derived(&v2.blob_id, "p").unwrap(), None, "nothing to invalidate: the new bytes are a new blob");
    assert!(s.derived(&v1.blob_id, "p").unwrap().is_some(), "the old version's (in the trash) stays until its blob goes");
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn gc_never_deletes_a_referenced_derived_image() {
    let (backend, clock, s) = env("derived-gc").await;
    s.create_bucket("imgs", BucketOptions::default()).await.unwrap();
    let src = put(&s, "imgs", "a.jpg", b"src").await;
    let (d, _) = make(&s, "imgs", &src.blob_id, "p", b"thumb").await;
    for _ in 0..3 {
        clock.advance(10 * DAY);
        // Keep it in use, so expiry doesn't take it.
        s.derived(&src.blob_id, "p").unwrap();
        let st = s.sweep().await.unwrap();
        assert_eq!((st.blobs_deleted, st.derived_expired), (0, 0));
        let o = s.orphan_scan().await.unwrap();
        assert_eq!((o.candidates, o.deleted), (0, 0), "a derived blob isn't an orphan");
        s.scrub().await.unwrap();
    }
    assert!(has_blob(&backend, &d.blob_id).await);
    assert_eq!(s.scrub().await.unwrap().missing, 0);
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn derived_images_go_with_their_source() {
    let (backend, clock, s) = env("derived-source").await;
    s.create_bucket("imgs", BucketOptions { recover_days: Some(0), ..Default::default() }).await.unwrap();
    let src = put(&s, "imgs", "a.jpg", b"src").await;
    let (d1, _) = make(&s, "imgs", &src.blob_id, "p1", b"t1").await;
    let (d2, _) = make(&s, "imgs", &src.blob_id, "p2", b"t2").await;
    s.delete_object("imgs", "a.jpg", None).await.unwrap();
    clock.advance(1000);
    // No recovery window: the source is unreferenced, in quarantine.
    let st = s.sweep().await.unwrap();
    assert_eq!((st.blobs_deleted, st.derived_expired), (0, 0));
    assert_eq!(blob_row(&s, &src.blob_id).map(|r| r.2), Some(0));
    assert_consistent(&s, &backend).await;
    // A source on its way out gets no new derived images.
    let blob = s.new_blob();
    let new = NewDerived { source_blob: src.blob_id.clone(), params: "p3".into(), blob_id: blob, size: 1, crc32c: 0, content_type: "image/png".into() };
    assert!(matches!(s.commit_derived(&new, None).await, Err(StoreError::NotFound(_))));
    // After the quarantine the source goes, and its derived images with it…
    clock.advance(DAY + 1000);
    let st = s.sweep().await.unwrap();
    assert_eq!((st.blobs_deleted, st.derived_expired), (1, 2));
    assert!(s.derived_of(&src.blob_id).unwrap().is_empty());
    assert!(!has_blob(&backend, &src.blob_id).await);
    // … whose blobs wait out their own quarantine, then go too.
    assert!(has_blob(&backend, &d1.blob_id).await && has_blob(&backend, &d2.blob_id).await);
    assert_eq!(blob_row(&s, &d1.blob_id).map(|r| r.2), Some(0));
    assert_consistent(&s, &backend).await;
    clock.advance(DAY + 1000);
    let st = s.sweep().await.unwrap();
    assert_eq!(st.blobs_deleted, 2);
    assert!(blobs(&backend).await.is_empty());
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn unused_derived_images_expire() {
    let (backend, clock, s) = env("derived-expiry").await;
    s.create_bucket("imgs", BucketOptions::default()).await.unwrap();
    let src = put(&s, "imgs", "a.jpg", b"src").await;
    let (used, _) = make(&s, "imgs", &src.blob_id, "used", b"u").await;
    let (idle, _) = make(&s, "imgs", &src.blob_id, "idle", b"i").await;
    clock.advance(20 * DAY);
    assert!(s.derived(&src.blob_id, "used").unwrap().is_some());
    clock.advance(11 * DAY);
    // 31 days: the idle one expires; the one used 11 days ago stays.
    let st = s.sweep().await.unwrap();
    assert_eq!(st.derived_expired, 1);
    assert_eq!(s.derived(&src.blob_id, "idle").unwrap(), None);
    assert_eq!(s.derived(&src.blob_id, "used").unwrap().map(|d| d.blob_id), Some(used.blob_id.clone()));
    assert_eq!(blob_row(&s, &idle.blob_id).map(|r| r.2), Some(0), "quarantined, not deleted yet");
    assert!(has_blob(&backend, &idle.blob_id).await);
    assert_consistent(&s, &backend).await;
    clock.advance(DAY + 1000);
    assert_eq!(s.sweep().await.unwrap().blobs_deleted, 1);
    assert!(!has_blob(&backend, &idle.blob_id).await);
    assert!(has_blob(&backend, &used.blob_id).await);
    // The source is untouched, and the image can simply be made again.
    assert_eq!(read(&s, "imgs", "a.jpg").await.unwrap(), b"src");
    assert!(make(&s, "imgs", &src.blob_id, "idle", b"i").await.1);
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn the_cache_survives_recovery() {
    let (backend, clock, s) = env("derived-recovery").await;
    s.create_bucket("imgs", BucketOptions::default()).await.unwrap();
    let src = put(&s, "imgs", "a.jpg", b"src").await;
    let (d, _) = make(&s, "imgs", &src.blob_id, "p", b"thumb").await;
    s.record_transform(&s.bucket("imgs").unwrap().id, &src.blob_id, true).await.unwrap();
    s.close().await;
    let s = open(&backend, &clock).await;
    assert_eq!(s.derived(&src.blob_id, "p").unwrap(), Some(d.clone()));
    assert_eq!(bytes_of(&s, &d).await, b"thumb");
    assert_eq!(s.image_usage(None, "0000-00").unwrap().len(), 1, "billing is durable");
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn metering_counts_unique_sources_per_month() {
    let (_backend, clock, s) = env("derived-meter").await;
    s.create_bucket("one", BucketOptions::default()).await.unwrap();
    s.create_bucket("two", BucketOptions::default()).await.unwrap();
    let (one, two) = (s.bucket("one").unwrap().id, s.bucket("two").unwrap().id);
    let month = derived::month_of(s.now());
    assert!(s.record_transform(&one, "blob-a", true).await.unwrap());
    assert!(!s.record_transform(&one, "blob-a", true).await.unwrap(), "once a month however many variants");
    assert!(s.record_transform(&one, "blob-b", true).await.unwrap());
    assert!(s.record_transform(&two, "blob-a", true).await.unwrap(), "per bucket");
    // A cache hit counts the source (billed) but no new variant.
    assert!(!s.record_transform(&one, "blob-b", false).await.unwrap());
    assert!(s.record_transform(&two, "blob-c", false).await.unwrap());
    let usage = s.image_usage(None, &month).unwrap();
    let mut want = vec![
        ImageUsage { bucket_id: one.clone(), month: month.clone(), origin_images: 2, variants: 3 },
        ImageUsage { bucket_id: two.clone(), month: month.clone(), origin_images: 2, variants: 1 },
    ];
    want.sort_by(|a, b| a.bucket_id.cmp(&b.bucket_id));
    assert_eq!(usage, want);
    // Next month counts again.
    clock.advance(32 * DAY);
    let next = derived::month_of(s.now());
    assert_ne!(next, month);
    assert!(s.record_transform(&one, "blob-a", true).await.unwrap());
    let mine = s.image_usage(Some(&one), &month).unwrap();
    assert_eq!(mine.iter().map(|u| (u.month.as_str(), u.origin_images)).collect::<Vec<_>>(), [(month.as_str(), 2), (next.as_str(), 1)]);
    assert_eq!(s.image_usage(Some(&one), &next).unwrap().len(), 1);
}
