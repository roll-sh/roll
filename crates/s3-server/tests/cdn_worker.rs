//! The CDN end to end: the Cloudflare Worker (`apps/cdn-worker`) running in workerd
//! (Miniflare, in a `node` container on the host network) in front of an in-process roll
//! server, and a mock Cloudflare API that hands purges to the Worker's cache.
//!
//! Covers edge token checks (presigned URLs and `?token=` JWTs refused at the edge without
//! reaching roll), caching (a second request never reaches roll; ranges and HEADs from the
//! cache), the origin's shared secret, immutable against mutable URLs, purge on change through
//! the outbox, and the Worker's usage reports (cached egress).
//!
//! Skipped when Docker or the Worker's `node_modules` aren't there, unless
//! `ROLL_CDN_REQUIRED=1` (`scripts/test-storage-cdn.sh`).

mod common;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use common::*;
use s3_server::Server;
use s3_store::cdn::{BucketCdn, CdnMode, CdnSettingsPatch};
use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ORIGIN_SECRET: &str = "edge-origin-secret-0123456789";
const JWT_SECRET: &str = "jwt-secret-for-tokens";
const IMAGE: &str = "node:22-bookworm-slim";

fn required() -> bool {
    std::env::var("ROLL_CDN_REQUIRED").is_ok_and(|v| v == "1")
}

fn repo() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn ready() -> bool {
    let docker = Command::new("docker").arg("info").output().is_ok_and(|o| o.status.success());
    let modules = repo().join("apps/cdn-worker/node_modules/miniflare").exists();
    let why = match (docker, modules) {
        (true, true) => return true,
        (false, _) => "Docker isn't available",
        (_, false) => "apps/cdn-worker has no node_modules (run `pnpm install`)",
    };
    if required() {
        panic!("{why}, and ROLL_CDN_REQUIRED=1");
    }
    eprintln!("skipping: {why}");
    false
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// The Worker in a container; removed on drop.
struct Worker {
    name: String,
    child: Child,
    url: String,
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.name]).output();
        let _ = self.child.kill();
    }
}

fn start_worker(port: u16, env: &[(&str, String)]) -> Worker {
    let root = repo();
    let dir = root.join("apps/cdn-worker");
    let name = format!("roll-cdn-worker-{}-{port}", std::process::id());
    let mut cmd = Command::new("docker");
    cmd.args(["run", "--rm", "--name", &name, "--network", "host", "-v"])
        .arg(format!("{}:{}:ro", root.display(), root.display()))
        .arg("-w")
        .arg(dir.display().to_string())
        .args(["-e", &format!("PORT={port}")]);
    for (k, v) in env {
        cmd.arg("-e").arg(format!("{k}={v}"));
    }
    cmd.args([IMAGE, "node", "test/serve.mjs"]).stdout(Stdio::piped()).stderr(Stdio::inherit());
    let mut child = cmd.spawn().expect("docker run");
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(u) = line.strip_prefix("ready ") {
                let _ = tx.send(u.to_string());
            }
        }
    });
    let url = rx.recv_timeout(Duration::from_secs(180)).expect("the Worker didn't start within 3 minutes");
    Worker { name, child, url }
}

/// A mock Cloudflare API: records purges and deletes the purged URLs from the Worker's cache
/// (its test hook), as Cloudflare would.
#[derive(Clone)]
struct Cf {
    calls: Arc<Mutex<Vec<Value>>>,
    worker: Arc<Mutex<String>>,
    http: reqwest::Client,
}

async fn purge_cache(State(cf): State<Cf>, Path(_zone): Path<String>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    cf.calls.lock().unwrap().push(body.clone());
    let worker = cf.worker.lock().unwrap().clone();
    let r = cf.http.post(format!("{worker}/__roll_test/purge")).header("x-roll-cdn-auth", ORIGIN_SECRET).json(&body).send().await;
    match r {
        Ok(r) if r.status().is_success() => (StatusCode::OK, Json(json!({ "success": true, "errors": [] }))),
        _ => (StatusCode::BAD_GATEWAY, Json(json!({ "success": false, "errors": [{ "code": 1, "message": "edge unreachable" }] }))),
    }
}

fn hs256(claims: &Value) -> String {
    use base64::Engine;
    use hmac::Mac;
    let b = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
    let head = b(br#"{"alg":"HS256","typ":"JWT"}"#);
    let body = b(claims.to_string().as_bytes());
    let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(JWT_SECRET.as_bytes()).unwrap();
    m.update(format!("{head}.{body}").as_bytes());
    format!("{head}.{body}.{}", b(&m.finalize().into_bytes()))
}

struct Edge {
    t: TestServer,
    s: Arc<Server>,
    w: Worker,
    cf: Cf,
    http: reqwest::Client,
}

impl Edge {
    fn origin_requests(&self) -> u64 {
        self.s.cdn.stats().origin_requests
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.http.get(format!("{}{path}", self.w.url)).send().await.unwrap()
    }

    /// GET until the edge has stored it (puts land just after the response).
    async fn hit(&self, path: &str) -> reqwest::Response {
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let r = self.get(path).await;
            if r.headers().get("x-roll-cdn-cache").is_some_and(|v| v == "HIT") {
                return r;
            }
        }
        panic!("{path} never became a cache hit");
    }

    async fn drain(&self) {
        for _ in 0..100 {
            self.s.dispatch_events().await.unwrap();
            if self.t.store.events_backlog().unwrap() == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("purges didn't go out: {:?}", self.t.store.pending_events(None, 10).unwrap());
    }
}

async fn edge() -> Option<Edge> {
    if !ready() {
        return None;
    }
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cf_addr = l.local_addr().unwrap();
    let cf = Cf { calls: Default::default(), worker: Default::default(), http: reqwest::Client::new() };
    let app = axum::Router::new().route("/client/v4/zones/{zone}/purge_cache", axum::routing::post(purge_cache)).with_state(cf.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let (t, s) = start_full(
        |_| {},
        move |c| {
            c.cdn.origin_secret = Some(ORIGIN_SECRET.into());
            c.cdn.cloudflare_api = format!("http://{cf_addr}/client/v4");
            c.notify.dispatch_interval = None;
            c.notify.backoff_base = Duration::from_millis(20);
            c.notify.backoff_max = Duration::from_millis(50);
        },
    )
    .await;
    let port = free_port();
    t.store
        .update_cdn_settings(CdnSettingsPatch {
            base_url: Some(format!("http://127.0.0.1:{port}")),
            zone_id: Some("zone1".into()),
            api_token: Some(s3_store::Secret::new("cf-token")),
            ..Default::default()
        })
        .await
        .unwrap();
    let (key_id, key_secret) = s3_server::cdn::presign_key(&t.store).await.unwrap();
    let env = s3_server::cdn::worker_env(&t.store, &t.endpoint, None, false).await.unwrap();
    let routes = env["vars"]["ROLL_ROUTES"].as_str().unwrap().to_string();
    let w = start_worker(
        port,
        &[
            ("ROLL_ORIGIN", t.endpoint.clone()),
            ("ROLL_ROUTES", routes),
            ("ROLL_CDN_ORIGIN_SECRET", ORIGIN_SECRET.into()),
            ("ROLL_CDN_PRESIGN_KEY_ID", key_id),
            ("ROLL_CDN_PRESIGN_SECRET", key_secret),
            ("ROLL_CDN_JWT_SECRET", JWT_SECRET.into()),
            ("ROLL_CDN_USAGE_FLUSH_MS", "3600000".into()),
            ("ROLL_TEST_HOOKS", "1".into()),
        ],
    );
    *cf.worker.lock().unwrap() = w.url.clone();
    Some(Edge { t, s, w, cf, http: reqwest::Client::new() })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_edge_caches_checks_tokens_purges_and_reports() {
    let Some(e) = edge().await else { return };
    let t = &e.t;
    t.store.create_bucket("pub", s3_store::BucketOptions { public: Some(true), ..Default::default() }).await.unwrap();
    t.store.create_bucket("priv", Default::default()).await.unwrap();
    t.store.create_bucket("nocdn", s3_store::BucketOptions { public: Some(true), ..Default::default() }).await.unwrap();
    for b in ["pub", "priv"] {
        t.store.set_bucket_cdn(b, Some(&BucketCdn { enabled: true, mode: CdnMode::Mutable })).await.unwrap();
    }
    let v1 = data(50_000, 1);
    t.client.put_object().bucket("pub").key("img/a b.png").body(v1.clone().into()).send().await.unwrap();
    t.client.put_object().bucket("priv").key("doc.pdf").body(b"private bytes".to_vec().into()).send().await.unwrap();
    t.client.put_object().bucket("nocdn").key("x").body(b"x".to_vec().into()).send().await.unwrap();
    e.drain().await;

    // ---- Caching: the second request never reaches roll.
    let r = e.get("/pub/img/a%20b.png").await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-roll-cdn-cache"], "MISS");
    assert!(r.headers().get("cdn-cache-control").is_none(), "internal headers stay at the edge");
    assert_eq!(r.bytes().await.unwrap().to_vec(), v1);
    let r = e.hit("/pub/img/a%20b.png").await;
    assert_eq!(r.bytes().await.unwrap().to_vec(), v1);
    let pulls = e.origin_requests();
    for _ in 0..3 {
        assert_eq!(e.get("/pub/img/a%20b.png").await.headers()["x-roll-cdn-cache"], "HIT");
    }
    // Ranges and HEADs from the cache too.
    let r = e.http.get(format!("{}/pub/img/a%20b.png", e.w.url)).header("range", "bytes=10-19").send().await.unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.bytes().await.unwrap().to_vec(), v1[10..20]);
    let r = e.http.head(format!("{}/pub/img/a%20b.png", e.w.url)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-roll-cdn-cache"], "HIT");
    assert_eq!(e.origin_requests(), pulls, "hits never reach roll");

    // ---- The origin's shared secret: without it (straight to roll) no CDN request is honoured.
    let r = e.http.get(format!("{}/pub/img/a%20b.png", t.endpoint)).header("x-roll-cdn-auth", "guess").send().await.unwrap();
    assert_eq!(r.status(), 403);
    // A bucket without the CDN isn't served through it.
    assert_eq!(e.get("/nocdn/x").await.status(), 403);

    // ---- Purge on change: an overwrite reaches the edge through the outbox.
    let v2 = data(40_000, 2);
    t.client.put_object().bucket("pub").key("img/a b.png").body(v2.clone().into()).send().await.unwrap();
    e.drain().await;
    let purged: Vec<String> = e.cf.calls.lock().unwrap().iter().flat_map(|c| c["files"].as_array().cloned().unwrap_or_default()).filter_map(|f| f.as_str().map(str::to_string)).collect();
    assert!(purged.contains(&format!("https://127.0.0.1:{}/pub/img/a%20b.png", e.w.url.rsplit(':').next().unwrap())), "{purged:?}");
    let r = e.get("/pub/img/a%20b.png").await;
    assert_eq!(r.headers()["x-roll-cdn-cache"], "MISS");
    assert_eq!(r.bytes().await.unwrap().to_vec(), v2);

    // ---- Immutable URLs: a year at the edge and in browsers.
    let meta = t.store.get_object("pub", "img/a b.png").unwrap();
    let u = s3_server::cdn::object_url(&t.store, "pub", &meta, true, None, REGION).await.unwrap().unwrap();
    let path = u.url.strip_prefix(&e.w.url).unwrap().to_string();
    assert!(path.starts_with("/v/"));
    let r = e.get(&path).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["cache-control"], "public, max-age=31536000, immutable");
    let r = e.hit(&path).await;
    assert_eq!(r.headers()["cache-control"], "public, max-age=31536000, immutable");
    assert_eq!(r.bytes().await.unwrap().to_vec(), v2);
    // The mutable URL keeps the object's own (none) for browsers.
    assert!(e.get("/pub/img/a%20b.png").await.headers().get("cache-control").is_none());
    let pulls = e.origin_requests();
    assert_eq!(e.get(&format!("/v/{}/pub/img/a%20b.png", "0".repeat(20))).await.status(), 404);
    assert_eq!(e.origin_requests(), pulls + 1, "an unknown version is the origin's to refuse");

    // ---- Private objects: presigned CDN URLs, checked at the edge.
    let pulls = e.origin_requests();
    assert_eq!(e.get("/priv/doc.pdf").await.status(), 403, "anonymous: the origin refuses");
    let meta = t.store.get_object("priv", "doc.pdf").unwrap();
    let u = s3_server::cdn::object_url(&t.store, "priv", &meta, false, Some(600), REGION).await.unwrap().unwrap();
    let signed = u.url.strip_prefix(&e.w.url).unwrap().to_string();
    let r = e.get(&signed).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "private bytes");
    let r = e.hit(&signed).await;
    assert_eq!(r.text().await.unwrap(), "private bytes");
    assert!(e.origin_requests() >= pulls + 2, "one refused anonymous pull, one signed fill");
    // The cached private copy is never served without a valid signature.
    assert_eq!(e.get("/priv/doc.pdf").await.status(), 403);
    let pulls = e.origin_requests();
    let tampered = signed.replace("X-Amz-Expires=600", "X-Amz-Expires=601");
    assert_eq!(e.get(&tampered).await.status(), 403);
    let expired = s3_server::cdn::object_url(&t.store, "priv", &meta, false, Some(1), REGION).await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_eq!(e.get(expired.url.strip_prefix(&e.w.url).unwrap()).await.status(), 403);
    // `?token=` JWTs: a bad or expired one never reaches roll either.
    let now = now_s();
    let wrong = hs256(&json!({ "url": "priv/other.pdf", "exp": now + 60 }));
    let old = hs256(&json!({ "url": "priv/doc.pdf", "exp": now - 1 }));
    assert_eq!(e.get(&format!("/priv/doc.pdf?token={wrong}")).await.status(), 403);
    assert_eq!(e.get(&format!("/priv/doc.pdf?token={old}")).await.status(), 403);
    assert_eq!(e.get(&format!("/priv/doc.pdf?token={}x", &old[..old.len() - 1])).await.status(), 403);
    assert_eq!(e.origin_requests(), pulls, "invalid credentials are refused at the edge");

    // ---- Usage: the Worker reports what it served, roll bills it as cached egress.
    let served = e.http.post(format!("{}/__roll_test/flush", e.w.url)).header("x-roll-cdn-auth", ORIGIN_SECRET).send().await.unwrap();
    assert_eq!(served.status(), 200);
    let b = t.store.bucket("pub").unwrap();
    let day = s3_store::day_of(t.store.now());
    let cached: u64 = t.store.usage(Some(&b.id), &day).unwrap().iter().map(|r| r.egress_cached).sum();
    assert!(cached >= 50_000 * 5 + 40_000 * 3, "cached egress {cached}");
    let p = t.store.bucket("priv").unwrap();
    assert!(t.store.usage(Some(&p.id), &day).unwrap().iter().map(|r| r.egress_cached).sum::<u64>() >= 13 * 2);
    assert!(e.s.cdn.stats().usage_batches >= 1);
}
