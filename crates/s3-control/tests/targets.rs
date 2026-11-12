//! `/v1/storage/replication-targets`, and replication rules that use them through the bucket
//! `PATCH`: a second roll server is the bucket outside roll.

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use reqwest::{Client, StatusCode};
use s3_control::{Control, ControlConfig};
use s3_store::{Store, StoreConfig};
use serde_json::{Value, json};
use std::sync::Arc;

const ACCESS: &str = "rolltargetadmin";
const SECRET: &str = "rolltargetsecret0123456789";

struct T {
    api: String,
    server: Arc<s3_server::Server>,
    http: Client,
    _dirs: Vec<tempfile::TempDir>,
}

async fn s3(name: &str, dirs: &mut Vec<tempfile::TempDir>) -> (Arc<Store>, Arc<s3_server::Server>, String) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(StoreConfig::new(dir.path().join("meta")).manual(), s3_store::test_backend(name).await).await.unwrap();
    store.ensure_key(ACCESS, SECRET, "admin").await.unwrap();
    let mut cfg = s3_server::Config { spool_dir: dir.path().to_path_buf(), ..Default::default() };
    cfg.notify.dispatch_interval = None;
    let server = s3_server::Server::new(store.clone(), cfg);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let serving = server.clone();
    tokio::spawn(async move { s3_server::serve(l, serving).await.unwrap() });
    dirs.push(dir);
    (store, server, endpoint)
}

fn client(endpoint: &str) -> aws_sdk_s3::Client {
    aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .endpoint_url(endpoint)
            .credentials_provider(Credentials::new(ACCESS, SECRET, None, None, "t"))
            .force_path_style(true)
            .build(),
    )
}

impl T {
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = self.http.request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), format!("{}{path}", self.api));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or_else(|_| panic!("{method} {path}: not JSON: {text}")))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn targets_and_replication() {
    let mut dirs = Vec::new();
    let (store, server, a_endpoint) = s3("s3-control-targets-a", &mut dirs).await;
    let (_bstore, _b, b_endpoint) = s3("s3-control-targets-b", &mut dirs).await;
    let control = Control::new(store.clone(), ControlConfig::local("http://127.0.0.1:1")).await.unwrap();
    control.set_notifier(server.notifier.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(l, s3_control::api::router(control)).await.unwrap() });
    let t = T { api, server, http: Client::new(), _dirs: dirs };

    let b = client(&b_endpoint);
    b.create_bucket().bucket("theirs").send().await.unwrap();
    b.put_bucket_versioning()
        .bucket("theirs")
        .versioning_configuration(aws_sdk_s3::types::VersioningConfiguration::builder().status(aws_sdk_s3::types::BucketVersioningStatus::Enabled).build())
        .send()
        .await
        .unwrap();

    // Targets: the secret is write-only.
    let body = json!({ "name": "elsewhere", "endpoint": b_endpoint, "access_key_id": ACCESS, "secret_access_key": SECRET, "buckets": ["theirs"] });
    let (s, v) = t.call("POST", "/v1/storage/replication-targets", Some(body.clone())).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["credentials"], "access_key");
    assert!(!v.to_string().contains(SECRET), "never shown: {v}");
    assert_eq!(t.call("POST", "/v1/storage/replication-targets", Some(body)).await.0, StatusCode::CONFLICT);
    assert_eq!(t.call("POST", "/v1/storage/replication-targets", Some(json!({ "name": "Bad Name" }))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(t.call("POST", "/v1/storage/replication-targets", Some(json!({ "name": "nosecret", "access_key_id": "AK" }))).await.0, StatusCode::BAD_REQUEST);
    let (_, list) = t.call("GET", "/v1/storage/replication-targets", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    let (s, v) = t.call("PUT", "/v1/storage/replication-targets/elsewhere", Some(json!({ "endpoint": b_endpoint, "access_key_id": ACCESS, "buckets": ["theirs"] }))).await;
    assert_eq!(s, StatusCode::OK, "the secret is kept: {v}");
    assert_eq!(t.call("GET", "/v1/storage/replication-targets/nothing", None).await.0, StatusCode::NOT_FOUND);

    // A replication rule to the target's bucket, through the bucket PATCH.
    let a = client(&a_endpoint);
    a.create_bucket().bucket("mine").send().await.unwrap();
    let (s, v) = t.call("PATCH", "/v1/storage/buckets/mine", Some(json!({ "versioning": "Enabled", "replication": { "rules": [{ "destination": "theirs" }] } }))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let rule = &v["replication"]["rules"][0];
    assert_eq!(rule["target"], "elsewhere");
    assert_eq!(rule["metrics"]["operations_pending"], 0);
    // A bucket of the target without versioning is refused, like S3 does.
    b.create_bucket().bucket("unversioned").send().await.unwrap();
    let (s, v) = t.call("PATCH", "/v1/storage/buckets/mine", Some(json!({ "replication": { "rules": [{ "destination": "unversioned", "target": "elsewhere" }] } }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"].as_str().unwrap().contains("versioning"), "{v}");

    // Pending, then replicated, with metrics.
    a.put_object().bucket("mine").key("k").body(aws_sdk_s3::primitives::ByteStream::from_static(b"over there")).send().await.unwrap();
    let (_, v) = t.call("GET", "/v1/storage/buckets/mine", None).await;
    assert_eq!(v["replication"]["rules"][0]["metrics"]["operations_pending"], 1, "{v}");
    assert_eq!(v["replication"]["rules"][0]["metrics"]["bytes_pending"], 10);
    t.server.dispatch_events().await.unwrap();
    let (_, v) = t.call("GET", "/v1/storage/buckets/mine", None).await;
    let m = &v["replication"]["rules"][0]["metrics"];
    assert_eq!(m["operations_pending"], 0, "{v}");
    assert_eq!(m["replicated"], 1);
    assert!(m["latency_ms"].is_number());
    let got = b.get_object().bucket("theirs").key("k").send().await.unwrap();
    assert_eq!(got.body.collect().await.unwrap().into_bytes().as_ref(), b"over there");

    // In use: can't be deleted.
    assert_eq!(t.call("DELETE", "/v1/storage/replication-targets/elsewhere", None).await.0, StatusCode::CONFLICT);
    t.call("PATCH", "/v1/storage/buckets/mine", Some(json!({ "replication": null }))).await;
    assert_eq!(t.call("DELETE", "/v1/storage/replication-targets/elsewhere", None).await.0, StatusCode::OK);
}
