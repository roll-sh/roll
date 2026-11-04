//! The relay link (`relay.rs`): against a scripted relay (a WebSocket server in the test, so
//! every frame the daemon sends is checked) and end to end against code-control's real relay
//! (its router on a real port), with a fake `Daemon` on the other side.

use agent_host::relay::{self, LinkConfig, LinkError, LinkEvent};
use agent_host::{Daemon, Outbound};
use agent_proto::{Command, DaemonFrame, Entity, MachineInfo, RelayFrame, Reply, Stamped};
use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

// ── a daemon ──

/// Records in memory, stamped like the real store; commands answered from receipts.
struct Fake {
    records: parking_lot::Mutex<BTreeMap<(String, String), Stamped>>,
    seq: AtomicU64,
    tx: broadcast::Sender<Outbound>,
    receipts: parking_lot::Mutex<HashMap<String, Reply>>,
    runs: AtomicU64,
}

impl Fake {
    fn new() -> Arc<Fake> {
        Arc::new(Fake { records: Default::default(), seq: AtomicU64::new(0), tx: broadcast::channel(1024).0, receipts: Default::default(), runs: AtomicU64::new(0) })
    }

    /// Store a project and broadcast it; its seq.
    fn upsert(&self, id: &str, name: &str) -> u64 {
        let entity: Entity = serde_json::from_value(json!({ "kind": "project", "id": id, "name": name, "path": "/x", "created_at": 1, "updated_at": 1 })).unwrap();
        let mut records = self.records.lock();
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let s = Stamped { seq, entity };
        records.insert(("project".into(), id.into()), s.clone());
        let _ = self.tx.send(Outbound::Entity(Box::new(s)));
        seq
    }
}

fn info(host: &str) -> MachineInfo {
    MachineInfo { hostname: host.into(), os: "linux".into(), arch: "x86_64".into(), version: "0".into(), home: "/home/me".into(), providers: vec![], settings: None }
}

#[async_trait]
impl Daemon for Fake {
    async fn info(&self) -> MachineInfo {
        info("box")
    }

    async fn since(&self, after: u64, limit: usize) -> anyhow::Result<Vec<Stamped>> {
        // A store read takes a moment, so a replay of thousands of records takes a while.
        tokio::time::sleep(Duration::from_millis(10)).await;
        let mut v: Vec<Stamped> = self.records.lock().values().filter(|s| s.seq > after).cloned().collect();
        v.sort_by_key(|s| s.seq);
        v.truncate(limit);
        Ok(v)
    }

    fn subscribe(&self) -> broadcast::Receiver<Outbound> {
        self.tx.subscribe()
    }

    async fn last_seq(&self) -> anyhow::Result<u64> {
        Ok(self.seq.load(Ordering::SeqCst))
    }

    /// `thread.archive` of `slow` takes 300 ms, of `write` writes a project (`Created {seq}`);
    /// every other command answers `Created {by}`; once per id.
    async fn run(&self, id: String, by: String, command: Command) -> Reply {
        if let Some(r) = self.receipts.lock().get(&id) {
            return r.clone();
        }
        if matches!(&command, Command::ThreadArchive { thread } if thread.as_str() == "slow") {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        self.runs.fetch_add(1, Ordering::SeqCst);
        let r = if matches!(&command, Command::ThreadArchive { thread } if thread.as_str() == "write") {
            Reply::Created { id: self.upsert(&format!("prj_{id}"), "written").to_string() }
        } else {
            Reply::Created { id: by }
        };
        self.receipts.lock().insert(id, r.clone());
        r
    }
}

fn archive(thread: &str) -> Command {
    Command::ThreadArchive { thread: agent_proto::ThreadId(thread.into()) }
}

fn start(daemon: Arc<dyn Daemon>, api: &str, bearer: &str) -> (tokio::task::JoinHandle<Result<(), LinkError>>, mpsc::UnboundedReceiver<LinkEvent>, oneshot::Sender<()>) {
    let (events, rx) = mpsc::unbounded_channel();
    let (stop, stopped) = oneshot::channel::<()>();
    let cfg = LinkConfig { api: api.into(), token: bearer.into() };
    let task = tokio::spawn(relay::run(daemon, cfg, events, async move {
        let _ = stopped.await;
    }));
    (task, rx, stop)
}

// ── a scripted relay ──

type Server = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

/// Accepts links on a port; each accepted socket (after checking the bearer) comes out of `rx`.
// tungstenite's handshake callback returns its (large) error response by value.
#[allow(clippy::result_large_err)]
async fn scripted(bearer: &'static str) -> (String, mpsc::UnboundedReceiver<Server>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { return };
            let check = |req: &tokio_tungstenite::tungstenite::handshake::server::Request, resp| {
                assert_eq!(req.uri().path(), "/v1/code/agents/connect");
                if req.headers().get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {bearer}")) {
                    Ok(resp)
                } else {
                    let mut r = tokio_tungstenite::tungstenite::handshake::server::ErrorResponse::new(None);
                    *r.status_mut() = StatusCode::UNAUTHORIZED;
                    Err(r)
                }
            };
            if let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, check).await {
                let _ = tx.send(ws);
            }
        }
    });
    (url, rx)
}

async fn next(ws: &mut Server) -> DaemonFrame {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), ws.next()).await.expect("a frame within 10 s") {
            Some(Ok(Message::Text(t))) => return serde_json::from_str(t.as_str()).unwrap(),
            Some(Ok(_)) => continue,
            other => panic!("the link ended: {other:?}"),
        }
    }
}

async fn send(ws: &mut Server, f: &RelayFrame) {
    ws.send(Message::text(serde_json::to_string(f).unwrap())).await.unwrap();
}

/// Hello, then welcome with `resume_from`.
async fn welcome(ws: &mut Server, resume_from: u64) {
    assert!(matches!(next(ws).await, DaemonFrame::Hello { protocol: agent_proto::PROTOCOL, .. }));
    send(ws, &RelayFrame::Welcome { machine: agent_proto::MachineId("mch_1".into()), owner: "acme".into(), resume_from }).await;
}

/// Entity frames until `upto` arrives: the seqs, in the order sent, and each frame's size.
async fn seqs(ws: &mut Server, upto: u64) -> (Vec<u64>, Vec<usize>) {
    let (mut got, mut frames) = (Vec::new(), Vec::new());
    while got.last() != Some(&upto) {
        match next(ws).await {
            DaemonFrame::Entities { items } => {
                frames.push(items.len());
                got.extend(items.iter().map(|s| s.seq));
            }
            other => panic!("expected entities, got {other:?}"),
        }
    }
    (got, frames)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_link_speaks_the_protocol() {
    let daemon = Fake::new();
    for i in 1..=300 {
        daemon.upsert(&format!("prj_{i}"), "x");
    }
    let (url, mut links) = scripted("rlm_good").await;
    let (task, mut events, stop) = start(daemon.clone(), &url, "rlm_good");
    let mut ws = links.recv().await.unwrap();
    welcome(&mut ws, 100).await;
    // The replay: everything after resume_from, in order, once, in batches of at most 256.
    let (got, frames) = seqs(&mut ws, 300).await;
    assert_eq!(got, (101..=300).collect::<Vec<_>>());
    assert!(frames.iter().all(|n| *n <= 256), "{frames:?}");
    loop {
        if let LinkEvent::Connected { machine, owner, resume_from } = events.recv().await.unwrap() {
            assert_eq!((machine.as_str(), owner.as_str(), resume_from), ("mch_1", "acme", 100));
            break;
        }
    }
    send(&mut ws, &RelayFrame::Ack { seq: 300 }).await;

    // Live: a new record, a delta and changed info.
    let s = daemon.upsert("prj_live", "y");
    assert_eq!(seqs(&mut ws, s).await.0, [s]);
    let delta = agent_proto::Delta::Terminal { terminal: agent_proto::TerminalId("trm_1".into()), data: "aGk=".into() };
    daemon.tx.send(Outbound::Delta(delta.clone())).unwrap();
    assert_eq!(next(&mut ws).await, DaemonFrame::Delta { delta });
    daemon.tx.send(Outbound::Info(info("renamed"))).unwrap();
    assert!(matches!(next(&mut ws).await, DaemonFrame::Hello { info, .. } if info.hostname == "renamed"));

    // Commands run at once and answer in the order they finish; a repeated id isn't run again.
    send(&mut ws, &RelayFrame::Command { id: "c1".into(), by: "acme".into(), command: archive("slow"), grant: None }).await;
    send(&mut ws, &RelayFrame::Command { id: "c2".into(), by: "agent:bot".into(), command: archive("fast"), grant: None }).await;
    let order: Vec<(String, Reply)> = [next(&mut ws).await, next(&mut ws).await]
        .into_iter()
        .map(|f| match f {
            DaemonFrame::Reply { id, result } => (id, result),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(order, [("c2".to_string(), Reply::Created { id: "agent:bot".into() }), ("c1".to_string(), Reply::Created { id: "acme".into() })]);
    send(&mut ws, &RelayFrame::Command { id: "c1".into(), by: "acme".into(), command: archive("slow"), grant: None }).await;
    assert!(matches!(next(&mut ws).await, DaemonFrame::Reply { id, .. } if id == "c1"));
    assert_eq!(daemon.runs.load(Ordering::SeqCst), 2);

    // Pings get pongs.
    send(&mut ws, &RelayFrame::Ping).await;
    assert_eq!(next(&mut ws).await, DaemonFrame::Pong);

    // The relay goes away: the link reconnects and resumes after what the relay says it holds.
    drop(ws);
    let missed = daemon.upsert("prj_missed", "z");
    let mut ws = tokio::time::timeout(Duration::from_secs(5), links.recv()).await.expect("reconnected").unwrap();
    welcome(&mut ws, s).await;
    assert_eq!(seqs(&mut ws, missed).await.0, [missed]);
    let after = daemon.upsert("prj_after", "w");
    assert_eq!(seqs(&mut ws, after).await.0, [after]);

    // Revoked: the link stops for good.
    ws.close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame { code: 4401.into(), reason: "revoked".into() })).await.unwrap();
    let res = tokio::time::timeout(Duration::from_secs(5), task).await.expect("stopped").unwrap();
    assert!(matches!(res, Err(LinkError::Revoked)), "{res:?}");
    drop(stop);
}

/// A command that finishes while a big backlog is still being replayed answers only after the
/// replay has sent the record it wrote (the reply used to overtake the replay).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replies_wait_for_their_records_during_a_replay() {
    let daemon = Fake::new();
    for i in 1..=5000 {
        daemon.upsert(&format!("prj_{i}"), "x");
    }
    let (url, mut links) = scripted("rlm_good").await;
    let (_task, _events, _stop) = start(daemon.clone(), &url, "rlm_good");
    let mut ws = links.recv().await.unwrap();
    welcome(&mut ws, 0).await;
    send(&mut ws, &RelayFrame::Command { id: "w1".into(), by: "acme".into(), command: archive("write"), grant: None }).await;
    let mut sent = std::collections::BTreeSet::new();
    let written = loop {
        match next(&mut ws).await {
            DaemonFrame::Entities { items } => sent.extend(items.iter().map(|s| s.seq)),
            DaemonFrame::Reply { id, result: Reply::Created { id: seq } } if id == "w1" => break seq.parse::<u64>().unwrap(),
            other => panic!("{other:?}"),
        }
    };
    assert!(written > 5000);
    let missing: Vec<u64> = (1..=written).filter(|s| !sent.contains(s)).collect();
    assert!(missing.is_empty(), "the reply came before {} of the records up to {written} (first {:?})", missing.len(), missing.first());
}

/// A frame the daemon can't read (garbage, a newer relay's frame, a command it doesn't know) is
/// skipped: the link stays up, and an unknown command gets an `invalid` reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreadable_frames_are_skipped() {
    let daemon = Fake::new();
    let (url, mut links) = scripted("rlm_good").await;
    let (_task, mut events, _stop) = start(daemon.clone(), &url, "rlm_good");
    let mut ws = links.recv().await.unwrap();
    welcome(&mut ws, 0).await;
    for text in ["not json", r#"{"type":"from_the_future","x":1}"#, r#"{"type":"command","id":"u1","by":"acme","command":{"type":"thread.teleport"}}"#] {
        ws.send(Message::text(text)).await.unwrap();
    }
    match next(&mut ws).await {
        DaemonFrame::Reply { id, result: Reply::Error(e) } => {
            assert_eq!((id.as_str(), e.code), ("u1", agent_proto::ErrorCode::Invalid));
        }
        other => panic!("{other:?}"),
    }
    // Still the same link: commands and pings work.
    send(&mut ws, &RelayFrame::Command { id: "c1".into(), by: "acme".into(), command: archive("fast"), grant: None }).await;
    assert!(matches!(next(&mut ws).await, DaemonFrame::Reply { id, .. } if id == "c1"));
    send(&mut ws, &RelayFrame::Ping).await;
    assert_eq!(next(&mut ws).await, DaemonFrame::Pong);
    // A notice (records over the machine's quota) reaches the user, and the link goes on.
    send(&mut ws, &RelayFrame::Notice { message: "roll didn't keep 2 of this machine's records".into() }).await;
    send(&mut ws, &RelayFrame::Ping).await;
    assert_eq!(next(&mut ws).await, DaemonFrame::Pong);
    let mut noticed = false;
    while let Ok(e) = events.try_recv() {
        assert!(!matches!(e, LinkEvent::Disconnected { .. }), "{e:?}");
        noticed |= matches!(e, LinkEvent::Notice { ref message } if message.contains("didn't keep 2"));
    }
    assert!(noticed, "the notice is a link event");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_tokens_and_shutdown() {
    let (url, _links) = scripted("rlm_good").await;
    // 401 at the upgrade: stop with a clear error, no retrying.
    let (task, _events, _stop) = start(Fake::new(), &url, "rlm_bad");
    let res = tokio::time::timeout(Duration::from_secs(5), task).await.expect("stopped").unwrap();
    let err = res.unwrap_err();
    assert!(matches!(err, LinkError::Revoked));
    assert!(err.to_string().contains("roll agent login"), "{err}");
    assert!(matches!(relay::check(&LinkConfig { api: url.clone(), token: "rlm_bad".into() }).await, Err(LinkError::Revoked)));
    relay::check(&LinkConfig { api: url.clone(), token: "rlm_good".into() }).await.unwrap();

    // Nobody listening: it keeps retrying (with backoff) until shut down.
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_url = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    let (task, mut events, stop) = start(Fake::new(), &dead_url, "rlm_good");
    let mut retries = Vec::new();
    while retries.len() < 2 {
        if let LinkEvent::Disconnected { retry, .. } = events.recv().await.unwrap() {
            retries.push(retry);
        }
    }
    assert!(retries[0] >= Duration::from_millis(500) && retries[0] <= Duration::from_secs(1), "{retries:?}");
    assert!(retries[1] >= Duration::from_secs(1) && retries[1] <= Duration::from_secs(2), "{retries:?}");
    stop.send(()).unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap().is_ok());
}

// ── against the real relay ──

/// A stopping daemon (SIGTERM: a sandbox going to sleep) sends the records its shutdown wrote,
/// runs no new command (the relay sends it again on the next link), and closes the link once the
/// relay has acknowledged everything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopping_daemon_sends_its_last_records_then_closes() {
    let daemon = Fake::new();
    let (url, mut links) = scripted("rlm_good").await;
    let (events, _events) = mpsc::unbounded_channel();
    let (stop, stopped) = oneshot::channel::<()>();
    let last = daemon.clone();
    let cfg = LinkConfig { api: url, token: "rlm_good".into() };
    let task = tokio::spawn(relay::run(daemon.clone(), cfg, events, async move {
        let _ = stopped.await;
        // What `Host::shutdown` does: parks and interrupts, and stores the records.
        last.upsert("prj_last", "final");
    }));
    let mut ws = links.recv().await.unwrap();
    welcome(&mut ws, 0).await;
    stop.send(()).unwrap();
    assert_eq!(seqs(&mut ws, 1).await.0, vec![1]);
    tokio::time::sleep(Duration::from_millis(50)).await;
    send(&mut ws, &RelayFrame::Command { id: "late".into(), by: "acme".into(), command: archive("t"), grant: None }).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!task.is_finished(), "open until the relay has the records");
    send(&mut ws, &RelayFrame::Ack { seq: 1 }).await;
    let closed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the link closes once acked");
    assert!(tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap().is_ok());
    assert_eq!(daemon.runs.load(Ordering::SeqCst), 0, "no command runs while stopping");
}

struct Relay {
    router: axum::Router,
    url: String,
    _dir: tempfile::TempDir,
}

impl Relay {
    async fn start() -> Relay {
        let dir = tempfile::tempdir().unwrap();
        let objects = pg_storage::Objects::new(&pg_storage::StorageConfig::Local(dir.path().join("objects"))).unwrap();
        let code = code_control::Code::open(code_control::Config::local(dir.path().to_path_buf()), objects).unwrap();
        let router = code_control::api::router(code);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let r = router.clone();
        tokio::spawn(async move { axum::serve(listener, r).await.unwrap() });
        Relay { router, url, _dir: dir }
    }

    async fn req(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let b = Request::builder().method(method).uri(path).header("content-type", "application/json");
        let req = b.body(body.map(|v| Body::from(v.to_string())).unwrap_or_else(Body::empty)).unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let s = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 << 20).await.unwrap();
        (s, if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() })
    }

    /// Projects in `/state`, by id → name.
    async fn projects(&self) -> BTreeMap<String, String> {
        let (_, v) = self.req("GET", "/v1/code/agents/acme/state", None).await;
        let state: agent_proto::State = serde_json::from_value(v).unwrap();
        state
            .entities
            .into_iter()
            .filter_map(|m| match m.entity {
                Entity::Project(p) => Some((p.id.0, p.name)),
                _ => None,
            })
            .collect()
    }

    async fn online(&self) -> bool {
        self.req("GET", "/v1/code/agents/acme/machines", None).await.1[0]["online"] == true
    }
}

/// The real thing: login-made token, mirrored records in `/state` and on the event stream,
/// a command round trip, a daemon restart that resumes without loss or duplicates, and
/// revoking the machine stopping the link.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_and_relay_end_to_end() {
    let relay = Relay::start().await;
    relay.req("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" }))).await;
    let (s, m) = relay.req("POST", "/v1/code/agents/acme/machines", Some(json!({ "name": "box" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{m}");
    let (mid, minted) = (m["machine"]["id"].as_str().unwrap().to_string(), m["token"].as_str().unwrap().to_string());

    let daemon = Fake::new();
    for i in 1..=600 {
        daemon.upsert(&format!("prj_{i}"), &format!("v{i}"));
    }
    // The event stream, opened before the daemon connects.
    let resp = relay.router.clone().oneshot(Request::get("/v1/code/agents/acme/events").body(Body::empty()).unwrap()).await.unwrap();
    let mut sse = resp.into_body().into_data_stream();
    let mut seen: Vec<(u64, String, String)> = Vec::new();
    let mut buf = String::new();
    let mut read_until = async |seen: &mut Vec<(u64, String, String)>, id: &str, name: &str| {
        while !seen.iter().any(|(_, i, n)| i == id && n == name) {
            let chunk = tokio::time::timeout(Duration::from_secs(10), sse.next()).await.expect("an event").unwrap().unwrap();
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(end) = buf.find("\n\n") {
                let block: String = buf.drain(..end + 2).collect();
                let Some(data) = block.lines().find_map(|l| l.strip_prefix("data:")) else { continue };
                if let agent_proto::StreamEvent::Entity(m) = serde_json::from_str(data.trim()).unwrap()
                    && let Entity::Project(p) = m.entity
                {
                    seen.push((m.cursor, p.id.0, p.name));
                }
            }
        }
    };

    let (task, _events, stop) = start(daemon.clone(), &relay.url, &minted);
    read_until(&mut seen, "prj_600", "v600").await;
    assert_eq!(seen.len(), 600);
    assert!(relay.online().await);
    assert_eq!(relay.projects().await.len(), 600);

    // A command from the API to the daemon and back.
    let path = format!("/v1/code/agents/acme/machines/{mid}/commands");
    let body = json!({ "id": "c1", "command": serde_json::to_value(archive("thr_1")).unwrap() });
    assert_eq!(relay.req("POST", &path, Some(body.clone())).await, (StatusCode::OK, json!({ "type": "created", "id": "admin" })));
    // The same id again: the daemon's receipt, not a second run.
    assert_eq!(relay.req("POST", &path, Some(body)).await.0, StatusCode::OK);
    assert_eq!(daemon.runs.load(Ordering::SeqCst), 1);

    // Live changes.
    daemon.upsert("prj_1", "changed");
    read_until(&mut seen, "prj_1", "changed").await;

    // The daemon stops; records change meanwhile; it comes back and resumes.
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    for _ in 0..200 {
        if !relay.online().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!relay.online().await);
    daemon.upsert("prj_2", "while away");
    daemon.upsert("prj_601", "new");
    let (task, mut events, _stop) = start(daemon.clone(), &relay.url, &minted);
    read_until(&mut seen, "prj_601", "new").await;
    loop {
        if let LinkEvent::Connected { resume_from, .. } = events.recv().await.unwrap() {
            assert_eq!(resume_from, 601, "the relay held everything up to the restart");
            break;
        }
    }
    // Every change once, in cursor order, nothing lost.
    let cursors: Vec<u64> = seen.iter().map(|(c, _, _)| *c).collect();
    assert!(cursors.windows(2).all(|w| w[0] < w[1]), "cursors ascend");
    assert_eq!(seen.len(), 603, "600 + changed + while away + new, each once");
    let projects = relay.projects().await;
    assert_eq!(projects.len(), 601);
    assert_eq!((projects["prj_1"].as_str(), projects["prj_2"].as_str()), ("changed", "while away"));

    // Revoking the machine stops the link with a clear error.
    assert_eq!(relay.req("DELETE", &format!("/v1/code/agents/acme/machines/{mid}"), None).await.0, StatusCode::NO_CONTENT);
    let res = tokio::time::timeout(Duration::from_secs(5), task).await.expect("stopped").unwrap();
    assert!(matches!(res, Err(LinkError::Revoked)), "{res:?}");
}

// ── a real daemon whose provider is slow to probe ──

/// `roll agent up` says hello without waiting for its providers' probes: an ACP agent that never
/// answers `initialize` shows as still being checked, and its status follows when the probe
/// gives up (docs/code/agents.md, "Providers").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hello_does_not_wait_for_a_stuck_probe() {
    use agent_host::provider::Provider;
    use agent_host::provider::acp::Acp;
    let home = tempfile::tempdir().unwrap();
    // An agent that starts and never says anything.
    let toml = "[[acp]]\nname = \"stuck\"\nbinary = \"/bin/sh\"\nargs = [\"-c\", \"exec sleep 600\"]\n";
    std::fs::write(home.path().join("config.toml"), toml).unwrap();
    let config = agent_host::config::Config::load_from(home.path().to_path_buf()).unwrap();
    let agent = config.acp[0].clone();
    let acp: Arc<dyn Provider> = Arc::new(Acp::new(agent.name, agent.config).with_probe_timeout(Duration::from_secs(2)));
    let (url, mut links) = scripted("rlm_good").await;

    let started = tokio::time::Instant::now();
    let host = agent_host::Host::open(config, vec![acp]).await.unwrap();
    let (_task, _events, _stop) = start(host, &url, "rlm_good");
    let mut ws = links.recv().await.unwrap();
    let DaemonFrame::Hello { info, .. } = next(&mut ws).await else { panic!("hello first") };
    assert!(started.elapsed() < Duration::from_secs(1), "hello took {:?}", started.elapsed());
    let p = &info.providers[0];
    assert_eq!(p.id.0, "acp:stuck");
    assert!(p.checking && !p.installed, "{p:?}");
    send(&mut ws, &RelayFrame::Welcome { machine: agent_proto::MachineId("mch_1".into()), owner: "acme".into(), resume_from: 0 }).await;

    // The probe gives up after its 2 s, and `hello` comes again with what it found.
    let p = loop {
        if let DaemonFrame::Hello { info, .. } = next(&mut ws).await
            && !info.providers[0].checking
        {
            break info.providers[0].clone();
        }
    };
    assert!(started.elapsed() >= Duration::from_secs(2), "{p:?}");
    assert_eq!(p.auth, agent_proto::AuthStatus::Unknown);
    let detail = p.detail.unwrap_or_default();
    assert!(detail.contains("no answer to initialize"), "{detail}");
}
