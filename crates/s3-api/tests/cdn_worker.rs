//! The developer API behind the CDN, end to end: the Cloudflare Worker (`apps/cdn-worker`) in
//! workerd (Miniflare, in a `node` container on the host network) in front of an in-process
//! roll (S3 API and developer API), and a mock Cloudflare API that hands purges (files, tags,
//! prefixes) to the Worker's cache.
//!
//! Covers the API's public and signed reads and renditions at the edge (hits never reach roll),
//! signed-URL tokens checked at the edge with the published key (the transformation they bind
//! keys the copy), WebP negotiation keyed at the edge, authenticated requests never served
//! there, the origin's shared secret, purges on overwrite through the S3 API and the developer
//! API (renditions included), immutable URLs, and cached egress attributed to the bucket.
//!
//! Skipped when Docker or the Worker's `node_modules` aren't there, unless
//! `ROLL_CDN_REQUIRED=1` (`scripts/test-storage-cdn.sh`).

mod common;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use common::{As, image_of, jpeg, start_with};
use image::ImageFormat;
use reqwest::Method;
use s3_store::cdn::{BucketCdn, CdnMode, CdnSettingsPatch};
use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ORIGIN_SECRET: &str = "edge-origin-secret-for-the-api-0123";
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

fn start_worker(port: u16, env: &[(String, String)]) -> Worker {
    let root = repo();
    let dir = root.join("apps/cdn-worker");
    let name = format!("roll-cdn-worker-api-{}-{port}", std::process::id());
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

/// A mock Cloudflare API: records purges and applies them to the Worker's cache (its test hook).
#[derive(Clone)]
struct Cf {
    calls: Arc<Mutex<Vec<Value>>>,
    worker: Arc<Mutex<String>>,
    http: reqwest::Client,
}

async fn purge_cache(State(cf): State<Cf>, Path(_zone): Path<String>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    cf.calls.lock().unwrap().push(body.clone());
    let worker = cf.worker.lock().unwrap().clone();
    match cf.http.post(format!("{worker}/__roll_test/purge")).header("x-roll-cdn-auth", ORIGIN_SECRET).json(&body).send().await {
        Ok(r) if r.status().is_success() => (StatusCode::OK, Json(json!({ "success": true, "errors": [] }))),
        _ => (StatusCode::BAD_GATEWAY, Json(json!({ "success": false, "errors": [{ "code": 1, "message": "edge unreachable" }] }))),
    }
}

struct Edge {
    t: common::T,
    w: Worker,
    cf: Cf,
    http: reqwest::Client,
}

struct Got {
    status: u16,
    cache: String,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
}

impl Got {
    fn h(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

impl Edge {
    fn pulls(&self) -> u64 {
        self.t.server.cdn.stats().origin_requests
    }

    async fn get_with(&self, path: &str, headers: &[(&str, &str)]) -> Got {
        let mut b = self.http.get(format!("{}{path}", self.w.url));
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let r = b.send().await.unwrap();
        let status = r.status().as_u16();
        let headers = r.headers().clone();
        let cache = headers.get("x-roll-cdn-cache").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        Got { status, cache, headers, body: r.bytes().await.unwrap().to_vec() }
    }

    async fn get(&self, path: &str) -> Got {
        self.get_with(path, &[]).await
    }

    /// GET until the edge has stored it (puts land just after the response).
    async fn hit_with(&self, path: &str, headers: &[(&str, &str)]) -> Got {
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let r = self.get_with(path, headers).await;
            if r.cache == "HIT" {
                return r;
            }
        }
        panic!("{path} never became a cache hit");
    }

    async fn hit(&self, path: &str) -> Got {
        self.hit_with(path, &[]).await
    }

    async fn drain(&self) {
        for _ in 0..100 {
            self.t.server.dispatch_events().await.unwrap();
            if self.t.store.events_backlog().unwrap() == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("purges didn't go out: {:?}", self.t.store.pending_events(None, 10).unwrap());
    }

    /// A signed URL's path at the edge (`/storage/v1/...?token=`).
    async fn sign(&self, bucket: &str, key: &str, body: Value) -> String {
        let r = self.t.json(Method::POST, &format!("/object/sign/{bucket}/{key}"), As::Service, body).await;
        assert_eq!(r.status, 200, "{}", r.text());
        format!("/storage/v1{}", r.json()["signedURL"].as_str().unwrap())
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
    let t = start_with(move |c| {
        c.cdn.origin_secret = Some(ORIGIN_SECRET.into());
        c.cdn.cloudflare_api = format!("http://{cf_addr}/client/v4");
        c.notify.dispatch_interval = None;
        c.notify.backoff_base = Duration::from_millis(20);
        c.notify.backoff_max = Duration::from_millis(50);
    })
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
    // What `GET /v1/storage/cdn/worker?secrets=true` hands the operator.
    let env = s3_server::cdn::worker_env(&t.store, &t.s3, Some(&t.url), true).await.unwrap();
    let mut vars: Vec<(String, String)> = env["vars"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string())).collect();
    assert!(vars.iter().any(|(k, v)| k == "ROLL_CDN_JWT_SECRET" && v.starts_with("hex:")));
    assert!(vars.iter().any(|(k, v)| k == "ROLL_API_ORIGIN" && !v.ends_with("/storage/v1")));
    vars.extend([("ROLL_CDN_ORIGIN_SECRET", ORIGIN_SECRET), ("ROLL_CDN_USAGE_FLUSH_MS", "3600000"), ("ROLL_TEST_HOOKS", "1")].map(|(k, v)| (k.to_string(), v.to_string())));
    let w = start_worker(port, &vars);
    *cf.worker.lock().unwrap() = w.url.clone();
    Some(Edge { t, w, cf, http: reqwest::Client::new() })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_edge_serves_the_storage_api() {
    let Some(e) = edge().await else { return };
    let t = &e.t;
    t.bucket("imgs", true).await;
    t.bucket("priv", false).await;
    t.bucket("nocdn", true).await;
    for b in ["imgs", "priv"] {
        t.store.set_bucket_cdn(b, Some(&BucketCdn { enabled: true, mode: CdnMode::Mutable })).await.unwrap();
    }
    let v1 = jpeg(400, 300);
    for (b, k) in [("imgs", "cat.jpg"), ("priv", "doc.jpg"), ("nocdn", "x.jpg")] {
        assert_eq!(t.upload(b, k, As::Service, "image/jpeg", &v1).await.status, 200);
    }
    e.drain().await;

    // ---- Public objects and renditions: the second request never reaches roll.
    let obj = "/storage/v1/object/public/imgs/cat.jpg";
    let thumb = "/storage/v1/render/image/public/imgs/cat.jpg?width=100&format=webp";
    let r = e.get(obj).await;
    assert_eq!((r.status, r.cache.as_str()), (200, "MISS"));
    assert_eq!(r.body, v1);
    assert!(r.h("cdn-cache-control").is_none(), "internal headers stay at the edge");
    let r = e.get(thumb).await;
    assert_eq!((r.status, r.cache.as_str()), (200, "MISS"));
    assert_eq!(image_of(&r.body), (ImageFormat::WebP, 100, 75));
    e.hit(obj).await;
    let r = e.hit(thumb).await;
    assert_eq!(image_of(&r.body), (ImageFormat::WebP, 100, 75));
    let pulls = e.pulls();
    for _ in 0..3 {
        assert_eq!(e.get(obj).await.cache, "HIT");
        assert_eq!(e.get(thumb).await.cache, "HIT");
    }
    assert_eq!(e.pulls(), pulls, "hits never reach roll");

    // ---- No format: WebP for clients that take it, the source's format for the rest; the
    // edge keeps one copy of each, never serving one to the other.
    let auto = "/storage/v1/render/image/public/imgs/cat.jpg?width=64";
    let webp = [("accept", "image/avif,image/webp,*/*;q=0.8")];
    assert_eq!(e.get_with(auto, &webp).await.h("content-type"), Some("image/webp"));
    assert_eq!(e.hit_with(auto, &webp).await.h("content-type"), Some("image/webp"));
    let r = e.get(auto).await;
    assert_eq!((r.cache.as_str(), r.h("content-type")), ("MISS", Some("image/jpeg")));
    assert_eq!(e.hit(auto).await.h("content-type"), Some("image/jpeg"));

    // ---- Signed URLs: checked at the edge with the published key; the transformation a token
    // binds keys the copy.
    let wide = e.sign("priv", "doc.jpg", json!({ "expiresIn": 600, "transform": { "width": 80, "format": "png" } })).await;
    let narrow = e.sign("priv", "doc.jpg", json!({ "expiresIn": 600, "transform": { "width": 40, "format": "png" } })).await;
    assert!(wide.starts_with("/storage/v1/render/image/sign/priv/doc.jpg?token="));
    let r = e.get(&wide).await;
    assert_eq!((r.status, r.cache.as_str()), (200, "MISS"));
    assert_eq!(image_of(&r.body), (ImageFormat::Png, 80, 60));
    assert_eq!(image_of(&e.hit(&wide).await.body), (ImageFormat::Png, 80, 60));
    let r = e.get(&narrow).await;
    assert_eq!(r.cache, "MISS", "another transformation is another copy");
    assert_eq!(image_of(&r.body), (ImageFormat::Png, 40, 30));
    assert_eq!(image_of(&e.hit(&narrow).await.body), (ImageFormat::Png, 40, 30));
    let plain = e.sign("priv", "doc.jpg", json!({ "expiresIn": 600 })).await;
    assert!(plain.starts_with("/storage/v1/object/sign/priv/doc.jpg?token="));
    assert_eq!(e.get(&plain).await.body, v1);
    assert_eq!(e.hit(&plain).await.body, v1);
    // Bad tokens never reach roll.
    let pulls = e.pulls();
    let token = wide.split("token=").nth(1).unwrap();
    let parts: Vec<&str> = token.split('.').collect();
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let mut claims: Value = serde_json::from_slice(&b64.decode(parts[1]).unwrap()).unwrap();
    claims["transformations"] = json!("width:2000");
    let forged = format!("{}.{}.{}", parts[0], b64.encode(claims.to_string()), parts[2]);
    assert_eq!(e.get(&format!("/storage/v1/render/image/sign/priv/doc.jpg?token={forged}")).await.status, 403);
    assert_eq!(e.get(&format!("/storage/v1/render/image/sign/priv/other.jpg?token={token}")).await.status, 403);
    assert_eq!(e.get("/storage/v1/render/image/sign/priv/doc.jpg").await.status, 403);
    assert_eq!(e.get("/storage/v1/object/sign/priv/doc.jpg?token=x.y.z").await.status, 403);
    let short = e.sign("priv", "doc.jpg", json!({ "expiresIn": 1, "transform": { "width": 40, "format": "png" } })).await;
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_eq!(e.get(&short).await.status, 403, "expired");
    assert_eq!(e.pulls(), pulls, "invalid tokens are refused at the edge");

    // ---- Authenticated requests are never served at the edge.
    let key = t.service.clone();
    let auth = [("apikey", key.as_str()), ("authorization", &format!("Bearer {key}"))];
    for path in ["/storage/v1/object/authenticated/priv/doc.jpg", "/storage/v1/render/image/authenticated/priv/doc.jpg?width=10", "/storage/v1/object/priv/doc.jpg", "/storage/v1/bucket"] {
        let r = e.get_with(path, &auth).await;
        assert_eq!(r.status, 404, "{path}");
        assert_eq!(r.cache, "PASS");
    }
    assert_eq!(e.pulls(), pulls, "not routed");
    // A key on a public route is ignored: the private object stays private.
    let r = e.get_with("/storage/v1/object/public/priv/doc.jpg", &auth).await;
    assert_ne!(r.status, 200);
    assert_ne!(e.get("/storage/v1/object/public/priv/doc.jpg").await.status, 200);
    // A bucket without the CDN isn't served through it.
    assert_ne!(e.get("/storage/v1/object/public/nocdn/x.jpg").await.status, 200);
    // The origin's shared secret: without it, nothing is honoured as the CDN's.
    let r = t.send(t.req(Method::GET, "/object/public/imgs/cat.jpg", As::Nobody).header("x-roll-cdn-auth", "guess")).await;
    assert_eq!(r.status, 403);

    // ---- Immutable URLs: the blob's, a year at the edge.
    let u = t.json(Method::POST, "/roll/url/imgs", As::Service, json!({ "path": "cat.jpg", "immutable": true, "transform": { "width": 40, "format": "png" } })).await.json();
    let immutable = u["publicUrl"].as_str().unwrap().strip_prefix(&e.w.url).unwrap().to_string();
    assert!(immutable.starts_with("/storage/v1/v/"), "{immutable}");
    let r = e.get(&immutable).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.h("cache-control"), Some("public, max-age=31536000, immutable"));
    let r = e.hit(&immutable).await;
    assert_eq!(image_of(&r.body), (ImageFormat::Png, 40, 30));

    // ---- Purge on overwrite through the developer API: the object and every rendition.
    let v2 = jpeg(200, 200);
    let r = t.send(t.req(Method::PUT, "/object/imgs/cat.jpg", As::Service).header("content-type", "image/jpeg").body(v2.clone())).await;
    assert_eq!(r.status, 200);
    e.drain().await;
    let tag = s3_store::cdn::object_tag("imgs", "cat.jpg");
    assert!(e.cf.calls.lock().unwrap().iter().any(|c| c["tags"].as_array().is_some_and(|a| a.contains(&json!(tag)))), "the object's tag is purged");
    let r = e.get(obj).await;
    assert_eq!((r.cache.as_str(), r.body == v2), ("MISS", true));
    let r = e.get(thumb).await;
    assert_eq!(r.cache, "MISS");
    assert_eq!(image_of(&r.body), (ImageFormat::WebP, 100, 100));
    assert_eq!(e.get_with(auto, &webp).await.cache, "MISS");
    // The old immutable URL names a blob that's gone (its copy went with the tag).
    let r = e.get(&immutable).await;
    assert_eq!(r.cache, "MISS");
    assert_eq!(serde_json::from_slice::<Value>(&r.body).unwrap()["statusCode"], "404");
    e.hit(obj).await;
    e.hit(thumb).await;

    // ---- And through the S3 API.
    let v3 = jpeg(300, 150);
    t.s3_client().put_object().bucket("imgs").key("cat.jpg").content_type("image/jpeg").body(v3.clone().into()).send().await.unwrap();
    e.drain().await;
    let r = e.get(obj).await;
    assert_eq!((r.cache.as_str(), r.body == v3), ("MISS", true));
    assert_eq!(image_of(&e.get(thumb).await.body), (ImageFormat::WebP, 100, 50));
    // Signed renditions too (their copies carry the object's tag).
    t.s3_client().put_object().bucket("priv").key("doc.jpg").content_type("image/jpeg").body(jpeg(160, 160).into()).send().await.unwrap();
    e.drain().await;
    let r = e.get(&wide).await;
    assert_eq!(r.cache, "MISS");
    assert_eq!(image_of(&r.body), (ImageFormat::Png, 80, 80));

    // ---- Cached egress: what the edge served of these routes, billed to their buckets.
    let r = e.http.post(format!("{}/__roll_test/flush", e.w.url)).header("x-roll-cdn-auth", ORIGIN_SECRET).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let day = s3_store::day_of(t.store.now());
    let cached = |bucket: &str| -> u64 {
        let b = t.store.bucket(bucket).unwrap();
        t.store.usage(Some(&b.id), &day).unwrap().iter().map(|r| r.egress_cached).sum()
    };
    assert!(cached("imgs") >= (v1.len() * 5) as u64, "imgs: {}", cached("imgs"));
    assert!(cached("priv") >= (v1.len() * 2) as u64, "priv: {}", cached("priv"));
    assert_eq!(cached("nocdn"), 0);
}
