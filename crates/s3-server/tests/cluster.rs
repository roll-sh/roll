//! roll storage on several nodes in one process (docs/storage/architecture.md, "Many nodes"):
//! requests through any node reach the metadata's owner, a standby takes over from a killed
//! owner with every acknowledged write, a drain hands the metadata off without failing a
//! request, a deposed owner can't commit, and events recorded before a crash reach their
//! destination from the next owner. Each runs on roll-cluster's memory backend and on DynamoDB
//! Local (`ROLL_CLUSTER_TEST_DYNAMODB`; skipped without it).

mod common;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart, Event, NotificationConfiguration, QueueConfiguration};
use common::fleet::*;
use common::*;
use futures::future::BoxFuture;
use s3_server::{Delivery, EventSink, RollDestination};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

macro_rules! on_backends {
    ($($name:ident),* $(,)?) => {
        mod memory {
            $(#[tokio::test(flavor = "multi_thread")]
            async fn $name() {
                super::$name(super::Kind::Memory).await
            })*
        }
        mod dynamodb {
            $(#[tokio::test(flavor = "multi_thread")]
            async fn $name() {
                super::$name(super::Kind::DynamoDb).await
            })*
        }
    };
}

on_backends!(
    requests_through_any_node_reach_the_owner,
    the_owner_killed_a_standby_takes_over_with_every_acknowledged_write,
    a_drain_hands_the_metadata_off_without_failing_a_request,
    a_deposed_owner_cannot_commit,
    events_recorded_before_a_crash_reach_their_destination_from_the_next_owner,
);

/// A client that doesn't retry: whatever the nodes answer, the test sees.
fn strict(n: &Node) -> aws_sdk_s3::Client {
    let conf = client(&n.endpoint, ACCESS, SECRET).config().to_builder().retry_config(aws_sdk_s3::config::retry::RetryConfig::disabled()).build();
    aws_sdk_s3::Client::from_conf(conf)
}

async fn put(c: &aws_sdk_s3::Client, bucket: &str, key: &str, body: Vec<u8>) {
    c.put_object().bucket(bucket).key(key).body(ByteStream::from(body)).send().await.unwrap_or_else(|e| panic!("PUT {key}: {e:?}"));
}

async fn get(c: &aws_sdk_s3::Client, bucket: &str, key: &str) -> Vec<u8> {
    let r = c.get_object().bucket(bucket).key(key).send().await.unwrap_or_else(|e| panic!("GET {key}: {e:?}"));
    r.body.collect().await.unwrap().into_bytes().to_vec()
}

async fn requests_through_any_node_reach_the_owner(kind: Kind) {
    let Some(f) = Fleet::start(kind, "route", 3).await else { return };
    let tenures = f.tenures();
    let via = f.non_owner(&[]);
    let c = strict(&f.nodes[via]);
    c.create_bucket().bucket("route").send().await.unwrap();
    // A small body (kept by the forwarder), a big one (streamed through), and a multipart upload.
    put(&c, "route", "small", b"hello".to_vec()).await;
    let big = data(9 << 20, 3);
    put(&c, "route", "big", big.clone()).await;
    let up = c.create_multipart_upload().bucket("route").key("multi").send().await.unwrap();
    let id = up.upload_id().unwrap();
    let (one, two) = (data(6 << 20, 1), data(1000, 2));
    let mut parts = Vec::new();
    for (n, body) in [(1, &one), (2, &two)] {
        let p = c.upload_part().bucket("route").key("multi").upload_id(id).part_number(n).body(ByteStream::from(body.clone())).send().await.unwrap();
        parts.push(CompletedPart::builder().part_number(n).e_tag(p.e_tag().unwrap()).build());
    }
    c.complete_multipart_upload().bucket("route").key("multi").upload_id(id).multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build()).send().await.unwrap();
    // Read back through every node.
    for n in &f.nodes {
        let c = strict(n);
        assert_eq!(get(&c, "route", "small").await, b"hello");
        assert_eq!(get(&c, "route", "big").await, big);
        assert_eq!(get(&c, "route", "multi").await, [one.clone(), two.clone()].concat());
    }
    let listed = c.list_objects_v2().bucket("route").send().await.unwrap();
    assert_eq!(listed.contents().iter().filter_map(|o| o.key()).collect::<Vec<_>>(), ["big", "multi", "small"]);
    // The metadata never moved meanwhile; a standby's copy keeps up with the log.
    assert_eq!(f.tenures(), tenures);
    let seq = f.store().stats().unwrap().log.seq;
    eventually(10, "a standby to follow the log", || f.nodes.iter().any(|n| n.storage.status().following.is_some_and(|s| s >= seq))).await;
}

async fn the_owner_killed_a_standby_takes_over_with_every_acknowledged_write(kind: Kind) {
    let Some(f) = Fleet::start(kind, "kill", 3).await else { return };
    let old = f.wait_owner().await;
    let via = f.non_owner(&[]);
    let c = strict(&f.nodes[via]);
    c.create_bucket().bucket("kill").send().await.unwrap();
    for i in 0..20 {
        put(&c, "kill", &format!("k{i}"), format!("value {i}").into_bytes()).await;
    }
    let seq = f.store().stats().unwrap().log.seq;
    eventually(10, "a standby to follow the log", || f.nodes.iter().any(|n| n.storage.status().following.is_some_and(|s| s >= seq))).await;
    let killed = Instant::now();
    f.nodes[old].kill();
    let tenures = f.tenures();
    let new = f.wait_owner_not(Some(old)).await;
    assert!(killed.elapsed() < Duration::from_secs(10), "took over in {:?}", killed.elapsed());
    // Every acknowledged write is there, through a node that never owned the metadata.
    let c = strict(&f.nodes[f.non_owner(&[old])]);
    for i in 0..20 {
        assert_eq!(get(&c, "kill", &format!("k{i}")).await, format!("value {i}").into_bytes());
    }
    put(&c, "kill", "after", b"x".to_vec()).await;
    assert_eq!(f.nodes[new].storage.status().tenures, tenures[new] + 1);
}

async fn a_drain_hands_the_metadata_off_without_failing_a_request(kind: Kind) {
    let Some(f) = Fleet::start(kind, "drain", 3).await else { return };
    let owner = f.wait_owner().await;
    let via = f.non_owner(&[owner]);
    let c = strict(&f.nodes[via]);
    c.create_bucket().bucket("drain").send().await.unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (c, stop) = (c.clone(), stop.clone());
        tokio::spawn(async move {
            let (mut written, mut failed) = (0usize, Vec::new());
            while !stop.load(Ordering::SeqCst) {
                let key = format!("w{written:04}");
                match c.put_object().bucket("drain").key(&key).body(ByteStream::from(key.clone().into_bytes())).send().await {
                    Ok(_) => written += 1,
                    Err(e) => failed.push(format!("{key}: {e:?}")),
                }
                if let Err(e) = c.head_object().bucket("drain").key("w0000").send().await {
                    failed.push(format!("HEAD: {e:?}"));
                }
            }
            (written, failed)
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let tenures = f.tenures();
    let handoffs = handoff_entries(&f).await;
    let (old, new) = f.drain_owner().await;
    assert_eq!(old, owner);
    tokio::time::sleep(Duration::from_millis(500)).await;
    stop.store(true, Ordering::SeqCst);
    let (written, failed) = writer.await.unwrap();
    assert!(failed.is_empty(), "requests failed during the handoff: {failed:#?}");
    assert!(written > 10, "{written}");
    let listed = c.list_objects_v2().bucket("drain").max_keys(1000).send().await.unwrap();
    assert_eq!(listed.contents().len(), written);
    assert_eq!(f.nodes[new].storage.status().tenures, tenures[new] + 1);
    // The old owner's last entry in the log says it handed off.
    assert_eq!(handoff_entries(&f).await, handoffs + 1);
}

/// Entries in the metadata log that end a tenure with a handoff.
async fn handoff_entries(f: &Fleet) -> usize {
    let kind = br#""kind":"handoff""#;
    let mut n = 0;
    for l in s3_store::backend::list_all(&*f.backend, "s3/meta/wal/").await.unwrap() {
        let b = f.backend.get_bytes(&l.key).await.unwrap().unwrap();
        n += usize::from(b.windows(kind.len()).any(|w| w == kind));
    }
    n
}

async fn a_deposed_owner_cannot_commit(kind: Kind) {
    let Some(f) = Fleet::start(kind, "fence", 2).await else { return };
    let old = f.wait_owner().await;
    let stale = f.nodes[old].storage.store().unwrap();
    let c = strict(&f.nodes[f.non_owner(&[])]);
    c.create_bucket().bucket("fence").send().await.unwrap();
    // The owner stalls (no renewals) and is killed mid-tenure: its store is still open, as in a
    // process that kept running without its lease.
    f.nodes[old].cluster.fault_freeze(true);
    f.nodes[old].storage.fault_kill();
    let new = f.wait_owner_not(Some(old)).await;
    put(&c, "fence", "after", b"new owner".to_vec()).await;
    // Acting as if it still owned the metadata, it can't commit anything.
    let r = stale.create_bucket("stale", s3_store::BucketOptions::default()).await;
    assert!(matches!(r, Err(s3_store::StoreError::Unavailable(_))), "{r:?}");
    assert!(!stale.is_open());
    assert!(c.head_bucket().bucket("stale").send().await.is_err());
    assert_eq!(get(&c, "fence", "after").await, b"new owner");
    assert!(f.nodes[new].owns());
}

/// Delivers for every node but `refuse` (an owner that dies before its deliveries go out).
struct Sink {
    node: String,
    refuse: Arc<Mutex<Option<String>>>,
    got: Arc<Mutex<Vec<(String, String, String)>>>,
}

impl EventSink for Sink {
    fn check<'a>(&'a self, _dest: &'a RollDestination) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn deliver<'a>(&'a self, _dest: &'a RollDestination, d: &'a Delivery) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if self.refuse.lock().unwrap().as_deref() == Some(self.node.as_str()) {
                return Err("unreachable".into());
            }
            self.got.lock().unwrap().push((self.node.clone(), d.event_name.clone(), d.key.clone()));
            Ok(())
        })
    }
}

async fn events_recorded_before_a_crash_reach_their_destination_from_the_next_owner(kind: Kind) {
    let refuse = Arc::new(Mutex::new(None::<String>));
    let got = Arc::new(Mutex::new(Vec::new()));
    let (r, g) = (refuse.clone(), got.clone());
    let sinks: Arc<dyn Fn(&str, Arc<s3_server::Server>) + Send + Sync> =
        Arc::new(move |node, server| server.notifier.set_sink(Arc::new(Sink { node: node.to_string(), refuse: r.clone(), got: g.clone() })));
    let Some(f) = Fleet::start_with(kind, "events", 3, |_| {}, Some(sinks)).await else { return };
    let old = f.wait_owner().await;
    let c = strict(&f.nodes[f.non_owner(&[])]);
    c.create_bucket().bucket("events").send().await.unwrap();
    let q = QueueConfiguration::builder().queue_arn("arn:roll:queues:%2F:uploads").events(Event::from("s3:ObjectCreated:*")).build().unwrap();
    c.put_bucket_notification_configuration().bucket("events").notification_configuration(NotificationConfiguration::builder().queue_configurations(q).build()).send().await.unwrap();
    *refuse.lock().unwrap() = Some(f.nodes[old].id.clone());
    put(&c, "events", "a.txt", b"a".to_vec()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let created = |got: &Mutex<Vec<(String, String, String)>>| got.lock().unwrap().iter().filter(|(_, e, _)| e.contains("ObjectCreated")).cloned().collect::<Vec<_>>();
    assert!(created(&got).is_empty(), "the owner couldn't deliver it");
    f.nodes[old].kill();
    let new = f.wait_owner_not(Some(old)).await;
    eventually(20, "the event from the new owner", || !created(&got).is_empty()).await;
    let delivered: Vec<(String, String, String)> = created(&got).into_iter().map(|(n, e, k)| (n, e.trim_start_matches("s3:").to_string(), k)).collect();
    assert_eq!(delivered, [(f.nodes[new].id.clone(), "ObjectCreated:Put".to_string(), "a.txt".to_string())]);
}
