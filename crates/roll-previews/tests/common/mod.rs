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
    /// The paths between two heads, by `"<from>..<to>"` (unlisted: not known).
    pub between: Mutex<HashMap<String, Vec<String>>>,
    /// The feedback left on changes' preview pages, by change.
    pub feedback: Mutex<HashMap<String, Vec<roll_previews::notes::Note>>>,
    /// The `health` path every service says it has.
    pub health: Mutex<Option<String>>,
    pub reports: Mutex<Vec<Preview>>,
    pub forgotten: Mutex<Vec<String>>,
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
    async fn change(&self, id: &str) -> anyhow::Result<Option<ChangeInfo>> {
        Ok(self.changes.lock().get(id).cloned())
    }
    async fn head(&self, repo: &str, branch: Option<&str>) -> anyhow::Result<Option<String>> {
        Ok(self.heads.lock().get(&format!("{repo}@{}", branch.unwrap_or("main"))).cloned())
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
        Ok(vec![ServiceSpec { name, image: "node:22".into(), run: "node app.js".into(), port: 3000, memory_mb: 512, cpus: 0.5, env: vec![], health: self.health.lock().clone() }])
    }
    async fn report(&self, p: &Preview, _s: &Settings) {
        self.reports.lock().push(p.clone());
    }
    async fn forget(&self, p: &Preview) {
        self.forgotten.lock().push(p.id.clone());
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
    /// Masks applied: (branch, columns).
    pub masks: Mutex<Vec<(String, std::collections::BTreeMap<String, String>)>>,
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
    async fn mask(&self, _parent: &str, branch: &str, columns: &std::collections::BTreeMap<String, String>) -> anyhow::Result<()> {
        self.masks.lock().push((branch.to_string(), columns.clone()));
        Ok(())
    }
    async fn schema(&self, _parent: &str, _branch: &str) -> anyhow::Result<Option<(Vec<String>, Vec<String>)>> {
        Ok(self.schema.lock().clone())
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
    /// roll apps' catalog for the project (unset: none, so roll.toml's services), and the memory
    /// its preview environments run with.
    pub catalog: Mutex<Option<Vec<roll_previews::ports::CatalogService>>>,
    pub memory: Mutex<Option<u32>>,
    /// Services woken (warmed after a push, or asked to wake).
    pub woken: Mutex<Vec<(String, String)>>,
    /// What a service answers on its health path (unset: the backend can't tell).
    pub answers: Mutex<Option<(u16, String)>>,
    /// The variables each service was last deployed with.
    pub vars: Mutex<HashMap<(String, String), Vars>>,
    pub removed: Mutex<Vec<(String, String)>>,
    /// Seeds run (their DATABASE_URL), and the exit code the next ones give.
    pub seeds: Mutex<Vec<String>>,
    pub seed_exit: std::sync::atomic::AtomicI32,
    /// Every seed or release run: (its command, the revision), and how many services were
    /// deployed at that revision when it ran.
    pub runs: Mutex<Vec<(String, String, usize)>>,
    /// The next deploys that answer "still deploying", then the next that fail.
    pub still: AtomicUsize,
    pub fail_deploy: AtomicUsize,
    /// Runs inside a deploy (an API call landing while the reconciler works).
    pub on_deploy: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
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
        self.vars.lock().insert((t.preview.clone(), t.service.clone()), t.vars.clone());
        Ok(None)
    }
    async fn catalog(&self, _project: &str) -> anyhow::Result<Option<Vec<roll_previews::ports::CatalogService>>> {
        Ok(self.catalog.lock().clone())
    }
    async fn memory_mb(&self, _: &str, _: &str) -> Option<u32> {
        *self.memory.lock()
    }
    async fn probe(&self, _: &str, _: &str, _path: &str) -> anyhow::Result<Option<(u16, String)>> {
        Ok(self.answers.lock().clone())
    }
    async fn wake(&self, preview: &str, service: &str) -> anyhow::Result<()> {
        self.woken.lock().push((preview.to_string(), service.to_string()));
        Ok(())
    }
    async fn sleep(&self, _: &str, _: &str) {}
    async fn remove(&self, p: &str, s: &str) -> anyhow::Result<()> {
        self.deployed.lock().remove(&(p.to_string(), s.to_string()));
        self.removed.lock().push((p.to_string(), s.to_string()));
        Ok(())
    }
    async fn logs(&self, _: &str, _: &str, _: usize) -> Vec<String> {
        Vec::new()
    }
    async fn seed(&self, job: &roll_previews::ports::SeedJob) -> anyhow::Result<(i32, String)> {
        let db = job.env.iter().find(|(k, _)| k == "DATABASE_URL").map(|(_, v)| v.clone()).unwrap_or_default();
        self.seeds.lock().push(db);
        let at_rev = self.deployed.lock().values().filter(|r| **r == job.revision).count();
        self.runs.lock().push((job.seed.run.clone(), job.revision.clone(), at_rev));
        Ok((self.seed_exit.load(Ordering::SeqCst), "seed output".into()))
    }
    fn status(&self, _: &str, _: &str) -> Option<ServiceState> {
        None
    }
}

/// roll secrets as previews see it: every `ensure` (environment, trusted).
#[derive(Default)]
pub struct FakeSecrets {
    pub ensured: Mutex<Vec<(String, bool)>>,
}

#[async_trait]
impl roll_previews::ports::Secrets for FakeSecrets {
    async fn ensure(&self, _: &str, env: &str, _: &str, _: &[(String, String)], trusted: bool, _: i64) -> anyhow::Result<()> {
        self.ensured.lock().push((env.to_string(), trusted));
        Ok(())
    }
    async fn resolve(&self, _: &str, _: &str, _: bool) -> anyhow::Result<roll_previews::ports::Resolved> {
        Ok(Default::default())
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
        github_pr: Some(100 + n),
    }
}

pub fn env(settings: serde_json::Value) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let ws = Arc::new(Workspace::open(&code_sql::Target::Sqlite(dir.path().join("ws.db")), Default::default()).unwrap());
    let p = ws.create("", "shop", "", None).unwrap();
    for (kind, r) in [(roll_workspace::Kind::Repo, "acme/web"), (roll_workspace::Kind::Repo, "acme/api"), (roll_workspace::Kind::Postgres, "prj_db"), (roll_workspace::Kind::Redis, "rdb_cache")] {
        ws.place(roll_workspace::Scope::All, &p.id, kind, r, 0.0, 0.0).unwrap();
    }
    let store = Arc::new(Store::open(&code_sql::Target::Sqlite(dir.path().join("previews.db")), Default::default()).unwrap());
    let s = Settings::default().patched(&settings).unwrap();
    store.put_settings(&p.id, &s).unwrap();
    let code = Arc::new(FakeCode::default());
    code.heads.lock().insert("acme/web@main".into(), "cs_web0".into());
    code.heads.lock().insert("acme/api@main".into(), "cs_api0".into());
    let pg = FakeData::new(Kind::Postgres, &["prj_db"]);
    let redis = FakeData::new(Kind::Redis, &["rdb_cache"]);
    let apps = Arc::new(FakeApps::default());
    let secrets = Arc::new(FakeSecrets::default());
    let engine = Engine::new(
        Config { domain: "preview.test".into(), ..Config::default() },
        store,
        ws.clone(),
        code.clone(),
        vec![pg.clone(), redis.clone()],
        apps.clone(),
        secrets.clone(),
        Box::new(|| true),
    );
    Env { secrets, _dir: dir, ws, engine, code, pg, redis, apps, project: p.id }
}

