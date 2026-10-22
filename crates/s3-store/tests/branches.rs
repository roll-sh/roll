mod common;

use common::*;
use s3_store::*;

#[tokio::test]
async fn a_branch_keeps_its_view() {
    let (backend, clock, s) = env("branch").await;
    s.create_bucket("main", BucketOptions { recover_days: Some(0), ..Default::default() }).await.unwrap();
    put(&s, "main", "a", b"a1").await;
    put(&s, "main", "b", b"b1").await;
    let br = s.branch_bucket("main", "preview").await.unwrap();
    assert_eq!(br.parent.as_ref().unwrap().name, "main");
    assert_eq!(s.bucket("main").unwrap().branches, ["preview"]);
    assert_eq!((br.objects, br.bytes), (0, 0), "nothing copied, nothing billed");
    assert_eq!(keys(&s, "preview"), ["a", "b"]);
    assert_eq!(read(&s, "preview", "a").await.unwrap(), b"a1");

    // The parent changes; the branch doesn't see it.
    put(&s, "main", "a", b"a2").await;
    s.delete_object("main", "b", None).await.unwrap();
    put(&s, "main", "c", b"c1").await;
    assert_eq!(keys(&s, "main"), ["a", "c"]);
    assert_eq!(keys(&s, "preview"), ["a", "b"]);
    assert_eq!(read(&s, "preview", "a").await.unwrap(), b"a1");
    assert_eq!(read(&s, "preview", "b").await.unwrap(), b"b1");
    assert!(read(&s, "preview", "c").await.is_none());
    assert_eq!(read(&s, "main", "a").await.unwrap(), b"a2");

    // The branch changes; the parent doesn't see it.
    put(&s, "preview", "d", b"d1").await;
    put(&s, "preview", "a", b"a3").await;
    s.delete_object("preview", "b", None).await.unwrap();
    assert_eq!(keys(&s, "preview"), ["a", "d"]);
    assert_eq!(keys(&s, "main"), ["a", "c"]);
    assert_eq!(read(&s, "main", "a").await.unwrap(), b"a2");

    // Branches block deleting their parent.
    assert!(matches!(s.delete_bucket("main", true).await, Err(StoreError::BucketHasBranches(_))));
    s.delete_bucket("preview", true).await.unwrap();
    s.delete_bucket("main", true).await.unwrap();
    clock.advance(2 * DAY);
    s.sweep().await.unwrap();
    assert!(blobs(&backend).await.is_empty(), "every blob freed once nothing references it");
    assert_eq!(s.stats().unwrap().blobs, 0);
}

#[tokio::test]
async fn nested_branches() {
    let (backend, _c, s) = env("nested").await;
    s.create_bucket("main", BucketOptions::default()).await.unwrap();
    put(&s, "main", "k", b"1").await;
    s.branch_bucket("main", "child").await.unwrap();
    put(&s, "child", "k", b"2").await;
    put(&s, "child", "only-child", b"x").await;
    s.branch_bucket("child", "grandchild").await.unwrap();
    put(&s, "main", "k", b"3").await;
    put(&s, "child", "k", b"4").await;
    put(&s, "main", "new-in-main", b"x").await;
    assert_eq!(read(&s, "main", "k").await.unwrap(), b"3");
    assert_eq!(read(&s, "child", "k").await.unwrap(), b"4");
    assert_eq!(read(&s, "grandchild", "k").await.unwrap(), b"2");
    assert_eq!(keys(&s, "grandchild"), ["k", "only-child"]);
    assert!(read(&s, "grandchild", "new-in-main").await.is_none());
    // Copy out of a branch, tags on an inherited object.
    s.copy_object("grandchild", "k", "main", "from-gc", &CopyOptions::default()).await.unwrap();
    assert_eq!(read(&s, "main", "from-gc").await.unwrap(), b"2");
    s.put_tags("grandchild", "only-child", [("t".to_string(), "1".to_string())].into()).await.unwrap();
    assert!(s.get_tags("child", "only-child").unwrap().is_empty());
    assert_eq!(s.get_tags("grandchild", "only-child").unwrap().len(), 1);
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn listing_merges_ancestors() {
    let (backend, _c, s) = env("merge").await;
    s.create_bucket("main", BucketOptions::default()).await.unwrap();
    for k in ["a/1", "a/2", "b/1", "c", "e/1"] {
        put(&s, "main", k, b"x").await;
    }
    s.branch_bucket("main", "brn").await.unwrap();
    put(&s, "brn", "a/3", b"x").await;
    put(&s, "brn", "d", b"x").await;
    s.delete_object("brn", "b/1", None).await.unwrap();
    s.delete_object("brn", "c", None).await.unwrap();
    put(&s, "main", "f", b"x").await;
    assert_eq!(keys(&s, "brn"), ["a/1", "a/2", "a/3", "d", "e/1"]);
    let (k, p) = list(&s, "brn", "", Some("/"));
    assert_eq!(k, ["d"]);
    assert_eq!(p, ["a/", "e/"], "b/ has only a hidden key");
    assert_eq!(list(&s, "brn", "a/", Some("/")).0, ["a/1", "a/2", "a/3"]);
    // Branch of an empty view isn't "not empty" because of tombstones.
    for k in ["a/1", "a/2", "a/3", "d", "e/1"] {
        s.delete_object("brn", k, None).await.unwrap();
    }
    assert!(keys(&s, "brn").is_empty());
    s.delete_bucket("brn", false).await.unwrap();
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn branch_trash_and_restore() {
    let (backend, _c, s) = env("brtrash").await;
    s.create_bucket("main", BucketOptions::default()).await.unwrap();
    put(&s, "main", "k", b"v1").await;
    s.branch_bucket("main", "brn").await.unwrap();
    // Deleting an inherited object puts it in the branch's trash.
    s.delete_object("brn", "k", None).await.unwrap();
    assert!(read(&s, "brn", "k").await.is_none());
    assert_eq!(read(&s, "main", "k").await.unwrap(), b"v1");
    let (t, _) = s.list_trash("brn", "", None, 10).unwrap();
    assert_eq!(t.len(), 1);
    s.restore("brn", "k", t[0].deleted_at).await.unwrap();
    assert_eq!(read(&s, "brn", "k").await.unwrap(), b"v1");
    assert!(s.list_trash("main", "", None, 10).unwrap().0.is_empty());
    assert_consistent(&s, &backend).await;
}

/// Everything a reader can see of a bucket: keys, bytes, ETags and tags.
async fn view(s: &Store, bucket: &str) -> Vec<(String, Vec<u8>, String, Vec<(String, String)>)> {
    let mut out = Vec::new();
    for k in keys(s, bucket) {
        let m = s.get_object(bucket, &k).unwrap();
        out.push((k.clone(), read(s, bucket, &k).await.unwrap(), m.etag, m.headers.tags.into_iter().collect()));
    }
    out
}

async fn multipart(s: &Store, bucket: &str, key: &str, data: &[u8]) -> ObjectMeta {
    let up = s.create_upload(bucket, key, ObjectHeaders::default(), None, None).await.unwrap();
    let d = s.put_part_data(&up, 1, bytes_body(bytes::Bytes::copy_from_slice(data)), data.len() as u64).await.unwrap();
    let p = s.commit_part(&up, 1, &d, &md5_hex(data), None).await.unwrap();
    s.complete_upload(bucket, &up.id, &[(1, p.etag)], None, &WriteCondition::none()).await.unwrap()
}

#[tokio::test]
async fn every_write_path_keeps_branches_frozen() {
    let (backend, clock, s) = env("frozen").await;
    s.create_bucket("main", BucketOptions::default()).await.unwrap();
    for k in ["put", "del", "batch", "pfx/1", "pfx/2", "copied-over", "tagged", "restored", "expired/1", "mp"] {
        put(&s, "main", k, k.as_bytes()).await;
    }
    put(&s, "main", "restored", b"newer").await;
    let rule = LifecycleRule { id: "x".into(), prefix: "expired/".into(), enabled: true, expire_days: Some(1), abort_multipart_days: None };
    s.update_bucket("main", BucketUpdate { lifecycle: Some(vec![rule]), ..Default::default() }).await.unwrap();
    s.branch_bucket("main", "brn").await.unwrap();
    s.update_bucket("brn", BucketUpdate { lifecycle: Some(vec![]), ..Default::default() }).await.unwrap();
    let frozen = view(&s, "brn").await;
    assert_eq!(frozen, view(&s, "main").await);
    clock.advance(2 * DAY);

    // Every kind of write to the parent, existing keys and new ones.
    put(&s, "main", "put", b"changed").await;
    put(&s, "main", "new-put", b"new").await;
    s.delete_object("main", "del", None).await.unwrap();
    s.delete_objects("main", &["batch".into(), "never-was".into()]).await.unwrap();
    s.delete_prefix("main", "pfx/").await.unwrap();
    s.copy_object("main", "put", "main", "copied-over", &CopyOptions::default()).await.unwrap();
    s.copy_object("main", "put", "main", "new-copy", &CopyOptions::default()).await.unwrap();
    s.put_tags("main", "tagged", [("t".to_string(), "1".to_string())].into()).await.unwrap();
    let t = s.list_trash("main", "restored", None, 10).unwrap().0;
    s.restore("main", "restored", t[0].deleted_at).await.unwrap();
    assert_eq!(s.run_lifecycle().await.unwrap(), 1);
    multipart(&s, "main", "mp", b"replaced in parts").await;
    multipart(&s, "main", "new-mp", b"new in parts").await;

    assert_ne!(view(&s, "main").await, frozen);
    assert_eq!(view(&s, "brn").await, frozen, "the branch's view never changes under it");
    for k in ["new-put", "new-copy", "new-mp"] {
        assert!(matches!(s.get_object("brn", k), Err(StoreError::NoSuchKey(_))), "{k}");
    }
    assert_consistent(&s, &backend).await;
    // The branch's pushed-down copies hold their blobs through the parent's trash expiry.
    clock.advance(10 * DAY);
    s.sweep().await.unwrap();
    clock.advance(2 * DAY);
    s.sweep().await.unwrap();
    assert_eq!(view(&s, "brn").await, frozen);
    assert_consistent(&s, &backend).await;
    // Billing: every key the parent changed was pushed down, and is the branch's own now.
    let b = s.bucket("brn").unwrap();
    assert_eq!((b.objects, b.bytes), (frozen.len() as u64, frozen.iter().map(|v| v.1.len() as u64).sum()));
    s.delete_bucket("brn", true).await.unwrap();
    s.delete_bucket("main", true).await.unwrap();
    clock.advance(2 * DAY);
    s.sweep().await.unwrap();
    assert!(blobs(&backend).await.is_empty());
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn deleting_a_branch_frees_only_its_references() {
    let (backend, clock, s) = env("delbranch").await;
    s.create_bucket("main", BucketOptions { recover_days: Some(0), ..Default::default() }).await.unwrap();
    put(&s, "main", "shared", b"v1").await;
    put(&s, "main", "kept", b"k").await;
    s.branch_bucket("main", "brn").await.unwrap();
    put(&s, "main", "shared", b"v2").await;
    put(&s, "brn", "own", b"o").await;
    s.delete_object("brn", "kept", None).await.unwrap();
    assert_consistent(&s, &backend).await;
    s.delete_bucket("brn", true).await.unwrap();
    assert_consistent(&s, &backend).await;
    clock.advance(2 * DAY);
    let st = s.sweep().await.unwrap();
    assert_eq!(st.blobs_deleted, 2, "v1 (only the branch had it) and the branch's own object");
    assert_eq!(read(&s, "main", "shared").await.unwrap(), b"v2");
    assert_eq!(read(&s, "main", "kept").await.unwrap(), b"k");
    assert_eq!(s.bucket("main").unwrap().branches, Vec::<String>::new());
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn a_tombstone_hides_what_its_ancestors_have() {
    let (backend, _c, s) = env("tombstone").await;
    s.create_bucket("main", BucketOptions::default()).await.unwrap();
    put(&s, "main", "k", b"1").await;
    put(&s, "main", "dir/a", b"1").await;
    s.branch_bucket("main", "child").await.unwrap();
    s.delete_object("child", "k", None).await.unwrap();
    s.delete_object("child", "dir/a", None).await.unwrap();
    s.branch_bucket("child", "grandchild").await.unwrap();
    put(&s, "main", "k", b"2").await;
    for b in ["child", "grandchild"] {
        assert!(matches!(s.get_object(b, "k"), Err(StoreError::NoSuchKey(_))), "{b}");
        assert!(keys(&s, b).is_empty(), "{b}");
        assert_eq!(list(&s, b, "", Some("/")), (vec![], vec![]), "{b}");
        assert!(!s.delete_object(b, "k", None).await.unwrap(), "nothing to delete in {b}");
    }
    // Writing over a tombstone, then deleting again.
    put(&s, "grandchild", "k", b"3").await;
    assert_eq!(read(&s, "grandchild", "k").await.unwrap(), b"3");
    assert!(read(&s, "child", "k").await.is_none());
    s.delete_object("grandchild", "k", None).await.unwrap();
    assert!(read(&s, "grandchild", "k").await.is_none());
    assert_eq!(read(&s, "main", "k").await.unwrap(), b"2");
    // An empty view (tombstones only) can be deleted without force, its parents untouched.
    s.delete_bucket("grandchild", false).await.unwrap();
    s.delete_bucket("child", false).await.unwrap();
    assert_eq!(keys(&s, "main"), ["dir/a", "k"]);
    assert_consistent(&s, &backend).await;
}
