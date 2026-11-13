//! `/v1/mcp` in process behind rolld's gate (docs/mcp.md): tools are listed by what the
//! connection can do, every call is decided by the same rules as the HTTP API, approvals come
//! back as results the agent acts on, and every call is in the audit log with its arguments
//! redacted.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::routing::{delete, get};
use http_body_util::BodyExt;
use roll_access::{Access, Config, Resolvers};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

const OPERATOR: &str = "operator-token-0123456789abcdef-0123";

struct Daemon {
    router: Router,
    _dir: tempfile::TempDir,
}

fn products() -> Router {
    Router::new()
        .route("/v1/projects", get(|| async { axum::Json(json!([{ "id": "pa" }])) }))
        .route("/v1/projects/{id}", delete(|| async { axum::http::StatusCode::NO_CONTENT }))
}

impl Daemon {
    fn new() -> Daemon {
        Daemon::with_products(vec!["postgres".into()])
    }

    fn with_products(on: Vec<String>) -> Daemon {
        let dir = tempfile::tempdir().unwrap();
        let ws = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
        let workspace = Arc::new(roll_workspace::Workspace::new(ws).unwrap());
        let config = Config { locked: true, approval_wait: std::time::Duration::from_millis(50), ..Config::default() };
        let access = Access::with_db(code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("access.db")), "access").unwrap(), config, Resolvers::default()).unwrap();
        let gate = roll_auth::Gate::new(Some(roll_auth::AdminToken::new(OPERATOR).unwrap()), vec![roll_auth::Rule { route: "/v1/mcp", access: roll_auth::Access::Product("MCP: answers 401 itself") }]);
        gate.set_authorizer(Arc::new(roll_access::evaluate::Evaluator(access.clone())));
        let layer = || axum::middleware::from_fn_with_state(gate.clone(), roll_auth::check);
        let api = products().merge(roll_workspace::router(workspace.clone())).merge(roll_access::api::router(access.clone()));
        let mcp_config = roll_mcp::Config { api_url: "http://127.0.0.1:7070".into(), web_url: "http://localhost:3000".into(), locked: true, products: on, access: Some(access.clone()) };
        let mcp = roll_mcp::routes(api.clone().layer(layer()), mcp_config, Vec::new());
        Daemon { router: api.merge(mcp).layer(layer()), _dir: dir }
    }

    async fn http(&self, token: &str, method: Method, path: &str, body: Option<Value>) -> (StatusCode, axum::http::HeaderMap, Value) {
        self.http_with(token, method, path, body, &[]).await
    }

    async fn http_with(&self, token: &str, method: Method, path: &str, body: Option<Value>, extra: &[(&str, &str)]) -> (StatusCode, axum::http::HeaderMap, Value) {
        let mut req = Request::builder().method(method).uri(path).header("content-type", "application/json").header("accept", "application/json, text/event-stream");
        for (k, v) in extra {
            req = req.header(*k, *v);
        }
        if !token.is_empty() {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let resp = self.router.clone().oneshot(req.body(body.map_or(Body::empty(), |b| Body::from(b.to_string()))).unwrap()).await.unwrap();
        let (status, headers) = (resp.status(), resp.headers().clone());
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, headers, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn rpc(&self, token: &str, method: &str, params: Value) -> Value {
        let (s, _, out) = self.http(token, Method::POST, "/v1/mcp", Some(json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))).await;
        assert_eq!(s, StatusCode::OK, "{out}");
        out["result"].clone()
    }

    async fn tool(&self, token: &str, name: &str, args: Value) -> Value {
        self.rpc(token, "tools/call", json!({ "name": name, "arguments": args })).await
    }

    async fn token(&self, rules: Value) -> String {
        let (s, _, out) = self.http(OPERATOR, Method::POST, "/v1/access/tokens", Some(json!({ "name": "cursor", "rules": rules }))).await;
        assert_eq!(s, StatusCode::CREATED, "{out}");
        out["token"].as_str().unwrap().to_string()
    }
}

fn names(list: &Value) -> Vec<String> {
    list["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn tools_follow_the_tokens_rules() {
    let d = Daemon::new();
    // No token: 401 naming the protected resource, so clients start OAuth.
    let (s, h, _) = d.http("", Method::POST, "/v1/mcp", Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert!(h["www-authenticate"].to_str().unwrap().contains("/.well-known/oauth-protected-resource/v1/mcp"));

    let init = d.rpc(OPERATOR, "initialize", json!({ "protocolVersion": "2025-06-18" })).await;
    assert_eq!(init["protocolVersion"], "2025-06-18");
    let all = names(&d.rpc(OPERATOR, "tools/list", json!({})).await);
    assert!(all.contains(&"postgres_delete_project".into()) && all.contains(&"workspace_list_projects".into()));
    assert!(!all.iter().any(|n| n.starts_with("redis_")), "products that are off have no tools");
    // A toolset narrows the list to those products' tools (approvals always there); an unknown one is refused.
    let (_, _, some) = d.http(OPERATOR, Method::POST, "/v1/mcp?toolsets=workspace", Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))).await;
    let some = names(&some["result"]);
    assert!(some.contains(&"workspace_list_projects".into()) && some.contains(&"access_wait_approval".into()) && !some.iter().any(|n| n.starts_with("postgres_")), "{some:?}");
    let (s, _, _) = d.http(OPERATOR, Method::POST, "/v1/mcp?toolsets=nope", Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // A read-only token sees only tools it can use, and its calls are decided by its rules.
    let reader = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }])).await;
    let listed = names(&d.rpc(&reader, "tools/list", json!({})).await);
    assert!(listed.contains(&"postgres_list_projects".into()) && listed.contains(&"postgres_query".into()));
    assert!(!listed.contains(&"postgres_delete_project".into()) && !listed.contains(&"postgres_execute".into()) && !listed.contains(&"workspace_list_projects".into()));
    // It can read what it may do, in words, before trying.
    let listed = d.rpc(&reader, "resources/list", json!({})).await;
    assert!(listed["resources"].as_array().unwrap().iter().any(|r| r["uri"] == "roll://access/me"), "{listed}");
    let me = d.rpc(&reader, "resources/read", json!({ "uri": "roll://access/me" })).await;
    let text = me["contents"][0]["text"].as_str().unwrap();
    assert!(text.contains("postgres: read, on every resource") && text.contains("Anything else is refused"), "{text}");
    // Prompts (roll's workflows as slash commands) only with their tools: a postgres reader gets
    // none of the secrets or apps ones.
    let prompts = d.rpc(&reader, "prompts/list", json!({})).await;
    let offered: Vec<&str> = prompts["prompts"].as_array().unwrap().iter().filter_map(|p| p["name"].as_str()).collect();
    assert!(!offered.iter().any(|n| *n == "fix_secrets_health" || *n == "debug_failing_service"), "{offered:?}");
    let ok = d.tool(&reader, "postgres_list_projects", json!({})).await;
    assert_eq!((ok["isError"].as_bool(), &ok["structuredContent"]["items"][0]["id"]), (Some(false), &json!("pa")));
    let refused = d.tool(&reader, "postgres_delete_project", json!({ "project": "pa" })).await;
    assert_eq!(refused["isError"], true);
    let bad = d.tool(&reader, "postgres_list_projects", json!({ "nope": 1 })).await;
    assert_eq!(bad["isError"], true);

    // Ask: the call comes back pending, and runs once approved when called again with it.
    let asker = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "delete": "ask" } }])).await;
    let pending = d.tool(&asker, "postgres_delete_project", json!({ "project": "pa" })).await;
    assert_eq!((pending["isError"].as_bool(), pending["structuredContent"]["status"].as_str()), (Some(false), Some("approval_pending")), "{pending}");
    let id = pending["structuredContent"]["approval"].as_str().unwrap();
    let waited = d.tool(&asker, "access_wait_approval", json!({ "approval_id": id, "wait": 0 })).await;
    assert_eq!(waited["structuredContent"]["state"], "pending");
    // Sent again without the id, the same call waits on the same approval (one per request) and,
    // once approved, runs.
    let again = d.tool(&asker, "postgres_delete_project", json!({ "project": "pa" })).await;
    assert_eq!(again["structuredContent"]["approval"].as_str(), Some(id), "{again}");
    assert_eq!(d.http(OPERATOR, Method::POST, &format!("/v1/access/approvals/{id}/approve"), None).await.0, StatusCode::OK);
    let done = d.tool(&asker, "postgres_delete_project", json!({ "project": "pa" })).await;
    assert_eq!(done["isError"], false, "{done}");

    // A client that opens URLs gets the approval as one to open (MCP's URL elicitation), named by
    // the session id initialize gave it.
    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-11-25", "capabilities": { "elicitation": { "url": {} } }, "clientInfo": { "name": "cursor", "version": "1.2" } } });
    let (_, h, _) = d.http(&asker, Method::POST, "/v1/mcp", Some(init)).await;
    let session = h["mcp-session-id"].to_str().unwrap().to_string();
    assert!(session.starts_with("roll-url-1.") && session.len() > 20, "unique per session: {session}");
    let call = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "postgres_delete_project", "arguments": { "project": "pb" } } });
    let (_, _, out) = d.http_with(&asker, Method::POST, "/v1/mcp", Some(call), &[("mcp-session-id", &session)]).await;
    let e = &out["error"]["data"]["elicitations"][0];
    assert_eq!((out["error"]["code"].as_i64(), e["mode"].as_str()), (Some(-32042), Some("url")), "{out}");
    assert!(e["url"].as_str().unwrap().contains(e["elicitationId"].as_str().unwrap()), "{out}");
    // And it's named in the audit log by what it called itself (the session id carries it).
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (_, _, log) = d.http(OPERATOR, Method::GET, "/v1/access/audit?source=mcp&limit=1", None).await;
    assert_eq!(log[0]["client"], "cursor 1.2", "{log}");

    // Every call is audited with its tool and redacted arguments.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    d.tool(&reader, "postgres_query", json!({ "endpoint": "ep_x", "sql": "select 'password'", "approval": "apr_secret" })).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    d.tool(OPERATOR, "postgres_list_projects", json!({})).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (_, _, log) = d.http(OPERATOR, Method::GET, "/v1/access/audit?source=mcp", None).await;
    assert!(log.as_array().unwrap().iter().any(|e| e["grant_id"].is_null() && e["tool"] == "postgres_list_projects"), "the operator's calls too: {log}");
    let tools: Vec<_> = log.as_array().unwrap().iter().map(|e| e["tool"].as_str().unwrap_or("")).collect();
    assert!(tools.contains(&"postgres_delete_project") && tools.contains(&"postgres_list_projects"), "{log}");
    let deletes: Vec<_> = log.as_array().unwrap().iter().filter(|e| e["tool"] == "postgres_delete_project").map(|e| e["decision"].as_str().unwrap()).collect();
    assert!(deletes.contains(&"deny") && deletes.contains(&"ask"), "{log}");
}

/// What `tools/list` costs with every product on, by hand in release:
/// `cargo test --release -p roll-mcp --test mcp tools_list_cost -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn tools_list_cost() {
    let d = Daemon::with_products(roll_auth::scope::PRODUCTS.iter().map(|p| p.to_string()).collect());
    let mut lat = Vec::new();
    for i in 0..600 {
        let start = std::time::Instant::now();
        let list = d.rpc(OPERATOR, "tools/list", json!({})).await;
        if i == 0 {
            println!("{} tools, {} bytes", list["tools"].as_array().unwrap().len(), list.to_string().len());
        }
        if i >= 100 {
            lat.push(start.elapsed().as_micros() as u64);
        }
    }
    lat.sort_unstable();
    println!("tools/list p50 {} µs  p99 {} µs", lat[lat.len() / 2], lat[lat.len() * 99 / 100]);
}

/// A prompt's arguments complete from what the connection may list (MCP `completion/complete`,
/// read through the gate): the operator gets matching projects; a token without the prompt's
/// tools gets nothing.
#[tokio::test(flavor = "multi_thread")]
async fn prompt_arguments_complete_from_what_it_may_read() {
    let d = Daemon::with_products(vec!["postgres".into(), "secrets".into(), "workspace".into()]);
    for name in ["billing", "blog", "api"] {
        let (s, _, _) = d.http(OPERATOR, Method::POST, "/v1/workspace/projects", Some(json!({ "name": name }))).await;
        assert_eq!(s, StatusCode::CREATED);
    }
    let ask = |typed: &str| json!({ "ref": { "type": "ref/prompt", "name": "fix_secrets_health" }, "argument": { "name": "project", "value": typed } });
    let got = d.rpc(OPERATOR, "completion/complete", ask("b")).await;
    let mut values: Vec<&str> = got["completion"]["values"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    values.retain(|v| !v.starts_with("prj_"));
    values.sort();
    assert_eq!(values, vec!["billing", "blog"], "{got}");
    // A Postgres project completes from Postgres's own list, not the workspace's.
    let pg = d.rpc(OPERATOR, "completion/complete", json!({ "ref": { "type": "ref/prompt", "name": "try_migration_on_branch" }, "argument": { "name": "project", "value": "" } })).await;
    assert_eq!(pg["completion"]["values"], json!(["pa"]), "{pg}");
    let reader = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }])).await;
    let none = d.rpc(&reader, "completion/complete", ask("")).await;
    assert_eq!(none["completion"]["values"], json!([]), "{none}");
}
