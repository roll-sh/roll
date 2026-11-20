//! The node's embedding pool: `multilingual-e5-small` ([`search_embed`]) for every cluster the
//! node serves, behind the `elasticsearch` inference service's E5 endpoints
//! ([`search_es::xpack::inference::local`]).
//!
//! - **Weights** are the node's, never a tenant's: fetched once into the node's cache
//!   (`<cache>/multilingual-e5-small/<revision>/`) from the operator's URL (default: the pinned
//!   Hugging Face revision, [`search_embed::REPOSITORY`]), or read from a directory the operator
//!   names (air-gapped nodes), and checked against [`search_embed::FILES`]' SHA-256 digests
//!   before they're used. A download that fails leaves nothing behind; one a request stopped
//!   waiting for goes on, so the next request finds it.
//! - **Loaded lazily**, on the first input, and **dropped** after [`EmbedConfig::idle_unload`]
//!   with nothing to do: an idle node holds no model.
//! - **Workers**: [`EmbedConfig::workers`] threads (a third of the CPUs by default), optionally
//!   pinned to [`EmbedConfig::cpus`]. The model runs on the calling thread alone, so a job runs on
//!   one core and the thread's CPU clock measures it exactly. They start with the first input.
//! - **Fair shares**: a call's inputs queue under its cluster in jobs of [`JOB_INPUTS`]; a free
//!   worker takes the next cluster's next job, round-robin, so a cluster with a 10,000-document
//!   bulk waits its turn like one with a single query. Past [`EmbedConfig::queue_inputs`] inputs
//!   queued, a cluster's call is refused whole, as a deployment with a full queue refuses
//!   (429 `es_rejected_execution_exception`).
//! - **Caps**: at most [`EmbedConfig::max_inputs`] inputs per call and
//!   [`EmbedConfig::max_input_chars`] characters per input (400).
//! - **Memory**: the model and the batches running are [`EmbedPool::resident_bytes`], which the
//!   node counts against its memory budget like loaded clusters (`Node::enforce_memory`).
//! - **Metering**: each job's thread CPU time goes to its cluster ([`EmbedPool::take_cpu`]),
//!   which the control plane bills as inference CPU.

use parking_lot::{Condvar, Mutex};
use search_es::xpack::inference::local::{EmbedError, Embedder};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Inputs per job: what one worker takes at a time.
pub const JOB_INPUTS: usize = 8;

/// Where the model comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelSource {
    /// No model: E5 endpoints answer as on a cluster without ML nodes.
    Off,
    /// Fetched once from `base` (`<base>/<file>`) into the cache.
    Fetch { base: String },
    /// A directory holding [`search_embed::WEIGHTS`] and [`search_embed::TOKENIZER`] (checked
    /// against the digests like a download).
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
    /// Characters one input may have.
    pub max_input_chars: usize,
    /// Inputs a cluster may have queued (not yet running).
    pub queue_inputs: usize,
    /// Skip the digest check of a directory's files (tests' made-up models only).
    pub unchecked: bool,
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
            max_input_chars: 1 << 20,
            queue_inputs: 4096,
            unchecked: false,
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

/// Parses a CPU list (`0-5,18-23`).
pub fn parse_cpus(s: &str) -> anyhow::Result<Vec<usize>> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (usize, usize) = (a.trim().parse()?, b.trim().parse()?);
                anyhow::ensure!(a <= b, "bad CPU range [{part}]");
                out.extend(a..=b);
            }
            None => out.push(part.parse()?),
        }
    }
    Ok(out)
}

struct Job {
    cluster: Arc<str>,
    texts: Vec<String>,
    done: tokio::sync::oneshot::Sender<Result<Vec<Vec<f32>>, String>>,
    /// The caller stopped waiting.
    cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    model: Option<Arc<search_embed::E5>>,
    /// Queued jobs per cluster, and the clusters with jobs in turn order.
    queues: HashMap<Arc<str>, VecDeque<Job>>,
    turns: VecDeque<Arc<str>>,
    queued_inputs: HashMap<Arc<str>, usize>,
    /// Jobs running, and the activation memory they reserve.
    running: usize,
    running_bytes: u64,
    workers: usize,
    last_used: Option<Instant>,
}

impl State {
    /// The next cluster's next job.
    fn pop(&mut self) -> Option<Job> {
        let cluster = self.turns.pop_front()?;
        let q = self.queues.get_mut(&cluster)?;
        let job = q.pop_front()?;
        if q.is_empty() {
            self.queues.remove(&cluster);
        } else {
            self.turns.push_back(cluster.clone());
        }
        if let Some(n) = self.queued_inputs.get_mut(&cluster) {
            *n = n.saturating_sub(job.texts.len());
            if *n == 0 {
                self.queued_inputs.remove(&cluster);
            }
        }
        Some(job)
    }
}

struct Shared {
    state: Mutex<State>,
    work: Condvar,
    cpu: Mutex<HashMap<String, Duration>>,
}

/// The pool. Costs nothing while idle: no model, and workers asleep.
pub struct EmbedPool {
    cfg: EmbedConfig,
    shared: Arc<Shared>,
    /// One load (and fetch) at a time.
    loading: tokio::sync::Mutex<()>,
    /// Weights directories already checked against the digests (once per process).
    verified: Mutex<Vec<PathBuf>>,
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

/// Pins the calling thread to `cpus`.
fn pin(cpus: &[usize]) {
    if cpus.is_empty() {
        return;
    }
    // SAFETY: a zeroed `cpu_set_t` is an empty set; `CPU_SET` and `sched_setaffinity` only
    // touch the set we own.
    let ok = unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        for &c in cpus {
            libc::CPU_SET(c, &mut set);
        }
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0
    };
    if !ok {
        tracing::warn!(?cpus, "pinning an embedding worker failed");
    }
}

impl EmbedPool {
    pub fn new(cfg: EmbedConfig) -> Arc<EmbedPool> {
        Arc::new(EmbedPool {
            cfg,
            shared: Arc::new(Shared { state: Mutex::new(State::default()), work: Condvar::new(), cpu: Mutex::new(HashMap::new()) }),
            loading: tokio::sync::Mutex::new(()),
            verified: Mutex::new(Vec::new()),
        })
    }

    pub fn config(&self) -> &EmbedConfig {
        &self.cfg
    }

    /// Memory the model and the running batches hold.
    pub fn resident_bytes(&self) -> u64 {
        let st = self.shared.state.lock();
        st.model.as_ref().map_or(0, |m| m.bytes()) + st.running_bytes
    }

    /// Whether the model is in memory.
    pub fn loaded(&self) -> bool {
        self.shared.state.lock().model.is_some()
    }

    /// Worker threads started.
    pub fn workers(&self) -> usize {
        self.shared.state.lock().workers
    }

    /// CPU time each cluster's inputs took since the last call, by cluster name.
    pub fn take_cpu(&self) -> Vec<(String, Duration)> {
        let mut v: Vec<(String, Duration)> = std::mem::take(&mut *self.shared.cpu.lock()).into_iter().collect();
        v.sort();
        v
    }

    /// The weights directory, fetching into the cache first when needed.
    async fn weights_dir(&self) -> Result<PathBuf, String> {
        let dir = match &self.cfg.source {
            ModelSource::Off => return Err("the model is off".into()),
            ModelSource::Dir(d) => d.clone(),
            ModelSource::Fetch { base } => {
                let dir = self.cfg.cache_dir.join("multilingual-e5-small").join(search_embed::REVISION);
                if search_embed::FILES.iter().any(|(f, _, _)| !dir.join(f).is_file()) {
                    // On its own task: a caller that stops waiting doesn't stop the download.
                    let (base, d) = (base.clone(), dir.clone());
                    tokio::spawn(async move { fetch(&base, &d).await }).await.map_err(|e| e.to_string())??;
                }
                dir
            }
        };
        if !self.cfg.unchecked && !self.verified.lock().contains(&dir) {
            let d = dir.clone();
            tokio::task::spawn_blocking(move || verify(&d)).await.map_err(|e| e.to_string())??;
            self.verified.lock().push(dir.clone());
        }
        Ok(dir)
    }

    /// The model, loading it first if needed.
    async fn model(&self) -> Result<Arc<search_embed::E5>, EmbedError> {
        if let Some(m) = self.shared.state.lock().model.clone() {
            return Ok(m);
        }
        let _one = self.loading.lock().await;
        if let Some(m) = self.shared.state.lock().model.clone() {
            return Ok(m);
        }
        let dir = self.weights_dir().await.map_err(|e| {
            tracing::warn!(error = %e, "the embedding model's weights aren't available");
            EmbedError::Failed("its weights aren't available on this node".into())
        })?;
        let started = Instant::now();
        let m = tokio::task::spawn_blocking(move || search_embed::E5::load(&dir)).await.map_err(|e| EmbedError::Failed(e.to_string()))?.map_err(|e| {
            tracing::warn!(error = %e, "loading the embedding model failed");
            EmbedError::Failed("it couldn't be loaded".into())
        })?;
        let m = Arc::new(m);
        tracing::info!(bytes = m.bytes(), ms = started.elapsed().as_millis() as u64, "embedding model loaded");
        let mut st = self.shared.state.lock();
        st.model = Some(m.clone());
        st.last_used = Some(Instant::now());
        Ok(m)
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

    /// Embeds `texts` for `cluster`; see [`Embedder::embed`].
    pub async fn embed_for(&self, cluster: &str, texts: Vec<String>, timeout: Duration) -> Result<Vec<Vec<f32>>, EmbedError> {
        if self.cfg.source == ModelSource::Off {
            return Err(EmbedError::Off);
        }
        if texts.len() > self.cfg.max_inputs {
            return Err(EmbedError::TooLarge(format!("[{}] inputs is more than the [{}] one inference call may carry on this node", texts.len(), self.cfg.max_inputs)));
        }
        if let Some((i, n)) = texts.iter().map(|t| t.chars().count()).enumerate().find(|(_, n)| *n > self.cfg.max_input_chars) {
            return Err(EmbedError::TooLarge(format!("input [{i}] has [{n}] characters, more than the [{}] an input may have on this node", self.cfg.max_input_chars)));
        }
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let deadline = Instant::now() + timeout;
        tokio::time::timeout(timeout, self.model()).await.map_err(|_| EmbedError::TimedOut(timeout))??;
        let cluster: Arc<str> = Arc::from(cluster);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut waits = Vec::new();
        {
            let mut st = self.shared.state.lock();
            let queued = st.queued_inputs.get(&cluster).copied().unwrap_or(0);
            if queued + texts.len() > self.cfg.queue_inputs {
                return Err(EmbedError::QueueFull);
            }
            let mut texts = texts;
            while !texts.is_empty() {
                let rest = texts.split_off(texts.len().min(JOB_INPUTS));
                let (tx, rx) = tokio::sync::oneshot::channel();
                *st.queued_inputs.entry(cluster.clone()).or_default() += texts.len();
                st.queues.entry(cluster.clone()).or_default().push_back(Job { cluster: cluster.clone(), texts, done: tx, cancelled: cancelled.clone() });
                waits.push(rx);
                texts = rest;
            }
            if !st.turns.contains(&cluster) {
                st.turns.push_back(cluster.clone());
            }
            self.start_workers(&mut st);
        }
        self.shared.work.notify_all();
        let all = async {
            let mut out = Vec::new();
            for rx in waits {
                match rx.await {
                    Ok(Ok(v)) => out.extend(v),
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "embedding failed");
                        return Err(EmbedError::Failed("it failed on this input".into()));
                    }
                    Err(_) => return Err(EmbedError::Failed("its worker stopped".into())),
                }
            }
            Ok(out)
        };
        let left = deadline.saturating_duration_since(Instant::now());
        let r = tokio::time::timeout(left, all).await.unwrap_or(Err(EmbedError::TimedOut(timeout)));
        if r.is_err() {
            // Jobs not started yet are skipped; running ones finish (and are metered).
            cancelled.store(true, Ordering::SeqCst);
        }
        r
    }
}

/// A worker: takes jobs in cluster turns for as long as the process runs; drops the model after
/// `idle` with nothing queued or running.
fn work(shared: &Shared, idle: Duration) {
    loop {
        let (job, model) = {
            let mut st = shared.state.lock();
            loop {
                if let Some(job) = st.pop() {
                    if job.cancelled.load(Ordering::SeqCst) {
                        continue;
                    }
                    let Some(m) = st.model.clone() else {
                        let _ = job.done.send(Err("the model isn't loaded".into()));
                        continue;
                    };
                    st.running += 1;
                    st.running_bytes += m.batch_bytes_max();
                    break (job, m);
                }
                let idle_for = st.last_used.map_or(Duration::MAX, |t| t.elapsed());
                if st.running == 0 && st.model.is_some() && idle_for >= idle {
                    st.model = None;
                    tracing::info!(idle_s = idle.as_secs(), "embedding model dropped");
                }
                let wait = if st.model.is_some() { idle.saturating_sub(idle_for).max(Duration::from_millis(10)) } else { Duration::from_secs(3600) };
                shared.work.wait_for(&mut st, wait);
            }
        };
        let before = thread_cpu();
        let texts: Vec<&str> = job.texts.iter().map(String::as_str).collect();
        let result = model.embed(&texts).map_err(|e| e.to_string());
        let cpu = thread_cpu().saturating_sub(before);
        *shared.cpu.lock().entry(job.cluster.to_string()).or_default() += cpu;
        {
            let mut st = shared.state.lock();
            st.running -= 1;
            st.running_bytes = st.running_bytes.saturating_sub(model.batch_bytes_max());
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

    async fn embed(&self, cluster: &str, texts: Vec<String>, timeout: Duration) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.embed_for(cluster, texts, timeout).await
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Checks the files in `dir` against the pinned digests.
fn verify(dir: &Path) -> Result<(), String> {
    use sha2::Digest;
    use std::io::Read;
    for (name, digest, _) in search_embed::FILES {
        let path = dir.join(name);
        let mut f = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut h = sha2::Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf).map_err(|e| format!("{}: {e}", path.display()))?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        let got = hex(&h.finalize());
        if got != digest {
            return Err(format!("{} has SHA-256 {got}, expected {digest}", path.display()));
        }
    }
    Ok(())
}

/// Downloads the missing files from `base` into `dir`: each to a temporary name, checked
/// (size and SHA-256) while it's written, then renamed into place.
async fn fetch(base: &str, dir: &Path) -> Result<(), String> {
    use sha2::Digest;
    use tokio::io::AsyncWriteExt;
    tokio::fs::create_dir_all(dir).await.map_err(|e| format!("{}: {e}", dir.display()))?;
    let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(30)).build().map_err(|e| e.to_string())?;
    for (name, digest, size) in search_embed::FILES {
        let path = dir.join(name);
        if path.is_file() {
            continue;
        }
        let url = format!("{}/{name}", base.trim_end_matches('/'));
        tracing::info!(%url, "fetching the embedding model");
        let mut resp = client.get(&url).send().await.and_then(|r| r.error_for_status()).map_err(|e| format!("{url}: {e}"))?;
        let part = dir.join(format!("{name}.part"));
        let mut out = tokio::fs::File::create(&part).await.map_err(|e| format!("{}: {e}", part.display()))?;
        let mut h = sha2::Sha256::new();
        let mut got: u64 = 0;
        let result = async {
            while let Some(chunk) = resp.chunk().await.map_err(|e| format!("{url}: {e}"))? {
                got += chunk.len() as u64;
                if got > size {
                    return Err(format!("{url} is larger than the pinned file's {size} bytes"));
                }
                h.update(&chunk);
                out.write_all(&chunk).await.map_err(|e| format!("{}: {e}", part.display()))?;
            }
            out.sync_all().await.map_err(|e| format!("{}: {e}", part.display()))?;
            let hex = hex(&h.finalize());
            if got != size || hex != digest {
                return Err(format!("{url}: {got} bytes with SHA-256 {hex}, expected {size} bytes with {digest}"));
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(e);
        }
        tokio::fs::rename(&part, &path).await.map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_lists() {
        assert_eq!(parse_cpus("0-2, 18-19,7").unwrap(), [0, 1, 2, 18, 19, 7]);
        assert!(parse_cpus("3-1").is_err());
    }
}
