//! `/v1/secrets` in process, on SQLite in a tempdir (docs/secrets.md), next to the workspace
//! router it reads projects from. One test per flow a person relies on.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use roll_secrets::links::{Field, Issued, Link, LinkProvider, Links, Mode, Planted};
use roll_secrets::syncs::{Push, Resources, SyncField, SyncTarget, Syncs};
use roll_secrets::{Keyring, Options, Secrets, Value as Secret};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

/// A fake product: a database whose password rotates, and access keys it issues and revokes.
#[derive(Default)]
struct Fake {
    password: Mutex<u32>,
    issued: Mutex<u32>,
    live_keys: Mutex<Vec<String>>,
    /// The product doesn't answer.
    down: std::sync::atomic::AtomicBool,
    /// Every resource it was asked to read or issue for, in order.
    asked: Mutex<Vec<String>>,
}

impl LinkProvider for Fake {
    fn kind(&self) -> &str {
        "bucket"
    }
    fn fields(&self) -> &[Field] {
        const F: &[Field] = &[
            Field { field: "url", label: "URL", modes: &[Mode::Live], secret: true, suggest: "BUCKET_URL", credential: None },
            Field { field: "access_key_id", label: "Access key id", modes: &[Mode::Managed, Mode::Dynamic], secret: false, suggest: "AWS_ACCESS_KEY_ID", credential: Some("key") },
            Field { field: "secret_access_key", label: "Secret", modes: &[Mode::Managed, Mode::Dynamic], secret: true, suggest: "AWS_SECRET_ACCESS_KEY", credential: Some("key") },
        ];
        F
    }
    fn suggested(&self) -> &[(&'static str, Mode)] {
        &[("url", Mode::Live), ("access_key_id", Mode::Managed), ("secret_access_key", Mode::Managed)]
    }
    fn read(&self, r#ref: &str, _: &str, _: &Link) -> roll_secrets::Result<Secret> {
        self.asked.lock().unwrap().push(r#ref.to_string());
        if self.down.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(roll_secrets::Error::Link("bucket: timed out".into()));
        }
        Ok(Secret::new(format!("s3://{ref}?pw={}", self.password.lock().unwrap())))
    }
    fn issue(&self, r#ref: &str, _: &str, _: &Link, _: &str, _: Option<u64>) -> roll_secrets::Result<Issued> {
        self.asked.lock().unwrap().push(r#ref.to_string());
        let mut n = self.issued.lock().unwrap();
        *n += 1;
        let id = format!("AK{n}");
        self.live_keys.lock().unwrap().push(id.clone());
        let parts = BTreeMap::from([("access_key_id".to_string(), Secret::new(id.clone())), ("secret_access_key".to_string(), Secret::new(format!("sk-{n}")))]);
        Ok(Issued { expires_at: None, parts, handle: json!({ "id": id }) })
    }
    fn revoke(&self, _: &str, handle: &Value) -> roll_secrets::Result<()> {
        self.live_keys.lock().unwrap().retain(|k| Some(k.as_str()) != handle["id"].as_str());
        Ok(())
    }
    fn rotate_source(&self, _: &str) -> roll_secrets::Result<bool> {
        *self.password.lock().unwrap() += 1;
        Ok(true)
    }
}

/// A fake roll Postgres, for decoys: the decoy roles it holds, by handle (`<database>/<role>`).
#[derive(Default)]
struct FakePg(Mutex<std::collections::BTreeSet<String>>);

impl LinkProvider for FakePg {
    fn kind(&self) -> &str {
        "postgres"
    }
    fn fields(&self) -> &[Field] {
        &[]
    }
    fn read(&self, _: &str, _: &str, _: &Link) -> roll_secrets::Result<Secret> {
        Err(roll_secrets::Error::Link("no live fields here".into()))
    }
    fn plant_decoy(&self, r#ref: &str) -> roll_secrets::Result<Planted> {
        let mut roles = self.0.lock().unwrap();
        let role = format!("roll_fakedecoy{}", roles.len());
        roles.insert(format!("{ref}/{role}"));
        Ok(Planted { value: Secret::new(format!("postgres://{role}:fake-decoy-password@db.example.test:5432/main")), handle: format!("{ref}/{role}") })
    }
    fn remove_decoy(&self, handle: &str) -> roll_secrets::Result<()> {
        self.0.lock().unwrap().remove(handle);
        Ok(())
    }
}

/// A push a fake platform got: its values and credentials (`credential:<key>`), exposed;
/// changed and removed names.
type Pushed = (BTreeMap<String, String>, Vec<String>, Vec<String>);

/// A fake outside platform: the pushes it got.
#[derive(Default)]
struct FakeSync {
    pushes: Mutex<Vec<Pushed>>,
}

impl SyncTarget for FakeSync {
    fn kind(&self) -> &str {
        "fake"
    }
    fn label(&self) -> &str {
        "Fake"
    }
    fn fields(&self) -> &[SyncField] {
        &[]
    }
    fn signed(&self) -> bool {
        true
    }
    fn credentials(&self, target: &Value) -> Vec<String> {
        target["token_key"].as_str().map(String::from).into_iter().collect()
    }
    fn check(&self, _: &str, _: &Resources, target: &Value) -> roll_secrets::Result<()> {
        target.get("name").map(|_| ()).ok_or_else(|| roll_secrets::Error::BadRequest("target needs a name".into()))
    }
    fn refuses(&self, name: &str) -> Option<String> {
        name.starts_with("FAKE_").then(|| format!("{name}: FAKE_ is the platform's"))
    }
    fn push(&self, _: &Value, p: &Push<'_>) -> roll_secrets::Result<()> {
        let credentials = p.credentials.iter().map(|(k, v)| (format!("credential:{k}"), v.expose().to_string()));
        let values = p.values.iter().map(|(k, v)| (k.clone(), v.expose().to_string())).chain(credentials).collect();
        self.pushes.lock().unwrap().push((values, p.changed.to_vec(), p.removed.to_vec()));
        Ok(())
    }
}

/// What a fake certificate platform got: certificate, private key, chain, the id it was given,
/// and the AWS session's key id it signed in with.
type CertPushed = (String, String, String, Option<String>, Option<String>);

/// A fake certificate platform (`aws-acm`'s shape): the first push makes `arn:fake:1`, later ones
/// replace what they're given.
#[derive(Default)]
struct FakeCertSync {
    pushes: Mutex<Vec<CertPushed>>,
}

impl SyncTarget for FakeCertSync {
    fn kind(&self) -> &str {
        "fake-cert"
    }
    fn label(&self) -> &str {
        "Fake certificates"
    }
    fn fields(&self) -> &[SyncField] {
        &[]
    }
    fn certificate(&self) -> bool {
        true
    }
    fn aws_role(&self, target: &Value) -> Option<String> {
        target["role"].as_str().map(String::from)
    }
    fn check(&self, _: &str, _: &Resources, _: &Value) -> roll_secrets::Result<()> {
        Ok(())
    }
    fn push(&self, _: &Value, _: &Push<'_>) -> roll_secrets::Result<()> {
        Err(roll_secrets::Error::BadRequest("certificates only".into()))
    }
    fn push_certificate(&self, _: &Value, p: &Push<'_>, c: &roll_secrets::syncs::Certificate, remote: Option<&str>) -> roll_secrets::Result<String> {
        let aws = p.aws.map(|s| s.access_key_id.expose().to_string());
        self.pushes.lock().unwrap().push((c.certificate.expose().into(), c.private_key.expose().into(), c.chain.expose().into(), remote.map(String::from), aws));
        Ok(remote.unwrap_or("arn:fake:1").to_string())
    }
}

/// Audit deliveries, kept instead of sent; an OIDC issuer's keys, served; vendors' APIs; a
/// Snowflake account's SQL API; MongoDB Atlas's Admin API; and STS, answering AWS identities'
/// logins with the caller set in `.8`; the providers validity checks ask (`.9`); and GitLab's,
/// OpenAI's and Datadog's key APIs (`.10`).
#[derive(Default)]
struct Sent(Mutex<Vec<Value>>, Mutex<Option<Value>>, std::sync::atomic::AtomicBool, Mutex<Option<(String, String)>>, Mutex<BTreeMap<(String, String), Value>>, Mutex<Vendors>, FakeSnowflake, FakeAtlas, Mutex<FakeStsLogin>, FakeProviders, FakeKeyVendors);

/// A self-managed GitLab's access tokens for project `acme/web`, OpenAI's service accounts in one
/// project, Datadog's keys on its EU site, an account's Cloudflare API tokens, a Databricks
/// workspace's tokens and Confluent Cloud's API keys, as their docs describe them, each taking only
/// its admin credential (all obviously fake): the key ids live, every request body, and whether the
/// admin credentials still work (`refuse`).
#[derive(Default)]
struct FakeKeyVendors {
    live: Mutex<Vec<String>>,
    made: std::sync::atomic::AtomicU32,
    bodies: Mutex<Vec<Value>>,
    refuse: std::sync::atomic::AtomicBool,
    /// The admin credential each GitLab request came with.
    gitlab_admins: Mutex<Vec<String>>,
}

const GITLAB_TOKENS: &str = "https://gitlab.example.com/api/v4/projects/acme%2Fweb/access_tokens";
const OPENAI_ACCOUNTS: &str = "https://api.openai.com/v1/organization/projects/proj_rollfake/service_accounts";
const DATADOG_API: &str = "https://api.datadoghq.eu/api/v2/";
const CLOUDFLARE_TOKENS: &str = "https://api.cloudflare.com/client/v4/accounts/rollfakeaccount/tokens";
const DATABRICKS_TOKENS: &str = "https://dbc-rollfake.cloud.databricks.com/api/2.0/token/";
const CONFLUENT_KEYS: &str = "https://api.confluent.cloud/iam/v2/api-keys";

/// The Cloudflare template's policies, as Cloudflare answers them (permission groups named too).
fn cloudflare_policies(named: bool) -> Value {
    let group = if named { json!({ "id": "fakepermissiongroup1", "name": "Workers Scripts Write" }) } else { json!({ "id": "fakepermissiongroup1" }) };
    json!([{ "effect": "allow", "resources": { "com.cloudflare.api.account.rollfakeaccount": "*" }, "permission_groups": [group] }])
}

impl FakeKeyVendors {
    fn answer(&self, method: &str, url: &str, headers: &[(&str, &Secret)], body: Option<(&str, &Secret)>) -> Option<(u16, Value)> {
        use std::sync::atomic::Ordering::Relaxed;
        let header = |name: &str| headers.iter().find(|(k, _)| *k == name).map(|(_, v)| v.expose().to_string()).unwrap_or_default();
        let (vendor, rest, authorized) = if let Some(rest) = url.strip_prefix(GITLAB_TOKENS) {
            self.gitlab_admins.lock().unwrap().push(header("private-token"));
            ("gitlab", rest, ["fake-gitlab-admin", "fake-gitlab-admin-2"].contains(&header("private-token").as_str()))
        } else if let Some(rest) = url.strip_prefix(OPENAI_ACCOUNTS) {
            ("openai", rest, header("authorization") == "Bearer fake-openai-admin")
        } else if let Some(rest) = url.strip_prefix(CLOUDFLARE_TOKENS) {
            ("cloudflare", rest, header("authorization") == "Bearer fake-cf-admin")
        } else if let Some(rest) = url.strip_prefix(DATABRICKS_TOKENS) {
            ("databricks", rest, header("authorization") == "Bearer fake-dbx-admin")
        } else if let Some(rest) = url.strip_prefix(CONFLUENT_KEYS) {
            use base64::Engine;
            ("confluent", rest, header("authorization") == format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("FAKECONFLUENTADMIN:fake-confluent-admin")))
        } else {
            let rest = url.strip_prefix(DATADOG_API)?;
            ("datadog", rest, header("dd-api-key") == "fake-dd-api" && header("dd-application-key") == "fake-dd-app")
        };
        if !authorized || self.refuse.load(Relaxed) {
            return Some((if vendor == "datadog" { 403 } else { 401 }, json!({ "message": "401 Unauthorized" })));
        }
        let body: Value = body.map(|(t, b)| {
            assert_eq!(t, "application/json");
            serde_json::from_str(b.expose()).unwrap()
        }).unwrap_or(Value::Null);
        let mut live = self.live.lock().unwrap();
        let made = || {
            let n = self.made.fetch_add(1, Relaxed) + 1;
            self.bodies.lock().unwrap().push(body.clone());
            n
        };
        Some(match (vendor, method, rest) {
            // Registering checks the credential, read only.
            ("gitlab", "GET", "?per_page=1") | ("openai", "GET", "?limit=1") => (200, json!([])),
            ("datadog", "GET", "api_keys?page%5Bsize%5D=1" | "current_user/application_keys?page%5Bsize%5D=1") | ("confluent", "GET", "?spec.owner=sa-rollfake&page_size=1") => (200, json!({ "data": [] })),
            ("databricks", "GET", "list") => (200, json!({ "token_infos": [] })),
            // The template, read at registration and before each new token.
            ("cloudflare", "GET", "/tmplrollfake") => {
                (200, json!({ "success": true, "errors": [], "result": { "id": "tmplrollfake", "name": "template", "status": "disabled", "policies": cloudflare_policies(true), "condition": { "request.ip": { "in": ["192.0.2.0/24"] } } } }))
            }
            ("cloudflare", "POST", "") => {
                assert_eq!((&body["policies"], &body["condition"]["request.ip"]["in"][0]), (&cloudflare_policies(false), &json!("192.0.2.0/24")), "the template's policies and condition: {body}");
                let n = made();
                live.push(format!("cftokfake{n}"));
                (200, json!({ "success": true, "errors": [], "result": { "id": format!("cftokfake{n}"), "name": body["name"], "status": "active", "value": format!("fake-cf-token-{n}") } }))
            }
            ("databricks", "POST", "create") => {
                assert_eq!(body["lifetime_seconds"], 30 * 86_400, "{body}");
                let n = made();
                live.push(format!("dbxtokfake{n}"));
                (200, json!({ "token_value": format!("fake-dbx-token-{n}"), "token_info": { "token_id": format!("dbxtokfake{n}"), "comment": body["comment"] } }))
            }
            ("databricks", "POST", "delete") => {
                let id = body["token_id"].as_str().unwrap().to_string();
                let found = live.contains(&id);
                live.retain(|k| *k != id);
                if found { (200, json!({})) } else { (404, json!({ "error_code": "RESOURCE_DOES_NOT_EXIST" })) }
            }
            ("confluent", "POST", "") => {
                assert_eq!((&body["spec"]["owner"], &body["spec"]["resource"]), (&json!({ "id": "sa-rollfake" }), &json!({ "id": "lkc-rollfake", "environment": "env-rollfake" })), "{body}");
                let n = made();
                live.push(format!("CONFLUENTFAKE{n}"));
                (202, json!({ "api_version": "iam/v2", "kind": "ApiKey", "id": format!("CONFLUENTFAKE{n}"), "spec": { "secret": format!("fake-confluent-secret-{n}"), "display_name": body["spec"]["display_name"] } }))
            }
            ("gitlab", "POST", "") => {
                assert!(body["expires_at"].as_str().is_some_and(|d| d.len() == 10) && body["scopes"] == json!(["read_api"]), "{body}");
                let n = made();
                live.push(format!("{n}"));
                (201, json!({ "id": n, "name": body["name"], "token": format!("fake-gitlab-token-{n}"), "active": true }))
            }
            ("openai", "POST", "") => {
                let n = made();
                let id = format!("svc_acct_fake{n}");
                live.push(id.clone());
                (200, json!({ "object": "organization.project.service_account", "id": id, "name": body["name"], "role": "member", "api_key": { "object": "organization.project.service_account.api_key", "value": format!("fake-openai-key-{n}"), "id": format!("key_fake{n}") } }))
            }
            ("datadog", "POST", "api_keys" | "current_user/application_keys") => {
                let kind = if rest == "api_keys" { "api_keys" } else { "application_keys" };
                assert_eq!(body["data"]["type"], kind, "{body}");
                let n = made();
                let id = format!("00000000-0000-0000-0000-{n:012}");
                live.push(id.clone());
                (201, json!({ "data": { "type": kind, "id": id, "attributes": { "name": body["data"]["attributes"]["name"], "key": format!("fake-dd-{kind}-{n}") } } }))
            }
            (_, "DELETE", r) => {
                let id = r.trim_start_matches('/').rsplit('/').next().unwrap().to_string();
                let found = live.contains(&id);
                live.retain(|k| *k != id);
                (if found { 204 } else { 404 }, Value::Null)
            }
            _ => (404, json!({ "message": "404 Not Found" })),
        })
    }
}

/// Fixture credentials for validity checks, built from pieces (push protection scans this repo):
/// obviously roll's, still each detector's shape. `live` ones work, the rest are refused.
struct Creds {
    github_live: String,
    github_dead: String,
    github_moved: String,
    aws_id: String,
    aws_secret: String,
    stripe: String,
    slack: String,
    openai: String,
    anthropic: String,
}

fn creds() -> Creds {
    let tail = "0123456789abcdef";
    Creds {
        github_live: ["ghp_", "RollFixtureLiveToken", tail].concat(),
        github_dead: ["ghp_", "RollFixtureDeadToken", tail].concat(),
        github_moved: ["ghp_", "RollFixtureRedirects", tail].concat(),
        aws_id: ["AKIA", "ROLLFIXTURE", "23456"].concat(),
        aws_secret: ["rollFixture", "SecretKey", "0123456789", "abcdefghij"].concat(),
        stripe: ["sk_", "live_", "RollFixtureLive", tail].concat(),
        slack: ["xoxb-", "1234567890-", "RollFixtureLive"].concat(),
        openai: ["sk-", "proj-", "RollFixtureLive", tail, "gh"].concat(),
        anthropic: ["sk-", "ant-", "RollFixtureLive", tail, "gh"].concat(),
    }
}

/// The providers validity checks ask, answering as each API does: every request kept (URL and
/// header names) to show nothing but the credential went.
#[derive(Default)]
struct FakeProviders(Mutex<Vec<(String, Vec<String>)>>);

impl FakeProviders {
    fn answer(&self, method: &str, url: &str, headers: &[(&str, &Secret)]) -> Option<(u16, Value)> {
        let c = creds();
        let header = |n: &str| headers.iter().find(|(h, _)| *h == n).map(|(_, v)| v.expose().to_string()).unwrap_or_default();
        let bearer = |t: &str| header("authorization") == format!("Bearer {t}");
        let (status, answer) = match (method, url) {
            ("POST", "https://sts.amazonaws.com/") => {
                let a = header("authorization");
                if a.starts_with(&format!("AWS4-HMAC-SHA256 Credential={}/", c.aws_id)) && a.contains("SignedHeaders=content-type;host;x-amz-date, Signature=") && !header("x-amz-date").is_empty() {
                    (200, Value::String("<GetCallerIdentityResponse><GetCallerIdentityResult><Arn>arn:aws:iam::123456789012:user/ci</Arn><Account>123456789012</Account></GetCallerIdentityResult></GetCallerIdentityResponse>".into()))
                } else {
                    (403, Value::String("<ErrorResponse><Error><Code>InvalidClientTokenId</Code></Error></ErrorResponse>".into()))
                }
            }
            ("GET", "https://api.github.com/user") if bearer(&c.github_live) => (200, json!({ "login": "roll-fixture", "id": 1 })),
            ("GET", "https://api.github.com/user") if bearer(&c.github_moved) => (302, json!({ "location": "https://github.example/elsewhere" })),
            ("GET", "https://api.github.com/user") => (401, json!({ "message": "Bad credentials" })),
            ("GET", "https://api.stripe.com/v1/balance") if bearer(&c.stripe) => (200, json!({ "object": "balance" })),
            ("POST", "https://slack.com/api/auth.test") if bearer(&c.slack) => (200, json!({ "ok": true, "team_id": "T0ROLLFIX" })),
            ("POST", "https://slack.com/api/auth.test") => (200, json!({ "ok": false, "error": "invalid_auth" })),
            ("GET", "https://api.openai.com/v1/models") if bearer(&c.openai) => (200, json!({ "data": [] })),
            ("GET", "https://api.anthropic.com/v1/models") if header("x-api-key") == c.anthropic && header("anthropic-version") == "2023-06-01" => (200, json!({ "data": [] })),
            ("GET", "https://api.stripe.com/v1/balance" | "https://api.openai.com/v1/models" | "https://api.anthropic.com/v1/models") => (401, json!({ "error": "unauthorized" })),
            _ => return None,
        };
        self.0.lock().unwrap().push((url.to_string(), headers.iter().map(|(h, _)| h.to_string()).collect()));
        Some((status, answer))
    }
}

/// STS as an AWS identity's login meets it: who signed (`None`: the signature doesn't match), and
/// what was sent to it.
#[derive(Default)]
struct FakeStsLogin {
    caller: Option<String>,
    calls: Vec<StsSent>,
}

/// A request roll sent STS: the host, the headers and the body.
type StsSent = (String, Vec<(String, String)>, String);

/// A fake Snowflake user: its two key slots (base64 SPKI DER), password, default role, granted
/// roles and days to expiry.
#[derive(Clone, Default, Debug)]
struct SnowflakeUser {
    keys: [Option<String>; 2],
    password: Option<String>,
    default_role: Option<String>,
    roles: Vec<String>,
    days: Option<i64>,
}

/// Snowflake's SQL API for the account `myorg-acct`: the admin key it trusts (fingerprint, PKCS#1
/// DER), its users and roles, every statement it ran, one answer still running, and a flag that
/// refuses role grants.
#[derive(Default)]
struct FakeSnowflake {
    admin: Mutex<Option<(String, Vec<u8>)>>,
    users: Mutex<BTreeMap<String, SnowflakeUser>>,
    roles: Mutex<Vec<String>>,
    statements: Mutex<Vec<String>>,
    running: Mutex<Option<Value>>,
    fail_grants: std::sync::atomic::AtomicBool,
}

/// A public key as Snowflake keeps it (base64 SPKI DER), and its fingerprint.
fn snowflake_public(key: &rsa::RsaPrivateKey) -> (String, String) {
    use base64::Engine;
    use rsa::pkcs8::EncodePublicKey;
    use sha2::Digest;
    let der = key.to_public_key().to_public_key_der().unwrap();
    let b64 = base64::engine::general_purpose::STANDARD;
    (b64.encode(der.as_bytes()), format!("SHA256:{}", b64.encode(sha2::Sha256::digest(der.as_bytes()))))
}

impl FakeSnowflake {
    fn trust(&self, key: &rsa::RsaPrivateKey) {
        use rsa::pkcs1::EncodeRsaPublicKey;
        *self.admin.lock().unwrap() = Some((snowflake_public(key).1, key.to_public_key().to_pkcs1_der().unwrap().as_bytes().to_vec()));
    }

    /// The bearer is a key-pair JWT for ROLL_ADMIN, signed by the key it trusts.
    fn signed_in(&self, headers: &[(&str, &str)]) -> bool {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = |k: &str| headers.iter().find(|(h, _)| h.eq_ignore_ascii_case(k)).map(|(_, v)| *v);
        let Some((fp, pkcs1)) = self.admin.lock().unwrap().clone() else { return false };
        let Some(jwt) = header("authorization").and_then(|a| a.strip_prefix("Bearer ")) else { return false };
        let parts: Vec<&str> = jwt.split('.').collect();
        if header("x-snowflake-authorization-token-type") != Some("KEYPAIR_JWT") || parts.len() != 3 {
            return false;
        }
        let claims: Value = serde_json::from_slice(&b64.decode(parts[1]).unwrap_or_default()).unwrap_or_default();
        let sig = b64.decode(parts[2]).unwrap_or_default();
        let signed = ring::signature::UnparsedPublicKey::new(&ring::signature::RSA_PKCS1_2048_8192_SHA256, pkcs1).verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).is_ok();
        signed && claims["iss"] == format!("MYORG-ACCT.ROLL_ADMIN.{fp}") && claims["sub"] == "MYORG-ACCT.ROLL_ADMIN"
    }

    fn statement(&self, s: &str) -> Result<(Vec<&'static str>, Vec<Vec<String>>), String> {
        let re = |p: &str| regex::Regex::new(&format!("^{p}$")).unwrap();
        let mut users = self.users.lock().unwrap();
        let none = || "SQL compilation error: User does not exist or not authorized.".to_string();
        if let Some(c) = re(r#"DESCRIBE USER "(\w+)""#).captures(s) {
            let u = users.get(&c[1]).ok_or_else(none)?;
            let fp = |k: &Option<String>| k.clone().map_or("null".to_string(), |k| format!("SHA256:{}", k.len()));
            return Ok((vec!["property", "value"], vec![vec!["RSA_PUBLIC_KEY_FP".into(), fp(&u.keys[0])], vec!["RSA_PUBLIC_KEY_2_FP".into(), fp(&u.keys[1])]]));
        }
        if let Some(c) = re(r"SHOW ROLES LIKE '(\w+)'").captures(s) {
            return Ok((vec!["created_on", "name"], self.roles.lock().unwrap().iter().filter(|r| **r == c[1]).map(|r| vec!["2026-01-01".into(), r.clone()]).collect()));
        }
        if let Some(c) = re(r#"ALTER USER "(\w+)" SET (RSA_PUBLIC_KEY(?:_2)?) = '([A-Za-z0-9+/=]+)'"#).captures(s) {
            users.get_mut(&c[1]).ok_or_else(none)?.keys[usize::from(&c[2] != "RSA_PUBLIC_KEY")] = Some(c[3].to_string());
        } else if let Some(c) = re(r#"ALTER USER "(\w+)" UNSET (RSA_PUBLIC_KEY(?:_2)?)"#).captures(s) {
            users.get_mut(&c[1]).ok_or_else(none)?.keys[usize::from(&c[2] != "RSA_PUBLIC_KEY")] = None;
        } else if let Some(c) = re(r#"ALTER USER "(\w+)" SET PASSWORD = '([^'\\]+)' MUST_CHANGE_PASSWORD = FALSE"#).captures(s) {
            users.get_mut(&c[1]).ok_or_else(none)?.password = Some(c[2].to_string());
        } else if let Some(c) = re(r#"ALTER USER "(\w+)" SET DAYS_TO_EXPIRY = (\d+)"#).captures(s) {
            users.get_mut(&c[1]).ok_or_else(none)?.days = c[2].parse().ok();
        } else if let Some(c) = re(r#"CREATE USER "(ROLL_\w+)" TYPE = SERVICE RSA_PUBLIC_KEY = '([A-Za-z0-9+/=]+)' DEFAULT_ROLE = "(\w+)" DAYS_TO_EXPIRY = (\d+) COMMENT = '([^'\\]*)'"#).captures(s) {
            users.insert(c[1].to_string(), SnowflakeUser { keys: [Some(c[2].to_string()), None], default_role: Some(c[3].to_string()), days: c[4].parse().ok(), ..Default::default() });
        } else if let Some(c) = re(r#"GRANT ROLE "(\w+)" TO USER "(\w+)""#).captures(s) {
            if self.fail_grants.load(std::sync::atomic::Ordering::Relaxed) || !self.roles.lock().unwrap().iter().any(|r| *r == c[1]) {
                return Err("SQL access control error: Insufficient privileges to operate on role.".into());
            }
            users.get_mut(&c[2]).ok_or_else(none)?.roles.push(c[1].to_string());
        } else if let Some(c) = re(r#"DROP USER IF EXISTS "(\w+)""#).captures(s) {
            users.remove(&c[1]);
        } else {
            return Err(format!("SQL compilation error: syntax error in {s}"));
        }
        Ok((vec!["status"], vec![vec!["Statement executed successfully.".into()]]))
    }
}

/// SendGrid and Twilio: the key ids each has live, and how many were ever made.
#[derive(Default)]
struct Vendors {
    live: Vec<String>,
    made: u32,
}

/// A made-up account SID, written in pieces: secret scanners take the whole shape for a real one.
const TWILIO_SID: &str = concat!("AC", "0123456789abcdef", "0123456789abcdef");
const TWILIO_KEYS: &str = concat!("https://api.twilio.com/2010-04-01/Accounts/", "AC", "0123456789abcdef", "0123456789abcdef", "/Keys");

const ATLAS_GROUP: &str = "5f1d3c2b4a6e7f8091a2b3c4";
const ATLAS_CLIENT: &str = "mdb_sa_id_0123456789abcdef";

/// MongoDB Atlas's Admin API for one project, as its docs describe it: the service account's
/// secret, the access tokens it gave, its database users (as the API shows them, with the
/// password kept aside), every call (method and path) and every request body.
#[derive(Default)]
struct FakeAtlas {
    secret: Mutex<String>,
    tokens: Mutex<Vec<String>>,
    token_requests: std::sync::atomic::AtomicUsize,
    users: Mutex<BTreeMap<String, (Value, String)>>,
    calls: Mutex<Vec<String>>,
    bodies: Mutex<Vec<String>>,
}

impl FakeAtlas {
    fn password(&self, user: &str) -> Option<String> {
        self.users.lock().unwrap().get(user).map(|(_, pw)| pw.clone())
    }

    /// An ISO 8601 date in UTC (`2026-11-17T12:00:00Z`), as unix ms.
    fn date_ms(d: &Value) -> Option<i64> {
        let c = regex::Regex::new(r"^(\d{4})-(\d\d)-(\d\d)T(\d\d):(\d\d):(\d\d)Z$").unwrap().captures(d.as_str()?)?;
        let n = |i: usize| c[i].parse::<u16>().unwrap();
        let date = time::Date::from_calendar_date(i32::from(n(1)), time::Month::try_from(n(2) as u8).ok()?, n(3) as u8).ok()?;
        Some(date.with_hms(n(4) as u8, n(5) as u8, n(6) as u8).ok()?.assume_utc().unix_timestamp() * 1000)
    }

    /// A date Atlas takes for `deleteAfterDate`: in the future, within a week of the request.
    fn check_date(&self, d: &Value, now_ms: i64) -> bool {
        Self::date_ms(d).is_some_and(|ms| ms > now_ms && ms <= now_ms + 7 * 24 * 3_600_000)
    }

    fn answer(&self, method: &str, url: &str, headers: &[(&str, &str)], body: Option<(&str, &str)>) -> (u16, Value) {
        use base64::Engine;
        let header = |k: &str| headers.iter().find(|(h, _)| h.eq_ignore_ascii_case(k)).map(|(_, v)| *v);
        let path = url.strip_prefix("https://cloud.mongodb.com").unwrap();
        self.calls.lock().unwrap().push(format!("{method} {path}"));
        if path == "/api/oauth/token" {
            self.token_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{ATLAS_CLIENT}:{}", self.secret.lock().unwrap())));
            if method != "POST" || body != Some(("application/x-www-form-urlencoded", "grant_type=client_credentials")) || header("authorization") != Some(basic.as_str()) {
                return (401, json!({ "error": "invalid_client", "error_description": "Invalid client credentials" }));
            }
            let token = format!("eyJ.fake-{}", self.tokens.lock().unwrap().len());
            self.tokens.lock().unwrap().push(token.clone());
            return (200, json!({ "access_token": token, "expires_in": 3600, "token_type": "Bearer" }));
        }
        let bearer = header("authorization").and_then(|a| a.strip_prefix("Bearer ")).unwrap_or_default();
        if !self.tokens.lock().unwrap().iter().any(|t| t == bearer) {
            return (401, json!({ "error": 401, "errorCode": "NOT_AUTHORIZED", "reason": "Unauthorized" }));
        }
        let Some(rest) = path.strip_prefix(&format!("/api/atlas/v2/groups/{ATLAS_GROUP}")) else {
            return (404, json!({ "error": 404, "errorCode": "GROUP_NOT_FOUND", "detail": "No group with ID found." }));
        };
        // Each resource answers only its own versions (406 otherwise), and a body is that version.
        let version = match rest {
            r if r.starts_with("/clusters/") => "application/vnd.atlas.2024-08-05+json",
            r if r.starts_with("/flexClusters/") => "application/vnd.atlas.2024-11-13+json",
            _ => "application/vnd.atlas.2023-01-01+json",
        };
        if header("accept") != Some(version) || body.is_some_and(|(t, _)| t != version) {
            return (406, json!({ "error": 406, "errorCode": "INVALID_VERSION_DATE" }));
        }
        let body: Value = body.map(|(_, b)| serde_json::from_str(b).unwrap()).unwrap_or_default();
        if body.is_object() {
            self.bodies.lock().unwrap().push(body.to_string());
        }
        let now = code_sql::now_ms();
        let mut users = self.users.lock().unwrap();
        let shown = |u: &Value| {
            let mut u = u.clone();
            u["links"] = json!([{ "href": "https://cloud.mongodb.com/x", "rel": "self" }]);
            u
        };
        match (method, rest) {
            ("GET", "/databaseUsers?itemsPerPage=1") => (200, json!({ "results": users.values().take(1).map(|(u, _)| shown(u)).collect::<Vec<_>>(), "totalCount": users.len() })),
            ("GET", "/clusters/Cluster0") => (200, json!({ "name": "Cluster0", "connectionStrings": { "standard": "mongodb://x", "standardSrv": "mongodb+srv://cluster0.abcde.mongodb.net" } })),
            ("GET", "/flexClusters/Flex0") => (200, json!({ "name": "Flex0", "connectionStrings": { "standardSrv": "mongodb+srv://flex0.fghij.mongodb.net" } })),
            ("GET", r) if r.starts_with("/clusters/") => (404, json!({ "error": 404, "errorCode": "CLUSTER_NOT_FOUND" })),
            ("GET", r) if r.starts_with("/flexClusters/") => (404, json!({ "error": 404, "errorCode": "FLEX_CLUSTER_NOT_FOUND" })),
            ("POST", "/databaseUsers") => {
                let name = body["username"].as_str().unwrap_or_default().to_string();
                let roles_ok = body["roles"].as_array().is_some_and(|r| !r.is_empty() && r.iter().all(|r| r["roleName"].is_string() && r["databaseName"].is_string()));
                if body["groupId"] != ATLAS_GROUP || body["databaseName"] != "admin" || !roles_ok || !body["password"].is_string() || body.get("deleteAfterDate").is_some_and(|d| !self.check_date(d, now)) {
                    return (400, json!({ "error": 400, "errorCode": "INVALID_ATTRIBUTE" }));
                }
                if users.contains_key(&name) {
                    return (409, json!({ "error": 409, "errorCode": "USER_ALREADY_EXISTS" }));
                }
                let mut u = body.clone();
                let pw = u.as_object_mut().unwrap().remove("password").unwrap().as_str().unwrap().to_string();
                u.as_object_mut().unwrap().remove("groupId");
                users.insert(name, (u.clone(), pw));
                (201, shown(&u))
            }
            (m, r) => {
                let Some(name) = r.strip_prefix("/databaseUsers/admin/") else { return (404, json!({ "error": 404, "errorCode": "RESOURCE_NOT_FOUND" })) };
                let name = name.replace("%40", "@");
                let Some((user, pw)) = users.get(&name).cloned() else { return (404, json!({ "error": 404, "errorCode": "USERNAME_NOT_FOUND" })) };
                match m {
                    "GET" => (200, shown(&user)),
                    "DELETE" => {
                        users.remove(&name);
                        (204, Value::Null)
                    }
                    "PATCH" => {
                        // The whole user comes back as read, with what changed.
                        if body["groupId"] != ATLAS_GROUP || body["username"] != name.as_str() || body.get("links").is_some() || body["roles"] != user["roles"] || body.get("scopes") != user.get("scopes") || body.get("deleteAfterDate").is_some_and(|d| !self.check_date(d, now)) {
                            return (400, json!({ "error": 400, "errorCode": "INVALID_ATTRIBUTE" }));
                        }
                        let mut u = body.clone();
                        let pw = u.as_object_mut().unwrap().remove("password").and_then(|p| p.as_str().map(String::from)).unwrap_or(pw);
                        u.as_object_mut().unwrap().remove("groupId");
                        users.insert(name, (u.clone(), pw));
                        (200, shown(&u))
                    }
                    _ => (405, json!({ "error": 405 })),
                }
            }
        }
    }
}

impl roll_secrets::Deliver for Sent {
    /// A recorded GetCallerIdentityResponse for the caller set, or STS's refusal.
    fn sts(&self, host: &str, headers: &[(String, String)], body: &str, _: usize) -> roll_secrets::Result<(u16, Vec<u8>)> {
        let mut sts = self.8.lock().unwrap();
        sts.calls.push((host.to_string(), headers.to_vec(), body.to_string()));
        let Some(arn) = sts.caller.clone() else {
            return Ok((403, br#"<ErrorResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/"><Error><Type>Sender</Type><Code>SignatureDoesNotMatch</Code><Message>The request signature we calculated does not match the signature you provided.</Message></Error><RequestId>4c8f1fbc-0000-4000-8000-000000000000</RequestId></ErrorResponse>"#.to_vec()));
        };
        let account = arn.split(':').nth(4).unwrap();
        let answer = format!(r#"<GetCallerIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <GetCallerIdentityResult>
    <Arn>{arn}</Arn>
    <UserId>AROAEXAMPLEROLEID:session</UserId>
    <Account>{account}</Account>
  </GetCallerIdentityResult>
  <ResponseMetadata>
    <RequestId>01234567-89ab-cdef-0123-456789abcdef</RequestId>
  </ResponseMetadata>
</GetCallerIdentityResponse>"#);
        Ok((200, answer.into_bytes()))
    }
    fn check(&self, url: &str) -> roll_secrets::Result<()> {
        if url.starts_with("https://") { Ok(()) } else { Err(roll_secrets::Error::BadRequest("url is a public https:// URL".into())) }
    }
    fn post(&self, _: &str, _: &str, _: &Secret, body: &Value) -> roll_secrets::Result<()> {
        self.0.lock().unwrap().push(body.clone());
        Ok(())
    }
    /// A SIEM's format: what it was sent with (the token as rolld would put it in a header).
    fn post_format(&self, url: &str, id: &str, secret: &Secret, format: &str, token: Option<&Secret>, body: &Value) -> roll_secrets::Result<()> {
        if format == "roll" {
            return self.post(url, id, secret, body);
        }
        self.0.lock().unwrap().push(json!({ "format": format, "token": token.map(|t| t.expose().to_string()), "events": body["events"] }));
        Ok(())
    }
    /// A Kubernetes API server at `KUBE` taking only the admin token: discovery; creating and
    /// deleting service accounts, roles and bindings (kept as `{"kube_obj": {"create": path,
    /// "body"}}` and `{"kube_obj": {"delete": path}}`; RoleBindings in `flaky` are forbidden); and
    /// TokenRequest for `ci/deployer` or an account it holds (each request kept, with the CA it
    /// trusted, as `{"kube": …}`).
    fn request_trusting(&self, ca: Option<&str>, method: &str, url: &str, headers: &[(&str, &Secret)], body: Option<(&str, &Secret)>, _: usize) -> roll_secrets::Result<(u16, zeroize::Zeroizing<Vec<u8>>)> {
        let admin = headers.iter().any(|(h, v)| *h == "authorization" && v.expose() == "Bearer fake-admin-token");
        let path = url.strip_prefix(KUBE).unwrap_or_default();
        let collection = ["/serviceaccounts", "/roles", "/rolebindings", "/clusterroles", "/clusterrolebindings"].iter().any(|c| path.ends_with(c));
        let live = kube_objects(&self.0.lock().unwrap());
        let account = path.strip_prefix("/api/v1/namespaces/").and_then(|p| p.strip_suffix("/token")).and_then(|p| p.split_once("/serviceaccounts/"));
        let (status, answer) = match (method, path) {
            _ if !admin => (401, json!({ "kind": "Status", "reason": "Unauthorized" })),
            ("GET", "/api") => (200, json!({ "kind": "APIVersions", "versions": ["v1"] })),
            ("POST", _) if collection => {
                let sent: Value = serde_json::from_str(body.unwrap().1.expose()).unwrap();
                let at = format!("{path}/{}", sent["metadata"]["name"].as_str().unwrap());
                if path.ends_with("/namespaces/flaky/rolebindings") {
                    (403, json!({ "kind": "Status", "reason": "Forbidden" }))
                } else if live.contains(&at) {
                    (409, json!({ "kind": "Status", "reason": "AlreadyExists" }))
                } else {
                    self.0.lock().unwrap().push(json!({ "kube_obj": { "create": at, "body": sent } }));
                    (201, sent)
                }
            }
            ("DELETE", _) if live.contains(path) => {
                self.0.lock().unwrap().push(json!({ "kube_obj": { "delete": path } }));
                (200, json!({ "kind": "Status", "status": "Success" }))
            }
            ("POST", _) if account.is_some_and(|(ns, sa)| (ns, sa) == ("ci", "deployer") || live.contains(&format!("/api/v1/namespaces/{ns}/serviceaccounts/{sa}"))) => {
                let sent: Value = serde_json::from_str(body.unwrap().1.expose()).unwrap();
                let mut kept = self.0.lock().unwrap();
                kept.push(json!({ "kube": { "ca": ca, "request": sent } }));
                let n = kept.iter().filter(|b| b.get("kube").is_some()).count();
                let t = time::OffsetDateTime::now_utc() + time::Duration::seconds(sent["spec"]["expirationSeconds"].as_i64().unwrap());
                let expires = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", t.year(), t.month() as u8, t.day(), t.hour(), t.minute(), t.second());
                (201, json!({ "kind": "TokenRequest", "spec": sent["spec"], "status": { "token": format!("fake-sa-token-{n}"), "expirationTimestamp": expires } }))
            }
            _ => (404, json!({ "kind": "Status", "reason": "NotFound" })),
        };
        Ok((status, answer.to_string().into_bytes().into()))
    }
    fn get(&self, url: &str, _: usize) -> roll_secrets::Result<Vec<u8>> {
        let doc = match url.strip_prefix(ISSUER) {
            Some("/.well-known/openid-configuration") => json!({ "issuer": ISSUER, "jwks_uri": format!("{ISSUER}/keys") }),
            Some("/keys") => self.1.lock().unwrap().clone().unwrap_or(json!({ "keys": [] })),
            _ => return Err(roll_secrets::Error::Link(format!("{url}: 404"))),
        };
        Ok(doc.to_string().into_bytes())
    }
    /// A roll bucket's objects.
    fn put_object(&self, bucket: &str, key: &str, body: &Value) -> roll_secrets::Result<()> {
        self.4.lock().unwrap().insert((bucket.into(), key.into()), body.clone());
        Ok(())
    }
    fn get_object(&self, bucket: &str, key: &str, _: usize) -> roll_secrets::Result<Value> {
        self.4.lock().unwrap().get(&(bucket.into(), key.into())).cloned().ok_or_else(|| roll_secrets::Error::Link("404".into()))
    }
    fn list_objects(&self, bucket: &str, prefix: &str) -> roll_secrets::Result<Vec<String>> {
        Ok(self.4.lock().unwrap().keys().filter(|(b, k)| b == bucket && k.starts_with(prefix)).map(|(_, k)| k.clone()).collect())
    }
    fn delete_object(&self, bucket: &str, key: &str) -> roll_secrets::Result<()> {
        self.4.lock().unwrap().remove(&(bucket.into(), key.into()));
        Ok(())
    }
    /// A rotator outside roll: each issue is a new vendor key.
    fn call(&self, url: &str, _: &str, secret: &Secret, body: &Value, _: usize) -> roll_secrets::Result<zeroize::Zeroizing<Vec<u8>>> {
        *self.3.lock().unwrap() = Some((url.to_string(), secret.expose().to_string()));
        if self.2.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(roll_secrets::Error::Link("rotator.example: connection refused".into()));
        }
        let mut sent = self.0.lock().unwrap();
        sent.push(body.clone());
        let n = sent.iter().filter(|b| b["action"] == "issue").count();
        let answer = if body["action"] == "issue" { json!({ "value": format!("vendor-key-{n}"), "handle": n }) } else { json!({}) };
        Ok(answer.to_string().into_bytes().into())
    }
    /// SendGrid's and Twilio's key APIs, matched on the URL, each taking only its credential;
    /// Snowflake's SQL API ([`Sent::snowflake`]) and MongoDB Atlas's Admin API ([`FakeAtlas`]) apart.
    fn request(&self, method: &str, url: &str, headers: &[(&str, &Secret)], body: Option<(&str, &Secret)>, _: usize) -> roll_secrets::Result<(u16, zeroize::Zeroizing<Vec<u8>>)> {
        if let Some((status, v)) = self.10.answer(method, url, headers, body) {
            return Ok((status, if v.is_null() { Vec::new() } else { v.to_string().into_bytes() }.into()));
        }
        if let Some((status, answer)) = self.9.answer(method, url, headers) {
            let bytes = match answer {
                Value::String(xml) => xml.into_bytes(),
                v => v.to_string().into_bytes(),
            };
            return Ok((status, bytes.into()));
        }
        if url.starts_with("https://cloud.mongodb.com/") {
            let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.expose())).collect();
            let (status, v) = self.7.answer(method, url, &headers, body.map(|(t, b)| (t, b.expose())));
            return Ok((status, if v.is_null() { Vec::new() } else { v.to_string().into_bytes() }.into()));
        }
        if url.starts_with("https://myorg-acct.snowflakecomputing.com") {
            return self.snowflake(method, url, headers, body);
        }
        use base64::Engine;
        let auth = headers.iter().find(|(h, _)| *h == "authorization").map(|(_, v)| v.expose().to_string()).unwrap_or_default();
        let twilio_auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{TWILIO_SID}:twilio-token")));
        let mut v = self.5.lock().unwrap();
        let (status, answer) = match (method, url) {
            ("POST", "https://api.sendgrid.com/v3/api_keys") if auth == "Bearer SG.admin" => {
                let sent: Value = serde_json::from_str(body.unwrap().1.expose()).unwrap();
                assert_eq!(sent["scopes"], json!(["mail.send"]));
                v.made += 1;
                let id = format!("sgk{}", v.made);
                v.live.push(id.clone());
                (201, json!({ "api_key": format!("SG.key-{}", v.made), "api_key_id": id, "name": sent["name"] }))
            }
            ("POST", u) if u == format!("{TWILIO_KEYS}.json") && auth == twilio_auth => {
                assert!(body.unwrap().1.expose().starts_with("FriendlyName=roll%20secrets%20production%20TWILIO_API_"));
                v.made += 1;
                let sid = format!("SK{}", v.made);
                v.live.push(sid.clone());
                (201, json!({ "sid": sid, "secret": format!("twilio-secret-{}", v.made) }))
            }
            // Registering checks the credential: SendGrid's scopes, Twilio's key list.
            ("GET", "https://api.sendgrid.com/v3/scopes") if auth == "Bearer SG.admin" => (200, json!({ "scopes": ["api_keys.create", "api_keys.delete", "mail.send"] })),
            ("GET", "https://api.sendgrid.com/v3/scopes") if auth == "Bearer SG.mail-only" => (200, json!({ "scopes": ["mail.send"] })),
            ("GET", u) if u == format!("{TWILIO_KEYS}.json?PageSize=1") && auth == twilio_auth => (200, json!({ "keys": [] })),
            ("DELETE", u) if (u.starts_with("https://api.sendgrid.com/v3/api_keys/") && auth == "Bearer SG.admin") || (u.starts_with(TWILIO_KEYS) && auth == twilio_auth) => {
                let id = u.rsplit('/').next().unwrap().trim_end_matches(".json").to_string();
                let found = v.live.contains(&id);
                v.live.retain(|k| *k != id);
                (if found { 204 } else { 404 }, json!(null))
            }
            _ => (401, json!({ "errors": [{ "message": "authorization required" }] })),
        };
        Ok((status, answer.to_string().into_bytes().into()))
    }
}

impl Sent {
    /// Snowflake's SQL API: one statement a request, as rolld's guarded client would send it.
    fn snowflake(&self, method: &str, url: &str, headers: &[(&str, &roll_secrets::Value)], body: Option<(&str, &roll_secrets::Value)>) -> roll_secrets::Result<(u16, zeroize::Zeroizing<Vec<u8>>)> {
        let sf = &self.6;
        let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.expose())).collect();
        let headers = &headers[..];
        let body: Option<Value> = body.map(|(t, b)| {
            assert_eq!(t, "application/json");
            serde_json::from_str(b.expose()).unwrap()
        });
        let body = body.as_ref();
        let answer = |status: u16, v: Value| Ok((status, v.to_string().into_bytes().into()));
        let Some(path) = url.strip_prefix("https://myorg-acct.snowflakecomputing.com") else { return Err(roll_secrets::Error::Link(format!("{url}: connection refused"))) };
        if !sf.signed_in(headers) {
            return answer(401, json!({ "code": "390144", "message": "JWT token is invalid." }));
        }
        if method == "GET" {
            let done = sf.running.lock().unwrap().take().filter(|_| path == "/api/v2/statements/01-fake-handle");
            return done.map_or_else(|| answer(404, json!({ "code": "000709", "message": "Statement not found." })), |v| answer(200, v));
        }
        let body = body.unwrap();
        assert!(path == "/api/v2/statements" && body["role"] == "ROLL_ADMIN", "{path} {body}");
        let statement = body["statement"].as_str().unwrap().to_string();
        sf.statements.lock().unwrap().push(statement.clone());
        match sf.statement(&statement) {
            Ok((columns, rows)) => {
                let v = json!({ "code": "090001", "resultSetMetaData": { "rowType": columns.iter().map(|c| json!({ "name": c })).collect::<Vec<_>>() }, "data": rows });
                // A DESCRIBE is still running when first asked: its answer comes from the status URL.
                if statement.starts_with("DESCRIBE") {
                    *sf.running.lock().unwrap() = Some(v);
                    return answer(202, json!({ "code": "333334", "statementHandle": "01-fake-handle", "statementStatusUrl": "/api/v2/statements/01-fake-handle" }));
                }
                answer(200, v)
            }
            Err(message) => answer(422, json!({ "code": "002003", "sqlState": "02000", "message": message })),
        }
    }
}

const ISSUER: &str = "https://token.actions.githubusercontent.com";
const ISSUER_URL: &str = "https://roll.example/v1/secrets/oidc";
/// The objects the fake Kubernetes API holds: created and not deleted since, by path.
fn kube_objects(kept: &[Value]) -> std::collections::BTreeSet<String> {
    let mut live = std::collections::BTreeSet::new();
    for op in kept.iter().filter_map(|b| b.get("kube_obj")) {
        match (op["create"].as_str(), op["delete"].as_str()) {
            (Some(p), _) => live.insert(p.to_string()),
            (_, Some(p)) => live.remove(p),
            _ => false,
        };
    }
    live
}

const KUBE: &str = "https://kube.example";
const KUBE_CA: &str = "-----BEGIN CERTIFICATE-----\nfake-cluster-ca\n-----END CERTIFICATE-----";

/// GCP and Azure: they trust roll's token only for the pinned subject, and keep what they saw.
#[derive(Default)]
struct FakeClouds {
    pinned: Mutex<Option<String>>,
    seen: Mutex<Vec<String>>,
    /// Answer every exchange with an outage.
    down: std::sync::atomic::AtomicBool,
}

impl FakeClouds {
    fn trust(&self, jwt: &Secret, audience: &str) -> Result<u32, String> {
        use base64::Engine;
        let claims: Value = serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(jwt.expose().split('.').nth(1).unwrap()).unwrap()).unwrap();
        if self.down.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("503 Service Unavailable".into());
        }
        let mut seen = self.seen.lock().unwrap();
        seen.push(jwt.expose().to_string());
        if claims["aud"] != audience || self.pinned.lock().unwrap().as_deref().is_some_and(|p| claims["sub"] != p) {
            return Err("invalid_grant: the subject isn't trusted".into());
        }
        Ok(seen.len() as u32)
    }
}

impl roll_secrets::clouds::CloudTokens for FakeClouds {
    fn gcp(&self, audience: &str, _: &str, jwt: &Secret, lifetime: u32) -> Result<(Secret, i64), String> {
        let n = self.trust(jwt, audience)?;
        Ok((Secret::new(format!("ya29.{n}")), i64::from(lifetime) * 1000))
    }
    fn azure(&self, _: &str, _: &str, _: &str, jwt: &Secret) -> Result<(Secret, i64), String> {
        let n = self.trust(jwt, roll_secrets::clouds::AZURE_AUDIENCE)?;
        Ok((Secret::new(format!("eyJ0eXAi.{n}")), 3_600_000))
    }
}

/// A Postgres server outside roll: each role's current password and, for a dynamic login, its
/// group and `VALID UNTIL`; the flag refuses every change; the gateways calls went through.
#[derive(Default)]
struct Outside(std::sync::Mutex<BTreeMap<String, String>>, std::sync::atomic::AtomicBool, std::sync::Mutex<BTreeMap<String, (String, i64)>>, std::sync::atomic::AtomicBool, std::sync::atomic::AtomicBool, std::sync::Mutex<Vec<Option<String>>>);

impl Outside {
    /// The admin's password as the server has it.
    fn admin(&self) -> String {
        self.0.lock().unwrap().get("admin").cloned().unwrap_or_else(|| "hunter2".into())
    }
}

impl roll_secrets::outside::OutsideDb for Outside {
    fn check(&self, at: &roll_secrets::outside::Target<'_>, roles: &[String], _: Option<&str>) -> Result<(), String> {
        self.5.lock().unwrap().push(at.gateway.map(str::to_string));
        // With the flag set, the server can't be reached at all.
        if self.4.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("connection timed out".into());
        }
        if !at.url.contains(&format!("admin:{}@", self.admin())) {
            return Err("password authentication failed".into());
        }
        if at.url.contains("@db.example") && roles.iter().all(|r| r.starts_with("app_")) { Ok(()) } else { Err("there's no role that can log in".into()) }
    }
    fn set_password(&self, at: &roll_secrets::outside::Target<'_>, role: &str, password: &str) -> Result<(), String> {
        self.5.lock().unwrap().push(at.gateway.map(str::to_string));
        if self.1.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("connection refused".into());
        }
        self.0.lock().unwrap().insert(role.into(), password.into());
        Ok(())
    }
    fn create_login(&self, _: &roll_secrets::outside::Target<'_>, name: &str, password: &str, group: &str, until: i64) -> Result<(), String> {
        self.0.lock().unwrap().insert(name.into(), password.into());
        self.2.lock().unwrap().insert(name.into(), (group.into(), until));
        Ok(())
    }
    fn extend_login(&self, _: &roll_secrets::outside::Target<'_>, name: &str, until: i64) -> Result<(), String> {
        self.2.lock().unwrap().get_mut(name).ok_or("no such role")?.1 = until;
        Ok(())
    }
    fn drop_login(&self, _: &roll_secrets::outside::Target<'_>, name: &str) -> Result<(), String> {
        self.0.lock().unwrap().remove(name);
        self.2.lock().unwrap().remove(name);
        Ok(())
    }
    fn set_own_password(&self, at: &roll_secrets::outside::Target<'_>, password: &str) -> Result<(), String> {
        // The admin logs in with the password it has, which then changes; with the flag set, the
        // reply is lost after the change.
        if !at.url.contains(&format!(":{}@", self.admin())) {
            return Err("password authentication failed".into());
        }
        self.0.lock().unwrap().insert("admin".into(), password.into());
        if self.3.load(std::sync::atomic::Ordering::Relaxed) { Err("connection reset by peer".into()) } else { Ok(()) }
    }
}

/// GitHub for App 1's installation 7: the live tokens, each with the repositories and
/// permissions it was narrowed to.
#[derive(Default)]
struct FakeGitHub(std::sync::Mutex<BTreeMap<String, Narrowed>>);

/// A token's repositories and permissions.
type Narrowed = (Vec<String>, BTreeMap<String, String>);

impl roll_secrets::github_app::GitHubApps for FakeGitHub {
    fn token(&self, _: &str, app_id: u64, key: &roll_secrets::Value, installation: u64, repositories: &[String], permissions: &BTreeMap<String, String>) -> Result<(roll_secrets::Value, i64), String> {
        if app_id != 1 || installation != 7 || !key.expose().contains("BEGIN RSA PRIVATE KEY") {
            return Err("404 Not Found: Integration not found".into());
        }
        let mut live = self.0.lock().unwrap();
        let token = format!("ghs_{}", live.len() + 100);
        live.insert(token.clone(), (repositories.to_vec(), permissions.clone()));
        Ok((roll_secrets::Value::new(token), 4_102_444_800_000))
    }
    fn revoke(&self, _: &str, token: &roll_secrets::Value) -> Result<(), String> {
        self.0.lock().unwrap().remove(token.expose());
        Ok(())
    }
    fn check(&self, api_base: &str) -> Result<(), String> {
        if api_base.starts_with("https://") { Ok(()) } else { Err("api_base is an https:// URL".into()) }
    }
}

/// A fake directory's entry: its password, groups and description.
type Entry = (String, Vec<String>, String);

/// An LDAP directory outside roll: its entries (DN → password, groups, description), the binds it
/// proved, and a flag that fails a new user's group membership after making the user.
#[derive(Default)]
struct FakeDirectory {
    entries: Mutex<BTreeMap<String, Entry>>,
    binds: std::sync::atomic::AtomicUsize,
    fail_groups: std::sync::atomic::AtomicBool,
}

impl FakeDirectory {
    fn bound(&self, b: &roll_secrets::directories::Bind<'_>) -> Result<(), String> {
        if b.url != "ldaps://ldap.example:636" || b.bind_password != "fake-bind-pw" {
            return Err("invalid credentials (49)".into());
        }
        Ok(())
    }
    fn password(&self, dn: &str) -> Option<String> {
        self.entries.lock().unwrap().get(dn).map(|e| e.0.clone())
    }
}

impl roll_secrets::directories::Directory for FakeDirectory {
    fn check(&self, b: &roll_secrets::directories::Bind<'_>, accounts: &[String], groups: &[String], base: Option<&str>) -> Result<(), String> {
        self.binds.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.bound(b)?;
        let entries = self.entries.lock().unwrap();
        match accounts.iter().chain(groups).map(String::as_str).chain(base).find(|dn| !entries.contains_key(*dn)) {
            Some(dn) => Err(format!("no such object: {dn}")),
            None => Ok(()),
        }
    }
    fn set_password(&self, b: &roll_secrets::directories::Bind<'_>, dn: &str, password: &str) -> Result<(), String> {
        self.bound(b)?;
        self.entries.lock().unwrap().get_mut(dn).ok_or("no such object")?.0 = password.into();
        Ok(())
    }
    fn create_user(&self, b: &roll_secrets::directories::Bind<'_>, dn: &str, password: &str, groups: &[String], description: &str, _: i64) -> Result<(), String> {
        self.bound(b)?;
        self.entries.lock().unwrap().insert(dn.into(), (password.into(), vec![], description.into()));
        if self.fail_groups.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("insufficient access to the group (50)".into());
        }
        self.entries.lock().unwrap().get_mut(dn).unwrap().1 = groups.to_vec();
        Ok(())
    }
    fn extend_user(&self, _: &roll_secrets::directories::Bind<'_>, _: &str, _: i64) -> Result<(), String> {
        Ok(())
    }
    fn delete_user(&self, b: &roll_secrets::directories::Bind<'_>, dn: &str, _: &[String]) -> Result<(), String> {
        self.bound(b)?;
        self.entries.lock().unwrap().remove(dn);
        Ok(())
    }
    fn account_names(&self, b: &roll_secrets::directories::Bind<'_>, dns: &[String]) -> Result<Vec<roll_secrets::directories::AccountNames>, String> {
        self.bound(b)?;
        Ok(dns.iter().map(|dn| roll_secrets::directories::AccountNames { account_name: dn[3..dn.find(',').unwrap()].into(), upn: format!("{}@corp.example", &dn[3..dn.find(',').unwrap()]) }).collect())
    }
}

/// An ES256 key of the fake issuer, its JWKS, and tokens it signs.
struct Issuer(ring::signature::EcdsaKeyPair);

impl Issuer {
    fn new() -> Issuer {
        use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        Issuer(EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap())
    }
    fn jwks(&self) -> Value {
        use ring::signature::KeyPair;
        let p = self.0.public_key().as_ref();
        json!({ "keys": [{ "kty": "EC", "crv": "P-256", "kid": "k1", "use": "sig", "x": b64(&p[1..33]), "y": b64(&p[33..65]) }] })
    }
    fn sign(&self, claims: Value) -> String {
        let signed = format!("{}.{}", b64(json!({ "alg": "ES256", "kid": "k1", "typ": "JWT" }).to_string().as_bytes()), b64(claims.to_string().as_bytes()));
        let sig = self.0.sign(&ring::rand::SystemRandom::new(), signed.as_bytes()).unwrap();
        format!("{signed}.{}", b64(sig.as_ref()))
    }
}

fn b64(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// People's accounts: `ada` until disabled, and `root`, an admin until demoted or disabled.
#[derive(Default)]
struct People {
    disabled: std::sync::Mutex<bool>,
    root_demoted: std::sync::Mutex<bool>,
    root_disabled: std::sync::Mutex<bool>,
}

impl roll_secrets::People for People {
    fn person(&self, user: &str) -> Result<Option<roll_secrets::Person>, String> {
        Ok(match user {
            "usr_ada" if !*self.disabled.lock().unwrap() => Some(roll_secrets::Person { admin: false, code_owner: Some("ada".into()) }),
            // Members of ada's projects (docs/members.md).
            "usr_bob" | "usr_vic" | "usr_cat" => Some(roll_secrets::Person { admin: false, code_owner: None }),
            "usr_root" if !*self.root_disabled.lock().unwrap() => Some(roll_secrets::Person { admin: !*self.root_demoted.lock().unwrap(), code_owner: None }),
            _ => None,
        })
    }

    fn admins(&self) -> Result<Option<usize>, String> {
        Ok(Some(usize::from(!*self.root_disabled.lock().unwrap() && !*self.root_demoted.lock().unwrap())))
    }
}

/// People's addresses, as roll accounts answers the workspace: `<handle>@example.com`.
impl roll_workspace::People for People {
    fn person(&self, user: &str) -> Option<roll_workspace::Person> {
        let p = roll_secrets::People::person(self, user).ok().flatten()?;
        let handle = user.strip_prefix("usr_")?;
        Some(roll_workspace::Person { handle: handle.into(), email: Some(format!("{handle}@example.com")), admin: p.admin })
    }
}

/// As rolld answers: an admin's canvas stands for anything, a person's for their own repos.
impl roll_workspace::Owners for People {
    fn trusts(&self, user: &str, kind: roll_workspace::Kind, r#ref: &str) -> bool {
        match roll_secrets::People::person(self, user) {
            Ok(Some(p)) => p.admin || (kind == roll_workspace::Kind::Repo && r#ref.starts_with(&format!("{}/", p.code_owner.unwrap_or_default()))),
            _ => false,
        }
    }
    fn owns(&self, user: &str, kind: roll_workspace::Kind, r#ref: &str) -> bool {
        match roll_secrets::People::person(self, user) {
            Ok(Some(p)) => kind == roll_workspace::Kind::Repo && r#ref.starts_with(&format!("{}/", p.code_owner.unwrap_or_default())),
            _ => false,
        }
    }
}

/// A customer's KMS: its key policy pins one project (or, unpinned, takes any), and it stops
/// decrypting once revoked.
#[derive(Default)]
struct FakeKms {
    revoked: Mutex<bool>,
    pinned: Mutex<Option<String>>,
}

impl roll_secrets::kms::ProjectKms for FakeKms {
    fn encrypt(&self, key: &str, project: &str, plain: &[u8]) -> Result<Vec<u8>, String> {
        if self.pinned.lock().unwrap().as_deref().is_some_and(|p| p != project) {
            return Err("AccessDeniedException".into());
        }
        Ok([format!("{key}|{project}|").as_bytes(), plain].concat())
    }
    fn decrypt(&self, key: &str, project: &str, ct: &[u8]) -> Result<zeroize::Zeroizing<Vec<u8>>, String> {
        if *self.revoked.lock().unwrap() {
            return Err("AccessDeniedException".into());
        }
        let prefix = format!("{key}|{project}|");
        ct.strip_prefix(prefix.as_bytes()).map(|p| zeroize::Zeroizing::new(p.to_vec())).ok_or_else(|| "InvalidCiphertextException".into())
}
}

/// STS for one role whose trust policy pins one ExternalId (or, unpinned, takes any).
#[derive(Default)]
struct FakeSts {
    pinned: Mutex<Option<String>>,
    /// The `sub` a role trusting roll's issuer pins (or, unpinned, any of roll's projects).
    web_pinned: Mutex<Option<String>>,
    /// STS throttling every call (a failure to ask, not a refusal).
    throttled: Mutex<bool>,
    sessions: Mutex<u32>,
    /// IAM user `legacy-ci`'s access key ids, and how many were ever made.
    keys: Mutex<(Vec<String>, u32)>,
}

impl roll_secrets::aws::Sts for FakeSts {
    fn assume(&self, role_arn: &str, external_id: &str, _session: &str, seconds: i32) -> Result<roll_secrets::aws::Session, String> {
        if *self.throttled.lock().unwrap() {
            return Err("Throttling".into());
        }
        if role_arn != "arn:aws:iam::123456789012:role/roll" || self.pinned.lock().unwrap().as_deref().is_some_and(|p| p != external_id) {
            return Err("AccessDenied".into());
        }
        let mut n = self.sessions.lock().unwrap();
        *n += 1;
        Ok(roll_secrets::aws::Session {
            access_key_id: Secret::new(format!("ASIA{n}")),
            secret_access_key: Secret::new(format!("secret-{n}")),
            session_token: Secret::new(format!("token-{n}")),
            expires_at: i64::from(seconds) * 1000,
        })
    }
    /// As AWS checks a web identity: the token's audience, and the subject the trust pins.
    fn assume_web_identity(&self, role_arn: &str, token: &str, _session: &str, seconds: i32) -> Result<roll_secrets::aws::Session, String> {
        use base64::Engine;
        let payload = token.split('.').nth(1).ok_or("InvalidIdentityToken")?;
        let claims: Value = serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).map_err(|_| "InvalidIdentityToken")?).map_err(|_| "InvalidIdentityToken")?;
        let sub_ok = self.web_pinned.lock().unwrap().as_deref().is_none_or(|p| claims["sub"] == p);
        if role_arn != "arn:aws:iam::123456789012:role/roll-oidc" || claims["aud"] != "sts.amazonaws.com" || claims["iss"] != ISSUER_URL || !sub_ok {
            return Err("AccessDenied".into());
        }
        let mut n = self.sessions.lock().unwrap();
        *n += 1;
        Ok(roll_secrets::aws::Session { access_key_id: Secret::new(format!("ASIA{n}")), secret_access_key: Secret::new(format!("secret-{n}")), session_token: Secret::new(format!("token-{n}")), expires_at: i64::from(seconds) * 1000 })
    }
    fn iam_keys(&self, role_arn: &str, external_id: &str, user: &str) -> Result<Vec<String>, String> {
        self.assume(role_arn, external_id, "iam", 900)?;
        if user != "legacy-ci" {
            return Err("NoSuchEntity".into());
        }
        Ok(self.keys.lock().unwrap().0.clone())
    }
    fn iam_create_key(&self, role_arn: &str, external_id: &str, user: &str) -> Result<(Secret, Secret), String> {
        if self.iam_keys(role_arn, external_id, user)?.len() >= 2 {
            return Err("LimitExceeded".into());
        }
        let mut k = self.keys.lock().unwrap();
        k.1 += 1;
        let id = format!("AKIA{}", k.1);
        k.0.push(id.clone());
        Ok((Secret::new(id), Secret::new(format!("iam-secret-{}", k.1))))
    }
    fn iam_delete_key(&self, role_arn: &str, external_id: &str, user: &str, key_id: &str) -> Result<(), String> {
        self.iam_keys(role_arn, external_id, user)?;
        self.keys.lock().unwrap().0.retain(|k| k != key_id);
        Ok(())
    }
}

/// rolld's gate as far as secrets sees it: a roll-access token (`rlt_…`) becomes its `Grant` on the
/// request; one it doesn't know is a 401. Workload identities' logins mint here too.
#[derive(Default)]
struct Gate {
    tokens: Mutex<BTreeMap<String, roll_auth::Grant>>,
    /// Grants made so far: ids stay unique after revocations.
    made: std::sync::atomic::AtomicUsize,
    /// Decoy grants' ids (roll-access's decoys): they allow nothing, and a use is told to secrets.
    decoys: Mutex<std::collections::BTreeSet<String>>,
    /// Where a decoy's use is told, as rolld's `on_decoy` hook does.
    secrets: std::sync::OnceLock<Arc<Secrets>>,
    /// Workload logins' grants: roll-access's token list leaves them out.
    workloads: Mutex<std::collections::BTreeSet<String>>,
    /// Every grant's person, revoked ones too (roll-access keeps revoked rows).
    owners: Mutex<BTreeMap<String, Option<String>>>,
}

impl Gate {
    /// A checksummed token's secret, as roll-access makes them (a decoy, with `decoy`): what
    /// scanners find.
    fn checksummed(&self, decoy: bool) -> String {
        let (short, id) = self.grant("found", roll_secrets::access::rule("p", None, None, &[roll_secrets::Action::Read]), None);
        let secret = roll_auth::credentials::new_secret("rlt_");
        let mut tokens = self.tokens.lock().unwrap();
        let g = tokens.remove(&short).expect("just made");
        tokens.insert(secret.clone(), g);
        if decoy {
            self.decoys.lock().unwrap().insert(id);
        }
        secret
    }

    /// A grant's secret and id.
    fn grant(&self, name: &str, rule: roll_auth::scope::Rule, user: Option<&str>) -> (String, String) {
        let mut tokens = self.tokens.lock().unwrap();
        let n = self.made.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (secret, id) = (format!("rlt_test{n}"), format!("tok_{n}"));
        let grant = roll_auth::Grant { ask_otherwise: false, user: user.map(Into::into), id: id.clone().into(), name: name.into(), kind: roll_auth::GrantKind::Token, scope: Arc::new(roll_auth::scope::Scope(vec![rule])) };
        tokens.insert(secret.clone(), grant);
        self.owners.lock().unwrap().insert(id.clone(), user.map(Into::into));
        (secret, id)
    }

    fn issue(&self, name: &str, rule: roll_auth::scope::Rule, user: Option<&str>) -> String {
        self.grant(name, rule, user).0
    }

    /// `user`'s OAuth connection (`rlo_…`: an MCP client they authorized) with `rule`; its id.
    fn oauth(&self, name: &str, rule: roll_auth::scope::Rule, user: &str) -> String {
        let (secret, id) = self.grant(name, rule, Some(user));
        self.tokens.lock().unwrap().get_mut(&secret).expect("just made").kind = roll_auth::GrantKind::Oauth;
        id
    }
}

/// roll-access's grants as #25 answers them: a person's own token is no grant once they can't
/// sign in.
struct Granted {
    gate: Arc<Gate>,
    people: Arc<People>,
    ws: Arc<roll_workspace::Workspace>,
}

/// A person's session grant as roll-access builds it: their memberships' roles, now.
fn session_of(ws: &roll_workspace::Workspace, user: &str) -> roll_auth::Grant {
    let scope = roll_auth::roles::session_scope(&ws.memberships_of(user).unwrap());
    roll_auth::Grant { id: format!("session:{user}").into(), name: "session".into(), kind: roll_auth::GrantKind::Session, scope: Arc::new(scope), user: Some(user.into()), ask_otherwise: false }
}

impl roll_auth::Grants for Granted {
    fn grant(&self, id: &str) -> Option<roll_auth::Grant> {
        if self.gate.decoys.lock().unwrap().contains(id) {
            return None;
        }
        let g = self.gate.tokens.lock().unwrap().values().find(|g| &*g.id == id).cloned()?;
        match &g.user {
            Some(user) if roll_secrets::People::person(&*self.people, user).ok().flatten().is_none() => None,
            _ => Some(g),
        }
    }

    fn owner(&self, id: &str) -> Option<Option<String>> {
        self.gate.owners.lock().unwrap().get(id).cloned()
    }

    fn session(&self, user: &str) -> Option<roll_auth::Grant> {
        roll_secrets::People::person(&*self.people, user).ok().flatten()?;
        Some(session_of(&self.ws, user))
    }
}

impl roll_secrets::Workloads for Gate {
    fn mint(&self, name: &str, rule: roll_auth::scope::Rule, _: Option<Vec<String>>, minutes: u32, owner: Option<&str>) -> roll_secrets::Result<roll_secrets::Minted> {
        let (secret, id) = self.grant(name, rule, owner);
        self.workloads.lock().unwrap().insert(id.clone());
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
        Ok(roll_secrets::Minted { id, secret, expires_at: now + minutes as i64 * 60_000 })
    }
    fn grants(&self) -> roll_secrets::Result<Vec<roll_auth::Grant>> {
        let decoys = self.decoys.lock().unwrap().clone();
        let workloads = self.workloads.lock().unwrap();
        Ok(self.tokens.lock().unwrap().values().filter(|g| !decoys.contains(&*g.id) && !workloads.contains(&*g.id)).cloned().collect())
    }
    fn revoke(&self, id: &str) -> roll_secrets::Result<()> {
        self.tokens.lock().unwrap().retain(|_, g| &*g.id != id);
        Ok(())
    }
    /// All or none, as roll-access's: an id it doesn't know refuses the lot.
    fn revoke_many(&self, ids: &[String], _: &str, _: &str) -> roll_secrets::Result<()> {
        let mut tokens = self.tokens.lock().unwrap();
        if let Some(id) = ids.iter().find(|id| !tokens.values().any(|g| &*g.id == id.as_str())) {
            return Err(roll_secrets::Error::NotFound(format!("no token {id}: nothing was revoked")));
        }
        tokens.retain(|_, g| !ids.iter().any(|id| *id == *g.id));
        Ok(())
    }
    fn mint_decoy(&self, name: &str, rule: roll_auth::scope::Rule, owner: Option<&str>) -> roll_secrets::Result<roll_secrets::Minted> {
        let (short, id) = self.grant(name, rule, owner);
        // As long and random as a real token's, so nothing about the value sets it apart.
        let secret = format!("rlt_fakedecoy{}{short}", "Q7wZ3kP9xR2mV8tL5nB4cY6h");
        let mut tokens = self.tokens.lock().unwrap();
        let g = tokens.remove(&short).expect("just made");
        tokens.insert(secret.clone(), g);
        self.decoys.lock().unwrap().insert(id.clone());
        Ok(roll_secrets::Minted { id, secret, expires_at: 0 })
    }
    fn revoke_decoy(&self, id: &str) -> roll_secrets::Result<()> {
        self.revoke(id)
    }
    /// As `POST /v1/access/tokens`: its owner is whoever's request it is. As roll-access's
    /// `create_grant`, extensions that name no one (no operator marker, person or grant) are
    /// refused, even on an open server.
    fn token(&self, ext: &axum::http::Extensions, name: &str, rule: roll_auth::scope::Rule) -> roll_secrets::Result<roll_secrets::Minted> {
        if !roll_auth::Operator::on(ext) && ext.get::<roll_auth::SignedIn>().is_none() && roll_auth::Grant::on(ext).is_none() {
            return Err(roll_secrets::Error::Forbidden("these extensions name no one".into()));
        }
        let (secret, id) = self.grant(name, rule, ext.get::<roll_auth::SignedIn>().map(|u| u.user.as_str()));
        Ok(roll_secrets::Minted { id, secret, expires_at: 0 })
    }
    fn revoke_token(&self, id: &str, _: &str) -> roll_secrets::Result<()> {
        self.revoke(id)
    }
}

async fn fake_gate(axum::extract::State(gate): axum::extract::State<Arc<Gate>>, mut req: Request<Body>, next: axum::middleware::Next) -> axum::response::Response {
    use axum::response::IntoResponse;
    if let Some(t) = roll_auth::bearer(req.headers()).map(str::to_string) {
        let Some(g) = gate.tokens.lock().unwrap().get(&t).cloned() else { return StatusCode::UNAUTHORIZED.into_response() };
        // A decoy answers as an unknown token does, and roll secrets hears of it.
        if gate.decoys.lock().unwrap().contains(&*g.id) {
            if let Some(s) = gate.secrets.get().cloned() {
                let id = g.id.to_string();
                tokio::task::spawn_blocking(move || s.decoy_used("rlt", &id, Some("203.0.113.7"), "/v1/secrets/whoami").unwrap()).await.unwrap();
            }
            return StatusCode::UNAUTHORIZED.into_response();
        }
        req.extensions_mut().insert(g);
    }
    next.run(req).await
}

struct Api {
    sent: Arc<Sent>,
    gate: Arc<Gate>,
    kms: Arc<FakeKms>,
    ws: Arc<roll_workspace::Workspace>,
    /// The workspace's own database: a project changing hands, which no route does yet.
    ws_db: code_sql::Db,
    sts: Arc<FakeSts>,
    people: Arc<People>,
    router: Router,
    secrets: Arc<Secrets>,
    db: code_sql::Db,
    fake: Arc<Fake>,
    pg: Arc<FakePg>,
    sync: Arc<FakeSync>,
    certs: Arc<FakeCertSync>,
    clouds: Arc<FakeClouds>,
    outside: Arc<Outside>,
    directory: Arc<FakeDirectory>,
    github: Arc<FakeGitHub>,
    project: String,
    _dir: tempfile::TempDir,
}

impl Api {
    async fn new() -> Api {
        let dir = tempfile::tempdir().unwrap();
        let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
        let ws = Arc::new(roll_workspace::Workspace::new(ws_db.clone()).unwrap());
        let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("secrets.db")), "secrets").unwrap();
        let fake = Arc::new(Fake::default());
        let mut links = Links::default();
        links.register(fake.clone());
        let pg = Arc::new(FakePg::default());
        links.register(pg.clone());
        let sync = Arc::new(FakeSync::default());
        let mut syncs = Syncs::default();
        syncs.register(sync.clone());
        let certs = Arc::new(FakeCertSync::default());
        syncs.register(certs.clone());
        let sent = Arc::new(Sent::default());
        let people = Arc::new(People::default());
        let kms = Arc::new(FakeKms::default());
        let outside = Arc::new(Outside::default());
        let directory = Arc::new(FakeDirectory::default());
        let github = Arc::new(FakeGitHub::default());
        let sts = Arc::new(FakeSts::default());
        ws.set_owners(people.clone());
        ws.set_people(people.clone());
        let clouds = Arc::new(FakeClouds::default());
        let gate = Arc::new(Gate::default());
        let granted: Arc<dyn roll_auth::Grants> = Arc::new(Granted { gate: gate.clone(), people: people.clone(), ws: ws.clone() });
        let opts = Options { keyring: Ok(Keyring::new(&"k".repeat(32), None).unwrap()), key_source: roll_secrets::KeySource::Env, links, syncs, open: true, trust_forwarded_for: false, web_url: None, deliver: Some(sent.clone()), grants: Some(granted), people: Some(people.clone()), workloads: Some(gate.clone()), kms: Some(kms.clone()), sts: Some(sts.clone()), outside: Some(outside.clone()), directory: Some(directory.clone()), github: Some(github.clone()), clouds: Some(roll_secrets::clouds::Clouds { issuer: ISSUER_URL.into(), tokens: clouds.clone() }), gateways: None };
        let secrets = Arc::new(Secrets::new(db.clone(), ws.clone(), opts).unwrap());
        secrets.watch_workspace();
        let _ = gate.secrets.set(secrets.clone());
        // As rolld: a member removed, demoted or gone (or narrowed by a group) ends what they
        // shared and handed out.
        let s2 = secrets.clone();
        ws.on_event(Box::new(move |e| match (e.action.as_str(), &e.subject) {
            ("member.remove" | "member.leave" | "member.role", Some(user)) => s2.member_changed(&e.project, user).map_err(|e| e.to_string()),
            ("group.lost", Some(user)) => e.lost_projects().iter().try_for_each(|p| s2.member_changed(p, user)).map_err(|e| e.to_string()),
            _ => Ok(()),
        }));
        let router = roll_workspace::router(ws.clone()).merge(roll_secrets::router(secrets.clone())).layer(axum::middleware::from_fn_with_state(gate.clone(), fake_gate));
        let mut api = Api { sent, gate, kms, sts, ws: ws.clone(), ws_db, people, router, secrets, db, fake, pg, sync, certs, outside, directory, github, clouds, project: String::new(), _dir: dir };
        let (_, p) = api.call(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "shop" }))).await;
        api.project = p["id"].as_str().unwrap().to_string();
        api
    }

    async fn req(&self, method: Method, path: &str, body: Option<Value>, token: Option<&str>) -> (StatusCode, Value) {
        let path = if path.starts_with("/v1/") { path.to_string() } else { format!("/v1/secrets/projects/{}{path}", self.project) };
        let mut req = Request::builder().method(method).uri(path).header("content-type", "application/json");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let req = req.body(body.map_or(Body::empty(), |b| Body::from(b.to_string()))).unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into())))
    }

    /// A roll-access token for this project with `scope`'s `environments`, `paths` and `actions`, as
    /// `POST /v1/access/tokens` makes from secrets' page.
    fn token(&self, scope: Value) -> String {
        let list = |k: &str| scope[k].as_array().map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect::<Vec<_>>());
        let actions: Vec<roll_secrets::Action> = serde_json::from_value(scope["actions"].clone()).unwrap();
        let name = scope["name"].as_str().unwrap_or("test");
        self.gate.issue(name, roll_secrets::access::rule(&self.project, list("environments"), list("paths"), &actions), None)
    }

    /// As `usr_ada`, signed in and not an admin (the gate put her session on the request).
    async fn as_ada(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.signed_in("ada", false, method, path, body).await
    }

    async fn signed_in(&self, handle: &str, admin: bool, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path).header("content-type", "application/json").body(body.map_or(Body::empty(), |b| Body::from(b.to_string()))).unwrap();
        req.extensions_mut().insert(roll_auth::SignedIn { user: format!("usr_{handle}"), handle: handle.into(), admin, code_owner: None, session: format!("ses_{handle}") });
        if admin {
            req.extensions_mut().insert(roll_auth::Operator);
        } else {
            // What rolld's gate adds for a person: their memberships as their session's grant.
            req.extensions_mut().insert(session_of(&self.ws, &format!("usr_{handle}")));
        }
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.req(method, path, body, None).await
    }

    async fn ok(&self, method: Method, path: &str, body: Option<Value>) -> Value {
        let (s, v) = self.call(method.clone(), path, body).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }

    async fn set(&self, env: &str, key: &str, value: &str) -> Value {
        self.ok(Method::PUT, &format!("/environments/{env}/secrets/{key}"), Some(json!({ "value": value }))).await
    }

    async fn values(&self, env: &str, path: &str) -> Value {
        self.ok(Method::GET, &format!("/environments/{env}/values?path={path}"), None).await
    }
}

fn slugs(v: &Value) -> Vec<&str> {
    v.as_array().unwrap().iter().map(|e| e["slug"].as_str().unwrap()).collect()
}

#[tokio::test]
async fn environments_start_as_three_and_can_be_added_renamed_reordered_and_deleted() {
    let api = Api::new().await;
    let envs = api.ok(Method::GET, "/environments", None).await;
    assert_eq!(slugs(&envs), ["development", "staging", "production"]);
    api.ok(Method::POST, "/environments", Some(json!({ "slug": "qa" }))).await;
    let (s, e) = api.call(Method::POST, "/environments", Some(json!({ "slug": "qa" }))).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("conflict")));
    // A reference to staging follows the rename.
    api.set("staging", "HOST", "db.internal").await;
    api.set("development", "UPSTREAM", "https://${staging.HOST}/api").await;
    let e = api.ok(Method::PATCH, "/environments/staging", Some(json!({ "slug": "preprod", "name": "Pre-production" }))).await;
    assert_eq!((e["slug"].as_str(), e["name"].as_str()), (Some("preprod"), Some("Pre-production")));
    assert_eq!(api.values("development", "/").await["values"]["UPSTREAM"], "https://db.internal/api");
    let s = api.ok(Method::GET, "/environments/development/secrets/UPSTREAM", None).await;
    assert_eq!(s["references"], json!(["preprod.HOST"]));

    let envs = api.ok(Method::PUT, "/environments/order", Some(json!({ "order": ["qa", "development", "preprod", "production"] }))).await;
    assert_eq!(slugs(&envs), ["qa", "development", "preprod", "production"]);
    let (s, _) = api.call(Method::DELETE, "/environments/qa", None).await;
    assert_eq!(s, StatusCode::OK);
    // Production starts protected: deleting it names it.
    let (s, e) = api.call(Method::DELETE, "/environments/production", None).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("confirm_required")));
    for slug in ["development", "preprod"] {
        api.call(Method::DELETE, &format!("/environments/{slug}"), None).await;
    }
    let (s, e) = api.call(Method::DELETE, "/environments/production?confirm=production", None).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("last_environment")));
}

#[tokio::test]
async fn every_write_is_a_version_and_an_environment_rolls_back_to_any_change() {
    let api = Api::new().await;
    api.set("production", "A", "1").await;
    let first = api.ok(Method::POST, "/environments/production/changes", Some(json!({ "message": "launch", "ops": [{ "op": "set", "key": "B", "value": "2", "comment": "the b" }, { "op": "set", "key": "A", "value": "10" }] }))).await;
    let seq = first["change"]["seq"].as_i64().unwrap();
    api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": [{ "op": "delete", "key": "A" }, { "op": "set", "key": "C", "value": "3" }] }))).await;
    assert_eq!(api.values("production", "/").await["values"], json!({ "B": "2", "C": "3" }));

    let versions = api.ok(Method::GET, "/environments/production/secrets/A/versions", None).await;
    let ops: Vec<(i64, &str)> = versions.as_array().unwrap().iter().map(|v| (v["version"].as_i64().unwrap(), v["op"].as_str().unwrap())).collect();
    assert_eq!(ops, [(3, "delete"), (2, "set"), (1, "set")]);
    let old = api.ok(Method::GET, "/environments/production/secrets/A/versions/1/value", None).await;
    assert_eq!(old["value"], "1");
    let detail = api.ok(Method::GET, &format!("/environments/production/changes/{seq}"), None).await;
    assert_eq!(detail["message"], "launch");
    assert!(!detail.to_string().contains("\"10\""), "a change shows fingerprints, never values: {detail}");

    // What production had then, without rolling back.
    let then = api.ok(Method::GET, &format!("/environments/production/values?seq={seq}"), None).await;
    assert_eq!((then["values"].clone(), then["seq"].as_i64()), (json!({ "A": "10", "B": "2" }), Some(seq)));
    let r = api.ok(Method::POST, "/environments/production/rollback", Some(json!({ "to": seq }))).await;
    assert_eq!(r["change"]["kind"], "rollback");
    assert_eq!(api.values("production", "/").await["values"], json!({ "A": "10", "B": "2" }));
    // A local copy against it, by fingerprint: names only, never a value.
    let d = api.ok(Method::POST, "/environments/production/diff", Some(json!({ "dotenv": "A=10\nB=stale\nNEW=x\n" }))).await;
    assert_eq!((d["same"].clone(), d["changed"].clone(), d["added"].clone(), d["missing"].clone()), (json!(["A"]), json!(["B"]), json!(["NEW"]), json!([])));
    assert!(!d.to_string().contains("stale"), "{d}");
        // Deleted and restored, a key keeps one history.
    let versions = api.ok(Method::GET, "/environments/production/secrets/A/versions", None).await;
    assert_eq!(versions[0]["version"], 4);
    let changes = api.ok(Method::GET, "/environments/production/changes", None).await;
    assert_eq!(changes.as_array().unwrap().len(), 4);
    // After a leak: older values are purged for good; history keeps the rows, a rollback says so.
    let p = api.ok(Method::POST, "/environments/production/secrets/A/purge", None).await;
    assert_eq!(p["purged"], 2);
    let (s, _) = api.call(Method::GET, "/environments/production/secrets/A/versions/1/value", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let r = api.ok(Method::POST, "/environments/production/rollback", Some(json!({ "to": 1 }))).await;
    assert_eq!(r["warnings"], json!(["A's value at #1 was purged: it stays as it is now"]));
    // A retention purges only what's older than it, never a value in use.
    api.ok(Method::PATCH, "", Some(json!({ "history_days": 1 }))).await;
    let sec = api.secrets.clone();
    tokio::task::spawn_blocking(move || sec.sweep().unwrap()).await.unwrap();
    assert_eq!(api.values("production", "/").await["values"]["A"], "10");
    assert_eq!(api.ok(Method::GET, "", None).await["history_days"], 1);
    // Check-and-set: two writers read A at one version; the second's change is refused whole.
    let b = api.ok(Method::GET, "/environments/production/secrets/A", None).await["version"].as_i64().unwrap();
    api.ok(Method::PUT, "/environments/production/secrets/A", Some(json!({ "value": "first", "if_version": b }))).await;
    let (st, e) = api.call(Method::POST, "/environments/production/changes", Some(json!({ "ops": [{ "op": "set", "key": "D", "value": "4" }, { "op": "set", "key": "A", "value": "second", "if_version": b }] }))).await;
    assert!(st == StatusCode::CONFLICT && e["code"] == "stale" && e["error"].as_str().unwrap().contains("A changed since version"), "{e}");
    assert_eq!((api.values("production", "/").await["values"]["A"].clone(), api.values("production", "/").await["values"]["D"].clone()), (json!("first"), Value::Null), "nothing of the refused change");
    // 0: only if it doesn't exist yet.
    assert_eq!(api.call(Method::PUT, "/environments/production/secrets/A", Some(json!({ "value": "x", "if_version": 0 }))).await.0, StatusCode::CONFLICT);
    api.ok(Method::PUT, "/environments/production/secrets/NEWKEY", Some(json!({ "value": "x", "if_version": 0 }))).await;
    // Protected: a stale read is told so, not sent to a request; a request keeps the check.
    let now = api.ok(Method::GET, "/environments/production/secrets/A", None).await["version"].as_i64().unwrap();
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true }))).await;
    let (st, e) = api.call(Method::PUT, "/environments/production/secrets/A", Some(json!({ "value": "y", "if_version": now - 1 }))).await;
    assert!(st == StatusCode::CONFLICT && e["code"] == "stale", "{e}");
    let (st, e) = api.call(Method::POST, "/environments/production/requests", Some(json!({ "ops": [{ "op": "set", "key": "A", "value": "y", "if_version": now - 1 }] }))).await;
    assert!(st == StatusCode::CONFLICT && e["code"] == "stale", "{e}");
    api.ok(Method::POST, "/environments/production/requests", Some(json!({ "ops": [{ "op": "set", "key": "A", "value": "y", "if_version": now }] }))).await;
}

#[tokio::test]
async fn a_change_of_thousands_of_keys_keeps_every_version_and_the_audit_chain() {
    let api = Api::new().await;
    const N: usize = 3000;
    let dotenv: String = (0..N).map(|i| format!("K{i:04}=one-{i}\n")).collect();
    let imported = api.ok(Method::POST, "/environments/production/import", Some(json!({ "dotenv": dotenv }))).await;
    assert_eq!(imported["items"].as_array().unwrap().len(), N);
    // One change: the first third changed, the second third deleted, as many keys added (some in
    // a new folder), the last third untouched.
    let mut ops: Vec<Value> = (0..N / 3).map(|i| json!({ "op": "set", "key": format!("K{i:04}"), "value": format!("two-{i}") })).collect();
    ops.extend((N / 3..2 * N / 3).map(|i| json!({ "op": "delete", "key": format!("K{i:04}") })));
    ops.extend((0..N / 3).map(|i| json!({ "op": "set", "key": format!("N{i:04}"), "value": format!("new-{i}"), "path": if i % 2 == 0 { "/" } else { "/svc/api" } })));
    let second = api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": ops }))).await;
    let seq = second["change"]["seq"].as_i64().unwrap();
    let detail = api.ok(Method::GET, &format!("/environments/production/changes/{seq}"), None).await;
    let items = detail["items"].as_array().unwrap();
    let what = |w: &str| items.iter().filter(|i| i["what"][0] == w).count();
    assert_eq!((detail["keys"].as_i64(), what("changed"), what("deleted"), what("added")), (Some(N as i64), N / 3, N / 3, N / 3));
    assert!(items.iter().all(|i| i["what"][0] != "changed" || (i["from_fp"].is_string() && i["from_fp"] != i["to_fp"])), "{detail}");
    // Deleted keys come back with their history, in the same change as other new ones.
    let back: Vec<Value> = (N / 3..N / 3 + 10).map(|i| json!({ "op": "set", "key": format!("K{i:04}"), "value": format!("three-{i}") })).collect();
    api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": back }))).await;

    let root = api.values("production", "/").await["values"].clone();
    let folder = api.values("production", "/svc/api").await["values"].clone();
    assert_eq!((root["K0000"].as_str(), root["K1000"].as_str(), root["K1010"].as_str(), root["K2999"].as_str()), (Some("two-0"), Some("three-1000"), None, Some("one-2999")));
    assert_eq!((root["N0000"].as_str(), folder["N0001"].as_str()), (Some("new-0"), Some("new-1")));
    assert_eq!(root.as_object().unwrap().len(), N / 3 + 10 + N / 3 + N / 6);
    let versions = api.ok(Method::GET, "/environments/production/secrets/K1000/versions", None).await;
    let ops: Vec<(i64, &str)> = versions.as_array().unwrap().iter().map(|v| (v["version"].as_i64().unwrap(), v["op"].as_str().unwrap())).collect();
    assert_eq!(ops, [(3, "set"), (2, "delete"), (1, "set")]);
    assert_eq!(api.ok(Method::GET, "/environments/production/secrets/K1000/versions/1/value", None).await["value"], "one-1000");
    assert_eq!(api.ok(Method::GET, "/audit/verify", None).await["ok"], true);
}

#[tokio::test]
async fn references_imports_and_personal_values_resolve_and_cycles_are_reported() {
    let api = Api::new().await;
    api.set("production", "DB_HOST", "prod.db").await;
    api.ok(Method::PUT, "/environments/development/secrets/PASS?path=/db", Some(json!({ "value": "pw" }))).await;
    api.ok(
        Method::POST,
        "/environments/development/changes",
        Some(json!({ "ops": [
            { "op": "set", "key": "URL", "value": "postgres://u:${development.db.PASS}@${production.DB_HOST}/x" },
            { "op": "set", "key": "LITERAL", "value": "$${HOME}" },
            { "op": "set", "key": "A", "value": "${B}" },
            { "op": "set", "key": "B", "value": "${A}" },
        ] })),
    )
    .await;
    let v = api.values("development", "/").await;
    assert_eq!(v["values"]["URL"], "postgres://u:pw@prod.db/x");
    assert_eq!(v["values"]["LITERAL"], "${HOME}");
    // What a reference reaches moves the digest (roll run --watch, syncs), in another environment too.
    let before = api.ok(Method::GET, "/environments/development/digest", None).await["digest"].clone();
    // A values answer carries the same digest, taken before its read: a watch compares from it.
    let (_, with) = api.call(Method::GET, "/environments/development/values?digest=1", None).await;
    assert_eq!((with["digest"].clone(), api.values("development", "/").await.get("digest").cloned()), (before.clone(), None), "asked for (a watch)");
    // A watcher long-polls instead: unchanged, it holds until `wait` runs out; a change answers it
    // at once (well before its 30 s), with the new digest.
    let since = before.as_str().unwrap().to_string();
    let (router, project) = (api.router.clone(), api.project.clone());
    let waiting = tokio::spawn(async move {
        let started = std::time::Instant::now();
        let req = Request::get(format!("/v1/secrets/projects/{project}/environments/development/digest?since={since}&wait=30")).body(Body::empty()).unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let body: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap()).unwrap();
        (body["digest"].clone(), started.elapsed())
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    api.set("production", "DB_HOST", "prod2.db").await;
    let (moved, took) = waiting.await.unwrap();
    assert!(moved != before && took < std::time::Duration::from_secs(10), "{moved} after {took:?}");
    assert_eq!(api.ok(Method::GET, "/environments/development/digest", None).await["digest"], moved);
    api.set("production", "DB_HOST", "prod.db").await;
    let problems: Vec<&str> = v["problems"].as_array().unwrap().iter().map(|p| p["problem"].as_str().unwrap()).collect();
    assert!(problems.iter().any(|p| p.starts_with("cycle: A → B → A")), "{problems:?}");
    let w = api.ok(Method::PUT, "/environments/development/secrets/X", Some(json!({ "value": "${staging.NOPE}" }))).await;
    assert_eq!(w["warnings"], json!(["X references staging.NOPE, which doesn't exist"]));

    // An import fills in what the folder doesn't set; the folder's own key wins.
    api.set("staging", "SHARED", "from-staging").await;
    api.set("staging", "URL", "staging-url").await;
    api.ok(Method::PUT, "/environments/development/imports?path=/", Some(json!([{ "environment": "staging", "path": "/" }]))).await;
    let v = api.values("development", "/").await;
    assert_eq!((v["values"]["SHARED"].as_str(), v["values"]["URL"].as_str()), (Some("from-staging"), Some("postgres://u:pw@prod.db/x")));
    let s = api.ok(Method::GET, "/environments/development/secrets/SHARED", None).await;
    assert_eq!(s["source"], json!({ "kind": "import", "environment": "staging", "path": "/" }));

    // A personal value replaces the shared one for its owner only.
    api.ok(Method::PUT, "/environments/staging/secrets/SHARED", Some(json!({ "value": "mine", "type": "personal" }))).await;
    assert_eq!(api.values("staging", "/").await["values"]["SHARED"], "mine");
    let token = api.token(json!({ "name": "ci", "actions": ["read", "reveal"] }));
    let (_, v) = api.req(Method::GET, "/environments/staging/values", None, Some(token.as_str())).await;
    assert_eq!(v["values"]["SHARED"], "from-staging");
}

#[tokio::test]
async fn a_set_without_a_value_changes_only_a_key_that_is_there() {
    let api = Api::new().await;
    api.set("staging", "TOKEN", "t0ken").await;
    // A wrong name, folder or type makes no empty key shadowing the real one: in a batch too.
    for (url, body) in [
        ("/environments/staging/secrets/TOKNE", json!({ "comment": "the api token" })),
        ("/environments/staging/secrets/TOKEN?path=/api", json!({ "comment": "the api token" })),
        ("/environments/staging/secrets/TOKEN", json!({ "comment": "the api token", "type": "personal" })),
        ("/environments/staging/changes", json!({ "ops": [{ "op": "set", "key": "TOKEN", "type": "personal", "tags": ["api"] }] })),
    ] {
        let method = if url.ends_with("/changes") { Method::POST } else { Method::PUT };
        let (s, v) = api.call(method, url, Some(body)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{url}: {v}");
        assert!(v.to_string().contains("a new secret needs a value"), "{v}");
    }
    assert_eq!(api.values("staging", "/").await["values"], json!({ "TOKEN": "t0ken" }));
    for path in ["/", "/api"] {
        let list = api.ok(Method::GET, &format!("/environments/staging/secrets?path={path}"), None).await;
        let own: Vec<_> = list.as_array().unwrap().iter().filter(|s| s["source"]["kind"] == "own").map(|s| (s["key"].clone(), s["type"].clone())).collect();
        assert_eq!(own, if path == "/" { vec![(json!("TOKEN"), json!("shared"))] } else { vec![] }, "{path}: {list}");
    }

    // On the key that's there, only its comment changes.
    api.ok(Method::PUT, "/environments/staging/secrets/TOKEN", Some(json!({ "comment": "the api token" }))).await;
    let s = api.ok(Method::GET, "/environments/staging/secrets/TOKEN", None).await;
    assert_eq!((s["comment"].as_str(), s["version"].as_i64()), (Some("the api token"), Some(2)));
    assert_eq!(api.values("staging", "/").await["values"], json!({ "TOKEN": "t0ken" }));
}

#[tokio::test]
async fn ephemeral_environments_inherit_their_base_and_go_by_label_or_expiry() {
    let api = Api::new().await;
    api.set("staging", "API", "https://staging").await;
    api.set("staging", "DATABASE_URL", "postgres://staging").await;
    let e = api
        .ok(
            Method::POST,
            "/environments",
            Some(json!({ "slug": "pr-117", "base": "staging", "labels": { "pr": "117" }, "ttl_minutes": 60, "secrets": [{ "op": "set", "key": "DATABASE_URL", "value": "postgres://pr-117" }] })),
        )
        .await;
    assert_eq!((e["kind"].as_str(), e["base"].as_str()), (Some("ephemeral"), Some("staging")));
    assert_eq!(api.values("pr-117", "/").await["values"], json!({ "API": "https://staging", "DATABASE_URL": "postgres://pr-117" }));
    // The base changes; the preview follows.
    api.set("staging", "API", "https://staging-2").await;
    assert_eq!(api.values("pr-117", "/").await["values"]["API"], "https://staging-2");
    let grid = api.ok(Method::GET, "/grid", None).await;
    assert_eq!((grid["environments"].as_array().unwrap().len(), grid["previews"].as_i64()), (3, Some(1)));

    api.ok(Method::POST, "/environments", Some(json!({ "slug": "pr-118", "base": "staging", "labels": { "pr": "118" } }))).await;
    let gone = api.ok(Method::DELETE, "/environments?label=pr=117", None).await;
    assert_eq!(gone["deleted"], json!(["pr-117"]));
    api.ok(Method::PATCH, "/environments/pr-118", Some(json!({ "expires_at": 1 }))).await;
    assert_eq!(api.secrets.sweep().unwrap().expired, 1);
    assert_eq!(slugs(&api.ok(Method::GET, "/environments", None).await), ["development", "staging", "production"]);

    // Ensuring one: created, then unchanged, then brought up to date in one change.
    let want = |db: &str, extra: bool| {
        let mut ops = vec![json!({ "op": "set", "key": "DATABASE_URL", "value": db })];
        if extra {
            ops.push(json!({ "op": "set", "key": "EXTRA", "value": "x" }));
        }
        json!({ "base": "staging", "labels": { "preview": "pv-1" }, "ttl_minutes": 60, "secrets": ops, "replace": true })
    };
    let (s, first) = api.call(Method::PUT, "/environments/pv-1", Some(want("postgres://pv-1", true))).await;
    assert_eq!((s, first["created"].as_bool()), (StatusCode::CREATED, Some(true)));
    let again = api.ok(Method::PUT, "/environments/pv-1", Some(want("postgres://pv-1", true))).await;
    assert_eq!((again["created"].as_bool(), again["change"].clone()), (Some(false), Value::Null), "ensuring what's there is no change");
    let ensured = api.ok(Method::GET, "/audit?action=environment.ensure", None).await;
    assert_eq!(ensured.as_array().map(Vec::len), Some(0), "nor an audit row, though \"an hour from now\" moved a little: {ensured}");
    let moved = api.ok(Method::PUT, "/environments/pv-1", Some(want("postgres://pv-1b", false))).await;
    assert_eq!(moved["change"]["keys"], 2, "one change: the new value, and EXTRA gone");
    assert_eq!(api.values("pv-1", "/").await["values"], json!({ "API": "https://staging-2", "DATABASE_URL": "postgres://pv-1b" }));
    let (s, e) = api.call(Method::PUT, "/environments/pv-1", Some(json!({ "base": "production" }))).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("mismatch")));
}

#[tokio::test]
async fn a_preview_never_gets_a_value_of_a_resource_it_branched() {
    let api = Api::new().await;
    for r in ["assets", "logs"] {
        api.ok(Method::PUT, &format!("/v1/workspace/projects/{}/resources/bucket/{r}", api.project), Some(json!({ "x": 0, "y": 0 }))).await;
    }
    let link = |r: &str, field: &str, mode: &str| json!({ "kind": "bucket", "ref": r, "field": field, "mode": mode });
    api.ok(
        Method::POST,
        "/environments/staging/changes",
        Some(json!({ "ops": [
            { "op": "set", "key": "BUCKET_URL", "link": link("assets", "url", "live") },
            { "op": "set", "key": "AWS_SECRET_ACCESS_KEY", "link": link("assets", "secret_access_key", "managed") },
            { "op": "set", "key": "TEMP_KEY", "link": link("assets", "access_key_id", "dynamic") },
            { "op": "set", "key": "LOGS_URL", "link": link("logs", "url", "live") },
        ] })),
    )
    .await;
    api.fake.asked.lock().unwrap().clear();
    // roll previews ensures pv-1 over staging, naming what it branched; a key of its own reaching
    // a followed one by reference doesn't resolve either.
    let ensure = |branches: Option<Value>| {
        let mut b = json!({ "base": "staging", "labels": { "preview": "pv-1" }, "secrets": [{ "op": "set", "key": "MIRROR", "value": "${BUCKET_URL}" }], "replace": true });
        if let Some(l) = branches {
            b["branches"] = l;
        }
        b
    };
    let assets = json!([{ "kind": "bucket", "parent": "assets" }]);
    let made = api.ok(Method::PUT, "/environments/pv-1", Some(ensure(Some(assets.clone())))).await;
    assert_eq!(made["environment"]["branches"], assets);
    let followed = json!([
        { "name": "AWS_SECRET_ACCESS_KEY", "kind": "bucket", "parent": "assets", "field": "secret_access_key" },
        { "name": "BUCKET_URL", "kind": "bucket", "parent": "assets", "field": "url" },
        { "name": "TEMP_KEY", "kind": "bucket", "parent": "assets", "field": "access_key_id" },
    ]);
    let v = api.values("pv-1", "/").await;
    assert_eq!((v["values"]["LOGS_URL"].as_str(), v["values"].as_object().unwrap().len()), (Some("s3://logs?pw=0"), 2), "LOGS_URL and MIRROR: {v}");
    assert_eq!((v["followed"].clone(), v["problems"][0]["key"].as_str()), (followed.clone(), Some("MIRROR")));
    let r = api.ok(Method::POST, "/environments/pv-1/resolve", Some(json!({ "consumer": "preview:pv-1/web", "untrusted": true }))).await;
    assert_eq!((r["followed"].clone(), r["values"].get("BUCKET_URL"), r["leases"].clone()), (followed.clone(), None, json!([])));
    let sk = [7u8; 32];
    let sealed = api.ok(Method::POST, "/environments/pv-1/resolve", Some(json!({ "consumer": "preview:pv-1/web", "seal_to": roll_secrets::crypto::public_key(&sk) }))).await;
    assert_eq!((sealed["followed"].clone(), sealed["leases"].clone()), (followed, json!([])), "named in the clear, nothing issued");
    let (_, text) = api.call(Method::GET, "/environments/pv-1/values?format=dotenv", None).await;
    assert!(text.as_str().unwrap().contains("LOGS_URL=") && !text.as_str().unwrap().contains("BUCKET_URL=") && !text.as_str().unwrap().contains("s3://assets"), "{text}");
    let digest = api.ok(Method::GET, "/environments/pv-1/digest", None).await["digest"].clone();
    api.secrets.forget_live_fingerprints();
    assert_eq!(api.ok(Method::GET, "/environments/pv-1/digest", None).await["digest"], digest);
    let (s, e) = api.call(Method::GET, "/environments/pv-1/secrets/BUCKET_URL/value", None).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("followed")));
    assert!(api.fake.asked.lock().unwrap().iter().all(|r| r == "logs"), "the parent was never asked: {:?}", api.fake.asked.lock().unwrap());

    // An ensure without the field keeps the list; a standard environment has none.
    api.ok(Method::PUT, "/environments/pv-1", Some(ensure(None))).await;
    assert_eq!(api.ok(Method::GET, "/environments/pv-1", None).await["branches"], assets);
    let (s, _) = api.call(Method::PUT, "/environments/staging", Some(json!({ "branches": [] }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // Nobody else changes the list: adding changes what previews fills, removing would hand the
    // preview's code the parent's values. The same list, or none, is fine.
    let t = api.token(json!({ "name": "ci", "actions": ["read", "reveal", "write"] }));
    let both = json!([{ "kind": "bucket", "parent": "assets" }, { "kind": "bucket", "parent": "logs" }]);
    let (s, _) = api.req(Method::PUT, "/environments/pv-1", Some(ensure(Some(both.clone()))), Some(&t)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = api.req(Method::PUT, "/environments/pv-2", Some(ensure(Some(both))), Some(&t)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = api.req(Method::PUT, "/environments/pv-1", Some(ensure(Some(json!([])))), Some(&t)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = api.req(Method::PUT, "/environments/pv-1", Some(ensure(Some(assets.clone()))), Some(&t)).await;
    assert_eq!(s, StatusCode::OK);
    assert!(api.values("pv-1", "/").await["values"].get("BUCKET_URL").is_none(), "still followed");
}

#[tokio::test]
async fn the_grid_flags_differences_and_promote_copies_them() {
    let api = Api::new().await;
    for (env, v) in [("development", "a"), ("staging", "a"), ("production", "b")] {
        api.set(env, "SAME_OR_NOT", v).await;
    }
    api.set("development", "NEW", "n").await;
    api.set("development", "OTHER", "o").await;
    // A required key nobody set yet is a row of its own, and reading values reports it.
    api.ok(Method::PUT, "/required", Some(json!([{ "key": "STRIPE_KEY", "comment": "payments" }]))).await;
    let grid = api.ok(Method::GET, "/grid", None).await;
    let status: BTreeMap<&str, &str> = grid["rows"].as_array().unwrap().iter().map(|r| (r["key"].as_str().unwrap(), r["status"].as_str().unwrap())).collect();
    assert_eq!(status, BTreeMap::from([("NEW", "missing"), ("OTHER", "missing"), ("SAME_OR_NOT", "different"), ("STRIPE_KEY", "missing")]));
    assert_eq!(api.values("staging", "/").await["problems"], json!([{ "key": "STRIPE_KEY", "problem": "required, but staging doesn't set it" }]));
    assert!(!grid.to_string().contains("\"a\""), "no values in the grid");

    let cmp = api.ok(Method::GET, "/compare?from=development&to=staging", None).await;
    let rows: Vec<(&str, &str)> = cmp["rows"].as_array().unwrap().iter().map(|r| (r["key"].as_str().unwrap(), r["status"].as_str().unwrap())).collect();
    assert_eq!(rows, [("NEW", "only_from"), ("OTHER", "only_from"), ("SAME_OR_NOT", "same")]);
    let p = api.ok(Method::POST, "/environments/staging/promote?from=development", Some(json!({ "keys": ["NEW"] }))).await;
    assert_eq!(p["change"]["kind"], "promote");
    assert_eq!(api.values("staging", "/").await["values"], json!({ "NEW": "n", "SAME_OR_NOT": "a" }));

    // Typed keys: a value that doesn't fit is refused by key (never echoed); one that only fits
    // once resolved is checked then, so roll run won't start with it.
    let (s, _) = api.call(Method::PUT, "/required", Some(json!([{ "key": "PORT", "type": "port" }, { "key": "REGION", "pattern": "[a-z]{2}-[a-z]+-[0-9" }]))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a pattern that doesn't compile");
    api.ok(Method::PUT, "/required", Some(json!([{ "key": "PORT", "type": "port" }, { "key": "REGION", "pattern": "[a-z]{2}-[a-z]+-[0-9]" }]))).await;
    let (s, e) = api.call(Method::PUT, "/environments/staging/secrets/PORT", Some(json!({ "value": "eighty-eighty" }))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("PORT must be a port (1–65535)")));
    let (s, _) = api.call(Method::POST, "/environments/staging/import", Some(json!({ "dotenv": "REGION=Europe\n" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    api.set("staging", "PORT", "8080").await;
    api.set("staging", "REGION", "eu-west-1").await;
    api.set("development", "WRONG", "not a port").await;
    api.set("development", "PORT", "${WRONG}").await;
    let problems = api.values("development", "/").await["problems"].clone();
    assert!(problems.as_array().unwrap().iter().any(|p| p["key"] == "PORT" && p["problem"] == "PORT must be a port (1–65535)"), "{problems}");
    // A password policy: a value too short, missing a class or using another is refused by key,
    // never echoed; a policy nobody could meet isn't taken.
    let (s, _) = api.call(Method::PUT, "/required", Some(json!([{ "key": "DB_PASSWORD", "password": { "length": 4 } }]))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "8 characters at least");
    api.ok(Method::PUT, "/required", Some(json!([{ "key": "PORT", "type": "port" }, { "key": "DB_PASSWORD", "password": { "length": 12, "classes": ["lower", "digit", "symbol"], "symbols": "-_" } }]))).await;
    for (value, why) in [("short-1", "DB_PASSWORD must be at least 12 characters"), ("no-digits-here", "DB_PASSWORD needs at least a digit"), ("Upper-case-1x", "DB_PASSWORD may only use a lowercase letter, a digit, a symbol")] {
        let (s, e) = api.call(Method::PUT, "/environments/staging/secrets/DB_PASSWORD", Some(json!({ "value": value }))).await;
        assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some(why)), "{value}");
    }
    api.set("staging", "DB_PASSWORD", "lower-and-digit-9").await;
    assert_eq!(api.ok(Method::GET, "/required", None).await[0]["password"]["symbols"], "-_");
    // A type added after the values: the grid says which don't fit, to whoever may reveal them.
    api.ok(Method::PUT, "/required", Some(json!([{ "key": "SAME_OR_NOT", "type": "int" }]))).await;
    let grid = api.ok(Method::GET, "/grid", None).await;
    let row = grid["rows"].as_array().unwrap().iter().find(|r| r["key"] == "SAME_OR_NOT").unwrap().clone();
    assert_eq!(row["cells"]["production"]["misfit"], "SAME_OR_NOT must be an integer");
}

#[tokio::test]
async fn protected_environments_take_change_requests_that_another_principal_approves() {
    let api = Api::new().await;
    api.set("production", "KEY", "old").await;
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true, "approvals": 1 }))).await;
    let (s, e) = api.call(Method::PUT, "/environments/production/secrets/KEY", Some(json!({ "value": "new" }))).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("approval_required")));

    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true, "approvals": 1, "notify_url": "https://hooks.example/T0/B0/secret-path" }))).await;
    let r = api.ok(Method::POST, "/environments/production/requests", Some(json!({ "message": "rotate", "ops": [{ "op": "set", "key": "KEY", "value": "new" }] }))).await;
    let id = r["id"].as_str().unwrap();
    assert_eq!((r["state"].as_str(), r["items"][0]["what"].clone()), (Some("open"), json!(["changed"])));
    assert!(!r.to_string().contains("\"new\""), "a request never shows its values");
    let (s, _) = api.call(Method::POST, &format!("/environments/production/requests/{id}/apply"), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // Its approvers hear about it, by key name only, from the sweeper; the environment shows the
    // URL's host, never the URL.
    let sec = api.secrets.clone();
    tokio::task::spawn_blocking(move || sec.sweep().unwrap()).await.unwrap();
    let notice = api.sent.0.lock().unwrap().iter().find(|b| b["request"]["id"] == id).cloned().unwrap();
    assert_eq!(notice["request"]["keys"], json!(["KEY"]));
    assert!(notice["text"].as_str().unwrap().contains("opened change request") && !notice.to_string().contains("\"new\""));
    let env = api.ok(Method::GET, "/environments/production", None).await;
    assert_eq!(env["notify"], "hooks.example");
    // An approver who may reveal sees what changes, the value now beside the one proposed.
    let v = api.ok(Method::GET, &format!("/environments/production/requests/{id}/values"), None).await;
    assert_eq!((v[0]["key"].as_str(), v[0]["now"].as_str(), v[0]["proposed"].as_str()), (Some("KEY"), Some("old"), Some("new")));
    let (s, _) = api.call(Method::POST, &format!("/environments/production/requests/{id}/review"), Some(json!({ "verdict": "approve" }))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "the author doesn't approve their own request");

    let reviewer = api.token(json!({ "name": "lead", "actions": ["read", "admin", "write"] }));
    let (s, r) = api.req(Method::POST, &format!("/environments/production/requests/{id}/review"), Some(json!({ "verdict": "approve", "comment": "ok" })), Some(reviewer.as_str())).await;
    assert_eq!((s, r["state"].as_str()), (StatusCode::OK, Some("approved")));
    let r = api.ok(Method::POST, &format!("/environments/production/requests/{id}/apply"), None).await;
    assert_eq!(r["state"], "applied");
    assert_eq!(api.values("production", "/").await["values"]["KEY"], "new");

    // An admin can bypass, and the bypass is in the audit log.
    let r = api.ok(Method::POST, "/environments/production/requests", Some(json!({ "ops": [{ "op": "set", "key": "KEY", "value": "newer" }] }))).await;
    api.ok(Method::POST, &format!("/environments/production/requests/{}/apply", r["id"].as_str().unwrap()), Some(json!({ "bypass": true }))).await;
    let audit = api.ok(Method::GET, "/audit?action=approval", None).await;
    assert_eq!(audit[0]["action"], "approval.bypass");

    // A promotion into it is a change request too, with the values the browser never had.
    api.set("staging", "KEY", "from-staging").await;
    let p = api.ok(Method::POST, "/environments/production/promote?from=staging", Some(json!({ "keys": ["KEY"] }))).await;
    assert_eq!((p["change"].clone(), p["request"]["state"].as_str()), (Value::Null, Some("open")));
    api.ok(Method::POST, &format!("/environments/production/requests/{}/apply", p["request"]["id"].as_str().unwrap()), Some(json!({ "bypass": true }))).await;
    assert_eq!(api.values("production", "/").await["values"]["KEY"], "from-staging");
    // A folder's own change policy, in an environment that isn't protected: its keys change only
    // through approved requests; the rest of the environment as before.
    api.ok(Method::PUT, "/environments/staging/folder-policies?path=/payments", Some(json!({ "approvals": 1 }))).await;
    let (s, e) = api.call(Method::PUT, "/environments/staging/secrets/STRIPE?path=/payments/eu", Some(json!({ "value": "sk_1" }))).await;
    assert!(s == StatusCode::CONFLICT && e["code"] == "approval_required" && e["error"].as_str().unwrap().contains("/payments"), "{e}");
    api.ok(Method::PUT, "/environments/staging/secrets/OTHER", Some(json!({ "value": "free" }))).await;
    let r = api.ok(Method::POST, "/environments/staging/requests", Some(json!({ "path": "/payments/eu", "ops": [{ "op": "set", "key": "STRIPE", "value": "sk_1" }] }))).await;
    assert_eq!(r["gates"], json!([{ "path": "/payments", "approvals": 1, "approved": 0 }]));
    let id = r["id"].as_str().unwrap();
    let (s, r) = api.req(Method::POST, &format!("/environments/staging/requests/{id}/review"), Some(json!({ "verdict": "approve" })), Some(&reviewer)).await;
    assert_eq!((s, r["state"].as_str()), (StatusCode::OK, Some("approved")));
    api.ok(Method::POST, &format!("/environments/staging/requests/{id}/apply"), None).await;
    assert_eq!(api.values("staging", "/payments/eu").await["values"]["STRIPE"], "sk_1");
    // Protected as well: an admin of /payments only approves for its folder's gate, never for the
    // environment's.
    api.ok(Method::PUT, "/environments/staging/policy", Some(json!({ "protected": true, "approvals": 1 }))).await;
    let folder_admin = api.token(json!({ "name": "payments", "environments": ["staging"], "paths": ["/payments"], "actions": ["read", "write", "admin"] }));
    let r = api.ok(Method::POST, "/environments/staging/requests", Some(json!({ "path": "/payments/eu", "ops": [{ "op": "set", "key": "STRIPE", "value": "sk_2" }] }))).await;
    let id = r["id"].as_str().unwrap();
    let (s, r) = api.req(Method::POST, &format!("/environments/staging/requests/{id}/review"), Some(json!({ "verdict": "approve" })), Some(&folder_admin)).await;
    assert_eq!((s, r["state"].as_str()), (StatusCode::OK, Some("open")), "{r}");
    assert_eq!(r["gates"].as_array().unwrap().iter().map(|g| g["approved"].as_i64().unwrap()).collect::<Vec<_>>(), [0, 1], "the environment's gate, then /payments'");
    api.ok(Method::PUT, "/environments/staging/policy", Some(json!({ "protected": false }))).await;
    // Nor by deleting the folder, or one above it.
    for folder in ["/payments", "/payments/eu"] {
        let (s, e) = api.call(Method::DELETE, &format!("/environments/staging/folders?path={folder}"), None).await;
        assert!(s == StatusCode::CONFLICT && e["code"] == "approval_required", "{folder}: {e}");
    }
}

#[tokio::test]
async fn a_token_reaches_only_its_environments_folders_and_actions() {
    let api = Api::new().await;
    api.ok(Method::PUT, "/environments/production/secrets/STRIPE?path=/api", Some(json!({ "value": "sk_live" }))).await;
    api.set("production", "ROOT", "r").await;
    // As `POST /v1/access/tokens` makes it from secrets' page; roll-access keeps it (expiry, client
    // addresses, revocation), secrets checks each second resource against it.
    let token = api.token(json!({ "environments": ["production"], "paths": ["/api"], "actions": ["read", "reveal"] }));
    let reads = api.secrets.metrics().unwrap().reads;
    let (s, v) = api.req(Method::GET, "/environments/production/values?path=/api", None, Some(&token)).await;
    assert_eq!(api.secrets.metrics().unwrap().reads, reads + 1, "rolld's metrics count each read");
    assert_eq!((s, v["values"]["STRIPE"].as_str()), (StatusCode::OK, Some("sk_live")));
    for (method, path, body) in [
        (Method::GET, "/environments/production/values?path=/", None),
        (Method::GET, "/environments/staging/values?path=/api", None),
        (Method::PUT, "/environments/production/secrets/STRIPE?path=/api", Some(json!({ "value": "x" }))),
    ] {
        let (s, _) = api.req(method.clone(), path, body, Some(&token)).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{method} {path}");
    }
    let (_, who) = api.req(Method::GET, "/v1/secrets/whoami", None, Some(&token)).await;
    assert_eq!(who["project"].as_str(), Some(api.project.as_str()));
    // A token in a loop is told to slow down before it decrypts and audits without end; the
    // operator sets how far (600 a minute unless told).
    api.secrets.set_reads_per_minute(5);
    let mut refused = None;
    for _ in 0..6 {
        let (s, v) = api.req(Method::GET, "/environments/production/values?path=/api", None, Some(&token)).await;
        if s == StatusCode::TOO_MANY_REQUESTS {
            refused = Some(v["code"].clone());
            break;
        }
    }
    assert_eq!(refused, Some(json!("rate_limited")));
    api.secrets.set_reads_per_minute(600);
    let (s, _) = api.req(Method::GET, "/environments/production/values?path=/api", None, Some(&token)).await;
    assert_eq!(s, StatusCode::OK);
    // A project-wide read answers per environment; one about the project as a whole (other
    // environments' key names, shares, required keys) needs a rule over all of it.
    assert_eq!(api.req(Method::GET, "/environments", None, Some(&token)).await.0, StatusCode::OK);
    for path in ["/suggestions", "/shares", "/required"] {
        assert_eq!(api.req(Method::GET, path, None, Some(&token)).await.0, StatusCode::FORBIDDEN, "{path}");
    }
}

#[tokio::test]
async fn links_read_live_values_issue_managed_credentials_rotate_and_lease_dynamic_ones() {
    let api = Api::new().await;
    let (s, e) = api.call(Method::PUT, "/environments/production/secrets/BUCKET_URL", Some(json!({ "link": { "kind": "bucket", "ref": "assets", "field": "url", "mode": "live" } }))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("bucket assets isn't in this project")));
    let place = format!("/v1/workspace/projects/{}/resources/bucket/assets", api.project);
    api.ok(Method::PUT, &place, Some(json!({ "x": 0, "y": 0 }))).await;
    // On the canvas and linked nowhere: offered for every environment, under its usual keys.
    let offered = api.ok(Method::GET, "/suggestions", None).await;
    assert_eq!((offered[0]["ref"].as_str(), offered[0]["environments"].clone()), (Some("assets"), json!(["development", "staging", "production"])));
    assert_eq!(offered[0]["links"][1], json!({ "key": "AWS_ACCESS_KEY_ID", "field": "access_key_id", "mode": "managed" }));
    let link = |field: &str, mode: &str| json!({ "kind": "bucket", "ref": "assets", "field": field, "mode": mode });
    api.ok(
        Method::POST,
        "/environments/production/changes",
        Some(json!({ "ops": [
            { "op": "set", "key": "BUCKET_URL", "link": link("url", "live") },
            { "op": "set", "key": "AWS_ACCESS_KEY_ID", "link": link("access_key_id", "managed") },
            { "op": "set", "key": "AWS_SECRET_ACCESS_KEY", "link": link("secret_access_key", "managed") },
        ] })),
    )
    .await;
    assert_eq!(api.ok(Method::GET, "/suggestions", None).await[0]["environments"], json!(["development", "staging"]), "linked in production now");
    // One credential for the pair; the live URL follows the resource.
    let v = api.values("production", "/").await["values"].clone();
    assert_eq!((v["AWS_ACCESS_KEY_ID"].as_str(), v["AWS_SECRET_ACCESS_KEY"].as_str(), v["BUCKET_URL"].as_str()), (Some("AK1"), Some("sk-1"), Some("s3://assets?pw=0")));
    // Rotating the resource's own credential moves the digest, so watches and deploys see it.
    let digest = api.ok(Method::GET, "/environments/production/digest", None).await["digest"].clone();
    api.ok(Method::POST, "/environments/production/secrets/BUCKET_URL/rotate", None).await;
    assert_eq!(api.values("production", "/").await["values"]["BUCKET_URL"], "s3://assets?pw=1");
    assert_ne!(api.ok(Method::GET, "/environments/production/digest", None).await["digest"], digest);

    // Rotation issues a new pair as one change; the old key works until its grace ends.
    let r = api.ok(Method::POST, "/environments/production/secrets/AWS_SECRET_ACCESS_KEY/rotate", None).await;
    assert_eq!(r["items"].as_array().unwrap().len(), 2);
    let v = api.values("production", "/").await["values"].clone();
    assert_eq!((v["AWS_ACCESS_KEY_ID"].as_str(), v["AWS_SECRET_ACCESS_KEY"].as_str()), (Some("AK2"), Some("sk-2")));
    assert_eq!(*api.fake.live_keys.lock().unwrap(), ["AK1", "AK2"]);
    // Of the linked keys, only the one whose field isn't a credential is plain (a log may show it).
    let all = api.values("production", "/").await;
    assert_eq!(all["plain"], json!(["AWS_ACCESS_KEY_ID"]), "{all}");
    // Each linked key names what it links (a consumer replacing some resources drops only theirs).
    let mut links = all["links"].as_array().cloned().unwrap_or_default();
    links.sort_by_key(|l| l["name"].as_str().unwrap_or_default().to_string());
    let of = |name: &str| json!({ "name": name, "kind": "bucket", "ref": "assets" });
    assert_eq!(links, [of("AWS_ACCESS_KEY_ID"), of("AWS_SECRET_ACCESS_KEY"), of("BUCKET_URL")], "{all}");
    let leases = api.ok(Method::GET, "/leases", None).await;
    assert_eq!(leases[0]["kind"], "retired");
    api.ok(Method::DELETE, &format!("/leases/{}", leases[0]["id"].as_str().unwrap()), None).await;
    assert_eq!(*api.fake.live_keys.lock().unwrap(), ["AK2"]);

    // Dynamic: every resolve issues a fresh credential under a lease.
    api.ok(Method::PUT, "/environments/staging/secrets/TEMP_KEY", Some(json!({ "link": { "kind": "bucket", "ref": "assets", "field": "access_key_id", "mode": "dynamic", "options": { "ttl_minutes": 1 } } }))).await;
    let a = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "app:web" }))).await;
    let b = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "app:web" }))).await;
    assert_ne!(a["values"]["TEMP_KEY"], b["values"]["TEMP_KEY"]);
    assert_eq!(a["leases"].as_array().unwrap().len(), 1);
    // A long-running service renews its lease instead of restarting.
    let lease = a["leases"][0].as_str().unwrap();
    let before = api.ok(Method::GET, "/leases", None).await.as_array().unwrap().iter().find(|l| l["id"] == lease).unwrap()["expires_at"].as_i64().unwrap();
    let renewed = api.ok(Method::POST, &format!("/leases/{lease}/renew"), Some(json!({ "ttl_minutes": 120 }))).await;
    assert!(renewed["expires_at"].as_i64().unwrap() > before + 60 * 60_000);
    assert_eq!(api.fake.live_keys.lock().unwrap().len(), 3);
    // A reader ends its own lease (Terraform's ephemeral reads close theirs with a read token), but
    // not another's: that would drop a running service's credential.
    // A roll-access token reading staging, as rolld's gate passes its requests on.
    let reader = {
        use roll_auth::scope::{Action as A, Effect, Resources, Rule, Scope, Selector};
        let rule = Rule {
            product: "secrets".into(),
            resources: Resources::Some(vec![Selector::Project { project: api.project.clone() }]),
            environments: Some(vec!["staging".into()]),
            paths: None,
            unprotected: false,
            actions: BTreeMap::from([(A::Read, Effect::Allow), (A::Reveal, Effect::Allow)]),
        };
        roll_auth::Grant { user: None, ask_otherwise: false, id: "tok_tf".into(), name: "tf".into(), kind: roll_auth::GrantKind::Token, scope: Arc::new(Scope(vec![rule])) }
    };
    let as_reader = |method: Method, path: String, body: Option<Value>| {
        let mut req = Request::builder().method(method).uri(format!("/v1/secrets/projects/{}{path}", api.project)).header("content-type", "application/json").body(body.map_or(Body::empty(), |b| Body::from(b.to_string()))).unwrap();
        req.extensions_mut().insert(roll_auth::Operator);
        req.extensions_mut().insert(reader.clone());
        let router = api.router.clone();
        async move {
            let resp = router.oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null))
        }
    };
    let mine = as_reader(Method::POST, "/environments/staging/resolve".into(), Some(json!({ "consumer": "app:web" }))).await.1;
    let other = b["leases"][0].as_str().unwrap();
    assert_eq!(as_reader(Method::DELETE, format!("/leases/{other}"), None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(as_reader(Method::POST, format!("/leases/{other}/renew"), None).await.0, StatusCode::FORBIDDEN, "nor keep it alive");
    assert_eq!(as_reader(Method::DELETE, format!("/leases/{}", mine["leases"][0].as_str().unwrap()), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(api.fake.live_keys.lock().unwrap().len(), 3);
    // Whoever may delete there ends any.
    api.ok(Method::DELETE, &format!("/leases/{other}"), None).await;
    assert_eq!(api.fake.live_keys.lock().unwrap().len(), 2);
    // A reader that can't renew (External Secrets Operator) asks for none: the key is left out
    // (named in `dynamic`), no lease. With `strict`, a key that doesn't resolve fails the whole
    // read, so the reader keeps what it had instead of writing an empty value.
    // A required key left out that way is still set: not a problem.
    api.ok(Method::PUT, "/required", Some(json!([{ "key": "TEMP_KEY" }]))).await;
    let pulled = api.ok(Method::GET, "/environments/staging/values?dynamic=false&strict=1", None).await;
    assert!(pulled["values"].get("TEMP_KEY").is_none() && pulled["dynamic"] == json!(["TEMP_KEY"]), "{pulled}");
    assert_eq!((pulled["leases"].as_array().map(Vec::len), api.fake.live_keys.lock().unwrap().len()), (Some(0), 2));
    // So does a service's resolve that can't renew (a preview's): no credential, no lease.
    let preview = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "preview:pr-7", "dynamic": false }))).await;
    assert!(preview["values"].get("TEMP_KEY").is_none() && preview["dynamic"] == json!(["TEMP_KEY"]) && preview["leases"] == json!([]), "{preview}");
    // Code nobody vetted never gets one, even asking: a fork's check acting for the project.
    let check = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "check:fork#1", "untrusted": true }))).await;
    assert!(check["values"].get("TEMP_KEY").is_none() && check["leases"] == json!([]), "{check}");
    // Nor through one key's reveal in an untrusted preview of staging (whoever asks).
    api.ok(Method::POST, "/environments", Some(json!({ "slug": "pr-dyn", "base": "staging" }))).await;
    let revealed = api.ok(Method::GET, "/environments/pr-dyn/secrets/TEMP_KEY/value", None).await;
    assert!(revealed["value"] == "" && revealed["problems"].to_string().contains("dynamic"), "a preview's reveal issues no dynamic credential: {revealed}");
    assert_eq!(api.fake.live_keys.lock().unwrap().len(), 2);
    api.set("staging", "BROKEN", "${NOPE}").await;
    let (s, e) = api.call(Method::GET, "/environments/staging/values?dynamic=false&strict=1", None).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("unresolved")), "{e}");
    // Dynamic keys asked for, and the read refused: what it issued is revoked, nothing outlives it.
    let (s, _) = api.call(Method::GET, "/environments/staging/values?strict=1", None).await;
    assert_eq!((s, api.fake.live_keys.lock().unwrap().len()), (StatusCode::CONFLICT, 2));
    api.ok(Method::DELETE, "/environments/staging/secrets/BROKEN", None).await;

    // An environment that goes takes its managed credentials with it.
    api.ok(Method::POST, "/environments", Some(json!({ "slug": "tmp" }))).await;
    api.ok(Method::PUT, "/environments/tmp/secrets/KEY_ID", Some(json!({ "link": link("access_key_id", "managed") }))).await;
    assert_eq!(api.fake.live_keys.lock().unwrap().len(), 3);
    api.ok(Method::DELETE, "/environments/tmp", None).await;
    assert_eq!(api.fake.live_keys.lock().unwrap().len(), 2);

    // A cache revalidates with the digest: unchanged is a 304 that decrypts and issues nothing; a
    // change reads again; and a folder with a dynamic key always reads again (a
    // credential's lease ends without moving the digest), unless dynamic keys are left out.
    let get = |env: &str, query: &str, tag: Option<String>| {
        let mut r = Request::get(format!("/v1/secrets/projects/{}/environments/{env}/values?{query}", api.project));
        if let Some(t) = tag {
            r = r.header("if-none-match", format!("\"{t}\""));
        }
        let router = api.router.clone();
        async move {
            let resp = router.oneshot(r.body(Body::empty()).unwrap()).await.unwrap();
            (resp.status(), resp.headers().get("etag").map(|v| v.to_str().unwrap().trim_matches('"').to_string()))
        }
    };
    let audited = || -> i64 { api.db.read(|c| c.query_row("SELECT COUNT(*) FROM audit WHERE project_id = ?", [api.project.as_str()], |r| r.get(0))).unwrap() };
    api.set("development", "FLAG", "off").await;
    let (s, tag) = get("development", "digest=1", None).await;
    assert!(s == StatusCode::OK && tag.is_some());
    let before = audited();
    assert_eq!(get("development", "", tag.clone()).await, (StatusCode::NOT_MODIFIED, tag.clone()));
    // A revalidation is still a read for health and insights: one row a day per caller and folder.
    assert_eq!(audited(), before + 1);
    assert_eq!(get("development", "", tag.clone()).await.0, StatusCode::NOT_MODIFIED);
    assert_eq!(audited(), before + 1, "not again the same day");
    api.set("development", "NEW_FLAG", "on").await;
    let (s, moved) = get("development", "", tag.clone()).await;
    assert!(s == StatusCode::OK && moved.is_some() && moved != tag);
    // A live link counts its value as the product answers: unchanged, a 304; changed by the product
    // itself (a password reset through it), the digest moves once the fingerprint is read again
    // (within seconds), and at once when the product says so.
    let (_, production) = get("production", "digest=1", None).await;
    assert_eq!(get("production", "", production.clone()).await.0, StatusCode::NOT_MODIFIED, "the live value is as it was");
    // A product that doesn't answer for a moment leaves the digest as it was.
    api.fake.down.store(true, std::sync::atomic::Ordering::Relaxed);
    api.secrets.forget_live_fingerprints();
    let held = api.ok(Method::GET, "/environments/production/digest", None).await;
    api.fake.down.store(false, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(held["digest"].as_str(), production.as_deref(), "a passing error moves nothing");
    *api.fake.password.lock().unwrap() += 1;
    api.secrets.forget_live_fingerprints();
    let (s, reset) = get("production", "", production.clone()).await;
    assert!(s == StatusCode::OK && reset != production, "a password reset in the product moves it");
    *api.fake.password.lock().unwrap() += 1;
    api.secrets.source_changed("bucket", "assets").unwrap();
    assert_eq!(get("production", "", reset).await.0, StatusCode::OK, "a product that says so moves it at once");
    let (_, staging) = get("staging", "dynamic=false&digest=1", None).await;
    assert_eq!(get("staging", "", staging.clone()).await.0, StatusCode::OK, "it holds a dynamic key");
    assert_eq!(get("staging", "dynamic=false", staging.clone()).await.0, StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn values_are_sealed_for_services_and_never_appear_in_the_audit_log() {
    let api = Api::new().await;
    let imported = api.ok(Method::POST, "/environments/production/import", Some(json!({ "dotenv": "# stripe\nSTRIPE_KEY=sk_live_123\nexport PORT=8080\n" }))).await;
    assert_eq!(imported["items"].as_array().unwrap().len(), 2);
    let s = api.ok(Method::GET, "/environments/production/secrets/STRIPE_KEY", None).await;
    assert_eq!(s["comment"], "stripe");
    let (_, text) = api.call(Method::GET, "/environments/production/values?format=dotenv", None).await;
    assert_eq!(text, "PORT=8080\nSTRIPE_KEY=sk_live_123\n");

    let sk = [9u8; 32];
    let r = api.ok(Method::POST, "/environments/production/resolve", Some(json!({ "consumer": "app:web", "seal_to": roll_secrets::crypto::public_key(&sk) }))).await;
    assert!(!r.to_string().contains("sk_live_123"));
    let sealed: roll_secrets::crypto::Sealed = serde_json::from_value(r["sealed"].clone()).unwrap();
    let plain = roll_secrets::crypto::open_sealed(&sk, r["context"].as_str().unwrap(), &sealed).unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&plain).unwrap()["STRIPE_KEY"], "sk_live_123");

    let audit = api.ok(Method::GET, "/audit", None).await;
    let actions: Vec<&str> = audit.as_array().unwrap().iter().map(|a| a["action"].as_str().unwrap()).collect();
    assert_eq!(&actions[..3], ["secrets.resolve", "secrets.export", "secrets.write"]);
    assert_eq!(audit[0]["summary"], "resolved 2 values for app:web");
    assert!(!audit.to_string().contains("sk_live_123"));

    // Other managers' JSON exports import too.
    let j = api.ok(Method::POST, "/environments/staging/import", Some(json!({ "json": [{ "key": "FROM_DOPPLER", "value": "d" }, { "key": "PORT", "value": 8080 }] }))).await;
    assert_eq!(j["items"].as_array().unwrap().len(), 2);
        // `replace` makes the folder match the file, deletions included, in one change.
    let r = api.ok(Method::POST, "/environments/production/import", Some(json!({ "dotenv": "STRIPE_KEY=sk_live_123\n", "replace": true }))).await;
    assert_eq!((r["items"][0]["key"].as_str(), r["items"][0]["what"][0].as_str()), (Some("PORT"), Some("deleted")));
}

#[tokio::test]
async fn nothing_reaches_an_environment_its_caller_cannot_read() {
    let api = Api::new().await;
    api.set("production", "DB_PASSWORD", "prod-pw").await;
    api.set("development", "REF", "${production.DB_PASSWORD}").await;
    api.ok(Method::PUT, "/environments/staging/imports?path=/", Some(json!([{ "environment": "production", "path": "/" }]))).await;
    api.ok(Method::POST, "/environments", Some(json!({ "slug": "pr-1", "base": "production" }))).await;
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true }))).await;

    // Checks, sandboxes and previews resolve as untrusted consumers, which may not touch protected
    // environments: not through a reference, an import or an ephemeral environment's base either.
    let body = || Some(json!({ "consumer": "check:acme/web#test", "untrusted": true }));
    let dev = api.ok(Method::POST, "/environments/development/resolve", body()).await;
    let staging = api.ok(Method::POST, "/environments/staging/resolve", body()).await;
    let preview = api.ok(Method::POST, "/environments/pr-1/resolve", body()).await;
    assert_eq!(dev["values"]["REF"], "${production.DB_PASSWORD}");
    assert_eq!(dev["problems"][0]["problem"], "production.DB_PASSWORD: no access to production");
    assert!(staging["values"].get("DB_PASSWORD").is_none() && preview["values"].get("DB_PASSWORD").is_none());
    let (s, _) = api.call(Method::POST, "/environments/production/resolve", Some(json!({ "consumer": "check:x", "untrusted": true }))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // A preview's environment is untrusted itself: whoever reads it (an app's deploy as the
    // operator here) gets nothing from a protected base, unless it was made trusted.
    assert!(api.values("pr-1", "/").await["values"].get("DB_PASSWORD").is_none());
    api.ok(Method::POST, "/environments", Some(json!({ "slug": "pr-2", "base": "production", "trusted": true }))).await;
    assert_eq!(api.values("pr-2", "/").await["values"]["DB_PASSWORD"], "prod-pw");

    // A token for development can't make development an overlay of production, nor link a
    // resource (that's an admin's), nor see the fingerprint of what it can't reveal.
    let t = api.token(json!({ "name": "dev", "environments": ["development"], "actions": ["read", "reveal", "write", "delete"] }));
    let token = Some(t.as_str());
    // The token's own scope names development: deleting it is confirmed.
    let (s, _) = api.req(Method::DELETE, "/environments/development?confirm=development", None, token).await;
    assert_eq!(s, StatusCode::OK, "kept for the window");
    let (s, _) = api.req(Method::POST, "/environments", Some(json!({ "slug": "development", "base": "production" })), token).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let link = json!({ "link": { "kind": "bucket", "ref": "assets", "field": "url", "mode": "live" } });
    let (s, _) = api.req(Method::PUT, "/environments/development/secrets/URL", Some(link), token).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let reader = api.token(json!({ "name": "read", "actions": ["read"] }));
    let (_, list) = api.req(Method::GET, "/environments/staging/secrets", None, Some(reader.as_str())).await;
    assert_eq!(list[0]["fp"], "");
}

#[tokio::test]
async fn readers_share_a_folders_first_digest_only_with_the_same_permissions() {
    let api = Api::new().await;
    api.set("production", "DB_PASSWORD", "prod-pw").await;
    api.set("staging", "APP", "a").await;
    api.ok(Method::PUT, "/environments/staging/imports?path=/", Some(json!([{ "environment": "production", "path": "/" }]))).await;
    // Staging through production's import for one, without it for the others.
    let wide = api.token(json!({ "name": "wide", "environments": ["staging", "production"], "actions": ["read", "reveal"] }));
    let narrow = api.token(json!({ "name": "narrow", "environments": ["staging"], "actions": ["read", "reveal"] }));
    let also = api.token(json!({ "name": "also", "environments": ["staging"], "actions": ["read", "reveal"] }));
    let digest = |token: String| {
        let api = &api;
        async move { api.req(Method::GET, "/environments/staging/digest", None, Some(&token)).await.1["digest"].as_str().unwrap().to_string() }
    };
    let (w, n) = (digest(wide.clone()).await, digest(narrow.clone()).await);
    assert_ne!(w, n);
    // A write elsewhere is a new state, and staging's digests come out as before: whoever computes
    // first after it, the others get their own, and the same permissions the same one.
    for round in 0..2 {
        api.set("development", "OTHER", &round.to_string()).await;
        let mut got = if round == 0 { vec![digest(wide.clone()).await, digest(narrow.clone()).await] } else { vec![digest(narrow.clone()).await, digest(wide.clone()).await] };
        if round == 1 {
            got.reverse();
        }
        got.push(digest(also.clone()).await);
        assert_eq!(got, [w.clone(), n.clone(), n.clone()], "round {round}");
    }
}

#[tokio::test]
async fn a_stored_value_is_found_by_fingerprint_and_short_values_never_are() {
    let api = Api::new().await;
    api.set("production", "STRIPE_KEY", "sk_live_51Hx9").await;
    api.set("development", "PORT", "8080").await;
    let candidates = ["sk_live_51Hx9", "8080", "something-else"].map(String::from);
    let leaks = api.secrets.leaks(&api.project, &candidates).unwrap();
    assert_eq!(leaks, [roll_secrets::Leak { key: "STRIPE_KEY".into(), environment: "production".into(), path: "/".into() }]);
}

#[tokio::test]
async fn validity_checks_ask_each_provider_once_and_only_when_an_admin_turned_them_on() {
    let api = Api::new().await;
    let c = creds();
    let (roll_live, roll_decoy, roll_gone) = (api.gate.checksummed(false), api.gate.checksummed(true), roll_auth::credentials::new_secret("rlt_"));
    let probe = |v: &str| json!({ "value": v });
    let body = json!({ "credentials": [
        probe(&c.github_live), probe(&c.github_dead), probe(&c.github_moved), { "value": c.aws_id, "pair": c.aws_secret },
        probe(&c.stripe), probe(&c.slack), probe(&c.openai), probe(&c.anthropic), probe("not a credential at all"), probe(&c.github_live),
        probe(&roll_live), probe(&roll_decoy), probe(&roll_gone),
    ] });
    let asked = || api.sent.9.0.lock().unwrap().clone();
    // Off by default: nothing is asked.
    assert_eq!(api.ok(Method::GET, "", None).await["validity_checks"], "off");
    let v = api.ok(Method::POST, "/validity", Some(body.clone())).await;
    assert_eq!((v["validity_checks"].as_str(), v["results"].as_array().map(Vec::len)), (Some("off"), Some(0)));
    assert!(asked().is_empty());
    // Turning them on is an admin's.
    let writer = api.token(json!({ "actions": ["read", "write"] }));
    let (s, _) = api.req(Method::PATCH, "", Some(json!({ "validity_checks": "on" })), Some(&writer)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(api.ok(Method::PATCH, "", Some(json!({ "validity_checks": "on" }))).await["validity_checks"], "on");

    let v = api.ok(Method::POST, "/validity", Some(body.clone())).await;
    let r = |i: usize| (v["results"][i]["result"].as_str().unwrap(), v["results"][i]["account"].as_str(), v["results"][i]["rotator"].as_str());
    assert_eq!(r(0), ("live", Some("roll-fixture"), Some("github-app")));
    assert_eq!(r(1), ("invalid", None, None));
    assert_eq!(r(2), ("unknown", None, None), "a redirect isn't followed, so nothing is known");
    // An AWS key isn't tried (STS is what canaries alert on): its account is read from its id.
    let account = roll_auth::credentials::aws_account(&c.aws_id).unwrap();
    assert_eq!(r(3), ("unknown", Some(account.as_str()), None));
    assert_eq!([r(4).0, r(5).0, r(6).0, r(7).0], ["live"; 4]);
    assert_eq!(r(5).1, Some("T0ROLLFIX"));
    assert_eq!((r(8).0, v["results"][8]["why"].as_str()), ("unknown", Some("roll can't check this kind of credential")));
    assert_eq!(v["results"][9], v["results"][0], "a repeat is the first one's answer");
    // Every roll token gets one answer, never looked up: a real one and a decoy read the same,
    // byte for byte, but for their fingerprints.
    let same = |i: usize| {
        let mut a = v["results"][i].clone();
        a.as_object_mut().unwrap().remove("fingerprint");
        serde_json::to_vec(&a).unwrap()
    };
    assert_eq!((r(10), v["results"][10]["why"].as_str()), (("unknown", None, None), Some("a roll token: roll revokes it when it lands")));
    assert_eq!(same(10), same(11), "a real token and a decoy are told apart");
    assert_eq!(same(10), same(12));
    // One request per credential a provider was asked about, to its own host, carrying only the
    // credential: none to STS.
    let sent = asked();
    assert_eq!(sent.len(), 7, "{sent:?}");
    for (url, headers) in &sent {
        assert_ne!(url, "https://sts.amazonaws.com/");
        let allowed: &[&str] = if url == "https://api.anthropic.com/v1/models" { &["x-api-key", "anthropic-version"] } else { &["authorization", "accept"] };
        assert!(headers.iter().all(|h| allowed.contains(&h.as_str())), "{url}: {headers:?}");
    }
    // Kept: asking again asks nobody, and the answer is there by fingerprint.
    api.ok(Method::POST, "/validity", Some(body)).await;
    assert_eq!(asked().len(), 7);
    let fp = v["results"][0]["fingerprint"].as_str().unwrap();
    let kept = api.ok(Method::GET, &format!("/unstored/{fp}"), None).await;
    assert_eq!((kept["result"].as_str(), kept["kind"].as_str()), (Some("live"), Some("a GitHub token")));
    // Each check is audited with kind, fingerprint and result, never the value; roll's tokens and
    // the AWS key asked nobody, so they have none, and nothing is kept for them.
    let audit = api.ok(Method::GET, "/audit?action=secrets.validity", None).await;
    assert_eq!(audit.as_array().unwrap().len(), 7, "{audit}");
    let (s, _) = api.call(Method::GET, &format!("/unstored/{}", v["results"][11]["fingerprint"].as_str().unwrap()), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "the decoy isn't kept");
    let text = api.ok(Method::GET, "/audit", None).await.to_string() + &v.to_string();
    assert!(text.contains(fp) && text.contains("a GitHub token") && text.contains("live"));
    for value in [&c.github_live, &c.aws_id, &c.aws_secret, &c.stripe, &c.slack, &c.openai, &c.anthropic, &roll_live, &roll_decoy] {
        assert!(!text.contains(value.as_str()), "a value got out");
    }
    assert!(!audit.to_string().contains(v["results"][11]["fingerprint"].as_str().unwrap()), "the decoy");
}

#[tokio::test]
async fn an_aws_key_is_tried_only_when_the_operator_allows_and_a_canary_never() {
    let api = Api::new().await;
    let c = creds();
    api.ok(Method::PATCH, "", Some(json!({ "validity_checks": "on" }))).await;
    // A canarytokens.org account's key, with a secret next to it.
    let canary = fake_aws_key_id(52_310_077_262);
    let body = json!({ "credentials": [{ "value": canary, "pair": c.aws_secret }, { "value": c.aws_id, "pair": c.aws_secret }, { "value": c.github_live }] });
    let operator = roll_secrets::validity::Canaries::parse(&format!("github:someone, {}", roll_auth::credentials::fingerprint(&c.github_live))).unwrap();
    api.secrets.set_validity_settings(roll_secrets::validity::Settings { aws_live: true, canaries: operator });
    let v = api.ok(Method::POST, "/validity", Some(body)).await;
    let r = |i: usize| (v["results"][i]["result"].as_str().unwrap(), v["results"][i]["account"].as_str());
    assert_eq!(r(0), ("unknown", Some("052310077262")));
    assert!(v["results"][0]["why"].as_str().unwrap().contains("canary"));
    assert_eq!(r(1), ("live", Some("123456789012")), "signed with the pair next to it");
    assert_eq!(r(2).0, "unknown", "a fingerprint the operator listed is never tried");
    // Only the allowed AWS key reached STS, and nothing reached GitHub.
    let sent: Vec<String> = api.sent.9.0.lock().unwrap().iter().map(|(u, _)| u.clone()).collect();
    assert_eq!(sent, ["https://sts.amazonaws.com/"]);
    assert!(roll_secrets::validity::Canaries::parse("aws").is_err() && roll_secrets::validity::Canaries::parse("sha256:short").is_err());
}

/// An AWS key id carrying `account` (the encoding `aws_account` reads), built here: no real key's
/// shape is in the repo.
fn fake_aws_key_id(account: u64) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let bits = (account << 7) << 2;
    let head: String = (0..10).rev().map(|i| ALPHABET[((bits >> (i * 5)) & 31) as usize] as char).collect();
    format!("AKIA{head}ROLLFX")
}

#[tokio::test]
async fn a_shared_value_opens_as_many_times_as_allowed_and_the_server_never_holds_it() {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    let api = Api::new().await;
    // The client encrypts; the key would be in the link's #fragment.
    let (key, nonce) = ([3u8; 32], [5u8; 12]);
    let cipher = aes_gcm::Aes256Gcm::new_from_slice(&key).unwrap();
    let ct = cipher.encrypt(&nonce.into(), Payload { msg: b"hunter2-staging-db", aad: b"roll-share-v1" }).unwrap();
    let blob = B64.encode([&nonce[..], &ct].concat());
    let share = api.ok(Method::POST, "/shares", Some(json!({ "ciphertext": blob, "note": "the staging db", "views": 2, "ttl_minutes": 60 }))).await;
    let id = share["id"].as_str().unwrap();

    // Looking at the link uses nothing up; each open does, and the last one drops it.
    let (_, peek) = api.call(Method::GET, &format!("/v1/secrets/shares/{id}"), None).await;
    assert_eq!((peek["note"].as_str(), peek["views_left"].as_i64()), (Some("the staging db"), Some(2)));
    for _ in 0..2 {
        let (s, opened) = api.call(Method::POST, &format!("/v1/secrets/shares/{id}/open"), None).await;
        assert_eq!(s, StatusCode::OK);
        let raw = B64.decode(opened["ciphertext"].as_str().unwrap()).unwrap();
        let plain = cipher.decrypt(raw[..12].into(), Payload { msg: &raw[12..], aad: b"roll-share-v1" }).unwrap();
        assert_eq!(plain, b"hunter2-staging-db");
    }
    let (s, _) = api.call(Method::POST, &format!("/v1/secrets/shares/{id}/open"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let list = api.ok(Method::GET, "/shares", None).await;
    assert_eq!((list[0]["open"].as_bool(), list[0]["views_left"].as_i64()), (Some(false), Some(0)));
    let audit = api.ok(Method::GET, "/audit?action=share", None).await;
    assert_eq!(audit.as_array().unwrap().len(), 3, "created and opened twice");
}

/// docs/accounts.md, "Offboarding": a disabled person (an admin, SCIM) keeps their memberships,
/// but what they handed out ends anyway. Their one-time share refuses to open (it checks its
/// maker), and rolld's offboarding hook revokes their dynamic credential now, not at its TTL,
/// checking only the projects holding what they made: never root's unrelated project with root's
/// own open share. Enabled again, they get nothing back.
#[tokio::test]
async fn a_disabled_persons_share_refuses_to_open_and_their_leases_are_revoked() {
    use base64::Engine as _;
    let api = Api::new().await;
    let p = api.project.clone();
    let (_, inv) = api.call(Method::POST, &format!("/v1/workspace/projects/{p}/invites"), Some(json!({ "email": "ada@example.com", "role": "admin" }))).await;
    let token = inv["url"].as_str().unwrap().split("token=").nth(1).unwrap().to_string();
    assert_eq!(api.as_ada(Method::POST, &format!("/v1/workspace/invites/{token}/accept"), None).await.0, StatusCode::OK);
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{p}/resources/bucket/assets"), Some(json!({ "x": 0, "y": 0 }))).await;
    api.ok(Method::PUT, "/environments/staging/secrets/TEMP_KEY", Some(json!({ "link": { "kind": "bucket", "ref": "assets", "field": "access_key_id", "mode": "dynamic" } }))).await;
    let at = |rest: &str| format!("/v1/secrets/projects/{p}{rest}");
    let (s, got) = api.as_ada(Method::POST, &at("/environments/staging/resolve"), Some(json!({ "consumer": "app:laptop" }))).await;
    assert_eq!(s, StatusCode::OK, "{got}");
    let key = got["values"]["TEMP_KEY"].as_str().unwrap().to_string();
    let blob = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7u8; 40]);
    let (s, share) = api.as_ada(Method::POST, &at("/shares"), Some(json!({ "ciphertext": blob, "note": "for the contractor" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{share}");
    let open = format!("/v1/secrets/shares/{}/open", share["id"].as_str().unwrap());
    let (_, unrelated) = api.signed_in("root", true, Method::POST, "/v1/workspace/projects", Some(json!({ "name": "root-shop" }))).await;
    let (s, e) = api.signed_in("root", true, Method::POST, &format!("/v1/secrets/projects/{}/shares", unrelated["id"].as_str().unwrap()), Some(json!({ "ciphertext": blob }))).await;
    assert_eq!(s, StatusCode::CREATED, "{e}");

    *api.people.disabled.lock().unwrap() = true;
    assert_eq!(api.call(Method::POST, &open, None).await.0, StatusCode::NOT_FOUND, "a disabled maker's share ends unopened");
    assert!(api.fake.live_keys.lock().unwrap().contains(&key), "until the hook runs, the credential is live");
    let rechecks = api.secrets.metrics().unwrap().member_rechecks;
    api.secrets.person_offboarded("usr_ada").unwrap();
    assert_eq!(api.secrets.metrics().unwrap().member_rechecks, rechecks + 1, "only her own project is checked, not root's");
    assert!(!api.fake.live_keys.lock().unwrap().contains(&key), "revoked now, not at its TTL");
    let leases = api.ok(Method::GET, "/leases", None).await;
    assert!(leases.as_array().unwrap().iter().all(|l| !l["revoked_at"].is_null()), "{leases}");

    *api.people.disabled.lock().unwrap() = false;
    api.secrets.person_offboarded("usr_ada").unwrap();
    assert_eq!(api.call(Method::POST, &open, None).await.0, StatusCode::NOT_FOUND, "nothing comes back");
    assert!(!api.fake.live_keys.lock().unwrap().contains(&key));
}

#[tokio::test]
async fn a_sync_pushes_each_change_once_removes_deleted_keys_and_is_audited() {
    let api = Api::new().await;
    api.set("staging", "DB_URL", "postgres://db").await;
    api.set("staging", "STRIPE_KEY", "sk_live_123").await;
    api.set("staging", "PLATFORM_TOKEN", "tok_platform_1").await;
    // Before it exists: what it would push (names only), its credential kept out.
    let p = api.ok(Method::POST, "/syncs/preview", Some(json!({ "environment": "staging", "kind": "fake", "target": { "name": "x", "token_key": "PLATFORM_TOKEN" }, "keys": ["DB_URL", "STRIPE_KEY", "TYPO"] }))).await;
    assert_eq!(p, json!({ "names": ["DB_URL", "STRIPE_KEY"], "credentials": ["PLATFORM_TOKEN"], "missing_credentials": [], "unknown_keys": ["TYPO"] }));
    // A name the platform keeps for itself (Supabase's SUPABASE_) is refused when it's made.
    let (s, e) = api.call(Method::POST, "/syncs", Some(json!({ "environment": "staging", "kind": "fake", "prefix": "FAKE_", "target": { "name": "x", "token_key": "PLATFORM_TOKEN" } }))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("FAKE_DB_URL: FAKE_ is the platform's")));
    let s = api.ok(Method::POST, "/syncs", Some(json!({ "environment": "staging", "kind": "fake", "target": { "name": "x", "token_key": "PLATFORM_TOKEN" } }))).await;
    let id = s["id"].as_str().unwrap().to_string();
    assert!(s["signing_secret"].as_str().unwrap().starts_with("whsec_"), "shown once: {s}");
    let pushes = |n: usize| api.sync.pushes.lock().unwrap()[n].clone();

    api.secrets.sweep().unwrap();
    let (values, changed, removed) = pushes(0);
    assert_eq!(values["STRIPE_KEY"], "sk_live_123");
    assert_eq!(values["credential:PLATFORM_TOKEN"], "tok_platform_1", "the platform's token, resolved at push time");
    assert!(!values.contains_key("PLATFORM_TOKEN"), "and never pushed itself");
    assert_eq!((changed, removed.len()), (vec!["DB_URL".to_string(), "STRIPE_KEY".into()], 0));
    api.secrets.sweep().unwrap();
    assert_eq!(api.sync.pushes.lock().unwrap().len(), 1, "an unchanged environment isn't pushed again");
    assert_eq!(api.secrets.sweep_due_syncs().unwrap(), (0, 0), "nothing changed: nothing is due");

    // A change marks the project's syncs due: the pass rolld runs every couple of seconds pushes it,
    // without waiting for the sweep.
    api.set("staging", "DB_URL", "postgres://db2").await;
    api.ok(Method::DELETE, "/environments/staging/secrets/STRIPE_KEY", None).await;
    assert_eq!(api.secrets.sweep_due_syncs().unwrap(), (1, 0));
    let (values, changed, removed) = pushes(1);
    assert_eq!((values.len(), changed, removed), (2, vec!["DB_URL".to_string()], vec!["STRIPE_KEY".to_string()]));

    let listed = api.ok(Method::GET, "/syncs", None).await;
    assert_eq!((listed["syncs"][0]["status"].as_str(), listed["syncs"][0]["synced"].as_u64()), (Some("ok"), Some(1)));
    assert!(listed["syncs"][0].get("signing_secret").is_none());
    let audit = api.ok(Method::GET, &format!("/audit?actor=sync:{id}"), None).await;
    assert_eq!(audit[0]["action"], "sync.push");
    assert_eq!(audit[0]["keys"], json!(["DB_URL", "STRIPE_KEY"]));
    assert_eq!(audit.as_array().unwrap().len(), 2);
    assert!(!audit.to_string().contains("sk_live_123") && !audit.to_string().contains("postgres://"));

    // A change that only comes through a reference is pushed too.
    api.set("production", "HOST", "db.one").await;
    api.set("staging", "REF", "${production.HOST}/x").await;
    api.secrets.sweep().unwrap();
    api.set("production", "HOST", "db.two").await;
    api.secrets.sweep().unwrap();
    let (values, changed, _) = pushes(3);
    assert_eq!((values["REF"].as_str(), changed), ("db.two/x", vec!["REF".to_string()]));
}

#[tokio::test]
async fn a_certificate_sync_pushes_a_pki_keys_certificate_on_issue_and_each_rotation_to_the_same_target() {
    let api = Api::new().await;
    api.secrets.set_public_api("https://roll.example".into());
    // It signs in as a project AWS role, assumed for each push.
    *api.sts.pinned.lock().unwrap() = Some(format!("roll-secrets:{}", api.project));
    api.ok(Method::POST, "/aws-roles", Some(json!({ "name": "certs", "role_arn": "arn:aws:iam::123456789012:role/roll" }))).await;
    let ca = api.ok(Method::POST, "/cas", Some(json!({ "name": "internal", "domains": ["svc.internal"] }))).await;
    let link = json!({ "kind": "pki", "ref": ca["id"], "field": "certificate", "mode": "managed", "options": { "names": "api.svc.internal", "rotate_days": 7 } });
    api.ok(Method::PUT, "/environments/production/secrets/TLS_CERT", Some(json!({ "link": link }))).await;
    api.set("production", "PLAIN", "fake-plain-value").await;
    // Only one managed pki key of its own folder, never in an untrusted environment.
    let sync = |env: &str, keys: Value| json!({ "environment": env, "kind": "fake-cert", "target": { "role": "certs" }, "keys": keys });
    for keys in [json!(["PLAIN"]), json!(["TLS_CERT", "PLAIN"]), Value::Null] {
        let (s, e) = api.call(Method::POST, "/syncs", Some(sync("production", keys))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    }
    api.ok(Method::POST, "/environments", Some(json!({ "slug": "pr-1", "base": "production" }))).await;
    let (s, e) = api.call(Method::POST, "/syncs", Some(sync("pr-1", json!(["TLS_CERT"])))).await;
    assert!(s == StatusCode::BAD_REQUEST && e["error"].as_str().unwrap().contains("untrusted"), "{e}");
    let made = api.ok(Method::POST, "/syncs", Some(sync("production", json!(["TLS_CERT"])))).await;
    let id = made["id"].as_str().unwrap().to_string();

    // On issue: the certificate the key holds, its private key and chain.
    assert_eq!(api.secrets.sweep_due_syncs().unwrap(), (1, 0));
    let pushes = || api.certs.pushes.lock().unwrap().clone();
    let first = pushes()[0].clone();
    assert_eq!(first.0, api.values("production", "/").await["values"]["TLS_CERT"].as_str().unwrap());
    assert!(first.1.starts_with("-----BEGIN PRIVATE KEY-----") && first.2 == ca["certificate"].as_str().unwrap(), "{first:?}");
    assert_eq!((first.3.as_deref(), first.4.as_deref().is_some_and(|k| k.starts_with("ASIA"))), (None, true));
    assert_eq!(api.ok(Method::GET, "/syncs", None).await["syncs"][0]["remote_id"], "arn:fake:1");
    api.secrets.sweep().unwrap();
    assert_eq!(pushes().len(), 1, "an unchanged certificate isn't pushed again");

    // On rotation: the new certificate, to the same target.
    api.ok(Method::POST, "/environments/production/secrets/TLS_CERT/rotate", None).await;
    assert_eq!(api.secrets.sweep_due_syncs().unwrap(), (1, 0));
    let second = pushes()[1].clone();
    assert!(second.0 != first.0 && second.1 != first.1);
    assert_eq!(second.3.as_deref(), Some("arn:fake:1"), "replacing what the first push made");

    // The private keys reached only the platform: not the audit log, not the events.
    let audit = api.ok(Method::GET, &format!("/audit?actor=sync:{id}"), None).await;
    assert_eq!((audit[0]["action"].as_str(), audit[0]["keys"].clone()), (Some("sync.push"), json!(["TLS_CERT"])));
    let everything = format!("{}{}", api.ok(Method::GET, "/audit", None).await, api.ok(Method::GET, "/events", None).await);
    for key in [&first.1, &second.1] {
        let line = key.lines().nth(1).unwrap();
        assert!(!everything.contains(line), "a private key in the audit or events");
    }
}

#[tokio::test]
async fn the_audit_log_exports_and_streams_to_a_sink_without_values() {
    let api = Api::new().await;
    api.set("production", "BEFORE", "x-secret-before").await;
    let (s, _) = api.call(Method::POST, "/audit/sinks", Some(json!({ "url": "http://10.0.0.1/hook" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let sink = api.ok(Method::POST, "/audit/sinks", Some(json!({ "url": "https://siem.example/roll" }))).await;
    assert!(sink["secret"].as_str().unwrap().starts_with("whsec_"));
    // From now on: rows after the sink was made, once each, in order.
    api.set("production", "STRIPE_KEY", "sk_live_never_sent").await;
    api.values("production", "/").await;
    // Rows go out once they've settled (a late commit can't be skipped).
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap();
    assert!(api.sent.0.lock().unwrap().is_empty());
    api.db.write(|c| c.execute("UPDATE audit SET at = at - 60000", ())).unwrap();
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || {
        s.sweep().unwrap();
        s.sweep().unwrap();
    })
    .await
    .unwrap();
    let sent = api.sent.0.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "one batch, not delivered twice");
    let actions: Vec<&str> = sent[0]["events"].as_array().unwrap().iter().map(|e| e["action"].as_str().unwrap()).collect();
    // The sweep's signed digest of the log goes out as its own event.
    assert_eq!(actions, ["sink.create", "secrets.write", "secrets.export", "audit.digest"]);
    assert!(sent[0]["events"][3]["digest"].as_str().is_some_and(|d| d.split('.').count() == 3));
    assert!(!serde_json::to_string(&sent).unwrap().contains("sk_live_never_sent"));
    // A SIEM's own format with its token, sealed: never shown again, sent only with the batch.
    assert_eq!(api.call(Method::POST, "/audit/sinks", Some(json!({ "url": "https://splunk.example:8088/services/collector/event", "format": "splunk" }))).await.0, StatusCode::BAD_REQUEST, "a token is needed");
    let splunk = api.ok(Method::POST, "/audit/sinks", Some(json!({ "url": "https://splunk.example:8088/services/collector/event", "format": "splunk", "token": "hec-token-1" }))).await;
    assert_eq!(splunk["format"], "splunk");
    assert!(!api.ok(Method::GET, "/audit/sinks", None).await.to_string().contains("hec-token-1") && !splunk.to_string().contains("hec-token-1"));
    api.set("production", "AFTER", "x-after").await;
    api.db.write(|c| c.execute("UPDATE audit SET at = at - 60000", ())).unwrap();
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap();
    let sent = api.sent.0.lock().unwrap().clone();
    let to_splunk = sent.iter().find(|b| b["format"] == "splunk").expect("a splunk batch");
    assert_eq!(to_splunk["token"], "hec-token-1");
    assert!(to_splunk["events"].as_array().unwrap().iter().any(|e| e["action"] == "secrets.write"));
    // Or a roll bucket: of the project's own canvas only.
    let (s, _) = api.call(Method::POST, "/audit/sinks", Some(json!({ "url": "bucket:audit-archive" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let p = api.project.clone();
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{p}/resources/bucket/audit-archive"), Some(json!({ "x": 0, "y": 0 }))).await;
    let bucket = api.ok(Method::POST, "/audit/sinks", Some(json!({ "url": "bucket:audit-archive" }))).await;
    assert_eq!(bucket["url"], "bucket:audit-archive");
    // A topic of a stream on the canvas, the same way.
    let (s, _) = api.call(Method::POST, "/audit/sinks", Some(json!({ "url": "stream:st_x/audit" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{p}/resources/stream/st_x"), Some(json!({ "x": 0, "y": 0 }))).await;
    let (s, _) = api.call(Method::POST, "/audit/sinks", Some(json!({ "url": "stream:st_x/bad topic" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    api.ok(Method::POST, "/audit/sinks", Some(json!({ "url": "stream:st_x/audit" }))).await;
    // An index of a search cluster on the canvas, the same way (and a name Elasticsearch takes).
    assert_eq!(api.call(Method::POST, "/audit/sinks", Some(json!({ "url": "search:cl_x/roll-audit" }))).await.0, StatusCode::BAD_REQUEST);
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{p}/resources/search/cl_x"), Some(json!({ "x": 0, "y": 0 }))).await;
    assert_eq!(api.call(Method::POST, "/audit/sinks", Some(json!({ "url": "search:cl_x/Roll Audit" }))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(api.call(Method::POST, "/audit/sinks", Some(json!({ "url": "search:cl_x/.security" }))).await.0, StatusCode::BAD_REQUEST, "a system index");
    api.ok(Method::POST, "/audit/sinks", Some(json!({ "url": "search:cl_x/roll-audit" }))).await;
    // Taken off the canvas later, it gets nothing more: checked at every delivery.
    api.ok(Method::DELETE, &format!("/v1/workspace/projects/{p}/resources/bucket/audit-archive"), None).await;
    api.set("production", "AFTER", "x").await;
    api.db.write(|c| c.execute("UPDATE audit SET at = at - 60000", ())).unwrap();
    let before = api.sent.0.lock().unwrap().len();
    let sw = api.secrets.clone();
    tokio::task::spawn_blocking(move || sw.sweep().unwrap()).await.unwrap();
    let sinks = api.ok(Method::GET, "/audit/sinks", None).await;
    let b = sinks.as_array().unwrap().iter().find(|x| x["id"] == bucket["id"]).unwrap();
    assert_eq!((b["status"].as_str(), b["error"].as_str().is_some_and(|e| e.contains("isn't on this project's canvas"))), (Some("error"), true), "{b}");
    assert_eq!(api.sent.0.lock().unwrap().iter().skip(before).filter(|x| x["sink"] == bucket["id"]).count(), 0);

    let (_, csv) = api.call(Method::GET, "/audit?format=csv", None).await;
    let csv = csv.as_str().unwrap();
    assert!(csv.starts_with("id,at,actor,action,") && csv.contains("\"secrets.export\"") && !csv.contains("sk_live"));
}

#[tokio::test]
async fn the_audit_log_is_a_signed_chain_that_shows_an_edit_and_a_deletion_and_starts_again_at_retentions_anchor() {
    let api = Api::new().await;
    for k in ["A", "B", "C", "D"] {
        api.set("production", k, "v").await;
    }
    api.values("production", "/").await;
    let sweep = |keep_ms: i64| {
        let s = api.secrets.clone();
        async move { tokio::task::spawn_blocking(move || s.sweep_audit_chain(keep_ms).unwrap()).await.unwrap() }
    };
    assert_eq!(sweep(i64::MAX / 2).await, 1, "a digest of the chain so far");
    let v = api.ok(Method::GET, "/audit/verify", None).await;
    assert_eq!((v["ok"].as_bool(), v["from"].as_i64(), v["through"].as_i64(), v["head"].as_i64()), (Some(true), Some(1), Some(5), Some(6)), "{v}");

    // A row edited in the database, and put back.
    let p = api.project.clone();
    let summary = |c: &code_sql::Conn<'_>| c.query_row("SELECT summary FROM audit WHERE project_id = ? AND seq = 3", [p.as_str()], |r| r.get::<_, String>(0));
    let original = api.db.read(summary).unwrap();
    let edit = |to: &str| api.db.write(|c| c.execute("UPDATE audit SET summary = ? WHERE project_id = ? AND seq = 3", [to, p.as_str()])).unwrap();
    edit("nothing happened");
    let v = api.ok(Method::GET, "/audit/verify", None).await;
    assert_eq!((v["ok"].as_bool(), v["broken"]["seq"].as_i64(), v["broken"]["reason"].as_str()), (Some(false), Some(3), Some("edited")), "{v}");
    edit(&original);
    assert_eq!(api.ok(Method::GET, "/audit/verify", None).await["ok"], true);
    // A row deleted: a gap.
    api.db.write(|c| c.execute("DELETE FROM audit WHERE project_id = ? AND seq = 4", [p.as_str()])).unwrap();
    let v = api.ok(Method::GET, "/audit/verify", None).await;
    assert_eq!((v["broken"]["seq"].as_i64(), v["broken"]["reason"].as_str()), (Some(4), Some("missing")), "{v}");

    // Retention cuts at the digest, which the log then starts from; the cut's own digest says so.
    api.set("production", "E", "v").await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    assert_eq!(sweep(0).await, 1);
    let v = api.ok(Method::GET, "/audit/verify", None).await;
    assert_eq!((v["ok"].as_bool(), v["from"].as_i64(), v["through"].as_i64(), v["digests"].as_i64()), (Some(true), Some(6), Some(7), Some(2)), "{v}");

    // Rows cut from the end and the head put back on the digest before them: the head's MAC needs
    // the deployment key.
    api.set("production", "F", "v").await;
    api.db.write(|c| c.execute("DELETE FROM audit WHERE project_id = ? AND seq = 9", [p.as_str()])).unwrap();
    api.db.write(|c| c.execute("UPDATE audit_heads SET seq = 8, hash = (SELECT hash FROM audit WHERE project_id = ? AND seq = 8) WHERE project_id = ?", [p.as_str(), p.as_str()])).unwrap();
    let v = api.ok(Method::GET, "/audit/verify", None).await;
    assert_eq!((v["ok"].as_bool(), v["broken"]["seq"].as_i64(), v["broken"]["reason"].as_str()), (Some(false), Some(8), Some("head")), "{v}");
}

#[tokio::test]
async fn an_old_head_put_back_over_cut_rows_leaves_a_gap_that_verify_reports_from_then_on() {
    let api = Api::new().await;
    api.set("production", "A", "v").await;
    let (now, hour) = (api.db.now_ms().unwrap(), 3_600_000);
    let sweep = |at: i64| {
        let s = api.secrets.clone();
        async move { tokio::task::spawn_blocking(move || s.sweep_audit_chain_at(i64::MAX / 2, at).unwrap()).await.unwrap() }
    };
    assert_eq!(sweep(now).await, 1);
    // A database writer copies the head...
    let p = api.project.clone();
    type Head = (i64, String, i64, i64, i64, i64, Option<String>);
    let copy: Head = api.db.read(|c| c.query_row("SELECT seq, hash, at, digest_seq, digest_at, start_seq, mac FROM audit_heads WHERE project_id = ?", [p.as_str()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))).unwrap();
    // ...roll writes more and signs it an hour later...
    api.set("production", "B", "v").await;
    assert_eq!(sweep(now + hour + 1).await, 1);
    api.set("production", "C", "v").await;
    // ...and they delete all of it, the digest too, and put the copy back. Its MAC holds.
    api.db.write(|c| c.execute("DELETE FROM audit WHERE project_id = ? AND seq > ?", code_sql::params![p, copy.0])).unwrap();
    let (seq, hash, at, digest_seq, digest_at, start, mac) = copy.clone();
    api.db.write(|c| c.execute("UPDATE audit_heads SET seq = ?, hash = ?, at = ?, digest_seq = ?, digest_at = ?, start_seq = ?, mac = ? WHERE project_id = ?", code_sql::params![seq, hash, at, digest_seq, digest_at, start, mac, p])).unwrap();
    assert_eq!(api.ok(Method::GET, "/audit/verify", None).await["ok"], true, "the copy passes until roll signs again");

    // Roll's next digest finds its newest one too old to vouch for what's after it, and says so;
    // verify reports the gap through later hours and later digests.
    assert_eq!(sweep(now + 3 * hour).await, 1);
    for later in [5, 9, 30] {
        let v = api.ok(Method::GET, "/audit/verify", None).await;
        assert_eq!((v["ok"].as_bool(), v["broken"]["reason"].as_str()), (Some(false), Some("gap")), "{v}");
        assert!(v["broken"]["seq"].as_i64().is_some_and(|s| s > copy.0), "{v}");
        assert_eq!(sweep(now + later * hour).await, 1);
    }
    assert_eq!(api.ok(Method::GET, "/audit/verify", None).await["broken"]["reason"], "gap");
}

#[tokio::test]
async fn a_signing_key_added_to_the_database_vouches_for_no_digest_and_isnt_published() {
    use base64::Engine as _;
    use ring::signature::KeyPair as _;
    use roll_secrets::chain;
    let api = Api::new().await;
    api.set("production", "A", "v").await;
    api.set("production", "B", "v").await;
    sweep_chain(&api).await;

    // A database writer adds a digest key of their own, older than roll's so roll's still signs...
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let der = ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
    let pair = ring::signature::Ed25519KeyPair::from_pkcs8(der.as_ref()).unwrap();
    let x = b64(pair.public_key().as_ref());
    api.db.write(|c| c.execute("INSERT INTO audit_keys (kid, sealed_dek, private, x, legacy, created_at) VALUES ('ak_theirs', 'v1:AAAA', ?, ?, '', 0)", code_sql::params![vec![0u8], x])).unwrap();
    // ...edits row #1, recomputes every hash after it and signs the digest again with that key.
    let p = api.project.clone();
    let mut rows: Vec<roll_secrets::AuditEntry> = serde_json::from_value(api.ok(Method::GET, "/audit", None).await).unwrap();
    rows.sort_by_key(|r| r.seq);
    let signed = rows.iter().find(|r| r.action == chain::DIGEST).and_then(|r| r.seq);
    let mut prev = String::new();
    for r in &mut rows {
        if r.seq == Some(1) {
            r.summary = "nothing happened".into();
        }
        if r.action == chain::DIGEST {
            let d = chain::Digest { project: p.clone(), seq: r.seq.unwrap() - 1, hash: prev.clone(), last_at: r.at, at: r.at, start: 1, gap: None };
            r.digest = Some(chain::sign("ak_theirs", &pair, &d));
        }
        prev = chain::link(&prev, &p, r);
        let (seq, hash, digest) = (r.seq.unwrap(), prev.clone(), r.digest.clone());
        let summary = r.summary.clone();
        api.db.write(|c| c.execute("UPDATE audit SET summary = ?, hash = ?, digest = ? WHERE project_id = ? AND seq = ?", code_sql::params![summary, hash, digest, p, seq])).unwrap();
    }
    let v = api.ok(Method::GET, "/audit/verify", None).await;
    assert_eq!((v["ok"].as_bool(), v["broken"]["seq"].as_i64(), v["broken"]["reason"].as_str()), (Some(false), signed, Some("signature")), "{v}");
    // Nor is it published: the digest JWKS has roll's key alone.
    let jwks = api.ok(Method::GET, "/v1/secrets/audit/jwks", None).await;
    assert!(kids(&jwks).len() == 1 && kids(&jwks)[0] != "ak_theirs", "{jwks}");
}

/// Sign every chain that's due (the sweeper's hourly digests).
async fn sweep_chain(api: &Api) -> usize {
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep_audit_chain(i64::MAX / 2).unwrap()).await.unwrap()
}

/// The kids of a JWKS's `keys`.
fn kids(jwks: &Value) -> Vec<String> {
    jwks["keys"].as_array().unwrap().iter().map(|k| k["kid"].as_str().unwrap().to_string()).collect()
}

/// A JWS's header.
fn jws_header(jws: &str) -> Value {
    use base64::Engine as _;
    serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(jws.split('.').next().unwrap()).unwrap()).unwrap()
}

/// The project's whole log, as an export has it, walked with nothing but `jwks` (as `audit verify
/// --file` does).
async fn verify_offline(api: &Api, jwks: &[Value]) -> roll_secrets::Verification {
    let mut rows: Vec<roll_secrets::AuditEntry> = serde_json::from_value(api.ok(Method::GET, "/audit?limit=10000", None).await).unwrap();
    rows.sort_by_key(|r| r.seq);
    walk_offline(&api.project, &rows, jwks)
}

fn walk_offline(project: &str, rows: &[roll_secrets::AuditEntry], jwks: &[Value]) -> roll_secrets::Verification {
    let keys = roll_secrets::chain::Keys::from_jwks(jwks);
    let mut walk = roll_secrets::chain::Walk::new(project, false);
    for r in rows {
        if !walk.push(r, &keys) {
            break;
        }
    }
    walk.finish()
}

/// Digests are signed by a key of their own (EdDSA), never the issuer's, and an export checks
/// against the digest JWKS saved earlier; the issuer's JWKS vouches for none of them.
#[tokio::test]
async fn new_digests_carry_the_digest_keys_kid_and_verify_offline_against_its_saved_jwks() {
    let api = Api::new().await;
    api.set("production", "A", "v").await;
    assert_eq!(sweep_chain(&api).await, 1);
    let rows = api.ok(Method::GET, "/audit", None).await;
    let digest = rows.as_array().unwrap().iter().find(|r| r["action"] == "audit.digest").unwrap()["digest"].as_str().unwrap().to_string();
    let header = jws_header(&digest);
    let saved = api.ok(Method::GET, "/v1/secrets/audit/jwks", None).await;
    let issuer = api.ok(Method::GET, "/v1/secrets/oidc/jwks", None).await;
    assert_eq!((header["alg"].as_str(), header["kid"].as_str().map(String::from)), (Some("EdDSA"), kids(&saved).first().cloned()), "{header} {saved}");
    assert!(saved["keys"][0]["kty"] == "OKP" && !kids(&issuer).contains(&kids(&saved)[0]), "the digest key is no issuer key: {issuer}");
    let v = api.ok(Method::GET, "/audit/verify", None).await;
    assert_eq!((v["ok"].as_bool(), v["through"].as_i64(), v.get("legacy"), v.get("pulled")), (Some(true), Some(1), None, None), "{v}");
    api.set("production", "B", "v").await;
    let offline = verify_offline(&api, &[saved]).await;
    assert!(offline.ok && offline.through == Some(1) && offline.head == Some(3), "{offline:?}");
    let issuers_only = verify_offline(&api, &[issuer]).await;
    assert_eq!(issuers_only.broken.map(|b| b.reason).as_deref(), Some("signature"));
}

/// A digest key that may have leaked is pulled: it vouches for nothing again, every log is signed
/// under the new key at once (which covers the pulled key's digests), an export taken before that
/// fails offline, and a database writer can't trust it back.
#[tokio::test]
async fn after_an_emergency_rotation_of_the_digest_key_its_digests_are_reported_and_need_a_later_good_digest() {
    let api = Api::new().await;
    api.set("production", "A", "v").await;
    sweep_chain(&api).await;
    let old = kids(&api.ok(Method::GET, "/v1/secrets/audit/jwks", None).await)[0].clone();
    let mut before: Vec<roll_secrets::AuditEntry> = serde_json::from_value(api.ok(Method::GET, "/audit", None).await).unwrap();
    before.sort_by_key(|r| r.seq);
    let rotate = "/v1/secrets/audit/key/rotate";
    for (body, status) in [(json!({ "now": true }), StatusCode::BAD_REQUEST), (json!({ "now": true, "retire": "ak_nope" }), StatusCode::NOT_FOUND)] {
        assert_eq!(api.call(Method::POST, rotate, Some(body)).await.0, status);
    }
    let r = api.ok(Method::POST, rotate, Some(json!({ "now": true, "retire": old }))).await;
    let new = r["kid"].as_str().unwrap().to_string();
    assert!(new != old && r["signed"] == 1, "every log signed under the new key at once: {r}");
    let jwks = api.ok(Method::GET, "/v1/secrets/audit/jwks", None).await;
    assert_eq!((kids(&jwks), jwks["pulled"][0]["kid"].as_str()), (vec![new.clone()], Some(old.as_str())), "{jwks}");
    // Online: the pulled key's digest is reported, covered by the next one under the new key.
    let v = api.ok(Method::GET, "/audit/verify", None).await;
    let p = &v["pulled"][0];
    assert_eq!((v["ok"].as_bool(), p["kid"].as_str(), p["digests"].as_i64(), p["countersigned"].as_i64()), (Some(true), Some(old.as_str()), Some(1), v["through"].as_i64().map(|t| t + 1)), "{v}");
    let audit = api.ok(Method::GET, "/audit", None).await;
    assert!(audit.as_array().unwrap().iter().any(|a| a["action"] == "audit.key.rotate" && a["actor"] == "operator"), "{audit}");
    // Offline: an export from before the rotation has no later good digest, so it doesn't verify.
    let stale = walk_offline(&api.project, &before, std::slice::from_ref(&jwks));
    assert_eq!(stale.broken.map(|b| b.reason).as_deref(), Some("pulled"), "{:?}", stale.pulled);
    assert!(verify_offline(&api, &[jwks]).await.ok);
    // Its private half is gone, so un-pulling the row in the database makes it nobody's key.
    let o = old.clone();
    api.db.write(move |c| c.execute("UPDATE audit_keys SET pulled_at = NULL, retired_at = NULL WHERE kid = ?", [o.as_str()])).unwrap();
    let v = api.ok(Method::GET, "/audit/verify", None).await;
    assert_eq!((v["ok"].as_bool(), v["broken"]["reason"].as_str()), (Some(false), Some("signature")), "{v}");
}

/// Digests an older roll signed with the issuer key still verify, marked legacy, against the
/// digest JWKS alone; the issuer key signs no digest after the digest key's first, and pulling the
/// issuer key leaves the log verified.
#[tokio::test]
async fn legacy_digests_still_verify_and_the_issuers_emergency_rotation_leaves_digests_alone() {
    let api = Api::new().await;
    // Before the upgrade: no digest key, and the issuer key signed the digests.
    api.db.write(|c| c.execute("DELETE FROM audit_keys", ())).unwrap();
    api.set("production", "A", "v").await;
    let s = api.secrets.clone();
    assert_eq!(tokio::task::spawn_blocking(move || s.sweep_audit_chain_as_before_the_digest_key(i64::MAX / 2).unwrap()).await.unwrap(), 1);
    let issuer = kids(&api.ok(Method::GET, "/v1/secrets/oidc/jwks", None).await)[0].clone();
    // The upgrade: the next digest is the digest key's, from the first hour.
    api.db.write(|c| c.execute("UPDATE audit_heads SET digest_at = 0", ())).unwrap();
    api.set("production", "B", "v").await;
    assert_eq!(sweep_chain(&api).await, 1);
    let jwks = api.ok(Method::GET, "/v1/secrets/audit/jwks", None).await;
    assert_eq!(jwks["legacy"][0]["kid"].as_str(), Some(issuer.as_str()), "{jwks}");
    let v = api.ok(Method::GET, "/audit/verify", None).await;
    assert_eq!((v["ok"].as_bool(), v["digests"].as_i64(), v["legacy"].clone()), (Some(true), Some(2), json!({ "signer": "issuer key", "digests": 1, "through": 1 })), "{v}");
    assert!(verify_offline(&api, &[jwks]).await.ok, "the digest JWKS carries the legacy keys");

    // Pulling the issuer key changes nothing the log rests on: its digest is covered by the next.
    let through = v["through"].clone();
    api.ok(Method::POST, "/v1/secrets/issuer/rotate", Some(json!({ "now": true, "retire": issuer }))).await;
    let v = api.ok(Method::GET, "/audit/verify", None).await;
    assert_eq!((v["ok"].as_bool(), v["through"].clone(), v["pulled"][0]["kid"].as_str(), v["pulled"][0]["countersigned"].as_i64()), (Some(true), through, Some(issuer.as_str()), Some(4)), "{v}");

    // The issuer key signing now (made since the digest key) vouches for no digest.
    api.db.write(|c| c.execute("UPDATE audit_heads SET digest_at = 0", ())).unwrap();
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep_audit_chain_as_before_the_digest_key(i64::MAX / 2).unwrap()).await.unwrap();
    let v = api.ok(Method::GET, "/audit/verify", None).await;
    assert_eq!((v["ok"].as_bool(), v["broken"]["reason"].as_str()), (Some(false), Some("signature")), "{v}");
}

#[tokio::test]
async fn insights_show_when_each_key_was_last_read_and_changed() {
    let api = Api::new().await;
    api.set("production", "USED", "u").await;
    api.ok(Method::PUT, "/environments/production/secrets/IDLE?path=/old", Some(json!({ "value": "i" }))).await;
    api.values("production", "/").await;
    api.ok(Method::PATCH, "/environments/production", Some(json!({ "max_age_days": 90 }))).await;
    // A vendor's key that stops working in three days: flagged, and its date outlives a new value.
    let soon = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64 + 3 * 86_400_000;
    api.ok(Method::PUT, "/environments/production/secrets/CERT", Some(json!({ "value": "c1", "expires_at": soon }))).await;
    let kept = api.ok(Method::PUT, "/environments/production/secrets/CERT", Some(json!({ "value": "c2" }))).await;
    assert_eq!(kept["secret"]["expires_at"].as_i64(), Some(soon));
    let i = api.ok(Method::GET, "/environments/production/insights", None).await;
    assert_eq!(i["max_age_days"], 90);
    let cert = &i["keys"][0];
    assert_eq!((cert["key"].as_str(), cert["expiring"].as_bool()), (Some("CERT"), Some(true)));
    let used = &i["keys"][1];
    assert_eq!((used["key"].as_str(), used["reads_30d"].as_i64(), used["last_read_by"].as_str(), used["stale"].as_bool()), (Some("USED"), Some(1), Some("operator"), Some(false)));
    let idle = api.ok(Method::GET, "/environments/production/insights?path=/old", None).await;
    assert_eq!((idle["keys"][0]["key"].as_str(), idle["keys"][0]["last_read_at"].clone()), (Some("IDLE"), Value::Null));

    // An environment with a limit and somewhere to say so is reminded of its stale keys, once a day.
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true, "notify_url": "https://hooks.example/T0/B0/x" }))).await;
    // Production's value pasted into staging, where more people and less vetted code read it.
    api.set("staging", "USED", "u").await;
    let i = api.ok(Method::GET, "/environments/production/insights", None).await;
    assert_eq!((i["keys"][1]["key"].as_str(), i["keys"][1]["exposed_in"].clone(), i["keys"][0]["exposed_in"].clone()), (Some("USED"), json!(["staging"]), json!([])));
    api.db.write(|c| c.execute("UPDATE secrets SET updated_at = updated_at - 8640000000 WHERE key = 'USED'", ())).unwrap();
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || {
        s.sweep().unwrap();
        s.sweep().unwrap();
    })
    .await
    .unwrap();
    let sent = api.sent.0.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "one reminder a day");
    let findings: Vec<(&str, &str)> = sent[0]["findings"].as_array().unwrap().iter().map(|f| (f["issue"].as_str().unwrap(), f["key"].as_str().unwrap())).collect();
    // Every finding there, worst first: USED's value is in staging too.
    assert_eq!(findings, [("exposed", "USED"), ("expiring", "CERT"), ("stale", "USED")]);
    assert!(sent[0]["text"].as_str().unwrap().starts_with("roll secrets: production has 1 exposed: USED (also in staging); 1 expiring: CERT (expires "), "{}", sent[0]["text"]);
    assert!(!sent[0].to_string().contains("\"u\"") && !sent[0].to_string().contains("\"c2\""), "names, never values");

    // A credential anyone could guess is flagged, to whoever may reveal it.
    api.set("staging", "DB_PASSWORD", "Password1").await;
    let weak = |v: &Value| v["keys"].as_array().unwrap().iter().find(|k| k["key"] == "DB_PASSWORD").map(|k| k["weak"].clone());
    let i = api.ok(Method::GET, "/environments/staging/insights", None).await;
    assert_eq!(weak(&i), Some(json!("a common password")));
    let token = api.token(json!({ "name": "lint", "actions": ["read"] }));
    let (_, i) = api.req(Method::GET, "/environments/staging/insights", None, Some(token.as_str())).await;
    assert_eq!(weak(&i), Some(Value::Null));
}

#[tokio::test]
async fn health_lists_every_finding_in_every_environment_worst_first() {
    let api = Api::new().await;
    api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": [{ "op": "set", "key": "DB_PASSWORD", "value": "Password1" }, { "op": "set", "key": "CERT", "value": "c", "expires_at": 1000 }] }))).await;
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true }))).await;
    api.set("staging", "DB_PASSWORD", "Password1").await;
    api.ok(Method::PUT, "/required", Some(json!([{ "key": "CERT", "owner": "@payments" }]))).await;
    let found = api.ok(Method::GET, "/health", None).await;
    let got: Vec<(&str, &str, &str)> = found.as_array().unwrap().iter().map(|f| (f["issue"].as_str().unwrap(), f["environment"].as_str().unwrap(), f["key"].as_str().unwrap())).collect();
    assert_eq!(
        got,
        [
            ("expired", "production", "CERT"),
            ("missing", "development", "CERT"),
            ("missing", "staging", "CERT"),
            ("exposed", "production", "DB_PASSWORD"),
            ("weak", "staging", "DB_PASSWORD"),
            ("weak", "production", "DB_PASSWORD"),
        ]
    );
    assert_eq!(found[3]["detail"], "also in staging");
    // A required key's owner is who to ask.
    assert_eq!((found[0]["owner"].as_str(), found[1]["owner"].as_str(), found[3]["owner"].as_str()), (Some("@payments"), Some("@payments"), None));
    // It filters itself (roll-access lets environment-limited tokens in on that): a token for
    // staging hears only of staging.
    let t = api.token(json!({ "name": "ci", "environments": ["staging"], "actions": ["read", "reveal"] }));
    let (_, mine) = api.req(Method::GET, "/health", None, Some(t.as_str())).await;
    assert!(mine.as_array().unwrap().iter().all(|f| f["environment"] == "staging") && !mine.as_array().unwrap().is_empty(), "{mine}");
    // Keys no repository on the canvas mentions, when asked (roll code's search is a fake here).
    struct Code;
    impl roll_secrets::health::CodeRefs for Code {
        fn referenced(&self, repo: &str, keys: &[String]) -> Option<std::collections::BTreeSet<String>> {
            (repo == "acme/web").then(|| keys.iter().filter(|k| *k == "CERT").cloned().collect())
        }
    }
    api.secrets.set_code_refs(Arc::new(Code));
    let p = api.project.clone();
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{p}/resources/repo/acme%2Fweb"), Some(json!({ "x": 0, "y": 0 }))).await;
    let with_code = api.ok(Method::GET, "/health?code=1", None).await;
    let unreferenced: Vec<(&str, &str)> = with_code.as_array().unwrap().iter().filter(|f| f["issue"] == "unreferenced").map(|f| (f["environment"].as_str().unwrap(), f["key"].as_str().unwrap())).collect();
    assert_eq!(unreferenced, [("staging", "DB_PASSWORD"), ("production", "DB_PASSWORD")]);
    // It says something about the repository's contents: never to a caller who can't read its code.
    let (s, _) = api.req(Method::GET, "/health?code=1", None, Some(t.as_str())).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Across projects: this one, with its findings.
    let all = api.ok(Method::GET, "/v1/secrets/findings", None).await;
    assert_eq!((all.as_array().map(Vec::len), all[0]["project"].as_str(), all[0]["findings"].as_array().map(Vec::len)), (Some(1), Some(api.project.as_str()), Some(6)));
    // A server's credential under a name a framework sends to every browser: the write warns and
    // health flags it; a publishable key under the same prefix is left alone.
    let w = api.ok(Method::PUT, "/environments/development/secrets/NEXT_PUBLIC_STRIPE_KEY", Some(json!({ "value": "sk_live_51Hx9Q2aBcD7eFgH8iJkLmN" }))).await;
    assert!(w["warnings"][0].as_str().unwrap().starts_with("NEXT_PUBLIC_STRIPE_KEY: NEXT_PUBLIC_ keys go into the browser bundle (Next.js)"), "{w}");
    api.set("development", "NEXT_PUBLIC_STRIPE_PUBLISHABLE", "pk_live_51Hx9Q2aBcD7eFgH8iJkLmN").await;
    let public: Vec<String> = api.ok(Method::GET, "/health", None).await.as_array().unwrap().iter().filter(|f| f["issue"] == "public").map(|f| f["key"].as_str().unwrap().to_string()).collect();
    assert_eq!(public, ["NEXT_PUBLIC_STRIPE_KEY"]);
    // What kind of credential it is comes from the value: not for a caller who can't reveal it.
    let reader = api.token(json!({ "name": "reader", "actions": ["read"] }));
    let (_, seen) = api.req(Method::GET, "/health", None, Some(&reader)).await;
    let detail = seen.as_array().unwrap().iter().find(|f| f["issue"] == "public").map(|f| f["detail"].clone());
    assert_eq!(detail, Some(json!("NEXT_PUBLIC_ keys go into the browser bundle (Next.js), and this one holds a server credential")));
}

#[tokio::test]
async fn a_scan_finds_values_only_its_caller_may_reveal() {
    let api = Api::new().await;
    api.set("production", "STRIPE_KEY", "sk_live_51Hx9Q2abc").await;
    let text = json!({ "candidates": ["hello world", "sk_live_51Hx9Q2abc"] });
    let found = api.ok(Method::POST, "/leaks", Some(text.clone())).await;
    assert_eq!(found, json!([{ "index": 1, "key": "STRIPE_KEY", "environment": "production", "path": "/" }]));
    // Without reveal, a scan would be an oracle for guessing values: it finds nothing.
    let token = api.token(json!({ "name": "lint", "actions": ["read"] }));
    let (s, v) = api.req(Method::POST, "/leaks", Some(text), Some(token.as_str())).await;
    assert_eq!((s, v), (StatusCode::OK, json!([])));
    // Nor someone else's personal value, even to a caller who may reveal the folder: only its owner
    // ever sees it.
    api.ok(Method::PUT, "/environments/production/secrets/DB_PASSWORD", Some(json!({ "value": "mine-only-7f3k2", "type": "personal" }))).await;
    let mine = json!({ "candidates": ["mine-only-7f3k2"] });
    let reveal = api.token(json!({ "name": "ci", "actions": ["read", "reveal"] }));
    let (_, v) = api.req(Method::POST, "/leaks", Some(mine.clone()), Some(reveal.as_str())).await;
    assert_eq!(v, json!([]));
    assert_eq!(api.ok(Method::POST, "/leaks", Some(mine)).await[0]["key"], "DB_PASSWORD", "its owner finds it");
    // A credential guard's scan (roll MCP, `X-Roll-Guard: 1`) is audited apart from a person's.
    let req = Request::builder().method(Method::POST).uri(format!("/v1/secrets/projects/{}/leaks", api.project)).header("content-type", "application/json").header("authorization", format!("Bearer {reveal}")).header("x-roll-guard", "1").body(Body::from(json!({ "candidates": ["sk_live_51Hx9Q2abc"] }).to_string())).unwrap();
    assert_eq!(api.router.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);
    let rows = api.ok(Method::GET, "/audit?action=secrets.guard", None).await;
    assert_eq!((rows.as_array().unwrap().len(), rows[0]["keys"].clone()), (1, json!(["STRIPE_KEY"])));
}

#[tokio::test]
async fn a_rotator_outside_roll_issues_and_rotates_a_keys_value() {
    let api = Api::new().await;
    let r = api.ok(Method::POST, "/rotators", Some(json!({ "name": "stripe", "url": "https://rotator.example/stripe?token=t" }))).await;
    assert!(r["secret"].as_str().unwrap().starts_with("whsec_") && r["host"] == "rotator.example", "{r}");
    let link = json!({ "kind": "rotator", "ref": r["id"], "field": "value", "mode": "managed" });
    api.ok(Method::PUT, "/environments/production/secrets/STRIPE_KEY", Some(json!({ "link": link }))).await;
    assert_eq!(api.values("production", "/").await["values"]["STRIPE_KEY"], "vendor-key-1");
    api.ok(Method::POST, "/environments/production/secrets/STRIPE_KEY/rotate", None).await;
    assert_eq!(api.values("production", "/").await["values"]["STRIPE_KEY"], "vendor-key-2");
    // The key it issues keeps it.
    let (s, _) = api.call(Method::DELETE, &format!("/rotators/{}", r["id"].as_str().unwrap()), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // Moved and re-keyed in place: the next call goes to the new URL, signed with the new secret
    // (shown once), and the link carries on.
    let id = r["id"].as_str().unwrap();
    // Not while a retired key waits to be revoked through the old endpoint.
    assert_eq!(api.call(Method::PATCH, &format!("/rotators/{id}"), Some(json!({ "url": "https://rotator2.example/stripe" }))).await.0, StatusCode::CONFLICT);
    let p = api.project.clone();
    api.db.write(move |c| c.execute("UPDATE leases SET expires_at = 0 WHERE project_id = ?", [p.as_str()])).unwrap();
    let sw = api.secrets.clone();
    tokio::task::spawn_blocking(move || sw.sweep().unwrap()).await.unwrap();
    let moved = api.ok(Method::PATCH, &format!("/rotators/{id}"), Some(json!({ "url": "https://rotator2.example/stripe" }))).await;
    assert_eq!(moved["host"], "rotator2.example");
    let rekeyed = api.ok(Method::POST, &format!("/rotators/{id}/rotate-secret"), None).await;
    let secret = rekeyed["secret"].as_str().unwrap().to_string();
    assert!(secret.starts_with("whsec_") && secret != r["secret"].as_str().unwrap());
    api.ok(Method::POST, "/environments/production/secrets/STRIPE_KEY/rotate", None).await;
    assert_eq!(api.sent.3.lock().unwrap().clone(), Some(("https://rotator2.example/stripe".to_string(), secret)));

    // A scheduled rotation and a revocation that fail show in health until they succeed.
    api.sent.2.store(true, std::sync::atomic::Ordering::Relaxed);
    let p = api.project.clone();
    api.db
        .write(move |c| {
            // Due now (rotate_days 1, issued long ago) and the retired key's grace over.
            let state: String = c.query_row("SELECT link_state FROM secrets WHERE project_id = ? AND key = 'STRIPE_KEY'", [p.as_str()], |r| r.get(0))?;
            let mut state: Value = serde_json::from_str(&state).unwrap();
            state["issued_at"] = json!(0);
            c.execute("UPDATE secrets SET link_state = ?, link = replace(link, '\"mode\":\"managed\"', '\"mode\":\"managed\",\"options\":{\"rotate_days\":1}') WHERE project_id = ? AND key = 'STRIPE_KEY'", [state.to_string().as_str(), p.as_str()])?;
            c.execute("UPDATE leases SET expires_at = 0 WHERE project_id = ?", [p.as_str()])
        })
        .unwrap();
    let sweep = || {
        let s = api.secrets.clone();
        async move { tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap() }
    };
    sweep().await;
    let health = api.ok(Method::GET, "/health", None).await;
    let issues: Vec<&str> = health.as_array().unwrap().iter().filter_map(|f| f["issue"].as_str()).collect();
    assert!(issues.contains(&"rotation_failing") && issues.contains(&"revoke_failing"), "{health}");
    assert!(health.to_string().contains("connection refused"), "the reason: {health}");
    // The rotations list says it too, per credential.
    let rot = api.ok(Method::GET, "/rotations", None).await;
    assert_eq!((rot[0]["keys"].clone(), rot[0]["kind"].as_str(), rot[0]["rotate_days"].as_u64()), (json!(["STRIPE_KEY"]), Some("rotator"), Some(1)), "{rot}");
    assert!(rot[0]["last_error"].as_str().unwrap().contains("connection refused"), "{rot}");
    api.sent.2.store(false, std::sync::atomic::Ordering::Relaxed);
    let p = api.project.clone();
    api.db.write(move |c| c.execute("UPDATE leases SET expires_at = 0 WHERE project_id = ? AND revoked_at IS NULL", [p.as_str()])).unwrap();
    sweep().await;
    let health = api.ok(Method::GET, "/health", None).await;
    assert!(!health.to_string().contains("failing"), "both cleared once they succeed: {health}");
    let rot = api.ok(Method::GET, "/rotations", None).await;
    assert!(rot[0]["last_error"].is_null() && rot[0]["next_due"].as_i64().unwrap() > rot[0]["issued_at"].as_i64().unwrap(), "{rot}");
    // A rotation window: a due rotation waits until it's open (UTC), and says when that is.
    let (s, _) = api.call(Method::PUT, "/environments/production/secrets/OTHER_KEY", Some(json!({ "link": { "kind": "rotator", "ref": r["id"], "field": "value", "mode": "managed", "options": { "rotate_window": "sun 02:00-04:00" } } }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a window goes with rotate_days");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let hhmm = |ms: i64| format!("{:02}:{:02}", ms.rem_euclid(86_400_000) / 3_600_000, ms.rem_euclid(3_600_000) / 60_000);
    let (closed, open) = (format!("daily {}-{}", hhmm(now + 3 * 3_600_000), hhmm(now + 5 * 3_600_000)), format!("daily {}-{}", hhmm(now - 1_800_000), hhmm(now + 5_400_000)));
    let window = |w: String| {
        let p = api.project.clone();
        api.db
            .write(move |c| {
                let state: String = c.query_row("SELECT link_state FROM secrets WHERE project_id = ? AND key = 'STRIPE_KEY'", [p.as_str()], |r| r.get(0))?;
                let mut state: Value = serde_json::from_str(&state).unwrap();
                state["issued_at"] = json!(0);
                let link: String = c.query_row("SELECT link FROM secrets WHERE project_id = ? AND key = 'STRIPE_KEY'", [p.as_str()], |r| r.get(0))?;
                let mut link: Value = serde_json::from_str(&link).unwrap();
                link["options"]["rotate_window"] = json!(w);
                c.execute("UPDATE secrets SET link_state = ?, link = ? WHERE project_id = ? AND key = 'STRIPE_KEY'", [state.to_string().as_str(), link.to_string().as_str(), p.as_str()])
            })
            .unwrap();
    };
    window(closed.clone());
    let before = api.values("production", "/").await["values"]["STRIPE_KEY"].clone();
    sweep().await;
    assert_eq!(api.values("production", "/").await["values"]["STRIPE_KEY"], before, "due, but its window isn't open");
    let rot = api.ok(Method::GET, "/rotations", None).await;
    assert_eq!(rot[0]["rotate_window"].as_str(), Some(closed.as_str()));
    assert!(rot[0]["next_due"].as_i64().unwrap() > now + 2 * 3_600_000, "the window's next opening: {rot}");
    window(open);
    sweep().await;
    assert_ne!(api.values("production", "/").await["values"]["STRIPE_KEY"], before, "rotated inside its window");

    // The project goes while the rotator is down: what it issued is still revoked, by the
    // sweeper, which keeps the rotator (and the project's key) until then, past the purge.
    api.sent.2.store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(delete_project(&api, "?confirm=shop", None).await.0, StatusCode::NO_CONTENT);
    let count = |table: &str| {
        let (sql, p) = (format!("SELECT COUNT(*) FROM {table} WHERE project_id = ?"), api.project.clone());
        api.db.read(move |c| c.query_row(&sql, [p.as_str()], |r| r.get::<_, i64>(0))).unwrap()
    };
    assert!(count("secrets") > 0, "kept for the recovery window");
    let p = api.project.clone();
    api.db.write(move |c| c.execute("UPDATE projects SET purge_at = 0 WHERE project_id = ?", [p.as_str()])).unwrap();
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap();
    assert_eq!((count("rotators"), count("projects"), count("secrets")), (1, 1, 0));
    api.sent.2.store(false, std::sync::atomic::Ordering::Relaxed);
    let p = api.project.clone();
    api.db.write(move |c| c.execute("UPDATE leases SET expires_at = 0 WHERE project_id = ?", [p.as_str()])).unwrap();
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap();
    let revoked: Vec<Value> = api.sent.0.lock().unwrap().iter().filter(|b| b["action"] == "revoke").map(|b| b["handle"].clone()).collect();
    assert!(revoked.contains(&json!(2)), "the key in use when the project went is revoked: {revoked:?}");
    assert_eq!((count("rotators"), count("projects"), count("leases")), (0, 0, 0));
}

#[tokio::test]
async fn a_github_app_gives_each_lease_a_narrowed_token_that_ends_with_it() {
    let api = Api::new().await;
    let key = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\n-----END RSA PRIVATE KEY-----";
    let (s, e) = api.call(Method::POST, "/github-apps", Some(json!({ "name": "deploy", "app_id": 1, "installation_id": 8, "private_key": key, "repositories": ["web", "api"], "permissions": { "contents": "write" } }))).await;
    assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains("Integration not found") && !e.to_string().contains("MIIE"), "{e}");
    let app = api.ok(Method::POST, "/github-apps", Some(json!({ "name": "deploy", "app_id": 1, "installation_id": 7, "private_key": key, "repositories": ["web", "api"], "permissions": { "contents": "write" } }))).await;
    assert!(app.get("private_key").is_none() && app["api_base"] == "https://api.github.com", "{app}");
    assert!(api.github.0.lock().unwrap().is_empty(), "the check's token is revoked at once");
    let (s, e) = api.call(Method::POST, "/github-apps", Some(json!({ "name": "wide", "app_id": 1, "installation_id": 7, "private_key": key, "repositories": [], "permissions": {} }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "an App names the most its tokens may reach: {e}");

    let link = |options: Value| json!({ "kind": "github_app", "ref": app["id"], "field": "token", "mode": "dynamic", "options": options });
    let (s, e) = api.call(Method::PUT, "/environments/staging/secrets/GITHUB_TOKEN", Some(json!({ "link": link(json!({ "ttl_minutes": 120 })) }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a lease can't outlive GitHub's hour: {e}");
    // A link narrows within the App's ceiling, never past it.
    let (s, e) = api.call(Method::PUT, "/environments/staging/secrets/GITHUB_TOKEN", Some(json!({ "link": link(json!({ "permissions": { "administration": "write" } })) }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    let narrow = json!({ "repositories": ["web"], "permissions": { "contents": "read" } });
    api.ok(Method::PUT, "/environments/staging/secrets/GITHUB_TOKEN", Some(json!({ "link": link(narrow) }))).await;
    let a = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "job:release" }))).await;
    let token = a["values"]["GITHUB_TOKEN"].as_str().unwrap().to_string();
    let (repos, perms) = api.github.0.lock().unwrap()[&token].clone();
    assert_eq!((repos, perms["contents"].as_str()), (vec!["web".to_string()], "read"));
    let lease = a["leases"][0].as_str().unwrap();
    api.ok(Method::DELETE, &format!("/leases/{lease}"), None).await;
    assert!(!api.github.0.lock().unwrap().contains_key(&token), "ending the lease revoked the token");

    let (s, _) = api.call(Method::DELETE, &format!("/github-apps/{}", app["id"].as_str().unwrap()), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "its key keeps it");
}

#[tokio::test]
async fn a_saas_rotator_issues_and_revokes_vendor_keys() {
    let api = Api::new().await;
    // A credential the vendor refuses, or one that can't manage keys, isn't kept.
    for cred in ["SG.wrong", "SG.mail-only"] {
        let (s, e) = api.call(Method::POST, "/saas", Some(json!({ "name": "mail", "vendor": "sendgrid", "target": {}, "credential": cred }))).await;
        assert!(s == StatusCode::BAD_REQUEST && !e.to_string().contains(cred), "{cred}: {e}");
    }
    let sg = api.ok(Method::POST, "/saas", Some(json!({ "name": "mail", "vendor": "sendgrid", "target": { "scopes": ["mail.send"] }, "credential": "SG.admin" }))).await;
    let tw = api.ok(Method::POST, "/saas", Some(json!({ "name": "sms", "vendor": "twilio", "target": { "account_sid": TWILIO_SID }, "credential": "twilio-token" }))).await;
    assert_eq!((sg["vendor"].as_str(), sg["target"].clone()), (Some("sendgrid"), json!({ "scopes": ["mail.send"] })), "{sg}");
    let listed = api.ok(Method::GET, "/saas", None).await;
    let audit = api.ok(Method::GET, "/audit", None).await;
    for shown in [&sg, &tw, &listed, &audit] {
        assert!(!shown.to_string().contains("SG.admin") && !shown.to_string().contains("twilio-token"), "never the credential: {shown}");
    }
    let (s, _) = api.call(Method::POST, "/saas", Some(json!({ "name": "sso", "vendor": "okta", "target": { "domain": "10.0.0.1", "app_id": "0oa1" }, "credential": "t" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a tenant domain is a hostname");

    // Linking issues a key; the id and the secret of one key share it.
    let link = |r: &Value, field: &str| json!({ "link": { "kind": "saas", "ref": r["id"], "field": field, "mode": "managed" } });
    api.ok(Method::PUT, "/environments/production/secrets/SENDGRID_API_KEY", Some(link(&sg, "secret"))).await;
    api.ok(Method::PUT, "/environments/production/secrets/TWILIO_API_KEY", Some(link(&tw, "id"))).await;
    api.ok(Method::PUT, "/environments/production/secrets/TWILIO_API_SECRET", Some(link(&tw, "secret"))).await;
    let values = |v: Value| (v["values"]["SENDGRID_API_KEY"].clone(), v["values"]["TWILIO_API_KEY"].clone(), v["values"]["TWILIO_API_SECRET"].clone());
    assert_eq!(values(api.values("production", "/").await), (json!("SG.key-1"), json!("SK2"), json!("twilio-secret-2")));

    // Rotating issues a new one; the old one keeps working through the grace period.
    api.ok(Method::POST, "/environments/production/secrets/SENDGRID_API_KEY/rotate", None).await;
    api.ok(Method::POST, "/environments/production/secrets/TWILIO_API_SECRET/rotate", None).await;
    assert_eq!(values(api.values("production", "/").await), (json!("SG.key-3"), json!("SK4"), json!("twilio-secret-4")));
    assert_eq!(api.sent.5.lock().unwrap().live, ["sgk1", "SK2", "sgk3", "SK4"]);
    let (s, _) = api.call(Method::DELETE, &format!("/saas/{}", sg["id"].as_str().unwrap()), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "a key it issues keeps it");

    // Once the grace period is over, the old keys are revoked at the vendor.
    let p = api.project.clone();
    api.db.write(move |c| c.execute("UPDATE leases SET expires_at = 0 WHERE project_id = ?", [p.as_str()])).unwrap();
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap();
    assert_eq!(api.sent.5.lock().unwrap().live, ["sgk3", "SK4"]);
}

/// A SendGrid key managed by roll in production, and a check of roll code repo `acme/web`'s
/// change that adds `leaked`: what roll code learns of each match (key, rotatable by a person).
async fn sendgrid_in_production(api: &Api) {
    let sg = api.ok(Method::POST, "/saas", Some(json!({ "name": "mail", "vendor": "sendgrid", "target": { "scopes": ["mail.send"] }, "credential": "SG.admin" }))).await;
    api.ok(Method::PUT, "/environments/production/secrets/SENDGRID_API_KEY", Some(json!({ "link": { "kind": "saas", "ref": sg["id"], "field": "secret", "mode": "managed" } }))).await;
}

async fn leak_in_code(api: &Api, leaked: &str) -> Vec<(String, bool)> {
    let (s, p, candidates) = (api.secrets.clone(), api.project.clone(), vec!["const port = 8080;".to_string(), leaked.to_string()]);
    tokio::task::spawn_blocking(move || s.leaks_in_code(&p, &candidates, "acme/web").unwrap()).await.unwrap().into_iter().map(|(_, l, rotatable)| (l.key, rotatable)).collect()
}

/// When a value leaks: with `leaks.rotate` off, the event says roll could rotate the key and a
/// person may (roll code's notice offers it), and nothing rotates; with `managed`, the next leak
/// rotates it at once, and the leaked key is revoked at SendGrid without a grace period, once.
#[tokio::test]
async fn a_leaked_managed_key_is_offered_for_rotation_or_rotated_at_once_without_grace() {
    let api = Api::new().await;
    sendgrid_in_production(&api).await;
    assert_eq!(api.ok(Method::GET, "", None).await["leaks"], json!({ "rotate": "off" }));
    let start = api.ok(Method::GET, "/events?latest=1", None).await["next"].as_i64().unwrap();
    assert_eq!(leak_in_code(&api, "SG.key-1").await, [("SENDGRID_API_KEY".to_string(), true)]);
    let leaked = api.ok(Method::GET, &format!("/events?after={start}&kinds=secret.leaked"), None).await;
    let data = &leaked["events"][0]["data"];
    assert_eq!((&data["repo"], &data["rotatable"], &data["rotating"]), (&json!("acme/web"), &json!(true), &Value::Null), "{leaked}");
    assert_eq!(api.values("production", "/").await["values"]["SENDGRID_API_KEY"], "SG.key-1", "off: nothing rotates");
    assert_eq!(api.sent.5.lock().unwrap().live, ["sgk1"]);

    let writer = api.token(json!({ "name": "ci", "actions": ["read", "write"] }));
    let (s, _) = api.req(Method::PATCH, "", Some(json!({ "leaks": { "rotate": "managed" } })), Some(writer.as_str())).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "an admin's setting");
    assert_eq!(api.ok(Method::PATCH, "", Some(json!({ "leaks": { "rotate": "managed" } }))).await, json!({ "leaks": { "rotate": "managed" } }));
    // The same leak again (the change read anew, within the event's day): rotated now.
    assert_eq!(leak_in_code(&api, "SG.key-1").await, [("SENDGRID_API_KEY".to_string(), false)]);
    assert_eq!(api.values("production", "/").await["values"]["SENDGRID_API_KEY"], "SG.key-2");
    assert_eq!(api.sent.5.lock().unwrap().live, ["sgk2"], "the leaked key is revoked at once, no grace");
    // The old value matches nothing now: checked again, nothing else rotates.
    assert!(leak_in_code(&api, "SG.key-1").await.is_empty());
    assert_eq!(api.sent.5.lock().unwrap().made, 2);

    let events = api.ok(Method::GET, &format!("/events?after={start}&kinds=secret.leaked,rotation.succeeded"), None).await;
    let events = events["events"].as_array().unwrap();
    assert_eq!(events.iter().map(|e| e["kind"].as_str().unwrap()).collect::<Vec<_>>(), ["secret.leaked", "secret.leaked", "rotation.succeeded"]);
    assert_eq!(events[1]["data"]["rotating"], json!(["SENDGRID_API_KEY"]));
    let done = &events[2]["data"];
    assert_eq!((&done["via"], &done["grace_minutes"], &done["version"]), (&json!("leak"), &json!(0), &json!(2)), "{done}");
    let audit = api.ok(Method::GET, "/audit", None).await.to_string();
    assert!(!events.iter().any(|e| e.to_string().contains("SG.key")) && !audit.contains("SG.key"), "never a value");
}

/// A leak never rotates a key whose environment needs approvals: it opens a change request for
/// the rotation instead, and the event says so; applying the approved request rotates it with no
/// grace.
#[tokio::test]
async fn a_leaked_key_in_an_environment_with_approvals_gets_a_change_request_instead() {
    let api = Api::new().await;
    sendgrid_in_production(&api).await;
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true, "approvals": 1 }))).await;
    api.ok(Method::PATCH, "", Some(json!({ "leaks": { "rotate": "managed" } }))).await;
    let start = api.ok(Method::GET, "/events?latest=1", None).await["next"].as_i64().unwrap();
    leak_in_code(&api, "SG.key-1").await;
    leak_in_code(&api, "SG.key-1").await;
    assert_eq!(api.values("production", "/").await["values"]["SENDGRID_API_KEY"], "SG.key-1", "not rotated without the approvals");
    let open = api.ok(Method::GET, "/environments/production/requests?state=open", None).await;
    let open = open.as_array().unwrap();
    assert_eq!(open.len(), 1, "one request, however often the leak is found: {open:?}");
    let (id, r) = (open[0]["id"].as_str().unwrap(), &open[0]);
    assert_eq!((&r["action"], &r["items"][0]["key"], &r["items"][0]["what"], &r["by"]), (&json!("rotate"), &json!("SENDGRID_API_KEY"), &json!(["rotated"]), &json!("roll:leaks")), "{r}");
    let leaked = api.ok(Method::GET, &format!("/events?after={start}&kinds=secret.leaked"), None).await;
    assert_eq!(leaked["events"][0]["data"]["requests"], json!([id]), "{leaked}");

    let (s, _) = api.call(Method::POST, &format!("/environments/production/requests/{id}/apply"), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "not before its approvals");
    api.ok(Method::POST, &format!("/environments/production/requests/{id}/review"), Some(json!({ "verdict": "approve" }))).await;
    let r = api.ok(Method::POST, &format!("/environments/production/requests/{id}/apply"), None).await;
    assert!(r["state"] == "applied" && r["applied_seq"].is_i64(), "{r}");
    assert_eq!(api.values("production", "/").await["values"]["SENDGRID_API_KEY"], "SG.key-2");
    assert_eq!(api.sent.5.lock().unwrap().live, ["sgk2"], "the leaked key is revoked at once");
}

/// Register `vendor` (a refused admin credential first, which isn't kept), link `key` to its
/// secret in production, managed with `options`, and answer the registration.
async fn key_vendor(api: &Api, vendor: &str, target: Value, credential: &str, key: &str, options: Value) -> Value {
    let wrong = "fake-wrong:admin";
    let (s, e) = api.call(Method::POST, "/saas", Some(json!({ "name": vendor, "vendor": vendor, "target": target, "credential": wrong }))).await;
    assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains(vendor) && !e.to_string().contains(wrong), "{vendor}: {e}");
    let r = api.ok(Method::POST, "/saas", Some(json!({ "name": vendor, "vendor": vendor, "target": target, "credential": credential }))).await;
    assert!(!r.to_string().contains(credential) && !api.ok(Method::GET, "/audit", None).await.to_string().contains(credential), "never the credential: {r}");
    api.ok(Method::PUT, &format!("/environments/production/secrets/{key}"), Some(json!({ "link": { "kind": "saas", "ref": r["id"], "field": "secret", "mode": "managed", "options": options } }))).await;
    r
}

/// The grace period over: the sweep revokes the retired keys.
async fn end_grace(api: &Api) {
    let p = api.project.clone();
    api.db.write(move |c| c.execute("UPDATE leases SET expires_at = 0 WHERE project_id = ? AND revoked_at IS NULL", [p.as_str()])).unwrap();
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap();
}

#[tokio::test]
async fn gitlab_access_tokens_rotate_and_the_old_one_is_revoked_after_the_grace_period() {
    let api = Api::new().await;
    let (s, _) = api.call(Method::POST, "/saas", Some(json!({ "name": "gl", "vendor": "gitlab", "target": { "host": "10.0.0.1", "project": "1", "scopes": ["api"] }, "credential": "x" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a self-managed host is a hostname");
    let target = json!({ "host": "gitlab.example.com", "project": "acme/web", "scopes": ["read_api"], "access_level": 20, "expires_days": 90 });
    key_vendor(&api, "gitlab", target, "fake-gitlab-admin", "GITLAB_TOKEN", json!({ "rotate_days": 60 })).await;
    assert_eq!(api.values("production", "/").await["values"]["GITLAB_TOKEN"], "fake-gitlab-token-1");
    assert_eq!(api.sent.10.bodies.lock().unwrap()[0]["access_level"], 20);

    // A new token, not GitLab's rotate: the old one keeps working through the grace period.
    let start = api.ok(Method::GET, "/events?latest=1", None).await["next"].as_i64().unwrap();
    api.ok(Method::POST, "/environments/production/secrets/GITLAB_TOKEN/rotate", None).await;
    assert_eq!(api.values("production", "/").await["values"]["GITLAB_TOKEN"], "fake-gitlab-token-2");
    assert_eq!(*api.sent.10.live.lock().unwrap(), ["1", "2"]);
    let events = api.ok(Method::GET, &format!("/events?after={start}&kinds=rotation.succeeded"), None).await;
    assert_eq!(events["events"][0]["key"], "GITLAB_TOKEN", "{events}");
    end_grace(&api).await;
    assert_eq!(*api.sent.10.live.lock().unwrap(), ["2"]);
}

#[tokio::test]
async fn keys_a_vendor_ends_rotate_before_that_and_health_warns_of_them_and_of_the_admin_credential() {
    let api = Api::new().await;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let day = 86_400_000;
    let gitlab = |expires_days: u16, name: &str, credential_expires_at: Option<i64>| {
        let mut r = json!({ "name": name, "vendor": "gitlab", "target": { "host": "gitlab.example.com", "project": "acme/web", "scopes": ["read_api"], "expires_days": expires_days }, "credential": "fake-gitlab-admin" });
        if let Some(t) = credential_expires_at {
            r["credential_expires_at"] = json!(t);
        }
        r
    };
    let (s, e) = api.call(Method::POST, "/saas", Some(gitlab(90, "gl", Some(now - day)))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "an admin credential that has ended already: {e}");
    let gl = api.ok(Method::POST, "/saas", Some(gitlab(90, "gl", Some(now + 10 * day)))).await;
    assert_eq!(gl["credential_expires_at"], now + 10 * day);
    assert!(!gl.to_string().contains("fake-gitlab-admin"), "{gl}");
    let link = |r: &Value, options: Value| json!({ "link": { "kind": "saas", "ref": r["id"], "field": "secret", "mode": "managed", "options": options } });

    // GitLab ends each token after 90 days: a link rotates it two weeks before, or isn't made.
    for options in [json!({}), json!({ "rotate_days": 77 })] {
        let (s, e) = api.call(Method::PUT, "/environments/production/secrets/GITLAB_TOKEN", Some(link(&gl, options.clone()))).await;
        assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains("at most 76"), "{options}: {e}");
    }
    assert!(api.sent.10.live.lock().unwrap().is_empty(), "nothing issued for a refused link");
    let set = api.ok(Method::PUT, "/environments/production/secrets/GITLAB_TOKEN", Some(link(&gl, json!({ "rotate_days": 76 })))).await;
    let ends = (time::OffsetDateTime::now_utc() + time::Duration::days(90)).date().midnight().assume_utc().unix_timestamp() * 1000;
    assert_eq!(set["secret"]["expires_at"].as_i64(), Some(ends), "the key carries GitLab's end: {set}");

    // A token that ends in ten days is listed two weeks ahead; one that ends in 90 isn't. The admin
    // credential ending in ten days is listed by the registration's name, never its value.
    let short = api.ok(Method::POST, "/saas", Some(gitlab(10, "gl-short", None))).await;
    api.ok(Method::PUT, "/environments/production/secrets/GITLAB_SHORT", Some(link(&short, json!({ "rotate_days": 5 })))).await;
    let health = api.ok(Method::GET, "/health", None).await;
    let expiring: Vec<(&str, &str)> = health.as_array().unwrap().iter().filter(|f| f["issue"] == "expiring").map(|f| (f["environment"].as_str().unwrap(), f["key"].as_str().unwrap())).collect();
    assert_eq!(expiring, [("", "gl"), ("production", "GITLAB_SHORT")], "{health}");
    assert!(!health.to_string().contains("fake-gitlab"), "{health}");
}

#[tokio::test]
async fn a_saas_rotators_credential_is_replaced_in_place_and_the_next_rotation_uses_it() {
    let api = Api::new().await;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let day = 86_400_000;
    let target = json!({ "host": "gitlab.example.com", "project": "acme/web", "scopes": ["read_api"], "access_level": 20, "expires_days": 90 });
    let r = key_vendor(&api, "gitlab", target, "fake-gitlab-admin", "GITLAB_TOKEN", json!({ "rotate_days": 60 })).await;
    let at = format!("/saas/{}", r["id"].as_str().unwrap());
    let last_admin = || api.sent.10.gitlab_admins.lock().unwrap().last().cloned().unwrap();
    let expiring = |health: &Value| -> Vec<String> { health.as_array().unwrap().iter().filter(|f| f["issue"] == "expiring" && f["environment"] == "").map(|f| f["key"].as_str().unwrap().to_string()).collect() };

    // A credential the vendor refuses changes nothing: the old one stays, dated as it was.
    let (s, e) = api.call(Method::PATCH, &at, Some(json!({ "credential": "fake-gitlab-wrong", "credential_expires_at": now + 10 * day, "name": "gl-ops" }))).await;
    assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains("gitlab answered 401") && !e.to_string().contains("fake-gitlab-wrong"), "{e}");
    let kept = &api.ok(Method::GET, "/saas", None).await[0];
    assert!(kept["name"] == "gitlab" && kept.get("credential_expires_at").is_none(), "{kept}");
    api.ok(Method::POST, "/environments/production/secrets/GITLAB_TOKEN/rotate", None).await;
    assert_eq!(last_admin(), "fake-gitlab-admin");

    // A good one is checked, swapped in place (the link stays), and the next rotation uses it.
    let patched = api.ok(Method::PATCH, &at, Some(json!({ "credential": "fake-gitlab-admin-2", "credential_expires_at": now + 10 * day, "name": "gl-ops" }))).await;
    assert!(patched["id"] == r["id"] && patched["name"] == "gl-ops" && patched["credential_expires_at"] == now + 10 * day && !patched.to_string().contains("fake-gitlab-admin"), "{patched}");
    assert_eq!(expiring(&api.ok(Method::GET, "/health", None).await), ["gl-ops"], "health warns of the new credential's end");
    api.ok(Method::POST, "/environments/production/secrets/GITLAB_TOKEN/rotate", None).await;
    assert_eq!((last_admin().as_str(), api.values("production", "/").await["values"]["GITLAB_TOKEN"].clone()), ("fake-gitlab-admin-2", json!("fake-gitlab-token-3")));
    // Retired keys are revoked through the new credential too.
    end_grace(&api).await;
    assert_eq!((api.sent.10.live.lock().unwrap().clone(), last_admin()), (vec!["3".to_string()], "fake-gitlab-admin-2".to_string()));

    // Moving the date moves health; audited, never with the credential.
    api.ok(Method::PATCH, &at, Some(json!({ "credential_expires_at": now + 100 * day }))).await;
    assert!(expiring(&api.ok(Method::GET, "/health", None).await).is_empty());
    let audit = api.ok(Method::GET, "/audit", None).await;
    assert!(audit.to_string().contains("changed SaaS rotator gl-ops's credential expiry") && !audit.to_string().contains("fake-gitlab-admin-2"), "{audit}");
}

#[tokio::test]
async fn openai_service_account_keys_rotate_and_a_leaked_one_is_revoked_at_once() {
    let api = Api::new().await;
    key_vendor(&api, "openai", json!({ "project_id": "proj_rollfake" }), "fake-openai-admin", "OPENAI_API_KEY", json!({})).await;
    assert_eq!(api.values("production", "/").await["values"]["OPENAI_API_KEY"], "fake-openai-key-1");
    api.ok(Method::PUT, "/environments/production/secrets/OPENAI_SERVICE_ACCOUNT_ID", Some(json!({ "link": { "kind": "saas", "ref": api.ok(Method::GET, "/saas", None).await[0]["id"], "field": "id", "mode": "managed" } }))).await;
    assert_eq!(api.values("production", "/").await["values"]["OPENAI_SERVICE_ACCOUNT_ID"], "svc_acct_fake1", "one service account for both keys");

    // A leak: rotate, then end the retired key's lease instead of waiting out the grace period.
    api.ok(Method::POST, "/environments/production/secrets/OPENAI_API_KEY/rotate", None).await;
    assert_eq!(api.values("production", "/").await["values"]["OPENAI_API_KEY"], "fake-openai-key-2");
    assert_eq!(*api.sent.10.live.lock().unwrap(), ["svc_acct_fake1", "svc_acct_fake2"]);
    let leases = api.ok(Method::GET, "/leases", None).await;
    let retired = leases.as_array().unwrap().iter().find(|l| l["kind"] == "retired" && l["revoked_at"].is_null()).unwrap();
    api.ok(Method::DELETE, &format!("/leases/{}", retired["id"].as_str().unwrap()), None).await;
    assert_eq!(*api.sent.10.live.lock().unwrap(), ["svc_acct_fake2"], "deleting the service account revoked its key");
}

#[tokio::test]
async fn datadog_keys_rotate_and_a_refused_rotation_is_rotation_failed() {
    let api = Api::new().await;
    let (s, _) = api.call(Method::POST, "/saas", Some(json!({ "name": "dd", "vendor": "datadog", "target": { "site": "datadog.attacker.example", "key": "api" }, "credential": "a:b" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "only Datadog's own sites");
    key_vendor(&api, "datadog", json!({ "site": "datadoghq.eu", "key": "api" }), "fake-dd-api:fake-dd-app", "DD_API_KEY", json!({})).await;
    assert_eq!(api.values("production", "/").await["values"]["DD_API_KEY"], "fake-dd-api_keys-1");
    // Application keys of the credential's user, with the scopes asked for.
    let app = api.ok(Method::POST, "/saas", Some(json!({ "name": "dd-app", "vendor": "datadog", "target": { "site": "datadoghq.eu", "key": "application", "scopes": ["dashboards_read"] }, "credential": "fake-dd-api:fake-dd-app" }))).await;
    api.ok(Method::PUT, "/environments/production/secrets/DD_APP_KEY", Some(json!({ "link": { "kind": "saas", "ref": app["id"], "field": "secret", "mode": "managed" } }))).await;
    assert_eq!(api.values("production", "/").await["values"]["DD_APP_KEY"], "fake-dd-application_keys-2");
    assert_eq!(api.sent.10.bodies.lock().unwrap()[1]["data"]["attributes"]["scopes"], json!(["dashboards_read"]));

    api.ok(Method::POST, "/environments/production/secrets/DD_API_KEY/rotate", None).await;
    assert_eq!(api.values("production", "/").await["values"]["DD_API_KEY"], "fake-dd-api_keys-3");
    end_grace(&api).await;
    let first = "00000000-0000-0000-0000-000000000001";
    assert!(!api.sent.10.live.lock().unwrap().iter().any(|k| k == first), "the old API key is deleted");

    // The admin key revoked at Datadog: the scheduled rotation fails, and says so.
    api.sent.10.refuse.store(true, std::sync::atomic::Ordering::Relaxed);
    let p = api.project.clone();
    api.db
        .write(move |c| {
            let state: String = c.query_row("SELECT link_state FROM secrets WHERE project_id = ? AND key = 'DD_API_KEY'", [p.as_str()], |r| r.get(0))?;
            let mut state: Value = serde_json::from_str(&state).unwrap();
            state["issued_at"] = json!(0);
            c.execute("UPDATE secrets SET link_state = ?, link = replace(link, '\"mode\":\"managed\"', '\"mode\":\"managed\",\"options\":{\"rotate_days\":1}') WHERE project_id = ? AND key = 'DD_API_KEY'", [state.to_string().as_str(), p.as_str()])
        })
        .unwrap();
    let start = api.ok(Method::GET, "/events?latest=1", None).await["next"].as_i64().unwrap();
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap();
    let events = api.ok(Method::GET, &format!("/events?after={start}&kinds=rotation.failed"), None).await;
    assert_eq!(events["events"][0]["key"], "DD_API_KEY", "{events}");
    assert!(events.to_string().contains("datadog answered 403"), "{events}");
    assert_eq!(api.values("production", "/").await["values"]["DD_API_KEY"], "fake-dd-api_keys-3", "the key in use stays");
}

#[tokio::test]
async fn cloudflare_tokens_copy_the_template_and_the_old_one_is_deleted_after_the_grace_period() {
    let api = Api::new().await;
    key_vendor(&api, "cloudflare", json!({ "account_id": "rollfakeaccount", "template": "tmplrollfake" }), "fake-cf-admin", "CLOUDFLARE_API_TOKEN", json!({})).await;
    assert_eq!(api.values("production", "/").await["values"]["CLOUDFLARE_API_TOKEN"], "fake-cf-token-1");
    // A new token, not Cloudflare's roll: the old value keeps working through the grace period.
    api.ok(Method::POST, "/environments/production/secrets/CLOUDFLARE_API_TOKEN/rotate", None).await;
    assert_eq!(api.values("production", "/").await["values"]["CLOUDFLARE_API_TOKEN"], "fake-cf-token-2");
    assert_eq!(*api.sent.10.live.lock().unwrap(), ["cftokfake1", "cftokfake2"]);
    end_grace(&api).await;
    assert_eq!(*api.sent.10.live.lock().unwrap(), ["cftokfake2"]);
}

#[tokio::test]
async fn databricks_tokens_rotate_and_the_old_one_is_revoked_after_the_grace_period() {
    let api = Api::new().await;
    let (s, _) = api.call(Method::POST, "/saas", Some(json!({ "name": "dbx", "vendor": "databricks", "target": { "host": "192.168.0.10" }, "credential": "x" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a workspace host is a hostname");
    key_vendor(&api, "databricks", json!({ "host": "dbc-rollfake.cloud.databricks.com", "lifetime_days": 30 }), "fake-dbx-admin", "DATABRICKS_TOKEN", json!({ "rotate_days": 16 })).await;
    assert_eq!(api.values("production", "/").await["values"]["DATABRICKS_TOKEN"], "fake-dbx-token-1");
    api.ok(Method::POST, "/environments/production/secrets/DATABRICKS_TOKEN/rotate", None).await;
    assert_eq!(api.values("production", "/").await["values"]["DATABRICKS_TOKEN"], "fake-dbx-token-2");
    assert_eq!(*api.sent.10.live.lock().unwrap(), ["dbxtokfake1", "dbxtokfake2"]);
    end_grace(&api).await;
    assert_eq!(*api.sent.10.live.lock().unwrap(), ["dbxtokfake2"]);
}

#[tokio::test]
async fn confluent_api_keys_rotate_for_the_same_owner_and_resource() {
    let api = Api::new().await;
    let target = json!({ "owner": "sa-rollfake", "resource": "lkc-rollfake", "environment": "env-rollfake" });
    let r = key_vendor(&api, "confluent", target, "FAKECONFLUENTADMIN:fake-confluent-admin", "CONFLUENT_API_SECRET", json!({})).await;
    api.ok(Method::PUT, "/environments/production/secrets/CONFLUENT_API_KEY", Some(json!({ "link": { "kind": "saas", "ref": r["id"], "field": "id", "mode": "managed" } }))).await;
    let pair = |v: Value| (v["values"]["CONFLUENT_API_KEY"].clone(), v["values"]["CONFLUENT_API_SECRET"].clone());
    assert_eq!(pair(api.values("production", "/").await), (json!("CONFLUENTFAKE1"), json!("fake-confluent-secret-1")));
    api.ok(Method::POST, "/environments/production/secrets/CONFLUENT_API_SECRET/rotate", None).await;
    assert_eq!(pair(api.values("production", "/").await), (json!("CONFLUENTFAKE2"), json!("fake-confluent-secret-2")));
    end_grace(&api).await;
    assert_eq!(*api.sent.10.live.lock().unwrap(), ["CONFLUENTFAKE2"]);
}

#[tokio::test]
async fn a_database_outside_roll_hands_out_logins_in_turn_and_per_lease() {
    let api = Api::new().await;
    // A password with a # unencoded: parsers disagree on where it ends, so it's refused, never
    // shown as the host.
    let (s, e) = api.call(Method::POST, "/outside-databases", Some(json!({ "name": "x", "url": "postgres://admin:abc#def@db.example/app", "roles": ["app_blue", "app_green"] }))).await;
    assert!(s == StatusCode::BAD_REQUEST && !e.to_string().contains("abc"), "{e}");
    let new = json!({ "name": "billing", "url": "postgres://admin:hunter2@db.example:5432/app?sslmode=require", "roles": ["app_blue", "app_green"], "group": "app_rw" });
    let d = api.ok(Method::POST, "/outside-databases", Some(new)).await;
    assert_eq!((d["host"].as_str(), d.get("url")), (Some("db.example:5432"), None), "the admin URL stays sealed: {d}");
    let link = |field: &str| json!({ "kind": "outside_database", "ref": d["id"], "field": field, "mode": "managed" });
    api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": [{ "op": "set", "key": "DB_USER", "link": link("username") }, { "op": "set", "key": "DB_PASSWORD", "link": link("password") }] }))).await;
    let login = || async {
        let v = api.values("production", "/").await["values"].clone();
        (v["DB_USER"].as_str().unwrap().to_string(), v["DB_PASSWORD"].as_str().unwrap().to_string())
    };
    let password = |role: &str| api.outside.0.lock().unwrap().get(role).cloned();
    let blue = login().await;
    assert_eq!((blue.0.as_str(), password("app_blue")), ("app_blue", Some(blue.1.clone())));

    // Each rotation hands out the other role; the one in use keeps working through the grace.
    let rotate = || api.call(Method::POST, "/environments/production/secrets/DB_PASSWORD/rotate", None);
    assert!(rotate().await.0.is_success());
    let green = login().await;
    assert_eq!((green.0.as_str(), password("app_green"), password("app_blue")), ("app_green", Some(green.1.clone()), Some(blue.1.clone())));
    // Both roles are taken (green in use, blue in its grace): another rotation waits, resetting
    // nothing, even once the issue's own short hold on green is over.
    let id = d["id"].as_str().unwrap().to_string();
    let unhold = || {
        let id = id.clone();
        api.db
            .write(move |c| {
                let s: String = c.query_row("SELECT state FROM outside_dbs WHERE id = ?", [id.as_str()], |r| r.get(0))?;
                let mut v: Value = serde_json::from_str(&s).unwrap();
                v["held"] = json!({});
                c.execute("UPDATE outside_dbs SET state = ? WHERE id = ?", [v.to_string().as_str(), id.as_str()])
            })
            .unwrap()
    };
    unhold();
    assert_eq!(rotate().await.0, StatusCode::CONFLICT);
    assert_eq!((login().await, password("app_blue")), (green.clone(), Some(blue.1.clone())));
    // Blue's grace ends: its password goes, and it's free again.
    let expire = |cond: &'static str| {
        let p = api.project.clone();
        api.db.write(move |c| c.execute(&format!("UPDATE leases SET expires_at = 0 WHERE project_id = ? AND {cond}"), [p.as_str()])).unwrap();
        let s = api.secrets.clone();
        async move { tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap() }
    };
    expire("1 = 1").await;
    assert_ne!(password("app_blue"), Some(blue.1.clone()));
    // A rotation the server refused changes nothing, and the next one takes the free role.
    api.outside.1.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(!rotate().await.0.is_success());
    api.outside.1.store(false, std::sync::atomic::Ordering::Relaxed);
    assert_eq!((login().await, password("app_green")), (green.clone(), Some(green.1.clone())));
    assert!(rotate().await.0.is_success());
    let blue2 = login().await;
    assert_eq!((blue2.0.as_str(), password("app_green")), ("app_blue", Some(green.1.clone())));
    // A retirement that comes late (blue's first) doesn't undo blue's reissue.
    let (p, late) = (api.project.clone(), json!({ "kind": "outside_database", "ref": d["id"], "field": "username", "mode": "managed" }).to_string());
    api.db
        .write(move |c| {
            c.execute(
                "INSERT INTO leases (id, project_id, env_id, key, kind, link, handle, consumer, issued_at, expires_at, revoked_at) VALUES ('lse_late', ?, '', '', 'retired', ?, '{\"role\":\"app_blue\",\"generation\":1}', 'test', 0, 1, NULL)",
                [p.as_str(), late.as_str()],
            )
        })
        .unwrap();
    expire("id = 'lse_late'").await;
    assert_eq!(password("app_blue"), Some(blue2.1.clone()));
    // Its two roles serve one environment.
    let (s, v) = api.call(Method::PUT, "/environments/staging/secrets/DB_USER", Some(json!({ "link": link("username") }))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");

    // Dynamic: each resolve is a login of its own in the group, valid until its lease ends (the
    // server enforces it), renewed with the lease, dropped when it ends.
    let dynamic = |field: &str| json!({ "kind": "outside_database", "ref": d["id"], "field": field, "mode": "dynamic", "options": { "ttl_minutes": 15 } });
    api.ok(Method::POST, "/environments/staging/changes", Some(json!({ "ops": [{ "op": "set", "key": "TMP_USER", "link": dynamic("username") }, { "op": "set", "key": "TMP_PASSWORD", "link": dynamic("password") }] }))).await;
    let a = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "job:report" }))).await;
    let b = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "job:report" }))).await;
    let user = a["values"]["TMP_USER"].as_str().unwrap().to_string();
    assert_ne!(user, b["values"]["TMP_USER"].as_str().unwrap());
    assert_eq!(password(&user).as_deref(), a["values"]["TMP_PASSWORD"].as_str());
    let (group, until) = api.outside.2.lock().unwrap()[&user].clone();
    let lease = a["leases"][0].as_str().unwrap();
    let expires = api.ok(Method::GET, "/leases", None).await.as_array().unwrap().iter().find(|l| l["id"] == lease).unwrap()["expires_at"].as_i64().unwrap();
    assert!(group == "app_rw" && until > expires && until <= expires + 10 * 60_000, "{group} {until} {expires}");
    let renewed = api.ok(Method::POST, &format!("/leases/{lease}/renew"), Some(json!({ "ttl_minutes": 120 }))).await;
    assert!(api.outside.2.lock().unwrap()[&user].1 > renewed["expires_at"].as_i64().unwrap());
    api.ok(Method::DELETE, &format!("/leases/{lease}"), None).await;
    assert_eq!(password(&user), None);

    let (s, _) = api.call(Method::DELETE, &format!("/outside-databases/{}", d["id"].as_str().unwrap()), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "its keys keep it");

    // The admin's own password rotates to one nobody knows but roll, which keeps using it; the
    // registration is edited in place (a name, a URL whose password changed elsewhere), links intact.
    let id = d["id"].as_str().unwrap();
    api.ok(Method::POST, &format!("/outside-databases/{id}/rotate-admin"), None).await;
    let admin = api.outside.0.lock().unwrap()["admin"].clone();
    assert!(admin.len() == 40 && admin != "hunter2");
    assert!(api.ok(Method::POST, &format!("/outside-databases/{id}/rotate-admin"), None).await["host"] == "db.example:5432", "a second rotation logs in with the first's password");
    assert_ne!(api.outside.0.lock().unwrap()["admin"], admin);
    // The server takes a new password but its reply is lost: roll keeps the one that logs in.
    api.outside.3.store(true, std::sync::atomic::Ordering::Relaxed);
    api.ok(Method::POST, &format!("/outside-databases/{id}/rotate-admin"), None).await;
    api.outside.3.store(false, std::sync::atomic::Ordering::Relaxed);
    api.ok(Method::POST, &format!("/outside-databases/{id}/rotate-admin"), None).await;
    // Lost reply and an unreachable server: roll keeps both URLs. Another rotation meanwhile is
    // refused (the first may still land), and the next use that finds only the new one logging in
    // finishes the rotation.
    let set = |flag: &std::sync::atomic::AtomicBool, on: bool| flag.store(on, std::sync::atomic::Ordering::Relaxed);
    set(&api.outside.3, true);
    set(&api.outside.4, true);
    assert!(!api.call(Method::POST, &format!("/outside-databases/{id}/rotate-admin"), None).await.0.is_success());
    set(&api.outside.3, false);
    set(&api.outside.4, false);
    assert_eq!(api.call(Method::POST, &format!("/outside-databases/{id}/rotate-admin"), None).await.0, StatusCode::CONFLICT, "another rotation is in flight");
    let lease = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "job:report" }))).await;
    assert!(password(lease["values"]["TMP_USER"].as_str().unwrap()).is_some(), "issuing finished the interrupted rotation");
    api.ok(Method::POST, &format!("/outside-databases/{id}/rotate-admin"), None).await;
    // Its logins live on this server: it moves to another only once nothing links to it.
    let (s, e) = api.call(Method::PATCH, &format!("/outside-databases/{id}"), Some(json!({ "url": "postgres://admin:x@other.example/app" }))).await;
    assert!(s == StatusCode::CONFLICT && !e.to_string().contains(":x@"), "{e}");
    // A password changed elsewhere: the registration takes the new URL, checked first.
    api.outside.0.lock().unwrap().insert("admin".into(), "reset".into());
    let (s, _) = api.call(Method::PATCH, &format!("/outside-databases/{id}"), Some(json!({ "url": "postgres://admin:wrong@db.example:5432/app" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a URL that doesn't log in isn't kept");
    let renamed = api.ok(Method::PATCH, &format!("/outside-databases/{id}"), Some(json!({ "name": "billing-eu", "url": "postgres://admin:reset@db.example:5432/app" }))).await;
    assert_eq!((renamed["name"].as_str(), renamed["host"].as_str()), (Some("billing-eu"), Some("db.example:5432")));
    let lease = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "job:report" }))).await;
    assert!(password(lease["values"]["TMP_USER"].as_str().unwrap()).is_some(), "its links carry on through the new URL");
}

#[tokio::test]
async fn a_kubernetes_cluster_issues_short_lived_service_account_tokens() {
    let api = Api::new().await;
    let (s, _) = api.call(Method::POST, "/kubernetes", Some(json!({ "name": "prod", "api_url": KUBE, "token": "not-the-admin-token" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "the API server must take the admin token");
    let k = api.ok(Method::POST, "/kubernetes", Some(json!({ "name": "prod", "api_url": format!("{KUBE}/"), "ca_pem": KUBE_CA, "token": "fake-admin-token" }))).await;
    assert_eq!((k["api_url"].as_str(), k["has_ca"].as_bool()), (Some(KUBE), Some(true)), "{k}");

    // Dynamic only, naming the service account; a token that can't be revoked has a short lease.
    let link = |field: &str, ttl: u64| json!({ "kind": "kubernetes", "ref": k["id"], "field": field, "mode": "dynamic", "options": { "namespace": "ci", "service_account": "deployer", "audiences": ["vault"], "ttl_minutes": ttl } });
    let (s, _) = api.call(Method::PUT, "/environments/staging/secrets/KUBE_TOKEN", Some(json!({ "link": link("token", 7 * 24 * 60) }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a week-long token is refused");
    let ops: Vec<Value> = [("KUBE_TOKEN", "token"), ("KUBE_API_URL", "api_url"), ("KUBE_CA", "ca")].iter().map(|(key, field)| json!({ "op": "set", "key": key, "link": link(field, 20) })).collect();
    api.ok(Method::POST, "/environments/staging/changes", Some(json!({ "ops": ops }))).await;

    // A resolve issues one token for the three keys, for the lease's TTL and the audiences, through the cluster's CA.
    let r = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "job:deploy" }))).await;
    assert_eq!((r["values"]["KUBE_TOKEN"].as_str(), r["values"]["KUBE_API_URL"].as_str(), r["values"]["KUBE_CA"].as_str()), (Some("fake-sa-token-1"), Some(KUBE), Some(KUBE_CA)), "{r}");
    let asked: Vec<Value> = api.sent.0.lock().unwrap().iter().filter_map(|b| b.get("kube").cloned()).collect();
    assert_eq!(asked, [json!({ "ca": KUBE_CA, "request": { "apiVersion": "authentication.k8s.io/v1", "kind": "TokenRequest", "spec": { "expirationSeconds": 1200, "audiences": ["vault"] } } })]);
    let lease_id = r["leases"][0].as_str().unwrap().to_string();
    let leases = api.ok(Method::GET, "/leases", None).await;
    let lease = leases.as_array().unwrap().iter().find(|l| l["id"] == lease_id.as_str()).unwrap().clone();
    assert_eq!(lease["expires_at"].as_i64().unwrap() - lease["issued_at"].as_i64().unwrap(), 20 * 60_000, "{lease}");

    // Code nobody vetted gets none, and no request reaches the cluster.
    let check = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "check:fork#1", "untrusted": true }))).await;
    assert!(check["values"].get("KUBE_TOKEN").is_none() && check["leases"] == json!([]), "{check}");
    assert_eq!(api.sent.0.lock().unwrap().iter().filter(|b| b.get("kube").is_some()).count(), 1);

    // Ending the lease can't end the token: the audit row says so.
    api.ok(Method::DELETE, &format!("/leases/{lease_id}"), None).await;
    let listed = api.ok(Method::GET, "/kubernetes", None).await;
    let audit = api.ok(Method::GET, "/audit", None).await;
    assert!(audit.to_string().contains("works until it expires"), "{audit}");
    for shown in [&k, &listed, &audit] {
        assert!(!shown.to_string().contains("fake-admin-token") && !shown.to_string().contains("fake-cluster-ca"), "never the admin token or the CA: {shown}");
    }
    let (s, _) = api.call(Method::DELETE, &format!("/kubernetes/{}", k["id"].as_str().unwrap()), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "its keys keep it");
}

#[tokio::test]
async fn a_kubernetes_link_with_role_rules_makes_a_service_account_per_lease_and_deletes_it() {
    let api = Api::new().await;
    let k = api.ok(Method::POST, "/kubernetes", Some(json!({ "name": "prod", "api_url": KUBE, "token": "fake-admin-token" }))).await;
    let rules = json!([{ "apiGroups": [""], "resources": ["pods", "pods/log"], "verbs": ["get", "list"] }, { "apiGroups": ["apps"], "resources": ["deployments"], "verbs": ["patch"], "resourceNames": ["web"] }]);
    let link = |options: Value| json!({ "link": { "kind": "kubernetes", "ref": k["id"], "field": "token", "mode": "dynamic", "options": options } });

    // One or the other: an existing account, or rules for one made per lease; rules are Kubernetes'.
    for (options, why) in [
        (json!({ "namespace": "ci", "service_account": "deployer", "role_rules": rules }), "exclusive"),
        (json!({ "namespace": "ci", "service_account": "deployer", "cluster_wide": true }), "cluster_wide needs rules"),
        (json!({ "namespace": "ci", "role_rules": [{ "apiGroups": [""], "resources": ["pods"], "verbs": ["GET"] }] }), "verbs are lowercase"),
        (json!({ "namespace": "ci", "role_rules": [{ "apiGroups": [""], "resources": ["pods"], "verbs": ["get"], "nonResourceURLs": ["/healthz"] }] }), "only the four fields"),
        (json!({ "namespace": "ci", "role_rules": [] }), "at least one rule"),
    ] {
        let (s, e) = api.call(Method::PUT, "/environments/staging/secrets/KUBE_TOKEN", Some(link(options))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{why}: {e}");
    }

    // A resolve makes the account, its Role and RoleBinding, labelled with the lease, then a token for it.
    api.ok(Method::PUT, "/environments/staging/secrets/KUBE_TOKEN", Some(link(json!({ "namespace": "ci", "role_rules": rules, "ttl_minutes": 20 })))).await;
    let r = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "job:deploy" }))).await;
    let lease = r["leases"][0].as_str().unwrap().to_string();
    let name = format!("roll-{}", lease.strip_prefix("lse_").unwrap());
    assert!(r["values"]["KUBE_TOKEN"].as_str().unwrap().starts_with("fake-sa-token-"), "{r}");
    let rbac = "/apis/rbac.authorization.k8s.io/v1";
    let made = |kept: &[Value]| kube_objects(kept).into_iter().collect::<Vec<_>>();
    let expected = [format!("/api/v1/namespaces/ci/serviceaccounts/{name}"), format!("{rbac}/namespaces/ci/rolebindings/{name}"), format!("{rbac}/namespaces/ci/roles/{name}")];
    assert_eq!(made(&api.sent.0.lock().unwrap()), expected);
    let bodies: Vec<Value> = api.sent.0.lock().unwrap().iter().filter_map(|b| b["kube_obj"].get("body").cloned()).collect();
    for b in &bodies {
        assert_eq!(b["metadata"]["labels"], json!({ "app.kubernetes.io/managed-by": "roll-secrets", "roll.sh/lease": lease }), "{b}");
    }
    assert_eq!((bodies[1]["kind"].as_str(), &bodies[1]["rules"]), (Some("Role"), &rules));
    assert_eq!(bodies[2]["roleRef"], json!({ "apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": name }));
    assert_eq!(bodies[2]["subjects"], json!([{ "kind": "ServiceAccount", "name": name, "namespace": "ci" }]));

    // Revoking deletes them, binding first: the token ends with its account.
    api.ok(Method::DELETE, &format!("/leases/{lease}"), None).await;
    let deleted: Vec<String> = api.sent.0.lock().unwrap().iter().filter_map(|b| b["kube_obj"]["delete"].as_str().map(String::from)).collect();
    assert_eq!(deleted, [expected[1].clone(), expected[2].clone(), expected[0].clone()]);
    let audit = api.ok(Method::GET, "/audit", None).await;
    assert!(!audit.to_string().contains("works until it expires"), "{audit}");

    // Cluster-wide: a ClusterRole and a ClusterRoleBinding.
    api.ok(Method::PUT, "/environments/staging/secrets/KUBE_TOKEN", Some(link(json!({ "namespace": "ci", "role_rules": rules, "cluster_wide": true })))).await;
    let r = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "job:deploy" }))).await;
    let name = format!("roll-{}", r["leases"][0].as_str().unwrap().strip_prefix("lse_").unwrap());
    assert_eq!(made(&api.sent.0.lock().unwrap()), [format!("/api/v1/namespaces/ci/serviceaccounts/{name}"), format!("{rbac}/clusterrolebindings/{name}"), format!("{rbac}/clusterroles/{name}")]);
    api.ok(Method::DELETE, &format!("/leases/{}", r["leases"][0].as_str().unwrap()), None).await;
    assert!(made(&api.sent.0.lock().unwrap()).is_empty());

    // A step that fails deletes what the issue made, and names the step and the status; no token.
    api.ok(Method::PUT, "/environments/staging/secrets/KUBE_TOKEN", Some(link(json!({ "namespace": "flaky", "role_rules": rules })))).await;
    let tokens_before = api.sent.0.lock().unwrap().iter().filter(|b| b.get("kube").is_some()).count();
    let r = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "job:deploy" }))).await;
    assert!(r["values"].get("KUBE_TOKEN").is_none_or(|v| v == "") && r["leases"] == json!([]), "{r}");
    let problem = r["problems"][0]["problem"].as_str().unwrap();
    assert!(problem.contains("403 to creating the RoleBinding") && problem.contains("create on rolebindings") && !problem.contains("couldn't delete"), "{problem}");
    let kept = api.sent.0.lock().unwrap();
    assert!(kube_objects(&kept).is_empty(), "the account and the role are deleted again");
    assert_eq!(kept.iter().filter(|b| b["kube_obj"]["delete"].as_str().is_some_and(|p| p.contains("/namespaces/flaky/"))).count(), 2);
    assert_eq!(kept.iter().filter(|b| b.get("kube").is_some()).count(), tokens_before, "no token asked for");
}

#[tokio::test]
async fn a_directory_hands_out_accounts_in_turn_and_a_user_per_lease() {
    let api = Api::new().await;
    let (blue, green, base, group) = ("cn=app-blue,ou=svc,dc=example,dc=com", "cn=app-green,ou=svc,dc=example,dc=com", "ou=roll,dc=example,dc=com", "cn=readers,ou=groups,dc=example,dc=com");
    for dn in [blue, green, base, group] {
        api.directory.entries.lock().unwrap().insert(dn.into(), ("set-by-hand".into(), vec![], String::new()));
    }
    let new = |pw: &str| json!({ "name": "corp", "url": "ldaps://ldap.example:636", "bind_dn": "cn=roll,dc=example,dc=com", "bind_password": pw, "flavor": "openldap", "user_base_dn": base, "managed_accounts": [blue, green], "groups": [group] });
    // Registering binds once to prove the credentials; a refusal says why, never the password.
    let (s, e) = api.call(Method::POST, "/directories", Some(new("fake-wrong-pw"))).await;
    assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains("invalid credentials") && !e.to_string().contains("fake-wrong-pw"), "{e}");
    let d = api.ok(Method::POST, "/directories", Some(new("fake-bind-pw"))).await;
    assert_eq!(api.directory.binds.load(std::sync::atomic::Ordering::Relaxed), 2);
    let id = d["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("dir_") && d.get("bind_password").is_none(), "{d}");

    // Managed: two accounts take turns; the one in use keeps its password through the grace.
    let link = |field: &str| json!({ "kind": "directory", "ref": id, "field": field, "mode": "managed" });
    api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": [{ "op": "set", "key": "LDAP_BIND_DN", "link": link("username") }, { "op": "set", "key": "LDAP_PASSWORD", "link": link("password") }] }))).await;
    let login = || async {
        let v = api.values("production", "/").await["values"].clone();
        (v["LDAP_BIND_DN"].as_str().unwrap().to_string(), v["LDAP_PASSWORD"].as_str().unwrap().to_string())
    };
    let first = login().await;
    assert_eq!((first.0.as_str(), api.directory.password(blue)), (blue, Some(first.1.clone())));
    let rotate = || api.call(Method::POST, "/environments/production/secrets/LDAP_PASSWORD/rotate", None);
    assert!(rotate().await.0.is_success());
    let second = login().await;
    assert_eq!((second.0.as_str(), api.directory.password(green), api.directory.password(blue)), (green, Some(second.1.clone()), Some(first.1.clone())));
    let unhold = || {
        let id = id.clone();
        api.db
            .write(move |c| {
                let s: String = c.query_row("SELECT state FROM directories WHERE id = ?", [id.as_str()], |r| r.get(0))?;
                let mut v: Value = serde_json::from_str(&s).unwrap();
                v["held"] = json!({});
                c.execute("UPDATE directories SET state = ? WHERE id = ?", [v.to_string().as_str(), id.as_str()])
            })
            .unwrap()
    };
    unhold();
    assert_eq!(rotate().await.0, StatusCode::CONFLICT, "blue is still in its grace");
    let expire = || {
        let p = api.project.clone();
        api.db.write(move |c| c.execute("UPDATE leases SET expires_at = 0 WHERE project_id = ? AND kind = 'retired'", [p.as_str()])).unwrap();
        let s = api.secrets.clone();
        async move { tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap() }
    };
    expire().await;
    assert_ne!(api.directory.password(blue), Some(first.1.clone()), "its grace over, blue's password is one nobody knows");
    assert_eq!(api.directory.password(green), Some(second.1.clone()));

    // Dynamic: each lease is a user of its own under the base, in the groups, named after the
    // lease; ending the lease deletes it.
    let dynamic = |field: &str| json!({ "kind": "directory", "ref": id, "field": field, "mode": "dynamic", "options": { "ttl_minutes": 15 } });
    api.ok(Method::POST, "/environments/staging/changes", Some(json!({ "ops": [{ "op": "set", "key": "TMP_DN", "link": dynamic("username") }, { "op": "set", "key": "TMP_PASSWORD", "link": dynamic("password") }] }))).await;
    let r = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "job:sync" }))).await;
    let lease = r["leases"][0].as_str().unwrap().to_string();
    let user = r["values"]["TMP_DN"].as_str().unwrap().to_string();
    assert_eq!(user, format!("cn=roll-{lease},{base}"));
    let (pw, groups, description) = api.directory.entries.lock().unwrap()[&user].clone();
    assert!(Some(pw.as_str()) == r["values"]["TMP_PASSWORD"].as_str() && groups == [group] && description.contains(&lease), "{groups:?} {description}");
    api.ok(Method::DELETE, &format!("/leases/{lease}"), None).await;
    assert!(api.directory.password(&user).is_none());
    // A user made halfway (its group refused) is deleted again; nothing is handed out.
    api.directory.fail_groups.store(true, std::sync::atomic::Ordering::Relaxed);
    let failed = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "job:sync" }))).await;
    api.directory.fail_groups.store(false, std::sync::atomic::Ordering::Relaxed);
    assert!(failed["leases"] == json!([]) && !api.directory.entries.lock().unwrap().keys().any(|dn| dn.starts_with("cn=roll-")), "{failed}");
    // Code nobody vetted gets no user at all.
    let check = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "check:fork#1", "untrusted": true }))).await;
    assert!(check["values"].get("TMP_PASSWORD").is_none() && check["leases"] == json!([]), "{check}");
    assert!(!api.directory.entries.lock().unwrap().keys().any(|dn| dn.starts_with("cn=roll-")));

    // Active Directory's logins also have an account name and a UPN (OpenLDAP's don't).
    let (s, e) = api.call(Method::PUT, "/environments/production/secrets/LDAP_UPN", Some(json!({ "link": link("upn") }))).await;
    assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains("only Active Directory"), "{e}");
    let svc = "cn=svc-reports,ou=svc,dc=example,dc=com";
    api.directory.entries.lock().unwrap().insert(svc.into(), ("set-by-hand".into(), vec![], String::new()));
    let ad = api.ok(Method::POST, "/directories", Some(json!({ "name": "ad", "url": "ldaps://ldap.example:636", "bind_dn": "cn=roll,dc=example,dc=com", "bind_password": "fake-bind-pw", "flavor": "active_directory", "user_base_dn": base, "managed_accounts": [svc] }))).await;
    // "Link a resource" offers what each directory issues: an account name and a UPN only on AD.
    let targets = api.ok(Method::GET, "/links", None).await;
    let fields = |r: &str| targets.as_array().unwrap().iter().find(|t| t["ref"] == r).unwrap()["fields"].as_array().unwrap().iter().map(|f| f["field"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!((fields(&id), fields(ad["id"].as_str().unwrap())), (vec!["username".to_string(), "password".into()], vec!["username".to_string(), "password".into(), "upn".into(), "account_name".into()]));
    let ad_link = |field: &str, mode: &str| json!({ "kind": "directory", "ref": ad["id"], "field": field, "mode": mode, "options": if mode == "dynamic" { json!({ "ttl_minutes": 15 }) } else { json!({}) } });
    let ops = |mode: &str| {
        let ops: Vec<Value> = ["username", "password", "upn", "account_name"].iter().map(|f| json!({ "op": "set", "key": format!("AD_{}_{}", mode.to_uppercase(), f.to_uppercase()), "link": ad_link(f, mode) })).collect();
        json!({ "ops": ops })
    };
    api.ok(Method::POST, "/environments/production/changes", Some(ops("managed"))).await;
    let v = api.values("production", "/").await["values"].clone();
    assert_eq!((v["AD_MANAGED_USERNAME"].as_str(), v["AD_MANAGED_ACCOUNT_NAME"].as_str(), v["AD_MANAGED_UPN"].as_str()), (Some(svc), Some("svc-reports"), Some("svc-reports@corp.example")));
    api.ok(Method::POST, "/environments/staging/changes", Some(ops("dynamic"))).await;
    let r = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "job:sync" }))).await;
    let lease = r["leases"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).find(|l| r["values"]["AD_DYNAMIC_USERNAME"].as_str().unwrap().contains(l)).unwrap();
    let sam = format!("roll-{}", &lease[4..19]);
    assert_eq!((r["values"]["AD_DYNAMIC_ACCOUNT_NAME"].as_str(), r["values"]["AD_DYNAMIC_UPN"].as_str()), (Some(sam.as_str()), Some(format!("{sam}@example.com").as_str())));

    // The bind password is in no answer and no audit row.
    let listed = api.ok(Method::GET, "/directories", None).await;
    let audit = api.ok(Method::GET, "/audit", None).await;
    let p = api.project.clone();
    let rows: Vec<String> = api.db.read(move |c| c.query("SELECT summary || keys || action FROM audit WHERE project_id = ?", [p.as_str()], |r| r.get(0))).unwrap();
    assert!(audit.to_string().contains("directory.add") && !format!("{listed}{audit}{rows:?}").contains("fake-bind-pw"));
    // In use, it can't go.
    let (s, _) = api.call(Method::DELETE, &format!("/directories/{id}"), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "its keys keep it");
}

#[tokio::test]
async fn a_snowflake_account_rotates_key_slots_in_turn_and_makes_a_user_per_lease() {
    use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey};
    let api = Api::new().await;
    let sf = &api.sent.6;
    // Throwaway keys made for this test.
    let pem = |k: &rsa::RsaPrivateKey| k.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap().to_string();
    let admin = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
    let stranger = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
    sf.trust(&admin);
    sf.users.lock().unwrap().insert("SVC_APP".into(), SnowflakeUser { keys: [Some("set-by-hand".into()), None], ..Default::default() });
    *sf.roles.lock().unwrap() = vec!["ANALYST".into(), "LOADER".into()];
    let new = |key: &str, account: &str, roles: Value| json!({ "name": "warehouse", "account": account, "admin_user": "roll_admin", "admin_role": "roll_admin", "admin_private_key": key, "managed_user": "svc_app", "roles": roles });
    // The account and every name are checked before anything is sent; a refused sign-in says so,
    // never the key.
    for (body, why) in [(new(&pem(&admin), "evil.example/x", json!([])), "account"), (new(&pem(&admin), "myorg-acct", json!(["accountadmin"])), "ACCOUNTADMIN"), (new(&pem(&admin), "myorg-acct", json!(["x\" OR 1=1"])), "unquoted")] {
        let (s, e) = api.call(Method::POST, "/snowflake-accounts", Some(body)).await;
        assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains(why), "{e}");
    }
    assert!(sf.statements.lock().unwrap().is_empty());
    let (s, e) = api.call(Method::POST, "/snowflake-accounts", Some(new(&pem(&stranger), "myorg-acct", json!(["analyst"])))).await;
    assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains("refused roll's sign-in") && !e.to_string().contains("PRIVATE KEY"), "{e}");
    let a = api.ok(Method::POST, "/snowflake-accounts", Some(new(&pem(&admin), "myorg-acct", json!(["analyst", "loader"])))).await;
    let id = a["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("sfa_") && a.get("admin_private_key").is_none() && a["admin_key_fp"] == snowflake_public(&admin).1 && a["managed_user"] == "SVC_APP" && a["roles"] == json!(["ANALYST", "LOADER"]), "{a}");

    // Managed: the two key slots take turns. The first slot holds a key set by hand, so roll's
    // first key goes in the second, and the hand-set one keeps working.
    let link = |field: &str| json!({ "kind": "snowflake", "ref": id, "field": field, "mode": "managed" });
    let (s, e) = api.call(Method::POST, "/environments/production/changes", Some(json!({ "ops": [{ "op": "set", "key": "SNOWFLAKE_PASSWORD", "link": link("password") }] }))).await;
    assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains("private_key"), "a key-pair account has no password: {e}");
    api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": [{ "op": "set", "key": "SNOWFLAKE_USER", "link": link("user") }, { "op": "set", "key": "SNOWFLAKE_ACCOUNT", "link": link("account") }, { "op": "set", "key": "SNOWFLAKE_PRIVATE_KEY", "link": link("private_key") }] }))).await;
    let public_of = |pem: &str| snowflake_public(&rsa::RsaPrivateKey::from_pkcs8_pem(pem).unwrap()).0;
    let login = || async {
        let v = api.values("production", "/").await["values"].clone();
        assert_eq!((v["SNOWFLAKE_USER"].as_str(), v["SNOWFLAKE_ACCOUNT"].as_str()), (Some("SVC_APP"), Some("myorg-acct")));
        public_of(v["SNOWFLAKE_PRIVATE_KEY"].as_str().unwrap())
    };
    let slots = || sf.users.lock().unwrap()["SVC_APP"].keys.clone();
    let first = login().await;
    assert_eq!(slots(), [Some("set-by-hand".to_string()), Some(first.clone())]);
    let rotate = || api.call(Method::POST, "/environments/production/secrets/SNOWFLAKE_PRIVATE_KEY/rotate", None);
    assert!(rotate().await.0.is_success());
    let second = login().await;
    assert_eq!(slots(), [Some(second.clone()), Some(first.clone())], "the first key keeps working through its grace");
    // The rotation's hold on its slot ends (it lasts two minutes).
    let unhold = id.clone();
    api.db
        .write(move |c| {
            let s: String = c.query_row("SELECT state FROM snowflake_accounts WHERE id = ?", [unhold.as_str()], |r| r.get(0))?;
            let mut v: Value = serde_json::from_str(&s).unwrap();
            v["held"] = json!({});
            c.execute("UPDATE snowflake_accounts SET state = ? WHERE id = ?", [v.to_string().as_str(), unhold.as_str()])
        })
        .unwrap();
    assert_eq!(rotate().await.0, StatusCode::CONFLICT, "the second slot is still in its grace");
    let p = api.project.clone();
    api.db.write(move |c| c.execute("UPDATE leases SET expires_at = 0 WHERE project_id = ? AND kind = 'retired'", [p.as_str()])).unwrap();
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap();
    assert_eq!(slots(), [Some(second.clone()), None], "its grace over, the old key's slot is cleared");

    // Dynamic: each lease is a user of its own, named after it, with the link's roles (the first
    // its default); ending the lease drops it.
    let dynamic = |field: &str, roles: Value| json!({ "kind": "snowflake", "ref": id, "field": field, "mode": "dynamic", "options": { "ttl_minutes": 15, "roles": roles } });
    let (s, e) = api.call(Method::POST, "/environments/staging/changes", Some(json!({ "ops": [{ "op": "set", "key": "TMP_USER", "link": dynamic("user", json!(["sysadmin"])) }] }))).await;
    assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains("SYSADMIN"), "only the account's roles: {e}");
    api.ok(Method::POST, "/environments/staging/changes", Some(json!({ "ops": [{ "op": "set", "key": "TMP_USER", "link": dynamic("user", json!(["loader"])) }, { "op": "set", "key": "TMP_KEY", "link": dynamic("private_key", json!(["loader"])) }] }))).await;
    let r = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "job:sync" }))).await;
    let lease = r["leases"][0].as_str().unwrap().to_string();
    let user = r["values"]["TMP_USER"].as_str().unwrap().to_string();
    assert_eq!(user, format!("ROLL_{}", lease.to_uppercase()));
    let made = sf.users.lock().unwrap()[&user].clone();
    assert_eq!((made.keys[0].clone(), made.default_role.as_deref(), made.roles.clone(), made.days), (Some(public_of(r["values"]["TMP_KEY"].as_str().unwrap())), Some("LOADER"), vec!["LOADER".to_string()], Some(1)));
    api.ok(Method::DELETE, &format!("/leases/{lease}"), None).await;
    assert!(!sf.users.lock().unwrap().contains_key(&user));
    // A user made halfway (its grant refused) is dropped again; nothing is handed out.
    sf.fail_grants.store(true, std::sync::atomic::Ordering::Relaxed);
    let failed = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "job:sync" }))).await;
    sf.fail_grants.store(false, std::sync::atomic::Ordering::Relaxed);
    assert!(failed["leases"] == json!([]) && !sf.users.lock().unwrap().keys().any(|u| u.starts_with("ROLL_")), "{failed}");
    // Code nobody vetted gets no user at all.
    let check = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "check:fork#1", "untrusted": true }))).await;
    assert!(check["values"].get("TMP_KEY").is_none() && check["leases"] == json!([]), "{check}");
    assert!(!sf.users.lock().unwrap().keys().any(|u| u.starts_with("ROLL_")));

    // The admin key is in no answer, audit row or statement.
    let listed = api.ok(Method::GET, "/snowflake-accounts", None).await;
    let audit = api.ok(Method::GET, "/audit", None).await;
    let p = api.project.clone();
    let rows: Vec<String> = api.db.read(move |c| c.query("SELECT summary || keys || action FROM audit WHERE project_id = ?", [p.as_str()], |r| r.get(0))).unwrap();
    let statements = sf.statements.lock().unwrap().join("\n");
    assert!(audit.to_string().contains("snowflake.add") && !format!("{listed}{audit}{rows:?}{statements}").contains("PRIVATE KEY"));
    // In use, it can't go.
    let (s, _) = api.call(Method::DELETE, &format!("/snowflake-accounts/{id}"), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "its keys keep it");
}

#[tokio::test]
async fn an_atlas_project_rotates_two_users_in_turn_and_makes_a_user_per_lease() {
    let api = Api::new().await;
    let atlas = &api.sent.7;
    // A throwaway secret for the fake service account.
    const SECRET: &str = "mdb_sa_sk_FakeOnlyForThisTest0123456789";
    *atlas.secret.lock().unwrap() = SECRET.into();
    for u in ["app_a", "app_b"] {
        let user = json!({ "username": u, "databaseName": "admin", "roles": [{ "roleName": "readWrite", "databaseName": "shop" }], "scopes": [{ "name": "Cluster0", "type": "CLUSTER" }] });
        atlas.users.lock().unwrap().insert(u.into(), (user, "set-by-hand".into()));
    }
    let tokens = || atlas.token_requests.load(std::sync::atomic::Ordering::Relaxed);
    let new = |secret: &str, users: Value, roles: Value| json!({ "name": "shop-atlas", "group_id": ATLAS_GROUP, "client_id": ATLAS_CLIENT, "client_secret": secret, "managed_users": users, "roles": roles });
    // Every name is checked before anything is sent.
    for (body, why) in [
        (new(SECRET, json!(["app_a"]), json!([])), "two"),
        (new(SECRET, json!(["app_a", "app_b"]), json!(["atlasAdmin@admin"])), "atlasAdmin"),
        (new(SECRET, json!(["app_a", "app_b"]), json!(["read"])), "role@db"),
        (json!({ "name": "x", "group_id": "../../orgs", "client_id": ATLAS_CLIENT, "client_secret": SECRET, "roles": ["read@shop"] }), "group_id"),
    ] {
        let (s, e) = api.call(Method::POST, "/atlas-projects", Some(body)).await;
        assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains(why), "{e}");
    }
    assert!(atlas.calls.lock().unwrap().is_empty());
    // A wrong secret gets no token: the answer has Atlas's code, never a secret.
    let (s, e) = api.call(Method::POST, "/atlas-projects", Some(new("mdb_sa_sk_wrong", json!(["app_a", "app_b"]), json!(["read@shop"])))).await;
    assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains("invalid_client") && !e.to_string().contains("mdb_sa_sk_wrong"), "{e}");
    let (s, e) = api.call(Method::POST, "/atlas-projects", Some(new(SECRET, json!(["app_a", "app_c"]), json!(["read@shop"])))).await;
    assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains("no database user app_c"), "{e}");
    let a = api.ok(Method::POST, "/atlas-projects", Some(new(SECRET, json!(["app_a", "app_b"]), json!(["read@shop", "readWrite@shop.orders"])))).await;
    let id = a["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("atl_") && a.get("client_secret").is_none() && a["client_id"] == ATLAS_CLIENT && a["group_id"] == ATLAS_GROUP && a["managed_users"] == json!(["app_a", "app_b"]), "{a}");
    assert_eq!(tokens(), 3, "each registration took a token of its own, kept nowhere");

    // Managed: the two users take turns, each rotation setting the idle one's password.
    let link = |field: &str, options: Value| json!({ "kind": "mongodb_atlas", "ref": id, "field": field, "mode": "managed", "options": options });
    let set = |ops: Vec<(&str, Value)>| json!({ "ops": ops.into_iter().map(|(k, l)| json!({ "op": "set", "key": k, "link": l })).collect::<Vec<_>>() });
    for (l, why) in [(link("password", json!({ "grace_minutes": 1 })), "grace_minutes"), (link("connection_string", json!({})), "cluster option"), (link("password", json!({ "cluster": "Nope" })), "CLUSTER_NOT_FOUND")] {
        let (s, e) = api.call(Method::POST, "/environments/production/changes", Some(set(vec![("MONGODB_PASSWORD", l)]))).await;
        assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains(why), "{e}");
    }
    // A Flex cluster isn't under clusters: its own route gives the connection string.
    let on = json!({ "cluster": "Flex0" });
    api.ok(Method::POST, "/environments/production/changes", Some(set(vec![("MONGODB_USERNAME", link("username", on.clone())), ("MONGODB_PASSWORD", link("password", on.clone())), ("MONGODB_URL", link("connection_string", on.clone()))]))).await;
    let login = || async {
        let v = api.values("production", "/").await["values"].clone();
        assert_eq!(v["MONGODB_URL"], "mongodb+srv://flex0.fghij.mongodb.net");
        (v["MONGODB_USERNAME"].as_str().unwrap().to_string(), v["MONGODB_PASSWORD"].as_str().unwrap().to_string())
    };
    let (user, first) = login().await;
    assert!(user == "app_a" && atlas.password("app_a") == Some(first.clone()) && first.len() == 32 && first.bytes().all(|b| b.is_ascii_alphanumeric()), "{user}");
    let rotate = || api.call(Method::POST, "/environments/production/secrets/MONGODB_PASSWORD/rotate", None);
    assert!(rotate().await.0.is_success());
    let (user, second) = login().await;
    assert!(user == "app_b" && atlas.password("app_b") == Some(second.clone()), "{user}");
    assert_eq!(atlas.password("app_a"), Some(first.clone()), "the first login keeps working through its grace");
    // The rotation's hold on its user ends (it lasts two minutes).
    let unhold = id.clone();
    api.db
        .write(move |c| {
            let s: String = c.query_row("SELECT state FROM atlas_projects WHERE id = ?", [unhold.as_str()], |r| r.get(0))?;
            let mut v: Value = serde_json::from_str(&s).unwrap();
            v["held"] = json!({});
            c.execute("UPDATE atlas_projects SET state = ? WHERE id = ?", [v.to_string().as_str(), unhold.as_str()])
        })
        .unwrap();
    assert_eq!(rotate().await.0, StatusCode::CONFLICT, "app_a is still in its grace");
    let p = api.project.clone();
    api.db.write(move |c| c.execute("UPDATE leases SET expires_at = 0 WHERE project_id = ? AND kind = 'retired'", [p.as_str()])).unwrap();
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap();
    let retired = atlas.password("app_a").unwrap();
    assert!(retired != first && retired != second, "its grace over, app_a's password is one nobody knows");
    assert_eq!(atlas.users.lock().unwrap()["app_a"].0["scopes"], json!([{ "name": "Cluster0", "type": "CLUSTER" }]), "a rotation keeps the user's scopes");
    assert_eq!(tokens(), 4, "one token served every call since");

    // Dynamic: a user per lease with the link's roles, on the link's cluster only, which Atlas
    // deletes after the lease (plus ten minutes) even if roll doesn't.
    let dynamic = |field: &str, roles: Value| json!({ "kind": "mongodb_atlas", "ref": id, "field": field, "mode": "dynamic", "options": { "ttl_minutes": 15, "roles": roles, "cluster": "Cluster0" } });
    let (s, e) = api.call(Method::POST, "/environments/staging/changes", Some(set(vec![("TMP_USER", dynamic("username", json!(["dbAdmin@shop"])))]))).await;
    assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains("dbAdmin@shop"), "only the project's roles: {e}");
    let orders = json!(["readWrite@shop.orders"]);
    api.ok(Method::POST, "/environments/staging/changes", Some(set(vec![("TMP_USER", dynamic("username", orders.clone())), ("TMP_PASSWORD", dynamic("password", orders.clone())), ("TMP_URL", dynamic("connection_string", orders.clone()))]))).await;
    // Atlas revokes the cached token: roll takes a new one, once.
    atlas.tokens.lock().unwrap().clear();
    let r = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "job:sync" }))).await;
    assert_eq!(tokens(), 5);
    let lease = r["leases"][0].as_str().unwrap().to_string();
    let user = r["values"]["TMP_USER"].as_str().unwrap().to_string();
    assert!(user == format!("roll_{lease}") && r["values"]["TMP_URL"] == "mongodb+srv://cluster0.abcde.mongodb.net", "{r}");
    let (made, pw) = atlas.users.lock().unwrap()[&user].clone();
    assert_eq!(Some(pw.as_str()), r["values"]["TMP_PASSWORD"].as_str());
    assert_eq!((made["roles"].clone(), made["scopes"].clone()), (json!([{ "roleName": "readWrite", "databaseName": "shop", "collectionName": "orders" }]), json!([{ "name": "Cluster0", "type": "CLUSTER" }])));
    let after = |u: &str| FakeAtlas::date_ms(&atlas.users.lock().unwrap()[u].0["deleteAfterDate"]).unwrap() - code_sql::now_ms();
    assert!((24 * 60_000..=26 * 60_000).contains(&after(&user)), "the lease's 15 minutes and ten more");
    // Renewing moves Atlas's date too.
    api.ok(Method::POST, &format!("/leases/{lease}/renew"), Some(json!({ "ttl_minutes": 120 }))).await;
    assert!((129 * 60_000..=131 * 60_000).contains(&after(&user)));
    // Ending the lease deletes the user.
    api.ok(Method::DELETE, &format!("/leases/{lease}"), None).await;
    assert!(!atlas.users.lock().unwrap().contains_key(&user));
    // Code nobody vetted gets no user at all.
    let check = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "check:fork#1", "untrusted": true }))).await;
    assert!(check["values"].get("TMP_PASSWORD").is_none() && check["leases"] == json!([]), "{check}");
    assert!(!atlas.users.lock().unwrap().keys().any(|u| u.starts_with("roll_")));

    // The service account's secret is in no answer, audit row, request body to the API, or path.
    let listed = api.ok(Method::GET, "/atlas-projects", None).await;
    let audit = api.ok(Method::GET, "/audit", None).await;
    let p = api.project.clone();
    let rows: Vec<String> = api.db.read(move |c| c.query("SELECT summary || keys || action FROM audit WHERE project_id = ?", [p.as_str()], |r| r.get(0))).unwrap();
    let sent = format!("{:?}{:?}", atlas.bodies.lock().unwrap(), atlas.calls.lock().unwrap());
    assert!(audit.to_string().contains("atlas.add") && !format!("{listed}{audit}{rows:?}{sent}").contains(SECRET));
    // In use, it can't go.
    let (s, _) = api.call(Method::DELETE, &format!("/atlas-projects/{id}"), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "its keys keep it");
}

#[tokio::test]
async fn a_cloud_account_trusts_one_projects_token() {
    use base64::Engine;
    let api = Api::new().await;
    let doc = api.ok(Method::GET, "/v1/secrets/oidc/.well-known/openid-configuration", None).await;
    assert_eq!((doc["issuer"].as_str(), doc["jwks_uri"].as_str()), (Some(ISSUER_URL), Some(format!("{ISSUER_URL}/jwks").as_str())));
    let gcp = json!({ "name": "analytics", "cloud": "gcp", "audience": "//iam.googleapis.com/projects/1234/locations/global/workloadIdentityPools/roll/providers/roll", "service_account": "reports@shop-prod.iam.gserviceaccount.com" });
    // An outage proves nothing about the trust: the account isn't taken until the cloud answers.
    api.clouds.down.store(true, std::sync::atomic::Ordering::Relaxed);
    let (s, e) = api.call(Method::POST, "/cloud-accounts", Some(gcp.clone())).await;
    assert!(s == StatusCode::BAD_REQUEST && e["error"].as_str().unwrap().contains("couldn't tell"), "{e}");
    api.clouds.down.store(false, std::sync::atomic::Ordering::Relaxed);
    // A pool that takes any of roll's projects (another customer's, say) is refused.
    let (s, e) = api.call(Method::POST, "/cloud-accounts", Some(gcp.clone())).await;
    assert!(s == StatusCode::BAD_REQUEST && e["error"].as_str().unwrap().contains(&format!("roll-secrets:{}", api.project)), "{e}");
    *api.clouds.pinned.lock().unwrap() = Some(format!("roll-secrets:{}", api.project));
    let g = api.ok(Method::POST, "/cloud-accounts", Some(gcp)).await;
    let a = api.ok(Method::POST, "/cloud-accounts", Some(json!({ "name": "infra", "cloud": "azure", "tenant": "contoso.onmicrosoft.com", "client_id": "11111111-2222-3333-4444-555555555555" }))).await;
    // Each resolve gets a token of its own, from a token roll signed for this project.
    for (key, kind, acct) in [("GOOGLE_OAUTH_ACCESS_TOKEN", "gcp", &g), ("AZURE_ACCESS_TOKEN", "azure", &a)] {
        api.ok(Method::PUT, &format!("/environments/production/secrets/{key}"), Some(json!({ "link": { "kind": kind, "ref": acct["id"], "field": "access_token", "mode": "dynamic", "options": { "ttl_minutes": 15 } } }))).await;
    }
    let one = api.ok(Method::POST, "/environments/production/resolve", Some(json!({ "consumer": "job:report" }))).await;
    let two = api.ok(Method::POST, "/environments/production/resolve", Some(json!({ "consumer": "job:report" }))).await;
    assert!(one["values"]["GOOGLE_OAUTH_ACCESS_TOKEN"].as_str().unwrap().starts_with("ya29.") && one["values"]["AZURE_ACCESS_TOKEN"].is_string());
    assert_ne!(one["values"]["GOOGLE_OAUTH_ACCESS_TOKEN"], two["values"]["GOOGLE_OAUTH_ACCESS_TOKEN"]);
    // What the cloud got verifies against roll's published keys, and names this project.
    let jwt = api.clouds.seen.lock().unwrap().last().unwrap().clone();
    let b64 = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).unwrap();
    let parts: Vec<&str> = jwt.split('.').collect();
    let (header, claims): (Value, Value) = (serde_json::from_slice(&b64(parts[0])).unwrap(), serde_json::from_slice(&b64(parts[1])).unwrap());
    let jwks = api.ok(Method::GET, "/v1/secrets/oidc/jwks", None).await;
    let key = jwks["keys"].as_array().unwrap().iter().find(|k| k["kid"] == header["kid"]).expect("the signing key is published");
    let public = ring::signature::RsaPublicKeyComponents { n: b64(key["n"].as_str().unwrap()), e: b64(key["e"].as_str().unwrap()) };
    public.verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, format!("{}.{}", parts[0], parts[1]).as_bytes(), &b64(parts[2])).expect("roll's signature");
    assert_eq!((claims["iss"].as_str(), claims["sub"].clone(), header["alg"].as_str()), (Some(ISSUER_URL), json!(format!("roll-secrets:{}", api.project)), Some("RS256")));
    assert_eq!(claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(), 300);
    // An account a key links to stays, and a lease can't outlive its token.
    assert_eq!(api.call(Method::DELETE, &format!("/cloud-accounts/{}", g["id"].as_str().unwrap()), None).await.0, StatusCode::CONFLICT);
    let (s, _) = api.call(Method::PUT, "/environments/production/secrets/LONG_TOKEN", Some(json!({ "link": { "kind": "gcp", "ref": g["id"], "field": "access_token", "mode": "dynamic", "options": { "ttl_minutes": 120 } } }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // The deployment key rotates: the issuer's key is resealed with the new one (health says when
    // the previous key can go), and keeps signing once the previous key is gone.
    let reopen = |current: &str, previous: Option<&str>| {
        let opts = Options { keyring: Ok(Keyring::new(current, previous).unwrap()), key_source: roll_secrets::KeySource::Env, links: Default::default(), syncs: Default::default(), open: true, trust_forwarded_for: false, web_url: None, deliver: None, grants: None, people: None, clouds: Some(roll_secrets::clouds::Clouds { issuer: ISSUER_URL.into(), tokens: api.clouds.clone() }), kms: None, sts: None, outside: None, directory: None, github: None, workloads: None, gateways: None };
        Secrets::new(api.db.clone(), api.ws.clone(), opts).unwrap()
    };
    let (old, new) = ("k".repeat(32), "n".repeat(32));
    let rotated = reopen(&new, Some(&old));
    let p = api.project.clone();
    // Any request reseals its project's key; health reseals the issuer's.
    let s = tokio::task::spawn_blocking(move || {
        rotated.cloud_accounts(&p, &roll_secrets::Caller::operator()).unwrap();
        rotated.health().unwrap()
    })
    .await
    .unwrap();
    assert_eq!(s["resealed"], "all", "{s}");
    let after = Arc::new(reopen(&new, None));
    let (a, acct) = (after.clone(), g["id"].as_str().unwrap().to_string());
    let jwks = tokio::task::spawn_blocking(move || a.oidc_jwks().unwrap()).await.unwrap();
    assert_eq!(jwks["keys"].as_array().unwrap().len(), 1, "the same key, still published");
    let a = after.clone();
    tokio::task::spawn_blocking(move || a.links().get("gcp").unwrap().issue(&acct, "token", &serde_json::from_value(json!({ "kind": "gcp", "ref": "x", "field": "access_token", "mode": "dynamic" })).unwrap(), "after", Some(15)).map(|_| ()))
        .await
        .unwrap()
        .expect("signing with the resealed key");
}

#[tokio::test]
async fn a_projects_ca_signs_short_lived_certificates_for_its_names_only() {
    use x509_parser::prelude::*;
    let api = Api::new().await;
    api.secrets.set_public_api("https://roll.example".into());
    let ca = api.ok(Method::POST, "/cas", Some(json!({ "name": "internal", "domains": ["svc.internal"] }))).await;
    let ca_pem = ca["certificate"].as_str().unwrap().to_string();
    let link = |field: &str, mode: &str, names: Value| json!({ "kind": "pki", "ref": ca["id"], "field": field, "mode": mode, "options": { "names": names, "ttl_minutes": 60, "rotate_days": 7 } });
    // Each resolve gets a certificate of its own, signed by the CA, for the link's names.
    let names = json!(["api.svc.internal"]);
    let ops: Vec<Value> = [("certificate", "TLS_CERT"), ("private_key", "TLS_KEY"), ("ca", "TLS_CA")].iter().map(|(f, k)| json!({ "op": "set", "key": k, "link": link(f, "dynamic", names.clone()) })).collect();
    api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": ops }))).await;
    let r = api.ok(Method::POST, "/environments/production/resolve", Some(json!({ "consumer": "app:api" }))).await;
    let (cert, key, chain) = (r["values"]["TLS_CERT"].as_str().unwrap(), r["values"]["TLS_KEY"].as_str().unwrap(), r["values"]["TLS_CA"].as_str().unwrap());
    assert!(key.starts_with("-----BEGIN PRIVATE KEY-----") && chain == ca_pem);
    let (_, ca_der) = parse_x509_pem(ca_pem.as_bytes()).unwrap();
    let (_, ca_cert) = parse_x509_certificate(&ca_der.contents).unwrap();
    let (_, der) = parse_x509_pem(cert.as_bytes()).unwrap();
    let (_, leaf) = parse_x509_certificate(&der.contents).unwrap();
    leaf.verify_signature(Some(ca_cert.public_key())).expect("signed by the project's CA");
    let sans: Vec<String> = leaf.subject_alternative_name().unwrap().unwrap().value.general_names.iter().map(|n| format!("{n}")).collect();
    assert_eq!(sans, ["DNSName(api.svc.internal)"]);
    let life = leaf.validity().not_after.timestamp() - leaf.validity().not_before.timestamp();
    assert!((3900..=4000).contains(&life), "the lease's TTL plus five minutes: {life}");
    // The CA is limited to its domains and no IP addresses, so trusting it trusts nothing else.
    let nc = ca_cert.name_constraints().unwrap().unwrap().value;
    assert_eq!(nc.permitted_subtrees.as_ref().map(Vec::len), Some(1));
    assert_eq!(nc.excluded_subtrees.as_ref().map(Vec::len), Some(2));
    // A name outside its domains is refused; managed certificates live twice their rotation.
    let (s, e) = api.call(Method::PUT, "/environments/staging/secrets/WEB_CERT", Some(json!({ "link": link("certificate", "managed", json!("web.example.com")) }))).await;
    assert!(s == StatusCode::BAD_REQUEST && e["error"].as_str().unwrap().contains("doesn't sign for web.example.com"), "{e}");
    // A managed certificate rotates before it ends, or isn't taken.
    for options in [json!({ "names": "web.svc.internal" }), json!({ "names": "web.svc.internal", "rotate_days": 60 })] {
        let (s, e) = api.call(Method::PUT, "/environments/staging/secrets/WEB_CERT", Some(json!({ "link": { "kind": "pki", "ref": ca["id"], "field": "certificate", "mode": "managed", "options": options } }))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    }
    api.ok(Method::PUT, "/environments/staging/secrets/WEB_CERT", Some(json!({ "link": link("certificate", "managed", json!("web.svc.internal, *.web.svc.internal")) }))).await;
    let stored = api.values("staging", "/").await["values"]["WEB_CERT"].as_str().unwrap().to_string();
    let (_, der) = parse_x509_pem(stored.as_bytes()).unwrap();
    let (_, managed) = parse_x509_certificate(&der.contents).unwrap();
    assert_eq!((managed.validity().not_after.timestamp() - managed.validity().not_before.timestamp()) / 86_400, 14);
    // A CSR: its key stays with the device; only its key and names are taken, never what else it
    // asks for (here, to be a CA itself and to sign code).
    let device = rcgen::KeyPair::generate().unwrap();
    let csr = |names: &[&str]| {
        let mut p = rcgen::CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>()).unwrap();
        p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        p.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::CodeSigning];
        p.serialize_request(&device).unwrap().pem().unwrap()
    };
    let at = format!("/cas/{}/sign", ca["id"].as_str().unwrap());
    let sign = |csr: String, days: u32| api.call(Method::POST, &at, Some(json!({ "csr": csr, "days": days })));
    let (s, signed) = sign(csr(&["sensor-7.svc.internal"]), 3).await;
    assert_eq!(s, StatusCode::OK, "{signed}");
    let (_, der) = parse_x509_pem(signed["certificate"].as_str().unwrap().as_bytes()).unwrap();
    let (_, leaf) = parse_x509_certificate(&der.contents).unwrap();
    leaf.verify_signature(Some(ca_cert.public_key())).expect("signed by the project's CA");
    assert_eq!(leaf.public_key().subject_public_key.data.as_ref(), rcgen::PublicKeyData::der_bytes(&device), "the device's own key");
    assert!(!leaf.is_ca() && !leaf.extended_key_usage().unwrap().unwrap().value.code_signing, "not what the CSR asked beyond its names");
    assert_eq!((leaf.validity().not_after.timestamp() - leaf.validity().not_before.timestamp()) / 86_400, 3);
    assert_eq!(sign(csr(&["sensor-7.example.com"]), 3).await.0, StatusCode::BAD_REQUEST, "a name outside the CA's domains");
    assert_eq!(sign(csr(&["sensor-7.svc.internal"]), 91).await.0, StatusCode::BAD_REQUEST, "longer than the CA allows");
    // Revoking: a lost device's certificate by hand, a lease's when it ends. The CA's CRL (public)
    // lists both, and every certificate names where it is.
    let id = ca["id"].as_str().unwrap();
    let crl_url = format!("https://roll.example/v1/secrets/crl/{id}");
    assert!(der.contents.windows(crl_url.len()).any(|w| w == crl_url.as_bytes()), "the CRL distribution point");
    let listed = api.ok(Method::GET, &format!("/cas/{id}/certificates"), None).await;
    assert!(listed.as_array().unwrap().iter().any(|c| c["serial"] == signed["serial"] && c["source"] == "csr") && listed.as_array().unwrap().iter().any(|c| c["source"] == "link"), "{listed}");
    let revoke_at = format!("/cas/{id}/revoke");
    let revoke = |serial: Value| api.call(Method::POST, &revoke_at, Some(json!({ "serial": serial })));
    assert_eq!(revoke(signed["serial"].clone()).await.0, StatusCode::NO_CONTENT);
    assert_eq!(revoke(json!("00")).await.0, StatusCode::NOT_FOUND);
    // A CRL fetched now is kept a minute; a revocation still shows in the next one.
    assert_eq!(api.call(Method::GET, &format!("/v1/secrets/crl/{id}?format=pem"), None).await.0, StatusCode::OK);
    let lease_cert = parse_x509_pem(cert.as_bytes()).unwrap().1;
    let lease_serial = parse_x509_certificate(&lease_cert.contents).unwrap().1.raw_serial().to_vec();
    assert_eq!(api.call(Method::DELETE, &format!("/leases/{}", r["leases"][0].as_str().unwrap()), None).await.0, StatusCode::NO_CONTENT);
    let (s, crl) = api.call(Method::GET, &format!("/v1/secrets/crl/{id}?format=pem"), None).await;
    assert_eq!(s, StatusCode::OK);
    let (_, pem) = parse_x509_pem(crl.as_str().unwrap().as_bytes()).unwrap();
    let (_, crl) = parse_x509_crl(&pem.contents).unwrap();
    crl.verify_signature(ca_cert.public_key()).expect("signed by the CA");
    let revoked: Vec<Vec<u8>> = crl.iter_revoked_certificates().map(|r| r.raw_serial().to_vec()).collect();
    assert!(revoked.contains(&leaf.raw_serial().to_vec()) && revoked.contains(&lease_serial), "both revoked: {revoked:?}");
    // A CA whose revocations still matter keeps serving them: it can't be deleted until they end.
    let devices = api.ok(Method::POST, "/cas", Some(json!({ "name": "devices", "domains": ["dev.internal"] }))).await;
    let dev_id = devices["id"].as_str().unwrap();
    let one = api.ok(Method::POST, &format!("/cas/{dev_id}/sign"), Some(json!({ "csr": csr(&["d1.dev.internal"]), "days": 2 }))).await;
    api.call(Method::POST, &format!("/cas/{dev_id}/revoke"), Some(json!({ "serial": one["serial"] }))).await;
    let (s, e) = api.call(Method::DELETE, &format!("/cas/{dev_id}"), None).await;
    assert!(s == StatusCode::CONFLICT && e.to_string().contains("revocation list"), "{e}");
    // A CSR's certificate ending within two weeks is health's to say (nothing renews it); a
    // revoked one isn't.
    let d2 = api.ok(Method::POST, &format!("/cas/{dev_id}/sign"), Some(json!({ "csr": csr(&["d2.dev.internal"]), "days": 5 }))).await;
    let health = api.ok(Method::GET, "/health", None).await;
    let ending: Vec<&Value> = health.as_array().unwrap().iter().filter(|f| f["issue"] == "certificate_expiring").collect();
    assert_eq!(ending.iter().map(|f| (f["key"].as_str().unwrap(), f["path"].as_str().unwrap())).collect::<Vec<_>>(), [("d2.dev.internal", "devices")], "{health}");
    // OCSP, beside the CRL: one revoked and one good certificate, answered and signed by the CA.
    {
        use der::{Decode, Encode};
        let ca_pem = devices["certificate"].as_str().unwrap();
        let (_, pem) = parse_x509_pem(ca_pem.as_bytes()).unwrap();
        let ca = x509_cert::Certificate::from_der(&pem.contents).unwrap();
        let sha1 = |b: &[u8]| der::asn1::OctetString::new(ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, b).as_ref().to_vec()).unwrap();
        let (name_hash, key_hash) = (sha1(&ca.tbs_certificate.subject.to_der().unwrap()), sha1(ca.tbs_certificate.subject_public_key_info.subject_public_key.raw_bytes()));
        let ask = |serial: &str| x509_ocsp::Request {
            req_cert: x509_ocsp::CertId {
                hash_algorithm: x509_cert::spki::AlgorithmIdentifierOwned { oid: der::oid::ObjectIdentifier::new_unwrap("1.3.14.3.2.26"), parameters: None },
                issuer_name_hash: name_hash.clone(),
                issuer_key_hash: key_hash.clone(),
                serial_number: x509_cert::serial_number::SerialNumber::new(&(0..serial.len()).step_by(2).map(|i| u8::from_str_radix(&serial[i..i + 2], 16).unwrap()).collect::<Vec<_>>()).unwrap(),
            },
            single_request_extensions: None,
        };
        let request = x509_ocsp::OcspRequest { tbs_request: x509_ocsp::TbsRequest { version: Default::default(), requestor_name: None, request_list: vec![ask(one["serial"].as_str().unwrap()), ask(d2["serial"].as_str().unwrap())], request_extensions: None }, optional_signature: None };
        let req = Request::builder().method(Method::POST).uri(format!("/v1/secrets/ocsp/{dev_id}")).body(Body::from(request.to_der().unwrap())).unwrap();
        let resp = api.router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let answer = x509_ocsp::OcspResponse::from_der(&bytes).unwrap();
        let basic = x509_ocsp::BasicOcspResponse::from_der(answer.response_bytes.unwrap().response.as_bytes()).unwrap();
        let statuses: Vec<&str> = basic.tbs_response_data.responses.iter().map(|r| match r.cert_status { x509_ocsp::CertStatus::Good(_) => "good", x509_ocsp::CertStatus::Revoked(_) => "revoked", x509_ocsp::CertStatus::Unknown(_) => "unknown" }).collect();
        assert_eq!(statuses, ["revoked", "good"]);
        let public = ca.tbs_certificate.subject_public_key_info.subject_public_key.raw_bytes();
        ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, public).verify(&basic.tbs_response_data.to_der().unwrap(), basic.signature.raw_bytes()).expect("signed by the CA");
    }
    // A CA a key links to stays.
    assert_eq!(api.call(Method::DELETE, &format!("/cas/{}", ca["id"].as_str().unwrap()), None).await.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn an_intermediate_ca_issues_under_the_customers_own_root() {
    use x509_parser::prelude::*;
    let api = Api::new().await;
    let ca = api.ok(Method::POST, "/cas", Some(json!({ "name": "under-root", "intermediate": true, "domains": ["svc.internal"] }))).await;
    let id = ca["id"].as_str().unwrap().to_string();
    assert_eq!((ca["status"].as_str(), ca["certificate"].as_str()), (Some("pending"), Some("")));
    let csr_pem = ca["csr"].as_str().unwrap();
    // A throwaway offline root (obviously fake) signs the CSR roll answered, as a customer's would.
    let root_key = rcgen::KeyPair::generate().unwrap();
    let mut root = rcgen::CertificateParams::default();
    root.distinguished_name.push(rcgen::DnType::CommonName, "Example Fake Offline Root");
    root.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    root.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign, rcgen::KeyUsagePurpose::CrlSign];
    let root_cert = root.self_signed(&root_key).unwrap();
    let root_issuer = rcgen::Issuer::from_params(&root, &root_key);
    let (_, csr_der) = parse_x509_pem(csr_pem.as_bytes()).unwrap();
    let (_, csr) = x509_parser::certification_request::X509CertificationRequest::from_der(&csr_der.contents).unwrap();
    csr.verify_signature().expect("the CSR is signed with the CA's key");
    let wanted = csr.requested_extensions().unwrap().any(|e| matches!(e, ParsedExtension::NameConstraints(_)));
    assert!(wanted, "the CSR asks for the CA's name constraints");
    let spki = rcgen::SubjectPublicKeyInfo::from_der(csr.certification_request_info.subject_pki.raw).unwrap();
    let intermediate = |days: i64, permitted: &str| {
        let mut p = rcgen::CertificateParams::default();
        p.distinguished_name.push(rcgen::DnType::CommonName, "roll intermediate");
        p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
        p.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign, rcgen::KeyUsagePurpose::CrlSign];
        p.name_constraints = Some(rcgen::NameConstraints { permitted_subtrees: vec![rcgen::GeneralSubtree::DnsName(permitted.into())], excluded_subtrees: vec![] });
        p.not_after = ::time::OffsetDateTime::now_utc() + ::time::Duration::days(days);
        p.use_authority_key_identifier_extension = true;
        p
    };
    let chain_at = format!("/cas/{id}/chain");
    // A pending CA signs nothing; a certificate for another key isn't installed.
    let device = rcgen::KeyPair::generate().unwrap();
    let leaf_csr = rcgen::CertificateParams::new(vec!["api.prod.svc.internal".to_string()]).unwrap().serialize_request(&device).unwrap().pem().unwrap();
    let (s, e) = api.call(Method::POST, &format!("/cas/{id}/sign"), Some(json!({ "csr": leaf_csr }))).await;
    assert!(s == StatusCode::BAD_REQUEST && e["error"].as_str().unwrap().contains("waiting for its certificate"), "{e}");
    let other = rcgen::KeyPair::generate().unwrap();
    let wrong = intermediate(30, "svc.internal").signed_by(&other, &root_issuer).unwrap();
    let (s, e) = api.call(Method::PUT, &chain_at, Some(json!({ "certificate": wrong.pem(), "chain": root_cert.pem() }))).await;
    assert!(s == StatusCode::BAD_REQUEST && e["error"].as_str().unwrap().contains("isn't for this CA's key"), "{e}");
    // The root signs narrower names than asked, for 30 days: the CA keeps the narrower names.
    let inter = intermediate(30, "prod.svc.internal").signed_by(&spki, &root_issuer).unwrap();
    let (s, e) = api.call(Method::PUT, &chain_at, Some(json!({ "certificate": inter.pem() }))).await;
    assert!(s == StatusCode::BAD_REQUEST && e["error"].as_str().unwrap().contains("doesn't reach a self-signed root"), "{e}");
    let ca = api.ok(Method::PUT, &chain_at, Some(json!({ "certificate": inter.pem(), "chain": root_cert.pem() }))).await;
    assert_eq!((ca["status"].as_str(), ca["domains"].clone()), (Some("active"), json!(["prod.svc.internal"])));
    assert!(ca.get("csr").is_none() && ca["ca_chain"].as_str().unwrap().ends_with(&root_cert.pem()));
    // A leaf never outlives the intermediate, and comes with the chain.
    let (s, e) = api.call(Method::POST, &format!("/cas/{id}/sign"), Some(json!({ "csr": leaf_csr, "days": 60 }))).await;
    assert!(s == StatusCode::BAD_REQUEST && e["error"].as_str().unwrap().contains("its own certificate ends"), "{e}");
    let signed = api.ok(Method::POST, &format!("/cas/{id}/sign"), Some(json!({ "csr": leaf_csr }))).await;
    let ders: Vec<Vec<u8>> = x509_parser::pem::Pem::iter_from_buffer(signed["ca_chain"].as_str().unwrap().as_bytes()).map(|p| p.unwrap().contents).collect();
    assert_eq!(ders.len(), 2);
    let (_, leaf_pem) = parse_x509_pem(signed["certificate"].as_str().unwrap().as_bytes()).unwrap();
    let leaf = rustls_pki_types::CertificateDer::from(leaf_pem.contents);
    let root_der = rustls_pki_types::CertificateDer::from(root_cert.der().to_vec());
    let anchors = [webpki::anchor_from_trusted_cert(&root_der).unwrap()];
    let through = [rustls_pki_types::CertificateDer::from(ders[0].clone())];
    let ee = webpki::EndEntityCert::try_from(&leaf).unwrap();
    ee.verify_for_usage(webpki::ALL_VERIFICATION_ALGS, &anchors, &through, rustls_pki_types::UnixTime::now(), webpki::KeyUsage::server_auth(), None, None).expect("the leaf verifies against the root through roll's intermediate");
    // A link's certificate carries the chain too.
    let link = json!({ "kind": "pki", "ref": id, "field": "ca_chain", "mode": "dynamic", "options": { "names": ["web.prod.svc.internal"], "ttl_minutes": 60 } });
    api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": [{ "op": "set", "key": "TLS_CA_CHAIN", "link": link }] }))).await;
    let r = api.ok(Method::POST, "/environments/production/resolve", Some(json!({ "consumer": "app:web" }))).await;
    assert_eq!(r["values"]["TLS_CA_CHAIN"], signed["ca_chain"]);
}

/// Another source of bound hosts (`Secrets::set_bound_hosts`), for bindings placeholders refuse to
/// make: a CA refuses them too.
#[derive(Default)]
struct FakeBound(Mutex<BTreeMap<String, BTreeMap<String, std::collections::BTreeSet<String>>>>);

impl roll_secrets::intercept::BoundHosts for FakeBound {
    fn bound_hosts(&self, consumer: &str) -> roll_secrets::Result<BTreeMap<String, std::collections::BTreeSet<String>>> {
        Ok(self.0.lock().unwrap().get(consumer).cloned().unwrap_or_default())
    }
}

#[tokio::test]
async fn an_interception_ca_names_only_its_consumers_bound_hosts_and_ends_with_it() {
    use roll_secrets::Error;
    use std::time::Duration;
    let api = Api::new().await;
    let s = &api.secrets;
    let consumer = "sandbox:sbx_1";
    assert!(matches!(s.intercept_ca(consumer, None), Err(Error::NotFound(_))), "no placeholders, no CA");
    // Placeholders' own bindings: the consumer's live placeholders' keys' hosts. It ends with them.
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY", Some(json!({ "value": "sk_test_fake_ca_1" }))).await;
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY/hosts", Some(json!({ "hosts": ["api.stripe.com", "*.stripe.dev"] }))).await;
    api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": consumer, "placeholders": true }))).await;
    let real = s.intercept_ca(consumer, None).unwrap();
    assert_eq!(real.hosts, ["*.stripe.dev", "api.stripe.com"]);
    assert_eq!(s.end_consumer(consumer).unwrap(), 1);
    assert!(!s.intercept_ca_live(&real.id).unwrap());
    assert!(matches!(s.intercept_ca(consumer, None), Err(Error::NotFound(_))), "its placeholders ended");
    let bound = Arc::new(FakeBound::default());
    let bind = |consumer: &str, hosts: &[&str]| {
        bound.0.lock().unwrap().insert(consumer.into(), BTreeMap::from([(api.project.clone(), hosts.iter().map(|h| h.to_string()).collect())]));
    };
    bind(consumer, &["api.stripe.com", "*.example.com"]);
    assert!(s.set_bound_hosts(bound.clone()));
    assert!(matches!(s.intercept_ca("sandbox:other", None), Err(Error::NotFound(_))), "a consumer with no placeholders");
    // The hosts are the bindings', and the TTL is a day at most.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let ca = s.intercept_ca(consumer, Some(Duration::from_secs(7 * 86_400))).unwrap();
    assert!((now + 86_400_000 - 60_000..=now + 86_400_000 + 60_000).contains(&ca.expires_at), "{}", ca.expires_at - now);
    assert_eq!(ca.hosts, ["*.example.com", "api.stripe.com"]);
    assert!(ca.cert_pem.starts_with("-----BEGIN CERTIFICATE-----") && ca.key.expose().starts_with("-----BEGIN PRIVATE KEY-----"));
    assert!(!format!("{ca:?}").contains("PRIVATE KEY"));
    // Asking again mints a new one, never the same key.
    let again = s.intercept_ca(consumer, None).unwrap();
    assert!(again.id != ca.id && again.key.expose() != ca.key.expose());
    assert!(s.intercept_ca_live(&ca.id).unwrap() && s.intercept_ca_live(&again.id).unwrap() && !s.intercept_ca_live("ica_nope").unwrap());
    // Audited with the consumer, hosts and expiry; never the key.
    let audit = api.ok(Method::GET, "/audit?action=secrets.intercept_ca", None).await.to_string();
    assert!(audit.contains(&format!("interception CA {} for sandbox:sbx_1: *.example.com, api.stripe.com, until 20", ca.id)), "{audit}");
    assert!(!audit.contains("PRIVATE KEY"));
    // Ending the consumer ends both, with a revocation row, and the proxy's next check drops them.
    assert_eq!(s.end_intercept_cas(consumer, "the sandbox stopped").unwrap(), 2);
    assert!(!s.intercept_ca_live(&ca.id).unwrap() && !s.intercept_ca_live(&again.id).unwrap());
    assert_eq!(s.end_intercept_cas(consumer, "again").unwrap(), 0);
    let ended = api.ok(Method::GET, "/audit?action=secrets.intercept_ca.end", None).await.to_string();
    assert!(ended.contains(&format!("ended interception CA {} for sandbox:sbx_1: the sandbox stopped", ca.id)), "{ended}");
    // A binding a CA must never name is refused, not narrowed.
    for wide in ["*.co.uk", "*", "10.0.0.1"] {
        bind("sandbox:sbx_2", &["api.stripe.com", wide]);
        assert!(matches!(s.intercept_ca("sandbox:sbx_2", None), Err(Error::BadRequest(_))), "{wide}");
    }
}

/// Placeholders' own bindings, counting each time they're read.
struct CountedBound {
    secrets: std::sync::Weak<Secrets>,
    reads: std::sync::atomic::AtomicUsize,
}

impl roll_secrets::intercept::BoundHosts for CountedBound {
    fn bound_hosts(&self, consumer: &str) -> roll_secrets::Result<BTreeMap<String, std::collections::BTreeSet<String>>> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        roll_secrets::intercept::BoundHosts::bound_hosts(&*self.secrets.upgrade().unwrap(), consumer)
    }
}

/// A consumer's hosts shrinking (a placeholder revoked, a binding taken off) ends its CAs that name a
/// host it lost, with a revocation row; the next one names only the hosts it still has. Growing
/// (a placeholder for another key, a binding widened) leaves its CAs standing but not live, and
/// the next one names every host. A connection's check reads one row, never the bindings.
#[tokio::test]
async fn an_interception_ca_ends_when_its_consumer_loses_one_of_its_hosts() {
    use roll_secrets::Error;
    use std::sync::atomic::Ordering;
    let api = Api::new().await;
    let s = &api.secrets;
    let counted = Arc::new(CountedBound { secrets: Arc::downgrade(s), reads: Default::default() });
    assert!(s.set_bound_hosts(counted.clone()));
    let consumer = "sandbox:sbx_1";
    for (key, host) in [("STRIPE_KEY", "api.stripe.com"), ("GITHUB_TOKEN", "api.github.com")] {
        api.ok(Method::PUT, &format!("/environments/staging/secrets/{key}"), Some(json!({ "value": format!("fake_{key}_value_0001") }))).await;
        api.ok(Method::PUT, &format!("/environments/staging/secrets/{key}/hosts"), Some(json!({ "hosts": [host] }))).await;
    }
    api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": consumer, "placeholders": true }))).await;
    let old = s.intercept_ca(consumer, None).unwrap();
    assert_eq!(old.hosts, ["api.github.com", "api.stripe.com"]);
    let listed = api.ok(Method::GET, "/placeholders", None).await;
    let stripe = listed.as_array().unwrap().iter().find(|p| p["key"] == "STRIPE_KEY").unwrap_or_else(|| panic!("{listed}"))["id"].as_str().unwrap().to_string();
    assert!(api.call(Method::DELETE, &format!("/placeholders/{stripe}"), None).await.0.is_success());
    // Ended by the revocation itself (before the proxy's next check), with its row.
    let ended = api.ok(Method::GET, "/audit?action=secrets.intercept_ca.end", None).await.to_string();
    assert!(ended.contains(&format!("ended interception CA {} for sandbox:sbx_1: api.stripe.com no longer bound", old.id)), "{ended}");
    assert!(!s.intercept_ca_live(&old.id).unwrap());
    let new = s.intercept_ca(consumer, None).unwrap();
    assert_eq!(new.hosts, ["api.github.com"]);
    // The other key's binding taken off: that one ends too, and nothing is left to intercept.
    assert!(api.call(Method::DELETE, "/environments/staging/secrets/GITHUB_TOKEN/hosts", None).await.0.is_success());
    assert_eq!(s.end_intercept_cas(consumer, "again").unwrap(), 0, "already ended");
    assert!(!s.intercept_ca_live(&new.id).unwrap());
    assert!(matches!(s.intercept_ca(consumer, None), Err(Error::NotFound(_))));

    // Another consumer holding only STRIPE_KEY's placeholder.
    let grows = "sandbox:sbx_2";
    api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": grows, "placeholders": true }))).await;
    let first = s.intercept_ca(grows, None).unwrap();
    assert_eq!(first.hosts, ["api.stripe.com"]);
    // Connections check its row, never the bindings.
    let reads = counted.reads.load(Ordering::SeqCst);
    for _ in 0..50 {
        assert!(s.intercept_ca_stands(&first.id).unwrap() && s.intercept_ca_live(&first.id).unwrap());
    }
    assert_eq!(counted.reads.load(Ordering::SeqCst), reads, "no connection read the bindings");
    // GITHUB_TOKEN bound again and a placeholder of it issued: the CA doesn't name api.github.com,
    // so it stops being live, but still stands for api.stripe.com (nothing ended).
    api.ok(Method::PUT, "/environments/staging/secrets/GITHUB_TOKEN/hosts", Some(json!({ "hosts": ["api.github.com"] }))).await;
    api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": grows, "placeholders": true }))).await;
    assert!(!s.intercept_ca_live(&first.id).unwrap() && s.intercept_ca_stands(&first.id).unwrap());
    let both = s.intercept_ca(grows, None).unwrap();
    assert_eq!(both.hosts, ["api.github.com", "api.stripe.com"]);
    assert!(s.intercept_ca_live(&both.id).unwrap());
    // A binding the consumer already holds a placeholder of, widened: stale too.
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY/hosts", Some(json!({ "hosts": ["api.stripe.com", "files.stripe.com"] }))).await;
    assert!(!s.intercept_ca_live(&both.id).unwrap() && s.intercept_ca_stands(&both.id).unwrap());
    assert_eq!(s.intercept_ca(grows, None).unwrap().hosts, ["api.github.com", "api.stripe.com", "files.stripe.com"]);
    let ended = api.ok(Method::GET, "/audit?action=secrets.intercept_ca.end", None).await.to_string();
    assert!(!ended.contains(&first.id) && !ended.contains(&both.id), "growing ends nothing: {ended}");
    // The key deleted: its hosts go, and the CAs naming them end at their next check.
    api.ok(Method::POST, "/environments/staging/changes", Some(json!({ "ops": [{ "op": "delete", "key": "GITHUB_TOKEN" }] }))).await;
    assert!(!s.intercept_ca_stands(&both.id).unwrap());
}

#[tokio::test]
async fn an_ssh_ca_signs_short_lived_user_certificates_for_its_principals() {
    use base64::Engine;
    use ed25519_dalek::Verifier;
    let api = Api::new().await;
    let ca = api.ok(Method::POST, "/ssh-cas", Some(json!({ "name": "ops", "principals": ["deploy", "ubuntu"], "max_hours": 8 }))).await;
    let ca_pub = ca["public_key"].as_str().unwrap();
    assert!(ca_pub.starts_with("ssh-ed25519 AAAA"), "{ca_pub}");
    let link = |field: &str, principals: &str| json!({ "kind": "ssh", "ref": ca["id"], "field": field, "mode": "dynamic", "options": { "principals": principals, "ttl_minutes": 30 } });
    // A login the CA doesn't sign for is refused when it's linked.
    let (s, e) = api.call(Method::PUT, "/environments/production/secrets/ROOT_CERT", Some(json!({ "link": link("certificate", "root") }))).await;
    assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains("doesn't sign for root"), "{e}");
    api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": [{ "op": "set", "key": "SSH_CERT", "link": link("certificate", "deploy") }, { "op": "set", "key": "SSH_KEY", "link": link("private_key", "deploy") }] }))).await;
    let r = api.ok(Method::POST, "/environments/production/resolve", Some(json!({ "consumer": "job:release" }))).await;
    let (cert, key) = (r["values"]["SSH_CERT"].as_str().unwrap(), r["values"]["SSH_KEY"].as_str().unwrap());
    assert!(key.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
    // The certificate: signed by the CA's key over everything before its signature, for `deploy`,
    // for the lease's TTL plus five minutes.
    let blob = base64::engine::general_purpose::STANDARD.decode(cert.split(' ').nth(1).unwrap()).unwrap();
    fn take(b: &[u8], p: &mut usize) -> Vec<u8> {
        let n = u32::from_be_bytes(b[*p..*p + 4].try_into().unwrap()) as usize;
        let s = b[*p + 4..*p + 4 + n].to_vec();
        *p += 4 + n;
        s
    }
    let mut p = 0usize;
    assert_eq!(take(&blob, &mut p), b"ssh-ed25519-cert-v01@openssh.com");
    take(&blob, &mut p); // nonce
    take(&blob, &mut p); // the user's key
    p += 8 + 4; // serial, type
    take(&blob, &mut p); // key id
    assert_eq!(&take(&blob, &mut p)[4..], b"deploy");
    let (after, before) = (u64::from_be_bytes(blob[p..p + 8].try_into().unwrap()), u64::from_be_bytes(blob[p + 8..p + 16].try_into().unwrap()));
    assert_eq!(before - after, 36 * 60, "the lease's TTL plus five minutes, and a minute of skew");
    p += 16;
    for _ in 0..3 {
        take(&blob, &mut p); // critical options, extensions, reserved
    }
    let signer = take(&blob, &mut p);
    let signed_to = p;
    let sig = take(&blob, &mut p);
    let ca_key = base64::engine::general_purpose::STANDARD.decode(ca_pub.split(' ').nth(1).unwrap()).unwrap();
    assert_eq!(signer, ca_key, "signed by the project's CA");
    let public = ed25519_dalek::VerifyingKey::from_bytes(ca_key[ca_key.len() - 32..].try_into().unwrap()).unwrap();
    public.verify(&blob[..signed_to], &ed25519_dalek::Signature::from_slice(&sig[sig.len() - 64..]).unwrap()).expect("the CA's signature");
    assert_eq!(api.call(Method::DELETE, &format!("/ssh-cas/{}", ca["id"].as_str().unwrap()), None).await.0, StatusCode::CONFLICT);
    // Host certificates: a server's own key signed for names under the CA's host domains, and the
    // known_hosts line that trusts them.
    for public in ["com", "co.uk"] {
        assert_eq!(api.call(Method::POST, "/ssh-cas", Some(json!({ "name": "wide", "principals": [], "host_domains": [public] }))).await.0, StatusCode::BAD_REQUEST, "{public}");
    }
    let hosts = api.ok(Method::POST, "/ssh-cas", Some(json!({ "name": "fleet", "principals": [], "host_domains": ["svc.internal"] }))).await;
    let hosts_pub = hosts["public_key"].as_str().unwrap();
    assert_eq!(hosts["known_hosts"], format!("@cert-authority *.svc.internal,svc.internal {hosts_pub}"));
    let server = ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]);
    let mut server_blob = vec![];
    for part in [&b"ssh-ed25519"[..], &server.verifying_key().to_bytes()[..]] {
        server_blob.extend_from_slice(&(part.len() as u32).to_be_bytes());
        server_blob.extend_from_slice(part);
    }
    let line = format!("ssh-ed25519 {} root@db1", base64::engine::general_purpose::STANDARD.encode(&server_blob));
    let sign_at = format!("/ssh-cas/{}/sign-host", hosts["id"].as_str().unwrap());
    let sign = |hosts: Value| api.call(Method::POST, &sign_at, Some(json!({ "public_key": line, "hosts": hosts, "days": 30 })));
    assert_eq!(sign(json!(["db1.example.com"])).await.0, StatusCode::BAD_REQUEST, "a name outside its domains");
    let (s, signed) = sign(json!(["db1.svc.internal"])).await;
    assert_eq!(s, StatusCode::OK, "{signed}");
    let blob = base64::engine::general_purpose::STANDARD.decode(signed["certificate"].as_str().unwrap().split(' ').nth(1).unwrap()).unwrap();
    let mut p = 0usize;
    assert_eq!(take(&blob, &mut p), b"ssh-ed25519-cert-v01@openssh.com");
    take(&blob, &mut p); // nonce
    assert_eq!(take(&blob, &mut p), server.verifying_key().to_bytes(), "the server's own key");
    p += 8;
    assert_eq!(u32::from_be_bytes(blob[p..p + 4].try_into().unwrap()), 2, "a host certificate");
    p += 4;
    take(&blob, &mut p); // key id
    assert_eq!(&take(&blob, &mut p)[4..], b"db1.svc.internal");
    p += 16;
    for _ in 0..3 {
        take(&blob, &mut p);
    }
    let signer = take(&blob, &mut p);
    let signed_to = p;
    let sig = take(&blob, &mut p);
    let fleet_key = base64::engine::general_purpose::STANDARD.decode(hosts_pub.split(' ').nth(1).unwrap()).unwrap();
    assert_eq!(signer, fleet_key);
    ed25519_dalek::VerifyingKey::from_bytes(fleet_key[fleet_key.len() - 32..].try_into().unwrap()).unwrap().verify(&blob[..signed_to], &ed25519_dalek::Signature::from_slice(&sig[sig.len() - 64..]).unwrap()).expect("the CA's signature");
    // A person's own key (in an agent, on a token): a user certificate for logins the CA allows.
    let user_at = format!("/ssh-cas/{}/sign-user", ca["id"].as_str().unwrap());
    assert_eq!(api.call(Method::POST, &user_at, Some(json!({ "public_key": line, "principals": ["root"] }))).await.0, StatusCode::BAD_REQUEST);
    let mine = api.ok(Method::POST, &user_at, Some(json!({ "public_key": line, "principals": ["deploy"], "ttl_minutes": 30 }))).await;
    let blob = base64::engine::general_purpose::STANDARD.decode(mine["certificate"].as_str().unwrap().split(' ').nth(1).unwrap()).unwrap();
    let mut p = 0usize;
    take(&blob, &mut p);
    take(&blob, &mut p);
    assert_eq!(take(&blob, &mut p), server.verifying_key().to_bytes(), "the person's own key");
    p += 8;
    assert_eq!(u32::from_be_bytes(blob[p..p + 4].try_into().unwrap()), 1, "a user certificate");
}

#[tokio::test]
async fn a_sensitive_keys_value_reaches_only_roll_s_runtimes() {
    let api = Api::new().await;
    api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": [{ "op": "set", "key": "PAYMENTS_KEY", "value": "sk_live_sensitive", "tags": ["sensitive"] }, { "op": "set", "key": "WRAPPED", "value": "Bearer ${PAYMENTS_KEY}" }] }))).await;
    // Not to a person or a token: left out of values (named in `withheld`), refused on its own, not
    // carried by a reference, not in history.
    let v = api.values("production", "/").await;
    assert!(v["values"].get("PAYMENTS_KEY").is_none() && v["withheld"] == json!(["PAYMENTS_KEY"]), "{v}");
    assert!(v["problems"].to_string().contains("PAYMENTS_KEY is sensitive"), "{v}");
    assert_eq!(api.call(Method::GET, "/environments/production/secrets/PAYMENTS_KEY/value", None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(api.call(Method::GET, "/environments/production/secrets/PAYMENTS_KEY/versions/1/value", None).await.0, StatusCode::FORBIDDEN);
    // Sensitive is for good.
    let (s, e) = api.call(Method::PUT, "/environments/production/secrets/PAYMENTS_KEY", Some(json!({ "tags": [] }))).await;
    assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains("sensitive for good"), "{e}");
    // roll's own runtimes (apps, previews, checks) get it, and the reference resolves for them.
    let (s, p) = (api.secrets.clone(), api.project.clone());
    let got = tokio::task::spawn_blocking(move || s.values(&p, "production", "/", true, Some("app:web"), &roll_secrets::Caller::consumer("app:web")).unwrap()).await.unwrap();
    assert_eq!((got.values["PAYMENTS_KEY"].expose(), got.values["WRAPPED"].expose()), ("sk_live_sensitive", "Bearer sk_live_sensitive"));
    // …and are told which values never to show, a key that holds one through a reference too.
    assert_eq!(got.sensitive, ["PAYMENTS_KEY", "WRAPPED"]);
    // A sandbox's or a check's resolve isn't a runtime's.
    let (s, p) = (api.secrets.clone(), api.project.clone());
    let sandbox = tokio::task::spawn_blocking(move || s.values(&p, "production", "/", true, Some("sandbox:x"), &roll_secrets::Caller::consumer("sandbox:x").not_runtime()).unwrap()).await.unwrap();
    assert!(!sandbox.values.contains_key("PAYMENTS_KEY"));
    // Nor is anyone's read of an untrusted preview: roll apps deploying a fork's preview doesn't
    // get its unprotected base's sensitive key.
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY", Some(json!({ "value": "sk_test_sensitive", "tags": ["sensitive"] }))).await;
    api.ok(Method::POST, "/environments", Some(json!({ "slug": "pv-7", "base": "staging" }))).await;
    let (s, p) = (api.secrets.clone(), api.project.clone());
    let preview = tokio::task::spawn_blocking(move || s.values(&p, "pv-7", "/", true, Some("app:web"), &roll_secrets::Caller::consumer("app:web")).unwrap()).await.unwrap();
    assert!(!preview.values.contains_key("STRIPE_KEY") && preview.withheld == ["STRIPE_KEY"], "{:?}", preview.withheld);

    // Tagging a key that already has a value: its older versions hold the value too, so none shows.
    api.ok(Method::PUT, "/environments/staging/secrets/OLD_KEY", Some(json!({ "value": "sk_live_old" }))).await;
    api.ok(Method::PUT, "/environments/staging/secrets/OLD_KEY", Some(json!({ "tags": ["sensitive"] }))).await;
    assert_eq!(api.call(Method::GET, "/environments/staging/secrets/OLD_KEY/versions/1/value", None).await.0, StatusCode::FORBIDDEN);
    // No test of a guess: the leak scan, a local file and another environment's copy don't say.
    assert_eq!(api.ok(Method::POST, "/leaks", Some(json!({ "candidates": ["sk_live_old"] }))).await, json!([]));
    api.ok(Method::PUT, "/environments/development/secrets/OLD_KEY", Some(json!({ "value": "sk_live_old" }))).await;
    let cmp = api.ok(Method::GET, "/compare?from=development&to=staging", None).await;
    assert_eq!(cmp["rows"].as_array().unwrap().iter().find(|r| r["key"] == "OLD_KEY").unwrap()["status"], "hidden");
}

#[tokio::test]
async fn a_totp_key_hands_out_its_current_code_to_whoever_may_reveal_it() {
    let api = Api::new().await;
    // RFC 6238's own SHA1 test seed (`12345678901234567890`, base32), 8 digits.
    const SEED: &str = "otpauth://totp/roll:ci?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&digits=8";
    api.ok(Method::PUT, "/required", Some(json!([{ "key": "REGISTRY_TOTP", "type": "totp" }]))).await;
    // A malformed seed is refused by its rule, never echoed.
    let (s, e) = api.call(Method::PUT, "/environments/production/secrets/REGISTRY_TOTP", Some(json!({ "value": "otpauth://totp/roll:ci?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&digits=9" }))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("REGISTRY_TOTP must be a TOTP seed (an otpauth://totp/ URI or base32): digits must be 6–8")));
    api.set("production", "REGISTRY_TOTP", SEED).await;
    api.set("production", "PLAIN", "not-a-seed").await;

    // The code for the step that ends at `expires_at`, as RFC 6238 computes it.
    let t = roll_secrets::totp::parse(SEED).unwrap();
    assert_eq!(t.code_at(59).as_str(), "94287082", "RFC 6238, Appendix B");
    let c = api.ok(Method::GET, "/environments/production/secrets/REGISTRY_TOTP/totp", None).await;
    let expires = c["expires_at"].as_i64().unwrap();
    assert!(expires > code_sql::now_ms() && expires <= code_sql::now_ms() + 30_000, "{expires}");
    let code = c["code"].as_str().unwrap().to_string();
    assert_eq!(code, t.code_at(expires as u64 / 1000 - 30).as_str());
    assert_eq!(api.call(Method::GET, "/environments/production/secrets/PLAIN/totp", None).await.0, StatusCode::BAD_REQUEST);

    // It's a credential: `reveal` on the folder, as a reveal; `read` alone isn't enough.
    let read = api.token(json!({ "environments": ["production"], "paths": ["/"], "actions": ["read"] }));
    assert_eq!(api.req(Method::GET, "/environments/production/secrets/REGISTRY_TOTP/totp", None, Some(&read)).await.0, StatusCode::FORBIDDEN);
    let reveal = api.token(json!({ "environments": ["production"], "paths": ["/"], "actions": ["read", "reveal"] }));
    let (s, c) = api.req(Method::GET, "/environments/production/secrets/REGISTRY_TOTP/totp", None, Some(&reveal)).await;
    assert!(s == StatusCode::OK && c["code"].as_str().is_some_and(|c| c.len() == 8), "{s} {c}");

    // A read that asks for codes gets `<KEY>_CODE` beside the seed (`roll run --totp-codes`).
    let v = api.ok(Method::GET, "/environments/production/values?codes=1", None).await;
    assert!(v["values"]["REGISTRY_TOTP_CODE"].as_str().is_some_and(|c| c.len() == 8) && v["values"].get("PLAIN_CODE").is_none(), "{v}");
    assert!(api.values("production", "/").await["values"].get("REGISTRY_TOTP_CODE").is_none());

    // Audited as `secrets.totp`, the code nowhere in it.
    let audit = api.ok(Method::GET, "/audit?action=secrets.totp", None).await;
    let rows = audit.as_array().unwrap();
    assert!(rows.len() == 2 && rows.iter().all(|r| r["summary"] == "made a one-time code of REGISTRY_TOTP" && r["keys"] == json!(["REGISTRY_TOTP"])), "{audit}");
    for r in rows {
        let mut r = r.clone();
        r.as_object_mut().unwrap().retain(|k, _| !["id", "at", "seq"].contains(&k.as_str()));
        assert!(!r.to_string().contains(&code), "{r}");
    }
}

#[tokio::test]
async fn an_encryption_key_encrypts_for_apps_and_never_leaves_roll() {
    use base64::Engine;
    let b64 = |s: &[u8]| base64::engine::general_purpose::STANDARD.encode(s);
    let api = Api::new().await;
    let k = api.ok(Method::POST, "/transit", Some(json!({ "name": "cards" }))).await;
    assert_eq!(k["latest_version"], 1);
    assert!(!api.ok(Method::GET, "/transit", None).await.to_string().contains("\"key\""), "no key material in a listing");
    // Encrypt bound to a context; it opens under that context only.
    let one = api.ok(Method::POST, "/transit/cards/encrypt", Some(json!({ "plaintext": b64(b"4111 1111 1111 1111"), "context": "order:42" }))).await;
    let ct = one["ciphertext"].as_str().unwrap().to_string();
    assert!(ct.starts_with("roll:v1:"), "{ct}");
    let open = |ct: &str, context: &str| api.call(Method::POST, "/transit/cards/decrypt", Some(json!({ "ciphertext": ct, "context": context })));
    assert_eq!(open(&ct, "order:42").await.1["plaintext"], b64(b"4111 1111 1111 1111"));
    assert_eq!(open(&ct, "order:43").await.0, StatusCode::BAD_REQUEST);
    // Rotation: new ciphertexts use v2, old ones still open, rewrap moves them without the plaintext.
    assert_eq!(api.ok(Method::POST, "/transit/cards/rotate", None).await["latest_version"], 2);
    let re = api.ok(Method::POST, "/transit/cards/rewrap", Some(json!({ "items": [{ "ciphertext": ct, "context": "order:42" }] }))).await;
    let ct2 = re["items"][0]["ciphertext"].as_str().unwrap().to_string();
    assert!(ct2.starts_with("roll:v2:") && open(&ct2, "order:42").await.1["plaintext"] == b64(b"4111 1111 1111 1111"));
    // Retiring v1: what it encrypted no longer opens.
    api.ok(Method::PATCH, "/transit/cards", Some(json!({ "min_decrypt_version": 2 }))).await;
    let (s, e) = open(&ct, "order:42").await;
    assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains("retired"), "{e}");
    // A data key for local envelope encryption: its ciphertext opens to it.
    let dk = api.ok(Method::POST, "/transit/cards/datakey", Some(json!({}))).await;
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(dk["plaintext"].as_str().unwrap()).unwrap().len(), 32);
    assert_eq!(api.call(Method::POST, "/transit/cards/decrypt", Some(json!({ "ciphertext": dk["ciphertext"] }))).await.1["plaintext"], dk["plaintext"]);
    // Signatures: by the newest version, checked here or by anyone with the public key.
    let sig = api.ok(Method::POST, "/transit/cards/sign", Some(json!({ "input": b64(b"invoice 7") }))).await["signature"].as_str().unwrap().to_string();
    assert!(sig.starts_with("roll:v2:"), "{sig}");
    let check = |input: &[u8], sig: &str| api.ok(Method::POST, "/transit/cards/verify", Some(json!({ "items": [{ "input": b64(input), "signature": sig }] })));
    assert_eq!(check(b"invoice 7", &sig).await["items"][0]["valid"], true);
    assert_eq!(check(b"invoice 8", &sig).await["items"][0]["valid"], false);
    assert_eq!(check(b"invoice 7", &sig.replace("roll:v2:", "roll:v9:")).await["items"][0]["valid"], false, "an unknown version doesn't verify");
    let public = api.ok(Method::GET, "/transit/cards/public", None).await;
    let raw = base64::engine::general_purpose::STANDARD.decode(public[1]["public_key"].as_str().unwrap()).unwrap();
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&sig["roll:v2:".len()..]).unwrap();
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, raw).verify(b"invoice 7", &bytes).expect("a stock Ed25519 check passes");
    // HMACs: stable for one input (a blind index), checked only by writers.
    let mac = |input: &[u8]| api.ok(Method::POST, "/transit/cards/hmac", Some(json!({ "input": b64(input) })));
    let m = mac(b"ada@example.com").await["hmac"].clone();
    assert_eq!(mac(b"ada@example.com").await["hmac"], m);
    assert_ne!(mac(b"bob@example.com").await["hmac"], m);
    assert_eq!(api.ok(Method::POST, "/transit/cards/hmac/verify", Some(json!({ "input": b64(b"ada@example.com"), "hmac": m }))).await["valid"], true);
    // A backup carries the key: restored into another project, it still opens what it encrypted.
    let secret = [9u8; 32];
    let b = api.ok(Method::POST, "/backup", Some(json!({ "recipient": roll_secrets::crypto::public_key(&secret) }))).await;
    let sealed: roll_secrets::crypto::Sealed = serde_json::from_value(b["sealed"].clone()).unwrap();
    let archive: Value = serde_json::from_slice(&roll_secrets::crypto::open_sealed(&secret, b["context"].as_str().unwrap(), &sealed).unwrap()).unwrap();
    let (_, other) = api.call(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "restored" }))).await;
    let to = |p: &str| format!("/v1/secrets/projects/{}{p}", other["id"].as_str().unwrap());
    api.ok(Method::POST, &to("/restore"), Some(archive)).await;
    let back = api.call(Method::POST, &to("/transit/cards/decrypt"), Some(json!({ "ciphertext": ct2, "context": "order:42" }))).await;
    assert_eq!(back.1["plaintext"], b64(b"4111 1111 1111 1111"), "{back:?}");
    assert_eq!(api.ok(Method::POST, &to("/transit/cards/sign"), Some(json!({ "input": b64(b"invoice 7") }))).await["signature"], sig.as_str(), "and signs as it did");
    // A key brought from Vault: Vault's own ciphertexts (aes256-gcm96, no AAD) open under the
    // version that was Vault's, and rewrap into roll's form.
    let vault_ct = |key: &[u8; 32], plain: &[u8]| {
        use aes_gcm::aead::{Aead, KeyInit};
        let nonce = [7u8; 12];
        let mut raw = nonce.to_vec();
        raw.extend(aes_gcm::Aes256Gcm::new_from_slice(key).unwrap().encrypt(aes_gcm::Nonce::from_slice(&nonce), plain).unwrap());
        raw
    };
    let (k1, k2) = ([3u8; 32], [4u8; 32]);
    assert_eq!(api.call(Method::POST, "/transit", Some(json!({ "name": "legacy", "key": b64(&[1u8; 16]) }))).await.0, StatusCode::BAD_REQUEST, "not 32 bytes");
    let made = api.ok(Method::POST, "/transit", Some(json!({ "name": "legacy", "key": b64(&k1), "vault_version": 1 }))).await;
    assert_eq!((made["versions"][0]["imported"].clone(), made["versions"][0]["vault_version"].clone()), (json!(true), json!(1)));
    api.ok(Method::POST, "/transit/legacy/rotate", Some(json!({ "key": b64(&k2), "vault_version": 2 }))).await;
    for (key, n, plain) in [(&k1, 1, &b"from vault v1"[..]), (&k2, 2, &b"from vault v2"[..])] {
        let ct = format!("vault:v{n}:{}", b64(&vault_ct(key, plain)));
        assert_eq!(api.ok(Method::POST, "/transit/legacy/decrypt", Some(json!({ "ciphertext": ct }))).await["plaintext"], b64(plain));
        let moved = api.ok(Method::POST, "/transit/legacy/rewrap", Some(json!({ "ciphertext": ct }))).await["ciphertext"].as_str().unwrap().to_string();
        assert!(moved.starts_with("roll:v2:"), "{moved}");
        assert_eq!(api.ok(Method::POST, "/transit/legacy/decrypt", Some(json!({ "ciphertext": moved }))).await["plaintext"], b64(plain));
    }
    let wrong = format!("vault:v1:{}", b64(&vault_ct(&k2, b"x")));
    assert_eq!(api.call(Method::POST, "/transit/legacy/decrypt", Some(json!({ "ciphertext": wrong }))).await.0, StatusCode::BAD_REQUEST);
    // Deleting makes everything unreadable for good: only with the name repeated.
    assert_eq!(api.call(Method::DELETE, "/transit/cards", None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(api.call(Method::DELETE, "/transit/cards?confirm=cards", None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(open(&ct2, "order:42").await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reads_unlike_the_reader_are_alerted() {
    let api = Api::new().await;
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true, "notify_url": "https://hooks.example/T0/B0/x" }))).await;
    let sweep = || {
        let s = api.secrets.clone();
        async move { tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap() }
    };
    sweep().await;
    // Reads as the audit log records them: a CI token from its usual address, then another; a
    // person revealing values one by one.
    let read = |actor: &str, action: &str, ip: &str| {
        let (p, actor, action, ip) = (api.project.clone(), actor.to_string(), action.to_string(), ip.to_string());
        api.db
            .write(move |c| {
                c.execute(
                    "INSERT INTO audit (project_id, at, actor, action, env, path, keys, summary, ip) VALUES (?, (SELECT COALESCE(MAX(at), 0) + 1 FROM audit), ?, ?, 'production', '/', '[\"API_KEY\"]', '', ?)",
                    code_sql::params![p, actor, action, ip],
                )
            })
            .unwrap();
    };
    read("token:tok_ci", "secrets.resolve", "10.0.0.1");
    read("token:tok_ci", "secrets.resolve", "10.0.0.1");
    for _ in 0..30 {
        read("alice", "secrets.reveal", "10.0.0.9");
    }
    read("token:tok_ci", "secrets.resolve", "203.0.113.7");
    sweep().await;
    sweep().await;
    let sent = api.sent.0.lock().unwrap().clone();
    // Notices queued in one sweep go out in no particular order.
    let mut alerts: Vec<Value> = sent.iter().map(|n| n["alert"].clone()).collect();
    alerts.sort_by_key(|a| a["kind"].as_str().map(String::from));
    assert_eq!(alerts, vec![json!({ "kind": "many_reveals", "actor": "alice", "count": 30 }), json!({ "kind": "new_address", "actor": "token:tok_ci", "ip": "203.0.113.7" })], "{sent:?}");
    // The audit log keeps them too: the environment's activity, and audit sinks.
    let logged = api.ok(Method::GET, "/audit?action=secrets.alert", None).await;
    assert_eq!(logged.as_array().map(Vec::len), Some(2), "{logged}");
}

/// A decoy key (docs/secrets.md, "Decoys") reads like any other key, and using its token raises
/// `decoy_used`: the audit log, health first, and the environment's notify URL. Never the value.
#[tokio::test]
async fn a_decoy_key_reads_as_any_other_and_its_use_is_alerted() {
    let api = Api::new().await;
    api.ok(Method::PUT, "/environments/staging/policy", Some(json!({ "protected": false, "notify_url": "https://hooks.example/T0/B0/x" }))).await;
    let made = api.ok(Method::POST, "/environments/staging/decoys", Some(json!({ "key": "LEGACY_DEPLOY_TOKEN", "kind": "rlt", "path": "/ci" }))).await;
    assert_eq!((made["key"].as_str(), made["decoy"]["kind"].as_str(), made["decoy"]["uses"].as_i64()), (Some("LEGACY_DEPLOY_TOKEN"), Some("rlt"), Some(0)), "{made}");
    // Only a new key, and only the kinds roll can watch.
    assert_eq!(api.call(Method::POST, "/environments/staging/decoys", Some(json!({ "key": "LEGACY_DEPLOY_TOKEN", "kind": "rlt", "path": "/ci" }))).await.0, StatusCode::CONFLICT);
    assert_eq!(api.call(Method::POST, "/environments/staging/decoys", Some(json!({ "key": "OLD_AWS_KEY", "kind": "aws" }))).await.0, StatusCode::BAD_REQUEST);
    // Its value is a roll token like any other, in the values a deploy reads.
    let value = api.ok(Method::GET, "/environments/staging/secrets/LEGACY_DEPLOY_TOKEN/value?path=/ci", None).await["value"].as_str().unwrap().to_string();
    assert!(value.starts_with("rlt_"), "{value}");
    assert_eq!(api.ok(Method::GET, "/environments/staging/values?path=/ci", None).await["values"]["LEGACY_DEPLOY_TOKEN"], value);
    // Whoever found it tries it: refused as an unknown token is.
    assert_eq!(api.req(Method::GET, "/v1/secrets/whoami", None, Some(&value)).await.0, StatusCode::UNAUTHORIZED);
    let health = api.ok(Method::GET, "/health", None).await;
    assert_eq!((health[0]["issue"].as_str(), health[0]["key"].as_str(), health[0]["path"].as_str()), (Some("decoy_used"), Some("LEGACY_DEPLOY_TOKEN"), Some("/ci")), "{health}");
    assert!(health[0]["detail"].as_str().unwrap().contains("203.0.113.7"), "{health}");
    let logged = api.ok(Method::GET, "/audit?action=secrets.alert", None).await;
    assert_eq!((logged[0]["action"].as_str(), logged[0]["ip"].as_str(), logged[0]["environment"].as_str()), (Some("secrets.alert.decoy_used"), Some("203.0.113.7"), Some("staging")), "{logged}");
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap();
    let sent = api.sent.0.lock().unwrap().clone();
    let alert = sent.iter().find(|n| n["alert"]["kind"] == "decoy_used").unwrap_or_else(|| panic!("{sent:?}"));
    assert_eq!((alert["alert"]["key"].as_str(), alert["alert"]["ip"].as_str()), (Some("LEGACY_DEPLOY_TOKEN"), Some("203.0.113.7")));
    // Never the value, anywhere it was told.
    for told in [json!(sent).to_string(), logged.to_string(), health.to_string()] {
        assert!(!told.contains(&value), "{told}");
    }
    // A second try within the hour is counted, not told again.
    api.req(Method::GET, "/v1/secrets/whoami", None, Some(&value)).await;
    let listed = api.ok(Method::GET, "/environments/staging/secrets?path=/ci", None).await;
    assert_eq!(listed[0]["decoy"]["uses"], 2, "{listed}");
    assert_eq!(api.ok(Method::GET, "/audit?action=secrets.alert", None).await.as_array().map(Vec::len), Some(1));
}

/// Only the project's admins, signed in or the operator, can tell a key is a decoy. To any token,
/// an admin's too (an agent on roll MCP), it answers as any other key does: listings, the key,
/// health, the audit log and its export, the sinks and notify URLs the token set, and the daily
/// reminder. No audit row marks the plant, and no token plants one. The session sees where it was
/// last used (the route pattern and the address); nothing else does.
#[tokio::test]
async fn only_an_admins_session_can_tell_a_key_is_a_decoy() {
    let api = Api::new().await;
    api.ok(Method::PUT, "/environments/production/secrets/STRIPE_KEY", Some(json!({ "value": "sk_test_not_a_real_key_0000" }))).await;
    let dev = api.token(json!({ "actions": ["read", "reveal", "write", "delete"] }));
    let admin = api.token(json!({ "actions": ["read", "reveal", "write", "delete", "admin"] }));
    let (s, token_sink) = api.req(Method::POST, "/audit/sinks", Some(json!({ "url": "https://siem.example/token" })), Some(&admin)).await;
    assert!(s.is_success(), "{s} {token_sink}");
    let operator_sink = api.ok(Method::POST, "/audit/sinks", Some(json!({ "url": "https://siem.example/operator" }))).await;
    // Production's channel, set by the admin token, with a staleness limit.
    let (s, _) = api.req(Method::PUT, "/environments/production/policy", Some(json!({ "protected": false, "notify_url": "https://hooks.example/T0/B0/token" })), Some(&admin)).await;
    assert!(s.is_success(), "{s}");
    api.ok(Method::PATCH, "/environments/production", Some(json!({ "max_age_days": 90 }))).await;
    // Planted from an admin's session: the person behind it owns it (and is mailed), not the operator.
    let base = format!("/v1/secrets/projects/{}", api.project);
    let (s, planted) = api.signed_in("root", true, Method::POST, &format!("{base}/environments/production/decoys"), Some(json!({ "key": "BACKUP_DB_TOKEN", "kind": "rlt" }))).await;
    assert!(s.is_success(), "{s} {planted}");
    // Used once.
    let (_, value) = api.req(Method::GET, "/environments/production/secrets/BACKUP_DB_TOKEN/value", None, Some(&dev)).await;
    let value = value["value"].as_str().unwrap().to_string();
    assert_eq!(api.gate.tokens.lock().unwrap()[&value].user.as_deref(), Some("usr_root"));
    api.req(Method::GET, "/v1/secrets/whoami", None, Some(&value)).await;
    for t in [&dev, &admin] {
        assert_eq!(api.req(Method::POST, "/environments/production/decoys", Some(json!({ "key": "OTHER", "kind": "rlt" })), Some(t)).await.0, StatusCode::FORBIDDEN);
        let (_, listed) = api.req(Method::GET, "/environments/production/secrets", None, Some(t)).await;
        let shape = |key: &str| listed.as_array().unwrap().iter().find(|s| s["key"] == key).unwrap().as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        assert_eq!(shape("BACKUP_DB_TOKEN"), shape("STRIPE_KEY"), "{listed}");
        let (_, one) = api.req(Method::GET, "/environments/production/secrets/BACKUP_DB_TOKEN", None, Some(t)).await;
        assert!(one.get("decoy").is_none(), "{one}");
        let (_, health) = api.req(Method::GET, "/health", None, Some(t)).await;
        assert!(!health.to_string().contains("decoy") && !health.to_string().contains("203.0.113.7"), "{health}");
        let (_, audit) = api.req(Method::GET, "/audit", None, Some(t)).await;
        assert!(!audit.to_string().contains("decoy"), "{audit}");
        let (_, export) = api.req(Method::GET, "/audit?format=jsonl", None, Some(t)).await;
        assert!(export.as_str().is_some_and(|e| e.contains("BACKUP_DB_TOKEN") && !e.contains("decoy")), "{export}");
    }
    // No row marks the plant, even for the operator: the key's write is as any new key's (but for
    // its change's number).
    let all = api.ok(Method::GET, "/audit", None).await;
    let rows = all.as_array().unwrap();
    assert!(!rows.iter().any(|r| r["action"] == "secrets.decoy"), "{all}");
    let write = |key: &str| rows.iter().rev().find(|r| r["keys"] == json!([key])).map(|r| (r["action"].clone(), r["summary"].as_str().unwrap().split_once(' ').unwrap().1.to_string())).unwrap();
    assert_eq!(write("BACKUP_DB_TOKEN"), write("STRIPE_KEY"), "{all}");
    // An admin's session, as the operator, sees it all: who planted it on the key, its use.
    let (_, listed) = api.signed_in("root", true, Method::GET, &format!("{base}/environments/production/secrets"), None).await;
    let decoy = listed.as_array().unwrap().iter().find(|s| s["key"] == "BACKUP_DB_TOKEN").unwrap()["decoy"].clone();
    assert_eq!((decoy["uses"].as_i64(), decoy["created_by"].as_str(), decoy["created_at"].is_i64()), (Some(1), Some("usr_root"), true), "{listed}");
    assert_eq!((decoy["used_ip"].as_str(), decoy["used_route"].as_str(), decoy["used_endpoint"].as_str(), decoy["used_at"].is_i64()), (Some("203.0.113.7"), Some("/v1/secrets/whoami"), None, true), "{decoy}");
    let (_, health) = api.signed_in("root", true, Method::GET, &format!("{base}/health"), None).await;
    assert_eq!(health[0]["issue"], "decoy_used", "{health}");
    assert_eq!(health[0]["decoy"], decoy, "{health}");
    assert!(health[0]["detail"].as_str().unwrap().contains("from 203.0.113.7 calling /v1/secrets/whoami"), "{health}");
    for told in [listed.to_string(), health.to_string()] {
        assert!(!told.contains(&value), "{told}");
    }
    let (_, audit) = api.signed_in("root", true, Method::GET, &format!("{base}/audit"), None).await;
    assert!(audit.to_string().contains("secrets.alert.decoy_used"), "{audit}");
    // A member's session sees them only where their role manages the project (docs/members.md).
    let p = api.project.clone();
    let (_, inv) = api.call(Method::POST, &format!("/v1/workspace/projects/{p}/invites"), Some(json!({ "email": "cat@example.com", "role": "developer" }))).await;
    let t = inv["url"].as_str().unwrap().split("token=").nth(1).unwrap().to_string();
    assert_eq!(api.signed_in("cat", false, Method::POST, &format!("/v1/workspace/invites/{t}/accept"), None).await.0, StatusCode::OK);
    let cat_sees = || async {
        let (s, listed) = api.signed_in("cat", false, Method::GET, &format!("{base}/environments/production/secrets"), None).await;
        assert!(s.is_success(), "{listed}");
        listed.as_array().unwrap().iter().find(|s| s["key"] == "BACKUP_DB_TOKEN").unwrap().get("decoy").is_some()
    };
    assert!(!cat_sees().await, "a developer's session is told nothing");
    assert_eq!(api.call(Method::PATCH, &format!("/v1/workspace/projects/{p}/members/usr_cat"), Some(json!({ "role": "admin" }))).await.0, StatusCode::OK);
    assert!(cat_sees().await, "an admin's session is");
    // Both keys past the limit; a day of sweeps: sinks, notices, the reminder.
    api.db.write(|c| c.execute("UPDATE audit SET at = at - 60000", ())).unwrap();
    api.db.write(|c| c.execute("UPDATE secrets SET updated_at = updated_at - 8640000000", ())).unwrap();
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || {
        s.sweep().unwrap();
        s.sweep().unwrap();
    })
    .await
    .unwrap();
    let sent = api.sent.0.lock().unwrap().clone();
    let actions = |sink: &Value| -> Vec<String> {
        sent.iter().filter(|b| b["sink"] == sink["id"]).flat_map(|b| b["events"].as_array().unwrap().iter().map(|e| e["action"].as_str().unwrap().to_string())).collect()
    };
    let (theirs, ours) = (actions(&token_sink), actions(&operator_sink));
    assert!(ours.iter().any(|a| a == "secrets.alert.decoy_used"), "{ours:?}");
    assert!(theirs.iter().any(|a| a == "secrets.write") && !theirs.iter().any(|a| a.contains("decoy")), "{theirs:?}");
    // The token's channel hears nothing of the decoy, and its reminder calls both keys stale.
    let notices: Vec<&Value> = sent.iter().filter(|b| b.get("sink").is_none()).collect();
    assert!(!json!(notices).to_string().contains("decoy") && !json!(notices).to_string().contains("/v1/secrets/whoami"), "{notices:?}");
    let reminder = notices.iter().find(|n| n["findings"].is_array()).unwrap_or_else(|| panic!("{notices:?}"));
    let mut stale: Vec<&str> = reminder["findings"].as_array().unwrap().iter().filter(|f| f["issue"] == "stale").map(|f| f["key"].as_str().unwrap()).collect();
    stale.sort();
    assert_eq!(stale, ["BACKUP_DB_TOKEN", "STRIPE_KEY"], "{reminder}");
}

/// Deleting a decoy key, or giving it a value of its own, ends the decoy: its token is revoked
/// after the write.
#[tokio::test]
async fn deleting_a_decoy_key_revokes_its_token() {
    let api = Api::new().await;
    let decoys = || api.gate.decoys.lock().unwrap().clone();
    let live = |id: &str| api.gate.tokens.lock().unwrap().values().any(|g| &*g.id == id);
    api.ok(Method::POST, "/environments/development/decoys", Some(json!({ "key": "OLD_CI_TOKEN", "kind": "rlt" }))).await;
    api.ok(Method::POST, "/environments/development/decoys", Some(json!({ "key": "SPARE_TOKEN", "kind": "rlt" }))).await;
    let ids: Vec<String> = decoys().into_iter().collect();
    assert!(ids.iter().all(|id| live(id)));
    api.ok(Method::DELETE, "/environments/development/secrets/OLD_CI_TOKEN", None).await;
    api.ok(Method::PUT, "/environments/development/secrets/SPARE_TOKEN", Some(json!({ "value": "an-ordinary-value-now" }))).await;
    assert!(ids.iter().all(|id| !live(id)), "{ids:?}");
    let listed = api.ok(Method::GET, "/environments/development/secrets", None).await;
    assert!(listed[0].get("decoy").is_none(), "{listed}");
}

/// A Postgres decoy is a role of a database on the canvas whose logins roll Postgres refuses: a
/// login (as its proxy reports it) raises `decoy_used` in health and the audit log, and deleting
/// the key drops the role.
#[tokio::test]
async fn a_postgres_decoy_is_a_role_whose_login_is_alerted_and_that_goes_with_its_key() {
    let api = Api::new().await;
    let plant = |resource: Option<&str>| json!({ "key": "LEGACY_DATABASE_URL", "kind": "postgres", "resource": resource });
    let (s, e) = api.call(Method::POST, "/environments/production/decoys", Some(plant(Some("prj_fakepg")))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("postgres prj_fakepg isn't in this project")));
    assert_eq!(api.call(Method::POST, "/environments/production/decoys", Some(plant(None))).await.0, StatusCode::BAD_REQUEST);
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{}/resources/postgres/prj_fakepg", api.project), Some(json!({ "x": 0, "y": 0 }))).await;
    let made = api.ok(Method::POST, "/environments/production/decoys", Some(plant(Some("prj_fakepg")))).await;
    assert_eq!(made["decoy"]["kind"], "postgres", "{made}");
    let value = api.ok(Method::GET, "/environments/production/secrets/LEGACY_DATABASE_URL/value", None).await["value"].as_str().unwrap().to_string();
    assert!(value.starts_with("postgres://roll_fakedecoy0:"), "{value}");
    let handle = api.pg.0.lock().unwrap().first().cloned().expect("a decoy role");

    // roll Postgres' proxy refused a login as the role and said so (rolld's `tell_postgres_decoys`).
    let s = api.secrets.clone();
    let h = handle.clone();
    let used = tokio::task::spawn_blocking(move || (s.decoy_used("postgres", &h, Some("198.51.100.23"), "ep_fake"), s.decoy_used("rlt", &h, None, "/"))).await.unwrap();
    assert_eq!((used.0.unwrap(), used.1.unwrap()), (true, false), "another kind's handle isn't this decoy");
    let health = api.ok(Method::GET, "/health", None).await;
    assert_eq!((health[0]["issue"].as_str(), health[0]["key"].as_str()), (Some("decoy_used"), Some("LEGACY_DATABASE_URL")), "{health}");
    assert!(health[0]["detail"].as_str().unwrap().contains("from 198.51.100.23 on Postgres endpoint ep_fake"), "{health}");
    let d = &health[0]["decoy"];
    assert_eq!((d["used_endpoint"].as_str(), d["used_route"].as_str(), d["used_ip"].as_str(), d["uses"].as_i64()), (Some("ep_fake"), None, Some("198.51.100.23"), Some(1)), "{health}");
    let logged = api.ok(Method::GET, "/audit?action=secrets.alert", None).await;
    assert_eq!((logged[0]["action"].as_str(), logged[0]["ip"].as_str()), (Some("secrets.alert.decoy_used"), Some("198.51.100.23")), "{logged}");
    assert!(logged[0]["summary"].as_str().unwrap().contains("Postgres endpoint ep_fake"), "{logged}");
    for told in [logged.to_string(), health.to_string()] {
        assert!(!told.contains(&value) && !told.contains("fake-decoy-password"), "{told}");
    }

    api.ok(Method::DELETE, "/environments/production/secrets/LEGACY_DATABASE_URL", None).await;
    assert!(api.pg.0.lock().unwrap().is_empty(), "the role went with its key");
}

/// A roll-access rule naming one encryption key (`{id: "prj_…", index: "payments"}`) reaches that
/// key alone: the list shows it, its operations work, another key's are refused.
#[tokio::test]
async fn a_rule_naming_one_encryption_key_reaches_it_alone() {
    use roll_auth::scope::{Action as A, Effect, Resources, Rule, Scope, Selector};
    let api = Api::new().await;
    for name in ["payments", "billing"] {
        api.ok(Method::POST, "/transit", Some(json!({ "name": name }))).await;
    }
    let rule = Rule {
        product: "secrets".into(),
        resources: Resources::Some(vec![Selector::Id { id: api.project.clone(), index: Some("payments".into()) }]),
        environments: None,
        paths: None,
        unprotected: false,
        actions: BTreeMap::from([(A::Read, Effect::Allow), (A::Write, Effect::Allow)]),
    };
    let grant = roll_auth::Grant { user: None, ask_otherwise: false, id: "tok_pay".into(), name: "pay".into(), kind: roll_auth::GrantKind::Token, scope: Arc::new(Scope(vec![rule])) };
    let call = |method: Method, path: &str, body: Option<Value>| {
        let mut req = Request::builder().method(method).uri(format!("/v1/secrets/projects/{}{path}", api.project)).header("content-type", "application/json").body(body.map_or(Body::empty(), |b| Body::from(b.to_string()))).unwrap();
        req.extensions_mut().insert(roll_auth::Operator);
        req.extensions_mut().insert(grant.clone());
        let router = api.router.clone();
        async move {
            let resp = router.oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null))
        }
    };
    let (s, keys) = call(Method::GET, "/transit", None).await;
    let names: Vec<&str> = keys.as_array().map(|k| k.iter().filter_map(|k| k["name"].as_str()).collect()).unwrap_or_default();
    assert_eq!((s, names), (StatusCode::OK, vec!["payments"]), "{keys}");
    let plain = json!({ "plaintext": "aGk=" });
    assert_eq!(call(Method::POST, "/transit/payments/encrypt", Some(plain.clone())).await.0, StatusCode::OK);
    assert_eq!(call(Method::POST, "/transit/billing/encrypt", Some(plain)).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_roll_access_grant_reaches_only_what_its_rules_name() {
    use roll_auth::scope::{Action as A, Effect, Resources, Rule, Scope, Selector};
    let api = Api::new().await;
    api.set("production", "DB_PASSWORD", "prod-pw").await;
    api.set("development", "REF", "${production.DB_PASSWORD}").await;
    let rule = Rule {
        product: "secrets".into(),
        resources: Resources::Some(vec![Selector::Project { project: api.project.clone() }]),
        environments: Some(vec!["development".into()]),
        paths: None,
        unprotected: false,
        actions: BTreeMap::from([(A::Read, Effect::Allow), (A::Reveal, Effect::Allow)]),
    };
    let grant = roll_auth::Grant { user: None, ask_otherwise: false, id: "tok_ci".into(), name: "ci".into(), kind: roll_auth::GrantKind::Token, scope: Arc::new(Scope(vec![rule])) };
    // As rolld's gate passes a grant's request on: the operator marker and the grant.
    let call = |path: String| {
        let mut req = Request::builder().uri(format!("/v1/secrets/projects/{}{path}", api.project)).body(Body::empty()).unwrap();
        req.extensions_mut().insert(roll_auth::Operator);
        req.extensions_mut().insert(grant.clone());
        let router = api.router.clone();
        async move {
            let resp = router.oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null))
        }
    };
    let (s, v) = call("/environments/development/values".into()).await;
    assert_eq!((s, v["values"]["REF"].as_str()), (StatusCode::OK, Some("${production.DB_PASSWORD}")), "the reference into production stays unresolved");
    assert_eq!(call("/environments/production/values".into()).await.0, StatusCode::FORBIDDEN);
    let audit = api.ok(Method::GET, "/audit?actor=token:tok_ci", None).await;
    assert_eq!(audit[0]["action"], "secrets.export");
    // A rule naming the project by id (as the dashboard's builder writes it) reaches it the same.
    let by_id = Rule { resources: Resources::Some(vec![Selector::Id { id: api.project.clone(), index: None }]), environments: Some(vec!["development".into()]), paths: None, product: "secrets".into(), unprotected: false, actions: BTreeMap::from([(A::Read, Effect::Allow), (A::Reveal, Effect::Allow)]) };
    let grant = roll_auth::Grant { scope: Arc::new(Scope(vec![by_id])), ..grant.clone() };
    let mut req = Request::builder().uri(format!("/v1/secrets/projects/{}/environments/development/values", api.project)).body(Body::empty()).unwrap();
    req.extensions_mut().insert(roll_auth::Operator);
    req.extensions_mut().insert(grant.clone());
    assert_eq!(api.router.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);
    // A request an approval let through (the gate puts `Approved` beside the grant): its audit row
    // names the approval and who decided it, and the export has them as columns.
    let mut req = Request::builder().uri(format!("/v1/secrets/projects/{}/environments/development/values", api.project)).body(Body::empty()).unwrap();
    req.extensions_mut().insert(roll_auth::Operator);
    req.extensions_mut().insert(grant);
    req.extensions_mut().insert(roll_auth::Approved { id: "apr_fake1".into(), by: vec!["operator".into()], until: Some(4_102_444_800_000) });
    assert_eq!(api.router.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);
    let audit = api.ok(Method::GET, "/audit?actor=token:tok_ci", None).await;
    assert_eq!(audit[0]["approval"], json!({ "id": "apr_fake1", "by": ["operator"], "until": 4_102_444_800_000i64 }));
    assert!(audit[1].get("approval").is_none(), "a request no approval let through names none");
}

#[tokio::test]
async fn a_signed_in_person_manages_only_their_own_projects_secrets() {
    let api = Api::new().await;
    api.set("development", "STRIPE_KEY", "the operator's").await;
    let (s, mine) = api.as_ada(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "ada-shop" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{mine}");
    let mine = mine["id"].as_str().unwrap().to_string();
    let at = |p: &str, rest: &str| format!("/v1/secrets/projects/{p}{rest}");
    let (s, _) = api.as_ada(Method::PUT, &at(&mine, "/environments/development/secrets/STRIPE_KEY"), Some(json!({ "value": "ada's" }))).await;
    assert_eq!(s, StatusCode::OK);
    let (_, v) = api.as_ada(Method::GET, &at(&mine, "/environments/development/values"), None).await;
    assert_eq!(v["values"]["STRIPE_KEY"], "ada's");
    // Anyone else's project doesn't exist for her, and searching finds only hers.
    assert_eq!(api.as_ada(Method::GET, &at(&api.project, "/environments"), None).await.0, StatusCode::NOT_FOUND);
    let (_, found) = api.as_ada(Method::GET, "/v1/secrets/search?key=STRIPE_KEY", None).await;
    assert_eq!(found.as_array().map(|a| a.iter().all(|f| f["project"] == mine.as_str()) && a.len() == 1), Some(true), "{found}");
    // Her canvas proves nothing about who owns what's on it: no sink or link to a product there.
    api.as_ada(Method::PUT, &format!("/v1/workspace/projects/{mine}/resources/bucket/someone-elses"), Some(json!({ "x": 0, "y": 0 }))).await;
    let (s, e) = api.as_ada(Method::POST, &at(&mine, "/audit/sinks"), Some(json!({ "url": "bucket:someone-elses" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    // A roll-access token she made acts as her (the evaluator puts her session beside its grant):
    // a rule naming someone else's project still doesn't reach it.
    let everything = roll_auth::scope::Rule {
        product: "secrets".into(),
        resources: roll_auth::scope::Resources::All,
        environments: None,
        paths: None,
        unprotected: false,
        actions: BTreeMap::from([(roll_auth::scope::Action::Read, roll_auth::scope::Effect::Allow)]),
    };
    let grant = roll_auth::Grant { ask_otherwise: false, user: Some("usr_ada".into()), id: "tok_ada".into(), name: "ada".into(), kind: roll_auth::GrantKind::Token, scope: Arc::new(roll_auth::scope::Scope(vec![everything])) };
    for (project, want) in [(&mine, StatusCode::OK), (&api.project, StatusCode::NOT_FOUND)] {
        let mut req = Request::builder().uri(at(project, "/environments")).body(Body::empty()).unwrap();
        req.extensions_mut().insert(grant.clone());
        req.extensions_mut().insert(roll_auth::SignedIn { user: "usr_ada".into(), handle: "ada".into(), admin: false, code_owner: None, session: "ses_ada".into() });
        assert_eq!(api.router.clone().oneshot(req).await.unwrap().status(), want, "{project}");
    }
    // An admin's own token reaches what its rules say, as the admin's session does.
    let (root, _) = api.gate.grant("root's", roll_secrets::access::rule(&api.project, None, None, &[roll_secrets::Action::Read]), Some("usr_root"));
    assert_eq!(api.req(Method::GET, &at(&api.project, "/environments"), None, Some(&root)).await.0, StatusCode::OK);
    // An admin's project is a whole canvas, though made through a session.
    let (_, theirs) = api.signed_in("root", true, Method::POST, "/v1/workspace/projects", Some(json!({ "name": "root-shop" }))).await;
    let theirs = theirs["id"].as_str().unwrap().to_string();
    api.signed_in("root", true, Method::PUT, &format!("/v1/workspace/projects/{theirs}/resources/bucket/archive"), Some(json!({ "x": 0, "y": 0 }))).await;
    let (s, e) = api.signed_in("root", true, Method::POST, &at(&theirs, "/audit/sinks"), Some(json!({ "url": "bucket:archive" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{e}");
    // A sync she made pushes as her, only while her account can sign in.
    api.as_ada(Method::PUT, &at(&mine, "/environments/staging/secrets/PLATFORM_TOKEN"), Some(json!({ "value": "tok_1" }))).await;
    let (s, e) = api.as_ada(Method::POST, &at(&mine, "/syncs"), Some(json!({ "environment": "staging", "kind": "fake", "target": { "name": "x", "token_key": "PLATFORM_TOKEN" } }))).await;
    assert_eq!(s, StatusCode::CREATED, "{e}");
    api.as_ada(Method::PUT, &at(&mine, "/environments/staging/secrets/A"), Some(json!({ "value": "1" }))).await;
    api.secrets.sweep().unwrap();
    assert_eq!(api.sync.pushes.lock().unwrap().len(), 1);
    *api.people.disabled.lock().unwrap() = true;
    api.as_ada(Method::PUT, &at(&mine, "/environments/staging/secrets/A"), Some(json!({ "value": "2" }))).await;
    api.secrets.sweep().unwrap();
    assert_eq!(api.sync.pushes.lock().unwrap().len(), 1, "nothing pushed once she can't sign in");
    // A sync made with her own token stops the same way (roll-access has no grant for it any more).
    *api.people.disabled.lock().unwrap() = false;
    let token = api.gate.issue("ada's", roll_secrets::access::rule(&mine, None, None, &[roll_secrets::Action::Read, roll_secrets::Action::Reveal, roll_secrets::Action::Admin]), Some("usr_ada"));
    let (s, e) = api.req(Method::POST, &at(&mine, "/syncs"), Some(json!({ "environment": "development", "kind": "fake", "target": { "name": "y", "token_key": "PLATFORM_TOKEN" } })), Some(&token)).await;
    assert_eq!(s, StatusCode::CREATED, "{e}");
    api.as_ada(Method::PUT, &at(&mine, "/environments/development/secrets/PLATFORM_TOKEN"), Some(json!({ "value": "tok_2" }))).await;
    api.secrets.sweep().unwrap();
    let pushed = api.sync.pushes.lock().unwrap().len();
    *api.people.disabled.lock().unwrap() = true;
    api.as_ada(Method::PUT, &at(&mine, "/environments/development/secrets/B"), Some(json!({ "value": "2" }))).await;
    api.secrets.sweep().unwrap();
    assert_eq!(api.sync.pushes.lock().unwrap().len(), pushed, "a token-made sync stops too once she can't sign in");
}

#[tokio::test]
async fn a_customer_managed_key_wraps_the_data_key_and_revoking_it_locks_roll_out() {
    let api = Api::new().await;
    api.set("production", "STRIPE_KEY", "sk_live_123").await;
    let key = "arn:aws:kms:eu-west-1:123456789012:key/k1";
    // A key whose policy would wrap any project's data key (another customer's, say) is refused.
    let (s, e) = api.call(Method::PUT, "/kms", Some(json!({ "key": key }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    assert!(e["error"].as_str().unwrap().contains("doesn't pin the project"), "{e}");
    *api.kms.pinned.lock().unwrap() = Some(api.project.clone());
    let set = api.ok(Method::PUT, "/kms", Some(json!({ "key": key }))).await;
    assert_eq!(set["key"], key);
    assert_eq!(api.ok(Method::GET, "/kms", None).await["key"], key);
    let stored: String = api.db.read(|c| c.query_row("SELECT dek FROM projects WHERE project_id = ?", [api.project.as_str()], |r| r.get(0))).unwrap();
    assert!(stored.starts_with("kms:v1:"), "{stored}");
    assert_eq!(api.values("production", "/").await["values"]["STRIPE_KEY"], "sk_live_123", "the values didn't change");
    // roll secrets' own link kinds open the key through the customer's too (a CA's, here).
    api.secrets.set_public_api("https://roll.example".into());
    let ca = api.ok(Method::POST, "/cas", Some(json!({ "name": "internal", "domains": ["svc.internal"] }))).await;
    let link = json!({ "kind": "pki", "ref": ca["id"], "field": "certificate", "mode": "dynamic", "options": { "names": ["api.svc.internal"], "ttl_minutes": 60 } });
    api.ok(Method::PUT, "/environments/production/secrets/TLS_CERT", Some(json!({ "link": link }))).await;
    let cert = api.values("production", "/").await["values"]["TLS_CERT"].clone();
    assert!(cert.as_str().is_some_and(|c| c.starts_with("-----BEGIN CERTIFICATE-----")), "{cert}");
    let node = |keyring: Keyring| {
        let opts = Options { keyring: Ok(keyring), key_source: roll_secrets::KeySource::Env, links: Default::default(), syncs: Default::default(), open: true, trust_forwarded_for: false, web_url: None, deliver: None, grants: None, people: None, kms: Some(api.kms.clone()), sts: None, outside: None, directory: None, github: None, clouds: None, workloads: None, gateways: None };
        Arc::new(Secrets::new(api.db.clone(), api.ws.clone(), opts).unwrap())
    };
    // The deployment key rotates: health counts the wrapped project as not resealed until the
    // sweeper reseals it inside its wrap (the previous key can't be dropped before).
    let rotated = node(Keyring::new(&"n".repeat(32), Some(&"k".repeat(32))).unwrap());
    assert_eq!(rotated.health().unwrap()["resealed"], json!(0));
    rotated.sweep().unwrap();
    assert_eq!(rotated.health().unwrap()["resealed"], "all");
    let fresh = node(Keyring::new(&"n".repeat(32), None).unwrap());
    let values = |s: Arc<Secrets>| {
        let req = Request::get(format!("/v1/secrets/projects/{}/environments/production/values", api.project)).body(Body::empty()).unwrap();
        async move {
            let res = roll_secrets::router(s).oneshot(req).await.unwrap();
            (res.status(), String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap())
        }
    };
    assert!(values(fresh.clone()).await.1.contains("sk_live_123"), "opens with the new deployment key alone");
    // The customer revokes it: a node without the key in memory is refused (423, not an outage).
    *api.kms.revoked.lock().unwrap() = true;
    let (status, body) = values(node(Keyring::new(&"n".repeat(32), None).unwrap())).await;
    assert_eq!(status, StatusCode::LOCKED);
    assert!(body.contains("kms_denied") && !body.contains("sk_live"), "{body}");
    // Nor can the wrapping be changed then, even by a node that has the key in memory.
    assert_eq!(roll_secrets::router(fresh).oneshot(Request::delete(format!("/v1/secrets/projects/{}/kms", api.project)).body(Body::empty()).unwrap()).await.unwrap().status(), StatusCode::LOCKED);
    // Back with roll alone: no KMS needed any more.
    *api.kms.revoked.lock().unwrap() = false;
    let opts = Options { keyring: Ok(Keyring::new(&"n".repeat(32), None).unwrap()), key_source: roll_secrets::KeySource::Env, links: Default::default(), syncs: Default::default(), open: true, trust_forwarded_for: false, web_url: None, deliver: None, grants: None, people: None, kms: Some(api.kms.clone()), sts: None, outside: None, directory: None, github: None, clouds: None, workloads: None, gateways: None };
    let after = Arc::new(Secrets::new(api.db.clone(), api.ws.clone(), opts).unwrap());
    assert!(roll_secrets::router(after.clone()).oneshot(Request::delete(format!("/v1/secrets/projects/{}/kms", api.project)).body(Body::empty()).unwrap()).await.unwrap().status().is_success());
    *api.kms.revoked.lock().unwrap() = true;
    assert!(values(after).await.1.contains("sk_live_123"));
}

#[tokio::test]
async fn an_aws_role_gives_each_lease_a_short_lived_session() {
    let api = Api::new().await;
    let role = json!({ "name": "deploy", "role_arn": "arn:aws:iam::123456789012:role/roll" });
    // A probe STS fails to answer (throttled) proves nothing: nothing is registered.
    *api.sts.throttled.lock().unwrap() = true;
    let (s, e) = api.call(Method::POST, "/aws-roles", Some(role.clone())).await;
    assert!(s == StatusCode::BAD_REQUEST && e["error"].as_str().unwrap().contains("couldn't check"), "{e}");
    *api.sts.throttled.lock().unwrap() = false;
    // A role whose trust policy takes any ExternalId (another customer's that trusts roll, say)
    // is refused: it must pin this project's.
    let (s, e) = api.call(Method::POST, "/aws-roles", Some(role.clone())).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    assert!(e["error"].as_str().unwrap().contains(&format!("sts:ExternalId = roll-secrets:{}", api.project)), "{e}");
    *api.sts.pinned.lock().unwrap() = Some(format!("roll-secrets:{}", api.project));
    let made = api.ok(Method::POST, "/aws-roles", Some(role)).await;
    let id = made["id"].as_str().unwrap().to_string();
    // Its three keys share one session per resolve; each resolve gets a session of its own.
    for (key, field) in [("AWS_ACCESS_KEY_ID", "access_key_id"), ("AWS_SECRET_ACCESS_KEY", "secret_access_key"), ("AWS_SESSION_TOKEN", "session_token")] {
        api.ok(Method::PUT, &format!("/environments/production/secrets/{key}"), Some(json!({ "link": { "kind": "aws", "ref": id, "field": field, "mode": "dynamic", "options": { "ttl_minutes": 15 } } }))).await;
    }
    let before = *api.sts.sessions.lock().unwrap();
    let a = api.ok(Method::POST, "/environments/production/resolve", Some(json!({ "consumer": "app:web" }))).await;
    let b = api.ok(Method::POST, "/environments/production/resolve", Some(json!({ "consumer": "app:web" }))).await;
    assert_eq!(*api.sts.sessions.lock().unwrap() - before, 2, "one session per resolve, for all three keys");
    let n = a["values"]["AWS_ACCESS_KEY_ID"].as_str().unwrap().trim_start_matches("ASIA").to_string();
    assert_eq!((a["values"]["AWS_SECRET_ACCESS_KEY"].as_str(), a["values"]["AWS_SESSION_TOKEN"].as_str()), (Some(format!("secret-{n}").as_str()), Some(format!("token-{n}").as_str())));
    assert_ne!(a["values"]["AWS_SESSION_TOKEN"], b["values"]["AWS_SESSION_TOKEN"]);
    assert_eq!(a["dynamic"].as_array().map(Vec::len), Some(3));
    // An AWS session can't be extended: resolve again for a new one.
    let lease = a["leases"][0].as_str().unwrap();
    let (s, _) = api.call(Method::POST, &format!("/leases/{lease}/renew"), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // A role keys still link to stays.
    assert_eq!(api.call(Method::DELETE, &format!("/aws-roles/{id}"), None).await.0, StatusCode::CONFLICT);

    // A role that trusts roll's issuer instead: no AWS credential of roll's, the same decoy check
    // (its trust must pin this project's subject), and sessions through AssumeRoleWithWebIdentity.
    let oidc = json!({ "name": "deploy-oidc", "role_arn": "arn:aws:iam::123456789012:role/roll-oidc", "trust": "oidc" });
    let (s, e) = api.call(Method::POST, "/aws-roles", Some(oidc.clone())).await;
    assert!(s == StatusCode::BAD_REQUEST && e["error"].as_str().unwrap().contains(&format!("sub = roll-secrets:{}", api.project)), "{e}");
    *api.sts.web_pinned.lock().unwrap() = Some(format!("roll-secrets:{}", api.project));
    let made = api.ok(Method::POST, "/aws-roles", Some(oidc)).await;
    assert_eq!(made["trust"], "oidc");
    api.ok(Method::PUT, "/environments/staging/secrets/AWS_SESSION_TOKEN", Some(json!({ "link": { "kind": "aws", "ref": made["id"], "field": "session_token", "mode": "dynamic" } }))).await;
    let before = *api.sts.sessions.lock().unwrap();
    let v = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "app:web" }))).await;
    assert!(v["values"]["AWS_SESSION_TOKEN"].as_str().unwrap().starts_with("token-") && *api.sts.sessions.lock().unwrap() == before + 1, "{v}");
    // Edited in place: a new ARN is proved as a new role is, and nothing changes when it isn't.
    let role_path = format!("/aws-roles/{}", made["id"].as_str().unwrap());
    let (s, _) = api.call(Method::PATCH, &role_path, Some(json!({ "role_arn": "arn:aws:iam::123456789012:role/gone" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "an ARN roll can't assume");
    let renamed = api.ok(Method::PATCH, &role_path, Some(json!({ "name": "deploy-web" }))).await;
    assert_eq!((renamed["name"].as_str(), renamed["role_arn"].as_str()), (Some("deploy-web"), Some("arn:aws:iam::123456789012:role/roll-oidc")));
}

#[tokio::test]
async fn an_iam_users_access_keys_take_turns() {
    let api = Api::new().await;
    *api.sts.pinned.lock().unwrap() = Some(format!("roll-secrets:{}", api.project));
    let role = api.ok(Method::POST, "/aws-roles", Some(json!({ "name": "keys", "role_arn": "arn:aws:iam::123456789012:role/roll" }))).await;
    let (s, _) = api.call(Method::POST, "/aws-users", Some(json!({ "name": "ci", "role": role["id"], "user": "nobody" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a user the role can't see is refused");
    let user = api.ok(Method::POST, "/aws-users", Some(json!({ "name": "ci", "role": role["id"], "user": "legacy-ci" }))).await;
    // Its rotation moves to another ExternalId role, proved by listing its keys there; a role that
    // trusts roll's issuer can't take IAM users, and one that has them can't switch to it.
    let other = api.ok(Method::POST, "/aws-roles", Some(json!({ "name": "keys-2", "role_arn": "arn:aws:iam::123456789012:role/roll" }))).await;
    let moved = api.ok(Method::PATCH, &format!("/aws-users/{}", user["id"].as_str().unwrap()), Some(json!({ "role": other["id"] }))).await;
    assert_eq!(moved["role"], other["id"]);
    let (s, _) = api.call(Method::PATCH, &format!("/aws-roles/{}", other["id"].as_str().unwrap()), Some(json!({ "trust": "oidc" }))).await;
    assert_eq!(s, StatusCode::CONFLICT, "its IAM user needs ExternalId trust");
    api.ok(Method::PATCH, &format!("/aws-users/{}", user["id"].as_str().unwrap()), Some(json!({ "role": role["id"] }))).await;
    let link = |field: &str| json!({ "kind": "aws_iam_user", "ref": user["id"], "field": field, "mode": "managed" });
    api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": [{ "op": "set", "key": "AWS_ACCESS_KEY_ID", "link": link("access_key_id") }, { "op": "set", "key": "AWS_SECRET_ACCESS_KEY", "link": link("secret_access_key") }] }))).await;
    let key = || async { api.values("production", "/").await["values"]["AWS_ACCESS_KEY_ID"].as_str().unwrap().to_string() };
    let keys = || api.sts.keys.lock().unwrap().0.clone();
    assert_eq!(key().await, "AKIA1");
    // A rotation makes the second key; the first works through its grace.
    api.ok(Method::POST, "/environments/production/secrets/AWS_ACCESS_KEY_ID/rotate", None).await;
    assert_eq!((key().await, keys()), ("AKIA2".to_string(), vec!["AKIA1".to_string(), "AKIA2".to_string()]));
    // Both slots are taken: another rotation is refused, and nothing is deleted to make room.
    let (s, e) = api.call(Method::POST, "/environments/production/secrets/AWS_ACCESS_KEY_ID/rotate", None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    assert_eq!((key().await, keys().len()), ("AKIA2".to_string(), 2));
    // The grace ends: the old key goes, and the slot is free again.
    let p = api.project.clone();
    api.db.write(move |c| c.execute("UPDATE leases SET expires_at = 0 WHERE project_id = ?", [p.as_str()])).unwrap();
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap();
    assert_eq!(keys(), ["AKIA2"]);
    api.ok(Method::POST, "/environments/production/secrets/AWS_ACCESS_KEY_ID/rotate", None).await;
    assert_eq!(key().await, "AKIA3");
    // What's in use stays.
    assert_eq!(api.call(Method::DELETE, &format!("/aws-roles/{}", role["id"].as_str().unwrap()), None).await.0, StatusCode::CONFLICT);
    assert_eq!(api.call(Method::DELETE, &format!("/aws-users/{}", user["id"].as_str().unwrap()), None).await.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_workload_logs_in_with_its_oidc_token_for_a_short_lived_scoped_token() {
    let api = Api::new().await;
    api.set("staging", "API_KEY", "staging-key").await;
    api.set("production", "API_KEY", "prod-key").await;
    let issuer = Issuer::new();
    *api.sent.1.lock().unwrap() = Some(issuer.jwks());
    let identity = |subject: &str| json!({ "name": "deploy", "issuer": ISSUER, "audience": "roll", "subject": subject, "claims": { "ref": "refs/heads/*" }, "environments": ["staging"], "actions": ["read", "reveal"], "ttl_minutes": 10 });
    // GitHub signs for every repo: an identity of its names one.
    let (s, _) = api.call(Method::POST, "/identities", Some(identity("repo:*"))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let i = api.ok(Method::POST, "/identities", Some(identity("repo:acme/web:*"))).await;
    let id = i["id"].as_str().unwrap().to_string();

    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let claims = |sub: &str, git_ref: &str, exp: i64| json!({ "iss": ISSUER, "aud": "roll", "sub": sub, "ref": git_ref, "exp": exp, "iat": now });
    let login = |token: String| api.call(Method::POST, "/v1/secrets/login", Some(json!({ "identity": id, "token": token })));
    let (s, l) = login(issuer.sign(claims("repo:acme/web:ref:refs/heads/main", "refs/heads/main", now + 300))).await;
    assert_eq!(s, StatusCode::OK, "{l}");
    let token = l["token"].as_str().unwrap().to_string();
    // At most ten minutes from when it was issued (bounded by the answer's time, not `now`: a
    // loaded machine can take more than a second to log in).
    let answered = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    assert!(token.starts_with("rlt_") && l["expires_at"].as_i64().unwrap() <= answered + 600_000, "{l}");
    let (s, v) = api.req(Method::GET, "/environments/staging/values", None, Some(&token)).await;
    assert_eq!((s, v["values"]["API_KEY"].as_str()), (StatusCode::OK, Some("staging-key")));
    let (s, _) = api.req(Method::GET, "/environments/production/values", None, Some(&token)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "only the identity's environments");

    // Another repo, another claim, an old token and a forged one log in as nobody.
    for (bad, want) in [
        (issuer.sign(claims("repo:acme/website:ref:refs/heads/main", "refs/heads/main", now + 300)), StatusCode::FORBIDDEN),
        (issuer.sign(claims("repo:acme/web:pull_request", "refs/pull/1/merge", now + 300)), StatusCode::FORBIDDEN),
        (issuer.sign(claims("repo:acme/web:ref:refs/heads/main", "refs/heads/main", now - 600)), StatusCode::UNAUTHORIZED),
        (Issuer::new().sign(claims("repo:acme/web:ref:refs/heads/main", "refs/heads/main", now + 300)), StatusCode::UNAUTHORIZED),
    ] {
        assert_eq!(login(bad).await.0, want);
    }
    let audit = api.ok(Method::GET, &format!("/audit?actor=identity:{id}"), None).await;
    assert_eq!(audit.as_array().unwrap().iter().map(|a| a["action"].as_str().unwrap()).collect::<Vec<_>>(), ["secrets.export", "identity.login"]);
    let listed = api.ok(Method::GET, "/identities", None).await;
    assert_eq!(listed[0]["last_subject"], "repo:acme/web:ref:refs/heads/main");

    // A cluster's own issuer isn't reachable: an identity pins its keys, and roll fetches nothing.
    *api.sent.1.lock().unwrap() = None;
    let cluster = Issuer::new();
    let k8s = "https://kubernetes.default.svc.cluster.local";
    let pods = |jwks: Value| json!({ "name": "pods", "issuer": k8s, "audience": "roll", "subject": "system:serviceaccount:shop:web", "environments": ["staging"], "actions": ["read", "reveal"], "jwks": jwks.to_string() });
    assert_eq!(api.call(Method::POST, "/identities", Some(pods(json!({ "keys": [] })))).await.0, StatusCode::BAD_REQUEST, "no usable key");
    let p = api.ok(Method::POST, "/identities", Some(pods(cluster.jwks()))).await;
    assert_eq!(p["pinned_keys"], 1);
    let pod = |signer: &Issuer| json!({ "identity": p["id"], "token": signer.sign(json!({ "iss": k8s, "aud": "roll", "sub": "system:serviceaccount:shop:web", "exp": now + 300, "iat": now })) });
    let (s, l) = api.call(Method::POST, "/v1/secrets/login", Some(pod(&cluster))).await;
    assert_eq!(s, StatusCode::OK, "{l}");
    assert_eq!(api.call(Method::POST, "/v1/secrets/login", Some(pod(&issuer))).await.0, StatusCode::UNAUTHORIZED, "only its own keys");
    // The cluster rotates its signing key: the identity's keys are replaced, its id kept.
    let rotated = Issuer::new();
    let (_, before) = api.call(Method::POST, "/v1/secrets/login", Some(pod(&cluster))).await;
    let k = api.ok(Method::PUT, &format!("/identities/{}/jwks", p["id"].as_str().unwrap()), Some(json!({ "jwks": rotated.jwks().to_string() }))).await;
    assert_eq!((k["id"].clone(), k["pinned_keys"].clone()), (p["id"].clone(), json!(1)));
    assert_eq!(api.call(Method::POST, "/v1/secrets/login", Some(pod(&rotated))).await.0, StatusCode::OK);
    assert_eq!(api.call(Method::POST, "/v1/secrets/login", Some(pod(&cluster))).await.0, StatusCode::UNAUTHORIZED, "the old key no longer");
    // What it gave out under the old key ends with it (a rotation is often for a leaked key).
    let (s, _) = api.req(Method::GET, "/environments/staging/values", None, before["token"].as_str()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // Azure Pipelines: one issuer per organization, so an identity names one pipeline (or service
    // connection), and the token's audience is Azure's own.
    let ado = "https://vstoken.dev.azure.com/00000000-0000-0000-0000-00000000a0b1";
    let azure = Issuer::new();
    let pipeline = |subject: &str| json!({ "name": "pipeline", "issuer": ado, "audience": "api://AzureADTokenExchange", "subject": subject, "environments": ["staging"], "actions": ["read", "reveal"], "jwks": azure.jwks().to_string() });
    assert_eq!(api.call(Method::POST, "/identities", Some(pipeline("p://acme/shop/*"))).await.0, StatusCode::BAD_REQUEST, "every pipeline of the organization");
    let a = api.ok(Method::POST, "/identities", Some(pipeline("p://acme/shop/deploy"))).await;
    let run = |sub: &str| json!({ "identity": a["id"], "token": azure.sign(json!({ "iss": ado, "aud": "api://AzureADTokenExchange", "sub": sub, "exp": now + 300, "iat": now })) });
    assert_eq!(api.call(Method::POST, "/v1/secrets/login", Some(run("p://acme/shop/deploy"))).await.0, StatusCode::OK);
    assert_eq!(api.call(Method::POST, "/v1/secrets/login", Some(run("p://acme/shop/deploy-preview"))).await.0, StatusCode::FORBIDDEN);

    // Deleting the identity ends what it gave out.
    api.ok(Method::DELETE, &format!("/identities/{id}"), None).await;
    let (s, _) = api.req(Method::GET, "/environments/staging/values", None, Some(&token)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_identity_matches_a_nested_role_claim() {
    let api = Api::new().await;
    api.set("staging", "API_KEY", "staging-key").await;
    // Keycloak puts a user's realm roles in an array under `realm_access`.
    let keycloak = Issuer::new();
    let realm = "https://sso.example.com/realms/acme";
    let identity = |claims: Value| json!({ "name": "ci", "issuer": realm, "audience": "roll", "subject": "service-account-ci", "claims": claims, "environments": ["staging"], "actions": ["read", "reveal"], "jwks": keycloak.jwks().to_string() });
    assert_eq!(api.call(Method::POST, "/identities", Some(identity(json!({ "/realm_access//roles": "deployer" })))).await.0, StatusCode::BAD_REQUEST);
    let i = api.ok(Method::POST, "/identities", Some(identity(json!({ "/realm_access/roles": "deployer" })))).await;
    assert_eq!(i["claims"], json!({ "/realm_access/roles": "deployer" }));
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let login = |roles: Value| {
        let token = keycloak.sign(json!({ "iss": realm, "aud": "roll", "sub": "service-account-ci", "realm_access": { "roles": roles }, "exp": now + 300, "iat": now }));
        api.call(Method::POST, "/v1/secrets/login", Some(json!({ "identity": i["id"], "token": token })))
    };
    let (s, l) = login(json!(["offline_access", "deployer"])).await;
    assert_eq!(s, StatusCode::OK, "{l}");
    let (s, l) = login(json!(["offline_access"])).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{l}");
}

/// A signed GetCallerIdentity as roll's clients send one (STS checks the signature; roll checks the
/// rest), dated `age` seconds ago, naming `server` in X-Roll-Server-ID. Each has its own signature.
fn signed_caller_identity(url: &str, body: &str, server: Option<&str>, age: i64) -> Value {
    static SIGNED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let signature = format!("{:064x}", SIGNED.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let t = time::OffsetDateTime::now_utc() - time::Duration::seconds(age);
    let date = format!("{:04}{:02}{:02}T{:02}{:02}{:02}Z", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute(), t.second());
    let mut headers = json!({
        "Content-Type": "application/x-www-form-urlencoded; charset=utf-8",
        "X-Amz-Date": date,
        "X-Amz-Security-Token": "FwoGZXIvYXdzEXAMPLETOKEN",
        "Authorization": format!("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/{}/us-east-1/sts/aws4_request, SignedHeaders=content-type;host;x-amz-date;x-amz-security-token;x-roll-server-id, Signature={signature}", &date[..8]),
    });
    if let Some(s) = server {
        headers["X-Roll-Server-ID"] = json!(s);
    }
    json!({ "method": "POST", "url": url, "headers": headers, "body": body })
}

#[tokio::test]
async fn a_machine_on_aws_logs_in_with_its_iam_role_through_sts() {
    let api = Api::new().await;
    api.set("staging", "API_KEY", "staging-key").await;
    api.set("production", "API_KEY", "prod-key").await;
    let identity = |arn: &str| json!({ "name": "web", "kind": "aws", "arn": arn, "environments": ["staging"], "actions": ["read", "reveal"] });
    // One role or user in one account: a wildcard anywhere but a session name is refused.
    for open in ["arn:aws:iam::*:role/app", "arn:aws:iam::123456789012:role/*", "arn:aws:iam::123456789012:root"] {
        assert_eq!(api.call(Method::POST, "/identities", Some(identity(open))).await.0, StatusCode::BAD_REQUEST, "{open}");
    }
    // Unset, the server ID is derived from the deployment key: never a host and port two
    // deployments could share.
    let derived = api.ok(Method::POST, "/identities", Some(json!({ "name": "other", "kind": "aws", "arn": "arn:aws:iam::123456789012:role/other", "actions": ["read"] }))).await;
    assert!(derived["server_id"].as_str().is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())), "{derived}");
    api.secrets.set_aws_server_id("https://roll.test/".into());
    let i = api.ok(Method::POST, "/identities", Some(identity("arn:aws:iam::123456789012:role/app"))).await;
    assert_eq!((i["kind"].as_str(), i["server_id"].as_str()), (Some("aws"), Some("https://roll.test")));
    let id = i["id"].as_str().unwrap().to_string();
    let sts = "https://sts.us-east-1.amazonaws.com/";
    const BODY: &str = "Action=GetCallerIdentity&Version=2011-06-15";
    let login = |aws: Value| api.call(Method::POST, "/v1/secrets/login", Some(json!({ "identity": id, "aws": aws })));
    let answers = |arn: Option<&str>| api.sent.8.lock().unwrap().caller = arn.map(String::from);
    let calls = || api.sent.8.lock().unwrap().calls.len();

    // An instance profile's session is its role's.
    answers(Some("arn:aws:sts::123456789012:assumed-role/app/i-0abcd1234efgh5678"));
    let signed = signed_caller_identity(sts, BODY, Some("https://roll.test"), 0);
    let (s, l) = login(signed.clone()).await;
    assert_eq!(s, StatusCode::OK, "{l}");
    // A signed request logs in once: replayed while its date is still in the window, it's refused.
    let (s, e) = login(signed.clone()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{e}");
    // Nor with a component repeated in front (STS gets the whole header) or a signature that isn't
    // hex: there's one Signature, the one STS checks, so a replay never gets a new key.
    let auth = signed["headers"]["Authorization"].as_str().unwrap().to_string();
    let rest = auth.strip_prefix("AWS4-HMAC-SHA256 ").unwrap();
    for changed in [format!("AWS4-HMAC-SHA256 Signature={}, {rest}", "0".repeat(64)), format!("AWS4-HMAC-SHA256 signature=junk, {rest}"), auth.replace("Signature=", "Signature=X")] {
        let mut replay = signed.clone();
        replay["headers"]["Authorization"] = json!(changed);
        let before = calls();
        let (s, e) = login(replay).await;
        assert!(matches!(s, StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED), "{changed}: {s} {e}");
        assert_eq!(calls(), before, "{changed} never reaches STS");
    }
    let token = l["token"].as_str().unwrap();
    let (s, v) = api.req(Method::GET, "/environments/staging/values", None, Some(token)).await;
    assert_eq!((s, v["values"]["API_KEY"].as_str()), (StatusCode::OK, Some("staging-key")));
    assert_eq!(api.req(Method::GET, "/environments/production/values", None, Some(token)).await.0, StatusCode::FORBIDDEN);
    // STS got the signed request as signed, at the URL roll rebuilt (its Host from that URL).
    let (host, headers, body) = api.sent.8.lock().unwrap().calls[0].clone();
    assert_eq!((host.as_str(), body.as_str()), ("sts.us-east-1.amazonaws.com", BODY));
    let names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(names, ["authorization", "content-type", "x-amz-date", "x-amz-security-token", "x-roll-server-id"]);
    let listed = api.ok(Method::GET, "/identities", None).await;
    let web = listed.as_array().unwrap().iter().find(|x| x["id"] == id.as_str()).unwrap();
    assert_eq!(web["last_subject"], "arn:aws:sts::123456789012:assumed-role/app/i-0abcd1234efgh5678");
    let audit = api.ok(Method::GET, &format!("/audit?actor=identity:{id}"), None).await;
    assert_eq!(audit[1]["action"], "identity.login");

    // Another account's role of the same name, another role, a user: STS says who, roll refuses.
    for other in ["arn:aws:sts::210987654321:assumed-role/app/i-0abcd1234efgh5678", "arn:aws:sts::123456789012:assumed-role/admin/i-0abcd1234efgh5678", "arn:aws:iam::123456789012:user/app"] {
        answers(Some(other));
        assert_eq!(login(signed_caller_identity(sts, BODY, Some("https://roll.test"), 0)).await.0, StatusCode::FORBIDDEN, "{other}");
    }
    // A signature STS doesn't accept.
    answers(None);
    assert_eq!(login(signed_caller_identity(sts, BODY, Some("https://roll.test"), 0)).await.0, StatusCode::UNAUTHORIZED);

    // Refused before STS is contacted: a request for another server or none, an old one, another
    // URL (only STS's, never one the client picks), another action.
    answers(Some("arn:aws:sts::123456789012:assumed-role/app/i-0abcd1234efgh5678"));
    let before = calls();
    for (aws, want) in [
        (signed_caller_identity(sts, BODY, None, 0), StatusCode::UNAUTHORIZED),
        (signed_caller_identity(sts, BODY, Some("https://vault.example.com"), 0), StatusCode::UNAUTHORIZED),
        (signed_caller_identity(sts, BODY, Some("https://roll.test"), 900), StatusCode::UNAUTHORIZED),
        (signed_caller_identity("https://sts.amazonaws.com.attacker.example/", BODY, Some("https://roll.test"), 0), StatusCode::BAD_REQUEST),
        (signed_caller_identity("http://169.254.169.254/latest/meta-data/", BODY, Some("https://roll.test"), 0), StatusCode::BAD_REQUEST),
        (signed_caller_identity("https://sts.us-east-1.amazonaws.com/?Action=AssumeRole", BODY, Some("https://roll.test"), 0), StatusCode::BAD_REQUEST),
        (signed_caller_identity(sts, "Action=AssumeRole&Version=2011-06-15", Some("https://roll.test"), 0), StatusCode::BAD_REQUEST),
    ] {
        let (s, e) = login(aws).await;
        assert_eq!(s, want, "{e}");
    }
    assert_eq!(calls(), before, "nothing reached STS");
    // An OIDC token for an AWS identity.
    assert_eq!(api.call(Method::POST, "/v1/secrets/login", Some(json!({ "identity": id, "token": "a.b.c" }))).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_folder_shared_by_another_project_reads_as_its_own_until_the_share_ends() {
    let api = Api::new().await;
    let (_, other) = api.call(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "platform" }))).await;
    let platform = other["id"].as_str().unwrap().to_string();
    let theirs = |p: &str| format!("/v1/secrets/projects/{platform}{p}");
    api.ok(Method::PUT, &theirs("/environments/production/secrets/SENTRY_DSN?path=/shared"), Some(json!({ "value": "https://key@sentry.example/1" }))).await;
    api.ok(Method::PUT, &theirs("/environments/production/secrets/NEXT?path=/shared"), Some(json!({ "value": "${SENTRY_DSN}" }))).await;
    let import = json!([{ "project": platform, "environment": "production", "path": "/shared" }]);
    // Not shared yet: no import.
    let (s, _) = api.call(Method::PUT, "/environments/staging/imports?path=/", Some(import.clone())).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    api.ok(Method::PUT, &theirs("/environments/production/exports?path=/shared"), Some(json!([{ "project": api.project }]))).await;
    assert_eq!(api.ok(Method::GET, "/shared", None).await, import, "offered here, ready to import");
    api.ok(Method::PUT, "/environments/staging/imports?path=/", Some(import)).await;
    // Its stored values, opened with its own key; a reference would resolve against the wrong
    // project, so it doesn't cross.
    let t = api.token(json!({ "name": "ci", "environments": ["staging"], "actions": ["read", "reveal"] }));
    let (s, v) = api.req(Method::GET, "/environments/staging/values", None, Some(t.as_str())).await;
    assert_eq!((s, v["values"].clone()), (StatusCode::OK, json!({ "SENTRY_DSN": "https://key@sentry.example/1" })));
    // The same environment, folder and key in this project stays this project's own.
    api.ok(Method::PUT, "/environments/production/secrets/SENTRY_DSN?path=/shared", Some(json!({ "value": "ours" }))).await;
    api.set("staging", "REF", "${production.shared.SENTRY_DSN}").await;
    let v = api.values("staging", "/").await;
    assert_eq!((v["values"]["REF"].as_str(), v["values"]["SENTRY_DSN"].as_str()), (Some("ours"), Some("https://key@sentry.example/1")));
    // A shared value pasted into this project's code is a leak too, named by its project.
    let leaks = api.secrets.leaks(&api.project, &["https://key@sentry.example/1".to_string()]).unwrap();
    assert_eq!(leaks.iter().map(|l| l.environment.as_str()).collect::<Vec<_>>(), ["platform/production"]);
    // A protected environment never reaches an untrusted consumer, shared or not.
    api.ok(Method::PUT, &theirs("/environments/production/policy"), Some(json!({ "protected": true }))).await;
    let r = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "preview:pr-1", "untrusted": true }))).await;
    assert!(r["values"].get("SENTRY_DSN").is_none(), "{r}");
    // The share ends: so does the import, at once.
    api.ok(Method::PUT, &theirs("/environments/production/exports?path=/shared"), Some(json!([]))).await;
    assert_eq!(api.values("staging", "/").await["values"]["SENTRY_DSN"], json!(null));
    let audit = api.ok(Method::GET, &theirs("/audit"), None).await;
    assert!(audit.as_array().unwrap().iter().any(|a| a["action"] == "exports.set"));
}

/// docs/secrets.md's HTTP table is the contract (and roll-access's catalogue is checked against
/// it): every route the router serves is in it.
#[tokio::test]
async fn every_route_is_in_the_http_contract() {
    let api = Api::new().await;
    let text = format!("{:?}", roll_secrets::router(api.secrets.clone()));
    let mut routes = vec![];
    let mut rest = text.split("fallback_router:").next().unwrap_or_default();
    while let Some(i) = rest.find("RouteId(") {
        rest = &rest[i + "RouteId(".len()..];
        let Some(after) = rest.trim_start_matches(|c: char| c.is_ascii_digit()).strip_prefix("): \"") else { continue };
        let Some(end) = after.find('"') else { break };
        if after.starts_with("/v1/secrets") {
            routes.push(after[..end].to_string());
        }
    }
    // `{anything}` → `{}`; the project prefix and a query dropped, as the table writes them.
    let norm = |p: &str| -> String {
        let p = p.split('?').next().unwrap_or(p).trim_end_matches('/');
        let p = p.strip_prefix("/v1/secrets/projects/{project}").map(|r| if r.is_empty() { "/".to_string() } else { r.to_string() }).unwrap_or_else(|| p.to_string());
        let mut out = String::new();
        let mut depth = 0;
        for c in p.chars() {
            match c {
                '{' => {
                    depth += 1;
                    out.push_str("{}");
                }
                '}' => depth -= 1,
                c if depth == 0 => out.push(c),
                _ => {}
            }
        }
        out
    };
    let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/secrets.md")).unwrap();
    let documented: std::collections::HashSet<String> = doc
        .lines()
        .filter(|l| l.starts_with("| `"))
        .filter_map(|l| l[3..].split('`').next())
        // A method alone (`GET`) is the project itself.
        .filter_map(|cell| match cell.split_once(' ') {
            Some((_, path)) => Some(norm(path)),
            None => ["GET", "PATCH", "PUT", "POST", "DELETE"].contains(&cell).then(|| "/".to_string()),
        })
        .collect();
    let missing: Vec<String> = routes.iter().map(|r| norm(r)).filter(|r| !documented.contains(r)).collect();
    assert!(routes.len() > 40, "the routes were read ({})", routes.len());
    assert!(missing.is_empty(), "routes missing from docs/secrets.md's HTTP table: {missing:?}");
}

#[tokio::test]
async fn scheduled_backups_land_sealed_in_the_projects_bucket_and_keep_the_newest() {
    let api = Api::new().await;
    api.ok(Method::PUT, "/environments/production/secrets/STRIPE", Some(json!({ "value": "sk_live_nightly" }))).await;
    let secret = [9u8; 32];
    let schedule = json!({ "bucket": "assets", "recipient": roll_secrets::crypto::public_key(&secret), "every_hours": 24, "keep": 2 });
    let (s, _) = api.call(Method::PUT, "/backup/schedule", Some(schedule.clone())).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "only a bucket of the project's own");
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{}/resources/bucket/assets", api.project), Some(json!({ "x": 0, "y": 0 }))).await;
    api.ok(Method::PUT, "/backup/schedule", Some(schedule)).await;
    // Two older ones there already: the run leaves the newest two, and nothing roll didn't write.
    let prefix = format!("roll-secrets/backups/{}/", api.project);
    for old in ["20200101T000000.000Z.json", "20200102T000000.000Z.json", "notes.txt"] {
        api.sent.4.lock().unwrap().insert(("assets".into(), format!("{prefix}{old}")), json!({}));
    }
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap();
    let mut kept: Vec<String> = api.sent.4.lock().unwrap().keys().map(|(_, k)| k.clone()).collect();
    kept.sort();
    assert_eq!(kept.pop().as_deref(), Some(format!("{prefix}notes.txt").as_str()));
    assert!(kept.len() == 2 && kept[0].ends_with("20200102T000000.000Z.json"), "{kept:?}");
    let schedule = api.ok(Method::GET, "/backup/schedule", None).await;
    assert!(schedule["last_error"].is_null() && schedule["last_key"] == kept[1], "{schedule}");
    // Sealed: only the recipient's secret key opens it, and it holds the project.
    let backup = api.sent.4.lock().unwrap()[&("assets".to_string(), kept[1].clone())].clone();
    assert!(!backup.to_string().contains("sk_live_nightly"));
    let sealed: roll_secrets::crypto::Sealed = serde_json::from_value(backup["sealed"].clone()).unwrap();
    let plain = roll_secrets::crypto::open_sealed(&secret, backup["context"].as_str().unwrap(), &sealed).unwrap();
    assert!(String::from_utf8_lossy(&plain).contains("sk_live_nightly"));
    // Not due again for a day.
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap();
    assert_eq!(api.sent.4.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn a_project_backs_up_sealed_to_a_key_and_restores_into_another() {
    let api = Api::new().await;
    api.set("staging", "API", "https://staging").await;
    api.ok(Method::PUT, "/environments/production/secrets/STRIPE?path=/pay", Some(json!({ "value": "sk_live_backup", "comment": "payments" }))).await;
    api.ok(Method::PUT, "/environments/development/imports?path=/", Some(json!([{ "environment": "staging", "path": "/" }]))).await;
    api.ok(Method::PUT, "/required", Some(json!([{ "key": "API", "type": "url" }]))).await;
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true, "approvals": 1 }))).await;

    let secret = [7u8; 32];
    let b = api.ok(Method::POST, "/backup", Some(json!({ "recipient": roll_secrets::crypto::public_key(&secret) }))).await;
    assert!(!b.to_string().contains("sk_live_backup"), "sealed, never in the clear");
    let sealed: roll_secrets::crypto::Sealed = serde_json::from_value(b["sealed"].clone()).unwrap();
    let plain = roll_secrets::crypto::open_sealed(&secret, b["context"].as_str().unwrap(), &sealed).unwrap();
    let archive: Value = serde_json::from_slice(&plain).unwrap();

    let (_, other) = api.call(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "restored" }))).await;
    let to = |p: &str| format!("/v1/secrets/projects/{}{p}", other["id"].as_str().unwrap());
    let r = api.ok(Method::POST, &to("/restore"), Some(archive.clone())).await;
    assert_eq!((r["secrets"].as_u64(), r["warnings"].clone()), (Some(2), json!([])));
    assert_eq!(api.ok(Method::GET, &to("/environments/production/values?path=/pay"), None).await["values"]["STRIPE"], "sk_live_backup");
    assert_eq!(api.ok(Method::GET, &to("/environments/development/values"), None).await["values"]["API"], "https://staging", "imports restored");
    assert_eq!(api.ok(Method::GET, &to("/environments/production"), None).await["protected"], true, "protection last");
    assert_eq!(api.ok(Method::GET, &to("/required"), None).await[0]["type"], "url");
    let (s, _) = api.call(Method::POST, &to("/restore"), Some(archive)).await;
    assert_eq!(s, StatusCode::CONFLICT, "only into a project with no secrets");
}

#[tokio::test]
async fn a_key_is_found_in_every_project_the_caller_may_read() {
    let api = Api::new().await;
    api.set("production", "STRIPE_KEY", "sk_live_1").await;
    api.set("staging", "STRIPE_WEBHOOK", "whsec_1").await;
    api.set("staging", "STRIPEXKEY", "not a prefix match").await;
    let (_, other) = api.call(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "billing" }))).await;
    let theirs = format!("/v1/secrets/projects/{}/environments/development/secrets/STRIPE_KEY", other["id"].as_str().unwrap());
    api.ok(Method::PUT, &theirs, Some(json!({ "value": "sk_test_2" }))).await;

    let found = api.ok(Method::GET, "/v1/secrets/search?key=STRIPE_KEY", None).await;
    let places: Vec<(&str, &str)> = found.as_array().unwrap().iter().map(|p| (p["project_name"].as_str().unwrap(), p["environment"].as_str().unwrap())).collect();
    assert_eq!(places.len(), 2, "{found}");
    assert!(places.contains(&("shop", "production")) && places.contains(&("billing", "development")));
    assert!(!found.to_string().contains("sk_"), "never a value");
    // `_` is a character, not LIKE's wildcard.
    let prefix = api.ok(Method::GET, "/v1/secrets/search?key=STRIPE_*", None).await;
    let keys: Vec<&str> = prefix.as_array().unwrap().iter().map(|p| p["key"].as_str().unwrap()).collect();
    assert!(keys.contains(&"STRIPE_WEBHOOK") && !keys.contains(&"STRIPEXKEY"), "{keys:?}");
    // A token for one project finds only there.
    let t = api.token(json!({ "name": "ro", "actions": ["read"] }));
    let (_, mine) = api.req(Method::GET, "/v1/secrets/search?key=STRIPE_KEY", None, Some(t.as_str())).await;
    assert_eq!(mine.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn reach_names_who_and_what_can_read_a_folder() {
    let api = Api::new().await;
    api.set("production", "STRIPE_KEY", "sk_live_1").await;
    let deploy = api.token(json!({ "name": "deploy", "environments": ["production"], "actions": ["read", "reveal"] }));
    api.token(json!({ "name": "staging-ci", "environments": ["staging"], "actions": ["read", "reveal"] }));
    api.ok(Method::PUT, "/environments/staging/imports?path=/", Some(json!([{ "environment": "production", "path": "/" }]))).await;
    api.req(Method::GET, "/environments/production/values", None, Some(&deploy)).await;
    // A read of staging reads production's key through the import: counted, and said so; so is
    // one of development, which imports staging (a chain).
    api.ok(Method::PUT, "/environments/development/imports?path=/", Some(json!([{ "environment": "staging", "path": "/" }]))).await;
    api.values("staging", "/").await;
    api.values("development", "/").await;
    let r = api.ok(Method::GET, "/environments/production/reach?path=/&key=STRIPE_KEY", None).await;
    let names = |k: &str| r[k].as_array().unwrap().iter().map(|h| h["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    // The production token reaches it (with what it may do), staging's doesn't.
    assert_eq!(names("tokens"), ["deploy"]);
    assert_eq!(r["tokens"][0]["actions"], json!(["read", "reveal"]));
    // Production starts protected: no untrusted reader reaches it.
    assert_eq!((r["untrusted"].as_bool(), r["imported_by"].clone()), (Some(false), json!(["staging:/", "development:/"])));
    let readers: Vec<&str> = r["readers"].as_array().unwrap().iter().map(|x| x["actor"].as_str().unwrap()).collect();
    assert!(readers.iter().any(|a| a.starts_with("token:")) && readers.contains(&"operator (through staging:/)") && readers.contains(&"operator (through development:/)"), "{readers:?}");
    // Another project it's shared with is named only as a project, never its environments.
    let (_, other) = api.call(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "platform" }))).await;
    let platform = other["id"].as_str().unwrap().to_string();
    api.ok(Method::PUT, "/environments/production/exports?path=/", Some(json!([{ "project": platform }]))).await;
    api.ok(Method::PUT, &format!("/v1/secrets/projects/{platform}/environments/staging/imports?path=/"), Some(json!([{ "project": api.project, "environment": "production", "path": "/" }]))).await;
    let r = api.ok(Method::GET, "/environments/production/reach?path=/", None).await;
    let mut by: Vec<&str> = r["imported_by"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
    by.sort();
    assert_eq!(by, ["development:/", &format!("project {platform}"), "staging:/"], "{r}");
    // It lists the project's credentials: a project admin's.
    let (s, _) = api.req(Method::GET, "/environments/production/reach", None, Some(&deploy)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_gateway_has_one_token_that_opens_only_its_link_and_reaches_its_projects_databases() {
    let api = Api::new().await;
    let reader = api.token(json!({ "actions": ["read"] }));
    let (s, _) = api.req(Method::POST, "/gateways", Some(json!({ "name": "vpc" })), Some(&reader)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "making a gateway is admin");
    // A token doesn't make a gateway's token (a project-wide rule doesn't cover it), but may make
    // one that logs in through an identity.
    let admin = api.token(json!({ "actions": ["read", "admin"] }));
    let (s, _) = api.req(Method::POST, "/gateways", Some(json!({ "name": "vpc" })), Some(&admin)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, ci) = api.req(Method::POST, "/gateways", Some(json!({ "name": "ci", "token": false })), Some(&admin)).await;
    assert!(s == StatusCode::CREATED && ci.get("token").is_none(), "{ci}");

    // The operator on a server without sign-in (no one marked on the request) gets its token.
    let made = api.ok(Method::POST, "/gateways", Some(json!({ "name": "vpc" }))).await;
    let gw = made["gateway"]["id"].as_str().unwrap().to_string();
    let secret = made["token"].as_str().unwrap().to_string();
    let grant = api.gate.tokens.lock().unwrap().get(&secret).cloned().unwrap();
    assert_eq!((grant.scope.0.as_slice(), made["gateway"]["token_id"].as_str()), (&[roll_secrets::gateways::rule(&gw)][..], Some(&*grant.id)), "one rule, naming the gateway");
    assert!(api.secrets.gateway_link(&gw, false, None, Some(&grant)).is_ok());
    // It opens its link and reads nothing; other tokens don't open the link.
    let (s, _) = api.req(Method::GET, "/environments/production/values?path=/", None, Some(&secret)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let reader_grant = api.gate.tokens.lock().unwrap().get(&reader).cloned().unwrap();
    assert!(api.secrets.gateway_link(&gw, false, None, Some(&reader_grant)).is_err());
    assert!(api.secrets.gateway_link(ci["gateway"]["id"].as_str().unwrap(), false, None, Some(&grant)).is_err(), "nor another gateway's");

    // Status as the link reports it; a replaced link's end leaves it online.
    let listed = api.ok(Method::GET, "/gateways", None).await;
    assert_eq!(listed.as_array().unwrap().len(), 2);
    assert_eq!(listed[1]["status"]["online"], false);
    let allow = vec!["10.0.0.0/8:5432".to_string()];
    let hello = |conn| roll_secrets::gateways::Hello { conn, node: "node-a", version: "0.1.0", allow: &allow, grant: Some(&grant.id) };
    api.secrets.gateway_connected(&gw, &hello("c1")).unwrap();
    api.secrets.gateway_connected(&gw, &hello("c2")).unwrap();
    api.secrets.gateway_disconnected(&gw, "c1", "replaced").unwrap();
    assert!(!api.secrets.gateway_seen(&gw, "c1").unwrap() && api.secrets.gateway_seen(&gw, "c2").unwrap());
    let status = api.ok(Method::GET, "/gateways", None).await[1]["status"].clone();
    assert_eq!((status["online"].as_bool(), status["node"].as_str(), status["allow"].clone()), (Some(true), Some("node-a"), json!(allow)));

    // A database through it: the provider is told which gateway (rolld lets it be private there).
    let db = json!({ "name": "billing", "url": "postgres://admin:hunter2@db.example:5432/app", "roles": ["app_blue", "app_green"], "gateway": gw });
    let d = api.ok(Method::POST, "/outside-databases", Some(db)).await;
    assert_eq!((d["gateway"].as_str(), api.outside.5.lock().unwrap().last().cloned().flatten()), (Some(gw.as_str()), Some(gw.clone())));
    let (s, _) = api.call(Method::POST, "/outside-databases", Some(json!({ "name": "y", "url": "postgres://a:b@db.example/x", "roles": ["app_c", "app_d"], "gateway": "gw_nottheirs" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "only the project's own gateways");
    // An identity that logs the gateway in: no scope of its own.
    let mut identity = json!({ "name": "gw-k8s", "issuer": "https://k8s.example", "audience": "roll", "subject": "system:serviceaccount:roll:gateway", "gateway": gw, "jwks": Issuer::new().jwks().to_string(), "actions": ["read"] });
    let (s, _) = api.call(Method::POST, "/identities", Some(identity.clone())).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    identity.as_object_mut().unwrap().remove("actions");
    let i = api.ok(Method::POST, "/identities", Some(identity)).await;
    assert_eq!(i["gateway"].as_str(), Some(gw.as_str()));

    // Deleting: refused while used, then its token is revoked and its link can't open.
    let (s, _) = api.call(Method::DELETE, &format!("/gateways/{gw}"), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    api.ok(Method::DELETE, &format!("/outside-databases/{}", d["id"].as_str().unwrap()), None).await;
    api.ok(Method::DELETE, &format!("/identities/{}", i["id"].as_str().unwrap()), None).await;
    let (s, _) = api.call(Method::DELETE, &format!("/gateways/{gw}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(!api.gate.tokens.lock().unwrap().contains_key(&secret), "its token is revoked");
    assert!(api.secrets.gateway_link(&gw, true, None, None).is_err());
    let audit = api.ok(Method::GET, "/audit?action=gateway.connect", None).await.to_string();
    assert!(audit.contains("node-a") && !audit.contains(&secret), "{audit}");
}

#[tokio::test]
async fn another_product_resolves_a_reference_only_for_a_resource_its_project_stands_for() {
    use roll_secrets::{Refusal, ResourceRef};
    use roll_workspace::Kind;
    let api = Api::new().await;
    api.set("production", "AWS_SECRET_ACCESS_KEY", "fake-secret-one").await;
    let photos = ResourceRef::new(Kind::Bucket, "photos");
    let reference = "roll://production/AWS_SECRET_ACCESS_KEY";
    let s = api.secrets.clone();
    let resolve = move |r: &ResourceRef, reference: &str| s.resolve_for(r, reference);

    // On no canvas: no project to read from.
    assert!(matches!(resolve(&photos, reference), Err(Refusal::NoProject(_))));
    // On a person's canvas that doesn't stand for it (anyone may put any bucket on theirs).
    let (_, mine) = api.as_ada(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "mine" }))).await;
    let mine = mine["id"].as_str().unwrap().to_string();
    let (s, _) = api.as_ada(Method::PUT, &format!("/v1/workspace/projects/{mine}/resources/bucket/photos"), Some(json!({ "x": 0, "y": 0 }))).await;
    assert!(s.is_success());
    let refused = resolve(&photos, reference).unwrap_err();
    assert!(matches!(refused, Refusal::NotTrusted(_)), "{refused}");

    // On the operator's project, which stands for its canvas: resolved as its runtime, protected
    // environments included (as a service's deploy), and audited as the bucket.
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{}/resources/bucket/photos", api.project), Some(json!({ "x": 0, "y": 0 }))).await;
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true, "approvals": 1 }))).await;
    let got = resolve(&photos, reference).unwrap();
    assert_eq!((got.value.expose(), got.version), ("fake-secret-one", 1));
    assert!(!format!("{got:?}").contains("fake-secret-one"), "the value redacts itself");
    let audit = api.ok(Method::GET, "/audit?action=secrets.resolve", None).await;
    assert_eq!((audit[0]["actor"].as_str(), audit[0]["keys"].clone()), (Some("storage:bucket:photos"), json!(["AWS_SECRET_ACCESS_KEY"])), "{audit}");
    assert!(!audit.to_string().contains("fake-secret-one"));
    // A reference may name only that project.
    assert!(resolve(&photos, &format!("roll://{}@production/AWS_SECRET_ACCESS_KEY", api.project)).is_ok());
    assert!(matches!(resolve(&photos, &format!("roll://{mine}@production/AWS_SECRET_ACCESS_KEY")), Err(Refusal::NotTrusted(_))));
    let missing = resolve(&photos, "roll://production/NOPE").unwrap_err();
    assert!(matches!(missing, Refusal::NoSuchKey(_)), "{missing}");
    // An ephemeral environment that isn't trusted never reaches its protected base.
    api.ok(Method::POST, "/environments", Some(json!({ "slug": "pr-1", "base": "production" }))).await;
    let refused = resolve(&photos, "roll://pr-1/AWS_SECRET_ACCESS_KEY").unwrap_err();
    assert!(matches!(refused, Refusal::Protected(_)) && !refused.to_string().contains("fake-secret-one"), "{refused}");

    // The digest moves with the value, without reading it.
    let before = api.secrets.digest_for(&photos, reference).unwrap();
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": false }))).await;
    api.set("production", "AWS_SECRET_ACCESS_KEY", "fake-secret-two").await;
    assert_ne!(api.secrets.digest_for(&photos, reference).unwrap(), before);
    assert_eq!(resolve(&photos, reference).unwrap().value.expose(), "fake-secret-two");
}

#[tokio::test]
async fn an_untrusted_resource_ref_never_reads_a_protected_environment_or_a_sensitive_key() {
    use roll_secrets::{Refusal, ResourceRef};
    use roll_workspace::Kind;
    let api = Api::new().await;
    api.set("production", "AWS_SECRET_ACCESS_KEY", "fake-protected").await;
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY", Some(json!({ "value": "fake-sensitive", "tags": ["sensitive"] }))).await;
    api.set("staging", "PLAIN_KEY", "fake-plain").await;
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true, "approvals": 1 }))).await;
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{}/resources/bucket/photos", api.project), Some(json!({ "x": 0, "y": 0 }))).await;
    let (trusted, branch) = (ResourceRef::new(Kind::Bucket, "photos"), ResourceRef::new(Kind::Bucket, "photos").untrusted());
    let (protected, sensitive, plain) = ("roll://production/AWS_SECRET_ACCESS_KEY", "roll://staging/STRIPE_KEY", "roll://staging/PLAIN_KEY");

    // The resource itself reads as its project's runtime.
    assert_eq!(api.secrets.resolve_for(&trusted, protected).unwrap().value.expose(), "fake-protected");
    assert_eq!(api.secrets.resolve_for(&trusted, sensitive).unwrap().value.expose(), "fake-sensitive");
    // The same ref as an untrusted copy: refused both, the digest refused the same way.
    for (reference, what) in [(protected, "protected"), (sensitive, "sensitive")] {
        let refused = api.secrets.resolve_for(&branch, reference).unwrap_err();
        let ok = match what {
            "protected" => matches!(refused, Refusal::Protected(_)),
            _ => matches!(refused, Refusal::Unusable(_)),
        };
        assert!(ok && !refused.to_string().contains("fake-"), "{what}: {refused}");
        let digest = api.secrets.digest_for(&branch, reference).unwrap_err();
        assert_eq!(std::mem::discriminant(&digest), std::mem::discriminant(&refused), "{what}: {digest}");
        assert!(api.secrets.digest_for(&trusted, reference).is_ok());
    }
    // What an untrusted reader may read still resolves, audited as the untrusted copy.
    assert_eq!(api.secrets.resolve_for(&branch, plain).unwrap().value.expose(), "fake-plain");
    assert!(api.secrets.digest_for(&branch, plain).is_ok());
    let audit = api.ok(Method::GET, "/audit?action=secrets.resolve", None).await;
    assert_eq!((audit[0]["actor"].as_str(), audit[0]["keys"].clone()), (Some("storage:bucket:photos (untrusted)"), json!(["PLAIN_KEY"])), "{audit}");
}

/// An incident's cut-off of a folder (`…/reach/cut-off`): what reach lists, cut in one call.
async fn cut_off(api: &Api, at: &str, body: Value) -> Value {
    api.ok(Method::POST, &format!("/environments/production/reach/cut-off?{at}"), Some(body)).await
}

fn ids(v: &Value) -> Vec<&str> {
    v.as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect()
}

#[tokio::test]
async fn a_cut_off_is_an_admins_and_revokes_the_tokens_that_reach_a_folder_and_no_others() {
    let api = Api::new().await;
    api.set("production", "STRIPE_KEY", "sk_test_fake_prod").await;
    api.set("staging", "STRIPE_KEY", "sk_test_fake_staging").await;
    let deploy = api.token(json!({ "name": "deploy", "environments": ["production"], "actions": ["read", "reveal", "write"] }));
    let ci = api.token(json!({ "name": "staging-ci", "environments": ["staging"], "actions": ["read", "reveal"] }));
    // Reach's own rule: a project admin's (a token that only reads and writes there can't).
    let (s, _) = api.req(Method::POST, "/environments/production/reach/cut-off?path=/", Some(json!({ "why": "fake incident" })), Some(&deploy)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(api.call(Method::POST, "/environments/production/reach/cut-off?path=/", Some(json!({ "why": " " }))).await.0, StatusCode::BAD_REQUEST, "it says why");
    let r = cut_off(&api, "path=/", json!({ "why": "incident 7: a fake leak" })).await;
    assert_eq!((ids(&r["tokens"]), r["batches"].as_u64(), r["failed"].clone()), (vec!["deploy"], Some(1), json!([])), "{r}");
    assert_eq!(api.req(Method::GET, "/environments/production/values", None, Some(&deploy)).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(api.req(Method::GET, "/environments/staging/values", None, Some(&ci)).await.0, StatusCode::OK, "a token that didn't reach it still works");
    // One audit row names what was cut and why; a second call finds nothing left.
    let audit = api.ok(Method::GET, "/audit?action=reach.cut_off", None).await;
    assert_eq!(audit.as_array().unwrap().len(), 1);
    assert!(audit[0]["summary"].as_str().unwrap().contains("1 token") && audit[0]["summary"].as_str().unwrap().contains("incident 7"), "{audit}");
    let again = cut_off(&api, "path=/", json!({ "why": "incident 7: a fake leak" })).await;
    assert_eq!((again["tokens"].clone(), again["batches"].clone()), (json!([]), json!(0)));
}

#[tokio::test]
async fn a_projects_admin_cuts_off_only_its_own_tokens_and_leaves_the_operators_to_the_operator() {
    let api = Api::new().await;
    let (_, mine) = api.as_ada(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "ada-shop" }))).await;
    let mine = mine["id"].as_str().unwrap().to_string();
    let at = |rest: &str| format!("/v1/secrets/projects/{mine}{rest}");
    api.as_ada(Method::PUT, &at("/environments/production/secrets/STRIPE_KEY"), Some(json!({ "value": "sk_test_fake" }))).await;
    // The operator's CI token reaches every project's secrets; hers names only her project.
    let everywhere = roll_auth::scope::Rule { product: "secrets".into(), resources: roll_auth::scope::Resources::All, environments: None, paths: None, unprotected: false, actions: BTreeMap::from([(roll_auth::scope::Action::Read, roll_auth::scope::Effect::Allow)]) };
    let (_, ops) = api.gate.grant("operator-ci", everywhere, None);
    let confined = |name: &str| api.gate.grant(name, roll_secrets::access::rule(&mine, None, None, &[roll_secrets::Action::Read]), None).1;
    let ci = confined("ada-ci");
    let live = |id: &str| api.gate.tokens.lock().unwrap().values().any(|g| &*g.id == id);
    let beyond = "reaches beyond this project: the operator revokes it";
    let (_, reach) = api.as_ada(Method::GET, &at("/environments/production/reach?path=/"), None).await;
    let marked: Vec<(&str, &Value)> = reach["tokens"].as_array().unwrap().iter().map(|h| (h["name"].as_str().unwrap(), &h["skipped"])).collect();
    assert_eq!(marked.len(), 2, "{reach}");
    assert!(marked.contains(&("operator-ci", &json!(beyond))) && marked.contains(&("ada-ci", &Value::Null)), "{reach}");
    // Her cut-off revokes her project's token and lists the operator's, which keeps working.
    let (s, r) = api.as_ada(Method::POST, &at("/environments/production/reach/cut-off?path=/"), Some(json!({ "why": "fake incident" }))).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!((ids(&r["tokens"]), r["skipped"].clone()), (vec!["ada-ci"], json!([{ "id": ops, "name": "operator-ci", "why": beyond }])), "{r}");
    assert!(live(&ops) && !live(&ci));
    // The operator's same cut-off revokes both kinds.
    let ci = confined("ada-ci-2");
    let r = api.ok(Method::POST, &at("/environments/production/reach/cut-off?path=/"), Some(json!({ "why": "fake incident" }))).await;
    let mut cut = ids(&r["tokens"]);
    cut.sort();
    assert_eq!((cut, r["skipped"].clone()), (vec!["ada-ci-2", "operator-ci"], json!([])), "{r}");
    assert!(!live(&ops) && !live(&ci));
}

#[tokio::test]
async fn a_cut_off_identity_doesnt_log_in_again_and_its_logins_end() {
    let api = Api::new().await;
    api.set("production", "API_KEY", "fake-prod-key").await;
    let issuer = Issuer::new();
    *api.sent.1.lock().unwrap() = Some(issuer.jwks());
    let i = api.ok(Method::POST, "/identities", Some(json!({ "name": "deploy", "issuer": ISSUER, "audience": "roll", "subject": "repo:acme/web:*", "environments": ["production"], "actions": ["read", "reveal"] }))).await;
    let id = i["id"].as_str().unwrap().to_string();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let login = || api.call(Method::POST, "/v1/secrets/login", Some(json!({ "identity": id, "token": issuer.sign(json!({ "iss": ISSUER, "aud": "roll", "sub": "repo:acme/web:ref:refs/heads/main", "exp": now + 300, "iat": now })) })));
    let (s, l) = login().await;
    assert_eq!(s, StatusCode::OK, "{l}");
    let r = cut_off(&api, "path=/", json!({ "why": "fake incident" })).await;
    assert_eq!((ids(&r["identities"]), r["logins"].as_u64()), (vec!["deploy (https://token.actions.githubusercontent.com repo:acme/web:*)"], Some(1)), "{r}");
    assert_eq!(api.req(Method::GET, "/environments/production/values", None, l["token"].as_str()).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(login().await.0, StatusCode::FORBIDDEN, "it doesn't log in again");
    // Until an admin enables it again.
    api.ok(Method::PATCH, &format!("/identities/{id}"), Some(json!({ "disabled": false }))).await;
    assert_eq!(login().await.0, StatusCode::OK);
}

#[tokio::test]
async fn a_cut_off_ends_the_folders_leases_and_revokes_their_credentials() {
    let api = Api::new().await;
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{}/resources/bucket/assets", api.project), Some(json!({ "x": 0, "y": 0 }))).await;
    api.ok(Method::PUT, "/environments/production/secrets/TEMP_KEY", Some(json!({ "link": { "kind": "bucket", "ref": "assets", "field": "access_key_id", "mode": "dynamic" } }))).await;
    api.ok(Method::POST, "/environments/production/resolve", Some(json!({ "consumer": "app:web" }))).await;
    assert_eq!(api.fake.live_keys.lock().unwrap().len(), 1);
    let r = cut_off(&api, "path=/&key=TEMP_KEY", json!({ "why": "fake incident" })).await;
    assert_eq!(ids(&r["leases"]), ["TEMP_KEY for app:web"], "{r}");
    assert!(api.fake.live_keys.lock().unwrap().is_empty(), "the product revoked it");
    assert!(api.ok(Method::GET, "/leases", None).await[0]["revoked_at"].is_i64());
}

#[tokio::test]
async fn a_cut_off_expires_the_keys_shares_and_unshares_the_folder() {
    let api = Api::new().await;
    api.set("production", "STRIPE_KEY", "sk_test_fake_prod").await;
    let share = |from: Value| json!({ "ciphertext": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", "note": "fake", "from": from });
    let made = api.ok(Method::POST, "/shares", Some(share(json!({ "environment": "production", "key": "STRIPE_KEY" })))).await;
    let other = api.ok(Method::POST, "/shares", Some(share(Value::Null))).await;
    let (_, p) = api.call(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "platform" }))).await;
    api.ok(Method::PUT, "/environments/production/exports?path=/", Some(json!([{ "project": p["id"] }]))).await;
    let reach = api.ok(Method::GET, "/environments/production/reach?path=/&key=STRIPE_KEY", None).await;
    assert_eq!((reach["shares"].as_array().unwrap().len(), reach["unattributed_shares"].as_u64()), (1, Some(1)), "{reach}");
    let r = cut_off(&api, "path=/&key=STRIPE_KEY", json!({ "why": "fake incident" })).await;
    assert_eq!((r["shares"][0]["id"].clone(), r["projects"].clone()), (made["id"].clone(), json!([p["id"]])), "{r}");
    assert_eq!(api.call(Method::GET, &format!("/v1/secrets/shares/{}", made["id"].as_str().unwrap()), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(api.call(Method::GET, &format!("/v1/secrets/shares/{}", other["id"].as_str().unwrap()), None).await.0, StatusCode::OK, "one that doesn't say where it came from stays");
    assert_eq!(api.ok(Method::GET, "/environments/production/exports?path=/", None).await, json!([]));
}

#[tokio::test]
async fn a_cut_off_pauses_the_folders_syncs_until_an_admin_resumes_them() {
    let api = Api::new().await;
    api.set("production", "DB_URL", "postgres://fake-db").await;
    let s = api.ok(Method::POST, "/syncs", Some(json!({ "environment": "production", "kind": "fake", "target": { "name": "x" } }))).await;
    let id = s["id"].as_str().unwrap().to_string();
    let r = cut_off(&api, "path=/", json!({ "why": "fake incident", "tokens": false })).await;
    assert_eq!(r["syncs"][0]["id"], s["id"], "{r}");
    api.set("production", "DB_URL", "postgres://fake-db-2").await;
    api.secrets.sweep().unwrap();
    assert!(api.sync.pushes.lock().unwrap().is_empty(), "a paused sync pushes nothing");
    let (st, e) = api.call(Method::POST, &format!("/syncs/{id}/run"), None).await;
    assert_eq!((st, e["code"].as_str()), (StatusCode::CONFLICT, Some("paused")));
    api.ok(Method::PATCH, &format!("/syncs/{id}"), Some(json!({ "paused": false }))).await;
    api.secrets.sweep().unwrap();
    assert_eq!(api.sync.pushes.lock().unwrap().len(), 1, "resumed, what changed goes out");
}

#[tokio::test]
async fn a_frozen_folder_answers_423_to_readers_and_lets_its_admins_through() {
    let api = Api::new().await;
    // Ada's own project: she administers it as a signed-in person, the operator as everyone's.
    let (_, mine) = api.as_ada(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "ada-shop" }))).await;
    let mine = mine["id"].as_str().unwrap().to_string();
    let at = |rest: &str| format!("/v1/secrets/projects/{mine}{rest}");
    api.as_ada(Method::PUT, &at("/environments/production/secrets/STRIPE_KEY"), Some(json!({ "value": "sk_test_fake_prod" }))).await;
    let reader = api.gate.issue("web", roll_secrets::access::rule(&mine, Some(vec!["production".into()]), None, &[roll_secrets::Action::Read, roll_secrets::Action::Reveal]), None);
    let (s, r) = api.as_ada(Method::POST, &at("/environments/production/reach/cut-off?path=/"), Some(json!({ "why": "fake incident", "tokens": false, "freeze": true }))).await;
    assert_eq!((s, r["frozen"].clone()), (StatusCode::OK, json!(true)), "{r}");
    let values = at("/environments/production/values");
    let (s, e) = api.req(Method::GET, &values, None, Some(&reader)).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::LOCKED, Some("frozen")), "{e}");
    // A sync isn't an admin, whoever made it: one made after the freeze pushes nothing.
    let (s, sync) = api.as_ada(Method::POST, &at("/syncs"), Some(json!({ "environment": "production", "kind": "fake", "target": { "name": "x" } }))).await;
    assert!(s.is_success(), "{s} {sync}");
    api.secrets.sweep().unwrap();
    assert!(api.sync.pushes.lock().unwrap().is_empty(), "a sync doesn't read past a freeze");
    // Its admins still read and write, so they can rotate: the person, and the operator.
    assert_eq!(api.as_ada(Method::GET, &values, None).await.1["values"]["STRIPE_KEY"], "sk_test_fake_prod");
    assert_eq!(api.ok(Method::GET, &values, None).await["values"]["STRIPE_KEY"], "sk_test_fake_prod");
    api.as_ada(Method::PUT, &at("/environments/production/secrets/STRIPE_KEY"), Some(json!({ "value": "sk_test_fake_rotated" }))).await;
    assert_eq!(api.as_ada(Method::DELETE, &at("/environments/production/reach/cut-off?path=/"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(api.req(Method::GET, &values, None, Some(&reader)).await.0, StatusCode::OK);
}

#[tokio::test]
async fn a_reference_a_person_chose_reads_only_what_they_may_reveal_now() {
    use roll_secrets::{Caller, Refusal, ResourceRef};
    use roll_workspace::Kind;
    let api = Api::new().await;
    let s = api.secrets.clone();
    let session = |user: &str, admin: bool| roll_auth::SignedIn { user: format!("usr_{user}"), handle: user.into(), admin, code_owner: None, session: format!("ses_{user}") };
    let as_root = s.caller_of(true, Some(&session("root", true)), None).unwrap();
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{}/resources/search/logs", api.project), Some(json!({ "x": 0, "y": 0 }))).await;
    let logs = ResourceRef::new(Kind::Search, "logs");
    api.set("production", "AWS_KEY", "fake-aws").await;
    api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": [{ "op": "set", "key": "STRIPE_KEY", "value": "fake-stripe", "tags": ["sensitive"] }, { "op": "set", "key": "WRAPPED", "value": "Bearer ${STRIPE_KEY}" }] }))).await;

    // A sensitive key is refused when it's chosen, even by an admin, and through a reference in a
    // value when it's read (a bind decrypts nothing); an operator-written target still reads it.
    let refused = s.bind_ref(&logs, "roll://production/STRIPE_KEY", &as_root).unwrap_err();
    assert!(matches!(refused, Refusal::Unusable(_)) && !refused.to_string().contains("fake-"), "{refused}");
    let wrapped = s.bind_ref(&logs, "roll://production/WRAPPED", &as_root).unwrap();
    let refused = s.resolve_as(&logs, "roll://production/WRAPPED", &wrapped).unwrap_err();
    assert!(matches!(refused, Refusal::Unusable(_)) && !refused.to_string().contains("fake-"), "{refused}");
    assert_eq!(s.resolve_for(&logs, "roll://production/STRIPE_KEY").unwrap().value.expose(), "fake-stripe");
    // A workload identity's login can't choose one: it ends with its token.
    let mut login = Caller::operator();
    login.principal = "identity:idn_1".into();
    assert!(matches!(s.bind_ref(&logs, "roll://production/AWS_KEY", &login), Err(Refusal::Failed(roll_secrets::Error::BadRequest(_)))));

    // roll apps' git login, named by a deployer of production (apps checked deploy): a sensitive
    // production key reads as the service's runtime there, audited as that, for them.
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{}/resources/service/web", api.project), Some(json!({ "x": 1, "y": 0 }))).await;
    let web = ResourceRef::new(Kind::Service, "web");
    let git = "roll://production/STRIPE_KEY";
    let deployer = s.bind_ref_runtime(&web, git, &as_root, "production").unwrap();
    assert_eq!(serde_json::to_value(&deployer).unwrap(), json!("usr_root@runtime:production"));
    assert_eq!(s.resolve_as(&web, git, &deployer).unwrap().value.expose(), "fake-stripe");
    assert!(s.digest_as(&web, git, &deployer).is_ok());
    let audit = api.ok(Method::GET, "/audit?action=secrets.resolve", None).await;
    assert_eq!(audit[0]["actor"].as_str(), Some("apps:service:web (runtime read for usr_root)"), "{audit}");
    assert!(matches!(s.bind_ref_runtime(&web, git, &login, "production"), Err(Refusal::Failed(roll_secrets::Error::BadRequest(_)))));
    // Any other environment keeps the maker's rules, and a runtime binding reads only its own.
    api.ok(Method::POST, "/environments/staging/changes", Some(json!({ "ops": [{ "op": "set", "key": "STRIPE_KEY", "value": "fake-stripe-staging", "tags": ["sensitive"] }] }))).await;
    let refused = s.bind_ref_runtime(&web, "roll://staging/STRIPE_KEY", &as_root, "production").unwrap_err();
    assert!(matches!(refused, Refusal::Unusable(_)) && !refused.to_string().contains("fake-"), "{refused}");
    let refused = s.resolve_as(&web, "roll://staging/STRIPE_KEY", &deployer).unwrap_err();
    assert!(matches!(refused, Refusal::NotAllowed(_)) && !refused.to_string().contains("fake-"), "{refused}");

    // An admin's session is kept as the person, and the read is audited as the resource, for them.
    let reference = "roll://production/AWS_KEY";
    let root = s.bind_ref(&logs, reference, &as_root).unwrap();
    assert_eq!(serde_json::to_value(&root).unwrap(), json!("usr_root"));
    assert_eq!(s.resolve_as(&logs, reference, &root).unwrap().value.expose(), "fake-aws");
    let audit = api.ok(Method::GET, "/audit?action=secrets.resolve", None).await;
    assert_eq!(audit[0]["actor"].as_str(), Some("search:search:logs"), "{audit}");
    assert!(audit[0].to_string().contains("usr_root"), "{audit}");
    // Tagged sensitive later: the next read is refused, the digest the same way.
    api.set("production", "LATER_KEY", "fake-later").await;
    let later = s.bind_ref(&logs, "roll://production/LATER_KEY", &as_root).unwrap();
    assert!(s.resolve_as(&logs, "roll://production/LATER_KEY", &later).is_ok());
    api.ok(Method::PUT, "/environments/production/secrets/LATER_KEY", Some(json!({ "tags": ["sensitive"] }))).await;
    assert!(matches!(s.resolve_as(&logs, "roll://production/LATER_KEY", &later), Err(Refusal::Unusable(_))));
    assert!(matches!(s.digest_as(&logs, "roll://production/LATER_KEY", &later), Err(Refusal::Unusable(_))));
    // Demoted, or unable to sign in: refused until they're an admin again.
    for flag in [&api.people.root_demoted, &api.people.root_disabled] {
        *flag.lock().unwrap() = true;
        let refused = s.resolve_as(&logs, reference, &root).unwrap_err();
        assert!(matches!(refused, Refusal::MakerUnreachable(_)), "{refused}");
        assert!(matches!(s.digest_as(&logs, reference, &root), Err(Refusal::MakerUnreachable(_))));
        *flag.lock().unwrap() = false;
    }
    assert!(s.resolve_as(&logs, reference, &root).is_ok());
    // An incident's freeze stops what anyone chose, an admin too (#304): their session reads past
    // it, a reference they chose doesn't, nor a runtime binding. Unfrozen, both read again.
    let runtime = s.bind_ref_runtime(&web, reference, &as_root, "production").unwrap();
    let r = cut_off(&api, "path=/", json!({ "why": "fake incident", "tokens": false, "freeze": true })).await;
    assert_eq!(r["frozen"], json!(true), "{r}");
    for (resource, maker) in [(&logs, &root), (&web, &runtime)] {
        let refused = s.resolve_as(resource, reference, maker).unwrap_err();
        assert!(matches!(refused, Refusal::Failed(roll_secrets::Error::Frozen(_))) && !refused.to_string().contains("fake-"), "{refused}");
        assert!(s.digest_as(resource, reference, maker).is_err());
    }
    assert_eq!(api.ok(Method::GET, "/environments/production/values", None).await["values"]["AWS_KEY"], "fake-aws");
    api.ok(Method::DELETE, "/environments/production/reach/cut-off?path=/", None).await;
    assert!(s.resolve_as(&logs, reference, &root).is_ok() && s.resolve_as(&web, reference, &runtime).is_ok());

    // An untrusted copy (a branch of the cluster) never gets a protected value.
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true, "approvals": 1 }))).await;
    assert!(s.resolve_as(&logs, reference, &root).is_ok());
    let branch = logs.clone().untrusted();
    assert!(matches!(s.resolve_as(&branch, reference, &root), Err(Refusal::Protected(_))));
    assert!(matches!(s.bind_ref(&branch, reference, &as_root), Err(Refusal::Protected(_))));
    // Nor a runtime binding: an untrusted copy of the service reads as an untrusted caller.
    let runtime = s.bind_ref_runtime(&web, reference, &as_root, "production").unwrap();
    assert_eq!(s.resolve_as(&web, reference, &runtime).unwrap().value.expose(), "fake-aws");
    let copy = web.clone().untrusted();
    assert!(matches!(s.resolve_as(&copy, reference, &runtime), Err(Refusal::Protected(_))));
    assert!(matches!(s.bind_ref_runtime(&copy, reference, &as_root, "production"), Err(Refusal::Protected(_))));
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": false }))).await;

    // A staging-only token can't reach production through staging's import, though the
    // resource's runtime would read it there.
    api.ok(Method::PUT, "/environments/staging/imports?path=/", Some(json!([{ "environment": "production", "path": "/" }]))).await;
    let staging = "roll://staging/AWS_KEY";
    assert_eq!(s.resolve_for(&logs, staging).unwrap().value.expose(), "fake-aws");
    let (secret, _) = api.gate.grant("staging only", roll_secrets::access::rule(&api.project, Some(vec!["staging".into()]), None, &[roll_secrets::Action::Read, roll_secrets::Action::Reveal]), None);
    let grant = api.gate.tokens.lock().unwrap()[&secret].clone();
    let token = s.caller_of(false, None, Some(&grant)).unwrap();
    assert!(matches!(s.bind_ref(&logs, "roll://production/AWS_KEY", &token), Err(Refusal::NotAllowed(_))));
    let refused = s.bind_ref(&logs, staging, &token).unwrap_err();
    assert!(matches!(refused, Refusal::NoSuchKey(_)) && !refused.to_string().contains("fake-"), "{refused}");
    // Set in staging itself, it's the token's to choose; revoked, the reference stops.
    api.set("staging", "AWS_KEY", "fake-staging").await;
    let by_token = s.bind_ref(&logs, staging, &token).unwrap();
    assert_eq!(s.resolve_as(&logs, staging, &by_token).unwrap().value.expose(), "fake-staging");
    api.gate.tokens.lock().unwrap().remove(&secret);
    assert!(matches!(s.resolve_as(&logs, staging, &by_token), Err(Refusal::MakerUnreachable(_))));

    // A person's own project: her reference reads while she's in it, and stops once she isn't.
    let (_, mine) = api.as_ada(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "mine" }))).await;
    let mine = mine["id"].as_str().unwrap().to_string();
    api.as_ada(Method::PUT, &format!("/v1/workspace/projects/{mine}/resources/repo/ada%2Fsite"), Some(json!({ "x": 0, "y": 0 }))).await;
    let (st, e) = api.as_ada(Method::PUT, &format!("/v1/secrets/projects/{mine}/environments/production/secrets/DEPLOY_KEY"), Some(json!({ "value": "fake-ada" }))).await;
    assert!(st.is_success(), "{e}");
    let site = ResourceRef::new(Kind::Repo, "ada/site");
    let as_ada = s.caller_of(false, Some(&session("ada", false)), None).unwrap();
    let ada = s.bind_ref(&site, "roll://production/DEPLOY_KEY", &as_ada).unwrap();
    assert_eq!(s.resolve_as(&site, "roll://production/DEPLOY_KEY", &ada).unwrap().value.expose(), "fake-ada");
    // Her roll code owner's reviewer key: through the project holding one of her repos, read in
    // reach as `code:review:ada`. Nobody else's owner reads it.
    let review = ResourceRef::code_owner("ada");
    let reviewer = s.bind_ref(&review, "roll://production/DEPLOY_KEY", &as_ada).unwrap();
    assert_eq!(s.resolve_as(&review, "roll://production/DEPLOY_KEY", &reviewer).unwrap().value.expose(), "fake-ada");
    // One of her agent machines (an endpoint's key): the same project, as `agent:laptop`.
    let laptop = ResourceRef::agent_machine("ada", "laptop");
    let on_laptop = s.bind_ref(&laptop, "roll://production/DEPLOY_KEY", &as_ada).unwrap();
    assert_eq!(s.resolve_as(&laptop, "roll://production/DEPLOY_KEY", &on_laptop).unwrap().value.expose(), "fake-ada");
    let (_, reach) = api.as_ada(Method::GET, &format!("/v1/secrets/projects/{mine}/environments/production/reach?path=/&key=DEPLOY_KEY"), None).await;
    let readers: Vec<&str> = reach["readers"].as_array().unwrap().iter().filter_map(|r| r["actor"].as_str()).collect();
    assert!(readers.contains(&"code:review:ada") && readers.contains(&"agent:laptop"), "{readers:?}");
    assert!(matches!(s.bind_ref(&ResourceRef::code_owner("bob"), "roll://production/DEPLOY_KEY", &as_ada), Err(Refusal::NoProject(_))));
    let (st, e) = api.as_ada(Method::POST, &format!("/v1/secrets/projects/{mine}/environments/production/changes"), Some(json!({ "ops": [{ "op": "set", "key": "GIT_TOKEN", "value": "fake-git", "tags": ["sensitive"] }] }))).await;
    assert!(st.is_success(), "{e}");
    let ada_runtime = s.bind_ref_runtime(&site, "roll://production/GIT_TOKEN", &as_ada, "production").unwrap();
    assert_eq!(s.resolve_as(&site, "roll://production/GIT_TOKEN", &ada_runtime).unwrap().value.expose(), "fake-git");
    // Handed to the operator, without her (docs/members.md: what a person reaches is their
    // memberships now).
    api.ws_db.write(|c| c.execute("UPDATE projects SET owner = '' WHERE id = ?", [&mine])).unwrap();
    api.ws_db.write(|c| c.execute("DELETE FROM members WHERE project_id = ? AND user_id = 'usr_ada'", [&mine])).unwrap();
    let refused = s.resolve_as(&site, "roll://production/DEPLOY_KEY", &ada).unwrap_err();
    assert!(matches!(refused, Refusal::MakerUnreachable(_)), "{refused}");
    let refused = s.resolve_as(&review, "roll://production/DEPLOY_KEY", &reviewer).unwrap_err();
    assert!(matches!(refused, Refusal::MakerUnreachable(_)), "{refused}");
    assert!(matches!(s.digest_as(&laptop, "roll://production/DEPLOY_KEY", &on_laptop), Err(Refusal::MakerUnreachable(_))));
    let refused = s.resolve_as(&site, "roll://production/GIT_TOKEN", &ada_runtime).unwrap_err();
    assert!(matches!(refused, Refusal::MakerUnreachable(_)) && !refused.to_string().contains("fake-"), "{refused}");
}

#[tokio::test]
async fn a_sync_made_from_an_admins_session_acts_as_them_and_stops_when_theyre_demoted() {
    let api = Api::new().await;
    api.set("staging", "PLATFORM_TOKEN", "tok_1").await;
    let (s, made) = api.signed_in("root", true, Method::POST, &format!("/v1/secrets/projects/{}/syncs", api.project), Some(json!({ "environment": "staging", "kind": "fake", "target": { "name": "x", "token_key": "PLATFORM_TOKEN" } }))).await;
    assert_eq!((s, made["created_by"].as_str()), (StatusCode::CREATED, Some("usr_root")), "{made}");
    api.set("staging", "A", "1").await;
    api.secrets.sweep().unwrap();
    assert_eq!(api.sync.pushes.lock().unwrap().len(), 1);
    *api.people.root_demoted.lock().unwrap() = true;
    api.set("staging", "A", "2").await;
    api.secrets.sweep().unwrap();
    assert_eq!(api.sync.pushes.lock().unwrap().len(), 1, "nothing pushed once they're no admin");
    // One made as the operator account (the deployment's token, or an admin's session before
    // makers were people) stays as it was, and health says so.
    let old = api.ok(Method::POST, "/syncs", Some(json!({ "environment": "staging", "kind": "fake", "target": { "name": "y", "token_key": "PLATFORM_TOKEN" } }))).await;
    assert_eq!(old["created_by"], "operator");
    let found = api.ok(Method::GET, "/health", None).await;
    let flagged: Vec<&str> = found.as_array().unwrap().iter().filter(|f| f["issue"] == "operator_made").map(|f| f["key"].as_str().unwrap()).collect();
    assert_eq!(flagged, [old["id"].as_str().unwrap()], "{found}");
}

/// An answer as the request's page makes it: each value sealed to the request's public key.
fn answer(id: &str, public_key: &str, values: &[(&str, &str)]) -> Value {
    let boxes: serde_json::Map<String, Value> = values
        .iter()
        .map(|(k, v)| (k.to_string(), serde_json::to_value(roll_secrets::crypto::seal_to(public_key, &roll_secrets::value_requests::answer_context(id, k), v.as_bytes()).unwrap()).unwrap()))
        .collect();
    json!({ "values": boxes })
}

#[tokio::test]
async fn a_secret_request_lands_an_outside_value_once_as_a_change() {
    let api = Api::new().await;
    // Asking takes `write` on the folder: the answer is written as whoever asked.
    let reader = api.token(json!({ "name": "reader", "actions": ["read"] }));
    let (s, _) = api.req(Method::POST, "/environments/staging/value-requests?path=/vendor", Some(json!({ "keys": ["VENDOR_KEY"] })), Some(&reader)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let r = api.ok(Method::POST, "/environments/staging/value-requests?path=/vendor", Some(json!({ "keys": ["VENDOR_KEY", "VENDOR_SECRET"], "note": "the example vendor's sandbox keys" }))).await;
    let (id, token) = (r["id"].as_str().unwrap().to_string(), r["token"].as_str().unwrap().to_string());
    assert_eq!((r["open"].as_bool(), r["answers_left"].as_i64(), r["expires_at"].as_i64().unwrap() - r["created_at"].as_i64().unwrap()), (Some(true), Some(1), 7 * 86_400_000));
    assert!(token.starts_with("rlq_") && roll_auth::credentials::checksum_ok(&token), "{token}");

    // A reader lists the requests but never their tokens, and the id answers nothing: the same 404
    // as a wrong token or one whose checksum fails, naming none of them.
    let (s, list) = api.req(Method::GET, "/value-requests", None, Some(&reader)).await;
    assert_eq!((s, list[0]["id"].as_str()), (StatusCode::OK, Some(id.as_str())));
    assert!(list[0].get("token").is_none() && list[0].get("link").is_none() && !list.to_string().contains(&token), "{list}");
    let mut bent = token.clone();
    bent.replace_range(bent.len() - 1.., if token.ends_with('a') { "b" } else { "a" });
    let mut misses = Vec::new();
    for t in [id.clone(), bent, roll_auth::credentials::new_secret("rlq_"), "rlq_fake".into()] {
        let (s, e) = api.call(Method::POST, &format!("/v1/secrets/value-requests/{t}/answer"), Some(json!({ "values": {} }))).await;
        let (g, _) = api.call(Method::GET, &format!("/v1/secrets/value-requests/{t}"), None).await;
        assert!(!e.to_string().contains(&t), "{e}");
        misses.push((s, g, e));
    }
    assert!(misses.iter().all(|m| *m == misses[0]) && misses[0].0 == StatusCode::NOT_FOUND && misses[0].1 == StatusCode::NOT_FOUND, "{misses:?}");

    // The page shows the note and the key names, and the key to seal to; looking uses nothing up.
    let (s, peek) = api.call(Method::GET, &format!("/v1/secrets/value-requests/{token}"), None).await;
    assert_eq!((s, peek["id"].as_str(), peek["note"].as_str(), peek["keys"].clone()), (StatusCode::OK, Some(id.as_str()), Some("the example vendor's sandbox keys"), json!(["VENDOR_KEY", "VENDOR_SECRET"])));
    assert!(peek.get("environment").is_none() && peek.get("created_by").is_none(), "the page names nothing about the project: {peek}");
    let public = peek["public_key"].as_str().unwrap().to_string();
    // A box sealed for another key, or a missing key, is refused and uses nothing up.
    let mut swapped = answer(&id, &public, &[("VENDOR_KEY", "fake-key-1"), ("VENDOR_SECRET", "fake-secret-1")]);
    swapped["values"]["VENDOR_KEY"] = swapped["values"]["VENDOR_SECRET"].clone();
    let (s, e) = api.call(Method::POST, &format!("/v1/secrets/value-requests/{token}/answer"), Some(swapped)).await;
    assert!(s == StatusCode::BAD_REQUEST && !e.to_string().contains("fake-secret-1"), "{s} {e}");
    let (s, _) = api.call(Method::POST, &format!("/v1/secrets/value-requests/{token}/answer"), Some(answer(&id, &public, &[("VENDOR_KEY", "fake-key-1")]))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, body) = api.call(Method::POST, &format!("/v1/secrets/value-requests/{token}/answer"), Some(answer(&id, &public, &[("VENDOR_KEY", "fake-key-1"), ("VENDOR_SECRET", "fake-secret-1")]))).await;
    assert_eq!((s, body), (StatusCode::NO_CONTENT, Value::String(String::new())), "the answer shows nothing back");
    let values = api.values("staging", "/vendor").await;
    assert_eq!((values["values"]["VENDOR_KEY"].as_str(), values["values"]["VENDOR_SECRET"].as_str()), (Some("fake-key-1"), Some("fake-secret-1")));
    let changes = api.ok(Method::GET, "/environments/staging/changes", None).await;
    assert_eq!(changes[0]["by"], format!("request:{id}"));
    assert!(changes[0]["message"].as_str().unwrap().contains("asked by operator"), "{}", changes[0]);

    // Answered once: the link is a 410 now, for the page and for another answer.
    let (s, e) = api.call(Method::POST, &format!("/v1/secrets/value-requests/{token}/answer"), Some(answer(&id, &public, &[("VENDOR_KEY", "fake-key-2"), ("VENDOR_SECRET", "fake-secret-2")]))).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::GONE, Some("gone")));
    assert_eq!(api.call(Method::GET, &format!("/v1/secrets/value-requests/{token}"), None).await.0, StatusCode::GONE);
    assert_eq!(api.values("staging", "/vendor").await["values"]["VENDOR_KEY"], "fake-key-1");
    let list = api.ok(Method::GET, "/value-requests", None).await;
    assert_eq!((list[0]["open"].as_bool(), list[0]["outcome"]["change"].as_i64()), (Some(false), changes[0]["seq"].as_i64()));
    let audit = api.ok(Method::GET, "/audit?action=value_request", None).await;
    let actions: Vec<&str> = audit.as_array().unwrap().iter().map(|a| a["action"].as_str().unwrap()).collect();
    assert_eq!(actions, ["value_request.answer", "value_request.create"]);
    assert!(!audit.to_string().contains("fake-key-1") && !list.to_string().contains("fake-key-1"));

    // An expired request, and a cancelled one, are 410s too.
    let late = api.ok(Method::POST, "/environments/staging/value-requests", Some(json!({ "keys": ["LATE"], "ttl_minutes": 60 }))).await;
    let (late_id, late_token) = (late["id"].as_str().unwrap(), late["token"].as_str().unwrap());
    api.db.write(|c| c.execute("UPDATE value_requests SET expires_at = 1 WHERE id = ?", [late_id])).unwrap();
    assert_eq!(api.call(Method::GET, &format!("/v1/secrets/value-requests/{late_token}"), None).await.0, StatusCode::GONE);
    let gone = api.ok(Method::POST, "/environments/staging/value-requests", Some(json!({ "keys": ["NEVER"] }))).await;
    let (gone_id, gone_token) = (gone["id"].as_str().unwrap(), gone["token"].as_str().unwrap());
    let (_, peek) = api.call(Method::GET, &format!("/v1/secrets/value-requests/{gone_token}"), None).await;
    let (s, _) = api.req(Method::DELETE, &format!("/value-requests/{gone_id}"), None, Some(&reader)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "only whoever asked, or an admin");
    let (s, _) = api.call(Method::DELETE, &format!("/value-requests/{gone_id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = api.call(Method::POST, &format!("/v1/secrets/value-requests/{gone_token}/answer"), Some(answer(gone_id, peek["public_key"].as_str().unwrap(), &[("NEVER", "fake")]))).await;
    assert_eq!(s, StatusCode::GONE);
    // A token found leaked (a landing, GitHub's secret scanning) cancels its request.
    let leaked = api.ok(Method::POST, "/environments/staging/value-requests", Some(json!({ "keys": ["LEAKED"] }))).await;
    let leaked_token = leaked["token"].as_str().unwrap();
    assert_eq!(api.secrets.cancel_leaked_value_request(leaked_token, "found in a test"), Some(format!("secret request {}", leaked["id"].as_str().unwrap())));
    assert_eq!(api.call(Method::GET, &format!("/v1/secrets/value-requests/{leaked_token}"), None).await.0, StatusCode::GONE);
    assert_eq!(api.secrets.cancel_leaked_value_request(leaked_token, "again"), None, "once");
}

#[tokio::test]
async fn a_secret_requests_answer_is_stored_as_sent_and_never_replayed() {
    let api = Api::new().await;
    api.set("production", "DB_PASSWORD", "fake-production-password").await;
    let r = api.ok(Method::POST, "/environments/staging/value-requests?path=/vendor", Some(json!({ "keys": ["VENDOR_KEY"], "answers": 3 }))).await;
    let (id, token) = (r["id"].as_str().unwrap().to_string(), r["token"].as_str().unwrap().to_string());
    let (_, peek) = api.call(Method::GET, &format!("/v1/secrets/value-requests/{token}"), None).await;
    let public = peek["public_key"].as_str().unwrap().to_string();
    let url = format!("/v1/secrets/value-requests/{token}/answer");

    // An outsider's `${…}` is text, not a reference: a broad reader never gets production's value.
    let sent = answer(&id, &public, &[("VENDOR_KEY", "${production.DB_PASSWORD}")]);
    assert_eq!(api.call(Method::POST, &url, Some(sent.clone())).await.0, StatusCode::NO_CONTENT);
    assert_eq!(api.values("staging", "/vendor").await["values"]["VENDOR_KEY"], "${production.DB_PASSWORD}");
    let meta = api.ok(Method::GET, "/environments/staging/secrets/VENDOR_KEY?path=/vendor", None).await;
    assert!(meta["refs"].as_array().is_none_or(|r| r.is_empty()), "{meta}");

    // The same boxes again are refused and use nothing up, with their version changed too.
    let (s, e) = api.call(Method::POST, &url, Some(sent.clone())).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("conflict")), "{e}");
    let mut renumbered = sent;
    renumbered["values"]["VENDOR_KEY"]["v"] = json!(2);
    assert_eq!(api.call(Method::POST, &url, Some(renumbered)).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(api.ok(Method::GET, "/value-requests", None).await[0]["answers_left"], 2);

    // The request's own count stops a sixth attempt in the minute, whatever else is asked for.
    for _ in 0..2 {
        assert_eq!(api.call(Method::POST, &url, Some(json!({ "values": {} }))).await.0, StatusCode::BAD_REQUEST);
    }
    assert_eq!(api.call(Method::POST, &url, Some(answer(&id, &public, &[("VENDOR_KEY", "fake-key")]))).await.0, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn a_secret_request_into_a_folder_that_needs_approval_opens_a_change_request() {
    let api = Api::new().await;
    api.ok(Method::PUT, "/environments/production/folder-policies?path=/payments", Some(json!({ "approvals": 1 }))).await;
    let r = api.ok(Method::POST, "/environments/production/value-requests?path=/payments", Some(json!({ "keys": ["PSP_KEY"] }))).await;
    let (id, token) = (r["id"].as_str().unwrap(), r["token"].as_str().unwrap());
    let (_, peek) = api.call(Method::GET, &format!("/v1/secrets/value-requests/{token}"), None).await;
    let (s, _) = api.call(Method::POST, &format!("/v1/secrets/value-requests/{token}/answer"), Some(answer(id, peek["public_key"].as_str().unwrap(), &[("PSP_KEY", "fake-psp-key")]))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(api.values("production", "/payments").await["values"].get("PSP_KEY").is_none(), "nothing lands before it's approved");
    let list = api.ok(Method::GET, "/value-requests", None).await;
    let request = list[0]["outcome"]["request"].as_str().unwrap();
    let cr = api.ok(Method::GET, &format!("/environments/production/requests/{request}"), None).await;
    // By whoever asked, so they can't approve their own answer in.
    assert_eq!((cr["state"].as_str(), cr["by"].as_str(), cr["items"][0]["key"].as_str()), (Some("open"), Some("operator"), Some("PSP_KEY")));
    let (s, _) = api.call(Method::POST, &format!("/environments/production/requests/{request}/review"), Some(json!({ "verdict": "approve" }))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let lead = api.token(json!({ "name": "lead", "actions": ["read", "write", "admin"] }));
    api.req(Method::POST, &format!("/environments/production/requests/{request}/review"), Some(json!({ "verdict": "approve" })), Some(&lead)).await;
    api.ok(Method::POST, &format!("/environments/production/requests/{request}/apply"), None).await;
    assert_eq!(api.values("production", "/payments").await["values"]["PSP_KEY"], "fake-psp-key");
}

/// A token of roll's issuer: its header and claims, and whether a key of `jwks` signed it.
fn roll_token(jwks: &Value, token: &str) -> (Value, Value, bool) {
    use base64::Engine;
    let b64 = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).unwrap();
    let parts: Vec<&str> = token.split('.').collect();
    let (header, claims): (Value, Value) = (serde_json::from_slice(&b64(parts[0])).unwrap(), serde_json::from_slice(&b64(parts[1])).unwrap());
    let signed = jwks["keys"].as_array().unwrap().iter().filter(|k| k["kid"] == header["kid"]).any(|k| {
        let public = ring::signature::RsaPublicKeyComponents { n: b64(k["n"].as_str().unwrap()), e: b64(k["e"].as_str().unwrap()) };
        public.verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, format!("{}.{}", parts[0], parts[1]).as_bytes(), &b64(parts[2])).is_ok()
    });
    (header, claims, signed)
}

fn app(project: &str, service: &str, env: &str) -> roll_secrets::Subject {
    roll_secrets::Subject::App { project: project.into(), service: service.into(), env: env.into() }
}

#[tokio::test]
async fn roll_signs_an_apps_instance_a_token_for_an_hour_at_most() {
    let api = Api::new().await;
    let (s, p) = (api.secrets.clone(), api.project.clone());
    let (tokens, refused) = tokio::task::spawn_blocking(move || {
        let hour = std::time::Duration::from_secs(3600);
        let tokens: Vec<String> = (0..3).map(|_| s.issue(app(&p, "svc_web", "production"), "sts.amazonaws.com", 2 * hour).unwrap()).collect();
        // No part may hold a `:` (a sub names one environment), nor an audience be empty or huge.
        let refused = [s.issue(app(&p, "svc_web", "prod:x"), "a", hour), s.issue(app(&p, "svc:web", "production"), "a", hour), s.issue(app(&p, "svc_web", "production"), "", hour), s.issue(app(&p, "svc_web", "production"), &"a".repeat(257), hour)]
            .into_iter()
            .map(|r| matches!(r, Err(roll_secrets::Error::BadRequest(_))))
            .collect::<Vec<_>>();
        (tokens, refused)
    })
    .await
    .unwrap();
    assert_eq!(refused, [true; 4]);
    let jwks = api.ok(Method::GET, "/v1/secrets/oidc/jwks", None).await;
    let (header, claims, signed) = roll_token(&jwks, &tokens[0]);
    assert!(signed && header["alg"] == "RS256", "{header}");
    let sub = format!("roll-apps:{}:svc_web:production", api.project);
    assert_eq!((claims["iss"].as_str(), claims["sub"].as_str(), claims["aud"].as_str()), (Some(ISSUER_URL), Some(sub.as_str()), Some("sts.amazonaws.com")));
    assert_eq!((claims["project"].as_str(), claims["service"].as_str(), claims["environment"].as_str()), (Some(api.project.as_str()), Some("svc_web"), Some("production")));
    assert_eq!(claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(), 3600, "two hours asked, one given");
    assert!(claims["nbf"] == claims["iat"] && claims["jti"].is_string() && claims["jti"] != roll_token(&jwks, &tokens[1]).1["jti"]);
    // Three tokens, one audit row for the hour.
    let audit = api.ok(Method::GET, "/audit", None).await;
    let issued: Vec<&Value> = audit.as_array().unwrap().iter().filter(|a| a["action"] == "oidc.issue").collect();
    assert_eq!(issued.len(), 1, "{audit}");
    assert_eq!((issued[0]["actor"].as_str(), issued[0]["environment"].as_str()), (Some("app:svc_web"), Some("production")));
    assert!(!audit.to_string().contains(tokens[0].as_str()), "never the token");
}

/// A preview's own service (roll previews' `roll.toml` services) gets roll's word under a subject of
/// its own, never an app's, in the preview's environment; a fork's preview (untrusted) gets nothing,
/// decided here from the environment's own flag; each is audited as the preview's service, never
/// the token.
#[tokio::test]
async fn a_previews_service_gets_a_token_of_its_own_and_a_forks_none() {
    let api = Api::new().await;
    for (slug, trusted) in [("pv-vetted1", true), ("pv-fork1", false)] {
        api.ok(Method::POST, "/environments", Some(json!({ "slug": slug, "base": "production", "trusted": trusted }))).await;
    }
    // A standard environment a person named like a preview's.
    api.ok(Method::POST, "/environments", Some(json!({ "slug": "pv-std1" }))).await;
    let (s, p) = (api.secrets.clone(), api.project.clone());
    let (vetted, fork, standard) = tokio::task::spawn_blocking(move || {
        let sub = |preview: &str| roll_secrets::Subject::Preview { project: p.clone(), preview: preview.into(), service: "web".into() };
        let hour = std::time::Duration::from_secs(3600);
        (s.issue(sub("pv_vetted1"), "sts.amazonaws.com", 2 * hour), s.issue(sub("pv_fork1"), "sts.amazonaws.com", hour), s.issue(sub("pv_std1"), "sts.amazonaws.com", hour))
    })
    .await
    .unwrap();
    assert!(matches!(fork, Err(roll_secrets::Error::Forbidden(_))), "{fork:?}");
    assert!(matches!(standard, Err(roll_secrets::Error::NotFound(_))), "a standard environment is no preview's: {standard:?}");
    let token = vetted.unwrap();
    let jwks = api.ok(Method::GET, "/v1/secrets/oidc/jwks", None).await;
    let (_, claims, signed) = roll_token(&jwks, &token);
    assert!(signed);
    let sub = format!("roll-previews:{}:pv_vetted1:web", api.project);
    assert_eq!(claims["sub"], json!(sub));
    assert_eq!((claims["environment"].clone(), claims["preview"].clone(), claims["preview_id"].clone()), (json!("pv-vetted1"), json!(true), json!("pv_vetted1")));
    assert_eq!(claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(), 3600, "an hour at most");
    assert!(matches!(roll_secrets::Subject::parse(&sub), Some(roll_secrets::Subject::Preview { .. })));
    let audit = api.ok(Method::GET, "/audit", None).await;
    assert!(audit.as_array().unwrap().iter().any(|a| a["action"] == "oidc.issue" && a["actor"] == "preview:pv_vetted1:web"), "{audit}");
    assert!(!audit.to_string().contains(token.as_str()), "never the token");
}

#[tokio::test]
async fn the_issuers_key_rotates_published_a_day_before_it_signs_and_retires_after_its_tokens() {
    use roll_secrets::issuer::{PUBLISH_AHEAD_MS, RETIRE_AFTER_MS};
    let api = Api::new().await;
    api.set("production", "A", "v").await;
    let issue = || {
        let (s, p) = (api.secrets.clone(), api.project.clone());
        async move { tokio::task::spawn_blocking(move || s.issue(app(&p, "svc_web", "production"), "roll", std::time::Duration::from_secs(3600)).unwrap()).await.unwrap() }
    };
    let sweep = || {
        let s = api.secrets.clone();
        async move { tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap() }
    };
    let old_token = issue().await;
    let jwks = api.ok(Method::GET, "/v1/secrets/oidc/jwks", None).await;
    let old = roll_token(&jwks, &old_token).0["kid"].as_str().unwrap().to_string();
    // A digest signed with the key that will be replaced.
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep_audit_chain(i64::MAX / 2).unwrap()).await.unwrap();
    let now = code_sql::now_ms();
    let at = |kid: &str, signs_from: i64| {
        let kid = kid.to_string();
        api.db.write(move |c| c.execute("UPDATE oidc_keys SET signs_from = ? WHERE kid = ?", code_sql::params![signs_from, kid])).unwrap();
    };

    // Not due yet: nothing changes.
    sweep().await;
    assert_eq!(kids(&api.ok(Method::GET, "/v1/secrets/oidc/jwks", None).await), [old.as_str()]);
    // 88 days on, the next key is published, and waits two days to sign.
    at(&old, now - 88 * 86_400_000 - 1000);
    sweep().await;
    let jwks = api.ok(Method::GET, "/v1/secrets/oidc/jwks", None).await;
    let next = kids(&jwks).into_iter().find(|k| *k != old).expect("the next key is published");
    assert_eq!(kids(&jwks).len(), 2);
    let waits: i64 = api.db.read(|c| c.query_row("SELECT signs_from FROM oidc_keys WHERE kid = ?", [next.as_str()], |r| r.get(0))).unwrap();
    assert!(waits >= now + PUBLISH_AHEAD_MS, "it signs a day or more after it's published");
    assert_eq!(roll_token(&jwks, &issue().await).0["kid"], json!(old), "the old key signs until then");
    // The operator's rotate-now answers the key already waiting, not another.
    let r = api.ok(Method::POST, "/v1/secrets/issuer/rotate", None).await;
    assert_eq!(r["kid"], json!(next));
    // Its time comes: it signs, and the old key still verifies its tokens.
    at(&next, now - 1000);
    let jwks = api.ok(Method::GET, "/v1/secrets/oidc/jwks", None).await;
    assert_eq!(roll_token(&jwks, &issue().await).0["kid"], json!(next));
    assert!(roll_token(&jwks, &old_token).2, "an old token verifies until it expires");
    sweep().await;
    assert_eq!(kids(&api.ok(Method::GET, "/v1/secrets/oidc/jwks", None).await).len(), 2);
    // Once the old key's last token has expired, it leaves the JWKS; the log still verifies.
    at(&next, now - RETIRE_AFTER_MS - 1000);
    sweep().await;
    assert_eq!(kids(&api.ok(Method::GET, "/v1/secrets/oidc/jwks", None).await), [next.as_str()]);
    let v = api.ok(Method::GET, "/audit/verify", None).await;
    assert_eq!(v["ok"], true, "{v}");
}

/// A leaked key is pulled at once: a new key signs now and the old one leaves the JWKS, so its
/// tokens stop verifying; health names it, and the project's log has the rotation.
#[tokio::test]
async fn an_emergency_rotation_signs_with_a_new_key_now_and_pulls_the_old_one() {
    let api = Api::new().await;
    let issue = || {
        let (s, p) = (api.secrets.clone(), api.project.clone());
        async move { tokio::task::spawn_blocking(move || s.issue(app(&p, "svc_web", "production"), "roll", std::time::Duration::from_secs(3600)).unwrap()).await.unwrap() }
    };
    let jwks = || api.ok(Method::GET, "/v1/secrets/oidc/jwks", None);
    let old_token = issue().await;
    let old = roll_token(&jwks().await, &old_token).0["kid"].as_str().unwrap().to_string();
    // Half an emergency is refused, and so is a key that isn't in the JWKS.
    let rotate = "/v1/secrets/issuer/rotate";
    for (body, status) in [(json!({ "now": true }), StatusCode::BAD_REQUEST), (json!({ "retire": old }), StatusCode::BAD_REQUEST), (json!({ "now": true, "retire": "k_nope" }), StatusCode::NOT_FOUND)] {
        assert_eq!(api.call(Method::POST, rotate, Some(body)).await.0, status);
    }
    let r = api.ok(Method::POST, rotate, Some(json!({ "now": true, "retire": old }))).await;
    let new = r["kid"].as_str().unwrap().to_string();
    assert_ne!(new, old);
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap();
    let jwks = jwks().await;
    assert_eq!(kids(&jwks), [new.as_str()], "the pulled key is out of the JWKS at once");
    assert!(!roll_token(&jwks, &old_token).2, "a token the pulled key signed no longer verifies");
    let (header, _, signed) = roll_token(&jwks, &issue().await);
    assert!(signed && header["kid"] == json!(new), "{header}");
    let health = api.ok(Method::GET, "/v1/secrets/health", None).await;
    assert_eq!(health["issuer_pulled"][0]["kid"], json!(old), "{health}");
    let audit = api.ok(Method::GET, "/audit", None).await;
    assert!(audit.as_array().unwrap().iter().any(|a| a["action"] == "issuer.rotate" && a["actor"] == "operator"), "{audit}");
}

#[tokio::test]
async fn an_apps_instance_logs_in_with_rolls_own_token_for_its_own_project_only() {
    let api = Api::new().await;
    api.set("production", "API_KEY", "prod-key").await;
    let other = api.ok(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "elsewhere" }))).await["id"].as_str().unwrap().to_string();
    let identity = |subject: String| json!({ "name": "web", "issuer": ISSUER_URL, "audience": "roll", "subject": subject, "environments": ["production"], "actions": ["read", "reveal"] });
    // roll signs for every project: an identity of its own issuer names one of this project's.
    for subject in [format!("roll-apps:{other}:svc_web:production"), format!("roll-apps:{}:svc_*:production", api.project), format!("roll-secrets:{}", api.project)] {
        assert_eq!(api.call(Method::POST, "/identities", Some(identity(subject))).await.0, StatusCode::BAD_REQUEST);
    }
    let i = api.ok(Method::POST, "/identities", Some(identity(format!("roll-apps:{}:svc_web:production", api.project)))).await;
    let token = |project: String| {
        let s = api.secrets.clone();
        async move { tokio::task::spawn_blocking(move || s.issue(app(&project, "svc_web", "production"), "roll", std::time::Duration::from_secs(600)).unwrap()).await.unwrap() }
    };
    // Checked against roll's own keys in process: the fake fetcher has no keys at roll's URL.
    let login = |t: String| api.call(Method::POST, "/v1/secrets/login", Some(json!({ "identity": i["id"], "token": t })));
    let (s, l) = login(token(api.project.clone()).await).await;
    assert_eq!(s, StatusCode::OK, "{l}");
    let (s, v) = api.req(Method::GET, "/environments/production/values", None, l["token"].as_str()).await;
    assert_eq!((s, v["values"]["API_KEY"].as_str()), (StatusCode::OK, Some("prod-key")));
    assert_eq!(login(token(other).await).await.0, StatusCode::FORBIDDEN, "another project's instance");
}

/// A key bound to hosts (docs/secrets.md, "Using a key without seeing it"): an untrusted
/// sandbox's resolve gets a placeholder for it, sensitive or not, and never the value; keys
/// without hosts read as they always did.
#[tokio::test]
async fn an_untrusted_resolve_of_a_host_bound_key_gets_a_placeholder_never_the_value() {
    use roll_secrets::{ResourceRef, proxy};
    let api = Api::new().await;
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY", Some(json!({ "value": "sk_test_fake_bound_1", "tags": ["sensitive"] }))).await;
    api.ok(Method::PUT, "/environments/staging/secrets/OTHER_SENSITIVE", Some(json!({ "value": "fake-unbound-sensitive", "tags": ["sensitive"] }))).await;
    api.set("staging", "PLAIN_KEY", "fake-plain").await;
    // Binding is an admin's: a writer's token can't.
    let writer = api.token(json!({ "name": "w", "environments": ["staging"], "actions": ["read", "reveal", "write"] }));
    // A sensitive key's placeholder reaches untrusted code only by the binding's opt-in.
    let bind = json!({ "hosts": ["api.stripe.com"], "header": "Authorization: Bearer {}", "untrusted": true });
    assert_eq!(api.req(Method::PUT, "/environments/staging/secrets/STRIPE_KEY/hosts", Some(bind.clone()), Some(&writer)).await.0, StatusCode::FORBIDDEN);
    assert_eq!(api.call(Method::PUT, "/environments/staging/secrets/STRIPE_KEY/hosts", Some(json!({ "hosts": ["*"] }))).await.0, StatusCode::BAD_REQUEST);
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY/hosts", Some(bind)).await;
    assert_eq!(api.ok(Method::GET, "/environments/staging/secrets/STRIPE_KEY", None).await["hosts"]["hosts"], json!(["api.stripe.com"]));

    let r = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "sandbox:sbx_1", "untrusted": true, "placeholders": true }))).await;
    let ph = r["values"]["STRIPE_KEY"].as_str().unwrap().to_string();
    assert!(ph.starts_with("rlph_") && roll_auth::credentials::checksum_ok(&ph), "{r}");
    assert_eq!((r["placeholders"][0]["key"].as_str(), r["placeholders"][0]["hosts"].clone()), (Some("STRIPE_KEY"), json!(["api.stripe.com"])), "{r}");
    // Keys without hosts are unchanged: the plain one's value, the sensitive one withheld.
    assert_eq!((r["values"]["PLAIN_KEY"].as_str(), r["withheld"].clone()), (Some("fake-plain"), json!(["OTHER_SENSITIVE"])), "{r}");
    // Without asking, the bound sensitive key is withheld as before.
    let plain = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "sandbox:sbx_1", "untrusted": true }))).await;
    assert!(plain["values"].get("STRIPE_KEY").is_none() && plain.get("placeholders").is_none(), "{plain}");
    let audit = api.ok(Method::GET, "/audit", None).await;
    assert!(!r.to_string().contains("sk_test_fake_bound") && !audit.to_string().contains("sk_test_fake_bound") && !audit.to_string().contains(&ph), "{audit}");
    assert!(audit.as_array().unwrap().iter().any(|a| a["summary"].as_str().is_some_and(|s| s.contains("1 as placeholders"))), "{audit}");
    assert_eq!(api.ok(Method::GET, "/placeholders", None).await[0]["consumer"], "sandbox:sbx_1");

    // A product's resource asking for placeholders gets one too, as an untrusted copy.
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{}/resources/bucket/photos", api.project), Some(json!({ "x": 0, "y": 0 }))).await;
    let branch = ResourceRef::new(roll_workspace::Kind::Bucket, "photos").untrusted().with_placeholders();
    let got = api.secrets.resolve_for(&branch, "roll://staging/STRIPE_KEY").unwrap();
    assert!(got.value.expose().starts_with("rlph_") && got.placeholder.is_some());
    let header = format!("Bearer {}", got.value.expose());
    let at = proxy::Position { place: proxy::Place::Header, name: "authorization", value: &header };
    assert_eq!(api.secrets.substitute(got.value.expose(), &branch.consumer(), "api.stripe.com", at).unwrap().value.expose(), "sk_test_fake_bound_1");
}

/// Only roll's proxy swaps a placeholder, only for a bound host and place: another host is
/// refused with an alert (likely an exfiltration attempt), once a minute with a count.
#[tokio::test]
async fn a_placeholder_is_swapped_for_a_bound_host_and_refused_with_an_alert_for_another() {
    use roll_secrets::proxy::{self, Blocked, Outgoing, Place, Position, Refusal};
    let api = Api::new().await;
    api.ok(Method::PUT, "/environments/staging/policy", Some(json!({ "protected": false, "notify_url": "https://hooks.example/T0/B0/x" }))).await;
    api.set("staging", "STRIPE_KEY", "sk_test_fake_swap_1").await;
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY/hosts", Some(json!({ "hosts": ["*.stripe.com"], "header": "Authorization: Bearer {}" }))).await;
    let r = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "sandbox:sbx_2", "untrusted": true, "placeholders": true }))).await;
    let ph = r["values"]["STRIPE_KEY"].as_str().unwrap().to_string();

    // Through the filter a proxy embeds: swapped in the header, for the bound host.
    let mut headers = vec![("Authorization".to_string(), format!("Bearer {ph}"))];
    let mut req = Outgoing { host: "api.stripe.com:443", path: "/v1/charges", headers: &mut headers, query: None };
    assert_eq!(proxy::filter(&mut req, "sandbox:sbx_2", &*api.secrets, &proxy::Limits::default()).unwrap().swapped(), 1);
    assert_eq!(headers[0].1, "Bearer sk_test_fake_swap_1");
    // A rotation needs no new placeholder.
    api.set("staging", "STRIPE_KEY", "sk_test_fake_swap_2").await;
    let bearer = format!("Bearer {ph}");
    let header = Position { place: Place::Header, name: "authorization", value: &bearer };
    assert_eq!(api.secrets.substitute(&ph, "sandbox:sbx_2", "files.stripe.com", header).unwrap().value.expose(), "sk_test_fake_swap_2");
    // Never in the query (the binding says a header), never to another host.
    let query = Position { place: Place::Query, name: "key", value: &ph };
    assert!(matches!(api.secrets.substitute(&ph, "sandbox:sbx_2", "api.stripe.com", query), Err(Refusal::WrongPlace(_))));
    for _ in 0..3 {
        let mut headers = vec![("Authorization".to_string(), format!("Bearer {ph}"))];
        let mut req = Outgoing { host: "evil.example", path: "/", headers: &mut headers, query: None };
        let refused = proxy::filter(&mut req, "sandbox:sbx_2", &*api.secrets, &proxy::Limits::default()).unwrap_err();
        assert_eq!(refused, Blocked::Refused { at: "header Authorization".into(), why: Refusal::WrongHost("evil.example".into()) });
        assert_eq!(headers[0].1, format!("Bearer {ph}"), "the placeholder stays, and the request isn't sent");
    }
    // One alert for the three (once a minute), in the audit log and to the notify URL.
    let alerts = api.ok(Method::GET, "/audit?action=secrets.alert", None).await;
    assert_eq!(alerts.as_array().unwrap().len(), 1, "{alerts}");
    let text = alerts[0]["summary"].as_str().unwrap();
    assert!(text.contains("evil.example") && text.contains("likely an exfiltration attempt") && !text.contains("sk_test_fake") && !text.contains(&ph), "{text}");
    let notices: i64 = api.db.read(|c| c.query_row("SELECT COUNT(*) FROM notices WHERE body LIKE '%placeholder_host%'", (), |r| r.get(0))).unwrap();
    assert_eq!(notices, 1);
    // The swaps were audited once (the first), quietly, without the value.
    let used = api.ok(Method::GET, "/audit?action=secrets.substitute", None).await;
    assert_eq!(used.as_array().unwrap().len(), 2, "one row per (placeholder, host) a minute: {used}");
    assert!(used[0]["summary"].as_str().unwrap().contains("once for sandbox:sbx_2") && !used.to_string().contains("sk_test_fake"), "{used}");
}

/// A binding names where its placeholder goes: one without (bound before that was required) is
/// still a trusted reader's, in any header, but an untrusted consumer's doesn't swap, untrusted
/// bindings need one, and Health lists it.
#[tokio::test]
async fn a_binding_without_a_placement_refuses_an_untrusted_swap_and_is_in_health() {
    use roll_secrets::proxy::{Place, Position, Refusal};
    let api = Api::new().await;
    api.set("staging", "STRIPE_KEY", "sk_test_fake_unplaced").await;
    let hosts = "/environments/staging/secrets/STRIPE_KEY/hosts";
    let (s, e) = api.call(Method::PUT, hosts, Some(json!({ "hosts": ["api.stripe.com"], "untrusted": true }))).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::BAD_REQUEST, Some("placement_required")), "{e}");
    api.ok(Method::PUT, hosts, Some(json!({ "hosts": ["api.stripe.com"] }))).await;
    let health = api.ok(Method::GET, "/health", None).await;
    assert!(health.as_array().unwrap().iter().any(|f| f["issue"] == "unplaced" && f["key"] == "STRIPE_KEY"), "{health}");
    let placeholder = |untrusted: bool| {
        let api = &api;
        async move { api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "sandbox:sbx_u", "untrusted": untrusted, "placeholders": true }))).await["values"]["STRIPE_KEY"].as_str().unwrap().to_string() }
    };
    let ph = placeholder(true).await;
    let at = Position { place: Place::Header, name: "x-anything", value: &ph };
    assert_eq!(api.secrets.substitute(&ph, "sandbox:sbx_u", "api.stripe.com", at).unwrap_err(), Refusal::PlacementRequired);
    let ph = placeholder(false).await;
    let at = Position { place: Place::Header, name: "x-anything", value: &ph };
    assert_eq!(api.secrets.substitute(&ph, "sandbox:sbx_u", "api.stripe.com", at).unwrap().value.expose(), "sk_test_fake_unplaced");
}

/// The filter swaps a placeholder only where its binding says: anywhere else the request is
/// blocked, so the code holding it can't pick a header the host echoes.
#[tokio::test]
async fn a_placeholder_outside_its_placement_is_blocked() {
    use roll_secrets::proxy::{self, Blocked, Outgoing, Refusal};
    let api = Api::new().await;
    api.set("staging", "STRIPE_KEY", "sk_test_fake_placed").await;
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY/hosts", Some(json!({ "hosts": ["api.stripe.com"], "header": "Authorization: Bearer {}" }))).await;
    let r = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "sandbox:sbx_p", "untrusted": true, "placeholders": true }))).await;
    let ph = r["values"]["STRIPE_KEY"].as_str().unwrap().to_string();
    let mut headers = vec![("Authorization".to_string(), format!("Bearer {ph}")), ("X-Request-Id".to_string(), ph.clone())];
    let before = headers.clone();
    let mut req = Outgoing { host: "api.stripe.com", path: "/v1/charges", headers: &mut headers, query: None };
    match proxy::filter(&mut req, "sandbox:sbx_p", &*api.secrets, &proxy::Limits::default()) {
        Err(Blocked::Refused { at, why: Refusal::WrongPlace(_) }) => assert_eq!(at, "header X-Request-Id"),
        other => panic!("{other:?}"),
    }
    assert_eq!(headers, before, "nothing swapped, nothing sent");
}

/// A response that echoes a swapped value back (in a header, split across two reads of the body,
/// base64-encoded) is caught by the request's scrub, and reporting it ends the placeholder with an
/// alert: the consumer never sees the value.
#[tokio::test]
async fn an_echoed_value_is_caught_and_ends_the_placeholder() {
    use base64::Engine;
    use roll_secrets::proxy::{self, Outgoing, Place, Position, Refusal};
    let api = Api::new().await;
    api.ok(Method::PUT, "/environments/staging/policy", Some(json!({ "protected": false, "notify_url": "https://hooks.example/T0/B0/x" }))).await;
    let value = "sk_test_fake_echo_value_1";
    api.set("staging", "STRIPE_KEY", value).await;
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY/hosts", Some(json!({ "hosts": ["api.stripe.com"], "header": "Authorization: Bearer {}" }))).await;
    let r = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "sandbox:sbx_e", "untrusted": true, "placeholders": true }))).await;
    let ph = r["values"]["STRIPE_KEY"].as_str().unwrap().to_string();
    let scrub = || {
        let mut headers = vec![("Authorization".to_string(), format!("Bearer {ph}")), ("Accept-Encoding".to_string(), "gzip".to_string())];
        let mut req = Outgoing { host: "api.stripe.com", path: "/v1/charges", headers: &mut headers, query: None };
        let scrub = proxy::filter(&mut req, "sandbox:sbx_e", &*api.secrets, &proxy::Limits::default()).unwrap();
        assert_eq!(headers[1..], [("accept-encoding".to_string(), "identity".to_string())]);
        scrub
    };
    // A response that doesn't echo it goes through.
    let mut clean = scrub();
    let mut h = axum::http::HeaderMap::new();
    h.insert("content-type", "application/json".parse().unwrap());
    assert!(clean.headers(&h).is_ok() && clean.chunk(b"{\"id\":\"ch_1\"}").is_ok());
    // In a header.
    h.insert("x-echo", format!("Bearer {value}").parse().unwrap());
    let echoed = scrub().headers(&h).unwrap_err();
    assert_eq!((echoed.key.as_str(), echoed.host.as_str()), ("STRIPE_KEY", "api.stripe.com"));
    assert_eq!(Some(echoed.placeholder.as_str()), r["placeholders"][0]["id"].as_str());
    // Split across two reads of the body.
    let mut split = scrub();
    assert!(split.chunk(br#"{"auth":"Bearer sk_test_fake_ec"#).is_ok());
    assert_eq!(split.chunk(br#"ho_value_1"}"#).unwrap_err(), echoed);
    // Base64-encoded.
    let b64 = base64::engine::general_purpose::STANDARD.encode(value);
    assert_eq!(scrub().chunk(format!("debug={b64}").as_bytes()).unwrap_err(), echoed);
    assert!(!format!("{echoed} {echoed:?} {split:?}").contains("sk_test_fake"));

    let ca = api.secrets.intercept_ca("sandbox:sbx_e", None).unwrap();
    api.secrets.echoed(&echoed.placeholder, &echoed.host).unwrap();
    assert!(!api.secrets.intercept_ca_live(&ca.id).unwrap(), "its CA ends with the host it no longer has");
    let at = Position { place: Place::Header, name: "authorization", value: &format!("Bearer {ph}") };
    assert_eq!(api.secrets.substitute(&ph, "sandbox:sbx_e", "api.stripe.com", at).unwrap_err(), Refusal::Revoked);
    let alerts = api.ok(Method::GET, "/audit?action=secrets.alert", None).await;
    let text = alerts[0]["summary"].as_str().unwrap();
    assert!(text.contains("api.stripe.com echoed STRIPE_KEY") && !alerts.to_string().contains("sk_test_fake") && !alerts.to_string().contains(&ph), "{alerts}");
    let notices: i64 = api.db.read(|c| c.query_row("SELECT COUNT(*) FROM notices WHERE body LIKE '%placeholder_echo%'", (), |r| r.get(0))).unwrap();
    assert_eq!(notices, 1);
}

/// A placeholder works only while it's live and whoever asked for it still may: expired, revoked,
/// ended with its consumer, its token gone or its binding off, it's refused.
#[tokio::test]
async fn an_expired_or_revoked_placeholder_is_refused() {
    use roll_secrets::proxy::{Place, Position, Refusal};
    let api = Api::new().await;
    api.set("staging", "STRIPE_KEY", "sk_test_fake_end_1").await;
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY/hosts", Some(json!({ "hosts": ["api.stripe.com"], "query": "key" }))).await;
    let issue = |token: Option<String>, consumer: &'static str| {
        let api = &api;
        async move {
            let (s, r) = api.req(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": consumer, "untrusted": true, "placeholders": true })), token.as_deref()).await;
            assert!(s.is_success(), "{s} {r}");
            (r["values"]["STRIPE_KEY"].as_str().unwrap().to_string(), r["placeholders"][0]["id"].as_str().unwrap().to_string())
        }
    };
    let swap = |ph: &str, consumer: &str| api.secrets.substitute(ph, consumer, "api.stripe.com", Position { place: Place::Query, name: "key", value: ph });

    let (ph, _) = issue(None, "sandbox:sbx_3").await;
    assert_eq!(swap(&ph, "sandbox:sbx_3").unwrap().value.expose(), "sk_test_fake_end_1");
    api.db.write(|c| c.execute("UPDATE placeholders SET expires_at = 1", ())).unwrap();
    assert_eq!(swap(&ph, "sandbox:sbx_3").unwrap_err(), Refusal::Expired);

    // Ending the consumer (its sandbox stopped) ends its placeholders; revoking one ends it.
    let (a, _) = issue(None, "sandbox:sbx_4").await;
    let (b, b_id) = issue(None, "sandbox:sbx_5").await;
    assert_eq!(api.secrets.end_consumer("sandbox:sbx_4").unwrap(), 1);
    assert_eq!(swap(&a, "sandbox:sbx_4").unwrap_err(), Refusal::Revoked);
    assert_eq!(api.call(Method::DELETE, &format!("/placeholders/{b_id}"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(swap(&b, "sandbox:sbx_5").unwrap_err(), Refusal::Revoked);
    assert_eq!(swap(&roll_auth::credentials::new_secret("rlph_"), "sandbox:sbx_3").unwrap_err(), Refusal::Unknown);

    // The token that asked for it is gone: refused at the next swap, nothing cached.
    let rule = roll_secrets::access::rule(&api.project, Some(vec!["staging".into()]), None, &[roll_secrets::Action::Read, roll_secrets::Action::Reveal]);
    let (secret, id) = api.gate.grant("sbx", rule, None);
    let (c, _) = issue(Some(secret), "sandbox:sbx_6").await;
    assert!(swap(&c, "sandbox:sbx_6").is_ok());
    roll_secrets::Workloads::revoke(&*api.gate, &id).unwrap();
    assert!(matches!(swap(&c, "sandbox:sbx_6"), Err(Refusal::NotAllowed(_))), "{:?}", swap(&c, "sandbox:sbx_6"));
    // The binding taken off: no placeholder of the key works.
    let (d, _) = issue(None, "sandbox:sbx_7").await;
    api.ok(Method::DELETE, "/environments/staging/secrets/STRIPE_KEY/hosts", None).await;
    assert!(matches!(swap(&d, "sandbox:sbx_7"), Err(Refusal::Unusable(_))));
}

/// A wildcard binding never covers names anyone can register (the Public Suffix List): a
/// prompt-injected sandbox could otherwise send the key to its own subdomain.
#[tokio::test]
async fn a_wildcard_under_a_shared_suffix_is_refused() {
    let api = Api::new().await;
    api.set("staging", "SUPABASE_KEY", "fake-supabase-key").await;
    for shared in ["*.supabase.co", "*.vercel.app", "*.workers.dev", "*.co.uk"] {
        let (s, e) = api.call(Method::PUT, "/environments/staging/secrets/SUPABASE_KEY/hosts", Some(json!({ "hosts": [shared] }))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{shared}: {e}");
        assert!(e["error"].as_str().unwrap().contains("Public Suffix List"), "{e}");
    }
    // The exact host there is fine.
    api.ok(Method::PUT, "/environments/staging/secrets/SUPABASE_KEY/hosts", Some(json!({ "hosts": ["myproject.supabase.co"] }))).await;
}

/// Without the binding's opt-in, an untrusted consumer gets no placeholder for a sensitive key
/// (withheld, as before placeholders); the opt-in is an admin's and audited, and taking it off
/// stops the swaps already handed out.
#[tokio::test]
async fn a_sensitive_key_without_the_opt_in_gives_untrusted_code_no_placeholder() {
    use roll_secrets::proxy::{Place, Position, Refusal};
    let api = Api::new().await;
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY", Some(json!({ "value": "sk_test_fake_optin", "tags": ["sensitive"] }))).await;
    let bind = |untrusted: bool| json!({ "hosts": ["api.stripe.com"], "header": "X-Api-Key: {}", "untrusted": untrusted });
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY/hosts", Some(bind(false))).await;
    let resolve = || api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "preview:fork-1", "untrusted": true, "placeholders": true })));
    let r = resolve().await;
    assert!(r["values"].get("STRIPE_KEY").is_none() && r.get("placeholders").is_none() && r["withheld"] == json!(["STRIPE_KEY"]), "{r}");
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY/hosts", Some(bind(true))).await;
    let audit = api.ok(Method::GET, "/audit?action=secrets.hosts", None).await;
    assert!(audit[0]["summary"].as_str().unwrap().contains("untrusted consumers too"), "{audit}");
    let ph = resolve().await["values"]["STRIPE_KEY"].as_str().unwrap().to_string();
    let at = Position { place: Place::Header, name: "x-api-key", value: &ph };
    assert_eq!(api.secrets.substitute(&ph, "preview:fork-1", "api.stripe.com", at).unwrap().value.expose(), "sk_test_fake_optin");
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY/hosts", Some(bind(false))).await;
    assert!(matches!(api.secrets.substitute(&ph, "preview:fork-1", "api.stripe.com", at), Err(Refusal::NotAllowed(_))));
}

/// An in-process consumer (a sandbox) names who made it, and its placeholders swap only while
/// they may still reveal there: checked again on every swap.
#[tokio::test]
async fn the_maker_losing_reveal_stops_the_swaps() {
    use roll_secrets::placeholders::Ask;
    use roll_secrets::proxy::{Place, Position, Refusal};
    let api = Api::new().await;
    api.set("staging", "STRIPE_KEY", "sk_test_fake_maker").await;
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY/hosts", Some(json!({ "hosts": ["api.stripe.com"], "header": "Authorization: {}" }))).await;
    let reveal = roll_secrets::access::rule(&api.project, Some(vec!["staging".into()]), None, &[roll_secrets::Action::Read, roll_secrets::Action::Reveal]);
    let (_, id) = api.gate.grant("ada's", reveal, None);
    let read_only = roll_secrets::access::rule(&api.project, Some(vec!["staging".into()]), None, &[roll_secrets::Action::Read]);
    let (_, reader) = api.gate.grant("reader", read_only, None);
    let sandbox = api.secrets.untrusted(&api.project, &roll_secrets::Caller::consumer("sandbox:sbx_m")).unwrap();
    let issue = |maker: Option<&str>| api.secrets.values_with_placeholders(&api.project, "staging", "/", "sandbox:sbx_m", false, Ask { minutes: None, maker }, &sandbox);
    // Nobody named, or someone who may not reveal there: refused, the latter with its own code (a
    // preview resolves without placeholders on it, and surfaces any other 403).
    assert!(matches!(issue(None), Err(roll_secrets::Error::BadRequest(_))));
    assert!(matches!(issue(Some(&format!("token:{reader}"))), Err(roll_secrets::Error::MakerUnreachable(_))));
    let (status, refused) = api.call(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "sandbox:sbx_m", "placeholders": true, "maker": format!("token:{reader}") }))).await;
    assert_eq!((status, refused["code"].as_str()), (StatusCode::FORBIDDEN, Some("maker_unreachable")), "{refused}");
    assert!(refused["error"].as_str().unwrap().contains("can no longer reveal staging") && !refused.to_string().contains("sk_test_fake_maker"), "{refused}");
    let maker = format!("token:{id}");
    let v = issue(Some(&maker)).unwrap();
    let ph = v.values["STRIPE_KEY"].expose().to_string();
    let at = Position { place: Place::Header, name: "authorization", value: &ph };
    assert_eq!(api.secrets.substitute(&ph, "sandbox:sbx_m", "api.stripe.com", at).unwrap().value.expose(), "sk_test_fake_maker");
    assert_eq!(api.ok(Method::GET, "/placeholders", None).await[0]["maker"], maker.as_str());
    roll_secrets::Workloads::revoke(&*api.gate, &id).unwrap();
    assert!(matches!(api.secrets.substitute(&ph, "sandbox:sbx_m", "api.stripe.com", at), Err(Refusal::MakerUnreachable(_))));
    assert!(matches!(issue(Some(&maker)), Err(roll_secrets::Error::MakerUnreachable(_))));
}

/// Reach lists a folder's live placeholders, and a cut-off ends them, without a freeze.
#[tokio::test]
async fn a_cut_off_ends_the_folders_placeholders() {
    use roll_secrets::proxy::{Place, Position, Refusal};
    let api = Api::new().await;
    // An untrusted preview reads its base only where that isn't protected.
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": false }))).await;
    api.set("production", "STRIPE_KEY", "sk_test_fake_cut").await;
    api.ok(Method::PUT, "/environments/production/secrets/STRIPE_KEY/hosts", Some(json!({ "hosts": ["api.stripe.com"] }))).await;
    // Resolved through an environment based on production: still production's key's.
    api.ok(Method::POST, "/environments", Some(json!({ "slug": "pr-9", "base": "production" }))).await;
    let r = api.ok(Method::POST, "/environments/pr-9/resolve", Some(json!({ "consumer": "preview:pr-9", "untrusted": true, "placeholders": true }))).await;
    let ph = r["values"]["STRIPE_KEY"].as_str().unwrap().to_string();
    let reach = api.ok(Method::GET, "/environments/production/reach?key=STRIPE_KEY", None).await;
    let listed = &reach["placeholders"][0];
    assert_eq!((listed["key"].as_str(), listed["consumer"].as_str(), listed["through"].as_str()), (Some("STRIPE_KEY"), Some("preview:pr-9"), Some("pr-9:/")), "{reach}");
    let cut = cut_off(&api, "path=/&key=STRIPE_KEY", json!({ "why": "fake incident" })).await;
    assert_eq!((cut["placeholders"].as_array().unwrap().len(), cut["frozen"].as_bool()), (1, Some(false)), "{cut}");
    let at = Position { place: Place::Header, name: "authorization", value: &ph };
    assert_eq!(api.secrets.substitute(&ph, "preview:pr-9", "api.stripe.com", at).unwrap_err(), Refusal::Revoked);
    assert!(api.ok(Method::GET, "/environments/production/reach", None).await["placeholders"].as_array().unwrap().is_empty());
}

/// A loop over many wrong hosts is one alert a minute per placeholder, not one per host.
#[tokio::test]
async fn the_wrong_host_alert_is_limited_per_placeholder() {
    use roll_secrets::proxy::{Place, Position, Refusal};
    let api = Api::new().await;
    api.ok(Method::PUT, "/environments/staging/policy", Some(json!({ "protected": false, "notify_url": "https://hooks.example/T0/B0/x" }))).await;
    api.set("staging", "STRIPE_KEY", "sk_test_fake_flood").await;
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY/hosts", Some(json!({ "hosts": ["api.stripe.com"] }))).await;
    let r = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "sandbox:sbx_f", "untrusted": true, "placeholders": true }))).await;
    let ph = r["values"]["STRIPE_KEY"].as_str().unwrap().to_string();
    let at = Position { place: Place::Header, name: "authorization", value: &ph };
    for i in 0..20 {
        let host = format!("x{i}.evil.example");
        assert_eq!(api.secrets.substitute(&ph, "sandbox:sbx_f", &host, at).unwrap_err(), Refusal::WrongHost(host));
    }
    let alerts = api.ok(Method::GET, "/audit?action=secrets.alert", None).await;
    assert_eq!(alerts.as_array().unwrap().len(), 1, "{alerts}");
    assert!(alerts[0]["summary"].as_str().unwrap().contains("x0.evil.example"), "{alerts}");
    let notices: i64 = api.db.read(|c| c.query_row("SELECT COUNT(*) FROM notices WHERE body LIKE '%placeholder_host%'", (), |r| r.get(0))).unwrap();
    assert_eq!(notices, 1);
}

/// A placeholder swaps only for the consumer it was issued to: one leaked from a sandbox and sent
/// through another consumer's proxy (another project's closed environment, a fork's preview, with
/// the same host bound) is refused with an alert, once a minute, and its own still swaps.
#[tokio::test]
async fn a_placeholder_swapped_by_another_consumers_proxy_is_refused_with_an_alert() {
    use roll_secrets::proxy::{self, Blocked, Outgoing, Refusal};
    let api = Api::new().await;
    api.ok(Method::PUT, "/environments/staging/policy", Some(json!({ "protected": false, "notify_url": "https://hooks.example/T0/B0/x" }))).await;
    api.set("staging", "STRIPE_KEY", "sk_test_fake_theft").await;
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY/hosts", Some(json!({ "hosts": ["api.stripe.com"], "header": "Authorization: Bearer {}" }))).await;
    let r = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "sandbox:sbx_a", "untrusted": true, "placeholders": true }))).await;
    let ph = r["values"]["STRIPE_KEY"].as_str().unwrap().to_string();
    let send = |consumer: &str| {
        let mut headers = vec![("Authorization".to_string(), format!("Bearer {ph}"))];
        let mut req = Outgoing { host: "api.stripe.com", path: "/v1/charges", headers: &mut headers, query: None };
        let got = proxy::filter(&mut req, consumer, &*api.secrets, &proxy::Limits::default()).map(|s| s.swapped());
        (got, headers[0].1.clone())
    };
    for _ in 0..3 {
        let (got, header) = send("apps:prj_other/production");
        assert_eq!(got, Err(Blocked::Refused { at: "header Authorization".into(), why: Refusal::WrongConsumer }));
        assert_eq!(header, format!("Bearer {ph}"), "nothing swapped, nothing sent");
    }
    let alerts = api.ok(Method::GET, "/audit?action=secrets.alert", None).await;
    assert_eq!(alerts.as_array().unwrap().len(), 1, "{alerts}");
    let text = alerts[0]["summary"].as_str().unwrap();
    assert!(text.contains("issued to sandbox:sbx_a") && text.contains("apps:prj_other/production") && text.contains("likely a leaked placeholder"), "{text}");
    assert!(!alerts.to_string().contains("sk_test_fake") && !alerts.to_string().contains(&ph), "{alerts}");
    let notices: i64 = api.db.read(|c| c.query_row("SELECT COUNT(*) FROM notices WHERE body LIKE '%placeholder_consumer%'", (), |r| r.get(0))).unwrap();
    assert_eq!(notices, 1);
    // Its own consumer's proxy still swaps it.
    assert_eq!(send("sandbox:sbx_a"), (Ok(1), "Bearer sk_test_fake_theft".to_string()));
}

/// A placeholder stands only for a literal value: a key holding a reference can't be bound, gets
/// none if it comes to hold one, and a swap refuses it, so a writer can't point a bound key at
/// another key's value. The other way round, a key referencing a bound key is left out of any
/// read that never gives a bound key's value (placeholders, or bound keys withheld).
#[tokio::test]
async fn a_key_holding_a_reference_gets_no_placeholder() {
    use roll_secrets::ResourceRef;
    use roll_secrets::proxy::{Place, Position, Refusal};
    let api = Api::new().await;
    api.ok(Method::PUT, "/environments/staging/secrets/OTHER_SENSITIVE", Some(json!({ "value": "fake-other-sensitive", "tags": ["sensitive"] }))).await;
    api.set("staging", "STRIPE_KEY", "${OTHER_SENSITIVE}").await;
    let (s, e) = api.call(Method::PUT, "/environments/staging/secrets/STRIPE_KEY/hosts", Some(json!({ "hosts": ["api.stripe.com"] }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    api.set("staging", "STRIPE_KEY", "sk_test_fake_literal").await;
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY/hosts", Some(json!({ "hosts": ["api.stripe.com"], "header": "Authorization: {}" }))).await;
    let resolve = || api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "sandbox:sbx_r", "untrusted": true, "placeholders": true })));
    let ph = resolve().await["values"]["STRIPE_KEY"].as_str().unwrap().to_string();
    // A writer points it at the other key: no new placeholder, and the old one doesn't swap.
    api.set("staging", "STRIPE_KEY", "${OTHER_SENSITIVE}").await;
    let r = resolve().await;
    assert!(r["values"].get("STRIPE_KEY").is_none() && r.get("placeholders").is_none(), "{r}");
    assert!(r["problems"].to_string().contains("reference"), "{r}");
    let at = Position { place: Place::Header, name: "authorization", value: &ph };
    assert!(matches!(api.secrets.substitute(&ph, "sandbox:sbx_r", "api.stripe.com", at), Err(Refusal::Unusable(_))));
    assert!(!r.to_string().contains("fake-other-sensitive"));

    // A key that references the bound one never carries its value: not in a resolve for
    // placeholders, nor a resource's, nor a read that withholds bound keys (nor its audit row).
    api.set("staging", "STRIPE_KEY", "sk_test_fake_literal").await;
    api.set("staging", "AUTH", "Bearer ${STRIPE_KEY}").await;
    api.set("staging", "PLAIN", "fake-plain").await;
    let r = resolve().await;
    assert!(r["values"]["STRIPE_KEY"].as_str().unwrap().starts_with("rlph_") && r["values"].get("AUTH").is_none() && r["values"]["PLAIN"] == "fake-plain", "{r}");
    assert!(r["problems"].as_array().unwrap().iter().any(|p| p["key"] == "AUTH" && p["problem"].as_str().unwrap().contains("references STRIPE_KEY, which is bound to hosts")), "{r}");
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{}/resources/bucket/photos", api.project), Some(json!({ "x": 0, "y": 0 }))).await;
    let photos = ResourceRef::new(roll_workspace::Kind::Bucket, "photos").untrusted().with_placeholders();
    assert!(matches!(api.secrets.resolve_for(&photos, "roll://staging/AUTH"), Err(e) if e.to_string().contains("bound to hosts")));
    let caller = roll_secrets::Caller::consumer("app:svc_ref");
    let v = api.secrets.values_without_bound(&api.project, "staging", "/", "app:svc_ref", false, &caller).unwrap();
    assert!(!v.values.contains_key("STRIPE_KEY") && !v.values.contains_key("AUTH") && v.values.contains_key("PLAIN"), "{:?}", v.values.keys());
    let audit = api.ok(Method::GET, "/audit?action=secrets.resolve", None).await;
    let row = audit.as_array().unwrap().iter().find(|a| a["summary"].as_str().is_some_and(|s| s.contains("for app:svc_ref"))).unwrap();
    assert!(!row.to_string().contains("STRIPE_KEY") && !row.to_string().contains("\"AUTH\""), "{row}");
    assert!(!r.to_string().contains("sk_test_fake_literal") && !audit.to_string().contains("sk_test_fake_literal"));
}

impl Api {
    /// `handle` joins `project` as `role` (`environments` for a developer): ada invites them and they
    /// accept, as the dashboard does.
    async fn join(&self, project: &str, handle: &str, role: &str, environments: Option<Vec<&str>>) {
        let (s, inv) = self.as_ada(Method::POST, &format!("/v1/workspace/projects/{project}/invites"), Some(json!({ "email": format!("{handle}@example.com"), "role": role, "environments": environments }))).await;
        assert_eq!(s, StatusCode::CREATED, "{inv}");
        let token = inv["url"].as_str().unwrap().split("token=").nth(1).unwrap().to_string();
        let (s, m) = self.signed_in(handle, false, Method::POST, &format!("/v1/workspace/invites/{token}/accept"), None).await;
        assert_eq!(s, StatusCode::OK, "{m}");
    }
}

/// docs/members.md, "secrets by role", on a project as it's made (nothing protected by hand): a
/// viewer sees names, never a value; a developer reveals and writes where the role applies, never
/// production unless it's named (it starts protected with no approvals, so its owner still writes
/// directly); once it asks for an approval, a developer's write there is a change request; an admin
/// reviews it, and an approval stops counting once its approver is removed. A project they aren't
/// in doesn't exist for them.
#[tokio::test]
async fn members_reach_secrets_by_their_role() {
    let api = Api::new().await;
    let (_, p) = api.as_ada(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "team" }))).await;
    let team = p["id"].as_str().unwrap().to_string();
    let at = |rest: &str| format!("/v1/secrets/projects/{team}{rest}");
    for (env, value) in [("development", "dev"), ("production", "prod")] {
        let (s, e) = api.as_ada(Method::PUT, &at(&format!("/environments/{env}/secrets/KEY")), Some(json!({ "value": value }))).await;
        assert!(s.is_success(), "{e}");
    }
    let (s, e) = api.as_ada(Method::GET, &at("/environments/production"), None).await;
    assert_eq!((s, e["protected"].as_bool(), e["approvals"].as_i64()), (StatusCode::OK, Some(true), Some(0)), "{e}");
    api.join(&team, "bob", "developer", None).await;
    api.join(&team, "vic", "viewer", None).await;
    api.join(&team, "cat", "admin", None).await;
    let get = |who: &'static str, rest: String| {
        let api = &api;
        async move { api.signed_in(who, false, Method::GET, &rest, None).await }
    };

    // A viewer: names and history, never a value (D2).
    let (s, names) = get("vic", at("/environments/development/secrets")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(names.to_string().contains("KEY") && !names.to_string().contains("\"dev\""), "{names}");
    assert_eq!(get("vic", at("/environments/development/secrets/KEY/value")).await.0, StatusCode::FORBIDDEN);
    // A developer: every environment that isn't protected (D3), never production, and never deletes.
    let (s, v) = get("bob", at("/environments/development/secrets/KEY/value")).await;
    assert_eq!((s, v["value"].as_str()), (StatusCode::OK, Some("dev")), "{v}");
    assert_eq!(get("bob", at("/environments/production/secrets/KEY/value")).await.0, StatusCode::FORBIDDEN);
    assert_eq!(api.signed_in("bob", false, Method::PUT, &at("/environments/production/secrets/KEY"), Some(json!({ "value": "mine" }))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(api.signed_in("bob", false, Method::DELETE, &at("/environments/development/secrets/KEY"), None).await.0, StatusCode::FORBIDDEN);
    // Production once it's named; then only where it's named.
    let (s, e) = api.as_ada(Method::PATCH, &format!("/v1/workspace/projects/{team}/members/usr_bob"), Some(json!({ "environments": ["production"] }))).await;
    assert_eq!(s, StatusCode::OK, "{e}");
    let (s, v) = get("bob", at("/environments/production/secrets/KEY/value")).await;
    assert_eq!((s, v["value"].as_str()), (StatusCode::OK, Some("prod")), "{v}");
    assert_eq!(get("bob", at("/environments/development/secrets/KEY/value")).await.0, StatusCode::FORBIDDEN);
    // Once production asks for an approval, his write there is a change request, which an admin
    // approves.
    let (s, e) = api.as_ada(Method::PUT, &at("/environments/production/policy"), Some(json!({ "protected": true, "approvals": 1 }))).await;
    assert!(s.is_success(), "{e}");
    let (s, e) = api.signed_in("bob", false, Method::PUT, &at("/environments/production/secrets/KEY"), Some(json!({ "value": "new" }))).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("approval_required")), "{e}");
    let (s, r) = api.signed_in("bob", false, Method::POST, &at("/environments/production/requests"), Some(json!({ "ops": [{ "op": "set", "key": "KEY", "value": "new" }] }))).await;
    assert!(s.is_success(), "{r}");
    let id = r["id"].as_str().unwrap().to_string();
    assert_eq!(api.signed_in("vic", false, Method::POST, &at(&format!("/environments/production/requests/{id}/review")), Some(json!({ "verdict": "approve" }))).await.0, StatusCode::FORBIDDEN);
    let (s, r) = api.signed_in("cat", false, Method::POST, &at(&format!("/environments/production/requests/{id}/review")), Some(json!({ "verdict": "approve" }))).await;
    assert_eq!((s, r["state"].as_str()), (StatusCode::OK, Some("approved")), "{r}");
    // Cat leaves the project before it's applied: her approval no longer counts.
    assert_eq!(api.signed_in("cat", false, Method::DELETE, &format!("/v1/workspace/projects/{team}/members/me"), None).await.0, StatusCode::NO_CONTENT);
    let (s, e) = api.as_ada(Method::POST, &at(&format!("/environments/production/requests/{id}/apply")), None).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("approval_required")), "{e}");
    // A project bob isn't in is a 404 for him, as the workspace answers.
    assert_eq!(get("bob", format!("/v1/secrets/projects/{}/environments/development/secrets", api.project)).await.0, StatusCode::NOT_FOUND);
}

/// A project as it's made, nothing protected by hand (#688): a developer on the default role works
/// in development and staging but never reveals production, and a check (an untrusted resolve)
/// reads staging but is refused production, even through the operator.
#[tokio::test]
async fn a_fresh_projects_production_is_out_of_a_developers_and_a_checks_reach() {
    let api = Api::new().await;
    let (_, p) = api.as_ada(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "fresh" }))).await;
    let fresh = p["id"].as_str().unwrap().to_string();
    let at = |rest: &str| format!("/v1/secrets/projects/{fresh}{rest}");
    let (s, e) = api.as_ada(Method::PUT, &at("/environments/production/secrets/KEY"), Some(json!({ "value": "fake-prod-value" }))).await;
    assert!(s.is_success(), "{e}");
    api.join(&fresh, "bob", "developer", None).await;

    for env in ["development", "staging"] {
        let value = format!("fake-{env}-value");
        let (s, e) = api.signed_in("bob", false, Method::PUT, &at(&format!("/environments/{env}/secrets/KEY")), Some(json!({ "value": value }))).await;
        assert!(s.is_success(), "{env}: {e}");
        let (s, v) = api.signed_in("bob", false, Method::GET, &at(&format!("/environments/{env}/secrets/KEY/value")), None).await;
        assert_eq!((s, v["value"].as_str()), (StatusCode::OK, Some(value.as_str())), "{env}: {v}");
    }
    let (s, v) = api.signed_in("bob", false, Method::GET, &at("/environments/production/secrets/KEY/value"), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert!(!v.to_string().contains("fake-prod-value"));

    let check = |env: &str| {
        let (api, path) = (&api, at(&format!("/environments/{env}/resolve")));
        async move { api.call(Method::POST, &path, Some(json!({ "consumer": "check:fresh/web#1", "untrusted": true }))).await }
    };
    let (s, r) = check("staging").await;
    assert_eq!((s, r["values"]["KEY"].as_str()), (StatusCode::OK, Some("fake-staging-value")), "{r}");
    let (s, r) = check("production").await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{r}");
    assert!(!r.to_string().contains("fake-prod-value"));
}

/// A project whose production isn't protected (made before new ones started with it protected) is
/// never changed silently: Health says so and names what reaches it (owners and admins, a developer
/// who names it, a check that read it), a developer's default still leaves it out, and protecting it
/// as a new project's is (no approvals) clears the finding with writes still direct.
#[tokio::test]
async fn an_unprotected_production_is_a_health_finding_until_protected() {
    let api = Api::new().await;
    let (_, p) = api.as_ada(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "old" }))).await;
    let old = p["id"].as_str().unwrap().to_string();
    let at = |rest: &str| format!("/v1/secrets/projects/{old}{rest}");
    let (s, e) = api.as_ada(Method::PUT, &at("/environments/production/policy"), Some(json!({ "protected": false }))).await;
    assert!(s.is_success(), "{e}");
    api.as_ada(Method::PUT, &at("/environments/production/secrets/KEY"), Some(json!({ "value": "prod" }))).await;
    api.join(&old, "bob", "developer", None).await;
    api.join(&old, "cat", "developer", Some(vec!["production"])).await;
    assert_eq!(api.signed_in("bob", false, Method::GET, &at("/environments/production/secrets/KEY/value"), None).await.0, StatusCode::FORBIDDEN, "D3 without protection too");
    let caller = roll_secrets::Caller::consumer("check:acme/web#deploy");
    api.secrets.values(&old, "production", "/", true, Some("check:acme/web#deploy"), &caller).unwrap();
    let flagged = |found: &Value| found.as_array().unwrap().iter().find(|f| f["issue"] == "production_unprotected").map(|f| f["detail"].as_str().unwrap().to_string());
    let (_, found) = api.as_ada(Method::GET, &at("/health"), None).await;
    let detail = flagged(&found).expect("flagged");
    assert!(detail.contains("owners") && detail.contains("ada") && detail.contains("developers who name it") && detail.contains("cat") && !detail.contains("bob") && detail.contains("checks acme/web#deploy"), "{detail}");
    let (s, e) = api.as_ada(Method::PUT, &at("/environments/production/policy"), Some(json!({ "protected": true, "approvals": 0 }))).await;
    assert!(s.is_success(), "{e}");
    let (_, found) = api.as_ada(Method::GET, &at("/health"), None).await;
    assert_eq!(flagged(&found), None);
    let (s, e) = api.as_ada(Method::PUT, &at("/environments/production/secrets/KEY"), Some(json!({ "value": "new" }))).await;
    assert!(s.is_success(), "no approvals asked: a direct write {e}");
}

/// A database from before #688: an admin had protected staging with `approvals: 0`, which took
/// change requests then (0 counted as 1). Opening the store keeps that, once: 0 becomes 1, in the
/// audit log, and a write there is still a change request. Production as old projects had it
/// (unprotected) isn't touched.
#[tokio::test]
async fn an_upgrade_keeps_requests_where_approvals_were_zero() {
    let api = Api::new().await;
    // Its environments are made on first use.
    api.ok(Method::GET, "/environments", None).await;
    api.db
        .write(|c| {
            c.execute("DELETE FROM code_migrations WHERE name = 'secrets-protected-approvals-zero-kept'", ())?;
            c.execute("UPDATE environments SET protected = 1, approvals = 0 WHERE project_id = ? AND slug = 'staging'", [api.project.as_str()])?;
            c.execute("UPDATE environments SET protected = 0, approvals = 0 WHERE project_id = ? AND slug = 'production'", [api.project.as_str()])?;
            Ok::<_, code_sql::Error>(())
        })
        .unwrap();
    let opts = Options { keyring: Ok(Keyring::new(&"k".repeat(32), None).unwrap()), key_source: roll_secrets::KeySource::Env, links: Default::default(), syncs: Default::default(), open: true, trust_forwarded_for: false, web_url: None, deliver: None, grants: None, people: None, kms: None, sts: None, outside: None, directory: None, github: None, clouds: None, workloads: None, gateways: None };
    let upgraded = Arc::new(Secrets::new(api.db.clone(), api.ws.clone(), opts).unwrap());
    let send = |method: Method, rest: &str, body: Option<Value>| {
        let req = Request::builder().method(method).uri(format!("/v1/secrets/projects/{}{rest}", api.project)).header("content-type", "application/json").body(Body::from(body.map(|b| b.to_string()).unwrap_or_default())).unwrap();
        let router = roll_secrets::router(upgraded.clone());
        async move {
            let res = router.oneshot(req).await.unwrap();
            let status = res.status();
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null))
        }
    };
    let (_, staging) = send(Method::GET, "/environments/staging", None).await;
    assert_eq!((staging["protected"].as_bool(), staging["approvals"].as_i64()), (Some(true), Some(1)), "{staging}");
    let (s, e) = send(Method::PUT, "/environments/staging/secrets/KEY", Some(json!({ "value": "x" }))).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("approval_required")), "{e}");
    let (_, production) = send(Method::GET, "/environments/production", None).await;
    assert_eq!((production["protected"].as_bool(), production["approvals"].as_i64()), (Some(false), Some(0)), "{production}");
    let audit = upgraded.audit(&api.project, roll_secrets::AuditFilter::default(), &roll_secrets::Caller::operator()).unwrap();
    assert!(audit.iter().any(|a| a.environment.as_deref() == Some("staging") && a.summary == "approvals 0 → 1: kept requests as before (#688)"), "{audit:?}");
}

/// docs/members.md, "Remove, and leave": once a member is removed, their own token stops reaching
/// the project, a sync they made stops pushing, and their identity's logins are refused.
#[tokio::test]
async fn a_removed_members_token_sync_and_identity_stop() {
    let api = Api::new().await;
    let (_, p) = api.as_ada(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "team" }))).await;
    let team = p["id"].as_str().unwrap().to_string();
    let at = |rest: &str| format!("/v1/secrets/projects/{team}{rest}");
    api.join(&team, "bob", "admin", None).await;
    for key in ["PLATFORM_TOKEN", "A"] {
        api.as_ada(Method::PUT, &at(&format!("/environments/development/secrets/{key}")), Some(json!({ "value": "1" }))).await;
    }
    let token = api.gate.issue("bob's", roll_secrets::access::rule(&team, None, None, &[roll_secrets::Action::Read, roll_secrets::Action::Reveal]), Some("usr_bob"));
    assert_eq!(api.req(Method::GET, &at("/environments/development/values"), None, Some(&token)).await.0, StatusCode::OK);
    let (s, e) = api.signed_in("bob", false, Method::POST, &at("/syncs"), Some(json!({ "environment": "development", "kind": "fake", "target": { "name": "x", "token_key": "PLATFORM_TOKEN" } }))).await;
    assert_eq!(s, StatusCode::CREATED, "{e}");
    api.secrets.sweep().unwrap();
    let pushed = api.sync.pushes.lock().unwrap().len();
    assert!(pushed >= 1);
    let issuer = Issuer::new();
    *api.sent.1.lock().unwrap() = Some(issuer.jwks());
    let identity = json!({ "name": "deploy", "issuer": ISSUER, "audience": "roll", "subject": "repo:acme/web:ref:refs/heads/main", "environments": ["development"], "actions": ["read", "reveal"], "ttl_minutes": 10 });
    let (s, i) = api.signed_in("bob", false, Method::POST, &at("/identities"), Some(identity)).await;
    assert_eq!(s, StatusCode::CREATED, "{i}");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let jwt = issuer.sign(json!({ "iss": ISSUER, "aud": "roll", "sub": "repo:acme/web:ref:refs/heads/main", "exp": now + 300, "iat": now }));
    let login = json!({ "identity": i["id"], "token": jwt });
    assert_eq!(api.call(Method::POST, "/v1/secrets/login", Some(login.clone())).await.0, StatusCode::OK);

    assert_eq!(api.as_ada(Method::DELETE, &format!("/v1/workspace/projects/{team}/members/usr_bob"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(api.req(Method::GET, &at("/environments/development/values"), None, Some(&token)).await.0, StatusCode::NOT_FOUND, "his token reaches only what his session does");
    api.as_ada(Method::PUT, &at("/environments/development/secrets/A"), Some(json!({ "value": "2" }))).await;
    api.secrets.sweep().unwrap();
    assert_eq!(api.sync.pushes.lock().unwrap().len(), pushed, "his sync stopped");
    let (s, e) = api.call(Method::POST, "/v1/secrets/login", Some(login)).await;
    assert!(!s.is_success(), "his identity's logins are refused: {s} {e}");
}

/// docs/members-v1-1.md, "When a group loses a role": bob is an admin only through a group. What he
/// made through it (a token, a sync, an identity) works while he's in the group, and stops as soon
/// as he's taken out of it (`maker_reaches` reads his grant now).
#[tokio::test]
async fn a_removed_group_members_token_sync_and_identity_stop() {
    let api = Api::new().await;
    let (_, p) = api.as_ada(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "team" }))).await;
    let team = p["id"].as_str().unwrap().to_string();
    let at = |rest: &str| format!("/v1/secrets/projects/{team}{rest}");
    for key in ["PLATFORM_TOKEN", "A"] {
        api.as_ada(Method::PUT, &at(&format!("/environments/development/secrets/{key}")), Some(json!({ "value": "1" }))).await;
    }
    // The operator makes the group and puts bob in it; ada, an owner but not in it, can't give it
    // a role (D11), and the operator can.
    let (s, g) = api.call(Method::POST, "/v1/workspace/groups", Some(json!({ "name": "platform" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{g}");
    let group = g["id"].as_str().unwrap().to_string();
    assert_eq!(api.call(Method::PUT, &format!("/v1/workspace/groups/{group}/members/usr_bob"), None).await.0, StatusCode::NO_CONTENT);
    let role = format!("/v1/workspace/projects/{team}/groups/{group}");
    let (s, e) = api.as_ada(Method::PUT, &role, Some(json!({ "role": "admin" }))).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::FORBIDDEN, Some("forbidden")), "{e}");
    let (s, e) = api.call(Method::PUT, &role, Some(json!({ "role": "admin" }))).await;
    assert_eq!(s, StatusCode::OK, "{e}");

    let token = api.gate.issue("bob's", roll_secrets::access::rule(&team, None, None, &[roll_secrets::Action::Read, roll_secrets::Action::Reveal]), Some("usr_bob"));
    assert_eq!(api.req(Method::GET, &at("/environments/development/values"), None, Some(&token)).await.0, StatusCode::OK);
    let (s, e) = api.signed_in("bob", false, Method::POST, &at("/syncs"), Some(json!({ "environment": "development", "kind": "fake", "target": { "name": "x", "token_key": "PLATFORM_TOKEN" } }))).await;
    assert_eq!(s, StatusCode::CREATED, "{e}");
    api.secrets.sweep().unwrap();
    let pushed = api.sync.pushes.lock().unwrap().len();
    assert!(pushed >= 1);
    let issuer = Issuer::new();
    *api.sent.1.lock().unwrap() = Some(issuer.jwks());
    let identity = json!({ "name": "deploy", "issuer": ISSUER, "audience": "roll", "subject": "repo:acme/web:ref:refs/heads/main", "environments": ["development"], "actions": ["read", "reveal"], "ttl_minutes": 10 });
    let (s, i) = api.signed_in("bob", false, Method::POST, &at("/identities"), Some(identity)).await;
    assert_eq!(s, StatusCode::CREATED, "{i}");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let jwt = issuer.sign(json!({ "iss": ISSUER, "aud": "roll", "sub": "repo:acme/web:ref:refs/heads/main", "exp": now + 300, "iat": now }));
    let login = json!({ "identity": i["id"], "token": jwt });
    assert_eq!(api.call(Method::POST, "/v1/secrets/login", Some(login.clone())).await.0, StatusCode::OK);

    // Out of the group: everything he made through it stops at once.
    assert_eq!(api.call(Method::DELETE, &format!("/v1/workspace/groups/{group}/members/usr_bob"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(api.req(Method::GET, &at("/environments/development/values"), None, Some(&token)).await.0, StatusCode::NOT_FOUND, "his token reaches only what his session does");
    api.as_ada(Method::PUT, &at("/environments/development/secrets/A"), Some(json!({ "value": "2" }))).await;
    api.secrets.sweep().unwrap();
    assert_eq!(api.sync.pushes.lock().unwrap().len(), pushed, "his sync stopped");
    let (s, e) = api.call(Method::POST, "/v1/secrets/login", Some(login)).await;
    assert!(!s.is_success(), "his identity's logins are refused: {s} {e}");
}

/// docs/members.md: what a member made runs at their rights as they are now. Demoted from admin to
/// developer, their backup schedule and gateway stop (they're an admin's) while their identity and
/// sync narrow to a developer's reach; demoted to viewer, the identity's logins and the sync stop
/// too.
#[tokio::test]
async fn a_demoted_members_backup_gateway_identity_and_sync_stop_or_narrow() {
    let api = Api::new().await;
    let p = api.project.clone();
    let (s, inv) = api.call(Method::POST, &format!("/v1/workspace/projects/{p}/invites"), Some(json!({ "email": "cat@example.com", "role": "admin" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{inv}");
    let token = inv["url"].as_str().unwrap().split("token=").nth(1).unwrap().to_string();
    assert_eq!(api.signed_in("cat", false, Method::POST, &format!("/v1/workspace/invites/{token}/accept"), None).await.0, StatusCode::OK);
    let as_cat = |method: Method, rest: &str, body: Option<Value>| {
        let path = format!("/v1/secrets/projects/{p}{rest}");
        let api = &api;
        async move { api.signed_in("cat", false, method, &path, body).await }
    };
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{p}/resources/bucket/assets"), Some(json!({ "x": 0, "y": 0 }))).await;
    api.set("staging", "API_KEY", "staging-key").await;
    api.set("development", "PLATFORM_TOKEN", "tok_1").await;

    // As an admin she sets up a backup schedule, an identity, a sync and a gateway.
    let schedule = json!({ "bucket": "assets", "recipient": roll_secrets::crypto::public_key(&[7u8; 32]), "every_hours": 24, "keep": 5 });
    let (s, e) = as_cat(Method::PUT, "/backup/schedule", Some(schedule)).await;
    assert!(s.is_success(), "{e}");
    let issuer = Issuer::new();
    *api.sent.1.lock().unwrap() = Some(issuer.jwks());
    let identity = json!({ "name": "deploy", "issuer": ISSUER, "audience": "roll", "subject": "repo:acme/web:ref:refs/heads/main", "environments": ["staging"], "actions": ["read", "reveal"], "ttl_minutes": 10 });
    let (s, i) = as_cat(Method::POST, "/identities", Some(identity)).await;
    assert_eq!(s, StatusCode::CREATED, "{i}");
    let (s, e) = as_cat(Method::POST, "/syncs", Some(json!({ "environment": "development", "kind": "fake", "target": { "name": "x", "token_key": "PLATFORM_TOKEN" } }))).await;
    assert_eq!(s, StatusCode::CREATED, "{e}");
    let (s, made) = as_cat(Method::POST, "/gateways", Some(json!({ "name": "vpc" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{made}");
    let gw = made["gateway"]["id"].as_str().unwrap().to_string();
    let gw_grant = api.gate.tokens.lock().unwrap().get(made["token"].as_str().unwrap()).cloned().unwrap();

    let backups = || api.sent.4.lock().unwrap().keys().filter(|(b, k)| b == "assets" && k.starts_with(&format!("roll-secrets/backups/{p}/"))).count();
    let sweep = || {
        let (s, db, p) = (api.secrets.clone(), api.db.clone(), p.clone());
        async move {
            // Every schedule due now, whatever ran before.
            db.write(move |c| c.execute("UPDATE backup_schedules SET next_at = 0 WHERE project_id = ?", [p.as_str()])).unwrap();
            tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap();
        }
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let jwt = issuer.sign(json!({ "iss": ISSUER, "aud": "roll", "sub": "repo:acme/web:ref:refs/heads/main", "exp": now + 300, "iat": now }));
    let login = || api.call(Method::POST, "/v1/secrets/login", Some(json!({ "identity": i["id"], "token": jwt })));
    let change = |v: &'static str| api.ok(Method::PUT, "/environments/development/secrets/A", Some(json!({ "value": v })));

    change("1").await;
    sweep().await;
    let (pushed, made_backups) = (api.sync.pushes.lock().unwrap().len(), backups());
    assert!(pushed >= 1 && made_backups == 1, "{pushed} {made_backups}");
    assert_eq!(login().await.0, StatusCode::OK);
    assert!(api.secrets.gateway_link(&gw, false, None, Some(&gw_grant)).is_ok());

    // A developer: what's an admin's stops; the identity and the sync narrow to her reach now.
    let demote = |role: &'static str| {
        let (api, path) = (&api, format!("/v1/workspace/projects/{p}/members/usr_cat"));
        async move { api.call(Method::PATCH, &path, Some(json!({ "role": role }))).await }
    };
    assert_eq!(demote("developer").await.0, StatusCode::OK);
    change("2").await;
    sweep().await;
    assert_eq!(backups(), made_backups, "her backup schedule stopped");
    let schedule = api.ok(Method::GET, "/backup/schedule", None).await;
    assert!(schedule["last_error"].as_str().is_some_and(|e| e.contains("usr_cat")), "{schedule}");
    assert!(api.secrets.gateway_link(&gw, false, None, Some(&gw_grant)).is_err(), "her gateway stopped");
    let (s, l) = login().await;
    assert_eq!(s, StatusCode::OK, "staging reveal is a developer's: {l}");
    let narrowed = l["token"].as_str().unwrap().to_string();
    assert_eq!(api.req(Method::GET, "/environments/staging/values", None, Some(&narrowed)).await.0, StatusCode::OK);
    assert!(api.sync.pushes.lock().unwrap().len() > pushed, "development is a developer's");

    // A viewer: no values anywhere, so the identity doesn't log in, the token it gave reaches
    // nothing it can't, and the sync stops.
    assert_eq!(demote("viewer").await.0, StatusCode::OK);
    assert_eq!(api.req(Method::GET, "/environments/staging/values", None, Some(&narrowed)).await.0, StatusCode::FORBIDDEN, "a login's token acts within its maker now");
    let (s, e) = login().await;
    assert!(!s.is_success(), "{s} {e}");
    let pushed = api.sync.pushes.lock().unwrap().len();
    change("3").await;
    sweep().await;
    assert_eq!(api.sync.pushes.lock().unwrap().len(), pushed, "her sync stopped");
}

/// Placeholders and host bindings follow their maker (docs/members.md, "Remove, and leave"): a
/// binding swaps only while whoever set it may still manage its folder, and a placeholder made for
/// a member's consumer only while they may still reveal there. Each stops when its maker is
/// demoted below what it does, and a binding that stopped still never hands out the value.
#[tokio::test]
async fn a_demoted_members_host_binding_and_placeholders_stop() {
    use roll_secrets::proxy::{Place, Position, Refusal};
    let api = Api::new().await;
    let p = api.project.clone();
    let (_, inv) = api.call(Method::POST, &format!("/v1/workspace/projects/{p}/invites"), Some(json!({ "email": "cat@example.com", "role": "admin" }))).await;
    let t = inv["url"].as_str().unwrap().split("token=").nth(1).unwrap().to_string();
    assert_eq!(api.signed_in("cat", false, Method::POST, &format!("/v1/workspace/invites/{t}/accept"), None).await.0, StatusCode::OK);
    api.set("staging", "STRIPE_KEY", "sk_test_fake_member_1").await;
    let bind = json!({ "hosts": ["api.stripe.com"], "query": "key" });
    let (s, e) = api.signed_in("cat", false, Method::PUT, &format!("/v1/secrets/projects/{p}/environments/staging/secrets/STRIPE_KEY/hosts"), Some(bind.clone())).await;
    assert!(s.is_success(), "{e}");
    let issue = |consumer: &'static str, maker: Option<&'static str>| {
        let api = &api;
        async move {
            let r = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": consumer, "untrusted": true, "placeholders": true, "maker": maker }))).await;
            r["values"]["STRIPE_KEY"].as_str().unwrap().to_string()
        }
    };
    let swap = |ph: &str, consumer: &str| api.secrets.substitute(ph, consumer, "api.stripe.com", Position { place: Place::Query, name: "key", value: ph });
    let demote = |role: &'static str| {
        let (api, path) = (&api, format!("/v1/workspace/projects/{p}/members/usr_cat"));
        async move { assert_eq!(api.call(Method::PATCH, &path, Some(json!({ "role": role }))).await.0, StatusCode::OK) }
    };
    let roll = issue("sandbox:sbx_m1", None).await;
    let hers = issue("sandbox:sbx_m2", Some("usr_cat")).await;
    assert_eq!(swap(&roll, "sandbox:sbx_m1").unwrap().value.expose(), "sk_test_fake_member_1");
    assert_eq!(swap(&hers, "sandbox:sbx_m2").unwrap().value.expose(), "sk_test_fake_member_1");

    // A developer may no longer manage staging: her binding swaps nothing, and a read still gets
    // a placeholder, never the value.
    demote("developer").await;
    assert!(matches!(swap(&roll, "sandbox:sbx_m1"), Err(Refusal::MakerUnreachable(_))), "{:?}", swap(&roll, "sandbox:sbx_m1"));
    let again = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "sandbox:sbx_m3", "untrusted": true, "placeholders": true }))).await;
    assert!(again["values"]["STRIPE_KEY"].as_str().unwrap().starts_with("rlph_"), "{again}");

    // Bound again by an admin: roll's placeholder works, and hers too (a developer reveals staging).
    api.ok(Method::PUT, "/environments/staging/secrets/STRIPE_KEY/hosts", Some(bind)).await;
    assert!(swap(&roll, "sandbox:sbx_m1").is_ok() && swap(&hers, "sandbox:sbx_m2").is_ok());
    // A viewer reveals nothing: the placeholder made for her consumer stops; roll's doesn't.
    demote("viewer").await;
    assert!(matches!(swap(&hers, "sandbox:sbx_m2"), Err(Refusal::MakerUnreachable(_))), "{:?}", swap(&hers, "sandbox:sbx_m2"));
    assert!(swap(&roll, "sandbox:sbx_m1").is_ok());
    // New placeholders for her consumer are refused with the code a preview falls back on.
    let (s, refused) = api.call(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "sandbox:sbx_m4", "untrusted": true, "placeholders": true, "maker": "usr_cat" }))).await;
    assert_eq!((s, refused["code"].as_str()), (StatusCode::FORBIDDEN, Some("maker_unreachable")), "{refused}");
}

/// The security review's finding 4: a rotator (a URL outside roll that chooses a key's value)
/// works only while whoever registered it may still manage the project. Removed, their rotator
/// stops choosing values at once.
#[tokio::test]
async fn a_removed_admins_rotator_stops_choosing_values() {
    let api = Api::new().await;
    let p = api.project.clone();
    let (_, inv) = api.call(Method::POST, &format!("/v1/workspace/projects/{p}/invites"), Some(json!({ "email": "cat@example.com", "role": "admin" }))).await;
    let token = inv["url"].as_str().unwrap().split("token=").nth(1).unwrap().to_string();
    assert_eq!(api.signed_in("cat", false, Method::POST, &format!("/v1/workspace/invites/{token}/accept"), None).await.0, StatusCode::OK);
    let (s, r) = api.signed_in("cat", false, Method::POST, &format!("/v1/secrets/projects/{p}/rotators"), Some(json!({ "name": "stripe", "url": "https://rotator.example/stripe" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{r}");
    let link = json!({ "kind": "rotator", "ref": r["id"], "field": "value", "mode": "managed" });
    api.ok(Method::PUT, "/environments/production/secrets/STRIPE_KEY", Some(json!({ "link": link }))).await;
    assert_eq!(api.values("production", "/").await["values"]["STRIPE_KEY"], "vendor-key-1");

    assert_eq!(api.call(Method::DELETE, &format!("/v1/workspace/projects/{p}/members/usr_cat"), None).await.0, StatusCode::NO_CONTENT);
    let (s, e) = api.call(Method::POST, "/environments/production/secrets/STRIPE_KEY/rotate", None).await;
    assert!(!s.is_success() && e.to_string().contains("usr_cat"), "{s} {e}");
    assert_eq!(api.values("production", "/").await["values"]["STRIPE_KEY"], "vendor-key-1", "the old value stands");
}


/// The re-review (docs/members.md, "Remove, and leave"): what a member did through their own
/// token is theirs. What they registered (a sink, a notify URL) outlives the token, at their rights
/// now; what the token handed out (a folder shared with another project, a one-time share, a
/// lease) ends with them. Removed, all of it stops.
#[tokio::test]
async fn what_a_member_did_through_their_token_follows_them() {
    use base64::Engine as _;
    let api = Api::new().await;
    let p = api.project.clone();
    let (_, inv) = api.call(Method::POST, &format!("/v1/workspace/projects/{p}/invites"), Some(json!({ "email": "cat@example.com", "role": "admin" }))).await;
    let t = inv["url"].as_str().unwrap().split("token=").nth(1).unwrap().to_string();
    assert_eq!(api.signed_in("cat", false, Method::POST, &format!("/v1/workspace/invites/{t}/accept"), None).await.0, StatusCode::OK);
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{p}/resources/bucket/assets"), Some(json!({ "x": 0, "y": 0 }))).await;
    api.set("production", "DB", "prod-db").await;
    api.ok(Method::PUT, "/environments/staging/secrets/TEMP_KEY", Some(json!({ "link": { "kind": "bucket", "ref": "assets", "field": "access_key_id", "mode": "dynamic", "options": { "ttl_minutes": 60 } } }))).await;
    let (_, other) = api.call(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "platform" }))).await;
    let platform = other["id"].as_str().unwrap().to_string();
    // She's in platform too: a folder is shared only with a project its sharer can see.
    let (_, inv) = api.call(Method::POST, &format!("/v1/workspace/projects/{platform}/invites"), Some(json!({ "email": "cat@example.com", "role": "viewer" }))).await;
    let t = inv["url"].as_str().unwrap().split("token=").nth(1).unwrap().to_string();
    assert_eq!(api.signed_in("cat", false, Method::POST, &format!("/v1/workspace/invites/{t}/accept"), None).await.0, StatusCode::OK);

    let actions = [roll_secrets::Action::Read, roll_secrets::Action::Reveal, roll_secrets::Action::Write, roll_secrets::Action::Admin];
    let token = api.gate.issue("cat's cli", roll_secrets::access::rule(&p, None, None, &actions), Some("usr_cat"));
    let with = |m: Method, path: &str, body: Value| {
        let (api, token, path) = (&api, token.clone(), path.to_string());
        async move {
            let (s, v) = api.req(m.clone(), &path, Some(body), Some(&token)).await;
            assert!(s.is_success(), "{m} {path}: {s} {v}");
            v
        }
    };
    // Through the token: share production with platform, a one-time share, a lease, a sink, a
    // notify URL.
    with(Method::PUT, "/environments/production/exports?path=/", json!([{ "project": platform }])).await;
    api.ok(Method::PUT, &format!("/v1/secrets/projects/{platform}/environments/staging/imports?path=/"), Some(json!([{ "project": p, "environment": "production", "path": "/" }]))).await;
    let imported = || {
        let (api, path) = (&api, format!("/v1/secrets/projects/{platform}/environments/staging/values"));
        async move { api.ok(Method::GET, &path, None).await }
    };
    assert_eq!(imported().await["values"]["DB"], "prod-db");
    let blob = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7u8; 40]);
    let share = with(Method::POST, "/shares", json!({ "ciphertext": blob, "note": "db", "views": 3, "ttl_minutes": 60 })).await;
    let lease = with(Method::POST, "/environments/staging/resolve", json!({ "consumer": "ci:deploy" })).await["leases"][0].as_str().unwrap().to_string();
    with(Method::POST, "/audit/sinks", json!({ "url": "https://siem.example/roll" })).await;
    with(Method::PUT, "/environments/production/policy", json!({ "protected": true, "approvals": 1, "notify_url": "https://hooks.example/T0/B0/x" })).await;
    let sweep = || {
        let (s, db) = (api.secrets.clone(), api.db.clone());
        async move {
            // Every audit row settled, so the sink has something to send.
            db.write(|c| c.execute("UPDATE audit SET at = at - 60000", ())).unwrap();
            tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap();
        }
    };
    let sent = || api.sent.0.lock().unwrap().len();

    // The token ends: what she registered is hers and goes on; what the token shared ends with it.
    api.gate.tokens.lock().unwrap().retain(|secret, _| *secret != token);
    sweep().await;
    let delivered = sent();
    assert!(delivered >= 1, "her sink outlives the token that made it");
    assert!(imported().await["values"].get("DB").is_none(), "a share the token made ends with the token");
    let open = |id: String| {
        let api = &api;
        async move { api.call(Method::GET, &format!("/v1/secrets/shares/{id}"), None).await.0 }
    };
    assert_eq!(open(share["id"].as_str().unwrap().to_string()).await, StatusCode::OK, "a one-time share stands until its maker goes");

    // She's removed: her share and lease end, her sink and notify URL stop.
    assert_eq!(api.call(Method::DELETE, &format!("/v1/workspace/projects/{p}/members/usr_cat"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(open(share["id"].as_str().unwrap().to_string()).await, StatusCode::NOT_FOUND);
    let leases = api.ok(Method::GET, "/leases", None).await;
    assert!(leases.as_array().unwrap().iter().find(|l| l["id"] == lease.as_str()).is_none_or(|l| l["revoked_at"].is_number()), "{leases}");
    api.ok(Method::POST, "/environments/production/requests", Some(json!({ "ops": [{ "op": "set", "key": "DB", "value": "x" }] }))).await;
    sweep().await;
    assert_eq!(sent(), delivered, "no sink batch and no approval notice once she's gone");
}

/// `DELETE /v1/workspace/projects/{id}` as the operator (or with `token`), `q` its query.
async fn delete_project(api: &Api, q: &str, token: Option<&str>) -> (StatusCode, Value) {
    api.req(Method::DELETE, &format!("/v1/workspace/projects/{}{q}", api.project), None, token).await
}

#[tokio::test]
async fn deleting_a_project_that_holds_secrets_needs_a_secrets_admins_confirmation() {
    let api = Api::new().await;
    // A project with nothing in it goes as before.
    let (_, empty) = api.call(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "empty" }))).await;
    let empty = empty["id"].as_str().unwrap().to_string();
    api.ok(Method::GET, &format!("/v1/secrets/projects/{empty}/environments"), None).await;
    assert_eq!(api.call(Method::DELETE, &format!("/v1/workspace/projects/{empty}"), None).await.0, StatusCode::NO_CONTENT);

    // An encryption key alone holds it back.
    api.ok(Method::POST, "/transit", Some(json!({ "name": "cards" }))).await;
    let (s, e) = delete_project(&api, "", None).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("project_holds_secrets")), "{e}");
    assert!(e["error"].as_str().unwrap().contains("?confirm=shop"), "{e}");
    api.set("production", "STRIPE_KEY", "sk_live_never_in_an_error").await;
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true }))).await;
    // Workspace `delete` isn't enough: a token that may write the project's secrets, not administer
    // them, is refused however it confirms.
    let writer = api.token(json!({ "actions": ["read", "write", "delete"] }));
    let (s, e) = delete_project(&api, "?confirm=shop", Some(&writer)).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::FORBIDDEN, Some("forbidden")), "{e}");
    let (s, e) = delete_project(&api, "?confirm=production", None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    assert!(e["error"].as_str().unwrap().contains("1 secret, the protected environment production and 1 encryption key"), "{e}");
    assert!(!e.to_string().contains("sk_live"));
    assert_eq!(api.call(Method::GET, "/environments/production/secrets/STRIPE_KEY", None).await.0, StatusCode::OK, "refused: nothing went");
    // An admin's token, with the project's name.
    let admin = api.token(json!({ "actions": ["admin"] }));
    assert_eq!(delete_project(&api, "?confirm=shop", Some(&admin)).await.0, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_deleted_project_reads_as_unknown_and_comes_back_within_its_window() {
    let api = Api::new().await;
    api.set("production", "STRIPE_KEY", "sk_live_kept").await;
    let reader = api.token(json!({ "actions": ["read", "reveal"] }));
    // A workload identity's login: its token is roll-access's, and ends with the delete.
    let (login, grant) = api.gate.grant("login", roll_secrets::access::rule(&api.project, None, None, &[roll_secrets::Action::Read]), None);
    let (p, g) = (api.project.clone(), grant.clone());
    api.db.write(move |c| c.execute("INSERT INTO identity_logins (grant_id, identity, project_id, expires_at) VALUES (?, 'idn_ci', ?, ?)", code_sql::params![g, p, i64::MAX])).unwrap();
    assert_eq!(api.req(Method::GET, "/environments", None, Some(&login)).await.0, StatusCode::OK);
    assert_eq!(delete_project(&api, "?confirm=shop", None).await.0, StatusCode::NO_CONTENT);
    // Nobody reads it: an unknown project, to the operator and to a token that could reveal it.
    assert_eq!(api.call(Method::GET, "/environments/production/values?path=/", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(api.req(Method::GET, "/environments/production/values?path=/", None, Some(&reader)).await.0, StatusCode::NOT_FOUND);
    assert_eq!(api.ok(Method::GET, "/v1/secrets/search?key=STRIPE_KEY", None).await, json!([]));
    let deleted = api.ok(Method::GET, "/v1/secrets/deleted", None).await;
    assert_eq!((deleted[0]["project"].as_str(), deleted[0]["name"].as_str(), deleted[0]["deleted_by"].as_str()), (Some(api.project.as_str()), Some("shop"), Some("operator")), "{deleted}");
    assert_eq!(deleted[0]["purge_at"].as_i64().unwrap() - deleted[0]["deleted_at"].as_i64().unwrap(), 7 * 86_400_000);
    // Someone else's: not theirs to see or recover.
    assert_eq!(api.as_ada(Method::GET, "/v1/secrets/deleted", None).await.1, json!([]));
    let recover = format!("/v1/secrets/projects/{}/recover", api.project);
    assert_eq!(api.as_ada(Method::POST, &recover, None).await.0, StatusCode::NOT_FOUND);
    // Her own: she brings it back herself (even when an admin deleted it).
    let (_, hers) = api.as_ada(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "notes" }))).await;
    let hers = hers["id"].as_str().unwrap().to_string();
    let (s, _) = api.as_ada(Method::PUT, &format!("/v1/secrets/projects/{hers}/environments/development/secrets/TOKEN"), Some(json!({ "value": "x" }))).await;
    assert_eq!(s, StatusCode::OK);
    // A token of hers acts as her (her membership), so it follows the window too.
    let (ada_ci, _) = api.gate.grant("ada-ci", roll_secrets::access::rule(&hers, None, None, &[roll_secrets::Action::Read]), Some("usr_ada"));
    let envs = format!("/v1/secrets/projects/{hers}/environments");
    assert_eq!(api.req(Method::GET, &envs, None, Some(&ada_ci)).await.0, StatusCode::OK);
    // A sandbox she copied development into reads it again as her at each start: in the project's
    // window it's kept (the sandbox waits), not refused (it would let go of the values for good).
    let copy = || api.secrets.values_as_maker("usr_ada", &hers, "development", "/", "sandbox:sbx_notes").map(|v| v.values.contains_key("TOKEN"));
    let standing = || api.secrets.environment_standing(&hers, "development").unwrap();
    assert!(copy().unwrap());
    assert_eq!(api.call(Method::DELETE, &format!("/v1/workspace/projects/{hers}?confirm=notes"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(api.req(Method::GET, &envs, None, Some(&ada_ci)).await.0, StatusCode::NOT_FOUND);
    assert!(copy().is_err());
    assert!(matches!(standing(), roll_secrets::deleted_envs::Standing::Kept { .. }), "{:?}", standing());
    assert_eq!(api.as_ada(Method::GET, "/v1/secrets/deleted", None).await.1[0]["project"], hers.as_str());
    assert_eq!(api.as_ada(Method::POST, &format!("/v1/secrets/projects/{hers}/recover"), None).await.0, StatusCode::OK);
    assert_eq!(api.req(Method::GET, &envs, None, Some(&ada_ci)).await.0, StatusCode::OK, "her membership came back with it");
    assert_eq!(standing(), roll_secrets::deleted_envs::Standing::Live);
    assert!(copy().unwrap(), "the sandbox's next start has its values again");

    let back = api.ok(Method::POST, &recover, None).await;
    assert_eq!(back["name"], "shop");
    assert_eq!(api.values("production", "/").await["values"]["STRIPE_KEY"], "sk_live_kept");
    assert_eq!(api.call(Method::GET, &format!("/v1/workspace/projects/{}", api.project), None).await.0, StatusCode::OK, "its workspace project came back with it");
    let audit = api.ok(Method::GET, "/audit", None).await;
    let actions: Vec<&str> = audit.as_array().unwrap().iter().map(|e| e["action"].as_str().unwrap()).collect();
    assert!(actions.contains(&"project.delete") && actions.contains(&"project.recover"), "{actions:?}");
    assert!(!audit.to_string().contains("sk_live_kept"));
    assert_eq!(api.call(Method::POST, &recover, None).await.0, StatusCode::NOT_FOUND, "not deleted any more");
    assert_eq!(api.req(Method::GET, "/environments", None, Some(&login)).await.0, StatusCode::UNAUTHORIZED, "the login's token was revoked at the delete");
}

#[tokio::test]
async fn past_its_window_a_deleted_project_ends_its_log_at_a_signed_digest_then_leaves_no_rows() {
    let api = Api::new().await;
    api.ok(Method::POST, "/audit/sinks", Some(json!({ "url": "https://siem.example/roll" }))).await;
    api.set("production", "STRIPE_KEY", "sk_live_purged").await;
    api.ok(Method::POST, "/transit", Some(json!({ "name": "cards" }))).await;
    assert_eq!(delete_project(&api, "?confirm=shop", None).await.0, StatusCode::NO_CONTENT);
    let sweep = || {
        let s = api.secrets.clone();
        async move { tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap() }
    };
    let count = |table: &str| {
        let (sql, p) = (format!("SELECT COUNT(*) FROM {table} WHERE project_id = ?"), api.project.clone());
        api.db.read(move |c| c.query_row(&sql, [p.as_str()], |r| r.get::<_, i64>(0))).unwrap()
    };
    sweep().await;
    assert_eq!((count("secrets"), count("transit_keys")), (1, 1), "nothing goes within the window");

    // The window over: closed with a last row and a signed digest, kept until the sink has them.
    let p = api.project.clone();
    api.db.write(move |c| c.execute("UPDATE projects SET purge_at = 0 WHERE project_id = ?", [p.as_str()])).unwrap();
    sweep().await;
    let recover = format!("/v1/secrets/projects/{}/recover", api.project);
    assert_eq!(api.call(Method::POST, &recover, None).await.0, StatusCode::GONE, "closed: no way back");
    assert_eq!(api.secrets.environment_standing(&api.project, "production").unwrap(), roll_secrets::deleted_envs::Standing::Gone, "a sandbox copying it lets go");
    assert_eq!(count("secrets"), 1, "the sink hasn't had the closing digest yet");
    api.db.write(|c| c.execute("UPDATE audit SET at = at - 60000", ())).unwrap();
    sweep().await;
    let events: Vec<Value> = api.sent.0.lock().unwrap().iter().flat_map(|b| b["events"].as_array().cloned().unwrap_or_default()).collect();
    let actions: Vec<&str> = events.iter().map(|e| e["action"].as_str().unwrap()).collect();
    assert!(actions.contains(&"project.delete"), "{actions:?}");
    assert_eq!(&actions[actions.len() - 2..], ["project.purge", "audit.digest"], "the log ends at a digest");
    assert!(events.last().unwrap()["digest"].as_str().is_some_and(|d| d.split('.').count() == 3));
    assert!(!serde_json::to_string(&events).unwrap().contains("sk_live_purged"));

    sweep().await;
    for t in ["secrets", "versions", "audit", "audit_heads", "audit_sinks", "transit_keys", "environments", "placeholders", "acme_bindings", "gateways", "projects"] {
        assert_eq!(count(t), 0, "{t}");
    }
    assert!(api.ws.deleted(&api.project).unwrap().is_none(), "the workspace let it go too");
    assert_eq!(api.call(Method::POST, &recover, None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_deleted_environment_reads_as_gone_its_references_say_so_and_it_comes_back() {
    let api = Api::new().await;
    api.set("staging", "DB", "postgres://staging.internal").await;
    api.set("development", "UPSTREAM", "${staging.DB}").await;
    // Before anything goes, the refusal and the dialog's preview name what breaks.
    let pre = api.ok(Method::GET, "/environments/staging/deletion", None).await;
    assert_eq!((pre["confirm"].as_bool(), pre["window_days"].as_i64()), (Some(true), Some(7)), "{pre}");
    assert_eq!(pre["breaks"], json!([{ "environment": "development", "path": "/", "key": "UPSTREAM", "via": "reference" }]));
    let (s, e) = api.call(Method::DELETE, "/environments/staging", None).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("confirm_required")), "{e}");
    assert!(e["error"].as_str().unwrap().contains("development:/UPSTREAM") && !e.to_string().contains("postgres://"), "{e}");
    let gone = api.ok(Method::DELETE, "/environments/staging?confirm=staging", None).await;
    assert_eq!(gone["purge_at"].as_i64().unwrap() - gone["deleted_at"].as_i64().unwrap(), 7 * 86_400_000);

    // Nobody reads it, a reference to it says why, and its slug stays its own.
    assert_eq!(api.call(Method::GET, "/environments/staging/values?path=/", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(slugs(&api.ok(Method::GET, "/environments", None).await), ["development", "production"]);
    assert_eq!(api.ok(Method::GET, "/v1/secrets/search?key=DB", None).await, json!([]));
    let v = api.values("development", "/").await;
    assert!(v["problems"].to_string().contains("references staging, which was deleted; recover it or change the reference"), "{v}");
    let (s, e) = api.call(Method::POST, "/environments", Some(json!({ "slug": "staging" }))).await;
    assert!(s == StatusCode::CONFLICT && e["error"].as_str().unwrap().contains("recover"), "{e}");
    let deleted = api.ok(Method::GET, "/v1/secrets/deleted", None).await;
    assert_eq!((deleted[0]["environment"].as_str(), deleted[0]["project"].as_str()), (Some("staging"), Some(api.project.as_str())), "{deleted}");
    assert_eq!(api.as_ada(Method::GET, "/v1/secrets/deleted", None).await.1, json!([]), "not hers to see");

    api.ok(Method::POST, "/environments/staging/recover", None).await;
    assert_eq!(api.values("development", "/").await["values"]["UPSTREAM"], "postgres://staging.internal");
    assert_eq!(api.call(Method::POST, "/environments/staging/recover", None).await.0, StatusCode::NOT_FOUND, "not deleted any more");
    let audit = api.ok(Method::GET, "/audit?action=environment", None).await;
    let actions: Vec<&str> = audit.as_array().unwrap().iter().map(|e| e["action"].as_str().unwrap()).collect();
    assert!(actions.contains(&"environment.delete") && actions.contains(&"environment.recover"), "{actions:?}");
    // A protected environment with no approvals (a new project's production) goes directly once confirmed.
    assert_eq!(api.call(Method::DELETE, "/environments/production", None).await.0, StatusCode::CONFLICT);
    assert_eq!(api.call(Method::DELETE, "/environments/production?confirm=production", None).await.0, StatusCode::OK);
}

#[tokio::test]
async fn deleting_a_protected_environment_that_asks_for_approvals_is_a_change_request() {
    let api = Api::new().await;
    api.set("production", "KEY", "kept").await;
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true, "approvals": 1 }))).await;
    let (s, r) = api.call(Method::DELETE, "/environments/production?confirm=production", None).await;
    assert_eq!((s, r["request"]["action"].as_str(), r["request"]["state"].as_str()), (StatusCode::ACCEPTED, Some("delete_environment"), Some("open")), "{r}");
    let id = r["request"]["id"].as_str().unwrap();
    assert_eq!(api.values("production", "/").await["values"]["KEY"], "kept", "nothing went yet");
    let (s, e) = api.call(Method::POST, &format!("/environments/production/requests/{id}/apply"), None).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("approval_required")));
    let reviewer = api.token(json!({ "name": "lead", "actions": ["read", "admin", "write", "delete"] }));
    let (s, r) = api.req(Method::POST, &format!("/environments/production/requests/{id}/review"), Some(json!({ "verdict": "approve" })), Some(&reviewer)).await;
    assert_eq!((s, r["state"].as_str()), (StatusCode::OK, Some("approved")), "{r}");
    let r = api.ok(Method::POST, &format!("/environments/production/requests/{id}/apply"), None).await;
    assert_eq!(r["state"], "applied");
    assert_eq!(api.call(Method::GET, "/environments/production", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(api.ok(Method::GET, "/v1/secrets/deleted", None).await[0]["environment"], "production");
}

/// Decide, then revoke: an environment's delete that its write refuses (here an ephemeral one based
/// on it meanwhile) revokes none of what it issued and is no event; the delete that goes revokes it
/// all, with its `env.deleted`.
#[tokio::test]
async fn an_environment_delete_its_write_refuses_revokes_nothing() {
    let api = Api::new().await;
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{}/resources/bucket/assets", api.project), Some(json!({ "x": 0, "y": 0 }))).await;
    let link = |field: &str, mode: &str| json!({ "link": { "kind": "bucket", "ref": "assets", "field": field, "mode": mode } });
    api.ok(Method::PUT, "/environments/production/secrets/AWS_ACCESS_KEY_ID", Some(link("access_key_id", "managed"))).await;
    api.ok(Method::PUT, "/environments/production/secrets/TEMP_KEY", Some(link("access_key_id", "dynamic"))).await;
    api.ok(Method::POST, "/environments/production/resolve", Some(json!({ "consumer": "app:web" }))).await;
    let live = api.fake.live_keys.lock().unwrap().clone();
    assert_eq!(live.len(), 2, "a managed key and a lease: {live:?}");
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true, "approvals": 1 }))).await;
    let (_, r) = api.call(Method::DELETE, "/environments/production?confirm=production", None).await;
    let id = r["request"]["id"].as_str().unwrap().to_string();
    let reviewer = api.token(json!({ "name": "lead", "actions": ["read", "admin", "write", "delete"] }));
    api.req(Method::POST, &format!("/environments/production/requests/{id}/review"), Some(json!({ "verdict": "approve" })), Some(&reviewer)).await;
    api.ok(Method::POST, "/environments", Some(json!({ "slug": "pv-1", "base": "production", "ttl_minutes": 60 }))).await;
    let (s, e) = api.call(Method::POST, &format!("/environments/production/requests/{id}/apply"), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    assert_eq!(*api.fake.live_keys.lock().unwrap(), live, "refused: nothing revoked");
    assert!(api.ok(Method::GET, "/leases", None).await.as_array().unwrap().iter().all(|l| l["revoked_at"].is_null()));
    assert_eq!(api.ok(Method::GET, "/events?kinds=env.deleted", None).await["events"], json!([]), "nor an event");
    api.ok(Method::DELETE, "/environments/pv-1", None).await;
    assert_eq!(api.ok(Method::POST, &format!("/environments/production/requests/{id}/apply"), None).await["state"], "applied");
    assert!(api.fake.live_keys.lock().unwrap().is_empty(), "deleted: what it issued is revoked");
    assert_eq!(api.ok(Method::GET, "/events?kinds=env.deleted", None).await["events"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_project_holding_only_a_registered_resource_needs_a_secrets_admins_confirmation() {
    let api = Api::new().await;
    api.ok(Method::POST, "/rotators", Some(json!({ "name": "stripe", "url": "https://rotator.example/stripe?token=t" }))).await;
    let (s, e) = delete_project(&api, "", None).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("project_holds_secrets")), "{e}");
    assert!(e["error"].as_str().unwrap().contains("1 rotator"), "{e}");
    let writer = api.token(json!({ "actions": ["read", "write", "delete"] }));
    assert_eq!(delete_project(&api, "?confirm=shop", Some(&writer)).await.0, StatusCode::FORBIDDEN);
    assert_eq!(delete_project(&api, "?confirm=shop", None).await.0, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn past_its_window_a_deleted_environment_is_purged_and_leaves_no_rows() {
    let api = Api::new().await;
    api.set("staging", "KEY", "v1").await;
    api.set("staging", "KEY", "v2").await;
    api.ok(Method::PUT, "/environments/staging/imports?path=/", Some(json!([{ "environment": "development", "path": "/" }]))).await;
    let p = api.project.clone();
    let id: String = api.db.read(move |c| c.query_row("SELECT id FROM environments WHERE project_id = ? AND slug = 'staging'", [p.as_str()], |r| r.get(0))).unwrap();
    api.ok(Method::DELETE, "/environments/staging", None).await;
    let sweep = || {
        let s = api.secrets.clone();
        async move { tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap() }
    };
    let count = |table: &str| {
        let (sql, e) = (format!("SELECT COUNT(*) FROM {table} WHERE {}", if table == "environments" { "id = ?" } else { "env_id = ?" }), id.clone());
        api.db.read(move |c| c.query_row(&sql, [e.as_str()], |r| r.get::<_, i64>(0))).unwrap()
    };
    sweep().await;
    assert_eq!((count("secrets"), count("environments")), (1, 1), "nothing goes within the window");
    let e = id.clone();
    api.db.write(move |c| c.execute("UPDATE environments SET purge_at = 0 WHERE id = ?", [e.as_str()])).unwrap();
    sweep().await;
    for t in ["secrets", "versions", "changes", "folders", "imports", "requests", "leases", "environments"] {
        assert_eq!(count(t), 0, "{t}");
    }
    let audit = api.ok(Method::GET, "/audit?action=environment.purge", None).await;
    assert_eq!(audit[0]["environment"], "staging", "{audit}");
    assert_eq!(api.call(Method::POST, "/environments/staging/recover", None).await.0, StatusCode::NOT_FOUND);
    api.ok(Method::POST, "/environments", Some(json!({ "slug": "staging" }))).await;
}


/// rolld's services and sandboxes as reach asks for them: `web` deploys staging, `api` reads
/// production through a `roll://` variable, and a sandbox holds development's KEY and OTHER.
struct FakeHolders;

impl roll_secrets::reach::Holders for FakeHolders {
    fn services(&self, _: &str) -> Result<Vec<roll_secrets::reach::ServiceUse>, String> {
        let svc = |id: &str, environment: &str, references: &[&str]| roll_secrets::reach::ServiceUse { service: id.into(), name: id.into(), environment: environment.into(), last_deployed_at: Some(1), references: references.iter().map(|r| r.to_string()).collect() };
        Ok(vec![svc("web", "staging", &[]), svc("api", "production", &["roll://production/KEY"])])
    }
    fn sandboxes(&self, _: &str) -> Result<Vec<roll_secrets::reach::SandboxCopy>, String> {
        Ok(vec![roll_secrets::reach::SandboxCopy { id: "sbx_1".into(), name: "try".into(), owner: "ada".into(), by: "usr_ada".into(), from: "development".into(), keys: vec!["KEY".into(), "OTHER".into()] }])
    }
}

/// Reach names people (docs/members.md): the members whose role reveals a folder and the
/// operator's admins as a count, a member's OAuth client, someone who read it and then left
/// (`knew`), and the services and sandboxes rolld says read it.
#[tokio::test]
async fn reach_names_the_people_clients_and_holders_of_a_folder() {
    let api = Api::new().await;
    api.secrets.set_reach_holders(Arc::new(FakeHolders));
    let (_, p) = api.as_ada(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "team" }))).await;
    let team = p["id"].as_str().unwrap().to_string();
    let at = |rest: &str| format!("/v1/secrets/projects/{team}{rest}");
    api.as_ada(Method::PUT, &at("/environments/development/secrets/KEY"), Some(json!({ "value": "fake-dev" }))).await;
    api.as_ada(Method::PUT, &at("/environments/staging/imports?path=/"), Some(json!([{ "environment": "development", "path": "/" }]))).await;
    api.join(&team, "bob", "developer", None).await;
    api.join(&team, "cat", "admin", None).await;
    api.join(&team, "vic", "developer", None).await;
    let (s, v) = api.signed_in("vic", false, Method::GET, &at("/environments/development/secrets/KEY/value"), None).await;
    assert_eq!((s, v["value"].as_str()), (StatusCode::OK, Some("fake-dev")));
    assert_eq!(api.as_ada(Method::DELETE, &format!("/v1/workspace/projects/{team}/members/usr_vic"), None).await.0, StatusCode::NO_CONTENT);
    let client = api.gate.oauth("claude", roll_secrets::access::rule(&team, None, None, &[roll_secrets::Action::Read, roll_secrets::Action::Reveal]), "usr_bob");

    let (s, r) = api.as_ada(Method::GET, &at("/environments/development/reach?path=/&key=KEY"), None).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let mut people: Vec<(&str, &str)> = r["people"].as_array().unwrap().iter().map(|p| (p["handle"].as_str().unwrap(), p["role"].as_str().unwrap())).collect();
    people.sort();
    assert_eq!(people, [("ada", "owner"), ("bob", "developer"), ("cat", "admin")], "{r}");
    assert_eq!(r["admins"], 1);
    assert_eq!((r["clients"][0]["id"].as_str(), r["clients"][0]["created_by"].as_str(), r["tokens"].as_array().unwrap().len()), (Some(client.as_str()), Some("usr_bob"), 0), "{r}");
    assert_eq!((r["knew"][0]["user"].as_str(), r["knew"][0]["why"].as_str(), r["knew"].as_array().unwrap().len()), (Some("usr_vic"), Some("left"), 1), "{r}");
    let services: Vec<(&str, &str)> = r["services"].as_array().unwrap().iter().map(|s| (s["name"].as_str().unwrap(), s["through"].as_str().unwrap())).collect();
    assert_eq!((services, r["sandboxes"][0]["keys"].clone()), (vec![("web", "staging:/")], json!(["KEY"])), "{r}");
    assert!(!r.to_string().contains("fake-dev"), "names, never a value");
}

#[tokio::test]
async fn the_events_feed_answers_in_commit_order_filtered_by_kind_environment_and_access_and_never_holds_a_value() {
    let api = Api::new().await;
    let (staging, production) = ("sk_live_feedstaging0123456789", "sk_live_feedproduction987654");
    api.set("staging", "STRIPE_KEY", staging).await;
    api.set("production", "STRIPE_KEY", production).await;
    let start = api.ok(Method::GET, "/events?latest=1", None).await;
    assert_eq!(start["events"], json!([]));
    let start = start["next"].as_i64().unwrap();
    assert!(start > 0, "the head moved with the writes: {start}");
    assert_eq!(api.call(Method::GET, "/events?latest=1&after=0", None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(api.call(Method::GET, "/events?kinds=secret", None).await.0, StatusCode::BAD_REQUEST);

    // A scan matching both (a row per environment), production losing its protection, an
    // environment deleted and recovered.
    api.ok(Method::POST, "/leaks", Some(json!({ "candidates": [staging, production, "not-a-stored-value-123"] }))).await;
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": false }))).await;
    api.ok(Method::POST, "/environments", Some(json!({ "slug": "qa" }))).await;
    api.ok(Method::DELETE, "/environments/qa", None).await;
    api.ok(Method::POST, "/environments/qa/recover", None).await;

    let all = api.ok(Method::GET, &format!("/events?after={start}"), None).await;
    let events = all["events"].as_array().unwrap();
    let got: Vec<(&str, Option<&str>)> = events.iter().map(|e| (e["kind"].as_str().unwrap(), e["environment"].as_str())).collect();
    assert_eq!(got, [("scan.finding", Some("production")), ("scan.finding", Some("staging")), ("production.unprotected", Some("production")), ("env.deleted", Some("qa")), ("env.recovered", Some("qa"))]);
    let seqs: Vec<i64> = events.iter().map(|e| e["seq"].as_i64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]) && seqs[0] > start, "{seqs:?}");
    assert_eq!(all["next"], api.ok(Method::GET, "/events?latest=1", None).await["next"], "a short page ends at the head");
    assert_eq!(events[0]["key"], "STRIPE_KEY");
    assert_eq!(events[0]["data"]["matched"], 2);
    // Never a value, or anything made from one: names, kinds, ids, counts and reasons.
    let text = all.to_string();
    assert!(!text.contains(staging) && !text.contains(production) && !text.contains("feedstaging") && !text.contains("feedproduction"), "{text}");
    for e in events {
        for field in e["data"].as_object().unwrap().keys() {
            assert!(["actor", "path", "keys", "candidates", "matched", "guard", "purge_at"].contains(&field.as_str()), "{field} in {e}");
        }
    }

    // Filtered by the route: kinds (a family too), the environment, and a page at a time.
    let kinds = |q: &str| {
        let api = &api;
        let q = q.to_string();
        async move { api.ok(Method::GET, &format!("/events?after={start}&{q}"), None).await["events"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap().to_string()).collect::<Vec<_>>() }
    };
    assert_eq!(kinds("kinds=scan.*").await, ["scan.finding", "scan.finding"]);
    assert_eq!(kinds("kinds=env.deleted,production.unprotected").await, ["production.unprotected", "env.deleted"]);
    assert_eq!(kinds("environment=staging").await, ["scan.finding"]);
    let page = api.ok(Method::GET, &format!("/events?after={start}&limit=1&kinds=production.unprotected,env.deleted"), None).await;
    assert_eq!((page["events"].as_array().unwrap().len(), &page["next"]), (1, &json!(seqs[2])));
    let rest = api.ok(Method::GET, &format!("/events?after={}&kinds=production.unprotected,env.deleted", seqs[2]), None).await;
    assert_eq!(rest["events"][0]["seq"], json!(seqs[3]));

    // A token that reads only staging: production's and qa's events are left out, and naming
    // production is a 403.
    let token = api.token(json!({ "environments": ["staging"], "actions": ["read"] }));
    let (s, mine) = api.req(Method::GET, &format!("/events?after={start}"), None, Some(&token)).await;
    assert_eq!(s, StatusCode::OK, "{mine}");
    let mine: Vec<&str> = mine["events"].as_array().unwrap().iter().map(|e| e["environment"].as_str().unwrap()).collect();
    assert_eq!(mine, ["staging"]);
    assert_eq!(api.req(Method::GET, "/events?environment=production", None, Some(&token)).await.0, StatusCode::FORBIDDEN);

    // `latest` is the head, past every event.
    let head = api.ok(Method::GET, "/events?latest=1", None).await["next"].as_i64().unwrap();
    assert!(head >= seqs[4]);
    assert_eq!(api.ok(Method::GET, &format!("/events?after={head}"), None).await["events"], json!([]));
}

/// A watcher's delta read (docs/secrets.md, "Delta reads"): after one key changes and another
/// goes, it gets that key, what references it and a live link's key (read again, as a full read
/// would), the gone key's name, and their new stamps, with the folder's whole `links`; it's
/// audited as an export naming only those keys. Sent back as it now is, it's nothing.
#[tokio::test]
async fn a_watchers_delta_returns_what_changed_and_is_audited() {
    let api = Api::new().await;
    for (k, v) in [("A", "1"), ("B", "2"), ("C", "3"), ("R", "${A}-x")] {
        api.set("production", k, v).await;
    }
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{}/resources/bucket/assets", api.project), Some(json!({ "x": 0, "y": 0 }))).await;
    api.ok(Method::PUT, "/environments/production/secrets/L", Some(json!({ "link": { "kind": "bucket", "ref": "assets", "field": "url", "mode": "live" } }))).await;
    let tok = api.token(json!({ "environments": ["production"], "actions": ["read", "reveal"] }));
    let delta = |held: Value| api.req(Method::POST, "/environments/production/values/delta", Some(held), Some(&tok));
    let (s, full) = delta(json!({})).await;
    assert_eq!((s, full["delta"].as_bool()), (StatusCode::OK, Some(false)), "{full}");
    assert_eq!(full["values"].as_object().unwrap().keys().collect::<Vec<_>>(), ["A", "B", "C", "L", "R"]);
    assert_eq!(full["stamps"].as_object().map(|s| s.len()), Some(5), "{full}");

    api.set("production", "A", "10").await;
    api.ok(Method::DELETE, "/environments/production/secrets/C", None).await;
    let held = |a: &Value| json!({ "digest": a["digest"], "basis": a["basis"], "stamps": a["stamps"] });
    let (s, d) = delta(held(&full)).await;
    assert_eq!((s, d["delta"].as_bool()), (StatusCode::OK, Some(true)), "{d}");
    assert_eq!((d["values"]["A"].clone(), d["values"]["L"].clone(), d["values"]["R"].clone(), d["removed"].clone()), (json!("10"), full["values"]["L"].clone(), json!("10-x"), json!(["C"])));
    assert_eq!(d["stamps"].as_object().unwrap().keys().collect::<Vec<_>>(), ["A", "L", "R"]);
    assert_eq!((d["links"].clone(), d["linked"].clone()), (full["links"].clone(), json!(["L"])), "{d}");
    assert_ne!(d["stamps"]["A"], full["stamps"]["A"]);
    let (_, now) = api.req(Method::GET, "/environments/production/digest", None, Some(&tok)).await;
    assert_eq!(d["digest"], now["digest"]);
    let audit = api.ok(Method::GET, "/audit?action=secrets.export", None).await;
    assert_eq!(audit[0]["keys"], json!(["A", "L", "R"]), "{}", audit[0]);

    let mut kept = full["stamps"].as_object().unwrap().clone();
    kept.remove("C");
    kept.extend(d["stamps"].as_object().unwrap().clone());
    let (_, again) = delta(json!({ "digest": d["digest"], "basis": d["basis"], "stamps": kept })).await;
    assert_eq!((again["delta"].as_bool(), again["values"].clone(), again["removed"].clone()), (Some(true), json!({}), json!([])), "{again}");
    assert_eq!(full["links"], json!([{ "name": "L", "kind": "bucket", "ref": "assets" }]));
}

/// A delta can't vouch for what a permission decided: once the token may no longer reveal the
/// environment a reference reads, the same claim gets the full set, which says so.
#[tokio::test]
async fn a_permission_change_since_the_answer_held_gets_the_full_set() {
    let api = Api::new().await;
    api.set("staging", "S", "upstream").await;
    api.set("production", "A", "1").await;
    api.set("production", "R", "${staging.S}").await;
    let tok = api.token(json!({ "environments": ["production", "staging"], "actions": ["read", "reveal"] }));
    let delta = |held: Value| api.req(Method::POST, "/environments/production/values/delta", Some(held), Some(&tok));
    let (_, full) = delta(json!({})).await;
    assert_eq!(full["values"], json!({ "A": "1", "R": "upstream" }), "{full}");

    let narrower = roll_secrets::access::rule(&api.project, Some(vec!["production".into()]), None, &[roll_secrets::Action::Read, roll_secrets::Action::Reveal]);
    api.gate.tokens.lock().unwrap().get_mut(&tok).unwrap().scope = Arc::new(roll_auth::scope::Scope(vec![narrower]));
    let (s, d) = delta(json!({ "digest": full["digest"], "basis": full["basis"], "stamps": full["stamps"] })).await;
    assert_eq!((s, d["delta"].as_bool()), (StatusCode::OK, Some(false)), "{d}");
    assert_eq!(d["values"]["A"], "1");
    assert!(d["problems"].as_array().unwrap().iter().any(|p| p["key"] == "R"), "{d}");
}

#[tokio::test]
async fn a_change_is_one_event_at_its_audit_rows_seq_naming_keys_and_versions_never_a_value() {
    let api = Api::new().await;
    let (a, b, p) = ("sk_live_changedA0123456789", "sk_live_changedB0123456789", "sk_live_changedP0123456789");
    let start = api.ok(Method::GET, "/events?latest=1", None).await["next"].as_i64().unwrap();
    // A bulk write is one row naming its keys, so one event; then production, a delete, and a
    // policy change (not a value: `access.changed`).
    let bulk = api.ok(Method::POST, "/environments/staging/changes", Some(json!({ "ops": [{ "op": "set", "key": "A", "value": a }, { "op": "set", "key": "B", "value": b }] }))).await;
    api.set("production", "STRIPE_KEY", p).await;
    api.ok(Method::DELETE, "/environments/staging/secrets/A", None).await;
    api.ok(Method::PUT, "/environments/staging/policy", Some(json!({ "protected": true, "approvals": 0 }))).await;

    let all = api.ok(Method::GET, &format!("/events?after={start}"), None).await;
    let events = all["events"].as_array().unwrap();
    let got: Vec<(&str, Option<&str>)> = events.iter().map(|e| (e["kind"].as_str().unwrap(), e["environment"].as_str())).collect();
    assert_eq!(got, [("secret.changed", Some("staging")), ("secret.changed", Some("production")), ("secret.deleted", Some("staging")), ("access.changed", Some("staging"))], "{all}");
    let change = bulk["change"]["seq"].as_i64().unwrap();
    assert_eq!(events[0]["data"]["keys"], json!(["A", "B"]));
    assert_eq!((&events[0]["data"]["versions"], &events[0]["data"]["via"], &events[0]["data"]["change"]), (&json!({ "A": 1, "B": 1 }), &json!("write"), &json!(change)));
    assert_eq!((&events[2]["key"], &events[2]["data"]["version"]), (&json!("A"), &json!(2)));
    // At its audit row's place in the chain.
    let audit = api.ok(Method::GET, "/audit?action=secrets.write", None).await;
    let row = audit.as_array().unwrap().iter().find(|r| r["summary"].as_str().unwrap().starts_with(&format!("#{change} "))).unwrap();
    assert_eq!(row["seq"], events[0]["seq"]);
    let text = all.to_string();
    assert!(![a, b, p, "changedA", "changedB", "changedP"].iter().any(|v| text.contains(v)), "{text}");

    // Filtered by family, and left out where the reader can't read.
    let kinds = api.ok(Method::GET, &format!("/events?after={start}&kinds=secret.*"), None).await;
    assert_eq!(kinds["events"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect::<Vec<_>>(), ["secret.changed", "secret.changed", "secret.deleted"]);
    let token = api.token(json!({ "environments": ["staging"], "actions": ["read"] }));
    let (_, mine) = api.req(Method::GET, &format!("/events?after={start}&kinds=secret.changed"), None, Some(&token)).await;
    assert_eq!(mine["events"].as_array().unwrap().iter().map(|e| e["environment"].as_str().unwrap()).collect::<Vec<_>>(), ["staging"], "{mine}");
}

/// Every write is an event (#793): a follower of rare kinds, of one environment, or with a grant
/// that leaves most rows out, has its cursor at the head once it has read up to it, so it never
/// walks the same rows again.
#[tokio::test]
async fn a_filtered_follower_moves_to_the_head_past_the_rows_its_filter_left_out() {
    let api = Api::new().await;
    let start = api.ok(Method::GET, "/events?latest=1", None).await["next"].as_i64().unwrap();
    for i in 0..300 {
        api.set("development", "N", &i.to_string()).await;
    }
    let head = api.ok(Method::GET, "/events?latest=1", None).await["next"].clone();
    assert!(head.as_i64().unwrap() >= start + 300, "{head}");
    for q in ["kinds=secret.leaked", "environment=staging", "kinds=secret.changed&environment=production"] {
        let page = api.ok(Method::GET, &format!("/events?after={start}&{q}"), None).await;
        assert_eq!((&page["events"], &page["next"]), (&json!([]), &head), "{q}");
    }
    let staging = api.token(json!({ "environments": ["staging"], "actions": ["read"] }));
    let (_, page) = api.req(Method::GET, &format!("/events?after={start}"), None, Some(&staging)).await;
    assert_eq!((&page["events"], &page["next"]), (&json!([]), &head), "the grant's");
    // A full page stops at its last event, short of the head.
    let page = api.ok(Method::GET, &format!("/events?after={start}&limit=10"), None).await;
    assert_eq!(page["next"], page["events"][9]["seq"]);
}

/// A change across folders (a batch whose operations name their folders) is one change at `/`: a
/// reader of one folder sees it, with that folder's keys only, never another folder's key names.
#[tokio::test]
async fn a_folder_scoped_reader_sees_a_change_across_folders_with_only_its_keys() {
    let api = Api::new().await;
    let start = api.ok(Method::GET, "/events?latest=1", None).await["next"].as_i64().unwrap();
    let ops = json!([{ "op": "set", "key": "DB_URL", "value": "postgres://app", "path": "/app" }, { "op": "set", "key": "X", "value": "x", "path": "/db" }]);
    api.ok(Method::POST, "/environments/staging/changes", Some(json!({ "ops": ops }))).await;
    let all = api.ok(Method::GET, &format!("/events?after={start}&kinds=secret.changed"), None).await;
    assert_eq!(all["events"][0]["data"]["folders"], json!({ "/app": ["DB_URL"], "/db": ["X"] }), "{all}");

    let app = api.token(json!({ "environments": ["staging"], "paths": ["/app"], "actions": ["read"] }));
    let (s, mine) = api.req(Method::GET, &format!("/events?after={start}&kinds=secret.changed"), None, Some(&app)).await;
    assert_eq!(s, StatusCode::OK, "{mine}");
    let e = &mine["events"][0];
    assert_eq!((mine["events"].as_array().unwrap().len(), &e["key"], &e["data"]["folders"], &e["data"]["versions"]), (1, &json!("DB_URL"), &json!({ "/app": ["DB_URL"] }), &json!({ "DB_URL": 1 })), "{mine}");
    let text = mine.to_string();
    assert!(!text.contains("\"X\"") && !text.contains("/db"), "{text}");
    let web = api.token(json!({ "environments": ["staging"], "paths": ["/web"], "actions": ["read"] }));
    assert_eq!(api.req(Method::GET, &format!("/events?after={start}"), None, Some(&web)).await.1["events"], json!([]));
}

#[tokio::test]
async fn a_rotation_now_is_rotation_succeeded_with_the_keys_new_version() {
    let api = Api::new().await;
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{}/resources/bucket/assets", api.project), Some(json!({ "x": 0, "y": 0 }))).await;
    let link = json!({ "kind": "bucket", "ref": "assets", "field": "secret_access_key", "mode": "managed" });
    api.ok(Method::PUT, "/environments/staging/secrets/AWS_SECRET_ACCESS_KEY", Some(json!({ "link": link }))).await;
    let start = api.ok(Method::GET, "/events?latest=1", None).await["next"].as_i64().unwrap();
    api.ok(Method::POST, "/environments/staging/secrets/AWS_SECRET_ACCESS_KEY/rotate", None).await;
    let events = api.ok(Method::GET, &format!("/events?after={start}"), None).await["events"].clone();
    let got: Vec<(&str, &Value)> = events.as_array().unwrap().iter().map(|e| (e["kind"].as_str().unwrap(), &e["data"]["via"])).collect();
    assert_eq!(got, [("secret.changed", &json!("rotation")), ("rotation.succeeded", &Value::Null)], "{events}");
    let done = &events[1];
    assert_eq!((&done["key"], &done["data"]["version"], &done["data"]["scheduled"]), (&json!("AWS_SECRET_ACCESS_KEY"), &json!(2), &json!(false)), "{done}");
    assert!(!events.to_string().contains("sk-2"));
}

/// A public ACME CA and DNS provider, as rolld's client answers: certificates signed by a root of
/// its own, an account made once (with the EAB it was given), names under `roll-fake.test` only,
/// and `unproven.…` names never proven.
struct FakeCa {
    root: rcgen::CertificateParams,
    root_key: rcgen::KeyPair,
    root_pem: String,
    orders: Mutex<Vec<(Vec<String>, bool)>>,
}

impl FakeCa {
    fn new() -> FakeCa {
        let root_key = rcgen::KeyPair::generate().unwrap();
        let mut root = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        root.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        root.distinguished_name.push(rcgen::DnType::CommonName, "roll fake public root");
        let root_pem = root.self_signed(&root_key).unwrap().pem();
        FakeCa { root, root_key, root_pem, orders: Mutex::default() }
    }
}

impl roll_secrets::public_certs::PublicCa for FakeCa {
    fn allows(&self, directory: &str) -> bool {
        roll_secrets::public_certs::PUBLIC_CAS.contains(&directory)
    }

    fn check(&self, _: roll_secrets::public_certs::DnsProvider, credential: &Secret, names: &[String]) -> Result<(), String> {
        if credential.expose() != "fake-cf-dns-token" {
            return Err("Cloudflare: find zone: HTTP 401".into());
        }
        match names.iter().find(|n| !n.ends_with("roll-fake.test")) {
            Some(n) => Err(format!("Cloudflare: no zone for {n} (does the token see it?)")),
            None => Ok(()),
        }
    }

    fn order(&self, o: roll_secrets::public_certs::Order<'_>) -> Result<roll_secrets::public_certs::Ordered, String> {
        self.orders.lock().unwrap().push((o.names.to_vec(), o.account.is_some()));
        assert_eq!(o.credential.expose(), "fake-cf-dns-token");
        if let Some(n) = o.names.iter().find(|n| n.starts_with("unproven.")) {
            return Err(format!("the CA refused: unauthorized No TXT record found at _acme-challenge.{n}"));
        }
        let account = match o.account {
            Some(a) => {
                assert_eq!(a.expose(), r#"{"fake":"account"}"#, "the account made on the first order");
                None
            }
            None => {
                let (kid, key) = o.eab.expect("EAB for a new account");
                assert_eq!((kid, key.expose()), ("kid-roll-fake", "cm9sbC1mYWtlLWhtYWM"));
                Some(Secret::new(r#"{"fake":"account"}"#.to_string()))
            }
        };
        let key = rcgen::KeyPair::generate().unwrap();
        let mut p = rcgen::CertificateParams::new(o.names.to_vec()).unwrap();
        let now = time::OffsetDateTime::now_utc();
        (p.not_before, p.not_after) = (now - time::Duration::hours(1), now + time::Duration::days(90));
        let leaf = p.signed_by(&key, &rcgen::Issuer::from_params(&self.root, &self.root_key)).unwrap();
        Ok(roll_secrets::public_certs::Ordered { chain: Secret::new(format!("{}{}", leaf.pem(), self.root_pem)), private_key: Secret::new(key.serialize_pem()), account })
    }
}

#[tokio::test]
async fn public_certificates_are_ordered_in_the_background_renewed_and_a_failed_renewal_is_rotation_failed() {
    let api = Api::new().await;
    let ca = Arc::new(FakeCa::new());
    api.secrets.set_public_ca(ca.clone());
    let le = roll_secrets::public_certs::PUBLIC_CAS[0];
    let new = |domains: Value, token: &str| json!({ "name": "web", "directory_url": le, "domains": domains, "dns_provider": "cloudflare", "dns_credential": token, "eab_kid": "kid-roll-fake", "eab_hmac_key": "cm9sbC1mYWtlLWhtYWM" });
    // Refused before anything is kept: a CA roll doesn't know, a name the token can't edit.
    let mut other = new(json!(["www.roll-fake.test"]), "fake-cf-dns-token");
    other["directory_url"] = json!("https://acme.attacker.example/directory");
    assert_eq!(api.call(Method::POST, "/public-certs", Some(other)).await.0, StatusCode::BAD_REQUEST);
    let (s, e) = api.call(Method::POST, "/public-certs", Some(new(json!(["www.example.org"]), "fake-cf-dns-token"))).await;
    assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains("no zone for www.example.org"), "{e}");
    let cert = api.ok(Method::POST, "/public-certs", Some(new(json!(["www.roll-fake.test", "*.roll-fake.test"]), "fake-cf-dns-token"))).await;
    let id = cert["id"].as_str().unwrap().to_string();
    assert_eq!((cert["status"].as_str(), ca.orders.lock().unwrap().len()), (Some("pending"), 0), "nothing ordered in the request: {cert}");
    assert!(!cert.to_string().contains("fake-cf-dns-token") && !cert.to_string().contains("cm9sbC1mYWtl"), "{cert}");
    let link = |field: &str| json!({ "link": { "kind": "public_cert", "ref": id, "field": field, "mode": "managed" } });
    let (s, e) = api.call(Method::PUT, "/environments/production/secrets/TLS_CERT", Some(link("certificate"))).await;
    assert!(s == StatusCode::CONFLICT && e.to_string().contains("not_issued"), "not issued yet: {s} {e}");

    // The background job orders it (a new account, with the EAB), and the key links to it.
    let sweep = || {
        let s = api.secrets.clone();
        async move { tokio::task::spawn_blocking(move || s.sweep_public_certs().unwrap()).await.unwrap() }
    };
    sweep().await;
    let listed = api.ok(Method::GET, "/public-certs", None).await;
    assert_eq!(listed[0]["status"], "issued", "{listed}");
    let days = (listed[0]["renews_at"].as_i64().unwrap() - listed[0]["issued_at"].as_i64().unwrap()) / 86_400_000;
    assert_eq!(days, 60, "two thirds of 90 days");
    for (key, field) in [("TLS_CERT", "certificate"), ("TLS_KEY", "private_key"), ("TLS_CA_CHAIN", "ca_chain"), ("TLS_FULLCHAIN", "fullchain")] {
        api.ok(Method::PUT, &format!("/environments/production/secrets/{key}"), Some(link(field))).await;
    }
    let (s, _) = api.call(Method::PUT, "/environments/staging/secrets/TLS_CERT", Some(json!({ "link": { "kind": "public_cert", "ref": id, "field": "certificate", "mode": "managed", "options": { "rotate_days": 30 } } }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "the registration renews, not the link");
    let v = api.values("production", "/").await["values"].clone();
    assert_eq!(v["TLS_CA_CHAIN"], ca.root_pem.as_str(), "chains to the CA's root");
    assert_eq!(v["TLS_FULLCHAIN"].as_str().unwrap(), format!("{}{}", v["TLS_CERT"].as_str().unwrap(), ca.root_pem));
    assert!(v["TLS_KEY"].as_str().unwrap().contains("PRIVATE KEY"));
    let first = v["TLS_CERT"].as_str().unwrap().to_string();
    let listed = api.ok(Method::GET, "/public-certs", None).await;
    assert!(!listed.to_string().contains("PRIVATE KEY"), "the key only through the reveal routes: {listed}");
    let health = api.ok(Method::GET, "/health", None).await;
    assert!(!health.to_string().contains("TLS_CERT"), "nothing to say yet: {health}");

    // Renewal at rotate_days: the job orders again (the same account) and every linked key rotates.
    let start = api.ok(Method::GET, "/events?latest=1", None).await["next"].as_i64().unwrap();
    let p = api.project.clone();
    api.db.write(move |c| c.execute("UPDATE public_certs SET issued_at = issued_at - 61 * 86400000 WHERE project_id = ?", [p.as_str()])).unwrap();
    sweep().await;
    let v = api.values("production", "/").await["values"].clone();
    assert_ne!(v["TLS_CERT"].as_str().unwrap(), first, "renewed");
    assert_eq!(*ca.orders.lock().unwrap().last().unwrap(), (vec!["www.roll-fake.test".to_string(), "*.roll-fake.test".to_string()], true));
    let events = api.ok(Method::GET, &format!("/events?after={start}&kinds=rotation.succeeded"), None).await;
    assert_eq!(events["events"].as_array().unwrap().len(), 1, "one credential, four keys: {events}");
    assert_eq!(events["events"][0]["data"]["sharing"].as_array().unwrap().len(), 4, "{events}");

    // A key's rotation asks for a new certificate; the job brings it.
    let r = api.ok(Method::POST, "/environments/production/secrets/TLS_KEY/rotate", None).await;
    assert!(r["warnings"][0].as_str().unwrap().contains("ordering a new certificate"), "{r}");
    let before = api.values("production", "/").await["values"]["TLS_KEY"].clone();
    sweep().await;
    assert_ne!(api.values("production", "/").await["values"]["TLS_KEY"], before);

    // A name the DNS records don't prove: the renewal fails, as rotation.failed on its keys and in
    // health, and the certificate in use stays.
    let in_use = api.values("production", "/").await["values"]["TLS_CERT"].clone();
    let start = api.ok(Method::GET, "/events?latest=1", None).await["next"].as_i64().unwrap();
    let changed = api.ok(Method::PATCH, &format!("/public-certs/{id}"), Some(json!({ "domains": ["www.roll-fake.test", "unproven.roll-fake.test"] }))).await;
    assert_eq!(changed["renews_at"].as_i64(), Some(0), "due at once: {changed}");
    sweep().await;
    let events = api.ok(Method::GET, &format!("/events?after={start}&kinds=rotation.failed"), None).await;
    assert_eq!(events["events"].as_array().unwrap().len(), 4, "{events}");
    assert!(events.to_string().contains("No TXT record found at _acme-challenge.unproven.roll-fake.test"), "{events}");
    let health = api.ok(Method::GET, "/health", None).await;
    assert!(health.as_array().unwrap().iter().any(|f| f["issue"] == "rotation_failing" && f["key"] == "TLS_CERT"), "{health}");
    assert_eq!(api.ok(Method::GET, "/public-certs", None).await[0]["status"], "failing");
    assert_eq!(api.values("production", "/").await["values"]["TLS_CERT"], in_use);
    // Linked keys hold it.
    assert_eq!(api.call(Method::DELETE, &format!("/public-certs/{id}"), None).await.0, StatusCode::CONFLICT);
}
