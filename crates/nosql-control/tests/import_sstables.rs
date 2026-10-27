//! SSTable backup imports (`file://` here; `s3://` goes through the same `Objects` API).
//!
//! - `fixtures`: the committed backups of `crates/nosql-sstable-import/tests/fixtures` (one per
//!   format: Cassandra `nb`, `oa`, BTI `da`; ScyllaDB `me`, `ms`, `mt`; made by `generate.sh`
//!   from `generate.cql` on the references) are imported and every row compared, write times
//!   and TTLs included, with the reference's rows recorded next to them
//!   (`<format>.expected.json.zst`). Then: tombstones still shadow older writes, counters keep
//!   counting, a branch sees the data, a refresh replaces it.
//! - `layouts`: the same SSTables arranged as ScyllaDB Manager and Medusa backups of two
//!   replicas (counters must not double), and across dialects.
//! - `corrupt_backups`: checksum, truncation, missing component and tag errors.
//! - `live_snapshots`: fresh `nodetool snapshot`s of the shared references (skipped when they
//!   or `docker` aren't reachable), compared row for row with the reference itself.
//!
//! `ROLL_IMPORT_REGEN=nb=127.0.0.1:29042,oa=…,da=…,mt=127.0.0.1:19042` rewrites the expected rows
//! from references holding the `imp_fx` keyspace that made the fixtures.

mod common;

use bytes::Bytes;
use common::*;
use nosql_control::import::{TableShape, token_ranges};
use nosql_proto::client::{Client, ClientConfig};
use nosql_proto::{QueryParams, Value as Wire};
use nosql_types::Dialect;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

const KS: &str = "imp_fx";
const TABLES: &[&str] = &["all_types", "c_deflate", "c_lz4", "c_none", "c_snappy", "c_zstd", "colls", "composite", "counters", "deletes", "dropped", "statics", "ttls", "wide"];
const FORMATS: &[(&str, &str)] = &[("nb", "cassandra"), ("oa", "cassandra"), ("da", "cassandra"), ("me", "scylla"), ("ms", "scylla"), ("mt", "scylla")];

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../nosql-sstable-import/tests/fixtures")
}

fn now_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Unpack a fixture into `into` (`imp_fx/<table>-<id>/snapshots/imp_fx/…`).
fn extract(fmt: &str, into: &Path) {
    let raw = std::fs::read(fixture_dir().join(format!("{fmt}.tar.zst"))).unwrap();
    let tar = zstd::decode_all(&raw[..]).unwrap();
    for (path, body) in nosql_sstable_import::tar::entries(&tar).unwrap() {
        let p = into.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
}

fn reference(addr: &str) -> Option<String> {
    let sock: std::net::SocketAddr = addr.parse().ok()?;
    std::net::TcpStream::connect_timeout(&sock, Duration::from_secs(2)).ok()?;
    Some(addr.to_string())
}

// ---- row snapshots (as in imports.rs) -------------------------------------------------------

async fn shape_of(c: &Client, ks: &str, t: &str) -> TableShape {
    let cols = q(c, &format!("SELECT column_name, kind, position, type FROM system_schema.columns WHERE keyspace_name = '{ks}' AND table_name = '{t}'")).await;
    let cols: Vec<(String, String, i32, String)> = cols.iter().map(|r| (text(&r[0]), text(&r[1]), int(&r[2]), text(&r[3]))).collect();
    let udts: HashSet<String> = q(c, &format!("SELECT type_name FROM system_schema.types WHERE keyspace_name = '{ks}'")).await.iter().map(|r| text(&r[0])).collect();
    TableShape::from_columns(ks, t, &cols, &udts)
}

/// Every row of a table: plain values, then `WRITETIME, TTL` of each timed column; sorted.
async fn snapshot(c: &Client, shape: &TableShape, d: Dialect) -> Vec<Vec<Option<Bytes>>> {
    try_snapshot(c, shape, d).await.unwrap()
}

async fn try_snapshot(c: &Client, shape: &TableShape, d: Dialect) -> Result<Vec<Vec<Option<Bytes>>>, nosql_proto::client::ClientError> {
    let (lo, hi) = token_ranges(1)[0];
    let params = QueryParams { values: vec![Wire::Set(Bytes::copy_from_slice(&lo.to_be_bytes())), Wire::Set(Bytes::copy_from_slice(&hi.to_be_bytes()))], page_size: Some(500), ..Default::default() };
    let npk = shape.partition_key.len();
    let mut keys: Vec<Vec<Option<Bytes>>> = c.pages(&shape.select(d), params).collect().await?.into_iter().map(|r| r[..npk].to_vec()).collect();
    keys.sort();
    keys.dedup();
    let mut rows = vec![];
    for k in keys {
        let values = k.into_iter().map(|v| v.map_or(Wire::Null, Wire::Set)).collect();
        rows.extend(c.pages(&shape.select_partition(d), QueryParams { values, page_size: Some(500), ..Default::default() }).collect().await?);
    }
    rows.sort();
    Ok(rows)
}

/// Drops a live test's keyspace and its snapshots on the reference, even when the test fails.
struct Cleanup(&'static str, String);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::process::Command::new(fixture_dir().join("cleanup.sh")).args([self.0, &self.1]).status();
    }
}

/// Values and write times exactly; TTLs within a few seconds, `elapsed` seconds after `a`'s.
fn same(what: &str, shape: &TableShape, a: &[Vec<Option<Bytes>>], b: &[Vec<Option<Bytes>>], elapsed: i64) {
    assert_eq!(a.len(), b.len(), "{what} {}: row counts", shape.name);
    let plain = shape.partition_key.len() + shape.clustering.len() + shape.statics.len() + shape.regulars.len();
    for (x, y) in a.iter().zip(b) {
        assert_eq!(x[..plain], y[..plain], "{what} {}: values", shape.name);
        for (i, (p, r)) in x[plain..].iter().zip(&y[plain..]).enumerate() {
            if i % 2 == 0 {
                assert_eq!(p, r, "{what} {}: write time of timed column {} in {x:?}", shape.name, i / 2);
            } else {
                let ttl = |v: &Option<Bytes>| v.as_deref().map(|b| i32::from_be_bytes(b.try_into().unwrap()) as i64);
                match (ttl(p), ttl(r)) {
                    (Some(s), Some(t)) => assert!((s - elapsed - t).abs() <= 5, "{what} {}: ttl {s} (-{elapsed}s) vs {t}", shape.name),
                    (s, t) => assert_eq!(s, t, "{what} {}: ttl", shape.name),
                }
            }
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Bytes {
    Bytes::from((0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect::<Vec<u8>>())
}

/// The reference's rows of every table, as recorded.
struct Expected {
    captured_at: i64,
    tables: BTreeMap<String, Vec<Vec<Option<Bytes>>>>,
}

fn expected_name(fmt: &str) -> &str {
    if fmt == "me" || fmt == "ms" { "mt" } else { fmt }
}

fn load_expected(fmt: &str) -> Expected {
    let raw = std::fs::read(fixture_dir().join(format!("{}.expected.json.zst", expected_name(fmt)))).unwrap();
    let v: Value = serde_json::from_slice(&zstd::decode_all(&raw[..]).unwrap()).unwrap();
    let mut tables = BTreeMap::new();
    for (t, rows) in v["tables"].as_object().unwrap() {
        let rows = rows.as_array().unwrap().iter().map(|r| r.as_array().unwrap().iter().map(|c| c.as_str().map(unhex)).collect()).collect();
        tables.insert(t.clone(), rows);
    }
    Expected { captured_at: v["captured_at"].as_i64().unwrap(), tables }
}

async fn wait_import(e: &Env, db: &str, imp: &str) -> Value {
    for _ in 0..1500 {
        let list = e.get(&format!("/v1/nosql/databases/{db}/imports")).await;
        let i = list.as_array().unwrap().iter().find(|i| i["id"] == imp).cloned().unwrap();
        if i["state"] == "done" || i["state"] == "failed" {
            return i;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("import {imp} didn't finish");
}

/// Create a database and import `from` into it; returns (db, import).
async fn import(e: &Env, name: &str, dialect: &str, from: &str) -> (Value, Value) {
    let db = e.post("/v1/nosql/databases", json!({"name": name, "dialect": dialect})).await;
    let imp = e.post(&format!("/v1/nosql/databases/{}/imports", id(&db)), json!({"from": from})).await;
    let done = wait_import(e, &id(&db), imp["id"].as_str().unwrap()).await;
    (db, done)
}

fn dialect_of(d: &str) -> Dialect {
    if d == "scylla" { Dialect::Scylla } else { Dialect::Cassandra }
}

/// Every fixture table of `dst` equals the recorded reference rows.
async fn compare_expected(what: &str, dst: &Client, d: Dialect, exp: &Expected) {
    for t in TABLES {
        let shape = shape_of(dst, KS, t).await;
        assert!(!shape.partition_key.is_empty(), "{what}: table {t} wasn't recreated");
        let mine = snapshot(dst, &shape, d).await;
        same(what, &shape, &exp.tables[*t], &mine, now_s() - exp.captured_at);
    }
}

// ---- tests ----------------------------------------------------------------------------------

#[tokio::test]
async fn regenerate_expected() {
    let Ok(spec) = std::env::var("ROLL_IMPORT_REGEN") else { return };
    for item in spec.split(',') {
        let (fmt, addr) = item.split_once('=').unwrap();
        let d = if fmt.starts_with('m') { Dialect::Scylla } else { Dialect::Cassandra };
        let c = Client::connect(ClientConfig::new(addr)).await.unwrap();
        let mut tables = serde_json::Map::new();
        let captured_at = now_s();
        for t in TABLES {
            let shape = shape_of(&c, KS, t).await;
            let rows = snapshot(&c, &shape, d).await;
            tables.insert(t.to_string(), Value::Array(rows.iter().map(|r| Value::Array(r.iter().map(|v| v.as_deref().map_or(Value::Null, |b| Value::String(hex(b)))).collect())).collect()));
        }
        let json = serde_json::to_vec(&json!({"captured_at": captured_at, "tables": tables})).unwrap();
        std::fs::write(fixture_dir().join(format!("{fmt}.expected.json.zst")), zstd::encode_all(&json[..], 19).unwrap()).unwrap();
    }
}

#[tokio::test]
async fn fixtures_import_exactly() {
    let e = env().await;
    for (fmt, dialect) in FORMATS {
        let dir = tempfile::tempdir().unwrap();
        extract(fmt, dir.path());
        let (db, done) = import(&e, &format!("fx-{fmt}"), dialect, &format!("file://{}", dir.path().display())).await;
        assert_eq!(done["state"], "done", "{fmt}: {done}");
        assert_eq!((done["tables_total"].as_i64(), done["tables_done"].as_i64()), (Some(TABLES.len() as i64), Some(TABLES.len() as i64)), "{fmt}: {done}");
        assert!(done["rows"].as_i64().unwrap() > 2000, "{fmt}: {done}");
        let d = dialect_of(dialect);
        let dst = e.connect(&db).await;
        compare_expected(fmt, &dst, d, &load_expected(fmt)).await;
        // The dropped column stays dropped; the user types came along.
        let cols = texts(&dst, &format!("SELECT column_name FROM system_schema.columns WHERE keyspace_name = '{KS}' AND table_name = 'dropped'")).await;
        assert_eq!(cols, ["k", "keep"], "{fmt}");
        assert_eq!(texts(&dst, &format!("SELECT type_name FROM system_schema.types WHERE keyspace_name = '{KS}'")).await, ["addr", "point"], "{fmt}");
        if *fmt != "nb" && *fmt != "mt" {
            continue;
        }

        // Imported tombstones still shadow older writes: a row deletion, a range deletion at
        // timestamp 1000, a partition deletion.
        for (w, visible) in [
            (format!("INSERT INTO {KS}.deletes (k, c1, c2, v) VALUES (1, 2, 'old', 1) USING TIMESTAMP 1"), false),
            (format!("INSERT INTO {KS}.deletes (k, c1, c2, v) VALUES (1, 3, 'z', 2) USING TIMESTAMP 999"), false),
            (format!("INSERT INTO {KS}.deletes (k, c1, c2, v) VALUES (1, 4, 'z', 3) USING TIMESTAMP 1001"), true),
            (format!("INSERT INTO {KS}.deletes (k, c1, c2, v) VALUES (3, 1, 'z', 4) USING TIMESTAMP 5"), false),
        ] {
            exec(&dst, &w).await;
            let probe = w.split("VALUES (").nth(1).unwrap().split(", ").take(3).collect::<Vec<_>>();
            let found = q(&dst, &format!("SELECT v FROM {KS}.deletes WHERE k = {} AND c1 = {} AND c2 = {}", probe[0], probe[1], probe[2])).await;
            assert_eq!(!found.is_empty(), visible, "{fmt}: {w}");
        }
        // Counters keep counting from the imported totals.
        exec(&dst, &format!("UPDATE {KS}.counters SET n = n + 1 WHERE k = 1 AND c = 1")).await;
        let n = q(&dst, &format!("SELECT n, m FROM {KS}.counters WHERE k = 1 AND c = 1")).await;
        assert_eq!((i64::from_be_bytes(n[0][0].as_deref().unwrap().try_into().unwrap()), i64::from_be_bytes(n[0][1].as_deref().unwrap().try_into().unwrap())), (13, -3), "{fmt}");
        // A branch right after the import sees everything.
        let br = e.post(&format!("/v1/nosql/databases/{}/branches", id(&db)), json!({"name": format!("pr-{fmt}")})).await;
        let bc = e.connect(&br).await;
        assert_eq!(q(&bc, &format!("SELECT c FROM {KS}.wide WHERE k = 1")).await.len(), 1450, "{fmt}");
        // A refresh replaces the data: the extra writes and the increment are gone.
        let imp = e.get(&format!("/v1/nosql/databases/{}/imports", id(&db))).await[0].clone();
        e.control.run_import(imp["id"].as_str().unwrap()).await.unwrap();
        let again = wait_import(&e, &id(&db), imp["id"].as_str().unwrap()).await;
        assert_eq!((again["state"].as_str(), again["runs"].as_i64()), (Some("done"), Some(2)), "{fmt}: {again}");
        let dst = e.connect(&db).await;
        compare_expected(&format!("{fmt} refresh"), &dst, d, &load_expected(fmt)).await;
    }
}

/// The ScyllaDB Manager layout (two replicas holding the same SSTables).
fn manager_layout(src: &Path, out: &Path) {
    let mut index = vec![];
    let mut schema_rows = vec![
        json!({"keyspace": KS, "type": "keyspace", "name": KS, "cql_stmt": format!("CREATE KEYSPACE {KS} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 3}} AND durable_writes = true;")}),
        json!({"keyspace": KS, "type": "type", "name": "addr", "cql_stmt": format!("CREATE TYPE {KS}.addr (street text, zip int, tags frozen<set<text>>);")}),
        json!({"keyspace": KS, "type": "type", "name": "point", "cql_stmt": format!("CREATE TYPE {KS}.point (x double, y double);")}),
    ];
    for table_dir in std::fs::read_dir(src.join(KS)).unwrap() {
        let table_dir = table_dir.unwrap().path();
        let dirname = table_dir.file_name().unwrap().to_string_lossy().to_string();
        let (table, _) = dirname.rsplit_once('-').unwrap();
        let snap = table_dir.join("snapshots").join(KS);
        let mut files = vec![];
        for f in std::fs::read_dir(&snap).unwrap() {
            let f = f.unwrap().path();
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            if name == "schema.cql" {
                schema_rows.push(json!({"keyspace": KS, "type": "table", "name": table, "cql_stmt": std::fs::read_to_string(&f).unwrap()}));
                continue;
            }
            if name == "manifest.json" {
                continue;
            }
            for node in ["n1", "n2"] {
                let d = out.join(format!("backup/sst/cluster/c1/dc/dc1/node/{node}/keyspace/{KS}/table/{table}/5a8b1ca866023f77a0459273d308917a"));
                std::fs::create_dir_all(&d).unwrap();
                std::fs::copy(&f, d.join(&name)).unwrap();
            }
            files.push(name);
        }
        index.push(json!({"keyspace": KS, "table": table, "replication_type": "vnodes", "version": "5a8b1ca866023f77a0459273d308917a", "files": files, "size": 1}));
    }
    let gz = |v: &Value| {
        let mut g = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut g, v.to_string().as_bytes()).unwrap();
        g.finish().unwrap()
    };
    let tag = "sm_20261020120000UTC";
    for node in ["n1", "n2"] {
        let d = out.join(format!("backup/meta/cluster/c1/dc/dc1/node/{node}"));
        std::fs::create_dir_all(&d).unwrap();
        let m = json!({"version": "v2", "cluster_name": "test", "index": index, "dc": "dc1", "node_id": node, "task_id": "t1", "snapshot_tag": tag});
        std::fs::write(d.join(format!("task_t1_tag_{tag}_manifest.json.gz")), gz(&m)).unwrap();
        // An older, incomplete run is ignored.
        std::fs::write(d.join("task_t1_tag_sm_20261019120000UTC_manifest.json.gz.tmp"), b"partial").unwrap();
    }
    let s = out.join("backup/schema/cluster/c1");
    std::fs::create_dir_all(&s).unwrap();
    std::fs::write(s.join(format!("task_t1_tag_{tag}_schema_with_internals.json.gz")), gz(&Value::Array(schema_rows))).unwrap();
}

/// A differential Medusa backup of two nodes holding the same SSTables.
fn medusa_layout(src: &Path, out: &Path) {
    let mut schema = format!("CREATE KEYSPACE {KS} WITH replication = {{'class': 'NetworkTopologyStrategy', 'dc1': '3'}} AND durable_writes = true;\n");
    let mut seen_types = false;
    for node in ["node1.example", "node2.example"] {
        let mut manifest = vec![];
        for table_dir in std::fs::read_dir(src.join(KS)).unwrap() {
            let table_dir = table_dir.unwrap().path();
            let dirname = table_dir.file_name().unwrap().to_string_lossy().to_string();
            let snap = table_dir.join("snapshots").join(KS);
            let mut objects = vec![];
            for f in std::fs::read_dir(&snap).unwrap() {
                let f = f.unwrap().path();
                let name = f.file_name().unwrap().to_string_lossy().to_string();
                if name == "schema.cql" {
                    if node == "node1.example" {
                        let text = std::fs::read_to_string(&f).unwrap();
                        // Medusa's schema is one DESCRIBE: each type once.
                        for stmt in text.split(";\n") {
                            if stmt.trim().is_empty() || (stmt.contains("CREATE TYPE") && seen_types) {
                                continue;
                            }
                            schema.push_str(stmt.trim());
                            schema.push_str(";\n");
                        }
                        seen_types |= text.contains("CREATE TYPE");
                    }
                    continue;
                }
                if name == "manifest.json" {
                    continue;
                }
                let d = out.join(format!("{node}/data/{KS}/{dirname}"));
                std::fs::create_dir_all(&d).unwrap();
                std::fs::copy(&f, d.join(&name)).unwrap();
                objects.push(json!({"path": format!("backups-bucket/{node}/data/{KS}/{dirname}/{name}"), "MD5": "x", "size": 1}));
            }
            manifest.push(json!({"keyspace": KS, "columnfamily": dirname, "objects": objects}));
        }
        let meta = out.join(format!("{node}/2026102012/meta"));
        std::fs::create_dir_all(&meta).unwrap();
        std::fs::write(meta.join("manifest.json"), Value::Array(manifest).to_string()).unwrap();
        std::fs::write(meta.join("schema.cql"), &schema).unwrap();
    }
}

#[tokio::test]
async fn layouts() {
    let e = env().await;
    // ScyllaDB Manager, from the ScyllaDB fixture.
    let src = tempfile::tempdir().unwrap();
    extract("mt", src.path());
    let mgr = tempfile::tempdir().unwrap();
    manager_layout(src.path(), mgr.path());
    let (db, done) = import(&e, "manager", "scylla", &format!("file://{}", mgr.path().display())).await;
    assert_eq!(done["state"], "done", "{done}");
    compare_expected("manager", &e.connect(&db).await, Dialect::Scylla, &load_expected("mt")).await;

    // Medusa, from the Cassandra fixture.
    let src = tempfile::tempdir().unwrap();
    extract("nb", src.path());
    let med = tempfile::tempdir().unwrap();
    medusa_layout(src.path(), med.path());
    let (db, done) = import(&e, "medusa", "cassandra", &format!("file://{}", med.path().display())).await;
    assert_eq!(done["state"], "done", "{done}");
    let dst = e.connect(&db).await;
    compare_expected("medusa", &dst, Dialect::Cassandra, &load_expected("nb")).await;
    // The keyspace definition came from the backup (its datacenter is renamed to the
    // database's when the dialect checks it, or kept as written).
    assert_eq!(q(&dst, &format!("SELECT keyspace_name FROM system_schema.keyspaces WHERE keyspace_name = '{KS}'")).await.len(), 1);

    // Across dialects: ScyllaDB's sstables (and options) into a Cassandra database and back.
    for (fmt, dialect) in [("mt", "cassandra"), ("oa", "scylla")] {
        let dir = tempfile::tempdir().unwrap();
        extract(fmt, dir.path());
        let (db, done) = import(&e, &format!("cross-{fmt}"), dialect, &format!("file://{}", dir.path().display())).await;
        assert_eq!(done["state"], "done", "{fmt} into {dialect}: {done}");
        compare_expected(&format!("{fmt} into {dialect}"), &e.connect(&db).await, dialect_of(dialect), &load_expected(fmt)).await;
    }

    // One keyspace of several, and a snapshot tag chosen by name.
    let dir = tempfile::tempdir().unwrap();
    extract("da", dir.path());
    let db = e.post("/v1/nosql/databases", json!({"name": "picked", "dialect": "cassandra"})).await;
    let imp = e.post(&format!("/v1/nosql/databases/{}/imports", id(&db)), json!({"from": format!("file://{}?tag={KS}", dir.path().display()), "keyspaces": [KS]})).await;
    assert_eq!(wait_import(&e, &id(&db), imp["id"].as_str().unwrap()).await["state"], "done");
    let imp = e.post(&format!("/v1/nosql/databases/{}/imports", id(&db)), json!({"from": format!("file://{}", dir.path().display()), "keyspaces": ["other_ks"]})).await;
    let failed = wait_import(&e, &id(&db), imp["id"].as_str().unwrap()).await;
    assert_eq!((failed["state"].as_str(), failed["error"].as_str()), (Some("failed"), Some("the backup has no keyspace other_ks")));
}

#[tokio::test]
async fn corrupt_backups() {
    let e = env().await;
    let fail = |name: &'static str, from: String| {
        let e = &e;
        async move {
            let (_, done) = import(e, name, "cassandra", &from).await;
            assert_eq!(done["state"], "failed", "{name}: {done}");
            done["error"].as_str().unwrap().to_string()
        }
    };
    let data = |dir: &Path, table: &str, component: &str| -> PathBuf {
        let t = std::fs::read_dir(dir.join(KS)).unwrap().map(|d| d.unwrap().path()).find(|p| p.file_name().unwrap().to_string_lossy().starts_with(&format!("{table}-"))).unwrap();
        let snap = t.join("snapshots").join(KS);
        std::fs::read_dir(&snap).unwrap().map(|d| d.unwrap().path()).find(|p| p.to_string_lossy().ends_with(component)).unwrap()
    };
    // A flipped byte in a compressed chunk.
    let dir = tempfile::tempdir().unwrap();
    extract("nb", dir.path());
    let f = data(dir.path(), "wide", "-Data.db");
    let mut b = std::fs::read(&f).unwrap();
    let mid = b.len() / 2;
    b[mid] ^= 0x55;
    std::fs::write(&f, b).unwrap();
    let err = fail("bitflip", format!("file://{}", dir.path().display())).await;
    assert!(err.contains("checksum mismatch") && err.contains("wide"), "{err}");
    // A truncated data file.
    let dir = tempfile::tempdir().unwrap();
    extract("oa", dir.path());
    let f = data(dir.path(), "c_lz4", "-Data.db");
    let b = std::fs::read(&f).unwrap();
    std::fs::write(&f, &b[..b.len() - 50]).unwrap();
    let err = fail("truncated", format!("file://{}", dir.path().display())).await;
    assert!(err.contains("c_lz4"), "{err}");
    // An uncompressed file whose CRC.db disagrees.
    let dir = tempfile::tempdir().unwrap();
    extract("mt", dir.path());
    let f = data(dir.path(), "c_none", "-Data.db");
    let mut b = std::fs::read(&f).unwrap();
    b[10] ^= 1;
    std::fs::write(&f, b).unwrap();
    let err = fail("crc", format!("file://{}", dir.path().display())).await;
    assert!(err.contains("checksum mismatch") || err.contains("Digest"), "{err}");
    // A missing Statistics.db.
    let dir = tempfile::tempdir().unwrap();
    extract("da", dir.path());
    std::fs::remove_file(data(dir.path(), "ttls", "-Statistics.db")).unwrap();
    let err = fail("missing", format!("file://{}", dir.path().display())).await;
    assert!(err.contains("incomplete") && err.contains("Statistics.db"), "{err}");
    // An unknown tag, an empty directory, a bad URL.
    let dir = tempfile::tempdir().unwrap();
    extract("da", dir.path());
    let err = fail("tag", format!("file://{}?tag=nightly", dir.path().display())).await;
    assert!(err.contains("no snapshot nightly"), "{err}");
    let empty = tempfile::tempdir().unwrap();
    let err = fail("empty", format!("file://{}", empty.path().display())).await;
    assert!(err.contains("empty"), "{err}");
    let db = e.post("/v1/nosql/databases", json!({"name": "badurl"})).await;
    let (s, v) = e.call(reqwest::Method::POST, &format!("/v1/nosql/databases/{}/imports", id(&db)), Some(json!({"from": "file://relative/path"}))).await;
    assert_eq!(s, 400, "{v}");
    let (s, _) = e.call(reqwest::Method::POST, &format!("/v1/nosql/databases/{}/imports", id(&db)), Some(json!({"from": "file:///nonexistent/roll/backup"}))).await;
    assert_eq!(s, 400);
}

/// `generate.sh` on a reference, imported and compared with the reference itself.
#[tokio::test]
async fn live_snapshots() {
    let docker = std::process::Command::new("docker").arg("version").output().is_ok_and(|o| o.status.success());
    let refs = [("nosql-ref-cassandra", "127.0.0.1:29042", "cassandra", vec![]), ("nosql-ref-scylla", "127.0.0.1:19042", "scylla", vec!["me"])];
    let e = env().await;
    for (container, addr, dialect, extra) in refs {
        if !docker || reference(addr).is_none() {
            eprintln!("skipped: {container} isn't reachable");
            continue;
        }
        let ks = format!("imp_live_{}_{}", std::process::id(), now_s() % 100_000);
        let out = tempfile::tempdir().unwrap();
        let snap = out.path().join("snap");
        let script = fixture_dir().join("generate.sh");
        let mut cmd = std::process::Command::new(&script);
        cmd.args([container, &ks, &snap.to_string_lossy()]).args(&extra);
        let _cleanup = Cleanup(container, ks.clone());
        let status = tokio::task::spawn_blocking(move || cmd.status()).await.unwrap().unwrap();
        assert!(status.success(), "generate.sh on {container} failed");
        let src = Client::connect(ClientConfig::new(addr)).await.unwrap();
        let d = dialect_of(dialect);
        let mut dirs = vec![(String::from("snapshot"), snap.clone())];
        for v in &extra {
            dirs.push((v.to_string(), PathBuf::from(format!("{}-{v}", snap.display()))));
        }
        for (label, dir) in dirs {
            let (db, done) = import(&e, &format!("live-{dialect}-{label}"), dialect, &format!("file://{}", dir.display())).await;
            assert_eq!(done["state"], "done", "{container} {label}: {done}");
            let dst = e.connect(&db).await;
            for t in TABLES {
                let shape = shape_of(&src, &ks, t).await;
                // Cassandra 5.0.9 sometimes fails its own reads of a table with a dropped
                // column ("[gone, keep] is not a subset of [keep]" in Columns.encodeBitmap):
                // retry, as a client would.
                let mut a = try_snapshot(&src, &shape, d).await;
                for _ in 0..5 {
                    if a.is_ok() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    a = try_snapshot(&src, &shape, d).await;
                }
                let a = a.unwrap_or_else(|e| panic!("reading {t} on {container}: {e}"));
                let b = snapshot(&dst, &shape, d).await;
                same(&format!("live {container} {label}"), &shape, &a, &b, 0);
            }
        }
    }
}

/// An `s3://` backup: a fixture uploaded to the test bucket (`ROLL_E2E_S3_BUCKET`, with
/// `ROLL_E2E_S3_ENDPOINT` passed as the URL's `endpoint`), imported through ranged GETs, plus an
/// empty prefix and a missing bucket. Skipped without a bucket.
#[tokio::test]
async fn s3_backup() {
    let Ok(bucket) = std::env::var("ROLL_E2E_S3_BUCKET") else { return };
    let endpoint = std::env::var("ROLL_E2E_S3_ENDPOINT").ok();
    let region = std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".into());
    let unique = format!("nosql-tests/import-{}-{}", std::process::id(), now_s());
    let objects = nosql_storage::Objects::new(&nosql_storage::StorageConfig::S3 { bucket: bucket.clone(), region: Some(region.clone()), endpoint: endpoint.clone(), prefix: Some(unique.clone()) }).unwrap();
    let dir = tempfile::tempdir().unwrap();
    extract("me", dir.path());
    let mut files = vec![dir.path().to_path_buf()];
    while let Some(p) = files.pop() {
        if p.is_dir() {
            files.extend(std::fs::read_dir(&p).unwrap().map(|d| d.unwrap().path()));
        } else {
            let key = p.strip_prefix(dir.path()).unwrap().to_string_lossy().into_owned();
            objects.put(&format!("backup/{key}"), Bytes::from(std::fs::read(&p).unwrap())).await.unwrap();
        }
    }
    let url = |prefix: &str| {
        let mut u = format!("s3://{bucket}/{unique}/{prefix}?region={region}");
        if let Some(ep) = &endpoint {
            u.push_str(&format!("&endpoint={ep}"));
        }
        u
    };
    let e = env().await;
    let (db, done) = import(&e, "s3-me", "scylla", &url("backup")).await;
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["tables_done"].as_i64(), Some(TABLES.len() as i64), "{done}");
    let dst = e.connect(&db).await;
    compare_expected("s3 me", &dst, Dialect::Scylla, &load_expected("me")).await;
    // Nothing under the prefix, and a bucket that doesn't exist.
    let (_, done) = import(&e, "s3-empty", "scylla", &url("nothing-here")).await;
    assert_eq!(done["state"], "failed", "{done}");
    assert!(done["error"].as_str().unwrap().contains("empty"), "{done}");
    let missing = url("backup").replacen(&format!("s3://{bucket}/"), "s3://roll-e2e-no-such-bucket/", 1);
    let (_, done) = import(&e, "s3-nobucket", "scylla", &missing).await;
    assert_eq!(done["state"], "failed", "{done}");
    objects.delete_prefix("").await.unwrap();
}
