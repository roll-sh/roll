//! Linear (docs/code/automations.md, "Linear") against a mock of Linear's GraphQL API and OAuth
//! endpoints (no real network): connecting with an API key and with roll's OAuth app (PKCE,
//! sealed tokens, refresh, revoke), webhook signatures, the replay window and idempotency, agent
//! sessions and replies, every step kind, and the session bridge driving a real daemon whose
//! fake provider asks for approval (answered from Linear).

mod common;

use agent_host::Host;
use agent_host::config::Config as HostConfig;
use agent_host::provider::Provider;
use agent_host::provider::fake::{Call, FakeProvider, Script};
use agent_host::relay::{LinkConfig, LinkEvent};
use agent_proto::automation::{AutomationId, AutomationRunId, Step};
use agent_proto::{Capabilities, Decision, Response};
use axum::http::StatusCode;
use code_control::agents::automations::{OwnerRef, StepContext, StepOutcome};
use code_control::agents::linear::LinearConfig;
use code_control::agents::linear::webhook::sign;
use common::{App, app_with};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const ADMIN: &str = "admin-secret";
const KEY: &str = "lin_api_good";
const APP_SECRET: &str = "app-signing-secret";

/// A mock of Linear: `POST /graphql` (by operation name), `POST /oauth/token`, `POST /oauth/revoke`.
mod mock {
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::{Json, Router};
    use base64::Engine;
    use parking_lot::Mutex;
    use serde_json::{Value, json};
    use std::collections::HashSet;
    use std::sync::Arc;

    #[derive(Default)]
    pub struct State_ {
        /// (operation, variables, the Authorization header).
        pub calls: Vec<(String, Value, String)>,
        /// Token endpoint forms.
        pub token_calls: Vec<Vec<(String, String)>>,
        pub revoked: Vec<String>,
        /// Access tokens Linear takes now.
        pub access: Vec<String>,
        pub refresh: Option<String>,
        pub refresh_refused: bool,
        pub issued: u32,
        /// PKCE: the challenge the authorize URL carried.
        pub challenge: Option<String>,
        /// The webhook roll made with an API key: its secret.
        pub webhook_secret: Option<String>,
        /// An API key that may not make webhooks.
        pub no_admin: bool,
        pub ids: HashSet<String>,
    }

    pub type Shared = Arc<Mutex<State_>>;

    pub struct Mock {
        pub url: String,
        pub s: Shared,
    }

    impl Mock {
        pub fn ops(&self, op: &str) -> Vec<Value> {
            self.s.lock().calls.iter().filter(|(o, _, _)| o == op).map(|(_, v, _)| v.clone()).collect()
        }

        /// The agent activities posted, as their `input`s.
        pub fn activities(&self) -> Vec<Value> {
            self.ops("RollActivity").into_iter().map(|v| v["input"].clone()).collect()
        }
    }

    pub async fn start() -> Mock {
        let s: Shared = Arc::default();
        let app = Router::new().route("/graphql", post(graphql)).route("/oauth/token", post(token)).route("/oauth/revoke", post(revoke)).with_state(s.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        Mock { url, s }
    }

    fn op_of(query: &str) -> String {
        let q = query.trim_start();
        let rest = q.strip_prefix("query ").or_else(|| q.strip_prefix("mutation ")).unwrap_or(q);
        rest.chars().take_while(|c| c.is_ascii_alphanumeric()).collect()
    }

    fn err(msg: &str) -> Response {
        Json(json!({ "errors": [{ "message": msg }] })).into_response()
    }

    fn states() -> Value {
        json!([
            { "id": "st-backlog", "name": "Backlog", "type": "backlog", "position": 0 },
            { "id": "st-todo", "name": "Todo", "type": "unstarted", "position": 1 },
            { "id": "st-review", "name": "In Review", "type": "started", "position": 3 },
            { "id": "st-progress", "name": "In Progress", "type": "started", "position": 2 },
            { "id": "st-done", "name": "Done", "type": "completed", "position": 4 },
        ])
    }

    async fn graphql(State(s): State<Shared>, h: HeaderMap, Json(b): Json<Value>) -> Response {
        let auth = h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        let op = op_of(b["query"].as_str().unwrap_or_default());
        let v = b["variables"].clone();
        let mut st = s.lock();
        let key = auth == super::KEY;
        let ok = key || auth.strip_prefix("Bearer ").is_some_and(|t| st.access.iter().any(|a| a == t));
        if !ok {
            return (StatusCode::UNAUTHORIZED, Json(json!({ "errors": [{ "message": "Authentication required", "extensions": { "code": "AUTHENTICATION_ERROR" } }] }))).into_response();
        }
        st.calls.push((op.clone(), v.clone(), auth.clone()));
        let input = &v["input"];
        let mut once = |id: &str| -> bool { st.ids.insert(id.to_string()) };
        let data = match op.as_str() {
            "RollViewer" => json!({
                "viewer": if key { json!({ "id": "user-key", "name": "Ada Lovelace", "displayName": "ada" }) } else { json!({ "id": "app-user", "name": "roll", "displayName": "roll" }) },
                "organization": { "id": "org-1", "name": "Acme", "urlKey": "acme" },
            }),
            "RollWebhook" => {
                if st.no_admin {
                    return err("Only admins can create webhooks");
                }
                st.webhook_secret = input["secret"].as_str().map(str::to_string);
                json!({ "webhookCreate": { "success": true, "webhook": { "id": "wh-1", "enabled": true } } })
            }
            "RollWebhookDelete" => json!({ "webhookDelete": { "success": true } }),
            "RollTeams" => json!({ "teams": { "nodes": [{ "id": "team-1", "key": "ENG", "name": "Engineering" }, { "id": "team-2", "key": "OPS", "name": "Operations" }] } }),
            "RollIssue" | "RollIssueById" => json!({ "issue": {
                "id": "issue-1", "identifier": "ENG-12", "title": "Fix login", "url": "https://linear.app/acme/issue/ENG-12",
                "priority": 2, "team": { "id": "team-1", "key": "ENG", "name": "Engineering" },
                "state": { "id": "st-todo", "name": "Todo", "type": "unstarted" }, "labels": { "nodes": [] }, "assignee": null,
            } }),
            "RollStates" => json!({ "workflowStates": { "nodes": states() } }),
            "RollLabels" => {
                let n = v["name"].as_str().unwrap_or_default();
                json!({ "issueLabels": { "nodes": [{ "id": format!("lab-{}", n.to_lowercase()), "name": n, "team": null }] } })
            }
            "RollUsers" => {
                let q = v["q"].as_str().unwrap_or_default().to_lowercase();
                let nodes = if q.contains("ada") { json!([{ "id": "user-ada", "name": "Ada" }]) } else { json!([]) };
                json!({ "users": { "nodes": nodes } })
            }
            "RollComment" => {
                let id = input["id"].as_str().unwrap_or("c").to_string();
                if !once(&id) {
                    return err("Entity already exists");
                }
                json!({ "commentCreate": { "success": true, "comment": { "id": id, "url": format!("https://linear.app/acme/comment/{id}") } } })
            }
            "RollIssueUpdate" => {
                let state = states().as_array().unwrap().iter().find(|s| s["id"] == input["stateId"]).map(|s| s["name"].clone()).unwrap_or(json!("Todo"));
                json!({ "issueUpdate": { "success": true, "issue": { "id": "issue-1", "identifier": "ENG-12", "url": "https://linear.app/acme/issue/ENG-12", "state": { "name": state } } } })
            }
            "RollIssueCreate" => {
                let id = input["id"].as_str().unwrap_or("i").to_string();
                if !once(&id) {
                    return err("Entity already exists");
                }
                json!({ "issueCreate": { "success": true, "issue": { "id": id, "identifier": "ENG-13", "url": "https://linear.app/acme/issue/ENG-13" } } })
            }
            "RollAttach" => json!({ "attachmentCreate": { "success": true, "attachment": { "id": "att-1", "url": input["url"] } } }),
            "RollSessionLink" | "RollSessionUrl" | "RollSessionPlan" => json!({ "agentSessionUpdate": { "success": true } }),
            "RollActivity" => {
                if let Some(id) = input["id"].as_str()
                    && !once(id)
                {
                    st.calls.pop();
                    return err("Entity already exists");
                }
                json!({ "agentActivityCreate": { "success": true, "agentActivity": { "id": input["id"].as_str().unwrap_or("act") } } })
            }
            other => return err(&format!("the mock has no {other}")),
        };
        Json(json!({ "data": data })).into_response()
    }

    async fn token(State(s): State<Shared>, body: String) -> Response {
        let form: Vec<(String, String)> = url::form_urlencoded::parse(body.as_bytes()).into_owned().collect();
        let get = |k: &str| form.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone()).unwrap_or_default();
        let mut st = s.lock();
        st.token_calls.push(form.clone());
        if get("client_id") != "cid" || get("client_secret") != "csecret" {
            return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "invalid_client" }))).into_response();
        }
        let refused = |why: &str| (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid_grant", "error_description": why }))).into_response();
        match get("grant_type").as_str() {
            "authorization_code" => {
                use sha2::Digest;
                let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(get("code_verifier").as_bytes()));
                if get("code") != "good-code" || st.challenge.as_deref() != Some(challenge.as_str()) {
                    return refused("bad code or verifier");
                }
            }
            "refresh_token" => {
                if st.refresh_refused || st.refresh.as_deref() != Some(get("refresh_token").as_str()) {
                    return refused("refresh token revoked");
                }
                // The old access token stops working.
                st.access.clear();
            }
            _ => return refused("grant"),
        }
        st.issued += 1;
        let (at, rt) = (format!("at-{}", st.issued), format!("rt-{}", st.issued));
        st.access.push(at.clone());
        st.refresh = Some(rt.clone());
        Json(json!({ "access_token": at, "token_type": "Bearer", "expires_in": 86399, "scope": "read write issues:create comments:create app:assignable app:mentionable", "refresh_token": rt })).into_response()
    }

    async fn revoke(State(s): State<Shared>, body: String) -> StatusCode {
        let form: Vec<(String, String)> = url::form_urlencoded::parse(body.as_bytes()).into_owned().collect();
        let mut st = s.lock();
        if let Some((_, t)) = form.into_iter().find(|(k, _)| k == "token") {
            st.access.retain(|a| *a != t);
            st.revoked.push(t);
        }
        StatusCode::OK
    }
}

struct World {
    a: App,
    mock: mock::Mock,
    /// Where the app is served (daemons connect here; git clones come from `{url}/git`).
    url: String,
    owner: String,
    agent: String,
    owner_id: String,
}

async fn world() -> World {
    let mock = mock::start().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let git_url = format!("{url}/git");
    let a = app_with(move |c| {
        c.admin_token = Some(ADMIN.into());
        c.review.secret_key = Some("a-test-secret-key-that-is-long-enough".into());
        c.git_url = git_url;
    });
    let router = a.router.clone();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    a.code.configure_linear(LinearConfig {
        client_id: Some("cid".into()),
        client_secret: Some("csecret".into()),
        webhook_secret: Some(APP_SECRET.into()),
        api_url: mock.url.clone(),
        authorize_url: format!("{}/oauth/authorize", mock.url),
        redirect_uri: None,
    });
    a.code.automations.set_tick(Duration::from_millis(50));
    a.code.start_automations();
    let (s, v) = a.req("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" })), Some(ADMIN)).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let token = |name: &'static str| {
        let a = &a;
        async move {
            let (s, t) = a.req("POST", "/v1/code/owners/acme/tokens", Some(json!({ "name": name })), Some(ADMIN)).await;
            assert_eq!(s, StatusCode::CREATED, "{t}");
            t["token"].as_str().unwrap().to_string()
        }
    };
    let owner = token("me").await;
    let agent = token("agent:bot").await;
    let owner_id = a.code.db.owner("acme").unwrap().unwrap().id;
    World { a, mock, url, owner, agent, owner_id }
}

impl World {
    async fn req(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.a.req(method, path, body, Some(&self.owner)).await
    }

    /// A delivery as Linear sends it, signed with `secret`, to `path`.
    async fn deliver(&self, path: &str, secret: &str, mut payload: Value) -> (StatusCode, Value) {
        if payload.get("webhookTimestamp").is_none() {
            payload["webhookTimestamp"] = json!(code_sql::now_ms());
        }
        let body = payload.to_string();
        self.deliver_raw(path, &body, &sign(secret, body.as_bytes())).await
    }

    async fn deliver_raw(&self, path: &str, body: &str, signature: &str) -> (StatusCode, Value) {
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .header("linear-delivery", "d-1")
            .header("linear-event", "Issue")
            .header("linear-signature", signature)
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        let resp = self.a.send(req).await;
        let s = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (s, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// The owner's inbox: (key, kind, data), oldest first.
    fn inbox(&self) -> Vec<(String, String, Value)> {
        self.a
            .code
            .db
            .sql()
            .query("SELECT key, kind, data FROM automation_events WHERE owner_id = ? ORDER BY at, id", [self.owner_id.as_str()], |r| {
                let d: String = r.get(2)?;
                Ok((r.get(0)?, r.get(1)?, serde_json::from_str(&d).unwrap()))
            })
            .unwrap()
    }

    /// Install roll's app by OAuth, as the owner's browser would.
    async fn install(&self) {
        let (s, v) = self.req("POST", "/v1/code/agents/acme/linear/oauth", Some(json!({ "return_to": "/dashboard/agents#agents/settings/owner" }))).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let url = url::Url::parse(v["url"].as_str().unwrap()).unwrap();
        let q: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
        self.mock.s.lock().challenge = Some(q["code_challenge"].clone());
        let (s, _, loc) = self.callback(&format!("code=good-code&state={}", q["state"])).await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        assert_eq!(loc, "https://roll.sh/dashboard/agents#agents/settings/owner?linear=connected");
    }

    async fn callback(&self, query: &str) -> (StatusCode, Value, String) {
        let req = axum::http::Request::builder().uri(format!("/v1/code/linear/oauth/callback?{query}")).body(axum::body::Body::empty()).unwrap();
        let resp = self.a.send(req).await;
        let s = resp.status();
        let loc = resp.headers().get("location").map(|l| l.to_str().unwrap().to_string()).unwrap_or_default();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (s, serde_json::from_slice(&bytes).unwrap_or(Value::Null), loc)
    }

    /// Delete the owner's automations (the Linear agent an install adds), for tests that drive
    /// what it would do by hand.
    async fn drop_automations(&self) {
        let (_, list) = self.req("GET", "/v1/code/agents/acme/automations", None).await;
        for a in list.as_array().unwrap() {
            let (s, _) = self.req("DELETE", &format!("/v1/code/agents/acme/automations/{}", a["id"].as_str().unwrap()), None).await;
            assert_eq!(s, StatusCode::NO_CONTENT);
        }
    }

    /// One attempt of a step kind, as the engine would make it.
    async fn step(&self, kind: &str, config: Value, ctx: Value, run: &str, state: Value) -> (StepOutcome, Value) {
        let k = self.a.code.automations.registry.step(kind).unwrap_or_else(|| panic!("no step kind {kind}"));
        let step: Step = serde_json::from_value(json!({ "id": "s1", "kind": kind, "config": config })).unwrap();
        let mut c = StepContext::new(
            self.a.code.clone(),
            OwnerRef { id: self.owner_id.clone(), name: "acme".into() },
            AutomationId("aut_linear".into()),
            "Linear agent".into(),
            None,
            AutomationRunId(run.into()),
            step,
            1,
            ctx,
            state,
            code_sql::now_ms(),
        );
        let o = k.run(&mut c).await;
        (o, c.state().clone())
    }
}

fn issue_payload(action: &str, updated_from: Value) -> Value {
    json!({
        "type": "Issue", "action": action, "organizationId": "org-1", "webhookId": "wh-1", "url": "https://linear.app/acme/issue/ENG-12",
        "actor": { "id": "user-ada", "type": "user", "name": "Ada" },
        "data": {
            "id": "issue-1", "identifier": "ENG-12", "title": "Fix login", "description": "It crashes", "priority": 1.0,
            "teamId": "team-1", "team": { "id": "team-1", "key": "ENG", "name": "Engineering" },
            "stateId": "st-progress", "state": { "id": "st-progress", "name": "In Progress", "type": "started" },
            "labelIds": ["lab-bug", "lab-roll"], "labels": [{ "id": "lab-bug", "name": "Bug" }, { "id": "lab-roll", "name": "roll" }],
            "updatedAt": "2026-11-04T10:00:00.000Z",
        },
        "updatedFrom": updated_from,
    })
}

fn session_payload(action: &str, session: &str, activity: Option<(&str, &str, Option<&str>)>) -> Value {
    let mut p = json!({
        "type": "AgentSessionEvent", "action": action, "organizationId": "org-1", "oauthClientId": "cid", "appUserId": "app-user",
        "agentSession": {
            "id": session, "status": "pending", "url": format!("https://linear.app/acme/agent/{session}"),
            "issueId": "issue-1", "commentId": "comment-root",
            "issue": { "id": "issue-1", "identifier": "ENG-12", "title": "Fix login", "url": "https://linear.app/acme/issue/ENG-12", "teamId": "team-1", "team": { "id": "team-1", "key": "ENG", "name": "Engineering" } },
            "comment": { "id": "comment-root", "body": "@roll please fix this" },
            "creator": { "id": "user-ada", "name": "Ada" },
        },
        "promptContext": "<issue identifier=\"ENG-12\"><title>Fix login</title></issue>",
    });
    if let Some((id, body, signal)) = activity {
        p["agentActivity"] = json!({ "id": id, "agentSessionId": session, "content": { "type": "prompt", "body": body }, "signal": signal, "user": { "id": "user-ada", "name": "Ada" } });
    }
    p
}

#[tokio::test]
async fn an_api_key_connects_and_its_webhook_is_verified() {
    let w = world().await;
    let (s, v) = w.req("GET", "/v1/code/agents/acme/linear", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["connected"], false);
    assert_eq!(v["app"]["oauth"], true);
    let (s, _) = w.a.req("PUT", "/v1/code/agents/acme/linear/key", Some(json!({ "key": KEY })), Some(&w.agent)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "agent tokens don't manage the owner's Linear");

    let (s, v) = w.req("PUT", "/v1/code/agents/acme/linear/key", Some(json!({ "key": "lin_api_wrong" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a key Linear refuses: {v}");
    let (s, v) = w.req("PUT", "/v1/code/agents/acme/linear/key", Some(json!({ "key": KEY }))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let c = &v["connection"];
    assert_eq!(c["kind"], "api_key");
    assert_eq!(c["workspace"]["name"], "Acme");
    assert_eq!(c["agent"], false, "an API key can't be an agent");
    assert_eq!(c["webhook"]["mode"], "workspace", "roll made the webhook itself");
    assert_eq!(c["webhook"]["status"], "waiting");
    assert!(!v.to_string().contains(KEY), "the key is never returned");
    let secret = w.mock.s.lock().webhook_secret.clone().expect("roll gave Linear a signing secret");
    let made = &w.mock.ops("RollWebhook")[0]["input"];
    let hook_url = c["webhook"]["url"].as_str().unwrap();
    assert_eq!(made["url"], hook_url);
    assert_eq!(made["resourceTypes"], json!(["Issue", "Comment"]));
    let path = hook_url.trim_start_matches("https://api.roll.sh").to_string();
    assert!(path.starts_with("/v1/code/linear/webhook/lwh_"), "{path}");

    // A delivery signed with the secret goes into the inbox once.
    let p = issue_payload("update", json!({ "stateId": "st-todo", "labelIds": ["lab-bug"] }));
    let (s, v) = w.deliver(&path, &secret, p.clone()).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["recorded"], 1);
    let inbox = w.inbox();
    assert_eq!(inbox.len(), 1);
    let (key, kind, data) = &inbox[0];
    assert_eq!(key, "linear:issue.updated:issue-1:2026-11-04T10:00:00.000Z");
    assert_eq!(kind, "issue.updated");
    assert_eq!(data["issue"]["identifier"], "ENG-12");
    assert_eq!(data["changes"]["state"]["to"], "In Progress");
    assert_eq!(data["changes"]["labels_added"], json!(["roll"]));
    assert_eq!(data["workspace"]["url_key"], "acme");

    // The same delivery again (Linear's retry, a replay within the window): recorded once.
    let (s, v) = w.deliver(&path, &secret, p.clone()).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!((v["recorded"].as_u64(), v["duplicates"].as_u64()), (Some(0), Some(1)));
    assert_eq!(w.inbox().len(), 1);

    // A bad signature, and one replayed after the window: 401, and the status says so.
    let body = p.to_string();
    let (s, _) = w.deliver_raw(&path, &body, &sign("not-the-secret", body.as_bytes())).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let mut old = p.clone();
    old["webhookTimestamp"] = json!(code_sql::now_ms() - 120_000);
    let (s, _) = w.deliver(&path, &secret, old).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "outside the replay window");
    let (s, _) = w.deliver_raw(&path, &body, "").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "no signature");
    let (_, v) = w.req("GET", "/v1/code/agents/acme/linear", None).await;
    assert_eq!(v["connection"]["webhook"]["status"], "failing");
    assert!(v["connection"]["webhook"]["rejected_why"].as_str().unwrap().contains("webhookTimestamp") || v["connection"]["webhook"]["rejected_why"].as_str().unwrap().contains("Signature"));
    assert_eq!(v["deliveries"][0]["result"], "duplicate");
    assert_eq!(v["deliveries"][1]["result"], "recorded");
    assert_eq!(v["deliveries"][1]["summary"], "ENG-12 updated");
    let (s, _) = w.deliver("/v1/code/linear/webhook/lwh_nope", &secret, issue_payload("create", Value::Null)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Comments: mentions and the issue's context.
    let comment = json!({
        "type": "Comment", "action": "create", "organizationId": "org-1", "url": "https://linear.app/acme/issue/ENG-12#comment-9",
        "data": { "id": "comment-9", "body": "@ada can you check", "issueId": "issue-1", "userId": "user-ada", "user": { "id": "user-ada", "name": "Ada" },
                  "issue": { "id": "issue-1", "identifier": "ENG-12", "title": "Fix login", "teamId": "team-1" } },
    });
    let (s, v) = w.deliver(&path, &secret, comment).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (key, kind, data) = w.inbox().pop().unwrap();
    assert_eq!((key.as_str(), kind.as_str()), ("linear:comment.created:comment-9", "comment.created"));
    assert_eq!(data["mentions_roll"], true, "the key's user is @ada");
    assert_eq!(data["comment"]["user"]["name"], "Ada");

    // Settings: a team's repo goes into its events.
    let (s, v) = w.req("PUT", "/v1/code/agents/acme/linear/settings", Some(json!({ "repos": { "ENG": "app" }, "default_repo": "acme/web" }))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["connection"]["settings"]["repos"]["ENG"], "app");
    let (s, v) = w.req("PUT", "/v1/code/agents/acme/linear/settings", Some(json!({ "repos": { "ENG": "a/b/c" } }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    w.deliver(&path, &secret, issue_payload("create", Value::Null)).await;
    let (_, kind, data) = w.inbox().pop().unwrap();
    assert_eq!(kind, "issue.created");
    assert_eq!(data["repo"], "app");
    let (s, v) = w.req("GET", "/v1/code/agents/acme/linear/teams", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["teams"][0]["key"], "ENG");

    // Disconnecting removes the webhook roll made.
    let (s, _) = w.req("DELETE", "/v1/code/agents/acme/linear", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(w.mock.ops("RollWebhookDelete")[0]["id"], "wh-1");
    let (_, v) = w.req("GET", "/v1/code/agents/acme/linear", None).await;
    assert_eq!(v["connected"], false);
    let (s, _) = w.deliver(&path, &secret, issue_payload("create", Value::Null)).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "the hook is gone");
}

#[tokio::test]
async fn a_key_that_cannot_make_webhooks_takes_a_hand_made_one() {
    let w = world().await;
    w.mock.s.lock().no_admin = true;
    let (s, v) = w.req("PUT", "/v1/code/agents/acme/linear/key", Some(json!({ "key": KEY }))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["connection"]["webhook"]["mode"], "manual");
    assert_eq!(v["connection"]["webhook"]["status"], "needs_secret");
    let path = v["connection"]["webhook"]["url"].as_str().unwrap().trim_start_matches("https://api.roll.sh").to_string();
    let (s, _) = w.deliver(&path, "lin_wh_handmade", issue_payload("create", Value::Null)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "no secret yet");
    let (s, v) = w.req("PUT", "/v1/code/agents/acme/linear/webhook/secret", Some(json!({ "secret": "lin_wh_handmade" }))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["connection"]["webhook"]["secret_hint"], "made");
    assert!(!v.to_string().contains("lin_wh_handmade"));
    let (s, v) = w.deliver(&path, "lin_wh_handmade", issue_payload("create", Value::Null)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["recorded"], 1);
}

#[tokio::test]
async fn roll_installs_as_a_linear_agent_and_refreshes_its_token() {
    let w = world().await;
    let (s, v) = w.req("POST", "/v1/code/agents/acme/linear/oauth", Some(json!({ "return_to": "//evil.example" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = w.req("POST", "/v1/code/agents/acme/linear/oauth", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let url = url::Url::parse(v["url"].as_str().unwrap()).unwrap();
    let q: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(url.path(), "/oauth/authorize");
    assert_eq!(q["client_id"], "cid");
    assert_eq!(q["actor"], "app");
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["redirect_uri"], "https://api.roll.sh/v1/code/linear/oauth/callback");
    for scope in ["app:assignable", "app:mentionable", "write"] {
        assert!(q["scope"].split(',').any(|s| s == scope), "{}", q["scope"]);
    }
    // A wrong verifier (the challenge isn't this state's) is refused; the state is spent either way.
    w.mock.s.lock().challenge = Some("someone-else".into());
    let (s, _, loc) = w.callback(&format!("code=good-code&state={}", q["state"])).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(loc.starts_with("https://roll.sh/dashboard?linear=error&reason="), "{loc}");
    let (_, _, loc) = w.callback(&format!("code=good-code&state={}", q["state"])).await;
    assert!(loc.contains("expired"), "a state works once: {loc}");

    w.install().await;
    let (_, v) = w.req("GET", "/v1/code/agents/acme/linear", None).await;
    let c = &v["connection"];
    assert_eq!(c["kind"], "oauth");
    assert_eq!(c["agent"], true);
    assert_eq!(c["user"]["id"], "app-user");
    assert_eq!(c["webhook"]["mode"], "app");
    assert!(!v.to_string().contains("at-1") && !v.to_string().contains("rt-1"), "tokens are never returned");
    let sealed: String = w.a.code.db.sql().query_row("SELECT token FROM linear_connections", (), |r| r.get(0)).unwrap();
    assert!(!sealed.contains("at-1"), "tokens are sealed at rest");

    // A token about to expire is refreshed before it's used, and the new one is kept.
    w.a.code.db.sql().execute("UPDATE linear_connections SET expires_at = ?", [code_sql::now_ms() + 60_000]).unwrap();
    let (s, v) = w.req("GET", "/v1/code/agents/acme/linear/teams", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let refreshes = |w: &World| w.mock.s.lock().token_calls.iter().filter(|f| f.iter().any(|(k, v)| k == "grant_type" && v == "refresh_token")).count();
    assert_eq!(refreshes(&w), 1);
    assert!(w.mock.s.lock().calls.last().unwrap().2.ends_with("at-2"), "the call used the new token");
    let (s, _) = w.req("GET", "/v1/code/agents/acme/linear/teams", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(refreshes(&w), 1, "a fresh token isn't refreshed again");

    // A token Linear stops taking (revoked elsewhere) is refreshed once and the call made again.
    w.mock.s.lock().access.clear();
    let (s, v) = w.req("GET", "/v1/code/agents/acme/linear/teams", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(refreshes(&w), 2);

    // A refresh token Linear refuses: the connection needs connecting again.
    {
        let mut m = w.mock.s.lock();
        m.access.clear();
        m.refresh_refused = true;
    }
    let (s, v) = w.req("GET", "/v1/code/agents/acme/linear/teams", None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let (_, v) = w.req("GET", "/v1/code/agents/acme/linear", None).await;
    assert_eq!(v["connection"]["status"], "reauth");

    // Installing again fixes it; disconnecting revokes the token.
    w.mock.s.lock().refresh_refused = false;
    w.install().await;
    let (_, v) = w.req("GET", "/v1/code/agents/acme/linear", None).await;
    assert_eq!(v["connection"]["status"], "ok");
    let (s, _) = w.req("DELETE", "/v1/code/agents/acme/linear", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(w.mock.s.lock().revoked.len(), 1);
}

#[tokio::test]
async fn a_workspace_belongs_to_one_owner() {
    let w = world().await;
    w.req("PUT", "/v1/code/agents/acme/linear/key", Some(json!({ "key": KEY }))).await;
    let (s, _) = w.a.req("POST", "/v1/code/owners", Some(json!({ "name": "other", "kind": "org" })), Some(ADMIN)).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, v) = w.a.req("PUT", "/v1/code/agents/other/linear/key", Some(json!({ "key": KEY })), Some(ADMIN)).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(w.mock.ops("RollWebhookDelete").len(), 1, "the webhook made for the refused connection is removed");
}

#[tokio::test]
async fn agent_sessions_and_replies_go_into_the_inbox_once() {
    let w = world().await;
    w.install().await;
    w.drop_automations().await;
    w.req("PUT", "/v1/code/agents/acme/linear/settings", Some(json!({ "repos": { "ENG": "app" } }))).await;
    let created = session_payload("created", "ses-1", None);
    let (s, v) = w.deliver("/v1/code/linear/webhook", APP_SECRET, created.clone()).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["recorded"], 1);
    let (key, kind, data) = w.inbox().pop().unwrap();
    assert_eq!(key, "linear:agent_session.created:ses-1");
    assert_eq!(kind, "agent_session.created");
    assert_eq!(data["session"]["id"], "ses-1");
    assert_eq!(data["issue"]["identifier"], "ENG-12");
    assert_eq!(data["prompt"], "@roll please fix this");
    assert!(data["prompt_context"].as_str().unwrap().contains("ENG-12"));
    assert_eq!(data["repo"], "app");
    assert_eq!(data["thread"], Value::Null);
    assert_eq!(data["attached"], false);
    assert_eq!(data["roll_user"], "app-user");

    // No automation takes sessions (the installed one was deleted): Linear hears so
    // at once, instead of showing roll as unresponsive.
    for _ in 0..200 {
        if !w.mock.activities().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let acts = w.mock.activities();
    assert_eq!(acts.len(), 1, "{acts:?}");
    assert_eq!(acts[0]["agentSessionId"], "ses-1");
    assert_eq!(acts[0]["content"]["type"], "error");

    // Linear sends it again: nothing new.
    let (_, v) = w.deliver("/v1/code/linear/webhook", APP_SECRET, created).await;
    assert_eq!(v["duplicates"], 1);

    // Replies, once each, in order.
    let (_, v) = w.deliver("/v1/code/linear/webhook", APP_SECRET, session_payload("prompted", "ses-1", Some(("act-1", "use the new API", None)))).await;
    assert_eq!(v["recorded"], 1);
    let (_, v) = w.deliver("/v1/code/linear/webhook", APP_SECRET, session_payload("prompted", "ses-1", Some(("act-1", "use the new API", None)))).await;
    assert_eq!(v["duplicates"], 1);
    w.deliver("/v1/code/linear/webhook", APP_SECRET, session_payload("prompted", "ses-1", Some(("act-2", "", Some("stop"))))).await;
    let (key, kind, data) = w.inbox().pop().unwrap();
    assert_eq!((key.as_str(), kind.as_str()), ("linear:agent_session.prompted:act-2", "agent_session.prompted"));
    assert_eq!(data["signal"], "stop");
    let prompts: Vec<(i64, String)> = w.a.code.db.sql().query("SELECT seq, activity_id FROM linear_prompts ORDER BY seq", (), |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(prompts, vec![(1, "act-1".into()), (2, "act-2".into())]);

    // A workspace no owner has is answered (Linear keeps the webhook on) and ignored.
    let mut stranger = session_payload("created", "ses-9", None);
    stranger["organizationId"] = json!("org-unknown");
    let (s, v) = w.deliver("/v1/code/linear/webhook", APP_SECRET, stranger).await;
    assert_eq!(s, StatusCode::OK);
    assert!(v["ignored"].is_string());
    let (s, _) = w.deliver("/v1/code/linear/webhook", "wrong", session_payload("created", "ses-3", None)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // Linear revoked the app: the connection is gone.
    w.deliver("/v1/code/linear/webhook", APP_SECRET, json!({ "type": "OAuthApp", "action": "revoked", "organizationId": "org-1", "oauthClientId": "cid" })).await;
    let (_, v) = w.req("GET", "/v1/code/agents/acme/linear", None).await;
    assert_eq!(v["connected"], false);
}

#[tokio::test]
async fn linear_steps_comment_update_link_create_and_post_activities() {
    let w = world().await;
    w.install().await;
    let ctx = json!({ "event": { "kind": "agent_session.created", "data": { "issue": { "id": "issue-1", "identifier": "ENG-12" }, "session": { "id": "ses-1" } } }, "steps": { "publish": { "url": "https://roll.sh/acme/app/changes/7", "change": 7 } } });

    // A comment, with a template; the same attempt again makes nothing twice.
    let (o, _) = w.step("linear.comment", json!({ "body": "Working on {{event.data.issue.identifier}}" }), ctx.clone(), "arn_1", Value::Null).await;
    let StepOutcome::Done(out) = o else { panic!("{o:?}") };
    let first = w.mock.ops("RollComment");
    assert_eq!(first[0]["input"]["body"], "Working on ENG-12");
    assert_eq!(first[0]["input"]["issueId"], "issue-1");
    let (o, _) = w.step("linear.comment", json!({ "body": "Working on {{event.data.issue.identifier}}" }), ctx.clone(), "arn_1", Value::Null).await;
    assert_eq!(o, StepOutcome::Done(json!({ "id": out["id"], "url": null })), "the retry finds the comment it made");
    assert_eq!(w.mock.ops("RollComment")[1]["input"]["id"], first[0]["input"]["id"]);

    // Update: a state by type, roll as the delegate, labels and a priority by name.
    let (o, _) = w.step("linear.update_issue", json!({ "state": "started", "delegate": "roll", "add_labels": ["roll"], "priority": "high" }), ctx.clone(), "arn_1", Value::Null).await;
    let StepOutcome::Done(out) = o else { panic!("{o:?}") };
    assert_eq!(out["state"], "In Progress", "the first started state");
    let input = &w.mock.ops("RollIssueUpdate")[0]["input"];
    assert_eq!(input["stateId"], "st-progress");
    assert_eq!(input["delegateId"], "app-user");
    assert_eq!(input["addedLabelIds"], json!(["lab-roll"]));
    assert_eq!(input["priority"], 2);
    let (o, _) = w.step("linear.update_issue", json!({ "state": "Shipped" }), ctx.clone(), "arn_1", Value::Null).await;
    assert!(matches!(o, StepOutcome::Fail(ref m) if m.contains("Shipped")), "{o:?}");

    // Link the change to the issue and the session.
    let (o, _) = w.step("linear.attach", json!({ "url": "{{steps.publish.url}}", "title": "roll change {{steps.publish.change}}" }), ctx.clone(), "arn_1", Value::Null).await;
    assert!(matches!(o, StepOutcome::Done(_)), "{o:?}");
    assert_eq!(w.mock.ops("RollAttach")[0]["input"], json!({ "issueId": "issue-1", "url": "https://roll.sh/acme/app/changes/7", "title": "roll change 7" }));
    assert_eq!(w.mock.ops("RollSessionLink")[0]["input"]["addedExternalUrls"][0]["url"], "https://roll.sh/acme/app/changes/7");

    // Create an issue in a team by key, assigned by name; the retry is the same issue.
    let cfg = json!({ "team": "eng", "title": "Follow up on {{event.data.issue.identifier}}", "assignee": "Ada", "labels": ["Bug"], "state": "Todo" });
    let (o, _) = w.step("linear.create_issue", cfg.clone(), ctx.clone(), "arn_2", Value::Null).await;
    assert_eq!(o, StepOutcome::Done(json!({ "id": w.mock.ops("RollIssueCreate")[0]["input"]["id"], "identifier": "ENG-13", "url": "https://linear.app/acme/issue/ENG-13" })));
    let input = &w.mock.ops("RollIssueCreate")[0]["input"];
    assert_eq!((input["teamId"].as_str(), input["assigneeId"].as_str(), input["stateId"].as_str()), (Some("team-1"), Some("user-ada"), Some("st-todo")));
    assert_eq!(input["title"], "Follow up on ENG-12");
    let (o, _) = w.step("linear.create_issue", cfg, ctx.clone(), "arn_2", Value::Null).await;
    assert!(matches!(o, StepOutcome::Done(ref v) if v["identifier"] == "ENG-12"), "the retry reads the issue it made: {o:?}");

    // Activities on the event's session.
    let (o, _) = w.step("linear.activity", json!({ "type": "elicitation", "body": "Which database?", "options": ["Postgres", "SQLite"] }), ctx.clone(), "arn_3", Value::Null).await;
    assert!(matches!(o, StepOutcome::Done(_)), "{o:?}");
    let a = w.mock.activities().pop().unwrap();
    assert_eq!(a["agentSessionId"], "ses-1");
    assert_eq!(a["content"], json!({ "type": "elicitation", "body": "Which database?" }));
    assert_eq!(a["signal"], "select");
    assert_eq!(a["signalMetadata"]["options"][1]["value"], "SQLite");
    let (o, _) = w.step("linear.activity", json!({ "type": "action", "action": "Ran", "parameter": "cargo test", "result": "ok", "ephemeral": true }), ctx.clone(), "arn_4", Value::Null).await;
    assert!(matches!(o, StepOutcome::Done(_)), "{o:?}");
    let a = w.mock.activities().pop().unwrap();
    assert_eq!(a["content"], json!({ "type": "action", "action": "Ran", "parameter": "cargo test", "result": "ok" }));
    assert_eq!(a["ephemeral"], true);

    // Save-time checks.
    let reg = &w.a.code.automations.registry;
    let owner = OwnerRef { id: w.owner_id.clone(), name: "acme".into() };
    assert!(reg.step("linear.activity").unwrap().check(&owner, &json!({ "type": "response", "ephemeral": true, "body": "x" })).is_err());
    assert!(reg.step("linear.update_issue").unwrap().check(&owner, &json!({ "priority": "soonish" })).is_err());
    assert!(reg.step("linear.comment").unwrap().check(&owner, &json!({ "text": "x" })).is_err());
    let t = reg.trigger("linear").unwrap();
    assert!(t.check(&owner, &["agent_session.created".into()], &json!({ "teams": ["ENG"] })).is_ok());
    assert!(t.check(&owner, &["issue.closed".into()], &Value::Null).is_err());
    assert!(!t.needs_setup(&owner));
    let cat = reg.catalog(&owner);
    assert!(cat.steps.iter().filter(|s| s.integration.as_deref() == Some("linear")).count() >= 6);
}

#[tokio::test]
async fn sessions_need_roll_as_an_app_not_an_api_key() {
    let w = world().await;
    let reg = &w.a.code.automations.registry;
    let owner = OwnerRef { id: w.owner_id.clone(), name: "acme".into() };
    let t = reg.trigger("linear").unwrap();
    assert!(t.needs_setup(&owner));
    assert!(t.check(&owner, &["issue.created".into()], &Value::Null).unwrap_err().contains("connect Linear"));
    w.req("PUT", "/v1/code/agents/acme/linear/key", Some(json!({ "key": KEY }))).await;
    assert!(t.check(&owner, &["issue.created".into()], &Value::Null).is_ok());
    assert!(t.check(&owner, &["agent_session.created".into()], &Value::Null).unwrap_err().contains("OAuth"));
}

/// A daemon with a fake `codex` that plays T3's demo turn (it asks to run a command, then
/// finishes), connected as machine `box`, its clones in a temp dir.
struct Daemon {
    machine: String,
    codex: Arc<FakeProvider>,
    _home: tempfile::TempDir,
    _clones: tempfile::TempDir,
    _stop: oneshot::Sender<()>,
}

async fn daemon(w: &World) -> Daemon {
    let (s, m) = w.req("POST", "/v1/code/agents/acme/machines", Some(json!({ "name": "box" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{m}");
    let machine = m["machine"]["id"].as_str().unwrap().to_string();
    let token = m["token"].as_str().unwrap().to_string();
    let home = tempfile::tempdir().unwrap();
    let clones = tempfile::tempdir().unwrap();
    let codex = FakeProvider::with("codex", Capabilities::default(), Script::Demo);
    let providers: Vec<Arc<dyn Provider>> = vec![codex.clone()];
    let config = HostConfig { home: home.path().to_path_buf(), clone_dir: Some(clones.path().to_path_buf()), ..HostConfig::default() };
    let host = Host::open(config, providers).await.unwrap();
    let (tx, mut events) = mpsc::unbounded_channel();
    let (stop, stopped) = oneshot::channel::<()>();
    let d: Arc<dyn agent_host::Daemon> = host;
    tokio::spawn(agent_host::relay::run(d, LinkConfig { api: w.url.clone(), token }, tx, async move {
        let _ = stopped.await;
    }));
    loop {
        match tokio::time::timeout(Duration::from_secs(10), events.recv()).await.expect("the daemon connects") {
            Some(LinkEvent::Connected { .. }) => break,
            Some(_) => continue,
            None => panic!("the link stopped"),
        }
    }
    Daemon { machine, codex, _home: home, _clones: clones, _stop: stop }
}

impl Daemon {
    /// What the daemon asked the fake provider's sessions to do, in order.
    fn calls(&self) -> Vec<Call> {
        self.codex.sessions().iter().flat_map(|s| s.calls()).collect()
    }
}

/// acme's repo `app`, with a README on main.
async fn repo(w: &World) {
    let (s, v) = w.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "app" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, v) = w.req("POST", "/v1/code/repos/acme/app/land", Some(json!({ "message": "init", "files": { "README.md": { "content": "# app\n" } } }))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

/// Attempts of the bridge step until it's done, or until `stop` holds between attempts (`None`,
/// with the state to go on from).
async fn drive(w: &World, ctx: &Value, run: &str, mut state: Value, stop: impl Fn(&World) -> bool) -> (Option<Value>, Value) {
    for _ in 0..1500 {
        let (o, st) = w.step("linear.agent_session", json!({}), ctx.clone(), run, state).await;
        state = st;
        match o {
            StepOutcome::Done(v) => return (Some(v), state),
            StepOutcome::Wait { .. } if stop(w) => return (None, state),
            StepOutcome::Wait { .. } => tokio::time::sleep(Duration::from_millis(20)).await,
            other => panic!("{other:?}; activities: {:?}", w.mock.activities()),
        }
    }
    panic!("the bridge never got there; activities: {:?}", w.mock.activities());
}

fn elicitations(w: &World) -> usize {
    w.mock.activities().iter().filter(|a| a["content"]["type"] == "elicitation").count()
}

fn reply(session: &str, activity: &str, body: &str, value: Option<&str>) -> Value {
    let mut p = session_payload("prompted", session, Some((activity, body, None)));
    if let Some(v) = value {
        p["agentActivity"]["signalMetadata"] = json!({ "value": v });
    }
    p
}

/// The bridge step on its own (the engine's waits are the loop's): it shows a thread's work in
/// the session, asks its approval there, takes the reply, answers with the result, and lets the
/// session go; a later reply goes to the same thread through a new run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_session_bridge_shows_the_thread_in_linear_and_answers_from_linear() {
    let w = world().await;
    w.install().await;
    repo(&w).await;
    let d = daemon(&w).await;
    w.drop_automations().await;
    // The run's automation (its steps' calls go through roll's API as it).
    let (s, saved) = w.req("POST", "/v1/code/agents/acme/automations", Some(json!({ "name": "bridge", "enabled": false, "trigger": { "type": "manual" }, "steps": [{ "id": "s1", "kind": "linear.agent_session" }] }))).await;
    assert_eq!(s, StatusCode::CREATED, "{saved}");
    let aut = saved["automation"]["id"].as_str().unwrap().to_string();
    w.a.code.db.sql().execute("UPDATE automations SET id = 'aut_linear' WHERE id = ?", [aut.as_str()]).unwrap();
    // roll is mentioned on ENG-12: Linear opens a session.
    w.deliver("/v1/code/linear/webhook", APP_SECRET, session_payload("created", "ses-1", None)).await;
    // What agent.start does: a project (the repo, cloned) and a thread with the prompt.
    let commands = format!("/v1/code/agents/acme/machines/{}/commands", d.machine);
    let cmd = |id: &str, c: Value| w.req("POST", &commands, Some(json!({ "id": id, "command": c })));
    let (s, v) = cmd("p1", json!({ "type": "project.add", "path": "", "clone": { "owner": "acme", "repo": "app" } })).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let project = v["id"].as_str().unwrap().to_string();
    let (s, v) = cmd("t1", json!({ "type": "thread.create", "project": project, "provider": "codex", "prompt": "fix ENG-12" })).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let thread = v["id"].as_str().unwrap().to_string();
    let ctx = json!({ "event": { "kind": "agent_session.created", "data": { "session": { "id": "ses-1" }, "issue": { "id": "issue-1", "identifier": "ENG-12" }, "prompt": "@roll please fix this" } }, "steps": { "start": { "machine": d.machine, "thread": thread } } });

    // The agent asks to run a command: an elicitation in the session. Ada picks Approve.
    let (done, st) = drive(&w, &ctx, "arn_s1", Value::Null, |w| elicitations(w) == 1).await;
    assert!(done.is_none(), "it waits for the answer");
    let ask = w.mock.activities().into_iter().find(|a| a["content"]["type"] == "elicitation").unwrap();
    assert!(ask["content"]["body"].as_str().unwrap().contains("tee ROLL_DEMO.md"), "{ask}");
    assert_eq!(ask["signal"], "select");
    let (_, v) = w.deliver("/v1/code/linear/webhook", APP_SECRET, reply("ses-1", "act-1", "", Some("approve"))).await;
    assert_eq!(v["recorded"], 1);
    let (_, _, data) = w.inbox().pop().unwrap();
    assert_eq!(data["attached"], false, "no engine run holds it here (the step loop isn't a run)");
    let (done, _) = drive(&w, &ctx, "arn_s1", st, |_| false).await;
    let out = done.unwrap();
    assert_eq!(out["outcome"], "done", "{out}");
    assert_eq!(out["thread"], thread.as_str());
    assert_eq!(out["message"], "Wrote ROLL_DEMO.md.");
    let calls = d.calls();
    assert!(calls.iter().any(|c| matches!(c, Call::Respond { response: Response::Decision { decision: Decision::Accept, .. }, .. })), "{calls:?}");

    let acts = w.mock.activities();
    assert!(acts.iter().any(|a| a["content"] == json!({ "type": "action", "action": "Ran", "parameter": "tee ROLL_DEMO.md", "result": "exit 0" })), "{acts:#?}");
    assert!(acts.iter().any(|a| a["content"]["action"] == "Edited" && a["content"]["parameter"] == "ROLL_DEMO.md"), "{acts:#?}");
    assert!(acts.iter().any(|a| a["content"]["type"] == "thought" && a["content"]["body"].as_str().unwrap().starts_with("Reading the project")), "the interim message is a thought: {acts:#?}");
    assert_eq!(acts.last().unwrap()["content"], json!({ "type": "response", "body": "Wrote ROLL_DEMO.md." }));
    let ids: std::collections::HashSet<&str> = acts.iter().filter_map(|a| a["id"].as_str()).collect();
    assert_eq!(ids.len(), acts.len(), "every activity once");
    let link = &w.mock.ops("RollSessionUrl")[0]["input"]["addedExternalUrls"][0];
    assert_eq!(link["label"], "roll thread");
    assert!(link["url"].as_str().unwrap().ends_with(&format!("/dashboard/agents?owner=acme&focus={thread}")), "{link}");
    let (holder, consumed): (Option<String>, i64) = w.a.code.db.sql().query_row("SELECT bridge_run, consumed_seq FROM linear_sessions WHERE session_id = 'ses-1'", (), |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!((holder, consumed), (None, 1), "the session is let go, its reply passed on");

    // A follow-up in Linear once it's done: no run shows the session, so it's an event of its own
    // (the template's next run), whose bridge sends it on to the same thread. That run asks
    // again; a reply in words declines and tells the agent why.
    let (_, v) = w.deliver("/v1/code/linear/webhook", APP_SECRET, reply("ses-1", "act-2", "also add a changelog line", None)).await;
    assert_eq!(v["recorded"], 1);
    let (_, kind, data) = w.inbox().pop().unwrap();
    assert_eq!(kind, "agent_session.prompted");
    assert_eq!(data["attached"], false);
    assert_eq!(data["thread"]["thread"], thread.as_str(), "the event says which thread works on the issue");
    let ctx2 = json!({ "event": { "kind": "agent_session.prompted", "data": data }, "steps": {} });
    let (done, st) = drive(&w, &ctx2, "arn_s2", Value::Null, |w| elicitations(w) == 2).await;
    assert!(done.is_none());
    w.deliver("/v1/code/linear/webhook", APP_SECRET, reply("ses-1", "act-3", "no, don't touch files yet", None)).await;
    let (done, _) = drive(&w, &ctx2, "arn_s2", st, |_| false).await;
    let out = done.unwrap();
    assert_eq!(out["outcome"], "done", "{out}");
    assert_eq!(out["message"], "Declined: nothing was written.");
    let calls = d.calls();
    assert!(calls.iter().any(|c| matches!(c, Call::Send(t) if t.contains("Ada (in Linear): also add a changelog line"))), "the follow-up went to the thread: {calls:?}");
    assert!(
        calls.iter().any(|c| matches!(c, Call::Respond { response: Response::Decision { decision: Decision::Decline, message: Some(m) }, .. } if m == "no, don't touch files yet")),
        "{calls:?}"
    );
}

/// The headline: roll installed as Linear's agent gets the "Linear agent" automation; mentioning
/// roll on an issue runs it through the engine: an agent in a worktree of the team's repo, its
/// approval asked and answered in Linear, its result as the session's response, and its change
/// published and linked to the issue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mentioning_roll_on_an_issue_runs_the_linear_agent_end_to_end() {
    let w = world().await;
    repo(&w).await;
    let d = daemon(&w).await;
    w.install().await;
    let list = w.req("GET", "/v1/code/agents/acme/automations", None).await.1;
    let auts = list.as_array().unwrap();
    assert_eq!(auts.len(), 1, "connecting installed the Linear agent: {list}");
    assert_eq!(auts[0]["name"], "Linear agent");
    assert_eq!(auts[0]["created_by"], "linear");
    w.install().await;
    assert_eq!(w.req("GET", "/v1/code/agents/acme/automations", None).await.1.as_array().unwrap().len(), 1, "installed once");
    w.req("PUT", "/v1/code/agents/acme/linear/settings", Some(json!({ "repos": { "ENG": "app" } }))).await;

    w.deliver("/v1/code/linear/webhook", APP_SECRET, session_payload("created", "ses-1", None)).await;
    // Linear hears from roll at once (it wants an activity within 10 s).
    for _ in 0..400 {
        if !w.mock.activities().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(w.mock.activities()[0]["content"], json!({ "type": "thought", "body": "On it: roll is getting an agent onto ENG-12." }));
    for _ in 0..1200 {
        if elicitations(&w) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(elicitations(&w), 1, "the approval is asked in Linear: {:?}", w.mock.activities());
    // While the run shows the session, a reply wakes it and isn't a new run.
    let (_, v) = w.deliver("/v1/code/linear/webhook", APP_SECRET, reply("ses-1", "act-1", "approve", None)).await;
    assert_eq!(v["recorded"], 1);
    assert_eq!(w.inbox().pop().unwrap().2["attached"], true);

    let mut last = Value::Null;
    for _ in 0..1200 {
        last = w.req("GET", "/v1/code/agents/acme/automations/runs", None).await.1;
        if last.as_array().is_some_and(|r| r.iter().any(|r| matches!(r["status"].as_str(), Some("succeeded" | "failed" | "cancelled")))) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let runs = last.as_array().unwrap();
    assert_eq!(runs.len(), 1, "the reply started no second run: {last}");
    let run = &runs[0];
    assert_eq!(run["status"], "succeeded", "{run:#}");
    let steps: Vec<(&str, &str)> = run["steps"].as_array().unwrap().iter().map(|s| (s["id"].as_str().unwrap(), s["status"].as_str().unwrap())).collect();
    assert_eq!(steps, [("ack", "succeeded"), ("started", "skipped"), ("start", "succeeded"), ("session", "succeeded"), ("publish", "succeeded"), ("link", "succeeded")], "{run:#}");
    assert_eq!(run["threads"].as_array().unwrap().len(), 1, "the canvas tags the thread with the run");
    let publish = &run["steps"][4]["output"];
    let change_url = publish["url"].as_str().unwrap();
    // The change is in roll code, and linked to the issue and the session.
    let attach = &w.mock.ops("RollAttach")[0]["input"];
    assert_eq!(attach["url"], change_url);
    assert_eq!(attach["issueId"], "issue-1");
    assert!(w.mock.ops("RollSessionLink").iter().any(|v| v["input"]["addedExternalUrls"][0]["url"] == change_url));
    let acts = w.mock.activities();
    assert_eq!(acts.last().unwrap()["content"]["type"], "response");
    assert!(d.calls().iter().any(|c| matches!(c, Call::Respond { response: Response::Decision { decision: Decision::Accept, .. }, .. })));
    let prompt = d.calls().into_iter().find_map(|c| match c {
        Call::Send(t) => Some(t),
        _ => None,
    });
    let prompt = prompt.expect("the agent got its prompt");
    assert!(prompt.contains("ENG-12: Fix login") && prompt.contains("@roll please fix this"), "{prompt}");
}


/// The dashboard's copy of Linear's catalog (`apps/dashboard/src/lib/linear-catalog.json`: the
/// demo and the template checks use it) is the server's. `ROLL_FIXTURES=write` rewrites it.
#[tokio::test]
async fn the_dashboard_has_linear_s_catalog() {
    let w = world().await;
    let owner = OwnerRef { id: w.owner_id.clone(), name: "acme".into() };
    let cat = w.a.code.automations.registry.catalog(&owner);
    let triggers: Vec<_> = cat.triggers.into_iter().filter(|t| t.source == "linear").map(|t| agent_proto::automation::TriggerInfo { needs_setup: false, ..t }).collect();
    let steps: Vec<_> = cat.steps.into_iter().filter(|s| s.integration.as_deref() == Some("linear")).map(|s| agent_proto::automation::StepInfo { needs_setup: false, ..s }).collect();
    let now = serde_json::to_string_pretty(&json!({ "triggers": triggers, "steps": steps })).unwrap() + "\n";
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/dashboard/src/lib/linear-catalog.json");
    if std::env::var("ROLL_FIXTURES").as_deref() == Ok("write") {
        std::fs::write(&path, &now).unwrap();
    }
    let kept = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(kept == now, "{} is out of date: run ROLL_FIXTURES=write cargo test -p code-control --test linear the_dashboard_has_linear_s_catalog", path.display());
}
