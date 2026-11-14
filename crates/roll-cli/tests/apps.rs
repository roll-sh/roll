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
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap();
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
    // Sealed: run with, never shown again, `--reveal` included; the rest reveal as a `.env`
    // `--from-file` reads back exactly (quotes, line breaks).
    let pem = "-----BEGIN-----\nab \"c\"\n-----END-----";
    t.ok(&["vars", "-s", "web", "-p", "acme", "API_KEY=sk-live-123", &format!("PEM={pem}"), "--seal", "API_KEY"]).await;
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
    // An environment's own error page, and back to roll's.
    std::fs::write(t.work.path().join("oops.html"), "<h1>oops</h1>").unwrap();
    let env = t.json(&["env", "-s", "web", "-p", "acme", "--error-page", "oops.html"]).await;
    assert_eq!(env["pages"]["error"], "<h1>oops</h1>", "{env}");
    let env = t.json(&["env", "-s", "web", "-p", "acme", "--roll-pages"]).await;
    assert!(env.get("pages").is_none(), "{env}");
}
