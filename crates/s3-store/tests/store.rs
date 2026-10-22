mod common;

use common::*;
use s3_store::*;
use std::collections::BTreeMap;

#[tokio::test]
async fn buckets_and_objects() {
    let (_b, _c, s) = env("crud").await;
    assert!(matches!(s.create_bucket("BAD", BucketOptions::default()).await, Err(StoreError::InvalidBucketName(_))));
    let b = s.create_bucket("photos", BucketOptions::default()).await.unwrap();
    assert_eq!(b.recover_days, 7);
    assert!(matches!(s.create_bucket("photos", BucketOptions::default()).await, Err(StoreError::BucketExists(_))));

    let m = put(&s, "photos", "a/1.jpg", b"one").await;
    assert_eq!(m.etag, md5_hex(b"one"));
    put(&s, "photos", "a/2.jpg", b"two").await;
    put(&s, "photos", "b.txt", b"bee").await;
    assert_eq!(read(&s, "photos", "a/1.jpg").await.unwrap(), b"one");
    assert!(matches!(s.get_object("photos", "nope"), Err(StoreError::NoSuchKey(_))));
    assert!(matches!(s.get_object("nobucket", "x"), Err(StoreError::NoSuchBucket(_))));

    assert_eq!(keys(&s, "photos"), ["a/1.jpg", "a/2.jpg", "b.txt"]);
    assert_eq!(list(&s, "photos", "", Some("/")), (vec!["b.txt".to_string()], vec!["a/".to_string()]));
    assert_eq!(list(&s, "photos", "a/", Some("/")).0, ["a/1.jpg", "a/2.jpg"]);
    let b = s.bucket("photos").unwrap();
    assert_eq!((b.objects, b.bytes), (3, 9));

    // Range reads.
    let meta = s.get_object("photos", "b.txt").unwrap();
    let (len, _) = s.open_blob(&meta, Some(1..3)).await.unwrap();
    assert_eq!(len, 2);

    assert!(s.delete_object("photos", "b.txt", None).await.unwrap());
    assert!(!s.delete_object("photos", "b.txt", None).await.unwrap());
    assert!(matches!(s.delete_bucket("photos", false).await, Err(StoreError::BucketNotEmpty(_))));
    let res = s.delete_objects("photos", &["a/1.jpg".into(), "a/2.jpg".into(), "zzz".into()]).await.unwrap();
    assert_eq!(res.iter().map(|(_, r)| *r.as_ref().unwrap()).collect::<Vec<_>>(), [true, true, false]);
    s.delete_bucket("photos", false).await.unwrap();
    assert!(s.buckets().unwrap().is_empty());
}

#[tokio::test]
async fn listing_pages_and_start_after() {
    let (_b, _c, s) = env("list").await;
    s.create_bucket("lst", BucketOptions::default()).await.unwrap();
    for k in ["a", "b/1", "b/2", "b/3", "c/x/1", "d", "e/"] {
        put(&s, "lst", k, b"x").await;
    }
    // Every page size gives the same result.
    let (k, p) = list(&s, "lst", "", Some("/"));
    assert_eq!(k, ["a", "d"]);
    assert_eq!(p, ["b/", "c/", "e/"]);
    // Resuming after a common prefix skips all of it.
    let page = s.list_objects("lst", &ListQuery { delimiter: Some("/".into()), start_after: Some("b/".into()), max_keys: 10, ..Default::default() }).unwrap();
    assert_eq!(page.prefixes, ["c/", "e/"]);
    // Plain start-after.
    let page = s.list_objects("lst", &ListQuery { start_after: Some("b/2".into()), max_keys: 2, ..Default::default() }).unwrap();
    assert_eq!(page.objects.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(), ["b/3", "c/x/1"]);
    assert!(page.truncated);
    assert_eq!(page.next.as_deref(), Some("c/x/1"));
    // Unicode keys sort by UTF-8 bytes.
    put(&s, "lst", "é", b"x").await;
    put(&s, "lst", "z", b"x").await;
    assert_eq!(keys(&s, "lst").last().unwrap(), "é");
}

#[tokio::test]
async fn overwrite_goes_to_trash_and_restores() {
    let (_b, _c, s) = env("trash").await;
    s.create_bucket("tra", BucketOptions::default()).await.unwrap();
    put(&s, "tra", "k", b"v1").await;
    put(&s, "tra", "k", b"v2").await;
    s.delete_object("tra", "k", None).await.unwrap();
    assert!(read(&s, "tra", "k").await.is_none());
    let (trash, more) = s.list_trash("tra", "", None, 10).unwrap();
    assert!(!more);
    assert_eq!(trash.len(), 2);
    assert_eq!(trash[0].etag, md5_hex(b"v2"), "newest first");
    assert_eq!(s.bucket("tra").unwrap().trash_bytes, 4);
    s.restore("tra", "k", trash[1].deleted_at).await.unwrap();
    assert_eq!(read(&s, "tra", "k").await.unwrap(), b"v1");
    let (trash, _) = s.list_trash("tra", "", None, 10).unwrap();
    assert_eq!(trash.len(), 1);
    // Restoring over a live version moves that one to the trash.
    s.restore("tra", "k", trash[0].deleted_at).await.unwrap();
    assert_eq!(read(&s, "tra", "k").await.unwrap(), b"v2");
    assert_eq!(s.list_trash("tra", "", None, 10).unwrap().0.len(), 1);
    assert!(matches!(s.restore("tra", "k", 1).await, Err(StoreError::NotFound(_))));
}

#[tokio::test]
async fn no_trash_when_recover_days_is_zero() {
    let (_b, _c, s) = env("notrash").await;
    s.create_bucket("tra", BucketOptions { recover_days: Some(0), ..Default::default() }).await.unwrap();
    put(&s, "tra", "k", b"v1").await;
    put(&s, "tra", "k", b"v2").await;
    s.delete_object("tra", "k", None).await.unwrap();
    assert!(s.list_trash("tra", "", None, 10).unwrap().0.is_empty());
    let st = s.stats().unwrap();
    assert_eq!(st.blobs, 2, "unreferenced blobs wait out the quarantine");
    assert!(matches!(s.update_bucket("tra", BucketUpdate { recover_days: Some(31), ..Default::default() }).await, Err(StoreError::InvalidArgument(_))));
}

#[tokio::test]
async fn copies_share_blobs() {
    let (backend, clock, s) = env("copy").await;
    s.create_bucket("aaa", BucketOptions { recover_days: Some(0), ..Default::default() }).await.unwrap();
    s.create_bucket("bbb", BucketOptions { recover_days: Some(0), ..Default::default() }).await.unwrap();
    put(&s, "aaa", "src", b"data").await;
    let mut h = ObjectHeaders::default();
    h.metadata.insert("x".into(), "1".into());
    let m = s.copy_object("aaa", "src", "bbb", "dst", &CopyOptions { headers: Some(h), ..Default::default() }).await.unwrap();
    assert_eq!(m.etag, md5_hex(b"data"));
    assert_eq!(m.headers.metadata.get("x").map(String::as_str), Some("1"));
    assert_eq!(blobs(&backend).await.len(), 1, "no bytes copied");
    s.delete_object("aaa", "src", None).await.unwrap();
    clock.advance(2 * DAY);
    assert_eq!(s.sweep().await.unwrap().blobs_deleted, 0, "the copy still references it");
    assert_eq!(read(&s, "bbb", "dst").await.unwrap(), b"data");
    s.delete_object("bbb", "dst", None).await.unwrap();
    assert_eq!(s.sweep().await.unwrap().blobs_deleted, 0, "quarantine starts at the last reference");
    clock.advance(2 * DAY);
    assert_eq!(s.sweep().await.unwrap().blobs_deleted, 1);
    assert!(blobs(&backend).await.is_empty());
    // A copy with a source condition.
    put(&s, "aaa", "src", b"new").await;
    let r = s.copy_object("aaa", "src", "aaa", "c", &CopyOptions { source_etag: Some("nope".into()), ..Default::default() }).await;
    assert!(matches!(r, Err(StoreError::PreconditionFailed)));
}

#[tokio::test]
async fn conditional_writes() {
    let (backend, _c, s) = env("cond").await;
    s.create_bucket("cnd", BucketOptions::default()).await.unwrap();
    let only_new = WriteCondition { if_none_match: Some("*".into()), ..Default::default() };
    put_with(&s, "cnd", "k", b"1", &only_new).await.unwrap();
    assert!(matches!(put_with(&s, "cnd", "k", b"2", &only_new).await, Err(StoreError::PreconditionFailed)));
    let wrong = WriteCondition { if_match: Some("\"abc\"".into()), ..Default::default() };
    assert!(matches!(put_with(&s, "cnd", "k", b"2", &wrong).await, Err(StoreError::PreconditionFailed)));
    let right = WriteCondition { if_match: Some(format!("\"{}\"", md5_hex(b"1"))), ..Default::default() };
    put_with(&s, "cnd", "k", b"2", &right).await.unwrap();
    let missing = WriteCondition { if_match: Some("*".into()), ..Default::default() };
    assert!(matches!(put_with(&s, "cnd", "nope", b"x", &missing).await, Err(StoreError::NoSuchKey(_))));
    // Failed conditional writes leave no blob behind.
    assert_eq!(blobs(&backend).await.len(), 2);
    assert!(matches!(s.delete_object("cnd", "k", Some("\"zzz\"")).await, Err(StoreError::PreconditionFailed)));
    assert!(s.delete_object("cnd", "k", Some(&md5_hex(b"2"))).await.unwrap());
}

#[tokio::test]
async fn limits_are_enforced() {
    let (_b, _c, s) = env("limits").await;
    s.create_bucket("lst", BucketOptions::default()).await.unwrap();
    assert!(matches!(put_with(&s, "lst", &"k".repeat(1025), b"x", &WriteCondition::none()).await, Err(StoreError::KeyTooLong)));
    let mut tags = BTreeMap::new();
    for i in 0..11 {
        tags.insert(format!("t{i}"), "v".into());
    }
    put(&s, "lst", "k", b"x").await;
    assert!(matches!(s.put_tags("lst", "k", tags).await, Err(StoreError::InvalidArgument(_))));
}

#[tokio::test]
async fn tags_dont_make_versions() {
    let (_b, _c, s) = env("tags").await;
    s.create_bucket("tra", BucketOptions::default()).await.unwrap();
    let m = put(&s, "tra", "k", b"x").await;
    s.put_tags("tra", "k", BTreeMap::from([("env".to_string(), "prod".to_string())])).await.unwrap();
    let after = s.get_object("tra", "k").unwrap();
    assert_eq!(after.modified_at, m.modified_at);
    assert_eq!(s.get_tags("tra", "k").unwrap().get("env").map(String::as_str), Some("prod"));
    assert!(s.list_trash("tra", "", None, 10).unwrap().0.is_empty());
    assert!(matches!(s.put_tags("tra", "nope", BTreeMap::new()).await, Err(StoreError::NoSuchKey(_))));
}

#[tokio::test]
async fn bucket_settings() {
    let (_b, _c, s) = env("settings").await;
    s.create_bucket("set", BucketOptions::default()).await.unwrap();
    let cors = vec![CorsRule { allowed_origins: vec!["*".into()], allowed_methods: vec!["GET".into()], ..Default::default() }];
    let lifecycle = vec![LifecycleRule { prefix: "tmp/".into(), enabled: true, expire_days: Some(1), ..Default::default() }];
    let b = s.update_bucket("set", BucketUpdate { public: Some(true), cors: Some(cors.clone()), lifecycle: Some(lifecycle), ..Default::default() }).await.unwrap();
    assert!(b.public);
    assert_eq!(b.cors, cors);
    assert_eq!(b.lifecycle[0].id, "rule-1");
    let b = s.update_bucket("set", BucketUpdate { cors: Some(vec![]), ..Default::default() }).await.unwrap();
    assert!(b.cors.is_empty());
    let bad = vec![CorsRule { allowed_origins: vec!["*".into()], allowed_methods: vec!["PATCH".into()], ..Default::default() }];
    assert!(s.update_bucket("set", BucketUpdate { cors: Some(bad), ..Default::default() }).await.is_err());
    s.delete_bucket("set", false).await.unwrap();
}

#[tokio::test]
async fn force_delete_removes_everything() {
    let (backend, clock, s) = env("force").await;
    s.create_bucket("frc", BucketOptions::default()).await.unwrap();
    for i in 0..30 {
        put(&s, "frc", &format!("k{i}"), b"x").await;
    }
    put(&s, "frc", "k0", b"y").await;
    let up = s.create_upload("frc", "big", ObjectHeaders::default(), None, None).await.unwrap();
    s.delete_bucket("frc", true).await.unwrap();
    assert!(matches!(s.bucket("frc"), Err(StoreError::NoSuchBucket(_))));
    assert!(matches!(s.upload("frc", &up.id), Err(StoreError::NoSuchBucket(_))));
    clock.advance(2 * DAY);
    let st = s.sweep().await.unwrap();
    assert_eq!(st.blobs_deleted, 31);
    assert!(blobs(&backend).await.is_empty());
}

#[tokio::test]
async fn keys_and_usage() {
    let (_b, clock, s) = env("keys").await;
    let k = s.create_key("ci", Permission::Write, Some(vec!["aaa".into()])).await.unwrap();
    assert_eq!(k.id.len(), 20);
    let found = s.access_key(&k.id).unwrap().unwrap();
    assert_eq!(found.secret, k.secret);
    assert!(found.can_write("aaa") && !found.can_read("bbb") && !found.is_admin());
    s.touch_key(&k.id);
    let admin = s.ensure_key("rollroot", "rollrootsecret", "root").await.unwrap();
    assert!(admin.is_admin());
    s.ensure_key("rollroot", "changedsecret", "root").await.unwrap();
    assert_eq!(s.access_key("rollroot").unwrap().unwrap().secret, "changedsecret");
    assert_eq!(s.keys().unwrap().len(), 2);
    s.delete_key(&k.id).await.unwrap();
    assert!(matches!(s.delete_key(&k.id).await, Err(StoreError::NotFound(_))));

    let b = s.create_bucket("use", BucketOptions { recover_days: Some(0), ..Default::default() }).await.unwrap();
    put(&s, "use", "k", &[0u8; 1000]).await;
    s.flush_meter().unwrap();
    s.meter.request(&b.id, Class::A);
    s.meter.request(&b.id, Class::B);
    s.meter.request(&b.id, Class::B);
    s.meter.request(&b.id, Class::Free);
    s.meter.bytes_in(&b.id, 1000);
    s.meter.egress(&b.id, 10, false);
    s.meter.egress(&b.id, 5, true);
    clock.advance(60_000);
    s.flush_meter().unwrap();
    let days = s.usage(Some(&b.id), "2000-01-01").unwrap();
    assert_eq!(days.len(), 1);
    let d = &days[0];
    assert_eq!((d.class_a, d.class_b, d.bytes_in, d.egress_internet, d.egress_internal), (1, 2, 1000, 10, 5));
    assert!(d.byte_ms >= 1000.0 * 60_000.0, "{}", d.byte_ms);
    assert_eq!(s.usage(None, "2000-01-01").unwrap().len(), 1);
}
