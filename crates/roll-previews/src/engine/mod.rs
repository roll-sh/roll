//! The reconciler (docs/previews.md, "Converging"): every pass discovers which previews should
//! exist from roll code's open changes, then converges each unsettled preview to its desired
//! state. Every step looks before it makes, so a pass after a crash carries on where the last
//! one stopped.

use crate::ports::{Apps, Brancher, ChangeInfo, EnvSource, Secrets, ServiceSpec, Source, Target};
use crate::settings::{Settings, is_production};
use crate::store::{New, ResourceRow, Row, Store};
use crate::{Desired, Error, Kind, Mode, Preview, ResourceState, Result, ServiceState, State, branch_name};
use async_trait::async_trait;
use parking_lot::Mutex;
use roll_workspace::Workspace;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{info, warn};

/// A preview's lease in a cluster: its holder converges it, runs its services and serves its
/// hosts, so previews spread over the nodes. Discovering previews is the singleton's
/// (`Cluster::singleton("previews")`).
pub fn preview_key(id: &str) -> String {
    format!("preview:{id}")
}

/// Settled previews aren't converged again until something changes.
/// The most refused hosts a preview's activity names (the rest are counted).
const EGRESS_TOLD_MAX: usize = 50;
const SETTLED: i64 = i64::MAX;
/// How long a converge holds a preview (its `next_at` meanwhile); a crash is retried after it.
const CONVERGE_LEASE_MS: i64 = 300_000;
/// The first retry after a failed step, doubling up to [`MAX_BACKOFF_MS`].
const BACKOFF_MS: i64 = 15_000;
const MAX_BACKOFF_MS: i64 = 600_000;
/// A preview unused this long has its branches told it's idle ([`Brancher::idle`]).
pub const IDLE_AFTER_MS: i64 = 3_600_000;

/// How long before an unused preview goes it says so: a day, or a quarter of a shorter TTL.
const WARN_BEFORE_MS: i64 = 86_400_000;

/// When a preview goes, if that's within its warning: unused since `last_active_at` for
/// `ttl_hours`, or at `expires_at`, whichever is first. `None` when it's kept or not close.
pub fn goes_soon(last_active_at: i64, expires_at: Option<i64>, kept_until: Option<i64>, ttl_hours: u32, now: i64) -> Option<i64> {
    let ttl_ms = ttl_hours as i64 * 3_600_000;
    // It goes once it's past both its keep and its first deadline (the TTL, or a fixed expiry).
    let deadline = expires_at.map_or(last_active_at + ttl_ms, |e| e.min(last_active_at + ttl_ms));
    let at = kept_until.map_or(deadline, |k| k.max(deadline));
    (at - now <= WARN_BEFORE_MS.min(ttl_ms / 4)).then_some(at)
}

/// Whether using a preview would push back when it goes (`at`): not when that's its keep or a
/// fixed expiry (a branch preview's).
pub fn use_extends(last_active_at: i64, expires_at: Option<i64>, ttl_hours: u32, at: i64) -> bool {
    at == last_active_at + ttl_hours as i64 * 3_600_000 && expires_at.is_none_or(|e| e > at)
}

/// A duration as the activity says it: `23 hours`, `40 minutes`.
fn in_words(ms: i64) -> String {
    let m = (ms.max(0) + 59_999) / 60_000;
    match m {
        0..=1 => "a minute".into(),
        2..=119 => format!("{m} minutes"),
        _ => format!("{} hours", (m + 30) / 60),
    }
}

/// Previews converged at once.
const PARALLEL: usize = 4;

#[derive(Clone, Debug)]
pub struct Config {
    /// Services answer on `<label>.<domain>`.
    pub domain: String,
    pub scheme: String,
    pub port: Option<u16>,
    /// A pass at least this often.
    pub interval: Duration,
    /// How often branches' sizes and costs are read again.
    pub usage_every: Duration,
    /// Where services reach roll's mail catcher (`host:port` as a client on this node writes it;
    /// containers get it rewritten). `None`: no catcher, and previews' mail fails.
    pub mail: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            domain: "preview.localhost".into(),
            scheme: "http".into(),
            port: None,
            interval: Duration::from_secs(15),
            usage_every: Duration::from_secs(300),
            mail: None,
        }
    }
}

impl Config {
    /// A host label's URL under a project's own preview domain (`(base, https)`), or under rolld's.
    pub fn url_in(&self, domain: Option<&(String, bool)>, host: &str) -> String {
        match domain {
            Some((base, https)) => format!("{}://{host}.{base}", if *https { "https" } else { "http" }),
            None => self.url(host),
        }
    }

    pub fn url(&self, host: &str) -> String {
        let port = match (self.scheme.as_str(), self.port) {
            ("http", Some(80)) | ("https", Some(443)) | (_, None) => String::new(),
            (_, Some(p)) => format!(":{p}"),
        };
        format!("{}://{host}.{}{port}", self.scheme, self.domain)
    }
}

/// What an engine did since it started: converges and screenshots, and how many failed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub converges: u64,
    pub converge_failures: u64,
    pub screenshots: u64,
    pub screenshot_failures: u64,
}

#[derive(Default)]
pub(crate) struct Counters {
    converges: std::sync::atomic::AtomicU64,
    converge_failures: std::sync::atomic::AtomicU64,
    screenshots: std::sync::atomic::AtomicU64,
    screenshot_failures: std::sync::atomic::AtomicU64,
}

impl Counters {
    pub(crate) fn add(c: &std::sync::atomic::AtomicU64) {
        c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A preview's errors by `(id, minutes)`: when read (ms), and what (masked).
type ErrorsRead = HashMap<(String, i64), (i64, Vec<crate::ErrorGroup>)>;
/// How many errors a preview's services wrote by `(id, minutes)`: when read (ms), and how many.
type ErrorCountsRead = HashMap<(String, i64), (i64, u64)>;

pub struct Engine {
    pub cfg: Config,
    pub store: Arc<Store>,
    workspace: Arc<Workspace>,
    source: Arc<dyn Source>,
    branchers: HashMap<Kind, Arc<dyn Brancher>>,
    apps: Arc<dyn Apps>,
    secrets: Arc<dyn Secrets>,
    /// Whether this node runs the reconciler (`singleton("previews")` in a cluster).
    leader: Box<dyn Fn() -> bool + Send + Sync>,
    kick: Notify,
    /// In a cluster: requests that act on running services go to the reconciler's node.
    pub cluster: std::sync::OnceLock<Arc<roll_cluster::Cluster>>,
    /// Who may open a project's previews under team access; unset, nobody signs in.
    pub visitors: std::sync::OnceLock<Arc<dyn crate::ports::Visitors>>,

    /// Where a Git URL service's previews are posted (rolld: roll apps' GitHub App connection).
    pub git_report: std::sync::OnceLock<Arc<dyn crate::ports::GitReport>>,
    usage_read: Mutex<HashMap<String, i64>>,
    /// What each preview's services used, read at (ms): `GET …/metrics` reads them again only after
    /// [`METRICS_FOR`], however many sheets poll.
    metrics_read: Mutex<HashMap<String, (i64, Vec<crate::ServiceMetrics>)>>,
    /// Each preview's errors over a span of minutes, read at (ms): `GET …/errors` reads them again
    /// only after [`requests::ERRORS_FOR`].
    errors_read: Mutex<ErrorsRead>,
    /// The same, counted only (a list's rows): nothing in them is masked, so no values are read.
    error_counts_read: Mutex<ErrorCountsRead>,
    /// Keys the fingerprints `GET …/errors` answers (random per process): one is a group's name on
    /// a sheet, never a hash of what the error said.
    errors_key: [u8; 32],
    /// Previews whose screenshot a converge asked for; taken after the pass (`pass`).
    shots_wanted: Mutex<std::collections::HashSet<String>>,
    /// Previews with a screenshot task queued or running, and whether it was asked again
    /// meanwhile: never a second one at once (its container's name is the service's, so two
    /// would refuse each other).
    shots_running: Mutex<HashMap<String, bool>>,
    /// Screenshots that failed once at a head (preview, head): tried again once, since the service
    /// may have been stopped or redeployed underneath, before the head is given up on.
    shots_failed: Mutex<std::collections::HashSet<(String, String)>>,
    /// What this node did, for rolld's metrics (`Engine::stats`).
    counters: Counters,
    /// The hosts each preview's egress proxy refused that its activity already says.
    egress_told: Mutex<HashMap<String, std::collections::HashSet<String>>>,
    /// Held while a Git URL preview's report is posted: a converge's and a screenshot's at once
    /// would each make the pull request's first comment before either kept its id.
    git_reporting: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// What each preview's output mustn't show (`masks.rs`).
    log_masks: Mutex<HashMap<String, masks::Masks>>,
    /// Screenshots taken at once on this node ([`screenshot_slots`]).
    shooting: Arc<tokio::sync::Semaphore>,
    /// Services a converge just deployed at a change's new head, woken after the pass (`warm`).
    warm_wanted: Mutex<Vec<(String, String)>>,
    /// When a service's deploy was first found still going (`StillDeploying`): one that never
    /// finishes fails after [`DEPLOY_WAIT_MS`].
    deploying_since: Mutex<HashMap<(String, String), i64>>,
    /// The unresolved secrets last told per preview (each set once in its activity).
    secrets_told: Mutex<HashMap<String, Vec<String>>>,
    /// When each project's own preview domain was last read from roll apps (ms).
    domain_read: Mutex<HashMap<String, i64>>,
    /// The paths a head changes (`repo`, `head`, or `from..head`), for the path filters. Between
    /// two changesets they never change; since the merge base they could, if part of the change
    /// lands first, and then the next push reads them again. Cleared past [`PATHS_KEPT`] heads.
    paths_seen: Mutex<HashMap<(String, String), HeadPaths>>,
    /// One pass at a time.
    passing: tokio::sync::Mutex<()>,
}

/// The variables a branch of `kind` is reached by (the first of a kind takes the plain names).
fn suffixed(vars: Vec<(String, String)>, suffix: Option<&str>) -> Vec<(String, String)> {
    match suffix {
        None => vars,
        Some(s) => vars.into_iter().map(|(k, v)| (format!("{k}_{s}"), v)).collect(),
    }
}

fn set(env: &mut Vec<(String, String)>, k: &str, v: String) {
    env.retain(|(name, _)| name != k);
    env.push((k.to_string(), v));
}

fn env_suffix(parent: &str) -> String {
    let s: String = parent.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    s.trim_matches('_').to_string()
}

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Config,
        store: Arc<Store>,
        workspace: Arc<Workspace>,
        source: Arc<dyn Source>,
        branchers: Vec<Arc<dyn Brancher>>,
        apps: Arc<dyn Apps>,
        secrets: Arc<dyn Secrets>,
        leader: Box<dyn Fn() -> bool + Send + Sync>,
    ) -> Arc<Engine> {
        Arc::new(Engine {
            cfg,
            store,
            workspace,
            source,
            branchers: branchers.into_iter().map(|b| (b.kind(), b)).collect(),
            apps,
            secrets,
            leader,
            kick: Notify::new(),
            cluster: std::sync::OnceLock::new(),
            visitors: std::sync::OnceLock::new(),

            git_report: std::sync::OnceLock::new(),
            usage_read: Mutex::new(HashMap::new()),
            metrics_read: Mutex::new(HashMap::new()),
            errors_read: Mutex::new(HashMap::new()),
            error_counts_read: Mutex::new(HashMap::new()),
            errors_key: {
                let mut k = [0u8; 32];
                getrandom::fill(&mut k).expect("system randomness");
                k
            },
            secrets_told: Mutex::new(HashMap::new()),
            domain_read: Mutex::new(HashMap::new()),
            paths_seen: Mutex::new(HashMap::new()),
            shots_wanted: Mutex::new(std::collections::HashSet::new()),
            shots_running: Mutex::new(HashMap::new()),
            shots_failed: Mutex::new(std::collections::HashSet::new()),
            counters: Counters::default(),
            shooting: Arc::new(tokio::sync::Semaphore::new(screenshot_slots())),
            egress_told: Mutex::new(HashMap::new()),
            git_reporting: Mutex::new(HashMap::new()),
            log_masks: Mutex::new(HashMap::new()),
            warm_wanted: Mutex::new(Vec::new()),
            deploying_since: Mutex::new(HashMap::new()),
            passing: tokio::sync::Mutex::new(()),
        })
    }

    /// Run passes until the engine is dropped: every `interval`, and as soon as something is
    /// asked of it or roll code nudges.
    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::downgrade(self);
        let mut nudges = self.source.nudges();
        tokio::spawn(async move {
            loop {
                let Some(e) = this.upgrade() else { return };
                // Every node converges the previews it holds; the leader also discovers.
                if let Err(err) = e.pass().await {
                    warn!(error = %err, "previews: a pass failed");
                }
                let interval = e.cfg.interval;
                let kicked = async {
                    match nudges.as_mut() {
                        Some(n) => {
                            let _ = n.recv().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    _ = e.kick.notified() => {}
                    _ = kicked => {}
                    _ = tokio::time::sleep(interval) => {}
                }
                drop(e);
            }
        })
    }

    /// What this node's engine did since it started (rolld publishes the change each minute).
    pub fn stats(&self) -> Stats {
        let get = |c: &std::sync::atomic::AtomicU64| c.load(std::sync::atomic::Ordering::Relaxed);
        let c = &self.counters;
        Stats { converges: get(&c.converges), converge_failures: get(&c.converge_failures), screenshots: get(&c.screenshots), screenshot_failures: get(&c.screenshot_failures) }
    }

    /// Converge soon.
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    /// One pass: discover, then converge every preview that's due.
    pub async fn pass(self: &Arc<Self>) -> Result<()> {
        let _one = self.passing.lock().await;
        if (self.leader)() {
            let enabled = self.store.enabled_projects()?;
            for (project, settings) in &enabled {
                if let Err(e) = self.discover(project, settings).await {
                    warn!(%project, error = %e, "previews: discovering");
                }
            }
            // Previews turned off: the project's previews go (never for rules that don't parse).
            let off = self.store.disabled_projects()?;
            for r in self.store.rows(None)? {
                if r.desired == Desired::Active && off.contains(&r.project_id) {
                    self.mark_deleted(&r.id, "previews were turned off for the project")?;
                }
            }
        }
        let now = self.store.now_ms()?;
        let due: Vec<Row> = self.store.rows(None)?.into_iter().filter(|r| r.next_at <= now).collect();
        let sem = Arc::new(tokio::sync::Semaphore::new(PARALLEL));
        let mut set = tokio::task::JoinSet::new();
        for r in due {
            // In a cluster, only the preview's holder converges it (and runs its services).
            let guard = match self.cluster.get() {
                Some(c) => match c.route_async(&preview_key(&r.id)).await {
                    Ok(roll_cluster::Route::Local(g)) => Some(g),
                    _ => continue,
                },
                None => None,
            };
            let this = self.clone();
            let permit = sem.clone().acquire_owned().await.map_err(|e| Error::Internal(e.to_string()))?;
            let cluster = self.cluster.get().cloned();
            set.spawn(async move {
                let id = r.id.clone();
                // Cut short if this node loses the preview's lease meanwhile (a partition): the
                // new holder converges it, never both at once.
                match (&cluster, &guard) {
                    (Some(c), Some(g)) => {
                        let _ = c.while_guarded(g, this.converge_one(r)).await;
                    }
                    _ => this.converge_one(r).await,
                }
                drop((permit, guard));
                id
            });
        }
        while set.join_next().await.is_some() {}
        self.tend_leases().await;
        self.note_refused_egress().await;
        // Services those converges deployed at a new head, woken so their first visit is warm (a
        // few at a time, off the pass).
        let warm: Vec<(String, String)> = std::mem::take(&mut *self.warm_wanted.lock());
        let waking = Arc::new(tokio::sync::Semaphore::new(4));
        for (id, svc) in warm {
            let (apps, gate, cluster) = (self.apps.clone(), waking.clone(), self.cluster.get().cloned());
            tokio::spawn(async move {
                let Ok(_one) = gate.acquire_owned().await else { return };
                // Only where the preview's services run (its lease's holder, which stops them
                // when the lease moves).
                let _guard = match &cluster {
                    Some(c) => match c.route_async(&preview_key(&id)).await {
                        Ok(roll_cluster::Route::Local(g)) => Some(g),
                        _ => return,
                    },
                    None => None,
                };
                if let Err(e) = apps.wake(&id, &svc).await {
                    warn!(preview = %id, service = %svc, error = %e, "previews: warming a service after a push");
                }
            });
        }
        // Screenshots asked for by those converges, one at a time, off the pass: a slow service
        // or a first pull of the browser image holds nothing else up.
        let wanted: Vec<String> = self.shots_wanted.lock().drain().collect();
        for id in wanted {
            // One task per preview at a time: an ask while one is queued or running marks it to
            // go again once it's done (at whatever head the preview is at then).
            {
                let mut running = self.shots_running.lock();
                if let Some(again) = running.get_mut(&id) {
                    *again = true;
                    continue;
                }
                running.insert(id.clone(), false);
            }
            let this = self.clone();
            let gate = self.shooting.clone();
            tokio::spawn(async move {
                loop {
                    this.screenshot_once(&gate, &id).await;
                    let mut running = this.shots_running.lock();
                    match running.get_mut(&id) {
                        Some(again) if *again => *again = false,
                        _ => {
                            running.remove(&id);
                            break;
                        }
                    }
                }
            });
        }
        Ok(())
    }
}

impl Engine {
    /// One screenshot run of a preview, in a slot: if it's still wanted (ready, the rules still
    /// ask, not taken at this head meanwhile), then the change and its pull request told at once.
    async fn screenshot_once(&self, gate: &Arc<tokio::sync::Semaphore>, id: &str) {
        let Ok(_one) = gate.clone().acquire_owned().await else { return };
        let Ok(r) = self.store.row(id) else { return };
        let Ok(s) = self.store.settings(&r.project_id) else { return };
        if r.state != State::Ready || !s.github.screenshot || self.store.screenshot_head(&r.id).ok().flatten().as_deref() == Some(r.build()) {
            return;
        }
        if self.take_screenshot(&r).await
            && let Ok(row) = self.store.row(id)
        {
            self.report(row).await;
        }
    }

    /// Whether `c` gets a preview: [`wants`], then the path filters on what it touches (unless
    /// its title asked for one). Paths that can't be read count as a match: one change's diff
    /// never stops the project's other previews.
    pub(super) async fn wanted(&self, s: &Settings, c: &ChangeInfo) -> bool {
        if !wants(s, c) {
            return false;
        }
        if (s.paths.is_empty() && s.ignore_paths.is_empty()) || asks(c) == Some(true) {
            return true;
        }
        match self.paths_of(&c.repo, &c.head, None).await {
            Ok(paths) => paths.is_none_or(|p| touches(s, &p)),
            Err(e) => {
                warn!(change = %c.id, error = %e, "previews: reading a change's paths");
                true
            }
        }
    }

    /// The paths `head` changes since its merge base, or (`from`) since changeset `from`, cached
    /// (see `paths_seen`).
    async fn paths_of(&self, repo: &str, head: &str, from: Option<&str>) -> anyhow::Result<HeadPaths> {
        let key = (repo.to_string(), match from {
            Some(f) => format!("{f}..{head}"),
            None => head.to_string(),
        });
        if let Some(p) = self.paths_seen.lock().get(&key).cloned() {
            return Ok(p);
        }
        let p = match from {
            Some(f) => self.source.paths_between(repo, f, head).await?,
            None => self.source.paths(repo, head).await?,
        }
        .map(Arc::new);
        let mut seen = self.paths_seen.lock();
        if seen.len() >= PATHS_KEPT {
            seen.clear();
        }
        seen.insert(key, p.clone());
        Ok(p)
    }

    /// Whether the change's own services all run [`Row::build`] (none still waits for its deploy).
    pub(super) fn runs_build(&self, r: &Row) -> anyhow::Result<bool> {
        Ok(self.store.services(&r.id)?.iter().filter(|x| x.repo == r.repo).all(|x| x.deployed.as_deref() == Some(r.build())))
    }

    /// Whether every path that differs between `from` and `to` is one `ignore_paths` matches,
    /// and none is `roll.toml` (what runs). `paths` doesn't decide it: another service of the
    /// repo may run what it leaves out. False with no `ignore_paths`, or when that isn't known.
    pub(super) async fn only_ignored(&self, s: &Settings, repo: &str, from: &str, to: &str) -> bool {
        if s.ignore_paths.is_empty() {
            return false;
        }
        match self.paths_of(repo, to, Some(from)).await {
            Ok(paths) => paths.is_some_and(|p| all_ignored(s, &p)),
            Err(e) => {
                warn!(repo, error = %e, "previews: reading what a push changed");
                false
            }
        }
    }

    /// What another repo of the project runs in a preview of `branch`: its own `branch` when it
    /// has one (a feature split across repos previews as one), else its default branch's head
    /// (`default`, when the caller read it already).
    pub(super) async fn head_for(&self, repo: &str, branch: Option<&str>, default: Option<Option<String>>) -> anyhow::Result<Option<String>> {
        if let Some(b) = branch
            && let Some(h) = self.source.head(repo, Some(b)).await?
        {
            return Ok(Some(h));
        }
        match default {
            Some(d) => Ok(d),
            None => self.source.head(repo, None).await,
        }
    }

    /// The preview's secrets resolved as its code gets them, for `consumer` (a start of a service,
    /// a job), unresolved keys said once in its activity (a check fails on them; a preview starts
    /// without them, saying which). The leases of the dynamic credentials among them come back
    /// too: the caller keeps them while what it starts runs ([`Engine::keep_leases`]) or revokes
    /// them when it's done.
    /// Also the keys whose values are plain (a host, a port: roll secrets' `plain`, and a followed
    /// key whose field isn't a credential), which a preview's logs may show.
    pub(super) async fn resolve_secrets(&self, r: &Row, untrusted: bool, dynamic: bool, consumer: &str) -> anyhow::Result<(Vec<(String, String)>, Vec<String>, Vec<String>)> {
        let consumer = format!("preview:{}/{consumer}", env_name(&r.id));
        let got = self.secrets.resolve(&r.project_id, &env_name(&r.id), untrusted, dynamic, &consumer).await.map_err(|e| anyhow::anyhow!("secrets: {e:#}"))?;
        let mut problems = got.problems.clone();
        problems.sort();
        let new = self.secrets_told.lock().insert(r.id.clone(), problems.clone()).is_none_or(|before| before != problems);
        if new && !problems.is_empty() {
            let shown: Vec<&str> = problems.iter().take(10).map(String::as_str).collect();
            let more = problems.len().saturating_sub(shown.len());
            let _ = self.store.log(&r.id, &format!("secrets that don't resolve: {}{}", shown.join("; "), if more > 0 { format!(" (and {more} more)") } else { String::new() }));
        }
        // A followed key: the branch's value for that field, or blank (never the parent's).
        let mut values = got.values;
        let mut plain = got.plain;
        if !got.followed.is_empty() {
            let branches = self.branch_vars(&r.id).await?;
            for (name, kind, parent, field) in got.followed {
                let vars = branches.iter().find(|(k, p, _)| *k == kind && *p == parent).map(|(_, _, v)| v.clone()).unwrap_or_default();
                let value = crate::names::follow(kind, &field, vars);
                values.retain(|(k, _)| *k != name);
                plain.retain(|k| *k != name);
                if !masks::credential_name(&field) {
                    plain.push(name.clone());
                }
                values.push((name, value));
            }
        }
        Ok((values, got.leases, plain))
    }

    /// A start of `service` runs on `leases` now: the previous start's are revoked (that
    /// container is gone), these renewed while it runs ([`Engine::tend_leases`]).
    pub(super) async fn keep_leases(&self, r: &Row, service: &str, leases: &[String]) -> anyhow::Result<()> {
        let now = self.store.now_ms()?;
        // Until the first renewal says: roll secrets' shortest TTL.
        self.store.add_leases(&r.id, service, leases, now + LEASE_MIN_MS)?;
        for old in self.store.leases(Some(&r.id))?.into_iter().filter(|l| l.service == service && !leases.contains(&l.lease)) {
            self.revoke_lease(&r.project_id, &old).await;
        }
        Ok(())
    }

    /// End a lease, forgetting it once roll secrets has (a failure is tried again next pass).
    async fn revoke_lease(&self, project: &str, l: &crate::LeaseRow) {
        match self.secrets.revoke(project, &l.lease).await {
            Ok(()) => {
                let _ = self.store.drop_lease(&l.preview, &l.lease);
            }
            Err(e) => warn!(preview = %l.preview, lease = %l.lease, error = %e, "previews: revoking a dynamic credential"),
        }
    }

    /// What the egress proxies of this node's previews refused since they were last asked: said
    /// in each preview's activity, a host once.
    async fn note_refused_egress(&self) {
        let Ok(rows) = self.store.rows(None) else { return };
        for r in rows.into_iter().filter(|r| r.desired == Desired::Active) {
            let Ok(s) = self.store.settings(&r.project_id) else { continue };
            if s.egress_for(r.trusted) != crate::settings::EgressRule::Roll || s.egress_hosts.is_empty() {
                continue;
            }
            let local = match self.cluster.get() {
                Some(c) => matches!(c.route_async(&preview_key(&r.id)).await, Ok(roll_cluster::Route::Local(_))),
                None => true,
            };
            if !local {
                continue;
            }
            let fresh: Vec<String> = {
                let refused = self.apps.egress_refused(&r.id).await;
                let mut told = self.egress_told.lock();
                let seen = told.entry(r.id.clone()).or_default();
                // At most `EGRESS_TOLD_MAX` hosts said per preview: code trying made-up names
                // mustn't grow this, or push the rest of the activity out.
                let room = EGRESS_TOLD_MAX.saturating_sub(seen.len());
                let mut fresh: Vec<String> = refused.into_iter().filter(|h| !seen.contains(h)).collect();
                let more = fresh.len().saturating_sub(room);
                fresh.truncate(room);
                seen.extend(fresh.iter().cloned());
                if more > 0 && seen.len() >= EGRESS_TOLD_MAX && seen.insert("…".into()) {
                    let _ = self.store.log(&r.id, &format!("its code tried {more} more hosts outside roll (refused); only the first {EGRESS_TOLD_MAX} are named"));
                }
                fresh
            };
            if fresh.is_empty() {
                continue;
            }
            // A name is its code's to choose (`<secret>.example.com`): masked as its logs are.
            let Ok(values) = self.masks(&r.id).await else {
                let _ = self.store.log(&r.id, &format!("its code tried to reach {} hosts outside roll (refused)", fresh.len()));
                continue;
            };
            let fresh: Vec<String> = fresh.iter().map(|h| code_preview::runner::mask(h, &values)).collect();
            // `<host> private`: listed, but it resolves only to private addresses.
            let (private, unlisted): (Vec<String>, Vec<String>) = fresh.into_iter().partition(|h| h.ends_with(" private"));
            if !unlisted.is_empty() {
                let _ = self.store.log(&r.id, &format!("its code tried to reach {} (not in the project's egress_hosts): refused", unlisted.join(", ")));
            }
            if !private.is_empty() {
                let names: Vec<&str> = private.iter().map(|h| h.trim_end_matches(" private")).collect();
                let _ = self.store.log(&r.id, &format!("its code tried to reach {}: in egress_hosts, but it resolves only to private addresses (roll's own, the host's or the cloud's): refused", names.join(", ")));
            }
        }
    }

    /// The dynamic credentials this node's previews' services started with: revoked once the
    /// service isn't running here (asleep, failed, removed, moved to another node), renewed when
    /// their end is near. One that can't be renewed (revoked, at its longest life) puts its
    /// service to sleep, so the next request starts it with a new credential, said in the activity.
    pub(super) async fn tend_leases(&self) {
        let Ok(all) = self.store.leases(None) else { return };
        let Ok(now) = self.store.now_ms() else { return };
        let mut held: HashMap<String, Option<Row>> = HashMap::new();
        for l in all {
            if !held.contains_key(&l.preview) {
                let local = match self.cluster.get() {
                    Some(c) => matches!(c.route_async(&preview_key(&l.preview)).await, Ok(roll_cluster::Route::Local(_))),
                    None => true,
                };
                held.insert(l.preview.clone(), if local { self.store.row(&l.preview).ok() } else { None });
            }
            let Some(Some(r)) = held.get(&l.preview) else { continue };
            // Frozen counts: its process keeps the credential and is back on the next request.
            let running = matches!(self.apps.status(&l.preview, &l.service), Some(ServiceState::Running | ServiceState::Deploying)) || self.apps.warm(&l.preview, &l.service);
            if !running || r.desired == Desired::Deleted {
                self.revoke_lease(&r.project_id, &l).await;
                continue;
            }
            if l.expires_at - now > LEASE_RENEW_MS {
                continue;
            }
            match self.secrets.renew(&r.project_id, &l.lease).await {
                Ok(Some(until)) => {
                    let _ = self.store.set_lease_expiry(&l.preview, &l.lease, until);
                }
                Ok(None) => {
                    let _ = self.store.drop_lease(&l.preview, &l.lease);
                    let _ = self.store.log(&l.preview, &format!("a dynamic credential {} started with ended and can't be renewed: it sleeps, and starts with a new one on its next request", l.service));
                    self.apps.sleep(&l.preview, &l.service).await;
                }
                Err(e) => warn!(preview = %l.preview, lease = %l.lease, error = %e, "previews: renewing a dynamic credential"),
            }
        }
    }
}

/// How long a deploy may stay "still going" before the preview fails (a build queue that never
/// moves, say).
const DEPLOY_WAIT_MS: i64 = 20 * 60_000;

/// How many screenshots a node takes at once: `ROLL_PREVIEWS_SCREENSHOT_SLOTS` (1 to 8), else one
/// for every 8 cores, 1 to 4 (a run is two containers of 1 CPU and 1 GiB each: Chromium's and
/// the other browsers').
fn screenshot_slots() -> usize {
    let set = std::env::var("ROLL_PREVIEWS_SCREENSHOT_SLOTS").ok().and_then(|v| v.parse::<usize>().ok()).filter(|n| (1..=8).contains(n));
    set.unwrap_or_else(|| (std::thread::available_parallelism().map_or(1, |n| n.get()) / 8).clamp(1, 4))
}

/// roll secrets' shortest lease (a dynamic link's least `ttl_minutes`): what a new lease is taken
/// to last until its first renewal says.
const LEASE_MIN_MS: i64 = 15 * 60_000;
/// How close to its end a lease is renewed (each renewal is in roll secrets' audit log, so not on
/// every pass).
const LEASE_RENEW_MS: i64 = 10 * 60_000;

/// How soon a converge comes back to a deploy that wasn't live yet.
const DEPLOY_POLL_MS: i64 = 10_000;

/// How long a preview's secrets environment outlives its last converge (roll secrets deletes it
/// then, revoking its leases): a preview converges every few minutes while it exists.
const SECRETS_ENV_MARGIN_MS: i64 = 24 * 3_600_000;

/// What a resource is masked with under `s` (empty: nothing): a branch masked with other rules is
/// masked again.
pub(super) fn mask_digest(s: &Settings, res: &ResourceRow) -> String {
    mask_digest_of(s.masking.get(&format!("{}:{}", res.kind.as_str(), res.parent)))
}

pub(super) fn mask_digest_of(cols: Option<&BTreeMap<String, String>>) -> String {
    let Some(cols) = cols.filter(|c| !c.is_empty()) else { return String::new() };
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (k, v) in cols {
        for b in k.bytes().chain([0]).chain(v.bytes()).chain([0]) {
            h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

/// The shares of `max_usd_month` (percent) a project is told about, once a month each.
const BUDGET_LEVELS: [u32; 2] = [80, 100];

/// What a service is deployed with (its variables, its access, whether it may read protected
/// secrets), as a digest (FNV-1a: change detection, not a secret): a change to any redeploys it,
/// so a service that lost trust drops what it read.
fn config_fingerprint(vars: &[(String, String)], private: bool, bypass: Option<&str>, trusted: bool, feedback: bool) -> String {
    let mut sorted: Vec<&(String, String)> = vars.iter().collect();
    sorted.sort();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let bypass = ("\0bypass".to_string(), bypass.unwrap_or_default().to_string());
    for (k, v) in sorted.into_iter().chain([&bypass]) {
        for b in k.bytes().chain([0]).chain(v.bytes()).chain([0]) {
            h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{h:016x}{}{}{}", if private { "p" } else { "" }, if trusted { "t" } else { "" }, if feedback { "f" } else { "" })
}

/// Whether a change gets a preview under the project's rules (approval aside).
fn wants(s: &Settings, c: &ChangeInfo) -> bool {
    if !c.open {
        return false;
    }
    match asks(c) {
        Some(asked) => asked,
        None => !s.manual && (s.drafts || !c.draft) && crate::glob::any(&s.branches, c.branch.as_deref().unwrap_or("*")) && !crate::glob::any(&s.skip_authors, &c.author),
    }
}

/// The paths a head changes (`None`: not known).
type HeadPaths = Option<Arc<Vec<String>>>;

/// The project's variables for a preview of `branch`: `env`, then every `env_for` glob that
/// matches the branch (a preview with none counts as `*`), in the globs' order.
pub(super) fn project_env(s: &Settings, branch: Option<&str>) -> Vec<(String, String)> {
    let branch = branch.unwrap_or("*");
    let mut env: Vec<(String, String)> = s.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (glob, vars) in &s.env_for {
        if crate::glob::matches(glob, branch) {
            for (k, v) in vars {
                set(&mut env, k, v.clone());
            }
        }
    }
    env
}

/// Heads whose paths [`Engine::wanted`] keeps.
const PATHS_KEPT: usize = 4096;

/// Whether `paths` has one the path filters count: matched by `paths` (or `paths` is empty) and
/// not by `ignore_paths`.
fn touches(s: &Settings, paths: &[String]) -> bool {
    use code_control::review::pattern::{any_match, Pattern};
    let take: Vec<Pattern> = s.paths.iter().filter_map(|p| Pattern::new(p)).collect();
    let ignore: Vec<Pattern> = s.ignore_paths.iter().filter_map(|p| Pattern::new(p)).collect();
    paths.iter().any(|p| (take.is_empty() || any_match(&take, p)) && !any_match(&ignore, p))
}

/// Whether every one of `paths` is one `ignore_paths` matches, and none is `roll.toml`.
fn all_ignored(s: &Settings, paths: &[String]) -> bool {
    use code_control::review::pattern::{any_match, Pattern};
    let ignore: Vec<Pattern> = s.ignore_paths.iter().filter_map(|p| Pattern::new(p)).collect();
    paths.iter().all(|p| p != "roll.toml" && any_match(&ignore, p))
}

/// What a change says about its preview: its title's words, or its labels (`no-preview`,
/// `preview`; GitHub's or roll's). A skip wins over an ask.
fn asks(c: &ChangeInfo) -> Option<bool> {
    let label = |n: &str| c.labels.iter().any(|l| l.eq_ignore_ascii_case(n));
    match (title_says(&c.title), label("no-preview"), label("preview")) {
        (Some(false), _, _) | (_, true, _) => Some(false),
        (Some(true), _, _) | (_, _, true) => Some(true),
        _ => None,
    }
}

/// What a change's title says about its preview, as Render's `[skip render]` does: `[skip preview]`
/// (or `[preview skip]`, `[no preview]`) skips it, and `[preview]` asks for one the project's
/// `branches` or `drafts` rules would leave out. `None` leaves it to the rules.
fn title_says(title: &str) -> Option<bool> {
    let t = title.to_ascii_lowercase();
    if ["[skip preview]", "[preview skip]", "[no preview]"].iter().any(|m| t.contains(m)) {
        Some(false)
    } else if t.contains("[preview]") {
        Some(true)
    } else {
        None
    }
}

/// The secrets environment of a preview: its branch name (`pv-<id>`, within roll secrets'
/// 32-character slugs).
pub fn env_name(preview_id: &str) -> String {
    branch_name(preview_id)
}

fn short(rev: &str) -> &str {
    let rev = rev.strip_prefix("cs_").unwrap_or(rev);
    &rev[..rev.len().min(10)]
}


mod converge;
mod discover;
mod env;
mod masks;
pub use masks::ErrorMasks;
pub use requests::error_span;
mod requests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_rules() {
        let c = Config { port: Some(7070), domain: "preview.test".into(), ..Config::default() };
        assert_eq!(c.url("web-pr7-shop"), "http://web-pr7-shop.preview.test:7070");
        let https = Config { scheme: "https".into(), port: Some(443), ..c };
        assert_eq!(https.url("x"), "https://x.preview.test");
        let s = Settings { enabled: true, branches: vec!["feat/*".into()], ..Settings::default() };
        let ch = ChangeInfo {
            id: "chg_1".into(),
            repo: "a/b".into(),
            number: 1,
            title: "t".into(),
            branch: Some("feat/x".into()),
            head: "cs_1".into(),
            open: true,
            draft: false,
            labels: Vec::new(),
            trusted: true,
            approved: false,
            author: String::new(),
            github_pr: None,
        };
        assert!(wants(&s, &ch));
        assert!(!wants(&s, &ChangeInfo { branch: Some("fix/x".into()), ..ch.clone() }));
        assert!(!wants(&s, &ChangeInfo { draft: true, ..ch.clone() }));
        assert!(!wants(&s, &ChangeInfo { title: "Fix typo [Skip Preview]".into(), ..ch.clone() }));
        // A dependency bot's pull request is skipped by its author, unless a person asks.
        let bots = Settings { skip_authors: vec!["*[bot]".into()], ..s.clone() };
        let bump = ChangeInfo { author: "dependabot[bot]".into(), ..ch.clone() };
        assert!(wants(&bots, &ChangeInfo { author: "ada".into(), ..ch.clone() }) && !wants(&bots, &bump));
        assert!(wants(&bots, &ChangeInfo { labels: vec!["preview".into()], ..bump.clone() }), "a label asks");
        assert!(wants(&s, &ChangeInfo { title: "[preview] try the new nav".into(), branch: Some("fix/x".into()), draft: true, ..ch.clone() }));
        assert!(!wants(&s, &ChangeInfo { title: "[preview]".into(), open: false, ..ch.clone() }));
        // Unused for 6 of a week's hours: quiet; within a day: when. A 4-hour TTL warns an hour ahead.
        let h = 3_600_000;
        assert_eq!(goes_soon(0, None, None, 168, 100 * h), None);
        assert_eq!(goes_soon(0, None, None, 168, 150 * h), Some(168 * h));
        assert!(use_extends(0, None, 168, 168 * h));
        // Kept past its TTL: quiet until a day before the keep runs out, then that.
        assert_eq!(goes_soon(0, None, Some(200 * h), 168, 150 * h), None);
        assert_eq!(goes_soon(0, None, Some(200 * h), 168, 180 * h), Some(200 * h));
        assert!(!use_extends(0, None, 168, 200 * h));
        // A branch preview's fixed expiry: using it changes nothing.
        assert_eq!(goes_soon(0, Some(30 * h), None, 168, 0), None);
        assert_eq!(goes_soon(0, Some(30 * h), None, 168, 9 * h), Some(30 * h));
        assert!(!use_extends(0, Some(30 * h), 168, 30 * h));
        assert_eq!(goes_soon(0, None, None, 4, 2 * h), None);
        assert_eq!(goes_soon(0, None, None, 4, 3 * h), Some(4 * h));
        let manual = Settings { manual: true, ..s.clone() };
        assert!(!wants(&manual, &ch));
        let labelled = |l: &[&str]| ChangeInfo { labels: l.iter().map(|x| x.to_string()).collect(), ..ch.clone() };
        assert!(wants(&manual, &labelled(&["Preview"])));
        assert!(!wants(&s, &labelled(&["no-preview"])));
        assert!(!wants(&manual, &labelled(&["preview", "no-preview"])), "a skip wins");
        assert!(wants(&manual, &ChangeInfo { title: "Try the nav [Preview]".into(), ..ch.clone() }));
        let paths = |v: &[&str]| v.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        let docs = Settings { ignore_paths: paths(&["docs/", "*.md"]), ..s.clone() };
        assert!(!touches(&docs, &paths(&["docs/a/b.txt", "README.md"])));
        assert!(touches(&docs, &paths(&["README.md", "src/main.rs"])));
        let web = Settings { paths: paths(&["apps/web/**"]), ignore_paths: paths(&["*.md"]), ..s.clone() };
        assert!(touches(&web, &paths(&["apps/web/src/a.ts"])));
        assert!(!touches(&web, &paths(&["apps/web/README.md", "apps/api/x.rs"])));
        assert!(!touches(&web, &[]));
        assert_eq!(env_suffix("prj_ab-c"), "PRJ_AB_C");
    }
}

