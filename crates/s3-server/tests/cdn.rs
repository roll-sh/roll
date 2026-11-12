//! The gateway's hot-object cache and the CDN's server side, in process: the origin's checks
//! (shared secret, immutable tokens, presigned CDN URLs, caching headers, origin-pull
//! metering), purges through the outbox to a mock Cloudflare API (batches, retries, tags,
//! bucket-wide purges), and the Worker's usage reports. The Worker itself runs in
//! `tests/cdn_worker.rs`.

mod common;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use common::*;
use s3_server::{CacheConfig, Server};
use s3_store::cdn::{BucketCdn, CdnMode, CdnSettingsPatch, PurgeMode};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SECRET_HEADER: &str = "x-roll-cdn-auth";
const ORIGIN_SECRET: &str = "origin-secret-0123456789";

fn http() -> reqwest::Client {
    reqwest::Client::builder().build().unwrap()
}

async fn put(t: &TestServer, bucket: &str, key: &str, body: Vec<u8>) {
    t.client.put_object().bucket(bucket).key(key).body(body.into()).send().await.unwrap();
}

fn chunk_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for shard in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(shard.path()).into_iter().flatten().flatten() {
            if f.path().extension().is_none() {
                out.push(f.path());
            }
        }
    }
    out
}

async fn cached_server(memory: u64, chunk: u64) -> (TestServer, Arc<Server>, tempfile::TempDir) {
    let cache_dir = tempfile::tempdir().unwrap();
    let dir = cache_dir.path().to_path_buf();
    let (t, s) = start_full(
        |_| {},
        move |c| {
            c.cache = CacheConfig { memory_bytes: memory, disk_bytes: 64 << 20, dir: Some(dir), chunk_bytes: chunk, max_object_bytes: 8 << 20, run_chunks: 8, kms: false };
        },
    )
    .await;
    (t, s, cache_dir)
}

async fn get_bytes(t: &TestServer, bucket: &str, key: &str, range: Option<&str>) -> Vec<u8> {
    let mut g = t.client.get_object().bucket(bucket).key(key);
    if let Some(r) = range {
        g = g.range(r);
    }
    g.send().await.unwrap().body.collect().await.unwrap().into_bytes().to_vec()
}

#[tokio::test]
async fn cache_serves_whole_objects_and_ranges_from_aligned_chunks() {
    let (t, s, _dir) = cached_server(16 << 20, 64 << 10).await;
    t.client.create_bucket().bucket("hot").send().await.unwrap();
    let body = data(300_000, 3);
    put(&t, "hot", "a.bin", body.clone()).await;
    assert_eq!(get_bytes(&t, "hot", "a.bin", None).await, body);
    let first = s.cache_stats();
    assert!(first.enabled);
    assert_eq!(first.misses, 5, "300 000 bytes are five 64 KiB chunks");
    assert_eq!(first.s3_gets, 1, "one ranged GET for the run of missing chunks");
    // Again, and ranges inside it: no more S3 GETs.
    assert_eq!(get_bytes(&t, "hot", "a.bin", None).await, body);
    for (a, b) in [(0, 0), (100, 70_000), (65_535, 65_536), (200_000, 299_999)] {
        assert_eq!(get_bytes(&t, "hot", "a.bin", Some(&format!("bytes={a}-{b}"))).await, body[a..=b], "{a}-{b}");
    }
    assert_eq!(get_bytes(&t, "hot", "a.bin", Some("bytes=-10")).await, body[body.len() - 10..]);
    let after = s.cache_stats();
    assert_eq!(after.s3_gets, 1);
    assert_eq!(after.gets_avoided, 6);
    assert!(after.memory_hits >= 5 + 5);
    assert!(after.bytes_saved >= 300_000);
    // Customers pay for the GETs as without the cache.
    let b = t.store.bucket("hot").unwrap();
    assert_eq!(t.store.meter.pending(&b.id).class_b, 7);

    // An overwrite is a new blob: never the old bytes.
    let new = data(200_000, 9);
    put(&t, "hot", "a.bin", new.clone()).await;
    assert_eq!(get_bytes(&t, "hot", "a.bin", None).await, new);
}

#[tokio::test]
async fn concurrent_misses_make_one_s3_request() {
    let (t, s, _dir) = cached_server(16 << 20, 64 << 10).await;
    t.client.create_bucket().bucket("hot").send().await.unwrap();
    let body = data(512 << 10, 5);
    put(&t, "hot", "big", body.clone()).await;
    let before = s.cache_stats().s3_gets;
    let reads: Vec<_> = (0..12)
        .map(|_| {
            let c = t.client.clone();
            tokio::spawn(async move { c.get_object().bucket("hot").key("big").send().await.unwrap().body.collect().await.unwrap().into_bytes().to_vec() })
        })
        .collect();
    for r in reads {
        assert_eq!(r.await.unwrap(), body);
    }
    let st = s.cache_stats();
    assert_eq!(st.s3_gets - before, 1, "{st:?}");
}

#[tokio::test]
async fn disk_entries_are_checked_and_bad_ones_never_served() {
    let (t, s, dir) = cached_server(16 << 20, 64 << 10).await;
    t.client.create_bucket().bucket("hot").send().await.unwrap();
    let body = data(200_000, 7);
    put(&t, "hot", "d.bin", body.clone()).await;
    let cache = s.cache.clone().unwrap();
    // Fetched once (memory), then again once memory forgot it: that admits it to disk.
    assert_eq!(get_bytes(&t, "hot", "d.bin", None).await, body);
    cache.clear_memory();
    assert_eq!(get_bytes(&t, "hot", "d.bin", None).await, body);
    let mut files = Vec::new();
    for _ in 0..100 {
        files = chunk_files(dir.path());
        if files.len() == 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(files.len(), 4, "200 000 bytes are four chunks on disk");
    // From disk.
    cache.clear_memory();
    let hits = s.cache_stats().disk_hits;
    assert_eq!(get_bytes(&t, "hot", "d.bin", None).await, body);
    assert_eq!(s.cache_stats().disk_hits - hits, 4);
    // A flipped bit on disk: dropped, and the bytes come from object storage.
    for f in &files {
        let mut d = std::fs::read(f).unwrap();
        let last = d.len() - 1;
        d[last] ^= 0x40;
        std::fs::write(f, d).unwrap();
    }
    cache.clear_memory();
    let gets = s.cache_stats().s3_gets;
    assert_eq!(get_bytes(&t, "hot", "d.bin", None).await, body);
    let st = s.cache_stats();
    assert_eq!(st.disk_corrupt, 4, "{st:?}");
    assert!(st.s3_gets > gets);
    for _ in 0..100 {
        if files.iter().all(|f| !f.exists()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(files.iter().all(|f| !f.exists()), "corrupt entries are deleted");
}

#[tokio::test]
async fn customer_keyed_and_large_objects_bypass_the_cache() {
    let (t, s, _dir) = cached_server(16 << 20, 64 << 10).await;
    t.client.create_bucket().bucket("hot").send().await.unwrap();
    let key = [7u8; 32];
    let b64 = |b: &[u8]| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b);
    let md5 = b64(&<md5::Md5 as md5::Digest>::digest(key));
    let body = data(100_000, 1);
    t.client.put_object().bucket("hot").key("c").body(body.clone().into()).sse_customer_algorithm("AES256").sse_customer_key(b64(&key)).sse_customer_key_md5(&md5).send().await.unwrap();
    let got = t.client.get_object().bucket("hot").key("c").sse_customer_algorithm("AES256").sse_customer_key(b64(&key)).sse_customer_key_md5(&md5).send().await.unwrap();
    assert_eq!(got.body.collect().await.unwrap().into_bytes().to_vec(), body);
    let big = data((8 << 20) + 1, 2);
    put(&t, "hot", "big", big.clone()).await;
    assert_eq!(get_bytes(&t, "hot", "big", Some("bytes=5-9")).await, big[5..=9]);
    let st = s.cache_stats();
    assert_eq!(st.bypassed, 2);
    assert_eq!(st.misses, 0);
}

/// Latency of a cache hit against a read from object storage. `cargo test -p s3-server --test cdn
/// -- --ignored --nocapture cache_latency`; with `ROLL_E2E_S3_BUCKET` (and
/// `ROLL_E2E_S3_ENDPOINT`) the backend is S3.
#[tokio::test]
#[ignore]
async fn cache_latency() {
    let (t, s, _dir) = cached_server(256 << 20, 1 << 20).await;
    t.client.create_bucket().bucket("lat").send().await.unwrap();
    for (key, size) in [("4k", 4 << 10), ("1m", 1 << 20)] {
        put(&t, "lat", key, data(size, 1)).await;
    }
    let b = t.store.bucket("lat").unwrap();
    let n = 60;
    for key in ["4k", "1m"] {
        let meta = t.store.get_object("lat", key).unwrap();
        // Straight from object storage (what a miss costs).
        let mut direct = Vec::new();
        for _ in 0..n {
            let at = std::time::Instant::now();
            let (_, st) = t.store.open_blob(&meta, None).await.unwrap();
            let _: Vec<_> = futures::StreamExt::collect::<Vec<_>>(st).await;
            direct.push(at.elapsed());
        }
        // Through the cache, warm.
        let cache = s.cache.clone().unwrap();
        let _ = futures::StreamExt::collect::<Vec<_>>(cache.open(&t.store, &meta, 0..meta.size).await.unwrap()).await;
        let mut hit = Vec::new();
        for _ in 0..n {
            let at = std::time::Instant::now();
            let st = cache.open(&t.store, &meta, 0..meta.size).await.unwrap();
            let _: Vec<_> = futures::StreamExt::collect::<Vec<_>>(st).await;
            hit.push(at.elapsed());
        }
        // From disk (memory dropped each time).
        let mut disk = Vec::new();
        cache.clear_memory();
        let _ = futures::StreamExt::collect::<Vec<_>>(cache.open(&t.store, &meta, 0..meta.size).await.unwrap()).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        for _ in 0..n {
            cache.clear_memory();
            let at = std::time::Instant::now();
            let st = cache.open(&t.store, &meta, 0..meta.size).await.unwrap();
            let _: Vec<_> = futures::StreamExt::collect::<Vec<_>>(st).await;
            disk.push(at.elapsed());
        }
        let p = |v: &mut Vec<Duration>, q: f64| {
            v.sort();
            v[((v.len() as f64 - 1.0) * q) as usize]
        };
        println!(
            "{key}: backend ({}) p50 {:?} p99 {:?} | memory hit p50 {:?} p99 {:?} | disk hit p50 {:?} p99 {:?}",
            t.store.describe(),
            p(&mut direct, 0.5),
            p(&mut direct, 0.99),
            p(&mut hit, 0.5),
            p(&mut hit, 0.99),
            p(&mut disk, 0.5),
            p(&mut disk, 0.99)
        );
    }
    let _ = b;
    println!("{:?}", s.cache_stats());
}

// ---- The origin ----

async fn cdn_server(tweak: impl FnOnce(&mut s3_server::Config) + Send + 'static) -> (TestServer, Arc<Server>) {
    start_full(
        |_| {},
        move |c| {
            c.cdn.origin_secret = Some(ORIGIN_SECRET.into());
            c.notify.dispatch_interval = None;
            c.notify.backoff_base = Duration::from_millis(10);
            c.notify.backoff_max = Duration::from_millis(20);
            tweak(c);
        },
    )
    .await
}

async fn settings(t: &TestServer, p: CdnSettingsPatch) {
    t.store.update_cdn_settings(p).await.unwrap();
}

#[tokio::test]
async fn origin_requires_the_shared_secret_and_the_bucket_setting() {
    let (t, s) = cdn_server(|_| {}).await;
    settings(&t, CdnSettingsPatch { base_url: Some("https://cdn.test".into()), ..Default::default() }).await;
    t.store.create_bucket("pub", s3_store::BucketOptions { public: Some(true), ..Default::default() }).await.unwrap();
    t.store.create_bucket("off", s3_store::BucketOptions { public: Some(true), ..Default::default() }).await.unwrap();
    t.store.set_bucket_cdn("pub", Some(&BucketCdn { enabled: true, mode: CdnMode::Mutable })).await.unwrap();
    let body = data(10_000, 4);
    put(&t, "pub", "a.txt", body.clone()).await;
    put(&t, "off", "a.txt", body.clone()).await;
    let url = |p: &str| format!("{}{p}", t.endpoint);
    let h = http();

    // Anonymous public reads work as always (no CDN headers).
    let r = h.get(url("/pub/a.txt")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().get("cdn-cache-control").is_none());
    assert_eq!(r.bytes().await.unwrap().len(), 10_000);
    let b = t.store.bucket("pub").unwrap();
    // Its egress is counted once the response is done.
    for _ in 0..200 {
        if t.store.meter.pending(&b.id).egress_internal == 10_000 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(t.store.meter.pending(&b.id).egress_internal, 10_000);
    // A wrong secret: refused.
    let r = h.get(url("/pub/a.txt")).header(SECRET_HEADER, "nope").send().await.unwrap();
    assert_eq!(r.status(), 403);
    // From the CDN: cached for the mutable TTL, an origin pull rather than egress.
    let before = t.store.meter.pending(&b.id);
    let r = h.get(url("/pub/a.txt")).header(SECRET_HEADER, ORIGIN_SECRET).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["cdn-cache-control"], "public, max-age=60");
    assert_eq!(r.bytes().await.unwrap().to_vec(), body);
    for _ in 0..200 {
        if t.store.meter.pending(&b.id).egress_cdn_origin > before.egress_cdn_origin {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let after = t.store.meter.pending(&b.id);
    assert_eq!(after.egress_cdn_origin - before.egress_cdn_origin, 10_000);
    assert_eq!(after.egress_internet + after.egress_internal, before.egress_internet + before.egress_internal);
    assert_eq!(after.class_b, before.class_b + 1, "an origin pull is a GET");
    assert_eq!(s.cdn.stats().origin_requests, 1);
    // A bucket without the CDN can't be pulled through it.
    let r = h.get(url("/off/a.txt")).header(SECRET_HEADER, ORIGIN_SECRET).send().await.unwrap();
    assert_eq!(r.status(), 403);
    // Writes never come through the CDN.
    let r = h.put(url("/pub/b.txt")).header(SECRET_HEADER, ORIGIN_SECRET).body("x").send().await.unwrap();
    assert_eq!(r.status(), 405);
    // An object that says no-store isn't cached at the edge.
    t.client.put_object().bucket("pub").key("private.txt").cache_control("private, max-age=0").body(b"p".to_vec().into()).send().await.unwrap();
    let r = h.get(url("/pub/private.txt")).header(SECRET_HEADER, ORIGIN_SECRET).send().await.unwrap();
    assert_eq!(r.headers()["cdn-cache-control"], "no-store");
}

#[tokio::test]
async fn immutable_urls_serve_only_their_blob() {
    let (t, _s) = cdn_server(|_| {}).await;
    settings(&t, CdnSettingsPatch { base_url: Some("https://cdn.test".into()), ..Default::default() }).await;
    t.store.create_bucket("pub", s3_store::BucketOptions { public: Some(true), ..Default::default() }).await.unwrap();
    t.store.set_bucket_cdn("pub", Some(&BucketCdn { enabled: true, mode: CdnMode::Immutable })).await.unwrap();
    put(&t, "pub", "app.js", b"v1".to_vec()).await;
    let meta = t.store.get_object("pub", "app.js").unwrap();
    let u = s3_server::cdn::object_url(&t.store, "pub", &meta, true, None, REGION).await.unwrap().unwrap();
    let token = s3_store::cdn::immutable_token(&meta.blob_id);
    assert_eq!(u.url, format!("https://cdn.test/v/{token}/pub/app.js"));
    let h = http();
    let get = |path: String| h.get(format!("{}{path}", t.endpoint)).header(SECRET_HEADER, ORIGIN_SECRET).send();
    let r = get(format!("/v/{token}/pub/app.js")).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["cache-control"], "public, max-age=31536000, immutable");
    assert_eq!(r.headers()["cdn-cache-control"], "public, max-age=31536000, immutable");
    assert_eq!(r.text().await.unwrap(), "v1");
    // Another token, or the object changed: not this URL's bytes.
    assert_eq!(get(format!("/v/{}/pub/app.js", "0".repeat(20))).await.unwrap().status(), 404);
    put(&t, "pub", "app.js", b"v2".to_vec()).await;
    assert_eq!(get(format!("/v/{token}/pub/app.js")).await.unwrap().status(), 404);
    // Without the CDN's secret, `/v/…` is just a bucket called `v`.
    assert_eq!(h.get(format!("{}/v/{token}/pub/app.js", t.endpoint)).send().await.unwrap().status(), 404);
}

#[tokio::test]
async fn transformed_images_from_the_cdn_follow_its_rules() {
    use image::ImageEncoder;
    let (t, _s) = cdn_server(|_| {}).await;
    settings(&t, CdnSettingsPatch { base_url: Some("https://cdn.test".into()), ..Default::default() }).await;
    let img = image::RgbImage::from_fn(80, 40, |x, y| image::Rgb([x as u8, y as u8, 0]));
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpeg).write_image(img.as_raw(), 80, 40, image::ExtendedColorType::Rgb8).unwrap();
    for b in ["pub", "off"] {
        t.store.create_bucket(b, s3_store::BucketOptions { public: Some(true), ..Default::default() }).await.unwrap();
        put(&t, b, "a.jpg", jpeg.clone()).await;
    }
    t.store.set_bucket_cdn("pub", Some(&BucketCdn { enabled: true, mode: CdnMode::Mutable })).await.unwrap();
    let meta = t.store.get_object("pub", "a.jpg").unwrap();
    let token = s3_store::cdn::immutable_token(&meta.blob_id);
    let h = http();
    let get = |path: String| h.get(format!("{}{path}?x-roll-transform=width%3A20%2Cformat%3Apng", t.endpoint)).header(SECRET_HEADER, ORIGIN_SECRET).send();
    let r = get("/pub/a.jpg".into()).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "image/png");
    assert_eq!(r.headers()["cdn-cache-control"], "public, max-age=60");
    let r = get(format!("/v/{token}/pub/a.jpg")).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["cache-control"], "public, max-age=31536000, immutable");
    // Only the blob the token names, and only buckets with the CDN on.
    assert_eq!(get(format!("/v/{}/pub/a.jpg", "0".repeat(20))).await.unwrap().status(), 404);
    assert_eq!(get("/off/a.jpg".into()).await.unwrap().status(), 403);
    put(&t, "pub", "a.jpg", jpeg).await;
    assert_eq!(get(format!("/v/{token}/pub/a.jpg")).await.unwrap().status(), 404);
}

#[tokio::test]
async fn presigned_cdn_urls_are_checked_again_at_the_origin() {
    let (t, _s) = cdn_server(|_| {}).await;
    settings(&t, CdnSettingsPatch { base_url: Some("http://cdn.test:8787".into()), ..Default::default() }).await;
    t.store.create_bucket("priv", Default::default()).await.unwrap();
    t.store.set_bucket_cdn("priv", Some(&BucketCdn { enabled: true, mode: CdnMode::Mutable })).await.unwrap();
    put(&t, "priv", "doc.pdf", b"secret".to_vec()).await;
    let meta = t.store.get_object("priv", "doc.pdf").unwrap();
    let u = s3_server::cdn::object_url(&t.store, "priv", &meta, false, Some(600), REGION).await.unwrap().unwrap();
    assert!(u.url.starts_with("http://cdn.test:8787/priv/doc.pdf?X-Amz-"), "{}", u.url);
    assert!(u.expires_at.is_some());
    let query = u.url.split_once('?').unwrap().1.to_string();
    let h = http();
    let go = |q: String, host: &'static str| h.get(format!("{}/priv/doc.pdf", t.endpoint)).header(SECRET_HEADER, ORIGIN_SECRET).header("x-roll-cdn-host", host).header("x-roll-cdn-presigned", q).send();
    let r = go(query.clone(), "cdn.test:8787").await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "secret");
    // Anonymous: the bucket is private.
    let r = h.get(format!("{}/priv/doc.pdf", t.endpoint)).header(SECRET_HEADER, ORIGIN_SECRET).send().await.unwrap();
    assert_eq!(r.status(), 403);
    // Signed for another host, or tampered with.
    assert_eq!(go(query.clone(), "evil.test").await.unwrap().status(), 403);
    assert_eq!(go(query.replace("X-Amz-Expires=600", "X-Amz-Expires=601"), "cdn.test:8787").await.unwrap().status(), 403);
    // The CDN key isn't one of the account's keys.
    assert!(t.store.access_key(s3_server::cdn::CDN_KEY_ID).unwrap().is_some());
}

// ---- Purges ----

#[derive(Clone, Default)]
struct MockCf {
    calls: Arc<Mutex<Vec<(String, String, Value)>>>,
    /// Fail this many calls first (500).
    fail: Arc<Mutex<u32>>,
}

async fn purge_cache(State(m): State<MockCf>, Path(zone): Path<String>, headers: HeaderMap, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    m.calls.lock().unwrap().push((zone, auth, body));
    let mut fail = m.fail.lock().unwrap();
    if *fail > 0 {
        *fail -= 1;
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "success": false, "errors": [{ "code": 10000, "message": "try later" }] })));
    }
    (StatusCode::OK, Json(json!({ "success": true, "errors": [], "result": { "id": "x" } })))
}

async fn mock_cloudflare() -> (MockCf, String) {
    let m = MockCf::default();
    let app = axum::Router::new().route("/client/v4/zones/{zone}/purge_cache", axum::routing::post(purge_cache)).with_state(m.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (m, format!("http://{addr}/client/v4"))
}

/// Dispatch until the outbox is empty (failed rows wait out their backoff).
async fn drain(s: &Server) {
    for _ in 0..50 {
        s.dispatch_events().await.unwrap();
        if s.store.events_backlog().unwrap() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("the outbox didn't drain: {:?}", s.store.pending_events(None, 10).unwrap());
}

#[tokio::test]
async fn changes_are_purged_through_the_outbox_in_batches_and_retried() {
    let (cf, api) = mock_cloudflare().await;
    let (t, s) = cdn_server(move |c| c.cdn.cloudflare_api = api).await;
    settings(
        &t,
        CdnSettingsPatch { base_url: Some("https://cdn.test".into()), zone_id: Some("zone1".into()), api_token: Some(s3_store::Secret::new("cf-token")), purge_batch: Some(2), ..Default::default() },
    )
    .await;
    t.client.create_bucket().bucket("pics").send().await.unwrap();
    t.client.create_bucket().bucket("plain").send().await.unwrap();
    t.store.set_bucket_cdn("pics", Some(&BucketCdn { enabled: true, mode: CdnMode::Mutable })).await.unwrap();
    assert_eq!(t.store.events_backlog().unwrap(), 0, "turning the CDN on has nothing to purge");
    *cf.fail.lock().unwrap() = 1;
    for k in ["a.png", "b c.png", "d.png"] {
        put(&t, "pics", k, b"x".to_vec()).await;
    }
    put(&t, "plain", "a.png", b"x".to_vec()).await;
    t.client.put_object_tagging().bucket("pics").key("a.png").tagging(aws_sdk_s3::types::Tagging::builder().tag_set(aws_sdk_s3::types::Tag::builder().key("k").value("v").build().unwrap()).build().unwrap()).send().await.unwrap();
    assert_eq!(t.store.events_backlog().unwrap(), 3, "one purge per change of a CDN bucket's object (tags don't change what's served)");
    drain(&s).await;
    let calls = cf.calls.lock().unwrap().clone();
    assert!(calls.len() >= 6, "three batches of two files and two of tags, one failed and sent again: {calls:?}");
    let mut purged = std::collections::BTreeSet::new();
    let mut tags = std::collections::BTreeSet::new();
    for (i, (zone, auth, body)) in calls.iter().enumerate() {
        assert_eq!(zone, "zone1");
        assert_eq!(auth, "Bearer cf-token");
        let (items, into) = match body.get("files") {
            Some(f) => (f.as_array().unwrap(), &mut purged),
            None => (body["tags"].as_array().unwrap(), &mut tags),
        };
        assert!(items.len() <= 2);
        if i > 0 {
            into.extend(items.iter().map(|f| f.as_str().unwrap().to_string()));
        }
    }
    let want: std::collections::BTreeSet<String> = ["a.png", "b%20c.png", "d.png"].iter().flat_map(|k| [format!("https://cdn.test/pics/{k}"), format!("https://cdn.test/pics/{k}?roll-cdn-ns=private")]).collect();
    assert_eq!(purged, want);
    // The storage API's copies (transformations, tokens, Accept) go by the object's tag.
    let want: std::collections::BTreeSet<String> = ["a.png", "b c.png", "d.png"].iter().map(|k| s3_store::cdn::object_tag("pics", k)).collect();
    assert_eq!(tags, want);
    assert_eq!(s.cdn.stats().purge_failures, 1);

    // By tag: one tag per object, every URL form at once (immutable ones too).
    cf.calls.lock().unwrap().clear();
    settings(&t, CdnSettingsPatch { purge: Some(PurgeMode::Tags), ..Default::default() }).await;
    t.client.delete_object().bucket("pics").key("d.png").send().await.unwrap();
    drain(&s).await;
    let calls = cf.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].2, json!({ "tags": [s3_store::cdn::object_tag("pics", "d.png")] }));

    // Making the bucket more private purges all of it.
    cf.calls.lock().unwrap().clear();
    t.store.update_bucket("pics", s3_store::BucketUpdate { public: Some(true), ..Default::default() }).await.unwrap();
    t.store.update_bucket("pics", s3_store::BucketUpdate { public: Some(false), ..Default::default() }).await.unwrap();
    drain(&s).await;
    let calls = cf.calls.lock().unwrap().clone();
    let bodies: Vec<&Value> = calls.iter().map(|c| &c.2).collect();
    assert!(bodies.contains(&&json!({ "tags": [s3_store::cdn::bucket_tag("pics")] })), "{bodies:?}");
    let prefixes: Vec<&str> = bodies.iter().filter_map(|b| b.get("prefixes")).flat_map(|p| p.as_array().unwrap()).filter_map(Value::as_str).collect();
    for p in ["cdn.test/pics/", "cdn.test/storage/v1/object/public/pics/", "cdn.test/storage/v1/render/image/sign/pics/"] {
        assert!(prefixes.contains(&p), "{p}: {prefixes:?}");
    }
}

#[tokio::test]
async fn purges_wait_for_a_token_and_vanish_without_a_cdn() {
    let (cf, api) = mock_cloudflare().await;
    let (t, s) = cdn_server(move |c| c.cdn.cloudflare_api = api).await;
    t.client.create_bucket().bucket("pics").send().await.unwrap();
    t.store.set_bucket_cdn("pics", Some(&BucketCdn { enabled: true, mode: CdnMode::Mutable })).await.unwrap();
    // No CDN hostname: nothing can be cached, nothing to purge.
    put(&t, "pics", "a", b"x".to_vec()).await;
    drain(&s).await;
    assert!(cf.calls.lock().unwrap().is_empty());
    // A hostname but no token yet: kept (and retried) until there is one.
    settings(&t, CdnSettingsPatch { base_url: Some("https://cdn.test".into()), zone_id: Some("z".into()), ..Default::default() }).await;
    put(&t, "pics", "b", b"x".to_vec()).await;
    s.dispatch_events().await.unwrap();
    let left = t.store.pending_events(None, 10).unwrap();
    assert_eq!(left.len(), 1);
    assert!(left[0].error.as_deref().unwrap().contains("API token"));
    settings(&t, CdnSettingsPatch { api_token: Some(s3_store::Secret::new("tok")), ..Default::default() }).await;
    drain(&s).await;
    assert_eq!(cf.calls.lock().unwrap().len(), 2, "the S3 routes' URLs, and the developer API's copies by tag");
    // The token is sealed at rest and never shown.
    let shown = t.store.update_cdn_settings(Default::default()).await.unwrap();
    assert!(shown.api_token.is_none());
    assert_eq!(t.store.cdn_settings().unwrap().api_token.unwrap().expose(), "tok");
}

// ---- Usage ----

#[tokio::test]
async fn worker_usage_reports_count_once_per_batch() {
    let (t, s) = cdn_server(|_| {}).await;
    t.client.create_bucket().bucket("pics").send().await.unwrap();
    let h = http();
    let report = |secret: &'static str, batch: &'static str, bytes: u64| {
        h.post(format!("{}/_roll/cdn/usage", t.endpoint))
            .header(SECRET_HEADER, secret)
            .json(&json!({ "batch": batch, "entries": [{ "bucket": "pics", "bytes": bytes, "requests": 3, "hits": 2 }, { "bucket": "gone", "bytes": 5 }] }))
            .send()
    };
    assert_eq!(report("wrong", "b1", 100).await.unwrap().status(), 403);
    let r = report(ORIGIN_SECRET, "b1", 1_500_000_000).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["duplicate"], false);
    // The Worker retrying the same batch changes nothing.
    assert_eq!(report(ORIGIN_SECRET, "b1", 1_500_000_000).await.unwrap().json::<Value>().await.unwrap()["duplicate"], true);
    report(ORIGIN_SECRET, "b2", 500_000_000).await.unwrap();
    let b = t.store.bucket("pics").unwrap();
    let day = s3_store::day_of(t.store.now());
    let rows = t.store.usage(Some(&b.id), &day).unwrap();
    assert_eq!(rows.iter().map(|r| r.egress_cached).sum::<u64>(), 2_000_000_000);
    assert_eq!(s.cdn.stats().usage_duplicates, 1);
    let r = h.post(format!("{}/_roll/cdn/usage", t.endpoint)).header(SECRET_HEADER, ORIGIN_SECRET).body("{").send().await.unwrap();
    assert_eq!(r.status(), 400);
    let r = h.get(format!("{}/_roll/cdn/usage", t.endpoint)).send().await.unwrap();
    assert_eq!(r.status(), 405);
}
