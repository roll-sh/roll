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
    let (s, v) = call(&c, "PUT", "/_snapshot/r2", Some(r#"{"type":"s3","settings":{"bucket":"b"}}"#)).await;
    assert_eq!(s, 500);
    assert_eq!(at(&v, "error.reason").as_str(), Some("[r2] repository type [s3] does not exist for project [default]"));
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
    assert_eq!((s, at(&v, "error.type").as_str()), (400, Some("invalid_snapshot_name_exception")));
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
