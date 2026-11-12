//! roll search on a cluster of two real `rolld` processes (docs/search/architecture.md, "On the
//! fleet"): leases in Postgres through roll code's cluster, the search catalog in the same
//! system database, segments and the WAL in S3 (MinIO). Writes through either node's REST port
//! land on the cluster's one owner; SIGTERM on the owner hands the cluster over with every
//! acknowledged document; the restarted node serves it again. Skipped unless `ROLL_E2E=1`.

use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const MINIO_IMAGE: &str = "cgr.dev/chainguard/minio:latest@sha256:a1ac4589a5f8136b616e667b48a0bea9719bb10bd45799ea946f24bbedfafde4";
const MC_IMAGE: &str = "cgr.dev/chainguard/minio-client:latest@sha256:10388d580a724f356b94c9b2631b5a179039cabe8c76e9b37d86ed007d569b05";
const POSTGRES_IMAGE: &str = "postgres:18-alpine@sha256:123b17998ccaf47e53b502e8c6a1e79afd8f579c6f3eec83c563da026f957395";
const SECRET: &str = "search-e2e-cluster-secret-0123456789";
const S3_USER: &str = "rolle2e";
const S3_PASS: &str = "rolle2e-secret";

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker").args(args).output().expect("docker runs");
    assert!(out.status.success(), "docker {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Containers named for this run, removed on drop.
struct Containers(Vec<String>);

impl Drop for Containers {
    fn drop(&mut self) {
        for c in &self.0 {
            let _ = Command::new("docker").args(["rm", "-f", "-v", c]).output();
        }
    }
}

struct Node {
    child: Child,
    api: String,
    rest: String,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Env {
    pg: String,
    s3: String,
    home: tempfile::TempDir,
}

impl Env {
    fn start(&self, id: &str, api: u16, rest: u16) -> Node {
        let home = self.home.path().join(id);
        let child = Command::new(env!("CARGO_BIN_EXE_rolld"))
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("RUST_LOG", "warn")
            .env("ROLL_PRODUCTS", "code,search")
            .env("ROLL_CLUSTER_SECRET", SECRET)
            .env("ROLL_SYSTEM_DATABASE", &self.pg)
            .env("ROLL_NODE_ID", id)
            .env("ROLL_ADVERTISE_URL", format!("http://127.0.0.1:{api}"))
            .env("ROLL_DRAIN_TIMEOUT", "20")
            .env("AWS_ACCESS_KEY_ID", S3_USER)
            .env("AWS_SECRET_ACCESS_KEY", S3_PASS)
            .env("AWS_REGION", "us-east-1")
            .args(["--home", home.to_str().unwrap(), "--api", &format!("127.0.0.1:{api}")])
            .args(["--s3-bucket", "roll", "--s3-endpoint", &self.s3, "--s3-prefix", "search-e2e"])
            .args(["--search-http", &format!("127.0.0.1:{rest}")])
            .stdout(Stdio::null())
            .spawn()
            .expect("rolld starts");
        Node { child, api: format!("http://127.0.0.1:{api}"), rest: format!("http://127.0.0.1:{rest}") }
    }
}

async fn up(http: &reqwest::Client, n: &Node) {
    let t = Instant::now();
    while !http.get(format!("{}/v1/search", n.api)).send().await.is_ok_and(|r| r.status().is_success()) {
        assert!(t.elapsed() < Duration::from_secs(120), "{} never answered", n.api);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn es(http: &reqwest::Client, n: &Node, auth: (&str, &str), method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut r = http.request(method, format!("{}{path}", n.rest)).basic_auth(auth.0, Some(auth.1));
    if let Some(b) = body {
        r = r.json(&b);
    }
    let r = r.send().await.unwrap();
    let s = r.status().as_u16();
    (s, r.json().await.unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cluster_moves_between_rolld_nodes_with_every_write() {
    if std::env::var("ROLL_E2E").as_deref() != Ok("1") {
        eprintln!("skipped: set ROLL_E2E=1 (Docker: Postgres and MinIO)");
        return;
    }
    let run = format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() & 0xffff_ffff);
    let (pg_name, s3_name) = (format!("roll-search-e2e-pg-{run}"), format!("roll-search-e2e-s3-{run}"));
    let _containers = Containers(vec![pg_name.clone(), s3_name.clone()]);
    docker(&["run", "-d", "--name", &pg_name, "-m", "1g", "-e", "POSTGRES_PASSWORD=roll", "-p", "127.0.0.1::5432", POSTGRES_IMAGE]);
    let (u, p) = (format!("MINIO_ROOT_USER={S3_USER}"), format!("MINIO_ROOT_PASSWORD={S3_PASS}"));
    docker(&["run", "-d", "--name", &s3_name, "-m", "1g", "-e", &u, "-e", &p, "-p", "127.0.0.1::9000", MINIO_IMAGE, "server", "/data"]);
    let port = |c: &str, p: &str| docker(&["port", c, p]).lines().next().unwrap().rsplit(':').next().unwrap().to_string();
    let env = Env { pg: format!("postgres://postgres:roll@127.0.0.1:{}/postgres?sslmode=disable", port(&pg_name, "5432/tcp")), s3: format!("http://127.0.0.1:{}", port(&s3_name, "9000/tcp")), home: tempfile::tempdir().unwrap() };
    let mc_host = format!("MC_HOST_local=http://{S3_USER}:{S3_PASS}@127.0.0.1:9000");
    let beside = format!("container:{s3_name}");
    let t = Instant::now();
    while !Command::new("docker").args(["run", "--rm", "--network", &beside, "-e", &mc_host, MC_IMAGE, "mb", "--ignore-existing", "local/roll"]).output().is_ok_and(|o| o.status.success()) {
        assert!(t.elapsed() < Duration::from_secs(60), "MinIO never made the bucket");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    while !Command::new("docker").args(["exec", &pg_name, "pg_isready", "-U", "postgres"]).output().is_ok_and(|o| o.status.success()) {
        assert!(t.elapsed() < Duration::from_secs(90), "Postgres never started");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).build().unwrap();
    let ports = [(free_port(), free_port()), (free_port(), free_port())];
    let mut nodes = vec![env.start("n1", ports[0].0, ports[0].1), env.start("n2", ports[1].0, ports[1].1)];
    for n in &nodes {
        up(&http, n).await;
    }
    let (a, b) = (&nodes[0], &nodes[1]);

    let c: Value = http.post(format!("{}/v1/search/clusters", a.api)).json(&json!({"name": "shop"})).send().await.unwrap().json().await.unwrap();
    let pw = c["password"].as_str().unwrap_or_else(|| panic!("{c}")).to_string();
    let auth = ("shop", pw.as_str());
    for i in 0..20 {
        let n = if i % 2 == 0 { a } else { b };
        let (s, v) = es(&http, n, auth, reqwest::Method::PUT, &format!("/items/_doc/{i}"), Some(json!({"n": i}))).await;
        assert!(s == 201, "write {i} through {}: {s} {v}", n.rest);
    }
    let (s, v) = es(&http, b, auth, reqwest::Method::POST, "/items/_refresh", None).await;
    assert_eq!(s, 200, "{v}");
    let (_, v) = es(&http, a, auth, reqwest::Method::GET, "/items/_count", None).await;
    assert_eq!(v["count"], 20, "{v}");

    // SIGTERM to whichever node owns it: the other takes over with every write.
    let shown: Value = http.get(format!("{}/v1/search/clusters/shop", a.api)).send().await.unwrap().json().await.unwrap();
    let owner = shown["status"]["node"].as_str().unwrap_or_else(|| panic!("no owner in {shown}")).to_string();
    let (stop, keep) = if owner == "n1" { (0, 1) } else { (1, 0) };
    let _ = Command::new("kill").args(["-TERM", &nodes[stop].child.id().to_string()]).status();
    let t = Instant::now();
    loop {
        let (s, v) = es(&http, &nodes[keep], auth, reqwest::Method::GET, "/items/_count", None).await;
        if s == 200 {
            assert_eq!(v["count"], 20, "{v}");
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(60), "the other node never served it: {s} {v}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let _ = nodes[stop].child.wait();

    // The stopped node comes back and serves it again (wherever it's owned now).
    let id = if stop == 0 { "n1" } else { "n2" };
    nodes[stop] = env.start(id, ports[stop].0, ports[stop].1);
    up(&http, &nodes[stop]).await;
    let (s, v) = es(&http, &nodes[stop], auth, reqwest::Method::GET, "/items/_count", None).await;
    assert_eq!((s, v["count"].as_i64()), (200, Some(20)), "{v}");
}
