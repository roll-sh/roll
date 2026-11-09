//! The dashboard's sign-in and IAM through the control plane: `POST /v1/storage/console/signin`,
//! `POST /v1/storage/iam` (the IAM query API as JSON), and the guard that makes a request with
//! a bearer token act as its IAM user: each operation checked as the S3 request it stands for,
//! the operator's operations refused.

use reqwest::{Client, StatusCode};
use s3_control::{Control, ControlConfig};
use s3_store::{Store, StoreConfig};
use serde_json::{Value, json};
use std::sync::Arc;

struct T {
    api: String,
    store: Arc<Store>,
    http: Client,
    _dir: tempfile::TempDir,
}

async fn start() -> T {
    let dir = tempfile::tempdir().unwrap();
    let backend = s3_store::test_backend("s3-control-console").await;
    let store = Store::open(StoreConfig::new(dir.path().join("meta")).manual(), backend).await.unwrap();
    let s3_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let s3 = format!("http://127.0.0.1:{}", s3_listener.local_addr().unwrap().port());
    let server = s3_server::Server::new(store.clone(), s3_server::Config { spool_dir: dir.path().to_path_buf(), ..Default::default() });
    let control = Control::new(store.clone(), ControlConfig::local(s3.clone())).await.unwrap();
    control.set_server(server.clone());
    tokio::spawn(async move { s3_server::serve(s3_listener, server).await.unwrap() });
    let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://127.0.0.1:{}", api_listener.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(api_listener, s3_control::api::router(control)).await.unwrap() });
    T { api, store, http: Client::new(), _dir: dir }
}

impl T {
    async fn call(&self, method: &str, path: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = self.http.request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), format!("{}{path}", self.api));
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or_else(|_| panic!("{method} {path}: not JSON: {text}")))
    }

    /// An IAM call as the operator (`token: None`) or a signed-in user.
    async fn iam(&self, token: Option<&str>, body: Value) -> (StatusCode, Value) {
        self.call("POST", "/v1/storage/iam", token, Some(body)).await
    }

    async fn iam_ok(&self, token: Option<&str>, body: Value) -> Value {
        let (s, v) = self.iam(token, body.clone()).await;
        assert_eq!(s, StatusCode::OK, "{body}: {v}");
        v
    }
}

#[tokio::test]
async fn iam_as_json() {
    let t = start().await;
    let acct = t.store.default_account_id();
    // Lists are arrays, leaves strings, empty lists null; parameters nest.
    let v = t.iam_ok(None, json!({"Action": "CreateUser", "UserName": "alice", "Tags": [{"Key": "team", "Value": "web"}]})).await;
    assert_eq!(v["User"]["Arn"], format!("arn:aws:iam::{acct}:user/alice"));
    assert_eq!(v["User"]["Tags"], json!([{"Key": "team", "Value": "web"}]));
    let v = t.iam_ok(None, json!({"Action": "ListUsers"})).await;
    assert_eq!((v["Users"][0]["UserName"].as_str(), v["IsTruncated"].as_str()), (Some("alice"), Some("false")));
    assert_eq!(t.iam_ok(None, json!({"Action": "ListGroups"})).await["Groups"], Value::Null);
    // Actions without a result answer `{}`.
    assert_eq!(t.iam_ok(None, json!({"Action": "TagUser", "UserName": "alice", "Tags": [{"Key": "env", "Value": "prod"}]})).await, json!({}));
    let (s, v) = t.iam(None, json!({"Action": "DeleteUser", "UserName": "nobody"})).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::NOT_FOUND, Some("NoSuchEntity")));
}

#[tokio::test]
async fn iam_errors_and_documents() {
    let t = start().await;
    // IAM's status and code, with the parser's message for a bad policy.
    let (s, v) = t.iam(None, json!({"Action": "PutUserPolicy", "UserName": "nobody", "PolicyName": "p", "PolicyDocument": "{}"})).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::NOT_FOUND, Some("NoSuchEntity")));
    t.iam_ok(None, json!({"Action": "CreateUser", "UserName": "alice"})).await;
    let (s, v) = t.iam(None, json!({"Action": "PutUserPolicy", "UserName": "alice", "PolicyName": "p", "PolicyDocument": "{\"Statement\": [{\"Effect\": \"Allow\"}]}"})).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::BAD_REQUEST, Some("MalformedPolicyDocument")));
    assert!(v["error"].as_str().unwrap().contains("Action"), "{v}");
    let doc = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Action": "s3:GetObject", "Resource": "*"}]}).to_string();
    t.iam_ok(None, json!({"Action": "PutUserPolicy", "UserName": "alice", "PolicyName": "p", "PolicyDocument": doc})).await;
    let got = t.iam_ok(None, json!({"Action": "GetUserPolicy", "UserName": "alice", "PolicyName": "p"})).await;
    assert_eq!(serde_json::from_str::<Value>(got["PolicyDocument"].as_str().unwrap()).unwrap()["Statement"][0]["Action"], "s3:GetObject", "decoded");
    let summary = t.iam_ok(None, json!({"Action": "GetAccountSummary"})).await;
    assert_eq!(summary["SummaryMap"]["Users"], "1");
    // `?missing=null`: a lookup of something absent is `null` rather than a 404.
    let (s, v) = t.call("POST", "/v1/storage/iam?missing=null", None, Some(json!({"Action": "GetLoginProfile", "UserName": "alice"}))).await;
    assert_eq!((s, v), (StatusCode::OK, Value::Null));
    let (s, v) = t.call("POST", "/v1/storage/iam?missing=null", None, Some(json!({"Action": "PutUserPolicy", "UserName": "alice", "PolicyName": "p", "PolicyDocument": "{}"}))).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::BAD_REQUEST, Some("MalformedPolicyDocument")), "other errors stay errors");
    let (s, v) = t.iam(None, json!({"Action": "Nope"})).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::BAD_REQUEST, Some("InvalidAction")));
    let (s, _) = t.call("POST", "/v1/storage/iam", None, Some(json!([1]))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn signed_in_users_act_as_themselves() {
    let t = start().await;
    let acct = t.store.default_account_id();
    t.call("POST", "/v1/storage/buckets", None, Some(json!({"name": "dash"}))).await;
    t.call("POST", "/v1/storage/buckets", None, Some(json!({"name": "other"}))).await;
    t.iam_ok(None, json!({"Action": "CreateUser", "UserName": "alice"})).await;
    t.iam_ok(None, json!({"Action": "CreateLoginProfile", "UserName": "alice", "Password": "Correct-Horse-9"})).await;
    t.iam_ok(None, json!({"Action": "CreateAccountAlias", "AccountAlias": "dash-co"})).await;
    let accounts = t.call("GET", "/v1/storage/accounts", None, None).await.1;
    assert_eq!(accounts[0]["alias"], "dash-co", "the account view shows its alias");

    // Signing in.
    let (s, v) = t.call("POST", "/v1/storage/console/signin", None, Some(json!({"account": "dash-co", "user": "alice", "password": "wrong-Horse-9"}))).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("InvalidCredentials")));
    let (s, v) = t.call("POST", "/v1/storage/console/signin", None, Some(json!({"account": "dash-co", "user": "alice", "password": "Correct-Horse-9"}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!((v["account_id"].as_str(), v["user"].as_str(), v["account_alias"].as_str()), (Some(acct.as_str()), Some("alice"), Some("dash-co")));
    let token = v["token"].as_str().unwrap().to_string();
    let tok = Some(token.as_str());
    let who = t.call("GET", "/v1/storage/console/session", tok, None).await;
    assert_eq!((who.0, who.1["arn"].as_str()), (StatusCode::OK, Some(format!("arn:aws:iam::{acct}:user/alice").as_str())));
    assert!(who.1.get("token").is_none(), "the token isn't echoed");
    let (s, v) = t.call("GET", "/v1/storage/console/session", Some("nonsense"), None).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("InvalidToken")));

    // Without permissions: nothing, through any route.
    let (s, v) = t.call("GET", "/v1/storage/buckets", tok, None).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::FORBIDDEN, Some("AccessDenied")));
    let (s, _) = t.call("GET", "/v1/storage/buckets/dash/objects", tok, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // The operator's routes are refused whatever the user's policies.
    for path in ["/v1/storage/accounts", "/v1/storage/keys", "/v1/storage/roles", "/v1/storage/replication-targets", "/v1/storage/usage?accounts=true"] {
        let (s, v) = t.call("GET", path, tok, None).await;
        assert_eq!((s, v["code"].as_str()), (StatusCode::FORBIDDEN, Some("OperatorOnly")), "{path}");
    }
    // Policies open things up, exactly as for S3 requests.
    let policy = json!({"Version": "2012-10-17", "Statement": [
        {"Effect": "Allow", "Action": ["s3:ListAllMyBuckets"], "Resource": "*"},
        {"Effect": "Allow", "Action": ["s3:ListBucket", "s3:GetObject", "s3:PutObject"], "Resource": ["arn:aws:s3:::dash", "arn:aws:s3:::dash/*"]},
        {"Effect": "Allow", "Action": "iam:GetUser", "Resource": "arn:aws:iam::*:user/${aws:username}"}
    ]});
    t.iam_ok(None, json!({"Action": "PutUserPolicy", "UserName": "alice", "PolicyName": "dash", "PolicyDocument": policy.to_string()})).await;
    let (s, v) = t.call("GET", "/v1/storage/buckets", tok, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v.as_array().unwrap().len(), 2, "ListBuckets lists the account's buckets");
    assert_eq!(t.call("GET", "/v1/storage/buckets/dash/objects", tok, None).await.0, StatusCode::OK);
    assert_eq!(t.call("GET", "/v1/storage/buckets/other/objects", tok, None).await.0, StatusCode::FORBIDDEN);
    let up = t.http.put(format!("{}/v1/storage/buckets/dash/upload?key=a.txt", t.api)).bearer_auth(&token).body("hello").send().await.unwrap();
    assert_eq!(up.status(), StatusCode::OK);
    let up = t.http.put(format!("{}/v1/storage/buckets/other/upload?key=a.txt", t.api)).bearer_auth(&token).body("hello").send().await.unwrap();
    assert_eq!(up.status(), StatusCode::FORBIDDEN);
    let down = t.http.get(format!("{}/v1/storage/buckets/dash/download?key=a.txt", t.api)).bearer_auth(&token).send().await.unwrap();
    assert_eq!(down.text().await.unwrap(), "hello");
    // Deleting needs s3:DeleteObject; changing the bucket's policy, s3:PutBucketPolicy.
    let (s, _) = t.call("DELETE", "/v1/storage/buckets/dash/object?key=a.txt", tok, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = t.call("POST", "/v1/storage/buckets/dash/delete", tok, Some(json!({"keys": ["a.txt"]}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = t.call("PATCH", "/v1/storage/buckets/dash", tok, Some(json!({"policy": null}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = t.call("POST", "/v1/storage/buckets", tok, Some(json!({"name": "mine"}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "s3:CreateBucket");
    // The account comes from the session, not x-roll-account.
    let alt = t.store.create_account("alt", None).await.unwrap();
    let r = t.http.get(format!("{}/v1/storage/buckets", t.api)).bearer_auth(&token).header("x-roll-account", &alt.id).send().await.unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v.as_array().unwrap().len(), 2, "still its own account's");

    // IAM as the user: what its policies allow.
    let me = t.iam_ok(tok, json!({"Action": "GetUser"})).await;
    assert_eq!(me["User"]["UserName"], "alice");
    let (s, v) = t.iam(tok, json!({"Action": "ListUsers"})).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::FORBIDDEN, Some("AccessDenied")));
    // A bucket policy naming a user that doesn't exist is refused here too.
    let bad = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"AWS": format!("arn:aws:iam::{acct}:user/nobody")}, "Action": "s3:GetObject", "Resource": "arn:aws:s3:::dash/*"}]});
    let (s, v) = t.call("PATCH", "/v1/storage/buckets/dash", None, Some(json!({"policy": bad}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("Invalid principal in policy"), "{v}");

    // The session ends with the user's password.
    t.iam_ok(None, json!({"Action": "DeleteLoginProfile", "UserName": "alice"})).await;
    let (s, v) = t.call("GET", "/v1/storage/buckets", tok, None).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("InvalidToken")));
}
