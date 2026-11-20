//! roll secrets' links to the products rolld serves (docs/secrets.md, "Links"): the real rolld with
//! each product in process. For every product a key is linked to a resource on the project's
//! canvas, the value it resolves to works with the product's own client (or is the product's own
//! credential), and a rotation is followed: a live link reads the product's new password, a managed
//! one gets a new credential and the old one stops working once its retired lease is revoked.
//!
//! Postgres runs its computes in Docker, so it's linked only with `ROLL_SECRETS_DOCKER=1`
//! (`pnpm test:secrets --docker`) and skipped otherwise.
//!
//! And a sealed start (`ROLL_SECRET_KEY_SEALED`): nothing that needs the deployment key runs until
//! its shares unseal the node.

use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TOKEN: &str = "secrets-links-test-operator-token-0123456789";

/// A port nothing listens on, never handed out twice in this process.
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

struct Rolld {
    child: Child,
    home: Arc<tempfile::TempDir>,
    api: String,
    /// roll search's HTTP address.
    search: String,
    http: reqwest::Client,
    docker: bool,
}

impl Drop for Rolld {
    fn drop(&mut self) {
        // SAFETY: kill(2) on the rolld this test started; a graceful stop takes its computes down.
        unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
        let stopping = Instant::now();
        while self.child.try_wait().ok().flatten().is_none() && stopping.elapsed() < Duration::from_secs(15) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if self.docker {
            let label = format!("label=roll.home={}", self.home.path().display());
            if let Ok(out) = Command::new("docker").args(["ps", "-aq", "--filter", &label]).output() {
                let ids: Vec<String> = String::from_utf8_lossy(&out.stdout).split_whitespace().map(String::from).collect();
                if !ids.is_empty() {
                    let _ = Command::new("docker").args(["rm", "-f"]).args(&ids).output();
                }
            }
        }
    }
}

impl Rolld {
    /// rolld with `products` on free loopback ports and an operator token (so the products' own
    /// credentials, not the operator's, decide their data planes).
    async fn start(products: &str, docker: bool) -> Rolld {
        Rolld::start_in(Arc::new(tempfile::tempdir().unwrap()), products, docker, &[], "/v1/info").await
    }

    /// [`Rolld::start`] in `home` with `env` set, started once `ready` answers a success.
    async fn start_in(home: Arc<tempfile::TempDir>, products: &str, docker: bool, env: &[(&str, &str)], ready: &str) -> Rolld {
        let local = |p: u16| format!("127.0.0.1:{p}");
        let api = free_port();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rolld"));
        cmd.env_clear().env("RUST_LOG", "warn").env("ROLL_HOME", home.path()).env("ROLL_API", local(api)).env("ROLL_PROXY", local(free_port()));
        cmd.env("ROLL_ADMIN_TOKEN", TOKEN).env("ROLL_PRODUCTS", products).envs(env.iter().copied());
        // Without Docker, nothing on PATH: no product may need it.
        if docker {
            cmd.env("PATH", std::env::var("PATH").unwrap_or_default());
        } else {
            let empty = home.path().join("bin");
            std::fs::create_dir_all(&empty).unwrap();
            cmd.env("PATH", empty);
        }
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
        ] {
            cmd.env(var, local(free_port()));
        }
        let search = local(free_port());
        cmd.env("ROLL_SEARCH_HTTP", &search);
        let child = cmd.stdout(Stdio::null()).spawn().expect("rolld starts");
        let mut r = Rolld { child, home, api: format!("http://127.0.0.1:{api}"), search, http: reqwest::Client::new(), docker };
        r.wait_for(ready).await;
        r
    }

    /// Until `path` answers a success.
    async fn wait_for(&mut self, path: &str) {
        let started = Instant::now();
        loop {
            if let Ok(a) = self.http.get(format!("{}{path}", self.api)).bearer_auth(TOKEN).send().await
                && a.status().is_success()
            {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(60), "rolld didn't answer {path}");
            assert!(self.child.try_wait().unwrap().is_none(), "rolld exited");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// A route as the operator; anything but a success fails the test.
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let mut rb = self.http.request(method.clone(), format!("{}{path}", self.api)).bearer_auth(TOKEN);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let a = rb.send().await.unwrap();
        let status = a.status();
        let text = a.text().await.unwrap();
        assert!(status.is_success(), "{method} {path}: {status} {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    async fn get(&self, path: &str) -> Value {
        self.call(reqwest::Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }
}

/// A workspace project and its production environment in roll secrets.
struct Project<'a> {
    rolld: &'a Rolld,
    id: String,
}

impl Project<'_> {
    async fn new(rolld: &Rolld) -> Project<'_> {
        let p = rolld.post("/v1/workspace/projects", json!({ "name": "shop" })).await;
        Project { rolld, id: p["id"].as_str().unwrap().to_string() }
    }

    /// Puts the resource on the project's canvas and links `(key, field, mode)` to it.
    async fn link(&self, kind: &str, r#ref: &str, keys: &[(&str, &str, &str)]) {
        self.rolld.call(reqwest::Method::PUT, &format!("/v1/workspace/projects/{}/resources/{kind}/{ref}", self.id), Some(json!({ "x": 0, "y": 0 }))).await;
        let ops: Vec<Value> = keys.iter().map(|(key, field, mode)| json!({ "op": "set", "key": key, "link": { "kind": kind, "ref": r#ref, "field": field, "mode": mode } })).collect();
        self.rolld.post(&format!("/v1/secrets/projects/{}/environments/production/changes", self.id), json!({ "ops": ops })).await;
    }

    async fn value(&self, key: &str) -> String {
        let v = self.rolld.get(&format!("/v1/secrets/projects/{}/environments/production/values?path=/", self.id)).await;
        v["values"][key].as_str().unwrap_or_else(|| panic!("{key} didn't resolve: {v}")).to_string()
    }

    async fn rotate(&self, key: &str) {
        self.rolld.post(&format!("/v1/secrets/projects/{}/environments/production/secrets/{key}/rotate", self.id), json!({})).await;
    }

    /// Revokes the rotated-out credentials still waiting out their grace period now (the list keeps
    /// revoked leases, and ending one again is a 409).
    async fn revoke_retired(&self) {
        let leases = self.rolld.get(&format!("/v1/secrets/projects/{}/leases", self.id)).await;
        let retired: Vec<&str> = leases.as_array().unwrap().iter().filter(|l| l["kind"] == "retired" && l["revoked_at"].is_null()).map(|l| l["id"].as_str().unwrap()).collect();
        assert!(!retired.is_empty(), "a rotation retires the old credential: {leases}");
        for id in retired {
            self.rolld.call(reqwest::Method::DELETE, &format!("/v1/secrets/projects/{}/leases/{id}", self.id), None).await;
        }
    }
}

/// What `AUTH` with the URL's user and password answers on the URL's host (`+OK` when it logs in).
async fn redis_auth(url: &str) -> String {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let u = url::Url::parse(url).unwrap();
    let password = u.password().unwrap();
    let mut conn = tokio::net::TcpStream::connect((u.host_str().unwrap(), u.port().unwrap())).await.unwrap();
    let arg = |s: &str| format!("${}\r\n{s}\r\n", s.len());
    conn.write_all(format!("*3\r\n{}{}{}", arg("AUTH"), arg(u.username()), arg(password)).as_bytes()).await.unwrap();
    let mut line = String::new();
    tokio::io::BufReader::new(conn).read_line(&mut line).await.unwrap();
    line.trim_end().to_string()
}

async fn cql_logs_in(contact: &str, user: &str, password: &str) -> bool {
    use nosql_proto::client::{Client, ClientConfig};
    match Client::connect(ClientConfig::new(contact).credentials(user, password)).await {
        Ok(c) => c.query("SELECT release_version FROM system.local", nosql_proto::QueryParams::default()).await.is_ok(),
        Err(_) => false,
    }
}

async fn s3_puts(endpoint: &str, region: &str, key_id: &str, secret: &str) -> bool {
    use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
    let conf = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(region.to_string()))
        .endpoint_url(endpoint)
        .force_path_style(true)
        .credentials_provider(Credentials::new(key_id, secret, None, None, "test"))
        .retry_config(aws_sdk_s3::config::retry::RetryConfig::disabled())
        .build();
    let put = aws_sdk_s3::Client::from_conf(conf).put_object().bucket("assets").key("k").body(b"v".to_vec().into()).send().await;
    put.is_ok()
}

/// One test, one rolld: every product's link and rotation, one after the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn links_resolve_to_working_credentials_and_follow_rotations_in_every_product() {
    let docker = std::env::var("ROLL_SECRETS_DOCKER").is_ok_and(|v| v == "1");
    let products = if docker { "postgres,redis,streams,storage,nosql,search" } else { "redis,streams,storage,nosql,search" };
    let r = Rolld::start(products, docker).await;
    let p = Project::new(&r).await;

    // Streams (live bootstrap, a managed API key): the key reads the stream over HTTP until it's
    // rotated out and revoked.
    let st = r.post("/v1/streams", json!({ "name": "events" })).await;
    let sid = st["id"].as_str().unwrap();
    r.post(&format!("/v1/streams/{sid}/topics"), json!({ "name": "orders", "partitions": 1 })).await;
    p.link("stream", sid, &[("KAFKA_BROKERS", "bootstrap", "live"), ("KAFKA_USERNAME", "username", "managed"), ("KAFKA_PASSWORD", "password", "managed")]).await;
    assert_eq!(p.value("KAFKA_BROKERS").await, r.get(&format!("/v1/streams/{sid}")).await["bootstrap"].as_str().unwrap());
    let reads = |user: String, password: String| {
        let req = r.http.get(format!("{}/v1/streams/{sid}/topics/orders/records", r.api)).basic_auth(user, Some(password));
        async move { req.send().await.unwrap().status().as_u16() }
    };
    let old = (p.value("KAFKA_USERNAME").await, p.value("KAFKA_PASSWORD").await);
    assert_eq!(reads(old.0.clone(), old.1.clone()).await, 200);
    p.rotate("KAFKA_PASSWORD").await;
    let new = (p.value("KAFKA_USERNAME").await, p.value("KAFKA_PASSWORD").await);
    assert_ne!(new.0, old.0, "both keys of the pair rotate together");
    p.revoke_retired().await;
    assert_eq!((reads(new.0, new.1).await, reads(old.0, old.1).await), (200, 401));

    // Buckets (live endpoint and region, a managed access key limited to the bucket).
    r.post("/v1/storage/buckets", json!({ "name": "assets" })).await;
    p.link(
        "bucket",
        "assets",
        &[("AWS_ENDPOINT_URL", "endpoint", "live"), ("AWS_REGION", "region", "live"), ("AWS_ACCESS_KEY_ID", "access_key_id", "managed"), ("AWS_SECRET_ACCESS_KEY", "secret_access_key", "managed")],
    )
    .await;
    let (endpoint, region) = (p.value("AWS_ENDPOINT_URL").await, p.value("AWS_REGION").await);
    assert_eq!(endpoint, r.get("/v1/storage").await["endpoint"].as_str().unwrap());
    let old = (p.value("AWS_ACCESS_KEY_ID").await, p.value("AWS_SECRET_ACCESS_KEY").await);
    assert!(s3_puts(&endpoint, &region, &old.0, &old.1).await, "the linked key writes the bucket");
    p.rotate("AWS_SECRET_ACCESS_KEY").await;
    let new = (p.value("AWS_ACCESS_KEY_ID").await, p.value("AWS_SECRET_ACCESS_KEY").await);
    assert_ne!(new.0, old.0);
    p.revoke_retired().await;
    assert!(s3_puts(&endpoint, &region, &new.0, &new.1).await && !s3_puts(&endpoint, &region, &old.0, &old.1).await, "the new key writes and the revoked one doesn't");

    // NoSQL (live): the database's own credentials log in over CQL; rotating resets the password.
    let db = r.post("/v1/nosql/databases", json!({ "name": "app" })).await;
    let id = db["id"].as_str().unwrap();
    p.link("nosql", id, &[("CASSANDRA_CONTACT_POINTS", "cql", "live"), ("CASSANDRA_USERNAME", "username", "live"), ("CASSANDRA_PASSWORD", "password", "live")]).await;
    let (cql, user, password) = (p.value("CASSANDRA_CONTACT_POINTS").await, p.value("CASSANDRA_USERNAME").await, p.value("CASSANDRA_PASSWORD").await);
    assert_eq!(password, db["credentials"]["password"].as_str().unwrap());
    assert!(cql_logs_in(&cql, &user, &password).await, "the linked credentials log in");
    p.rotate("CASSANDRA_PASSWORD").await;
    let rotated = p.value("CASSANDRA_PASSWORD").await;
    assert_ne!(rotated, password);
    assert!(cql_logs_in(&cql, &user, &rotated).await && !cql_logs_in(&cql, &user, &password).await, "the new password logs in and the old one doesn't");

    // Search (live URL, a managed data API key; never the cluster's password).
    let sc = r.post("/v1/search/clusters", json!({ "name": "logs" })).await;
    let id = sc["id"].as_str().unwrap();
    p.link("search", id, &[("ELASTICSEARCH_URL", "url", "live"), ("ELASTICSEARCH_API_KEY", "api_key", "managed")]).await;
    let url = p.value("ELASTICSEARCH_URL").await;
    assert_eq!(url, sc["url"].as_str().unwrap());
    let searches = |key: String| {
        let req = r.http.post(format!("{url}/_search")).header("authorization", format!("ApiKey {key}")).json(&json!({}));
        async move { req.send().await.unwrap().status().as_u16() }
    };
    let old = p.value("ELASTICSEARCH_API_KEY").await;
    assert_eq!(searches(old.clone()).await, 200);
    p.rotate("ELASTICSEARCH_API_KEY").await;
    let new = p.value("ELASTICSEARCH_API_KEY").await;
    assert_ne!(new, old);
    p.revoke_retired().await;
    assert_eq!((searches(new).await, searches(old).await), (200, 401));

    // Redis (live): the database's own URL logs in; rotating it resets the password and the
    // link reads the new one.
    let db = r.post("/v1/redis/databases", json!({ "name": "cache" })).await;
    let id = db["id"].as_str().unwrap();
    p.link("redis", id, &[("REDIS_URL", "url", "live")]).await;
    let url = p.value("REDIS_URL").await;
    assert_eq!(url, r.get(&format!("/v1/redis/databases/{id}")).await["redis_url"].as_str().unwrap());
    assert_eq!(redis_auth(&url).await, "+OK", "the linked URL logs in");
    p.rotate("REDIS_URL").await;
    let rotated = p.value("REDIS_URL").await;
    assert_ne!(rotated, url);
    assert_eq!(redis_auth(&rotated).await, "+OK", "the new password logs in");
    assert_ne!(redis_auth(&url).await, "+OK", "the old one doesn't");

    if !docker {
        eprintln!("postgres: skipped (its computes run in Docker; ROLL_SECRETS_DOCKER=1 links it)");
        return;
    }
    // Postgres (the branch's live URL, a managed role of its own): both connect; a rotated-out
    // role can't once it's revoked.
    let pg = r.post("/v1/projects", json!({ "name": "shopdb" })).await;
    let pid = pg["id"].as_str().unwrap();
    p.link("postgres", pid, &[("DATABASE_URL", "url", "live"), ("APP_DATABASE_URL", "role_url", "managed")]).await;
    let url = p.value("DATABASE_URL").await;
    let branches = r.get(&format!("/v1/projects/{pid}")).await["branches"].clone();
    let main = branches.as_array().unwrap().iter().find(|b| b["name"] == "main").unwrap();
    assert_eq!(url, main["connection_uri"].as_str().unwrap());
    assert!(pg_connects(&url, Duration::from_secs(90)).await, "the branch's URL connects");
    let old = p.value("APP_DATABASE_URL").await;
    assert!(pg_connects(&old, Duration::from_secs(30)).await, "the role's URL connects");
    p.rotate("APP_DATABASE_URL").await;
    let new = p.value("APP_DATABASE_URL").await;
    assert_ne!(new, old);
    p.revoke_retired().await;
    assert!(pg_connects(&new, Duration::from_secs(30)).await && !pg_connects(&old, Duration::ZERO).await, "the new role connects and the dropped one doesn't");
}

/// Whether `SELECT 1` runs over `url` within `patience` (a compute may still be starting).
async fn pg_connects(url: &str, patience: Duration) -> bool {
    let started = Instant::now();
    loop {
        if let Ok((client, conn)) = tokio_postgres::connect(url, tokio_postgres::NoTls).await {
            tokio::spawn(conn);
            if client.simple_query("SELECT 1").await.is_ok() {
                return true;
            }
        }
        if started.elapsed() >= patience {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// A sealed start inside rolld (docs/secrets.md, "Starting sealed"): until a threshold of the
/// deployment key's shares unseal it, values aren't read, roll search (which seals its clusters'
/// secrets with the key) doesn't start, and roll secrets' sweeper (expiries, leases, rotations)
/// doesn't run; once they do, every one of them works with the key the shares gave.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sealed_rolld_runs_nothing_that_needs_the_key_until_its_shares_unseal_it() {
    const KEY: &str = "fake-deployment-key-for-the-sealed-rolld-test-00000";
    let home = Arc::new(tempfile::tempdir().unwrap());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    // First with the key itself: a value, and a preview's environment that expires tomorrow.
    let r = Rolld::start_in(home.clone(), "search", false, &[("ROLL_SECRET_KEY", KEY)], "/v1/info").await;
    let project = Project::new(&r).await.id;
    let envs = format!("/v1/secrets/projects/{project}/environments");
    r.post(&format!("{envs}/production/changes"), json!({ "ops": [{ "op": "set", "key": "FAKE_API_KEY", "value": "fake-value-0001" }] })).await;
    r.post(&envs, json!({ "slug": "pr-1", "kind": "ephemeral", "base": "staging", "expires_at": now + 86_400_000 })).await;
    drop(r);
    // Its expiry passes while the node is down: the sweeper's first pass deletes it.
    let db = home.path().join("secrets.db");
    let expiry = || rusqlite::Connection::open(&db).unwrap().query_row("SELECT expires_at FROM environments WHERE slug = 'pr-1'", [], |r| r.get::<_, i64>(0)).ok();
    rusqlite::Connection::open(&db).unwrap().execute("UPDATE environments SET expires_at = 1 WHERE slug = 'pr-1'", []).unwrap();

    let shares = roll_keyshares::split(KEY.as_bytes(), 3, 2).unwrap();
    let set = roll_keyshares::Share::parse(1, &shares[0]).unwrap().set;
    let sealed = format!("{}:2", set.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let mut r = Rolld::start_in(home.clone(), "search", false, &[("ROLL_SECRET_KEY_SEALED", &sealed)], "/v1/secrets/unseal").await;
    let values = format!("{}{envs}/production/values?path=/", r.api);
    let refused = || async {
        let a = r.http.get(&values).bearer_auth(TOKEN).send().await.unwrap();
        (a.status().as_u16(), a.json::<Value>().await.unwrap()["sealed"].clone())
    };
    let unseal = |share: String| {
        let req = r.http.post(format!("{}/v1/secrets/unseal", r.api)).json(&json!({ "share": share }));
        async move { req.send().await.unwrap().json::<Value>().await.unwrap() }
    };
    assert_eq!(refused().await, (503, json!(true)));
    assert!(tokio::net::TcpStream::connect(&r.search).await.is_err(), "roll search started sealed");
    // Long enough for the sweeper's first pass, which runs as roll secrets starts.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(expiry(), Some(1), "the sweeper ran sealed");
    assert_eq!(unseal(shares[0].clone()).await, json!({ "sealed": true, "have": 1, "need": 2 }));
    assert_eq!(refused().await, (503, json!(true)), "one share of two unsealed it");

    assert_eq!(unseal(shares[2].clone()).await, json!({ "sealed": false, "have": 2, "need": 2 }));
    r.wait_for("/v1/info").await;
    assert_eq!(Project { rolld: &r, id: project.clone() }.value("FAKE_API_KEY").await, "fake-value-0001");
    assert!(tokio::net::TcpStream::connect(&r.search).await.is_ok(), "roll search didn't start once unsealed");
    let started = Instant::now();
    while expiry().is_some() {
        assert!(started.elapsed() < Duration::from_secs(30), "the sweeper didn't run once unsealed");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
