//! Point-in-time restore of a log store from a versioned S3 bucket: after restoring to a moment,
//! the log opens to exactly what was there then (tenants, data, pruned history), takes new
//! writes, and a later restore undoes the restore. Needs `ROLL_TASKS_E2E_VERSIONED`
//! (`s3://bucket/prefix` of a versioned bucket; `scripts/test-tasks.sh` sets one on MinIO).
#![cfg(feature = "restore")]

use object_store::ObjectStoreExt;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tasks::store::log::{Log, LogConfig, LogStore};
use tasks::store::{Expect, Store};

async fn put(s: &LogStore, id: &str, n: i64) {
    assert!(s.doc_put("q", id, &json!({ "n": n }), Expect::Any).await.unwrap());
}

async fn get(s: &LogStore, id: &str) -> Option<i64> {
    s.doc_get("q", id).await.unwrap().map(|v| v.value["n"].as_i64().unwrap())
}

async fn open(url: &str, prefix: &str, dir: &std::path::Path) -> Arc<Log> {
    let (objects, _) = tasks::store::log::objects_from_url(url).unwrap();
    let mut c = LogConfig::new(objects, prefix, dir.to_path_buf());
    c.commit_ms = 5;
    c.relaxed_ms = 50;
    // Snapshot and prune often, so the restore has pruned history to bring back.
    c.snapshot_commits = 6;
    Log::open(c).await.unwrap()
}

async fn eventually(f: impl Fn() -> bool) {
    for _ in 0..400 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("condition never held");
}

/// A moment strictly between what came before and what comes after, at S3's one-second
/// timestamp precision.
async fn moment() -> String {
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let t = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    t
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_log_restores_to_any_moment() {
    let Ok(base) = std::env::var("ROLL_TASKS_E2E_VERSIONED") else {
        eprintln!("skipped: set ROLL_TASKS_E2E_VERSIONED to a versioned s3:// bucket");
        return;
    };
    let (bucket, root) = base.strip_prefix("s3://").unwrap().split_once('/').unwrap_or((base.strip_prefix("s3://").unwrap(), "e2e"));
    let prefix = format!("{root}/restore-{}", tasks::ids::ulid());
    let url = format!("s3://{bucket}/{prefix}");

    // Before: two tenants, enough commits to snapshot and prune.
    let d1 = tempfile::tempdir().unwrap();
    let log = open(&url, &prefix, d1.path()).await;
    let a = log.create_tenant("acme").await.unwrap();
    let gone = log.create_tenant("gone").await.unwrap();
    for i in 0..30 {
        put(&a, &format!("k{i}"), i).await;
        put(&gone, &format!("g{i}"), i).await;
    }
    eventually(|| log.stats().pruned > 0).await;
    let at = moment().await;

    // After: overwrites, deletions, a deleted tenant, a new one, and more pruning.
    put(&a, "k0", 100).await;
    for i in 30..60 {
        put(&a, &format!("k{i}"), i).await;
    }
    assert!(a.doc_delete("q", "k1").await.unwrap());
    log.drop_tenant("gone").await.unwrap();
    let late = log.create_tenant("late").await.unwrap();
    put(&late, "x", 1).await;
    let pruned_before = log.stats().pruned;
    for i in 60..90 {
        put(&a, &format!("k{i}"), i).await;
    }
    eventually(|| log.stats().pruned > pruned_before).await;
    drop((a, gone, late));
    log.close().await;
    let after = moment().await;

    // Refused while a leader holds the lease; fine once it's released (the lease isn't restored).
    let t = tasks::restore::parse_time(&at).unwrap();
    let (objects, _) = tasks::store::log::objects_from_url(&url).unwrap();
    let lease = object_store::path::Path::from(format!("{prefix}/lease"));
    objects.put(&lease, json!({ "node": "node-a", "released": false }).to_string().into()).await.unwrap();
    let err = tasks::restore::run(&url, t, true, false).await.unwrap_err();
    assert!(err.to_string().contains("holds the lease"), "{err}");
    objects.put(&lease, json!({ "node": "node-a", "released": true }).to_string().into()).await.unwrap();

    // A dry run changes nothing.
    let dry = tasks::restore::run(&url, t, false, false).await.unwrap();
    assert!(!dry.applied && dry.restored > 0 && dry.deleted > 0, "{dry:?}");
    let s = tasks::restore::run(&url, t, true, false).await.unwrap();
    assert!(s.applied, "{s:?}");
    assert!(tasks::restore::run(&url, t, false, false).await.unwrap().restored == 0, "a second restore has nothing left to do");

    // Exactly the state at that moment, and it takes new writes.
    let d2 = tempfile::tempdir().unwrap();
    let log = open(&url, &prefix, d2.path()).await;
    assert_eq!(log.tenant_ids(), vec!["acme".to_string(), "gone".to_string()]);
    let a = log.tenant("acme").unwrap();
    for i in 0..30 {
        assert_eq!(get(&a, &format!("k{i}")).await, Some(i), "k{i}");
    }
    assert_eq!(get(&a, "k30").await, None);
    let gone = log.tenant("gone").unwrap();
    assert_eq!(get(&gone, "g29").await, Some(29));
    put(&a, "after-restore", 1).await;
    drop((a, gone));
    log.close().await;
    let d3 = tempfile::tempdir().unwrap();
    let log = open(&url, &prefix, d3.path()).await;
    assert_eq!(get(&log.tenant("acme").unwrap(), "after-restore").await, Some(1));
    log.close().await;

    // The restore can be undone: back to just before it.
    tasks::restore::run(&url, tasks::restore::parse_time(&after).unwrap(), true, false).await.unwrap();
    let d4 = tempfile::tempdir().unwrap();
    let log = open(&url, &prefix, d4.path()).await;
    let a = log.tenant("acme").unwrap();
    assert_eq!(get(&a, "k0").await, Some(100));
    assert_eq!(get(&a, "k1").await, None);
    assert_eq!(get(&a, "k89").await, Some(89));
    assert!(log.tenant("gone").is_none());
    let lease_now = objects.get(&lease).await.unwrap().bytes().await.unwrap();
    assert!(String::from_utf8_lossy(&lease_now).contains("\"released\":true"), "the lease was left alone");
    assert_eq!(get(&log.tenant("late").unwrap(), "x").await, Some(1));
    drop(a);
    log.close().await;
}
