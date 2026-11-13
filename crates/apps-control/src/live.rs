//! One environment's instances on its service's owner (docs/apps/README.md, "The runtime"):
//! the waking proxy's side ([`EnvLive::acquire`]), the reaper that suspends, stops and scales
//! ([`EnvLive::tick`]), and the two halves of a rollout ([`EnvLive::prepare`] and
//! [`EnvLive::commit`]), or of a gradual one ([`EnvLive::begin_canary`], then
//! [`EnvLive::promote_canary`] or [`EnvLive::abort_canary`]).
//!
//! The state lives only in the owner's memory: a new owner starts from the store (the active
//! deployment) and makes instances again.

use crate::logs::{LogSink, Stream};
use crate::metrics::Meter;
use crate::model::{InstanceState, Restart, Run, RunType};
use crate::runtime::{InstanceSpec, Runtime};
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::sync::Arc;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How far back crashes count toward a crash loop.
const CRASH_WINDOW: Duration = Duration::from_secs(600);

/// What instances of the active deployment are made from.
#[derive(Clone, Debug)]
pub struct Template {
    pub deployment: String,
    pub spec: InstanceSpec,
    pub run: Run,
    /// A single-page app (its plan says so): the proxy answers a page path the app has no file
    /// for with `/index.html`.
    pub spa: bool,
    /// A static site whose files were exported (`sites.rs`): the proxy serves them by this key
    /// (the image's digest) and no instance ever runs.
    pub site: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Made, never started (or stopped): a start away.
    Created,
    Starting,
    Running(Option<SocketAddr>),
    Suspended(Option<SocketAddr>),
    /// The reaper is stopping or suspending it: no request is sent to it, nothing starts it.
    Stopping,
    Crashed,
}

pub struct Inst {
    pub name: String,
    pub deployment: String,
    phase: Mutex<Phase>,
    in_flight: AtomicU32,
    last_used: Mutex<Instant>,
    started_at: Mutex<Option<i64>>,
    cpu_seen: Mutex<Option<u64>>,
    /// Consecutive crashes (a worker's restart backoff).
    crashes: AtomicU32,
    /// A worker its restart policy leaves stopped: it isn't started again or replaced.
    done: AtomicBool,
    /// When its private connections were last counted (a dear read, at most every 5 s).
    private_checked: Mutex<Option<Instant>>,
    /// Failed health checks in a row while running.
    health_fails: AtomicU32,
    /// The stop a crash started: a start of the same instance waits for it, so it never stops
    /// the restarted process.
    stopping: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Inst {
    fn new(name: String, deployment: String) -> Arc<Inst> {
        Arc::new(Inst {
            name,
            deployment,
            phase: Mutex::new(Phase::Created),
            in_flight: AtomicU32::new(0),
            last_used: Mutex::new(Instant::now()),
            started_at: Mutex::new(None),
            cpu_seen: Mutex::new(None),
            crashes: AtomicU32::new(0),
            done: AtomicBool::new(false),
            private_checked: Mutex::new(None),
            health_fails: AtomicU32::new(0),
            stopping: Mutex::new(None),
        })
    }

    fn phase(&self) -> Phase {
        *self.phase.lock()
    }

    fn awake(&self) -> bool {
        matches!(self.phase(), Phase::Running(_) | Phase::Suspended(_) | Phase::Starting)
    }

    pub fn state(&self) -> InstanceState {
        match self.phase() {
            Phase::Created => InstanceState::Stopped,
            Phase::Starting => InstanceState::Waking,
            Phase::Running(_) => InstanceState::Running,
            Phase::Suspended(_) => InstanceState::Suspended,
            // Up until the stop or suspend finishes (no request goes to it meanwhile).
            Phase::Stopping => InstanceState::Running,
            Phase::Crashed => InstanceState::Crashed,
        }
    }

    pub fn in_flight(&self) -> u32 {
        self.in_flight.load(Ordering::SeqCst)
    }

    pub fn started_at(&self) -> Option<i64> {
        *self.started_at.lock()
    }
}

/// A request's hold on an instance: in flight until dropped.
pub struct Lease {
    pub addr: SocketAddr,
    pub inst: Arc<Inst>,
    /// How long the request waited for a wake (0 when an instance was up).
    pub waited: Duration,
    pub cold: bool,
    /// Its deployment speaks HTTP/2 without TLS ([`crate::model::Run::h2c`]).
    pub h2c: bool,
    /// How long the app has to answer (a web service's `timeout_s`); None: as long as it takes.
    pub timeout: Option<Duration>,
    /// Its deployment keeps visitors on one instance ([`crate::model::Run::affinity`]).
    pub affinity: bool,
}

/// What a visitor's affinity cookie holds for an instance: a short digest of its name (the name
/// itself stays roll's).
pub fn affinity_key(instance: &str) -> String {
    crate::public::token_hash(instance)[..12].to_string()
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.inst.in_flight.fetch_sub(1, Ordering::SeqCst);
        *self.inst.last_used.lock() = Instant::now();
    }
}

/// A rollout's new instances, started and healthy, waiting for [`EnvLive::commit`].
pub struct Prepared {
    pub made: Vec<Arc<Inst>>,
    pub took: Duration,
    /// The environment's lock, held from a volume service's stop of its old instance to the
    /// commit.
    hold: Option<tokio::sync::OwnedMutexGuard<()>>,
}

#[derive(Debug, thiserror::Error)]
pub enum WakeError {
    #[error("this environment has no deployment yet")]
    NoDeployment,
    #[error("this service isn't a web service")]
    NotWeb,
    #[error("the service didn't start: {0}")]
    Failed(String),
    #[error("the service took longer than {0:?} to start")]
    Timeout(Duration),
    #[error("too many requests are waiting for this service to start")]
    Busy,
    #[error("this project reached its spending limit for the month")]
    Capped,
}

/// [`EnvLive::hold_asleep`]'s hold: let go when dropped, whatever happened.
pub struct Held {
    live: Arc<EnvLive>,
    _lock: tokio::sync::OwnedMutexGuard<()>,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.live.held.store(false, Ordering::SeqCst);
    }
}

/// One deployment's answers during a canary step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Served {
    pub requests: u64,
    pub errors: u64,
    /// Its instances that crashed or failed their health check (no request sees those).
    pub crashes: u64,
}

impl Served {
    pub fn error_pct(&self) -> f64 {
        if self.requests == 0 { 0.0 } else { self.errors as f64 * 100.0 / self.requests as f64 }
    }
}

/// What an environment's public names let through, kept with its live state on the owner so a
/// request reads no row for it.
#[derive(Debug, Default)]
pub struct Gate {
    /// In maintenance, with this message (empty: roll's words).
    pub maintenance: Option<String>,
    /// Only these networks get in (empty: anyone).
    pub allow: Vec<ipnet::IpNet>,
    /// Each client's requests a minute.
    pub rate: Option<crate::model::RateLimit>,
    /// Its firewall rules, in order.
    pub firewall: Vec<crate::model::FirewallRule>,
}

/// Clients a rate limit remembers at most (a full bucket is the same as none, so those go first).
const MAX_BUCKETS: usize = 100_000;

impl Gate {
    /// Whether its firewall lets the request in: the first rule that matches decides; none
    /// matching, it does.
    pub fn firewall_allows(&self, method: &str, path: &str, user_agent: &str) -> bool {
        self.firewall.iter().find(|r| r.matches(method, path, user_agent)).is_none_or(|r| r.action == crate::model::FirewallAction::Allow)
    }

    /// Whether `client` (the address the fleet vouches for) gets in. None, with a list: no.
    pub fn admits(&self, client: Option<&str>) -> bool {
        if self.allow.is_empty() {
            return true;
        }
        let Some(ip) = client_ip(client) else { return false };
        self.allow.iter().any(|n| n.contains(&ip))
    }
}

/// A client's address as the fleet gives it, an IPv4 one over IPv6 (`::ffff:a.b.c.d`) as IPv4.
pub fn client_ip(client: Option<&str>) -> Option<std::net::IpAddr> {
    let ip = client?.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>().ok()?;
    Some(match ip {
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, std::net::IpAddr::V4),
        v4 => v4,
    })
}

#[derive(Clone, Debug)]
pub struct LiveConfig {
    pub wake_timeout: Duration,
    /// How long a worker must keep running after a start to count as up.
    pub worker_grace: Duration,
    /// A crashed worker's first restart wait, doubling with each crash in a row to 60 times it.
    pub restart_backoff: Duration,
    /// How long old instances may finish their requests after a switch.
    pub drain: Duration,
    /// How long more instances than needed may stay up before one is stopped.
    pub scale_down_after: Duration,
    /// Between two probes of a starting instance.
    pub probe_every: Duration,
    /// Requests that may wait for one environment's wake; more get a 503 at once.
    pub max_waiters: u32,
    /// Between two health checks of a running web instance (its `health_path`, when set).
    pub health_every: Duration,
    /// Failed health checks in a row that replace an instance.
    pub health_failures: u32,
    /// How long CPU is averaged over before `run.cpu_target` scales.
    pub cpu_window: Duration,
}

impl Default for LiveConfig {
    fn default() -> Self {
        LiveConfig {
            wake_timeout: Duration::from_secs(60),
            worker_grace: Duration::from_secs(2),
            restart_backoff: Duration::from_secs(1),
            drain: Duration::from_secs(30),
            scale_down_after: Duration::from_secs(60),
            probe_every: Duration::from_millis(10),
            max_waiters: 1000,
            health_every: Duration::from_secs(10),
            health_failures: 3,
            cpu_window: Duration::from_secs(60),
        }
    }
}

pub struct EnvLive {
    pub service: String,
    pub env: String,
    runtime: Arc<dyn Runtime>,
    cfg: LiveConfig,
    template: Mutex<Option<Arc<Template>>>,
    insts: Mutex<Vec<Arc<Inst>>>,
    /// One change of instances at a time (wakes, scale-ups, rollouts).
    wake: Arc<tokio::sync::Mutex<()>>,
    /// Torn down: nothing is made or adopted any more (a request or a tick that held this
    /// environment from before can't bring it back).
    closed: AtomicBool,
    /// Its project reached its spend limit: nothing starts, and the tick stops what runs.
    capped: AtomicBool,
    /// Its volume is being restored: nothing starts until it's done.
    held: AtomicBool,
    /// A scaling worker's instances for its backlog ([`crate::model::Scale`]); 0: its floor.
    wanted: AtomicU32,
    /// The backlog it last read (`u64::MAX`: not read yet).
    backlog: AtomicU64,
    /// What scaling last said in the log (said again only when it changes).
    scale_note: Mutex<String>,
    /// Since when it has wanted fewer than run (a scale-in waits `scale_down_after`).
    fewer_since: Mutex<Option<Instant>>,
    /// A gradual rollout's new deployment and the percent of requests it takes.
    canary: Mutex<Option<(Arc<Template>, u8)>>,
    /// Requests routed so far (the canary's share is taken by turns).
    routed: AtomicU64,
    /// Requests answered and 5xx answers per deployment while a canary runs.
    served: Mutex<HashMap<String, Served>>,
    /// Deployments asked for by their own URL (not the active one): their templates, one
    /// instance each at most, gone a minute after their last request.
    viewed: Mutex<HashMap<String, (Arc<Template>, Instant)>>,
    /// Requests waiting on a wake.
    waiters: AtomicU32,
    /// The last failed wake, of which deployment, and how many failed in a row: the requests
    /// behind it get its error instead of each trying again, for a backoff that grows from 1 s to
    /// 30 s. A canary's failures don't hold back the active deployment's wakes.
    failed: Mutex<Option<(String, Instant, String, u32)>>,
    /// Crashes of the last ten minutes: when, when in unix ms, why.
    /// Recent crashes: when, unix ms, why, and the deployment the instance ran.
    crashes: Mutex<std::collections::VecDeque<(Instant, i64, String, String)>>,
    seq: AtomicU64,
    pub log: LogSink,
    pub meter: Mutex<Meter>,
    over_since: Mutex<Option<Instant>>,
    /// A tick is running (the reaper never runs two of one environment at once).
    pub ticking: std::sync::atomic::AtomicBool,
    /// A private caller's wake is under way (the next look waits for it).
    answering: std::sync::atomic::AtomicBool,
    /// What its public names let through, as last read (None: read it again), and how many
    /// times it was forgotten (a read that began before a change isn't kept).
    gate: Mutex<Option<Arc<Gate>>>,
    gate_gen: AtomicU64,
    /// Each client's tokens and when they were counted (its rate limit; None: no address).
    buckets: Mutex<HashMap<Option<std::net::IpAddr>, (f64, Instant)>>,
    /// CPU its awake instances used and could have used, per tick, for `cpu_window` (only what
    /// the runtime measures).
    cpu_samples: Mutex<std::collections::VecDeque<(Instant, f64, f64)>>,
    /// The instances CPU last asked for: kept while the window fills again after a scale-out,
    /// so the new instance isn't taken straight back in.
    cpu_need: AtomicU32,
    last_tick: Mutex<Option<Instant>>,
    last_health: Mutex<Option<Instant>>,
    client: hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, axum::body::Body>,
    /// HTTP/2 without TLS, for an h2c app's health checks.
    client_h2: hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, axum::body::Body>,
}

fn now_ms() -> i64 {
    code_sql::now_ms()
}

impl EnvLive {
    pub fn new(service: &str, env: &str, runtime: Arc<dyn Runtime>, cfg: LiveConfig, log: LogSink) -> Arc<EnvLive> {
        Arc::new(EnvLive {
            service: service.to_string(),
            env: env.to_string(),
            runtime,
            cfg,
            template: Mutex::new(None),
            insts: Mutex::new(Vec::new()),
            wake: Arc::new(tokio::sync::Mutex::new(())),
            closed: AtomicBool::new(false),
            capped: AtomicBool::new(false),
            held: AtomicBool::new(false),
            wanted: AtomicU32::new(0),
            backlog: AtomicU64::new(u64::MAX),
            scale_note: Mutex::new(String::new()),
            fewer_since: Mutex::new(None),
            canary: Mutex::new(None),
            routed: AtomicU64::new(0),
            served: Mutex::new(HashMap::new()),
            viewed: Mutex::new(HashMap::new()),
            waiters: AtomicU32::new(0),
            failed: Mutex::new(None),
            crashes: Mutex::new(std::collections::VecDeque::new()),
            seq: AtomicU64::new(0),
            log,
            meter: Mutex::new(Meter::default()),
            over_since: Mutex::new(None),
            ticking: std::sync::atomic::AtomicBool::new(false),
            answering: std::sync::atomic::AtomicBool::new(false),
            gate: Mutex::new(None),
            gate_gen: AtomicU64::new(0),
            buckets: Mutex::new(HashMap::new()),
            cpu_samples: Mutex::new(std::collections::VecDeque::new()),
            cpu_need: AtomicU32::new(0),
            last_tick: Mutex::new(None),
            last_health: Mutex::new(None),
            client: hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build_http(),
            client_h2: hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).http2_only(true).build_http(),
        })
    }

    pub fn template(&self) -> Option<Arc<Template>> {
        self.template.lock().clone()
    }

    pub fn instances(&self) -> Vec<Arc<Inst>> {
        self.insts.lock().clone()
    }

    /// The deployment a request goes to: a visitor's bucket (0–99, its `roll_bucket` cookie)
    /// below the canary's share goes to the canary, so a visitor stays on one deployment through
    /// a step; without one, the canary's share of requests by turns (37 is coprime with 100, so
    /// each hundred requests sends exactly that many, spread out).
    pub fn route(&self, bucket: Option<u8>) -> Option<Arc<Template>> {
        if let Some((c, share)) = self.canary.lock().clone() {
            let b = bucket.map_or_else(|| (self.routed.fetch_add(1, Ordering::Relaxed) % 100 * 37) % 100, u64::from);
            if b < share as u64 {
                return Some(c);
            }
        }
        self.template()
    }

    /// `deployment`'s template while it still serves (the active one, the canary, or one asked
    /// for by its own URL).
    fn serving(&self, deployment: &str) -> Option<Arc<Template>> {
        if let Some((c, _)) = self.canary.lock().as_ref()
            && c.deployment == deployment
        {
            return Some(c.clone());
        }
        if let Some(t) = self.template().filter(|t| t.deployment == deployment) {
            return Some(t);
        }
        self.viewed.lock().get(deployment).map(|(t, _)| t.clone())
    }

    /// The canary's deployment and share, while a gradual rollout runs.
    pub fn canary(&self) -> Option<(String, u8)> {
        self.canary.lock().as_ref().map(|(t, s)| (t.deployment.clone(), *s))
    }

    /// The proxy's answer to a request `inst` served: counted per deployment while a canary
    /// runs (the rollout judges the new deployment by them).
    pub fn answered(&self, inst: &Inst, status: u16) {
        self.count(&inst.deployment, |s| {
            s.requests += 1;
            if status >= 500 {
                s.errors += 1;
            }
        });
    }

    /// `deployment`'s counts while a canary runs.
    fn count(&self, deployment: &str, f: impl FnOnce(&mut Served)) {
        if self.canary.lock().is_none() {
            return;
        }
        f(self.served.lock().entry(deployment.to_string()).or_default());
    }

    /// Answers since the last call, per deployment (and counting again from zero).
    pub fn take_served(&self) -> HashMap<String, Served> {
        std::mem::take(&mut *self.served.lock())
    }

    /// How many instances are up or starting.
    pub fn awake(&self) -> usize {
        self.awake_count()
    }

    pub fn state(&self) -> InstanceState {
        let insts = self.insts.lock();
        let states: Vec<InstanceState> = insts.iter().map(|i| i.state()).collect();
        match self.template.lock().as_ref() {
            None => return InstanceState::None,
            // Its files answer: always up, nothing to wake.
            Some(t) if t.site.is_some() => return InstanceState::Running,
            _ => {}
        }
        for s in [InstanceState::Running, InstanceState::Waking, InstanceState::Suspended, InstanceState::Crashed] {
            if states.contains(&s) {
                return s;
            }
        }
        InstanceState::Stopped
    }

    fn name_for(&self, tpl: &Template) -> String {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let short = tpl.deployment.strip_prefix("dep_").unwrap_or(&tpl.deployment);
        let svc = self.service.strip_prefix("svc_").unwrap_or(&self.service);
        format!("roll-apps-{svc}-{}-{}-{n}", self.env, &short[..short.len().min(6)])
    }

    async fn make(&self, tpl: &Template) -> anyhow::Result<Arc<Inst>> {
        anyhow::ensure!(!self.closed.load(Ordering::SeqCst), "this environment was torn down");
        let mut spec = tpl.spec.clone();
        spec.name = self.name_for(tpl);
        self.runtime.create(&spec).await?;
        Ok(Inst::new(spec.name, tpl.deployment.clone()))
    }

    /// Whether something answers HTTP at `addr` (`path`: a 2xx or 3xx there; none: any answer,
    /// since a published port accepts connections before the app listens).
    async fn answers(&self, addr: SocketAddr, path: Option<&str>, h2c: bool) -> bool {
        let uri = format!("http://{addr}{}", path.unwrap_or("/"));
        let version = if h2c { axum::http::Version::HTTP_2 } else { axum::http::Version::HTTP_11 };
        let Ok(req) = axum::http::Request::get(uri).version(version).header("user-agent", "roll-apps-health").body(axum::body::Body::empty()) else { return false };
        let client = if h2c { &self.client_h2 } else { &self.client };
        match tokio::time::timeout(Duration::from_secs(5), client.request(req)).await {
            Ok(Ok(resp)) => path.is_none() || resp.status().is_success() || resp.status().is_redirection(),
            _ => false,
        }
    }

    /// Start `inst` and wait until it answers (web) or has kept running (worker).
    async fn start(&self, inst: &Arc<Inst>, run: &Run, health: Option<&str>, deadline: Instant) -> Result<Option<SocketAddr>, WakeError> {
        let crash_stop = inst.stopping.lock().take();
        if let Some(stop) = crash_stop {
            let _ = stop.await;
        }
        *inst.phase.lock() = Phase::Starting;
        let log = self.log.instance(&inst.name, &inst.deployment);
        let addr = match self.runtime.start(&inst.name, log).await {
            Ok(a) => a,
            Err(e) => {
                self.crashed(inst, &format!("it didn't start: {e}"));
                return Err(WakeError::Failed(e.to_string()));
            }
        };
        *inst.started_at.lock() = Some(now_ms());
        inst.health_fails.store(0, Ordering::SeqCst);
        *inst.cpu_seen.lock() = None;
        match (run.kind, addr) {
            (RunType::Web, Some(a)) => loop {
                // A TCP service is up when its port accepts; anything else answers HTTP.
                let up = if run.tcp {
                    tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(a)).await.is_ok_and(|c| c.is_ok())
                } else {
                    self.answers(a, health, run.h2c).await
                };
                if up {
                    break;
                }
                if Instant::now() >= deadline {
                    self.crashed(inst, "it didn't answer in time");
                    return Err(WakeError::Timeout(self.cfg.wake_timeout));
                }
                if !self.runtime.running(&inst.name).await {
                    let why = format!("the process exited before it answered: {}", self.exit_reason(&inst.name, run).await);
                    self.crashed(inst, &why);
                    return Err(WakeError::Failed(why));
                }
                tokio::time::sleep(self.cfg.probe_every).await;
            },
            _ => {
                tokio::time::sleep(self.cfg.worker_grace).await;
                if !self.runtime.running(&inst.name).await {
                    let why = format!("the process exited right after it started: {}", self.exit_reason(&inst.name, run).await);
                    self.crashed(inst, &why);
                    return Err(WakeError::Failed(why));
                }
            }
        }
        *inst.phase.lock() = Phase::Running(addr);
        *inst.last_used.lock() = Instant::now();
        // Up: from here it runs within its CPU limit (it started with a boost).
        let (rt, name) = (self.runtime.clone(), inst.name.clone());
        tokio::spawn(async move { rt.settle(&name).await });
        Ok(addr)
    }

    /// A running instance of `deployment` with room under `concurrency` (the least busy), leased.
    fn pick(&self, deployment: &str, concurrency: u32, over: bool) -> Option<Lease> {
        let insts = self.insts.lock();
        let best = insts
            .iter()
            .filter(|i| i.deployment == deployment)
            .filter_map(|i| match i.phase() {
                Phase::Running(Some(a)) => Some((i, a)),
                _ => None,
            })
            .filter(|(i, _)| over || i.in_flight() < concurrency)
            .min_by_key(|(i, _)| i.in_flight())?;
        best.0.in_flight.fetch_add(1, Ordering::SeqCst);
        Some(Lease { addr: best.1, inst: best.0.clone(), waited: Duration::ZERO, cold: false, h2c: false, timeout: None, affinity: false })
    }

    /// The instance a visitor's affinity cookie names, if it's up with room.
    fn pick_affine(&self, deployment: &str, concurrency: u32, key: &str) -> Option<Lease> {
        let insts = self.insts.lock();
        let (i, a) = insts.iter().filter(|i| i.deployment == deployment && i.in_flight() < concurrency && affinity_key(&i.name) == key).find_map(|i| match i.phase() {
            Phase::Running(Some(a)) => Some((i, a)),
            _ => None,
        })?;
        i.in_flight.fetch_add(1, Ordering::SeqCst);
        Some(Lease { addr: a, inst: i.clone(), waited: Duration::ZERO, cold: false, h2c: false, timeout: None, affinity: false })
    }

    /// Take an idle instance out of rotation before stopping or suspending it: under the lock
    /// [`Self::pick`] takes, so no request is handed to it in between. Its phase before, if it
    /// was up and had nothing in flight.
    fn claim_idle(&self, i: &Inst) -> Option<Phase> {
        let _picking = self.insts.lock();
        let mut phase = i.phase.lock();
        if i.in_flight() > 0 || !matches!(*phase, Phase::Running(_) | Phase::Suspended(_)) {
            return None;
        }
        Some(std::mem::replace(&mut *phase, Phase::Stopping))
    }

    fn running_count(&self, deployment: &str) -> usize {
        self.insts.lock().iter().filter(|i| i.deployment == deployment && matches!(i.phase(), Phase::Running(_))).count()
    }

    fn awake_count(&self) -> usize {
        self.insts.lock().iter().filter(|i| i.awake()).count()
    }

    fn awake_of(&self, deployment: &str) -> usize {
        self.insts.lock().iter().filter(|i| i.deployment == deployment && i.awake()).count()
    }

    /// An instance to send a request to: one that's up, else a suspended one resumed, else one
    /// started (the cold start), within `wake_timeout`. Requests that come while one wakes wait
    /// for the same wake.
    pub async fn acquire(self: &Arc<Self>) -> Result<Lease, WakeError> {
        self.acquire_for(None).await
    }

    /// [`Self::acquire`] for a visitor in rollout `bucket` (0–99).
    pub async fn acquire_for(self: &Arc<Self>, bucket: Option<u8>) -> Result<Lease, WakeError> {
        self.may_wake()?;
        let tpl = self.route(bucket).ok_or(WakeError::NoDeployment)?;
        self.acquire_tpl(tpl).await
    }

    /// A request to one deployment by its own URL (Vercel's deployment URLs, Cloud Run's
    /// revision tags): the active one's instances when it's that one, else an instance of it
    /// (one at most), which goes a minute after its last request.
    pub async fn acquire_deployment(self: &Arc<Self>, tpl: Template) -> Result<Lease, WakeError> {
        self.may_wake()?;
        let tpl = match self.serving(&tpl.deployment).filter(|_| !self.viewed.lock().contains_key(&tpl.deployment)) {
            // The active one or the canary: theirs.
            Some(t) => t,
            None => {
                let mut t = tpl;
                t.run.min_instances = 0;
                t.run.max_instances = 1;
                let mut viewed = self.viewed.lock();
                let e = viewed.entry(t.deployment.clone()).or_insert_with(|| (Arc::new(t), Instant::now()));
                e.1 = Instant::now();
                e.0.clone()
            }
        };
        self.acquire_pinned(tpl, true).await
    }

    /// Whether `deployment` serves only through its own URL (not the active one or the canary).
    fn only_viewed(&self, deployment: &str) -> bool {
        self.viewed.lock().contains_key(deployment) && self.template().is_none_or(|t| t.deployment != deployment) && self.canary.lock().as_ref().is_none_or(|(c, _)| c.deployment != deployment)
    }

    pub fn may_wake(&self) -> Result<(), WakeError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(WakeError::NoDeployment);
        }
        if self.capped.load(Ordering::SeqCst) {
            return Err(WakeError::Capped);
        }
        if self.held.load(Ordering::SeqCst) {
            return Err(WakeError::Busy);
        }
        Ok(())
    }

    pub async fn acquire_tpl(self: &Arc<Self>, tpl: Arc<Template>) -> Result<Lease, WakeError> {
        self.acquire_pinned(tpl, false).await
    }

    /// [`Self::acquire_tpl`], to the instance `prefer` (an affinity cookie) names when it can.
    pub async fn acquire_preferring(self: &Arc<Self>, tpl: Arc<Template>, prefer: Option<&str>) -> Result<Lease, WakeError> {
        if let Some(k) = prefer.filter(|_| tpl.run.affinity)
            && let Some(mut l) = self.pick_affine(&tpl.deployment, tpl.run.concurrency, k)
        {
            (l.h2c, l.timeout, l.affinity) = (tpl.run.h2c, tpl.run.timeout_s.map(|s| Duration::from_secs(s as u64)), true);
            return Ok(l);
        }
        self.acquire_pinned(tpl, false).await
    }

    /// `pinned`: the request asked for this deployment (its own URL), and gets it or nothing.
    async fn acquire_pinned(self: &Arc<Self>, tpl: Arc<Template>, pinned: bool) -> Result<Lease, WakeError> {
        let (h2c, timeout, affinity) = (tpl.run.h2c, tpl.run.timeout_s.map(|s| Duration::from_secs(s as u64)), tpl.run.affinity);
        self.acquire_inner(tpl, pinned).await.map(|mut l| {
            (l.h2c, l.timeout, l.affinity) = (h2c, timeout, affinity);
            l
        })
    }

    async fn acquire_inner(self: &Arc<Self>, tpl: Arc<Template>, pinned: bool) -> Result<Lease, WakeError> {
        let began = Instant::now();
        if tpl.run.kind != RunType::Web {
            return Err(WakeError::NotWeb);
        }
        let (run, dep) = (&tpl.run, tpl.deployment.as_str());
        if let Some(l) = self.pick(dep, run.concurrency, false) {
            return Ok(l);
        }
        // Everything is busy but something's running: scale out behind it, and go to the least
        // busy.
        if self.running_count(dep) > 0 && (self.awake_of(dep) as u32) < run.max_instances {
            let (this, t) = (self.clone(), tpl.clone());
            tokio::spawn(async move { this.scale_up(t).await });
            if let Some(l) = self.pick(dep, run.concurrency, true) {
                return Ok(l);
            }
        }
        if self.waiters.fetch_add(1, Ordering::SeqCst) >= self.cfg.max_waiters {
            self.waiters.fetch_sub(1, Ordering::SeqCst);
            return Err(WakeError::Busy);
        }
        // The wake runs on its own task: a client that goes away mid-wake can't leave an
        // instance half started (or a container half made); the wake finishes, and its lease,
        // if nobody takes it, is dropped there.
        let this = self.clone();
        let routed = tpl.deployment.clone();
        let r = tokio::spawn(async move {
            let r = this.wake_slow(began, tpl, pinned).await;
            this.waiters.fetch_sub(1, Ordering::SeqCst);
            r
        })
        .await
        .unwrap_or_else(|e| Err(WakeError::Failed(e.to_string())));
        // A visitor sent to a canary that won't start got a 5xx the canary must answer for.
        if let Err(WakeError::Failed(_) | WakeError::Timeout(_)) = &r {
            self.count(&routed, |s| {
                s.requests += 1;
                s.errors += 1;
            });
        }
        r
    }

    async fn wake_slow(self: &Arc<Self>, began: Instant, routed: Arc<Template>, pinned: bool) -> Result<Lease, WakeError> {
        let deadline = began + self.cfg.wake_timeout;
        let _g = tokio::time::timeout_at(deadline.into(), self.wake.lock()).await.map_err(|_| WakeError::Timeout(self.cfg.wake_timeout))?;
        if self.closed.load(Ordering::SeqCst) {
            return Err(WakeError::NoDeployment);
        }
        // Read again under the lock: the deployment it was routed to may have been switched,
        // promoted or rolled back meanwhile.
        let tpl = match self.serving(&routed.deployment) {
            Some(t) => t,
            // A deployment's own URL never quietly answers with another deployment.
            None if pinned => return Err(WakeError::NoDeployment),
            None => self.template().ok_or(WakeError::NoDeployment)?,
        };
        let dep = tpl.deployment.as_str();
        if let Some(mut l) = self.pick(dep, tpl.run.concurrency, self.awake_of(dep) as u32 >= tpl.run.max_instances) {
            l.waited = began.elapsed();
            return Ok(l);
        }
        if let Some((d, at, why, n)) = self.failed.lock().clone()
            && d == dep
            && at.elapsed() < Duration::from_secs(1 << (n.saturating_sub(1)).min(5)).min(Duration::from_secs(30))
        {
            return Err(WakeError::Failed(why));
        }
        // A frozen one first: milliseconds.
        let suspended = self.insts.lock().iter().find(|i| i.deployment == dep && matches!(i.phase(), Phase::Suspended(_))).cloned();
        if let Some(i) = suspended {
            let Phase::Suspended(addr) = i.phase() else { unreachable!() };
            if self.runtime.resume(&i.name).await.is_ok() {
                *i.phase.lock() = Phase::Running(addr);
                if let Some(mut l) = self.pick(dep, tpl.run.concurrency, true) {
                    l.waited = began.elapsed();
                    return Ok(l);
                }
            } else {
                self.crashed(&i, "it couldn't be resumed");
            }
        }
        // Then a start: a created instance of the active deployment, or a new one.
        let woke = Instant::now();
        let started = match self.ready_instance(&tpl).await {
            Ok(inst) => self.start(&inst, &tpl.run, None, deadline).await.map(|_| inst),
            Err(e) => Err(WakeError::Failed(e.to_string())),
        };
        let inst = match started {
            Ok(i) => {
                *self.failed.lock() = None;
                i
            }
            Err(e) => {
                let mut f = self.failed.lock();
                let n = f.as_ref().filter(|(d, ..)| d == dep).map_or(0, |(.., n)| *n) + 1;
                *f = Some((dep.to_string(), Instant::now(), e.to_string(), n));
                return Err(e);
            }
        };
        let ms = woke.elapsed().as_secs_f64() * 1000.0;
        self.meter.lock().cold_start(now_ms(), ms);
        self.log.stream(Stream::System).line(format!("cold start: {} answered in {ms:.0} ms", inst.name));
        let mut l = self.pick(dep, tpl.run.concurrency, true).ok_or_else(|| WakeError::Failed("the instance stopped again".into()))?;
        l.waited = began.elapsed();
        l.cold = true;
        Ok(l)
    }

    pub fn gate(&self) -> Option<Arc<Gate>> {
        self.gate.lock().clone()
    }

    pub fn gate_gen(&self) -> u64 {
        self.gate_gen.load(Ordering::SeqCst)
    }

    /// Keep what was read, unless its settings changed since the read began (`seen`).
    pub fn set_gate(&self, g: Arc<Gate>, seen: u64) {
        let mut cur = self.gate.lock();
        if self.gate_gen.load(Ordering::SeqCst) == seen {
            *cur = Some(g);
        }
    }

    /// Its settings changed: the next request reads them again.
    pub fn forget_gate(&self) {
        let mut cur = self.gate.lock();
        self.gate_gen.fetch_add(1, Ordering::SeqCst);
        *cur = None;
        self.buckets.lock().clear();
    }

    /// One request from `client` under `limit`: let through (a token taken), or how long until
    /// the next token.
    pub fn take_token(&self, client: Option<std::net::IpAddr>, limit: &crate::model::RateLimit) -> Result<(), Duration> {
        let (rate, burst) = (limit.per_minute as f64 / 60.0, limit.burst() as f64);
        // An IPv6 client counts by its /64: one host has the whole of it to rotate through.
        let client = client.map(|ip| match ip {
            std::net::IpAddr::V6(v6) => std::net::IpAddr::V6(std::net::Ipv6Addr::from(u128::from(v6) & !((1u128 << 64) - 1))),
            v4 => v4,
        });
        let now = Instant::now();
        let mut b = self.buckets.lock();
        if b.len() >= MAX_BUCKETS && !b.contains_key(&client) {
            // Full ones first: they're the same as no entry. Then the longest unseen, never
            // everyone's (a flood of new addresses mustn't reset the limited ones).
            b.retain(|_, (tokens, at)| *tokens + now.duration_since(*at).as_secs_f64() * rate < burst);
            while b.len() >= MAX_BUCKETS {
                let Some(oldest) = b.iter().min_by_key(|(_, (_, at))| *at).map(|(k, _)| *k) else { break };
                b.remove(&oldest);
            }
        }
        let (tokens, at) = b.entry(client).or_insert((burst, now));
        *tokens = (*tokens + now.duration_since(*at).as_secs_f64() * rate).min(burst);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - *tokens) / rate))
        }
    }

    /// Private callers ([`crate::runtime::Callers`]), which rolld never sees: a sleeping instance
    /// someone waits at wakes (a frozen one resumes; a stopped one starts, and its waker hands
    /// it the caller), and with `running` a running one with connections from the project's
    /// other services counts as in use (Railway's private network keeps a service awake too).
    pub async fn answer_private(self: &Arc<Self>, running: bool) {
        let Some(tpl) = self.template() else { return };
        if tpl.run.kind != RunType::Web || self.answering.load(Ordering::SeqCst) {
            return;
        }
        // A running instance's private connections are counted only near its sleep (5.6 ms a
        // read on a busy host, see `runtime::netns`): idle for all but its last two seconds.
        let run = &tpl.run;
        let rest = [run.suspend_after_s, run.sleep_after_s].into_iter().filter(|s| *s > 0).min().unwrap_or(run.sleep_after_s);
        let near_sleep = Duration::from_secs(rest as u64).saturating_sub(Duration::from_secs(2));
        let mut asked = Vec::new();
        for i in self.instances().into_iter().filter(|i| i.deployment == tpl.deployment) {
            match i.phase() {
                // Asleep (or stopped by a crash, its waker armed again): a connection its
                // network accepted since is a caller.
                Phase::Suspended(_) | Phase::Created | Phase::Crashed => {
                    if self.runtime.take_callers(&i.name).await {
                        asked.push(i);
                    }
                }
                Phase::Running(_) => {
                    let idle = i.last_used.lock().elapsed();
                    let due = i.private_checked.lock().is_none_or(|t| t.elapsed() >= Duration::from_secs(5));
                    if running && idle >= near_sleep && due {
                        *i.private_checked.lock() = Some(Instant::now());
                        if self.runtime.private_peers(&i.name).await.is_some_and(|n| n > 0) {
                            *i.last_used.lock() = Instant::now();
                        }
                    }
                }
                _ => {}
            }
        }
        if asked.is_empty() || self.answering.swap(true, Ordering::SeqCst) {
            return;
        }
        // Its own task: a slow start here holds up no other environment's callers.
        let this = self.clone();
        tokio::spawn(async move {
            this.wake_for_callers(&tpl, asked).await;
            this.answering.store(false, Ordering::SeqCst);
        });
    }

    async fn wake_for_callers(self: &Arc<Self>, tpl: &Template, asked: Vec<Arc<Inst>>) {
        let _g = self.wake.lock().await;
        // Torn down, over its spending limit or restoring its volume: nothing wakes.
        if self.may_wake().is_err() {
            return;
        }
        for i in asked {
            let woke = Instant::now();
            match i.phase() {
                Phase::Suspended(addr) => match self.runtime.resume(&i.name).await {
                    Ok(()) => {
                        *i.phase.lock() = Phase::Running(addr);
                        *i.last_used.lock() = Instant::now();
                    }
                    Err(_) => self.crashed(&i, "it couldn't be resumed"),
                },
                // The same container (a crashed one too): its waker holds the caller for it.
                Phase::Created | Phase::Crashed => {
                    let deadline = Instant::now() + self.cfg.wake_timeout;
                    match self.start(&i, &tpl.run, None, deadline).await {
                        Ok(_) => {
                            *i.last_used.lock() = Instant::now();
                            let ms = woke.elapsed().as_secs_f64() * 1000.0;
                            self.meter.lock().cold_start(now_ms(), ms);
                            self.log.stream(Stream::System).line(format!("cold start: {} answered a private caller in {ms:.0} ms", i.name));
                        }
                        Err(e) => self.log.stream(Stream::System).line(format!("a private caller couldn't wake {}: {e}", i.name)),
                    }
                }
                _ => {}
            }
        }
    }

    /// The proxy couldn't connect to a running instance: it's gone (killed, out of memory), and
    /// the next request starts a fresh one instead of trying it again.
    pub fn connect_failed(&self, inst: &Inst) {
        if matches!(inst.phase(), Phase::Running(_)) {
            self.crashed(inst, "its port stopped answering");
        }
    }

    /// Why a stopped instance stopped, as the runtime tells it (its exit code, out of memory),
    /// else that it exited on its own.
    async fn exit_reason(&self, name: &str, run: &Run) -> String {
        match self.runtime.exited(name).await {
            Some(e) => e.reason(run.memory_mb),
            None => "the process exited on its own".to_string(),
        }
    }

    /// An instance crashed: it's marked so (the next start makes a fresh one), stopped (a process
    /// that never answered its health check would otherwise keep running unmetered, at its
    /// start's boosted CPU), and the crash is counted. Three in ten minutes is a crash loop, said
    /// once in the environment's log.
    fn crashed(&self, inst: &Inst, why: &str) {
        self.crashed_with(inst, why, Duration::from_secs(2));
    }

    /// [`Self::crashed`], the stop giving requests in flight `grace` to finish.
    fn crashed_with(&self, inst: &Inst, why: &str, grace: Duration) {
        *inst.phase.lock() = Phase::Crashed;
        self.count(&inst.deployment, |s| s.crashes += 1);
        let (runtime, name) = (self.runtime.clone(), inst.name.clone());
        *inst.stopping.lock() = Some(tokio::spawn(async move {
            let _ = runtime.stop(&name, grace).await;
        }));
        let entered = {
            let mut c = self.crashes.lock();
            let was = Self::looping(&c);
            c.push_back((Instant::now(), now_ms(), why.to_string(), inst.deployment.clone()));
            while c.len() > 20 || c.front().is_some_and(|(at, ..)| at.elapsed() > CRASH_WINDOW) {
                c.pop_front();
            }
            !was && Self::looping(&c)
        };
        if entered {
            self.log.stream(Stream::System).line(format!("crash loop: {} crashes in 10 minutes, the last because {why}", self.crashes.lock().len()));
        }
    }

    fn looping(c: &std::collections::VecDeque<(Instant, i64, String, String)>) -> bool {
        c.iter().filter(|(at, ..)| at.elapsed() <= CRASH_WINDOW).count() >= 3
    }

    /// How many of `deployment`'s instances crashed at or after `since` (unix ms).
    pub fn crashes_of(&self, deployment: &str, since: i64) -> usize {
        self.crashes.lock().iter().filter(|(_, at, _, d)| d == deployment && *at >= since).count()
    }

    /// The environment's crashes of the last ten minutes when they're a loop (three or more).
    pub fn crash_loop(&self) -> Option<crate::model::CrashLoop> {
        let c = self.crashes.lock();
        if !Self::looping(&c) {
            return None;
        }
        let recent: Vec<_> = c.iter().filter(|(at, ..)| at.elapsed() <= CRASH_WINDOW).collect();
        let (_, at, why, _) = recent.last()?;
        Some(crate::model::CrashLoop { crashes: recent.len() as u32, last_at: *at, last_error: why.clone() })
    }

    /// A created (not running) instance of the active deployment, made if there's none.
    async fn ready_instance(&self, tpl: &Template) -> anyhow::Result<Arc<Inst>> {
        let found = self.insts.lock().iter().find(|i| i.deployment == tpl.deployment && matches!(i.phase(), Phase::Created | Phase::Crashed)).cloned();
        if let Some(i) = found {
            if i.phase() == Phase::Crashed {
                // Whatever happened to it, start from a clean container.
                self.runtime.remove(&i.name).await;
                self.insts.lock().retain(|x| !Arc::ptr_eq(x, &i));
            } else {
                return Ok(i);
            }
        }
        let i = self.make(tpl).await?;
        self.insts.lock().push(i.clone());
        Ok(i)
    }

    async fn scale_up(self: Arc<Self>, routed: Arc<Template>) {
        let Ok(_g) = self.wake.try_lock() else { return };
        let Some(tpl) = self.serving(&routed.deployment) else { return };
        if self.awake_of(&tpl.deployment) as u32 >= tpl.run.max_instances {
            return;
        }
        let Ok(inst) = self.ready_instance(&tpl).await else { return };
        let deadline = Instant::now() + self.cfg.wake_timeout;
        let woke = Instant::now();
        if self.start(&inst, &tpl.run, None, deadline).await.is_ok() {
            self.meter.lock().cold_start(now_ms(), woke.elapsed().as_secs_f64() * 1000.0);
            self.log.stream(Stream::System).line(format!("scaled out: {} instances", self.awake_count()));
        }
    }

    /// Make and start the instances a new deployment needs and check their health: the old
    /// ones keep serving meanwhile, except with a volume (one writer): then the old ones stop
    /// first, and the environment stays locked until [`EnvLive::commit`], so no request starts
    /// an old instance on the volume in between. On failure nothing of the new deployment is
    /// left.
    pub async fn prepare(self: &Arc<Self>, tpl: &Arc<Template>, health_path: Option<&str>) -> anyhow::Result<Prepared> {
        let run = &tpl.run;
        let was_awake = self.awake_count() as u32;
        let n = match run.kind {
            RunType::Cron => 0,
            // A worker following a backlog starts at what it wants, not back at its floor.
            RunType::Worker => run.min_instances.max(1).max(if run.scale.is_some() { self.wanted.load(Ordering::SeqCst) } else { 0 }),
            RunType::Web => was_awake.max(run.min_instances).max(1),
        }
        .min(run.max_instances);
        if run.kind == RunType::Cron || tpl.site.is_some() {
            // Nothing runs between runs (the next run uses the new template), or ever (a static
            // site's files answer).
            return Ok(Prepared { made: Vec::new(), took: Duration::ZERO, hold: None });
        }
        let mut hold = None;
        if run.volume.is_some() {
            // One volume, one instance: the old one lets go of it first.
            hold = Some(self.wake.clone().lock_owned().await);
            let old: Vec<Arc<Inst>> = self.insts.lock().drain(..).collect();
            for i in old {
                let _ = self.runtime.stop(&i.name, Duration::from_secs(10)).await;
                self.runtime.remove(&i.name).await;
            }
        }
        let began = Instant::now();
        let mut made = Vec::new();
        for _ in 0..n {
            match self.make(tpl).await {
                Ok(i) => made.push(i),
                Err(e) => {
                    self.discard(&made).await;
                    anyhow::bail!("making an instance: {e}");
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(run.health_timeout_s as u64);
        for i in &made {
            self.log.stream(Stream::System).line(format!("starting {}", i.name));
            if let Err(e) = self.start(i, run, health_path.or(run.health_path.as_deref()), deadline).await {
                self.discard(&made).await;
                let why = match e {
                    WakeError::Timeout(_) => format!("the health check didn't pass within {} s", run.health_timeout_s),
                    e => e.to_string(),
                };
                anyhow::bail!("{why}");
            }
        }
        Ok(Prepared { made, took: began.elapsed(), hold })
    }

    /// An environment that was asleep stays asleep: the new deployment's healthy instances stop
    /// (created, so the next request is only a start) before anything switches, so a deploy that
    /// reads as done has nothing of it still running.
    pub async fn rest_if_asleep(&self, tpl: &Template, prepared: &Prepared) {
        if tpl.run.kind != RunType::Web || tpl.run.min_instances > 0 || self.awake_count() > 0 {
            return;
        }
        for i in &prepared.made {
            let _ = self.runtime.stop(&i.name, Duration::from_secs(5)).await;
            *i.phase.lock() = Phase::Created;
        }
    }

    async fn discard(&self, insts: &[Arc<Inst>]) {
        for i in insts {
            let _ = self.runtime.stop(&i.name, Duration::from_secs(1)).await;
            self.runtime.remove(&i.name).await;
        }
    }

    /// The new deployment takes over: new requests go to `made`, the old instances finish what
    /// they're doing (up to `drain`) and go. An environment that was asleep goes back to sleep
    /// with the new deployment created, so the next request is a start, not a build.
    pub async fn commit(self: &Arc<Self>, tpl: Arc<Template>, prepared: Prepared) {
        let Prepared { made, hold, .. } = prepared;
        let _g = match hold {
            Some(g) => g,
            None => self.wake.clone().lock_owned().await,
        };
        if self.closed.load(Ordering::SeqCst) {
            self.discard(&made).await;
            return;
        }
        let was_awake = self.awake_count();
        *self.template.lock() = Some(tpl.clone());
        // A canary of an earlier rollout (one this deploy superseded) goes with the old ones.
        *self.canary.lock() = None;
        // Awake instances of this deployment already here stay: a request between the store's
        // switch and this commit can adopt the deployment from the store and wake one. The rest
        // (the old deployment's, and one adopted but never started) go.
        let old: Vec<Arc<Inst>> = {
            let mut insts = self.insts.lock();
            let (same, old): (Vec<_>, Vec<_>) = std::mem::take(&mut *insts).into_iter().partition(|i| i.deployment == tpl.deployment && i.awake());
            *insts = made.iter().cloned().chain(same).collect();
            old
        };
        // Asleep before: asleep after (the deploy path did this before its switch already).
        if tpl.run.kind == RunType::Web && was_awake == 0 && tpl.run.min_instances == 0 {
            for i in made.iter().filter(|i| matches!(i.phase(), Phase::Running(_))) {
                let _ = self.runtime.stop(&i.name, Duration::from_secs(5)).await;
                *i.phase.lock() = Phase::Created;
            }
        }
        if made.is_empty() && tpl.run.kind == RunType::Web && tpl.site.is_none() {
            // Warm-create one so the first request is only a start.
            if let Ok(i) = self.make(&tpl).await {
                self.insts.lock().push(i);
            }
        }
        self.drain(old, self.drain_of(&tpl.run));
    }

    /// How long an instance being replaced gets: `run.drain_s`, else the node's default.
    fn drain_of(&self, run: &Run) -> Duration {
        run.drain_s.map_or(self.cfg.drain, |s| Duration::from_secs(s as u64))
    }

    /// Instances out of rotation finish their requests, then get SIGTERM, and SIGKILL once
    /// `window` is up (a second after SIGTERM at least). Each on its own: one slow to finish
    /// doesn't use up another's time.
    fn drain(&self, old: Vec<Arc<Inst>>, window: Duration) {
        for i in old {
            let runtime = self.runtime.clone();
            tokio::spawn(async move {
                let until = Instant::now() + window;
                while i.in_flight() > 0 && Instant::now() < until {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                let grace = until.saturating_duration_since(Instant::now()).max(Duration::from_secs(1));
                let _ = runtime.stop(&i.name, grace).await;
                runtime.remove(&i.name).await;
            });
        }
    }

    /// A gradual rollout begins: the new deployment's healthy instances join the active one's,
    /// and `share` percent of requests go to them.
    pub async fn begin_canary(self: &Arc<Self>, tpl: Arc<Template>, prepared: Prepared, share: u8) {
        let _g = self.wake.lock().await;
        if self.closed.load(Ordering::SeqCst) {
            self.discard(&prepared.made).await;
            return;
        }
        self.served.lock().clear();
        self.insts.lock().extend(prepared.made);
        *self.canary.lock() = Some((tpl, share));
    }

    /// The canary's next step.
    pub fn set_share(&self, share: u8) {
        if let Some((_, s)) = self.canary.lock().as_mut() {
            *s = share;
        }
    }

    /// The canary takes every request and becomes the active deployment; the old instances
    /// drain.
    pub async fn promote_canary(self: &Arc<Self>) {
        let _g = self.wake.lock().await;
        let Some((tpl, _)) = self.canary.lock().take() else { return };
        *self.template.lock() = Some(tpl.clone());
        let old: Vec<Arc<Inst>> = {
            let mut insts = self.insts.lock();
            let (keep, old) = std::mem::take(&mut *insts).into_iter().partition(|i| i.deployment == tpl.deployment);
            *insts = keep;
            old
        };
        self.served.lock().clear();
        self.drain(old, self.drain_of(&tpl.run));
    }

    /// The canary is rolled back: every request goes to the active deployment again, and the
    /// canary's instances drain.
    pub async fn abort_canary(self: &Arc<Self>) {
        let _g = self.wake.lock().await;
        let Some((tpl, _)) = self.canary.lock().take() else { return };
        let gone: Vec<Arc<Inst>> = {
            let mut insts = self.insts.lock();
            let (gone, keep) = std::mem::take(&mut *insts).into_iter().partition(|i| i.deployment == tpl.deployment);
            *insts = keep;
            gone
        };
        self.served.lock().clear();
        self.drain(gone, self.drain_of(&tpl.run));
    }

    /// Set the active deployment without starting anything (a new owner taking the service).
    pub async fn adopt(self: &Arc<Self>, tpl: Arc<Template>) {
        let _g = self.wake.lock().await;
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        *self.template.lock() = Some(tpl.clone());
        if tpl.run.kind == RunType::Web
            && tpl.site.is_none()
            && self.insts.lock().is_empty()
            && let Ok(i) = self.make(&tpl).await
        {
            self.insts.lock().push(i);
        }
    }

    pub fn set_capped(&self, on: bool) {
        self.capped.store(on, Ordering::SeqCst);
    }

    /// A scaling worker's instances for its backlog now.
    pub fn set_wanted(&self, n: u32, backlog: Option<u64>) {
        self.wanted.store(n, Ordering::SeqCst);
        self.backlog.store(backlog.unwrap_or(u64::MAX), Ordering::SeqCst);
    }

    /// A line about scaling in the environment's log, once per change (every ten seconds would
    /// be thousands a day); empty clears it.
    pub fn note_scale(&self, line: &str) {
        let mut n = self.scale_note.lock();
        if *n != line {
            *n = line.to_string();
            if !line.is_empty() {
                self.log.stream(Stream::System).line(line);
            }
        }
    }

    /// What a scaling worker follows now: its backlog as last read, and the instances it wants.
    pub fn scaling(&self) -> Option<(Option<u64>, u32)> {
        let tpl = self.template()?;
        let run = &tpl.run;
        run.scale.as_ref()?;
        let b = self.backlog.load(Ordering::SeqCst);
        Some(((b != u64::MAX).then_some(b), self.wanted.load(Ordering::SeqCst).clamp(run.min_instances, run.max_instances.max(run.min_instances))))
    }

    /// Hold it asleep (a restore of its volume) until the guard drops: what runs stops, and no
    /// request, floor, rollout or cron run starts an instance meanwhile (they wait for the wake
    /// lock the guard keeps, or see `held`).
    pub async fn hold_asleep(self: &Arc<Self>) -> Held {
        self.held.store(true, Ordering::SeqCst);
        let lock = self.wake.clone().lock_owned().await;
        for i in self.instances() {
            if i.awake() {
                let _ = self.runtime.stop(&i.name, Duration::from_secs(10)).await;
                *i.phase.lock() = Phase::Created;
            }
        }
        Held { live: self.clone(), _lock: lock }
    }

    /// Stop every instance (they stay created); the next request wakes one.
    pub async fn sleep(self: &Arc<Self>) {
        let _g = self.wake.lock().await;
        for i in self.instances() {
            if i.awake() {
                let _ = self.runtime.stop(&i.name, Duration::from_secs(10)).await;
                *i.phase.lock() = Phase::Created;
            }
        }
    }

    /// Restart: stop everything and start what `min_instances` asks for again.
    pub async fn restart(self: &Arc<Self>) {
        self.sleep().await;
        // What a restart policy left stopped goes: fresh ones take its place.
        let done: Vec<Arc<Inst>> = self.insts.lock().iter().filter(|i| i.done.load(Ordering::SeqCst)).cloned().collect();
        self.insts.lock().retain(|i| !i.done.load(Ordering::SeqCst));
        for i in done {
            self.runtime.remove(&i.name).await;
        }
        self.tick().await;
    }

    /// Make every instance look idle for `d` longer than it is (tests drive the reaper with
    /// this instead of waiting).
    pub fn backdate(&self, d: Duration) {
        for i in self.instances() {
            let mut l = i.last_used.lock();
            *l = l.checked_sub(d).unwrap_or(*l);
        }
        if let Some(t) = self.over_since.lock().as_mut() {
            *t = t.checked_sub(d).unwrap_or(*t);
        }
    }

    /// Remove every instance (an environment's teardown, or the service leaving this node).
    pub async fn shutdown(&self) {
        let _g = self.wake.lock().await;
        self.closed.store(true, Ordering::SeqCst);
        *self.canary.lock() = None;
        self.viewed.lock().clear();
        let all: Vec<Arc<Inst>> = self.insts.lock().drain(..).collect();
        self.discard(&all).await;
        *self.template.lock() = None;
    }

    /// The reaper, about once a second: meter what runs, suspend and stop what's idle, keep
    /// `min_instances` up, scale down what's no longer needed, restart crashed workers.
    pub async fn tick(self: &Arc<Self>) {
        if self.template().is_none() {
            // Nothing live, but a deployment's own URL may have woken one: metered and reaped.
            if !self.viewed.lock().is_empty() {
                self.meter_tick(Instant::now()).await;
                let _g = self.wake.lock().await;
                self.reap_viewed().await;
            }
            return;
        }
        let now = Instant::now();
        let insts = self.meter_tick(now).await;
        let due = {
            let mut last = self.last_health.lock();
            let due = last.is_none_or(|t| now.duration_since(t) >= self.cfg.health_every);
            if due {
                *last = Some(now);
            }
            due
        };
        // Each deployment's instances by its own health path (a canary's may differ).
        let serving: Vec<Arc<Template>> = self.template().into_iter().chain(self.canary.lock().as_ref().map(|(t, _)| t.clone())).collect();
        for tpl in serving.iter().filter(|_| due) {
            if tpl.run.kind == RunType::Web
                && let Some(path) = tpl.run.health_path.as_deref()
            {
                let mine: Vec<Arc<Inst>> = insts.iter().filter(|i| i.deployment == tpl.deployment).cloned().collect();
                self.check_health(&mine, path, tpl.run.concurrency, tpl.run.h2c).await;
            }
        }
        if self.held.load(Ordering::SeqCst) {
            return;
        }
        if self.capped.load(Ordering::SeqCst) {
            // Capped: stop what still runs, keep no floor.
            if insts.iter().any(|i| i.awake()) {
                self.log.stream(Stream::System).line("the project reached its spending limit: stopped");
                self.sleep().await;
            }
            return;
        }
        let _g = self.wake.lock().await;
        // Read again under the lock: a teardown (`shutdown`) or a new deployment may have
        // happened while this tick waited, and the floor must not make instances for either.
        let Some(tpl) = self.template() else { return };
        match tpl.run.kind {
            RunType::Cron => {}
            // A static site's files answer: no floor, nothing to reap but what was left over.
            RunType::Web if tpl.site.is_some() => {}
            RunType::Worker => self.keep_workers(&tpl).await,
            RunType::Web => self.reap_web(&tpl).await,
        }
        self.reap_viewed().await;
    }

    /// Metering, once a tick: only running instances use anything. Its instances.
    async fn meter_tick(&self, now: Instant) -> Vec<Arc<Inst>> {
        let elapsed = {
            let mut last = self.last_tick.lock();
            let e = last.map(|l| now.duration_since(l).as_secs_f64()).unwrap_or(0.0).min(10.0);
            *last = Some(now);
            e
        };
        let insts = self.instances();
        let (mut cpu, mut mem, mut up) = (0.0, 0u64, 0usize);
        // What was measured, and what those instances could have used (`run.cpu` each).
        let (mut used, mut capacity) = (0.0, 0.0);
        for i in &insts {
            if !matches!(i.phase(), Phase::Running(_)) {
                continue;
            }
            up += 1;
            match self.runtime.usage(&i.name).await {
                Some(u) => {
                    let mut seen = i.cpu_seen.lock();
                    if let Some(prev) = *seen {
                        let d = u.cpu_ns.saturating_sub(prev) as f64 / 1e9;
                        cpu += d;
                        if let Some(tpl) = self.serving(&i.deployment) {
                            used += d;
                            capacity += tpl.run.cpu * elapsed;
                        }
                    }
                    *seen = Some(u.cpu_ns);
                    mem += u.memory_bytes;
                }
                // A runtime that can't say what an instance uses (Fargate, which bills by size ×
                // time) meters what it was given: its CPU for the interval and its memory.
                None => {
                    if let Some(tpl) = self.serving(&i.deployment) {
                        cpu += tpl.run.cpu * elapsed;
                        mem += (tpl.run.memory_mb as u64) << 20;
                    }
                }
            }
        }
        if up > 0 || elapsed > 0.0 {
            self.meter.lock().sample(now_ms(), cpu, mem, elapsed, up);
        }
        let mut s = self.cpu_samples.lock();
        if capacity > 0.0 {
            s.push_back((now, used, capacity));
        }
        while s.front().is_some_and(|(at, ..)| now.duration_since(*at) > self.cfg.cpu_window) {
            s.pop_front();
        }
        insts
    }

    /// Its awake instances' CPU over the last `cpu_window`, in percent of what they could use;
    /// None until the window is nearly full of measurements.
    fn cpu_load(&self) -> Option<f64> {
        let s = self.cpu_samples.lock();
        let span = s.back()?.0.duration_since(s.front()?.0);
        if span < self.cfg.cpu_window.mul_f64(0.8) {
            return None;
        }
        let (used, cap) = s.iter().fold((0.0, 0.0), |(u, c), (_, du, dc)| (u + du, c + dc));
        (cap > 0.0).then(|| used / cap * 100.0)
    }

    /// Instances of deployments asked for by their own URL go a minute after their last
    /// request, and the deployment is forgotten once none is left.
    async fn reap_viewed(&self) {
        if self.viewed.lock().is_empty() {
            return;
        }
        let viewed: std::collections::HashSet<String> = self.viewed.lock().keys().cloned().collect();
        let idle = |i: &Inst| i.in_flight() == 0 && i.last_used.lock().elapsed() >= Duration::from_secs(60);
        let gone: Vec<Arc<Inst>> = self.instances().into_iter().filter(|i| viewed.contains(&i.deployment) && idle(i)).collect();
        for i in &gone {
            if i.awake() && self.claim_idle(i).is_none() {
                continue;
            }
            let _ = self.runtime.stop(&i.name, Duration::from_secs(5)).await;
            self.runtime.remove(&i.name).await;
            self.insts.lock().retain(|x| !Arc::ptr_eq(x, i));
        }
        let left: std::collections::HashSet<String> = self.instances().into_iter().map(|i| i.deployment.clone()).collect();
        self.viewed.lock().retain(|d, (_, asked)| left.contains(d) || asked.elapsed() < Duration::from_secs(60));
    }

    /// Health checks while running (Render's, Fly's `checks`): every running instance's
    /// `health_path`, all at once. `health_failures` failures in a row (an app that hangs or
    /// answers errors with its port still open) replace it: it's out of rotation and stopped
    /// as a crash (counted toward a crash loop), and the next request or the floor starts a
    /// fresh one.
    async fn check_health(&self, insts: &[Arc<Inst>], path: &str, concurrency: u32, h2c: bool) {
        // An instance with as many requests as it takes is busy, not broken: a probe that queues
        // behind them proves nothing, so it isn't checked until it has room.
        let running: Vec<(&Arc<Inst>, SocketAddr)> = insts
            .iter()
            .filter(|i| i.in_flight() < concurrency.max(1))
            .filter_map(|i| match i.phase() {
                Phase::Running(Some(a)) => Some((i, a)),
                _ => None,
            })
            .collect();
        let probes = running.iter().map(|(i, a)| async move { (*i, *a, self.answers(*a, Some(path), h2c).await) });
        for (i, addr, ok) in futures::future::join_all(probes).await {
            if ok {
                i.health_fails.store(0, Ordering::SeqCst);
                continue;
            }
            let n = i.health_fails.fetch_add(1, Ordering::SeqCst) + 1;
            self.log.stream(Stream::System).line(format!("{}: health check {path} failed ({n} of {})", i.name, self.cfg.health_failures));
            // Still the instance that was checked (not stopped or replaced meanwhile).
            if n >= self.cfg.health_failures && i.phase() == Phase::Running(Some(addr)) {
                // Out of rotation now; what it's serving gets the drain to finish.
                self.crashed_with(i, &format!("its health check {path} failed {n} times in a row"), self.cfg.drain);
            }
        }
    }

    async fn reap_web(self: &Arc<Self>, tpl: &Arc<Template>) {
        let run = &tpl.run;
        // A deployment's own URL's instances are reap_viewed's, not part of the scale.
        let insts: Vec<Arc<Inst>> = self.instances().into_iter().filter(|i| !self.only_viewed(&i.deployment)).collect();
        let suspend_after = Duration::from_secs(run.suspend_after_s as u64);
        let sleep_after = Duration::from_secs(run.sleep_after_s as u64);
        let mut awake = insts.iter().filter(|i| i.awake()).count() as u32;
        // Fewer than needed by the requests in flight for a while → one goes.
        let in_flight: u32 = insts.iter().map(|i| i.in_flight()).sum();
        let mut needed = in_flight.div_ceil(run.concurrency).max(run.min_instances).max(1);
        // On CPU too: as many as would bring the average under the target.
        if let (Some(target), Some(load)) = (run.cpu_target, self.cpu_load().filter(|_| awake > 0)) {
            let by_cpu = ((awake as f64 * load / target as f64).ceil() as u32).min(run.max_instances.max(1));
            self.cpu_need.store(by_cpu, Ordering::SeqCst);
            if by_cpu > awake && awake < run.max_instances {
                // One more at a time, then a fresh window to judge it by.
                if let Ok(inst) = self.make(tpl).await {
                    self.insts.lock().push(inst.clone());
                    let deadline = Instant::now() + self.cfg.wake_timeout;
                    if self.start(&inst, run, None, deadline).await.is_ok() {
                        awake += 1;
                        self.cpu_samples.lock().clear();
                        self.cpu_need.store(awake, Ordering::SeqCst);
                        self.log.stream(Stream::System).line(format!("scaled out: CPU at {load:.0}% of the {target}% target, {awake} instances"));
                    }
                }
            }
            needed = needed.max(by_cpu);
        } else if run.cpu_target.is_some() {
            needed = needed.max(self.cpu_need.load(Ordering::SeqCst));
        }
        let over = awake > needed;
        let scale_down = {
            let mut s = self.over_since.lock();
            match (over, *s) {
                (false, _) => {
                    *s = None;
                    false
                }
                (true, None) => {
                    *s = Some(Instant::now());
                    false
                }
                (true, Some(t)) => t.elapsed() >= self.cfg.scale_down_after,
            }
        };
        if scale_down
            && let Some(i) = insts.iter().filter(|i| matches!(i.phase(), Phase::Running(_)) && i.in_flight() == 0).min_by_key(|i| *i.last_used.lock())
            && self.claim_idle(i).is_some()
        {
            let _ = self.runtime.stop(&i.name, Duration::from_secs(10)).await;
            *i.phase.lock() = Phase::Created;
            awake -= 1;
            *self.over_since.lock() = None;
            self.log.stream(Stream::System).line(format!("scaled in: {awake} instances"));
        }
        for i in &insts {
            if matches!(i.phase(), Phase::Running(_) | Phase::Suspended(_)) && !self.runtime.running(&i.name).await {
                // Exited on its own: the next request starts it again.
                let why = self.exit_reason(&i.name, run).await;
                self.crashed(i, &why);
                awake = awake.saturating_sub(1);
                self.log.stream(Stream::System).line(format!("{} stopped: {why}", i.name));
                continue;
            }
            if i.in_flight() > 0 {
                continue;
            }
            let idle = i.last_used.lock().elapsed();
            match i.phase() {
                Phase::Running(addr) if awake > run.min_instances => {
                    if idle >= sleep_after || (!self.runtime.can_suspend() && idle >= suspend_after.max(sleep_after)) {
                        if self.claim_idle(i).is_some() {
                            let _ = self.runtime.stop(&i.name, Duration::from_secs(10)).await;
                            *i.phase.lock() = Phase::Created;
                            awake -= 1;
                        }
                    } else if self.runtime.can_suspend() && run.suspend_after_s > 0 && idle >= suspend_after && self.claim_idle(i).is_some() {
                        let suspended = self.runtime.suspend(&i.name).await.is_ok();
                        *i.phase.lock() = if suspended { Phase::Suspended(addr) } else { Phase::Running(addr) };
                    }
                }
                Phase::Suspended(_) if idle >= sleep_after && awake > run.min_instances && self.claim_idle(i).is_some() => {
                    let _ = self.runtime.stop(&i.name, Duration::from_secs(10)).await;
                    *i.phase.lock() = Phase::Created;
                    awake -= 1;
                }
                _ => {}
            }
        }
        // Keep the floor.
        while awake < run.min_instances {
            let Ok(inst) = self.ready_instance(tpl).await else { break };
            let deadline = Instant::now() + self.cfg.wake_timeout;
            if self.start(&inst, run, None, deadline).await.is_err() {
                break;
            }
            awake += 1;
        }
    }

    /// Whether a stopped instance's process ended by itself with 0 (its work done).
    async fn exited_cleanly(&self, name: &str) -> bool {
        self.runtime.exited(name).await.is_some_and(|e| e.code == Some(0) && !e.oom)
    }

    /// A worker stopped (`clean`: exit 0): counted, and by its restart policy started again on
    /// a later pass or left stopped (`done`) until a deploy or a restart.
    fn after_stop(&self, i: &Inst, run: &Run, clean: bool, why: &str) {
        let in_a_row = if clean && run.restart != Restart::Always { i.crashes.load(Ordering::SeqCst) } else { i.crashes.fetch_add(1, Ordering::SeqCst) + 1 };
        let again = match run.restart {
            Restart::Always => true,
            Restart::OnFailure => !clean && in_a_row < run.restart_tries.unwrap_or(10),
            Restart::Never => false,
        };
        if again {
            self.log.stream(Stream::System).line(format!("{} stopped: {why}; restarting", i.name));
        } else {
            i.done.store(true, Ordering::SeqCst);
            self.log.stream(Stream::System).line(format!("{} stopped: {why}; its restart policy ({}) leaves it stopped until a deploy or a restart", i.name, run.restart.as_str()));
        }
    }

    async fn keep_workers(self: &Arc<Self>, tpl: &Arc<Template>) {
        let run = &tpl.run;
        // A worker that follows its work runs what its backlog wants; else its floor.
        let target = match &run.scale {
            Some(_) => self.wanted.load(Ordering::SeqCst).clamp(run.min_instances, run.max_instances.max(run.min_instances)),
            None => run.min_instances,
        };
        let insts = self.instances();
        let mut up = 0;
        for i in &insts {
            match i.phase() {
                Phase::Running(_) if self.runtime.running(&i.name).await => {
                    up += 1;
                    // A minute up: its crashes are no longer in a row.
                    if i.started_at().is_some_and(|t| now_ms() - t > 60_000) {
                        i.crashes.store(0, Ordering::SeqCst);
                    }
                }
                Phase::Running(_) => {
                    let why = self.exit_reason(&i.name, run).await;
                    let clean = self.exited_cleanly(&i.name).await;
                    if clean && run.restart != Restart::Always {
                        // Done with its work, not a crash.
                        *i.phase.lock() = Phase::Created;
                    } else {
                        self.crashed(i, &why);
                    }
                    self.after_stop(i, run, clean, &why);
                }
                _ => {}
            }
        }
        // Left stopped by the policy: they count, so nothing replaces them.
        let finished = insts.iter().filter(|i| i.done.load(Ordering::SeqCst)).count() as u32;
        for i in insts.iter().filter(|i| i.phase() == Phase::Crashed && !i.done.load(Ordering::SeqCst)) {
            if up >= target {
                break;
            }
            // Back off: `restart_backoff` (1 s) doubling to 60 times it since it last ran.
            let crashes = i.crashes.load(Ordering::SeqCst).min(6);
            let wait = (self.cfg.restart_backoff * (1u32 << crashes)).min(self.cfg.restart_backoff * 60);
            if i.last_used.lock().elapsed() < wait {
                continue;
            }
            *i.last_used.lock() = Instant::now();
            let deadline = Instant::now() + self.cfg.wake_timeout;
            if self.start(i, run, None, deadline).await.is_ok() {
                up += 1;
            } else {
                // Ended within its grace: the policy decides as for any stop.
                let clean = self.exited_cleanly(&i.name).await;
                if clean && run.restart != Restart::Always {
                    *i.phase.lock() = Phase::Created;
                }
                self.after_stop(i, run, clean, "it ended right after it started");
            }
        }
        while up + finished < target {
            let Ok(inst) = self.make(tpl).await else { break };
            self.insts.lock().push(inst.clone());
            let deadline = Instant::now() + self.cfg.wake_timeout;
            if self.start(&inst, run, None, deadline).await.is_err() {
                break;
            }
            up += 1;
            self.log.stream(Stream::System).line(format!("scaled out: {up} instances for the backlog"));
        }
        // Fewer wanted for `scale_down_after`: one goes (the newest), its work given the drain
        // to finish; the rest follow on later ticks.
        let fewer = up > target;
        let scale_in = {
            let mut s = self.fewer_since.lock();
            match (fewer, *s) {
                (false, _) => {
                    *s = None;
                    false
                }
                (true, None) => {
                    *s = Some(Instant::now());
                    false
                }
                (true, Some(t)) => t.elapsed() >= self.cfg.scale_down_after,
            }
        };
        if scale_in
            && let Some(i) = self.instances().into_iter().filter(|i| matches!(i.phase(), Phase::Running(_))).max_by_key(|i| i.started_at())
        {
            // Out of the set now; its stop (the drain to finish its work) runs off the wake lock,
            // so a deploy's commit doesn't wait for it.
            *i.phase.lock() = Phase::Stopping;
            self.insts.lock().retain(|x| !Arc::ptr_eq(x, &i));
            *self.fewer_since.lock() = None;
            self.log.stream(Stream::System).line(format!("scaled in: {} instances for the backlog", up - 1));
            let (runtime, name, grace) = (self.runtime.clone(), i.name.clone(), self.drain_of(run));
            tokio::spawn(async move {
                let _ = runtime.stop(&name, grace).await;
                runtime.remove(&name).await;
            });
        }
    }

    /// One cron run: a fresh instance of the active deployment, to its exit or `timeout`.
    pub async fn run_once(self: &Arc<Self>, timeout: Duration) -> anyhow::Result<(String, Option<i32>, bool)> {
        let e = self.run_to_exit(None, &[], timeout, None, tokio_util::sync::CancellationToken::new(), |_| {}).await?;
        Ok((e.deployment, e.code, e.timed_out))
    }

    /// A fresh instance of the active deployment run to its exit, `timeout` or `cancel`: a cron
    /// run, or a one-off job (`command` in place of the image's, with no port and no volume,
    /// since the service's own instance may be using it). `made` gets the container's name
    /// before it starts; the container is removed after, whatever happened.
    pub async fn run_to_exit(
        self: &Arc<Self>,
        command: Option<&str>,
        vars: &[(String, String)],
        timeout: Duration,
        log: Option<LogSink>,
        cancel: tokio_util::sync::CancellationToken,
        made: impl FnOnce(&str),
    ) -> anyhow::Result<Exit> {
        let mut tpl = (*self.template().ok_or_else(|| anyhow::anyhow!("no deployment"))?).clone();
        anyhow::ensure!(command.is_some() || tpl.spec.volume.is_none() || !self.held.load(Ordering::SeqCst), "its volume is being restored");
        if let Some(cmd) = command {
            tpl.spec.command = Some(cmd.to_string());
            tpl.spec.port = None;
            tpl.spec.volume = None;
            tpl.spec.aliases.clear();
        }
        tpl.spec.vars.extend(vars.iter().cloned());
        let inst = self.make(&tpl).await?;
        made(&inst.name);
        let log = log.unwrap_or_else(|| self.log.instance(&inst.name, &inst.deployment));
        let began = Instant::now();
        let ran = async {
            self.runtime.start(&inst.name, log).await?;
            // A one-off runs within its limit from the start.
            self.runtime.settle(&inst.name).await;
            tokio::select! {
                code = tokio::time::timeout(timeout, self.runtime.wait(&inst.name)) => Ok::<_, anyhow::Error>(match code {
                    Ok(code) => (code.ok(), false, false),
                    Err(_) => (None, true, false),
                }),
                _ = cancel.cancelled() => Ok((None, false, true)),
            }
        }
        .await;
        let _ = self.runtime.stop(&inst.name, Duration::from_secs(10)).await;
        let secs = began.elapsed().as_secs_f64();
        // A run is metered at its allocation: it's short, and the container is gone after.
        self.meter.lock().sample(now_ms(), secs * tpl.run.cpu * 0.5, (tpl.run.memory_mb as u64) << 20, secs, 1);
        self.runtime.remove(&inst.name).await;
        let (code, timed_out, canceled) = ran?;
        Ok(Exit { deployment: tpl.deployment.clone(), container: inst.name.clone(), code, timed_out, canceled })
    }
}

/// How a run ended.
#[derive(Debug)]
pub struct Exit {
    pub deployment: String,
    pub container: String,
    pub code: Option<i32>,
    pub timed_out: bool,
    pub canceled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::LogHub;
    use crate::runtime::fake::{Fake, FakeState};

    fn template(run: Run) -> Arc<Template> {
        template_of("dep_aaaaaaaaaaaa", run, &[])
    }

    fn template_of(deployment: &str, run: Run, vars: &[(&str, &str)]) -> Arc<Template> {
        Arc::new(Template {
            deployment: deployment.into(),
            spec: InstanceSpec {
                name: String::new(),
                service: "svc_x".into(),
                env: "production".into(),
                deployment: deployment.into(),
                image: "img".into(),
                command: None,
                vars: vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                port: Some(8080),
                cpu: 0.5,
                memory_mb: 512,
                network: None,
                aliases: vec![],
                volume: None,
                idle_stop_s: None,
                registry: None,
            },
            run,
            spa: false,
            site: None,
        })
    }

    #[tokio::test]
    async fn idle_instances_suspend_then_stop_and_a_request_wakes_one() {
        let fake = Fake::new();
        let hub = LogHub::new(None);
        let cfg = LiveConfig { probe_every: Duration::from_millis(1), ..Default::default() };
        let live = EnvLive::new("svc_x", "production", fake.clone(), cfg, hub.sink("env:x", Stream::System));
        let run = Run { suspend_after_s: 30, sleep_after_s: 600, ..Run::default() };
        let tpl = template(run);
        let prepared = live.prepare(&tpl, None).await.unwrap();
        live.commit(tpl, prepared).await;
        assert_eq!(live.state(), InstanceState::Stopped, "asleep before: asleep after, created");
        let l = live.acquire().await.unwrap();
        assert!(l.cold);
        drop(l);
        assert_eq!(live.state(), InstanceState::Running);
        live.backdate(Duration::from_secs(31));
        live.tick().await;
        assert_eq!(live.state(), InstanceState::Suspended);
        let l = live.acquire().await.unwrap();
        assert!(!l.cold, "resumed, not started");
        drop(l);
        live.backdate(Duration::from_secs(601));
        live.tick().await;
        live.tick().await;
        assert_eq!(live.state(), InstanceState::Stopped);
        let name = &live.instances()[0].name;
        assert_eq!(fake.state(name), Some(FakeState::Stopped));
    }

    fn live(fake: &Arc<Fake>) -> Arc<EnvLive> {
        let cfg = LiveConfig { probe_every: Duration::from_millis(1), ..Default::default() };
        EnvLive::new("svc_x", "production", fake.clone(), cfg, LogHub::new(None).sink("env:x", Stream::System))
    }

    #[tokio::test]
    async fn an_instance_that_dies_is_replaced_and_a_wake_left_by_its_client_still_finishes() {
        let fake = Fake::new();
        let live = live(&fake);
        let tpl = template_of("dep_aaaaaaaaaaaa", Run::default(), &[("FAKE_BOOT_MS", "150")]);
        let prepared = live.prepare(&tpl, None).await.unwrap();
        live.commit(tpl, prepared).await;
        // The client gives up mid-wake: the wake finishes anyway, nothing is left Starting.
        assert!(tokio::time::timeout(Duration::from_millis(20), live.acquire()).await.is_err());
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(live.state(), InstanceState::Running);
        // Killed under it (well before it would go to sleep): the reaper notices, and the next
        // request starts a fresh one.
        let dead = live.instances()[0].name.clone();
        fake.crash(&dead);
        live.tick().await;
        assert_eq!(live.state(), InstanceState::Crashed);
        let l = live.acquire().await.unwrap();
        assert!(l.cold && l.inst.name != dead);
        assert!(live.crash_loop().is_none(), "one crash isn't a loop");
        // Two more: a crash loop, with the last reason.
        drop(l);
        for _ in 0..2 {
            let name = live.instances().iter().find(|i| i.state() == InstanceState::Running).unwrap().name.clone();
            fake.crash(&name);
            live.tick().await;
            drop(live.acquire().await.unwrap());
        }
        let c = live.crash_loop().expect("three crashes in ten minutes");
        assert_eq!((c.crashes, c.last_error.as_str()), (3, "it was killed (exit 137: out of memory, or a SIGKILL)"), "the runtime's exit code says why");
    }

    #[tokio::test]
    async fn a_volume_rollout_never_starts_the_old_deployment_on_the_volume() {
        let fake = Fake::new();
        let live = live(&fake);
        let run = Run { volume: Some(crate::model::Volume { mount: "/data".into(), size_gb: 1 }), min_instances: 1, ..Run::default() };
        let old = template_of("dep_aaaaaaaaaaaa", run.clone(), &[]);
        let prepared = live.prepare(&old, None).await.unwrap();
        live.commit(old, prepared).await;
        // The new deployment boots slowly; a request comes in while the old instance is gone.
        let new = template_of("dep_bbbbbbbbbbbb", run, &[("FAKE_BOOT_MS", "200")]);
        let rollout = {
            let (live, new) = (live.clone(), new.clone());
            tokio::spawn(async move {
                let p = live.prepare(&new, None).await.unwrap();
                live.commit(new, p).await;
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let l = live.acquire().await.unwrap();
        assert_eq!(l.inst.deployment, "dep_bbbbbbbbbbbb", "the request waited for the new one");
        rollout.await.unwrap();
        let up: Vec<_> = fake.instances("svc_x").into_iter().filter(|(_, _, st)| *st == FakeState::Running).collect();
        assert_eq!(up.len(), 1, "{up:?}");
    }

    #[tokio::test]
    async fn nothing_comes_back_after_a_teardown() {
        let fake = Fake::new();
        let live = live(&fake);
        let tpl = template(Run::default());
        let prepared = live.prepare(&tpl, None).await.unwrap();
        live.commit(tpl.clone(), prepared).await;
        live.shutdown().await;
        // A request or a new owner's adopt that held this environment from before.
        live.adopt(tpl).await;
        assert!(matches!(live.acquire().await, Err(WakeError::NoDeployment)));
        live.tick().await;
        assert!(fake.instances("svc_x").is_empty());
    }
}
