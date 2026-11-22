//! rolld as a fleet node: products switched off with `ROLL_PRODUCTS` (no Docker needed without
//! Postgres), and `ROLL_LOG_FORMAT=json` access lines (docs/cloud/README.md, "Logs").

mod common;

use serde_json::Value;
use std::io::BufRead;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Rolld {
    child: Child,
    started: common::Started,
    api: String,
    stdout: Arc<Mutex<Vec<String>>>,
}

impl Drop for Rolld {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// rolld on loopback ports of its own with nothing on PATH (no `docker`), `env` on top, and
/// where it listens.
async fn start(home: &std::path::Path, env: &[(&str, &str)]) -> (Rolld, common::Addrs) {
    let empty = home.join("bin");
    std::fs::create_dir_all(&empty).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rolld"));
    cmd.env_clear().env("PATH", &empty).env("RUST_LOG", "info").env("ROLL_HOME", home);
    let started = common::on_any_port(&mut cmd, common::LISTENERS, home);
    cmd.envs(env.iter().copied()).stdout(Stdio::piped());
    let mut child = cmd.spawn().expect("rolld starts");
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let (out, lines) = (child.stdout.take().unwrap(), stdout.clone());
    std::thread::spawn(move || {
        for l in std::io::BufReader::new(out).lines().map_while(Result::ok) {
            lines.lock().unwrap().push(l);
        }
    });
    let addrs = started.addrs(&mut child).await;
    (Rolld { child, started, api: addrs.url("api"), stdout }, addrs)
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
            assert!(self.child.try_wait().unwrap().is_none(), "rolld exited: {}\n{}", self.stdout.lock().unwrap().join("\n"), self.started.tail());
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
    let (mut rolld, addrs) = start(home.path(), &[("ROLL_PRODUCTS", "redis,streams,tasks,storage,nosql,queues,code,search")]).await;
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
    assert_eq!(addrs.get("proxy"), None, "the Postgres proxy doesn't listen");
}

/// JSON logs: every line is an object with `level`, `target` and `msg`; a request gets an access
/// line with its route's pattern and numeric status, duration and size; tokens, cookies, query
/// strings and the path's names appear nowhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_access_lines_have_the_route_and_no_secrets() {
    let home = tempfile::tempdir().unwrap();
    let (mut rolld, _) = start(home.path(), &[("ROLL_PRODUCTS", "code"), ("ROLL_LOG_FORMAT", "json")]).await;
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

/// A mock OpenAI embeddings server on a free loopback port: every request's `Authorization`.
async fn mock_openai() -> (String, Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let (mut buf, mut chunk) = (Vec::new(), [0u8; 8192]);
                let head = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break String::from_utf8_lossy(&buf[..i]).to_string();
                    }
                };
                let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                while buf.len() < head.len() + 4 + len {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let auth = head.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("authorization")).map(|(_, v)| v.trim().to_string()));
                log.lock().unwrap().push(auth.unwrap_or_default());
                let out = r#"{"object":"list","data":[{"object":"embedding","index":0,"embedding":[0.6,0.8]}],"model":"m"}"#;
                let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}", out.len());
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}"), seen)
}

/// An inference endpoint keyed by a roll secret of the cluster's project: created with the
/// secret's value, and once the secret is rotated, roll secrets pushes the new value and the
/// endpoint calls the provider with it. The value is never in an answer or a log line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_inference_endpoint_keyed_by_a_secret_follows_its_rotation() {
    let home = tempfile::tempdir().unwrap();
    let (mock, seen) = mock_openai().await;
    let env = [
        ("ROLL_PRODUCTS", "search"),
        ("ROLL_SECRET_KEY", "node-test-secret-key-0123456789abcdef"),
        ("ROLL_SEARCH_INFERENCE_ALLOW_PRIVATE", "true"),
    ];
    let (mut rolld, _) = start(home.path(), &env).await;
    let http = reqwest::Client::new();
    rolld.info(&http).await;
    let api = rolld.api.clone();
    let send = |m: reqwest::Method, path: &str, body: Value| {
        let r = http.request(m, format!("{api}{path}")).json(&body);
        async move {
            let r = r.send().await.unwrap();
            let status = r.status().as_u16();
            (status, r.json::<Value>().await.unwrap_or(Value::Null))
        }
    };
    use reqwest::Method;
    use serde_json::json;
    let (s, cluster) = send(Method::POST, "/v1/search/clusters", json!({ "name": "emb" })).await;
    assert!(s < 300, "{cluster}");
    let cluster = cluster["id"].as_str().unwrap().to_string();
    let (s, project) = send(Method::POST, "/v1/workspace/projects", json!({ "name": "shop" })).await;
    assert!(s < 300, "{project}");
    let project = project["id"].as_str().unwrap().to_string();
    let (s, b) = send(Method::PUT, &format!("/v1/workspace/projects/{project}/resources/search/{cluster}"), json!({ "x": 0, "y": 0 })).await;
    assert!(s < 300, "{b}");
    let key = format!("/v1/secrets/projects/{project}/environments/production/secrets/OPENAI_API_KEY");
    let (s, b) = send(Method::PUT, &key, json!({ "value": "sk-first-value" })).await;
    assert!(s < 300, "{b}");

    let made = json!({
        "inference_id": "emb",
        "task_type": "text_embedding",
        "config": { "service": "openai", "service_settings": { "model_id": "m", "url": format!("{mock}/v1/embeddings") } },
        "secret": { "environment": "production", "key": "OPENAI_API_KEY" },
    });
    let (s, b) = send(Method::POST, &format!("/v1/search/clusters/{cluster}/inference/secrets"), made).await;
    assert_eq!(s, 201, "{b}");
    let sync = b["binding"]["sync"].as_str().unwrap().to_string();
    assert!(!b.to_string().contains("sk-first"), "{b}");
    assert_eq!(seen.lock().unwrap().last().map(String::as_str), Some("Bearer sk-first-value"), "created with the secret's value");

    // Rotated: the sweeper pushes the new value, which the endpoint then uses. The provider sees it
    // when `_update` validates it, before the endpoint stores it: wait for the push to finish.
    let rotated_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    let (s, b) = send(Method::PUT, &key, json!({ "value": "sk-second-value" })).await;
    assert!(s < 300, "{b}");
    let started = Instant::now();
    loop {
        let (_, b) = send(Method::GET, &format!("/v1/search/clusters/{cluster}/inference/secrets"), Value::Null).await;
        let pushed = b["bindings"][0]["status"] == "ok" && b["bindings"][0]["last_synced_at"].as_u64().is_some_and(|t| t > rotated_at);
        if pushed && seen.lock().unwrap().iter().any(|a| a == "Bearer sk-second-value") {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(30), "the new value never reached the endpoint: {b}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let infer = json!({ "method": "POST", "path": "/_inference/text_embedding/emb", "body": { "input": "hello" } });
    let (s, b) = send(Method::POST, &format!("/v1/search/clusters/{cluster}/request"), infer).await;
    assert_eq!((s, b["status"].as_u64()), (200, Some(200)), "{b}");
    let calls = seen.lock().unwrap().clone();
    let (_, b) = send(Method::GET, &format!("/v1/search/clusters/{cluster}/inference/secrets"), Value::Null).await;
    assert_eq!(calls.last().map(String::as_str), Some("Bearer sk-second-value"), "{calls:?} {b}");
    assert_eq!((b["bindings"][0]["key"].as_str(), b["bindings"][0]["status"].as_str()), (Some("OPENAI_API_KEY"), Some("ok")), "{b}");
    let (_, b) = send(Method::POST, &format!("/v1/search/clusters/{cluster}/request"), json!({ "method": "GET", "path": "/_inference/emb" })).await;
    assert!(!b.to_string().contains("sk-"), "{b}");

    let es = |method: &str, path: &str, body: Value| json!({ "method": method, "path": path, "body": body });
    let mine = |a: &String| a.starts_with("Bearer sk-") && !a.contains("sk-other");

    // Deleted, kept for restoring, and its name taken by another cluster with an endpoint of the
    // same id: a rotation goes nowhere, and the sync says so.
    let (s, b) = send(Method::DELETE, &format!("/v1/search/clusters/{cluster}"), Value::Null).await;
    assert!(s < 300, "{b}");
    let (s, b) = send(Method::POST, "/v1/search/clusters", json!({ "name": "emb" })).await;
    assert!(s < 300, "{b}");
    let other = b["id"].as_str().unwrap().to_string();
    let theirs = json!({ "service": "openai", "service_settings": { "api_key": "sk-other", "model_id": "m", "url": format!("{mock}/v1/embeddings") } });
    let (_, b) = send(Method::POST, &format!("/v1/search/clusters/{other}/request"), es("PUT", "/_inference/text_embedding/emb", theirs)).await;
    assert_eq!(b["status"].as_u64(), Some(200), "{b}");
    let before = seen.lock().unwrap().iter().filter(|a| mine(a)).count();
    let (s, b) = send(Method::PUT, &key, json!({ "value": "sk-while-deleted" })).await;
    assert!(s < 300, "{b}");
    let syncs = format!("/v1/secrets/projects/{project}/syncs");
    let started = Instant::now();
    loop {
        let (_, b) = send(Method::GET, &syncs, Value::Null).await;
        let row = b["syncs"].as_array().and_then(|a| a.iter().find(|r| r["id"] == sync.as_str())).cloned().unwrap_or(Value::Null);
        if row["status"] == "error" {
            assert!(row["error"].as_str().is_some_and(|e| e.contains(&cluster) && e.contains("restored")), "{row}");
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(30), "the push to a deleted cluster never failed: {row}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(seen.lock().unwrap().iter().filter(|a| mine(a)).count(), before, "a key went out while its cluster was deleted");

    // Restored under another name: the sync follows the id, so the key reaches it, never the
    // cluster that took the old name.
    let (s, b) = send(Method::POST, &format!("/v1/search/clusters/{cluster}/restore"), json!({ "name": "emb-back" })).await;
    assert!(s < 300, "{b}");
    let (s, b) = send(Method::POST, &format!("{syncs}/{sync}/run"), Value::Null).await;
    assert_eq!((s, b["status"].as_str()), (200, Some("ok")), "{b}");
    let infer = json!({ "method": "POST", "path": "/_inference/text_embedding/emb", "body": { "input": "hello" } });
    let (_, b) = send(Method::POST, &format!("/v1/search/clusters/{other}/request"), infer.clone()).await;
    assert_eq!(b["status"].as_u64(), Some(200), "{b}");
    assert_eq!(seen.lock().unwrap().last().map(String::as_str), Some("Bearer sk-other"), "the cluster that took the name got the key");
    let (_, b) = send(Method::POST, &format!("/v1/search/clusters/{cluster}/request"), infer).await;
    assert_eq!(b["status"].as_u64(), Some(200), "{b}");
    assert_eq!(seen.lock().unwrap().last().map(String::as_str), Some("Bearer sk-while-deleted"), "the restored cluster's endpoint");

    // Recreated by someone who may change the cluster but not reveal the key: same id, service and
    // URL, one header more. The next rotation isn't sent there.
    let (_, b) = send(Method::POST, &format!("/v1/search/clusters/{cluster}/request"), es("DELETE", "/_inference/text_embedding/emb", Value::Null)).await;
    assert_eq!(b["status"].as_u64(), Some(200), "{b}");
    let again = json!({ "service": "openai", "service_settings": { "api_key": "sk-their-own", "model_id": "m", "url": format!("{mock}/v1/embeddings") }, "task_settings": { "headers": { "X-Route": "theirs" } } });
    let (_, b) = send(Method::POST, &format!("/v1/search/clusters/{cluster}/request"), es("PUT", "/_inference/text_embedding/emb", again)).await;
    assert_eq!(b["status"].as_u64(), Some(200), "{b}");
    let (s, b) = send(Method::PUT, &key, json!({ "value": "sk-third-value" })).await;
    assert!(s < 300, "{b}");
    let started = Instant::now();
    loop {
        let (_, b) = send(Method::GET, &format!("/v1/search/clusters/{cluster}/inference/secrets"), Value::Null).await;
        if b["bindings"][0]["status"] == "error" {
            assert!(b["bindings"][0]["error"].as_str().is_some_and(|e| e.contains("settings changed")), "{b}");
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(30), "the push never refused: {b}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(!seen.lock().unwrap().iter().any(|a| a.contains("sk-third")), "the rotated key reached a recreated endpoint");
    assert!(!rolld.stdout.lock().unwrap().iter().any(|l| ["sk-first", "sk-second", "sk-while-deleted", "sk-third"].iter().any(|v| l.contains(v))), "a value in rolld's log");
}
