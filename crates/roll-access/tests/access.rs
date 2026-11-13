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

struct Daemon {
    router: Router,
    access: Arc<Access>,
    workspace: Arc<roll_workspace::Workspace>,
    _dir: tempfile::TempDir,
}

/// What the stand-in products saw: the operator's marker and a grant.
fn seen(ext: &axum::http::Extensions) -> Value {
    json!({ "operator": roll_auth::Operator::on(ext), "grant": roll_auth::Grant::on(ext).map(|g| g.id.to_string()) })
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
        .route("/v1/apps/services/{id}/links/{kind}/{ref}", axum::routing::put(|| async { axum::Json(json!({ "ok": true })) }))
        .route("/v1/apps/services/{id}/environments/{env}", axum::routing::put(|| async { axum::Json(json!({ "ok": true })) }))
        .route("/v1/search/clusters/{id}/indices/{index}/search", post(|| async { axum::Json(json!({ "hits": [] })) }))
        .route("/v1/search/clusters/{id}/indices", get(|| async { axum::Json(json!([])) }))
        .route("/v1/search/clusters/{id}/request", post(|| async { axum::Json(json!({})) }))
        .route("/v1/search/clusters/{id}/indices/{index}/bulk", post(|body: String| async move { axum::Json(json!({ "lines": body.lines().count() })) }))
        .route("/v1/secrets/projects/{project}/environments/{env}/secrets", get(|| async { axum::Json(json!([])) }))
        .route("/v1/secrets/projects/{project}/health", get(|| async { axum::Json(json!({ "findings": [] })) }))
        .route("/v1/secrets/projects/{project}/environments/{env}/values", get(|| async { axum::Json(json!({})) }))
        .route("/v1/secrets/projects/{project}/environments/{env}/promote", axum::routing::post(|| async { axum::Json(json!({})) }))
        .route("/v1/secrets/projects/{project}/environments/{env}/imports", axum::routing::put(|| async { axum::Json(json!({})) }))
}

impl Daemon {
    fn new(wait_ms: u64) -> Daemon {
        Daemon::with(wait_ms, |_| {})
    }

    fn with(wait_ms: u64, configure: impl FnOnce(&mut Config)) -> Daemon {
        let dir = tempfile::tempdir().unwrap();
        let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
        let workspace = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
        let mut resolvers = Resolvers::default();
        resolvers.by_kind.insert("endpoint", Arc::new(Endpoints));
        resolvers.by_kind.insert("default-branch", Arc::new(DefaultBranch));
        resolvers.workspace = Some(Arc::new(WorkspaceResolver(workspace.clone())));
        // Metadata documents from 127.0.0.1 (the test's own server) are allowed here only.
        let mut config = Config { locked: true, approval_wait: std::time::Duration::from_millis(wait_ms), metadata_allow_private: true, ..Config::default() };
        configure(&mut config);
        let access = Access::with_db(code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("access.db")), "access").unwrap(), config, resolvers).unwrap();
        let public = roll_auth::Rule { route: "/v1/open", access: roll_auth::Access::Public("a shared link") };
        let gate = roll_auth::Gate::new(Some(roll_auth::AdminToken::new(OPERATOR).unwrap()), vec![public]);
        gate.set_authorizer(Arc::new(roll_access::evaluate::Evaluator(access.clone())));
        let gated = postgres()
            .merge(roll_workspace::router(workspace.clone()))
            .merge(roll_access::api::router(access.clone()))
            .layer(axum::middleware::from_fn_with_state(gate, roll_auth::check));
        let router = roll_access::oauth::public_router(access.clone()).merge(gated);
        let access = access.clone();
        Daemon { router, access, workspace, _dir: dir }
    }

    async fn call(&self, token: &str, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.send(token, method, path, body, &[]).await
    }

    async fn send(&self, token: &str, method: Method, path: &str, body: Option<Value>, headers: &[(&str, &str)]) -> (StatusCode, Value) {
        let body = body.map_or(Body::empty(), |b| Body::from(b.to_string()));
        self.raw(token, method, path, body, headers, None).await
    }

    /// A request with any body, and (a signed hop from another node) a `Forwarded` extension.
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

    // Secrets are per project and environment: a rule for the project's development environment
    // reaches those and only those, names but not values.
    let (secrets, _) = d
        .token(json!([{ "product": "secrets", "resources": [{ "project": project.id }], "environments": ["development"], "actions": { "read": "allow" } }]), json!({}))
        .await;
    let at = |env: &str, rest: &str| format!("/v1/secrets/projects/{}/environments/{env}/{rest}", project.id);
    assert_eq!(d.call(&secrets, Method::GET, &at("development", "secrets"), None).await.0, StatusCode::OK);
    assert_eq!(d.call(&secrets, Method::GET, &at("production", "secrets"), None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(d.call(&secrets, Method::GET, &at("development", "values"), None).await.0, StatusCode::FORBIDDEN);
    // Health spans every environment: refused until secrets filters it by the token's grant.
    assert_eq!(d.call(&secrets, Method::GET, &format!("/v1/secrets/projects/{}/health", project.id), None).await.0, StatusCode::FORBIDDEN);

    // Rotated: the old secret is dead at once, the new one has the same rules.
    let (s, rotated) = d.call(OPERATOR, Method::POST, &format!("/v1/access/tokens/{grant}/rotate"), None).await;
    assert_eq!(s, StatusCode::OK, "{rotated}");
    assert_eq!(d.call(&token, Method::GET, "/v1/projects", None).await.0, StatusCode::UNAUTHORIZED);
    let token = rotated["token"].as_str().unwrap().to_string();
    assert_eq!(d.call(&token, Method::GET, "/v1/projects", None).await.0, StatusCode::OK);

    // Revoked: dead on the next request; the audit log has every decision.
    assert_eq!(d.call(OPERATOR, Method::DELETE, &format!("/v1/access/tokens/{grant}"), None).await.0, StatusCode::OK);
    assert_eq!(d.call(&token, Method::GET, "/v1/projects", None).await.0, StatusCode::UNAUTHORIZED);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (_, log) = d.call(OPERATOR, Method::GET, &format!("/v1/access/audit?grant={grant}"), None).await;
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
    let d = Daemon::new(50);
    let (s, meta) = d.call("", Method::GET, "/.well-known/oauth-authorization-server", None).await;
    assert_eq!((s, meta["code_challenge_methods_supported"][0].as_str()), (StatusCode::OK, Some("S256")));
    let (s, pr) = d.call("", Method::GET, "/.well-known/oauth-protected-resource/v1/mcp", None).await;
    assert_eq!((s, pr["resource"].as_str()), (StatusCode::OK, Some("http://127.0.0.1:7070/v1/mcp")));

    let redirect = "http://127.0.0.1:43111/callback";
    let (s, client) = d.call("", Method::POST, "/v1/access/oauth/register", Some(json!({ "client_name": "Claude Code", "redirect_uris": [redirect] }))).await;
    assert_eq!(s, StatusCode::CREATED, "{client}");
    assert!(client.as_object().unwrap().values().all(|v| !v.is_null()), "RFC 7591: unset fields are left out: {client}");
    let client_id = client["client_id"].as_str().unwrap();
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
    assert_eq!(d.call(&access, Method::GET, "/v1/projects", None).await.0, StatusCode::OK);
    assert_eq!(d.call(&access, Method::DELETE, "/v1/projects/pa", None).await.0, StatusCode::FORBIDDEN);

    let refresh = tokens["refresh_token"].as_str().unwrap();
    assert_eq!(d.call(refresh, Method::GET, "/v1/projects", None).await.0, StatusCode::UNAUTHORIZED, "a refresh token opens nothing");
    let (s, next) = d.form("/v1/access/oauth/token", &[("grant_type", "refresh_token"), ("refresh_token", refresh), ("client_id", client_id)]).await;
    assert_eq!(s, StatusCode::OK, "{next}");
    let reuse = d.form("/v1/access/oauth/token", &[("grant_type", "refresh_token"), ("refresh_token", refresh), ("client_id", client_id)]).await;
    assert_eq!(reuse.1["error"], "invalid_grant", "refresh tokens rotate");

    let grant = next["grant_id"].as_str().unwrap();
    d.call(OPERATOR, Method::DELETE, &format!("/v1/access/tokens/{grant}"), None).await;
    let (s, _) = d.call(next["access_token"].as_str().unwrap(), Method::GET, "/v1/projects", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
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
    let hook = roll_access::new_secret("rlh_");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let s2 = seen.clone();
    d.access.on_leaked("rlh_", move |secret, why| {
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
    assert_eq!(approve(apr.clone(), json!("1h")).await.0, StatusCode::OK);
    assert_eq!(d.send(&token, Method::DELETE, "/v1/projects/pa", None, &[("x-roll-approval", &apr)]).await.0, StatusCode::OK);
    let hour = apr.clone();
    // For an hour: the same request runs without asking; another project still asks.
    assert_eq!(ask("pa").await.0, StatusCode::OK);
    assert_eq!(ask("pb").await.1["code"], "approval_pending");
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
    // Taken back before the hour is up: asking again.
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
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (_, ua) = d.call(OPERATOR, Method::GET, "/v1/access/audit?resource=cl_1/logs-ua", None).await;
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

    let (both, both_id) = d.token(json!([
        { "product": "apps", "resources": "*", "actions": { "write": "allow", "deploy": "allow" } },
        { "product": "postgres", "resources": [{ "id": "prj_x" }], "actions": { "reveal": "allow" } },
        { "product": "postgres", "resources": [{ "id": "prj_y" }, { "id": "prj_z" }], "actions": { "reveal": "ask" } },
    ]), json!({})).await;
    assert_eq!(link(&both, "postgres", "prj_x").await.0, StatusCode::OK);
    // The link's reveal is what the token used too: tightening keeps it.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (_, t) = d.call(OPERATOR, Method::GET, &format!("/v1/access/tokens/{both_id}/tighten"), None).await;
    assert!(t["used"].as_array().unwrap().iter().any(|u| u["product"] == "postgres" && u["action"] == "reveal" && u["resource"] == "prj_x"), "{t}");
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
