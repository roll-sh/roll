//! Event notifications, replication and inventory reports through the AWS SDK: the event JSON,
//! filters, test events, delivery to a test sink and to a webhook (a local axum server),
//! retries, destination validation, replication end to end, and inventory output.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    DeleteMarkerReplication, DeleteMarkerReplicationStatus, Destination, Event, FilterRule, FilterRuleName, InventoryConfiguration, InventoryDestination, InventoryFormat, InventoryFrequency,
    InventoryIncludedObjectVersions, InventoryOptionalField, InventoryS3BucketDestination, InventorySchedule, NotificationConfiguration, NotificationConfigurationFilter, QueueConfiguration,
    ReplicationConfiguration, ReplicationRule, ReplicationRuleFilter, ReplicationRuleStatus, S3KeyFilter, TopicConfiguration, VersioningConfiguration,
};
use common::*;
use futures::future::BoxFuture;
use s3_server::{Delivery, EventSink, RollDestination, Server};
use serde_json::Value;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn code<E: aws_sdk_s3::error::ProvideErrorMetadata>(e: &aws_sdk_s3::error::SdkError<E, aws_sdk_s3::config::http::HttpResponse>) -> String {
    aws_sdk_s3::error::ProvideErrorMetadata::code(e).unwrap_or_default().to_string()
}

/// roll queues and streams, as far as storage can tell: a set of destinations that exist, and
/// what was delivered to them. Fails the next `fail` deliveries.
#[derive(Default)]
struct TestSink {
    known: Vec<RollDestination>,
    got: Mutex<Vec<(RollDestination, Delivery)>>,
    fail: AtomicU32,
}

impl EventSink for TestSink {
    fn check<'a>(&'a self, dest: &'a RollDestination) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { if self.known.contains(dest) { Ok(()) } else { Err(format!("{dest:?} doesn't exist")) } })
    }

    fn deliver<'a>(&'a self, dest: &'a RollDestination, d: &'a Delivery) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) > 0 {
                self.fail.fetch_sub(1, Ordering::SeqCst);
                return Err("queue unavailable".into());
            }
            if !self.known.contains(dest) {
                return Err("no such queue".into());
            }
            self.got.lock().unwrap().push((dest.clone(), d.clone()));
            Ok(())
        })
    }
}

impl TestSink {
    fn take(&self) -> Vec<(RollDestination, Value)> {
        self.got.lock().unwrap().drain(..).map(|(d, x)| (d, serde_json::from_slice(&x.body).unwrap())).collect()
    }
}

fn queue(name: &str) -> RollDestination {
    RollDestination::Queue { vhost: "/".into(), queue: name.into() }
}

async fn setup(tweak: impl FnOnce(&mut s3_server::Config)) -> (TestServer, Arc<Server>, Arc<TestSink>) {
    let (t, server) = start_full(
        |_| {},
        |c| {
            c.notify.dispatch_interval = None;
            c.notify.inventory_interval = None;
            c.notify.backoff_base = Duration::from_millis(5);
            c.notify.backoff_max = Duration::from_millis(20);
            tweak(c);
        },
    )
    .await;
    let sink = Arc::new(TestSink { known: vec![queue("orders"), queue("other"), RollDestination::Stream { stream: "main".into(), topic: "s3".into() }], ..Default::default() });
    server.notifier.set_sink(sink.clone());
    (t, server, sink)
}

async fn bucket(t: &TestServer, name: &str) {
    t.client.create_bucket().bucket(name).send().await.unwrap();
}

async fn versioned(t: &TestServer, name: &str) {
    bucket(t, name).await;
    t.client
        .put_bucket_versioning()
        .bucket(name)
        .versioning_configuration(VersioningConfiguration::builder().status(aws_sdk_s3::types::BucketVersioningStatus::Enabled).build())
        .send()
        .await
        .unwrap();
}

async fn put(t: &TestServer, b: &str, k: &str, body: &[u8]) -> aws_sdk_s3::operation::put_object::PutObjectOutput {
    t.client.put_object().bucket(b).key(k).body(ByteStream::from(body.to_vec())).send().await.unwrap()
}

fn queue_config(id: &str, arn: &str, events: &[&str], prefix: Option<&str>, suffix: Option<&str>) -> QueueConfiguration {
    let mut rules = Vec::new();
    if let Some(p) = prefix {
        rules.push(FilterRule::builder().name(FilterRuleName::Prefix).value(p).build());
    }
    if let Some(s) = suffix {
        rules.push(FilterRule::builder().name(FilterRuleName::Suffix).value(s).build());
    }
    let mut b = QueueConfiguration::builder().id(id).queue_arn(arn);
    for e in events {
        b = b.events(Event::from(*e));
    }
    if !rules.is_empty() {
        b = b.filter(NotificationConfigurationFilter::builder().key(S3KeyFilter::builder().set_filter_rules(Some(rules)).build()).build());
    }
    b.build().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn configuration_round_trip_test_event_and_records() {
    let (t, server, sink) = setup(|_| {}).await;
    bucket(&t, "notes").await;
    let c = &t.client;
    let cfg = NotificationConfiguration::builder()
        .queue_configurations(queue_config("images", "arn:roll:queues:/:orders", &["s3:ObjectCreated:*"], Some("img/"), Some(".jpg")))
        .topic_configurations(TopicConfiguration::builder().id("gone").topic_arn("arn:roll:streams:main:s3").events(Event::from("s3:ObjectRemoved:*")).build().unwrap())
        .build();
    c.put_bucket_notification_configuration().bucket("notes").notification_configuration(cfg).send().await.unwrap();
    // S3 sends each destination a test event when the configuration is put.
    let tests = sink.take();
    assert_eq!(tests.len(), 2);
    for (_, v) in &tests {
        assert_eq!(v["Event"], "s3:TestEvent");
        assert_eq!(v["Bucket"], "notes");
        assert_eq!(v["Service"], "Amazon S3");
    }
    let got = c.get_bucket_notification_configuration().bucket("notes").send().await.unwrap();
    let q = &got.queue_configurations()[0];
    assert_eq!((q.id(), q.queue_arn()), (Some("images"), "arn:roll:queues:/:orders"));
    let rules = q.filter().unwrap().key().unwrap().filter_rules();
    assert_eq!(rules.len(), 2);
    assert_eq!(got.topic_configurations()[0].topic_arn(), "arn:roll:streams:main:s3");

    let out = put(&t, "notes", "img/red flower.jpg", b"petals").await;
    put(&t, "notes", "img/a.png", b"x").await;
    put(&t, "notes", "doc/a.jpg", b"x").await;
    c.delete_object().bucket("notes").key("img/a.png").send().await.unwrap();
    assert!(sink.take().is_empty(), "nothing is delivered before the dispatcher runs");
    server.dispatch_events().await.unwrap();
    let got = sink.take();
    assert_eq!(got.len(), 2, "{got:?}");
    let (dest, v) = &got[0];
    assert_eq!(*dest, queue("orders"));
    let r = &v["Records"][0];
    assert_eq!(r["eventVersion"], "2.1");
    assert_eq!(r["eventSource"], "aws:s3");
    assert_eq!(r["awsRegion"], "us-east-1");
    assert_eq!(r["eventName"], "ObjectCreated:Put");
    assert_eq!(r["userIdentity"]["principalId"], ACCESS);
    assert_eq!(r["requestParameters"]["sourceIPAddress"], "127.0.0.1");
    assert!(r["responseElements"]["x-amz-request-id"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(r["eventTime"].as_str().unwrap().ends_with('Z'));
    let s3 = &r["s3"];
    assert_eq!(s3["s3SchemaVersion"], "1.0");
    assert_eq!(s3["configurationId"], "images");
    assert_eq!(s3["bucket"]["name"], "notes");
    assert_eq!(s3["bucket"]["arn"], "arn:aws:s3:::notes");
    assert_eq!(s3["bucket"]["ownerIdentity"]["principalId"], t.store.owner_id());
    assert_eq!(s3["object"]["key"], "img/red+flower.jpg", "keys are URL-encoded like S3's");
    assert_eq!(s3["object"]["size"], 6);
    assert_eq!(format!("\"{}\"", s3["object"]["eTag"].as_str().unwrap()), out.e_tag().unwrap());
    assert!(s3["object"].get("versionId").is_none(), "an unversioned bucket");
    assert_eq!(s3["object"]["sequencer"].as_str().unwrap().len(), 18);
    let (dest, v) = &got[1];
    assert_eq!(*dest, RollDestination::Stream { stream: "main".into(), topic: "s3".into() });
    assert_eq!(v["Records"][0]["eventName"], "ObjectRemoved:Delete");
    assert!(v["Records"][0]["s3"]["object"].get("size").is_none());
    assert_eq!(t.store.events_backlog().unwrap(), 0, "delivered rows are gone");
    assert_eq!(server.notifier.stats().delivered, 2, "test events are sent while validating, not by the dispatcher");
}

#[tokio::test(flavor = "multi_thread")]
async fn destinations_are_validated_like_s3() {
    let (t, _server, sink) = setup(|_| {}).await;
    bucket(&t, "val").await;
    let c = &t.client;
    let put_cfg = |arn: &str| NotificationConfiguration::builder().queue_configurations(queue_config("q", arn, &["s3:ObjectCreated:*"], None, None)).build();
    for arn in ["arn:roll:queues:/:missing", "arn:aws:sqs:us-east-1:123456789012:q", "arn:roll:webhook:http://127.0.0.1:9/hook", "not-an-arn"] {
        let e = c.put_bucket_notification_configuration().bucket("val").notification_configuration(put_cfg(arn)).send().await.unwrap_err();
        assert_eq!(code(&e), "InvalidArgument", "{arn}");
        let body = String::from_utf8_lossy(e.raw_response().unwrap().body().bytes().unwrap()).to_string();
        assert!(body.contains("<ArgumentName1>") && body.contains(arn), "{body}");
    }
    assert!(c.get_bucket_notification_configuration().bucket("val").send().await.unwrap().queue_configurations().is_empty(), "nothing was stored");
    // x-amz-skip-destination-validation: stored without checks or test events.
    c.put_bucket_notification_configuration()
        .bucket("val")
        .notification_configuration(put_cfg("arn:roll:queues:/:missing"))
        .skip_destination_validation(true)
        .send()
        .await
        .unwrap();
    assert!(sink.take().is_empty());
    // S3's own rules.
    let overlap = NotificationConfiguration::builder()
        .queue_configurations(queue_config("a", "arn:roll:queues:/:orders", &["s3:ObjectCreated:*"], Some("x/"), None))
        .queue_configurations(queue_config("b", "arn:roll:queues:/:other", &["s3:ObjectCreated:Put"], Some("x/y"), None))
        .build();
    let e = c.put_bucket_notification_configuration().bucket("val").notification_configuration(overlap).send().await.unwrap_err();
    assert_eq!(code(&e), "InvalidArgument");
    let bad_event = put_cfg("arn:roll:queues:/:orders");
    let mut r = Raw::new("PUT", "/val");
    r.query = "notification";
    r.body = b"<NotificationConfiguration><QueueConfiguration><Queue>arn:roll:queues:/:orders</Queue><Event>s3:Bogus</Event></QueueConfiguration></NotificationConfiguration>".to_vec();
    assert_eq!(r.send(&t).await.status(), 400);
    let _ = bad_event;
    // An empty configuration removes it.
    c.put_bucket_notification_configuration().bucket("val").notification_configuration(NotificationConfiguration::builder().build()).send().await.unwrap();
    assert!(t.store.notifications("val").unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_deliveries_are_retried_in_order_then_dropped() {
    let (t, server, sink) = setup(|c| c.notify.retention = Duration::from_millis(400)).await;
    bucket(&t, "retry").await;
    let cfg = NotificationConfiguration::builder().queue_configurations(queue_config("q", "arn:roll:queues:/:orders", &["s3:ObjectCreated:*"], None, None)).build();
    t.client.put_bucket_notification_configuration().bucket("retry").notification_configuration(cfg).send().await.unwrap();
    sink.take();
    put(&t, "retry", "k", b"1").await;
    put(&t, "retry", "k", b"22").await;
    sink.fail.store(2, Ordering::SeqCst);
    server.dispatch_events().await.unwrap();
    assert!(sink.take().is_empty(), "the first event failed, and the second waits behind it");
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        server.dispatch_events().await.unwrap();
        if t.store.events_backlog().unwrap() == 0 {
            break;
        }
    }
    let got = sink.take();
    let sizes: Vec<i64> = got.iter().map(|(_, v)| v["Records"][0]["s3"]["object"]["size"].as_i64().unwrap()).collect();
    assert_eq!(sizes, [1, 2], "delivered after retries, in order");
    assert!(server.notifier.stats().retried >= 2);
    // A destination that never works: dropped after the retention window, and counted.
    sink.fail.store(u32::MAX, Ordering::SeqCst);
    put(&t, "retry", "doomed", b"x").await;
    server.dispatch_events().await.unwrap();
    tokio::time::sleep(Duration::from_millis(450)).await;
    for _ in 0..10 {
        server.dispatch_events().await.unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(t.store.events_backlog().unwrap(), 0);
    assert_eq!(server.notifier.stats().dropped, 1);
}

/// What a webhook receiver got: headers and body of each request.
type Received = Arc<Mutex<Vec<(axum::http::HeaderMap, bytes::Bytes)>>>;

#[derive(Clone)]
struct Hook {
    got: Received,
    statuses: Arc<Mutex<Vec<u16>>>,
}

async fn hook(axum::extract::State(h): axum::extract::State<Hook>, headers: axum::http::HeaderMap, body: bytes::Bytes) -> axum::http::StatusCode {
    h.got.lock().unwrap().push((headers, body));
    let mut s = h.statuses.lock().unwrap();
    let code = if s.is_empty() { 200 } else { s.remove(0) };
    axum::http::StatusCode::from_u16(code).unwrap()
}

/// A local webhook receiver: records requests, answers with the next status from `statuses`
/// (then 200).
async fn webhook_server(statuses: Vec<u16>) -> (String, Received) {
    let got: Received = Default::default();
    let app = axum::Router::new().route("/hook", axum::routing::post(hook)).with_state(Hook { got: got.clone(), statuses: Arc::new(Mutex::new(statuses)) });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}/hook", l.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, got)
}

#[tokio::test(flavor = "multi_thread")]
async fn webhooks_are_signed_and_retried() {
    let (t, server, _sink) = setup(|c| c.notify.webhook_allow_private = true).await;
    versioned(&t, "hooks").await;
    let (url, got) = webhook_server(vec![200, 500, 503]).await;
    let arn = format!("arn:roll:webhook:{url}");
    let cfg = NotificationConfiguration::builder()
        .topic_configurations(TopicConfiguration::builder().id("wh").topic_arn(&arn).events(Event::from("s3:ObjectCreated:*")).events(Event::from("s3:ObjectTagging:*")).build().unwrap())
        .build();
    t.client.put_bucket_notification_configuration().bucket("hooks").notification_configuration(cfg).send().await.unwrap();
    let secret = t.store.webhook_secret("hooks", false).await.unwrap();
    let out = put(&t, "hooks", "a.txt", b"abc").await;
    t.client
        .put_object_tagging()
        .bucket("hooks")
        .key("a.txt")
        .tagging(aws_sdk_s3::types::Tagging::builder().tag_set(aws_sdk_s3::types::Tag::builder().key("k").value("v").build().unwrap()).build().unwrap())
        .send()
        .await
        .unwrap();
    for _ in 0..100 {
        server.dispatch_events().await.unwrap();
        if t.store.events_backlog().unwrap() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let got = got.lock().unwrap().clone();
    // The test event, two failed attempts of the PUT's event, then both events.
    assert_eq!(got.len(), 5, "{got:?}");
    for (headers, body) in &got {
        let sig = headers.get("x-roll-signature").unwrap().to_str().unwrap();
        let t_s: i64 = sig.split(',').next().unwrap().strip_prefix("t=").unwrap().parse().unwrap();
        assert_eq!(sig, s3_server::notify::webhook_signature(&secret, t_s, body), "signed with the bucket's secret");
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
    }
    let names: Vec<String> = got.iter().map(|(h, _)| h.get("x-roll-event").unwrap().to_str().unwrap().to_string()).collect();
    assert_eq!(names, ["s3:TestEvent", "s3:ObjectCreated:Put", "s3:ObjectCreated:Put", "s3:ObjectCreated:Put", "s3:ObjectTagging:Put"]);
    let ids: Vec<&str> = got[1..4].iter().map(|(h, _)| h.get("x-roll-delivery").unwrap().to_str().unwrap()).collect();
    assert!(ids.iter().all(|i| *i == ids[0]), "retries keep the delivery id");
    let v: Value = serde_json::from_slice(&got[3].1).unwrap();
    assert_eq!(v["Records"][0]["s3"]["object"]["versionId"], out.version_id().unwrap(), "versioned buckets name the version");
    let tag: Value = serde_json::from_slice(&got[4].1).unwrap();
    assert_eq!(tag["Records"][0]["eventVersion"], "2.3");
}

fn replication_rule(dest: &str, prefix: &str, markers: bool) -> ReplicationRule {
    let status = if markers { DeleteMarkerReplicationStatus::Enabled } else { DeleteMarkerReplicationStatus::Disabled };
    ReplicationRule::builder()
        .id("r1")
        .priority(1)
        .status(ReplicationRuleStatus::Enabled)
        .filter(ReplicationRuleFilter::builder().prefix(prefix).build())
        .delete_marker_replication(DeleteMarkerReplication::builder().status(status).build())
        .destination(Destination::builder().bucket(format!("arn:aws:s3:::{dest}")).build().unwrap())
        .build()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn replication_end_to_end() {
    let (t, server, _sink) = setup(|_| {}).await;
    let c = &t.client;
    versioned(&t, "rsrc").await;
    bucket(&t, "rdst").await;
    let cfg = |dest: &str| ReplicationConfiguration::builder().role("arn:aws:iam::123456789012:role/replication").rules(replication_rule(dest, "", true)).build().unwrap();
    // Like S3: versioning on both sides; a destination outside roll isn't supported.
    let e = c.put_bucket_replication().bucket("rsrc").replication_configuration(cfg("rdst")).send().await.unwrap_err();
    assert_eq!(code(&e), "InvalidRequest");
    let e = c.put_bucket_replication().bucket("rsrc").replication_configuration(cfg("some-aws-bucket")).send().await.unwrap_err();
    assert_eq!(code(&e), "NotImplemented");
    c.put_bucket_versioning()
        .bucket("rdst")
        .versioning_configuration(VersioningConfiguration::builder().status(aws_sdk_s3::types::BucketVersioningStatus::Enabled).build())
        .send()
        .await
        .unwrap();
    c.put_bucket_replication().bucket("rsrc").replication_configuration(cfg("rdst")).send().await.unwrap();
    let got = c.get_bucket_replication().bucket("rsrc").send().await.unwrap();
    let rc = got.replication_configuration().unwrap();
    assert_eq!(rc.role(), "arn:aws:iam::123456789012:role/replication");
    assert_eq!(rc.rules()[0].destination().unwrap().bucket(), "arn:aws:s3:::rdst");

    let blobs = t.store.stats().unwrap().blobs;
    let v1 = put(&t, "rsrc", "doc.txt", b"version one").await;
    let head = c.head_object().bucket("rsrc").key("doc.txt").send().await.unwrap();
    assert_eq!(head.replication_status().map(|s| s.as_str()), Some("PENDING"));
    server.dispatch_events().await.unwrap();
    let head = c.head_object().bucket("rsrc").key("doc.txt").send().await.unwrap();
    assert_eq!(head.replication_status().map(|s| s.as_str()), Some("COMPLETED"));
    let replica = c.get_object().bucket("rdst").key("doc.txt").send().await.unwrap();
    assert_eq!(replica.replication_status().map(|s| s.as_str()), Some("REPLICA"));
    assert_eq!(replica.version_id(), v1.version_id(), "same version id as the source");
    assert_eq!(replica.e_tag(), v1.e_tag());
    assert_eq!(replica.body.collect().await.unwrap().into_bytes().as_ref(), b"version one");
    assert_eq!(t.store.stats().unwrap().blobs, blobs + 1, "the replica shares the source's blob");

    // A delete marker replicates; a version delete doesn't.
    let marker = c.delete_object().bucket("rsrc").key("doc.txt").send().await.unwrap();
    server.dispatch_events().await.unwrap();
    let e = c.head_object().bucket("rdst").key("doc.txt").send().await.unwrap_err();
    assert_eq!(e.raw_response().unwrap().headers().get("x-amz-delete-marker"), Some("true"));
    c.delete_object().bucket("rsrc").key("doc.txt").version_id(v1.version_id().unwrap()).send().await.unwrap();
    server.dispatch_events().await.unwrap();
    let vs = c.list_object_versions().bucket("rdst").send().await.unwrap();
    assert_eq!(vs.versions().len(), 1, "the replica's version stays");
    assert_eq!(vs.delete_markers()[0].version_id(), marker.version_id());
    // Deleting the configuration stops replication.
    c.delete_bucket_replication().bucket("rsrc").send().await.unwrap();
    assert_eq!(code(&c.get_bucket_replication().bucket("rsrc").send().await.unwrap_err()), "ReplicationConfigurationNotFoundError");
    put(&t, "rsrc", "later.txt", b"x").await;
    server.dispatch_events().await.unwrap();
    assert!(c.head_object().bucket("rdst").key("later.txt").send().await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn inventory_reports() {
    let (t, server, _sink) = setup(|_| {}).await;
    let c = &t.client;
    versioned(&t, "stock").await;
    bucket(&t, "reports").await;
    put(&t, "stock", "a b.txt", b"1").await;
    put(&t, "stock", "a b.txt", b"22").await;
    put(&t, "stock", "c.txt", b"333").await;
    let dest = InventoryDestination::builder()
        .s3_bucket_destination(InventoryS3BucketDestination::builder().bucket("arn:aws:s3:::reports").format(InventoryFormat::Csv).prefix("inv").build().unwrap())
        .build();
    let inv = InventoryConfiguration::builder()
        .id("all")
        .is_enabled(true)
        .destination(dest.clone())
        .included_object_versions(InventoryIncludedObjectVersions::All)
        .schedule(InventorySchedule::builder().frequency(InventoryFrequency::Daily).build().unwrap())
        .optional_fields(InventoryOptionalField::Size)
        .optional_fields(InventoryOptionalField::ETag)
        .optional_fields(InventoryOptionalField::IsMultipartUploaded)
        .optional_fields(InventoryOptionalField::ChecksumAlgorithm)
        .build()
        .unwrap();
    c.put_bucket_inventory_configuration().bucket("stock").id("all").inventory_configuration(inv).send().await.unwrap();
    // Parquet and ORC aren't written.
    let parquet = InventoryConfiguration::builder()
        .id("pq")
        .is_enabled(true)
        .destination(InventoryDestination::builder().s3_bucket_destination(InventoryS3BucketDestination::builder().bucket("arn:aws:s3:::reports").format(InventoryFormat::Parquet).build().unwrap()).build())
        .included_object_versions(InventoryIncludedObjectVersions::Current)
        .schedule(InventorySchedule::builder().frequency(InventoryFrequency::Weekly).build().unwrap())
        .build()
        .unwrap();
    assert_eq!(code(&c.put_bucket_inventory_configuration().bucket("stock").id("pq").inventory_configuration(parquet).send().await.unwrap_err()), "NotImplemented");

    assert_eq!(server.run_inventory(false).await, 1, "a new configuration is due");
    assert_eq!(server.run_inventory(false).await, 0, "and then not until tomorrow");
    let listed = c.list_objects_v2().bucket("reports").prefix("inv/stock/all/").send().await.unwrap();
    let keys: Vec<&str> = listed.contents().iter().filter_map(|o| o.key()).collect();
    let manifest_key = keys.iter().find(|k| k.ends_with("/manifest.json")).unwrap().to_string();
    assert!(keys.iter().any(|k| k.ends_with("/manifest.checksum")));
    assert!(keys.iter().any(|k| k.contains("/hive/dt=") && k.ends_with("/symlink.txt")));
    let folder = manifest_key.strip_prefix("inv/stock/all/").unwrap().strip_suffix("/manifest.json").unwrap();
    assert_eq!(folder.len(), "2024-01-01T00-00Z".len(), "{folder}");
    let get = |k: String| async move { c.get_object().bucket("reports").key(k).send().await.unwrap().body.collect().await.unwrap().into_bytes() };
    let manifest_bytes = get(manifest_key.clone()).await;
    let checksum = get(manifest_key.replace("manifest.json", "manifest.checksum")).await;
    assert_eq!(String::from_utf8_lossy(&checksum), md5_hex(&manifest_bytes));
    let manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    assert_eq!(manifest["sourceBucket"], "stock");
    assert_eq!(manifest["destinationBucket"], "arn:aws:s3:::reports");
    assert_eq!(manifest["fileFormat"], "CSV");
    assert_eq!(manifest["fileSchema"], "Bucket, Key, VersionId, IsLatest, IsDeleteMarker, Size, ETag, IsMultipartUploaded, ChecksumAlgorithm");
    let file = &manifest["files"][0];
    let data_key = file["key"].as_str().unwrap().to_string();
    assert!(data_key.starts_with("inv/stock/all/data/") && data_key.ends_with(".csv.gz"), "{data_key}");
    let gz = get(data_key).await;
    assert_eq!(file["MD5checksum"], md5_hex(&gz));
    assert_eq!(file["size"], gz.len());
    let mut csv = String::new();
    std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&gz[..]), &mut csv).unwrap();
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines.len(), 3, "{csv}");
    assert!(lines[0].starts_with("\"stock\",\"a+b.txt\","), "{}", lines[0]);
    assert!(lines[0].contains(",\"true\",\"false\",\"2\","), "{}", lines[0]);
    assert!(lines[1].contains(",\"false\",\"false\",\"1\","), "{}", lines[1]);
    assert!(lines[2].ends_with(",\"false\",\"CRC32\""), "{}", lines[2]);
}
