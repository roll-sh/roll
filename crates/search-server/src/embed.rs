//! The node's embedding pool: `multilingual-e5-small` ([`search_embed`]) for every cluster the
//! node serves, behind the `elasticsearch` inference service's E5 endpoints
//! ([`search_es::xpack::inference::local`]).
//!
//! - **Weights** are the node's, never a tenant's: fetched once into the node's cache
//!   (`<cache>/multilingual-e5-small/<revision>/`) from the operator's URL (default: the pinned
//!   Hugging Face revision, [`search_embed::REPOSITORY`]), or read from a directory the operator
//!   names (air-gapped nodes), and checked against [`search_embed::FILES`]' SHA-256 digests
//!   before they're used. Each download streams into a file of its own (`<file>.part.<pid>.<n>`),
//!   checked for size and digest as it's written, then renamed into place; a cached file whose
//!   digest is wrong is deleted and fetched again.
//! - **One load at a time**: fetching, checking and loading the model is one detached task
//!   behind a shared slot ([`State::load`]). Callers wait on it with their own timeouts, and a
//!   caller that gives up neither cancels nor restarts it, so a loop of `timeout=1ms` calls
//!   starts one load, not one each. The load's memory (about twice the weights at its peak)
//!   counts from when it starts reading the weights. A load that fails is remembered with a
//!   growing backoff (30 s, doubling to 10 minutes): until it ends, calls answer the failure at
//!   once instead of downloading or loading again.
//! - **Loaded lazily**, on the first input, and **dropped** after [`EmbedConfig::idle_unload`]
//!   with nothing queued, running or loading: an idle node holds no model.
//! - **Workers**: [`EmbedConfig::workers`] threads (a third of the CPUs by default), optionally
//!   pinned to [`EmbedConfig::cpus`]. The model runs on the calling thread alone, so a job runs on
//!   one core and the thread's CPU clock measures it exactly. A job that panics is answered with
//!   an internal error and its worker carries on.
//! - **Fair shares**: inputs are cut to the start their 512 tokens come from
//!   ([`search_embed::E5::cut`]) and queue under their cluster in jobs of about
//!   [`EmbedConfig::job_tokens`] tokens (estimated from bytes). A free worker takes the next
//!   owner's next cluster's next job: owners take turns, and each owner's clusters take turns
//!   among themselves, so one person's 50 clusters get one turn per round like another person's
//!   one; clusters without an owner (the operator's) share one turn. Within a cluster, searches
//!   go before ingest, so a query never waits behind its own cluster's bulk. A caller that goes
//!   away (the client disconnected, an outer timeout) cancels its jobs not yet started.
//! - **Caps**: [`EmbedConfig::max_inputs`] inputs per call (400); per cluster
//!   [`EmbedConfig::queue_inputs`] inputs and [`EmbedConfig::queue_bytes`] bytes queued; node-wide
//!   [`EmbedConfig::node_queue_bytes`] bytes and [`EmbedConfig::node_queue_tokens`] tokens queued.
//!   A call past any of them is refused whole, as a deployment with a full queue refuses (429
//!   `es_rejected_execution_exception`).
//! - **Memory**: the model, the load in progress, the queued inputs and the jobs running are
//!   [`EmbedPool::resident_bytes`], which the node counts against its memory budget like loaded
//!   clusters (`Node::enforce_memory`).
//! - **Metering**: each job's thread CPU time goes to its cluster ([`EmbedPool::take_cpu`]),
//!   which the control plane bills as inference CPU.

use parking_lot::{Condvar, Mutex};
use search_es::xpack::inference::local::{EmbedError, Embedder, InputKind, Tenant};
use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Estimated tokens per job: about two full-length inputs, or many short ones.
pub const JOB_TOKENS: usize = 1024;

/// What a job that panicked answers with.
const PANICKED: &str = "an internal error";

/// The model as the pool uses it ([`search_embed::E5`]; tests use fakes).
pub trait Model: Send + Sync {
    /// The bounded start of `text` its embedding comes from (what queues).
    fn cut(&self, text: &str) -> String;
    /// Embeddings of `prefix` before each of `texts`.
    fn embed(&self, prefix: &str, texts: &[String]) -> Result<Vec<Vec<f32>>, String>;
    /// Memory the loaded model holds.
    fn bytes(&self) -> u64;
    /// Memory one job holds while it runs.
    fn job_bytes(&self) -> u64;
}

impl Model for search_embed::E5 {
    fn cut(&self, text: &str) -> String {
        search_embed::E5::cut(self, text).into_owned()
    }

    fn embed(&self, prefix: &str, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        self.embed_prefixed(prefix, texts).map_err(|e| e.to_string())
    }

    fn bytes(&self) -> u64 {
        search_embed::E5::bytes(self)
    }

    fn job_bytes(&self) -> u64 {
        self.batch_bytes_max()
    }
}

/// Loads the model from a directory holding its files.
pub type LoadFn = dyn Fn(&Path) -> Result<Arc<dyn Model>, String> + Send + Sync;

/// [`LoadFn`], cloneable and printable.
#[derive(Clone)]
pub struct Loader(pub Arc<LoadFn>);

impl Default for Loader {
    fn default() -> Self {
        Loader(Arc::new(|dir: &Path| search_embed::E5::load(dir).map(|m| Arc::new(m) as Arc<dyn Model>).map_err(|e| e.to_string())))
    }
}

impl std::fmt::Debug for Loader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Loader")
    }
}

/// One of the model's files: its name, SHA-256 digest (lowercase hex) and size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelFile {
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

/// The pinned revision's files ([`search_embed::FILES`]).
pub fn pinned_files() -> Vec<ModelFile> {
    search_embed::FILES.iter().map(|(n, d, s)| ModelFile { name: n.to_string(), sha256: d.to_string(), size: *s }).collect()
}

/// Where the model comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelSource {
    /// No model: E5 endpoints answer as on a cluster without ML nodes.
    Off,
    /// Fetched once from `base` (`<base>/<file>`) into the cache.
    Fetch { base: String },
    /// A directory holding [`search_embed::WEIGHTS`] and [`search_embed::TOKENIZER`] (checked
    /// against the digests like a download; never deleted or rewritten).
    Dir(PathBuf),
}

impl ModelSource {
    /// The pinned revision on Hugging Face.
    pub fn default_fetch() -> ModelSource {
        ModelSource::Fetch { base: format!("{}/{}", search_embed::REPOSITORY, search_embed::REVISION) }
    }
}

#[derive(Clone, Debug)]
pub struct EmbedConfig {
    pub source: ModelSource,
    /// Where fetched weights are kept (`<cache>/multilingual-e5-small/<revision>/`).
    pub cache_dir: PathBuf,
    /// Worker threads (0: a third of the CPUs, or of [`EmbedConfig::cpus`], at least 1).
    pub workers: usize,
    /// CPUs the workers are pinned to (empty: not pinned).
    pub cpus: Vec<usize>,
    /// Drop the model after this long with nothing to do.
    pub idle_unload: Duration,
    /// Inputs one call may carry.
    pub max_inputs: usize,
    /// Inputs, and bytes of them, a cluster may have queued (not yet running).
    pub queue_inputs: usize,
    pub queue_bytes: usize,
    /// Bytes and estimated tokens queued on the node, every cluster's together.
    pub node_queue_bytes: usize,
    pub node_queue_tokens: usize,
    /// Estimated tokens per job ([`JOB_TOKENS`]).
    pub job_tokens: usize,
    /// How long a download may go without a byte before it fails.
    pub fetch_read_timeout: Duration,
    /// The files and their digests ([`pinned_files`]).
    pub files: Vec<ModelFile>,
    /// Skip the digest check of a directory's files (tests' made-up models only).
    pub unchecked: bool,
    /// How the model is loaded ([`search_embed::E5::load`]).
    pub loader: Loader,
}

impl EmbedConfig {
    pub fn new(source: ModelSource, cache_dir: PathBuf) -> EmbedConfig {
        EmbedConfig {
            source,
            cache_dir,
            workers: 0,
            cpus: Vec::new(),
            idle_unload: Duration::from_secs(300),
            max_inputs: 2048,
            queue_inputs: 4096,
            queue_bytes: 64 << 20,
            node_queue_bytes: 512 << 20,
            node_queue_tokens: 1 << 20,
            job_tokens: JOB_TOKENS,
            fetch_read_timeout: Duration::from_secs(60),
            files: pinned_files(),
            unchecked: false,
            loader: Loader::default(),
        }
    }

    pub fn worker_count(&self) -> usize {
        if self.workers > 0 {
            return self.workers;
        }
        let cpus = if self.cpus.is_empty() { std::thread::available_parallelism().map_or(1, |n| n.get()) } else { self.cpus.len() };
        (cpus / 3).max(1)
    }
}

/// Parses a CPU list (`0-5,18-23`); every CPU must fit a `cpu_set_t`.
pub fn parse_cpus(s: &str) -> anyhow::Result<Vec<usize>> {
    let max = libc::CPU_SETSIZE as usize;
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (usize, usize) = (a.trim().parse()?, b.trim().parse()?);
                anyhow::ensure!(a <= b, "bad CPU range [{part}]");
                anyhow::ensure!(b < max, "CPU [{b}] is past the largest a CPU set holds ({})", max - 1);
                out.extend(a..=b);
            }
            None => {
                let c: usize = part.parse()?;
                anyhow::ensure!(c < max, "CPU [{c}] is past the largest a CPU set holds ({})", max - 1);
                out.push(c);
            }
        }
    }
    Ok(out)
}

/// Estimated tokens of an input: about three bytes a token (most scripts take more per
/// token, CJK about three), at most [`search_embed::MAX_TOKENS`].
fn tokens_of(prefix: &str, text: &str) -> usize {
    ((prefix.len() + text.len()) / 3 + 2).min(search_embed::MAX_TOKENS)
}

struct Job {
    owner: Arc<str>,
    cluster: Arc<str>,
    kind: InputKind,
    texts: Vec<String>,
    tokens: usize,
    bytes: usize,
    done: tokio::sync::oneshot::Sender<Result<Vec<Vec<f32>>, String>>,
    /// The caller stopped waiting.
    cancelled: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Default)]
struct Queued {
    inputs: usize,
    bytes: usize,
    tokens: usize,
}

impl Queued {
    fn add(&mut self, j: &Job) {
        self.inputs += j.texts.len();
        self.bytes += j.bytes;
        self.tokens += j.tokens;
    }

    fn sub(&mut self, j: &Job) {
        self.inputs = self.inputs.saturating_sub(j.texts.len());
        self.bytes = self.bytes.saturating_sub(j.bytes);
        self.tokens = self.tokens.saturating_sub(j.tokens);
    }
}

type LoadSlot = tokio::sync::watch::Receiver<Option<Result<(), String>>>;

/// A cluster's queued jobs: searches first, then everything else.
#[derive(Default)]
struct ClusterQueue {
    search: VecDeque<Job>,
    rest: VecDeque<Job>,
}

impl ClusterQueue {
    fn is_empty(&self) -> bool {
        self.search.is_empty() && self.rest.is_empty()
    }

    fn push(&mut self, job: Job) {
        if job.kind == InputKind::Search { self.search.push_back(job) } else { self.rest.push_back(job) }
    }

    fn pop_front(&mut self) -> Option<Job> {
        self.search.pop_front().or_else(|| self.rest.pop_front())
    }
}

/// A load that failed: what callers are told until `until`, and the backoff that set it.
struct Failure {
    reason: &'static str,
    until: Instant,
    backoff: Duration,
}

/// The first backoff after a failed load, and the longest.
const FIRST_BACKOFF: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(600);

/// Why a load failed: what callers are told (no path, URL or digest: they're the operator's),
/// and the detail logged.
struct LoadFailure {
    reason: &'static str,
    detail: String,
}

impl LoadFailure {
    fn new(reason: &'static str, detail: impl Into<String>) -> LoadFailure {
        LoadFailure { reason, detail: detail.into() }
    }
}

/// Cancels a call's queued jobs when the call goes away, whether it finished, failed or was
/// dropped (its client disconnected): workers skip them.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct State {
    model: Option<Arc<dyn Model>>,
    /// The load in progress: it answers `Some(result)` when done.
    load: Option<LoadSlot>,
    /// Memory the load in progress may reach (once it reads the weights).
    load_bytes: u64,
    /// The last load's failure, while its backoff lasts.
    failure: Option<Failure>,
    /// Owners with queued jobs in turn order; each owner's clusters with queued jobs in turn
    /// order; each cluster's jobs. A cluster is in its owner's turns exactly while it has jobs,
    /// and an owner in `owners` exactly while it has such clusters.
    owners: VecDeque<Arc<str>>,
    clusters: HashMap<Arc<str>, VecDeque<Arc<str>>>,
    queues: HashMap<Arc<str>, ClusterQueue>,
    queued: HashMap<Arc<str>, Queued>,
    node: Queued,
    /// Jobs running, and the memory they hold.
    running: usize,
    running_bytes: u64,
    workers: usize,
    last_used: Option<Instant>,
}

impl State {
    fn push(&mut self, job: Job) {
        let (owner, cluster) = (job.owner.clone(), job.cluster.clone());
        self.queued.entry(cluster.clone()).or_default().add(&job);
        self.node.add(&job);
        let q = self.queues.entry(cluster.clone()).or_default();
        let first = q.is_empty();
        q.push(job);
        if first {
            let turns = self.clusters.entry(owner.clone()).or_default();
            if turns.is_empty() {
                self.owners.push_back(owner);
            }
            turns.push_back(cluster);
        }
    }

    /// The next owner's next cluster's next job.
    fn pop(&mut self) -> Option<Job> {
        let owner = self.owners.pop_front()?;
        let cluster = self.clusters.get_mut(&owner).and_then(VecDeque::pop_front)?;
        let q = self.queues.get_mut(&cluster)?;
        let job = q.pop_front()?;
        let more = !q.is_empty();
        if !more {
            self.queues.remove(&cluster);
        }
        if let Some(turns) = self.clusters.get_mut(&owner) {
            if more {
                turns.push_back(cluster.clone());
            }
            if turns.is_empty() {
                self.clusters.remove(&owner);
            } else {
                self.owners.push_back(owner);
            }
        }
        if let Some(c) = self.queued.get_mut(&cluster) {
            c.sub(&job);
            if c.inputs == 0 {
                self.queued.remove(&cluster);
            }
        }
        self.node.sub(&job);
        Some(job)
    }
}

struct Shared {
    state: Mutex<State>,
    work: Condvar,
    cpu: Mutex<HashMap<String, Duration>>,
    /// Weights directories already checked against the digests (once per process).
    verified: Mutex<Vec<PathBuf>>,
}

/// The pool. Costs nothing while idle: no model, and workers asleep.
pub struct EmbedPool {
    cfg: EmbedConfig,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for EmbedPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbedPool").field("source", &self.cfg.source).finish()
    }
}

/// The calling thread's CPU time.
fn thread_cpu() -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `clock_gettime` only writes the timespec we own.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } != 0 {
        return Duration::ZERO;
    }
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Pins the calling thread to `cpus` (each below `CPU_SETSIZE`: [`parse_cpus`]).
fn pin(cpus: &[usize]) {
    if cpus.is_empty() {
        return;
    }
    // SAFETY: a zeroed `cpu_set_t` is an empty set; `CPU_SET` and `sched_setaffinity` only
    // touch the set we own, and every CPU is below `CPU_SETSIZE`.
    let ok = unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        for &c in cpus.iter().filter(|c| **c < libc::CPU_SETSIZE as usize) {
            libc::CPU_SET(c, &mut set);
        }
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0
    };
    if !ok {
        tracing::warn!(?cpus, "pinning an embedding worker failed");
    }
}

/// `fut`, given until `deadline` (`None`: forever).
async fn until<T>(deadline: Option<Instant>, fut: impl std::future::Future<Output = T>) -> Option<T> {
    match deadline {
        Some(d) => tokio::time::timeout_at(d.into(), fut).await.ok(),
        None => Some(fut.await),
    }
}

impl EmbedPool {
    pub fn new(cfg: EmbedConfig) -> Arc<EmbedPool> {
        Arc::new(EmbedPool {
            cfg,
            shared: Arc::new(Shared { state: Mutex::new(State::default()), work: Condvar::new(), cpu: Mutex::new(HashMap::new()), verified: Mutex::new(Vec::new()) }),
        })
    }

    pub fn config(&self) -> &EmbedConfig {
        &self.cfg
    }

    /// Memory the model, a load in progress, the queued inputs and the running jobs hold.
    pub fn resident_bytes(&self) -> u64 {
        let st = self.shared.state.lock();
        st.model.as_ref().map_or(0, |m| m.bytes()) + st.load_bytes + st.node.bytes as u64 + st.running_bytes
    }

    /// Whether the model is in memory.
    pub fn loaded(&self) -> bool {
        self.shared.state.lock().model.is_some()
    }

    /// Worker threads started.
    pub fn workers(&self) -> usize {
        self.shared.state.lock().workers
    }

    /// Inputs queued (not yet running), every cluster's.
    pub fn queued_inputs(&self) -> usize {
        self.shared.state.lock().node.inputs
    }

    /// CPU time each cluster's inputs took since the last call, by [`Tenant::cluster`] (the
    /// cluster's catalog id).
    pub fn take_cpu(&self) -> Vec<(String, Duration)> {
        let mut v: Vec<(String, Duration)> = std::mem::take(&mut *self.shared.cpu.lock()).into_iter().collect();
        v.sort();
        v
    }

    /// The model, waiting until `deadline` for the one load in progress (starting it when there
    /// is none). Giving up leaves the load running for the next caller.
    async fn model(&self, deadline: Option<Instant>, timeout: Duration) -> Result<Arc<dyn Model>, EmbedError> {
        loop {
            let mut slot = {
                let mut st = self.shared.state.lock();
                if let Some(m) = st.model.clone() {
                    st.last_used = Some(Instant::now());
                    return Ok(m);
                }
                if st.load.is_none()
                    && let Some(f) = &st.failure
                {
                    let left = f.until.saturating_duration_since(Instant::now());
                    if !left.is_zero() {
                        return Err(EmbedError::Failed(format!("{}; it's tried again in {}s", f.reason, left.as_secs().max(1))));
                    }
                }
                match &st.load {
                    Some(slot) => slot.clone(),
                    None => self.start_load(&mut st),
                }
            };
            let done = until(deadline, async { slot.wait_for(Option::is_some).await.map(|r| r.clone()) }).await.ok_or(EmbedError::TimedOut(timeout))?;
            match done {
                Ok(Some(Err(reason))) => return Err(EmbedError::Failed(reason)),
                Ok(_) => continue,
                Err(_) => {
                    // The load task ended without answering: clear its slot so the next caller
                    // starts another.
                    let mut st = self.shared.state.lock();
                    if st.load.as_ref().is_some_and(|s| s.same_channel(&slot)) {
                        st.load = None;
                        st.load_bytes = 0;
                    }
                    return Err(EmbedError::Failed("its loader stopped".into()));
                }
            }
        }
    }

    /// Starts the one load task: fetch what's missing, check the digests, load, install.
    fn start_load(&self, st: &mut State) -> LoadSlot {
        let (tx, slot) = tokio::sync::watch::channel(None);
        st.load = Some(slot.clone());
        let (shared, cfg) = (self.shared.clone(), self.cfg.clone());
        tokio::spawn(async move {
            let started = Instant::now();
            let loaded = load(&shared, &cfg).await;
            let answer = {
                let mut st = shared.state.lock();
                st.load = None;
                st.load_bytes = 0;
                match loaded {
                    Ok(m) => {
                        tracing::info!(bytes = m.bytes(), ms = started.elapsed().as_millis() as u64, "embedding model loaded");
                        st.model = Some(m);
                        st.last_used = Some(Instant::now());
                        st.failure = None;
                        Ok(())
                    }
                    Err(e) => {
                        let backoff = st.failure.as_ref().map_or(FIRST_BACKOFF, |f| (f.backoff * 2).min(MAX_BACKOFF));
                        tracing::warn!(error = %e.detail, backoff_s = backoff.as_secs(), "loading the embedding model failed");
                        st.failure = Some(Failure { reason: e.reason, until: Instant::now() + backoff, backoff });
                        Err(format!("{}; it's tried again in {}s", e.reason, backoff.as_secs()))
                    }
                }
            };
            shared.work.notify_all();
            let _ = tx.send(Some(answer));
        });
        slot
    }

    fn start_workers(&self, st: &mut State) {
        let want = self.cfg.worker_count();
        while st.workers < want {
            let shared = self.shared.clone();
            let cpus = self.cfg.cpus.clone();
            let idle = self.cfg.idle_unload;
            let spawned = std::thread::Builder::new().name(format!("search-embed-{}", st.workers)).spawn(move || {
                pin(&cpus);
                work(&shared, idle)
            });
            match spawned {
                Ok(_) => st.workers += 1,
                Err(e) => {
                    tracing::warn!(error = %e, "starting an embedding worker failed");
                    break;
                }
            }
        }
    }

    /// Embeds `texts` for `tenant`; see [`Embedder::embed`].
    pub async fn embed_for(&self, tenant: Tenant<'_>, kind: InputKind, texts: Vec<String>, timeout: Duration) -> Result<Vec<Vec<f32>>, EmbedError> {
        let prefix = kind.prefix();
        if self.cfg.source == ModelSource::Off {
            return Err(EmbedError::Off);
        }
        if texts.len() > self.cfg.max_inputs {
            return Err(EmbedError::TooLarge(format!("[{}] inputs is more than the [{}] one inference call may carry on this node", texts.len(), self.cfg.max_inputs)));
        }
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let deadline = Instant::now().checked_add(timeout);
        let (owner, cluster): (Arc<str>, Arc<str>) = (Arc::from(tenant.owner.unwrap_or("")), Arc::from(tenant.cluster));
        let cancelled = Arc::new(AtomicBool::new(false));
        let waits = loop {
            let model = self.model(deadline, timeout).await?;
            // Only the bounded start of each input queues.
            let cut: Vec<String> = texts.iter().map(|t| model.cut(t)).collect();
            let mut st = self.shared.state.lock();
            if st.model.is_none() {
                // Dropped as idle between loading and queueing: load it again.
                continue;
            }
            let bytes: usize = cut.iter().map(String::len).sum();
            let tokens: usize = cut.iter().map(|t| tokens_of(prefix, t)).sum();
            // Workers first: a call nobody could serve fails now, not after its deadline (with
            // no deadline, never). The next call tries to start them again.
            self.start_workers(&mut st);
            if st.workers == 0 {
                return Err(EmbedError::Failed("no embedding worker could be started on this node".into()));
            }
            let mine = st.queued.get(&cluster).copied().unwrap_or_default();
            if mine.inputs + cut.len() > self.cfg.queue_inputs
                || mine.bytes + bytes > self.cfg.queue_bytes
                || st.node.bytes + bytes > self.cfg.node_queue_bytes
                || st.node.tokens + tokens > self.cfg.node_queue_tokens
            {
                return Err(EmbedError::QueueFull);
            }
            let mut waits = Vec::new();
            let mut job: Vec<String> = Vec::new();
            let (mut job_tokens, mut job_bytes) = (0, 0);
            let mut flush = |job: &mut Vec<String>, job_tokens: &mut usize, job_bytes: &mut usize, st: &mut State| {
                let (tx, rx) = tokio::sync::oneshot::channel();
                st.push(Job {
                    owner: owner.clone(),
                    cluster: cluster.clone(),
                    kind,
                    texts: std::mem::take(job),
                    tokens: std::mem::take(job_tokens),
                    bytes: std::mem::take(job_bytes),
                    done: tx,
                    cancelled: cancelled.clone(),
                });
                waits.push(rx);
            };
            for t in cut {
                let n = tokens_of(prefix, &t);
                if !job.is_empty() && job_tokens + n > self.cfg.job_tokens {
                    flush(&mut job, &mut job_tokens, &mut job_bytes, &mut st);
                }
                job_tokens += n;
                job_bytes += t.len();
                job.push(t);
            }
            flush(&mut job, &mut job_tokens, &mut job_bytes, &mut st);
            st.last_used = Some(Instant::now());
            break waits;
        };
        // From here, the call going away (done, failed or dropped) skips its jobs not started.
        let _cancel = CancelOnDrop(cancelled.clone());
        drop(texts);
        self.shared.work.notify_all();
        let all = async {
            let mut out = Vec::new();
            for rx in waits {
                match rx.await {
                    Ok(Ok(v)) => out.extend(v),
                    Ok(Err(e)) if e == PANICKED => return Err(EmbedError::Failed(PANICKED.into())),
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "embedding failed");
                        return Err(EmbedError::Failed("it failed on this input".into()));
                    }
                    Err(_) => return Err(EmbedError::Failed("its worker stopped".into())),
                }
            }
            Ok(out)
        };
        // On a timeout the guard skips the jobs not started; running ones finish (and are
        // metered).
        until(deadline, all).await.unwrap_or(Err(EmbedError::TimedOut(timeout)))
    }
}

/// A worker: takes jobs in owner and cluster turns for as long as the process runs; drops the
/// model after `idle` with nothing queued, running or loading.
fn work(shared: &Shared, idle: Duration) {
    loop {
        let (job, model) = {
            let mut st = shared.state.lock();
            loop {
                if let Some(job) = st.pop() {
                    if job.cancelled.load(Ordering::SeqCst) || job.done.is_closed() {
                        continue;
                    }
                    let Some(m) = st.model.clone() else {
                        let _ = job.done.send(Err("the model isn't loaded".into()));
                        continue;
                    };
                    st.running += 1;
                    st.running_bytes += m.job_bytes();
                    break (job, m);
                }
                let idle_for = st.last_used.map_or(Duration::MAX, |t| t.elapsed());
                if st.running == 0 && st.load.is_none() && st.model.is_some() && idle_for >= idle {
                    st.model = None;
                    tracing::info!(idle_s = idle.as_secs(), "embedding model dropped");
                }
                let wait = if st.model.is_some() { idle.saturating_sub(idle_for).max(Duration::from_millis(10)) } else { Duration::from_secs(3600) };
                shared.work.wait_for(&mut st, wait);
            }
        };
        let before = thread_cpu();
        let result = match catch_unwind(AssertUnwindSafe(|| model.embed(job.kind.prefix(), &job.texts))) {
            Ok(r) => r,
            Err(_) => {
                tracing::error!(cluster = &*job.cluster, "an embedding job panicked");
                Err(PANICKED.to_string())
            }
        };
        let cpu = thread_cpu().saturating_sub(before);
        *shared.cpu.lock().entry(job.cluster.to_string()).or_default() += cpu;
        {
            let mut st = shared.state.lock();
            st.running -= 1;
            st.running_bytes = st.running_bytes.saturating_sub(model.job_bytes());
            st.last_used = Some(Instant::now());
        }
        drop(model);
        let _ = job.done.send(result);
    }
}

#[async_trait::async_trait]
impl Embedder for EmbedPool {
    fn available(&self) -> bool {
        self.cfg.source != ModelSource::Off
    }

    async fn embed(&self, tenant: Tenant<'_>, kind: InputKind, texts: Vec<String>, timeout: Duration) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.embed_for(tenant, kind, texts, timeout).await
    }
}

/// Fetches what's missing, checks the rest once per process (refetching a cached file whose
/// digest is wrong), and loads the model, counting its memory from when it reads the weights.
async fn load(shared: &Arc<Shared>, cfg: &EmbedConfig) -> Result<Arc<dyn Model>, LoadFailure> {
    let dir = weights_dir(shared, cfg).await?;
    // The weights read once, then copied out into the model (the file's pages and the model's
    // arrays alive together), and the tokenizer.
    shared.state.lock().load_bytes = 2 * cfg.files.iter().map(|f| f.size).sum::<u64>();
    let loader = cfg.loader.0.clone();
    match tokio::task::spawn_blocking(move || loader(&dir)).await {
        Ok(Ok(m)) => Ok(m),
        Ok(Err(e)) => Err(LoadFailure::new("the model couldn't be loaded on this node", e)),
        Err(e) => Err(LoadFailure::new("the model couldn't be loaded on this node", format!("loading the model panicked: {e}"))),
    }
}

async fn weights_dir(shared: &Arc<Shared>, cfg: &EmbedConfig) -> Result<PathBuf, LoadFailure> {
    const DOWNLOAD: &str = "the model's files couldn't be downloaded on this node";
    let (dir, base) = match &cfg.source {
        ModelSource::Off => return Err(LoadFailure::new("the model is off on this node", "off")),
        ModelSource::Dir(d) => (d.clone(), None),
        ModelSource::Fetch { base } => (cfg.cache_dir.join("multilingual-e5-small").join(search_embed::REVISION), Some(base.as_str())),
    };
    let mut fetched = match base {
        Some(base) => fetch(base, &dir, &cfg.files, cfg.fetch_read_timeout).await.map_err(|e| LoadFailure::new(DOWNLOAD, e))?,
        None => Vec::new(),
    };
    if cfg.unchecked || shared.verified.lock().contains(&dir) {
        return Ok(dir);
    }
    // Files just fetched were checked as they were written.
    let to_check: Vec<ModelFile> = cfg.files.iter().filter(|f| !fetched.contains(&f.name)).cloned().collect();
    let d = dir.clone();
    let bad = match tokio::task::spawn_blocking(move || mismatched(&d, &to_check)).await {
        Ok(Ok(bad)) => bad,
        Ok(Err(e)) => return Err(LoadFailure::new("the model's files couldn't be read on this node", e)),
        Err(e) => return Err(LoadFailure::new("the model's files couldn't be read on this node", e.to_string())),
    };
    if !bad.is_empty() {
        let names: Vec<&str> = bad.iter().map(|(n, _)| n.as_str()).collect();
        let Some(base) = base else {
            return Err(LoadFailure::new(
                "the model's files don't match their pinned digests on this node",
                format!("{} in {} don't match their pinned SHA-256 digests", names.join(", "), dir.display()),
            ));
        };
        for (name, checked) in &bad {
            // Only the file that was checked: another process sharing the cache may have
            // replaced it with a good copy meanwhile.
            let path = dir.join(name);
            if checked.is_some() && stamp(&path) == *checked {
                tracing::warn!(file = name, dir = %dir.display(), "a cached model file doesn't match its pinned digest: fetching it again");
                let _ = tokio::fs::remove_file(&path).await;
            }
        }
        fetched.extend(fetch(base, &dir, &cfg.files, cfg.fetch_read_timeout).await.map_err(|e| LoadFailure::new(DOWNLOAD, e))?);
        // A file another process put in place meanwhile is checked again on the next load.
        let d = dir.clone();
        let again: Vec<ModelFile> = cfg.files.iter().filter(|f| !fetched.contains(&f.name)).cloned().collect();
        let still = tokio::task::spawn_blocking(move || mismatched(&d, &again)).await.map_err(|e| LoadFailure::new(DOWNLOAD, e.to_string()))?.map_err(|e| LoadFailure::new(DOWNLOAD, e))?;
        if !still.is_empty() {
            return Err(LoadFailure::new(DOWNLOAD, format!("{} in {} still don't match their digests", still.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", "), dir.display())));
        }
    }
    shared.verified.lock().push(dir.clone());
    Ok(dir)
}

/// What identifies one version of a file: its inode, size and modification time.
type Stamp = Option<(u64, u64, i64, i64)>;

fn stamp(path: &Path) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).ok()?;
    Some((m.ino(), m.len(), m.mtime(), m.mtime_nsec()))
}

/// Removes download parts (`<file>.part.<pid>.<n>`) in `dir` older than an hour, whichever
/// process left them (one that died mid-download).
async fn sweep_parts(dir: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else { return };
    while let Ok(Some(e)) = entries.next_entry().await {
        if !e.file_name().to_string_lossy().contains(".part.") {
            continue;
        }
        let old = e.metadata().await.ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).is_some_and(|age| age > Duration::from_secs(3600));
        if old {
            let _ = tokio::fs::remove_file(e.path()).await;
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The names of `files` in `dir` whose digest isn't the pinned one (or that can't be read),
/// each with the stamp of the version that was checked (`None`: missing).
fn mismatched(dir: &Path, files: &[ModelFile]) -> Result<Vec<(String, Stamp)>, String> {
    use sha2::Digest;
    use std::io::Read;
    let mut bad = Vec::new();
    for f in files {
        let path = dir.join(&f.name);
        let checked = stamp(&path);
        let Ok(mut file) = std::fs::File::open(&path) else {
            bad.push((f.name.clone(), None));
            continue;
        };
        let mut h = sha2::Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = file.read(&mut buf).map_err(|e| format!("{}: {e}", path.display()))?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        if hex(&h.finalize()) != f.sha256 {
            bad.push((f.name.clone(), checked));
        }
    }
    Ok(bad)
}

/// Downloads the files missing from `dir` from `base`: each into a file of its own
/// (`<name>.part.<pid>.<n>`), checked for size and SHA-256 as it's written, then renamed into
/// place (a part whose rename fails is removed). Parts older than an hour, left by a process
/// that died mid-download, are removed first. A connection that stalls for `read_timeout`
/// fails. Returns the names fetched.
async fn fetch(base: &str, dir: &Path, files: &[ModelFile], read_timeout: Duration) -> Result<Vec<String>, String> {
    use sha2::Digest;
    use tokio::io::AsyncWriteExt;
    static ATTEMPT: AtomicU64 = AtomicU64::new(0);
    let missing: Vec<&ModelFile> = files.iter().filter(|f| !dir.join(&f.name).is_file()).collect();
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    tokio::fs::create_dir_all(dir).await.map_err(|e| format!("{}: {e}", dir.display()))?;
    sweep_parts(dir).await;
    let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(30)).read_timeout(read_timeout).build().map_err(|e| e.to_string())?;
    let mut fetched = Vec::new();
    for f in missing {
        let path = dir.join(&f.name);
        let url = format!("{}/{}", base.trim_end_matches('/'), f.name);
        tracing::info!(%url, "fetching the embedding model");
        let mut resp = client.get(&url).send().await.and_then(|r| r.error_for_status()).map_err(|e| format!("{url}: {e}"))?;
        let part = dir.join(format!("{}.part.{}.{}", f.name, std::process::id(), ATTEMPT.fetch_add(1, Ordering::Relaxed)));
        let mut out = tokio::fs::File::create(&part).await.map_err(|e| format!("{}: {e}", part.display()))?;
        let mut h = sha2::Sha256::new();
        let mut got: u64 = 0;
        let result = async {
            while let Some(chunk) = resp.chunk().await.map_err(|e| format!("{url}: {e}"))? {
                got += chunk.len() as u64;
                if got > f.size {
                    return Err(format!("{url} is larger than the pinned file's {} bytes", f.size));
                }
                h.update(&chunk);
                out.write_all(&chunk).await.map_err(|e| format!("{}: {e}", part.display()))?;
            }
            out.sync_all().await.map_err(|e| format!("{}: {e}", part.display()))?;
            let hex = hex(&h.finalize());
            if got != f.size || hex != f.sha256 {
                return Err(format!("{url}: {got} bytes with SHA-256 {hex}, expected {} bytes with {}", f.size, f.sha256));
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(e);
        }
        if let Err(e) = tokio::fs::rename(&part, &path).await {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(format!("{}: {e}", path.display()));
        }
        fetched.push(f.name.clone());
    }
    Ok(fetched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn cpu_lists() {
        assert_eq!(parse_cpus("0-2, 18-19,7").unwrap(), [0, 1, 2, 18, 19, 7]);
        assert!(parse_cpus("3-1").is_err());
        assert!(parse_cpus("0-100000").is_err());
        assert!(parse_cpus("4096").is_err());
    }

    /// A model that records what it embeds; `block` holds a job until released, `panic`
    /// panics.
    #[derive(Default)]
    struct Fake {
        seen: Mutex<Vec<String>>,
        gate: Mutex<bool>,
        opened: Condvar,
        started: AtomicBool,
    }

    impl Model for Fake {
        fn cut(&self, text: &str) -> String {
            text.to_string()
        }

        fn embed(&self, _prefix: &str, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            for t in texts {
                if t == "panic" {
                    panic!("a fake model's panic");
                }
                if t == "block" {
                    self.started.store(true, Ordering::SeqCst);
                    let mut open = self.gate.lock();
                    while !*open {
                        self.opened.wait(&mut open);
                    }
                }
                self.seen.lock().push(t.clone());
            }
            Ok(texts.iter().map(|_| vec![1.0; 4]).collect())
        }

        fn bytes(&self) -> u64 {
            1000
        }

        fn job_bytes(&self) -> u64 {
            10
        }
    }

    fn pool_with(load: impl Fn() -> Result<Arc<dyn Model>, String> + Send + Sync + 'static) -> Arc<EmbedPool> {
        let dir = std::env::temp_dir();
        let mut cfg = EmbedConfig::new(ModelSource::Dir(dir.clone()), dir);
        cfg.unchecked = true;
        cfg.workers = 1;
        cfg.job_tokens = 1;
        // Unchecked; its size is what a load in progress counts.
        cfg.files = vec![ModelFile { name: "weights".into(), sha256: String::new(), size: 500 }];
        cfg.loader = Loader(Arc::new(move |_: &Path| load()));
        EmbedPool::new(cfg)
    }

    fn pool(fake: Arc<Fake>, loads: Arc<AtomicUsize>, load_ms: u64) -> Arc<EmbedPool> {
        pool_with(move || {
            loads.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(load_ms));
            Ok(fake.clone() as Arc<dyn Model>)
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_load_is_answered_from_its_backoff_without_loading_again() {
        let loads = Arc::new(AtomicUsize::new(0));
        let l = loads.clone();
        let p = pool_with(move || {
            l.fetch_add(1, Ordering::SeqCst);
            Err("no such file".into())
        });
        for _ in 0..2 {
            let e = p.embed_for(tenant("o", "c"), InputKind::Other, vec!["x".into()], Duration::from_secs(10)).await.unwrap_err();
            let EmbedError::Failed(m) = e else { panic!("{e:?}") };
            assert!(m.starts_with("the model couldn't be loaded on this node; it's tried again in "), "{m}");
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        // Nothing is counted for a load that isn't running.
        assert_eq!(p.resident_bytes(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn searches_go_before_their_clusters_ingest_and_dropped_calls_are_skipped() {
        let fake = Arc::new(Fake::default());
        let p = pool(fake.clone(), Arc::new(AtomicUsize::new(0)), 0);
        let call = |kind: InputKind, texts: &[&str]| {
            let (p, texts) = (p.clone(), texts.iter().map(|t| t.to_string()).collect::<Vec<_>>());
            tokio::spawn(async move { p.embed_for(tenant("o", "c"), kind, texts, Duration::from_secs(30)).await })
        };
        let first = call(InputKind::Ingest, &["block"]);
        while !fake.started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let bulk = call(InputKind::Ingest, &["i1", "i2", "i3"]);
        queued(&p, 3).await;
        // A caller that goes away: its jobs are skipped.
        let gone = call(InputKind::Ingest, &["gone"]);
        queued(&p, 4).await;
        gone.abort();
        let _ = gone.await;
        let query = call(InputKind::Search, &["q"]);
        queued(&p, 5).await;
        *fake.gate.lock() = true;
        fake.opened.notify_all();
        for h in [first, bulk, query] {
            h.await.unwrap().unwrap();
        }
        assert_eq!(*fake.seen.lock(), ["block", "q", "i1", "i2", "i3"]);
    }

    fn tenant<'a>(owner: &'a str, cluster: &'a str) -> Tenant<'a> {
        Tenant { cluster, owner: Some(owner) }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn callers_that_give_up_start_one_load_between_them() {
        let loads = Arc::new(AtomicUsize::new(0));
        let p = pool(Arc::new(Fake::default()), loads.clone(), 300);
        let calls: Vec<_> = (0..20)
            .map(|i| {
                let p = p.clone();
                tokio::spawn(async move { p.embed_for(tenant("o", &format!("c{i}")), InputKind::Other, vec!["x".into()], Duration::from_millis(1)).await })
            })
            .collect();
        for c in calls {
            assert!(matches!(c.await.unwrap(), Err(EmbedError::TimedOut(_))));
        }
        // The load they started goes on, and counts while it does (twice the files' size).
        assert!(p.resident_bytes() == 1000 || p.loaded());
        let v = p.embed_for(tenant("o", "c"), InputKind::Other, vec!["x".into()], Duration::from_secs(10)).await.unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    async fn queued(p: &EmbedPool, n: usize) {
        for _ in 0..500 {
            if p.queued_inputs() == n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("never {n} queued");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn turns_go_by_owner_then_by_cluster() {
        let fake = Arc::new(Fake::default());
        let p = pool(fake.clone(), Arc::new(AtomicUsize::new(0)), 0);
        let call = |owner: &'static str, cluster: &'static str, texts: &[&str]| {
            let (p, texts) = (p.clone(), texts.iter().map(|t| t.to_string()).collect::<Vec<_>>());
            tokio::spawn(async move { p.embed_for(tenant(owner, cluster), InputKind::Other, texts, Duration::from_secs(30)).await })
        };
        // The one worker is held on a job of owner x while the others queue.
        let first = call("x", "x0", &["block"]);
        while !fake.started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let a = call("x", "x1", &["x1a", "x1b"]);
        queued(&p, 2).await;
        let b = call("x", "x2", &["x2a", "x2b"]);
        queued(&p, 4).await;
        let c = call("y", "y1", &["y1"]);
        queued(&p, 5).await;
        *fake.gate.lock() = true;
        fake.opened.notify_all();
        for h in [first, a, b, c] {
            h.await.unwrap().unwrap();
        }
        // y's one cluster gets every other turn though x has two; x's clusters alternate.
        assert_eq!(*fake.seen.lock(), ["block", "x1a", "y1", "x2a", "x1b", "x2b"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_job_that_panics_answers_an_internal_error_and_the_worker_goes_on() {
        let p = pool(Arc::new(Fake::default()), Arc::new(AtomicUsize::new(0)), 0);
        let e = p.embed_for(tenant("o", "c"), InputKind::Other, vec!["panic".into()], Duration::from_secs(10)).await.unwrap_err();
        assert_eq!(e, EmbedError::Failed(PANICKED.into()));
        assert_eq!(p.embed_for(tenant("o", "c"), InputKind::Other, vec!["fine".into()], Duration::from_secs(10)).await.unwrap().len(), 1);
        assert_eq!(p.workers(), 1);
        // Nothing left counted as running or queued.
        assert_eq!(p.resident_bytes(), 1000);
    }

    /// A one-request-per-connection HTTP server of the files in `dir`, counting requests by
    /// path.
    async fn serve(dir: PathBuf) -> (String, Arc<Mutex<HashMap<String, usize>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(Mutex::new(HashMap::new()));
        let h = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else { return };
                let (dir, h) = (dir.clone(), h.clone());
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        let Ok(n) = s.read(&mut buf).await else { return };
                        if n == 0 {
                            return;
                        }
                        req.extend_from_slice(&buf[..n]);
                    }
                    let line = String::from_utf8_lossy(&req).lines().next().unwrap_or_default().to_string();
                    let name = line.split(' ').nth(1).unwrap_or("/").trim_start_matches('/').to_string();
                    *h.lock().entry(name.clone()).or_default() += 1;
                    let body = std::fs::read(dir.join(&name)).unwrap_or_default();
                    let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(&body).await;
                });
            }
        });
        (format!("http://{addr}"), hits)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cached_file_with_the_wrong_digest_is_fetched_again() {
        use sha2::Digest;
        let root = std::env::temp_dir().join(format!("roll-embed-fetch-{}", std::process::id()));
        let origin = root.join("origin");
        search_embed::tiny::write(&origin).unwrap();
        let files: Vec<ModelFile> = [search_embed::WEIGHTS, search_embed::TOKENIZER]
            .iter()
            .map(|n| {
                let b = std::fs::read(origin.join(n)).unwrap();
                ModelFile { name: n.to_string(), sha256: hex(&sha2::Sha256::digest(&b)), size: b.len() as u64 }
            })
            .collect();
        let (base, hits) = serve(origin.clone()).await;
        let cache = root.join("cache");
        let dir = cache.join("multilingual-e5-small").join(search_embed::REVISION);
        std::fs::create_dir_all(&dir).unwrap();
        // The tokenizer is cached intact; the weights are cached corrupted.
        std::fs::copy(origin.join(search_embed::TOKENIZER), dir.join(search_embed::TOKENIZER)).unwrap();
        std::fs::write(dir.join(search_embed::WEIGHTS), b"not the weights").unwrap();
        let mut cfg = EmbedConfig::new(ModelSource::Fetch { base }, cache);
        cfg.workers = 1;
        cfg.files = files.clone();
        let p = EmbedPool::new(cfg);
        let v = p.embed_for(tenant("o", "c"), InputKind::Search, vec!["hello".into()], Duration::from_secs(30)).await.unwrap();
        assert_eq!(v[0].len(), search_embed::tiny::HIDDEN);
        let hits = hits.lock().clone();
        assert_eq!((hits.get(search_embed::WEIGHTS), hits.get(search_embed::TOKENIZER)), (Some(&1), None), "{hits:?}");
        assert!(mismatched(&dir, &files).unwrap().is_empty());
        // No partial download is left behind.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }
}
