//! Bucket configuration and per-object ACLs and website redirects: durable, recovered from
//! object storage alone, and carried through copies, branch push-down, the trash and multipart.

mod common;

use bytes::Bytes;
use common::*;
use s3_store::config::kind;
use s3_store::*;

const ACL: &str = "{\"grants\":[{\"grantee\":{\"type\":\"group\",\"uri\":\"http://acs.amazonaws.com/groups/global/AllUsers\"},\"permission\":\"READ\"}]}";

async fn put_acl(s: &Store, bucket: &str, key: &str, data: &[u8], acl: Option<&str>, redirect: Option<&str>) -> ObjectMeta {
    let b = s.bucket(bucket).unwrap();
    let blob = s.new_blob();
    let up = s.put_blob(&blob, &b.id, key, bytes_body(Bytes::copy_from_slice(data)), data.len() as u64, None).await.unwrap();
    let headers = ObjectHeaders { acl: acl.map(str::to_string), website_redirect: redirect.map(str::to_string), ..Default::default() };
    s.commit_put(bucket, key, NewObject { blob_id: blob, size: up.size, crc32c: up.crc32c, etag: md5_hex(data), headers, checksum: None }, &WriteCondition::none()).await.unwrap()
}

fn extras(s: &Store, bucket: &str, key: &str) -> (Option<String>, Option<String>) {
    let h = s.get_object(bucket, key).unwrap().headers;
    (h.acl, h.website_redirect)
}

#[tokio::test]
async fn configuration_is_recovered_from_object_storage() {
    let (backend, clock, s) = env("config-recover").await;
    s.create_bucket("conf", BucketOptions::default()).await.unwrap();
    let owner = s.owner_id();
    assert_eq!(owner.len(), 64);
    s.set_bucket_tags("conf", &[("team".to_string(), "storage".to_string())].into()).await.unwrap();
    s.set_bucket_config("conf", kind::POLICY, "", Some("{\"Statement\":[]}"), None).await.unwrap();
    let website = WebsiteConfig { index_document: Some("index.html".into()), ..Default::default() };
    s.set_bucket_config("conf", kind::WEBSITE, "", Some(&serde_json::to_string(&website).unwrap()), None).await.unwrap();
    for i in 0..3 {
        s.set_bucket_config("conf", kind::INVENTORY, &format!("inv{i}"), Some("<InventoryConfiguration/>"), Some(1000)).await.unwrap();
    }
    put_acl(&s, "conf", "k", b"v", Some(ACL), Some("/elsewhere")).await;
    // The node dies; a new one rebuilds from object storage alone.
    let s2 = open(&backend, &clock).await;
    assert_eq!(s2.owner_id(), owner, "the owner id is part of the metadata");
    assert_eq!(s2.bucket_tags("conf").unwrap().get("team").map(String::as_str), Some("storage"));
    assert_eq!(s2.bucket_config("conf", kind::POLICY, "").unwrap().as_deref(), Some("{\"Statement\":[]}"));
    assert_eq!(s2.bucket_website("conf").unwrap(), Some(website.clone()));
    assert_eq!(s2.list_bucket_config("conf", kind::INVENTORY, None, 10).unwrap().len(), 3);
    assert_eq!(s2.list_bucket_config("conf", kind::INVENTORY, Some("inv0"), 1).unwrap()[0].0, "inv1");
    assert_eq!(extras(&s2, "conf", "k"), (Some(ACL.into()), Some("/elsewhere".into())));
    // Through a snapshot too, and removals stick.
    s2.set_bucket_config("conf", kind::POLICY, "", None, None).await.unwrap();
    s2.snapshot_now().await.unwrap();
    s2.set_bucket_tags("conf", &Default::default()).await.unwrap();
    let s3 = open(&backend, &clock).await;
    assert!(s3.bucket_config("conf", kind::POLICY, "").unwrap().is_none());
    assert!(s3.bucket_tags("conf").unwrap().is_empty());
    assert_eq!(s3.bucket_website("conf").unwrap(), Some(website));
    assert_eq!(s3.owner_id(), owner);
    drop((s, s2));
}

#[tokio::test]
async fn configuration_limits_branches_and_deletes() {
    let (_b, _c, s) = env("config-limits").await;
    s.create_bucket("lim", BucketOptions::default()).await.unwrap();
    s.set_bucket_config("lim", kind::METRICS, "a", Some("1"), Some(2)).await.unwrap();
    s.set_bucket_config("lim", kind::METRICS, "b", Some("2"), Some(2)).await.unwrap();
    s.set_bucket_config("lim", kind::METRICS, "b", Some("2b"), Some(2)).await.unwrap();
    assert!(matches!(s.set_bucket_config("lim", kind::METRICS, "c", Some("3"), Some(2)).await, Err(StoreError::InvalidArgument(_))));
    assert!(matches!(s.set_bucket_config("nope", kind::METRICS, "c", Some("3"), None).await, Err(StoreError::NoSuchBucket(_))));
    assert!(matches!(s.set_bucket_tags("lim", &[("aws:x".to_string(), "y".to_string())].into()).await, Err(StoreError::InvalidArgument(_))));
    s.set_bucket_tags("lim", &[("k".to_string(), "v".to_string())].into()).await.unwrap();
    s.set_bucket_config("lim", kind::POLICY, "", Some("{\"Statement\":[]}"), None).await.unwrap();
    // A branch starts with its parent's configuration (but its policy, which names the parent),
    // then has its own.
    s.branch_bucket("lim", "lim-branch").await.unwrap();
    assert_eq!(s.bucket_tags("lim-branch").unwrap().len(), 1);
    assert!(s.bucket_config("lim-branch", kind::POLICY, "").unwrap().is_none());
    assert_eq!(s.list_bucket_config("lim-branch", kind::METRICS, None, 10).unwrap(), vec![("a".to_string(), "1".to_string()), ("b".to_string(), "2b".to_string())]);
    s.set_bucket_tags("lim-branch", &Default::default()).await.unwrap();
    assert_eq!(s.bucket_tags("lim").unwrap().len(), 1);
    // A deleted bucket's configuration goes with it.
    s.delete_bucket("lim-branch", false).await.unwrap();
    s.create_bucket("lim-branch", BucketOptions::default()).await.unwrap();
    assert!(s.list_bucket_config("lim-branch", kind::METRICS, None, 10).unwrap().is_empty());
    let settings = s.bucket_settings(&s.bucket("lim").unwrap().id).unwrap();
    assert!(settings.contains_key(kind::TAGGING) && !settings.contains_key(kind::METRICS), "only one-per-bucket settings");
}

#[tokio::test]
async fn public_flag_respects_the_public_access_block() {
    let (_b, _c, s) = env("config-pab").await;
    s.create_bucket("pub", BucketOptions::default()).await.unwrap();
    let id = s.bucket("pub").unwrap().id;
    assert_eq!(s.public_access_block(&id).unwrap(), (None, PublicAccessBlock::default()));
    let block = PublicAccessBlock { block_public_policy: true, ..Default::default() };
    s.set_bucket_config("pub", kind::PUBLIC_ACCESS_BLOCK, "", Some(&serde_json::to_string(&block).unwrap()), None).await.unwrap();
    assert_eq!(s.public_access_block(&id).unwrap(), (Some(block), block));
    assert!(matches!(s.update_bucket("pub", BucketUpdate { public: Some(true), ..Default::default() }).await, Err(StoreError::InvalidArgument(_))));
    assert!(!s.bucket("pub").unwrap().public);
    s.update_bucket("pub", BucketUpdate { public: Some(false), ..Default::default() }).await.unwrap();
    s.set_bucket_config("pub", kind::PUBLIC_ACCESS_BLOCK, "", None, None).await.unwrap();
    s.update_bucket("pub", BucketUpdate { public: Some(true), ..Default::default() }).await.unwrap();
}

#[tokio::test]
async fn object_acls_survive_copies_branches_and_the_trash() {
    let (backend, clock, s) = env("config-acl").await;
    s.create_bucket("src", BucketOptions::default()).await.unwrap();
    put_acl(&s, "src", "k", b"v1", Some(ACL), Some("/r")).await;
    // A copy keeps them unless told otherwise (the S3 API always tells: see s3-server).
    s.copy_object("src", "k", "src", "copy", &CopyOptions::default()).await.unwrap();
    assert_eq!(extras(&s, "src", "copy"), (Some(ACL.into()), Some("/r".into())));
    let src = s.get_object("src", "k").unwrap();
    let replaced = CopyOptions { headers: Some(ObjectHeaders { acl: None, website_redirect: None, ..src.headers.clone() }), ..Default::default() };
    s.copy_object("src", "k", "src", "plain", &replaced).await.unwrap();
    assert_eq!(extras(&s, "src", "plain"), (None, None));
    // Tagging rewrites the row in place and keeps them.
    s.put_tags("src", "k", [("a".to_string(), "b".to_string())].into()).await.unwrap();
    assert_eq!(extras(&s, "src", "k"), (Some(ACL.into()), Some("/r".into())));
    // A branch's frozen view keeps the old ACL when the parent's changes or is overwritten.
    s.branch_bucket("src", "snap").await.unwrap();
    s.set_object_acl("src", "k", None).await.unwrap();
    assert_eq!(extras(&s, "src", "k"), (None, Some("/r".into())));
    assert_eq!(extras(&s, "snap", "k"), (Some(ACL.into()), Some("/r".into())));
    put_acl(&s, "src", "copy", b"v2", None, None).await;
    assert_eq!(extras(&s, "snap", "copy"), (Some(ACL.into()), Some("/r".into())), "pushed down with its ACL");
    // Delete, then restore from the trash: the ACL comes back with the version.
    s.delete_object("snap", "copy", None).await.unwrap();
    let t = s.list_trash("snap", "copy", None, 10).unwrap().0;
    s.restore("snap", "copy", t[0].deleted_at).await.unwrap();
    assert_eq!(extras(&s, "snap", "copy"), (Some(ACL.into()), Some("/r".into())));
    // Multipart uploads carry them from Create to Complete.
    let headers = ObjectHeaders { acl: Some(ACL.into()), website_redirect: Some("https://example.com/".into()), ..Default::default() };
    let up = s.create_upload("src", "mpu", headers, None, None).await.unwrap();
    assert_eq!(s.upload("src", &up.id).unwrap().headers.acl.as_deref(), Some(ACL));
    let d = s.put_part_data(&up, 1, bytes_body(Bytes::from_static(b"part")), 4).await.unwrap();
    s.commit_part(&up, 1, &d, &md5_hex(b"part"), None).await.unwrap();
    s.complete_upload("src", &up.id, &[(1, md5_hex(b"part"))], None, &WriteCondition::none()).await.unwrap();
    assert_eq!(extras(&s, "src", "mpu"), (Some(ACL.into()), Some("https://example.com/".into())));
    // All of it from object storage alone.
    let s2 = open(&backend, &clock).await;
    assert_eq!(extras(&s2, "snap", "k"), (Some(ACL.into()), Some("/r".into())));
    assert_eq!(extras(&s2, "src", "k"), (None, Some("/r".into())));
    assert_eq!(extras(&s2, "src", "mpu"), (Some(ACL.into()), Some("https://example.com/".into())));
    drop(s);
}
