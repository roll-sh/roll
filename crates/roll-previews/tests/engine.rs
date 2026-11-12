//! The reconciler against fake products: what a person relies on from a preview's lifecycle.

use async_trait::async_trait;
use parking_lot::Mutex;
use roll_previews::engine::Config;
use roll_previews::ports::{Apps, Brancher, ChangeInfo, NoSecrets, ServiceSpec, Source, Target, Usage};
use roll_previews::{Desired, Engine, Kind, Mode, Preview, ResourceState, ServiceState, Settings, State, Store};
use roll_workspace::Workspace;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct FakeCode {
    changes: Mutex<HashMap<String, ChangeInfo>>,
    heads: Mutex<HashMap<String, String>>,
    reports: Mutex<Vec<Preview>>,
    forgotten: Mutex<Vec<String>>,
}

impl FakeCode {
    fn open(&self, c: ChangeInfo) {
        self.changes.lock().insert(c.id.clone(), c);
    }

    fn edit(&self, id: &str, f: impl FnOnce(&mut ChangeInfo)) {
        f(self.changes.lock().get_mut(id).unwrap());
    }

    fn last_report(&self, change: &str) -> Preview {
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
    async fn services(&self, repo: &str, _revision: &str) -> anyhow::Result<Vec<ServiceSpec>> {
        let name = repo.rsplit('/').next().unwrap().to_string();
        Ok(vec![ServiceSpec { name, image: "node:22".into(), run: "node app.js".into(), port: 3000, memory_mb: 512, cpus: 0.5, env: vec![] }])
    }
    async fn report(&self, p: &Preview, _s: &Settings) {
        self.reports.lock().push(p.clone());
    }
    async fn forget(&self, p: &Preview) {
        self.forgotten.lock().push(p.id.clone());
    }
    fn nudges(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        None
    }
}

/// A product whose branches are `<parent>~<name>`, refusing to touch a parent.
struct FakeData {
    kind: Kind,
    branches: Mutex<HashSet<String>>,
    parents: HashSet<String>,
    made: AtomicUsize,
    fail_next: AtomicUsize,
}

impl FakeData {
    fn new(kind: Kind, parents: &[&str]) -> Arc<FakeData> {
        Arc::new(FakeData {
            kind,
            branches: Mutex::new(HashSet::new()),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            made: AtomicUsize::new(0),
            fail_next: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl Brancher for FakeData {
    fn kind(&self) -> Kind {
        self.kind
    }
    async fn branch(&self, parent: &str, name: &str, _mode: Mode) -> anyhow::Result<String> {
        if self.fail_next.load(Ordering::SeqCst) > 0 {
            self.fail_next.fetch_sub(1, Ordering::SeqCst);
            anyhow::bail!("the product is busy");
        }
        anyhow::ensure!(self.parents.contains(parent), "no parent {parent}");
        let id = format!("{parent}~{name}");
        if self.branches.lock().insert(id.clone()) {
            self.made.fetch_add(1, Ordering::SeqCst);
        }
        Ok(id)
    }
    async fn reset(&self, parent: &str, branch: &str, name: &str, mode: Mode) -> anyhow::Result<String> {
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }
    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        anyhow::ensure!(branch == format!("{parent}~{name}"), "refusing to delete {branch}: not {parent}'s branch {name}");
        self.branches.lock().remove(branch);
        Ok(())
    }
    async fn connect(&self, _parent: &str, branch: &str) -> anyhow::Result<Vec<(String, String)>> {
        let var = match self.kind {
            Kind::Postgres => "DATABASE_URL",
            _ => "REDIS_URL",
        };
        Ok(vec![(var.into(), format!("fake://{branch}"))])
    }
    async fn usage(&self, _branch: &str) -> anyhow::Result<Usage> {
        Ok(Usage { bytes: 1 << 20, storage_usd_month: 0.001, active_usd_hour: 0.04, month_to_date_usd: 0.0 })
    }
}

#[derive(Default)]
struct FakeApps {
    deployed: Mutex<HashMap<(String, String), String>>,
    removed: Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl Apps for FakeApps {
    async fn deploy(&self, t: &Target) -> anyhow::Result<()> {
        self.deployed.lock().insert((t.preview.clone(), t.service.clone()), t.revision.clone());
        Ok(())
    }
    async fn wake(&self, _: &str, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn sleep(&self, _: &str, _: &str) {}
    async fn remove(&self, p: &str, s: &str) {
        self.deployed.lock().remove(&(p.to_string(), s.to_string()));
        self.removed.lock().push((p.to_string(), s.to_string()));
    }
    async fn logs(&self, _: &str, _: &str, _: usize) -> Vec<String> {
        Vec::new()
    }
    fn status(&self, _: &str, _: &str) -> Option<ServiceState> {
        None
    }
}

struct Env {
    _dir: tempfile::TempDir,
    engine: Arc<Engine>,
    code: Arc<FakeCode>,
    pg: Arc<FakeData>,
    redis: Arc<FakeData>,
    apps: Arc<FakeApps>,
    project: String,
}

fn change(id: &str, n: i64) -> ChangeInfo {
    ChangeInfo {
        id: id.into(),
        repo: "acme/web".into(),
        number: n,
        title: format!("change {n}"),
        branch: Some(format!("feat/{n}")),
        head: format!("cs_{n}a"),
        open: true,
        draft: false,
        trusted: true,
        approved: false,
        github_pr: Some(100 + n),
    }
}

fn env(settings: serde_json::Value) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let ws = Arc::new(Workspace::open(&code_sql::Target::Sqlite(dir.path().join("ws.db")), Default::default()).unwrap());
    let p = ws.create("shop", "", None).unwrap();
    for (kind, r) in [(roll_workspace::Kind::Repo, "acme/web"), (roll_workspace::Kind::Repo, "acme/api"), (roll_workspace::Kind::Postgres, "prj_db"), (roll_workspace::Kind::Redis, "rdb_cache")] {
        ws.place(&p.id, kind, r, 0.0, 0.0).unwrap();
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
    let engine = Engine::new(
        Config { domain: "preview.test".into(), ..Config::default() },
        store,
        ws,
        code.clone(),
        vec![pg.clone(), redis.clone()],
        apps.clone(),
        Arc::new(NoSecrets),
        Box::new(|| true),
    );
    Env { _dir: dir, engine, code, pg, redis, apps, project: p.id }
}

#[tokio::test]
async fn a_change_gets_a_full_preview_that_follows_pushes_and_goes_when_it_lands() {
    let e = env(json!({ "enabled": true }));
    e.code.open(change("chg_1", 7));
    e.engine.pass().await.unwrap();

    let p = e.code.last_report("chg_1");
    assert_eq!(p.state, State::Ready, "{p:?}");
    assert_eq!(p.slug, "pr107-shop");
    let refs: Vec<_> = p.resources.iter().map(|r| (r.kind, r.r#ref.clone().unwrap(), r.state)).collect();
    assert!(refs.contains(&(Kind::Postgres, format!("prj_db~pv-{}", &p.id[3..]), ResourceState::Ready)), "{refs:?}");
    assert_eq!(refs.len(), 2, "a branch of every data resource");
    // Every service of the project: the change's repo at its head, the other at its default head.
    let urls: Vec<_> = p.services.iter().map(|s| (s.name.as_str(), s.url.as_str(), s.revision.as_deref().unwrap())).collect();
    assert!(urls.contains(&("web", "http://web-pr107-shop.preview.test", "cs_7a")), "{urls:?}");
    assert!(urls.contains(&("api", "http://api-pr107-shop.preview.test", "cs_api0")), "{urls:?}");
    assert!(p.cost.sleeping_usd_month > 0.0 && p.cost.active_usd_hour > 0.0, "{:?}", p.cost);

    // Converging again changes nothing: branching is idempotent.
    e.engine.pass().await.unwrap();
    assert_eq!(e.pg.made.load(Ordering::SeqCst), 1);

    // A push redeploys the change's service at the new head; the data stays.
    e.code.edit("chg_1", |c| c.head = "cs_7b".into());
    e.engine.pass().await.unwrap();
    assert_eq!(e.apps.deployed.lock()[&(p.id.clone(), "web".to_string())], "cs_7b");
    assert_eq!(e.pg.made.load(Ordering::SeqCst), 1, "a push keeps the preview's data");

    // A reset re-branches from the parent, keeping the preview.
    e.engine.reset(&p.id, vec!["postgres:prj_db".into()]).unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(e.pg.made.load(Ordering::SeqCst), 2);
    assert!(e.engine.store.preview(&p.id).unwrap().last_reset_at.is_some());

    // The change lands: services, branches and the preview go; the parents stay.
    e.code.edit("chg_1", |c| c.open = false);
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    assert!(e.engine.store.row(&p.id).is_err(), "the preview is gone");
    assert!(e.pg.branches.lock().is_empty() && e.redis.branches.lock().is_empty());
    assert!(e.apps.deployed.lock().is_empty());
    assert_eq!(*e.code.forgotten.lock(), vec![p.id.clone()]);
}

#[tokio::test]
async fn a_fork_waits_for_approval_and_a_failed_step_is_retried() {
    let e = env(json!({ "enabled": true, "resources": { "redis:rdb_cache": "skip" } }));
    e.code.open(ChangeInfo { trusted: false, ..change("chg_f", 3) });
    e.engine.pass().await.unwrap();
    let p = e.code.last_report("chg_f");
    assert_eq!(p.state, State::AwaitingApproval);
    assert_eq!(e.pg.made.load(Ordering::SeqCst), 0, "nothing of a fork runs or branches before approval");
    assert!(e.apps.deployed.lock().is_empty());

    // A writer approves the head; the product fails once, the retry converges.
    e.code.edit("chg_f", |c| c.approved = true);
    e.pg.fail_next.store(1, Ordering::SeqCst);
    e.engine.pass().await.unwrap();
    let failed = e.engine.store.row(&p.id).unwrap();
    assert_eq!(failed.state, State::Failed);
    assert!(failed.error.as_deref().unwrap().contains("busy"));
    e.engine.store.update(&p.id, |r| r.next_at = 0).unwrap(); // the backoff has passed
    e.engine.pass().await.unwrap();
    let p = e.engine.store.preview(&p.id).unwrap();
    assert_eq!(p.state, State::Ready, "{p:?}");
    assert_eq!(p.resources.len(), 1, "redis is skipped by the project's rules");

    // A new push un-approves it: its services stop and it waits again.
    e.code.edit("chg_f", |c| {
        c.head = "cs_3b".into();
        c.approved = false;
    });
    e.engine.pass().await.unwrap();
    assert_eq!(e.engine.store.row(&p.id).unwrap().state, State::AwaitingApproval);
}

#[tokio::test]
async fn previews_beyond_the_limit_wait_and_start_as_others_go() {
    let e = env(json!({ "enabled": true, "max_previews": 1 }));
    e.code.open(change("chg_a", 1));
    e.engine.pass().await.unwrap();
    e.code.open(change("chg_b", 2));
    e.engine.pass().await.unwrap();
    let b = e.engine.store.row_by_change("chg_b").unwrap().unwrap();
    assert_eq!(b.state, State::Queued);
    let a = e.engine.store.row_by_change("chg_a").unwrap().unwrap();
    e.engine.delete(&a.id).unwrap();
    for _ in 0..3 {
        e.engine.pass().await.unwrap();
    }
    let b = e.engine.store.row(&b.id).unwrap();
    assert_eq!((b.state, b.desired), (State::Ready, Desired::Active));
    // A manual preview of a branch is refused at the limit rather than queued.
    e.code.heads.lock().insert("acme/web@feat/x".into(), "cs_x".into());
    let err = e.engine.create(&e.project, "acme/web", Some("feat/x"), None, "me").await.unwrap_err();
    assert!(err.to_string().contains("max_previews"), "{err}");
}
