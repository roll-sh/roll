//! `cql://` imports from the shared reference containers (ScyllaDB `127.0.0.1:19042`,
//! Cassandra `127.0.0.1:29042`; `ROLL_NOSQL_REF_SCYLLA` / `ROLL_NOSQL_REF_CASSANDRA` override):
//! a source keyspace with every type, statics, TTLs, write times, counters and key-only rows is
//! imported and compared row for row, write times and TTLs included; then a refresh after a
//! delete at the source. Skipped when a reference isn't reachable.

mod common;

use bytes::Bytes;
use common::*;
use nosql_control::import::{TableShape, token_ranges};
use nosql_proto::client::{Client, ClientConfig};
use nosql_proto::{QueryParams, Value as Wire};
use nosql_types::Dialect;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::Duration;

fn reference(var: &str, default: &str) -> Option<String> {
    let addr = std::env::var(var).unwrap_or_else(|_| default.to_string());
    let sock: std::net::SocketAddr = addr.parse().ok()?;
    std::net::TcpStream::connect_timeout(&sock, Duration::from_secs(2)).ok()?;
    Some(addr)
}

fn unique(prefix: &str) -> String {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    format!("{prefix}_{}_{}", std::process::id(), n % 1_000_000_000)
}

/// Rows of `ks.t` as the import reads them (plain values, then write times and TTLs), sorted:
/// partition by partition (ScyllaDB's range scans show a static-only partition with the
/// clustering key of the row before it), the partitions found by a token-range scan.
async fn snapshot(c: &Client, shape: &TableShape, d: Dialect) -> Vec<Vec<Option<Bytes>>> {
    let (lo, hi) = token_ranges(1)[0];
    let params = QueryParams {
        values: vec![Wire::Set(Bytes::copy_from_slice(&lo.to_be_bytes())), Wire::Set(Bytes::copy_from_slice(&hi.to_be_bytes()))],
        page_size: Some(100),
        ..Default::default()
    };
    let npk = shape.partition_key.len();
    let mut keys: Vec<Vec<Option<Bytes>>> = c.pages(&shape.select(d), params).collect().await.unwrap().into_iter().map(|r| r[..npk].to_vec()).collect();
    keys.sort();
    keys.dedup();
    let mut rows = vec![];
    for k in keys {
        let values = k.into_iter().map(|v| v.map_or(Wire::Null, Wire::Set)).collect();
        rows.extend(c.pages(&shape.select_partition(d), QueryParams { values, page_size: Some(100), ..Default::default() }).collect().await.unwrap());
    }
    rows.sort();
    rows
}

/// Compares two snapshots: values and write times exactly, TTLs within a few seconds (they
/// count down between the two reads).
fn same(shape: &TableShape, a: &[Vec<Option<Bytes>>], b: &[Vec<Option<Bytes>>]) {
    assert_eq!(a.len(), b.len(), "{}: row counts", shape.name);
    let plain = shape.partition_key.len() + shape.clustering.len() + shape.statics.len() + shape.regulars.len();
    for (x, y) in a.iter().zip(b) {
        assert_eq!(x[..plain], y[..plain], "{}: values", shape.name);
        for (i, (p, q)) in x[plain..].iter().zip(&y[plain..]).enumerate() {
            if i % 2 == 0 {
                assert_eq!(p, q, "{}: write time of timed column {}", shape.name, i / 2);
            } else {
                let ttl = |v: &Option<Bytes>| v.as_deref().map(|b| i32::from_be_bytes(b.try_into().unwrap()));
                match (ttl(p), ttl(q)) {
                    (Some(s), Some(r)) => assert!((s - r).abs() <= 5, "{}: ttl {s} vs {r}", shape.name),
                    (s, r) => assert_eq!(s, r, "{}: ttl", shape.name),
                }
            }
        }
    }
}

async fn shape_of(c: &Client, ks: &str, t: &str) -> TableShape {
    let cols = q(c, &format!("SELECT column_name, kind, position, type FROM system_schema.columns WHERE keyspace_name = '{ks}' AND table_name = '{t}'")).await;
    let cols: Vec<(String, String, i32, String)> = cols.iter().map(|r| (text(&r[0]), text(&r[1]), int(&r[2]), text(&r[3]))).collect();
    let udts: HashSet<String> = q(c, &format!("SELECT type_name FROM system_schema.types WHERE keyspace_name = '{ks}'")).await.iter().map(|r| text(&r[0])).collect();
    TableShape::from_columns(ks, t, &cols, &udts)
}

async fn source_schema(src: &Client, ks: &str, dialect: &str) {
    let tablets = if dialect == "scylla" { " AND tablets = {'enabled': false}" } else { "" };
    exec(src, &format!("CREATE KEYSPACE {ks} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}{tablets}")).await;
    exec(src, &format!("CREATE TYPE {ks}.addr (street text, zip int)")).await;
    let vector = if dialect == "cassandra" { ", vec vector<float, 3>" } else { "" };
    exec(
        src,
        &format!(
            "CREATE TABLE {ks}.all_types (k int, c text, s text static,
              a_ascii ascii, a_bigint bigint, a_blob blob, a_bool boolean, a_date date, a_decimal decimal, a_double double,
              a_duration duration, a_float float, a_inet inet, a_int int, a_smallint smallint, a_text text, a_time time,
              a_ts timestamp, a_timeuuid timeuuid, a_tinyint tinyint, a_uuid uuid, a_varchar varchar, a_varint varint,
              l list<int>, st set<text>, m map<text, int>, fl frozen<list<int>>, tup tuple<int, text>, u addr, fu frozen<addr>{vector},
              PRIMARY KEY (k, c)) WITH comment = 'every type' AND default_time_to_live = 0"
        ),
    )
    .await;
    exec(src, &format!("CREATE TABLE {ks}.counts (k text PRIMARY KEY, hits counter, misses counter)")).await;
    exec(src, &format!("CREATE TABLE {ks}.keys_only (a int, b int, PRIMARY KEY ((a, b)))")).await;
    exec(src, &format!("CREATE TABLE {ks}.\"Quoted\" (\"Key\" int PRIMARY KEY, \"Val\" text)")).await;
}

async fn source_data(src: &Client, ks: &str, dialect: &str) {
    let vector = if dialect == "cassandra" { ", vec" } else { "" };
    let vector_v = if dialect == "cassandra" { ", [1.5, 2.5, 3.5]" } else { "" };
    for k in 0..20 {
        exec(
            src,
            &format!(
                "INSERT INTO {ks}.all_types (k, c, a_ascii, a_bigint, a_blob, a_bool, a_date, a_decimal, a_double, a_duration, a_float, a_inet,
                  a_int, a_smallint, a_text, a_time, a_ts, a_timeuuid, a_tinyint, a_uuid, a_varchar, a_varint, l, st, m, fl, tup, u, fu{vector})
                 VALUES ({k}, 'c{k}', 'asc', {big}, 0xcafe{k:02x}, true, '2024-02-29', 12.345, 1.25e300, 1h2m3s, 3.5, '10.0.0.{k}',
                  {k}, 7, 'tëxt {k}', '08:12:54.123456789', '2024-01-02 03:04:05.678+0000', now(), -3, 6f1c0b0a-1d4c-4a3b-9c1e-0123456789ab,
                  'vc', 123456789012345678901234567890, [1, 2, {k}], {{'x', 'y'}}, {{'a': 1, 'b': {k}}}, [9, 8], (1, 'one'),
                  {{street: 'Main', zip: {k}}}, {{street: 'Frozen', zip: 1}}{vector_v})
                 USING TIMESTAMP {ts} AND TTL {ttl}",
                big = i64::MAX - k,
                ts = 1_700_000_000_000_000i64 + k * 1000,
                ttl = 86_400 + k * 60
            ),
        )
        .await;
        // A second clustering row with other write times, and some cells without a TTL.
        exec(src, &format!("INSERT INTO {ks}.all_types (k, c, a_int, a_text) VALUES ({k}, 'd', {k}, 'no ttl') USING TIMESTAMP {}", 1_600_000_000_000_000i64 + k)).await;
        exec(src, &format!("UPDATE {ks}.all_types USING TIMESTAMP {} AND TTL 5000 SET a_text = 'updated' WHERE k = {k} AND c = 'd'", 1_650_000_000_000_000i64 + k)).await;
        exec(src, &format!("UPDATE {ks}.all_types USING TIMESTAMP 1690000000000000 SET s = 'static {k}' WHERE k = {k}")).await;
        // A key-only row (no regular cells) and a null-heavy row.
        exec(src, &format!("INSERT INTO {ks}.all_types (k, c) VALUES ({k}, 'e')")).await;
        exec(src, &format!("UPDATE {ks}.counts SET hits = hits + {}, misses = misses - 2 WHERE k = 'p{k}'", k + 1)).await;
        exec(src, &format!("INSERT INTO {ks}.keys_only (a, b) VALUES ({k}, {})", k * 2)).await;
        exec(src, &format!("INSERT INTO {ks}.\"Quoted\" (\"Key\", \"Val\") VALUES ({k}, 'Q{k}')")).await;
    }
    // A partition with only a static cell.
    exec(src, &format!("INSERT INTO {ks}.all_types (k, s) VALUES (100, 'lonely static') USING TTL 3000")).await;
}

const TABLES: &[&str] = &["all_types", "counts", "keys_only", "Quoted"];

async fn wait_import(e: &Env, db: &str, imp: &str) -> Value {
    for _ in 0..600 {
        let list = e.get(&format!("/v1/nosql/databases/{db}/imports")).await;
        let i = list.as_array().unwrap().iter().find(|i| i["id"] == imp).cloned().unwrap();
        if i["state"] == "done" || i["state"] == "failed" {
            return i;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("import {imp} didn't finish");
}

async fn import_from(dialect: &str, addr: &str) {
    let d = if dialect == "scylla" { Dialect::Scylla } else { Dialect::Cassandra };
    let src = Client::connect(ClientConfig::new(addr)).await.unwrap();
    let ks = unique("ctl_imp");
    source_schema(&src, &ks, dialect).await;
    source_data(&src, &ks, dialect).await;

    let e = env().await;
    let db = e.post("/v1/nosql/databases", json!({"name": "mirror", "dialect": dialect})).await;
    let dbid = id(&db);
    let imp = e.post(&format!("/v1/nosql/databases/{dbid}/imports"), json!({"from": format!("cql://{addr}"), "keyspaces": [ks], "every": 86_400})).await;
    let done = wait_import(&e, &dbid, imp["id"].as_str().unwrap()).await;
    assert_eq!(done["state"], "done", "{dialect}: {done}");
    assert_eq!(done["tables_total"], 4);
    assert_eq!(done["tables_done"], 4);
    assert_eq!(done["runs"], 1);
    assert_eq!(done["rows"], 20 * 3 + 1 + 20 * 3, "{done}");
    assert!(done["next_run_at"].as_i64().unwrap() > nosql_control::control::now() + 86_000);

    let dst = e.connect(&db).await;
    for t in TABLES {
        let shape = shape_of(&src, &ks, t).await;
        let mine = shape_of(&dst, &ks, t).await;
        assert_eq!((&mine.partition_key, &mine.clustering, &mine.statics, &mine.regulars), (&shape.partition_key, &shape.clustering, &shape.statics, &shape.regulars), "{dialect} {t}: schema");
        same(&shape, &snapshot(&src, &shape, d).await, &snapshot(&dst, &shape, d).await);
        // Every cell, collections included.
        let all = format!("SELECT * FROM {ks}.{}", nosql_types::quote_identifier(t, d));
        let (mut a, mut b) = (q(&src, &all).await, q(&dst, &all).await);
        a.sort();
        b.sort();
        assert_eq!(a, b, "{dialect} {t}: SELECT *");
    }
    let comment = q(&dst, &format!("SELECT comment FROM system_schema.tables WHERE keyspace_name = '{ks}' AND table_name = 'all_types'")).await;
    assert_eq!(text(&comment[0][0]), "every type");

    // A refresh: deletes at the source disappear, new rows appear, and the run is a new
    // manifest version.
    exec(&src, &format!("DELETE FROM {ks}.all_types WHERE k = 3")).await;
    exec(&src, &format!("INSERT INTO {ks}.keys_only (a, b) VALUES (500, 1)")).await;
    let versions = |e: &Env| {
        let objects = e.control.objects().clone();
        let id = dbid.clone();
        async move { nosql_storage::manifest::list_versions(&objects, &id).await.unwrap().len() }
    };
    let before = versions(&e).await;
    drop(dst);
    e.control.run_import(imp["id"].as_str().unwrap()).await.unwrap();
    let again = wait_import(&e, &dbid, imp["id"].as_str().unwrap()).await;
    assert_eq!((again["state"].as_str(), again["runs"].as_i64()), (Some("done"), Some(2)), "{again}");
    assert!(versions(&e).await > before);
    let dst = e.connect(&db).await;
    for t in ["all_types", "keys_only"] {
        let shape = shape_of(&src, &ks, t).await;
        same(&shape, &snapshot(&src, &shape, d).await, &snapshot(&dst, &shape, d).await);
    }
    assert!(q(&dst, &format!("SELECT * FROM {ks}.all_types WHERE k = 3")).await.is_empty());

    // Branches can be taken from the mirror.
    let br = e.post(&format!("/v1/nosql/databases/{dbid}/branches"), json!({"name": "pr"})).await;
    let bc = e.connect(&br).await;
    assert_eq!(q(&bc, &format!("SELECT * FROM {ks}.keys_only")).await.len(), 21);

    // A keyspace the source doesn't have fails the import clearly.
    let bad = e.post(&format!("/v1/nosql/databases/{dbid}/imports"), json!({"from": format!("cql://{addr}"), "keyspaces": ["ctl_missing_ks"]})).await;
    let failed = wait_import(&e, &dbid, bad["id"].as_str().unwrap()).await;
    assert_eq!((failed["state"].as_str(), failed["error"].as_str()), (Some("failed"), Some("the source has no keyspace ctl_missing_ks")));

    exec(&src, &format!("DROP KEYSPACE {ks}")).await;
}

#[tokio::test]
async fn import_from_scylla() {
    let Some(addr) = reference("ROLL_NOSQL_REF_SCYLLA", "127.0.0.1:19042") else {
        eprintln!("skipped: no ScyllaDB reference");
        return;
    };
    import_from("scylla", &addr).await;
}

#[tokio::test]
async fn import_from_cassandra() {
    let Some(addr) = reference("ROLL_NOSQL_REF_CASSANDRA", "127.0.0.1:29042") else {
        eprintln!("skipped: no Cassandra reference");
        return;
    };
    import_from("cassandra", &addr).await;
}
