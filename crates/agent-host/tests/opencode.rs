//! The OpenCode adapter against a fake `opencode serve` that replays recorded HTTP and SSE
//! transcripts (`tests/fixtures/opencode/*.ndjson`): T3 Code's OpenCode 2.0.18 replay fixtures
//! (MIT, from `apps/server/src/orchestration-v2/testkit/fixtures/opencode2_*/
//! opencode_transcript.ndjson`, protocol `opencode2-http.sse`), ported to roll (each file's
//! `metadata.roll` says what changed: the instructions entry is `roll`, and T3's title updates,
//! MCP rules and the reads only T3 makes are dropped). Each scenario's events are compared with
//! `tests/fixtures/opencode/<name>.events` (regenerate with `ROLL_FIXTURES=write cargo test -p
//! agent-host --test opencode`), and the important ones are asserted outright.
//!
//! The fake is this test binary run again (`fake_opencode`): `--version` prints a version, and
//! `serve` prints the listening line and answers HTTP from the transcript the way T3's replay
//! harness does (`OpenCode2AdapterV2.testkit.ts`, `OpenCodeAdapterV2.testkit.ts`): each request
//! maps to an SDK operation (the inverse of T3's SDK → HTTP table), must match the next
//! `expect_outbound` (a subset match where `<any>` matches anything, with the ids of roll's
//! prompts bound to the recorded ones), and waits until every earlier recorded event and answer
//! has gone out; `GET /api/event` streams the recorded events; a `null` answer is a 204. It
//! checks Basic auth and the server environment, logs every operation, writes `complete` when
//! the transcript ends, and fails loudly (stderr, the log, a 500 and the event stream closed) on
//! the first request it didn't expect.

use agent_host::provider::opencode::OpenCode;
use agent_host::provider::{
    ForkAt, ForkFrom, McpServer, Provider, ProviderConfig, ProviderEvent, Session, SessionConfig, TurnEnd, TurnInput,
};
use agent_proto::{
    ApprovalAction, AuthStatus, Decision, InteractionMode, NodeBody, NodeStatus, RequestBody, Response, RuntimeMode, ToolKind,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

const TRANSCRIPT: &str = "ROLL_FAKE_OPENCODE";
const LOG: &str = "ROLL_FAKE_OPENCODE_LOG";
const ARGS: &str = "ROLL_FAKE_OPENCODE_ARGS";
const WORK: &str = "ROLL_FAKE_OPENCODE_WORK";

// ---------------------------------------------------------------------------------------------
// The fake
// ---------------------------------------------------------------------------------------------

/// Not a test: the fake OpenCode server, when the adapter runs this binary with
/// `ROLL_FAKE_OPENCODE` set.
#[test]
fn fake_opencode() {
    let Ok(transcript) = std::env::var(TRANSCRIPT) else { return };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let code = match runtime.block_on(fake::serve(PathBuf::from(transcript))) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("fake-opencode: {e}");
            3
        }
    };
    std::process::exit(code);
}

mod fake {
    use super::*;
    use axum::body::{Body, Bytes};
    use axum::extract::State;
    use axum::http::{HeaderMap, Method, StatusCode, Uri};
    use axum::response::IntoResponse;
    use base64::Engine;

    struct Ctl {
        entries: Vec<Value>,
        state: Mutex<CtlState>,
        changed: tokio::sync::Notify,
        log: Mutex<Option<std::fs::File>>,
        work: String,
        auth: String,
    }

    #[derive(Default)]
    struct CtlState {
        cursor: usize,
        ids: HashMap<String, String>,
        failure: Option<String>,
        complete: bool,
    }

    pub async fn serve(transcript: PathBuf) -> Result<(), String> {
        let text = std::fs::read_to_string(&transcript).map_err(|e| format!("{}: {e}", transcript.display()))?;
        let entries: Vec<Value> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).map_err(|e| e.to_string()))
            .collect::<Result<Vec<Value>, _>>()?
            .into_iter()
            .filter(|e| e["type"] != "transcript_start")
            .collect();
        let args = std::env::var(ARGS).unwrap_or_default();
        let port = args.split_whitespace().find_map(|a| a.strip_prefix("--port=")).ok_or("no --port")?.to_string();
        let host = args.split_whitespace().find_map(|a| a.strip_prefix("--hostname=")).ok_or("no --hostname")?.to_string();
        let password = std::env::var("OPENCODE_PASSWORD").map_err(|_| "no OPENCODE_PASSWORD")?;
        let log = std::env::var(LOG).ok().map(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).expect("log"));
        let ctl = Arc::new(Ctl {
            entries,
            state: Mutex::default(),
            changed: tokio::sync::Notify::new(),
            log: Mutex::new(log),
            work: std::env::var(WORK).unwrap_or_else(|_| "<work>".into()),
            auth: format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"))),
        });
        ctl.write(json!({ "args": args, "server_password_inherited": std::env::var_os("OPENCODE_SERVER_PASSWORD").is_some(),
            "config_content": std::env::var("OPENCODE_CONFIG_CONTENT").ok() }));
        let listener = tokio::net::TcpListener::bind(format!("{host}:{port}")).await.map_err(|e| e.to_string())?;
        println!("opencode server listening on http://{host}:{port}");
        let app = axum::Router::new().fallback(handle).with_state(ctl);
        axum::serve(listener, app).await.map_err(|e| e.to_string())
    }

    impl Ctl {
        fn write(&self, line: Value) {
            if let Some(log) = self.log.lock().unwrap().as_mut() {
                writeln!(log, "{line}").unwrap();
            }
        }

        fn fail(&self, state: &mut CtlState, why: String) -> String {
            if state.failure.is_none() {
                eprintln!("fake-opencode: {why}");
                self.write(json!({ "failure": why }));
                state.failure = Some(why.clone());
            }
            self.changed.notify_waiters();
            why
        }

        /// Move past an entry. The transcript is complete once only a clean exit is left (T3's
        /// `assertComplete`).
        fn advance(&self, state: &mut CtlState) {
            state.cursor += 1;
            let rest = &self.entries[state.cursor.min(self.entries.len())..];
            if !state.complete && rest.iter().all(|e| e["type"] == "runtime_exit" && e["status"] == "success") {
                state.complete = true;
                self.write(json!({ "complete": true }));
            }
            self.changed.notify_waiters();
        }

        /// Wait until `check` says something.
        async fn wait<R>(&self, mut check: impl FnMut(&Ctl, &mut CtlState) -> Option<R>) -> R {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if let Some(r) = check(self, &mut self.state.lock().unwrap()) {
                    return r;
                }
                changed.await;
            }
        }

        fn materialize(&self, value: &Value, ids: &HashMap<String, String>) -> Value {
            match value {
                Value::Array(a) => Value::Array(a.iter().map(|v| self.materialize(v, ids)).collect()),
                Value::Object(o) => Value::Object(
                    o.iter()
                        .map(|(k, v)| {
                            let bound = matches!(k.as_str(), "id" | "messageID" | "parentID" | "inboxID" | "before")
                                .then(|| v.as_str().and_then(|s| ids.get(s)))
                                .flatten();
                            (k.clone(), bound.map(|b| json!(b)).unwrap_or_else(|| self.materialize(v, ids)))
                        })
                        .collect(),
                ),
                Value::String(s) if s.contains("<work>") => json!(s.replace("<work>", &self.work)),
                other => other.clone(),
            }
        }

        async fn expect(&self, actual: &Value) -> Result<(), String> {
            let mut state = self.state.lock().unwrap();
            if let Some(f) = &state.failure {
                return Err(f.clone());
            }
            let Some(entry) = self.entries.get(state.cursor).filter(|e| e["type"] == "expect_outbound") else {
                let next = self.entries.get(state.cursor).cloned().unwrap_or(json!("the end of the transcript"));
                let why = format!("unexpected {actual} at entry {}; next is {next}", state.cursor);
                return Err(self.fail(&mut state, why));
            };
            let expected = &entry["frame"];
            let mut ids = state.ids.clone();
            let mut conflict = false;
            if expected["type"] == "session.prompt" && actual["type"] == "session.prompt" {
                let (recorded, mine) = (expected["input"]["id"].as_str(), actual["input"]["id"].as_str());
                if let (Some(r), Some(m)) = (recorded, mine)
                    && r != "<any>"
                    && !ids.contains_key(r)
                {
                    conflict = ids.values().any(|v| v == m);
                    ids.insert(r.to_string(), m.to_string());
                }
            }
            let expected = self.materialize(expected, &ids);
            if conflict || !matches(&expected, actual) {
                let why = format!("entry {}: expected {expected}, got {actual}", state.cursor);
                return Err(self.fail(&mut state, why));
            }
            state.ids = ids;
            self.advance(&mut state);
            Ok(())
        }

        async fn until_inbound_delivered(&self) {
            self.wait(|ctl, st| (st.failure.is_some() || ctl.entries.get(st.cursor).is_none_or(|e| e["type"] != "emit_inbound")).then_some(())).await
        }

        async fn response(&self, operation: &str) -> Result<Value, String> {
            self.wait(|ctl, st| {
                if let Some(f) = &st.failure {
                    return Some(Err(f.clone()));
                }
                let entry = ctl.entries.get(st.cursor)?;
                let frame = &entry["frame"];
                if entry["type"] == "emit_inbound" && frame["type"] == "sdk.response" && frame["operation"] == operation {
                    let data = ctl.materialize(&frame["data"], &st.ids);
                    ctl.advance(st);
                    return Some(Ok(data));
                }
                if entry["type"] == "runtime_exit" {
                    return Some(Err(ctl.fail(st, format!("the transcript ended before answering {operation}"))));
                }
                None
            })
            .await
        }

        /// The next recorded event; `None` once the replay failed. After the transcript's end the
        /// stream stays open and quiet, as a live server's does between turns.
        async fn next_event(&self) -> Option<Value> {
            self.wait(|ctl, st| {
                if st.failure.is_some() {
                    return Some(None);
                }
                let entry = ctl.entries.get(st.cursor)?;
                if entry["type"] == "emit_inbound" && entry["frame"]["type"] == "sdk.event" {
                    let event = ctl.materialize(&entry["frame"]["event"], &st.ids);
                    ctl.advance(st);
                    return Some(Some(event));
                }
                if entry["type"] == "runtime_exit" {
                    if entry["status"] != "success" {
                        ctl.fail(st, format!("runtime_exit {entry}"));
                        return Some(None);
                    }
                    ctl.advance(st);
                }
                None
            })
            .await
        }
    }

    /// T3's `replayValueMatches`.
    fn matches(expected: &Value, actual: &Value) -> bool {
        match expected {
            Value::String(s) if s == "<any>" || s == "<workspace>" => true,
            Value::Array(e) => actual.as_array().is_some_and(|a| a.len() == e.len() && e.iter().zip(a).all(|(x, y)| matches(x, y))),
            Value::Object(e) => actual.is_object() && e.iter().all(|(k, v)| matches(v, actual.get(k).unwrap_or(&Value::Null)) && (actual.get(k).is_some() || v == "<any>")),
            other => other == actual,
        }
    }

    fn decode(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'%' if i + 2 < bytes.len() => {
                    out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap_or(b'?'));
                    i += 3;
                }
                b'+' => {
                    out.push(b' ');
                    i += 1;
                }
                b => {
                    out.push(b);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// T3's `operationOf`: the SDK operation of an HTTP request.
    fn operation(method: &Method, path: &str, query: &serde_json::Map<String, Value>, body: Option<Value>) -> Value {
        let q = Value::Object(query.clone());
        let merge = |base: Value, extra: &Value| {
            let mut out = base.as_object().cloned().unwrap_or_default();
            if let Some(e) = extra.as_object() {
                out.extend(e.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.clone(), v.clone())));
            }
            Value::Object(out)
        };
        let body = body.unwrap_or(Value::Null);
        let m = method.as_str();
        match (m, path) {
            ("GET", "/api/event") => return json!({ "type": "event.subscribe" }),
            ("GET", "/api/model") => return json!({ "type": "model.list", "input": q }),
            ("GET", "/api/agent") => return json!({ "type": "agent.list", "input": q }),
            ("GET", "/api/command") => return json!({ "type": "command.list", "input": q }),
            ("GET", "/api/skill") => return json!({ "type": "skill.list", "input": q }),
            ("POST", "/api/session") => return json!({ "type": "session.create", "input": body }),
            ("GET", "/api/session/active") => return json!({ "type": "session.active" }),
            _ => {}
        }
        if let Some(server) = path.strip_prefix("/api/experimental/mcp/") {
            let input = merge(merge(json!({ "server": decode(server) }), &q), &body);
            return json!({ "type": if m == "PUT" { "mcp.add" } else { "mcp.remove" }, "input": input });
        }
        if let Some(rest) = path.strip_prefix("/api/experimental/session/")
            && let Some((session, key)) = rest.split_once("/instructions/entries/")
        {
            return json!({ "type": "session.instructions.entry.put", "input": merge(json!({ "sessionID": decode(session), "key": decode(key) }), &body) });
        }
        if let Some(rest) = path.strip_prefix("/api/session/") {
            let (session, rest) = rest.split_once('/').map(|(s, r)| (s, format!("/{r}"))).unwrap_or((rest, String::new()));
            let input = merge(merge(json!({ "sessionID": decode(session) }), &q), &body);
            let kind = match (m, rest.as_str()) {
                ("GET", "") => "session.get",
                ("DELETE", "") => "session.remove",
                ("PATCH", "") => "session.update",
                ("POST", "/prompt") => "session.prompt",
                ("POST", "/command") => "session.command",
                ("POST", "/compact") => "session.compact",
                ("POST", "/interrupt") => "session.interrupt",
                ("POST", "/model") => "session.switchModel",
                ("POST", "/agent") => "session.switchAgent",
                ("POST", "/move") => "session.move",
                ("POST", "/synthetic") => "session.synthetic",
                ("POST", "/fork") => "session.fork",
                ("POST", "/revert/stage") => "session.revert.stage",
                ("POST", "/revert/commit") => "session.revert.commit",
                ("DELETE", "/revert") => "session.revert.clear",
                ("GET", "/message") => "message.list",
                ("GET", "/permission") => "permission.list",
                ("GET", "/form") => "session.form.list",
                _ => "",
            };
            if !kind.is_empty() {
                return json!({ "type": kind, "input": input });
            }
            if let Some(inbox) = rest.strip_prefix("/inbox/").filter(|_| m == "DELETE") {
                return json!({ "type": "session.inbox.cancel", "input": merge(input, &json!({ "inboxID": decode(inbox) })) });
            }
            if let Some(form) = rest.strip_prefix("/form/") {
                let (id, reply) = form.split_once('/').map(|(f, r)| (f, r == "reply")).unwrap_or((form, false));
                let input = merge(input.clone(), &json!({ "formID": decode(id) }));
                if m == "POST" && reply {
                    return json!({ "type": "session.form.reply", "input": input });
                }
                if m == "DELETE" {
                    return json!({ "type": "session.form.cancel", "input": input });
                }
            }
            if let Some(id) = rest.strip_prefix("/permission/").and_then(|r| r.strip_suffix("/reply")).filter(|_| m == "POST") {
                return json!({ "type": "permission.reply", "input": merge(input, &json!({ "requestID": decode(id) })) });
            }
        }
        json!({ "type": format!("{m} {path}"), "input": merge(q, &json!({ "body": body })) })
    }

    async fn handle(State(ctl): State<Arc<Ctl>>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> axum::response::Response {
        if headers.get("authorization").and_then(|h| h.to_str().ok()) != Some(ctl.auth.as_str()) {
            let mut st = ctl.state.lock().unwrap();
            ctl.fail(&mut st, format!("{method} {uri} without the server password"));
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let path = uri.path().to_string();
        if method == Method::GET && path == "/api/info" {
            return axum::Json(json!({ "version": "2.0.18", "pid": std::process::id() })).into_response();
        }
        let mut query = serde_json::Map::new();
        for pair in uri.query().unwrap_or("").split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            query.insert(decode(k), json!(decode(v)));
        }
        let body = (!body.is_empty()).then(|| serde_json::from_slice::<Value>(&body).unwrap_or(json!("<not json>")));
        let op = operation(&method, &path, &query, body);
        ctl.write(json!({ "op": op }));
        let kind = op["type"].as_str().unwrap_or("").to_string();
        if kind != "event.subscribe" {
            ctl.until_inbound_delivered().await;
        }
        if let Err(e) = ctl.expect(&op).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
        if kind == "event.subscribe" {
            let stream = futures_util::stream::unfold(ctl.clone(), |ctl| async move {
                let event = ctl.next_event().await?;
                Some((Ok::<_, std::convert::Infallible>(Bytes::from(format!("data: {event}\n\n"))), ctl))
            });
            return ([("content-type", "text/event-stream")], Body::from_stream(stream)).into_response();
        }
        match ctl.response(&kind).await {
            Ok(Value::Null) => StatusCode::NO_CONTENT.into_response(),
            Ok(data) if data.get("status").is_some() && data.get("body").is_some() && data.as_object().is_some_and(|o| o.len() == 2) => {
                let status = StatusCode::from_u16(data["status"].as_u64().unwrap_or(500) as u16).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                (status, axum::Json(data["body"].clone())).into_response()
            }
            Ok(data) => axum::Json(data).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode")
}

/// A `ProviderConfig` whose `opencode` is the fake replaying `transcript`.
fn fake_config(transcript: &Path, log: &Path, work: &Path, version: &str) -> ProviderConfig {
    let exe = std::env::current_exe().unwrap();
    let script = format!(
        r#"if [ "$1" = --version ]; then echo "{version}"; exit 0; fi
ROLL_FAKE_OPENCODE_ARGS="$*" exec "$0" fake_opencode --exact --nocapture -q"#
    );
    ProviderConfig {
        binary: Some("/bin/sh".into()),
        args: vec!["-c".into(), script, exe.display().to_string()],
        env: vec![
            (TRANSCRIPT.into(), transcript.display().to_string()),
            (LOG.into(), log.display().to_string()),
            (WORK.into(), work.display().to_string()),
            ("OPENCODE_SERVER_PASSWORD".into(), "inherited-1x".into()),
        ],
        ..ProviderConfig::default()
    }
}

fn session_config(cwd: &Path) -> SessionConfig {
    SessionConfig {
        cwd: cwd.to_path_buf(),
        model: Some("opencode/big-pickle".into()),
        effort: None,
        options: Default::default(),
        runtime_mode: RuntimeMode::FullAccess,
        interaction_mode: InteractionMode::Default,
        resume: None,
        resume_at: None,
        fork: None,
        env: Vec::new(),
        mcp: Vec::new(),
        instructions: Some("Be brief.".into()),
    }
}

struct Harness {
    dir: tempfile::TempDir,
    provider: OpenCode,
    session: Box<dyn Session>,
    rx: mpsc::Receiver<ProviderEvent>,
    events: Vec<ProviderEvent>,
}

fn transcript_path(name: &str) -> PathBuf {
    match name.starts_with('/') {
        true => PathBuf::from(name),
        false => fixtures().join(format!("{name}.ndjson")),
    }
}

impl Harness {
    async fn open(transcript: &str) -> Self {
        Self::open_with(transcript, |_| {}).await
    }

    async fn open_with(transcript: &str, tweak: impl FnOnce(&mut SessionConfig)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();
        let provider = OpenCode::new(fake_config(&transcript_path(transcript), &dir.path().join("log.ndjson"), &work, "2.0.18"));
        let mut cfg = session_config(&work);
        tweak(&mut cfg);
        let (session, rx) = provider.start(cfg).await.expect("start");
        Self { dir, provider, session, rx, events: Vec::new() }
    }

    fn work(&self) -> PathBuf {
        self.dir.path().join("work")
    }

    async fn until(&mut self, what: &str, pred: impl Fn(&ProviderEvent) -> bool) -> ProviderEvent {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let event = tokio::time::timeout_at(deadline, self.rx.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}; got {:#?}\nlog: {:#?}", self.events, self.log()))
                .unwrap_or_else(|| panic!("events ended waiting for {what}"));
            self.events.push(event.clone());
            if pred(&event) {
                return event;
            }
            if let ProviderEvent::Exited { error } = &event {
                panic!("opencode exited waiting for {what}: {error:?}");
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

    fn log(&self) -> Vec<Value> {
        read_log(&self.dir.path().join("log.ndjson"))
    }

    /// Close the session and return every event (ending with `Exited`) and every operation the
    /// adapter sent; the replay must have reached the transcript's end without a failure.
    async fn finish(mut self) -> (Vec<ProviderEvent>, Vec<Value>) {
        self.session.close().await;
        self.until("exit", |e| matches!(e, ProviderEvent::Exited { .. })).await;
        let log = self.log();
        assert_replayed(&log);
        let work = self.work();
        drop(self.provider);
        (normalize(self.events, &work), ops(&log))
    }
}

fn read_log(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn assert_replayed(log: &[Value]) {
    let failures: Vec<&Value> = log.iter().filter(|l| l.get("failure").is_some()).collect();
    assert!(failures.is_empty(), "the replay failed: {failures:#?}");
    assert!(log.iter().any(|l| l.get("complete").is_some()), "the replay didn't reach the transcript's end: {log:#?}");
}

fn ops(log: &[Value]) -> Vec<Value> {
    log.iter().filter_map(|l| l.get("op").cloned()).collect()
}

fn sent<'a>(ops: &'a [Value], kind: &str) -> Vec<&'a Value> {
    ops.iter().filter(|o| o["type"] == kind).map(|o| &o["input"]).collect()
}

/// roll's own ids are random and the workspace is a temporary directory: name them by order.
fn normalize(events: Vec<ProviderEvent>, work: &Path) -> Vec<ProviderEvent> {
    let mut names: HashMap<String, String> = HashMap::new();
    let work = work.display().to_string();
    events
        .into_iter()
        .map(|e| {
            let mut text = serde_json::to_string(&event_json(&e)).unwrap().replace(&work, "<work>");
            for prefix in ["msg_roll_turn_", "msg_roll_steer_"] {
                let mut at = 0;
                while let Some(i) = text[at..].find(prefix).map(|i| i + at) {
                    let start = i + prefix.len();
                    let tail: String = text[start..].chars().take_while(char::is_ascii_alphanumeric).collect();
                    if tail.len() != 20 {
                        at = start;
                        continue;
                    }
                    let n = names.len() + 1;
                    let name = names.entry(format!("{prefix}{tail}")).or_insert_with(|| format!("{prefix}#{n}")).clone();
                    text.replace_range(i..start + tail.len(), &name);
                    at = i + name.len();
                }
            }
            from_json(&serde_json::from_str(&text).unwrap(), e)
        })
        .collect()
}

/// Put normalized ids back into an event (only the fields that can hold them).
fn from_json(v: &Value, original: ProviderEvent) -> ProviderEvent {
    match original {
        ProviderEvent::TurnRef { .. } => ProviderEvent::TurnRef { id: v["turn_ref"].as_str().unwrap().into() },
        ProviderEvent::Item(mut i) => {
            i.id = v["item"]["id"].as_str().unwrap().into();
            i.body = serde_json::from_value(v["item"]["body"].clone()).unwrap();
            ProviderEvent::Item(i)
        }
        ProviderEvent::Request { id, item, .. } => ProviderEvent::Request { id, item, body: serde_json::from_value(v["request"]["body"].clone()).unwrap() },
        ProviderEvent::TextDelta { item, .. } => ProviderEvent::TextDelta { item, text: v["text_delta"]["text"].as_str().unwrap().into() },
        ProviderEvent::OutputDelta { item, .. } => ProviderEvent::OutputDelta { item, text: v["output_delta"]["text"].as_str().unwrap().into() },
        other => other,
    }
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

/// The completed messages of the turn's own session, in order.
fn messages(events: &[ProviderEvent]) -> Vec<String> {
    items(events)
        .into_iter()
        .filter(|i| i.status == NodeStatus::Completed && i.parent.is_none())
        .filter_map(|i| if let NodeBody::Message { text } = &i.body { Some(text.clone()) } else { None })
        .collect()
}

fn turn_refs(events: &[ProviderEvent]) -> Vec<String> {
    events.iter().filter_map(|e| if let ProviderEvent::TurnRef { id } = e { Some(id.clone()) } else { None }).collect()
}

fn last_usage(events: &[ProviderEvent]) -> agent_proto::Usage {
    events.iter().rev().find_map(|e| if let ProviderEvent::Usage(u) = e { Some(*u) } else { None }).expect("usage")
}

// ---------------------------------------------------------------------------------------------
// T3's replay scenarios
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn simple() {
    let mut h = Harness::open_with("simple", |c| {
        c.model = Some("opencode/space-bunny-free".into());
        c.effort = Some("high".into());
    })
    .await;
    let work = h.work().display().to_string();
    assert_eq!(h.turn("Think carefully step by step about whether 391 is prime").await, TurnEnd::Completed);
    let (events, ops) = h.finish().await;
    golden("simple", &events);
    assert_eq!(events[0], ProviderEvent::NativeThread { id: "ses_f148cdfa1ffeIFwbC0FCx0fKpk".into() });
    assert_eq!(events[1], ProviderEvent::Model { model: "opencode/space-bunny-free".into() });
    assert_eq!(messages(&events), ["√391 ≈ 19.8, so I need to test primes 2, 3, 5, 7, 11, 13, 17, 19: it's odd, digits sum to 13, doesn't end in 0/5, and 7×55=385, 11×35=385, 13×30=390 leave remainders — but 17×23 = 391 exactly, so the search stops there.\n\n391 is not prime: it's the product 17 × 23."]);
    assert!(items(&events).iter().any(|i| matches!(&i.body, NodeBody::Reasoning { text } if text.ends_with("391 is composite.") && i.status == NodeStatus::Completed)));
    let usage = last_usage(&events);
    assert_eq!((usage.input_tokens, usage.cached_tokens, usage.output_tokens, usage.cost_usd), (8701 + 489, 489, 113 + 147, None));
    assert!(events.iter().any(|e| matches!(e, ProviderEvent::Context(c) if c.used_tokens == 8701 + 489 + 113 && c.max_tokens.is_some())));
    assert_eq!(turn_refs(&events), ["msg_roll_turn_#1"]);
    assert_eq!(events.last(), Some(&ProviderEvent::Exited { error: None }));

    // What the adapter sent: T3's session for full access, the model and variant, roll's
    // instructions entry, and a prompt with the turn's own id.
    let create = sent(&ops, "session.create")[0];
    assert_eq!(create["location"], json!({ "directory": work }));
    assert_eq!(create["model"], json!({ "providerID": "opencode", "id": "space-bunny-free", "variant": "high" }));
    assert_eq!(create["permissions"], json!([{ "action": "*", "resource": "*", "effect": "allow" }]));
    assert_eq!(sent(&ops, "session.instructions.entry.put")[0]["value"], "Be brief.");
    let prompt = sent(&ops, "session.prompt")[0];
    assert!(prompt["id"].as_str().unwrap().starts_with("msg_roll_turn_"));
    assert!(prompt.get("delivery").is_none());
}

#[tokio::test]
async fn tool_call() {
    let mut h = Harness::open("tool_call").await;
    assert_eq!(h.turn("Run a shell command").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("tool_call", &events);
    let tools: Vec<_> = latest(&events).into_values().filter_map(|i| if let NodeBody::Tool(t) = i.body { Some((i.status, t)) } else { None }).collect();
    assert!(!tools.is_empty());
    let (status, shell) = tools.iter().find(|(_, t)| t.kind == ToolKind::Command).expect("a command");
    assert_eq!(*status, NodeStatus::Completed);
    assert!(shell.output.is_some());
}

#[tokio::test]
async fn permission() {
    let mut h = Harness::open_with("permission", |c| c.runtime_mode = RuntimeMode::Ask).await;
    h.session.send(TurnInput { text: "Run echo FIRST, then echo SECOND".into(), ..Default::default() }).await.unwrap();
    let mut asked = Vec::new();
    for decision in [Decision::Accept, Decision::Decline, Decision::Decline, Decision::Decline] {
        let (id, item, body) = h.request().await;
        let RequestBody::Approval(a) = &body else { panic!("{body:?}") };
        assert_eq!(a.action, ApprovalAction::Command);
        assert_eq!(a.decisions, [Decision::Accept, Decision::AcceptForSession, Decision::Decline, Decision::Cancel]);
        asked.push((a.command.clone().unwrap(), item.is_some()));
        h.session.respond(&id, &Response::Decision { decision, message: None }).await.unwrap();
    }
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let (events, ops) = h.finish().await;
    golden("permission", &events);
    assert_eq!(asked[0], ("echo FIRST".to_string(), true));
    assert_eq!(asked[1].0, "echo SECOND");
    // Supervised rules, with the build agent's own path allows.
    let rules = sent(&ops, "session.create")[0]["permissions"].as_array().unwrap().clone();
    assert_eq!(rules[..3], [json!({"action": "shell", "resource": "*", "effect": "ask"}), json!({"action": "edit", "resource": "*", "effect": "ask"}), json!({"action": "external_directory", "resource": "*", "effect": "ask"})]);
    assert!(rules[3..].iter().all(|r| r["effect"] == "allow" && r["action"] == "external_directory"));
    let replies = sent(&ops, "permission.reply");
    assert_eq!(replies[0]["decision"], "once");
    assert!(replies[1..].iter().all(|r| r["decision"] == "reject" && r["message"] == "The user declined this request."));
    let notes = sent(&ops, "session.synthetic");
    assert_eq!(notes.len(), 3);
    assert!(notes[0]["text"].as_str().unwrap().starts_with("The user declined the shell request (echo SECOND)."));
}

#[tokio::test]
async fn question() {
    let mut h = Harness::open("question").await;
    h.session.send(TurnInput { text: "Ask me my favourite colour".into(), ..Default::default() }).await.unwrap();
    let (id, _, body) = h.request().await;
    let RequestBody::Questions { questions } = &body else { panic!("{body:?}") };
    assert_eq!(questions[0].header, "Color preference");
    assert_eq!(questions[0].question, "Which color do you prefer?");
    assert_eq!(questions[0].options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), ["Red", "Blue"]);
    h.session.respond(&id, &Response::Answers { answers: BTreeMap::from([("q0".into(), "blue".into())]) }).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    let (events, ops) = h.finish().await;
    golden("question", &events);
    assert_eq!(sent(&ops, "session.form.reply")[0]["answer"], json!({ "q0": "blue" }));
}

#[tokio::test]
async fn interrupt() {
    let mut h = Harness::open("interrupt").await;
    h.session.send(TurnInput { text: "Run sleep 60".into(), ..Default::default() }).await.unwrap();
    h.until("the shell call", |e| matches!(e, ProviderEvent::Item(i) if matches!(&i.body, NodeBody::Tool(t) if t.title.starts_with("sleep 60")))).await;
    h.session.interrupt().await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Interrupted);
    let (events, _) = h.finish().await;
    golden("interrupt", &events);
    let shell = latest(&events).into_values().find(|i| matches!(&i.body, NodeBody::Tool(t) if t.kind == ToolKind::Command)).unwrap();
    assert_eq!(shell.status, NodeStatus::Interrupted);
}

#[tokio::test]
async fn inbox_steer() {
    let mut h = Harness::open("inbox").await;
    h.session.send(TurnInput { text: "Run sleep 12 && echo A, then reply DONE_A".into(), ..Default::default() }).await.unwrap();
    h.until("the shell call", |e| matches!(e, ProviderEvent::Item(i) if matches!(&i.body, NodeBody::Tool(_)))).await;
    h.session.steer(TurnInput { text: "Also mention the word STEERED in your final reply.".into(), ..Default::default() }).await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    assert_eq!(h.turn("Reply exactly QUEUED_B.").await, TurnEnd::Completed);
    let (events, ops) = h.finish().await;
    golden("inbox", &events);
    assert_eq!(messages(&events), ["DONE_A\n\nSTEERED", "QUEUED_B"]);
    let prompts = sent(&ops, "session.prompt");
    assert_eq!(prompts[1]["delivery"], "steer");
    assert!(prompts[1]["id"].as_str().unwrap().starts_with("msg_roll_steer_"));
    assert_eq!(turn_refs(&events), ["msg_roll_turn_#1", "msg_roll_turn_#2"]);
}

#[tokio::test]
async fn compaction() {
    let mut h = Harness::open("compaction").await;
    assert_eq!(h.turn("Remember the codeword PAPAYA. Reply OK.").await, TurnEnd::Completed);
    h.session.compact().await.unwrap();
    assert_eq!(h.turn_end().await, TurnEnd::Completed);
    assert_eq!(h.turn("What was the codeword? One word.").await, TurnEnd::Completed);
    let (events, ops) = h.finish().await;
    golden("compaction", &events);
    let compaction = latest(&events).into_values().find(|i| matches!(i.body, NodeBody::Compaction { .. })).expect("a compaction");
    assert_eq!(compaction.status, NodeStatus::Completed);
    assert!(matches!(compaction.body, NodeBody::Compaction { before_tokens: Some(b), .. } if b == 8749 + 487 + 8));
    assert_eq!(messages(&events).last().unwrap(), "PAPAYA");
    assert!(sent(&ops, "session.compact")[0]["id"].as_str().unwrap().starts_with("msg_roll_turn_"));
    assert_eq!(turn_refs(&events).len(), 3);
}

#[tokio::test]
async fn fork() {
    let mut h = Harness::open("fork").await;
    assert_eq!(h.turn("Reply exactly ONE.").await, TurnEnd::Completed);
    assert_eq!(h.turn("Reply exactly TWO.").await, TurnEnd::Completed);
    let refs = turn_refs(&h.events);
    let mut cfg = session_config(&h.work());
    cfg.fork = Some(ForkFrom { thread: "ses_f1484ea8affe5aTo8CcgbGIszj".into(), at: ForkAt::Turn(refs[0].clone()) });
    let (fork, mut rx) = h.provider.start(cfg).await.expect("fork");
    fork.send(TurnInput { text: "What did you reply last time? Answer in one word.".into(), ..Default::default() }).await.unwrap();
    let mut forked = Vec::new();
    loop {
        let e = tokio::time::timeout(Duration::from_secs(30), rx.recv()).await.expect("the fork's turn").expect("events");
        let end = matches!(e, ProviderEvent::TurnEnded(_));
        forked.push(e);
        if end {
            break;
        }
    }
    fork.close().await;
    let (events, ops) = h.finish().await;
    golden("fork", &events);
    golden("fork_copy", &normalize(forked.clone(), Path::new("/nowhere")));
    assert_eq!(forked[0], ProviderEvent::NativeThread { id: "ses_f1484db83ffeLGtrRCFimo1H0e".into() });
    assert_eq!(messages(&forked), ["ONE"]);
    // The fork cuts before the second turn's prompt, and the copy gets this session's rules.
    let prompts = sent(&ops, "session.prompt");
    assert_eq!(sent(&ops, "session.fork")[0]["before"], prompts[1]["id"]);
    assert_eq!(sent(&ops, "session.update")[0]["permissions"], json!([{ "action": "*", "resource": "*", "effect": "allow" }]));
}

#[tokio::test]
async fn revert() {
    let mut h = Harness::open("revert").await;
    assert_eq!(h.turn("Create a file named reverted.txt containing ALPHA").await, TurnEnd::Completed);
    assert_eq!(h.turn("Overwrite reverted.txt so it contains BETA").await, TurnEnd::Completed);
    h.session.rollback(1).await.unwrap();
    let (events, ops) = h.finish().await;
    golden("revert", &events);
    let prompts = sent(&ops, "session.prompt");
    let stage = sent(&ops, "session.revert.stage")[0];
    assert_eq!(stage["messageID"], prompts[1]["id"]);
    assert_eq!(stage["files"], false);
    assert_eq!(sent(&ops, "session.revert.commit").len(), 1);
}

#[tokio::test]
async fn command() {
    let mut h = Harness::open_with("command", |c| c.model = Some("openrouter/deepseek/deepseek-v4-flash".into())).await;
    assert_eq!(h.turn("/hello WORLD").await, TurnEnd::Completed);
    let (events, ops) = h.finish().await;
    golden("command", &events);
    assert_eq!(messages(&events), ["HELLO WORLD"]);
    assert_eq!(sent(&ops, "session.command")[0], &json!({ "sessionID": "ses_f0f27cc49ffe6wl0jdiu4kO4FC", "name": "hello", "text": "WORLD" }));
    // A command's turn is named by the message OpenCode made of it.
    assert_eq!(turn_refs(&events), ["msg_0f0d833f4001DhEyXmiqfz0kcF"]);
    assert_eq!(events.iter().find(|e| matches!(e, ProviderEvent::Usage(_))), Some(&ProviderEvent::Usage(agent_proto::Usage { input_tokens: 9421, output_tokens: 20, cached_tokens: 0, cost_usd: Some(0.00132454) })));
}

#[tokio::test]
async fn subagent() {
    let mut h = Harness::open("subagent").await;
    assert_eq!(h.turn("Use the subagent tool to delegate to the explore subagent").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("subagent", &events);
    let all = latest(&events);
    let call = &all["call_function_okrf3eahp2li_1"];
    let NodeBody::Subagent { agent, prompt, result, model, .. } = &call.body else { panic!("{call:?}") };
    assert_eq!((agent.as_str(), call.status), ("explore", NodeStatus::Completed));
    assert!(prompt.starts_with("List the files"));
    assert!(result.as_deref().unwrap().starts_with("The current directory"), "{result:?}");
    assert_eq!(model.as_deref(), Some("opencode/big-pickle"));
    // The child's work hangs off the call, scoped by the child's session.
    let child: Vec<_> = all.values().filter(|i| i.parent.as_deref() == Some("call_function_okrf3eahp2li_1")).collect();
    assert!(child.iter().any(|i| i.id == "ses_f14861bfdffea1l0D7wDj5no7d:call_function_8gjur9skkejo_2" && matches!(&i.body, NodeBody::Tool(t) if t.kind == ToolKind::Search)));
    assert!(child.iter().any(|i| matches!(&i.body, NodeBody::Tool(t) if t.kind == ToolKind::Read && t.output.as_deref().is_some_and(|o| o.contains("hello.txt")))));
    assert_eq!(messages(&events).last().unwrap(), "The current directory contains a single file, `hello.txt` (plus the `.git` directory).");
}

#[tokio::test]
async fn missing_step_usage() {
    let mut h = Harness::open_with("missing_step_usage", |c| {
        c.model = Some("opencode/space-bunny-free".into());
        c.effort = Some("high".into());
    })
    .await;
    assert_eq!(h.turn("Think about 391").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("missing_step_usage", &events);
    // The second step failed without a token report: the turn's use is the first step's.
    let usage: Vec<_> = events.iter().filter_map(|e| if let ProviderEvent::Usage(u) = e { Some(*u) } else { None }).collect();
    assert_eq!(usage.len(), 1);
    assert_eq!((usage[0].input_tokens, usage[0].output_tokens), (8701 + 489, 113 + 147));
}

#[tokio::test]
async fn background_subagent_folds_into_its_turn() {
    let mut h = Harness::open("background").await;
    assert_eq!(h.turn("Use the subagent tool with background enabled").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("background", &events);
    // The turn ends after OpenCode delivered the child's report, not when the first execution did.
    assert_eq!(messages(&events), ["PARENT_OK", "The background subagent finished and returned `CHILD_OK`."]);
    let call = &latest(&events)["call-cccf2157-e8a3-456c-a74f-fccd26fb83eb"];
    assert!(matches!(&call.body, NodeBody::Subagent { result: Some(r), .. } if r == "CHILD_OK"));
    let ended = events.iter().position(|e| matches!(e, ProviderEvent::TurnEnded(_))).unwrap();
    let child_done = events.iter().position(|e| matches!(e, ProviderEvent::Item(i) if i.parent.is_some() && matches!(&i.body, NodeBody::Message { text } if text == "CHILD_OK"))).unwrap();
    assert!(child_done < ended);
    assert_eq!(last_usage(&events).output_tokens, 76 + 5 + 14);
}

#[tokio::test]
async fn nested_background_subagents_fold_into_the_turn() {
    let mut h = Harness::open_with("nested_background", |c| c.model = Some("openrouter/deepseek/deepseek-v4-flash".into())).await;
    assert_eq!(h.turn("Chain subagents").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    golden("nested_background", &events);
    assert_eq!(messages(&events), ["ROOT_OK"]);
    let all = latest(&events);
    let middle = "ses_f08c06e25ffeF0asx24ydmo3oy:call_8272fb6a288c4f94869b6ce6";
    assert!(matches!(&all[middle].body, NodeBody::Subagent { result: Some(r), .. } if r == "GRANDCHILD_OK"));
    assert_eq!(all[middle].parent.as_deref(), Some("call_3d90a8b7d44c49f6ae053fd7"));
    // The grandchild's own work hangs off the call that started it.
    assert!(all.values().any(|i| i.parent.as_deref() == Some(middle) && matches!(&i.body, NodeBody::Tool(t) if t.title == "sleep 25")));
    let ended = events.iter().position(|e| matches!(e, ProviderEvent::TurnEnded(_))).unwrap();
    let last_child = events.iter().rposition(|e| matches!(e, ProviderEvent::Item(i) if i.parent.is_some())).unwrap();
    assert!(last_child < ended);
}

// ---------------------------------------------------------------------------------------------
// roll's own scenarios
// ---------------------------------------------------------------------------------------------

/// A transcript written for one test.
fn synthetic(dir: &Path, name: &str, entries: &[Value]) -> String {
    let path = dir.join(format!("{name}.ndjson"));
    let text: String = entries.iter().map(|e| format!("{e}\n")).collect();
    std::fs::write(&path, text).unwrap();
    path.display().to_string()
}

fn expect(kind: &str, input: Value) -> Value {
    match input {
        Value::Null => json!({ "type": "expect_outbound", "frame": { "type": kind } }),
        input => json!({ "type": "expect_outbound", "frame": { "type": kind, "input": input } }),
    }
}

fn answer(kind: &str, data: Value) -> Value {
    json!({ "type": "emit_inbound", "frame": { "type": "sdk.response", "operation": kind, "data": data } })
}

fn event(kind: &str, data: Value) -> Value {
    json!({ "type": "emit_inbound", "frame": { "type": "sdk.event", "event": { "id": "evt", "type": kind, "data": data } } })
}

fn models_answer() -> Value {
    json!({ "data": [
        { "id": "big-pickle", "providerID": "opencode", "name": "Big Pickle", "variants": [{ "id": "low" }, { "id": "medium" }, { "id": "high" }],
          "limit": { "context": 200000 }, "settings": { "apiKey": "sk-never-shown", "baseURL": "https://secret.example" },
          "headers": { "Authorization": "Bearer sk-never-shown" } },
    ]})
}

fn opening() -> Vec<Value> {
    vec![expect("event.subscribe", Value::Null), expect("model.list", json!({ "location[directory]": "<any>" })), answer("model.list", models_answer())]
}

fn exit_ok() -> Value {
    json!({ "type": "runtime_exit", "status": "success" })
}

#[tokio::test]
async fn status_lists_models_and_never_their_settings() {
    let dir = tempfile::tempdir().unwrap();
    let transcript = synthetic(
        dir.path(),
        "info",
        &[
            expect("event.subscribe", Value::Null),
            expect("model.list", json!({ "location[directory]": "<any>" })),
            answer("model.list", models_answer()),
            expect("model.list", json!({ "location[directory]": "<any>" })),
            answer("model.list", models_answer()),
            exit_ok(),
        ],
    );
    let log = dir.path().join("log.ndjson");
    let provider = OpenCode::new(fake_config(Path::new(&transcript), &log, dir.path(), "2.0.18"));
    let info = provider.info().await;
    assert!(info.installed, "{info:?}");
    assert_eq!((info.version.as_deref(), info.auth, info.name.as_str()), (Some("2.0.18"), AuthStatus::Ready, "OpenCode"));
    assert_eq!(info.models.len(), 1);
    let model = &info.models[0];
    assert_eq!((model.id.as_str(), model.name.as_str()), ("opencode/big-pickle", "Big Pickle"));
    assert_eq!(model.efforts(), ["low", "medium", "high"]);
    assert!(matches!(model.reasoning(), Some(agent_proto::ModelOption::Select { default: Some(d), .. }) if d == "medium"));
    let shown = serde_json::to_string(&info).unwrap() + &format!("{info:?}");
    assert!(!shown.contains("sk-never-shown") && !shown.contains("secret.example"));
    let lines = read_log(&log);
    assert_replayed(&lines);
    // The server got roll's own password and an empty config, never the inherited 1.x password.
    let started = &lines[0];
    assert_eq!(started["server_password_inherited"], false);
    assert_eq!(started["config_content"], "{}");
    assert!(started["args"].as_str().unwrap().starts_with("serve --hostname=127.0.0.1 --port="));
}

#[tokio::test]
async fn status_without_models_asks_for_a_login() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = vec![expect("event.subscribe", Value::Null)];
    // A cold server is read until the catalog settles; an empty one never does.
    for _ in 0..30 {
        entries.push(expect("model.list", json!({ "location[directory]": "<any>" })));
        entries.push(answer("model.list", json!({ "data": [] })));
    }
    let transcript = synthetic(dir.path(), "info", &entries);
    let provider = OpenCode::new(fake_config(Path::new(&transcript), &dir.path().join("log.ndjson"), dir.path(), "2.0.18"));
    let info = provider.info().await;
    assert_eq!((info.installed, info.auth), (true, AuthStatus::Missing));
    assert!(info.detail.unwrap().contains("opencode auth login"));
}

#[tokio::test]
async fn opencode_1x_is_left_to_the_acp_agent() {
    let dir = tempfile::tempdir().unwrap();
    let provider = OpenCode::new(fake_config(Path::new("/nonexistent"), &dir.path().join("log.ndjson"), dir.path(), "1.14.19"));
    let info = provider.info().await;
    assert!(!info.installed);
    assert_eq!(info.version.as_deref(), Some("1.14.19"));
    assert_eq!(info.detail.as_deref(), Some(agent_host::provider::opencode::NEEDS_V2));
    assert!(!dir.path().join("log.ndjson").exists(), "no server starts for 1.x");
}

#[tokio::test]
async fn resume_registers_mcp_and_switches_modes() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let mut entries = opening();
    entries.extend([
        expect("session.get", json!({ "sessionID": "ses_old" })),
        answer("session.get", json!({ "data": { "id": "ses_old", "agent": "build", "model": { "providerID": "opencode", "id": "big-pickle" },
            "permissions": [{ "action": "*", "resource": "*", "effect": "allow" }], "location": { "directory": "/elsewhere" } } })),
        expect("agent.list", json!({ "location[directory]": "<any>" })),
        answer("agent.list", json!({ "data": [
            { "id": "build", "permissions": [{ "action": "external_directory", "resource": "/tmp/opencode/*", "effect": "allow" }] },
            { "id": "plan", "permissions": [{ "action": "edit", "resource": "/plans/*", "effect": "allow" }, { "action": "edit", "resource": "*", "effect": "deny" }] },
        ]})),
        expect("session.update", json!({ "sessionID": "ses_old", "permissions": "<any>" })),
        answer("session.update", Value::Null),
        expect("session.move", json!({ "sessionID": "ses_old", "directory": "<work>" })),
        answer("session.move", Value::Null),
        expect("mcp.add", json!({ "server": "<any>", "location[directory]": "<work>",
            "config": { "type": "remote", "url": "http://127.0.0.1:9/mcp", "headers": { "authorization": "Bearer t" }, "oauth": false } })),
        answer("mcp.add", Value::Null),
        expect("session.instructions.entry.put", json!({ "sessionID": "ses_old", "key": "roll", "value": "Be brief." })),
        answer("session.instructions.entry.put", Value::Null),
        // The turn runs in plan mode: OpenCode's plan agent, then the plan rules, then the
        // effort's variant.
        expect("session.switchAgent", json!({ "sessionID": "ses_old", "agent": "plan" })),
        answer("session.switchAgent", Value::Null),
        expect("session.update", json!({ "sessionID": "ses_old", "permissions": "<any>" })),
        answer("session.update", Value::Null),
        expect("session.switchModel", json!({ "sessionID": "ses_old", "model": { "providerID": "opencode", "id": "big-pickle", "variant": "high" } })),
        answer("session.switchModel", Value::Null),
        expect("session.prompt", json!({ "sessionID": "ses_old", "id": "msg_rec", "text": "plan it" })),
        answer("session.prompt", json!({ "data": { "id": "msg_rec" } })),
        event("session.inbox.enqueued", json!({ "sessionID": "ses_old", "inboxID": "msg_rec", "item": { "type": "user", "payload": { "text": "plan it" } } })),
        event("session.execution.started", json!({ "sessionID": "ses_old" })),
        event("session.text.ended", json!({ "sessionID": "ses_old", "assistantMessageID": "msg_a", "ordinal": 0, "text": "A plan." })),
        event("session.execution.succeeded", json!({ "sessionID": "ses_old" })),
        expect("mcp.remove", json!({ "server": "<any>", "location[directory]": "<work>" })),
        answer("mcp.remove", Value::Null),
        exit_ok(),
    ]);
    let transcript = synthetic(dir.path(), "resume", &entries);
    let log = dir.path().join("log.ndjson");
    let provider = OpenCode::new(fake_config(Path::new(&transcript), &log, &work, "2.0.18"));
    let mut cfg = session_config(&work);
    cfg.resume = Some("ses_old".into());
    cfg.effort = Some("high".into());
    cfg.runtime_mode = RuntimeMode::AutoEdit;
    cfg.mcp = vec![McpServer { name: "roll".into(), url: "http://127.0.0.1:9/mcp".into(), headers: vec![("authorization".into(), "Bearer t".into())] }];
    let (session, rx) = provider.start(cfg).await.expect("resume");
    let mut h = Harness { dir, provider, session, rx, events: Vec::new() };
    h.session.set_modes(RuntimeMode::AutoEdit, InteractionMode::Plan).await.unwrap();
    assert_eq!(h.turn("plan it").await, TurnEnd::Completed);
    let (events, ops) = h.finish().await;
    assert_eq!(events[0], ProviderEvent::NativeThread { id: "ses_old".into() });
    assert_eq!(messages(&events), ["A plan."]);
    let name = sent(&ops, "mcp.add")[0]["server"].as_str().unwrap().to_string();
    assert!(name.starts_with("roll-") && name.ends_with("-roll"), "{name}");
    let prefix = name.trim_end_matches("roll");
    let mcp = [json!({ "action": "roll-*", "resource": "*", "effect": "deny" }), json!({ "action": format!("{prefix}*"), "resource": "*", "effect": "allow" })];
    let rule = |action: &str, resource: &str, effect: &str| json!({ "action": action, "resource": resource, "effect": effect });
    let updates = sent(&ops, "session.update");
    let mut auto = vec![rule("shell", "*", "ask"), rule("edit", "*", "allow"), rule("external_directory", "*", "ask"), rule("external_directory", "/tmp/opencode/*", "allow")];
    auto.extend(mcp.clone());
    assert_eq!(updates[0]["permissions"], json!(auto));
    // The session's agent is OpenCode's plan agent now: its path allows (T3's `rulesFor`).
    let mut plan = vec![rule("shell", "*", "ask"), rule("edit", "*", "allow"), rule("external_directory", "*", "ask"), rule("edit", "*", "deny"), rule("edit", "/plans/*", "allow")];
    plan.extend(mcp);
    assert_eq!(updates[1]["permissions"], json!(plan));
    assert_eq!(sent(&ops, "mcp.remove")[0]["server"], name);
}


#[tokio::test]
async fn an_execution_left_running_stops_before_the_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let mut entries = opening();
    entries.extend([
        expect("session.create", json!("<any>")),
        answer("session.create", json!({ "data": { "id": "ses_1", "model": { "providerID": "opencode", "id": "big-pickle" } } })),
        expect("session.instructions.entry.put", json!({ "sessionID": "ses_1", "key": "roll" })),
        answer("session.instructions.entry.put", Value::Null),
        // OpenCode started an execution by itself (a report after an interrupted turn).
        event("session.execution.started", json!({ "sessionID": "ses_1" })),
        event("permission.asked", json!({ "id": "per_stray", "sessionID": "ses_1", "action": "shell", "resources": ["ls"] })),
        expect("permission.reply", json!({ "sessionID": "ses_1", "requestID": "per_stray", "decision": "reject" })),
        answer("permission.reply", Value::Null),
        expect("session.interrupt", json!({ "sessionID": "ses_1" })),
        answer("session.interrupt", json!({ "interrupted": true })),
        event("session.execution.interrupted", json!({ "sessionID": "ses_1" })),
        expect("session.prompt", json!({ "sessionID": "ses_1", "id": "msg_rec", "text": "go" })),
        answer("session.prompt", json!({ "data": { "id": "msg_rec" } })),
        event("session.execution.started", json!({ "sessionID": "ses_1" })),
        event("session.text.ended", json!({ "sessionID": "ses_1", "assistantMessageID": "msg_a", "ordinal": 0, "text": "gone" })),
        event("session.execution.succeeded", json!({ "sessionID": "ses_1" })),
        exit_ok(),
    ]);
    let transcript = synthetic(dir.path(), "stray", &entries);
    let provider = OpenCode::new(fake_config(Path::new(&transcript), &dir.path().join("log.ndjson"), &work, "2.0.18"));
    let (session, rx) = provider.start(session_config(&work)).await.expect("start");
    let mut h = Harness { dir, provider, session, rx, events: Vec::new() };
    // The stray's ask is refused (nothing can show it) before the turn.
    for _ in 0..100 {
        if ops(&h.log()).iter().any(|o| o["type"] == "permission.reply") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(h.turn("go").await, TurnEnd::Completed);
    let (events, _) = h.finish().await;
    assert_eq!(messages(&events), ["gone"]);
    assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Request { .. })));
}
