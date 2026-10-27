//! IAM test helpers shared by `identity.rs` and `saml.rs` (included with `#[path]`): a server
//! with a test clock, credentials that call S3, STS and IAM (raw SigV4-signed IAM queries), and
//! TOTP codes computed here.

#![allow(dead_code)]

use crate::common::*;
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_sts::config::{BehaviorVersion, Credentials, Region};
use base64::Engine;
use s3_store::Permission;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

pub struct Env {
    pub t: TestServer,
    pub server: Arc<s3_server::Server>,
    pub clock: Arc<AtomicI64>,
}

impl Env {
    pub fn advance_s(&self, s: i64) {
        self.clock.fetch_add(s * 1000, Ordering::SeqCst);
    }

    pub fn now_s(&self) -> i64 {
        self.clock.load(Ordering::SeqCst) / 1000
    }

    pub fn account(&self) -> String {
        self.t.store.default_account_id()
    }
}

pub async fn env() -> Env {
    env_with(|_| {}).await
}

pub async fn env_with(tweak: impl FnOnce(&mut s3_server::Config)) -> Env {
    let clock = Arc::new(AtomicI64::new(now_s() * 1000));
    let c = clock.clone();
    let (t, server) = start_full(move |s| s.clock = Arc::new(move || c.load(Ordering::SeqCst)), tweak).await;
    Env { t, server, clock }
}

/// Credentials: an access key, its secret, maybe a session token.
#[derive(Clone)]
pub struct Creds(pub String, pub String, pub Option<String>);

impl Creds {
    pub fn root() -> Self {
        Self(ACCESS.into(), SECRET.into(), None)
    }

    pub fn of(k: &s3_store::AccessKey) -> Self {
        Self(k.id.clone(), k.secret.clone(), None)
    }

    pub fn from_sts(c: &aws_sdk_sts::types::Credentials) -> Self {
        Self(c.access_key_id().into(), c.secret_access_key().into(), Some(c.session_token().into()))
    }

    pub fn s3(&self, t: &TestServer) -> aws_sdk_s3::Client {
        let conf = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new(REGION))
            .endpoint_url(&t.endpoint)
            .credentials_provider(aws_sdk_s3::config::Credentials::new(&self.0, &self.1, self.2.clone(), None, "test"))
            .force_path_style(true)
            .build();
        aws_sdk_s3::Client::from_conf(conf)
    }

    pub fn sts(&self, t: &TestServer) -> aws_sdk_sts::Client {
        let conf = aws_sdk_sts::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(REGION))
            .endpoint_url(&t.endpoint)
            .credentials_provider(Credentials::new(&self.0, &self.1, self.2.clone(), None, "test"))
            .build();
        aws_sdk_sts::Client::from_conf(conf)
    }

    /// An IAM query (service `iam`, SigV4) at the store's clock: `(status, body)`.
    pub async fn iam(&self, e: &Env, params: &[(&str, &str)]) -> (u16, String) {
        let t = &e.t;
        let mut form = url::form_urlencoded::Serializer::new(String::new());
        form.append_pair("Version", "2010-05-08");
        for (k, v) in params {
            form.append_pair(k, v);
        }
        let body = form.finish().replace('+', "%20");
        let amz_date = s3_proto::time::amz_date(e.now_s());
        let date = &amz_date[..8];
        let host = format!("127.0.0.1:{}", t.port);
        let mut headers = vec![("content-type", "application/x-www-form-urlencoded; charset=utf-8".to_string()), ("host", host), ("x-amz-date", amz_date.clone())];
        if let Some(tok) = &self.2 {
            headers.push(("x-amz-security-token", tok.clone()));
        }
        let names: Vec<&str> = headers.iter().map(|(k, _)| *k).collect();
        let canonical = format!("POST\n/\n\n{}\n\n{}\n{}", headers.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join("\n"), names.join(";"), sha256_hex(body.as_bytes()));
        let scope = format!("{date}/{REGION}/iam/aws4_request");
        let sig = s3_proto::sigv4::sign(&s3_proto::sigv4::signing_key(&self.1, date, REGION, "iam"), &s3_proto::sigv4::string_to_sign(&amz_date, &scope, &canonical));
        let auth = format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={}, Signature={sig}", self.0, names.join(";"));
        let mut req = reqwest::Client::new().post(format!("{}/", t.endpoint)).header("authorization", auth);
        for (k, v) in &headers {
            if *k != "host" {
                req = req.header(*k, v);
            }
        }
        let r = req.body(body).send().await.unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }

    /// An IAM call that must succeed: its body.
    pub async fn ok(&self, e: &Env, params: &[(&str, &str)]) -> String {
        let (s, b) = self.iam(e, params).await;
        assert_eq!(s, 200, "{params:?}: {b}");
        b
    }

    /// An IAM call that must fail: `(status, code)`.
    pub async fn fail(&self, e: &Env, params: &[(&str, &str)]) -> (u16, String) {
        let (s, b) = self.iam(e, params).await;
        assert_ne!(s, 200, "{params:?} must fail: {b}");
        assert!(b.contains("<ErrorResponse xmlns=\"https://iam.amazonaws.com/doc/2010-05-08/\">"), "IAM's error format: {b}");
        (s, error_code(&b))
    }

    /// An IAM call that must fail: its message.
    pub async fn fail_message(&self, e: &Env, params: &[(&str, &str)]) -> (String, String) {
        let (s, b) = self.iam(e, params).await;
        assert_ne!(s, 200, "{params:?} must fail: {b}");
        (error_code(&b), tag(&b, "Message"))
    }
}

/// The text of the first `<name>` element.
pub fn tag(body: &str, name: &str) -> String {
    body.split(&format!("<{name}>")).nth(1).and_then(|s| s.split(&format!("</{name}>")).next()).unwrap_or_else(|| panic!("no <{name}> in {body}")).to_string()
}

pub fn tags(body: &str, name: &str) -> Vec<String> {
    body.split(&format!("<{name}>")).skip(1).filter_map(|s| s.split(&format!("</{name}>")).next()).map(str::to_string).collect()
}

/// An IAM user's access key.
pub fn access_key(body: &str) -> Creds {
    Creds(tag(body, "AccessKeyId"), tag(body, "SecretAccessKey"), None)
}

/// The error code of an SDK call (S3 or STS).
pub fn code<T: std::fmt::Debug, E: ProvideErrorMetadata + std::fmt::Debug>(r: Result<T, SdkError<E>>) -> String {
    match r {
        Ok(v) => panic!("expected an error, got {v:?}"),
        Err(e) => e.as_service_error().and_then(|s| s.code()).map(str::to_string).unwrap_or_else(|| format!("{e:?}")),
    }
}

/// The error message of an SDK call.
pub fn message<T: std::fmt::Debug, E: ProvideErrorMetadata + std::fmt::Debug>(r: Result<T, SdkError<E>>) -> String {
    match r {
        Ok(v) => panic!("expected an error, got {v:?}"),
        Err(e) => e.as_service_error().and_then(|s| s.message()).map(str::to_string).unwrap_or_else(|| format!("{e:?}")),
    }
}

/// Another account with its root key.
pub async fn other_account(t: &TestServer, name: &str) -> (s3_store::Account, Creds) {
    let a = t.store.create_account(name, Some(&format!("{name}@example.com"))).await.unwrap();
    let k = t.store.create_key_in(&a.id, "root", Permission::Admin, None).await.unwrap();
    (a, Creds::of(&k))
}

/// A user with an access key.
pub async fn user(e: &Env, name: &str) -> Creds {
    let root = Creds::root();
    root.ok(e, &[("Action", "CreateUser"), ("UserName", name)]).await;
    access_key(&root.ok(e, &[("Action", "CreateAccessKey"), ("UserName", name)]).await)
}

// ---- TOTP (RFC 6238), computed here ----

pub fn base32_decode(s: &str) -> Vec<u8> {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let (mut buf, mut bits, mut out) = (0u64, 0u32, Vec::new());
    for c in s.trim_end_matches('=').bytes() {
        buf = (buf << 5) | A.iter().position(|x| *x == c).expect("base32") as u64;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    out
}

pub fn totp(seed: &[u8], unix_s: i64) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, seed);
    let h = ring::hmac::sign(&key, &((unix_s / 30) as u64).to_be_bytes());
    let h = h.as_ref();
    let o = (h[19] & 0x0f) as usize;
    let n = u32::from_be_bytes([h[o] & 0x7f, h[o + 1], h[o + 2], h[o + 3]]) % 1_000_000;
    format!("{n:06}")
}

/// A virtual MFA device enabled for `user` (`None`: the root user): its serial and seed.
pub async fn mfa_device(e: &Env, by: &Creds, name: &str, user: Option<&str>) -> (String, Vec<u8>) {
    let out = by.ok(e, &[("Action", "CreateVirtualMFADevice"), ("VirtualMFADeviceName", name)]).await;
    let serial = tag(&out, "SerialNumber");
    let seed_text = base64::engine::general_purpose::STANDARD.decode(tag(&out, "Base32StringSeed")).unwrap();
    let seed = base32_decode(std::str::from_utf8(&seed_text).unwrap());
    let now = e.now_s();
    let (c1, c2) = (totp(&seed, now - 30), totp(&seed, now));
    let mut p = vec![("Action", "EnableMFADevice"), ("SerialNumber", serial.as_str()), ("AuthenticationCode1", c1.as_str()), ("AuthenticationCode2", c2.as_str())];
    if let Some(u) = user {
        p.push(("UserName", u));
    }
    by.ok(e, &p).await;
    // The enabling codes are used: the next code is a step later.
    e.advance_s(30);
    (serial, seed)
}

/// URL-decoded (IAM returns policy documents URL-encoded).
pub fn decoded(s: &str) -> String {
    s3_proto::http::percent_decode_utf8(s).unwrap()
}
