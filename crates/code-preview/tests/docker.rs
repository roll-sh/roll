//! Checks and previews against real Docker: checks on a change, a preview reached through the
//! host router, post-merge checks opening and closing a `broken-main` work item, and the preview
//! going away when its change lands. Skipped when `docker info` fails, unless
//! `ROLL_CODE_DOCKER_REQUIRED` is set.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use code_control::model::{CheckStatus, LandPolicy, RepoRow, Visibility};
use code_control::{Code, Config, Runner};
use code_preview::{DockerRunner, PreviewConfig, Previews, docker, route};
use pg_storage::{Objects, StorageConfig};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

const IMAGE: &str = "busybox:1.37";

/// Starting the service removes leftover containers: the tests take turns.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct App {
    code: Arc<Code>,
    router: axum::Router,
    _dir: tempfile::TempDir,
}

impl App {
    async fn send(&self, method: &str, path: &str, host: Option<&str>, body: Option<Value>) -> (StatusCode, Vec<u8>) {
        let mut b = Request::builder().method(method).uri(path);
        if let Some(h) = host {
            b = b.header("host", h);
        }
        let req = match body {
            Some(v) => b.header("content-type", "application/json").body(Body::from(v.to_string())).unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        (status, axum::body::to_bytes(resp.into_body(), 16 << 20).await.unwrap().to_vec())
    }

    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (s, b) = self.send(method, path, None, body).await;
        let v: Value = serde_json::from_slice(&b).unwrap_or(Value::Null);
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }

    async fn until(&self, what: &str, secs: u64, mut f: impl AsyncFnMut(&App) -> Option<Value>) -> Value {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(v) = f(self).await {
                return v;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

async fn app() -> App {
    let dir = tempfile::tempdir().unwrap();
    let objects = Objects::new(&StorageConfig::Local(dir.path().join("objects"))).unwrap();
    let code = Code::open(Config::local(dir.path().to_path_buf()), objects).unwrap();
    let previews = Previews::new(PreviewConfig::default(), None);
    code_preview::start(&code, Arc::new(DockerRunner::new(2)), previews.clone());
    let router = code_control::api::router(code.clone()).layer(axum::middleware::from_fn_with_state(previews, route));
    App { code, router, _dir: dir }
}

fn roll_toml(extra: &str) -> String {
    format!(
        "[checks.readme]\nimage = \"{IMAGE}\"\nrun = \"test -f README.md\"\ntimeout = 120\n{extra}\n\
         [preview]\nimage = \"{IMAGE}\"\nrun = \"httpd -f -p $PORT -h /app\"\nport = 8080\nmemory = 128\n"
    )
}

/// Docker tests skip without Docker, unless `ROLL_CODE_DOCKER_REQUIRED` is set (`pnpm test:code`).
/// Check containers still around, after waiting up to `secs` for them to go. A test that ends
/// mid-check leaves its `docker` child running for a moment; the containers remove themselves.
async fn check_containers_after(secs: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let ids = docker::docker(&["ps", "-aq", "--filter", "label=sh.roll.code=check"]).await.unwrap();
        if ids.is_empty() || Instant::now() > deadline {
            return ids;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn docker_or_skip() -> bool {
    if docker::available().await {
        return true;
    }
    assert!(std::env::var_os("ROLL_CODE_DOCKER_REQUIRED").is_none(), "docker isn't available");
    eprintln!("skipping: docker isn't available");
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn runner_reports_success_failure_and_timeouts() {
    let _serial = SERIAL.lock().await;
    if !docker_or_skip().await {
        return;
    }
    // Checks a previous test's runtime abandoned mid-run (it ended under them) aren't ours.
    docker::remove_leftovers("check").await;
    check_containers_after(60).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("README.md"), "# hi\n").unwrap();
    std::fs::write(
        dir.path().join("roll.toml"),
        format!(
            "[checks.ok]\nimage = \"{IMAGE}\"\nrun = \"test -f README.md && echo fine\"\n\
             [checks.bad]\nimage = \"{IMAGE}\"\nrun = \"echo boom >&2; exit 3\"\n\
             [checks.slow]\nimage = \"{IMAGE}\"\nrun = \"sleep 30\"\ntimeout = 2\n"
        ),
    )
    .unwrap();
    let repo = RepoRow {
        id: "repo_t".into(),
        owner_id: "own_t".into(),
        owner: "acme".into(),
        name: "web".into(),
        description: None,
        visibility: Visibility::Public,
        source: "native".into(),
        default_branch: "main".into(),
        land_policy: LandPolicy::PostMerge,
        required_checks: Vec::new(),
        github: None,
        import: None,
        created_at: 0,
        updated_at: 0,
    };
    let runner = DockerRunner::new(3);
    let plan = runner.plan(&repo, dir.path()).await.unwrap();
    assert_eq!(plan.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(), ["bad", "ok", "slow"]);
    assert!(plan.iter().all(|k| k.status == CheckStatus::Queued));
    let (bad, ok, slow) = tokio::join!(
        runner.run(&repo, None, dir.path(), &plan[0]),
        runner.run(&repo, None, dir.path(), &plan[1]),
        runner.run(&repo, None, dir.path(), &plan[2]),
    );
    let (bad, ok, slow) = (bad.unwrap(), ok.unwrap(), slow.unwrap());
    assert_eq!(ok.status, CheckStatus::Success, "{:?}", ok.summary);
    assert!(ok.summary.as_deref().unwrap().contains("fine"));
    assert_eq!(bad.status, CheckStatus::Failure);
    let s = bad.summary.unwrap();
    assert!(s.contains("exit code 3") && s.contains("boom"), "{s}");
    assert_eq!(slow.status, CheckStatus::Failure);
    assert!(slow.summary.unwrap().contains("timed out"));
    // Nothing is left behind.
    assert_eq!(check_containers_after(30).await, "");

    std::fs::write(dir.path().join("roll.toml"), "[checks.x]\nimage = \"a\"\n").unwrap();
    let plan = runner.plan(&repo, dir.path()).await.unwrap();
    assert_eq!((plan.len(), plan[0].name.as_str(), plan[0].status), (1, "roll.toml", CheckStatus::Failure));
}

#[tokio::test(flavor = "multi_thread")]
async fn changes_previews_and_post_merge() {
    let _serial = SERIAL.lock().await;
    if !docker_or_skip().await {
        return;
    }
    let a = app().await;
    a.ok("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" }))).await;
    a.ok("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "web" }))).await;
    let p = "/v1/code/repos/acme/web";
    let init = a
        .ok(
            "POST",
            &format!("{p}/land"),
            Some(json!({ "message": "init", "files": {
                "README.md": { "content": "# web\n" },
                "index.html": { "content": "hello preview\n" },
                "roll.toml": { "content": roll_toml("") },
            } })),
        )
        .await;
    let base = init["changeset"].as_str().unwrap().to_string();

    // A change that adds a failing check: checks run on its head.
    let c = a
        .ok(
            "POST",
            &format!("{p}/changes"),
            Some(json!({ "title": "Add a check", "base": base, "message": "add boom", "files": {
                "roll.toml": { "content": roll_toml(&format!("[checks.boom]\nimage = \"{IMAGE}\"\nrun = \"echo boom >&2; exit 3\"\n")) },
                "index.html": { "content": "hello from the change\n" },
            } })),
        )
        .await;
    let id = c["id"].as_str().unwrap().to_string();
    let path = format!("/v1/code/changes/{id}");
    let c = a
        .until("the change's checks", 240, async |a| {
            let c = a.ok("GET", &path, None).await;
            let checks = c["checks"].as_array().unwrap();
            (checks.len() == 2 && checks.iter().all(|k| ["success", "failure"].contains(&k["status"].as_str().unwrap()))).then_some(c)
        })
        .await;
    let checks = c["checks"].as_array().unwrap();
    let get = |n: &str| checks.iter().find(|k| k["name"] == n).unwrap();
    assert_eq!(get("readme")["status"], "success", "{}", get("readme"));
    assert_eq!(get("boom")["status"], "failure");
    assert_eq!(get("boom")["source"], "runner");
    assert!(get("boom")["summary"].as_str().unwrap().contains("boom"));

    // `[preview]` gives every change a preview that sleeps until visited.
    let c = a.until("the preview", 30, async |a| Some(a.ok("GET", &path, None).await).filter(|c| !c["preview"].is_null())).await;
    assert_eq!(c["preview"]["state"], "sleeping", "{}", c["preview"]);
    let url = c["preview"]["url"].as_str().unwrap();
    let host = url.trim_start_matches("http://").to_string();
    assert!(host.starts_with("chg-") && host.ends_with(".preview.localhost"), "{host}");

    // Visiting it starts the container and proxies to it.
    let body = a
        .until("the preview to answer", 240, async |a| {
            let (s, b) = a.send("GET", "/index.html", Some(&host), None).await;
            (s == StatusCode::OK).then(|| Value::String(String::from_utf8_lossy(&b).into()))
        })
        .await;
    assert_eq!(body, "hello from the change\n");
    let c = a.ok("GET", &path, None).await;
    assert_eq!(c["preview"]["state"], "running");
    let (s, _) = a.send("GET", "/", Some("chg-nope.preview.localhost"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // Other hosts still reach the API.
    let (s, _) = a.send("GET", "/v1/code", Some("127.0.0.1:7070"), None).await;
    assert_eq!(s, StatusCode::OK);

    // Landing it breaks main: one P0 broken-main item, and the preview goes away.
    a.ok("POST", &format!("{path}/land"), None).await;
    let w = a
        .until("a broken-main work item", 240, async |a| {
            let items = a.ok("GET", &format!("{p}/work?label=broken-main"), None).await;
            items.as_array().unwrap().first().cloned()
        })
        .await;
    assert_eq!(w["priority"], 0);
    assert_eq!(w["state"], "open");
    assert!(w["title"].as_str().unwrap().contains("boom"), "{}", w["title"]);
    assert!(w["body"].as_str().unwrap().contains("boom"));
    let c = a.until("the preview to go", 60, async |a| Some(a.ok("GET", &path, None).await).filter(|c| c["preview"].is_null())).await;
    assert_eq!(c["state"], "landed");
    let name = format!("roll-preview-{}", id.replace('_', "-"));
    a.until("the preview container to stop", 30, async |_| (!docker::running(&name).await).then_some(Value::Null)).await;

    // A fix closes the item.
    let head = a.ok("GET", p, None).await["head"].as_str().unwrap().to_string();
    a.ok("POST", &format!("{p}/land"), Some(json!({ "base": head, "message": "fix", "files": { "roll.toml": { "content": roll_toml("") } } }))).await;
    let w = a
        .until("the item to close", 240, async |a| {
            let w = a.ok("GET", &format!("/v1/code/work/{}", w["id"].as_str().unwrap()), None).await;
            (w["state"] == "done").then_some(w)
        })
        .await;
    assert!(w["body"].as_str().unwrap().contains("Checks pass again"));
    // Only one item was ever opened.
    let items = a.ok("GET", &format!("{p}/work?label=broken-main"), None).await;
    assert_eq!(items.as_array().unwrap().len(), 1);
    // Let checks still running (a bisection, the last post-merge run) finish before the runtime
    // goes away under them, so they don't outlive the test as stray containers.
    check_containers_after(120).await;
    drop(a.code);
    docker::remove_leftovers("check").await;
}
