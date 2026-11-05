//! The ACP adapter against a fake agent that replays recorded JSON-RPC transcripts
//! (`tests/fixtures/acp/*.ndjson`): T3 Code's ACP replay fixtures (MIT, from
//! `apps/server/src/orchestration-v2/testkit/fixtures/*/{registry,kiro,fx,grok}_transcript.ndjson`,
//! ported by normalizing T3's prompt wrapper and initialize capabilities), plus transcripts
//! written here. Each scenario's events are compared with `tests/fixtures/acp/<name>.events`
//! (regenerate with `ROLL_FIXTURES=write cargo test -p agent-host --test acp`), and the
//! important ones are asserted outright.
//!
//! The fake is this test binary run again (`fake_acp`). A transcript says which frames the
//! adapter must send (`expect_outbound`: requests and notifications by method with their params
//! matched as a subset, `<any>` matching anything and `<workspace>` the session's directory;
//! replies to the agent's requests by result) and which the agent sends (`emit_inbound`).
//! Transcripts carry no JSON-RPC ids: the fake answers the oldest open request of a method and
//! numbers its own. It speaks on fd 3 so libtest's output can't reach the protocol stream, logs
//! every frame it receives, and fails loudly (stderr, exit 3) on the first frame it didn't
//! expect: the adapter reports that as `Exited`.

use agent_host::provider::acp::{Acp, installed_agents, known_agents};
use agent_host::provider::{McpServer, Provider, ProviderConfig, ProviderEvent, Session, SessionConfig, TurnEnd, TurnInput};
use agent_proto::{
    ApprovalAction, AuthStatus, Decision, InteractionMode, NodeBody, NodeStatus, RequestBody, Response, RuntimeMode,
    TodoStatus, ToolKind,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

const TRANSCRIPT: &str = "ROLL_FAKE_ACP";
const LOG: &str = "ROLL_FAKE_ACP_LOG";

// ---------------------------------------------------------------------------------------------
// The fake
// ---------------------------------------------------------------------------------------------

/// Not a test: the fake agent, when the adapter runs this binary with `ROLL_FAKE_ACP` set.
#[test]
fn fake_acp() {
    let Ok(transcript) = std::env::var(TRANSCRIPT) else { return };
    let code = match replay(Path::new(&transcript), std::env::var(LOG).ok().map(PathBuf::from)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fake-acp: {e}");
            3
        }
    };
    std::process::exit(code);
}

/// `expected` matches `actual`: objects by the expected keys (all of them when `exact`), `<any>`
/// anything, a string holding `<any>` by its other parts in order.
fn matches(expected: &Value, actual: &Value, ws: &str, exact: bool) -> bool {
    match (expected, actual) {
        (Value::String(e), _) if e == "<any>" => true,
        (Value::String(e), Value::String(a)) => {
            let e = e.replace("<workspace>", ws);
            if !e.contains("<any>") {
                return e == *a;
            }
            let mut rest = a.as_str();
            for (i, part) in e.split("<any>").enumerate() {
                match rest.find(part) {
                    Some(at) if i > 0 || at == 0 => rest = &rest[at + part.len()..],
                    _ => return false,
                }
            }
            true
        }
        (Value::Object(e), Value::Object(a)) => {
            (!exact || e.len() == a.len()) && e.iter().all(|(k, v)| a.get(k).is_some_and(|av| matches(v, av, ws, exact)))
        }
        (Value::Array(e), Value::Array(a)) => e.len() == a.len() && e.iter().zip(a).all(|(e, a)| matches(e, a, ws, exact)),
        (e, a) => e == a,
    }
}

fn substitute(v: &mut Value, ws: &str) {
    match v {
        Value::String(s) if s.contains("<workspace>") => *s = s.replace("<workspace>", ws),
        Value::Array(items) => items.iter_mut().for_each(|i| substitute(i, ws)),
        Value::Object(map) => map.values_mut().for_each(|i| substitute(i, ws)),
        _ => {}
    }
}

fn replay(transcript: &Path, log: Option<PathBuf>) -> Result<i32, String> {
    let mut out = std::fs::OpenOptions::new().write(true).open("/dev/fd/3").map_err(|e| format!("fd 3: {e}"))?;
    let mut log = log.map(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).expect("log"));
    if let Some(log) = &mut log {
        writeln!(log, "{}", json!({ "pid": std::process::id() })).unwrap();
    }
    let ws = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap().display().to_string();
    let text = std::fs::read_to_string(transcript).map_err(|e| format!("{}: {e}", transcript.display()))?;
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    // The adapter's open requests by method, and the fake's.
    let mut ours: HashMap<String, VecDeque<Value>> = HashMap::new();
    let mut theirs: HashMap<String, VecDeque<Value>> = HashMap::new();
    let mut next_id = 1000;
    let mut read = |log: &mut Option<std::fs::File>| -> Option<Value> {
        let line = lines.next()?.ok()?;
        if let Some(log) = log {
            writeln!(log, "{line}").unwrap();
        }
        serde_json::from_str(&line).ok()
    };
    for (n, line) in text.lines().enumerate() {
        let entry: Value = serde_json::from_str(line).map_err(|e| format!("line {}: {e}", n + 1))?;
        let at = n + 1;
        let label = entry["label"].as_str().unwrap_or("?").to_string();
        match entry["type"].as_str().unwrap_or("") {
            "emit_inbound" => {
                let mut frame = entry["frame"].clone();
                substitute(&mut frame, &ws);
                let method = frame["method"].as_str().unwrap_or_default().to_string();
                let wire = match frame["kind"].as_str() {
                    Some("request") => {
                        next_id += 1;
                        theirs.entry(method.clone()).or_default().push_back(json!(next_id));
                        json!({ "jsonrpc": "2.0", "id": next_id, "method": method, "params": frame["params"] })
                    }
                    Some("notification") => json!({ "jsonrpc": "2.0", "method": method, "params": frame["params"] }),
                    Some("response") => {
                        let id = ours
                            .get_mut(&method)
                            .and_then(VecDeque::pop_front)
                            .ok_or_else(|| format!("line {at}: no open {method} request to answer ({label})"))?;
                        let mut wire = json!({ "jsonrpc": "2.0", "id": id });
                        match frame.get("error") {
                            Some(error) => wire["error"] = error.clone(),
                            None => wire["result"] = frame["result"].clone(),
                        }
                        wire
                    }
                    other => return Err(format!("line {at}: unknown kind {other:?}")),
                };
                writeln!(out, "{wire}").map_err(|e| e.to_string())?;
                out.flush().map_err(|e| e.to_string())?;
            }
            "expect_outbound" => {
                let expected = &entry["frame"];
                let method = expected["method"].as_str().unwrap_or_default().to_string();
                let Some(actual) = read(&mut log) else {
                    return Err(format!("line {at}: the adapter stopped before sending {label}"));
                };
                let ok = match expected["kind"].as_str() {
                    Some("request") => {
                        let ok = actual["method"] == json!(method) && actual.get("id").is_some() && matches(&expected["params"], &actual["params"], &ws, false);
                        if ok {
                            ours.entry(method.clone()).or_default().push_back(actual["id"].clone());
                        }
                        ok
                    }
                    Some("notification") => {
                        actual["method"] == json!(method) && actual.get("id").is_none() && matches(&expected["params"], &actual["params"], &ws, false)
                    }
                    Some("response") => {
                        let id = theirs.get_mut(&method).and_then(VecDeque::pop_front);
                        actual.get("method").is_none()
                            && id.as_ref() == actual.get("id")
                            && match expected.get("error") {
                                Some(error) => actual["error"]["code"] == error["code"],
                                None => matches(&expected["result"], &actual["result"], &ws, true),
                            }
                    }
                    other => return Err(format!("line {at}: unknown kind {other:?}")),
                };
                if !ok {
                    return Err(format!("line {at}: expected {label} {expected}, the adapter sent {actual}"));
                }
            }
            "runtime_exit" => {
                if let Some(stderr) = entry["stderr"].as_str() {
                    eprintln!("{stderr}");
                }
                match entry["status"].as_str() {
                    Some("error") => return Ok(1),
                    // Ignore stdin closing: only a kill ends this one.
                    Some("hang") => {
                        std::thread::sleep(Duration::from_secs(60));
                        return Ok(0);
                    }
                    _ => {
                        while read(&mut log).is_some() {}
                        return Ok(0);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp")
}

/// A `ProviderConfig` whose agent is the fake replaying `transcript`.
fn fake_config(transcript: &Path, log: &Path) -> ProviderConfig {
    let exe = std::env::current_exe().unwrap();
    let script = r#"exec "$0" fake_acp --exact --nocapture -q 3>&1 1>/dev/null"#;
    ProviderConfig {
        binary: Some("/bin/sh".into()),
        args: vec!["-c".into(), script.into(), exe.display().to_string()],
        env: vec![(TRANSCRIPT.into(), transcript.display().to_string()), (LOG.into(), log.display().to_string())],
        ..Default::default()
    }
}

fn transcript_path(name: &str) -> PathBuf {
    match name.starts_with('/') {
        true => PathBuf::from(name),
        false => fixtures().join(format!("{name}.ndjson")),
    }
}

fn session_config(cwd: &Path, runtime: RuntimeMode) -> SessionConfig {
    SessionConfig {
        cwd: cwd.to_path_buf(),
        model: None,
        effort: None,
        options: Default::default(),
        runtime_mode: runtime,
        interaction_mode: InteractionMode::Default,
        resume: None,
        resume_at: None,
        fork: None,
        env: Vec::new(),
        mcp: Vec::new(),
        instructions: None,
    }
}

struct Harness {
    dir: tempfile::TempDir,
    session: Box<dyn Session>,
    rx: mpsc::Receiver<ProviderEvent>,
    events: Vec<ProviderEvent>,
}

impl Harness {
    async fn open(transcript: &str, runtime: RuntimeMode) -> Self {
        Self::open_with(transcript, runtime, |_| {}).await
    }

    async fn open_with(transcript: &str, runtime: RuntimeMode, tweak: impl FnOnce(&mut SessionConfig)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self::open_in(dir, transcript, runtime, tweak).await
    }

    async fn open_in(dir: tempfile::TempDir, transcript: &str, runtime: RuntimeMode, tweak: impl FnOnce(&mut SessionConfig)) -> Self {
        let acp = Acp::new("test".into(), fake_config(&transcript_path(transcript), &dir.path().join("log.ndjson")));
        let mut cfg = session_config(&std::fs::canonicalize(dir.path()).unwrap(), runtime);
        tweak(&mut cfg);
        let (session, rx) = acp.start(cfg).await.expect("start");
        Self { dir, session, rx, events: Vec::new() }
    }

    fn ws(&self) -> PathBuf {
        std::fs::canonicalize(self.dir.path()).unwrap()
    }

    /// Collect events until one matches.
    async fn until(&mut self, what: &str, pred: impl Fn(&ProviderEvent) -> bool) -> ProviderEvent {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let event = tokio::time::timeout_at(deadline, self.rx.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}; got {:#?}", self.events))
                .unwrap_or_else(|| panic!("events ended waiting for {what}"));
            self.events.push(event.clone());
            if pred(&event) {
                return event;
            }
            if let ProviderEvent::Exited { error } = &event {
                panic!("the agent exited waiting for {what}: {error:?}");
            }
        }
    }

    async fn turn_end(&mut self) -> TurnEnd {
        match self.until("the turn's end", |e| matches!(e, ProviderEvent::TurnEnded(_))).await {
            ProviderEvent::TurnEnded(end) => end,
            _ => unreachable!(),
        }
    }

    async fn send(&mut self, text: &str) {
        self.session.send(TurnInput { text: text.into(), attachments: Vec::new() }).await.expect("send");
    }

    async fn turn(&mut self, text: &str) -> TurnEnd {
        self.send(text).await;
        self.turn_end().await
    }

    async fn request(&mut self) -> (String, Option<String>, RequestBody) {
        match self.until("a request", |e| matches!(e, ProviderEvent::Request { .. })).await {
            ProviderEvent::Request { id, item, body } => (id, item, body),
            _ => unreachable!(),
        }
    }

    async fn decide(&mut self, id: &str, decision: Decision) {
        self.session.respond(id, &Response::Decision { decision, message: None }).await.expect("respond");
    }

    /// Close the session and return every event (ending with `Exited`) and every frame the
    /// adapter sent.
    async fn finish(mut self) -> (Vec<ProviderEvent>, Vec<Value>) {
        self.session.close().await;
        self.until("exit", |e| matches!(e, ProviderEvent::Exited { .. })).await;
        let log = read_log(&self.dir.path().join("log.ndjson"));
        (self.events, log)
    }
}

fn read_log(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .filter(|v: &Value| v.get("pid").is_none())
        .collect()
}

/// The fake's pid, from the first line it logs.
fn fake_pid(dir: &Path) -> u32 {
    let log = std::fs::read_to_string(dir.join("log.ndjson")).unwrap();
    serde_json::from_str::<Value>(log.lines().next().unwrap()).unwrap()["pid"].as_u64().unwrap() as u32
}

fn sent<'a>(log: &'a [Value], method: &str) -> Vec<&'a Value> {
    log.iter().filter(|f| f["method"] == method).map(|f| &f["params"]).collect()
}

/// The replies the adapter sent to the agent's requests.
fn replies(log: &[Value]) -> Vec<&Value> {
    log.iter().filter(|f| f.get("method").is_none()).collect()
}

/// The prompts a transcript expects, in order.
fn prompts(name: &str) -> Vec<String> {
    std::fs::read_to_string(transcript_path(name))
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["type"] == "expect_outbound" && e["frame"]["method"] == "session/prompt")
        .map(|e| e["frame"]["params"]["prompt"][0]["text"].as_str().unwrap().to_string())
        .collect()
}

fn event_json(e: &ProviderEvent) -> Value {
    match e {
        ProviderEvent::NativeThread { id } => json!({ "native_thread": id }),
        ProviderEvent::Model { model } => json!({ "model": model }),
        ProviderEvent::Catalog { models } => json!({ "catalog": models }),
        ProviderEvent::Item(i) => json!({ "item": { "id": i.id, "parent": i.parent, "status": i.status, "body": i.body } }),
        ProviderEvent::TextDelta { item, text } => json!({ "text_delta": { "item": item, "text": text } }),
        ProviderEvent::OutputDelta { item, text } => json!({ "output_delta": { "item": item, "text": text } }),
        ProviderEvent::Request { id, item, body } => json!({ "request": { "id": id, "item": item, "body": body } }),
        ProviderEvent::RequestCancelled { id } => json!({ "request_cancelled": id }),
        ProviderEvent::Usage(u) => json!({ "usage": u }),
        ProviderEvent::Context(c) => json!({ "context": c }),
        ProviderEvent::TurnRef { id } => json!({ "turn_ref": id }),
        ProviderEvent::TurnEnded(TurnEnd::Completed) => json!({ "turn_ended": "completed" }),
        ProviderEvent::TurnEnded(TurnEnd::Interrupted) => json!({ "turn_ended": "interrupted" }),
        ProviderEvent::TurnEnded(TurnEnd::Failed(m)) => json!({ "turn_ended": { "failed": m } }),
        ProviderEvent::Exited { error } => json!({ "exited": error }),
    }
}

/// Compare a scenario's events with its golden file (workspace paths written as `<workspace>`).
fn golden(name: &str, events: &[ProviderEvent], ws: &Path) {
    let ws = ws.display().to_string();
    let actual: String = events.iter().map(|e| format!("{}\n", event_json(e).to_string().replace(&ws, "<workspace>"))).collect();
    let path = fixtures().join(format!("{name}.events"));
    if std::env::var("ROLL_FIXTURES").as_deref() == Ok("write") {
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("{} is missing; run with ROLL_FIXTURES=write", path.display()));
    if expected != actual {
        let (e, a): (Vec<_>, Vec<_>) = (expected.lines().collect(), actual.lines().collect());
        let at = e.iter().zip(&a).position(|(x, y)| x != y).unwrap_or(e.len().min(a.len()));
        panic!(
            "{name}: events differ from {} at event {}\nexpected: {}\n  actual: {}\n(ROLL_FIXTURES=write regenerates)",
            path.display(),
            at + 1,
            e.get(at).unwrap_or(&"<end>"),
            a.get(at).unwrap_or(&"<end>"),
        );
    }
}

fn items(events: &[ProviderEvent]) -> Vec<&agent_host::provider::Item> {
    events.iter().filter_map(|e| if let ProviderEvent::Item(i) = e { Some(i) } else { None }).collect()
}

/// The last version of each item, by id.
fn latest(events: &[ProviderEvent]) -> BTreeMap<String, agent_host::provider::Item> {
    items(events).into_iter().map(|i| (i.id.clone(), i.clone())).collect()
}

fn completed_texts(events: &[ProviderEvent], reasoning: bool) -> Vec<String> {
    items(events)
        .into_iter()
        .filter(|i| i.status == NodeStatus::Completed)
        .filter_map(|i| match (&i.body, reasoning) {
            (NodeBody::Message { text }, false) | (NodeBody::Reasoning { text }, true) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn messages(events: &[ProviderEvent]) -> Vec<String> {
    completed_texts(events, false)
}

fn turn_ends(events: &[ProviderEvent]) -> Vec<TurnEnd> {
    events.iter().filter_map(|e| if let ProviderEvent::TurnEnded(t) = e { Some(t.clone()) } else { None }).collect()
}

/// A transcript written for one test.
fn synthetic(dir: &Path, name: &str, entries: &[Value]) -> String {
    let path = dir.join(format!("{name}.ndjson"));
    let text: String = entries.iter().map(|e| format!("{e}\n")).collect();
    std::fs::write(&path, text).unwrap();
    path.display().to_string()
}

fn expect(kind: &str, method: &str, params: Value) -> Value {
    json!({ "type": "expect_outbound", "label": method, "frame": { "kind": kind, "method": method, "params": params } })
}

fn expect_reply(method: &str, result: Value) -> Value {
    json!({ "type": "expect_outbound", "label": format!("{method} reply"), "frame": { "kind": "response", "method": method, "result": result } })
}

fn expect_error(method: &str, code: i64) -> Value {
    json!({ "type": "expect_outbound", "label": format!("{method} error"), "frame": { "kind": "response", "method": method, "error": { "code": code } } })
}

fn emit(kind: &str, method: &str, body: Value) -> Value {
    let key = match kind {
        "response" => "result",
        _ => "params",
    };
    json!({ "type": "emit_inbound", "frame": { "kind": kind, "method": method, key: body } })
}

fn update(update: Value) -> Value {
    emit("notification", "session/update", json!({ "sessionId": "s1", "update": update }))
}

/// initialize (v1, with fs and terminals of no interest to it) and session/new answered `s1`.
fn opening() -> Vec<Value> {
    vec![
        expect("request", "initialize", json!({ "protocolVersion": 2 })),
        emit("response", "initialize", json!({ "protocolVersion": 1, "agentInfo": { "name": "synthetic", "version": "1.0.0" },
            "agentCapabilities": { "loadSession": true, "promptCapabilities": { "image": true }, "mcpCapabilities": { "http": true } } })),
        expect("request", "session/new", json!({ "cwd": "<workspace>", "mcpServers": "<any>" })),
        emit("response", "session/new", json!({ "sessionId": "s1" })),
    ]
}

fn exit_ok() -> Value {
    json!({ "type": "runtime_exit", "status": "success" })
}

// ---------------------------------------------------------------------------------------------
// T3's replay scenarios: generic registry agents
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn registry_simple() {
    let mut h = Harness::open("registry_simple", RuntimeMode::FullAccess).await;
    assert_eq!(h.turn(&prompts("registry_simple")[0]).await, TurnEnd::Completed);
    let ws = h.ws();
    let (events, log) = h.finish().await;
    golden("registry_simple", &events, &ws);
    assert_eq!(events[0], ProviderEvent::NativeThread { id: "grok-replay-session-1".into() });
    assert_eq!(messages(&events), ["fixture simple ok"]);
    assert_eq!(completed_texts(&events, true), ["I should answer exactly."]);
    let deltas: String = events
        .iter()
        .filter_map(|e| if let ProviderEvent::TextDelta { item, text } = e { item.starts_with("msg").then_some(text.as_str()) } else { None })
        .collect();
    assert_eq!(deltas, "fixture simple ok");
    assert_eq!(events.last(), Some(&ProviderEvent::Exited { error: None }));

    // The handshake offers v2 with v1's fields, and what roll implements.
    let init = sent(&log, "initialize")[0];
    assert_eq!(init["protocolVersion"], 2);
    assert_eq!(init["clientInfo"]["name"], "roll");
    assert_eq!(init["info"]["name"], "roll");
    assert_eq!(init["clientCapabilities"]["fs"], json!({ "readTextFile": true, "writeTextFile": true }));
    assert_eq!(init["clientCapabilities"]["terminal"], true);
    assert_eq!(sent(&log, "session/new")[0]["cwd"], json!(ws));
    assert_eq!(sent(&log, "session/prompt")[0]["prompt"], json!([{ "type": "text", "text": "Respond with the following text: fixture simple ok" }]));
}

#[tokio::test]
async fn registry_multi_turn() {
    let mut h = Harness::open("registry_multi_turn", RuntimeMode::FullAccess).await;
    for prompt in prompts("registry_multi_turn") {
        assert_eq!(h.turn(&prompt).await, TurnEnd::Completed);
    }
    let ws = h.ws();
    let (events, _) = h.finish().await;
    golden("registry_multi_turn", &events, &ws);
    assert_eq!(messages(&events).len(), 2);
    // Each turn's message is its own item.
    let ids: Vec<_> = items(&events).iter().filter(|i| matches!(i.body, NodeBody::Message { .. })).map(|i| i.id.clone()).collect();
    assert!(ids.contains(&"msg:1:1".to_string()) && ids.iter().any(|i| i.starts_with("msg:2:")));
}

#[tokio::test]
async fn registry_turn_interrupt() {
    let mut h = Harness::open("registry_turn_interrupt", RuntimeMode::FullAccess).await;
    h.send(&prompts("registry_turn_interrupt")[0]).await;
    h.session.interrupt().await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Interrupted);
    let ws = h.ws();
    let (events, log) = h.finish().await;
    golden("registry_turn_interrupt", &events, &ws);
    assert_eq!(sent(&log, "session/cancel"), [&json!({ "sessionId": "grok-replay-session-1" })]);
}

/// Steering is the orchestrator's interrupt and restart: cancel, then the new prompt.
#[tokio::test]
async fn registry_message_steering() {
    let p = prompts("registry_message_steering");
    let mut h = Harness::open("registry_message_steering", RuntimeMode::FullAccess).await;
    h.send(&p[0]).await;
    h.session.interrupt().await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Interrupted);
    assert_eq!(h.turn(&p[1]).await, TurnEnd::Completed);
    let ws = h.ws();
    let (events, _) = h.finish().await;
    golden("registry_message_steering", &events, &ws);
    assert_eq!(turn_ends(&events), [TurnEnd::Interrupted, TurnEnd::Completed]);
}

#[tokio::test]
async fn registry_tool_call_read_only() {
    let mut h = Harness::open("registry_tool_call_read_only", RuntimeMode::Ask).await;
    assert_eq!(h.turn(&prompts("registry_tool_call_read_only")[0]).await, TurnEnd::Completed);
    let ws = h.ws();
    let (events, _) = h.finish().await;
    golden("registry_tool_call_read_only", &events, &ws);
    let tools = latest(&events);
    let NodeBody::Tool(read) = &tools["read-package"].body else { panic!() };
    assert_eq!(read.kind, ToolKind::Read);
    assert_eq!(read.title, "Read package.json");
    assert_eq!(read.output.as_deref(), Some("{\"name\":\"cursor-read-only-fixture\"}"));
    assert_eq!(tools["read-tsconfig"].status, NodeStatus::Completed);
    // The message before the tools ended when the first tool started.
    assert_eq!(messages(&events), ["Reading both files now.\n", "read only tool fixture complete"]);
}

#[tokio::test]
async fn registry_tool_call_on_request() {
    let mut h = Harness::open("registry_tool_call_on_request", RuntimeMode::Ask).await;
    h.send(&prompts("registry_tool_call_on_request")[0]).await;
    let (id, item, body) = h.request().await;
    let RequestBody::Approval(approval) = body else { panic!() };
    assert_eq!(item.as_deref(), Some("write-probe"));
    assert_eq!(approval.action, ApprovalAction::Command);
    assert_eq!(approval.command.as_deref(), Some("printf 'codex app-server approval fixture' > .codex-probe-write-action.txt"));
    assert_eq!(approval.decisions, [Decision::Accept, Decision::AcceptForSession, Decision::Decline, Decision::Cancel]);
    h.decide(&id, Decision::Accept).await;
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let ws = h.ws();
    let (events, log) = h.finish().await;
    golden("registry_tool_call_on_request", &events, &ws);
    assert_eq!(replies(&log)[0]["result"], json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } }));
    let NodeBody::Tool(tool) = &latest(&events)["write-probe"].body else { panic!() };
    assert_eq!((tool.title.as_str(), tool.exit_code), ("printf 'codex app-server approval fixture' > .codex-probe-write-action.txt", Some(0)));
}

#[tokio::test]
async fn registry_todo_list() {
    let mut h = Harness::open("registry_todo_list", RuntimeMode::FullAccess).await;
    assert_eq!(h.turn(&prompts("registry_todo_list")[0]).await, TurnEnd::Completed);
    let ws = h.ws();
    let (events, _) = h.finish().await;
    golden("registry_todo_list", &events, &ws);
    let plan = &latest(&events)["plan:1"];
    assert_eq!(plan.status, NodeStatus::Completed);
    let NodeBody::Todos { items: todos } = &plan.body else { panic!() };
    assert_eq!(todos.len(), 3);
    assert!(todos.iter().all(|t| t.status == TodoStatus::Completed));
    let first = items(&events).into_iter().find(|i| i.id == "plan:1").unwrap();
    let NodeBody::Todos { items: first } = &first.body else { panic!() };
    assert_eq!((first[0].text.as_str(), first[0].status), ("Inspect package.json", TodoStatus::InProgress));
}

#[tokio::test]
async fn registry_prompt_error() {
    let mut h = Harness::open("registry_prompt_error", RuntimeMode::FullAccess).await;
    let TurnEnd::Failed(why) = h.turn(&prompts("registry_prompt_error")[0]).await else { panic!() };
    assert!(why.contains("grok prompt error fixture"), "{why}");
    let ws = h.ws();
    let (events, _) = h.finish().await;
    golden("registry_prompt_error", &events, &ws);
}

#[tokio::test]
async fn acp_elicitation() {
    let mut h = Harness::open("acp_elicitation", RuntimeMode::Ask).await;
    h.send(&prompts("acp_elicitation")[0]).await;
    let (id, _, body) = h.request().await;
    let RequestBody::Questions { questions } = body else { panic!() };
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].id, "schema_vs_ui_flexibility");
    assert_eq!(questions[0].header, "Fixture");
    assert_eq!(questions[0].question, "Should this fixture prefer strict schemas or UI flexibility?");
    assert_eq!(questions[0].options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), ["Strict schemas (Recommended)", "UI flexibility"]);
    let answers = [("schema_vs_ui_flexibility".to_string(), "Strict schemas (Recommended)".to_string())].into();
    h.session.respond(&id, &Response::Answers { answers }).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let ws = h.ws();
    let (events, _) = h.finish().await;
    golden("acp_elicitation", &events, &ws);
    assert_eq!(messages(&events), ["plan questions fixture complete"]);
}

#[tokio::test]
async fn malformed_session_update() {
    let mut h = Harness::open("malformed_session_update", RuntimeMode::FullAccess).await;
    assert_eq!(h.turn(&prompts("malformed_session_update")[0]).await, TurnEnd::Completed);
    let ws = h.ws();
    let (events, _) = h.finish().await;
    golden("malformed_session_update", &events, &ws);
    assert_eq!(messages(&events), ["malformed update fixture ok"]);
    // The malformed usage update (no `used`/`size`) is dropped; the valid one has no cost.
    assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Usage(_))));
    assert!(!items(&events).iter().any(|i| matches!(i.body, NodeBody::System { .. })));
}

// ---------------------------------------------------------------------------------------------
// Kiro
// ---------------------------------------------------------------------------------------------

fn kiro(cfg: &mut SessionConfig) {
    cfg.model = Some("claude-haiku-4.5".into());
}

#[tokio::test]
async fn kiro_simple() {
    let mut h = Harness::open_with("kiro_simple", RuntimeMode::FullAccess, kiro).await;
    assert_eq!(h.turn(&prompts("kiro_simple")[0]).await, TurnEnd::Completed);
    let ws = h.ws();
    let (events, log) = h.finish().await;
    golden("kiro_simple", &events, &ws);
    assert_eq!(messages(&events), ["fixture simple ok"]);
    assert!(events.contains(&ProviderEvent::Model { model: "claude-haiku-4.5".into() }));
    // Autopilot is already on for full access: only the model is written.
    assert_eq!(sent(&log, "session/set_config_option").len(), 1);
    assert_eq!(sent(&log, "session/close").len(), 1);
}

#[tokio::test]
async fn kiro_turn_interrupt_mid_tool() {
    let mut h = Harness::open_with("kiro_turn_interrupt_mid_tool", RuntimeMode::FullAccess, kiro).await;
    h.send(&prompts("kiro_turn_interrupt_mid_tool")[0]).await;
    // The permission is answered by policy; the tool runs (pending, then in progress twice).
    for _ in 0..3 {
        h.until("the tool", |e| matches!(e, ProviderEvent::Item(i) if matches!(i.body, NodeBody::Tool(_)))).await;
    }
    h.session.interrupt().await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Interrupted);
    let ws = h.ws();
    let (events, log) = h.finish().await;
    golden("kiro_turn_interrupt_mid_tool", &events, &ws);
    assert_eq!(replies(&log)[0]["result"], json!({ "outcome": { "outcome": "selected", "optionId": "accept" } }));
    // Kiro fails the cancelled tool: roll shows it as interrupted.
    let tool = latest(&events).into_values().find(|i| matches!(i.body, NodeBody::Tool(_))).unwrap();
    assert_eq!(tool.status, NodeStatus::Interrupted);
    let NodeBody::Tool(call) = &tool.body else { unreachable!() };
    assert_eq!(call.kind, ToolKind::Command);
    assert!(call.title.starts_with("node -e"), "{}", call.title);
    assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Request { .. })));
}

/// Ask: Kiro's write and its own end-of-turn review both come to the user; Kiro's
/// `allow_always` outlives the session, so it isn't offered.
#[tokio::test]
async fn kiro_permission_supervised() {
    let mut h = Harness::open_with("kiro_permission_supervised", RuntimeMode::Ask, kiro).await;
    h.send(&prompts("kiro_permission_supervised")[0]).await;
    for title in ["Edit kiro-permission-probe.txt", "Review changes"] {
        let (id, _, body) = h.request().await;
        let RequestBody::Approval(approval) = body else { panic!() };
        assert_eq!(approval.title, title);
        assert_eq!(approval.decisions, [Decision::Accept, Decision::Decline, Decision::Cancel]);
        h.decide(&id, Decision::Accept).await;
    }
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let ws = h.ws();
    let (events, log) = h.finish().await;
    golden("kiro_permission_supervised", &events, &ws);
    let options: Vec<_> = sent(&log, "session/set_config_option").iter().map(|p| (p["configId"].clone(), p["value"].clone())).collect();
    assert_eq!(options, [(json!("model"), json!("claude-haiku-4.5")), (json!("autopilot"), json!("off"))]);
    assert!(replies(&log).iter().all(|r| r["result"] == json!({ "outcome": { "outcome": "selected", "optionId": "accept" } })));
    assert_eq!(messages(&events).last().map(String::as_str), Some("kiro permission done"));
}

#[tokio::test]
async fn kiro_permission_full_access() {
    let mut h = Harness::open_with("kiro_permission_full_access", RuntimeMode::FullAccess, kiro).await;
    assert_eq!(h.turn(&prompts("kiro_permission_full_access")[0]).await, TurnEnd::Completed);
    let ws = h.ws();
    let (events, log) = h.finish().await;
    golden("kiro_permission_full_access", &events, &ws);
    assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Request { .. })));
    assert_eq!(replies(&log)[0]["result"], json!({ "outcome": { "outcome": "selected", "optionId": "accept" } }));
}

/// A second process loads the session: its replayed history doesn't show again.
#[tokio::test]
async fn kiro_resume() {
    let mut first = Harness::open_with("kiro_resume_1", RuntimeMode::FullAccess, kiro).await;
    assert_eq!(first.turn(&prompts("kiro_resume_1")[0]).await, TurnEnd::Completed);
    let ws = first.ws();
    let (events, _) = first.finish().await;
    golden("kiro_resume_1", &events, &ws);
    let ProviderEvent::NativeThread { id } = &events[0] else { panic!() };

    let resume = id.clone();
    let mut second = Harness::open_with("kiro_resume_2", RuntimeMode::FullAccess, |cfg| {
        kiro(cfg);
        cfg.resume = Some(resume);
    })
    .await;
    assert_eq!(second.turn(&prompts("kiro_resume_2")[0]).await, TurnEnd::Completed);
    let ws = second.ws();
    let (events, log) = second.finish().await;
    golden("kiro_resume_2", &events, &ws);
    assert_eq!(&events[0], &ProviderEvent::NativeThread { id: id.clone() });
    assert_eq!(sent(&log, "session/load")[0]["sessionId"], json!(id));
    // Only this turn's answer: the replayed first turn is gone.
    let said = messages(&events);
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(said[0].contains("provider thread resume fixture first turn complete"), "{said:?}");
}

// ---------------------------------------------------------------------------------------------
// fx
// ---------------------------------------------------------------------------------------------

fn fx(cfg: &mut SessionConfig) {
    cfg.effort = Some("medium".into());
}

#[tokio::test]
async fn fx_simple() {
    let mut h = Harness::open_with("fx_simple", RuntimeMode::Ask, fx).await;
    assert_eq!(h.turn(&prompts("fx_simple")[0]).await, TurnEnd::Completed);
    let ws = h.ws();
    let (events, log) = h.finish().await;
    golden("fx_simple", &events, &ws);
    // Ask maps onto fx's `ask` mode, after the effort.
    let options: Vec<_> = sent(&log, "session/set_config_option").iter().map(|p| (p["configId"].clone(), p["value"].clone())).collect();
    assert_eq!(options, [(json!("effort"), json!("medium")), (json!("mode"), json!("ask"))]);
    let usage = events.iter().find_map(|e| if let ProviderEvent::Usage(u) = e { Some(*u) } else { None }).unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (19540, 9));
    assert_eq!(events[1], ProviderEvent::Model { model: "anthropic/claude-opus-5.5".into() });
}

#[tokio::test]
async fn fx_tool_call_denied_write() {
    let mut h = Harness::open_with("fx_tool_call_denied_write", RuntimeMode::Ask, fx).await;
    h.send(&prompts("fx_tool_call_denied_write")[0]).await;
    let (id, item, body) = h.request().await;
    let RequestBody::Approval(approval) = body else { panic!() };
    assert_eq!(item.as_deref(), Some("toolu_01UGENWURULQ8Y3F7F8Npwzg"));
    assert_eq!(approval.action, ApprovalAction::FileChange);
    assert_eq!(approval.title, "Edit .claude-denied-write.txt");
    assert_eq!(approval.tool.as_deref(), Some("write_file"));
    h.decide(&id, Decision::Decline).await;
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let ws = h.ws();
    let (events, log) = h.finish().await;
    golden("fx_tool_call_denied_write", &events, &ws);
    assert_eq!(replies(&log)[0]["result"], json!({ "outcome": { "outcome": "selected", "optionId": "reject_once" } }));
}

#[tokio::test]
async fn fx_early_stop() {
    let p = prompts("fx_early_stop");
    let mut h = Harness::open_with("fx_early_stop", RuntimeMode::Ask, fx).await;
    h.send(&p[0]).await;
    h.session.interrupt().await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Interrupted);
    assert_eq!(h.turn(&p[1]).await, TurnEnd::Completed);
    let ws = h.ws();
    let (events, _) = h.finish().await;
    golden("fx_early_stop", &events, &ws);
    assert_eq!(messages(&events), ["fixture simple ok"]);
}

// ---------------------------------------------------------------------------------------------
// Grok
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn grok_simple() {
    let mut h = Harness::open("grok_simple", RuntimeMode::FullAccess).await;
    assert_eq!(h.turn(&prompts("grok_simple")[0]).await, TurnEnd::Completed);
    let ws = h.ws();
    let (events, _) = h.finish().await;
    golden("grok_simple", &events, &ws);
    assert_eq!(messages(&events).concat(), "fixture simple ok");
    assert_eq!(completed_texts(&events, true).len(), 1);
}

#[tokio::test]
async fn grok_tool_call_read_only() {
    let mut h = Harness::open("grok_tool_call_read_only", RuntimeMode::FullAccess).await;
    assert_eq!(h.turn(&prompts("grok_tool_call_read_only")[0]).await, TurnEnd::Completed);
    let ws = h.ws();
    let (events, _) = h.finish().await;
    golden("grok_tool_call_read_only", &events, &ws);
    assert!(latest(&events).values().any(|i| matches!(&i.body, NodeBody::Tool(t) if t.kind == ToolKind::Read)));
}

#[tokio::test]
async fn grok_plan_questions() {
    let mut h = Harness::open("grok_plan_questions", RuntimeMode::FullAccess).await;
    h.send(&prompts("grok_plan_questions")[0]).await;
    let (id, item, body) = h.request().await;
    assert_eq!(item.as_deref(), Some("ask-user-question-tool-call-1"));
    let RequestBody::Questions { questions } = body else { panic!() };
    assert_eq!(questions[0].options.len(), 2);
    assert!(!questions[0].multi);
    let answers = [(questions[0].id.clone(), "Strict schemas (Recommended)".to_string())].into();
    h.session.respond(&id, &Response::Answers { answers }).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let ws = h.ws();
    let (events, _) = h.finish().await;
    golden("grok_plan_questions", &events, &ws);
}

/// Subagents' own sessions report under other session ids: only the parent's `Task` tools show,
/// as subagents with their results.
#[tokio::test]
async fn grok_subagent_lineage() {
    let mut h = Harness::open("grok_subagent_lineage", RuntimeMode::FullAccess).await;
    assert_eq!(h.turn(&prompts("grok_subagent_lineage")[0]).await, TurnEnd::Completed);
    let ws = h.ws();
    let (events, _) = h.finish().await;
    golden("grok_subagent_lineage", &events, &ws);
    assert_eq!(messages(&events), ["Audit complete."]);
    let subagents: Vec<_> = latest(&events)
        .into_values()
        .filter_map(|i| match i.body {
            NodeBody::Subagent { agent, result, .. } => Some((agent, result.unwrap_or_default(), i.status)),
            _ => None,
        })
        .collect();
    assert_eq!(subagents.len(), 2);
    assert!(subagents.iter().all(|(_, _, status)| *status == NodeStatus::Completed));
    assert!(subagents.iter().any(|(agent, result, _)| agent == "generalPurpose" && result.starts_with("Server audit complete.")));
}

#[tokio::test]
async fn grok_prompt_error() {
    let p = prompts("grok_prompt_error");
    let mut h = Harness::open("grok_prompt_error", RuntimeMode::FullAccess).await;
    let TurnEnd::Failed(why) = h.turn(&p[0]).await else { panic!() };
    assert!(why.contains("400 Bad Request"), "{why}");
    // The session survives a failed prompt.
    assert_eq!(h.turn(&p[1]).await, TurnEnd::Completed);
    let ws = h.ws();
    let (events, _) = h.finish().await;
    golden("grok_prompt_error", &events, &ws);
    assert_eq!(messages(&events).concat(), "grok prompt error fixture recovered");
}

// ---------------------------------------------------------------------------------------------
// Written here
// ---------------------------------------------------------------------------------------------

/// `fs/*` stays inside the workspace; writes and terminals follow the runtime mode.
#[tokio::test]
async fn client_methods_are_confined_to_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let ws = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(ws.join("notes.txt"), "a\nb\nc\n").unwrap();
    let mut t = opening();
    t.extend([
        expect("request", "session/prompt", json!({ "sessionId": "s1" })),
        emit("request", "fs/read_text_file", json!({ "sessionId": "s1", "path": "<workspace>/notes.txt", "line": 2, "limit": 1 })),
        expect_reply("fs/read_text_file", json!({ "content": "b\n" })),
        emit("request", "fs/read_text_file", json!({ "sessionId": "s1", "path": "/etc/hostname" })),
        expect_error("fs/read_text_file", -32603),
        emit("request", "fs/read_text_file", json!({ "sessionId": "s1", "path": "<workspace>/../escape" })),
        expect_error("fs/read_text_file", -32603),
        // Ask mode: no write and no terminal until the user approves one.
        emit("request", "fs/write_text_file", json!({ "sessionId": "s1", "path": "<workspace>/out.txt", "content": "x" })),
        expect_error("fs/write_text_file", -32603),
        emit("request", "terminal/create", json!({ "sessionId": "s1", "command": "echo", "args": ["hi"] })),
        expect_error("terminal/create", -32603),
        update(json!({ "sessionUpdate": "tool_call", "toolCallId": "w", "kind": "edit", "title": "Write out.txt", "locations": [{ "path": "<workspace>/out.txt" }] })),
        emit("request", "session/request_permission", json!({ "sessionId": "s1", "toolCall": { "toolCallId": "w" },
            "options": [{ "optionId": "y", "kind": "allow_once" }, { "optionId": "n", "kind": "reject_once" }] })),
        expect_reply("session/request_permission", json!({ "outcome": { "outcome": "selected", "optionId": "y" } })),
        emit("request", "fs/write_text_file", json!({ "sessionId": "s1", "path": "<workspace>/out.txt", "content": "written" })),
        expect_reply("fs/write_text_file", json!({})),
        emit("request", "unknown/method", json!({})),
        expect_error("unknown/method", -32601),
        update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "w", "status": "completed" })),
        emit("response", "session/prompt", json!({ "stopReason": "end_turn" })),
        exit_ok(),
    ]);
    let path = synthetic(dir.path(), "confined", &t);
    let mut h = Harness::open_in(dir, &path, RuntimeMode::Ask, |_| {}).await;
    h.send("go").await;
    let (id, item, body) = h.request().await;
    assert_eq!(item.as_deref(), Some("w"));
    let RequestBody::Approval(approval) = body else { panic!() };
    assert_eq!(approval.files[0].path, "out.txt");
    h.decide(&id, Decision::Accept).await;
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    assert_eq!(std::fs::read_to_string(ws.join("out.txt")).unwrap(), "written");
    let (_, log) = h.finish().await;
    assert!(replies(&log).iter().any(|r| r["error"]["message"].as_str().is_some_and(|m| m.contains("outside the workspace"))));
}

/// Full access runs the agent's terminals; the tool that embeds one shows its output.
#[tokio::test]
async fn terminals_run_and_embed_their_output() {
    let mut t = opening();
    t.extend([
        expect("request", "session/prompt", json!({ "sessionId": "s1" })),
        emit("request", "terminal/create", json!({ "sessionId": "s1", "command": "printf", "args": ["terminal says hi"] })),
        expect_reply("terminal/create", json!({ "terminalId": "roll-term-1" })),
        emit("request", "terminal/wait_for_exit", json!({ "sessionId": "s1", "terminalId": "roll-term-1" })),
        expect_reply("terminal/wait_for_exit", json!({ "exitCode": 0, "signal": null })),
        update(json!({ "sessionUpdate": "tool_call", "toolCallId": "run", "kind": "execute", "title": "printf", "status": "completed",
            "content": [{ "type": "terminal", "terminalId": "roll-term-1" }] })),
        emit("request", "terminal/output", json!({ "sessionId": "s1", "terminalId": "roll-term-1" })),
        expect_reply("terminal/output", json!({ "output": "terminal says hi", "truncated": false, "exitStatus": { "exitCode": 0, "signal": null } })),
        emit("request", "terminal/release", json!({ "sessionId": "s1", "terminalId": "roll-term-1" })),
        expect_reply("terminal/release", json!({})),
        emit("response", "session/prompt", json!({ "stopReason": "end_turn" })),
        exit_ok(),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let path = synthetic(dir.path(), "terminal", &t);
    let mut h = Harness::open_in(dir, &path, RuntimeMode::FullAccess, |_| {}).await;
    assert_eq!(h.turn("go").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    let NodeBody::Tool(call) = &latest(&events)["run"].body else { panic!() };
    assert_eq!((call.output.as_deref(), call.exit_code), (Some("terminal says hi"), Some(0)));
}

/// Stop while a permission is pending: the request is answered `cancelled` (and withdrawn)
/// before `session/cancel`, as ACP requires.
#[tokio::test]
async fn cancel_with_a_pending_permission() {
    let mut t = opening();
    t.extend([
        expect("request", "session/prompt", json!({ "sessionId": "s1" })),
        update(json!({ "sessionUpdate": "tool_call", "toolCallId": "rm", "kind": "execute", "rawInput": { "command": "rm -rf build" } })),
        emit("request", "session/request_permission", json!({ "sessionId": "s1", "toolCall": { "toolCallId": "rm" }, "options": [{ "optionId": "y", "kind": "allow_once" }] })),
        expect_reply("session/request_permission", json!({ "outcome": { "outcome": "cancelled" } })),
        expect("notification", "session/cancel", json!({ "sessionId": "s1" })),
        update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "rm", "status": "failed" })),
        emit("response", "session/prompt", json!({ "stopReason": "cancelled" })),
        exit_ok(),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let path = synthetic(dir.path(), "cancel", &t);
    let mut h = Harness::open_in(dir, &path, RuntimeMode::Ask, |_| {}).await;
    h.send("clean up").await;
    let (id, _, body) = h.request().await;
    let RequestBody::Approval(approval) = body else { panic!() };
    assert_eq!(approval.title, "Run `rm -rf build`");
    h.session.interrupt().await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Interrupted);
    let (events, _) = h.finish().await;
    assert!(events.contains(&ProviderEvent::RequestCancelled { id: id.clone() }));
    assert_eq!(latest(&events)["rm"].status, NodeStatus::Interrupted);
    // A late answer finds nothing to answer.
}

/// An ACP v2 agent: typed MCP servers, config options by `configId`, and a prompt that ends
/// with `state_update: idle`.
#[tokio::test]
async fn a_v2_agent() {
    let t = vec![
        expect("request", "initialize", json!({ "protocolVersion": 2 })),
        emit("response", "initialize", json!({ "protocolVersion": 2, "info": { "name": "v2-agent", "version": "0.2.0" },
            "capabilities": { "session": { "prompt": { "image": {} }, "mcp": { "http": {} } } } })),
        expect("request", "session/new", json!({ "cwd": "<workspace>",
            "mcpServers": [{ "type": "http", "name": "roll", "url": "https://roll.test/mcp", "headers": [{ "name": "Authorization", "value": "Bearer t" }] }] })),
        emit("response", "session/new", json!({ "sessionId": "s1", "configOptions": [
            { "type": "select", "configId": "model", "name": "Model", "category": "model", "currentValue": "small", "options": [{ "value": "small", "name": "Small" }, { "value": "big", "name": "Big" }] }] })),
        expect("request", "session/set_config_option", json!({ "sessionId": "s1", "configId": "model", "value": "big", "type": "id" })),
        emit("response", "session/set_config_option", json!({ "configOptions": [
            { "type": "select", "configId": "model", "name": "Model", "category": "model", "currentValue": "big", "options": [{ "value": "big", "name": "Big" }] }] })),
        expect("request", "session/prompt", json!({ "sessionId": "s1", "prompt": [{ "type": "text", "text": "hi" }, { "type": "image", "mimeType": "image/png", "data": "iVBO" }] })),
        emit("response", "session/prompt", json!({})),
        update(json!({ "sessionUpdate": "agent_message", "messageId": "m1", "content": [{ "type": "text", "text": "hello " }, { "type": "text", "text": "v2" }] })),
        update(json!({ "sessionUpdate": "state_update", "state": "idle", "stopReason": "end_turn", "usage": { "inputTokens": 3, "outputTokens": 4 } })),
        expect("request", "session/close", json!({ "sessionId": "s1" })),
        emit("response", "session/close", json!({})),
        exit_ok(),
    ];
    let dir = tempfile::tempdir().unwrap();
    let path = synthetic(dir.path(), "v2", &t);
    let mut h = Harness::open_in(dir, &path, RuntimeMode::FullAccess, |cfg| {
        cfg.model = Some("big".into());
        cfg.mcp = vec![McpServer { name: "roll".into(), url: "https://roll.test/mcp".into(), headers: vec![("Authorization".into(), "Bearer t".into())] }];
    })
    .await;
    let image = agent_proto::Attachment { name: "a.png".into(), mime: "image/png".into(), data: "iVBO".into() };
    h.session.send(TurnInput { text: "hi".into(), attachments: vec![image] }).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    assert_eq!(messages(&events), ["hello v2"]);
    assert!(events.contains(&ProviderEvent::Model { model: "big".into() }));
    assert!(events.iter().any(|e| matches!(e, ProviderEvent::Usage(u) if u.input_tokens == 3 && u.output_tokens == 4)));
}

/// Plan mode picks the agent's plan mode (legacy `session/set_mode`) and leaving it goes back;
/// instructions ride in front of the first prompt only.
#[tokio::test]
async fn plan_mode_and_instructions() {
    let t = vec![
        expect("request", "initialize", json!({ "protocolVersion": 2 })),
        emit("response", "initialize", json!({ "protocolVersion": 1, "agentInfo": { "name": "gemini-like", "version": "1" } })),
        expect("request", "session/new", json!({ "cwd": "<workspace>", "mcpServers": [] })),
        emit("response", "session/new", json!({ "sessionId": "s1", "modes": { "currentModeId": "default",
            "availableModes": [{ "id": "default", "name": "Default" }, { "id": "autoEdit", "name": "Auto edit" }, { "id": "plan", "name": "Plan" }] } })),
        expect("request", "session/set_mode", json!({ "sessionId": "s1", "modeId": "plan" })),
        emit("response", "session/set_mode", json!({})),
        expect("request", "session/prompt", json!({ "prompt": [{ "type": "text", "text": "<roll_instructions>\nbe brief\n</roll_instructions>\n\nplan it" }] })),
        update(json!({ "sessionUpdate": "plan_update", "plan": { "type": "markdown", "planId": "p", "content": "1. do it" } })),
        emit("response", "session/prompt", json!({ "stopReason": "end_turn" })),
        expect("request", "session/set_mode", json!({ "sessionId": "s1", "modeId": "default" })),
        emit("response", "session/set_mode", json!({})),
        expect("request", "session/prompt", json!({ "prompt": [{ "type": "text", "text": "do it" }] })),
        update(json!({ "sessionUpdate": "current_mode_update", "currentModeId": "autoEdit" })),
        emit("response", "session/prompt", json!({ "stopReason": "end_turn" })),
        exit_ok(),
    ];
    let dir = tempfile::tempdir().unwrap();
    let path = synthetic(dir.path(), "plan", &t);
    let mut h = Harness::open_in(dir, &path, RuntimeMode::FullAccess, |cfg| {
        cfg.interaction_mode = InteractionMode::Plan;
        cfg.instructions = Some("be brief".into());
    })
    .await;
    assert_eq!(h.turn("plan it").await, TurnEnd::Completed);
    h.session.set_modes(RuntimeMode::FullAccess, InteractionMode::Default).await.unwrap();
    assert_eq!(h.turn("do it").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    assert!(items(&events).iter().any(|i| i.body == NodeBody::Plan { text: "1. do it".into() }));
}

/// An agent that can't resume fails `start`, so the orchestrator starts fresh with a handoff;
/// an agent that dies reports why.
#[tokio::test]
async fn resume_unsupported_and_crashes() {
    let dir = tempfile::tempdir().unwrap();
    let t = vec![
        expect("request", "initialize", json!({ "protocolVersion": 2 })),
        emit("response", "initialize", json!({ "protocolVersion": 1, "agentInfo": { "name": "forgetful", "version": "1" } })),
        exit_ok(),
    ];
    let path = synthetic(dir.path(), "noresume", &t);
    let acp = Acp::new("test".into(), fake_config(Path::new(&path), &dir.path().join("log.ndjson")));
    let mut cfg = session_config(dir.path(), RuntimeMode::FullAccess);
    cfg.resume = Some("old".into());
    let err = acp.start(cfg).await.err().expect("no resume");
    assert!(err.to_string().contains("can't resume"), "{err:#}");

    let mut t = opening();
    t.extend([
        expect("request", "session/prompt", json!({ "sessionId": "s1" })),
        json!({ "type": "runtime_exit", "status": "error", "stderr": "agent fell over" }),
    ]);
    let path = synthetic(dir.path(), "crash", &t);
    let mut h = Harness::open(&path, RuntimeMode::FullAccess).await;
    h.send("go").await;
    let ProviderEvent::Exited { error } = h.until("exit", |e| matches!(e, ProviderEvent::Exited { .. })).await else { unreachable!() };
    assert!(error.unwrap().contains("agent fell over"));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn close_kills_an_agent_that_ignores_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let mut t = opening();
    t.push(json!({ "type": "runtime_exit", "status": "hang" }));
    let path = synthetic(dir.path(), "hang", &t);
    let mut h = Harness::open(&path, RuntimeMode::FullAccess).await;
    let pid = fake_pid(h.dir.path());
    let started = std::time::Instant::now();
    h.session.close().await;
    assert!(started.elapsed() < Duration::from_secs(5), "close took {:?}", started.elapsed());
    h.until("exit", |e| matches!(e, ProviderEvent::Exited { .. })).await;
    // Killed and reaped: no process, not even a zombie.
    assert!(!Path::new(&format!("/proc/{pid}")).exists(), "the agent {pid} is still there");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn dropping_a_session_kills_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let mut t = opening();
    t.push(json!({ "type": "runtime_exit", "status": "hang" }));
    let path = synthetic(dir.path(), "drop", &t);
    let h = Harness::open(&path, RuntimeMode::FullAccess).await;
    let pid = fake_pid(h.dir.path());
    let Harness { session, dir: _dir, .. } = h;
    drop(session);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while Path::new(&format!("/proc/{pid}")).exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!Path::new(&format!("/proc/{pid}")).exists(), "the agent {pid} is still there");
}

/// `info` runs `initialize` only: never a session, never a login.
#[tokio::test]
async fn info_probes_without_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let t = vec![
        expect("request", "initialize", json!({ "protocolVersion": 2 })),
        emit("response", "initialize", serde_json::from_str::<Value>(include_str!("fixtures/acp/antigravity-initialize.json")).unwrap()),
        exit_ok(),
    ];
    let path = synthetic(dir.path(), "info", &t);
    let log = dir.path().join("log.ndjson");
    let info = Acp::new("antigravity".into(), fake_config(Path::new(&path), &log)).info().await;
    assert!(info.installed);
    assert_eq!(info.id.as_str(), "acp:antigravity");
    assert_eq!(info.name, "Google Antigravity");
    assert_eq!(info.version.as_deref(), Some("1.1.1"));
    assert_eq!(info.auth, AuthStatus::Unknown);
    assert!(info.capabilities.images && info.capabilities.mode_switch && !info.capabilities.steer);
    assert!(info.detail.unwrap().contains("Log in with Google"));
    let methods: Vec<_> = read_log(&log).iter().map(|f| f["method"].clone()).collect();
    assert_eq!(methods, [json!("initialize")]);

    let missing = Acp::new("gemini".into(), ProviderConfig { binary: Some("roll-no-such-agent".into()), ..Default::default() }).info().await;
    assert!(!missing.installed);
    assert_eq!(missing.name, "Gemini CLI");
    assert!(missing.detail.unwrap().contains("not installed"));
}

/// An agent on the PATH that never answers `initialize` isn't offered: nothing delegates to a hung
/// agent until a refresh finds it answering.
#[tokio::test]
async fn info_marks_an_agent_that_never_answers_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let t = vec![expect("request", "initialize", json!({ "protocolVersion": 2 })), json!({ "type": "runtime_exit", "status": "hang" })];
    let path = synthetic(dir.path(), "silent", &t);
    let log = dir.path().join("log.ndjson");
    let info = Acp::new("grok".into(), fake_config(Path::new(&path), &log)).with_probe_timeout(Duration::from_millis(300)).info().await;
    assert!(!info.installed);
    assert!(info.detail.unwrap().contains("no answer to initialize"), "says why");
}

#[test]
fn known_agents_cover_t3s() {
    let names: Vec<_> = known_agents().into_iter().map(|a| a.name).collect();
    for name in ["gemini", "opencode", "grok", "kiro", "fx", "qwen", "goose", "copilot"] {
        assert!(names.contains(&name.to_string()), "{name}");
    }
    assert!(installed_agents().len() <= names.len());
}
