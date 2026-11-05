//! The daemon's data directory and config file.
//!
//! The data directory is `$ROLL_AGENT_HOME`, else `~/.local/share/roll/agent`. It holds
//! `agent.db` (the entity store), `worktrees/<project>/<thread>` and `config.toml`:
//!
//! ```toml
//! [providers.claude]
//! binary = "/opt/claude/bin/claude"     # optional; found on PATH otherwise
//! env = { ANTHROPIC_BASE_URL = "https://…" }
//!
//! [providers.codex]
//! args = []
//!
//! [providers.opencode]                   # OpenCode 2 (`opencode serve`); 1.x runs as `acp:opencode`
//! binary = "/opt/opencode/bin/opencode"
//!
//! [providers.pi]                         # Pi (`pi --mode rpc`); args are Pi's launch arguments
//! args = ["--provider", "openrouter", "--model", "deepseek/deepseek-v4-flash"]
//!
//! [providers.muse]                       # Muse Code (`muse serve`); signs in with `muse login`
//! binary = "/opt/muse/bin/muse"
//!
//! [[acp]]                                # an ACP agent, provider id `acp:gemini`
//! name = "gemini"
//! binary = "gemini"
//! args = ["--experimental-acp"]
//! env = { GEMINI_API_KEY = "…" }
//!
//! [delegation]                           # roll's orchestrator MCP (docs/code/agents.md)
//! max_depth = 3                          # subagents of subagents, at most this deep
//! max_children = 6                       # unfinished delegated tasks per thread
//!
//! [code]                                 # roll code repos cloned as projects
//! clone_dir = "~/src"                    # where they go (default ~/roll)
//!
//! [projects]                             # auto-pull (docs/code/agents.md, "Project settings")
//! pull_every = 60                        # seconds between fetches; 0 never
//! ```

use crate::provider::ProviderConfig;
use agent_proto::ProviderId;
use anyhow::Context;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default)]
pub struct Config {
    /// The data directory.
    pub home: PathBuf,
    /// `[providers.<id>]`, keyed by provider id (`codex`, `claude`, `opencode`, `muse`).
    pub providers: BTreeMap<String, ProviderConfig>,
    /// `[[acp]]`: ACP agents, in file order.
    pub acp: Vec<AcpAgent>,
    pub delegation: Delegation,
    /// `[code] clone_dir`: where roll code repos are cloned as projects (`None`: `~/roll`).
    pub clone_dir: Option<PathBuf>,
    /// `[projects] pull_every`; read it through [`Config::pull_every`].
    pub pull_every: Option<std::time::Duration>,
}

/// `[delegation]`: limits on what agents may delegate through roll's orchestrator MCP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Delegation {
    /// How deep subagents may nest: a thread the user started is depth 0, its subagents 1, …
    pub max_depth: u32,
    /// How many unfinished delegated tasks one thread may have at once.
    pub max_children: u32,
}

impl Default for Delegation {
    fn default() -> Self {
        Delegation { max_depth: 3, max_children: 6 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcpAgent {
    pub name: String,
    pub config: ProviderConfig,
}

impl AcpAgent {
    pub fn provider_id(&self) -> ProviderId {
        ProviderId(format!("acp:{}", self.name))
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    providers: BTreeMap<String, RawProvider>,
    #[serde(default)]
    acp: Vec<RawAcp>,
    #[serde(default)]
    delegation: RawDelegation,
    #[serde(default)]
    code: RawCode,
    #[serde(default)]
    projects: RawProjects,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawProjects {
    pull_every: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawCode {
    clone_dir: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawDelegation {
    max_depth: Option<u32>,
    max_children: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawProvider {
    binary: Option<PathBuf>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAcp {
    name: String,
    binary: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

impl From<RawProvider> for ProviderConfig {
    fn from(raw: RawProvider) -> Self {
        ProviderConfig { binary: raw.binary, args: raw.args, env: raw.env.into_iter().collect(), ..Default::default() }
    }
}

/// `$ROLL_AGENT_HOME`, else `~/.local/share/roll/agent`.
pub fn default_home() -> anyhow::Result<PathBuf> {
    if let Some(home) = std::env::var_os("ROLL_AGENT_HOME").filter(|h| !h.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    let home = user_home().context("no home directory (set ROLL_AGENT_HOME)")?;
    Ok(home.join(".local/share/roll/agent"))
}

pub fn user_home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

impl Config {
    /// The config in the default data directory.
    pub fn load() -> anyhow::Result<Config> {
        Config::load_from(default_home()?)
    }

    /// The config in `home` (an absent `config.toml` is the defaults).
    pub fn load_from(home: PathBuf) -> anyhow::Result<Config> {
        let path = home.join("config.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut config = Config::parse(&text).with_context(|| format!("in {}", path.display()))?;
        config.home = home;
        Ok(config)
    }

    /// Parses a `config.toml` (`home` is left empty).
    pub fn parse(text: &str) -> anyhow::Result<Config> {
        let file: File = toml::from_str(text)?;
        let mut acp = Vec::new();
        for raw in file.acp {
            if raw.name.is_empty() || !raw.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                anyhow::bail!("acp agent name {:?} must be letters, digits, '-' or '_'", raw.name);
            }
            if acp.iter().any(|a: &AcpAgent| a.name == raw.name) {
                anyhow::bail!("acp agent {:?} is listed twice", raw.name);
            }
            acp.push(AcpAgent {
                name: raw.name,
                config: ProviderConfig {
                    binary: Some(raw.binary),
                    args: raw.args,
                    env: raw.env.into_iter().collect(),
                    ..Default::default()
                },
            });
        }
        Ok(Config {
            home: PathBuf::new(),
            providers: file.providers.into_iter().map(|(k, v)| (k, v.into())).collect(),
            acp,
            delegation: Delegation {
                max_depth: file.delegation.max_depth.unwrap_or(Delegation::default().max_depth),
                max_children: file.delegation.max_children.unwrap_or(Delegation::default().max_children),
            },
            clone_dir: file.code.clone_dir.filter(|d| !d.trim().is_empty()).map(|d| expand_tilde(d.trim())),
            pull_every: file.projects.pull_every.map(std::time::Duration::from_secs),
        })
    }

    /// Where roll code repos are cloned: `[code] clone_dir`, else `~/roll`.
    pub fn clone_dir(&self) -> Option<PathBuf> {
        self.clone_dir.clone().or_else(|| user_home().map(|h| h.join("roll")))
    }

    /// The config for a provider id (`acp:<name>` looks in `[[acp]]`); defaults when unlisted.
    pub fn provider(&self, id: &ProviderId) -> ProviderConfig {
        if let Some(name) = id.as_str().strip_prefix("acp:") {
            return self.acp.iter().find(|a| a.name == name).map(|a| a.config.clone()).unwrap_or_default();
        }
        self.providers.get(id.as_str()).cloned().unwrap_or_default()
    }

    pub fn db_path(&self) -> PathBuf {
        self.home.join("agent.db")
    }

    pub fn worktrees(&self) -> PathBuf {
        self.home.join("worktrees")
    }

    pub fn worktree_path(&self, project: &str, thread: &str) -> PathBuf {
        self.worktrees().join(project).join(thread)
    }

    /// How often projects with auto-pull fetch their remote (`[projects] pull_every`, seconds;
    /// 60 by default, 0 never). roll code projects don't poll: the relay asks when they move.
    pub fn pull_every(&self) -> Option<std::time::Duration> {
        match self.pull_every {
            None => Some(std::time::Duration::from_secs(60)),
            Some(d) if d.is_zero() => None,
            Some(d) => Some(d),
        }
    }
}

/// `~` and `~/…` expanded against the user's home.
pub fn expand_tilde(path: &str) -> PathBuf {
    match (path.strip_prefix('~'), user_home()) {
        (Some(""), Some(home)) => home,
        (Some(rest), Some(home)) if rest.starts_with('/') => home.join(rest.trim_start_matches('/')),
        _ => Path::new(path).to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_providers_and_acp_agents() {
        let config = Config::parse(
            r#"
            [providers.claude]
            binary = "/opt/claude"
            env = { ANTHROPIC_BASE_URL = "https://example.test" }

            [[acp]]
            name = "gemini"
            binary = "gemini"
            args = ["--experimental-acp"]
            "#,
        )
        .unwrap();
        let claude = config.provider(&ProviderId::claude());
        assert_eq!(claude.binary, Some(PathBuf::from("/opt/claude")));
        assert_eq!(claude.env, vec![("ANTHROPIC_BASE_URL".to_string(), "https://example.test".to_string())]);
        assert_eq!(config.provider(&ProviderId::codex()), ProviderConfig::default());
        let gemini = config.provider(&ProviderId("acp:gemini".into()));
        assert_eq!(gemini.args, vec!["--experimental-acp"]);
        assert_eq!(config.acp[0].provider_id().as_str(), "acp:gemini");
    }

    #[test]
    fn refuses_bad_acp_names_and_unknown_keys() {
        assert!(Config::parse("[[acp]]\nname = \"a b\"\nbinary = \"x\"").is_err());
        assert!(Config::parse("[[acp]]\nname = \"a\"\nbinary = \"x\"\n[[acp]]\nname = \"a\"\nbinary = \"y\"").is_err());
        assert!(Config::parse("colour = 1").is_err());
        assert!(Config::parse("").unwrap().providers.is_empty());
    }

    #[test]
    fn delegation_limits_default_and_parse() {
        assert_eq!(Config::parse("").unwrap().delegation, Delegation::default());
        let config = Config::parse("[delegation]\nmax_depth = 1").unwrap();
        assert_eq!(config.delegation, Delegation { max_depth: 1, max_children: Delegation::default().max_children });
        assert!(Config::parse("[delegation]\ndepth = 1").is_err());
    }

    #[test]
    fn clone_dir_parses_and_expands() {
        assert_eq!(Config::parse("").unwrap().clone_dir, None);
        assert_eq!(Config::parse("[code]\nclone_dir = \"/srv/src\"").unwrap().clone_dir, Some(PathBuf::from("/srv/src")));
        assert!(Config::parse("[code]\ndir = \"x\"").is_err());
    }

    #[test]
    fn auto_pull_looks_every_minute_unless_told_otherwise() {
        assert_eq!(Config::parse("").unwrap().pull_every(), Some(std::time::Duration::from_secs(60)));
        assert_eq!(Config::parse("[projects]\npull_every = 300").unwrap().pull_every(), Some(std::time::Duration::from_secs(300)));
        assert_eq!(Config::parse("[projects]\npull_every = 0").unwrap().pull_every(), None);
        assert!(Config::parse("[projects]\nevery = 1").is_err());
    }
}
