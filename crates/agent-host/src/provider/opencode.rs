//! OpenCode 2 through `opencode serve`: HTTP and server-sent events (docs/code/agents.md,
//! "Providers").
//!
//! A port of T3 Code's v2 OpenCode driver (MIT): `packages/provider-opencode/src/server/v2/
//! adapter.ts` (sessions, turns, requests, subagents, rollback, fork), `OpenCode2Client.ts` and
//! `OpenCode2Server.ts` (the client, Basic auth, the event stream), `OpenCodeServerOwner.ts`
//! (one server shared by every session, stopped 30 s after the last one), `openCode2Catalog.ts`
//! and `status.ts` (the model catalog and status), `versionProbe.ts` (`opencode --version`; 2.x
//! is this driver) and `toolItems.ts` (tools by name).
//!
//! One server per provider (and per extra environment) serves every session; each OpenCode
//! session names its directory (`location`), so the server has no working directory. A session
//! is created with the permission rules of its modes (`full_access`: allow everything; otherwise
//! shell and edits ask, edits allowed in `auto_edit`, work outside the project asks; plan mode is
//! OpenCode's `plan` agent and denies edits). Turns are prompts with ids roll picks
//! (`msg_roll_turn_…`): each is the turn's `TurnRef`, and rollback (`revert`) and fork (`fork`
//! with `before`) cut at them. Steering is native (`delivery: "steer"`). Background subagents are
//! folded into the turn that started them (see `translate`).
//!
//! T3 also reads OpenCode's `auth.json` and database for usage limits; roll never reads
//! OpenCode's private files or credentials, so OpenCode reports no usage limits. A model entry's
//! `settings` and `headers` (keys, base URLs) are never read (`wire::Model`).
//!
//! - `http`: the HTTP client and the event stream.
//! - `server`: the shared server process and the routing of its events.
//! - `wire`: the slice of OpenCode's API roll reads.
//! - `tools`: tools → roll tool calls.
//! - `translate`: events → `ProviderEvent`s (pure).
//! - `session`: opening a session and the `Session` methods.

mod http;
mod server;
mod session;
mod tools;
mod translate;
mod wire;

use super::{Provider, ProviderConfig, ProviderEvent, Session, SessionConfig};
use agent_proto::{AuthStatus, Capabilities, ModelInfo, ModelOption, OptionChoice, ProviderId, ProviderInfo};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

/// T3's `OPENCODE_VERSION_PROBE_TIMEOUT`: wrapped installs can take seconds to start cold.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// How long opening a session may take (a cold server lists its models and agents first).
const START_TIMEOUT: Duration = Duration::from_secs(90);
/// What a 1.x install says: the ACP agent runs it.
pub const NEEDS_V2: &str = "OpenCode 2 or newer is needed; 1.x runs as the ACP agent";

pub struct OpenCode {
    cfg: super::LiveConfig,
    pool: server::Pool,
}

impl OpenCode {
    pub fn new(cfg: ProviderConfig) -> Self {
        Self { cfg: super::LiveConfig::new(cfg), pool: server::Pool::default() }
    }

    pub fn provider_id() -> ProviderId {
        ProviderId("opencode".into())
    }

    fn binary(&self) -> PathBuf {
        self.cfg.get().binary.clone().unwrap_or_else(|| PathBuf::from("opencode"))
    }

    fn spec(&self, extra: &[(String, String)]) -> server::Spec {
        let cfg = self.cfg.get();
        server::Spec { binary: self.binary(), args: cfg.args.clone(), env: cfg.env.iter().chain(extra).cloned().collect() }
    }

    async fn cli_version(&self) -> std::io::Result<Option<String>> {
        let mut cmd = tokio::process::Command::new(self.binary());
        cmd.args(&self.cfg.get().args).arg("--version").stdin(std::process::Stdio::null()).kill_on_drop(true);
        for (k, v) in &self.cfg.get().env {
            cmd.env(k, v);
        }
        let out = tokio::time::timeout(VERSION_TIMEOUT, cmd.output())
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "opencode --version timed out"))??;
        Ok(parse_version(&String::from_utf8_lossy(&out.stdout)))
    }
}

/// The first `x.y.z` in `opencode --version` (`2.0.18`, `v1.14.19`, `opencode 2.1.0-beta.1`).
fn parse_version(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|w| w.trim_start_matches('v'))
        .find(|w| w.split('.').count() >= 3 && w.split('.').take(2).all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())))
        .map(str::to_string)
}

fn major(version: &str) -> Option<u64> {
    version.split('.').next()?.parse().ok()
}

/// T3's `titleCaseSlug`.
fn title_case(slug: &str) -> String {
    slug.split(['-', '_', '/'])
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut c = s.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// T3's `inferDefaultVariant`.
fn default_variant(provider: &str, variants: &[&str]) -> Option<String> {
    let has = |v: &str| variants.contains(&v);
    if variants.len() == 1 {
        return Some(variants[0].to_string());
    }
    if provider == "anthropic" || provider.starts_with("google") {
        return has("high").then(|| "high".into());
    }
    if provider == "openai" || provider == "opencode" {
        return if has("medium") { Some("medium".into()) } else { has("high").then(|| "high".into()) };
    }
    None
}

/// At most this many variants are kept from one model.
const MAX_CHOICES: usize = 16;

/// The catalog as T3's `checkOpenCode2` shows it: `provider/model` slugs, the variants as the
/// reasoning option, sorted by name, and every release of a family but the newest (and every
/// deprecated model) marked legacy (`markOpenCodeLegacy`).
fn catalog(models: Vec<wire::Model>) -> Vec<ModelInfo> {
    let family = |m: &wire::Model| m.family.as_ref().filter(|f| !f.is_empty()).map(|f| format!("{}/{f}", m.provider_id));
    let deprecated = |m: &wire::Model| m.status.as_deref() == Some("deprecated");
    let released = |m: &wire::Model| m.time.as_ref().and_then(|t| t.released);
    let mut newest: HashMap<String, f64> = HashMap::new();
    for m in models.iter().filter(|m| !deprecated(m)) {
        if let (Some(f), Some(r)) = (family(m), released(m)) {
            let e = newest.entry(f).or_insert(r);
            *e = e.max(r);
        }
    }
    let mut out: Vec<ModelInfo> = models
        .iter()
        .map(|m| {
            let variants: Vec<&str> = m.variants.iter().map(|v| v.id.as_str()).filter(|v| !v.is_empty()).take(MAX_CHOICES).collect();
            let mut options = Vec::new();
            if !variants.is_empty() {
                let default = default_variant(&m.provider_id, &variants);
                options.push(ModelOption::reasoning(variants.iter().map(|v| OptionChoice::new(v, &title_case(v))).collect(), default.as_deref()));
            }
            let superseded = family(m).and_then(|f| newest.get(&f).copied()).zip(released(m)).is_some_and(|(n, r)| r < n);
            let slug = format!("{}/{}", m.provider_id, m.id);
            ModelInfo {
                options,
                legacy: deprecated(m) || superseded,
                images: m.capabilities.as_ref().is_none_or(|c| c.input.is_empty() || c.input.iter().any(|i| i == "image")),
                ..ModelInfo::new(slug.clone(), if m.name.trim().is_empty() { slug } else { m.name.clone() })
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    out
}

/// Where the status probe lists models: the user's home.
fn probe_dir() -> String {
    std::env::var("HOME").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| "/".into())
}

#[async_trait]
impl Provider for OpenCode {
    fn id(&self) -> ProviderId {
        Self::provider_id()
    }

    fn name(&self) -> String {
        "OpenCode".into()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            steer: true,
            // `session.fork` before a turn's message.
            fork: true,
            // `revert` at a turn's message.
            rollback: true,
            plan_mode: true,
            images: true,
            subagents: true,
            // Rules (`PATCH`) and the agent (`/agent`) change between turns.
            mode_switch: true,
            compact: true,
        }
    }

    fn configure(&self, cfg: ProviderConfig) {
        self.cfg.set(cfg);
    }

    /// T3's `DEFAULT_OPENCODE_MODEL_CAPABILITIES` for a model OpenCode doesn't list.
    fn custom_model_options(&self, _slug: &str, _listed: &[ModelInfo]) -> Vec<ModelOption> {
        let levels = ["low", "medium", "high", "xhigh"].iter().map(|v| OptionChoice::new(v, &agent_proto::effort_label(v))).collect();
        vec![ModelOption::reasoning(levels, Some("medium"))]
    }

    async fn info(&self) -> ProviderInfo {
        let mut info = ProviderInfo {
            id: self.id(),
            name: self.name(),
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
        let version = match self.cli_version().await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info.detail = Some(format!("`{}` is not installed", self.binary().display()));
                return info;
            }
            Err(e) => {
                info.detail = Some(format!("couldn't run `{} --version`: {e}", self.binary().display()));
                return info;
            }
            Ok(None) => {
                info.detail = Some("couldn't tell the OpenCode version from `opencode --version`".into());
                return info;
            }
            Ok(Some(v)) => v,
        };
        info.version = Some(version.clone());
        if major(&version).is_none_or(|m| m < 2) {
            info.detail = Some(NEEDS_V2.into());
            return info;
        }
        info.installed = true;
        // A short-lived server lists the models; it creates no session and changes nothing.
        let listed = async {
            let lease = self.pool.acquire(self.spec(&[])).await?;
            anyhow::Ok(lease.server.models(&probe_dir()).await?)
        };
        match tokio::time::timeout(START_TIMEOUT, listed).await {
            Ok(Ok(models)) => {
                info.models = catalog(models);
                if info.models.is_empty() {
                    info.auth = AuthStatus::Missing;
                    info.detail = Some("OpenCode lists no models: run `opencode auth login` in a terminal".into());
                } else {
                    info.auth = AuthStatus::Ready;
                    let n = info.models.len();
                    info.detail = Some(format!("OpenCode {version} lists {n} model{}", if n == 1 { "" } else { "s" }));
                }
            }
            Ok(Err(e)) => info.detail = Some(format!("couldn't check OpenCode: {e:#}")),
            Err(_) => info.detail = Some("timed out checking OpenCode".into()),
        }
        info
    }

    async fn start(&self, cfg: SessionConfig) -> anyhow::Result<(Box<dyn Session>, mpsc::Receiver<ProviderEvent>)> {
        let opened = tokio::time::timeout(START_TIMEOUT, async {
            let lease = self.pool.acquire(self.spec(&cfg.env)).await?;
            session::open(lease, &cfg).await
        })
        .await
        .map_err(|_| anyhow::anyhow!("OpenCode didn't open a session within {}s", START_TIMEOUT.as_secs()))??;
        let (session, events) = opened;
        Ok((Box::new(session), events))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse() {
        assert_eq!(parse_version("2.0.18\n").as_deref(), Some("2.0.18"));
        assert_eq!(parse_version("opencode v1.14.19").as_deref(), Some("1.14.19"));
        assert_eq!(parse_version("opencode"), None);
        assert_eq!(major("2.0.18"), Some(2));
    }

    #[test]
    fn catalog_follows_t3() {
        let models: wire::Data<Vec<wire::Model>> = serde_json::from_value(serde_json::json!({ "data": [
            { "id": "claude-opus-5-5", "providerID": "anthropic", "name": "Claude Opus 5.5", "family": "opus", "time": { "released": 2 },
              "variants": [{ "id": "low" }, { "id": "high" }, { "id": "max" }], "settings": { "apiKey": "sk-secret" } },
            { "id": "claude-opus-4-5", "providerID": "anthropic", "name": "Claude Opus 4.5", "family": "opus", "time": { "released": 1 } },
            { "id": "big-pickle", "providerID": "opencode", "name": "Big Pickle", "status": "deprecated", "capabilities": { "input": ["text"] } },
        ]}))
        .unwrap();
        let models = catalog(models.data);
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["opencode/big-pickle", "anthropic/claude-opus-4-5", "anthropic/claude-opus-5-5"]);
        assert!(models[0].legacy && !models[0].images);
        assert!(models[1].legacy && !models[2].legacy);
        assert_eq!(models[2].efforts(), ["low", "high", "max"]);
        assert!(matches!(models[2].reasoning(), Some(ModelOption::Select { default: Some(d), choices, .. }) if d == "high" && choices[2].label == "Max"));
        assert!(!format!("{models:?}").contains("sk-secret"));
        assert_eq!(default_variant("openai", &["low", "medium"]).as_deref(), Some("medium"));
        assert_eq!(default_variant("x", &["a", "b"]), None);
        assert_eq!(title_case("extra_high"), "Extra High");
    }
}
