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
/// The first retry after a failed step, doubling up to [`MAX_BACKOFF_MS`].
const BACKOFF_MS: i64 = 15_000;
const MAX_BACKOFF_MS: i64 = 600_000;
/// A preview unused this long has its branches told it's idle ([`Brancher::idle`]).
pub const IDLE_AFTER_MS: i64 = 3_600_000;

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
    /// When a service's deploy was first found still going (`StillDeploying`): one that never
    /// finishes fails after [`DEPLOY_WAIT_MS`].
    deploying_since: Mutex<HashMap<(String, String), i64>>,
    /// The unresolved secrets last told per preview (each set once in its activity).
    secrets_told: Mutex<HashMap<String, Vec<String>>>,
    /// The paths a head changes (`repo`, `head`), for the path filters: a head's paths never
    /// change. Cleared past [`PATHS_KEPT`] heads.
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
    /// What another repo of the project runs in a preview of `branch`: its own `branch` when it
    /// has one (a feature split across repos previews as one), else its default branch's head
    /// (`default`, when the caller read it already).
    /// Whether `c` gets a preview: [`wants`], then the path filters on what it touches (unless
    /// its title asked for one).
    pub(super) async fn wanted(&self, s: &Settings, c: &ChangeInfo) -> anyhow::Result<bool> {
        if !wants(s, c) {
            return Ok(false);
        }
        if (s.paths.is_empty() && s.ignore_paths.is_empty()) || title_says(&c.title) == Some(true) {
            return Ok(true);
        }
        let paths = self.paths_of(&c.repo, &c.head, None).await?;
        Ok(paths.is_none_or(|p| touches(s, &p)))
    }

    /// The paths `head` changes since its merge base, or (`from`) since changeset `from`: cached,
    /// since neither ever changes.
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

    /// Whether every path that differs between `from` and `to` is one the path filters leave
    /// out (false with no filters, or when that isn't known).
    pub(super) async fn only_ignored(&self, s: &Settings, repo: &str, from: &str, to: &str) -> anyhow::Result<bool> {
        if s.paths.is_empty() && s.ignore_paths.is_empty() {
            return Ok(false);
        }
        Ok(self.paths_of(repo, to, Some(from)).await?.is_some_and(|p| !touches(s, &p)))
    }

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
    match title_says(&c.title) {
        Some(asked) => asked,
        None => (s.drafts || !c.draft) && crate::glob::any(&s.branches, c.branch.as_deref().unwrap_or("*")),
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

