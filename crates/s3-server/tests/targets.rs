//! Replication to buckets outside roll: a second roll server stands in for another
//! deployment (an S3-compatible endpoint), configured as a replication target. The bytes are
//! streamed (single PUT and multipart), checksums verified, metadata, tags, delete markers and
//! existing objects replicated, statuses and metrics kept.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{BucketVersioningStatus, DeleteMarkerReplication, DeleteMarkerReplicationStatus, Destination, ExistingObjectReplication, ExistingObjectReplicationStatus, ReplicationConfiguration, ReplicationRule, ReplicationRuleFilter, ReplicationRuleStatus, VersioningConfiguration};
use common::*;
use s3_store::TargetSpec;

async fn versioned(c: &aws_sdk_s3::Client, name: &str) {
    c.create_bucket().bucket(name).send().await.unwrap();
    c.put_bucket_versioning().bucket(name).versioning_configuration(VersioningConfiguration::builder().status(BucketVersioningStatus::Enabled).build()).send().await.unwrap();
}

fn rule(id: &str, dest: &str, existing: bool) -> ReplicationRule {
    let mut r = ReplicationRule::builder()
        .id(id)
        .priority(1)
        .status(ReplicationRuleStatus::Enabled)
        .filter(ReplicationRuleFilter::builder().prefix("").build())
        .delete_marker_replication(DeleteMarkerReplication::builder().status(DeleteMarkerReplicationStatus::Enabled).build())
        .destination(Destination::builder().bucket(format!("arn:aws:s3:::{dest}")).storage_class(aws_sdk_s3::types::StorageClass::StandardIa).build().unwrap());
    if existing {
        r = r.existing_object_replication(ExistingObjectReplication::builder().status(ExistingObjectReplicationStatus::Enabled).build().unwrap());
    }
    r.build().unwrap()
}

/// Two servers: `a` replicates to `dst` on `b`, a target named `other-roll`.
async fn pair() -> (TestServer, std::sync::Arc<s3_server::Server>, TestServer) {
    let (a, sa) = start_full(|_| {}, |c| c.notify.dispatch_interval = None).await;
    let b = start().await;
    versioned(&b.client, "dst").await;
    let spec = TargetSpec { endpoint: Some(b.endpoint.clone()), region: Some(REGION.into()), access_key_id: Some(ACCESS.into()), secret_access_key: Some(s3_store::Secret::new(SECRET)), path_style: Some(true), buckets: vec!["dst".into()] };
    let t = a.store.put_replication_target("other-roll", &spec, true).await.unwrap();
    assert!(t.secret.is_none(), "never handed back");
    (a, sa, b)
}

#[tokio::test(flavor = "multi_thread")]
async fn to_another_roll() {
    let (a, sa, b) = pair().await;
    versioned(&a.client, "src").await;
    a.client.put_object().bucket("src").key("before").body(ByteStream::from_static(b"already here")).send().await.unwrap();
    // Small parts, so multipart is exercised.
    sa.notifier.set_replication_part_size(5 << 20, 6 << 20);
    let cfg = ReplicationConfiguration::builder().role("arn:aws:iam::1:role/r").rules(rule("all", "dst", true)).build().unwrap();
    a.client.put_bucket_replication().bucket("src").replication_configuration(cfg).send().await.unwrap();
    let got = a.client.get_bucket_replication().bucket("src").send().await.unwrap();
    assert_eq!(got.replication_configuration().unwrap().rules()[0].destination().unwrap().bucket(), "arn:aws:s3:::dst");
    let stored = a.store.replication("src").unwrap().unwrap();
    assert_eq!(stored.rules[0].target.as_deref(), Some("other-roll"), "the target that lists the bucket");

    let small = a
        .client
        .put_object()
        .bucket("src")
        .key("dir/small.txt")
        .content_type("text/plain")
        .cache_control("max-age=60")
        .metadata("team", "storage")
        .tagging("k=v&a=b")
        .body(ByteStream::from_static(b"hello target"))
        .send()
        .await
        .unwrap();
    let big = data((12 << 20) + 17, 5);
    let up = a.client.create_multipart_upload().bucket("src").key("big.bin").content_type("application/x-big").send().await.unwrap();
    let id = up.upload_id().unwrap();
    let mut parts = Vec::new();
    for (i, chunk) in big.chunks(5 << 20).enumerate() {
        let n = i as i32 + 1;
        let p = a.client.upload_part().bucket("src").key("big.bin").upload_id(id).part_number(n).body(ByteStream::from(chunk.to_vec())).send().await.unwrap();
        parts.push(aws_sdk_s3::types::CompletedPart::builder().part_number(n).e_tag(p.e_tag().unwrap()).build());
    }
    a.client.complete_multipart_upload().bucket("src").key("big.bin").upload_id(id).multipart_upload(aws_sdk_s3::types::CompletedMultipartUpload::builder().set_parts(Some(parts)).build()).send().await.unwrap();
    let head = a.client.head_object().bucket("src").key("dir/small.txt").version_id(small.version_id().unwrap()).send().await.unwrap();
    assert_eq!(head.replication_status().map(|s| s.as_str()), Some("PENDING"));
    let src_bucket = a.store.bucket("src").unwrap();
    let pending = a.store.replication_backlog(&src_bucket.id).unwrap();
    let p = &pending["dst@other-roll"];
    assert!(p.operations >= 2 && p.bytes >= big.len() as u64, "{p:?}");

    a.store.flush_meter().unwrap();
    sa.dispatch_events().await.unwrap();
    // Backfill queues the existing version; one more pass replicates it.
    sa.dispatch_events().await.unwrap();

    let s = b.client.get_object().bucket("dst").key("dir/small.txt").send().await.unwrap();
    assert_eq!(s.content_type(), Some("text/plain"));
    assert_eq!(s.cache_control(), Some("max-age=60"));
    assert_eq!(s.metadata().unwrap().get("team").map(String::as_str), Some("storage"));
    assert_eq!(s.body.collect().await.unwrap().into_bytes().as_ref(), b"hello target");
    let tags = b.client.get_object_tagging().bucket("dst").key("dir/small.txt").send().await.unwrap();
    assert_eq!(tags.tag_set().len(), 2);
    let g = b.client.get_object().bucket("dst").key("big.bin").send().await.unwrap();
    assert_eq!(g.content_type(), Some("application/x-big"));
    assert!(g.e_tag().unwrap().ends_with("-3\""), "multipart on the target: {:?}", g.e_tag());
    assert_eq!(g.body.collect().await.unwrap().into_bytes().as_ref(), big.as_slice());
    assert_eq!(b.client.get_object().bucket("dst").key("before").send().await.unwrap().body.collect().await.unwrap().into_bytes().as_ref(), b"already here");

    let head = a.client.head_object().bucket("src").key("dir/small.txt").send().await.unwrap();
    assert_eq!(head.replication_status().map(|s| s.as_str()), Some("COMPLETED"));
    assert!(a.store.replication_backlog(&src_bucket.id).unwrap().is_empty());
    let latency = sa.notifier.replication_latency(&src_bucket.id);
    assert!(latency["dst@other-roll"].replicated >= 3, "{latency:?}");
    assert_eq!(sa.notifier.server_side_copies(), 0, "another service: streamed");

    // Delete markers replicate as deletes.
    a.client.delete_object().bucket("src").key("dir/small.txt").send().await.unwrap();
    sa.dispatch_events().await.unwrap();
    let e = b.client.head_object().bucket("dst").key("dir/small.txt").send().await.unwrap_err();
    assert_eq!(e.raw_response().map(|r| r.status().as_u16()), Some(404));
    let versions = b.client.list_object_versions().bucket("dst").prefix("dir/").send().await.unwrap();
    assert_eq!(versions.delete_markers().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn refusals_and_failures() {
    let (a, sa, b) = pair().await;
    versioned(&a.client, "src2").await;
    // A target's bucket without versioning is refused, as S3 refuses one.
    b.client.create_bucket().bucket("plain").send().await.unwrap();
    let mut spec = TargetSpec { endpoint: Some(b.endpoint.clone()), access_key_id: Some(ACCESS.into()), secret_access_key: Some(s3_store::Secret::new(SECRET)), buckets: vec!["plain".into(), "gone".into()], ..Default::default() };
    a.store.put_replication_target("second", &spec, true).await.unwrap();
    let cfg = ReplicationConfiguration::builder().role("r").rules(rule("p", "plain", false)).build().unwrap();
    let e = a.client.put_bucket_replication().bucket("src2").replication_configuration(cfg).send().await.unwrap_err();
    assert_eq!(e.as_service_error().and_then(|s| aws_sdk_s3::error::ProvideErrorMetadata::code(s)), Some("InvalidRequest"));
    // Neither a roll bucket nor a target's.
    let cfg = ReplicationConfiguration::builder().role("r").rules(rule("p", "nowhere", false)).build().unwrap();
    let e = a.client.put_bucket_replication().bucket("src2").replication_configuration(cfg).send().await.unwrap_err();
    assert_eq!(e.as_service_error().and_then(|s| aws_sdk_s3::error::ProvideErrorMetadata::code(s)), Some("InvalidRequest"));

    // A destination that goes away after the configuration: FAILED, with the event.
    versioned(&b.client, "gone").await;
    let cfg = ReplicationConfiguration::builder().role("r").rules(rule("g", "gone", false)).build().unwrap();
    a.client.put_bucket_replication().bucket("src2").replication_configuration(cfg).send().await.unwrap();
    b.client.delete_bucket().bucket("gone").send().await.unwrap();
    let put = a.client.put_object().bucket("src2").key("k").body(ByteStream::from_static(b"x")).send().await.unwrap();
    sa.dispatch_events().await.unwrap();
    let head = a.client.head_object().bucket("src2").key("k").version_id(put.version_id().unwrap()).send().await.unwrap();
    assert_eq!(head.replication_status().map(|s| s.as_str()), Some("FAILED"));

    // A target in use can't be deleted; its secret is kept when it's updated without one.
    assert!(a.store.delete_replication_target("second").await.is_err());
    spec.secret_access_key = None;
    spec.buckets = vec!["gone".into()];
    a.store.put_replication_target("second", &spec, false).await.unwrap();
    assert!(a.store.replication_target("second").unwrap().unwrap().secret.is_some());
    a.client.delete_bucket_replication().bucket("src2").send().await.unwrap();
    a.store.delete_replication_target("second").await.unwrap();
}
