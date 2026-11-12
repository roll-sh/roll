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

struct Daemon {
    router: Router,
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
}

impl Daemon {
    fn new(wait_ms: u64) -> Daemon {
        let dir = tempfile::tempdir().unwrap();
        let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
        let workspace = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
        let mut resolvers = Resolvers::default();
        resolvers.by_kind.insert("endpoint", Arc::new(Endpoints));
        resolvers.workspace = Some(Arc::new(WorkspaceResolver(workspace.clone())));
        let config = Config { locked: true, approval_wait: std::time::Duration::from_millis(wait_ms), ..Config::default() };
        let access = Access::open(&code_sql::Target::Sqlite(dir.path().join("access.db")), Default::default(), config, resolvers).unwrap();
        let gate = roll_auth::Gate::new(Some(roll_auth::AdminToken::new(OPERATOR).unwrap()), Vec::new());
        gate.set_authorizer(Arc::new(roll_access::evaluate::Evaluator(access.clone())));
        let gated = postgres()
            .merge(roll_workspace::router(workspace.clone()))
            .merge(roll_access::api::router(access.clone()))
            .layer(axum::middleware::from_fn_with_state(gate, roll_auth::check));
        let router = roll_access::oauth::public_router(access.clone()).merge(gated);
        Daemon { router, workspace, _dir: dir }
    }

    async fn call(&self, token: &str, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.send(token, method, path, body, &[]).await
    }

    async fn send(&self, token: &str, method: Method, path: &str, body: Option<Value>, headers: &[(&str, &str)]) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path).header("content-type", "application/json");
        if !token.is_empty() {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let req = req.body(body.map_or(Body::empty(), |b| Body::from(b.to_string()))).unwrap();
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
/// operator's, the wrong address refused, read-only SQL decided by its body, and a revoked token
/// dead on its next request.
#[tokio::test]
async fn a_scoped_token_does_only_what_its_rules_say() {
    let d = Daemon::new(50);
    let project = d.workspace.create("billing", "", None).unwrap();
    d.workspace.place(&project.id, roll_workspace::Kind::Postgres, "pa", 0.0, 0.0).unwrap();
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

    // The operator's routes stay the operator's, and the token's own info is its own.
    let (s, me) = d.call(&token, Method::GET, "/v1/access", None).await;
    assert_eq!((s, me["grant"]["id"].as_str()), (StatusCode::OK, Some(grant.as_str())));
    assert_eq!(d.call(&token, Method::GET, "/v1/access/tokens", None).await.0, StatusCode::FORBIDDEN);

    // Moving the database out of the project takes it out of reach at once.
    d.workspace.remove(&project.id, roll_workspace::Kind::Postgres, "pa").unwrap();
    let (_, list) = d.call(&token, Method::GET, "/v1/projects", None).await;
    assert_eq!(ids(&list), Vec::<&str>::new());

    // An address outside the allowlist (an in-process request has none) is refused.
    let (fenced, _) = d.token(json!([{ "product": "*", "resources": "*", "actions": { "read": "allow" } }]), json!({ "ip_allow": ["10.0.0.0/8"] })).await;
    assert_eq!(d.call(&fenced, Method::GET, "/v1/projects", None).await.1["code"], "ip_not_allowed");

    // Revoked: dead on the next request; the audit log has every decision.
    assert_eq!(d.call(OPERATOR, Method::DELETE, &format!("/v1/access/tokens/{grant}"), None).await.0, StatusCode::OK);
    assert_eq!(d.call(&token, Method::GET, "/v1/projects", None).await.0, StatusCode::UNAUTHORIZED);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (_, log) = d.call(OPERATOR, Method::GET, &format!("/v1/access/audit?grant={grant}"), None).await;
    let decisions: Vec<_> = log.as_array().unwrap().iter().map(|e| e["decision"].as_str().unwrap()).collect();
    assert!(decisions.contains(&"allow") && decisions.contains(&"deny"), "{log}");
    assert!(log.as_array().unwrap().iter().all(|e| e["route"].as_str().unwrap().starts_with('/')), "routes, not paths: {log}");
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
    assert_eq!(make(json!([{ "product": "postgres", "resources": "*", "actions": { "write": "allow" } }]), Some(end)).await.0, StatusCode::FORBIDDEN);
    assert_eq!(make(read.clone(), None).await.0, StatusCode::FORBIDDEN, "outlives its maker");
    assert_eq!(make(read, Some(end - 1000)).await.0, StatusCode::CREATED);

    let project = d.workspace.create("app", "", None).unwrap();
    let (scoped, _) = d.token(json!([{ "product": "postgres", "resources": [{ "project": project.id }], "actions": { "write": "allow", "read": "allow" } }]), json!({})).await;
    assert_eq!(d.call(&scoped, Method::POST, "/v1/projects", Some(json!({ "name": "x" }))).await.0, StatusCode::FORBIDDEN);
    let (s, made) = d.send(&scoped, Method::POST, "/v1/projects", Some(json!({ "name": "x" })), &[("x-roll-project", &project.id)]).await;
    assert_eq!((s, made["password"].as_str()), (StatusCode::CREATED, Some("[redacted]")));
    assert_eq!(d.workspace.project_of("postgres", "pnew").unwrap().as_deref(), Some(project.id.as_str()));
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
