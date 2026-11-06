//! Muse Code through `muse serve`: MSP (newline-delimited JSON-RPC 2.0) on stdio
//! (docs/code/agents.md, "Providers").
//!
//! Behaviour follows T3 Code's Muse driver (MIT): `packages/provider-muse/src/server/driver.ts`
//! and `sdk.ts` (how Muse is started and the environment it gets), `adapter.ts` (sessions),
//! `status.ts` and `modelCatalog.ts` (the status probe and model catalog), `itemPresentation.ts`,
//! `protocol.ts`, `usage.ts` and `usageLimits.ts`. One process serves one session:
//! `muse serve --trust-workspace` (with `--disable-sandbox` in full access), `initialize` (MSP
//! schema 1, asking for session MCP servers), then `session/start` with a client-minted session
//! id (or `session/resume` and `session/setApprovalMode`), then a `turn/start` per turn.
//!
//! Muse gets the daemon's environment without `META_API_KEY` (so it uses its own login; the
//! provider's config may set one) and with `MUSE_NO_AUTO_UPDATE=1`. Unlike T3, roll never reads
//! Muse's home directory, session logs or binary (T3 reads them for usage history and updates):
//! usage limits come only from `usage/changed`, and updating Muse is the user's. MSP has no login
//! signal, so the login is always unknown; an empty model list says to run `muse login`.
//!
//! - `rpc`: the process and the MSP connection.
//! - `wire`: the slice of MSP the adapter reads.
//! - `translate`: MSP messages → `ProviderEvent`s and answers → commands (pure).
//! - `session`: the `Session` methods.

mod rpc;
mod session;
mod translate;
mod wire;

use super::{Provider, ProviderConfig, ProviderEvent, Session, SessionConfig};
use agent_proto::{Capabilities, ModelInfo, ModelOption, OptionChoice, ProviderId, ProviderInfo, RuntimeMode};
use anyhow::{Context, bail};
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

/// How long opening a session may take (its MCP servers start within it).
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// T3's catalog deadline (`checkMuseProviderStatus`).
const PROBE_TIMEOUT: Duration = Duration::from_secs(12);
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// Muse's own key: left out of the inherited environment so Muse uses its login (T3's
/// `makeMuseEnvironment`).
const META_API_KEY: &str = "META_API_KEY";
/// The only provider Muse's catalog and sessions name.
const META: &str = "meta";
/// At most this many models are kept from a catalog.
const MAX_MODELS: usize = 500;
/// T3's reasoning tiers when a model doesn't list its own (`FALLBACK_EFFORTS`, default `high`).
const FALLBACK_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const FALLBACK_EFFORT: &str = "high";

pub struct Muse {
    cfg: super::LiveConfig,
}

impl Muse {
    pub fn new(cfg: ProviderConfig) -> Self {
        Self { cfg: super::LiveConfig::new(cfg) }
    }

    pub fn provider_id() -> ProviderId {
        ProviderId("muse".into())
    }

    fn binary(&self) -> PathBuf {
        self.cfg.get().binary.unwrap_or_else(|| PathBuf::from("muse"))
    }

    /// `muse [args] serve …` with Muse's environment.
    fn spawn(&self, serve: &[&str], env: &[(String, String)], cwd: Option<&Path>) -> anyhow::Result<(std::sync::Arc<rpc::Rpc>, mpsc::UnboundedReceiver<rpc::Inbound>, rpc::Process)> {
        let cfg = self.cfg.get();
        let binary = self.binary();
        let args = cfg.args.iter().cloned().chain(serve.iter().map(|a| a.to_string())).collect();
        rpc::spawn(rpc::Spawn { binary: &binary, args, env_remove: &[META_API_KEY], env: muse_env(&cfg.env, env), cwd })
    }
}

/// `MUSE_NO_AUTO_UPDATE=1` (roll never lets Muse replace itself under a running daemon), then
/// the provider's config and the session's own variables.
fn muse_env(config: &[(String, String)], session: &[(String, String)]) -> Vec<(String, String)> {
    let mut env = vec![("MUSE_NO_AUTO_UPDATE".to_string(), "1".to_string())];
    env.extend(config.iter().chain(session).cloned());
    env
}

/// `initialize` + `initialized`; only MSP schema 1 is spoken (T3's `createMuseSdkHost`).
async fn initialize(rpc: &rpc::Rpc, read_only: bool) -> anyhow::Result<wire::InitializeResult> {
    let requested: &[&str] = if read_only { &[] } else { &["sessionMcp"] };
    let params = json!({
        "clientInfo": { "name": "roll", "title": "roll", "version": env!("CARGO_PKG_VERSION") },
        "capabilities": { "requestedCapabilities": requested },
    });
    let result: wire::InitializeResult = serde_json::from_value(rpc.request("initialize", params).await?).context("initialize answer")?;
    if result.schema.version != Some(1) {
        bail!("muse speaks MSP schema {:?}; roll speaks version 1", result.schema.version);
    }
    rpc.notify("initialized");
    Ok(result)
}

/// T3's `museApprovalMode` and `musePermissionProfile`: Muse has no accept-edits mode, so only
/// full access differs (`:unrestricted`); otherwise Muse asks for what its policy doesn't allow.
fn approval_mode(mode: RuntimeMode) -> (&'static str, &'static str) {
    match mode {
        RuntimeMode::FullAccess => ("allowAll", ":unrestricted"),
        RuntimeMode::Ask | RuntimeMode::AutoEdit => ("promptUnmatched", ":ask-me"),
    }
}

/// roll's MCP servers as Muse session config, each optional so an unreachable one doesn't fail
/// runs (T3 does the same for its own).
fn mcp_config(cfg: &SessionConfig) -> Option<Value> {
    if cfg.mcp.is_empty() {
        return None;
    }
    let servers: Map<String, Value> = cfg
        .mcp
        .iter()
        .map(|s| {
            let headers: Map<String, Value> = s.headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
            (s.name.clone(), json!({ "transport": "streamableHttp", "mode": "optional", "url": s.url, "headers": headers }))
        })
        .collect();
    Some(json!({ "mcpServers": servers }))
}

/// Open (or resume) the session on a fresh host.
async fn open(rpc: &rpc::Rpc, cfg: &SessionConfig, root: &str) -> anyhow::Result<session::Opened> {
    let init = initialize(rpc, false).await?;
    let config = mcp_config(cfg);
    if config.is_some() && !init.granted_capabilities.iter().any(|c| c == "sessionMcp") {
        bail!("this Muse Code can't take session MCP servers; update it");
    }
    let (mode, profile) = approval_mode(cfg.runtime_mode);
    let (session, method, mut params) = match &cfg.resume {
        Some(id) => (id.clone(), "session/resume", json!({ "sessionId": id, "excludeItems": true, "permissionProfile": profile })),
        None => {
            let id = rpc::mint();
            let mut params = json!({ "sessionId": id, "workspaceRoot": root, "providerId": META, "approvalMode": mode, "permissionProfile": profile });
            if let Some(model) = cfg.model.as_ref().filter(|m| !m.is_empty()) {
                params["modelId"] = json!(model);
            }
            (id, "session/start", params)
        }
    };
    if let Some(config) = config {
        params["config"] = config;
    }
    let result = match rpc.command(method, params).await {
        Err(e) if cfg.resume.is_some() && matches!(e.kind(), Some("sessionNotFound" | "notFound")) => {
            // The orchestrator starts a fresh conversation and hands the thread over.
            bail!("muse has no session {session} to resume ({e})")
        }
        other => other?,
    };
    let result: wire::SessionResult = serde_json::from_value(result).with_context(|| format!("{method} answer"))?;
    if result.session.session_id != session {
        bail!("muse answered {method} for session {}, not {session}", result.session.session_id);
    }
    if cfg.resume.is_some() {
        rpc.command("session/setApprovalMode", json!({ "sessionId": session, "mode": mode })).await?;
    }
    Ok(session::Opened { session, model: result.session.model_id })
}

/// T3's `parseMuseVersion`: Muse's own form (`1.1.1-R2514.1`) first, then any `x.y.z`.
fn parse_version(text: &str) -> Option<String> {
    let words: Vec<&str> = text.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',')).filter(|w| !w.is_empty()).collect();
    let semver = |w: &str| {
        let w = w.trim_start_matches('v');
        let core = w.split('-').next().unwrap_or_default();
        let parts: Vec<&str> = core.split('.').collect();
        (parts.len() >= 3 && parts.iter().take(3).all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))).then(|| w.to_string())
    };
    let revision = |w: &str| semver(w).filter(|v| v.split_once("-R").is_some_and(|(_, r)| r.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))));
    words.iter().find_map(|w| revision(w)).or_else(|| words.iter().find_map(|w| semver(w)))
}

fn effort_choice(tier: &str) -> Option<&'static str> {
    Some(match tier {
        "none" => "None",
        "minimal" => "Minimal",
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra High",
        "max" => "Max",
        "ultra" => "Ultra",
        _ => return None,
    })
}

/// T3's `museModelCapabilities`: the tiers a model reports (or the fallback), with its default.
fn reasoning(variants: Option<&Value>, default: Option<&str>, described: &[wire::EffortVariant]) -> Vec<ModelOption> {
    let reported: Option<Vec<&str>> = variants.and_then(Value::as_array).map(|v| v.iter().filter_map(Value::as_str).collect());
    let default = if reported.is_some() { default } else { Some(FALLBACK_EFFORT) };
    let tiers: Vec<&str> = reported.unwrap_or_else(|| FALLBACK_EFFORTS.to_vec()).into_iter().filter(|t| effort_choice(t).is_some()).collect();
    let Some(first) = tiers.first() else { return Vec::new() };
    let default = default.filter(|d| tiers.contains(d)).unwrap_or(first);
    let choices = tiers
        .iter()
        .map(|t| {
            let c = OptionChoice::new(t, effort_choice(t).expect("filtered"));
            match described.iter().find(|v| v.tier == *t).and_then(|v| v.description.as_deref()).filter(|d| !d.is_empty()) {
                Some(d) => c.described(d),
                None => c,
            }
        })
        .collect();
    vec![ModelOption::reasoning(choices, Some(default))]
}

/// T3's `discoverMuseModels`: Meta's models, once each, named by their label (a bare
/// `muse-spark-1.3` id becomes "Muse Spark 1.3").
fn models(list: wire::ModelList) -> anyhow::Result<Vec<ModelInfo>> {
    if list.provider_id != META {
        bail!("muse listed models for {:?}, not {META}", list.provider_id);
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in list.models.into_iter().filter(|m| m.provider_id == META && !m.model_id.is_empty()).take(MAX_MODELS) {
        if !seen.insert(m.model_id.clone()) {
            continue;
        }
        let label = m.display_label.trim();
        let label = if label.is_empty() { m.model_id.as_str() } else { label };
        let name = if label == m.model_id && label.starts_with("muse-") {
            label
                .split('-')
                .map(|w| {
                    let mut c = w.chars();
                    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
                })
                .collect::<Vec<String>>()
                .join(" ")
        } else {
            label.to_string()
        };
        out.push(ModelInfo {
            default: m.is_default,
            options: reasoning(m.variants.as_ref().filter(|v| v.is_array()), m.default_reasoning_effort.as_deref(), &m.reasoning_effort_variants),
            ..ModelInfo::new(m.model_id.clone(), name)
        });
    }
    Ok(out)
}

impl Muse {
    /// T3's `discoverMuseModels`: a read-only host (no shell, no writes, no session log) that
    /// only lists models.
    async fn catalog(&self) -> anyhow::Result<Vec<ModelInfo>> {
        let (rpc, _inbound, process) = self.spawn(&["serve", "--disable-shell", "--disable-write", "--no-session-log"], &[], None)?;
        let result = tokio::time::timeout(PROBE_TIMEOUT, async {
            initialize(&rpc, true).await?;
            let list: wire::ModelList = serde_json::from_value(rpc.request("model/list", json!({})).await?).context("model/list answer")?;
            models(list)
        })
        .await;
        process.close().await;
        result.map_err(|_| anyhow::anyhow!("timed out listing muse's models"))?
    }

    /// `muse --version`: `Ok(None)` when it ran but printed no version.
    async fn cli_version(&self) -> std::io::Result<Result<Option<String>, String>> {
        let cfg = self.cfg.get();
        let mut cmd = tokio::process::Command::new(self.binary());
        cmd.args(&cfg.args).arg("--version").stdin(std::process::Stdio::null()).kill_on_drop(true).env_remove(META_API_KEY);
        for (k, v) in muse_env(&cfg.env, &[]) {
            cmd.env(k, v);
        }
        let out = tokio::time::timeout(VERSION_TIMEOUT, cmd.output())
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "muse --version timed out"))??;
        let text = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        Ok(if out.status.success() { Ok(parse_version(&text)) } else { Err(parse_version(&text).unwrap_or_default()) })
    }
}

#[async_trait]
impl Provider for Muse {
    fn id(&self) -> ProviderId {
        Self::provider_id()
    }

    fn name(&self) -> String {
        "Muse Code".into()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // `turn/steer`.
            steer: true,
            // Muse rejects forks of completed turns (T3: `InvalidCut`) and can't roll back.
            fork: false,
            rollback: false,
            // No dedicated plan mode (T3 refuses it).
            plan_mode: false,
            images: true,
            subagents: true,
            // The approval mode is set when a session starts or resumes.
            mode_switch: false,
            // `session/compact`.
            compact: true,
        }
    }

    fn configure(&self, cfg: ProviderConfig) {
        self.cfg.set(cfg);
    }

    /// A custom model gets T3's fallback reasoning tiers.
    fn custom_model_options(&self, _slug: &str, _listed: &[ModelInfo]) -> Vec<ModelOption> {
        reasoning(None, None, &[])
    }

    /// T3's `checkMuseProviderStatus`. MSP has no login signal and serves cached models while
    /// logged out, so the login stays unknown; failures show on the first turn.
    async fn info(&self) -> ProviderInfo {
        let mut info = ProviderInfo::new(self.id(), self.name(), self.capabilities());
        match self.cli_version().await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info.detail = Some(format!("`{}` is not installed. Install Muse Code and run `muse login`.", self.binary().display()));
                return info;
            }
            Err(e) => {
                info.detail = Some(format!("couldn't run `{} --version`: {e}", self.binary().display()));
                return info;
            }
            Ok(Err(version)) => {
                info.installed = true;
                info.version = Some(version).filter(|v| !v.is_empty());
                info.detail = Some("Muse Code is installed but failed to run.".into());
                return info;
            }
            Ok(Ok(version)) => {
                info.installed = true;
                info.version = version;
            }
        }
        match self.catalog().await {
            Ok(models) if models.is_empty() => info.detail = Some("Muse Code listed no models. Run `muse login`, then check again.".into()),
            Ok(models) => info.models = models,
            Err(e) => info.detail = Some(format!("Muse Code couldn't list its models ({e:#}). Check the install and run `muse login`.")),
        }
        info
    }

    async fn start(&self, cfg: SessionConfig) -> anyhow::Result<(Box<dyn Session>, mpsc::Receiver<ProviderEvent>)> {
        if cfg.fork.is_some() {
            bail!("Muse Code can't fork a conversation");
        }
        // Muse checks the workspace's real path, case and all (T3's `museCanonicalPath`).
        let cwd = tokio::fs::canonicalize(&cfg.cwd).await.unwrap_or_else(|_| cfg.cwd.clone());
        let root = cwd.to_string_lossy().into_owned();
        let serve: &[&str] =
            if cfg.runtime_mode == RuntimeMode::FullAccess { &["serve", "--trust-workspace", "--disable-sandbox"] } else { &["serve", "--trust-workspace"] };
        let (rpc, inbound, process) = self.spawn(serve, &cfg.env, Some(&cwd))?;
        let opened = match tokio::time::timeout(START_TIMEOUT, open(&rpc, &cfg, &root)).await {
            Ok(Ok(opened)) => opened,
            Ok(Err(e)) => {
                process.close().await;
                return Err(e);
            }
            Err(_) => {
                process.close().await;
                bail!("muse serve didn't open a session within {}s", START_TIMEOUT.as_secs());
            }
        };
        let (session, events) = session::MuseSession::new(rpc, process, inbound, &cfg, root, opened);
        Ok((Box::new(session), events))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogs_map_like_t3() {
        let list: wire::ModelList = serde_json::from_value(json!({
            "providerId": "meta",
            "models": [
                { "modelId": "muse-spark-1.3-contributor", "displayLabel": "", "providerId": "meta", "isDefault": true, "variants": "unknown" },
                { "modelId": "muse-pro", "displayLabel": "Muse Pro", "providerId": "meta", "isDefault": false, "variants": ["low", "medium", "warp"],
                  "defaultReasoningEffort": "medium", "reasoningEffortVariants": [{ "tier": "low", "description": "Fast" }] },
                { "modelId": "muse-pro", "displayLabel": "again", "providerId": "meta", "isDefault": false },
                { "modelId": "other", "displayLabel": "Other", "providerId": "openai", "isDefault": false },
            ],
        }))
        .unwrap();
        let m = models(list).unwrap();
        assert_eq!(m.iter().map(|m| (m.id.as_str(), m.name.as_str(), m.default)).collect::<Vec<_>>(), [
            ("muse-spark-1.3-contributor", "Muse Spark 1.3 Contributor", true),
            ("muse-pro", "Muse Pro", false),
        ]);
        assert_eq!(m[0].efforts(), FALLBACK_EFFORTS);
        assert_eq!(m[1].efforts(), ["low", "medium"]);
        let Some(ModelOption::Select { default, choices, .. }) = m[1].reasoning() else { panic!() };
        assert_eq!((default.as_deref(), choices[0].description.as_deref()), (Some("medium"), Some("Fast")));
        assert!(models(wire::ModelList { provider_id: "openai".into(), models: Vec::new() }).is_err());
    }
}
