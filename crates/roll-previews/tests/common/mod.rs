//! Fake products for the reconciler's tests.
#![allow(dead_code)]

use async_trait::async_trait;
use parking_lot::Mutex;
use roll_previews::engine::Config;
use roll_previews::ports::{Apps, Branched, Brancher, ChangeInfo, ServiceSpec, Source, Target, Usage};
use roll_previews::{Engine, Kind, Mode, Preview, ServiceState, Settings, Store};
use roll_workspace::Workspace;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
pub struct FakeCode {
    pub changes: Mutex<HashMap<String, ChangeInfo>>,
    pub heads: Mutex<HashMap<String, String>>,
    /// The paths a head changes, by head (unlisted: not known).
    pub paths: Mutex<HashMap<String, Vec<String>>>,
    /// Each head's git commit (unlisted: none).
    pub git: Mutex<HashMap<String, String>>,
    /// The paths between two heads, by `"<from>..<to>"` (unlisted: not known).
    pub between: Mutex<HashMap<String, Vec<String>>>,
    /// What every repo's service watches (`roll.toml` `watch`).
    pub watch: Mutex<Vec<String>>,
    /// The feedback left on changes' preview pages, by change.
    pub feedback: Mutex<HashMap<String, Vec<roll_previews::notes::Note>>>,
    /// The `health` path every service says it has.
    pub health: Mutex<Option<String>>,
    /// The page states every service's `roll.toml` names (`[[preview.states]]`).
    pub repo_states: Mutex<Vec<roll_previews::settings::StateRule>>,
    /// What every roll apps service's `[app.previews.screenshots]` says, and the roots it was read at.
    pub app_screenshots: Mutex<Option<code_preview::config::Screenshots>>,
    pub app_roots: Mutex<Vec<String>>,
    pub reports: Mutex<Vec<Preview>>,
    pub forgotten: Mutex<Vec<String>>,
    /// Previews that went with their teardown failing: (preview, what it said).
    pub teardowns: Mutex<Vec<(String, String)>>,
    /// Budget alerts sent: (the repos told, the alert).
    pub budgets: Mutex<Vec<(Vec<String>, roll_previews::ports::BudgetAlert)>>,
}

impl FakeCode {
    pub fn open(&self, c: ChangeInfo) {
        self.changes.lock().insert(c.id.clone(), c);
    }

    pub fn edit(&self, id: &str, f: impl FnOnce(&mut ChangeInfo)) {
        f(self.changes.lock().get_mut(id).unwrap());
    }

    pub fn last_report(&self, change: &str) -> Preview {
        self.reports.lock().iter().rev().find(|p| p.change.as_deref() == Some(change)).cloned().expect("a report")
    }
}

#[async_trait]
impl Source for FakeCode {
    async fn open_changes(&self, repo: &str) -> anyhow::Result<Vec<ChangeInfo>> {
        Ok(self.changes.lock().values().filter(|c| c.repo == repo && c.open).cloned().collect())
    }
    async fn git_commit(&self, _repo: &str, revision: &str) -> anyhow::Result<Option<String>> {
        Ok(self.git.lock().get(revision).cloned())
    }
    async fn screenshots(&self, _repo: &str, _revision: &str, root: &str) -> anyhow::Result<Option<code_preview::config::Screenshots>> {
        self.app_roots.lock().push(root.to_string());
        Ok(self.app_screenshots.lock().clone())
    }
    async fn change(&self, id: &str) -> anyhow::Result<Option<ChangeInfo>> {
        Ok(self.changes.lock().get(id).cloned())
    }
    async fn head(&self, repo: &str, branch: Option<&str>) -> anyhow::Result<Option<String>> {
        Ok(self.heads.lock().get(&format!("{repo}@{}", branch.unwrap_or("main"))).cloned())
    }
    async fn merge_base(&self, _repo: &str, _head: &str) -> anyhow::Result<Option<String>> {
        // Every change leaves main where the fake's main is.
        Ok(self.heads.lock().get("acme/web@main").cloned())
    }
    async fn default_branch(&self, _repo: &str) -> anyhow::Result<Option<String>> {
        Ok(Some("main".into()))
    }
    async fn paths(&self, _repo: &str, head: &str) -> anyhow::Result<Option<Vec<String>>> {
        Ok(self.paths.lock().get(head).cloned())
    }
    async fn feedback(&self, change: &str) -> anyhow::Result<Vec<roll_previews::notes::Note>> {
        Ok(self.feedback.lock().get(change).cloned().unwrap_or_default())
    }
    async fn paths_between(&self, _repo: &str, from: &str, to: &str) -> anyhow::Result<Option<Vec<String>>> {
        Ok(self.between.lock().get(&format!("{from}..{to}")).cloned())
    }
    async fn services(&self, repo: &str, _revision: &str) -> anyhow::Result<Vec<ServiceSpec>> {
        let name = repo.rsplit('/').next().unwrap().to_string();
        Ok(vec![ServiceSpec { name, image: "node:22".into(), run: "node app.js".into(), port: 3000, memory_mb: 512, cpus: 0.5, env: vec![], health: self.health.lock().clone(), states: self.repo_states.lock().clone(), pages: vec![], hide: vec![], watch: self.watch.lock().clone() }])
    }
    async fn report(&self, p: &Preview, _s: &Settings) {
        self.reports.lock().push(p.clone());
    }
    async fn forget(&self, p: &Preview, teardown: Option<&str>) {
        self.forgotten.lock().push(p.id.clone());
        if let Some(t) = teardown {
            self.teardowns.lock().push((p.id.clone(), t.to_string()));
        }
    }
    /// A plain body from the preview's state (roll code builds the real one from its summary).
    fn git_report_body(&self, p: &Preview, _s: &Settings, deleted: Option<Option<&str>>) -> Option<roll_previews::ports::GitReportBody> {
        let state = match (deleted, p.state) {
            (Some(_), _) => None,
            (None, roll_previews::State::Ready | roll_previews::State::Sleeping) => Some("success"),
            (None, roll_previews::State::Failed) => Some("failure"),
            (None, _) => Some("pending"),
        };
        Some(roll_previews::ports::GitReportBody {
            state: state.map(str::to_string),
            description: format!("{:?}", p.state),
            target_url: p.services.first().map(|s| s.url.clone()),
            comment: format!("{:?} at {}{}", p.state, p.head, if deleted.is_some() { ", deleted" } else { "" }),
        })
    }
    async fn budget(&self, repos: &[String], alert: &roll_previews::ports::BudgetAlert) -> anyhow::Result<()> {
        self.budgets.lock().push((repos.to_vec(), alert.clone()));
        Ok(())
    }
    fn nudges(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        None
    }
}

/// A product whose branches are `<parent>~<name>`, refusing to touch a parent.
pub struct FakeData {
    pub kind: Kind,
    pub branches: Mutex<HashSet<String>>,
    pub parents: HashSet<String>,
    pub made: AtomicUsize,
    pub fail_next: AtomicUsize,
    /// What `schema` answers: (the parent's lines, the branch's).
    pub schema: Mutex<Option<(Vec<String>, Vec<String>)>>,
    /// What reading a branch alone finds (unset: its parent's lines, as a branch is when it's made).
    pub branch_now: Mutex<Option<Vec<String>>>,
    /// Masks applied: (branch, columns).
    pub masks: Mutex<Vec<(String, std::collections::BTreeMap<String, String>)>>,
    /// Branched from history at (Postgres's fake keeps history; the others don't).
    pub at: Mutex<Vec<i64>>,
    /// What `columns` answers, by parent (`Err`: it sleeps).
    pub columns: Mutex<std::collections::HashMap<String, Result<Vec<roll_previews::mask::Column>, roll_previews::ports::Asleep>>>,
}

impl FakeData {
    pub fn new(kind: Kind, parents: &[&str]) -> Arc<FakeData> {
        Arc::new(FakeData {
            kind,
            branches: Mutex::new(HashSet::new()),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            made: AtomicUsize::new(0),
            fail_next: AtomicUsize::new(0),
            schema: Mutex::new(None),
            masks: Mutex::new(Vec::new()),
            at: Mutex::new(Vec::new()),
            columns: Mutex::new(Default::default()),
            branch_now: Mutex::new(None),
        })
    }
}

#[async_trait]
impl Brancher for FakeData {
    fn kind(&self) -> Kind {
        self.kind
    }
    async fn branch(&self, parent: &str, name: &str, _mode: Mode) -> anyhow::Result<Branched> {
        if self.fail_next.load(Ordering::SeqCst) > 0 {
            self.fail_next.fetch_sub(1, Ordering::SeqCst);
            anyhow::bail!("the product is busy");
        }
        anyhow::ensure!(self.parents.contains(parent), "no parent {parent}");
        let id = format!("{parent}~{name}");
        if self.branches.lock().insert(id.clone()) {
            self.made.fetch_add(1, Ordering::SeqCst);
        }
        Ok(Branched::plain(id))
    }
    async fn branch_at(&self, parent: &str, name: &str, mode: Mode, at: i64) -> anyhow::Result<Option<Branched>> {
        if self.kind != Kind::Postgres {
            return Ok(None);
        }
        self.at.lock().push(at);
        self.branch(parent, name, mode).await.map(Some)
    }
    async fn snapshot(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<Option<Branched>> {
        if !matches!(self.kind, Kind::Postgres | Kind::Redis) {
            return Ok(None);
        }
        anyhow::ensure!(self.branches.lock().contains(branch), "no branch {branch} to copy");
        let id = format!("{parent}~{name}");
        self.branches.lock().insert(id.clone());
        Ok(Some(Branched::plain(id)))
    }
    async fn mask(&self, _parent: &str, branch: &str, _kept: Option<&str>, columns: &std::collections::BTreeMap<String, String>) -> anyhow::Result<()> {
        self.masks.lock().push((branch.to_string(), columns.clone()));
        Ok(())
    }
    async fn schema(&self, _parent: &str, _branch: &str) -> anyhow::Result<Option<(Vec<String>, Vec<String>)>> {
        Ok(self.schema.lock().clone())
    }
    async fn branch_schema(&self, _parent: &str, _branch: &str) -> anyhow::Result<Option<Vec<String>>> {
        // As it's made, a branch is its parent.
        if let Some(now) = self.branch_now.lock().clone() {
            return Ok(Some(now));
        }
        Ok(self.schema.lock().clone().map(|(parent, _)| parent))
    }
    async fn columns(&self, parent: &str, max: usize) -> anyhow::Result<Vec<roll_previews::mask::Column>> {
        match self.columns.lock().get(parent) {
            Some(Err(_)) => Err(roll_previews::ports::Asleep.into()),
            Some(Ok(c)) => Ok(c.iter().take(max).cloned().collect()),
            None => Ok(Vec::new()),
        }
    }
    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }
    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        anyhow::ensure!(branch == format!("{parent}~{name}"), "refusing to delete {branch}: not {parent}'s branch {name}");
        self.branches.lock().remove(branch);
        Ok(())
    }
    async fn connect(&self, _parent: &str, branch: &str, _kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let var = match self.kind {
            Kind::Postgres => "DATABASE_URL",
            _ => "REDIS_URL",
        };
        Ok(vec![(var.into(), format!("fake://{branch}"))])
    }
    async fn find(&self, parent: &str, name: &str) -> anyhow::Result<Option<String>> {
        let id = format!("{parent}~{name}");
        Ok(self.branches.lock().contains(&id).then_some(id))
    }
    async fn usage(&self, _parent: &str, _branch: &str) -> anyhow::Result<Usage> {
        Ok(Usage { bytes: 1 << 20, storage_usd_month: 0.001, active_usd_hour: 0.04, month_to_date_usd: 0.01 })
    }
}

/// A deploy's variables.
pub type Vars = Vec<(String, String)>;

#[derive(Default)]
pub struct FakeApps {
    pub deployed: Mutex<HashMap<(String, String), String>>,
    /// Every deploy a deploy hook asked for: (preview, service, its rebuild number).
    pub rebuilt: Mutex<Vec<(String, String, i64)>>,

    /// Every project's own preview domain, verified (roll apps' project preview domain).
    pub domain: Mutex<Option<(String, bool)>>,
    /// roll apps' catalog for the project (unset: none, so roll.toml's services), and the memory
    /// its preview environments run with.
    pub catalog: Mutex<Option<Vec<roll_previews::ports::CatalogService>>>,
    pub memory: Mutex<Option<u32>>,
    /// Services woken (warmed after a push, or asked to wake).
    pub woken: Mutex<Vec<(String, String)>>,
    /// The build each service runs now, by service: (git commit, its own URL).
    pub live: Mutex<HashMap<String, (String, String)>>,
    /// What a service answers on its health path (unset: the backend can't tell).
    pub answers: Mutex<Option<(u16, String)>>,
    /// How many health checks find the service still starting (asked again later).
    pub starting: std::sync::atomic::AtomicUsize,
    /// The variables each service was last deployed with.
    pub vars: Mutex<HashMap<(String, String), Vars>>,
    pub removed: Mutex<Vec<(String, String)>>,
    /// What each service uses, by name (unset: not running).
    pub metrics: Mutex<HashMap<String, (f64, u64)>>,
    /// Each service's errors, by name.
    /// Each service's error groups, by `preview/service` or by service for every preview.
    pub errors: Mutex<HashMap<String, Vec<roll_previews::ErrorGroup>>>,
    /// What every service's log holds.
    pub log: Mutex<Vec<String>>,
    /// What a followed log gives, each line with whether it came on stderr (unset: the backend
    /// can't follow, so the engine reads `log` again and again).
    pub followed: Mutex<Option<Vec<(bool, String)>>>,
    /// What each seed or release job was run able to reach.
    pub seed_egress: Mutex<Vec<roll_previews::settings::EgressRule>>,
    /// What each service was deployed able to reach.
    pub egress: Mutex<HashMap<(String, String), roll_previews::settings::EgressRule>>,
    /// Seeds run (their DATABASE_URL), and the exit code the next ones give.
    pub seeds: Mutex<Vec<String>>,
    pub seed_exit: std::sync::atomic::AtomicI32,
    /// Every seed or release run: (its command, the revision), and how many services were
    /// deployed at that revision when it ran.
    pub runs: Mutex<Vec<(String, String, usize)>>,
    /// The next deploys that answer "still deploying", then the next that fail.
    pub still: AtomicUsize,
    pub fail_deploy: AtomicUsize,
    /// The next removals that fail.
    pub fail_remove: AtomicUsize,
    /// Runs inside a deploy (an API call landing while the reconciler works).
    pub on_deploy: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// Which service each screenshot run was of.
    pub shot_services: Mutex<Vec<String>>,
    /// The notes each screenshot was asked to crop (a `#missing` selector finds nothing).
    pub crops_asked: Mutex<Vec<String>>,
    /// Where each drain follower asked to start (`drain_from`'s `since`), and the end the test
    /// writes the followed container's lines into (`(stderr, "<stamp> <text>")`).
    pub drain_since: Mutex<Vec<String>>,
    pub drain_tx: Mutex<Option<tokio::sync::mpsc::Sender<(bool, String)>>>,
    /// A roll apps backend's own traces (its `pv-` environment's), and their spans; and whether each
    /// deploy asked for them (the project's `traces`).
    pub app_traces: Mutex<Vec<(roll_previews::engine::traces::TraceRow, Vec<roll_previews::engine::traces::Span>)>>,
    pub traces_asked: Mutex<HashMap<(String, String), bool>>,
    /// Tars written into services, by the directory they were written under.
    pub files: Mutex<HashMap<String, Vec<u8>>>,
    /// Services the runtime says run on this node, and those put to sleep.
    pub running: Mutex<std::collections::HashSet<(String, String)>>,
    pub slept: Mutex<Vec<(String, String)>>,
    /// The PNG every screenshot is (unset: the backend can't take one), and the previews taken.
    pub shot: Mutex<Option<Vec<u8>>>,
    /// What the screenshot's page logs as its first error.
    pub page_says: Mutex<Option<String>>,
    /// The API operations a screenshot reads when it's asked to (`github.openapi`), by a
    /// revision the preview runs.
    pub api: Mutex<HashMap<String, Vec<String>>>,
    /// Each service's last deploy asked for the feedback button.
    pub feedback: Mutex<HashMap<(String, String), bool>>,
    pub shots: Mutex<Vec<String>>,
    /// The page states each screenshot was asked to reach, by name.
    pub states: Mutex<Vec<String>>,
    /// The other browsers' run fails (no `browser:` pages come back).
    pub no_browsers: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl Apps for FakeApps {
    async fn deploy(&self, t: &Target) -> anyhow::Result<Option<String>> {
        if let Some(f) = self.on_deploy.lock().as_ref() {
            f();
        }
        // What the backend was sent, whatever it answers.
        self.vars.lock().insert((t.preview.clone(), t.service.clone()), t.vars.clone());
        if self.still.load(Ordering::SeqCst) > 0 {
            self.still.fetch_sub(1, Ordering::SeqCst);
            return Err(roll_previews::ports::StillDeploying("building".into()).into());
        }
        if self.fail_deploy.load(Ordering::SeqCst) > 0 {
            self.fail_deploy.fetch_sub(1, Ordering::SeqCst);
            anyhow::bail!("the build failed");
        }
        self.deployed.lock().insert((t.preview.clone(), t.service.clone()), t.revision.clone());
        if let Some(n) = t.rebuild {
            self.rebuilt.lock().push((t.preview.clone(), t.service.clone(), n));
        }
        self.vars.lock().insert((t.preview.clone(), t.service.clone()), t.vars.clone());
        self.feedback.lock().insert((t.preview.clone(), t.service.clone()), t.feedback);
        self.egress.lock().insert((t.preview.clone(), t.service.clone()), t.egress);
        self.traces_asked.lock().insert((t.preview.clone(), t.service.clone()), t.traces);
        Ok(None)
    }
    async fn catalog(&self, _project: &str) -> anyhow::Result<Option<Vec<roll_previews::ports::CatalogService>>> {
        Ok(self.catalog.lock().clone())
    }
    async fn preview_domain(&self, _project: &str) -> Option<(String, bool)> {
        self.domain.lock().clone()
    }
    async fn memory_mb(&self, _: &str, _: &str) -> Option<u32> {
        *self.memory.lock()
    }
    async fn probe(&self, _: &str, _: &str, _path: &str) -> anyhow::Result<Option<(u16, String)>> {
        if self.starting.load(Ordering::SeqCst) > 0 {
            self.starting.fetch_sub(1, Ordering::SeqCst);
            return Err(roll_previews::ports::StillDeploying("starting".into()).into());
        }
        Ok(self.answers.lock().clone())
    }
    async fn wake(&self, preview: &str, service: &str) -> anyhow::Result<()> {
        self.woken.lock().push((preview.to_string(), service.to_string()));
        Ok(())
    }
    async fn sleep(&self, p: &str, s: &str) {
        self.slept.lock().push((p.to_string(), s.to_string()));
    }
    async fn remove(&self, p: &str, s: &str) -> anyhow::Result<()> {
        if self.fail_remove.load(Ordering::SeqCst) > 0 {
            self.fail_remove.fetch_sub(1, Ordering::SeqCst);
            anyhow::bail!("the runtime is busy");
        }
        self.deployed.lock().remove(&(p.to_string(), s.to_string()));
        self.removed.lock().push((p.to_string(), s.to_string()));
        Ok(())
    }
    async fn metrics(&self, _: &str, service: &str) -> Option<(f64, u64)> {
        self.metrics.lock().get(service).copied()
    }
    async fn live_build(&self, _: &str, service: &str) -> Option<(String, String)> {
        self.live.lock().get(service).cloned()
    }
    async fn errors(&self, preview: &str, service: &str, _: i64, _: Option<&roll_previews::engine::ErrorMasks<'_>>) -> roll_previews::Result<Vec<roll_previews::ErrorGroup>> {
        // `pv_…/web` (one preview's) over `web` (every preview's).
        let errors = self.errors.lock();
        Ok(errors.get(&format!("{preview}/{service}")).or_else(|| errors.get(service)).cloned().unwrap_or_default())
    }
    async fn logs(&self, _: &str, _: &str, _: usize) -> Vec<String> {
        self.log.lock().clone()
    }
    async fn follow(&self, _: &str, _: &str, _: usize) -> Option<tokio::sync::mpsc::Receiver<(bool, String)>> {
        let lines = self.followed.lock().clone()?;
        let (tx, rx) = tokio::sync::mpsc::channel(lines.len().max(1));
        for l in lines {
            let _ = tx.try_send(l);
        }
        Some(rx)
    }
    async fn traces(&self, _: &str, _: &str, _: i64) -> Vec<roll_previews::engine::traces::TraceRow> {
        self.app_traces.lock().iter().map(|(r, _)| r.clone()).collect()
    }
    async fn trace(&self, _: &str, _: &str, trace: &str) -> Option<Vec<roll_previews::engine::traces::Span>> {
        self.app_traces.lock().iter().find(|(r, _)| r.trace == trace).map(|(_, s)| s.clone())
    }
    /// What was written under `path` (nothing: an empty stream, as `docker cp` of a missing path).
    async fn read_files(&self, _: &str, _: &str, path: &str) -> anyhow::Result<Option<roll_previews::ports::FileStream>> {
        let got = self.files.lock().get(path).cloned().unwrap_or_default();
        Ok(Some(Box::new(std::io::Cursor::new(got))))
    }
    async fn write_files(&self, _: &str, _: &str, dir: &str, tar: bytes::Bytes) -> anyhow::Result<bool> {
        self.files.lock().insert(dir.to_string(), tar.to_vec());
        Ok(true)
    }
    /// A forwarded port that echoes what it's sent.
    async fn forward(&self, _: &str, _: &str, _: u16) -> anyhow::Result<Option<Box<dyn roll_previews::ports::ShellIo>>> {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            let (mut r, mut w) = tokio::io::split(theirs);
            let _ = tokio::io::copy(&mut r, &mut w).await;
        });
        Ok(Some(Box::new(ours)))
    }
    /// A terminal that echoes what it's sent.
    async fn shell(&self, _: &str, service: &str, _: u16, _: u16) -> anyhow::Result<Option<roll_previews::ports::Shell>> {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            let (mut r, mut w) = tokio::io::split(theirs);
            let _ = tokio::io::copy(&mut r, &mut w).await;
        });
        Ok(Some(roll_previews::ports::Shell { id: format!("exec-{service}"), io: Box::new(ours) }))
    }
    async fn drain_from(&self, _: &str, _: &str, since: &str) -> Option<tokio::sync::mpsc::Receiver<(bool, String)>> {
        self.drain_since.lock().push(since.to_string());
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        *self.drain_tx.lock() = Some(tx);
        Some(rx)
    }
    async fn seed(&self, job: &roll_previews::ports::SeedJob) -> anyhow::Result<(i32, String)> {
        let db = job.env.iter().find(|(k, _)| k == "DATABASE_URL").map(|(_, v)| v.clone()).unwrap_or_default();
        self.seeds.lock().push(db);
        self.seed_egress.lock().push(job.egress);
        let at_rev = self.deployed.lock().values().filter(|r| **r == job.revision).count();
        self.runs.lock().push((job.seed.run.clone(), job.revision.clone(), at_rev));
        // A job that prints what it was given.
        let printed: Vec<String> = job.env.iter().filter(|(k, _)| k == "STRIPE_KEY").map(|(k, v)| format!(", {k}={v}")).collect();
        Ok((self.seed_exit.load(Ordering::SeqCst), format!("seed output{}", printed.concat())))
    }
    async fn screenshot(&self, preview: &str, service: &str, opts: &roll_previews::ports::ShotOptions) -> anyhow::Result<Option<roll_previews::ports::Shot>> {
        let Some(png) = self.shot.lock().clone() else { return Ok(None) };
        self.shot_services.lock().push(service.to_string());
        // The API the preview serves: by any revision it runs (whichever service is first).
        let revs: Vec<String> = self.deployed.lock().iter().filter(|((p, _), _)| p == preview).map(|(_, r)| r.clone()).collect();
        let api = opts.openapi.as_ref().and_then(|_| revs.iter().find_map(|r| self.api.lock().get(r).cloned()));
        self.shots.lock().push(preview.to_string());
        self.states.lock().extend(opts.states.iter().map(|s| s.name.clone()));
        let pages = opts
            .pages
            .iter()
            .cloned()
            .chain(opts.browsers.iter().filter(|_| !self.no_browsers.load(Ordering::SeqCst)).map(|b| format!("browser:{b}")))
            .map(|path| roll_previews::ports::PageShot { path, png: png.clone(), errors: 0, report: Default::default() })
            .collect();
        self.crops_asked.lock().extend(opts.crops.iter().map(|c| c.note.clone()));
        let crops = opts.crops.iter().filter(|c| c.selector != "#missing").map(|c| (c.note.clone(), png.clone())).collect();
        let health = self.page_says.lock().clone().map(|said| roll_previews::PageHealth { errors: 1, first_error: Some(said), failed: 0, first_failed: None, unstable: false });
        Ok(Some(roll_previews::ports::Shot { png, speed: None, health, a11y: None, phone: None, pages, text: None, links: None, api, crops }))
    }
    fn status(&self, p: &str, s: &str) -> Option<ServiceState> {
        self.running.lock().contains(&(p.to_string(), s.to_string())).then_some(ServiceState::Running)
    }
    fn warm(&self, _: &str, _: &str) -> bool {
        false
    }
    async fn egress_refused(&self, _: &str) -> Vec<String> {
        Vec::new()
    }
    async fn intercepted(&self, _: &str) -> Option<roll_previews::AppsIntercepted> {
        None
    }
}

/// roll secrets as previews see it: every `ensure` (environment, trusted). With `dynamic`, each
/// A Git URL service's repository: records each status and comment (a comment gets id 77,
/// edits name it). `connected: false` posts nothing.
/// A project's log drains: whether they take previews' output (every source), and every line
/// sent (environment, service, source, line).
#[derive(Default)]
pub struct FakeDrains {
    pub wanted: std::sync::atomic::AtomicBool,
    pub sent: Mutex<Vec<(String, String, &'static str, roll_previews::ports::DrainLine)>>,
}

#[async_trait]
impl roll_previews::ports::Drains for FakeDrains {
    async fn wanted(&self, _: &str, _: roll_previews::ports::DrainSource) -> bool {
        self.wanted.load(Ordering::SeqCst)
    }
    fn send(&self, _: &str, environment: &str, service: &str, source: roll_previews::ports::DrainSource, lines: Vec<roll_previews::ports::DrainLine>) {
        self.sent.lock().extend(lines.into_iter().map(|l| (environment.to_string(), service.to_string(), source.as_str(), l)));
    }
}

/// A posted status: service, sha, state, description, link.
pub type PostedStatus = (String, String, String, String, Option<String>);

pub struct FakeGitReport {
    pub connected: std::sync::atomic::AtomicBool,
    pub statuses: Mutex<Vec<PostedStatus>>,
    pub comments: Mutex<Vec<(u64, Option<u64>, String)>>,
}

impl Default for FakeGitReport {
    fn default() -> Self {
        FakeGitReport { connected: std::sync::atomic::AtomicBool::new(true), statuses: Mutex::new(Vec::new()), comments: Mutex::new(Vec::new()) }
    }
}

#[async_trait]
impl roll_previews::ports::GitReport for FakeGitReport {
    async fn status(&self, service: &str, sha: &str, state: &str, description: &str, target_url: Option<&str>) -> anyhow::Result<bool> {
        if !self.connected.load(Ordering::SeqCst) {
            return Ok(false);
        }
        self.statuses.lock().push((service.into(), sha.into(), state.into(), description.into(), target_url.map(Into::into)));
        Ok(true)
    }
    async fn comment(&self, _service: &str, _repo: &str, number: u64, body: &str, existing: Option<u64>) -> anyhow::Result<Option<u64>> {
        if !self.connected.load(Ordering::SeqCst) {
            return Ok(None);
        }
        self.comments.lock().push((number, existing, body.into()));
        Ok(Some(77))
    }
}

/// resolve issues a credential (`DB_PASSWORD=pw-<n>` under lease `lease-<n>`); renewals and
/// revocations are recorded, and a lease in `ended` can't be renewed.
#[derive(Default)]
pub struct FakeSecrets {
    pub ensured: Mutex<Vec<(String, bool)>>,
    pub dynamic: std::sync::atomic::AtomicBool,
    pub issued: AtomicUsize,
    pub renewed: Mutex<Vec<String>>,
    pub revoked: Mutex<Vec<String>>,
    pub ended: Mutex<std::collections::HashSet<String>>,
    /// What each ensure said the preview branched.
    pub branched: Mutex<Vec<(Kind, String)>>,
    /// Keys resolve answers as followed (links to a branched parent: nothing of it resolved).
    pub followed: Mutex<Vec<(String, Kind, String, String)>>,
    /// The base's links (`links`), and what each ensure set as the environment's own keys.
    pub links: Mutex<Vec<(String, Kind, String, String)>>,
    pub own: Mutex<Vec<(String, String)>>,
    /// The references the last ensure sent.
    pub refs: Mutex<Vec<(String, String)>>,
    /// Resolves that asked for placeholders: (consumer, maker).
    pub placeholders: Mutex<Vec<(String, String)>>,
    /// What those resolves answer as placeholders: (key, its bound hosts).
    pub placeholder_hosts: Mutex<Vec<(String, Vec<String>)>>,
    /// Values every resolve answers (static secrets).
    pub values: Mutex<Vec<(String, String)>>,
    /// Why each resolve was asked for (`logs`: values read to mask output).
    pub reasons: Mutex<Vec<String>>,
}

#[async_trait]
impl roll_previews::ports::Secrets for FakeSecrets {
    async fn ensure(&self, _: &str, env: &str, _: &str, vars: &[(String, String)], refs: &[(String, String)], trusted: bool, _: i64, branches: &[(Kind, String)]) -> anyhow::Result<()> {
        self.ensured.lock().push((env.to_string(), trusted));
        *self.refs.lock() = refs.to_vec();
        *self.branched.lock() = branches.to_vec();
        *self.own.lock() = vars.to_vec();
        Ok(())
    }
    async fn resolve_with_placeholders(&self, project: &str, env: &str, untrusted: bool, dynamic: bool, consumer: &str, maker: &str) -> anyhow::Result<roll_previews::ports::Resolved> {
        self.placeholders.lock().push((consumer.to_string(), maker.to_string()));
        let mut got = self.resolve(project, env, untrusted, dynamic, consumer).await?;
        got.placeholders = self.placeholder_hosts.lock().clone();
        Ok(got)
    }
    async fn resolve(&self, _: &str, _: &str, _: bool, dynamic: bool, reason: &str) -> anyhow::Result<roll_previews::ports::Resolved> {
        self.reasons.lock().push(reason.to_string());
        let followed = self.followed.lock().clone();
        let values = self.values.lock().clone();
        if !dynamic || !self.dynamic.load(Ordering::SeqCst) {
            return Ok(roll_previews::ports::Resolved { followed, values, ..Default::default() });
        }
        let n = self.issued.fetch_add(1, Ordering::SeqCst) + 1;
        let values = values.into_iter().chain([("DB_PASSWORD".into(), format!("pw-{n}"))]).collect();
        Ok(roll_previews::ports::Resolved { values, problems: Vec::new(), leases: vec![format!("lease-{n}")], followed, plain: Vec::new(), placeholders: Vec::new() })
    }
    async fn links(&self, _: &str, _: &str) -> anyhow::Result<Vec<(String, Kind, String, String)>> {
        Ok(self.links.lock().clone())
    }
    async fn renew(&self, _: &str, lease: &str) -> anyhow::Result<Option<i64>> {
        if self.ended.lock().contains(lease) {
            return Ok(None);
        }
        self.renewed.lock().push(lease.to_string());
        Ok(Some(i64::MAX / 2))
    }
    async fn revoke(&self, _: &str, lease: &str) -> anyhow::Result<()> {
        self.revoked.lock().push(lease.to_string());
        Ok(())
    }
    async fn delete(&self, _: &str, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

pub struct Env {
    pub secrets: Arc<FakeSecrets>,
    pub _dir: tempfile::TempDir,
    pub ws: Arc<Workspace>,
    pub engine: Arc<Engine>,
    pub code: Arc<FakeCode>,
    pub pg: Arc<FakeData>,
    pub redis: Arc<FakeData>,
    /// A search cluster's fake, placed in the project only by [`env_with_search`].
    pub search: Arc<FakeData>,
    pub apps: Arc<FakeApps>,
    pub project: String,
}

pub fn change(id: &str, n: i64) -> ChangeInfo {
    ChangeInfo {
        id: id.into(),
        repo: "acme/web".into(),
        number: n,
        title: format!("change {n}"),
        branch: Some(format!("feat/{n}")),
        head: format!("cs_{n}a"),
        open: true,
        draft: false,
        labels: Vec::new(),
        trusted: true,
        approved: false,
        author: "carol".into(),
        github_pr: Some(100 + n),
        owner_id: "own_acme".into(),
    }
}

/// A sealer for tests: hex, bound to its associated data (no value shows in the file).
pub struct HexSealer;

impl roll_previews::ports::Sealer for HexSealer {
    fn seal(&self, aad: &str, plain: &str) -> String {
        format!("v1:{}|{}", hex::encode(aad), hex::encode(plain))
    }
    fn open(&self, aad: &str, sealed: &str) -> Option<String> {
        let rest = sealed.strip_prefix(&format!("v1:{}|", hex::encode(aad)))?;
        String::from_utf8(hex::decode(rest).ok()?).ok()
    }
}

pub fn env(settings: serde_json::Value) -> Env {
    env_of(settings, false)
}

/// [`env`] with a search cluster (`srch_logs`) in the project too.
pub fn env_with_search(settings: serde_json::Value) -> Env {
    env_of(settings, true)
}

fn env_of(settings: serde_json::Value, with_search: bool) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let ws = Arc::new(Workspace::open(&code_sql::Target::Sqlite(dir.path().join("ws.db")), Default::default()).unwrap());
    let p = ws.create("", "shop", "", None).unwrap();
    for (kind, r) in [(roll_workspace::Kind::Repo, "acme/web"), (roll_workspace::Kind::Repo, "acme/api"), (roll_workspace::Kind::Postgres, "prj_db"), (roll_workspace::Kind::Redis, "rdb_cache")] {
        ws.place(roll_workspace::Scope::All, &p.id, kind, r, 0.0, 0.0).unwrap();
    }
    if with_search {
        ws.place(roll_workspace::Scope::All, &p.id, roll_workspace::Kind::Search, "srch_logs", 0.0, 0.0).unwrap();
    }
    let store = Arc::new(Store::open(&code_sql::Target::Sqlite(dir.path().join("previews.db")), Default::default()).unwrap());
    // A deployment key, as rolld has one: pasted values are kept sealed.
    store.set_sealer(Arc::new(HexSealer)).unwrap();
    let s = Settings::default().patched(&settings).unwrap();
    store.put_settings(&p.id, &s).unwrap();
    let code = Arc::new(FakeCode::default());
    code.heads.lock().insert("acme/web@main".into(), "cs_web0".into());
    code.heads.lock().insert("acme/api@main".into(), "cs_api0".into());
    let pg = FakeData::new(Kind::Postgres, &["prj_db"]);
    let redis = FakeData::new(Kind::Redis, &["rdb_cache"]);
    let search = FakeData::new(Kind::Search, &["srch_logs"]);
    let apps = Arc::new(FakeApps::default());
    let secrets = Arc::new(FakeSecrets::default());
    let engine = Engine::new(
        Config { domain: "preview.test".into(), otlp: Some("127.0.0.1:7070".into()), ..Config::default() },
        store,
        ws.clone(),
        code.clone(),
        vec![pg.clone(), redis.clone(), search.clone()],
        apps.clone(),
        secrets.clone(),
        Box::new(|| true),
    );
    Env { secrets, _dir: dir, ws, engine, code, pg, redis, search, apps, project: p.id }
}

