//! The Pi adapter against a fake `pi --mode rpc` that replays recorded JSON-lines transcripts
//! (`tests/fixtures/pi/*.ndjson`): T3 Code's Pi replay fixtures (MIT, from
//! `apps/server/src/orchestration-v2/testkit/fixtures/*/pi_transcript.ndjson`, recorded from Pi
//! 1.0.0), normalised where roll behaves differently from T3 (no session names, one idle probe
//! after a compaction, rollbacks that resume the session in a new process and fork it there; the
//! scenario tests say which), and transcripts written here for what T3's set doesn't cover.
//! Each scenario's events are compared with `tests/fixtures/pi/<name>.events` (regenerate with
//! `ROLL_FIXTURES=write cargo test -p agent-host --test pi`), and the important ones are
//! asserted outright.
//!
//! The fake is this test binary run again (`fake_pi`) behind a small `pi` script that answers
//! `--version` and `auth check` itself and hands the launch arguments over in the environment.
//! Like T3's `PiAdapterV2.testkit.ts` it checks `process_start` arguments (`<any>` matches
//! anything), rebinds the adapter's request ids, lets writes within a turn arrive out of order,
//! and serves later launches from the entries labelled `@pN` (each process claims its ordinal in
//! a state directory). It speaks on fd 3 so libtest's own output can't reach the protocol stream,
//! logs every frame it receives, and on the first frame it didn't expect it writes the reason to
//! its result file and stderr and exits 3, which the adapter reports as `Exited`. `pause`
//! entries wait for the test to say go.

use agent_host::provider::pi::{EXTENSION_FILE, EXTENSION_SOURCE, Pi};
use agent_host::provider::{
    ForkAt, ForkFrom, McpServer, Provider, ProviderConfig, ProviderEvent, RollbackStyle, Session, SessionConfig, TurnEnd, TurnInput,
};
use agent_proto::{ApprovalAction, AuthStatus, Decision, InteractionMode, NodeBody, NodeStatus, RequestBody, Response, RuntimeMode, ToolKind};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const TRANSCRIPT: &str = "ROLL_FAKE_PI";
const STATE: &str = "ROLL_FAKE_PI_STATE";
const ARGV: &str = "ROLL_FAKE_PI_ARGV";

// ---------------------------------------------------------------------------------------------
// The fake
// ---------------------------------------------------------------------------------------------

/// Not a test: the fake Pi, when the adapter runs this binary through the `pi` script.
#[test]
fn fake_pi() {
    let (Ok(transcript), Ok(state)) = (std::env::var(TRANSCRIPT), std::env::var(STATE)) else { return };
    let state = PathBuf::from(state);
    let ordinal = (1..).find(|n| std::fs::OpenOptions::new().write(true).create_new(true).open(state.join(format!("proc.{n}"))).is_ok()).unwrap();
    let env: BTreeMap<String, String> = std::env::vars().filter(|(k, _)| k.starts_with("ROLL_PI_")).collect();
    let cwd = std::env::current_dir().unwrap().display().to_string();
    std::fs::write(state.join(format!("p{ordinal}.env.json")), json!({ "env": env, "cwd": cwd }).to_string()).unwrap();
    let code = match replay(Path::new(&transcript), &state, ordinal) {
        Ok(()) => {
            std::fs::write(state.join(format!("p{ordinal}.result")), "ok").unwrap();
            0
        }
        Err(e) => {
            std::fs::write(state.join(format!("p{ordinal}.result")), &e).unwrap();
            eprintln!("fake-pi p{ordinal}: {e}");
            3
        }
    };
    std::process::exit(code);
}

fn process_of(entry: &Value) -> u32 {
    let label = entry["label"].as_str().unwrap_or("");
    label.rsplit_once("@p").and_then(|(_, n)| n.parse().ok()).unwrap_or(1)
}

fn adapter_id(v: &Value) -> Option<&str> {
    v.get("id").and_then(Value::as_str).filter(|id| id.starts_with("roll-"))
}

/// T3's `replayValueMatches`: `<any>` matches anything; objects match key by key.
fn matches(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::String(s), _) if s == "<any>" => true,
        (Value::Array(e), Value::Array(a)) => e.len() == a.len() && e.iter().zip(a).all(|(e, a)| matches(e, a)),
        (Value::Object(e), Value::Object(a)) => e.keys().chain(a.keys()).all(|k| match (e.get(k), a.get(k)) {
            (Some(e), Some(a)) => matches(e, a),
            (Some(Value::String(s)), None) => s == "<any>",
            _ => false,
        }),
        _ => expected == actual,
    }
}

/// The expected frame with the adapter's request id in place of the recorded one.
fn rebound(expected: &Value, actual: &Value) -> Value {
    let mut e = expected.clone();
    if let (Some(_), Some(id)) = (adapter_id(expected), adapter_id(actual)) {
        e["id"] = json!(id);
    }
    e
}

fn starts_turn(entry: &Value) -> bool {
    entry["type"] == "expect_outbound" && matches!(entry["frame"]["type"].as_str(), Some("prompt" | "compact" | "process_start"))
}

fn replay(transcript: &Path, state: &Path, ordinal: u32) -> Result<(), String> {
    let mut out = std::fs::OpenOptions::new().write(true).open("/dev/fd/3").map_err(|e| format!("fd 3: {e}"))?;
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(state.join(format!("p{ordinal}.log"))).unwrap();
    let text = std::fs::read_to_string(transcript).map_err(|e| format!("{}: {e}", transcript.display()))?;
    let entries: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|e| matches!(e["type"].as_str(), Some("expect_outbound" | "emit_inbound" | "runtime_exit" | "pause")) && process_of(e) == ordinal)
        .collect();
    let args: Vec<String> = std::env::var(ARGV).unwrap_or_default().split('\u{1f}').filter(|a| !a.is_empty()).map(str::to_string).collect();
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut consumed = vec![false; entries.len()];
    let mut held: Vec<Value> = Vec::new();
    let mut cursor = 0;
    let mut next: Option<Value> = Some(json!({ "type": "process_start", "args": args }));
    loop {
        if let Some(frame) = next.take() {
            // Acceptable if some pending expectation before the next turn's start matches it.
            let mut possible = false;
            for (i, e) in entries.iter().enumerate().skip(cursor) {
                if e["type"] != "expect_outbound" || consumed[i] {
                    continue;
                }
                if matches(&rebound(&e["frame"], &frame), &frame) {
                    possible = true;
                    break;
                }
                if starts_turn(e) {
                    break;
                }
            }
            if !possible {
                let expected = entries[cursor..].iter().find(|e| e["type"] == "expect_outbound").map(|e| e["frame"].to_string()).unwrap_or("<end>".into());
                return Err(format!("unexpected frame {frame}; next expected {expected}"));
            }
            held.push(frame);
        }
        // Advance: send what's due, consume held frames that are next.
        loop {
            let mut progressed = false;
            while cursor < entries.len() {
                let e = &entries[cursor];
                match e["type"].as_str() {
                    Some("emit_inbound") => {
                        let mut frame = e["frame"].clone();
                        if let Some(actual) = frame.get("id").and_then(Value::as_str).and_then(|id| ids.get(id)) {
                            frame["id"] = json!(actual);
                        }
                        writeln!(out, "{frame}").map_err(|e| e.to_string())?;
                        out.flush().map_err(|e| e.to_string())?;
                    }
                    Some("pause") => {
                        let go = state.join(format!("go.{}", e["label"].as_str().unwrap_or("")));
                        let deadline = Instant::now() + Duration::from_secs(20);
                        while !go.exists() {
                            if Instant::now() > deadline {
                                return Err(format!("nobody said go for {}", go.display()));
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                    Some("runtime_exit") => {
                        if let Some(stderr) = e["stderr"].as_str() {
                            eprintln!("{stderr}");
                        }
                        std::process::exit(1);
                    }
                    _ if consumed[cursor] => {}
                    _ => break,
                }
                cursor += 1;
                progressed = true;
            }
            let found = held.iter().enumerate().find_map(|(h, frame)| {
                entries[cursor..]
                    .iter()
                    .enumerate()
                    .take_while(|(_, e)| e["type"] == "expect_outbound")
                    .find(|(i, e)| !consumed[cursor + i] && matches(&rebound(&e["frame"], frame), frame))
                    .map(|(i, _)| (h, cursor + i))
            });
            if let Some((h, i)) = found {
                let frame = held.remove(h);
                if let (Some(recorded), Some(actual)) = (adapter_id(&entries[i]["frame"]), adapter_id(&frame)) {
                    ids.insert(recorded.to_string(), actual.to_string());
                }
                consumed[i] = true;
                progressed = true;
            }
            if !progressed {
                break;
            }
        }
        let line = match lines.next() {
            Some(Ok(line)) => line,
            _ if cursor == entries.len() && held.is_empty() => return Ok(()),
            _ => {
                let expected = entries[cursor..].iter().find(|e| e["type"] == "expect_outbound").map(|e| e["label"].to_string()).unwrap_or_default();
                return Err(format!("stdin closed before the adapter sent {expected}"));
            }
        };
        writeln!(log, "{line}").unwrap();
        let frame: Value = serde_json::from_str(&line).map_err(|e| format!("not JSON: {line}: {e}"))?;
        if cursor == entries.len() && held.is_empty() {
            return Err(format!("unexpected frame after the transcript ended: {frame}"));
        }
        next = Some(frame);
    }
}

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi")
}

/// The `pi` the adapter runs: answers `--version` and `auth check`, else the fake.
fn write_script(dir: &Path) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let script = format!(
        r#"#!/bin/sh
case "$1" in
  --version) echo "pi ${{ROLL_FAKE_PI_VERSION:-1.0.0}}"; exit 0;;
  auth) printf '%s\n' "$ROLL_FAKE_PI_AUTH"; exit 0;;
esac
IFS=$(printf '\037')
{ARGV}="$*"
export {ARGV}
exec "{}" fake_pi --exact --nocapture -q 3>&1 1>/dev/null
"#,
        exe.display()
    );
    let path = dir.join("pi");
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // A test spawning meanwhile may have forked while the script was open for writing, and an
    // exec then fails with "text file busy" until that child execs: wait until it runs once.
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Err(e) = std::process::Command::new(&path).arg("--version").output() {
        assert!(e.raw_os_error() == Some(26) && Instant::now() < deadline, "running {}: {e}", path.display());
        std::thread::sleep(Duration::from_millis(5));
    }
    path
}

/// T3's transcripts name the launch arguments and the model they were recorded with.
fn metadata(transcript: &Path) -> (Vec<String>, Option<String>) {
    let first = std::fs::read_to_string(transcript).unwrap().lines().next().map(|l| serde_json::from_str::<Value>(l).unwrap()).unwrap_or_default();
    let args = first.pointer("/metadata/launchArgs").and_then(Value::as_str).unwrap_or("").split_whitespace().map(str::to_string).collect();
    (args, first.pointer("/metadata/model").and_then(Value::as_str).map(str::to_string))
}

const MODEL: &str = "openrouter/deepseek/deepseek-v4-flash";

struct Harness {
    dir: tempfile::TempDir,
    pi: Pi,
    /// The model the transcript was recorded with (T3 asks for it with `set_model`).
    model: Option<String>,
    session: Option<Box<dyn Session>>,
    rx: Option<mpsc::Receiver<ProviderEvent>>,
    events: Vec<ProviderEvent>,
}

impl Harness {
    /// A Pi whose `pi` replays `transcript` (a fixture name or a path), with `env` for the
    /// script.
    fn new(transcript: &str, env: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = match transcript.starts_with('/') {
            true => PathBuf::from(transcript),
            false => fixtures().join(format!("{transcript}.ndjson")),
        };
        std::fs::create_dir_all(dir.path().join("state")).unwrap();
        std::fs::create_dir_all(dir.path().join("work")).unwrap();
        let mut vars = vec![(TRANSCRIPT.to_string(), path.display().to_string()), (STATE.to_string(), dir.path().join("state").display().to_string())];
        vars.extend(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let (args, model) = metadata(&path);
        let cfg = ProviderConfig { binary: Some(write_script(dir.path())), args, env: vars, ..Default::default() };
        let pi = Pi::new(cfg, dir.path().join("pi-data"));
        Self { dir, pi, model, session: None, rx: None, events: Vec::new() }
    }

    fn config(&self) -> SessionConfig {
        SessionConfig {
            cwd: self.dir.path().join("work"),
            model: self.model.clone(),
            effort: None,
            options: Default::default(),
            runtime_mode: RuntimeMode::FullAccess,
            interaction_mode: InteractionMode::Default,
            resume: None,
            resume_at: None,
            fork: None,
            env: Vec::new(),
            mcp: Vec::new(),
            instructions: None,
        }
    }

    async fn open(transcript: &str) -> Self {
        let mut h = Self::new(transcript, &[]);
        h.start(|_| {}).await;
        h
    }

    async fn start(&mut self, tweak: impl FnOnce(&mut SessionConfig)) {
        let mut cfg = self.config();
        tweak(&mut cfg);
        let (session, rx) = self.pi.start(cfg).await.expect("start");
        self.session = Some(session);
        self.rx = Some(rx);
    }

    fn session(&self) -> &dyn Session {
        self.session.as_deref().expect("a session")
    }

    /// Collect events until one matches.
    async fn until(&mut self, what: &str, pred: impl Fn(&ProviderEvent) -> bool) -> ProviderEvent {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let rx = self.rx.as_mut().expect("a session");
            let event = tokio::time::timeout_at(deadline, rx.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}; got {:#?}", self.events))
                .unwrap_or_else(|| panic!("events ended waiting for {what}"));
            self.events.push(event.clone());
            if pred(&event) {
                return event;
            }
            if let ProviderEvent::Exited { error } = &event {
                panic!("pi exited waiting for {what}: {error:?}");
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
        self.session().send(text_input(text)).await.expect("send");
        self.turn_end().await
    }

    async fn request(&mut self) -> (String, RequestBody) {
        match self.until("a request", |e| matches!(e, ProviderEvent::Request { .. })).await {
            ProviderEvent::Request { id, body, .. } => (id, body),
            _ => unreachable!(),
        }
    }

    /// Close the session and wait for its `Exited`.
    async fn close(&mut self) {
        self.session().close().await;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(20), self.rx.as_mut().unwrap().recv()).await.expect("exit").expect("exit");
            self.events.push(event.clone());
            if matches!(event, ProviderEvent::Exited { .. }) {
                break;
            }
        }
        self.session = None;
        self.rx = None;
    }

    fn go(&self, label: &str) {
        std::fs::write(self.dir.path().join("state").join(format!("go.{label}")), "").unwrap();
    }

    /// Every frame process `n` received.
    fn log(&self, n: u32) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("state").join(format!("p{n}.log")))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn env(&self, n: u32) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.dir.path().join("state").join(format!("p{n}.env.json"))).unwrap()).unwrap()
    }

    /// Close any session, check that `processes` fake processes each replayed their whole part
    /// of the transcript, and return every event.
    async fn finish(mut self, processes: u32) -> Vec<ProviderEvent> {
        if self.session.is_some() {
            self.close().await;
        }
        for n in 1..=processes {
            let path = self.dir.path().join("state").join(format!("p{n}.result"));
            let deadline = Instant::now() + Duration::from_secs(10);
            while !path.exists() && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let result = std::fs::read_to_string(&path).unwrap_or_else(|_| format!("process {n} never finished"));
            assert_eq!(result, "ok", "fake pi process {n}");
        }
        assert!(!self.dir.path().join("state").join(format!("proc.{}", processes + 1)).exists(), "more than {processes} pi processes started");
        self.events
    }
}

fn text_input(text: &str) -> TurnInput {
    TurnInput { text: text.into(), attachments: Vec::new() }
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
        ProviderEvent::Context(c) => json!({ "context": c }),
        ProviderEvent::TurnRef { id } => json!({ "turn_ref": id }),
        ProviderEvent::TurnEnded(TurnEnd::Completed) => json!({ "turn_ended": "completed" }),
        ProviderEvent::TurnEnded(TurnEnd::Interrupted) => json!({ "turn_ended": "interrupted" }),
        ProviderEvent::TurnEnded(TurnEnd::Failed(m)) => json!({ "turn_ended": { "failed": m } }),
        ProviderEvent::Exited { error } => json!({ "exited": error }),
    }
}

/// Compare a scenario's events with its golden file.
fn golden(name: &str, events: &[ProviderEvent]) {
    golden_in(name, events, None);
}

/// `golden`, with the session's directory (a temporary one) written as `<cwd>`.
fn golden_in(name: &str, events: &[ProviderEvent], cwd: Option<&Path>) {
    let mut actual: String = events.iter().map(|e| format!("{}\n", event_json(e))).collect();
    if let Some(cwd) = cwd {
        actual = actual.replace(&cwd.display().to_string(), "<cwd>");
    }
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

/// The text of each completed message, in order.
fn messages(events: &[ProviderEvent]) -> Vec<String> {
    items(events)
        .into_iter()
        .filter(|i| i.status == NodeStatus::Completed)
        .filter_map(|i| if let NodeBody::Message { text } = &i.body { Some(text.clone()) } else { None })
        .collect()
}

fn refs(events: &[ProviderEvent]) -> Vec<&str> {
    events.iter().filter_map(|e| if let ProviderEvent::TurnRef { id } = e { Some(id.as_str()) } else { None }).collect()
}

fn threads(events: &[ProviderEvent]) -> Vec<&str> {
    events.iter().filter_map(|e| if let ProviderEvent::NativeThread { id } = e { Some(id.as_str()) } else { None }).collect()
}

fn sent<'a>(log: &'a [Value], kind: &str) -> Vec<&'a Value> {
    log.iter().filter(|f| f["type"] == kind).collect()
}

// ---------------------------------------------------------------------------------------------
// T3's replay scenarios
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn simple() {
    let mut h = Harness::open("simple").await;
    assert_eq!(h.turn("Respond with the following text: fixture simple ok").await, TurnEnd::Completed);
    let log = h.log(1);
    let events = h.finish(1).await;
    golden("simple", &events);
    assert_eq!(events[0], ProviderEvent::NativeThread { id: "/pi-sessions/session-1.jsonl".into() });
    assert_eq!(events[1], ProviderEvent::Model { model: MODEL.into() });
    assert_eq!(messages(&events), ["fixture simple ok"]);
    let reasoning: Vec<_> = items(&events).into_iter().filter(|i| matches!(&i.body, NodeBody::Reasoning { text } if text.contains("fixture simple ok"))).collect();
    assert_eq!(reasoning.last().map(|i| i.status), Some(NodeStatus::Completed));
    assert!(events.iter().any(|e| matches!(e, ProviderEvent::Usage(u) if u.input_tokens == 1786 && u.output_tokens == 28)));
    // The context from `get_session_stats`, then the turn's ref (its leaf), then the end.
    let n = events.len();
    assert!(matches!(&events[n - 4], ProviderEvent::Context(c) if c.used_tokens == 1814 && c.max_tokens == Some(1048576) && c.total_processed_tokens == Some(1814)));
    assert_eq!(events[n - 3], ProviderEvent::TurnRef { id: "ac4aa63e".into() });
    assert_eq!(events[n - 2], ProviderEvent::TurnEnded(TurnEnd::Completed));
    assert_eq!(events[n - 1], ProviderEvent::Exited { error: None });
    // The startup notice came outside a turn: dropped, as T3 does.
    assert!(!items(&events).iter().any(|i| matches!(i.body, NodeBody::System { .. })));
    assert_eq!(sent(&log, "prompt")[0], &json!({ "type": "prompt", "message": "Respond with the following text: fixture simple ok", "streamingBehavior": "steer" }));
    assert_eq!(sent(&log, "get_entries")[1]["since"], "670d1d31");
}

#[tokio::test]
async fn multi_turn() {
    let mut h = Harness::open("multi_turn").await;
    assert_eq!(h.turn("Respond with exactly: first fixture turn complete").await, TurnEnd::Completed);
    assert_eq!(h.turn("Respond with exactly: second fixture turn complete").await, TurnEnd::Completed);
    let events = h.finish(1).await;
    golden("multi_turn", &events);
    assert_eq!(messages(&events), ["first fixture turn complete", "second fixture turn complete"]);
    assert_eq!(refs(&events), ["5c0d3865", "00246ed4"]);
    // Item ids are per turn, so the second turn's message is a node of its own.
    let ids: Vec<&str> = items(&events).iter().filter(|i| matches!(i.body, NodeBody::Message { .. })).map(|i| i.id.as_str()).collect();
    assert!(ids.contains(&"t1:m1:c1") && ids.contains(&"t2:m1:c1"));
}

#[tokio::test]
async fn message_steering() {
    let mut h = Harness::open("message_steering").await;
    h.session().send(text_input("Respond with exactly: steering fixture initial response")).await.unwrap();
    // Steering while the run goes: the same `prompt`, which Pi queues for the running agent.
    h.session().steer(text_input("Actually, respond with exactly: steering fixture observed")).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let log = h.log(1);
    let events = h.finish(1).await;
    golden("message_steering", &events);
    assert_eq!(messages(&events), ["steering fixture initial response", "steering fixture observed"]);
    assert_eq!(sent(&log, "prompt").len(), 2);
    assert_eq!(events.iter().filter(|e| matches!(e, ProviderEvent::TurnEnded(_))).count(), 1);
}

#[tokio::test]
async fn turn_interrupt_mid_tool() {
    let mut h = Harness::open("turn_interrupt_mid_tool").await;
    h.session()
        .send(text_input("Run this exact local command: `node -e \"console.log('interrupt fixture tool started'); setTimeout(() => {}, 30000)\"`. Do not answer until it completes, then respond exactly: interrupt fixture should not finish naturally."))
        .await
        .unwrap();
    h.until("the command", |e| matches!(e, ProviderEvent::Item(i) if matches!(&i.body, NodeBody::Tool(t) if t.kind == ToolKind::Command))).await;
    h.session().interrupt().await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Interrupted);
    let log = h.log(1);
    let events = h.finish(1).await;
    golden("turn_interrupt_mid_tool", &events);
    // T3's stop: clear Pi's queue, then abort; the aborted command is interrupted, not failed.
    let order: Vec<&str> = log.iter().filter_map(|f| f["type"].as_str()).collect();
    assert_eq!(&order[order.len() - 3..], ["clear_queue", "abort", "get_entries"]);
    let tool = items(&events).into_iter().rfind(|i| i.id == "call_9726ad99cc114832996228eb").unwrap();
    assert_eq!(tool.status, NodeStatus::Interrupted);
    let NodeBody::Tool(call) = &tool.body else { panic!() };
    assert_eq!(call.title, "node -e \"console.log('interrupt fixture tool started'); setTimeout(() => {}, 30000)\"");
    assert_eq!(call.output.as_deref(), Some("Command aborted"));
    // The interrupted turn still names where it left the session (for a fork or a rollback).
    assert_eq!(refs(&events), ["206b2637"]);
}

#[tokio::test]
async fn provider_thread_resume() {
    let mut h = Harness::open("provider_thread_resume").await;
    assert_eq!(h.turn("Respond with exactly: provider thread resume fixture first turn complete").await, TurnEnd::Completed);
    h.close().await;
    // A new process resumes the session file by path.
    h.start(|cfg| cfg.resume = Some("/pi-sessions/session-1.jsonl".into())).await;
    let second = "Using the conversation history available in this resumed thread, first repeat the exact final answer you gave in the previous turn. Then on a new line write exactly: provider thread resume fixture second turn complete";
    assert_eq!(h.turn(second).await, TurnEnd::Completed);
    let log = h.log(2);
    let events = h.finish(2).await;
    golden("provider_thread_resume", &events);
    assert_eq!(threads(&events), ["/pi-sessions/session-1.jsonl", "/pi-sessions/session-1.jsonl"]);
    assert_eq!(log[0], json!({ "type": "switch_session", "sessionPath": "/pi-sessions/session-1.jsonl", "id": log[0]["id"] }));
    assert!(messages(&events)[1].contains("provider thread resume fixture second turn complete"));
}

#[tokio::test]
async fn pi_compaction() {
    let mut h = Harness::open("pi_compaction").await;
    assert_eq!(h.turn("Remember the opaque marker compaction-marker-4Z7Q for later. Respond with exactly: compaction marker stored").await, TurnEnd::Completed);
    // `/compact` with instructions is Pi's `compact` command; this one has nothing to compact.
    assert_eq!(h.turn("/compact keep the opaque marker compaction-marker-4Z7Q").await, TurnEnd::Completed);
    assert_eq!(h.turn("Respond with exactly: second compaction fixture turn").await, TurnEnd::Completed);
    assert_eq!(h.turn("Respond with exactly: third compaction fixture turn").await, TurnEnd::Completed);
    h.session().compact().await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    assert_eq!(h.turn("Return the opaque marker I asked you to remember. Respond with only the marker.").await, TurnEnd::Completed);
    let log = h.log(1);
    let events = h.finish(1).await;
    golden("pi_compaction", &events);
    let compactions: Vec<_> = items(&events).into_iter().filter(|i| matches!(i.body, NodeBody::Compaction { .. }) && i.status != NodeStatus::Running).collect();
    assert_eq!(compactions.len(), 2);
    assert_eq!(compactions[0].status, NodeStatus::Failed);
    assert!(matches!(&compactions[0].body, NodeBody::Compaction { error: Some(e), .. } if e.contains("Nothing to compact")));
    assert_eq!(compactions[1].status, NodeStatus::Completed);
    assert!(matches!(compactions[1].body, NodeBody::Compaction { before_tokens: Some(_), after_tokens: Some(_), error: None }));
    assert_eq!(sent(&log, "compact"), [&json!({ "type": "compact", "customInstructions": "keep the opaque marker compaction-marker-4Z7Q" }), &json!({ "type": "compact" })]);
    assert_eq!(messages(&events).last().map(String::as_str), Some("compaction-marker-4Z7Q"));
}

#[tokio::test]
async fn thread_rollback() {
    let mut h = Harness::open("thread_rollback").await;
    assert_eq!(h.turn("Respond with exactly: rollback fixture first turn complete").await, TurnEnd::Completed);
    assert_eq!(h.turn("Respond with exactly: rollback fixture second turn complete").await, TurnEnd::Completed);
    assert_eq!(refs(&h.events), ["c906b4e2", "76a48ea1"]);
    assert_eq!(h.pi.rollback_style(), RollbackStyle::ResumeAt);
    // Rolled back to the first turn: the orchestrator resumes the conversation at its ref.
    h.close().await;
    h.start(|cfg| {
        cfg.resume = Some("/pi-sessions/session-1.jsonl".into());
        cfg.resume_at = Some("c906b4e2".into());
    })
    .await;
    assert_eq!(h.turn("Repeat the conversation verbatim.").await, TurnEnd::Completed);
    let log = h.log(2);
    let events = h.finish(2).await;
    golden("thread_rollback", &events);
    // The fork at the second turn's user message is a new session file: the new native thread.
    assert_eq!(threads(&events), ["/pi-sessions/session-1.jsonl", "/pi-sessions/session-2.jsonl"]);
    assert_eq!(sent(&log, "fork"), [&json!({ "type": "fork", "entryId": "022fe9c1", "id": sent(&log, "fork")[0]["id"] })]);
    assert_eq!(refs(&events)[2], "0aec84a3");
}

#[tokio::test]
async fn thread_rollback_after_stop() {
    let mut h = Harness::open("thread_rollback_after_stop").await;
    assert_eq!(h.turn("Respond with exactly: rollback fixture first turn complete").await, TurnEnd::Completed);
    h.session()
        .send(text_input("Run this exact local command: `node -e \"console.log('interrupt fixture tool started'); setTimeout(() => {}, 30000)\"`. Do not answer until it completes, then respond exactly: interrupt fixture should not finish naturally."))
        .await
        .unwrap();
    h.until("the command's output", |e| matches!(e, ProviderEvent::OutputDelta { .. })).await;
    h.session().interrupt().await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Interrupted);
    // T3 restarts Pi on stop; roll restarts it on a mode or model change, or a daemon restart.
    h.close().await;
    h.start(|cfg| cfg.resume = Some("/pi-sessions/session-1.jsonl".into())).await;
    assert_eq!(h.turn("Respond with exactly: rollback fixture second turn complete").await, TurnEnd::Completed);
    assert_eq!(refs(&h.events), ["b184b5a8", "e3ab1495", "31bf1b6c"]);
    // Rolled back to the first run, across the restart: its ref still finds the cut.
    h.close().await;
    h.start(|cfg| {
        cfg.resume = Some("/pi-sessions/session-1.jsonl".into());
        cfg.resume_at = Some("b184b5a8".into());
    })
    .await;
    assert_eq!(h.turn("Repeat the conversation verbatim.").await, TurnEnd::Completed);
    let log = h.log(3);
    let events = h.finish(3).await;
    golden("thread_rollback_after_stop", &events);
    assert_eq!(sent(&log, "fork")[0]["entryId"], "ee73daae");
    assert_eq!(threads(&events).last(), Some(&"/pi-sessions/session-2.jsonl"));
}

// ---------------------------------------------------------------------------------------------
// Written here
// ---------------------------------------------------------------------------------------------

fn expect(frame: Value) -> Value {
    let label = frame["type"].as_str().unwrap_or("?").to_string();
    json!({ "type": "expect_outbound", "label": label, "frame": frame })
}

fn emit(frame: Value) -> Value {
    json!({ "type": "emit_inbound", "frame": frame })
}

fn on(process: u32, mut entry: Value) -> Value {
    let label = entry["label"].as_str().unwrap_or("?").to_string();
    entry["label"] = json!(format!("{label}@p{process}"));
    entry
}

fn response(id: &str, command: &str, data: Value) -> Value {
    emit(json!({ "type": "response", "id": id, "command": command, "success": true, "data": data }))
}

fn ack(command: &str) -> Value {
    emit(json!({ "type": "response", "command": command, "success": true, "data": { "disposition": "started" } }))
}

fn model() -> Value {
    json!({ "provider": "anthropic", "id": "claude-test", "name": "Claude Test", "reasoning": true, "input": ["text", "image"], "contextWindow": 200000 })
}

fn state(file: &str, idle: bool) -> Value {
    json!({ "model": model(), "thinkingLevel": "medium", "isStreaming": !idle, "isCompacting": false, "pendingMessageCount": 0, "sessionFile": file })
}

fn user(id: &str, parent: Option<&str>) -> Value {
    json!({ "type": "message", "id": id, "parentId": parent, "message": { "role": "user" } })
}

fn assistant(id: &str, parent: &str) -> Value {
    json!({ "type": "message", "id": id, "parentId": parent, "message": { "role": "assistant" } })
}

/// get_state, get_available_models and get_entries for an empty session `/s/1.jsonl`.
fn opening() -> Vec<Value> {
    vec![
        expect(json!({ "type": "process_start", "args": ["--mode", "rpc", "--extension", "<any>"] })),
        expect(json!({ "type": "get_state", "id": "roll-0" })),
        response("roll-0", "get_state", state("/s/1.jsonl", true)),
        expect(json!({ "type": "get_available_models", "id": "roll-1" })),
        response("roll-1", "get_available_models", json!({ "models": [model()] })),
        expect(json!({ "type": "get_entries", "id": "roll-2" })),
        response("roll-2", "get_entries", json!({ "entries": [], "leafId": null })),
    ]
}

fn prompt(text: &str) -> Value {
    expect(json!({ "type": "prompt", "message": text, "streamingBehavior": "steer" }))
}

fn reply(text: &str) -> Vec<Value> {
    vec![
        emit(json!({ "type": "message_start", "message": { "role": "assistant" } })),
        emit(json!({ "type": "message_update", "assistantMessageEvent": { "type": "text_delta", "contentIndex": 0, "delta": text } })),
        emit(json!({ "type": "message_update", "assistantMessageEvent": { "type": "text_end", "contentIndex": 0, "content": text } })),
        emit(json!({ "type": "message_end", "message": { "role": "assistant", "stopReason": "stop", "usage": { "input": 10, "output": 2, "cacheRead": 0, "cost": { "total": 0.001 } } } })),
    ]
}

/// agent_settled, then the idle probe, the tree since `since` (leaf `leaf`) and the stats.
fn settle(n: u32, since: Option<&str>, entries: Value, leaf: &str) -> Vec<Value> {
    let mut get_entries = json!({ "type": "get_entries", "id": format!("roll-e{n}") });
    if let Some(since) = since {
        get_entries["since"] = json!(since);
    }
    vec![
        emit(json!({ "type": "agent_settled" })),
        expect(json!({ "type": "get_state", "id": format!("roll-s{n}") })),
        response(&format!("roll-s{n}"), "get_state", state("/s/1.jsonl", true)),
        expect(get_entries),
        response(&format!("roll-e{n}"), "get_entries", json!({ "entries": entries, "leafId": leaf })),
        expect(json!({ "type": "get_session_stats", "id": format!("roll-t{n}") })),
        response(&format!("roll-t{n}"), "get_session_stats", json!({ "tokens": { "total": 120 }, "contextUsage": { "tokens": 100, "contextWindow": 200000 } })),
    ]
}

/// A transcript written for one test.
fn synthetic(dir: &Path, name: &str, entries: &[Value]) -> String {
    let path = dir.join(format!("{name}.ndjson"));
    let text: String = entries.iter().map(|e| format!("{e}\n")).collect();
    std::fs::write(&path, text).unwrap();
    path.display().to_string()
}

fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn ui(id: &str, method: &str, extra: Value) -> Value {
    let mut frame = json!({ "type": "extension_ui_request", "id": id, "method": method });
    frame.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    emit(frame)
}

fn ui_response(fields: Value) -> Value {
    let mut frame = json!({ "type": "extension_ui_response" });
    frame.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
    expect(frame)
}

#[tokio::test]
async fn approvals_from_the_extension() {
    let t = scratch();
    let bash = "{\n  \"command\": \"make test\"\n}";
    let write = "{\n  \"path\": \"a.txt\",\n  \"content\": \"x\"\n}";
    let mut entries = opening();
    entries.extend([
        prompt("do it"),
        ack("prompt"),
        emit(json!({ "type": "agent_start" })),
        ui("u1", "confirm", json!({ "title": "Allow bash?", "message": bash })),
        ui_response(json!({ "id": "u1", "confirmed": true })),
        emit(json!({ "type": "tool_execution_start", "toolCallId": "c1", "toolName": "bash", "args": { "command": "make test" } })),
        emit(json!({ "type": "tool_execution_end", "toolCallId": "c1", "toolName": "bash", "result": { "content": [{ "type": "text", "text": "ok" }], "details": { "exitCode": 0 } }, "isError": false })),
        ui("u2", "confirm", json!({ "title": "Allow write?", "message": write })),
        ui_response(json!({ "id": "u2", "confirmed": true })),
        // Accepted for the session: the same ask is answered without asking.
        ui("u3", "confirm", json!({ "title": "Allow write?", "message": write })),
        ui_response(json!({ "id": "u3", "confirmed": true })),
        ui("u4", "confirm", json!({ "title": "Allow edit?", "message": "{\"path\": \"b.txt\"}" })),
        ui_response(json!({ "id": "u4", "confirmed": false })),
        ui("u5", "confirm", json!({ "title": "Allow mcp__roll__delegate_task?", "message": "{\"prompt\": \"x\"}" })),
        ui_response(json!({ "id": "u5", "cancelled": true })),
        // Nobody answers in time: withdrawn, and Pi hears it was cancelled.
        ui("u6", "confirm", json!({ "title": "Allow bash?", "message": "{\"command\": \"rm -rf /\"}", "timeout": 50 })),
        ui_response(json!({ "id": "u6", "cancelled": true })),
        ui("n1", "notify", json!({ "message": "heads up", "notifyType": "info" })),
    ]);
    entries.extend(reply("done"));
    entries.extend(settle(1, None, json!([user("u", None), assistant("a", "u")]), "a"));
    let path = synthetic(t.path(), "approvals", &entries);
    let mut h = Harness::new(&path, &[]);
    h.start(|cfg| cfg.runtime_mode = RuntimeMode::Ask).await;
    h.session().send(text_input("do it")).await.unwrap();
    let decide = |decision| Response::Decision { decision, message: None };
    let (id, body) = h.request().await;
    let RequestBody::Approval(a) = body else { panic!("{body:?}") };
    assert_eq!((id.as_str(), a.action, a.title.as_str(), a.command.as_deref()), ("u1", ApprovalAction::Command, "Run `make test`", Some("make test")));
    h.session().respond(&id, &decide(Decision::Accept)).await.unwrap();
    let (id, body) = h.request().await;
    let RequestBody::Approval(a) = body else { panic!() };
    assert_eq!((id.as_str(), a.action, a.files[0].path.as_str()), ("u2", ApprovalAction::FileChange, "a.txt"));
    h.session().respond(&id, &decide(Decision::AcceptForSession)).await.unwrap();
    let (id, _) = h.request().await;
    assert_eq!(id, "u4");
    h.session().respond(&id, &decide(Decision::Decline)).await.unwrap();
    let (id, body) = h.request().await;
    assert!(matches!(&body, RequestBody::Approval(a) if a.action == ApprovalAction::Tool && a.tool.as_deref() == Some("mcp__roll__delegate_task")));
    h.session().respond(&id, &decide(Decision::Cancel)).await.unwrap();
    let (id, _) = h.request().await;
    assert_eq!(id, "u6");
    h.until("the expiry", |e| matches!(e, ProviderEvent::RequestCancelled { id } if id == "u6")).await;
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    assert!(h.session().respond("u6", &decide(Decision::Accept)).await.is_err());
    let cwd = h.config().cwd;
    let events = h.finish(1).await;
    golden_in("approvals", &events, Some(&cwd));
    assert!(items(&events).iter().any(|i| matches!(&i.body, NodeBody::System { text } if text == "heads up")));
    assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Request { id, .. } if id == "u3")));
}

#[tokio::test]
async fn questions_from_extensions() {
    let t = scratch();
    let mut entries = opening();
    entries.extend([
        prompt("pick"),
        ack("prompt"),
        emit(json!({ "type": "agent_start" })),
        ui("q1", "select", json!({ "title": "Deploy where?", "options": ["", "staging", "production"] })),
        ui_response(json!({ "id": "q1", "value": "" })),
        ui("q2", "input", json!({ "title": "Release name", "placeholder": "v1.2" })),
        ui_response(json!({ "id": "q2", "value": "v1.3" })),
        ui("q3", "editor", json!({ "title": "Notes", "prefill": "x" })),
        emit(json!({ "type": "agent_settled" })),
        expect(json!({ "type": "get_state", "id": "roll-s" })),
        response("roll-s", "get_state", state("/s/1.jsonl", true)),
        // Still open when the turn ends: withdrawn.
        ui_response(json!({ "id": "q3", "cancelled": true })),
        expect(json!({ "type": "get_entries", "id": "roll-e" })),
        response("roll-e", "get_entries", json!({ "entries": [user("u", None)], "leafId": "u" })),
        expect(json!({ "type": "get_session_stats", "id": "roll-t" })),
        response("roll-t", "get_session_stats", json!({})),
    ]);
    let path = synthetic(t.path(), "questions", &entries);
    let mut h = Harness::new(&path, &[]);
    h.start(|_| {}).await;
    h.session().send(text_input("pick")).await.unwrap();
    let answer = |id: &str, text: &str| Response::Answers { answers: [(id.to_string(), text.to_string())].into() };
    let (id, body) = h.request().await;
    let RequestBody::Questions { questions } = body else { panic!() };
    assert_eq!((questions[0].header.as_str(), questions[0].question.as_str()), ("Question", "Deploy where?"));
    assert_eq!(questions[0].options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), ["Empty value", "staging", "production"]);
    h.session().respond(&id, &answer(&id, "Empty value")).await.unwrap();
    let (id, body) = h.request().await;
    let RequestBody::Questions { questions } = body else { panic!() };
    assert_eq!((questions[0].header.as_str(), questions[0].question.as_str()), ("Release name", "v1.2"));
    h.session().respond(&id, &answer(&id, "v1.3")).await.unwrap();
    let (id, _) = h.request().await;
    assert_eq!(id, "q3");
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let events = h.finish(1).await;
    golden("questions", &events);
    assert!(events.iter().any(|e| matches!(e, ProviderEvent::RequestCancelled { id } if id == "q3")));
}

#[tokio::test]
async fn prompts_wait_out_a_compaction() {
    let t = scratch();
    let mut entries = opening();
    entries.extend([prompt("first"), ack("prompt"), emit(json!({ "type": "agent_start" }))]);
    entries.extend(reply("one"));
    entries.extend([
        emit(json!({ "type": "compaction_start", "reason": "threshold" })),
        // The test steers here: Pi would refuse it, so it waits.
        json!({ "type": "pause", "label": "steered" }),
        emit(json!({ "type": "compaction_end", "reason": "threshold", "result": { "summary": "s", "tokensBefore": 900, "estimatedTokensAfter": 300 } })),
        expect(json!({ "type": "prompt", "message": "and then", "streamingBehavior": "steer" })),
        emit(json!({ "type": "response", "command": "prompt", "success": true, "data": { "disposition": "queued" } })),
    ]);
    entries.extend(reply("two"));
    entries.extend(settle(1, None, json!([user("u1", None), assistant("a1", "u1")]), "a1"));
    // A prompt racing a compaction Pi already started: refused, held, sent again after it.
    entries.extend([
        prompt("second"),
        emit(json!({ "type": "compaction_start", "reason": "threshold" })),
        emit(json!({ "type": "response", "command": "prompt", "success": false, "error": "Cannot submit a prompt while compaction is in progress" })),
        emit(json!({ "type": "compaction_end", "reason": "threshold", "aborted": true })),
        prompt("second"),
        ack("prompt"),
        emit(json!({ "type": "agent_start" })),
    ]);
    entries.extend(reply("three"));
    entries.extend(settle(2, Some("a1"), json!([user("u2", Some("a1")), assistant("a2", "u2")]), "a2"));
    let path = synthetic(t.path(), "held", &entries);
    let mut h = Harness::new(&path, &[]);
    h.start(|_| {}).await;
    h.session().send(text_input("first")).await.unwrap();
    h.until("the compaction", |e| matches!(e, ProviderEvent::Item(i) if matches!(i.body, NodeBody::Compaction { .. }))).await;
    h.session().steer(text_input("and then")).await.unwrap();
    // Nothing went out while Pi compacts.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(sent(&h.log(1), "prompt").len(), 1);
    h.go("steered");
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    assert_eq!(h.turn("second").await, TurnEnd::Completed);
    let log = h.log(1);
    let events = h.finish(1).await;
    golden("held_prompts", &events);
    assert_eq!(messages(&events), ["one", "two", "three"]);
    assert_eq!(sent(&log, "prompt").len(), 4);
    let compactions: Vec<NodeStatus> = items(&events).into_iter().filter(|i| matches!(i.body, NodeBody::Compaction { .. })).map(|i| i.status).collect();
    assert_eq!(compactions, [NodeStatus::Running, NodeStatus::Completed, NodeStatus::Running, NodeStatus::Interrupted]);
}

#[tokio::test]
async fn failures_end_the_turn() {
    let t = scratch();
    let mut entries = opening();
    entries.extend([
        prompt("a"),
        emit(json!({ "type": "response", "command": "prompt", "success": false, "error": "No API key found for anthropic" })),
        expect(json!({ "type": "get_entries", "id": "roll-e0" })),
        response("roll-e0", "get_entries", json!({ "entries": [], "leafId": null })),
        expect(json!({ "type": "get_session_stats", "id": "roll-t0" })),
        response("roll-t0", "get_session_stats", json!({})),
        prompt("b"),
        ack("prompt"),
        emit(json!({ "type": "agent_start" })),
        emit(json!({ "type": "message_start", "message": { "role": "assistant" } })),
        emit(json!({ "type": "message_end", "message": { "role": "assistant", "stopReason": "error", "errorMessage": "429 rate limited" } })),
    ]);
    entries.extend(settle(1, None, json!([user("u", None), assistant("a", "u")]), "a"));
    let path = synthetic(t.path(), "failures", &entries);
    let mut h = Harness::new(&path, &[]);
    h.start(|_| {}).await;
    assert_eq!(h.turn("a").await, TurnEnd::Failed("No API key found for anthropic".into()));
    assert_eq!(h.turn("b").await, TurnEnd::Failed("429 rate limited".into()));
    let events = h.finish(1).await;
    golden("failures", &events);
}

#[tokio::test]
async fn work_outside_a_turn_stops_the_session() {
    let t = scratch();
    let mut entries = opening();
    entries.push(emit(json!({ "type": "agent_start" })));
    let path = synthetic(t.path(), "unsolicited", &entries);
    let mut h = Harness::new(&path, &[]);
    h.start(|_| {}).await;
    let exited = loop {
        match h.rx.as_mut().unwrap().recv().await.expect("an exit") {
            ProviderEvent::Exited { error } => break error,
            _ => continue,
        }
    };
    assert!(exited.unwrap().contains("outside a turn"));
    h.session = None;
    h.rx = None;
    h.finish(1).await;
}

#[tokio::test]
async fn mcp_servers_and_instructions_reach_the_extension_through_the_environment_only() {
    let t = scratch();
    let path = synthetic(t.path(), "mcp", &opening());
    let mut h = Harness::new(&path, &[]);
    let secret = "secret-pi-token-1234";
    h.start(|cfg| {
        cfg.runtime_mode = RuntimeMode::AutoEdit;
        cfg.instructions = Some("Delegate with roll's tools.".into());
        cfg.mcp = vec![McpServer { name: "roll".into(), url: "http://127.0.0.1:9/mcp".into(), headers: vec![("Authorization".into(), format!("Bearer {secret}"))] }];
    })
    .await;
    h.close().await;
    let env = h.env(1);
    assert_eq!(env["env"]["ROLL_PI_RUNTIME_MODE"], "auto_edit");
    assert_eq!(env["env"]["ROLL_PI_INSTRUCTIONS"], "Delegate with roll's tools.");
    let servers: Value = serde_json::from_str(env["env"]["ROLL_PI_MCP"].as_str().unwrap()).unwrap();
    assert_eq!(servers, json!([{ "name": "roll", "url": "http://127.0.0.1:9/mcp", "headers": { "Authorization": format!("Bearer {secret}") } }]));
    assert_eq!(env["cwd"], h.dir.path().join("work").canonicalize().unwrap().display().to_string());
    // The extension is on disk, as roll embeds it, and the token is nowhere on disk.
    let data = h.dir.path().join("pi-data");
    assert_eq!(std::fs::read_to_string(data.join(EXTENSION_FILE)).unwrap(), EXTENSION_SOURCE);
    for dir in [data, h.dir.path().join("work")] {
        for entry in walk(&dir) {
            assert!(!std::fs::read_to_string(&entry).unwrap_or_default().contains(secret), "{} holds the MCP token", entry.display());
        }
    }
    h.finish(1).await;
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() { out.extend(walk(&path)) } else { out.push(path) }
    }
    out
}

#[tokio::test]
async fn a_fork_copies_the_session_up_to_the_turn() {
    let t = scratch();
    let source = json!([user("u1", None), assistant("a1", "u1"), json!({ "type": "model_change", "id": "m2", "parentId": "a1" }), user("u2", Some("m2")), assistant("a2", "u2")]);
    let entries = vec![
        // A bare, offline copy in the fork's directory, cut before the second turn.
        expect(json!({ "type": "process_start", "args": ["--mode", "rpc", "--offline", "--no-extensions", "--no-tools", "--fork", "/s/1.jsonl"] })),
        expect(json!({ "type": "get_entries", "id": "roll-0" })),
        response("roll-0", "get_entries", json!({ "entries": source, "leafId": "a2" })),
        expect(json!({ "type": "fork", "entryId": "u2", "id": "roll-1" })),
        response("roll-1", "fork", json!({ "text": "second", "cancelled": false })),
        expect(json!({ "type": "get_state", "id": "roll-2" })),
        response("roll-2", "get_state", state("/s/fork.jsonl", true)),
        // The session itself opens the copy.
        on(2, expect(json!({ "type": "process_start", "args": ["--mode", "rpc", "--extension", "<any>"] }))),
        on(2, expect(json!({ "type": "switch_session", "sessionPath": "/s/fork.jsonl", "id": "roll-0" }))),
        on(2, response("roll-0", "switch_session", json!({ "cancelled": false }))),
        on(2, expect(json!({ "type": "get_state", "id": "roll-1" }))),
        on(2, response("roll-1", "get_state", state("/s/fork.jsonl", true))),
        on(2, expect(json!({ "type": "get_available_models", "id": "roll-2" }))),
        on(2, response("roll-2", "get_available_models", json!({ "models": [model()] }))),
        on(2, expect(json!({ "type": "get_entries", "id": "roll-3" }))),
        on(2, response("roll-3", "get_entries", json!({ "entries": [user("u1", None), assistant("a1", "u1")], "leafId": "a1" }))),
        on(2, expect(json!({ "type": "set_thinking_level", "level": "high", "id": "roll-4" }))),
        on(2, response("roll-4", "set_thinking_level", json!(null))),
    ];
    let path = synthetic(t.path(), "fork", &entries);
    let mut h = Harness::new(&path, &[]);
    h.start(|cfg| {
        cfg.model = Some("default".into());
        cfg.effort = Some("high".into());
        cfg.fork = Some(ForkFrom { thread: "/s/1.jsonl".into(), at: ForkAt::Turn("a1".into()) });
    })
    .await;
    h.until("the native thread", |e| matches!(e, ProviderEvent::NativeThread { .. })).await;
    let events = h.finish(2).await;
    assert_eq!(threads(&events), ["/s/fork.jsonl"]);
    assert_eq!(events[1], ProviderEvent::Model { model: "anthropic/claude-test".into() });
    // A fork point Pi can't name fails the start, and the orchestrator hands a summary over.
    let h = Harness::new(&path, &[]);
    let mut cfg = h.config();
    cfg.fork = Some(ForkFrom { thread: "/s/1.jsonl".into(), at: ForkAt::DropLast(1) });
    let err = h.pi.start(cfg).await.err().expect("refused").to_string();
    assert!(err.contains("not by counting turns"), "{err}");
}

#[tokio::test]
async fn reserved_launch_arguments_are_refused() {
    let t = scratch();
    let path = synthetic(t.path(), "none", &[]);
    let h = Harness::new(&path, &[]);
    h.pi.configure(ProviderConfig { binary: Some(write_script(h.dir.path())), args: vec!["--session".into(), "/x".into()], ..Default::default() });
    let err = h.pi.start(h.config()).await.err().expect("refused").to_string();
    assert!(err.contains("'--session' is controlled by roll"), "{err}");
    let info = h.pi.info().await;
    assert!(info.installed && info.detail.unwrap().contains("'--session' is controlled by roll"));
    // Not installed at all.
    let missing = Pi::new(ProviderConfig { binary: Some(t.path().join("missing")), ..Default::default() }, t.path().join("data"));
    let info = missing.info().await;
    assert!(!info.installed && info.detail.unwrap().contains("not installed"));
}

// ---------------------------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------------------------

fn probe(models: Value, active: Value) -> Vec<Value> {
    let mut s = state("/s/x.jsonl", true);
    s["model"] = active;
    vec![
        expect(json!({ "type": "process_start", "args": ["--mode", "rpc", "--no-session", "--offline"] })),
        expect(json!({ "type": "get_state", "id": "roll-0" })),
        response("roll-0", "get_state", s),
        expect(json!({ "type": "get_available_models", "id": "roll-1" })),
        response("roll-1", "get_available_models", json!({ "models": models })),
    ]
}

#[tokio::test]
async fn status_lists_pis_models_with_their_thinking_levels() {
    let t = scratch();
    let models = json!([model(), { "provider": "openrouter", "id": "deepseek/v4", "reasoning": true, "thinkingLevelMap": { "minimal": null, "xhigh": "xhigh" }, "input": ["text"] }]);
    let path = synthetic(t.path(), "probe", &probe(models, model()));
    let h = Harness::new(&path, &[]);
    let info = h.pi.info().await;
    assert!(info.installed);
    assert_eq!(info.version.as_deref(), Some("1.0.0"));
    assert_eq!(info.auth, AuthStatus::Ready);
    assert_eq!(info.detail, None);
    assert_eq!(info.name, "Pi");
    let ids: Vec<&str> = info.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["default", "anthropic/claude-test", "openrouter/deepseek/v4"]);
    assert!(info.models[0].default);
    assert_eq!(info.models[1].efforts(), ["off", "minimal", "low", "medium", "high"]);
    assert_eq!(info.models[2].efforts(), ["off", "low", "medium", "high", "xhigh"]);
    assert!(!info.models[2].images);
    assert!(info.capabilities.steer && info.capabilities.fork && info.capabilities.rollback && info.capabilities.compact && !info.capabilities.mode_switch);
    h.finish(1).await;
}

#[tokio::test]
async fn status_says_how_to_log_in() {
    let t = scratch();
    let path = synthetic(t.path(), "probe", &probe(json!([]), json!(null)));
    let h = Harness::new(&path, &[]);
    let info = h.pi.info().await;
    assert_eq!(info.auth, AuthStatus::Missing);
    assert!(info.detail.unwrap().contains("/login"));
    h.finish(1).await;

    // Pi's default model's provider lists nothing: `pi auth check` says why.
    let path = synthetic(t.path(), "probe2", &probe(json!([model()]), json!({ "provider": "openai", "id": "gpt-x" })));
    let h = Harness::new(&path, &[("ROLL_FAKE_PI_VERSION", "0.84.2"), ("ROLL_FAKE_PI_AUTH", "{\"reason\":\"credentials_not_configured\"}")]);
    let info = h.pi.info().await;
    assert_eq!(info.auth, AuthStatus::Ready);
    assert!(info.detail.unwrap().contains("Pi default (openai/gpt-x) has no credentials configured for openai"));
    h.finish(1).await;

    // Too old for roll: no RPC process at all.
    let h = Harness::new(&path, &[("ROLL_FAKE_PI_VERSION", "0.80.4")]);
    let info = h.pi.info().await;
    assert!(info.installed);
    assert!(info.detail.unwrap().contains("Pi 0.80.4 is unsupported"));
    h.finish(0).await;
}
