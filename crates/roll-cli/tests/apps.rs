//! `roll apps …` end to end: roll apps (its fake runtime, no Docker) and the workspace API run in
//! process, and the real `roll` binary talks to them through `ROLL_API_URL`.

use apps_control::integrations::{NoResources, NoSecrets, NoTls, StaticDns};
use apps_control::runtime::fake::Fake;
use apps_control::store::{Sealer, Store};
use apps_control::{Apps, AppsConfig, Deps};
use serde_json::Value;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

struct T {
    api: String,
    work: tempfile::TempDir,
    _data: tempfile::TempDir,
}

async fn start() -> T {
    let data = tempfile::tempdir().unwrap();
    let ws = Arc::new(roll_workspace::Workspace::open(&code_sql::Target::Sqlite(data.path().join("ws.db")), Default::default()).unwrap());
    ws.create("", "acme", "", None).unwrap();
    let db = code_sql::Db::open_sqlite(&data.path().join("apps.db"), code_sql::SqliteOptions { readers: 2, cache_kib: 1024 }).unwrap();
    let store = Store::new(db, Sealer::new("0123456789abcdef0123456789abcdef").unwrap()).unwrap();
    let cluster = roll_cluster::Cluster::join(Box::new(roll_cluster::Memory::solo()), roll_cluster::ClusterConfig::new("n1", "http://127.0.0.1:1", b"0123456789abcdef".to_vec())).unwrap();
    let fake = Fake::new();
    let mut cfg = AppsConfig::new(data.path().to_path_buf());
    cfg.live.probe_every = Duration::from_millis(2);
    let deps = Deps {
        runtime: fake.clone(),
        builder: fake,
        sources: None,
        secrets: Arc::new(NoSecrets),
        resources: Arc::new(NoResources),
        resolver: Arc::new(StaticDns::default()),
        tls: Arc::new(NoTls),
        objects: None,
        cluster,
        workspace: Some(ws.clone()),
    };
    let apps = Apps::new(cfg, store, deps);
    apps.start();
    let router = apps_control::api::router(apps).merge(roll_workspace::router(ws));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    T { api, work: tempfile::tempdir().unwrap(), _data: data }
}

impl T {
    /// `roll apps <args>`: its exit code, stdout and stderr.
    async fn run(&self, args: &[&str]) -> (Option<i32>, String, String) {
        let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_roll"))
            .arg("apps")
            .args(args)
            .current_dir(self.work.path())
            .env("ROLL_API_URL", &self.api)
            .env_remove("ROLL_API_KEY")
            // A credential of roll's in the caller's shell, which `exec`'s command never gets.
            .env("ROLL_FAKE_TOKEN", "fake-inherited-token")
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap();
        (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    }

    /// `roll apps <args>` from a directory under the work directory.
    async fn run_in(&self, sub: &str, args: &[&str]) -> (Option<i32>, String, String) {
        let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_roll")).arg("apps").args(args).current_dir(self.work.path().join(sub)).env("ROLL_API_URL", &self.api).env_remove("ROLL_API_KEY").stdin(Stdio::null()).output().await.unwrap();
        (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    }

    /// `roll up` in the work directory.
    async fn run_up(&self) -> (Option<i32>, String, String) {
        let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_roll")).arg("up").current_dir(self.work.path()).env("ROLL_API_URL", &self.api).env_remove("ROLL_API_KEY").stdin(Stdio::null()).output().await.unwrap();
        (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    }

    async fn ok(&self, args: &[&str]) -> String {
        let (code, out, err) = self.run(args).await;
        assert_eq!(code, Some(0), "roll apps {args:?}: {err}\n{out}");
        out
    }

    async fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        serde_json::from_str(&self.ok(&all).await).unwrap_or_else(|e| panic!("{e}: {args:?}"))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn variables_and_an_environment_through_the_cli() {
    let t = start().await;
    t.ok(&["create", "web", "-p", "acme", "--image", "nginx:1.27", "--port", "80"]).await;
    // Live first, so what's set next is the next deploy's.
    for _ in 0..200 {
        if t.json(&["deploys", "-s", "web", "-p", "acme"]).await[0]["status"] == "active" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // Sealed: run with, never shown again, `--reveal` included; the rest reveal as a `.env`
    // `--from-file` reads back exactly (quotes, line breaks).
    let pem = "-----BEGIN-----\nab \"c\"\n-----END-----";
    t.ok(&["vars", "-s", "web", "-p", "acme", "API_KEY=sk-live-123", &format!("PEM={pem}"), "--seal", "API_KEY"]).await;
    // What the next deploy changes, by name; and where it answers.
    let pending = t.ok(&["pending", "-s", "web", "-p", "acme"]).await;
    assert!(pending.contains("+ API_KEY") && pending.contains("+ PEM") && !pending.contains("sk-live"), "{pending}");
    assert!(t.ok(&["open", "--print", "-s", "web", "-p", "acme"]).await.trim().starts_with("http"));
    // A new name that reads as a secret is sealed without being asked; --plain keeps one revealable.
    t.ok(&["vars", "-s", "web", "-p", "acme", "GITHUB_TOKEN=ghp_x", "WEBHOOK_SECRET=w", "--plain", "WEBHOOK_SECRET"]).await;
    let listed = t.ok(&["vars", "-s", "web", "-p", "acme"]).await;
    let line = |n: &str| listed.lines().find(|l| l.starts_with(n)).unwrap_or_default().to_string();
    assert!(line("GITHUB_TOKEN").ends_with("sealed") && !line("WEBHOOK_SECRET").ends_with("sealed"), "{listed}");
    // Set and put live in one go (Heroku's config:set).
    let dep = t.json(&["vars", "-s", "web", "-p", "acme", "MODE=live", "--deploy"]).await;
    assert_eq!(dep["trigger"], "settings", "{dep}");
    let listed = t.ok(&["vars", "-s", "web", "-p", "acme"]).await;
    assert!(listed.lines().any(|l| l.starts_with("API_KEY") && l.ends_with("sealed")) && !listed.contains("sk-live-123"), "{listed}");
    let revealed = t.ok(&["vars", "-s", "web", "-p", "acme", "--reveal"]).await;
    assert!(!revealed.contains("API_KEY") && !revealed.contains("sk-live-123"), "{revealed}");
    std::fs::write(t.work.path().join("back.env"), &revealed).unwrap();
    t.ok(&["env", "-s", "web", "-p", "acme", "-e", "staging"]).await;
    t.ok(&["vars", "-s", "web", "-p", "acme", "-e", "staging", "--from-file", "back.env"]).await;
    let staging = t.ok(&["vars", "-s", "web", "-p", "acme", "-e", "staging", "--reveal"]).await;
    let pem_line = staging.lines().find(|l| l.starts_with("PEM=")).unwrap_or_else(|| panic!("{staging}"));
    assert_eq!(pem_line, revealed.lines().find(|l| l.starts_with("PEM=")).unwrap(), "read back exactly");
    // Pulled into a file here (`vercel env pull`): only this user's, the sealed ones named.
    let (code, _, err) = t.run(&["vars", "-s", "web", "-p", "acme", "--pull", "pulled.env"]).await;
    assert_eq!(code, Some(0), "{err}");
    let pulled = std::fs::read_to_string(t.work.path().join("pulled.env")).unwrap();
    assert!(pulled.contains(pem_line) && !pulled.contains("sk-live-123") && pulled.contains("sealed, not here: API_KEY") && err.contains("API_KEY"), "{pulled}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(t.work.path().join("pulled.env")).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settings_through_the_cli() {
    let t = start().await;
    t.ok(&["create", "web", "-p", "acme", "--image", "nginx:1.27", "--port", "80"]).await;
    // A rename, the health timeout, one alert changed with the others kept, a start command.
    let svc = t.json(&["set", "-s", "web", "-p", "acme", "--name", "site", "--health-timeout", "90", "--alert-p95", "800", "--alert-log", "panic", "--start", "nginx -g 'daemon off;'"]).await;
    assert_eq!(svc["name"], "site", "{svc}");
    assert_eq!(svc["run"]["health_timeout_s"], 90, "{svc}");
    let alerts = &svc["run"]["alerts"];
    assert_eq!((alerts["p95_ms"].as_u64(), alerts["memory_pct"].as_u64(), alerts["log_match"].as_str()), (Some(800), Some(90), Some("panic")), "{svc}");
    // `none` is a text setting's default; an alert left out stays.
    let svc = t.json(&["set", "-s", "site", "-p", "acme", "--start", "none", "--alert-log", "none"]).await;
    assert!(svc["run"].get("start").is_none() && svc["run"]["alerts"].get("log_match").is_none(), "{svc}");
    assert_eq!(svc["run"]["alerts"]["p95_ms"], 800, "{svc}");
    // A route alert, the capacity alert, a floor by the clock, the queue, and any setting as JSON.
    let svc = t.json(&["set", "-s", "site", "-p", "acme", "--alert-route", "post /checkout 5xx>=2%", "--alert-capacity", "20", "--floor", "mon 08:00..fri 18:00=1", "--floor-timezone", "Europe/Berlin", "--queue", "5", "--set", "concurrency=40"]).await;
    let run = &svc["run"];
    assert_eq!(run["alerts"]["routes"], serde_json::json!([{"route": "/checkout", "method": "POST", "error_pct": 2, "p95_ms": 0}]), "{svc}");
    assert_eq!((run["alerts"]["capacity_pct"].as_u64(), run["alerts"]["p95_ms"].as_u64(), run["queue_s"].as_u64(), run["concurrency"].as_u64()), (Some(20), Some(800), Some(5), Some(40)), "{svc}");
    assert_eq!(run["schedule_scale"], serde_json::json!([{"windows": [{"from": "mon 08:00", "to": "fri 18:00"}], "timezone": "Europe/Berlin", "min_instances": 1}]), "{svc}");
    // A copy: the settings and own variables, a sealed one named instead of copied.
    t.ok(&["vars", "-s", "site", "-p", "acme", "MODE=prod", "KEY=sk-1", "--seal", "KEY"]).await;
    let copy = t.json(&["duplicate", "site-copy", "-s", "site", "-p", "acme"]).await;
    assert_eq!((copy["service"]["run"]["health_timeout_s"].as_u64(), &copy["sealed"]), (Some(90), &serde_json::json!(["KEY"])), "{copy}");
    let vars = t.ok(&["vars", "-s", "site-copy", "-p", "acme", "--reveal"]).await;
    assert!(vars.contains("MODE=prod") && !vars.contains("KEY="), "{vars}");
    // A directory linked to a service made elsewhere: no --service, in it or under it.
    t.ok(&["use", "site", "-p", "acme"]).await;
    std::fs::create_dir_all(t.work.path().join("src/lib")).unwrap();
    let here = t.run_in("src/lib", &["--json", "show"]).await;
    assert_eq!((here.0, serde_json::from_str::<serde_json::Value>(&here.1).map(|v| v["name"].clone()).ok()), (Some(0), Some(serde_json::json!("site"))), "{here:?}");
    // A command here with the environment's variables, the platform's own left out, its exit code kept.
    // A variable that would steer this machine's programs (anyone who may write variables could set one) isn't set either.
    t.ok(&["vars", "-s", "site", "-p", "acme", "NODE_OPTIONS=--require=/tmp/evil.js"]).await;
    let (code, out, err) = t.run(&["exec", "-s", "site", "-p", "acme", "--", "sh", "-c", "echo \"$MODE ${PORT:-no-port} ${KEY:-no-key} ${NODE_OPTIONS:-no-node-options} ${ROLL_FAKE_TOKEN:-no-roll-token}\"; exit 3"]).await;
    assert_eq!((code, out.trim()), (Some(3), "prod no-port no-key no-node-options no-roll-token"), "{err}");
    assert!(err.contains("not set here (sealed") && err.contains("KEY") && err.contains("steer") && err.contains("NODE_OPTIONS"), "{err}");
    // What a size costs before it's made, and what its week says (nothing yet).
    let q = t.json(&["quote", "--memory", "1024", "--min", "1"]).await;
    assert!(q["month"]["least"].as_f64().is_some_and(|u| u > 0.0) && q["scales_to_zero"] == false, "{q}");
    assert!(t.json(&["advice", "-s", "site", "-p", "acme"]).await["advice"].as_array().is_some_and(Vec::is_empty));
    // `roll up` doesn't upload to an image service: it says what deploys one.
    let (code, _, err) = t.run_up().await;
    assert!(code != Some(0) && err.contains("roll deploy"), "{err}");
    // A freeze, and a deploy that overrides it; any environment setting as JSON.
    let env = t.json(&["env", "-s", "site", "-p", "acme", "--freeze", "the launch", "--set", r#"heartbeats=[{"name":"backup","every_s":86400}]"#]).await;
    assert_eq!((env["frozen"].as_bool(), env["heartbeats"][0]["name"].as_str()), (Some(true), Some("backup")), "{env}");
    let (code, _, err) = t.run(&["redeploy", "-s", "site", "-p", "acme"]).await;
    assert!(code != Some(0) && err.contains("frozen: the launch"), "{err}");
    let dep = t.json(&["redeploy", "-s", "site", "-p", "acme", "--override"]).await;
    assert!(dep["id"].as_str().is_some_and(|d| d.starts_with("dep_")), "{dep}");
    let (code, _, err) = t.run(&["env", "-s", "site", "-p", "acme", "--set", "heartbeats"]).await;
    assert!(code != Some(0) && err.contains("FIELD=JSON"), "{err}");
    let env = t.json(&["env", "-s", "site", "-p", "acme", "--unfreeze"]).await;
    assert!(env["frozen"] != true && env["freeze"].is_null(), "{env}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_tree_is_planned_before_anything_is_made() {
    let t = start().await;
    let app = t.work.path().join("web");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join("package.json"), r#"{"scripts":{"start":"node server.js"}}"#).unwrap();
    let plan = t.json(&["plan", "web"]).await;
    assert_eq!((plan["provider"].as_str(), plan["start"].as_str()), (Some("node"), Some("node server.js")), "{plan}");
    // Its roll.toml `[app]` applied as the environment's deploy would, and a mistake in it said
    // here, before anything is uploaded.
    std::fs::write(app.join("roll.toml"), "[app.environments.production]\nstart = \"node cluster.js\"\n").unwrap();
    let plan = t.json(&["plan", "web"]).await;
    assert_eq!(plan["start"], "node cluster.js", "{plan}");
    assert_eq!(t.json(&["plan", "web", "-e", "staging"]).await["start"], "node server.js");
    std::fs::write(app.join("roll.toml"), "[app.run]\nmin_instaces = 1\n").unwrap();
    let (code, _, err) = t.run(&["plan", "web"]).await;
    assert!(code != Some(0) && err.contains("min_instaces"), "{err}");
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_roll")).args(["up", "web", "-p", "acme"]).current_dir(t.work.path()).env("ROLL_API_URL", &t.api).env_remove("ROLL_API_KEY").output().await.unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("min_instaces"), "{}", String::from_utf8_lossy(&out.stderr));
    std::fs::remove_file(app.join("roll.toml")).unwrap();
    // A tree nothing builds: said before any project or service is made.
    std::fs::create_dir_all(t.work.path().join("empty")).unwrap();
    std::fs::write(t.work.path().join("empty/notes.txt"), "hi").unwrap();
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_roll")).args(["up", "empty", "-p", "acme"]).current_dir(t.work.path()).env("ROLL_API_URL", &t.api).env_remove("ROLL_API_KEY").output().await.unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("couldn't tell how to build"), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(t.json(&["list"]).await, serde_json::json!([]), "nothing made");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_projects_services_logs_read_together() {
    let t = start().await;
    t.ok(&["create", "web", "-p", "acme", "--image", "nginx:1.27", "--port", "80"]).await;
    t.ok(&["create", "api", "-p", "acme", "--image", "nginx:1.27", "--port", "80"]).await;
    // Each deploy writes its own lines; read together, each is named by its service, oldest first.
    let mut names = std::collections::BTreeSet::new();
    let mut lines = serde_json::json!([]);
    for _ in 0..100 {
        let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_roll")).args(["--json", "logs", "--all-services", "-p", "acme"]).current_dir(t.work.path()).env("ROLL_API_URL", &t.api).env_remove("ROLL_API_KEY").output().await.unwrap();
        lines = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)));
        names = lines.as_array().unwrap().iter().filter_map(|l| l["service_name"].as_str().map(str::to_string)).collect();
        if names.len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(names, ["api".to_string(), "web".to_string()].into(), "{lines}");
    let ts: Vec<i64> = lines.as_array().unwrap().iter().map(|l| l["t"].as_i64().unwrap()).collect();
    assert!(ts.windows(2).all(|w| w[0] <= w[1]), "merged by time: {ts:?}");
}

/// `roll apps exec` points a linked service's private URL at a local tunnel to it, so a command
/// here reaches the service it links as the deployed one does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_reaches_a_linked_service_through_a_tunnel() {
    let t = start().await;
    let api = t.json(&["create", "api", "-p", "acme", "--image", "nginx:1.27", "--port", "80"]).await;
    t.ok(&["create", "web", "-p", "acme", "--image", "nginx:1.27", "--port", "80"]).await;
    t.ok(&["link", "service", api["id"].as_str().unwrap(), "-s", "web", "-p", "acme"]).await;
    for _ in 0..200 {
        if t.json(&["deploys", "-s", "api", "-p", "acme"]).await[0]["status"] == "active" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let script = r#"u=${API_URL#http://}; h=${u%%:*}; p=${u##*:}; p=${p%%/*}; echo "url=$API_URL"; exec 3<>/dev/tcp/$h/$p; printf 'GET /via HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n' >&3; cat <&3"#;
    let (code, out, err) = t.run(&["exec", "-s", "web", "-p", "acme", "--", "bash", "-c", script]).await;
    assert_eq!(code, Some(0), "{err}\n{out}");
    assert!(out.contains("url=http://127.0.0.1:") && out.contains("HTTP/1.1 200") && out.contains(r#""path":"/via""#), "{out}\n{err}");
}

/// A whole project is promoted (Heroku Pipelines' promote): what each service runs in staging,
/// listed, then each promoted through its own call with that deployment as its `expect`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_project_is_promoted_whole() {
    let t = start().await;
    for name in ["web", "api"] {
        t.ok(&["create", name, "-p", "acme", "--image", "nginx:1.27", "--port", "80"]).await;
    }
    t.ok(&["env", "-s", "web", "-p", "acme", "-e", "staging", "--all-services"]).await;
    for name in ["web", "api"] {
        let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_roll")).args(["--json", "deploy", "-s", name, "-p", "acme", "-e", "staging", "--image", "nginx:1.27"]).current_dir(t.work.path()).env("ROLL_API_URL", &t.api).env_remove("ROLL_API_KEY").stdin(Stdio::null()).output().await.unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    // Live in staging first: what's promoted is what runs there.
    let mut live = Vec::new();
    for _ in 0..200 {
        live.clear();
        for name in ["web", "api"] {
            let d = t.json(&["deploys", "-s", name, "-p", "acme", "-e", "staging"]).await;
            if d[0]["status"] == "active" {
                live.push(d[0]["id"].as_str().unwrap_or_default().to_string());
            }
        }
        if live.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(live.len(), 2, "staging live for both");
    let out = t.json(&["promote", "-s", "web", "-p", "acme", "--from", "staging", "--all-services"]).await;
    let promoted: Vec<&str> = out.as_array().unwrap().iter().filter_map(|r| r["deployment"]["id"].as_str()).collect();
    assert_eq!(promoted.len(), 2, "{out}");
    for r in out.as_array().unwrap() {
        assert_eq!(r["deployment"]["trigger"], "promote", "{r}");
    }
}
