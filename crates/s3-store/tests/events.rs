//! The event outbox: an event is in the outbox if and only if its write committed, it survives
//! restarts and recovery, it's handed out only once durable and in order per key, and
//! replication applies it once, sharing blobs.

mod common;

use common::*;
use s3_store::*;
use std::collections::BTreeMap;

fn queue_target(id: &str, events: &[&str], prefix: Option<&str>) -> NotificationTarget {
    NotificationTarget {
        id: id.into(),
        kind: "queue".into(),
        destination: format!("arn:roll:queues:/:{id}"),
        events: events.iter().map(|e| e.to_string()).collect(),
        prefix: prefix.map(Into::into),
        suffix: None,
    }
}

async fn notify_all(s: &Store, bucket: &str) {
    let mut cfg = NotificationConfig { configurations: vec![queue_target("all", &["s3:ObjectCreated:*", "s3:ObjectRemoved:*", "s3:ObjectTagging:*", "s3:ObjectAcl:Put", "s3:ObjectRestore:*", "s3:LifecycleExpiration:*"], None)], event_bridge: false };
    cfg.validate().unwrap();
    s.set_notifications(bucket, Some(&cfg)).await.unwrap();
}

fn names(s: &Store) -> Vec<(String, String)> {
    s.pending_events(None, 1000).unwrap().iter().filter(|e| e.kind == "notify").map(|e| (e.record().unwrap().name, e.key.clone())).collect()
}

#[tokio::test]
async fn no_configuration_writes_no_events() {
    let (_b, _c, s) = env("ev-none").await;
    s.create_bucket("plain", BucketOptions::default()).await.unwrap();
    put(&s, "plain", "a", b"1").await;
    s.delete_object("plain", "a", None).await.unwrap();
    assert_eq!(s.events_backlog().unwrap(), 0);
}

#[tokio::test]
async fn every_change_records_its_event_in_its_transaction() {
    let (_b, _c, s) = env("ev-kinds").await;
    s.create_bucket("evt", BucketOptions::default()).await.unwrap();
    s.create_bucket("other", BucketOptions::default()).await.unwrap();
    notify_all(&s, "evt").await;
    let m = put(&s, "evt", "a", b"hello").await;
    put(&s, "other", "x", b"x").await;
    s.copy_object("evt", "a", "evt", "b", &CopyOptions::default()).await.unwrap();
    s.put_tags("evt", "a", BTreeMap::from([("k".into(), "v".into())])).await.unwrap();
    s.put_version_tags("evt", "a", None, BTreeMap::new()).await.unwrap();
    s.set_object_acl("evt", "a", None, Some("[]".into())).await.unwrap();
    s.delete_object("evt", "b", None).await.unwrap();
    s.delete_object("evt", "missing", None).await.unwrap();
    let trashed = s.list_trash("evt", "", None, 10).unwrap().0;
    s.restore("evt", "b", trashed[0].deleted_at).await.unwrap();
    let got = names(&s);
    let want: Vec<(String, String)> = [
        ("ObjectCreated:Put", "a"),
        ("ObjectCreated:Copy", "b"),
        ("ObjectTagging:Put", "a"),
        ("ObjectTagging:Delete", "a"),
        ("ObjectAcl:Put", "a"),
        ("ObjectRemoved:Delete", "b"),
        ("ObjectRestore:Completed", "b"),
    ]
    .iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect();
    assert_eq!(got, want, "nothing for the other bucket or the key that wasn't there");
    let all = s.pending_events(None, 100).unwrap();
    let first = all[0].record().unwrap();
    assert_eq!((first.bucket.as_str(), first.key.as_str(), first.size, first.etag.as_deref()), ("evt", "a", Some(5), Some(m.etag.as_str())));
    assert_eq!(first.config_id, "all");
    assert_eq!(first.version_id, None, "unversioned buckets have no version ids in events");
    assert_eq!(all[0].dest, "arn:roll:queues:/:all");
    let seqs: Vec<String> = all.iter().map(|e| e.record().unwrap().sequencer).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "sequencers only grow: {seqs:?}");
    let removed = all.iter().find(|e| e.record().unwrap().name == "ObjectRemoved:Delete").unwrap().record().unwrap();
    assert_eq!((removed.size, removed.etag), (None, None), "removals carry no size or ETag");
}

#[tokio::test]
async fn filters_decide_what_is_recorded() {
    let (_b, _c, s) = env("ev-filter").await;
    s.create_bucket("flt", BucketOptions::default()).await.unwrap();
    let mut cfg = NotificationConfig {
        configurations: vec![
            NotificationTarget { suffix: Some(".jpg".into()), ..queue_target("img", &["s3:ObjectCreated:Put"], Some("img/")) },
            queue_target("rm", &["s3:ObjectRemoved:*"], Some("img/")),
        ],
        event_bridge: true,
    };
    cfg.validate().unwrap();
    s.set_notifications("flt", Some(&cfg)).await.unwrap();
    put(&s, "flt", "img/a.jpg", b"1").await;
    put(&s, "flt", "img/a.png", b"1").await;
    put(&s, "flt", "doc/a.jpg", b"1").await;
    s.delete_object("flt", "img/a.jpg", None).await.unwrap();
    let rows = s.pending_events(None, 100).unwrap();
    let got: Vec<(String, String, String)> = rows.iter().map(|e| (e.dest.clone(), e.record().unwrap().name, e.key.clone())).collect();
    assert_eq!(
        got,
        [
            ("arn:roll:queues:/:img", "ObjectCreated:Put", "img/a.jpg"),
            ("eventbridge", "ObjectCreated:Put", "img/a.jpg"),
            ("eventbridge", "ObjectCreated:Put", "img/a.png"),
            ("eventbridge", "ObjectCreated:Put", "doc/a.jpg"),
            ("arn:roll:queues:/:rm", "ObjectRemoved:Delete", "img/a.jpg"),
            ("eventbridge", "ObjectRemoved:Delete", "img/a.jpg"),
        ]
        .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
    );
    // One change, two destinations, one sequencer.
    assert_eq!(rows[0].record().unwrap().sequencer, rows[1].record().unwrap().sequencer);
    s.set_notifications("flt", None).await.unwrap();
    put(&s, "flt", "img/b.jpg", b"1").await;
    assert_eq!(s.pending_events(None, 100).unwrap().len(), 6, "removing the configuration stops new events");
}

#[tokio::test]
async fn a_refused_write_records_nothing() {
    let (_b, _c, s) = env("ev-refused").await;
    s.create_bucket("rfz", BucketOptions::default()).await.unwrap();
    notify_all(&s, "rfz").await;
    put(&s, "rfz", "a", b"1").await;
    let cond = WriteCondition { if_none_match: Some("*".into()), ..Default::default() };
    assert!(matches!(put_with(&s, "rfz", "a", b"2", &cond).await, Err(StoreError::PreconditionFailed)));
    assert!(s.delete_object("rfz", "a", Some("\"nope\"")).await.is_err());
    assert_eq!(names(&s).len(), 1, "only the write that committed has an event");
}

#[tokio::test]
async fn a_fenced_commit_records_nothing_and_hands_nothing_out() {
    let (backend, clock, s) = env("ev-fenced").await;
    s.create_bucket("fen", BucketOptions::default()).await.unwrap();
    notify_all(&s, "fen").await;
    put(&s, "fen", "before", b"1").await;
    // Another node takes the metadata log over: this node's next commit fails.
    let s2 = open(&backend, &clock).await;
    let b = s.bucket("fen").unwrap();
    let blob = s.new_blob();
    let up = s.put_blob(&blob, &b.id, "after", bytes_body(bytes::Bytes::from_static(b"2")), 1, None).await.unwrap();
    let obj = NewObject { blob_id: blob, size: up.size, crc32c: up.crc32c, etag: md5_hex(b"2"), headers: ObjectHeaders::default(), checksum: None };
    let none = WriteCondition::none();
    let (w, due) = tokio::join!(s.commit_put("fen", "after", obj, &none), async {
        tokio::task::yield_now().await;
        s.due_events(100).await
    });
    assert!(matches!(w, Err(StoreError::Unavailable(_))), "{w:?}");
    match due {
        Ok(rows) => assert!(rows.iter().all(|e| e.key != "after"), "an unacknowledged write's event was handed out"),
        Err(e) => assert!(matches!(e, StoreError::Unavailable(_)), "{e}"),
    }
    assert!(s.due_events(100).await.is_err(), "a fenced node hands out nothing");
    s2.close().await;
    let s3 = open(&backend, &clock).await;
    let keys: Vec<String> = s3.pending_events(None, 100).unwrap().into_iter().map(|e| e.key).collect();
    assert_eq!(keys, ["before"], "the fenced write's event never existed");
}

#[tokio::test]
async fn events_survive_restart_and_recovery() {
    let (backend, clock, s) = env("ev-recover").await;
    s.create_bucket("rcv", BucketOptions::default()).await.unwrap();
    notify_all(&s, "rcv").await;
    put(&s, "rcv", "a", b"1").await;
    s.snapshot_now().await.unwrap();
    put(&s, "rcv", "b", b"2").await;
    s.close().await;
    // A fresh node with nothing local: the snapshot plus the log after it.
    let s2 = open(&backend, &clock).await;
    assert_eq!(names(&s2), [("ObjectCreated:Put".to_string(), "a".to_string()), ("ObjectCreated:Put".to_string(), "b".to_string())]);
    let before = s2.pending_events(None, 10).unwrap();
    put(&s2, "rcv", "c", b"3").await;
    let after = s2.pending_events(None, 10).unwrap();
    assert!(after[2].record().unwrap().sequencer > before[1].record().unwrap().sequencer, "the sequencer carries on after recovery");
}

#[tokio::test]
async fn rows_go_out_in_order_per_destination_and_key() {
    let (_b, clock, s) = env("ev-order").await;
    s.create_bucket("ord", BucketOptions::default()).await.unwrap();
    notify_all(&s, "ord").await;
    put(&s, "ord", "k", b"1").await;
    put(&s, "ord", "k", b"2").await;
    put(&s, "ord", "j", b"3").await;
    let due = s.due_events(10).await.unwrap();
    let keys: Vec<&str> = due.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(keys, ["k", "j"], "the second event of k waits for the first");
    // A failed delivery is retried later, and still holds back the key's later events.
    s.event_retry(due[0].seq, s.now() + 60_000, "boom").unwrap();
    s.events_done(&[due[1].seq]).unwrap();
    assert!(s.due_events(10).await.unwrap().is_empty());
    clock.advance(61_000);
    let due = s.due_events(10).await.unwrap();
    assert_eq!((due.len(), due[0].attempts, due[0].error.as_deref()), (1, 1, Some("boom")));
    s.events_done(&[due[0].seq]).unwrap();
    let due = s.due_events(10).await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].key, "k");
    s.events_done(&[due[0].seq]).unwrap();
    assert_eq!(s.events_backlog().unwrap(), 0);
}

#[tokio::test]
async fn versioned_buckets_report_version_ids_and_markers() {
    let (_b, clock, s) = env("ev-versions").await;
    s.create_bucket("vbk", BucketOptions::default()).await.unwrap();
    s.update_bucket("vbk", BucketUpdate { versioning: Some("Enabled".into()), ..Default::default() }).await.unwrap();
    notify_all(&s, "vbk").await;
    let v1 = put(&s, "vbk", "k", b"1").await;
    let marker = s.delete_object_version("vbk", "k", None, None, false).await.unwrap();
    s.delete_object_version("vbk", "k", Some(&v1.version_id), None, false).await.unwrap();
    let recs: Vec<EventRecord> = s.pending_events(None, 10).unwrap().iter().map(|e| e.record().unwrap()).collect();
    let got: Vec<(&str, Option<&str>)> = recs.iter().map(|r| (r.name.as_str(), r.version_id.as_deref())).collect();
    assert_eq!(
        got,
        [("ObjectCreated:Put", Some(v1.version_id.as_str())), ("ObjectRemoved:DeleteMarkerCreated", marker.version_id.as_deref()), ("ObjectRemoved:Delete", Some(v1.version_id.as_str()))]
    );
    // Lifecycle: expiring the lone delete marker.
    s.update_bucket("vbk", BucketUpdate { lifecycle: Some(vec![LifecycleRule { id: "m".into(), enabled: true, expire_delete_markers: true, ..Default::default() }]), ..Default::default() }).await.unwrap();
    clock.advance(DAY);
    s.run_lifecycle().await.unwrap();
    let last = s.pending_events(None, 10).unwrap().pop().unwrap().record().unwrap();
    assert_eq!((last.name.as_str(), last.version_id), ("LifecycleExpiration:Delete", marker.version_id));
}

#[tokio::test]
async fn branches_dont_inherit_notifications() {
    let (_b, _c, s) = env("ev-branch").await;
    s.create_bucket("main", BucketOptions::default()).await.unwrap();
    notify_all(&s, "main").await;
    s.branch_bucket("main", "dev").await.unwrap();
    assert!(s.notifications("dev").unwrap().is_none());
    put(&s, "dev", "a", b"1").await;
    put(&s, "main", "a", b"2").await;
    let rows = s.pending_events(None, 10).unwrap();
    assert_eq!(rows.len(), 1, "the parent's write only, not the push-down to the branch");
    assert_eq!(rows[0].record().unwrap().bucket, "main");
}

async fn versioned(s: &Store, name: &str) {
    s.create_bucket(name, BucketOptions::default()).await.unwrap();
    s.update_bucket(name, BucketUpdate { versioning: Some("Enabled".into()), ..Default::default() }).await.unwrap();
}

fn rule(dest: &str, markers: bool) -> ReplicationConfig {
    ReplicationConfig {
        role: "arn:aws:iam::1:role/x".into(),
        rules: vec![ReplicationRule { id: "r".into(), priority: Some(1), enabled: true, filter: Some(RuleFilter::default()), delete_markers: markers, destination: dest.into(), ..Default::default() }],
    }
}

/// Run every due replication row.
async fn replicate_all(s: &Store) -> Vec<Replicated> {
    let mut out = Vec::new();
    loop {
        let due: Vec<PendingEvent> = s.due_events(100).await.unwrap().into_iter().filter(|e| e.kind != "notify").collect();
        if due.is_empty() {
            return out;
        }
        for e in due {
            if e.kind == "backfill" {
                s.backfill(&e).await.unwrap();
            } else {
                out.push(s.replicate(&e).await.unwrap());
            }
        }
    }
}

#[tokio::test]
async fn replication_needs_versioning_on_both_sides() {
    let (_b, _c, s) = env("rep-rules").await;
    s.create_bucket("src", BucketOptions::default()).await.unwrap();
    s.create_bucket("dst", BucketOptions::default()).await.unwrap();
    assert!(matches!(s.set_replication("src", Some(&rule("dst", false))).await, Err(StoreError::InvalidRequest(_))));
    s.update_bucket("src", BucketUpdate { versioning: Some("Enabled".into()), ..Default::default() }).await.unwrap();
    let e = s.set_replication("src", Some(&rule("dst", false))).await.unwrap_err();
    assert!(e.to_string().contains("Destination bucket must have versioning"), "{e}");
    assert!(s.set_replication("src", Some(&rule("nope", false))).await.unwrap_err().to_string().contains("must exist"));
    s.update_bucket("dst", BucketUpdate { versioning: Some("Enabled".into()), ..Default::default() }).await.unwrap();
    s.set_replication("src", Some(&rule("dst", false))).await.unwrap();
    assert_eq!(s.replication("src").unwrap().unwrap().rules[0].destination, "dst");
    s.set_replication("src", None).await.unwrap();
    assert!(s.replication("src").unwrap().is_none());
}

#[tokio::test]
async fn replicas_share_the_blob_and_keep_the_version_id() {
    let (_b, clock, s) = env("rep-meta").await;
    versioned(&s, "src").await;
    versioned(&s, "dst").await;
    s.set_replication("src", Some(&rule("dst", true))).await.unwrap();
    let v1 = put(&s, "src", "k", b"one").await;
    let v2 = put(&s, "src", "k", b"two").await;
    let src = s.bucket("src").unwrap();
    let dst = s.bucket("dst").unwrap();
    assert_eq!(s.replication_status(&src.id, "k", &v1.version_id).unwrap().as_deref(), Some("PENDING"));
    let blobs = s.stats().unwrap().blobs;
    assert_eq!(replicate_all(&s).await, [Replicated::Completed, Replicated::Completed]);
    assert_eq!(s.stats().unwrap().blobs, blobs, "no bytes copied");
    let r1 = s.get_object_version("dst", "k", Some(&v1.version_id)).unwrap();
    let r2 = s.get_object_version("dst", "k", None).unwrap();
    assert_eq!((r1.blob_id.as_str(), r2.version_id.as_str()), (v1.blob_id.as_str(), v2.version_id.as_str()));
    assert_eq!(r2.modified_at, v2.modified_at);
    assert_eq!(s.replication_status(&src.id, "k", &v2.version_id).unwrap().as_deref(), Some("COMPLETED"));
    assert_eq!(s.replication_status(&dst.id, "k", &v2.version_id).unwrap().as_deref(), Some("REPLICA"));
    // A delete marker replicates; deleting a version by id doesn't.
    let marker = s.delete_object_version("src", "k", None, None, false).await.unwrap();
    s.delete_object_version("src", "k", Some(&v1.version_id), None, false).await.unwrap();
    replicate_all(&s).await;
    assert!(matches!(s.get_object_version("dst", "k", None), Err(StoreError::DeleteMarker { .. })));
    let vs = s.list_versions("dst", &VersionsQuery { max_keys: 10, ..Default::default() }).unwrap().versions;
    let ids: Vec<&str> = vs.iter().map(|v| v.version_id.as_str()).collect();
    assert_eq!(ids, [marker.version_id.as_deref().unwrap(), v2.version_id.as_str(), v1.version_id.as_str()]);
    // The source's copy of v1 is gone for good; the replica keeps the blob alive.
    s.update_bucket("src", BucketUpdate { recover_days: Some(0), ..Default::default() }).await.unwrap();
    s.delete_object_version("src", "k", Some(&v2.version_id), None, false).await.unwrap();
    clock.advance(30 * DAY);
    s.sweep().await.unwrap();
    clock.advance(2 * DAY);
    s.sweep().await.unwrap();
    assert_eq!(read_version(&s, "dst", "k", &v1.version_id).await, b"one");
    assert_eq!(read_version(&s, "dst", "k", &v2.version_id).await, b"two");
}

async fn read_version(s: &Store, bucket: &str, key: &str, vid: &str) -> Vec<u8> {
    use futures::StreamExt;
    let m = s.get_object_version(bucket, key, Some(vid)).unwrap();
    let (_, mut body) = s.open_blob(&m, None).await.unwrap();
    let mut out = Vec::new();
    while let Some(c) = body.next().await {
        out.extend_from_slice(&c.unwrap());
    }
    out
}

#[tokio::test]
async fn replication_is_applied_once_and_fails_visibly() {
    let (_b, _c, s) = env("rep-once").await;
    versioned(&s, "src").await;
    versioned(&s, "dst").await;
    notify_all(&s, "dst").await;
    s.set_replication("src", Some(&rule("dst", false))).await.unwrap();
    let v = put(&s, "src", "k", b"x").await;
    let due: Vec<PendingEvent> = s.due_events(10).await.unwrap();
    assert_eq!(s.replicate(&due[0]).await.unwrap(), Replicated::Completed);
    assert_eq!(s.replicate(&due[0]).await.unwrap(), Replicated::Skipped, "a second run does nothing");
    assert_eq!(s.list_versions("dst", &VersionsQuery { max_keys: 10, ..Default::default() }).unwrap().versions.len(), 1);
    let dest_events = s.pending_events(Some(&s.bucket("dst").unwrap().id), 10).unwrap();
    assert_eq!(dest_events.len(), 1);
    assert_eq!(dest_events[0].record().unwrap().principal.as_deref(), Some("roll:replication"));
    // The destination loses versioning: the next replication fails, visibly.
    s.update_bucket("dst", BucketUpdate { versioning: Some("Suspended".into()), ..Default::default() }).await.unwrap();
    let mut cfg = NotificationConfig { configurations: vec![queue_target("rep", &["s3:Replication:*"], None)], event_bridge: false };
    cfg.validate().unwrap();
    s.set_notifications("src", Some(&cfg)).await.unwrap();
    let v2 = put(&s, "src", "k", b"y").await;
    let out = replicate_all(&s).await;
    assert!(matches!(&out[..], [Replicated::Failed(_)]), "{out:?}");
    let src = s.bucket("src").unwrap();
    assert_eq!(s.replication_status(&src.id, "k", &v2.version_id).unwrap().as_deref(), Some("FAILED"));
    assert_eq!(s.replication_status(&src.id, "k", &v.version_id).unwrap().as_deref(), Some("COMPLETED"));
    let failed = s.pending_events(Some(&src.id), 10).unwrap().pop().unwrap().record().unwrap();
    assert_eq!(failed.name, "Replication:OperationFailedReplication");
    assert_eq!(failed.replication_destination.as_deref(), Some("dst"));
}

#[tokio::test]
async fn existing_objects_are_backfilled() {
    let (_b, _c, s) = env("rep-backfill").await;
    versioned(&s, "src").await;
    versioned(&s, "dst").await;
    for i in 0..5 {
        put(&s, "src", &format!("k{i}"), b"v").await;
    }
    let mut cfg = rule("dst", false);
    cfg.rules[0].existing_objects = true;
    s.set_replication("src", Some(&cfg)).await.unwrap();
    put(&s, "src", "new", b"v").await;
    let done = replicate_all(&s).await;
    assert_eq!(done.len(), 6);
    assert_eq!(s.list_objects("dst", &ListQuery { max_keys: 100, ..Default::default() }).unwrap().objects.len(), 6);
    assert_eq!(s.events_backlog().unwrap(), 0);
}

#[tokio::test]
async fn inventory_pages_cover_every_version() {
    let (_b, _c, s) = env("inv-page").await;
    versioned(&s, "ivb").await;
    put(&s, "ivb", "a", b"1").await;
    put(&s, "ivb", "a", b"22").await;
    put(&s, "ivb", "b", b"333").await;
    s.delete_object("ivb", "b", None).await.unwrap();
    let (cur, next) = s.inventory_page("ivb", false, "", None, 10).unwrap();
    assert_eq!(cur.iter().map(|e| (e.key.as_str(), e.size)).collect::<Vec<_>>(), [("a", 2)]);
    assert!(next.is_none());
    let mut all = Vec::new();
    let mut after = None;
    loop {
        let (page, next) = s.inventory_page("ivb", true, "", after, 2).unwrap();
        all.extend(page);
        match next {
            Some(n) => after = Some(n),
            None => break,
        }
    }
    let got: Vec<(&str, bool, bool)> = all.iter().map(|e| (e.key.as_str(), e.is_latest, e.delete_marker)).collect();
    assert_eq!(got, [("a", true, false), ("a", false, false), ("b", true, true), ("b", false, false)]);
}
