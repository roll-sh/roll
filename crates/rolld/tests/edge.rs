//! rolld with the edge on (`ROLL_EDGE=1`, dev-mode certificates): HTTPS routed by Host to the
//! API, tasks, code's git routes and the dashboard; redis-cli over TLS with the database named by
//! SNI; and a stop that fails `/healthz` at once while the listeners keep serving.
//!
//! redis-cli runs in Docker (`redis:8-alpine`); without Docker that part is skipped.

use serde_json::{Value, json};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};

fn free_port() -> u16 {
    static GIVEN: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
    loop {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut given = GIVEN.lock().unwrap();
        if !given.contains(&port) {
            given.push(port);
            return port;
        }
    }
}

struct Rolld(Child);

impl Drop for Rolld {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One HTTPS request through the edge: (status, body).
async fn https(port: u16, roots: &RootCertStore, host: &str, path: &str) -> (u16, String) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap().with_root_certificates(roots.clone()).with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut tls = tokio_rustls::TlsConnector::from(Arc::new(cfg)).connect(ServerName::try_from(host.to_string()).unwrap(), tcp).await.unwrap();
    tls.write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tls.read_to_end(&mut out).await;
    let text = String::from_utf8_lossy(&out).to_string();
    let status = text.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    (status, text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default())
}

async fn healthz(http: &reqwest::Client, api: &str) -> u16 {
    http.get(format!("{api}/healthz")).send().await.map_or(0, |r| r.status().as_u16())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edge_routes_by_host_and_sni_and_drains_on_sigterm() {
    let home = tempfile::tempdir().unwrap();
    let certs = home.path().join("certs");
    // The dashboard, standing in for the web app's container.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dashboard = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, axum::Router::new().fallback(|| async { "dashboard" })).await });

    let (api, https_port, edge_redis) = (free_port(), free_port(), free_port());
    let local = |p: u16| format!("127.0.0.1:{p}");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rolld"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("RUST_LOG", "warn")
        .env("ROLL_EDGE", "1")
        .env("ROLL_EDGE_DOMAIN", "roll.test")
        .env("ROLL_EDGE_SITE_DOMAIN", "rollsite.test")
        .env("ROLL_EDGE_REGION", "r1")
        .env("ROLL_EDGE_CERT_DIR", &certs)
        .env("ROLL_EDGE_DRAIN_DELAY", "3")
        .env("ROLL_DASHBOARD_UPSTREAM", &dashboard)
        .env("ROLL_EDGE_HTTPS", local(https_port))
        .env("ROLL_EDGE_REDIS", local(edge_redis));
    for var in ["ROLL_EDGE_PG", "ROLL_EDGE_KAFKA", "ROLL_EDGE_NOSQL", "ROLL_EDGE_AMQPS", "ROLL_EDGE_MQTTS", "ROLL_EDGE_STOMPS", "ROLL_EDGE_MQ_STREAM"] {
        cmd.env(var, local(free_port()));
    }
    cmd.args(["--no-queues", "--no-nosql", "--home"]).arg(home.path()).args(["--api", &local(api)]);
    for (flag, port) in [("--proxy", free_port()), ("--redis", free_port()), ("--redis-http", free_port()), ("--kafka", free_port()), ("--s3", free_port()), ("--storage-api", free_port())] {
        cmd.args([flag, &local(port)]);
    }
    let mut rolld = Rolld(cmd.spawn().expect("rolld starts"));
    let api = format!("http://127.0.0.1:{api}");
    let http = reqwest::Client::new();
    let started = Instant::now();
    while healthz(&http, &api).await != 200 {
        assert!(started.elapsed() < Duration::from_secs(60), "rolld didn't become healthy");
        assert!(rolld.0.try_wait().unwrap().is_none(), "rolld exited");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut roots = RootCertStore::empty();
    for c in CertificateDer::pem_file_iter(certs.join("edge.crt")).unwrap() {
        roots.add(c.unwrap()).unwrap();
    }

    // Host picks the router: the API, tasks, code's git routes, the dashboard.
    let (s, body) = https(https_port, &roots, "api.roll.test", "/v1/redis").await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(https(https_port, &roots, "tasks.r1.roll.test", "/v1/redis").await.0, 404, "tasks has no Redis API");
    assert_eq!(https(https_port, &roots, "tasks.r1.roll.test", "/v1/tasks/queues").await.0, 200);
    assert_eq!(https(https_port, &roots, "roll.test", "/dashboard").await.1, "dashboard");
    let (s, body) = https(https_port, &roots, "roll.test", "/acme/app.git/info/refs?service=git-upload-pack").await;
    assert!(s == 404 && body != "dashboard", "git paths go to code: {s} {body}");

    // redis-cli over TLS: the server name picks the database.
    let docker = Command::new("docker").arg("info").output().is_ok_and(|o| o.status.success());
    if docker {
        let db: Value = http.post(format!("{api}/v1/redis/databases")).json(&json!({ "name": "cache" })).send().await.unwrap().json().await.unwrap();
        let (id, password) = db["rest_token"].as_str().unwrap().rsplit_once('_').unwrap();
        let sni = format!("{id}.redis.r1.roll.test");
        let cli = |args: &[&str]| {
            let port = edge_redis.to_string();
            let mut a = vec!["run", "--rm", "--network", "host", "redis:8-alpine", "redis-cli", "--tls", "--insecure", "--sni", &sni, "-h", "127.0.0.1", "-p", &port, "--no-auth-warning", "-a", password];
            a.extend_from_slice(args);
            let out = Command::new("docker").args(&a).output().unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string() + &String::from_utf8_lossy(&out.stderr)
        };
        assert_eq!(cli(&["SET", "k", "through the edge"]), "OK");
        assert_eq!(cli(&["GET", "k"]), "through the edge");
    } else {
        eprintln!("redis-cli part skipped: no Docker");
    }

    // SIGTERM: /healthz fails at once; the listeners keep serving for the drain delay; then it exits.
    let stopped = Instant::now();
    assert!(Command::new("kill").args(["-TERM", &rolld.0.id().to_string()]).status().unwrap().success());
    while healthz(&http, &api).await == 200 {
        assert!(stopped.elapsed() < Duration::from_secs(1), "healthz still 200 after SIGTERM");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(healthz(&http, &api).await, 503);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(https(https_port, &roots, "api.roll.test", "/v1/redis").await.0, 200, "still serving while the NLB lets go");
    loop {
        if rolld.0.try_wait().unwrap().is_some() {
            break;
        }
        assert!(stopped.elapsed() < Duration::from_secs(60), "rolld didn't exit");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(stopped.elapsed() >= Duration::from_secs(3), "exited before the drain delay: {:?}", stopped.elapsed());
}
