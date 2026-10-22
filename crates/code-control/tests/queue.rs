//! The merge queue, with a fake runner (no Docker): batches land together in FIFO order, lanes,
//! bisecting red batches to one or two culprits, conflicts ejected, the default branch moving
//! under a batch, pause and resume, dequeueing mid-test, restart recovery (in-flight and
//! landing), git pushes queued, admin bypass audited, blockers, the API shapes, reverts, and
//! 2,000 changes through the queue with random culprits.

mod common;

use async_trait::async_trait;
use axum::http::StatusCode;
use code_control::model::{Change, Check, CheckStatus, RepoRow};
use code_control::{Caller, Runner};
use common::{App, app, git, git_raw, open_in, write};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

const P: &str = "/v1/code/repos/acme/api";

fn toml(extra: &str) -> String {
    format!("[checks.ci]\nimage = \"busybox\"\nrun = \"true\"\n{extra}")
}

/// Plans every `[checks.*]` in roll.toml. A check fails when the tree has a `bad/` directory,
/// or a `bad-<check>` file (just that check). With a gate, each run waits for a permit.
#[derive(Default)]
struct QueueRunner {
    plans: AtomicUsize,
    runs: AtomicUsize,
    gate: Option<Arc<Semaphore>>,
    /// Plans that fail outright (the runner broke) before planning works again.
    fail_plans: AtomicUsize,
}

impl QueueRunner {
    fn gated() -> (Arc<QueueRunner>, Arc<Semaphore>) {
        let gate = Arc::new(Semaphore::new(0));
        (Arc::new(QueueRunner { gate: Some(gate.clone()), ..QueueRunner::default() }), gate)
    }
}

#[async_trait]
impl Runner for QueueRunner {
    async fn plan(&self, _repo: &RepoRow, dir: &Path) -> anyhow::Result<Vec<Check>> {
        self.plans.fetch_add(1, Ordering::SeqCst);
        if self.fail_plans.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok() {
            anyhow::bail!("the runner is down");
        }
        let text = std::fs::read_to_string(dir.join("roll.toml")).unwrap_or_default();
        Ok(text
            .lines()
            .filter_map(|l| l.trim().strip_prefix("[checks.").and_then(|r| r.strip_suffix(']')))
            .map(|name| Check {
                name: name.into(),
                status: CheckStatus::Queued,
                source: "runner".into(),
                url: None,
                summary: None,
                started_at: None,
                finished_at: None,
            })
            .collect())
    }

    async fn run(&self, _repo: &RepoRow, _change: Option<&Change>, dir: &Path, check: &Check) -> anyhow::Result<Check> {
        if let Some(g) = &self.gate {
            g.acquire().await?.forget();
        }
        self.runs.fetch_add(1, Ordering::SeqCst);
        let bad = dir.join("bad").is_dir() || dir.join(format!("bad-{}", check.name)).exists();
        Ok(Check {
            status: if bad { CheckStatus::Failure } else { CheckStatus::Success },
            summary: Some(if bad { format!("{}: 1 test failed", check.name) } else { "ok".into() }),
            ..check.clone()
        })
    }

    fn validate(&self, text: &str) -> Option<String> {
        text.contains("timeout = \"").then(|| "roll.toml [checks.ci]: timeout must be a number of seconds".into())
    }
}

/// acme/api with roll.toml on main, the queue policy and `runner`. Queues aren't started.
async fn queue_app(extra_toml: &str, runner: Arc<QueueRunner>) -> App {
    let a = app(None);
    a.repo("acme", "api").await;
    a.ok("POST", &format!("{P}/land"), Some(json!({ "message": "init", "files": { "roll.toml": { "content": toml(extra_toml) }, "README.md": { "content": "# api\n" } } }))).await;
    let r = a.ok("PATCH", P, Some(json!({ "land_policy": "queue" }))).await;
    assert_eq!(r["land_policy"], "queue");
    a.code.set_runner(runner);
    a
}

async fn change(a: &App, title: &str, files: Value) -> Value {
    a.ok("POST", &format!("{P}/changes"), Some(json!({ "title": title, "message": title, "files": files }))).await
}

async fn enqueue(a: &App, c: &Value, lane: &str) -> Value {
    let id = c["id"].as_str().unwrap();
    let (s, v) = a.req("POST", &format!("/v1/code/changes/{id}/enqueue"), Some(json!({ "lane": lane })), None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    v
}

fn id(c: &Value) -> String {
    c["id"].as_str().unwrap().to_string()
}

/// Subjects of main's first-parent history, oldest first.
async fn main_log(a: &App) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let q = cursor.as_ref().map(|c| format!("&cursor={c}")).unwrap_or_default();
        let v = a.ok("GET", &format!("{P}/log?rev=main&limit=500{q}"), None).await;
        for item in v["items"].as_array().unwrap() {
            out.push(item["message"].as_str().unwrap().lines().next().unwrap().to_string());
        }
        match v["cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    out.reverse();
    out
}

async fn queue(a: &App) -> Value {
    a.ok("GET", &format!("{P}/queue"), None).await
}

/// Poll the queue until `f` holds (30 s at most).
async fn wait_queue(a: &App, what: &str, f: impl Fn(&Value) -> bool) -> Value {
    let mut last = Value::Null;
    for _ in 0..3000 {
        last = queue(a).await;
        if f(&last) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("queue never got {what}: {last}");
}

async fn drained(a: &App) -> Value {
    wait_queue(a, "empty", |q| q["stats"]["depth"] == 0 && q["batch"].is_null()).await
}

async fn file(a: &App, path: &str) -> Option<String> {
    let (s, b) = a.raw("GET", &format!("{P}/blob/main/{path}"), None, None).await;
    (s == StatusCode::OK).then(|| String::from_utf8(b).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batches_land_together_in_fifo_order() {
    let runner = Arc::new(QueueRunner::default());
    let a = queue_app("", runner.clone()).await;
    let mut ids = Vec::new();
    for i in 0..5 {
        let c = change(&a, &format!("change {i}"), json!({ format!("f/{i}"): { "content": format!("{i}\n") } })).await;
        let v = enqueue(&a, &c, "normal").await;
        assert_eq!(v["position"], i + 1);
        assert_eq!(v["change"]["state"], "queued");
        assert!(v["queue_id"].as_str().unwrap().starts_with("qe_"));
        ids.push(id(&c));
    }
    let q = queue(&a).await;
    assert_eq!(q["state"], "idle");
    assert_eq!(q["stats"]["depth"], 5);
    assert_eq!(a.ok("GET", P, None).await["stats"]["queue_depth"], 5, "the repo's queue depth counts the merge queue");
    assert_eq!(a.ok("GET", "/v1/code", None).await["stats"]["queue_depth"], 5);
    assert_eq!(q["entries"].as_array().unwrap().len(), 5);
    assert_eq!(q["entries"][0]["position"], 1);
    assert_eq!(q["entries"][0]["state"], "waiting");

    a.code.start_queues().unwrap();
    drained(&a).await;
    // One batch: one test.
    assert_eq!(runner.plans.load(Ordering::SeqCst), 1);
    let log = main_log(&a).await;
    assert_eq!(&log[1..], ["change 0", "change 1", "change 2", "change 3", "change 4"]);
    for (i, id) in ids.iter().enumerate() {
        let c = a.wait_change(id, "landed").await;
        assert_eq!(c["queue"]["state"], "landed");
        assert_eq!(file(&a, &format!("f/{i}")).await.as_deref(), Some(format!("{i}\n").as_str()));
    }
    let q = queue(&a).await;
    assert_eq!(q["stats"]["landed_1h"], 5);
    assert_eq!(q["recent"].as_array().unwrap().len(), 5);
    assert!(q["last_green"].as_str().unwrap().starts_with("cs_"));
    // Scratch refs never show up.
    let refs = a.ok("GET", &format!("{P}/refs"), None).await;
    assert!(refs.as_array().unwrap().iter().all(|r| !r["name"].as_str().unwrap().contains("roll")), "{refs}");
    // Every landing counted as a merge.
    let repo = a.ok("GET", P, None).await;
    assert_eq!(repo["stats"]["merges_today"], 6);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn urgent_lane_goes_first() {
    let a = queue_app("[queue]\nbatch_size = 1\n", Arc::new(QueueRunner::default())).await;
    let x = change(&a, "normal one", json!({ "a": { "content": "a\n" } })).await;
    let y = change(&a, "normal two", json!({ "b": { "content": "b\n" } })).await;
    let z = change(&a, "urgent", json!({ "c": { "content": "c\n" } })).await;
    enqueue(&a, &x, "normal").await;
    enqueue(&a, &y, "normal").await;
    let v = enqueue(&a, &z, "urgent").await;
    assert_eq!(v["position"], 1, "urgent jumps the normal lane");
    let c = a.ok("GET", &format!("/v1/code/changes/{}", id(&x)), None).await;
    assert_eq!(c["queue"]["position"], 2);
    assert_eq!(c["queue"]["lane"], "normal");
    // Enqueueing again is a no-op that reports the same entry.
    let again = enqueue(&a, &x, "urgent").await;
    assert_eq!(again["queue_id"], c["queue"]["id"]);
    // Bad lane.
    let (s, _) = a.req("POST", &format!("/v1/code/changes/{}/enqueue", id(&y)), Some(json!({ "lane": "vip" })), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    a.code.start_queues().unwrap();
    drained(&a).await;
    assert_eq!(&main_log(&a).await[1..], ["urgent", "normal one", "normal two"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_red_batch_is_bisected_to_its_culprit() {
    let runner = Arc::new(QueueRunner::default());
    let a = queue_app("", runner.clone()).await;
    let mut ids = Vec::new();
    for i in 0..8 {
        let mut files = json!({ format!("f/{i}"): { "content": "x\n" } });
        if i == 5 {
            files["bad/5"] = json!({ "content": "breaks ci\n" });
        }
        let c = change(&a, &format!("change {i}"), files).await;
        enqueue(&a, &c, "normal").await;
        ids.push(id(&c));
    }
    a.code.start_queues().unwrap();
    drained(&a).await;
    let culprit = a.ok("GET", &format!("/v1/code/changes/{}", ids[5]), None).await;
    assert_eq!(culprit["state"], "open", "{culprit}");
    assert_eq!(culprit["queue"]["state"], "ejected");
    let err = culprit["queue"]["error"].as_str().unwrap();
    assert_eq!(err, "failed the queue's checks: ci", "the reason, one line");
    let out = culprit["queue"]["output"].as_str().unwrap();
    assert!(out.contains("ci:\nci: 1 test failed"), "the log tail is apart: {out}");
    for (i, id) in ids.iter().enumerate().filter(|(i, _)| *i != 5) {
        let c = a.wait_change(id, "landed").await;
        assert_eq!(c["queue"]["state"], "landed", "change {i}");
    }
    assert_eq!(&main_log(&a).await[1..], ["change 0", "change 1", "change 2", "change 3", "change 4", "change 6", "change 7"]);
    assert_eq!(file(&a, "bad/5").await, None, "the culprit never reached main");
    // The batch halved after the failure, then grew back.
    let q = queue(&a).await;
    assert!(q["stats"]["batch_size"].as_u64().unwrap() <= 8);
    // Bisecting 8 takes log2(8) = 3 more tests (plus one of the head, never seen green before).
    assert!(runner.plans.load(Ordering::SeqCst) <= 1 + 3 + 1 + 2, "plans: {}", runner.plans.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_culprits_are_both_ejected() {
    let a = queue_app("", Arc::new(QueueRunner::default())).await;
    let mut ids = Vec::new();
    for i in 0..8 {
        let mut files = json!({ format!("f/{i}"): { "content": "x\n" } });
        if i == 1 || i == 6 {
            files[format!("bad/{i}")] = json!({ "content": "!\n" });
        }
        let c = change(&a, &format!("change {i}"), files).await;
        enqueue(&a, &c, "normal").await;
        ids.push(id(&c));
    }
    a.code.start_queues().unwrap();
    drained(&a).await;
    for i in [1, 6] {
        let c = a.ok("GET", &format!("/v1/code/changes/{}", ids[i]), None).await;
        assert_eq!(c["queue"]["state"], "ejected", "change {i}: {c}");
    }
    assert_eq!(&main_log(&a).await[1..], ["change 0", "change 2", "change 3", "change 4", "change 5", "change 7"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_check_that_fails_on_main_too_doesnt_eject_anyone() {
    // `lint` is already red on main; the batch only has to not make things worse.
    let runner = Arc::new(QueueRunner::default());
    let a = queue_app("[checks.lint]\nimage = \"busybox\"\nrun = \"true\"\n", runner.clone()).await;
    a.ok("POST", &format!("{P}/land"), Some(json!({ "message": "break lint", "files": { "bad-lint": { "content": "1\n" } }, "bypass": true }))).await;
    let fix = change(&a, "unrelated", json!({ "x": { "content": "x\n" } })).await;
    enqueue(&a, &fix, "normal").await;
    a.code.start_queues().unwrap();
    a.wait_change(&id(&fix), "landed").await;
    // And a change that fixes lint gets through too.
    let fix = change(&a, "fix lint", json!({ "bad-lint": { "delete": true } })).await;
    enqueue(&a, &fix, "normal").await;
    a.wait_change(&id(&fix), "landed").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conflicts_are_ejected_with_their_paths() {
    let a = queue_app("", Arc::new(QueueRunner::default())).await;
    let x = change(&a, "x", json!({ "shared.txt": { "content": "from x\n" } })).await;
    let y = change(&a, "y", json!({ "shared.txt": { "content": "from y\n" } })).await;
    enqueue(&a, &x, "normal").await;
    enqueue(&a, &y, "normal").await;
    a.code.start_queues().unwrap();
    drained(&a).await;
    a.wait_change(&id(&x), "landed").await;
    let y = a.wait_change(&id(&y), "conflict").await;
    assert_eq!(y["conflict"]["paths"], json!(["shared.txt"]));
    assert_eq!(y["queue"]["state"], "ejected");
    assert!(y["queue"]["error"].as_str().unwrap().contains("shared.txt"));
    // Queued again unrevised (conflict → queued), it conflicts again.
    let again = enqueue(&a, &y, "normal").await;
    assert_eq!(again["change"]["state"], "queued");
    assert_ne!(again["queue_id"], y["queue"]["id"], "a new entry");
    wait_queue(&a, "the second ejection", |q| q["recent"][0]["id"] == again["queue_id"] && q["recent"][0]["state"] == "ejected").await;
    let y = a.wait_change(&id(&y), "conflict").await;
    // Revised on top of x, it goes through.
    let head = a.ok("GET", P, None).await["head"].clone();
    let y = a.ok("POST", &format!("/v1/code/changes/{}/revise", id(&y)), Some(json!({ "base": head, "message": "y again", "files": { "shared.txt": { "content": "from x and y\n" } } }))).await;
    assert_eq!(y["state"], "open");
    enqueue(&a, &y, "normal").await;
    a.wait_change(&id(&y), "landed").await;
    assert_eq!(file(&a, "shared.txt").await.as_deref(), Some("from x and y\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_batch_is_rebuilt_when_main_moves_under_it() {
    let (runner, gate) = QueueRunner::gated();
    let a = queue_app("", runner).await;
    let x = change(&a, "queued", json!({ "q": { "content": "q\n" } })).await;
    enqueue(&a, &x, "normal").await;
    a.code.start_queues().unwrap();
    let q = wait_queue(&a, "testing", |q| q["state"] == "testing" && q["batch"]["tip"].is_string() && !q["batch"]["checks"][0].is_null()).await;
    assert_eq!(q["batch"]["changes"], json!([id(&x)]));
    assert_eq!(q["entries"][0]["state"], "testing");
    assert_eq!(q["batch"]["checks"][0]["name"], "ci");
    // An admin lands straight on main meanwhile.
    let v = a.ok("POST", &format!("{P}/land"), Some(json!({ "message": "hotfix", "files": { "hot": { "content": "fix\n" } }, "bypass": true }))).await;
    assert_eq!(v["status"], "landed");
    gate.add_permits(100);
    a.wait_change(&id(&x), "landed").await;
    assert_eq!(&main_log(&a).await[1..], ["hotfix", "queued"]);
    let audit = a.ok("GET", &format!("{P}/audit"), None).await;
    assert_eq!(audit[0]["action"], "queue_bypass");
    assert_eq!(audit[0]["detail"]["ref"], "main");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bypass_needs_the_admin_token() {
    let a = common::app(Some("admin-secret"));
    let admin = Some("admin-secret");
    a.req("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" })), admin).await;
    let (s, v) = a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "api" })), admin).await;
    assert!(s.is_success(), "{v}");
    a.req("PATCH", P, Some(json!({ "land_policy": "queue" })), admin).await;
    let token = a.req("POST", "/v1/code/owners/acme/tokens", Some(json!({ "name": "agent" })), admin).await.1["token"].as_str().unwrap().to_string();
    let body = json!({ "message": "m", "files": { "a": { "content": "a\n" } }, "bypass": true });
    let (s, v) = a.req("POST", &format!("{P}/land"), Some(body.clone()), Some(&token)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    // Without bypass the owner's write is queued.
    let (s, v) = a.req("POST", &format!("{P}/land"), Some(json!({ "message": "m", "files": { "a": { "content": "a\n" } } })), Some(&token)).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["change"]["queue"]["enqueued_by"], "acme");
    let (s, v) = a.req("POST", &format!("{P}/land"), Some(body), admin).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, audit) = a.req("GET", &format!("{P}/audit"), None, admin).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(audit.as_array().unwrap().len(), 1);
    assert_eq!(audit[0]["actor"], "admin");
    // Anonymous can't read the audit log, or pause the queue.
    assert_eq!(a.req("GET", &format!("{P}/audit"), None, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(a.req("POST", &format!("{P}/queue/pause"), None, None).await.0, StatusCode::UNAUTHORIZED);
    // Anyone can look at a public repo's queue.
    assert_eq!(a.req("GET", &format!("{P}/queue"), None, None).await.0, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_and_resume() {
    let a = queue_app("", Arc::new(QueueRunner::default())).await;
    let q = a.ok("POST", &format!("{P}/queue/pause"), None).await;
    assert_eq!(q["paused"], true);
    assert_eq!(q["state"], "paused");
    let x = change(&a, "x", json!({ "x": { "content": "x\n" } })).await;
    enqueue(&a, &x, "normal").await;
    a.code.start_queues().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let c = a.ok("GET", &format!("/v1/code/changes/{}", id(&x)), None).await;
    assert_eq!(c["state"], "queued");
    assert_eq!(c["queue"]["state"], "waiting");
    let q = a.ok("POST", &format!("{P}/queue/resume"), None).await;
    assert_eq!(q["paused"], false);
    a.wait_change(&id(&x), "landed").await;
    let audit = a.ok("GET", &format!("{P}/audit"), None).await;
    let actions: Vec<&str> = audit.as_array().unwrap().iter().map(|e| e["action"].as_str().unwrap()).collect();
    assert_eq!(actions, ["queue_resume", "queue_pause"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dequeueing_a_change_under_test_rebuilds_the_batch() {
    let (runner, gate) = QueueRunner::gated();
    let a = queue_app("", runner).await;
    let x = change(&a, "x", json!({ "x": { "content": "x\n" } })).await;
    let y = change(&a, "y", json!({ "y": { "content": "y\n" } })).await;
    enqueue(&a, &x, "normal").await;
    enqueue(&a, &y, "normal").await;
    a.code.start_queues().unwrap();
    wait_queue(&a, "testing", |q| q["state"] == "testing" && q["batch"]["changes"].as_array().is_some_and(|c| c.len() == 2)).await;
    let v = a.ok("DELETE", &format!("/v1/code/changes/{}/enqueue", id(&x)), None).await;
    assert_eq!(v["state"], "open");
    assert_eq!(v["queue"], Value::Null);
    let (s, _) = a.req("DELETE", &format!("/v1/code/changes/{}/enqueue", id(&x)), None, None).await;
    assert_eq!(s, StatusCode::CONFLICT, "not queued any more");
    gate.add_permits(100);
    a.wait_change(&id(&y), "landed").await;
    let x = a.ok("GET", &format!("/v1/code/changes/{}", id(&x)), None).await;
    assert_eq!(x["state"], "open");
    assert_eq!(&main_log(&a).await[1..], ["y"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revising_or_closing_a_queued_change() {
    let (runner, gate) = QueueRunner::gated();
    let a = queue_app("", runner).await;
    let x = change(&a, "x", json!({ "x": { "content": "1\n" } })).await;
    let y = change(&a, "y", json!({ "y": { "content": "y\n" } })).await;
    enqueue(&a, &x, "normal").await;
    enqueue(&a, &y, "normal").await;
    a.code.start_queues().unwrap();
    wait_queue(&a, "testing", |q| q["state"] == "testing").await;
    // A revision keeps its place but the batch is retested with the new head.
    let x = a.ok("POST", &format!("/v1/code/changes/{}/revise", id(&x)), Some(json!({ "message": "x v2", "files": { "x": { "content": "2\n" } } }))).await;
    assert_eq!(x["state"], "queued");
    // Closing y takes it out.
    let y = a.ok("POST", &format!("/v1/code/changes/{}/close", id(&y)), None).await;
    assert_eq!(y["state"], "closed");
    gate.add_permits(1000);
    a.wait_change(&id(&x), "landed").await;
    assert_eq!(file(&a, "x").await.as_deref(), Some("2\n"));
    assert_eq!(file(&a, "y").await, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_batch_under_test_is_shown_in_landing_order() {
    let (runner, gate) = QueueRunner::gated();
    let a = queue_app("[queue]\nbatch_size = 2\n", runner).await;
    let x = change(&a, "normal a", json!({ "a": { "content": "a\n" } })).await;
    let y = change(&a, "normal b", json!({ "b": { "content": "b\n" } })).await;
    let z = change(&a, "urgent c", json!({ "c": { "content": "c\n" } })).await;
    enqueue(&a, &x, "normal").await;
    enqueue(&a, &y, "normal").await;
    enqueue(&a, &z, "urgent").await;
    a.code.start_queues().unwrap();
    let q = wait_queue(&a, "testing", |q| q["state"] == "testing" && q["batch"]["tip"].is_string()).await;
    assert_eq!(q["batch"]["changes"], json!([id(&z), id(&x)]));
    let order: Vec<(Value, Value)> = q["entries"].as_array().unwrap().iter().map(|e| (e["change"].clone(), e["position"].clone())).collect();
    assert_eq!(order, vec![(json!(id(&z)), json!(1)), (json!(id(&x)), json!(2)), (json!(id(&y)), json!(3))]);
    let c = a.ok("GET", &format!("/v1/code/changes/{}", id(&z)), None).await;
    assert_eq!(c["queue"]["position"], 1);
    assert_eq!(c["queue"]["state"], "testing");
    assert!(c["queue"]["batch"].as_str().unwrap().starts_with("qb_"));
    gate.add_permits(100);
    drained(&a).await;
    assert_eq!(&main_log(&a).await[1..], ["urgent c", "normal a", "normal b"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_never_lands_a_change_that_left_the_queue() {
    // Even if nothing told the worker (the entry vanished underneath it), landing checks that
    // every change is still queued at the head that was tested.
    let (runner, gate) = QueueRunner::gated();
    let a = queue_app("", runner).await;
    let x = change(&a, "x", json!({ "x": { "content": "x\n" } })).await;
    let y = change(&a, "y", json!({ "y": { "content": "y\n" } })).await;
    enqueue(&a, &x, "normal").await;
    enqueue(&a, &y, "normal").await;
    a.code.start_queues().unwrap();
    wait_queue(&a, "testing", |q| q["state"] == "testing" && q["batch"]["tip"].is_string()).await;
    a.code.db.remove_entry(&id(&x)).unwrap();
    gate.add_permits(100);
    a.wait_change(&id(&y), "landed").await;
    drained(&a).await;
    assert_eq!(&main_log(&a).await[1..], ["y"]);
    assert_eq!(file(&a, "x").await, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn main_moving_while_a_batch_is_bisected_retests_everything() {
    let (runner, gate) = QueueRunner::gated();
    let a = queue_app("", runner).await;
    // Main is known green: no baseline run, so the permits below map to batch and bisect runs.
    let head = a.ok("GET", P, None).await["head"].as_str().unwrap().to_string();
    let r = a.code.find_repo("acme", "api").unwrap();
    a.code.db.set_last_green(&r.id, &head).unwrap();
    let mut ids = Vec::new();
    for i in 0..4 {
        let mut files = json!({ format!("f/{i}"): { "content": "x\n" } });
        if i == 2 {
            files["bad/2"] = json!({ "content": "!\n" });
        }
        let c = change(&a, &format!("change {i}"), files).await;
        enqueue(&a, &c, "normal").await;
        ids.push(id(&c));
    }
    a.code.start_queues().unwrap();
    gate.add_permits(1);
    wait_queue(&a, "bisecting", |q| q["state"] == "bisecting").await;
    a.ok("POST", &format!("{P}/land"), Some(json!({ "message": "hotfix", "files": { "hot": { "content": "1\n" } }, "bypass": true }))).await;
    gate.add_permits(1000);
    drained(&a).await;
    let culprit = a.ok("GET", &format!("/v1/code/changes/{}", ids[2]), None).await;
    assert_eq!(culprit["queue"]["state"], "ejected", "{culprit}");
    assert_eq!(&main_log(&a).await[1..], ["hotfix", "change 0", "change 1", "change 3"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_a_repo_stops_its_queue_worker() {
    let a = queue_app("", Arc::new(QueueRunner::default())).await;
    a.ok("POST", &format!("{P}/queue/pause"), None).await;
    let x = change(&a, "x", json!({ "x": { "content": "x\n" } })).await;
    enqueue(&a, &x, "normal").await;
    a.code.start_queues().unwrap();
    for _ in 0..100 {
        if a.code.queue_workers() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(a.code.queue_workers(), 1);
    let (s, _) = a.req("DELETE", P, None, None).await;
    assert!(s.is_success());
    for _ in 0..500 {
        if a.code.queue_workers() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(a.code.queue_workers(), 0, "the worker stopped");
    assert!(a.code.db.queued_repos().unwrap().is_empty(), "its entries are gone");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_retests_what_was_in_flight() {
    let (runner, _gate) = QueueRunner::gated();
    let a = queue_app("", runner).await;
    let x = change(&a, "x", json!({ "x": { "content": "x\n" } })).await;
    enqueue(&a, &x, "normal").await;
    a.code.start_queues().unwrap();
    wait_queue(&a, "testing", |q| q["state"] == "testing").await;
    // The process "dies" with the batch under test (its runner never returns); a new one
    // opens the same state and tests the batch again.
    let code = open_in(a.dir.path(), |_| {});
    code.set_runner(Arc::new(QueueRunner::default()));
    code.start_queues().unwrap();
    let b = App { router: code_control::api::router(code.clone()), code, dir: tempfile::tempdir().unwrap() };
    let c = b.wait_change(&id(&x), "landed").await;
    assert_eq!(c["queue"]["state"], "landed");
    assert_eq!(c["landed_as"], b.ok("GET", P, None).await["head"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_finishes_a_batch_that_had_landed() {
    let a = queue_app("", Arc::new(QueueRunner::default())).await;
    let x = change(&a, "x", json!({ "x": { "content": "x\n" } })).await;
    enqueue(&a, &x, "normal").await;
    // Simulate a crash after the ref moved but before the entry was finished: land the change
    // directly and leave its entry `landing` with that changeset.
    let r = a.code.find_repo("acme", "api").unwrap();
    let cs = a.code.store.resolve(&r.id, x["head"].as_str().unwrap()).unwrap().unwrap();
    let landed = a.code.store.land(&r.id, "main", cs).await.unwrap();
    let e = a.code.db.entry_for_change(&id(&x)).unwrap().unwrap();
    a.code.db.set_entry(&e.id, code_control::model::EntryState::Landing, Some("qb_x"), Some(&code_control::cs_id(&landed.changeset))).unwrap();
    // Another `landing` entry whose tip never made it: back in line.
    let y = change(&a, "y", json!({ "y": { "content": "y\n" } })).await;
    enqueue(&a, &y, "normal").await;
    let ey = a.code.db.entry_for_change(&id(&y)).unwrap().unwrap();
    let fake_tip = a.code.store.resolve(&r.id, y["head"].as_str().unwrap()).unwrap().unwrap();
    a.code.db.set_entry(&ey.id, code_control::model::EntryState::Landing, Some("qb_x"), Some(&code_control::cs_id(&fake_tip))).unwrap();

    a.code.start_queues().unwrap();
    let c = a.wait_change(&id(&x), "landed").await;
    assert_eq!(c["landed_as"], code_control::cs_id(&landed.changeset));
    a.wait_change(&id(&y), "landed").await;
    assert_eq!(&main_log(&a).await[1..], ["x", "y"], "x landed once");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_pushes_to_main_are_queued() {
    let a = queue_app("", Arc::new(QueueRunner::default())).await;
    let base = a.serve().await;
    let url = format!("{base}/git/acme/api.git");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    git(home, home, &["clone", "-q", &url, "w"]);
    let w = home.join("w");
    write(&w, "pushed.txt", "hi\n");
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "pushed by git"]);
    let out = git_raw(home, &w, &["push", "origin", "main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the ref isn't updated yet: {err}");
    assert!(err.contains("queued as change #1 (position 1)"), "{err}");
    assert!(err.contains("roll code queue acme/api"), "{err}");
    // refs/land/main goes through the queue as well; the same head isn't queued twice.
    let out = git_raw(home, &w, &["push", "origin", "HEAD:refs/land/main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("queued as change #1"), "{err}");
    let q = queue(&a).await;
    assert_eq!(q["stats"]["depth"], 1);
    assert_eq!(q["entries"][0]["title"], "pushed by git");
    a.code.start_queues().unwrap();
    drained(&a).await;
    assert_eq!(file(&a, "pushed.txt").await.as_deref(), Some("hi\n"));
    git(home, &w, &["pull", "-q", "--rebase", "origin", "main"]);
    // Pushes to other branches are untouched by the queue.
    git(home, &w, &["push", "-q", "origin", "HEAD:feature"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blockers_keep_changes_out_of_the_queue() {
    let a = queue_app("[queue]\nrequire_change_checks = true\n", Arc::new(QueueRunner::default())).await;
    // A draft.
    let c = a.ok("POST", &format!("{P}/changes"), Some(json!({ "title": "wip", "message": "wip", "draft": true, "files": { "w": { "content": "w\n" } } }))).await;
    assert_eq!(c["draft"], true);
    let kinds = |v: &Value| v["blockers"].as_array().unwrap().iter().map(|b| b["kind"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let c = a.ok("GET", &format!("/v1/code/changes/{}", id(&c)), None).await;
    assert_eq!(kinds(&c), ["draft", "checks"]);
    let (s, v) = a.req("POST", &format!("/v1/code/changes/{}/enqueue", id(&c)), None, None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(kinds(&v), ["draft", "checks"]);
    assert!(v["error"].as_str().unwrap().contains("draft"));
    let c = a.ok("PATCH", &format!("/v1/code/changes/{}", id(&c)), Some(json!({ "draft": false, "title": "ready now" }))).await;
    assert_eq!((c["draft"].clone(), c["title"].clone()), (json!(false), json!("ready now")));
    assert_eq!(kinds(&c), ["checks"]);
    assert!(c["blockers"][0]["message"].as_str().unwrap().contains("ci (not run)"));
    a.ok("POST", &format!("/v1/code/changes/{}/checks", id(&c)), Some(json!({ "name": "ci", "status": "success" }))).await;
    let c = a.ok("GET", &format!("/v1/code/changes/{}", id(&c)), None).await;
    assert_eq!(c["blockers"], json!([]));
    enqueue(&a, &c, "normal").await;
    // Marking a queued change draft takes it out.
    let c = a.ok("PATCH", &format!("/v1/code/changes/{}", id(&c)), Some(json!({ "draft": true }))).await;
    assert_eq!(c["state"], "open");
    assert_eq!(queue(&a).await["stats"]["depth"], 0);
    let (s, _) = a.req("PATCH", &format!("/v1/code/changes/{}", id(&c)), Some(json!({ "draft": "yes" })), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // An untrusted change (a fork's pull request) needs a writer to approve its head.
    let r = a.code.find_repo("acme", "api").unwrap();
    let u = a
        .code
        .create_change_as(&r, &Caller::Admin, &json!({ "title": "from a fork", "message": "fork", "files": { "f": { "content": "f\n" } } }), None, false)
        .await
        .unwrap();
    let u = a.ok("GET", &format!("/v1/code/changes/{}", u.id), None).await;
    assert_eq!(u["trusted"], false);
    assert!(kinds(&u).contains(&"untrusted".to_string()));
    let u = a.ok("POST", &format!("/v1/code/changes/{}/approve-run", id(&u)), None).await;
    assert_eq!(u["run_approved"], true);
    assert!(!kinds(&u).contains(&"untrusted".to_string()));
    // A new revision needs approving again.
    let u = a.ok("POST", &format!("/v1/code/changes/{}/revise", id(&u)), Some(json!({ "message": "fork v2", "files": { "f": { "content": "f2\n" } } }))).await;
    assert_eq!(u["run_approved"], false);
    assert!(kinds(&u).contains(&"untrusted".to_string()));
    let audit = a.ok("GET", &format!("{P}/audit"), None).await;
    assert_eq!(audit[0]["action"], "approve_run");

    // Landed and closed changes can't be queued.
    let x = change(&a, "closed", json!({ "z": { "content": "z\n" } })).await;
    a.ok("POST", &format!("/v1/code/changes/{}/close", id(&x)), None).await;
    let (s, _) = a.req("POST", &format!("/v1/code/changes/{}/enqueue", id(&x)), None, None).await;
    assert_eq!(s, StatusCode::CONFLICT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changes_that_stop_being_mergeable_while_waiting_are_ejected() {
    let a = queue_app("", Arc::new(QueueRunner::default())).await;
    let r = a.code.find_repo("acme", "api").unwrap();
    let u = a
        .code
        .create_change_as(&r, &Caller::Admin, &json!({ "title": "fork", "message": "fork", "files": { "f": { "content": "f\n" } } }), None, false)
        .await
        .unwrap();
    a.ok("POST", &format!("/v1/code/changes/{}/approve-run", u.id), None).await;
    let v = enqueue(&a, &json!({ "id": u.id }), "normal").await;
    assert_eq!(v["status"], "queued");
    // The fork pushes a new revision while it waits: its approval was for the old head.
    a.ok("POST", &format!("/v1/code/changes/{}/revise", u.id), Some(json!({ "message": "sneaky", "files": { "f": { "content": "evil\n" } } }))).await;
    a.code.start_queues().unwrap();
    drained(&a).await;
    let c = a.ok("GET", &format!("/v1/code/changes/{}", u.id), None).await;
    assert_eq!(c["state"], "open");
    assert_eq!(c["queue"]["state"], "ejected");
    assert!(c["queue"]["error"].as_str().unwrap().contains("approve"), "{c}");
    assert_eq!(file(&a, "f").await, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_events_and_shapes() {
    let a = queue_app("[queue]\nchecks = [\"ci\"]\nbatch_size = 4\ntimeout = 60\n[main]\nauto_revert = true\n", Arc::new(QueueRunner::default())).await;
    let mut events = a.code.subscribe();
    let q = queue(&a).await;
    assert_eq!(q["repo"], "acme/api");
    assert_eq!(q["policy"], "queue");
    assert_eq!(q["state"], "idle");
    assert_eq!(q["paused"], false);
    assert_eq!(q["batch"], Value::Null);
    assert_eq!(q["entries"], json!([]));
    assert_eq!(q["recent"], json!([]));
    assert_eq!(q["stats"], json!({ "depth": 0, "landed_1h": 0, "avg_wait_s": 0.0, "batch_size": 4, "max_batch_size": 4 }));
    assert_eq!(q["settings"], json!({ "checks": ["ci"], "change_checks": [], "batch_size": 4, "timeout_s": 60, "require_change_checks": false, "auto_revert": true, "error": null }));
    let x = change(&a, "x", json!({ "x": { "content": "x\n" } })).await;
    enqueue(&a, &x, "normal").await;
    a.code.start_queues().unwrap();
    a.wait_change(&id(&x), "landed").await;
    let mut kinds = Vec::new();
    while let Ok(e) = events.try_recv() {
        if e.kind == "queue" {
            assert!(e.data["state"].is_string());
            assert!(e.data["depth"].is_u64());
            assert!(e.data.get("batch").is_some());
            assert!(e.data.get("paused").is_some());
        }
        kinds.push(e.kind);
    }
    assert!(kinds.contains(&"queue"), "{kinds:?}");
    let entry = &queue(&a).await["recent"][0];
    for k in ["id", "change", "number", "title", "author", "position", "lane", "state", "error", "batch", "enqueued_at", "enqueued_by", "finished_at"] {
        assert!(entry.get(k).is_some(), "recent entry has {k}: {entry}");
    }
    // A broken roll.toml on main shows in the settings (defaults apply).
    a.ok("POST", &format!("{P}/land"), Some(json!({ "message": "oops", "files": { "roll.toml": { "content": "[queue]\nbatch_size = 0\n" } }, "bypass": true }))).await;
    let q = queue(&a).await;
    assert!(q["settings"]["error"].as_str().unwrap().contains("batch_size"));
    // Unknown repo, and land_policy validation.
    assert_eq!(a.req("GET", "/v1/code/repos/acme/nope/queue", None, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(a.req("PATCH", P, Some(json!({ "land_policy": "fast" })), None).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_merge_repos_land_directly_but_respect_drafts() {
    let a = app(None);
    a.repo("acme", "api").await;
    a.ok("POST", &format!("{P}/land"), Some(json!({ "message": "init", "files": { "a": { "content": "a\n" } } }))).await;
    let c = a.ok("POST", &format!("{P}/changes"), Some(json!({ "title": "d", "message": "d", "draft": true, "files": { "b": { "content": "b\n" } } }))).await;
    let (s, v) = a.req("POST", &format!("/v1/code/changes/{}/land", id(&c)), None, None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    a.ok("PATCH", &format!("/v1/code/changes/{}", id(&c)), Some(json!({ "draft": false }))).await;
    let v = a.ok("POST", &format!("/v1/code/changes/{}/land", id(&c)), None).await;
    assert_eq!(v["status"], "landed");
    // The queue still works on a post_merge repo when asked.
    let c = a.ok("POST", &format!("{P}/changes"), Some(json!({ "title": "q", "message": "q", "files": { "c": { "content": "c\n" } } }))).await;
    enqueue(&a, &c, "normal").await;
    a.code.start_queues().unwrap();
    a.wait_change(&id(&c), "landed").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revert_restores_the_parent_and_keeps_later_work() {
    let a = app(None);
    a.repo("acme", "api").await;
    let init = a
        .ok("POST", &format!("{P}/land"), Some(json!({ "message": "init", "files": {
            "keep.txt": { "content": "1\n2\n3\n4\n5\n6\n7\n8\n" },
            "gone.txt": { "content": "deleted by the culprit\n" },
            "run.sh": { "content": "#!/bin/sh\n", "mode": "100755" },
            "dir/a": { "content": "a\n" },
        } })))
        .await;
    let culprit = a
        .ok("POST", &format!("{P}/land"), Some(json!({ "base": init["changeset"], "message": "the culprit", "files": {
            "keep.txt": { "content": "1\nBROKEN\n3\n4\n5\n6\n7\n8\n" },
            "gone.txt": { "delete": true },
            "new.txt": { "content": "added by the culprit\n" },
            "run.sh": { "content": "#!/bin/sh\n", "mode": "100644" },
            "dir/a": { "delete": true },
        } })))
        .await;
    // Later work on the same file, elsewhere in it.
    a.ok("POST", &format!("{P}/land"), Some(json!({ "message": "later", "files": { "keep.txt": { "content": "1\nBROKEN\n3\n4\n5\n6\n7\nlater\n" } } }))).await;

    let r = a.code.find_repo("acme", "api").unwrap();
    let cs = a.code.store.resolve(&r.id, culprit["changeset"].as_str().unwrap()).unwrap().unwrap();
    let w = a.code.create_work(&r, &json!({ "title": "main broken", "labels": ["broken-main"], "priority": 0 })).unwrap();
    // As a suggestion, the item keeps its state (it never passes through review).
    let suggested = a.code.revert(&r, &cs, Some(&w.id), "It broke ci.", true).await.unwrap();
    assert_eq!(a.code.work_or_404(&w.id).unwrap().state.as_str(), "open");
    a.code.close_change(&suggested).unwrap();
    let revert = a.code.revert(&r, &cs, Some(&w.id), "It broke ci.", false).await.unwrap();
    assert_eq!(a.code.work_or_404(&w.id).unwrap().state.as_str(), "review", "a revert that will land starts the work");
    assert_eq!(revert.title, "Revert: the culprit");
    assert_eq!(revert.author, "roll");
    assert_eq!(revert.work_item.as_deref(), Some(w.id.as_str()));
    assert!(revert.body.contains("It broke ci."));
    let v = a.code.land_revert(&r, &revert).await.unwrap();
    assert_eq!(v["status"], "landed", "{v}");
    assert_eq!(file(&a, "keep.txt").await.as_deref(), Some("1\n2\n3\n4\n5\n6\n7\nlater\n"), "the culprit's line is undone, later work kept");
    assert_eq!(file(&a, "gone.txt").await.as_deref(), Some("deleted by the culprit\n"));
    assert_eq!(file(&a, "new.txt").await, None);
    assert_eq!(file(&a, "dir/a").await.as_deref(), Some("a\n"));
    let tree = a.ok("GET", &format!("{P}/tree/main"), None).await;
    let run = tree["entries"].as_array().unwrap().iter().find(|e| e["name"] == "run.sh").unwrap().clone();
    assert_eq!(run["mode"], "100755", "the mode comes back too");
    let item = a.ok("GET", &format!("/v1/code/work/{}", w.id), None).await;
    assert_eq!(item["state"], "done", "landing the revert finishes the broken-main item");
    // The first changeset can't be reverted.
    let first = a.code.store.resolve(&r.id, init["changeset"].as_str().unwrap()).unwrap().unwrap();
    assert!(a.code.revert(&r, &first, None, "", false).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revert_on_a_queue_repo_jumps_the_line() {
    let a = queue_app("[queue]\nbatch_size = 1\n", Arc::new(QueueRunner::default())).await;
    let culprit = a.ok("POST", &format!("{P}/land"), Some(json!({ "message": "culprit", "files": { "c": { "content": "c\n" } }, "bypass": true }))).await;
    let x = change(&a, "waiting", json!({ "w": { "content": "w\n" } })).await;
    enqueue(&a, &x, "normal").await;
    let r = a.code.find_repo("acme", "api").unwrap();
    let cs = a.code.store.resolve(&r.id, culprit["changeset"].as_str().unwrap()).unwrap().unwrap();
    let revert = a.code.revert(&r, &cs, None, "It broke ci.", false).await.unwrap();
    let v = a.code.land_revert(&r, &revert).await.unwrap();
    assert_eq!(v["status"], "queued");
    assert_eq!(v["position"], 1, "urgent, ahead of the waiting change");
    assert_eq!(v["change"]["queue"]["lane"], "urgent");
    assert_eq!(v["change"]["queue"]["enqueued_by"], "roll");
    a.code.start_queues().unwrap();
    drained(&a).await;
    assert_eq!(&main_log(&a).await[1..], ["culprit", "Revert \"culprit\"", "waiting"]);
    assert_eq!(file(&a, "c").await, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_culprit_bisects_from_the_last_green_head() {
    let runner = Arc::new(QueueRunner::default());
    let a = app(None);
    a.repo("acme", "api").await;
    a.code.set_runner(runner.clone());
    let green = a.ok("POST", &format!("{P}/land"), Some(json!({ "message": "init", "files": { "roll.toml": { "content": toml("") } } }))).await;
    let r = a.code.find_repo("acme", "api").unwrap();
    // No known green head yet: nothing to bisect from.
    let head = a.code.store.resolve(&r.id, green["changeset"].as_str().unwrap()).unwrap().unwrap();
    let failing: std::collections::BTreeSet<String> = ["ci".to_string()].into();
    assert_eq!(a.code.find_culprit(&r, &head, &failing).await.unwrap(), None);
    a.code.db.set_last_green(&r.id, green["changeset"].as_str().unwrap()).unwrap();
    let mut last = None;
    let mut culprit = None;
    for i in 0..16 {
        let mut files = json!({ format!("f/{i}"): { "content": "x\n" } });
        if i == 11 {
            files["bad/x"] = json!({ "content": "!\n" });
        }
        let v = a.ok("POST", &format!("{P}/land"), Some(json!({ "message": format!("c{i}"), "files": files }))).await;
        if i == 11 {
            culprit = v["changeset"].as_str().map(str::to_string);
        }
        last = v["changeset"].as_str().map(str::to_string);
    }
    let bad = a.code.store.resolve(&r.id, &last.unwrap()).unwrap().unwrap();
    let before = runner.runs.load(Ordering::SeqCst);
    let found = a.code.find_culprit(&r, &bad, &failing).await.unwrap().unwrap();
    assert_eq!(code_control::cs_id(&found), culprit.unwrap());
    assert!(runner.runs.load(Ordering::SeqCst) - before <= 4, "log2(16) tests");
}

/// Changes through the queue (batch size 8) with 2% culprits: every other change lands exactly
/// once, in queue order, on a linear main. `CODE_QUEUE_N` sets how many (`pnpm test:code`
/// runs 2,000; the default keeps `cargo test` quick, since every test materializes the tree).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn throughput_with_culprits() {
    let n: usize = std::env::var("CODE_QUEUE_N").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
    let runner = Arc::new(QueueRunner::default());
    let a = queue_app("", runner.clone()).await;
    let r = a.code.find_repo("acme", "api").unwrap();
    // Deterministic culprits: ~2%.
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let mut culprits = std::collections::BTreeSet::new();
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let bad = seed.is_multiple_of(50);
        let mut files = json!({ format!("f/{}/{i}", i % 64): { "content": format!("{i}\n") } });
        if bad {
            files["bad/x"] = json!({ "content": format!("{i}\n") });
            culprits.insert(i);
        }
        let c = a.code.create_change(&r, &Caller::Admin, &json!({ "title": format!("change {i}"), "message": format!("change {i}"), "files": files }), None).await.unwrap();
        a.code.enqueue(&r, &c, code_control::model::Lane::Normal, &Caller::Admin).await.unwrap();
        ids.push(c.id);
    }
    let t0 = Instant::now();
    a.code.start_queues().unwrap();
    let mut last = Value::Null;
    for _ in 0..60_000 {
        last = queue(&a).await;
        if last["stats"]["depth"] == 0 && last["batch"].is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(last["stats"]["depth"], 0, "the queue drained: {}", last["stats"]);
    let secs = t0.elapsed().as_secs_f64();
    let landed = n - culprits.len();
    println!(
        "merge queue: {landed} changes landed ({} culprits ejected) in {secs:.1}s = {:.0} merges/s · {} test runs",
        culprits.len(),
        landed as f64 / secs,
        runner.plans.load(Ordering::SeqCst)
    );
    for (i, id) in ids.iter().enumerate() {
        let c = a.code.change_or_404(id).unwrap();
        let want = if culprits.contains(&i) { "ejected" } else { "landed" };
        assert_eq!(c.queue.as_ref().map(|q| q.state.as_str()), Some(want), "change {i}");
    }
    let log = main_log(&a).await;
    assert_eq!(log.len(), 1 + landed, "each change landed exactly once");
    let order: Vec<usize> = log[1..].iter().map(|m| m.strip_prefix("change ").unwrap().parse().unwrap()).collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "queue order");
    assert!(order.iter().all(|i| !culprits.contains(i)));
    // Linear: every changeset on main has one parent.
    let head = a.code.head(&r).unwrap().unwrap();
    let mut cur = Some(head);
    while let Some(c) = cur {
        let m = a.code.store.meta(&r.id, &c).unwrap().unwrap();
        assert!(m.cs.parents.len() <= 1);
        cur = m.cs.parents.first().copied();
    }
    assert!(a.code.store.read_file(&r.id, &head, "bad/x").await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lanes_can_change_without_losing_place() {
    let a = queue_app("[queue]\nbatch_size = 1\n", Arc::new(QueueRunner::default())).await;
    let x = change(&a, "x", json!({ "x": { "content": "x\n" } })).await;
    let y = change(&a, "y", json!({ "y": { "content": "y\n" } })).await;
    let z = change(&a, "z", json!({ "z": { "content": "z\n" } })).await;
    for c in [&x, &y, &z] {
        enqueue(&a, c, "normal").await;
    }
    let before = a.ok("GET", &format!("/v1/code/changes/{}", id(&z)), None).await;
    assert_eq!(before["queue"]["position"], 3);
    let moved = a.ok("PATCH", &format!("/v1/code/changes/{}/enqueue", id(&z)), Some(json!({ "lane": "urgent" }))).await;
    assert_eq!((moved["queue"]["lane"].as_str(), moved["queue"]["position"].as_u64()), (Some("urgent"), Some(1)));
    assert_eq!(moved["queue"]["enqueued_at"], before["queue"]["enqueued_at"], "it keeps when it was queued");
    assert_eq!(moved["queue"]["id"], before["queue"]["id"]);
    // Back to normal: behind x and y again, where its time puts it.
    let back = a.ok("PATCH", &format!("/v1/code/changes/{}/enqueue", id(&z)), Some(json!({ "lane": "normal" }))).await;
    assert_eq!(back["queue"]["position"], 3);
    // y moves up ahead of x, but x stays ahead of z.
    a.ok("PATCH", &format!("/v1/code/changes/{}/enqueue", id(&y)), Some(json!({ "lane": "urgent" }))).await;
    let q = queue(&a).await;
    let order: Vec<Value> = q["entries"].as_array().unwrap().iter().map(|e| e["change"].clone()).collect();
    assert_eq!(order, vec![json!(id(&y)), json!(id(&x)), json!(id(&z))]);
    let audit = a.ok("GET", &format!("{P}/audit"), None).await;
    assert!(audit.as_array().unwrap().iter().any(|e| e["action"] == "queue_lane"));
    // Errors: no lane, a bad lane, a change that isn't queued.
    let (s, _) = a.req("PATCH", &format!("/v1/code/changes/{}/enqueue", id(&x)), Some(json!({})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = a.req("PATCH", &format!("/v1/code/changes/{}/enqueue", id(&x)), Some(json!({ "lane": "vip" })), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let w = change(&a, "w", json!({ "w": { "content": "w\n" } })).await;
    let (s, _) = a.req("PATCH", &format!("/v1/code/changes/{}/enqueue", id(&w)), Some(json!({ "lane": "urgent" })), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    a.code.start_queues().unwrap();
    drained(&a).await;
    assert_eq!(&main_log(&a).await[1..], ["y", "x", "z"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_batch_shows_the_prefix_under_test() {
    let (runner, gate) = QueueRunner::gated();
    let a = queue_app("[queue]\nbatch_size = 4\n", runner).await;
    let mut ids = Vec::new();
    for i in 0..4 {
        let mut files = json!({ format!("f/{i}"): { "content": "x\n" } });
        if i == 1 {
            files["bad/1"] = json!({ "content": "breaks ci\n" });
        }
        let c = change(&a, &format!("change {i}"), files).await;
        enqueue(&a, &c, "normal").await;
        ids.push(id(&c));
    }
    assert_eq!(a.ok("GET", P, None).await["stats"]["queue_batch"], 0, "nothing in flight yet");
    a.code.start_queues().unwrap();
    let q = wait_queue(&a, "the whole batch under test", |q| q["state"] == "testing" && q["batch"]["testing_prefix"] == 4).await;
    assert_eq!(q["batch"]["changes"].as_array().unwrap().len(), 4);
    assert_eq!(a.ok("GET", P, None).await["stats"]["queue_batch"], 4);
    // The batch fails; the head is tested once (never green before), then half the batch.
    gate.add_permits(2);
    let q = wait_queue(&a, "bisecting half", |q| q["state"] == "bisecting" && q["batch"]["testing_prefix"] == 2).await;
    assert_eq!(q["batch"]["changes"].as_array().unwrap().len(), 4);
    gate.add_permits(100);
    drained(&a).await;
    assert_eq!(a.ok("GET", P, None).await["stats"]["queue_batch"], 0);
    let culprit = a.ok("GET", &format!("/v1/code/changes/{}", ids[1]), None).await;
    assert_eq!(culprit["queue"]["state"], "ejected");
    assert!(culprit["queue"]["output"].as_str().unwrap().contains("1 test failed"));
    let q = queue(&a).await;
    let recent = q["recent"].as_array().unwrap().iter().find(|e| e["change"] == ids[1].as_str()).unwrap().clone();
    assert_eq!(recent["error"], "failed the queue's checks: ci");
    assert!(recent["output"].as_str().unwrap().contains("1 test failed"));
    // A conflict's ejection has a reason and no output.
    let landed = a.wait_change(&ids[0], "landed").await;
    assert!(landed["queue"]["output"].is_null());
}

/// The landing settings only parse `[queue]` and `[main]`; a check table the runner rejects
/// must still show as the settings' error instead of a healthy queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_check_table_the_runner_rejects_shows_in_the_settings() {
    let a = queue_app("[queue]\nchecks = [\"ci\"]\n", Arc::new(QueueRunner::default())).await;
    assert_eq!(queue(&a).await["settings"]["error"], Value::Null);
    let broken = "[checks.ci]\nimage = \"busybox\"\nrun = \"true\"\ntimeout = \"60s\"\n[queue]\nchecks = [\"ci\"]\n";
    a.ok("POST", &format!("{P}/land"), Some(json!({ "message": "oops", "files": { "roll.toml": { "content": broken } }, "bypass": true }))).await;
    let q = queue(&a).await;
    assert!(q["settings"]["error"].as_str().unwrap().contains("timeout"), "{q}");
    // The [queue] table itself still applies.
    assert_eq!(q["settings"]["checks"], json!(["ci"]));
}

/// Wait until a change's queue entry is in `state`.
async fn wait_entry(a: &App, id: &str, state: &str) -> Value {
    let mut last = Value::Null;
    for _ in 0..3000 {
        last = a.ok("GET", &format!("/v1/code/changes/{id}"), None).await;
        if last["queue"]["state"] == state {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("change {id}'s entry never became {state}: {last}");
}

/// The batch size halves after every red batch (never below 1), doubles after every green one,
/// never goes above `[queue] batch_size`, and follows a lowered `batch_size` at once. The next
/// batch takes no more than the current size.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_batch_size_halves_on_red_and_doubles_on_green() {
    let runner = Arc::new(QueueRunner::default());
    let a = queue_app("[queue]\nbatch_size = 4\n", runner.clone()).await;
    a.code.start_queues().unwrap();
    let size = |q: &Value| q["stats"]["batch_size"].as_u64().unwrap();
    assert_eq!(size(&queue(&a).await), 4, "starts at the most");
    for (i, want) in [2u64, 1, 1].into_iter().enumerate() {
        let c = change(&a, &format!("culprit {i}"), json!({ format!("bad/{i}"): { "content": "!\n" } })).await;
        enqueue(&a, &c, "normal").await;
        wait_entry(&a, &id(&c), "ejected").await;
        let q = wait_queue(&a, "the halved size", |q| q["batch"].is_null() && size(q) == want).await;
        assert_eq!(size(&q), want, "after red batch {i}");
    }
    // At size 1, two waiting changes take two batches (1, then 2); the size ends at 4.
    a.ok("POST", &format!("{P}/queue/pause"), None).await;
    let x = change(&a, "green x", json!({ "x": { "content": "x\n" } })).await;
    let y = change(&a, "green y", json!({ "y": { "content": "y\n" } })).await;
    enqueue(&a, &x, "normal").await;
    enqueue(&a, &y, "normal").await;
    let plans = runner.plans.load(Ordering::SeqCst);
    a.ok("POST", &format!("{P}/queue/resume"), None).await;
    a.wait_change(&id(&y), "landed").await;
    let q = wait_queue(&a, "the doubled size", |q| q["batch"].is_null() && size(q) == 4).await;
    assert_eq!(runner.plans.load(Ordering::SeqCst) - plans, 2, "one batch per change at size 1, then 2");
    assert_eq!(q["stats"]["max_batch_size"], 4);
    // Green at the cap stays at the cap.
    let z = change(&a, "green z", json!({ "z": { "content": "z\n" } })).await;
    enqueue(&a, &z, "normal").await;
    a.wait_change(&id(&z), "landed").await;
    assert_eq!(size(&wait_queue(&a, "idle", |q| q["batch"].is_null()).await), 4);
    // Lowering `[queue] batch_size` caps the current size right away.
    a.ok("POST", &format!("{P}/land"), Some(json!({ "message": "smaller batches", "files": { "roll.toml": { "content": toml("[queue]\nbatch_size = 2\n") } }, "bypass": true }))).await;
    let q = queue(&a).await;
    assert_eq!((size(&q), q["stats"]["max_batch_size"].as_u64()), (2, Some(2)));
}

/// Pausing stops new batches but lets the one under test finish; the queue's state goes
/// testing → landing → paused, and the `queue` events say so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pausing_lets_the_batch_under_test_finish() {
    let (runner, gate) = QueueRunner::gated();
    let a = queue_app("[queue]\nbatch_size = 1\n", runner).await;
    let mut events = a.code.subscribe();
    let x = change(&a, "x", json!({ "x": { "content": "x\n" } })).await;
    let y = change(&a, "y", json!({ "y": { "content": "y\n" } })).await;
    enqueue(&a, &x, "normal").await;
    enqueue(&a, &y, "normal").await;
    a.code.start_queues().unwrap();
    wait_queue(&a, "testing", |q| q["state"] == "testing" && q["batch"]["tip"].is_string()).await;
    let q = a.ok("POST", &format!("{P}/queue/pause"), None).await;
    assert_eq!((q["state"].as_str(), q["paused"].as_bool()), (Some("testing"), Some(true)), "the batch in flight shows over the pause");
    gate.add_permits(100);
    a.wait_change(&id(&x), "landed").await;
    let q = wait_queue(&a, "paused and idle", |q| q["batch"].is_null()).await;
    assert_eq!(q["state"], "paused");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let c = a.ok("GET", &format!("/v1/code/changes/{}", id(&y)), None).await;
    assert_eq!((c["state"].as_str(), c["queue"]["state"].as_str()), (Some("queued"), Some("waiting")), "no new batch while paused");
    let mut states = Vec::new();
    while let Ok(e) = events.try_recv() {
        if e.kind == "queue" {
            states.push(e.data["state"].as_str().unwrap().to_string());
        }
    }
    let at = |s: &str| states.iter().position(|x| x == s).unwrap_or_else(|| panic!("no {s} event: {states:?}"));
    assert!(at("idle") < at("testing") && at("testing") < at("landing") && at("landing") < states.iter().rposition(|x| x == "paused").unwrap(), "{states:?}");
    a.ok("POST", &format!("{P}/queue/resume"), None).await;
    a.wait_change(&id(&y), "landed").await;
}

/// Dequeueing a change while its batch is being bisected: the bisect stops, the batch is
/// rebuilt without it (bisecting → waiting → testing), and the culprit is still found.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dequeueing_during_a_bisect_rebuilds_the_batch() {
    let (runner, gate) = QueueRunner::gated();
    let a = queue_app("", runner).await;
    let head = a.ok("GET", P, None).await["head"].as_str().unwrap().to_string();
    let r = a.code.find_repo("acme", "api").unwrap();
    a.code.db.set_last_green(&r.id, &head).unwrap();
    let mut ids = Vec::new();
    for i in 0..4 {
        let mut files = json!({ format!("f/{i}"): { "content": "x\n" } });
        if i == 2 {
            files["bad/2"] = json!({ "content": "!\n" });
        }
        let c = change(&a, &format!("change {i}"), files).await;
        enqueue(&a, &c, "normal").await;
        ids.push(id(&c));
    }
    a.code.start_queues().unwrap();
    gate.add_permits(1);
    wait_queue(&a, "bisecting", |q| q["state"] == "bisecting").await;
    assert_eq!(a.ok("GET", &format!("/v1/code/changes/{}", ids[3]), None).await["queue"]["state"], "bisecting");
    a.ok("DELETE", &format!("/v1/code/changes/{}/enqueue", ids[0]), None).await;
    // The half under test finishes; the batch is thrown away and rebuilt from the three left.
    gate.add_permits(1);
    let q = wait_queue(&a, "the rebuilt batch", |q| q["state"] == "testing" && q["batch"]["changes"].as_array().is_some_and(|c| c.len() == 3)).await;
    assert_eq!(q["batch"]["changes"], json!([ids[1], ids[2], ids[3]]));
    gate.add_permits(1000);
    drained(&a).await;
    assert_eq!(a.ok("GET", &format!("/v1/code/changes/{}", ids[2]), None).await["queue"]["state"], "ejected");
    assert_eq!(a.ok("GET", &format!("/v1/code/changes/{}", ids[0]), None).await["state"], "open");
    assert_eq!(&main_log(&a).await[1..], ["change 1", "change 3"]);
}

/// A restart while a batch is bisected: its entries go back in line and are tested again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_during_a_bisect_retests_the_batch() {
    let (runner, gate) = QueueRunner::gated();
    let a = queue_app("", runner).await;
    let head = a.ok("GET", P, None).await["head"].as_str().unwrap().to_string();
    let r = a.code.find_repo("acme", "api").unwrap();
    a.code.db.set_last_green(&r.id, &head).unwrap();
    let x = change(&a, "x", json!({ "x": { "content": "x\n" } })).await;
    let y = change(&a, "y", json!({ "y": { "content": "y\n" }, "bad/y": { "content": "!\n" } })).await;
    enqueue(&a, &x, "normal").await;
    enqueue(&a, &y, "normal").await;
    a.code.start_queues().unwrap();
    gate.add_permits(1);
    wait_queue(&a, "bisecting", |q| q["state"] == "bisecting").await;
    let code = open_in(a.dir.path(), |_| {});
    code.set_runner(Arc::new(QueueRunner::default()));
    code.start_queues().unwrap();
    let b = App { router: code_control::api::router(code.clone()), code, dir: tempfile::tempdir().unwrap() };
    b.wait_change(&id(&x), "landed").await;
    let y = b.wait_change(&id(&y), "open").await;
    assert_eq!(y["queue"]["state"], "ejected");
    assert_eq!(&main_log(&b).await[1..], ["x"]);
}

/// A failed step (here the runner itself errors) puts the batch back in line and tries again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_step_puts_the_batch_back_and_retries() {
    let runner = Arc::new(QueueRunner { fail_plans: AtomicUsize::new(2), ..QueueRunner::default() });
    let a = queue_app("", runner.clone()).await;
    let x = change(&a, "x", json!({ "x": { "content": "x\n" } })).await;
    enqueue(&a, &x, "normal").await;
    a.code.start_queues().unwrap();
    let c = a.wait_change(&id(&x), "landed").await;
    assert_eq!(c["queue"]["state"], "landed");
    assert_eq!(runner.plans.load(Ordering::SeqCst), 3, "two failed tries, then the test that passed");
    assert_eq!(&main_log(&a).await[1..], ["x"]);
}

/// A queued change whose head reached the default branch some other way counts as landed when
/// its batch is built (testing → landed), and isn't landed twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_already_on_main_counts_as_landed() {
    let a = queue_app("", Arc::new(QueueRunner::default())).await;
    let x = change(&a, "x", json!({ "x": { "content": "x\n" } })).await;
    enqueue(&a, &x, "normal").await;
    let r = a.code.find_repo("acme", "api").unwrap();
    let cs = a.code.store.resolve(&r.id, x["head"].as_str().unwrap()).unwrap().unwrap();
    a.code.store.land(&r.id, "main", cs).await.unwrap();
    a.code.start_queues().unwrap();
    let c = a.wait_change(&id(&x), "landed").await;
    assert_eq!(c["queue"]["state"], "landed");
    assert_eq!(c["landed_as"], code_control::cs_id(&cs));
    drained(&a).await;
    assert_eq!(&main_log(&a).await[1..], ["x"]);
}

/// Every reason a waiting change leaves at the gate: it no longer exists, a required check went
/// red, it was marked draft or closed behind the queue's back. A closed change stays closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blockers_that_appear_while_waiting_eject_at_the_gate() {
    let a = queue_app("[queue]\nrequire_change_checks = true\n", Arc::new(QueueRunner::default())).await;
    let mut cs = Vec::new();
    for name in ["gone", "red", "draft", "closed", "fine"] {
        let c = change(&a, name, json!({ name: { "content": format!("{name}\n") } })).await;
        a.ok("POST", &format!("/v1/code/changes/{}/checks", id(&c)), Some(json!({ "name": "ci", "status": "success" }))).await;
        enqueue(&a, &c, "normal").await;
        cs.push(id(&c));
    }
    let gone_entry = a.ok("GET", &format!("/v1/code/changes/{}", cs[0]), None).await["queue"]["id"].clone();
    a.code.db.lock().execute("DELETE FROM changes WHERE id = ?", [&cs[0]]).unwrap();
    a.ok("POST", &format!("/v1/code/changes/{}/checks", cs[1]), Some(json!({ "name": "ci", "status": "failure" }))).await;
    a.code.db.update_change_meta(&cs[2], None, None, Some(true)).unwrap();
    a.code.db.set_change_state(&cs[3], code_control::model::ChangeState::Closed, None, None).unwrap();
    a.code.start_queues().unwrap();
    a.wait_change(&cs[4], "landed").await;
    let q = drained(&a).await;
    let gone = q["recent"].as_array().unwrap().iter().find(|e| e["id"] == gone_entry).unwrap();
    assert_eq!((gone["state"].as_str(), gone["error"].as_str()), (Some("ejected"), Some("the change no longer exists")));
    let red = a.wait_change(&cs[1], "open").await;
    assert_eq!(red["queue"]["state"], "ejected");
    assert!(red["queue"]["error"].as_str().unwrap().contains("ci (failure)"), "{red}");
    let draft = a.wait_change(&cs[2], "open").await;
    assert_eq!(draft["queue"]["state"], "ejected");
    assert!(draft["queue"]["error"].as_str().unwrap().contains("draft"), "{draft}");
    let closed = wait_entry(&a, &cs[3], "ejected").await;
    assert_eq!(closed["state"], "closed", "ejecting doesn't reopen it");
    assert!(closed["queue"]["error"].as_str().unwrap().contains("closed"), "{closed}");
    assert_eq!(&main_log(&a).await[1..], ["fine"]);
}

/// A required check that goes red while the change's batch is under test keeps it from
/// landing: the batch goes back in line and the gate ejects it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_check_going_red_during_the_test_keeps_the_change_off_main() {
    let (runner, gate) = QueueRunner::gated();
    let a = queue_app("[queue]\nrequire_change_checks = true\n", runner).await;
    let x = change(&a, "x", json!({ "x": { "content": "x\n" } })).await;
    let y = change(&a, "y", json!({ "y": { "content": "y\n" } })).await;
    for c in [&x, &y] {
        a.ok("POST", &format!("/v1/code/changes/{}/checks", id(c)), Some(json!({ "name": "ci", "status": "success" }))).await;
        enqueue(&a, c, "normal").await;
    }
    a.code.start_queues().unwrap();
    wait_queue(&a, "testing both", |q| q["state"] == "testing" && q["batch"]["changes"].as_array().is_some_and(|c| c.len() == 2)).await;
    a.ok("POST", &format!("/v1/code/changes/{}/checks", id(&x)), Some(json!({ "name": "ci", "status": "failure" }))).await;
    gate.add_permits(1000);
    a.wait_change(&id(&y), "landed").await;
    let x = a.wait_change(&id(&x), "open").await;
    assert_eq!(x["queue"]["state"], "ejected");
    assert!(x["queue"]["error"].as_str().unwrap().contains("ci (failure)"), "{x}");
    assert_eq!(&main_log(&a).await[1..], ["y"]);
    assert_eq!(file(&a, "x").await, None);
}

/// A batch whose checks outlive `[queue] timeout` fails them, and its change is ejected with
/// the timeout as the output.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_that_runs_out_of_time_is_ejected() {
    let (runner, _gate) = QueueRunner::gated();
    let a = queue_app("[queue]\ntimeout = 1\n", runner).await;
    let head = a.ok("GET", P, None).await["head"].as_str().unwrap().to_string();
    let r = a.code.find_repo("acme", "api").unwrap();
    a.code.db.set_last_green(&r.id, &head).unwrap();
    let x = change(&a, "slow", json!({ "x": { "content": "x\n" } })).await;
    enqueue(&a, &x, "normal").await;
    a.code.start_queues().unwrap();
    let x = a.wait_change(&id(&x), "open").await;
    assert_eq!(x["queue"]["state"], "ejected");
    assert_eq!(x["queue"]["error"], "failed the queue's checks: ci");
    assert!(x["queue"]["output"].as_str().unwrap().contains("timeout (1s) ran out"), "{x}");
}
