//! A live OpenCode session: one OpenCode session on a shared server, its events from the
//! server's stream (T3's v2 `adapter.ts`: `ensureThread`, `resumeThread`, `forkThread`,
//! `startTurn`, `steerTurn`, `interruptTurn`, `respondToRuntimeRequest`, `rollbackThread`).
//!
//! The pump feeds events through the [`Translator`] under one lock and queues what comes out in
//! order; a forwarder moves them to the session's bounded channel.

use super::http::{self, Client};
use super::server::{Inbound, Lease, Server};
use super::translate::{Action, Asked, STEER_PREFIX, TURN_PREFIX, Translator, form_answer};
use super::wire::{self, Effect, ModelRef, Rule};
use crate::provider::{ForkAt, ProviderEvent, Session, SessionConfig, TurnEnd, TurnInput};
use agent_proto::{Decision, InteractionMode, Response, RuntimeMode};
use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use base64::Engine;
use rand::Rng;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// The instructions entry roll writes (T3 writes `t3-code`).
const INSTRUCTIONS_KEY: &str = "roll";
/// T3's `INTERRUPT_TIMEOUT`.
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a new turn waits for an execution left running to stop.
const STRAY_WAIT: Duration = Duration::from_secs(5);
/// T3's `INVENTORY_TIMEOUT`.
const INVENTORY_TIMEOUT: Duration = Duration::from_secs(5);
/// T3's `DECLINED`: a rejection without a message stops the whole execution (Cancel).
const DECLINED: &str = "The user declined this request.";
const STILL_STOPPING: &str = "OpenCode is still stopping the previous turn. Send the message again in a moment.";

pub struct OpenCodeSession {
    inner: Arc<Inner>,
}

struct Inner {
    server: Arc<Server>,
    lease: Mutex<Option<Lease>>,
    session: String,
    dir: String,
    state: Mutex<State>,
    queue: mpsc::UnboundedSender<ProviderEvent>,
    closing: AtomicBool,
    changed: tokio::sync::Notify,
    /// roll's MCP servers registered for this session.
    mcp: Vec<String>,
    files: PathBuf,
}

struct State {
    tr: Translator,
    runtime: RuntimeMode,
    interaction: InteractionMode,
    agent: String,
    /// The model the session runs now.
    model: Option<ModelRef>,
    /// The model roll asks for.
    want: Option<ModelRef>,
    /// The rules last written to the session.
    rules: Option<Vec<Rule>>,
    /// "Accept for session" grants, by asking session.
    grants: HashMap<String, Vec<Rule>>,
    mcp_prefix: Option<String>,
}

/// T3's `sessionRules`: the runtime mode's rules, the session's grants, plan mode's edit deny,
/// the agents' own path allows, then only this session's roll MCP servers.
pub fn session_rules(runtime: RuntimeMode, interaction: InteractionMode, paths: &[Rule], grants: &[Rule], mcp_prefix: Option<&str>) -> Vec<Rule> {
    let mut rules = match runtime {
        RuntimeMode::FullAccess => vec![Rule::any("*", Effect::Allow)],
        mode => vec![
            Rule::any("shell", Effect::Ask),
            Rule::any("edit", if mode == RuntimeMode::AutoEdit { Effect::Allow } else { Effect::Ask }),
            Rule::any("external_directory", Effect::Ask),
        ],
    };
    if runtime != RuntimeMode::FullAccess {
        rules.extend(grants.iter().cloned());
    }
    if interaction == InteractionMode::Plan {
        rules.push(Rule::any("edit", Effect::Deny));
    }
    rules.extend(paths.iter().cloned());
    if let Some(prefix) = mcp_prefix {
        // The last match wins: every session's roll servers denied, then this one's allowed.
        rules.push(Rule::any("roll-*", Effect::Deny));
        rules.push(Rule::any(&format!("{prefix}*"), Effect::Allow));
    }
    rules
}

/// T3's `rulesFor`: the agents' path rules come from the server when the mode needs them.
async fn rules_for(server: &Server, dir: &str, agent: &str, runtime: RuntimeMode, interaction: InteractionMode, grants: &[Rule], mcp_prefix: Option<&str>) -> Vec<Rule> {
    let plan = interaction == InteractionMode::Plan;
    let mut paths: Vec<Rule> = Vec::new();
    if runtime != RuntimeMode::FullAccess || plan {
        let known = server.agent_paths(dir).await;
        let agents: &[&str] = if plan { &[agent, "plan"] } else { &[agent] };
        for rule in agents.iter().filter_map(|a| known.get(*a)).flatten() {
            match paths.iter_mut().find(|r| r.action == rule.action && r.resource == rule.resource) {
                Some(seen) => *seen = rule.clone(),
                None => paths.push(rule.clone()),
            }
        }
    }
    session_rules(runtime, interaction, &paths, grants, mcp_prefix)
}

fn new_id(prefix: &str) -> String {
    let mut rng = rand::thread_rng();
    let tail: String = (0..20).map(|_| rng.sample(rand::distributions::Alphanumeric) as char).collect();
    format!("{prefix}{tail}")
}

fn agent_for(interaction: InteractionMode) -> &'static str {
    match interaction {
        InteractionMode::Plan => "plan",
        InteractionMode::Default => "build",
    }
}

fn session_path(id: &str, rest: &str) -> String {
    format!("/api/session/{}{rest}", http::encode(id))
}

/// A session answer, whether or not it comes wrapped in `data`.
fn session_of(value: Option<Value>, what: &str) -> anyhow::Result<wire::Session> {
    let value = value.with_context(|| format!("OpenCode answered {what} without a session"))?;
    let inner = match value.get("data") {
        Some(data) if data.is_object() => data.clone(),
        _ => value,
    };
    serde_json::from_value(inner).with_context(|| format!("OpenCode's answer to {what}"))
}

/// The messages that start turns, oldest first: user prompts (not roll's steers) and
/// compactions. A rollback or fork cuts at one of them.
async fn boundaries(client: &Client, session: &str) -> anyhow::Result<Vec<String>> {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..100 {
        let query = match &cursor {
            Some(c) => format!("?limit=100&cursor={}", http::encode(c)),
            None => "?limit=100".to_string(),
        };
        let page: wire::MessagePage = client.get(&session_path(session, &format!("/message{query}"))).await?;
        let empty = page.data.is_empty();
        all.extend(page.data);
        cursor = page.cursor.and_then(|c| c.next).filter(|c| !c.is_empty() && !empty);
        if cursor.is_none() {
            break;
        }
    }
    let mut starts: Vec<(f64, String)> = all
        .into_iter()
        .filter(|m| (m.kind == "user" && !m.id.starts_with(STEER_PREFIX)) || m.kind == "compaction")
        .map(|m| (m.time.and_then(|t| t.created).unwrap_or(0.0), m.id))
        .collect();
    starts.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    Ok(starts.into_iter().map(|(_, id)| id).collect())
}

/// Where a fork of `session` cuts (`before` that message), or `None` to copy everything.
async fn fork_before(client: &Client, session: &str, at: &ForkAt) -> anyhow::Result<Option<String>> {
    match at {
        ForkAt::Latest => Ok(None),
        ForkAt::DropLast(0) => Ok(None),
        ForkAt::Turn(turn) => {
            let starts = boundaries(client, session).await?;
            let i = starts.iter().position(|s| s == turn).with_context(|| format!("OpenCode's conversation has no turn {turn}"))?;
            Ok(starts.get(i + 1).cloned())
        }
        ForkAt::DropLast(n) => {
            let starts = boundaries(client, session).await?;
            let i = starts.len().checked_sub(*n as usize).context("OpenCode's conversation has fewer turns than the fork drops")?;
            Ok(starts.get(i).cloned())
        }
    }
}

/// Open the session `cfg` asks for on a borrowed server.
pub(super) async fn open(lease: Lease, cfg: &SessionConfig) -> anyhow::Result<(OpenCodeSession, mpsc::Receiver<ProviderEvent>)> {
    let server = lease.server.clone();
    let client = &server.client;
    let dir = cfg.cwd.to_string_lossy().into_owned();
    let want = match &cfg.model {
        Some(m) => Some(ModelRef::parse(m, cfg.effort.as_deref()).with_context(|| format!("OpenCode model '{m}' must use provider/model format"))?),
        None => None,
    };
    let mcp_prefix = (!cfg.mcp.is_empty()).then(|| format!("roll-{:08x}-", rand::thread_rng().r#gen::<u32>()));
    let windows = server.windows(&dir).await;
    let (runtime, interaction) = (cfg.runtime_mode, cfg.interaction_mode);
    let native = match (&cfg.fork, &cfg.resume) {
        (Some(fork), _) => {
            let before = fork_before(client, &fork.thread, &fork.at).await?;
            let body = match before {
                Some(b) => json!({ "before": b }),
                None => json!({}),
            };
            session_of(client.post(&session_path(&fork.thread, "/fork"), &body).await?, "the fork")?
        }
        (None, Some(id)) => session_of(client.call(reqwest::Method::GET, &session_path(id, ""), None).await?, "the session")?,
        (None, None) => {
            let rules = rules_for(&server, &dir, "build", runtime, interaction, &[], mcp_prefix.as_deref()).await;
            let mut body = json!({ "location": { "directory": dir }, "permissions": rules });
            if let Some(model) = &want {
                body["model"] = json!(model);
            }
            let mut created = session_of(client.post("/api/session", &body).await?, "the new session")?;
            created.permissions = Some(rules);
            created
        }
    };
    let id = native.id.clone();
    let agent = native.agent.clone().unwrap_or_else(|| "build".into());
    let mut rules = native.permissions.clone();
    if cfg.fork.is_some() || cfg.resume.is_some() {
        // A fork or a resumed session keeps the rules it had; bring them to this session's.
        let wanted = rules_for(&server, &dir, &agent, runtime, interaction, &[], mcp_prefix.as_deref()).await;
        if rules.as_ref() != Some(&wanted) {
            client.send(reqwest::Method::PATCH, &session_path(&id, ""), Some(&json!({ "permissions": wanted }))).await?;
        }
        rules = Some(wanted);
        if native.location.as_ref().is_some_and(|l| l.directory != dir) {
            client.send(reqwest::Method::POST, &session_path(&id, "/move"), Some(&json!({ "directory": dir }))).await?;
        }
    }
    let mut mcp = Vec::new();
    for s in &cfg.mcp {
        let prefix = mcp_prefix.as_deref().unwrap_or("roll-");
        let clean: String = s.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        let name: String = format!("{prefix}{clean}").chars().take(64).collect();
        let headers: serde_json::Map<String, Value> = s.headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        let body = json!({ "config": { "type": "remote", "url": s.url, "headers": headers, "oauth": false } });
        let path = format!("/api/experimental/mcp/{}?{}", http::encode(&name), http::location(&dir));
        match tokio::time::timeout(INVENTORY_TIMEOUT, client.send(reqwest::Method::PUT, &path, Some(&body))).await {
            Ok(Ok(())) => mcp.push(name),
            Ok(Err(e)) => tracing::warn!(target: "agent_host::opencode", "couldn't add roll's MCP server to OpenCode: {e}"),
            Err(_) => tracing::warn!(target: "agent_host::opencode", "OpenCode didn't add roll's MCP server in time"),
        }
    }
    if let Some(text) = cfg.instructions.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        let path = format!("/api/experimental/session/{}/instructions/entries/{INSTRUCTIONS_KEY}", http::encode(&id));
        client.send(reqwest::Method::PUT, &path, Some(&json!({ "value": text }))).await?;
    }

    let (queue, queued) = mpsc::unbounded_channel();
    let (events, rx) = mpsc::channel(1024);
    let _ = queue.send(ProviderEvent::NativeThread { id: id.clone() });
    let model = native.model.clone();
    if let Some(m) = &model {
        let _ = queue.send(ProviderEvent::Model { model: m.slug() });
    }
    let (route, inbound) = mpsc::unbounded_channel();
    server.register(&id, route);
    let files = std::env::temp_dir().join(format!("roll-opencode-{}", new_id("")));
    let inner = Arc::new(Inner {
        server: server.clone(),
        lease: Mutex::new(Some(lease)),
        session: id.clone(),
        dir: dir.clone(),
        state: Mutex::new(State {
            tr: Translator::new(id, dir, mcp_prefix.clone(), windows, model.as_ref().map(ModelRef::slug)),
            runtime,
            interaction,
            agent,
            model,
            want,
            rules,
            grants: HashMap::new(),
            mcp_prefix,
        }),
        queue,
        closing: AtomicBool::new(false),
        changed: tokio::sync::Notify::new(),
        mcp,
        files,
    });
    tokio::spawn(forward(queued, events));
    tokio::spawn(pump(Arc::downgrade(&inner), inbound));
    Ok((OpenCodeSession { inner }, rx))
}

async fn forward(mut queued: mpsc::UnboundedReceiver<ProviderEvent>, events: mpsc::Sender<ProviderEvent>) {
    while let Some(event) = queued.recv().await {
        let last = matches!(event, ProviderEvent::Exited { .. });
        if events.send(event).await.is_err() || last {
            return;
        }
    }
}

async fn pump(inner: std::sync::Weak<Inner>, mut inbound: mpsc::UnboundedReceiver<Inbound>) {
    while let Some(message) = inbound.recv().await {
        let Some(inner) = inner.upgrade() else { return };
        match message {
            Inbound::Event(event) => {
                let state = &mut *inner.state.lock().unwrap();
                let actions = state.tr.on_event(event);
                inner.run_locked(actions);
            }
            Inbound::Closed(why) => {
                let _state = inner.state.lock().unwrap();
                if !inner.closing.swap(true, Ordering::Relaxed) {
                    let _ = inner.queue.send(ProviderEvent::Exited { error: Some(why) });
                }
                return;
            }
        }
        inner.changed.notify_waiters();
    }
}

impl Inner {
    /// Carry out translated actions, with the state lock held so events stay in order.
    fn run_locked(&self, actions: Vec<Action>) {
        for action in actions {
            let call = match action {
                Action::Event(event) => {
                    if !self.closing.load(Ordering::Relaxed) {
                        let _ = self.queue.send(event);
                    }
                    continue;
                }
                Action::Interrupt(session) => (reqwest::Method::POST, session_path(&session, "/interrupt"), None),
                Action::Reject { session, request } => {
                    (reqwest::Method::POST, session_path(&session, &format!("/permission/{}/reply", http::encode(&request))), Some(json!({ "decision": "reject" })))
                }
                Action::CancelForm { session, form } => (reqwest::Method::DELETE, session_path(&session, &format!("/form/{}", http::encode(&form))), None),
            };
            let client = self.server.client.clone();
            tokio::spawn(async move {
                if let Err(e) = client.send(call.0, &call.1, call.2.as_ref()).await {
                    tracing::debug!(target: "agent_host::opencode", "{e}");
                }
            });
        }
    }

    fn client(&self) -> &Client {
        &self.server.client
    }

    fn path(&self, rest: &str) -> String {
        session_path(&self.session, rest)
    }

    /// An execution OpenCode started by itself (a report after an interrupted turn) must stop
    /// before a new turn, or its end would end the new one.
    async fn settle_stray(&self) -> anyhow::Result<()> {
        if !self.state.lock().unwrap().tr.busy() {
            return Ok(());
        }
        let _ = tokio::time::timeout(INTERRUPT_TIMEOUT, self.client().send(reqwest::Method::POST, &self.path("/interrupt"), None)).await;
        let deadline = tokio::time::Instant::now() + STRAY_WAIT;
        loop {
            // Listen before looking, so an event between the two isn't missed.
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.state.lock().unwrap().tr.busy() {
                return Ok(());
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                if self.state.lock().unwrap().tr.busy() {
                    bail!(STILL_STOPPING);
                }
                return Ok(());
            }
        }
    }

    /// T3's `startTurn` setup: the agent for the interaction mode, then the rules, then the model.
    async fn prepare(&self) -> anyhow::Result<()> {
        let (agent_now, runtime, interaction, grants, prefix) = {
            let s = self.state.lock().unwrap();
            (s.agent.clone(), s.runtime, s.interaction, s.grants.get(&self.session).cloned().unwrap_or_default(), s.mcp_prefix.clone())
        };
        let agent = agent_for(interaction);
        if agent != agent_now {
            self.client().send(reqwest::Method::POST, &self.path("/agent"), Some(&json!({ "agent": agent }))).await?;
            self.state.lock().unwrap().agent = agent.into();
        }
        let rules = rules_for(&self.server, &self.dir, agent, runtime, interaction, &grants, prefix.as_deref()).await;
        if self.state.lock().unwrap().rules.as_ref() != Some(&rules) {
            self.client().send(reqwest::Method::PATCH, &self.path(""), Some(&json!({ "permissions": rules }))).await?;
            self.state.lock().unwrap().rules = Some(rules);
        }
        let switch = {
            let s = self.state.lock().unwrap();
            s.want.clone().filter(|w| !w.same(s.model.as_ref()))
        };
        if let Some(model) = switch {
            self.client().send(reqwest::Method::POST, &self.path("/model"), Some(&json!({ "model": model }))).await?;
            self.state.lock().unwrap().model = Some(model);
        }
        Ok(())
    }

    /// Images go to OpenCode as files it reads (`file://` URIs), written under the session's
    /// own temporary directory.
    fn files(&self, input: &TurnInput) -> anyhow::Result<Vec<Value>> {
        let mut files = Vec::new();
        for (n, a) in input.attachments.iter().enumerate() {
            if !a.mime.starts_with("image/") {
                bail!("OpenCode takes image attachments only, not {} ({})", a.name, a.mime);
            }
            let bytes = base64::engine::general_purpose::STANDARD.decode(a.data.as_bytes()).with_context(|| format!("{} isn't base64", a.name))?;
            std::fs::create_dir_all(&self.files)?;
            let name: String = a.name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).take(100).collect();
            let path = self.files.join(format!("{}-{n}-{name}", new_id("")));
            std::fs::write(&path, bytes)?;
            files.push(json!({ "uri": format!("file://{}", path.display()), "name": a.name }));
        }
        Ok(files)
    }

    fn end_turn(&self, end: TurnEnd) {
        let state = &mut *self.state.lock().unwrap();
        let actions = state.tr.end_turn(end);
        self.run_locked(actions);
    }

    /// Write the rules of a session that asked for an "accept for session" grant.
    async fn write_grants(&self, session: &str) -> anyhow::Result<()> {
        let (agent, runtime, interaction, grants, prefix) = {
            let s = self.state.lock().unwrap();
            (s.agent.clone(), s.runtime, s.interaction, s.grants.get(session).cloned().unwrap_or_default(), s.mcp_prefix.clone())
        };
        if session == self.session {
            let rules = rules_for(&self.server, &self.dir, &agent, runtime, interaction, &grants, prefix.as_deref()).await;
            self.client().send(reqwest::Method::PATCH, &self.path(""), Some(&json!({ "permissions": rules }))).await?;
            self.state.lock().unwrap().rules = Some(rules);
        } else {
            let rules = session_rules(runtime, interaction, &[], &grants, prefix.as_deref());
            self.client().send(reqwest::Method::PATCH, &session_path(session, ""), Some(&json!({ "permissions": rules }))).await?;
        }
        Ok(())
    }
}

/// `/name args`, a workspace command if the directory has one by that name.
fn command_of(text: &str) -> Option<(&str, &str)> {
    let rest = text.trim().strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).map(|(n, a)| (n, a.trim())).unwrap_or((rest, ""));
    (!name.is_empty() && !name.contains('/')).then_some((name, args))
}

/// T3's `declinedNote`: steered in before the rejection, or the model only sees a failed call
/// and retries.
fn declined_note(action: &str, resources: &[String]) -> String {
    let what = if resources.is_empty() { String::new() } else { format!(" ({})", resources.join(", ")) };
    format!("The user declined the {action} request{what}. Do not retry it; continue without it or ask the user how to proceed.")
}

/// A request that's already gone (answered elsewhere, or its session ended) needs no answer.
fn gone(e: &http::HttpError) -> bool {
    e.status() == Some(404)
}

#[async_trait]
impl Session for OpenCodeSession {
    async fn send(&self, input: TurnInput) -> anyhow::Result<()> {
        if input.is_compact() {
            return self.compact().await;
        }
        let inner = &self.inner;
        if inner.state.lock().unwrap().tr.running() {
            bail!("an OpenCode turn is already running");
        }
        inner.settle_stray().await?;
        inner.prepare().await?;
        if input.attachments.is_empty()
            && let Some((name, args)) = command_of(&input.text)
        {
            let path = format!("/api/command?{}", http::location(&inner.dir));
            let listed = tokio::time::timeout(INVENTORY_TIMEOUT, inner.client().get::<wire::Data<Vec<wire::Command>>>(&path)).await;
            if listed.ok().and_then(Result::ok).is_some_and(|l| l.data.iter().any(|c| c.name == name)) {
                inner.state.lock().unwrap().tr.start_turn(None);
                let sent = inner.client().send(reqwest::Method::POST, &inner.path("/command"), Some(&json!({ "name": name, "text": args }))).await;
                if let Err(e) = sent {
                    inner.state.lock().unwrap().tr.abandon_turn();
                    return Err(e.into());
                }
                return Ok(());
            }
        }
        let files = inner.files(&input)?;
        let id = new_id(TURN_PREFIX);
        let mut body = json!({ "id": id, "text": input.text });
        if !files.is_empty() {
            body["files"] = json!(files);
        }
        inner.state.lock().unwrap().tr.start_turn(Some(id));
        if let Err(e) = inner.client().send(reqwest::Method::POST, &inner.path("/prompt"), Some(&body)).await {
            inner.state.lock().unwrap().tr.abandon_turn();
            return Err(e.into());
        }
        Ok(())
    }

    /// T3's `session.compact` with the turn's id: the compaction runs as an execution of its own.
    async fn compact(&self) -> anyhow::Result<()> {
        let inner = &self.inner;
        if inner.state.lock().unwrap().tr.running() {
            bail!("an OpenCode turn is already running");
        }
        inner.settle_stray().await?;
        inner.prepare().await?;
        let id = new_id(TURN_PREFIX);
        inner.state.lock().unwrap().tr.start_turn(Some(id.clone()));
        if let Err(e) = inner.client().send(reqwest::Method::POST, &inner.path("/compact"), Some(&json!({ "id": id }))).await {
            inner.state.lock().unwrap().tr.abandon_turn();
            return Err(e.into());
        }
        Ok(())
    }

    /// Native steering: a prompt delivered into the running execution at its next step.
    async fn steer(&self, input: TurnInput) -> anyhow::Result<()> {
        let inner = &self.inner;
        let files = inner.files(&input)?;
        let id = new_id(STEER_PREFIX);
        {
            let mut s = inner.state.lock().unwrap();
            if !s.tr.running() {
                bail!("no OpenCode turn is running");
            }
            s.tr.add_steer(&id);
        }
        let mut body = json!({ "id": id, "text": input.text, "delivery": "steer" });
        if !files.is_empty() {
            body["files"] = json!(files);
        }
        if let Err(e) = inner.client().send(reqwest::Method::POST, &inner.path("/prompt"), Some(&body)).await {
            let state = &mut *inner.state.lock().unwrap();
            let actions = state.tr.drop_steer(&id);
            inner.run_locked(actions);
            return Err(e.into());
        }
        Ok(())
    }

    async fn interrupt(&self) -> anyhow::Result<()> {
        let inner = &self.inner;
        let children = {
            let mut s = inner.state.lock().unwrap();
            if !s.tr.running() {
                return Ok(());
            }
            s.tr.interrupting()
        };
        for child in children {
            let client = inner.client().clone();
            tokio::spawn(async move {
                let _ = tokio::time::timeout(INTERRUPT_TIMEOUT, client.send(reqwest::Method::POST, &session_path(&child, "/interrupt"), None)).await;
            });
        }
        let reply = tokio::time::timeout(INTERRUPT_TIMEOUT, inner.client().post::<wire::Interrupted>(&inner.path("/interrupt"), &json!({}))).await;
        match reply {
            Ok(Ok(Some(r))) if r.interrupted => Ok(()),
            // Nothing was running (the turn waits on background work, or its prompt wasn't
            // taken yet): the turn ends here.
            Ok(Ok(_)) => {
                inner.end_turn(TurnEnd::Interrupted);
                Ok(())
            }
            Ok(Err(e)) => Err(e.into()),
            Err(_) => {
                inner.end_turn(TurnEnd::Interrupted);
                Ok(())
            }
        }
    }

    async fn respond(&self, request: &str, response: &Response) -> anyhow::Result<()> {
        let inner = &self.inner;
        let pending = inner.state.lock().unwrap().tr.take_request(request).ok_or_else(|| anyhow!("no pending OpenCode request {request}"))?;
        let session = pending.session.clone();
        let result = match (pending.asked, response) {
            (Asked::Permission { action, resources, save }, Response::Decision { decision, message }) => {
                if *decision == Decision::AcceptForSession {
                    {
                        let mut s = inner.state.lock().unwrap();
                        let grants = s.grants.entry(session.clone()).or_default();
                        for resource in &save {
                            if !grants.iter().any(|g| g.action == action && &g.resource == resource) {
                                grants.push(Rule { action: action.clone(), resource: resource.clone(), effect: Effect::Allow });
                            }
                        }
                    }
                    if let Err(e) = inner.write_grants(&session).await {
                        tracing::warn!(target: "agent_host::opencode", "couldn't save an OpenCode grant: {e:#}");
                    }
                }
                if *decision == Decision::Decline {
                    let note = json!({ "text": declined_note(&action, &resources), "delivery": "steer", "resume": false });
                    if let Err(e) = inner.client().send(reqwest::Method::POST, &session_path(&session, "/synthetic"), Some(&note)).await {
                        tracing::debug!(target: "agent_host::opencode", "{e}");
                    }
                }
                let body = match decision {
                    Decision::Accept | Decision::AcceptForSession => json!({ "decision": "once" }),
                    Decision::Decline => json!({ "decision": "reject", "message": message.as_deref().filter(|m| !m.trim().is_empty()).unwrap_or(DECLINED) }),
                    Decision::Cancel => json!({ "decision": "reject" }),
                };
                let path = session_path(&session, &format!("/permission/{}/reply", http::encode(request)));
                inner.client().send(reqwest::Method::POST, &path, Some(&body)).await
            }
            (Asked::Form(fields), Response::Answers { answers }) => {
                let path = session_path(&session, &format!("/form/{}/reply", http::encode(request)));
                inner.client().send(reqwest::Method::POST, &path, Some(&json!({ "answer": form_answer(&fields, answers) }))).await
            }
            (Asked::Form(_), Response::Decision { .. }) => {
                inner.client().send(reqwest::Method::DELETE, &session_path(&session, &format!("/form/{}", http::encode(request))), None).await
            }
            (Asked::Permission { .. }, Response::Answers { .. }) => bail!("OpenCode's approval {request} takes a decision"),
        };
        match result {
            Err(e) if !gone(&e) => Err(e.into()),
            _ => Ok(()),
        }
    }

    async fn set_modes(&self, runtime: RuntimeMode, interaction: InteractionMode) -> anyhow::Result<()> {
        let mut s = self.inner.state.lock().unwrap();
        s.runtime = runtime;
        s.interaction = interaction;
        Ok(())
    }

    /// T3's `rollBackTo`: stage a revert at the first dropped turn's message (history only; roll's
    /// checkpoints own the files), then commit it; a failure clears the staged revert.
    async fn rollback(&self, turns: u32) -> anyhow::Result<()> {
        let inner = &self.inner;
        if turns == 0 {
            return Ok(());
        }
        let starts = boundaries(inner.client(), &inner.session).await?;
        let Some(boundary) = starts.get(starts.len().saturating_sub(turns as usize)).cloned() else { return Ok(()) };
        let staged = async {
            inner.client().send(reqwest::Method::POST, &inner.path("/revert/stage"), Some(&json!({ "messageID": boundary, "files": false }))).await?;
            inner.client().send(reqwest::Method::POST, &inner.path("/revert/commit"), None).await
        }
        .await;
        if let Err(e) = staged {
            let _ = inner.client().send(reqwest::Method::DELETE, &inner.path("/revert"), None).await;
            return Err(e.into());
        }
        Ok(())
    }

    async fn close(&self) {
        let inner = &self.inner;
        {
            let _state = inner.state.lock().unwrap();
            if inner.closing.swap(true, Ordering::Relaxed) {
                return;
            }
            let _ = inner.queue.send(ProviderEvent::Exited { error: None });
        }
        inner.server.unregister(&inner.session);
        for name in &inner.mcp {
            let path = format!("/api/experimental/mcp/{}?{}", http::encode(name), http::location(&inner.dir));
            let _ = tokio::time::timeout(INVENTORY_TIMEOUT, inner.client().send(reqwest::Method::DELETE, &path, None)).await;
        }
        let _ = std::fs::remove_dir_all(&inner.files);
        inner.lease.lock().unwrap().take();
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.server.unregister(&self.session);
        let _ = std::fs::remove_dir_all(&self.files);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_follow_the_modes() {
        let full = session_rules(RuntimeMode::FullAccess, InteractionMode::Default, &[], &[Rule::any("shell", Effect::Allow)], None);
        assert_eq!(full, vec![Rule::any("*", Effect::Allow)]);
        let auto = session_rules(RuntimeMode::AutoEdit, InteractionMode::Plan, &[], &[], Some("roll-1-"));
        assert_eq!(
            auto,
            vec![
                Rule::any("shell", Effect::Ask),
                Rule::any("edit", Effect::Allow),
                Rule::any("external_directory", Effect::Ask),
                Rule::any("edit", Effect::Deny),
                Rule::any("roll-*", Effect::Deny),
                Rule::any("roll-1-*", Effect::Allow),
            ]
        );
    }

    #[test]
    fn commands_parse() {
        assert_eq!(command_of("/hello WORLD"), Some(("hello", "WORLD")));
        assert_eq!(command_of(" /init "), Some(("init", "")));
        assert_eq!(command_of("/a/b"), None);
        assert_eq!(command_of("hello"), None);
    }
}
