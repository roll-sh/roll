//! Roll-hosted sandboxes (docs/code/agents.md, "Sandboxes"): machines roll runs for an owner, each
//! a container running `roll agent up` with a machine token of its own, so it shows up on the canvas
//! like any other machine (`Machine.sandbox`).
//!
//! - A [`SandboxBackend`] creates, starts, stops and deletes them: [`agent37`] (the hosted
//!   product) or [`docker`] (self-hosted and development).
//! - Requests record what the owner wants (`desired`); the owner's reconciler ([`reconcile`])
//!   makes the backend match, syncs what it reports into the sandbox and its machine (a `machine`
//!   event), and meters running time for billing. It runs on the node holding the lease
//!   `agents:<owner id>`, like everything else of the owner's agents (docs/code/cluster.md).
//! - Sandboxes run only while their agents work ([`wake`]): the reconciler puts one to sleep
//!   (stops it) when its machine has been idle for `idle_stop`, and a command to it wakes it.
//! - What a sandbox needs to start (its machine token and the provider env the owner gave) is
//!   sealed with `ROLL_SECRET_KEY` until the backend has it, then dropped. It goes to the backend
//!   and nowhere else: never into a response, a log or an error.

pub mod agent37;
pub mod api;
pub mod docker;
mod reconcile;
pub mod store;
mod wake;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use tokio::sync::Notify;

/// A size owners pick, and its price.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Size {
    pub name: String,
    pub cpu: u32,
    pub memory_gb: u32,
    pub disk_gb: u32,
    /// What it costs per running hour, in US cents.
    pub hourly_cents: u32,
}

impl Size {
    fn new(name: &str, cpu: u32, memory_gb: u32, disk_gb: u32, hourly_cents: u32) -> Size {
        Size { name: name.into(), cpu, memory_gb, disk_gb, hourly_cents }
    }
}

/// What a sandbox should be: what the owner asked for, or asleep (the reconciler stopped it while
/// idle; a command to it wakes it, as one to an owner-stopped sandbox does).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Desired {
    Running,
    Stopped,
    Asleep,
    Deleted,
}

impl Desired {
    pub fn as_str(self) -> &'static str {
        match self {
            Desired::Running => "running",
            Desired::Stopped => "stopped",
            Desired::Asleep => "asleep",
            Desired::Deleted => "deleted",
        }
    }

    fn parse(s: &str) -> Desired {
        match s {
            "stopped" => Desired::Stopped,
            "asleep" => Desired::Asleep,
            "deleted" => Desired::Deleted,
            _ => Desired::Running,
        }
    }

    /// Stopped at the backend, for whatever reason.
    pub fn is_stopped(self) -> bool {
        matches!(self, Desired::Stopped | Desired::Asleep)
    }
}

/// Sandboxes on this server (`Config::sandboxes`).
#[derive(Clone, Debug)]
pub struct SandboxConfig {
    /// Where sandboxes run; `None` turns them off (creating one is a 503).
    pub backend: Option<BackendConfig>,
    /// What owners may pick from; the first is the default.
    pub sizes: Vec<Size>,
    /// Sandboxes one owner may have.
    pub max_per_owner: u32,
    /// How often an owner's reconciler looks at the backend (and meters).
    pub reconcile_every: Duration,
    /// A sandbox the backend hasn't created by then has failed.
    pub create_timeout: Duration,
    /// How often what the backend holds for roll is checked for strays ([`Code::sweep_sandboxes`]).
    pub sweep_every: Duration,
    /// The API URL sandboxes connect to (`ROLL_API_URL` inside them); default `Config::api_url`.
    pub api_url: Option<String>,
    /// A running sandbox whose machine did nothing for this long goes to sleep (is stopped until
    /// a command wakes it); zero keeps sandboxes up.
    pub idle_stop: Duration,
    /// How long a command to a sleeping (or stopped) sandbox waits for it to wake and connect.
    pub wake_timeout: Duration,
}

#[derive(Clone, Debug)]
pub enum BackendConfig {
    Agent37(agent37::Agent37Config),
    Docker(docker::DockerConfig),
}

impl Default for SandboxConfig {
    fn default() -> Self {
        SandboxConfig {
            backend: None,
            sizes: default_sizes(),
            max_per_owner: 5,
            reconcile_every: Duration::from_secs(30),
            create_timeout: Duration::from_secs(15 * 60),
            sweep_every: Duration::from_secs(10 * 60),
            api_url: None,
            idle_stop: Duration::from_secs(5 * 60),
            wake_timeout: Duration::from_secs(180),
        }
    }
}

/// `300`, `300s`, `5m` or `1h`.
fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (n, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: u64 = n.parse().ok()?;
    let secs = match unit.trim() {
        "s" => n,
        "m" => n.checked_mul(60)?,
        "h" => n.checked_mul(3600)?,
        _ => return None,
    };
    Some(Duration::from_secs(secs))
}

/// 3¢ an hour for the 2 vCPU / 4 GB, and the larger agent37 shapes.
pub fn default_sizes() -> Vec<Size> {
    vec![Size::new("standard", 2, 4, 8, 3), Size::new("large", 4, 8, 16, 6), Size::new("xlarge", 4, 16, 32, 10)]
}

/// `name=cpu:memory_gb:disk_gb:hourly_cents,…`.
pub fn parse_sizes(s: &str) -> anyhow::Result<Vec<Size>> {
    let mut out: Vec<Size> = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (name, rest) = part.split_once('=').ok_or_else(|| anyhow::anyhow!("size {part:?}: expected name=cpu:memory_gb:disk_gb:hourly_cents"))?;
        let n: Vec<u32> = rest.split(':').map(|x| x.trim().parse::<u32>()).collect::<Result<_, _>>().map_err(|_| anyhow::anyhow!("size {part:?}: numbers expected"))?;
        let name = name.trim();
        if n.len() != 4 || n[0] == 0 || n[1] == 0 || name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            anyhow::bail!("size {part:?}: expected name=cpu:memory_gb:disk_gb:hourly_cents");
        }
        if out.iter().any(|s| s.name == name) {
            anyhow::bail!("size {name} is named twice");
        }
        out.push(Size::new(name, n[0], n[1], n[2], n[3]));
    }
    if out.is_empty() {
        anyhow::bail!("no sizes");
    }
    Ok(out)
}

impl SandboxConfig {
    /// From the environment:
    /// - `ROLL_SANDBOX_BACKEND`: `agent37` (the default when `ROLL_AGENT37_API_KEY` is set) or `docker`
    /// - agent37: `ROLL_AGENT37_API_KEY`, `ROLL_AGENT37_URL`, `ROLL_AGENT37_TEMPLATE`, `ROLL_AGENT37_REGION`
    /// - docker: `ROLL_SANDBOX_IMAGE`, `ROLL_SANDBOX_RUNTIME` (`auto`: `runsc` when Docker has it),
    ///   `ROLL_SANDBOX_DOCKER_NETWORK` (default: `roll-sandboxes`, made on first use),
    ///   `ROLL_SANDBOX_DOCKER_HOST_GATEWAY` (`1`: `host.docker.internal`), `ROLL_SANDBOX_DOCKER_DISK_LIMIT`
    ///   (`1`: `--storage-opt size=`)
    /// - `ROLL_SANDBOX_SIZES` (`name=cpu:memory_gb:disk_gb:hourly_cents,…`), `ROLL_SANDBOX_MAX_PER_OWNER`,
    ///   `ROLL_SANDBOX_API_URL`
    /// - `ROLL_SANDBOX_IDLE_STOP` (`5m`; `0` keeps sandboxes up), `ROLL_SANDBOX_WAKE_TIMEOUT` (`180s`):
    ///   seconds, or with `s`, `m` or `h`
    pub fn from_env() -> anyhow::Result<SandboxConfig> {
        Self::from_lookup(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<SandboxConfig> {
        let mut c = SandboxConfig::default();
        let key = get("ROLL_AGENT37_API_KEY");
        let backend = get("ROLL_SANDBOX_BACKEND").or_else(|| key.as_ref().map(|_| "agent37".into()));
        c.backend = match backend.as_deref() {
            None | Some("none") => None,
            Some("agent37") => {
                let api_key = key.ok_or_else(|| anyhow::anyhow!("ROLL_SANDBOX_BACKEND=agent37 needs ROLL_AGENT37_API_KEY"))?;
                let mut a = agent37::Agent37Config::new(api_key);
                if let Some(u) = get("ROLL_AGENT37_URL") {
                    a.url = u;
                }
                if let Some(t) = get("ROLL_AGENT37_TEMPLATE") {
                    a.template = t;
                }
                if let Some(r) = get("ROLL_AGENT37_REGION") {
                    a.region = r;
                }
                Some(BackendConfig::Agent37(a))
            }
            Some("docker") => {
                let mut d = docker::DockerConfig::default();
                if let Some(i) = get("ROLL_SANDBOX_IMAGE") {
                    d.image = i;
                }
                if let Some(r) = get("ROLL_SANDBOX_RUNTIME") {
                    d.runtime = docker::Runtime::parse(&r);
                }
                d.network = get("ROLL_SANDBOX_DOCKER_NETWORK");
                if let Some(n) = d.network.as_deref().filter(|n| !docker::network_allowed(n)) {
                    anyhow::bail!("ROLL_SANDBOX_DOCKER_NETWORK: sandboxes never join the {n} network");
                }
                d.host_gateway = get("ROLL_SANDBOX_DOCKER_HOST_GATEWAY").is_some_and(|v| v == "1" || v == "true");
                d.disk_limit = get("ROLL_SANDBOX_DOCKER_DISK_LIMIT").is_some_and(|v| v == "1" || v == "true");
                Some(BackendConfig::Docker(d))
            }
            Some(other) => anyhow::bail!("ROLL_SANDBOX_BACKEND is agent37, docker or none, not {other}"),
        };
        if let Some(s) = get("ROLL_SANDBOX_SIZES") {
            c.sizes = parse_sizes(&s).map_err(|e| anyhow::anyhow!("ROLL_SANDBOX_SIZES: {e}"))?;
        }
        if let Some(n) = get("ROLL_SANDBOX_MAX_PER_OWNER") {
            c.max_per_owner = n.parse().map_err(|_| anyhow::anyhow!("ROLL_SANDBOX_MAX_PER_OWNER is a number"))?;
        }
        c.api_url = get("ROLL_SANDBOX_API_URL");
        if let Some(d) = get("ROLL_SANDBOX_IDLE_STOP") {
            c.idle_stop = parse_duration(&d).ok_or_else(|| anyhow::anyhow!("ROLL_SANDBOX_IDLE_STOP is a duration (300, 300s, 5m), not {d:?}"))?;
        }
        if let Some(d) = get("ROLL_SANDBOX_WAKE_TIMEOUT") {
            c.wake_timeout = parse_duration(&d)
                .filter(|d| !d.is_zero())
                .ok_or_else(|| anyhow::anyhow!("ROLL_SANDBOX_WAKE_TIMEOUT is a duration over zero (180, 3m), not {d:?}"))?;
        }
        Ok(c)
    }
}

/// What a backend needs to create a sandbox. `env` holds secrets (the machine token, the owner's
/// provider keys): `Debug` shows only its names.
#[derive(Clone)]
pub struct SandboxSpec {
    /// roll's id (`sbx_…`): backends name and label what they make with it, so a create whose
    /// answer was lost is found again.
    pub id: String,
    pub name: String,
    pub size: Size,
    pub env: BTreeMap<String, String>,
}

impl std::fmt::Debug for SandboxSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxSpec").field("id", &self.id).field("name", &self.name).field("size", &self.size).field("env", &self.env.keys().collect::<Vec<_>>()).finish()
    }
}

/// What a backend says a sandbox is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendState {
    Creating,
    Running,
    Stopped,
    /// On its way between states (starting, stopping, restarting): look again later.
    Busy,
    Failed(String),
    /// The backend has no such sandbox.
    Gone,
}

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// Worth trying again (network, capacity, a state in transition).
    #[error("{0}")]
    Transient(String),
    /// Won't work as asked (refused, misconfigured).
    #[error("{0}")]
    Permanent(String),
}

/// A sandbox a backend holds that roll made: its id there, and roll's id it's tagged with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tagged {
    pub backend_id: String,
    pub sandbox_id: String,
}

/// Where sandboxes run. Every call is idempotent: the reconciler repeats them.
#[async_trait]
pub trait SandboxBackend: Send + Sync {
    /// `agent37` or `docker` (`SandboxInfo.backend`).
    fn name(&self) -> &'static str;
    /// Create sandbox `spec.id` and start it: the backend's id for it. If it exists already (a
    /// create whose answer was lost), its id.
    async fn create(&self, spec: &SandboxSpec) -> Result<String, BackendError>;
    /// The backend's id of roll's sandbox `sandbox_id`, if it has one.
    async fn find(&self, sandbox_id: &str) -> Result<Option<String>, BackendError> {
        Ok(self.tagged().await?.into_iter().find(|t| t.sandbox_id == sandbox_id).map(|t| t.backend_id))
    }
    /// Everything the backend holds that roll made (by its tag), leaving out what's being deleted.
    async fn tagged(&self) -> Result<Vec<Tagged>, BackendError>;
    async fn status(&self, backend_id: &str) -> Result<BackendState, BackendError>;
    async fn start(&self, backend_id: &str) -> Result<(), BackendError>;
    async fn stop(&self, backend_id: &str) -> Result<(), BackendError>;
    /// Delete it and its data. One that's already gone counts as deleted.
    async fn delete(&self, backend_id: &str) -> Result<(), BackendError>;
}

/// Sandboxes' live state on this node: the backend and the owners' reconcilers.
pub struct Sandboxes {
    pub(crate) backend: Option<Arc<dyn SandboxBackend>>,
    reconcilers: Mutex<HashMap<String, Arc<Reconciler>>>,
    /// One look at a sandbox at a time (the reconciler's, the sweep's, or a request's).
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// The sweep runs on this node (it acts only while it holds the singleton).
    sweeping: AtomicBool,
    /// The link each machine had when the reconciler stopped its sandbox, by machine: a wake
    /// waits for a newer one (that link belongs to a daemon on its way down).
    stopped_links: Mutex<HashMap<String, Option<u64>>>,
}

struct Reconciler {
    wake: Notify,
    /// Kicked since the reconciler last looked: it looks again before it stops.
    kicked: AtomicBool,
    /// When it started on this node (ms). Nothing sleeps sooner than one idle window after: what
    /// a previous process (or node) saw since the last durable `last_active_at` is unknown.
    since: i64,
}

impl Sandboxes {
    pub fn new(config: &SandboxConfig) -> anyhow::Result<Sandboxes> {
        let backend: Option<Arc<dyn SandboxBackend>> = match &config.backend {
            None => None,
            Some(BackendConfig::Agent37(c)) => Some(Arc::new(agent37::Agent37::new(c.clone())?)),
            Some(BackendConfig::Docker(c)) => Some(Arc::new(docker::Docker::new(c.clone()))),
        };
        Ok(Sandboxes { backend, reconcilers: Mutex::default(), locks: Mutex::default(), sweeping: AtomicBool::new(false), stopped_links: Mutex::default() })
    }

    /// `agent37` or `docker`; `None` when sandboxes are off.
    pub fn backend_name(&self) -> Option<&'static str> {
        self.backend.as_ref().map(|b| b.name())
    }

    fn lock(&self, sandbox_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock();
        if locks.len() > 1024 {
            locks.retain(|_, l| Arc::strong_count(l) > 1);
        }
        locks.entry(sandbox_id.to_string()).or_default().clone()
    }
}

/// `[from, to)` (ms) split at UTC month boundaries: `(YYYY-MM, ms)` for each month it touches.
pub fn split_by_month(from: i64, to: i64) -> Vec<(String, i64)> {
    use chrono::{Datelike, TimeZone, Utc};
    let mut out = Vec::new();
    let mut at = from;
    while at < to {
        let Some(d) = Utc.timestamp_millis_opt(at).single() else { break };
        let (y, m) = if d.month() == 12 { (d.year() + 1, 1) } else { (d.year(), d.month() + 1) };
        let next = Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0).single().map_or(to, |n| n.timestamp_millis());
        let end = next.min(to);
        out.push((d.format("%Y-%m").to_string(), end - at));
        at = end;
    }
    out
}

/// What `running_ms` of a size priced at `hourly_cents` costs, in US dollars: hours × cents / 100.
pub fn price_usd(running_ms: i64, hourly_cents: u32) -> f64 {
    // ms × cents is exact in i128; one division keeps whole hours at whole cents.
    (running_ms.max(0) as i128 * hourly_cents as i128) as f64 / 360_000_000.0
}

/// A dollar amount as the API shows it: to a hundredth of a cent, so short runs at a few cents an
/// hour don't round to nothing.
pub fn round_usd(usd: f64) -> f64 {
    (usd * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months_split_at_their_boundaries() {
        use chrono::{TimeZone, Utc};
        let t = |y, m, d, h| Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap().timestamp_millis();
        assert_eq!(split_by_month(t(2026, 1, 31, 23), t(2026, 2, 1, 2)), vec![("2026-01".into(), 3_600_000), ("2026-02".into(), 7_200_000)]);
        assert_eq!(split_by_month(t(2026, 12, 31, 0), t(2027, 1, 1, 1)), vec![("2026-12".into(), 86_400_000), ("2027-01".into(), 3_600_000)]);
        assert_eq!(split_by_month(t(2026, 5, 1, 0), t(2026, 5, 1, 0)), vec![]);
        // Running time is priced by the hour.
        let hour = 3_600_000;
        assert_eq!(round_usd(price_usd(hour, 3)), 0.03);
        assert_eq!(round_usd(price_usd(hour * 3 / 2, 3)), 0.045);
        assert_eq!(round_usd(price_usd(730 * hour, 10)), 73.0);
        assert_eq!(price_usd(-1, 3), 0.0);
        assert_eq!(default_sizes()[0], Size::new("standard", 2, 4, 8, 3));
    }

    #[test]
    fn sizes_and_env_config() {
        let s = parse_sizes("small=2:4:8:3, big=4:16:32:10").unwrap();
        assert_eq!(s[1], Size::new("big", 4, 16, 32, 10));
        for bad in ["", "x=1:2:3", "x=0:2:3:4", "=1:2:3:4", "a=1:1:1:1,a=1:1:1:1", "a b=1:1:1:1"] {
            assert!(parse_sizes(bad).is_err(), "{bad}");
        }
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        assert!(SandboxConfig::from_lookup(env(&[])).unwrap().backend.is_none());
        let c = SandboxConfig::from_lookup(env(&[("ROLL_AGENT37_API_KEY", "sk_live_x"), ("ROLL_SANDBOX_MAX_PER_OWNER", "2")])).unwrap();
        assert!(matches!(c.backend, Some(BackendConfig::Agent37(_))));
        assert_eq!(c.max_per_owner, 2);
        // The key never shows in Debug output.
        assert!(!format!("{c:?}").contains("sk_live_x"));
        let c = SandboxConfig::from_lookup(env(&[("ROLL_SANDBOX_BACKEND", "docker"), ("ROLL_SANDBOX_RUNTIME", "runsc")])).unwrap();
        assert!(matches!(c.backend, Some(BackendConfig::Docker(ref d)) if d.runtime == docker::Runtime::Named("runsc".into())));
        assert!(SandboxConfig::from_lookup(env(&[("ROLL_SANDBOX_BACKEND", "agent37")])).is_err());
        assert!(SandboxConfig::from_lookup(env(&[("ROLL_SANDBOX_BACKEND", "k8s")])).is_err());
        // Sleeping: five minutes idle by default, `0` keeps sandboxes up.
        let c = SandboxConfig::from_lookup(env(&[])).unwrap();
        assert_eq!((c.idle_stop, c.wake_timeout), (Duration::from_secs(300), Duration::from_secs(180)));
        let c = SandboxConfig::from_lookup(env(&[("ROLL_SANDBOX_IDLE_STOP", "0"), ("ROLL_SANDBOX_WAKE_TIMEOUT", "4m")])).unwrap();
        assert_eq!((c.idle_stop, c.wake_timeout), (Duration::ZERO, Duration::from_secs(240)));
        assert_eq!(SandboxConfig::from_lookup(env(&[("ROLL_SANDBOX_IDLE_STOP", "90s")])).unwrap().idle_stop, Duration::from_secs(90));
        assert_eq!(SandboxConfig::from_lookup(env(&[("ROLL_SANDBOX_IDLE_STOP", "1h")])).unwrap().idle_stop, Duration::from_secs(3600));
        for bad in [("ROLL_SANDBOX_IDLE_STOP", "soon"), ("ROLL_SANDBOX_IDLE_STOP", "5d"), ("ROLL_SANDBOX_WAKE_TIMEOUT", "0")] {
            assert!(SandboxConfig::from_lookup(move |k: &str| (k == bad.0).then(|| bad.1.to_string())).is_err(), "{bad:?}");
        }
        // A spec's env values never show in Debug output either.
        let spec = SandboxSpec { id: "sbx_1".into(), name: "n".into(), size: default_sizes()[0].clone(), env: [("ROLL_AGENT_TOKEN".to_string(), "rlm_secret".to_string())].into() };
        let d = format!("{spec:?}");
        assert!(d.contains("ROLL_AGENT_TOKEN") && !d.contains("rlm_secret"), "{d}");
    }
}
