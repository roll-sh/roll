//! The Muse Code adapter against a fake `muse serve` that replays recorded MSP transcripts
//! (`tests/fixtures/muse/*.ndjson`): T3 Code's Muse replay fixtures (MIT, from
//! `apps/server/src/orchestration-v2/testkit/fixtures/*/muse_transcript.ndjson`, protocol
//! `muse.msp-jsonl`, recorded from Muse 1.4.3–1.4.6), and transcripts written here for what they
//! don't cover. Each T3 scenario's events are compared with `tests/fixtures/muse/<name>.events`
//! (regenerate with `ROLL_FIXTURES=write cargo test -p agent-host --test muse`), and the
//! important ones are asserted outright.
//!
//! The fake is this test binary run again (`fake_muse`), replaying the way T3's harness does
//! (`apps/server/src/orchestration-v2/Adapters/MuseAdapterV2.testkit.ts`): every frame the
//! adapter writes must match a pending `expect_outbound` of the current turn (writes may come
//! out of order within it, never ahead of the next `turn/start`, `session/compact` or host
//! start); `emit_inbound` frames go out once every earlier expectation is met. A match is
//! exact, except that `<any>` matches anything and `<workspace>` any path, the ids the client
//! mints (`metadata.commandIds`) bind to roll's on first sight and are rewritten in what the
//! fake sends back, JSON-RPC request ids are rebound, `clientInfo` is the client's own, and
//! T3's `<runtime_info>` input part (its instructions) is dropped. It speaks on fd 3 so
//! libtest's own output can't reach the protocol stream, logs what it saw, and fails loudly
//! (stderr, the log, exit 3) on the first frame it didn't expect.

use agent_host::provider::muse::Muse;
use agent_host::provider::{McpServer, Provider, ProviderConfig, ProviderEvent, Session, SessionConfig, TurnEnd, TurnInput};
use agent_proto::{
    ApprovalAction, AuthStatus, Decision, InteractionMode, NodeBody, NodeStatus, RequestBody, Response, RuntimeMode, ToolKind, UsageWindowKind,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

const TRANSCRIPT: &str = "ROLL_FAKE_MUSE";
const LOG: &str = "ROLL_FAKE_MUSE_LOG";
const ARGS: &str = "ROLL_FAKE_MUSE_ARGS";
const MODEL: &str = "muse-spark-1.3-contributor";

// ---------------------------------------------------------------------------------------------
// The fake
// ---------------------------------------------------------------------------------------------

/// Not a test: the fake Muse, when the adapter runs this binary with `ROLL_FAKE_MUSE` set.
#[test]
fn fake_muse() {
    let Ok(transcript) = std::env::var(TRANSCRIPT) else { return };
    let mut log = Log(std::env::var(LOG).ok().and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok()));
    let code = match Replay::open(Path::new(&transcript), &mut log).and_then(|mut r| r.run(&mut log)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fake-muse: {e}");
            log.write(json!({ "error": e }));
            3
        }
    };
    std::process::exit(code);
}

struct Log(Option<std::fs::File>);

impl Log {
    fn write(&mut self, v: Value) {
        if let Some(f) = &mut self.0 {
            writeln!(f, "{v}").unwrap();
        }
    }
}

struct Replay {
    entries: Vec<Value>,
    /// The ids the client mints, as recorded.
    minted: HashSet<String>,
    /// Recorded minted id → the one roll used.
    bound: HashMap<String, String>,
    /// Recorded JSON-RPC request id → roll's.
    ids: HashMap<String, Value>,
    cursor: usize,
    consumed: HashSet<usize>,
    held: Vec<Value>,
    out: std::fs::File,
}

fn starts_turn(entry: &Value) -> bool {
    entry["type"] == "expect_outbound"
        && matches!(entry["frame"]["method"].as_str().or(entry["frame"]["type"].as_str()), Some("turn/start" | "session/compact" | "host_start"))
}

fn label(entry: Option<&Value>) -> String {
    entry.map(|e| e["label"].as_str().unwrap_or(e["type"].as_str().unwrap_or("?")).to_string()).unwrap_or_else(|| "<end>".into())
}

impl Replay {
    fn open(path: &Path, log: &mut Log) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut entries = Vec::new();
        let mut minted = HashSet::new();
        for (n, line) in text.lines().enumerate() {
            let entry: Value = serde_json::from_str(line).map_err(|e| format!("line {}: {e}", n + 1))?;
            if entry["type"] == "transcript_start" {
                for host in entry["metadata"]["commandIds"].as_array().into_iter().flatten() {
                    minted.extend(host.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string));
                }
            } else {
                entries.push(entry);
            }
        }
        let out = std::fs::OpenOptions::new().write(true).open("/dev/fd/3").map_err(|e| format!("fd 3: {e}"))?;
        let env = |k: &str| std::env::var(k).ok();
        log.write(json!({ "start": {
            "args": env(ARGS).unwrap_or_default().split_whitespace().collect::<Vec<_>>(),
            "cwd": std::env::current_dir().unwrap(),
            "env": { "MUSE_NO_AUTO_UPDATE": env("MUSE_NO_AUTO_UPDATE"), "META_API_KEY": env("META_API_KEY") },
        } }));
        Ok(Replay { entries, minted, bound: HashMap::new(), ids: HashMap::new(), cursor: 0, consumed: HashSet::new(), held: Vec::new(), out })
    }

    fn run(&mut self, log: &mut Log) -> Result<i32, String> {
        let args: Vec<String> = std::env::var(ARGS).unwrap_or_default().split_whitespace().map(str::to_string).collect();
        if let Some(code) = self.receive(json!({ "type": "host_start", "args": args }), log)? {
            return Ok(code);
        }
        let stdin = std::io::stdin();
        let mut lines = stdin.lock().lines();
        loop {
            if self.cursor == self.entries.len() && self.held.is_empty() {
                log.write(json!({ "complete": true }));
                // Done: anything more is a frame no one expected.
                for line in lines.by_ref() {
                    let line = line.map_err(|e| e.to_string())?;
                    if !line.trim().is_empty() {
                        log.write(json!({ "frame": serde_json::from_str::<Value>(&line).unwrap_or(Value::String(line.clone())) }));
                        return Err(format!("the adapter sent {line} after the transcript ended"));
                    }
                }
                return Ok(0);
            }
            let Some(line) = lines.next() else {
                return Err(format!("the adapter closed stdin before {} (entry {})", label(self.entries.get(self.cursor)), self.cursor + 1));
            };
            let line = line.map_err(|e| e.to_string())?;
            if line.trim().is_empty() {
                continue;
            }
            let frame: Value = serde_json::from_str(&line).map_err(|e| format!("the adapter wrote a line that isn't JSON ({e}): {line}"))?;
            log.write(json!({ "frame": frame }));
            if let Some(code) = self.receive(frame, log)? {
                return Ok(code);
            }
        }
    }

    /// T3's `MuseReplayController.receive`: a write must be possible before the next turn starts.
    fn receive(&mut self, actual: Value, log: &mut Log) -> Result<Option<i32>, String> {
        let mut possible = false;
        for i in self.cursor..self.entries.len() {
            let entry = &self.entries[i];
            let pending = entry["type"] == "expect_outbound" && !self.consumed.contains(&i);
            if pending && self.matches(&entry["frame"], &actual).is_some() {
                possible = true;
                break;
            }
            if pending && starts_turn(entry) {
                break;
            }
        }
        if !possible {
            let expected = self.entries.get(self.cursor).map(|e| e["frame"].to_string()).unwrap_or_default();
            return Err(format!("entry {} ({}): expected {expected}\n  the adapter sent {actual}", self.cursor + 1, label(self.entries.get(self.cursor))));
        }
        self.held.push(actual);
        self.advance(log)
    }

    fn advance(&mut self, log: &mut Log) -> Result<Option<i32>, String> {
        loop {
            let (mut progressed, exit) = self.drain()?;
            if exit.is_some() {
                return Ok(exit);
            }
            for k in 0..self.held.len() {
                let write = self.held[k].clone();
                if self.consume(&write, log) {
                    self.held.remove(k);
                    progressed = true;
                    break;
                }
            }
            if !progressed {
                return Ok(None);
            }
        }
    }

    /// Match `write` to a pending expectation of the run at the cursor, and bind its ids.
    fn consume(&mut self, write: &Value, log: &mut Log) -> bool {
        for i in self.cursor..self.entries.len() {
            let entry = &self.entries[i];
            if entry["type"] != "expect_outbound" {
                return false;
            }
            if self.consumed.contains(&i) {
                continue;
            }
            let Some(binds) = self.matches(&entry["frame"], write) else { continue };
            for (recorded, actual) in binds {
                log.write(json!({ "bind": [recorded, actual] }));
                self.bound.insert(recorded, actual);
            }
            if entry["frame"].get("method").is_some()
                && let (Some(rec), Some(act)) = (entry["frame"].get("id"), write.get("id"))
            {
                self.ids.insert(rec.to_string(), act.clone());
            }
            self.consumed.insert(i);
            while self.consumed.contains(&self.cursor) {
                self.cursor += 1;
            }
            return true;
        }
        false
    }

    /// Send inbound frames up to the next expectation.
    fn drain(&mut self) -> Result<(bool, Option<i32>), String> {
        let mut emitted = false;
        while let Some(entry) = self.entries.get(self.cursor).cloned() {
            match entry["type"].as_str() {
                Some("emit_inbound") => {
                    self.cursor += 1;
                    emitted = true;
                    let mut text = entry["frame"].to_string();
                    for (recorded, actual) in &self.bound {
                        text = text.replace(recorded.as_str(), actual);
                    }
                    let mut frame: Value = serde_json::from_str(&text).unwrap();
                    // Responses carry the client's request id; server requests their own.
                    if frame.get("method").is_none()
                        && let Some(actual) = frame.get("id").and_then(|id| self.ids.get(&id.to_string()))
                    {
                        frame["id"] = actual.clone();
                    }
                    writeln!(self.out, "{frame}").map_err(|e| e.to_string())?;
                    self.out.flush().map_err(|e| e.to_string())?;
                }
                Some("runtime_exit") => {
                    self.cursor += 1;
                    if let Some(stderr) = entry["stderr"].as_str() {
                        eprintln!("{stderr}");
                    }
                    return Ok((true, Some(if entry["status"] == "error" { 1 } else { 0 })));
                }
                _ => return Ok((emitted, None)),
            }
        }
        Ok((emitted, None))
    }

    /// The ids this write binds, if it matches `expected`.
    fn matches(&self, expected: &Value, actual: &Value) -> Option<Vec<(String, String)>> {
        let mut binds = Vec::new();
        let mut expected = expected.clone();
        // A request's id is the client's own (T3's `withRequestId`).
        if expected.get("method").is_some() && expected.get("id").is_some() && actual.get("method").is_some() && actual.get("id").is_some() {
            expected["id"] = actual["id"].clone();
        }
        self.walk(&expected, actual, "", &mut binds).then_some(binds)
    }

    fn walk(&self, expected: &Value, actual: &Value, key: &str, binds: &mut Vec<(String, String)>) -> bool {
        match (expected, actual) {
            (Value::String(e), _) if e == "<any>" => true,
            (Value::String(e), Value::String(a)) if e == "<workspace>" => !a.is_empty(),
            (Value::String(e), Value::String(a)) if self.minted.contains(e) => {
                let known = self.bound.get(e).or_else(|| binds.iter().find(|(r, _)| r == e).map(|(_, a)| a));
                match known {
                    Some(k) => k == a,
                    None => {
                        let taken = self.bound.values().any(|v| v == a) || binds.iter().any(|(_, v)| v == a);
                        if !taken {
                            binds.push((e.clone(), a.clone()));
                        }
                        !taken
                    }
                }
            }
            (Value::Object(e), Value::Object(a)) => {
                let keys: HashSet<&String> = e.keys().chain(a.keys()).collect();
                keys.into_iter().all(|k| match (e.get(k), a.get(k)) {
                    // The client's own name.
                    (Some(_), Some(_)) if k == "clientInfo" => true,
                    (Some(ev), Some(av)) => self.walk(ev, av, k, binds),
                    _ => false,
                })
            }
            (Value::Array(e), Value::Array(a)) => {
                // T3's runtime instructions are T3's, not roll's.
                let e: Vec<&Value> = e.iter().filter(|p| key != "input" || !p["text"].as_str().is_some_and(|t| t.starts_with("<runtime_info>"))).collect();
                e.len() == a.len() && e.iter().zip(a).all(|(ev, av)| self.walk(ev, av, "", binds))
            }
            _ => expected == actual,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/muse")
}

/// A `ProviderConfig` whose `muse` is the fake replaying `transcript`.
fn fake_config(transcript: &Path, log: &Path) -> ProviderConfig {
    let exe = std::env::current_exe().unwrap();
    let script = r#"if [ "$1" = --version ]; then echo "Muse Code 1.4.6 (1.4.6-R5018.1)"; exit 0; fi
ROLL_FAKE_MUSE_ARGS="$*" exec "$0" fake_muse --exact --nocapture -q 3>&1 1>/dev/null"#;
    ProviderConfig {
        binary: Some("/bin/sh".into()),
        args: vec!["-c".into(), script.into(), exe.display().to_string()],
        env: vec![(TRANSCRIPT.into(), transcript.display().to_string()), (LOG.into(), log.display().to_string())],
        ..Default::default()
    }
}

fn session_config(cwd: &Path, mode: RuntimeMode) -> SessionConfig {
    SessionConfig {
        cwd: cwd.to_path_buf(),
        model: None,
        effort: None,
        options: Default::default(),
        runtime_mode: mode,
        interaction_mode: InteractionMode::Default,
        resume: None,
        resume_at: None,
        fork: None,
        env: Vec::new(),
        mcp: Vec::new(),
        instructions: None,
    }
}

/// What T3's recorder asked for: its catalog's default model and effort.
fn t3(cfg: &mut SessionConfig) {
    cfg.model = Some(MODEL.into());
    cfg.effort = Some("high".into());
}

struct Harness {
    dir: tempfile::TempDir,
    session: Box<dyn Session>,
    rx: mpsc::Receiver<ProviderEvent>,
    events: Vec<ProviderEvent>,
}

impl Harness {
    async fn open(transcript: &str, mode: RuntimeMode, tweak: impl FnOnce(&mut SessionConfig)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (session, rx) = start(&dir, transcript, mode, tweak).await.expect("start");
        Self { dir, session, rx, events: Vec::new() }
    }

    async fn t3(name: &str, mode: RuntimeMode) -> Self {
        Self::open(name, mode, t3).await
    }

    /// Collect events until one matches.
    async fn until(&mut self, what: &str, pred: impl Fn(&ProviderEvent) -> bool) -> ProviderEvent {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let event = tokio::time::timeout_at(deadline, self.rx.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}; got {:#?}\nlog: {:#?}", self.events, raw_log(&self.dir)))
                .unwrap_or_else(|| panic!("events ended waiting for {what}"));
            self.events.push(event.clone());
            if pred(&event) {
                return event;
            }
            if let ProviderEvent::Exited { error } = &event {
                panic!("muse exited waiting for {what}: {error:?}");
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
        self.session.send(text_input(text)).await.expect("send");
        self.turn_end().await
    }

    async fn request(&mut self) -> (String, Option<String>, RequestBody) {
        match self.until("a request", |e| matches!(e, ProviderEvent::Request { .. })).await {
            ProviderEvent::Request { id, item, body } => (id, item, body),
            _ => unreachable!(),
        }
    }

    /// Close the session; return every event (ending with `Exited`, minted ids put back as
    /// recorded) and every frame the adapter sent. Fails unless the transcript was played whole.
    async fn finish(mut self) -> (Vec<ProviderEvent>, Vec<Value>) {
        self.session.close().await;
        self.until("exit", |e| matches!(e, ProviderEvent::Exited { .. })).await;
        let log = raw_log(&self.dir);
        assert!(log.iter().any(|l| l.get("complete").is_some()), "the transcript wasn't played to its end: {log:#?}");
        (unbind(&self.events, &log), frames(&log))
    }
}

async fn start(dir: &tempfile::TempDir, transcript: &str, mode: RuntimeMode, tweak: impl FnOnce(&mut SessionConfig)) -> anyhow::Result<(Box<dyn Session>, mpsc::Receiver<ProviderEvent>)> {
    let path = match transcript.starts_with('/') {
        true => PathBuf::from(transcript),
        false => fixtures().join(format!("{transcript}.ndjson")),
    };
    let muse = Muse::new(fake_config(&path, &dir.path().join("log.ndjson")));
    let mut cfg = session_config(dir.path(), mode);
    tweak(&mut cfg);
    muse.start(cfg).await
}

fn text_input(text: &str) -> TurnInput {
    TurnInput { text: text.into(), attachments: Vec::new() }
}

fn raw_log(dir: &tempfile::TempDir) -> Vec<Value> {
    std::fs::read_to_string(dir.path().join("log.ndjson")).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn frames(log: &[Value]) -> Vec<Value> {
    log.iter().filter_map(|l| l.get("frame").cloned()).collect()
}

fn started(log: &[Value]) -> Value {
    log.iter().find_map(|l| l.get("start").cloned()).expect("the fake started")
}

/// Events with roll's minted ids replaced by the recorded ones, so goldens stay put.
fn unbind(events: &[ProviderEvent], log: &[Value]) -> Vec<ProviderEvent> {
    let binds: Vec<(String, String)> =
        log.iter().filter_map(|l| l.get("bind")).map(|b| (b[0].as_str().unwrap().to_string(), b[1].as_str().unwrap().to_string())).collect();
    events
        .iter()
        .map(|e| {
            let mut text = event_json(e).to_string();
            for (recorded, actual) in &binds {
                text = text.replace(actual.as_str(), recorded);
            }
            from_json(e, serde_json::from_str(&text).unwrap())
        })
        .collect()
}

fn sent<'a>(frames: &'a [Value], method: &str) -> Vec<&'a Value> {
    frames.iter().filter(|f| f["method"] == method).map(|f| &f["params"]).collect()
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
        // `checked_at` is the clock's: left out so goldens stay put.
        ProviderEvent::UsageLimits(l) => json!({ "usage_limits": { "windows": l.windows, "plan": l.plan } }),
        ProviderEvent::Exited { error } => json!({ "exited": error }),
    }
}

/// The event `v` (an `event_json` of `e` with its ids rewritten) describes.
fn from_json(e: &ProviderEvent, v: Value) -> ProviderEvent {
    let s = |v: &Value| v.as_str().unwrap().to_string();
    match e {
        ProviderEvent::NativeThread { .. } => ProviderEvent::NativeThread { id: s(&v["native_thread"]) },
        ProviderEvent::Item(_) => {
            let i = &v["item"];
            ProviderEvent::Item(agent_host::provider::Item {
                id: s(&i["id"]),
                parent: i["parent"].as_str().map(str::to_string),
                status: serde_json::from_value(i["status"].clone()).unwrap(),
                body: serde_json::from_value(i["body"].clone()).unwrap(),
            })
        }
        ProviderEvent::Request { .. } => {
            let r = &v["request"];
            ProviderEvent::Request { id: s(&r["id"]), item: r["item"].as_str().map(str::to_string), body: serde_json::from_value(r["body"].clone()).unwrap() }
        }
        ProviderEvent::RequestCancelled { .. } => ProviderEvent::RequestCancelled { id: s(&v["request_cancelled"]) },
        other => other.clone(),
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

/// Completed top-level messages, in order.
fn messages(events: &[ProviderEvent]) -> Vec<String> {
    let mut seen = Vec::new();
    let mut out: Vec<String> = Vec::new();
    for i in items(events) {
        if let NodeBody::Message { text } = &i.body
            && i.status == NodeStatus::Completed
            && i.parent.is_none()
            && !seen.contains(&i.id)
        {
            seen.push(i.id.clone());
            out.push(text.clone());
        }
    }
    out
}

fn ends(events: &[ProviderEvent]) -> Vec<&TurnEnd> {
    events.iter().filter_map(|e| if let ProviderEvent::TurnEnded(end) = e { Some(end) } else { None }).collect()
}

// Transcripts written here ---------------------------------------------------------------------

/// The ids hand-written transcripts let the client mint.
const MINTED: [&str; 8] = ["sess-1", "cmd-1", "turn-1", "turn-2", "cmd-2", "cmd-3", "cmd-4", "cmd-5"];

fn synthetic(dir: &Path, name: &str, entries: &[Value]) -> String {
    let path = dir.join(format!("{name}.ndjson"));
    let start = json!({ "type": "transcript_start", "provider": "muse", "protocol": "muse.msp-jsonl", "scenario": name, "metadata": { "commandIds": [MINTED] } });
    let text: String = std::iter::once(&start).chain(entries).map(|e| format!("{e}\n")).collect();
    std::fs::write(&path, text).unwrap();
    path.display().to_string()
}

fn expect(frame: Value) -> Value {
    let mut frame = frame;
    frame["jsonrpc"] = json!("2.0");
    json!({ "type": "expect_outbound", "label": frame["method"].as_str().unwrap_or("reply"), "frame": frame })
}

fn emit(frame: Value) -> Value {
    let mut frame = frame;
    frame["jsonrpc"] = json!("2.0");
    json!({ "type": "emit_inbound", "frame": frame })
}

fn note(method: &str, mut params: Value) -> Value {
    params["sessionId"] = json!("sess-1");
    emit(json!({ "method": method, "params": params }))
}

fn host(args: &[&str]) -> Value {
    json!({ "type": "expect_outbound", "label": "host_start", "frame": { "type": "host_start", "args": args } })
}

/// The host, `initialize`, and `session/start` (no model) in `mode`.
fn opening(mode: RuntimeMode) -> Vec<Value> {
    let full = mode == RuntimeMode::FullAccess;
    let args: &[&str] = if full { &["serve", "--trust-workspace", "--disable-sandbox"] } else { &["serve", "--trust-workspace"] };
    vec![
        host(args),
        expect(json!({ "id": 1, "method": "initialize", "params": { "clientInfo": {}, "capabilities": { "requestedCapabilities": ["sessionMcp"] } } })),
        emit(json!({ "id": 1, "result": { "serverInfo": { "name": "muse", "version": "1.4.6" }, "schema": { "version": 1 }, "grantedCapabilities": ["sessionMcp"] } })),
        expect(json!({ "method": "initialized" })),
        expect(json!({ "id": 2, "method": "session/start", "params": {
            "sessionId": "sess-1", "workspaceRoot": "<workspace>", "providerId": "meta",
            "approvalMode": if full { "allowAll" } else { "promptUnmatched" }, "permissionProfile": if full { ":unrestricted" } else { ":ask-me" }, "commandId": "cmd-1",
        } })),
        emit(json!({ "id": 2, "result": { "session": { "sessionId": "sess-1", "modelId": "muse-test" } } })),
    ]
}

/// `turn/start` as request `id` for turn `turn` with `text`, answered, and `turn/started`.
fn turn_start(id: u64, turn: &str, text: &str) -> Vec<Value> {
    vec![
        expect(json!({ "id": id, "method": "turn/start", "params": {
            "sessionId": "sess-1", "input": [{ "type": "text", "text": text }], "displayText": text,
            "workspaceRoots": ["<workspace>"], "ifBusy": "queue", "commandId": turn,
        } })),
        emit(json!({ "id": id, "result": { "commandId": turn, "status": "accepted", "turnId": turn } })),
        note("turn/started", json!({ "turnId": turn })),
    ]
}

fn item(event: &str, item: Value) -> Value {
    note(event, json!({ "item": item }))
}

fn message(id: &str, turn: &str, text: &str) -> Value {
    item("item/completed", json!({ "itemId": id, "kind": "agentMessage", "turnId": turn, "revision": 2, "status": "completed", "text": text }))
}

fn turn_completed(turn: &str, extra: Value) -> Value {
    let mut params = json!({ "turnId": turn, "terminal": "completed" });
    for (k, v) in extra.as_object().into_iter().flatten() {
        params[k] = v.clone();
    }
    note("turn/completed", params)
}

/// Muse's shell approval as it sends one (`approval/request`, then `approval/requested`).
fn shell_approval(rpc_id: u64, approval: &str, turn: &str, origin: Option<&str>) -> Vec<Value> {
    let mut params = json!({
        "approvalId": approval, "turnId": turn, "itemId": approval, "toolName": "bash", "rawArgs": "{\"command\":\"rm -rf build\"}",
        "subject": { "kind": "shell", "command": "rm -rf build" },
        "currentRequirementId": { "approvalId": approval, "sourceIndex": 0 },
        "availableChoices": [
            { "choiceId": "allow_once", "label": "Allow once", "decision": "approved", "scope": "once" },
            { "choiceId": "allow_session", "label": "Allow for this session", "decision": "approvedForSession", "scope": "session" },
            { "choiceId": "abort", "label": "Reject", "decision": "abort", "scope": "once" },
        ],
    });
    if let Some(sub) = origin {
        params["subagentOrigin"] = json!({ "subagentId": sub });
    }
    params["sessionId"] = json!("sess-1");
    vec![
        emit(json!({ "id": rpc_id, "method": "approval/request", "params": params.clone() })),
        note("approval/requested", params),
        expect(json!({ "id": rpc_id, "result": {} })),
    ]
}

fn decide(id: u64, approval: &str, choice: &str, command: &str) -> Value {
    expect(json!({ "id": id, "method": "approval/decide", "params": {
        "sessionId": "sess-1", "approvalId": approval, "requirementId": { "approvalId": approval, "sourceIndex": 0 }, "choiceId": choice, "commandId": command,
    } }))
}

fn accepted(id: u64) -> Value {
    emit(json!({ "id": id, "result": { "status": "accepted" } }))
}

// ---------------------------------------------------------------------------------------------
// T3's replay scenarios
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn simple() {
    let mut h = Harness::t3("simple", RuntimeMode::FullAccess).await;
    assert_eq!(h.turn("Respond with the following text: fixture simple ok").await, TurnEnd::Completed);
    let root = std::fs::canonicalize(h.dir.path()).unwrap();
    let (events, frames) = h.finish().await;
    golden("simple", &events);
    assert_eq!(events[0], ProviderEvent::NativeThread { id: "01a11a6a-bbb5-7000-ad0b-0c9e728406e5".into() });
    assert_eq!(events[1], ProviderEvent::Model { model: MODEL.into() });
    assert_eq!(messages(&events), ["fixture simple ok"]);
    let deltas: String = events.iter().filter_map(|e| if let ProviderEvent::TextDelta { text, .. } = e { Some(text.as_str()) } else { None }).collect();
    assert_eq!(deltas, "fixture simple ok");
    // Muse's housekeeping (the user's own message, reminder children) never shows.
    assert_eq!(items(&events).len(), 2, "{events:#?}");
    assert!(events.iter().any(|e| matches!(e, ProviderEvent::Usage(u) if u.input_tokens == 32350 && u.output_tokens == 24)));
    assert!(events.iter().any(|e| matches!(e, ProviderEvent::Context(c) if c.used_tokens == 32374 && c.max_tokens == Some(1_007_997))));
    assert_eq!(events.last(), Some(&ProviderEvent::Exited { error: None }));

    // What the adapter sent: T3's handshake and turn, roll's name and workspace.
    let init = sent(&frames, "initialize")[0];
    assert_eq!(init["clientInfo"]["name"], "roll");
    let start = sent(&frames, "session/start")[0];
    assert_eq!(start["workspaceRoot"], json!(root));
    let turn = sent(&frames, "turn/start")[0];
    assert_eq!(turn["input"], json!([{ "type": "text", "text": "Respond with the following text: fixture simple ok" }]));
    assert_eq!(turn["workspaceRoots"], json!([root]));
    assert_eq!(turn["reasoningEffort"], "high");
}

#[tokio::test]
async fn multi_turn() {
    let mut h = Harness::t3("multi_turn", RuntimeMode::FullAccess).await;
    assert_eq!(h.turn("Respond with exactly: first fixture turn complete").await, TurnEnd::Completed);
    assert_eq!(h.turn("Respond with exactly: second fixture turn complete").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("multi_turn", &events);
    assert_eq!(messages(&events), ["first fixture turn complete", "second fixture turn complete"]);
}

#[tokio::test]
async fn turn_interrupt() {
    let mut h = Harness::t3("turn_interrupt", RuntimeMode::FullAccess).await;
    h.session.send(text_input("Do not answer immediately. First run the local shell command `sleep 30`, then respond with exactly: interrupt fixture should not finish naturally.")).await.unwrap();
    h.session.interrupt().await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Interrupted);
    let (events, frames) = h.finish().await;
    golden("turn_interrupt", &events);
    assert_eq!(sent(&frames, "turn/interrupt")[0]["turnId"], sent(&frames, "turn/start")[0]["commandId"]);
}

#[tokio::test]
async fn subagent_v2() {
    let mut h = Harness::t3("subagent_v2", RuntimeMode::FullAccess).await;
    assert_eq!(h.turn("Spawn one subagent whose only task is to reply with exactly: Hello. Wait for it to finish, then reply with exactly what it said.").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("subagent_v2", &events);
    let all = latest(&events);
    let sub = &all["01a12d17-81f1-7990-b8c5-4aec1f46ac18"];
    assert_eq!(sub.status, NodeStatus::Completed);
    let NodeBody::Subagent { agent, prompt, result, .. } = &sub.body else { panic!("{sub:?}") };
    // The role comes from the spawn call.
    assert_eq!((agent.as_str(), prompt.as_str(), result.as_deref()), ("hello agent", "Reply with exactly: Hello", Some("Hello")));
    // The subagent's own message (under its child session) shows under it.
    let child = &all["3e525e25-801f-492a-b669-2b34a3535bbb"];
    assert_eq!(child.parent.as_deref(), Some(sub.id.as_str()));
    assert_eq!(child.body, NodeBody::Message { text: "Hello".into() });
    assert_eq!(messages(&events), ["Hello"]);
}

#[tokio::test]
async fn muse_permission() {
    let mut h = Harness::t3("muse_permission", RuntimeMode::Ask).await;
    h.session.send(text_input("Run the shell command `touch muse-approved.txt` with your shell tool, then reply DONE.")).await.unwrap();
    let (id, item, body) = h.request().await;
    assert_eq!(item.as_deref(), Some("01a11a6d-4730-75f1-bc60-4e5df1a3cdc0"));
    let RequestBody::Approval(a) = body else { panic!() };
    assert_eq!((a.action, a.title.as_str(), a.command.as_deref()), (ApprovalAction::Command, "Run `touch muse-approved.txt`", Some("touch muse-approved.txt")));
    // Allow once, and Reject (Muse's abort) as the decline.
    assert_eq!(a.decisions, [Decision::Accept, Decision::Decline]);
    h.session.respond(&id, &Response::Decision { decision: Decision::Accept, message: None }).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);

    h.session.send(text_input("Run the shell command `touch muse-declined.txt` with your shell tool once. If it is rejected, do not retry; reply REJECTED.")).await.unwrap();
    let (id, _, _) = h.request().await;
    h.session.respond(&id, &Response::Decision { decision: Decision::Decline, message: None }).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let (events, frames) = h.finish().await;
    golden("muse_permission", &events);
    assert_eq!(messages(&events), ["DONE", "REJECTED"]);
    let choices: Vec<_> = sent(&frames, "approval/decide").iter().map(|p| p["choiceId"].clone()).collect();
    assert_eq!(choices, [json!("allow_once"), json!("abort")]);
    // Answered here, so never withdrawn.
    assert!(!events.iter().any(|e| matches!(e, ProviderEvent::RequestCancelled { .. })));
    let tool = &latest(&events)["01a11a6d-4730-75f1-bc60-4e5df1a3cdc0"];
    let NodeBody::Tool(t) = &tool.body else { panic!() };
    assert_eq!((t.kind, t.title.as_str(), t.exit_code, t.output.as_deref()), (ToolKind::Command, "touch muse-approved.txt", Some(0), None));
}

#[tokio::test]
async fn muse_subagent_approval() {
    let mut h = Harness::t3("muse_subagent_approval", RuntimeMode::Ask).await;
    h.session.send(text_input("Do not run any commands yourself. Spawn one subagent whose only task is to run this exact shell command: printf 'subagent approval fixture' > subagent-approval.txt and then reply with exactly: Written. Wait for it to finish, then reply with exactly what it said.")).await.unwrap();
    let (id, item, _) = h.request().await;
    // A subagent's approval shows on the subagent.
    assert_eq!(item.as_deref(), Some("01a12d1d-b4d9-74e2-ae49-6a2363589cbc"));
    h.session.respond(&id, &Response::Decision { decision: Decision::Accept, message: None }).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let (events, frames) = h.finish().await;
    golden("muse_subagent_approval", &events);
    assert_eq!(sent(&frames, "approval/decide")[0]["choiceId"], "allow_once");
    assert_eq!(messages(&events), ["Written"]);
}

#[tokio::test]
async fn muse_subagent_interrupt() {
    let mut h = Harness::t3("muse_subagent_interrupt", RuntimeMode::FullAccess).await;
    h.session.send(text_input("Spawn one subagent whose only task is to run the shell command `sleep 30` and then reply with exactly: Done. Wait for it to finish, then reply with exactly what it said.")).await.unwrap();
    h.until("the subagent", |e| matches!(e, ProviderEvent::Item(i) if matches!(i.body, NodeBody::Subagent { .. }))).await;
    h.session.interrupt().await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Interrupted);
    let (events, _) = h.finish().await;
    golden("muse_subagent_interrupt", &events);
    assert_eq!(latest(&events)["01a12d28-885c-7a80-80d9-d97502d7cda8"].status, NodeStatus::Interrupted);
}

#[tokio::test]
async fn muse_workflow() {
    let mut h = Harness::t3("muse_workflow", RuntimeMode::FullAccess).await;
    assert_eq!(h.turn("Use your workflow tool to launch a workflow with exactly TWO agents: one labeled 'ping' that replies with the word PING, and one labeled 'pong' that replies with the word PONG. Launch it and end your turn; do not wait for it.").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("muse_workflow", &events);
    let wf = &latest(&events)["e1f5bbdc-9a2d-52c5-8501-9b6ef280aa94"];
    let NodeBody::Tool(t) = &wf.body else { panic!() };
    assert_eq!((t.name.as_str(), t.title.as_str()), ("workflow", "ping-pong"));
    // The workflow's agents ran after the turn, and Muse's own wake turn isn't adopted (a known
    // gap): one turn, and none of the wake turn's message.
    assert_eq!(ends(&events), [&TurnEnd::Completed]);
    assert!(messages(&events).is_empty(), "{events:#?}");
}

// ---------------------------------------------------------------------------------------------
// Written here
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn questions_are_answered() {
    let dir = tempfile::tempdir().unwrap();
    let ask = json!({ "userInputId": "q1", "turnId": "turn-1", "questions": [
        { "id": "color", "header": "", "question": "Which color?", "options": [{ "label": "Red" }, { "label": "Blue", "description": "calm" }], "selection": { "mode": "single" } },
        { "id": "extras", "header": "Extras", "question": "Which extras?", "options": [{ "label": "A" }, { "label": "B" }], "selection": { "mode": "multiple", "maxSelections": 2 } },
    ] });
    let mut entries = opening(RuntimeMode::FullAccess);
    entries.extend(turn_start(3, "turn-1", "ask me"));
    entries.extend([
        emit(json!({ "id": 9, "method": "userInput/request", "params": { "sessionId": "sess-1", "userInputId": "q1" } })),
        note("userInput/requested", ask),
        expect(json!({ "id": 9, "result": {} })),
        expect(json!({ "id": 4, "method": "userInput/answer", "params": { "sessionId": "sess-1", "userInputId": "q1", "commandId": "<any>", "answers": [
            { "questionId": "color", "selectedLabel": "Blue" },
            { "questionId": "extras", "selectedLabels": ["A", "B"], "note": "and C" },
        ] } })),
        accepted(4),
        note("userInput/settled", json!({ "userInputId": "q1" })),
        message("m1", "turn-1", "Blue it is"),
        turn_completed("turn-1", json!({})),
    ]);
    let mut h = Harness::open(&synthetic(dir.path(), "questions", &entries), RuntimeMode::FullAccess, |_| {}).await;
    h.session.send(text_input("ask me")).await.unwrap();
    let (id, _, body) = h.request().await;
    let RequestBody::Questions { questions } = body else { panic!() };
    assert_eq!((questions[0].header.as_str(), questions[0].multi, questions[1].multi), ("Question", false, true));
    assert_eq!(questions[0].options[1].description.as_deref(), Some("calm"));
    let answers = BTreeMap::from([("color".to_string(), "Blue".to_string()), ("extras".to_string(), "A, B, and C".to_string())]);
    h.session.respond(&id, &Response::Answers { answers }).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    assert_eq!(messages(&events), ["Blue it is"]);
    assert!(!events.iter().any(|e| matches!(e, ProviderEvent::RequestCancelled { .. })));
}

#[tokio::test]
async fn unanswered_requests_are_withdrawn() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = opening(RuntimeMode::Ask);
    entries.extend(turn_start(3, "turn-1", "go"));
    entries.extend(shell_approval(7, "a1", "turn-1", None));
    entries.extend([
        // Settled without us (a timeout): withdrawn.
        note("approval/resolved", json!({ "approvalId": "a1", "decision": "abort" })),
        // An unknown server request is refused, not left hanging.
        emit(json!({ "id": 8, "method": "fs/readFile", "params": {} })),
        expect(json!({ "id": 8, "error": { "code": -32601, "message": "roll doesn't handle fs/readFile" } })),
    ]);
    entries.extend(shell_approval(9, "a2", "turn-1", None));
    entries.push(turn_completed("turn-1", json!({ "terminal": "cancelled", "reason": "stopped" })));
    let mut h = Harness::open(&synthetic(dir.path(), "withdrawn", &entries), RuntimeMode::Ask, |_| {}).await;
    h.session.send(text_input("go")).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Interrupted);
    let (events, _) = h.finish().await;
    let cancelled: Vec<_> = events.iter().filter_map(|e| if let ProviderEvent::RequestCancelled { id } = e { Some(id.as_str()) } else { None }).collect();
    // The second one was still open when the turn ended.
    assert_eq!(cancelled, ["a1", "a2"]);
}

#[tokio::test]
async fn steering_and_instructions() {
    let dir = tempfile::tempdir().unwrap();
    let instructions = "<roll_instructions>\n## roll orchestration\n</roll_instructions>";
    let mut entries = opening(RuntimeMode::FullAccess);
    entries.extend([
        expect(json!({ "id": 3, "method": "turn/start", "params": {
            "sessionId": "sess-1", "input": [{ "type": "text", "text": instructions }, { "type": "text", "text": "write a poem" }, { "type": "image", "base64Data": "QUJD", "mediaType": "image/png" }],
            "displayText": "write a poem", "workspaceRoots": ["<workspace>"], "ifBusy": "queue", "reasoningEffort": "max", "commandId": "turn-1",
        } })),
        emit(json!({ "id": 3, "result": { "turnId": "turn-1" } })),
        note("turn/started", json!({ "turnId": "turn-1" })),
        expect(json!({ "id": 4, "method": "turn/steer", "params": {
            "sessionId": "sess-1", "expectedTurnId": "turn-1", "input": [{ "type": "text", "text": "make it short" }], "commandId": "<any>",
        } })),
        emit(json!({ "id": 4, "result": { "turnId": "turn-1" } })),
        message("m1", "turn-1", "Short poem."),
        turn_completed("turn-1", json!({})),
    ]);
    let mut h = Harness::open(&synthetic(dir.path(), "steer", &entries), RuntimeMode::FullAccess, |c| {
        c.instructions = Some("## roll orchestration".into());
        c.effort = Some("max".into());
    })
    .await;
    let image = agent_proto::Attachment { name: "a.png".into(), mime: "image/png".into(), data: "QUJD".into() };
    h.session.send(TurnInput { text: "write a poem".into(), attachments: vec![image] }).await.unwrap();
    h.session.steer(text_input("make it short")).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    assert_eq!(messages(&events), ["Short poem."]);
}

#[tokio::test]
async fn compaction() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = opening(RuntimeMode::FullAccess);
    entries.extend([
        expect(json!({ "id": 3, "method": "session/compact", "params": { "sessionId": "sess-1", "commandId": "turn-1" } })),
        emit(json!({ "id": 3, "result": { "status": "accepted" } })),
        item("item/started", json!({ "itemId": "k1", "kind": "compaction", "turnId": "muse-own", "revision": 1, "status": "inProgress", "trigger": "manual" })),
        item("item/completed", json!({ "itemId": "k1", "kind": "compaction", "turnId": "muse-own", "revision": 2, "status": "completed", "outcome": "compacted" })),
        note("session/contextUsage", json!({ "usedTokens": 1200, "windowTokens": 1_000_000 })),
        // A second one with nothing to compact.
        expect(json!({ "id": 4, "method": "session/compact", "params": { "sessionId": "sess-1", "commandId": "turn-2" } })),
        emit(json!({ "id": 4, "result": { "status": "noop", "reason": "Nothing to compact yet." } })),
    ]);
    let mut h = Harness::open(&synthetic(dir.path(), "compact", &entries), RuntimeMode::FullAccess, |_| {}).await;
    h.session.compact().await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    h.until("the context after", |e| matches!(e, ProviderEvent::Context(c) if c.used_tokens == 1200)).await;
    h.session.compact().await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Failed("Nothing to compact yet.".into()));
    let (events, _) = h.finish().await;
    let k = &latest(&events)["k1"];
    assert_eq!((k.status, &k.body), (NodeStatus::Completed, &NodeBody::Compaction { before_tokens: None, after_tokens: None, error: None }));
}

#[tokio::test]
async fn workflow_children_show_under_the_workflow() {
    let dir = tempfile::tempdir().unwrap();
    let wf = |rev: i64, status: &str, children: Value| item("item/updated", json!({ "itemId": "w1", "kind": "workflow", "turnId": "turn-1", "revision": rev, "status": status, "entryId": "fan-out", "children": children }));
    let mut entries = opening(RuntimeMode::FullAccess);
    entries.extend(turn_start(3, "turn-1", "fan out"));
    entries.extend([
        // The workflow tool's own call is noise: the workflow item is its row.
        item("item/completed", json!({ "itemId": "call", "kind": "toolCall", "turnId": "turn-1", "revision": 2, "status": "completed", "tool": "workflow", "args": "{}" })),
        wf(1, "inProgress", json!([{ "childId": "c1", "attempt": 1, "status": "scheduled", "label": "lint" }, { "childId": "c2", "attempt": 1, "status": "scheduled", "label": "test" }])),
        wf(2, "inProgress", json!([{ "childId": "c1", "attempt": 1, "status": "started" }, { "childId": "c2", "attempt": 1, "status": "started" }])),
        wf(3, "completed", json!([
            { "childId": "c1", "attempt": 1, "status": "terminal", "terminal": "completed" },
            { "childId": "c2", "attempt": 1, "status": "terminal", "terminal": "completed", "failureKind": "timeout", "failureReason": "ran out of time" },
        ])),
        turn_completed("turn-1", json!({})),
    ]);
    let mut h = Harness::open(&synthetic(dir.path(), "workflow", &entries), RuntimeMode::FullAccess, |_| {}).await;
    assert_eq!(h.turn("fan out").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    let all = latest(&events);
    assert!(!all.contains_key("call"));
    assert_eq!(all["w1"].status, NodeStatus::Completed);
    let lint = &all["w1:agent:c1"];
    let test = &all["w1:agent:c2"];
    assert_eq!((lint.parent.as_deref(), lint.status), (Some("w1"), NodeStatus::Completed));
    // Labels stay after later revisions drop them; a failed attempt fails even if "completed".
    assert!(matches!(&lint.body, NodeBody::Subagent { agent, .. } if agent == "lint"));
    assert_eq!(test.status, NodeStatus::Failed);
    assert!(matches!(&test.body, NodeBody::Subagent { agent, result, .. } if agent == "test" && result.as_deref() == Some("ran out of time")));
}

#[tokio::test]
async fn usage_limits_and_quota_failures() {
    let dir = tempfile::tempdir().unwrap();
    // 2100-01-01T00:00:00Z.
    let reset = 4_102_444_800_000u64;
    let usage = |used: f64| emit(json!({ "method": "usage/changed", "params": {
        "observedAtMs": 1, "tier": "plus", "window": { "usedPercent": used, "windowDurationMins": 300, "resetsAtMs": reset }, "weekly": { "usedPercent": 30, "resetsAtMs": reset + 1 },
    } }));
    let mut entries = opening(RuntimeMode::FullAccess);
    // Account-wide, between turns too.
    entries.push(usage(80.0));
    entries.extend(turn_start(3, "turn-1", "hi"));
    entries.extend([
        note("session/tokenUsage", json!({ "turnId": "turn-1", "promptTokens": 100, "usage": { "inputTokens": 100, "outputTokens": 5, "cachedTokens": 0, "cacheReadTokens": 40, "reasoningTokens": 0 } })),
        note("session/tokenUsage", json!({ "turnId": "turn-1", "promptTokens": 120, "usage": { "inputTokens": 120, "outputTokens": 7, "cachedTokens": 10, "reasoningTokens": 0 } })),
        usage(100.0),
        turn_completed("turn-1", json!({ "terminal": "failed", "error": { "message": "429: quota exceeded", "quota": { "resetsAtMs": reset - 60_000 } } })),
    ]);
    let mut h = Harness::open(&synthetic(dir.path(), "quota", &entries), RuntimeMode::FullAccess, |_| {}).await;
    let ProviderEvent::UsageLimits(limits) = h.until("usage limits", |e| matches!(e, ProviderEvent::UsageLimits(_))).await else { unreachable!() };
    assert_eq!(limits.plan.as_deref(), Some("plus"));
    let windows: Vec<_> = limits.windows.iter().map(|w| (w.id.as_str(), w.kind, w.used_percent, w.resets_at)).collect();
    assert_eq!(windows, [("window", UsageWindowKind::Session, 80.0, Some(reset)), ("weekly", UsageWindowKind::Weekly, 30.0, Some(reset + 1))]);
    h.session.send(text_input("hi")).await.unwrap();
    // The latest reset of what's exhausted: the full window's.
    assert_eq!(
        h.turn_end().await,
        TurnEnd::Failed("Muse Code usage limit reached; it resets at 2100-01-01 00:00 UTC. Send the message again then. (429: quota exceeded)".into())
    );
    let (events, _) = h.finish().await;
    let usages: Vec<_> = events.iter().filter_map(|e| if let ProviderEvent::Usage(u) = e { Some((u.input_tokens, u.output_tokens, u.cached_tokens)) } else { None }).collect();
    assert_eq!(usages, [(100, 5, 40), (220, 12, 50)]);
    assert_eq!(events.iter().filter(|e| matches!(e, ProviderEvent::UsageLimits(_))).count(), 2);
}

#[tokio::test]
async fn full_access_approves_a_childs_approval() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = opening(RuntimeMode::FullAccess);
    entries.extend(turn_start(3, "turn-1", "delegate"));
    entries.push(item("item/started", json!({ "itemId": "sub", "kind": "subagent", "turnId": "turn-1", "revision": 1, "status": "inProgress", "subagentId": "subagent-1", "childSessionId": "child", "objective": "clean" })));
    entries.extend(shell_approval(7, "a1", "child", Some("subagent-1")));
    entries.extend([decide(4, "a1", "allow_once", "<any>"), accepted(4), note("approval/resolved", json!({ "approvalId": "a1", "decision": "approved" })), turn_completed("turn-1", json!({}))]);
    let mut h = Harness::open(&synthetic(dir.path(), "child_full", &entries), RuntimeMode::FullAccess, |_| {}).await;
    assert_eq!(h.turn("delegate").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Request { .. } | ProviderEvent::RequestCancelled { .. })));
}

#[tokio::test]
async fn a_childs_approval_between_turns_waits_for_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = opening(RuntimeMode::Ask);
    entries.extend(turn_start(3, "turn-1", "delegate"));
    entries.push(item("item/started", json!({ "itemId": "sub", "kind": "subagent", "turnId": "turn-1", "revision": 1, "status": "inProgress", "subagentId": "subagent-1", "childSessionId": "child", "objective": "clean" })));
    entries.push(turn_completed("turn-1", json!({})));
    // The subagent goes on, and asks while no turn of ours runs.
    entries.extend(shell_approval(7, "a1", "child", Some("subagent-1")));
    entries.extend(turn_start(4, "turn-2", "next"));
    entries.extend([decide(5, "a1", "abort", "<any>"), accepted(5), note("approval/resolved", json!({ "approvalId": "a1", "decision": "abort" })), turn_completed("turn-2", json!({}))]);
    let mut h = Harness::open(&synthetic(dir.path(), "child_later", &entries), RuntimeMode::Ask, |_| {}).await;
    assert_eq!(h.turn("delegate").await, TurnEnd::Completed);
    h.session.send(text_input("next")).await.unwrap();
    let (id, item, body) = h.request().await;
    assert_eq!((id.as_str(), item.as_deref()), ("a1", Some("sub")));
    let RequestBody::Approval(a) = body else { panic!() };
    assert_eq!(a.decisions, [Decision::Accept, Decision::AcceptForSession, Decision::Decline]);
    h.session.respond(&id, &Response::Decision { decision: Decision::Decline, message: None }).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    h.finish().await;
}

#[tokio::test]
async fn a_turn_muse_starts_while_ours_runs_is_joined() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = opening(RuntimeMode::FullAccess);
    // Muse wakes by itself while idle: not adopted.
    entries.extend([
        note("turn/started", json!({ "turnId": "wake" })),
        message("w0", "wake", "unseen"),
        note("session/contextUsage", json!({ "usedTokens": 7 })),
    ]);
    entries.extend(turn_start(3, "turn-1", "go"));
    entries.extend([
        // Ours queued behind it: the wake turn's work shows from here, and Stop ends both.
        message("w1", "wake", "seen"),
        expect(json!({ "id": 4, "method": "turn/interrupt", "params": { "sessionId": "sess-1", "turnId": "wake", "commandId": "<any>" } })),
        accepted(4),
        expect(json!({ "id": 5, "method": "turn/interrupt", "params": { "sessionId": "sess-1", "turnId": "turn-1", "commandId": "<any>" } })),
        accepted(5),
        turn_completed("wake", json!({ "terminal": "cancelled" })),
        turn_completed("turn-1", json!({ "terminal": "cancelled" })),
    ]);
    let mut h = Harness::open(&synthetic(dir.path(), "joined", &entries), RuntimeMode::FullAccess, |_| {}).await;
    // Seen once Muse's own turn has started and spoken.
    h.until("the context", |e| matches!(e, ProviderEvent::Context(c) if c.used_tokens == 7)).await;
    h.session.send(text_input("go")).await.unwrap();
    h.until("the joined turn's message", |e| matches!(e, ProviderEvent::Item(i) if i.id == "w1")).await;
    h.session.interrupt().await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Interrupted);
    let (events, _) = h.finish().await;
    assert_eq!(messages(&events), ["seen"]);
}

#[tokio::test]
async fn resume_and_model_switch() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![
        host(&["serve", "--trust-workspace"]),
        expect(json!({ "id": 1, "method": "initialize", "params": { "clientInfo": {}, "capabilities": { "requestedCapabilities": ["sessionMcp"] } } })),
        emit(json!({ "id": 1, "result": { "schema": { "version": 1 }, "grantedCapabilities": ["sessionMcp"] } })),
        expect(json!({ "method": "initialized" })),
        expect(json!({ "id": 2, "method": "session/resume", "params": {
            "sessionId": "old-session", "excludeItems": true, "permissionProfile": ":ask-me", "commandId": "<any>",
            "config": { "mcpServers": { "roll": { "transport": "streamableHttp", "mode": "optional", "url": "http://127.0.0.1:9/mcp", "headers": { "authorization": "Bearer t" } } } },
        } })),
        emit(json!({ "id": 2, "result": { "session": { "sessionId": "old-session", "modelId": "muse-old" } } })),
        expect(json!({ "id": 3, "method": "session/setApprovalMode", "params": { "sessionId": "old-session", "mode": "promptUnmatched", "commandId": "<any>" } })),
        emit(json!({ "id": 3, "result": { "status": "accepted" } })),
        expect(json!({ "id": 4, "method": "session/setModel", "params": { "sessionId": "old-session", "model": { "modelId": "muse-new", "providerId": "meta" }, "commandId": "<any>" } })),
        emit(json!({ "id": 4, "result": { "status": "accepted" } })),
        expect(json!({ "id": 5, "method": "turn/start", "params": {
            "sessionId": "old-session", "input": [{ "type": "text", "text": "again" }], "displayText": "again", "workspaceRoots": ["<workspace>"], "ifBusy": "queue", "commandId": "turn-1",
        } })),
        emit(json!({ "id": 5, "result": { "turnId": "turn-1" } })),
        emit(json!({ "method": "turn/completed", "params": { "sessionId": "old-session", "turnId": "turn-1", "terminal": "completed" } })),
    ];
    let mut h = Harness::open(&synthetic(dir.path(), "resume", &entries), RuntimeMode::Ask, |c| {
        c.resume = Some("old-session".into());
        c.model = Some("muse-new".into());
        c.mcp = vec![McpServer { name: "roll".into(), url: "http://127.0.0.1:9/mcp".into(), headers: vec![("authorization".into(), "Bearer t".into())] }];
    })
    .await;
    assert_eq!(h.turn("again").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    assert_eq!(events[0], ProviderEvent::NativeThread { id: "old-session".into() });
    let models: Vec<_> = events.iter().filter_map(|e| if let ProviderEvent::Model { model } = e { Some(model.as_str()) } else { None }).collect();
    assert_eq!(models, ["muse-old", "muse-new"]);
}

#[tokio::test]
async fn resuming_a_session_muse_lost_fails_so_a_fresh_one_starts() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![
        host(&["serve", "--trust-workspace", "--disable-sandbox"]),
        expect(json!({ "id": 1, "method": "initialize", "params": { "clientInfo": {}, "capabilities": { "requestedCapabilities": ["sessionMcp"] } } })),
        emit(json!({ "id": 1, "result": { "schema": { "version": 1 }, "grantedCapabilities": [] } })),
        expect(json!({ "method": "initialized" })),
        expect(json!({ "id": 2, "method": "session/resume", "params": { "sessionId": "gone", "excludeItems": true, "permissionProfile": ":unrestricted", "commandId": "<any>" } })),
        emit(json!({ "id": 2, "error": { "code": -32000, "message": "Saved session is absent", "data": { "kind": "sessionNotFound" } } })),
    ];
    let transcript = synthetic(dir.path(), "lost", &entries);
    let err = start(&dir, &transcript, RuntimeMode::FullAccess, |c| c.resume = Some("gone".into())).await.err().expect("resume fails");
    assert!(err.to_string().contains("muse has no session gone to resume"), "{err:#}");
    // No session was started in its place: the orchestrator does that, handing the thread over.
    let log = raw_log(&dir);
    assert!(log.iter().any(|l| l.get("complete").is_some()), "{log:#?}");
    assert!(sent(&frames(&log), "session/start").is_empty());

    // And a fresh start works (this Muse can't take session MCP servers, so none are asked for).
    let dir = tempfile::tempdir().unwrap();
    let mut entries = opening(RuntimeMode::FullAccess);
    entries[2] = emit(json!({ "id": 1, "result": { "schema": { "version": 1 }, "grantedCapabilities": [] } }));
    let transcript = synthetic(dir.path(), "fresh", &entries);
    let (session, mut rx) = start(&dir, &transcript, RuntimeMode::FullAccess, |_| {}).await.unwrap();
    assert_eq!(rx.recv().await, Some(ProviderEvent::NativeThread { id: rx_bound(&dir, "sess-1") }));
    session.close().await;
}

/// The id roll minted for the recorded `recorded`.
fn rx_bound(dir: &tempfile::TempDir, recorded: &str) -> String {
    raw_log(dir).iter().filter_map(|l| l.get("bind")).find(|b| b[0] == recorded).map(|b| b[1].as_str().unwrap().to_string()).expect("bound")
}

#[tokio::test]
async fn an_old_muse_without_session_mcp_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = opening(RuntimeMode::FullAccess);
    entries[2] = emit(json!({ "id": 1, "result": { "schema": { "version": 1 }, "grantedCapabilities": [] } }));
    entries.truncate(4);
    let transcript = synthetic(dir.path(), "old", &entries);
    let err = start(&dir, &transcript, RuntimeMode::FullAccess, |c| c.mcp = vec![McpServer { name: "roll".into(), url: "http://x/mcp".into(), headers: Vec::new() }])
        .await
        .err()
        .expect("refused");
    assert!(err.to_string().contains("can't take session MCP servers"), "{err:#}");

    // Nor is another MSP schema.
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![host(&["serve", "--trust-workspace", "--disable-sandbox"]), expect(json!({ "id": 1, "method": "initialize", "params": "<any>" })), emit(json!({ "id": 1, "result": { "schema": { "version": 2 } } }))];
    let transcript = synthetic(dir.path(), "schema", &entries);
    let err = start(&dir, &transcript, RuntimeMode::FullAccess, |_| {}).await.err().expect("refused");
    assert!(err.to_string().contains("MSP schema"), "{err:#}");
}

// ---------------------------------------------------------------------------------------------
// The status probe
// ---------------------------------------------------------------------------------------------

fn probe_transcript(dir: &Path, models: Value) -> String {
    synthetic(dir, "probe", &[
        host(&["serve", "--disable-shell", "--disable-write", "--no-session-log"]),
        expect(json!({ "id": 1, "method": "initialize", "params": { "clientInfo": {}, "capabilities": { "requestedCapabilities": [] } } })),
        emit(json!({ "id": 1, "result": { "schema": { "version": 1 }, "grantedCapabilities": [] } })),
        expect(json!({ "method": "initialized" })),
        expect(json!({ "id": 2, "method": "model/list", "params": {} })),
        emit(json!({ "id": 2, "result": { "providerId": "meta", "models": models } })),
    ])
}

#[tokio::test]
async fn info_reads_version_and_models_without_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let models = json!([
        { "modelId": MODEL, "displayLabel": "Muse Spark 1.3", "providerId": "meta", "isDefault": true, "variants": ["low", "high", "ultra"], "defaultReasoningEffort": "high" },
        { "modelId": "muse-lite", "displayLabel": "", "providerId": "meta", "isDefault": false },
    ]);
    let muse = Muse::new(fake_config(Path::new(&probe_transcript(dir.path(), models)), &dir.path().join("log.ndjson")));
    let info = muse.info().await;
    assert!(info.installed);
    assert_eq!((info.name.as_str(), info.version.as_deref(), info.detail.as_deref()), ("Muse Code", Some("1.4.6-R5018.1"), None));
    // MSP has no login signal.
    assert_eq!(info.auth, AuthStatus::Unknown);
    assert_eq!(info.capabilities, muse.capabilities());
    assert_eq!(info.models.iter().map(|m| (m.id.as_str(), m.name.as_str(), m.default)).collect::<Vec<_>>(), [
        (MODEL, "Muse Spark 1.3", true),
        ("muse-lite", "Muse Lite", false),
    ]);
    assert_eq!(info.models[0].efforts(), ["low", "high", "ultra"]);
    assert_eq!(info.models[1].efforts(), ["low", "medium", "high", "xhigh", "max"]);
    assert_eq!(info.usage_limits, None);
    // A read-only host that only listed models: no session, no turn.
    let log = raw_log(&dir);
    assert!(log.iter().any(|l| l.get("complete").is_some()), "{log:#?}");
    let methods: Vec<_> = frames(&log).iter().filter_map(|f| f["method"].as_str().map(str::to_string)).collect();
    assert_eq!(methods, ["initialize", "initialized", "model/list"]);
    assert_eq!(started(&log)["env"]["MUSE_NO_AUTO_UPDATE"], "1");
}

#[tokio::test]
async fn info_without_models_says_to_log_in() {
    let dir = tempfile::tempdir().unwrap();
    let muse = Muse::new(fake_config(Path::new(&probe_transcript(dir.path(), json!([]))), &dir.path().join("log.ndjson")));
    let info = muse.info().await;
    assert!(info.installed && info.models.is_empty());
    assert_eq!(info.auth, AuthStatus::Unknown);
    assert!(info.detail.as_deref().unwrap().contains("muse login"), "{info:?}");
}

#[tokio::test]
async fn info_without_muse() {
    let muse = Muse::new(ProviderConfig { binary: Some("/nonexistent/muse".into()), ..Default::default() });
    let info = muse.info().await;
    assert!(!info.installed);
    assert_eq!(info.auth, AuthStatus::Unknown);
    assert!(info.detail.as_deref().unwrap().contains("not installed"), "{info:?}");
}

// ---------------------------------------------------------------------------------------------
// The environment
// ---------------------------------------------------------------------------------------------

/// Run in a child test process that has `META_API_KEY` set (`muse_drops_an_inherited_meta_api_key`).
#[tokio::test]
async fn env_child() {
    if std::env::var("ROLL_MUSE_ENV_CHILD").is_err() {
        return;
    }
    assert_eq!(std::env::var("META_API_KEY").as_deref(), Ok("inherited"));
    let dir = tempfile::tempdir().unwrap();
    let transcript = synthetic(dir.path(), "env", &opening(RuntimeMode::FullAccess));
    let (session, _rx) = start(&dir, &transcript, RuntimeMode::FullAccess, |_| {}).await.unwrap();
    session.close().await;
    let env = &started(&raw_log(&dir))["env"];
    assert_eq!((env["META_API_KEY"].clone(), env["MUSE_NO_AUTO_UPDATE"].clone()), (Value::Null, json!("1")));

    // Unless the provider's config sets one.
    let dir = tempfile::tempdir().unwrap();
    let transcript = synthetic(dir.path(), "env", &opening(RuntimeMode::FullAccess));
    let mut cfg = fake_config(Path::new(&transcript), &dir.path().join("log.ndjson"));
    cfg.env.push(("META_API_KEY".into(), "configured".into()));
    let (session, _rx) = Muse::new(cfg).start(session_config(dir.path(), RuntimeMode::FullAccess)).await.unwrap();
    session.close().await;
    assert_eq!(started(&raw_log(&dir))["env"]["META_API_KEY"], "configured");
}

#[test]
fn muse_drops_an_inherited_meta_api_key() {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["env_child", "--exact", "--nocapture", "-q"])
        .env("ROLL_MUSE_ENV_CHILD", "1")
        .env("META_API_KEY", "inherited")
        .status()
        .unwrap();
    assert!(status.success());
}
