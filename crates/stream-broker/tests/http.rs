//! The HTTP API: control plane, connectionless produce/consume, server-managed groups, SSE,
//! Confluent REST v3, auth, and interop with Kafka clients on the same data.

mod common;

use common::TestBroker;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{BaseConsumer, Consumer};
use rdkafka::message::Message;
use rdkafka::producer::{BaseRecord, DefaultProducerContext, Producer, ThreadedProducer};
use rdkafka::TopicPartitionList;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};
use stream_broker::partitioner::partition_for_key;

const T: Duration = Duration::from_secs(20);

fn client() -> Client {
    Client::builder().timeout(Duration::from_secs(90)).build().unwrap()
}

async fn ok(r: reqwest::RequestBuilder) -> Value {
    let resp = r.send().await.unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert!(status.is_success(), "{status}: {text}");
    if text.is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::String(text)) }
}

async fn status(r: reqwest::RequestBuilder) -> (StatusCode, Value) {
    let resp = r.send().await.unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

async fn create_topic(c: &Client, base: &str, stream: &str, name: &str, partitions: i32) -> Value {
    ok(c.post(format!("{base}/v1/streams/{stream}/topics")).json(&json!({ "name": name, "partitions": partitions }))).await
}

fn values(v: &Value) -> Vec<Value> {
    v["records"].as_array().unwrap().iter().map(|r| r["value"].clone()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn control_plane_streams_topics_keys() {
    let tb = TestBroker::start("http-control").await;
    let (c, base) = (client(), tb.http.clone());

    let created = ok(c.post(format!("{base}/v1/streams")).json(&json!({ "name": "payments" }))).await;
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["name"], "payments");
    assert_eq!(created["bootstrap"], tb.addr);
    let username = created["connect"]["username"].as_str().unwrap();
    assert!(username.starts_with("rk_"));
    assert!(created["connect"]["properties"].as_str().unwrap().contains("sasl.jaas.config"));
    let (st, err) = status(c.post(format!("{base}/v1/streams")).json(&json!({ "name": "payments" }))).await;
    assert_eq!(st, StatusCode::CONFLICT, "{err}");
    let (st, _) = status(c.post(format!("{base}/v1/streams")).json(&json!({ "name": "bad name" }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let list = ok(c.get(format!("{base}/v1/streams"))).await;
    assert!(list.as_array().unwrap().iter().any(|s| s["id"] == id.as_str()));
    // By name works as well as by id.
    let got = ok(c.get(format!("{base}/v1/streams/payments"))).await;
    assert_eq!(got["id"], id.as_str());
    assert_eq!(got["keys"].as_array().unwrap().len(), 1);

    let patched = ok(c.patch(format!("{base}/v1/streams/{id}")).json(&json!({ "settings": { "default_partitions": 4, "auto_create_topics": false } }))).await;
    assert_eq!(patched["settings"]["default_partitions"], 4);
    let (st, _) = status(c.patch(format!("{base}/v1/streams/{id}")).json(&json!({ "settings": { "nope": 1 } }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let key = ok(c.post(format!("{base}/v1/streams/{id}/keys")).json(&json!({ "label": "ci" }))).await;
    let user2 = key["username"].as_str().unwrap().to_string();
    assert_eq!(key["secret"].as_str().unwrap().len(), 40);
    assert_eq!(ok(c.get(format!("{base}/v1/streams/{id}/keys"))).await.as_array().unwrap().len(), 2);
    ok(c.delete(format!("{base}/v1/streams/{id}/keys/{user2}"))).await;
    assert_eq!(ok(c.get(format!("{base}/v1/streams/{id}/keys"))).await.as_array().unwrap().len(), 1);
    assert!(tb.broker.credential(&user2).is_none());

    // Topics: create with the stream's default partitions, then grow and reconfigure online.
    let t = ok(c.post(format!("{base}/v1/streams/{id}/topics")).json(&json!({ "name": "orders", "configs": { "retention.ms": "3600000" } }))).await;
    assert_eq!(t["partitions"], 4);
    assert_eq!(t["effective_configs"]["retention.ms"]["value"], "3600000");
    let (st, _) = status(c.post(format!("{base}/v1/streams/{id}/topics")).json(&json!({ "name": "orders" }))).await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, _) = status(c.post(format!("{base}/v1/streams/{id}/topics")).json(&json!({ "name": "x", "configs": { "retention.ms": "later" } }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let t = ok(c.patch(format!("{base}/v1/streams/{id}/topics/orders")).json(&json!({ "partitions": 6, "configs": { "retention.ms": null, "cleanup.policy": "compact" } }))).await;
    assert_eq!(t["partitions"], 6);
    assert_eq!(t["configs"], json!({ "cleanup.policy": "compact" }));
    assert_eq!(t["partition_stats"].as_array().unwrap().len(), 6);
    let (st, _) = status(c.patch(format!("{base}/v1/streams/{id}/topics/orders")).json(&json!({ "partitions": 2 }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(ok(c.get(format!("{base}/v1/streams/{id}/topics"))).await.as_array().unwrap().len(), 1);

    // Auto-create is off for this stream now.
    let (st, _) = status(c.post(format!("{base}/v1/streams/{id}/topics/missing/records")).json(&json!({ "value": 1 }))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Compacted topics need a key on every record.
    let (st, body) = status(c.post(format!("{base}/v1/streams/{id}/topics/orders/records")).json(&json!({ "value": "a" }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["offsets"][0]["code"], "invalid_record");
    ok(c.post(format!("{base}/v1/streams/{id}/topics/orders/records")).json(&json!([{ "key": "1", "value": "a" }, { "key": "2", "value": "b" }]))).await;
    let usage = ok(c.get(format!("{base}/v1/streams/{id}/usage?hours=1"))).await;
    assert!(usage["usage"].as_array().unwrap().iter().map(|r| r["records_in"].as_i64().unwrap()).sum::<i64>() >= 2);
    assert!(usage["estimate"]["msk_monthly_usd"]["total"].as_f64().unwrap() > 0.0);

    ok(c.delete(format!("{base}/v1/streams/{id}/topics/orders"))).await;
    let (st, _) = status(c.get(format!("{base}/v1/streams/{id}/topics/orders"))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    ok(c.delete(format!("{base}/v1/streams/{id}"))).await;
    let (st, _) = status(c.get(format!("{base}/v1/streams/{id}"))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(tb.broker.credential(username).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn produce_formats_and_partitioning() {
    let tb = TestBroker::start("http-produce").await;
    let (c, base) = (client(), format!("{}/v1/streams/test", tb.http));
    create_topic(&c, &tb.http, "test", "events", 4).await;
    let url = format!("{base}/topics/events/records");

    // One JSON record, with a key, headers and a timestamp.
    let r = ok(c.post(&url).json(&json!({ "key": "user-1", "value": { "n": 1 }, "headers": { "source": "web" }, "timestamp": 1_700_000_000_000i64 }))).await;
    let p = r["offsets"][0]["partition"].as_i64().unwrap() as i32;
    assert_eq!(p, partition_for_key(b"user-1", 4), "keyed records use Kafka's murmur2 partitioner");
    assert_eq!(r["offsets"][0]["timestamp"], 1_700_000_000_000i64);

    // An array, an envelope, NDJSON and a raw body.
    ok(c.post(&url).json(&json!([{ "value": "a", "partition": 0 }, { "value": "b", "partition": 0 }]))).await;
    ok(c.post(&url).json(&json!({ "records": [{ "value": "c", "partition": 0 }, { "value_base64": "/w==", "partition": 0 }] }))).await;
    ok(c.post(&url).header("content-type", "application/x-ndjson").body("{\"value\":\"d\",\"partition\":0}\n\n{\"value\":[1,2],\"partition\":0}\n")).await;
    ok(c.post(format!("{url}?partition=0&key=raw")).header("content-type", "text/plain").body("plain text body")).await;
    // curl -d sends form encoding; JSON-looking bodies are still JSON.
    ok(c.post(&url).header("content-type", "application/x-www-form-urlencoded").body("{\"value\":\"e\",\"partition\":0}")).await;

    let read = ok(c.get(format!("{base}/topics/events/partitions/0/records?offset=earliest"))).await;
    let vals = values(&read);
    let tail: Vec<Value> = vals.iter().rev().take(8).rev().cloned().collect();
    assert_eq!(tail, vec![json!("a"), json!("b"), json!("c"), json!("/w=="), json!("d"), json!([1, 2]), json!("plain text body"), json!("e")]);
    let recs = read["records"].as_array().unwrap();
    let binary = recs.iter().find(|r| r["value"] == "/w==").unwrap();
    assert_eq!(binary["value_encoding"], "base64", "non-UTF-8 values come back base64");
    let raw = recs.iter().find(|r| r["value"] == "plain text body").unwrap();
    assert_eq!(raw["key"], "raw");

    let keyed = ok(c.get(format!("{base}/topics/events/partitions/{p}/records"))).await;
    let rec = &keyed["records"][0];
    assert_eq!(rec["value"], json!({ "n": 1 }));
    assert_eq!(rec["headers"], json!([{ "key": "source", "value": "web" }]));
    assert_eq!(rec["timestamp"], 1_700_000_000_000i64);

    // Errors.
    let (st, body) = status(c.post(&url).json(&json!({ "value": 1, "partition": 9 }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    let (st, _) = status(c.post(&url).header("content-type", "application/json").body("{nope")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = status(c.post(format!("{url}?acks=2")).json(&json!({ "value": 1 }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = status(c.post(&url).json(&json!({ "value_base64": "!!" }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // acks=0 and 1 return offsets without waiting for fsync.
    let r = ok(c.post(format!("{url}?acks=0")).json(&json!({ "value": "fast", "partition": 1 }))).await;
    assert_eq!(r["offsets"][0]["offset"], 0);

    // Auto-create on produce (the test stream allows it).
    ok(c.post(format!("{base}/topics/brand-new/records")).json(&json!({ "value": 1 }))).await;
    assert!(tb.broker.find_stream("test").unwrap().topic("brand-new").is_some());

    // Large requests split into several batches under max.message.bytes.
    ok(c.patch(format!("{}/v1/streams/test/topics/events", tb.http)).json(&json!({ "configs": { "max.message.bytes": "10000" } }))).await;
    let big: Vec<Value> = (0..200).map(|i| json!({ "value": format!("{i:0>200}"), "partition": 2 })).collect();
    let r = ok(c.post(&url).json(&big)).await;
    let offs: Vec<i64> = r["offsets"].as_array().unwrap().iter().map(|o| o["offset"].as_i64().unwrap()).collect();
    assert_eq!(offs, (0..200).collect::<Vec<_>>());
    let read = ok(c.get(format!("{base}/topics/events/partitions/2/records?limit=1000"))).await;
    assert_eq!(read["records"].as_array().unwrap().len(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stateless_reads_offsets_timestamps_long_poll() {
    let tb = TestBroker::start("http-read").await;
    let (c, base) = (client(), format!("{}/v1/streams/test", tb.http));
    create_topic(&c, &tb.http, "test", "log", 1).await;
    let url = format!("{base}/topics/log/records");
    let recs: Vec<Value> = (0..100).map(|i| json!({ "value": i, "timestamp": 1_000_000 + i * 1000 })).collect();
    ok(c.post(&url).json(&recs)).await;
    let read = format!("{base}/topics/log/partitions/0/records");

    let r = ok(c.get(format!("{read}?offset=10&limit=5"))).await;
    assert_eq!(values(&r), (10..15).map(|i| json!(i.to_string())).collect::<Vec<_>>());
    assert_eq!(r["next_offset"], 15);
    assert_eq!(r["high_watermark"], 100);
    // Numbers are stored as their JSON text; `encoding=json` returns them as JSON.
    let r = ok(c.get(format!("{read}?offset=-3&encoding=json"))).await;
    assert_eq!(values(&r), vec![json!(97), json!(98), json!(99)]);
    let r = ok(c.get(format!("{read}?offset=latest"))).await;
    assert_eq!(r["records"].as_array().unwrap().len(), 0);
    assert_eq!(r["next_offset"], 100);
    let r = ok(c.get(format!("{read}?timestamp=1050500&limit=1"))).await;
    assert_eq!(r["records"][0]["offset"], 51);
    let r = ok(c.get(format!("{read}?timestamp=1970-01-01T00:17:30.000Z&limit=1"))).await;
    assert_eq!(r["records"][0]["offset"], 50);
    let r = ok(c.get(format!("{read}?offset=0&encoding=base64&limit=1"))).await;
    assert_eq!(r["records"][0]["value"], "MA==");

    let (st, body) = status(c.get(format!("{read}?offset=500"))).await;
    assert_eq!(st, StatusCode::RANGE_NOT_SATISFIABLE, "{body}");
    assert_eq!(body["high_watermark"], 100);
    let (st, _) = status(c.get(format!("{base}/topics/log/partitions/7/records"))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Long poll: the read parks until a record arrives.
    let c2 = c.clone();
    let url2 = url.clone();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        ok(c2.post(&url2).json(&json!({ "value": "late" }))).await;
    });
    let started = Instant::now();
    let r = ok(c.get(format!("{read}?offset=100&wait_ms=10000"))).await;
    assert_eq!(values(&r), vec![json!("late")]);
    assert!(started.elapsed() >= Duration::from_millis(300) && started.elapsed() < Duration::from_secs(5));
    writer.await.unwrap();
    // And returns empty when nothing arrives in time.
    let started = Instant::now();
    let r = ok(c.get(format!("{read}?offset=101&wait_ms=300"))).await;
    assert!(r["records"].as_array().unwrap().is_empty());
    assert!(started.elapsed() >= Duration::from_millis(250));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cursor_reads_cover_every_partition() {
    let tb = TestBroker::start("http-cursor").await;
    let (c, base) = (client(), format!("{}/v1/streams/test", tb.http));
    create_topic(&c, &tb.http, "test", "multi", 3).await;
    let url = format!("{base}/topics/multi/records");
    let recs: Vec<Value> = (0..90).map(|i| json!({ "key": format!("k{i}"), "value": i })).collect();
    ok(c.post(&url).json(&recs)).await;

    let mut seen = BTreeSet::new();
    let mut cursor: Option<String> = None;
    for _ in 0..20 {
        let q = match &cursor {
            Some(cur) => format!("{url}?limit=25&cursor={cur}"),
            None => format!("{url}?limit=25"),
        };
        let r = ok(c.get(q)).await;
        for v in values(&r) {
            assert!(seen.insert(v.as_str().unwrap().parse::<i64>().unwrap()), "no duplicates");
        }
        cursor = Some(r["cursor"].as_str().unwrap().to_string());
        if seen.len() == 90 {
            break;
        }
    }
    assert_eq!(seen, (0..90).collect());
    // Nothing more past the cursor; new partitions (added online) start from their beginning.
    let r = ok(c.get(format!("{url}?cursor={}", cursor.clone().unwrap()))).await;
    assert!(r["records"].as_array().unwrap().is_empty());
    ok(c.patch(format!("{}/v1/streams/test/topics/multi", tb.http)).json(&json!({ "partitions": 4 }))).await;
    ok(c.post(&url).json(&json!({ "value": "p3", "partition": 3 }))).await;
    let r = ok(c.get(format!("{url}?cursor={}", cursor.unwrap()))).await;
    assert_eq!(values(&r), vec![json!("p3")]);
    // `from=latest` and a partition filter.
    let r = ok(c.get(format!("{url}?from=latest"))).await;
    assert!(r["records"].as_array().unwrap().is_empty());
    let r = ok(c.get(format!("{url}?partitions=3"))).await;
    assert_eq!(values(&r), vec![json!("p3")]);
}

/// Read a group until `n` records arrive; returns values and the response's partitions.
async fn group_read(c: &Client, url: &str, n: usize) -> (Vec<i64>, Value) {
    let mut out = vec![];
    let mut last = Value::Null;
    let deadline = Instant::now() + T;
    while out.len() < n {
        assert!(Instant::now() < deadline, "got {} of {n}", out.len());
        let r = ok(c.get(url)).await;
        out.extend(values(&r).iter().map(|v| v.as_str().unwrap().parse::<i64>().unwrap()));
        last = r;
    }
    (out, last)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn managed_groups_share_partitions_and_redeliver() {
    let tb = TestBroker::start("http-groups").await;
    let (c, base) = (client(), format!("{}/v1/streams/test", tb.http));
    create_topic(&c, &tb.http, "test", "jobs", 4).await;
    let url = format!("{base}/topics/jobs/records");
    let recs: Vec<Value> = (0..40).map(|i| json!({ "value": i, "partition": i % 4 })).collect();
    ok(c.post(&url).json(&recs)).await;

    // Two consumers split the partitions.
    let a = ok(c.get(format!("{url}?group=workers&consumer=a&limit=1&wait_ms=100"))).await;
    let b = ok(c.get(format!("{url}?group=workers&consumer=b&limit=1&wait_ms=100"))).await;
    assert!(b["partitions"].as_array().unwrap().is_empty(), "a holds everything until its next read");
    // a's next read gives back half; b picks them up on its next read.
    let a2 = ok(c.get(format!("{url}?group=workers&consumer=a&limit=1&wait_ms=100"))).await;
    let b2 = ok(c.get(format!("{url}?group=workers&consumer=b&limit=1&wait_ms=100"))).await;
    let pa: BTreeSet<i64> = a2["partitions"].as_array().unwrap().iter().map(|p| p.as_i64().unwrap()).collect();
    let pb: BTreeSet<i64> = b2["partitions"].as_array().unwrap().iter().map(|p| p.as_i64().unwrap()).collect();
    assert_eq!(pa.len() + pb.len(), 4, "{pa:?} {pb:?}");
    assert!(pa.is_disjoint(&pb));
    assert_eq!(a["consumer"], "a");

    // Drain with both; everything arrives exactly once (each poll commits the previous one).
    let mut seen: BTreeMap<i64, usize> = BTreeMap::new();
    for r in [&a, &b, &a2, &b2] {
        for v in values(r) {
            *seen.entry(v.as_str().unwrap().parse().unwrap()).or_default() += 1;
        }
    }
    for _ in 0..30 {
        for who in ["a", "b"] {
            let r = ok(c.get(format!("{url}?group=workers&consumer={who}&limit=5"))).await;
            for v in values(&r) {
                *seen.entry(v.as_str().unwrap().parse().unwrap()).or_default() += 1;
            }
        }
    }
    assert_eq!(seen.keys().copied().collect::<Vec<_>>(), (0..40).collect::<Vec<_>>());
    assert!(seen.values().all(|n| *n == 1), "{seen:?}");
    // Leaving commits the last delivery.
    ok(c.delete(format!("{base}/groups/workers/consumers/a"))).await;
    ok(c.delete(format!("{base}/groups/workers/consumers/b"))).await;
    let g = ok(c.get(format!("{}/v1/streams/test/groups/workers", tb.http))).await;
    assert_eq!(g["lag"], 0, "{g}");

    // At-least-once: a consumer that disappears without polling again gets its records
    // redelivered to someone else after the lease.
    ok(c.post(&url).json(&json!({ "value": 100, "partition": 0 }))).await;
    let (got, _) = group_read(&c, &format!("{url}?group=workers&consumer=crashy&lease_ms=1000"), 1).await;
    assert_eq!(got, vec![100]);
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let (got, _) = group_read(&c, &format!("{url}?group=workers&consumer=rescuer"), 1).await;
    assert_eq!(got, vec![100]);

    // Manual commits.
    ok(c.post(&url).json(&json!({ "value": 200, "partition": 1 }))).await;
    let (got, last) = group_read(&c, &format!("{url}?group=manual&commit=manual"), 41).await;
    assert!(got.contains(&200));
    let consumer = last["consumer"].as_str().unwrap().to_string();
    assert!(consumer.starts_with("http-"));
    let commit = last["commit"].clone();
    ok(c.post(format!("{base}/groups/manual/commit")).json(&json!({ "consumer": consumer, "offsets": commit }))).await;
    let g = ok(c.get(format!("{}/v1/streams/test/groups/manual", tb.http))).await;
    assert_eq!(g["lag"], 0, "{g}");
    assert_eq!(g["http_consumers"].as_array().unwrap().len(), 1);

    // Offsets are visible to Kafka clients (and resettable while inactive).
    let addr = tb.addr.clone();
    let committed = tokio::task::spawn_blocking(move || {
        let k: BaseConsumer = ClientConfig::new().set("bootstrap.servers", &addr).set("group.id", "manual").create().unwrap();
        let mut tpl = TopicPartitionList::new();
        for p in 0..4 {
            tpl.add_partition("jobs", p);
        }
        let got = k.committed_offsets(tpl, T).unwrap();
        got.elements().iter().map(|e| e.offset().to_raw().unwrap()).sum::<i64>()
    })
    .await
    .unwrap();
    assert_eq!(committed, 42);
    let (st, _) = status(c.post(format!("{}/v1/streams/test/groups/manual/reset", tb.http)).json(&json!({ "topic": "jobs", "to": "earliest" }))).await;
    assert_eq!(st, StatusCode::CONFLICT, "active HTTP consumers block resets");
    ok(c.delete(format!("{base}/groups/manual/consumers/{consumer}"))).await;
    let r = ok(c.post(format!("{}/v1/streams/test/groups/manual/reset", tb.http)).json(&json!({ "topic": "jobs", "to": "earliest" }))).await;
    assert_eq!(r["lag"], 42);
    let groups = ok(c.get(format!("{}/v1/streams/test/groups", tb.http))).await;
    assert!(groups.as_array().unwrap().iter().any(|g| g["group_id"] == "manual"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_tail_streams_new_records() {
    let tb = TestBroker::start("http-sse").await;
    let (c, base) = (client(), format!("{}/v1/streams/test", tb.http));
    create_topic(&c, &tb.http, "test", "live", 2).await;
    ok(c.post(format!("{base}/topics/live/records")).json(&json!({ "value": "old" }))).await;
    let mut resp = c.get(format!("{base}/topics/live/tail")).send().await.unwrap();
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let c2 = c.clone();
    let url = format!("{base}/topics/live/records");
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        for i in 0..5 {
            ok(c2.post(&url).json(&json!({ "value": format!("new-{i}"), "partition": i % 2 }))).await;
        }
    });
    let mut buf = String::new();
    let deadline = Instant::now() + T;
    while buf.matches("event: record").count() < 5 {
        assert!(Instant::now() < deadline, "{buf}");
        let chunk = tokio::time::timeout(T, resp.chunk()).await.unwrap().unwrap().unwrap();
        buf.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(!buf.contains("\"old\""), "tail starts at the end");
    for i in 0..5 {
        assert!(buf.contains(&format!("new-{i}")), "{buf}");
    }
    let last_id = buf.lines().rev().find_map(|l| l.strip_prefix("id: ")).unwrap().to_string();
    drop(resp);

    // Resume with Last-Event-ID: nothing is replayed, new records arrive.
    ok(c.post(format!("{base}/topics/live/records")).json(&json!({ "value": "after", "partition": 0 }))).await;
    let mut resp = c.get(format!("{base}/topics/live/tail")).header("last-event-id", last_id).send().await.unwrap();
    let mut buf = String::new();
    while !buf.contains("after") {
        let chunk = tokio::time::timeout(T, resp.chunk()).await.unwrap().unwrap().unwrap();
        buf.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(!buf.contains("new-0"), "{buf}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn api_keys_guard_the_data_plane() {
    let tb = TestBroker::start_with("http-auth", |c| c.http_require_auth = true).await;
    let (other, ocred, osecret) = tb.broker.create_stream("other", Default::default()).unwrap();
    let c = client();
    let url = format!("{}/v1/streams/test/topics/sec/records", tb.http);
    let (st, _) = status(c.post(&url).json(&json!({ "value": 1 }))).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _) = status(c.post(&url).bearer_auth(format!("{}:wrong", tb.username)).json(&json!({ "value": 1 }))).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _) = status(c.post(&url).bearer_auth(format!("{}:{osecret}", ocred.username)).json(&json!({ "value": 1 }))).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "keys only work for their own stream");
    ok(c.post(&url).bearer_auth(format!("{}:{}", tb.username, tb.secret)).json(&json!({ "value": 1 }))).await;
    let r = ok(c.get(format!("{}/v1/streams/test/topics/sec/partitions/0/records", tb.http)).basic_auth(&tb.username, Some(&tb.secret))).await;
    assert_eq!(values(&r), vec![json!("1")]);
    let (st, _) = status(c.get(format!("{}/kafka/v3/clusters", tb.http))).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let clusters = ok(c.get(format!("{}/kafka/v3/clusters", tb.http)).basic_auth(&ocred.username, Some(&osecret))).await;
    assert_eq!(clusters["data"].as_array().unwrap().len(), 1);
    assert_eq!(clusters["data"][0]["cluster_id"], other.cluster_id.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confluent_rest_v3() {
    let tb = TestBroker::start("http-v3").await;
    let c = client();
    let base = format!("{}/kafka/v3", tb.http);
    let clusters = ok(c.get(format!("{base}/clusters"))).await;
    assert_eq!(clusters["kind"], "KafkaClusterList");
    let cid = clusters["data"][0]["cluster_id"].as_str().unwrap().to_string();
    assert_eq!(cid, tb.broker.find_stream("test").unwrap().cluster_id);

    let t = ok(c.post(format!("{base}/clusters/{cid}/topics")).json(&json!({ "topic_name": "v3", "partitions_count": 2, "configs": [{ "name": "retention.ms", "value": "60000" }] }))).await;
    assert_eq!(t["partitions_count"], 2);
    assert_eq!(ok(c.get(format!("{base}/clusters/{cid}/topics"))).await["data"].as_array().unwrap().len(), 1);
    assert_eq!(ok(c.get(format!("{base}/clusters/{cid}/topics/v3/partitions"))).await["data"].as_array().unwrap().len(), 2);

    let r = ok(c
        .post(format!("{base}/clusters/{cid}/topics/v3/records"))
        .json(&json!({ "partition_id": 1, "key": { "type": "STRING", "data": "k" }, "value": { "type": "JSON", "data": { "a": 1 } }, "headers": [{ "name": "h", "value": "aGk=" }], "timestamp": "2021-02-05T19:14:42Z" })))
    .await;
    assert_eq!(r["error_code"], 200);
    assert_eq!(r["partition_id"], 1);
    assert_eq!(r["offset"], 0);
    assert_eq!(r["timestamp"], "2021-02-05T19:14:42.000Z");
    assert_eq!(r["value"], json!({ "type": "JSON", "size": 7 }));

    // Streaming mode: several records in one body, one result each.
    let body = r#"{"partition_id":0,"value":{"type":"BINARY","data":"AAE="}}
{"partition_id":0,"value":{"type":"STRING","data":"two"}}
{"partition_id":0,"value":{"type":"AVRO","data":{}}}"#;
    let resp = c.post(format!("{base}/clusters/{cid}/topics/v3/records")).header("content-type", "application/json").body(body).send().await.unwrap();
    assert!(resp.status().is_success());
    let lines: Vec<Value> = resp.text().await.unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 3);
    assert_eq!((lines[0]["offset"].clone(), lines[1]["offset"].clone()), (json!(0), json!(1)));
    assert_eq!(lines[2]["error_code"], 400);

    let read = ok(c.get(format!("{}/v1/streams/test/topics/v3/partitions/1/records", tb.http))).await;
    assert_eq!(read["records"][0]["key"], "k");
    assert_eq!(read["records"][0]["value"], json!({ "a": 1 }));
    assert_eq!(read["records"][0]["headers"][0]["value"], "hi");

    let (st, body) = status(c.post(format!("{base}/clusters/{cid}/topics/missing/records")).json(&json!({ "value": { "type": "JSON", "data": 1 } }))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["error_code"], 404);
    ok(c.delete(format!("{base}/clusters/{cid}/topics/v3"))).await;
    let (st, _) = status(c.get(format!("{base}/clusters/{cid}/topics/v3"))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_and_kafka_clients_share_the_log() {
    let tb = TestBroker::start("http-interop").await;
    let c = client();
    let base = format!("{}/v1/streams/test", tb.http);
    create_topic(&c, &tb.http, "test", "shared", 3).await;
    // HTTP in, Kafka out: keys land where a Kafka client would expect them.
    let recs: Vec<Value> = (0..30).map(|i| json!({ "key": format!("key-{i}"), "value": { "i": i } })).collect();
    ok(c.post(format!("{base}/topics/shared/records")).json(&recs)).await;

    let addr = tb.addr.clone();
    let got = tokio::task::spawn_blocking(move || {
        let k: BaseConsumer = ClientConfig::new()
            .set("bootstrap.servers", &addr)
            .set("group.id", "interop")
            .set("auto.offset.reset", "earliest")
            .create()
            .unwrap();
        k.subscribe(&["shared"]).unwrap();
        let mut out = vec![];
        let deadline = Instant::now() + T;
        while out.len() < 30 && Instant::now() < deadline {
            if let Some(Ok(m)) = k.poll(Duration::from_millis(200)) {
                out.push((m.partition(), String::from_utf8_lossy(m.key().unwrap()).into_owned(), String::from_utf8_lossy(m.payload().unwrap()).into_owned()));
            }
        }
        // Kafka in, HTTP out (transactional, with an aborted transaction hidden).
        let p: ThreadedProducer<DefaultProducerContext> = ClientConfig::new().set("bootstrap.servers", &addr).set("transactional.id", "interop-tx").create().unwrap();
        p.init_transactions(T).unwrap();
        p.begin_transaction().unwrap();
        p.send(BaseRecord::to("shared").partition(0).key("tx").payload("aborted")).map_err(|e| e.0).unwrap();
        p.flush(T).unwrap();
        p.abort_transaction(T).unwrap();
        p.begin_transaction().unwrap();
        p.send(BaseRecord::to("shared").partition(0).key("tx").payload("committed")).map_err(|e| e.0).unwrap();
        p.commit_transaction(T).unwrap();
        out
    })
    .await
    .unwrap();
    assert_eq!(got.len(), 30);
    for (p, k, v) in &got {
        assert_eq!(*p, partition_for_key(k.as_bytes(), 3), "{k}");
        let i: i64 = k.strip_prefix("key-").unwrap().parse().unwrap();
        assert_eq!(serde_json::from_str::<Value>(v).unwrap(), json!({ "i": i }));
    }
    let r = ok(c.get(format!("{base}/topics/shared/partitions/0/records?offset=earliest&limit=1000"))).await;
    let vals = values(&r);
    assert!(vals.contains(&json!("committed")));
    assert!(!vals.contains(&json!("aborted")), "read_committed hides aborted data");
    let r = ok(c.get(format!("{base}/topics/shared/partitions/0/records?offset=earliest&limit=1000&isolation=read_uncommitted"))).await;
    assert!(values(&r).contains(&json!("aborted")));
}
