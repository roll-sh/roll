//! `roll agent …`, the real binary against a real relay (code-control's router on a free port):
//! the device login and the file it writes, `up` connecting a daemon that answers commands,
//! Ctrl-C stopping it cleanly, `status`, `logout`, and a revoked machine stopping `up`.

use code_control::{Code, Config};
use pg_storage::{Objects, StorageConfig};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Env {
    api: String,
    http: reqwest::Client,
    /// `ROLL_AGENT_CONFIG`.
    login: PathBuf,
    /// `ROLL_AGENT_HOME`: the daemon's data.
    data: PathBuf,
    dir: tempfile::TempDir,
    _code: Arc<Code>,
}

async fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let objects = Objects::new(&StorageConfig::Local(dir.path().join("objects"))).unwrap();
    let code = Code::open(Config::local(dir.path().join("home")), objects).unwrap();
    let router = code_control::api::router(code.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let data = dir.path().join("agent");
    std::fs::create_dir_all(&data).unwrap();
    // No real provider runs in tests: point them at binaries that don't exist.
    let config = "[providers.codex]\nbinary = \"/nonexistent/codex\"\n\n[providers.claude]\nbinary = \"/nonexistent/claude\"\n";
    std::fs::write(data.join("config.toml"), config).unwrap();
    let env = Env { api, http: reqwest::Client::new(), login: dir.path().join("config/roll/agent.json"), data, dir, _code: code };
    let (s, _) = env.req("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" }))).await;
    assert!(s.is_success());
    env
}

impl Env {
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_roll"));
        c.args(args)
            .current_dir(self.dir.path())
            .env("ROLL_API_URL", &self.api)
            .env("ROLL_AGENT_CONFIG", &self.login)
            .env("ROLL_AGENT_HOME", &self.data)
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }

    async fn run(&self, args: &[&str]) -> Output {
        let mut c = self.cmd(args);
        tokio::task::spawn_blocking(move || c.output().unwrap()).await.unwrap()
    }

    async fn req(&self, method: &str, path: &str, body: Option<Value>) -> (reqwest::StatusCode, Value) {
        let mut b = self.http.request(method.parse().unwrap(), format!("{}{path}", self.api));
        if let Some(v) = body {
            b = b.json(&v);
        }
        let r = b.send().await.unwrap();
        let s = r.status();
        (s, r.json().await.unwrap_or(Value::Null))
    }

    async fn machines(&self) -> Vec<Value> {
        self.req("GET", "/v1/code/agents/acme/machines", None).await.1.as_array().cloned().unwrap_or_default()
    }

    /// `roll agent login --name <name>`, approved from the API as soon as it prints its code;
    /// what it printed after.
    async fn login(&self, name: &str) -> String {
        let mut child = self.cmd(&["agent", "login", "--name", name]).spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let mut printed = Vec::new();
        let code = loop {
            let line = rx.recv_timeout(Duration::from_secs(10)).expect("login prints a code");
            printed.push(line.clone());
            if let Some(c) = line.split("confirm the code ").nth(1) {
                break c.trim().to_string();
            }
        };
        assert!(printed.iter().any(|l| l.contains("/dashboard/agents/connect?code=")), "{printed:?}");
        let (s, v) = self.req("POST", "/v1/code/agents/acme/login/approve", Some(json!({ "user_code": code }))).await;
        assert_eq!(s, 204, "{v}");
        let out = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()).await.unwrap();
        assert!(out.status.success(), "login failed: {}", String::from_utf8_lossy(&out.stderr));
        tokio::time::sleep(Duration::from_millis(50)).await;
        rx.try_iter().collect::<Vec<_>>().join("\n")
    }
}

fn wait_for(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return Some(s);
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Stderr lines of a running child, as they come.
fn lines(child: &mut Child) -> std::sync::mpsc::Receiver<String> {
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    rx
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_up_status_and_logout() {
    let e = env().await;
    // Not logged in yet.
    let out = e.run(&["agent", "status"]).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("roll agent login"));

    let said = e.login("laptop").await;
    assert!(said.contains("Connected laptop to acme"), "{said}");
    let mode = std::fs::metadata(&e.login).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&e.login).unwrap()).unwrap();
    assert_eq!((saved["owner"].as_str(), saved["name"].as_str(), saved["api"].as_str()), (Some("acme"), Some("laptop"), Some(e.api.as_str())));
    assert!(saved["token"].as_str().unwrap().starts_with("rlm_"));
    let machine = saved["machine"].as_str().unwrap().to_string();
    assert_eq!(e.machines().await[0]["id"], machine.as_str());

    let out = e.run(&["--json", "agent", "status"]).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!((v["accepted"].as_bool(), v["machine"].as_str()), (Some(true), Some(machine.as_str())));

    // `up`: online, and commands reach the daemon.
    let mut up = e.cmd(&["agent", "up"]).spawn().unwrap();
    let log = lines(&mut up);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !e.machines().await[0]["online"].as_bool().unwrap_or(false) {
        assert!(Instant::now() < deadline, "never online");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let info = &e.machines().await[0]["info"];
    for id in ["codex", "claude"] {
        let p = info["providers"].as_array().unwrap().iter().find(|p| p["id"] == id).cloned().unwrap_or_else(|| panic!("{id}: {info}"));
        assert_eq!(p["installed"], false, "{info}");
    }
    let project = e.dir.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let cmd = json!({ "id": "c1", "command": { "type": "project.add", "path": project, "name": "proj" } });
    let (s, reply) = e.req("POST", &format!("/v1/code/agents/acme/machines/{machine}/commands"), Some(cmd)).await;
    assert_eq!((s.as_u16(), reply["type"].as_str()), (200, Some("created")), "{reply}");
    let (_, state) = e.req("GET", "/v1/code/agents/acme/state", None).await;
    assert!(state["entities"].as_array().unwrap().iter().any(|m| m["entity"]["kind"] == "project" && m["entity"]["name"] == "proj"), "{state}");

    // Ctrl-C: a clean stop, and the machine goes offline.
    // SAFETY: signalling the child this test started.
    unsafe { libc::kill(up.id() as i32, libc::SIGINT) };
    let status = tokio::task::spawn_blocking(move || wait_for(&mut up, Duration::from_secs(10))).await.unwrap().expect("stopped on Ctrl-C");
    assert!(status.success(), "{status:?}");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let said: Vec<String> = log.try_iter().collect();
    assert!(said.iter().any(|l| l.contains("connected to roll as acme/")), "{said:?}");
    assert!(said.iter().any(|l| l.contains("stopped")), "{said:?}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while e.machines().await[0]["online"].as_bool().unwrap_or(true) {
        assert!(Instant::now() < deadline, "never offline");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Logout: the machine is gone from roll and the file from here.
    let out = e.run(&["agent", "logout"]).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!e.login.exists());
    assert!(e.machines().await.is_empty());
    assert!(!e.run(&["agent", "status"]).await.status.success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revoked_machine_stops_up_with_a_clear_error() {
    let e = env().await;
    e.login("box").await;
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&e.login).unwrap()).unwrap();
    let machine = saved["machine"].as_str().unwrap().to_string();
    let mut up = e.cmd(&["agent", "up"]).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !e.machines().await.first().is_some_and(|m| m["online"] == true) {
        assert!(Instant::now() < deadline, "never online");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (s, _) = e.req("DELETE", &format!("/v1/code/agents/acme/machines/{machine}"), None).await;
    assert_eq!(s, 204);
    let out = tokio::task::spawn_blocking(move || {
        wait_for(&mut up, Duration::from_secs(10)).expect("up stops");
        up.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("roll agent login"), "{}", String::from_utf8_lossy(&out.stderr));
    // `status` says so too.
    let out = e.run(&["agent", "status"]).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("revoked"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_denied_login_fails() {
    let e = env().await;
    let mut child = e.cmd(&["--json", "agent", "login"]).spawn().unwrap();
    let mut first = String::new();
    BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut first).unwrap();
    let start: Value = serde_json::from_str(&first).unwrap();
    let (s, _) = e.req("POST", "/v1/code/agents/acme/login/deny", Some(json!({ "user_code": start["user_code"] }))).await;
    assert_eq!(s, 204);
    let out = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()).await.unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("denied"));
    assert!(!e.login.exists());
}
