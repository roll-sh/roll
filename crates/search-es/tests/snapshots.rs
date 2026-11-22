//! Snapshot and repository APIs on a node with `path.repo` (which the reference Elasticsearch
//! used for fixtures doesn't have, so these follow the shapes Elasticsearch's YAML suites and
//! `SnapshotInfo` define): registration rules, the repository UUID living at the location,
//! create/get/status/clone/delete, restore over a closed index, global state and the
//! `repositories.default_repository` setting.

use search_es::json::{Value, parse};
use search_es::{Cluster, ClusterConfig, EsRequest, MemoryStore};
use std::sync::Arc;

async fn cluster() -> Cluster {
    let mut cfg = ClusterConfig::new("snap-test");
    cfg.node_settings.push(("path.repo".into(), "/repo".into()));
    Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap()
}

async fn call(c: &Cluster, method: &str, uri: &str, body: Option<&str>) -> (u16, Value) {
    let mut r = EsRequest::new(method, uri);
    if let Some(b) = body {
        r.headers.push(("content-type".into(), "application/json".into()));
        r.body = b.to_string().into();
    }
    let resp = search_es::handle(c, r).await;
    let v = parse(&resp.body).ok().flatten().unwrap_or(Value::Str(String::from_utf8_lossy(&resp.body).into_owned()));
    (resp.status, v)
}

fn at<'a>(v: &'a Value, path: &str) -> &'a Value {
    let mut cur = v;
    for p in path.split('.') {
        cur = match cur {
            Value::Arr(a) => p.parse::<usize>().ok().and_then(|i| a.get(i)).unwrap_or(&Value::Null),
            other => other.get(p).unwrap_or(&Value::Null),
        };
    }
    cur
}

#[tokio::test]
async fn repositories() {
    let c = cluster().await;
    let (s, v) = call(&c, "PUT", "/_snapshot/r1", Some(r#"{"type":"fs","settings":{"location":"loc1"}}"#)).await;
    assert_eq!((s, at(&v, "acknowledged")), (200, &Value::Bool(true)));
    let (s, v) = call(&c, "PUT", "/_snapshot/r2", Some(r#"{"type":"fs","settings":{"location":"/elsewhere"}}"#)).await;
    assert_eq!(s, 500);
    assert_eq!(at(&v, "error.caused_by.reason").as_str(), Some("[r2] location [/elsewhere] doesn't match any of the locations specified by path.repo"));
    let (s, v) = call(&c, "PUT", "/_snapshot/r2", Some(r#"{"type":"hdfs","settings":{"uri":"hdfs://x"}}"#)).await;
    assert_eq!(s, 500);
    assert_eq!(at(&v, "error.reason").as_str(), Some("[r2] repository type [hdfs] does not exist for project [default]"));
    let (s, v) = call(&c, "GET", "/_snapshot", None).await;
    assert_eq!(s, 200);
    assert_eq!(at(&v, "r1.type").as_str(), Some("fs"));
    assert_eq!(at(&v, "r1.settings.location").as_str(), Some("loc1"));
    assert!(at(&v, "r1.uuid").is_null(), "no UUID before anything is written");
    let (s, v) = call(&c, "POST", "/_snapshot/r1/_verify", None).await;
    assert_eq!(s, 200);
    assert!(matches!(at(&v, "nodes"), Value::Obj(m) if m.len() == 1));
    let (_, v) = call(&c, "POST", "/_snapshot/r1/_cleanup", None).await;
    assert_eq!(at(&v, "results.deleted_blobs").as_i64(), Some(0));
    // Default repository setting.
    let (s, v) = call(&c, "PUT", "/_cluster/settings", Some(r#"{"persistent":{"repositories.default_repository":"nope"}}"#)).await;
    assert_eq!(s, 400);
    assert!(at(&v, "error.caused_by.reason").as_str().unwrap().starts_with("Repository [nope] is not registered."));
    let (s, _) = call(&c, "PUT", "/_cluster/settings", Some(r#"{"persistent":{"repositories.default_repository":"r1"}}"#)).await;
    assert_eq!(s, 200);
    let (s, v) = call(&c, "DELETE", "/_snapshot/r1", None).await;
    assert_eq!(s, 400);
    assert!(at(&v, "error.reason").as_str().unwrap().starts_with("cannot delete the default repository"));
    let (s, v) = call(&c, "PUT", "/_snapshot/r1", Some(r#"{"type":"fs","settings":{"location":"loc1","readonly":true}}"#)).await;
    assert_eq!(s, 400, "{v:?}");
    call(&c, "PUT", "/_cluster/settings", Some(r#"{"persistent":{"repositories.default_repository":null}}"#)).await;
    let (s, _) = call(&c, "DELETE", "/_snapshot/r1", None).await;
    assert_eq!(s, 200);
    let (s, v) = call(&c, "GET", "/_snapshot/r1", None).await;
    assert_eq!((s, at(&v, "error.type").as_str()), (404, Some("repository_missing_exception")));
    let (_, v) = call(&c, "GET", "/_cat/repositories", None).await;
    assert_eq!(v, Value::Str(String::new()));
}

#[tokio::test]
async fn snapshot_lifecycle() {
    let c = cluster().await;
    call(&c, "PUT", "/_snapshot/repo", Some(r#"{"type":"fs","settings":{"location":"loc"}}"#)).await;
    call(&c, "PUT", "/idx", Some(r#"{"settings":{"number_of_shards":2,"number_of_replicas":0},"mappings":{"properties":{"f":{"type":"keyword"}}},"aliases":{"a1":{}}}"#)).await;
    call(&c, "PUT", "/other", Some(r#"{"settings":{"number_of_replicas":0}}"#)).await;
    call(&c, "PUT", "/_index_template/t1", Some(r#"{"index_patterns":["zz*"],"priority":7}"#)).await;
    let (s, v) = call(&c, "PUT", "/_snapshot/repo/s1?wait_for_completion=true", Some(r#"{"metadata":{"by":"test"}}"#)).await;
    assert_eq!(s, 200, "{v:?}");
    assert_eq!(at(&v, "snapshot.snapshot").as_str(), Some("s1"));
    assert_eq!(at(&v, "snapshot.state").as_str(), Some("SUCCESS"));
    assert_eq!(at(&v, "snapshot.shards.successful").as_i64(), Some(3));
    assert_eq!(at(&v, "snapshot.shards.failed").as_i64(), Some(0));
    assert_eq!(at(&v, "snapshot.indices"), &Value::from(vec![Value::from("idx"), Value::from("other")]));
    assert_eq!(at(&v, "snapshot.metadata.by").as_str(), Some("test"));
    assert!(at(&v, "snapshot.version_id").as_i64().unwrap() > 0);
    // The repository now has a UUID, kept by its location.
    let (_, v) = call(&c, "GET", "/_snapshot/repo", None).await;
    let uuid = at(&v, "repo.uuid").as_str().unwrap().to_string();
    call(&c, "DELETE", "/_snapshot/repo", None).await;
    call(&c, "PUT", "/_snapshot/again", Some(r#"{"type":"fs","settings":{"location":"loc"}}"#)).await;
    let (_, v) = call(&c, "GET", "/_snapshot/again", None).await;
    assert_eq!(at(&v, "again.uuid").as_str(), Some(uuid.as_str()));
    // Name rules and duplicates.
    let (s, v) = call(&c, "PUT", "/_snapshot/again/s1?wait_for_completion=true", None).await;
    assert_eq!((s, at(&v, "error.type").as_str()), (400, Some("snapshot_name_already_in_use_exception")));
    let (s, _) = call(&c, "PUT", "/_snapshot/again/Bad", None).await;
    assert_eq!(s, 400);
    let (s, v) = call(&c, "PUT", "/_snapshot/again/s2?wait_for_completion=true", Some(r#"{"indices":"missing"}"#)).await;
    assert_eq!((s, at(&v, "error.type").as_str()), (404, Some("index_not_found_exception")));
    let (s, v) = call(&c, "PUT", "/_snapshot/again/s2?wait_for_completion=true", Some(r#"{"indices":"missing","ignore_unavailable":true}"#)).await;
    assert_eq!(s, 200);
    assert_eq!(at(&v, "snapshot.shards.successful").as_i64(), Some(0));
    // Get: verbose, non-verbose, filters, missing.
    let (_, v) = call(&c, "GET", "/_snapshot/again/s1?index_details=true&human=true", None).await;
    assert_eq!(at(&v, "snapshots.0.index_details.idx.shard_count").as_i64(), Some(2));
    assert!(at(&v, "snapshots.0.index_details.idx.size_in_bytes").as_i64().unwrap() > 0);
    assert!(at(&v, "snapshots.0.failures").as_array().is_some());
    let (_, v) = call(&c, "GET", "/_snapshot/again/s1?verbose=false", None).await;
    assert!(at(&v, "snapshots.0.shards").is_null() && at(&v, "snapshots.0.version").is_null());
    assert_eq!(at(&v, "snapshots.0.state").as_str(), Some("SUCCESS"));
    let (_, v) = call(&c, "GET", "/_snapshot/again/_all?sort=name&order=desc", None).await;
    assert_eq!(at(&v, "snapshots.0.snapshot").as_str(), Some("s2"));
    assert_eq!(at(&v, "total").as_i64(), Some(2));
    let (_, v) = call(&c, "GET", "/_snapshot/again/*?state=FAILED", None).await;
    assert_eq!(at(&v, "snapshots").as_array().map(Vec::len), Some(0));
    let (s, v) = call(&c, "GET", "/_snapshot/again/s1?state=FOO", None).await;
    assert_eq!((s, at(&v, "error.reason").as_str()), (400, Some("No enum constant org.elasticsearch.snapshots.SnapshotState.FOO")));
    let (s, v) = call(&c, "GET", "/_snapshot/again/nope", None).await;
    assert_eq!((s, at(&v, "error.reason").as_str()), (404, Some("[again:nope] is missing")));
    let (s, _) = call(&c, "GET", "/_snapshot/again/nope?ignore_unavailable=true", None).await;
    assert_eq!(s, 200);
    // Status.
    let (_, v) = call(&c, "GET", "/_snapshot/again/s1/_status", None).await;
    assert_eq!(at(&v, "snapshots.0.state").as_str(), Some("SUCCESS"));
    assert!(at(&v, "snapshots.0.stats.total.file_count").as_i64().unwrap() > 0);
    assert_eq!(at(&v, "snapshots.0.indices.idx.shards_stats.done").as_i64(), Some(2));
    // Clone a subset.
    let (s, _) = call(&c, "PUT", "/_snapshot/again/s1/_clone/c1", Some(r#"{"indices":"other"}"#)).await;
    assert_eq!(s, 200);
    let (_, v) = call(&c, "GET", "/_snapshot/again/c1", None).await;
    assert_eq!(at(&v, "snapshots.0.indices"), &Value::from(vec![Value::from("other")]));
    // Restore: an open index of the same name blocks it; closing it lets it through.
    let (s, v) = call(&c, "POST", "/_snapshot/again/s1/_restore?wait_for_completion=true", Some(r#"{"indices":"idx"}"#)).await;
    assert_eq!((s, at(&v, "error.type").as_str()), (500, Some("snapshot_restore_exception")));
    call(&c, "POST", "/idx/_close", None).await;
    call(&c, "DELETE", "/_index_template/t1", None).await;
    let (s, v) = call(
        &c,
        "POST",
        "/_snapshot/again/s1/_restore?wait_for_completion=true",
        Some(r#"{"indices":"idx","include_global_state":true,"index_settings":{"index.number_of_replicas":0,"routing.rebalance.enable":"none"}}"#),
    )
    .await;
    assert_eq!(s, 200, "{v:?}");
    assert_eq!(at(&v, "snapshot.shards.successful").as_i64(), Some(2));
    let (_, v) = call(&c, "GET", "/idx", None).await;
    assert_eq!(at(&v, "idx.settings.index.routing.rebalance.enable").as_str(), Some("none"));
    assert_eq!(at(&v, "idx.mappings.properties.f.type").as_str(), Some("keyword"));
    assert!(at(&v, "idx.aliases.a1").as_object().is_some());
    let (_, v) = call(&c, "GET", "/idx/_recovery", None).await;
    assert_eq!(at(&v, "idx.shards.0.type").as_str(), Some("SNAPSHOT"));
    assert_eq!(at(&v, "idx.shards.0.index.files.recovered").as_i64(), Some(1));
    assert_eq!(at(&v, "idx.shards.0.source.snapshot").as_str(), Some("s1"));
    let (s, _) = call(&c, "GET", "/_index_template/t1", None).await;
    assert_eq!(s, 200, "global state brought the template back");
    // Rename on restore.
    let (s, v) =
        call(&c, "POST", "/_snapshot/again/s1/_restore?wait_for_completion=true", Some(r#"{"indices":"other","rename_pattern":"(.+)","rename_replacement":"restored-$1"}"#)).await;
    assert_eq!(s, 200, "{v:?}");
    assert_eq!(at(&v, "snapshot.indices"), &Value::from(vec![Value::from("restored-other")]));
    // cat.snapshots and delete.
    let (_, v) = call(&c, "GET", "/_cat/snapshots/again?h=id,status,indices,total_shards", None).await;
    let text = v.as_str().unwrap().to_string();
    assert!(text.contains("s1 SUCCESS 2 3"), "{text}");
    let (s, _) = call(&c, "DELETE", "/_snapshot/again/s1,c1", None).await;
    assert_eq!(s, 200);
    let (s, _) = call(&c, "DELETE", "/_snapshot/again/s1", None).await;
    assert_eq!(s, 404);
    let (_, v) = call(&c, "GET", "/_snapshot/again/_all", None).await;
    assert_eq!(at(&v, "total").as_i64(), Some(1));
}

/// A snapshot keeps its shards' documents: restoring over a closed index (or under a new name,
/// or from a clone) brings them back with their ids, routing and versions, and documents
/// written after the snapshot are not in it.
#[tokio::test]
async fn restore_brings_documents_back() {
    let c = cluster().await;
    call(&c, "PUT", "/_snapshot/r", Some(r#"{"type":"fs","settings":{"location":"loc"}}"#)).await;
    call(&c, "PUT", "/docs", Some(r#"{"settings":{"number_of_shards":2}}"#)).await;
    for i in 0..10 {
        call(&c, "PUT", &format!("/docs/_doc/{i}?routing=r{}", i % 3), Some(&format!(r#"{{"n":{i}}}"#))).await;
    }
    call(&c, "PUT", "/docs/_doc/3?routing=r0", Some(r#"{"n":33}"#)).await;
    let (s, v) = call(&c, "PUT", "/_snapshot/r/s1?wait_for_completion=true", Some(r#"{"indices":"docs"}"#)).await;
    assert_eq!((s, at(&v, "snapshot.state").as_str()), (200, Some("SUCCESS")));
    call(&c, "PUT", "/docs/_doc/later?refresh=true", Some(r#"{"n":-1}"#)).await;
    let (s, _) = call(&c, "PUT", "/_snapshot/r/s1/_clone/s2", Some(r#"{"indices":"docs"}"#)).await;
    assert_eq!(s, 200);
    call(&c, "POST", "/docs/_close", None).await;
    let (s, v) = call(&c, "POST", "/_snapshot/r/s1/_restore?wait_for_completion=true", None).await;
    assert_eq!((s, at(&v, "snapshot.shards.successful").as_i64()), (200, Some(2)), "{v:?}");
    let (_, v) = call(&c, "GET", "/docs/_count", None).await;
    assert_eq!(at(&v, "count").as_i64(), Some(10));
    let (_, v) = call(&c, "GET", "/docs/_doc/3?routing=r0", None).await;
    assert_eq!((at(&v, "_version").as_i64(), at(&v, "_routing").as_str(), at(&v, "_source.n").as_i64()), (Some(2), Some("r0"), Some(33)));
    let (_, v) = call(&c, "GET", "/docs/_doc/later", None).await;
    assert_eq!(at(&v, "found"), &Value::Bool(false));
    // From the clone, under another name.
    let (s, _) = call(&c, "POST", "/_snapshot/r/s2/_restore?wait_for_completion=true", Some(r#"{"rename_pattern":"docs","rename_replacement":"copy"}"#)).await;
    assert_eq!(s, 200);
    let (_, v) = call(&c, "GET", "/copy/_count", None).await;
    assert_eq!(at(&v, "count").as_i64(), Some(10));
    // A deleted snapshot can't be restored any more; the other one still can.
    call(&c, "DELETE", "/_snapshot/r/s2", None).await;
    call(&c, "DELETE", "/copy", None).await;
    let (s, _) = call(&c, "POST", "/_snapshot/r/s1/_restore?wait_for_completion=true", Some(r#"{"rename_pattern":"docs","rename_replacement":"copy"}"#)).await;
    assert_eq!(s, 200);
    let (_, v) = call(&c, "GET", "/copy/_count", None).await;
    assert_eq!(at(&v, "count").as_i64(), Some(10));
}

/// Elasticsearch 9.5.5 captures for `_status` and `index_details`
/// (`tests/fixtures/snapshot_status.json`).
fn fixture(key: &str) -> Value {
    let all = parse(include_bytes!("fixtures/snapshot_status.json")).unwrap().unwrap();
    all.get(key).unwrap_or_else(|| panic!("no fixture {key}")).clone()
}

/// A captured `(status, body)`.
fn captured(key: &str) -> (u16, Value) {
    match fixture(key) {
        Value::Arr(mut a) if a.len() == 2 => {
            let body = a.pop().unwrap();
            (a[0].as_i64().unwrap() as u16, body)
        }
        other => panic!("{key} is not a captured response: {other:?}"),
    }
}

/// `v` with every scalar under one of `keys` (at any depth) replaced by null, and object keys
/// in `rename` renamed: what differs between two clusters (uuids, times, names, node ids).
fn masked(v: &Value, keys: &[&str], rename: &[(&str, &str)]) -> Value {
    match v {
        Value::Obj(m) => Value::Obj(
            m.iter()
                .map(|(k, x)| {
                    let k = rename.iter().find(|(from, _)| from == k).map_or(k.clone(), |(_, to)| to.to_string());
                    let x = if keys.contains(&k.as_str()) && !matches!(x, Value::Obj(_) | Value::Arr(_)) { Value::Null } else { masked(x, keys, rename) };
                    (k, x)
                })
                .collect(),
        ),
        Value::Arr(a) => Value::Arr(a.iter().map(|x| masked(x, keys, rename)).collect()),
        other => other.clone(),
    }
}

/// The keys of `v` in order, scalars dropped: the shape Elasticsearch prints.
fn shape(v: &Value) -> Value {
    match v {
        Value::Obj(m) => Value::Obj(m.iter().map(|(k, x)| (k.clone(), shape(x))).collect()),
        Value::Arr(a) => Value::Arr(a.iter().map(shape).collect()),
        _ => Value::Null,
    }
}

const VARYING: &[&str] = &["uuid", "snapshot", "repository", "start_time", "start_time_in_millis", "time", "time_in_millis", "end_time", "end_time_in_millis", "duration", "duration_in_millis", "node"];

/// `_status` and `index_details` report each shard's files: an empty shard is one 227-byte
/// commit point; a second snapshot of an unchanged index adds nothing (incremental 0) and a
/// clone never does; new segments are incremental. Without snapshot names `_status` lists the
/// running snapshots, of any repository name.
#[tokio::test]
async fn status_reports_shard_files_like_elasticsearch() {
    let c = cluster().await;
    call(&c, "PUT", "/_snapshot/ops-r", Some(r#"{"type":"fs","settings":{"location":"loc"}}"#)).await;
    call(&c, "PUT", "/ops-empty", Some(r#"{"settings":{"number_of_shards":1}}"#)).await;
    call(&c, "PUT", "/_snapshot/ops-r/ops-s4?wait_for_completion=true", Some(r#"{"indices":"ops-empty"}"#)).await;
    let (_, v) = call(&c, "GET", "/_snapshot/ops-r/ops-s4/_status", None).await;
    assert_eq!(masked(&v, VARYING, &[]), masked(&captured("empty_shard_status").1, VARYING, &[]));
    let (_, v) = call(&c, "GET", "/_snapshot/ops-r/ops-s4?index_details=true", None).await;
    assert_eq!(at(&v, "snapshots.0.index_details"), at(&captured("empty_shard_index_details").1, "snapshots.0.index_details"));

    // Two shards with documents: real segment sizes, the human variants where ES puts them.
    call(&c, "PUT", "/ops-idx", Some(r#"{"settings":{"number_of_shards":2}}"#)).await;
    for i in 0..3 {
        call(&c, "POST", "/ops-idx/_doc?refresh=true", Some(&format!(r#"{{"a":{i}}}"#))).await;
    }
    call(&c, "PUT", "/_snapshot/ops-r/ops-s2?wait_for_completion=true", Some(r#"{"indices":"ops-idx"}"#)).await;
    let (_, v) = call(&c, "GET", "/_snapshot/ops-r/ops-s2/_status?human", None).await;
    assert_eq!(shape(&v), shape(&captured("status_human").1));
    let total = at(&v, "snapshots.0.stats.total.size_in_bytes").as_i64().unwrap();
    let shard_sum: i64 = (0..2).map(|n| at(&v, &format!("snapshots.0.indices.ops-idx.shards.{n}.stats.total.size_in_bytes")).as_i64().unwrap()).sum();
    assert!(total > 2 * 227 && total == shard_sum, "{v:?}");
    assert_eq!(at(&v, "snapshots.0.stats.incremental"), at(&v, "snapshots.0.stats.total"));
    let (_, d) = call(&c, "GET", "/_snapshot/ops-r/ops-s2?index_details=true&human", None).await;
    assert_eq!(shape(at(&d, "snapshots.0.index_details")), shape(at(&captured("get_index_details_human").1, "snapshots.0.index_details")));
    assert_eq!(at(&d, "snapshots.0.index_details.ops-idx.size_in_bytes").as_i64(), Some(total));
    assert!(at(&d, "snapshots.0.index_details.ops-idx.max_segments_per_shard").as_i64().unwrap() >= 1);

    // The same index again, unchanged: nothing incremental (ES captured the same).
    call(&c, "PUT", "/_snapshot/ops-r/again?wait_for_completion=true", Some(r#"{"indices":"ops-idx"}"#)).await;
    let (_, v) = call(&c, "GET", "/_snapshot/ops-r/again/_status", None).await;
    let es = fixture("snapshot_in_progress");
    let es_second = es.get("status_second_snapshot").unwrap();
    assert_eq!(at(&v, "snapshots.0.stats.incremental"), at(es_second, "snapshots.0.stats.incremental"));
    assert_eq!(at(&v, "snapshots.0.stats.total.size_in_bytes").as_i64(), Some(total));
    // A clone: every file reused, taken at once.
    call(&c, "PUT", "/_snapshot/ops-r/ops-s2/_clone/ops-s3", Some(r#"{"indices":"ops-idx"}"#)).await;
    let (_, v) = call(&c, "GET", "/_snapshot/ops-r/ops-s3/_status", None).await;
    let es_clone = captured("clone_status").1;
    assert_eq!(shape(&v), shape(&es_clone));
    assert_eq!(at(&v, "snapshots.0.indices.ops-idx.shards.1.stats.incremental"), at(&es_clone, "snapshots.0.indices.ops-idx.shards.1.stats.incremental"));
    assert_eq!(at(&v, "snapshots.0.indices.ops-idx.shards.1.stats.time_in_millis").as_i64(), Some(0));
    // A new document: its segment is incremental, the old ones aren't.
    call(&c, "POST", "/ops-idx/_doc?refresh=true", Some(r#"{"a":9}"#)).await;
    call(&c, "PUT", "/_snapshot/ops-r/third?wait_for_completion=true", Some(r#"{"indices":"ops-idx"}"#)).await;
    let (_, v) = call(&c, "GET", "/_snapshot/ops-r/third/_status", None).await;
    let inc = at(&v, "snapshots.0.stats.incremental.size_in_bytes").as_i64().unwrap();
    let all = at(&v, "snapshots.0.stats.total.size_in_bytes").as_i64().unwrap();
    assert!(inc > 227 && inc < all, "{v:?}");

    // Name conflicts and listings with nothing running.
    for _ in 0..2 {
        call(&c, "PUT", "/_snapshot/ops-r/ops-s1?wait_for_completion=true", Some(r#"{"indices":"ops-none*"}"#)).await;
    }
    let (s, v) = call(&c, "PUT", "/_snapshot/ops-r/ops-s1?wait_for_completion=true", Some(r#"{"indices":"ops-none*"}"#)).await;
    assert_eq!((s, v), captured("name_already_exists"));
    for (uri, key) in [
        ("/_snapshot/nope/_status", "status_missing_repository_without_names"),
        ("/_snapshot/_status", "status_all_idle"),
        ("/_snapshot/nope/x/_status", "status_missing_repository_with_names"),
        ("/_snapshot/ops-r/_current", "current_idle"),
        ("/_snapshot/ops-r/_current/_status", "current_status_idle"),
    ] {
        let (s, v) = call(&c, "GET", uri, None).await;
        assert_eq!((s, v), captured(key), "{uri}");
    }
}

/// Durable snapshot storage whose shard captures each wait for a permit, so a test decides
/// how long a snapshot runs. Every shard reports `3 + shard` files of `1000 + shard` bytes.
struct Gated {
    permits: tokio::sync::Semaphore,
    copies: parking_lot::Mutex<Vec<search_es::engine::SnapshotCopy>>,
    /// Captures fail.
    failing: std::sync::atomic::AtomicBool,
    /// Loads fail, as when a shard's contents were lost.
    lost: std::sync::atomic::AtomicBool,
}

struct GatedShards(Arc<Gated>);

impl search_es::engine::ShardStorage for GatedShards {
    fn open(&self, uuid: &str, shard: u32, schema: search_index::Schema) -> search_es::EsResult<search_index::Shard> {
        search_es::engine::MemoryShards.open(uuid, shard, schema)
    }

    fn snapshots(&self) -> Option<Arc<dyn search_es::engine::SnapshotStorage>> {
        Some(self.0.clone())
    }
}

#[async_trait::async_trait]
impl search_es::engine::SnapshotStorage for Gated {
    async fn capture(
        &self,
        snapshot: &str,
        index_uuid: &str,
        shard: u32,
        _engine: Option<&Arc<search_es::engine::IndexEngine>>,
        _previous: Option<&str>,
    ) -> search_es::EsResult<search_es::meta::snapshots::ShardFiles> {
        self.permits.acquire().await.unwrap().forget();
        if self.failing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(search_es::EsError::new(500, "snapshot_exception", format!("[{snapshot}] injected object store failure")));
        }
        self.copies.lock().push(search_es::engine::SnapshotCopy { snapshot: snapshot.into(), index_uuid: index_uuid.into(), shard });
        let (files, bytes) = (3 + u64::from(shard), 1000 + u64::from(shard));
        Ok(search_es::meta::snapshots::ShardFiles { file_count: files, size_in_bytes: bytes, incremental_file_count: files, incremental_size_in_bytes: bytes, segments: 1, ..Default::default() })
    }

    async fn load(&self, snapshot: &str, index_uuid: &str, shards: u32) -> search_es::EsResult<Vec<search_es::engine::SnapshotShard>> {
        if self.lost.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(search_es::EsError::new(500, "snapshot_restore_exception", format!("[{snapshot}] the contents of shard [{index_uuid}][0] are missing from the snapshot's storage")));
        }
        Ok((0..shards).map(|_| search_es::engine::SnapshotShard { segments: Vec::new(), numbers: Vec::new() }).collect())
    }

    async fn clone_snapshot(&self, _source: &str, _target: &str, _index_uuids: &[String]) -> search_es::EsResult<()> {
        Ok(())
    }

    async fn copies(&self) -> search_es::EsResult<Vec<search_es::engine::SnapshotCopy>> {
        Ok(self.copies.lock().clone())
    }

    async fn drop_copies(&self, copies: &[search_es::engine::SnapshotCopy], older_than: Option<std::time::Duration>) -> search_es::EsResult<()> {
        if older_than.is_none() {
            self.copies.lock().retain(|c| !copies.contains(c));
        }
        Ok(())
    }
}

async fn gated_cluster() -> (Cluster, Arc<Gated>) {
    let mut c = cluster().await;
    let gate = Arc::new(Gated {
        permits: tokio::sync::Semaphore::new(0),
        copies: Default::default(),
        failing: Default::default(),
        lost: Default::default(),
    });
    c.engine = search_es::engine::Engine::with_storage(Arc::new(GatedShards(gate.clone())));
    (c, gate)
}

/// Polls `uri` until `done` accepts its body.
async fn until(c: &Cluster, uri: &str, done: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..100_000 {
        let (_, v) = call(c, "GET", uri, None).await;
        if done(&v) {
            return v;
        }
        tokio::task::yield_now().await;
    }
    panic!("{uri} never got there");
}

/// A snapshot being taken shows as Elasticsearch's `SnapshotsInProgress` entry does:
/// `IN_PROGRESS` with no end time to `GET _snapshot` and `_current`, `STARTED` with each
/// shard's stage (and the node) to `_status`, and its name taken; once recorded, `SUCCESS`
/// with the files every shard reported.
#[tokio::test]
async fn running_snapshots_are_visible_like_elasticsearch() {
    let (c, gate) = gated_cluster().await;
    call(&c, "PUT", "/_snapshot/r", Some(r#"{"type":"fs","settings":{"location":"loc"}}"#)).await;
    call(&c, "PUT", "/snapidx", Some(r#"{"settings":{"number_of_shards":2}}"#)).await;
    let es = fixture("snapshot_in_progress");
    let create = call(&c, "PUT", "/_snapshot/r/s1?wait_for_completion=true", Some(r#"{"indices":"snapidx"}"#));
    let observe = async {
        let get = until(&c, "/_snapshot/r/s1", |v| at(v, "snapshots.0.state").as_str() == Some("IN_PROGRESS")).await;
        let keep = &["uuid", "start_time", "start_time_in_millis"];
        assert_eq!(masked(&get, keep, &[]), masked(es.get("get").unwrap(), keep, &[]));
        let (_, current) = call(&c, "GET", "/_snapshot/r/_current", None).await;
        assert_eq!(masked(&current, keep, &[]), masked(es.get("current").unwrap(), keep, &[]));
        let (_, all) = call(&c, "GET", "/_snapshot/r/_all", None).await;
        assert_eq!(at(&all, "total").as_i64(), Some(1));
        // The name is taken while it runs.
        let (s, v) = call(&c, "PUT", "/_snapshot/r/s1?wait_for_completion=true", Some(r#"{"indices":"snapidx"}"#)).await;
        assert_eq!((s, at(&v, "error.type").as_str()), (400, Some("snapshot_name_already_in_use_exception")));
        assert_eq!(at(&v, "error.reason").as_str(), Some("[r:s1] Invalid snapshot name [s1], snapshot with the same name is already in-progress"));

        // Shard 0 is being captured, shard 1 waits.
        let status = until(&c, "/_snapshot/r/s1/_status", |v| at(v, "snapshots.0.indices.snapidx.shards.0.stage").as_str() == Some("STARTED")).await;
        let s0 = at(&status, "snapshots.0");
        let es_s0 = at(es.get("status").unwrap(), "snapshots.0");
        let keys = |v: &Value| match v {
            Value::Obj(m) => m.keys().cloned().collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        assert_eq!(keys(s0), keys(es_s0));
        assert_eq!((at(s0, "state"), at(s0, "uuid")), (at(es_s0, "state"), at(&get, "snapshots.0.uuid")));
        assert_eq!(at(s0, "shards_stats"), &parse(br#"{"initializing":1,"started":1,"finalizing":0,"done":0,"failed":0,"total":2}"#).unwrap().unwrap());
        assert_eq!(keys(at(s0, "indices.snapidx.shards.0")), keys(at(es_s0, "indices.snapidx.shards.0")));
        assert_eq!(at(s0, "indices.snapidx.shards.1.stage").as_str(), Some("INIT"));
        let (_, everywhere) = call(&c, "GET", "/_snapshot/_status", None).await;
        assert_eq!(at(&everywhere, "snapshots.0.snapshot").as_str(), Some("s1"));
        let (_, in_repo) = call(&c, "GET", "/_snapshot/r/_status", None).await;
        assert_eq!(at(&in_repo, "snapshots.0.state").as_str(), Some("STARTED"));

        // Shard 0 done: its files, on this node; the snapshot's stats sum what is done.
        gate.permits.add_permits(1);
        let status = until(&c, "/_snapshot/r/s1/_status", |v| at(v, "snapshots.0.shards_stats.done").as_i64() == Some(1)).await;
        let shard0 = at(&status, "snapshots.0.indices.snapidx.shards.0");
        assert_eq!(at(shard0, "stage").as_str(), Some("DONE"));
        assert_eq!(at(shard0, "stats.total"), &parse(br#"{"file_count":3,"size_in_bytes":1000}"#).unwrap().unwrap());
        assert!(at(shard0, "node").as_str().is_some_and(|n| !n.is_empty()));
        assert_eq!(at(&status, "snapshots.0.stats.total.size_in_bytes").as_i64(), Some(1000));
        let get = until(&c, "/_snapshot/r/s1", |v| at(v, "snapshots.0.shards.successful").as_i64() == Some(1)).await;
        assert_eq!(at(&get, "snapshots.0.state").as_str(), Some("IN_PROGRESS"));
        let (_, cat) = call(&c, "GET", "/_cat/snapshots/r?h=id,status,successful_shards,failed_shards,total_shards", None).await;
        assert_eq!(cat.as_str().map(str::split_whitespace).map(Iterator::collect::<Vec<_>>), Some(vec!["s1", "IN_PROGRESS", "1", "0", "2"]));
        let (_, stats) = call(&c, "GET", "/_cluster/stats", None).await;
        assert_eq!(at(&stats, "snapshots.current_counts"), &parse(br#"{"snapshots":1,"shard_snapshots":1,"snapshot_deletions":0,"concurrent_operations":1,"cleanups":0}"#).unwrap().unwrap());
        let repo = at(&stats, "snapshots.repositories.r.current_counts");
        assert_eq!((at(repo, "snapshots").as_i64(), at(repo, "shards.complete").as_i64(), at(repo, "shards.states.INIT").as_i64()), (Some(1), Some(1), Some(1)));
        assert_eq!(at(&stats, "snapshots.repositories.r.oldest_start_time_millis"), at(&get, "snapshots.0.start_time_in_millis"));
        gate.permits.add_permits(1);
    };
    let ((s, v), ()) = tokio::join!(create, observe);
    assert_eq!((s, at(&v, "snapshot.state").as_str()), (200, Some("SUCCESS")), "{v:?}");

    // Recorded: the shards' files, no node, and the shape of a finished snapshot's status.
    let (_, done) = call(&c, "GET", "/_snapshot/r/s1/_status", None).await;
    assert_eq!(shape(&done), shape(es.get("status_done").unwrap()));
    assert_eq!(at(&done, "snapshots.0.state").as_str(), Some("SUCCESS"));
    assert_eq!(at(&done, "snapshots.0.stats.total"), &parse(br#"{"file_count":7,"size_in_bytes":2001}"#).unwrap().unwrap());
    assert_eq!(at(&done, "snapshots.0.indices.snapidx.shards.1.stats.incremental"), &parse(br#"{"file_count":4,"size_in_bytes":1001}"#).unwrap().unwrap());
    let (_, current) = call(&c, "GET", "/_snapshot/r/_current", None).await;
    assert_eq!(current, captured("current_idle").1);
    let (_, d) = call(&c, "GET", "/_snapshot/r/s1?index_details=true", None).await;
    assert_eq!(at(&d, "snapshots.0.index_details.snapidx"), &parse(br#"{"shard_count":2,"size_in_bytes":2001,"max_segments_per_shard":1}"#).unwrap().unwrap());

    // A snapshot whose capture fails is no longer running, and its name is free again.
    gate.failing.store(true, std::sync::atomic::Ordering::SeqCst);
    gate.permits.add_permits(1);
    let (s, v) = call(&c, "PUT", "/_snapshot/r/s2?wait_for_completion=true", Some(r#"{"indices":"snapidx"}"#)).await;
    assert_eq!((s, at(&v, "error.type").as_str()), (500, Some("snapshot_exception")), "{v:?}");
    let (_, current) = call(&c, "GET", "/_snapshot/r/_current", None).await;
    assert_eq!(current, captured("current_idle").1);
    gate.failing.store(false, std::sync::atomic::Ordering::SeqCst);
    gate.permits.add_permits(2);
    let (s, _) = call(&c, "PUT", "/_snapshot/r/s2?wait_for_completion=true", Some(r#"{"indices":"snapidx"}"#)).await;
    assert_eq!(s, 200);

    // A restore whose shard contents are lost fails before it creates anything.
    gate.lost.store(true, std::sync::atomic::Ordering::SeqCst);
    let (s, v) = call(&c, "POST", "/_snapshot/r/s1/_restore?wait_for_completion=true", Some(r#"{"rename_pattern":"snapidx","rename_replacement":"restored"}"#)).await;
    assert_eq!((s, at(&v, "error.type").as_str()), (500, Some("snapshot_restore_exception")), "{v:?}");
    let (s, _) = call(&c, "GET", "/restored", None).await;
    assert_eq!(s, 404, "no index is left behind");
    gate.lost.store(false, std::sync::atomic::Ordering::SeqCst);
    let (s, _) = call(&c, "POST", "/_snapshot/r/s1/_restore?wait_for_completion=true", Some(r#"{"rename_pattern":"snapidx","rename_replacement":"restored"}"#)).await;
    assert_eq!(s, 200);
}

/// Restoring snapshots Elasticsearch wrote (`api::snapshots::elastic`): an 8.19.7 and a 9.5.5
/// `fs` repository of generated data (`tests/fixtures/es-snapshots`, regenerated with
/// `scripts/search-es-snapshots.sh --write`), with routing, nested documents, deletes and
/// updates, several segments, small `chunk_size` parts, `best_compression`, and indices without
/// a stored `_source`. Documents, ids, routing, sequence numbers, versions and `_source` come
/// back as Elasticsearch had them, and searches answer as it did.
#[tokio::test]
async fn restores_snapshots_elasticsearch_wrote() {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/es-snapshots");
    let mut cfg = ClusterConfig::new("es-restore");
    cfg.node_settings.push(("path.repo".into(), fixtures.display().to_string()));
    let c = Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap();
    for version in ["es-8.19.7", "es-9.5.5"] {
        let repo = format!(r#"{{"type":"fs","settings":{{"location":"{version}"}}}}"#);
        check_restore(&c, &fixtures, version, &repo, false).await;
    }
}

/// The larger set `scripts/search-es-snapshots.sh` makes (thousands of documents, global state),
/// from an `fs` repository and from the same repository in a MinIO bucket.
#[tokio::test]
#[ignore = "needs scripts/search-es-snapshots.sh"]
async fn restores_larger_snapshots_elasticsearch_wrote() {
    let dir = std::path::PathBuf::from(std::env::var("ROLL_ES_SNAPSHOTS").expect("ROLL_ES_SNAPSHOTS"));
    let minio = std::env::var("ROLL_ES_SNAPSHOTS_S3").expect("ROLL_ES_SNAPSHOTS_S3");
    let mut cfg = ClusterConfig::new("es-restore-large");
    cfg.node_settings.push(("path.repo".into(), dir.display().to_string()));
    cfg.node_settings.push((search_es::api::snapshots::S3_ALLOW_PRIVATE.into(), "true".into()));
    let c = Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap();
    for version in ["es-8.19.7", "es-9.5.5"] {
        let started = std::time::Instant::now();
        check_restore(&c, &dir, version, &format!(r#"{{"type":"fs","settings":{{"location":"{version}"}}}}"#), true).await;
        println!("{version} from fs: {:?}", started.elapsed());
        let started = std::time::Instant::now();
        let s3 = format!(
            r#"{{"type":"s3","settings":{{"bucket":"es","base_path":"{version}","endpoint":"{minio}","path_style_access":true,"access_key":"roll","secret_key":"roll-secret"}}}}"#
        );
        check_restore(&c, &dir, version, &s3, true).await;
        println!("{version} from a bucket: {:?}", started.elapsed());
    }
}

async fn check_restore(c: &Cluster, dir: &std::path::Path, version: &str, repository: &str, global: bool) {
    let expected: Value = parse(&std::fs::read(dir.join(format!("{version}.expected.json"))).unwrap()).unwrap().unwrap();
    let repo = version.replace('.', "");
    let (s, v) = call(c, "PUT", &format!("/_snapshot/{repo}"), Some(repository)).await;
    assert_eq!(s, 200, "{v:?}");
    let (s, v) = call(c, "GET", &format!("/_snapshot/{repo}/_all"), None).await;
    assert_eq!(s, 200, "{v:?}");
    assert_eq!(at(&v, "snapshots.0.snapshot").as_str(), Some("snap-1"));
    assert_eq!(at(&v, "snapshots.0.state").as_str(), Some("SUCCESS"));
    assert_eq!(at(&v, "snapshots.0.indices").as_array().map(|a| a.len()), Some(5));
    // Nothing is ever written where Elasticsearch keeps its snapshots.
    let (s, v) = call(c, "PUT", &format!("/_snapshot/{repo}/mine?wait_for_completion=true"), Some("{}")).await;
    assert!(s >= 400 && at(&v, "error.reason").as_str().unwrap_or_default().contains("readonly"), "{s} {v:?}");
    let (s, _) = call(c, "DELETE", &format!("/_snapshot/{repo}/snap-1"), None).await;
    assert!(s >= 400);
    // Indices without their `_source` are refused, by name and reason.
    let restore = format!("/_snapshot/{repo}/snap-1/_restore?wait_for_completion=true");
    let (s, v) = call(c, "POST", &restore, Some(r#"{"indices":"*"}"#)).await;
    let reason = at(&v, "error.reason").as_str().unwrap_or_default().to_string();
    assert_eq!(s, 500, "{v:?}");
    assert!(reason.contains("[synthetic]") && reason.contains("[nosource]") && reason.contains("partial"), "{reason}");
    let body = format!(r#"{{"indices":"*","partial":true,"rename_pattern":"(.+)","rename_replacement":"r-$1","include_global_state":{global}}}"#);
    let (s, v) = call(c, "POST", &restore, Some(&body)).await;
    assert_eq!(s, 200, "{v:?}");
    assert_eq!(at(&v, "snapshot.shards.total").as_i64(), Some(4), "{v:?}");
    assert_eq!(at(&v, "snapshot.shards.failed").as_i64(), Some(0));
    if global {
        for uri in ["/_index_template/fixture-template", "/_ingest/pipeline/fixture-pipeline", "/_ilm/policy/fixture-policy"] {
            assert_eq!(call(c, "GET", uri, None).await.0, 200, "{uri}");
        }
    }
    for (index, want) in expected.as_object().unwrap() {
        let target = format!("r-{index}");
        let (_, v) = call(c, "GET", &format!("/{target}/_count"), None).await;
        assert_eq!(at(&v, "count"), at(want, "count"), "{version} {index}");
        let (_, hits) = call(c, "POST", &format!("/{target}/_search?seq_no_primary_term=true&version=true"), Some(r#"{"size":10000,"_source":{"exclude_vectors":false}}"#)).await;
        let got: std::collections::HashMap<String, &Value> =
            at(&hits, "hits.hits").as_array().unwrap().iter().map(|h| (at(h, "_id").as_str().unwrap().to_string(), h)).collect();
        for d in at(want, "docs").as_array().unwrap() {
            let id = at(d, "_id").as_str().unwrap();
            let h = got.get(id).unwrap_or_else(|| panic!("{version} {index}: no [{id}]"));
            assert_eq!(at(h, "_routing"), at(d, "_routing"), "{id}");
            assert_eq!(at(h, "_seq_no"), at(d, "_seq_no"), "{id}");
            assert_eq!(at(h, "_version"), at(d, "_version"), "{id}");
            assert!(same(at(h, "_source"), at(d, "_source")), "{version} {index} {id}: {:?} vs {:?}", at(h, "_source"), at(d, "_source"));
            // `GET _source` gives what Elasticsearch's gave (roll prints it again: the stored bytes
            // are compared in search-lucene's tests); vectors it left out show only on request.
            let mut uri = format!("/{target}/_source/{}", urlencode(id));
            if let Some(r) = at(d, "_routing").as_str() {
                uri.push_str(&format!("?routing={}", urlencode(r)));
            }
            let raw = raw_body(c, &uri).await;
            let kept: Value = parse(at(d, "source_raw").as_str().unwrap().as_bytes()).unwrap().unwrap();
            assert!(same(&parse(raw.as_bytes()).unwrap().unwrap(), &kept), "{version} {index} {id}: {raw}");
        }
        for (name, count) in at(want, "counts").as_object().unwrap() {
            let q = match name.as_str() {
                "match_beta" => r#"{"match":{"title":"beta"}}"#,
                "term_red" => r#"{"term":{"tag":"red"}}"#,
                "range_count" => r#"{"range":{"count":{"gte":10,"lt":30}}}"#,
                "nested_age" => r#"{"nested":{"path":"people","query":{"range":{"people.age":{"gte":50}}}}}"#,
                "geo" => r#"{"geo_distance":{"distance":"5000km","loc":{"lat":0,"lon":0}}}"#,
                _ => r#"{"range":{"when":{"gte":"2024-06-01"}}}"#,
            };
            let (_, v) = call(c, "POST", &format!("/{target}/_count"), Some(&format!(r#"{{"query":{q}}}"#))).await;
            assert_eq!(at(&v, "count"), count, "{version} {index} {name}");
        }
        let (_, v) = call(c, "POST", &format!("/{target}/_search"), Some(r#"{"size":3,"_source":false,"knn":{"field":"vec","query_vector":[0.1,0.2,-0.3,0.4],"k":3,"num_candidates":100}}"#)).await;
        let top: Vec<&str> = at(&v, "hits.hits").as_array().unwrap().iter().map(|h| at(h, "_id").as_str().unwrap()).collect();
        assert!(top.contains(&at(want, "knn.0").as_str().unwrap()), "{version} {index} knn {top:?}");
    }
    let (_, v) = call(c, "GET", "/r-mixed/_recovery", None).await;
    assert_eq!(at(&v, "r-mixed.shards.0.type").as_str(), Some("SNAPSHOT"));
    assert_eq!(at(&v, "r-mixed.shards.0.stage").as_str(), Some("DONE"));
    for i in ["r-mixed", "r-compressed", "r-excludes"] {
        call(c, "DELETE", &format!("/{i}"), None).await;
    }
    call(c, "DELETE", &format!("/_snapshot/{repo}"), None).await;
}

/// Equal sources, numbers within `float` precision: cosine vectors come back from their
/// normalized form and magnitude, on roll as on Elasticsearch, each with its own rounding.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Obj(x), Value::Obj(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w))),
        (Value::Arr(x), Value::Arr(y)) => x.len() == y.len() && x.iter().zip(y).all(|(v, w)| same(v, w)),
        (Value::Num(_), Value::Num(_)) => {
            let (x, y) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            (x - y).abs() <= 1e-6 * x.abs().max(1.0)
        }
        _ => a == b,
    }
}

fn urlencode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

async fn raw_body(c: &Cluster, uri: &str) -> String {
    let resp = search_es::handle(c, EsRequest::new("GET", uri)).await;
    assert_eq!(resp.status, 200, "{uri}: {}", String::from_utf8_lossy(&resp.body));
    String::from_utf8(resp.body.to_vec()).unwrap()
}

/// The rate of a restore from an Elasticsearch repository (docs/search/README.md, "Next"):
/// `ROLL_ES_BENCH=<an fs repository Elasticsearch wrote> cargo test --release -p search-es --test
/// snapshots elasticsearch_restore_rate -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs ROLL_ES_BENCH"]
async fn elasticsearch_restore_rate() {
    let dir = std::path::PathBuf::from(std::env::var("ROLL_ES_BENCH").expect("ROLL_ES_BENCH"));
    let mut cfg = ClusterConfig::new("es-restore-rate");
    cfg.node_settings.push(("path.repo".into(), dir.parent().unwrap().display().to_string()));
    let c = Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap();
    let body = format!(r#"{{"type":"fs","settings":{{"location":"{}"}}}}"#, dir.file_name().unwrap().to_str().unwrap());
    assert_eq!(call(&c, "PUT", "/_snapshot/bench", Some(&body)).await.0, 200);
    let (_, v) = call(&c, "GET", "/_snapshot/bench/_all", None).await;
    let snap = at(&v, "snapshots.0.snapshot").as_str().unwrap().to_string();
    let started = std::time::Instant::now();
    let (s, v) = call(&c, "POST", &format!("/_snapshot/bench/{snap}/_restore?wait_for_completion=true"), Some(r#"{"indices":"*"}"#)).await;
    let took = started.elapsed();
    assert_eq!(s, 200, "{v:?}");
    let index = at(&v, "snapshot.indices.0").as_str().unwrap().to_string();
    let (_, v) = call(&c, "GET", &format!("/{index}/_count"), None).await;
    let docs = at(&v, "count").as_i64().unwrap();
    println!("restored {docs} documents in {took:?}: {:.0} docs/s", docs as f64 / took.as_secs_f64());
}

/// What a restore from an Elasticsearch repository holds of each document, from the moment it
/// is read until it is indexed, is the `request` breaker's: three 4 MiB documents (compressed to
/// almost nothing) under a 6mb limit trip it (the chunk being read alone fits), and with room
/// they restore.
#[tokio::test]
async fn documents_a_restore_holds_are_charged_to_the_breaker() {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/es-snapshots");
    let mut cfg = ClusterConfig::new("es-restore-breaker");
    cfg.node_settings.push(("path.repo".into(), fixtures.display().to_string()));
    let c = Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap();
    assert_eq!(call(&c, "PUT", "/_snapshot/large", Some(r#"{"type":"fs","settings":{"location":"es-9.5.5-large"}}"#)).await.0, 200);
    let (s, _) = call(&c, "PUT", "/_cluster/settings", Some(r#"{"persistent":{"indices.breaker.request.limit":"6mb"}}"#)).await;
    assert_eq!(s, 200);
    let (s, v) = call(&c, "POST", "/_snapshot/large/snap-1/_restore?wait_for_completion=true", Some(r#"{"indices":"big"}"#)).await;
    assert_eq!((s, at(&v, "error.type").as_str()), (429, Some("circuit_breaking_exception")), "{v:?}");
    let (_, v) = call(&c, "GET", "/big/_recovery", None).await;
    assert_eq!(at(&v, "big.shards.0.type").as_str(), Some("SNAPSHOT"));
    call(&c, "DELETE", "/big", None).await;
    call(&c, "PUT", "/_cluster/settings", Some(r#"{"persistent":{"indices.breaker.request.limit":null}}"#)).await;
    let (s, v) = call(&c, "POST", "/_snapshot/large/snap-1/_restore?wait_for_completion=true", Some(r#"{"indices":"big"}"#)).await;
    assert_eq!(s, 200, "{v:?}");
    let (_, v) = call(&c, "GET", "/big/_count", None).await;
    assert_eq!(at(&v, "count").as_i64(), Some(3));
}

/// An `index.latest` put where roll keeps an `fs` repository's snapshots doesn't make it
/// Elasticsearch's: roll's snapshots stay listed and the repository writable.
#[tokio::test]
async fn a_planted_index_latest_leaves_rolls_snapshots_alone() {
    let dir = std::env::temp_dir().join(format!("roll-es-planted-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("mine")).unwrap();
    let mut cfg = ClusterConfig::new("es-planted");
    cfg.node_settings.push(("path.repo".into(), dir.display().to_string()));
    let c = Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap();
    assert_eq!(call(&c, "PUT", "/_snapshot/mine", Some(r#"{"type":"fs","settings":{"location":"mine"}}"#)).await.0, 200);
    call(&c, "PUT", "/i/_doc/1?refresh=true", Some(r#"{"a":1}"#)).await;
    let (s, v) = call(&c, "PUT", "/_snapshot/mine/s1?wait_for_completion=true", Some(r#"{"indices":"i"}"#)).await;
    assert_eq!(s, 200, "{v:?}");
    // Elasticsearch's repository data appears at the location.
    let es = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/es-snapshots/es-9.5.5");
    for f in ["index.latest", "index-0"] {
        std::fs::copy(es.join(f), dir.join("mine").join(f)).unwrap();
    }
    let (s, v) = call(&c, "GET", "/_snapshot/mine/_all", None).await;
    assert_eq!(s, 200, "{v:?}");
    let names: Vec<&str> = at(&v, "snapshots").as_array().unwrap().iter().map(|s| at(s, "snapshot").as_str().unwrap()).collect();
    assert_eq!(names, ["s1"]);
    let (s, v) = call(&c, "PUT", "/_snapshot/mine/s2?wait_for_completion=true", Some(r#"{"indices":"i"}"#)).await;
    assert_eq!(s, 200, "{v:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
