//! Accounts, the IAM query API, MFA and Requester Pays: other accounts' buckets, objects and
//! ACL grants (by canonical id and email), cross-account bucket policies and condition keys,
//! IAM users, groups, inline and managed policies, permissions boundaries, access keys, roles
//! with trust policies (cross-account, chaining, federation), virtual MFA devices with codes
//! computed here (TOTP), MFA-authenticated sessions, MFA Delete, and Requester Pays billing.
//! IAM is called with raw SigV4-signed queries (service `iam`); STS with `aws-sdk-sts`.

mod common;

use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{BucketVersioningStatus, MfaDelete, ObjectOwnership, OwnershipControls, OwnershipControlsRule, Payer, RequestPaymentConfiguration, VersioningConfiguration};
use aws_sdk_sts::config::{BehaviorVersion, Credentials, Region};
use base64::Engine;
use common::*;
use s3_store::Permission;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

struct Env {
    t: TestServer,
    clock: Arc<AtomicI64>,
}

impl Env {
    fn advance_s(&self, s: i64) {
        self.clock.fetch_add(s * 1000, Ordering::SeqCst);
    }

    fn now_s(&self) -> i64 {
        self.clock.load(Ordering::SeqCst) / 1000
    }
}

async fn env() -> Env {
    let clock = Arc::new(AtomicI64::new(now_s() * 1000));
    let c = clock.clone();
    let (t, _) = start_full(move |s| s.clock = Arc::new(move || c.load(Ordering::SeqCst)), |_| {}).await;
    Env { t, clock }
}

/// Credentials: an access key, its secret, maybe a session token.
#[derive(Clone)]
struct Creds(String, String, Option<String>);

impl Creds {
    fn root() -> Self {
        Self(ACCESS.into(), SECRET.into(), None)
    }

    fn of(k: &s3_store::AccessKey) -> Self {
        Self(k.id.clone(), k.secret.clone(), None)
    }

    fn s3(&self, t: &TestServer) -> aws_sdk_s3::Client {
        let conf = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new(REGION))
            .endpoint_url(&t.endpoint)
            .credentials_provider(aws_sdk_s3::config::Credentials::new(&self.0, &self.1, self.2.clone(), None, "test"))
            .force_path_style(true)
            .build();
        aws_sdk_s3::Client::from_conf(conf)
    }

    fn sts(&self, t: &TestServer) -> aws_sdk_sts::Client {
        let conf = aws_sdk_sts::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(REGION))
            .endpoint_url(&t.endpoint)
            .credentials_provider(Credentials::new(&self.0, &self.1, self.2.clone(), None, "test"))
            .build();
        aws_sdk_sts::Client::from_conf(conf)
    }

    fn from_sts(c: &aws_sdk_sts::types::Credentials) -> Self {
        Self(c.access_key_id().into(), c.secret_access_key().into(), Some(c.session_token().into()))
    }

    /// An IAM query (service `iam`, SigV4) at the store's clock; `(status, body)`.
    async fn iam(&self, e: &Env, params: &[(&str, &str)]) -> (u16, String) {
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
    async fn ok(&self, e: &Env, params: &[(&str, &str)]) -> String {
        let (s, b) = self.iam(e, params).await;
        assert_eq!(s, 200, "{params:?}: {b}");
        b
    }

    /// An IAM call that must fail: `(status, code)`.
    async fn fail(&self, e: &Env, params: &[(&str, &str)]) -> (u16, String) {
        let (s, b) = self.iam(e, params).await;
        assert_ne!(s, 200, "{params:?} must fail: {b}");
        assert!(b.contains("<ErrorResponse xmlns=\"https://iam.amazonaws.com/doc/2010-05-08/\">"), "IAM's error format: {b}");
        (s, error_code(&b))
    }
}

/// The text of the first `<name>` element.
fn tag(body: &str, name: &str) -> String {
    body.split(&format!("<{name}>")).nth(1).and_then(|s| s.split(&format!("</{name}>")).next()).unwrap_or_else(|| panic!("no <{name}> in {body}")).to_string()
}

fn tags(body: &str, name: &str) -> Vec<String> {
    body.split(&format!("<{name}>")).skip(1).filter_map(|s| s.split(&format!("</{name}>")).next()).map(str::to_string).collect()
}

/// An IAM user's access key: `(id, secret)`.
fn access_key(body: &str) -> Creds {
    Creds(tag(body, "AccessKeyId"), tag(body, "SecretAccessKey"), None)
}

/// The error code of an SDK call (S3 or STS).
fn code<T: std::fmt::Debug, E: ProvideErrorMetadata + std::fmt::Debug>(r: Result<T, SdkError<E>>) -> String {
    match r {
        Ok(v) => panic!("expected an error, got {v:?}"),
        Err(e) => e.as_service_error().and_then(|s| s.code()).map(str::to_string).unwrap_or_else(|| format!("{e:?}")),
    }
}

async fn put(c: &aws_sdk_s3::Client, b: &str, k: &str) -> Result<(), String> {
    c.put_object().bucket(b).key(k).body(ByteStream::from_static(b"hi")).send().await.map(|_| ()).map_err(|e| code::<(), _>(Err(e)))
}

async fn get(c: &aws_sdk_s3::Client, b: &str, k: &str) -> Result<(), String> {
    c.get_object().bucket(b).key(k).send().await.map(|_| ()).map_err(|e| code::<(), _>(Err(e)))
}

/// Another account with its root key.
async fn other_account(t: &TestServer, name: &str) -> (s3_store::Account, Creds) {
    let a = t.store.create_account(name, Some(&format!("{name}@example.com"))).await.unwrap();
    let k = t.store.create_key_in(&a.id, "root", Permission::Admin, None).await.unwrap();
    (a, Creds::of(&k))
}

// ---- TOTP (RFC 6238), computed here ----

fn base32_decode(s: &str) -> Vec<u8> {
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

fn totp(seed: &[u8], unix_s: i64) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, seed);
    let h = ring::hmac::sign(&key, &((unix_s / 30) as u64).to_be_bytes());
    let h = h.as_ref();
    let o = (h[19] & 0x0f) as usize;
    let n = u32::from_be_bytes([h[o] & 0x7f, h[o + 1], h[o + 2], h[o + 3]]) % 1_000_000;
    format!("{n:06}")
}

/// A virtual MFA device enabled for `user` (`None`: the root user): its serial and seed.
async fn mfa_device(e: &Env, by: &Creds, name: &str, user: Option<&str>) -> (String, Vec<u8>) {
    let out = by.ok(e, &[("Action", "CreateVirtualMFADevice"), ("VirtualMFADeviceName", name)]).await;
    let serial = tag(&out, "SerialNumber");
    let seed_text = base64::engine::general_purpose::STANDARD.decode(tag(&out, "Base32StringSeed")).unwrap();
    let seed = base32_decode(std::str::from_utf8(&seed_text).unwrap());
    let png = base64::engine::general_purpose::STANDARD.decode(tag(&out, "QRCodePNG")).unwrap();
    assert!(png.starts_with(b"\x89PNG"), "a QR code PNG");
    let now = e.now_s();
    let (c1, c2) = (totp(&seed, now - 30), totp(&seed, now));
    let mut p = vec![("Action", "EnableMFADevice"), ("SerialNumber", serial.as_str()), ("AuthenticationCode1", c1.as_str()), ("AuthenticationCode2", c2.as_str())];
    if let Some(u) = user {
        p.push(("UserName", u));
    }
    // Two codes that aren't consecutive don't enable it.
    let wrong = totp(&seed, now - 300);
    let bad: Vec<(&str, &str)> = p.iter().map(|(k, v)| if *k == "AuthenticationCode2" { (*k, wrong.as_str()) } else { (*k, *v) }).collect();
    assert_eq!(by.fail(e, &bad).await, (403, "InvalidAuthenticationCode".into()));
    by.ok(e, &p).await;
    (serial, seed)
}

#[tokio::test]
async fn accounts_buckets_and_cross_account_access() {
    let e = env().await;
    let t = &e.t;
    let main = t.store.require_account(&t.store.default_account_id()).unwrap();
    t.store.update_account(&main.id, s3_store::AccountUpdate { name: Some("main".into()), email: Some(Some("Main@Example.com".into())) }).await.unwrap();
    let (alt, alt_root) = other_account(t, "alt").await;
    assert_eq!(alt.id.len(), 12);
    assert_eq!(alt.canonical.len(), 64);
    let altc = alt_root.s3(t);

    // Bucket names are global; who owns one decides the error.
    t.client.create_bucket().bucket("shared").send().await.unwrap();
    assert_eq!(code(altc.create_bucket().bucket("shared").send().await), "BucketAlreadyExists");
    t.client.create_bucket().bucket("shared").send().await.unwrap(); // your own: a no-op in us-east-1
    altc.create_bucket().bucket("alts").send().await.unwrap();
    let names = |l: &aws_sdk_s3::operation::list_buckets::ListBucketsOutput| l.buckets().iter().filter_map(|b| b.name().map(str::to_string)).collect::<Vec<_>>();
    let mine = t.client.list_buckets().send().await.unwrap();
    assert_eq!(names(&mine), vec!["shared"]);
    assert_eq!(mine.owner().unwrap().id(), Some(main.canonical.as_str()));
    let theirs = altc.list_buckets().send().await.unwrap();
    assert_eq!(names(&theirs), vec!["alts"]);
    assert_eq!(theirs.owner().unwrap().id(), Some(alt.canonical.as_str()));
    assert_eq!(theirs.owner().unwrap().display_name(), Some("alt"));

    // Another account's bucket: nothing without a grant.
    put(&t.client, "shared", "doc.txt").await.unwrap();
    assert_eq!(get(&altc, "shared", "doc.txt").await.unwrap_err(), "AccessDenied");
    assert_eq!(put(&altc, "shared", "x").await.unwrap_err(), "AccessDenied");
    assert_eq!(code(altc.list_objects_v2().bucket("shared").send().await), "AccessDenied");
    assert_eq!(code(altc.get_object().bucket("nosuch").key("k").send().await), "NoSuchBucket");

    // A bucket policy naming the other account (with the account condition keys) grants it.
    let policy = json!({"Version": "2012-10-17", "Statement": [
        {"Effect": "Allow", "Principal": {"AWS": format!("arn:aws:iam::{}:root", alt.id)}, "Action": ["s3:GetObject", "s3:PutObject"], "Resource": "arn:aws:s3:::shared/*",
         "Condition": {"StringEquals": {"aws:PrincipalAccount": alt.id, "aws:ResourceAccount": main.id, "s3:ResourceAccount": main.id}}},
        {"Effect": "Allow", "Principal": {"AWS": alt.id}, "Action": "s3:ListBucket", "Resource": "arn:aws:s3:::shared"},
    ]});
    t.client.put_bucket_policy().bucket("shared").policy(policy.to_string()).send().await.unwrap();
    get(&altc, "shared", "doc.txt").await.unwrap();
    altc.list_objects_v2().bucket("shared").send().await.unwrap();
    // x-amz-expected-bucket-owner is the bucket owner's account id.
    altc.get_object().bucket("shared").key("doc.txt").expected_bucket_owner(&main.id).send().await.unwrap();
    assert_eq!(code(altc.get_object().bucket("shared").key("doc.txt").expected_bucket_owner(&alt.id).send().await), "AccessDenied");
    t.client.head_bucket().bucket("shared").expected_bucket_owner(&main.id).send().await.unwrap();
    assert!(t.client.head_bucket().bucket("shared").expected_bucket_owner(&alt.id).send().await.is_err());

    // Cross-account: the other account's own side must allow too. An IAM user of `alt` without
    // policies gets nothing from the bucket policy naming its account.
    let created = alt_root.ok(&e, &[("Action", "CreateUser"), ("UserName", "worker")]).await;
    assert!(tag(&created, "Arn").ends_with(&format!("{}:user/worker", alt.id)));
    let worker = access_key(&alt_root.ok(&e, &[("Action", "CreateAccessKey"), ("UserName", "worker")]).await);
    assert_eq!(get(&worker.s3(t), "shared", "doc.txt").await.unwrap_err(), "AccessDenied");
    let doc = json!({"Statement": [{"Effect": "Allow", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::shared/*"}]}).to_string();
    alt_root.ok(&e, &[("Action", "PutUserPolicy"), ("UserName", "worker"), ("PolicyName", "read-shared"), ("PolicyDocument", &doc)]).await;
    get(&worker.s3(t), "shared", "doc.txt").await.unwrap();
    assert_eq!(put(&worker.s3(t), "shared", "w").await.unwrap_err(), "AccessDenied", "its policy doesn't allow writes");
    // ... and the main account's own key with a read permission level can't use alt's grant.
    let reader = t.store.create_key("reader", Permission::Read, None).await.unwrap();
    assert_eq!(get(&Creds::of(&reader).s3(t), "alts", "nothing").await.unwrap_err(), "AccessDenied");

    // Object ownership: with ObjectWriter, what alt writes is alt's; the bucket owner can't
    // read it until alt grants it (by email here), but can delete it.
    let ow = OwnershipControls::builder().rules(OwnershipControlsRule::builder().object_ownership(ObjectOwnership::ObjectWriter).build().unwrap()).build().unwrap();
    t.client.put_bucket_ownership_controls().bucket("shared").ownership_controls(ow).send().await.unwrap();
    put(&altc, "shared", "by-alt.txt").await.unwrap();
    assert_eq!(get(&t.client, "shared", "by-alt.txt").await.unwrap_err(), "AccessDenied", "not the object's owner");
    // Its owner reads its ACL (the bucket policy grants nothing here, the object's ACL does).
    let acl = altc.get_object_acl().bucket("shared").key("by-alt.txt").send().await.unwrap();
    assert_eq!(acl.owner().unwrap().id(), Some(alt.canonical.as_str()));
    altc.put_object_acl().bucket("shared").key("by-alt.txt").grant_read("emailAddress=main@example.com").send().await.unwrap();
    get(&t.client, "shared", "by-alt.txt").await.unwrap();
    let acl = t.client.get_object_acl().bucket("shared").key("by-alt.txt").send().await;
    assert_eq!(code(acl), "AccessDenied", "READ isn't READ_ACP");
    assert_eq!(code(altc.put_object_acl().bucket("shared").key("by-alt.txt").grant_read("emailAddress=nobody@example.com").send().await), "UnresolvableGrantByEmailAddress");
    // A grant by canonical id when writing.
    altc.put_object().bucket("shared").key("granted.txt").grant_read(format!("id={}", main.canonical)).body(ByteStream::from_static(b"g")).send().await.unwrap();
    get(&t.client, "shared", "granted.txt").await.unwrap();
    t.client.delete_object().bucket("shared").key("by-alt.txt").send().await.unwrap();
    // BucketOwnerPreferred with bucket-owner-full-control: the bucket owner's.
    let bop = OwnershipControls::builder().rules(OwnershipControlsRule::builder().object_ownership(ObjectOwnership::BucketOwnerPreferred).build().unwrap()).build().unwrap();
    t.client.put_bucket_ownership_controls().bucket("shared").ownership_controls(bop).send().await.unwrap();
    altc.put_object().bucket("shared").key("handed-over.txt").acl(aws_sdk_s3::types::ObjectCannedAcl::BucketOwnerFullControl).body(ByteStream::from_static(b"h")).send().await.unwrap();
    let acl = t.client.get_object_acl().bucket("shared").key("handed-over.txt").send().await.unwrap();
    assert_eq!(acl.owner().unwrap().id(), Some(main.canonical.as_str()));
    let listed = t.client.list_objects_v2().bucket("shared").fetch_owner(true).send().await.unwrap();
    let owner_of = |k: &str| listed.contents().iter().find(|o| o.key() == Some(k)).and_then(|o| o.owner()).and_then(|o| o.id()).map(str::to_string);
    assert_eq!(owner_of("granted.txt").as_deref(), Some(alt.canonical.as_str()));
    assert_eq!(owner_of("handed-over.txt").as_deref(), Some(main.canonical.as_str()));
    // With BucketOwnerEnforced again, the bucket owner owns everything.
    let boe = OwnershipControls::builder().rules(OwnershipControlsRule::builder().object_ownership(ObjectOwnership::BucketOwnerEnforced).build().unwrap()).build().unwrap();
    t.client.put_bucket_ownership_controls().bucket("shared").ownership_controls(boe).send().await.unwrap();
    let acl = t.client.get_object_acl().bucket("shared").key("granted.txt").send().await.unwrap();
    assert_eq!(acl.owner().unwrap().id(), Some(main.canonical.as_str()));
    assert_eq!(code(altc.put_object().bucket("shared").key("p").acl(aws_sdk_s3::types::ObjectCannedAcl::Private).body(ByteStream::from_static(b"p")).send().await), "AccessControlListNotSupported");
    altc.put_object().bucket("shared").key("p").acl(aws_sdk_s3::types::ObjectCannedAcl::BucketOwnerFullControl).body(ByteStream::from_static(b"p")).send().await.unwrap();

    // Whatever a policy grants, only the owner's account manages the bucket policy (405).
    let all = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"AWS": alt.id}, "Action": "s3:*", "Resource": ["arn:aws:s3:::shared", "arn:aws:s3:::shared/*"]}]});
    t.client.put_bucket_policy().bucket("shared").policy(all.to_string()).send().await.unwrap();
    fn status<T, E: std::fmt::Debug>(r: Result<T, SdkError<E, aws_sdk_s3::config::http::HttpResponse>>) -> u16 {
        match r {
            Ok(_) => 200,
            Err(e) => e.raw_response().map(|r| r.status().as_u16()).unwrap_or_else(|| panic!("{e:?}")),
        }
    }
    assert_eq!(status(altc.get_bucket_policy().bucket("shared").send().await), 405);
    assert_eq!(status(altc.delete_bucket_policy().bucket("shared").send().await), 405);
    t.client.get_bucket_policy().bucket("shared").send().await.unwrap();
    altc.get_bucket_acl().bucket("shared").send().await.unwrap();
}

#[tokio::test]
async fn iam_users_groups_and_policies() {
    let e = env().await;
    let t = &e.t;
    let root = Creds::root();
    let account = t.store.account_id();
    for b in ["photos", "other"] {
        t.client.create_bucket().bucket(b).send().await.unwrap();
        put(&t.client, b, "seed.txt").await.unwrap();
    }
    let u = root.ok(&e, &[("Action", "CreateUser"), ("UserName", "alice"), ("Path", "/eng/"), ("Tags.member.1.Key", "team"), ("Tags.member.1.Value", "red")]).await;
    assert_eq!(tag(&u, "Arn"), format!("arn:aws:iam::{account}:user/eng/alice"));
    assert!(tag(&u, "UserId").starts_with("AIDA"));
    assert_eq!(root.fail(&e, &[("Action", "CreateUser"), ("UserName", "ALICE")]).await, (409, "EntityAlreadyExists".into()), "names are case-insensitive");
    assert_eq!(root.fail(&e, &[("Action", "GetUser"), ("UserName", "bob")]).await, (404, "NoSuchEntity".into()));
    assert_eq!(root.fail(&e, &[("Action", "CreateUser"), ("UserName", "bad name")]).await.1, "ValidationError");
    let alice = access_key(&root.ok(&e, &[("Action", "CreateAccessKey"), ("UserName", "alice")]).await);
    let a = alice.s3(t);

    // No policies: nothing at all.
    assert_eq!(code(a.list_buckets().send().await), "AccessDenied");
    assert_eq!(get(&a, "photos", "seed.txt").await.unwrap_err(), "AccessDenied");
    assert_eq!(code(a.create_bucket().bucket("alices").send().await), "AccessDenied");
    // An inline policy (with a policy condition on her tags).
    let doc = json!({"Version": "2012-10-17", "Statement": [
        {"Effect": "Allow", "Action": "s3:*", "Resource": ["arn:aws:s3:::photos", "arn:aws:s3:::photos/*"], "Condition": {"StringEquals": {"aws:PrincipalTag/team": "red"}}},
        {"Effect": "Allow", "Action": "s3:ListAllMyBuckets", "Resource": "*"},
    ]})
    .to_string();
    assert_eq!(root.fail(&e, &[("Action", "PutUserPolicy"), ("UserName", "alice"), ("PolicyName", "p"), ("PolicyDocument", "{\"Statement\": 5}")]).await, (400, "MalformedPolicyDocument".into()));
    root.ok(&e, &[("Action", "PutUserPolicy"), ("UserName", "alice"), ("PolicyName", "photos"), ("PolicyDocument", &doc)]).await;
    let got = root.ok(&e, &[("Action", "GetUserPolicy"), ("UserName", "alice"), ("PolicyName", "photos")]).await;
    let decoded: String = url::form_urlencoded::parse(format!("d={}", tag(&got, "PolicyDocument")).as_bytes()).next().unwrap().1.into_owned();
    assert!(decoded.contains("s3:ListAllMyBuckets"), "URL-encoded, as IAM returns it: {decoded}");
    assert_eq!(tags(&root.ok(&e, &[("Action", "ListUserPolicies"), ("UserName", "alice")]).await, "member"), vec!["photos"]);
    put(&a, "photos", "a.jpg").await.unwrap();
    get(&a, "photos", "seed.txt").await.unwrap();
    assert_eq!(get(&a, "other", "seed.txt").await.unwrap_err(), "AccessDenied");
    assert_eq!(a.list_buckets().send().await.unwrap().buckets().len(), 2, "the account's buckets");
    // Groups: AWS's AmazonS3ReadOnlyAccess for readers.
    root.ok(&e, &[("Action", "CreateGroup"), ("GroupName", "readers")]).await;
    root.ok(&e, &[("Action", "AttachGroupPolicy"), ("GroupName", "readers"), ("PolicyArn", "arn:aws:iam::aws:policy/AmazonS3ReadOnlyAccess")]).await;
    root.ok(&e, &[("Action", "AddUserToGroup"), ("GroupName", "readers"), ("UserName", "alice")]).await;
    get(&a, "other", "seed.txt").await.unwrap();
    assert_eq!(put(&a, "other", "x").await.unwrap_err(), "AccessDenied");
    assert_eq!(tags(&root.ok(&e, &[("Action", "ListGroupsForUser"), ("UserName", "alice")]).await, "GroupName"), vec!["readers"]);
    assert!(root.ok(&e, &[("Action", "GetGroup"), ("GroupName", "readers")]).await.contains("<UserName>alice</UserName>"));
    let attached = root.ok(&e, &[("Action", "ListAttachedGroupPolicies"), ("GroupName", "readers")]).await;
    assert_eq!(tag(&attached, "PolicyName"), "AmazonS3ReadOnlyAccess");

    // A customer-managed policy as a permissions boundary: reads only, whatever else allows.
    let reads = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Action": ["s3:Get*", "s3:List*"], "Resource": "*"}]}).to_string();
    let p = root.ok(&e, &[("Action", "CreatePolicy"), ("PolicyName", "reads-only"), ("PolicyDocument", &reads), ("Description", "boundary")]).await;
    let parn = tag(&p, "Arn");
    assert_eq!(parn, format!("arn:aws:iam::{account}:policy/reads-only"));
    assert_eq!(root.fail(&e, &[("Action", "CreatePolicy"), ("PolicyName", "reads-only"), ("PolicyDocument", &reads)]).await.1, "EntityAlreadyExists");
    root.ok(&e, &[("Action", "PutUserPermissionsBoundary"), ("UserName", "alice"), ("PermissionsBoundary", &parn)]).await;
    assert_eq!(put(&a, "photos", "b.jpg").await.unwrap_err(), "AccessDenied", "the boundary");
    get(&a, "photos", "a.jpg").await.unwrap();
    root.ok(&e, &[("Action", "DeleteUserPermissionsBoundary"), ("UserName", "alice")]).await;
    put(&a, "photos", "b.jpg").await.unwrap();
    // Versions: a new default changes what attached entities may do; at most five.
    root.ok(&e, &[("Action", "AttachUserPolicy"), ("UserName", "alice"), ("PolicyArn", &parn)]).await;
    let deny_get = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Deny", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::other/*"}]}).to_string();
    let v = root.ok(&e, &[("Action", "CreatePolicyVersion"), ("PolicyArn", &parn), ("PolicyDocument", &deny_get), ("SetAsDefault", "true")]).await;
    assert_eq!(tag(&v, "VersionId"), "v2");
    assert_eq!(get(&a, "other", "seed.txt").await.unwrap_err(), "AccessDenied", "an explicit deny in v2");
    root.ok(&e, &[("Action", "SetDefaultPolicyVersion"), ("PolicyArn", &parn), ("VersionId", "v1")]).await;
    get(&a, "other", "seed.txt").await.unwrap();
    for _ in 0..3 {
        root.ok(&e, &[("Action", "CreatePolicyVersion"), ("PolicyArn", &parn), ("PolicyDocument", &reads)]).await;
    }
    assert_eq!(root.fail(&e, &[("Action", "CreatePolicyVersion"), ("PolicyArn", &parn), ("PolicyDocument", &reads)]).await, (409, "LimitExceeded".into()));
    assert_eq!(root.fail(&e, &[("Action", "DeletePolicyVersion"), ("PolicyArn", &parn), ("VersionId", "v1")]).await.1, "DeleteConflict", "the default");
    assert_eq!(root.fail(&e, &[("Action", "DeletePolicy"), ("PolicyArn", &parn)]).await.1, "DeleteConflict", "attached");
    assert_eq!(tags(&root.ok(&e, &[("Action", "ListPolicyVersions"), ("PolicyArn", &parn)]).await, "VersionId").len(), 5);
    let listed = root.ok(&e, &[("Action", "ListPolicies"), ("Scope", "Local")]).await;
    assert_eq!(tags(&listed, "PolicyName"), vec!["reads-only"]);
    assert!(root.ok(&e, &[("Action", "ListPolicies")]).await.contains("AmazonS3FullAccess"), "AWS's built-ins");

    // Access keys: at most two, deactivated keys don't authenticate, last use is recorded.
    let second = access_key(&root.ok(&e, &[("Action", "CreateAccessKey"), ("UserName", "alice")]).await);
    assert_eq!(root.fail(&e, &[("Action", "CreateAccessKey"), ("UserName", "alice")]).await, (409, "LimitExceeded".into()));
    root.ok(&e, &[("Action", "UpdateAccessKey"), ("UserName", "alice"), ("AccessKeyId", &second.0), ("Status", "Inactive")]).await;
    assert_eq!(get(&second.s3(t), "photos", "a.jpg").await.unwrap_err(), "InvalidAccessKeyId");
    root.ok(&e, &[("Action", "UpdateAccessKey"), ("UserName", "alice"), ("AccessKeyId", &second.0), ("Status", "Active")]).await;
    get(&second.s3(t), "photos", "a.jpg").await.unwrap();
    let keys = root.ok(&e, &[("Action", "ListAccessKeys"), ("UserName", "alice")]).await;
    assert_eq!(tags(&keys, "AccessKeyId").len(), 2);
    assert!(!keys.contains("SecretAccessKey"), "secrets are shown once");
    let used = root.ok(&e, &[("Action", "GetAccessKeyLastUsed"), ("AccessKeyId", &alice.0)]).await;
    assert_eq!(tag(&used, "ServiceName"), "s3");
    assert_eq!(root.fail(&e, &[("Action", "DeleteAccessKey"), ("UserName", "alice"), ("AccessKeyId", ACCESS)]).await.1, "NoSuchEntity", "not hers");

    // IAM permissions: users need iam: actions; account keys with a permission level have none.
    assert_eq!(alice.fail(&e, &[("Action", "CreateUser"), ("UserName", "mallory")]).await, (403, "AccessDenied".into()));
    let admin = json!({"Statement": [{"Effect": "Allow", "Action": ["iam:CreateUser", "iam:GetUser"], "Resource": format!("arn:aws:iam::{account}:user/*")}]}).to_string();
    root.ok(&e, &[("Action", "PutUserPolicy"), ("UserName", "alice"), ("PolicyName", "iam"), ("PolicyDocument", &admin)]).await;
    alice.ok(&e, &[("Action", "CreateUser"), ("UserName", "bob")]).await;
    assert_eq!(alice.fail(&e, &[("Action", "DeleteUser"), ("UserName", "bob")]).await.1, "AccessDenied");
    assert_eq!(tag(&alice.ok(&e, &[("Action", "GetUser")]).await, "UserName"), "alice", "herself");
    let writer = t.store.create_key("writer", Permission::Admin, Some(vec!["photos".into()])).await.unwrap();
    assert_eq!(Creds::of(&writer).fail(&e, &[("Action", "ListUsers")]).await.1, "AccessDenied");
    let summary = root.ok(&e, &[("Action", "GetAccountSummary")]).await;
    assert!(summary.contains("<entry><key>Users</key><value>2</value></entry>") && summary.contains("<entry><key>Groups</key><value>1</value></entry>"), "{summary}");

    // Deleting a user: first its keys, policies and groups (DeleteConflict).
    assert_eq!(root.fail(&e, &[("Action", "DeleteUser"), ("UserName", "alice")]).await, (409, "DeleteConflict".into()));
    for k in [&alice.0, &second.0] {
        root.ok(&e, &[("Action", "DeleteAccessKey"), ("UserName", "alice"), ("AccessKeyId", k)]).await;
    }
    for p in ["photos", "iam"] {
        root.ok(&e, &[("Action", "DeleteUserPolicy"), ("UserName", "alice"), ("PolicyName", p)]).await;
    }
    root.ok(&e, &[("Action", "DetachUserPolicy"), ("UserName", "alice"), ("PolicyArn", &parn)]).await;
    assert_eq!(root.fail(&e, &[("Action", "DeleteGroup"), ("GroupName", "readers")]).await.1, "DeleteConflict");
    root.ok(&e, &[("Action", "RemoveUserFromGroup"), ("GroupName", "readers"), ("UserName", "alice")]).await;
    root.ok(&e, &[("Action", "DeleteUser"), ("UserName", "alice")]).await;
    assert_eq!(get(&alice.s3(t), "photos", "a.jpg").await.unwrap_err(), "InvalidAccessKeyId");
    let users = root.ok(&e, &[("Action", "ListUsers")]).await;
    assert_eq!(tags(&users, "UserName"), vec!["bob"]);
}

#[tokio::test]
async fn roles_trust_policies_chaining_and_federation() {
    let e = env().await;
    let t = &e.t;
    let root = Creds::root();
    let account = t.store.account_id();
    t.client.create_bucket().bucket("data").send().await.unwrap();
    put(&t.client, "data", "x.txt").await.unwrap();
    root.ok(&e, &[("Action", "CreateUser"), ("UserName", "alice")]).await;
    let alice = access_key(&root.ok(&e, &[("Action", "CreateAccessKey"), ("UserName", "alice")]).await);
    let alice_arn = format!("arn:aws:iam::{account}:user/alice");
    let trust = |principal: serde_json::Value| json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": principal, "Action": ["sts:AssumeRole", "sts:TagSession"]}]}).to_string();

    // A role trusting alice directly: she may assume it without a policy of her own.
    let r = root.ok(&e, &[("Action", "CreateRole"), ("RoleName", "reader"), ("AssumeRolePolicyDocument", &trust(json!({"AWS": alice_arn}))), ("MaxSessionDuration", "7200")]).await;
    let reader_arn = tag(&r, "Arn");
    assert_eq!(reader_arn, format!("arn:aws:iam::{account}:role/reader"));
    assert!(tag(&r, "RoleId").starts_with("AROA"));
    root.ok(&e, &[("Action", "AttachRolePolicy"), ("RoleName", "reader"), ("PolicyArn", "arn:aws:iam::aws:policy/AmazonS3ReadOnlyAccess")]).await;
    let out = alice.sts(t).assume_role().role_arn(&reader_arn).role_session_name("s1").send().await.unwrap();
    let s1 = Creds::from_sts(out.credentials().unwrap());
    get(&s1.s3(t), "data", "x.txt").await.unwrap();
    assert_eq!(put(&s1.s3(t), "data", "y").await.unwrap_err(), "AccessDenied");
    // The session acts with the role as it is now: detaching the policy takes reads away.
    root.ok(&e, &[("Action", "DetachRolePolicy"), ("RoleName", "reader"), ("PolicyArn", "arn:aws:iam::aws:policy/AmazonS3ReadOnlyAccess")]).await;
    assert_eq!(get(&s1.s3(t), "data", "x.txt").await.unwrap_err(), "AccessDenied");
    root.ok(&e, &[("Action", "AttachRolePolicy"), ("RoleName", "reader"), ("PolicyArn", "arn:aws:iam::aws:policy/AmazonS3ReadOnlyAccess")]).await;
    get(&s1.s3(t), "data", "x.txt").await.unwrap();
    // The root user can't assume a role with a trust policy (as on AWS); nobody else trusted can.
    assert_eq!(code(root.sts(t).assume_role().role_arn(&reader_arn).role_session_name("rt").send().await), "AccessDenied");

    // A trust naming the account delegates: alice then needs sts:AssumeRole herself.
    root.ok(&e, &[("Action", "CreateRole"), ("RoleName", "delegated"), ("AssumeRolePolicyDocument", &trust(json!({"AWS": account})))]).await;
    let delegated = format!("arn:aws:iam::{account}:role/delegated");
    assert_eq!(code(alice.sts(t).assume_role().role_arn(&delegated).role_session_name("dd").send().await), "AccessDenied");
    let may = json!({"Statement": [{"Effect": "Allow", "Action": "sts:AssumeRole", "Resource": delegated}]}).to_string();
    root.ok(&e, &[("Action", "PutUserPolicy"), ("UserName", "alice"), ("PolicyName", "assume"), ("PolicyDocument", &may)]).await;
    alice.sts(t).assume_role().role_arn(&delegated).role_session_name("dd").send().await.unwrap();

    // Another account's user assumes a role here: our trust names it, its policy allows it.
    let (other, other_root) = other_account(t, "partner").await;
    other_root.ok(&e, &[("Action", "CreateUser"), ("UserName", "ci")]).await;
    let ci = access_key(&other_root.ok(&e, &[("Action", "CreateAccessKey"), ("UserName", "ci")]).await);
    let ci_arn = format!("arn:aws:iam::{}:user/ci", other.id);
    let cross_trust = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"AWS": ci_arn}, "Action": "sts:AssumeRole", "Condition": {"StringEquals": {"sts:ExternalId": "partner-123"}}}]}).to_string();
    root.ok(&e, &[("Action", "CreateRole"), ("RoleName", "partner"), ("AssumeRolePolicyDocument", &cross_trust)]).await;
    root.ok(&e, &[("Action", "AttachRolePolicy"), ("RoleName", "partner"), ("PolicyArn", "arn:aws:iam::aws:policy/AmazonS3FullAccess")]).await;
    let partner_arn = format!("arn:aws:iam::{account}:role/partner");
    assert_eq!(code(ci.sts(t).assume_role().role_arn(&partner_arn).role_session_name("pp").external_id("partner-123").send().await), "AccessDenied", "its own account's policy must allow it too");
    let ci_may = json!({"Statement": [{"Effect": "Allow", "Action": "sts:AssumeRole", "Resource": partner_arn}]}).to_string();
    other_root.ok(&e, &[("Action", "PutUserPolicy"), ("UserName", "ci"), ("PolicyName", "assume"), ("PolicyDocument", &ci_may)]).await;
    assert_eq!(code(ci.sts(t).assume_role().role_arn(&partner_arn).role_session_name("pp").send().await), "AccessDenied", "the external id");
    let p = ci.sts(t).assume_role().role_arn(&partner_arn).role_session_name("pp").external_id("partner-123").send().await.unwrap();
    let ps = Creds::from_sts(p.credentials().unwrap());
    let who = ps.sts(t).get_caller_identity().send().await.unwrap();
    assert_eq!(who.account(), Some(account.as_str()), "the role's account");
    assert_eq!(who.arn(), Some(format!("arn:aws:sts::{account}:assumed-role/partner/pp").as_str()));
    put(&ps.s3(t), "data", "from-partner").await.unwrap();

    // Role chaining: a session of `reader` assumes `chained`, for at most an hour.
    root.ok(&e, &[("Action", "CreateRole"), ("RoleName", "chained"), ("AssumeRolePolicyDocument", &trust(json!({"AWS": reader_arn}))), ("MaxSessionDuration", "43200")]).await;
    let chained = format!("arn:aws:iam::{account}:role/chained");
    assert_eq!(code(s1.sts(t).assume_role().role_arn(&chained).role_session_name("cc").duration_seconds(7200).send().await), "ValidationError");
    let c = s1.sts(t).assume_role().role_arn(&chained).role_session_name("cc").send().await.unwrap();
    assert!(c.credentials().unwrap().expiration().secs() <= e.now_s() + 3600 + 2);

    // Session policies bound the role's identity policies only: a bucket policy naming the
    // session itself still grants; one naming the role is bounded.
    let only_get = json!({"Statement": [{"Effect": "Allow", "Action": "s3:GetObject", "Resource": "*"}]}).to_string();
    let narrow = alice.sts(t).assume_role().role_arn(&reader_arn).role_session_name("narrow").policy(&only_get).send().await.unwrap();
    let ns = Creds::from_sts(narrow.credentials().unwrap()).s3(t);
    assert_eq!(code(ns.list_objects_v2().bucket("data").send().await), "AccessDenied", "the session policy");
    let by_session = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"AWS": format!("arn:aws:sts::{account}:assumed-role/reader/narrow")}, "Action": "s3:PutObject", "Resource": "arn:aws:s3:::data/*"}]});
    t.client.put_bucket_policy().bucket("data").policy(by_session.to_string()).send().await.unwrap();
    put(&ns, "data", "via-session-arn").await.unwrap();
    let by_role = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"AWS": reader_arn}, "Action": "s3:PutObject", "Resource": "arn:aws:s3:::data/*"}]});
    t.client.put_bucket_policy().bucket("data").policy(by_role.to_string()).send().await.unwrap();
    assert_eq!(put(&ns, "data", "via-role-arn").await.unwrap_err(), "AccessDenied", "bounded by the session policy");
    put(&s1.s3(t), "data", "via-role-arn").await.unwrap();
    // Revocation (AWSRevokeOlderSessions) denies older sessions.
    t.store.revoke_role_sessions("reader").await.unwrap();
    assert_eq!(get(&s1.s3(t), "data", "x.txt").await.unwrap_err(), "AccessDenied");
    // Deleting a role takes its policies first (DeleteConflict).
    assert_eq!(root.fail(&e, &[("Action", "DeleteRole"), ("RoleName", "partner")]).await, (409, "DeleteConflict".into()));
    root.ok(&e, &[("Action", "DetachRolePolicy"), ("RoleName", "partner"), ("PolicyArn", "arn:aws:iam::aws:policy/AmazonS3FullAccess")]).await;
    root.ok(&e, &[("Action", "DeleteRole"), ("RoleName", "partner")]).await;
    assert_eq!(put(&ps.s3(t), "data", "again").await.unwrap_err(), "AccessDenied", "the role is gone");
    let got = root.ok(&e, &[("Action", "GetRole"), ("RoleName", "reader")]).await;
    assert!(got.contains("<AssumeRolePolicyDocument>%7B"), "URL-encoded: {got}");

    // GetFederationToken: the caller's permissions bounded by the session policy.
    let fed_may = json!({"Statement": [{"Effect": "Allow", "Action": ["sts:GetFederationToken", "s3:*"], "Resource": "*"}]}).to_string();
    root.ok(&e, &[("Action", "PutUserPolicy"), ("UserName", "alice"), ("PolicyName", "fed"), ("PolicyDocument", &fed_may)]).await;
    let f = alice.sts(t).get_federation_token().name("bob").policy(&only_get).duration_seconds(900).send().await.unwrap();
    assert_eq!(f.federated_user().unwrap().arn(), format!("arn:aws:sts::{account}:federated-user/bob"));
    let fs = Creds::from_sts(f.credentials().unwrap());
    get(&fs.s3(t), "data", "x.txt").await.unwrap();
    assert_eq!(put(&fs.s3(t), "data", "fed").await.unwrap_err(), "AccessDenied");
    assert_eq!(fs.sts(t).get_caller_identity().send().await.unwrap().arn(), Some(format!("arn:aws:sts::{account}:federated-user/bob").as_str()));
    assert_eq!(fs.fail(&e, &[("Action", "ListUsers")]).await.1, "AccessDenied", "federated users can't call IAM");
    let nothing = alice.sts(t).get_federation_token().name("eve").send().await.unwrap();
    assert_eq!(get(&Creds::from_sts(nothing.credentials().unwrap()).s3(t), "data", "x.txt").await.unwrap_err(), "AccessDenied", "no session policy: no permissions");
    // OIDC providers and roles that trust them.
    let oidc = root.ok(&e, &[("Action", "CreateOpenIDConnectProvider"), ("Url", "https://token.actions.githubusercontent.com"), ("ClientIDList.member.1", "sts.amazonaws.com")]).await;
    let parn = tag(&oidc, "OpenIDConnectProviderArn");
    assert_eq!(parn, format!("arn:aws:iam::{account}:oidc-provider/token.actions.githubusercontent.com"));
    assert!(root.ok(&e, &[("Action", "ListOpenIDConnectProviders")]).await.contains(&parn));
    assert_eq!(tag(&root.ok(&e, &[("Action", "GetOpenIDConnectProvider"), ("OpenIDConnectProviderArn", &parn)]).await, "Url"), "token.actions.githubusercontent.com");
    root.ok(&e, &[("Action", "DeleteOpenIDConnectProvider"), ("OpenIDConnectProviderArn", &parn)]).await;
}

#[tokio::test]
async fn mfa_sessions_and_mfa_delete() {
    let e = env().await;
    let t = &e.t;
    let root = Creds::root();
    let account = t.store.account_id();
    t.client.create_bucket().bucket("vault").send().await.unwrap();
    root.ok(&e, &[("Action", "CreateUser"), ("UserName", "ops")]).await;
    let ops = access_key(&root.ok(&e, &[("Action", "CreateAccessKey"), ("UserName", "ops")]).await);
    let needs_mfa = json!({"Statement": [
        {"Effect": "Allow", "Action": "s3:*", "Resource": "*", "Condition": {"Bool": {"aws:MultiFactorAuthPresent": "true"}, "NumericLessThan": {"aws:MultiFactorAuthAge": "600"}}},
        {"Effect": "Allow", "Action": ["iam:ListMFADevices"], "Resource": "*"}
    ]})
    .to_string();
    root.ok(&e, &[("Action", "PutUserPolicy"), ("UserName", "ops"), ("PolicyName", "mfa"), ("PolicyDocument", &needs_mfa)]).await;
    assert_eq!(put(&ops.s3(t), "vault", "k").await.unwrap_err(), "AccessDenied", "no MFA");

    // A virtual MFA device for ops (codes computed here), and an MFA-authenticated session.
    let (serial, seed) = mfa_device(&e, &root, "ops", Some("ops")).await;
    assert_eq!(serial, format!("arn:aws:iam::{account}:mfa/ops"));
    let listed = root.ok(&e, &[("Action", "ListMFADevices"), ("UserName", "ops")]).await;
    assert_eq!(tag(&listed, "SerialNumber"), serial);
    e.advance_s(30);
    let code_now = totp(&seed, e.now_s());
    let gst = ops.sts(t).get_session_token().serial_number(&serial).token_code(&code_now).send().await.unwrap();
    let with_mfa = Creds::from_sts(gst.credentials().unwrap());
    put(&with_mfa.s3(t), "vault", "k").await.unwrap();
    // The same code again is refused (no replay); a session without MFA has none.
    assert_eq!(code(ops.sts(t).get_session_token().serial_number(&serial).token_code(&code_now).send().await), "AccessDenied");
    let plain = Creds::from_sts(ops.sts(t).get_session_token().send().await.unwrap().credentials().unwrap());
    assert_eq!(put(&plain.s3(t), "vault", "k2").await.unwrap_err(), "AccessDenied");
    assert_eq!(plain.fail(&e, &[("Action", "ListMFADevices")]).await.1, "InvalidClientTokenId", "GetSessionToken sessions call IAM only with MFA");
    with_mfa.ok(&e, &[("Action", "ListMFADevices")]).await;
    // MultiFactorAuthAge: ten minutes later the session's MFA is too old for the policy. (The
    // test clock stays within the 15 minutes of skew requests may have.)
    e.advance_s(630);
    assert_eq!(put(&with_mfa.s3(t), "vault", "k3").await.unwrap_err(), "AccessDenied");
    // A device in use can't be deleted; deactivated, it can.
    assert_eq!(root.fail(&e, &[("Action", "DeleteVirtualMFADevice"), ("SerialNumber", &serial)]).await, (409, "DeleteConflict".into()));
    root.ok(&e, &[("Action", "DeactivateMFADevice"), ("UserName", "ops"), ("SerialNumber", &serial)]).await;
    root.ok(&e, &[("Action", "DeleteVirtualMFADevice"), ("SerialNumber", &serial)]).await;

    // MFA Delete: only the root user, with its own device and x-amz-mfa.
    let (root_serial, root_seed) = mfa_device(&e, &root, "root-mfa", None).await;
    let mfa_header = |e: &Env| format!("{root_serial} {}", totp(&root_seed, e.now_s()));
    let enable = VersioningConfiguration::builder().status(BucketVersioningStatus::Enabled).mfa_delete(MfaDelete::Enabled).build();
    assert_eq!(code(t.client.put_bucket_versioning().bucket("vault").versioning_configuration(enable.clone()).send().await), "AccessDenied", "x-amz-mfa is required");
    e.advance_s(30);
    let admin_key = t.store.create_key("ops-admin", Permission::Admin, Some(vec!["vault".into()])).await.unwrap();
    let not_root = Creds::of(&admin_key).s3(t).put_bucket_versioning().bucket("vault").versioning_configuration(enable.clone()).mfa(mfa_header(&e)).send().await;
    assert_eq!(code(not_root), "AccessDenied", "only the root user");
    t.client.put_bucket_versioning().bucket("vault").versioning_configuration(enable.clone()).mfa(mfa_header(&e)).send().await.unwrap();
    let v = t.client.get_bucket_versioning().bucket("vault").send().await.unwrap();
    assert_eq!(v.mfa_delete(), Some(&aws_sdk_s3::types::MfaDeleteStatus::Enabled));
    // Deleting a version (or changing versioning) now takes a fresh code.
    let vid = t.client.put_object().bucket("vault").key("doc").body(ByteStream::from_static(b"v1")).send().await.unwrap().version_id().unwrap().to_string();
    t.client.delete_object().bucket("vault").key("doc").send().await.unwrap(); // a delete marker is fine
    assert_eq!(code(t.client.delete_object().bucket("vault").key("doc").version_id(&vid).send().await), "AccessDenied");
    e.advance_s(30);
    let header = mfa_header(&e);
    assert_eq!(code(t.client.delete_object().bucket("vault").key("doc").version_id(&vid).mfa(format!("{root_serial} 000000")).send().await), "AccessDenied", "a wrong code");
    t.client.delete_object().bucket("vault").key("doc").version_id(&vid).mfa(&header).send().await.unwrap();
    let vid2 = t.client.put_object().bucket("vault").key("doc").body(ByteStream::from_static(b"v2")).send().await.unwrap().version_id().unwrap().to_string();
    assert_eq!(code(t.client.delete_object().bucket("vault").key("doc").version_id(&vid2).mfa(&header).send().await), "AccessDenied", "a used code can't be replayed");
    let suspend = VersioningConfiguration::builder().status(BucketVersioningStatus::Suspended).build();
    assert_eq!(code(t.client.put_bucket_versioning().bucket("vault").versioning_configuration(suspend.clone()).send().await), "AccessDenied");
    e.advance_s(30);
    let off = VersioningConfiguration::builder().status(BucketVersioningStatus::Suspended).mfa_delete(MfaDelete::Disabled).build();
    t.client.put_bucket_versioning().bucket("vault").versioning_configuration(off).mfa(mfa_header(&e)).send().await.unwrap();
    t.client.delete_object().bucket("vault").key("doc").version_id(&vid2).send().await.unwrap();
    assert_eq!(t.client.get_bucket_versioning().bucket("vault").send().await.unwrap().mfa_delete(), Some(&aws_sdk_s3::types::MfaDeleteStatus::Disabled));
}

#[tokio::test]
async fn requester_pays() {
    let e = env().await;
    let t = &e.t;
    let (other, other_root) = other_account(t, "payer").await;
    let oc = other_root.s3(t);
    t.client.create_bucket().bucket("datasets").send().await.unwrap();
    t.client.put_object().bucket("datasets").key("big.csv").body(ByteStream::from(vec![7u8; 10_000])).send().await.unwrap();
    let anyone = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": "*", "Action": ["s3:GetObject", "s3:ListBucket"], "Resource": ["arn:aws:s3:::datasets", "arn:aws:s3:::datasets/*"]}]});
    t.client.put_bucket_policy().bucket("datasets").policy(anyone.to_string()).send().await.unwrap();
    get(&oc, "datasets", "big.csv").await.unwrap();
    t.client
        .put_bucket_request_payment()
        .bucket("datasets")
        .request_payment_configuration(RequestPaymentConfiguration::builder().payer(Payer::Requester).build().unwrap())
        .send()
        .await
        .unwrap();
    let bucket = t.store.bucket("datasets").unwrap();
    t.store.flush_meter().unwrap();

    // Another account must accept the charges.
    assert_eq!(get(&oc, "datasets", "big.csv").await.unwrap_err(), "AccessDenied");
    assert_eq!(code(oc.list_objects_v2().bucket("datasets").send().await), "AccessDenied");
    let got = oc.get_object().bucket("datasets").key("big.csv").request_payer(aws_sdk_s3::types::RequestPayer::Requester).send().await.unwrap();
    assert_eq!(got.request_charged(), Some(&aws_sdk_s3::types::RequestCharged::Requester));
    assert_eq!(got.body.collect().await.unwrap().into_bytes().len(), 10_000);
    oc.list_objects_v2().bucket("datasets").request_payer(aws_sdk_s3::types::RequestPayer::Requester).send().await.unwrap();
    let paid = t.store.meter.pending_paid(&other.id, &bucket.id);
    assert_eq!((paid.class_a, paid.class_b, paid.egress_internal), (1, 1, 10_000), "billed to the requester: {paid:?}");
    assert_eq!(t.store.meter.pending(&bucket.id).class_b, 0, "not to the owner");
    // Anonymous requests can't pay: refused.
    let anon = reqwest::Client::new().get(format!("{}/datasets/big.csv", t.endpoint)).header("x-amz-request-payer", "requester").send().await.unwrap();
    assert_eq!(anon.status().as_u16(), 403);
    // The owner's own requests: the owner pays, nothing charged.
    let own = t.client.get_object().bucket("datasets").key("big.csv").send().await.unwrap();
    assert_eq!(own.request_charged(), None);
    assert_eq!(t.store.meter.pending(&bucket.id).class_b, 1);
    // Flushed per account.
    t.store.flush_meter().unwrap();
    let day = s3_store::day_of(t.store.now());
    let rows = t.store.usage_paid(Some(&other.id), None, &day).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].bucket_id.as_str(), rows[0].class_b, rows[0].egress_internal), (bucket.id.as_str(), 1, 10_000));
}
