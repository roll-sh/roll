//! Scoped tokens end to end in process (docs/permissions.md): rolld's gate with the evaluator in
//! front of `/v1/access`, the workspace and a stand-in for Postgres's routes, on SQLite.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::routing::{delete, get, post};
use http_body_util::BodyExt;
use roll_access::{Access, Config, Resolved, Resolver, Resolvers, WorkspaceResolver};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

const OPERATOR: &str = "operator-token-0123456789abcdef-0123";

/// Postgres's endpoints belong to its projects: `ep_<project>`.
struct Endpoints;

impl Resolver for Endpoints {
    fn resolve(&self, _kind: &str, id: &str) -> Option<Resolved> {
        id.strip_prefix("ep_").map(|p| Resolved { id: p.to_string(), environment: None })
    }
}

/// Every repo's default branch is `main`.
struct DefaultBranch;

impl Resolver for DefaultBranch {
    fn resolve(&self, _kind: &str, _repo: &str) -> Option<Resolved> {
        Some(Resolved { id: "main".into(), environment: None })
    }
}

/// Signed-in people: a cookie `rls_<user>` is that user's session; a user taken out is disabled.
/// The people accounts knows, what they've been mailed (`user: subject`), and its SSO connections.
#[derive(Default)]
struct People(std::sync::Mutex<std::collections::HashMap<String, roll_auth::SignedIn>>, std::sync::Mutex<Vec<String>>, std::sync::Mutex<std::collections::HashMap<String, roll_auth::SsoTrust>>, std::sync::Mutex<std::collections::HashSet<String>>);

impl People {
    fn add(&self, user: &str, handle: &str) {
        let p = roll_auth::SignedIn { user: user.into(), handle: handle.into(), admin: false, code_owner: None, session: String::new() };
        self.0.lock().unwrap().insert(user.into(), p);
    }
    fn remove(&self, user: &str) {
        self.0.lock().unwrap().remove(user);
    }
    /// An SSO connection `id` for `issuer` with these proven domains (`None`: removed).
    fn sso(&self, id: &str, at: Option<(&str, &[&str])>) {
        let mut all = self.2.lock().unwrap();
        match at {
            Some((issuer, verified)) => all.insert(id.into(), roll_auth::SsoTrust { name: "Okta".into(), issuer: issuer.into(), verified: verified.iter().map(|d| d.to_string()).collect() }),
            None => all.remove(id),
        };
    }
    fn set_admin(&self, user: &str, admin: bool) {
        if let Some(p) = self.0.lock().unwrap().get_mut(user) {
            p.admin = admin;
        }
    }
}

impl roll_auth::Sessions for People {
    fn cookie(&self) -> &str {
        "roll_session"
    }
    fn resolve<'a>(&'a self, token: &'a str) -> roll_auth::BoxFuture<'a, Result<Option<roll_auth::Session>, String>> {
        let p = token.strip_prefix("rls_").and_then(|u| self.0.lock().unwrap().get(u).cloned());
        Box::pin(async move { Ok(p.map(|p| roll_auth::Session::Full(roll_auth::SignedIn { session: "ses_1".into(), ..p }))) })
    }
    fn person<'a>(&'a self, user: &'a str) -> roll_auth::BoxFuture<'a, Result<Option<roll_auth::SignedIn>, String>> {
        let p = self.0.lock().unwrap().get(user).cloned();
        Box::pin(async move { Ok(p) })
    }
    fn person_now(&self, user: &str) -> Result<Option<roll_auth::SignedIn>, String> {
        Ok(self.0.lock().unwrap().get(user).cloned())
    }
    /// Everyone's address is `<handle>@acme.test`.
    fn person_by_email<'a>(&'a self, email: &'a str) -> roll_auth::BoxFuture<'a, Result<Option<roll_auth::SignedIn>, String>> {
        let p = self.0.lock().unwrap().values().find(|p| format!("{}@acme.test", p.handle) == email.to_lowercase()).cloned();
        Box::pin(async move { Ok(p) })
    }
    fn sso_trust<'a>(&'a self, id: &'a str) -> roll_auth::BoxFuture<'a, Result<Option<roll_auth::SsoTrust>, String>> {
        let t = self.2.lock().unwrap().get(id).cloned();
        Box::pin(async move { Ok(t) })
    }
    /// Fresh unless the test made the person's sessions stale.
    fn fresh<'a>(&'a self, user: &'a str, _session: &'a str) -> roll_auth::BoxFuture<'a, Result<bool, String>> {
        let stale = self.3.lock().unwrap().contains(user);
        Box::pin(async move { Ok(!stale) })
    }
    fn admins(&self) -> roll_auth::BoxFuture<'_, Result<Vec<String>, String>> {
        Box::pin(async { Ok(vec!["usr_root".to_string()]) })
    }

    fn tell<'a>(&'a self, user: &'a str, subject: &'a str, _text: &'a str) -> roll_auth::BoxFuture<'a, Result<(), String>> {
        self.1.lock().unwrap().push(format!("{user}: {subject}"));
        Box::pin(async { Ok(()) })
    }
}

struct Daemon {
    router: Router,
    access: Arc<Access>,
    people: Arc<People>,
    workspace: Arc<roll_workspace::Workspace>,
    _dir: tempfile::TempDir,
}

/// What the stand-in products saw: the operator's marker and a grant.
fn seen(ext: &axum::http::Extensions) -> Value {
    let approved = roll_auth::Approved::on(ext).map(|a| json!({ "id": &*a.id, "by": a.by.iter().map(|b| &**b).collect::<Vec<_>>(), "until": a.until }));
    json!({ "operator": roll_auth::Operator::on(ext), "grant": roll_auth::Grant::on(ext).map(|g| g.id.to_string()), "approved": approved })
}

fn postgres() -> Router {
    Router::new()
        .route(
            "/v1/projects",
            get(|| async {
                axum::Json(json!([
                    { "id": "pa", "branches": [{ "connection_uri": "postgres://roll:secret@db:5432/main" }] },
                    { "id": "pb", "branches": [{ "connection_uri": "postgres://roll:other@db:5432/main" }] },
                ]))
            })
            .post(|| async { (StatusCode::CREATED, axum::Json(json!({ "id": "pnew", "password": "fresh" }))) }),
        )
        .route("/v1/projects/{id}", delete(|ext: axum::http::Extensions| async move { axum::Json(seen(&ext)) }))
        .route("/v1/endpoints/{id}/query", post(|body: String| async move { axum::Json(json!({ "ran": body })) }))
        .route("/v1/queues/overview", get(|| async { "{}" }))
        .route("/v1/unclassified", get(|| async { "{}" }))
        .route("/v1/open", get(|ext: axum::http::Extensions| async move { axum::Json(seen(&ext)) }))
        .route("/internal/tasks", axum::routing::any(|ext: axum::http::Extensions| async move { axum::Json(seen(&ext)) }))
        .route("/git/{owner}/{repo}/git-receive-pack", post(|ext: axum::http::Extensions| async move { axum::Json(seen(&ext)) }))
        .route("/v1/storage/buckets/{name}", get(|| async { axum::Json(json!({ "name": "logs" })) }))
        .route("/v1/storage/buckets", get(|| async { axum::Json(json!([{ "name": "logs" }, { "name": "media" }])) }))
        .route("/v1/apps/services", post(|| async { axum::Json(json!({ "id": "svc_new" })) }))
        .route("/v1/apps/plan", post(|| async { axum::Json(json!({ "builder": "node" })) }))
        .route("/v1/apps/services/{id}", axum::routing::patch(|| async { axum::Json(json!({ "ok": true })) }))
        .route("/v1/apps/services/{id}/links/{kind}/{ref}", axum::routing::put(|| async { axum::Json(json!({ "ok": true })) }))
        .route("/v1/apps/services/{id}/environments/{env}", axum::routing::put(|| async { axum::Json(json!({ "ok": true })) }))
        .route("/v1/apps/services/{id}/environments/{env}/redeploy", post(|| async { axum::Json(json!({ "ok": true })) }))
        .route("/v1/search/clusters/{id}/indices/{index}/search", post(|| async { axum::Json(json!({ "hits": [] })) }))
        .route("/v1/search/clusters", post(|| async { axum::Json(json!({ "id": "cl_new", "password": "fresh" })) }))
        .route("/v1/search/clusters/{id}/indices", get(|| async { axum::Json(json!([])) }))
        .route("/v1/search/clusters/{id}/request", post(|| async { axum::Json(json!({})) }))
        .route("/v1/search/clusters/{id}/indices/{index}/bulk", post(|body: String| async move { axum::Json(json!({ "lines": body.lines().count() })) }))
        .route("/v1/secrets/projects/{project}/environments/{env}/secrets", get(|| async { axum::Json(json!([])) }))
        .route("/v1/secrets/projects/{project}/health", get(|| async { axum::Json(json!({ "findings": [] })) }))
        .route("/v1/secrets/projects/{project}/transit", get(|| async { axum::Json(json!([])) }))
        .route("/v1/secrets/projects/{project}/transit/{name}/encrypt", axum::routing::post(|| async { axum::Json(json!({ "ciphertext": "roll:v1:x" })) }))
        .route("/v1/secrets/projects/{project}/environments/{env}/values", get(|| async { axum::Json(json!({})) }))
        .route("/v1/secrets/projects/{project}/environments/{env}/promote", axum::routing::post(|| async { axum::Json(json!({})) }))
        .route("/v1/secrets/projects/{project}/environments/{env}/imports", axum::routing::put(|| async { axum::Json(json!({})) }))
}

/// A test daemon's database: Postgres when `ROLL_CODE_TEST_DATABASE` says so, else SQLite with one
/// reader, not production's four. Every test runs a daemon with two of these at once, so five
/// connections each ran a gate inside a systemd user unit (1,024 open files) out of descriptors.
fn test_db(path: std::path::PathBuf, name: &str) -> code_sql::Db {
    if std::env::var(code_sql::TEST_DATABASE_ENV).is_ok_and(|u| !u.is_empty()) {
        return code_sql::Db::open_or_test(&code_sql::Target::Sqlite(path), name).unwrap();
    }
    code_sql::Db::open_sqlite(&path, code_sql::SqliteOptions { readers: 1, cache_kib: 2048 }).unwrap()
}

impl Daemon {
    fn new(wait_ms: u64) -> Daemon {
        Daemon::with(wait_ms, |_| {})
    }

    fn with(wait_ms: u64, configure: impl FnOnce(&mut Config)) -> Daemon {
        let dir = tempfile::tempdir().unwrap();
        let ws_db = test_db(dir.path().join("workspace.db"), "workspace");
        let workspace = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
        let mut resolvers = Resolvers::default();
        resolvers.by_kind.insert("endpoint", Arc::new(Endpoints));
        resolvers.by_kind.insert("default-branch", Arc::new(DefaultBranch));
        resolvers.workspace = Some(Arc::new(WorkspaceResolver(workspace.clone())));
        // Metadata documents from 127.0.0.1 (the test's own server) are allowed here only.
        let mut config = Config { locked: true, approval_wait: std::time::Duration::from_millis(wait_ms), metadata_allow_private: true, ..Config::default() };
        configure(&mut config);
        let access = Access::with_db(test_db(dir.path().join("access.db"), "access"), config, resolvers).unwrap();
        let rules = vec![
            roll_auth::Rule { route: "/v1/open", access: roll_auth::Access::Public("a shared link") },
            roll_auth::Rule { route: "/v1/workspace", access: roll_auth::Access::User("a user's own projects") },
            roll_auth::Rule { route: "/v1/access", access: roll_auth::Access::User("a user's own tokens") },
            roll_auth::Rule { route: "/internal", access: roll_auth::Access::Product("signed hops") },
        ];
        let people = Arc::new(People::default());
        let _ = access.people.set(people.clone());
        let gate = roll_auth::Gate::with_sessions(Some(roll_auth::AdminToken::new(OPERATOR).unwrap()), rules, people.clone(), roll_auth::Origins::Local, "http://localhost:3000");
        gate.set_authorizer(Arc::new(roll_access::evaluate::Evaluator(access.clone())));
        let gated = postgres()
            .merge(roll_workspace::router(workspace.clone()))
            .merge(roll_access::api::router(access.clone()))
            .layer(axum::middleware::from_fn_with_state(gate, roll_auth::check));
        let router = roll_access::oauth::public_router(access.clone()).merge(gated);
        let access = access.clone();
        Daemon { router, access, people, workspace, _dir: dir }
    }

    /// The audit log at `path` once `until` holds of it (rows go through a batching writer), at
    /// most 5 s.
    async fn audit_until(&self, path: &str, until: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..50 {
            let (_, log) = self.call(OPERATOR, Method::GET, path, None).await;
            if until(&log) {
                return log;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        self.call(OPERATOR, Method::GET, path, None).await.1
    }

    async fn call(&self, token: &str, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.send(token, method, path, body, &[]).await
    }

    async fn send(&self, token: &str, method: Method, path: &str, body: Option<Value>, headers: &[(&str, &str)]) -> (StatusCode, Value) {
        let body = body.map_or(Body::empty(), |b| Body::from(b.to_string()));
        self.raw(token, method, path, body, headers, None).await
    }

    /// As a signed-in user (their session cookie, and the CSRF header a write needs).
    async fn as_user(&self, user: &str, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let token = format!("rls_{user}");
        let (cookie, csrf) = (format!("roll_session={token}"), roll_auth::csrf_token(&token));
        let body = body.map_or(Body::empty(), |b| Body::from(b.to_string()));
        self.raw("", method, path, body, &[("cookie", &cookie), ("x-roll-csrf", &csrf)], None).await
    }

    /// A request with any body, and (a signed hop from another node) a `Forwarded` extension.
    /// A GET's answer headers.
    async fn head_of(&self, token: &str, path: &str) -> axum::http::HeaderMap {
        let req = Request::builder().method(Method::GET).uri(path).header("authorization", format!("Bearer {token}")).body(Body::empty()).unwrap();
        self.router.clone().oneshot(req).await.unwrap().headers().clone()
    }

    async fn raw(&self, token: &str, method: Method, path: &str, body: Body, headers: &[(&str, &str)], hop: Option<roll_cluster::Forwarded>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path).header("content-type", "application/json");
        if !token.is_empty() {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let mut req = req.body(body).unwrap();
        if let Some(f) = hop {
            req.extensions_mut().insert(f);
        }
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into())))
    }

    /// A request roll's MCP server makes for a tool call (its `McpCall` extension), with `token`.
    async fn via_mcp(&self, token: &str, method: Method, path: &str) -> StatusCode {
        let mut req = Request::builder().method(method).uri(path).header("authorization", format!("Bearer {token}")).body(Body::empty()).unwrap();
        req.extensions_mut().insert(roll_access::evaluate::McpCall { tool: "test_tool".into(), args: Arc::new(json!({})), ip: None, elicits: false, client: None, quiet: false });
        self.router.clone().oneshot(req).await.unwrap().status()
    }

    async fn form(&self, path: &str, form: &[(&str, &str)]) -> (StatusCode, Value) {
        let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(form).finish();
        let req = Request::builder().method(Method::POST).uri(path).header("content-type", "application/x-www-form-urlencoded").body(Body::from(body)).unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn token(&self, rules: Value, extra: Value) -> (String, String) {
        let mut body = json!({ "name": "agent", "rules": rules });
        body.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap_or_default());
        let (s, out) = self.call(OPERATOR, Method::POST, "/v1/access/tokens", Some(body)).await;
        assert_eq!(s, StatusCode::CREATED, "{out}");
        (out["token"].as_str().unwrap().to_string(), out["grant"]["id"].as_str().unwrap().to_string())
    }
}

fn ids(list: &Value) -> Vec<&str> {
    list.as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect()
}

/// A token sees and does exactly what its rules allow: lists filtered to its project, credentials
/// hidden without `reveal`, other actions and products refused, routes nobody classified the
/// operator's, the wrong address refused, read-only SQL decided by its body, a rotated token's old
/// secret and a revoked token dead on their next request.
#[tokio::test]
async fn a_scoped_token_does_only_what_its_rules_say() {
    let d = Daemon::new(50);
    let project = d.workspace.create("", "billing", "", None).unwrap();
    d.workspace.place(roll_workspace::Scope::All, &project.id, roll_workspace::Kind::Postgres, "pa", 0.0, 0.0).unwrap();
    let (token, grant) = d.token(json!([{ "product": "postgres", "resources": [{ "project": project.id }], "actions": { "read": "allow" } }]), json!({})).await;
    assert!(token.starts_with("rlt_"));

    let (s, list) = d.call(&token, Method::GET, "/v1/projects", None).await;
    assert_eq!((s, ids(&list)), (StatusCode::OK, vec!["pa"]));
    assert_eq!(list[0]["branches"][0]["connection_uri"], "postgres://roll:redacted@db:5432/main");
    // Reveal on one database, read on all: the list shows only that one's credentials (a list is
    // judged item by item, not as a whole).
    let (some, _) = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }, { "product": "postgres", "resources": [{ "id": "pa" }], "actions": { "reveal": "allow" } }]), json!({})).await;
    let (_, both) = d.call(&some, Method::GET, "/v1/projects", None).await;
    let uri = |id: &str| both.as_array().unwrap().iter().find(|p| p["id"] == id).unwrap()["branches"][0]["connection_uri"].clone();
    assert_eq!((uri("pa"), uri("pb")), (json!("postgres://roll:secret@db:5432/main"), json!("postgres://roll:redacted@db:5432/main")), "{both}");

    let (s, e) = d.call(&token, Method::DELETE, "/v1/projects/pa", None).await;
    assert_eq!((s, e["code"].as_str(), e["needs"]["action"].as_str()), (StatusCode::FORBIDDEN, Some("forbidden"), Some("delete")));
    assert_eq!(d.call(&token, Method::GET, "/v1/workspace/projects", None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(d.call(&token, Method::GET, "/v1/queues/overview", None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(d.call(&token, Method::GET, "/v1/unclassified", None).await.1["code"], "operator_only");
    // Read-only SQL is a read; SQL that may write is a write.
    let (s, _) = d.call(&token, Method::POST, "/v1/endpoints/ep_pa/query", Some(json!({ "sql": "select 1", "read_only": true }))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(d.call(&token, Method::POST, "/v1/endpoints/ep_pa/query", Some(json!({ "sql": "drop table t" }))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(d.call(&token, Method::POST, "/v1/endpoints/ep_pb/query", Some(json!({ "sql": "select 1", "read_only": true }))).await.0, StatusCode::FORBIDDEN);

    // A signed hop to the tasks leader is decided as the request it carries.
    let hop = d.send(&token, Method::GET, "/internal/tasks", None, &[("x-roll-tasks-uri", "/v1/tasks/queues")]).await;
    assert_eq!(hop.1["needs"]["product"], "tasks", "{}", hop.1);
    // A tasks token for one queue reaches that queue, not another, nor a topic of its name.
    let (queue, _) = d.token(json!([{ "product": "tasks", "resources": [{ "id": "queue:emails" }], "actions": { "read": "allow" } }]), json!({})).await;
    for (uri, status) in [("/v1/tasks/queues/emails", StatusCode::OK), ("/v1/tasks/queues/billing", StatusCode::FORBIDDEN), ("/v1/tasks/topics/emails", StatusCode::FORBIDDEN)] {
        assert_eq!(d.send(&queue, Method::GET, "/internal/tasks", None, &[("x-roll-tasks-uri", uri)]).await.0, status, "{uri}");
    }

    // A public route is public for it too, as anyone (no grant, not the operator).
    let (s, open) = d.call(&token, Method::GET, "/v1/open", None).await;
    assert_eq!((s, open["operator"].as_bool(), open["grant"].is_null()), (StatusCode::OK, Some(false), true));

    // It can ask what it may do without doing it.
    let (_, c) = d.call(&token, Method::POST, "/v1/access/check", Some(json!({ "method": "DELETE", "path": "/v1/projects/pa" }))).await;
    assert_eq!((c["decision"].as_str(), c["action"].as_str()), (Some("deny"), Some("delete")), "{c}");

    // The operator's routes stay the operator's, and the token's own info is its own.
    let (s, me) = d.call(&token, Method::GET, "/v1/access", None).await;
    assert_eq!((s, me["grant"]["id"].as_str()), (StatusCode::OK, Some(grant.as_str())));
    assert_eq!(d.call(&token, Method::GET, "/v1/access/tokens", None).await.0, StatusCode::FORBIDDEN);

    // Moving the database out of the project takes it out of reach at once.
    d.workspace.remove(roll_workspace::Scope::All, &project.id, roll_workspace::Kind::Postgres, "pa").unwrap();
    let (_, list) = d.call(&token, Method::GET, "/v1/projects", None).await;
    assert_eq!(ids(&list), Vec::<&str>::new());

    // An address outside the allowlist (an in-process request has none) is refused.
    // A search bulk is decided by its lines: indexing is write, a delete line needs delete; the body
    // goes on untouched.
    let (indexer, _) = d.token(json!([{ "product": "search", "resources": "*", "actions": { "write": "allow" } }]), json!({})).await;
    let bulk = |ndjson: &'static str| d.raw(&indexer, Method::POST, "/v1/search/clusters/c1/indices/logs/bulk", Body::from(ndjson), &[], None);
    assert_eq!(bulk("{\"index\":{}}\n{\"msg\":\"hi\"}\n").await, (StatusCode::OK, json!({ "lines": 2 })));
    let (s, e) = bulk("{\"index\":{}}\n{\"msg\":\"hi\"}\n{\"delete\":{\"_id\":\"1\"}}\n").await;
    assert_eq!((s, e["needs"]["action"].as_str()), (StatusCode::FORBIDDEN, Some("delete")));

    // A bucket in another storage account is `<account>/<bucket>`: a rule for one doesn't reach
    // the same name in the default account or in a third.
    let (bucket, _) = d.token(json!([{ "product": "storage", "resources": [{ "id": "acme/logs" }], "actions": { "read": "allow" } }]), json!({})).await;
    assert_eq!(d.send(&bucket, Method::GET, "/v1/storage/buckets/logs", None, &[("x-roll-account", "acme")]).await.0, StatusCode::OK);
    assert_eq!(d.send(&bucket, Method::GET, "/v1/storage/buckets/logs", None, &[("x-roll-account", "other")]).await.0, StatusCode::FORBIDDEN);
    assert_eq!(d.call(&bucket, Method::GET, "/v1/storage/buckets/logs", None).await.0, StatusCode::FORBIDDEN);
    let (s, list) = d.send(&bucket, Method::GET, "/v1/storage/buckets", None, &[("x-roll-account", "acme")]).await;
    assert_eq!((s, list), (StatusCode::OK, json!([{ "name": "logs" }])), "the account's list, filtered to its bucket");

    let (fenced, _) = d.token(json!([{ "product": "*", "resources": "*", "actions": { "read": "allow" } }]), json!({ "ip_allow": ["10.0.0.0/8"] })).await;
    assert_eq!(d.call(&fenced, Method::GET, "/v1/projects", None).await.1["code"], "ip_not_allowed");

    // A token limited to 2 requests a minute: told how many are left, then 429 with when to retry.
    let (slow, _) = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }]), json!({ "rate_per_minute": 2 })).await;
    let get = || async {
        let req = Request::get("/v1/projects").header("authorization", format!("Bearer {slow}")).body(Body::empty()).unwrap();
        let resp = d.router.clone().oneshot(req).await.unwrap();
        let h = |n: &str| resp.headers().get(n).map(|v| v.to_str().unwrap().to_string());
        (resp.status(), h("ratelimit-remaining"), h("retry-after"))
    };
    assert_eq!(get().await, (StatusCode::OK, Some("1".into()), None));
    assert_eq!(get().await, (StatusCode::OK, Some("0".into()), None));
    assert_eq!(get().await, (StatusCode::TOO_MANY_REQUESTS, Some("0".into()), Some("30".into())));
    // On three nodes each enforces a third of the rate, so the fleet allows about the rate.
    let _ = d.access.nodes.set(Box::new(|| 3));
    let (six, _) = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }]), json!({ "rate_per_minute": 6 })).await;
    let mut codes = Vec::new();
    for _ in 0..3 {
        codes.push(d.call(&six, Method::GET, "/v1/projects", None).await.0);
    }
    assert_eq!(codes, [StatusCode::OK, StatusCode::OK, StatusCode::TOO_MANY_REQUESTS], "two here: this node's share of six");

    // Secrets are per project and environment: a rule for the project's development environment
    // reaches those and only those, names but not values.
    let (secrets, _) = d
        .token(json!([{ "product": "secrets", "resources": [{ "project": project.id }], "environments": ["development"], "actions": { "read": "allow" } }]), json!({}))
        .await;
    let at = |env: &str, rest: &str| format!("/v1/secrets/projects/{}/environments/{env}/{rest}", project.id);
    assert_eq!(d.call(&secrets, Method::GET, &at("development", "secrets"), None).await.0, StatusCode::OK);
    assert_eq!(d.call(&secrets, Method::GET, &at("production", "secrets"), None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(d.call(&secrets, Method::GET, &at("development", "values"), None).await.0, StatusCode::FORBIDDEN);
    // Health spans every environment and filters itself by the token's grant (roll-secrets' tests
    // check that it hears only of development): the gate lets it in.
    assert_eq!(d.call(&secrets, Method::GET, &format!("/v1/secrets/projects/{}/health", project.id), None).await.0, StatusCode::OK);

    // Rotated: the old secret is dead at once, the new one has the same rules.
    let (s, rotated) = d.call(OPERATOR, Method::POST, &format!("/v1/access/tokens/{grant}/rotate"), None).await;
    assert_eq!(s, StatusCode::OK, "{rotated}");
    assert_eq!(d.call(&token, Method::GET, "/v1/projects", None).await.0, StatusCode::UNAUTHORIZED);
    let token = rotated["token"].as_str().unwrap().to_string();
    assert_eq!(d.call(&token, Method::GET, "/v1/projects", None).await.0, StatusCode::OK);
    // Rotated with a grace (Stripe's roll key): both work until it ends; rotated again without one,
    // the old ones stop at once. At most a week.
    let (_, graced) = d.call(OPERATOR, Method::POST, &format!("/v1/access/tokens/{grant}/rotate"), Some(json!({ "keep_old_for_s": 3600 }))).await;
    let newer = graced["token"].as_str().unwrap().to_string();
    assert_eq!((d.call(&token, Method::GET, "/v1/projects", None).await.0, d.call(&newer, Method::GET, "/v1/projects", None).await.0), (StatusCode::OK, StatusCode::OK));
    // The old one, in its grace, calls products but manages no tokens (it may have leaked).
    let (s, e) = d.call(&token, Method::POST, &format!("/v1/access/tokens/{grant}/rotate"), None).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::FORBIDDEN, Some("rotated_secret")), "{e}");
    assert_eq!(d.call(&token, Method::POST, "/v1/access/tokens", Some(json!({ "name": "child", "rules": [] }))).await.0, StatusCode::FORBIDDEN);
    let (_, plain) = d.call(OPERATOR, Method::POST, &format!("/v1/access/tokens/{grant}/rotate"), None).await;
    assert_eq!((d.call(&token, Method::GET, "/v1/projects", None).await.0, d.call(&newer, Method::GET, "/v1/projects", None).await.0), (StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED));
    let token = plain["token"].as_str().unwrap().to_string();
    assert_eq!(d.call(OPERATOR, Method::POST, &format!("/v1/access/tokens/{grant}/rotate"), Some(json!({ "keep_old_for_s": 604_801 }))).await.0, StatusCode::BAD_REQUEST);

    // Revoked: dead on the next request; the audit log has every decision.
    assert_eq!(d.call(OPERATOR, Method::DELETE, &format!("/v1/access/tokens/{grant}"), None).await.0, StatusCode::OK);
    assert_eq!(d.call(&token, Method::GET, "/v1/projects", None).await.0, StatusCode::UNAUTHORIZED);
    let has = |l: &Value, d: &str| l.as_array().is_some_and(|a| a.iter().any(|e| e["decision"] == d));
    let log = d.audit_until(&format!("/v1/access/audit?grant={grant}"), |l| has(l, "allow") && has(l, "deny")).await;
    let decisions: Vec<_> = log.as_array().unwrap().iter().map(|e| e["decision"].as_str().unwrap()).collect();
    assert!(decisions.contains(&"allow") && decisions.contains(&"deny"), "{log}");
    assert!(log.as_array().unwrap().iter().all(|e| e["route"].as_str().unwrap().starts_with('/')), "routes, not paths: {log}");
    // The list says what each token did lately.
    let (_, grants) = d.call(OPERATOR, Method::GET, "/v1/access/tokens", None).await;
    let mine = grants.as_array().unwrap().iter().find(|g| g["id"] == grant.as_str()).unwrap();
    assert!(mine["activity"]["allowed"].as_i64().unwrap() > 0 && mine["activity"]["denied"].as_i64().unwrap() > 0, "{mine}");
}

/// The ways around a token's rules stay shut: pulling another resource into its project, a list
/// with an `ask` rule beside a narrower `allow`, and an approval reused with different headers.
#[tokio::test]
async fn escalation_paths_are_shut() {
    let d = Daemon::new(50);
    let a = d.workspace.create("", "a", "", None).unwrap();
    d.workspace.place(roll_workspace::Scope::All, &a.id, roll_workspace::Kind::Postgres, "pa", 0.0, 0.0).unwrap();
    // "This project only": write everything in project a, including the workspace's own routes.
    let (scoped, _) = d.token(json!([{ "product": "*", "resources": [{ "project": a.id }], "actions": { "read": "allow", "write": "allow" } }]), json!({})).await;
    let pull = format!("/v1/workspace/projects/{}/resources/postgres/pb", a.id);
    assert_eq!(d.call(&scoped, Method::PUT, &pull, Some(json!({ "x": 0, "y": 0 }))).await.0, StatusCode::FORBIDDEN, "pulled pb into its project");
    assert!(d.workspace.projects_of("postgres", "pb").unwrap().is_empty());
    // With admin on Postgres everywhere it may move it.
    let (admin, _) = d
        .token(json!([{ "product": "*", "resources": [{ "project": a.id }], "actions": { "write": "allow" } }, { "product": "postgres", "resources": "*", "actions": { "admin": "allow" } }]), json!({}))
        .await;
    assert_eq!(d.call(&admin, Method::PUT, &pull, Some(json!({ "x": 0, "y": 0 }))).await.0, StatusCode::OK);
    d.workspace.remove(roll_workspace::Scope::All, &a.id, roll_workspace::Kind::Postgres, "pb").unwrap();

    // Each user places resources in their own projects: a token for one user's project reaches
    // what's in it, even when the operator's project has it too.
    let mine = d.workspace.create("usr_alice", "mine", "", None).unwrap();
    d.workspace.place(roll_workspace::Scope::All, &mine.id, roll_workspace::Kind::Postgres, "pa", 0.0, 0.0).unwrap();
    let (alice, _) = d.token(json!([{ "product": "postgres", "resources": [{ "project": mine.id }], "actions": { "read": "allow" } }]), json!({})).await;
    let (s, list) = d.call(&alice, Method::GET, "/v1/projects", None).await;
    assert_eq!((s, ids(&list)), (StatusCode::OK, vec!["pa"]));

    // A narrower allow doesn't turn an ask for every project into a free full list.
    let (mixed, _) = d
        .token(json!([{ "product": "postgres", "resources": [{ "project": a.id }], "actions": { "read": "allow" } }, { "product": "postgres", "resources": "*", "actions": { "read": "ask" } }]), json!({}))
        .await;
    let (s, list) = d.call(&mixed, Method::GET, "/v1/projects", None).await;
    assert_eq!((s, ids(&list)), (StatusCode::OK, vec!["pa"]));

    // An approval runs the request it was asked for, not the same path with other headers.
    let (asker, _) = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "delete": "ask" } }]), json!({})).await;
    let (_, pending) = d.call(&asker, Method::DELETE, "/v1/projects/pa", None).await;
    let id = pending["approval"].as_str().unwrap().to_string();
    d.call(OPERATOR, Method::POST, &format!("/v1/access/approvals/{id}/approve"), None).await;
    let other = d.send(&asker, Method::DELETE, "/v1/projects/pa", None, &[("x-roll-approval", &id), ("x-roll-project", &a.id)]).await;
    assert_eq!(other.1["code"], "approval_mismatch");
    assert_eq!(d.send(&asker, Method::DELETE, "/v1/projects/pa", None, &[("x-roll-approval", &id)]).await.0, StatusCode::OK);

    // A push needs every action its ref updates take; refs/land/main lands on main: a deploy.
    let pkt = |s: String| format!("{:04x}{s}", s.len() + 4);
    let (z, c) = ("0".repeat(40), "c".repeat(40));
    let push = |updates: &[String]| Body::from(format!("{}0000PACK", updates.iter().map(|u| pkt(u.clone())).collect::<String>()));
    let git = "/git/acme/site/git-receive-pack";
    let code = |actions: Value| d.token(json!([{ "product": "code", "resources": [{ "id": "acme/site" }], "actions": actions }]), json!({}));
    let (writer, _) = code(json!({ "write": "allow" })).await;
    let (s, e) = d.raw(&writer, Method::POST, git, push(&[format!("{z} {c} refs/land/main\0report-status\n")]), &[], None).await;
    assert_eq!((s, e["needs"]["action"].as_str()), (StatusCode::FORBIDDEN, Some("deploy")), "{e}");
    let (cleaner, _) = code(json!({ "delete": "allow" })).await;
    let mixed = || push(&[format!("{c} {z} refs/heads/old\0report-status\n"), format!("{z} {c} refs/heads/new\n")]);
    let (s, e) = d.raw(&cleaner, Method::POST, git, mixed(), &[], None).await;
    assert_eq!((s, e["needs"]["action"].as_str()), (StatusCode::FORBIDDEN, Some("write")), "a delete doesn't carry a new branch in: {e}");
    let (both, _) = code(json!({ "write": "allow", "delete": "ask" })).await;
    let (s, e) = d.raw(&both, Method::POST, git, mixed(), &[], None).await;
    assert_eq!(e["code"], "approval_pending", "{s} {e}");
    let (_, asked) = d.call(OPERATOR, Method::GET, &format!("/v1/access/approvals/{}", e["approval"].as_str().unwrap()), None).await;
    assert_eq!(asked["args"]["updates"][0]["ref"], "refs/heads/old", "the approval shows the ref updates: {asked}");
    // The same push again, its pack different (repacked, or split otherwise), finds that approval.
    let repacked = push(&[format!("{c} {z} refs/heads/old\0report-status\n"), format!("{z} {c} refs/heads/new\n")]);
    let repacked = Body::from(format!("{}MORE", String::from_utf8(axum::body::to_bytes(repacked, 1 << 20).await.unwrap().to_vec()).unwrap()));
    let (_, again) = d.raw(&both, Method::POST, git, repacked, &[], None).await;
    assert_eq!(again["approval"], e["approval"], "{again}");
    assert_eq!(d.raw(&cleaner, Method::POST, git, Body::from("garbage"), &[], None).await.0, StatusCode::FORBIDDEN);

    // A signed hop from a node that decided it for this grant: no second approval, and the
    // client's address is the one that node saw. A hop that names another grant is decided again.
    let (fleet, grant) = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "delete": "ask" } }]), json!({ "ip_allow": ["10.1.2.0/24"] })).await;
    let hop = |decided: &str| Some(roll_cluster::Forwarded { node: "n1".into(), hops: 1, client: Some("10.1.2.3".into()), decided: Some(decided.into()) });
    let (s, out) = d.raw(&fleet, Method::DELETE, "/v1/projects/pa", Body::empty(), &[], hop(&grant)).await;
    assert_eq!((s, out["grant"].as_str()), (StatusCode::OK, Some(grant.as_str())), "{out}");
    let (_, out) = d.raw(&fleet, Method::DELETE, "/v1/projects/pa", Body::empty(), &[], hop("tok_other")).await;
    assert_eq!(out["code"], "approval_pending", "{out}");
    let stranger = Some(roll_cluster::Forwarded { node: "n1".into(), hops: 1, client: Some("192.0.2.9".into()), decided: Some(grant.clone()) });
    let (_, out) = d.raw(&fleet, Method::DELETE, "/v1/projects/pa", Body::empty(), &[], stranger).await;
    assert_eq!(out["code"], "ip_not_allowed", "{out}");
}

/// An `ask` action waits for a person: approved, the request runs as sent (once); denied, it's
/// refused; undecided, the caller gets the approval and can send the request again with it.
#[tokio::test]
async fn ask_waits_for_a_person() {
    let d = Daemon::new(3000);
    let (token, _) = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "delete": "ask" } }]), json!({})).await;

    let pending = || async {
        for _ in 0..100 {
            let (_, list) = d.call(OPERATOR, Method::GET, "/v1/access/approvals?state=pending", None).await;
            if let Some(a) = list.as_array().and_then(|l| l.first()) {
                return a.clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("no approval appeared");
    };
    // Approved while the request waits: it runs as the operator's, with the grant.
    let req = d.call(&token, Method::DELETE, "/v1/projects/pa", None);
    let decide = async {
        let a = pending().await;
        assert_eq!((a["action"].as_str(), a["resource"].as_str()), (Some("delete"), Some("pa")));
        // A grant never decides; the operator does.
        assert_eq!(d.call(&token, Method::POST, &format!("/v1/access/approvals/{}/approve", a["id"].as_str().unwrap()), None).await.0, StatusCode::FORBIDDEN);
        d.call(OPERATOR, Method::POST, &format!("/v1/access/approvals/{}/approve", a["id"].as_str().unwrap()), None).await
    };
    let ((s, out), (ds, _)) = tokio::join!(req, decide);
    assert_eq!((s, ds, out["operator"].as_bool()), (StatusCode::OK, StatusCode::OK, Some(true)), "{out}");
    assert!(out["grant"].as_str().unwrap().starts_with("tok_"));

    // Denied.
    let req = d.call(&token, Method::DELETE, "/v1/projects/pb", None);
    let decide = async {
        let a = pending().await;
        d.call(OPERATOR, Method::POST, &format!("/v1/access/approvals/{}/deny", a["id"].as_str().unwrap()), Some(json!({ "reason": "not today" }))).await
    };
    let ((s, out), _) = tokio::join!(req, decide);
    assert_eq!((s, out["code"].as_str()), (StatusCode::FORBIDDEN, Some("denied")), "{out}");
    assert!(out["error"].as_str().unwrap().contains("not today"));

    // Undecided: the approval comes back; approved later, the same request runs once with it.
    let d = Daemon::new(50);
    let (token, _) = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "delete": "ask" } }]), json!({})).await;
    let (s, out) = d.call(&token, Method::DELETE, "/v1/projects/pc", None).await;
    assert_eq!((s, out["code"].as_str()), (StatusCode::FORBIDDEN, Some("approval_pending")), "{out}");
    let id = out["approval"].as_str().unwrap().to_string();
    assert_eq!(d.call(OPERATOR, Method::POST, &format!("/v1/access/approvals/{id}/approve"), None).await.0, StatusCode::OK);
    // Not for another request.
    let other = d.send(&token, Method::DELETE, "/v1/projects/pd", None, &[("x-roll-approval", &id)]).await;
    assert_eq!(other.1["code"], "approval_mismatch");
    let (s, _) = d.send(&token, Method::DELETE, "/v1/projects/pc", None, &[("x-roll-approval", &id)]).await;
    assert_eq!(s, StatusCode::OK);
    let again = d.send(&token, Method::DELETE, "/v1/projects/pc", None, &[("x-roll-approval", &id)]).await;
    assert_eq!(again.1["code"], "approval_used");
}

/// A deploy past a freeze (`X-Roll-Override: 1`) is admin, and a deploy's approval is never one
/// for the same request with the override: it's another request, asked about on its own.
#[tokio::test]
async fn an_approved_deploy_is_no_approval_past_a_freeze() {
    let d = Daemon::new(50);
    let (token, _) = d.token(json!([{ "product": "apps", "resources": "*", "actions": { "deploy": "ask", "admin": "ask" } }]), json!({})).await;
    let path = "/v1/apps/services/svc_a/environments/production/redeploy";
    let (s, out) = d.call(&token, Method::POST, path, Some(json!({}))).await;
    assert_eq!((s, out["code"].as_str()), (StatusCode::FORBIDDEN, Some("approval_pending")), "{out}");
    let id = out["approval"].as_str().unwrap().to_string();
    let (_, list) = d.call(OPERATOR, Method::GET, "/v1/access/approvals?state=pending", None).await;
    assert_eq!(list[0]["action"], "deploy", "{list}");
    assert_eq!(d.call(OPERATOR, Method::POST, &format!("/v1/access/approvals/{id}/approve"), None).await.0, StatusCode::OK);
    // The same request with the override: not what was approved.
    let over = d.send(&token, Method::POST, path, Some(json!({})), &[("x-roll-approval", &id), ("x-roll-override", "1")]).await;
    assert_eq!(over.1["code"], "approval_mismatch", "{over:?}");
    // Asked on its own, it's about admin.
    let (s, out) = d.send(&token, Method::POST, path, Some(json!({})), &[("x-roll-override", "1")]).await;
    assert_eq!((s, out["code"].as_str()), (StatusCode::FORBIDDEN, Some("approval_pending")), "{out}");
    let (_, list) = d.call(OPERATOR, Method::GET, "/v1/access/approvals?state=pending", None).await;
    assert!(list.as_array().unwrap().iter().any(|a| a["action"] == "admin"), "{list}");
    // The approved request itself still runs once.
    assert_eq!(d.send(&token, Method::POST, path, Some(json!({})), &[("x-roll-approval", &id)]).await.0, StatusCode::OK);
}

/// A token allowed to manage access makes only tokens it covers, for no longer than it lives;
/// a token for one project makes resources only in it.
#[tokio::test]
async fn a_token_never_makes_a_stronger_one() {
    let d = Daemon::new(50);
    let end = code_sql::now_ms() + 3_600_000;
    let (admin, _) = d
        .token(
            json!([
                { "product": "access", "resources": "*", "actions": { "admin": "allow", "read": "allow" } },
                { "product": "postgres", "resources": "*", "actions": { "read": "allow" } }
            ]),
            json!({ "expires_at": end }),
        )
        .await;
    let make = |rules: Value, expires: Option<i64>| {
        let body = json!({ "name": "child", "rules": rules, "expires_at": expires });
        d.call(&admin, Method::POST, "/v1/access/tokens", Some(body))
    };
    let read = json!([{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }]);
    // The operator's policy: tokens live at most a week, a new secret for an old token too.
    let (_, old) = d.token(read.clone(), json!({})).await;
    assert_eq!(d.call(OPERATOR, Method::PUT, "/v1/access/policy", Some(json!({ "max_days": 7 }))).await.0, StatusCode::OK);
    assert_eq!(d.call(OPERATOR, Method::POST, &format!("/v1/access/tokens/{old}/rotate"), None).await.0, StatusCode::BAD_REQUEST);
    let forever = d.call(OPERATOR, Method::POST, "/v1/access/tokens", Some(json!({ "name": "forever", "rules": read }))).await;
    assert_eq!(forever.0, StatusCode::BAD_REQUEST, "{}", forever.1);
    let month = json!({ "name": "month", "rules": read, "expires_at": code_sql::now_ms() + 30 * 86_400_000 });
    assert_eq!(d.call(OPERATOR, Method::POST, "/v1/access/tokens", Some(month)).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(d.call(OPERATOR, Method::PUT, "/v1/access/policy", Some(json!({}))).await.0, StatusCode::OK);
    // A name reaches consent screens and agents' instructions: one hiding text (tag characters) is
    // refused.
    let hidden = json!({ "name": "ci\u{e0072}\u{e006d}", "rules": read, "expires_at": code_sql::now_ms() + 86_400_000 });
    let (s, e) = d.call(OPERATOR, Method::POST, "/v1/access/tokens", Some(hidden)).await;
    assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains("invisible"), "{e}");
    // A token can start later (Cloudflare's TTL start): refused until then, as an unknown one is.
    let later = json!({ "name": "release-window", "rules": read, "not_before": code_sql::now_ms() + 3_600_000, "expires_at": code_sql::now_ms() + 86_400_000 });
    let (s, made) = d.call(OPERATOR, Method::POST, "/v1/access/tokens", Some(later)).await;
    assert_eq!(s, StatusCode::CREATED, "{made}");
    let early = made["token"].as_str().unwrap().to_string();
    let (s, e) = d.call(&early, Method::GET, "/v1/projects", None).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("not_yet_valid")), "not yet, and it says so: {e}");
    let id = made["grant"]["id"].as_str().or(made["id"].as_str()).unwrap().to_string();
    assert_eq!(d.call(OPERATOR, Method::PATCH, &format!("/v1/access/tokens/{id}"), Some(json!({ "not_before": null }))).await.0, StatusCode::OK);
    assert_eq!(d.call(&early, Method::GET, "/v1/projects", None).await.0, StatusCode::OK, "started");
    // Every answer says when it stops (GitHub's GitHub-Authentication-Token-Expiration).
    let h = d.head_of(&early, "/v1/projects").await;
    assert!(h.get("roll-token-expiration").and_then(|v| v.to_str().ok()).is_some_and(|v| v.ends_with('Z') && v.contains('T')), "{h:?}");
    let backwards = json!({ "name": "backwards", "rules": read, "not_before": code_sql::now_ms() + 86_400_000, "expires_at": code_sql::now_ms() + 3_600_000 });
    assert_eq!(d.call(OPERATOR, Method::POST, "/v1/access/tokens", Some(backwards)).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(make(json!([{ "product": "postgres", "resources": "*", "actions": { "write": "allow" } }]), Some(end)).await.0, StatusCode::FORBIDDEN);
    assert_eq!(make(read.clone(), None).await.0, StatusCode::FORBIDDEN, "outlives its maker");
    // A limited maker makes only tokens limited as tightly.
    let (limited, _) = d.token(json!([{ "product": "access", "resources": "*", "actions": { "admin": "allow" } }, { "product": "postgres", "resources": "*", "actions": { "read": "allow" } }]), json!({ "rate_per_minute": 60 })).await;
    let child = |rate: Value| d.call(&limited, Method::POST, "/v1/access/tokens", Some(json!({ "name": "child", "rules": read, "rate_per_minute": rate })));
    assert_eq!(child(Value::Null).await.0, StatusCode::FORBIDDEN, "unlimited from a limited maker");
    assert_eq!(child(json!(61)).await.0, StatusCode::FORBIDDEN);
    assert_eq!(child(json!(60)).await.0, StatusCode::CREATED);
    assert_eq!(make(read, Some(end - 1000)).await.0, StatusCode::CREATED);

    let project = d.workspace.create("", "app", "", None).unwrap();
    let (scoped, _) = d.token(json!([{ "product": "postgres", "resources": [{ "project": project.id }], "actions": { "write": "allow", "read": "allow" } }]), json!({})).await;
    assert_eq!(d.call(&scoped, Method::POST, "/v1/projects", Some(json!({ "name": "x" }))).await.0, StatusCode::FORBIDDEN);
    let (s, made) = d.send(&scoped, Method::POST, "/v1/projects", Some(json!({ "name": "x" })), &[("x-roll-project", &project.id)]).await;
    assert_eq!((s, made["password"].as_str()), (StatusCode::CREATED, Some("[redacted]")));
    assert_eq!(d.workspace.projects_of("postgres", "pnew").unwrap(), vec![project.id.clone()]);
}

/// Records what would be posted to chat.
#[derive(Default)]
struct FakeChat(std::sync::Mutex<Vec<String>>);

impl roll_access::ApprovalChat for FakeChat {
    fn post(&self, install: String, channel: String, a: roll_access::store::Approval) -> futures::future::BoxFuture<'static, Result<String, String>> {
        self.0.lock().unwrap().push(format!("post {install} {channel} {} {}", a.id, a.state));
        Box::pin(async { Ok("1700.1".to_string()) })
    }
    fn update(&self, install: String, channel: String, ts: String, a: roll_access::store::Approval) -> futures::future::BoxFuture<'static, Result<(), String>> {
        self.0.lock().unwrap().push(format!("update {install} {channel} {ts} {} {}", a.id, a.state));
        Box::pin(async { Ok(()) })
    }
    fn notice(&self, install: String, channel: String, text: String) -> futures::future::BoxFuture<'static, Result<(), String>> {
        self.0.lock().unwrap().push(format!("notice {install} {channel} {text}"));
        Box::pin(async { Ok(()) })
    }
}

/// With approvals going to a chat channel, a new approval is posted there; a press from another
/// workspace, or from someone the operator didn't name, decides nothing; an approver's decides
/// it, and the card follows.
#[tokio::test(flavor = "multi_thread")]
async fn approvals_are_posted_to_chat_and_decided_there() {
    let d = Daemon::new(50);
    let chat = Arc::new(FakeChat::default());
    let _ = d.access.chat.set(chat.clone());
    let (s, _) = d.call(OPERATOR, Method::PUT, "/v1/access/notify", Some(json!({ "chat": { "install": "chi_a", "channel": "C9", "approvers": ["U1"] } }))).await;
    assert_eq!(s, StatusCode::OK);
    let (asker, _) = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "delete": "ask", "read": "allow" } }]), json!({})).await;
    assert_eq!(d.call(&asker, Method::PUT, "/v1/access/notify", Some(json!({ "chat": null }))).await.0, StatusCode::FORBIDDEN, "a token never moves approvals");
    let (_, pending) = d.call(&asker, Method::DELETE, "/v1/projects/pa", None).await;
    let id = pending["approval"].as_str().unwrap().to_string();
    let seen = |what: String| {
        let chat = chat.clone();
        async move {
            for _ in 0..100 {
                if chat.0.lock().unwrap().iter().any(|l| l.starts_with(&what)) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            panic!("never {what}: {:?}", chat.0.lock().unwrap());
        }
    };
    seen(format!("post chi_a C9 {id} pending")).await;
    let other = d.access.decide_from_chat(id.clone(), true, "slack:U1".into(), "chi_other".into()).await;
    assert!(other.is_err());
    let stranger = d.access.decide_from_chat(id.clone(), true, "slack:U2".into(), "chi_a".into()).await;
    assert!(stranger.is_err(), "only the approvers the operator named decide");
    d.access.decide_from_chat(id.clone(), true, "slack:U1".into(), "chi_a".into()).await.unwrap();
    seen(format!("update chi_a C9 1700.1 {id} approved")).await;
    let (_, a) = d.call(OPERATOR, Method::GET, &format!("/v1/access/approvals/{id}"), None).await;
    assert_eq!((a["state"].as_str(), a["decided_by"].as_str()), (Some("approved"), Some("slack:U1")));

    // Under a two-person rule a chat press counts only as the account the operator linked it to,
    // and never for that person's own token's request.
    let notify = json!({ "chat": { "install": "chi_a", "channel": "C9", "approvers": ["U1", "U3"], "people": { "U1": "usr_ana", "U3": "usr_ben" } } });
    assert_eq!(d.call(OPERATOR, Method::PUT, "/v1/access/notify", Some(json!({ "chat": { "install": "chi_a", "channel": "C9", "approvers": ["U1"], "people": { "U2": "usr_x" } } }))).await.0, StatusCode::BAD_REQUEST, "people are approvers");
    assert_eq!(d.call(OPERATOR, Method::PUT, "/v1/access/notify", Some(notify)).await.0, StatusCode::OK);
    d.call(OPERATOR, Method::PUT, "/v1/access/policy", Some(json!({ "approval_rules": [{ "product": "postgres", "actions": ["delete"], "approvals": 2 }] }))).await;
    let (_, pending) = d.call(&asker, Method::DELETE, "/v1/projects/pb", None).await;
    let id = pending["approval"].as_str().unwrap().to_string();
    d.access.decide_from_chat(id.clone(), true, "slack:U1".into(), "chi_a".into()).await.unwrap();
    assert!(d.access.decide_from_chat(id.clone(), true, "slack:U1".into(), "chi_a".into()).await.is_err(), "once");
    d.access.decide_from_chat(id.clone(), true, "slack:U3".into(), "chi_a".into()).await.unwrap();
    let (_, a) = d.call(OPERATOR, Method::GET, &format!("/v1/access/approvals/{id}"), None).await;
    assert_eq!((a["state"].as_str(), a["approved_by"].clone()), (Some("approved"), json!(["usr_ana", "usr_ben"])), "{a}");

    // Once a day, the tokens about to expire are posted there too.
    d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }]), json!({ "name": "ci-deploy", "expires_at": code_sql::now_ms() + 2 * 86_400_000 })).await;
    assert!(d.access.expiry_digest().await.unwrap());
    assert!(chat.0.lock().unwrap().iter().any(|l| l.starts_with("notice chi_a C9 One roll token expires within a week: ci-deploy (")), "{:?}", chat.0.lock().unwrap());
    assert!(!d.access.expiry_digest().await.unwrap(), "once a day");
}

/// An MCP client known by its metadata document (MCP 2025-11-25: its client_id is the document's
/// URL) connects without registering; a document that names another client is refused.
#[tokio::test(flavor = "multi_thread")]
async fn clients_connect_by_their_metadata_document() {
    let d = Daemon::new(50);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (b1, b2) = (base.clone(), base.clone());
    let docs = Router::new()
        .route("/client.json", get(move || async move { axum::Json(json!({ "client_id": format!("{b1}/client.json"), "client_name": "Doc Client", "redirect_uris": ["http://127.0.0.1:43111/cb"] })) }))
        .route("/liar.json", get(move || async move { axum::Json(json!({ "client_id": format!("{b2}/other.json"), "redirect_uris": ["http://127.0.0.1:43111/cb"] })) }));
    tokio::spawn(async move { axum::serve(listener, docs).await.unwrap() });
    let id = format!("{base}/client.json");
    let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    let (s, c) = d.call(OPERATOR, Method::GET, &format!("/v1/access/oauth/clients/{}", enc(&id)), None).await;
    assert_eq!((s, c["name"].as_str()), (StatusCode::OK, Some("Doc Client")), "{c}");
    let (s, _) = d.call(OPERATOR, Method::GET, &format!("/v1/access/oauth/clients/{}", enc(&format!("{base}/liar.json"))), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (verifier, challenge) = ("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk", "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    let consent = json!({ "client_id": id, "redirect_uri": "http://127.0.0.1:43111/cb", "code_challenge": challenge, "rules": [{ "product": "workspace", "resources": "*", "actions": { "read": "allow" } }] });
    let (s, out) = d.call(OPERATOR, Method::POST, "/v1/access/oauth/authorize", Some(consent)).await;
    assert_eq!(s, StatusCode::OK, "{out}");
    let code = url::Url::parse(out["redirect"].as_str().unwrap()).unwrap().query_pairs().find(|(k, _)| k == "code").unwrap().1.to_string();
    let (s, tokens) = d.form("/v1/access/oauth/token", &[("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", "http://127.0.0.1:43111/cb"), ("client_id", &id), ("code_verifier", verifier)]).await;
    assert_eq!(s, StatusCode::OK, "{tokens}");
    assert_eq!(d.call(tokens["access_token"].as_str().unwrap(), Method::GET, "/v1/workspace/projects", None).await.0, StatusCode::OK);
}

/// An MCP client connects with OAuth: it registers, the operator consents (narrowing what it
/// gets), the code is exchanged once with PKCE, the access token works within the consented rules,
/// refreshing rotates the refresh token, and revoking the connection ends it.
#[tokio::test]
async fn oauth_connects_an_mcp_client() {
    // No grace for a spent refresh token here: presented again, it's a stolen copy.
    let d = Daemon::with(50, |c| c.refresh_grace = std::time::Duration::ZERO);
    let (s, meta) = d.call("", Method::GET, "/.well-known/oauth-authorization-server", None).await;
    assert_eq!((s, meta["code_challenge_methods_supported"][0].as_str()), (StatusCode::OK, Some("S256")));
    let (s, pr) = d.call("", Method::GET, "/.well-known/oauth-protected-resource/v1/mcp", None).await;
    assert_eq!((s, pr["resource"].as_str()), (StatusCode::OK, Some("http://127.0.0.1:7070/v1/mcp")));

    let redirect = "http://127.0.0.1:43111/callback";
    let (s, client) = d.call("", Method::POST, "/v1/access/oauth/register", Some(json!({ "client_name": "Claude Code", "redirect_uris": [redirect] }))).await;
    assert_eq!(s, StatusCode::CREATED, "{client}");
    assert!(client.as_object().unwrap().values().all(|v| !v.is_null()), "RFC 7591: unset fields are left out: {client}");
    let client_id = client["client_id"].as_str().unwrap();
    // Registration stores nothing (a flood has nothing to fill): the id is the client, signed, and
    // a forged one is no client.
    assert!(client_id.starts_with("rlc_"), "{client_id}");
    assert!(d.access.store.list_clients().unwrap().is_empty(), "nothing kept before consent");
    let (body, _) = client_id.split_once('.').unwrap();
    let forged = format!("{body}.AAAA");
    assert!(d.access.store.client(&forged).unwrap().is_none(), "a forged id is no client");
    let bad = d.call("", Method::POST, "/v1/access/oauth/register", Some(json!({ "redirect_uris": ["http://evil.example/cb"] }))).await;
    assert_eq!(bad.1["error"], "invalid_redirect_uri");

    // RFC 7636's example pair.
    let (verifier, challenge) = ("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk", "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    let consent = json!({
        "client_id": client_id, "redirect_uri": redirect, "code_challenge": challenge, "code_challenge_method": "S256", "state": "xyz",
        "resource": "http://127.0.0.1:7070/v1/mcp",
        "rules": [{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }],
    });
    let (s, out) = d.call(OPERATOR, Method::POST, "/v1/access/oauth/authorize", Some(consent)).await;
    assert_eq!(s, StatusCode::OK, "{out}");
    assert_eq!(d.access.store.list_clients().unwrap().len(), 1, "kept once a person consented");
    let url = url::Url::parse(out["redirect"].as_str().unwrap()).unwrap();
    let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(q["state"], "xyz");
    let code = q["code"].clone();

    let d = &d;
    let exchange = |verifier: &'static str, code: String| async move {
        d.form("/v1/access/oauth/token", &[("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", redirect), ("client_id", client_id), ("code_verifier", verifier), ("resource", "http://127.0.0.1:7070/v1/mcp")]).await
    };
    let (s, tokens) = exchange(verifier, code.clone()).await;
    assert_eq!(s, StatusCode::OK, "{tokens}");
    assert_eq!(exchange(verifier, code).await.1["error"], "invalid_grant", "a code works once");
    let access = tokens["access_token"].as_str().unwrap().to_string();
    assert!(access.starts_with("rlo_"));
    // It asked for the MCP server (RFC 8707), so it works through that server only: the API
    // directly refuses it (lockdown, the credential guard and a read-only URL can't be stepped
    // around), and a token request for another resource is refused.
    assert_eq!(d.via_mcp(&access, Method::GET, "/v1/projects").await, StatusCode::OK);
    assert_eq!(d.via_mcp(&access, Method::DELETE, "/v1/projects/pa").await, StatusCode::FORBIDDEN);
    let (s, direct) = d.call(&access, Method::GET, "/v1/projects", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{direct}");
    assert!(direct.to_string().contains("MCP server"), "{direct}");
    let elsewhere = d.form("/v1/access/oauth/token", &[("grant_type", "authorization_code"), ("code", "x"), ("redirect_uri", redirect), ("client_id", client_id), ("code_verifier", verifier), ("resource", "https://elsewhere.example/mcp")]).await;
    assert_eq!(elsewhere.1["error"], "invalid_target");

    let refresh = tokens["refresh_token"].as_str().unwrap();
    assert_eq!(d.call(refresh, Method::GET, "/v1/projects", None).await.0, StatusCode::UNAUTHORIZED, "a refresh token opens nothing");
    let (s, next) = d.form("/v1/access/oauth/token", &[("grant_type", "refresh_token"), ("refresh_token", refresh), ("client_id", client_id)]).await;
    assert_eq!(s, StatusCode::OK, "{next}");
    assert_eq!(d.via_mcp(next["access_token"].as_str().unwrap(), Method::GET, "/v1/projects").await, StatusCode::OK);
    assert_eq!(d.call(next["access_token"].as_str().unwrap(), Method::GET, "/v1/projects", None).await.0, StatusCode::UNAUTHORIZED, "a refresh keeps the audience");
    // A spent refresh token used again: someone else has a copy (OAuth 2.1), so the whole
    // connection is revoked, the fresh tokens with it, and it says why.
    let reuse = d.form("/v1/access/oauth/token", &[("grant_type", "refresh_token"), ("refresh_token", refresh), ("client_id", client_id)]).await;
    assert_eq!(reuse.1["error"], "invalid_grant", "refresh tokens rotate");
    assert_eq!(d.via_mcp(next["access_token"].as_str().unwrap(), Method::GET, "/v1/projects").await, StatusCode::UNAUTHORIZED);
    let grant = next["grant_id"].as_str().unwrap();
    let (_, g) = d.call(OPERATOR, Method::GET, &format!("/v1/access/tokens/{grant}"), None).await;
    assert!(g["revoked_why"].as_str().is_some_and(|w| w.contains("used again")), "{g}");
}

/// A device signs in with a code (RFC 8628, `roll login` over SSH): it registers without a
/// redirect, gets a short code, polls (too fast is `slow_down`), a person enters the code and
/// allows it, and its next poll takes the tokens, once. A token never looks a code up, wrong codes
/// are capped per caller, and a refused device is told so.
#[tokio::test]
async fn a_device_signs_in_with_a_code() {
    // The default interval (5 s): two polls in a row are always inside it, however slow the
    // machine, and an allowed, refused or expired request answers whatever the interval.
    let d = Daemon::new(0);
    let (_, meta) = d.call("", Method::GET, "/.well-known/oauth-authorization-server", None).await;
    assert!(meta["device_authorization_endpoint"].as_str().is_some_and(|e| e.ends_with("/v1/access/oauth/device_authorization")), "{meta}");
    let (s, client) = d.call("", Method::POST, "/v1/access/oauth/register", Some(json!({ "client_name": "roll CLI", "redirect_uris": [], "grant_types": ["urn:ietf:params:oauth:grant-type:device_code", "refresh_token"] }))).await;
    assert_eq!(s, StatusCode::CREATED, "{client}");
    let client_id = client["client_id"].as_str().unwrap().to_string();
    let start = || {
        let (d, client_id) = (&d, client_id.clone());
        async move { d.form("/v1/access/oauth/device_authorization", &[("client_id", &client_id), ("scope", "development")]).await }
    };
    let (s, dev) = start().await;
    assert_eq!(s, StatusCode::OK, "{dev}");
    let user_code = dev["user_code"].as_str().unwrap().to_string();
    assert!(user_code.len() == 9 && user_code.as_bytes()[4] == b'-', "{user_code}");
    let device_code = dev["device_code"].as_str().unwrap().to_string();
    let poll = |code: String| {
        let client_id = client_id.clone();
        let d = &d;
        async move { d.form("/v1/access/oauth/token", &[("grant_type", "urn:ietf:params:oauth:grant-type:device_code"), ("device_code", &code), ("client_id", &client_id)]).await }
    };
    assert_eq!(poll(device_code.clone()).await.1["error"], "authorization_pending");
    assert_eq!(poll(device_code.clone()).await.1["error"], "slow_down", "polled within the interval");

    // A token never signs a device in, whatever its rules.
    let (token, _) = d.token(json!([{ "product": "access", "resources": "*", "actions": { "admin": "allow" } }]), json!({})).await;
    assert_eq!(d.call(&token, Method::GET, &format!("/v1/access/oauth/devices/{user_code}"), None).await.0, StatusCode::FORBIDDEN);
    // The person types it as they like; the page shows what asks and from where.
    let typed = user_code.to_lowercase().replace('-', " ");
    let (s, seen) = d.call(OPERATOR, Method::GET, &format!("/v1/access/oauth/devices/{}", urlencoding(&typed)), None).await;
    assert_eq!((s, seen["client"]["name"].as_str(), seen["scope"].as_str()), (StatusCode::OK, Some("roll CLI"), Some("development")), "{seen}");
    let allow = json!({ "rules": [{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }] });
    let (s, ok) = d.call(OPERATOR, Method::POST, &format!("/v1/access/oauth/devices/{user_code}"), Some(allow.clone())).await;
    assert_eq!(s, StatusCode::OK, "{ok}");
    assert_eq!(d.call(OPERATOR, Method::POST, &format!("/v1/access/oauth/devices/{user_code}"), Some(allow)).await.0, StatusCode::NOT_FOUND, "a code is used once");
    let (s, tokens) = poll(device_code.clone()).await;
    assert_eq!(s, StatusCode::OK, "{tokens}");
    assert_eq!(tokens["grant_id"], ok["grant_id"]);
    let access = tokens["access_token"].as_str().unwrap();
    assert_eq!(d.call(access, Method::GET, "/v1/projects", None).await.0, StatusCode::OK);
    assert_eq!(d.call(access, Method::DELETE, "/v1/projects/pa", None).await.0, StatusCode::FORBIDDEN, "only what was allowed");
    assert_eq!(poll(device_code).await.1["error"], "invalid_grant", "the tokens are taken once");

    // Refused: the device hears it.
    let (_, dev) = start().await;
    let refused = dev["user_code"].as_str().unwrap();
    assert_eq!(d.call(OPERATOR, Method::DELETE, &format!("/v1/access/oauth/devices/{refused}"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(poll(dev["device_code"].as_str().unwrap().to_string()).await.1["error"], "access_denied");

    // Only a client registered without a redirect signs devices in.
    let (_, redirecting) = d.call("", Method::POST, "/v1/access/oauth/register", Some(json!({ "client_name": "web app", "redirect_uris": ["http://127.0.0.1:9/cb"] }))).await;
    let (_, refused_client) = d.form("/v1/access/oauth/device_authorization", &[("client_id", redirecting["client_id"].as_str().unwrap())]).await;
    assert_eq!(refused_client["error"], "unauthorized_client", "{refused_client}");

    // Guessing: ten wrong codes a minute (the used code above was one), then 429 for every code,
    // a live one too, so nothing tells a hit from a miss.
    let (_, live) = start().await;
    for _ in 0..9 {
        assert_eq!(d.call(OPERATOR, Method::GET, "/v1/access/oauth/devices/BCDF-GHJK", None).await.0, StatusCode::NOT_FOUND);
    }
    assert_eq!(d.call(OPERATOR, Method::GET, "/v1/access/oauth/devices/BCDF-GHJK", None).await.0, StatusCode::TOO_MANY_REQUESTS);
    let live = live["user_code"].as_str().unwrap();
    assert_eq!(d.call(OPERATOR, Method::GET, &format!("/v1/access/oauth/devices/{live}"), None).await.0, StatusCode::TOO_MANY_REQUESTS);
}

/// An OAuth connection that ended long ago is forgotten with its client (each `roll login` is a
/// connection and a client), so neither piles up; a scoped token stays listed.
#[tokio::test]
async fn ended_connections_and_their_clients_are_forgotten() {
    let d = Daemon::new(0);
    let (_, client) = d.call("", Method::POST, "/v1/access/oauth/register", Some(json!({ "client_name": "roll CLI", "redirect_uris": [], "grant_types": ["urn:ietf:params:oauth:grant-type:device_code"] }))).await;
    let client_id = client["client_id"].as_str().unwrap();
    let (_, dev) = d.form("/v1/access/oauth/device_authorization", &[("client_id", client_id)]).await;
    let rules = json!({ "rules": [{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }] });
    let (_, ok) = d.call(OPERATOR, Method::POST, &format!("/v1/access/oauth/devices/{}", dev["user_code"].as_str().unwrap()), Some(rules)).await;
    let (_, tokens) = d.form("/v1/access/oauth/token", &[("grant_type", "urn:ietf:params:oauth:grant-type:device_code"), ("device_code", dev["device_code"].as_str().unwrap()), ("client_id", client_id)]).await;
    let (_, scoped) = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }]), json!({})).await;
    assert_eq!(d.call(OPERATOR, Method::DELETE, &format!("/v1/access/tokens/{scoped}"), None).await.0, StatusCode::OK);
    d.form("/v1/access/oauth/revoke", &[("token", tokens["refresh_token"].as_str().unwrap())]).await;
    assert_eq!(d.access.store.list_clients().unwrap().len(), 1);
    // Ended "0 days ago" is ended long enough here.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    d.access.store.prune(0).unwrap();
    let connection = ok["grant_id"].as_str().unwrap();
    assert_eq!(d.call(OPERATOR, Method::GET, &format!("/v1/access/tokens/{connection}"), None).await.0, StatusCode::NOT_FOUND);
    assert!(d.access.store.list_clients().unwrap().is_empty(), "its client went with it");
    assert_eq!(d.call(OPERATOR, Method::GET, &format!("/v1/access/tokens/{scoped}"), None).await.0, StatusCode::OK, "a scoped token stays listed");
}

fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>().replace('+', "%20")
}

/// A delivery: its headers and body.
type Sent = (Vec<(String, String)>, Vec<u8>);

/// Records what a sink would be sent; fails while `fail` is set.
#[derive(Default)]
struct FakeSink {
    sent: std::sync::Mutex<Vec<Sent>>,
    fail: std::sync::atomic::AtomicBool,
}

impl roll_access::sinks::SinkTransport for FakeSink {
    fn check(&self, url: &str) -> Result<(), String> {
        if url.starts_with("https://") { Ok(()) } else { Err("a sink is an https URL".into()) }
    }
    fn seal(&self, id: &str, secret: &str) -> Result<String, String> {
        Ok(format!("{id}|{secret}"))
    }
    fn open(&self, id: &str, sealed: &str) -> Result<String, String> {
        sealed.strip_prefix(&format!("{id}|")).map(str::to_string).ok_or_else(|| "sealed for another sink".into())
    }
    fn post(&self, _url: String, headers: Vec<(String, String)>, body: Vec<u8>) -> futures::future::BoxFuture<'static, Result<(), String>> {
        let failing = self.fail.load(std::sync::atomic::Ordering::SeqCst);
        if !failing {
            self.sent.lock().unwrap().push((headers, body));
        }
        Box::pin(async move { if failing { Err("siem.example answered 503".to_string()) } else { Ok(()) } })
    }
}

/// The audit log streams to a SIEM: only the operator adds a sink; rows arrive in order, signed
/// with the secret shown once, each once; a failed batch is kept for later.
#[tokio::test(flavor = "multi_thread")]
async fn the_audit_log_streams_to_a_sink() {
    let d = Daemon::new(50);
    let fake = Arc::new(FakeSink::default());
    let _ = d.access.sinks.set(fake.clone());
    let (token, _) = d.token(json!([{ "product": "access", "resources": "*", "actions": { "admin": "allow", "read": "allow" } }]), json!({})).await;
    let new = json!({ "url": "https://siem.example/roll", "from_start": true });
    assert_eq!(d.call(&token, Method::POST, "/v1/access/audit/sinks", Some(new.clone())).await.0, StatusCode::FORBIDDEN, "a token never moves the log");
    assert_eq!(d.call(OPERATOR, Method::POST, "/v1/access/audit/sinks", Some(json!({ "url": "http://siem.example" }))).await.0, StatusCode::BAD_REQUEST);
    let (s, made) = d.call(OPERATOR, Method::POST, "/v1/access/audit/sinks", Some(new)).await;
    assert_eq!(s, StatusCode::CREATED, "{made}");
    let (id, secret) = (made["sink"]["id"].as_str().unwrap().to_string(), made["secret"].as_str().unwrap().to_string());
    assert!(!d.call(OPERATOR, Method::GET, "/v1/access/audit/sinks", None).await.1.to_string().contains(&secret), "the secret is shown once");

    // Rows old enough to have settled (a row is written just after its request).
    let old = code_sql::now_ms() - 60_000;
    let row = |n: i64| roll_access::store::AuditEvent {
        id: format!("aud_{n:03}"),
        at: old + n,
        grant_id: None,
        grant_name: None,
        source: "api".into(),
        tool: None,
        method: "GET".into(),
        route: "/v1/projects".into(),
        product: Some("postgres".into()),
        action: Some("read".into()),
        resource: None,
        decision: "allow".into(),
        approval: None,
        status: 200,
        args: None,
        ip: None,
        duration_ms: 1,
        client: None,
    };
    for n in 0..3 {
        d.access.store.audit(&row(n));
    }
    fake.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(d.access.sweep_sinks().await.unwrap(), (0, 1));
    let (_, list) = d.call(OPERATOR, Method::GET, "/v1/access/audit/sinks", None).await;
    assert_eq!((list[0]["status"].as_str(), list[0]["error"].as_str()), (Some("error"), Some("siem.example answered 503")));
    // Kept for later: once the retry is due, the same rows go.
    fake.fail.store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(d.access.sweep_sinks().await.unwrap(), (0, 0), "not before its retry");
    assert_eq!(d.call(OPERATOR, Method::POST, &format!("/v1/access/audit/sinks/{id}/retry"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(d.access.sweep_sinks().await.unwrap(), (1, 0));
    let (headers, body) = fake.sent.lock().unwrap()[0].clone();
    let h = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone()).unwrap();
    assert_eq!(h("x-roll-sink"), id);
    assert_eq!(h("x-roll-signature-256"), roll_access::sinks::signature(&secret, h("x-roll-timestamp").parse().unwrap(), &body));
    let sent: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(ids(&sent["events"]), vec!["aud_000", "aud_001", "aud_002"]);
    // Nothing twice; the next rows next time.
    assert_eq!(d.access.sweep_sinks().await.unwrap(), (0, 0));
    d.access.store.audit(&row(3));
    assert_eq!(d.access.sweep_sinks().await.unwrap(), (1, 0));
    let sent: Value = serde_json::from_slice(&fake.sent.lock().unwrap()[1].1).unwrap();
    assert_eq!(ids(&sent["events"]), vec!["aud_003"]);
    assert_eq!(d.call(OPERATOR, Method::DELETE, &format!("/v1/access/audit/sinks/{id}"), None).await.0, StatusCode::NO_CONTENT);
}

/// A token's row says what its secret looks like (`rlt_…a1b2`, as Stripe lists keys), so one found
/// in a config can be matched to it, and rotating it changes the hint with the secret.
#[tokio::test]
async fn a_token_shows_what_its_secret_looks_like() {
    let d = Daemon::new(0);
    let (token, id) = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }]), json!({})).await;
    let shown = |t: &str| format!("rlt_…{}", &t[t.len() - 4..]);
    let (_, g) = d.call(OPERATOR, Method::GET, &format!("/v1/access/tokens/{id}"), None).await;
    assert_eq!(g["hint"].as_str(), Some(shown(&token).as_str()), "{g}");
    let (_, rotated) = d.call(OPERATOR, Method::POST, &format!("/v1/access/tokens/{id}/rotate"), Some(json!({}))).await;
    let fresh = rotated["token"].as_str().unwrap();
    assert_eq!(rotated["grant"]["hint"].as_str(), Some(shown(fresh).as_str()), "{rotated}");
}

/// Changes are in the audit log as well as requests (GitHub's `personal_access_token.create`,
/// Stripe's key history): a token's log tells who made, changed and revoked it, a person's own
/// change names them, the policy's says who set it, and a grant's own request isn't written twice.
#[tokio::test(flavor = "multi_thread")]
async fn changes_are_in_the_audit_log() {
    let d = Daemon::new(50);
    d.people.add("usr_alice", "alice");
    let read = json!([{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }]);
    let (_, made) = d.call(OPERATOR, Method::POST, "/v1/access/tokens", Some(json!({ "name": "ci", "rules": read }))).await;
    let id = made["grant"]["id"].as_str().unwrap().to_string();
    assert_eq!(d.call(OPERATOR, Method::PATCH, &format!("/v1/access/tokens/{id}"), Some(json!({ "name": "ci-2" }))).await.0, StatusCode::OK);
    assert_eq!(d.call(OPERATOR, Method::DELETE, &format!("/v1/access/tokens/{id}"), None).await.0, StatusCode::OK);
    let story = |log: &Value| log.as_array().unwrap().iter().filter(|e| e["source"] == "access").map(|e| (e["decision"].as_str().unwrap().to_string(), e["args"]["by"].as_str().unwrap_or_default().to_string())).collect::<Vec<_>>();
    let log = d.audit_until(&format!("/v1/access/audit?grant={id}"), |l| story(l).len() >= 3).await;
    let mut got = story(&log);
    got.sort();
    assert_eq!(got, [("change", "operator"), ("create", "operator"), ("revoke", "operator")].map(|(a, b)| (a.to_string(), b.to_string())), "{log}");

    let (_, hers) = d.as_user("usr_alice", Method::POST, "/v1/access/tokens", Some(json!({ "name": "mine", "rules": [{ "product": "workspace", "resources": "*", "actions": { "read": "allow" } }] }))).await;
    let hers = hers["grant"]["id"].as_str().unwrap().to_string();
    let log = d.audit_until(&format!("/v1/access/audit?grant={hers}"), |l| !story(l).is_empty()).await;
    assert_eq!(story(&log), [("create".to_string(), "usr_alice".to_string())], "{log}");
    // A change to a token isn't something it did: its last day's activity stays empty.
    let (_, list) = d.call(OPERATOR, Method::GET, "/v1/access/tokens", None).await;
    let row = list.as_array().unwrap().iter().find(|g| g["id"] == hers.as_str()).unwrap().clone();
    assert_eq!(row["activity"], json!({ "allowed": 0, "denied": 0, "asked": 0 }), "{row}");

    // A grant that manages tokens: its request is the gate's row, not a second one.
    let (admin, admin_id) = d.token(json!([{ "product": "*", "resources": "*", "actions": { "read": "allow", "admin": "allow" } }]), json!({})).await;
    let (s, sub) = d.call(&admin, Method::POST, "/v1/access/tokens", Some(json!({ "name": "sub", "rules": read }))).await;
    assert_eq!(s, StatusCode::CREATED, "{sub}");
    let log = d.audit_until(&format!("/v1/access/audit?grant={admin_id}"), |l| l.as_array().unwrap().iter().any(|e| e["source"] == "api" && e["route"] == "/v1/access/tokens")).await;
    assert!(log.as_array().unwrap().iter().any(|e| e["source"] == "api" && e["status"] == 201), "the gate's row: {log}");
    let sub_id = sub["grant"]["id"].as_str().unwrap();
    let (_, sub_log) = d.call(OPERATOR, Method::GET, &format!("/v1/access/audit?grant={sub_id}"), None).await;
    assert!(sub_log.as_array().unwrap().iter().all(|e| e["source"] != "access"), "{sub_log}");

    assert_eq!(d.call(OPERATOR, Method::PUT, "/v1/access/policy", Some(json!({ "max_days": 90 }))).await.0, StatusCode::OK);
    let log = d.audit_until("/v1/access/audit", |l| l.as_array().unwrap().iter().any(|e| e["decision"] == "policy")).await;
    assert!(log.as_array().unwrap().iter().any(|e| e["decision"] == "policy" && e["args"]["by"] == "operator"), "{log}");
}

/// A person's token that reaches what the policy has an admin approve waits for one (GitHub's
/// organization approval of fine-grained tokens): it doesn't work, says why, and only an admin
/// decides; widening an approved one waits again.
#[tokio::test(flavor = "multi_thread")]
async fn a_persons_token_past_the_policy_waits_for_an_admin() {
    let d = Daemon::new(50);
    d.people.add("usr_alice", "alice");
    // Made before the policy asked: it works, and an admin's checkup says it was never approved.
    let early_rules = json!([{ "product": "workspace", "resources": "*", "actions": { "read": "allow", "admin": "allow" } }]);
    let (_, early) = d.as_user("usr_alice", Method::POST, "/v1/access/tokens", Some(json!({ "name": "early", "rules": early_rules }))).await;
    let early_id = early["grant"]["id"].as_str().unwrap().to_string();
    assert_eq!(d.call(OPERATOR, Method::PUT, "/v1/access/policy", Some(json!({ "approve_people": ["reveal", "admin"] }))).await.0, StatusCode::OK);
    let kinds = |list: &Value, id: &str| list.as_array().unwrap().iter().find(|c| c["grant"]["id"] == json!(id)).map(|c| c["findings"].as_array().unwrap().iter().map(|f| f["kind"].as_str().unwrap().to_string()).collect::<Vec<_>>()).unwrap_or_default();
    let (_, admins) = d.call(OPERATOR, Method::GET, "/v1/access/checkup", None).await;
    assert_eq!(kinds(&admins, &early_id).first().map(String::as_str), Some("unapproved"), "{admins}");
    assert_eq!(d.call(OPERATOR, Method::POST, &format!("/v1/access/tokens/{early_id}/approve"), None).await.0, StatusCode::OK);
    let (_, admins) = d.call(OPERATOR, Method::GET, "/v1/access/checkup", None).await;
    assert!(!kinds(&admins, &early_id).iter().any(|k| k == "unapproved"), "approved now: {admins}");
    // Her OAuth connection past the policy waits too, as her tokens do.
    let (_, client) = d.call("", Method::POST, "/v1/access/oauth/register", Some(json!({ "client_name": "Cursor", "redirect_uris": ["http://127.0.0.1:43112/cb"] }))).await;
    let consent = json!({ "client_id": client["client_id"], "redirect_uri": "http://127.0.0.1:43112/cb", "code_challenge": "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM", "rules": [{ "product": "workspace", "resources": "*", "actions": { "reveal": "allow" } }] });
    assert_eq!(d.as_user("usr_alice", Method::POST, "/v1/access/oauth/authorize", Some(consent)).await.0, StatusCode::OK);
    let (_, hers) = d.as_user("usr_alice", Method::GET, "/v1/access/tokens", None).await;
    assert!(hers.as_array().unwrap().iter().any(|g| g["kind"] == "oauth" && g["pending"] == true), "{hers}");
    let read = json!([{ "product": "workspace", "resources": "*", "actions": { "read": "allow" } }]);
    let reveal = json!([{ "product": "workspace", "resources": "*", "actions": { "read": "allow", "reveal": "allow" } }]);
    // She's told first what waits (the builder and consent screen say so before she makes one).
    let (_, me) = d.as_user("usr_alice", Method::GET, "/v1/access", None).await;
    assert_eq!(me["approve_people"], json!(["reveal", "admin"]), "{me}");
    // Within the policy: it works at once.
    let (_, plain) = d.as_user("usr_alice", Method::POST, "/v1/access/tokens", Some(json!({ "name": "reader", "rules": read }))).await;
    assert!(plain["grant"]["pending"].is_null(), "{plain}");
    // Past it: made, but waiting.
    let (s, made) = d.as_user("usr_alice", Method::POST, "/v1/access/tokens", Some(json!({ "name": "revealer", "rules": reveal }))).await;
    assert_eq!((s, made["grant"]["pending"].as_bool()), (StatusCode::CREATED, Some(true)), "{made}");
    let (token, id) = (made["token"].as_str().unwrap().to_string(), made["grant"]["id"].as_str().unwrap().to_string());
    let (s, e) = d.call(&token, Method::GET, "/v1/workspace/projects", None).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::FORBIDDEN, Some("pending_approval")), "{e}");
    // On MCP too: a 403, so an OAuth client doesn't send her through sign-in again.
    let (s, e) = d.call(&token, Method::POST, "/v1/mcp", Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::FORBIDDEN, Some("pending_approval")), "{e}");
    // What the admin will decide doesn't change under them.
    assert_eq!(d.as_user("usr_alice", Method::PATCH, &format!("/v1/access/tokens/{id}"), Some(json!({ "rules": [{ "product": "*", "resources": "*", "actions": { "admin": "allow" } }] }))).await.0, StatusCode::CONFLICT);
    // It leads an admin's checkup; hers doesn't list it (she sees it waits, and it isn't hers to decide).
    let (_, admins) = d.call(OPERATOR, Method::GET, "/v1/access/checkup", None).await;
    let first = admins.as_array().unwrap().iter().find(|c| c["grant"]["id"] == json!(id)).map(|c| c["findings"][0]["kind"].clone());
    assert_eq!(first, Some(json!("pending")), "{admins}");
    let (_, hers) = d.as_user("usr_alice", Method::GET, "/v1/access/checkup", None).await;
    assert!(hers.as_array().unwrap().iter().all(|c| c["findings"].as_array().unwrap().iter().all(|f| f["kind"] != "pending")), "{hers}");
    // She can't approve her own, nor can a token; an admin can.
    assert_eq!(d.as_user("usr_alice", Method::POST, &format!("/v1/access/tokens/{id}/approve"), None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(d.call(plain["token"].as_str().unwrap(), Method::POST, &format!("/v1/access/tokens/{id}/approve"), None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(d.call(OPERATOR, Method::POST, &format!("/v1/access/tokens/{id}/approve"), None).await.0, StatusCode::OK);
    assert_eq!(d.call(&token, Method::GET, "/v1/workspace/projects", None).await.0, StatusCode::OK, "approved, it works");
    // Who decided is kept, and the audit log has it (GitHub's request_approved).
    let (_, g) = d.call(OPERATOR, Method::GET, &format!("/v1/access/tokens/{id}"), None).await;
    assert_eq!(g["decided_by"].as_str(), Some("operator"), "{g}");
    let log = d.audit_until(&format!("/v1/access/audit?grant={id}"), |l| l.as_array().is_some_and(|a| a.iter().any(|e| e["decision"] == "approve"))).await;
    assert!(log.as_array().unwrap().iter().any(|e| e["decision"] == "approve" && e["source"] == "access"), "{log}");
    // Widening the approved one past the policy waits again.
    let admin = json!([{ "product": "workspace", "resources": "*", "actions": { "read": "allow", "admin": "allow" } }]);
    let plain_id = plain["grant"]["id"].as_str().unwrap();
    let (_, widened) = d.as_user("usr_alice", Method::PATCH, &format!("/v1/access/tokens/{plain_id}"), Some(json!({ "rules": admin }))).await;
    assert_eq!(widened["pending"].as_bool(), Some(true), "{widened}");
    // Denied, it's revoked and says why; she hears both decisions.
    assert_eq!(d.call(OPERATOR, Method::POST, &format!("/v1/access/tokens/{plain_id}/deny"), None).await.0, StatusCode::OK);
    let (_, g) = d.call(OPERATOR, Method::GET, &format!("/v1/access/tokens/{plain_id}"), None).await;
    assert_eq!(g["revoked_why"].as_str(), Some("an admin denied it"), "{g}");
    // Mail goes out on its own tasks: wait for the last ones this checks.
    let mails = || d.people.1.lock().unwrap().clone();
    for _ in 0..50 {
        let got = mails();
        if got.iter().any(|m| m.contains("\"reader\" was denied")) && got.iter().any(|m| m.contains("waits for your approval: \"reader\"")) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let got = mails();
    assert!(got.iter().any(|m| m.contains("\"revealer\" was approved")) && got.iter().any(|m| m.contains("\"reader\" was denied")), "{got:?}");
    // And an admin heard each time one waited.
    assert!(got.iter().any(|m| m == "usr_root: A roll token waits for your approval: \"revealer\" (alice)"), "{got:?}");
    assert!(got.iter().any(|m| m == "usr_root: A roll token waits for your approval: \"reader\" (alice)"), "{got:?}");
}

/// A signed-in user's own token acts as them (docs/permissions.md, "Users' own tokens"): it
/// reaches only what their account does, whatever its rules say; they see and manage only their
/// own tokens and decide their own token's approvals; and it stops when their account does.
async fn approve_with(d: &Daemon, who: &str, apr: &str, body: Value) -> (StatusCode, Value) {
    d.as_user(who, Method::POST, &format!("/v1/access/approvals/{apr}/approve"), Some(body)).await
}

/// A token asks for one more rule for a while, saying why (docs/permissions.md, "Access
/// requests"): a person approves it, the policy's gates apply, and it's a rule until it lapses.
#[tokio::test(flavor = "multi_thread")]
async fn a_token_asks_for_access_for_a_while() {
    let d = Daemon::new(50);
    let ask = json!({ "product": "postgres", "action": "delete", "resource": "pa", "for": "1h", "reason": "drop the scratch database I made" });
    // A token made never to ask a person doesn't ask for more either.
    let (fixed, _) = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }]), json!({})).await;
    assert_eq!(d.call(&fixed, Method::POST, "/v1/access/requests", Some(ask.clone())).await.0, StatusCode::FORBIDDEN);
    let (token, id) = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }, { "product": "workspace", "resources": "*", "actions": { "delete": "ask" } }]), json!({})).await;
    assert_eq!(d.call(&token, Method::DELETE, "/v1/projects/pa", None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(d.call(&token, Method::POST, "/v1/access/requests", Some(json!({ "product": "postgres", "action": "delete", "for": "1y", "reason": "x" }))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(d.call(OPERATOR, Method::POST, "/v1/access/requests", Some(ask.clone())).await.0, StatusCode::BAD_REQUEST, "only a token asks");
    let tokens = json!({ "product": "access", "action": "admin", "for": "15m", "reason": "let me fix my own rules" });
    assert_eq!(d.call(&token, Method::POST, "/v1/access/requests", Some(tokens)).await.0, StatusCode::FORBIDDEN, "never tokens and approvals");
    let (s, apr) = d.call(&token, Method::POST, "/v1/access/requests", Some(ask.clone())).await;
    assert_eq!((s, apr["state"].as_str(), apr["args"]["reason"].as_str()), (StatusCode::ACCEPTED, Some("pending"), Some("drop the scratch database I made")), "{apr}");
    assert!(apr["summary"].as_str().is_some_and(|s| s.contains("every environment")), "the person reads its whole reach: {apr}");
    let apr = apr["id"].as_str().unwrap().to_string();
    assert_eq!(d.call(&token, Method::POST, "/v1/access/requests", Some(ask)).await.1["id"], json!(apr), "the same request again is the one waiting");
    // It can't be approved for longer, and a token never decides it.
    assert_eq!(d.call(OPERATOR, Method::POST, &format!("/v1/access/approvals/{apr}/approve"), Some(json!({ "remember": "always" }))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(d.call(&token, Method::POST, &format!("/v1/access/approvals/{apr}/approve"), Some(json!({}))).await.0, StatusCode::FORBIDDEN);
    // Decided anywhere (chat decides through the same call), it's the rule at once.
    assert_eq!(d.access.decide(&apr, true, "operator", None).unwrap().state, "approved");
    // Now it may, on that one database, for the hour; never another.
    assert_eq!(d.call(&token, Method::DELETE, "/v1/projects/pa", None).await.0, StatusCode::OK);
    assert_eq!(d.call(&token, Method::DELETE, "/v1/projects/pb", None).await.0, StatusCode::FORBIDDEN);
    let (_, g) = d.call(OPERATOR, Method::GET, &format!("/v1/access/tokens/{id}"), None).await;
    assert_eq!(g["temporary"][0]["approval"], json!(apr), "{g}");

    // Under a two-person rule, what two people approved is usable: the gate doesn't ask again.
    d.people.add("usr_bob", "bob");
    d.call(OPERATOR, Method::PUT, "/v1/access/policy", Some(json!({ "approval_rules": [{ "product": "postgres", "actions": ["delete"], "approvals": 2, "approvers": ["operator", "usr_bob"] }] }))).await;
    let ask = json!({ "product": "postgres", "action": "delete", "resource": "pc", "for": "15m", "reason": "and this one" });
    let apr = d.call(&token, Method::POST, "/v1/access/requests", Some(ask)).await.1["id"].as_str().unwrap().to_string();
    assert_eq!(d.access.decide(&apr, true, "operator", None).unwrap().state, "pending");
    assert_eq!(d.access.decide(&apr, true, "usr_bob", None).unwrap().state, "approved");
    assert_eq!(d.call(&token, Method::DELETE, "/v1/projects/pc", None).await.0, StatusCode::OK);
    // And the one approved by one person before the gate asks again.
    assert_eq!(d.call(&token, Method::DELETE, "/v1/projects/pa", None).await.1["code"], "approval_pending");
}

/// What roll's policy names needs several people (docs/permissions.md, "Two-person approvals"):
/// each approves once, the person whose token asked never counts, it stays pending until there
/// are enough, it's never remembered, and the product is told who approved it.
#[tokio::test(flavor = "multi_thread")]
async fn two_people_approve_what_the_policy_names() {
    let d = Daemon::new(50);
    for (u, h) in [("usr_alice", "alice"), ("usr_bob", "bob"), ("usr_carol", "carol")] {
        d.people.add(u, h);
    }
    d.people.set_admin("usr_bob", true);
    d.people.set_admin("usr_carol", true);
    let policy = |rules: Value| {
        let d = &d;
        async move { d.call(OPERATOR, Method::PUT, "/v1/access/policy", Some(json!({ "approval_rules": rules }))).await }
    };
    assert_eq!(policy(json!([{ "product": "postgres", "approvals": 2, "approvers": ["usr_bob"] }])).await.0, StatusCode::BAD_REQUEST, "two approvals from one approver");
    assert_eq!(policy(json!([{ "product": "nope", "approvals": 2 }])).await.0, StatusCode::BAD_REQUEST);
    let (s, p) = policy(json!([{ "product": "postgres", "actions": ["delete"], "approvals": 2 }])).await;
    assert_eq!(s, StatusCode::OK, "{p}");

    let (token, _) = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "delete": "ask" } }]), json!({})).await;
    let apr = d.call(&token, Method::DELETE, "/v1/projects/pa", None).await.1["approval"].as_str().unwrap().to_string();
    let approve = |who: &'static str, body: Value| {
        let (d, apr) = (&d, apr.clone());
        async move { d.as_user(who, Method::POST, &format!("/v1/access/approvals/{apr}/approve"), Some(body)).await }
    };
    // The operator's token isn't one person: it counts only where a rule names it.
    let (s, e) = d.call(OPERATOR, Method::POST, &format!("/v1/access/approvals/{apr}/approve"), Some(json!({}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{e}");
    // One person isn't enough, and approves once; it can't be remembered.
    let (s, a) = approve("usr_bob", json!({})).await;
    assert_eq!((s, a["state"].as_str(), a["approved_by"].clone()), (StatusCode::OK, Some("pending"), json!(["usr_bob"])), "{a}");
    assert_eq!(a["gates"], json!([{ "approvals": 2, "approved": 1 }]), "what it still needs");
    // An admin acts as the operator and is still named (the page knows they've approved).
    let (_, me) = d.as_user("usr_bob", Method::GET, "/v1/access", None).await;
    assert_eq!((me["kind"].as_str(), me["user"]["id"].as_str()), (Some("operator"), Some("usr_bob")), "{me}");
    assert_eq!(approve("usr_bob", json!({})).await.0, StatusCode::CONFLICT);
    assert_eq!(approve("usr_carol", json!({ "remember": "1h" })).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(d.send(&token, Method::DELETE, "/v1/projects/pa", None, &[("x-roll-approval", &apr)]).await.1["code"], "approval_pending");
    // The second: it runs, and the product hears both.
    let (s, a) = approve("usr_carol", json!({})).await;
    assert_eq!((s, a["state"].as_str()), (StatusCode::OK, Some("approved")), "{a}");
    let (s, ran) = d.send(&token, Method::DELETE, "/v1/projects/pa", None, &[("x-roll-approval", &apr)]).await;
    assert_eq!((s, ran["approved"]["by"].clone()), (StatusCode::OK, json!(["usr_bob", "usr_carol"])), "{ran}");

    // A rule remembered for a while before a gate covered it asks again: the gate isn't skipped.
    policy(json!([])).await;
    let apr = d.call(&token, Method::DELETE, "/v1/projects/pr", None).await.1["approval"].as_str().unwrap().to_string();
    assert_eq!(approve_with(&d, "usr_bob", &apr, json!({ "remember": "1h" })).await.0, StatusCode::OK);
    assert_eq!(d.call(&token, Method::DELETE, "/v1/projects/pr", None).await.0, StatusCode::OK, "remembered");
    policy(json!([{ "product": "postgres", "actions": ["delete"], "approvals": 2 }])).await;
    assert_eq!(d.call(&token, Method::DELETE, "/v1/projects/pr", None).await.1["code"], "approval_pending", "a gate came since");
    // Denied after one approval: denied.
    let apr = d.call(&token, Method::DELETE, "/v1/projects/pd", None).await.1["approval"].as_str().unwrap().to_string();
    approve_with(&d, "usr_bob", &apr, json!({})).await;
    assert_eq!(d.as_user("usr_carol", Method::POST, &format!("/v1/access/approvals/{apr}/deny"), Some(json!({}))).await.1["state"], "denied");

    // Named approvers: only they count, and a named person who isn't an admin sees and decides it.
    policy(json!([{ "product": "postgres", "actions": ["delete"], "approvals": 1, "approvers": ["usr_carol", "usr_alice"] }])).await;
    let apr = d.call(&token, Method::DELETE, "/v1/projects/pb", None).await.1["approval"].as_str().unwrap().to_string();
    let (s, e) = d.as_user("usr_bob", Method::POST, &format!("/v1/access/approvals/{apr}/approve"), Some(json!({}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{e}");
    assert!(e["error"].as_str().is_some_and(|m| m.contains("usr_carol")), "{e}");
    let (_, hers) = d.as_user("usr_alice", Method::GET, "/v1/access/approvals?state=pending", None).await;
    assert!(hers.as_array().unwrap().iter().any(|x| x["id"] == json!(apr)), "{hers}");
    assert_eq!(d.as_user("usr_alice", Method::POST, &format!("/v1/access/approvals/{apr}/approve"), Some(json!({}))).await.1["state"], "approved");

    // A person's own token: someone else approves its request, never them.
    policy(json!([{ "product": "workspace", "actions": ["delete"], "approvals": 1 }])).await;
    let rules = json!([{ "product": "workspace", "resources": "*", "actions": { "write": "allow", "delete": "ask" } }]);
    let (_, made) = d.as_user("usr_alice", Method::POST, "/v1/access/tokens", Some(json!({ "name": "agent", "rules": rules }))).await;
    let hers = made["token"].as_str().unwrap().to_string();
    let (_, p) = d.call(&hers, Method::POST, "/v1/workspace/projects", Some(json!({ "name": "scratch" }))).await;
    let (_, asked) = d.call(&hers, Method::DELETE, &format!("/v1/workspace/projects/{}", p["id"].as_str().unwrap()), None).await;
    let apr = asked["approval"].as_str().unwrap_or_else(|| panic!("{asked}")).to_string();
    let (s, e) = d.as_user("usr_alice", Method::POST, &format!("/v1/access/approvals/{apr}/approve"), Some(json!({}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{e}");
    assert_eq!(d.as_user("usr_bob", Method::POST, &format!("/v1/access/approvals/{apr}/approve"), Some(json!({}))).await.1["state"], "approved");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_users_token_acts_as_them() {
    let d = Daemon::new(50);
    d.people.add("usr_alice", "alice");
    d.people.add("usr_bob", "bob");
    d.workspace.create("usr_bob", "bobs", "", None).unwrap();
    let rules = json!([
        { "product": "workspace", "resources": "*", "actions": { "read": "allow", "write": "allow", "delete": "ask" } },
        { "product": "postgres", "resources": "*", "actions": { "read": "allow" } }
    ]);
    let (s, made) = d.as_user("usr_alice", Method::POST, "/v1/access/tokens", Some(json!({ "name": "cursor", "rules": rules }))).await;
    assert_eq!((s, made["grant"]["user"].as_str()), (StatusCode::CREATED, Some("usr_alice")), "{made}");
    let (token, id) = (made["token"].as_str().unwrap().to_string(), made["grant"]["id"].as_str().unwrap().to_string());

    // As alice: what it makes is hers, and it sees her projects, not bob's.
    let (s, p) = d.call(&token, Method::POST, "/v1/workspace/projects", Some(json!({ "name": "agent-made" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{p}");
    let (_, list) = d.call(&token, Method::GET, "/v1/workspace/projects", None).await;
    let names: Vec<&str> = list.as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["agent-made"]);
    // Never more than her account: an operator's route is refused though the rules name it.
    let (s, e) = d.call(&token, Method::GET, "/v1/projects", None).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::FORBIDDEN, Some("operator_only")));
    // And it says so when asked what it may do (MCP's roll://access/me reads this).
    let (_, me) = d.call(&token, Method::GET, "/v1/access", None).await;
    assert_eq!((me["as_user"].as_str(), me["reach"].as_array().map(Vec::len)), (Some("alice"), Some(4)), "{me}");

    // She sees and manages only her own tokens; bob can't touch hers.
    let (_, op_id) = d.token(json!([{ "product": "*", "resources": "*", "actions": { "read": "allow" } }]), json!({})).await;
    let (_, mine) = d.as_user("usr_alice", Method::GET, "/v1/access/tokens", None).await;
    assert_eq!(ids(&mine), vec![id.as_str()]);
    assert_eq!(d.as_user("usr_alice", Method::DELETE, &format!("/v1/access/tokens/{op_id}"), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(d.as_user("usr_bob", Method::DELETE, &format!("/v1/access/tokens/{id}"), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(d.as_user("usr_bob", Method::GET, &format!("/v1/access/tokens/{id}/tighten"), None).await.0, StatusCode::NOT_FOUND, "nor read what it did");
    assert_eq!(d.as_user("usr_bob", Method::GET, &format!("/v1/access/tokens/{id}/networks"), None).await.0, StatusCode::NOT_FOUND, "nor where from");
    let (_, bobs) = d.as_user("usr_bob", Method::GET, "/v1/access/checkup", None).await;
    assert!(bobs.as_array().unwrap().iter().all(|e| e["grant"]["id"] != json!(id)), "nor its checkup: {bobs}");
    assert_eq!(d.as_user("usr_bob", Method::POST, &format!("/v1/access/tokens/{id}/tighten"), Some(json!({ "from": [] }))).await.0, StatusCode::NOT_FOUND, "nor tighten it");

    // Her token's `ask` is hers to decide (bob can't); approved, it runs once.
    let path = format!("/v1/workspace/projects/{}", p["id"].as_str().unwrap());
    let (_, pending) = d.call(&token, Method::DELETE, &path, None).await;
    let apr = pending["approval"].as_str().unwrap_or_else(|| panic!("{pending}")).to_string();
    // She's mailed that it waits for her (her token's approvals are hers to decide).
    for _ in 0..100 {
        if d.people.1.lock().unwrap().iter().any(|m| m == "usr_alice: Your roll token \"cursor\" waits on your approval") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(d.people.1.lock().unwrap().iter().any(|m| m == "usr_alice: Your roll token \"cursor\" waits on your approval"), "{:?}", d.people.1.lock().unwrap());
    assert_eq!(d.as_user("usr_bob", Method::POST, &format!("/v1/access/approvals/{apr}/approve"), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(d.as_user("usr_bob", Method::DELETE, &format!("/v1/access/tokens/{id}/temporary/{apr}"), None).await.0, StatusCode::NOT_FOUND, "nor takes back hers");
    assert_eq!(d.as_user("usr_alice", Method::POST, &format!("/v1/access/approvals/{apr}/approve"), None).await.0, StatusCode::OK);
    assert!(d.send(&token, Method::DELETE, &path, None, &[("x-roll-approval", &apr)]).await.0.is_success());

    // On a signed hop, the request it carries is judged as hers too (tasks is the operator's), and
    // a hop carries only its own product's routes.
    let hop = |carried: &'static str| {
        let (d, t) = (&d, token.clone());
        async move { d.send(&t, Method::GET, "/internal/tasks", None, &[("x-roll-tasks-uri", carried)]).await }
    };
    assert_eq!(hop("/v1/tasks/queues").await.1["code"], "operator_only");
    let (s, e) = hop("/v1/workspace/projects").await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::FORBIDDEN, Some("forbidden")), "{e}");

    // Her account turned off: the token stops on its next request, and for a product's own
    // background work (a sync it made looks its grant up), at once too.
    use roll_auth::Grants as _;
    assert!(d.access.grant(&id).is_some());
    d.people.remove("usr_alice");
    assert_eq!(d.call(&token, Method::GET, "/v1/workspace/projects", None).await.0, StatusCode::UNAUTHORIZED);
    assert!(d.access.grant(&id).is_none());

    // An admin's token is theirs too: the operator's reach while they're an admin, their own once
    // they aren't, nothing once they're gone.
    d.people.add("usr_root", "root");
    d.people.set_admin("usr_root", true);
    let read_all = json!({ "name": "admin's", "rules": [{ "product": "*", "resources": "*", "actions": { "read": "allow" } }] });
    let (s, made) = d.as_user("usr_root", Method::POST, "/v1/access/tokens", Some(read_all)).await;
    assert_eq!((s, made["grant"]["user"].as_str()), (StatusCode::CREATED, Some("usr_root")), "{made}");
    let admins = made["token"].as_str().unwrap().to_string();
    assert_eq!(d.call(&admins, Method::GET, "/v1/projects", None).await.0, StatusCode::OK);
    d.people.set_admin("usr_root", false);
    assert_eq!(d.call(&admins, Method::GET, "/v1/projects", None).await.1["code"], "operator_only");
    d.people.remove("usr_root");
    assert_eq!(d.call(&admins, Method::GET, "/v1/projects", None).await.0, StatusCode::UNAUTHORIZED);
}

/// A leaked token (docs/permissions.md, "Leaked tokens") is revoked as soon as roll finds it: it
/// stops on its next request, says where it was found, and the audit log has it.
#[tokio::test(flavor = "multi_thread")]
async fn a_leaked_token_is_revoked() {
    let d = Daemon::new(50);
    let (token, id) = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }]), json!({})).await;
    assert!(roll_access::checksum_ok(&token), "new tokens carry a checksum");
    let found = roll_access::find_secrets(&format!("ROLL_TOKEN={token}\n"));
    let revoked = d.access.revoke_leaked(&found, "acme/site .env at 1a2b3c4d5e6f").unwrap();
    assert_eq!(revoked.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(), vec![id.as_str()]);
    assert_eq!(d.call(&token, Method::GET, "/v1/projects", None).await.0, StatusCode::UNAUTHORIZED);
    let (_, g) = d.call(OPERATOR, Method::GET, &format!("/v1/access/tokens/{id}"), None).await;
    assert_eq!(g["revoked_why"], "found in acme/site .env at 1a2b3c4d5e6f");
    let (_, log) = d.call(OPERATOR, Method::GET, &format!("/v1/access/audit?grant={id}"), None).await;
    assert!(log.as_array().unwrap().iter().any(|e| e["source"] == "leak" && e["decision"] == "revoke"), "{log}");
    assert!(d.access.revoke_leaked(&found, "again").unwrap().is_empty(), "once");

    // Another product's secret (apps' deploy hooks) found the same way goes to its revoker.
    let hook = roll_access::new_secret("rlk_");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let s2 = seen.clone();
    d.access.on_leaked("rlk_", move |secret, why| {
        s2.lock().unwrap().push((secret.to_string(), why.to_string()));
        Some("svc_a's production deploy hook".into())
    });
    let found = roll_access::find_secrets(&format!("curl https://api.roll.sh/v1/apps/hooks/svc_a/production/{hook}"));
    assert_eq!(found, vec![hook.clone()]);
    assert!(d.access.revoke_leaked(&found, "acme/site deploy.sh").unwrap().is_empty(), "no grant of ours");
    assert_eq!(seen.lock().unwrap().as_slice(), [(hook, "found in acme/site deploy.sh".to_string())]);
    let (_, log) = d.call(OPERATOR, Method::GET, "/v1/access/audit?product=apps", None).await;
    assert!(log.as_array().unwrap().iter().any(|e| e["decision"] == "revoke"), "{log}");
}

/// GitHub's secret scanning partner program reports roll tokens it finds in public code: a report
/// signed with one of GitHub's published keys revokes each live token and answers which were real;
/// an unsigned or wrongly signed one changes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn github_reports_of_leaked_tokens_revoke_them() {
    use base64::Engine as _;
    use ring::signature::KeyPair as _;
    // GitHub's key list, served locally: one P-256 key as SubjectPublicKeyInfo PEM.
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(&ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
    let pair = ring::signature::EcdsaKeyPair::from_pkcs8(&ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng).unwrap();
    let mut spki = vec![0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00];
    spki.extend_from_slice(pair.public_key().as_ref());
    let pem = format!("-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----", base64::engine::general_purpose::STANDARD.encode(&spki));
    let keys = json!({ "public_keys": [{ "key_identifier": "k1", "key": pem, "is_current": true }] });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/keys", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, Router::new().route("/keys", get(move || async move { axum::Json(keys) }))).await.unwrap() });

    let d = Daemon::with(50, |c| c.github_keys_url = url);
    let (token, id) = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }]), json!({})).await;
    let report = json!([
        { "token": token, "type": "roll_token", "url": "https://github.com/someone/thing/blob/main/.env", "source": "content" },
        { "token": format!("rlt_{}", "a".repeat(40)), "type": "roll_token", "url": "https://github.com/x/y", "source": "content" }
    ])
    .to_string();
    let post = |sig: String| {
        let req = Request::post("/v1/access/leaks/github")
            .header("content-type", "application/json")
            .header("github-public-key-identifier", "k1")
            .header("github-public-key-signature", sig)
            .body(Body::from(report.clone()))
            .unwrap();
        d.router.clone().oneshot(req)
    };
    let forged = base64::engine::general_purpose::STANDARD.encode(pair.sign(&rng, b"something else").unwrap().as_ref());
    assert_eq!(post(forged).await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(d.call(&token, Method::GET, "/v1/projects", None).await.0, StatusCode::OK, "a forged report revokes nothing");

    let sig = base64::engine::general_purpose::STANDARD.encode(pair.sign(&rng, report.as_bytes()).unwrap().as_ref());
    let resp = post(sig).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let answer: Value = serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let labels: Vec<&str> = answer.as_array().unwrap().iter().map(|a| a["label"].as_str().unwrap()).collect();
    assert_eq!(labels, vec!["true_positive", "false_positive"], "{answer}");
    assert_eq!(d.call(&token, Method::GET, "/v1/projects", None).await.0, StatusCode::UNAUTHORIZED);
    let (_, g) = d.call(OPERATOR, Method::GET, &format!("/v1/access/tokens/{id}"), None).await;
    assert_eq!(g["revoked_why"], "found in https://github.com/someone/thing/blob/main/.env (GitHub secret scanning)");
}

/// Approving can also allow what's asked again (docs/permissions.md, "Remembered approvals"): the
/// same action on the same resource for a while or for good. Another resource still asks, and a
/// while never becomes for good through a token the token makes.
#[tokio::test(flavor = "multi_thread")]
async fn approvals_can_be_remembered() {
    let d = Daemon::new(50);
    let rules = json!([
        { "product": "postgres", "resources": "*", "actions": { "delete": "ask" } },
        { "product": "access", "resources": "*", "actions": { "admin": "allow" } }
    ]);
    let (token, id) = d.token(rules, json!({})).await;
    let ask = |project: &'static str| {
        let (d, token) = (&d, token.clone());
        async move { d.call(&token, Method::DELETE, &format!("/v1/projects/{project}"), None).await }
    };
    let apr = ask("pa").await.1["approval"].as_str().unwrap().to_string();
    let approve = |apr: String, remember: Value| {
        let d = &d;
        async move { d.call(OPERATOR, Method::POST, &format!("/v1/access/approvals/{apr}/approve"), Some(json!({ "remember": remember }))).await }
    };
    assert_eq!(approve(apr.clone(), json!("forever")).await.0, StatusCode::BAD_REQUEST, "nothing is decided");
    // For 30 days (a developer's own CLI): the longest a while lasts.
    assert_eq!(approve(apr.clone(), json!("30d")).await.0, StatusCode::OK);
    // The product is told which approval let the request run, who decided it and, for a
    // remembered rule, until when (its audit names them).
    let (s, ran) = d.send(&token, Method::DELETE, "/v1/projects/pa", None, &[("x-roll-approval", &apr)]).await;
    assert_eq!((s, ran["approved"]["id"].as_str(), ran["approved"]["by"][0].as_str()), (StatusCode::OK, Some(apr.as_str()), Some("operator")), "{ran}");
    let hour = apr.clone();
    // For 30 days: the same request runs without asking; another project still asks.
    let (s, ran) = ask("pa").await;
    let until = ran["approved"]["until"].as_i64().unwrap_or(0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    assert_eq!((s, ran["approved"]["id"].as_str()), (StatusCode::OK, Some(hour.as_str())), "{ran}");
    assert!(until > now + 29 * 24 * 3_600_000 && until <= now + 30 * 24 * 3_600_000, "until in 30 days: {ran}");
    let (_, pending) = ask("pb").await;
    assert_eq!(pending["code"], "approval_pending");
    // Approved once: that request alone (no until).
    let once = pending["approval"].as_str().unwrap().to_string();
    assert_eq!(d.call(OPERATOR, Method::POST, &format!("/v1/access/approvals/{once}/approve"), Some(json!({}))).await.0, StatusCode::OK);
    let (s, ran) = d.send(&token, Method::DELETE, "/v1/projects/pb", None, &[("x-roll-approval", &once)]).await;
    assert_eq!((s, ran["approved"]["id"].as_str(), ran["approved"]["until"].is_null()), (StatusCode::OK, Some(once.as_str()), true), "{ran}");
    // A while never becomes for good through a token it makes.
    let child = json!({ "name": "child", "rules": [{ "product": "postgres", "resources": [{ "id": "pa" }], "actions": { "delete": "allow" } }] });
    assert_eq!(d.call(&token, Method::POST, "/v1/access/tokens", Some(child)).await.0, StatusCode::FORBIDDEN);
    // Always: a rule of the token.
    let apr = ask("pc").await.1["approval"].as_str().unwrap().to_string();
    assert_eq!(approve(apr, json!("always")).await.0, StatusCode::OK);
    let (_, g) = d.call(OPERATOR, Method::GET, &format!("/v1/access/tokens/{id}"), None).await;
    assert_eq!((g["temporary"].as_array().map(Vec::len), g["rules"].as_array().map(Vec::len)), (Some(1), Some(3)), "{g}");
    assert_eq!(g["temporary"][0]["approval"].as_str(), Some(hour.as_str()), "a while names the approval that allowed it");
    assert_eq!(ask("pc").await.0, StatusCode::OK);
    // Taken back before it's up: asking again.
    assert_eq!(d.call(&token, Method::DELETE, &format!("/v1/access/tokens/{id}/temporary/{hour}"), None).await.0, StatusCode::FORBIDDEN, "only the operator");
    assert_eq!(d.call(OPERATOR, Method::DELETE, &format!("/v1/access/tokens/{id}/temporary/{hour}"), None).await.0, StatusCode::OK);
    // (The approval itself is taken back too: it never ran the request, the rule did.)
    let again = ask("pa").await;
    assert_eq!(again.1["code"], "approval_pending", "{again:?}");

    // "Ask for anything else": what the rules don't allow asks instead of being refused, and an
    // approval remembered for good grows the token. The operator's routes stay shut.
    let (asks, asks_id) = d.token(json!([{ "product": "workspace", "resources": "*", "actions": { "read": "allow" } }]), json!({ "ask_otherwise": true })).await;
    let (_, pending) = d.call(&asks, Method::DELETE, "/v1/projects/pd", None).await;
    let apr = pending["approval"].as_str().unwrap_or_else(|| panic!("{pending}")).to_string();
    assert_eq!(approve(apr, json!("always")).await.0, StatusCode::OK);
    assert_eq!(d.call(&asks, Method::DELETE, "/v1/projects/pd", None).await.0, StatusCode::OK);
    assert_eq!(d.call(&asks, Method::GET, "/v1/unclassified", None).await.1["code"], "operator_only");
    // Its dry run says what the gate would: ask.
    let (_, c) = d.call(&asks, Method::POST, "/v1/access/check", Some(json!({ "method": "DELETE", "path": "/v1/projects/pz" }))).await;
    assert_eq!(c["decision"], "ask", "{c}");
    let (_, g) = d.call(OPERATOR, Method::GET, &format!("/v1/access/tokens/{asks_id}"), None).await;
    assert_eq!(g["rules"].as_array().map(Vec::len), Some(2), "{g}");
    // A token's page lists its own approvals, however many others there are.
    // (`id`'s approvals are older than `asks`' just now.)
    let (_, its) = d.call(OPERATOR, Method::GET, &format!("/v1/access/approvals?grant={id}&limit=1"), None).await;
    assert_eq!(its.as_array().map(|l| l.iter().filter(|x| x["grant_id"] == json!(id)).count()), Some(1), "{its}");
    // Only a token that asks may make one that asks.
    let child = |ask: bool| json!({ "name": "child", "rules": [{ "product": "postgres", "resources": "*", "actions": { "delete": "ask" } }], "ask_otherwise": ask });
    assert_eq!(d.call(&token, Method::POST, "/v1/access/tokens", Some(child(false))).await.0, StatusCode::CREATED);
    let (s, e) = d.call(&token, Method::POST, "/v1/access/tokens", Some(child(true))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{e}");

    // Copying another environment's variables is reading them: it needs reveal there too.
    let (dev, _) = d.token(json!([{ "product": "apps", "resources": "*", "environments": ["dev", "dev2"], "actions": { "write": "allow", "reveal": "allow" } }]), json!({})).await;
    let put = |from: Value| {
        let (d, t) = (&d, dev.clone());
        async move { d.call(&t, Method::PUT, "/v1/apps/services/svc_a/environments/dev2", Some(json!({ "variables_from": from }))).await }
    };
    let (s, e) = put(json!("production")).await;
    assert_eq!((s, e["needs"]["action"].as_str()), (StatusCode::FORBIDDEN, Some("reveal")), "{e}");
    assert_eq!(put(json!("dev")).await.0, StatusCode::OK);
    // Where a copy asks, the approval is about the environment copied from.
    let (copier, _) = d.token(json!([{ "product": "apps", "resources": "*", "actions": { "write": "allow", "reveal": "ask" } }]), json!({})).await;
    let (_, pending) = d.call(&copier, Method::PUT, "/v1/apps/services/svc_a/environments/staging", Some(json!({ "variables_from": "production" }))).await;
    let apr = pending["approval"].as_str().unwrap_or_else(|| panic!("{pending}")).to_string();
    let (_, asked) = d.call(OPERATOR, Method::GET, &format!("/v1/access/approvals/{apr}"), None).await;
    assert_eq!((asked["action"].as_str(), asked["environment"].as_str()), (Some("reveal"), Some("production")), "{asked}");
    assert_eq!(d.call(&dev, Method::PUT, "/v1/apps/services/svc_a/environments/dev2", Some(json!({}))).await.0, StatusCode::OK);
    // Secrets' promote names where it copies from in the query: the same rule.
    let (promoter, _) = d.token(json!([{ "product": "secrets", "resources": "*", "environments": ["development", "staging"], "actions": { "write": "allow", "reveal": "allow" } }]), json!({})).await;
    let promote = |from: &'static str| {
        let (d, t) = (&d, promoter.clone());
        async move { d.call(&t, Method::POST, &format!("/v1/secrets/projects/prj_s/environments/staging/promote?from={from}"), Some(json!({}))).await }
    };
    let (s, e) = promote("production").await;
    assert_eq!((s, e["needs"]["action"].as_str()), (StatusCode::FORBIDDEN, Some("reveal")), "{e}");
    // A parameter the decision reads is given once (secrets would read another copy of it).
    assert_eq!(promote("development&from=production").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(promote("development").await.0, StatusCode::OK);
    // Imports read each in-project source (reveal there); another project's folder is admin.
    let import = |list: Value| {
        let (d, t) = (&d, promoter.clone());
        async move { d.call(&t, Method::PUT, "/v1/secrets/projects/prj_s/environments/staging/imports?path=/api", Some(list)).await }
    };
    let (s, e) = import(json!([{ "environment": "development", "path": "/" }, { "environment": "production", "path": "/" }])).await;
    assert_eq!((s, e["needs"]["action"].as_str()), (StatusCode::FORBIDDEN, Some("reveal")), "{e}");
    assert_eq!(import(json!([{ "environment": "development", "path": "/" }])).await.0, StatusCode::OK);
    let (s, e) = import(json!([{ "project": "prj_o", "environment": "development", "path": "/" }])).await;
    assert_eq!((s, e["needs"]["action"].as_str()), (StatusCode::FORBIDDEN, Some("admin")), "{e}");
    // Asked about an import, the approval is about the folder imported; denied, asking again
    // answers the denial instead of asking the person again.
    let (asks, _) = d.token(json!([{ "product": "secrets", "resources": "*", "actions": { "write": "allow", "reveal": "ask" } }]), json!({})).await;
    let put = || {
        let (d, t) = (&d, asks.clone());
        async move { d.call(&t, Method::PUT, "/v1/secrets/projects/prj_s/environments/staging/imports?path=/app", Some(json!([{ "environment": "production", "path": "/db" }]))).await }
    };
    let apr = put().await.1["approval"].as_str().unwrap().to_string();
    let (_, asked) = d.call(OPERATOR, Method::GET, &format!("/v1/access/approvals/{apr}"), None).await;
    assert_eq!((asked["environment"].as_str(), asked["scope_path"].as_str()), (Some("production"), Some("/db")), "{asked}");
    d.call(OPERATOR, Method::POST, &format!("/v1/access/approvals/{apr}/deny"), Some(json!({ "reason": "not prod" }))).await;
    let (_, again) = put().await;
    assert_eq!((again["code"].as_str(), again["approval"].as_str()), (Some("denied"), Some(apr.as_str())), "{again}");

    // Remembered where it was asked: development's values, never production's.
    let (secrets, _) = d.token(json!([{ "product": "secrets", "resources": "*", "actions": { "reveal": "ask" } }]), json!({})).await;
    let values = |env: &'static str| {
        let (d, t) = (&d, secrets.clone());
        async move { d.call(&t, Method::GET, &format!("/v1/secrets/projects/prj_s/environments/{env}/values"), None).await }
    };
    let apr = values("development").await.1["approval"].as_str().unwrap().to_string();
    assert_eq!(approve(apr, json!("1h")).await.0, StatusCode::OK);
    assert_eq!(values("development").await.0, StatusCode::OK);
    assert_eq!(values("production").await.1["code"], "approval_pending");
}

/// Machine clients (OAuth client credentials): the operator makes one with rules; it trades its
/// secret, or a JWT it signs with its key, for an hour's token that does what its rules allow and
/// nothing more; an assertion works once; and revoking the client ends its tokens.
#[tokio::test]
async fn a_machine_client_trades_its_credential_for_a_token_its_rules_bound() {
    use base64::Engine;
    let d = Daemon::new(50);
    let rules = json!([{ "product": "search", "resources": [{ "id": "cl_1" }], "actions": { "read": "allow" } }]);
    let (s, made) = d.call(OPERATOR, Method::POST, "/v1/access/clients", Some(json!({ "name": "ci", "rules": rules }))).await;
    assert_eq!(s, StatusCode::CREATED, "{made}");
    let (id, secret) = (made["client_id"].as_str().unwrap().to_string(), made["client_secret"].as_str().unwrap().to_string());
    let (s, wrong) = d.form("/v1/access/oauth/token", &[("grant_type", "client_credentials"), ("client_id", &id), ("client_secret", "rlc_nope")]).await;
    assert_eq!((s, wrong["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("invalid_client")));
    let (s, got) = d.form("/v1/access/oauth/token", &[("grant_type", "client_credentials"), ("client_id", &id), ("client_secret", &secret)]).await;
    assert_eq!(s, StatusCode::OK, "{got}");
    let token = got["access_token"].as_str().unwrap().to_string();
    let search = |t: String| {
        let d = &d;
        async move { d.call(&t, Method::POST, "/v1/search/clusters/cl_1/indices/logs/search", Some(json!({}))).await.0 }
    };
    assert_eq!(search(token.clone()).await, StatusCode::OK);
    assert_eq!(d.call(&token, Method::GET, "/v1/secrets/projects/prj_a/transit", None).await.0, StatusCode::FORBIDDEN, "only what its rules allow");
    assert_eq!(d.call(&token, Method::GET, "/v1/access/clients", None).await.0, StatusCode::FORBIDDEN, "a client never manages clients");
    // A token can be asked for narrower: scope=read-only keeps only its rules' reads.
    let wide = json!([{ "product": "workspace", "resources": "*", "actions": { "read": "allow", "write": "allow" } }]);
    let (_, w) = d.call(OPERATOR, Method::POST, "/v1/access/clients", Some(json!({ "name": "wide", "rules": wide }))).await;
    let (wid, wsecret) = (w["client_id"].as_str().unwrap().to_string(), w["client_secret"].as_str().unwrap().to_string());
    let trade = |scope: &'static str| {
        let (d, wid, wsecret) = (&d, wid.clone(), wsecret.clone());
        async move { d.form("/v1/access/oauth/token", &[("grant_type", "client_credentials"), ("client_id", &wid), ("client_secret", &wsecret), ("scope", scope)]).await }
    };
    let (_, ro) = trade("read-only").await;
    assert_eq!(ro["scope"], "read-only", "{ro}");
    let ro = ro["access_token"].as_str().unwrap().to_string();
    assert_eq!(d.call(&ro, Method::GET, "/v1/workspace/projects", None).await.0, StatusCode::OK);
    assert_eq!(d.call(&ro, Method::POST, "/v1/workspace/projects", Some(json!({ "name": "nope" }))).await.0, StatusCode::FORBIDDEN, "read-only can't write");
    let full = trade("granted").await.1["access_token"].as_str().unwrap().to_string();
    assert_eq!(d.call(&full, Method::POST, "/v1/workspace/projects", Some(json!({ "name": "yes" }))).await.0, StatusCode::CREATED, "its rules otherwise");
    assert_eq!(trade("full").await.1["error"], "invalid_scope", "never wider");
    // Each sign-in is in the audit log on the client's grant: who, with what (never a secret);
    // and its making is too, named for what it was.
    let wgrant = w["grant"]["id"].as_str().unwrap().to_string();
    // (Audit rows are written off the request: wait for the making's too, not only the sign-ins.)
    let log = d
        .audit_until(&format!("/v1/access/audit?grant={wgrant}"), |l| {
            let rows = l.as_array().unwrap();
            rows.iter().filter(|e| e["decision"] == "sign_in").count() >= 2 && rows.iter().any(|e| e["decision"] == "create_client")
        })
        .await;
    let sign_in = log.as_array().unwrap().iter().find(|e| e["decision"] == "sign_in").unwrap();
    assert_eq!((sign_in["args"]["by"].as_str(), sign_in["args"]["credential"].as_str()), (Some(wid.as_str()), Some("secret")), "{sign_in}");
    assert!(!log.to_string().contains(&wsecret), "no secret in the log");
    assert!(log.as_array().unwrap().iter().any(|e| e["decision"] == "create_client"), "{log}");
    // The list says when it last signed in (never, for one that hasn't).
    let (_, list) = d.call(OPERATOR, Method::GET, "/v1/access/clients", None).await;
    let seen = |c: &str| list.as_array().unwrap().iter().find(|e| e["client_id"] == c).map(|e| e["last_sign_in"].is_i64());
    assert_eq!((seen(&wid), seen(&id)), (Some(true), Some(true)), "{list}");
    // Its id is no ordinary OAuth client's: no device or consent flow may hang another grant on
    // it (someone else's, which its sign-ins could then land on).
    let (s, e) = d.form("/v1/access/oauth/device_authorization", &[("client_id", &id)]).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("invalid_client")), "{e}");

    // A key it signs with: nothing secret is stored, and an assertion works once.
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).unwrap();
    let public = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
    let jwk = json!({ "kty": "OKP", "crv": "Ed25519", "x": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(public.as_bytes()) });
    let (s, keyed) = d.call(OPERATOR, Method::POST, "/v1/access/clients", Some(json!({ "name": "deployer", "rules": rules, "public_jwk": jwk }))).await;
    assert!(s == StatusCode::CREATED && keyed.get("client_secret").is_none(), "{keyed}");
    let kid = keyed["client_id"].as_str().unwrap().to_string();
    let mut der = vec![0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20];
    der.extend_from_slice(&seed);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let claims = json!({ "iss": kid, "sub": kid, "aud": made["token_endpoint"], "exp": now + 60, "jti": "one" });
    let jwt = jsonwebtoken::encode(&jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA), &claims, &jsonwebtoken::EncodingKey::from_ed_der(&der)).unwrap();
    let assert_with = |jwt: String| {
        let d = &d;
        async move { d.form("/v1/access/oauth/token", &[("grant_type", "client_credentials"), ("client_assertion_type", "urn:ietf:params:oauth:client-assertion-type:jwt-bearer"), ("client_assertion", &jwt)]).await }
    };
    let (s, signed) = assert_with(jwt.clone()).await;
    assert_eq!(s, StatusCode::OK, "{signed}");
    assert_eq!(search(signed["access_token"].as_str().unwrap().to_string()).await, StatusCode::OK);
    assert_eq!(assert_with(jwt).await.0, StatusCode::UNAUTHORIZED, "a replayed assertion");
    // The checkup nudges a client on a secret toward a key (or CI's own identity), not one on a key.
    let (_, check) = d.call(OPERATOR, Method::GET, "/v1/access/checkup", None).await;
    let kinds = |grant: &Value| check.as_array().unwrap().iter().find(|c| c["grant"]["id"] == *grant).map(|c| c["findings"].as_array().unwrap().iter().map(|f| f["kind"].as_str().unwrap().to_string()).collect::<Vec<_>>()).unwrap_or_default();
    assert!(kinds(&made["grant"]["id"]).contains(&"secret_client".to_string()) && !kinds(&keyed["grant"]["id"]).contains(&"secret_client".to_string()), "{check}");

    // Rotated, it keeps its id: kept a minute, the old secret works beside the new one; rotated
    // again keeping nothing, the old ones stop at once, and so do the tokens they got.
    let (_, rot) = d.call(OPERATOR, Method::POST, "/v1/access/clients", Some(json!({ "name": "rotating", "rules": rules }))).await;
    let rid = rot["client_id"].as_str().unwrap().to_string();
    let trade = |s: String| {
        let (d, rid) = (&d, rid.clone());
        async move { d.form("/v1/access/oauth/token", &[("grant_type", "client_credentials"), ("client_id", &rid), ("client_secret", &s)]).await }
    };
    let first = rot["client_secret"].as_str().unwrap().to_string();
    let early = trade(first.clone()).await.1["access_token"].as_str().unwrap().to_string();
    let rotate = |body: Value| {
        let (d, path) = (&d, format!("/v1/access/clients/{rid}/rotate"));
        async move { d.call(OPERATOR, Method::POST, &path, Some(body)).await.1["client_secret"].as_str().unwrap().to_string() }
    };
    let second = rotate(json!({ "keep_old_for_s": 60 })).await;
    assert_eq!((trade(first.clone()).await.0, trade(second.clone()).await.0), (StatusCode::OK, StatusCode::OK));
    let third = rotate(json!({})).await;
    assert_eq!((trade(first).await.0, trade(second).await.0, trade(third).await.0), (StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED, StatusCode::OK));
    assert_eq!(search(early).await, StatusCode::UNAUTHORIZED, "what the old secret got stops too");

    // A leaked secret (found in a push, say) revokes the client, and so its tokens, at once.
    assert!(roll_access::checksum_ok(&secret));
    assert_eq!(d.access.revoke_leaked(std::slice::from_ref(&secret), "a test").unwrap().len(), 1);
    assert_eq!(search(token).await, StatusCode::UNAUTHORIZED);
    // Revoking one by hand does the same.
    assert_eq!(d.call(OPERATOR, Method::DELETE, &format!("/v1/access/clients/{kid}"), None).await.0, StatusCode::OK);
    assert_eq!(search(signed["access_token"].as_str().unwrap().to_string()).await, StatusCode::UNAUTHORIZED);
}

/// A machine client can trust CI's own OIDC token instead of holding a credential (trusted
/// publishing): a token from its repository and branch signs in for an hour's token within its
/// rules; anyone else's, another audience, an expired one or one another key signed doesn't, and a
/// shared issuer's identity must name one repository.
#[tokio::test]
async fn a_ci_job_signs_in_with_its_own_oidc_token() {
    use base64::Engine;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
    let d = Daemon::new(50);
    let rng = ring::rand::SystemRandom::new();
    let pair = |rng: &ring::rand::SystemRandom| {
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, rng).unwrap();
        let point = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), rng).unwrap().public_key().as_ref().to_vec();
        (pkcs8.as_ref().to_vec(), point)
    };
    let (ci_key, ci_point) = pair(&rng);
    let (other_key, _) = pair(&rng);
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    // GitHub's issuer, its keys pinned (no fetch in a test).
    let jwks = json!({ "keys": [{ "kty": "EC", "crv": "P-256", "kid": "gh1", "x": b64(&ci_point[1..33]), "y": b64(&ci_point[33..]) }] }).to_string();
    let gh = "https://token.actions.githubusercontent.com";
    let rules = json!([{ "product": "search", "resources": [{ "id": "cl_1" }], "actions": { "read": "allow" } }]);
    let open = json!({ "name": "deploy", "rules": rules, "oidc": { "issuer": gh, "subject": "repo:*", "jwks": jwks } });
    assert_eq!(d.call(OPERATOR, Method::POST, "/v1/access/clients", Some(open)).await.0, StatusCode::BAD_REQUEST, "a shared issuer's identity names one repository");
    let body = json!({ "name": "deploy", "rules": rules, "oidc": { "issuer": gh, "subject": "repo:acme/web:ref:refs/heads/main", "claims": { "repository_owner_id": "42" }, "jwks": jwks } });
    let (s, made) = d.call(OPERATOR, Method::POST, "/v1/access/clients", Some(body)).await;
    assert!(s == StatusCode::CREATED && made.get("client_secret").is_none(), "{made}");
    let id = made["client_id"].as_str().unwrap().to_string();
    let (_, list) = d.call(OPERATOR, Method::GET, "/v1/access/clients", None).await;
    let trusts = &list.as_array().unwrap().iter().find(|c| c["client_id"] == id.as_str()).unwrap()["trusts"];
    assert_eq!((trusts["subject"].as_str(), trusts["pinned_keys"].as_u64()), (Some("repo:acme/web:ref:refs/heads/main"), Some(1)), "{trusts}");
    let aud = trusts["audience"].as_str().unwrap().to_string();

    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let mint = |key: &[u8], claims: Value| {
        let mut h = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        h.kid = Some("gh1".into());
        jsonwebtoken::encode(&h, &claims, &jsonwebtoken::EncodingKey::from_ec_der(key)).unwrap()
    };
    let claims = |sub: &str, aud: &str, exp: i64| json!({ "iss": gh, "sub": sub, "aud": aud, "exp": exp, "iat": now, "repository_owner_id": "42" });
    let sign_in = |jwt: String| {
        let (d, id) = (&d, id.clone());
        async move { d.form("/v1/access/oauth/token", &[("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"), ("client_id", &id), ("assertion", &jwt)]).await }
    };
    let (s, got) = sign_in(mint(&ci_key, claims("repo:acme/web:ref:refs/heads/main", &aud, now + 300))).await;
    assert_eq!(s, StatusCode::OK, "{got}");
    let token = got["access_token"].as_str().unwrap().to_string();
    assert_eq!(d.call(&token, Method::POST, "/v1/search/clusters/cl_1/indices/logs/search", Some(json!({}))).await.0, StatusCode::OK);
    assert_eq!(d.call(&token, Method::GET, "/v1/secrets/projects/prj_a/transit", None).await.0, StatusCode::FORBIDDEN, "only what its rules allow");
    // The audit log says which CI run signed in: the token's subject, never the token.
    let grant = made["grant"]["id"].as_str().unwrap();
    let log = d.audit_until(&format!("/v1/access/audit?grant={grant}"), |l| l.as_array().unwrap().iter().any(|e| e["decision"] == "sign_in")).await;
    let row = log.as_array().unwrap().iter().find(|e| e["decision"] == "sign_in").unwrap();
    assert_eq!((row["args"]["credential"].as_str(), row["args"]["subject"].as_str()), (Some("oidc"), Some("repo:acme/web:ref:refs/heads/main")), "{row}");

    for (why, jwt) in [
        ("another repository", mint(&ci_key, claims("repo:evil/web:ref:refs/heads/main", &aud, now + 300))),
        ("another branch", mint(&ci_key, claims("repo:acme/web:ref:refs/heads/dev", &aud, now + 300))),
        ("another audience", mint(&ci_key, claims("repo:acme/web:ref:refs/heads/main", "https://elsewhere.example", now + 300))),
        ("expired", mint(&ci_key, claims("repo:acme/web:ref:refs/heads/main", &aud, now - 600))),
        ("another key", mint(&other_key, claims("repo:acme/web:ref:refs/heads/main", &aud, now + 300))),
    ] {
        let (s, e) = sign_in(jwt).await;
        assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("invalid_grant")), "{why}: {e}");
    }
}

/// A refusal says where a person can allow it: the token's own page with the refused rule
/// proposed (product, action, the resource), which they decide on; nothing widens by itself.
#[tokio::test]
async fn a_refusal_links_to_where_a_person_can_allow_it() {
    let d = Daemon::new(50);
    let (token, id) = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }]), json!({})).await;
    let (s, e) = d.call(&token, Method::DELETE, "/v1/projects/prj_a", None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let widen = url::Url::parse(e["widen"].as_str().unwrap_or_else(|| panic!("{e}"))).unwrap();
    let q: std::collections::HashMap<String, String> = widen.query_pairs().into_owned().collect();
    assert!(widen.path().ends_with(&format!("/dashboard/access/tokens/{id}")), "{widen}");
    assert_eq!((q.get("add").map(String::as_str), q.get("id").map(String::as_str)), (Some("postgres:delete"), Some("prj_a")), "{widen}");
    // Its token stays as it was: the link only proposes.
    assert_eq!(d.call(&token, Method::DELETE, "/v1/projects/prj_a", None).await.0, StatusCode::FORBIDDEN);
}

/// A person's own machine client acts as them, as their tokens do: within their account's reach,
/// theirs alone to see and revoke, past the policy only once an admin approves, and it stops when
/// they can't sign in.
#[tokio::test(flavor = "multi_thread")]
async fn a_persons_machine_client_acts_as_them() {
    let d = Daemon::new(50);
    d.people.add("usr_alice", "alice");
    d.people.add("usr_bob", "bob");
    d.workspace.create("usr_alice", "alices", "", None).unwrap();
    d.workspace.create("usr_bob", "bobs", "", None).unwrap();
    let read = json!([{ "product": "*", "resources": "*", "actions": { "read": "allow" } }]);
    let (s, made) = d.as_user("usr_alice", Method::POST, "/v1/access/clients", Some(json!({ "name": "nightly", "rules": read }))).await;
    assert_eq!((s, made["grant"]["user"].as_str()), (StatusCode::CREATED, Some("usr_alice")), "{made}");
    let id = made["client_id"].as_str().unwrap().to_string();
    let trade = |id: String, secret: String| {
        let d = &d;
        async move { d.form("/v1/access/oauth/token", &[("grant_type", "client_credentials"), ("client_id", &id), ("client_secret", &secret)]).await }
    };
    let (_, got) = trade(id.clone(), made["client_secret"].as_str().unwrap().to_string()).await;
    let token = got["access_token"].as_str().unwrap_or_else(|| panic!("{got}")).to_string();
    let (_, list) = d.call(&token, Method::GET, "/v1/workspace/projects", None).await;
    assert_eq!(list.as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect::<Vec<_>>(), vec!["alices"]);
    assert_eq!(d.call(&token, Method::GET, "/v1/projects", None).await.0, StatusCode::FORBIDDEN, "never past her account");
    assert!(d.people.1.lock().unwrap().iter().any(|m| m == "usr_alice: A machine client was added to your roll account: \"nightly\""));

    // Hers alone: bob neither sees nor revokes it; the operator sees it.
    let clients = |list: &Value| list.as_array().unwrap().iter().map(|c| c["client_id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert!(clients(&d.as_user("usr_bob", Method::GET, "/v1/access/clients", None).await.1).is_empty());
    assert_eq!(d.as_user("usr_bob", Method::DELETE, &format!("/v1/access/clients/{id}"), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(clients(&d.as_user("usr_alice", Method::GET, "/v1/access/clients", None).await.1), vec![id.clone()]);
    assert_eq!(clients(&d.call(OPERATOR, Method::GET, "/v1/access/clients", None).await.1), vec![id.clone()]);

    // Past the policy, it waits for an admin: no token until one approves.
    assert_eq!(d.call(OPERATOR, Method::PUT, "/v1/access/policy", Some(json!({ "approve_people": ["admin"] }))).await.0, StatusCode::OK);
    let admin = json!([{ "product": "workspace", "resources": "*", "actions": { "admin": "allow" } }]);
    let (_, wide) = d.as_user("usr_alice", Method::POST, "/v1/access/clients", Some(json!({ "name": "wide", "rules": admin }))).await;
    assert_eq!(wide["grant"]["pending"], true, "{wide}");
    let (wide_id, wide_secret) = (wide["client_id"].as_str().unwrap().to_string(), wide["client_secret"].as_str().unwrap().to_string());
    assert_eq!(trade(wide_id.clone(), wide_secret.clone()).await.0, StatusCode::UNAUTHORIZED);
    let grant = wide["grant"]["id"].as_str().unwrap();
    assert_eq!(d.call(OPERATOR, Method::POST, &format!("/v1/access/tokens/{grant}/approve"), None).await.0, StatusCode::OK);
    assert_eq!(trade(wide_id, wide_secret).await.0, StatusCode::OK);

    // She can't sign in any more: its token stops at once.
    d.people.remove("usr_alice");
    assert_eq!(d.call(&token, Method::GET, "/v1/workspace/projects", None).await.0, StatusCode::UNAUTHORIZED);
}

/// A rule can grant one encryption key of a secrets project: its routes are decided key by key,
/// and the project's list stays shut to it (secrets doesn't filter the list by key yet).
#[tokio::test]
async fn a_rule_can_grant_one_encryption_key() {
    let d = Daemon::new(50);
    let (pay, _) = d.token(json!([{ "product": "secrets", "resources": [{ "id": "prj_a", "index": "payments" }], "actions": { "read": "allow", "write": "allow" } }]), json!({})).await;
    let encrypt = |key: &str| {
        let (d, t, path) = (&d, pay.clone(), format!("/v1/secrets/projects/prj_a/transit/{key}/encrypt"));
        async move { d.call(&t, Method::POST, &path, Some(json!({ "plaintext": "aGk=" }))).await.0 }
    };
    assert_eq!(encrypt("payments").await, StatusCode::OK);
    assert_eq!(encrypt("billing").await, StatusCode::FORBIDDEN);
    // The list lets it through: roll secrets shows it only the keys it names (its own test).
    assert_eq!(d.call(&pay, Method::GET, "/v1/secrets/projects/prj_a/transit", None).await.0, StatusCode::OK);
    // A rule on the whole project still reaches every key.
    let (all, _) = d.token(json!([{ "product": "secrets", "resources": [{ "id": "prj_a" }], "actions": { "write": "allow" } }]), json!({})).await;
    assert_eq!(d.call(&all, Method::POST, "/v1/secrets/projects/prj_a/transit/billing/encrypt", Some(json!({ "plaintext": "aGk=" }))).await.0, StatusCode::OK);
}

/// The search console is `admin`; a console request whose answer holds a credential (a new API
/// key, a connector's or Fleet's secret), however its path is spelled, is `reveal` too, as
/// `POST …/keys` is.
#[tokio::test]
async fn a_console_request_for_a_credential_needs_reveal() {
    let d = Daemon::new(50);
    let (admin, _) = d.token(json!([{ "product": "search", "resources": [{ "id": "cl_1" }], "actions": { "admin": "allow" } }]), json!({})).await;
    let (both, _) = d.token(json!([{ "product": "search", "resources": [{ "id": "cl_1" }], "actions": { "admin": "allow", "reveal": "allow" } }]), json!({})).await;
    let console = |t: &str, method: &str, path: &str| {
        let (d, t, body) = (&d, t.to_string(), json!({ "method": method, "path": path }));
        async move { d.call(&t, Method::POST, "/v1/search/clusters/cl_1/request", Some(body)).await.0 }
    };
    assert_eq!(console(&admin, "GET", "/_cat/indices").await, StatusCode::OK);
    for (method, path) in [("POST", "/_security/api_key"), ("PUT", "_security/%61pi_key"), ("GET", "/_connector/_secret/s1"), ("GET", "//_fleet//secret/s1"), ("POST", "/_security/service/elastic/fleet-server/credential/token/t1")] {
        assert_eq!(console(&admin, method, path).await, StatusCode::FORBIDDEN, "{method} {path}");
        assert_eq!(console(&both, method, path).await, StatusCode::OK, "{method} {path}");
    }
}

/// A rule can grant some indices of a search cluster: index routes are decided name by name, the
/// cluster's own (the index list, the console) stay shut, and a remembered approval keeps to the
/// index it was about.
#[tokio::test]
async fn a_rule_can_grant_some_indices() {
    let d = Daemon::new(50);
    let (logs, logs_id) = d.token(json!([{ "product": "search", "resources": [{ "id": "cl_1", "index": "logs-*" }], "actions": { "read": "allow" } }]), json!({})).await;
    let search = |t: &str, index: &str| {
        let (d, t, path) = (&d, t.to_string(), format!("/v1/search/clusters/cl_1/indices/{index}/search"));
        async move { d.call(&t, Method::POST, &path, Some(json!({}))).await.0 }
    };
    assert_eq!(search(&logs, "logs-2025").await, StatusCode::OK);
    assert_eq!(search(&logs, "logs-*,-logs-old").await, StatusCode::OK);
    assert_eq!(search(&logs, "logs-a,users").await, StatusCode::FORBIDDEN);
    assert_eq!(search(&logs, "*").await, StatusCode::FORBIDDEN);
    assert_eq!(d.call(&logs, Method::GET, "/v1/search/clusters/cl_1/indices", None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(d.call(&logs, Method::POST, "/v1/search/clusters/cl_1/request", Some(json!({}))).await.0, StatusCode::FORBIDDEN);
    // Tightened to what it used: the two expressions it searched, nothing else.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // Who touched cl_1: the cluster and what's under it; an index alone; a pattern is literal.
    let who = |q: &str| {
        let (d, path) = (&d, format!("/v1/access/audit?{q}"));
        async move { d.call(OPERATOR, Method::GET, &path, None).await.1.as_array().unwrap().iter().map(|e| e["resource"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>() }
    };
    // The audit writer batches: wait for the searches' rows rather than a fixed time.
    d.audit_until("/v1/access/audit?resource=cl_1", |l| l.as_array().is_some_and(|a| a.iter().any(|e| e["resource"] == "cl_1/logs-2025"))).await;
    let all = who("resource=cl_1").await;
    assert!(all.contains(&"cl_1".to_string()) && all.contains(&"cl_1/logs-2025".to_string()), "{all:?}");
    assert!(who("resource=cl_1/logs-2025&action=read").await.iter().all(|r| r == "cl_1/logs-2025"));
    assert!(who("resource=cl_%25").await.is_empty(), "a % is a percent sign");
    assert!(who("resource=CL_1").await.is_empty(), "exact, case and all");
    let (_, t) = d.call(OPERATOR, Method::GET, &format!("/v1/access/tokens/{logs_id}/tighten"), None).await;
    assert_eq!(t["narrower"], true, "{t}");
    assert_eq!(t["rules"][0]["resources"], json!([{ "id": "cl_1", "index": "logs-*,-logs-old" }, { "id": "cl_1", "index": "logs-2025" }]), "{t}");
    // Applied only over the rules it was made from: a change in between is a conflict.
    let apply = |from: Value| {
        let (d, path) = (&d, format!("/v1/access/tokens/{logs_id}/tighten"));
        async move { d.call(OPERATOR, Method::POST, &path, Some(json!({ "from": from }))).await }
    };
    assert_eq!(apply(json!([{ "product": "search", "resources": "*", "actions": { "read": "allow" } }])).await.0, StatusCode::CONFLICT);
    let (s, g) = apply(t["from"].clone()).await;
    assert_eq!((s, &g["rules"]), (StatusCode::OK, &t["rules"]), "{g}");

    // Asked about one index and remembered: that index, not its neighbours.
    let (asker, _) = d.token(json!([{ "product": "search", "resources": [{ "id": "cl_1" }], "actions": { "read": "ask" } }]), json!({})).await;
    let (_, pending) = d.call(&asker, Method::POST, "/v1/search/clusters/cl_1/indices/logs-a/search", Some(json!({}))).await;
    let apr = pending["approval"].as_str().unwrap_or_else(|| panic!("{pending}")).to_string();
    let (s, _) = d.call(OPERATOR, Method::POST, &format!("/v1/access/approvals/{apr}/approve"), Some(json!({ "remember": "1h" }))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(search(&asker, "logs-a").await, StatusCode::OK);
    let (_, again) = d.call(&asker, Method::POST, "/v1/search/clusters/cl_1/indices/logs-b/search", Some(json!({}))).await;
    assert_eq!(again["code"], "approval_pending", "logs-b still asks: {again}");
    // A request says what made it (its User-Agent), and the log keeps it.
    d.send(&logs, Method::POST, "/v1/search/clusters/cl_1/indices/logs-ua/search", Some(json!({})), &[("user-agent", "roll-cli/0.9\tforged")]).await;
    let ua = d.audit_until("/v1/access/audit?resource=cl_1/logs-ua", |l| !l[0]["client"].is_null()).await;
    assert_eq!(ua[0]["client"], "roll-cli/0.9forged", "one line: {ua}");
}

/// A link hands the linked resource's credentials to a service (its variables): it needs reveal
/// on that resource, by path or in a new service's body, and an approval for it is about that
/// resource (remembering it allows revealing exactly that one).
#[tokio::test]
async fn a_link_needs_reveal_on_what_it_links() {
    let d = Daemon::new(50);
    let (apps, _) = d.token(json!([{ "product": "apps", "resources": "*", "actions": { "write": "allow", "deploy": "allow" } }]), json!({})).await;
    let link = |t: &str, kind: &str, r: &str| {
        let (d, t, path) = (&d, t.to_string(), format!("/v1/apps/services/svc_a/links/{kind}/{r}"));
        async move { d.call(&t, Method::PUT, &path, Some(json!({}))).await }
    };
    let (s, e) = link(&apps, "postgres", "prj_x").await;
    assert_eq!((s, e["needs"]["product"].as_str(), e["needs"]["action"].as_str()), (StatusCode::FORBIDDEN, Some("postgres"), Some("reveal")), "{e}");
    assert_eq!(link(&apps, "mystery", "x").await.0, StatusCode::FORBIDDEN, "a kind roll doesn't know");
    let create = json!({ "project": "p", "name": "web", "source": { "image": "nginx" }, "links": [{ "kind": "redis", "ref": "rdb_9" }] });
    let (s, e) = d.call(&apps, Method::POST, "/v1/apps/services", Some(create)).await;
    assert_eq!((s, e["needs"]["product"].as_str()), (StatusCode::FORBIDDEN, Some("redis")), "{e}");
    // A worker scaled on a stream group's lag: apps reads that stream for it, so read there too.
    let scale = json!({ "run": { "scale": { "stream": "st_9", "group": "workers", "per_instance": 100 } } });
    let (s, e) = d.call(&apps, Method::PATCH, "/v1/apps/services/svc_a", Some(scale.clone())).await;
    assert_eq!((s, e["needs"]["product"].as_str(), e["needs"]["action"].as_str()), (StatusCode::FORBIDDEN, Some("streams"), Some("read")), "{e}");
    let (reader, _) = d.token(json!([
        { "product": "apps", "resources": "*", "actions": { "write": "allow" } },
        { "product": "streams", "resources": [{ "id": "st_9" }], "actions": { "read": "allow" } },
    ]), json!({})).await;
    assert_eq!(d.call(&reader, Method::PATCH, "/v1/apps/services/svc_a", Some(scale)).await.0, StatusCode::OK);
    // A stream pushed to a web service: read on it (apps consumes it for the service), and write
    // with a dead-letter topic (apps appends to it).
    let push = |dead: bool| {
        let mut rule = json!({ "stream": "st_9", "topic": "orders", "group": "web", "path": "/events" });
        if dead {
            rule["dead_letter"] = json!("orders-dead");
        }
        json!({ "run": { "push": [rule] } })
    };
    let (s, e) = d.call(&apps, Method::PATCH, "/v1/apps/services/svc_a", Some(push(false))).await;
    assert_eq!((s, e["needs"]["product"].as_str(), e["needs"]["action"].as_str()), (StatusCode::FORBIDDEN, Some("streams"), Some("read")), "{e}");
    // (A setting that changes what production does, `push` is deploy-level, not `write`.)
    let (pusher, _) = d.token(json!([
        { "product": "apps", "resources": "*", "actions": { "write": "allow", "deploy": "allow" } },
        { "product": "streams", "resources": [{ "id": "st_9" }], "actions": { "read": "allow" } },
    ]), json!({})).await;
    assert_eq!(d.call(&pusher, Method::PATCH, "/v1/apps/services/svc_a", Some(push(false))).await.0, StatusCode::OK);
    let (s, e) = d.call(&pusher, Method::PATCH, "/v1/apps/services/svc_a", Some(push(true))).await;
    assert_eq!((s, e["needs"]["action"].as_str()), (StatusCode::FORBIDDEN, Some("write")), "{e}");
    // A roll code repo apps reads for the caller (a service built from it, a plan of it): read on
    // that repo too, so no apps grant reaches a repo's code, head or stack by itself.
    let from_repo = json!({ "project": "p", "name": "api", "source": { "kind": "repo", "repo": "acme/secret" } });
    let (s, e) = d.call(&apps, Method::POST, "/v1/apps/services", Some(from_repo.clone())).await;
    assert_eq!((s, e["needs"]["product"].as_str(), e["needs"]["action"].as_str()), (StatusCode::FORBIDDEN, Some("code"), Some("read")), "{e}");
    let (s, e) = d.call(&apps, Method::POST, "/v1/apps/plan", Some(json!({ "repo": "acme/secret" }))).await;
    assert_eq!((s, e["needs"]["product"].as_str()), (StatusCode::FORBIDDEN, Some("code")), "{e}");
    let (coder, _) = d.token(json!([
        { "product": "apps", "resources": "*", "actions": { "read": "allow", "write": "allow", "deploy": "allow" } },
        { "product": "code", "resources": [{ "id": "acme/secret" }], "actions": { "read": "allow" } },
    ]), json!({})).await;
    assert_eq!(d.call(&coder, Method::POST, "/v1/apps/services", Some(from_repo)).await.0, StatusCode::OK);
    assert_eq!(d.call(&coder, Method::POST, "/v1/apps/plan", Some(json!({ "repo": "acme/secret" }))).await.0, StatusCode::OK);

    let (both, both_id) = d.token(json!([
        { "product": "apps", "resources": "*", "actions": { "write": "allow", "deploy": "allow" } },
        { "product": "postgres", "resources": [{ "id": "prj_x" }], "actions": { "reveal": "allow" } },
        { "product": "postgres", "resources": [{ "id": "prj_y" }, { "id": "prj_z" }], "actions": { "reveal": "ask" } },
    ]), json!({})).await;
    assert_eq!(link(&both, "postgres", "prj_x").await.0, StatusCode::OK);
    // The link's reveal is what the token used too: tightening keeps it. Usage is recorded after
    // the answer, so it's waited for (a fixed 100 ms wasn't enough under a loaded gate).
    let used = |t: &Value| t["used"].as_array().is_some_and(|u| u.iter().any(|u| u["product"] == "postgres" && u["action"] == "reveal" && u["resource"] == "prj_x"));
    let mut t = Value::Null;
    for _ in 0..50 {
        t = d.call(OPERATOR, Method::GET, &format!("/v1/access/tokens/{both_id}/tighten"), None).await.1;
        if used(&t) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(used(&t), "{t}");
    let (_, pending) = link(&both, "postgres", "prj_y").await;
    let apr = pending["approval"].as_str().unwrap_or_else(|| panic!("{pending}")).to_string();
    let (_, asked) = d.call(OPERATOR, Method::GET, &format!("/v1/access/approvals/{apr}"), None).await;
    assert_eq!((asked["product"].as_str(), asked["action"].as_str(), asked["resource"].as_str()), (Some("postgres"), Some("reveal"), Some("prj_y")), "{asked}");
    // Several links it may not reveal outright: the approval names them all, and is approved once.
    let two = json!({ "project": "p", "name": "web", "source": { "image": "nginx" }, "links": [{ "kind": "postgres", "ref": "prj_y" }, { "kind": "postgres", "ref": "prj_z" }] });
    let (_, pending) = d.call(&both, Method::POST, "/v1/apps/services", Some(two)).await;
    let apr = pending["approval"].as_str().unwrap_or_else(|| panic!("{pending}")).to_string();
    let (_, asked) = d.call(OPERATOR, Method::GET, &format!("/v1/access/approvals/{apr}"), None).await;
    assert_eq!(asked["resource"], "prj_y, prj_z", "{asked}");
    let (s, _) = d.call(OPERATOR, Method::POST, &format!("/v1/access/approvals/{apr}/approve"), Some(json!({ "remember": "1h" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// A search cluster made with a `parent` is a branch of it, a copy of its indices: it needs `write`
/// on the parent too, as `…/{id}/branches` does, so a token for one project's clusters can't
/// branch (and read) any other cluster.
#[tokio::test]
async fn a_search_branch_by_parent_needs_write_on_the_parent() {
    let d = Daemon::new(50);
    let project = d.workspace.create("", "app", "", None).unwrap();
    let mine = json!({ "product": "search", "resources": [{ "project": project.id }], "actions": { "write": "allow" } });
    let (scoped, _) = d.token(json!([mine.clone()]), json!({})).await;
    let create = |t: &str, body: Value| {
        let (d, t, p) = (&d, t.to_string(), project.id.clone());
        async move { d.send(&t, Method::POST, "/v1/search/clusters", Some(body), &[("x-roll-project", &p)]).await }
    };
    assert_eq!(create(&scoped, json!({ "name": "fresh" })).await.0, StatusCode::OK);
    let (s, e) = create(&scoped, json!({ "name": "copy", "parent": "cl_9" })).await;
    assert_eq!((s, e["needs"]["action"].as_str(), e["needs"]["resource"].as_str()), (StatusCode::FORBIDDEN, Some("write"), Some("cl_9")), "{e}");
    let (both, _) = d.token(json!([mine, { "product": "search", "resources": [{ "id": "cl_9" }], "actions": { "write": "allow" } }]), json!({})).await;
    assert_eq!(create(&both, json!({ "name": "copy", "parent": "cl_9" })).await.0, StatusCode::OK);
}

/// What the gate costs a request: a token's request against the same one by the operator (no
/// evaluator), on a list and on one resource. Run by hand, in release:
/// `cargo test --release -p roll-access --test access gate_latency -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn gate_latency() {
    let d = Daemon::new(50);
    let (token, token_id) = d.token(json!([{ "product": "*", "resources": "*", "actions": { "read": "allow", "delete": "allow" } }]), json!({})).await;
    let (narrow, _) = d.token(json!([{ "product": "postgres", "resources": [{ "id": "pa" }, { "id": "pb" }], "actions": { "read": "allow", "delete": "allow" } }]), json!({})).await;
    for (what, method, path) in [("list", Method::GET, "/v1/projects"), ("one", Method::DELETE, "/v1/projects/pa")] {
        for (who, t) in [("operator", OPERATOR), ("token *", token.as_str()), ("token ids", narrow.as_str())] {
            let mut lat = Vec::new();
            for i in 0..2200 {
                let start = std::time::Instant::now();
                let (s, _) = d.call(t, method.clone(), path, None).await;
                assert!(s.is_success(), "{who} {what}: {s}");
                if i >= 200 {
                    lat.push(start.elapsed().as_micros() as u64);
                }
            }
            lat.sort_unstable();
            println!("{what:5} {who:10} p50 {:>5} µs  p99 {:>5} µs", lat[lat.len() / 2], lat[lat.len() * 99 / 100]);
        }
    }
    // Throughput: 32 callers at once, each request audited (a token's) or not (the operator's).
    let d = Arc::new(d);
    for (who, t) in [("operator", OPERATOR.to_string()), ("token *", token.clone())] {
        let start = std::time::Instant::now();
        let tasks: Vec<_> = (0..32)
            .map(|_| {
                let (d, t) = (d.clone(), t.clone());
                tokio::spawn(async move {
                    for _ in 0..300 {
                        assert!(d.call(&t, Method::DELETE, "/v1/projects/pa", None).await.0.is_success());
                    }
                })
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }
        println!("32 at once {who:10} {:>6.0} requests/s", 9600.0 / start.elapsed().as_secs_f64());
    }
    // Every request audited: none lost in the writer's queue.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let activity = d.access.store.activity().unwrap();
    assert_eq!(activity.get(&token_id).map(|a| a.0), Some(2 * 2200 + 9600), "every request of the token is in the log");
}

/// The audit log keeps its days and drops what's older, however much there is (in batches, so a
/// big prune never holds the database).
#[tokio::test]
async fn the_audit_log_keeps_its_days() {
    let d = Daemon::new(50);
    let day = 24 * 3600 * 1000;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let row = |n: i64, at: i64| roll_access::store::AuditEvent {
        id: format!("aud_{n:06}"),
        at,
        grant_id: None,
        grant_name: None,
        source: "api".into(),
        tool: None,
        method: "GET".into(),
        route: "/v1/projects".into(),
        product: Some("postgres".into()),
        action: Some("read".into()),
        resource: None,
        decision: "allow".into(),
        approval: None,
        status: 200,
        args: None,
        ip: None,
        duration_ms: 1,
        client: None,
    };
    for n in 0..12_000 {
        d.access.store.audit(&row(n, now - 91 * day + n));
    }
    for n in 0..3 {
        d.access.store.audit(&row(100_000 + n, now - day));
    }
    d.access.store.prune(90).unwrap();
    let (_, kept) = d.call(OPERATOR, Method::GET, "/v1/access/audit?limit=1000", None).await;
    assert_eq!(kept.as_array().map(Vec::len), Some(3), "only the last 90 days");
    // A NUL (a path's `%00`) is dropped from what's kept, never the row (Postgres refuses NULs).
    let mut nul = row(200_000, now);
    nul.resource = Some("prj_\0x".into());
    assert!(d.access.store.queue_audit(nul).is_none());
    assert!(d.access.store.flush_audit(std::time::Duration::from_secs(5)));
    let (_, kept) = d.call(OPERATOR, Method::GET, "/v1/access/audit?resource=prj_x", None).await;
    assert_eq!(kept.as_array().map(Vec::len), Some(1), "{kept}");
}

/// A token used from a network it hasn't been before is news (a leaked token is used from
/// somewhere new): its first network isn't, a known one isn't, and once it roams (five) neither is
/// the next. Networks are /24s and /48s, so one office or runner pool is one.
#[tokio::test]
async fn a_new_network_is_news_until_a_token_roams() {
    let d = Daemon::new(50);
    let (_, id) = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }]), json!({})).await;
    let used = |ip: &str| d.access.store.record_use(&id, Some(ip)).map(|n| (n.network, n.networks));
    assert_eq!(used("203.0.113.5"), None, "where it starts");
    assert_eq!(used("203.0.113.77"), None, "the same /24");
    assert_eq!(used("198.51.100.9"), Some(("198.51.100.0/24".to_string(), 2)));
    assert_eq!(used("2001:db8:1:2::7"), Some(("2001:db8:1::/48".to_string(), 3)));
    assert_eq!(used("198.51.100.200"), None, "known");
    assert_eq!(used("::ffff:198.51.100.10"), None, "the same IPv4 network, on a dual-stack listener");
    assert!(used("192.0.2.1").is_some() && used("100.64.0.1").is_some(), "the fourth and the fifth");
    assert_eq!(used("10.9.8.7"), None, "roaming now");
    let (_, list) = d.call(OPERATOR, Method::GET, &format!("/v1/access/tokens/{id}/networks"), None).await;
    assert_eq!(list.as_array().map(Vec::len), Some(6), "{list}");
}

/// The access checkup names the tokens that need attention and why, each finding with its fix:
/// admin over everything and never expiring; used for less than its rules allow; a well-scoped,
/// expiring token that uses what it has is left alone.
#[tokio::test]
async fn the_checkup_names_what_needs_attention() {
    let d = Daemon::new(50);
    let week = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64 + 7 * 86_400_000;
    let (_, root) = d.token(json!([{ "product": "*", "resources": "*", "actions": { "admin": "allow" } }]), json!({})).await;
    let (wide, wide_id) = d.token(json!([{ "product": "*", "resources": "*", "actions": { "read": "allow" } }]), json!({ "expires_at": week })).await;
    let (fit, fit_id) = d.token(json!([{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }]), json!({ "expires_at": week })).await;
    for t in [&wide, &fit] {
        assert_eq!(d.call(t, Method::GET, "/v1/projects", None).await.0, StatusCode::OK);
    }
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let (_, list) = d.call(OPERATOR, Method::GET, "/v1/access/checkup", None).await;
    let kinds = |id: &str| -> Vec<String> {
        list.as_array().unwrap().iter().find(|e| e["grant"]["id"] == id).map(|e| e["findings"].as_array().unwrap().iter().map(|f| f["kind"].as_str().unwrap().to_string()).collect()).unwrap_or_default()
    };
    assert_eq!(kinds(&root), ["everything", "never_expires"], "{list}");
    assert_eq!(kinds(&wide_id), ["narrower"], "{list}");
    assert!(kinds(&fit_id).is_empty(), "{list}");
}

/// A person hears of their own tokens by mail (GitHub mails a token's owner): when one is about to
/// expire and when roll revokes one as leaked. Nobody else hears of them, and an operator's token
/// mails no one.
#[tokio::test(flavor = "multi_thread")]
async fn a_person_is_told_of_their_tokens() {
    let d = Daemon::new(50);
    d.people.add("usr_alice", "alice");
    d.people.add("usr_bob", "bob");
    let rules = json!([{ "product": "workspace", "resources": "*", "actions": { "read": "allow" } }]);
    let soon = code_sql::now_ms() + 2 * 86_400_000;
    let (s, made) = d.as_user("usr_alice", Method::POST, "/v1/access/tokens", Some(json!({ "name": "cursor", "rules": rules, "expires_at": soon }))).await;
    assert_eq!(s, StatusCode::CREATED, "{made}");
    d.token(rules.clone(), json!({ "name": "ci", "expires_at": soon })).await;

    let told = |want: Vec<&'static str>| {
        let people = d.people.clone();
        async move {
            for _ in 0..100 {
                if *people.1.lock().unwrap() == want {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            panic!("told {:?}, not {want:?}", people.1.lock().unwrap());
        }
    };
    // Added to her account: she hears at once (a token from a stolen session is noticed).
    told(vec!["usr_alice: A token was added to your roll account: \"cursor\""]).await;
    assert!(d.access.expiry_digest().await.unwrap());
    told(vec!["usr_alice: A token was added to your roll account: \"cursor\"", "usr_alice: Your roll token \"cursor\" expires within a week"]).await;

    let token = made["token"].as_str().unwrap().to_string();
    assert_eq!(d.access.revoke_leaked(&[token], "a public gist").unwrap().len(), 1);
    told(vec![
        "usr_alice: A token was added to your roll account: \"cursor\"",
        "usr_alice: Your roll token \"cursor\" expires within a week",
        "usr_alice: roll revoked your token \"cursor\"",
    ])
    .await;

    // A device signed in with a code she entered (or was tricked into entering): she hears it.
    let (_, client) = d.call("", Method::POST, "/v1/access/oauth/register", Some(json!({ "client_name": "roll CLI", "redirect_uris": [], "grant_types": ["urn:ietf:params:oauth:grant-type:device_code"] }))).await;
    let (_, dev) = d.form("/v1/access/oauth/device_authorization", &[("client_id", client["client_id"].as_str().unwrap())]).await;
    let code = dev["user_code"].as_str().unwrap();
    assert_eq!(d.as_user("usr_alice", Method::POST, &format!("/v1/access/oauth/devices/{code}"), Some(json!({ "rules": rules }))).await.0, StatusCode::OK);
    told(vec![
        "usr_alice: A token was added to your roll account: \"cursor\"",
        "usr_alice: Your roll token \"cursor\" expires within a week",
        "usr_alice: roll revoked your token \"cursor\"",
        "usr_alice: A device was signed in to your roll account",
    ])
    .await;
}

/// A product's page lists the tokens made for one of its resources (roll secrets' project tokens):
/// those with a rule on that product naming it, not every token that reaches it.
#[tokio::test(flavor = "multi_thread")]
async fn tokens_are_listed_by_what_they_were_made_for() {
    let d = Daemon::new(50);
    let rule = |resources: Value| json!([{ "product": "secrets", "resources": resources, "environments": ["staging"], "actions": { "read": "allow" } }]);
    let (_, ci) = d.token(rule(json!([{ "project": "prj_a" }])), json!({ "name": "ci" })).await;
    d.token(rule(json!([{ "project": "prj_b" }])), json!({ "name": "other" })).await;
    d.token(rule(json!("*")), json!({ "name": "everything" })).await;
    let (s, list) = d.call(OPERATOR, Method::GET, "/v1/access/tokens?product=secrets&resource=prj_a", None).await;
    assert_eq!(s, StatusCode::OK, "{list}");
    assert_eq!(ids(&list), vec![ci.as_str()]);
    assert_eq!(d.call(OPERATOR, Method::GET, "/v1/access/tokens?resource=prj_a", None).await.0, StatusCode::BAD_REQUEST);
}

/// A workload that proved who it is (roll secrets' identity logins) gets a token for minutes,
/// within its identity's scope and owned by whoever set it up: not while that person can't sign
/// in, kept out of the token list, and working like any token until it ends.
#[tokio::test(flavor = "multi_thread")]
async fn a_workload_gets_a_token_for_minutes() {
    let d = Daemon::new(50);
    d.people.add("usr_alice", "alice");
    let scope = |actions: Value| serde_json::from_value::<roll_auth::scope::Scope>(json!([{ "product": "workspace", "resources": "*", "actions": actions }])).unwrap();
    let new = |actions: Value| serde_json::from_value::<roll_access::store::NewGrant>(json!({ "name": "ci: acme/site main", "rules": [{ "product": "workspace", "resources": "*", "actions": actions }] })).unwrap();
    let within = scope(json!({ "read": "allow" }));

    assert!(d.access.mint_workload(new(json!({ "write": "allow" })), &within, 15, Some("usr_alice")).await.is_err(), "beyond the identity's scope");
    assert!(d.access.mint_workload(new(json!({ "read": "allow" })), &within, 240, Some("usr_alice")).await.is_err(), "for hours");
    let (g, secret) = d.access.mint_workload(new(json!({ "read": "allow" })), &within, 15, Some("usr_alice")).await.unwrap();
    assert_eq!((g.kind, g.user.as_deref()), ("workload", Some("usr_alice")));
    assert!(g.expires_at.unwrap() <= code_sql::now_ms() + 15 * 60_000);
    assert_eq!(d.call(&secret, Method::GET, "/v1/workspace/projects", None).await.0, StatusCode::OK);

    let (_, list) = d.call(OPERATOR, Method::GET, "/v1/access/tokens", None).await;
    assert!(!ids(&list).contains(&g.id.as_str()), "not in the token list");
    let (_, list) = d.call(OPERATOR, Method::GET, "/v1/access/tokens?kind=workload", None).await;
    assert_eq!(ids(&list), vec![g.id.as_str()]);

    // It makes no tokens, even short ones within its rules.
    let maker = serde_json::from_value::<roll_access::store::NewGrant>(json!({ "name": "ci: maker", "rules": [{ "product": "access", "resources": "*", "actions": { "read": "allow", "admin": "allow" } }] })).unwrap();
    let access_scope = serde_json::from_value::<roll_auth::scope::Scope>(json!([{ "product": "access", "resources": "*", "actions": { "read": "allow", "admin": "allow" } }])).unwrap();
    let (_, maker_secret) = d.access.mint_workload(maker, &access_scope, 15, None).await.unwrap();
    let child = json!({ "name": "child", "rules": [{ "product": "access", "resources": "*", "actions": { "read": "allow" } }], "expires_at": code_sql::now_ms() + 5 * 60_000 });
    let (s, e) = d.call(&maker_secret, Method::POST, "/v1/access/tokens", Some(child)).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::FORBIDDEN, Some("a workload token can't make or change tokens")), "{e}");

    // Fixed: never given longer, rotated or grown.
    let edit = d.call(OPERATOR, Method::PATCH, &format!("/v1/access/tokens/{}", g.id), Some(json!({ "expires_at": null }))).await;
    assert_eq!(edit.0, StatusCode::CONFLICT, "{}", edit.1);
    assert_eq!(d.call(OPERATOR, Method::POST, &format!("/v1/access/tokens/{}/rotate", g.id), None).await.0, StatusCode::CONFLICT);

    // Its identity gone, it stops at once; only a workload grant is revoked this way.
    let (_, op_id) = d.token(json!([{ "product": "workspace", "resources": "*", "actions": { "read": "allow" } }]), json!({})).await;
    assert!(!d.access.revoke_workload(&op_id, "identity deleted").unwrap(), "a person's token isn't a workload's");
    assert!(d.access.revoke_workload(&g.id, "identity deleted").unwrap());
    assert_eq!(d.call(&secret, Method::GET, "/v1/workspace/projects", None).await.0, StatusCode::UNAUTHORIZED);

    d.people.remove("usr_alice");
    assert!(d.access.mint_workload(new(json!({ "read": "allow" })), &within, 15, Some("usr_alice")).await.is_err(), "its owner can't sign in");
    d.access.store.prune(30).unwrap();
}

/// Open registration (RFC 7591) can't be flooded: a network registers at most 20 clients a minute
/// on a node, then 429 with Retry-After.
#[tokio::test]
async fn registration_from_one_network_is_limited() {
    let d = Daemon::new(50);
    let register = || d.call("", Method::POST, "/v1/access/oauth/register", Some(json!({ "client_name": "flood", "redirect_uris": ["http://127.0.0.1:9/cb"] })));
    for _ in 0..20 {
        assert_eq!(register().await.0, StatusCode::CREATED);
    }
    let (s, e) = register().await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some("too_many_requests")), "{e}");
}

/// A company's identity provider signs a person in to an app (SEP-990, the ID-JAG grant): the app
/// authenticates, the provider's assertion names the person, and the token acts as them within
/// the app's rules. Replays, another client's assertion, another audience and an unknown person
/// are refused; removing the app ends its people's tokens.
#[tokio::test(flavor = "multi_thread")]
async fn an_enterprise_idp_signs_a_person_in_to_an_app() {
    use base64::Engine;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
    let d = Daemon::new(50);
    d.people.add("usr_alice", "alice");
    d.people.add("usr_bob", "bob");
    d.people.add("usr_root", "root");
    d.people.set_admin("usr_root", true);
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    let point = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap().public_key().as_ref().to_vec();
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let jwks = json!({ "keys": [{ "kty": "EC", "crv": "P-256", "kid": "okta1", "x": b64(&point[1..33]), "y": b64(&point[33..]) }] }).to_string();
    let okta = "https://acme.okta.example";
    assert_eq!(d.call(OPERATOR, Method::PUT, "/v1/access/enterprise/idps", Some(json!({ "idps": [{ "issuer": "http://acme.okta.example", "jwks": jwks }] }))).await.0, StatusCode::BAD_REQUEST, "an https issuer");
    assert_eq!(d.call(OPERATOR, Method::PUT, "/v1/access/enterprise/idps", Some(json!({ "idps": [{ "issuer": okta, "jwks": jwks }] }))).await.0, StatusCode::BAD_REQUEST, "email matching names its domains");
    let trust = |admins: bool| {
        let (d, jwks) = (&d, jwks.clone());
        async move { d.call(OPERATOR, Method::PUT, "/v1/access/enterprise/idps", Some(json!({ "idps": [{ "issuer": okta, "jwks": jwks, "name": "Okta", "domains": ["acme.test"], "admins": admins }] }))).await.0 }
    };
    assert_eq!(trust(false).await, StatusCode::OK);
    let rules = json!([{ "product": "workspace", "resources": "*", "actions": { "read": "allow", "write": "allow" } }]);
    let (s, app) = d.call(OPERATOR, Method::POST, "/v1/access/enterprise/apps", Some(json!({ "name": "Claude", "rules": rules }))).await;
    assert_eq!(s, StatusCode::CREATED, "{app}");
    let (client_id, secret) = (app["client_id"].as_str().unwrap().to_string(), app["client_secret"].as_str().unwrap().to_string());
    let (_, listed) = d.call(OPERATOR, Method::GET, "/v1/access/enterprise", None).await;
    assert!(listed["apps"][0].get("client_secret").is_none() && listed["apps"][0].get("secret_hash").is_none(), "never its secret: {listed}");

    let api = d.access.config.api_url.trim_end_matches('/').to_string();
    assert_eq!((listed["audience"].as_str(), listed["token_endpoint"].as_str()), (Some(api.as_str()), Some(format!("{api}/v1/access/oauth/token").as_str())), "what the provider is set up with");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let idjag = |claims: Value| {
        let mut h = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        h.kid = Some("okta1".into());
        h.typ = Some("oauth-id-jag+jwt".into());
        jsonwebtoken::encode(&h, &claims, &jsonwebtoken::EncodingKey::from_ec_der(pkcs8.as_ref())).unwrap()
    };
    let claims = |jti: &str, email: &str, aud: &str, client: &str| json!({ "iss": okta, "sub": "00u1alice", "aud": aud, "client_id": client, "jti": jti, "exp": now + 300, "iat": now, "email": email, "email_verified": true, "resource": api });
    let sign_in = |assertion: String, secret: String| {
        let (d, id) = (&d, client_id.clone());
        async move { d.form("/v1/access/oauth/token", &[("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"), ("assertion", &assertion), ("client_id", &id), ("client_secret", &secret)]).await }
    };
    let (s, got) = sign_in(idjag(claims("j1", "alice@acme.test", &api, &client_id)), secret.clone()).await;
    assert_eq!((s, got.get("refresh_token")), (StatusCode::OK, None), "{got}");
    let token = got["access_token"].as_str().unwrap().to_string();
    let (_, me) = d.call(&token, Method::GET, "/v1/access", None).await;
    assert_eq!((me["grant"]["user"].as_str(), me["grant"]["created_by"].as_str()), (Some("usr_alice"), Some(format!("enterprise:{okta}").as_str())), "it acts as alice: {me}");
    assert_eq!(d.call(&token, Method::POST, "/v1/workspace/projects", Some(json!({ "name": "from-claude" }))).await.0, StatusCode::CREATED);
    // An assertion for roll's MCP server alone: its token works through that server, never on the
    // API directly (draft -06: the token is held to the assertion's resources).
    let mut for_mcp = claims("j40", "alice@acme.test", &api, &client_id);
    for_mcp["resource"] = json!(format!("{api}/v1/mcp"));
    let (_, mcp_only) = sign_in(idjag(for_mcp), secret.clone()).await;
    let mcp_token = mcp_only["access_token"].as_str().unwrap_or_else(|| panic!("{mcp_only}")).to_string();
    assert_eq!(d.via_mcp(&mcp_token, Method::GET, "/v1/workspace/projects").await, StatusCode::OK);
    assert_eq!(d.call(&mcp_token, Method::GET, "/v1/workspace/projects", None).await.0, StatusCode::UNAUTHORIZED, "the MCP server's only");
    // Refusals: a replay, the wrong secret, no client authentication, another client's assertion,
    // another audience, someone with no account.
    assert_eq!(sign_in(idjag(claims("j1", "alice@acme.test", &api, &client_id)), secret.clone()).await.1["error"], "invalid_grant");
    assert_eq!(sign_in(idjag(claims("j2", "alice@acme.test", &api, &client_id)), "rlc_wrong".into()).await.1["error"], "invalid_client");
    let bare = d.form("/v1/access/oauth/token", &[("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"), ("assertion", &idjag(claims("j3", "alice@acme.test", &api, &client_id)))]).await;
    assert_eq!(bare.1["error"], "invalid_client", "{bare:?}");
    assert_eq!(sign_in(idjag(claims("j4", "alice@acme.test", &api, "ent_other")), secret.clone()).await.1["error"], "invalid_grant");
    assert_eq!(sign_in(idjag(claims("j5", "alice@acme.test", "https://elsewhere.example", &client_id)), secret.clone()).await.1["error"], "invalid_grant");
    assert_eq!(sign_in(idjag(claims("j6", "mallory@acme.test", &api, &client_id)), secret.clone()).await.1["error"], "access_denied");
    // An email the provider didn't say it verified, or outside its domains: no.
    let mut unverified = claims("j8", "alice@acme.test", &api, &client_id);
    unverified.as_object_mut().unwrap().remove("email_verified");
    assert_eq!(sign_in(idjag(unverified), secret.clone()).await.1["error"], "access_denied");
    assert_eq!(sign_in(idjag(claims("j9", "alice@elsewhere.test", &api, &client_id)), secret.clone()).await.1["error"], "access_denied");
    // An admin acts as the operator: only from a provider the operator lets sign admins in.
    assert_eq!(sign_in(idjag(claims("j10", "root@acme.test", &api, &client_id)), secret.clone()).await.1["error"], "access_denied");
    trust(true).await;
    assert_eq!(sign_in(idjag(claims("j11", "root@acme.test", &api, &client_id)), secret.clone()).await.0, StatusCode::OK);
    trust(false).await;
    // The same provider as people's SSO connection: its issuer, and the domains its DNS records
    // proved, as they are at each sign-in (never typed domains).
    let tie = |idp: Value| {
        let d = &d;
        async move { d.call(OPERATOR, Method::PUT, "/v1/access/enterprise/idps", Some(json!({ "idps": [idp] }))).await.0 }
    };
    d.people.sso("sso_okta", Some((okta, &["acme.test"])));
    d.people.sso("sso_other", Some(("https://other.example", &["acme.test"])));
    assert_eq!(tie(json!({ "issuer": okta, "jwks": jwks, "sso": "sso_nope" })).await, StatusCode::BAD_REQUEST, "no such connection");
    assert_eq!(tie(json!({ "issuer": okta, "jwks": jwks, "sso": "sso_other" })).await, StatusCode::BAD_REQUEST, "another issuer's connection");
    assert_eq!(tie(json!({ "issuer": okta, "jwks": jwks, "sso": "sso_okta", "domains": ["acme.test"] })).await, StatusCode::BAD_REQUEST, "its domains are the connection's");
    assert_eq!(tie(json!({ "issuer": okta, "jwks": jwks, "sso": "sso_okta" })).await, StatusCode::OK);
    assert_eq!(sign_in(idjag(claims("j50", "alice@acme.test", &api, &client_id)), secret.clone()).await.0, StatusCode::OK);
    d.people.sso("sso_okta", Some((okta, &[])));
    assert_eq!(sign_in(idjag(claims("j51", "alice@acme.test", &api, &client_id)), secret.clone()).await.1["error"], "access_denied", "the domain's proof is gone");
    d.people.sso("sso_okta", None);
    assert_eq!(sign_in(idjag(claims("j52", "alice@acme.test", &api, &client_id)), secret.clone()).await.1["error"], "access_denied", "the connection is gone");
    trust(false).await;
    // Exactly one audience (draft-ietf-oauth-identity-assertion-authz-grant-06 §4.4.1): an array
    // of one is roll's, a second audience is refused.
    let mut two = claims("j30", "alice@acme.test", &api, &client_id);
    two["aud"] = json!([api, "https://elsewhere.example"]);
    assert_eq!(sign_in(idjag(two), secret.clone()).await.1["error"], "invalid_grant", "two audiences");
    let mut one = claims("j31", "alice@acme.test", &api, &client_id);
    one["aud"] = json!([api]);
    assert_eq!(sign_in(idjag(one), secret.clone()).await.0, StatusCode::OK, "an array of one");
    // A multi-tenant issuer: trusted with one tenant, never its others; an assertion naming a
    // tenant from a provider trusted without one is refused.
    let tenant_of = |jti: &str, tenant: &str| {
        let mut c = claims(jti, "alice@acme.test", &api, &client_id);
        c["tenant"] = json!(tenant);
        c
    };
    assert_eq!(sign_in(idjag(tenant_of("j32", "t-acme")), secret.clone()).await.1["error"], "invalid_grant", "a tenant roll wasn't told about");
    let put = d.call(OPERATOR, Method::PUT, "/v1/access/enterprise/idps", Some(json!({ "idps": [{ "issuer": okta, "jwks": jwks, "domains": ["acme.test"], "tenant": "t-acme" }] }))).await;
    assert_eq!(put.0, StatusCode::OK, "{put:?}");
    assert_eq!(sign_in(idjag(tenant_of("j33", "t-acme")), secret.clone()).await.0, StatusCode::OK, "the trusted tenant");
    assert_eq!(sign_in(idjag(tenant_of("j34", "t-evil")), secret.clone()).await.1["error"], "invalid_grant", "another tenant at the same issuer");
    assert_eq!(sign_in(idjag(claims("j35", "alice@acme.test", &api, &client_id)), secret.clone()).await.1["error"], "invalid_grant", "no tenant, from a tenant-pinned provider");
    trust(false).await;
    let (_, meta) = d.call(OPERATOR, Method::GET, "/.well-known/oauth-authorization-server", None).await;
    assert_eq!(meta["authorization_grant_profiles_supported"], json!(["urn:ietf:params:oauth:grant-profile:id-jag"]), "{meta}");
    // The assertion's read-only scope: the token reads, never writes.
    let mut reads = claims("j12", "bob@acme.test", &api, &client_id);
    reads["scope"] = json!("read-only");
    let (_, ro) = sign_in(idjag(reads), secret.clone()).await;
    let ro_token = ro["access_token"].as_str().unwrap_or_else(|| panic!("{ro}")).to_string();
    assert_eq!(d.call(&ro_token, Method::POST, "/v1/workspace/projects", Some(json!({ "name": "nope" }))).await.0, StatusCode::FORBIDDEN, "read-only");
    // Revoked in roll: the provider can't hand it back.
    let bobs = d.call(&ro_token, Method::GET, "/v1/access", None).await.1["grant"]["id"].as_str().unwrap().to_string();
    assert_eq!(d.call(OPERATOR, Method::DELETE, &format!("/v1/access/tokens/{bobs}"), None).await.0, StatusCode::OK);
    assert_eq!(sign_in(idjag(claims("j13", "bob@acme.test", &api, &client_id)), secret.clone()).await.1["error"], "access_denied");
    // Again later: the same connection, a new token.
    let (_, again) = sign_in(idjag(claims("j7", "ALICE@acme.test", &api, &client_id)), secret.clone()).await;
    let token2 = again["access_token"].as_str().unwrap_or_else(|| panic!("{again}")).to_string();
    let (_, me2) = d.call(&token2, Method::GET, "/v1/access", None).await;
    assert_eq!(me2["grant"]["id"], me["grant"]["id"], "one connection per person and app");
    // Narrowing the app narrows every live token at once (its people's connections take its rules).
    let reads_only = json!([{ "product": "workspace", "resources": "*", "actions": { "read": "allow" } }]);
    let (s, changed) = d.call(OPERATOR, Method::PATCH, &format!("/v1/access/enterprise/apps/{client_id}"), Some(json!({ "rules": reads_only }))).await;
    assert_eq!(s, StatusCode::OK, "{changed}");
    assert_eq!(d.call(&token2, Method::POST, "/v1/workspace/projects", Some(json!({ "name": "after-narrowing" }))).await.0, StatusCode::FORBIDDEN, "narrowed at once");
    assert_eq!(d.call(&token2, Method::GET, "/v1/access", None).await.0, StatusCode::OK, "the connection stays");
    // A rotation keeping the old secret a while: both sign in; the new one is answered once.
    let (s, rotated) = d.call(OPERATOR, Method::POST, &format!("/v1/access/enterprise/apps/{client_id}/rotate"), Some(json!({ "keep_old_for_s": 3600 }))).await;
    let secret2 = rotated["client_secret"].as_str().unwrap_or_else(|| panic!("{s} {rotated}")).to_string();
    assert!(rotated["old_until"].as_i64().is_some(), "{rotated}");
    assert_eq!(sign_in(idjag(claims("j20", "alice@acme.test", &api, &client_id)), secret.clone()).await.0, StatusCode::OK, "the old secret, kept");
    let (_, fresh) = sign_in(idjag(claims("j21", "alice@acme.test", &api, &client_id)), secret2.clone()).await;
    let token3 = fresh["access_token"].as_str().unwrap_or_else(|| panic!("{fresh}")).to_string();
    // Leaked (found by a scan, or GitHub's): the secret, the kept one and every token go at once.
    assert!(d.access.revoke_leaked(std::slice::from_ref(&secret2), "a public gist").unwrap().is_empty(), "no grant is revoked: the connections stay");
    assert_eq!(d.call(&token3, Method::GET, "/v1/access", None).await.0, StatusCode::UNAUTHORIZED, "its tokens ended");
    for (jti, s) in [("j22", &secret2), ("j23", &secret)] {
        assert_eq!(sign_in(idjag(claims(jti, "alice@acme.test", &api, &client_id)), s.clone()).await.1["error"], "invalid_client", "after the leak");
    }
    let (_, listed) = d.call(OPERATOR, Method::GET, "/v1/access/enterprise", None).await;
    assert_eq!(listed["apps"][0]["leaked"], "found in a public gist", "{listed}");
    // The operator rotates it: a new secret, and the same people back on their connections.
    let (_, again) = d.call(OPERATOR, Method::POST, &format!("/v1/access/enterprise/apps/{client_id}/rotate"), Some(json!({ "keep_old_for_s": 3600 }))).await;
    assert!(again["old_until"].is_null() && again["leaked"].is_null(), "a leaked secret is never kept: {again}");
    let secret3 = again["client_secret"].as_str().unwrap().to_string();
    let (_, back) = sign_in(idjag(claims("j24", "alice@acme.test", &api, &client_id)), secret3).await;
    let token4 = back["access_token"].as_str().unwrap_or_else(|| panic!("{back}")).to_string();
    assert_eq!(d.call(&token4, Method::GET, "/v1/access", None).await.1["grant"]["id"], me["grant"]["id"], "the same connection");
    let token2 = token4;
    // Removing the app ends its people's tokens (alice's and root's; bob's was revoked already).
    assert_eq!(d.call(OPERATOR, Method::DELETE, &format!("/v1/access/enterprise/apps/{client_id}"), None).await.1["revoked"], 2);
    assert_eq!(d.call(&token2, Method::GET, "/v1/access", None).await.0, StatusCode::UNAUTHORIZED);
}

/// Sudo mode (docs/accounts.md): a token made from a person's session needs a recent proof; the
/// operator's token, which has no session, doesn't.
#[tokio::test]
async fn a_token_from_a_session_needs_a_recent_proof() {
    let d = Daemon::new(50);
    d.people.add("usr_alice", "alice");
    let rules = json!([{ "product": "workspace", "resources": "*", "actions": { "read": "allow" } }]);
    d.people.3.lock().unwrap().insert("usr_alice".into());
    let (s, e) = d.as_user("usr_alice", Method::POST, "/v1/access/tokens", Some(json!({ "name": "ci", "rules": rules }))).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::FORBIDDEN, Some("reauth_required")), "{e}");
    assert_eq!(d.call(OPERATOR, Method::POST, "/v1/access/tokens", Some(json!({ "name": "ci", "rules": rules }))).await.0, StatusCode::CREATED);
    d.people.3.lock().unwrap().clear();
    assert_eq!(d.as_user("usr_alice", Method::POST, "/v1/access/tokens", Some(json!({ "name": "ci", "rules": rules }))).await.0, StatusCode::CREATED);
}

/// An incident's cut-off (docs/permissions.md, "Revoking at once"): every listed token at once,
/// with why, all or none; the operator's only.
#[tokio::test]
async fn tokens_are_revoked_at_once_or_not_at_all() {
    let d = Daemon::new(50);
    d.people.add("usr_alice", "alice");
    let rules = json!([{ "product": "workspace", "resources": "*", "actions": { "read": "allow" } }]);
    let mut made = Vec::new();
    for name in ["ci", "deploy", "kept"] {
        let (_, g) = d.call(OPERATOR, Method::POST, "/v1/access/tokens", Some(json!({ "name": name, "rules": rules }))).await;
        made.push((g["grant"]["id"].as_str().unwrap().to_string(), g["token"].as_str().unwrap().to_string()));
    }
    let ids = |i: &[usize]| json!(i.iter().map(|&n| made[n].0.clone()).collect::<Vec<_>>());
    // An unknown id refuses the lot.
    let (s, _) = d.call(OPERATOR, Method::POST, "/v1/access/tokens/revoke", Some(json!({ "ids": [made[0].0, "grt_nope"], "why": "incident 42" }))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(d.call(&made[0].1, Method::GET, "/v1/workspace/projects", None).await.0, StatusCode::OK, "nothing was revoked");
    // Not a person's, not a token's.
    assert_eq!(d.as_user("usr_alice", Method::POST, "/v1/access/tokens/revoke", Some(json!({ "ids": ids(&[0]), "why": "x" }))).await.0, StatusCode::FORBIDDEN);
    let (s, r) = d.call(OPERATOR, Method::POST, "/v1/access/tokens/revoke", Some(json!({ "ids": ids(&[0, 1]), "why": "incident 42: the CI box leaked" }))).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert!(r["revoked"].as_array().unwrap().iter().all(|g| g["revoked_why"] == "incident 42: the CI box leaked" && g["revoked_at"].is_i64()), "{r}");
    assert_eq!(d.call(&made[0].1, Method::GET, "/v1/workspace/projects", None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(d.call(&made[1].1, Method::GET, "/v1/workspace/projects", None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(d.call(&made[2].1, Method::GET, "/v1/workspace/projects", None).await.0, StatusCode::OK, "only the listed ones");
}

/// A decoy token (docs/permissions.md, "Decoy tokens"): it never allows anything, answers as an
/// unknown token does, and its use is in the audit log, on the token, and in the admins' mail.
#[tokio::test(flavor = "multi_thread")]
async fn a_decoy_token_allows_nothing_and_gives_its_user_away() {
    let d = Daemon::new(50);
    d.people.add("usr_alice", "alice");
    // A person's decoy never waits for an admin, whatever its rules (they're never used).
    assert_eq!(d.call(OPERATOR, Method::PUT, "/v1/access/policy", Some(json!({ "approve_people": ["reveal", "admin"] }))).await.0, StatusCode::OK);
    let rules = json!([{ "product": "workspace", "resources": "*", "actions": { "read": "allow", "admin": "allow" } }]);
    let (s, g) = d.as_user("usr_alice", Method::POST, "/v1/access/tokens", Some(json!({ "name": "aws creds in the old wiki", "rules": rules, "decoy": true }))).await;
    assert_eq!(s, StatusCode::CREATED, "{g}");
    assert_eq!((g["grant"]["decoy"].as_bool(), g["grant"].get("pending")), (Some(true), None), "{g}");
    let (id, token) = (g["grant"]["id"].as_str().unwrap().to_string(), g["token"].as_str().unwrap().to_string());
    // Used: refused as a revoked or made-up token is, word for word.
    let (s, used) = d.call(&token, Method::GET, "/v1/workspace/projects", None).await;
    let (_, unknown) = d.call(&roll_access::new_secret("rlt_"), Method::GET, "/v1/workspace/projects", None).await;
    assert_eq!((s, &used), (StatusCode::UNAUTHORIZED, &unknown));
    // It's on the token and in the audit log; the admins and its owner are told.
    let (_, got) = d.call(OPERATOR, Method::GET, &format!("/v1/access/tokens/{id}"), None).await;
    assert!(got["last_used_at"].is_i64(), "{got}");
    let (_, log) = d.call(OPERATOR, Method::GET, "/v1/access/audit?limit=50", None).await;
    assert!(log.as_array().unwrap().iter().any(|e| e["source"] == "decoy" && e["grant_id"] == id.as_str() && e["route"] == "/v1/workspace/projects"), "{log}");
    let mut told = Vec::new();
    for _ in 0..100 {
        told = d.people.1.lock().unwrap().clone();
        if told.iter().any(|t| t.starts_with("usr_root: A decoy token was used")) && told.iter().any(|t| t.starts_with("usr_alice: A decoy token was used")) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(told.iter().any(|t| t.starts_with("usr_root: A decoy token was used")) && told.iter().any(|t| t.starts_with("usr_alice: A decoy token was used")), "{told:?}");
}
