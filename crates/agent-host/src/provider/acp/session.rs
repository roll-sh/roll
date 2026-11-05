//! A live ACP session: one agent process, one ACP session.
//!
//! The reader task in `rpc` routes responses; this module's pump feeds everything else (and the
//! running prompt's answer, which `rpc` delivers in stream order) through the [`Translator`]
//! under one lock, so events never reorder. The agent's `fs/*` and `terminal/*` calls run on
//! their own tasks (`terminal/wait_for_exit` waits as long as the command runs). A forwarder
//! moves events to the session's bounded channel: a slow consumer delays events, never replies.
//!
//! Opening a session (T3: `AcpSessionRuntime.ts`): `initialize` offering v2 with v1's fields
//! alongside (the agent's answer decides the generation), then `session/new`, or for a resume
//! `session/load` (v1) or `session/resume` (v1 capability, or v2). A load replays the history as
//! updates first; those are dropped, keeping only settings, and an agent that never answers the
//! load is taken as loaded once its replay has been quiet for [`REPLAY_IDLE`]. Then the model,
//! effort and modes are applied with `session/set_config_option`, `session/set_model` or
//! `session/set_mode`.

use super::antigravity;
use super::client::Client;
use super::cursor;
use super::policy::{self, OptionValue, Setting, Wanted};
use super::rpc::{Inbound, Process, Rpc, RpcError};
use super::translate::{Action, Quirks, Translator, rpc_id_string};
use super::wire::{self, Flavor, Generation, Initialize};
use crate::provider::{ProviderEvent, Session, SessionConfig, TurnInput};
use agent_proto::{InteractionMode, Response, RuntimeMode};
use anyhow::{Context, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// How long a quiet replay means `session/load` is done (T3's `defaultSessionLoadReplayIdleGap`).
const REPLAY_IDLE: Duration = Duration::from_secs(2);
/// How long the agent has to end a turn after `session/cancel` before roll ends it.
const CANCEL_GRACE: Duration = Duration::from_secs(15);
/// How long `session/close` may take when the session ends.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

pub struct AcpSession {
    inner: Arc<Inner>,
}

struct Inner {
    rpc: Arc<Rpc>,
    process: Process,
    init: Initialize,
    client: Arc<Client>,
    state: Mutex<State>,
    queue: mpsc::UnboundedSender<ProviderEvent>,
    closing: AtomicBool,
}

struct State {
    tr: Translator,
    model: Option<String>,
    effort: Option<String>,
    options: agent_proto::ModelOptions,
    /// Sent in front of the first prompt (ACP has no system prompt).
    instructions: Option<String>,
    /// Cursor's skills, whose `$name` mentions go out as its `/name`.
    skills: BTreeSet<String>,
}

/// `initialize`: v2 fields with v1's alongside, so either generation can answer (T3:
/// `toNegotiatingInitializeRequest`). The capabilities are what roll implements, less what the
/// agent `expected` to answer handles badly (Antigravity: no writes, no terminals).
pub fn initialize_params(expected: Flavor) -> Value {
    let info = json!({ "name": "roll", "title": "roll", "version": env!("CARGO_PKG_VERSION") });
    let mut capabilities = json!({
        "fs": { "readTextFile": true, "writeTextFile": true },
        "terminal": true,
        "elicitation": { "form": {} },
    });
    if expected == Flavor::Antigravity
        && let (Some(caps), Value::Object(agy)) = (capabilities.as_object_mut(), antigravity::client_capabilities())
    {
        caps.extend(agy);
    }
    // Cursor lists a model's parameters as options of their own only when asked to.
    if expected == Flavor::Cursor {
        capabilities["_meta"] = cursor::client_meta();
    }
    json!({
        "protocolVersion": 2,
        "info": info,
        "capabilities": { "elicitation": { "form": {} } },
        "clientInfo": info,
        "clientCapabilities": capabilities,
    })
}

/// The agent needs a sign-in that roll never does for the user: ACP's -32000 ("authentication
/// required", T3's `isAcpAuthenticationRequired`), or Antigravity's sign-in link.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SignInRequired(pub String);

/// An error starting the session, as a [`SignInRequired`] when it is one.
fn setup_error(e: RpcError, flavor: Flavor) -> anyhow::Error {
    let antigravity = flavor == Flavor::Antigravity;
    match &e {
        RpcError::Server { code: -32000, .. } if antigravity => SignInRequired(antigravity::SIGN_IN.into()).into(),
        RpcError::Server { code: -32000, .. } => SignInRequired(format!("{e}: sign in to the agent in a terminal first")).into(),
        RpcError::Closed(why) if why == antigravity::SIGN_IN => SignInRequired(antigravity::SIGN_IN.into()).into(),
        _ => e.into(),
    }
}

/// roll's MCP servers, as the agent can take them (streamable HTTP only).
fn mcp_servers(init: &Initialize, cfg: &SessionConfig) -> Value {
    if !init.mcp_http {
        if !cfg.mcp.is_empty() {
            tracing::warn!(target: "agent_host::acp", agent = init.name, "the agent takes no HTTP MCP servers; roll's tools are left out");
        }
        return json!([]);
    }
    let servers: Vec<Value> = cfg
        .mcp
        .iter()
        .map(|s| {
            let headers: Vec<Value> = s.headers.iter().map(|(name, value)| json!({ "name": name, "value": value })).collect();
            json!({ "type": "http", "name": s.name, "url": s.url, "headers": headers })
        })
        .collect();
    Value::Array(servers)
}

/// Start the session on a running agent: everything after `spawn`. `expected` is the flavor
/// the agent was started as (its client capabilities depend on it).
pub(super) async fn open(
    rpc: Arc<Rpc>,
    process: Process,
    mut inbound: mpsc::UnboundedReceiver<Inbound>,
    cfg: &SessionConfig,
    expected: Flavor,
) -> anyhow::Result<(AcpSession, mpsc::Receiver<ProviderEvent>)> {
    let answer = rpc.request("initialize", initialize_params(expected)).await.map_err(|e| setup_error(e, expected))?;
    let init = wire::initialize(&answer);
    let flavor = init.flavor;
    let antigravity = flavor == Flavor::Antigravity;
    if antigravity {
        // Eagerly, as T3 does: a session/new before it fails or starts a browser sign-in.
        let method = antigravity::auth_method(&init.auth_methods);
        rpc.request("authenticate", json!({ "methodId": method })).await.map_err(|e| match setup_error(e, flavor) {
            e if e.is::<SignInRequired>() => e,
            other => SignInRequired(format!("{} ({other:#})", antigravity::SIGN_IN)).into(),
        })?;
    }
    let mut instructions = cfg.instructions.clone().filter(|i| !i.trim().is_empty());
    // Antigravity 1.4.5 takes instructions as its system prompt's tail on every activation.
    let meta = match antigravity {
        true => antigravity::session_meta(init.version.as_deref(), instructions.as_deref()),
        false => None,
    };
    if antigravity && antigravity::appends_system_prompt(init.version.as_deref()) {
        instructions = None;
    }
    let cwd = cfg.cwd.display().to_string();
    let servers = mcp_servers(&init, cfg);
    let (method, mut params) = match (&cfg.resume, init.generation) {
        (None, _) => ("session/new", json!({ "cwd": cwd, "mcpServers": servers })),
        // Antigravity: a load replays tool calls; a resume doesn't (T3's `resumeMethod`).
        (Some(id), _) if antigravity && init.resume_session => {
            ("session/resume", json!({ "sessionId": id, "cwd": cwd, "mcpServers": servers }))
        }
        (Some(id), Generation::V1) if init.load_session => {
            ("session/load", json!({ "sessionId": id, "cwd": cwd, "mcpServers": servers }))
        }
        // v2 replaced `session/load` with `session/resume`; roll needs no replay.
        (Some(id), _) if init.resume_session || (init.generation == Generation::V2 && init.load_session) => {
            ("session/resume", json!({ "sessionId": id, "cwd": cwd, "mcpServers": servers }))
        }
        (Some(_), _) => bail!("{} can't resume a session (it offers neither session/load nor session/resume)", agent_name(&init)),
    };
    if let Some(meta) = meta {
        params["_meta"] = meta;
    }
    let loading = method != "session/new";
    let tag = rpc.request_ordered(method, params);
    // Drive the connection until the setup answers; keep what arrives meanwhile.
    let mut early = Vec::new();
    let answer = loop {
        let wait = if loading && !early.is_empty() { REPLAY_IDLE } else { Duration::from_secs(3600) };
        match tokio::time::timeout(wait, inbound.recv()).await {
            Err(_) => break json!({}),
            Ok(None) => bail!("the agent closed the connection"),
            Ok(Some(Inbound::Response { tag: t, result })) if t == tag => break result.map_err(|e| setup_error(e, flavor))?,
            Ok(Some(Inbound::Response { .. })) => {}
            Ok(Some(Inbound::Notification { method, params })) => early.push((method, params)),
            Ok(Some(Inbound::Request { id, method, .. })) => {
                // Nothing can be answered before the session exists.
                match method.as_str() {
                    "session/request_permission" => rpc.respond(&id, json!({ "outcome": { "outcome": "cancelled" } })),
                    _ => rpc.respond_error(&id, -32603, "the session isn't ready yet"),
                }
            }
            Ok(Some(Inbound::Closed { error })) => {
                let why = error.unwrap_or_else(|| "the agent exited".into());
                return Err(if why == antigravity::SIGN_IN { SignInRequired(why).into() } else { anyhow::anyhow!(why) });
            }
        }
    };
    let setup = wire::setup(&answer);
    let session = match (&cfg.resume, &setup.session_id) {
        (Some(id), _) => id.clone(),
        (None, Some(id)) => id.clone(),
        (None, None) => bail!("{method} answered without a session id"),
    };
    let mut tr = Translator::new(session.clone(), super::client::root(&cfg.cwd), init.generation, Quirks::for_agent(&init), setup);
    tr.runtime = cfg.runtime_mode;
    tr.interaction = cfg.interaction_mode;
    // Settings that came with the setup (or its replay) count; replayed content doesn't.
    tr.loading = true;
    for (method, params) in &early {
        tr.on_notification(method, params);
    }
    tr.loading = false;

    let (queue, queued) = mpsc::unbounded_channel();
    let (events, rx) = mpsc::channel(1024);
    let _ = queue.send(ProviderEvent::NativeThread { id: session });
    if let Some(model) = tr.current_model() {
        let _ = queue.send(ProviderEvent::Model { model });
    }
    // What the agent offers, as it set the session up (before roll's choices are written): the
    // models and options the picker shows from now on (`catalog.rs`).
    let models = super::catalog::models(&tr.config, tr.models.as_ref());
    // Cursor's parameters as T3 shows them.
    let models = if flavor == Flavor::Cursor { cursor::catalog(models) } else { models };
    if !models.is_empty() {
        let _ = queue.send(ProviderEvent::Catalog { models });
    }
    let skills = match flavor {
        Flavor::Cursor => {
            let (cwd, home) = (tr.cwd().to_path_buf(), home(&cfg.env));
            tokio::task::spawn_blocking(move || cursor::skill_names(&cwd, home.as_deref())).await.unwrap_or_default()
        }
        _ => BTreeSet::new(),
    };
    let client = Arc::new(Client::new(tr.cwd().to_path_buf(), cfg.env.clone()));
    let inner = Arc::new(Inner {
        rpc,
        process,
        init,
        client,
        state: Mutex::new(State {
            tr,
            model: cfg.model.clone(),
            effort: cfg.effort.clone(),
            options: cfg.options.clone(),
            instructions,
            skills,
        }),
        queue,
        closing: AtomicBool::new(false),
    });
    if let Err(e) = inner.apply_settings(true).await {
        inner.process.close().await;
        return Err(e);
    }
    tokio::spawn(forward(queued, events));
    tokio::spawn(pump(Arc::downgrade(&inner), inbound));
    Ok((AcpSession { inner }, rx))
}

fn agent_name(init: &Initialize) -> &str {
    if init.name.is_empty() { "the agent" } else { &init.name }
}

/// The home directory the agent sees: the session's `HOME`, else the daemon's.
fn home(env: &[(String, String)]) -> Option<std::path::PathBuf> {
    env.iter().rev().find(|(k, _)| k == "HOME").map(|(_, v)| v.into()).or_else(|| std::env::var_os("HOME").map(Into::into))
}

/// Move queued events to the session's channel, in order, until `Exited`.
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
            Inbound::Notification { method, mut params } => {
                if method == "session/update" {
                    inner.client.resolve_terminals(&mut params);
                }
                let mut state = inner.state.lock().unwrap();
                let actions = state.tr.on_notification(&method, &params);
                inner.run_locked(actions);
            }
            Inbound::Request { id, method, params } => {
                if method.starts_with("fs/") || method.starts_with("terminal/") {
                    let access = inner.state.lock().unwrap().tr.access();
                    let (rpc, client, session) = (inner.rpc.clone(), inner.client.clone(), Arc::downgrade(&inner));
                    tokio::spawn(async move {
                        let answer = client.handle(&method, &params, access).await;
                        // Before the answer goes out, so the agent's next update sees it.
                        if method == "fs/read_text_file"
                            && let (Some(path), Some(inner)) = (params["path"].as_str(), session.upgrade())
                        {
                            let mut state = inner.state.lock().unwrap();
                            let actions = state.tr.read_attempted(path, matches!(answer, Some(Ok(_))));
                            inner.run_locked(actions);
                        }
                        match answer {
                            Some(Ok(result)) => rpc.respond(&id, result),
                            Some(Err(e)) => {
                                tracing::debug!(target: "agent_host::acp", method, "refused: {}", e.message);
                                rpc.respond_error(&id, e.code, &e.message)
                            }
                            None => rpc.respond_error(&id, -32601, &format!("{method} is not supported")),
                        }
                    });
                    continue;
                }
                let mut state = inner.state.lock().unwrap();
                let actions = state.tr.on_request(id, &method, &params);
                inner.run_locked(actions);
            }
            Inbound::Response { tag, result } => {
                let mut state = inner.state.lock().unwrap();
                let actions = state.tr.on_prompt_result(tag, result.map_err(|e| e.to_string()));
                inner.run_locked(actions);
            }
            Inbound::Closed { error } => {
                inner.client.close();
                let error = if inner.closing.load(Ordering::Relaxed) { None } else { error.or(Some("the agent exited".into())) };
                let _ = inner.queue.send(ProviderEvent::Exited { error });
                return;
            }
        }
    }
}

impl Inner {
    /// Carry out translated actions. Called with the state lock held so events stay in order.
    fn run_locked(&self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Event(event) => {
                    let _ = self.queue.send(event);
                }
                Action::Reply { id, result } => self.rpc.respond(&id, result),
                Action::ReplyError { id, code, message } => self.rpc.respond_error(&id, code, &message),
            }
        }
    }

    /// Bring the agent's model, effort and modes to the session's.
    async fn apply_settings(&self, opening: bool) -> anyhow::Result<()> {
        let (session, generation, settings) = {
            let state = self.state.lock().unwrap();
            let tr = &state.tr;
            // Cursor's own id for T3's `auto`.
            let offered: Vec<&str> = policy::model_option(&tr.config).map(|o| o.values().iter().map(|c| c.id.as_str()).collect()).unwrap_or_default();
            let model = match self.init.flavor {
                Flavor::Cursor => state.model.as_deref().map(|m| cursor::model(m, &offered)),
                _ => state.model.as_deref(),
            };
            let want = Wanted {
                model,
                effort: state.effort.as_deref(),
                options: &state.options,
                runtime: tr.runtime,
                interaction: tr.interaction,
                opening,
                permission_mode: (self.init.flavor == Flavor::Antigravity).then(|| antigravity::permission_mode(tr.runtime)),
            };
            let models = tr.models.as_ref();
            let settings = policy::settings(
                &tr.config,
                tr.modes.as_ref(),
                models.and_then(|m| m.current.as_deref()),
                models.is_some(),
                tr.before_plan.as_deref(),
                &want,
            );
            (tr.session().to_string(), tr.generation(), settings)
        };
        for setting in settings {
            match setting {
                Setting::Option { id, value, optional } => {
                    let mut params = json!({ "sessionId": session, "configId": id });
                    match &value {
                        OptionValue::Id(v) => params["value"] = json!(v),
                        OptionValue::Bool(b) => {
                            params["value"] = json!(b);
                            params["type"] = json!("boolean");
                        }
                    }
                    if generation == Generation::V2 && matches!(value, OptionValue::Id(_)) {
                        params["type"] = json!("id");
                    }
                    let answer = match self.rpc.request("session/set_config_option", params).await {
                        Ok(answer) => answer,
                        Err(e) if optional => {
                            tracing::debug!(target: "agent_host::acp", "setting {id}: {e}");
                            continue;
                        }
                        Err(e) => return Err(e).with_context(|| format!("setting {id}")),
                    };
                    let mut state = self.state.lock().unwrap();
                    let previous = policy::mode_option(&state.tr.config).filter(|o| o.id == id).and_then(|o| o.current().map(str::to_string));
                    self.remember_plan_exit(&mut state, previous);
                    if answer["configOptions"].is_array() {
                        let event = state.tr.set_config(wire::config_options(&answer["configOptions"]));
                        self.run_locked(event.into_iter().map(Action::Event).collect());
                    }
                }
                Setting::Mode(mode) => {
                    self.rpc.request("session/set_mode", json!({ "sessionId": session, "modeId": mode })).await.context("setting the mode")?;
                    let mut state = self.state.lock().unwrap();
                    let previous = state.tr.modes.as_ref().map(|m| m.current.clone());
                    self.remember_plan_exit(&mut state, previous);
                    state.tr.set_mode(&mode);
                }
                Setting::Model(model) => {
                    self.rpc.request("session/set_model", json!({ "sessionId": session, "modelId": model })).await.context("setting the model")?;
                    let mut state = self.state.lock().unwrap();
                    if let Some(models) = &mut state.tr.models {
                        models.current = Some(model.clone());
                    }
                    let _ = self.queue.send(ProviderEvent::Model { model });
                }
            }
        }
        Ok(())
    }

    /// Entering plan mode remembers the mode it left, to go back to.
    fn remember_plan_exit(&self, state: &mut State, previous: Option<String>) {
        if state.tr.interaction == InteractionMode::Plan
            && let Some(previous) = previous.filter(|m| !policy::is_plan_mode(m))
        {
            state.tr.before_plan = Some(previous);
        }
    }

    fn prompt(&self, input: &TurnInput, instructions: Option<&str>) -> anyhow::Result<Value> {
        let mut blocks = Vec::new();
        let text = match instructions {
            Some(i) => format!("<roll_instructions>\n{}\n</roll_instructions>\n\n{}", i.trim(), input.text),
            None => input.text.clone(),
        };
        if !text.trim().is_empty() {
            blocks.push(json!({ "type": "text", "text": text }));
        }
        for a in &input.attachments {
            if !a.mime.starts_with("image/") {
                bail!("ACP agents take image attachments only, not {} ({})", a.name, a.mime);
            }
            if !self.init.images {
                bail!("{} doesn't take images", agent_name(&self.init));
            }
            blocks.push(json!({ "type": "image", "mimeType": a.mime, "data": a.data }));
        }
        if blocks.is_empty() {
            bail!("a turn needs text or an image");
        }
        Ok(Value::Array(blocks))
    }
}

#[async_trait]
impl Session for AcpSession {
    async fn send(&self, input: TurnInput) -> anyhow::Result<()> {
        let inner = &self.inner;
        let mut state = inner.state.lock().unwrap();
        if state.tr.turn_active() {
            bail!("a turn is already running");
        }
        let input = TurnInput { text: cursor::rewrite_mentions(&input.text, &state.skills), ..input };
        let prompt = inner.prompt(&input, state.instructions.as_deref())?;
        state.instructions = None;
        let params = json!({ "sessionId": state.tr.session(), "prompt": prompt });
        // Under the lock: the answer can't be handled before the turn exists.
        let tag = inner.rpc.request_ordered("session/prompt", params);
        state.tr.start_turn(tag);
        Ok(())
    }

    async fn steer(&self, _input: TurnInput) -> anyhow::Result<()> {
        bail!("ACP agents take input only between turns")
    }

    async fn interrupt(&self) -> anyhow::Result<()> {
        let inner = &self.inner;
        let turn = {
            let mut state = inner.state.lock().unwrap();
            let (running, actions) = state.tr.interrupt();
            inner.run_locked(actions);
            if !running {
                return Ok(());
            }
            inner.rpc.notify("session/cancel", json!({ "sessionId": state.tr.session() }));
            state.tr.turn_number()
        };
        // An agent that never finishes the cancelled turn doesn't hold the thread forever.
        if let Some(turn) = turn {
            let weak = Arc::downgrade(inner);
            tokio::spawn(async move {
                tokio::time::sleep(CANCEL_GRACE).await;
                if let Some(inner) = weak.upgrade() {
                    let mut state = inner.state.lock().unwrap();
                    let actions = state.tr.force_interrupted(turn);
                    inner.run_locked(actions);
                }
            });
        }
        Ok(())
    }

    async fn respond(&self, request: &str, response: &Response) -> anyhow::Result<()> {
        let mut state = self.inner.state.lock().unwrap();
        let (id, result) = state.tr.reply(request, response)?;
        debug_assert_eq!(rpc_id_string(&id), request);
        self.inner.rpc.respond(&id, result);
        Ok(())
    }

    async fn set_modes(&self, runtime: RuntimeMode, interaction: InteractionMode) -> anyhow::Result<()> {
        {
            let mut state = self.inner.state.lock().unwrap();
            state.tr.set_runtime(runtime);
            state.tr.interaction = interaction;
        }
        self.inner.apply_settings(false).await
    }

    async fn rollback(&self, _turns: u32) -> anyhow::Result<()> {
        bail!("ACP agents can't drop turns")
    }

    async fn close(&self) {
        let inner = &self.inner;
        inner.closing.store(true, Ordering::Relaxed);
        if inner.init.close_session {
            let session = inner.state.lock().unwrap().tr.session().to_string();
            let close = inner.rpc.request("session/close", json!({ "sessionId": session }));
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, close).await;
        }
        inner.client.close();
        inner.process.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_offers_v2_and_v1_and_what_roll_implements() {
        let p = initialize_params(Flavor::Generic);
        assert_eq!(p["protocolVersion"], 2);
        assert_eq!(p["info"]["name"], "roll");
        assert_eq!(p["clientInfo"], p["info"]);
        assert_eq!(p["clientCapabilities"]["fs"], json!({ "readTextFile": true, "writeTextFile": true }));
        assert_eq!(p["clientCapabilities"]["terminal"], true);
        // Antigravity reads through roll, but writes and runs commands itself.
        let p = initialize_params(Flavor::Antigravity);
        assert_eq!(p["clientCapabilities"]["fs"], json!({ "readTextFile": true, "writeTextFile": false }));
        assert_eq!(p["clientCapabilities"]["terminal"], false);
        assert_eq!(p["clientInfo"]["name"], "roll");
        // Cursor is asked for its parameterized model picker.
        let p = initialize_params(Flavor::Cursor);
        assert_eq!(p["clientCapabilities"]["_meta"], json!({ "parameterizedModelPicker": true }));
        assert_eq!(p["clientCapabilities"]["terminal"], true);
        assert!(initialize_params(Flavor::Generic)["clientCapabilities"].get("_meta").is_none());
    }
}
