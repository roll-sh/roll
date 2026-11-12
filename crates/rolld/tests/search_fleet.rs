//! `rolld --search-only` fleet end to end, with real processes: two nodes sharing a catalog
//! (SQLite fleet metadata) and object storage (a directory), the front proxy in process. A
//! SIGTERM hands the owner's clusters to the other node while writes keep flowing through the
//! proxy (none fails, none is lost); a SIGKILL of the next owner fails over to a freshly started
//! node once the lease expires, with every acknowledged write recovered from the WAL. Nodes A
//! and C serve TLS with boot-time certificates (`--search-tls-self-signed`) and B plain HTTP:
//! the proxy reaches each the way its record says, so a fleet can switch over node by node.
//! A TLS node serves its control API with the same certificate: the cluster is created through
//! the proxy's API port, which reaches A over pinned TLS.

use search_proxy::proxy::{ListenOptions, Port};
use search_proxy::{Directory, Proxy, ProxyConfig};
use serde_json::{Value, json};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Node {
    id: String,
    child: Child,
    api: String,
    http: std::net::SocketAddr,
}

impl Node {
    fn start(id: &str, dir: &Path, tls: bool) -> Node {
        let (http, api) = (free_port(), free_port());
        let home = dir.join(id);
        let log = std::fs::File::create(dir.join(format!("{id}.log"))).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_rolld"))
            .args(["--search-only", "--home"])
            .arg(&home)
            .args(["--api", &format!("127.0.0.1:{api}"), "--search-http", &format!("127.0.0.1:{http}")])
            .arg("--search-meta")
            .arg(dir.join("fleet.db"))
            .arg("--search-storage-dir")
            .arg(dir.join("objects"))
            .args(["--search-node-id", id, "--search-az", "az1", "--search-gc-interval", "0", "--search-shutdown-timeout", "20"])
            .args(tls.then_some("--search-tls-self-signed"))
            .env("RUST_LOG", "info,search_server=debug")
            .env_remove("ROLL_SEARCH_META")
            .env_remove("ECS_CONTAINER_METADATA_URI_V4")
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .stdin(Stdio::null())
            .spawn()
            .expect("start rolld");
        let scheme = if tls { "https" } else { "http" };
        Node { id: id.to_string(), child, api: format!("{scheme}://127.0.0.1:{api}"), http: std::net::SocketAddr::from(([127, 0, 0, 1], http)) }
    }

    fn signal(&self, sig: i32) {
        // SAFETY: sends a signal to a child process this test started.
        assert_eq!(unsafe { libc::kill(self.child.id() as i32, sig) }, 0);
    }

    /// Waits for the process to exit; its exit status.
    async fn wait(&mut self, within: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + within;
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s;
            }
            assert!(Instant::now() < deadline, "{} didn't exit", self.id);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn until(what: &str, within: Duration, mut f: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !f().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn put(http: &reqwest::Client, url: &str, user: &str, pw: &str, path: &str, body: Value) -> (u16, Value) {
    let r = http.put(format!("{url}{path}")).basic_auth(user, Some(pw)).json(&body).send().await.unwrap();
    let s = r.status().as_u16();
    (s, r.json().await.unwrap_or(Value::Null))
}

/// The node holding `cluster`'s lease now (search-server's SQLite lease table).
fn owner(dir: &Path, cluster: &str) -> Option<String> {
    let c = rusqlite::Connection::open(dir.join("fleet.db")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    c.query_row("SELECT node FROM search_leases WHERE cluster = ?1 AND expires_ms > ?2", rusqlite::params![cluster, now], |r| r.get::<_, String>(0)).ok()
}

#[tokio::test]
async fn sigterm_hands_over_and_sigkill_fails_over() {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("rolld-search-fleet-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut a = Node::start("node-a", &dir, true);
    let mut b = Node::start("node-b", &dir, false);
    // Direct calls to a TLS node's API skip verification; the proxy pins.
    let http = reqwest::Client::builder().danger_accept_invalid_certs(true).build().unwrap();
    // The fleet store exists once a node is up; the proxy follows it.
    until("the nodes' API", Duration::from_secs(60), async || http.get(format!("{}/v1/search", a.api)).send().await.is_ok() && http.get(format!("{}/v1/search", b.api)).send().await.is_ok()).await;
    let members = search_proxy::fleet::open(&dir.join("fleet.db").display().to_string(), None, None).await.unwrap();
    let d = Directory::new(members, ProxyConfig { node_ttl: Duration::from_secs(3), refresh: Duration::from_millis(100), hold_timeout: Duration::from_secs(60), ..ProxyConfig::default() });
    until("both nodes to heartbeat", Duration::from_secs(10), async || {
        d.refresh().await.unwrap();
        d.live().len() == 2
    })
    .await;
    assert!(d.tls(&a.http) && !d.tls(&b.http), "A publishes a certificate pin, B none");
    let a_api: std::net::SocketAddr = a.api.trim_start_matches("https://").parse().unwrap();
    assert!(d.tls(&a_api), "A's API serves TLS with the same certificate");
    // B serves its API in plain HTTP, so the fleet's API calls aren't all TLS yet: A's API
    // answers plain HTTP too (what nodes from before api_tls call it with) until no such node
    // is left (checked at the end).
    let plain = http.get(format!("http://{a_api}/v1/search")).send().await.unwrap();
    assert!(plain.status().is_success(), "A's API answers plain HTTP beside a plain node: {plain:?}");
    d.spawn_refresh();
    let proxy = Proxy::new(d.clone(), None);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let al = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let api_url = format!("http://{}", al.local_addr().unwrap());
    let (_stop, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(proxy.clone().serve(l, ListenOptions::plain(Port::Rest), rx.clone()));
    tokio::spawn(proxy.clone().serve(al, ListenOptions::plain(Port::Api), rx));

    // Created through the proxy's API port (any node takes it, over TLS or plain).
    let c: Value = http.post(format!("{api_url}/v1/search/clusters")).json(&json!({ "name": "shop" })).send().await.unwrap().json().await.unwrap();
    let (user, pw) = (c["username"].as_str().unwrap().to_string(), c["password"].as_str().unwrap().to_string());
    assert_eq!(put(&http, &url, &user, &pw, "/docs", json!({})).await.0, 200);
    let first = owner(&dir, "shop").expect("an owner");

    // Writers through the proxy while the owner gets SIGTERM.
    let acked = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..4)
        .map(|w| {
            let (http, url, user, pw, acked, failures, stop) = (http.clone(), url.clone(), user.clone(), pw.clone(), acked.clone(), failures.clone(), stop.clone());
            tokio::spawn(async move {
                let mut i = 0;
                while !stop.load(Ordering::SeqCst) {
                    let (s, v) = put(&http, &url, &user, &pw, &format!("/docs/_doc/{w}-{i}"), json!({ "w": w })).await;
                    if s == 201 {
                        acked.fetch_add(1, Ordering::SeqCst);
                    } else {
                        failures.lock().push((s, v));
                    }
                    i += 1;
                }
            })
        })
        .collect();
    // Writes acknowledged before, and after, the handoff (counts, not sleeps: a loaded machine
    // with slow fsyncs acknowledges few writes per second).
    let acked_at_least = async |n: usize| {
        let deadline = Instant::now() + Duration::from_secs(30);
        while acked.load(Ordering::SeqCst) < n && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    acked_at_least(8).await;
    let (term, other) = if first == a.id { (&mut a, &mut b) } else { (&mut b, &mut a) };
    term.signal(libc::SIGTERM);
    let status = term.wait(Duration::from_secs(30)).await;
    assert!(status.success(), "a drained node exits cleanly: {status}");
    acked_at_least(acked.load(Ordering::SeqCst) + 8).await;
    stop.store(true, Ordering::SeqCst);
    for w in writers {
        w.await.unwrap();
    }
    assert!(failures.lock().is_empty(), "no write failed across the handoff: {:?}", failures.lock());
    let n_acked = acked.load(Ordering::SeqCst);
    assert!(n_acked > 10, "{n_acked}");
    assert_eq!(owner(&dir, "shop").as_deref(), Some(other.id.as_str()));
    let count = async |http: &reqwest::Client| -> i64 {
        let _ = http.post(format!("{url}/docs/_refresh")).basic_auth(&user, Some(&pw)).send().await.unwrap();
        let v: Value = http.get(format!("{url}/docs/_count")).basic_auth(&user, Some(&pw)).send().await.unwrap().json().await.unwrap();
        v["count"].as_i64().unwrap_or(-1)
    };
    assert_eq!(count(&http).await, n_acked as i64);

    // More writes, unrefreshed, then the owner is killed. A new node takes over once the
    // lease expires and replays the WAL.
    for i in 0..20 {
        assert_eq!(put(&http, &url, &user, &pw, &format!("/docs/_doc/late-{i}"), json!({ "late": true })).await.0, 201);
    }
    other.signal(libc::SIGKILL);
    other.wait(Duration::from_secs(10)).await;
    let mut c = Node::start("node-c", &dir, true);
    until("node-c's API", Duration::from_secs(60), async || http.get(format!("{}/v1/search", c.api)).send().await.is_ok()).await;
    let t = Instant::now();
    assert_eq!(count(&http).await, n_acked as i64 + 20);
    assert!(t.elapsed() < Duration::from_secs(30), "failover took {:?}", t.elapsed());
    assert_eq!(owner(&dir, "shop").as_deref(), Some("node-c"));
    // Alone in the fleet (the others left, or their records aged out), C's API is TLS only.
    let c_api = c.api.trim_start_matches("https://").to_string();
    until("node-c's API to refuse plain HTTP", Duration::from_secs(30), async || {
        let fresh = reqwest::Client::builder().pool_max_idle_per_host(0).build().unwrap();
        fresh.get(format!("http://{c_api}/v1/search")).send().await.is_err()
    })
    .await;
    assert!(http.get(format!("{}/v1/search", c.api)).send().await.unwrap().status().is_success(), "and TLS still works");
    c.signal(libc::SIGTERM);
    assert!(c.wait(Duration::from_secs(30)).await.success());
    // Leaving removes the node record.
    d.refresh().await.unwrap();
    assert!(d.live().is_empty(), "{:?}", d.live());
    let _ = std::fs::remove_dir_all(&dir);
}
