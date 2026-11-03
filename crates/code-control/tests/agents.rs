//! The agents relay (`src/agents/`, docs/code/agents.md): device login, machines, the daemon's
//! WebSocket (a test daemon speaking the real protocol over a real socket), the mirror behind
//! `/state` and `/events`, and commands.

mod common;

use agent_proto::{DaemonFrame, Entity, RelayFrame, Reply, Stamped, StreamEvent};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::daemon::{Daemon, dial, info};
use common::{App, app};
use futures::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;

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

/// `POST /v1/code/agents/login` from a client address (as `rolld` serves it, with ConnectInfo).
async fn login_from(w: &World, addr: &str) -> (StatusCode, Value) {
    let mut req = Request::post("/v1/code/agents/login").header("content-type", "application/json").body(Body::from(json!({ "name": "m" }).to_string())).unwrap();
    req.extensions_mut().insert(axum::extract::ConnectInfo(addr.parse::<std::net::SocketAddr>().unwrap()));
    let r = w.a.send(req).await;
    let status = r.status();
    let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// Anyone may start a login, so pending logins are capped per client address (an IPv6 /64 is
/// one client) and a full table makes room from the noisiest client instead of refusing
/// everyone: one client (or many) can't lock the others out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logins_are_capped_per_client_and_a_full_table_never_locks_others_out() {
    use code_control::agents::store::{self, LoginCaps, LoginMachine, MAX_PENDING_PER_CLIENT, NewLogin};
    let w = world().await;
    for i in 0..MAX_PENDING_PER_CLIENT {
        let (s, v) = login_from(&w, &format!("10.9.0.1:{}", 1000 + i)).await;
        assert_eq!(s, StatusCode::OK, "{i}: {v}");
    }
    let (s, v) = login_from(&w, "10.9.0.1:9").await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{v}");
    assert!(v["error"].as_str().unwrap().contains("from this address"), "{v}");
    assert_eq!(login_from(&w, "10.9.0.2:9").await.0, StatusCode::OK, "another address");
    for i in 0..MAX_PENDING_PER_CLIENT {
        assert_eq!(login_from(&w, &format!("[2001:db8::{:x}]:1", i + 1)).await.0, StatusCode::OK);
    }
    assert_eq!(login_from(&w, "[2001:db8::ffff]:1").await.0, StatusCode::TOO_MANY_REQUESTS, "the same /64");
    assert_eq!(login_from(&w, "[2001:db8:0:1::1]:1").await.0, StatusCode::OK, "another /64");

    // A full table: the oldest login of the client with the most waiting goes.
    let db = w.a.code.db.sql();
    db.execute("DELETE FROM agent_logins", ()).unwrap();
    let caps = LoginCaps { per_client: 5, total: 6 };
    let m = LoginMachine { name: "m", hostname: None, os: None };
    let add = |n: &str, client: &str| store::create_login(db, &format!("hash-{n}"), &format!("CODE-{n}"), &m, client, caps).unwrap();
    for n in ["A1", "A2", "A3", "A4"] {
        assert!(matches!(add(n, "ip:a"), NewLogin::Created));
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(matches!(add("B1", "ip:b"), NewLogin::Created));
    assert!(matches!(add("A5", "ip:a"), NewLogin::Created));
    assert!(matches!(add("A6", "ip:a"), NewLogin::Busy), "a's own cap");
    assert!(matches!(add("C1", "ip:c"), NewLogin::Created), "full, and still not refused");
    let left = |code: &str| store::pending_login(db, code).unwrap().is_some();
    assert!(!left("CODE-A1") && left("CODE-A2") && left("CODE-B1") && left("CODE-C1"));
    assert!(matches!(add("D1", "ip:d"), NewLogin::Created));
    assert!(!left("CODE-A2") && left("CODE-B1") && left("CODE-C1") && left("CODE-D1"));
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
async fn a_machine_can_revoke_itself() {
    let w = world().await;
    let (id, minted) = w.machine("box").await;
    let (_other, other_token) = w.machine("other").await;
    let (mut d, _) = Daemon::connect(&w.url, &minted).await;
    // Only machine tokens, and only their own machine.
    for bad in [w.owner.as_str(), "rlm_nope"] {
        assert_eq!(w.a.req("DELETE", "/v1/code/agents/connect", None, Some(bad)).await.0, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(w.a.req("DELETE", "/v1/code/agents/connect", None, Some(&minted)).await.0, StatusCode::NO_CONTENT);
    assert_eq!(d.recv_or_close().await.unwrap_err(), Some(4401));
    assert_eq!(w.a.req("DELETE", "/v1/code/agents/connect", None, Some(&minted)).await.0, StatusCode::UNAUTHORIZED);
    let (_, list) = w.req("GET", "/v1/code/agents/acme/machines", None, &w.owner).await;
    let ids: Vec<&str> = list.as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert!(!ids.contains(&id.as_str()) && ids.len() == 1, "{list}");
    Daemon::connect(&w.url, &other_token).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_link_refuses_other_tokens_and_protocols() {
    let w = world().await;
    let (_, minted) = w.machine("box").await;
    for bad in [w.owner.as_str(), "rlm_nope", ADMIN] {
        let err = dial(&w.url, bad).await.err().expect("refused");
        assert!(err.to_string().contains("401"), "{bad}: {err}");
    }
    // A plain GET (no upgrade) says which machine the token is (`roll agent login --machine-token`).
    let (s, v) = w.a.req("GET", "/v1/code/agents/connect", None, Some(&minted)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let who: agent_proto::MachineLogin = serde_json::from_value(v).unwrap();
    assert_eq!((who.owner.as_str(), who.machine.name.as_str(), who.machine.online), ("acme", "box", false));
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
                Some(RelayFrame::Command { id, by, command, .. }) => got.push((id, by, command)),
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

    // Ids are per sender: another sender's command with an id that is waiting is a conflict (a
    // reply names only its id), not a share in the first one's reply.
    let pending = {
        let (w2, path) = (w2.clone(), path.clone());
        tokio::spawn(async move { w2.req("POST", &path, Some(cmd("c9")), &w2.owner).await })
    };
    assert!(matches!(d.recv().await, Some(RelayFrame::Command { id, .. }) if id == "c9"));
    let (s, r) = w2.req("POST", &path, Some(cmd("c9")), &w2.agent).await;
    assert_eq!((s, r["code"].as_str()), (StatusCode::CONFLICT, Some("conflict")), "{r}");
    d.send(&DaemonFrame::Reply { id: "c9".into(), result: Reply::Ok }).await;
    assert_eq!(pending.await.unwrap(), (StatusCode::OK, json!({ "type": "ok" })));
    tokio::time::timeout(Duration::from_millis(200), d.recv()).await.expect_err("the agent's c9 never went out");

    // Who may: the owner's tokens; bad bodies and unknown machines.
    let w = &*w2;
    assert_eq!(w.req("POST", &path, Some(cmd("c6")), &w.other).await.0, StatusCode::FORBIDDEN);
    assert_eq!(w.a.req("POST", &path, Some(cmd("c6")), None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(w.req("POST", &path, Some(json!({ "id": "c7", "command": { "type": "nope" } })), &w.owner).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(w.req("POST", &path, Some(json!({ "id": "", "command": { "type": "provider.refresh" } })), &w.owner).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(w.req("POST", "/v1/code/agents/acme/machines/mch_nope/commands", Some(cmd("c8")), &w.owner).await.0, StatusCode::NOT_FOUND);
}


#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_logins_say_which_machine_asks() {
    let w = world().await;
    let (s, login) = w.a.req("POST", "/v1/code/agents/login", Some(json!({ "name": "ada's laptop", "hostname": "ada-mbp.local\n", "os": "macos" })), None).await;
    assert_eq!(s, StatusCode::OK, "{login}");
    let code = login["user_code"].as_str().unwrap().to_string();
    // Typed any way: lowercase, without the dash.
    let path = format!("/v1/code/agents/acme/login/{}", code.to_lowercase().replace('-', ""));
    let (s, p) = w.req("GET", &path, None, &w.owner).await;
    assert_eq!(s, StatusCode::OK, "{p}");
    let p: agent_proto::PendingLogin = serde_json::from_value(p).unwrap();
    assert_eq!((p.user_code.as_str(), p.name.as_str(), p.hostname.as_deref(), p.os.as_deref()), (code.as_str(), "ada's laptop", Some("ada-mbp.local"), Some("macos")));
    assert_eq!(p.expires_at - p.created_at, 900_000);
    // Only those who could approve it.
    for (who, want) in [(Some(w.agent.as_str()), StatusCode::FORBIDDEN), (Some(w.other.as_str()), StatusCode::FORBIDDEN), (None, StatusCode::UNAUTHORIZED)] {
        assert_eq!(w.a.req("GET", &path, None, who).await.0, want);
    }
    assert_eq!(w.req("GET", "/v1/code/agents/acme/login/BCDF-GHJK", None, &w.owner).await.0, StatusCode::NOT_FOUND);
    // Without hostname and os (an older CLI); a non-string is refused.
    let (_, bare) = w.a.req("POST", "/v1/code/agents/login", None, None).await;
    let (_, p) = w.req("GET", &format!("/v1/code/agents/acme/login/{}", bare["user_code"].as_str().unwrap()), None, &w.owner).await;
    assert_eq!((p["name"].as_str(), p.get("hostname"), p.get("os")), (Some("machine"), None, None), "{p}");
    assert_eq!(w.a.req("POST", "/v1/code/agents/login", Some(json!({ "hostname": 7 })), None).await.0, StatusCode::BAD_REQUEST);
    // Once decided, it isn't pending.
    assert_eq!(w.req("POST", "/v1/code/agents/acme/login/approve", Some(json!({ "user_code": code })), &w.owner).await.0, StatusCode::NO_CONTENT);
    assert_eq!(w.req("GET", &path, None, &w.owner).await.0, StatusCode::NOT_FOUND);
}

/// The canvas reconnects with both `?after=` (the URL it first opened) and `Last-Event-ID`
/// (the newest cursor it applied): the stream resumes after the newer, whichever carries it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_resume_after_the_newer_of_after_and_last_event_id() {
    let w = world().await;
    let (_, minted) = w.machine("box").await;
    let (mut d, _) = Daemon::connect(&w.url, &minted).await;
    d.upsert(vec![stamped(1, project("prj_1", "x"))]).await;
    let (_, state) = w.req("GET", "/v1/code/agents/acme/state", None, &w.owner).await;
    let first = state["cursor"].as_u64().unwrap();
    d.upsert(vec![stamped(2, thread("thr_a", "a"))]).await;
    let (_, state) = w.req("GET", "/v1/code/agents/acme/state", None, &w.owner).await;
    let second = state["cursor"].as_u64().unwrap();
    d.upsert(vec![stamped(3, thread("thr_b", "b"))]).await;
    assert!(first < second);

    let ids = |v: &[Entity]| v.iter().map(|e| e.id().to_string()).collect::<Vec<_>>();
    for (query, header, want) in [
        (Some(first), Some(second), vec!["thr_b"]),
        (Some(second), Some(first), vec!["thr_b"]),
        (Some(first), None, vec!["thr_a", "thr_b"]),
        (None, Some(first), vec!["thr_a", "thr_b"]),
        (Some(second), Some(second), vec!["thr_b"]),
    ] {
        let path = match query {
            Some(q) => format!("/v1/code/agents/acme/events?after={q}"),
            None => "/v1/code/agents/acme/events".into(),
        };
        let mut ev = Events::open(&w.a, &path, &w.owner, header).await;
        let mut got = Vec::new();
        for _ in 0..want.len() {
            got.push(ev.entity().await.1);
        }
        assert_eq!(ids(&got), want, "after={query:?} Last-Event-ID={header:?}");
        ev.quiet(150).await;
    }
    // A Last-Event-ID that isn't a cursor is ignored (the query still counts).
    let resp = w
        .a
        .send(Request::get(format!("/v1/code/agents/acme/events?after={second}")).header("authorization", format!("Bearer {}", w.owner)).header("last-event-id", "").body(Body::empty()).unwrap())
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let mut ev = Events { body: resp.into_body().into_data_stream(), buf: String::new() };
    assert_eq!(ev.entity().await.1.id(), "thr_b");
    // A query that isn't a cursor is a 400.
    assert_eq!(w.req("GET", "/v1/code/agents/acme/events?after=x", None, &w.owner).await.0, StatusCode::BAD_REQUEST);
}

fn subthread(id: &str, parent: &str, relation: &str, at: u64) -> Entity {
    let mut v = serde_json::to_value(thread(id, id)).unwrap();
    v["lineage"] = json!({ "parent": parent, "relation": relation, "root": "thr_a" });
    v["created_at"] = json!(at);
    entity(v)
}

/// What a machine mirrored for `thr_a`: a project and thread under the home directory, a subagent
/// tree (`thr_sub` → `thr_grand`), a fork, a run with an image, nodes, a request, a checkpoint
/// and a terminal.
fn share_world() -> Vec<Stamped> {
    let mut proj = serde_json::to_value(project("prj_1", "x")).unwrap();
    proj["path"] = json!("/home/me/src/x");
    proj["repo"] = json!({ "owner": "acme", "repo": "secret-repo" });
    proj["git"] = json!({ "branch": "main", "head": "abc", "dirty": false, "remote": "https://ada:ghp_0123456789abcdefABCD@github.com/acme/secret-repo.git" });
    let mut root = serde_json::to_value(thread("thr_a", "Fix /home/me/src/x")).unwrap();
    root["cwd"] = json!("/home/me/src/x");
    root["queued"] = json!([{ "id": "q1", "text": "later", "at": 5 }]);
    let run = entity(json!({
        "kind": "run", "id": "run_1", "thread": "thr_a", "ordinal": 1, "provider": "codex", "model": null, "status": "completed",
        "prompt": "look at this", "attachments": [{ "name": "shot.png", "mime": "image/png", "data": "aGVsbG8=" }],
        "requested_at": 10, "started_at": 11, "completed_at": 30
    }));
    let tool = entity(json!({
        "kind": "node", "id": "nod_t", "thread": "thr_a", "run": "run_1", "ordinal": 2, "status": "completed", "started_at": 12, "completed_at": 14,
        "body": { "type": "tool", "name": "Bash", "kind": "command", "title": "ls /home/me", "input": { "command": "ls /home/me/src" }, "output": "/home/me/src/x\n", "exit_code": 0 }
    }));
    let leaky = entity(json!({
        "kind": "node", "id": "nod_u", "thread": "thr_a", "run": "run_1", "ordinal": 3, "status": "completed", "started_at": 13, "completed_at": 14,
        "body": { "type": "tool", "name": "Bash", "kind": "command", "title": "build",
            "input": { "command": "cc -I/home/me/inc x.c && curl -H 'Authorization: Bearer rlm_0123456789abcdef0123' https://ada:hunter22@roll.sh" },
            "output": "see file:///home/me/src/x/a.c\nGITHUB_TOKEN=ghp_0123456789abcdefABCD\nAWS AKIAIOSFODNN7EXAMPLE", "exit_code": 0 }
    }));
    let request = entity(json!({
        "kind": "request", "id": "req_1", "thread": "thr_a", "run": "run_1", "node": "nod_r", "status": "resolved", "created_at": 15, "resolved_at": 16,
        "body": { "type": "approval", "action": "command", "title": "Run `cargo test`", "decisions": ["accept", "decline"] },
        "response": { "type": "decision", "decision": "accept" }
    }));
    let checkpoint = entity(json!({ "kind": "checkpoint", "id": "chk_1", "thread": "thr_a", "run": "run_1", "ordinal": 1, "commit": "abc", "stat": { "files": 1, "additions": 2, "deletions": 0 }, "created_at": 30 }));
    let terminal = entity(json!({ "kind": "terminal", "id": "trm_1", "thread": "thr_a", "cwd": "/home/me", "title": "zsh", "status": "running", "cols": 80, "rows": 24, "created_at": 1 }));
    let mut sub_node = serde_json::to_value(node("nod_s", "thr_sub", "child work")).unwrap();
    sub_node["run"] = json!("run_s");
    let items = vec![
        entity(proj),
        entity(root),
        subthread("thr_sub", "thr_a", "subagent", 20),
        subthread("thr_grand", "thr_sub", "subagent", 21),
        subthread("thr_fork", "thr_a", "fork", 22),
        run,
        node("nod_m", "thr_a", "edited /home/me/src/x/lib.rs"),
        tool,
        leaky,
        request,
        checkpoint,
        terminal,
        entity(sub_node),
        node("nod_f", "thr_fork", "fork work"),
    ];
    items.into_iter().enumerate().map(|(i, e)| stamped(i as u64 + 1, e)).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shares_freeze_a_redacted_copy_for_anyone_with_the_link() {
    let w = world().await;
    let (_, minted) = w.machine("box").await;
    let (mut d, _) = Daemon::connect(&w.url, &minted).await;
    d.upsert(share_world()).await;
    let create = "/v1/code/agents/acme/threads/thr_a/shares";

    // Sharing is the owner's: not its agents, not other owners, not anonymous.
    for (who, want) in [(Some(w.agent.as_str()), StatusCode::FORBIDDEN), (Some(w.other.as_str()), StatusCode::FORBIDDEN), (None, StatusCode::UNAUTHORIZED)] {
        assert_eq!(w.a.req("POST", create, None, who).await.0, want);
    }
    assert_eq!(w.req("POST", "/v1/code/agents/acme/threads/thr_nope/shares", None, &w.owner).await.0, StatusCode::NOT_FOUND);
    assert_eq!(w.req("POST", create, Some(json!({ "subtree": "yes" })), &w.owner).await.0, StatusCode::BAD_REQUEST);

    let (s, one) = w.req("POST", create, None, &w.owner).await;
    assert_eq!(s, StatusCode::CREATED, "{one}");
    let one: agent_proto::ShareSummary = serde_json::from_value(one).unwrap();
    assert!(one.id.starts_with("shr_") && one.id.len() == 28, "{}", one.id);
    assert_eq!(one.url, format!("https://roll.sh/agents/s/{}", one.id));
    assert_eq!((one.threads, one.runs, one.subtree, one.created_by.as_str(), one.title.as_str()), (1, 1, false, "acme", "Fix ~/src/x"));

    // Anyone with the link reads it, never cached.
    let resp = w.a.send(Request::get(format!("/v1/code/agents/shares/{}", one.id)).body(Body::empty()).unwrap()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let raw = String::from_utf8(axum::body::to_bytes(resp.into_body(), 1 << 24).await.unwrap().to_vec()).unwrap();
    assert!(!raw.contains("/home/me"), "a home directory is in the share: {raw}");
    for secret in ["ghp_0123", "rlm_0123", "hunter22", "ada:", "AKIAIOSF", "secret-repo"] {
        assert!(!raw.contains(secret), "{secret} is in the share: {raw}");
    }
    let share: agent_proto::Share = serde_json::from_str(&raw).unwrap();
    assert_eq!((share.owner.as_str(), share.machine.name.as_str(), share.machine.os.as_deref(), share.thread.as_str()), ("acme", "box", Some("linux"), "thr_a"));
    let ids: Vec<&str> = share.entities.iter().map(|e| e.id()).collect();
    assert_eq!(ids, ["prj_1", "thr_a", "run_1", "req_1", "chk_1", "nod_m", "nod_t", "nod_u"], "no terminal, no other thread");
    for e in &share.entities {
        match e {
            Entity::Project(p) => assert!(p.path == "~/src/x" && p.repo.is_none() && p.git.is_none(), "{p:?}"),
            Entity::Thread(t) => assert!(t.cwd == "~/src/x" && t.queued.is_empty(), "{t:?}"),
            Entity::Run(r) => assert_eq!((r.attachments[0].name.as_str(), r.attachments[0].data.as_str()), ("shot.png", "")),
            Entity::Node(n) => match &n.body {
                agent_proto::NodeBody::Message { text } => assert_eq!(text, "edited ~/src/x/lib.rs"),
                agent_proto::NodeBody::Tool(t) if t.title == "build" => assert_eq!(
                    (t.input["command"].as_str(), t.output.as_deref()),
                    (
                        Some("cc -I~/inc x.c && curl -H 'Authorization: Bearer [redacted]' https://[redacted]@roll.sh"),
                        Some("see file://~/src/x/a.c\nGITHUB_TOKEN=ghp_[redacted]\nAWS AKIA[redacted]")
                    )
                ),
                agent_proto::NodeBody::Tool(t) => assert_eq!((t.title.as_str(), t.input["command"].as_str(), t.output.as_deref()), ("ls ~", Some("ls ~/src"), Some("~/src/x\n"))),
                other => panic!("{other:?}"),
            },
            _ => {}
        }
    }

    // With the subagent subtree: children and grandchildren, not forks.
    let (s, all) = w.req("POST", create, Some(json!({ "subtree": true })), &w.owner).await;
    assert_eq!(s, StatusCode::CREATED, "{all}");
    assert_eq!((all["threads"].as_u64(), all["subtree"].as_bool()), (Some(3), Some(true)));
    let (_, tree) = w.a.req("GET", &format!("/v1/code/agents/shares/{}", all["id"].as_str().unwrap()), None, None).await;
    let tree: agent_proto::Share = serde_json::from_value(tree).unwrap();
    let threads: Vec<&str> = tree.entities.iter().filter(|e| matches!(e, Entity::Thread(_))).map(|e| e.id()).collect();
    assert_eq!(threads, ["thr_a", "thr_sub", "thr_grand"]);
    assert!(tree.entities.iter().any(|e| e.id() == "nod_s") && !tree.entities.iter().any(|e| e.id() == "nod_f"));

    // Frozen: later work on the thread doesn't reach it.
    d.upsert(vec![stamped(50, thread("thr_a", "renamed"))]).await;
    let (_, again) = w.a.req("GET", &format!("/v1/code/agents/shares/{}", one.id), None, None).await;
    assert_eq!(again["title"], "Fix ~/src/x");

    // Listed newest first, for the owner's tokens; 404 to everyone else.
    let (s, list) = w.req("GET", "/v1/code/agents/acme/shares", None, &w.agent).await;
    assert_eq!(s, StatusCode::OK);
    let list: Vec<agent_proto::ShareSummary> = serde_json::from_value(list).unwrap();
    assert_eq!(list.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), [all["id"].as_str().unwrap(), one.id.as_str()]);
    assert_eq!(w.req("GET", "/v1/code/agents/acme/shares", None, &w.other).await.0, StatusCode::NOT_FOUND);
    assert_eq!(w.req("GET", "/v1/code/agents/other/shares", None, &w.other).await.1, json!([]));

    // Revoking: the owner's; the link is gone at once.
    let del = format!("/v1/code/agents/acme/shares/{}", one.id);
    assert_eq!(w.req("DELETE", &del, None, &w.agent).await.0, StatusCode::FORBIDDEN);
    assert_eq!(w.req("DELETE", &format!("/v1/code/agents/other/shares/{}", one.id), None, &w.other).await.0, StatusCode::NOT_FOUND);
    assert_eq!(w.req("DELETE", &del, None, &w.owner).await.0, StatusCode::NO_CONTENT);
    assert_eq!(w.a.req("GET", &format!("/v1/code/agents/shares/{}", one.id), None, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(w.req("DELETE", &del, None, &w.owner).await.0, StatusCode::NOT_FOUND);
    assert_eq!(w.a.req("GET", "/v1/code/agents/shares/shr_nope", None, None).await.0, StatusCode::NOT_FOUND);

    // A revoked machine's shares stay: they're copies.
    let (_, machines) = w.req("GET", "/v1/code/agents/acme/machines", None, &w.owner).await;
    let mid = machines[0]["id"].as_str().unwrap().to_string();
    assert_eq!(w.req("DELETE", &format!("/v1/code/agents/acme/machines/{mid}"), None, &w.owner).await.0, StatusCode::NO_CONTENT);
    assert_eq!(w.a.req("GET", &format!("/v1/code/agents/shares/{}", all["id"].as_str().unwrap()), None, None).await.0, StatusCode::OK);
    assert_eq!(w.req("POST", create, None, &w.owner).await.0, StatusCode::NOT_FOUND, "its records are gone from the mirror");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_shares_are_trimmed_to_fit_or_refused() {
    let w = world().await;
    let (_, minted) = w.machine("box").await;
    let (mut d, _) = Daemon::connect(&w.url, &minted).await;
    // 80 commands with 64 KiB of output each: over 4 MiB until the outputs are cut.
    let mut items = vec![stamped(1, project("prj_1", "x")), stamped(2, thread("thr_a", "a"))];
    let out = "x".repeat(64 << 10);
    for i in 0..80u64 {
        items.push(stamped(
            3 + i,
            entity(json!({
                "kind": "node", "id": format!("nod_{i}"), "thread": "thr_a", "run": "run_1", "ordinal": i, "status": "completed", "started_at": i,
                "body": { "type": "tool", "name": "Bash", "kind": "command", "title": "yes", "output": out }
            })),
        ));
    }
    d.upsert(items).await;
    let (s, v) = w.req("POST", "/v1/code/agents/acme/threads/thr_a/shares", None, &w.owner).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert!(v["bytes"].as_u64().unwrap() < 1 << 20, "{v}");
    let (_, share) = w.a.req("GET", &format!("/v1/code/agents/shares/{}", v["id"].as_str().unwrap()), None, None).await;
    assert_eq!(share["trimmed"], true);
    let output = share["entities"][2]["body"]["output"].as_str().unwrap();
    assert!(output.len() < 5000 && output.contains("bytes left out of this share"), "{}", output.len());

    // Messages aren't cut: too much of them is refused.
    let text = "y".repeat(1 << 20);
    let msgs = (0..5u64).map(|i| stamped(200 + i, entity(json!({
        "kind": "node", "id": format!("nod_m{i}"), "thread": "thr_b", "run": "run_2", "ordinal": i, "status": "completed", "started_at": i,
        "body": { "type": "message", "text": text }
    }))));
    let mut items = vec![stamped(199, thread("thr_b", "b"))];
    items.extend(msgs);
    d.upsert(items).await;
    let (s, v) = w.req("POST", "/v1/code/agents/acme/threads/thr_b/shares", None, &w.owner).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v["error"].as_str().unwrap().contains("too large to share"), "{v}");
}
