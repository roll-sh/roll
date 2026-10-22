//! `roll nosql …` against a mock `/v1/nosql` server: argument parsing and the exact requests.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, StatusCode, Uri};
use axum::response::IntoResponse;
use clap::Parser;
use serde_json::{Value, json};

use super::*;

#[derive(Parser)]
struct T {
    #[command(subcommand)]
    cmd: Nosql,
}

fn parse(args: &[&str]) -> Nosql {
    T::try_parse_from(std::iter::once("roll").chain(args.iter().copied())).unwrap_or_else(|e| panic!("{args:?}: {e}")).cmd
}

fn rejects(args: &[&str]) {
    assert!(T::try_parse_from(std::iter::once("roll").chain(args.iter().copied())).is_err(), "{args:?} should not parse");
}

/// One request the mock saw.
#[derive(Clone, Debug)]
struct Seen {
    method: Method,
    path: String,
    body: Value,
}

type Log = Arc<Mutex<Vec<Seen>>>;

fn db() -> Value {
    json!({
        "id": "nk3v9q2m7xa1", "name": "app-prod", "dialect": "scylla", "state": "running",
        "datacenter": "us-east-1", "rack": "rack1", "cluster_name": "prod",
        "endpoints": { "cql": "nk3v9q2m7xa1.nosql.roll.sh:9042", "alternator": "https://nk3v9q2m7xa1.alternator.roll.sh" },
        "tls": true, "server_name": "nk3v9q2m7xa1.nosql.roll.sh",
        "credentials": { "username": "cassandra", "password": "p@ss" },
        "idle_timeout": 300, "durability": "async", "history_retention": 604800,
        "storage_bytes": 2048
    })
}

async fn handler(State(log): State<Log>, method: Method, uri: Uri, body: Bytes) -> impl IntoResponse {
    let path = uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    log.lock().unwrap().push(Seen { method: method.clone(), path: path.clone(), body: body.clone() });
    let d = db();
    let ok = |v: Value| (StatusCode::OK, axum::Json(v)).into_response();
    match (method.as_str(), path.as_str()) {
        ("GET", "/v1/nosql/databases") => ok(json!([d])),
        ("GET", "/v1/nosql/databases/nk3v9q2m7xa1") => ok(d),
        ("GET", p) if p.starts_with("/v1/nosql/databases/") && p.matches('/').count() == 4 => {
            (StatusCode::NOT_FOUND, axum::Json(json!({ "error": "no such database" }))).into_response()
        }
        ("POST", "/v1/nosql/databases") => ok(json!({ "id": "nnew0000000a", "name": body["name"], "dialect": body["dialect"], "state": "running" })),
        ("POST", "/v1/nosql/databases/nk3v9q2m7xa1/branches") => ok(json!({ "id": "nbr000000001", "name": body["name"], "parent_id": "nk3v9q2m7xa1" })),
        ("POST", "/v1/nosql/databases/nk3v9q2m7xa1/cql") => match body["query"].as_str() {
            Some(q) if q.starts_with("SELECT") => ok(json!({
                "columns": [{ "name": "k", "type": "int" }, { "name": "v", "type": "text" }],
                "rows": [[1, "a"], [2, null]],
                "warnings": ["Aggregation query used without partition key"],
            })),
            Some(q) if q.starts_with("USE") => ok(json!({})),
            _ => ok(json!({ "error": "SyntaxException: line 1:0 no viable alternative at input 'SELEC'" })),
        },
        ("GET", "/v1/nosql/databases/nk3v9q2m7xa1/diff") => ok(json!({ "diff": "ALTER TABLE ks.t ADD x int;" })),
        ("DELETE", _) => (StatusCode::NO_CONTENT, "").into_response(),
        _ => ok(json!({ "ok": true, "id": "x1", "state": "queued" })),
    }
}

async fn mock() -> (Api, Log) {
    let log: Log = Arc::default();
    let app = Router::new().fallback(handler).with_state(log.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (Api { base: format!("http://{addr}"), http: reqwest::Client::new(), key: Some("k:s".into()) }, log)
}

/// Runs a command and returns the requests after the database lookup.
async fn requests(args: &[&str]) -> Vec<Seen> {
    let (api, log) = mock().await;
    run(&api, true, parse(args)).await.unwrap_or_else(|e| panic!("{args:?}: {e}"));
    let seen = log.lock().unwrap().clone();
    seen.into_iter().filter(|s| !(s.method == Method::GET && s.path.starts_with("/v1/nosql/databases") && s.path.matches('/').count() <= 4 && !s.path.contains('?'))).collect()
}

fn one(seen: &[Seen]) -> &Seen {
    assert_eq!(seen.len(), 1, "{seen:?}");
    &seen[0]
}

#[tokio::test]
async fn list_show_and_lookup_by_name() {
    let (api, log) = mock().await;
    run(&api, true, parse(&["list"])).await.unwrap();
    run(&api, true, parse(&["show", "app-prod"])).await.unwrap();
    run(&api, false, parse(&["show", "nk3v9q2m7xa1"])).await.unwrap();
    let paths: Vec<String> = log.lock().unwrap().iter().map(|s| format!("{} {}", s.method, s.path)).collect();
    assert_eq!(
        paths,
        [
            "GET /v1/nosql/databases",
            "GET /v1/nosql/databases/app-prod",
            "GET /v1/nosql/databases",
            "GET /v1/nosql/databases/nk3v9q2m7xa1",
            "GET /v1/nosql/databases/nk3v9q2m7xa1",
        ]
    );
    let err = run(&api, true, parse(&["show", "nope"])).await.unwrap_err();
    assert_eq!(err.to_string(), "no nosql database nope");
}

#[tokio::test]
async fn create() {
    let seen = requests(&[
        "create", "pr-482", "--dialect", "cassandra", "--datacenter", "us-east-1", "--rack", "r1", "--cluster-name", "prod",
        "--setting", "batch_size_warn_threshold_in_kb=10", "--setting", "authenticator=PasswordAuthenticator",
    ])
    .await;
    let s = one(&seen);
    assert_eq!((s.method.as_str(), s.path.as_str()), ("POST", "/v1/nosql/databases"));
    assert_eq!(
        s.body,
        json!({ "name": "pr-482", "dialect": "cassandra", "datacenter": "us-east-1", "rack": "r1", "cluster_name": "prod",
                "settings": { "batch_size_warn_threshold_in_kb": 10, "authenticator": "PasswordAuthenticator" } })
    );
    let seen = requests(&["create", "plain"]).await;
    assert_eq!(one(&seen).body, json!({ "name": "plain" }));
    rejects(&["create", "x", "--dialect", "mysql"]);
    rejects(&["create"]);
}

#[tokio::test]
async fn set() {
    let seen = requests(&[
        "set", "app-prod", "--idle", "5m", "--durability", "sync", "--retention", "7d", "--read-only", "true", "--expires", "never",
        "--mask", "ks.users.email=mask_inner(1, 1)", "--mask", "ks.users.ssn=mask_hash('SHA-256')", "--setting", "tombstone_warn_threshold=500",
    ])
    .await;
    let s = one(&seen);
    assert_eq!((s.method.as_str(), s.path.as_str()), ("PATCH", "/v1/nosql/databases/nk3v9q2m7xa1"));
    assert_eq!(
        s.body,
        json!({ "idle_timeout": 300, "durability": "sync", "history_retention": 604800, "read_only": true, "expires_at": null,
                "masking": { "ks.users.email": "mask_inner(1, 1)", "ks.users.ssn": "mask_hash('SHA-256')" },
                "settings": { "tombstone_warn_threshold": 500 } })
    );
    let seen = requests(&["set", "app-prod", "--no-masking", "--idle", "0"]).await;
    assert_eq!(one(&seen).body, json!({ "masking": {}, "idle_timeout": 0 }));
    let seen = requests(&["set", "app-prod", "--expires", "2026-11-01 00:00"]).await;
    assert_eq!(one(&seen).body, json!({ "expires_at": 1_793_491_200 }));
    let before = now();
    let seen = requests(&["set", "app-prod", "--expires", "72h"]).await;
    let t = one(&seen).body["expires_at"].as_i64().unwrap();
    assert!((before + 72 * 3600..=now() + 72 * 3600).contains(&t));
    let (api, _) = mock().await;
    assert!(run(&api, true, parse(&["set", "app-prod"])).await.is_err());
    rejects(&["set", "app-prod", "--durability", "maybe"]);
    rejects(&["set", "app-prod", "--mask", "a=b", "--no-masking"]);
    rejects(&["set", "app-prod", "--read-only", "perhaps"]);
}

#[tokio::test]
async fn branch_reset_restore() {
    let seen = requests(&["branch", "app-prod", "--name", "pr-482", "--at", "2026-10-20 08:00", "--schema-only", "--expires", "1800000000", "--read-only"]).await;
    let s = one(&seen);
    assert_eq!((s.method.as_str(), s.path.as_str()), ("POST", "/v1/nosql/databases/nk3v9q2m7xa1/branches"));
    assert_eq!(s.body, json!({ "name": "pr-482", "at": 1_792_483_200, "schema_only": true, "expires_at": 1_800_000_000, "read_only": true }));
    let seen = requests(&["branch", "app-prod", "--name", "pr-1"]).await;
    assert_eq!(one(&seen).body, json!({ "name": "pr-1" }));
    let before = now();
    let seen = requests(&["branch", "app-prod", "--name", "pr-2", "--at", "15m"]).await;
    let at = one(&seen).body["at"].as_i64().unwrap();
    assert!((before - 900..=now() - 900).contains(&at));
    rejects(&["branch", "app-prod"]);

    let seen = requests(&["reset", "app-prod"]).await;
    let s = one(&seen);
    assert_eq!((s.path.as_str(), &s.body), ("/v1/nosql/databases/nk3v9q2m7xa1/reset", &json!({ "at": null })));
    let seen = requests(&["reset", "app-prod", "--at", "1792483200"]).await;
    assert_eq!(one(&seen).body, json!({ "at": 1_792_483_200 }));
    let seen = requests(&["restore", "app-prod", "--at", "2026-10-20T08:00:00Z"]).await;
    let s = one(&seen);
    assert_eq!((s.path.as_str(), &s.body), ("/v1/nosql/databases/nk3v9q2m7xa1/restore", &json!({ "at": 1_792_483_200 })));
    rejects(&["restore", "app-prod"]);
}

#[tokio::test]
async fn imports_and_misc() {
    let seen = requests(&["import", "app-prod", "--from", "cql://ro:pw@prod:9042", "--keyspaces", "app,events", "--every", "24h"]).await;
    let s = one(&seen);
    assert_eq!((s.method.as_str(), s.path.as_str()), ("POST", "/v1/nosql/databases/nk3v9q2m7xa1/imports"));
    assert_eq!(s.body, json!({ "from": "cql://ro:pw@prod:9042", "keyspaces": ["app", "events"], "every": 86400 }));
    let seen = requests(&["import", "app-prod", "--from", "s3://bucket/backup"]).await;
    assert_eq!(one(&seen).body, json!({ "from": "s3://bucket/backup" }));
    rejects(&["import", "app-prod"]);
    for (cmd, method, path) in [
        ("imports", "GET", "/imports"),
        ("diff", "GET", "/diff"),
        ("rotate-password", "POST", "/password"),
        ("flush", "POST", "/flush"),
        ("compact", "POST", "/compact"),
        ("unload", "POST", "/unload"),
        ("tablestats", "GET", "/schema"),
        ("delete", "DELETE", ""),
    ] {
        let seen = requests(&[cmd, "app-prod"]).await;
        let s = one(&seen);
        assert_eq!((s.method.as_str(), s.path.as_str()), (method, format!("/v1/nosql/databases/nk3v9q2m7xa1{path}").as_str()), "{cmd}");
    }
    let seen = requests(&["usage", "app-prod", "--days", "7"]).await;
    assert_eq!(one(&seen).path, "/v1/nosql/databases/nk3v9q2m7xa1/usage?days=7");
    let seen = requests(&["usage", "app-prod"]).await;
    assert_eq!(one(&seen).path, "/v1/nosql/databases/nk3v9q2m7xa1/usage?days=30");
    let seen = requests(&["status", "app-prod"]).await;
    assert_eq!(one(&seen).path, "/v1/nosql/databases/nk3v9q2m7xa1/activity");
    let seen = requests(&["tablestats", "app-prod", "ks.t"]).await;
    assert_eq!(one(&seen).path, "/v1/nosql/databases/nk3v9q2m7xa1/schema");
    let (api, log) = mock().await;
    run(&api, false, parse(&["diff", "app-prod"])).await.unwrap();
    run(&api, false, parse(&["url", "app-prod"])).await.unwrap();
    run(&api, false, parse(&["connect", "app-prod", "--driver", "go"])).await.unwrap();
    assert!(log.lock().unwrap().iter().all(|s| s.method == Method::GET));
    rejects(&["connect", "app-prod", "--driver", "cobol"]);
}

#[tokio::test]
async fn exec_through_the_api() {
    let (api, log) = mock().await;
    let err = run(&api, true, parse(&["exec", "app-prod", "--via-api", "-k", "ks", "-e", "SELECT * FROM t; SELEC 1"])).await.unwrap_err();
    assert_eq!(err.downcast_ref::<Exit>().map(|e| e.0), Some(2));
    let seen: Vec<Seen> = log.lock().unwrap().iter().filter(|s| s.path.ends_with("/cql")).cloned().collect();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].path, "/v1/nosql/databases/nk3v9q2m7xa1/cql");
    assert_eq!(seen[0].body, json!({ "query": "SELECT * FROM t", "keyspace": "ks", "page_state": null }));
    assert_eq!(seen[1].body["query"], "SELEC 1");
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("m.cql");
    std::fs::write(&f, "USE other;\nSELECT 'a;b' FROM t;\n").unwrap();
    let seen = requests(&["exec", "app-prod", "--via-api", "-f", f.to_str().unwrap()]).await;
    assert_eq!(seen[0].body["query"], "USE other");
    assert_eq!(seen[1].body, json!({ "query": "SELECT 'a;b' FROM t", "keyspace": "other", "page_state": null }));
    rejects(&["exec", "app-prod"]);
    rejects(&["exec", "app-prod", "-e", "x", "-f", "y"]);
}

#[test]
fn cqlsh_arguments() {
    let Nosql::Cqlsh { db, args } = parse(&["cqlsh", "--host", "10.0.0.1", "--port", "9043", "-u", "u", "-p", "pw", "-k", "ks", "-e", "SELECT 1", "--ssl", "--no-color", "--protocol-version", "4", "--request-timeout", "30"]) else {
        panic!()
    };
    assert_eq!(db, None);
    assert_eq!((args.host.as_deref(), args.port, args.username.as_deref(), args.password.as_deref()), (Some("10.0.0.1"), Some(9043), Some("u"), Some("pw")));
    assert_eq!((args.keyspace.as_deref(), args.execute.as_deref(), args.ssl, args.no_color), (Some("ks"), Some("SELECT 1"), true, true));
    assert_eq!((args.protocol_version, args.request_timeout, args.connect_timeout), (Some(4), 30, 5));
    let Nosql::Cqlsh { db, args } = parse(&["cqlsh", "app-prod", "-f", "x.cql", "-C", "-t"]) else { panic!() };
    assert_eq!((db.as_deref(), args.file.as_deref(), args.color, args.tty), (Some("app-prod"), Some("x.cql"), true, true));
    rejects(&["cqlsh", "-C", "--no-color"]);
}

#[test]
fn connection_info() {
    let d = db();
    assert_eq!(cql_url(&d), "cql://cassandra:p%40ss@nk3v9q2m7xa1.nosql.roll.sh:9042?ssl=true");
    let ep = endpoint(&d);
    assert_eq!((ep.host.as_str(), ep.port, ep.tls), ("nk3v9q2m7xa1.nosql.roll.sh", 9042, true));
    assert_eq!(ep.server_name.as_deref(), Some("nk3v9q2m7xa1.nosql.roll.sh"));
    assert_eq!((ep.username.as_deref(), ep.password.as_deref()), (Some("cassandra"), Some("p@ss")));
    let all = snippets(&d, None);
    for (name, needle) in [
        ("java", ".withLocalDatacenter(\"us-east-1\")"),
        ("python", "DCAwareRoundRobinPolicy(local_dc=\"us-east-1\")"),
        ("python", "\"server_hostname\": \"nk3v9q2m7xa1.nosql.roll.sh\""),
        ("go", "ServerName: \"nk3v9q2m7xa1.nosql.roll.sh\""),
        ("go", "DCAwareRoundRobinPolicy(\"us-east-1\")"),
        ("rust", ".known_node(\"nk3v9q2m7xa1.nosql.roll.sh:9042\")"),
        ("node", "localDataCenter: 'us-east-1'"),
        ("node", "servername: 'nk3v9q2m7xa1.nosql.roll.sh'"),
    ] {
        assert!(all.contains(needle), "{name}: {needle}");
        assert!(snippets(&d, Some(name)).contains(needle), "{name} alone");
    }
    assert!(!snippets(&d, Some("go")).contains("CqlSession"));
    let mut local = db();
    local["tls"] = json!(false);
    local["endpoints"]["cql"] = json!("127.0.0.1:59042");
    assert_eq!(cql_url(&local), "cql://cassandra:p%40ss@127.0.0.1:59042");
    assert!(!snippets(&local, None).contains("ssl"));
}

#[test]
fn values() {
    assert_eq!(seconds("90").unwrap(), 90);
    assert_eq!(seconds("15m").unwrap(), 900);
    assert_eq!(seconds("2h").unwrap(), 7200);
    assert_eq!(seconds("7d").unwrap(), 604_800);
    assert_eq!(seconds("1w").unwrap(), 604_800);
    assert!(seconds("3x").is_err());
    assert_eq!(at("1792483200").unwrap(), 1_792_483_200);
    assert_eq!(at("2026-10-20 08:00").unwrap(), 1_792_483_200);
    assert_eq!(at("2026-10-20T08:00:00+00:00").unwrap(), 1_792_483_200);
    assert_eq!(at("2026-10-20").unwrap(), 1_792_454_400);
    assert!(at("yesterday").is_err());
    assert_eq!(expires("never").unwrap(), Value::Null);
    assert_eq!(expires("2026-10-20 08:00").unwrap(), json!(1_792_483_200));
    let p = pairs(&["a=1".into(), "b=x y".into(), "c=true".into(), "d={\"k\": 1}".into()]).unwrap();
    assert_eq!(Value::Object(p), json!({ "a": 1, "b": "x y", "c": true, "d": { "k": 1 } }));
    assert!(pairs(&["novalue".into()]).is_err());
}
