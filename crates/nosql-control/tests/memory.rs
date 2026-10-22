//! Memory budgets through the control plane: a database's `memtable_space` setting (its plan)
//! reaches storage at load and live on PATCH, never above the node's budget; `GET /v1/nosql`,
//! `activity` and `usage` report it; `system_views.settings` shows it; an unload frees it.

mod common;

use common::*;
use reqwest::Method;
use serde_json::json;

const MIB: u64 = 1 << 20;

#[tokio::test]
async fn memtable_caps_from_settings_to_storage() {
    let e = env_with(|c| {
        c.storage.memtable_budget_bytes = 256 * MIB;
        c.storage.memtable_db_bytes = 64 * MIB;
    })
    .await;
    let info = e.get("/v1/nosql").await;
    let m = &info["memory"];
    assert_eq!((m["memtable_budget_bytes"].as_u64(), m["memtable_db_bytes"].as_u64()), (Some(256 * MIB), Some(64 * MIB)), "{m}");
    assert!(m["cache_memory_budget_bytes"].as_u64().unwrap() > 0);
    assert!(m["process"]["rss_bytes"].as_u64().unwrap() > 0, "{m}");

    let db = e.post("/v1/nosql/databases", json!({"name": "plan", "dialect": "cassandra", "settings": {"memtable_space": "32MiB"}})).await;
    let id = id(&db);
    let path = format!("/v1/nosql/databases/{id}");
    let c = e.connect(&db).await;
    keyspace(&c, "ks").await;
    exec(&c, "INSERT INTO ks.t (k, v) VALUES (1, 'one')").await;
    let cap = || e.control.node.get_loaded(&id).expect("loaded").db.store().memory().cap();
    assert_eq!(cap(), 32 * MIB);
    assert_eq!(e.control.node.get_loaded(&id).unwrap().db.store().config().memtable_space_bytes, Some(32 * MIB));
    assert_eq!(texts(&c, "SELECT value FROM system_views.settings WHERE name = 'memtable_heap_space'").await, vec!["32MiB"]);

    // A plan change applies to the loaded database at once.
    e.patch(&path, json!({"settings": {"memtable_space": "48MiB"}})).await;
    assert_eq!(cap(), 48 * MIB);
    assert_eq!(texts(&c, "SELECT value FROM system_views.settings WHERE name = 'memtable_heap_space'").await, vec!["48MiB"]);
    let a = e.get(&format!("{path}/activity")).await;
    assert_eq!(a["memory"]["memtable_cap_bytes"].as_u64(), Some(48 * MIB), "{a}");
    assert!(a["memory"]["memtable_bytes"].as_u64().unwrap() > 0, "{a}");
    let u = e.get(&format!("{path}/usage")).await;
    assert_eq!(u["memtable_cap_bytes"].as_u64(), Some(48 * MIB), "{u}");
    // Cassandra's spellings; never above the node's budget; removed: the node's default.
    e.patch(&path, json!({"settings": {"memtable_space": null, "memtable_heap_space_in_mb": 16}})).await;
    assert_eq!(cap(), 16 * MIB);
    e.patch(&path, json!({"settings": {"memtable_heap_space_in_mb": null, "memtable_heap_space": "1GiB"}})).await;
    assert_eq!(cap(), 256 * MIB);
    assert_eq!(e.get(&format!("{path}/usage")).await["memtable_cap_bytes"].as_u64(), Some(256 * MIB));
    e.patch(&path, json!({"settings": {"memtable_heap_space": null}})).await;
    assert_eq!(cap(), 64 * MIB);
    let (s, v) = e.call(Method::PATCH, &path, Some(json!({"settings": {"memtable_space": "100KiB"}}))).await;
    assert_eq!((s, v["error"].as_str()), (400, Some("memtable_space must be at least 1MiB")));
    let (s, _) = e.call(Method::POST, "/v1/nosql/databases", Some(json!({"name": "bad", "settings": {"memtable_space": "lots"}}))).await;
    assert_eq!(s, 400);

    // The node's view, and an unload giving everything back.
    assert!(e.get("/v1/nosql").await["memory"]["memtable_bytes"].as_u64().unwrap() > 0);
    drop(c);
    e.post(&format!("{path}/unload"), json!({})).await;
    let m = e.get("/v1/nosql").await["memory"].clone();
    assert_eq!((m["memtable_bytes"].as_u64(), m["databases_with_memtables"].as_u64(), m["sstable_index_bytes"].as_u64()), (Some(0), Some(0), Some(0)), "{m}");
    let a = e.get(&format!("{path}/activity")).await;
    assert_eq!(a["state"], "idle");
}
