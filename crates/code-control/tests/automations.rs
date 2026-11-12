//! roll code automations (docs/code/automations.md) end to end through the API: a fake daemon
//! speaking the relay protocol over a real WebSocket, mock HTTP servers for webhooks, restarts
//! on the same state directory, two nodes on one control database.

mod common;

use agent_proto::{Command, DaemonFrame, Entity, RelayFrame, Reply, Stamped, StreamEvent};
use axum::http::StatusCode;
use common::{App, app_with};
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const ADMIN: &str = "admin-secret";
const SECRET_KEY: &str = "0123456789abcdef0123456789abcdef";
/// How long a helper waits for the state it checks (a run ending, a machine online, the next
/// event): long, since a loaded runner is slow; each returns as soon as the state is there.
const PATIENCE: Duration = Duration::from_secs(60);

fn config(c: &mut code_control::Config) {
    c.admin_token = Some(ADMIN.into());
    c.review.secret_key = Some(SECRET_KEY.into());
    c.allow_private_webhooks = Some(true);
}

struct World {
    a: App,
    url: String,
    owner: String,
    agent: String,
}

async fn world() -> World {
    let a = app_with(config);
    world_on(a, true).await
}

/// A world on `a`; `serve` binds a port (a fake daemon needs one, and keeps the app alive).
async fn world_on(a: App, serve: bool) -> World {
    for o in ["acme", "other"] {
        let (s, v) = a.req("POST", "/v1/code/owners", Some(json!({ "name": o, "kind": "org" })), Some(ADMIN)).await;
        assert!(s == StatusCode::CREATED || s == StatusCode::CONFLICT, "{v}");
    }
    let owner = mint(&a, "acme", "me").await;
    let agent = mint(&a, "acme", "agent:bot").await;
    a.code.automations.set_tick(Duration::from_millis(50));
    let url = if serve { a.serve().await } else { String::new() };
    World { a, url, owner, agent }
}

async fn mint(a: &App, owner: &str, name: &str) -> String {
    let (s, t) = a.req("POST", &format!("/v1/code/owners/{owner}/tokens"), Some(json!({ "name": name })), Some(ADMIN)).await;
    assert_eq!(s, StatusCode::CREATED);
    t["token"].as_str().unwrap().to_string()
}

impl World {
    async fn req(&self, method: &str, path: &str, body: Option<Value>, who: &str) -> (StatusCode, Value) {
        self.a.req(method, path, body, Some(who)).await
    }

    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (s, v) = self.req(method, path, body, &self.owner).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }

    async fn create(&self, spec: Value) -> Value {
        let (s, v) = self.req("POST", "/v1/code/agents/acme/automations", Some(spec), &self.owner).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        v
    }

    async fn run(&self, id: &str, body: Value) -> Value {
        let (s, v) = self.req("POST", &format!("/v1/code/agents/acme/automations/{id}/run"), Some(body), &self.owner).await;
        assert!(s == StatusCode::ACCEPTED || s == StatusCode::OK, "{s} {v}");
        v
    }

    /// Poll a run until it's final (20 s at most).
    async fn finished(&self, run: &str) -> Value {
        let (mut last, deadline) = (Value::Null, tokio::time::Instant::now() + PATIENCE);
        while tokio::time::Instant::now() < deadline {
            last = self.ok("GET", &format!("/v1/code/agents/acme/automations/runs/{run}"), None).await;
            if matches!(last["run"]["status"].as_str(), Some("succeeded" | "failed" | "cancelled" | "skipped")) {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("run {run} never ended: {last}");
    }

    /// Poll until `f` holds of the owner's runs.
    async fn runs_until(&self, f: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let (mut last, deadline) = (vec![], tokio::time::Instant::now() + PATIENCE);
        while tokio::time::Instant::now() < deadline {
            last = self.ok("GET", "/v1/code/agents/acme/automations/runs?limit=200", None).await.as_array().unwrap().clone();
            if f(&last) {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the runs never got there: {last:?}");
    }
}

/// A repo of acme's.
async fn repo(w: &World, name: &str) {
    let (s, v) = w.a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": name })), Some(ADMIN)).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
}

fn notify(id: &str, title: &str) -> Value {
    json!({ "id": id, "kind": "notify", "config": { "title": title } })
}

fn entity(v: Value) -> Entity {
    serde_json::from_value(v).unwrap()
}

// ── a fake daemon ──

/// A machine whose daemon starts threads that finish on their own, and records every command
/// it was sent (by id).
struct Fake {
    machine: String,
    commands: Arc<Mutex<Vec<(String, Command)>>>,
    out: mpsc::UnboundedSender<DaemonFrame>,
    seq: Arc<AtomicU64>,
}

fn thread_json(id: &str, project: &str, status: &str, title: &str, run: Option<&str>) -> Value {
    json!({
        "kind": "thread", "id": id, "project": project, "title": title, "provider": "codex", "model": null,
        "runtime_mode": "full_access", "interaction_mode": "default", "cwd": "/src/app", "status": status,
        "active_run": run, "run_count": 1, "pending_requests": 0, "created_at": 1, "updated_at": 1
    })
}

fn run_json(id: &str, thread: &str, status: &str, prompt: &str) -> Value {
    json!({ "kind": "run", "id": id, "thread": thread, "ordinal": 1, "provider": "codex", "model": null, "status": status, "prompt": prompt, "requested_at": 1 })
}

impl Fake {
    async fn start(w: &World, name: &str) -> Fake {
        let (s, v) = w.req("POST", "/v1/code/agents/acme/machines", Some(json!({ "name": name })), &w.owner).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        let machine = v["machine"]["id"].as_str().unwrap().to_string();
        let token = v["token"].as_str().unwrap().to_string();
        let mut req = format!("{}/v1/code/agents/connect", w.url.replace("http://", "ws://")).into_client_request().unwrap();
        req.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let (mut tx, mut rx) = ws.split();
        let hello = DaemonFrame::Hello { protocol: agent_proto::PROTOCOL, info: common::daemon::info() };
        tx.send(Message::text(serde_json::to_string(&hello).unwrap())).await.unwrap();
        let (out, mut outq) = mpsc::unbounded_channel::<DaemonFrame>();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let seq = Arc::new(AtomicU64::new(10));
        let fake = Fake { machine: machine.clone(), commands: commands.clone(), out: out.clone(), seq: seq.clone() };
        // A project the threads start in.
        fake.upsert(vec![entity(json!({ "kind": "project", "id": "prj_app", "name": "app", "path": "/src/app", "repo": { "owner": "acme", "repo": "app" }, "created_at": 1, "updated_at": 1 }))]);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    f = outq.recv() => match f {
                        Some(f) => if tx.send(Message::text(serde_json::to_string(&f).unwrap())).await.is_err() { return },
                        None => return,
                    },
                    m = rx.next() => {
                        let Some(Ok(Message::Text(t))) = m else {
                            if m.is_none() { return }
                            continue;
                        };
                        let Ok(f) = serde_json::from_str::<RelayFrame>(t.as_str()) else { continue };
                        match f {
                            RelayFrame::Ping => { let _ = out.send(DaemonFrame::Pong); }
                            RelayFrame::Command { id, command, .. } => {
                                commands.lock().push((id.clone(), (*command).clone()));
                                let n = commands.lock().len();
                                let reply = answer(&out, &seq, &command, n);
                                let _ = out.send(DaemonFrame::Reply { id, result: reply });
                            }
                            _ => {}
                        }
                    }
                }
            }
        });
        wait_online(w, &machine).await;
        fake
    }

    fn upsert(&self, entities: Vec<Entity>) {
        let items = entities.into_iter().map(|e| Stamped { seq: self.seq.fetch_add(1, Ordering::SeqCst), entity: e }).collect();
        let _ = self.out.send(DaemonFrame::Entities { items });
    }

    fn sent(&self, kind: &str) -> Vec<(String, Command)> {
        self.commands.lock().iter().filter(|(_, c)| serde_json::to_value(c).unwrap()["type"] == kind).cloned().collect()
    }
}

/// What the fake daemon does with a command: records first, then the reply.
fn answer(out: &mpsc::UnboundedSender<DaemonFrame>, seq: &Arc<AtomicU64>, c: &Command, n: usize) -> Reply {
    let send = |es: Vec<Value>| {
        let items = es.into_iter().map(|e| Stamped { seq: seq.fetch_add(1, Ordering::SeqCst), entity: entity(e) }).collect();
        let _ = out.send(DaemonFrame::Entities { items });
    };
    match c {
        Command::ThreadCreate { prompt, title, .. } => {
            let (t, r) = (format!("thr_{n:04}"), format!("run_{n:04}"));
            let prompt = prompt.clone().unwrap_or_default();
            let title = title.clone().unwrap_or_else(|| "agent".into());
            send(vec![thread_json(&t, "prj_app", "running", &title, Some(&r)), run_json(&r, &t, "running", &prompt)]);
            let (out, seq) = (out.clone(), seq.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let node = json!({ "kind": "node", "id": format!("nod_{n:04}"), "thread": t, "run": r, "ordinal": 1, "status": "completed", "started_at": 1, "body": { "type": "message", "text": format!("did: {prompt}") } });
                let items = [run_json(&r, &t, "completed", &prompt), node, thread_json(&t, "prj_app", "idle", &title, None)]
                    .into_iter()
                    .map(|e| Stamped { seq: seq.fetch_add(1, Ordering::SeqCst), entity: entity(e) })
                    .collect();
                let _ = out.send(DaemonFrame::Entities { items });
            });
            Reply::Created { id: format!("thr_{n:04}") }
        }
        Command::ProjectAdd { clone: Some(r), .. } => {
            send(vec![json!({ "kind": "project", "id": "prj_clone", "name": r.repo, "path": "/src/clone", "repo": { "owner": r.owner, "repo": r.repo }, "created_at": 1, "updated_at": 1 })]);
            Reply::Created { id: "prj_clone".into() }
        }
        Command::MessageSend { .. } => Reply::Created { id: format!("run_m{n}") },
        Command::ThreadPublish { .. } => serde_json::from_value(json!({ "type": "published", "branch": "roll/fix", "commit": "abc123" })).unwrap(),
        _ => Reply::Ok,
    }
}

async fn wait_online(w: &World, machine: &str) {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while tokio::time::Instant::now() < deadline {
        let (_, v) = w.req("GET", "/v1/code/agents/acme/machines", None, &w.owner).await;
        if v.as_array().unwrap().iter().any(|m| m["id"] == machine && m["online"] == true) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{machine} never came online");
}

// ── a mock HTTP server ──

/// Answers with the statuses in `script` in turn (then 200), recording each request.
struct Mock {
    url: String,
    hits: Arc<Mutex<Vec<(axum::http::HeaderMap, String)>>>,
}

async fn mock(script: Vec<u16>) -> Mock {
    let hits = Arc::new(Mutex::new(Vec::new()));
    let n = Arc::new(AtomicUsize::new(0));
    let (h, script) = (hits.clone(), Arc::new(script));
    let app = axum::Router::new().fallback(move |headers: axum::http::HeaderMap, body: String| {
        let (h, n, script) = (h.clone(), n.clone(), script.clone());
        async move {
            h.lock().push((headers, body));
            let i = n.fetch_add(1, Ordering::SeqCst);
            let code = script.get(i).copied().unwrap_or(200);
            (StatusCode::from_u16(code).unwrap(), axum::Json(json!({ "ok": code < 300, "n": i })))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Mock { url, hits }
}

// ── the tests ──

/// Definitions: saved and checked (templates, kinds, filters, schedules), listed, replaced over a
/// revision, enabled, disabled, deleted; the catalog lists the built-in triggers and steps; a dry
/// run fills templates in and runs nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn automations_are_saved_checked_and_listed() {
    let w = world().await;
    let base = "/v1/code/agents/acme/automations";
    for (bad, why) in [
        (json!({ "name": "", "trigger": { "type": "manual" }, "steps": [notify("a", "x")] }), "name"),
        (json!({ "name": "x", "trigger": { "type": "manual" }, "steps": [] }), "steps"),
        (json!({ "name": "x", "trigger": { "type": "manual" }, "steps": [{ "id": "a", "kind": "nope" }] }), "steps.0.kind"),
        (json!({ "name": "x", "trigger": { "type": "manual" }, "steps": [notify("a", "{{oops")] }), "steps.0.config.title"),
        (json!({ "name": "x", "trigger": { "type": "manual" }, "steps": [notify("a", "{{secrets.T}}")] }), "secrets"),
        (json!({ "name": "x", "trigger": { "type": "manual" }, "steps": [notify("a", "x"), notify("a", "y")] }), "twice"),
        (json!({ "name": "x", "trigger": { "type": "schedule", "cron": "99 * * * *" }, "steps": [notify("a", "x")] }), "cron"),
        (json!({ "name": "x", "trigger": { "type": "interval", "every_s": 5 }, "steps": [notify("a", "x")] }), "every_s"),
        (json!({ "name": "x", "trigger": { "type": "code", "events": ["nope"] }, "steps": [notify("a", "x")] }), "trigger.events"),
        (json!({ "name": "x", "trigger": { "type": "integration", "source": "nope" }, "steps": [notify("a", "x")] }), "trigger.source"),
        (json!({ "name": "x", "trigger": { "type": "manual" }, "filters": [{ "field": "event..x", "op": "exists" }], "steps": [notify("a", "x")] }), "filters"),
        (json!({ "name": "x", "trigger": { "type": "manual" }, "steps": [{ "id": "w", "kind": "webhook", "config": { "url": "ftp://x" } }] }), "url"),
    ] {
        let (s, v) = w.req("POST", base, Some(bad.clone()), &w.owner).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {v}");
        assert!(v["error"].as_str().unwrap().contains(why), "{bad}: {v}");
    }
    let saved = w.create(json!({ "name": "Nightly", "trigger": { "type": "schedule", "cron": "0 3 * * *", "timezone": "Europe/Budapest" }, "steps": [notify("a", "hi {{owner}}")] })).await;
    let a = &saved["automation"];
    let id = a["id"].as_str().unwrap();
    assert!(id.starts_with("aut_"));
    assert_eq!((a["revision"].as_u64(), a["enabled"].as_bool(), a["created_by"].as_str()), (Some(1), Some(true), Some("acme")));
    assert!(a["next_at"].as_u64().unwrap() > 0, "{a}");
    assert!(saved["webhook_secret"].is_null());
    let list = w.ok("GET", base, None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    // Replaced over a stale revision: 409 with what's there.
    let spec = json!({ "name": "Nightly", "trigger": { "type": "manual" }, "steps": [notify("a", "x")], "revision": 7 });
    let (s, v) = w.req("PUT", &format!("{base}/{id}"), Some(spec.clone()), &w.owner).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["current"]["revision"], 1);
    let mut spec = spec;
    spec["revision"] = json!(1);
    let v = w.ok("PUT", &format!("{base}/{id}"), Some(spec)).await;
    assert_eq!((v["automation"]["revision"].as_u64(), v["automation"]["next_at"].as_u64()), (Some(2), None));
    let v = w.ok("POST", &format!("{base}/{id}/disable"), None).await;
    assert_eq!(v["enabled"], false);
    let v = w.ok("POST", &format!("{base}/{id}/enable"), None).await;
    assert_eq!(v["enabled"], true);
    let cat = w.ok("GET", &format!("{base}/catalog"), None).await;
    let sources: Vec<&str> = cat["triggers"].as_array().unwrap().iter().map(|t| t["source"].as_str().unwrap()).collect();
    for s in ["manual", "schedule", "interval", "webhook", "code", "agent"] {
        assert!(sources.contains(&s), "{s}: {sources:?}");
    }
    let kinds: Vec<&str> = cat["steps"].as_array().unwrap().iter().map(|t| t["kind"].as_str().unwrap()).collect();
    for k in agent_proto::automation::step_kinds::ALL {
        assert!(kinds.contains(&k), "{k}");
        let s = cat["steps"].as_array().unwrap().iter().find(|s| s["kind"] == k).unwrap();
        assert_eq!(s["config"]["type"], "object", "{k}");
    }
    // A dry run says what an automation would do with an event (filters, templates, an unknown
    // kind's error) and runs nothing.
    let spec = json!({ "name": "t", "trigger": { "type": "code", "events": ["landed"] },
        "filters": [{ "field": "event.data.repo", "op": "equals", "value": "acme/app" }, { "field": "event.data.change.labels", "op": "has_label", "value": "bug" }],
        "steps": [notify("a", "{{event.data.change.title | upper}}"), { "id": "b", "kind": "nope" }] });
    let r = w.ok("POST", &format!("{base}/test"), Some(json!({ "automation": spec, "event": { "source": "code", "kind": "landed", "data": { "repo": "acme/app", "change": { "title": "fix", "labels": ["bug"] } } } }))).await;
    assert_eq!((r["matched"].as_bool(), r["trigger"].as_bool()), (Some(true), Some(true)), "{r}");
    assert_eq!(r["steps"][0]["config"]["title"], "FIX");
    assert!(r["steps"][1]["error"].as_str().unwrap().contains("nope"));
    assert!(w.ok("GET", &format!("{base}/runs"), None).await.as_array().unwrap().is_empty());
    let (s, _) = w.req("DELETE", &format!("{base}/{id}"), None, &w.owner).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(w.ok("GET", base, None).await, json!([]));
    let (s, _) = w.req("GET", &format!("{base}/{id}"), None, &w.owner).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

/// The dashboard's copy of the built-in triggers and steps (`apps/dashboard/src/lib/automation-catalog.json`:
/// its template tests render every template against these triggers' samples) is what the registry
/// says. `ROLL_FIXTURES=write` rewrites it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_dashboard_has_the_built_in_catalog() {
    let w = world().await;
    let cat = w.ok("GET", "/v1/code/agents/acme/automations/catalog", None).await;
    let built_in = ["manual", "schedule", "interval", "webhook", "code", "agent"];
    let mut triggers: Vec<Value> = cat["triggers"].as_array().unwrap().iter().filter(|t| built_in.contains(&t["source"].as_str().unwrap())).cloned().collect();
    let mut steps: Vec<Value> = cat["steps"].as_array().unwrap().iter().filter(|s| s["integration"].is_null()).cloned().collect();
    for v in triggers.iter_mut().chain(steps.iter_mut()) {
        v.as_object_mut().unwrap().remove("needs_setup");
    }
    assert_eq!((triggers.len(), steps.len()), (built_in.len(), agent_proto::automation::step_kinds::ALL.len()));
    for t in &triggers {
        for e in t["events"].as_array().unwrap() {
            assert!(e["sample"].is_object(), "{} {} has no sample", t["source"], e["kind"]);
        }
    }
    let want = json!({ "triggers": triggers, "steps": steps });
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/dashboard/src/lib/automation-catalog.json");
    if std::env::var("ROLL_FIXTURES").as_deref() == Ok("write") {
        std::fs::write(&path, serde_json::to_string_pretty(&want).unwrap() + "\n").unwrap();
    }
    let have: Value = serde_json::from_str(&std::fs::read_to_string(&path).expect("the fixture (ROLL_FIXTURES=write)")).unwrap();
    assert_eq!(have, want, "apps/dashboard/src/lib/automation-catalog.json is stale: ROLL_FIXTURES=write cargo test -p code-control --test automations");
}

/// tells the owner: every step's output feeds the next one's templates, the run lists the thread
/// it started, and the stream carries the run and the notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_starts_an_agent_waits_for_it_and_notifies() {
    let w = world().await;
    let fake = Fake::start(&w, "box").await;
    let a = w
        .create(json!({
            "name": "Fix it",
            "trigger": { "type": "manual" },
            "defaults": { "provider": "codex", "worktree": true },
            "steps": [
                { "id": "start", "kind": "agent.start", "config": {
                    "target": { "type": "machine", "machine": "box" }, "project": { "type": "id", "id": "prj_app" },
                    "title": "Fix {{event.data.input.what}}", "prompt": "Please fix {{event.data.input.what}} ({{automation.name}})", "orchestrator": true } },
                { "id": "wait", "kind": "agent.wait", "config": { "thread": "{{steps.start.thread}}", "timeout_s": 30 } },
                { "id": "tell", "kind": "notify", "config": { "title": "{{steps.wait.outcome}}: {{steps.wait.message}}", "level": "success" } },
            ],
        }))
        .await;
    let id = a["automation"]["id"].as_str().unwrap();
    let mut events = Events::open(&w, &w.owner).await;
    let run = w.run(id, json!({ "input": { "what": "the queue" }, "key": "k1" })).await;
    let rid = run["id"].as_str().unwrap().to_string();
    let done = w.finished(&rid).await;
    let r = &done["run"];
    assert_eq!(r["status"], "succeeded", "{done}");
    assert_eq!(r["steps"][0]["output"]["thread"], "thr_0001");
    assert_eq!(r["steps"][0]["output"]["machine"], fake.machine.as_str());
    assert_eq!(r["steps"][1]["output"]["outcome"], "done");
    assert_eq!(r["steps"][1]["output"]["message"], "did: Please fix the queue (Fix it)");
    assert_eq!(r["threads"][0]["thread"], "thr_0001");
    assert_eq!(done["event"]["data"]["input"]["what"], "the queue");
    let creates = fake.sent("thread.create");
    assert_eq!(creates.len(), 1);
    match &creates[0].1 {
        Command::ThreadCreate { title, worktree, orchestrator, provider, project, .. } => {
            assert_eq!((title.as_deref(), *worktree, *orchestrator, provider.as_str(), project.as_str()), (Some("Fix the queue"), true, true, "codex", "prj_app"));
        }
        other => panic!("{other:?}"),
    }
    assert!(creates[0].0.starts_with("auto_"), "ids are made from the run and step: {}", creates[0].0);
    // The same key is the same run.
    let again = w.run(id, json!({ "key": "k1" })).await;
    assert_eq!(again["id"], rid.as_str());
    // The stream: the run's changes and the notice.
    let (mut saw_run, mut saw_notice) = (false, false);
    while !(saw_run && saw_notice) {
        match events.next().await {
            StreamEvent::AutomationRun { run, .. } if run.id.as_str() == rid && run.status == agent_proto::automation::RunState::Succeeded => saw_run = true,
            StreamEvent::Notice { notice } => {
                assert_eq!(notice.title, "done: did: Please fix the queue (Fix it)");
                saw_notice = true;
            }
            _ => {}
        }
    }
    // Kept for a dashboard that wasn't open (A12): listed newest first, with its run.
    let kept = w.ok("GET", "/v1/code/agents/acme/automations/notices", None).await;
    assert_eq!(kept[0]["title"], "done: did: Please fix the queue (Fix it)", "{kept}");
    assert_eq!(kept[0]["run"], rid.as_str());
    let older = w.ok("GET", &format!("/v1/code/agents/acme/automations/notices?before={}", kept[0]["at"]), None).await;
    assert!(older.as_array().unwrap().iter().all(|n| n["id"] != kept[0]["id"]), "{older}");
}

/// A repo project the machine doesn't have is cloned onto it; any online machine is picked; a
/// message, publishing with land, and settling go to the thread, each once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_steps_clone_message_publish_and_settle() {
    let w = world().await;
    let fake = Fake::start(&w, "box").await;
    repo(&w, "web").await;
    let a = w
        .create(json!({
            "name": "Ship it",
            "trigger": { "type": "manual" },
            "steps": [
                { "id": "start", "kind": "agent.start", "config": { "target": { "type": "any_online" }, "project": { "type": "repo", "repo": "web" }, "provider": "codex", "prompt": "go" } },
                { "id": "nudge", "kind": "agent.message", "config": { "text": "and the docs", "mode": "steer" } },
                { "id": "ship", "kind": "agent.publish", "config": { "message": "Ship {{steps.start.thread}}" } },
                { "id": "done", "kind": "agent.settle", "config": { "action": "archive" } },
            ],
        }))
        .await;
    let rid = w.run(a["automation"]["id"].as_str().unwrap(), json!({})).await["id"].as_str().unwrap().to_string();
    let done = w.finished(&rid).await;
    assert_eq!(done["run"]["status"], "succeeded", "{done}");
    assert_eq!(fake.sent("project.add").len(), 1, "cloned once");
    assert_eq!(done["run"]["steps"][2]["output"]["branch"], "roll/fix");
    match &fake.sent("message.send")[0].1 {
        Command::MessageSend { text, mode, thread, .. } => assert_eq!((text.as_str(), *mode, thread.as_str()), ("and the docs", agent_proto::SendMode::Steer, "thr_0002")),
        other => panic!("{other:?}"),
    }
    match &fake.sent("thread.publish")[0].1 {
        Command::ThreadPublish { message, .. } => assert_eq!(message, "Ship thr_0002"),
        other => panic!("{other:?}"),
    }
    assert_eq!(fake.sent("thread.archive").len(), 1);
}

/// Outgoing webhooks: retried with backoff on a 5xx, signed with the automation's secret, with
/// secrets in headers; the secret's value is never returned, nor in the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhooks_retry_and_secrets_stay_sealed() {
    let w = world().await;
    let m = mock(vec![503]).await;
    let a = w
        .create(json!({
            "name": "Tell CI",
            "trigger": { "type": "manual" },
            "steps": [{ "id": "tell", "kind": "webhook", "retry": { "attempts": 3, "backoff_s": 1 }, "config": {
                "url": format!("{}/hook", m.url), "headers": { "authorization": "Bearer {{secrets.TOKEN}}" },
                "body": { "who": "{{owner}}", "input": "{{event.data.input}}" }, "secret": "SIGN" } }],
        }))
        .await;
    let id = a["automation"]["id"].as_str().unwrap();
    for (name, value) in [("TOKEN", "s3cret-token"), ("SIGN", "rlh_signing")] {
        let (s, v) = w.req("PUT", &format!("/v1/code/agents/acme/automations/{id}/secrets/{name}"), Some(json!({ "value": value })), &w.owner).await;
        assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    }
    let got = w.ok("GET", &format!("/v1/code/agents/acme/automations/{id}"), None).await;
    assert_eq!(got["secrets"], json!(["SIGN", "TOKEN"]));
    assert!(!got.to_string().contains("s3cret"));
    let rid = w.run(id, json!({ "input": { "n": 1 } })).await["id"].as_str().unwrap().to_string();
    let done = w.finished(&rid).await;
    assert_eq!(done["run"]["status"], "succeeded", "{done}");
    let step = &done["run"]["steps"][0];
    assert_eq!((step["attempt"].as_u64(), step["output"]["status"].as_u64()), (Some(2), Some(200)));
    assert!(!done.to_string().contains("s3cret"), "{done}");
    let hits = m.hits.lock().clone();
    assert_eq!(hits.len(), 2);
    let (h, body) = &hits[1];
    assert_eq!(h["authorization"], "Bearer s3cret-token");
    assert_eq!(h["x-roll-delivery"], hits[0].0["x-roll-delivery"], "the same key on every attempt");
    let ts: i64 = h["x-roll-timestamp"].to_str().unwrap().parse().unwrap();
    assert_eq!(h["x-roll-signature-256"].to_str().unwrap(), code_control::review::apps::signature("rlh_signing", ts, body.as_bytes()));
    assert_eq!(serde_json::from_str::<Value>(body).unwrap(), json!({ "who": "acme", "input": { "n": 1 } }));
}

/// Inbound webhooks: signed with the automation's secret (roll's or GitHub's signature), stale
/// or wrong signatures refused, a redelivery recorded once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbound_webhooks_are_signed_and_idempotent() {
    let w = world().await;
    let saved = w.create(json!({ "name": "From CI", "trigger": { "type": "webhook" }, "steps": [notify("t", "{{event.data.body.status}}")] })).await;
    let id = saved["automation"]["id"].as_str().unwrap();
    let secret = saved["webhook_secret"].as_str().unwrap().to_string();
    assert!(secret.starts_with("rlh_"));
    assert!(saved["automation"]["webhook"]["url"].as_str().unwrap().ends_with(&format!("/v1/code/agents/acme/automations/{id}/webhook")));
    let hook = format!("{}/v1/code/agents/acme/automations/{id}/webhook", w.url);
    let http = reqwest::Client::new();
    let body = r#"{"status":"green"}"#;
    let post = |sig: String, ts: i64, delivery: &'static str| http.post(&hook).header("x-roll-signature-256", sig).header("x-roll-timestamp", ts.to_string()).header("x-roll-delivery", delivery).body(body).send();
    let now = code_sql::now_ms() / 1000;
    let good = code_control::review::apps::signature(&secret, now, body.as_bytes());
    assert_eq!(post("sha256=00".into(), now, "d1").await.unwrap().status(), 401);
    assert_eq!(post(code_control::review::apps::signature(&secret, now - 600, body.as_bytes()), now - 600, "d1").await.unwrap().status(), 401, "stale");
    let r = post(good.clone(), now, "d1").await.unwrap();
    assert_eq!(r.status(), 202);
    assert_eq!(r.json::<Value>().await.unwrap()["duplicate"], false);
    let r = post(good, now, "d1").await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["duplicate"], true);
    let runs = w.runs_until(|r| r.iter().any(|x| x["status"] == "succeeded")).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["event"]["source"], "webhook");
    // GitHub's signature, keyed by its delivery id.
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    let gh = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    let r = http.post(&hook).header("x-hub-signature-256", gh).header("x-github-delivery", "g-1").body(body).send().await.unwrap();
    assert_eq!(r.status(), 202);
    w.runs_until(|r| r.len() == 2).await;
    // A new secret: the old one stops working.
    let rotated = w.ok("POST", &format!("/v1/code/agents/acme/automations/{id}/webhook/secret"), None).await;
    let fresh = rotated["webhook_secret"].as_str().unwrap().to_string();
    assert_ne!(fresh, secret);
    let now = code_sql::now_ms() / 1000;
    let old = code_control::review::apps::signature(&secret, now, body.as_bytes());
    assert_eq!(post(old, now, "d2").await.unwrap().status(), 401);
    let secret = fresh;
    // A disabled automation's webhook says so.
    w.ok("POST", &format!("/v1/code/agents/acme/automations/{id}/disable"), None).await;
    let now = code_sql::now_ms() / 1000;
    let r = http
        .post(&hook)
        .header("x-roll-signature-256", code_control::review::apps::signature(&secret, now, body.as_bytes()))
        .header("x-roll-timestamp", now.to_string())
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
}

/// Agent events start runs: a thread finishing (from the mirror, in its own write) runs an
/// automation listening for thread.done; its filters see the event; a replay records nothing twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_events_start_runs_once() {
    let w = world().await;
    let fake = Fake::start(&w, "box").await;
    w.create(json!({
        "name": "On done",
        "trigger": { "type": "agent", "events": ["thread.done"] },
        "filters": [{ "field": "event.data.run.prompt", "op": "contains", "value": "deploy" }],
        "steps": [notify("t", "{{event.data.thread.id}} done")],
    }))
    .await;
    let done = |seq: u64, prompt: &str| Stamped { seq, entity: entity(run_json("run_x", "thr_x", "completed", prompt)) };
    let _ = fake.out.send(DaemonFrame::Entities { items: vec![Stamped { seq: 99, entity: entity(thread_json("thr_x", "prj_x", "running", "Deploy the app", Some("run_x"))) }] });
    let _ = fake.out.send(DaemonFrame::Entities { items: vec![Stamped { seq: 100, entity: entity(run_json("run_x", "thr_x", "running", "deploy it")) }] });
    let _ = fake.out.send(DaemonFrame::Entities { items: vec![done(101, "deploy it")] });
    // The same record again (a reconnect's replay) and a run the filter refuses.
    let _ = fake.out.send(DaemonFrame::Entities { items: vec![done(101, "deploy it")] });
    let _ = fake.out.send(DaemonFrame::Entities { items: vec![Stamped { seq: 102, entity: entity(run_json("run_y", "thr_y", "completed", "just look")) }] });
    let runs = w.runs_until(|r| r.iter().any(|x| x["status"] == "succeeded")).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let runs2 = w.ok("GET", "/v1/code/agents/acme/automations/runs", None).await;
    assert_eq!(runs2.as_array().unwrap().len(), 1, "{runs2}");
    assert_eq!(runs[0]["event"]["kind"], "thread.done");
    // A run's event carries its thread as the thread's own events do (templates read
    // `event.data.thread.title`), and has every field the catalog's sample shows.
    let detail = w.ok("GET", &format!("/v1/code/agents/acme/automations/runs/{}", runs[0]["id"].as_str().unwrap()), None).await;
    let data = &detail["event"]["data"];
    assert_eq!((data["thread"]["id"].as_str(), data["thread"]["title"].as_str(), data["project"].as_str()), (Some("thr_x"), Some("Deploy the app"), Some("prj_x")), "{data}");
    assert_eq!(data["run"]["prompt"], "deploy it");
    has_the_samples_fields(&w, "agent", "thread.done", data).await;
}

/// Every object path of `v` (`thread`, `thread.title`; not into lists).
fn paths(v: &Value, at: &str, out: &mut std::collections::BTreeSet<String>) {
    if let Some(m) = v.as_object() {
        for (k, x) in m {
            let p = if at.is_empty() { k.clone() } else { format!("{at}.{k}") };
            paths(x, &p, out);
            out.insert(p);
        }
    }
}

/// A real event of `source`/`kind` has every field its sample in the catalog has: the
/// builder's tests and the dashboard's templates read the samples as the events' shapes.
async fn has_the_samples_fields(w: &World, source: &str, kind: &str, real: &Value) {
    let cat = w.ok("GET", "/v1/code/agents/acme/automations/catalog", None).await;
    let t = cat["triggers"].as_array().unwrap().iter().find(|t| t["source"] == source).unwrap();
    let sample = &t["events"].as_array().unwrap().iter().find(|e| e["kind"] == kind).unwrap()["sample"];
    let (mut want, mut have) = Default::default();
    paths(sample, "", &mut want);
    paths(real, "", &mut have);
    let missing: Vec<&String> = want.difference(&have).collect();
    assert!(missing.is_empty(), "{source} {kind}: the sample has {missing:?}, the real event doesn't: {real}");
}

/// roll code events reach automations from the deliveries' write: a work item opened in a repo
/// the trigger names runs it, with the repo in the event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn code_events_start_runs() {
    let w = world().await;
    repo(&w, "app").await;
    repo(&w, "other").await;
    w.create(json!({ "name": "On work", "trigger": { "type": "code", "events": ["work"], "repos": ["app"] }, "steps": [notify("t", "{{event.data.repo}}")] })).await;
    w.ok("POST", "/v1/code/repos/acme/other/work", Some(json!({ "title": "not this one" }))).await;
    w.ok("POST", "/v1/code/repos/acme/app/work", Some(json!({ "title": "Fix the queue" }))).await;
    let runs = w.runs_until(|r| r.iter().any(|x| x["status"] == "succeeded")).await;
    assert_eq!(runs.len(), 1, "only the named repo: {runs:?}");
    let detail = w.ok("GET", &format!("/v1/code/agents/acme/automations/runs/{}", runs[0]["id"].as_str().unwrap()), None).await;
    assert_eq!(detail["event"]["data"]["repo"], "acme/app");
    assert_eq!(detail["event"]["source"], "code");
    assert_eq!(detail["event"]["data"]["title"], "Fix the queue");
    has_the_samples_fields(&w, "code", "work", &detail["event"]["data"]).await;
}

/// Concurrency: `skip` records a second run as skipped while one waits; `queue` holds it until
/// the first ends; `cancel_previous` cancels the first. A run cancelled by hand stops; a failed
/// one can be retried from its failed step.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrency_cancel_and_retry() {
    let w = world().await;
    let make = |policy: &str| {
        json!({ "name": format!("delay {policy}"), "trigger": { "type": "manual" }, "concurrency": policy,
                "steps": [{ "id": "d", "kind": "delay", "config": { "seconds": 2 } }, notify("t", "x")] })
    };
    for (policy, second) in [("skip", "skipped"), ("queue", "queued"), ("cancel_previous", "running")] {
        let id = w.create(make(policy)).await["automation"]["id"].as_str().unwrap().to_string();
        let first = w.run(&id, json!({})).await["id"].as_str().unwrap().to_string();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let next = w.run(&id, json!({})).await;
        assert_eq!(next["status"], second, "{policy}: {next}");
        let first_now = w.ok("GET", &format!("/v1/code/agents/acme/automations/runs/{first}"), None).await;
        if policy == "cancel_previous" {
            assert_eq!(first_now["run"]["status"], "cancelled", "{first_now}");
        } else {
            assert!(matches!(first_now["run"]["status"].as_str(), Some("waiting" | "running")), "{first_now}");
        }
        let n = next["id"].as_str().unwrap().to_string();
        if policy != "skip" {
            assert_eq!(w.finished(&n).await["run"]["status"], "succeeded", "{policy}");
        }
    }
    // Cancelled by hand while it waits.
    let id = w.create(json!({ "name": "long", "trigger": { "type": "manual" }, "steps": [{ "id": "d", "kind": "delay", "config": { "seconds": 60 } }] })).await["automation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = w.run(&id, json!({})).await["id"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let c = w.ok("POST", &format!("/v1/code/agents/acme/automations/runs/{r}/cancel"), None).await;
    assert_eq!((c["status"].as_str(), c["steps"][0]["status"].as_str()), (Some("cancelled"), Some("cancelled")));
    // A failed run, retried: it goes on from the failed step.
    let m = mock(vec![400, 200]).await;
    let id = w
        .create(json!({ "name": "flaky", "trigger": { "type": "manual" }, "steps": [notify("a", "first"), { "id": "w", "kind": "webhook", "config": { "url": m.url } }] }))
        .await["automation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = w.run(&id, json!({})).await["id"].as_str().unwrap().to_string();
    let failed = w.finished(&r).await;
    assert_eq!(failed["run"]["status"], "failed", "a 4xx isn't retried: {failed}");
    let (s, _) = w.req("POST", &format!("/v1/code/agents/acme/automations/runs/{r}/retry"), None, &w.agent).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "agents can't");
    w.ok("POST", &format!("/v1/code/agents/acme/automations/runs/{r}/retry"), None).await;
    let done = w.finished(&r).await;
    assert_eq!(done["run"]["status"], "succeeded", "{done}");
    assert_eq!(done["run"]["steps"][0]["attempt"], 1, "the first step wasn't run again");
    // The retry is on the run, and the failed attempt stays in the step's log, before its new start.
    let retried = done["run"]["retried_at"].as_u64().expect("retried_at");
    assert!(retried >= failed["run"]["finished_at"].as_u64().unwrap(), "{done}");
    let log: Vec<(&str, &str, u64)> = done["run"]["steps"][1]["log"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| (l["level"].as_str().unwrap_or("info"), l["text"].as_str().unwrap(), l["at"].as_u64().unwrap()))
        .collect();
    let at = log.iter().position(|(_, t, _)| t.starts_with("Retried after the run failed")).unwrap_or_else(|| panic!("{log:?}"));
    assert!(log[..at].iter().any(|(level, t, _)| *level == "error" && t.contains("400")), "the failure stays: {log:?}");
    assert!(log[at].2 <= done["run"]["steps"][1]["started_at"].as_u64().unwrap(), "{log:?}");
    assert!(done["run"]["steps"][1].get("error").is_none(), "{done}");
}

/// Rate limits skip runs past them; agent tokens run an automation only when it allows agents.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limits_and_agent_tokens() {
    let w = world().await;
    let id = w.create(json!({ "name": "limited", "trigger": { "type": "manual" }, "rate_limit": { "max": 2, "per_s": 3600 }, "steps": [notify("t", "x")] })).await["automation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let states: Vec<String> = {
        let mut v = vec![];
        for _ in 0..3 {
            v.push(w.run(&id, json!({})).await["status"].as_str().unwrap().to_string());
        }
        v
    };
    assert_eq!(states[2], "skipped", "{states:?}");
    let (s, _) = w.req("POST", &format!("/v1/code/agents/acme/automations/{id}/run"), Some(json!({})), &w.agent).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let open = w.create(json!({ "name": "open", "trigger": { "type": "manual" }, "allow_agents": true, "steps": [notify("t", "{{event.data.by}}")] })).await;
    let oid = open["automation"]["id"].as_str().unwrap();
    let (s, r) = w.req("POST", &format!("/v1/code/agents/acme/automations/{oid}/run"), Some(json!({})), &w.agent).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    let d = w.finished(r["id"].as_str().unwrap()).await;
    assert_eq!(d["event"]["data"]["by"], "agent:bot");
}

/// Run now on an automation another trigger starts: `data` is the event's data as that trigger
/// sends it, so its conditions and its steps' templates read what was typed (A9).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_now_takes_the_triggers_event_data() {
    let w = world().await;
    let id = w
        .create(json!({ "name": "on landing", "trigger": { "type": "code", "events": ["landed"] },
            "filters": [{ "field": "event.data.repo", "op": "equals", "value": "acme/app" }],
            "steps": [notify("t", "{{event.data.change.title}}")] }))
        .await["automation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = w.run(&id, json!({ "data": { "repo": "acme/app", "change": { "title": "fix the login" } }, "input": { "note": "by hand" } })).await;
    let d = w.finished(r["id"].as_str().unwrap()).await;
    assert_eq!(d["run"]["status"], "succeeded", "{d}");
    let data = &d["event"]["data"];
    assert_eq!((data["repo"].as_str(), data["change"]["title"].as_str(), data["input"]["note"].as_str()), (Some("acme/app"), Some("fix the login"), Some("by hand")), "{d}");
    // roll says who ran it, whatever the data says.
    let r = w.run(&id, json!({ "data": { "repo": "acme/app", "by": "someone else" } })).await;
    assert_eq!(w.finished(r["id"].as_str().unwrap()).await["event"]["data"]["by"], data["by"]);
    assert_ne!(data["by"], "someone else");
    // Without the data its conditions don't hold.
    let r = w.run(&id, json!({})).await;
    assert_ne!(w.finished(r["id"].as_str().unwrap()).await["run"]["status"], "succeeded");
    // Data is an object.
    let (s, _) = w.req("POST", &format!("/v1/code/agents/acme/automations/{id}/run"), Some(json!({ "data": "main is red" })), &w.owner).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// Durability: a run parked on a delay when the process stops is resumed by the next one (on
/// the same state directory) and runs its remaining steps once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_resume_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let m = mock(vec![]).await;
    let (id, rid) = {
        let code = common::open_in(dir.path(), config);
        let a = App { router: code_control::api::router(code.clone()), code, dir: tempfile::tempdir().unwrap() };
        let w = world_on(a, false).await;
        let id = w
            .create(json!({ "name": "later", "trigger": { "type": "manual" }, "steps": [{ "id": "d", "kind": "delay", "config": { "seconds": 2 } }, { "id": "w", "kind": "webhook", "config": { "url": m.url } }] }))
            .await["automation"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let rid = w.run(&id, json!({})).await["id"].as_str().unwrap().to_string();
        tokio::time::sleep(Duration::from_millis(400)).await;
        (id, rid)
    };
    // The first process is gone (its engine stops with it); a new one picks the run up.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let code = common::open_in(dir.path(), config);
    let a = App { router: code_control::api::router(code.clone()), code: code.clone(), dir: tempfile::tempdir().unwrap() };
    let w = world_on(a, false).await;
    code.start_automations();
    let done = w.finished(&rid).await;
    assert_eq!(done["run"]["status"], "succeeded", "{done}");
    assert_eq!(m.hits.lock().len(), 1, "the webhook was called once");
    let _ = id;
}

/// Two nodes on one control database never work on one run at once: every run's webhook is
/// called exactly once though both engines race for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_never_run_a_run_twice() {
    let w = world().await;
    let m = mock(vec![]).await;
    let b = second_node(&w.a);
    b.code.automations.set_tick(Duration::from_millis(20));
    b.code.start_automations();
    let id = w.create(json!({ "name": "race", "trigger": { "type": "manual" }, "rate_limit": { "max": 1000, "per_s": 60 }, "steps": [{ "id": "w", "kind": "webhook", "config": { "url": m.url } }] })).await["automation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    const N: usize = 20;
    for i in 0..N {
        // Half the runs are asked for through the other node.
        let (s, v) = if i % 2 == 0 {
            w.req("POST", &format!("/v1/code/agents/acme/automations/{id}/run"), Some(json!({ "key": format!("k{i}") })), &w.owner).await
        } else {
            b.req("POST", &format!("/v1/code/agents/acme/automations/{id}/run"), Some(json!({ "key": format!("k{i}") })), Some(&w.owner)).await
        };
        assert!(s.is_success(), "{s} {v}");
    }
    w.runs_until(|r| r.len() == N && r.iter().all(|x| x["status"] == "succeeded")).await;
    let hits = m.hits.lock().clone();
    let mut keys: Vec<String> = hits.iter().map(|(h, _)| h["x-roll-delivery"].to_str().unwrap().to_string()).collect();
    keys.sort();
    let n = keys.len();
    keys.dedup();
    assert_eq!((n, keys.len()), (N, N), "each run's webhook once");
}

fn second_node(a: &App) -> App {
    let control = match a.code.db.sql().pg_schema() {
        Some(schema) => {
            let url = std::env::var(code_sql::TEST_DATABASE_ENV).unwrap();
            code_sql::Db::connect(&url, schema, code_sql::PgOptions { max_connections: 8, ..Default::default() }).unwrap()
        }
        None => code_sql::Db::open(&code_sql::Target::Sqlite(a.dir.path().join("code").join("control.db"))).unwrap(),
    };
    let dir = tempfile::tempdir().unwrap();
    let objects = pg_storage::Objects::new(&pg_storage::StorageConfig::Local(dir.path().join("objects"))).unwrap();
    let mut c = code_control::Config::local(dir.path().to_path_buf());
    config(&mut c);
    let code = code_control::Code::open_with_db(c, objects, control).unwrap();
    let router = code_control::api::router(code.clone());
    App { code, router, dir }
}

/// An SSE stream of the owner's events.
struct Events {
    body: axum::body::BodyDataStream,
    buf: String,
}

impl Events {
    async fn open(w: &World, who: &str) -> Events {
        let req = axum::http::Request::get("/v1/code/agents/acme/events").header("authorization", format!("Bearer {who}")).body(axum::body::Body::empty()).unwrap();
        let resp = w.a.send(req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        Events { body: resp.into_body().into_data_stream(), buf: String::new() }
    }

    async fn next(&mut self) -> StreamEvent {
        loop {
            if let Some(end) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..end + 2).collect();
                if let Some(d) = block.lines().find_map(|l| l.strip_prefix("data:")) {
                    return serde_json::from_str(d.trim()).unwrap();
                }
                continue;
            }
            let chunk = tokio::time::timeout(PATIENCE, self.body.next()).await.expect("an event in time").expect("open").unwrap();
            self.buf.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
}
