//! The dashboard's data API: CQL over JSON with paging, the schema, table pages, auth.

mod common;

use std::sync::Arc;

use common::*;
use nosql_engine::Dialect;
use nosql_server::Directory;
use serde_json::{Value, json};

struct Api {
    base: String,
    http: reqwest::Client,
    s: SingleServer,
}

impl Api {
    async fn start(dialect: Dialect, token: Option<&str>) -> Api {
        let token = token.map(str::to_string);
        let s = single_with(dialect, |_| {}, |spec| spec.tokens.extend(token), |_| {}).await;
        let dir: Arc<dyn Directory> = s.dir.clone();
        let addr = nosql_server::http::start(dir).await.unwrap();
        Api { base: format!("http://{addr}"), http: reqwest::Client::new(), s }
    }

    async fn cql(&self, token: Option<&str>, body: Value) -> (u16, Value) {
        let mut r = self.http.post(format!("{}/cql", self.base)).json(&body);
        if let Some(t) = token {
            r = r.bearer_auth(t);
        }
        let resp = r.send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap())
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let resp = self.http.get(format!("{}{path}", self.base)).bearer_auth("db1_secret").send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }
}

const T: Option<&str> = Some("db1_secret");

async fn setup(api: &Api) {
    for q in [
        "CREATE KEYSPACE srv_h WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}",
        "CREATE TABLE srv_h.t (k int, c int, v text, l list<int>, m map<text, int>, b boolean, d double, PRIMARY KEY (k, c))",
    ] {
        let (st, body) = api.cql(T, json!({ "query": q })).await;
        assert_eq!(st, 200, "{body}");
    }
}

#[tokio::test]
async fn cql_over_json() {
    let api = Api::start(Dialect::Scylla, Some("secret")).await;
    // Auth.
    let (st, body) = api.cql(None, json!({ "query": "SELECT key FROM system.local" })).await;
    assert_eq!((st, body), (401, json!({ "error": "Unauthorized" })));
    assert_eq!(api.cql(Some("db1_wrong"), json!({ "query": "SELECT key FROM system.local" })).await.0, 401);
    let (st, body) = api.cql(T, json!({ "query": "CREATE KEYSPACE srv_h WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}" })).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["kind"], "schema_change");
    assert_eq!(body["schema_change"], json!({ "change": "CREATED", "target": "KEYSPACE", "keyspace": "srv_h", "name": null, "arg_types": null }));
    assert!(body["elapsed_ms"].is_number());
    let (st, _) = api.cql(T, json!({ "query": "CREATE TABLE srv_h.t (k int, c int, v text, l list<int>, m map<text, int>, b boolean, d double, PRIMARY KEY (k, c))" })).await;
    assert_eq!(st, 200);
    let (st, body) = api.cql(T, json!({ "query": "INSERT INTO srv_h.t (k, c, v, l, m, b, d) VALUES (1, 2, 'hi', [1, 2], {'a': 1}, true, 1.5)" })).await;
    assert_eq!((st, &body["kind"]), (200, &json!("void")));
    let (st, body) = api.cql(T, json!({ "query": "SELECT k, c, v, l, m, b, d FROM srv_h.t" })).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["kind"], "rows");
    let types: Vec<&str> = body["columns"].as_array().unwrap().iter().map(|c| c["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["int", "int", "text", "list<int>", "map<text, int>", "boolean", "double"]);
    assert_eq!(body["columns"][0], json!({ "name": "k", "type": "int", "keyspace": "srv_h", "table": "t" }));
    assert_eq!(body["rows"], json!([["1", "2", "hi", "[1, 2]", "{'a': 1}", "True", "1.5"]]));
    assert_eq!(body["values"], json!([[1, 2, "hi", [1, 2], { "a": 1 }, true, 1.5]]));
    assert_eq!(body["page_state"], Value::Null);
    assert_eq!(body["warnings"], json!([]));
    // Nulls.
    api.cql(T, json!({ "query": "INSERT INTO srv_h.t (k, c) VALUES (1, 3)" })).await;
    let (_, body) = api.cql(T, json!({ "query": "SELECT v FROM srv_h.t WHERE k = 1 AND c = 3" })).await;
    assert_eq!((&body["rows"], &body["values"]), (&json!([[null]]), &json!([[null]])));
    // A keyspace for the request, and USE.
    let (st, body) = api.cql(T, json!({ "query": "SELECT c FROM t WHERE k = 1", "keyspace": "srv_h" })).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["rows"].as_array().unwrap().len(), 2);
    let (_, body) = api.cql(T, json!({ "query": "USE srv_h" })).await;
    assert_eq!((&body["kind"], &body["keyspace"]), (&json!("set_keyspace"), &json!("srv_h")));
    // CQL errors.
    let (st, body) = api.cql(T, json!({ "query": "SELEC" })).await;
    assert_eq!((st, &body["code"], &body["code_name"]), (400, &json!(0x2000), &json!("SYNTAX_ERROR")));
    assert!(body["error"].as_str().unwrap().starts_with("line 1:0"), "{body}");
    let (st, body) = api.cql(T, json!({ "query": "SELECT * FROM nope.t" })).await;
    assert_eq!((st, &body["code_name"]), (400, &json!("INVALID")));
    // Bad requests.
    let resp = api.http.post(format!("{}/cql", api.base)).bearer_auth("db1_secret").body("{not json").send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let (st, body) = api.cql(T, json!({ "query": "SELECT k FROM srv_h.t", "page_state": "!!!" })).await;
    assert_eq!((st, body), (400, json!({ "error": "invalid page_state" })));
    // Other ways to pass the token.
    let resp = api.http.post(format!("{}/cql?_token=db1_secret", api.base)).json(&json!({ "query": "SELECT key FROM system.local" })).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let resp = api.http.post(format!("{}/cql", api.base)).basic_auth("x", Some("db1_secret")).json(&json!({ "query": "SELECT key FROM system.local" })).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let m = api.s.dir.loaded().metrics.snapshot();
    assert!(m.http_requests >= 10 && m.errors >= 3 && m.bytes_out > 0, "{m:?}");
}

#[tokio::test]
async fn paging_through_json() {
    let api = Api::start(Dialect::Cassandra, Some("secret")).await;
    setup(&api).await;
    for c in 0..25 {
        let (st, _) = api.cql(T, json!({ "query": format!("INSERT INTO srv_h.t (k, c, v) VALUES (7, {c}, 'x')") })).await;
        assert_eq!(st, 200);
    }
    let mut seen = 0;
    let mut state: Option<String> = None;
    let mut pages = 0;
    loop {
        let (st, body) = api.cql(T, json!({ "query": "SELECT c FROM srv_h.t WHERE k = 7", "page_size": 10, "page_state": state })).await;
        assert_eq!(st, 200, "{body}");
        let n = body["rows"].as_array().unwrap().len();
        assert_eq!(body["values"][0][0], json!(seen), "pages continue where they stopped");
        seen += n;
        pages += 1;
        match body["page_state"].as_str() {
            Some(s) => state = Some(s.to_string()),
            None => break,
        }
    }
    assert_eq!(seen, 25);
    assert!(pages >= 3);
    // Table pages.
    let (st, body) = api.get("/tables/srv_h/t?page_size=4").await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["rows"].as_array().unwrap().len(), 4);
    assert!(body["page_state"].is_string());
    let next = body["page_state"].as_str().unwrap().replace('+', "%2B").replace('/', "%2F").replace('=', "%3D");
    let (st, body2) = api.get(&format!("/tables/srv_h/t?page_size=4&page_state={next}")).await;
    assert_eq!(st, 200);
    assert_ne!(body2["values"], body["values"]);
    let (st, body) = api.get("/tables/srv_h/nope").await;
    assert_eq!((st, body), (404, json!({ "error": "table srv_h.nope does not exist" })));
}

#[tokio::test]
async fn schema_listing() {
    let api = Api::start(Dialect::Scylla, Some("secret")).await;
    setup(&api).await;
    api.cql(T, json!({ "query": "CREATE TYPE srv_h.addr (street text, zip int)" })).await;
    let (st, body) = api.get("/schema").await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["dialect"], "scylla");
    let ks = body["keyspaces"].as_array().unwrap();
    assert!(ks.iter().any(|k| k["name"] == "system" && k["system"] == true));
    let h = ks.iter().find(|k| k["name"] == "srv_h").unwrap();
    assert_eq!(h["system"], false);
    assert_eq!(h["replication"]["class"], "org.apache.cassandra.locator.NetworkTopologyStrategy");
    let t = &h["tables"][0];
    assert_eq!(t["name"], "t");
    assert_eq!(t["partition_key"], json!(["k"]));
    assert_eq!(t["clustering_key"], json!(["c"]));
    let cols: Vec<(&str, &str, &str)> =
        t["columns"].as_array().unwrap().iter().map(|c| (c["name"].as_str().unwrap(), c["type"].as_str().unwrap(), c["kind"].as_str().unwrap())).collect();
    assert!(cols.contains(&("k", "int", "partition_key")), "{cols:?}");
    assert!(cols.contains(&("c", "int", "clustering")), "{cols:?}");
    assert!(cols.contains(&("m", "map<text, int>", "regular")), "{cols:?}");
    if let Some(types) = h["types"].as_array().filter(|t| !t.is_empty()) {
        assert_eq!(types[0]["name"], "addr");
        assert_eq!(types[0]["fields"][1], json!({ "name": "zip", "type": "int" }));
    }
    assert!(body.get("describe").is_some());
}

#[tokio::test]
async fn routing_cors_and_errors() {
    // Without tokens the single database is open.
    let api = Api::start(Dialect::Scylla, None).await;
    let (st, _) = api.cql(None, json!({ "query": "SELECT key FROM system.local" })).await;
    assert_eq!(st, 200);
    let resp = api.http.request(reqwest::Method::OPTIONS, format!("{}/cql", api.base)).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 204);
    assert_eq!(resp.headers()["access-control-allow-origin"], "*");
    let (st, body) = api.get("/nope").await;
    assert_eq!((st, body), (404, json!({ "error": "not found" })));
    // A node: tokens name their database.
    let n = node(nosql_server::NodeConfig::default(), nosql_server::ListenOptions::default()).await;
    n.catalog.insert(nosql_server::DbSpec::new("n1", Dialect::Scylla).with_token("s1"));
    n.catalog.insert(nosql_server::DbSpec::new("n2", Dialect::Cassandra).with_token("s2"));
    let dir: Arc<dyn Directory> = n.node.clone();
    let addr = nosql_server::http::start(dir).await.unwrap();
    let http = reqwest::Client::new();
    for (tok, want) in [("n1_s1", 200), ("n2_s2", 200), ("n1_s2", 401), ("n3_s1", 401)] {
        let resp = http.post(format!("http://{addr}/cql")).bearer_auth(tok).json(&json!({ "query": "SELECT key FROM system.local" })).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), want, "{tok}");
    }
    let resp = http.get(format!("http://{addr}/schema")).bearer_auth("n2_s2").send().await.unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["dialect"], "cassandra");
    assert_eq!(n.node.metrics("n2").unwrap().http_requests, 2);
}
