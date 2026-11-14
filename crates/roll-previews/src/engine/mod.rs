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
}

impl Default for Config {
    fn default() -> Self {
        Config {
            domain: "preview.localhost".into(),
            scheme: "http".into(),
            port: None,
            interval: Duration::from_secs(15),
            usage_every: Duration::from_secs(300),
        }
    }
}

impl Config {
    pub fn url(&self, host: &str) -> String {
        let port = match (self.scheme.as_str(), self.port) {
            ("http", Some(80)) | ("https", Some(443)) | (_, None) => String::new(),
            (_, Some(p)) => format!(":{p}"),
        };
        format!("{}://{host}.{}{port}", self.scheme, self.domain)
    }
}

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
    usage_read: Mutex<HashMap<String, i64>>,
    /// Previews whose screenshot a converge asked for; taken after the pass (`pass`).
    shots_wanted: Mutex<std::collections::HashSet<String>>,
    /// One screenshot at a time.
    shooting: Arc<tokio::sync::Semaphore>,
    /// Services a converge just deployed at a change's new head, woken after the pass (`warm`).
    warm_wanted: Mutex<Vec<(String, String)>>,
    /// When a service's deploy was first found still going (`StillDeploying`): one that never
    /// finishes fails after [`DEPLOY_WAIT_MS`].
    deploying_since: Mutex<HashMap<(String, String), i64>>,
    /// The unresolved secrets last told per preview (each set once in its activity).
    secrets_told: Mutex<HashMap<String, Vec<String>>>,
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
            usage_read: Mutex::new(HashMap::new()),
            secrets_told: Mutex::new(HashMap::new()),
            paths_seen: Mutex::new(HashMap::new()),
            shots_wanted: Mutex::new(std::collections::HashSet::new()),
            shooting: Arc::new(tokio::sync::Semaphore::new(1)),
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
            let this = self.clone();
            let gate = self.shooting.clone();
            tokio::spawn(async move {
                let Ok(_one) = gate.acquire_owned().await else { return };
                let Ok(r) = this.store.row(&id) else { return };
                let Ok(s) = this.store.settings(&r.project_id) else { return };
                // Still wanted: ready, the rules still ask, not taken at this head meanwhile.
                if r.state != State::Ready || !s.github.screenshot || this.store.screenshot_head(&r.id).ok().flatten().as_deref() == Some(r.build()) {
                    return;
                }
                if this.take_screenshot(&r).await {
                    // The change and its pull request comment show the new one now, not at the
                    // preview's next converge.
                    if let Ok(row) = this.store.row(&id) {
                        this.report(row).await;
                    }
                }
            });
        }
        Ok(())
    }
}

impl Engine {
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

    /// The preview's secrets resolved as its code gets them, unresolved keys said once in its
    /// activity (a check fails on them; a preview starts without them, saying which).
    pub(super) async fn resolve_secrets(&self, r: &Row, untrusted: bool) -> anyhow::Result<Vec<(String, String)>> {
        let got = self.secrets.resolve(&r.project_id, &env_name(&r.id), untrusted).await.map_err(|e| anyhow::anyhow!("secrets: {e:#}"))?;
        let mut problems = got.problems.clone();
        problems.sort();
        let new = self.secrets_told.lock().insert(r.id.clone(), problems.clone()).is_none_or(|before| before != problems);
        if new && !problems.is_empty() {
            let shown: Vec<&str> = problems.iter().take(10).map(String::as_str).collect();
            let more = problems.len().saturating_sub(shown.len());
            let _ = self.store.log(&r.id, &format!("secrets that don't resolve: {}{}", shown.join("; "), if more > 0 { format!(" (and {more} more)") } else { String::new() }));
        }
        Ok(got.values)
    }
}

/// How long a deploy may stay "still going" before the preview fails (a build queue that never
/// moves, say).
const DEPLOY_WAIT_MS: i64 = 20 * 60_000;

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
fn config_fingerprint(vars: &[(String, String)], private: bool, bypass: Option<&str>, trusted: bool) -> String {
    let mut sorted: Vec<&(String, String)> = vars.iter().collect();
    sorted.sort();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let bypass = ("\0bypass".to_string(), bypass.unwrap_or_default().to_string());
    for (k, v) in sorted.into_iter().chain([&bypass]) {
        for b in k.bytes().chain([0]).chain(v.bytes()).chain([0]) {
            h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{h:016x}{}{}", if private { "p" } else { "" }, if trusted { "t" } else { "" })
}

/// Whether a change gets a preview under the project's rules (approval aside).
fn wants(s: &Settings, c: &ChangeInfo) -> bool {
    if !c.open {
        return false;
    }
    match asks(c) {
        Some(asked) => asked,
        None => !s.manual && (s.drafts || !c.draft) && crate::glob::any(&s.branches, c.branch.as_deref().unwrap_or("*")),
    }
}

/// The paths a head changes (`None`: not known).
type HeadPaths = Option<Arc<Vec<String>>>;

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
            github_pr: None,
        };
        assert!(wants(&s, &ch));
        assert!(!wants(&s, &ChangeInfo { branch: Some("fix/x".into()), ..ch.clone() }));
        assert!(!wants(&s, &ChangeInfo { draft: true, ..ch.clone() }));
        assert!(!wants(&s, &ChangeInfo { title: "Fix typo [Skip Preview]".into(), ..ch.clone() }));
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

