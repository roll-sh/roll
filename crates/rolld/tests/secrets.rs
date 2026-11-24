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
//!
//! And roll apps (`ForApps`): what a service got from roll secrets is shown only as roll secrets
//! would show it to whoever asks.
//!
//! And reach (`ReachHolders`): the services that read a folder, and the ones an environment's delete
//! would break.

mod common;

use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TOKEN: &str = "secrets-links-test-operator-token-0123456789";

struct Rolld {
    child: Child,
    started: common::Started,
    home: Arc<tempfile::TempDir>,
    api: String,
    /// roll search's HTTP address.
    search: String,
    http: reqwest::Client,
    docker: bool,
}

impl Drop for Rolld {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("rolld's stderr, last lines:\n{}", self.started.tail());
        }
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
    /// rolld with `products` on loopback ports of its own and an operator token (so the products' own
    /// credentials, not the operator's, decide their data planes).
    async fn start(products: &str, docker: bool) -> Rolld {
        Rolld::start_in(Arc::new(tempfile::tempdir().unwrap()), products, docker, &[], "/v1/info").await
    }

    /// [`Rolld::start`] in `home` with `env` set, started once `ready` answers a success.
    async fn start_in(home: Arc<tempfile::TempDir>, products: &str, docker: bool, env: &[(&str, &str)], ready: &str) -> Rolld {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rolld"));
        cmd.env_clear().env("RUST_LOG", "warn").env("ROLL_HOME", home.path());
        cmd.env("ROLL_ADMIN_TOKEN", TOKEN).env("ROLL_PRODUCTS", products).envs(env.iter().copied());
        // Without Docker, nothing on PATH: no product may need it.
        if docker {
            cmd.env("PATH", std::env::var("PATH").unwrap_or_default());
        } else {
            let empty = home.path().join("bin");
            std::fs::create_dir_all(&empty).unwrap();
            cmd.env("PATH", empty);
        }
        let started = common::on_any_port(&mut cmd, common::LISTENERS, home.path());
        let mut child = cmd.stdout(Stdio::null()).spawn().expect("rolld starts");
        // A sealed start writes it too: the API answers the unseal, the rest come once unsealed.
        let addrs = started.addrs(&mut child).await;
        let (api, search) = (addrs.url("api"), addrs.addr("search-http"));
        let mut r = Rolld { child, started, home, api, search, http: reqwest::Client::new(), docker };
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
    // A link's `names` and `permission: read` narrow its key: it reads those indices only.
    let (owner, password) = (sc["username"].as_str().unwrap(), sc["password"].as_str().unwrap());
    for index in ["logs-app", "audit"] {
        let put = r.http.put(format!("{url}/{index}/_doc/1?refresh=true")).basic_auth(owner, Some(password)).json(&json!({ "m": "hi" })).send().await.unwrap();
        assert_eq!(put.status().as_u16(), 201);
    }
    let read_link = json!({ "kind": "search", "ref": id, "field": "api_key", "mode": "managed", "options": { "names": ["logs-app"], "permission": "read" } });
    r.post(&format!("/v1/secrets/projects/{}/environments/production/changes", p.id), json!({ "ops": [{ "op": "set", "key": "SEARCH_READ_KEY", "link": read_link }] })).await;
    let reader = p.value("SEARCH_READ_KEY").await;
    let send = |m: reqwest::Method, path: &str| {
        let req = r.http.request(m, format!("{url}{path}")).header("authorization", format!("ApiKey {reader}")).json(&json!({}));
        async move { req.send().await.unwrap().status().as_u16() }
    };
    assert_eq!(send(reqwest::Method::POST, "/logs-app/_search").await, 200);
    assert_eq!(send(reqwest::Method::PUT, "/logs-app/_doc/2").await, 403);
    assert_eq!(send(reqwest::Method::POST, "/audit/_search").await, 403);

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

/// rolld wires roll secrets into the workspace's deletes: a project holding secrets is refused
/// until a secrets admin confirms, then its secrets wait out the operator's window, unreadable,
/// and come back with the workspace project.
#[tokio::test]
async fn a_workspace_delete_asks_roll_secrets_first_and_keeps_the_secrets_for_its_window() {
    const KEY: &str = "fake-deployment-key-for-the-delete-window-test-0000";
    let r = Rolld::start_in(Arc::new(tempfile::tempdir().unwrap()), "search", false, &[("ROLL_SECRET_KEY", KEY), ("ROLL_SECRETS_DELETE_WINDOW_DAYS", "3")], "/v1/info").await;
    let p = Project::new(&r).await;
    r.call(reqwest::Method::PUT, &format!("/v1/secrets/projects/{}/environments/production/secrets/API_KEY", p.id), Some(json!({ "value": "kept-through-the-window" }))).await;
    let send = |method: reqwest::Method, path: String| r.http.request(method, format!("{}{path}", r.api)).bearer_auth(TOKEN).send();
    let a = send(reqwest::Method::DELETE, format!("/v1/workspace/projects/{}", p.id)).await.unwrap();
    assert_eq!(a.status(), 409);
    assert_eq!(a.json::<Value>().await.unwrap()["code"], "project_holds_secrets");
    let a = send(reqwest::Method::DELETE, format!("/v1/workspace/projects/{}?confirm=shop", p.id)).await.unwrap();
    assert_eq!(a.status(), 204);
    let a = send(reqwest::Method::GET, format!("/v1/secrets/projects/{}/environments/production/values?path=/", p.id)).await.unwrap();
    assert_eq!(a.status(), 404, "read by nobody in the window");
    let deleted = r.get("/v1/secrets/deleted").await;
    assert_eq!(deleted[0]["purge_at"].as_i64().unwrap() - deleted[0]["deleted_at"].as_i64().unwrap(), 3 * 86_400_000, "{deleted}");
    r.post(&format!("/v1/secrets/projects/{}/recover", p.id), json!({})).await;
    assert_eq!(p.value("API_KEY").await, "kept-through-the-window");
}

/// roll apps shows a service's values from roll secrets only as roll secrets would show them to
/// the asker: a token that may reveal staging, but not production, gets a 403 for staging's
/// values and shell once a staging key references production (the service reads it), and can't
/// write such a reference itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apps_shows_a_staging_reference_to_production_only_to_who_may_reveal_production() {
    // With search: the harness reads every rolld's search address.
    let r = Rolld::start("apps,search", false).await;
    let project = Project::new(&r).await.id;
    let secrets = format!("/v1/secrets/projects/{project}/environments");
    r.call(reqwest::Method::PUT, &format!("{secrets}/production/secrets/DB_PASSWORD"), Some(json!({ "value": "fake-production-password" }))).await;
    r.call(reqwest::Method::PUT, &format!("{secrets}/staging/secrets/LOG_LEVEL"), Some(json!({ "value": "debug" }))).await;
    let svc = r.post("/v1/apps/services", json!({ "project": project, "name": "web", "source": { "kind": "upload" } })).await;
    let staging = format!("/v1/apps/services/{}/environments/staging", svc["id"].as_str().unwrap());
    r.call(reqwest::Method::PUT, &staging, Some(json!({}))).await;
    let rules = json!([
        { "product": "secrets", "resources": [{ "project": project }], "environments": ["staging"], "actions": { "read": "allow", "write": "allow", "reveal": "allow" } },
        { "product": "apps", "resources": "*", "actions": { "read": "allow", "write": "allow", "reveal": "allow", "admin": "allow" } },
    ]);
    let token = r.post("/v1/access/tokens", json!({ "name": "staging", "rules": rules })).await["token"].as_str().unwrap().to_string();
    let as_token = |method: reqwest::Method, path: String, body: Option<Value>| {
        let mut rb = r.http.request(method, format!("{}{path}", r.api)).bearer_auth(&token);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        async move {
            let a = rb.send().await.unwrap();
            (a.status().as_u16(), a.text().await.unwrap())
        }
    };
    let values = format!("{staging}/variables/values");
    let (s, v) = as_token(reqwest::Method::GET, values.clone(), None).await;
    assert!(s == 200 && v.contains("debug"), "staging's own values: {s} {v}");

    // It can't make staging read production itself.
    let reference = json!({ "value": "${production.DB_PASSWORD}" });
    let (s, e) = as_token(reqwest::Method::PUT, format!("{secrets}/staging/secrets/DB_URL"), Some(reference.clone())).await;
    assert_eq!(s, 403, "{e}");
    // Someone who may reveal production does: the service reads production's value.
    r.call(reqwest::Method::PUT, &format!("{secrets}/staging/secrets/DB_URL"), Some(reference)).await;
    assert_eq!(r.get(&values).await["DB_URL"], "fake-production-password");
    // The token sees none of it, in the values or in a shell.
    let (s, e) = as_token(reqwest::Method::GET, values, None).await;
    assert!(s == 403 && e.contains("DB_URL") && !e.contains("fake-production-password"), "{s} {e}");
    let (s, e) = as_token(reqwest::Method::POST, format!("{staging}/shell/sessions"), Some(json!({}))).await;
    assert!(s == 403 && e.contains("roll secrets"), "{s} {e}");
}

/// A roll apps log drain into roll search, end to end: rolld makes the drain's data stream (roll
/// apps' template, the stream, its retention) through search's console as the operator, mints a
/// key that may only append to that stream through the search link's provider, and the drain's
/// batch lands there as an ECS document through `_bulk`; the key reads and deletes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_apps_drain_appends_to_its_own_search_data_stream() {
    let r = Rolld::start("apps,search", false).await;
    let p = Project::new(&r).await;
    let sc = r.post("/v1/search/clusters", json!({ "name": "logs" })).await;
    let id = sc["id"].as_str().unwrap();
    r.call(reqwest::Method::PUT, &format!("/v1/workspace/projects/{}/resources/search/{id}", p.id), Some(json!({ "x": 0, "y": 0 }))).await;
    let drains = format!("/v1/apps/projects/{}/drains", p.id);
    let d = r.post(&drains, json!({ "search": id, "namespace": "web", "retention_days": 7 })).await;
    assert_eq!(d["stream"], "logs-roll.apps-web", "{d}");
    let t = r.post(&format!("{drains}/{}/test", d["id"].as_str().unwrap()), json!({})).await;
    assert_eq!(t["ok"], true, "{t}");
    let (url, owner, password) = (sc["url"].as_str().unwrap(), sc["username"].as_str().unwrap(), sc["password"].as_str().unwrap());
    let as_owner = |m: reqwest::Method, path: &str| r.http.request(m, format!("{url}{path}")).basic_auth(owner, Some(password));
    as_owner(reqwest::Method::POST, "/logs-roll.apps-web/_refresh").send().await.unwrap();
    let found: Value = as_owner(reqwest::Method::POST, "/logs-roll.apps-web/_search").json(&json!({})).send().await.unwrap().json().await.unwrap();
    let doc = &found["hits"]["hits"][0]["_source"];
    assert!(doc["@timestamp"].is_string() && doc["event"]["dataset"] == "roll.apps", "an ECS document: {found}");
    let life: Value = as_owner(reqwest::Method::GET, "/_data_stream/logs-roll.apps-web/_lifecycle").send().await.unwrap().json().await.unwrap();
    assert_eq!(life["data_streams"][0]["lifecycle"]["data_retention"], "7d", "{life}");
    // Its key (never shown) is listed live, and revoked with the drain.
    let name = format!("roll apps drain:{}", p.id);
    let drain_keys = || {
        let (req, name) = (as_owner(reqwest::Method::GET, "/_security/api_key"), name.clone());
        async move {
            let v: Value = req.send().await.unwrap().json().await.unwrap();
            v["api_keys"].as_array().unwrap_or_else(|| panic!("{v}")).iter().filter(|k| k["name"] == name.as_str()).map(|k| k["invalidated"] == true).collect::<Vec<_>>()
        }
    };
    assert_eq!(drain_keys().await, [false], "the drain's key");
    r.call(reqwest::Method::DELETE, &format!("{drains}/{}", d["id"].as_str().unwrap()), None).await;
    assert!(drain_keys().await.iter().all(|&invalidated| invalidated), "the deleted drain's key still works");
}

/// An apps service's search link with `indices` and `access: read` (roll secrets' search link
/// provider, as `names` and `permission`) gets a key that reads those indices only, and
/// unlinking revokes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_apps_search_link_is_narrowed_and_unlinking_revokes_it() {
    let r = Rolld::start("apps,search", false).await;
    let p = Project::new(&r).await;
    let sc = r.post("/v1/search/clusters", json!({ "name": "logs" })).await;
    let id = sc["id"].as_str().unwrap();
    r.call(reqwest::Method::PUT, &format!("/v1/workspace/projects/{}/resources/search/{id}", p.id), Some(json!({ "x": 0, "y": 0 }))).await;
    let (url, owner, password) = (sc["url"].as_str().unwrap(), sc["username"].as_str().unwrap(), sc["password"].as_str().unwrap());
    for index in ["logs-app", "audit"] {
        let put = r.http.put(format!("{url}/{index}/_doc/1?refresh=true")).basic_auth(owner, Some(password)).json(&json!({ "m": "hi" })).send().await.unwrap();
        assert_eq!(put.status().as_u16(), 201);
    }
    let svc = r.post("/v1/apps/services", json!({ "project": p.id, "name": "web", "source": { "kind": "upload" } })).await;
    let svc = svc["id"].as_str().unwrap();
    r.call(reqwest::Method::PUT, &format!("/v1/apps/services/{svc}/environments/production"), Some(json!({}))).await;
    let link = format!("/v1/apps/services/{svc}/links/search/{id}");
    let l = r.call(reqwest::Method::PUT, &link, Some(json!({ "indices": ["logs-app"], "access": "read" }))).await;
    assert_eq!((&l["indices"], &l["access"]), (&json!(["logs-app"]), &json!("read")), "{l}");
    let vars = r.get(&format!("/v1/apps/services/{svc}/environments/production/variables/values")).await;
    let key = vars["ELASTICSEARCH_API_KEY"].as_str().unwrap_or_else(|| panic!("{vars}")).to_string();
    let send = |m: reqwest::Method, path: &str| {
        let req = r.http.request(m, format!("{url}{path}")).header("authorization", format!("ApiKey {key}")).json(&json!({}));
        async move { req.send().await.unwrap().status().as_u16() }
    };
    assert_eq!(send(reqwest::Method::POST, "/logs-app/_search").await, 200);
    assert_eq!(send(reqwest::Method::PUT, "/logs-app/_doc/2").await, 403);
    assert_eq!(send(reqwest::Method::POST, "/audit/_search").await, 403);
    r.call(reqwest::Method::DELETE, &link, None).await;
    assert_eq!(send(reqwest::Method::POST, "/logs-app/_search").await, 401, "unlinked, its key is revoked");
}

/// A snapshot repository on roll storage whose credentials are `roll://` references (#361): a
/// rotation of the keys they name is followed (snapshots keep working once the old key is
/// revoked), and a branch of the cluster, an untrusted copy, reads a repository with staging's key
/// but is refused production's (protected), without the key in the answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repository_key_follows_rotation_and_a_branch_gets_no_protected_key() {
    let env = [("ROLL_SECRET_KEY", "fake-deployment-key-for-the-repository-test-00000"), ("ROLL_SEARCH_REPOSITORY_ALLOW_PRIVATE", "1")];
    let r = Rolld::start_in(Arc::new(tempfile::tempdir().unwrap()), "storage,search", false, &env, "/v1/info").await;
    let p = Project::new(&r).await;
    r.post("/v1/storage/buckets", json!({ "name": "backups" })).await;
    p.link("bucket", "backups", &[("REPO_REGION", "region", "live"), ("REPO_KEY_ID", "access_key_id", "managed"), ("REPO_SECRET", "secret_access_key", "managed")]).await;
    let sc = r.post("/v1/search/clusters", json!({ "name": "data" })).await;
    let id = sc["id"].as_str().unwrap();
    r.call(reqwest::Method::PUT, &format!("/v1/workspace/projects/{}/resources/search/{id}", p.id), Some(json!({ "x": 0, "y": 0 }))).await;
    let settings = json!({
        "bucket": "backups",
        "endpoint": r.get("/v1/storage").await["endpoint"],
        "path_style_access": true,
        "region": p.value("REPO_REGION").await,
        "access_key": "roll://production/REPO_KEY_ID",
        "secret_key": "roll://production/REPO_SECRET",
    });
    r.call(reqwest::Method::PUT, &format!("/v1/search/clusters/{id}/repositories/backups"), Some(json!({ "type": "s3", "settings": settings }))).await;
    let url = sc["url"].as_str().unwrap();
    let as_user = |user: String, password: String, path: String, body: Value| {
        let req = r.http.put(format!("{url}{path}")).basic_auth(user, Some(password)).json(&body);
        async move {
            let a = req.send().await.unwrap();
            (a.status().as_u16(), a.text().await.unwrap())
        }
    };
    let owner = (sc["username"].as_str().unwrap().to_string(), sc["password"].as_str().unwrap().to_string());
    let (s, t) = as_user(owner.0.clone(), owner.1.clone(), "/notes/_doc/1?refresh=true".into(), json!({ "m": "kept" })).await;
    assert_eq!(s, 201, "{t}");
    let snapshot = |user: (String, String), repo: &str, name: &str| as_user(user.0, user.1, format!("/_snapshot/{repo}/{name}?wait_for_completion=true"), json!({}));
    let (s, t) = snapshot(owner.clone(), "backups", "s1").await;
    assert!(s == 200 && t.contains("\"SUCCESS\""), "{s} {t}");
    let old = p.value("REPO_SECRET").await;
    p.rotate("REPO_SECRET").await;
    p.revoke_retired().await;
    assert_ne!(p.value("REPO_SECRET").await, old);
    let (s, t) = snapshot(owner.clone(), "backups", "s2").await;
    assert!(s == 200 && t.contains("\"SUCCESS\""), "after the rotation: {s} {t}");
    // The same, with staging's own key (staging isn't protected).
    let ops: Vec<Value> = [("REPO_KEY_ID", "access_key_id"), ("REPO_SECRET", "secret_access_key")]
        .iter()
        .map(|(key, field)| json!({ "op": "set", "key": key, "link": { "kind": "bucket", "ref": "backups", "field": field, "mode": "managed" } }))
        .collect();
    r.post(&format!("/v1/secrets/projects/{}/environments/staging/changes", p.id), json!({ "ops": ops })).await;
    let mut staging = settings.clone();
    staging["base_path"] = json!("staging");
    staging["access_key"] = json!("roll://staging/REPO_KEY_ID");
    staging["secret_key"] = json!("roll://staging/REPO_SECRET");
    r.call(reqwest::Method::PUT, &format!("/v1/search/clusters/{id}/repositories/staged"), Some(json!({ "type": "s3", "settings": staging }))).await;
    let (s, t) = snapshot(owner.clone(), "staged", "s3").await;
    assert!(s == 200 && t.contains("\"SUCCESS\""), "{s} {t}");
    // A branch keeps both repositories (read only) and reads them as an untrusted copy:
    // with staging's key, never production's (protected).
    let b = r.post(&format!("/v1/search/clusters/{id}/branches"), json!({ "name": "data-dev" })).await;
    let (user, password) = (b["username"].as_str().unwrap().to_string(), b["password"].as_str().unwrap().to_string());
    let listed = |repo: &str| {
        let req = r.http.get(format!("{url}/_snapshot/{repo}/_all")).basic_auth(&user, Some(&password));
        async move {
            let a = req.send().await.unwrap();
            (a.status().as_u16(), a.text().await.unwrap())
        }
    };
    let (s, t) = listed("staged").await;
    assert!(s == 200 && t.contains("\"s3\""), "a branch reads with staging's key: {s} {t}");
    let (s, t) = listed("backups").await;
    assert!(s != 200 && !t.contains("\"s2\""), "a branch read with production's key: {s} {t}");
    let secret = p.value("REPO_SECRET").await;
    assert!(!t.contains(&secret), "{t}");
}

/// Reach names the roll apps services that read a folder (rolld's `ReachHolders`): one deploying
/// the environment reads its root, and one whose `roll://` variable names a key of a subfolder
/// reads that folder too. It opens no variable: a write-only one whose sealed value no key opens
/// doesn't stop it, and a reference set before roll apps kept them beside the values is marked
/// once, when the store opens.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reach_names_the_services_that_read_a_folder() {
    // With search: the harness reads every rolld's search address.
    let r = Rolld::start("apps,search", false).await;
    let project = Project::new(&r).await.id;
    let secrets = format!("/v1/secrets/projects/{project}/environments");
    r.call(reqwest::Method::PUT, &format!("{secrets}/production/secrets/LOG_LEVEL"), Some(json!({ "value": "debug" }))).await;
    r.call(reqwest::Method::PUT, &format!("{secrets}/production/secrets/API_TOKEN?path=/api"), Some(json!({ "value": "fake-api-token" }))).await;
    for name in ["web", "worker"] {
        let svc = r.post("/v1/apps/services", json!({ "project": project, "name": name, "source": { "kind": "upload" } })).await;
        let env = format!("/v1/apps/services/{}/environments/production", svc["id"].as_str().unwrap());
        r.call(reqwest::Method::PUT, &env, Some(json!({}))).await;
        if name == "worker" {
            r.call(reqwest::Method::PATCH, &format!("{env}/variables"), Some(json!({ "set": { "TOKEN": "roll://production/api/API_TOKEN", "SIGNING_KEY": "fake-signing-key" }, "seal": ["SIGNING_KEY"] }))).await;
        }
    }
    let home = r.home.clone();
    drop(r);
    let db = rusqlite::Connection::open(home.path().join("apps.db")).unwrap();
    db.execute("UPDATE variables SET reference = NULL WHERE name = 'TOKEN'", []).unwrap();
    assert_eq!(db.execute("UPDATE variables SET sealed = 'not-a-sealed-value' WHERE name = 'SIGNING_KEY' AND write_only = 1", []).unwrap(), 1);
    drop(db);
    let r = Rolld::start_in(home, "apps,search", false, &[], "/v1/info").await;
    let through = |reach: &Value| -> Vec<(String, String)> { reach["services"].as_array().unwrap().iter().map(|s| (s["name"].as_str().unwrap().to_string(), s["through"].as_str().unwrap().to_string())).collect() };
    let root = r.get(&format!("{secrets}/production/reach?path=/")).await;
    assert_eq!(through(&root), [("web".to_string(), "production:/".to_string()), ("worker".to_string(), "production:/".to_string())], "{root}");
    let api = r.get(&format!("{secrets}/production/reach?path=/api")).await;
    assert_eq!(through(&api), [("worker".to_string(), "roll://production/api/API_TOKEN".to_string())], "{api}");
    assert!(!api.to_string().contains("fake-api-token"), "names, never a value");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_environments_delete_names_the_services_that_read_it_and_needs_a_confirm() {
    let r = Rolld::start("apps,search", false).await;
    let project = Project::new(&r).await.id;
    let envs = format!("/v1/secrets/projects/{project}/environments");
    r.call(reqwest::Method::PUT, &format!("{envs}/staging/secrets/LOG_LEVEL"), Some(json!({ "value": "debug" }))).await;
    let svc = r.post("/v1/apps/services", json!({ "project": project, "name": "web", "source": { "kind": "upload" } })).await;
    r.call(reqwest::Method::PUT, &format!("/v1/apps/services/{}/environments/staging", svc["id"].as_str().unwrap()), Some(json!({}))).await;
    let preview = r.get(&format!("{envs}/staging/deletion")).await;
    assert_eq!(preview["confirm"], true, "something reads it: {preview}");
    assert!(preview["breaks"].as_array().unwrap().iter().any(|b| b["via"] == "service" && b["name"] == "web" && b["id"] == svc["id"]), "{preview}");
    assert!(!preview.to_string().contains("debug"), "names, never a value");
    let delete = |q: &str| r.http.delete(format!("{}{envs}/staging{q}", r.api)).bearer_auth(TOKEN).send();
    let refused = delete("").await.unwrap();
    assert_eq!(refused.status(), 409);
    assert!(refused.text().await.unwrap().contains("service web"));
    assert_eq!(delete("?confirm=staging").await.unwrap().status(), 200);
}

/// An environment's deletion preview names only the tokens that reach it, judged as reach judges
/// them (real roll-access): another person's token whose rule is "every project, `staging`" reaches
/// nothing in a project they aren't in, so it isn't named there and asks for no confirm; it is in
/// their own project's, and the operator's token of the same rule is in both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_environments_delete_names_only_the_tokens_that_reach_it() {
    const DASHBOARD: &str = "https://roll.sh";
    let home = Arc::new(tempfile::tempdir().unwrap());
    let outbox = home.path().join("outbox");
    let r = Rolld::start_in(home, "search", false, &[("ROLL_ACCOUNTS_OUTBOX", outbox.to_str().unwrap()), ("ROLL_DASHBOARD_ORIGIN", DASHBOARD)], "/v1/info").await;
    let theirs = Project::new(&r).await.id;
    let cookie = sign_in(&r, &outbox).await;
    let session = r.http.get(format!("{}/v1/accounts/session", r.api)).header("cookie", &cookie).send().await.unwrap().json::<Value>().await.unwrap();
    let csrf = session["csrf"].as_str().unwrap().to_string();
    let as_ada = |path: &str, body: Value| {
        let req = r.http.post(format!("{}{path}", r.api)).header("cookie", &cookie).header("origin", DASHBOARD).header("x-roll-csrf", &csrf).json(&body);
        async move {
            let a = req.send().await.unwrap();
            let status = a.status();
            let v: Value = a.json().await.unwrap_or_default();
            assert!(status.is_success(), "{status} {v}");
            v
        }
    };
    let hers = as_ada("/v1/workspace/projects", json!({ "name": "notes" })).await["id"].as_str().unwrap().to_string();
    let staging = json!([{ "product": "secrets", "resources": "*", "environments": ["staging"], "actions": { "read": "allow" } }]);
    as_ada("/v1/access/tokens", json!({ "name": "ada-staging-ci", "rules": staging })).await;
    for p in [&theirs, &hers] {
        r.call(reqwest::Method::PUT, &format!("/v1/secrets/projects/{p}/environments/staging/secrets/LOG_LEVEL"), Some(json!({ "value": "debug" }))).await;
    }
    let tokens = |preview: &Value| -> Vec<String> { preview["breaks"].as_array().unwrap().iter().filter(|b| b["via"] == "token").map(|b| b["name"].as_str().unwrap().to_string()).collect() };

    let preview = r.get(&format!("/v1/secrets/projects/{theirs}/environments/staging/deletion")).await;
    assert_eq!((tokens(&preview), &preview["confirm"]), (vec![], &json!(false)), "{preview}");
    let delete = |q: &str| r.http.delete(format!("{}/v1/secrets/projects/{theirs}/environments/staging{q}", r.api)).bearer_auth(TOKEN).send();
    let preview = r.get(&format!("/v1/secrets/projects/{hers}/environments/staging/deletion")).await;
    assert_eq!(tokens(&preview), ["ada-staging-ci"], "it reaches her own project's: {preview}");

    r.post("/v1/access/tokens", json!({ "name": "fleet-staging-ci", "rules": staging })).await;
    let preview = r.get(&format!("/v1/secrets/projects/{theirs}/environments/staging/deletion")).await;
    assert_eq!((tokens(&preview), &preview["confirm"]), (vec!["fleet-staging-ci".to_string()], &json!(true)), "{preview}");
    assert_eq!(delete("").await.unwrap().status(), 409);
    assert_eq!(delete("?confirm=staging").await.unwrap().status(), 200);
}

/// Invite `ada@example.com` with the operator's token and sign in by email code (from the
/// outbox): the session cookie, as `name=value`.
async fn sign_in(r: &Rolld, outbox: &std::path::Path) -> String {
    let invite = r.post("/v1/accounts/invites", json!({ "email": "ada@example.com" })).await;
    let start: Value = r.http.post(format!("{}/v1/accounts/email/start", r.api)).json(&json!({ "email": "ada@example.com", "invite": invite["token"] })).send().await.unwrap().json().await.unwrap();
    let mut code = None;
    for _ in 0..200 {
        let mails = std::fs::read_dir(outbox).into_iter().flatten().flatten().filter(|e| !e.file_name().to_string_lossy().starts_with('.'));
        code = mails.filter_map(|e| serde_json::from_slice::<Value>(&std::fs::read(e.path()).ok()?).ok()).find_map(|m| m["subject"].as_str().filter(|s| s.contains("sign-in code")).map(|s| s[..6].to_string()));
        if code.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let verify = r.http.post(format!("{}/v1/accounts/email/verify", r.api)).json(&json!({ "challenge": start["challenge"], "code": code.expect("a sign-in mail") })).send().await.unwrap();
    assert_eq!(verify.status(), 200);
    verify.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string()
}

/// Publicly trusted certificates end to end (docs/secrets.md, "Publicly trusted certificates"):
/// rolld orders from Pebble (Let's Encrypt's test CA, with External Account Binding required),
/// proving names over dns-01 through a stand-in for Cloudflare's DNS API that writes the records
/// into pebble-challtestsrv, the DNS server Pebble asks. Names under `unproven-acme.test` get
/// records nobody serves. Needs Docker: `ROLL_SECRETS_DOCKER=1` (`pnpm test:secrets --docker`).
mod public_certs {
    use super::*;
    use axum::Json;
    use axum::extract::{Path, Query, State};
    use std::collections::HashMap;
    use std::sync::Mutex;

    const PEBBLE: &str = "ghcr.io/letsencrypt/pebble:2.10.1@sha256:ddf230642b1a584f519f32e347de1b05a6e4c1f6c35c1863b33effeab5f78199";
    const CHALLTESTSRV: &str = "ghcr.io/letsencrypt/pebble-challtestsrv:2.10.1@sha256:12ce21884def456bcf9786542113949e1f19dc7738d2c70e156c2d0c38a1405b";
    /// Pebble's own test key for `kid-1` (test/config/pebble-config-external-account-bindings.json).
    const EAB_KEY: &str = "zWNDZM6eQGHWpSRTPal5eIUYFTu7EajVIoguysqZ9wG44nMEtx3MUAsUDkMTQ12W";
    const DNS_TOKEN: &str = "fake-cf-dns-token-roll-e2e";

    fn docker(args: &[&str]) -> Result<String, String> {
        let out = Command::new("docker").args(args).output().map_err(|e| e.to_string())?;
        match out.status.success() {
            true => Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()),
            false => Err(format!("docker {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr))),
        }
    }

    fn published(container: &str, port: &str) -> u16 {
        let p = docker(&["port", container, port]).unwrap();
        p.lines().next().and_then(|l| l.rsplit(':').next()).and_then(|p| p.parse().ok()).unwrap_or_else(|| panic!("{container} {port}: {p}"))
    }

    fn logs(container: &str) -> String {
        Command::new("docker").args(["logs", container]).output().map(|o| format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))).unwrap_or_default()
    }

    /// The containers and network this test started, removed by name however it ends.
    struct Started {
        ca: String,
        dns: String,
        network: String,
    }

    impl Drop for Started {
        fn drop(&mut self) {
            let _ = docker(&["rm", "-f", &self.ca, &self.dns]);
            let _ = docker(&["network", "rm", &self.network]);
        }
    }

    /// Cloudflare's DNS API, as much as the ACME client uses.
    #[derive(Clone)]
    struct FakeCloudflare {
        management: String,
        records: Arc<Mutex<HashMap<String, String>>>,
        http: reqwest::Client,
    }

    const ZONES: &[(&str, &str)] = &[("roll-acme.test", "zone-proven"), ("unproven-acme.test", "zone-unproven")];

    fn authorized(h: &axum::http::HeaderMap) -> bool {
        h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {DNS_TOKEN}"))
    }

    async fn zones(State(_): State<FakeCloudflare>, h: axum::http::HeaderMap, Query(q): Query<HashMap<String, String>>) -> (axum::http::StatusCode, Json<Value>) {
        if !authorized(&h) {
            return (axum::http::StatusCode::UNAUTHORIZED, Json(json!({ "success": false, "errors": [{ "code": 10000, "message": "Authentication error" }], "result": null })));
        }
        let name = q.get("name").cloned().unwrap_or_default();
        let found: Vec<Value> = ZONES.iter().filter(|(n, _)| *n == name).map(|(n, id)| json!({ "id": id, "name": n })).collect();
        (axum::http::StatusCode::OK, Json(json!({ "success": true, "errors": [], "result": found })))
    }

    async fn create(State(cf): State<FakeCloudflare>, h: axum::http::HeaderMap, Path(zone): Path<String>, Json(body): Json<Value>) -> Json<Value> {
        assert!(authorized(&h));
        let host = format!("{}.", body["name"].as_str().unwrap());
        // The unproven zone takes the record and serves nothing: Pebble finds no TXT record.
        if zone == "zone-proven" {
            cf.http.post(format!("{}/set-txt", cf.management)).json(&json!({ "host": host, "value": body["content"] })).send().await.unwrap().error_for_status().unwrap();
        }
        let id = format!("rec{}", cf.records.lock().unwrap().len() + 1);
        cf.records.lock().unwrap().insert(id.clone(), host);
        Json(json!({ "success": true, "errors": [], "result": { "id": id } }))
    }

    async fn remove(State(cf): State<FakeCloudflare>, Path((_zone, id)): Path<(String, String)>) -> Json<Value> {
        if let Some(host) = cf.records.lock().unwrap().remove(&id) {
            let (http, url) = (cf.http.clone(), format!("{}/clear-txt", cf.management));
            tokio::spawn(async move { http.post(url).json(&json!({ "host": host })).send().await });
        }
        Json(json!({ "success": true, "errors": [], "result": { "id": id } }))
    }

    /// PEM certificates, in order.
    fn ders(pem: &str) -> Vec<Vec<u8>> {
        x509_parser::pem::Pem::iter_from_buffer(pem.as_bytes()).map(|p| p.unwrap().contents).collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn public_certificates_come_from_pebble_over_dns01_renew_and_an_unproven_name_is_rotation_failed() {
        if !std::env::var("ROLL_SECRETS_DOCKER").is_ok_and(|v| v == "1") || docker(&["info"]).is_err() {
            eprintln!("public certificates: skipped (Pebble runs in Docker; ROLL_SECRETS_DOCKER=1 runs it)");
            return;
        }
        for image in [PEBBLE, CHALLTESTSRV] {
            if docker(&["image", "inspect", image]).is_err() {
                docker(&["pull", "-q", image]).unwrap();
            }
        }
        let pid = std::process::id();
        let started = Started { ca: format!("roll-secrets-acme-{pid}-pebble"), dns: format!("roll-secrets-acme-{pid}-dns"), network: format!("roll-secrets-acme-{pid}") };
        docker(&["network", "create", &started.network]).unwrap();
        docker(&["run", "-d", "--name", &started.dns, "--network", &started.network, "-p", "127.0.0.1::8055", CHALLTESTSRV, "-defaultIPv6", "", "-defaultIPv4", "127.0.0.1"]).unwrap();
        let dns_addr = format!("{}:8053", started.dns);
        docker(&[
            "run", "-d", "--name", &started.ca, "--network", &started.network, "-p", "127.0.0.1::14000", "-p", "127.0.0.1::15000",
            "-e", "PEBBLE_VA_NOSLEEP=1", "-e", "PEBBLE_WFE_NONCEREJECT=0", "-e", "PEBBLE_AUTHZREUSE=0",
            PEBBLE, "-config", "test/config/pebble-config-external-account-bindings.json", "-dnsserver", &dns_addr,
        ])
        .unwrap();
        let home = Arc::new(tempfile::tempdir().unwrap());
        let ca_file = home.path().join("pebble.minica.pem");
        docker(&["cp", &format!("{}:/test/certs/pebble.minica.pem", started.ca), ca_file.to_str().unwrap()]).unwrap();
        let directory = format!("https://localhost:{}/dir", published(&started.ca, "14000/tcp"));
        let management = format!("http://127.0.0.1:{}", published(&started.dns, "8055/tcp"));
        for i in 0.. {
            if logs(&started.ca).contains("Listening on") && logs(&started.dns).contains("management server") {
                break;
            }
            assert!(i < 200, "Pebble didn't come up: {}", logs(&started.ca));
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let fake = FakeCloudflare { management, records: Arc::default(), http: reqwest::Client::new() };
        let api = axum::Router::new()
            .route("/zones", axum::routing::get(zones))
            .route("/zones/{zone}/dns_records", axum::routing::post(create))
            .route("/zones/{zone}/dns_records/{id}", axum::routing::delete(remove))
            .with_state(fake.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cf_url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, api).await });

        let env = [
            ("ROLL_SECRETS_ACME_ALLOW", directory.as_str()),
            ("ROLL_SECRETS_ACME_CA_FILE", ca_file.to_str().unwrap()),
            ("ROLL_SECRETS_ACME_PROPAGATION_S", "0"),
            ("ROLL_CLOUDFLARE_API", cf_url.as_str()),
            // Pebble and the stand-in are on loopback.
            ("ROLL_CODE_WEBHOOK_ALLOW_PRIVATE", "1"),
        ];
        let r = Rolld::start_in(home.clone(), "search", false, &env, "/v1/info").await;
        let p = Project::new(&r).await;
        let base = format!("/v1/secrets/projects/{}/public-certs", p.id);
        let new = json!({ "name": "web", "directory_url": directory, "domains": ["www.roll-acme.test", "*.roll-acme.test"], "dns_provider": "cloudflare", "dns_credential": DNS_TOKEN, "eab_kid": "kid-1", "eab_hmac_key": EAB_KEY });
        let cert = r.post(&base, new).await;
        assert_eq!(cert["status"], "pending", "{cert}");
        let id = cert["id"].as_str().unwrap().to_string();
        // The background job orders it (every 5 seconds).
        let wait_for = |want: &'static str, serial: Option<String>| {
            let (r, base) = (&r, base.clone());
            async move {
                for _ in 0..240 {
                    let l = r.get(&base).await;
                    if l[0]["status"] == want && (serial.is_none() || l[0]["serial"].as_str() != serial.as_deref()) {
                        return l[0].clone();
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                panic!("public certificate never {want}: {}", r.get(&base).await);
            }
        };
        let issued = wait_for("issued", None).await;
        let keys = [("TLS_CERT", "certificate", "managed"), ("TLS_KEY", "private_key", "managed"), ("TLS_CA_CHAIN", "ca_chain", "managed")];
        let ops: Vec<Value> = keys.iter().map(|(key, field, mode)| json!({ "op": "set", "key": key, "link": { "kind": "public_cert", "ref": id, "field": field, "mode": mode } })).collect();
        r.post(&format!("/v1/secrets/projects/{}/environments/production/changes", p.id), json!({ "ops": ops })).await;

        // The certificate names what was asked, carries the key, and chains to Pebble's root.
        let (leaf_pem, chain_pem) = (p.value("TLS_CERT").await, p.value("TLS_CA_CHAIN").await);
        let leaf_der = &ders(&leaf_pem)[0];
        let (_, leaf) = x509_parser::parse_x509_certificate(leaf_der).unwrap();
        let mut names: Vec<&str> = leaf
            .subject_alternative_name()
            .unwrap()
            .unwrap()
            .value
            .general_names
            .iter()
            .filter_map(|n| match n {
                x509_parser::extensions::GeneralName::DNSName(d) => Some(*d),
                _ => None,
            })
            .collect();
        names.sort();
        assert_eq!(names, ["*.roll-acme.test", "www.roll-acme.test"]);
        let key = p.value("TLS_KEY").await;
        let pair = rcgen::KeyPair::from_pem(&key).unwrap();
        assert_eq!(leaf.public_key().subject_public_key.data.as_ref(), rcgen::PublicKeyData::der_bytes(&pair), "the key is the certificate's");
        let root_pem = reqwest::Client::builder().add_root_certificate(reqwest::Certificate::from_pem(&std::fs::read(&ca_file).unwrap()).unwrap()).build().unwrap()
            .get(format!("https://localhost:{}/roots/0", published(&started.ca, "15000/tcp"))).send().await.unwrap().text().await.unwrap();
        let mut path: Vec<Vec<u8>> = vec![leaf_der.clone()];
        path.extend(ders(&chain_pem));
        path.extend(ders(&root_pem));
        for pair in path.windows(2) {
            let (_, child) = x509_parser::parse_x509_certificate(&pair[0]).unwrap();
            let (_, parent) = x509_parser::parse_x509_certificate(&pair[1]).unwrap();
            child.verify_signature(Some(parent.public_key())).expect("signed by the next one up");
        }
        let listed = r.get(&base).await.to_string();
        assert!(!listed.contains(DNS_TOKEN) && !listed.contains(EAB_KEY) && !listed.contains("PRIVATE KEY"), "{listed}");
        assert!(fake.records.lock().unwrap().is_empty(), "the challenge records are taken down");

        // A renewal (the same account): a new certificate, and the linked keys rotate to it.
        r.post(&format!("{base}/{id}/renew"), json!({})).await;
        wait_for("issued", issued["serial"].as_str().map(String::from)).await;
        for _ in 0..40 {
            if p.value("TLS_CERT").await != leaf_pem {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert_ne!(p.value("TLS_CERT").await, leaf_pem, "the key got the renewed certificate");
        assert_ne!(p.value("TLS_KEY").await, key);

        // A name the records don't prove: the renewal fails as rotation.failed, and the
        // certificate in use stays.
        let events = format!("/v1/secrets/projects/{}/events", p.id);
        let start = r.get(&format!("{events}?latest=1")).await["next"].as_i64().unwrap();
        let in_use = p.value("TLS_CERT").await;
        r.call(reqwest::Method::PATCH, &format!("{base}/{id}"), Some(json!({ "domains": ["www.roll-acme.test", "www.unproven-acme.test"] }))).await;
        let failing = wait_for("failing", None).await;
        assert!(failing["last_error"].as_str().unwrap().contains("unproven-acme.test"), "{failing}");
        let failed = r.get(&format!("{events}?after={start}&kinds=rotation.failed")).await;
        assert_eq!(failed["events"].as_array().unwrap().len(), 3, "{failed}");
        assert_eq!(p.value("TLS_CERT").await, in_use);
        drop(r);
        drop(started);
    }
}
