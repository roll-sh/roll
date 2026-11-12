//! Alternator served by nosql-server: the AWS SDK against a real server in single mode and on a
//! multi-tenant node (routing by access key and by `Host`), SigV4 against the database's roles,
//! the same tables seen through CQL (and written through CQL), GSIs and LSIs, Streams, TTL,
//! persistence across unload, wake-on-request, metrics and HTTPS.

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aws_sdk_dynamodb::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_dynamodb::error::ProvideErrorMetadata;
use aws_sdk_dynamodb::types::*;
use nosql_engine::Dialect;
use nosql_proto::client::{Client, ClientConfig};
use nosql_proto::{QueryParams, ResultBody, Version};
use nosql_server::alternator::{self, AlternatorOptions};
use nosql_server::{DbSpec, Directory, ListenOptions, NodeConfig};
use serde_json::{Value, json};

type Item = HashMap<String, AttributeValue>;

fn s(v: &str) -> AttributeValue {
    AttributeValue::S(v.into())
}

fn n(v: &str) -> AttributeValue {
    AttributeValue::N(v.into())
}

fn item(pairs: &[(&str, AttributeValue)]) -> Item {
    pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

fn def(name: &str, ty: ScalarAttributeType) -> AttributeDefinition {
    AttributeDefinition::builder().attribute_name(name).attribute_type(ty).build().unwrap()
}

fn key(name: &str, ty: KeyType) -> KeySchemaElement {
    KeySchemaElement::builder().attribute_name(name).key_type(ty).build().unwrap()
}

fn sdk(addr: SocketAddr, access_key: &str, secret: &str) -> aws_sdk_dynamodb::Client {
    let conf = aws_sdk_dynamodb::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .endpoint_url(format!("http://{addr}"))
        .credentials_provider(Credentials::new(access_key, secret, None, None, "test"))
        .retry_config(aws_sdk_dynamodb::config::retry::RetryConfig::disabled())
        .build();
    aws_sdk_dynamodb::Client::from_conf(conf)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn opts() -> AlternatorOptions {
    let mut o = AlternatorOptions { ttl_period: Duration::from_millis(100), ..Default::default() };
    o.config.streams_time_window_ms = 0;
    o
}

/// An unsigned DynamoDB JSON request (databases that don't authenticate), with a `Host`.
async fn raw(addr: SocketAddr, host: &str, op: &str, body: Value) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(format!("http://{addr}/"))
        .header("Host", host)
        .header("Content-Type", "application/x-amz-json-1.0")
        .header("X-Amz-Target", format!("DynamoDB_20120810.{op}"))
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, serde_json::from_slice(&r.bytes().await.unwrap()).unwrap_or(Value::Null))
}

async fn rows(c: &Client, q: &str) -> Vec<Vec<Option<bytes::Bytes>>> {
    match c.query(q, QueryParams::default()).await.unwrap() {
        ResultBody::Rows(r) => r.to_vec(),
        other => panic!("{q}: {other:?}"),
    }
}

fn text(b: &Option<bytes::Bytes>) -> String {
    String::from_utf8_lossy(b.as_deref().unwrap_or_default()).into_owned()
}

/// A `map<text, blob>` cell as `name=hex` pairs.
fn attrs(b: &Option<bytes::Bytes>) -> Vec<String> {
    let Some(b) = b else { return Vec::new() };
    nosql_types::map_entries(b).unwrap().iter().map(|(k, v)| format!("{}={}", String::from_utf8_lossy(k), hex(v))).collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The table every test uses: hash `p`, range `c`, a GSI on `g`, an LSI on `l`, a stream.
async fn create_table(c: &aws_sdk_dynamodb::Client, name: &str) {
    c.create_table()
        .table_name(name)
        .key_schema(key("p", KeyType::Hash))
        .key_schema(key("c", KeyType::Range))
        .attribute_definitions(def("p", ScalarAttributeType::S))
        .attribute_definitions(def("c", ScalarAttributeType::N))
        .attribute_definitions(def("g", ScalarAttributeType::S))
        .attribute_definitions(def("l", ScalarAttributeType::N))
        .billing_mode(BillingMode::PayPerRequest)
        .global_secondary_indexes(
            GlobalSecondaryIndex::builder()
                .index_name("gsi")
                .key_schema(key("g", KeyType::Hash))
                .projection(Projection::builder().projection_type(ProjectionType::All).build())
                .build()
                .unwrap(),
        )
        .local_secondary_indexes(
            LocalSecondaryIndex::builder()
                .index_name("lsi")
                .key_schema(key("p", KeyType::Hash))
                .key_schema(key("l", KeyType::Range))
                .projection(Projection::builder().projection_type(ProjectionType::All).build())
                .build()
                .unwrap(),
        )
        .stream_specification(StreamSpecification::builder().stream_enabled(true).stream_view_type(StreamViewType::NewAndOldImages).build().unwrap())
        .tags(Tag::builder().key("team").value("db").build().unwrap())
        .tags(Tag::builder().key("system:initial_tablets").value("1").build().unwrap())
        .send()
        .await
        .unwrap();
}

async fn put(c: &aws_sdk_dynamodb::Client, t: &str, it: Item) {
    c.put_item().table_name(t).set_item(Some(it)).send().await.unwrap();
}

async fn query_index(c: &aws_sdk_dynamodb::Client, t: &str, index: &str, cond: &str, v: AttributeValue) -> Vec<Item> {
    c.query()
        .table_name(t)
        .index_name(index)
        .key_condition_expression(cond)
        .expression_attribute_values(":v", v)
        .send()
        .await
        .unwrap()
        .items
        .unwrap_or_default()
}

fn get_s(it: &Item, k: &str) -> String {
    match it.get(k) {
        Some(AttributeValue::S(v)) | Some(AttributeValue::N(v)) => v.clone(),
        other => panic!("{k}: {other:?}"),
    }
}

/// Every record of a table's stream, by shard (DynamoDB Streams JSON over raw HTTP).
async fn stream_records(addr: SocketAddr, host: &str, table: &str) -> Vec<Value> {
    let (_, d) = raw(addr, host, "DescribeTable", json!({ "TableName": table })).await;
    let arn = d["Table"]["LatestStreamArn"].clone();
    let (_, desc) = raw(addr, host, "DescribeStream", json!({ "StreamArn": arn })).await;
    let mut out = Vec::new();
    for sh in desc["StreamDescription"]["Shards"].as_array().unwrap() {
        let (_, it) = raw(addr, host, "GetShardIterator", json!({ "StreamArn": arn, "ShardId": sh["ShardId"], "ShardIteratorType": "TRIM_HORIZON" })).await;
        let (_, r) = raw(addr, host, "GetRecords", json!({ "ShardIterator": it["ShardIterator"] })).await;
        out.extend(r["Records"].as_array().cloned().unwrap_or_default());
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_mode_sdk_cql_streams_and_ttl() {
    let srv = common::single(Dialect::Scylla).await;
    let dir: Arc<dyn Directory> = srv.dir.clone();
    let addr = alternator::start(dir, opts(), "127.0.0.1:0".parse().unwrap()).await.unwrap();
    let c = sdk(addr, "anyone", "anything");
    create_table(&c, "users").await;
    for (p, cc, g, l) in [("a", "1", "red", "30"), ("a", "2", "blue", "10"), ("b", "1", "red", "20")] {
        put(&c, "users", item(&[("p", s(p)), ("c", n(cc)), ("g", s(g)), ("l", n(l)), ("name", s(&format!("{p}{cc}")))])).await;
    }
    // Reads through the table, the GSI and the LSI.
    let got = c.get_item().table_name("users").key("p", s("a")).key("c", n("2")).send().await.unwrap().item.unwrap();
    assert_eq!(get_s(&got, "name"), "a2");
    let red = query_index(&c, "users", "gsi", "g = :v", s("red")).await;
    let mut names: Vec<String> = red.iter().map(|i| get_s(i, "name")).collect();
    names.sort();
    assert_eq!(names, ["a1", "b1"]);
    let by_l: Vec<String> = query_index(&c, "users", "lsi", "p = :v", s("a")).await.iter().map(|i| get_s(i, "l")).collect();
    assert_eq!(by_l, ["10", "30"], "LSI order is by l");
    // Moving an item to another GSI partition and removing its key attribute.
    c.update_item().table_name("users").key("p", s("b")).key("c", n("1")).update_expression("SET g = :g").expression_attribute_values(":g", s("blue")).send().await.unwrap();
    assert_eq!(query_index(&c, "users", "gsi", "g = :v", s("red")).await.len(), 1);
    assert_eq!(query_index(&c, "users", "gsi", "g = :v", s("blue")).await.len(), 2);
    c.update_item().table_name("users").key("p", s("a")).key("c", n("2")).update_expression("REMOVE g").send().await.unwrap();
    assert_eq!(query_index(&c, "users", "gsi", "g = :v", s("blue")).await.len(), 1);

    // CQL sees the table as ScyllaDB lays it out.
    let cql = common::connect(srv.addr, Version::V4, None).await;
    let r = rows(&cql, "SELECT p, c, \":attrs\" FROM alternator_users.users WHERE p = 'a' AND c = 1").await;
    assert_eq!(r.len(), 1);
    assert_eq!(attrs(&r[0][2]), ["g=00726564", "l=03000000001e", "name=006131"]);
    let r = rows(&cql, "SELECT g, p, c FROM alternator_users.\"users:gsi\"").await;
    assert_eq!(r.len(), 2);
    let r = rows(&cql, "SELECT keyspace_name, table_name, column_name FROM system_schema.computed_columns WHERE keyspace_name = 'alternator_users'").await;
    assert_eq!(r.iter().map(|x| text(&x[1])).collect::<Vec<_>>(), ["users!:lsi", "users:gsi"]);
    // A CQL write is an item, and keeps the GSI up to date.
    cql.query("UPDATE alternator_users.users SET \":attrs\"[ 'g' ] = 0x00677265656e, \":attrs\"['name'] = 0x0063716c WHERE p = 'c' AND c = 7", QueryParams::default()).await.unwrap();
    let green = query_index(&c, "users", "gsi", "g = :v", s("green")).await;
    assert_eq!(green.len(), 1);
    assert_eq!(get_s(&green[0], "name"), "cql");
    assert_eq!(get_s(&green[0], "p"), "c");

    // Streams: every write, with images.
    let recs = stream_records(addr, "localhost", "users").await;
    let events: Vec<&str> = recs.iter().map(|r| r["eventName"].as_str().unwrap()).collect();
    // Three puts insert; the updates (and the CQL UPDATE, a CDC update without a row marker)
    // modify.
    assert_eq!(events, ["INSERT", "INSERT", "INSERT", "MODIFY", "MODIFY", "MODIFY"]);
    let moved = recs.iter().find(|r| r["dynamodb"]["Keys"]["p"]["S"] == "b" && r["eventName"] == "MODIFY").unwrap();
    assert_eq!(moved["dynamodb"]["OldImage"]["g"]["S"], "red");
    assert_eq!(moved["dynamodb"]["NewImage"]["g"]["S"], "blue");

    // TTL: an expired item is deleted by the scanner, from the GSI too, and the stream says who.
    c.update_time_to_live()
        .table_name("users")
        .time_to_live_specification(TimeToLiveSpecification::builder().enabled(true).attribute_name("exp").build().unwrap())
        .send()
        .await
        .unwrap();
    let past = (now_secs() - 60).to_string();
    put(&c, "users", item(&[("p", s("t")), ("c", n("1")), ("g", s("ttl")), ("exp", n(&past))])).await;
    let future = (now_secs() + 3600).to_string();
    put(&c, "users", item(&[("p", s("t")), ("c", n("2")), ("exp", n(&future))])).await;
    let mut gone = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if c.get_item().table_name("users").key("p", s("t")).key("c", n("1")).send().await.unwrap().item.is_none() {
            gone = true;
            break;
        }
    }
    assert!(gone, "the expired item is deleted");
    assert!(c.get_item().table_name("users").key("p", s("t")).key("c", n("2")).send().await.unwrap().item.is_some());
    assert!(query_index(&c, "users", "gsi", "g = :v", s("ttl")).await.is_empty());
    let recs = stream_records(addr, "localhost", "users").await;
    let expired = recs.iter().find(|r| r["eventName"] == "REMOVE" && r["dynamodb"]["Keys"]["p"]["S"] == "t").unwrap();
    assert_eq!(expired["userIdentity"], json!({ "Type": "Service", "PrincipalId": "dynamodb.amazonaws.com" }));
    assert_eq!(expired["dynamodb"]["OldImage"]["g"]["S"], "ttl");
    // Metrics.
    let m = srv.dir.loaded().metrics.snapshot();
    assert!(m.alternator_requests >= 20, "{m:?}");
    assert!(m.alternator_requests <= m.requests);
    // DeleteTable drops the keyspace.
    c.delete_table().table_name("users").send().await.unwrap();
    assert!(rows(&cql, "SELECT keyspace_name FROM system_schema.keyspaces WHERE keyspace_name = 'alternator_users'").await.is_empty());
}

/// n1 authenticates (superuser cassandra/pw1), n2 doesn't.
async fn two_databases(idle: Option<Duration>) -> (common::NodeServer, SocketAddr) {
    let cfg = NodeConfig { idle_timeout: idle, alternator_domain: Some("alternator.test".into()), ..Default::default() };
    let n = common::node(cfg, ListenOptions::default()).await;
    n.catalog.insert(DbSpec::new("n1", Dialect::Scylla).with_password("cassandra", "pw1"));
    n.catalog.insert(DbSpec::new("n2", Dialect::Scylla));
    let dir: Arc<dyn Directory> = n.node.clone();
    let addr = alternator::start(dir, opts(), "127.0.0.1:0".parse().unwrap()).await.unwrap();
    (n, addr)
}

async fn cql_as(addr: SocketAddr, user: &str, password: &str) -> Client {
    Client::connect(ClientConfig::new(addr.to_string()).credentials(user, password)).await.unwrap()
}

async fn salted_hash(c: &Client, role: &str) -> String {
    text(&rows(c, &format!("SELECT salted_hash FROM system.roles WHERE role = '{role}'")).await[0][0])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_tenant_routing_and_sigv4_roles() {
    let (node, addr) = two_databases(None).await;
    let cql = cql_as(node.addr, "cassandra@n1", "pw1").await;
    let secret = salted_hash(&cql, "cassandra").await;
    assert!(secret.starts_with("$6$"), "{secret}");
    // The access key names the database.
    let c1 = sdk(addr, "cassandra@n1", &secret);
    create_table(&c1, "orders").await;
    put(&c1, "orders", item(&[("p", s("x")), ("c", n("1"))])).await;
    assert_eq!(c1.list_tables().send().await.unwrap().table_names(), ["orders"]);
    // n2 (by Host) is another database.
    let (st, v) = raw(addr, "n2.alternator.test", "ListTables", json!({})).await;
    assert_eq!((st, v), (200, json!({ "TableNames": [] })));
    let (st, _) = raw(addr, "n2.alternator.test:8000", "CreateTable", json!({"TableName":"other","KeySchema":[{"AttributeName":"p","KeyType":"HASH"}],"AttributeDefinitions":[{"AttributeName":"p","AttributeType":"S"}],"BillingMode":"PAY_PER_REQUEST"})).await;
    assert_eq!(st, 200);
    assert_eq!(c1.list_tables().send().await.unwrap().table_names(), ["orders"]);
    // n1 refuses unsigned requests, wrong secrets, unknown roles and roles that can't log in.
    let (st, v) = raw(addr, "n1.alternator.test", "ListTables", json!({})).await;
    assert_eq!(st, 400);
    assert_eq!(v["__type"], "com.amazonaws.dynamodb.v20120810#MissingAuthenticationTokenException");
    let e = sdk(addr, "cassandra@n1", "wrong").list_tables().send().await.unwrap_err();
    assert_eq!(e.code(), Some("UnrecognizedClientException"));
    let e = sdk(addr, "mallory@n1", "x").list_tables().send().await.unwrap_err();
    assert_eq!(e.code(), Some("UnrecognizedClientException"));
    // Roles created with CQL sign at once, with their own salted hash.
    cql.query("CREATE ROLE alice WITH PASSWORD = 'pa' AND LOGIN = true", QueryParams::default()).await.unwrap();
    cql.query("CREATE ROLE nologin WITH PASSWORD = 'pn' AND LOGIN = false", QueryParams::default()).await.unwrap();
    let alice = salted_hash(&cql, "alice").await;
    assert_eq!(sdk(addr, "alice@n1", &alice).list_tables().send().await.unwrap().table_names(), ["orders"]);
    let e = sdk(addr, "nologin@n1", &salted_hash(&cql, "nologin").await).list_tables().send().await.unwrap_err();
    assert_eq!(e.code(), Some("UnrecognizedClientException"));
    // An unknown database falls back to nothing (no default database).
    let (st, v) = raw(addr, "zz.alternator.test", "ListTables", json!({})).await;
    assert_eq!(st, 400);
    assert_eq!(v["__type"], "com.amazonaws.dynamodb.v20120810#UnrecognizedClientException");
    // Per-database metrics.
    let m1 = node.node.metrics("n1").unwrap();
    let m2 = node.node.metrics("n2").unwrap();
    assert!(m1.alternator_requests >= 7 && m1.alternator_errors >= 4, "{m1:?}");
    assert_eq!((m2.alternator_requests, m2.alternator_errors), (2, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn persistence_across_unload_and_wake_on_request() {
    let (node, addr) = two_databases(Some(Duration::from_millis(300))).await;
    let host = "n2.alternator.test";
    let c = sdk(addr, "any@n2", "x");
    create_table(&c, "keep").await;
    c.tag_resource().resource_arn("arn:scylla:alternator:alternator_keep:scylla:table/keep").tags(Tag::builder().key("k").value("v").build().unwrap()).send().await.unwrap();
    c.update_time_to_live()
        .table_name("keep")
        .time_to_live_specification(TimeToLiveSpecification::builder().enabled(true).attribute_name("exp").build().unwrap())
        .send()
        .await
        .unwrap();
    for i in 0..20 {
        put(&c, "keep", item(&[("p", s(&format!("p{}", i % 4))), ("c", n(&i.to_string())), ("g", s(if i % 2 == 0 { "even" } else { "odd" })), ("l", n(&(100 - i).to_string()))])).await;
    }
    let before = c.describe_table().table_name("keep").send().await.unwrap().table.unwrap();
    // Idle: unloaded on its own; the next request loads it again.
    let mut unloaded = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if node.node.get_loaded("n2").is_none() {
            unloaded = true;
            break;
        }
    }
    assert!(unloaded, "an idle database is unloaded");
    let loads = node.node.metrics("n2").unwrap().loads;
    let after = c.describe_table().table_name("keep").send().await.unwrap().table.unwrap();
    assert_eq!(node.node.metrics("n2").unwrap().loads, loads + 1, "the request woke it");
    assert_eq!(after.table_id, before.table_id);
    assert_eq!(after.key_schema, before.key_schema);
    assert_eq!(after.global_secondary_indexes.as_ref().map(|g| g.len()), Some(1));
    assert_eq!(after.local_secondary_indexes.as_ref().map(|g| g.len()), Some(1));
    assert_eq!(after.latest_stream_arn, before.latest_stream_arn);
    let tags = c.list_tags_of_resource().resource_arn("arn:scylla:alternator:alternator_keep:scylla:table/keep").send().await.unwrap();
    let keys: Vec<&str> = tags.tags().iter().map(|t| t.key()).collect();
    assert!(keys.contains(&"k") && keys.contains(&"team"), "{keys:?}");
    let ttl = c.describe_time_to_live().table_name("keep").send().await.unwrap();
    assert_eq!(ttl.time_to_live_description.unwrap().attribute_name(), Some("exp"));
    // Items, the GSI and the LSI survived.
    let scan = c.scan().table_name("keep").send().await.unwrap();
    assert_eq!(scan.count, 20);
    assert_eq!(query_index(&c, "keep", "gsi", "g = :v", s("even")).await.len(), 10);
    let by_l: Vec<String> = query_index(&c, "keep", "lsi", "p = :v", s("p1")).await.iter().map(|i| get_s(i, "l")).collect();
    assert_eq!(by_l, ["83", "87", "91", "95", "99"]);
    // So did the stream's records.
    assert_eq!(stream_records(addr, host, "keep").await.len(), 20);
    // Evicting (as a delete or restore does) works the same.
    assert!(node.node.evict("n2").await.unwrap());
    assert_eq!(c.get_item().table_name("keep").key("p", s("p3")).key("c", n("19")).send().await.unwrap().item.map(|i| get_s(&i, "g")), Some("odd".into()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn https() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let ck = rcgen::generate_simple_self_signed(vec!["db1.alternator.test".into()]).unwrap();
    let acceptor = nosql_server::net::tls_acceptor(ck.cert.pem().as_bytes(), ck.signing_key.serialize_pem().as_bytes()).unwrap();
    let srv = common::single(Dialect::Scylla).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(alternator::serve_tls(listener, acceptor, srv.dir.clone(), opts()));
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.add(ck.cert.der().clone()).unwrap();
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let cfg = tokio_rustls::rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap().with_root_certificates(roots).with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    let name = tokio_rustls::rustls::pki_types::ServerName::try_from("db1.alternator.test").unwrap();
    let mut tls = connector.connect(name, sock).await.unwrap();
    let body = "{}";
    let req = format!(
        "POST / HTTP/1.1\r\nHost: db1.alternator.test\r\nContent-Type: application/x-amz-json-1.0\r\nX-Amz-Target: DynamoDB_20120810.ListTables\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    tls.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    tls.read_to_end(&mut out).await.unwrap();
    let out = String::from_utf8_lossy(&out);
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(out.ends_with(r#"{"TableNames":[]}"#), "{out}");
}

fn denied(e: impl ProvideErrorMetadata) -> bool {
    e.code() == Some("AccessDeniedException")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cql_permissions_authorize_operations() {
    let cfg = NodeConfig { idle_timeout: None, ..Default::default() };
    let node = common::node(cfg, ListenOptions::default()).await;
    let mut spec = DbSpec::new("n1", Dialect::Scylla).with_password("cassandra", "pw1");
    spec.config.settings.authorizer = "CassandraAuthorizer".into();
    node.catalog.insert(spec);
    let dir: Arc<dyn Directory> = node.node.clone();
    let addr = alternator::start(dir, opts(), "127.0.0.1:0".parse().unwrap()).await.unwrap();
    let cql = cql_as(node.addr, "cassandra@n1", "pw1").await;
    let admin = sdk(addr, "cassandra@n1", &salted_hash(&cql, "cassandra").await);
    create_table(&admin, "perm").await;
    put(&admin, "perm", item(&[("p", s("a")), ("c", n("1")), ("g", s("x"))])).await;
    cql.query("CREATE ROLE alice WITH PASSWORD = 'pa' AND LOGIN = true", QueryParams::default()).await.unwrap();
    let alice = sdk(addr, "alice@n1", &salted_hash(&cql, "alice").await);
    let get = || alice.get_item().table_name("perm").key("p", s("a")).key("c", n("1")).send();
    // No permission yet; nothing is needed to describe.
    assert!(denied(get().await.unwrap_err()));
    alice.describe_table().table_name("perm").send().await.unwrap();
    alice.list_tables().send().await.unwrap();
    // SELECT on the table reads it (not its index), MODIFY writes it.
    cql.query("GRANT SELECT ON alternator_perm.perm TO alice", QueryParams::default()).await.unwrap();
    assert!(get().await.unwrap().item.is_some());
    assert!(denied(alice.query().table_name("perm").index_name("gsi").key_condition_expression("g = :v").expression_attribute_values(":v", s("x")).send().await.unwrap_err()));
    assert!(denied(alice.put_item().table_name("perm").item("p", s("b")).item("c", n("1")).send().await.unwrap_err()));
    cql.query("GRANT MODIFY ON KEYSPACE alternator_perm TO alice", QueryParams::default()).await.unwrap();
    alice.put_item().table_name("perm").item("p", s("b")).item("c", n("1")).send().await.unwrap();
    cql.query("GRANT SELECT ON alternator_perm.\"perm:gsi\" TO alice", QueryParams::default()).await.unwrap();
    assert_eq!(query_index(&alice, "perm", "gsi", "g = :v", s("x")).await.len(), 1);
    assert!(denied(alice.update_table().table_name("perm").billing_mode(BillingMode::PayPerRequest).send().await.unwrap_err()));
    // CreateTable needs CREATE on all keyspaces and grants the creator its table and indexes.
    let mine = || {
        alice
            .create_table()
            .table_name("mine")
            .key_schema(key("p", KeyType::Hash))
            .attribute_definitions(def("p", ScalarAttributeType::S))
            .attribute_definitions(def("x", ScalarAttributeType::S))
            .billing_mode(BillingMode::PayPerRequest)
            .global_secondary_indexes(
                GlobalSecondaryIndex::builder()
                    .index_name("byx")
                    .key_schema(key("x", KeyType::Hash))
                    .projection(Projection::builder().projection_type(ProjectionType::All).build())
                    .build()
                    .unwrap(),
            )
            .send()
    };
    assert!(denied(mine().await.unwrap_err()));
    cql.query("GRANT CREATE ON ALL KEYSPACES TO alice", QueryParams::default()).await.unwrap();
    mine().await.unwrap();
    alice.put_item().table_name("mine").item("p", s("k")).item("x", s("y")).send().await.unwrap();
    assert_eq!(query_index(&alice, "mine", "byx", "x = :v", s("y")).await.len(), 1);
    alice.update_table().table_name("mine").billing_mode(BillingMode::PayPerRequest).send().await.unwrap();
    // Deleting it revokes the grants: a table of the same name made by someone else isn't hers.
    alice.delete_table().table_name("mine").send().await.unwrap();
    admin
        .create_table()
        .table_name("mine")
        .key_schema(key("p", KeyType::Hash))
        .attribute_definitions(def("p", ScalarAttributeType::S))
        .billing_mode(BillingMode::PayPerRequest)
        .send()
        .await
        .unwrap();
    assert!(denied(alice.get_item().table_name("mine").key("p", s("k")).send().await.unwrap_err()));
    let r = rows(&cql, "SELECT resource FROM system.role_permissions WHERE role = 'alice'").await;
    let resources: Vec<String> = r.iter().map(|x| text(&x[0])).collect();
    assert!(!resources.iter().any(|x| x.contains("alternator_mine")), "{resources:?}");
    // Superusers need no grants.
    cql.query("CREATE ROLE root WITH PASSWORD = 'pr' AND LOGIN = true AND SUPERUSER = true", QueryParams::default()).await.unwrap();
    let root = sdk(addr, "root@n1", &salted_hash(&cql, "root").await);
    root.get_item().table_name("mine").key("p", s("k")).send().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prometheus_metrics_and_system_clients() {
    let srv = common::single(Dialect::Scylla).await;
    let dir: Arc<dyn Directory> = srv.dir.clone();
    let addr = alternator::start(dir.clone(), opts(), "127.0.0.1:0".parse().unwrap()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let metrics_addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, alternator::metrics_router(dir, opts())).await });
    let c = sdk(addr, "anyone", "anything");
    create_table(&c, "met").await;
    put(&c, "met", item(&[("p", s("a")), ("c", n("1")), ("x", s(&"y".repeat(5000)))])).await;
    c.get_item().table_name("met").key("p", s("a")).key("c", n("1")).consistent_read(true).send().await.unwrap();
    let e = c.put_item().table_name("met").item("p", s("a")).item("c", n("1")).condition_expression("attribute_not_exists(p)").send().await.unwrap_err();
    assert_eq!(e.code(), Some("ConditionalCheckFailedException"));
    let page = reqwest::get(format!("http://{metrics_addr}/metrics")).await.unwrap().text().await.unwrap();
    let value = |line_start: &str| -> f64 {
        page.lines().find(|l| l.starts_with(line_start)).and_then(|l| l.rsplit(' ').next()).and_then(|v| v.parse().ok()).unwrap_or_else(|| panic!("no {line_start} in\n{page}"))
    };
    assert_eq!(value("scylla_alternator_operation{op=\"PutItem\""), 2.0);
    assert_eq!(value("scylla_alternator_table_operation{op=\"GetItem\",cf=\"met\""), 1.0);
    assert_eq!(value("scylla_alternator_rcu_total{op=\"GetItem\""), 2.0, "5 KB consistently: 2 RCU");
    assert_eq!(value("scylla_alternator_wcu_total{op=\"PutItem\""), 5.0, "5 KB: 5 WCU");
    assert_eq!(value("scylla_alternator_conditional_check_failed{op=\"PutItem\""), 1.0);
    assert_eq!(value("scylla_alternator_op_latency_count{op=\"PutItem\""), 1.0, "only successes");
    assert!(value("scylla_alternator_total_operations") >= 4.0);
    assert!(page.contains("scylla_alternator_operation_size_kb_bucket{op=\"GetItem\",le=\"4.000000\",shard=\"0\"} 0"), "{page}");
    assert!(page.contains("scylla_alternator_operation_size_kb_bucket{op=\"GetItem\",le=\"5.000000\",shard=\"0\"} 1"), "{page}");
    // The connection shows in system.clients as an Alternator client.
    let cql = common::connect(srv.addr, Version::V4, None).await;
    let r = rows(&cql, "SELECT client_type, driver_name FROM system.clients").await;
    assert!(r.iter().any(|x| text(&x[0]) == "alternator" && text(&x[1]).contains("aws-sdk-rust")), "{:?}", r.iter().map(|x| (text(&x[0]), text(&x[1]))).collect::<Vec<_>>());
}

/// Live options set with CQL (`UPDATE system.config`) apply to Alternator: the expression
/// cache's size (and its metrics), `max_concurrent_requests_per_shard` (requests refused with
/// `RequestLimitExceeded`, the connection kept), `alternator_force_read_before_write` (write
/// units from the old item); ScyllaDB's audit filters log Alternator requests; tracing records
/// them as `Alternator <Op>` sessions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cql_options_audit_and_tracing() {
    let srv = common::single(Dialect::Scylla).await;
    let dir: Arc<dyn Directory> = srv.dir.clone();
    let addr = alternator::start(dir.clone(), opts(), "127.0.0.1:0".parse().unwrap()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let metrics_addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, alternator::metrics_router(dir, opts())).await });
    let c = sdk(addr, "anyone", "anything");
    let cql = common::connect(srv.addr, Version::V4, None).await;
    let set = |name: &'static str, value: &'static str| {
        let cql = &cql;
        async move {
            cql.query(&format!("UPDATE system.config SET value = '{value}' WHERE name = '{name}'"), QueryParams::default())
                .await
                .unwrap();
        }
    };
    create_table(&c, "opts").await;
    // `system.config` shows Alternator's flags as configured (the image's default is 10).
    let r = rows(&cql, "SELECT value FROM system.config WHERE name = 'alternator_streams_time_window_s'").await;
    assert_eq!(text(&r[0][0]), "0");
    // The expression cache, flushed by setting its size.
    set("alternator_max_expression_cache_entries_per_shard", "0").await;
    set("alternator_max_expression_cache_entries_per_shard", "2").await;
    for v in ["1", "2", "3"] {
        c.update_item().table_name("opts").key("p", s("a")).key("c", n("1")).update_expression("SET x = :v").expression_attribute_values(":v", s(v)).send().await.unwrap();
    }
    let page = reqwest::get(format!("http://{metrics_addr}/metrics")).await.unwrap().text().await.unwrap();
    assert!(page.contains("scylla_alternator_expression_cache_hits{expression=\"UpdateExpression\",shard=\"0\"} 2"), "{page}");
    assert!(page.contains("scylla_alternator_expression_cache_misses{expression=\"UpdateExpression\",shard=\"0\"} 1"), "{page}");
    // Shedding.
    set("max_concurrent_requests_per_shard", "0").await;
    let (status, body) = raw(addr, "localhost", "DescribeTable", json!({"TableName": "opts"})).await;
    assert_eq!(status, 400);
    assert!(body["__type"].as_str().unwrap().ends_with("RequestLimitExceeded"), "{body}");
    set("max_concurrent_requests_per_shard", "4294967295").await;
    assert_eq!(raw(addr, "localhost", "DescribeTable", json!({"TableName": "opts"})).await.0, 200);
    // Write units of a batch count the item it overwrites when reads before writes are forced.
    put(&c, "opts", item(&[("p", s("b")), ("c", n("1")), ("x", s(&"y".repeat(2000)))])).await;
    set("alternator_force_read_before_write", "true").await;
    let r = c
        .batch_write_item()
        .request_items("opts", vec![WriteRequest::builder().put_request(PutRequest::builder().set_item(Some(item(&[("p", s("b")), ("c", n("1"))]))).build().unwrap()).build()])
        .return_consumed_capacity(ReturnConsumedCapacity::Total)
        .send()
        .await
        .unwrap();
    assert_eq!(r.consumed_capacity()[0].capacity_units(), Some(2.0));
    set("alternator_force_read_before_write", "false").await;
    // Audit: categories and keyspaces set with CQL.
    set("audit_categories", "DML,QUERY").await;
    set("audit_keyspaces", "alternator_opts").await;
    put(&c, "opts", item(&[("p", s("audited")), ("c", n("1"))])).await;
    let r = rows(&cql, "SELECT category, consistency, keyspace_name, table_name, operation FROM audit.audit_log").await;
    let got: Vec<Vec<String>> = r.iter().map(|x| x.iter().map(text).collect()).collect();
    assert!(
        got.iter().any(|x| x[..4] == ["DML", "LOCAL_QUORUM", "alternator_opts", "opts"] && x[4].starts_with("PutItem|{") && x[4].contains("audited")),
        "{got:?}"
    );
    // Tracing.
    srv.db.set_trace_probability(1.0).unwrap();
    put(&c, "opts", item(&[("p", s("traced")), ("c", n("1"))])).await;
    srv.db.set_trace_probability(0.0).unwrap();
    let r = rows(&cql, "SELECT request, parameters FROM system_traces.sessions").await;
    let traced = r.iter().find(|x| text(&x[0]) == "Alternator PutItem").expect("an Alternator PutItem session");
    let params: Vec<(String, String)> = nosql_types::map_entries(traced[1].as_deref().unwrap())
        .unwrap()
        .iter()
        .map(|(k, v)| (String::from_utf8_lossy(k).into_owned(), String::from_utf8_lossy(v).into_owned()))
        .collect();
    assert!(params.contains(&("alternator_op".into(), "PutItem".into())), "{params:?}");
    assert!(params.contains(&("table".into(), "opts".into())), "{params:?}");
    assert!(params.iter().any(|(k, v)| k == "query" && v.contains("traced")), "{params:?}");
}

/// A Scan stops after `query_tombstone_page_limit` tombstones (resuming where it stopped), and
/// a tablet count change of a table with a stream starts a generation of shards whose parents
/// are the shards they split from.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tombstone_pages_and_stream_generations() {
    let srv = common::single(Dialect::Scylla).await;
    let dir: Arc<dyn Directory> = srv.dir.clone();
    // Deletions of missing items write tombstones (an LWT delete of nothing writes nothing).
    let mut o = opts();
    o.config.write_isolation = nosql_alternator::WriteIsolation::OnlyRmwUsesLwt;
    let addr = alternator::start(dir, o, "127.0.0.1:0".parse().unwrap()).await.unwrap();
    let c = sdk(addr, "anyone", "anything");
    let cql = common::connect(srv.addr, Version::V4, None).await;
    cql.query("UPDATE system.config SET value = '10' WHERE name = 'query_tombstone_page_limit'", QueryParams::default()).await.unwrap();
    // No stream: with images, deleting a missing item writes nothing.
    c.create_table()
        .table_name("tomb")
        .key_schema(key("p", KeyType::Hash))
        .key_schema(key("c", KeyType::Range))
        .attribute_definitions(def("p", ScalarAttributeType::S))
        .attribute_definitions(def("c", ScalarAttributeType::N))
        .billing_mode(BillingMode::PayPerRequest)
        .send()
        .await
        .unwrap();
    put(&c, "tomb", item(&[("p", s("a")), ("c", n("0"))])).await;
    for i in 1..20 {
        c.delete_item().table_name("tomb").key("p", s("a")).key("c", n(&i.to_string())).send().await.unwrap();
    }
    put(&c, "tomb", item(&[("p", s("a")), ("c", n("20"))])).await;
    let first = c.scan().table_name("tomb").send().await.unwrap();
    assert_eq!(first.count(), 1);
    let mut found = 1;
    let mut next = first.last_evaluated_key().cloned();
    let mut pages = 1;
    while let Some(k) = next {
        let r = c.scan().table_name("tomb").set_exclusive_start_key(Some(k)).send().await.unwrap();
        found += r.count();
        next = r.last_evaluated_key().cloned();
        pages += 1;
    }
    assert_eq!(found, 2);
    assert_eq!(pages, 2, "the first page stops among the tombstones");
    // Streams: one generation, then two after the log's tablets split (1 -> 2).
    create_table(&c, "gen").await;
    let arn = c.describe_table().table_name("gen").send().await.unwrap().table().unwrap().latest_stream_arn().unwrap().to_string();
    let st = aws_sdk_dynamodbstreams_shards(addr, &arn).await;
    assert_eq!(st.len(), 1);
    cql.query("ALTER TABLE \"alternator_gen\".\"gen_scylla_cdc_log\" WITH tablets = {'min_tablet_count': 2}", QueryParams::default()).await.unwrap();
    let st = aws_sdk_dynamodbstreams_shards(addr, &arn).await;
    assert_eq!(st.len(), 3, "{st:?}");
    let root = st[0]["ShardId"].as_str().unwrap();
    assert!(st[0]["SequenceNumberRange"]["EndingSequenceNumber"].is_string(), "the old generation is closed");
    for child in &st[1..] {
        assert_eq!(child["ParentShardId"].as_str(), Some(root), "{st:?}");
    }
    let (_, r) = raw(addr, "localhost", "DescribeStream", json!({"StreamArn": arn, "ShardFilter": {"Type": "CHILD_SHARDS", "ShardId": root}})).await;
    assert_eq!(r["StreamDescription"]["Shards"].as_array().unwrap().len(), 2, "{r}");
}

/// DescribeStream's shards (unsigned request, the Streams API's target prefix).
async fn aws_sdk_dynamodbstreams_shards(addr: SocketAddr, arn: &str) -> Vec<Value> {
    let r = reqwest::Client::new()
        .post(format!("http://{addr}/"))
        .header("Content-Type", "application/x-amz-json-1.0")
        .header("X-Amz-Target", "DynamoDBStreams_20120810.DescribeStream")
        .body(json!({"StreamArn": arn}).to_string())
        .send()
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&r.bytes().await.unwrap()).unwrap();
    v["StreamDescription"]["Shards"].as_array().cloned().unwrap_or_default()
}
