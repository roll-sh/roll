//! `/v1/storage` end to end: the control plane and the S3 API in process, driven over HTTP (and
//! through `aws-sdk-s3` where keys and presigned URLs must work against the S3 API).

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use reqwest::{Client, StatusCode};
use s3_control::{Control, ControlConfig};
use s3_store::{Clock, Store, StoreConfig};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

struct T {
    api: String,
    s3: String,
    store: Arc<Store>,
    http: Client,
    _dir: tempfile::TempDir,
}

async fn start() -> T {
    start_with(|_| {}, |_| {}).await
}

async fn start_with(store_cfg: impl FnOnce(&mut StoreConfig), control_cfg: impl FnOnce(&mut ControlConfig)) -> T {
    let dir = tempfile::tempdir().unwrap();
    let backend = s3_store::test_backend("s3-control").await;
    let mut sc = StoreConfig::new(dir.path().join("meta")).manual();
    store_cfg(&mut sc);
    let store = Store::open(sc, backend).await.unwrap();

    let s3_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let s3 = format!("http://127.0.0.1:{}", s3_listener.local_addr().unwrap().port());
    let server = s3_server::Server::new(store.clone(), s3_server::Config { spool_dir: dir.path().to_path_buf(), ..Default::default() });
    tokio::spawn(async move { s3_server::serve(s3_listener, server).await.unwrap() });

    let mut cc = ControlConfig::local(s3.clone());
    control_cfg(&mut cc);
    let control = Control::new(store.clone(), cc).await.unwrap();
    let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://127.0.0.1:{}", api_listener.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(api_listener, s3_control::api::router(control)).await.unwrap() });
    T { api, s3, store, http: Client::new(), _dir: dir }
}

impl T {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.api)
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = self.http.request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), self.url(path));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or_else(|_| panic!("{method} {path}: not JSON: {text}")))
    }

    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (s, v) = self.call(method, path, body).await;
        assert_eq!(s, StatusCode::OK, "{method} {path}: {v}");
        v
    }

    /// The status of a failing call, checking the body is `{"error": "..."}`.
    async fn err(&self, method: &str, path: &str, body: Option<Value>) -> StatusCode {
        let (s, v) = self.call(method, path, body).await;
        assert!(!s.is_success(), "{method} {path} should fail: {v}");
        assert!(v["error"].as_str().is_some_and(|e| !e.is_empty()), "{method} {path}: error body {v}");
        s
    }

    async fn bucket(&self, name: &str) -> Value {
        self.ok("POST", "/v1/storage/buckets", Some(json!({ "name": name }))).await
    }

    async fn upload(&self, bucket: &str, key: &str, data: &[u8], headers: &[(&str, &str)]) -> Value {
        let mut req = self.http.put(self.url(&format!("/v1/storage/buckets/{bucket}/upload?key={}", q(key)))).body(data.to_vec());
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let v: Value = resp.json().await.unwrap();
        assert_eq!(status, StatusCode::OK, "upload {bucket}/{key}: {v}");
        v
    }

    async fn download(&self, bucket: &str, key: &str) -> reqwest::Response {
        self.http.get(self.url(&format!("/v1/storage/buckets/{bucket}/download?key={}", q(key)))).send().await.unwrap()
    }

    fn sdk(&self, access: &str, secret: &str) -> aws_sdk_s3::Client {
        let conf = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .endpoint_url(&self.s3)
            .credentials_provider(Credentials::new(access, secret, None, None, "test"))
            .force_path_style(true)
            .build();
        aws_sdk_s3::Client::from_conf(conf)
    }
}

fn md5_hex(b: &[u8]) -> String {
    let mut m = s3_proto::checksum::Md5::new();
    m.update(b);
    hex(&m.finalize())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn q(s: &str) -> String {
    url_encode(s)
}

fn url_encode(s: &str) -> String {
    s3_proto::http::uri_encode(s, true)
}

#[tokio::test]
async fn info_reports_endpoint_pricing_and_totals() {
    let t = start().await;
    t.bucket("alpha").await;
    t.upload("alpha", "a.txt", b"hello", &[]).await;
    let v = t.ok("GET", "/v1/storage", None).await;
    assert_eq!(v["endpoint"], t.s3);
    assert_eq!(v["region"], "us-east-1");
    assert_eq!(v["virtual_host_domain"], Value::Null);
    assert_eq!(v["storage_class"], "LOCAL");
    assert_eq!(v["recover_days_default"], 7);
    assert_eq!(v["pricing"], json!({ "usd_per_gb_month": 0.023, "usd_per_million_class_a": 5.0, "usd_per_million_class_b": 0.4, "usd_per_gb_egress_internet": 0.09, "usd_per_gb_egress_internal": 0.0 }));
    assert_eq!((v["buckets"].as_u64(), v["objects"].as_u64(), v["bytes"].as_u64()), (Some(1), Some(1), Some(5)));
    assert_eq!(v["scrub"], json!({ "last_at": null, "missing": 0, "mismatched": 0 }));
    assert!(v["storage"].as_str().is_some_and(|s| !s.is_empty()));
}

#[tokio::test]
async fn bucket_lifecycle_and_errors() {
    let t = start().await;
    let b = t.bucket("photos").await;
    assert!(b["id"].as_str().unwrap().starts_with("bkt_"));
    assert_eq!(b["name"], "photos");
    // The region the S3 API reports.
    assert_eq!(b["region"], t.ok("GET", "/v1/storage", None).await["region"]);
    assert_eq!(b["public"], false);
    assert_eq!(b["recover_days"], 7);
    assert_eq!(b["parent"], Value::Null);
    assert_eq!(b["branches"], json!([]));
    assert_eq!(b["endpoint"], t.s3);
    assert_eq!(b["url"], format!("{}/photos", t.s3));
    assert_eq!(b["virtual_host_url"], Value::Null);
    assert_eq!((b["objects"].as_u64(), b["bytes"].as_u64(), b["trash_bytes"].as_u64()), (Some(0), Some(0), Some(0)));

    let pub_b = t.ok("POST", "/v1/storage/buckets", Some(json!({ "name": "public-assets", "public": true, "recover_days": 0 }))).await;
    assert_eq!((pub_b["public"].as_bool(), pub_b["recover_days"].as_u64()), (Some(true), Some(0)));

    // Errors.
    assert_eq!(t.err("POST", "/v1/storage/buckets", Some(json!({ "name": "photos" }))).await, StatusCode::CONFLICT);
    assert_eq!(t.err("POST", "/v1/storage/buckets", Some(json!({ "name": "Bad_Name" }))).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("POST", "/v1/storage/buckets", Some(json!({ "name": "ab" }))).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("POST", "/v1/storage/buckets", Some(json!({ "name": "ok-name", "recover_days": 31 }))).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("POST", "/v1/storage/buckets", Some(json!({ "nope": 1 }))).await, StatusCode::BAD_REQUEST);
    let raw = t.http.post(t.url("/v1/storage/buckets")).header("content-type", "application/json").body("{not json").send().await.unwrap();
    assert_eq!(raw.status(), StatusCode::BAD_REQUEST);
    assert!(raw.json::<Value>().await.unwrap()["error"].is_string());
    assert_eq!(t.err("GET", "/v1/storage/buckets/missing", None).await, StatusCode::NOT_FOUND);
    assert_eq!(t.err("PATCH", "/v1/storage/buckets/missing", Some(json!({ "public": true }))).await, StatusCode::NOT_FOUND);
    assert_eq!(t.err("DELETE", "/v1/storage/buckets/missing", None).await, StatusCode::NOT_FOUND);

    let list = t.ok("GET", "/v1/storage/buckets", None).await;
    let names: Vec<&str> = list.as_array().unwrap().iter().map(|b| b["name"].as_str().unwrap()).collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"photos") && names.contains(&"public-assets"));
    assert!(list.as_array().unwrap().iter().all(|b| b["url"].is_string() && b["endpoint"].is_string()));

    // Settings.
    let cors = json!([{ "allowed_origins": ["https://app.example"], "allowed_methods": ["GET", "PUT"], "allowed_headers": ["*"], "expose_headers": ["etag"], "max_age_s": 600 }]);
    let lifecycle = json!([{ "id": "tmp", "prefix": "tmp/", "enabled": true, "expire_days": 3, "abort_multipart_days": 1 }]);
    let u = t.ok("PATCH", "/v1/storage/buckets/photos", Some(json!({ "public": true, "recover_days": 14, "cors": cors, "lifecycle": lifecycle }))).await;
    assert_eq!((u["public"].as_bool(), u["recover_days"].as_u64()), (Some(true), Some(14)));
    assert_eq!(u["cors"][0]["allowed_origins"], json!(["https://app.example"]));
    assert_eq!(u["cors"][0]["max_age_s"], 600);
    assert_eq!(u["lifecycle"][0]["expire_days"], 3);
    assert_eq!(u["lifecycle"][0]["abort_multipart_days"], 1);
    let again = t.ok("GET", "/v1/storage/buckets/photos", None).await;
    assert_eq!(again["cors"], u["cors"]);
    assert_eq!(again["lifecycle"], u["lifecycle"]);
    // An empty list removes a setting; omitted settings stay.
    let cleared = t.ok("PATCH", "/v1/storage/buckets/photos", Some(json!({ "cors": [] }))).await;
    assert_eq!(cleared["cors"], json!([]));
    assert_eq!(cleared["lifecycle"], u["lifecycle"]);
    assert_eq!(cleared["public"], true);
    assert_eq!(t.err("PATCH", "/v1/storage/buckets/photos", Some(json!({ "recover_days": 99 }))).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("PATCH", "/v1/storage/buckets/photos", Some(json!({ "cors": [{ "allowed_origins": [], "allowed_methods": [] }] }))).await, StatusCode::BAD_REQUEST);

    // Delete: refused while not empty, forced deletes everything.
    t.upload("photos", "cat.jpg", b"meow", &[]).await;
    assert_eq!(t.err("DELETE", "/v1/storage/buckets/photos", None).await, StatusCode::CONFLICT);
    let d = t.ok("DELETE", "/v1/storage/buckets/photos?force=true", None).await;
    assert_eq!(d, json!({ "deleted": "photos" }));
    assert_eq!(t.err("GET", "/v1/storage/buckets/photos", None).await, StatusCode::NOT_FOUND);
    assert_eq!(t.ok("DELETE", "/v1/storage/buckets/public-assets", None).await, json!({ "deleted": "public-assets" }));
    assert_eq!(t.ok("GET", "/v1/storage/buckets", None).await, json!([]));
}

#[tokio::test]
async fn virtual_host_urls_use_the_domain_and_port() {
    let t = start_with(|_| {}, |c| c.virtual_host_domain = Some("s3.roll.test".into())).await;
    let b = t.bucket("site").await;
    let port = t.s3.rsplit(':').next().unwrap();
    assert_eq!(b["virtual_host_url"], format!("http://site.s3.roll.test:{port}"));
    assert_eq!(t.ok("GET", "/v1/storage", None).await["virtual_host_domain"], "s3.roll.test");
}

#[tokio::test]
async fn branches_are_copy_on_write() {
    let t = start().await;
    t.bucket("main").await;
    t.upload("main", "shared.txt", b"v1", &[]).await;
    let br = t.ok("POST", "/v1/storage/buckets/main/branches", Some(json!({ "name": "preview-1" }))).await;
    assert_eq!(br["name"], "preview-1");
    assert_eq!(br["parent"]["name"], "main");
    assert!(br["parent"]["at"].as_i64().unwrap() > 0);
    assert_eq!(t.ok("GET", "/v1/storage/buckets/main", None).await["branches"], json!(["preview-1"]));

    // The branch sees the parent's object, and later writes stay on their side.
    let got = t.download("preview-1", "shared.txt").await;
    assert_eq!(got.bytes().await.unwrap().as_ref(), b"v1");
    t.upload("main", "shared.txt", b"v2", &[]).await;
    t.upload("preview-1", "only-branch.txt", b"b", &[]).await;
    assert_eq!(t.download("preview-1", "shared.txt").await.bytes().await.unwrap().as_ref(), b"v1");
    assert_eq!(t.download("main", "shared.txt").await.bytes().await.unwrap().as_ref(), b"v2");
    assert_eq!(t.download("main", "only-branch.txt").await.status(), StatusCode::NOT_FOUND);

    assert_eq!(t.err("POST", "/v1/storage/buckets/missing/branches", Some(json!({ "name": "x-branch" }))).await, StatusCode::NOT_FOUND);
    assert_eq!(t.err("POST", "/v1/storage/buckets/main/branches", Some(json!({ "name": "main" }))).await, StatusCode::CONFLICT);
    assert_eq!(t.err("POST", "/v1/storage/buckets/main/branches", Some(json!({ "name": "BAD" }))).await, StatusCode::BAD_REQUEST);
    // A bucket with branches can't go, even forced.
    assert_eq!(t.err("DELETE", "/v1/storage/buckets/main?force=true", None).await, StatusCode::CONFLICT);
    t.ok("DELETE", "/v1/storage/buckets/preview-1?force=true", None).await;
    t.ok("DELETE", "/v1/storage/buckets/main?force=true", None).await;
}

#[tokio::test]
async fn upload_passes_headers_and_download_streams() {
    let t = start().await;
    t.bucket("docs").await;
    let data = b"%PDF-1.7 hello".to_vec();
    let o = t
        .upload(
            "docs",
            "reports/2026/q3 report.pdf",
            &data,
            &[
                ("content-type", "application/pdf"),
                ("cache-control", "max-age=60"),
                ("content-disposition", "inline"),
                ("content-encoding", "identity"),
                ("content-language", "en"),
                ("expires", "Wed, 21 Oct 2026 07:28:00 GMT"),
                ("x-amz-meta-owner", "ada"),
                ("x-amz-tagging", "team=finance&year=2026"),
            ],
        )
        .await;
    assert_eq!(o["key"], "reports/2026/q3 report.pdf");
    assert_eq!(o["size"], data.len());
    assert_eq!(o["etag"], md5_hex(&data));
    assert_eq!(o["content_type"], "application/pdf");
    assert_eq!(o["cache_control"], "max-age=60");
    assert_eq!(o["content_disposition"], "inline");
    assert_eq!(o["content_encoding"], "identity");
    assert_eq!(o["content_language"], "en");
    assert_eq!(o["expires"], "Wed, 21 Oct 2026 07:28:00 GMT");
    assert_eq!(o["metadata"], json!({ "owner": "ada" }));
    assert_eq!(o["tags"], json!({ "team": "finance", "year": "2026" }));
    assert_eq!(o["checksum"]["algorithm"], "CRC64NVME");
    assert_eq!(o["checksum"]["type"], "FULL_OBJECT");
    let want = s3_proto::checksum::b64(&s3_proto::checksum::Algorithm::Crc64Nvme.checksum(&data));
    assert_eq!(o["checksum"]["value"], want);
    assert_eq!(o["parts"], 0);
    assert!(o["last_modified"].as_i64().unwrap() > 0);

    let info = t.ok("GET", &format!("/v1/storage/buckets/docs/object?key={}", q("reports/2026/q3 report.pdf")), None).await;
    assert_eq!(info, o);

    let r = t.download("docs", "reports/2026/q3 report.pdf").await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["content-type"], "application/pdf");
    assert_eq!(r.headers()["content-length"], data.len().to_string());
    assert_eq!(r.headers()["etag"], format!("\"{}\"", md5_hex(&data)));
    let cd = r.headers()["content-disposition"].to_str().unwrap().to_string();
    assert!(cd.starts_with("attachment; filename=\"q3_report.pdf\""), "{cd}");
    assert!(cd.contains("filename*=UTF-8''q3%20report.pdf"), "{cd}");
    assert_eq!(r.bytes().await.unwrap().as_ref(), &data[..]);

    // Overwrite, empty object, a big one.
    let o2 = t.upload("docs", "reports/2026/q3 report.pdf", b"v2", &[]).await;
    assert_eq!(o2["content_type"], Value::Null);
    assert_eq!(o2["metadata"], json!({}));
    let empty = t.upload("docs", "empty", b"", &[]).await;
    assert_eq!(empty["size"], 0);
    assert_eq!(t.download("docs", "empty").await.bytes().await.unwrap().len(), 0);
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 253) as u8).collect();
    let b = t.upload("docs", "big.bin", &big, &[]).await;
    assert_eq!(b["etag"], md5_hex(&big));
    assert_eq!(t.download("docs", "big.bin").await.bytes().await.unwrap().as_ref(), &big[..]);
    let r = t.download("docs", "empty").await;
    assert_eq!(r.headers()["content-type"], "application/octet-stream");
}

#[tokio::test]
async fn upload_and_download_errors() {
    let t = start().await;
    t.bucket("b-one").await;
    let up = |key: &str, bucket: &str| t.http.put(t.url(&format!("/v1/storage/buckets/{bucket}/upload?key={}", q(key))));

    // Unknown bucket, missing key, bad key, metadata too large, bad tags.
    let r = up("k", "nope").body("x").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let r = t.http.put(t.url("/v1/storage/buckets/b-one/upload")).body("x").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(r.json::<Value>().await.unwrap()["error"].as_str().unwrap().contains("key"));
    let r = up(&"k".repeat(1025), "b-one").body("x").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let r = up("k", "b-one").header("x-amz-meta-big", "v".repeat(2100)).body("x").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let r = up("k", "b-one").header("x-amz-tagging", "a=1&a=2").body("x").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    // No Content-Length (a chunked body) is refused.
    let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"abc"))]);
    let r = up("chunked", "b-one").body(reqwest::Body::wrap_stream(stream)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(r.json::<Value>().await.unwrap()["error"].as_str().unwrap().contains("Content-Length"));

    // A body that ends before its Content-Length commits nothing.
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let addr = t.api.trim_start_matches("http://").to_string();
        let mut s = tokio::net::TcpStream::connect(&addr).await.unwrap();
        s.write_all(format!("PUT /v1/storage/buckets/b-one/upload?key=short HTTP/1.1\r\nhost: {addr}\r\ncontent-length: 100\r\n\r\nonly-ten-b").as_bytes()).await.unwrap();
        s.shutdown().await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), s.read_to_end(&mut out)).await;
    }
    assert_eq!(t.err("GET", "/v1/storage/buckets/b-one/object?key=short", None).await, StatusCode::NOT_FOUND);

    // Download errors are JSON too.
    for (bucket, key, status) in [("b-one", "missing", StatusCode::NOT_FOUND), ("nope", "k", StatusCode::NOT_FOUND)] {
        let r = t.download(bucket, key).await;
        assert_eq!(r.status(), status);
        assert!(r.headers()["content-type"].to_str().unwrap().starts_with("application/json"));
        assert!(r.json::<Value>().await.unwrap()["error"].is_string());
    }
    let r = t.http.get(t.url("/v1/storage/buckets/b-one/download")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert_eq!(t.err("GET", "/v1/storage/buckets/b-one/object", None).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("GET", "/v1/storage/buckets/b-one/object?key=", None).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("GET", "/v1/storage/buckets/b-one/object?key=missing", None).await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn listing_with_prefixes_and_cursors() {
    let t = start().await;
    t.bucket("tree").await;
    for k in ["a.txt", "b.txt", "c.txt", "dir/x", "dir/y", "dir/sub/z", "other/w"] {
        t.upload("tree", k, k.as_bytes(), &[("content-type", "text/plain")]).await;
    }
    let top = t.ok("GET", "/v1/storage/buckets/tree/objects?delimiter=/", None).await;
    let keys: Vec<&str> = top["objects"].as_array().unwrap().iter().map(|o| o["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["a.txt", "b.txt", "c.txt"]);
    assert_eq!(top["prefixes"], json!(["dir/", "other/"]));
    assert_eq!(top["next_cursor"], Value::Null);
    let o = &top["objects"][0];
    assert_eq!(o["size"], 5);
    assert_eq!(o["etag"], md5_hex(b"a.txt"));
    assert_eq!(o["content_type"], "text/plain");
    assert!(o["last_modified"].as_i64().unwrap() > 0);
    assert!(o.get("metadata").is_none(), "summaries are short: {o}");

    let dir = t.ok("GET", "/v1/storage/buckets/tree/objects?prefix=dir/&delimiter=/", None).await;
    let keys: Vec<&str> = dir["objects"].as_array().unwrap().iter().map(|o| o["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["dir/x", "dir/y"]);
    assert_eq!(dir["prefixes"], json!(["dir/sub/"]));

    // No delimiter lists everything; walk it two at a time.
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let path = match &cursor {
            Some(c) => format!("/v1/storage/buckets/tree/objects?limit=2&cursor={c}"),
            None => "/v1/storage/buckets/tree/objects?limit=2".to_string(),
        };
        let page = t.ok("GET", &path, None).await;
        pages += 1;
        all.extend(page["objects"].as_array().unwrap().iter().map(|o| o["key"].as_str().unwrap().to_string()));
        match page["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(pages, 4);
    assert_eq!(all, ["a.txt", "b.txt", "c.txt", "dir/sub/z", "dir/x", "dir/y", "other/w"]);

    // Pages of prefixes resume after the prefix.
    let p1 = t.ok("GET", "/v1/storage/buckets/tree/objects?delimiter=/&limit=4", None).await;
    assert_eq!(p1["objects"].as_array().unwrap().len(), 3);
    assert_eq!(p1["prefixes"], json!(["dir/"]));
    let c = p1["next_cursor"].as_str().unwrap();
    let p2 = t.ok("GET", &format!("/v1/storage/buckets/tree/objects?delimiter=/&limit=4&cursor={c}"), None).await;
    assert_eq!(p2["objects"], json!([]));
    assert_eq!(p2["prefixes"], json!(["other/"]));
    assert_eq!(p2["next_cursor"], Value::Null);

    assert_eq!(t.err("GET", "/v1/storage/buckets/tree/objects?cursor=bad!", None).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("GET", "/v1/storage/buckets/nope/objects", None).await, StatusCode::NOT_FOUND);
    let empty = t.ok("GET", "/v1/storage/buckets/tree/objects?prefix=zzz", None).await;
    assert_eq!(empty, json!({ "objects": [], "prefixes": [], "next_cursor": null }));
}

#[tokio::test]
async fn deletes_trash_and_restore() {
    let t = start().await;
    t.bucket("bin").await;
    for k in ["k1", "k2", "k3", "logs/a", "logs/b", "keep/c"] {
        t.upload("bin", k, k.as_bytes(), &[]).await;
    }
    assert_eq!(t.ok("DELETE", "/v1/storage/buckets/bin/object?key=k1", None).await, json!({ "deleted": "k1" }));
    assert_eq!(t.err("DELETE", "/v1/storage/buckets/bin/object?key=k1", None).await, StatusCode::NOT_FOUND);
    assert_eq!(t.err("DELETE", "/v1/storage/buckets/bin/object", None).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("DELETE", "/v1/storage/buckets/nope/object?key=k1", None).await, StatusCode::NOT_FOUND);

    // Keys (missing ones don't count) and a prefix.
    let d = t.ok("POST", "/v1/storage/buckets/bin/delete", Some(json!({ "keys": ["k2", "k3", "never"] }))).await;
    assert_eq!(d, json!({ "deleted": 2 }));
    let d = t.ok("POST", "/v1/storage/buckets/bin/delete", Some(json!({ "prefix": "logs/" }))).await;
    assert_eq!(d, json!({ "deleted": 2 }));
    assert_eq!(t.err("POST", "/v1/storage/buckets/bin/delete", Some(json!({}))).await, StatusCode::BAD_REQUEST);
    let too_many: Vec<String> = (0..1001).map(|i| format!("k{i}")).collect();
    assert_eq!(t.err("POST", "/v1/storage/buckets/bin/delete", Some(json!({ "keys": too_many }))).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("POST", "/v1/storage/buckets/nope/delete", Some(json!({ "prefix": "" }))).await, StatusCode::NOT_FOUND);

    // Everything deleted is in the trash, newest state recoverable; page through it.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let path = match &cursor {
            Some(c) => format!("/v1/storage/buckets/bin/trash?limit=2&cursor={c}"),
            None => "/v1/storage/buckets/bin/trash?limit=2".to_string(),
        };
        let page = t.ok("GET", &path, None).await;
        for e in page["objects"].as_array().unwrap() {
            assert_eq!(e["size"], e["key"].as_str().unwrap().len());
            assert_eq!(e["etag"], md5_hex(e["key"].as_str().unwrap().as_bytes()));
            let (deleted, expires) = (e["deleted_at"].as_i64().unwrap(), e["expires_at"].as_i64().unwrap());
            assert_eq!(expires - deleted, 7 * 86_400_000);
            seen.push((e["key"].as_str().unwrap().to_string(), deleted));
        }
        match page["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    let keys: Vec<&str> = seen.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["k1", "k2", "k3", "logs/a", "logs/b"]);
    let logs = t.ok("GET", "/v1/storage/buckets/bin/trash?prefix=logs/", None).await;
    assert_eq!(logs["objects"].as_array().unwrap().len(), 2);
    assert_eq!(t.err("GET", "/v1/storage/buckets/bin/trash?cursor=bad!", None).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("GET", "/v1/storage/buckets/nope/trash", None).await, StatusCode::NOT_FOUND);

    let (key, at) = seen[0].clone();
    let restored = t.ok("POST", "/v1/storage/buckets/bin/trash/restore", Some(json!({ "key": key, "deleted_at": at }))).await;
    assert_eq!(restored["key"], "k1");
    assert_eq!(restored["etag"], md5_hex(b"k1"));
    assert_eq!(t.download("bin", "k1").await.bytes().await.unwrap().as_ref(), b"k1");
    assert_eq!(t.err("POST", "/v1/storage/buckets/bin/trash/restore", Some(json!({ "key": "k1", "deleted_at": at }))).await, StatusCode::NOT_FOUND);
    assert_eq!(t.err("POST", "/v1/storage/buckets/bin/trash/restore", Some(json!({ "key": "k2", "deleted_at": 1 }))).await, StatusCode::NOT_FOUND);
    assert_eq!(t.err("POST", "/v1/storage/buckets/bin/trash/restore", Some(json!({ "key": "k2" }))).await, StatusCode::BAD_REQUEST);

    // An empty prefix empties the bucket.
    let d = t.ok("POST", "/v1/storage/buckets/bin/delete", Some(json!({ "prefix": "" }))).await;
    assert_eq!(d, json!({ "deleted": 2 }));
    assert_eq!(t.ok("GET", "/v1/storage/buckets/bin/objects", None).await["objects"], json!([]));
    let b = t.ok("GET", "/v1/storage/buckets/bin", None).await;
    assert_eq!((b["objects"].as_u64(), b["bytes"].as_u64()), (Some(0), Some(0)));
    assert!(b["trash_bytes"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn presigned_urls_work_against_the_s3_api() {
    let t = start().await;
    t.bucket("share").await;
    t.upload("share", "dir/hello world.txt", b"hi there", &[("content-type", "text/plain")]).await;

    let p = t.ok("POST", "/v1/storage/buckets/share/presign", Some(json!({ "key": "dir/hello world.txt" }))).await;
    assert_eq!(p["method"], "GET");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    assert!((p["expires_at"].as_i64().unwrap() - now - 3_600_000).abs() < 60_000);
    let url = p["url"].as_str().unwrap();
    assert!(url.starts_with(&format!("{}/share/dir/hello%20world.txt?", t.s3)), "{url}");
    let r = t.http.get(url).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"hi there");
    // A tampered URL is refused.
    let r = t.http.get(url.replace("hello%20world", "other")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);

    // PUT, any Content-Type.
    let p = t.ok("POST", "/v1/storage/buckets/share/presign", Some(json!({ "key": "upload.json", "method": "put", "expires_s": 60 }))).await;
    assert_eq!(p["method"], "PUT");
    let r = t.http.put(p["url"].as_str().unwrap()).header("content-type", "application/json").body("{\"a\":1}").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text().await.unwrap());
    let o = t.ok("GET", "/v1/storage/buckets/share/object?key=upload.json", None).await;
    assert_eq!(o["content_type"], "application/json");
    assert_eq!(o["size"], 7);

    assert_eq!(t.err("POST", "/v1/storage/buckets/share/presign", Some(json!({ "key": "k", "method": "DELETE" }))).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("POST", "/v1/storage/buckets/share/presign", Some(json!({ "key": "k", "expires_s": 0 }))).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("POST", "/v1/storage/buckets/share/presign", Some(json!({ "key": "k", "expires_s": 604_801 }))).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("POST", "/v1/storage/buckets/share/presign", Some(json!({ "key": "" }))).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("POST", "/v1/storage/buckets/nope/presign", Some(json!({ "key": "k" }))).await, StatusCode::NOT_FOUND);
    // The longest allowed expiry works.
    t.ok("POST", "/v1/storage/buckets/share/presign", Some(json!({ "key": "k", "expires_s": 604_800 }))).await;
}

#[tokio::test]
async fn presigning_key_survives_restarts_and_is_hidden() {
    let dir = tempfile::tempdir().unwrap();
    let backend: Arc<dyn s3_store::Backend> = Arc::new(s3_store::LocalBackend::new(&dir.path().join("objects")).unwrap());
    let store = Store::open(StoreConfig::new(dir.path().join("meta1")).manual(), backend.clone()).await.unwrap();
    let c1 = Control::new(store.clone(), ControlConfig::local("http://127.0.0.1:1")).await.unwrap();
    store.create_bucket("b-1", Default::default()).await.unwrap();
    let (u1, _) = c1.presign("b-1", "k", "GET", 60).unwrap();
    assert!(c1.keys().unwrap().is_empty());
    assert!(store.keys().unwrap().iter().any(|k| k.id == s3_control::control::CONTROL_KEY_ID));
    let secret = |s: &Store| s.access_key(s3_control::control::CONTROL_KEY_ID).unwrap().unwrap().secret;
    let first = secret(&store);
    store.close().await;
    // Rebuilt from object storage: the same secret, so earlier URLs stay valid.
    let store2 = Store::open(StoreConfig::new(dir.path().join("meta2")).manual(), backend).await.unwrap();
    let c2 = Control::new(store2.clone(), ControlConfig::local("http://127.0.0.1:1")).await.unwrap();
    assert_eq!(first, secret(&store2));
    let (u2, _) = c2.presign("b-1", "k", "GET", 60).unwrap();
    assert!(u1.contains(s3_control::control::CONTROL_KEY_ID) && u2.contains(s3_control::control::CONTROL_KEY_ID));
    assert!(matches!(c2.delete_key(s3_control::control::CONTROL_KEY_ID).await, Err(s3_control::ControlError::NotFound(_))));
}

#[tokio::test]
async fn access_keys_work_through_the_s3_api() {
    let t = start().await;
    t.bucket("team-a").await;
    t.bucket("team-b").await;
    t.upload("team-a", "readme", b"A", &[]).await;
    t.upload("team-b", "readme", b"B", &[]).await;

    let k = t.ok("POST", "/v1/storage/keys", Some(json!({ "name": "ci", "permission": "read", "buckets": ["team-a"] }))).await;
    let (id, secret) = (k["id"].as_str().unwrap().to_string(), k["secret_access_key"].as_str().unwrap().to_string());
    assert_eq!(id.len(), 20);
    assert!(id.starts_with("RK"));
    assert_eq!(secret.len(), 40);
    assert_eq!(k["name"], "ci");
    assert_eq!(k["permission"], "read");
    assert_eq!(k["buckets"], json!(["team-a"]));
    assert!(k["created_at"].as_i64().unwrap() > 0);
    assert_eq!(k["last_used_at"], Value::Null);

    // Listed without the secret, and without the control plane's own key.
    let keys = t.ok("GET", "/v1/storage/keys", None).await;
    assert_eq!(keys.as_array().unwrap().len(), 1);
    assert_eq!(keys[0]["id"], id);
    assert!(keys[0].get("secret_access_key").is_none() && keys[0].get("secret").is_none());

    let s3 = t.sdk(&id, &secret);
    let got = s3.get_object().bucket("team-a").key("readme").send().await.unwrap();
    assert_eq!(got.body.collect().await.unwrap().into_bytes().as_ref(), b"A");
    let buckets = s3.list_buckets().send().await.unwrap();
    let names: Vec<_> = buckets.buckets().iter().filter_map(|b| b.name()).collect();
    assert_eq!(names, ["team-a"]);
    assert!(s3.get_object().bucket("team-b").key("readme").send().await.is_err(), "scoped to team-a");
    assert!(s3.put_object().bucket("team-a").key("x").body(b"x".to_vec().into()).send().await.is_err(), "read-only");

    let w = t.ok("POST", "/v1/storage/keys", Some(json!({ "name": "writer", "permission": "write" }))).await;
    let ws = t.sdk(w["id"].as_str().unwrap(), w["secret_access_key"].as_str().unwrap());
    ws.put_object().bucket("team-b").key("new").body(b"via sdk".to_vec().into()).send().await.unwrap();
    assert_eq!(t.download("team-b", "new").await.bytes().await.unwrap().as_ref(), b"via sdk");
    assert!(ws.create_bucket().bucket("not-allowed").send().await.is_err(), "write keys can't create buckets");
    let a = t.ok("POST", "/v1/storage/keys", Some(json!({ "name": "root", "permission": "admin" }))).await;
    t.sdk(a["id"].as_str().unwrap(), a["secret_access_key"].as_str().unwrap()).create_bucket().bucket("made-by-sdk").send().await.unwrap();
    assert_eq!(t.ok("GET", "/v1/storage/buckets/made-by-sdk", None).await["name"], "made-by-sdk");

    // Use is recorded.
    let keys = t.ok("GET", "/v1/storage/keys", None).await;
    let ci = keys.as_array().unwrap().iter().find(|k| k["id"] == id.as_str()).unwrap().clone();
    assert!(ci["last_used_at"].as_i64().is_some(), "{ci}");

    // Errors.
    assert_eq!(t.err("POST", "/v1/storage/keys", Some(json!({ "name": "x", "permission": "owner" }))).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("POST", "/v1/storage/keys", Some(json!({ "name": "", "permission": "read" }))).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("POST", "/v1/storage/keys", Some(json!({ "name": "x", "permission": "read", "buckets": [] }))).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("POST", "/v1/storage/keys", Some(json!({ "name": "x", "permission": "read", "buckets": ["Not A Bucket"] }))).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("POST", "/v1/storage/keys", Some(json!({ "permission": "read" }))).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("DELETE", "/v1/storage/keys/RKNOSUCHKEY000000000", None).await, StatusCode::NOT_FOUND);
    assert_eq!(t.err("DELETE", &format!("/v1/storage/keys/{}", s3_control::control::CONTROL_KEY_ID), None).await, StatusCode::NOT_FOUND);

    // A deleted key stops working.
    assert_eq!(t.ok("DELETE", &format!("/v1/storage/keys/{id}"), None).await, json!({ "deleted": id }));
    assert!(s3.get_object().bucket("team-a").key("readme").send().await.is_err());
    assert_eq!(t.ok("GET", "/v1/storage/keys", None).await.as_array().unwrap().len(), 2);
}

/// A clock tests move by hand.
fn manual_clock(start: i64) -> (Clock, Arc<AtomicI64>) {
    let t = Arc::new(AtomicI64::new(start));
    let c = t.clone();
    (Arc::new(move || c.load(Ordering::SeqCst)), t)
}

const DAY: i64 = 86_400_000;
const MONTH: i64 = 730 * 3_600_000;

#[tokio::test]
async fn usage_and_cost_math() {
    // Noon UTC, so the day doesn't change under the test.
    let t0 = 20_000 * DAY + DAY / 2;
    let (clock, now) = manual_clock(t0);
    let t = start_with(|c| c.clock = clock, |_| {}).await;
    t.bucket("metered").await;
    t.bucket("idle").await;
    let b = t.store.bucket("metered").unwrap();
    // 1000 bytes stored for 1000 months (all of it billed today) = 1 GB-month = $0.023.
    t.upload("metered", "obj", &[7u8; 1000], &[]).await;
    let day = t0 + 1000 * MONTH;
    now.store(day - day.rem_euclid(DAY) + DAY / 2, Ordering::SeqCst);
    let elapsed = now.load(Ordering::SeqCst) - t0;
    for _ in 0..999_999 {
        t.store.meter.request(&b.id, s3_store::Class::A);
    }
    for _ in 0..2_500_000 {
        t.store.meter.request(&b.id, s3_store::Class::B);
    }
    t.store.meter.request(&b.id, s3_store::Class::Free);
    t.store.meter.egress(&b.id, 2_000_000_000, false);
    t.store.meter.egress(&b.id, 5_000_000_000, true);
    t.store.flush_meter().unwrap();

    let u = t.ok("GET", "/v1/storage/buckets/metered/usage?days=1", None).await;
    assert_eq!(u["bucket"], "metered");
    assert_eq!(u["days"], 1);
    // The upload itself was one class A request.
    assert_eq!(u["class_a"], 1_000_000);
    assert_eq!(u["class_b"], 2_500_000);
    assert_eq!(u["bytes_in"], 1000);
    assert_eq!(u["egress_internet_bytes"], 2_000_000_000u64);
    assert_eq!(u["egress_internal_bytes"], 5_000_000_000u64);
    assert_eq!(u["storage_bytes"], 1000);
    assert_eq!(u["objects"], 1);
    let gb_months = 1000.0 * elapsed as f64 / (1e9 * MONTH as f64);
    assert!((u["gb_months"].as_f64().unwrap() - gb_months).abs() < 1e-9);
    let c = &u["cost"];
    let close = |v: &Value, want: f64| assert!((v.as_f64().unwrap() - want).abs() < 2e-6, "{v} != {want}");
    close(&c["storage_usd"], gb_months * 0.023);
    close(&c["class_a_usd"], 5.0);
    close(&c["class_b_usd"], 1.0);
    close(&c["egress_usd"], 0.18);
    close(&c["total_usd"], gb_months * 0.023 + 6.18);
    // Traffic at today's rate for 30 days, plus 1000 bytes for a month.
    close(&c["projected_month_usd"], 6.18 * 30.0 + 1000.0 / 1e9 * 0.023);
    let daily = u["daily"].as_array().unwrap();
    assert_eq!(daily.len(), 1);
    assert_eq!(daily[0]["day"], s3_store::day_of(now.load(Ordering::SeqCst)));
    assert_eq!(daily[0]["class_a"], 1_000_000);
    assert_eq!(daily[0]["egress_bytes"], 7_000_000_000u64);
    close(&daily[0]["usd"], gb_months * 0.023 + 6.18);
    // All of the byte-ms landed today (half a day so far).
    assert_eq!(daily[0]["storage_bytes"], (1000.0 * elapsed as f64 / (DAY / 2) as f64) as u64);

    // A wider window sees the older day too; the default is 30 days.
    let wide = t.ok("GET", "/v1/storage/buckets/metered/usage", None).await;
    assert_eq!(wide["days"], 30);
    let clamped = t.ok("GET", "/v1/storage/buckets/metered/usage?days=0", None).await;
    assert_eq!(clamped["days"], 1);
    assert_eq!(t.ok("GET", "/v1/storage/buckets/metered/usage?days=5000", None).await["days"], 366);
    assert_eq!(t.err("GET", "/v1/storage/buckets/nope/usage", None).await, StatusCode::NOT_FOUND);

    // Every bucket, with a summed daily series.
    let all = t.ok("GET", "/v1/storage/usage?days=1", None).await;
    assert_eq!(all["days"], 1);
    let names: Vec<&str> = all["buckets"].as_array().unwrap().iter().map(|b| b["bucket"].as_str().unwrap()).collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"metered") && names.contains(&"idle"));
    let idle = all["buckets"].as_array().unwrap().iter().find(|b| b["bucket"] == "idle").unwrap();
    assert_eq!(idle["class_a"], 0);
    assert_eq!(idle["cost"]["total_usd"], 0.0);
    close(&all["cost"]["total_usd"], c["total_usd"].as_f64().unwrap());
    assert_eq!(all["daily"].as_array().unwrap().len(), 1);
    assert_eq!(all["daily"][0]["class_b"], 2_500_000);

    // Deleted buckets still count in the total.
    t.ok("DELETE", "/v1/storage/buckets/metered?force=true", None).await;
    let after = t.ok("GET", "/v1/storage/usage?days=1", None).await;
    assert_eq!(after["buckets"].as_array().unwrap().len(), 1);
    close(&after["cost"]["total_usd"], c["total_usd"].as_f64().unwrap());
}

#[tokio::test]
async fn control_plane_traffic_is_metered() {
    let t = start().await;
    t.bucket("meter").await;
    let id = t.store.bucket("meter").unwrap().id;
    t.upload("meter", "a", b"12345", &[]).await;
    t.download("meter", "a").await.bytes().await.unwrap();
    // Let the download's stream drop.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let c = t.store.meter.pending(&id);
    assert_eq!((c.class_a, c.class_b, c.bytes_in, c.egress_internal, c.egress_internet), (1, 1, 5, 5, 0));
}

#[tokio::test]
async fn a_closed_store_answers_503() {
    let t = start().await;
    t.bucket("x-1").await;
    t.store.close().await;
    assert_eq!(t.err("GET", "/v1/storage/buckets", None).await, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(t.err("POST", "/v1/storage/buckets", Some(json!({ "name": "x-2" }))).await, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn cors_allows_the_dashboard() {
    let t = start().await;
    let r = t
        .http
        .request(reqwest::Method::OPTIONS, t.url("/v1/storage/buckets"))
        .header("origin", "http://localhost:3000")
        .header("access-control-request-method", "POST")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    assert!(r.headers().contains_key("access-control-allow-origin"));
}

#[tokio::test]
async fn usage_all_sums_buckets_per_day() {
    let t0 = 20_100 * DAY + DAY / 2;
    let (clock, now) = manual_clock(t0);
    let t = start_with(|c| c.clock = clock, |_| {}).await;
    t.bucket("one-1").await;
    t.bucket("two-2").await;
    let (one, two) = (t.store.bucket("one-1").unwrap().id, t.store.bucket("two-2").unwrap().id);
    let meter = |id: &str, a: u64, b: u64, out: u64| {
        for _ in 0..a {
            t.store.meter.request(id, s3_store::Class::A);
        }
        for _ in 0..b {
            t.store.meter.request(id, s3_store::Class::B);
        }
        t.store.meter.egress(id, out, false);
    };
    // Day one: both buckets. Day two: only the first.
    meter(&one, 10, 100, 1_000_000_000);
    meter(&two, 30, 300, 3_000_000_000);
    t.store.flush_meter().unwrap();
    now.store(t0 + DAY, Ordering::SeqCst);
    meter(&one, 5, 50, 0);
    t.store.flush_meter().unwrap();

    let all = t.ok("GET", "/v1/storage/usage?days=2", None).await;
    let daily = all["daily"].as_array().unwrap();
    let days: Vec<&str> = daily.iter().map(|d| d["day"].as_str().unwrap()).collect();
    assert_eq!(days, [s3_store::day_of(t0), s3_store::day_of(t0 + DAY)]);
    assert_eq!((daily[0]["class_a"].as_u64(), daily[0]["class_b"].as_u64(), daily[0]["egress_bytes"].as_u64()), (Some(40), Some(400), Some(4_000_000_000)));
    assert_eq!((daily[1]["class_a"].as_u64(), daily[1]["class_b"].as_u64(), daily[1]["egress_bytes"].as_u64()), (Some(5), Some(50), Some(0)));
    // Each summed day costs what the buckets' own rows for that day cost.
    for (i, d) in daily.iter().enumerate() {
        let per_bucket: f64 = all["buckets"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b["daily"].as_array().unwrap().iter().find(|x| x["day"] == d["day"]))
            .map(|x| x["usd"].as_f64().unwrap())
            .sum();
        assert!((d["usd"].as_f64().unwrap() - per_bucket).abs() < 2e-6, "day {i}: {} != {per_bucket}", d["usd"]);
    }
    // $5/M class A, $0.40/M class B, $0.09/GB to the internet.
    let want = 45.0 / 1e6 * 5.0 + 450.0 / 1e6 * 0.4 + 4.0 * 0.09;
    assert!((all["cost"]["total_usd"].as_f64().unwrap() - want).abs() < 2e-6, "{}", all["cost"]);
    // The total is the sum of the buckets.
    let sum: f64 = all["buckets"].as_array().unwrap().iter().map(|b| b["cost"]["total_usd"].as_f64().unwrap()).sum();
    assert!((all["cost"]["total_usd"].as_f64().unwrap() - sum).abs() < 2e-6);
    // A one-day window only sees today.
    let today = t.ok("GET", "/v1/storage/usage?days=1", None).await;
    assert_eq!(today["daily"].as_array().unwrap().len(), 1);
    assert_eq!(today["daily"][0]["class_a"], 5);
    // Nothing metered at all: empty series, zero cost.
    let fresh = start().await;
    let v = fresh.ok("GET", "/v1/storage/usage", None).await;
    assert_eq!((v["days"].as_i64(), v["daily"].clone(), v["buckets"].clone()), (Some(30), json!([]), json!([])));
    assert_eq!(v["cost"]["total_usd"], 0.0);
}

#[tokio::test]
async fn upload_size_limits() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let t = start().await;
    t.bucket("limits").await;
    let addr = t.api.trim_start_matches("http://").to_string();
    // Refused from the headers alone, before any body is read.
    for (len, want) in [("5368709121", "5 GiB"), ("abc", "Content-Length"), ("-1", "Content-Length")] {
        let mut s = tokio::net::TcpStream::connect(&addr).await.unwrap();
        s.write_all(format!("PUT /v1/storage/buckets/limits/upload?key=k HTTP/1.1\r\nhost: {addr}\r\ncontent-length: {len}\r\nconnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let mut out = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(10), s.read_to_end(&mut out)).await.expect("answered without the body").unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("HTTP/1.1 400"), "{len}: {text}");
        if len != "abc" && len != "-1" {
            assert!(text.contains(want), "{len}: {text}");
        }
    }
    assert_eq!(t.err("GET", "/v1/storage/buckets/limits/object?key=k", None).await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn restore_puts_the_current_version_in_the_trash() {
    let t = start().await;
    t.bucket("docs-r").await;
    t.upload("docs-r", "page", b"old", &[]).await;
    t.upload("docs-r", "page", b"new", &[]).await;
    let trash = t.ok("GET", "/v1/storage/buckets/docs-r/trash", None).await;
    let e = &trash["objects"][0];
    assert_eq!(e["etag"], md5_hex(b"old"));
    let restored = t.ok("POST", "/v1/storage/buckets/docs-r/trash/restore", Some(json!({ "key": "page", "deleted_at": e["deleted_at"] }))).await;
    assert_eq!(restored["etag"], md5_hex(b"old"));
    assert_eq!(t.download("docs-r", "page").await.bytes().await.unwrap().as_ref(), b"old");
    // The version it replaced is now the one in the trash, and can come back too.
    let trash = t.ok("GET", "/v1/storage/buckets/docs-r/trash", None).await;
    let entries = trash["objects"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["etag"], md5_hex(b"new"));
    t.ok("POST", "/v1/storage/buckets/docs-r/trash/restore", Some(json!({ "key": "page", "deleted_at": entries[0]["deleted_at"] }))).await;
    assert_eq!(t.download("docs-r", "page").await.bytes().await.unwrap().as_ref(), b"new");

    // With recover_days 0 nothing goes to the trash.
    t.ok("POST", "/v1/storage/buckets", Some(json!({ "name": "no-trash", "recover_days": 0 }))).await;
    t.upload("no-trash", "x", b"1", &[]).await;
    t.ok("DELETE", "/v1/storage/buckets/no-trash/object?key=x", None).await;
    assert_eq!(t.ok("GET", "/v1/storage/buckets/no-trash/trash", None).await, json!({ "objects": [], "next_cursor": null }));
    assert_eq!(t.ok("GET", "/v1/storage/buckets/no-trash", None).await["trash_bytes"], 0);
}

#[tokio::test]
async fn presigned_urls_for_unusual_keys() {
    let t = start().await;
    t.bucket("odd-keys").await;
    for key in ["a+b=c&d.txt", "ünïcödé/日本語.txt", "spaces  and%20percent", "trailing/", "~tilde*star'quote(paren)"] {
        let p = t.ok("POST", "/v1/storage/buckets/odd-keys/presign", Some(json!({ "key": key, "method": "PUT" }))).await;
        let r = t.http.put(p["url"].as_str().unwrap()).body(key.as_bytes().to_vec()).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "PUT {key}: {}", r.text().await.unwrap());
        let p = t.ok("POST", "/v1/storage/buckets/odd-keys/presign", Some(json!({ "key": key }))).await;
        let r = t.http.get(p["url"].as_str().unwrap()).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "GET {key}");
        assert_eq!(r.bytes().await.unwrap().as_ref(), key.as_bytes(), "{key}");
        // The control plane sees the same key.
        assert_eq!(t.ok("GET", &format!("/v1/storage/buckets/odd-keys/object?key={}", q(key)), None).await["key"], key);
    }
    // An expired URL is refused.
    let (clock, now) = manual_clock(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64);
    let t = start_with(|c| c.clock = clock, |_| {}).await;
    t.bucket("expiring").await;
    t.upload("expiring", "k", b"v", &[]).await;
    let p = t.ok("POST", "/v1/storage/buckets/expiring/presign", Some(json!({ "key": "k", "expires_s": 60 }))).await;
    let url = p["url"].as_str().unwrap();
    assert_eq!(t.http.get(url).send().await.unwrap().status(), StatusCode::OK);
    // Valid for a minute; two minutes later it isn't.
    now.fetch_add(120_000, Ordering::SeqCst);
    assert_eq!(t.http.get(url).send().await.unwrap().status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn lost_blobs_are_json_500s_and_show_in_the_scrub() {
    let (clock, now) = manual_clock(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64);
    let t = start_with(|c| c.clock = clock, |_| {}).await;
    t.bucket("scrubbed").await;
    t.upload("scrubbed", "gone", b"bytes that vanish", &[]).await;
    t.upload("scrubbed", "kept", b"still here", &[]).await;
    let meta = t.store.get_object("scrubbed", "gone").unwrap();
    t.store.backend().delete(&s3_store::blob_key(&meta.blob_id)).await.unwrap();

    let r = t.download("scrubbed", "gone").await;
    assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(r.headers()["content-type"].to_str().unwrap().starts_with("application/json"));
    assert!(r.json::<Value>().await.unwrap()["error"].as_str().unwrap().contains("missing"));
    assert_eq!(t.download("scrubbed", "kept").await.bytes().await.unwrap().as_ref(), b"still here");

    // The scrub skips blobs from its last minute (they may still be in flight).
    now.fetch_add(120_000, Ordering::SeqCst);
    t.store.scrub().await.unwrap();
    let scrub = &t.ok("GET", "/v1/storage", None).await["scrub"];
    assert_eq!((scrub["missing"].as_u64(), scrub["mismatched"].as_u64()), (Some(1), Some(0)));
    assert!(scrub["last_at"].as_i64().unwrap() > 0);
    // The scrub only reports: the metadata is untouched.
    assert_eq!(t.ok("GET", "/v1/storage/buckets/scrubbed/object?key=gone", None).await["size"], 17);
}

#[tokio::test]
async fn list_limits_are_clamped() {
    let t = start().await;
    t.bucket("many").await;
    for i in 0..3 {
        t.upload("many", &format!("k{i}"), b"x", &[]).await;
    }
    let one = t.ok("GET", "/v1/storage/buckets/many/objects?limit=0", None).await;
    assert_eq!(one["objects"].as_array().unwrap().len(), 1);
    assert!(one["next_cursor"].is_string());
    let all = t.ok("GET", "/v1/storage/buckets/many/objects?limit=100000", None).await;
    assert_eq!(all["objects"].as_array().unwrap().len(), 3);
    // Malformed query parameters are JSON errors too.
    assert_eq!(t.err("GET", "/v1/storage/buckets/many/objects?limit=-1", None).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("GET", "/v1/storage/buckets/many/usage?days=soon", None).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("DELETE", "/v1/storage/buckets/many?force=maybe", None).await, StatusCode::BAD_REQUEST);
    t.ok("DELETE", "/v1/storage/buckets/many/object?key=k0", None).await;
    t.ok("DELETE", "/v1/storage/buckets/many/object?key=k1", None).await;
    let trash = t.ok("GET", "/v1/storage/buckets/many/trash?limit=0", None).await;
    assert_eq!(trash["objects"].as_array().unwrap().len(), 1);
    assert!(trash["next_cursor"].is_string());
}

#[tokio::test]
async fn bucket_settings_are_validated() {
    let t = start().await;
    t.bucket("settings").await;
    let bad = [
        json!({ "lifecycle": [{ "id": "x", "prefix": "", "enabled": true, "expire_days": 0, "abort_multipart_days": null }] }),
        json!({ "lifecycle": [{ "id": "x", "prefix": "", "enabled": true, "expire_days": null, "abort_multipart_days": null }] }),
        json!({ "cors": [{ "allowed_origins": ["*"], "allowed_methods": ["FETCH"], "allowed_headers": [], "expose_headers": [], "max_age_s": null }] }),
        json!({ "public": "yes" }),
        json!({ "recover_days": -1 }),
    ];
    for body in bad {
        assert_eq!(t.err("PATCH", "/v1/storage/buckets/settings", Some(body.clone())).await, StatusCode::BAD_REQUEST, "{body}");
    }
    // Nothing changed.
    let b = t.ok("GET", "/v1/storage/buckets/settings", None).await;
    assert_eq!((b["cors"].clone(), b["lifecycle"].clone(), b["public"].clone()), (json!([]), json!([]), json!(false)));
    // An empty PATCH changes nothing and succeeds.
    assert_eq!(t.ok("PATCH", "/v1/storage/buckets/settings", Some(json!({}))).await["recover_days"], 7);
    // Branches inherit the parent's settings at creation.
    t.ok("PATCH", "/v1/storage/buckets/settings", Some(json!({ "public": true, "recover_days": 2 }))).await;
    let br = t.ok("POST", "/v1/storage/buckets/settings/branches", Some(json!({ "name": "settings-br" }))).await;
    assert_eq!((br["public"].as_bool(), br["recover_days"].as_u64()), (Some(true), Some(2)));
}

#[tokio::test]
async fn versioning_versions_and_object_lock() {
    let t = start().await;
    let b = t.bucket("ver").await;
    assert_eq!((b["versioning"].clone(), b["object_lock"].clone(), b["versions"].clone(), b["version_bytes"].clone()), (Value::Null, Value::Null, json!(0), json!(0)));
    let b = t.ok("PATCH", "/v1/storage/buckets/ver", Some(json!({ "versioning": "Enabled" }))).await;
    assert_eq!(b["versioning"], "Enabled");
    assert_eq!(t.err("PATCH", "/v1/storage/buckets/ver", Some(json!({ "versioning": "On" }))).await, StatusCode::BAD_REQUEST);
    let v1 = t.upload("ver", "k", b"one", &[]).await["version_id"].as_str().unwrap().to_string();
    let v2 = t.upload("ver", "k", b"twotwo", &[]).await["version_id"].as_str().unwrap().to_string();
    assert_ne!(v1, v2);
    let b = t.ok("GET", "/v1/storage/buckets/ver", None).await;
    assert_eq!((b["objects"].as_u64(), b["bytes"].as_u64(), b["versions"].as_u64(), b["version_bytes"].as_u64()), (Some(1), Some(6), Some(1), Some(3)));
    // Noncurrent versions are stored bytes.
    assert_eq!(t.ok("GET", "/v1/storage/buckets/ver/usage", None).await["storage_bytes"], 9);

    // A version: its info and its bytes.
    let i = t.ok("GET", &format!("/v1/storage/buckets/ver/object?key=k&version_id={v1}"), None).await;
    assert_eq!((i["version_id"].as_str(), i["size"].as_u64()), (Some(v1.as_str()), Some(3)));
    let r = t.http.get(t.url(&format!("/v1/storage/buckets/ver/download?key=k&version_id={v1}"))).send().await.unwrap();
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"one");
    assert_eq!(t.ok("GET", "/v1/storage/buckets/ver/object?key=k", None).await["version_id"], v2.as_str());
    for bad in ["nope", "3HL4kqtJlcpXroDTDmJ+rmSpXd3dIbrHY"] {
        assert_eq!(t.err("GET", &format!("/v1/storage/buckets/ver/object?key=k&version_id={}", q(bad)), None).await, StatusCode::BAD_REQUEST);
    }
    assert_eq!(t.err("GET", "/v1/storage/buckets/ver/object?key=k&version_id=0000000000000000aaaaaaaaaaaaaaaa", None).await, StatusCode::NOT_FOUND);

    // A delete without a version adds a marker; one for a key that isn't there adds nothing.
    let d = t.ok("DELETE", "/v1/storage/buckets/ver/object?key=k", None).await;
    assert_eq!((d["deleted"].as_str(), d["delete_marker"].as_bool()), (Some("k"), Some(true)));
    let marker = d["version_id"].as_str().unwrap().to_string();
    assert_eq!(t.err("DELETE", "/v1/storage/buckets/ver/object?key=never", None).await, StatusCode::NOT_FOUND);
    assert_eq!(t.err("GET", "/v1/storage/buckets/ver/object?key=k", None).await, StatusCode::NOT_FOUND);

    // The versions listing, whole and a page at a time.
    let l = t.ok("GET", "/v1/storage/buckets/ver/versions", None).await;
    assert_eq!(l["next_cursor"], Value::Null);
    let vs = l["versions"].as_array().unwrap();
    let ids: Vec<&str> = vs.iter().map(|v| v["version_id"].as_str().unwrap()).collect();
    assert_eq!(ids, [marker.as_str(), v2.as_str(), v1.as_str()]);
    assert_eq!((vs[0]["is_latest"].as_bool(), vs[0]["delete_marker"].as_bool()), (Some(true), Some(true)));
    assert_eq!((vs[1]["key"].as_str(), vs[1]["size"].as_u64(), vs[1]["etag"].as_str(), vs[1]["is_latest"].as_bool()), (Some("k"), Some(6), Some(md5_hex(b"twotwo").as_str()), Some(false)));
    assert!(vs[2]["last_modified"].as_i64().is_some());
    let mut paged = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let path = match &cursor {
            Some(c) => format!("/v1/storage/buckets/ver/versions?limit=1&prefix=k&cursor={c}"),
            None => "/v1/storage/buckets/ver/versions?limit=1&prefix=k".to_string(),
        };
        let page = t.ok("GET", &path, None).await;
        paged.extend(page["versions"].as_array().unwrap().iter().map(|v| v["version_id"].as_str().unwrap().to_string()));
        match page["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(paged, ids);
    assert_eq!(t.err("GET", "/v1/storage/buckets/ver/versions?cursor=%25%25", None).await, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("GET", "/v1/storage/buckets/nope/versions", None).await, StatusCode::NOT_FOUND);
    assert!(t.ok("GET", "/v1/storage/buckets/ver/versions?prefix=zzz", None).await["versions"].as_array().unwrap().is_empty());

    // A version deleted for good goes to the trash, with its id.
    let d = t.ok("DELETE", &format!("/v1/storage/buckets/ver/object?key=k&version_id={v1}"), None).await;
    assert_eq!((d["version_id"].as_str(), d["delete_marker"].as_bool()), (Some(v1.as_str()), Some(false)));
    let trash = t.ok("GET", "/v1/storage/buckets/ver/trash", None).await;
    assert_eq!(trash["objects"][0]["version_id"], v1.as_str());
    assert_eq!(t.err("DELETE", &format!("/v1/storage/buckets/ver/object?key=k&version_id={v1}"), None).await, StatusCode::NOT_FOUND);
    // Restored behind the newer versions.
    let restored = t.ok("POST", "/v1/storage/buckets/ver/trash/restore", Some(json!({ "key": "k", "deleted_at": trash["objects"][0]["deleted_at"] }))).await;
    assert_eq!(restored["version_id"], v1.as_str());
    assert_eq!(t.ok("GET", "/v1/storage/buckets/ver/versions", None).await["versions"].as_array().unwrap().len(), 3);
    // Deleting the marker brings v2 back.
    t.ok("DELETE", &format!("/v1/storage/buckets/ver/object?key=k&version_id={marker}"), None).await;
    assert_eq!(t.ok("GET", "/v1/storage/buckets/ver/object?key=k", None).await["version_id"], v2.as_str());

    // Object Lock: at creation, it turns versioning on; locked versions can't be deleted for good.
    let l = t.ok("POST", "/v1/storage/buckets", Some(json!({ "name": "locked", "object_lock": true }))).await;
    assert_eq!((l["versioning"].as_str(), l["object_lock"].clone()), (Some("Enabled"), json!({ "mode": null, "days": null, "years": null })));
    assert_eq!(t.err("PATCH", "/v1/storage/buckets/locked", Some(json!({ "versioning": "Suspended" }))).await, StatusCode::CONFLICT);
    let lv = t.upload("locked", "k", b"x", &[]).await["version_id"].as_str().unwrap().to_string();
    t.store.put_legal_hold("locked", "k", Some(&lv), true).await.unwrap();
    assert!(t.ok("GET", "/v1/storage/buckets/locked/object?key=k", None).await["lock"]["legal_hold"].as_bool().unwrap());
    assert_eq!(t.err("DELETE", &format!("/v1/storage/buckets/locked/object?key=k&version_id={lv}"), None).await, StatusCode::CONFLICT);
    assert_eq!(t.err("DELETE", "/v1/storage/buckets/locked?force=true", None).await, StatusCode::CONFLICT);
    assert_eq!(t.ok("POST", "/v1/storage/buckets/locked/delete", Some(json!({ "prefix": "" }))).await["deleted"], 1, "a delete marker is fine");
    assert_eq!(t.ok("GET", "/v1/storage/buckets/locked/versions", None).await["versions"].as_array().unwrap().len(), 2);
    t.store.put_legal_hold("locked", "k", Some(&lv), false).await.unwrap();
    t.ok("DELETE", "/v1/storage/buckets/locked?force=true", None).await;

    // The lifecycle JSON takes the full model; the old shape keeps working (other tests).
    let rules = json!([{
        "id": "all", "prefix": "logs/", "enabled": true, "expire_days": null, "abort_multipart_days": null,
        "tags": { "tier": "tmp" }, "size_greater_than": 10, "size_less_than": null, "expire_at": null, "expire_delete_markers": false,
        "noncurrent_days": 30, "newer_noncurrent_versions": 2,
        "transitions": [{ "days": 30, "date": null, "storage_class": "GLACIER" }],
        "noncurrent_transitions": [{ "noncurrent_days": 10, "newer_noncurrent_versions": null, "storage_class": "DEEP_ARCHIVE" }]
    }]);
    let b = t.ok("PATCH", "/v1/storage/buckets/ver", Some(json!({ "lifecycle": rules }))).await;
    assert_eq!(b["lifecycle"], rules);
    let bad = json!({ "lifecycle": [{ "id": "x", "prefix": "", "enabled": true, "transitions": [{ "days": 1, "storage_class": "NOPE" }] }] });
    assert_eq!(t.err("PATCH", "/v1/storage/buckets/ver", Some(bad)).await, StatusCode::BAD_REQUEST);
    // Branches inherit the versioning state.
    assert_eq!(t.ok("POST", "/v1/storage/buckets/ver/branches", Some(json!({ "name": "ver-br" }))).await["versioning"], "Enabled");
}
