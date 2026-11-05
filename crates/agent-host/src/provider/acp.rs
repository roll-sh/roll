//! Agent Client Protocol agents (Gemini CLI, OpenCode, Grok, Kiro, fx, Qwen Code, goose, …):
//! newline-delimited JSON-RPC 2.0 on stdio (docs/code/agents.md, "Providers").
//!
//! Behaviour follows T3 Code (MIT): `packages/effect-acp` (the protocol client: v2 negotiation
//! with a v1 fallback, v2 shapes normalized onto v1's), `packages/provider-acp` (the session
//! runtime, update projection, client policy and terminals), `packages/provider-acp-registry`
//! (which agents exist and how each starts) and the Grok, Kiro and fx flavors for their quirks.
//! One process serves one session.
//!
//! - `rpc`: the process and the JSON-RPC connection.
//! - `wire`: the slice of ACP's schema the adapter reads, parsed tolerantly.
//! - `translate`: ACP messages → `ProviderEvent`s and `Response`s → replies (pure).
//! - `policy`: what roll answers itself, and runtime/plan modes → the agent's settings.
//! - `client`: `fs/*` and `terminal/*`, confined to the workspace.
//! - `session`: opening a session and the `Session` methods.

mod client;
mod policy;
mod rpc;
mod session;
mod translate;
mod wire;

use super::{Provider, ProviderConfig, ProviderEvent, Session, SessionConfig};
use crate::config::AcpAgent;
use agent_proto::{AuthStatus, Capabilities, ModelInfo, ProviderId, ProviderInfo};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

/// How long starting a session may take (agents start their MCP servers within it).
const START_TIMEOUT: Duration = Duration::from_secs(120);
/// T3's ACP status probe timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Acp {
    name: String,
    cfg: super::LiveConfig,
    probe_timeout: Duration,
}

impl Acp {
    /// The agent `[[acp]]` names `name` (provider id `acp:<name>`).
    pub fn new(name: String, cfg: ProviderConfig) -> Self {
        Self { name, cfg: super::LiveConfig::new(cfg), probe_timeout: PROBE_TIMEOUT }
    }

    /// How long the probe waits for `initialize` ([`PROBE_TIMEOUT`] unless set).
    pub fn with_probe_timeout(mut self, timeout: Duration) -> Self {
        self.probe_timeout = timeout;
        self
    }

    fn binary(&self) -> PathBuf {
        self.cfg.get().binary.clone().unwrap_or_else(|| PathBuf::from(&self.name))
    }

    fn spawn(
        &self,
        env: &[(String, String)],
        cwd: Option<&Path>,
    ) -> anyhow::Result<(std::sync::Arc<rpc::Rpc>, mpsc::UnboundedReceiver<rpc::Inbound>, rpc::Process)> {
        let binary = self.binary();
        let env = self.cfg.get().env.iter().chain(env).cloned().collect();
        rpc::spawn(rpc::Spawn { binary: &binary, args: self.cfg.get().args.clone(), env, cwd })
    }

    /// `initialize` and nothing else: never `authenticate`, never a session (T3: "setup must
    /// not happen as a health-check side effect"; its Grok and Kiro probes do the same).
    async fn probe(&self) -> anyhow::Result<wire::Initialize> {
        let (rpc, _inbound, process) = self.spawn(&[], None)?;
        let result = tokio::time::timeout(self.probe_timeout, rpc.request("initialize", session::initialize_params())).await;
        process.close().await;
        let answer = result.map_err(|_| anyhow::anyhow!("no answer to initialize within {}s", self.probe_timeout.as_secs_f32()))??;
        Ok(wire::initialize(&answer))
    }

    fn base_capabilities(images: bool) -> Capabilities {
        Capabilities {
            // ACP has no steering: the orchestrator interrupts and restarts the run.
            steer: false,
            fork: false,
            rollback: false,
            // Agents with a `plan`/`architect` mode get it; others run plan turns as usual.
            plan_mode: true,
            images,
            subagents: false,
            // Modes go out with `session/set_config_option`/`set_mode`, and approvals are roll's.
            mode_switch: true,
            // ACP has no compaction request; an agent's own `/compact` command still goes
            // through as a message.
            compact: false,
        }
    }
}

#[async_trait]
impl Provider for Acp {
    fn id(&self) -> ProviderId {
        ProviderId(format!("acp:{}", self.name))
    }

    fn capabilities(&self) -> Capabilities {
        // Images are known only once the agent answers `initialize`; `info` reports them.
        Self::base_capabilities(true)
    }

    fn configure(&self, cfg: ProviderConfig) {
        self.cfg.set(cfg);
    }

    fn name(&self) -> String {
        KNOWN.iter().find(|k| k.name == self.name).map(|k| k.title.to_string()).unwrap_or_else(|| self.name.clone())
    }

    async fn info(&self) -> ProviderInfo {
        let known = KNOWN.iter().find(|k| k.name == self.name);
        let mut info = ProviderInfo {
            id: self.id(),
            name: Provider::name(self),
            installed: false,
            version: None,
            auth: AuthStatus::Unknown,
            detail: None,
            models: Vec::new(),
            capabilities: Self::base_capabilities(true),
            checking: false,
        };
        let binary = self.binary();
        if which(&binary).is_none() {
            info.detail = Some(format!("`{}` is not installed", binary.display()));
            return info;
        }
        info.installed = true;
        match self.probe().await {
            Ok(init) => {
                if known.is_none()
                    && let Some(title) = init.title.clone().or_else(|| (!init.name.is_empty()).then(|| init.name.clone()))
                {
                    info.name = title;
                }
                info.version = init.version.as_deref().map(version);
                info.capabilities = Self::base_capabilities(init.images);
                if let Some(models) = init.models {
                    info.models = models
                        .available
                        .into_iter()
                        .map(|m| ModelInfo { default: models.current.as_deref() == Some(m.id.as_str()), id: m.id, name: m.name, efforts: Vec::new(), custom: false })
                        .collect();
                }
                // Starting a session would prove a login, and might start one: roll doesn't
                // (T3 keeps registry agents' auth unknown too).
                if !init.auth_methods.is_empty() {
                    let names: Vec<&str> = init.auth_methods.iter().map(|m| m.name.as_str()).collect();
                    info.detail = Some(format!("Signs in with: {}", names.join(", ")));
                }
            }
            // An agent that doesn't answer isn't usable: nothing picks it (delegation, the
            // composer) until a refresh finds it answering.
            Err(e) => {
                info.installed = false;
                info.detail = Some(format!("couldn't start {}: {e:#}", binary.display()));
            }
        }
        info
    }

    async fn start(&self, cfg: SessionConfig) -> anyhow::Result<(Box<dyn Session>, mpsc::Receiver<ProviderEvent>)> {
        let (rpc, inbound, process) = self.spawn(&cfg.env, Some(&cfg.cwd))?;
        let opened = tokio::time::timeout(START_TIMEOUT, session::open(rpc, process, inbound, &cfg)).await;
        match opened {
            Ok(Ok((session, events))) => Ok((Box::new(session), events)),
            Ok(Err(e)) => Err(e),
            Err(_) => anyhow::bail!("{} didn't start a session within {}s", self.name, START_TIMEOUT.as_secs()),
        }
    }
}

/// `1.2.3` out of an agent's version string (`agy_acp_server_1.1.1`, `v0.66.0`); the whole
/// string when there's no such number.
fn version(text: &str) -> String {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
        .flat_map(|w| w.split('_'))
        .map(|w| w.trim_start_matches('v'))
        .find(|w| w.split('.').count() >= 2 && w.split('.').all(|p| !p.is_empty()) && w.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or(text)
        .to_string()
}

/// The binary to run: a path as given, a bare name looked up on `PATH`.
pub fn which(binary: &Path) -> Option<PathBuf> {
    let executable = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                m.is_file() && m.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            m.is_file()
        })
    };
    if binary.components().count() > 1 {
        return executable(binary).then(|| binary.to_path_buf());
    }
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(binary)).find(|p| executable(p))
}

struct Known {
    name: &'static str,
    title: &'static str,
    binary: &'static str,
    args: &'static [&'static str],
    env: &'static [(&'static str, &'static str)],
}

/// The agents T3 Code knows, with the launch each one's own package installs on `PATH`:
/// T3's Grok, Kiro and fx drivers, and the ACP registry
/// (`cdn.agentclientprotocol.com/registry/v1/latest/registry.json`, which T3 reads) for the rest.
const KNOWN: &[Known] = &[
    Known { name: "gemini", title: "Gemini CLI", binary: "gemini", args: &["--acp"], env: &[] },
    Known { name: "opencode", title: "OpenCode", binary: "opencode", args: &["acp"], env: &[] },
    Known { name: "grok", title: "Grok Build", binary: "grok", args: &["agent", "stdio"], env: &[] },
    Known { name: "kiro", title: "Kiro CLI", binary: "kiro-cli", args: &["acp", "--agent-engine=v3", "--auth-method=cli"], env: &[] },
    Known { name: "fx", title: "fx", binary: "fx", args: &["acp"], env: &[] },
    Known { name: "qwen", title: "Qwen Code", binary: "qwen", args: &["--acp", "--experimental-skills"], env: &[] },
    Known { name: "goose", title: "goose", binary: "goose", args: &["acp"], env: &[] },
    Known { name: "copilot", title: "GitHub Copilot", binary: "copilot", args: &["--acp"], env: &[] },
    Known { name: "cursor", title: "Cursor", binary: "cursor-agent", args: &["acp"], env: &[] },
    Known { name: "auggie", title: "Auggie CLI", binary: "auggie", args: &["--acp"], env: &[("AUGMENT_DISABLE_AUTO_UPDATE", "1")] },
    Known { name: "kimi", title: "Kimi CLI", binary: "kimi", args: &["acp"], env: &[] },
    Known { name: "kilo", title: "Kilo", binary: "kilo", args: &["acp"], env: &[] },
    Known { name: "devin", title: "Devin", binary: "devin", args: &["acp"], env: &[] },
    Known {
        name: "droid",
        title: "Factory Droid",
        binary: "droid",
        args: &["exec", "--output-format", "acp-daemon"],
        env: &[("DROID_DISABLE_AUTO_UPDATE", "true"), ("FACTORY_DROID_AUTO_UPDATE_ENABLED", "false")],
    },
    Known { name: "cline", title: "Cline", binary: "cline", args: &["--acp"], env: &[] },
    Known { name: "mistral-vibe", title: "Mistral Vibe", binary: "vibe-acp", args: &[], env: &[] },
    Known { name: "amp", title: "Amp", binary: "amp-acp", args: &[], env: &[] },
    Known { name: "claude-acp", title: "Claude Agent (ACP)", binary: "claude-agent-acp", args: &[], env: &[] },
    Known { name: "codex-acp", title: "Codex (ACP)", binary: "codex-acp", args: &[], env: &[] },
];

/// Every ACP agent roll knows how to start, as `[[acp]]` entries.
pub fn known_agents() -> Vec<AcpAgent> {
    KNOWN
        .iter()
        .map(|k| AcpAgent {
            name: k.name.to_string(),
            config: ProviderConfig {
                binary: Some(PathBuf::from(k.binary)),
                args: k.args.iter().map(|a| a.to_string()).collect(),
                env: k.env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            },
        })
        .collect()
}

/// The known agents whose binary is on `PATH` (what `roll agent up` can offer).
pub fn installed_agents() -> Vec<AcpAgent> {
    known_agents().into_iter().filter(|a| a.config.binary.as_deref().and_then(which).is_some()).collect()
}

/// The ACP agents a daemon offers: everything in `config.toml`'s `[[acp]]`, then the known
/// agents found on `PATH` that the config doesn't already name.
pub fn agents(configured: &[AcpAgent]) -> Vec<AcpAgent> {
    merge(configured, installed_agents())
}

fn merge(configured: &[AcpAgent], discovered: Vec<AcpAgent>) -> Vec<AcpAgent> {
    let mut all = configured.to_vec();
    all.extend(discovered.into_iter().filter(|d| !configured.iter().any(|c| c.name == d.name)));
    all
}

/// The display name of a known agent.
pub fn known_title(name: &str) -> Option<&'static str> {
    KNOWN.iter().find(|k| k.name == name).map(|k| k.title)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_agents_win_over_discovered_ones() {
        let agent = |name: &str, bin: &str| AcpAgent {
            name: name.into(),
            config: ProviderConfig { binary: Some(PathBuf::from(bin)), ..ProviderConfig::default() },
        };
        let configured = [agent("gemini", "/opt/gemini"), agent("mine", "/opt/mine")];
        let all = merge(&configured, vec![agent("gemini", "gemini"), agent("goose", "goose")]);
        let got: Vec<_> = all.iter().map(|a| (a.name.as_str(), a.config.binary.as_deref().unwrap().to_str().unwrap())).collect();
        assert_eq!(got, [("gemini", "/opt/gemini"), ("mine", "/opt/mine"), ("goose", "goose")]);
    }

    #[test]
    fn known_agents_are_valid_config_entries() {
        let agents = known_agents();
        assert!(agents.len() >= 10);
        for name in ["gemini", "opencode", "grok", "kiro", "fx"] {
            assert!(agents.iter().any(|a| a.name == name), "{name}");
        }
        let mut names: Vec<_> = agents.iter().map(|a| a.name.as_str()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), agents.len());
        // Each is a valid `[[acp]]` entry.
        let toml: String = agents
            .iter()
            .map(|a| format!("[[acp]]\nname = {:?}\nbinary = {:?}\nargs = {:?}\n", a.name, a.config.binary.as_ref().unwrap(), a.config.args))
            .collect();
        let config = crate::config::Config::parse(&toml).unwrap();
        assert_eq!(config.acp.len(), agents.len());
        assert_eq!(known_title("kiro"), Some("Kiro CLI"));
    }

    #[test]
    fn versions_and_binaries() {
        assert_eq!(version("agy_acp_server_1.1.1"), "1.1.1");
        assert_eq!(version("v0.66.0"), "0.66.0");
        assert_eq!(version("2.32.0"), "2.32.0");
        assert_eq!(version("nightly"), "nightly");
        assert!(which(Path::new("sh")).is_some());
        assert!(which(Path::new("/bin/sh")).is_some());
        assert!(which(Path::new("roll-no-such-agent")).is_none());
    }
}
