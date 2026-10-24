//! `/v1/storage/accounts` and account scoping: every endpoint acts in the account named by
//! `x-roll-account` (or `?account=`), the default account otherwise; usage per account with
//! Requester Pays.

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use reqwest::{Client, StatusCode};
use s3_control::{Control, ControlConfig};
use s3_store::{Store, StoreConfig};
use serde_json::{Value, json};
use std::sync::Arc;

struct T {
    api: String,
    s3: String,
    store: Arc<Store>,
    http: Client,
    _dir: tempfile::TempDir,
}

async fn start() -> T {
    let dir = tempfile::tempdir().unwrap();
    let backend = s3_store::test_backend("s3-control-accounts").await;
    let store = Store::open(StoreConfig::new(dir.path().join("meta")).manual(), backend).await.unwrap();
    let s3_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let s3 = format!("http://127.0.0.1:{}", s3_listener.local_addr().unwrap().port());
    let server = s3_server::Server::new(store.clone(), s3_server::Config { spool_dir: dir.path().to_path_buf(), ..Default::default() });
    tokio::spawn(async move { s3_server::serve(s3_listener, server).await.unwrap() });
    let control = Control::new(store.clone(), ControlConfig::local(s3.clone())).await.unwrap();
    let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://127.0.0.1:{}", api_listener.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(api_listener, s3_control::api::router(control)).await.unwrap() });
    T { api, s3, store, http: Client::new(), _dir: dir }
}

impl T {
    async fn call(&self, method: &str, path: &str, account: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = self.http.request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), format!("{}{path}", self.api));
        if let Some(a) = account {
            req = req.header("x-roll-account", a);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or_else(|_| panic!("{method} {path}: not JSON: {text}")))
    }

    async fn ok(&self, method: &str, path: &str, account: Option<&str>, body: Option<Value>) -> Value {
        let (s, v) = self.call(method, path, account, body).await;
        assert_eq!(s, StatusCode::OK, "{method} {path}: {v}");
        v
    }

    fn s3_client(&self, key: &Value) -> aws_sdk_s3::Client {
        let creds = Credentials::new(key["id"].as_str().unwrap(), key["secret_access_key"].as_str().unwrap(), None, None, "control");
        let conf = aws_sdk_s3::Config::builder().behavior_version(BehaviorVersion::latest()).region(Region::new("us-east-1")).endpoint_url(&self.s3).credentials_provider(creds).force_path_style(true).build();
        aws_sdk_s3::Client::from_conf(conf)
    }
}

#[tokio::test]
async fn accounts_and_scoping() {
    let t = start().await;
    let default = t.store.default_account_id();
    let list = t.ok("GET", "/v1/storage/accounts", None, None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!((list[0]["id"].as_str(), list[0]["default"].as_bool()), (Some(default.as_str()), Some(true)));

    // A new account, with its root user's key.
    let a = t.ok("POST", "/v1/storage/accounts", None, Some(json!({"name": "acme", "email": "Ops@Acme.example"}))).await;
    let acme = a["id"].as_str().unwrap().to_string();
    assert_eq!((a["email"].as_str(), a["default"].as_bool(), a["canonical"].as_str().map(str::len)), (Some("ops@acme.example"), Some(false), Some(64)));
    let root = a["root_key"].clone();
    assert_eq!((root["account"].as_str(), root["permission"].as_str(), root["buckets"].is_null()), (Some(acme.as_str()), Some("admin"), true));
    assert_eq!(t.call("POST", "/v1/storage/accounts", None, Some(json!({"name": "x", "email": "ops@acme.example"}))).await.0, StatusCode::CONFLICT);
    assert_eq!(t.call("POST", "/v1/storage/accounts", None, Some(json!({"name": "x", "nope": 1}))).await.0, StatusCode::BAD_REQUEST);
    let renamed = t.ok("PATCH", &format!("/v1/storage/accounts/{acme}"), None, Some(json!({"name": "Acme Inc"}))).await;
    assert_eq!((renamed["name"].as_str(), renamed["email"].as_str()), (Some("Acme Inc"), Some("ops@acme.example")));
    assert_eq!(t.call("GET", "/v1/storage/accounts/123", None, None).await.0, StatusCode::NOT_FOUND);

    // Each account sees its own buckets, keys and roles; the default one without a header.
    t.ok("POST", "/v1/storage/buckets", None, Some(json!({"name": "home-bucket"}))).await;
    t.ok("POST", "/v1/storage/buckets", Some(&acme), Some(json!({"name": "acme-bucket"}))).await;
    let names = |v: &Value| v.as_array().unwrap().iter().map(|b| b["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(names(&t.ok("GET", "/v1/storage/buckets", None, None).await), vec!["home-bucket"]);
    assert_eq!(names(&t.ok("GET", "/v1/storage/buckets", Some(&acme), None).await), vec!["acme-bucket"]);
    assert_eq!(names(&t.ok("GET", &format!("/v1/storage/buckets?account={acme}"), None, None).await), vec!["acme-bucket"], "or ?account=");
    assert_eq!(t.call("GET", "/v1/storage/buckets/acme-bucket", None, None).await.0, StatusCode::NOT_FOUND, "another account's bucket");
    assert_eq!(t.call("GET", "/v1/storage/buckets/acme-bucket/objects", None, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(t.call("DELETE", "/v1/storage/buckets/acme-bucket", None, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(t.call("POST", "/v1/storage/buckets", Some(&acme), Some(json!({"name": "home-bucket"}))).await.0, StatusCode::CONFLICT, "names are global");
    assert_eq!(t.call("GET", "/v1/storage/buckets", Some("999999999999"), None).await.0, StatusCode::NOT_FOUND, "an unknown account");
    let info = t.ok("GET", "/v1/storage", Some(&acme), None).await;
    assert_eq!((info["account_id"].as_str(), info["owner_id"].as_str(), info["buckets"].as_u64()), (Some(acme.as_str()), a["canonical"].as_str(), Some(1)));
    assert_eq!(info["deployment"]["accounts"], 2);
    let k = t.ok("POST", "/v1/storage/keys", Some(&acme), Some(json!({"name": "ci", "permission": "read"}))).await;
    assert_eq!(k["account"].as_str(), Some(acme.as_str()));
    assert_eq!(t.ok("GET", "/v1/storage/keys", Some(&acme), None).await.as_array().unwrap().len(), 2, "its root key and ci");
    assert_eq!(t.call("DELETE", &format!("/v1/storage/keys/{}", k["id"].as_str().unwrap()), None, None).await.0, StatusCode::NOT_FOUND, "not the default account's");
    t.ok("POST", "/v1/storage/roles", Some(&acme), Some(json!({"name": "uploader", "permission": "write"}))).await;
    assert_eq!(t.ok("GET", "/v1/storage/roles", None, None).await, json!([]));
    let r = t.ok("GET", "/v1/storage/roles/uploader", Some(&acme), None).await;
    assert_eq!(r["arn"].as_str(), Some(format!("arn:aws:iam::{acme}:role/uploader").as_str()));
    let s = t.ok("POST", "/v1/storage/roles/uploader/sessions", Some(&acme), Some(json!({}))).await;
    assert_eq!(s["assumed_role_arn"].as_str(), Some(format!("arn:aws:sts::{acme}:assumed-role/uploader/roll-control-plane").as_str()));

    // The account's root key works on its buckets through the S3 API, not on others'.
    let c = t.s3_client(&root);
    c.put_object().bucket("acme-bucket").key("k").body(aws_sdk_s3::primitives::ByteStream::from_static(b"x")).send().await.unwrap();
    assert!(c.get_object().bucket("home-bucket").key("k").send().await.is_err());
    let listed = c.list_buckets().send().await.unwrap();
    assert_eq!(listed.buckets().iter().filter_map(|b| b.name()).collect::<Vec<_>>(), vec!["acme-bucket"]);

    // Requester Pays: the requester's account pays; usage shows both sides.
    let home = t.store.bucket("home-bucket").unwrap();
    t.store.set_bucket_config("home-bucket", s3_store::config::kind::REQUEST_PAYMENT, "", Some("Requester"), None).await.unwrap();
    let policy = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"AWS": acme}, "Action": "s3:GetObject", "Resource": "arn:aws:s3:::home-bucket/*"}]});
    t.ok("PATCH", "/v1/storage/buckets/home-bucket", None, Some(json!({"policy": policy}))).await;
    t.store.meter.request(&home.id, s3_store::Class::A);
    t.store.meter.request_by(&home.id, Some(&acme), s3_store::Class::B);
    t.store.meter.egress_by(&home.id, Some(&acme), 1_000_000_000, false);
    t.store.flush_meter().unwrap();
    let mine = t.ok("GET", "/v1/storage/usage", None, None).await;
    assert_eq!(mine["account"].as_str(), Some(default.as_str()));
    assert_eq!(mine["billed_to_requesters"]["class_b"], 1);
    assert_eq!(mine["billed_to_requesters"]["accounts"], json!([acme]));
    assert!(mine["as_requester"].is_null());
    let theirs = t.ok("GET", "/v1/storage/usage", Some(&acme), None).await;
    assert_eq!(theirs["as_requester"]["egress_internet_bytes"], 1_000_000_000_u64);
    assert!(theirs["cost"]["egress_usd"].as_f64().unwrap() > 0.08, "a GB of egress is on the requester's bill: {theirs}");
    assert_eq!(mine["cost"]["egress_usd"].as_f64(), Some(0.0), "not on the owner's");
    let bucket = t.ok("GET", "/v1/storage/buckets/home-bucket/usage", None, None).await;
    assert_eq!((bucket["class_a"].as_u64(), bucket["class_b"].as_u64()), (Some(1), Some(0)), "the bucket's own requests only");
    assert_eq!(bucket["billed_to_requesters"]["class_b"], 1);
    let all = t.ok("GET", "/v1/storage/usage?accounts=true", None, None).await;
    assert_eq!(all["accounts"].as_array().unwrap().len(), 2);

    // MFA Delete keeps the control plane from deleting versions.
    t.ok("PATCH", "/v1/storage/buckets/home-bucket", None, Some(json!({"versioning": "Enabled"}))).await;
    t.store.set_bucket_config("home-bucket", s3_store::config::kind::MFA_DELETE, "", Some("Enabled"), None).await.unwrap();
    let up = t.http.put(format!("{}/v1/storage/buckets/home-bucket/upload?key=v", t.api)).body("v1").send().await.unwrap();
    let vid = up.json::<Value>().await.unwrap()["version_id"].as_str().unwrap().to_string();
    assert_eq!(t.call("DELETE", &format!("/v1/storage/buckets/home-bucket/object?key=v&version_id={vid}"), None, None).await.0, StatusCode::CONFLICT);
    assert_eq!(t.call("PATCH", "/v1/storage/buckets/home-bucket", None, Some(json!({"versioning": "Suspended"}))).await.0, StatusCode::CONFLICT);

    // Deleting an account: not while it owns buckets; never the default one.
    assert_eq!(t.call("DELETE", &format!("/v1/storage/accounts/{acme}"), None, None).await.0, StatusCode::CONFLICT);
    t.ok("DELETE", "/v1/storage/buckets/acme-bucket?force=true", Some(&acme), None).await;
    t.ok("DELETE", &format!("/v1/storage/accounts/{acme}"), None, None).await;
    assert!(c.list_buckets().send().await.is_err(), "its keys went with it");
    assert_eq!(t.call("DELETE", &format!("/v1/storage/accounts/{default}"), None, None).await.0, StatusCode::BAD_REQUEST);
}
