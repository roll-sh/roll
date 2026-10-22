//! Alternator and CQL on the same tables, differential against ScyllaDB: the same DynamoDB API
//! requests go to the reference's Alternator and to roll's, then CQL reads both sides' tables
//! (the base table, the index views, the CDC log) and `system_schema` rows and compares them;
//! the other way round, CQL writes reach both sides' DynamoDB API reads.
//!
//! References: `ROLL_NOSQL_REF_SCYLLA=127.0.0.1:19042` (CQL) and
//! `ROLL_NOSQL_REF_SCYLLA_ALTERNATOR=http://127.0.0.1:18000` (the shared container, with the
//! image's default `always_use_lwt` write isolation; see docs/nosql/contracts.md). Without them
//! the test is skipped. Tables are `altx_cmp_<n>` and are deleted on both sides.

mod common;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use nosql_engine::Dialect;
use nosql_proto::client::{Client, ClientConfig};
use nosql_proto::{QueryParams, ResultBody};
use nosql_server::Directory;
use nosql_server::alternator::{self, AlternatorOptions};
use serde_json::{Value, json};

struct Side {
    url: String,
    cql: Client,
}

async fn api(url: &str, op: &str, body: &Value) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(format!("{url}/"))
        .header("Content-Type", "application/x-amz-json-1.0")
        .header("X-Amz-Target", format!("DynamoDB_20120810.{op}"))
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let st = r.status().as_u16();
    (st, serde_json::from_slice(&r.bytes().await.unwrap()).unwrap_or(Value::Null))
}

type Rows = Vec<Vec<Option<Bytes>>>;

async fn select(c: &Client, q: &str) -> Rows {
    match c.query(q, QueryParams::default()).await {
        Ok(ResultBody::Rows(r)) => r.to_vec(),
        other => panic!("{q}: {other:?}"),
    }
}

/// Both sides answer `op` alike (status and body, apart from volatile members).
async fn both(sides: &[Side], op: &str, body: Value) -> Value {
    let mut out = Vec::new();
    for s in sides {
        let (st, mut v) = api(&s.url, op, &body).await;
        scrub(&mut v);
        out.push((st, v));
    }
    assert_eq!(out[0], out[1], "{op} {body}");
    out.remove(0).1
}

/// Leaves out what differs between runs (times, ids, ARNs).
fn scrub(v: &mut Value) {
    const VOLATILE: &[&str] = &["CreationDateTime", "TableId", "LatestStreamArn", "LatestStreamLabel", "LastUpdateToPayPerRequestDateTime", "TableSizeBytes", "ItemCount"];
    match v {
        Value::Object(m) => {
            for k in VOLATILE {
                m.remove(*k);
            }
            m.values_mut().for_each(scrub);
        }
        Value::Array(a) => a.iter_mut().for_each(scrub),
        _ => {}
    }
}

/// `scylla_tags` without the creation time; other extensions as stored.
fn extensions(b: &Option<Bytes>) -> BTreeMap<String, String> {
    let Some(b) = b else { return BTreeMap::new() };
    nosql_types::map_entries(b)
        .unwrap()
        .into_iter()
        .map(|(k, v)| {
            let k = String::from_utf8_lossy(k).into_owned();
            let v = if k == "scylla_tags" {
                let mut m = nosql_engine::schema::params::parse_scylla_map_blob(v).unwrap();
                m.remove("system:table_creation_time");
                format!("{m:?}")
            } else {
                format!("{v:?}")
            };
            (k, v)
        })
        .collect()
}

/// The CDC log as (operation, sequence, key, `:attrs`) per write, in write order (stream ids
/// and times differ between sides).
fn log_rows(mut rows: Rows) -> Vec<Vec<Option<Bytes>>> {
    // cdc$time (timeuuid) is column 0: order by its timestamp, then by cdc$batch_seq_no.
    let ts = |r: &Vec<Option<Bytes>>| {
        let u = r[0].as_deref().unwrap_or(&[0; 16]);
        let low = u32::from_be_bytes([u[0], u[1], u[2], u[3]]) as u64;
        let mid = u16::from_be_bytes([u[4], u[5]]) as u64;
        let hi = (u16::from_be_bytes([u[6], u[7]]) & 0xfff) as u64;
        (hi << 48) | (mid << 32) | low
    };
    rows.sort_by_key(|r| (ts(r), r[1].clone()));
    rows.into_iter().map(|mut r| r.split_off(1)).collect()
}

fn show(r: &[Option<Bytes>]) -> String {
    r.iter().map(|c| c.as_ref().map_or("null".to_string(), |b| format!("{:?}", String::from_utf8_lossy(b)))).collect::<Vec<_>>().join(" | ")
}

async fn compare_cql(sides: &[Side], what: &str, q: &str, f: impl Fn(Rows) -> Rows) {
    let a = f(select(&sides[0].cql, q).await);
    let b = f(select(&sides[1].cql, q).await);
    if a != b {
        let i = a.iter().zip(&b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
        panic!(
            "{what}: {q}\n{} rows on ScyllaDB, {} on roll; first difference at row {i}:\n  scylla: {}\n  roll:   {}",
            a.len(),
            b.len(),
            a.get(i).map_or("-".into(), |r| show(r)),
            b.get(i).map_or("-".into(), |r| show(r)),
        );
    }
}

fn sorted(mut r: Rows) -> Rows {
    r.sort();
    r
}

/// Without ScyllaDB's Paxos state tables (`T$paxos`, created by LWT on tablets; roll's LWT
/// keeps no such table).
fn no_paxos(r: Rows) -> Rows {
    sorted(r.into_iter().filter(|x| !x[1].as_deref().unwrap_or_default().ends_with(b"$paxos")).collect())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alternator_tables_through_cql_match_scylla() {
    let (Ok(ref_cql), Ok(ref_alt)) = (std::env::var("ROLL_NOSQL_REF_SCYLLA"), std::env::var("ROLL_NOSQL_REF_SCYLLA_ALTERNATOR")) else {
        eprintln!("skipped: set ROLL_NOSQL_REF_SCYLLA and ROLL_NOSQL_REF_SCYLLA_ALTERNATOR");
        return;
    };
    let srv = common::single(Dialect::Scylla).await;
    let dir: Arc<dyn Directory> = srv.dir.clone();
    let mut opts = AlternatorOptions::default();
    opts.config.streams_time_window_ms = 0;
    let addr: SocketAddr = alternator::start(dir, opts, "127.0.0.1:0".parse().unwrap()).await.unwrap();
    let sides = [
        Side { url: ref_alt.trim_end_matches('/').to_string(), cql: Client::connect(ClientConfig::new(ref_cql.clone())).await.unwrap() },
        Side { url: format!("http://{addr}"), cql: common::connect(srv.addr, nosql_proto::Version::V4, None).await },
    ];
    let t = format!("altx_cmp_{}", std::process::id());
    let ks = format!("alternator_{t}");
    for s in &sides {
        api(&s.url, "DeleteTable", &json!({ "TableName": t })).await;
    }
    let schema = json!({
        "TableName": t, "BillingMode": "PAY_PER_REQUEST",
        "KeySchema": [{"AttributeName": "p", "KeyType": "HASH"}, {"AttributeName": "c", "KeyType": "RANGE"}],
        "AttributeDefinitions": [{"AttributeName": "p", "AttributeType": "S"}, {"AttributeName": "c", "AttributeType": "N"},
                                 {"AttributeName": "g", "AttributeType": "S"}, {"AttributeName": "l", "AttributeType": "B"}],
        "GlobalSecondaryIndexes": [{"IndexName": "gsi", "KeySchema": [{"AttributeName": "g", "KeyType": "HASH"}], "Projection": {"ProjectionType": "ALL"}}],
        "LocalSecondaryIndexes": [{"IndexName": "lsi", "KeySchema": [{"AttributeName": "p", "KeyType": "HASH"}, {"AttributeName": "l", "KeyType": "RANGE"}], "Projection": {"ProjectionType": "ALL"}}],
        "StreamSpecification": {"StreamEnabled": true, "StreamViewType": "NEW_AND_OLD_IMAGES"},
        "Tags": [{"Key": "team", "Value": "db"}, {"Key": "system:initial_tablets", "Value": "1"}]
    });
    both(&sides, "CreateTable", schema).await;
    let result = async {
        // Writes of every kind and attribute type.
        let items = [
            json!({"p": {"S": "a"}, "c": {"N": "1.50"}, "g": {"S": "red"}, "l": {"B": "AAE="}, "n": {"N": "-12.3e5"}, "t": {"BOOL": true},
                   "z": {"NULL": true}, "m": {"M": {"k": {"L": [{"N": "1.50"}, {"S": "x"}]}}}, "ss": {"SS": ["b", "a"]}, "e": {"S": ""}}),
            json!({"p": {"S": "a"}, "c": {"N": "2"}, "g": {"S": "blue"}, "bs": {"BS": ["AQ==", "AA=="]}, "ns": {"NS": ["3", "1", "2.0"]}}),
            json!({"p": {"S": "b"}, "c": {"N": "-1"}, "g": {"S": "red"}, "l": {"B": "Ag=="}}),
        ];
        for it in &items {
            both(&sides, "PutItem", json!({"TableName": t, "Item": it})).await;
        }
        both(&sides, "PutItem", json!({"TableName": t, "Item": items[2]})).await;
        both(&sides, "UpdateItem", json!({"TableName": t, "Key": {"p": {"S": "a"}, "c": {"N": "2"}},
            "UpdateExpression": "SET g = :g, x = :x REMOVE bs ADD cnt :one", "ExpressionAttributeValues": {":g": {"S": "green"}, ":x": {"N": "7"}, ":one": {"N": "1"}}})).await;
        both(&sides, "UpdateItem", json!({"TableName": t, "Key": {"p": {"S": "c"}, "c": {"N": "0"}}, "UpdateExpression": "SET v = :v",
            "ExpressionAttributeValues": {":v": {"S": "new"}}})).await;
        both(&sides, "UpdateItem", json!({"TableName": t, "Key": {"p": {"S": "c"}, "c": {"N": "0"}}, "UpdateExpression": "REMOVE v"})).await;
        both(&sides, "PutItem", json!({"TableName": t, "Item": {"p": {"S": "b"}, "c": {"N": "-1"}, "q": {"S": "cond"}},
            "ConditionExpression": "attribute_exists(p)"})).await;
        both(&sides, "DeleteItem", json!({"TableName": t, "Key": {"p": {"S": "a"}, "c": {"N": "1.5"}}, "ReturnValues": "ALL_OLD"})).await;
        both(&sides, "DeleteItem", json!({"TableName": t, "Key": {"p": {"S": "zz"}, "c": {"N": "9"}}})).await;
        // The DynamoDB API reads agree.
        both(&sides, "Query", json!({"TableName": t, "IndexName": "gsi", "KeyConditionExpression": "g = :g", "ExpressionAttributeValues": {":g": {"S": "red"}}})).await;
        both(&sides, "Query", json!({"TableName": t, "IndexName": "lsi", "KeyConditionExpression": "p = :p", "ExpressionAttributeValues": {":p": {"S": "a"}}})).await;
        both(&sides, "DescribeTable", json!({"TableName": t})).await;
        both(&sides, "ListTagsOfResource", json!({"ResourceArn": format!("arn:scylla:alternator:{ks}:scylla:table/{t}")})).await;

        // CQL sees the same rows, views, schema and CDC log.
        compare_cql(&sides, "base table", &format!("SELECT * FROM \"{ks}\".\"{t}\""), sorted).await;
        compare_cql(&sides, "GSI view", &format!("SELECT * FROM \"{ks}\".\"{t}:gsi\""), sorted).await;
        compare_cql(&sides, "LSI view", &format!("SELECT * FROM \"{ks}\".\"{t}!:lsi\""), sorted).await;
        compare_cql(&sides, "keyspace", &format!("SELECT keyspace_name, durable_writes, replication FROM system_schema.keyspaces WHERE keyspace_name = '{ks}'"), |r| r).await;
        compare_cql(&sides, "tablets", &format!("SELECT keyspace_name, initial_tablets FROM system_schema.scylla_keyspaces WHERE keyspace_name = '{ks}'"), |r| r).await;
        compare_cql(&sides, "columns", &format!("SELECT * FROM system_schema.columns WHERE keyspace_name = '{ks}'"), no_paxos).await;
        compare_cql(&sides, "computed columns", &format!("SELECT * FROM system_schema.computed_columns WHERE keyspace_name = '{ks}'"), sorted).await;
        compare_cql(&sides, "views", &format!("SELECT view_name, base_table_name, include_all_columns, where_clause, extensions FROM system_schema.views WHERE keyspace_name = '{ks}'"), sorted).await;
        let q = format!("SELECT table_name, extensions FROM system_schema.tables WHERE keyspace_name = '{ks}' AND table_name IN ('{t}', '{t}_scylla_cdc_log')");
        let ext = |r: Rows| -> Vec<String> { r.iter().map(|x| format!("{:?} {:?}", x[0], extensions(&x[1]))).collect() };
        assert_eq!(ext(select(&sides[0].cql, &q).await), ext(select(&sides[1].cql, &q).await), "table extensions");
        compare_cql(
            &sides,
            "CDC log",
            &format!("SELECT \"cdc$time\", \"cdc$batch_seq_no\", \"cdc$operation\", p, c, \":attrs\", \"cdc$deleted_:attrs\", \"cdc$deleted_elements_:attrs\", \"cdc$end_of_batch\" FROM \"{ks}\".\"{t}_scylla_cdc_log\""),
            log_rows,
        )
        .await;

        // A batch (ScyllaDB gives its writes one CDC timestamp, so its log groups differ: the
        // log was compared above), then the tables again.
        both(&sides, "BatchWriteItem", json!({"RequestItems": {t.clone(): [
            {"PutRequest": {"Item": {"p": {"S": "d"}, "c": {"N": "1"}, "g": {"S": "red"}}}},
            {"DeleteRequest": {"Key": {"p": {"S": "c"}, "c": {"N": "0"}}}}]}})).await;
        both(&sides, "Query", json!({"TableName": t, "IndexName": "gsi", "KeyConditionExpression": "g = :g", "ExpressionAttributeValues": {":g": {"S": "red"}}})).await;
        compare_cql(&sides, "base table after the batch", &format!("SELECT * FROM \"{ks}\".\"{t}\""), sorted).await;
        compare_cql(&sides, "GSI view after the batch", &format!("SELECT * FROM \"{ks}\".\"{t}:gsi\""), sorted).await;

        // CQL writes are items on both sides.
        for s in &sides {
            let q = format!("INSERT INTO \"{ks}\".\"{t}\" (p, c, \":attrs\") VALUES ('w', 3, {{'g': 0x00796f, 'k': 0x03000000020096}})");
            s.cql.query(&q, QueryParams::default()).await.unwrap();
            let q = format!("UPDATE \"{ks}\".\"{t}\" SET \":attrs\"['b'] = 0x0201 WHERE p = 'w' AND c = 3");
            s.cql.query(&q, QueryParams::default()).await.unwrap();
        }
        both(&sides, "GetItem", json!({"TableName": t, "Key": {"p": {"S": "w"}, "c": {"N": "3"}}})).await;
        both(&sides, "Query", json!({"TableName": t, "IndexName": "gsi", "KeyConditionExpression": "g = :g", "ExpressionAttributeValues": {":g": {"S": "yo"}}})).await;
    };
    let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(result)).await;
    for s in &sides {
        api(&s.url, "DeleteTable", &json!({ "TableName": t })).await;
    }
    if let Err(e) = outcome {
        std::panic::resume_unwind(e);
    }
}
