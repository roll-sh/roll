//! What triggers checks and previews, with a fake runner (no Docker): checks on every change
//! head with stale results dropped, post-merge runs that coalesce, `broken-main` items opening
//! once and closing on green, a broken roll.toml, repos without roll.toml, previews created and
//! removed with their change, and checks resumed after a restart.

use async_trait::async_trait;
use code_control::model::{Change, Check, CheckStatus, Preview, PreviewState, RepoRow};
use code_control::{Code, Config, PreviewBackend, Runner};
use code_preview::Service;
use parking_lot::Mutex;
use pg_storage::{Objects, StorageConfig};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const TOML: &str = "[checks.test]\nimage = \"busybox\"\nrun = \"true\"\n";

/// Plans one check, `test`. A run fails when the tree's `status` file says `fail`, sleeps for the
/// milliseconds in `delay`, and records which change it ran for and the tree's `n`.
#[derive(Default)]
struct FakeRunner {
    plans: Mutex<usize>,
    runs: Mutex<Vec<(Option<String>, String)>>,
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap_or_default().trim().to_string()
}

#[async_trait]
impl Runner for FakeRunner {
    async fn plan(&self, _repo: &RepoRow, dir: &Path) -> anyhow::Result<Vec<Check>> {
        *self.plans.lock() += 1;
        if read(dir, "roll.toml").contains("broken-config") {
            anyhow::bail!("roll.toml: unknown key `broken-config`");
        }
        Ok(vec![Check { name: "test".into(), status: CheckStatus::Queued, source: "runner".into(), url: None, summary: None, started_at: None, finished_at: None }])
    }

    async fn run(&self, _repo: &RepoRow, change: Option<&Change>, dir: &Path, check: &Check) -> anyhow::Result<Check> {
        if let Ok(ms) = read(dir, "delay").parse::<u64>() {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        self.runs.lock().push((change.map(|c| c.id.clone()), read(dir, "n")));
        let fail = read(dir, "status") == "fail";
        Ok(Check {
            status: if fail { CheckStatus::Failure } else { CheckStatus::Success },
            summary: Some(if fail { "boom: 1 failed".into() } else { "ok".into() }),
            ..check.clone()
        })
    }
}

#[derive(Default)]
struct FakePreviews {
    created: Mutex<Vec<String>>,
    deleted: Mutex<Vec<String>>,
}

#[async_trait]
impl PreviewBackend for FakePreviews {
    async fn create(&self, _repo: &RepoRow, change: &Change, _dir: &Path) -> anyhow::Result<Preview> {
        self.created.lock().push(change.id.clone());
        Ok(Preview { state: PreviewState::Sleeping, url: Some("http://p".into()), postgres: None, redis: None, error: None, created_at: 1 })
    }
    async fn delete(&self, _repo: &RepoRow, change: &Change) -> anyhow::Result<()> {
        self.deleted.lock().push(change.id.clone());
        Ok(())
    }
}

struct App {
    code: Arc<Code>,
    runner: Arc<FakeRunner>,
    repo: RepoRow,
    _dir: tempfile::TempDir,
}

/// roll code with acme/web (roll.toml landed) and, if `start`, the service following it.
async fn app(start: bool, toml: &str) -> App {
    let dir = tempfile::tempdir().unwrap();
    let objects = Objects::new(&StorageConfig::Local(dir.path().join("objects"))).unwrap();
    let code = Code::open(Config::local(dir.path().to_path_buf()), objects).unwrap();
    let runner = Arc::new(FakeRunner::default());
    code.create_owner(&code_control::Caller::Admin, "acme", code_control::model::OwnerKind::Org, None).unwrap();
    let repo = code.create_repo(&code_control::Caller::Admin, &json!({ "owner": "acme", "name": "web" })).await.unwrap();
    if start {
        Service::new(code.clone(), runner.clone()).start();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let a = App { code, runner, repo, _dir: dir };
    if !toml.is_empty() {
        a.land(json!({ "roll.toml": { "content": toml }, "status": { "content": "pass" } })).await;
    }
    a
}

impl App {
    async fn land(&self, files: Value) -> Value {
        self.code.land_api(&self.repo, &code_control::Caller::Admin, &serde_json::from_value(json!({ "message": "m", "files": files })).unwrap(), None, false).await.unwrap()
    }

    async fn change(&self, files: Value) -> Change {
        self.code.create_change(&self.repo, &code_control::Caller::Admin, &json!({ "title": "c", "message": "c", "files": files }), None).await.unwrap()
    }

    fn get(&self, id: &str) -> Change {
        self.code.change_or_404(id).unwrap()
    }

    fn broken(&self) -> Vec<code_control::model::WorkItem> {
        self.code.db.work_items(&self.repo.id, None, Some("broken-main"), 50).unwrap()
    }

    fn main_runs(&self) -> Vec<String> {
        self.runner.runs.lock().iter().filter(|(c, _)| c.is_none()).map(|(_, n)| n.clone()).collect()
    }
}

async fn until<T>(what: &str, secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn change_checks_run_and_stale_heads_are_dropped() {
    let a = app(true, TOML).await;
    let c = a.change(json!({ "x": { "content": "1" } })).await;
    let done = until("the change's check", 10, || a.get(&c.id).checks.into_iter().find(|k| k.status == CheckStatus::Success)).await;
    assert_eq!(done.name, "test");
    assert_eq!(done.source, "runner");
    assert_eq!(done.summary.as_deref(), Some("ok"));
    assert!(done.started_at.is_some() && done.finished_at.is_some());

    // A slow failing revision, then a quick passing one: the slow result arrives for a head the
    // change has left, and is dropped.
    let slow = json!({ "changeset": null, "message": "slow", "files": { "status": { "content": "fail" }, "delay": { "content": "1500" } } });
    let c2 = a.code.revise_change(&a.repo, &code_control::Caller::Admin, &a.get(&c.id), &slow).await.unwrap();
    until("the slow run to start", 10, || (a.get(&c.id).checks.first().map(|k| k.status) == Some(CheckStatus::Running)).then_some(())).await;
    let fast = json!({ "message": "fast", "files": { "status": { "content": "pass" }, "delay": { "content": "0" } } });
    let c3 = a.code.revise_change(&a.repo, &code_control::Caller::Admin, &c2, &fast).await.unwrap();
    assert_eq!(c3.revision, 3);
    until("the fast run", 10, || (a.get(&c.id).checks.first().map(|k| k.status) == Some(CheckStatus::Success)).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let checks = a.get(&c.id).checks;
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].status, CheckStatus::Success, "the stale failure didn't overwrite the current head's result");
    assert_eq!(a.runner.runs.lock().iter().filter(|(id, _)| id.as_deref() == Some(c.id.as_str())).count(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_merge_opens_one_broken_main_item_and_closes_it_on_green() {
    let a = app(true, TOML).await;
    until("the first post-merge run", 10, || (a.main_runs().len() == 1).then_some(())).await;
    assert!(a.broken().is_empty(), "green main opens nothing");

    a.land(json!({ "status": { "content": "fail" }, "n": { "content": "1" } })).await;
    let item = until("a broken-main item", 10, || a.broken().into_iter().next()).await;
    assert_eq!(item.priority, 0);
    assert_eq!(item.labels, ["broken-main"]);
    assert!(item.title.starts_with("main broken at cs_") && item.title.ends_with("(test)"), "{}", item.title);
    assert!(item.body.contains("boom: 1 failed") && item.body.contains("Landed: m"), "{}", item.body);

    // Bisected to the landing that broke it, with a revert waiting for someone to land it.
    assert!(item.body.contains("Bisected to `cs_"), "{}", item.body);
    let reverts = a.code.db.changes(&a.repo.id, None, 50).unwrap();
    let revert = reverts.iter().find(|c| c.title == "Revert: m").expect("a revert change").clone();
    assert_eq!(revert.state, code_control::model::ChangeState::Open);
    assert_eq!(revert.work_item.as_deref(), Some(item.id.as_str()));
    assert_eq!(a.code.work_or_404(&item.id).unwrap().state, code_control::model::WorkState::Open, "still claimable");

    // Still failing: no second item.
    a.land(json!({ "n": { "content": "2" } })).await;
    until("the second failing run", 10, || (a.main_runs().len() >= 3).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(a.broken().len(), 1);

    // Green again: the item closes itself.
    a.land(json!({ "status": { "content": "pass" }, "n": { "content": "3" } })).await;
    until("the item to close", 10, || (a.broken()[0].state == code_control::model::WorkState::Done).then_some(())).await;
    assert!(a.broken()[0].body.contains("Checks pass again"));
    assert_eq!(a.get(&revert.id).state, code_control::model::ChangeState::Closed, "the unneeded revert is closed");

    // Broken again: a new item. An agent claims it; green doesn't take it away from them.
    a.land(json!({ "status": { "content": "fail" }, "n": { "content": "4" } })).await;
    until("a new item", 10, || (a.broken().len() == 2).then_some(())).await;
    let claimed = a.code.claim(&a.repo, &json!({ "agent": "fixer", "labels": ["broken-main"] })).unwrap().unwrap();
    a.land(json!({ "status": { "content": "pass" }, "n": { "content": "5" } })).await;
    until("the green run", 10, || a.main_runs().contains(&"5".to_string()).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let w = a.code.work_or_404(&claimed.id).unwrap();
    assert_eq!(w.state, code_control::model::WorkState::Claimed);
    assert_eq!(w.claimed_by.as_deref(), Some("fixer"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_merge_runs_coalesce_onto_the_latest_head() {
    let a = app(true, "").await;
    a.land(json!({ "roll.toml": { "content": TOML }, "delay": { "content": "800" }, "n": { "content": "0" } })).await;
    until("the first run to start", 10, || (*a.runner.plans.lock() == 1).then_some(())).await;
    for i in 1..=5 {
        a.land(json!({ "n": { "content": i.to_string() } })).await;
    }
    until("the latest head's run", 20, || (a.main_runs().last().map(String::as_str) == Some("5")).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let runs = a.main_runs();
    assert!(runs.len() <= 3, "six landings, at most a run in flight plus one more on the latest head: {runs:?}");
    assert_eq!(runs.first().map(String::as_str), Some("0"));
    assert_eq!(runs.last().map(String::as_str), Some("5"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_broken_roll_toml_breaks_main_and_no_roll_toml_runs_nothing() {
    let a = app(true, "").await;
    let c = a.change(json!({ "x": { "content": "1" } })).await;
    a.land(json!({ "y": { "content": "1" } })).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(*a.runner.plans.lock(), 0, "without roll.toml nothing is materialized or planned");
    assert!(a.get(&c.id).checks.is_empty());

    a.land(json!({ "roll.toml": { "content": "broken-config = 1\n" } })).await;
    let item = until("a broken-main item", 10, || a.broken().into_iter().next()).await;
    assert!(item.title.contains("(roll.toml)"), "{}", item.title);
    assert!(item.body.contains("unknown key"), "{}", item.body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn previews_follow_their_change() {
    let toml = format!("{TOML}\n[preview]\nimage = \"busybox\"\nrun = \"httpd -f -p $PORT\"\nport = 8080\n");
    let a = app(false, &toml).await;
    let previews = Arc::new(FakePreviews::default());
    a.code.set_preview_backend(previews.clone());
    Service::new(a.code.clone(), a.runner.clone()).start();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let c = a.change(json!({ "x": { "content": "1" } })).await;
    until("an automatic preview", 10, || (previews.created.lock().as_slice() == [c.id.clone()]).then_some(())).await;
    until("the preview on the change", 10, || a.get(&c.id).preview).await;
    let landed = a.code.land_change(&a.repo, &code_control::Caller::Admin, &a.get(&c.id), false, code_control::model::Lane::Normal).await.unwrap();
    assert_eq!(landed["status"], "landed");
    until("the preview to go", 10, || (previews.deleted.lock().as_slice() == [c.id.clone()]).then_some(())).await;
    until("the change without a preview", 10, || a.get(&c.id).preview.is_none().then_some(())).await;

    // Closing does the same; `auto = false` creates nothing.
    let c2 = a.change(json!({ "x2": { "content": "1" } })).await;
    until("a second preview", 10, || (previews.created.lock().len() == 2).then_some(())).await;
    until("the preview on the change", 10, || a.get(&c2.id).preview).await;
    a.code.close_change(&a.get(&c2.id)).unwrap();
    until("the second preview to go", 10, || (previews.deleted.lock().len() == 2).then_some(())).await;
    a.land(json!({ "roll.toml": { "content": format!("{toml}auto = false\n") } })).await;
    let c3 = a.change(json!({ "x3": { "content": "1" } })).await;
    until("c3's checks", 10, || a.get(&c3.id).checks.iter().any(|k| k.status == CheckStatus::Success).then_some(())).await;
    assert_eq!(previews.created.lock().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupted_checks_resume_after_a_restart() {
    let a = app(false, TOML).await;
    let interrupted = a.change(json!({ "x": { "content": "1" }, "n": { "content": "i" } })).await;
    let finished = a.change(json!({ "y": { "content": "1" }, "n": { "content": "f" } })).await;
    let external = a.change(json!({ "z": { "content": "1" }, "n": { "content": "e" } })).await;
    a.code.report_check(&interrupted, &json!({ "name": "test", "status": "running", "source": "runner" })).unwrap();
    a.code.report_check(&finished, &json!({ "name": "test", "status": "success", "source": "runner" })).unwrap();
    a.code.report_check(&external, &json!({ "name": "ci", "status": "running" })).unwrap();

    Service::new(a.code.clone(), a.runner.clone()).recover();
    until("the interrupted change's check", 10, || {
        a.get(&interrupted.id).checks.iter().any(|k| k.name == "test" && k.status == CheckStatus::Success).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let ran: Vec<String> = a.runner.runs.lock().iter().map(|(_, n)| n.clone()).collect();
    assert_eq!(ran, ["i"], "finished checks and external checks aren't re-run");
    assert_eq!(a.get(&external.id).checks[0].status, CheckStatus::Running, "an external check is the reporter's to finish");
}

/// A pull request from a fork is code from someone without write access: roll doesn't run its
/// checks (or start its preview) on its own. The trusted change next to it still runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fork_changes_run_nothing_on_their_own() {
    let a = app(true, TOML).await;
    let input = json!({ "title": "from a fork", "message": "fork", "files": { "x": { "content": "1" }, "n": { "content": "fork" } } });
    let fork = a.code.create_change_as(&a.repo, &code_control::Caller::Admin, &input, None, false).await.unwrap();
    assert!(!fork.trusted);
    let ours = a.change(json!({ "x": { "content": "2" }, "n": { "content": "ours" } })).await;
    assert!(ours.trusted);
    until("the trusted change's check", 10, || a.get(&ours.id).checks.into_iter().find(|k| k.status == CheckStatus::Success)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(a.get(&fork.id).checks.is_empty(), "{:?}", a.get(&fork.id).checks);
    assert!(a.runner.runs.lock().iter().all(|(_, n)| n != "fork"), "the fork's tree ran");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_revert_undoes_the_change_that_broke_main() {
    let toml = format!("{TOML}[main]\nauto_revert = true\n");
    let a = app(true, &toml).await;
    until("the first post-merge run", 10, || (a.main_runs().len() == 1).then_some(())).await;
    a.land(json!({ "other": { "content": "1" } })).await;
    until("the second green run", 10, || (a.main_runs().len() >= 2).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    a.land(json!({ "status": { "content": "fail" }, "n": { "content": "breaks" } })).await;
    let item = until("a broken-main item", 10, || a.broken().into_iter().next()).await;
    let revert = until("the revert to land", 10, || {
        a.code.db.changes(&a.repo.id, None, 50).unwrap().into_iter().find(|c| c.title == "Revert: m" && c.state == code_control::model::ChangeState::Landed)
    })
    .await;
    assert_eq!(revert.author, "roll");
    let head = a.code.head(&a.repo).unwrap().unwrap();
    let status = a.code.store.read_file(&a.repo.id, &head, "status").await.unwrap().unwrap();
    assert_eq!(&status.data[..], b"pass", "main is back to passing");
    assert!(a.code.store.read_file(&a.repo.id, &head, "other").await.unwrap().is_some(), "only the culprit was reverted");
    until("the item to finish", 10, || (a.code.work_or_404(&item.id).unwrap().state == code_control::model::WorkState::Done).then_some(())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approved_fork_changes_and_queued_changes_run_their_checks() {
    let a = app(true, TOML).await;
    let input = json!({ "title": "from a fork", "message": "fork", "files": { "x": { "content": "1" }, "n": { "content": "fork" } } });
    let fork = a.code.create_change_as(&a.repo, &code_control::Caller::Admin, &input, None, false).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(a.get(&fork.id).checks.is_empty());
    a.code.approve_run(&a.repo, &code_control::Caller::Admin, &fork).unwrap();
    until("the approved fork's check", 10, || a.get(&fork.id).checks.into_iter().find(|k| k.status == CheckStatus::Success)).await;
    assert!(a.runner.runs.lock().iter().any(|(_, n)| n == "fork"));

    // A queued change's checks run as well (and their results stay with it).
    let c = a.change(json!({ "x": { "content": "q" } })).await;
    a.code.enqueue(&a.repo, &c, code_control::model::Lane::Normal, &code_control::Caller::Admin).await.unwrap();
    until("the queued change's check", 10, || a.get(&c.id).checks.into_iter().find(|k| k.status == CheckStatus::Success)).await;
}
