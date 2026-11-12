//! One environment's instances on its service's owner (docs/apps/README.md, "The runtime"):
//! the waking proxy's side ([`EnvLive::acquire`]), the reaper that suspends, stops and scales
//! ([`EnvLive::tick`]), and the two halves of a rollout ([`EnvLive::prepare`] and
//! [`EnvLive::commit`]).
//!
//! The state lives only in the owner's memory: a new owner starts from the store (the active
//! deployment) and makes instances again.

use crate::logs::{LogSink, Stream};
use crate::metrics::Meter;
use crate::model::{InstanceState, Run, RunType};
use crate::runtime::{InstanceSpec, Runtime};
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// What instances of the active deployment are made from.
#[derive(Clone, Debug)]
pub struct Template {
    pub deployment: String,
    pub spec: InstanceSpec,
    pub run: Run,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Made, never started (or stopped): a start away.
    Created,
    Starting,
    Running(Option<SocketAddr>),
    Suspended(Option<SocketAddr>),
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
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.inst.in_flight.fetch_sub(1, Ordering::SeqCst);
        *self.inst.last_used.lock() = Instant::now();
    }
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
}

#[derive(Clone, Debug)]
pub struct LiveConfig {
    pub wake_timeout: Duration,
    /// How long a worker must keep running after a start to count as up.
    pub worker_grace: Duration,
    /// How long old instances may finish their requests after a switch.
    pub drain: Duration,
    /// How long more instances than needed may stay up before one is stopped.
    pub scale_down_after: Duration,
    /// Between two probes of a starting instance.
    pub probe_every: Duration,
}

impl Default for LiveConfig {
    fn default() -> Self {
        LiveConfig {
            wake_timeout: Duration::from_secs(60),
            worker_grace: Duration::from_secs(2),
            drain: Duration::from_secs(30),
            scale_down_after: Duration::from_secs(60),
            probe_every: Duration::from_millis(10),
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
    wake: tokio::sync::Mutex<()>,
    seq: AtomicU64,
    pub log: LogSink,
    pub meter: Mutex<Meter>,
    over_since: Mutex<Option<Instant>>,
    /// A tick is running (the reaper never runs two of one environment at once).
    pub ticking: std::sync::atomic::AtomicBool,
    last_tick: Mutex<Option<Instant>>,
    client: hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, axum::body::Body>,
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
            wake: tokio::sync::Mutex::new(()),
            seq: AtomicU64::new(0),
            log,
            meter: Mutex::new(Meter::default()),
            over_since: Mutex::new(None),
            ticking: std::sync::atomic::AtomicBool::new(false),
            last_tick: Mutex::new(None),
            client: hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build_http(),
        })
    }

    pub fn template(&self) -> Option<Arc<Template>> {
        self.template.lock().clone()
    }

    pub fn instances(&self) -> Vec<Arc<Inst>> {
        self.insts.lock().clone()
    }

    pub fn state(&self) -> InstanceState {
        let insts = self.insts.lock();
        let states: Vec<InstanceState> = insts.iter().map(|i| i.state()).collect();
        if self.template.lock().is_none() {
            return InstanceState::None;
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
        let mut spec = tpl.spec.clone();
        spec.name = self.name_for(tpl);
        self.runtime.create(&spec).await?;
        Ok(Inst::new(spec.name, tpl.deployment.clone()))
    }

    /// Whether something answers HTTP at `addr` (`path`: a 2xx or 3xx there; none: any answer,
    /// since a published port accepts connections before the app listens).
    async fn answers(&self, addr: SocketAddr, path: Option<&str>) -> bool {
        let uri = format!("http://{addr}{}", path.unwrap_or("/"));
        let Ok(req) = axum::http::Request::get(uri).header("user-agent", "roll-apps-health").body(axum::body::Body::empty()) else { return false };
        match tokio::time::timeout(Duration::from_secs(5), self.client.request(req)).await {
            Ok(Ok(resp)) => path.is_none() || resp.status().is_success() || resp.status().is_redirection(),
            _ => false,
        }
    }

    /// Start `inst` and wait until it answers (web) or has kept running (worker).
    async fn start(&self, inst: &Arc<Inst>, run: &Run, health: Option<&str>, deadline: Instant) -> Result<Option<SocketAddr>, WakeError> {
        *inst.phase.lock() = Phase::Starting;
        let log = self.log.instance(&inst.name, &inst.deployment);
        let addr = match self.runtime.start(&inst.name, log).await {
            Ok(a) => a,
            Err(e) => {
                *inst.phase.lock() = Phase::Crashed;
                return Err(WakeError::Failed(e.to_string()));
            }
        };
        *inst.started_at.lock() = Some(now_ms());
        *inst.cpu_seen.lock() = None;
        match (run.kind, addr) {
            (RunType::Web, Some(a)) => loop {
                if self.answers(a, health).await {
                    break;
                }
                if Instant::now() >= deadline {
                    *inst.phase.lock() = Phase::Crashed;
                    return Err(WakeError::Timeout(self.cfg.wake_timeout));
                }
                if !self.runtime.running(&inst.name).await {
                    *inst.phase.lock() = Phase::Crashed;
                    return Err(WakeError::Failed("the process exited before it answered".into()));
                }
                tokio::time::sleep(self.cfg.probe_every).await;
            },
            _ => {
                tokio::time::sleep(self.cfg.worker_grace).await;
                if !self.runtime.running(&inst.name).await {
                    *inst.phase.lock() = Phase::Crashed;
                    return Err(WakeError::Failed("the process exited right after it started".into()));
                }
            }
        }
        *inst.phase.lock() = Phase::Running(addr);
        *inst.last_used.lock() = Instant::now();
        Ok(addr)
    }

    /// A running instance with room under `concurrency` (the least busy), leased.
    fn pick(&self, concurrency: u32, over: bool) -> Option<Lease> {
        let insts = self.insts.lock();
        let best = insts
            .iter()
            .filter_map(|i| match i.phase() {
                Phase::Running(Some(a)) => Some((i, a)),
                _ => None,
            })
            .filter(|(i, _)| over || i.in_flight() < concurrency)
            .min_by_key(|(i, _)| i.in_flight())?;
        best.0.in_flight.fetch_add(1, Ordering::SeqCst);
        Some(Lease { addr: best.1, inst: best.0.clone(), waited: Duration::ZERO, cold: false })
    }

    fn running_count(&self) -> usize {
        self.insts.lock().iter().filter(|i| matches!(i.phase(), Phase::Running(_))).count()
    }

    fn awake_count(&self) -> usize {
        self.insts.lock().iter().filter(|i| i.awake()).count()
    }

    /// An instance to send a request to: one that's up, else a suspended one resumed, else one
    /// started (the cold start), within `wake_timeout`. Requests that come while one wakes wait
    /// for the same wake.
    pub async fn acquire(self: &Arc<Self>) -> Result<Lease, WakeError> {
        let began = Instant::now();
        let tpl = self.template().ok_or(WakeError::NoDeployment)?;
        if tpl.run.kind != RunType::Web {
            return Err(WakeError::NotWeb);
        }
        let run = &tpl.run;
        if let Some(l) = self.pick(run.concurrency, false) {
            return Ok(l);
        }
        // Everything is busy but something's running: scale out behind it, and go to the least
        // busy.
        if self.running_count() > 0 && (self.awake_count() as u32) < run.max_instances {
            let this = self.clone();
            tokio::spawn(async move { this.scale_up().await });
            if let Some(l) = self.pick(run.concurrency, true) {
                return Ok(l);
            }
        }
        let _g = self.wake.lock().await;
        if let Some(mut l) = self.pick(run.concurrency, self.awake_count() as u32 >= run.max_instances) {
            l.waited = began.elapsed();
            return Ok(l);
        }
        let tpl = self.template().ok_or(WakeError::NoDeployment)?;
        // A frozen one first: milliseconds.
        let suspended = self.insts.lock().iter().find(|i| matches!(i.phase(), Phase::Suspended(_))).cloned();
        if let Some(i) = suspended {
            let Phase::Suspended(addr) = i.phase() else { unreachable!() };
            if self.runtime.resume(&i.name).await.is_ok() {
                *i.phase.lock() = Phase::Running(addr);
                if let Some(mut l) = self.pick(tpl.run.concurrency, true) {
                    l.waited = began.elapsed();
                    return Ok(l);
                }
            } else {
                *i.phase.lock() = Phase::Crashed;
            }
        }
        // Then a start: a created instance of the active deployment, or a new one.
        let deadline = began + self.cfg.wake_timeout;
        let inst = self.ready_instance(&tpl).await.map_err(|e| WakeError::Failed(e.to_string()))?;
        let woke = Instant::now();
        self.start(&inst, &tpl.run, None, deadline).await?;
        let ms = woke.elapsed().as_secs_f64() * 1000.0;
        self.meter.lock().cold_start(now_ms(), ms);
        self.log.stream(Stream::System).line(format!("cold start: {} answered in {ms:.0} ms", inst.name));
        let mut l = self.pick(tpl.run.concurrency, true).ok_or_else(|| WakeError::Failed("the instance stopped again".into()))?;
        l.waited = began.elapsed();
        l.cold = true;
        Ok(l)
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

    async fn scale_up(self: Arc<Self>) {
        let Ok(_g) = self.wake.try_lock() else { return };
        let Some(tpl) = self.template() else { return };
        if self.awake_count() as u32 >= tpl.run.max_instances {
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
    /// ones keep serving meanwhile. On failure nothing of the new deployment is left.
    pub async fn prepare(self: &Arc<Self>, tpl: &Arc<Template>, health_path: Option<&str>) -> anyhow::Result<(Vec<Arc<Inst>>, Duration)> {
        let run = &tpl.run;
        let was_awake = self.awake_count() as u32;
        let n = match run.kind {
            RunType::Cron => 0,
            RunType::Worker => run.min_instances.max(1),
            RunType::Web => was_awake.max(run.min_instances).max(1),
        }
        .min(run.max_instances);
        if run.kind == RunType::Cron {
            // Nothing runs between runs; the next run uses the new template.
            return Ok((Vec::new(), Duration::ZERO));
        }
        if run.volume.is_some() {
            // One volume, one instance: the old one lets go of it first.
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
        Ok((made, began.elapsed()))
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
    pub async fn commit(self: &Arc<Self>, tpl: Arc<Template>, made: Vec<Arc<Inst>>) {
        let _g = self.wake.lock().await;
        let was_awake = self.awake_count();
        *self.template.lock() = Some(tpl.clone());
        let old: Vec<Arc<Inst>> = std::mem::replace(&mut *self.insts.lock(), made.clone());
        if tpl.run.kind == RunType::Web && was_awake == 0 && tpl.run.min_instances == 0 {
            for i in &made {
                let _ = self.runtime.stop(&i.name, Duration::from_secs(5)).await;
                *i.phase.lock() = Phase::Created;
            }
        }
        if made.is_empty() && tpl.run.kind == RunType::Web {
            // Warm-create one so the first request is only a start.
            if let Ok(i) = self.make(&tpl).await {
                self.insts.lock().push(i);
            }
        }
        let runtime = self.runtime.clone();
        let drain = self.cfg.drain;
        tokio::spawn(async move {
            let until = Instant::now() + drain;
            for i in old {
                while i.in_flight() > 0 && Instant::now() < until {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                let _ = runtime.stop(&i.name, Duration::from_secs(10)).await;
                runtime.remove(&i.name).await;
            }
        });
    }

    /// Set the active deployment without starting anything (a new owner taking the service).
    pub async fn adopt(self: &Arc<Self>, tpl: Arc<Template>) {
        let _g = self.wake.lock().await;
        *self.template.lock() = Some(tpl.clone());
        if tpl.run.kind == RunType::Web
            && self.insts.lock().is_empty()
            && let Ok(i) = self.make(&tpl).await
        {
            self.insts.lock().push(i);
        }
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
        let all: Vec<Arc<Inst>> = self.insts.lock().drain(..).collect();
        self.discard(&all).await;
        *self.template.lock() = None;
    }

    /// The reaper, about once a second: meter what runs, suspend and stop what's idle, keep
    /// `min_instances` up, scale down what's no longer needed, restart crashed workers.
    pub async fn tick(self: &Arc<Self>) {
        if self.template().is_none() {
            return;
        }
        let now = Instant::now();
        let elapsed = {
            let mut last = self.last_tick.lock();
            let e = last.map(|l| now.duration_since(l).as_secs_f64()).unwrap_or(0.0).min(10.0);
            *last = Some(now);
            e
        };
        // Metering: only running instances use anything.
        let insts = self.instances();
        let (mut cpu, mut mem, mut up) = (0.0, 0u64, 0usize);
        for i in &insts {
            if !matches!(i.phase(), Phase::Running(_)) {
                continue;
            }
            up += 1;
            if let Some(u) = self.runtime.usage(&i.name).await {
                let mut seen = i.cpu_seen.lock();
                if let Some(prev) = *seen {
                    cpu += u.cpu_ns.saturating_sub(prev) as f64 / 1e9;
                }
                *seen = Some(u.cpu_ns);
                mem += u.memory_bytes;
            }
        }
        if up > 0 || elapsed > 0.0 {
            self.meter.lock().sample(now_ms(), cpu, mem, elapsed, up);
        }
        let _g = self.wake.lock().await;
        // Read again under the lock: a teardown (`shutdown`) or a new deployment may have
        // happened while this tick waited, and the floor must not make instances for either.
        let Some(tpl) = self.template() else { return };
        match tpl.run.kind {
            RunType::Cron => {}
            RunType::Worker => self.keep_workers(&tpl).await,
            RunType::Web => self.reap_web(&tpl).await,
        }
    }

    async fn reap_web(self: &Arc<Self>, tpl: &Arc<Template>) {
        let run = &tpl.run;
        let insts = self.instances();
        let suspend_after = Duration::from_secs(run.suspend_after_s as u64);
        let sleep_after = Duration::from_secs(run.sleep_after_s as u64);
        let mut awake = insts.iter().filter(|i| i.awake()).count() as u32;
        // Fewer than needed by the requests in flight for a while → one goes.
        let in_flight: u32 = insts.iter().map(|i| i.in_flight()).sum();
        let needed = in_flight.div_ceil(run.concurrency).max(run.min_instances).max(1);
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
        if scale_down && let Some(i) = insts.iter().filter(|i| matches!(i.phase(), Phase::Running(_)) && i.in_flight() == 0).min_by_key(|i| *i.last_used.lock()) {
            let _ = self.runtime.stop(&i.name, Duration::from_secs(10)).await;
            *i.phase.lock() = Phase::Created;
            awake -= 1;
            *self.over_since.lock() = None;
            self.log.stream(Stream::System).line(format!("scaled in: {awake} instances"));
        }
        for i in &insts {
            if i.in_flight() > 0 {
                continue;
            }
            let idle = i.last_used.lock().elapsed();
            match i.phase() {
                Phase::Running(addr) if awake > run.min_instances => {
                    if idle >= sleep_after || (!self.runtime.can_suspend() && idle >= suspend_after.max(sleep_after)) {
                        let _ = self.runtime.stop(&i.name, Duration::from_secs(10)).await;
                        *i.phase.lock() = Phase::Created;
                        awake -= 1;
                    } else if self.runtime.can_suspend() && run.suspend_after_s > 0 && idle >= suspend_after && self.runtime.suspend(&i.name).await.is_ok() {
                        *i.phase.lock() = Phase::Suspended(addr);
                    }
                }
                Phase::Suspended(_) if idle >= sleep_after && awake > run.min_instances => {
                    let _ = self.runtime.stop(&i.name, Duration::from_secs(10)).await;
                    *i.phase.lock() = Phase::Created;
                    awake -= 1;
                }
                Phase::Running(_) | Phase::Suspended(_) if !self.runtime.running(&i.name).await => {
                    // Exited on its own: the next request starts it again.
                    *i.phase.lock() = Phase::Crashed;
                    awake = awake.saturating_sub(1);
                    self.log.stream(Stream::System).line(format!("{} exited", i.name));
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

    async fn keep_workers(self: &Arc<Self>, tpl: &Arc<Template>) {
        let run = &tpl.run;
        let insts = self.instances();
        let mut up = 0;
        for i in &insts {
            match i.phase() {
                Phase::Running(_) if self.runtime.running(&i.name).await => up += 1,
                Phase::Running(_) => {
                    *i.phase.lock() = Phase::Crashed;
                    i.crashes.fetch_add(1, Ordering::SeqCst);
                    self.log.stream(Stream::System).line(format!("{} exited; restarting", i.name));
                }
                _ => {}
            }
        }
        for i in insts.iter().filter(|i| i.phase() == Phase::Crashed) {
            if up >= run.min_instances {
                break;
            }
            // Back off: 1 s doubling to 60 s since it last ran.
            let crashes = i.crashes.load(Ordering::SeqCst).min(6);
            let wait = Duration::from_secs((1u64 << crashes).min(60));
            if i.last_used.lock().elapsed() < wait {
                continue;
            }
            *i.last_used.lock() = Instant::now();
            let deadline = Instant::now() + self.cfg.wake_timeout;
            if self.start(i, run, None, deadline).await.is_ok() {
                up += 1;
            } else {
                i.crashes.fetch_add(1, Ordering::SeqCst);
            }
        }
        while up < run.min_instances {
            let Ok(inst) = self.make(tpl).await else { break };
            self.insts.lock().push(inst.clone());
            let deadline = Instant::now() + self.cfg.wake_timeout;
            if self.start(&inst, run, None, deadline).await.is_err() {
                break;
            }
            up += 1;
        }
    }

    /// One cron run: a fresh instance of the active deployment, to its exit or `timeout`.
    pub async fn run_once(self: &Arc<Self>, timeout: Duration) -> anyhow::Result<(String, Option<i32>, bool)> {
        let tpl = self.template().ok_or_else(|| anyhow::anyhow!("no deployment"))?;
        let inst = self.make(&tpl).await?;
        let log = self.log.instance(&inst.name, &inst.deployment);
        let began = Instant::now();
        self.runtime.start(&inst.name, log).await?;
        let code = tokio::time::timeout(timeout, self.runtime.wait(&inst.name)).await;
        let timed_out = code.is_err();
        if timed_out {
            let _ = self.runtime.stop(&inst.name, Duration::from_secs(10)).await;
        }
        let secs = began.elapsed().as_secs_f64();
        // A run is metered at its allocation: it's short, and the container is gone after.
        self.meter.lock().sample(now_ms(), secs * tpl.run.cpu * 0.5, (tpl.run.memory_mb as u64) << 20, secs, 1);
        self.runtime.remove(&inst.name).await;
        Ok((tpl.deployment.clone(), code.ok().and_then(Result::ok), timed_out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::LogHub;
    use crate::runtime::fake::{Fake, FakeState};

    fn template(run: Run) -> Arc<Template> {
        Arc::new(Template {
            deployment: "dep_aaaaaaaaaaaa".into(),
            spec: InstanceSpec {
                name: String::new(),
                service: "svc_x".into(),
                env: "production".into(),
                deployment: "dep_aaaaaaaaaaaa".into(),
                image: "img".into(),
                command: None,
                vars: vec![],
                port: Some(8080),
                cpu: 0.5,
                memory_mb: 512,
                network: None,
                aliases: vec![],
                volume: None,
            },
            run,
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
        let (made, _) = live.prepare(&tpl, None).await.unwrap();
        live.commit(tpl, made).await;
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
}
