//! Every `/v1/nosql` route through the router: happy paths and every error status.

mod common;

use common::*;
use nosql_proto::code;
use reqwest::Method;
use serde_json::{Value, json};

const ROUTES: &[(&str, &str)] = &[
    ("GET", ""),
    ("PATCH", ""),
    ("DELETE", ""),
    ("POST", "/branches"),
    ("POST", "/reset"),
    ("POST", "/restore"),
    ("GET", "/diff"),
    ("GET", "/imports"),
    ("POST", "/imports"),
    ("POST", "/password"),
    ("POST", "/cql"),
    ("GET", "/schema"),
    ("GET", "/usage"),
    ("GET", "/activity"),
    ("GET", "/alternator"),
    ("POST", "/flush"),
    ("POST", "/compact"),
    ("POST", "/unload"),
];

fn body_for(path: &str) -> Value {
    match path {
        "/branches" => json!({"name": "b"}),
        "/restore" => json!({"at": 1}),
        "/imports" => json!({"from": "cql://127.0.0.1:1"}),
        "/cql" => json!({"query": "SELECT * FROM system.local"}),
        _ => json!({}),
    }
}

#[tokio::test]
async fn info_and_unknown_databases() {
    let e = env().await;
    let info = e.get("/v1/nosql").await;
    assert_eq!(info["dialects"], json!(["scylla", "cassandra"]));
    assert_eq!(info["endpoints"]["cql"], format!("127.0.0.1:{}", e.cql.port()));
    assert_eq!(info["defaults"]["dialect"], "scylla");
    assert_eq!(info["settings"]["cassandra"]["batch_size_warn_threshold"], 5120);
    assert_eq!(info["pricing"]["keyspaces_usd_per_million_writes"], 0.625);
    assert_eq!(e.get("/v1/nosql/databases").await, json!([]));
    for (m, p) in ROUTES {
        let method = Method::from_bytes(m.as_bytes()).unwrap();
        let (s, v) = e.call(method, &format!("/v1/nosql/databases/nope{p}"), Some(body_for(p))).await;
        assert_eq!((s, v["error"].as_str()), (404, Some("no nosql database nope")), "{m} {p}");
    }
}

#[tokio::test]
async fn create_validation_and_views() {
    let e = env().await;
    let db = e.post("/v1/nosql/databases", json!({"name": "app-prod", "datacenter": "us-east-1", "settings": {"tombstone_warn_threshold": 50}})).await;
    let id = id(&db);
    assert_eq!(id.len(), 12);
    assert!(id.starts_with('n'));
    assert_eq!(db["name"], "app-prod");
    assert_eq!(db["dialect"], "scylla");
    assert_eq!(db["datacenter"], "us-east-1");
    assert_eq!(db["rack"], "rack1");
    assert_eq!(db["cluster_name"], "app-prod");
    assert_eq!(db["state"], "running");
    assert_eq!(db["endpoints"]["cql"], format!("127.0.0.1:{}", e.cql.port()));
    assert_eq!(db["endpoints"]["alternator"], format!("http://127.0.0.1:{}", e.alternator.port()));
    assert_eq!(db["alternator"]["access_key_id"], format!("cassandra@{id}"));
    assert_eq!(db["tls"], false);
    assert_eq!(db["credentials"]["username"], format!("cassandra@{id}"));
    assert_eq!(db["credentials"]["password"].as_str().unwrap().len(), 32);
    assert_eq!(db["idle_timeout"], 300);
    assert_eq!(db["history_retention"], 604_800);
    assert_eq!(db["durability"], "async");
    assert_eq!(db["read_only"], false);
    assert_eq!(db["settings"], json!({"tombstone_warn_threshold": 50}));
    assert_eq!(db["num_tokens"], 256);
    assert!(db["host_id"].as_str().unwrap().len() == 36);
    assert!(db.get("superuser").is_none() && db.get("password").is_none());

    // The presented node has the database's names.
    let c = e.connect(&db).await;
    let local = q(&c, "SELECT data_center, rack, cluster_name FROM system.local").await;
    assert_eq!((text(&local[0][0]), text(&local[0][1]), text(&local[0][2])), ("us-east-1".into(), "rack1".into(), "app-prod".into()));

    let cass = e.post("/v1/nosql/databases", json!({"name": "c5", "dialect": "cassandra", "cluster_name": "Prod Cluster"})).await;
    assert_eq!((cass["dialect"].as_str(), cass["cluster_name"].as_str(), cass["num_tokens"].as_i64()), (Some("cassandra"), Some("Prod Cluster"), Some(16)));

    // Lookups by id and name, and the list.
    assert_eq!(e.get("/v1/nosql/databases/app-prod").await["id"], id.as_str());
    assert_eq!(e.get(&format!("/v1/nosql/databases/{id}")).await["name"], "app-prod");
    assert_eq!(e.get("/v1/nosql/databases").await.as_array().unwrap().len(), 2);

    // Errors.
    let bad = [
        (json!({"name": "app-prod"}), 409, "a database named app-prod already exists"),
        (json!({"name": "Bad_Name"}), 400, "name: 1-40 lowercase letters, digits or '-'"),
        (json!({"name": "x", "dialect": "mongo"}), 400, "unknown dialect mongo (scylla or cassandra)"),
        (json!({"name": "x", "durability": "never"}), 400, "durability must be async or sync"),
        (json!({"name": "x", "settings": {"bogus": 1}}), 400, ""),
        (json!({"name": "x", "datacenter": "a'b"}), 400, "datacenter: 1-64 printable characters without quotes"),
        (json!({"name": "x", "idle_timeout": -1}), 400, "idle_timeout and history_retention can't be negative"),
        (json!({"name": "x", "history_retention": 100 * 86_400}), 400, "history_retention is at most 35 days"),
        (json!({"name": "x", "expires_at": 5}), 400, "expires_at is in the past"),
        (json!({"name": "x", "masking": {"ks.t": "mask_null()"}}), 400, "masking: \"ks.t\" is not keyspace.table.column"),
        (json!({"nope": 1}), 400, ""),
    ];
    for (body, status, msg) in bad {
        let (s, v) = e.call(Method::POST, "/v1/nosql/databases", Some(body.clone())).await;
        assert_eq!(s, status, "{body}: {v}");
        if !msg.is_empty() {
            assert_eq!(v["error"], msg, "{body}");
        } else {
            assert!(v["error"].is_string(), "{body}: {v}");
        }
    }
    let r = e.http.post(format!("{}/v1/nosql/databases", e.api)).header("content-type", "application/json").body("{not json").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400);
    assert!(r.json::<Value>().await.unwrap()["error"].as_str().unwrap().starts_with("invalid request body"));

    // The dashboard calls the API from another origin.
    let pre = e
        .http
        .request(Method::OPTIONS, format!("{}/v1/nosql/databases/{id}", e.api))
        .header("origin", "http://localhost:3000")
        .header("access-control-request-method", "PATCH")
        .send()
        .await
        .unwrap();
    assert!(pre.status().is_success());
    assert!(pre.headers().contains_key("access-control-allow-origin"));
}

#[tokio::test]
async fn patch_settings_live_and_errors() {
    let e = env().await;
    let db = e.post("/v1/nosql/databases", json!({"name": "a"})).await;
    e.post("/v1/nosql/databases", json!({"name": "b"})).await;
    let id = id(&db);
    let path = format!("/v1/nosql/databases/{id}");
    let c = e.connect(&db).await;
    keyspace(&c, "ks").await;
    exec(&c, "INSERT INTO ks.t (k, v) VALUES (1, 'one')").await;

    // Durability and the restore window apply to the loaded database at once.
    let v = e.patch(&path, json!({"durability": "sync", "history_retention": 3600, "idle_timeout": 0})).await;
    assert_eq!((v["durability"].as_str(), v["history_retention"].as_i64(), v["idle_timeout"].as_i64()), (Some("sync"), Some(3600), Some(0)));
    let l = e.control.node.get_loaded(&id).expect("still loaded");
    let sc = l.db.store().config();
    assert_eq!((sc.durability, sc.history_retention_ms), (nosql_storage::log::Durability::Sync, 3_600_000));
    exec(&c, "INSERT INTO ks.t (k, v) VALUES (2, 'two')").await;
    // So does the idle timeout (zero: never unload).
    assert_eq!(e.control.node.idle_timeout(&id), None);
    e.patch(&path, json!({"idle_timeout": 60})).await;
    assert_eq!(e.control.node.idle_timeout(&id), Some(std::time::Duration::from_secs(60)));

    // Engine settings apply live: the database stays loaded, clients keep their connections.
    let v = e.patch(&path, json!({"settings": {"tombstone_warn_threshold": 7, "batch_size_warn_threshold_in_kb": 1}})).await;
    assert_eq!(v["settings"], json!({"tombstone_warn_threshold": 7, "batch_size_warn_threshold_in_kb": 1}));
    let l = e.control.node.get_loaded(&id).expect("applied live");
    assert_eq!(l.db.settings().tombstone_warn_threshold, 7);
    assert_eq!(l.db.settings().batch_size_warn_threshold, 1024);
    assert_eq!(texts(&c, "SELECT v FROM ks.t").await, ["one", "two"]);
    // A setting the engine reads at open reloads the database: clients reconnect and see it.
    e.patch(&path, json!({"settings": {"shards": 2}})).await;
    assert!(e.control.node.get_loaded(&id).is_none(), "reloaded");
    let c = e.connect(&db).await;
    let l = e.control.node.get_loaded(&id).unwrap();
    assert_eq!((l.db.config().settings.shards, l.db.settings().tombstone_warn_threshold), (2, 7));
    assert_eq!(texts(&c, "SELECT v FROM ks.t").await, ["one", "two"]);
    // `null` removes a setting.
    let v = e.patch(&path, json!({"settings": {"tombstone_warn_threshold": null, "shards": null}})).await;
    assert_eq!(v["settings"], json!({"batch_size_warn_threshold_in_kb": 1}));
    let c = e.connect(&db).await;

    // Read-only applies live: writes and role changes are refused on the same connection,
    // reads and listings work.
    e.patch(&path, json!({"read_only": true, "masking": {"ks.t.v": "mask_inner(1, 1)"}, "expires_at": 4_000_000_000i64})).await;
    assert!(e.control.node.get_loaded(&id).is_some(), "not reloaded");
    for cql in ["INSERT INTO ks.t (k, v) VALUES (3, 'x')", "CREATE ROLE r WITH PASSWORD = 'r' AND LOGIN = true", "TRUNCATE ks.t"] {
        let (code_, msg) = server_code(c.query(cql, Default::default()).await.unwrap_err());
        assert_eq!((code_, msg.as_str()), (code::UNAUTHORIZED, "This database is read-only"), "{cql}");
    }
    assert_eq!(texts(&c, "SELECT v FROM ks.t").await.len(), 2);
    assert!(!q(&c, "LIST ROLES").await.is_empty());
    // New connections (and the next load) see it too.
    let c2 = e.connect(&db).await;
    assert!(c2.query("INSERT INTO ks.t (k, v) VALUES (3, 'x')", Default::default()).await.is_err());
    // The superuser's password rotates on a read-only database.
    let rotated = e.post(&format!("{path}/password"), json!({})).await;
    assert!(e.control.node.get_loaded(&id).is_some(), "rotated in place");
    e.try_connect(&rotated).await.unwrap();
    assert!(e.try_connect(&db).await.is_err(), "the old password stops working");
    let v = e.get(&path).await;
    assert_eq!((v["read_only"].as_bool(), v["expires_at"].as_i64()), (Some(true), Some(4_000_000_000)));
    assert_eq!(v["masking"], json!({"ks.t.v": "mask_inner(1, 1)"}));
    let v = e.patch(&path, json!({"read_only": false, "expires_at": null, "name": "a2"})).await;
    assert_eq!((v["read_only"].as_bool(), v["expires_at"].is_null(), v["name"].as_str()), (Some(false), true, Some("a2")));
    exec(&c, "INSERT INTO ks.t (k, v) VALUES (3, 'three')").await;

    for (body, status) in [
        (json!({"durability": "maybe"}), 400),
        (json!({"name": "b"}), 409),
        (json!({"name": "NOPE"}), 400),
        (json!({"settings": {"shards": 0}}), 400),
        (json!({"masking": {"ks.t.v": "upper(v)"}}), 400),
        (json!({"expires_at": 10}), 400),
        (json!({"history_retention": -5}), 400),
        (json!({"read_only": "yes"}), 400),
    ] {
        let (s, v) = e.call(Method::PATCH, &path, Some(body.clone())).await;
        assert_eq!(s, status, "{body}: {v}");
        assert!(v["error"].is_string());
    }
}

#[tokio::test]
async fn console_schema_activity_usage() {
    let e = env().await;
    let db = e.post("/v1/nosql/databases", json!({"name": "con"})).await;
    let id = id(&db);
    let cql = |q: &str| json!({ "query": q });
    let p = format!("/v1/nosql/databases/{id}/cql");
    let v = e.post(&p, cql("CREATE KEYSPACE app WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}")).await;
    assert_eq!(v["kind"], "schema_change");
    let v = e.post(&p, cql("USE app")).await;
    assert_eq!((v["kind"].as_str(), v["keyspace"].as_str()), (Some("set_keyspace"), Some("app")));
    e.post(&p, json!({"query": "CREATE TABLE t (k int PRIMARY KEY, v text)", "keyspace": "app"})).await;
    for i in 0..3 {
        let v = e.post(&p, json!({"query": format!("INSERT INTO t (k, v) VALUES ({i}, 'v{i}')"), "keyspace": "app"})).await;
        assert_eq!(v["kind"], "void");
    }
    let v = e.post(&p, json!({"query": "SELECT k, v FROM app.t", "page_size": 2})).await;
    assert_eq!(v["kind"], "rows");
    assert_eq!(v["columns"][1], json!({"name": "v", "type": "text", "keyspace": "app", "table": "t"}));
    assert_eq!(v["rows"].as_array().unwrap().len(), 2);
    let next = v["page_state"].as_str().expect("more pages").to_string();
    let v2 = e.post(&p, json!({"query": "SELECT k, v FROM app.t", "page_size": 2, "page_state": next})).await;
    assert_eq!(v2["rows"].as_array().unwrap().len(), 1);
    assert!(v2["page_state"].is_null());
    // CQL errors are results, as cqlsh shows them.
    let v = e.post(&p, cql("SELEC 1")).await;
    assert_eq!(v["code"], code::SYNTAX_ERROR);
    assert_eq!(v["code_name"], "SYNTAX_ERROR");
    assert!(v["error"].as_str().unwrap().contains("SELEC"));
    let (s, _) = e.call(Method::POST, &p, Some(json!({"query": " "}))).await;
    assert_eq!(s, 400);
    let (s, v) = e.call(Method::POST, &p, Some(json!({"query": "SELECT * FROM app.t", "page_state": "%%%"}))).await;
    assert_eq!((s, v["error"].as_str()), (400, Some("invalid page_state")));
    let (s, _) = e.call(Method::POST, &p, Some(json!({}))).await;
    assert_eq!(s, 400, "query is required");

    // Schema with statistics.
    let s = e.get(&format!("/v1/nosql/databases/{id}/schema")).await;
    let app = s["keyspaces"].as_array().unwrap().iter().find(|k| k["name"] == "app").unwrap();
    let t = &app["tables"][0];
    assert_eq!(t["name"], "t");
    assert_eq!(t["partition_key"], json!(["k"]));
    assert!(t["stats"]["writes"].as_u64().unwrap() >= 3, "{}", t["stats"]);
    assert_eq!(s["dialect"], "scylla");

    // Activity of a loaded database lists connected clients.
    let c = e.connect(&db).await;
    let a = e.get(&format!("/v1/nosql/databases/{id}/activity")).await;
    assert_eq!(a["state"], "running");
    assert!(a["metrics"]["connections"].as_u64().unwrap() >= 1);
    let clients = a["clients"].as_array().unwrap();
    assert_eq!(clients.len(), 1, "{a}");
    let cl = &clients[0];
    assert!(cl["address"].as_str().unwrap().starts_with("127.0.0.1:"), "{a}");
    assert_eq!(cl["username"], "cassandra", "{a}");
    assert_eq!(cl["driver_name"], "roll nosql-proto", "{a}");
    assert!(a["slow_queries"].as_array().unwrap().is_empty(), "{a}");
    // The console's CREATE KEYSPACE got ScyllaDB's replication factor warning.
    assert_eq!(a["warnings"].as_array().unwrap().len(), 1, "{a}");
    assert!(a["warnings"][0]["warning"].as_str().unwrap().contains("minimum_replication_factor_warn_threshold"), "{a}");
    assert!(a["warnings"][0]["statement"].as_str().unwrap().starts_with("CREATE KEYSPACE app"), "{a}");
    // Warnings clients got, newest first, with statement and role.
    let l = e.control.node.get_loaded(&id).unwrap();
    let mut st = l.db.settings().as_ref().clone();
    st.minimum_replication_factor_warn_threshold = 0;
    l.db.set_settings(st).unwrap();
    exec(&c, "CREATE KEYSPACE quiet WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}").await;
    let mut st = l.db.settings().as_ref().clone();
    st.minimum_replication_factor_warn_threshold = 3;
    l.db.set_settings(st).unwrap();
    exec(&c, "DROP KEYSPACE quiet").await;
    exec(&c, "CREATE KEYSPACE loud WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}").await;
    let a = e.get(&format!("/v1/nosql/databases/{id}/activity")).await;
    assert_eq!(a["warnings"].as_array().unwrap().len(), 2, "the live threshold silenced one: {a}");
    let w = &a["warnings"][0];
    assert!(w["warning"].as_str().unwrap().contains("minimum_replication_factor_warn_threshold"), "{a}");
    assert_eq!(w["statement"], "CREATE KEYSPACE loud WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}");
    assert_eq!(w["role"], "cassandra");
    assert!(w["at_ms"].as_i64().unwrap() >= a["warnings"][1]["at_ms"].as_i64().unwrap());
    exec(&c, "DROP KEYSPACE loud").await;
    // The slow query log, once on (ScyllaDB's `/storage_service/slow_query`).
    l.db.set_slow_query_log(nosql_engine::trace::SlowQueryLog { enable: true, threshold: 0, ..Default::default() });
    exec(&c, "SELECT v FROM app.t WHERE k = 1").await;
    l.db.set_slow_query_log(Default::default());
    let a = e.get(&format!("/v1/nosql/databases/{id}/activity")).await;
    let slow = a["slow_queries"].as_array().unwrap();
    assert!(slow.iter().any(|q| q["command"] == "SELECT v FROM app.t WHERE k = 1"), "{a}");
    // The console traces on request: the result names the trace, the events are readable.
    let v = e.post(&p, json!({"query": "SELECT v FROM app.t WHERE k = 1", "trace": true})).await;
    let trace = v["tracing_id"].as_str().expect("a tracing id").to_string();
    let events = e.post(&p, cql(&format!("SELECT activity FROM system_traces.events WHERE session_id = {trace}"))).await;
    assert!(!events["rows"].as_array().unwrap().is_empty(), "{events}");
    let v = e.post(&p, json!({"query": "SELECT v FROM app.t WHERE k = 1"})).await;
    assert!(v["tracing_id"].is_null(), "{v}");

    // Alternator requests are metered too (signed as the superuser: its salted hash is the
    // secret the API hands out).
    let creds = e.get(&format!("/v1/nosql/databases/{id}/alternator")).await;
    assert_eq!(creds["endpoint"], format!("http://127.0.0.1:{}", e.alternator.port()));
    assert_eq!(creds["enforced"], true);
    let secret = creds["secret_access_key"].as_str().unwrap().to_string();
    assert!(secret.starts_with("$6$"), "{creds}");
    let access = creds["access_key_id"].as_str().unwrap().to_string();
    for (op, body) in [("CreateTable", r#"{"TableName":"kv1","KeySchema":[{"AttributeName":"k","KeyType":"HASH"}],"AttributeDefinitions":[{"AttributeName":"k","AttributeType":"S"}],"BillingMode":"PAY_PER_REQUEST"}"#), ("PutItem", r#"{"TableName":"kv1","Item":{"k":{"S":"a"}}}"#), ("ListTables", "{}")] {
        let (status, v) = signed_dynamodb(&e, op, body, &access, &secret).await;
        assert_eq!(status, 200, "{op}: {v}");
    }
    let (_, v) = signed_dynamodb(&e, "ListTables", "{}", &access, &secret).await;
    assert_eq!(v, json!({"TableNames": ["kv1"]}));
    let (status, v) = signed_dynamodb(&e, "ListTables", "{}", &access, "wrong").await;
    assert_eq!(status, 400, "{v}");

    // Metering → usage and cost.
    exec(&c, "SELECT * FROM app.t").await;
    e.control.sample(60).unwrap();
    let u = e.get(&format!("/v1/nosql/databases/{id}/usage?days=1")).await;
    assert_eq!(u["days"], 1);
    assert_eq!(u["active_seconds"], 60);
    assert!(u["requests"].as_i64().unwrap() >= 7, "{u}");
    assert_eq!(u["alternator_requests"], 5, "{u}");
    assert!(u["writes"].as_i64().unwrap() >= 3, "{u}");
    assert!(u["reads"].as_i64().unwrap() >= 1, "{u}");
    assert!(u["connection_seconds"].as_i64().unwrap() >= 60, "{u}");
    assert!(u["net_out_bytes"].as_i64().unwrap() > 0, "{u}");
    let cost = &u["cost"];
    assert!(cost["total_usd"].as_f64().unwrap() > 0.0);
    for k in ["compute_usd", "read_usd", "write_usd", "storage_usd", "egress_usd", "keyspaces_usd", "dynamodb_usd", "astra_usd", "scylla_cloud_usd"] {
        assert!(cost[k].is_number(), "{k}");
    }
    assert!(cost["scylla_cloud_usd"].as_f64().unwrap() > cost["total_usd"].as_f64().unwrap());
    // A second sample only adds the new counts.
    e.control.sample(60).unwrap();
    let u2 = e.get(&format!("/v1/nosql/databases/{id}/usage")).await;
    assert_eq!(u2["active_seconds"], 120);
    assert_eq!(u2["writes"], u["writes"]);
    drop(c);

    // Flush, compact, unload.
    assert_eq!(e.post(&format!("/v1/nosql/databases/{id}/flush"), json!({})).await, json!({"flushed": true}));
    assert!(e.post(&format!("/v1/nosql/databases/{id}/compact"), json!({})).await["compacted_tables"].as_u64().unwrap() >= 1);
    let d = e.get(&format!("/v1/nosql/databases/{id}")).await;
    assert!(d["storage_bytes"].as_i64().unwrap() > 0, "{d}");
    assert_eq!(e.post(&format!("/v1/nosql/databases/{id}/unload"), json!({})).await, json!({"unloaded": true}));
    assert_eq!(e.get(&format!("/v1/nosql/databases/{id}")).await["state"], "idle");
    assert_eq!(e.post(&format!("/v1/nosql/databases/{id}/flush"), json!({})).await, json!({"flushed": false}));
    assert_eq!(e.get(&format!("/v1/nosql/databases/{id}/activity")).await["state"], "idle");
    assert_eq!(e.post(&format!("/v1/nosql/databases/{id}/unload"), json!({})).await, json!({"unloaded": false}));
    // Data survived.
    let v = e.post(&p, cql("SELECT v FROM app.t")).await;
    assert_eq!(v["rows"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn passwords_and_deletion() {
    let e = env().await;
    let db = e.post("/v1/nosql/databases", json!({"name": "pw", "dialect": "cassandra"})).await;
    let id = id(&db);
    let c = e.connect(&db).await;
    let rotated = e.post(&format!("/v1/nosql/databases/{id}/password"), json!({})).await;
    let new = rotated["credentials"]["password"].as_str().unwrap();
    assert_ne!(new, db["credentials"]["password"].as_str().unwrap());
    // The old password no longer works (neither the platform credential nor the role).
    let (code_, _) = server_code(e.try_connect(&db).await.unwrap_err());
    assert_eq!(code_, code::BAD_CREDENTIALS);
    e.connect(&rotated).await;
    // Plain role logins with the database's role table work too, through the engine.
    let rows = q(&c, "SELECT role, is_superuser FROM system_auth.roles WHERE role = 'cassandra'").await;
    assert_eq!(rows.len(), 1);
    // Existing connections stay.
    q(&c, "SELECT * FROM system.local").await;

    let (s, v) = e.call(Method::DELETE, &format!("/v1/nosql/databases/{id}"), None).await;
    assert_eq!((s, v), (200, json!({"deleted": id})));
    assert!(c.query("SELECT * FROM system.local", Default::default()).await.is_err(), "clients of a deleted database are disconnected");
    let (s, _) = e.call(Method::GET, &format!("/v1/nosql/databases/{id}"), None).await;
    assert_eq!(s, 404);
    assert!(e.try_connect(&rotated).await.is_err());
    // Its objects are collectable once garbage collection has seen them twice.
    let (_, st) = e.control.collect_garbage(std::time::Duration::ZERO).await.unwrap();
    assert!(st.unreferenced >= st.deleted);
}

#[tokio::test]
async fn branch_reset_restore_diff_import_errors() {
    let e = env().await;
    let db = e.post("/v1/nosql/databases", json!({"name": "root"})).await;
    let id = id(&db);
    let p = |r: &str| format!("/v1/nosql/databases/{id}{r}");
    let checks = [
        ("/branches", json!({"name": "root"}), 409, "a database named root already exists"),
        ("/branches", json!({"name": "Bad"}), 400, "name: 1-40 lowercase letters, digits or '-'"),
        ("/branches", json!({"name": "b", "at": 4_000_000_000i64}), 400, "that point in time is in the future"),
        ("/branches", json!({"name": "b", "at": 1}), 400, ""),
        ("/branches", json!({"name": "b", "expires_at": 1}), 400, "expires_at is in the past"),
        ("/branches", json!({}), 400, ""),
        ("/reset", json!({}), 409, "root is not a branch"),
        ("/restore", json!({}), 400, "at is required (unix seconds)"),
        ("/restore", json!({"at": 4_000_000_000i64}), 400, "that point in time is in the future"),
        ("/restore", json!({"at": 1}), 400, ""),
        ("/imports", json!({"from": "file:///nonexistent/roll-backup"}), 400, "/nonexistent/roll-backup is not a directory"),
        ("/imports", json!({"from": "file://relative"}), 400, "file:// sources need an absolute path"),
        ("/imports", json!({"from": "s3://"}), 400, "the source URL has no bucket"),
        ("/imports", json!({"from": "ftp://x"}), 400, "unsupported source scheme ftp:// (cql://, s3:// or file://)"),
        ("/imports", json!({"from": "cql://h:1", "every": 5}), 400, "every must be at least 60 seconds"),
        ("/imports", json!({"from": "cql://h:1", "keyspaces": ["system_auth"]}), 400, "can't import keyspace \"system_auth\""),
        ("/imports", json!({}), 400, ""),
    ];
    for (route, body, status, msg) in checks {
        let (s, v) = e.call(Method::POST, &p(route), Some(body.clone())).await;
        assert_eq!(s, status, "{route} {body}: {v}");
        if !msg.is_empty() {
            assert_eq!(v["error"], msg, "{route} {body}");
        }
    }
    let (s, v) = e.call(Method::GET, &p("/diff"), None).await;
    assert_eq!((s, v["error"].as_str()), (409, Some("root is not a branch")));

    // Reset and diff of a branch whose parent is gone.
    let br = e.post(&p("/branches"), json!({"name": "orphan"})).await;
    let (s, _) = e.call(Method::DELETE, &p(""), None).await;
    assert_eq!(s, 200);
    let orphan = format!("/v1/nosql/databases/{}", id_of(&br));
    let parent_gone = format!("the parent of orphan ({id}) was deleted");
    for (m, r) in [(Method::POST, "/reset"), (Method::GET, "/diff")] {
        let (s, v) = e.call(m, &format!("{orphan}{r}"), Some(json!({}))).await;
        assert_eq!((s, v["error"].as_str()), (409, Some(parent_gone.as_str())));
    }
    // The orphan still serves its data.
    e.connect(&br).await;

    // Imports into a read-only database are refused; an unreachable source fails in the
    // background and shows up in the list.
    let ro = e.post("/v1/nosql/databases", json!({"name": "ro", "read_only": true})).await;
    let (s, v) = e.call(Method::POST, &format!("/v1/nosql/databases/{}/imports", id_of(&ro)), Some(json!({"from": "cql://127.0.0.1:1"}))).await;
    assert_eq!((s, v["error"].as_str()), (409, Some("ro is read-only")));
    let imp = e.post(&format!("{orphan}/imports"), json!({"from": "cql://reader:secret@127.0.0.1:1", "keyspaces": ["app"], "every": 3600})).await;
    assert_eq!(imp["from"], "cql://reader:***@127.0.0.1:1");
    assert!(imp["state"] == "queued" || imp["state"] == "running");
    let mut list = Value::Null;
    for _ in 0..100 {
        list = e.get(&format!("{orphan}/imports")).await;
        if list[0]["state"] == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(list[0]["state"], "failed", "{list}");
    assert!(list[0]["error"].as_str().unwrap().contains("connecting to 127.0.0.1:1"), "{list}");
    assert_eq!(list[0]["every"], 3600);
    assert!(list[0]["next_run_at"].as_i64().is_some(), "a failed scheduled import runs again");
    assert!(!list.to_string().contains("secret"));
}

#[tokio::test]
async fn query_strings_bodies_and_internal_errors() {
    let e = env().await;
    let db = e.post("/v1/nosql/databases", json!({"name": "errs"})).await;
    let id = id(&db);
    let p = |r: &str| format!("/v1/nosql/databases/{id}{r}");
    // `days` is clamped to 1..=366; a malformed one is a JSON 400 like a malformed body.
    assert_eq!(e.get(&p("/usage?days=0")).await["days"], 1);
    assert_eq!(e.get(&p("/usage?days=5000")).await["days"], 366);
    assert_eq!(e.get(&p("/usage")).await["days"], 30);
    let (s, v) = e.call(Method::GET, &p("/usage?days=soon"), None).await;
    assert_eq!(s, 400, "{v}");
    assert!(v["error"].as_str().unwrap().starts_with("Failed to deserialize query string"), "{v}");
    // An empty body reads as `{}`: a create without a name, a patch that changes nothing.
    let r = e.http.post(format!("{}/v1/nosql/databases", e.api)).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400);
    assert!(r.json::<Value>().await.unwrap()["error"].as_str().unwrap().contains("missing field `name`"));
    let r = e.http.patch(format!("{}{}", e.api, p(""))).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    // Other methods and unknown routes aren't served.
    let (s, _) = e.call(Method::PUT, &p(""), Some(json!({}))).await;
    assert_eq!(s, 405);
    let (s, _) = e.call(Method::GET, &p("/nope"), None).await;
    assert_eq!(s, 404);

    // Storage failures are `500 {"error": …}`: a corrupt manifest can't be loaded, and the
    // database loads again once it is repaired.
    e.post(&p("/cql"), json!({"query": "CREATE KEYSPACE ks WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}"})).await;
    assert_eq!(e.post(&p("/unload"), json!({})).await, json!({"unloaded": true}));
    let key = nosql_storage::manifest::manifest_key(&id);
    let good = e.control.objects().get(&key).await.unwrap().expect("a manifest");
    e.control.objects().put(&key, bytes::Bytes::from_static(b"{not json")).await.unwrap();
    for (m, r) in [(Method::GET, "/schema"), (Method::POST, "/compact"), (Method::POST, "/cql")] {
        let (s, v) = e.call(m, &p(r), Some(json!({"query": "SELECT * FROM system.local"}))).await;
        assert_eq!(s, 500, "{r}: {v}");
        assert!(!v["error"].as_str().unwrap().is_empty(), "{r}: {v}");
    }
    e.control.objects().put(&key, good).await.unwrap();
    let s = e.get(&p("/schema")).await;
    assert!(s["keyspaces"].as_array().unwrap().iter().any(|k| k["name"] == "ks"), "{s}");
}

fn id_of(v: &Value) -> String {
    v["id"].as_str().unwrap().to_string()
}

/// A DynamoDB API request signed with SigV4 as `access` / `secret`.
async fn signed_dynamodb(e: &common::Env, op: &str, body: &str, access: &str, secret: &str) -> (u16, Value) {
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let host = e.alternator.to_string();
    let target = format!("DynamoDB_20120810.{op}");
    let req = nosql_alternator::http::HttpRequest {
        method: "POST".into(),
        path: "/".into(),
        query: String::new(),
        headers: vec![("host".into(), host.clone()), ("x-amz-date".into(), now.clone()), ("x-amz-target".into(), target.clone())],
        body: bytes::Bytes::copy_from_slice(body.as_bytes()),
        peer: None,
    };
    let signed = ["host".to_string(), "x-amz-date".to_string(), "x-amz-target".to_string()];
    let date = &now[..8];
    let sig = nosql_alternator::auth::sign(&req, body.as_bytes(), &signed, &now, (date, "us-east-1", "dynamodb"), secret);
    let r = e
        .http
        .post(format!("http://{host}/"))
        .header("Content-Type", "application/x-amz-json-1.0")
        .header("X-Amz-Target", target)
        .header("X-Amz-Date", now.clone())
        .header("Authorization", format!("AWS4-HMAC-SHA256 Credential={access}/{date}/us-east-1/dynamodb/aws4_request, SignedHeaders=host;x-amz-date;x-amz-target, Signature={sig}"))
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, serde_json::from_slice(&r.bytes().await.unwrap()).unwrap_or(Value::Null))
}
