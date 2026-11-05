//! `roll agent …`, the real binary against a real relay (code-control's router on a free port):
//! the device login and the file it writes, a login with a machine token, `up` connecting a
//! daemon that answers commands, Ctrl-C stopping it cleanly, `status`, `logout`, and a revoked
//! machine stopping `up`.

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
    env_with(|_| {}).await
}

/// [`env`] with the relay's config adjusted, and `setup` run on it before it serves.
async fn env_with(f: impl FnOnce(&mut Config)) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let objects = Objects::new(&StorageConfig::Local(dir.path().join("objects"))).unwrap();
    let mut config = Config::local(dir.path().join("home"));
    f(&mut config);
    let code = Code::open(config, objects).unwrap();
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
            .env_remove("ROLL_AGENT_TOKEN")
            .env_remove("ROLL_AGENT_FAKE")
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
        // It said which machine it is, for the dashboard to show before approving.
        let (s, p) = self.req("GET", &format!("/v1/code/agents/acme/login/{code}"), None).await;
        assert_eq!(s, 200, "{p}");
        assert_eq!((p["name"].as_str(), p["os"].as_str()), (Some(name), Some(std::env::consts::OS)), "{p}");
        assert!(p["hostname"].as_str().is_some_and(|h| !h.is_empty()), "{p}");
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

/// The environment process `pid` was started with (Linux's `/proc/<pid>/environ`) holds none of
/// the sandbox variables and not `secret`.
fn assert_environ_clean(pid: u32, secret: &str) {
    let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
    let vars: Vec<String> = environ.split(|b| *b == 0).map(|v| String::from_utf8_lossy(v).into_owned()).collect();
    assert!(vars.iter().any(|v| v.starts_with("ROLL_AGENT_CONFIG=")), "that's the daemon's environment: {vars:?}");
    for v in &vars {
        assert!(!v.contains(secret), "{v}");
        assert!(!["ROLL_AGENT_TOKEN=", "ROLL_AGENT_NAME="].iter().any(|k| v.starts_with(k)), "{v}");
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

/// A sandbox has no login file: `up` takes the machine token roll gave it from the environment
/// (`ROLL_AGENT_TOKEN` with `ROLL_API_URL`), and what it starts doesn't inherit the token.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn up_takes_a_sandbox_token_from_the_environment() {
    use base64::Engine;
    let e = env().await;
    let (s, minted) = e.req("POST", "/v1/code/agents/acme/machines", Some(json!({ "name": "sandbox" }))).await;
    assert_eq!(s, 201, "{minted}");
    let (machine, token) = (minted["machine"]["id"].as_str().unwrap().to_string(), minted["token"].as_str().unwrap().to_string());
    assert!(!e.login.exists());
    let mut up = e
        .cmd(&["agent", "up"])
        .env("ROLL_AGENT_TOKEN", &token)
        .env("ROLL_AGENT_MACHINE", &machine)
        .env("ROLL_AGENT_OWNER", "acme")
        .env("ROLL_AGENT_NAME", "sandbox")
        .spawn()
        .unwrap();
    let log = lines(&mut up);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !e.machines().await[0]["online"].as_bool().unwrap_or(false) {
        assert!(Instant::now() < deadline, "never online: {:?}", log.try_iter().collect::<Vec<_>>());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!e.login.exists(), "the token isn't written anywhere");
    // Not in the daemon's own environment either (`/proc/<pid>/environ` keeps what a process
    // started with, and the user's other processes can read it).
    assert_environ_clean(up.id(), &token);

    // A terminal on the machine doesn't see the token.
    let open = json!({ "id": "t1", "command": { "type": "terminal.open", "command": "echo \"token=[$ROLL_AGENT_TOKEN] machine=[$ROLL_AGENT_MACHINE]\"; sleep 5", "cols": 80, "rows": 24 } });
    let (s, reply) = e.req("POST", &format!("/v1/code/agents/acme/machines/{machine}/commands"), Some(open)).await;
    assert_eq!((s.as_u16(), reply["type"].as_str()), (200, Some("created")), "{reply}");
    let terminal = reply["id"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(10);
    let seen = loop {
        let cmd = json!({ "id": format!("s{}", Instant::now().elapsed().as_nanos()), "command": { "type": "terminal.scrollback", "terminal": terminal } });
        let (_, r) = e.req("POST", &format!("/v1/code/agents/acme/machines/{machine}/commands"), Some(cmd)).await;
        let text = String::from_utf8_lossy(&base64::engine::general_purpose::STANDARD.decode(r["data"].as_str().unwrap_or("")).unwrap_or_default()).into_owned();
        if text.contains("token=[") {
            break text;
        }
        assert!(Instant::now() < deadline, "the terminal said nothing: {r}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(seen.contains("token=[] machine=[]"), "{seen}");
    assert!(!seen.contains(&token));

    // SAFETY: signalling the child this test started.
    unsafe { libc::kill(up.id() as i32, libc::SIGINT) };
    let status = tokio::task::spawn_blocking(move || wait_for(&mut up, Duration::from_secs(10))).await.unwrap().expect("stopped on Ctrl-C");
    assert!(status.success(), "{status:?}");

    // With a login file the file wins, and the variables are dropped all the same.
    let deadline = Instant::now() + Duration::from_secs(10);
    while e.machines().await[0]["online"].as_bool().unwrap_or(false) {
        assert!(Instant::now() < deadline, "still online after Ctrl-C");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let out = e.run(&["agent", "login", "--machine-token", &token]).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut up = e.cmd(&["agent", "up"]).env("ROLL_AGENT_TOKEN", "rlm_from_the_environment").env("ROLL_AGENT_NAME", "sandbox").spawn().unwrap();
    let log = lines(&mut up);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !e.machines().await[0]["online"].as_bool().unwrap_or(false) {
        assert!(Instant::now() < deadline, "never online: {:?}", log.try_iter().collect::<Vec<_>>());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_environ_clean(up.id(), "rlm_from_the_environment");
    // SAFETY: signalling the child this test started.
    unsafe { libc::kill(up.id() as i32, libc::SIGINT) };
    let status = tokio::task::spawn_blocking(move || wait_for(&mut up, Duration::from_secs(10))).await.unwrap().expect("stopped on Ctrl-C");
    assert!(status.success(), "{status:?}");
    std::fs::remove_file(&e.login).unwrap();

    // A token that isn't a machine token is refused up front.
    let out = {
        let mut c = e.cmd(&["agent", "up"]);
        c.env("ROLL_AGENT_TOKEN", "rlc_nope");
        tokio::task::spawn_blocking(move || c.output().unwrap()).await.unwrap()
    };
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("ROLL_AGENT_TOKEN"), "{}", String::from_utf8_lossy(&out.stderr));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_machine_token_logs_in_without_a_code() {
    let e = env().await;
    let (s, minted) = e.req("POST", "/v1/code/agents/acme/machines", Some(json!({ "name": "build-box" }))).await;
    assert_eq!(s, 201, "{minted}");
    let token = minted["token"].as_str().unwrap().to_string();
    let machine = minted["machine"]["id"].as_str().unwrap().to_string();

    let out = e.run(&["--json", "agent", "login", "--machine-token", &token]).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let said: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!((said["status"].as_str(), said["machine"].as_str(), said["owner"].as_str(), said["name"].as_str()), (Some("approved"), Some(machine.as_str()), Some("acme"), Some("build-box")));
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&e.login).unwrap()).unwrap();
    assert_eq!(saved, json!({ "api": e.api, "machine": machine, "owner": "acme", "name": "build-box", "token": token }));
    assert_eq!(std::fs::metadata(&e.login).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(e.run(&["agent", "status"]).await.status.success());
    std::fs::remove_file(&e.login).unwrap();

    // From the environment too (sandboxes and scripts), and the text output.
    let mut c = e.cmd(&["agent", "login"]);
    c.env("ROLL_AGENT_TOKEN", &token);
    let out = tokio::task::spawn_blocking(move || c.output().unwrap()).await.unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("Connected build-box to acme"), "{}", String::from_utf8_lossy(&out.stdout));

    // Not a machine token, a revoked one, or with --name: exit 1/2, nothing new written.
    std::fs::remove_file(&e.login).unwrap();
    let out = e.run(&["agent", "login", "--machine-token", "rlc_nope"]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("rlm_"), "{}", String::from_utf8_lossy(&out.stderr));
    let (s, _) = e.req("DELETE", &format!("/v1/code/agents/acme/machines/{machine}"), None).await;
    assert_eq!(s, 204);
    let out = e.run(&["agent", "login", "--machine-token", &token]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("doesn't accept"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!e.run(&["agent", "login", "--machine-token", &token, "--name", "x"]).await.status.success());
    assert!(!e.login.exists());
}

/// What a sandbox going to sleep does to `roll agent up`: SIGTERM while a run waits on an approval
/// stops it cleanly with the run still waiting on roll (records sent before it exits), and after the
/// next start the approval is still answerable: the answer resumes the conversation, the agent's ask
/// for the same command is answered at once, and the run completes (the fake provider's demo turn
/// writes its file).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sigterm_parks_a_waiting_run_and_the_answer_resumes_it_after_a_restart() {
    let env = env().await;
    let e = &env;
    e.login("box").await;
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&e.login).unwrap()).unwrap();
    let machine = saved["machine"].as_str().unwrap().to_string();
    let path = format!("/v1/code/agents/acme/machines/{machine}/commands");
    let up = || {
        let mut c = e.cmd(&["agent", "up"]);
        c.env("ROLL_AGENT_FAKE", "1");
        let mut child = c.spawn().unwrap();
        let log = lines(&mut child);
        (child, log)
    };
    let online = |want: bool| async move {
        let deadline = Instant::now() + Duration::from_secs(20);
        while e.machines().await[0]["online"].as_bool().unwrap_or(false) != want {
            assert!(Instant::now() < deadline, "never online={want}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let records = |kind: &'static str| async move {
        let (_, state) = e.req("GET", "/v1/code/agents/acme/state", None).await;
        state["entities"].as_array().unwrap().iter().filter(|m| m["entity"]["kind"] == kind).map(|m| m["entity"].clone()).collect::<Vec<_>>()
    };

    let (mut daemon, _log) = up();
    online(true).await;
    let project = e.dir.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let add = json!({ "id": "p1", "command": { "type": "project.add", "path": project, "name": "proj" } });
    let (_, r) = e.req("POST", &path, Some(add)).await;
    let project_id = r["id"].as_str().unwrap_or_else(|| panic!("{r}")).to_string();
    let create = json!({ "id": "t1", "command": { "type": "thread.create", "project": project_id, "provider": "fake", "runtime_mode": "ask", "prompt": "write the demo file" } });
    let (s, r) = e.req("POST", &path, Some(create)).await;
    assert_eq!(s.as_u16(), 200, "{r}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !records("request").await.iter().any(|r| r["status"] == "pending") {
        assert!(Instant::now() < deadline, "never asked");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // SIGTERM (`docker stop`, agent37's stop): a clean exit, the run still waiting on roll.
    // SAFETY: signalling the child this test started.
    unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    let status = tokio::task::spawn_blocking(move || wait_for(&mut daemon, Duration::from_secs(10))).await.unwrap().expect("stopped on SIGTERM");
    assert!(status.success(), "{status:?}");
    online(false).await;
    let thread = records("thread").await.pop().unwrap();
    assert_eq!((thread["status"].as_str(), thread["pending_requests"].as_u64()), (Some("waiting"), Some(1)), "{thread}");
    let request = records("request").await.pop().unwrap();
    assert_eq!(request["status"], "pending", "{request}");
    assert_eq!(records("run").await[0]["status"], "waiting");

    // Woken: the same approval, answered now.
    let (mut daemon, log) = up();
    online(true).await;
    let answer = json!({ "id": "a1", "command": { "type": "request.respond", "request": request["id"], "response": { "type": "decision", "decision": "accept" } } });
    let (s, r) = e.req("POST", &path, Some(answer)).await;
    assert_eq!((s.as_u16(), r["type"].as_str()), (200, Some("ok")), "{r}");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let runs = records("run").await;
        if runs.len() == 1 && runs[0]["status"] == "completed" {
            break;
        }
        assert!(Instant::now() < deadline, "never completed: {runs:?} {:?}", log.try_iter().collect::<Vec<_>>());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let wrote = std::fs::read_to_string(project.join("ROLL_DEMO.md")).unwrap();
    assert!(wrote.contains("I approved it"), "{wrote}");
    assert!(records("request").await.iter().all(|r| r["status"] == "resolved"), "the repeated ask was answered too");
    // SAFETY: signalling the child this test started.
    unsafe { libc::kill(daemon.id() as i32, libc::SIGINT) };
    let status = tokio::task::spawn_blocking(move || wait_for(&mut daemon, Duration::from_secs(10))).await.unwrap().expect("stopped");
    assert!(status.success(), "{status:?}");
}

/// `roll agent automation …` drives the automation routes: create from a file, list, get,
/// disable and enable, run (waiting for the run, exit 1 unless it succeeded), runs, show,
/// test, catalog, delete; an unknown automation name fails with exit 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn automation_commands_drive_the_routes() {
    let e = env().await;
    let spec = e.dir.path().join("spec.json");
    std::fs::write(&spec, json!({ "name": "Hello", "trigger": { "type": "manual" }, "steps": [{ "id": "hi", "kind": "notify", "config": { "title": "hi {{event.data.input.who}}" } }] }).to_string()).unwrap();
    let spec = spec.to_str().unwrap().to_string();
    let a = |args: &[&str]| {
        let mut v = vec!["agent", "automation", "--owner", "acme"];
        v.extend_from_slice(args);
        v.into_iter().map(str::to_string).collect::<Vec<_>>()
    };
    let run = |args: Vec<String>| {
        let e = &e;
        async move {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            e.run(&refs).await
        }
    };
    let out = run(a(&["create", &spec])).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("created aut_"), "{}", String::from_utf8_lossy(&out.stdout));
    let out = run(a(&["list"])).await;
    assert!(String::from_utf8_lossy(&out.stdout).contains("Hello"), "{}", String::from_utf8_lossy(&out.stdout));
    let mut json_args = vec!["--json".to_string()];
    json_args.extend(a(&["get", "Hello"]));
    let got: Value = serde_json::from_slice(&run(json_args).await.stdout).unwrap();
    assert_eq!(got["name"], "Hello");
    for c in ["disable", "enable"] {
        assert!(run(a(&[c, "Hello"])).await.status.success(), "{c}");
    }
    let out = run(a(&["run", "Hello", "--input", r#"{"who":"ada"}"#, "--wait"])).await;
    assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let printed = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(printed.contains("succeeded"), "{printed}");
    let rid = printed.split_whitespace().next().unwrap().to_string();
    let mut json_args = vec!["--json".to_string()];
    json_args.extend(a(&["runs", "--automation", "Hello"]));
    let runs: Value = serde_json::from_slice(&run(json_args).await.stdout).unwrap();
    assert_eq!(runs[0]["id"], rid.as_str());
    let out = run(a(&["show", &rid])).await;
    assert!(String::from_utf8_lossy(&out.stdout).contains("hi"), "{}", String::from_utf8_lossy(&out.stdout));
    let event = e.dir.path().join("event.json");
    std::fs::write(&event, json!({ "source": "manual", "kind": "run", "data": { "input": { "who": "bo" } } }).to_string()).unwrap();
    let mut json_args = vec!["--json".to_string()];
    json_args.extend(a(&["test", &spec, event.to_str().unwrap()]));
    let tested: Value = serde_json::from_slice(&run(json_args).await.stdout).unwrap();
    assert_eq!(tested["steps"][0]["config"]["title"], "hi bo");
    let cat: Value = serde_json::from_slice(&run(a(&["catalog"])).await.stdout).unwrap();
    assert!(cat["steps"].as_array().unwrap().iter().any(|s| s["kind"] == "agent.start"));
    let out = run(a(&["get", "Nope"])).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no automation named"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(run(a(&["delete", "Hello"])).await.status.success());
    let out = run(a(&["list"])).await;
    assert!(!String::from_utf8_lossy(&out.stdout).contains("Hello"));
}

/// Chat ops end to end (docs/code/chatops.md, "The key flow"): the real daemon runs the fake
/// provider's turn, which asks to run a command; `slack.status` posts the thread's status and the
/// approval as buttons to a mock Slack; a signed click on Approve answers it through
/// `request.respond`; the turn finishes and the status says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slack_button_answers_the_fake_providers_approval() {
    use agent_proto::automation::{AutomationId, AutomationRunId, Step};
    use code_control::agents::automations::{OwnerRef, StepContext, StepOutcome};

    // A mock Slack Web API that records what it's sent.
    let calls: Arc<std::sync::Mutex<Vec<(String, Value)>>> = Default::default();
    let slack = {
        let calls = calls.clone();
        let n = Arc::new(std::sync::atomic::AtomicU64::new(0));
        axum::Router::new().fallback(move |uri: axum::http::Uri, body: String| {
            let (calls, n) = (calls.clone(), n.clone());
            async move {
                let b: Value = serde_json::from_str(&body).unwrap_or(Value::String(body));
                let method = uri.path().trim_start_matches('/').to_string();
                calls.lock().unwrap().push((method.clone(), b.clone()));
                let ts = format!("1700000000.{:06}", n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1);
                axum::Json(match method.as_str() {
                    "oauth.v2.access" => json!({ "ok": true, "access_token": "xoxb-e2e", "token_type": "bot", "bot_user_id": "UBOT", "app_id": "A1", "scope": "chat:write", "team": { "id": "T1", "name": "Acme" }, "authed_user": { "id": "UADA" } }),
                    "chat.postMessage" => json!({ "ok": true, "channel": b["channel"], "ts": ts }),
                    "chat.update" => json!({ "ok": true, "channel": b["channel"], "ts": b["ts"] }),
                    _ => json!({ "ok": true }),
                })
            }
        })
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let slack_base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, slack).await.unwrap() });
    let e = &env_with(|c| c.review.secret_key = Some("0123456789abcdef0123456789abcdef".into())).await;
    let code = e._code.clone();
    const SIGNING: &str = "e2e-signing-secret";
    let cfg = code_control::chatops::SlackConfig {
        client_id: "1.2".into(),
        client_secret: "s".into(),
        signing_secret: SIGNING.into(),
        api_base: slack_base,
        authorize_url: "https://slack.com/oauth/v2/authorize".into(),
    };
    code_control::chatops::enable(&code, Some(cfg), None).unwrap();

    // The owner connects the workspace, and has an automation (as the engine would save it).
    let (_, v) = e.req("POST", "/v1/code/agents/acme/integrations/slack/install", None).await;
    let state = v["url"].as_str().unwrap().split("state=").nth(1).unwrap().split('&').next().unwrap().to_string();
    // The callback sends the browser on to the dashboard: not followed here.
    let no_redirects = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let r = no_redirects.get(format!("{}/v1/code/chat/slack/oauth?code=c&state={state}", e.api)).send().await.unwrap();
    assert_eq!(r.status(), 303);
    assert!(r.headers()["location"].to_str().unwrap().contains("connected=Acme"), "{:?}", r.headers());
    let owner_id = code.db.owner("acme").unwrap().unwrap().id;
    let spec = json!({ "name": "/roll in Slack", "trigger": { "type": "integration", "source": "slack", "events": ["command"] }, "steps": [{ "id": "say", "kind": "notify", "config": { "title": "heard" } }] });
    let (s, v) = e.req("POST", "/v1/code/agents/acme/automations", Some(spec)).await;
    assert_eq!(s, 201, "{v}");
    let automation = v["automation"]["id"].as_str().unwrap().to_string();

    // A machine with the fake provider, and a thread whose turn waits on an approval.
    e.login("box").await;
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&e.login).unwrap()).unwrap();
    let machine = saved["machine"].as_str().unwrap().to_string();
    let path = format!("/v1/code/agents/acme/machines/{machine}/commands");
    let mut c = e.cmd(&["agent", "up"]);
    c.env("ROLL_AGENT_FAKE", "1");
    let mut daemon = c.spawn().unwrap();
    let log = lines(&mut daemon);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !e.machines().await[0]["online"].as_bool().unwrap_or(false) {
        assert!(Instant::now() < deadline, "never online");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let project = e.dir.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let (_, r) = e.req("POST", &path, Some(json!({ "id": "p1", "command": { "type": "project.add", "path": project, "name": "proj" } }))).await;
    let project_id = r["id"].as_str().unwrap_or_else(|| panic!("{r}")).to_string();
    let create = json!({ "id": "t1", "command": { "type": "thread.create", "project": project_id, "provider": "fake", "runtime_mode": "ask", "prompt": "write the demo file" } });
    let (_, r) = e.req("POST", &path, Some(create)).await;
    let thread = r["id"].as_str().unwrap_or_else(|| panic!("{r}")).to_string();
    let records = |kind: &'static str| async move {
        let (_, state) = e.req("GET", "/v1/code/agents/acme/state", None).await;
        state["entities"].as_array().unwrap().iter().filter(|m| m["entity"]["kind"] == kind).map(|m| m["entity"].clone()).collect::<Vec<_>>()
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while !records("request").await.iter().any(|r| r["status"] == "pending") {
        assert!(Instant::now() < deadline, "never asked");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The status step, as the engine runs it after `/roll` in C1.
    let step = |state: Value| {
        let s: Step = serde_json::from_value(json!({ "id": "status", "kind": "slack.status", "config": { "machine": machine, "thread": thread } })).unwrap();
        let ctx = json!({ "event": { "source": "slack", "kind": "command", "data": { "team": "T1", "channel": "C1", "thread_ts": null } }, "steps": {} });
        StepContext::new(code.clone(), OwnerRef { id: owner_id.clone(), name: "acme".into() }, AutomationId(automation.clone()), "e2e".into(), None, AutomationRunId("arn_e2e".into()), s, 1, ctx, state, code_sql::now_ms())
    };
    let kind = code.automations.registry.step("slack.status").unwrap();
    let mut ctx = step(Value::Null);
    assert!(matches!(kind.run(&mut ctx).await, StepOutcome::Wait { .. }));
    let card = calls.lock().unwrap().iter().filter(|c| c.0 == "chat.postMessage").nth(1).cloned().expect("a request card").1;
    assert!(card.to_string().contains("tee ROLL_DEMO.md"), "{card}");
    let approve = card["blocks"].as_array().unwrap().iter().filter(|b| b["type"] == "actions").flat_map(|b| b["elements"].as_array().unwrap().clone()).find(|b| b["text"]["text"] == "Approve").unwrap();

    // The click, signed as Slack signs it.
    let payload = json!({ "type": "block_actions", "team": { "id": "T1" }, "user": { "id": "UADA", "username": "ada" }, "channel": { "id": "C1" },
        "message": { "ts": "1700000000.000002" }, "trigger_id": "trig-1", "actions": [{ "type": "button", "action_id": approve["action_id"], "block_id": "roll.choices", "value": approve["value"] }] });
    let body = format!("payload={}", urlencoding::encode(&payload.to_string()));
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let r = e.http.post(format!("{}/v1/code/chat/slack/interactions", e.api))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("x-slack-request-timestamp", ts.to_string())
        .header("x-slack-signature", code_chat::slack::sign(SIGNING, ts, body.as_bytes()))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // The turn goes on and finishes; the file is written.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let runs = records("run").await;
        if runs.iter().any(|r| r["status"] == "completed") && records("thread").await[0]["status"] == "idle" {
            break;
        }
        assert!(Instant::now() < deadline, "never completed: {runs:?} {:?}", log.try_iter().collect::<Vec<_>>());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(project.join("ROLL_DEMO.md").exists());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !calls.lock().unwrap().iter().any(|c| c.0 == "chat.update" && c.1.to_string().contains("Answered by")) {
        assert!(Instant::now() < deadline, "the card says who answered");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Answering the fake's repeated ask (it asks again after the first answer) the same way.
    for r in records("request").await.into_iter().filter(|r| r["status"] == "pending") {
        let answer = json!({ "id": format!("a-{}", r["id"].as_str().unwrap()), "command": { "type": "request.respond", "request": r["id"], "response": { "type": "decision", "decision": "accept" } } });
        e.req("POST", &path, Some(answer)).await;
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let out = loop {
        let mut ctx2 = step(ctx.state().clone());
        match kind.run(&mut ctx2).await {
            StepOutcome::Done(out) => break out,
            StepOutcome::Wait { .. } => ctx = ctx2,
            other => panic!("{other:?}"),
        }
        assert!(Instant::now() < deadline, "the status never finished");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(out["status"], "done", "{out}");
    // SAFETY: signalling the child this test started.
    unsafe { libc::kill(daemon.id() as i32, libc::SIGINT) };
    let status = tokio::task::spawn_blocking(move || wait_for(&mut daemon, Duration::from_secs(10))).await.unwrap().expect("stopped");
    assert!(status.success(), "{status:?}");
}
