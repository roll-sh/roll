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
    let (backend, _c, s) = env("trash").await;
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
    assert_consistent(&s, &backend).await;
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
    assert_consistent(&s, &backend).await;
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

async fn new_object(s: &Store, bucket: &str, data: &'static [u8]) -> NewObject {
    let b = s.bucket(bucket).unwrap();
    let blob = s.new_blob();
    let up = s.put_blob(&blob, &b.id, "k", bytes_body(bytes::Bytes::from_static(data)), data.len() as u64, None).await.unwrap();
    NewObject { blob_id: blob, size: up.size, crc32c: up.crc32c, etag: md5_hex(data), headers: ObjectHeaders::default(), checksum: None }
}

#[tokio::test]
async fn rejected_puts_leave_nothing_behind() {
    let (backend, _c, s) = env("rejected").await;
    s.create_bucket("rej", BucketOptions::default()).await.unwrap();
    let none = WriteCondition::none();
    let o = new_object(&s, "rej", b"x").await;
    assert!(matches!(s.commit_put("nobucket", "k", o, &none).await, Err(StoreError::NoSuchBucket(_))));
    let o = new_object(&s, "rej", b"x").await;
    assert!(matches!(s.commit_put("rej", "", o, &none).await, Err(StoreError::InvalidArgument(_))));
    let mut o = new_object(&s, "rej", b"x").await;
    o.headers.metadata.insert("big".into(), "x".repeat(3000));
    assert!(matches!(s.commit_put("rej", "k", o, &none).await, Err(StoreError::MetadataTooLarge)));
    let mut o = new_object(&s, "rej", b"x").await;
    o.headers.tags.insert(String::new(), "v".into());
    assert!(matches!(s.commit_put("rej", "k", o, &none).await, Err(StoreError::InvalidArgument(_))));
    assert!(matches!(put_with(&s, "rej", &"k".repeat(1025), b"x", &none).await, Err(StoreError::KeyTooLong)));
    assert!(blobs(&backend).await.is_empty(), "definite failures delete their blob");
    assert_eq!(s.stats().unwrap().blobs, 0);
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn a_blob_the_metadata_knows_is_never_discarded() {
    let (backend, _c, s) = env("knownblob").await;
    s.create_bucket("kbl", BucketOptions::default()).await.unwrap();
    let m = put(&s, "kbl", "a", b"precious").await;
    // A caller committing the same blob twice (a retry bug) gets an error, not data loss.
    let again = NewObject { blob_id: m.blob_id.clone(), size: m.size, crc32c: crc32c::crc32c(b"precious"), etag: m.etag.clone(), headers: ObjectHeaders::default(), checksum: None };
    assert!(s.commit_put("kbl", "b", again.clone(), &WriteCondition::none()).await.is_err());
    assert_eq!(read(&s, "kbl", "a").await.unwrap(), b"precious");
    assert!(s.commit_put("kbl", &"b".repeat(2000), again, &WriteCondition::none()).await.is_err());
    s.discard_blob(&m.blob_id).await;
    assert_eq!(read(&s, "kbl", "a").await.unwrap(), b"precious");
    // In the trash, and quarantined: still known.
    s.delete_object("kbl", "a", None).await.unwrap();
    s.discard_blob(&m.blob_id).await;
    assert!(blobs(&backend).await.contains(&blob_key(&m.blob_id)));
    let up = s.create_upload("kbl", "mp", ObjectHeaders::default(), None, None).await.unwrap();
    s.discard_blob(&up.blob_id).await;
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn reads_of_missing_and_partial_data() {
    let (backend, _c, s) = env("reads").await;
    s.create_bucket("rds", BucketOptions::default()).await.unwrap();
    let m = put(&s, "rds", "k", b"abc").await;
    for (range, want) in [(0..3, 3), (2..100, 1), (5..9, 0), (0..0, 0)] {
        assert_eq!(s.open_blob(&m, Some(range.clone())).await.unwrap().0, want, "{range:?}");
    }
    backend.delete(&blob_key(&m.blob_id)).await.unwrap();
    assert!(matches!(s.open_blob(&m, None).await, Err(StoreError::Internal(_))), "missing data is an error, never an empty object");
    assert!(matches!(s.get_tags("rds", "nope"), Err(StoreError::NoSuchKey(_))));
    assert!(matches!(s.delete_object("nobucket", "k", None).await, Err(StoreError::NoSuchBucket(_))));
    assert!(matches!(s.delete_object("rds", "nope", Some("*")).await, Err(StoreError::NoSuchKey(_))), "If-Match on a missing key");
    assert!(matches!(s.list_objects("nobucket", &ListQuery { max_keys: 1, ..Default::default() }), Err(StoreError::NoSuchBucket(_))));
}

#[tokio::test]
async fn copy_rules() {
    let (backend, _c, s) = env("copyrules").await;
    s.create_bucket("cpy", BucketOptions::default()).await.unwrap();
    let none = CopyOptions::default();
    let mut h = ObjectHeaders { content_type: Some("text/csv".into()), ..Default::default() };
    h.tags.insert("t".into(), "1".into());
    let b = s.bucket("cpy").unwrap();
    let blob = s.new_blob();
    let up = s.put_blob(&blob, &b.id, "src", bytes_body(bytes::Bytes::from_static(b"data")), 4, None).await.unwrap();
    s.commit_put("cpy", "src", NewObject { blob_id: blob, size: 4, crc32c: up.crc32c, etag: md5_hex(b"data"), headers: h, checksum: None }, &WriteCondition::none()).await.unwrap();
    assert!(matches!(s.copy_object("cpy", "nope", "cpy", "d", &none).await, Err(StoreError::NoSuchKey(_))));
    assert!(matches!(s.copy_object("nobucket", "src", "cpy", "d", &none).await, Err(StoreError::NoSuchBucket(_))));
    assert!(matches!(s.copy_object("cpy", "src", "nobucket", "d", &none).await, Err(StoreError::NoSuchBucket(_))));
    assert!(matches!(s.copy_object("cpy", "src", "cpy", &"d".repeat(1025), &none).await, Err(StoreError::KeyTooLong)));
    let too_many: std::collections::BTreeMap<String, String> = (0..11).map(|i| (format!("t{i}"), "v".into())).collect();
    assert!(matches!(s.copy_object("cpy", "src", "cpy", "d", &CopyOptions { tags: Some(too_many), ..Default::default() }).await, Err(StoreError::InvalidArgument(_))));
    let exists = CopyOptions { condition: WriteCondition { if_none_match: Some("*".into()), ..Default::default() }, ..Default::default() };
    s.copy_object("cpy", "src", "cpy", "d", &exists).await.unwrap();
    assert!(matches!(s.copy_object("cpy", "src", "cpy", "d", &exists).await, Err(StoreError::PreconditionFailed)), "destination conditions");
    // COPY keeps headers and tags; REPLACE swaps headers but keeps tags unless they're replaced too.
    let d = s.get_object("cpy", "d").unwrap();
    assert_eq!((d.headers.content_type.as_deref(), d.headers.tags.len()), (Some("text/csv"), 1));
    let replaced = s.copy_object("cpy", "src", "cpy", "e", &CopyOptions { headers: Some(ObjectHeaders::default()), source_etag: Some(format!("\"{}\"", md5_hex(b"data"))), ..Default::default() }).await.unwrap();
    assert_eq!((replaced.headers.content_type, replaced.headers.tags.len()), (None, 1));
    let retagged = s.copy_object("cpy", "src", "cpy", "f", &CopyOptions { tags: Some(Default::default()), ..Default::default() }).await.unwrap();
    assert!(retagged.headers.tags.is_empty());
    // Copying an object onto itself (S3's way to change metadata) keeps one blob and the old version in the trash.
    let mut meta = ObjectHeaders::default();
    meta.metadata.insert("v".into(), "2".into());
    let me = s.copy_object("cpy", "src", "cpy", "src", &CopyOptions { headers: Some(meta), ..Default::default() }).await.unwrap();
    assert_eq!(me.headers.metadata.get("v").map(String::as_str), Some("2"));
    assert_eq!(s.list_trash("cpy", "src", None, 10).unwrap().0.len(), 1);
    assert_eq!(blobs(&backend).await.len(), 1);
    assert_eq!(blob_row(&s, &me.blob_id).unwrap().2, 5, "src, its old version in the trash, d, e and f");
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn batch_and_prefix_deletes() {
    let (backend, _c, s) = env("batchdel").await;
    s.create_bucket("bdl", BucketOptions::default()).await.unwrap();
    for k in ["a/1", "a/2", "a/3", "b"] {
        put(&s, "bdl", k, b"x").await;
    }
    let res = s.delete_objects("bdl", &["a/1".into(), String::new(), "k".repeat(1025), "nope".into()]).await.unwrap();
    assert!(matches!(res[0].1, Ok(true)));
    assert!(matches!(res[1].1, Err(StoreError::InvalidArgument(_))));
    assert!(matches!(res[2].1, Err(StoreError::KeyTooLong)));
    assert!(matches!(res[3].1, Ok(false)));
    assert!(matches!(s.delete_objects("nobucket", &["a".into()]).await, Err(StoreError::NoSuchBucket(_))));
    assert_eq!(s.delete_prefix("bdl", "a/").await.unwrap(), 2);
    assert_eq!(s.delete_prefix("bdl", "a/").await.unwrap(), 0);
    assert_eq!(keys(&s, "bdl"), ["b"]);
    assert_eq!(s.list_trash("bdl", "", None, 10).unwrap().0.len(), 3, "to the trash");
    assert_eq!(s.delete_prefix("bdl", "").await.unwrap(), 1);
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn trash_pages_by_key_then_newest_and_expires() {
    // A frozen clock: several versions deleted in the same millisecond.
    let backend = test_backend("trashpages").await;
    let mut cfg = config(&TestClock::default());
    cfg.clock = std::sync::Arc::new(|| 1_700_000_000_000);
    let s = Store::open(cfg, backend.clone()).await.unwrap();
    s.create_bucket("tpg", BucketOptions { recover_days: Some(3), ..Default::default() }).await.unwrap();
    for v in [&b"1"[..], b"2", b"3", b"4"] {
        put(&s, "tpg", "a", v).await;
    }
    put(&s, "tpg", "b/x", b"5").await;
    put(&s, "tpg", "b/x", b"6").await;
    put(&s, "tpg", "c", b"7").await;
    s.delete_object("tpg", "c", None).await.unwrap();
    let mut all = Vec::new();
    let mut after: Option<(String, i64)> = None;
    loop {
        let (page, more) = s.list_trash("tpg", "", after.as_ref().map(|(k, d)| (k.as_str(), *d)), 2).unwrap();
        assert!(page.len() <= 2);
        after = page.last().map(|e| (e.key.clone(), e.deleted_at));
        all.extend(page);
        if !more {
            break;
        }
    }
    let got: Vec<(&str, &str)> = all.iter().map(|e| (e.key.as_str(), e.etag.as_str())).collect();
    let (m1, m2, m3, m5, m7) = (md5_hex(b"1"), md5_hex(b"2"), md5_hex(b"3"), md5_hex(b"5"), md5_hex(b"7"));
    assert_eq!(got, [("a", m3.as_str()), ("a", m2.as_str()), ("a", m1.as_str()), ("b/x", m5.as_str()), ("c", m7.as_str())]);
    let a: Vec<i64> = all.iter().filter(|e| e.key == "a").map(|e| e.deleted_at).collect();
    assert!(a.windows(2).all(|w| w[0] > w[1]), "same-millisecond versions get distinct times: {a:?}");
    assert!(all.iter().all(|e| e.expires_at == e.deleted_at + 3 * DAY));
    let (b, more) = s.list_trash("tpg", "b/", None, 10).unwrap();
    assert_eq!((b.len(), more), (1, false));
    let (one, more) = s.list_trash("tpg", "a", None, 1).unwrap();
    assert_eq!((one.len(), more), (1, true));
    // Each one restores.
    for e in all.iter().filter(|e| e.key == "a") {
        let m = s.restore("tpg", "a", e.deleted_at).await.unwrap();
        assert_eq!(m.etag, e.etag);
    }
    assert!(matches!(s.list_trash("nobucket", "", None, 1), Err(StoreError::NoSuchBucket(_))));
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn restore_edge_cases() {
    let (backend, clock, s) = env("restore").await;
    s.create_bucket("rst", BucketOptions::default()).await.unwrap();
    put(&s, "rst", "k", b"v1").await;
    put(&s, "rst", "k", b"v2").await;
    let (t, _) = s.list_trash("rst", "", None, 10).unwrap();
    assert!(matches!(s.restore("nobucket", "k", t[0].deleted_at).await, Err(StoreError::NoSuchBucket(_))));
    assert!(matches!(s.restore("rst", "other", t[0].deleted_at).await, Err(StoreError::NotFound(_))));
    // The window shrank to nothing: the restored version replaces the current one outright.
    s.update_bucket("rst", BucketUpdate { recover_days: Some(0), ..Default::default() }).await.unwrap();
    s.restore("rst", "k", t[0].deleted_at).await.unwrap();
    assert_eq!(read(&s, "rst", "k").await.unwrap(), b"v1");
    assert!(s.list_trash("rst", "", None, 10).unwrap().0.is_empty());
    assert_consistent(&s, &backend).await;
    clock.advance(2 * DAY);
    assert_eq!(s.sweep().await.unwrap().blobs_deleted, 1, "v2, after the quarantine");
    assert_eq!(read(&s, "rst", "k").await.unwrap(), b"v1");
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn bucket_lookups_and_validation() {
    let (_b, _c, s) = env("bucketval").await;
    let b = s.create_bucket("zeta", BucketOptions { public: Some(true), recover_days: Some(0), ..Default::default() }).await.unwrap();
    assert_eq!(b.region, s.cfg.region);
    assert!(b.public && b.recover_days == 0 && b.parent.is_none() && b.branches.is_empty());
    assert!(matches!(s.create_bucket("alpha", BucketOptions { recover_days: Some(31), ..Default::default() }).await, Err(StoreError::InvalidArgument(_))));
    s.create_bucket("alpha", BucketOptions::default()).await.unwrap();
    assert_eq!(s.buckets().unwrap().iter().map(|b| b.name.as_str()).collect::<Vec<_>>(), ["alpha", "zeta"]);
    assert_eq!(s.bucket_by_id(&b.id).unwrap().unwrap().name, "zeta");
    assert!(s.bucket_by_id("bkt_nope").unwrap().is_none());
    assert!(matches!(s.update_bucket("nobucket", BucketUpdate::default()).await, Err(StoreError::NoSuchBucket(_))));
    let lc = |expire, abort| vec![LifecycleRule { id: String::new(), prefix: String::new(), enabled: true, expire_days: expire, abort_multipart_days: abort, ..Default::default() }];
    for bad in [lc(Some(0), None), lc(None, None)] {
        assert!(matches!(s.update_bucket("zeta", BucketUpdate { lifecycle: Some(bad), ..Default::default() }).await, Err(StoreError::InvalidArgument(_))));
    }
    let mut cors = vec![CorsRule { allowed_origins: vec!["https://app.dev".into()], allowed_methods: vec!["GET".into()], ..Default::default() }];
    let lifecycle = lc(Some(5), Some(2));
    s.update_bucket("zeta", BucketUpdate { cors: Some(cors.clone()), lifecycle: Some(lifecycle), recover_days: Some(4), public: Some(false), ..Default::default() }).await.unwrap();
    // A branch starts with its parent's settings.
    let br = s.branch_bucket("zeta", "zeta-dev").await.unwrap();
    assert_eq!((br.public, br.recover_days, br.cors.clone(), br.lifecycle.len(), br.region.clone()), (false, 4, cors.clone(), 1, s.cfg.region.clone()));
    assert_eq!(br.parent.as_ref().map(|p| (p.id.as_str(), p.name.as_str())), Some((b.id.as_str(), "zeta")));
    assert!(matches!(s.branch_bucket("nobucket", "x-dev").await, Err(StoreError::NoSuchBucket(_))));
    assert!(matches!(s.branch_bucket("zeta", "alpha").await, Err(StoreError::BucketExists(_))));
    assert!(matches!(s.branch_bucket("zeta", "Bad_Name").await, Err(StoreError::InvalidBucketName(_))));
    // Changing the parent afterwards doesn't change the branch's settings.
    cors[0].allowed_methods.push("PUT".into());
    s.update_bucket("zeta", BucketUpdate { cors: Some(cors), lifecycle: Some(vec![]), ..Default::default() }).await.unwrap();
    let br = s.bucket("zeta-dev").unwrap();
    assert_eq!((br.cors[0].allowed_methods.len(), br.lifecycle.len()), (1, 1));
    assert!(s.bucket("zeta").unwrap().lifecycle.is_empty());
    assert!(matches!(s.delete_bucket("nobucket", false).await, Err(StoreError::NoSuchBucket(_))));
}

#[tokio::test]
async fn deleting_a_bucket_still_quarantines_its_blobs() {
    let (backend, clock, s) = env("delquarantine").await;
    s.create_bucket("dq1", BucketOptions::default()).await.unwrap();
    put(&s, "dq1", "k", b"x").await;
    s.delete_object("dq1", "k", None).await.unwrap();
    // Only trash left: the view is empty, so a plain delete works, and takes the trash with it.
    s.delete_bucket("dq1", false).await.unwrap();
    s.create_bucket("dq2", BucketOptions::default()).await.unwrap();
    put(&s, "dq2", "k", b"y").await;
    s.delete_bucket("dq2", true).await.unwrap();
    assert_consistent(&s, &backend).await;
    assert_eq!(s.sweep().await.unwrap().blobs_deleted, 0, "the quarantine applies to deleted buckets too");
    assert_eq!(blobs(&backend).await.len(), 2);
    clock.advance(DAY + 60_000);
    assert_eq!(s.sweep().await.unwrap().blobs_deleted, 2);
    assert!(blobs(&backend).await.is_empty());
    assert_eq!(s.stats().unwrap().blobs, 0);
}

#[tokio::test]
async fn key_rules_and_usage_times() {
    let (backend, clock, s) = env("keyrules").await;
    assert!(matches!(s.create_key("", Permission::Read, None).await, Err(StoreError::InvalidArgument(_))));
    assert!(matches!(s.create_key(&"n".repeat(129), Permission::Read, None).await, Err(StoreError::InvalidArgument(_))));
    assert!(matches!(s.ensure_key("ab", "longenoughsecret", "x").await, Err(StoreError::InvalidArgument(_))));
    assert!(matches!(s.ensure_key("abc", "short", "x").await, Err(StoreError::InvalidArgument(_))));
    assert!(s.access_key("nope").unwrap().is_none());
    let r = s.create_key("reader", Permission::Read, None).await.unwrap();
    assert!(r.can_read("any") && !r.can_write("any") && !r.is_admin());
    let w = s.create_key("writer", Permission::Write, Some(vec!["one".into()])).await.unwrap();
    assert!(w.can_write("one") && !w.can_write("two") && !w.covers("two"));
    assert_ne!(r.id, w.id);
    assert_ne!(r.secret, w.secret);
    // ensure_key promotes an existing key to an unrestricted admin.
    let promoted = s.ensure_key(&w.id, "newsecretnewsecret", "ignored").await.unwrap();
    assert!(promoted.is_admin() && promoted.buckets.is_none());
    assert_eq!(promoted.name, "writer");
    // Usage times: written at most hourly.
    assert!(s.access_key(&r.id).unwrap().unwrap().last_used_at.is_none());
    s.touch_key(&r.id);
    let first = s.access_key(&r.id).unwrap().unwrap().last_used_at.unwrap();
    clock.advance(60_000);
    s.touch_key(&r.id);
    assert_eq!(s.access_key(&r.id).unwrap().unwrap().last_used_at, Some(first));
    clock.advance(3_600_000);
    s.touch_key(&r.id);
    let second = s.access_key(&r.id).unwrap().unwrap().last_used_at.unwrap();
    assert!(second > first);
    s.touch_key("RKUNKNOWN");
    // Relaxed writes still reach object storage with the next commit (here, the close).
    s.close().await;
    let s2 = open(&backend, &clock).await;
    let keys = s2.keys().unwrap();
    assert_eq!(keys.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(), ["reader", "writer"]);
    assert_eq!(s2.access_key(&r.id).unwrap().unwrap().last_used_at, Some(second));
    assert_eq!(s2.access_key(&w.id).unwrap().unwrap().secret, "newsecretnewsecret");
}

#[tokio::test]
async fn metering_rules() {
    let (backend, clock, s) = env("metering").await;
    let a = s.create_bucket("mta", BucketOptions::default()).await.unwrap();
    let gone = s.create_bucket("mtb", BucketOptions::default()).await.unwrap();
    put(&s, "mta", "k", &[0u8; 100]).await;
    put(&s, "mta", "k", &[0u8; 50]).await;
    s.flush_meter().unwrap();
    s.meter.request(&a.id, Class::A);
    s.meter.request(&a.id, Class::Free);
    s.meter.request(&gone.id, Class::B);
    s.delete_bucket("mtb", false).await.unwrap();
    clock.advance(1000);
    s.flush_meter().unwrap();
    let today = day_of(s.now());
    let rows = s.usage(None, &today).unwrap();
    let get = |id: &str| rows.iter().find(|d| d.bucket_id == id).cloned().unwrap();
    let ua = get(&a.id);
    assert_eq!((ua.class_a, ua.class_b), (1, 0), "DELETE and Abort are free");
    assert!(ua.byte_ms >= 150.0 * 1000.0, "the trash is billed as storage: {}", ua.byte_ms);
    assert_eq!(get(&gone.id).class_b, 1, "requests to a bucket deleted meanwhile still count");
    assert!(s.usage(Some(&a.id), "9999-01-01").unwrap().is_empty());
    // Close flushes what's pending; a closed store keeps what it couldn't flush.
    s.meter.request(&a.id, Class::B);
    s.close().await;
    s.meter.request(&a.id, Class::A);
    assert!(s.flush_meter().is_err());
    assert_eq!(s.meter.pending(&a.id).class_a, 1);
    let s2 = open(&backend, &clock).await;
    let after = s2.usage(Some(&a.id), &today).unwrap();
    assert_eq!((after[0].class_a, after[0].class_b), (1, 1));
}
