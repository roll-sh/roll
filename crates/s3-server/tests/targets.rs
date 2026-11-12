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
    let big_done = a.client.complete_multipart_upload().bucket("src").key("big.bin").upload_id(id).multipart_upload(aws_sdk_s3::types::CompletedMultipartUpload::builder().set_parts(Some(parts)).build()).send().await.unwrap();
    let head = a.client.head_object().bucket("src").key("dir/small.txt").version_id(small.version_id().unwrap()).send().await.unwrap();
    assert_eq!(head.replication_status().map(|s| s.as_str()), Some("PENDING"));
    let src_bucket = a.store.bucket("src").unwrap();
    let pending = a.store.replication_backlog(&src_bucket.id).unwrap();
    let p = &pending["all"];
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
    let metrics = sa.notifier.replication_metrics(&src_bucket.id);
    assert!(metrics["all"].replicated >= 3, "{metrics:?}");
    assert_eq!(sa.notifier.server_side_copies(), 0, "another service: streamed");

    // Another roll deployment keeps the source's version ids and Last-Modified (roll's
    // replication extension), as S3's replication does, and the replicas are REPLICAs.
    let svid = small.version_id().unwrap();
    let replica = b.client.head_object().bucket("dst").key("dir/small.txt").send().await.unwrap();
    assert_eq!(replica.version_id(), Some(svid));
    assert_eq!(replica.replication_status().map(|s| s.as_str()), Some("REPLICA"));
    let src_meta = a.store.get_object_version("src", "dir/small.txt", Some(svid)).unwrap();
    let dst_meta = b.store.get_object_version("dst", "dir/small.txt", Some(svid)).unwrap();
    assert_eq!(dst_meta.modified_at, src_meta.modified_at, "Last-Modified to the millisecond");
    assert_eq!(dst_meta.etag, src_meta.etag);
    let gb = b.client.head_object().bucket("dst").key("big.bin").send().await.unwrap();
    assert_eq!(gb.version_id(), big_done.version_id());
    assert_eq!(gb.e_tag(), big_done.e_tag(), "part for part: the source's multipart ETag");
    let found = a.store.replica_versions(&src_bucket.id, "dir/small.txt", svid).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!((found[0].bucket.as_str(), found[0].target.as_str(), found[0].version_id.as_deref()), ("dst", "other-roll", Some(svid)));

    // Tags changed on the source go to the replica (on the same version).
    a.client
        .put_object_tagging()
        .bucket("src")
        .key("dir/small.txt")
        .version_id(svid)
        .tagging(aws_sdk_s3::types::Tagging::builder().tag_set(aws_sdk_s3::types::Tag::builder().key("new").value("1").build().unwrap()).build().unwrap())
        .send()
        .await
        .unwrap();
    sa.dispatch_events().await.unwrap();
    let tags = b.client.get_object_tagging().bucket("dst").key("dir/small.txt").version_id(svid).send().await.unwrap();
    assert_eq!(tags.tag_set().iter().map(|t| t.key()).collect::<Vec<_>>(), ["new"]);
    assert_eq!(b.store.events_backlog().unwrap(), 0);

    // Delete markers replicate as deletes, with the source marker's version id.
    let marker = a.client.delete_object().bucket("src").key("dir/small.txt").send().await.unwrap();
    sa.dispatch_events().await.unwrap();
    let e = b.client.head_object().bucket("dst").key("dir/small.txt").send().await.unwrap_err();
    assert_eq!(e.raw_response().map(|r| r.status().as_u16()), Some(404));
    let versions = b.client.list_object_versions().bucket("dst").prefix("dir/").send().await.unwrap();
    assert_eq!(versions.delete_markers().len(), 1);
    assert_eq!(versions.delete_markers()[0].version_id(), marker.version_id());
}

/// A PUT carrying roll's replication extension (as `a`'s replication sends it to `b`).
async fn replica_put(c: &aws_sdk_s3::Client, bucket: &str, key: &str, vid: &str, at: i64, body: &'static [u8]) -> Result<aws_sdk_s3::operation::put_object::PutObjectOutput, (u16, String)> {
    let (v, m) = (vid.to_string(), at.to_string());
    c.put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from_static(body))
        .customize()
        .mutate_request(move |r| {
            r.headers_mut().insert("x-roll-replica-version-id", v.clone());
            r.headers_mut().insert("x-roll-replica-last-modified", m.clone());
        })
        .send()
        .await
        .map_err(|e| (e.raw_response().map_or(0, |r| r.status().as_u16()), aws_sdk_s3::error::ProvideErrorMetadata::code(&e).unwrap_or_default().to_string()))
}

/// The receiving side of roll's replication extension: only identities that may replicate,
/// only into versioned buckets; a retry of the same replica answers as the first did, another
/// one with the same version id is refused; version ids keep their order.
#[tokio::test(flavor = "multi_thread")]
async fn replica_writes_are_checked() {
    let b = start().await;
    versioned(&b.client, "dst").await;
    b.client.create_bucket().bucket("plain").send().await.unwrap();
    let older = s3_store::version_id(now_s() * 1000 - 60_000);
    let newer = s3_store::version_id(now_s() * 1000 - 1_000);
    let at = now_s() * 1000 - 60_000;
    // A write key can't rewrite history.
    let w = b.store.create_key("writer", s3_store::Permission::Write, None).await.unwrap();
    let wc = client(&b.endpoint, &w.id, &w.secret);
    assert_eq!(replica_put(&wc, "dst", "k", &older, at, b"one").await.unwrap_err(), (403, "AccessDenied".into()));
    // Not into an unversioned bucket; not with a malformed version id.
    assert_eq!(replica_put(&b.client, "plain", "k", &older, at, b"one").await.unwrap_err(), (400, "InvalidRequest".into()));
    assert_eq!(replica_put(&b.client, "dst", "k", "nope", at, b"one").await.unwrap_err(), (400, "InvalidArgument".into()));
    // The newer one first, then the older: they keep their order by version id.
    let n = replica_put(&b.client, "dst", "k", &newer, at + 59_000, b"two").await.unwrap();
    assert_eq!(n.version_id(), Some(newer.as_str()));
    assert_eq!(b.client.head_object().bucket("dst").key("k").send().await.unwrap().replication_status().map(|s| s.as_str()), Some("REPLICA"));
    let o = replica_put(&b.client, "dst", "k", &older, at, b"one").await.unwrap();
    assert_eq!(o.version_id(), Some(older.as_str()));
    let cur = b.client.get_object().bucket("dst").key("k").send().await.unwrap();
    assert_eq!(cur.version_id(), Some(newer.as_str()), "the newer version stays current");
    let vs = b.client.list_object_versions().bucket("dst").send().await.unwrap();
    assert_eq!(vs.versions().iter().map(|v| v.version_id().unwrap()).collect::<Vec<_>>(), [newer.as_str(), older.as_str()]);
    assert_eq!(b.store.get_object_version("dst", "k", Some(&older)).unwrap().modified_at, at);
    // A retry (same content) is answered again; other content under that id is refused.
    let again = replica_put(&b.client, "dst", "k", &older, at, b"one").await.unwrap();
    assert_eq!(again.version_id(), Some(older.as_str()));
    assert_eq!(b.client.list_object_versions().bucket("dst").send().await.unwrap().versions().len(), 2);
    assert_eq!(replica_put(&b.client, "dst", "k", &older, at, b"other").await.unwrap_err(), (400, "InvalidRequest".into()));
    // A bucket policy granting s3:ReplicateObject lets the write key replicate (as a
    // replication role's permission does).
    let policy = format!(r#"{{"Version":"2012-10-17","Statement":[{{"Effect":"Allow","Principal":{{"AWS":"{}"}},"Action":["s3:ReplicateObject","s3:ReplicateDelete"],"Resource":"arn:aws:s3:::dst/*"}}]}}"#, w.id);
    b.client.put_bucket_policy().bucket("dst").policy(policy).send().await.unwrap();
    let third = s3_store::version_id(now_s() * 1000 - 30_000);
    assert_eq!(replica_put(&wc, "dst", "k2", &third, at, b"three").await.unwrap().version_id(), Some(third.as_str()));
}

/// Object Lock replicates to another roll deployment: it's checked when the configuration is
/// put (the target bucket must have Object Lock), and retention and legal hold are on the
/// replica, then kept in sync.
#[tokio::test(flavor = "multi_thread")]
async fn object_lock_to_another_roll() {
    use aws_sdk_s3::types::{ObjectLockLegalHold, ObjectLockLegalHoldStatus, ObjectLockMode, ObjectLockRetentionMode};
    let (a, sa, b) = pair().await;
    b.client.create_bucket().bucket("locked").object_lock_enabled_for_bucket(true).send().await.unwrap();
    let spec = TargetSpec { endpoint: Some(b.endpoint.clone()), region: Some(REGION.into()), access_key_id: Some(ACCESS.into()), secret_access_key: Some(s3_store::Secret::new(SECRET)), path_style: Some(true), buckets: vec!["locked".into()] };
    a.store.put_replication_target("lock-roll", &spec, true).await.unwrap();
    a.client.create_bucket().bucket("lsrc").object_lock_enabled_for_bucket(true).send().await.unwrap();
    let e = a.client.put_bucket_replication().bucket("lsrc").replication_configuration(ReplicationConfiguration::builder().role("r").rules(rule("l", "dst", false)).build().unwrap()).send().await.unwrap_err();
    assert_eq!(e.as_service_error().and_then(|s| aws_sdk_s3::error::ProvideErrorMetadata::code(s)), Some("InvalidRequest"), "dst has no Object Lock");
    a.client.put_bucket_replication().bucket("lsrc").replication_configuration(ReplicationConfiguration::builder().role("r").rules(rule("l", "locked", false)).build().unwrap()).send().await.unwrap();
    let until = aws_sdk_s3::primitives::DateTime::from_secs(now_s() + 86_400);
    let v = a
        .client
        .put_object()
        .bucket("lsrc")
        .key("held")
        .object_lock_mode(ObjectLockMode::Governance)
        .object_lock_retain_until_date(until)
        .object_lock_legal_hold_status(ObjectLockLegalHoldStatus::On)
        .checksum_algorithm(aws_sdk_s3::types::ChecksumAlgorithm::Crc32)
        .body(ByteStream::from_static(b"locked"))
        .send()
        .await
        .unwrap();
    sa.dispatch_events().await.unwrap();
    let vid = v.version_id().unwrap();
    let r = b.client.get_object_retention().bucket("locked").key("held").version_id(vid).send().await.unwrap();
    assert_eq!(r.retention().unwrap().mode(), Some(&ObjectLockRetentionMode::Governance));
    assert_eq!(r.retention().unwrap().retain_until_date().map(|d| d.secs()), Some(until.secs()));
    let h = b.client.get_object_legal_hold().bucket("locked").key("held").version_id(vid).send().await.unwrap();
    assert_eq!(h.legal_hold().unwrap().status(), Some(&ObjectLockLegalHoldStatus::On));
    // A locked replica can't be deleted on the target.
    assert!(b.client.delete_object().bucket("locked").key("held").version_id(vid).send().await.is_err());
    a.client.put_object_legal_hold().bucket("lsrc").key("held").version_id(vid).legal_hold(ObjectLockLegalHold::builder().status(ObjectLockLegalHoldStatus::Off).build()).send().await.unwrap();
    let later = aws_sdk_s3::primitives::DateTime::from_secs(now_s() + 2 * 86_400);
    a.client
        .put_object_retention()
        .bucket("lsrc")
        .key("held")
        .version_id(vid)
        .retention(aws_sdk_s3::types::ObjectLockRetention::builder().mode(ObjectLockRetentionMode::Governance).retain_until_date(later).build())
        .send()
        .await
        .unwrap();
    sa.dispatch_events().await.unwrap();
    let h = b.client.get_object_legal_hold().bucket("locked").key("held").version_id(vid).send().await.unwrap();
    assert_eq!(h.legal_hold().unwrap().status(), Some(&ObjectLockLegalHoldStatus::Off));
    let r = b.client.get_object_retention().bucket("locked").key("held").version_id(vid).send().await.unwrap();
    assert_eq!(r.retention().unwrap().retain_until_date().map(|d| d.secs()), Some(later.secs()), "extended on the replica too");
}

/// Replication is prompt (S3's Replication Time Control SLA is 15 minutes; roll's dispatcher
/// looks every 250 ms): the latency from each write to its replica on another roll deployment,
/// measured and reported.
#[tokio::test(flavor = "multi_thread")]
async fn replication_latency_is_measured() {
    let (a, sa) = start_full(|_| {}, |_| {}).await;
    let b = start().await;
    versioned(&b.client, "dst").await;
    let spec = TargetSpec { endpoint: Some(b.endpoint.clone()), region: Some(REGION.into()), access_key_id: Some(ACCESS.into()), secret_access_key: Some(s3_store::Secret::new(SECRET)), path_style: Some(true), buckets: vec!["dst".into()] };
    a.store.put_replication_target("other-roll", &spec, true).await.unwrap();
    versioned(&a.client, "src").await;
    a.client.put_bucket_replication().bucket("src").replication_configuration(ReplicationConfiguration::builder().role("r").rules(rule("all", "dst", false)).build().unwrap()).send().await.unwrap();
    let n = 20;
    let mut latencies = Vec::new();
    for i in 0..n {
        let started = std::time::Instant::now();
        let key = format!("k{i}");
        let put = a.client.put_object().bucket("src").key(&key).body(ByteStream::from(data(4096, i as u8))).send().await.unwrap();
        loop {
            if let Ok(h) = b.client.head_object().bucket("dst").key(&key).send().await
                && h.version_id() == put.version_id()
            {
                break;
            }
            assert!(started.elapsed() < std::time::Duration::from_secs(30), "{key} wasn't replicated within 30 s");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        latencies.push(started.elapsed().as_millis() as u64);
    }
    latencies.sort();
    let (p50, max) = (latencies[n / 2], latencies[n - 1]);
    println!("replication latency to another roll deployment over {n} writes: p50 {p50} ms, max {max} ms");
    // Each write replicated within 30 s (checked above): well under S3's 15-minute threshold
    // however loaded the machine is. The figures are printed, not asserted.
    assert!(max < 900_000, "replication must beat the 15-minute threshold: {latencies:?}");
    let src = a.store.bucket("src").unwrap();
    // The source's status lands just after the replica does: wait for it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let m = loop {
        let m = sa.notifier.replication_metrics(&src.id);
        if m.get("all").is_some_and(|r| r.replicated == n as u64) {
            break m;
        }
        assert!(std::time::Instant::now() < deadline, "replication metrics didn't count {n} writes within 30 s: {m:?}");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert!(m["all"].max_ms < 900_000, "{m:?}");
    println!("roll's own ReplicationLatency (write to replica): last {} ms, max {} ms", m["all"].last_ms, m["all"].max_ms);
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
