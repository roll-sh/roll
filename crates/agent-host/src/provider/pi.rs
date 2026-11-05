//! Pi (the `pi` coding agent) through `pi --mode rpc`: JSON lines on stdio (docs/code/agents.md,
//! "Providers").
//!
//! Behaviour follows T3 Code's Pi driver (MIT), `packages/provider-pi/src/server`: `adapter.ts`
//! for sessions, `rpc.ts` for the connection, `status.ts` and `thinkingCapabilities.ts` for the
//! status probe and models, `commands.ts` for `/compact`, `mcpInjection.ts` for launches, and
//! `mcpExtensionSource.ts` for the extension roll writes in its own words (`pi/extension.ts`).
//! One process serves one session: `pi --mode rpc <launch args> --extension <roll's extension>`,
//! in the session's directory and its own process group.
//!
//! - Threads are Pi session files: the native thread id is `get_state`'s `sessionFile`, and a
//!   resume is `switch_session` to it in a new process.
//! - A turn's ref is the session leaf it left. Rolling back resumes the session at a kept turn's
//!   ref ([`RollbackStyle::ResumeAt`]): Pi's `fork` at the next user message copies everything
//!   before it into a new session file, which becomes the native thread. Pi keeps entry ids in
//!   the copy, and the refs stay valid across process restarts, which T3 needs its stored refs
//!   for too (its `thread_rollback_after_stop` scenario).
//! - A fork starts in a short-lived `pi --fork <source>` (no extensions, no tools, offline), which
//!   copies the session with the fork's own directory, then `fork`s it at the point.
//! - Approvals, MCP servers and roll's instructions live in the extension (Pi has none of them);
//!   their settings travel in the environment only (`pi/extension.rs`).
//!
//! - `rpc`: the process and the JSON-lines connection.
//! - `session`: opening a session and the `Session` methods.
//! - `items`: Pi's events → items (pure).
//! - `tree`: the session tree and where to cut it.
//! - `catalog`: models, thinking levels, launch arguments, versions.
//! - `extension`: roll's Pi extension and its environment.

mod catalog;
mod extension;
mod items;
mod rpc;
mod session;
mod tree;

use super::{ForkAt, ForkFrom, LiveConfig, Provider, ProviderConfig, ProviderEvent, RollbackStyle, Session, SessionConfig};
use agent_proto::{AuthStatus, Capabilities, ProviderId, ProviderInfo};
use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

/// How long starting a session may take (extensions load and MCP servers connect within it).
const START_TIMEOUT: Duration = Duration::from_secs(120);
/// T3's `VERSION_PROBE_TIMEOUT_MS`.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// T3's `PI_RPC_DISCOVERY_TIMEOUT_MS`.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);

pub use extension::{FILE_NAME as EXTENSION_FILE, SOURCE as EXTENSION_SOURCE};

/// The provider id (`[providers.pi]` in the daemon's config).
pub const ID: &str = "pi";

pub struct Pi {
    cfg: LiveConfig,
    /// Where roll's extension is written.
    dir: PathBuf,
}

impl Pi {
    /// `dir` is the daemon's directory for Pi (roll's extension goes there).
    pub fn new(cfg: ProviderConfig, dir: impl Into<PathBuf>) -> Self {
        Self { cfg: LiveConfig::new(cfg), dir: dir.into() }
    }

    fn binary(&self) -> PathBuf {
        self.cfg.get().binary.unwrap_or_else(|| PathBuf::from("pi"))
    }

    fn spawn(&self, args: Vec<String>, env: Vec<(String, String)>, cwd: Option<&Path>) -> anyhow::Result<(std::sync::Arc<rpc::Rpc>, mpsc::UnboundedReceiver<rpc::Inbound>, rpc::Process)> {
        let binary = self.binary();
        let env = self.cfg.get().env.into_iter().chain(env).collect();
        rpc::spawn(rpc::Spawn { binary: &binary, args, env, env_remove: &extension::ALL_ENV, cwd })
    }

    fn launch_args(&self) -> anyhow::Result<Vec<String>> {
        catalog::launch_args(&self.cfg.get().args).map_err(|e| anyhow!(e))
    }

    /// Run `pi <args>` to completion (`--version`, `auth check`), bounded.
    async fn run(&self, args: &[&str]) -> std::io::Result<std::process::Output> {
        let mut cmd = tokio::process::Command::new(self.binary());
        cmd.args(args).stdin(std::process::Stdio::null()).kill_on_drop(true);
        for (k, v) in self.cfg.get().env {
            cmd.env(k, v);
        }
        tokio::time::timeout(VERSION_TIMEOUT, cmd.output())
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, format!("pi {} timed out", args.join(" "))))?
    }

    /// T3's `discoverPiViaRpc`: a short-lived `--no-session --offline` process that only reads.
    async fn discover(&self, args: &[String]) -> anyhow::Result<(Value, Value)> {
        let launch = catalog::build(catalog::Launch { args, ephemeral: true, short_lived: true, ..Default::default() });
        let (rpc, _inbound, process) = self.spawn(launch, Vec::new(), None)?;
        let found = tokio::time::timeout(DISCOVERY_TIMEOUT, async {
            let state = rpc.request("get_state", json!({}), rpc::REQUEST_TIMEOUT).await?;
            let models = rpc.request("get_available_models", json!({}), rpc::REQUEST_TIMEOUT).await?;
            anyhow::Ok((state, models))
        })
        .await;
        process.close().await;
        found.map_err(|_| anyhow!("pi didn't answer within {}s", DISCOVERY_TIMEOUT.as_secs()))?
    }

    /// Why Pi's default model can't start a turn (T3's `describeUnavailablePiModel`): `pi auth
    /// check` (0.84.1+) says whether that provider has no credentials, without refreshing or
    /// printing them. It can't see a runtime `--api-key`, so it's skipped when one is given.
    async fn unavailable(&self, version: &str, args: &[String], provider: &str, id: &str) -> String {
        let label = format!("Pi default ({provider}/{id})");
        let missing = catalog::at_least(version, catalog::AUTH_CHECK_VERSION) && !args.iter().any(|a| a == "--api-key") && {
            let out = self.run(&["auth", "check", "--provider", provider, "--json", "--no-refresh"]).await;
            out.ok()
                .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
                .is_some_and(|v| v.get("reason").and_then(Value::as_str) == Some("credentials_not_configured"))
        };
        if missing {
            format!("{label} has no credentials configured for {provider}. Choose another model, or run `pi` in a terminal and use /login or configure its API key.")
        } else {
            format!("{label} uses {provider}, which has no available models in Pi. Choose another model, or check that provider's setup and Pi's launch arguments.")
        }
    }

    /// T3's `forkThread`: copy `from`'s session into a new file in this session's directory
    /// (`pi --fork`), then fork that copy at the point. The new file's path.
    async fn fork(&self, from: &ForkFrom, cfg: &SessionConfig, args: &[String]) -> anyhow::Result<String> {
        if let ForkAt::DropLast(_) = from.at {
            bail!("Pi forks at a turn's ref, not by counting turns");
        }
        let mut launch = catalog::build(catalog::Launch { args, short_lived: true, bare: true, ..Default::default() });
        launch.extend(["--fork".to_string(), from.thread.clone()]);
        let (rpc, _inbound, process) = self.spawn(launch, Vec::new(), Some(&cfg.cwd))?;
        let forked = tokio::time::timeout(START_TIMEOUT, async {
            if let ForkAt::Turn(at) = &from.at {
                let entries = rpc.request("get_entries", json!({}), rpc::REQUEST_TIMEOUT).await?;
                let listing = tree::parse(&entries).context("pi listed no session tree to fork")?;
                if let Some(entry) = listing.cut_after(at).map_err(|e| anyhow!(e))? {
                    let result = rpc.request("fork", json!({ "entryId": entry }), rpc::LIFECYCLE_TIMEOUT).await?;
                    if result.get("cancelled") == Some(&Value::Bool(true)) {
                        bail!("a Pi extension cancelled the fork");
                    }
                }
            }
            let state = rpc.request("get_state", json!({}), rpc::REQUEST_TIMEOUT).await?;
            match state.get("sessionFile").and_then(Value::as_str) {
                Some(file) if file != from.thread => Ok(file.to_string()),
                _ => bail!("Pi's fork didn't make a session file of its own"),
            }
        })
        .await;
        process.close().await;
        forked.map_err(|_| anyhow!("pi didn't fork the session within {}s", START_TIMEOUT.as_secs()))?
    }
}

#[async_trait]
impl Provider for Pi {
    fn id(&self) -> ProviderId {
        ProviderId(ID.into())
    }

    fn name(&self) -> String {
        "Pi".into()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // `prompt` with `streamingBehavior: "steer"`.
            steer: true,
            // `pi --fork` and `fork` at a turn's ref.
            fork: true,
            rollback: true,
            plan_mode: false,
            images: true,
            subagents: false,
            // The approval hook keeps one mode per process.
            mode_switch: false,
            // The `compact` command.
            compact: true,
        }
    }

    fn rollback_style(&self) -> RollbackStyle {
        RollbackStyle::ResumeAt
    }

    fn configure(&self, cfg: ProviderConfig) {
        self.cfg.set(cfg);
    }

    /// T3's `checkPiProviderStatus`: `pi --version` (0.80.5 or newer), the launch arguments, then
    /// the models a short-lived RPC process lists. Ready when Pi lists any model.
    async fn info(&self) -> ProviderInfo {
        let mut info = ProviderInfo {
            id: self.id(),
            name: self.name(),
            installed: false,
            version: None,
            auth: AuthStatus::Unknown,
            detail: None,
            models: vec![catalog::default_model()],
            update_required: Vec::new(),
            capabilities: self.capabilities(),
            checking: false,
        };
        let version = match self.run(&["--version"]).await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info.detail = Some(format!("`{}` is not installed. Install it with `npm install -g --ignore-scripts @earendil-works/pi-coding-agent`.", self.binary().display()));
                return info;
            }
            Err(e) => {
                info.installed = true;
                info.detail = Some(format!("couldn't run `{} --version`: {e}", self.binary().display()));
                return info;
            }
            Ok(out) => {
                info.installed = true;
                let text = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
                info.version = catalog::parse_version(&text);
                if !out.status.success() {
                    info.detail = Some("Pi is installed but `pi --version` failed.".into());
                    return info;
                }
                match info.version.clone() {
                    None => {
                        info.detail = Some(format!("roll couldn't tell which Pi this is. Pi {} or newer is required.", catalog::MIN_VERSION));
                        return info;
                    }
                    Some(v) if !catalog::at_least(&v, catalog::MIN_VERSION) => {
                        info.detail = Some(format!("Pi {v} is unsupported. Update to Pi {} or newer.", catalog::MIN_VERSION));
                        return info;
                    }
                    Some(v) => v,
                }
            }
        };
        let args = match self.launch_args() {
            Ok(args) => args,
            Err(e) => {
                info.detail = Some(e.to_string());
                return info;
            }
        };
        let (state, listed) = match self.discover(&args).await {
            Ok(found) => found,
            Err(e) => {
                info.detail = Some(format!("Pi is installed, but roll couldn't read its models: {e:#}"));
                return info;
            }
        };
        let models = match catalog::models(&listed, &state) {
            Ok(models) => models,
            Err(e) => {
                info.detail = Some(format!("Pi listed its models in a way roll can't read: {e}"));
                return info;
            }
        };
        if models.is_empty() {
            info.auth = AuthStatus::Missing;
            info.detail = Some("Pi has no usable models. Run `pi` in a terminal and use /login, or configure an API key in ~/.pi/agent.".into());
            return info;
        }
        info.auth = AuthStatus::Ready;
        let active = state.get("model");
        let provider = active.and_then(|m| m.get("provider")).and_then(Value::as_str);
        let id = active.and_then(|m| m.get("id")).and_then(Value::as_str);
        if let (Some(provider), Some(id)) = (provider, id)
            && !models.iter().any(|m| catalog::parse_slug(&m.id).is_some_and(|(p, _)| p == provider))
        {
            info.detail = Some(self.unavailable(&version, &args, provider, id).await);
        }
        info.models.extend(models);
        info
    }

    async fn start(&self, cfg: SessionConfig) -> anyhow::Result<(Box<dyn Session>, mpsc::Receiver<ProviderEvent>)> {
        let args = self.launch_args()?;
        // Always loaded, MCP or not: without its approval hook, `ask` would ask nothing.
        let ext = extension::materialize(&self.dir).with_context(|| format!("writing roll's Pi extension into {}", self.dir.display()))?;
        let (resume, resume_at) = match &cfg.fork {
            Some(from) => (Some(self.fork(from, &cfg, &args).await?), None),
            None => (cfg.resume.clone(), cfg.resume_at.as_deref()),
        };
        let launch = catalog::build(catalog::Launch { args: &args, extension: Some(&ext), ..Default::default() });
        let env = cfg.env.iter().cloned().chain(extension::env(cfg.runtime_mode, &cfg.mcp, cfg.instructions.as_deref())).collect();
        let (rpc, inbound, process) = self.spawn(launch, env, Some(&cfg.cwd))?;
        let opened = match tokio::time::timeout(START_TIMEOUT, session::open(&rpc, &cfg, resume.as_deref(), resume_at)).await {
            Ok(Ok(opened)) => opened,
            Ok(Err(e)) => {
                process.close().await;
                return Err(e);
            }
            Err(_) => {
                process.close().await;
                bail!("pi didn't open its session within {}s", START_TIMEOUT.as_secs());
            }
        };
        let (session, events) = session::PiSession::new(rpc, process, inbound, &cfg, opened);
        Ok((Box::new(session), events))
    }
}
