//! The ScyllaDB REST API subset, and (with `ROLL_NOSQL_REF_SCYLLA_REST=127.0.0.1:10000`) the
//! shapes of its answers against the reference's.

mod common;

use std::sync::Arc;

use common::*;
use nosql_engine::Dialect;
use nosql_proto::QueryParams;
use nosql_server::Directory;
use serde_json::{Value, json};

struct Rest {
    base: String,
    http: reqwest::Client,
    s: SingleServer,
}

async fn start(dialect: Dialect) -> Rest {
    let s = single(dialect).await;
    let c = connect(s.addr, nosql_proto::Version::V4, None).await;
    let q = QueryParams::default;
    c.query("CREATE KEYSPACE srv_rest WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}", q()).await.unwrap();
    c.query("CREATE TABLE srv_rest.t (k int PRIMARY KEY, v text)", q()).await.unwrap();
    c.query("INSERT INTO srv_rest.t (k, v) VALUES (1, 'a')", q()).await.unwrap();
    c.query("INSERT INTO srv_rest.t (k, v) VALUES (2, 'b')", q()).await.unwrap();
    let dir: Arc<dyn Directory> = s.dir.clone();
    let addr = nosql_server::rest::start(dir).await.unwrap();
    Rest { base: format!("http://{addr}"), http: reqwest::Client::new(), s }
}

impl Rest {
    async fn call(&self, method: reqwest::Method, path: &str) -> (u16, String) {
        let resp = self.http.request(method, format!("{}{path}", self.base)).send().await.unwrap();
        let st = resp.status().as_u16();
        assert_eq!(resp.headers()["content-type"], "application/json", "{path}");
        (st, resp.text().await.unwrap())
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let (st, body) = self.call(reqwest::Method::GET, path).await;
        (st, serde_json::from_str(&body).unwrap_or_else(|e| panic!("{path}: {e}: {body}")))
    }

    async fn post(&self, path: &str) -> (u16, String) {
        self.call(reqwest::Method::POST, path).await
    }

    async fn delete(&self, path: &str) -> (u16, String) {
        self.call(reqwest::Method::DELETE, path).await
    }
}

fn err(code: u16, msg: &str) -> String {
    json!({ "message": msg, "code": code }).to_string()
}

#[tokio::test]
async fn node_identity_and_state() {
    let r = start(Dialect::Scylla).await;
    let host = nosql_types::timeuuid::format_uuid(&r.s.db.config().host_id());
    assert_eq!(r.get("/storage_service/host_id").await, (200, json!([{ "key": "127.0.0.1", "value": host }])));
    assert_eq!(r.get("/storage_service/hostid/local").await, (200, json!(host)));
    assert_eq!(r.get("/storage_service/release_version").await, (200, json!("3.0.8")));
    assert_eq!(r.get("/storage_service/scylla_release_version").await.1, json!(nosql_server::rest::SCYLLA_RELEASE_VERSION));
    let (_, v) = r.get("/storage_service/schema_version").await;
    assert_eq!(v.as_str().unwrap().len(), 36);
    assert_eq!(r.get("/storage_service/cluster_name").await.1, json!(""));
    assert_eq!(r.get("/storage_service/partitioner_name").await.1, json!("org.apache.cassandra.dht.Murmur3Partitioner"));
    for (p, v) in [
        ("/storage_service/operation_mode", json!("NORMAL")),
        ("/storage_service/is_starting", json!(false)),
        ("/storage_service/is_initialized", json!(true)),
        ("/storage_service/native_transport", json!(true)),
        ("/storage_service/gossiping", json!(true)),
        ("/storage_service/rpc_server", json!(false)),
        ("/storage_service/nodes/leaving", json!([])),
        ("/storage_service/nodes/joining", json!([])),
        ("/storage_service/nodes/moving", json!([])),
        ("/storage_service/removal_status", json!("No token removals in process.")),
        ("/gossiper/endpoint/live/", json!(["127.0.0.1"])),
        ("/gossiper/endpoint/down/", json!([])),
        ("/failure_detector/simple_states", json!([{ "key": "127.0.0.1", "value": "UP" }])),
        ("/failure_detector/count/endpoint/up", json!(1)),
        ("/snitch/datacenter", json!("datacenter1")),
        ("/snitch/rack", json!("rack1")),
        ("/snitch/name", json!("org.apache.cassandra.locator.SimpleSnitch")),
        ("/compaction_manager/compactions", json!([])),
        ("/compaction_manager/metrics/pending_tasks", json!(0)),
        ("/storage_service/slow_query", json!({ "enable": false, "ttl": 86400, "threshold": 500000, "fast": false })),
    ] {
        assert_eq!(r.get(p).await, (200, v), "{p}");
    }
    assert!(r.get("/system/uptime_ms").await.1.is_u64());
    assert!(r.get("/storage_service/load").await.1.is_u64());
    // Keyspaces by type.
    assert_eq!(r.get("/storage_service/keyspaces?type=user").await.1, json!(["srv_rest"]));
    let (_, all) = r.get("/storage_service/keyspaces").await;
    assert!(all.as_array().unwrap().contains(&json!("system")) && all.as_array().unwrap().contains(&json!("srv_rest")));
    let (_, nl) = r.get("/storage_service/keyspaces?type=non_local_strategy").await;
    assert!(!nl.as_array().unwrap().contains(&json!("system")) && nl.as_array().unwrap().contains(&json!("srv_rest")));
    // The ring: ScyllaDB's 256 tokens.
    let (_, tokens) = r.get("/storage_service/tokens").await;
    let tokens: Vec<i64> = tokens.as_array().unwrap().iter().map(|t| t.as_str().unwrap().parse().unwrap()).collect();
    assert_eq!(tokens.len(), 256);
    assert!(tokens.windows(2).all(|w| w[0] < w[1]));
    let (_, te) = r.get("/storage_service/tokens_endpoint").await;
    assert_eq!(te[0], json!({ "key": tokens[0].to_string(), "value": "127.0.0.1" }));
    let (_, ring) = r.get("/storage_service/describe_ring/srv_rest").await;
    let ring = ring.as_array().unwrap();
    assert_eq!(ring.len(), 256);
    assert_eq!(ring[0]["start_token"], json!(tokens[255].to_string()));
    assert_eq!(ring[0]["end_token"], json!(tokens[0].to_string()));
    assert_eq!(ring[0]["endpoint_details"], json!([{ "host": "127.0.0.1", "datacenter": "datacenter1", "rack": "rack1" }]));
    assert_eq!(r.call(reqwest::Method::GET, "/storage_service/describe_ring/nope").await, (400, err(400, "Can't find a keyspace nope")));
    assert_eq!(r.get("/storage_service/natural_endpoints/srv_rest?cf=t&key=1").await.1, json!(["127.0.0.1"]));
    assert_eq!(
        r.call(reqwest::Method::GET, "/storage_service/ownership/system_schema").await,
        (500, err(500, "std::runtime_error (Ownership values for keyspaces with LocalStrategy are meaningless)"))
    );
    assert_eq!(r.call(reqwest::Method::GET, "/nonexistent").await, (404, err(404, "Not found")));
    assert_eq!(r.call(reqwest::Method::POST, "/storage_service/host_id").await, (404, err(404, "Not found")));
    // Cassandra's release.
    let c = start(Dialect::Cassandra).await;
    assert_eq!(c.get("/storage_service/release_version").await.1, json!("5.0.9"));
}

#[tokio::test]
async fn flush_compact_and_table_stats() {
    let r = start(Dialect::Scylla).await;
    assert_eq!(r.get("/column_family/metrics/memtable_columns_count/srv_rest:t").await.1, json!(2));
    assert_eq!(r.get("/column_family/metrics/live_ss_table_count/srv_rest:t").await.1, json!(0));
    assert_eq!(r.get("/column_family/sstables/per_level/srv_rest:t").await.1, json!([]));
    assert!(r.get("/column_family/metrics/write/srv_rest:t").await.1.as_u64().unwrap() >= 2);
    assert_eq!(r.post("/storage_service/keyspace_flush/srv_rest").await, (200, String::new()));
    assert_eq!(r.post("/storage_service/keyspace_flush/srv_rest?cf=t").await, (200, String::new()));
    assert_eq!(r.post("/storage_service/keyspace_flush/srv_rest?cf=nope").await, (400, err(400, "Can't find a column family nope in keyspace srv_rest")));
    assert_eq!(r.post("/storage_service/keyspace_flush/nope").await, (400, err(400, "Can't find a keyspace nope")));
    assert_eq!(r.get("/column_family/metrics/memtable_columns_count/srv_rest:t").await.1, json!(0));
    assert!(r.get("/column_family/metrics/live_ss_table_count/srv_rest:t").await.1.as_u64().unwrap() >= 1);
    assert!(r.get("/column_family/metrics/live_disk_space_used/srv_rest:t").await.1.as_u64().unwrap() > 0);
    assert!(!r.get("/column_family/sstables/per_level/srv_rest:t").await.1.as_array().unwrap().is_empty());
    assert_eq!(r.post("/storage_service/keyspace_compaction/srv_rest?cf=t").await, (200, String::new()));
    assert_eq!(r.post("/storage_service/keyspace_compaction/nope").await, (400, err(400, "Can't find a keyspace nope")));
    assert_eq!(r.post("/storage_service/compact").await.0, 200);
    assert_eq!(r.post("/storage_service/flush").await.0, 200);
    assert_eq!(r.post("/storage_service/keyspace_cleanup/srv_rest").await, (200, "0".into()));
    assert_eq!(r.post("/column_family/major_compaction/srv_rest:t").await.0, 200);
    // Data is intact.
    let c = connect(r.s.addr, nosql_proto::Version::V4, None).await;
    let res = c.query("SELECT k FROM srv_rest.t", QueryParams::default()).await.unwrap();
    let nosql_proto::ResultBody::Rows(rows) = res else { panic!() };
    assert_eq!(rows.len(), 2);
    // Table listings.
    let (_, cfs) = r.get("/column_family/").await;
    assert!(cfs.as_array().unwrap().contains(&json!({ "ks": "srv_rest", "cf": "t", "type": "ColumnFamilies" })));
    let (_, names) = r.get("/column_family/name").await;
    assert!(names.as_array().unwrap().contains(&json!("srv_rest:t")));
    assert!(r.get("/column_family/name/keyspace").await.1.as_array().unwrap().contains(&json!("srv_rest")));
    // Name errors.
    assert_eq!(
        r.call(reqwest::Method::GET, "/column_family/metrics/memtable_columns_count/srv_rest").await,
        (400, err(400, "Column family name should be in keyspace:column_family format"))
    );
    assert_eq!(
        r.call(reqwest::Method::GET, "/column_family/metrics/memtable_columns_count/nope:t").await,
        (400, err(400, "Can't find a column family t in keyspace nope"))
    );
    // Histogram shapes.
    assert_eq!(
        r.get("/column_family/metrics/read_latency/histogram/srv_rest:t").await.1,
        json!({ "count": 0, "sum": 0, "min": 0, "max": 0, "variance": 0, "mean": 0 })
    );
    let (_, h) = r.get("/column_family/metrics/read_latency/moving_average_histogram/srv_rest:t").await;
    assert_eq!(h["meter"], json!({ "rates": [0, 0, 0], "mean_rate": 0, "count": 0 }));
    let (_, h) = r.get("/column_family/metrics/read_latency/estimated_histogram/srv_rest:t").await;
    assert_eq!((h["buckets"].as_array().unwrap().len(), h["bucket_offsets"].as_array().unwrap().len()), (65, 65));
    let (_, h) = r.get("/column_family/metrics/sstables_per_read_histogram/srv_rest:t").await;
    assert_eq!((h["buckets"].as_array().unwrap().len(), h["bucket_offsets"].as_array().unwrap().len()), (36, 35));
    assert_eq!(r.get("/column_family/metrics/estimated_row_size_histogram/srv_rest:t").await.1, json!({ "buckets": [0] }));
    assert_eq!(r.call(reqwest::Method::GET, "/column_family/metrics/key_cache_hit_rate/srv_rest:t").await, (500, err(500, "API call is not supported yet")));
    assert_eq!(r.get("/column_family/autocompaction/srv_rest:t").await.1, json!(true));
    assert_eq!(r.post("/column_family/autocompaction/srv_rest:t").await, (200, String::new()));
    let (_, strategy) = r.get("/column_family/compaction_strategy/srv_rest:t").await;
    assert!(strategy.as_str().unwrap().ends_with("CompactionStrategy"), "{strategy}");
    assert_eq!(r.get("/column_family/built_indexes/srv_rest:t").await.1, json!([]));
    // Node-wide totals.
    assert!(r.get("/column_family/metrics/live_ss_table_count").await.1.is_u64());
    assert!(r.get("/column_family/metrics/read_latency/histogram/").await.1.is_object());
}

#[tokio::test]
async fn snapshots_and_settings() {
    let r = start(Dialect::Scylla).await;
    assert_eq!(r.post("/storage_service/snapshots?kn=srv_rest").await, (500, err(500, "std::runtime_error (You must supply a snapshot name.)")));
    assert_eq!(r.post("/storage_service/snapshots?tag=x&kn=nope").await, (400, err(400, "Can't find a keyspace nope")));
    assert_eq!(r.post("/storage_service/snapshots?tag=s1&kn=srv_rest").await, (200, String::new()));
    assert_eq!(
        r.post("/storage_service/snapshots?tag=s1&kn=srv_rest").await,
        (500, err(500, "std::runtime_error (Keyspace srv_rest: snapshot s1 already exists.)"))
    );
    assert_eq!(
        r.get("/storage_service/snapshots").await.1,
        json!([{ "key": "s1", "value": [{ "ks": "srv_rest", "cf": "t", "total": 0, "live": 0 }] }])
    );
    assert_eq!(r.delete("/storage_service/snapshots?tag=s1&kn=srv_rest").await, (200, String::new()));
    assert_eq!(r.delete("/storage_service/snapshots?tag=never").await.0, 200);
    assert_eq!(r.get("/storage_service/snapshots").await.1, json!([]));
    // Trace probability.
    assert_eq!(r.post("/storage_service/trace_probability?probability=2").await, (400, err(400, "trace probability must be in a [0,1] range")));
    assert_eq!(r.post("/storage_service/trace_probability").await, (400, err(400, "Missing mandatory parameter 'probability'")));
    assert_eq!(r.post("/storage_service/trace_probability?probability=0.25").await.0, 200);
    assert_eq!(r.get("/storage_service/trace_probability").await.1, json!(0.25));
    assert_eq!(r.s.db.trace_probability(), 0.25, "the engine traces that share of requests");
    assert_eq!(r.post("/storage_service/trace_probability?probability=abc").await, (400, err(400, "Bad format in a probability value: \"abc\"")));
    assert_eq!(r.post("/storage_service/trace_probability?probability=0").await.0, 200);
    // The slow query log: each given parameter changes its setting (as the reference parses
    // them: `stol`, `true`), and the engine logs by it.
    let defaults = json!({ "enable": false, "ttl": 86400, "threshold": 500000, "fast": false });
    assert_eq!(r.get("/storage_service/slow_query").await.1, defaults);
    assert_eq!(r.post("/storage_service/slow_query?enable=true&ttl=600&threshold=0&fast=true").await, (200, String::new()));
    assert_eq!(r.get("/storage_service/slow_query").await.1, json!({ "enable": true, "ttl": 600, "threshold": 0, "fast": true }));
    assert_eq!(r.s.db.slow_query_log(), nosql_engine::trace::SlowQueryLog { enable: true, ttl: 600, threshold: 0, fast: true });
    let c = connect(r.s.addr, nosql_proto::Version::V4, None).await;
    c.query("SELECT v FROM srv_rest.t WHERE k = 1", QueryParams::default()).await.unwrap();
    let slow = c.query("SELECT command FROM system_traces.node_slow_log", QueryParams::default()).await.unwrap();
    let nosql_proto::ResultBody::Rows(slow) = slow else { panic!("rows") };
    assert!(slow.to_vec().iter().any(|row| row[0].as_deref() == Some(&b"SELECT v FROM srv_rest.t WHERE k = 1"[..])));
    assert_eq!(r.post("/storage_service/slow_query?ttl=12abc&enable=maybe&fast=").await.0, 200);
    assert_eq!(r.get("/storage_service/slow_query").await.1, json!({ "enable": false, "ttl": 12, "threshold": 0, "fast": true }));
    assert_eq!(r.post("/storage_service/slow_query?ttl=abc").await, (500, err(500, "std::invalid_argument (stol)")));
    assert_eq!(r.post("/storage_service/slow_query?threshold=500000&ttl=86400&fast=false").await.0, 200);
    assert_eq!(r.get("/storage_service/slow_query").await.1, defaults);
    assert_eq!(r.post("/storage_service/compaction_throughput?value=16").await.0, 200);
    assert_eq!(r.get("/storage_service/compaction_throughput").await.1, json!(16));
    assert_eq!(r.get("/storage_service/stream_throughput").await.1, json!(0));
    assert_eq!(r.call(reqwest::Method::GET, "/storage_service/tombstone_warn_threshold").await, (500, err(500, "API call is not supported yet")));
}

/// Snapshots outlive the database's unload: still listed (and refused again) when it comes
/// back, cleared from object storage after.
#[tokio::test]
async fn snapshots_survive_unloads() {
    let n = node(nosql_server::NodeConfig { default_db: Some("n1".into()), ..Default::default() }, nosql_server::ListenOptions::default()).await;
    n.catalog.insert(nosql_server::DbSpec::new("n1", Dialect::Scylla));
    let c = connect(n.addr, nosql_proto::Version::V4, None).await;
    let q = QueryParams::default;
    c.query("CREATE KEYSPACE srv_snap WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}", q()).await.unwrap();
    c.query("CREATE TABLE srv_snap.t (k int PRIMARY KEY, v text)", q()).await.unwrap();
    c.query("CREATE TABLE srv_snap.u (k int PRIMARY KEY)", q()).await.unwrap();
    c.query("INSERT INTO srv_snap.t (k, v) VALUES (1, 'a')", q()).await.unwrap();
    drop(c);
    let dir: Arc<dyn Directory> = n.node.clone();
    let base = format!("http://{}", nosql_server::rest::start(dir).await.unwrap());
    let http = reqwest::Client::new();
    let call = |m: reqwest::Method, path: &str| {
        let req = http.request(m, format!("{base}{path}"));
        async move {
            let r = req.send().await.unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }
    };
    let list = || async { serde_json::from_str::<Value>(&call(reqwest::Method::GET, "/storage_service/snapshots").await.1).unwrap() };
    assert_eq!(call(reqwest::Method::POST, "/storage_service/snapshots?tag=s1&kn=srv_snap&cf=t").await, (200, String::new()));
    assert_eq!(call(reqwest::Method::POST, "/storage_service/snapshots?tag=all").await.0, 200);
    let both = json!([
        { "key": "all", "value": [{ "ks": "srv_snap", "cf": "t", "total": 0, "live": 0 }, { "ks": "srv_snap", "cf": "u", "total": 0, "live": 0 }] },
        { "key": "s1", "value": [{ "ks": "srv_snap", "cf": "t", "total": 0, "live": 0 }] },
    ]);
    assert_eq!(list().await, both);
    assert!(n.node.evict("n1").await.unwrap());
    assert!(n.node.get_loaded("n1").is_none());
    // The next request loads the database again; its snapshots are still there.
    assert_eq!(list().await, both);
    assert_eq!(
        call(reqwest::Method::POST, "/storage_service/snapshots?tag=s1&kn=srv_snap").await,
        (500, err(500, "std::runtime_error (Keyspace srv_snap: snapshot s1 already exists.)"))
    );
    let c = connect(n.addr, nosql_proto::Version::V4, None).await;
    let r = c.query("SELECT v FROM srv_snap.t WHERE k = 1", q()).await.unwrap();
    let nosql_proto::ResultBody::Rows(r) = r else { panic!("rows") };
    assert_eq!(r.to_vec(), vec![vec![Some(text("a"))]]);
    drop(c);
    assert_eq!(call(reqwest::Method::DELETE, "/storage_service/snapshots?tag=s1").await, (200, String::new()));
    assert!(n.node.evict("n1").await.unwrap());
    assert_eq!(list().await, json!([both[0]]));
    // Without a tag: every snapshot, and nothing of them stays in object storage.
    assert_eq!(call(reqwest::Method::DELETE, "/storage_service/snapshots").await.0, 200);
    assert_eq!(list().await, json!([]));
    for prefix in ["nosql/db/n1/snapshots/", "nosql/db/n1/snapshot-info/"] {
        assert_eq!(n.store.objects().list(prefix).await.unwrap(), vec![], "{prefix}");
    }
}

/// The structure of a JSON value: object keys with the shapes of their values, the shape of
/// an array's first element, and scalar kinds.
fn shape(v: &Value) -> Value {
    match v {
        Value::Null => json!("null"),
        Value::Bool(_) => json!("bool"),
        Value::Number(_) => json!("number"),
        Value::String(_) => json!("string"),
        Value::Array(a) => json!([a.first().map(shape)]),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), shape(v))).collect()),
    }
}

/// Same JSON shape, where an empty array matches any array: lists of running work (active
/// compactions, pending tasks) depend on what the server happens to be doing.
fn same_shape(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Array(x), Value::Array(y)) if x.is_empty() || y.is_empty() => true,
        (Value::Array(x), Value::Array(y)) => same_shape(&x[0], &y[0]),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same_shape(v, w)))
        }
        _ => shape(a) == shape(b),
    }
}

/// Every endpoint answers with the reference's status and JSON shape.
#[tokio::test]
async fn shapes_match_the_reference() {
    let Ok(reference) = std::env::var("ROLL_NOSQL_REF_SCYLLA_REST") else {
        eprintln!("skipped: set ROLL_NOSQL_REF_SCYLLA_REST=127.0.0.1:10000");
        return;
    };
    let r = start(Dialect::Scylla).await;
    // The reference gets the same keyspace (dropped at the end).
    let ref_cql = std::env::var("ROLL_NOSQL_REF_SCYLLA").unwrap_or_else(|_| "127.0.0.1:19042".into());
    let rc = nosql_proto::client::Client::connect(nosql_proto::client::ClientConfig::new(ref_cql)).await.unwrap();
    let q = QueryParams::default;
    let _ = rc.query("DROP KEYSPACE IF EXISTS srv_rest_shape", q()).await;
    rc.query("CREATE KEYSPACE srv_rest_shape WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1} AND tablets = {'enabled': false}", q())
        .await
        .unwrap();
    rc.query("CREATE TABLE srv_rest_shape.t (k int PRIMARY KEY, v text)", q()).await.unwrap();
    let rq = connect(r.s.addr, nosql_proto::Version::V4, None).await;
    rq.query("CREATE KEYSPACE srv_rest_shape WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}", q()).await.unwrap();
    rq.query("CREATE TABLE srv_rest_shape.t (k int PRIMARY KEY, v text)", q()).await.unwrap();
    let http = reqwest::Client::new();
    let paths = [
        "/storage_service/host_id",
        "/storage_service/hostid/local",
        "/storage_service/release_version",
        "/storage_service/scylla_release_version",
        "/storage_service/schema_version",
        "/storage_service/cluster_name",
        "/storage_service/partitioner_name",
        "/storage_service/operation_mode",
        "/storage_service/is_starting",
        "/storage_service/gossiping",
        "/storage_service/native_transport",
        "/storage_service/rpc_server",
        "/storage_service/join_ring",
        "/storage_service/nodes/leaving",
        "/storage_service/tokens",
        "/storage_service/tokens_endpoint",
        "/storage_service/load",
        "/storage_service/load_map",
        "/storage_service/generation_number",
        "/storage_service/keyspaces",
        "/storage_service/describe_ring/srv_rest_shape",
        "/storage_service/describe_ring/nope",
        "/storage_service/natural_endpoints/srv_rest_shape?cf=t&key=1",
        "/storage_service/trace_probability",
        "/storage_service/compaction_throughput",
        "/storage_service/slow_query",
        "/storage_service/removal_status",
        "/storage_service/drain",
        "/gossiper/endpoint/live/",
        "/gossiper/endpoint/down/",
        "/failure_detector/simple_states",
        "/failure_detector/count/endpoint/up",
        "/snitch/datacenter",
        "/snitch/rack",
        "/snitch/name",
        "/system/uptime_ms",
        "/compaction_manager/compactions",
        "/compaction_manager/metrics/pending_tasks",
        "/column_family/",
        "/column_family/name",
        "/column_family/metrics/memtable_columns_count/srv_rest_shape:t",
        "/column_family/metrics/memtable_columns_count/srv_rest_shape",
        "/column_family/metrics/memtable_columns_count/nope:t",
        "/column_family/metrics/live_ss_table_count/srv_rest_shape:t",
        "/column_family/metrics/read_latency/histogram/srv_rest_shape:t",
        "/column_family/metrics/read_latency/moving_average_histogram/srv_rest_shape:t",
        "/column_family/metrics/read_latency/estimated_histogram/srv_rest_shape:t",
        "/column_family/metrics/sstables_per_read_histogram/srv_rest_shape:t",
        "/column_family/metrics/estimated_row_size_histogram/srv_rest_shape:t",
        "/column_family/metrics/tombstone_scanned_histogram/srv_rest_shape:t",
        "/column_family/sstables/per_level/srv_rest_shape:t",
        "/column_family/autocompaction/srv_rest_shape:t",
        "/column_family/compaction_strategy/srv_rest_shape:t",
        "/column_family/built_indexes/srv_rest_shape:t",
        "/nonexistent",
    ];
    let mut diffs = Vec::new();
    for p in paths {
        let get = |base: String| {
            let http = http.clone();
            async move {
                let resp = http.get(format!("{base}{p}")).send().await.unwrap();
                let st = resp.status().as_u16();
                let body: Value = serde_json::from_str(&resp.text().await.unwrap()).unwrap_or(Value::Null);
                (st, body)
            }
        };
        let (rs, rb) = get(r.base.clone()).await;
        let (fs, fb) = get(format!("http://{reference}")).await;
        // Error texts must match too.
        let same = rs == fs && same_shape(&rb, &fb) && (rs < 300 || rb == fb);
        if !same {
            diffs.push(format!("{p}: roll {rs} {rb}\n  ref {fs} {fb}"));
        }
    }
    let _ = rc.query("DROP KEYSPACE srv_rest_shape", q()).await;
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
}

/// `nodetool disablebinary` / `enablebinary`: CQL connections close and new ones fail while the
/// native transport is off; REST keeps working.
#[tokio::test]
async fn native_transport_off_and_on() {
    let r = start(Dialect::Scylla).await;
    let c = connect(r.s.addr, nosql_proto::Version::V4, None).await;
    c.query("SELECT * FROM srv_rest.t", QueryParams::default()).await.unwrap();
    assert_eq!(r.delete("/storage_service/native_transport").await.0, 200);
    assert_eq!(r.get("/storage_service/native_transport").await, (200, json!(false)));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(c.query("SELECT * FROM srv_rest.t", QueryParams::default()).await.is_err());
    let mut cfg = cfg(r.s.addr, nosql_proto::Version::V4);
    cfg.request_timeout = Some(std::time::Duration::from_secs(2));
    let fresh = nosql_proto::client::Client::connect(cfg).await;
    let refused = match fresh {
        Err(_) => true,
        Ok(c) => c.query("SELECT * FROM srv_rest.t", QueryParams::default()).await.is_err(),
    };
    assert!(refused);
    assert_eq!(r.post("/storage_service/native_transport").await.0, 200);
    assert_eq!(r.get("/storage_service/native_transport").await, (200, json!(true)));
    let c = connect(r.s.addr, nosql_proto::Version::V4, None).await;
    c.query("SELECT * FROM srv_rest.t", QueryParams::default()).await.unwrap();
}

/// What ScyllaDB's suites drive through the REST API: a whole trace probability reads `1`,
/// compaction without flushing the memtables, auto compaction per table, pending compactions,
/// and the bloom filter false positives of point reads.
#[tokio::test]
async fn compaction_controls_and_read_metrics() {
    let r = start(Dialect::Scylla).await;
    assert_eq!(r.post("/storage_service/trace_probability?probability=1").await.0, 200);
    assert_eq!(r.call(reqwest::Method::GET, "/storage_service/trace_probability").await.1, "1");
    r.post("/storage_service/trace_probability?probability=0.5").await;
    assert_eq!(r.call(reqwest::Method::GET, "/storage_service/trace_probability").await.1, "0.5");
    r.post("/storage_service/trace_probability?probability=0").await;
    assert_eq!(r.call(reqwest::Method::GET, "/storage_service/trace_probability").await.1, "0");
    let c = connect(r.s.addr, nosql_proto::Version::V4, None).await;
    let q = QueryParams::default;
    // Auto compaction off: the flushed SSTables wait, and are counted as a pending task.
    assert_eq!(r.delete("/storage_service/auto_compaction/srv_rest?cf=t").await.0, 200);
    for k in 10..15 {
        c.query(&format!("INSERT INTO srv_rest.t (k, v) VALUES ({k}, 'x')"), q()).await.unwrap();
        r.post("/storage_service/keyspace_flush/srv_rest?cf=t").await;
    }
    let (_, pending) = r.get("/compaction_manager/metrics/pending_tasks_by_table").await;
    assert_eq!(pending, json!([{ "ks": "srv_rest", "cf": "t", "task": 1 }]));
    // Compaction without a flush leaves the memtable alone.
    c.query("INSERT INTO srv_rest.t (k, v) VALUES (20, 'm')", q()).await.unwrap();
    r.post("/storage_service/keyspace_compaction/srv_rest?cf=t&flush_memtables=false").await;
    let (_, sstables) = r.get("/column_family/metrics/live_ss_table_count/srv_rest:t").await;
    assert_eq!(sstables, json!(1));
    let (_, memtable) = r.get("/column_family/metrics/memtable_columns_count/srv_rest:t").await;
    assert_eq!(memtable, json!(1), "the memtable kept its row");
    let (_, pending) = r.get("/compaction_manager/metrics/pending_tasks").await;
    assert_eq!(pending, json!(0));
    assert_eq!(r.post("/storage_service/auto_compaction/srv_rest?cf=t").await.0, 200);
    // Point reads of missing keys the bloom filter lets through.
    for k in 1000..1200 {
        c.query(&format!("SELECT * FROM srv_rest.t WHERE k = {k}"), q()).await.unwrap();
    }
    let (_, fp) = r.get("/column_family/metrics/bloom_filter_false_positives/srv_rest:t").await;
    let fp = fp.as_u64().unwrap();
    assert!(fp > 0 && fp < 20, "{fp} false positives of 200 at 1%");
}
