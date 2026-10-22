//! Temporary credentials: the STS query API through `aws-sdk-sts`, and S3 through
//! `aws-sdk-s3` with the credentials it returns (session policies, principal tags, expiry,
//! revocation, web identity, presigned URLs, tampered tokens).

mod common;

use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_sts::config::{BehaviorVersion, Credentials, Region};
use common::*;
use s3_store::Permission;
use s3_store::sts::{OidcProvider, RoleSpec, Trust};
use serde_json::json;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};

struct Env {
    t: TestServer,
    server: Arc<s3_server::Server>,
    clock: Arc<AtomicI64>,
}

impl Env {
    fn advance_s(&self, s: i64) {
        self.clock.fetch_add(s * 1000, Ordering::SeqCst);
    }
}

async fn env() -> Env {
    env_with(|_| {}).await
}

async fn env_with(tweak: impl FnOnce(&mut s3_server::Config)) -> Env {
    let clock = Arc::new(AtomicI64::new(now_s() * 1000));
    let c = clock.clone();
    let (t, server) = start_full(move |s| s.clock = Arc::new(move || c.load(Ordering::SeqCst)), tweak).await;
    Env { t, server, clock }
}

fn sts(endpoint: &str, access: &str, secret: &str, token: Option<&str>) -> aws_sdk_sts::Client {
    let conf = aws_sdk_sts::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(REGION))
        .endpoint_url(endpoint)
        .credentials_provider(Credentials::new(access, secret, token.map(str::to_string), None, "test"))
        .build();
    aws_sdk_sts::Client::from_conf(conf)
}

/// An STS client without credentials (web identity).
fn sts_anonymous(endpoint: &str) -> aws_sdk_sts::Client {
    let conf = aws_sdk_sts::Config::builder().behavior_version(BehaviorVersion::latest()).region(Region::new(REGION)).endpoint_url(endpoint).build();
    aws_sdk_sts::Client::from_conf(conf)
}

fn s3_with(endpoint: &str, c: &aws_sdk_sts::types::Credentials) -> aws_sdk_s3::Client {
    let conf = aws_sdk_s3::Config::builder()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .region(aws_sdk_s3::config::Region::new(REGION))
        .endpoint_url(endpoint)
        .credentials_provider(aws_sdk_s3::config::Credentials::new(c.access_key_id(), c.secret_access_key(), Some(c.session_token().to_string()), None, "sts"))
        .force_path_style(true)
        .build();
    aws_sdk_s3::Client::from_conf(conf)
}

fn s3_raw(endpoint: &str, access: &str, secret: &str, token: Option<&str>) -> aws_sdk_s3::Client {
    let conf = aws_sdk_s3::Config::builder()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .region(aws_sdk_s3::config::Region::new(REGION))
        .endpoint_url(endpoint)
        .credentials_provider(aws_sdk_s3::config::Credentials::new(access, secret, token.map(str::to_string), None, "test"))
        .force_path_style(true)
        .build();
    aws_sdk_s3::Client::from_conf(conf)
}

/// The error code of a failed SDK call (S3 or STS).
fn code<T: std::fmt::Debug, E: ProvideErrorMetadata + std::fmt::Debug>(r: Result<T, SdkError<E>>) -> String {
    match r {
        Ok(v) => panic!("expected an error, got {v:?}"),
        Err(e) => e.as_service_error().and_then(|s| s.code()).map(str::to_string).unwrap_or_else(|| format!("{e:?}")),
    }
}

fn sts_code<T: std::fmt::Debug, E: ProvideErrorMetadata + std::fmt::Debug>(r: Result<T, SdkError<E>>) -> String {
    code(r)
}

async fn bucket(t: &TestServer, name: &str) {
    t.client.create_bucket().bucket(name).send().await.unwrap();
}

/// The error code of a failed [`put`].
fn failed(r: Result<(), String>) -> String {
    r.expect_err("expected an error")
}

/// PutObject; `Err` is the error code.
async fn put(c: &aws_sdk_s3::Client, b: &str, k: &str) -> Result<(), String> {
    match c.put_object().bucket(b).key(k).body(ByteStream::from_static(b"hi")).send().await {
        Ok(_) => Ok(()),
        Err(e) => Err(code::<(), _>(Err(e))),
    }
}

fn spec(permission: Permission, keys: &[&str]) -> RoleSpec {
    RoleSpec { description: String::new(), permission, buckets: None, policy: None, trust: Trust { keys: keys.iter().map(|k| k.to_string()).collect(), ..Default::default() }, max_session_s: Some(7200) }
}

#[tokio::test]
async fn get_session_token_and_caller_identity() {
    let e = env().await;
    let t = &e.t;
    bucket(t, "gst").await;
    let admin = sts(&t.endpoint, ACCESS, SECRET, None);
    let id = admin.get_caller_identity().send().await.unwrap();
    let account = t.store.account_id();
    assert_eq!(id.account(), Some(account.as_str()));
    // An admin key over every bucket is the account's root user, as AWS shows it.
    assert_eq!(id.arn(), Some(format!("arn:aws:iam::{account}:root").as_str()));
    assert_eq!(id.user_id(), Some(account.as_str()));

    let out = admin.get_session_token().duration_seconds(900).send().await.unwrap();
    let c = out.credentials().unwrap();
    assert!(c.access_key_id().starts_with("RT") && c.access_key_id().len() == 20);
    let exp = c.expiration().secs();
    assert!((exp - (now_s() + 900)).abs() <= 2, "{exp}");
    let s3 = s3_with(&t.endpoint, c);
    put(&s3, "gst", "a.txt").await.unwrap();
    let got = s3.get_object().bucket("gst").key("a.txt").send().await.unwrap();
    assert_eq!(got.body.collect().await.unwrap().into_bytes().as_ref(), b"hi");
    assert!(s3.list_buckets().send().await.unwrap().buckets().iter().any(|b| b.name() == Some("gst")));

    // The session is its key's (here the root user's).
    let temp = sts(&t.endpoint, c.access_key_id(), c.secret_access_key(), Some(c.session_token()));
    let id = temp.get_caller_identity().send().await.unwrap();
    assert_eq!(id.user_id(), Some(account.as_str()));
    assert_eq!(sts_code(temp.get_session_token().send().await), "AccessDenied", "no sessions of sessions");

    // A temporary key without its token is unknown; a permanent key with a token is refused.
    assert_eq!(failed(put(&s3_raw(&t.endpoint, c.access_key_id(), c.secret_access_key(), None), "gst", "b").await), "InvalidAccessKeyId");
    assert_eq!(failed(put(&s3_raw(&t.endpoint, ACCESS, SECRET, Some(c.session_token())), "gst", "b").await), "InvalidToken");
    // Durations are bounded: the root user's sessions to an hour, an account key's to 36 h.
    assert_eq!(sts_code(admin.get_session_token().duration_seconds(899).send().await), "ValidationError");
    assert_eq!(sts_code(admin.get_session_token().duration_seconds(3601).send().await), "ValidationError");
    let key = t.store.create_key("app", Permission::Write, None).await.unwrap();
    let app = sts(&t.endpoint, &key.id, &key.secret, None);
    let long = app.get_session_token().duration_seconds(129_600).send().await.unwrap();
    assert!((long.credentials().unwrap().expiration().secs() - (now_s() + 129_600)).abs() <= 2);
    assert_eq!(sts_code(app.get_session_token().duration_seconds(129_601).send().await), "ValidationError");
    let id = app.get_caller_identity().send().await.unwrap();
    assert_eq!(id.arn(), Some(format!("arn:aws:iam::{account}:user/{}", key.id).as_str()), "an account key is a user named by its id");
}

#[tokio::test]
async fn assume_role_with_a_session_policy() {
    let e = env().await;
    let t = &e.t;
    for b in ["photos", "other"] {
        bucket(t, b).await;
    }
    put(&t.client, "photos", "public/cat.jpg").await.unwrap();
    put(&t.client, "other", "x").await.unwrap();
    let key = t.store.create_key("backend", Permission::Write, None).await.unwrap();
    let outsider = t.store.create_key("outsider", Permission::Admin, None).await.unwrap();
    let mut s = spec(Permission::Write, &[&key.id]);
    s.buckets = Some(vec!["photos".into()]);
    t.store.create_role("uploader", s).await.unwrap();
    let account = t.store.account_id();
    let arn = format!("arn:aws:iam::{account}:role/uploader");

    let client = sts(&t.endpoint, &key.id, &key.secret, None);
    let policy = json!({ "Version": "2012-10-17", "Statement": [
        { "Effect": "Allow", "Action": ["s3:PutObject", "s3:GetObject"], "Resource": "arn:aws:s3:::photos/uploads/*" },
        { "Effect": "Allow", "Action": "s3:ListBucket", "Resource": "arn:aws:s3:::photos" },
    ]});
    let out = client.assume_role().role_arn(&arn).role_session_name("web-42").policy(policy.to_string()).duration_seconds(1800).send().await.unwrap();
    let user = out.assumed_role_user().unwrap();
    assert_eq!(user.arn(), format!("arn:aws:sts::{account}:assumed-role/uploader/web-42"));
    assert!(user.assumed_role_id().starts_with("RR") && user.assumed_role_id().ends_with(":web-42"));
    let c = out.credentials().unwrap();
    let s3 = s3_with(&t.endpoint, c);

    put(&s3, "photos", "uploads/a.jpg").await.unwrap();
    s3.get_object().bucket("photos").key("uploads/a.jpg").send().await.unwrap();
    s3.list_objects_v2().bucket("photos").send().await.unwrap();
    // The session policy narrows the role: other prefixes, deletes and other buckets are out.
    assert_eq!(put(&s3, "photos", "elsewhere.jpg").await.unwrap_err(), "AccessDenied");
    assert_eq!(code(s3.get_object().bucket("photos").key("public/cat.jpg").send().await), "AccessDenied");
    assert_eq!(code(s3.delete_object().bucket("photos").key("uploads/a.jpg").send().await), "AccessDenied");
    assert_eq!(code(s3.get_object().bucket("other").key("x").send().await), "AccessDenied");
    assert_eq!(code(s3.create_bucket().bucket("newone").send().await), "AccessDenied");
    assert_eq!(code(s3.list_buckets().send().await), "AccessDenied");

    // Without a session policy: the role's write permission on its bucket, nothing more.
    let plain = client.assume_role().role_arn(&arn).role_session_name("plain").send().await.unwrap();
    let s3p = s3_with(&t.endpoint, plain.credentials().unwrap());
    put(&s3p, "photos", "anything").await.unwrap();
    s3p.delete_object().bucket("photos").key("anything").send().await.unwrap();
    assert_eq!(put(&s3p, "other", "y").await.unwrap_err(), "AccessDenied");
    assert_eq!(code(s3p.create_bucket().bucket("newone").send().await), "AccessDenied", "write isn't admin");
    let id = sts(&t.endpoint, plain.credentials().unwrap().access_key_id(), plain.credentials().unwrap().secret_access_key(), Some(plain.credentials().unwrap().session_token())).get_caller_identity().send().await.unwrap();
    assert_eq!(id.arn(), Some(format!("arn:aws:sts::{account}:assumed-role/uploader/plain").as_str()));
    // A control-plane trust names permanent keys: a role session isn't one (chaining needs an
    // IAM trust policy naming the role; see tests/iam.rs).
    let chained = sts(&t.endpoint, plain.credentials().unwrap().access_key_id(), plain.credentials().unwrap().secret_access_key(), Some(plain.credentials().unwrap().session_token()));
    assert_eq!(sts_code(chained.assume_role().role_arn(&arn).role_session_name("again").send().await), "AccessDenied");

    // Who may assume it, and how.
    let other = sts(&t.endpoint, &outsider.id, &outsider.secret, None);
    assert_eq!(sts_code(other.assume_role().role_arn(&arn).role_session_name("x1").send().await), "AccessDenied", "not in the trust");
    assert_eq!(sts_code(client.assume_role().role_arn(format!("arn:aws:iam::{account}:role/nope")).role_session_name("x1").send().await), "AccessDenied");
    assert_eq!(sts_code(client.assume_role().role_arn("arn:aws:iam::000000000000:role/uploader").role_session_name("x1").send().await), "AccessDenied", "another account");
    assert_eq!(sts_code(client.assume_role().role_arn(&arn).role_session_name("x1").duration_seconds(7201).send().await), "ValidationError");
    assert_eq!(sts_code(client.assume_role().role_arn(&arn).role_session_name("x").send().await), "ValidationError", "session names are 2+ characters");
    assert_eq!(sts_code(client.assume_role().role_arn(&arn).role_session_name("x1").policy("{\"Statement\": 5}").send().await), "MalformedPolicyDocument");
    let with_principal = json!({ "Statement": [{ "Effect": "Allow", "Principal": "*", "Action": "s3:*", "Resource": "*" }] });
    assert_eq!(sts_code(client.assume_role().role_arn(&arn).role_session_name("x1").policy(with_principal.to_string()).send().await), "MalformedPolicyDocument");
    // Managed session policies (PolicyArns) narrow the session like an inline one.
    let arns = aws_sdk_sts::types::PolicyDescriptorType::builder().arn("arn:aws:iam::aws:policy/AmazonS3ReadOnlyAccess").build();
    let ro = client.assume_role().role_arn(&arn).role_session_name("ro").policy_arns(arns).send().await.unwrap();
    let s3ro = s3_with(&t.endpoint, ro.credentials().unwrap());
    s3ro.get_object().bucket("photos").key("uploads/a.jpg").send().await.unwrap();
    assert_eq!(put(&s3ro, "photos", "uploads/b.jpg").await.unwrap_err(), "AccessDenied", "read-only");
    let missing = aws_sdk_sts::types::PolicyDescriptorType::builder().arn(format!("arn:aws:iam::{account}:policy/nope")).build();
    assert_eq!(sts_code(client.assume_role().role_arn(&arn).role_session_name("x1").policy_arns(missing).send().await), "InvalidParameterValue");
    // MFA: an account key has no MFA device (see tests/iam.rs for MFA).
    assert_eq!(sts_code(client.assume_role().role_arn(&arn).role_session_name("x1").serial_number("arn:aws:iam::1:mfa/x").token_code("123456").send().await), "AccessDenied");
}

#[tokio::test]
async fn external_ids_and_role_policies() {
    let e = env().await;
    let t = &e.t;
    bucket(t, "data").await;
    let key = t.store.create_key("partner", Permission::Read, None).await.unwrap();
    let mut s = spec(Permission::Admin, &[&key.id]);
    s.trust.external_id = Some("Unguessable-123".into());
    // The role's inline policy bounds its sessions even though the role is admin.
    s.policy = Some(json!({ "Statement": [{ "Effect": "Allow", "Action": ["s3:Get*", "s3:List*"], "Resource": "*" }] }).to_string());
    t.store.create_role("partner", s).await.unwrap();
    let arn = format!("arn:aws:iam::{}:role/partner", t.store.account_id());
    let client = sts(&t.endpoint, &key.id, &key.secret, None);
    assert_eq!(sts_code(client.assume_role().role_arn(&arn).role_session_name("p1").send().await), "AccessDenied");
    assert_eq!(sts_code(client.assume_role().role_arn(&arn).role_session_name("p1").external_id("wrong-one").send().await), "AccessDenied");
    let out = client.assume_role().role_arn(&arn).role_session_name("p1").external_id("Unguessable-123").send().await.unwrap();
    let s3 = s3_with(&t.endpoint, out.credentials().unwrap());
    s3.list_objects_v2().bucket("data").send().await.unwrap();
    assert_eq!(put(&s3, "data", "k").await.unwrap_err(), "AccessDenied", "the role policy doesn't allow writes");
    assert_eq!(code(s3.delete_bucket().bucket("data").send().await), "AccessDenied");
}

#[tokio::test]
async fn expiry_revocation_and_key_rotation() {
    let e = env().await;
    let t = &e.t;
    bucket(t, "rev").await;
    let key = t.store.create_key("app", Permission::Write, None).await.unwrap();
    t.store.create_role("app", spec(Permission::Write, &[&key.id])).await.unwrap();
    let arn = format!("arn:aws:iam::{}:role/app", t.store.account_id());
    let client = sts(&t.endpoint, &key.id, &key.secret, None);
    let assume = |name: &'static str| {
        let client = client.clone();
        let arn = arn.clone();
        async move { client.assume_role().role_arn(arn).role_session_name(name).duration_seconds(900).send().await.unwrap().credentials().unwrap().clone() }
    };

    // Expiry (the store's clock decides; the token is checked before the request's time).
    let c = assume("s1").await;
    put(&s3_with(&t.endpoint, &c), "rev", "a").await.unwrap();
    e.advance_s(899);
    put(&s3_with(&t.endpoint, &c), "rev", "a").await.unwrap();
    e.advance_s(1);
    assert_eq!(put(&s3_with(&t.endpoint, &c), "rev", "a").await.unwrap_err(), "ExpiredToken");
    e.clock.store(now_s() * 1000, Ordering::SeqCst);

    // Revoking a role's sessions (AWSRevokeOlderSessions): the old ones are denied, as on AWS;
    // new ones work.
    let old = assume("s2").await;
    put(&s3_with(&t.endpoint, &old), "rev", "b").await.unwrap();
    t.store.revoke_role_sessions("app").await.unwrap();
    assert_eq!(put(&s3_with(&t.endpoint, &old), "rev", "b").await.unwrap_err(), "AccessDenied");
    assert!(t.store.role("app").unwrap().unwrap().iam.policies.contains_key("AWSRevokeOlderSessions"));
    e.advance_s(1);
    let new = assume("s3").await;
    put(&s3_with(&t.endpoint, &new), "rev", "b").await.unwrap();

    // Changing the role changes what its sessions may do (they act with it as it is now).
    let mut read_only = spec(Permission::Read, &[&key.id]);
    read_only.policy = None;
    t.store.update_role("app", read_only).await.unwrap();
    assert_eq!(put(&s3_with(&t.endpoint, &new), "rev", "b").await.unwrap_err(), "AccessDenied", "now read-only");
    s3_with(&t.endpoint, &new).list_objects_v2().bucket("rev").send().await.unwrap();
    t.store.update_role("app", spec(Permission::Write, &[&key.id])).await.unwrap();
    put(&s3_with(&t.endpoint, &new), "rev", "b").await.unwrap();
    // Deleting it leaves its sessions with nothing.
    let again = assume("s4").await;
    put(&s3_with(&t.endpoint, &again), "rev", "b").await.unwrap();
    t.store.delete_role("app").await.unwrap();
    assert_eq!(put(&s3_with(&t.endpoint, &again), "rev", "b").await.unwrap_err(), "AccessDenied");
    // A role of the same name is a different role.
    t.store.create_role("app", spec(Permission::Write, &[&key.id])).await.unwrap();
    assert_eq!(put(&s3_with(&t.endpoint, &again), "rev", "b").await.unwrap_err(), "AccessDenied");

    // GetSessionToken sessions die with their key.
    let gst = client.get_session_token().send().await.unwrap().credentials().unwrap().clone();
    put(&s3_with(&t.endpoint, &gst), "rev", "c").await.unwrap();

    // Rotating the session-token key keeps the previous key's tokens working, once.
    let before = assume("s5").await;
    t.store.rotate_session_key().await.unwrap();
    put(&s3_with(&t.endpoint, &before), "rev", "d").await.unwrap();
    let mid = assume("s6").await;
    t.store.rotate_session_key().await.unwrap();
    assert_eq!(put(&s3_with(&t.endpoint, &before), "rev", "d").await.unwrap_err(), "InvalidToken", "retired after two rotations");
    put(&s3_with(&t.endpoint, &mid), "rev", "d").await.unwrap();

    assert_eq!(put(&s3_with(&t.endpoint, &gst), "rev", "c").await.unwrap_err(), "InvalidToken", "sealed with a retired key");

    // A GetSessionToken session has nothing once its key is gone.
    let gst = client.get_session_token().send().await.unwrap().credentials().unwrap().clone();
    put(&s3_with(&t.endpoint, &gst), "rev", "c").await.unwrap();
    t.store.delete_key(&key.id).await.unwrap();
    assert_eq!(put(&s3_with(&t.endpoint, &gst), "rev", "c").await.unwrap_err(), "AccessDenied");
}

#[tokio::test]
async fn tampered_and_foreign_tokens() {
    let e = env().await;
    let t = &e.t;
    bucket(t, "tamper").await;
    let c = sts(&t.endpoint, ACCESS, SECRET, None).get_session_token().send().await.unwrap().credentials().unwrap().clone();
    let tok = c.session_token().to_string();
    let flip = |i: usize| {
        let mut b = tok.clone().into_bytes();
        b[i] = if b[i] == b'A' { b'B' } else { b'A' };
        String::from_utf8(b).unwrap()
    };
    for i in [0, 3, 10, 30, tok.len() / 2, tok.len() - 1] {
        let bad = flip(i);
        assert_eq!(failed(put(&s3_raw(&t.endpoint, c.access_key_id(), c.secret_access_key(), Some(&bad)), "tamper", "k").await), "InvalidToken", "byte {i}");
    }
    assert_eq!(failed(put(&s3_raw(&t.endpoint, c.access_key_id(), c.secret_access_key(), Some("garbage")), "tamper", "k").await), "InvalidToken");
    assert_eq!(failed(put(&s3_raw(&t.endpoint, c.access_key_id(), c.secret_access_key(), Some(&"A".repeat(13_000))), "tamper", "k").await), "InvalidToken");
    // The token names its key: another temporary key id with it fails.
    let c2 = sts(&t.endpoint, ACCESS, SECRET, None).get_session_token().send().await.unwrap().credentials().unwrap().clone();
    assert_eq!(failed(put(&s3_raw(&t.endpoint, c2.access_key_id(), c.secret_access_key(), Some(&tok)), "tamper", "k").await), "InvalidToken");
    // The right token with the wrong secret is a signature mismatch.
    assert_eq!(failed(put(&s3_raw(&t.endpoint, c.access_key_id(), "wrongsecretwrongsecretwrongsecret0000000", Some(&tok)), "tamper", "k").await), "SignatureDoesNotMatch");
    // Another deployment's token means nothing here.
    let other = env().await;
    let foreign = sts(&other.t.endpoint, ACCESS, SECRET, None).get_session_token().send().await.unwrap().credentials().unwrap().clone();
    assert_eq!(put(&s3_with(&t.endpoint, &foreign), "tamper", "k").await.unwrap_err(), "InvalidToken");
    put(&s3_with(&t.endpoint, &c), "tamper", "k").await.unwrap();
}

#[tokio::test]
async fn presigned_urls_and_post_forms_with_session_tokens() {
    let e = env().await;
    let t = &e.t;
    bucket(t, "pre").await;
    put(&t.client, "pre", "f.txt").await.unwrap();
    let c = sts(&t.endpoint, ACCESS, SECRET, None).get_session_token().send().await.unwrap().credentials().unwrap().clone();
    let s3 = s3_with(&t.endpoint, &c);
    let cfg = aws_sdk_s3::presigning::PresigningConfig::expires_in(std::time::Duration::from_secs(300)).unwrap();
    let url = s3.get_object().bucket("pre").key("f.txt").presigned(cfg.clone()).await.unwrap().uri().to_string();
    assert!(url.contains("X-Amz-Security-Token="), "{url}");
    let r = reqwest::get(&url).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "hi");
    // Without the token (the signature no longer matches either, but the key is unknown first).
    let stripped: String = url.split('&').filter(|p| !p.starts_with("X-Amz-Security-Token=")).collect::<Vec<_>>().join("&");
    assert_eq!(status_code(reqwest::get(&stripped).await.unwrap()).await, (403, "InvalidAccessKeyId".to_string()));
    let tampered = url.replacen("X-Amz-Security-Token=", "X-Amz-Security-Token=A", 1);
    assert_eq!(status_code(reqwest::get(&tampered).await.unwrap()).await.1, "InvalidToken");
    // Presigned PUT.
    let put_url = s3.put_object().bucket("pre").key("up.txt").presigned(cfg).await.unwrap().uri().to_string();
    assert_eq!(reqwest::Client::new().put(&put_url).body("uploaded").send().await.unwrap().status(), 200);
    assert_eq!(t.client.get_object().bucket("pre").key("up.txt").send().await.unwrap().body.collect().await.unwrap().into_bytes().as_ref(), b"uploaded");

    // A browser form upload (POST policy) signed with temporary credentials.
    let date = s3_proto::time::amz_date(now_s());
    let cred = format!("{}/{}/{REGION}/s3/aws4_request", c.access_key_id(), &date[..8]);
    let policy = json!({ "expiration": s3_proto::time::iso8601_ms((now_s() + 600) * 1000), "conditions": [
        { "bucket": "pre" }, ["starts-with", "$key", "form/"], { "x-amz-algorithm": "AWS4-HMAC-SHA256" }, { "x-amz-credential": cred }, { "x-amz-date": date },
        { "x-amz-security-token": c.session_token() },
    ]});
    let b64 = s3_proto::checksum::b64(policy.to_string().as_bytes());
    let key = s3_proto::sigv4::signing_key(c.secret_access_key(), &date[..8], REGION, "s3");
    let sig = s3_proto::sigv4::sign(&key, &b64);
    let form = |token: &str| {
        let fields = [
            ("key", "form/x.txt"),
            ("x-amz-algorithm", "AWS4-HMAC-SHA256"),
            ("x-amz-credential", cred.as_str()),
            ("x-amz-date", date.as_str()),
            ("x-amz-security-token", token),
            ("policy", b64.as_str()),
            ("x-amz-signature", sig.as_str()),
        ];
        let mut b = Vec::new();
        for (k, v) in fields {
            b.extend_from_slice(format!("--xx\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
        }
        b.extend_from_slice(b"--xx\r\nContent-Disposition: form-data; name=\"file\"; filename=\"x.txt\"\r\n\r\nfrom a form\r\n--xx--\r\n");
        reqwest::Client::new().post(format!("{}/pre", t.endpoint)).header("content-type", "multipart/form-data; boundary=xx").body(b)
    };
    let r = form(c.session_token()).send().await.unwrap();
    assert_eq!(r.status(), 204, "{}", r.text().await.unwrap());
    assert_eq!(status_code(form("forged").send().await.unwrap()).await.1, "InvalidToken");
}

#[tokio::test]
async fn bucket_policies_on_roles_and_principal_tags() {
    let e = env().await;
    let t = &e.t;
    bucket(t, "tagged").await;
    let key = t.store.create_key("svc", Permission::Read, None).await.unwrap();
    t.store.create_role("reader", spec(Permission::Read, &[&key.id])).await.unwrap();
    let account = t.store.account_id();
    let role_arn = format!("arn:aws:iam::{account}:role/reader");
    // Writes for the role's sessions tagged team=red; deletes only for one named session.
    let policy = json!({ "Version": "2012-10-17", "Statement": [
        { "Effect": "Allow", "Principal": { "AWS": role_arn }, "Action": "s3:PutObject", "Resource": "arn:aws:s3:::tagged/*",
          "Condition": { "StringEquals": { "aws:PrincipalTag/team": "red", "aws:PrincipalArn": role_arn, "aws:PrincipalType": "AssumedRole" } } },
        { "Effect": "Allow", "Principal": { "AWS": format!("arn:aws:sts::{account}:assumed-role/reader/ops-1") }, "Action": "s3:DeleteObject", "Resource": "arn:aws:s3:::tagged/*" },
        { "Effect": "Deny", "Principal": "*", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::tagged/secret/*",
          "Condition": { "StringNotEquals": { "aws:PrincipalTag/clearance": "high" } } },
    ]});
    t.client.put_bucket_policy().bucket("tagged").policy(policy.to_string()).send().await.unwrap();
    let client = sts(&t.endpoint, &key.id, &key.secret, None);
    let tag = |k: &str, v: &str| aws_sdk_sts::types::Tag::builder().key(k).value(v).build().unwrap();
    let assume = |name: &'static str, tags: Vec<aws_sdk_sts::types::Tag>| {
        let client = client.clone();
        let role_arn = role_arn.clone();
        async move { client.assume_role().role_arn(role_arn).role_session_name(name).set_tags(Some(tags)).send().await.unwrap().credentials().unwrap().clone() }
    };
    let red = s3_with(&t.endpoint, &assume("ops-1", vec![tag("team", "red")]).await);
    let blue = s3_with(&t.endpoint, &assume("ops-2", vec![tag("team", "blue"), tag("clearance", "high")]).await);
    put(&red, "tagged", "a").await.unwrap();
    assert_eq!(put(&blue, "tagged", "a").await.unwrap_err(), "AccessDenied");
    put(&t.client, "tagged", "secret/s").await.unwrap();
    assert_eq!(code(red.get_object().bucket("tagged").key("secret/s").send().await), "AccessDenied");
    blue.get_object().bucket("tagged").key("secret/s").send().await.unwrap();
    red.delete_object().bucket("tagged").key("a").send().await.unwrap();
    assert_eq!(code(blue.delete_object().bucket("tagged").key("secret/s").send().await), "AccessDenied");
    // Bad tags are refused.
    assert_eq!(sts_code(client.assume_role().role_arn(&role_arn).role_session_name("x1").tags(tag("aws:x", "1")).send().await), "ValidationError");
    assert_eq!(sts_code(client.assume_role().role_arn(&role_arn).role_session_name("x1").tags(tag("a", "1")).transitive_tag_keys("b").send().await), "ValidationError");
}

// ---- Web identity ----

struct Rsa {
    key: rsa::RsaPrivateKey,
}

/// One 2048-bit key per test binary (generating one isn't free).
fn rsa_key() -> &'static Rsa {
    static KEY: OnceLock<Rsa> = OnceLock::new();
    KEY.get_or_init(|| Rsa { key: rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap() })
}

fn b64u(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

impl Rsa {
    fn jwks(&self, kid: &str) -> serde_json::Value {
        use rsa::traits::PublicKeyParts;
        let p = self.key.to_public_key();
        json!({ "keys": [{ "kty": "RSA", "kid": kid, "use": "sig", "alg": "RS256", "n": b64u(&p.n().to_bytes_be()), "e": b64u(&p.e().to_bytes_be()) }] })
    }

    fn jwt(&self, kid: &str, claims: serde_json::Value) -> String {
        use rsa::signature::{SignatureEncoding, Signer};
        let msg = format!("{}.{}", b64u(json!({ "alg": "RS256", "typ": "JWT", "kid": kid }).to_string().as_bytes()), b64u(claims.to_string().as_bytes()));
        let sk = rsa::pkcs1v15::SigningKey::<sha2::Sha256>::new(self.key.clone());
        format!("{msg}.{}", b64u(&sk.sign(msg.as_bytes()).to_bytes()))
    }
}

/// Serves an issuer's discovery document and JWKS in memory.
struct FakeIdp(std::collections::HashMap<String, Vec<u8>>);

impl s3_server::JwksFetch for FakeIdp {
    fn get<'a>(&'a self, url: &'a str, _limit: usize) -> futures::future::BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move { self.0.get(url).cloned().ok_or_else(|| format!("{url}: 404")) })
    }
}

const ISSUER: &str = "https://login.example.com/";

fn idp_claims(sub: &str, exp: i64) -> serde_json::Value {
    json!({ "iss": ISSUER, "aud": "my-web-app", "sub": sub, "iat": now_s(), "exp": exp,
            "https://aws.amazon.com/tags": { "principal_tags": { "tenant": ["acme"] } } })
}

async fn web_role(t: &TestServer, issuer: &str, jwks_uri: Option<String>) -> String {
    let mut s = spec(Permission::Write, &[]);
    s.buckets = Some(vec!["web".into()]);
    s.trust.oidc = vec![OidcProvider { issuer: issuer.into(), audiences: vec!["my-web-app".into()], subject: Some("user|*".into()), jwks_uri }];
    t.store.create_role("browser", s).await.unwrap();
    format!("arn:aws:iam::{}:role/browser", t.store.account_id())
}

#[tokio::test]
async fn assume_role_with_web_identity() {
    let e = env().await;
    let t = &e.t;
    bucket(t, "web").await;
    let rsa = rsa_key();
    let mut docs = std::collections::HashMap::new();
    docs.insert(format!("{}/.well-known/openid-configuration", ISSUER.trim_end_matches('/')), json!({ "jwks_uri": "https://login.example.com/.well-known/jwks.json" }).to_string().into_bytes());
    docs.insert("https://login.example.com/.well-known/jwks.json".into(), rsa.jwks("k1").to_string().into_bytes());
    e.server.set_jwks_fetcher(Arc::new(FakeIdp(docs)));
    let arn = web_role(t, ISSUER, None).await;
    // Writes only under the tenant's prefix: the token's tag and audience in a bucket policy.
    let bucket_policy = json!({ "Statement": [{ "Effect": "Deny", "Principal": "*", "Action": "s3:PutObject", "NotResource": "arn:aws:s3:::web/acme/*",
        "Condition": { "StringEquals": { "aws:PrincipalTag/tenant": "acme", "login.example.com:aud": "my-web-app" } } }] });
    t.client.put_bucket_policy().bucket("web").policy(bucket_policy.to_string()).send().await.unwrap();

    let anon = sts_anonymous(&t.endpoint);
    let token = rsa.jwt("k1", idp_claims("user|123", now_s() + 300));
    let out = anon.assume_role_with_web_identity().role_arn(&arn).role_session_name("browser-1").web_identity_token(&token).send().await.unwrap();
    assert_eq!(out.subject_from_web_identity_token(), Some("user|123"));
    assert_eq!(out.audience(), Some("my-web-app"));
    assert_eq!(out.provider(), Some(ISSUER));
    let c = out.credentials().unwrap();
    let s3 = s3_with(&t.endpoint, c);
    put(&s3, "web", "acme/photo.jpg").await.unwrap();
    assert_eq!(put(&s3, "web", "globex/photo.jpg").await.unwrap_err(), "AccessDenied", "the tag and aud conditions apply");
    let id = sts(&t.endpoint, c.access_key_id(), c.secret_access_key(), Some(c.session_token())).get_caller_identity().send().await.unwrap();
    assert_eq!(id.arn(), Some(format!("arn:aws:sts::{}:assumed-role/browser/browser-1", t.store.account_id()).as_str()));

    let try_token = |tok: String| {
        let anon = anon.clone();
        let arn = arn.clone();
        async move { sts_code(anon.assume_role_with_web_identity().role_arn(arn).role_session_name("browser-2").web_identity_token(tok).send().await) }
    };
    assert_eq!(try_token(rsa.jwt("k1", idp_claims("user|1", now_s() - 3600))).await, "ExpiredTokenException");
    assert_eq!(try_token(rsa.jwt("k1", idp_claims("admin", now_s() + 300))).await, "AccessDenied", "subject pattern");
    let mut wrong_aud = idp_claims("user|1", now_s() + 300);
    wrong_aud["aud"] = "someone-else".into();
    assert_eq!(try_token(rsa.jwt("k1", wrong_aud)).await, "InvalidIdentityToken");
    let mut wrong_iss = idp_claims("user|1", now_s() + 300);
    wrong_iss["iss"] = "https://evil.example.com/".into();
    assert_eq!(try_token(rsa.jwt("k1", wrong_iss)).await, "InvalidIdentityToken");
    let good = rsa.jwt("k1", idp_claims("user|1", now_s() + 300));
    let (head, rest) = good.split_once('.').unwrap();
    let (_, sig) = rest.split_once('.').unwrap();
    let mut forged = idp_claims("user|1", now_s() + 300);
    forged["https://aws.amazon.com/tags"] = json!({ "principal_tags": { "tenant": ["globex"] } });
    assert_eq!(try_token(format!("{head}.{}.{sig}", b64u(forged.to_string().as_bytes()))).await, "InvalidIdentityToken");
    assert_eq!(try_token("not-a-jwt".into()).await, "InvalidIdentityToken");
}

/// The real fetcher: discovery and JWKS from a local HTTP server (plain HTTP allowed for tests
/// only), and the IdP being down.
#[tokio::test]
async fn web_identity_keys_over_http() {
    let e = env_with(|c| c.oidc_allow_http = true).await;
    let t = &e.t;
    bucket(t, "web").await;
    let rsa = rsa_key();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let jwks = rsa.jwks("k9").to_string();
    let discovery = json!({ "issuer": issuer, "jwks_uri": format!("{issuer}/keys") }).to_string();
    let app = axum::Router::new()
        .route("/.well-known/openid-configuration", axum::routing::get(move || std::future::ready(discovery.clone())))
        .route("/keys", axum::routing::get(move || std::future::ready(jwks.clone())));
    let idp = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let arn = web_role(t, &issuer, None).await;
    let mut claims = idp_claims("user|7", now_s() + 300);
    claims["iss"] = issuer.clone().into();
    let anon = sts_anonymous(&t.endpoint);
    let out = anon.assume_role_with_web_identity().role_arn(&arn).role_session_name("http-1").web_identity_token(rsa.jwt("k9", claims.clone())).send().await.unwrap();
    put(&s3_with(&t.endpoint, out.credentials().unwrap()), "web", "x").await.unwrap();
    // Keys are cached: the IdP going away doesn't matter for a while.
    idp.abort();
    anon.assume_role_with_web_identity().role_arn(&arn).role_session_name("http-2").web_identity_token(rsa.jwt("k9", claims.clone())).send().await.unwrap();
    // A token naming a key that was never published (refetch fails): the IdP can't be reached.
    let r = anon.assume_role_with_web_identity().role_arn(&arn).role_session_name("http-3").web_identity_token(rsa.jwt("unknown", claims)).send().await;
    assert!(["IDPCommunicationError", "InvalidIdentityToken"].contains(&sts_code(r).as_str()));
}

/// Default servers fetch keys over HTTPS only.
#[tokio::test]
async fn web_identity_needs_https() {
    let e = env().await;
    let t = &e.t;
    bucket(t, "web").await;
    let arn = web_role(t, "http://127.0.0.1:9", Some("http://127.0.0.1:9/keys".into())).await;
    let mut claims = idp_claims("user|7", now_s() + 300);
    claims["iss"] = "http://127.0.0.1:9".into();
    let r = sts_anonymous(&t.endpoint).assume_role_with_web_identity().role_arn(&arn).role_session_name("h1").web_identity_token(rsa_key().jwt("k", claims)).send().await;
    assert_eq!(sts_code(r), "IDPCommunicationError");
}

// ---- The query API itself ----

#[tokio::test]
async fn query_api_errors_and_cors() {
    let e = env().await;
    let t = &e.t;
    let http = reqwest::Client::new();
    let form = |body: &'static str| http.post(format!("{}/", t.endpoint)).header("content-type", "application/x-www-form-urlencoded").body(body);
    let r = form("Action=GetCallerIdentity&Version=2011-06-15").send().await.unwrap();
    assert_eq!(r.status(), 403);
    let body = r.text().await.unwrap();
    assert!(body.contains("<ErrorResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\">") && body.contains("<Type>Sender</Type>") && body.contains("<Code>MissingAuthenticationToken</Code>"), "{body}");
    let r = form("Action=Nope&Version=2011-06-15").send().await.unwrap();
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("<Code>InvalidAction</Code>"));
    let r = form("Action=DecodeAuthorizationMessage&Version=2011-06-15").send().await.unwrap();
    assert!(r.text().await.unwrap().contains("supported by roll"));
    let r = form("Version=2011-06-15").send().await.unwrap();
    assert!(r.text().await.unwrap().contains("<Code>MissingAction</Code>"));
    let r = form("Action=GetCallerIdentity&Version=2010-01-01").send().await.unwrap();
    assert!(r.text().await.unwrap().contains("<Code>InvalidAction</Code>"));
    // Bodies are read whole, at most 1 MiB.
    let big = "Action=AssumeRoleWithWebIdentity&WebIdentityToken=".to_string() + &"a".repeat(1_100_000);
    let r = http.post(format!("{}/", t.endpoint)).header("content-type", "application/x-www-form-urlencoded").body(big).send().await.unwrap();
    assert_eq!(r.status(), 400);
    // GET with the parameters in the query.
    let r = http.get(format!("{}/?Action=GetCallerIdentity&Version=2011-06-15", t.endpoint)).send().await.unwrap();
    assert_eq!(r.status(), 403);
    // S3 still answers `/` (ListBuckets) as before.
    assert!(t.client.list_buckets().send().await.is_ok());
    // A bad signature with service s3 in the scope is refused.
    let r = form("Action=GetCallerIdentity&Version=2011-06-15")
        .header("x-amz-date", s3_proto::time::amz_date(now_s()))
        .header("authorization", format!("AWS4-HMAC-SHA256 Credential={ACCESS}/20240101/us-east-1/s3/aws4_request, SignedHeaders=host, Signature=00"))
        .send()
        .await
        .unwrap();
    assert!(r.text().await.unwrap().contains("<Code>IncompleteSignature</Code>"));
    // Browsers.
    let r = http.request(reqwest::Method::OPTIONS, format!("{}/", t.endpoint)).header("origin", "https://app.example.com").header("access-control-request-method", "POST").header("access-control-request-headers", "content-type, x-amz-user-agent").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["access-control-allow-origin"], "*");
    assert!(r.headers()["access-control-allow-headers"].to_str().unwrap().contains("x-amz-user-agent"));
    let r = form("Action=GetCallerIdentity&Version=2011-06-15").header("origin", "https://app.example.com").send().await.unwrap();
    assert_eq!(r.headers()["access-control-allow-origin"], "*");
}
