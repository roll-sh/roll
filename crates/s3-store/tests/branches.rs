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
    let (_b, _c, s) = env("nested").await;
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
}

#[tokio::test]
async fn listing_merges_ancestors() {
    let (_b, _c, s) = env("merge").await;
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
}

#[tokio::test]
async fn branch_trash_and_restore() {
    let (_b, _c, s) = env("brtrash").await;
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
}
