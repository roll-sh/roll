//! Running suites: discovery, scheduling, the test lifecycle and step execution.
//!
//! Each test runs like `ESClientYamlSuiteTestCase.test()`: prerequisites of setup, teardown and
//! the test are evaluated; setup runs; the stash is cleared; the test's steps run until the first
//! failure; teardown always runs; then the cluster is wiped (or the test had a fresh cluster).
//!
//! Against a single shared cluster (a real Elasticsearch) files run one at a time. When a
//! [`ClusterHook`] can create isolated clusters (roll's control API, or any endpoint returning a
//! URL and credentials), or a [`SpawnCommand`] starts one server process per cluster, files run in
//! parallel on `jobs` workers, each with its own cluster and keep-alive connection. Several
//! `--url`s are the third way: one worker per (shared, wiped) cluster.

use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use bytes::Bytes;
use regex::Regex;
use tokio::sync::Mutex;

use crate::assertions::{self, Cmp, Mismatch};
use crate::http::{Client, Endpoint, Response, basic_auth};
use crate::regex_java::RegexCache;
use crate::report::{Failure, Phase, Report, SkipEntry, Status, TargetSummary, TestResult, glob_match};
use crate::spawn::{SpawnCommand, Spawned};
use crate::spec::{ApiSpec, MethodStrategy};
use crate::stash::{Stash, evaluate};
use crate::suite::{AssertOp, Body, DoStep, ParseError, Step, StepKind, Suite, TestSection};
use crate::target::{Flavor, TargetInfo};
use crate::value::{Map, Value};
use crate::wipe::{WipeMode, wipe};
use crate::xcontent::{Format, MediaType};

/// A directory of `.yml` files, with an optional label prefixed to its test ids.
#[derive(Debug, Clone)]
pub struct SuiteRoot {
    pub label: String,
    pub path: PathBuf,
}

impl SuiteRoot {
    /// Parses `path` or `label=path`.
    pub fn parse(s: &str) -> SuiteRoot {
        match s.split_once('=') {
            Some((label, path)) if !label.is_empty() && !label.contains('/') => {
                SuiteRoot { label: label.to_owned(), path: PathBuf::from(path) }
            }
            _ => SuiteRoot { label: String::new(), path: PathBuf::from(s) },
        }
    }
}

/// Creates (and deletes) isolated clusters for parallel runs.
///
/// `POST <create_url>` with `{"name": "...", "flavor"?: "..."}` must answer with JSON holding
/// `url` (or `endpoint`) and optionally `username`/`password`, `api_key`, `headers` (an object),
/// and `delete_url` or `id` (deleted with `DELETE <create_url>/<id>`); the fields may also be
/// nested under `cluster`.
#[derive(Debug, Clone)]
pub struct ClusterHook {
    pub create_url: String,
    pub headers: Vec<(String, String)>,
}

/// Run configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// The cluster to test (optional with a cluster hook).
    pub url: Option<String>,
    /// Without a hook: more clusters like `url` (say, several Elasticsearch containers); every
    /// cluster gets one worker and files are spread across them.
    pub more_urls: Vec<String>,
    pub suites: Vec<SuiteRoot>,
    pub api_spec: PathBuf,
    /// Only run tests whose id matches.
    pub filter: Option<Regex>,
    /// Only run these test ids (compared without trailing whitespace), on top of `filter`: a
    /// baseline, or the failures of an earlier report.
    pub only_ids: Option<HashSet<String>>,
    pub skip_list: Vec<SkipEntry>,
    pub jobs: usize,
    pub flavor: Option<Flavor>,
    pub claimed_features: HashSet<String>,
    /// `None` picks `full` for a shared cluster and `light` with a cluster hook.
    pub wipe: Option<WipeMode>,
    pub cluster_hook: Option<ClusterHook>,
    /// Instead of a hook: start a server process per isolated cluster.
    pub spawn: Option<SpawnCommand>,
    /// With a hook or spawn: a fresh cluster for every test instead of every file.
    pub fresh_per_test: bool,
    pub timeout: Duration,
    pub method_strategy: MethodStrategy,
    /// Add `error_trace=true` to requests, as the Java runner does.
    pub error_trace: bool,
    /// The content type of request bodies the test doesn't type itself.
    pub content_type: ContentTypes,
    /// Seeds the per-test generators of the random strategies (content types, paths, methods).
    pub seed: u64,
    pub xpack: Option<bool>,
    pub serverless: bool,
    pub feature_flags: HashSet<String>,
    /// Extra headers for every request (authentication).
    pub headers: Vec<(String, String)>,
    pub progress: bool,
    /// Log every request of every `do` (method, target, status, time) to stderr.
    pub trace: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            url: None,
            more_urls: Vec::new(),
            suites: Vec::new(),
            api_spec: PathBuf::new(),
            filter: None,
            only_ids: None,
            skip_list: Vec::new(),
            jobs: 1,
            flavor: None,
            claimed_features: HashSet::new(),
            wipe: None,
            cluster_hook: None,
            spawn: None,
            fresh_per_test: false,
            timeout: Duration::from_secs(90),
            method_strategy: MethodStrategy::Natural,
            error_trace: true,
            content_type: ContentTypes::Json,
            seed: 0,
            xpack: None,
            serverless: false,
            feature_flags: HashSet::new(),
            headers: Vec::new(),
            progress: false,
            trace: false,
        }
    }
}

impl Config {
    /// Whether clusters are created per file (or test): a hook or spawned processes.
    pub fn isolated(&self) -> bool {
        self.cluster_hook.is_some() || self.spawn.is_some()
    }

    /// Whether the test with this id is selected by `filter` and `only_ids`.
    pub fn selects(&self, id: &str) -> bool {
        self.filter.as_ref().is_none_or(|f| f.is_match(id)) && self.only_ids.as_ref().is_none_or(|ids| ids.contains(id.trim_end()))
    }
}

/// How request bodies are encoded when the test doesn't set `Content-Type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContentTypes {
    /// Always JSON.
    #[default]
    Json,
    /// One type; multi-document bodies (bulk, msearch) use it if it is JSON or Smile and JSON
    /// otherwise, as Elasticsearch only streams those two.
    Fixed(MediaType),
    /// What the Java runner does: a random type per request (any of the eight `XContentType`s for
    /// one body, JSON or Smile for several), from a generator seeded by [`Config::seed`] and the
    /// test id so a run is reproducible.
    Random,
}

impl ContentTypes {
    /// Parses `json`, `yaml`, `cbor`, `smile`, `vnd-<type>` or `random`.
    pub fn parse(s: &str) -> Option<ContentTypes> {
        match s {
            "json" => Some(ContentTypes::Json),
            "random" => Some(ContentTypes::Random),
            other => MediaType::parse(other).map(ContentTypes::Fixed),
        }
    }

    pub fn describe(self) -> String {
        match self {
            ContentTypes::Json => "json".into(),
            ContentTypes::Fixed(m) => m.mime().into(),
            ContentTypes::Random => "random".into(),
        }
    }

    fn pick(self, multi: bool, rng: &mut u64) -> MediaType {
        match self {
            ContentTypes::Json => MediaType::JSON,
            ContentTypes::Fixed(m) if multi => {
                if m.format == Format::Smile {
                    MediaType::STREAMING[1]
                } else {
                    MediaType::JSON
                }
            }
            ContentTypes::Fixed(m) => m,
            ContentTypes::Random => {
                let choices: &[MediaType] = if multi { &MediaType::STREAMING } else { &MediaType::ALL };
                choices[(splitmix64(rng) % choices.len() as u64) as usize]
            }
        }
    }
}

/// SplitMix64: a tiny, well-mixed generator for reproducible choices.
pub(crate) fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A per-test generator state: FNV-1a of the test id, mixed with the seed.
fn test_rng(seed: u64, id: &str) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for b in id.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01B3);
    }
    h ^ seed.rotate_left(17)
}

/// A file to run, with the tests selected in it.
struct FileJob {
    root: Arc<SuiteRoot>,
    parsed: Result<Arc<Suite>, (PathBuf, String, String, ParseError)>,
    tests: Vec<usize>,
}

impl FileJob {
    /// Rough cost in requests: every selected test runs setup, its steps, teardown and a wipe.
    fn cost(&self) -> usize {
        match &self.parsed {
            Ok(suite) => {
                let fixed = suite.setup.steps.len() + suite.teardown.steps.len() + 4;
                self.tests.iter().map(|&i| fixed + suite.tests[i].steps.len()).sum()
            }
            Err(_) => 0,
        }
    }
}

/// Finds the `.yml` files below a root, sorted.
pub fn discover(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if root.is_file() {
        out.push(root.to_path_buf());
        return Ok(out);
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).with_context(|| format!("reading {}", d.display()))? {
            let p = entry?.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "yml" || e == "yaml") {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

fn full_id(label: &str, id: &str) -> String {
    if label.is_empty() { id.to_owned() } else { format!("{label}:{id}") }
}

/// Runs everything and returns the report.
pub async fn run(cfg: Config) -> anyhow::Result<Report> {
    let started_at = chrono::Utc::now().to_rfc3339();
    let started = Instant::now();
    let spec = Arc::new(ApiSpec::load_dir(&cfg.api_spec)?);
    let mut jobs = VecDeque::new();
    for root in &cfg.suites {
        let root = Arc::new(root.clone());
        let base = if root.path.is_file() { root.path.parent().unwrap_or(Path::new("")).to_path_buf() } else { root.path.clone() };
        for file in discover(&root.path)? {
            match Suite::load(&base, &file) {
                Ok(suite) => {
                    let tests: Vec<usize> = (0..suite.tests.len())
                        .filter(|&i| cfg.selects(&full_id(&root.label, &suite.test_id(&suite.tests[i]))))
                        .collect();
                    if !tests.is_empty() {
                        jobs.push_back(FileJob { root: root.clone(), parsed: Ok(Arc::new(suite)), tests });
                    }
                }
                Err(e) => {
                    let (id, area) = crate::suite::suite_id(&base, &file);
                    let fid = full_id(&root.label, &id);
                    let listed = cfg.only_ids.as_ref().is_none_or(|ids| {
                        let prefix = format!("{fid}/");
                        ids.iter().any(|i| i.starts_with(&prefix))
                    });
                    if listed && cfg.filter.as_ref().is_none_or(|f| f.is_match(&fid)) {
                        jobs.push_back(FileJob { root: root.clone(), parsed: Err((file, id, area, e)), tests: Vec::new() });
                    }
                }
            }
        }
    }
    // Longest files first: with several workers the run then ends with short files instead of
    // one worker grinding through a long file while the others idle. Steps (plus the per-test
    // wipe) approximate cost; the sort is stable so equal files keep their order.
    jobs.make_contiguous().sort_by_key(|j| std::cmp::Reverse(j.cost()));
    let total_files = jobs.len();
    let cfg = Arc::new(cfg);

    // Learn what the target is, on the shared cluster or a first fresh one.
    if cfg.cluster_hook.is_some() && cfg.spawn.is_some() {
        bail!("--cluster-hook and --spawn are alternatives");
    }
    let mut probe_hook_client = hook_client(&cfg)?;
    let (mut probe_client, probe_cluster) = if cfg.isolated() {
        let fresh = provision(&cfg, probe_hook_client.as_mut(), "probe", "probe").await?;
        let ep = fresh_endpoint(&fresh, &cfg)?;
        (Client::new(ep, cfg.timeout), Some(fresh))
    } else {
        let url = cfg.url.as_deref().context("either --url, --cluster-hook or --spawn is required")?;
        (Client::new(endpoint(url, &cfg)?, cfg.timeout), None)
    };
    let mut info = match TargetInfo::detect(&mut probe_client, cfg.flavor).await {
        Ok(info) => info,
        Err(e) => {
            if let Some(fresh) = probe_cluster {
                release(fresh, probe_hook_client.as_mut()).await;
            }
            return Err(e);
        }
    };
    if let Some(x) = cfg.xpack {
        info.products.xpack = x;
    }
    info.claimed_features.extend(cfg.claimed_features.iter().cloned());
    info.serverless = cfg.serverless;
    info.feature_flags.extend(cfg.feature_flags.iter().cloned());
    let target_summary = TargetSummary {
        url: match (&cfg.cluster_hook, &cfg.spawn) {
            (Some(h), _) => h.create_url.clone(),
            (None, Some(s)) => format!("spawn: {}", s.command),
            (None, None) => cfg.url.clone().unwrap_or_default(),
        },
        more_urls: if cfg.isolated() { Vec::new() } else { cfg.more_urls.clone() },
        flavor: info.flavor.name().to_owned(),
        version: info.version_string.clone(),
        os: info.os.clone(),
        content_type: cfg.content_type.describe(),
        method_strategy: format!("{:?}", cfg.method_strategy).to_lowercase(),
        seed: (cfg.content_type == ContentTypes::Random || cfg.method_strategy == MethodStrategy::Random).then_some(cfg.seed),
    };
    let info = Arc::new(info);
    let wipe_mode = cfg.wipe.unwrap_or(if cfg.isolated() { WipeMode::Light } else { WipeMode::Full });
    if let Some(fresh) = probe_cluster {
        release(fresh, probe_hook_client.as_mut()).await;
    } else if wipe_mode != WipeMode::None {
        // Start from a clean cluster.
        wipe(&mut probe_client, &info, wipe_mode).await.map_err(|e| anyhow::anyhow!("initial cleanup failed: {e}"))?;
    }

    let mut shared_clients = Vec::new();
    if !cfg.isolated() {
        shared_clients.push(probe_client);
        for url in &cfg.more_urls {
            let mut c = Client::new(endpoint(url, &cfg)?, cfg.timeout);
            if wipe_mode != WipeMode::None {
                wipe(&mut c, &info, wipe_mode).await.map_err(|e| anyhow::anyhow!("initial cleanup of {url} failed: {e}"))?;
            }
            shared_clients.push(c);
        }
    }
    let workers = if cfg.isolated() { cfg.jobs.max(1) } else { shared_clients.len() };
    if cfg.jobs > workers && !cfg.isolated() {
        eprintln!(
            "search-yaml: --jobs needs isolated clusters (--cluster-hook, --spawn, or one --url per job); running {workers} at a time"
        );
    }
    let mut shared_clients = shared_clients.into_iter();
    let queue = Arc::new(Mutex::new(jobs));
    let results = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut handles = Vec::new();
    for w in 0..workers {
        let shared = Shared {
            cfg: cfg.clone(),
            spec: spec.clone(),
            info: info.clone(),
            wipe_mode,
            total_files,
            done: done.clone(),
        };
        let client = shared_clients.next();
        let hook_client = hook_client(&cfg)?;
        let queue = queue.clone();
        let results = results.clone();
        handles.push(tokio::spawn(async move {
            let mut worker = Worker { shared, client, hook_client, rx: RegexCache::default(), id: w, requests: 0, clusters: 0 };
            loop {
                let job = queue.lock().await.pop_front();
                let Some(job) = job else { break };
                // Last resort (test-level panics are caught in run_file): keep the run going.
                let fallback = (job.root.label.clone(), job.parsed.as_ref().ok().cloned(), job.tests.clone());
                let out = match catch_unwind(worker.run_file(job)).await {
                    Ok(out) => out,
                    Err(msg) => {
                        eprintln!("search-yaml: worker {w} panicked: {msg}");
                        if let Some(c) = worker.client.as_mut() {
                            c.disconnect();
                        }
                        match fallback {
                            (label, Some(suite), tests) => {
                                failed_tests(&suite, &label, &tests, Phase::Test, "panic", &format!("search-yaml panicked running this file: {msg}"))
                            }
                            _ => Vec::new(),
                        }
                    }
                };
                results.lock().await.extend(out);
            }
        }));
    }
    for h in handles {
        h.await?;
    }
    let mut tests = Arc::try_unwrap(results).map(Mutex::into_inner).unwrap_or_default();
    tests.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Report {
        target: target_summary,
        started: started_at,
        duration_ms: started.elapsed().as_millis() as u64,
        totals: Report::totals(&tests),
        baseline: None,
        tests,
    })
}

/// `(scheme://, rest)` of a URL.
fn split_scheme(url: &str) -> (&str, &str) {
    for scheme in ["http://", "https://"] {
        if let Some(rest) = url.strip_prefix(scheme) {
            return (scheme, rest);
        }
    }
    ("http://", url)
}

fn base_of(url: &str) -> String {
    // The hook client only needs scheme, host and port; targets carry their own paths.
    let (scheme, rest) = split_scheme(url);
    let end = rest.find('/').unwrap_or(rest.len());
    format!("{scheme}{}", &rest[..end])
}

fn path_of(url: &str) -> String {
    let (_, rest) = split_scheme(url);
    rest.find('/').map(|i| rest[i..].to_owned()).unwrap_or_else(|| "/".into())
}

fn endpoint(url: &str, cfg: &Config) -> anyhow::Result<Endpoint> {
    let mut ep = Endpoint::parse(url)?;
    for (k, v) in &cfg.headers {
        ep.headers.retain(|(h, _)| !h.eq_ignore_ascii_case(k));
        ep.headers.push((k.clone(), v.clone()));
    }
    Ok(ep)
}

/// A cluster created through the hook or a spawned process.
struct FreshCluster {
    endpoint: Option<Endpoint>,
    delete_target: Option<DeleteTarget>,
    process: Option<Spawned>,
}

/// How to delete a hook-created cluster.
struct DeleteTarget {
    /// `scheme://host:port` when it isn't the hook's own.
    base: Option<String>,
    path: String,
    headers: Vec<(String, String)>,
}

/// A worker's own connection to the hook (cluster creation can be slow; workers don't queue on
/// each other's).
fn hook_client(cfg: &Config) -> anyhow::Result<Option<Client>> {
    match &cfg.cluster_hook {
        Some(h) => Ok(Some(Client::new(Endpoint::parse(&base_of(&h.create_url))?, cfg.timeout))),
        None => Ok(None),
    }
}

/// Creates an isolated cluster: through the hook, or by spawning a process. `slot` identifies the
/// worker (spawn log file).
async fn provision(cfg: &Config, hook_client: Option<&mut Client>, name: &str, slot: &str) -> anyhow::Result<FreshCluster> {
    if let (Some(hook), Some(client)) = (&cfg.cluster_hook, hook_client) {
        return create_cluster(hook, client, name, cfg).await;
    }
    let spawn = cfg.spawn.as_ref().context("no cluster hook or spawn command")?;
    let process = spawn.start(&format!("yaml-{}-{name}", std::process::id()), slot).await?;
    Ok(FreshCluster { endpoint: Some(endpoint(&process.url, cfg)?), delete_target: None, process: Some(process) })
}

/// Adds a note on the cluster to the failures among `results`.
fn annotate<'a>(results: impl Iterator<Item = &'a mut TestResult>, note: &str) {
    for r in results {
        if let Some(f) = r.failure.as_mut() {
            f.message.push_str(&format!(" [{note}]"));
        }
    }
}

/// Where a fresh cluster is reached: what the hook returned, else `--url`.
fn fresh_endpoint(fresh: &FreshCluster, cfg: &Config) -> anyhow::Result<Endpoint> {
    match (&fresh.endpoint, &cfg.url) {
        (Some(ep), _) => Ok(ep.clone()),
        (None, Some(url)) => endpoint(url, cfg),
        (None, None) => bail!("the cluster hook returned no url"),
    }
}

/// Deletes (stops) a fresh cluster. For a spawned process that had died on its own, returns a
/// note for the failures it caused.
async fn release(fresh: FreshCluster, hook_client: Option<&mut Client>) -> Option<String> {
    let mut note = None;
    if let Some(p) = fresh.process
        && let Some((status, log)) = p.stop().await
    {
        let n = format!("the cluster process exited during the run ({status}); its output is in {}", log.display());
        eprintln!("search-yaml: {n}");
        note = Some(n);
    }
    if let (Some(t), Some(client)) = (&fresh.delete_target, hook_client) {
        let r = match &t.base {
            None => client.send("DELETE", &t.path, &t.headers, None).await,
            Some(base) => match Endpoint::parse(base) {
                Ok(ep) => Client::new(ep, client.timeout()).send("DELETE", &t.path, &t.headers, None).await,
                Err(e) => Err(e),
            },
        };
        // A cluster that can't be deleted is the hook's to reap; the results stand.
        match r {
            Ok(r) if r.status < 300 || r.status == 404 => {}
            Ok(r) => eprintln!("search-yaml: deleting a cluster: DELETE {} returned {}", t.path, r.status),
            Err(e) => eprintln!("search-yaml: deleting a cluster: DELETE {} failed: {e:#}", t.path),
        }
    }
    note
}

async fn create_cluster(hook: &ClusterHook, client: &mut Client, name: &str, cfg: &Config) -> anyhow::Result<FreshCluster> {
    let name = format!("yaml-{}-{name}", std::process::id());
    let mut req = Map::new();
    req.insert("name".to_owned(), Value::Str(name));
    if let Some(f) = cfg.flavor {
        req.insert("flavor".to_owned(), Value::Str(f.name().to_owned()));
    }
    let body = Value::Map(req).to_json();
    let target = path_of(&hook.create_url);
    let r = client
        .send("POST", &target, &hook.headers, Some((Bytes::from(body), "application/json".into())))
        .await?;
    if r.status >= 300 {
        bail!("cluster hook POST {} returned {} {}", hook.create_url, r.status, String::from_utf8_lossy(&r.body));
    }
    let v = Value::from_json(&r.body).context("cluster hook response is not JSON")?;
    // The fields may sit at the top level or under `cluster` (`{"cluster": {...}, "password": ..}`).
    let s = |k: &str| {
        v.get(k)
            .or_else(|| v.get("cluster").and_then(|c| c.get(k)))
            .and_then(|x| match x {
                Value::Str(s) => Some(s.clone()),
                Value::Int(_) => Some(x.java_string()),
                _ => None,
            })
    };
    let endpoint = match s("url").or_else(|| s("endpoint")) {
        Some(url) => {
            let mut ep = endpoint(&url, cfg)?;
            let user = s("username").or_else(|| s("user"));
            if let Some(pass) = s("password") {
                let user = user.unwrap_or_else(|| "elastic".into());
                ep.headers.retain(|(h, _)| !h.eq_ignore_ascii_case("authorization"));
                ep.headers.push(("Authorization".into(), basic_auth(&user, &pass)));
            }
            if let Some(key) = s("api_key") {
                ep.headers.retain(|(h, _)| !h.eq_ignore_ascii_case("authorization"));
                ep.headers.push(("Authorization".into(), format!("ApiKey {key}")));
            }
            if let Some(h) = v.get("headers").or_else(|| v.get("cluster").and_then(|c| c.get("headers"))).and_then(Value::as_map) {
                for (k, val) in h {
                    ep.headers.push((k.clone(), val.java_string()));
                }
            }
            Some(ep)
        }
        None => None,
    };
    let hook_base = base_of(&hook.create_url);
    let delete_target = s("delete_url")
        .map(|u| {
            let absolute = u.starts_with("http://") || u.starts_with("https://");
            let base = base_of(&u);
            DeleteTarget {
                base: (absolute && base != hook_base).then_some(base),
                path: if absolute { path_of(&u) } else { u },
                headers: hook.headers.clone(),
            }
        })
        .or_else(|| {
            s("id").map(|id| DeleteTarget {
                base: None,
                path: format!("{}/{}", path_of(&hook.create_url).trim_end_matches('/'), id),
                headers: hook.headers.clone(),
            })
        });
    Ok(FreshCluster { endpoint, delete_target, process: None })
}

struct Shared {
    cfg: Arc<Config>,
    spec: Arc<ApiSpec>,
    info: Arc<TargetInfo>,
    wipe_mode: WipeMode,
    total_files: usize,
    done: Arc<std::sync::atomic::AtomicUsize>,
}

struct Worker {
    shared: Shared,
    /// The connection to the shared cluster (no hook) or the current fresh cluster.
    client: Option<Client>,
    /// This worker's connection to the cluster hook.
    hook_client: Option<Client>,
    rx: RegexCache,
    id: usize,
    /// Requests sent, for unique `X-Opaque-Id`s (OpenSearch).
    requests: u64,
    /// Isolated clusters created, for unique names.
    clusters: u64,
}

/// The last response of a `do`.
struct Last {
    /// The API of the `do` that got this response.
    api: String,
    /// The request body's media type when it wasn't plain JSON (for reproducing failures).
    media: Option<&'static str>,
    method: String,
    target: String,
    status: u16,
    reason: String,
    body: Option<Value>,
    text: Option<String>,
}

/// A step failure, before it's placed in a phase.
struct StepFail {
    message: String,
    expected: Option<Box<Value>>,
    actual: Option<Box<Value>>,
}

impl StepFail {
    fn msg(message: impl Into<String>) -> StepFail {
        StepFail { message: message.into(), expected: None, actual: None }
    }
}

impl From<Mismatch> for StepFail {
    fn from(m: Mismatch) -> StepFail {
        StepFail { message: m.message, expected: m.expected, actual: m.actual }
    }
}

impl From<crate::stash::StashError> for StepFail {
    fn from(e: crate::stash::StashError) -> StepFail {
        StepFail::msg(e.0)
    }
}

/// Per-test execution state.
struct Exec {
    stash: Stash,
    last: Option<Last>,
    /// The file is in elasticsearch-clients-tests format (it has a top-level `requires`). Those
    /// files are written for the official clients' runners, whose semantics differ from the
    /// Java runner's in a few places: see [`Worker::run_do`].
    clients: bool,
    /// Generator state for random body content types.
    rng: u64,
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes)", &s[..end], s.len())
}

/// A short key grouping failures that share a root cause, for the summary's "top causes": the
/// API and the error a `do` got (`index → 400 illegal_argument_exception: ...`), or the API whose
/// response an assertion rejected. Digits are folded to `#` so index names, ids and counts don't
/// split a group.
fn cause_of(step: &Step, exec: &Exec) -> String {
    let last = exec.last.as_ref();
    let (head, detail) = match &step.kind {
        StepKind::Do(d) => match last.filter(|l| l.api == d.api) {
            Some(l) if l.status >= 400 => {
                let error = l.body.as_ref().and_then(|b| b.get("error"));
                let field = |k: &str| error.and_then(|e| e.get(k)).and_then(Value::as_str).unwrap_or("");
                let detail = match error {
                    Some(Value::Str(s)) => s.clone(),
                    Some(_) => format!("{}: {}", field("type"), field("reason")),
                    None => String::new(),
                };
                (format!("{} → {}", d.api, l.status), detail)
            }
            Some(l) => match d.catch.as_deref() {
                Some(c) => (format!("{} → {}", d.api, l.status), format!("expected catch {c}")),
                None => (format!("{} → {}", d.api, l.status), "unexpected headers".to_owned()),
            },
            None => (format!("{}:", d.api), "no response".to_owned()),
        },
        _ => match last {
            Some(l) => (format!("{}:", l.api), format!("{} mismatch", step.kind.name())),
            None => (format!("{}:", step.kind.name()), "without a response".to_owned()),
        },
    };
    let mut out = head;
    if !detail.is_empty() {
        out.push(' ');
        let limit = out.len() + 120;
        let mut in_digits = false;
        for c in detail.chars() {
            if out.len() >= limit {
                out.push('…');
                break;
            }
            if c.is_ascii_digit() {
                if !in_digits {
                    out.push('#');
                }
                in_digits = true;
                continue;
            }
            in_digits = false;
            out.push(if c.is_whitespace() { ' ' } else { c });
        }
    }
    out
}

impl Exec {
    fn new(clients: bool, rng: u64) -> Exec {
        Exec { stash: Stash::new(), last: None, clients, rng }
    }

    fn clear(&mut self) {
        self.stash.clear();
        self.last = None;
    }

    /// `ClientYamlTestResponse.evaluate(path, stash)`.
    fn response(&self, path: &str) -> Result<Option<Cow<'_, Value>>, StepFail> {
        let Some(last) = &self.last else {
            return Ok(None);
        };
        match &last.body {
            Some(body) => Ok(evaluate(body, path, &self.stash)?),
            None => {
                if path.is_empty() && last.method == "HEAD" {
                    Ok(Some(Cow::Owned(Value::Bool(last.status < 400))))
                } else {
                    Ok(None)
                }
            }
        }
    }

    fn response_excerpt(&self) -> Option<String> {
        let last = self.last.as_ref()?;
        let body = match (&last.body, &last.text) {
            (Some(v), _) => v.to_json(),
            (None, Some(t)) => t.clone(),
            _ => String::new(),
        };
        Some(format!("{} {} {}", last.status, last.reason, truncate(&body, 4000)))
    }
}

/// Parses a response body by content type (JSON, YAML, CBOR or Smile → value; anything else →
/// text). For the binary types the text is the JSON form, as the Java runner logs them.
/// Elasticsearch's runner rejects an empty body declared as structured; OpenSearch's reads `{}`.
fn parse_body(r: &Response, method: &str, flavor: Flavor) -> Result<(Option<Value>, Option<String>), String> {
    if method == "HEAD" || r.body.is_empty() && r.header("content-type").is_none() {
        return Ok((None, None));
    }
    let ct_raw = r.header("content-type").unwrap_or("");
    let Some(format) = Format::from_content_type(ct_raw) else {
        return Ok((None, Some(String::from_utf8_lossy(&r.body).into_owned())));
    };
    if r.body.is_empty() {
        if flavor == Flavor::OpenSearch {
            return Ok((Some(Value::Map(Map::new())), Some(String::new())));
        }
        return Err(format!("Empty body is invalid for content-type [{ct_raw}]"));
    }
    let v = format.decode(&r.body)?;
    let text = if format.is_binary() { v.to_json() } else { String::from_utf8_lossy(&r.body).into_owned() };
    // `ObjectPath.createFromXContent` keeps a root array and otherwise calls `mapOrdered()`,
    // which reads a scalar (or null) root as an empty map: `$body` and `is_true: ''` see `{}`.
    let v = match v {
        v @ (Value::Map(_) | Value::List(_)) => v,
        _ => Value::Map(Map::new()),
    };
    Ok((Some(v), Some(text)))
}

/// Failed results for some tests of a file, all with the same failure.
fn failed_tests(suite: &Suite, label: &str, tests: &[usize], phase: Phase, step: &str, message: &str) -> Vec<TestResult> {
    tests
        .iter()
        .map(|&i| {
            let t = &suite.tests[i];
            TestResult {
                id: full_id(label, &suite.test_id(t)),
                suite: label.to_owned(),
                area: suite.area.clone(),
                file: suite.file.display().to_string(),
                name: t.name.clone(),
                status: Status::Fail,
                skip_reason: None,
                failure: Some(Failure {
                    phase,
                    cause: None,
                    line: t.line,
                    step: step.into(),
                    api: None,
                    request: None,
                    message: message.to_owned(),
                    expected: None,
                    actual: None,
                    response: None,
                }),
                teardown_failure: None,
                duration_ms: 0,
            }
        })
        .collect()
}

/// Runs a future, turning a panic while polling it into `Err(panic message)`. Responses come
/// from the server under test, so a panic in handling one (a runner bug) must fail that test,
/// not abort the run and lose the report. The future is dropped after a panic, never repolled.
async fn catch_unwind<F: Future>(fut: F) -> Result<F::Output, String> {
    let mut fut = std::pin::pin!(fut);
    std::future::poll_fn(move |cx| match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fut.as_mut().poll(cx))) {
        Ok(std::task::Poll::Ready(v)) => std::task::Poll::Ready(Ok(v)),
        Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
        Err(payload) => std::task::Poll::Ready(Err(panic_message(payload.as_ref()))),
    })
    .await
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    match (payload.downcast_ref::<&str>(), payload.downcast_ref::<String>()) {
        (Some(s), _) => (*s).to_owned(),
        (_, Some(s)) => s.clone(),
        _ => "(non-string panic payload)".to_owned(),
    }
}

impl Worker {
    fn cfg(&self) -> &Config {
        &self.shared.cfg
    }

    async fn run_file(&mut self, job: FileJob) -> Vec<TestResult> {
        let started = Instant::now();
        let label = job.root.label.clone();
        let suite = match job.parsed {
            Ok(s) => s,
            Err((file, id, area, e)) => {
                let result = TestResult {
                    id: full_id(&label, &format!("{id}/(file)")),
                    suite: label,
                    area,
                    file: file.display().to_string(),
                    name: "(file)".into(),
                    status: Status::Fail,
                    skip_reason: None,
                    failure: Some(Failure {
                        phase: Phase::Parse,
                        cause: None,
                        line: e.line,
                        step: "parse".into(),
                        api: None,
                        request: None,
                        message: format!("Error parsing {id}: {}", e.message),
                        expected: None,
                        actual: None,
                        response: None,
                    }),
                    teardown_failure: None,
                    duration_ms: 0,
                };
                self.progress(&id, std::slice::from_ref(&result), started);
                return vec![result];
            }
        };
        let isolated = self.cfg().isolated();
        let mut file_cluster = None;
        if isolated && !self.cfg().fresh_per_test {
            match self.fresh().await {
                Ok(fc) => file_cluster = Some(fc),
                Err(e) => return self.cluster_failure(&suite, &label, &job.tests, &e),
            }
        }
        let mut out = Vec::with_capacity(job.tests.len());
        for &i in &job.tests {
            let test = &suite.tests[i];
            let mut test_cluster = None;
            if isolated && self.cfg().fresh_per_test {
                match self.fresh().await {
                    Ok(fc) => test_cluster = Some(fc),
                    Err(e) => {
                        out.extend(self.cluster_failure(&suite, &label, &[i], &e));
                        continue;
                    }
                }
            }
            match catch_unwind(self.run_test(&suite, &label, test)).await {
                Ok(r) => out.push(r),
                Err(msg) => {
                    // The connection may be mid-exchange: start the next test on a new one.
                    if let Some(c) = self.client.as_mut() {
                        c.disconnect();
                    }
                    out.extend(failed_tests(&suite, &label, &[i], Phase::Test, "panic", &format!("search-yaml panicked running this test: {msg}")));
                }
            }
            if let Some(fc) = test_cluster
                && let Some(note) = self.drop_cluster(fc).await
            {
                annotate(out.last_mut().into_iter(), &note);
            }
        }
        if let Some(fc) = file_cluster
            && let Some(note) = self.drop_cluster(fc).await
        {
            annotate(out.iter_mut(), &note);
        }
        self.progress(&full_id(&label, &suite.id), &out, started);
        out
    }

    fn progress(&self, id: &str, results: &[TestResult], started: Instant) {
        let n = self.shared.done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if !self.cfg().progress {
            return;
        }
        let count = |s: Status| results.iter().filter(|r| r.status == s).count();
        eprintln!(
            "[{n}/{}] {id}: {} pass, {} fail, {} skip ({:.1}s){}",
            self.shared.total_files,
            count(Status::Pass),
            count(Status::Fail),
            count(Status::Skip),
            started.elapsed().as_secs_f64(),
            if self.shared.cfg.jobs > 1 { format!(" [w{}]", self.id) } else { String::new() }
        );
    }

    async fn fresh(&mut self) -> anyhow::Result<FreshCluster> {
        self.clusters += 1;
        let name = format!("w{}-{}", self.id, self.clusters);
        let cfg = self.shared.cfg.clone();
        let fc = provision(&cfg, self.hook_client.as_mut(), &name, &format!("w{}", self.id)).await?;
        let ep = fresh_endpoint(&fc, &cfg)?;
        self.client = Some(Client::new(ep, cfg.timeout));
        Ok(fc)
    }

    async fn drop_cluster(&mut self, fc: FreshCluster) -> Option<String> {
        // Close the connection first: a stopped server would otherwise leave it half open.
        self.client = None;
        release(fc, self.hook_client.as_mut()).await
    }

    fn cluster_failure(&self, suite: &Suite, label: &str, tests: &[usize], e: &anyhow::Error) -> Vec<TestResult> {
        failed_tests(suite, label, tests, Phase::Cluster, "cluster", &format!("creating a fresh cluster failed: {e:#}"))
    }

    async fn run_test(&mut self, suite: &Suite, label: &str, test: &TestSection) -> TestResult {
        let started = Instant::now();
        let id = full_id(label, &suite.test_id(test));
        let mut result = TestResult {
            id: id.clone(),
            suite: label.to_owned(),
            area: suite.area.clone(),
            file: suite.file.display().to_string(),
            name: test.name.clone(),
            status: Status::Pass,
            skip_reason: None,
            failure: None,
            teardown_failure: None,
            duration_ms: 0,
        };
        if let Some(entry) = self.cfg().skip_list.iter().find(|e| glob_match(&e.pattern, &id)) {
            result.status = Status::Skip;
            result.skip_reason = Some(format!("skip list: {}", entry.reason));
            return result;
        }
        let info = self.shared.info.clone();
        let skip = {
            let client = self.client.as_mut().expect("client");
            let mut reason = suite.clients_requires.as_ref().and_then(|r| info.clients_skip_reason(r));
            for (p, what) in [(&suite.setup.prereq, &suite.id), (&suite.teardown.prereq, &suite.id), (&test.prereq, &id)] {
                if reason.is_some() {
                    break;
                }
                match info.skip_reason(client, p, what).await {
                    Ok(r) => reason = r,
                    Err(e) => {
                        result.status = Status::Fail;
                        result.failure = Some(Failure {
                            // A capabilities call that errored is the cluster's fault, not the file's.
                            phase: if e.starts_with(crate::target::CAPABILITY_ERROR) { Phase::Setup } else { Phase::Parse },
                            cause: None,
                            line: test.line,
                            step: "skip".into(),
                            api: None,
                            request: None,
                            message: e,
                            expected: None,
                            actual: None,
                            response: None,
                        });
                        return result;
                    }
                }
            }
            reason
        };
        if let Some(reason) = skip {
            result.status = Status::Skip;
            result.skip_reason = Some(reason);
            return result;
        }

        let mut exec = Exec::new(suite.clients_requires.is_some(), test_rng(self.cfg().seed, &id));
        let mut failure: Option<Failure> = None;
        if let Some(f) = self.run_steps(&mut exec, &suite.setup.steps, Phase::Setup).await {
            failure = Some(f);
        }
        exec.clear();
        exec.stash.set("_project_id_prefix_", Value::Str(String::new()));
        if failure.is_none()
            && let Some(f) = self.run_steps(&mut exec, &test.steps, Phase::Test).await
        {
            failure = Some(f);
        }
        // Teardown runs whatever happened, and every step of it runs. Its failures don't fail
        // the test: the Java runner collects them after it has already checked for errors.
        for step in &suite.teardown.steps {
            if let Some(f) = self.run_steps(&mut exec, std::slice::from_ref(step), Phase::Teardown).await
                && result.teardown_failure.is_none()
            {
                result.teardown_failure = Some(f);
            }
        }
        if self.shared.wipe_mode != WipeMode::None && !(self.cfg().fresh_per_test && self.cfg().isolated()) {
            let trace = self.shared.cfg.trace;
            let client = self.client.as_mut().expect("client");
            let wipe_started = Instant::now();
            let wiped = wipe(client, &info, self.shared.wipe_mode).await;
            if trace {
                eprintln!("  wipe ({:.1} ms)", wipe_started.elapsed().as_secs_f64() * 1000.0);
            }
            if let Err(e) = wiped
                && failure.is_none()
            {
                failure = Some(Failure {
                    phase: Phase::Cleanup,
                    cause: None,
                    line: test.line,
                    step: "wipe".into(),
                    api: None,
                    request: None,
                    message: e.0,
                    expected: None,
                    actual: None,
                    response: None,
                });
            }
        }
        if let Some(f) = failure {
            result.status = Status::Fail;
            result.failure = Some(f);
        }
        result.duration_ms = started.elapsed().as_millis() as u64;
        result
    }

    /// Runs steps until one fails.
    async fn run_steps(&mut self, exec: &mut Exec, steps: &[Step], phase: Phase) -> Option<Failure> {
        for step in steps {
            if let Err(f) = self.run_step(exec, step).await {
                let request = exec.last.as_ref().map(|l| match l.media {
                    Some(m) => format!("{} {} ({m} body)", l.method, l.target),
                    None => format!("{} {}", l.method, l.target),
                });
                let api = match &step.kind {
                    StepKind::Do(d) => Some(d.api.clone()),
                    _ => None,
                };
                let cause = Some(cause_of(step, exec));
                return Some(Failure {
                    phase,
                    line: step.line,
                    step: step.kind.name().into(),
                    api,
                    request,
                    cause,
                    message: f.message,
                    expected: f.expected.map(|b| *b),
                    actual: f.actual.map(|b| *b),
                    response: exec.response_excerpt(),
                });
            }
        }
        None
    }

    async fn run_step(&mut self, exec: &mut Exec, step: &Step) -> Result<(), StepFail> {
        match &step.kind {
            StepKind::Do(d) => self.run_do(exec, d).await,
            StepKind::Set(pairs) => {
                for (path, name) in pairs {
                    let v = exec.response(path)?.map(Cow::into_owned).unwrap_or(Value::Null);
                    exec.stash.set(name, v);
                }
                Ok(())
            }
            StepKind::TransformAndSet(pairs) => {
                for (name, expr) in pairs {
                    let mut value = Value::Str(expr.clone());
                    if let Some(args) = expr.strip_prefix("#base64EncodeCredentials(").and_then(|r| r.strip_suffix(')')) {
                        // Java's String.split drops trailing empty strings (`a,b,` has two parts).
                        let mut parts: Vec<&str> = args.split(',').collect();
                        if !args.is_empty() {
                            while parts.last() == Some(&"") {
                                parts.pop();
                            }
                        }
                        if parts.len() != 2 {
                            return Err(StepFail::msg("base64EncodeCredentials requires a username/id and a password parameters"));
                        }
                        let get = |p: &str| -> Result<String, StepFail> {
                            Ok(exec.response(p.trim())?.map(|v| v.java_string()).unwrap_or_else(|| "null".into()))
                        };
                        let creds = format!("{}:{}", get(parts[0])?, get(parts[1])?);
                        use base64::Engine;
                        value = Value::Str(base64::engine::general_purpose::STANDARD.encode(creds));
                    }
                    exec.stash.set(name, value);
                }
                Ok(())
            }
            StepKind::Assert(a) => {
                let actual: Option<Cow<'_, Value>> = if Stash::is_stashed_value(&a.field) {
                    Some(Cow::Owned(exec.stash.get_value(&a.field)?))
                } else {
                    exec.response(&a.field)?
                };
                let needs_expected = !matches!(a.op, AssertOp::IsTrue | AssertOp::IsFalse | AssertOp::Exists | AssertOp::NotExists);
                let expected: Cow<'_, Value> = if !needs_expected {
                    Cow::Borrowed(&a.expected)
                } else {
                    match &a.expected {
                        Value::Map(_) => Cow::Owned(exec.stash.replace_stashed_values(&a.expected)?),
                        // The Java runner leaves lists alone; the clients' runners unstash them.
                        Value::List(_) if exec.clients => Cow::Owned(exec.stash.replace_stashed_values(&a.expected)?),
                        Value::Str(s) if Stash::contains_stashed_value(s) => Cow::Owned(exec.stash.get_value(s)?),
                        v => Cow::Borrowed(v),
                    }
                };
                let actual = actual.as_deref();
                let clients = exec.clients;
                let f = &a.field;
                let rx = &mut self.rx;
                let r = match &a.op {
                    AssertOp::Match => assertions::assert_match(f, actual, &expected, rx),
                    AssertOp::Length => assertions::assert_length(f, actual, &expected),
                    AssertOp::IsTrue => assertions::assert_true(f, actual),
                    AssertOp::IsFalse => assertions::assert_false(f, actual),
                    AssertOp::Gt => assertions::assert_cmp(Cmp::Gt, f, actual, &expected, clients),
                    AssertOp::Gte => assertions::assert_cmp(Cmp::Gte, f, actual, &expected, clients),
                    AssertOp::Lt => assertions::assert_cmp(Cmp::Lt, f, actual, &expected, clients),
                    AssertOp::Lte => assertions::assert_cmp(Cmp::Lte, f, actual, &expected, clients),
                    AssertOp::Contains => assertions::assert_contains(f, actual, &expected),
                    AssertOp::CloseTo { error } => assertions::assert_close_to(f, actual, &expected, *error),
                    AssertOp::MatchesInAnyOrder => assertions::assert_any_order(f, actual, &expected, rx),
                    AssertOp::IsAfter => assertions::assert_is_after(f, actual, &expected),
                    AssertOp::Exists => assertions::assert_exists(f, actual, true),
                    AssertOp::NotExists => assertions::assert_exists(f, actual, false),
                };
                r.map_err(StepFail::from)
            }
        }
    }

    async fn run_do(&mut self, exec: &mut Exec, d: &DoStep) -> Result<(), StepFail> {
        if d.catch.as_deref() == Some("param") {
            // The client would reject the call before sending it; nothing to check here.
            return Ok(());
        }
        let cfg = self.shared.cfg.clone();
        let mut params: Vec<(String, String)> = Vec::with_capacity(d.params.len() + 1);
        let mut ignore: Vec<u16> = Vec::new();
        let mut has_error_trace = false;
        for (k, v) in &d.params {
            let v = if Stash::contains_stashed_value(v) { exec.stash.get_value(v)?.java_string() } else { v.clone() };
            if k == "ignore" {
                ignore.extend(v.split(',').filter_map(|c| c.trim().parse::<u16>().ok()));
                continue;
            }
            if k == "error_trace" {
                has_error_trace = true;
                if v == "false" {
                    continue;
                }
            }
            params.push((k.clone(), v));
        }
        if cfg.error_trace && !has_error_trace {
            params.push(("error_trace".into(), "true".into()));
        }
        let mut headers: Vec<(String, String)> = Vec::with_capacity(d.headers.len());
        for (k, v) in &d.headers {
            let v = if Stash::contains_stashed_value(v) { exec.stash.get_value(v)?.java_string() } else { v.clone() };
            headers.push((k.clone(), v));
        }
        // The runner may add headers below; only the test's own are stashed as request_headers.
        let test_headers = headers.len();
        let content_type = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.clone());
        // Bodies the test doesn't type go out in the configured content type (the Java runner's
        // random one), unless a body is raw text; clients-tests files follow the clients, which
        // send the spec's type.
        let multi = d.bodies.len() > 1;
        let media = if content_type.is_none() && !exec.clients && d.bodies.iter().all(|b| matches!(b, Body::Value(_))) {
            cfg.content_type.pick(multi, &mut exec.rng)
        } else {
            MediaType::JSON
        };
        let body = if d.bodies.is_empty() {
            None
        } else {
            let mut buf = Vec::new();
            for b in &d.bodies {
                let Body::Value(v) = b;
                let v = exec.stash.replace_stashed_values(v)?;
                media.format.encode(&v, &mut buf);
                if multi {
                    buf.push(media.format.bulk_separator());
                }
            }
            Some(Bytes::from(buf))
        };
        let plan = self
            .shared
            .spec
            .plan_with(&d.api, d.method.as_deref(), &params, body.is_some(), cfg.method_strategy, exec.clients, &mut exec.rng)
            .map_err(StepFail::msg)?;
        // The Java runner sometimes (`!frequently()`: one time in ten) sends a small JSON or YAML
        // body as `GET ?source=&source_content_type=` when the path takes GET.
        let mut plan = plan;
        let mut body = body;
        if cfg.method_strategy == MethodStrategy::Random
            && d.method.is_none()
            && !exec.clients
            && content_type.is_none()
            && let Some(b) = &body
            && b.len() <= 2000
            && !media.vendor
            && matches!(media.format, Format::Json | Format::Yaml)
            && plan.path_allows_get
            && splitmix64(&mut exec.rng).is_multiple_of(10)
        {
            let mut q = String::from(if plan.target.contains('?') { "&source=" } else { "?source=" });
            crate::spec::encode_query_component(&String::from_utf8_lossy(b), &mut q);
            q.push_str("&source_content_type=");
            crate::spec::encode_query_component(&media.request_content_type(), &mut q);
            plan.target.push_str(&q);
            plan.method = "GET".into();
            body = None;
        }
        // The official clients send the spec's preferred media types; the Java runner sends
        // JSON bodies and no Accept header.
        if exec.clients
            && let Some(accept) = &plan.accept
            && !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("accept"))
        {
            headers.push(("Accept".into(), accept.clone()));
        }
        // OpenSearch emits a deprecation warning once per (key, X-Opaque-Id) for the node's
        // lifetime, so without a fresh id per request a warning a test expects may already have
        // been spent by an earlier test. Elasticsearch adds the header every time.
        self.requests += 1;
        if self.shared.info.flavor == Flavor::OpenSearch && !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("x-opaque-id")) {
            headers.push(("X-Opaque-Id".into(), format!("search-yaml-{}-{}", self.id, self.requests)));
        }
        let default_ct = match &plan.content_type {
            Some(ct) if exec.clients => ct.clone(),
            _ => media.request_content_type(),
        };
        let sent_media = (body.is_some() && content_type.is_none() && media != MediaType::JSON && !exec.clients).then(|| media.mime());
        let body = body.map(|b| (b, content_type.clone().unwrap_or(default_ct)));
        let client = self.client.as_mut().expect("client");
        let sent = Instant::now();
        let resp = client
            .send(&plan.method, &plan.target, &headers, body)
            .await
            .map_err(|e| StepFail::msg(format!("{e:#}")))?;
        if cfg.trace {
            eprintln!(
                "  {} {} -> {} ({:.1} ms, {} bytes)",
                plan.method,
                plan.target,
                resp.status,
                sent.elapsed().as_secs_f64() * 1000.0,
                resp.body.len()
            );
        }
        let parsed = parse_body(&resp, &plan.method, self.shared.info.flavor);
        let (body_value, text) = match parsed {
            Ok(p) => p,
            Err(e) => {
                exec.last = Some(Last {
                    api: d.api.clone(),
                    media: sent_media,
                    method: plan.method.clone(),
                    target: plan.target.clone(),
                    status: resp.status,
                    reason: resp.reason.clone(),
                    body: None,
                    text: Some(String::from_utf8_lossy(&resp.body).into_owned()),
                });
                return Err(StepFail::msg(e));
            }
        };
        let stash_body = match (&body_value, &text) {
            (Some(v), _) => v.clone(),
            (None, Some(t)) => Value::Str(t.clone()),
            _ => Value::Null,
        };
        exec.stash.set("body", stash_body);
        if test_headers > 0 {
            let m: Map = headers[..test_headers].iter().map(|(k, v)| (k.clone(), Value::Str(v.clone()))).collect();
            exec.stash.set("request_headers", Value::Map(m));
        }
        exec.last = Some(Last {
            api: d.api.clone(),
            media: sent_media,
            method: plan.method.clone(),
            target: plan.target.clone(),
            status: resp.status,
            reason: resp.reason.clone(),
            body: body_value,
            text,
        });
        let status = resp.status;
        let ok = status < 300 || (plan.method == "HEAD" && status == 404) || ignore.contains(&status);
        let describe = |exec: &Exec, expected: &str| {
            let api = &d.api;
            let body = exec.last.as_ref().and_then(|l| l.text.clone()).unwrap_or_default();
            format!(
                "expected [{expected}] status code but api [{api}] returned [{status} {}] [{}]",
                resp.reason,
                truncate(&body, 4000)
            )
        };
        if ok {
            if let Some(catch) = &d.catch {
                let expected = match catch_status(catch, self.shared.info.flavor) {
                    Some((code, _)) => code.to_owned(),
                    None if catch.starts_with('/') && catch.ends_with('/') => "4xx|5xx".into(),
                    None if exec.clients => "4xx|5xx".into(),
                    None => return Err(StepFail::msg(format!("catch value [{catch}] not supported"))),
                };
                return Err(StepFail::msg(describe(exec, &expected)));
            }
            let info = &self.shared.info;
            if info.flavor == Flavor::Elasticsearch {
                let products: Vec<&str> = resp.headers_named("x-elastic-product").collect();
                if products.is_empty() {
                    return Err(StepFail::msg("Response is missing required X-Elastic-Product response header"));
                }
                if !products.contains(&"Elasticsearch") {
                    return Err(StepFail::msg(format!(
                        "did not get expected product header [Elasticsearch], found header{} [{}]",
                        if products.len() > 1 { "s" } else { "" },
                        products.join(", ")
                    )));
                }
            }
            // The clients' runners only look at warnings when a test expects some.
            if exec.clients && d.warnings.is_empty() && d.warnings_regex.is_empty() {
                return Ok(());
            }
            let warnings: Vec<&str> = resp.headers_named("warning").collect();

            let exp = crate::warnings::Expectations {
                expected: &d.warnings,
                expected_regex: &d.warnings_regex,
                allowed: &d.allowed_warnings,
                allowed_regex: &d.allowed_warnings_regex,
            };
            crate::warnings::check(&warnings, info.flavor, &exp, &mut self.rx).map_err(|m| {
                let mut params_desc: Vec<String> = d.params.iter().map(|(k, v)| format!("{k}={v}")).collect();
                params_desc.sort();
                StepFail::msg(format!("{}[{{{}}}]: {m}", d.api, params_desc.join(", ")))
            })?;
            return Ok(());
        }
        let Some(catch) = &d.catch else {
            return Err(StepFail::msg(describe(exec, "2xx")));
        };
        if let Some((code, accept)) = catch_status(catch, self.shared.info.flavor) {
            if accept(status) {
                return Ok(());
            }
            return Err(StepFail::msg(describe(exec, code)));
        }
        if catch.len() > 2 && catch.starts_with('/') && catch.ends_with('/') {
            if status < 400 {
                return Err(StepFail::msg(describe(exec, "4xx|5xx")));
            }
            let Some(error) = exec.response("error")? else {
                return Err(StepFail::msg("error was expected in the response"));
            };
            let text = error.java_string();
            let pattern = &catch[1..catch.len() - 1];
            return match self.rx.find(pattern, false, &text) {
                Ok(true) => Ok(()),
                Ok(false) => Err(StepFail {
                    message: format!("the error message was expected to match the provided regex but didn't: /{pattern}/ vs [{}]", truncate(&text, 2000)),
                    expected: Some(Box::new(Value::Str(catch.clone()))),
                    actual: Some(Box::new(Value::Str(text))),
                }),
                Err(e) => Err(StepFail::msg(e)),
            };
        }
        if exec.clients {
            // clients-tests also write an error type as the catch value
            // (`catch: resource_not_found_exception`): any error of that type.
            let error_type = exec.response("error.type")?.map(|v| v.java_string());
            return match error_type {
                Some(t) if t != *catch => Err(StepFail {
                    message: format!("expected an error of type [{catch}] but api [{}] returned [{t}]", d.api),
                    expected: Some(Box::new(Value::Str(catch.clone()))),
                    actual: Some(Box::new(Value::Str(t))),
                }),
                _ => Ok(()),
            };
        }
        Err(StepFail::msg(format!("catch value [{catch}] not supported")))
    }
}

type StatusCheck = fn(u16) -> bool;

/// The named `catch` values: expected code (for messages) and acceptance. OpenSearch's runner
/// has no `gone`, so its `request` also accepts 410.
fn catch_status(catch: &str, flavor: Flavor) -> Option<(&'static str, StatusCheck)> {
    let os = flavor == Flavor::OpenSearch;
    Some(match catch {
        "bad_request" => ("400", |s| s == 400),
        "unauthorized" => ("401", |s| s == 401),
        "forbidden" => ("403", |s| s == 403),
        "missing" => ("404", |s| s == 404),
        "request_timeout" => ("408", |s| s == 408),
        "conflict" => ("409", |s| s == 409),
        "gone" if !os => ("410", |s| s == 410),
        "unavailable" => ("503", |s| s == 503),
        "request" if os => ("4xx|5xx", |s| s >= 400 && !matches!(s, 400 | 401 | 403 | 404 | 408 | 409)),
        "request" => ("4xx|5xx", |s| s >= 400 && !matches!(s, 400 | 401 | 403 | 404 | 408 | 409 | 410)),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(content_type: &str, body: &str) -> Response {
        Response {
            status: 200,
            reason: "OK".into(),
            headers: vec![("content-type".into(), content_type.into())],
            body: Bytes::from(body.to_owned()),
        }
    }

    #[tokio::test]
    async fn panics_become_results() {
        assert_eq!(catch_unwind(async { 7 }).await, Ok(7));
        let r = catch_unwind(async {
            tokio::task::yield_now().await;
            let v: Vec<u8> = Vec::new();
            let i = std::hint::black_box(3);
            v[i]
        })
        .await;
        assert!(r.unwrap_err().contains("index out of bounds"));
        let r = catch_unwind(async { std::panic::panic_any(5u8) }).await;
        assert_eq!(r, Err::<(), _>("(non-string panic payload)".into()));
        let r = catch_unwind(async { panic!("static") }).await;
        assert_eq!(r, Err::<(), _>("static".into()));
    }

    #[test]
    fn response_bodies() {
        let parse = |ct: &str, body: &str| parse_body(&response(ct, body), "GET", Flavor::Elasticsearch);
        let (v, text) = parse("application/json", r#"{"a":[1]}"#).unwrap();
        assert_eq!(v.unwrap().to_json(), r#"{"a":[1]}"#);
        assert_eq!(text.as_deref(), Some(r#"{"a":[1]}"#));
        assert_eq!(parse("application/json", "[1,2]").unwrap().0.unwrap().to_json(), "[1,2]");
        // A scalar root reads as an empty map (`mapOrdered()`), the text stays as sent.
        for scalar in ["true", "false", "1", "\"x\"", "null"] {
            let (v, text) = parse("application/json; charset=UTF-8", scalar).unwrap();
            assert_eq!(v.unwrap().to_json(), "{}", "{scalar}");
            assert_eq!(text.as_deref(), Some(scalar));
        }
        assert_eq!(parse("application/yaml", "--- true\n").unwrap().0.unwrap().to_json(), "{}");
        // Jackson's STRICT_DUPLICATE_DETECTION: a repeated field fails the response.
        let err = parse("application/json", r#"{"status":"green","status":"red"}"#).unwrap_err();
        assert!(err.contains("Duplicate field 'status'"), "{err}");
        let err = parse("application/json", r#"{"a":{"b":1,"b":1}}"#).unwrap_err();
        assert!(err.contains("Duplicate field 'b'"), "{err}");
        let err = parse("application/yaml", "a: 1\na: 2\n").unwrap_err();
        assert!(err.contains("Duplicate field 'a'"), "{err}");
        assert!(parse("application/json", "").unwrap_err().starts_with("Empty body is invalid"));
        assert_eq!(parse("text/plain", "true").unwrap(), (None, Some("true".into())));
    }

    #[test]
    fn suite_roots() {
        let r = SuiteRoot::parse("free=/x/test/free");
        assert_eq!((r.label.as_str(), r.path.to_str().unwrap()), ("free", "/x/test/free"));
        let r = SuiteRoot::parse("/x/a=b");
        assert_eq!((r.label.as_str(), r.path.to_str().unwrap()), ("", "/x/a=b"));
    }

    #[test]
    fn selection() {
        let mut cfg = Config { filter: Some(Regex::new("^free:search").unwrap()), ..Config::default() };
        assert!(cfg.selects("free:search/10_basic/x"));
        assert!(!cfg.selects("free:index/10_basic/x"));
        cfg.only_ids = Some(["free:search/10_basic/trailing".to_owned()].into_iter().collect());
        assert!(cfg.selects("free:search/10_basic/trailing "));
        assert!(!cfg.selects("free:search/10_basic/x"));
        cfg.filter = None;
        assert!(!cfg.selects("free:index/10_basic/x"));
    }

    #[test]
    fn content_type_choices() {
        let mut rng = test_rng(7, "free:search/10_basic/x");
        assert_eq!(ContentTypes::Json.pick(false, &mut rng), MediaType::JSON);
        let smile = MediaType::parse("smile").unwrap();
        let cbor = MediaType::parse("vnd-cbor").unwrap();
        assert_eq!(ContentTypes::Fixed(cbor).pick(false, &mut rng), cbor);
        // Multi-document bodies only stream as JSON or Smile.
        assert_eq!(ContentTypes::Fixed(cbor).pick(true, &mut rng), MediaType::JSON);
        assert_eq!(ContentTypes::Fixed(MediaType::parse("vnd-smile").unwrap()).pick(true, &mut rng), smile);
        // Random: reproducible per (seed, test), covers every type, streams only JSON/Smile.
        let draw = |seed: u64, id: &str, multi: bool| {
            let mut rng = test_rng(seed, id);
            (0..64).map(|_| ContentTypes::Random.pick(multi, &mut rng)).collect::<Vec<_>>()
        };
        assert_eq!(draw(1, "a", false), draw(1, "a", false));
        assert_ne!(draw(1, "a", false), draw(2, "a", false));
        assert_ne!(draw(1, "a", false), draw(1, "b", false));
        let all: HashSet<MediaType> = draw(3, "a", false).into_iter().collect();
        assert_eq!(all.len(), MediaType::ALL.len());
        assert!(draw(3, "a", true).iter().all(|m| MediaType::STREAMING.contains(m)));
        assert_eq!(ContentTypes::parse("random"), Some(ContentTypes::Random));
        assert_eq!(ContentTypes::parse("yaml"), Some(ContentTypes::Fixed(MediaType::parse("yaml").unwrap())));
        assert_eq!(ContentTypes::parse("xml"), None);
    }

    #[test]
    fn failure_causes() {
        let step = |api: &str, catch: Option<&str>| Step {
            line: 1,
            kind: StepKind::Do(Box::new(DoStep { api: api.into(), catch: catch.map(str::to_owned), ..DoStep::default() })),
        };
        let last = |api: &str, status: u16, body: Option<&str>| Last {
            api: api.into(),
            media: None,
            method: "PUT".into(),
            target: "/x".into(),
            status,
            reason: String::new(),
            body: body.map(crate::value::json),
            text: None,
        };
        let mut exec = Exec::new(false, 0);
        assert_eq!(cause_of(&step("index", None), &exec), "index: no response");
        // Error responses: type and reason, digits folded so per-test names group together.
        exec.last = Some(last(
            "index",
            400,
            Some(r#"{"error":{"type":"mapper_parsing_exception","reason":"failed to parse field [f1] of type [long] in document with id '12'"}}"#),
        ));
        assert_eq!(
            cause_of(&step("index", None), &exec),
            "index → 400 mapper_parsing_exception: failed to parse field [f#] of type [long] in document with id '#'"
        );
        exec.last = Some(last("get", 404, Some(r#"{"error":"no handler found for uri [/x] and method [GET]"}"#)));
        assert_eq!(cause_of(&step("get", None), &exec), "get → 404 no handler found for uri [/x] and method [GET]");
        // A response from an earlier `do` isn't this step's.
        assert_eq!(cause_of(&step("index", None), &exec), "index: no response");
        exec.last = Some(last("get", 200, Some("{}")));
        assert_eq!(cause_of(&step("get", Some("missing")), &exec), "get → 200 expected catch missing");
        assert_eq!(cause_of(&step("get", None), &exec), "get → 200 unexpected headers");
        let m = Step {
            line: 2,
            kind: StepKind::Assert(crate::suite::Assertion { op: AssertOp::Match, field: "a".into(), expected: Value::Null }),
        };
        assert_eq!(cause_of(&m, &exec), "get: match mismatch");
        exec.last = None;
        assert_eq!(cause_of(&m, &exec), "match: without a response");
        // Long reasons are cut.
        let long = format!(r#"{{"error":{{"type":"t","reason":"{}"}}}}"#, "x".repeat(500));
        exec.last = Some(last("search", 500, Some(&long)));
        let c = cause_of(&step("search", None), &exec);
        assert!(c.ends_with('…') && c.chars().count() < 160, "{c}");
    }

    #[test]
    fn catches() {
        let es = Flavor::Elasticsearch;
        assert!(catch_status("missing", es).unwrap().1(404));
        assert!(!catch_status("missing", es).unwrap().1(400));
        assert!(catch_status("request", es).unwrap().1(500));
        assert!(catch_status("request", es).unwrap().1(429));
        assert!(!catch_status("request", es).unwrap().1(404));
        assert!(!catch_status("request", es).unwrap().1(410));
        assert!(catch_status("gone", es).unwrap().1(410));
        assert!(catch_status("/x/", es).is_none());
        let os = Flavor::OpenSearch;
        assert!(catch_status("gone", os).is_none());
        assert!(catch_status("request", os).unwrap().1(410));
    }

    #[test]
    fn urls() {
        assert_eq!(base_of("http://h:1/v1/search/clusters"), "http://h:1");
        assert_eq!(path_of("http://h:1/v1/search/clusters"), "/v1/search/clusters");
        assert_eq!(path_of("http://h:1"), "/");
        assert_eq!(base_of("https://h:1/v1/x"), "https://h:1");
        assert_eq!(path_of("https://h/v1/x"), "/v1/x");
    }
}
