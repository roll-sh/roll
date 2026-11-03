//! Codex through `codex app-server`: newline-delimited JSON-RPC 2.0 on stdio
//! (docs/code/agents.md, "Providers").
//!
//! Behaviour follows T3 Code's adapter (MIT): `apps/server/src/orchestration-v2/Adapters/
//! CodexAdapterV2.ts` for sessions, `apps/server/src/provider/CodexProvider.ts` for the status
//! probe, and `packages/effect-codex-app-server` for the protocol client. One process serves one
//! session: `initialize`, then `thread/start` (or `thread/resume`), then a `turn/start` per turn
//! carrying the approval policy, sandbox and collaboration mode for the session's modes.
//!
//! - `rpc`: the process and the JSON-RPC connection.
//! - `wire`: the slice of Codex's schema the adapter reads.
//! - `translate`: Codex messages → `ProviderEvent`s and `Response`s → replies (pure).
//! - `session`: the `Session` methods.

mod rpc;
mod session;
mod translate;
mod wire;

use super::{Provider, ProviderConfig, ProviderEvent, Session, SessionConfig};
use agent_proto::{AuthStatus, Capabilities, ModelInfo, ProviderId, ProviderInfo};
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
    cfg: ProviderConfig,
}

impl Codex {
    pub fn new(cfg: ProviderConfig) -> Self {
        Self { cfg }
    }

    fn binary(&self) -> PathBuf {
        self.cfg.binary.clone().unwrap_or_else(|| PathBuf::from("codex"))
    }

    fn spawn(&self, env: &[(String, String)], cwd: Option<&Path>) -> anyhow::Result<(std::sync::Arc<rpc::Rpc>, mpsc::UnboundedReceiver<rpc::Inbound>, rpc::Process)> {
        let binary = self.binary();
        let mut args = self.cfg.args.clone();
        args.push("app-server".into());
        let env = self.cfg.env.iter().chain(env).cloned().collect();
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
                    models.extend(page.data.into_iter().filter(|m| !m.hidden).map(|m| ModelInfo {
                        name: if m.display_name.is_empty() { m.model.clone() } else { m.display_name },
                        id: m.model,
                        default: m.is_default,
                        efforts: m.supported_reasoning_efforts.into_iter().map(|e| e.reasoning_effort).collect(),
                    }));
                    cursor = page.next_cursor.filter(|c| !c.is_empty());
                    if cursor.is_none() {
                        break;
                    }
                }
            }
            let version = init.user_agent.split_whitespace().next().and_then(|ua| ua.split_once('/')).map(|(_, v)| v.to_string());
            anyhow::Ok(Probe { version, account, models })
        };
        let result = tokio::time::timeout(PROBE_TIMEOUT, result).await;
        process.close().await;
        result.map_err(|_| anyhow::anyhow!("timed out checking codex"))?
    }

    async fn cli_version(&self) -> std::io::Result<Option<String>> {
        let mut cmd = tokio::process::Command::new(self.binary());
        cmd.args(&self.cfg.args).arg("--version").stdin(std::process::Stdio::null()).kill_on_drop(true);
        for (k, v) in &self.cfg.env {
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

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            steer: true,
            // Codex can fork (`thread/fork`), but the Session contract has no fork call yet.
            fork: false,
            rollback: true,
            plan_mode: true,
            images: true,
            subagents: true,
            // Modes travel with every `turn/start`.
            mode_switch: true,
        }
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
            capabilities: self.capabilities(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse() {
        assert_eq!(parse_version("codex-cli 0.164.0\n").as_deref(), Some("0.164.0"));
        assert_eq!(parse_version("codex-cli 0.160.0-alpha.1").as_deref(), Some("0.160.0-alpha.1"));
        assert_eq!(parse_version("0.1.4161006").as_deref(), Some("0.1.4161006"));
        assert_eq!(parse_version("codex"), None);
    }

    #[test]
    fn archived_errors_are_recognised() {
        assert!(is_archived("session 01a0 is archived; run `codex unarchive 01a0`"));
        assert!(!is_archived("thread not found"));
    }
}
