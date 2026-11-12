//! Sync kinds for hosting platforms (docs/secrets.md, "Syncs"): `vercel` (a project's environment
//! variables), `cloudflare` (a Worker's secrets) and `aws-ssm` (Parameter Store `SecureString`s,
//! signed with SigV4 by hand: two JSON calls don't need an SDK).
//!
//! Each platform's own credential is a secret of the sync's environment and folder, named in the
//! target (`token_key`, `access_key_id_key`…): roll-secrets resolves it at each push and never
//! pushes it ([`SyncTarget::credentials`]). The hosts are fixed public API hosts, so the client
//! needs no `PublicOnly`; it has timeouts, no redirects and a bounded read. Errors name keys and
//! the target, and from the platform only its status and error code: never a value or a token.

use super::{bad, block_on, target};
use roll_secrets::syncs::{Push, Resources, SyncField, SyncTarget};
use roll_secrets::{Error, Result, Value};
use serde_json::{Value as Json, json};
use std::time::Duration;

/// The most of a platform's answer read.
const ANSWER_MAX: usize = 1 << 20;

/// The platforms' API bases (tests point them at a mock).
#[derive(Clone)]
pub(super) struct Http {
    client: reqwest::Client,
    vercel: String,
    cloudflare: String,
    /// `None`: `https://ssm.<region>.amazonaws.com`.
    ssm: Option<String>,
}

impl Http {
    pub(super) fn new() -> Http {
        Http::with_bases("https://api.vercel.com", "https://api.cloudflare.com", None)
    }

    fn with_bases(vercel: &str, cloudflare: &str, ssm: Option<&str>) -> Http {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("roll-secrets-sync")
            .build()
            .expect("an HTTP client");
        Http { client, vercel: vercel.into(), cloudflare: cloudflare.into(), ssm: ssm.map(Into::into) }
    }

    /// Send, and read at most [`ANSWER_MAX`] of the answer as JSON (`Null` if it isn't).
    async fn send(&self, req: reqwest::RequestBuilder, what: &str) -> Result<(reqwest::StatusCode, Json)> {
        let mut resp = req.send().await.map_err(|e| Error::Link(format!("{what}: {}", e.without_url())))?;
        let status = resp.status();
        let mut body = Vec::new();
        while let Some(c) = resp.chunk().await.map_err(|e| Error::Link(format!("{what}: {}", e.without_url())))? {
            if body.len() + c.len() > ANSWER_MAX {
                return Err(Error::Link(format!("{what}: the answer is over {} KiB", ANSWER_MAX >> 10)));
            }
            body.extend_from_slice(&c);
        }
        Ok((status, serde_json::from_slice(&body).unwrap_or(Json::Null)))
    }
}

/// A credential roll-secrets resolved for this push.
fn credential<'a>(p: &'a Push<'_>, key: &str) -> Result<&'a str> {
    p.credentials.get(key).map(Value::expose).ok_or_else(|| bad(format!("{key} isn't set in {}:{}", p.environment, p.path)))
}

fn ident(s: &str, max: usize, extra: &str) -> bool {
    !s.is_empty() && s.len() <= max && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

fn key_field(kind: &str, name: &str, v: Option<String>) -> Result<String> {
    let v = v.unwrap_or_default();
    roll_secrets::names::key(&v).map_err(|_| bad(format!("a {kind} target's {name} is a key of this environment")))?;
    Ok(v)
}

/// The keys a target's credential fields name.
fn credential_keys(t: &Json, fields: &[&str]) -> Vec<String> {
    fields.iter().filter_map(|f| t[*f].as_str().map(|k| k.trim().to_string())).collect()
}

/// A value over `max` bytes, named.
fn too_long(p: &Push<'_>, max: usize, what: &str) -> Result<()> {
    match p.changed.iter().find(|n| p.values.get(*n).is_some_and(|v| v.expose().len() > max)) {
        Some(n) => Err(bad(format!("{n} is longer than {what}"))),
        None => Ok(()),
    }
}

// ── vercel ──

struct VercelTarget {
    project: String,
    team: Option<String>,
    targets: Vec<String>,
    token_key: String,
}

pub(super) struct Vercel(pub(super) Http);

impl Vercel {
    fn target(&self, t: &Json) -> Result<VercelTarget> {
        // `target` is a list, or a comma-separated string (the dashboard's form); the rest are
        // strings.
        let mut rest = t.clone();
        let targets = rest.as_object_mut().and_then(|m| m.remove("target"));
        let [project, team, token_key] = target("vercel", &rest, [("project", true), ("team", false), ("token_key", true)])?;
        let project = project.unwrap_or_default();
        if !ident(&project, 100, "-_.") || team.as_ref().is_some_and(|t| !ident(t, 100, "-_")) {
            return Err(bad("project is a Vercel project's id or name, team a team's id"));
        }
        let mut targets: Vec<String> = match targets {
            Some(Json::Array(a)) => a.into_iter().map(|v| v.as_str().map(|s| s.trim().to_string())).collect::<Option<_>>().unwrap_or_default(),
            Some(Json::String(s)) => s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect(),
            _ => vec![],
        };
        targets.sort();
        targets.dedup();
        if targets.is_empty() || targets.iter().any(|t| !["development", "preview", "production"].contains(&t.as_str())) {
            return Err(bad("target lists production, preview and/or development"));
        }
        Ok(VercelTarget { project, team, targets, token_key: key_field("vercel", "token_key", token_key)? })
    }
}

impl SyncTarget for Vercel {
    fn kind(&self) -> &str {
        "vercel"
    }
    fn label(&self) -> &str {
        "Vercel environment variables"
    }
    fn fields(&self) -> &[SyncField] {
        const F: &[SyncField] = &[
            SyncField { name: "project", label: "Vercel project (id or name)", required: true, placeholder: "shop-web" },
            SyncField { name: "team", label: "Team id (optional)", required: false, placeholder: "team_…" },
            SyncField { name: "target", label: "Vercel environments", required: true, placeholder: "production, preview" },
            SyncField { name: "token_key", label: "Key holding the Vercel token", required: true, placeholder: "VERCEL_TOKEN" },
        ];
        F
    }
    fn credentials(&self, t: &Json) -> Vec<String> {
        credential_keys(t, &["token_key"])
    }
    fn check(&self, _: &str, _: &Resources, t: &Json) -> Result<()> {
        self.target(t).map(|_| ())
    }
    fn push(&self, t: &Json, p: &Push<'_>) -> Result<()> {
        let t = self.target(t)?;
        let token = credential(p, &t.token_key)?;
        too_long(p, 64 * 1024, "Vercel's 64 KB")?;
        let what = format!("vercel {}", t.project);
        let project = urlencoding::encode(&t.project);
        let env = format!("{}/v10/projects/{project}/env", self.0.vercel);
        // `?teamId=…` (checked: an identifier) and anything after it.
        let query = |extra: &[&str]| {
            let parts: Vec<String> = t.team.iter().map(|id| format!("teamId={id}")).chain(extra.iter().map(|e| e.to_string())).collect();
            if parts.is_empty() { String::new() } else { format!("?{}", parts.join("&")) }
        };
        let code = |v: &Json| v["error"]["code"].as_str().unwrap_or_default().to_string();
        block_on(async {
            for names in p.changed.chunks(50) {
                let body: Vec<Json> = names
                    .iter()
                    .filter_map(|n| p.values.get(n).map(|v| json!({ "key": n, "value": v.expose(), "type": "encrypted", "target": t.targets })))
                    .collect();
                let req = self.0.client.post(format!("{env}{}", query(&["upsert=true"]))).bearer_auth(token).json(&body);
                let (status, v) = self.0.send(req, &what).await?;
                if !status.is_success() {
                    return Err(Error::Link(format!("{what}: {status} {}", code(&v))));
                }
            }
            if p.removed.is_empty() {
                return Ok(());
            }
            // Deleting takes the variable's id. Only variables of this sync's environments go:
            // one that also serves another environment is the team's own.
            let (status, v) = self.0.send(self.0.client.get(format!("{env}{}", query(&[]))).bearer_auth(token), &what).await?;
            if !status.is_success() {
                return Err(Error::Link(format!("{what}: listing: {status} {}", code(&v))));
            }
            for var in v["envs"].as_array().into_iter().flatten() {
                let (Some(id), Some(key)) = (var["id"].as_str(), var["key"].as_str()) else { continue };
                let ours = var["target"].as_array().is_some_and(|ts| ts.iter().all(|x| x.as_str().is_some_and(|x| t.targets.iter().any(|y| y == x))));
                if !ours || !p.removed.iter().any(|r| r == key) {
                    continue;
                }
                let url = format!("{}/v9/projects/{project}/env/{}{}", self.0.vercel, urlencoding::encode(id), query(&[]));
                let (status, v) = self.0.send(self.0.client.delete(url).bearer_auth(token), &what).await?;
                if !status.is_success() && status != reqwest::StatusCode::NOT_FOUND {
                    return Err(Error::Link(format!("{what}: deleting {key}: {status} {}", code(&v))));
                }
            }
            Ok(())
        })
    }
}

// ── cloudflare ──

pub(super) struct Cloudflare(pub(super) Http);

impl Cloudflare {
    /// (account, script, token_key)
    fn target(&self, t: &Json) -> Result<(String, String, String)> {
        let [account, script, token_key] = target("cloudflare", t, [("account", true), ("script", true), ("token_key", true)])?;
        let (account, script) = (account.unwrap_or_default(), script.unwrap_or_default());
        if !ident(&account, 64, "") || !ident(&script, 64, "-_") {
            return Err(bad("account is a Cloudflare account id, script a Worker's name"));
        }
        Ok((account, script, key_field("cloudflare", "token_key", token_key)?))
    }
}

/// Cloudflare's error codes, without their messages.
fn cf_codes(v: &Json) -> String {
    v["errors"].as_array().into_iter().flatten().filter_map(|e| e["code"].as_i64()).map(|c| c.to_string()).collect::<Vec<_>>().join(",")
}

impl SyncTarget for Cloudflare {
    fn kind(&self) -> &str {
        "cloudflare"
    }
    fn label(&self) -> &str {
        "Cloudflare Workers secrets"
    }
    fn fields(&self) -> &[SyncField] {
        const F: &[SyncField] = &[
            SyncField { name: "account", label: "Account id", required: true, placeholder: "023e105f4ecef8ad9ca31a8372d0c353" },
            SyncField { name: "script", label: "Worker", required: true, placeholder: "shop-api" },
            SyncField { name: "token_key", label: "Key holding the API token", required: true, placeholder: "CLOUDFLARE_API_TOKEN" },
        ];
        F
    }
    fn credentials(&self, t: &Json) -> Vec<String> {
        credential_keys(t, &["token_key"])
    }
    fn check(&self, _: &str, _: &Resources, t: &Json) -> Result<()> {
        self.target(t).map(|_| ())
    }
    fn push(&self, t: &Json, p: &Push<'_>) -> Result<()> {
        let (account, script, token_key) = self.target(t)?;
        let token = credential(p, &token_key)?;
        too_long(p, 5 * 1024, "a Worker secret's 5 KB")?;
        let what = format!("cloudflare {script}");
        let base = format!("{}/client/v4/accounts/{account}/workers/scripts/{script}/secrets", self.0.cloudflare);
        block_on(async {
            for name in p.changed {
                let Some(v) = p.values.get(name) else { continue };
                let req = self.0.client.put(&base).bearer_auth(token).json(&json!({ "name": name, "text": v.expose(), "type": "secret_text" }));
                let (status, body) = self.0.send(req, &what).await?;
                if !status.is_success() {
                    return Err(Error::Link(format!("{what}: {name}: {status} {}", cf_codes(&body))));
                }
            }
            for name in p.removed {
                let req = self.0.client.delete(format!("{base}/{}", urlencoding::encode(name))).bearer_auth(token);
                let (status, body) = self.0.send(req, &what).await?;
                if !status.is_success() && status != reqwest::StatusCode::NOT_FOUND {
                    return Err(Error::Link(format!("{what}: deleting {name}: {status} {}", cf_codes(&body))));
                }
            }
            Ok(())
        })
    }
}

// ── aws-ssm ──

struct SsmTarget {
    region: String,
    prefix: String,
    access_key_id_key: String,
    secret_access_key_key: String,
    kms_key_id: Option<String>,
}

pub(super) struct Ssm(pub(super) Http);

/// Parameter Store's standard tier.
const SSM_VALUE_MAX: usize = 4096;

impl Ssm {
    fn target(&self, t: &Json) -> Result<SsmTarget> {
        let [region, prefix, ak, sk, kms] =
            target("aws-ssm", t, [("region", true), ("prefix", true), ("access_key_id_key", true), ("secret_access_key_key", true), ("kms_key_id", false)])?;
        let region = region.unwrap_or_default();
        if !ident(&region, 32, "-") || region.chars().any(|c| c.is_ascii_uppercase()) {
            return Err(bad("region is an AWS region (us-east-1)"));
        }
        let prefix = prefix.unwrap_or_default();
        let lower = prefix.to_ascii_lowercase();
        if !prefix.starts_with('/') || !prefix.ends_with('/') || !ident(&prefix, 512, "/_.-") || prefix.contains("//") || lower.starts_with("/aws") || lower.starts_with("/ssm") {
            return Err(bad("prefix is a parameter path such as /shop/production/ (not under /aws or /ssm)"));
        }
        if kms.as_ref().is_some_and(|k| !ident(k, 2048, "-_:/.")) {
            return Err(bad("kms_key_id is a KMS key's id, alias or ARN"));
        }
        Ok(SsmTarget {
            region,
            prefix,
            access_key_id_key: key_field("aws-ssm", "access_key_id_key", ak)?,
            secret_access_key_key: key_field("aws-ssm", "secret_access_key_key", sk)?,
            kms_key_id: kms,
        })
    }
}

/// The `Authorization` of an AWS JSON 1.1 call to SSM (SigV4; content type, host, date and
/// target signed), with s3-proto's SigV4 primitives for the key and the string to sign.
fn sigv4(host: &str, region: &str, amz_target: &str, amz_date: &str, body: &[u8], access_key: &str, secret: &str) -> String {
    use sha2::{Digest, Sha256};
    let payload: String = Sha256::digest(body).iter().map(|b| format!("{b:02x}")).collect();
    let signed = "content-type;host;x-amz-date;x-amz-target";
    let canonical = format!("POST\n/\n\ncontent-type:application/x-amz-json-1.1\nhost:{host}\nx-amz-date:{amz_date}\nx-amz-target:{amz_target}\n\n{signed}\n{payload}");
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/ssm/aws4_request");
    let sts = s3_proto::sigv4::string_to_sign(amz_date, &scope, &canonical);
    let sig = s3_proto::sigv4::sign(&s3_proto::sigv4::signing_key(secret, date, region, "ssm"), &sts);
    format!("AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed}, Signature={sig}")
}

impl SyncTarget for Ssm {
    fn kind(&self) -> &str {
        "aws-ssm"
    }
    fn label(&self) -> &str {
        "AWS Parameter Store"
    }
    fn fields(&self) -> &[SyncField] {
        const F: &[SyncField] = &[
            SyncField { name: "region", label: "Region", required: true, placeholder: "us-east-1" },
            SyncField { name: "prefix", label: "Parameter path", required: true, placeholder: "/shop/production/" },
            SyncField { name: "access_key_id_key", label: "Key holding the IAM access key id", required: true, placeholder: "AWS_SYNC_ACCESS_KEY_ID" },
            SyncField { name: "secret_access_key_key", label: "Key holding the IAM secret key", required: true, placeholder: "AWS_SYNC_SECRET_ACCESS_KEY" },
            SyncField { name: "kms_key_id", label: "KMS key (optional)", required: false, placeholder: "alias/shop" },
        ];
        F
    }
    fn credentials(&self, t: &Json) -> Vec<String> {
        credential_keys(t, &["access_key_id_key", "secret_access_key_key"])
    }
    fn check(&self, _: &str, _: &Resources, t: &Json) -> Result<()> {
        self.target(t).map(|_| ())
    }
    fn push(&self, t: &Json, p: &Push<'_>) -> Result<()> {
        let t = self.target(t)?;
        let (ak, sk) = (credential(p, &t.access_key_id_key)?, credential(p, &t.secret_access_key_key)?);
        too_long(p, SSM_VALUE_MAX, "Parameter Store's 4 KB (standard tier)")?;
        let base = self.0.ssm.clone().unwrap_or_else(|| format!("https://ssm.{}.amazonaws.com", t.region));
        let url = url::Url::parse(&format!("{base}/")).map_err(|e| Error::Internal(e.to_string()))?;
        // What reqwest sends as `host`: the port only when it isn't the scheme's.
        let host = match url.port() {
            Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
            None => url.host_str().unwrap_or_default().to_string(),
        };
        let what = format!("aws-ssm {}", t.region);
        let call = |op: &str, body: Json| {
            let body = serde_json::to_vec(&body).expect("JSON");
            let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
            let amz_target = format!("AmazonSSM.{op}");
            let auth = sigv4(&host, &t.region, &amz_target, &amz_date, &body, ak, sk);
            self.0
                .client
                .post(url.clone())
                .header("content-type", "application/x-amz-json-1.1")
                .header("x-amz-date", amz_date)
                .header("x-amz-target", amz_target)
                .header("authorization", auth)
                .body(body)
        };
        // `__type` without its namespace (`ParameterNotFound`); never the message.
        let kind = |v: &Json| v["__type"].as_str().unwrap_or_default().rsplit('#').next().unwrap_or_default().to_string();
        block_on(async {
            for name in p.changed {
                let Some(v) = p.values.get(name) else { continue };
                let mut body = json!({ "Name": format!("{}{name}", t.prefix), "Value": v.expose(), "Type": "SecureString", "Overwrite": true, "Tier": "Standard" });
                if let Some(k) = &t.kms_key_id {
                    body["KeyId"] = json!(k);
                }
                let (status, v) = self.0.send(call("PutParameter", body), &what).await?;
                if !status.is_success() {
                    return Err(Error::Link(format!("{what}: {}{name}: {status} {}", t.prefix, kind(&v))));
                }
            }
            for name in p.removed {
                let (status, v) = self.0.send(call("DeleteParameter", json!({ "Name": format!("{}{name}", t.prefix) })), &what).await?;
                if !status.is_success() && kind(&v) != "ParameterNotFound" {
                    return Err(Error::Link(format!("{what}: deleting {}{name}: {status} {}", t.prefix, kind(&v))));
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::{HeaderMap, Method, StatusCode, Uri};
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, Mutex};

    /// Every request the mock got: method, path and query, and its JSON body.
    type Seen = Arc<Mutex<Vec<(String, String, Json)>>>;

    /// One mock for the three platforms. Vercel lists two `OLD`s: one of the sync's environment,
    /// one that also serves development (the team's own). SSM checks SigV4 with s3-proto's
    /// verifier, and has no parameter to delete.
    async fn mock() -> (String, Seen) {
        async fn any(State(seen): State<Seen>, method: Method, uri: Uri, headers: HeaderMap, body: axum::body::Bytes) -> (StatusCode, axum::Json<Json>) {
            let pq = uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default();
            seen.lock().unwrap().push((method.to_string(), pq, serde_json::from_slice(&body).unwrap_or(Json::Null)));
            if let Some(target) = headers.get("x-amz-target") {
                use sha2::{Digest, Sha256};
                let hs: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_string())).collect();
                let hs: Vec<(&str, &str)> = hs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                let req = s3_proto::sigv4::Request { method: method.as_str(), path: uri.path(), query: uri.query().unwrap_or(""), headers: &hs };
                let hash: String = Sha256::digest(&body).iter().map(|b| format!("{b:02x}")).collect();
                let verified = s3_proto::sigv4::authenticate_service(&req, chrono::Utc::now().timestamp(), "ssm", &hash, |ak| (ak == "AKIATEST").then(|| "aws-secret".to_string()));
                if !matches!(verified, Ok(Some(_))) {
                    return (StatusCode::FORBIDDEN, axum::Json(json!({ "__type": "InvalidSignatureException", "message": "no" })));
                }
                return match target.to_str().unwrap() {
                    "AmazonSSM.DeleteParameter" => (StatusCode::BAD_REQUEST, axum::Json(json!({ "__type": "com.amazonaws.ssm#ParameterNotFound", "message": "gone" }))),
                    _ => (StatusCode::OK, axum::Json(json!({ "Version": 1 }))),
                };
            }
            if method == Method::GET {
                let envs = json!({ "envs": [
                    { "id": "env_old", "key": "OLD", "target": ["production"] },
                    { "id": "env_theirs", "key": "OLD", "target": ["production", "development"] },
                ] });
                return (StatusCode::OK, axum::Json(envs));
            }
            (StatusCode::OK, axum::Json(json!({ "success": true })))
        }
        let seen: Seen = Default::default();
        let app = axum::Router::new().fallback(any).with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, seen)
    }

    /// Check `target`, then push `NEW` and remove `OLD` with `credentials`, on a blocking thread
    /// as the store does.
    async fn push(t: Arc<dyn SyncTarget>, target: Json, credentials: &[(&str, &str)]) -> Result<()> {
        let credentials: BTreeMap<String, Value> = credentials.iter().map(|(k, v)| (k.to_string(), Value::new(v.to_string()))).collect();
        tokio::task::spawn_blocking(move || {
            t.check("prj_1", &BTreeSet::new(), &target)?;
            let values = BTreeMap::from([("NEW".to_string(), Value::new("v-123".to_string()))]);
            let (changed, removed) = (vec!["NEW".to_string()], vec!["OLD".to_string()]);
            let p = Push {
                sync: "syn_1",
                project: "prj_1",
                resources: &BTreeSet::new(),
                environment: "production",
                path: "/",
                digest: "d",
                at: 0,
                values: &values,
                changed: &changed,
                removed: &removed,
                secret: None,
                credentials: &credentials,
            };
            t.push(&target, &p)
        })
        .await
        .unwrap()
    }

    fn http(base: &str) -> Http {
        Http::with_bases(base, base, Some(base))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn vercel_upserts_encrypted_and_deletes_only_its_own_environments_variables() {
        let (base, seen) = mock().await;
        let t = json!({ "project": "shop-web", "team": "team_1", "target": ["production"], "token_key": "VERCEL_TOKEN" });
        let missing = push(Arc::new(Vercel(http(&base))), t.clone(), &[]).await.unwrap_err().to_string();
        assert!(missing.contains("VERCEL_TOKEN"), "{missing}");
        push(Arc::new(Vercel(http(&base))), t, &[("VERCEL_TOKEN", "vtok")]).await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!((seen[0].0.as_str(), seen[0].1.as_str()), ("POST", "/v10/projects/shop-web/env?teamId=team_1&upsert=true"));
        assert_eq!(seen[0].2, json!([{ "key": "NEW", "value": "v-123", "type": "encrypted", "target": ["production"] }]));
        let deleted: Vec<&str> = seen.iter().filter(|s| s.0 == "DELETE").map(|s| s.1.as_str()).collect();
        assert_eq!(deleted, ["/v9/projects/shop-web/env/env_old?teamId=team_1"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cloudflare_puts_secret_text_and_deletes_removed_names() {
        let (base, seen) = mock().await;
        let t = json!({ "account": "acc1", "script": "shop-api", "token_key": "CLOUDFLARE_API_TOKEN" });
        push(Arc::new(Cloudflare(http(&base))), t, &[("CLOUDFLARE_API_TOKEN", "cftok")]).await.unwrap();
        let seen = seen.lock().unwrap();
        let path = "/client/v4/accounts/acc1/workers/scripts/shop-api/secrets";
        assert_eq!((seen[0].0.as_str(), seen[0].1.as_str(), &seen[0].2), ("PUT", path, &json!({ "name": "NEW", "text": "v-123", "type": "secret_text" })));
        assert_eq!((seen[1].0.as_str(), seen[1].1.as_str()), ("DELETE", format!("{path}/OLD").as_str()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ssm_puts_signed_secure_strings_and_a_missing_parameter_counts_as_deleted() {
        let (base, seen) = mock().await;
        let t = json!({ "region": "eu-west-1", "prefix": "/shop/production/", "access_key_id_key": "AK", "secret_access_key_key": "SK", "kms_key_id": "alias/shop" });
        push(Arc::new(Ssm(http(&base))), t.clone(), &[("AK", "AKIATEST"), ("SK", "aws-secret")]).await.expect("signed as s3-proto's verifier checks");
        let put = seen.lock().unwrap()[0].2.clone();
        assert_eq!(put, json!({ "Name": "/shop/production/NEW", "Value": "v-123", "Type": "SecureString", "Overwrite": true, "Tier": "Standard", "KeyId": "alias/shop" }));
        let wrong = push(Arc::new(Ssm(http(&base))), t, &[("AK", "AKIATEST"), ("SK", "not-it")]).await.unwrap_err().to_string();
        assert!(wrong.contains("InvalidSignatureException") && !wrong.contains("not-it") && !wrong.contains("v-123"), "{wrong}");
        let host_in_region = json!({ "region": "evil.com/x", "prefix": "/a/", "access_key_id_key": "AK", "secret_access_key_key": "SK" });
        assert!(Ssm(http(&base)).check("p", &BTreeSet::new(), &host_in_region).is_err());
    }
}
