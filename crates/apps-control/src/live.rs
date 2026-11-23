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
use crate::runtime::Adopted;
use crate::runtime::{InstanceSpec, Runtime};
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::sync::Arc;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The names of a deployment's instances in an environment, but their number:
/// `roll-apps-<service>-<env>-<deployment, 6>-`.
/// A cron run's container: its own id in its name, so its lines in the environment's log are its
/// alone (a counter would start again with the next process).
pub fn run_name(service: &str, env: &str, deployment: &str, run: &str) -> String {
    format!("{}run-{}", instance_prefix(service, env, deployment), run.strip_prefix("run_").unwrap_or(run))
}

pub fn instance_prefix(service: &str, env: &str, deployment: &str) -> String {
    let short = deployment.strip_prefix("dep_").unwrap_or(deployment);
    let svc = service.strip_prefix("svc_").unwrap_or(service);
    format!("roll-apps-{svc}-{env}-{}-", &short[..short.len().min(6)])
}

/// How far back crashes count toward a crash loop.
const CRASH_WINDOW: Duration = Duration::from_secs(600);
/// How long an instance over `run.busy_cpu` stays held after its last busy sample.
const BUSY_HOLD: Duration = Duration::from_secs(30);

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
    /// That site's edge functions ([`crate::edge`]), and the service that runs them.
    pub edge: Vec<apps_build::edge::EdgeFn>,
    pub edge_service: String,
    /// A closed environment's: the hosts its placeholders are bound to (its CA names them all).
    pub intercept_hosts: Vec<String>,
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
    /// Its last hour by minute (Railway's per replica): `(minute, CPU seconds, seconds measured,
    /// peak memory bytes)`, from the reaper's samples.
    minutes: Mutex<std::collections::VecDeque<(i64, f64, f64, u64)>>,
    /// Its template's other ports (custom domains' own), and where they answer, learned at each
    /// start.
    wanted: Vec<u16>,
    ports: Mutex<HashMap<u16, SocketAddr>>,
    /// A scrape of its metrics endpoint is out ([`crate::prom`]).
    scraping: AtomicBool,
    /// Kept awake until then (`ROLL_AWAKE_URL`: work after its answer), whatever its idle time.
    held_until: Mutex<Option<Instant>>,
}

/// An instance's last hour by minute ([`EnvLive::by_instance`]).
#[derive(Clone, Debug, serde::Serialize)]
pub struct InstanceSeries {
    pub instance: String,
    pub deployment: String,
    pub minutes: Vec<i64>,
    /// Cores in use, on average over each minute.
    pub cpu: Vec<f64>,
    /// The minute's peak.
    pub memory_bytes: Vec<u64>,
}

impl Inst {
    /// One sample: `cpu_s` CPU seconds over `elapsed` seconds, and memory in use now.
    fn record(&self, minute: i64, cpu_s: f64, elapsed: f64, memory: u64) {
        let mut m = self.minutes.lock();
        match m.back_mut() {
            Some(last) if last.0 == minute => {
                last.1 += cpu_s;
                last.2 += elapsed;
                last.3 = last.3.max(memory);
            }
            _ => m.push_back((minute, cpu_s, elapsed, memory)),
        }
        while m.front().is_some_and(|f| f.0 <= minute - 60) {
            m.pop_front();
        }
    }

    fn series(&self) -> Option<InstanceSeries> {
        let m = self.minutes.lock();
        if m.is_empty() {
            return None;
        }
        Some(InstanceSeries {
            instance: self.name.clone(),
            deployment: self.deployment.clone(),
            minutes: m.iter().map(|x| x.0).collect(),
            cpu: m.iter().map(|x| if x.2 > 0.0 { (x.1 / x.2 * 1000.0).round() / 1000.0 } else { 0.0 }).collect(),
            memory_bytes: m.iter().map(|x| x.3).collect(),
        })
    }

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
            minutes: Mutex::new(std::collections::VecDeque::new()),
            wanted: Vec::new(),
            ports: Mutex::new(HashMap::new()),
            scraping: AtomicBool::new(false),
            held_until: Mutex::new(None),
        })
    }

    /// It's kept awake now (a hold that hasn't ended).
    /// Held awake until at least `for_` from now (a later hold stands). The hold's end.
    fn hold(&self, for_: Duration) -> Instant {
        let until = Instant::now() + for_;
        let mut h = self.held_until.lock();
        let until = h.filter(|t| *t > until).unwrap_or(until);
        *h = Some(until);
        until
    }

    pub fn held(&self) -> bool {
        self.held_until.lock().is_some_and(|t| t > Instant::now())
    }

    /// An instance of `tpl`'s deployment, knowing the other ports it publishes.
    fn of(name: String, tpl: &Template) -> Arc<Inst> {
        let mut i = Inst::new(name, tpl.deployment.clone());
        if let Some(i) = Arc::get_mut(&mut i) {
            i.wanted = tpl.spec.ports.clone();
        }
        i
    }

    /// Where it answers while it runs (never while suspended or stopped).
    pub fn running_addr(&self) -> Option<SocketAddr> {
        match *self.phase.lock() {
            Phase::Running(a) => a,
            _ => None,
        }
    }

    /// Whether this scrape may start (none other is out).
    pub fn begin_scrape(&self) -> bool {
        !self.scraping.swap(true, Ordering::SeqCst)
    }

    pub fn end_scrape(&self) {
        self.scraping.store(false, Ordering::SeqCst);
    }

    /// Where it answers on `port` (a custom domain's own), if it publishes it.
    pub fn port_addr(&self, port: u16) -> Option<SocketAddr> {
        self.ports.lock().get(&port).copied()
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
    /// Its environment's "a slot is free", for a request queued at the hard limit.
    freed: Arc<tokio::sync::Notify>,
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
        self.freed.notify_one();
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
    /// Every instance at its concurrency, at max_instances, for all of `run.queue_s`.
    #[error("every instance was busy for {0:?}")]
    Full(Duration),
    #[error("this project reached its spending limit for the month")]
    Capped,
    #[error("this environment is paused")]
    Paused,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Served {
    pub requests: u64,
    pub errors: u64,
    /// Its instances that crashed or failed their health check (no request sees those).
    pub crashes: u64,
    /// Its answers' times (to their headers): how many in each of [`LAT_BUCKETS`] buckets of
    /// 2^¼ steps (ms, about 19% each).
    pub lat: [u32; LAT_BUCKETS],
}

/// Buckets of a step's answer times: bucket `i` holds up to 2^((i+1)/4) ms (the last, the
/// rest: past 16 s).
pub const LAT_BUCKETS: usize = 96;

impl Default for Served {
    fn default() -> Self {
        Served { requests: 0, errors: 0, crashes: 0, lat: [0; LAT_BUCKETS] }
    }
}

impl Served {
    pub fn error_pct(&self) -> f64 {
        if self.requests == 0 { 0.0 } else { self.errors as f64 * 100.0 / self.requests as f64 }
    }

    /// One answer that took `ms`.
    pub fn took(&mut self, ms: f64) {
        let i = if ms <= 1.0 { 0 } else { ((ms.log2() * 4.0).ceil() as usize).saturating_sub(1).min(LAT_BUCKETS - 1) };
        self.lat[i] = self.lat[i].saturating_add(1);
    }

    /// Its answers' p95 (ms, read between its bucket's ends by rank), when it timed any.
    pub fn p95_ms(&self) -> Option<f64> {
        let n: u64 = self.lat.iter().map(|&c| c as u64).sum();
        if n == 0 {
            return None;
        }
        let want = (n as f64 * 0.95).ceil() as u64;
        let mut seen = 0;
        for (i, &c) in self.lat.iter().enumerate() {
            if c > 0 && seen + c as u64 >= want {
                let (lo, hi) = (if i == 0 { 0.0 } else { 2f64.powf(i as f64 / 4.0) }, 2f64.powf((i + 1) as f64 / 4.0));
                return Some(lo + (hi - lo) * (want - seen) as f64 / c as f64);
            }
            seen += c as u64;
        }
        None
    }

    /// Another's counts added in.
    pub fn add(&mut self, o: &Served) {
        self.requests += o.requests;
        self.errors += o.errors;
        self.crashes += o.crashes;
        for (a, b) in self.lat.iter_mut().zip(o.lat.iter()) {
            *a = a.saturating_add(*b);
        }
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
    /// Its own error and maintenance pages.
    pub pages: crate::model::Pages,
    /// A preview's (ephemeral): never indexed.
    pub preview: bool,
    /// Headers its answers carry.
    pub headers: Vec<crate::model::HeaderRule>,
    /// The live deployment's roll.toml firewall rules (deny, limit, challenge), judged beside
    /// `firewall`: the stricter answer wins, so the tree only ever tightens.
    pub file_firewall: Vec<crate::model::FirewallRule>,
    /// The live deployment's roll.toml headers, after `headers` and never over one they set.
    pub file_headers: Vec<crate::model::HeaderRule>,
    /// Paths another service answers, the first that takes a path deciding.
    pub routes: Vec<crate::model::Route>,
    /// Its own redirects, answered before anything wakes.
    pub redirects: Option<Arc<crate::site_rules::EnvRedirects>>,
    /// Traces are on: the proxy adds its own span to each request's trace.
    pub traces: bool,
    /// Its pages carry roll's Web Vitals tag.
    pub vitals: bool,
    /// Its challenge rules' key ([`crate::challenge`]), when it has such a rule.
    pub challenge_key: Option<Vec<u8>>,
    /// Its split test.
    pub split: Option<crate::model::Split>,
    /// Where a share of its bodiless requests is copied.
    pub mirror: Option<crate::model::Split>,
    /// Its snippets, joined: before `</head>`, before `</body>`.
    pub snippets: Option<(bytes::Bytes, bytes::Bytes)>,
}

/// What a gRPC health check is called in the log (`run.health_grpc`).
const GRPC_HEALTH: &str = "grpc.health.v1.Health/Check";

/// Whether a gRPC frame holds a HealthCheckResponse whose `status` (field 1) is SERVING (1).
fn grpc_serving(frame: &[u8]) -> bool {
    let Some(len) = frame.get(1..5).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize) else { return false };
    let Some(msg) = frame.get(5..5 + len).filter(|_| frame[0] == 0) else { return false };
    // Fields in order: a varint `status` is tag 0x08; anything else (unknown fields) is skipped.
    let mut i = 0;
    let mut serving = false;
    while i < msg.len() {
        let tag = msg[i];
        i += 1;
        let mut varint = || {
            let (mut v, mut shift) = (0u64, 0);
            while let Some(b) = msg.get(i) {
                i += 1;
                v |= u64::from(b & 0x7f) << shift;
                if b & 0x80 == 0 {
                    return Some(v);
                }
                shift += 7;
                if shift > 63 {
                    return None;
                }
            }
            None
        };
        match tag & 7 {
            0 => match varint() {
                Some(v) if tag >> 3 == 1 => serving = v == 1,
                Some(_) => {}
                None => return false,
            },
            2 => match varint() {
                Some(n) => i = i.saturating_add(n as usize),
                None => return false,
            },
            _ => return false,
        }
    }
    serving
}

/// A rate-limit bucket: which (0 the environment's, 1 + i firewall rule i's, [`FILE_BUCKETS`] + i
/// the live deployment's roll.toml rule i's) and whose.
/// A bucket, and its client: an address, or a rule's key's hashed value.
type BucketKey = (u16, Option<std::net::IpAddr>, Option<u64>);

/// Where the buckets of a deployment's own firewall rules start (past any environment's).
const FILE_BUCKETS: u16 = 1000;

/// The first of `rules` that matches decides; none: in. A limit's bucket is `base` + its index.
fn first_match(rules: &[crate::model::FirewallRule], asked: &crate::model::Asked<'_>, base: u16) -> Verdict {
    match rules.iter().enumerate().find(|(_, r)| r.matches(asked)) {
        None => Verdict::Allow,
        Some((_, r)) if r.action == crate::model::FirewallAction::Allow => Verdict::Allow,
        Some((i, r)) if r.action == crate::model::FirewallAction::Limit => Verdict::Limit(base + i as u16, crate::model::RateLimit { per_minute: r.per_minute.unwrap_or(60), burst: r.burst }, r.client_key(asked)),
        Some((_, r)) if r.action == crate::model::FirewallAction::Challenge => Verdict::Challenge,
        Some(_) => Verdict::Deny,
    }
}

/// A firewall's answer: in, a 403, or in under a rule's own limit (its bucket, 1 + its index).
#[derive(Debug, PartialEq)]
pub enum Verdict {
    Allow,
    Deny,
    /// Its bucket, its limit, and its client by the rule's key (none: by address).
    Limit(u16, crate::model::RateLimit, Option<u64>),
    /// In with a pass ([`crate::challenge`]); else its page.
    Challenge,
}

/// Clients a rate limit remembers at most (a full bucket is the same as none, so those go first).
const MAX_BUCKETS: usize = 100_000;

impl Gate {
    /// Whether its firewall lets the request in: the first rule that matches decides; none
    /// matching, it does.
    /// What its firewall says of the request: the first rule that matches decides; none lets it
    /// in.
    pub fn firewall_verdict(&self, asked: &crate::model::Asked<'_>) -> Verdict {
        let own = first_match(&self.firewall, asked, 1);
        if self.file_firewall.is_empty() {
            return own;
        }
        // The tree's rules on their own, their limits in buckets of their own: the stricter of
        // the two answers (deny, then a challenge, then the lower limit) decides.
        let file = first_match(&self.file_firewall, asked, FILE_BUCKETS);
        let rank = |v: &Verdict| match v {
            Verdict::Allow => 0,
            Verdict::Limit(..) => 1,
            Verdict::Challenge => 2,
            Verdict::Deny => 3,
        };
        match (own, file) {
            (Verdict::Limit(a, x, ka), Verdict::Limit(b, y, kb)) => if y.per_minute < x.per_minute { Verdict::Limit(b, y, kb) } else { Verdict::Limit(a, x, ka) },
            (a, b) => if rank(&b) > rank(&a) { b } else { a },
        }
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

/// Whether a TCP app listens behind `addr`: Docker's proxy on a published port accepts the
/// connection whatever the app does, and closes it at once when nothing listens inside, so a
/// connection still open a moment later (or one that already said something) is the app.
async fn tcp_answers(addr: SocketAddr) -> bool {
    use tokio::io::AsyncReadExt;
    let Ok(Ok(mut c)) = tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(addr)).await else { return false };
    let mut b = [0u8; 1];
    match tokio::time::timeout(Duration::from_millis(100), c.read(&mut b)).await {
        Err(_) => true,
        Ok(Ok(n)) => n > 0,
        Ok(Err(_)) => false,
    }
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
    /// Let go to another node ([`Self::let_go`]): stops and removes spawned before don't run (on
    /// a shared runtime the next owner may have kept the instance; it removes what it didn't).
    released: Arc<AtomicBool>,
    /// Its project reached its spend limit: nothing starts, and the tick stops what runs.
    capped: AtomicBool,
    /// Paused by its owner: the same, until it's resumed.
    paused: AtomicBool,
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
    /// Instances out of rotation still finishing their requests, and each one's deployment.
    draining: Arc<Mutex<HashMap<String, String>>>,
    /// Requests waiting on a wake.
    waiters: AtomicU32,
    /// A lease ended: a request queued at the hard limit ([`crate::model::Run::queue_s`]) looks again.
    freed: Arc<tokio::sync::Notify>,
    /// The last failed wake, of which deployment, and how many failed in a row: the requests
    /// behind it get its error instead of each trying again, for a backoff that grows from 1 s to
    /// 30 s. A canary's failures don't hold back the active deployment's wakes.
    failed: Mutex<Option<(String, Instant, String, u32)>>,
    /// Crashes of the last ten minutes: when, when in unix ms, why.
    /// Recent crashes: when, unix ms, why, and the deployment the instance ran.
    crashes: Mutex<std::collections::VecDeque<(Instant, i64, String, String)>>,
    seq: AtomicU64,
    /// Every instance this process made here (a deploy's being checked isn't in `insts` until
    /// its commit): [`Self::adopt`] never takes one for a previous owner's.
    mine: Mutex<std::collections::HashSet<String>>,
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
    /// Each client's tokens, by bucket: 0 the environment's rate limit, 1 + i firewall rule i's.
    buckets: Mutex<HashMap<BucketKey, (f64, Instant)>>,
    /// CPU its awake instances used and could have used, per tick, for `cpu_window` (only what
    /// the runtime measures).
    cpu_samples: Mutex<std::collections::VecDeque<(Instant, f64, f64)>>,
    /// Its answers' times since the window began (`run.latency_target_ms`), and the last
    /// whole window's p95.
    lat_window: Mutex<(Instant, Served, Option<f64>)>,
    /// The same for memory: bytes in use and the instances' limits, per tick.
    mem_samples: Mutex<std::collections::VecDeque<(Instant, f64, f64)>>,
    /// The instances CPU last asked for: kept while the window fills again after a scale-out,
    /// so the new instance isn't taken straight back in.
    cpu_need: AtomicU32,
    last_tick: Mutex<Option<Instant>>,
    /// When each serving deployment's instances were last health-checked.
    last_health: Mutex<HashMap<String, Instant>>,
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
            released: Arc::default(),
            capped: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            held: AtomicBool::new(false),
            wanted: AtomicU32::new(0),
            backlog: AtomicU64::new(u64::MAX),
            scale_note: Mutex::new(String::new()),
            fewer_since: Mutex::new(None),
            canary: Mutex::new(None),
            routed: AtomicU64::new(0),
            served: Mutex::new(HashMap::new()),
            viewed: Mutex::new(HashMap::new()),
            draining: Default::default(),
            waiters: AtomicU32::new(0),
            freed: Arc::new(tokio::sync::Notify::new()),
            failed: Mutex::new(None),
            crashes: Mutex::new(std::collections::VecDeque::new()),
            seq: AtomicU64::new(0),
            mine: Mutex::default(),
            log,
            meter: Mutex::new(Meter::default()),
            over_since: Mutex::new(None),
            ticking: std::sync::atomic::AtomicBool::new(false),
            answering: std::sync::atomic::AtomicBool::new(false),
            gate: Mutex::new(None),
            gate_gen: AtomicU64::new(0),
            buckets: Mutex::new(HashMap::new()),
            cpu_samples: Mutex::new(std::collections::VecDeque::new()),
            lat_window: Mutex::new((Instant::now(), Served::default(), None)),
            mem_samples: Mutex::new(std::collections::VecDeque::new()),
            cpu_need: AtomicU32::new(0),
            last_tick: Mutex::new(None),
            last_health: Mutex::new(HashMap::new()),
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

    /// Keep instance `name` awake for `for_` from now (an hour's quarter at most; a longer hold it
    /// has stays): it's never suspended, put to sleep or scaled in meanwhile. When it ends, or
    /// `None` when it isn't one of its awake instances.
    pub fn hold_awake(&self, name: &str, for_: Duration) -> Option<Instant> {
        let insts = self.insts.lock();
        let i = insts.iter().find(|i| i.name == name && matches!(i.phase(), Phase::Running(_) | Phase::Suspended(_)))?;
        Some(i.hold(for_.min(Duration::from_secs(15 * 60))))
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
    pub(crate) fn serving(&self, deployment: &str) -> Option<Arc<Template>> {
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
    pub fn answered(&self, inst: &Inst, status: u16, ms: Option<f64>) {
        if let Some(ms) = ms {
            self.lat_window.lock().1.took(ms);
        }
        self.count(&inst.deployment, |s| {
            s.requests += 1;
            if status >= 500 {
                s.errors += 1;
            }
            if let Some(ms) = ms {
                s.took(ms);
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
        format!("{}{n}", instance_prefix(&self.service, &self.env, &tpl.deployment))
    }

    /// A cron run's or a job's container: `…-run<n>`, never one of the service's instances (a
    /// takeover never keeps it as one).
    async fn make_run(&self, tpl: &Template, name: Option<String>) -> anyhow::Result<Arc<Inst>> {
        anyhow::ensure!(!self.closed.load(Ordering::SeqCst), "this environment was torn down");
        let mut spec = tpl.spec.clone();
        spec.name = match name {
            Some(n) => n,
            None => {
                let n = self.seq.fetch_add(1, Ordering::SeqCst);
                format!("{}run{n}", instance_prefix(&self.service, &self.env, &tpl.deployment))
            }
        };
        spec.vars.push(("ROLL_INSTANCE".into(), spec.name.clone()));
        self.runtime.create(&spec).await?;
        Ok(Inst::of(spec.name, tpl))
    }

    async fn make(&self, tpl: &Template) -> anyhow::Result<Arc<Inst>> {
        anyhow::ensure!(!self.closed.load(Ordering::SeqCst), "this environment was torn down");
        let mut spec = tpl.spec.clone();
        spec.name = self.name_for(tpl);
        // Which instance a line or an error came from (Railway's `RAILWAY_REPLICA_ID`): its name,
        // as roll's own log lines name it.
        spec.vars.push(("ROLL_INSTANCE".into(), spec.name.clone()));
        self.mine.lock().insert(spec.name.clone());
        self.runtime.create(&spec).await?;
        Ok(Inst::of(spec.name, tpl))
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

    /// A gRPC server's health (`run.health_grpc`): a unary `grpc.health.v1.Health/Check` for
    /// `service` over h2c that answers 200, `grpc-status` 0 and SERVING (its body read to 1 KiB).
    async fn answers_grpc(&self, addr: SocketAddr, service: &str) -> bool {
        use http_body_util::BodyExt;
        // HealthCheckRequest { service = 1 }, in gRPC's length-prefixed frame (names are short:
        // one length byte).
        let mut msg = Vec::with_capacity(service.len() + 2);
        if !service.is_empty() {
            msg.extend([0x0a, service.len() as u8]);
            msg.extend(service.as_bytes());
        }
        let mut body = vec![0u8];
        body.extend((msg.len() as u32).to_be_bytes());
        body.extend(msg);
        let Ok(req) = axum::http::Request::post(format!("http://{addr}/grpc.health.v1.Health/Check"))
            .version(axum::http::Version::HTTP_2)
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .header("user-agent", "roll-apps-health")
            .body(axum::body::Body::from(body))
        else {
            return false;
        };
        let answer = async {
            let resp = self.client_h2.request(req).await.ok()?;
            if resp.status() != axum::http::StatusCode::OK {
                return None;
            }
            // Trailers-only (an error) puts the status in the headers.
            let early = resp.headers().get("grpc-status").cloned();
            let got = http_body_util::Limited::new(resp.into_body(), 1024).collect().await.ok()?;
            let status = got.trailers().and_then(|t| t.get("grpc-status").cloned()).or(early)?;
            Some((status, got.to_bytes()))
        };
        let Ok(Some((status, bytes))) = tokio::time::timeout(Duration::from_secs(5), answer).await else { return false };
        status == "0" && grpc_serving(&bytes)
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
        let (mut looked, mut hint) = (Instant::now(), None);
        match (run.kind, addr) {
            (RunType::Web, Some(a)) => loop {
                // A TCP service is up when its port accepts; a UDP one once it runs (nothing to
                // ask over UDP); anything else answers HTTP.
                let up = if run.udp {
                    true
                } else if run.tcp {
                    tcp_answers(a).await
                } else if let Some(svc) = run.health_grpc.as_deref() {
                    self.answers_grpc(a, svc).await
                } else {
                    self.answers(a, health, run.h2c).await
                };
                if up {
                    break;
                }
                // Not answering for a while: where it listens instead, if anywhere (the most common
                // first-deploy mistake). On its port but only on loopback can never be reached.
                if looked.elapsed() >= Duration::from_secs(2) {
                    looked = Instant::now();
                    match self.port_hint(&inst.name, run).await {
                        Some((why, true)) => {
                            self.crashed(inst, &why);
                            return Err(WakeError::Failed(why));
                        }
                        Some((why, false)) => hint = Some(why),
                        None => {}
                    }
                }
                if Instant::now() >= deadline {
                    self.crashed(inst, "it didn't answer in time");
                    return Err(match hint {
                        Some(h) => WakeError::Failed(format!("it didn't answer in time: {h}")),
                        None => WakeError::Timeout(self.cfg.wake_timeout),
                    });
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
        // Its other ports (custom domains' own) answer where the runtime published them.
        let mut found = HashMap::new();
        for p in inst.wanted.iter().copied() {
            if let Some(a) = self.runtime.port_address(&inst.name, p).await {
                found.insert(p, a);
            }
        }
        *inst.ports.lock() = found;
        *inst.phase.lock() = Phase::Running(addr);
        *inst.last_used.lock() = Instant::now();
        // Up: from here it runs within its CPU limit (it started with a boost).
        let (rt, name) = (self.runtime.clone(), inst.name.clone());
        tokio::spawn(async move { rt.settle(&name).await });
        Ok(addr)
    }

    /// Why `name` may not answer, from where it listens: (what to tell, whether it never can).
    async fn port_hint(&self, name: &str, run: &Run) -> Option<(String, bool)> {
        let port = run.port.unwrap_or(8080);
        // Docker's embedded DNS listens in every user-defined network: never the app's.
        let at: Vec<SocketAddr> = self.runtime.listeners(name).await?.into_iter().filter(|a| a.ip() != std::net::IpAddr::from([127, 0, 0, 11])).collect();
        if at.is_empty() || at.iter().any(|a| a.port() == port && !a.ip().is_loopback()) {
            return None;
        }
        if at.iter().any(|a| a.port() == port) {
            return Some((format!("the app listens on {} only, which nothing outside its container reaches: listen on 0.0.0.0:{port} (or [::]:{port})", at.iter().filter(|a| a.port() == port).map(|a| a.to_string()).collect::<Vec<_>>().join(", ")), true));
        }
        let list = at.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ");
        Some((format!("the app listens on {list}, not on port {port}: listen on $PORT ({port}), or set the service's port"), false))
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
        // Even the least busy is full: every instance is busy at max_instances (saturation).
        if best.0.in_flight.fetch_add(1, Ordering::SeqCst) >= concurrency {
            self.meter.lock().over_capacity(code_sql::now_ms());
        }
        Some(Lease { addr: best.1, inst: best.0.clone(), waited: Duration::ZERO, cold: false, h2c: false, timeout: None, affinity: false, freed: self.freed.clone() })
    }

    /// The instance a visitor's affinity cookie names, if it's up with room.
    fn pick_affine(&self, deployment: &str, concurrency: u32, key: &str) -> Option<Lease> {
        let insts = self.insts.lock();
        let (i, a) = insts.iter().filter(|i| i.deployment == deployment && i.in_flight() < concurrency && affinity_key(&i.name) == key).find_map(|i| match i.phase() {
            Phase::Running(Some(a)) => Some((i, a)),
            _ => None,
        })?;
        i.in_flight.fetch_add(1, Ordering::SeqCst);
        Some(Lease { addr: a, inst: i.clone(), waited: Duration::ZERO, cold: false, h2c: false, timeout: None, affinity: false, freed: self.freed.clone() })
    }

    /// Take an idle instance out of rotation before stopping or suspending it: under the lock
    /// [`Self::pick`] takes, so no request is handed to it in between. Its phase before, if it
    /// was up and had nothing in flight.
    fn claim_idle(&self, i: &Inst) -> Option<Phase> {
        let _picking = self.insts.lock();
        let mut phase = i.phase.lock();
        if i.in_flight() > 0 || i.held() || !matches!(*phase, Phase::Running(_) | Phase::Suspended(_)) {
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
        if self.paused.load(Ordering::SeqCst) {
            return Err(WakeError::Paused);
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
        // The hard limit: at max_instances, wait for a slot rather than go past concurrency.
        if let Some(q) = run.queue_s
            && self.running_count(dep) > 0
            && self.awake_of(dep) as u32 >= run.max_instances
        {
            return self.queued(dep, run.concurrency, Duration::from_secs(q as u64), began).await;
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

    /// Wait up to `wait` for an instance of `dep` with room under `concurrency`; counted over
    /// capacity when none comes (`alert.capacity` reads it).
    async fn queued(&self, dep: &str, concurrency: u32, wait: Duration, began: Instant) -> Result<Lease, WakeError> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            // Listening before looking: a slot freed in between still wakes it.
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            if let Some(mut l) = self.pick(dep, concurrency, false) {
                l.waited = began.elapsed();
                return Ok(l);
            }
            if tokio::time::timeout_at(deadline, freed).await.is_err() {
                self.meter.lock().over_capacity(now_ms());
                return Err(WakeError::Full(wait));
            }
        }
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
        self.take_token_in(0, client, None, limit)
    }

    /// [`Self::take_token`] in one of its buckets (a firewall rule's own: 1 + its index).
    pub fn take_token_in(&self, bucket: u16, client: Option<std::net::IpAddr>, keyed: Option<u64>, limit: &crate::model::RateLimit) -> Result<(), Duration> {
        let (rate, burst) = (limit.per_minute as f64 / 60.0, limit.burst() as f64);
        // An IPv6 client counts by its /64: one host has the whole of it to rotate through.
        let client = client.map(|ip| match ip {
            std::net::IpAddr::V6(v6) => std::net::IpAddr::V6(std::net::Ipv6Addr::from(u128::from(v6) & !((1u128 << 64) - 1))),
            v4 => v4,
        });
        let now = Instant::now();
        // Keyed: the key's value is the client, wherever it comes from.
        let key = match keyed {
            Some(k) => (bucket, None, Some(k)),
            None => (bucket, client, None),
        };
        let mut b = self.buckets.lock();
        if b.len() >= MAX_BUCKETS && !b.contains_key(&key) {
            // Full ones first: they're the same as no entry. Then the longest unseen, never
            // everyone's (a flood of new addresses mustn't reset the limited ones).
            b.retain(|_, (tokens, at)| *tokens + now.duration_since(*at).as_secs_f64() * rate < burst);
            // Still full: the longest unseen tenth goes at once (one pass, not one per client).
            if b.len() >= MAX_BUCKETS {
                let mut ats: Vec<Instant> = b.values().map(|(_, at)| *at).collect();
                let cut = ats.len() / 10;
                let (_, nth, _) = ats.select_nth_unstable(cut);
                let nth = *nth;
                b.retain(|_, (_, at)| *at > nth);
            }
        }
        let (tokens, at) = b.entry(key).or_insert((burst, now));
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
        let (runtime, name, released) = (self.runtime.clone(), inst.name.clone(), self.released.clone());
        *inst.stopping.lock() = Some(tokio::spawn(async move {
            if !released.load(Ordering::SeqCst) {
                let _ = runtime.stop(&name, grace).await;
            }
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
    /// ones keep serving meanwhile, except with a volume (one writer) or `rollout.overlap: false`
    /// (one runner): then the old ones stop first, draining for `drain_s`, and the environment
    /// stays locked until [`EnvLive::commit`], so no request starts an old instance in between.
    /// On failure nothing of the new deployment is left.
    pub async fn prepare(self: &Arc<Self>, tpl: &Arc<Template>, health_path: Option<&str>) -> anyhow::Result<Prepared> {
        let run = &tpl.run;
        let was_awake = self.awake_count() as u32;
        let n = match run.kind {
            RunType::Cron => 0,
            // A worker following a backlog starts at what it wants, not back at its floor; one
            // that scales to zero still starts one, so the deploy sees it boot (it goes once
            // it's caught up for `scale_down_after`).
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
        if run.volume.is_some() || !run.rollout.overlap {
            // One volume, or one runner: the old ones let go first, finishing their work.
            hold = Some(self.wake.clone().lock_owned().await);
            let old: Vec<Arc<Inst>> = self.insts.lock().drain(..).collect();
            if !old.is_empty() {
                self.log.stream(Stream::System).line(format!("stopping {} old instance{} before the new one starts", old.len(), if old.len() == 1 { "" } else { "s" }));
            }
            let drain = self.drain_of(run);
            for i in old {
                let _ = self.runtime.stop(&i.name, drain).await;
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
            let (runtime, released, draining) = (self.runtime.clone(), self.released.clone(), self.draining.clone());
            draining.lock().insert(i.name.clone(), i.deployment.clone());
            tokio::spawn(async move {
                let until = Instant::now() + window;
                while i.in_flight() > 0 && Instant::now() < until {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                if released.load(Ordering::SeqCst) {
                    // The next owner's to stop: this node only forgets it.
                    runtime.release(&i.name).await;
                } else {
                    let grace = until.saturating_duration_since(Instant::now()).max(Duration::from_secs(1));
                    let _ = runtime.stop(&i.name, grace).await;
                    runtime.remove(&i.name).await;
                }
                draining.lock().remove(&i.name);
            });
        }
    }

    /// The deployments whose instances are still draining here.
    pub fn draining(&self) -> Vec<String> {
        self.draining.lock().values().cloned().collect()
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
        if tpl.site.is_some() {
            return;
        }
        // The live deployment's instances the previous owner ran, still running on the shared
        // runtime (a node died, its containers or tasks didn't): kept, so its death costs
        // visitors no cold start; one that can't be kept goes.
        let kept = self.keep_running(&tpl).await;
        if kept == 0
            && tpl.run.kind == RunType::Web
            && self.insts.lock().is_empty()
            && let Ok(i) = self.make(&tpl).await
        {
            self.insts.lock().push(i);
        }
    }

    /// [`Self::adopt`]'s instances still running: how many it kept. Only a web service's or a
    /// worker's (a cron run or a job is named `…-run<n>` and never kept); those already here
    /// stay as they are; the rest of the deployment's go.
    async fn keep_running(&self, tpl: &Template) -> usize {
        let prefix = instance_prefix(&self.service, &self.env, &tpl.deployment);
        let here: Vec<String> = self.insts.lock().iter().map(|i| i.name.clone()).collect();
        let mut kept = 0;
        let listed = self.runtime.list(Some(&self.service)).await;
        // Only names that still exist need remembering.
        self.mine.lock().retain(|n| listed.contains(n));
        for name in listed {
            let Some(n) = name.strip_prefix(&prefix).and_then(|n| n.parse::<u64>().ok()) else { continue };
            // New names after every one seen, kept or not (the old owner may still be removing it).
            self.seq.fetch_max(n + 1, Ordering::SeqCst);
            if here.contains(&name) || self.mine.lock().contains(&name) {
                continue;
            }
            if !matches!(tpl.run.kind, RunType::Web | RunType::Worker) {
                self.runtime.remove(&name).await;
                continue;
            }
            let mut spec = tpl.spec.clone();
            spec.name = name.clone();
            let found = self.runtime.adopt(&spec, self.log.instance(&name, &tpl.deployment)).await;
            let phase = match (tpl.run.kind, found) {
                (_, None) => None,
                // Asleep and frozen: kept so, nothing asked of it (a frozen process answers nothing).
                (_, Some(a)) if a.paused => Some(Phase::Suspended(a.addr)),
                (RunType::Web, Some(Adopted { addr: Some(a), .. })) => {
                    let up = if tpl.run.udp { true } else if tpl.run.tcp { tcp_answers(a).await } else { self.answers(a, None, tpl.run.h2c).await };
                    up.then_some(Phase::Running(Some(a)))
                }
                (RunType::Web, Some(_)) => None,
                (_, Some(a)) => Some(Phase::Running(a.addr)),
            };
            let Some(phase) = phase else {
                // Not one to keep (stopped, or it doesn't answer): it goes, as the rest did.
                self.runtime.remove(&name).await;
                continue;
            };
            let i = Inst::of(name.clone(), tpl);
            let mut found = HashMap::new();
            for p in i.wanted.iter().copied() {
                if let Some(a) = self.runtime.port_address(&name, p).await {
                    found.insert(p, a);
                }
            }
            *i.ports.lock() = found;
            *i.phase.lock() = phase;
            *i.started_at.lock() = Some(now_ms());
            self.insts.lock().push(i);
            kept += 1;
        }
        if kept > 0 {
            self.log.stream(Stream::System).line(format!("kept {kept} instance{} the previous owner ran", if kept == 1 { "" } else { "s" }));
        }
        kept
    }

    pub fn set_capped(&self, on: bool) {
        self.capped.store(on, Ordering::SeqCst);
    }

    pub fn set_paused(&self, on: bool) {
        self.paused.store(on, Ordering::SeqCst);
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// A scaling worker's instances for its backlog now.
    /// Its backlog couldn't be read: unknown until it can, so a worker that may scale to zero
    /// keeps one meanwhile (what it wanted stays).
    pub fn forget_backlog(&self) {
        self.backlog.store(u64::MAX, Ordering::SeqCst);
    }

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

    /// Replace one instance (a stuck one, its pool leaked). A web service's, of the live
    /// deployment, without a volume and below `max_instances`: a fresh one is started (outside
    /// the wake lock, as a deploy's are) and answering first, then the two swap under the lock and
    /// this one drains. Otherwise (a volume, a worker, an old deployment's or a canary's, at its
    /// most) it stops first, as a volume rollout does, and the reaper or the next request starts
    /// what's wanted. If the fresh one doesn't start, this one stays.
    pub async fn restart_instance(self: &Arc<Self>, name: &str) -> Result<(), String> {
        self.may_wake().map_err(|e| e.to_string())?;
        let (tpl, inst, replace) = {
            let _g = self.wake.lock().await;
            let tpl = self.template().ok_or("nothing is deployed here")?;
            let inst = self.instances().into_iter().find(|i| i.name == name).ok_or_else(|| format!("no instance {name} here"))?;
            let replace = tpl.run.kind == RunType::Web
                && tpl.run.volume.is_none()
                && inst.awake()
                && inst.deployment == tpl.deployment
                && self.canary.lock().is_none()
                && (self.awake_of(&tpl.deployment) as u32) < tpl.run.max_instances;
            if !replace {
                self.insts.lock().retain(|i| !Arc::ptr_eq(i, &inst));
                *inst.phase.lock() = Phase::Stopping;
            }
            (tpl, inst, replace)
        };
        if !replace {
            self.log.stream(Stream::System).line(format!("restarting {name}: it stops first"));
            let _ = self.runtime.stop(&inst.name, self.drain_of(&tpl.run)).await;
            self.runtime.remove(&inst.name).await;
            self.tick().await;
            return Ok(());
        }
        let fresh = self.make(&tpl).await.map_err(|e| format!("a fresh instance couldn't be made: {e:#}"))?;
        let deadline = Instant::now() + Duration::from_secs(tpl.run.health_timeout_s as u64);
        if let Err(e) = self.start(&fresh, &tpl.run, tpl.run.health_path.as_deref(), deadline).await {
            self.discard(&[fresh]).await;
            return Err(format!("the fresh instance didn't start ({e}); {name} keeps serving"));
        }
        {
            let _g = self.wake.lock().await;
            // A deploy, a scale-in or a teardown meanwhile: the fresh one isn't wanted.
            let still = !self.closed.load(Ordering::SeqCst) && self.template().is_some_and(|t| t.deployment == tpl.deployment) && self.insts.lock().iter().any(|i| Arc::ptr_eq(i, &inst));
            if !still {
                drop(_g);
                self.discard(&[fresh]).await;
                return Err(format!("{name} changed meanwhile (a deploy, a scale-in): nothing was restarted"));
            }
            let mut insts = self.insts.lock();
            insts.retain(|i| !Arc::ptr_eq(i, &inst));
            insts.push(fresh);
            *inst.phase.lock() = Phase::Stopping;
        }
        self.log.stream(Stream::System).line(format!("restarting {name}: a fresh instance answers in its place"));
        self.drain(vec![inst], self.drain_of(&tpl.run));
        Ok(())
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
    /// Give the environment up without touching what runs (a runtime the nodes share: the next
    /// owner keeps the live deployment's instances, [`Self::adopt`], and removes the rest):
    /// nothing here sends to, wakes or stops them any more.
    pub async fn let_go(&self) {
        self.released.store(true, Ordering::SeqCst);
        let _g = self.wake.lock().await;
        self.closed.store(true, Ordering::SeqCst);
        *self.canary.lock() = None;
        self.viewed.lock().clear();
        let all: Vec<Arc<Inst>> = self.insts.lock().drain(..).collect();
        for i in all {
            self.runtime.release(&i.name).await;
        }
        *self.template.lock() = None;
    }

    /// A file instances made from now on get (`path`, its contents), in place of one there: a
    /// closed environment's trust bundle when its interception CA changes while it sleeps.
    /// Its template made again while nothing runs (a closed environment's new CA and placeholders
    /// while it slept): its next start uses it.
    pub fn set_template(&self, tpl: Arc<Template>) {
        *self.template.lock() = Some(tpl);
    }

    pub fn set_file(&self, path: &str, contents: &str) {
        let with = |t: &Arc<Template>| {
            let mut t = (**t).clone();
            t.spec.files.retain(|(p, _)| p != path);
            t.spec.files.push((path.to_string(), contents.to_string()));
            Arc::new(t)
        };
        let mut tpl = self.template.lock();
        if let Some(t) = tpl.as_ref() {
            *tpl = Some(with(t));
        }
        if let Some((t, _)) = self.canary.lock().as_mut() {
            *t = with(t);
        }
    }

    /// The service's registry login changed (None: it went): instances made from now on use it.
    pub fn set_registry(&self, login: Option<&crate::runtime::RegistryLogin>) {
        let strip = |t: &Arc<Template>| {
            let mut t = (**t).clone();
            t.spec.registry = login.cloned();
            Arc::new(t)
        };
        let mut tpl = self.template.lock();
        if let Some(t) = tpl.as_ref() {
            *tpl = Some(strip(t));
        }
        if let Some((t, _)) = self.canary.lock().as_mut() {
            *t = strip(t);
        }
    }

    /// Give the environment up on a runtime this node alone runs (its service moved): nothing new
    /// comes here, and its instances stop once their requests in flight end (the drain window),
    /// not under them.
    pub async fn retire(&self) {
        let all = {
            let _g = self.wake.lock().await;
            self.closed.store(true, Ordering::SeqCst);
            *self.canary.lock() = None;
            self.viewed.lock().clear();
            let all: Vec<Arc<Inst>> = self.insts.lock().drain(..).collect();
            let run = self.template.lock().take().map(|t| t.run.clone());
            (all, run)
        };
        let (insts, run) = all;
        let window = run.as_ref().map_or(self.cfg.drain, |r| self.drain_of(r));
        self.drain(insts, window);
    }

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
        // Each deployment's instances by its own health path and interval (a canary's may differ).
        let serving: Vec<Arc<Template>> = self.template().into_iter().chain(self.canary.lock().as_ref().map(|(t, _)| t.clone())).collect();
        let due: Vec<&Arc<Template>> = {
            let mut last = self.last_health.lock();
            last.retain(|d, _| serving.iter().any(|t| &t.deployment == d));
            serving
                .iter()
                .filter(|t| {
                    let every = t.run.health_every_s.map_or(self.cfg.health_every, |s| Duration::from_secs(s as u64));
                    let due = last.get(&t.deployment).is_none_or(|at| now.duration_since(*at) >= every);
                    if due {
                        last.insert(t.deployment.clone(), now);
                    }
                    due
                })
                .collect()
        };
        for tpl in due {
            if tpl.run.kind == RunType::Web
                && let Some(path) = tpl.run.health_path.as_deref().or(tpl.run.health_grpc.as_ref().map(|_| GRPC_HEALTH))
            {
                let mine: Vec<Arc<Inst>> = insts.iter().filter(|i| i.deployment == tpl.deployment).cloned().collect();
                self.check_health(&mine, path, &tpl.run).await;
            }
        }
        if self.held.load(Ordering::SeqCst) {
            return;
        }
        if self.capped.load(Ordering::SeqCst) || self.paused.load(Ordering::SeqCst) {
            // Capped or paused: stop what still runs, keep no floor.
            if insts.iter().any(|i| i.awake()) {
                self.log.stream(Stream::System).line(if self.capped.load(Ordering::SeqCst) { "the project reached its spending limit: stopped" } else { "paused: stopped" });
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
        // What was measured, and what those instances could have used (`run.cpu` each), and
        // their memory against its limits.
        let (mut used, mut capacity) = (0.0, 0.0);
        let (mut mem_used, mut mem_cap) = (0.0, 0.0);
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
                        i.record(now_ms() / 60_000, d, elapsed, u.memory_bytes);
                        cpu += d;
                        // Its sidecars' use is the instance's (usage adds it): so is their size.
                        if let Some(tpl) = self.serving(&i.deployment) {
                            used += d;
                            let size = tpl.run.cpu + tpl.spec.sidecars_size().0;
                            capacity += size * elapsed;
                            // Busy on its own (`run.busy_cpu`): held awake as a request would.
                            if let Some(b) = tpl.run.busy_cpu.filter(|_| elapsed > 0.0 && size > 0.0) {
                                let pct = d / elapsed / size * 100.0;
                                if pct >= b as f64 {
                                    let was = i.held();
                                    i.hold(BUSY_HOLD);
                                    if !was {
                                        self.log.stream(Stream::System).line(format!("{} kept awake: CPU at {pct:.0}%, over its {b}%", i.name));
                                    }
                                }
                            }
                        }
                    }
                    *seen = Some(u.cpu_ns);
                    mem += u.memory_bytes;
                    if let Some(tpl) = self.serving(&i.deployment) {
                        mem_used += u.memory_bytes as f64;
                        mem_cap += (((tpl.run.memory_mb + tpl.spec.sidecars_size().1) as u64) << 20) as f64;
                    }
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
        for (samples, used, cap) in [(&self.cpu_samples, used, capacity), (&self.mem_samples, mem_used, mem_cap)] {
            let mut s = samples.lock();
            if cap > 0.0 {
                s.push_back((now, used, cap));
            }
            while s.front().is_some_and(|(at, ..)| now.duration_since(*at) > self.cfg.cpu_window) {
                s.pop_front();
            }
        }
        insts
    }

    /// Its instances' last hour by minute (those it has now, that were measured), by name.
    pub fn by_instance(&self) -> Vec<InstanceSeries> {
        let mut v: Vec<InstanceSeries> = self.instances().iter().filter_map(|i| i.series()).collect();
        v.sort_by(|a, b| a.instance.cmp(&b.instance));
        v
    }

    /// Its awake instances' CPU over the last `cpu_window`, in percent of what they could use;
    /// None until the window is nearly full of measurements.
    fn cpu_load(&self) -> Option<f64> {
        self.load(&self.cpu_samples)
    }

    /// Its answers' p95 over the last whole window (at least 20 answers timed), ms: a window
    /// closes each `cpu_window`, and a scale-out starts a fresh one.
    fn latency_p95(&self) -> Option<f64> {
        let mut w = self.lat_window.lock();
        if w.0.elapsed() >= self.cfg.cpu_window {
            let timed: u64 = w.1.lat.iter().map(|&c| c as u64).sum();
            w.2 = (timed >= 20).then(|| w.1.p95_ms()).flatten();
            *w = (Instant::now(), Served::default(), w.2);
        }
        w.2
    }

    /// Its awake instances' memory over the same window, in percent of their limits.
    fn mem_load(&self) -> Option<f64> {
        self.load(&self.mem_samples)
    }

    fn load(&self, samples: &Mutex<std::collections::VecDeque<(Instant, f64, f64)>>) -> Option<f64> {
        let s = samples.lock();
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
    async fn check_health(&self, insts: &[Arc<Inst>], path: &str, run: &Run) {
        let (concurrency, h2c) = (run.concurrency, run.h2c);
        let failures = run.health_failures.unwrap_or(self.cfg.health_failures);
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
        let grpc = run.health_grpc.as_deref();
        let probes = running.iter().map(|(i, a)| async move {
            let ok = match grpc {
                Some(svc) => self.answers_grpc(*a, svc).await,
                None => self.answers(*a, Some(path), h2c).await,
            };
            (*i, *a, ok)
        });
        for (i, addr, ok) in futures::future::join_all(probes).await {
            if ok {
                i.health_fails.store(0, Ordering::SeqCst);
                continue;
            }
            let n = i.health_fails.fetch_add(1, Ordering::SeqCst) + 1;
            self.log.stream(Stream::System).line(format!("{}: health check {path} failed ({n} of {failures})", i.name));
            // Still the instance that was checked (not stopped or replaced meanwhile).
            if n >= failures && i.phase() == Phase::Running(Some(addr)) {
                // Out of rotation now; what it's serving gets the drain to finish.
                self.crashed_with(i, &format!("its health check {path} failed {n} times in a row"), self.cfg.drain);
            }
        }
    }

    async fn reap_web(self: &Arc<Self>, tpl: &Arc<Template>) {
        // Its floor as its schedule has it now.
        let scheduled = tpl.run.at(code_sql::now_ms());
        let run = &scheduled;
        // A deployment's own URL's instances are reap_viewed's, not part of the scale.
        let insts: Vec<Arc<Inst>> = self.instances().into_iter().filter(|i| !self.only_viewed(&i.deployment)).collect();
        let suspend_after = Duration::from_secs(run.suspend_after_s as u64);
        let sleep_after = Duration::from_secs(run.sleep_after_s as u64);
        let mut awake = insts.iter().filter(|i| i.awake()).count() as u32;
        // Fewer than needed by the requests in flight for a while → one goes.
        let in_flight: u32 = insts.iter().map(|i| i.in_flight()).sum();
        let mut needed = in_flight.div_ceil(run.concurrency).max(run.min_instances).max(1);
        // On CPU and memory too: as many as would bring the average under the target (the one
        // that asks for more decides).
        let by = |target: Option<u32>, load: Option<f64>, what: &'static str| -> Option<(u32, f64, u32, &'static str)> {
            let (target, load) = (target?, load.filter(|_| awake > 0)?);
            Some((((awake as f64 * load / target as f64).ceil() as u32).min(run.max_instances.max(1)), load, target, what))
        };
        let wanted = [by(run.cpu_target, self.cpu_load(), "CPU"), by(run.memory_target, self.mem_load(), "memory"), by(run.latency_target_ms, run.latency_target_ms.and_then(|_| self.latency_p95()), "p95")].into_iter().flatten().max_by_key(|(n, ..)| *n);
        if let Some((by_cpu, load, target, what)) = wanted {
            self.cpu_need.store(by_cpu, Ordering::SeqCst);
            if by_cpu > awake && awake < run.max_instances {
                // One more at a time, then a fresh window to judge it by.
                if let Ok(inst) = self.make(tpl).await {
                    self.insts.lock().push(inst.clone());
                    let deadline = Instant::now() + self.cfg.wake_timeout;
                    if self.start(&inst, run, None, deadline).await.is_ok() {
                        awake += 1;
                        self.cpu_samples.lock().clear();
                        self.mem_samples.lock().clear();
                        *self.lat_window.lock() = (Instant::now(), Served::default(), None);
                        self.cpu_need.store(awake, Ordering::SeqCst);
                        let at = if what == "p95" { format!("p95 at {load:.0} ms, over its {target} ms target") } else { format!("{what} at {load:.0}% of the {target}% target") };
                        self.log.stream(Stream::System).line(format!("scaled out: {at}, {awake} instances"));
                    }
                }
            }
            needed = needed.max(by_cpu);
        } else if run.cpu_target.is_some() || run.memory_target.is_some() || run.latency_target_ms.is_some() {
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
            && let Some(i) = insts.iter().filter(|i| matches!(i.phase(), Phase::Running(_)) && i.in_flight() == 0 && !i.held()).min_by_key(|i| *i.last_used.lock())
            && self.claim_idle(i).is_some()
        {
            // SIGTERM, then the service's own drain to flush what it holds (`drain_s`).
            let _ = self.runtime.stop(&i.name, self.drain_of(run)).await;
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
            if i.in_flight() > 0 || i.held() {
                continue;
            }
            let idle = i.last_used.lock().elapsed();
            match i.phase() {
                Phase::Running(addr) if awake > run.min_instances => {
                    if idle >= sleep_after || (!self.runtime.can_suspend() && idle >= suspend_after.max(sleep_after)) {
                        if self.claim_idle(i).is_some() {
                            let _ = self.runtime.stop(&i.name, self.drain_of(run)).await;
                            *i.phase.lock() = Phase::Created;
                            awake -= 1;
                        }
                    } else if self.runtime.can_suspend() && run.suspend_after_s > 0 && idle >= suspend_after && self.claim_idle(i).is_some() {
                        let suspended = self.runtime.suspend(&i.name).await.is_ok();
                        *i.phase.lock() = if suspended { Phase::Suspended(addr) } else { Phase::Running(addr) };
                    }
                }
                Phase::Suspended(_) if idle >= sleep_after && awake > run.min_instances && self.claim_idle(i).is_some() => {
                    let _ = self.runtime.stop(&i.name, self.drain_of(run)).await;
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
        let scheduled = tpl.run.at(code_sql::now_ms());
        let run = &scheduled;
        // A worker that follows its work runs what its backlog wants; else its floor. One that
        // may scale to zero keeps one while its backlog can't be read: nothing it can't see
        // leaves its work undone.
        let target = match &run.scale {
            Some(_) => {
                let floor = if self.backlog.load(Ordering::SeqCst) == u64::MAX { run.min_instances.max(1) } else { run.min_instances };
                self.wanted.load(Ordering::SeqCst).clamp(floor, run.max_instances.max(floor))
            }
            None => run.min_instances,
        };
        let insts = self.instances();
        let mut up = 0;
        for i in &insts {
            match i.phase() {
                // Up but failing its health command (a consumer that hung): replaced as a crash.
                Phase::Running(_) if tpl.spec.health.is_some() && self.runtime.unhealthy(&i.name).await => {
                    let h = tpl.spec.health.as_ref().expect("checked");
                    let why = format!("its health command failed {} times in a row", h.failures);
                    self.log.stream(Stream::System).line(format!("{}: {why}", i.name));
                    self.crashed(i, &why);
                    self.after_stop(i, run, false, &why);
                }
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
        // Left stopped by the policy: they count, so nothing replaces them. So do crashed ones
        // waiting out their backoff: a new instance in their place would skip it (and run beside
        // the crashed one once it starts again).
        let finished = insts.iter().filter(|i| i.done.load(Ordering::SeqCst)).count() as u32;
        let mut waiting = 0;
        for i in insts.iter().filter(|i| i.phase() == Phase::Crashed && !i.done.load(Ordering::SeqCst)) {
            if up + finished >= target {
                break;
            }
            // Back off: `restart_backoff` (1 s) doubling to 60 times it since it last ran.
            let crashes = i.crashes.load(Ordering::SeqCst).min(6);
            let wait = (self.cfg.restart_backoff * (1u32 << crashes)).min(self.cfg.restart_backoff * 60);
            if i.last_used.lock().elapsed() < wait {
                waiting += 1;
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
        while up + finished + waiting < target {
            let Ok(inst) = self.make(tpl).await else { break };
            self.insts.lock().push(inst.clone());
            let deadline = Instant::now() + self.cfg.wake_timeout;
            if self.start(&inst, run, None, deadline).await.is_err() {
                break;
            }
            up += 1;
            self.log.stream(Stream::System).line(format!("scaled out: {up} instances for the backlog"));
        }
        // Crashed ones past what's wanted (its backlog fell while they waited): they go, their
        // crash kept in the crash list; none is started again to be scaled in.
        if up + finished >= target {
            let extra: Vec<Arc<Inst>> = self.instances().into_iter().filter(|i| i.phase() == Phase::Crashed && !i.done.load(Ordering::SeqCst)).collect();
            for i in extra {
                self.insts.lock().retain(|x| !Arc::ptr_eq(x, &i));
                let (runtime, name, stopping, released) = (self.runtime.clone(), i.name.clone(), i.stopping.lock().take(), self.released.clone());
                tokio::spawn(async move {
                    if let Some(s) = stopping {
                        let _ = s.await;
                    }
                    if !released.load(Ordering::SeqCst) {
                        runtime.remove(&name).await;
                    }
                });
            }
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
            let (runtime, name, grace, released) = (self.runtime.clone(), i.name.clone(), self.drain_of(run), self.released.clone());
            tokio::spawn(async move {
                if released.load(Ordering::SeqCst) {
                    return;
                }
                let _ = runtime.stop(&name, grace).await;
                runtime.remove(&name).await;
            });
        }
    }

    /// One cron run: a fresh instance of the active deployment, to its exit or `timeout`.
    pub async fn run_once(self: &Arc<Self>, timeout: Duration) -> anyhow::Result<(String, Option<i32>, bool)> {
        let e = self.run_to_exit(None, &[], timeout, None, tokio_util::sync::CancellationToken::new(), None, None).await?;
        Ok((e.deployment, e.code, e.timed_out))
    }

    /// A fresh instance of the active deployment run to its exit, `timeout` or `cancel`: a cron
    /// run, or a one-off job (`command` in place of the image's, with no port and no volume,
    /// since the service's own instance may be using it). `made` gets the container's name
    /// before it starts; the container is removed after, whatever happened.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_to_exit(
        self: &Arc<Self>,
        command: Option<&str>,
        vars: &[(String, String)],
        timeout: Duration,
        log: Option<LogSink>,
        cancel: tokio_util::sync::CancellationToken,
        name: Option<String>,
        size: Option<(f64, u32)>,
    ) -> anyhow::Result<Exit> {
        let mut tpl = (*self.template().ok_or_else(|| anyhow::anyhow!("no deployment"))?).clone();
        // A job's own size (Heroku's `run --size`): its container's, and what it's metered at.
        if let Some((cpu, mb)) = size {
            (tpl.spec.cpu, tpl.spec.memory_mb, tpl.run.cpu, tpl.run.memory_mb) = (cpu, mb, cpu, mb);
        }
        anyhow::ensure!(command.is_some() || tpl.spec.volume.is_none() || !self.held.load(Ordering::SeqCst), "its volume is being restored");
        if let Some(cmd) = command {
            tpl.spec.command = Some(cmd.to_string());
            tpl.spec.port = None;
            tpl.spec.volume = None;
            tpl.spec.aliases.clear();
        }
        tpl.spec.vars.extend(vars.iter().cloned());
        self.run_template(tpl, name, log, timeout, cancel, None).await
    }

    /// A browser check's container (`crate::browser`): roll's checker `image` on the live
    /// deployment's network with `vars` only (none of the app's), no port, volume, sidecar or
    /// name of its own there, 1 vCPU and 1 GiB, to its exit or `timeout`; metered to the service.
    /// What it left at `keep` comes back in [`Exit::kept`].
    #[allow(clippy::too_many_arguments)]
    pub async fn run_checker(self: &Arc<Self>, image: &str, command: &str, vars: Vec<(String, String)>, timeout: Duration, log: LogSink, keep: &str, cancel: tokio_util::sync::CancellationToken) -> anyhow::Result<Exit> {
        let mut tpl = (*self.template().ok_or_else(|| anyhow::anyhow!("no deployment"))?).clone();
        let s = &mut tpl.spec;
        (s.image, s.command, s.vars, s.port, s.volume, s.idle_stop_s, s.registry, s.health) = (image.to_string(), Some(command.to_string()), vars, None, None, None, None, None);
        s.ports.clear();
        s.aliases.clear();
        s.files.clear();
        s.sidecars.clear();
        (s.cpu, s.memory_mb, tpl.run.cpu, tpl.run.memory_mb) = (1.0, 1024, 1.0, 1024);
        self.run_template(tpl, None, Some(log), timeout, cancel, Some(keep)).await
    }

    async fn run_template(self: &Arc<Self>, tpl: Template, name: Option<String>, log: Option<LogSink>, timeout: Duration, cancel: tokio_util::sync::CancellationToken, keep: Option<&str>) -> anyhow::Result<Exit> {
        let inst = self.make_run(&tpl, name).await?;
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
        // With its sidecars' size: they run beside it.
        let (side_cpu, side_mb) = tpl.spec.sidecars_size();
        self.meter.lock().sample(now_ms(), secs * (tpl.run.cpu + side_cpu) * 0.5, ((tpl.run.memory_mb + side_mb) as u64) << 20, secs, 1);
        // What it was asked to keep, from the stopped container, bounded.
        let kept = match keep {
            Some(path) => match self.runtime.copy_from(&inst.name, path).await {
                Ok(stream) => {
                    use tokio::io::AsyncReadExt;
                    let mut buf = Vec::new();
                    match stream.take(MAX_KEPT + 1).read_to_end(&mut buf).await {
                        Ok(_) if buf.len() as u64 <= MAX_KEPT => Some(buf),
                        _ => None,
                    }
                }
                Err(_) => None,
            },
            None => None,
        };
        self.runtime.remove(&inst.name).await;
        let (code, timed_out, canceled) = ran?;
        Ok(Exit { deployment: tpl.deployment.clone(), container: inst.name.clone(), code, timed_out, canceled, kept })
    }
}

/// A one-off console's container ([`EnvLive::start_console`]): what to meter and remove at its end.
pub struct ConsoleBox {
    pub name: String,
    began: Instant,
    cpu: f64,
    memory_mb: u32,
}

impl EnvLive {
    /// A one-off console (`roll apps run -it`, Heroku's `heroku run bash`): a fresh container of
    /// the live deployment, with its variables and on its network, no port and no volume, its own
    /// `cpu` and `memory_mb` when asked; it waits (`sleep`) for the terminal opened in it.
    pub async fn start_console(self: &Arc<Self>, of: Option<Arc<Template>>, cpu: Option<f64>, memory_mb: Option<u32>) -> anyhow::Result<ConsoleBox> {
        let mut tpl = (*of.or_else(|| self.template()).ok_or_else(|| anyhow::anyhow!("no deployment"))?).clone();
        // Kept up while the terminal is open (a day at most: the shell's idle limit ends it first).
        tpl.spec.command = Some("exec sleep 86400".into());
        tpl.spec.port = None;
        tpl.spec.ports.clear();
        tpl.spec.volume = None;
        tpl.spec.aliases.clear();
        tpl.spec.health = None;
        if let Some(c) = cpu {
            tpl.spec.cpu = c;
            tpl.run.cpu = c;
        }
        if let Some(m) = memory_mb {
            tpl.spec.memory_mb = m;
            tpl.run.memory_mb = m;
        }
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let name = format!("{}console{n}", instance_prefix(&self.service, &self.env, &tpl.deployment));
        let inst = self.make_run(&tpl, Some(name)).await?;
        let log = self.log.instance(&inst.name, &inst.deployment);
        if let Err(e) = self.runtime.start(&inst.name, log).await {
            self.runtime.remove(&inst.name).await;
            return Err(e);
        }
        self.runtime.settle(&inst.name).await;
        // Metered with its sidecars, which run beside it.
        let (side_cpu, side_mb) = tpl.spec.sidecars_size();
        Ok(ConsoleBox { name: inst.name.clone(), began: Instant::now(), cpu: tpl.run.cpu + side_cpu, memory_mb: tpl.run.memory_mb + side_mb })
    }

    /// A console ended: its container stops and goes, metered at its allocation as a run is.
    pub async fn end_console(&self, c: ConsoleBox) {
        let _ = self.runtime.stop(&c.name, Duration::from_secs(5)).await;
        let secs = c.began.elapsed().as_secs_f64();
        self.meter.lock().sample(now_ms(), secs * c.cpu * 0.5, (c.memory_mb as u64) << 20, secs, 1);
        self.runtime.remove(&c.name).await;
    }

    /// A sandbox's container ([`crate::sandboxes`]) made from `tpl` as `name` and started; gone
    /// again if it didn't start.
    pub(crate) async fn start_sandbox(&self, tpl: &Template, name: String) -> anyhow::Result<()> {
        let inst = self.make_run(tpl, Some(name)).await?;
        if let Err(e) = self.runtime.start(&inst.name, self.log.instance(&inst.name, &inst.deployment)).await {
            self.runtime.remove(&inst.name).await;
            return Err(e);
        }
        self.runtime.settle(&inst.name).await;
        Ok(())
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
    /// What a run kept at the path it was asked to keep, copied out after it stopped: a tar
    /// archive, at most [`MAX_KEPT`] (a browser check's failure: `crate::browser`).
    pub kept: Option<Vec<u8>>,
}

/// The most a run's kept files may be.
pub const MAX_KEPT: u64 = 24 << 20;

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
                ports: vec![],
                udp: false,
                cpu: 0.5,
                memory_mb: 512,
                network: None,
                closed: false,
                aliases: vec![],
                volume: None,
                idle_stop_s: None,
                registry: None,
                files: vec![],
                health: None,
                sidecars: vec![],
            },
            run,
            spa: false,
            site: None,
            edge: Vec::new(),
            edge_service: String::new(),
            intercept_hosts: Vec::new(),
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
        let run = Run { volume: Some(crate::model::Volume { mount: "/data".into(), size_gb: 1, backups: None }), min_instances: 1, ..Run::default() };
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
