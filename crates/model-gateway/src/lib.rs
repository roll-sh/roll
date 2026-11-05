//! roll's model gateway (docs/code/gateway.md): an HTTP server on one of the owner's machines
//! that takes the requests their coding agents make (Anthropic Messages, OpenAI Chat
//! Completions and Responses) and sends them on to the owner's upstreams (Anthropic, OpenAI,
//! Azure OpenAI, Bedrock, Vertex, OpenRouter, any compatible base URL), translating between the
//! APIs where an upstream speaks another one, failing over between upstreams on errors and
//! outages, and keeping usage and cost history.
//!
//! It runs inside `roll agent up` (`agent-host/src/gateway.rs` supervises it, and its
//! management is daemon commands, so there is no management HTTP API to protect), or alone as
//! `roll gateway serve --config <file>` ([`config`]).
//!
//! **What it never does** (docs/code/gateway.md, "Limits"): store, proxy or rotate a
//! consumer-subscription login (Claude.ai or ChatGPT sign-ins: upstreams take API keys and
//! cloud credentials only), present one client as another (requests go out as the gateway,
//! with the client's own headers dropped, never as Claude Code or Codex), or move between keys
//! of one provider to get around its rate limits (a 429 cools that upstream down and only an
//! upstream of another provider may take over).
//!
//! Layout:
//! - [`wire`]: the three APIs, the translation between them (requests, responses and streams)
//!   and a bounded SSE parser. No I/O.
//! - [`upstream`]: each upstream kind's URL, auth (bearer, `x-api-key`, `api-key`, SigV4, a
//!   Vertex access token) and stream framing (Bedrock's event stream).
//! - [`stream`]: an upstream's stream read event by event, each with its exact bytes.
//! - [`route`]: which upstreams serve a request, in order: routes, aliases, priorities,
//!   circuits, cool-downs and session affinity.
//! - [`net`]: the HTTP client: no redirects, private addresses refused unless allowed.
//! - [`server`]: the inbound routes, client keys, limits, listeners and the proxy loop.
//! - [`store`]: `gateway.db`: client keys (hashes), usage roll-ups, the request log.
//! - [`price`]: list prices and cost.
//! - [`config`]: the standalone config file and its hot reload.

pub mod config;
pub mod net;
pub mod price;
pub mod route;
pub mod server;
pub mod store;
pub mod stream;
pub mod upstream;
pub mod wire;

mod types;

pub use types::{ClientRef, Record, Secret, Usage};

use agent_proto::{ClientSeen, GatewayClient, GatewayRun, GatewaySettings, GatewayStatus, LogBody, LogEntry, MachineId, UpstreamCheck, UsageBucket, UsageBy, UsageReport};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use upstream::Upstream;

/// Requests waiting to be written to the store; past this, records are dropped (and logged).
const RECORD_QUEUE: usize = 4096;
/// Requests remembered per upstream for `requests_hour` and `p50_ms`.
const STATS_PER_UPSTREAM: usize = 20_000;
const HOUR_MS: u64 = 3_600_000;

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A running (or stopped) gateway: its store, its settings and keys, and its listeners. Cheap
/// to clone; every clone is the same gateway.
#[derive(Clone)]
pub struct Gateway {
    pub(crate) inner: Arc<Inner>,
}

/// The settings and upstreams requests use: swapped whole by [`Gateway::apply`]; a request
/// keeps the one it started with.
pub(crate) struct Live {
    pub settings: GatewaySettings,
    pub upstreams: HashMap<String, Arc<Upstream>>,
}

/// What an upstream lists, and when it was asked.
pub(crate) struct Listing {
    pub at: u64,
    /// `None`: it can't list, or asking failed (it's then assumed to serve what it's asked).
    pub models: Option<Vec<String>>,
}

/// (when, time to the answer's headers; `None` when it failed), oldest first.
type Ring = VecDeque<(u64, Option<u32>)>;

enum Job {
    Record(Box<Record>, Vec<agent_proto::ModelPrice>, bool),
}

pub(crate) struct Inner {
    dir: PathBuf,
    pub(crate) store: Arc<store::Store>,
    pub(crate) live: RwLock<Arc<Live>>,
    pub(crate) health: route::Health,
    pub(crate) net: net::Net,
    pub(crate) run: tokio::sync::watch::Sender<GatewayRun>,
    pub(crate) listeners: tokio::sync::Mutex<server::Listeners>,
    /// Per upstream: (when, time to its answer's headers; `None` when it failed).
    stats: Mutex<HashMap<String, Ring>>,
    pub(crate) models: Mutex<HashMap<String, Listing>>,
    /// One refresh of the model lists at a time.
    pub(crate) listing: tokio::sync::Mutex<()>,
    pub(crate) inflight: Arc<tokio::sync::Semaphore>,
    recorder: Mutex<Option<(std::sync::mpsc::SyncSender<Job>, std::thread::JoinHandle<()>)>>,
    retention: Arc<AtomicU16>,
    pub(crate) vertex_token_uri: Mutex<Option<String>>,
}

impl Gateway {
    /// Opens (or makes) `dir/gateway.db` (mode 0600). Doesn't listen: [`Gateway::apply`] does.
    pub async fn open(dir: &Path) -> anyhow::Result<Gateway> {
        let dir = dir.to_path_buf();
        let store = {
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || -> anyhow::Result<store::Store> {
                std::fs::create_dir_all(&dir)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
                }
                store::Store::open(&dir.join("gateway.db"))
            })
            .await??
        };
        let store = Arc::new(store);
        let retention = Arc::new(AtomicU16::new(GatewaySettings::default().retention_days));
        let (tx, rx) = std::sync::mpsc::sync_channel(RECORD_QUEUE);
        let thread = {
            let store = store.clone();
            let retention = retention.clone();
            std::thread::Builder::new().name("gateway-record".into()).spawn(move || record_loop(&store, &rx, &retention))?
        };
        let inner = Inner {
            dir,
            store,
            live: RwLock::new(Arc::new(Live { settings: GatewaySettings::default(), upstreams: HashMap::new() })),
            health: route::Health::new(),
            net: net::Net::new()?,
            run: tokio::sync::watch::channel(GatewayRun::Stopped).0,
            listeners: tokio::sync::Mutex::new(server::Listeners::default()),
            stats: Mutex::new(HashMap::new()),
            models: Mutex::new(HashMap::new()),
            listing: tokio::sync::Mutex::new(()),
            inflight: Arc::new(tokio::sync::Semaphore::new(server::MAX_IN_FLIGHT)),
            recorder: Mutex::new(Some((tx, thread))),
            retention,
            vertex_token_uri: Mutex::new(None),
        };
        Ok(Gateway { inner: Arc::new(inner) })
    }

    /// Uses `settings` and `keys` (upstream id → key) from the next request on, and listens as
    /// they say: loopback plus `settings.listen`, on `settings.port`; nothing when disabled.
    /// Listeners are only rebound when the addresses or port change, so a change of upstreams
    /// or routes never drops a request in flight. Returns the run state it reached.
    ///
    /// Settings that don't pass [`GatewaySettings::check`] change nothing: the gateway keeps
    /// serving as it was, and the state says why it didn't take them.
    pub async fn apply(&self, settings: GatewaySettings, keys: HashMap<String, Secret>) -> GatewayRun {
        let settings = match settings.check() {
            Ok(s) => s,
            Err(e) => {
                let addresses = match self.run_state() {
                    GatewayRun::Running { addresses, .. } | GatewayRun::Failed { addresses, .. } => addresses,
                    GatewayRun::Stopped => vec![],
                };
                let run = GatewayRun::Failed { error: format!("settings not applied: {e}"), addresses };
                self.inner.run.send_replace(run.clone());
                return run;
            }
        };
        let old = self.live();
        let upstreams: HashMap<String, Arc<Upstream>> = settings
            .upstreams
            .iter()
            .map(|spec| {
                let key = keys.get(&spec.id);
                let up = match old.upstreams.get(&spec.id) {
                    Some(u) if u.same(spec, key) => u.clone(),
                    _ => Arc::new(Upstream::new(spec.clone(), key.cloned())),
                };
                (spec.id.clone(), up)
            })
            .collect();
        {
            // A changed upstream lists its models again.
            let mut models = self.inner.models.lock().unwrap();
            models.retain(|id, _| upstreams.get(id).is_some_and(|u| old.upstreams.get(id).is_some_and(|o| Arc::ptr_eq(o, u))));
        }
        let ids: Vec<&str> = settings.upstreams.iter().map(|u| u.id.as_str()).collect();
        self.inner.health.retain(&ids);
        self.inner.stats.lock().unwrap().retain(|k, _| ids.contains(&k.as_str()));
        self.inner.retention.store(settings.retention_days, Ordering::Relaxed);
        let want = if settings.enabled { server::addresses(&settings) } else { Vec::new() };
        *self.inner.live.write().unwrap() = Arc::new(Live { settings, upstreams });
        self.listen(want).await
    }

    pub(crate) fn live(&self) -> Arc<Live> {
        self.inner.live.read().unwrap().clone()
    }

    /// Whether it serves now.
    pub fn run_state(&self) -> GatewayRun {
        self.inner.run.borrow().clone()
    }

    /// A receiver that sees every change of [`Gateway::run_state`] (a listener failing, a
    /// retry binding it).
    pub fn watch_run(&self) -> tokio::sync::watch::Receiver<GatewayRun> {
        self.inner.run.subscribe()
    }

    /// Stops listening (requests in flight finish).
    pub async fn stop(&self) {
        self.listen(Vec::new()).await;
    }

    /// Mints a client key (`rgk_…`). The store keeps only its hash; the caller seals the key to
    /// the machine that uses it and drops it. A machine's earlier client is revoked.
    pub fn issue_client(&self, name: &str, machine: Option<MachineId>) -> anyhow::Result<(GatewayClient, Secret)> {
        self.inner.store.issue(name, machine, now_ms())
    }

    /// Revokes a client: its next request is refused. `false`: no such client.
    pub fn revoke_client(&self, id: &str) -> anyhow::Result<bool> {
        self.inner.store.revoke(id)
    }

    pub fn clients(&self) -> Vec<GatewayClient> {
        self.inner.store.clients().unwrap_or_else(|e| {
            tracing::warn!("gateway: listing clients: {e:#}");
            Vec::new()
        })
    }

    /// Health of every upstream, when each client was last seen, the models offered.
    pub async fn status(&self) -> GatewayStatus {
        let live = self.live();
        let now = now_ms();
        let upstreams = live
            .settings
            .upstreams
            .iter()
            .map(|u| {
                let mut h = self.inner.health.health(&u.id, now);
                (h.requests_hour, h.p50_ms) = self.stats_of(&u.id, now);
                h
            })
            .collect();
        let store = self.inner.store.clone();
        let seen = tokio::task::spawn_blocking(move || store.last_used()).await.ok().and_then(|r| r.ok()).unwrap_or_default();
        GatewayStatus {
            run: self.run_state(),
            upstreams,
            clients_seen: seen.into_iter().map(|(client, at)| ClientSeen { client, at }).collect(),
            models: self.offered(&live),
        }
    }

    /// Asks one upstream (or every enabled one) for its models: never generates.
    pub async fn test(&self, upstream: Option<&str>) -> Vec<UpstreamCheck> {
        let live = self.live();
        let token_uri = self.inner.vertex_token_uri.lock().unwrap().clone();
        let picked: Vec<Arc<Upstream>> = live
            .settings
            .upstreams
            .iter()
            .filter(|u| match upstream {
                Some(id) => u.id == id,
                None => !u.disabled,
            })
            .filter_map(|u| live.upstreams.get(&u.id).cloned())
            .collect();
        let checks = picked.into_iter().map(|up| {
            let net = self.inner.net.clone();
            let token_uri = token_uri.clone();
            async move {
                let started = Instant::now();
                let ctx = upstream::Ctx { net: &net, now_ms: now_ms(), vertex_token_uri: token_uri.as_deref() };
                let result = tokio::time::timeout(Duration::from_secs(30), up.check(&ctx)).await.unwrap_or_else(|_| Err("no answer in 30 s".into()));
                let ms = started.elapsed().as_millis() as u32;
                match result {
                    Ok(models) => UpstreamCheck { upstream: up.spec.id.clone(), ok: true, ms, models, error: None },
                    Err(e) => UpstreamCheck { upstream: up.spec.id.clone(), ok: false, ms, models: 0, error: Some(e) },
                }
            }
        });
        futures_util::future::join_all(checks).await
    }

    pub fn usage(&self, from: u64, to: u64, by: UsageBy, bucket: UsageBucket) -> anyhow::Result<UsageReport> {
        self.inner.store.usage(from, to, by, bucket)
    }

    pub fn log(&self, after: Option<u64>, before: Option<u64>, limit: u16, errors: bool) -> anyhow::Result<Vec<LogEntry>> {
        self.inner.store.log(after, before, limit, errors)
    }

    pub fn log_body(&self, seq: u64) -> anyhow::Result<Option<LogBody>> {
        self.inner.store.log_body(seq)
    }

    /// Stops and deletes `gateway.db`: clients, usage and the log.
    pub async fn wipe(self) -> anyhow::Result<()> {
        self.stop().await;
        let recorder = self.inner.recorder.lock().unwrap().take();
        if let Some((tx, thread)) = recorder {
            drop(tx);
            tokio::task::spawn_blocking(move || thread.join()).await?.map_err(|_| anyhow::anyhow!("the gateway's recorder panicked"))?;
        }
        for name in ["gateway.db", "gateway.db-wal", "gateway.db-shm"] {
            match std::fs::remove_file(self.inner.dir.join(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Tests only: Vertex tokens come from `uri` (a mock) instead of Google's token endpoint.
    #[doc(hidden)]
    pub fn set_vertex_token_uri_for_tests(&self, uri: Option<String>) {
        *self.inner.vertex_token_uri.lock().unwrap() = uri;
    }

    /// Queues a finished request for the store (written off the async path).
    pub(crate) fn record(&self, record: Record) {
        let live = self.live();
        let job = Job::Record(Box::new(record), live.settings.prices.clone(), live.settings.log_bodies);
        let recorder = self.inner.recorder.lock().unwrap();
        if let Some((tx, _)) = recorder.as_ref()
            && let Err(std::sync::mpsc::TrySendError::Full(_)) = tx.try_send(job)
        {
            tracing::warn!("gateway: the store is behind; a request wasn't recorded");
        }
    }

    pub(crate) fn note_attempt(&self, upstream: &str, at: u64, first_byte_ms: Option<u32>) {
        let mut stats = self.inner.stats.lock().unwrap();
        let ring = stats.entry(upstream.to_string()).or_default();
        while ring.front().is_some_and(|(t, _)| at.saturating_sub(*t) > HOUR_MS) || ring.len() >= STATS_PER_UPSTREAM {
            ring.pop_front();
        }
        ring.push_back((at, first_byte_ms));
    }

    /// (requests in the last hour, the median time to an answer among them).
    fn stats_of(&self, upstream: &str, now: u64) -> (u32, Option<u32>) {
        let stats = self.inner.stats.lock().unwrap();
        let Some(ring) = stats.get(upstream) else { return (0, None) };
        let recent: Vec<&(u64, Option<u32>)> = ring.iter().filter(|(t, _)| now.saturating_sub(*t) <= HOUR_MS).collect();
        let mut times: Vec<u32> = recent.iter().filter_map(|(_, ms)| *ms).collect();
        times.sort_unstable();
        (recent.len() as u32, times.get(times.len() / 2).copied().filter(|_| !times.is_empty()))
    }
}

fn record_loop(store: &store::Store, rx: &std::sync::mpsc::Receiver<Job>, retention: &AtomicU16) {
    let mut pruned = Instant::now().checked_sub(Duration::from_secs(3600)).unwrap_or_else(Instant::now);
    loop {
        if pruned.elapsed() >= Duration::from_secs(3600) {
            if let Err(e) = store.prune(now_ms(), retention.load(Ordering::Relaxed)) {
                tracing::warn!("gateway: pruning the store: {e:#}");
            }
            pruned = Instant::now();
        }
        match rx.recv_timeout(Duration::from_secs(600)) {
            Ok(Job::Record(record, prices, log_bodies)) => {
                if let Err(e) = store.record(&record, &prices, log_bodies) {
                    tracing::warn!("gateway: recording a request: {e:#}");
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}
