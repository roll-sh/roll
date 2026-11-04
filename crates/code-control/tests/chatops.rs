//! Chat ops end to end against mock Slack and Discord APIs (docs/code/chatops.md): signed
//! requests (and refused ones), replays recorded once, the OAuth install with a sealed token,
//! who may drive roll from a workspace, `/roll` into the automation inbox, and the live status
//! flow: a status message and a request card posted for an agent thread (a scripted daemon's),
//! the card's button answering the request through `request.respond`, a thread reply steering
//! the agent, and the status finishing with the change's link.

mod common;

use agent_proto::automation::{AutomationId, AutomationRunId, Step};
use agent_proto::{DaemonFrame, Entity, RelayFrame, Reply, Stamped};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, Uri};
use code_control::agents::automations::{OwnerRef, StepContext, StepOutcome};
use code_control::chatops::{self, DiscordConfig, SlackConfig};
use common::daemon::Daemon;
use common::{App, app_with};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const ADMIN: &str = "admin-secret";
const SIGNING: &str = "8f742231b10e8888abcd99yyyzzz85a5";
const BOT_TOKEN: &str = "xoxb-test-bot-token";
const SECRET_KEY: &str = "0123456789abcdef0123456789abcdef";

// ── a mock Slack Web API (and Discord's) ──

#[derive(Default)]
struct Mock {
    /// (path, authorization, body) of every call.
    calls: Mutex<Vec<(String, String, String)>>,
    ts: AtomicU64,
    /// The team the OAuth exchange answers.
    team: Mutex<String>,
}

impl Mock {
    fn calls_to(&self, path: &str) -> Vec<Value> {
        self.calls.lock().iter().filter(|c| c.0.ends_with(path)).map(|c| serde_json::from_str(&c.2).unwrap_or(Value::String(c.2.clone()))).collect()
    }
}

async fn mock_api(State(m): State<Arc<Mock>>, uri: Uri, headers: axum::http::HeaderMap, body: String) -> axum::Json<Value> {
    let path = uri.path().to_string();
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    m.calls.lock().push((path.clone(), auth, body.clone()));
    let b: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let ts = || format!("1700000000.{:06}", m.ts.fetch_add(1, Ordering::SeqCst) + 1);
    axum::Json(match path.trim_start_matches("/slack").trim_start_matches('/') {
        "oauth.v2.access" => json!({
            "ok": true, "access_token": BOT_TOKEN, "token_type": "bot", "scope": "chat:write,commands", "bot_user_id": "UBOT", "app_id": "A1",
            "team": { "id": m.team.lock().clone(), "name": "Acme" }, "authed_user": { "id": "UADA" }
        }),
        "chat.postMessage" => json!({ "ok": true, "channel": b["channel"], "ts": ts() }),
        "chat.update" => json!({ "ok": true, "channel": b["channel"], "ts": b["ts"] }),
        "conversations.open" => json!({ "ok": true, "channel": { "id": "D1" } }),
        "conversations.history" => json!({ "ok": true, "messages": [{ "ts": "1600000000.000100", "user": "UEVE", "text": "the login page 500s" }] }),
        "discord/oauth2/token" => json!({ "access_token": "user-token", "token_type": "Bearer", "guild": { "id": "G1", "name": "Acme Discord" } }),
        p if p.starts_with("discord/") => {
            if p.ends_with("/messages") {
                json!({ "id": format!("m{}", m.ts.fetch_add(1, Ordering::SeqCst) + 1), "channel_id": "C9" })
            } else {
                json!({ "id": "m0", "channel_id": "C9" })
            }
        }
        _ => json!({ "ok": true }),
    })
}

async fn serve_mock() -> (String, Arc<Mock>) {
    let m = Arc::new(Mock::default());
    *m.team.lock() = "T1".into();
    let r = Router::new().fallback(mock_api).with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, r).await.unwrap() });
    (base, m)
}

struct World {
    a: App,
    mock: Arc<Mock>,
    owner: String,
    agent: String,
    owner_id: String,
    discord: code_chat::discord::TestSigner,
}

async fn world() -> World {
    let (base, mock) = serve_mock().await;
    let a = app_with(|c| {
        c.admin_token = Some(ADMIN.into());
        c.review.secret_key = Some(SECRET_KEY.into());
    });
    let discord = code_chat::discord::TestSigner::new([7; 32]);
    let slack = SlackConfig {
        client_id: "111.222".into(),
        client_secret: "slack-client-secret".into(),
        signing_secret: SIGNING.into(),
        api_base: format!("{base}/slack"),
        authorize_url: "https://slack.com/oauth/v2/authorize".into(),
    };
    let dc = DiscordConfig {
        application_id: "app1".into(),
        public_key: discord.public_key_hex(),
        bot_token: "discord-bot-token".into(),
        client_secret: "discord-client-secret".into(),
        api_base: format!("{base}/discord"),
        authorize_url: "https://discord.com/oauth2/authorize".into(),
    };
    chatops::enable(&a.code, Some(slack), Some(dc)).unwrap();
    for o in ["acme", "other"] {
        let (s, v) = a.req("POST", "/v1/code/owners", Some(json!({ "name": o, "kind": "org" })), Some(ADMIN)).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
    }
    let mint = |name: &'static str| {
        let a = &a;
        async move { a.req("POST", "/v1/code/owners/acme/tokens", Some(json!({ "name": name })), Some(ADMIN)).await.1["token"].as_str().unwrap().to_string() }
    };
    let owner = mint("me").await;
    let agent = mint("agent:bot").await;
    let owner_id = a.code.db.owner("acme").unwrap().unwrap().id;
    World { a, mock, owner, agent, owner_id, discord }
}

fn now_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn signed(path: &str, body: &str, content_type: &str, ts: i64) -> Request<Body> {
    Request::post(path)
        .header("content-type", content_type)
        .header("x-slack-request-timestamp", ts.to_string())
        .header("x-slack-signature", code_chat::slack::sign(SIGNING, ts, body.as_bytes()))
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn send(a: &App, req: Request<Body>) -> (StatusCode, Value) {
    let resp = a.send(req).await;
    let s = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (s, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn form(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish()
}

async fn until<F: FnMut() -> bool>(what: &str, mut f: F) {
    for _ in 0..300 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

impl World {
    /// Connect the Slack workspace `team` to acme through the OAuth flow; the install's id.
    async fn install(&self, team: &str) -> String {
        *self.mock.team.lock() = team.into();
        let (s, v) = self.a.req("POST", "/v1/code/agents/acme/integrations/slack/install", None, Some(&self.owner)).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let url = url::Url::parse(v["url"].as_str().unwrap()).unwrap();
        let state = url.query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
        let resp = self.a.send(Request::get(format!("/v1/code/chat/slack/oauth?code=grant-1&state={state}")).body(Body::empty()).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let to = resp.headers()["location"].to_str().unwrap().to_string();
        assert!(to.contains("connected=Acme"), "{to}");
        let v = self.a.ok_as("GET", "/v1/code/agents/acme/integrations", &self.owner).await;
        v["slack"]["installs"].as_array().unwrap().iter().find(|i| i["team_id"] == team).unwrap()["id"].as_str().unwrap().to_string()
    }

    /// An automation that listens to `source` (all kinds): what the engine's save would write.
    fn automation(&self, source: &str) -> AutomationId {
        let id = AutomationId(format!("aut_{source}test"));
        let db = self.a.code.db.sql();
        db.execute(
            "INSERT INTO automations (id, owner_id, spec, enabled, source, revision, cursor, created_at, updated_at, created_by) VALUES (?, ?, '{}', 1, ?, 1, 0, 0, 0, 'test')",
            code_sql::params![id.as_str(), self.owner_id, source],
        )
        .unwrap();
        db.execute("INSERT INTO automation_listens (automation_id, owner_id, source, kind, repo_id) VALUES (?, ?, ?, '*', '*')", code_sql::params![id.as_str(), self.owner_id, source]).unwrap();
        id
    }

    fn events(&self) -> Vec<(String, String, Value)> {
        self.a
            .code
            .db
            .sql()
            .query("SELECT key, kind, data FROM automation_events WHERE owner_id = ? ORDER BY at, id", [self.owner_id.as_str()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })
            .unwrap()
            .into_iter()
            .map(|(k, kind, d)| (k, kind, serde_json::from_str(&d).unwrap()))
            .collect()
    }
}

trait OkAs {
    async fn ok_as(&self, method: &str, path: &str, token: &str) -> Value;
}

impl OkAs for App {
    async fn ok_as(&self, method: &str, path: &str, token: &str) -> Value {
        let (s, v) = self.req(method, path, None, Some(token)).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }
}

fn command_body(user: &str, text: &str, trigger: &str) -> String {
    form(&[("team_id", "T1"), ("channel_id", "C1"), ("user_id", user), ("user_name", "ada"), ("command", "/roll"), ("text", text), ("trigger_id", trigger), ("response_url", "https://hooks.slack.com/x")])
}

// ── tests ──

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slack_requests_are_signed_fresh_and_recorded_once() {
    let w = world().await;
    w.install("T1").await;
    let event = |id: &str| json!({ "type": "event_callback", "team_id": "T1", "event_id": id, "event_time": 1, "event": { "type": "app_mention", "user": "UADA", "channel": "C1", "text": "<@UBOT> hi", "ts": "1.1" } }).to_string();
    let body = event("Ev1");

    // No signature, a wrong one, a stale one: refused before anything is read.
    let bare = Request::post("/v1/code/chat/slack/events").header("content-type", "application/json").body(Body::from(body.clone())).unwrap();
    assert_eq!(send(&w.a, bare).await.0, StatusCode::UNAUTHORIZED);
    let mut forged = signed("/v1/code/chat/slack/events", &body, "application/json", now_s());
    forged.headers_mut().insert("x-slack-signature", "v0=00".parse().unwrap());
    assert_eq!(send(&w.a, forged).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&w.a, signed("/v1/code/chat/slack/events", &body, "application/json", now_s() - 600)).await.0, StatusCode::UNAUTHORIZED, "a replay from ten minutes ago");
    let tampered = signed("/v1/code/chat/slack/events", &body, "application/json", now_s());
    let (parts, _) = tampered.into_parts();
    assert_eq!(send(&w.a, Request::from_parts(parts, Body::from(event("Ev2")))).await.0, StatusCode::UNAUTHORIZED, "the body is signed");
    assert!(chatops::actions(&w.a.code).is_empty());

    // Slack's URL check.
    let check = json!({ "type": "url_verification", "challenge": "c-123", "token": "x" }).to_string();
    assert_eq!(send(&w.a, signed("/v1/code/chat/slack/events", &check, "application/json", now_s())).await, (StatusCode::OK, json!({ "challenge": "c-123" })));

    // A delivery and Slack's retries of it (X-Slack-Retry-Num): one action.
    for retry in 0..3 {
        let mut r = signed("/v1/code/chat/slack/events", &body, "application/json", now_s());
        if retry > 0 {
            r.headers_mut().insert("x-slack-retry-num", retry.to_string().parse().unwrap());
        }
        assert_eq!(send(&w.a, r).await.0, StatusCode::OK);
    }
    let actions = chatops::actions(&w.a.code);
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert_eq!(actions[0].0, "slack:event:Ev1");
    until("the action", || chatops::actions(&w.a.code)[0].2 != "pending").await;

    // A workspace nobody connected: acknowledged, nothing recorded.
    let stranger = json!({ "type": "event_callback", "team_id": "T404", "event_id": "Ev9", "event_time": 1, "event": { "type": "app_mention", "user": "U", "channel": "C", "text": "x", "ts": "1.1" } }).to_string();
    assert_eq!(send(&w.a, signed("/v1/code/chat/slack/events", &stranger, "application/json", now_s())).await.0, StatusCode::OK);
    assert_eq!(chatops::actions(&w.a.code).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slack_installs_once_per_owner_with_the_token_sealed() {
    let w = world().await;
    // Only the owner manages it: not its agents, not other owners.
    let (s, _) = w.a.req("POST", "/v1/code/agents/acme/integrations/slack/install", None, Some(&w.agent)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = w.a.req("GET", "/v1/code/agents/acme/integrations", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, v) = w.a.req("POST", "/v1/code/agents/acme/integrations/slack/install", None, Some(&w.owner)).await;
    let url = url::Url::parse(v["url"].as_str().unwrap()).unwrap();
    assert_eq!(s, StatusCode::OK);
    let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(q["client_id"], "111.222");
    assert_eq!(q["redirect_uri"], "https://api.roll.sh/v1/code/chat/slack/oauth");
    assert!(q["scope"].contains("commands") && q["scope"].contains("chat:write"), "{q:?}");

    let id = w.install("T1").await;
    let oauth = w.mock.calls_to("oauth.v2.access");
    assert_eq!(oauth.len(), 1);
    let form = oauth[0].as_str().unwrap();
    assert!(form.contains("code=grant-1") && form.contains("client_secret=slack-client-secret"), "{form}");
    // The token is sealed at rest, and never in an answer.
    let sealed: String = w.a.code.db.sql().query_row("SELECT token_sealed FROM chat_installs WHERE id = ?", [id.as_str()], |r| r.get(0)).unwrap();
    assert!(!sealed.contains(BOT_TOKEN) && !sealed.is_empty());
    let v = w.a.ok_as("GET", "/v1/code/agents/acme/integrations", &w.owner).await;
    assert!(!v.to_string().contains(BOT_TOKEN));
    assert_eq!(v["slack"]["installs"][0]["installer"], "UADA");
    assert_eq!(v["slack"]["configured"], true);

    // A connect link works once.
    let (_, v) = w.a.req("POST", "/v1/code/agents/acme/integrations/slack/install", None, Some(&w.owner)).await;
    let state = url::Url::parse(v["url"].as_str().unwrap()).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
    let go = |state: String| Request::get(format!("/v1/code/chat/slack/oauth?code=grant-2&state={state}")).body(Body::empty()).unwrap();
    let first = w.a.send(go(state.clone())).await;
    assert!(first.headers()["location"].to_str().unwrap().contains("connected="));
    let again = w.a.send(go(state)).await;
    assert!(again.headers()["location"].to_str().unwrap().contains("error=expired"));
    let forged = w.a.send(go("made-up".into())).await;
    assert!(forged.headers()["location"].to_str().unwrap().contains("error=expired"));

    // Another owner can't take the workspace.
    let (_, t) = w.a.req("POST", "/v1/code/owners/other/tokens", Some(json!({ "name": "me" })), Some(ADMIN)).await;
    let other = t["token"].as_str().unwrap().to_string();
    let (_, v) = w.a.req("POST", "/v1/code/agents/other/integrations/slack/install", None, Some(&other)).await;
    let state = url::Url::parse(v["url"].as_str().unwrap()).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
    let taken = w.a.send(go(state)).await;
    assert!(taken.headers()["location"].to_str().unwrap().contains("another+roll+owner"), "{:?}", taken.headers()["location"]);
    let (s, _) = w.a.req("PATCH", &format!("/v1/code/agents/other/integrations/slack/{id}"), Some(json!({ "anyone": true })), Some(&other)).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "not other's install");

    // Members, then disconnecting.
    let (s, v) = w.a.req("PATCH", &format!("/v1/code/agents/acme/integrations/slack/{id}"), Some(json!({ "members": ["UBEN", " UBEN ", ""] })), Some(&w.owner)).await;
    assert_eq!((s, v["members"].clone()), (StatusCode::OK, json!(["UBEN"])));
    let (s, _) = w.a.req("PATCH", &format!("/v1/code/agents/acme/integrations/slack/{id}"), Some(json!({ "members": ["<script>"] })), Some(&w.owner)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = w.a.req("DELETE", &format!("/v1/code/agents/acme/integrations/slack/{id}"), None, Some(&w.owner)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let v = w.a.ok_as("GET", "/v1/code/agents/acme/integrations", &w.owner).await;
    assert_eq!(v["slack"]["installs"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn roll_commands_reach_the_inbox_only_from_allowed_people() {
    let w = world().await;
    let id = w.install("T1").await;
    let cmd = |user: &str, text: &str, trigger: &str| signed("/v1/code/chat/slack/commands", &command_body(user, text, trigger), "application/x-www-form-urlencoded", now_s());

    // No automation listens yet: said so, nothing recorded.
    let (s, v) = send(&w.a, cmd("UADA", "fix the login test", "tr1")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(v["text"].as_str().unwrap().contains("No automation"), "{v}");
    w.automation("slack");

    // A stranger to the install.
    let (_, v) = send(&w.a, cmd("UEVE", "rm -rf", "tr2")).await;
    assert!(v["text"].as_str().unwrap().contains("Only people"), "{v}");
    // The installer.
    let (_, v) = send(&w.a, cmd("UADA", "fix the <login> test", "tr3")).await;
    assert_eq!(v["response_type"], "ephemeral");
    assert_eq!(v["text"], "Sent to roll: fix the &lt;login&gt; test");
    // The same command delivered twice is one event.
    send(&w.a, cmd("UADA", "fix the <login> test", "tr3")).await;
    until("the event", || !w.events().is_empty()).await;
    let ev = w.events();
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!((ev[0].0.as_str(), ev[0].1.as_str()), ("slack:T1:command:tr3", "command"));
    assert_eq!(ev[0].2["prompt"], "fix the <login> test");
    assert_eq!(ev[0].2["channel"], "C1");

    // Opened to anyone: the stranger too.
    w.a.req("PATCH", &format!("/v1/code/agents/acme/integrations/slack/{id}"), Some(json!({ "anyone": true })), Some(&w.owner)).await;
    send(&w.a, cmd("UEVE", "add a test", "tr4")).await;
    until("the second event", || w.events().len() == 2).await;

    // A reaction hands a message to roll, with its text.
    let reaction = json!({ "type": "event_callback", "team_id": "T1", "event_id": "EvR", "event_time": 1, "event": {
        "type": "reaction_added", "user": "UADA", "reaction": "robot_face", "item_user": "UEVE", "item": { "type": "message", "channel": "C1", "ts": "1600000000.000100" }
    } })
    .to_string();
    send(&w.a, signed("/v1/code/chat/slack/events", &reaction, "application/json", now_s())).await;
    until("the reaction", || w.events().len() == 3).await;
    let r = &w.events()[2];
    assert_eq!((r.1.as_str(), r.2["text"].as_str(), r.2["reaction"].as_str()), ("reaction", Some("the login page 500s"), Some("robot_face")));
}

/// The key flow: an agent's live status in a Slack thread, its approval as buttons, a click
/// answering it through `request.respond`, a reply steering it, and the status finishing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slack_status_buttons_answer_requests_and_replies_steer() {
    let w = world().await;
    w.install("T1").await;
    let automation = w.automation("slack");
    let url = w.a.serve().await;
    let (s, m) = w.a.req("POST", "/v1/code/agents/acme/machines", Some(json!({ "name": "build-box" })), Some(&w.owner)).await;
    assert_eq!(s, StatusCode::CREATED);
    let (machine, token) = (m["machine"]["id"].as_str().unwrap().to_string(), m["token"].as_str().unwrap().to_string());
    let (mut d, _) = Daemon::connect(&url, &token).await;
    let thread = |status: &str, pending: u32, change: Option<Value>| -> Entity {
        let mut t = json!({
            "kind": "thread", "id": "thr_1", "project": "prj_1", "title": "Fix the flaky login test", "provider": "codex", "model": null,
            "runtime_mode": "ask", "interaction_mode": "default", "cwd": "/src/app", "status": status, "run_count": 1,
            "pending_requests": pending, "created_at": 1, "updated_at": 2
        });
        if status != "idle" {
            t["active_run"] = json!("run_1");
        }
        if let Some(c) = change {
            t["change"] = c;
        }
        serde_json::from_value(t).unwrap()
    };
    let run = |status: &str| -> Entity { serde_json::from_value(json!({ "kind": "run", "id": "run_1", "thread": "thr_1", "ordinal": 1, "provider": "codex", "status": status, "prompt": "fix it", "requested_at": 1 })).unwrap() };
    let tool: Entity = serde_json::from_value(json!({
        "kind": "node", "id": "nod_1", "thread": "thr_1", "run": "run_1", "ordinal": 1, "status": "completed", "started_at": 1,
        "body": { "type": "tool", "name": "Bash", "kind": "command", "title": "npm test", "input": {} }
    }))
    .unwrap();
    let request = |status: &str| -> Entity {
        let mut r = json!({
            "kind": "request", "id": "req_1", "thread": "thr_1", "run": "run_1", "node": "nod_2", "status": status, "created_at": 3,
            "body": { "type": "approval", "action": "command", "title": "Run the migration", "command": "npm run migrate", "input": {}, "decisions": ["accept", "decline", "cancel"] }
        });
        if status != "pending" {
            r["resolved_by"] = json!("owner");
            r["response"] = json!({ "type": "decision", "decision": "accept" });
        }
        serde_json::from_value(r).unwrap()
    };
    d.upsert(vec![Stamped { seq: 1, entity: thread("waiting", 1, None) }, Stamped { seq: 2, entity: run("waiting") }, Stamped { seq: 3, entity: tool }, Stamped { seq: 4, entity: request("pending") }])
        .await;

    // The step as the engine runs it: after /roll in C1.
    let step = |state: Value| {
        StepContext::new(
            w.a.code.clone(),
            OwnerRef { id: w.owner_id.clone(), name: "acme".into() },
            automation.clone(),
            "/roll in Slack".into(),
            None,
            AutomationRunId("arn_1".into()),
            serde_json::from_value::<Step>(json!({ "id": "status", "kind": "slack.status", "config": { "machine": machine, "thread": "thr_1" } })).unwrap(),
            1,
            json!({ "event": { "source": "slack", "kind": "command", "data": { "team": "T1", "channel": "C1", "thread_ts": null, "prompt": "fix it" } }, "steps": {} }),
            state,
            code_sql::now_ms(),
        )
    };
    let kind = w.a.code.automations.registry.step("slack.status").expect("registered");
    let mut ctx = step(Value::Null);
    let out = kind.run(&mut ctx).await;
    assert!(matches!(out, StepOutcome::Wait { .. }), "{out:?}");
    let posts = w.mock.calls_to("chat.postMessage");
    assert_eq!(posts.len(), 2, "the status, then the request card: {posts:?}");
    assert_eq!(posts[0]["channel"], "C1");
    assert!(posts[0]["text"].as_str().unwrap().contains("Fix the flaky login test"), "{}", posts[0]);
    let status_ts = "1700000000.000001";
    assert_eq!(posts[1]["thread_ts"], status_ts, "requests go in the status's thread");
    assert!(posts[1].to_string().contains("npm run migrate"));
    let auths: Vec<String> = w.mock.calls.lock().iter().filter(|c| c.0.contains("chat.")).map(|c| c.1.clone()).collect();
    assert!(auths.iter().all(|a| a == &format!("Bearer {BOT_TOKEN}")));
    // Waking again with nothing new edits nothing and posts nothing.
    let (updates, state) = (w.mock.calls_to("chat.update").len(), ctx.state().clone());
    let mut ctx = step(state);
    assert!(matches!(kind.run(&mut ctx).await, StepOutcome::Wait { .. }));
    assert_eq!((w.mock.calls_to("chat.postMessage").len(), w.mock.calls_to("chat.update").len()), (2, updates));

    // The Approve button's value, as Slack sends it back.
    let buttons: Vec<Value> = posts[1]["blocks"].as_array().unwrap().iter().filter(|b| b["type"] == "actions").flat_map(|b| b["elements"].as_array().unwrap().clone()).collect();
    let approve = buttons.iter().find(|b| b["text"]["text"] == "Approve").expect("an Approve button").clone();
    let click = |user: &str| {
        let payload = json!({
            "type": "block_actions", "team": { "id": "T1" }, "user": { "id": user, "username": "ada" }, "channel": { "id": "C1" },
            "message": { "ts": "1700000000.000002", "thread_ts": status_ts }, "container": { "type": "message" }, "trigger_id": format!("trig-{user}"),
            "actions": [{ "type": "button", "action_id": approve["action_id"], "block_id": "roll.choices", "value": approve["value"] }], "state": { "values": {} }
        });
        signed("/v1/code/chat/slack/interactions", &form(&[("payload", &payload.to_string())]), "application/x-www-form-urlencoded", now_s())
    };

    // Someone the install doesn't allow: told so, nothing sent.
    assert_eq!(send(&w.a, click("UEVE")).await.0, StatusCode::OK);
    until("the stranger's notice", || w.mock.calls_to("chat.postEphemeral").iter().any(|c| c["user"] == "UEVE")).await;
    tokio::time::timeout(Duration::from_millis(300), d.recv()).await.expect_err("no command for a stranger's click");

    // The installer: request.respond reaches the daemon, as the automation.
    assert_eq!(send(&w.a, click("UADA")).await.0, StatusCode::OK);
    let (id, by, command) = match tokio::time::timeout(Duration::from_secs(10), d.recv()).await.unwrap() {
        Some(RelayFrame::Command { id, by, command, .. }) => (id, by, serde_json::to_value(&command).unwrap()),
        other => panic!("{other:?}"),
    };
    assert_eq!(command, json!({ "type": "request.respond", "request": "req_1", "response": { "type": "decision", "decision": "accept" } }));
    // Sent as the owner (the automation's call token is the owner's, named for the automation).
    assert_eq!(by, "acme");
    d.send(&DaemonFrame::Reply { id, result: Reply::Ok }).await;
    until("the answered card", || w.mock.calls_to("chat.update").iter().any(|u| u["ts"] == "1700000000.000002" && u.to_string().contains("Answered by"))).await;

    // A reply in the thread steers the agent; the mention event Slack also sends for it is the
    // same steer, sent once.
    let reply = |kind: &str, id: &str| {
        json!({ "type": "event_callback", "team_id": "T1", "event_id": id, "event_time": 1, "event": {
            "type": kind, "user": "UADA", "channel": "C1", "text": "<@UBOT> use the staging database", "ts": "1700000000.000500", "thread_ts": status_ts
        } })
        .to_string()
    };
    send(&w.a, signed("/v1/code/chat/slack/events", &reply("message", "EvM"), "application/json", now_s())).await;
    let (id, command) = match tokio::time::timeout(Duration::from_secs(10), d.recv()).await.unwrap() {
        Some(RelayFrame::Command { id, command, .. }) => (id, serde_json::to_value(&command).unwrap()),
        other => panic!("{other:?}"),
    };
    assert_eq!((command["type"].as_str(), command["mode"].as_str(), command["thread"].as_str()), (Some("message.send"), Some("steer"), Some("thr_1")));
    assert!(command["text"].as_str().unwrap().starts_with("use the staging database"), "{command}");
    d.send(&DaemonFrame::Reply { id: id.clone(), result: Reply::Ok }).await;
    send(&w.a, signed("/v1/code/chat/slack/events", &reply("app_mention", "EvA"), "application/json", now_s())).await;
    match tokio::time::timeout(Duration::from_secs(10), d.recv()).await {
        // The same command id: the relay answers it from its receipt, and the daemon is asked once.
        Ok(Some(RelayFrame::Command { id: again, .. })) => {
            assert_eq!(again, id, "the same steer");
            d.send(&DaemonFrame::Reply { id: again, result: Reply::Ok }).await;
        }
        Ok(other) => panic!("{other:?}"),
        Err(_) => {}
    }
    until("both events handled", || chatops::actions(&w.a.code).iter().all(|a| a.2 != "pending")).await;
    assert_eq!(w.mock.calls_to("reactions.add").len(), 1, "one 👀 for one steer");

    // The thread finishes: the status says so, and the step ends. (The change's link comes from
    // the relay's own record of what the thread published: cards.rs's tests.)
    d.upsert(vec![Stamped { seq: 5, entity: request("resolved") }, Stamped { seq: 6, entity: run("completed") }, Stamped { seq: 7, entity: thread("idle", 0, None) }]).await;
    let mut ctx = step(ctx.state().clone());
    let out = kind.run(&mut ctx).await;
    let StepOutcome::Done(out) = out else { panic!("{out:?}") };
    assert_eq!((out["status"].as_str(), out["ts"].as_str(), out["thread_ts"].as_str()), (Some("done"), Some(status_ts), Some(status_ts)));
    let last = w.mock.calls_to("chat.update").into_iter().rev().find(|u| u["ts"] == status_ts).unwrap();
    assert!(last.to_string().contains("Done"), "{last}");
    assert_eq!(w.mock.calls_to("chat.postMessage").len(), 2, "nothing posted twice");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discord_interactions_are_verified_and_reach_the_inbox() {
    let w = world().await;
    let ts = now_s().to_string();
    let req = |body: &str, ts: &str, sig: Option<String>| {
        Request::post("/v1/code/chat/discord/interactions")
            .header("content-type", "application/json")
            .header("x-signature-timestamp", ts)
            .header("x-signature-ed25519", sig.unwrap_or_else(|| w.discord.sign(ts, body.as_bytes())))
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let ping = json!({ "type": 1, "id": "i0", "application_id": "app1", "token": "t" }).to_string();
    assert_eq!(send(&w.a, req(&ping, &ts, None)).await, (StatusCode::OK, json!({ "type": 1 })));
    assert_eq!(send(&w.a, req(&ping, &ts, Some("00".repeat(64)))).await.0, StatusCode::UNAUTHORIZED);
    let old = (now_s() - 900).to_string();
    assert_eq!(send(&w.a, req(&ping, &old, None)).await.0, StatusCode::UNAUTHORIZED, "a replayed interaction");

    // Connected through the bot install (the guild from the token exchange), then /roll.
    let (_, v) = w.a.req("POST", "/v1/code/agents/acme/integrations/discord/install", None, Some(&w.owner)).await;
    let state = url::Url::parse(v["url"].as_str().unwrap()).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
    // The token exchange's answer names the guild; the redirect's own guild_id is ignored.
    let resp = w.a.send(Request::get(format!("/v1/code/chat/discord/oauth?code=g&state={state}&guild_id=G-forged")).body(Body::empty()).unwrap()).await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let v = w.a.ok_as("GET", "/v1/code/agents/acme/integrations", &w.owner).await;
    let install = v["discord"]["installs"][0].clone();
    assert_ne!(install["team_id"], "G-forged", "{v}");
    let guild = install["team_id"].as_str().unwrap().to_string();
    assert!(w.mock.calls.lock().iter().any(|c| c.0.contains("/applications/app1/guilds/") && c.0.ends_with("/commands")), "/roll is registered");

    let command = |id: &str, user: &str| {
        json!({ "type": 2, "id": id, "application_id": "app1", "token": "tok", "guild_id": guild, "channel_id": "C9",
            "member": { "user": { "id": user, "username": "ada" } }, "data": { "name": "roll", "options": [{ "name": "prompt", "type": 3, "value": "fix the build" }] } })
        .to_string()
    };
    // Nobody is allowed yet (Discord doesn't say who installed it).
    let (_, v) = send(&w.a, req(&command("i1", "42"), &ts, None)).await;
    assert_eq!((v["type"].as_u64(), v["data"]["flags"].as_u64()), (Some(4), Some(64)));
    assert!(v["data"]["content"].as_str().unwrap().contains("Only people"), "{v}");
    let id = install["id"].as_str().unwrap();
    w.a.req("PATCH", &format!("/v1/code/agents/acme/integrations/discord/{id}"), Some(json!({ "members": ["42"] })), Some(&w.owner)).await;
    w.automation("discord");
    let (_, v) = send(&w.a, req(&command("i2", "42"), &ts, None)).await;
    assert_eq!(v["data"]["content"], "Sent to roll.");
    send(&w.a, req(&command("i2", "42"), &ts, None)).await;
    until("the event", || !w.events().is_empty()).await;
    let ev = w.events();
    assert_eq!(ev.len(), 1);
    assert_eq!((ev[0].1.as_str(), ev[0].2["prompt"].as_str(), ev[0].2["guild"].as_str()), ("command", Some("fix the build"), Some(guild.as_str())));

    // A status card's Reply button opens a modal at once.
    let click = json!({ "type": 3, "id": "i3", "application_id": "app1", "token": "tok", "guild_id": guild, "channel_id": "C9", "message": { "id": "m1" },
        "member": { "user": { "id": "42", "username": "ada" } }, "data": { "custom_id": "roll.steer:cht_x", "component_type": 2 } })
    .to_string();
    let (_, v) = send(&w.a, req(&click, &ts, None)).await;
    assert_eq!(v["type"], 9, "{v}");
}
