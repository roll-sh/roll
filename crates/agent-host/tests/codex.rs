//! The Codex adapter against a fake `codex app-server` that replays recorded JSON-RPC
//! transcripts (`tests/fixtures/codex/*.ndjson`): T3 Code's Codex replay fixtures (MIT, from
//! `apps/server/src/orchestration-v2/testkit/fixtures/*/codex_transcript.ndjson`) and sessions
//! recorded from codex-cli 0.164.0. Each scenario's events are compared with
//! `tests/fixtures/codex/<name>.events` (regenerate with `ROLL_FIXTURES=write cargo test -p
//! agent-host --test codex`), and the important ones are asserted outright.
//!
//! The fake is this test binary run again (`fake_codex`): a transcript says which frames the
//! adapter must send (`expect_outbound`: requests and notifications by method, replies to server
//! requests exactly) and which Codex sends back (`emit_inbound`). It speaks on fd 3 so libtest's
//! own output can't reach the protocol stream, logs every frame it receives, and fails loudly
//! (stderr, exit 3) on the first frame it didn't expect: the adapter reports that as `Exited`.

use agent_host::provider::codex::Codex;
use agent_host::provider::{McpServer, Provider, ProviderConfig, ProviderEvent, Session, SessionConfig, TurnEnd, TurnInput};
use agent_proto::{
    ApprovalAction, AuthStatus, ChangeKind, Decision, InteractionMode, NodeBody, NodeStatus, RequestBody, Response,
    RuntimeMode, ToolKind,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const TRANSCRIPT: &str = "ROLL_FAKE_CODEX";
const LOG: &str = "ROLL_FAKE_CODEX_LOG";

// ---------------------------------------------------------------------------------------------
// The fake
// ---------------------------------------------------------------------------------------------

/// Not a test: the fake Codex, when the adapter runs this binary with `ROLL_FAKE_CODEX` set.
#[test]
fn fake_codex() {
    let Ok(transcript) = std::env::var(TRANSCRIPT) else { return };
    let code = match replay(Path::new(&transcript), std::env::var(LOG).ok().map(PathBuf::from)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fake-codex: {e}");
            3
        }
    };
    std::process::exit(code);
}

fn replay(transcript: &Path, log: Option<PathBuf>) -> Result<i32, String> {
    let mut out = std::fs::OpenOptions::new().write(true).open("/dev/fd/3").map_err(|e| format!("fd 3: {e}"))?;
    let mut log = log.map(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).expect("log"));
    if let Some(log) = &mut log {
        writeln!(log, "{}", json!({ "pid": std::process::id() })).unwrap();
    }
    let text = std::fs::read_to_string(transcript).map_err(|e| format!("{}: {e}", transcript.display()))?;
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    // The transcript's ids for the adapter's requests → the ids the adapter used.
    let mut ids: HashMap<String, Value> = HashMap::new();
    let mut read = |log: &mut Option<std::fs::File>| -> Option<Value> {
        let line = lines.next()?.ok()?;
        if let Some(log) = log {
            writeln!(log, "{line}").unwrap();
        }
        serde_json::from_str(&line).ok()
    };
    for (n, line) in text.lines().enumerate() {
        let entry: Value = serde_json::from_str(line).map_err(|e| format!("line {}: {e}", n + 1))?;
        match entry["type"].as_str().unwrap_or("") {
            "emit_inbound" => {
                let mut frame = entry["frame"].clone();
                if frame.get("method").is_none()
                    && let Some(id) = frame.get("id").map(Value::to_string)
                    && let Some(actual) = ids.get(&id)
                {
                    frame["id"] = actual.clone();
                }
                writeln!(out, "{frame}").map_err(|e| e.to_string())?;
                out.flush().map_err(|e| e.to_string())?;
            }
            "expect_outbound" => {
                let expected = &entry["frame"];
                let label = entry["label"].as_str().unwrap_or("?");
                let Some(actual) = read(&mut log) else {
                    return Err(format!("line {}: the adapter stopped before sending {label}", n + 1));
                };
                match expected.get("method") {
                    Some(method) => {
                        if actual.get("method") != Some(method) {
                            return Err(format!("line {}: expected {method}, the adapter sent {actual}", n + 1));
                        }
                        if let Some(id) = expected.get("id") {
                            ids.insert(id.to_string(), actual["id"].clone());
                        }
                    }
                    None => {
                        let code = |f: &Value| f.get("error").map(|e| e["code"].clone());
                        if actual.get("id") != expected.get("id")
                            || actual.get("result") != expected.get("result")
                            || code(&actual) != code(expected)
                        {
                            return Err(format!("line {}: expected reply {expected}, the adapter sent {actual}", n + 1));
                        }
                    }
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
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex")
}

/// A `ProviderConfig` whose `codex` is the fake replaying `transcript`.
fn fake_config(transcript: &Path, log: &Path) -> ProviderConfig {
    let exe = std::env::current_exe().unwrap();
    let script = r#"if [ "$1" = --version ]; then echo "codex-cli 0.164.0"; exit 0; fi
exec "$0" fake_codex --exact --nocapture -q 3>&1 1>/dev/null"#;
    ProviderConfig {
        binary: Some("/bin/sh".into()),
        args: vec!["-c".into(), script.into(), exe.display().to_string()],
        env: vec![(TRANSCRIPT.into(), transcript.display().to_string()), (LOG.into(), log.display().to_string())],
    }
}

struct Harness {
    dir: tempfile::TempDir,
    codex: Codex,
    session: Box<dyn Session>,
    rx: mpsc::Receiver<ProviderEvent>,
    events: Vec<ProviderEvent>,
}

fn session_config(cwd: &Path) -> SessionConfig {
    SessionConfig {
        cwd: cwd.to_path_buf(),
        model: None,
        effort: None,
        runtime_mode: RuntimeMode::FullAccess,
        interaction_mode: InteractionMode::Default,
        resume: None,
        env: Vec::new(),
        mcp: Vec::new(),
        instructions: None,
    }
}

impl Harness {
    async fn open(transcript: &str) -> Self {
        Self::open_with(transcript, |_| {}).await
    }

    async fn open_with(transcript: &str, tweak: impl FnOnce(&mut SessionConfig)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = match transcript.starts_with('/') {
            true => PathBuf::from(transcript),
            false => fixtures().join(format!("{transcript}.ndjson")),
        };
        let codex = Codex::new(fake_config(&path, &dir.path().join("log.ndjson")));
        let mut cfg = session_config(dir.path());
        tweak(&mut cfg);
        let (session, rx) = codex.start(cfg).await.expect("start");
        Self { dir, codex, session, rx, events: Vec::new() }
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
                panic!("codex exited waiting for {what}: {error:?}");
            }
        }
    }

    async fn turn_end(&mut self) -> TurnEnd {
        match self.until("the turn's end", |e| matches!(e, ProviderEvent::TurnEnded(_))).await {
            ProviderEvent::TurnEnded(end) => end,
            _ => unreachable!(),
        }
    }

    async fn turn(&mut self, text: &str) -> TurnEnd {
        self.session.send(TurnInput { text: text.into(), attachments: Vec::new() }).await.expect("send");
        self.turn_end().await
    }

    async fn request(&mut self) -> (String, Option<String>, RequestBody) {
        match self.until("a request", |e| matches!(e, ProviderEvent::Request { .. })).await {
            ProviderEvent::Request { id, item, body } => (id, item, body),
            _ => unreachable!(),
        }
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

fn sent<'a>(log: &'a [Value], method: &str) -> Vec<&'a Value> {
    log.iter().filter(|f| f["method"] == method).map(|f| &f["params"]).collect()
}

fn event_json(e: &ProviderEvent) -> Value {
    match e {
        ProviderEvent::NativeThread { id } => json!({ "native_thread": id }),
        ProviderEvent::Model { model } => json!({ "model": model }),
        ProviderEvent::Item(i) => json!({ "item": { "id": i.id, "parent": i.parent, "status": i.status, "body": i.body } }),
        ProviderEvent::TextDelta { item, text } => json!({ "text_delta": { "item": item, "text": text } }),
        ProviderEvent::OutputDelta { item, text } => json!({ "output_delta": { "item": item, "text": text } }),
        ProviderEvent::Request { id, item, body } => json!({ "request": { "id": id, "item": item, "body": body } }),
        ProviderEvent::RequestCancelled { id } => json!({ "request_cancelled": id }),
        ProviderEvent::Usage(u) => json!({ "usage": u }),
        ProviderEvent::TurnEnded(TurnEnd::Completed) => json!({ "turn_ended": "completed" }),
        ProviderEvent::TurnEnded(TurnEnd::Interrupted) => json!({ "turn_ended": "interrupted" }),
        ProviderEvent::TurnEnded(TurnEnd::Failed(m)) => json!({ "turn_ended": { "failed": m } }),
        ProviderEvent::Exited { error } => json!({ "exited": error }),
    }
}

/// Compare a scenario's events with its golden file.
fn golden(name: &str, events: &[ProviderEvent]) {
    let actual: String = events.iter().map(|e| format!("{}\n", event_json(e))).collect();
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

fn messages(events: &[ProviderEvent]) -> Vec<String> {
    let mut seen = Vec::new();
    let mut out: Vec<String> = Vec::new();
    for i in items(events) {
        if let NodeBody::Message { text } = &i.body
            && i.status == NodeStatus::Completed
            && !seen.contains(&i.id)
        {
            seen.push(i.id.clone());
            out.push(text.clone());
        }
    }
    out
}

/// A transcript written for one test.
fn synthetic(dir: &Path, name: &str, entries: &[Value]) -> String {
    let path = dir.join(format!("{name}.ndjson"));
    let text: String = entries.iter().map(|e| format!("{e}\n")).collect();
    std::fs::write(&path, text).unwrap();
    path.display().to_string()
}

fn expect(frame: Value) -> Value {
    json!({ "type": "expect_outbound", "label": frame["method"].as_str().unwrap_or("reply"), "frame": frame })
}

fn emit(frame: Value) -> Value {
    json!({ "type": "emit_inbound", "frame": frame })
}

/// initialize, thread/start answered with thread `thr`, turn/start answered with turn `t1`.
fn opening() -> Vec<Value> {
    vec![
        expect(json!({ "id": 1, "method": "initialize" })),
        emit(json!({ "id": 1, "result": { "userAgent": "roll/0.164.0", "codexHome": "/h", "platformFamily": "unix", "platformOs": "linux" } })),
        expect(json!({ "method": "initialized" })),
        expect(json!({ "id": 2, "method": "thread/start" })),
        emit(json!({ "id": 2, "result": { "thread": { "id": "thr" }, "model": "gpt-test" } })),
        expect(json!({ "id": 3, "method": "turn/start" })),
        emit(json!({ "id": 3, "result": { "turn": { "id": "t1", "status": "inProgress", "items": [] } } })),
        emit(json!({ "method": "turn/started", "params": { "threadId": "thr", "turn": { "id": "t1", "status": "inProgress" } } })),
    ]
}

fn turn_completed(status: &str) -> Value {
    emit(json!({ "method": "turn/completed", "params": { "threadId": "thr", "turn": { "id": "t1", "status": status, "items": [] } } }))
}

fn exit_ok() -> Value {
    json!({ "type": "runtime_exit", "status": "success" })
}

// ---------------------------------------------------------------------------------------------
// T3's replay scenarios
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn simple() {
    let mut h = Harness::open("simple").await;
    assert_eq!(h.turn("Respond with the following text: fixture simple ok").await, TurnEnd::Completed);
    let (events, log) = h.finish().await;
    golden("simple", &events);
    assert_eq!(events[0], ProviderEvent::NativeThread { id: "01a0d5eb-f0c0-7481-a417-5e8871805cbc".into() });
    assert_eq!(events[1], ProviderEvent::Model { model: "gpt-6-luna".into() });
    assert_eq!(messages(&events), ["fixture simple ok"]);
    let deltas: String =
        events.iter().filter_map(|e| if let ProviderEvent::TextDelta { text, .. } = e { Some(text.as_str()) } else { None }).collect();
    assert_eq!(deltas, "fixture simple ok");
    assert!(events.iter().any(|e| matches!(e, ProviderEvent::Usage(u) if u.input_tokens == 22730 && u.output_tokens == 7)));
    assert_eq!(events.last(), Some(&ProviderEvent::Exited { error: None }));

    // What the adapter sent: T3's handshake and turn parameters for full access.
    let init = sent(&log, "initialize")[0];
    assert_eq!(init["capabilities"]["optOutNotificationMethods"], json!(["turn/diff/updated"]));
    assert_eq!(init["clientInfo"]["name"], "roll");
    let start = sent(&log, "thread/start")[0];
    assert_eq!(start["config"]["tools.update_plan.enabled"], true);
    assert!(start.get("model").is_none());
    let turn = sent(&log, "turn/start")[0];
    assert_eq!(turn["approvalPolicy"], "never");
    assert_eq!(turn["sandboxPolicy"], json!({ "type": "dangerFullAccess" }));
    assert_eq!(turn["approvalsReviewer"], "user");
    assert_eq!(turn["summary"], "detailed");
    assert_eq!(turn["collaborationMode"], json!({ "mode": "default", "settings": { "model": "gpt-6-luna" } }));
    assert_eq!(turn["input"], json!([{ "type": "text", "text": "Respond with the following text: fixture simple ok", "text_elements": [] }]));
}

#[tokio::test]
async fn multi_turn() {
    let mut h = Harness::open("multi_turn").await;
    assert_eq!(h.turn("Respond with exactly: first fixture turn complete").await, TurnEnd::Completed);
    assert_eq!(h.turn("Respond with exactly: second fixture turn complete").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("multi_turn", &events);
    assert_eq!(messages(&events), ["first fixture turn complete", "second fixture turn complete"]);
}

#[tokio::test]
async fn message_steering() {
    let mut h = Harness::open("message_steering").await;
    h.session.send(TurnInput { text: "Respond with exactly: steering fixture initial response".into(), ..Default::default() }).await.unwrap();
    h.session.steer(TurnInput { text: "Actually, respond with exactly: steering fixture observed".into(), ..Default::default() }).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let (events, log) = h.finish().await;
    golden("message_steering", &events);
    assert_eq!(messages(&events), ["steering fixture initial response", "steering fixture observed"]);
    let steer = sent(&log, "turn/steer")[0];
    assert_eq!(steer["expectedTurnId"], "01a0d5ed-670e-7032-9a87-707c736491a6");
    assert_eq!(steer["threadId"], "01a0d5ed-65ea-7d42-b8e9-d06f045cfa86");
}

#[tokio::test]
async fn turn_interrupt() {
    let mut h = Harness::open("turn_interrupt").await;
    h.session.send(TurnInput { text: "sleep".into(), ..Default::default() }).await.unwrap();
    h.session.interrupt().await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Interrupted);
    let (events, log) = h.finish().await;
    golden("turn_interrupt", &events);
    assert_eq!(sent(&log, "turn/interrupt")[0]["turnId"], "01a0d5ed-38b3-7a11-9203-bd9650a02764");
}

#[tokio::test]
async fn turn_interrupt_mid_tool() {
    let mut h = Harness::open("turn_interrupt_mid_tool").await;
    h.session.send(TurnInput { text: "run node".into(), ..Default::default() }).await.unwrap();
    h.until("the command", |e| matches!(e, ProviderEvent::Item(i) if matches!(&i.body, NodeBody::Tool(t) if t.kind == ToolKind::Command))).await;
    h.session.interrupt().await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Interrupted);
    let (events, log) = h.finish().await;
    golden("turn_interrupt_mid_tool", &events);
    // The PTY the command ran in is terminated, and the command ends interrupted.
    assert_eq!(sent(&log, "thread/backgroundTerminals/terminate")[0]["processId"], "87302");
    let command = latest(&events).into_values().find(|i| matches!(&i.body, NodeBody::Tool(t) if t.name == "commandExecution")).unwrap();
    assert_eq!(command.status, NodeStatus::Interrupted);
    let NodeBody::Tool(tool) = &command.body else { unreachable!() };
    assert!(tool.title.starts_with("node -e"), "{}", tool.title);
}

#[tokio::test]
async fn command_approval() {
    let mut h = Harness::open_with("tool_call_read_only_on_request", |c| c.runtime_mode = RuntimeMode::AutoEdit).await;
    h.session.send(TurnInput { text: "write a file".into(), ..Default::default() }).await.unwrap();
    let (id, item, body) = h.request().await;
    assert_eq!(id, "0");
    assert_eq!(item.as_deref(), Some("exec-02c3fc6e-7a54-4c15-afda-bc59a03b138a"));
    let RequestBody::Approval(approval) = body else { panic!("{body:?}") };
    assert_eq!(approval.action, ApprovalAction::Command);
    assert!(approval.title.starts_with("Run `printf"), "{}", approval.title);
    assert!(approval.reason.as_deref().unwrap().starts_with("May I create"));
    assert_eq!(approval.decisions, [Decision::Accept, Decision::AcceptForSession, Decision::Decline, Decision::Cancel]);
    // The command waits on the approval.
    assert!(matches!(h.events.iter().rev().nth(1), Some(ProviderEvent::Item(i)) if i.status == NodeStatus::Waiting));
    h.session.respond(&id, &Response::Decision { decision: Decision::Accept, message: None }).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let (events, log) = h.finish().await;
    golden("tool_call_read_only_on_request", &events);
    let turn = sent(&log, "turn/start")[0];
    assert_eq!(turn["approvalPolicy"], "on-request");
    assert_eq!(turn["sandboxPolicy"], json!({ "type": "workspaceWrite" }));
}

#[tokio::test]
async fn file_change_approval() {
    let mut h = Harness::open("tool_call_restricted_granular").await;
    h.session.send(TurnInput { text: "write a file".into(), ..Default::default() }).await.unwrap();
    let (id, _, body) = h.request().await;
    let RequestBody::Approval(approval) = body else { panic!("{body:?}") };
    assert_eq!(approval.action, ApprovalAction::FileChange);
    assert_eq!(approval.files.len(), 1);
    assert_eq!(approval.files[0].change, ChangeKind::Add);
    assert_eq!(approval.files[0].diff.as_deref(), Some("@@ -0,0 +1,1 @@\n+codex app-server approval fixture.\n"));
    h.session.respond(&id, &Response::Decision { decision: Decision::Accept, message: None }).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("tool_call_restricted_granular", &events);
}

#[tokio::test]
async fn tool_call_without_approval() {
    let mut h = Harness::open("tool_call_workspace_never").await;
    assert_eq!(h.turn("write a file").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("tool_call_workspace_never", &events);
    assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Request { .. })));
    let tool = latest(&events).into_values().find_map(|i| if let NodeBody::Tool(t) = i.body { Some((i.status, t)) } else { None }).unwrap();
    assert_eq!(tool.0, NodeStatus::Completed);
    assert_eq!(tool.1.exit_code, Some(0));
}

#[tokio::test]
async fn todo_list() {
    let mut h = Harness::open("todo_list").await;
    assert_eq!(h.turn("track three steps").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("todo_list", &events);
    let todos = latest(&events).into_values().find(|i| matches!(i.body, NodeBody::Todos { .. })).unwrap();
    assert!(todos.id.starts_with("turn-plan:"));
    assert_eq!(todos.status, NodeStatus::Completed);
    let NodeBody::Todos { items } = &todos.body else { unreachable!() };
    assert_eq!(items.len(), 3);
}

#[tokio::test]
async fn proposed_plan() {
    let mut h = Harness::open_with("proposed_plan", |c| c.interaction_mode = InteractionMode::Plan).await;
    assert_eq!(h.turn("make a plan").await, TurnEnd::Completed);
    let (events, log) = h.finish().await;
    golden("proposed_plan", &events);
    let plan = latest(&events).into_values().find(|i| matches!(i.body, NodeBody::Plan { .. })).unwrap();
    let NodeBody::Plan { text } = &plan.body else { unreachable!() };
    assert!(text.starts_with("1. **Find the replay boundary.**"), "{text}");
    assert_eq!(sent(&log, "turn/start")[0]["collaborationMode"]["mode"], "plan");
}

#[tokio::test]
async fn plan_questions() {
    let mut h = Harness::open_with("plan_questions", |c| c.interaction_mode = InteractionMode::Plan).await;
    h.session.send(TurnInput { text: "ask me".into(), ..Default::default() }).await.unwrap();
    let (id, _, body) = h.request().await;
    let RequestBody::Questions { questions } = body else { panic!("{body:?}") };
    assert_eq!(questions[0].id, "schema_preference");
    assert_eq!(questions[0].options[0].label, "Strict schemas (Recommended)");
    // Only the question Codex asked is answered (the fake checks the exact reply).
    let answers = BTreeMap::from([
        ("schema_preference".to_string(), "Strict schemas (Recommended)".to_string()),
        ("not_asked".to_string(), "x".to_string()),
    ]);
    h.session.respond(&id, &Response::Answers { answers }).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("plan_questions", &events);
}

/// Subagents' items hang off their subagent item, and their final answers are its result.
fn assert_subagents(events: &[ProviderEvent], expected: usize) {
    let latest = latest(events);
    let subagents: Vec<_> = latest.values().filter(|i| matches!(i.body, NodeBody::Subagent { .. })).collect();
    assert_eq!(subagents.len(), expected, "{subagents:#?}");
    for sub in &subagents {
        let NodeBody::Subagent { result, child, .. } = &sub.body else { unreachable!() };
        assert!(child.is_none());
        assert!(result.as_deref().is_some_and(|r| !r.is_empty()), "{sub:#?}");
        assert!(latest.values().any(|i| i.parent.as_deref() == Some(sub.id.as_str())), "no children under {}", sub.id);
    }
}

#[tokio::test]
async fn subagent_collab_spawn() {
    let mut h = Harness::open("subagent").await;
    assert_eq!(h.turn("spawn two").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("subagent", &events);
    assert_subagents(&events, 2);
    let first = latest(&events).into_values().find(|i| matches!(i.body, NodeBody::Subagent { .. })).unwrap();
    assert_eq!(first.status, NodeStatus::Completed);
    let NodeBody::Subagent { prompt, .. } = &first.body else { unreachable!() };
    assert!(prompt.starts_with("Read "), "{prompt}");
}

#[tokio::test]
async fn subagent_v2() {
    let mut h = Harness::open("subagent_v2").await;
    assert_eq!(h.turn("spawn one").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("subagent_v2", &events);
    assert_subagents(&events, 1);
    let sub = latest(&events).into_values().find(|i| matches!(i.body, NodeBody::Subagent { .. })).unwrap();
    assert_eq!(sub.status, NodeStatus::Completed);
    assert_eq!(sub.body, NodeBody::Subagent { agent: "say_hello".into(), prompt: String::new(), child: None, result: Some("Hello.".into()) });
}

#[tokio::test]
async fn subagent_v2_nested() {
    let mut h = Harness::open("subagent_v2_nested").await;
    assert_eq!(h.turn("nest").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("subagent_v2_nested", &events);
    let latest = latest(&events);
    let subs: Vec<_> = latest.values().filter(|i| matches!(i.body, NodeBody::Subagent { .. })).collect();
    // The grandchild hangs off the child.
    assert!(subs.iter().any(|s| s.parent.is_none()));
    assert!(subs.iter().any(|s| s.parent.as_ref().is_some_and(|p| subs.iter().any(|o| &o.id == p))), "{subs:#?}");
}

#[tokio::test]
async fn subagent_v2_approval() {
    let mut h = Harness::open_with("subagent_v2_approval", |c| c.runtime_mode = RuntimeMode::Ask).await;
    h.session.send(TurnInput { text: "delegate".into(), ..Default::default() }).await.unwrap();
    let (id, item, _) = h.request().await;
    let parent = latest(&h.events)[item.as_deref().unwrap()].parent.clone();
    assert!(parent.is_some_and(|p| p.contains(':')), "the subagent's command hangs off the subagent");
    h.session.respond(&id, &Response::Decision { decision: Decision::Accept, message: None }).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let (events, log) = h.finish().await;
    golden("subagent_v2_approval", &events);
    assert_eq!(sent(&log, "turn/start")[0]["approvalPolicy"], "untrusted");
    assert_eq!(sent(&log, "turn/start")[0]["sandboxPolicy"], json!({ "type": "readOnly" }));
}

#[tokio::test]
async fn thread_rollback() {
    let mut h = Harness::open("thread_rollback").await;
    assert_eq!(h.turn("first").await, TurnEnd::Completed);
    assert_eq!(h.turn("second").await, TurnEnd::Completed);
    h.session.rollback(1).await.unwrap();
    assert_eq!(h.turn("Repeat the conversation verbatim.").await, TurnEnd::Completed);
    let (events, log) = h.finish().await;
    golden("thread_rollback", &events);
    let list = sent(&log, "thread/turns/list")[0];
    assert_eq!(list["limit"], 1);
    assert_eq!(list["sortDirection"], "desc");
    assert_eq!(sent(&log, "thread/revert")[0]["beforeTurnId"], "01a0d5ef-ac4e-7471-a4a8-6776e39e6d29");
}

#[tokio::test]
async fn provider_thread_resume() {
    let mut h = Harness::open("provider_thread_resume.1").await;
    assert_eq!(h.turn("first").await, TurnEnd::Completed);
    let (first, _) = h.finish().await;
    let ProviderEvent::NativeThread { id } = &first[0] else { panic!("{first:?}") };

    let resume = fixtures().join("provider_thread_resume.2.ndjson").display().to_string();
    let id2 = id.clone();
    let mut h = Harness::open_with("provider_thread_resume.1", move |c| {
        c.resume = Some(id2);
        c.model = Some("gpt-6-luna".into());
        c.env = vec![(TRANSCRIPT.into(), resume)];
    })
    .await;
    assert_eq!(h.turn("repeat").await, TurnEnd::Completed);
    let (second, log) = h.finish().await;
    golden("provider_thread_resume.1", &first);
    golden("provider_thread_resume.2", &second);
    assert_eq!(second[0], ProviderEvent::NativeThread { id: id.clone() });
    let params = sent(&log, "thread/resume")[0];
    assert_eq!(params["threadId"], json!(id));
    assert_eq!(params["excludeTurns"], true);
    assert_eq!(params["model"], "gpt-6-luna");
    assert_eq!(sent(&log, "turn/start")[0]["model"], "gpt-6-luna");
}

#[tokio::test]
async fn command_output_streaming() {
    let mut h = Harness::open("command_output_streaming").await;
    assert_eq!(h.turn("stream").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("command_output_streaming", &events);
    let streamed: String =
        events.iter().filter_map(|e| if let ProviderEvent::OutputDelta { text, .. } = e { Some(text.as_str()) } else { None }).collect();
    assert!(streamed.contains("stream line 4"), "{streamed:?}");
    let command = latest(&events).into_values().find(|i| matches!(&i.body, NodeBody::Tool(t) if t.kind == ToolKind::Command)).unwrap();
    let NodeBody::Tool(tool) = &command.body else { unreachable!() };
    assert!(tool.output.as_deref().unwrap().contains("progress 100%"));
}

#[tokio::test]
async fn codex_model_reroute() {
    let mut h = Harness::open("codex_model_reroute").await;
    assert_eq!(h.turn("first").await, TurnEnd::Completed);
    assert_eq!(h.turn("second").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("codex_model_reroute", &events);
    assert!(events.contains(&ProviderEvent::Model { model: "gpt-6-astra".into() }));
}

#[tokio::test]
async fn web_search() {
    let mut h = Harness::open("web_search").await;
    assert_eq!(h.turn("search").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("web_search", &events);
    let search = latest(&events).into_values().find(|i| matches!(&i.body, NodeBody::Tool(t) if t.kind == ToolKind::Web)).unwrap();
    let NodeBody::Tool(tool) = &search.body else { unreachable!() };
    assert_eq!(tool.title, "FIFA World Cup 2026 ticket prices official");
}

// ---------------------------------------------------------------------------------------------
// Recorded from codex-cli 0.164.0
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn real_simple() {
    let mut h = Harness::open("real_simple").await;
    assert_eq!(h.turn("Reply with exactly: ok").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("real_simple", &events);
    assert_eq!(messages(&events), ["ok"]);
}

#[tokio::test]
async fn real_approval() {
    let mut h = Harness::open_with("real_approval", |c| c.runtime_mode = RuntimeMode::Ask).await;
    h.session.send(TurnInput { text: "touch".into(), ..Default::default() }).await.unwrap();
    let (id, _, body) = h.request().await;
    let RequestBody::Approval(approval) = body else { panic!("{body:?}") };
    assert_eq!(approval.title, "Run `touch roll-approval.txt`");
    assert_eq!(approval.command.as_deref(), Some("/bin/bash -lc 'touch roll-approval.txt'"));
    h.session.respond(&id, &Response::Decision { decision: Decision::Accept, message: None }).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("real_approval", &events);
    assert_eq!(messages(&events), ["I’ll run the command now.", "done"]);
}

#[tokio::test]
async fn info_reads_version_account_and_models() {
    let dir = tempfile::tempdir().unwrap();
    let codex = Codex::new(fake_config(&fixtures().join("real_info.ndjson"), &dir.path().join("log.ndjson")));
    let info = codex.info().await;
    assert!(info.installed);
    assert_eq!(info.version.as_deref(), Some("0.164.0"));
    // This machine's Codex runs a configured provider: no OpenAI login needed.
    assert_eq!(info.auth, AuthStatus::Ready);
    assert_eq!(info.capabilities, codex.capabilities());
    let default = info.models.iter().find(|m| m.default).unwrap();
    assert_eq!(default.id, "gpt-6.1-sol");
    assert_eq!(default.name, "GPT-6.1-Sol");
    assert_eq!(default.efforts, ["low", "medium", "high", "xhigh", "max", "ultra"]);
    // Only reads: no thread, no turn, no login.
    let log = read_log(&dir.path().join("log.ndjson"));
    let methods: Vec<_> = log.iter().filter_map(|f| f["method"].as_str()).collect();
    assert_eq!(methods, ["initialize", "initialized", "account/read", "model/list"]);
    assert_eq!(sent(&log, "account/read")[0], &json!({}));
}

#[tokio::test]
async fn info_without_codex() {
    let codex = Codex::new(ProviderConfig { binary: Some("/nonexistent/codex".into()), ..Default::default() });
    let info = codex.info().await;
    assert!(!info.installed);
    assert_eq!(info.auth, AuthStatus::Unknown);
    assert!(info.models.is_empty());
}

#[tokio::test]
async fn info_needs_login() {
    let dir = tempfile::tempdir().unwrap();
    let transcript = synthetic(dir.path(), "login", &[
        expect(json!({ "id": 1, "method": "initialize" })),
        emit(json!({ "id": 1, "result": { "userAgent": "roll/0.164.0" } })),
        expect(json!({ "method": "initialized" })),
        expect(json!({ "id": 2, "method": "account/read" })),
        emit(json!({ "id": 2, "result": { "account": null, "requiresOpenaiAuth": true } })),
        exit_ok(),
    ]);
    let codex = Codex::new(fake_config(Path::new(&transcript), &dir.path().join("log.ndjson")));
    let info = codex.info().await;
    assert_eq!(info.auth, AuthStatus::Missing);
    assert_eq!(info.detail.as_deref(), Some("Run `codex login`"));
}

// ---------------------------------------------------------------------------------------------
// Protocol details and robustness
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn accept_for_session_answers_later_requests() {
    let dir = tempfile::tempdir().unwrap();
    let approval = |id: u64, item: &str| {
        emit(json!({ "method": "item/commandExecution/requestApproval", "id": id, "params": {
            "threadId": "thr", "turnId": "t1", "itemId": item, "command": "/bin/bash -lc 'rm x'", "cwd": "/w", "startedAtMs": 1, "kind": "command",
        } }))
    };
    let mut entries = opening();
    entries.extend([
        approval(0, "c1"),
        expect(json!({ "id": 0, "result": { "decision": "acceptForSession" } })),
        // Answered by the allowance; never surfaced.
        approval(1, "c2"),
        expect(json!({ "id": 1, "result": { "decision": "acceptForSession" } })),
        turn_completed("completed"),
        exit_ok(),
    ]);
    let mut h = Harness::open(&synthetic(dir.path(), "allow", &entries)).await;
    h.session.send(TurnInput { text: "go".into(), ..Default::default() }).await.unwrap();
    let (id, _, _) = h.request().await;
    h.session.respond(&id, &Response::Decision { decision: Decision::AcceptForSession, message: None }).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    assert_eq!(events.iter().filter(|e| matches!(e, ProviderEvent::Request { .. })).count(), 1);
}

#[tokio::test]
async fn withdrawn_and_unanswered_requests_are_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = opening();
    entries.extend([
        emit(json!({ "method": "item/fileChange/requestApproval", "id": 7, "params": { "threadId": "thr", "turnId": "t1", "itemId": "f1", "grantRoot": "/tmp" } })),
        emit(json!({ "method": "serverRequest/resolved", "params": { "threadId": "thr", "requestId": 7 } })),
        emit(json!({ "method": "item/permissions/requestApproval", "id": 8, "params": {
            "threadId": "thr", "turnId": "t1", "itemId": "c1", "cwd": "/w", "reason": "needs network", "permissions": { "network": { "enabled": true }, "fileSystem": null },
        } })),
        // An unknown server request is refused, not left hanging.
        emit(json!({ "method": "item/tool/call", "id": 9, "params": {} })),
        expect(json!({ "id": 9, "error": { "code": -32601 } })),
        turn_completed("interrupted"),
        exit_ok(),
    ]);
    let mut h = Harness::open(&synthetic(dir.path(), "withdrawn", &entries)).await;
    h.session.send(TurnInput { text: "go".into(), ..Default::default() }).await.unwrap();
    let (first, _, body) = h.request().await;
    let RequestBody::Approval(a) = body else { panic!() };
    assert_eq!(a.reason.as_deref(), Some("Allow writes under /tmp"));
    h.until("withdrawal", |e| *e == ProviderEvent::RequestCancelled { id: first.clone() }).await;
    let (second, _, body) = h.request().await;
    let RequestBody::Approval(a) = body else { panic!() };
    assert_eq!(a.action, ApprovalAction::Permission);
    assert_eq!(a.title, "Allow network access");
    assert_eq!(a.decisions, [Decision::Accept, Decision::AcceptForSession, Decision::Decline]);
    assert_eq!(h.turn_end().await, TurnEnd::Interrupted);
    // The turn ended unanswered: the request is cancelled before the turn ends.
    assert!(h.events.contains(&ProviderEvent::RequestCancelled { id: second.clone() }));
    assert!(h.session.respond(&second, &Response::Decision { decision: Decision::Accept, message: None }).await.is_err());
    h.finish().await;
}

#[tokio::test]
async fn permission_and_elicitation_replies() {
    let dir = tempfile::tempdir().unwrap();
    let permissions = json!({ "network": null, "fileSystem": { "read": null, "write": ["/data"] } });
    let mut entries = opening();
    entries.extend([
        emit(json!({ "method": "item/permissions/requestApproval", "id": 0, "params": {
            "threadId": "thr", "turnId": "t1", "itemId": "c1", "cwd": "/w", "reason": null, "permissions": permissions,
        } })),
        expect(json!({ "id": 0, "result": { "permissions": permissions, "scope": "session" } })),
        emit(json!({ "method": "mcpServer/elicitation/request", "id": 1, "params": {
            "threadId": "thr", "turnId": "t1", "serverName": "github", "mode": "form", "message": "Allow ChatGPT to use GitHub?",
            "requestedSchema": { "type": "object", "properties": { "choice": { "type": "string", "enum": ["allow_once", "deny"] } }, "required": ["choice"] },
            "_meta": null,
        } })),
        expect(json!({ "id": 1, "result": { "action": "accept", "content": { "choice": "allow_once" }, "_meta": null } })),
        // URL elicitations are declined without asking (T3).
        emit(json!({ "method": "mcpServer/elicitation/request", "id": 2, "params": {
            "threadId": "thr", "turnId": "t1", "serverName": "x", "mode": "url", "message": "log in", "url": "https://x", "elicitationId": "e", "_meta": null,
        } })),
        expect(json!({ "id": 2, "result": { "action": "decline", "content": null, "_meta": null } })),
        turn_completed("completed"),
        exit_ok(),
    ]);
    let mut h = Harness::open(&synthetic(dir.path(), "replies", &entries)).await;
    h.session.send(TurnInput { text: "go".into(), ..Default::default() }).await.unwrap();
    let (id, _, body) = h.request().await;
    let RequestBody::Approval(a) = body else { panic!() };
    assert_eq!(a.title, "Allow writing outside the workspace");
    h.session.respond(&id, &Response::Decision { decision: Decision::AcceptForSession, message: None }).await.unwrap();
    let (id, _, body) = h.request().await;
    let RequestBody::Approval(a) = body else { panic!() };
    assert_eq!((a.action, a.tool.as_deref()), (ApprovalAction::Tool, Some("github")));
    assert_eq!(a.decisions, [Decision::Accept, Decision::Decline, Decision::Cancel]);
    h.session.respond(&id, &Response::Decision { decision: Decision::Accept, message: None }).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    assert_eq!(events.iter().filter(|e| matches!(e, ProviderEvent::Request { .. })).count(), 2);
}

#[tokio::test]
async fn failed_turns_say_why() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = opening();
    entries.extend([
        emit(json!({ "method": "error", "params": { "threadId": "thr", "turnId": "t1", "willRetry": true, "error": { "message": "stream disconnected 1/5" } } })),
        emit(json!({ "method": "error", "params": { "threadId": "thr", "turnId": "t1", "willRetry": false, "error": { "message": "usage", "additionalDetails": "You've hit your usage limit." } } })),
        emit(json!({ "method": "turn/completed", "params": { "threadId": "thr", "turn": { "id": "t1", "status": "failed", "error": { "message": "usage" } } } })),
        exit_ok(),
    ]);
    let mut h = Harness::open(&synthetic(dir.path(), "failed", &entries)).await;
    assert_eq!(h.turn("go").await, TurnEnd::Failed("You've hit your usage limit.".into()));
    let (events, _) = h.finish().await;
    let retry = latest(&events).remove("retry:t1").unwrap();
    assert_eq!(retry.body, NodeBody::System { text: "Reconnecting: stream disconnected 1/5".into() });
    assert_eq!(retry.status, NodeStatus::Completed);
}

#[tokio::test]
async fn mcp_servers_instructions_and_images_reach_codex() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = opening();
    entries.extend([turn_completed("completed"), exit_ok()]);
    let mut h = Harness::open_with(&synthetic(dir.path(), "config", &entries), |c| {
        c.model = Some("gpt-x".into());
        c.effort = Some("high".into());
        c.instructions = Some("Use roll's tools.".into());
        c.mcp = vec![McpServer { name: "roll".into(), url: "http://127.0.0.1:9/mcp".into(), headers: vec![("Authorization".into(), "Bearer t".into())] }];
    })
    .await;
    let image = agent_proto::Attachment { name: "a.png".into(), mime: "image/png".into(), data: "iVBO".into() };
    h.session.send(TurnInput { text: "look".into(), attachments: vec![image] }).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let (_, log) = h.finish().await;
    let start = sent(&log, "thread/start")[0];
    assert_eq!(start["model"], "gpt-x");
    assert_eq!(
        start["config"]["mcp_servers.roll"],
        json!({ "url": "http://127.0.0.1:9/mcp", "http_headers": { "Authorization": "Bearer t" }, "tool_timeout_sec": 3900 }),
    );
    let turn = sent(&log, "turn/start")[0];
    assert_eq!(turn["input"][1], json!({ "type": "image", "url": "data:image/png;base64,iVBO" }));
    assert_eq!(turn["effort"], "high");
    assert_eq!(turn["collaborationMode"], json!({ "mode": "default", "settings": { "model": "gpt-x", "reasoning_effort": "high" } }));
    assert_eq!(turn["additionalContext"], json!({ "roll_instructions": { "kind": "application", "value": "Use roll's tools." } }));
}

#[tokio::test]
async fn compaction_restores_instructions() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = opening();
    entries.extend([
        emit(json!({ "method": "item/started", "params": { "threadId": "thr", "turnId": "t1", "item": { "type": "contextCompaction", "id": "cc" } } })),
        emit(json!({ "method": "item/completed", "params": { "threadId": "thr", "turnId": "t1", "item": { "type": "contextCompaction", "id": "cc" } } })),
        expect(json!({ "id": 4, "method": "thread/inject_items" })),
        emit(json!({ "id": 4, "result": {} })),
        turn_completed("completed"),
        exit_ok(),
    ]);
    let mut h = Harness::open_with(&synthetic(dir.path(), "compact", &entries), |c| c.instructions = Some("Be brief.".into())).await;
    assert_eq!(h.turn("go").await, TurnEnd::Completed);
    let (events, log) = h.finish().await;
    assert_eq!(latest(&events)["cc"].body, NodeBody::System { text: "Context compacted".into() });
    assert_eq!(
        sent(&log, "thread/inject_items")[0]["items"],
        json!([{ "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "<roll_instructions>Be brief.</roll_instructions>" }] }]),
    );
}

#[tokio::test]
async fn a_crash_reports_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = opening();
    entries.push(json!({ "type": "runtime_exit", "status": "error", "stderr": "thread 'main' panicked: boom" }));
    let mut h = Harness::open(&synthetic(dir.path(), "crash", &entries)).await;
    h.session.send(TurnInput { text: "go".into(), ..Default::default() }).await.unwrap();
    let ProviderEvent::Exited { error } = h.until("exit", |e| matches!(e, ProviderEvent::Exited { .. })).await else { unreachable!() };
    assert!(error.as_deref().is_some_and(|e| e.contains("panicked: boom")), "{error:?}");
    // Requests after the exit fail at once.
    assert!(h.session.interrupt().await.is_err() || h.session.steer(TurnInput { text: "x".into(), ..Default::default() }).await.is_err());
}

#[tokio::test]
async fn a_failed_start_reports_why() {
    let dir = tempfile::tempdir().unwrap();
    let transcript = synthetic(dir.path(), "nostart", &[
        expect(json!({ "id": 1, "method": "initialize" })),
        json!({ "type": "runtime_exit", "status": "error", "stderr": "error: unknown model provider" }),
    ]);
    let codex = Codex::new(fake_config(Path::new(&transcript), &dir.path().join("log.ndjson")));
    let err = codex.start(session_config(dir.path())).await.err().expect("start fails");
    assert!(format!("{err:#}").contains("unknown model provider"), "{err:#}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn close_kills_a_codex_that_ignores_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = opening()[..5].to_vec();
    entries.push(json!({ "type": "runtime_exit", "status": "hang" }));
    let mut h = Harness::open(&synthetic(dir.path(), "hang", &entries)).await;
    let pid: u32 = {
        let log = std::fs::read_to_string(h.dir.path().join("log.ndjson")).unwrap();
        serde_json::from_str::<Value>(log.lines().next().unwrap()).unwrap()["pid"].as_u64().unwrap() as u32
    };
    let started = Instant::now();
    h.session.close().await;
    assert!(started.elapsed() < Duration::from_secs(5), "close took {:?}", started.elapsed());
    h.until("exit", |e| matches!(e, ProviderEvent::Exited { .. })).await;
    // Killed and reaped: no process, not even a zombie.
    assert!(!Path::new(&format!("/proc/{pid}")).exists(), "codex {pid} is still there");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn dropping_a_session_kills_codex() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = opening()[..5].to_vec();
    entries.push(json!({ "type": "runtime_exit", "status": "hang" }));
    let h = Harness::open(&synthetic(dir.path(), "drop", &entries)).await;
    let log = std::fs::read_to_string(h.dir.path().join("log.ndjson")).unwrap();
    let pid = serde_json::from_str::<Value>(log.lines().next().unwrap()).unwrap()["pid"].as_u64().unwrap();
    let Harness { session, dir: _dir, codex: _codex, .. } = h;
    drop(session);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Path::new(&format!("/proc/{pid}")).exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!Path::new(&format!("/proc/{pid}")).exists(), "codex {pid} outlived its session");
}
