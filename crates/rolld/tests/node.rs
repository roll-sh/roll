//! rolld as a fleet node: products switched off with `ROLL_PRODUCTS` (no Docker needed without
//! Postgres), and `ROLL_LOG_FORMAT=json` access lines (docs/cloud/README.md, "Logs").

use serde_json::Value;
use std::io::BufRead;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    static GIVEN: Mutex<Vec<u16>> = Mutex::new(Vec::new());
    loop {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut given = GIVEN.lock().unwrap();
        if !given.contains(&port) {
            given.push(port);
            return port;
        }
    }
}

struct Rolld {
    child: Child,
    api: String,
    stdout: Arc<Mutex<Vec<String>>>,
}

impl Drop for Rolld {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// rolld on free loopback ports with nothing on PATH (no `docker`), `env` on top.
fn start(home: &std::path::Path, env: &[(&str, &str)]) -> (Rolld, u16) {
    let empty = home.join("bin");
    std::fs::create_dir_all(&empty).unwrap();
    let local = |p: u16| format!("127.0.0.1:{p}");
    let (api, proxy) = (free_port(), free_port());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rolld"));
    cmd.env_clear().env("PATH", &empty).env("RUST_LOG", "info").env("ROLL_HOME", home).env("ROLL_API", local(api)).env("ROLL_PROXY", local(proxy));
    for var in [
        "ROLL_REDIS",
        "ROLL_REDIS_HTTP",
        "ROLL_KAFKA",
        "ROLL_S3",
        "ROLL_STORAGE_API",
        "ROLL_NOSQL_CQL",
        "ROLL_NOSQL_CQL_SHARD_AWARE",
        "ROLL_NOSQL_ALTERNATOR",
        "ROLL_NOSQL_REST",
        "ROLL_QUEUES_AMQP",
        "ROLL_QUEUES_MANAGEMENT",
        "ROLL_QUEUES_STOMP",
        "ROLL_QUEUES_WEB_STOMP",
        "ROLL_QUEUES_MQTT",
        "ROLL_QUEUES_WEB_MQTT",
        "ROLL_QUEUES_STREAM",
        "ROLL_QUEUES_HTTP",
        "ROLL_SEARCH_HTTP",
    ] {
        cmd.env(var, local(free_port()));
    }
    cmd.envs(env.iter().copied()).stdout(Stdio::piped());
    let mut child = cmd.spawn().expect("rolld starts");
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let (out, lines) = (child.stdout.take().unwrap(), stdout.clone());
    std::thread::spawn(move || {
        for l in std::io::BufReader::new(out).lines().map_while(Result::ok) {
            lines.lock().unwrap().push(l);
        }
    });
    (Rolld { child, api: format!("http://127.0.0.1:{api}"), stdout }, proxy)
}

impl Rolld {
    async fn info(&mut self, http: &reqwest::Client) -> Value {
        let started = Instant::now();
        loop {
            if let Ok(r) = http.get(format!("{}/v1/info", self.api)).send().await
                && r.status().is_success()
            {
                return r.json().await.unwrap();
            }
            assert!(started.elapsed() < Duration::from_secs(60), "rolld didn't answer");
            assert!(self.child.try_wait().unwrap().is_none(), "rolld exited: {}", self.stdout.lock().unwrap().join("\n"));
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn get(&self, http: &reqwest::Client, path: &str) -> (u16, String) {
        let r = http.get(format!("{}{path}", self.api)).send().await.unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }
}

/// The fleet's setting until Postgres runs in process: every product but Postgres, with no
/// Docker on the machine. Postgres binds no port and its routes say it's off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_product_but_postgres_runs_without_docker() {
    let home = tempfile::tempdir().unwrap();
    let (mut rolld, proxy) = start(home.path(), &[("ROLL_PRODUCTS", "redis,streams,tasks,storage,nosql,queues,code,search")]);
    let http = reqwest::Client::new();
    let info = rolld.info(&http).await;
    assert_eq!(info["products"], serde_json::json!(["redis", "streams", "tasks", "storage", "nosql", "queues", "code", "search"]));
    for path in ["/v1/redis", "/v1/kafka", "/v1/tasks/queues", "/v1/storage", "/v1/nosql", "/v1/queues/overview", "/v1/code", "/v1/search"] {
        let (status, body) = rolld.get(&http, path).await;
        assert_eq!(status, 200, "{path}: {body}");
    }
    let (status, body) = rolld.get(&http, "/v1/projects").await;
    assert_eq!(status, 404);
    assert!(body.contains("postgres is off"), "{body}");
    assert!(std::net::TcpStream::connect(("127.0.0.1", proxy)).is_err(), "the Postgres proxy's port isn't bound");
}

/// JSON logs: every line is an object with `level`, `target` and `msg`; a request gets an access
/// line with its route's pattern and numeric status, duration and size; tokens, cookies, query
/// strings and the path's names appear nowhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_access_lines_have_the_route_and_no_secrets() {
    let home = tempfile::tempdir().unwrap();
    let (mut rolld, _) = start(home.path(), &[("ROLL_PRODUCTS", "code"), ("ROLL_LOG_FORMAT", "json")]);
    let http = reqwest::Client::new();
    rolld.info(&http).await;
    let r = http
        .get(format!("{}/v1/code/repos/owner-sekrit/repo-sekrit?token=query-sekrit", rolld.api))
        .header("authorization", "Bearer auth-sekrit")
        .header("cookie", "session=cookie-sekrit")
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    r.bytes().await.unwrap();
    unsafe { libc::kill(rolld.child.id() as i32, libc::SIGTERM) };
    let exited = Instant::now();
    while rolld.child.try_wait().unwrap().is_none() {
        assert!(exited.elapsed() < Duration::from_secs(30), "rolld didn't stop");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    std::thread::sleep(Duration::from_millis(200));
    let lines = rolld.stdout.lock().unwrap().clone();
    assert!(!lines.is_empty());
    for l in &lines {
        let v: Value = serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}"));
        assert!(v["level"].is_string() && v["target"].is_string() && v["msg"].is_string(), "{l}");
        assert!(!l.contains("sekrit"), "a secret or tenant name in the logs: {l}");
    }
    let access: Vec<Value> = lines.iter().map(|l| serde_json::from_str::<Value>(l).unwrap()).filter(|v| v["target"] == "access" && v["route"] == "/v1/code/repos/{owner}/{repo}").collect();
    assert_eq!(access.len(), 1, "{lines:#?}");
    let a = &access[0];
    assert_eq!((a["method"].as_str(), a["status"].as_u64(), a["product"].as_str(), a["level"].as_str()), (Some("GET"), Some(status as u64), Some("code"), Some("INFO")));
    assert!(a["duration_ms"].is_number() && a["ttfb_ms"].is_number() && a["bytes"].is_u64(), "{a}");
    assert_eq!(a["_aws"]["CloudWatchMetrics"][0]["Namespace"], "RollCloud");
}
