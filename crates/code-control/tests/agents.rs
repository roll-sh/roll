//! The agents relay (`src/agents/`, docs/code/agents.md): device login, machines, the daemon's
//! WebSocket (a test daemon speaking the real protocol over a real socket), the mirror behind
//! `/state` and `/events`, and commands.

mod common;

use agent_proto::{DaemonFrame, Entity, MachineInfo, RelayFrame, Reply, Stamped, StreamEvent};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{App, app};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const ADMIN: &str = "admin-secret";

/// `acme` with an owner token, an agent token and another owner's token.
struct World {
    a: App,
    url: String,
    owner: String,
    agent: String,
    other: String,
}

async fn world() -> World {
    let a = app(Some(ADMIN));
    for o in ["acme", "other"] {
        let (s, v) = a.req("POST", "/v1/code/owners", Some(json!({ "name": o, "kind": "org" })), Some(ADMIN)).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
    }
    let mint = |owner: &'static str, name: &'static str| {
        let a = &a;
        async move {
            let (s, t) = a.req("POST", &format!("/v1/code/owners/{owner}/tokens"), Some(json!({ "name": name })), Some(ADMIN)).await;
            assert_eq!(s, StatusCode::CREATED);
            t["token"].as_str().unwrap().to_string()
        }
    };
    let owner = mint("acme", "me").await;
    let agent = mint("acme", "agent:bot").await;
    let other = mint("other", "me").await;
    let url = a.serve().await;
    World { a, url, owner, agent, other }
}

impl World {
    async fn req(&self, method: &str, path: &str, body: Option<Value>, who: &str) -> (StatusCode, Value) {
        self.a.req(method, path, body, Some(who)).await
    }

    /// A machine added from the dashboard: (id, its token).
    async fn machine(&self, name: &str) -> (String, String) {
        let (s, v) = self.req("POST", "/v1/code/agents/acme/machines", Some(json!({ "name": name })), &self.owner).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        (v["machine"]["id"].as_str().unwrap().to_string(), v["token"].as_str().unwrap().to_string())
    }
}

fn info() -> MachineInfo {
    MachineInfo { hostname: "box".into(), os: "linux".into(), arch: "x86_64".into(), version: "0.1.0".into(), home: "/home/me".into(), providers: vec![] }
}

fn entity(v: Value) -> Entity {
    serde_json::from_value(v).unwrap()
}

fn project(id: &str, name: &str) -> Entity {
    entity(json!({ "kind": "project", "id": id, "name": name, "path": "/src/x", "created_at": 1, "updated_at": 1 }))
}

fn thread(id: &str, title: &str) -> Entity {
    entity(json!({
        "kind": "thread", "id": id, "project": "prj_1", "title": title, "provider": "codex", "model": null,
        "runtime_mode": "full_access", "interaction_mode": "default", "cwd": "/src/x", "status": "idle",
        "run_count": 0, "pending_requests": 0, "created_at": 1, "updated_at": 1
    }))
}

fn node(id: &str, thread: &str, text: &str) -> Entity {
    entity(json!({
        "kind": "node", "id": id, "thread": thread, "run": "run_1", "ordinal": 1, "status": "completed",
        "started_at": 1, "body": { "type": "message", "text": text }
    }))
}

fn stamped(seq: u64, entity: Entity) -> Stamped {
    Stamped { seq, entity }
}

/// A daemon speaking the relay protocol over a real WebSocket.
struct Daemon {
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
}

async fn dial(url: &str, bearer: &str) -> Result<Daemon, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("{}/v1/code/agents/connect", url.replace("http://", "ws://")).into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {bearer}").parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(req).await?;
    Ok(Daemon { ws })
}

impl Daemon {
    /// Connect and say hello: the welcome's `resume_from`.
    async fn connect(url: &str, bearer: &str) -> (Daemon, u64) {
        let mut d = dial(url, bearer).await.unwrap();
        d.send(&DaemonFrame::Hello { protocol: agent_proto::PROTOCOL, info: info() }).await;
        match d.recv().await {
            Some(RelayFrame::Welcome { resume_from, owner, .. }) => {
                assert_eq!(owner, "acme");
                (d, resume_from)
            }
            other => panic!("expected welcome, got {other:?}"),
        }
    }

    async fn send(&mut self, f: &DaemonFrame) {
        self.ws.send(Message::text(serde_json::to_string(f).unwrap())).await.unwrap();
    }

    /// The next relay frame (pings answered), or `None` once closed.
    async fn recv(&mut self) -> Option<RelayFrame> {
        self.recv_or_close().await.ok()
    }

    /// The next relay frame, or the close code.
    async fn recv_or_close(&mut self) -> Result<RelayFrame, Option<u16>> {
        loop {
            let m = tokio::time::timeout(Duration::from_secs(10), self.ws.next()).await.expect("a frame within 10 s");
            match m {
                Some(Ok(Message::Text(t))) => {
                    let f: RelayFrame = serde_json::from_str(t.as_str()).unwrap();
                    if f == RelayFrame::Ping {
                        self.send(&DaemonFrame::Pong).await;
                        continue;
                    }
                    return Ok(f);
                }
                Some(Ok(Message::Close(c))) => return Err(c.map(|c| u16::from(c.code))),
                Some(Ok(_)) => continue,
                Some(Err(_)) | None => return Err(None),
            }
        }
    }

    /// Send records and wait for their ack.
    async fn upsert(&mut self, items: Vec<Stamped>) -> u64 {
        self.send(&DaemonFrame::Entities { items }).await;
        match self.recv().await {
            Some(RelayFrame::Ack { seq }) => seq,
            other => panic!("expected ack, got {other:?}"),
        }
    }
}

/// An SSE stream read from the router.
struct Events {
    body: axum::body::BodyDataStream,
    buf: String,
}

#[derive(Debug)]
struct Sse {
    id: Option<u64>,
    event: String,
    data: StreamEvent,
}

impl Events {
    async fn open(a: &App, path: &str, who: &str, last: Option<u64>) -> Events {
        let mut b = Request::get(path).header("authorization", format!("Bearer {who}"));
        if let Some(l) = last {
            b = b.header("last-event-id", l.to_string());
        }
        let resp = a.send(b.body(Body::empty()).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        Events { body: resp.into_body().into_data_stream(), buf: String::new() }
    }

    async fn next(&mut self) -> Sse {
        loop {
            if let Some(end) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..end + 2).collect();
                let (mut id, mut event, mut data) = (None, String::new(), None);
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("id:") {
                        id = Some(v.trim().parse().unwrap());
                    } else if let Some(v) = line.strip_prefix("event:") {
                        event = v.trim().to_string();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data = Some(serde_json::from_str(v.trim()).unwrap());
                    }
                }
                // Keep-alive comments have no data.
                if let Some(data) = data {
                    return Sse { id, event, data };
                }
                continue;
            }
            let chunk = tokio::time::timeout(Duration::from_secs(10), self.body.next()).await.expect("an event within 10 s").expect("stream open").unwrap();
            self.buf.push_str(&String::from_utf8_lossy(&chunk));
        }
    }

    /// The next entity event.
    async fn entity(&mut self) -> (u64, Entity) {
        loop {
            let e = self.next().await;
            if let StreamEvent::Entity(m) = e.data {
                assert_eq!((e.event.as_str(), e.id), ("entity", Some(m.cursor)));
                return (m.cursor, m.entity);
            }
        }
    }

    /// No event within `ms` (keep-alives aside).
    async fn quiet(&mut self, ms: u64) {
        if let Ok(e) = tokio::time::timeout(Duration::from_millis(ms), self.next()).await {
            panic!("expected nothing, got {e:?}");
        }
    }
}

async fn wait_online(w: &World, machine: &str, online: bool) {
    for _ in 0..200 {
        let (_, v) = w.req("GET", "/v1/code/agents/acme/machines", None, &w.owner).await;
        if v.as_array().unwrap().iter().any(|m| m["id"] == machine && m["online"] == online) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{machine} never became online={online}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn device_login_hands_out_a_machine_token_once() {
    let w = world().await;
    let (s, login) = w.a.req("POST", "/v1/code/agents/login", Some(json!({ "name": "laptop" })), None).await;
    assert_eq!(s, StatusCode::OK, "{login}");
    let user_code = login["user_code"].as_str().unwrap().to_string();
    let device = json!({ "device_code": login["device_code"] });
    assert_eq!(login["verification_uri"], format!("https://roll.sh/dashboard/agents/connect?code={user_code}"));
    assert_eq!((login["interval"].as_u64(), login["expires_in"].as_u64()), (Some(5), Some(900)));

    let (_, p) = w.a.req("POST", "/v1/code/agents/login/poll", Some(device.clone()), None).await;
    assert_eq!(p["status"], "pending");
    // Only the owner approves: not its agents, not another owner, not anonymous.
    let body = json!({ "user_code": user_code.to_lowercase().replace('-', "") });
    for (who, want) in [(Some(w.agent.as_str()), StatusCode::FORBIDDEN), (Some(w.other.as_str()), StatusCode::FORBIDDEN), (None, StatusCode::UNAUTHORIZED)] {
        assert_eq!(w.a.req("POST", "/v1/code/agents/acme/login/approve", Some(body.clone()), who).await.0, want);
    }
    let (s, _) = w.req("POST", "/v1/code/agents/acme/login/approve", Some(json!({ "user_code": user_code, "name": "work laptop" })), &w.owner).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // Approving twice: the login isn't pending any more.
    assert_eq!(w.req("POST", "/v1/code/agents/acme/login/approve", Some(json!({ "user_code": user_code })), &w.owner).await.0, StatusCode::NOT_FOUND);

    let (_, p) = w.a.req("POST", "/v1/code/agents/login/poll", Some(device.clone()), None).await;
    assert_eq!((p["status"].as_str(), p["owner"].as_str()), (Some("approved"), Some("acme")), "{p}");
    let minted = p["token"].as_str().unwrap().to_string();
    assert!(minted.starts_with("rlm_"));
    let machine = p["machine"].as_str().unwrap().to_string();
    // Once.
    assert_eq!(w.a.req("POST", "/v1/code/agents/login/poll", Some(device), None).await.1["status"], "expired");

    let (_, list) = w.req("GET", "/v1/code/agents/acme/machines", None, &w.owner).await;
    assert_eq!(list[0]["id"], machine.as_str());
    assert_eq!((list[0]["name"].as_str(), list[0]["online"].as_bool()), (Some("work laptop"), Some(false)));
    // The token opens the link and nothing else.
    let (d, resume_from) = Daemon::connect(&w.url, &minted).await;
    assert_eq!(resume_from, 0);
    drop(d);
    assert_eq!(w.req("GET", "/v1/code/agents/acme/machines", None, &minted).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(w.req("GET", "/v1/code/repos", None, &minted).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn denied_and_unknown_logins() {
    let w = world().await;
    let (_, login) = w.a.req("POST", "/v1/code/agents/login", None, None).await;
    let device = json!({ "device_code": login["device_code"] });
    let (s, _) = w.req("POST", "/v1/code/agents/acme/login/deny", Some(json!({ "user_code": login["user_code"] })), &w.owner).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(w.a.req("POST", "/v1/code/agents/login/poll", Some(device.clone()), None).await.1["status"], "denied");
    assert_eq!(w.a.req("POST", "/v1/code/agents/login/poll", Some(device), None).await.1["status"], "expired");
    assert_eq!(w.a.req("POST", "/v1/code/agents/login/poll", Some(json!({ "device_code": "rld_nope" })), None).await.1["status"], "expired");
    assert_eq!(w.a.req("POST", "/v1/code/agents/login/poll", Some(json!({})), None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(w.req("POST", "/v1/code/agents/acme/login/approve", Some(json!({ "user_code": "BCDF-GHJK" })), &w.owner).await.0, StatusCode::NOT_FOUND);
    assert_eq!(w.req("POST", "/v1/code/agents/acme/login/approve", Some(json!({})), &w.owner).await.0, StatusCode::BAD_REQUEST);
    let (_, list) = w.req("GET", "/v1/code/agents/acme/machines", None, &w.owner).await;
    assert_eq!(list, json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn machines_are_managed_by_the_owner_and_revoking_closes_the_link() {
    let w = world().await;
    let (id, minted) = w.machine("box").await;
    // Agents read and drive, but don't manage.
    assert_eq!(w.req("GET", "/v1/code/agents/acme/machines", None, &w.agent).await.0, StatusCode::OK);
    for (m, p) in [("POST", "/v1/code/agents/acme/machines".to_string()), ("PATCH", format!("/v1/code/agents/acme/machines/{id}")), ("DELETE", format!("/v1/code/agents/acme/machines/{id}"))] {
        assert_eq!(w.req(m, &p, Some(json!({ "name": "x" })), &w.agent).await.0, StatusCode::FORBIDDEN, "{m} {p}");
        assert_eq!(w.req(m, &p, Some(json!({ "name": "x" })), &w.other).await.0, StatusCode::FORBIDDEN, "{m} {p}");
    }
    assert_eq!(w.req("GET", "/v1/code/agents/acme/machines", None, &w.other).await.0, StatusCode::NOT_FOUND);
    let (s, m) = w.req("PATCH", &format!("/v1/code/agents/acme/machines/{id}"), Some(json!({ "name": "  build box \n" })), &w.owner).await;
    assert_eq!((s, m["name"].as_str()), (StatusCode::OK, Some("build box")));
    assert_eq!(w.req("PATCH", &format!("/v1/code/agents/acme/machines/{id}"), Some(json!({ "name": "" })), &w.owner).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(w.req("PATCH", "/v1/code/agents/acme/machines/mch_nope", Some(json!({ "name": "x" })), &w.owner).await.0, StatusCode::NOT_FOUND);

    let (mut d, _) = Daemon::connect(&w.url, &minted).await;
    wait_online(&w, &id, true).await;
    let mut ev = Events::open(&w.a, "/v1/code/agents/acme/events", &w.owner, None).await;
    assert_eq!(w.req("DELETE", &format!("/v1/code/agents/acme/machines/{id}"), None, &w.owner).await.0, StatusCode::NO_CONTENT);
    // The link closes at once, saying the token is revoked.
    assert_eq!(d.recv_or_close().await.unwrap_err(), Some(4401));
    let e = ev.next().await;
    assert!(matches!(&e.data, StreamEvent::MachineRemoved { machine, .. } if machine.as_str() == id), "{e:?}");
    assert_eq!(e.event, "machine_removed");
    // The token is dead, and the machine gone.
    let err = dial(&w.url, &minted).await.err().expect("refused");
    assert!(err.to_string().contains("401"), "{err}");
    assert_eq!(w.req("GET", "/v1/code/agents/acme/machines", None, &w.owner).await.1, json!([]));
    assert_eq!(w.req("DELETE", &format!("/v1/code/agents/acme/machines/{id}"), None, &w.owner).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_link_refuses_other_tokens_and_protocols() {
    let w = world().await;
    let (_, minted) = w.machine("box").await;
    for bad in [w.owner.as_str(), "rlm_nope", ADMIN] {
        let err = dial(&w.url, bad).await.err().expect("refused");
        assert!(err.to_string().contains("401"), "{bad}: {err}");
    }
    // A plain GET (no upgrade) with a good token is a JSON 400.
    let (s, v) = w.a.req("GET", "/v1/code/agents/connect", None, Some(&minted)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"].is_string());
    // Another protocol version.
    let mut d = dial(&w.url, &minted).await.unwrap();
    d.send(&DaemonFrame::Hello { protocol: agent_proto::PROTOCOL + 1, info: info() }).await;
    assert_eq!(d.recv_or_close().await.unwrap_err(), Some(4400));
    // Something other than hello first.
    let mut d = dial(&w.url, &minted).await.unwrap();
    d.send(&DaemonFrame::Pong).await;
    assert_eq!(d.recv_or_close().await.unwrap_err(), Some(4400));
    // A newer link replaces the older one.
    let (mut first, _) = Daemon::connect(&w.url, &minted).await;
    let (_second, _) = Daemon::connect(&w.url, &minted).await;
    assert_eq!(first.recv_or_close().await.unwrap_err(), Some(4409));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn records_are_mirrored_into_state_and_events() {
    let w = world().await;
    let (id, minted) = w.machine("box").await;
    let mut ev = Events::open(&w.a, "/v1/code/agents/acme/events?threads=thr_a", &w.agent, None).await;
    let (mut d, from) = Daemon::connect(&w.url, &minted).await;
    assert_eq!(from, 0);
    // The machine came online.
    let e = ev.next().await;
    let StreamEvent::Machine { machine, cursor } = &e.data else { panic!("{e:?}") };
    assert_eq!((machine.id.as_str(), machine.online, e.id), (id.as_str(), true, Some(*cursor)));
    assert_eq!(machine.info.as_ref().unwrap().hostname, "box");

    let acked = d
        .upsert(vec![stamped(1, project("prj_1", "x")), stamped(2, thread("thr_a", "a")), stamped(3, thread("thr_b", "b")), stamped(4, node("nod_1", "thr_a", "hi")), stamped(5, node("nod_2", "thr_b", "yo"))])
        .await;
    assert_eq!(acked, 5);
    // Shell records, and detail only for the thread asked for.
    let mut got = Vec::new();
    for _ in 0..4 {
        got.push(ev.entity().await.1.id().to_string());
    }
    assert_eq!(got, ["prj_1", "thr_a", "thr_b", "nod_1"]);
    ev.quiet(200).await;

    // An older seq of a record changes nothing; a newer one replaces it.
    assert_eq!(d.upsert(vec![stamped(2, thread("thr_a", "stale"))]).await, 2);
    assert_eq!(d.upsert(vec![stamped(6, thread("thr_a", "renamed"))]).await, 6);
    let (_, e) = ev.entity().await;
    assert!(matches!(&e, Entity::Thread(t) if t.title == "renamed"));

    // Deltas: for the threads asked for, without an id.
    let delta = agent_proto::Delta::Text { thread: agent_proto::ThreadId("thr_a".into()), node: agent_proto::NodeId("nod_1".into()), text: "…".into() };
    d.send(&DaemonFrame::Delta { delta: agent_proto::Delta::Text { thread: agent_proto::ThreadId("thr_b".into()), node: agent_proto::NodeId("nod_2".into()), text: "x".into() } }).await;
    d.send(&DaemonFrame::Delta { delta: delta.clone() }).await;
    let e = ev.next().await;
    assert_eq!((e.event.as_str(), e.id), ("delta", None));
    assert!(matches!(&e.data, StreamEvent::Delta { delta: got, .. } if *got == delta));

    let (s, state) = w.req("GET", "/v1/code/agents/acme/state?threads=thr_b", None, &w.owner).await;
    assert_eq!(s, StatusCode::OK);
    let state: agent_proto::State = serde_json::from_value(state).unwrap();
    let ids: Vec<&str> = state.entities.iter().map(|m| m.entity.id()).collect();
    assert_eq!(ids, ["prj_1", "thr_b", "nod_2", "thr_a"]);
    assert!(state.entities.iter().all(|m| m.machine.as_str() == id));
    assert_eq!(state.machines.len(), 1);
    assert!(state.machines[0].online);
    assert!(state.cursor >= state.entities.iter().map(|m| m.cursor).max().unwrap());

    // A removal is a tombstone on the stream and gone from the snapshot.
    let gone = entity(json!({ "kind": "removed", "entity": "thread", "id": "thr_b" }));
    d.upsert(vec![stamped(7, gone)]).await;
    assert!(matches!(ev.entity().await.1, Entity::Removed(_)));
    let (_, state) = w.req("GET", "/v1/code/agents/acme/state", None, &w.owner).await;
    let ids: Vec<&str> = state["entities"].as_array().unwrap().iter().map(|m| m["entity"]["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["prj_1", "thr_a"]);

    // Other owners and anonymous callers see nothing.
    assert_eq!(w.req("GET", "/v1/code/agents/acme/state", None, &w.other).await.0, StatusCode::NOT_FOUND);
    assert_eq!(w.a.req("GET", "/v1/code/agents/acme/events", None, None).await.0, StatusCode::NOT_FOUND);

    // Going away: offline, and the records stay.
    drop(d);
    wait_online(&w, &id, false).await;
    let (_, state) = w.req("GET", "/v1/code/agents/acme/state", None, &w.owner).await;
    assert_eq!(state["entities"].as_array().unwrap().len(), 2);
}

/// A stream that reconnects with `Last-Event-ID` gets exactly what it missed, in order, then
/// goes on live; a reconnecting daemon resumes after the newest seq the relay holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streams_and_daemons_resume_without_loss_or_duplicates() {
    let w = world().await;
    let (_, minted) = w.machine("box").await;
    let (mut d, _) = Daemon::connect(&w.url, &minted).await;
    d.upsert(vec![stamped(1, project("prj_1", "x")), stamped(2, thread("thr_a", "a"))]).await;
    let (_, state) = w.req("GET", "/v1/code/agents/acme/state", None, &w.owner).await;
    let snapshot = state["cursor"].as_u64().unwrap();

    // Missed while away: three upserts (one record twice) and a machine rename.
    d.upsert(vec![stamped(3, thread("thr_b", "b")), stamped(4, thread("thr_a", "a2"))]).await;
    let (id, _) = (state["machines"][0]["id"].as_str().unwrap().to_string(), ());
    w.req("PATCH", &format!("/v1/code/agents/acme/machines/{id}"), Some(json!({ "name": "renamed" })), &w.owner).await;
    d.upsert(vec![stamped(5, thread("thr_a", "a3"))]).await;

    let mut ev = Events::open(&w.a, "/v1/code/agents/acme/events", &w.owner, Some(snapshot)).await;
    let mut seen = Vec::new();
    let mut last = snapshot;
    for _ in 0..3 {
        let e = ev.next().await;
        let id = e.id.unwrap();
        assert!(id > last, "cursors ascend");
        last = id;
        seen.push(match e.data {
            StreamEvent::Entity(m) => match m.entity {
                Entity::Thread(t) => t.title,
                other => panic!("{other:?}"),
            },
            StreamEvent::Machine { machine, .. } => machine.name,
            other => panic!("{other:?}"),
        });
    }
    // thr_a appears once, at its newest.
    assert_eq!(seen, ["b", "renamed", "a3"]);
    ev.quiet(200).await;
    // Then live.
    d.upsert(vec![stamped(6, project("prj_2", "y"))]).await;
    assert_eq!(ev.entity().await.1.id(), "prj_2");

    // `?after=` works like the header.
    let mut ev2 = Events::open(&w.a, &format!("/v1/code/agents/acme/events?after={last}"), &w.owner, None).await;
    assert_eq!(ev2.entity().await.1.id(), "prj_2");

    // The daemon reconnects: resume after the newest seq held.
    drop(d);
    let (_d, from) = Daemon::connect(&w.url, &minted).await;
    assert_eq!(from, 6);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commands_go_to_the_daemon_and_come_back() {
    let w = world().await;
    let (id, minted) = w.machine("box").await;
    let path = format!("/v1/code/agents/acme/machines/{id}/commands");
    let cmd = |cid: &str| json!({ "id": cid, "command": { "type": "run.interrupt", "thread": "thr_a" } });

    // Offline.
    let (s, r) = w.req("POST", &path, Some(cmd("c1")), &w.owner).await;
    assert_eq!((s, r["type"].as_str(), r["code"].as_str()), (StatusCode::SERVICE_UNAVAILABLE, Some("error"), Some("machine_offline")), "{r}");
    assert!(r["error"].is_string());

    let (mut d, _) = Daemon::connect(&w.url, &minted).await;
    // The daemon answers whatever it's sent, in reverse order of arrival.
    let daemon = tokio::spawn(async move {
        let mut got = Vec::new();
        while got.len() < 2 {
            match d.recv().await {
                Some(RelayFrame::Command { id, by, command }) => got.push((id, by, command)),
                other => panic!("{other:?}"),
            }
        }
        for (id, by, command) in got.into_iter().rev() {
            let result = match id.as_str() {
                "c2" => Reply::Created { id: format!("{by}:{}", serde_json::to_value(&command).unwrap()["type"].as_str().unwrap()) },
                _ => Reply::Error(agent_proto::ReplyError { code: agent_proto::ErrorCode::Conflict, message: "busy".into() }),
            };
            d.send(&DaemonFrame::Reply { id, result }).await;
        }
        d
    });
    let (a, b) = tokio::join!(w.req("POST", &path, Some(cmd("c2")), &w.agent), w.req("POST", &path, Some(cmd("c3")), &w.owner));
    assert_eq!(a, (StatusCode::OK, json!({ "type": "created", "id": "agent:bot:run.interrupt" })));
    assert_eq!((b.0, b.1["code"].as_str()), (StatusCode::CONFLICT, Some("conflict")));
    let mut d = daemon.await.unwrap();

    // No answer: a timeout. The same id again while it waits shares one send.
    w.a.code.agents.set_command_timeout(Duration::from_millis(300));
    let (t1, t2) = tokio::join!(w.req("POST", &path, Some(cmd("c4")), &w.owner), w.req("POST", &path, Some(cmd("c4")), &w.owner));
    for t in [t1, t2] {
        assert_eq!((t.0, t.1["code"].as_str()), (StatusCode::GATEWAY_TIMEOUT, Some("timeout")));
    }
    assert!(matches!(d.recv().await, Some(RelayFrame::Command { id, .. }) if id == "c4"));
    tokio::time::timeout(Duration::from_millis(200), d.recv()).await.expect_err("sent once");

    // A link that drops while a command waits gets it again on reconnect.
    w.a.code.agents.set_command_timeout(Duration::from_secs(10));
    let w2 = std::sync::Arc::new(w);
    let pending = {
        let (w2, path) = (w2.clone(), path.clone());
        tokio::spawn(async move { w2.req("POST", &path, Some(cmd("c5")), &w2.owner).await })
    };
    assert!(matches!(d.recv().await, Some(RelayFrame::Command { id, .. }) if id == "c5"));
    drop(d);
    let (mut d, _) = Daemon::connect(&w2.url, &minted).await;
    assert!(matches!(d.recv().await, Some(RelayFrame::Command { id, .. }) if id == "c5"));
    d.send(&DaemonFrame::Reply { id: "c5".into(), result: Reply::Ok }).await;
    assert_eq!(pending.await.unwrap(), (StatusCode::OK, json!({ "type": "ok" })));

    // Who may: the owner's tokens; bad bodies and unknown machines.
    let w = &*w2;
    assert_eq!(w.req("POST", &path, Some(cmd("c6")), &w.other).await.0, StatusCode::FORBIDDEN);
    assert_eq!(w.a.req("POST", &path, Some(cmd("c6")), None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(w.req("POST", &path, Some(json!({ "id": "c7", "command": { "type": "nope" } })), &w.owner).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(w.req("POST", &path, Some(json!({ "id": "", "command": { "type": "provider.refresh" } })), &w.owner).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(w.req("POST", "/v1/code/agents/acme/machines/mch_nope/commands", Some(cmd("c8")), &w.owner).await.0, StatusCode::NOT_FOUND);
}
