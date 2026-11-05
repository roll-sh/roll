//! The ACP registry on this machine (docs/code/agents.md, "The ACP registry"): T3 Code's
//! `packages/provider-acp-registry` (`AcpRegistrySupport.ts`: the catalog's fetch, cache and
//! installs), with the daemon doing all of it and the relay only forwarding.
//!
//! - The index is fetched from [`DEFAULT_URL`] (or `config.toml`'s `[registry] url`), at most
//!   [`index::MAX_BYTES`], redirects only on the same host, with a timeout. A copy is kept at
//!   `<home>/acp/registry.json`: `registry.list` reuses what it fetched for [`LIST_MAX_AGE`], and
//!   falls back to the copy (saying why) when the registry can't be reached.
//! - Nothing installs while a provider is probed: only `registry.install` does. A binary is
//!   downloaded and unpacked into `<home>/acp/tools/<id>/<version>/<target>/` (`archive.rs`); a
//!   package goes into `<home>/acp/tools/<id>/<version>/{npm,python}` with the machine's own
//!   `npm`/`uv` (`packages.rs`). One install per agent at a time; a failed one leaves nothing.
//! - What was installed is kept in `<home>/acp/installed.json` (the command, its arguments and
//!   the registry's variables), so the next start offers it ([`acp_providers`]).
//! - Registry variables that could hold a credential are never used (`index.rs`).

pub mod archive;
pub mod index;
mod packages;

use crate::config::{AcpAgent, Config};
use crate::provider::acp::{self, Acp};
use crate::provider::{Provider, ProviderConfig};
use agent_proto::{Millis, ProviderId, RegistryInstalled, RegistryKind, is_credential_env, is_registry_id};
use anyhow::{Context, bail};
use index::{Agent, Choice, Index};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The official registry (the one T3 Code reads).
pub const DEFAULT_URL: &str = "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";
/// `registry.list` reuses a fetch this recent (T3's `REGISTRY_SEARCH_MAX_AGE_MS`); an install
/// always fetches.
pub const LIST_MAX_AGE: Duration = Duration::from_secs(5 * 60);
/// T3's `REGISTRY_REQUEST_TIMEOUT`.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// T3's `ARCHIVE_REQUEST_TIMEOUT`.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const MAX_INSTALLED_FILE: u64 = 1 << 20;

/// An agent the daemon installed, as `installed.json` keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    pub id: String,
    /// The registry's name for it.
    pub name: String,
    pub version: String,
    pub kind: RegistryKind,
    /// The command, absolute, inside `<home>/acp/tools/<id>/`.
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub installed_at: Millis,
}

impl Installed {
    pub fn provider_id(&self) -> ProviderId {
        ProviderId(format!("acp:{}", self.id))
    }

    /// Its `[[acp]]`-style entry: the provider it runs as.
    pub fn agent(&self) -> AcpAgent {
        AcpAgent {
            name: self.id.clone(),
            config: ProviderConfig {
                binary: Some(self.command.clone()),
                args: self.args.clone(),
                env: self.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                ..Default::default()
            },
        }
    }

    pub fn provider(&self) -> Arc<dyn Provider> {
        let a = self.agent();
        Arc::new(Acp::new(a.name, a.config).with_title(self.name.clone()))
    }

    pub fn wire(&self) -> RegistryInstalled {
        RegistryInstalled { id: self.id.clone(), version: self.version.clone(), provider: self.provider_id(), kind: self.kind, installed_at: self.installed_at }
    }
}

/// What the daemon knows of the index.
pub struct Listed {
    pub index: Option<Arc<Index>>,
    /// When it was fetched (0: never).
    pub fetched_at: Millis,
    /// Why this isn't a fresh fetch.
    pub error: Option<String>,
}

struct Fetched {
    index: Arc<Index>,
    at: Millis,
    /// When it came from the registry (`None`: from the kept copy).
    when: Option<Instant>,
}

pub struct Registry {
    /// `<home>/acp`.
    dir: PathBuf,
    url: String,
    /// The configured registry's origin when it's plain HTTP (a local copy, in tests): its
    /// archives may be HTTP too. Everything else must be HTTPS.
    insecure: Option<String>,
    index_client: reqwest::Client,
    download_client: reqwest::Client,
    /// Also one fetch at a time.
    fetched: tokio::sync::Mutex<Option<Fetched>>,
    installed: std::sync::Mutex<Vec<Installed>>,
    /// Agents being installed or removed now.
    busy: std::sync::Mutex<HashSet<String>>,
    limits: archive::Limits,
}

fn now() -> Millis {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn origin(url: &reqwest::Url) -> String {
    url.origin().ascii_serialization()
}

impl Registry {
    pub fn new(config: &Config) -> Registry {
        let dir = config.home.join("acp");
        let url = config.registry_url.clone().unwrap_or_else(|| DEFAULT_URL.into());
        let insecure = reqwest::Url::parse(&url).ok().filter(|u| u.scheme() == "http").map(|u| origin(&u));
        let allowed = insecure.clone();
        let same_host = reqwest::redirect::Policy::custom(|a| {
            let first = a.previous().first().map(|u| (u.scheme().to_string(), u.host_str().map(str::to_string)));
            if a.previous().len() > 5 {
                a.error("too many redirects")
            } else if first == Some((a.url().scheme().to_string(), a.url().host_str().map(str::to_string))) {
                a.follow()
            } else {
                a.stop()
            }
        });
        // Release downloads redirect to another host (GitHub's object store): HTTPS only.
        let https_only = reqwest::redirect::Policy::custom(move |a| {
            let ok = a.url().scheme() == "https" || allowed.as_deref() == Some(origin(a.url()).as_str());
            if a.previous().len() > 5 {
                a.error("too many redirects")
            } else if ok {
                a.follow()
            } else {
                a.error("a redirect left HTTPS")
            }
        });
        let client = |redirect, timeout| {
            reqwest::Client::builder()
                .redirect(redirect)
                .connect_timeout(Duration::from_secs(15))
                .timeout(timeout)
                .user_agent(concat!("roll-agent/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("an HTTP client")
        };
        Registry {
            installed: std::sync::Mutex::new(load_installed(&dir)),
            dir,
            url,
            insecure,
            index_client: client(same_host, FETCH_TIMEOUT),
            download_client: client(https_only, DOWNLOAD_TIMEOUT),
            fetched: tokio::sync::Mutex::new(None),
            busy: std::sync::Mutex::new(HashSet::new()),
            limits: archive::Limits::default(),
        }
    }

    /// Caps archives at `limits` instead of the defaults (tests).
    pub fn set_limits(&mut self, limits: archive::Limits) {
        self.limits = limits;
    }

    pub fn installed(&self) -> Vec<Installed> {
        self.installed.lock().unwrap().clone()
    }

    pub fn installed_one(&self, id: &str) -> Option<Installed> {
        self.installed.lock().unwrap().iter().find(|i| i.id == id).cloned()
    }

    /// The index: what was fetched within `max_age` (`None`: always fetch), else a new fetch,
    /// else the copy kept on disk, with why.
    pub async fn index(&self, max_age: Option<Duration>) -> Listed {
        let mut fetched = self.fetched.lock().await;
        if let (Some(f), Some(max)) = (fetched.as_ref(), max_age)
            && f.when.is_some_and(|w| w.elapsed() < max)
        {
            return Listed { index: Some(f.index.clone()), fetched_at: f.at, error: None };
        }
        let error = match self.fetch().await {
            Ok((index, text)) => {
                if let Err(e) = self.keep(&text) {
                    tracing::warn!("couldn't keep a copy of the ACP registry: {e:#}");
                }
                let f = fetched.insert(Fetched { index: Arc::new(index), at: now(), when: Some(Instant::now()) });
                return Listed { index: Some(f.index.clone()), fetched_at: f.at, error: None };
            }
            Err(e) => format!("couldn't fetch the ACP registry: {e:#}"),
        };
        if fetched.is_none()
            && let Some((index, at)) = self.kept()
        {
            *fetched = Some(Fetched { index: Arc::new(index), at, when: None });
        }
        match fetched.as_ref() {
            Some(f) => Listed { index: Some(f.index.clone()), fetched_at: f.at, error: Some(error) },
            None => Listed { index: None, fetched_at: 0, error: Some(error) },
        }
    }

    async fn fetch(&self) -> anyhow::Result<(Index, String)> {
        let mut res = self.index_client.get(&self.url).send().await?;
        if !res.status().is_success() {
            bail!("{} answered {}", self.url, res.status());
        }
        let mut body = Vec::new();
        while let Some(chunk) = res.chunk().await? {
            if body.len() + chunk.len() > index::MAX_BYTES {
                bail!("the registry is over {} bytes", index::MAX_BYTES);
            }
            body.extend_from_slice(&chunk);
        }
        let text = String::from_utf8(body).context("the registry isn't UTF-8")?;
        let index = index::parse(&text, self.insecure.as_deref()).map_err(anyhow::Error::msg)?;
        Ok((index, text))
    }

    fn cache_path(&self) -> PathBuf {
        self.dir.join("registry.json")
    }

    fn keep(&self, text: &str) -> anyhow::Result<()> {
        write_atomic(&self.cache_path(), text.as_bytes())
    }

    /// The copy kept on disk and when it was written.
    fn kept(&self) -> Option<(Index, Millis)> {
        let path = self.cache_path();
        let meta = std::fs::metadata(&path).ok().filter(|m| m.len() <= index::MAX_BYTES as u64)?;
        let text = std::fs::read_to_string(&path).ok()?;
        let at = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64);
        Some((index::parse(&text, self.insecure.as_deref()).ok()?, at))
    }

    /// What an install of `agent` would use here: a binary for this machine, else a package whose
    /// manager is on `PATH`.
    pub fn choose<'a>(&self, agent: &'a Agent) -> Option<Choice<'a>> {
        agent.choices(index::platform()).into_iter().find(|c| match c {
            Choice::Binary { .. } => true,
            Choice::Npx(_) => acp::which(Path::new("npm")).is_some(),
            Choice::Uvx(_) => acp::which(Path::new("uv")).is_some(),
        })
    }

    fn tools(&self, id: &str) -> PathBuf {
        self.dir.join("tools").join(id)
    }

    /// Installs `agent` and records it; the files are in place and `installed.json` written when
    /// this returns. Another install or removal of the same agent fails at once.
    pub async fn install(&self, agent: &Agent) -> anyhow::Result<Installed> {
        let _busy = self.hold(&agent.id)?;
        let choice = self.choose(agent).with_context(|| match agent.choices(index::platform()).first() {
            Some(Choice::Npx(_)) => format!("{} needs npm on this machine's PATH", agent.name),
            Some(Choice::Uvx(_)) => format!("{} needs uv on this machine's PATH", agent.name),
            _ => format!("the registry has no {} for {}", agent.name, index::platform().unwrap_or(std::env::consts::OS)),
        })?;
        let version_dir = self.tools(&agent.id).join(&agent.version);
        let installed = match choice {
            Choice::Binary { target, binary } => self.install_binary(binary, &version_dir.join(target)).await,
            Choice::Npx(pkg) => fresh(&version_dir.join("npm"), packages::npm(Path::new("npm"), &agent.id, pkg, &version_dir.join("npm"))).await,
            Choice::Uvx(pkg) => fresh(&version_dir.join("python"), packages::uv(Path::new("uv"), &agent.id, pkg, &version_dir.join("python"))).await,
        };
        let command = installed.inspect_err(|_| {
            // Folders the attempt made and left empty.
            let _ = std::fs::remove_dir(&version_dir);
            let _ = std::fs::remove_dir(self.tools(&agent.id));
        })?;
        let launch = choice.launch();
        let done = Installed {
            id: agent.id.clone(),
            name: agent.name.clone(),
            version: agent.version.clone(),
            kind: choice.kind(),
            command,
            args: launch.args.clone(),
            env: launch.env.iter().cloned().collect(),
            installed_at: now(),
        };
        {
            let mut all = self.installed.lock().unwrap();
            let mut next = all.clone();
            next.retain(|i| i.id != done.id);
            next.push(done.clone());
            next.sort_by(|a, b| a.id.cmp(&b.id));
            save_installed(&self.dir, &next)?;
            *all = next;
        }
        // Other versions of it are no longer used.
        if let Ok(dir) = std::fs::read_dir(self.tools(&agent.id)) {
            for e in dir.flatten() {
                if e.file_name() != agent.version.as_str() {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        Ok(done)
    }

    /// Downloads and unpacks into `dest` (replacing it), through a temporary folder beside it.
    async fn install_binary(&self, binary: &index::Binary, dest: &Path) -> anyhow::Result<PathBuf> {
        let parent = dest.parent().expect("under tools/<id>");
        std::fs::create_dir_all(parent)?;
        let tmp = Tmp(parent.join(format!(".install-{:016x}", rand::random::<u64>())));
        std::fs::create_dir(&tmp.0)?;
        let archive_path = tmp.0.join("download");
        archive::download(&self.download_client, &binary.archive, &archive_path, binary.sha256.as_deref(), self.limits.download).await?;
        let root = tmp.0.join("root");
        std::fs::create_dir(&root)?;
        let (kind, cmd, limits) = (archive::kind(&binary.archive), binary.cmd.clone(), self.limits);
        let r = root.clone();
        tokio::task::spawn_blocking(move || archive::unpack(kind, &archive_path, &r, &cmd, limits)).await??;
        if dest.exists() {
            std::fs::remove_dir_all(dest)?;
        }
        std::fs::rename(&root, dest)?;
        Ok(dest.join(binary.cmd.iter().collect::<PathBuf>()))
    }

    /// Forgets an installed agent and deletes its files.
    pub fn remove(&self, id: &str) -> anyhow::Result<()> {
        let _busy = self.hold(id)?;
        {
            let mut all = self.installed.lock().unwrap();
            let next: Vec<Installed> = all.iter().filter(|i| i.id != id).cloned().collect();
            save_installed(&self.dir, &next)?;
            *all = next;
        }
        let tools = self.tools(id);
        if tools.exists() {
            std::fs::remove_dir_all(&tools).with_context(|| format!("removing {}", tools.display()))?;
        }
        Ok(())
    }

    fn hold(&self, id: &str) -> anyhow::Result<Busy<'_>> {
        if !self.busy.lock().unwrap().insert(id.to_string()) {
            bail!("{id} is being installed or removed already");
        }
        Ok(Busy(self, id.to_string()))
    }
}

struct Busy<'a>(&'a Registry, String);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.busy.lock().unwrap().remove(&self.1);
    }
}

/// A temporary folder removed when dropped.
struct Tmp(PathBuf);

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs a package install into `dir` from scratch; a failure leaves nothing there.
async fn fresh(dir: &Path, install: impl std::future::Future<Output = anyhow::Result<PathBuf>>) -> anyhow::Result<PathBuf> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    let got = install.await;
    if got.is_err() {
        let _ = std::fs::remove_dir_all(dir);
    }
    got
}

fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let dir = path.parent().expect("a file in a folder");
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.{:016x}.tmp", path.file_name().unwrap_or_default().to_string_lossy(), rand::random::<u64>()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

fn save_installed(dir: &Path, all: &[Installed]) -> anyhow::Result<()> {
    write_atomic(&dir.join("installed.json"), &serde_json::to_vec_pretty(all)?).context("writing installed.json")
}

/// `<acp dir>/installed.json`; entries that don't hold up (an id that isn't the registry's, a
/// command outside the agent's folder) are dropped, and so is a variable that could hold a
/// credential.
fn load_installed(dir: &Path) -> Vec<Installed> {
    let path = dir.join("installed.json");
    let Ok(meta) = std::fs::metadata(&path) else { return Vec::new() };
    let parsed = (meta.len() <= MAX_INSTALLED_FILE)
        .then(|| std::fs::read(&path).ok())
        .flatten()
        .and_then(|b| serde_json::from_slice::<Vec<Installed>>(&b).map_err(|e| tracing::warn!("ignoring {}: {e}", path.display())).ok())
        .unwrap_or_default();
    let tools = dir.join("tools");
    let mut out: Vec<Installed> = Vec::new();
    for mut i in parsed {
        let ok = is_registry_id(&i.id) && i.command.is_absolute() && i.command.starts_with(tools.join(&i.id)) && !out.iter().any(|o| o.id == i.id);
        if !ok {
            tracing::warn!(agent = %i.id, "ignoring an entry of {}", path.display());
            continue;
        }
        i.env.retain(|k, _| !is_credential_env(k));
        out.push(i);
    }
    out
}

/// The agents the daemon installed in `config.home`.
pub fn installed_agents(config: &Config) -> Vec<Installed> {
    load_installed(&config.home.join("acp"))
}

/// The ACP providers a daemon offers, in order: `config.toml`'s `[[acp]]`, then the agents it
/// installed from the registry, then the known agents found on `PATH`; a name taken earlier
/// wins.
pub fn acp_providers(config: &Config) -> Vec<Arc<dyn Provider>> {
    let mut names: HashSet<String> = config.acp.iter().map(|a| a.name.clone()).collect();
    let mut out: Vec<Arc<dyn Provider>> = config.acp.iter().map(|a| Arc::new(Acp::new(a.name.clone(), a.config.clone())) as Arc<dyn Provider>).collect();
    for i in installed_agents(config) {
        if names.insert(i.id.clone()) {
            out.push(i.provider());
        }
    }
    for a in acp::installed_agents() {
        if names.insert(a.name.clone()) {
            out.push(Arc::new(Acp::new(a.name, a.config)));
        }
    }
    out
}

/// Providers that are a registry agent under another name: a known ACP agent's own name, or a
/// native driver (T3's built-in OpenCode and Pi).
const SAME_AS: &[(&str, &[&str])] = &[
    ("github-copilot-cli", &["acp:copilot"]),
    ("factory-droid", &["acp:droid"]),
    ("grok-build", &["acp:grok"]),
    ("qwen-code", &["acp:qwen"]),
    ("antigravity-acp", &["acp:antigravity"]),
    ("amp-acp", &["acp:amp"]),
    ("opencode", &["opencode"]),
    ("opencode-v2", &["opencode", "acp:opencode"]),
    ("pi-acp", &["pi"]),
];

/// The provider ids a registry agent could already be on this machine as, `acp:<id>` first.
pub fn same_as(id: &str) -> Vec<ProviderId> {
    let mut out = vec![ProviderId(format!("acp:{id}"))];
    if let Some((_, others)) = SAME_AS.iter().find(|(r, _)| *r == id) {
        out.extend(others.iter().map(|o| ProviderId(o.to_string())));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(dir: &Path, id: &str) -> Installed {
        Installed {
            id: id.into(),
            name: id.into(),
            version: "1.0.0".into(),
            kind: RegistryKind::Binary,
            command: dir.join("tools").join(id).join("1.0.0/linux-x86_64/agent"),
            args: vec!["acp".into()],
            env: [("AGENT_NO_UPDATE".to_string(), "1".to_string()), ("AGENT_API_KEY".to_string(), "x".to_string())].into(),
            installed_at: 1,
        }
    }

    #[test]
    fn installed_json_round_trips_and_drops_what_doesnt_hold_up() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut outside = installed(dir, "outside");
        outside.command = PathBuf::from("/usr/bin/sh");
        let mut bad_id = installed(dir, "Bad");
        bad_id.command = dir.join("tools/Bad/x");
        save_installed(dir, &[installed(dir, "goose"), outside, bad_id, installed(dir, "goose")]).unwrap();
        let got = load_installed(dir);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "goose");
        assert_eq!(got[0].env.keys().collect::<Vec<_>>(), ["AGENT_NO_UPDATE"]);
        assert_eq!(got[0].agent().config.binary.as_deref(), Some(dir.join("tools/goose/1.0.0/linux-x86_64/agent").as_path()));
        std::fs::write(dir.join("installed.json"), "not json").unwrap();
        assert!(load_installed(dir).is_empty());
    }

    #[test]
    fn registry_ids_map_onto_providers_roll_already_has() {
        assert_eq!(same_as("goose"), [ProviderId("acp:goose".into())]);
        assert_eq!(same_as("github-copilot-cli")[1], ProviderId("acp:copilot".into()));
        assert_eq!(same_as("opencode-v2")[1..], [ProviderId("opencode".into()), ProviderId("acp:opencode".into())]);
        assert_eq!(same_as("pi-acp")[1], ProviderId("pi".into()));
    }
}
