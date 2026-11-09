//! Codex through `codex app-server`: newline-delimited JSON-RPC 2.0 on stdio
//! (docs/code/agents.md, "Providers").
//!
//! Behaviour follows T3 Code's adapter (MIT): `apps/server/src/orchestration-v2/Adapters/
//! CodexAdapterV2.ts` for sessions, `apps/server/src/provider/CodexProvider.ts` for the status
//! probe, and `packages/effect-codex-app-server` for the protocol client. One process serves one
//! session: `initialize`, then `thread/start` (or `thread/resume`, or `thread/fork` for a session
//! that starts as a fork), then a `turn/start` per turn carrying the approval policy, sandbox and
//! collaboration mode for the session's modes.
//!
//! - `rpc`: the process and the JSON-RPC connection.
//! - `wire`: the slice of Codex's schema the adapter reads.
//! - `translate`: Codex messages → `ProviderEvent`s and `Response`s → replies (pure).
//! - `session`: the `Session` methods.

mod rpc;
mod session;
mod translate;
mod wire;

use super::{ForkAt, ForkFrom, Provider, ProviderConfig, ProviderEvent, Session, SessionConfig};
use agent_proto::{AuthStatus, Capabilities, ModelInfo, ModelOption, OptionChoice, OptionRole, ProviderId, ProviderInfo, effort_label};
use anyhow::Context;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

/// T3's MCP tool timeout: delegating and waiting on a thread can take an hour.
const MCP_TOOL_TIMEOUT_SEC: u64 = 65 * 60;
/// How long starting a session may take (MCP servers start within it).
const START_TIMEOUT: Duration = Duration::from_secs(120);
/// T3's `CODEX_AUTH_PROBE_TIMEOUT_MS`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Codex {
    cfg: super::LiveConfig,
}

impl Codex {
    pub fn new(cfg: ProviderConfig) -> Self {
        Self { cfg: super::LiveConfig::new(cfg) }
    }

    fn binary(&self) -> PathBuf {
        self.cfg.get().binary.clone().unwrap_or_else(|| PathBuf::from("codex"))
    }

    fn spawn(&self, env: &[(String, String)], cwd: Option<&Path>) -> anyhow::Result<(std::sync::Arc<rpc::Rpc>, mpsc::UnboundedReceiver<rpc::Inbound>, rpc::Process)> {
        let binary = self.binary();
        let mut args = self.cfg.get().args.clone();
        args.push("app-server".into());
        let env = self.cfg.get().env.iter().chain(env).cloned().collect();
        rpc::spawn(rpc::Spawn { binary: &binary, args, env, cwd })
    }
}

/// `initialize` + `initialized`. Opting out of `turn/diff/updated`: roll diffs checkpoints.
async fn initialize(rpc: &rpc::Rpc) -> anyhow::Result<wire::InitializeResponse> {
    let params = json!({
        "clientInfo": { "name": "roll", "title": "roll", "version": env!("CARGO_PKG_VERSION") },
        "capabilities": { "experimentalApi": true, "optOutNotificationMethods": ["turn/diff/updated"] },
    });
    let response = serde_json::from_value(rpc.request("initialize", params).await?).context("initialize answer")?;
    rpc.notify("initialized");
    Ok(response)
}

/// T3's `codexThreadRuntimeParams`: per-thread config, with roll's MCP servers.
fn thread_params(cfg: &SessionConfig) -> Value {
    let mut config = serde_json::Map::new();
    config.insert("tools.update_plan.enabled".into(), json!(true));
    config.insert("mcp_optional_startup_grace_ms".into(), json!(10_000));
    for server in &cfg.mcp {
        let headers: serde_json::Map<String, Value> = server.headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        config.insert(
            format!("mcp_servers.{}", server.name),
            json!({ "url": server.url, "http_headers": headers, "tool_timeout_sec": MCP_TOOL_TIMEOUT_SEC }),
        );
    }
    let mut params = json!({ "cwd": cfg.cwd, "config": config });
    if let Some(model) = &cfg.model {
        params["model"] = json!(model);
    }
    params
}

async fn open_thread(rpc: &rpc::Rpc, cfg: &SessionConfig) -> anyhow::Result<wire::ThreadResponse> {
    let mut params = thread_params(cfg);
    if let Some(fork) = &cfg.fork {
        return fork_thread(rpc, params, fork).await;
    }
    let response = match &cfg.resume {
        None => rpc.request("thread/start", params).await?,
        Some(thread) => {
            params["threadId"] = json!(thread);
            params["excludeTurns"] = json!(true);
            match rpc.request("thread/resume", params.clone()).await {
                // Codex refuses to resume a thread archived elsewhere until it is unarchived.
                Err(rpc::RpcError::Server { message, .. }) if is_archived(&message) => {
                    rpc.request("thread/unarchive", json!({ "threadId": thread })).await?;
                    rpc.request("thread/resume", params).await?
                }
                other => other?,
            }
        }
    };
    let thread: wire::ThreadResponse = serde_json::from_value(response).context("thread answer")?;
    anyhow::ensure!(!thread.thread.id.is_empty(), "codex answered without a thread id");
    Ok(thread)
}

/// T3's `forkThread`: `thread/fork` through the fork point's turn (`lastTurnId` is inclusive), or
/// of the latest state and then the copy rolled back by the turns after the point.
async fn fork_thread(rpc: &rpc::Rpc, mut params: Value, fork: &ForkFrom) -> anyhow::Result<wire::ThreadResponse> {
    params["threadId"] = json!(fork.thread);
    params["excludeTurns"] = json!(true);
    if let ForkAt::Turn(turn) = &fork.at {
        params["lastTurnId"] = json!(turn);
    }
    let thread: wire::ThreadResponse = serde_json::from_value(rpc.request("thread/fork", params).await?).context("thread/fork answer")?;
    anyhow::ensure!(!thread.thread.id.is_empty(), "codex forked without a thread id");
    if let ForkAt::DropLast(turns) = fork.at {
        session::revert(rpc, &thread.thread.id, turns).await.context("rolling the fork back to its turn")?;
    }
    Ok(thread)
}

/// T3: `/\bsession \S+ is archived\b|\bcodex unarchive\b/i`.
fn is_archived(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("codex unarchive") || (m.contains("session ") && m.contains(" is archived"))
}

/// `codex-cli 0.164.0`, `codex-cli 0.160.0-alpha.1`, or a bare `0.1.4161006`.
fn parse_version(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|w| w.trim_start_matches('v'))
        .find(|w| w.split('.').count() >= 3 && w.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(str::to_string)
}

/// T3's `codexAccountAuthLabel`, simplified.
fn account_detail(account: &wire::Account) -> String {
    match account.kind.as_str() {
        "apiKey" => "OpenAI API key".to_string(),
        "amazonBedrock" => "Amazon Bedrock".to_string(),
        _ => {
            let plan = account.plan_type.as_deref().filter(|p| !p.is_empty() && *p != "unknown");
            let plan = plan.map(|p| format!("ChatGPT {}{}", p[..1].to_uppercase(), &p[1..])).unwrap_or_else(|| "ChatGPT".into());
            match account.email.as_deref().filter(|e| !e.is_empty()) {
                Some(email) => format!("Logged in as {email} ({plan})"),
                None => format!("Logged in with {plan}"),
            }
        }
    }
}

struct Probe {
    version: Option<String>,
    account: wire::AccountResponse,
    models: Vec<ModelInfo>,
    limits: Option<agent_proto::UsageLimits>,
}

/// The option id of a Codex model's service tier (`turn/start`'s `serviceTier`).
pub(crate) const SERVICE_TIER: &str = "serviceTier";
/// The service tier that asks for none (T3's `DEFAULT_SERVICE_TIER_ID`).
pub(crate) const STANDARD_TIER: &str = "default";
/// At most this many reasoning levels or tiers are kept from one model.
const MAX_CHOICES: usize = 16;

/// A model from `model/list` with its options, as T3's `mapCodexModelCapabilities`
/// (`apps/server/src/provider/CodexProvider.ts`): the reasoning levels with the model's default,
/// and its service tiers after "Standard" (or the older `additionalSpeedTiers`, `fast` named
/// "Fast").
fn model_info(m: wire::Model) -> ModelInfo {
    let mut options = Vec::new();
    let default_effort = if m.model.starts_with("gpt-6-astra") { Some("medium".to_string()) } else { m.default_reasoning_effort.clone() };
    let levels: Vec<OptionChoice> = m
        .supported_reasoning_efforts
        .iter()
        .take(MAX_CHOICES)
        .map(|e| {
            let c = OptionChoice::new(&e.reasoning_effort, &effort_label(&e.reasoning_effort));
            if e.description.is_empty() { c } else { c.described(&e.description) }
        })
        .collect();
    if !levels.is_empty() {
        let default = default_effort.filter(|d| levels.iter().any(|l| &l.id == d));
        options.push(ModelOption::reasoning(levels, default.as_deref()));
    }
    let tiers: Vec<wire::ServiceTier> = if m.service_tiers.is_empty() {
        m.additional_speed_tiers.iter().map(|id| wire::ServiceTier { name: if id == "fast" { "Fast".into() } else { id.clone() }, id: id.clone(), description: String::new() }).collect()
    } else {
        m.service_tiers
    };
    if !tiers.is_empty() {
        let mut choices = vec![OptionChoice::new(STANDARD_TIER, "Standard")];
        choices.extend(tiers.iter().filter(|t| !t.id.is_empty() && t.id != STANDARD_TIER).take(MAX_CHOICES).map(|t| {
            let c = OptionChoice::new(&t.id, if t.name.is_empty() { &t.id } else { &t.name });
            let description = if t.id == "ultrafast" && t.description.is_empty() { "Even faster, more expensive" } else { t.description.as_str() };
            if description.is_empty() { c } else { c.described(description) }
        }));
        let default = m.default_service_tier.filter(|d| choices.iter().any(|c| &c.id == d)).unwrap_or_else(|| STANDARD_TIER.into());
        options.push(ModelOption::Select {
            id: SERVICE_TIER.into(),
            label: "Service Tier".into(),
            description: None,
            choices,
            default: Some(default),
            role: Some(OptionRole::Speed),
        });
    }
    ModelInfo {
        default: m.is_default,
        options,
        images: m.input_modalities.is_none_or(|i| i.iter().any(|x| x == "image")),
        ..ModelInfo::new(m.model.clone(), if m.display_name.is_empty() { m.model } else { m.display_name })
    }
}

impl Codex {
    /// T3's `probeCodexAppServerProvider`: a short-lived app-server that only reads (`account/read`
    /// without a token refresh, `model/list`).
    async fn probe(&self) -> anyhow::Result<Probe> {
        let (rpc, _inbound, process) = self.spawn(&[], None)?;
        let result = async {
            let init = initialize(&rpc).await?;
            let account: wire::AccountResponse = serde_json::from_value(rpc.request("account/read", json!({})).await?)?;
            let mut models = Vec::new();
            if account.account.is_some() || !account.requires_openai_auth {
                let mut cursor: Option<String> = None;
                for _ in 0..20 {
                    let params = match &cursor {
                        Some(c) => json!({ "cursor": c }),
                        None => json!({}),
                    };
                    let page: wire::ModelList = serde_json::from_value(rpc.request("model/list", params).await?)?;
                    models.extend(page.data.into_iter().filter(|m| !m.hidden).map(model_info));
                    cursor = page.next_cursor.filter(|c| !c.is_empty());
                    if cursor.is_none() {
                        break;
                    }
                }
            }
            // The account's usage limits (T3's `probeCodexAppServerProvider`); API-key accounts and
            // older app-servers have none, which isn't a failure.
            let limits = match account.account.is_some() {
                true => rpc
                    .request("account/rateLimits/read", Value::Null)
                    .await
                    .ok()
                    .and_then(|v| serde_json::from_value::<super::usage::CodexRateLimits>(v).ok())
                    .and_then(|l| super::usage::codex(&l, super::usage::now())),
                false => None,
            };
            let version = init.user_agent.split_whitespace().next().and_then(|ua| ua.split_once('/')).map(|(_, v)| v.to_string());
            anyhow::Ok(Probe { version, account, models, limits })
        };
        let result = tokio::time::timeout(PROBE_TIMEOUT, result).await;
        process.close().await;
        result.map_err(|_| anyhow::anyhow!("timed out checking codex"))?
    }

    async fn cli_version(&self) -> std::io::Result<Option<String>> {
        let mut cmd = tokio::process::Command::new(self.binary());
        cmd.args(&self.cfg.get().args).arg("--version").stdin(std::process::Stdio::null()).kill_on_drop(true);
        for (k, v) in &self.cfg.get().env {
            cmd.env(k, v);
        }
        let out = tokio::time::timeout(Duration::from_secs(10), cmd.output())
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "codex --version timed out"))??;
        Ok(parse_version(&String::from_utf8_lossy(&out.stdout)))
    }
}

#[async_trait]
impl Provider for Codex {
    fn id(&self) -> ProviderId {
        ProviderId::codex()
    }

    fn name(&self) -> String {
        "Codex".into()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            steer: true,
            // `thread/fork`, at a turn (`lastTurnId`).
            fork: true,
            rollback: true,
            plan_mode: true,
            images: true,
            subagents: true,
            // Modes travel with every `turn/start`.
            mode_switch: true,
            // `thread/compact/start`.
            compact: true,
        }
    }

    fn configure(&self, cfg: ProviderConfig) {
        self.cfg.set(cfg);
    }

    /// Codex takes a model provider of roll's (`-c model_providers.roll_<id>…`, the way
    /// CLIProxyAPI's instructions configure Codex) with the key in a variable (`env_key`), never
    /// in an argument.
    fn endpoint_use(&self, endpoint: &agent_proto::EndpointSpec, key: Option<&str>) -> Result<crate::endpoints::EndpointUse, String> {
        crate::endpoints::codex_use(endpoint, key)
    }

    /// T3 gives a bare custom Codex slug the first listed model's options.
    fn custom_model_options(&self, _slug: &str, listed: &[ModelInfo]) -> Vec<ModelOption> {
        listed.first().map(|m| m.options.clone()).unwrap_or_default()
    }

    async fn info(&self) -> ProviderInfo {
        let mut info = ProviderInfo {
            id: self.id(),
            name: "Codex".into(),
            installed: false,
            version: None,
            auth: AuthStatus::Unknown,
            detail: None,
            models: Vec::new(),
            update_required: Vec::new(),
            capabilities: self.capabilities(),
            checking: false,
            usage_limits: None,
        };
        match self.cli_version().await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info.detail = Some(format!("`{}` is not installed", self.binary().display()));
                return info;
            }
            Err(e) => {
                info.detail = Some(format!("couldn't run `{} --version`: {e}", self.binary().display()));
                return info;
            }
            Ok(version) => {
                info.installed = true;
                info.version = version;
            }
        }
        match self.probe().await {
            Ok(probe) => {
                info.version = info.version.or(probe.version);
                info.models = probe.models;
                info.usage_limits = probe.limits;
                match (&probe.account.account, probe.account.requires_openai_auth) {
                    (Some(account), _) => {
                        info.auth = AuthStatus::Ready;
                        info.detail = Some(account_detail(account));
                    }
                    (None, true) => {
                        info.auth = AuthStatus::Missing;
                        info.detail = Some("Run `codex login`".into());
                    }
                    // A configured model provider that needs no OpenAI login.
                    (None, false) => info.auth = AuthStatus::Ready,
                }
            }
            Err(e) => info.detail = Some(format!("couldn't check codex: {e:#}")),
        }
        info
    }

    async fn start(&self, cfg: SessionConfig) -> anyhow::Result<(Box<dyn Session>, mpsc::Receiver<ProviderEvent>)> {
        let (rpc, inbound, process) = self.spawn(&cfg.env, Some(&cfg.cwd))?;
        let opened = tokio::time::timeout(START_TIMEOUT, async {
            initialize(&rpc).await?;
            open_thread(&rpc, &cfg).await
        })
        .await;
        let thread = match opened {
            Ok(Ok(thread)) => thread,
            Ok(Err(e)) => {
                process.close().await;
                return Err(e);
            }
            Err(_) => {
                process.close().await;
                anyhow::bail!("codex app-server didn't start a thread within {}s", START_TIMEOUT.as_secs());
            }
        };
        let (session, events) = session::CodexSession::new(rpc, process, inbound, &cfg, thread);
        Ok((Box::new(session), events))
    }
}
