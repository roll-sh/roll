//! Versioning, Object Lock and the full lifecycle model through the AWS SDK (and raw signed
//! requests for what the SDK can't be made to send).

mod common;

use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::primitives::{ByteStream, DateTime};
use aws_sdk_s3::types::{
    BucketLifecycleConfiguration, BucketVersioningStatus, CompletedMultipartUpload, CompletedPart, DefaultRetention, Delete, ExpirationStatus, LifecycleExpiration, LifecycleRule, LifecycleRuleAndOperator,
    LifecycleRuleFilter, NoncurrentVersionExpiration, NoncurrentVersionTransition, ObjectIdentifier, ObjectLockConfiguration, ObjectLockEnabled, ObjectLockLegalHold, ObjectLockLegalHoldStatus, ObjectLockMode,
    ObjectLockRetention, ObjectLockRetentionMode, ObjectLockRule, Tag, Tagging, Transition, TransitionStorageClass, VersioningConfiguration,
};
use common::*;
use s3_store::{BucketUpdate, Permission};
use std::time::{Duration, SystemTime};

fn code<E: ProvideErrorMetadata + std::fmt::Debug>(e: &aws_sdk_s3::error::SdkError<E, aws_sdk_s3::config::http::HttpResponse>) -> String {
    e.as_service_error().and_then(|s| s.code()).map(str::to_string).unwrap_or_else(|| format!("{e:?}"))
}

fn status<E>(e: &aws_sdk_s3::error::SdkError<E, aws_sdk_s3::config::http::HttpResponse>) -> u16 {
    e.raw_response().map(|r| r.status().as_u16()).unwrap_or_default()
}

fn header<E>(e: &aws_sdk_s3::error::SdkError<E, aws_sdk_s3::config::http::HttpResponse>, name: &str) -> Option<String> {
    e.raw_response().and_then(|r| r.headers().get(name)).map(str::to_string)
}

async fn enable(t: &TestServer, b: &str, status: BucketVersioningStatus) {
    t.client.put_bucket_versioning().bucket(b).versioning_configuration(VersioningConfiguration::builder().status(status).build()).send().await.unwrap();
}

async fn put(t: &TestServer, b: &str, k: &str, body: &[u8]) -> String {
    t.client.put_object().bucket(b).key(k).body(ByteStream::from(body.to_vec())).send().await.unwrap().version_id().unwrap_or("").to_string()
}

async fn get_version(t: &TestServer, b: &str, k: &str, v: &str) -> Vec<u8> {
    t.client.get_object().bucket(b).key(k).version_id(v).send().await.unwrap().body.collect().await.unwrap().into_bytes().to_vec()
}

/// Every (key, version id, latest, delete marker), `page` at a time through the markers.
async fn list_all(t: &TestServer, b: &str, prefix: &str, page: i32) -> Vec<(String, String, bool, bool)> {
    let mut out = Vec::new();
    let (mut km, mut vm): (Option<String>, Option<String>) = (None, None);
    loop {
        let r = t.client.list_object_versions().bucket(b).prefix(prefix).max_keys(page).set_key_marker(km.clone()).set_version_id_marker(vm.clone()).send().await.unwrap();
        let mut entries: Vec<(String, String, bool, bool)> = r.versions().iter().map(|v| (v.key().unwrap().to_string(), v.version_id().unwrap().to_string(), v.is_latest().unwrap(), false)).collect();
        entries.extend(r.delete_markers().iter().map(|d| (d.key().unwrap().to_string(), d.version_id().unwrap().to_string(), d.is_latest().unwrap(), true)));
        assert!(entries.len() <= page as usize);
        // The SDK splits versions and markers; put them back in S3's order (key, newest first).
        entries.sort_by(|a, b| a.0.cmp(&b.0).then(b.2.cmp(&a.2)).then(a.1.cmp(&b.1)));
        out.extend(entries);
        if !r.is_truncated().unwrap_or(false) {
            return out;
        }
        km = r.next_key_marker().map(str::to_string);
        vm = r.next_version_id_marker().map(str::to_string);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn versioning_through_the_sdk() {
    let t = start().await;
    t.client.create_bucket().bucket("ver").send().await.unwrap();
    assert_eq!(t.client.get_bucket_versioning().bucket("ver").send().await.unwrap().status(), None, "never versioned");
    assert_eq!(put(&t, "ver", "plain", b"p").await, "", "no version id outside versioned buckets");
    enable(&t, "ver", BucketVersioningStatus::Enabled).await;
    assert_eq!(t.client.get_bucket_versioning().bucket("ver").send().await.unwrap().status(), Some(&BucketVersioningStatus::Enabled));
    let v1 = put(&t, "ver", "k", b"one").await;
    let v2 = put(&t, "ver", "k", b"two").await;
    assert!(!v1.is_empty() && v1 != v2 && v2 < v1, "newer versions sort first");
    assert_eq!(get_version(&t, "ver", "k", &v1).await, b"one");
    let h = t.client.head_object().bucket("ver").key("k").send().await.unwrap();
    assert_eq!(h.version_id(), Some(v2.as_str()));
    let h = t.client.head_object().bucket("ver").key("k").version_id(&v1).send().await.unwrap();
    assert_eq!((h.version_id(), h.content_length()), (Some(v1.as_str()), Some(3)));
    assert_eq!(t.client.get_object().bucket("ver").key("plain").send().await.unwrap().version_id(), Some("null"), "objects from before versioning are the null version");

    // Delete markers.
    let d = t.client.delete_object().bucket("ver").key("k").send().await.unwrap();
    assert_eq!(d.delete_marker(), Some(true));
    let marker = d.version_id().unwrap().to_string();
    let e = t.client.get_object().bucket("ver").key("k").send().await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (404, "NoSuchKey".into()));
    assert_eq!(header(&e, "x-amz-delete-marker").as_deref(), Some("true"));
    assert_eq!(header(&e, "x-amz-version-id"), Some(marker.clone()));
    let e = t.client.head_object().bucket("ver").key("k").send().await.unwrap_err();
    assert_eq!((status(&e), header(&e, "x-amz-delete-marker").as_deref()), (404, Some("true")));
    let e = t.client.get_object().bucket("ver").key("k").version_id(&marker).send().await.unwrap_err();
    assert_eq!((status(&e), header(&e, "x-amz-delete-marker").as_deref(), header(&e, "x-amz-version-id")), (405, Some("true"), Some(marker.clone())));
    assert!(t.client.list_objects_v2().bucket("ver").send().await.unwrap().contents().iter().all(|o| o.key() != Some("k")));
    // Deleting the marker brings the last version back.
    let d = t.client.delete_object().bucket("ver").key("k").version_id(&marker).send().await.unwrap();
    assert_eq!((d.delete_marker(), d.version_id()), (Some(true), Some(marker.as_str())));
    assert_eq!(get_version(&t, "ver", "k", &v2).await, b"two");
    assert_eq!(t.client.get_object().bucket("ver").key("k").send().await.unwrap().version_id(), Some(v2.as_str()));
    // A permanent delete: to the trash.
    let d = t.client.delete_object().bucket("ver").key("k").version_id(&v2).send().await.unwrap();
    assert_eq!((d.delete_marker(), d.version_id()), (None, Some(v2.as_str())));
    assert_eq!(t.client.get_object().bucket("ver").key("k").send().await.unwrap().version_id(), Some(v1.as_str()));
    assert_eq!(t.store.list_trash("ver", "", None, 10).unwrap().0[0].version_id, v2);
    let e = t.client.get_object().bucket("ver").key("k").version_id(&v2).send().await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (404, "NoSuchVersion".into()));
    t.client.delete_object().bucket("ver").key("k").version_id("0000000000000000aaaaaaaaaaaaaaaa").send().await.unwrap();
    assert_eq!(status(&t.client.get_object().bucket("ver").key("k").version_id("bad id").send().await.unwrap_err()), 400);

    // Copies name their source version.
    let v3 = put(&t, "ver", "k", b"three").await;
    let c = t.client.copy_object().bucket("ver").key("copy").copy_source(format!("ver/k?versionId={v1}")).send().await.unwrap();
    assert_eq!(c.copy_source_version_id(), Some(v1.as_str()));
    assert!(c.version_id().is_some());
    assert_eq!(get_version(&t, "ver", "copy", c.version_id().unwrap()).await, b"one");
    let c = t.client.copy_object().bucket("ver").key("copy2").copy_source("ver/k").send().await.unwrap();
    assert_eq!(c.copy_source_version_id(), Some(v3.as_str()));
    // Restoring an old version the S3 way: copy it onto its own key.
    t.client.copy_object().bucket("ver").key("k").copy_source(format!("ver/k?versionId={v1}")).send().await.unwrap();
    assert_eq!(t.client.get_object().bucket("ver").key("k").send().await.unwrap().body.collect().await.unwrap().into_bytes().as_ref(), b"one");
    let m = t.client.delete_object().bucket("ver").key("gone").send().await.unwrap().version_id().unwrap().to_string();
    let e = t.client.copy_object().bucket("ver").key("x").copy_source(format!("ver/gone?versionId={m}")).send().await.unwrap_err();
    assert_eq!(code(&e), "InvalidRequest", "a copy can't name a delete marker");
    assert_eq!(code(&t.client.copy_object().bucket("ver").key("x").copy_source("ver/gone").send().await.unwrap_err()), "NoSuchKey");
    // UploadPartCopy from a version.
    let up = t.client.create_multipart_upload().bucket("ver").key("mp").send().await.unwrap().upload_id().unwrap().to_string();
    let pc = t.client.upload_part_copy().bucket("ver").key("mp").upload_id(&up).part_number(1).copy_source(format!("ver/k?versionId={v3}")).send().await.unwrap();
    assert_eq!(pc.copy_source_version_id(), Some(v3.as_str()));
    let etag = pc.copy_part_result().unwrap().e_tag().unwrap().to_string();
    let done = t
        .client
        .complete_multipart_upload()
        .bucket("ver")
        .key("mp")
        .upload_id(&up)
        .multipart_upload(CompletedMultipartUpload::builder().parts(CompletedPart::builder().part_number(1).e_tag(etag).build()).build())
        .send()
        .await
        .unwrap();
    assert!(done.version_id().is_some(), "Complete returns the version");
    assert_eq!(get_version(&t, "ver", "mp", done.version_id().unwrap()).await, b"three");

    // Tags per version.
    let tags = Tagging::builder().tag_set(Tag::builder().key("v").value("1").build().unwrap()).build().unwrap();
    let r = t.client.put_object_tagging().bucket("ver").key("k").version_id(&v1).tagging(tags).send().await.unwrap();
    assert_eq!(r.version_id(), Some(v1.as_str()));
    assert_eq!(t.client.get_object_tagging().bucket("ver").key("k").version_id(&v1).send().await.unwrap().tag_set().len(), 1);
    assert!(t.client.get_object_tagging().bucket("ver").key("k").send().await.unwrap().tag_set().is_empty());
    t.client.delete_object_tagging().bucket("ver").key("k").version_id(&v1).send().await.unwrap();
    assert!(t.client.get_object_tagging().bucket("ver").key("k").version_id(&v1).send().await.unwrap().tag_set().is_empty());
    assert_eq!(status(&t.client.get_object_tagging().bucket("ver").key("gone").version_id(&m).send().await.unwrap_err()), 405);

    // DeleteObjects with versions.
    let ids = vec![
        ObjectIdentifier::builder().key("k").version_id(&v1).build().unwrap(),
        ObjectIdentifier::builder().key("copy2").build().unwrap(),
        ObjectIdentifier::builder().key("gone").version_id(&m).build().unwrap(),
        ObjectIdentifier::builder().key("bad").version_id("not a version").build().unwrap(),
    ];
    let r = t.client.delete_objects().bucket("ver").delete(Delete::builder().set_objects(Some(ids)).build().unwrap()).send().await.unwrap();
    let by_key = |k: &str| r.deleted().iter().find(|d| d.key() == Some(k)).unwrap();
    assert_eq!(by_key("k").version_id(), Some(v1.as_str()));
    assert_eq!((by_key("copy2").delete_marker(), by_key("copy2").delete_marker_version_id().is_some()), (Some(true), true));
    assert_eq!((by_key("gone").delete_marker(), by_key("gone").delete_marker_version_id()), (Some(true), Some(m.as_str())));
    assert_eq!(r.errors().len(), 1);
    assert_eq!((r.errors()[0].key(), r.errors()[0].code()), (Some("bad"), Some("InvalidArgument")));
    assert!(matches!(t.client.head_object().bucket("ver").key("gone").send().await, Err(e) if status(&e) == 404 && header(&e, "x-amz-delete-marker").is_none()), "its only version is gone");

    // Suspended: null versions.
    enable(&t, "ver", BucketVersioningStatus::Suspended).await;
    assert_eq!(t.client.get_bucket_versioning().bucket("ver").send().await.unwrap().status(), Some(&BucketVersioningStatus::Suspended));
    assert_eq!(put(&t, "ver", "k", b"null").await, "null");
    assert_eq!(get_version(&t, "ver", "k", "null").await, b"null");
    assert_eq!(t.client.delete_object().bucket("ver").key("k").send().await.unwrap().version_id(), Some("null"));

    // Non-admin keys can't change versioning.
    let w = t.store.create_key("w", Permission::Write, None).await.unwrap();
    let wc = client(&t.endpoint, &w.id, &w.secret);
    let e = wc.put_bucket_versioning().bucket("ver").versioning_configuration(VersioningConfiguration::builder().status(BucketVersioningStatus::Enabled).build()).send().await.unwrap_err();
    assert_eq!(code(&e), "AccessDenied");
    // A bucket with versions isn't empty.
    assert_eq!(code(&t.client.delete_bucket().bucket("ver").send().await.unwrap_err()), "BucketNotEmpty");
}

#[tokio::test(flavor = "multi_thread")]
async fn listing_object_versions() {
    let t = start().await;
    t.client.create_bucket().bucket("lst").send().await.unwrap();
    put(&t, "lst", "n", b"null").await;
    enable(&t, "lst", BucketVersioningStatus::Enabled).await;
    for (k, n) in [("a", 3), ("b/1", 1), ("b/2", 2), ("c d", 2), ("n", 1)] {
        for i in 0..n {
            put(&t, "lst", k, format!("{k}{i}").as_bytes()).await;
        }
    }
    t.client.delete_object().bucket("lst").key("b/2").send().await.unwrap();
    let all = list_all(&t, "lst", "", 1000).await;
    assert_eq!(all.len(), 11);
    assert_eq!(all.iter().filter(|e| e.2).count(), 5);
    assert_eq!(all.iter().filter(|e| e.3).count(), 1);
    for page in [1, 2, 3, 5] {
        assert_eq!(list_all(&t, "lst", "", page).await, all, "page size {page}");
    }
    let r = t.client.list_object_versions().bucket("lst").delimiter("/").send().await.unwrap();
    assert_eq!(r.common_prefixes().iter().map(|p| p.prefix().unwrap()).collect::<Vec<_>>(), ["b/"]);
    assert_eq!(r.versions().len(), 3 + 2 + 2);
    assert!(r.delete_markers().is_empty());
    let r = t.client.list_object_versions().bucket("lst").prefix("b/").send().await.unwrap();
    assert_eq!((r.versions().len(), r.delete_markers().len()), (3, 1));
    let r = t.client.list_object_versions().bucket("lst").max_keys(2).send().await.unwrap();
    assert_eq!((r.is_truncated(), r.next_key_marker(), r.max_keys()), (Some(true), Some("a"), Some(2)));
    assert!(r.next_version_id_marker().is_some());
    let r = t.client.list_object_versions().bucket("lst").key_marker("n").version_id_marker("null").send().await.unwrap();
    assert!(r.versions().is_empty() && r.is_truncated() == Some(false), "nothing after n's null version");
    let r = t.client.list_object_versions().bucket("lst").max_keys(0).send().await.unwrap();
    assert!(r.versions().is_empty() && r.is_truncated() == Some(false));
    let raw = |q: &'static str| {
        let mut r = Raw::new("GET", "/lst");
        r.query = q;
        r
    };
    let body = raw("encoding-type=url&prefix=c&versions").send(&t).await.text().await.unwrap();
    assert!(body.contains("<Key>c+d</Key>") && body.contains("<EncodingType>url</EncodingType>"), "{body}");
    // Errors.
    assert_eq!(status_code(raw("versions&version-id-marker=null").send(&t).await).await, (400, "InvalidArgument".into()), "a version-id marker needs a key marker");
    assert_eq!(status_code(raw("versions&key-marker=a&version-id-marker=nope").send(&t).await).await, (400, "InvalidArgument".into()));
    assert_eq!(status_code(raw("versions&max-keys=-1").send(&t).await).await, (400, "InvalidArgument".into()));
    assert_eq!(status_code(raw("versions&encoding-type=base64").send(&t).await).await, (400, "InvalidArgument".into()));
    // Read keys may list; keys scoped elsewhere may not.
    let r = t.store.create_key("r", Permission::Read, Some(vec!["lst".into()])).await.unwrap();
    assert_eq!(client(&t.endpoint, &r.id, &r.secret).list_object_versions().bucket("lst").send().await.unwrap().versions().len(), 10);
    let o = t.store.create_key("o", Permission::Admin, Some(vec!["other".into()])).await.unwrap();
    assert_eq!(code(&client(&t.endpoint, &o.id, &o.secret).list_object_versions().bucket("lst").send().await.unwrap_err()), "AccessDenied");
}

fn days_from_now(days: u64) -> DateTime {
    DateTime::from(SystemTime::now() + Duration::from_secs(days * 86_400))
}

#[tokio::test(flavor = "multi_thread")]
async fn object_lock_through_the_sdk() {
    let t = start().await;
    t.client.create_bucket().bucket("lck").object_lock_enabled_for_bucket(true).send().await.unwrap();
    assert_eq!(t.client.get_bucket_versioning().bucket("lck").send().await.unwrap().status(), Some(&BucketVersioningStatus::Enabled));
    let c = t.client.get_object_lock_configuration().bucket("lck").send().await.unwrap();
    let c = c.object_lock_configuration().unwrap();
    assert_eq!((c.object_lock_enabled(), c.rule()), (Some(&ObjectLockEnabled::Enabled), None));
    let e = t.client.put_bucket_versioning().bucket("lck").versioning_configuration(VersioningConfiguration::builder().status(BucketVersioningStatus::Suspended).build()).send().await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (409, "InvalidBucketState".into()));
    // Buckets without it.
    t.client.create_bucket().bucket("open").send().await.unwrap();
    assert_eq!(code(&t.client.get_object_lock_configuration().bucket("open").send().await.unwrap_err()), "ObjectLockConfigurationNotFoundError");
    let cfg = |mode: ObjectLockRetentionMode, days: i32| {
        ObjectLockConfiguration::builder().object_lock_enabled(ObjectLockEnabled::Enabled).rule(ObjectLockRule::builder().default_retention(DefaultRetention::builder().mode(mode).days(days).build()).build()).build()
    };
    let e = t.client.put_object_lock_configuration().bucket("open").object_lock_configuration(cfg(ObjectLockRetentionMode::Governance, 1)).send().await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (409, "InvalidBucketState".into()));
    let e = t.client.put_object().bucket("open").key("k").object_lock_legal_hold_status(ObjectLockLegalHoldStatus::On).body(ByteStream::from_static(b"x")).send().await.unwrap_err();
    assert_eq!(code(&e), "InvalidRequest");
    assert_eq!(code(&t.client.get_object_retention().bucket("open").key("k").send().await.unwrap_err()), "InvalidRequest");

    // An object from before a default retention has none.
    let plain = put(&t, "lck", "plain", b"p").await;
    assert_eq!(code(&t.client.get_object_retention().bucket("lck").key("plain").send().await.unwrap_err()), "NoSuchObjectLockConfiguration");
    assert_eq!(code(&t.client.get_object_legal_hold().bucket("lck").key("plain").send().await.unwrap_err()), "NoSuchObjectLockConfiguration");
    t.client.delete_object().bucket("lck").key("plain").version_id(&plain).send().await.unwrap();

    // Explicit lock headers.
    let until = days_from_now(2);
    let p = t
        .client
        .put_object()
        .bucket("lck")
        .key("c")
        .object_lock_mode(ObjectLockMode::Compliance)
        .object_lock_retain_until_date(until)
        .object_lock_legal_hold_status(ObjectLockLegalHoldStatus::On)
        .body(ByteStream::from_static(b"locked"))
        .send()
        .await
        .unwrap();
    let cv = p.version_id().unwrap().to_string();
    let h = t.client.head_object().bucket("lck").key("c").send().await.unwrap();
    assert_eq!((h.object_lock_mode(), h.object_lock_legal_hold_status()), (Some(&ObjectLockMode::Compliance), Some(&ObjectLockLegalHoldStatus::On)));
    assert_eq!(h.object_lock_retain_until_date().unwrap().secs(), until.secs());
    let g = t.client.get_object().bucket("lck").key("c").send().await.unwrap();
    assert_eq!(g.object_lock_mode(), Some(&ObjectLockMode::Compliance));
    let r = t.client.get_object_retention().bucket("lck").key("c").send().await.unwrap();
    assert_eq!(r.retention().unwrap().mode(), Some(&ObjectLockRetentionMode::Compliance));
    assert_eq!(t.client.get_object_legal_hold().bucket("lck").key("c").version_id(&cv).send().await.unwrap().legal_hold().unwrap().status(), Some(&ObjectLockLegalHoldStatus::On));
    // A lock without Content-MD5 or a checksum is refused, as on S3.
    let mut raw = Raw::new("PUT", "/lck/raw");
    raw.headers = vec![("x-amz-object-lock-mode", "GOVERNANCE".into()), ("x-amz-object-lock-retain-until-date", "2099-01-01T00:00:00Z".into())];
    raw.body = b"x".to_vec();
    assert_eq!(status_code(raw.send(&t).await).await, (400, "InvalidRequest".into()));
    let mut raw = Raw::new("PUT", "/lck/raw");
    raw.headers = vec![("x-amz-object-lock-mode", "GOVERNANCE".into()), ("x-amz-object-lock-retain-until-date", "2001-01-01T00:00:00Z".into()), ("content-md5", s3_proto::checksum::b64(&s3_proto::checksum::md5(b"x")))];
    raw.body = b"x".to_vec();
    assert_eq!(status_code(raw.send(&t).await).await, (400, "InvalidArgument".into()), "a date in the past");

    // Locked versions can't be deleted; a delete marker is fine.
    let e = t.client.delete_object().bucket("lck").key("c").version_id(&cv).send().await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (403, "AccessDenied".into()));
    t.client.put_object_legal_hold().bucket("lck").key("c").version_id(&cv).legal_hold(ObjectLockLegalHold::builder().status(ObjectLockLegalHoldStatus::Off).build()).send().await.unwrap();
    assert_eq!(code(&t.client.delete_object().bucket("lck").key("c").version_id(&cv).bypass_governance_retention(true).send().await.unwrap_err()), "AccessDenied", "COMPLIANCE can't be bypassed");
    assert_eq!(t.client.delete_object().bucket("lck").key("c").send().await.unwrap().delete_marker(), Some(true));
    let shorter = ObjectLockRetention::builder().mode(ObjectLockRetentionMode::Compliance).retain_until_date(days_from_now(1)).build();
    assert_eq!(code(&t.client.put_object_retention().bucket("lck").key("c").version_id(&cv).retention(shorter).send().await.unwrap_err()), "AccessDenied");
    let longer = ObjectLockRetention::builder().mode(ObjectLockRetentionMode::Compliance).retain_until_date(days_from_now(3)).build();
    t.client.put_object_retention().bucket("lck").key("c").version_id(&cv).retention(longer).send().await.unwrap();

    // Default retention, and the governance bypass (admin keys only).
    t.client.put_object_lock_configuration().bucket("lck").object_lock_configuration(cfg(ObjectLockRetentionMode::Governance, 1)).send().await.unwrap();
    let c = t.client.get_object_lock_configuration().bucket("lck").send().await.unwrap();
    let rule = c.object_lock_configuration().unwrap().rule().unwrap().default_retention().unwrap();
    assert_eq!((rule.mode(), rule.days(), rule.years()), (Some(&ObjectLockRetentionMode::Governance), Some(1), None));
    let gv = put(&t, "lck", "g", b"gov").await;
    assert_eq!(t.client.head_object().bucket("lck").key("g").send().await.unwrap().object_lock_mode(), Some(&ObjectLockMode::Governance));
    assert_eq!(code(&t.client.delete_object().bucket("lck").key("g").version_id(&gv).send().await.unwrap_err()), "AccessDenied");
    let w = t.store.create_key("w", Permission::Write, None).await.unwrap();
    let wc = client(&t.endpoint, &w.id, &w.secret);
    assert_eq!(code(&wc.delete_object().bucket("lck").key("g").version_id(&gv).bypass_governance_retention(true).send().await.unwrap_err()), "AccessDenied");
    t.client.delete_object().bucket("lck").key("g").version_id(&gv).bypass_governance_retention(true).send().await.unwrap();
    assert_eq!(t.store.list_trash("lck", "g", None, 10).unwrap().0.len(), 1, "even a bypassed delete is recoverable");

    // Multipart and copies take lock headers too.
    let up = t.client.create_multipart_upload().bucket("lck").key("mp").object_lock_mode(ObjectLockMode::Compliance).object_lock_retain_until_date(days_from_now(1)).send().await.unwrap();
    let id = up.upload_id().unwrap().to_string();
    let part = t.client.upload_part().bucket("lck").key("mp").upload_id(&id).part_number(1).body(ByteStream::from_static(b"mp")).send().await.unwrap();
    t.client
        .complete_multipart_upload()
        .bucket("lck")
        .key("mp")
        .upload_id(&id)
        .multipart_upload(CompletedMultipartUpload::builder().parts(CompletedPart::builder().part_number(1).e_tag(part.e_tag().unwrap()).build()).build())
        .send()
        .await
        .unwrap();
    assert_eq!(t.client.head_object().bucket("lck").key("mp").send().await.unwrap().object_lock_mode(), Some(&ObjectLockMode::Compliance));
    let cp = t.client.copy_object().bucket("lck").key("cp").copy_source("lck/mp").object_lock_legal_hold_status(ObjectLockLegalHoldStatus::On).send().await.unwrap();
    let h = t.client.head_object().bucket("lck").key("cp").version_id(cp.version_id().unwrap()).send().await.unwrap();
    assert_eq!((h.object_lock_legal_hold_status(), h.object_lock_mode()), (Some(&ObjectLockLegalHoldStatus::On), Some(&ObjectLockMode::Governance)), "a copy gets its own lock, never the source's");

    // Buckets holding locked versions stay.
    assert_eq!(code(&t.client.delete_bucket().bucket("lck").send().await.unwrap_err()), "BucketNotEmpty");
    let e = t.store.delete_bucket("lck", true).await.unwrap_err();
    assert!(matches!(e, s3_store::StoreError::ObjectLocked(_)), "{e}");
}

/// POST Object (browser form) with Object Lock fields, into a versioned bucket.
#[tokio::test(flavor = "multi_thread")]
async fn post_object_with_lock_fields() {
    let t = start().await;
    t.client.create_bucket().bucket("frm").object_lock_enabled_for_bucket(true).send().await.unwrap();
    let amz_date = s3_proto::time::amz_date(now_s());
    let date = &amz_date[..8];
    let credential = format!("{ACCESS}/{date}/{REGION}/s3/aws4_request");
    let until = s3_proto::time::iso8601_ms((now_s() + 86_400) * 1000);
    let policy = serde_json::json!({
        "expiration": s3_proto::time::iso8601_ms((now_s() + 600) * 1000),
        "conditions": [
            {"bucket": "frm"},
            ["starts-with", "$key", ""],
            ["starts-with", "$x-amz-object-lock-mode", ""],
            ["starts-with", "$x-amz-object-lock-retain-until-date", ""],
            {"x-amz-algorithm": "AWS4-HMAC-SHA256"},
            {"x-amz-credential": credential},
            {"x-amz-date": amz_date},
        ]
    });
    let policy_b64 = s3_proto::checksum::b64(policy.to_string().as_bytes());
    let signature = s3_proto::sigv4::sign(&s3_proto::sigv4::signing_key(SECRET, date, REGION, "s3"), &policy_b64);
    let send = |mode: &str| {
        let fields = [
            ("key", "form.txt".to_string()),
            ("x-amz-object-lock-mode", mode.to_string()),
            ("x-amz-object-lock-retain-until-date", until.clone()),
            ("x-amz-algorithm", "AWS4-HMAC-SHA256".to_string()),
            ("x-amz-credential", credential.clone()),
            ("x-amz-date", amz_date.clone()),
            ("policy", policy_b64.clone()),
            ("x-amz-signature", signature.clone()),
        ];
        let mut body = Vec::new();
        for (k, v) in &fields {
            body.extend_from_slice(format!("--b\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
        }
        body.extend_from_slice(b"--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\n\r\nform bytes\r\n--b--\r\n");
        reqwest::Client::new().post(format!("{}/frm", t.endpoint)).header("content-type", "multipart/form-data; boundary=b").body(body).send()
    };
    let r = send("COMPLIANCE").await.unwrap();
    assert_eq!(r.status(), 204);
    let version = r.headers()["x-amz-version-id"].to_str().unwrap().to_string();
    let h = t.client.head_object().bucket("frm").key("form.txt").send().await.unwrap();
    assert_eq!((h.version_id(), h.object_lock_mode()), (Some(version.as_str()), Some(&ObjectLockMode::Compliance)));
    assert_eq!(status_code(send("FOREVER").await.unwrap()).await, (400, "InvalidArgument".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn lifecycle_full_model_through_the_sdk() {
    let t = start().await;
    t.client.create_bucket().bucket("lcy").send().await.unwrap();
    enable(&t, "lcy", BucketVersioningStatus::Enabled).await;
    let and = LifecycleRuleAndOperator::builder().prefix("logs/").tags(Tag::builder().key("tier").value("tmp").build().unwrap()).object_size_greater_than(10).object_size_less_than(1_000_000).build();
    let rules = vec![
        LifecycleRule::builder()
            .id("and")
            .status(ExpirationStatus::Enabled)
            .filter(LifecycleRuleFilter::builder().and(and).build())
            .expiration(LifecycleExpiration::builder().days(30).build())
            .transitions(Transition::builder().days(10).storage_class(TransitionStorageClass::Glacier).build())
            .noncurrent_version_transitions(NoncurrentVersionTransition::builder().noncurrent_days(5).storage_class(TransitionStorageClass::DeepArchive).build())
            .noncurrent_version_expiration(NoncurrentVersionExpiration::builder().noncurrent_days(7).newer_noncurrent_versions(2).build())
            .build()
            .unwrap(),
        LifecycleRule::builder()
            .id("markers")
            .status(ExpirationStatus::Enabled)
            .filter(LifecycleRuleFilter::builder().prefix("m/").build())
            .expiration(LifecycleExpiration::builder().expired_object_delete_marker(true).build())
            .build()
            .unwrap(),
        LifecycleRule::builder()
            .id("date")
            .status(ExpirationStatus::Enabled)
            .filter(LifecycleRuleFilter::builder().tag(Tag::builder().key("expire").value("now").build().unwrap()).build())
            .expiration(LifecycleExpiration::builder().date(DateTime::from_secs(1_577_836_800)).build())
            .build()
            .unwrap(),
    ];
    t.client.put_bucket_lifecycle_configuration().bucket("lcy").lifecycle_configuration(BucketLifecycleConfiguration::builder().set_rules(Some(rules)).build().unwrap()).send().await.unwrap();
    let got = t.client.get_bucket_lifecycle_configuration().bucket("lcy").send().await.unwrap();
    let r = &got.rules()[0];
    let and = r.filter().unwrap().and().unwrap();
    assert_eq!((and.prefix(), and.tags().len(), and.object_size_greater_than(), and.object_size_less_than()), (Some("logs/"), 1, Some(10), Some(1_000_000)));
    assert_eq!(r.transitions()[0].storage_class(), Some(&TransitionStorageClass::Glacier));
    assert_eq!(r.noncurrent_version_transitions()[0].noncurrent_days(), Some(5));
    let nce = r.noncurrent_version_expiration().unwrap();
    assert_eq!((nce.noncurrent_days(), nce.newer_noncurrent_versions()), (Some(7), Some(2)));
    assert_eq!(got.rules()[1].expiration().unwrap().expired_object_delete_marker(), Some(true));
    assert_eq!(got.rules()[2].expiration().unwrap().date().unwrap().secs(), 1_577_836_800);
    let stored = &t.store.bucket("lcy").unwrap().lifecycle;
    assert_eq!((stored[0].noncurrent_days, stored[0].transitions.len(), stored[2].expire_at), (Some(7), 1, Some(1_577_836_800_000)));

    // A past date expires every matching current version: here, a delete marker each.
    t.client.put_object().bucket("lcy").key("old").tagging("expire=now").body(ByteStream::from_static(b"x")).send().await.unwrap();
    t.client.put_object().bucket("lcy").key("keep").body(ByteStream::from_static(b"x")).send().await.unwrap();
    // Transitions never change the storage class anything reports.
    t.client.put_object().bucket("lcy").key("logs/a").tagging("tier=tmp").body(ByteStream::from(vec![0u8; 100])).send().await.unwrap();
    assert_eq!(t.store.run_lifecycle().await.unwrap(), 1);
    let v = list_all(&t, "lcy", "", 1000).await;
    assert!(v.iter().any(|e| e.0 == "old" && e.2 && e.3), "{v:?}");
    assert!(v.iter().any(|e| e.0 == "keep" && e.2 && !e.3));
    let l = t.client.list_object_versions().bucket("lcy").prefix("logs/").send().await.unwrap();
    assert_eq!(l.versions()[0].storage_class(), Some(&aws_sdk_s3::types::ObjectVersionStorageClass::Standard));
    assert_eq!(t.client.head_object().bucket("lcy").key("logs/a").send().await.unwrap().storage_class(), None);

    // The legacy `<Rule><Prefix>` form still parses, and stays in that form.
    let mut raw = Raw::new("PUT", "/lcy");
    raw.query = "lifecycle";
    raw.body = b"<LifecycleConfiguration><Rule><ID>legacy</ID><Prefix>tmp/</Prefix><Status>Enabled</Status><Expiration><Days>1</Days></Expiration></Rule></LifecycleConfiguration>".to_vec();
    raw.headers = vec![("content-md5", s3_proto::checksum::b64(&s3_proto::checksum::md5(&raw.body)))];
    assert_eq!(raw.send(&t).await.status(), 200);
    let mut raw = Raw::new("GET", "/lcy");
    raw.query = "lifecycle";
    let body = raw.send(&t).await.text().await.unwrap();
    assert!(body.contains("<Rule><ID>legacy</ID><Prefix>tmp/</Prefix><Status>Enabled</Status>"), "{body}");
    // S3's validation.
    let bad = LifecycleRule::builder()
        .status(ExpirationStatus::Enabled)
        .filter(LifecycleRuleFilter::builder().tag(Tag::builder().key("a").value("b").build().unwrap()).build())
        .expiration(LifecycleExpiration::builder().expired_object_delete_marker(true).build())
        .build()
        .unwrap();
    let e = t.client.put_bucket_lifecycle_configuration().bucket("lcy").lifecycle_configuration(BucketLifecycleConfiguration::builder().rules(bad).build().unwrap()).send().await.unwrap_err();
    assert_eq!(code(&e), "InvalidRequest");
    t.store.update_bucket("lcy", BucketUpdate { lifecycle: Some(vec![]), ..Default::default() }).await.unwrap();
}
