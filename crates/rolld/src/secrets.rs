//! roll secrets in rolld (docs/secrets.md). Always on: not a product in `ROLL_PRODUCTS`.
//!
//! On a node of its own the data is `secrets.db` under `--home`; with the system database
//! (`ROLL_SYSTEM_DATABASE`) it's that database's `secrets` schema, shared by every node. The
//! deployment key is `ROLL_SECRET_KEY` (`ROLL_SECRET_KEY_PREVIOUS` while rotating it); a single
//! node without one keeps a generated key in `<home>/secrets.key`.
//!
//! Links (docs/secrets.md, "Links") are providers that call each product's own API in process,
//! as the operator, on the router of every product's routes: roll-secrets knows no product.
//! Syncs' kinds (docs/secrets.md, "Syncs") are in [`syncs`].

use axum::body::Body;
use axum::http::{Method, Request};
use http_body_util::BodyExt;
use roll_secrets::links::{Field, Issued, Link, LinkProvider, Links, Mode, Planted};
use roll_secrets::{Error, KeySource, Keyring, Secrets, Value};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use tower::ServiceExt;
mod acme;
mod clouds;
mod directory;
pub mod gateway;
mod github;

mod outside;
mod relay;
mod syncs;

/// roll search's inference endpoints keyed by a project's secret (`syncs::search`).
pub use syncs::search::router as search_inference;

/// `X-Roll-Guard` (a credential guard's scan, audited apart as `secrets.guard`) is roll MCP's
/// alone: kept only on a request its guard tool makes in process (`McpCall` with tool `guard`),
/// removed from anyone else's, so no caller can file deliberate scans under the guard's name.
pub async fn guard_header(req: Request<Body>) -> Request<Body> {
    keep_guard_header(req)
}

fn keep_guard_header(mut req: Request<Body>) -> Request<Body> {
    let from_guard = req.extensions().get::<roll_access::evaluate::McpCall>().is_some_and(|m| &*m.tool == "guard");
    if !from_guard {
        req.headers_mut().remove("x-roll-guard");
    }
    req
}

/// The router of every product's routes (no gate), set once rolld has built it.
#[derive(Clone, Default)]
pub struct Api(Arc<OnceLock<axum::Router>>);

impl Api {
    pub fn set(&self, router: axum::Router) {
        let _ = self.0.set(router);
    }

    /// Call a product's route as the operator: its status and JSON answer, whatever the status.
    /// Runs on the store's blocking threads.
    fn call_status(&self, method: Method, path: &str, body: Option<Json>) -> roll_secrets::Result<(axum::http::StatusCode, Json)> {
        self.call_status_as(method, path, body, |e| {
            e.insert(roll_auth::Operator);
        })
    }

    /// [`Api::call_status`] as whoever `who` puts on the request (`Operator`, `SignedIn`), so the
    /// product scopes the answer to them (roll search: a person's own clusters).
    fn call_status_as(&self, method: Method, path: &str, body: Option<Json>, who: impl FnOnce(&mut axum::http::Extensions)) -> roll_secrets::Result<(axum::http::StatusCode, Json)> {
        let router = self.0.get().ok_or_else(|| Error::Link("the product APIs aren't up yet".into()))?.clone();
        let mut req = Request::builder().method(method).uri(path).header("content-type", "application/json");
        req.extensions_mut().map(who);
        let req = req.body(body.map_or(Body::empty(), |b| Body::from(b.to_string()))).map_err(|e| Error::Internal(e.to_string()))?;
        tokio::runtime::Handle::current().block_on(async move {
            let resp = router.oneshot(req).await.map_err(|e| Error::Link(e.to_string()))?;
            let status = resp.status();
            let bytes = resp.into_body().collect().await.map_err(|e| Error::Link(e.to_string()))?.to_bytes();
            Ok((status, if bytes.is_empty() { Json::Null } else { serde_json::from_slice(&bytes).unwrap_or(Json::Null) }))
        })
    }

    /// POST `body` as is (`content_type`) to a product's route as the operator: its status and JSON
    /// answer (roll search's bulk takes NDJSON).
    fn post_raw(&self, path: &str, content_type: &str, body: Vec<u8>) -> roll_secrets::Result<(axum::http::StatusCode, Json)> {
        let router = self.0.get().ok_or_else(|| Error::Link("the product APIs aren't up yet".into()))?.clone();
        let mut req = Request::builder().method(Method::POST).uri(path).header("content-type", content_type);
        req.extensions_mut().map(|e| e.insert(roll_auth::Operator));
        let req = req.body(Body::from(body)).map_err(|e| Error::Internal(e.to_string()))?;
        tokio::runtime::Handle::current().block_on(async move {
            let resp = router.oneshot(req).await.map_err(|e| Error::Link(e.to_string()))?;
            let status = resp.status();
            let bytes = resp.into_body().collect().await.map_err(|e| Error::Link(e.to_string()))?.to_bytes();
            Ok((status, if bytes.is_empty() { Json::Null } else { serde_json::from_slice(&bytes).unwrap_or(Json::Null) }))
        })
    }

    /// Call a product's route as the operator; a status other than 2xx is an error naming it.
    fn call(&self, method: Method, path: &str, body: Option<Json>) -> roll_secrets::Result<Json> {
        let (status, v) = self.call_status(method.clone(), path, body)?;
        if !status.is_success() {
            // Product errors name resources, not credentials.
            let why = v.get("error").and_then(Json::as_str).unwrap_or(status.as_str()).to_string();
            return Err(Error::Link(format!("{method} {}: {} {why}", path.split('?').next().unwrap_or(path), status.as_u16())));
        }
        Ok(v)
    }

    /// GET a product's route as the operator, reading at most `limit` bytes of its answer (JSON,
    /// 2xx only): for objects whose size the caller knows, never the whole of whatever is there.
    pub(crate) fn get_bounded(&self, path: &str, limit: usize) -> roll_secrets::Result<Json> {
        let router = self.0.get().ok_or_else(|| Error::Link("the product APIs aren't up yet".into()))?.clone();
        let mut req = Request::builder().method(Method::GET).uri(path);
        req.extensions_mut().map(|e| e.insert(roll_auth::Operator));
        let req = req.body(Body::empty()).map_err(|e| Error::Internal(e.to_string()))?;
        let route = path.split('?').next().unwrap_or(path).to_string();
        tokio::runtime::Handle::current().block_on(async move {
            let resp = router.oneshot(req).await.map_err(|e| Error::Link(e.to_string()))?;
            if !resp.status().is_success() {
                return Err(Error::Link(format!("GET {route}: {}", resp.status().as_u16())));
            }
            let bytes = http_body_util::Limited::new(resp.into_body(), limit).collect().await.map_err(|_| Error::Link(format!("GET {route}: an answer over {limit} bytes")))?.to_bytes();
            serde_json::from_slice(&bytes).map_err(|_| Error::Link(format!("GET {route}: not JSON")))
        })
    }

    /// Delete a product's resource; one that's already gone (404) is done.
    fn delete_gone(&self, path: &str) -> roll_secrets::Result<()> {
        match self.call_status(Method::DELETE, path, None)? {
            (s, _) if s.is_success() || s == axum::http::StatusCode::NOT_FOUND => Ok(()),
            (s, v) => Err(Error::Link(format!("DELETE {}: {} {}", path.split('?').next().unwrap_or(path), s.as_u16(), v.get("error").and_then(Json::as_str).unwrap_or(s.as_str())))),
        }
    }
}

/// The deployment key: the one a sealed start's shares gave (`ROLL_SECRET_KEY_SEALED`),
/// `ROLL_SECRET_KEY_KMS` (a KMS ciphertext, decrypted at startup), `ROLL_SECRET_KEY`, else (one
/// node) a generated key file.
fn keyring(args: &crate::Args, home: &Path, shared: bool) -> (Result<Keyring, String>, KeySource) {
    if let Some(k) = &args.unsealed_key {
        return (Keyring::new(k, None).map_err(|e| e.to_string()), KeySource::Shares);
    }
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    if let Some(ct) = env("ROLL_SECRET_KEY_KMS") {
        let run = async {
            let conf = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;
            let key = kms_decrypt(&conf, &ct).await?;
            let previous = match env("ROLL_SECRET_KEY_KMS_PREVIOUS") {
                Some(p) => Some(kms_decrypt(&conf, &p).await?),
                None => None,
            };
            Keyring::new(key.expose(), previous.as_ref().map(Value::expose)).map_err(|e| e.to_string())
        };
        let keyring = tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(run));
        return (keyring, KeySource::Kms);
    }
    if let Some(k) = args.secret_key() {
        return (Keyring::new(&k, env("ROLL_SECRET_KEY_PREVIOUS").as_deref()).map_err(|e| e.to_string()), KeySource::Env);
    }
    if shared {
        return (Err("roll secrets needs ROLL_SECRET_KEY on every node that shares the system database".into()), KeySource::Missing);
    }
    let path = home.join("secrets.key");
    let key = match std::fs::read_to_string(&path) {
        Ok(k) => k,
        Err(_) => {
            let k = roll_secrets::names::new_id("key", 48);
            if let Err(e) = write_private(&path, &k) {
                return (Err(format!("writing {}: {e}", path.display())), KeySource::Missing);
            }
            tracing::warn!("roll secrets: no ROLL_SECRET_KEY, so a key was generated in {} (back it up, or set ROLL_SECRET_KEY)", path.display());
            k
        }
    };
    (Keyring::new(&key, None).map_err(|e| e.to_string()), KeySource::File)
}

/// The deployment key from its KMS ciphertext (base64, made with `aws kms encrypt
/// --encryption-context roll=secrets`): the context binds it to roll secrets, and the plaintext
/// lives only in memory (zeroized). Errors name KMS's code, never key material.
async fn kms_decrypt(conf: &aws_config::SdkConfig, ciphertext: &str) -> Result<Value, String> {
    use base64::Engine as _;
    let blob = base64::engine::general_purpose::STANDARD.decode(ciphertext.trim()).map_err(|_| "ROLL_SECRET_KEY_KMS isn't base64 (the CiphertextBlob of aws kms encrypt)".to_string())?;
    let out = aws_sdk_kms::Client::new(conf)
        .decrypt()
        .ciphertext_blob(aws_sdk_kms::primitives::Blob::new(blob))
        .encryption_context("roll", "secrets")
        .send()
        .await
        .map_err(|e| format!("KMS couldn't decrypt the deployment key: {}", aws_sdk_kms::error::ProvideErrorMetadata::code(&e).unwrap_or("no answer")))?;
    let plain = out.plaintext.ok_or("KMS answered without a plaintext")?.into_inner();
    String::from_utf8(plain).map(Value::new).map_err(|_| "the KMS-decrypted deployment key isn't text".to_string())
}

/// Customers' own KMS keys for their projects' data keys (docs/secrets.md, "Customer-managed
/// keys"): AWS KMS in the key's own region, with roll's credentials (the customer's key policy
/// grants roll's role), the encryption context naming roll secrets and the project. Errors name
/// KMS's code, never key material.
pub struct CustomerKms {
    conf: tokio::sync::OnceCell<aws_config::SdkConfig>,
}

impl CustomerKms {
    pub fn new(conf: Option<aws_config::SdkConfig>) -> CustomerKms {
        CustomerKms { conf: tokio::sync::OnceCell::new_with(conf) }
    }

    async fn client(&self, key: &str) -> Result<aws_sdk_kms::Client, String> {
        let conf = self.conf.get_or_init(|| aws_config::load_defaults(aws_config::BehaviorVersion::latest())).await;
        // arn:aws:kms:<region>:<account>:key/<id>: the key's own region.
        let region = key.split(':').nth(3).filter(|r| !r.is_empty()).ok_or("the key's ARN names no region")?;
        Ok(aws_sdk_kms::Client::from_conf(aws_sdk_kms::config::Builder::from(conf).region(aws_sdk_kms::config::Region::new(region.to_string())).build()))
    }

    // The store calls from its blocking threads (and nothing else may): wait for KMS there.
    fn block<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
    }
}

fn kms_code<E: aws_sdk_kms::error::ProvideErrorMetadata>(e: &E) -> String {
    e.code().unwrap_or("no answer").to_string()
}

impl roll_secrets::kms::ProjectKms for CustomerKms {
    fn encrypt(&self, key: &str, project: &str, plain: &[u8]) -> Result<Vec<u8>, String> {
        Self::block(async {
            let out = self
                .client(key)
                .await?
                .encrypt()
                .key_id(key)
                .plaintext(aws_sdk_kms::primitives::Blob::new(plain))
                .encryption_context("roll", "secrets")
                .encryption_context("project", project)
                .send()
                .await
                .map_err(|e| kms_code(&e))?;
            out.ciphertext_blob.map(|b| b.into_inner()).ok_or_else(|| "KMS answered without a ciphertext".to_string())
        })
    }

    fn decrypt(&self, key: &str, project: &str, ciphertext: &[u8]) -> Result<zeroize::Zeroizing<Vec<u8>>, String> {
        Self::block(async {
            let out = self
                .client(key)
                .await?
                .decrypt()
                .key_id(key)
                .ciphertext_blob(aws_sdk_kms::primitives::Blob::new(ciphertext))
                .encryption_context("roll", "secrets")
                .encryption_context("project", project)
                .send()
                .await
                .map_err(|e| kms_code(&e))?;
            out.plaintext.map(|b| zeroize::Zeroizing::new(b.into_inner())).ok_or_else(|| "KMS answered without a plaintext".to_string())
        })
    }
}

/// STS for dynamic AWS credentials from roles projects register (roll-secrets' aws.rs): roll's own
/// credentials assume the customer's role with the project's ExternalId. Errors are STS's code.
pub struct RollSts {
    conf: tokio::sync::OnceCell<aws_config::SdkConfig>,
}

impl RollSts {
    pub fn new(conf: Option<aws_config::SdkConfig>) -> RollSts {
        RollSts { conf: tokio::sync::OnceCell::new_with(conf) }
    }
}

impl roll_secrets::aws::Sts for RollSts {
    fn assume(&self, role_arn: &str, external_id: &str, session_name: &str, seconds: i32) -> Result<roll_secrets::aws::Session, String> {
        // The store calls from its blocking threads (and nothing else may): wait for STS there.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let conf = self.conf.get_or_init(|| aws_config::load_defaults(aws_config::BehaviorVersion::latest())).await;
                let out = aws_sdk_sts::Client::new(conf)
                    .assume_role()
                    .role_arn(role_arn)
                    .external_id(external_id)
                    .role_session_name(session_name)
                    .duration_seconds(seconds)
                    .send()
                    .await
                    .map_err(|e| aws_sdk_sts::error::ProvideErrorMetadata::code(&e).unwrap_or("no answer").to_string())?;
                let c = out.credentials.ok_or("STS answered without credentials")?;
                Ok(roll_secrets::aws::Session {
                    expires_at: c.expiration.to_millis().map_err(|_| "STS answered an expiry roll can't read")?,
                    access_key_id: Value::new(c.access_key_id),
                    secret_access_key: Value::new(c.secret_access_key),
                    session_token: Value::new(c.session_token),
                })
            })
        })
    }

    /// AssumeRoleWithWebIdentity isn't signed: AWS trusts roll's issuer's token, not a credential
    /// of roll's (none is needed on the server).
    fn assume_web_identity(&self, role_arn: &str, token: &str, session_name: &str, seconds: i32) -> Result<roll_secrets::aws::Session, String> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let conf = self.conf.get_or_init(|| aws_config::load_defaults(aws_config::BehaviorVersion::latest())).await;
                // A server with no AWS settings at all still calls STS: the role's partition's
                // global endpoint when no region is set.
                let region = conf.region().cloned().unwrap_or_else(|| {
                    aws_config::Region::from_static(match role_arn.split(':').nth(1) {
                        Some("aws-cn") => "cn-north-1",
                        Some("aws-us-gov") => "us-gov-west-1",
                        _ => "us-east-1",
                    })
                });
                let out = aws_sdk_sts::Client::from_conf(aws_sdk_sts::config::Builder::from(conf).region(region).build())
                    .assume_role_with_web_identity()
                    .role_arn(role_arn)
                    .web_identity_token(token)
                    .role_session_name(session_name)
                    .duration_seconds(seconds)
                    .send()
                    .await
                    .map_err(|e| sts_failure(&e))?;
                let c = out.credentials.ok_or("STS answered without credentials")?;
                Ok(roll_secrets::aws::Session {
                    expires_at: c.expiration.to_millis().map_err(|_| "STS answered an expiry roll can't read")?,
                    access_key_id: Value::new(c.access_key_id),
                    secret_access_key: Value::new(c.secret_access_key),
                    session_token: Value::new(c.session_token),
                })
            })
        })
    }

    fn iam_keys(&self, role_arn: &str, external_id: &str, user: &str) -> Result<Vec<String>, String> {
        let iam = self.iam(role_arn, external_id)?;
        Self::wait(async move {
            let out = iam.list_access_keys().user_name(user).send().await.map_err(|e| aws_code(&e))?;
            Ok(out.access_key_metadata.into_iter().filter_map(|k| k.access_key_id).collect())
        })
    }

    fn iam_create_key(&self, role_arn: &str, external_id: &str, user: &str) -> Result<(Value, Value), String> {
        let iam = self.iam(role_arn, external_id)?;
        Self::wait(async move {
            let k = iam.create_access_key().user_name(user).send().await.map_err(|e| aws_code(&e))?.access_key.ok_or("IAM answered without a key")?;
            Ok((Value::new(k.access_key_id), Value::new(k.secret_access_key)))
        })
    }

    fn iam_delete_key(&self, role_arn: &str, external_id: &str, user: &str, key_id: &str) -> Result<(), String> {
        let iam = self.iam(role_arn, external_id)?;
        Self::wait(async move {
            match iam.delete_access_key().user_name(user).access_key_id(key_id).send().await {
                Ok(_) => Ok(()),
                Err(e) if aws_code(&e) == "NoSuchEntity" => Ok(()),
                Err(e) => Err(aws_code(&e)),
            }
        })
    }
}

impl RollSts {
    /// The store calls from its blocking threads (and nothing else may): wait for AWS there.
    fn wait<T>(f: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
    }

    /// IAM as `role_arn`, assumed for a quarter of an hour with the project's ExternalId (IAM is
    /// global: us-east-1).
    fn iam(&self, role_arn: &str, external_id: &str) -> Result<aws_sdk_iam::Client, String> {
        use roll_secrets::aws::Sts;
        let s = self.assume(role_arn, external_id, "roll-iam", 900)?;
        let conf = Self::wait(async { Ok(self.conf.get_or_init(|| aws_config::load_defaults(aws_config::BehaviorVersion::latest())).await.clone()) })?;
        let creds = aws_sdk_iam::config::Credentials::new(s.access_key_id.expose(), s.secret_access_key.expose(), Some(s.session_token.expose().to_string()), None, "roll-secrets");
        Ok(aws_sdk_iam::Client::from_conf(aws_sdk_iam::config::Builder::from(&conf).credentials_provider(creds).region(aws_sdk_iam::config::Region::new("us-east-1")).build()))
    }
}

/// AWS's error code, never the request (it may hold a key).
/// Why an STS call failed: AWS's error code, else what kept it from answering (never the request).
fn sts_failure<E: aws_sdk_sts::error::ProvideErrorMetadata, R>(e: &aws_sdk_sts::error::SdkError<E, R>) -> String {
    use aws_sdk_sts::error::{ProvideErrorMetadata, SdkError};
    if let Some(code) = e.code() {
        return code.to_string();
    }
    match e {
        SdkError::ConstructionFailure(_) => "the request couldn't be made (is a region set?)",
        SdkError::TimeoutError(_) => "STS timed out",
        SdkError::DispatchFailure(_) => "STS couldn't be reached",
        _ => "no answer",
    }
    .to_string()
}

fn aws_code<E: aws_sdk_iam::error::ProvideErrorMetadata>(e: &E) -> String {
    e.code().unwrap_or("no answer").to_string()
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    o.mode(0o600);
    o.open(path)?.write_all(text.as_bytes())
}

/// How roll-access's trusted machine clients fetch their issuers' keys: the guarded client roll
/// secrets' identities use, private addresses only where webhooks may go to them.
pub fn oidc_fetch(args: &crate::Args, open: bool) -> Arc<dyn roll_oidc::Fetch + Send + Sync> {
    Arc::new(syncs::OidcFetch::new(args.code_webhook_allow_private.unwrap_or(open)))
}

/// Where roll secrets keeps its data: the system database's `secrets` schema, else
/// `<home>/secrets.db`.
pub fn store(args: &crate::Args, home: &Path) -> anyhow::Result<(code_sql::Target, code_sql::PgOptions)> {
    let target = match args.code.cluster.system_database.as_deref().filter(|u| !u.is_empty()) {
        Some(url) => code_sql::Target::Postgres { url: url.to_string(), schema: "secrets".into() },
        None => code_sql::Target::Sqlite(home.join("secrets.db")),
    };
    Ok((target, code_sql::PgOptions { max_connections: args.db_budget()?.secrets, ca: args.code.code_db_ca.clone(), ..Default::default() }))
}

#[allow(clippy::too_many_arguments)]
pub fn open(args: &crate::Args, home: &Path, workspace: Arc<roll_workspace::Workspace>, open: bool, api: Api, access: Arc<roll_access::Access>, people: People, gateways: Arc<gateway::Gateways>) -> anyhow::Result<Arc<Secrets>> {
    let (target, pg) = store(args, home)?;
    let (keyring, key_source) = keyring(args, home, matches!(target, code_sql::Target::Postgres { .. }));
    if let Err(why) = &keyring {
        tracing::error!("roll secrets is unavailable: {why}");
    }
    let web_url = std::env::var("ROLL_WEB_URL").ok().filter(|u| !u.trim().is_empty());
    // Syncs' webhooks go to private addresses only where roll code's may (local development).
    let allow_private = args.code_webhook_allow_private.unwrap_or(open);
    let opts = roll_secrets::Options { keyring, key_source, links: links(api.clone()), syncs: syncs::kinds(api.clone(), allow_private), open, trust_forwarded_for: args.code_trust_forwarded_for, web_url, deliver: Some(Arc::new(syncs::AuditDeliver::new(allow_private, api))),
        // Grants are looked up when a sync made by one pushes; identities' logins mint workload tokens.
        grants: Some(access.clone() as Arc<dyn roll_auth::Grants>),
        workloads: Some(Arc::new(Workloads(access.clone()))),
        people: Some(Arc::new(people)),
        kms: Some(Arc::new(CustomerKms::new(None))), sts: Some(Arc::new(RollSts::new(None))), outside: Some(Arc::new(outside::Guarded { allow_private, gateways: gateways.clone() })), directory: Some(Arc::new(directory::Guarded { allow_private, gateways: gateways.clone() })), github: Some(Arc::new(github::RollGitHub::new(allow_private))), clouds: Some(roll_secrets::clouds::Clouds { issuer: issuer_url(args), tokens: Arc::new(clouds::RollClouds::new()) }),
        // A deleted gateway, or a revoked credential that opened one, closes its link at once.
        gateways: Some(gateways.clone()),
    };
    // Opening blocks on the database.
    let secrets = Arc::new(tokio::task::block_in_place(|| Secrets::open(&target, pg, workspace.clone(), opts))?);
    gateways.set_secrets(secrets.clone());
    // How many reads of values a token or a person makes a minute on this node (0: no limit).
    if let Ok(n) = std::env::var("ROLL_SECRETS_READS_PER_MINUTE") {
        secrets.set_reads_per_minute(n.trim().parse().map_err(|_| anyhow::anyhow!("ROLL_SECRETS_READS_PER_MINUTE is a number of reads a minute (0: no limit), not {n:?}"))?);
    }
    // Where clients reach rolld: a CA's certificates name its CRL there.
    secrets.set_public_api(args.code_public_url.clone().unwrap_or_else(|| format!("http://{}:{}", args.public_host, args.api.port())));
    // How often roll's issuer key rotates (90 days unless set).
    if let Ok(days) = std::env::var("ROLL_SECRETS_OIDC_ROTATE_DAYS") {
        match days.trim().parse::<u32>() {
            Ok(d) => secrets.set_issuer_rotation_days(d),
            Err(_) => tracing::warn!("ROLL_SECRETS_OIDC_ROTATE_DAYS isn't a number of days: rotating every {}", roll_secrets::issuer::ROTATE_DAYS),
        }
    }
    // And the audit log's digest key (90 days unless set).
    if let Ok(days) = std::env::var("ROLL_SECRETS_AUDIT_KEY_ROTATE_DAYS") {
        match days.trim().parse::<u32>() {
            Ok(d) => secrets.set_audit_key_rotation_days(d),
            Err(_) => tracing::warn!("ROLL_SECRETS_AUDIT_KEY_ROTATE_DAYS isn't a number of days: rotating every {}", roll_secrets::audit_key::ROTATE_DAYS),
        }
    }
    // What AWS identities' signed requests name (X-Roll-Server-ID): ROLL_SECRETS_AWS_SERVER_ID, or
    // the configured public URL. Never the host and port made up above, which two deployments can
    // share: unset, roll-secrets derives one from the deployment key.
    if let Some(id) = std::env::var("ROLL_SECRETS_AWS_SERVER_ID").ok().filter(|v| !v.trim().is_empty()).or_else(|| args.code_public_url.clone()) {
        secrets.set_aws_server_id(id);
    }
    // Validity checks (docs/secrets.md, "Is it live?"): AWS keys are tried only when the operator
    // says so, and never a canary's.
    let aws_live = match std::env::var("ROLL_SECRETS_VALIDITY_AWS_LIVE").unwrap_or_default().trim() {
        "" | "0" | "false" | "off" => false,
        "1" | "true" | "on" => true,
        v => anyhow::bail!("ROLL_SECRETS_VALIDITY_AWS_LIVE is on or off, not {v:?}"),
    };
    let canaries = roll_secrets::validity::Canaries::parse(&std::env::var("ROLL_SECRETS_VALIDITY_CANARIES").unwrap_or_default()).map_err(|e| anyhow::anyhow!("ROLL_SECRETS_VALIDITY_CANARIES: {e}"))?;
    secrets.set_validity_settings(roll_secrets::validity::Settings { aws_live, canaries });
    tell_decoys(&access, secrets.clone());
    // ACME's http-01 and dns-01 checks: public addresses only, as webhooks.
    secrets.set_acme_checks(Arc::new(acme::Checks::new(allow_private)));
    // A member removed, demoted or gone (docs/members.md): their shared folders, one-time shares
    // and leases end once they no longer hold what those need.
    let s = secrets.clone();
    workspace.on_event(Box::new(move |e| match (e.action.as_str(), &e.subject) {
        ("member.remove" | "member.leave" | "member.role", Some(user)) => s.member_changed(&e.project, user).map_err(|e| e.to_string()),
        _ => Ok(()),
    }));
    // A deleted project's secrets go with it.
    let s = secrets.clone();
    workspace.on_delete(Box::new(move |id| {
        let s = s.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = s.forget_project(&id) {
                tracing::error!(project = %id, "forgetting a deleted project's secrets failed: {e}");
            }
        });
    }));
    // Two sweepers: work in roll (expired leases and environments, due rotations, old audit
    // rows), and deliveries outside it (notices, syncs, sinks), so a slow platform holds up neither.
    sweeper(secrets.clone(), "local", Secrets::sweep_local);
    sweeper(secrets.clone(), "outbound", Secrets::sweep_outbound);
    // Syncs a change marked due, within seconds rather than at the next 30 s sweep.
    let s = secrets.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let s = s.clone();
            match tokio::task::spawn_blocking(move || s.sweep_due_syncs()).await {
                Ok(Ok((_, failed))) if failed > 0 => tracing::warn!(failed, "roll secrets: some syncs failed"),
                Ok(Ok(_)) | Ok(Err(Error::Unavailable(_))) => {}
                Ok(Err(e)) => tracing::warn!("roll secrets: syncing failed: {e}"),
                Err(e) => tracing::warn!("roll secrets: syncing panicked: {e}"),
            }
        }
    });
    Ok(secrets)
}

/// A decoy key's token was used: roll-access tells roll secrets, which raises `decoy_used` on its
/// project (off the request's task: it writes).
fn tell_decoys(access: &roll_access::Access, secrets: Arc<Secrets>) {
    access.on_decoy(move |used| {
        let (s, used) = (secrets.clone(), used.clone());
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        rt.spawn_blocking(move || {
            if let Err(e) = s.decoy_used("rlt", &used.grant, used.ip.as_deref(), &used.route) {
                tracing::warn!(grant = %used.grant, "raising a decoy key's alert failed: {e}");
            }
        });
    });
}

/// A decoy role's login, refused by roll Postgres' proxy: roll secrets raises `decoy_used` on its
/// key (off the login's task: it writes). The address and the endpoint, never the role's name.
pub fn tell_postgres_decoys(control: &pg_control::Control, secrets: Arc<Secrets>) {
    control.on_decoy_login(move |login| {
        let (s, login) = (secrets.clone(), login.clone());
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        rt.spawn_blocking(move || {
            let ip = login.peer.map(|p| p.to_string());
            if let Err(e) = s.decoy_used("postgres", &decoy_handle(&login.project_id, &login.role), ip.as_deref(), &login.endpoint_id) {
                tracing::warn!(endpoint = %login.endpoint_id, "raising a decoy role's alert failed: {e}");
            }
        });
    });
}

/// How roll secrets names a Postgres decoy role: what it's planted as and what its logins report.
fn decoy_handle(project: &str, role: &str) -> String {
    format!("{project}/{role}")
}

/// roll's OIDC issuer for GCP and Azure (roll-secrets' issuer.rs): where clouds reach rolld.
fn issuer_url(args: &crate::Args) -> String {
    let api = args.code_public_url.clone().unwrap_or_else(|| format!("http://{}:{}", args.public_host, args.api.port()));
    format!("{}/v1/secrets/oidc", api.trim_end_matches('/'))
}

fn sweeper(s: Arc<Secrets>, what: &'static str, pass: fn(&Secrets) -> roll_secrets::Result<roll_secrets::Swept>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let s = s.clone();
            match tokio::task::spawn_blocking(move || pass(&s)).await {
                Ok(Ok(w)) if w.failed > 0 => tracing::warn!(?w, what, "roll secrets: some background work failed"),
                Ok(Err(Error::Unavailable(_))) | Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!(what, "roll secrets: sweeping failed: {e}"),
                Err(e) => tracing::warn!(what, "roll secrets: sweeping panicked: {e}"),
            }
        }
    });
}

/// The provider for a kind roll apps links with credentials of its own (a bucket's key, a
/// stream's or a NoSQL database's), over rolld's own API: issued and revoked as roll secrets'
/// managed links are (`apps::RollResources`).
pub fn link_provider(api: &Api, kind: &str) -> Option<Arc<dyn LinkProvider>> {
    match kind {
        "bucket" => Some(Arc::new(Bucket(api.clone()))),
        "stream" => Some(Arc::new(Stream(api.clone()))),
        "nosql" => Some(Arc::new(Nosql(api.clone()))),
        // A vhost user of the link's own (`AMQP_URL`), never the vhost's admin.
        "queue" => Some(Arc::new(Queue(api.clone()))),
        // A data-only key of the cluster's own (`ELASTICSEARCH_API_KEY`, or an OpenSearch-flavor
        // cluster's internal user), never its password.
        "search" => Some(Arc::new(Search(api.clone(), Default::default()))),
        _ => None,
    }
}

/// The link providers of every product rolld serves.
fn links(api: Api) -> Links {
    let mut l = Links::default();
    l.register(Arc::new(Postgres(api.clone())));
    l.register(Arc::new(Redis(api.clone())));
    l.register(Arc::new(Stream(api.clone())));
    l.register(Arc::new(Bucket(api.clone())));
    l.register(Arc::new(Nosql(api.clone())));
    l.register(Arc::new(Queue(api.clone())));
    l.register(Arc::new(Search(api, Default::default())));
    l
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn text(v: &Json, field: &str) -> roll_secrets::Result<Value> {
    v.pointer(field).and_then(Json::as_str).map(Value::new).ok_or_else(|| Error::Link(format!("the product didn't return {}", field.trim_start_matches('/'))))
}

/// A URL's parts: user, password, host, port, path without `/`.
fn url_part(u: &str, part: &str) -> roll_secrets::Result<Value> {
    let parsed = url::Url::parse(u).map_err(|_| Error::Link("the product's URL doesn't parse".into()))?;
    let s = match part {
        "host" => parsed.host_str().unwrap_or_default().to_string(),
        "port" => parsed.port_or_known_default().map(|p| p.to_string()).unwrap_or_default(),
        "user" => url::form_urlencoded::parse(parsed.username().as_bytes()).map(|(k, _)| k.into_owned()).next().unwrap_or_default(),
        "password" => parsed.password().map(|p| url::form_urlencoded::parse(p.as_bytes()).map(|(k, _)| k.into_owned()).collect::<String>()).unwrap_or_default(),
        "database" => parsed.path().trim_start_matches('/').to_string(),
        _ => return Err(Error::BadRequest(format!("no URL part {part}"))),
    };
    Ok(Value::new(s))
}

/// Random lowercase letters and digits for names and passwords the provider makes.
fn random(n: usize) -> String {
    roll_secrets::names::new_id("x", n)[2..].to_string()
}

const LIVE: &[Mode] = &[Mode::Live];
const ISSUED: &[Mode] = &[Mode::Managed, Mode::Dynamic];

const fn live(field: &'static str, label: &'static str, secret: bool, suggest: &'static str) -> Field {
    Field { field, label, modes: LIVE, secret, suggest, credential: None }
}

const fn issued(field: &'static str, label: &'static str, secret: bool, suggest: &'static str, credential: &'static str) -> Field {
    Field { field, label, modes: ISSUED, secret, suggest, credential: Some(credential) }
}

/// Postgres: the branch's connection string (live), and roles of their own (managed or dynamic,
/// made with `CREATE ROLE … VALID UNTIL` through the console's query route). Option `branch`
/// (default `main`); option `permission` `read` or `write` (default) for a role.
struct Postgres(Api);

impl Postgres {
    fn branch(&self, project: &str, link: &Link) -> roll_secrets::Result<Json> {
        let p = self.0.call(Method::GET, &format!("/v1/projects/{}", enc(project)), None)?;
        let want = link.options.get("branch").and_then(Json::as_str).unwrap_or("main");
        p["branches"].as_array().into_iter().flatten().find(|b| b["name"] == want || b["id"] == want).cloned().ok_or_else(|| Error::Link(format!("postgres {project} has no branch {want}")))
    }
}

impl LinkProvider for Postgres {
    fn kind(&self) -> &str {
        "postgres"
    }
    fn fields(&self) -> &[Field] {
        const F: &[Field] = &[
            live("url", "Connection string", true, "DATABASE_URL"),
            live("host", "Host", false, "PGHOST"),
            live("port", "Port", false, "PGPORT"),
            live("user", "User", false, "PGUSER"),
            live("password", "Password", true, "PGPASSWORD"),
            live("database", "Database", false, "PGDATABASE"),
            issued("role_url", "Connection string for a role of its own", true, "DATABASE_URL", "role"),
            issued("role_user", "Role name", false, "PGUSER", "role"),
            issued("role_password", "Role password", true, "PGPASSWORD", "role"),
        ];
        F
    }
    fn suggested(&self) -> &[(&'static str, Mode)] {
        // A role of the app's own, never the project's password.
        &[("role_url", Mode::Managed)]
    }
    fn name(&self, r#ref: &str) -> Option<String> {
        self.0.call(Method::GET, &format!("/v1/projects/{}", enc(r#ref)), None).ok()?["name"].as_str().map(Into::into)
    }
    fn read(&self, r#ref: &str, field: &str, link: &Link) -> roll_secrets::Result<Value> {
        let b = self.branch(r#ref, link)?;
        let uri = b["connection_uri"].as_str().ok_or_else(|| Error::Link(format!("postgres {ref}'s branch has no endpoint")))?;
        match field {
            "url" => Ok(Value::new(uri)),
            part => url_part(uri, part),
        }
    }
    fn issue(&self, r#ref: &str, _: &str, link: &Link, _: &str, ttl: Option<u64>) -> roll_secrets::Result<Issued> {
        let b = self.branch(r#ref, link)?;
        let uri = b["connection_uri"].as_str().ok_or_else(|| Error::Link(format!("postgres {ref}'s branch has no endpoint")))?.to_string();
        let endpoint = b["endpoint"]["id"].as_str().ok_or_else(|| Error::Link(format!("postgres {ref}'s branch has no endpoint")))?.to_string();
        // Only letters and digits we made: nothing to escape.
        let role = format!("roll_{}", random(12));
        let password = random(32);
        let until = match ttl {
            Some(m) => format!("VALID UNTIL '{}'", chrono::Utc::now().checked_add_signed(chrono::Duration::minutes(m as i64 + 5)).unwrap_or_else(chrono::Utc::now).to_rfc3339()),
            None => String::new(),
        };
        let grants = match link.options.get("permission").and_then(Json::as_str) {
            Some("read") => "GRANT pg_read_all_data TO {r};",
            _ => "GRANT pg_read_all_data, pg_write_all_data TO {r}; GRANT ALL ON SCHEMA public TO {r};",
        }
        .replace("{r}", &role);
        // The SCRAM verifier, not the password: a statement that fails is logged by the compute.
        let sql = format!("CREATE ROLE {role} LOGIN PASSWORD '{}' {until}; {grants}", scram_verifier(&password));
        self.0.call(Method::POST, &format!("/v1/endpoints/{}/query", enc(&endpoint)), Some(json!({ "sql": sql })))?;
        let mut url = url::Url::parse(&uri).map_err(|_| Error::Link("the branch's URL doesn't parse".into()))?;
        let _ = url.set_username(&role);
        let _ = url.set_password(Some(&password));
        let parts = BTreeMap::from([
            ("role_url".to_string(), Value::new(url.to_string())),
            ("role_user".to_string(), Value::new(role.clone())),
            ("role_password".to_string(), Value::new(password)),
        ]);
        Ok(Issued { parts, handle: json!({ "role": role, "endpoint": endpoint }) })
    }
    /// A decoy role (docs/secrets.md, "Decoys"): named and passworded as `issue`'s roles are, in
    /// the root branch's connection string. roll Postgres keeps no password for it: the proxy
    /// refuses every login, as a wrong password, and reports it ([`tell_postgres_decoys`]).
    fn plant_decoy(&self, r#ref: &str) -> roll_secrets::Result<Planted> {
        let p = self.0.call(Method::GET, &format!("/v1/projects/{}", enc(r#ref)), None)?;
        let root = p["branches"].as_array().into_iter().flatten().find(|b| b["parent_id"].is_null());
        let uri = root.and_then(|b| b["connection_uri"].as_str()).ok_or_else(|| Error::Link(format!("postgres {ref} has no endpoint")))?;
        let mut url = url::Url::parse(uri).map_err(|_| Error::Link("the branch's URL doesn't parse".into()))?;
        let role = format!("roll_{}", random(12));
        self.0.call(Method::POST, &format!("/v1/projects/{}/decoy-roles", enc(r#ref)), Some(json!({ "role": role })))?;
        let _ = url.set_username(&role);
        let _ = url.set_password(Some(&random(32)));
        Ok(Planted { value: Value::new(url.to_string()), handle: decoy_handle(r#ref, &role) })
    }
    fn remove_decoy(&self, handle: &str) -> roll_secrets::Result<()> {
        let (project, role) = handle.split_once('/').ok_or_else(|| Error::Internal("a postgres decoy's handle is project/role".into()))?;
        self.0.delete_gone(&format!("/v1/projects/{}/decoy-roles/{}", enc(project), enc(role)))
    }
    /// A renewed lease: the role's `VALID UNTIL` follows (with the same 5 minutes of slack).
    fn renew(&self, _: &str, h: &Json, until: i64) -> roll_secrets::Result<()> {
        let (Some(role), Some(endpoint)) = (h["role"].as_str(), h["endpoint"].as_str()) else { return Ok(()) };
        if !role.starts_with("roll_") || !role[5..].bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) {
            return Err(Error::Internal("a lease names a role roll didn't make".into()));
        }
        let at = chrono::DateTime::from_timestamp_millis(until + 5 * 60_000).ok_or_else(|| Error::BadRequest("a time out of range".into()))?.to_rfc3339();
        self.0.call(Method::POST, &format!("/v1/endpoints/{}/query", enc(endpoint)), Some(json!({ "sql": format!("ALTER ROLE {role} VALID UNTIL '{at}'") }))).map(|_| ())
    }
    /// Ends the role's sessions and drops it. What it made (tables from a migration) is given to
    /// `roll` first, so revoking a credential never drops data.
    fn revoke(&self, _: &str, h: &Json) -> roll_secrets::Result<()> {
        let (Some(role), Some(endpoint)) = (h["role"].as_str(), h["endpoint"].as_str()) else { return Ok(()) };
        if !role.starts_with("roll_") || !role[5..].bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) {
            return Err(Error::Internal("a lease names a role roll didn't make".into()));
        }
        let sql = format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = '{role}'; \
             DO $$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role}') THEN EXECUTE 'REASSIGN OWNED BY {role} TO CURRENT_USER'; EXECUTE 'DROP OWNED BY {role}'; EXECUTE 'DROP ROLE {role}'; END IF; END $$;"
        );
        self.0.call(Method::POST, &format!("/v1/endpoints/{}/query", enc(endpoint)), Some(json!({ "sql": sql }))).map(|_| ())
    }
}

/// A SCRAM-SHA-256 verifier (RFC 5802/7677, as Postgres stores it): Postgres takes it as a
/// role's password, so the password itself never reaches the database.
fn scram_verifier(password: &str) -> String {
    use base64::Engine as _;
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};
    const ITERATIONS: u32 = 4096;
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).expect("system randomness");
    let mut salted = [0u8; 32];
    pbkdf2::pbkdf2::<Hmac<Sha256>>(password.as_bytes(), &salt, ITERATIONS, &mut salted).expect("a valid output length");
    let mac = |key: &[u8], msg: &[u8]| {
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("any key length");
        m.update(msg);
        m.finalize().into_bytes()
    };
    let client = mac(&salted, b"Client Key");
    let stored = Sha256::digest(client);
    let server = mac(&salted, b"Server Key");
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    format!("SCRAM-SHA-256${ITERATIONS}:{}${}:{}", b64(&salt), b64(&stored), b64(&server))
}

/// Redis: the database's URLs and token (live); rotating resets its password.
struct Redis(Api);

impl LinkProvider for Redis {
    fn kind(&self) -> &str {
        "redis"
    }
    fn fields(&self) -> &[Field] {
        const F: &[Field] = &[
            live("url", "redis:// URL", true, "REDIS_URL"),
            live("host", "Host", false, "REDIS_HOST"),
            live("port", "Port", false, "REDIS_PORT"),
            live("password", "Password", true, "REDIS_PASSWORD"),
            live("rest_url", "REST URL", false, "UPSTASH_REDIS_REST_URL"),
            live("rest_token", "REST token", true, "UPSTASH_REDIS_REST_TOKEN"),
        ];
        F
    }
    fn suggested(&self) -> &[(&'static str, Mode)] {
        &[("url", Mode::Live)]
    }
    fn name(&self, r#ref: &str) -> Option<String> {
        self.0.call(Method::GET, &format!("/v1/redis/databases/{}", enc(r#ref)), None).ok()?["name"].as_str().map(Into::into)
    }
    fn read(&self, r#ref: &str, field: &str, _: &Link) -> roll_secrets::Result<Value> {
        let d = self.0.call(Method::GET, &format!("/v1/redis/databases/{}", enc(r#ref)), None)?;
        match field {
            "url" => text(&d, "/redis_url"),
            "host" => text(&d, "/host"),
            "port" => Ok(Value::new(d["port"].to_string())),
            "password" => url_part(text(&d, "/redis_url")?.expose(), "password"),
            "rest_url" => text(&d, "/rest_url"),
            "rest_token" => text(&d, "/rest_token"),
            f => Err(Error::BadRequest(format!("redis has no field {f}"))),
        }
    }
    fn rotate_source(&self, r#ref: &str) -> roll_secrets::Result<bool> {
        self.0.call(Method::POST, &format!("/v1/redis/databases/{}/password", enc(r#ref)), Some(json!({}))).map(|_| true)
    }
}

/// Streams: the bootstrap servers (live), and API keys of their own (managed or dynamic).
struct Stream(Api);

impl LinkProvider for Stream {
    fn kind(&self) -> &str {
        "stream"
    }
    fn fields(&self) -> &[Field] {
        const F: &[Field] = &[
            live("bootstrap", "Bootstrap servers", false, "KAFKA_BROKERS"),
            live("security_protocol", "Security protocol", false, "KAFKA_SECURITY_PROTOCOL"),
            issued("username", "SASL username", false, "KAFKA_USERNAME", "key"),
            issued("password", "SASL password", true, "KAFKA_PASSWORD", "key"),
            issued("properties", "Client properties", true, "KAFKA_PROPERTIES", "key"),
        ];
        F
    }
    fn suggested(&self) -> &[(&'static str, Mode)] {
        &[("bootstrap", Mode::Live), ("username", Mode::Managed), ("password", Mode::Managed)]
    }
    fn name(&self, r#ref: &str) -> Option<String> {
        self.0.call(Method::GET, &format!("/v1/streams/{}", enc(r#ref)), None).ok()?["name"].as_str().map(Into::into)
    }
    fn read(&self, r#ref: &str, field: &str, _: &Link) -> roll_secrets::Result<Value> {
        let s = self.0.call(Method::GET, &format!("/v1/streams/{}", enc(r#ref)), None)?;
        text(&s, &format!("/{field}"))
    }
    fn issue(&self, r#ref: &str, _: &str, _: &Link, label: &str, _: Option<u64>) -> roll_secrets::Result<Issued> {
        let k = self.0.call(Method::POST, &format!("/v1/streams/{}/keys", enc(r#ref)), Some(json!({ "label": label.chars().take(64).collect::<String>() })))?;
        let username = text(&k, "/username")?;
        let parts = BTreeMap::from([
            ("username".to_string(), username.clone()),
            ("password".to_string(), text(&k, "/secret")?),
            ("properties".to_string(), text(&k, "/connect/properties")?),
        ]);
        Ok(Issued { parts, handle: json!({ "username": username.expose() }) })
    }
    fn revoke(&self, r#ref: &str, h: &Json) -> roll_secrets::Result<()> {
        let Some(u) = h["username"].as_str() else { return Ok(()) };
        self.0.delete_gone(&format!("/v1/streams/{}/keys/{}", enc(r#ref), enc(u)))
    }
}

/// Buckets: the S3 endpoint, region and name (live), and access keys limited to the bucket
/// (managed or dynamic). Option `permission`: `read`, `write` (default) or `admin`.
struct Bucket(Api);

impl LinkProvider for Bucket {
    fn kind(&self) -> &str {
        "bucket"
    }
    fn fields(&self) -> &[Field] {
        const F: &[Field] = &[
            live("bucket", "Bucket name", false, "S3_BUCKET"),
            live("endpoint", "S3 endpoint", false, "AWS_ENDPOINT_URL"),
            live("region", "Region", false, "AWS_REGION"),
            issued("access_key_id", "Access key id", false, "AWS_ACCESS_KEY_ID", "key"),
            issued("secret_access_key", "Secret access key", true, "AWS_SECRET_ACCESS_KEY", "key"),
        ];
        F
    }
    fn suggested(&self) -> &[(&'static str, Mode)] {
        &[("bucket", Mode::Live), ("endpoint", Mode::Live), ("region", Mode::Live), ("access_key_id", Mode::Managed), ("secret_access_key", Mode::Managed)]
    }
    fn read(&self, r#ref: &str, field: &str, _: &Link) -> roll_secrets::Result<Value> {
        if field == "bucket" {
            return Ok(Value::new(r#ref));
        }
        let s = self.0.call(Method::GET, "/v1/storage", None)?;
        text(&s, &format!("/{field}"))
    }
    fn issue(&self, r#ref: &str, _: &str, link: &Link, label: &str, _: Option<u64>) -> roll_secrets::Result<Issued> {
        let permission = link.options.get("permission").and_then(Json::as_str).unwrap_or("write");
        let k = self.0.call(Method::POST, "/v1/storage/keys", Some(json!({ "name": label.chars().take(64).collect::<String>(), "permission": permission, "buckets": [r#ref] })))?;
        let id = text(&k, "/id")?;
        let parts = BTreeMap::from([("access_key_id".to_string(), id.clone()), ("secret_access_key".to_string(), text(&k, "/secret_access_key")?)]);
        Ok(Issued { parts, handle: json!({ "id": id.expose() }) })
    }
    fn revoke(&self, _: &str, h: &Json) -> roll_secrets::Result<()> {
        let Some(id) = h["id"].as_str() else { return Ok(()) };
        self.0.delete_gone(&format!("/v1/storage/keys/{}", enc(id)))
    }
}

/// NoSQL: the CQL endpoint and credentials, the Alternator endpoint (live); rotating resets the
/// password.
struct Nosql(Api);

impl LinkProvider for Nosql {
    fn kind(&self) -> &str {
        "nosql"
    }
    fn fields(&self) -> &[Field] {
        const F: &[Field] = &[
            live("cql", "CQL contact point", false, "CASSANDRA_CONTACT_POINTS"),
            live("username", "Username", false, "CASSANDRA_USERNAME"),
            live("password", "Password", true, "CASSANDRA_PASSWORD"),
            live("http_token", "HTTP token", true, "NOSQL_TOKEN"),
            live("alternator", "Alternator (DynamoDB) endpoint", false, "DYNAMODB_ENDPOINT"),
        ];
        F
    }
    fn suggested(&self) -> &[(&'static str, Mode)] {
        &[("cql", Mode::Live), ("username", Mode::Live), ("password", Mode::Live)]
    }
    fn name(&self, r#ref: &str) -> Option<String> {
        self.0.call(Method::GET, &format!("/v1/nosql/databases/{}", enc(r#ref)), None).ok()?["name"].as_str().map(Into::into)
    }
    fn read(&self, r#ref: &str, field: &str, _: &Link) -> roll_secrets::Result<Value> {
        let d = self.0.call(Method::GET, &format!("/v1/nosql/databases/{}", enc(r#ref)), None)?;
        let pointer = match field {
            "cql" => "/endpoints/cql",
            "username" => "/credentials/username",
            "password" => "/credentials/password",
            "http_token" => "/http_token",
            "alternator" => "/endpoints/alternator",
            f => return Err(Error::BadRequest(format!("nosql has no field {f}"))),
        };
        text(&d, pointer)
    }
    fn rotate_source(&self, r#ref: &str) -> roll_secrets::Result<bool> {
        self.0.call(Method::POST, &format!("/v1/nosql/databases/{}/password", enc(r#ref)), Some(json!({}))).map(|_| true)
    }
}

/// Queues (a roll queues vhost, by name): where clients reach it (live), and a user of its own
/// (managed or dynamic) made through `/v1/queues/users` with permissions on that vhost only. The
/// vhost's own user can't be read back (`…/credentials` shows its password once, and every call
/// makes a new one), so no credential is live. Option `permission`: `read` (consume), `write`
/// (publish and consume) or `configure` (default: also declare and delete queues, exchanges and
/// bindings, as the vhost's own user may).
struct Queue(Api);

impl Queue {
    /// The vhost's AMQP URL as roll queues answers it (the admin's user in it: only its scheme,
    /// host, port, path and query are used, never edited into a link's value).
    fn amqp_url(&self, vhost: &str) -> roll_secrets::Result<url::Url> {
        let list = self.0.call(Method::GET, "/v1/queues/vhosts", None)?;
        let v = list.as_array().into_iter().flatten().find(|v| v["name"] == vhost).ok_or_else(|| Error::Link(format!("roll queues has no vhost {vhost}")))?;
        let u = v["amqp_url"].as_str().ok_or_else(|| Error::Link("the product didn't return amqp_url".into()))?;
        url::Url::parse(u).map_err(|_| Error::Link("the vhost's AMQP URL doesn't parse".into()))
    }

    /// RabbitMQ's `configure`, `write` and `read` patterns for `permission`. A pattern that
    /// matches nothing is `$.`, not empty: an empty pattern matches the default exchange's name.
    fn patterns(permission: &str) -> Option<(&'static str, &'static str, &'static str)> {
        match permission {
            "read" => Some(("$.", "$.", ".*")),
            "write" => Some(("$.", ".*", ".*")),
            "configure" => Some((".*", ".*", ".*")),
            _ => None,
        }
    }

    fn permission(link: &Link) -> &str {
        link.options.get("permission").and_then(Json::as_str).unwrap_or("configure")
    }

    /// `user`'s URL for the vhost `admin` reaches, built from its scheme, host, port, path and
    /// query: the admin's user and password are never in it, and anything that doesn't come out
    /// exactly as built is refused rather than handed out.
    fn user_url(admin: &url::Url, user: &str, password: &str) -> roll_secrets::Result<String> {
        let bad = || Error::Link("the vhost's AMQP URL can't carry a user of its own".into());
        let host = admin.host_str().filter(|h| !h.is_empty()).ok_or_else(bad)?;
        let port = admin.port().map(|p| format!(":{p}")).unwrap_or_default();
        let query = admin.query().map(|q| format!("?{q}")).unwrap_or_default();
        let u = url::Url::parse(&format!("{}://{user}:{password}@{host}{port}{}{query}", admin.scheme(), admin.path())).map_err(|_| bad())?;
        if u.username() != user || u.password() != Some(password) || u.host_str() != Some(host) || u.port() != admin.port() || u.path() != admin.path() {
            return Err(bad());
        }
        Ok(u.to_string())
    }

    /// Only a user this provider made (`roll-` and its random part): a lease never names another.
    fn ours(user: &str) -> bool {
        user.strip_prefix("roll-").is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
    }
}

impl LinkProvider for Queue {
    fn kind(&self) -> &str {
        "queue"
    }
    fn fields(&self) -> &[Field] {
        const F: &[Field] = &[
            live("host", "Host", false, "AMQP_HOST"),
            live("port", "Port", false, "AMQP_PORT"),
            live("vhost", "Virtual host", false, "AMQP_VHOST"),
            issued("url", "AMQP URL", true, "AMQP_URL", "user"),
            issued("username", "Username", false, "AMQP_USERNAME", "user"),
            issued("password", "Password", true, "AMQP_PASSWORD", "user"),
        ];
        F
    }
    fn suggested(&self) -> &[(&'static str, Mode)] {
        // A user of the app's own, never the vhost's.
        &[("url", Mode::Managed)]
    }
    fn read(&self, r#ref: &str, field: &str, _: &Link) -> roll_secrets::Result<Value> {
        if field == "vhost" {
            return Ok(Value::new(r#ref));
        }
        let u = self.amqp_url(r#ref)?;
        match field {
            "host" => Ok(Value::new(u.host_str().unwrap_or_default())),
            "port" => Ok(Value::new(u.port().unwrap_or(if u.scheme() == "amqps" { 5671 } else { 5672 }).to_string())),
            f => Err(Error::BadRequest(format!("queue has no live field {f}"))),
        }
    }
    fn issue(&self, r#ref: &str, _: &str, link: &Link, _: &str, _: Option<u64>) -> roll_secrets::Result<Issued> {
        let (configure, write, read) = Self::patterns(Self::permission(link)).ok_or_else(|| Error::BadRequest("a queue link's permission is read, write or configure".into()))?;
        let admin = self.amqp_url(r#ref)?;
        // Only letters and digits we made: nothing to escape.
        let user = format!("roll-{}", random(12));
        let password = random(32);
        // Before the user exists: a URL that can't carry it is refused, never handed out.
        let url = Self::user_url(&admin, &user, &password)?;
        // No tags: the user can't sign in to the management API.
        self.0.call(Method::PUT, &format!("/v1/queues/users/{}", enc(&user)), Some(json!({ "password": password, "tags": [] })))?;
        let granted = self.0.call(Method::PUT, &format!("/v1/queues/permissions/{}/{}", enc(r#ref), enc(&user)), Some(json!({ "configure": configure, "write": write, "read": read })));
        if let Err(e) = granted {
            // A user without its permission is no use: it goes again.
            let _ = self.0.delete_gone(&format!("/v1/queues/users/{}", enc(&user)));
            return Err(e);
        }
        let parts = BTreeMap::from([
            ("url".to_string(), Value::new(url)),
            ("username".to_string(), Value::new(user.clone())),
            ("password".to_string(), Value::new(password)),
        ]);
        Ok(Issued { parts, handle: json!({ "user": user }) })
    }
    /// Deletes the user, which closes its connections and drops its permissions.
    fn revoke(&self, _: &str, h: &Json) -> roll_secrets::Result<()> {
        let Some(user) = h["user"].as_str() else { return Ok(()) };
        if !Self::ours(user) {
            return Err(Error::Internal("a lease names a queue user roll didn't make".into()));
        }
        self.0.delete_gone(&format!("/v1/queues/users/{}", enc(user)))
    }
    fn check(&self, link: &Link) -> roll_secrets::Result<()> {
        if link.mode != Mode::Live && Self::patterns(Self::permission(link)).is_none() {
            return Err(Error::BadRequest("a queue link's permission is read, write or configure".into()));
        }
        Ok(())
    }
}

/// Search clusters (roll search): where clients reach one (live), and credentials of their own
/// (managed or dynamic) from `POST /v1/search/clusters/{id}/keys`: an API key on an Elasticsearch
/// cluster, an internal user (`user`, `password`) on an OpenSearch one, which has no API keys.
/// Never the cluster's own password, which rotating would take from everyone else.
/// The product's API, and clusters' flavors as last read (`(when, flavor)`), so naming a
/// cluster's fields for one request asks roll search once.
struct Search(Api, std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, String)>>);

const SEARCH_FIELDS: &[Field] = &[
    live("url", "URL", false, "ELASTICSEARCH_URL"),
    live("host", "Host", false, "ELASTICSEARCH_HOST"),
    live("port", "Port", false, "ELASTICSEARCH_PORT"),
    live("username", "Cluster name", false, "ELASTICSEARCH_USERNAME"),
    issued("api_key", "API key (Authorization: ApiKey …)", true, "ELASTICSEARCH_API_KEY", "key"),
    // OpenSearch's: a user of its own (`<user>@<cluster>`, which the node routes).
    issued("user", "User (OpenSearch)", false, "OPENSEARCH_USERNAME", "key"),
    issued("password", "Password (OpenSearch)", true, "OPENSEARCH_PASSWORD", "key"),
];

/// The same fields as an OpenSearch cluster's clients name them.
const OPENSEARCH_FIELDS: &[Field] = &[
    live("url", "URL", false, "OPENSEARCH_URL"),
    live("host", "Host", false, "OPENSEARCH_HOST"),
    live("port", "Port", false, "OPENSEARCH_PORT"),
    live("username", "Cluster name", false, "OPENSEARCH_CLUSTER"),
    issued("api_key", "API key (Elasticsearch only)", true, "ELASTICSEARCH_API_KEY", "key"),
    issued("user", "User", false, "OPENSEARCH_USERNAME", "key"),
    issued("password", "Password", true, "OPENSEARCH_PASSWORD", "key"),
];

impl Search {
    /// The cluster's flavor (`elasticsearch`, `opensearch`), when roll search says; kept 30 s.
    fn flavor(&self, r#ref: &str) -> Option<String> {
        if let Some((at, f)) = self.1.lock().expect("flavors").get(r#ref)
            && at.elapsed() < std::time::Duration::from_secs(30)
        {
            return Some(f.clone());
        }
        let f = self.0.call(Method::GET, &format!("/v1/search/clusters/{}", enc(r#ref)), None).ok()?["flavor"].as_str().map(String::from)?;
        let mut m = self.1.lock().expect("flavors");
        if m.len() > 1_000 {
            m.retain(|_, (at, _)| at.elapsed() < std::time::Duration::from_secs(30));
        }
        m.insert(r#ref.to_string(), (std::time::Instant::now(), f.clone()));
        Some(f)
    }
}

impl LinkProvider for Search {
    fn kind(&self) -> &str {
        "search"
    }
    fn fields(&self) -> &[Field] {
        SEARCH_FIELDS
    }
    /// An OpenSearch cluster's fields are named as its clients read them (`OPENSEARCH_URL`).
    fn fields_for(&self, r#ref: &str) -> Vec<Field> {
        if self.flavor(r#ref).as_deref() == Some("opensearch") { OPENSEARCH_FIELDS.to_vec() } else { SEARCH_FIELDS.to_vec() }
    }
    fn suggested_for(&self, r#ref: &str) -> Vec<(&'static str, Mode)> {
        match self.flavor(r#ref).as_deref() {
            Some("opensearch") => vec![("url", Mode::Live), ("user", Mode::Managed), ("password", Mode::Managed)],
            Some(_) => self.suggested().to_vec(),
            // Not known (roll search didn't answer): where it is, never a guess at a credential.
            None => vec![("url", Mode::Live)],
        }
    }
    fn suggested(&self) -> &[(&'static str, Mode)] {
        &[("url", Mode::Live), ("api_key", Mode::Managed)]
    }
    fn name(&self, r#ref: &str) -> Option<String> {
        self.0.call(Method::GET, &format!("/v1/search/clusters/{}", enc(r#ref)), None).ok()?["name"].as_str().map(Into::into)
    }
    fn read(&self, r#ref: &str, field: &str, _: &Link) -> roll_secrets::Result<Value> {
        let c = self.0.call(Method::GET, &format!("/v1/search/clusters/{}", enc(r#ref)), None)?;
        match field {
            "port" => Ok(Value::new(c["port"].to_string())),
            f => text(&c, &format!("/{f}")),
        }
    }
    fn issue(&self, r#ref: &str, _: &str, _: &Link, label: &str, ttl: Option<u64>) -> roll_secrets::Result<Issued> {
        // Data access only (indices, templates, pipelines, lifecycle): a leaked app key can't make
        // more keys or users.
        let mut body = json!({ "name": label.chars().take(64).collect::<String>(), "access": "data" });
        // A dynamic key expires by itself a little after its lease, should a revoke fail.
        if let Some(m) = ttl {
            body["expires_in_s"] = json!((m + 5) * 60);
        }
        let k = self.0.call(Method::POST, &format!("/v1/search/clusters/{}/keys", enc(r#ref)), Some(body))?;
        let id = text(&k, "/id")?;
        // roll search says what it made (#29): anything but a data key, or a key that doesn't
        // say, is deleted and refused (docs/secrets.md, "Links": search).
        if k["access"].as_str() != Some("data") {
            let _ = self.0.delete_gone(&format!("/v1/search/clusters/{}/keys/{}", enc(r#ref), enc(id.expose())));
            let made = k["access"].as_str().map_or("a key without its access".to_string(), |a| format!("an {a} key"));
            return Err(Error::Link(format!("roll search made {made}, not a data one: refused (and deleted)")));
        }
        Ok(Issued { parts: search_parts(&k)?, handle: json!({ "id": id.expose() }) })
    }
    /// A field the cluster's flavor doesn't issue (an API key from OpenSearch, a user from
    /// Elasticsearch) is refused when the link is written. A build of roll search that doesn't
    /// say its flavor is taken at its word later, at the first issue.
    fn check(&self, link: &Link) -> roll_secrets::Result<()> {
        let c = self.0.call(Method::GET, &format!("/v1/search/clusters/{}", enc(&link.r#ref)), None)?;
        match (c["flavor"].as_str(), link.field.as_str()) {
            (Some("opensearch"), "api_key") => Err(Error::BadRequest("an OpenSearch cluster has no API keys: link user and password (OPENSEARCH_USERNAME, OPENSEARCH_PASSWORD)".into())),
            (Some("elasticsearch"), "user" | "password") => Err(Error::BadRequest("an Elasticsearch cluster issues API keys: link api_key (ELASTICSEARCH_API_KEY)".into())),
            _ => Ok(()),
        }
    }
    /// A key's expiry is set when it's made (roll search has no way to move it), so a renewal
    /// would leave a lease that outlives its key.
    fn renew(&self, _: &str, _: &Json, _: i64) -> roll_secrets::Result<()> {
        Err(Error::BadRequest("search keys can't be renewed: resolve again for a new key".into()))
    }
    fn revoke(&self, r#ref: &str, h: &Json) -> roll_secrets::Result<()> {
        let Some(id) = h["id"].as_str() else { return Ok(()) };
        self.0.delete_gone(&format!("/v1/search/clusters/{}/keys/{}", enc(r#ref), enc(id)))
    }
}

/// What `…/keys` answered, by field: an API key (Elasticsearch), or a user and its password
/// (OpenSearch).
fn search_parts(k: &Json) -> roll_secrets::Result<BTreeMap<String, Value>> {
    if k["api_key"].is_string() {
        return Ok(BTreeMap::from([("api_key".to_string(), text(k, "/api_key")?)]));
    }
    Ok(BTreeMap::from([("user".to_string(), text(k, "/username")?), ("password".to_string(), text(k, "/password")?)]))
}

/// roll code's search for roll secrets' health (`code` check): whether a repository's default
/// branch mentions each key, from roll code's trigram index. Any error (code search off, the
/// index not ready, the repository not on this node) is "can't tell": nothing gets flagged.
pub struct CodeRefs(pub Arc<code_control::Code>);

impl roll_secrets::health::CodeRefs for CodeRefs {
    fn referenced(&self, repo: &str, keys: &[String]) -> Option<std::collections::BTreeSet<String>> {
        let (owner, name) = repo.split_once('/')?;
        let r = self.0.find_repo(owner, name).ok()?;
        tokio::runtime::Handle::current().block_on(async {
            let mut found = std::collections::BTreeSet::new();
            for k in keys {
                let v = self.0.search_code(&r, k, false, true, None, 1, None).await.ok()?;
                if v["matches"].as_array().is_some_and(|m| !m.is_empty()) {
                    found.insert(k.clone());
                }
            }
            Some(found)
        })
    }
}

/// People's accounts for roll secrets and the workspace (docs/accounts.md): whether a person can
/// still sign in, their roll code owner's name, and what of a canvas is theirs. Looked up on every
/// use.
#[derive(Clone)]
pub struct People {
    pub accounts: Arc<roll_accounts::Accounts>,
    pub code: Option<Arc<code_control::Code>>,
    pub search: Option<Arc<search_control::Control>>,
}

impl roll_secrets::People for People {
    fn person(&self, user: &str) -> Result<Option<roll_secrets::Person>, String> {
        let u = match self.accounts.user(user) {
            Ok(u) if !u.disabled => u,
            Ok(_) | Err(roll_accounts::Error::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        let owner_id = self.accounts.code_owner_id(user).map_err(|e| e.to_string())?;
        let code_owner = match (owner_id, &self.code) {
            (Some(id), Some(code)) => code.db.owner_by_id(&id).map_err(|e| e.to_string())?.map(|o| o.name),
            _ => None,
        };
        Ok(Some(roll_secrets::Person { admin: u.admin, code_owner }))
    }
}

/// Who people are for the workspace's members (docs/members.md): their handle and address, while
/// they can sign in. An invite is mailed only to an address that has an account (roll accounts
/// mails people, never an address a caller names).
impl roll_workspace::People for People {
    fn person(&self, user: &str) -> Option<roll_workspace::Person> {
        let u = self.accounts.user(user).ok().filter(|u| !u.disabled)?;
        Some(roll_workspace::Person { handle: u.handle, email: Some(u.email).filter(|e| !e.is_empty()), admin: u.admin })
    }

    fn mail(&self, email: &str, subject: &str, text: &str) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let (accounts, email, subject, text) = (self.accounts.clone(), email.to_string(), subject.to_string(), text.to_string());
        rt.spawn(async move {
            if let Ok(Some(p)) = accounts.sessions().person_by_email(&email).await
                && let Err(e) = accounts.mail_user(&p.user, &subject, &text).await
            {
                tracing::warn!(error = %e, "mailing a project invite");
            }
        });
    }
}

/// What of a person's canvas is theirs: everything for an admin; a repo under their roll code
/// owner; a search cluster they own; nothing else until each product says who owns its resources.
/// A failed lookup is a no.
impl roll_workspace::Owners for People {
    fn trusts(&self, user: &str, kind: roll_workspace::Kind, r#ref: &str) -> bool {
        let Ok(Some(p)) = roll_secrets::People::person(self, user) else { return false };
        p.admin || self.owns(user, kind, r#ref)
    }

    /// Theirs whatever their account is: an admin account added as an owner of someone else's
    /// project adds only what's its own (docs/members.md, "The canvas rule").
    fn owns(&self, user: &str, kind: roll_workspace::Kind, r#ref: &str) -> bool {
        let Ok(Some(p)) = roll_secrets::People::person(self, user) else { return false };
        match kind {
            roll_workspace::Kind::Repo => roll_secrets::access::owns_repo(p.code_owner.as_deref(), r#ref),
            roll_workspace::Kind::Search => self.search.as_ref().is_some_and(|s| matches!(s.store.owner_of(r#ref), Ok(Some(Some(o))) if o == user)),
            _ => false,
        }
    }
}

/// After a product's own route changes a resource's credential (a password reset, a restore),
/// tell roll secrets, so every folder linking it live moves its digest at once (watches, syncs,
/// apps' redeploys and conditional reads see it). Without this they'd still see it within
/// seconds: a digest reads live values' fingerprints. Only the route's pattern is looked at.
pub async fn source_changes(axum::extract::State(s): axum::extract::State<Arc<Secrets>>, req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let changed = (req.method() == Method::POST).then(|| changed_source(req.uri().path())).flatten();
    let resp = next.run(req).await;
    if let Some((kind, r#ref)) = changed
        && resp.status().is_success()
    {
        tokio::task::spawn_blocking(move || {
            if let Err(e) = s.source_changed(kind, &r#ref) {
                tracing::warn!(%kind, "telling roll secrets a credential changed failed: {e}");
            }
        });
    }
    resp
}

/// The link kind and resource a credential-changing product route names.
fn changed_source(path: &str) -> Option<(&'static str, String)> {
    let p: Vec<&str> = path.trim_matches('/').split('/').collect();
    match p.as_slice() {
        ["v1", "redis", "databases", id, "password" | "restore"] => Some(("redis", id.to_string())),
        ["v1", "nosql", "databases", id, "password" | "reset" | "restore"] => Some(("nosql", id.to_string())),
        ["v1", "projects", id, "branches", _, "restore" | "reset"] => Some(("postgres", id.to_string())),
        _ => None,
    }
}

/// roll secrets for roll apps' deploys (`apps_control::integrations::SecretSource`), set once the
/// store is open (it opens after roll apps starts).
#[derive(Clone, Default)]
pub struct ForApps(Arc<OnceLock<(Arc<Secrets>, Arc<roll_workspace::Workspace>)>>);

impl ForApps {
    pub fn set(&self, secrets: Arc<Secrets>, workspace: Arc<roll_workspace::Workspace>) {
        let _ = self.0.set((secrets, workspace));
    }
}

#[async_trait::async_trait]
impl apps_control::integrations::SecretSource for ForApps {
    fn name(&self) -> &'static str {
        "roll secrets"
    }

    /// The service's project's `env`, resolved as `app:<svc>`. A project or environment roll
    /// secrets doesn't have is no variables (a service needn't use secrets), and so is a project
    /// that doesn't stand for the service (`Workspace::trusts`). An unresolved reference fails the
    /// deploy, and so does a dynamic credential: it ends with its lease, and a service that scales
    /// to zero can't renew it. An untrusted ephemeral environment (a preview of code nobody vetted)
    /// reads as roll previews' own resolve does: no protected environment, no sensitive key, no
    /// dynamic credential, and what doesn't resolve is left out rather than failing the deploy.
    async fn resolve(&self, project: &str, env: &str, service: &str) -> anyhow::Result<Vec<apps_control::integrations::SecretVar>> {
        let (secrets, ws) = self.0.get().cloned().ok_or_else(|| anyhow::anyhow!("roll secrets is starting: deploy again in a moment"))?;
        let (project, env, service) = (project.to_string(), env.to_string(), service.to_string());
        tokio::task::spawn_blocking(move || {
            let Ok(p) = ws.get(roll_workspace::Scope::All, &project) else { return Ok(Vec::new()) };
            if !ws.trusts(&p, roll_workspace::Kind::Service, &service) {
                tracing::warn!(%project, %service, "apps: the project doesn't stand for the service, so it gets none of the project's secrets");
                return Ok(Vec::new());
            }
            let consumer = format!("app:{service}");
            let caller = roll_secrets::Caller::consumer(&consumer);
            let untrusted = match secrets.environment(&project, &env, &caller) {
                Ok(e) => e.kind == roll_secrets::EnvironmentKind::Ephemeral && !e.trusted,
                Err(roll_secrets::Error::NotFound(_)) => return Ok(Vec::new()),
                Err(e) => return Err(e.into()),
            };
            // roll secrets narrows an untrusted environment's reader itself (no protected
            // environment, no sensitive key); dynamic keys are left out here.
            let v = secrets.values_for(&project, &env, "/", true, Some(&consumer), !untrusted, &caller)?;
            if untrusted {
                for p in &v.problems {
                    tracing::info!(%project, %env, key = %p.key, "apps: an untrusted preview's secret didn't resolve, so it's left out: {}", p.problem);
                }
            } else if let Some(p) = v.problems.first() {
                anyhow::bail!("{} of {env}'s secrets didn't resolve ({}: {})", v.problems.len(), p.key, p.problem);
            }
            if let Some(k) = v.dynamic.first().filter(|_| !untrusted) {
                anyhow::bail!("{k} in {env} is a dynamic credential, which ends with its lease: link it as managed or live for a service");
            }
            // Linked keys are addresses as rolld sees them: apps rewrites them for the container.
            let linked: std::collections::HashSet<String> = v.linked.into_iter().collect();
            // Sensitive keys (and keys holding one through a reference) are flagged: apps seals
            // them and never shows them, not even to whoever may reveal the environment.
            let sensitive: std::collections::HashSet<String> = v.sensitive.into_iter().collect();
            // Link fields that aren't credentials (a host, a port): apps' logs may show these.
            let plain: std::collections::HashSet<String> = v.plain.into_iter().collect();
            Ok(v.values.into_iter().map(|(name, value)| apps_control::integrations::SecretVar { linked: linked.contains(&name), sensitive: sensitive.contains(&name), plain: plain.contains(&name), value: value.expose().to_string(), name }).collect())
        })
        .await?
    }

    /// apps' `…/variables/values` shows secrets' values: only to whoever may reveal `env` by roll
    /// secrets' own rules (a token's scope, a person's own projects, protected environments).
    /// A variable's `roll://` reference to its own environment (apps refuses another's), read as
    /// the service through `resolve_for`: the project is the one whose canvas holds the service
    /// and stands for it, and the read is audited as `apps:service:<id>`. An untrusted preview
    /// reads as an untrusted copy (no protected environment, no sensitive key).
    async fn resolve_ref(&self, project: &str, env: &str, service: &str, reference: &str) -> anyhow::Result<String> {
        let (secrets, _) = self.0.get().cloned().ok_or_else(|| anyhow::anyhow!("roll secrets is starting: deploy again in a moment"))?;
        let (project, env, service, reference) = (project.to_string(), env.to_string(), service.to_string(), reference.to_string());
        tokio::task::spawn_blocking(move || {
            let caller = roll_secrets::Caller::consumer(&format!("app:{service}"));
            let untrusted = matches!(secrets.environment(&project, &env, &caller), Ok(e) if e.kind == roll_secrets::EnvironmentKind::Ephemeral && !e.trusted);
            let mut r = roll_secrets::ResourceRef::new(roll_workspace::Kind::Service, &service);
            if untrusted {
                r = r.untrusted();
            }
            secrets.resolve_for(&r, &reference).map(|v| v.value.expose().to_string()).map_err(|e| anyhow::anyhow!("{e}"))
        })
        .await?
    }

    /// The environment's digest (versions and keyed fingerprints, nothing decrypted): apps keeps
    /// it with each live deployment and says when the secrets changed since. None for a project
    /// or environment roll secrets doesn't have.
    async fn digest(&self, project: &str, env: &str) -> anyhow::Result<Option<String>> {
        let Some((secrets, _)) = self.0.get().cloned() else { return Ok(None) };
        let (project, env) = (project.to_string(), env.to_string());
        tokio::task::spawn_blocking(move || match secrets.digest(&project, &env, "/", &roll_secrets::Caller::consumer("roll:apps")) {
            Ok(d) => Ok(Some(d)),
            Err(roll_secrets::Error::NotFound(_)) => Ok(None),
            Err(e) => Err(e.into()),
        })
        .await?
    }

    async fn wait_digest(&self, project: &str, env: &str, since: &str, wait: std::time::Duration) -> anyhow::Result<Option<String>> {
        let Some((secrets, _)) = self.0.get().cloned() else { return Ok(None) };
        // Held polls are counted per principal: one per environment, so a service's watch never
        // crowds out another's (the node's cap still holds).
        let caller = roll_secrets::Caller::consumer(&format!("roll:apps:{project}/{env}"));
        match roll_secrets::api::wait_digest(secrets, project.into(), env.into(), "/".into(), Some(since.into()), wait.as_secs(), caller).await {
            Ok(d) => Ok(Some(d)),
            Err(roll_secrets::Error::NotFound(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// A new apps environment's own in roll secrets (`qa`), made as roll apps: a standard
    /// environment, no keys. The defaults (`production`, `staging`, `development`) come with the
    /// project whenever it's first used, so they're left to that.
    async fn ensure(&self, project: &str, env: &str) -> anyhow::Result<()> {
        let Some((secrets, _)) = self.0.get().cloned() else { return Ok(()) };
        if roll_secrets::DEFAULT_ENVIRONMENTS.iter().any(|(slug, _)| *slug == env) {
            return Ok(());
        }
        let (project, env) = (project.to_string(), env.to_string());
        tokio::task::spawn_blocking(move || {
            let caller = roll_secrets::Caller::consumer("roll:apps");
            match secrets.environments(&project, &caller) {
                Ok(_) => secrets.ensure_environment(&project, &env, roll_secrets::Ensure::default(), &caller).map(drop).map_err(anyhow::Error::from),
                Err(roll_secrets::Error::NotFound(_)) => Ok(()),
                Err(e) => Err(e.into()),
            }
        })
        .await?
    }

    /// An instance's token from roll's issuer, only for a service its project stands for
    /// (`Workspace::trusts`, as `resolve`), and only for an environment roll secrets has that
    /// isn't an untrusted preview (`Secrets::issue` refuses one: where `resolve` gives a fork's
    /// preview a narrowed read, it gets no token at all). None where the issuer isn't (no public
    /// URL).
    async fn issue(&self, project: &str, service: &str, env: &str, aud: &str, ttl: std::time::Duration) -> anyhow::Result<Option<String>> {
        let (secrets, ws) = self.0.get().cloned().ok_or_else(|| anyhow::anyhow!("roll secrets is starting: try again in a moment"))?;
        let subject = roll_secrets::Subject::App { project: project.to_string(), service: service.to_string(), env: env.to_string() };
        let aud = aud.to_string();
        tokio::task::spawn_blocking(move || {
            let roll_secrets::Subject::App { project, service, .. } = &subject;
            let p = ws.get(roll_workspace::Scope::All, project).map_err(|e| anyhow::anyhow!("{e}"))?;
            if !ws.trusts(&p, roll_workspace::Kind::Service, service) {
                anyhow::bail!("project {project} doesn't stand for {service}, so roll signs no token for it");
            }
            if !secrets.issues() {
                return Ok(None);
            }
            Ok(Some(secrets.issue(subject, &aud, ttl)?))
        })
        .await?
    }

    /// A variable's reference into another environment, bound to whoever writes it
    /// (`Secrets::bind_ref`: they may reveal it for the service now): the maker to keep.
    async fn bind_ref(&self, who: apps_control::integrations::Asker<'_>, project: &str, service: &str, reference: &str) -> anyhow::Result<String> {
        let (secrets, _) = self.0.get().cloned().ok_or_else(|| anyhow::anyhow!("roll secrets is starting: try again in a moment"))?;
        let caller = secrets.caller_of(who.operator, who.user, who.grant).map_err(|e| anyhow::anyhow!("{e}"))?;
        let (service, reference, _project) = (service.to_string(), reference.to_string(), project.to_string());
        tokio::task::spawn_blocking(move || {
            let r = roll_secrets::ResourceRef::new(roll_workspace::Kind::Service, &service);
            let maker = secrets.bind_ref(&r, &reference, &caller).map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(serde_json::to_value(&maker)?.as_str().unwrap_or_default().to_string())
        })
        .await?
    }

    /// A Git login's reference, bound as the service's runtime read in `runtime_env`
    /// (`Secrets::bind_ref_runtime`; the gate asked for deploy on the service).
    async fn bind_ref_runtime(&self, who: apps_control::integrations::Asker<'_>, _project: &str, service: &str, reference: &str, runtime_env: &str) -> anyhow::Result<String> {
        let (secrets, _) = self.0.get().cloned().ok_or_else(|| anyhow::anyhow!("roll secrets is starting: try again in a moment"))?;
        let caller = secrets.caller_of(who.operator, who.user, who.grant).map_err(|e| anyhow::anyhow!("{e}"))?;
        let (service, reference, runtime_env) = (service.to_string(), reference.to_string(), runtime_env.to_string());
        tokio::task::spawn_blocking(move || {
            let r = roll_secrets::ResourceRef::new(roll_workspace::Kind::Service, &service);
            let maker = secrets.bind_ref_runtime(&r, &reference, &caller, &runtime_env).map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(serde_json::to_value(&maker)?.as_str().unwrap_or_default().to_string())
        })
        .await?
    }

    /// The digest of the folder a bound reference reads, as its maker may now.
    async fn digest_as(&self, project: &str, env: &str, service: &str, reference: &str, maker: &str) -> anyhow::Result<String> {
        let (secrets, _) = self.0.get().cloned().ok_or_else(|| anyhow::anyhow!("roll secrets is starting"))?;
        let (project, env, service, reference) = (project.to_string(), env.to_string(), service.to_string(), reference.to_string());
        let maker: roll_secrets::Maker = serde_json::from_value(serde_json::Value::String(maker.to_string()))?;
        tokio::task::spawn_blocking(move || {
            let caller = roll_secrets::Caller::consumer(&format!("app:{service}"));
            let untrusted = matches!(secrets.environment(&project, &env, &caller), Ok(e) if e.kind == roll_secrets::EnvironmentKind::Ephemeral && !e.trusted);
            let mut r = roll_secrets::ResourceRef::new(roll_workspace::Kind::Service, &service);
            if untrusted {
                r = r.untrusted();
            }
            secrets.digest_as(&r, &reference, &maker).map_err(|e| anyhow::anyhow!("{e}"))
        })
        .await?
    }

    /// That reference's value for the service in `env`, read as its maker may now (untrusted in an
    /// untrusted preview, as `resolve_ref`).
    async fn resolve_as(&self, project: &str, env: &str, service: &str, reference: &str, maker: &str) -> anyhow::Result<String> {
        let (secrets, _) = self.0.get().cloned().ok_or_else(|| anyhow::anyhow!("roll secrets is starting: deploy again in a moment"))?;
        let (project, env, service, reference) = (project.to_string(), env.to_string(), service.to_string(), reference.to_string());
        let maker: roll_secrets::Maker = serde_json::from_value(serde_json::Value::String(maker.to_string()))?;
        tokio::task::spawn_blocking(move || {
            let caller = roll_secrets::Caller::consumer(&format!("app:{service}"));
            let untrusted = matches!(secrets.environment(&project, &env, &caller), Ok(e) if e.kind == roll_secrets::EnvironmentKind::Ephemeral && !e.trusted);
            let mut r = roll_secrets::ResourceRef::new(roll_workspace::Kind::Service, &service);
            if untrusted {
                r = r.untrusted();
            }
            secrets.resolve_as(&r, &reference, &maker).map(|v| v.value.expose().to_string()).map_err(|e| anyhow::anyhow!("{e}"))
        })
        .await?
    }

    async fn may_reveal(&self, who: apps_control::integrations::Asker<'_>, project: &str, env: &str) -> bool {
        let Some((secrets, _)) = self.0.get().cloned() else { return false };
        match secrets.caller_of(who.operator, who.user, who.grant) {
            Ok(caller) => caller.allows(roll_secrets::Action::Reveal, &roll_secrets::Scope::at(project, env, "/")),
            Err(_) => false,
        }
    }
}

/// roll secrets for roll storage's replication targets (`s3_server::TargetCredentials`): a
/// target's `roll://` references resolve for the source bucket, the canvas resource asking
/// (`Secrets::resolve_for`: its project must stand for it; audited as `storage:bucket:<name>`).
/// Set once the store is open (it opens after storage starts).
#[derive(Clone, Default)]
pub struct ForStorage(Arc<OnceLock<Arc<Secrets>>>);

impl ForStorage {
    pub fn set(&self, secrets: Arc<Secrets>) {
        let _ = self.0.set(secrets);
    }

    async fn run<T: Send + 'static>(&self, bucket: &str, reference: &str, f: fn(&Secrets, &roll_secrets::ResourceRef, &str) -> Result<T, roll_secrets::Refusal>) -> Result<T, String> {
        let secrets = self.0.get().cloned().ok_or("roll secrets is starting")?;
        let (resource, reference) = (roll_secrets::ResourceRef::new(roll_workspace::Kind::Bucket, bucket), reference.to_string());
        tokio::task::spawn_blocking(move || f(&secrets, &resource, &reference).map_err(|e| e.to_string())).await.map_err(|e| e.to_string())?
    }
}

impl s3_server::TargetCredentials for ForStorage {
    fn resolve<'a>(&'a self, bucket: &'a str, reference: &'a str) -> futures::future::BoxFuture<'a, Result<s3_store::Secret, String>> {
        Box::pin(async move { self.run(bucket, reference, |s, r, x| s.resolve_for(r, x).map(|v| s3_store::Secret::new(v.value.expose()))).await })
    }

    fn digest<'a>(&'a self, bucket: &'a str, reference: &'a str) -> futures::future::BoxFuture<'a, Result<String, String>> {
        Box::pin(async move { self.run(bucket, reference, |s, r, x| s.digest_for(r, x)).await })
    }
}

/// roll secrets for roll search's snapshot repositories (`search_control::secret_refs`): a
/// repository's `roll://` credentials are bound by the caller who writes them
/// (`Secrets::bind_ref`, the caller as a request to secrets would be), and read as that maker
/// (`resolve_as`/`digest_as`, their rights looked up again on every use) for the cluster, the
/// canvas resource asking (`search:search:<id>` in the audit log; a branch asks as the cluster it
/// came from, untrusted).
pub struct ForSearch(pub Arc<Secrets>);

impl ForSearch {
    fn resource(holder: &search_control::secret_refs::Holder) -> roll_secrets::ResourceRef {
        let resource = roll_secrets::ResourceRef::new(roll_workspace::Kind::Search, holder.id.clone());
        if holder.copy { resource.untrusted() } else { resource }
    }

    /// What a refusal says. A branch learns nothing about which keys a protected environment
    /// holds: protected and missing read alike.
    fn refusal(resource: &roll_secrets::ResourceRef, reference: &str, e: roll_secrets::Refusal) -> String {
        match e {
            roll_secrets::Refusal::Protected(_) | roll_secrets::Refusal::NoSuchKey(_) if resource.is_untrusted() => format!("not available to a branch: {reference} is protected or doesn't exist"),
            e => e.to_string(),
        }
    }

    /// `f` for the reference, as its stored maker (a string only roll's control plane writes).
    async fn as_maker<T: Send + 'static>(&self, holder: &search_control::secret_refs::Holder, reference: &str, f: fn(&Secrets, &roll_secrets::ResourceRef, &str, &roll_secrets::Maker) -> Result<T, roll_secrets::Refusal>) -> Result<T, String> {
        let resource = Self::resource(holder);
        let maker: roll_secrets::Maker = serde_json::from_value(serde_json::Value::String(holder.maker.clone())).map_err(|_| "nobody roll knows chose it".to_string())?;
        let (secrets, reference) = (self.0.clone(), reference.to_string());
        tokio::task::spawn_blocking(move || f(&secrets, &resource, &reference, &maker).map_err(|e| Self::refusal(&resource, &reference, e))).await.map_err(|e| e.to_string())?
    }
}

#[async_trait::async_trait]
impl search_control::secret_refs::ResourceSecrets for ForSearch {
    async fn may_write(&self, who: &search_control::secret_refs::Who, holder: &search_control::secret_refs::Holder, reference: &str) -> Result<String, search_control::secret_refs::Refused> {
        use search_control::secret_refs::Refused;
        let resource = Self::resource(holder);
        let (secrets, reference, who) = (self.0.clone(), reference.to_string(), who.clone());
        tokio::task::spawn_blocking(move || {
            let caller = secrets.caller_of(who.operator, who.user.as_ref(), who.grant.as_ref()).map_err(|e| Refused::Forbidden(e.to_string()))?;
            let maker = secrets.bind_ref(&resource, &reference, &caller).map_err(|e| match e {
                roll_secrets::Refusal::NotAllowed(_) | roll_secrets::Refusal::NotTrusted(_) | roll_secrets::Refusal::Protected(_) => Refused::Forbidden(e.to_string()),
                e => Refused::Invalid(e.to_string()),
            })?;
            Ok(maker.to_string())
        })
        .await
        .map_err(|e| Refused::Invalid(e.to_string()))?
    }

    async fn resolve(&self, holder: &search_control::secret_refs::Holder, reference: &str) -> Result<search_control::secret_refs::Revealed, String> {
        self.as_maker(holder, reference, |s, r, x, m| s.resolve_as(r, x, m).map(|v| search_control::secret_refs::Revealed::new(v.value.expose()))).await
    }

    async fn digest(&self, holder: &search_control::secret_refs::Holder, reference: &str) -> Result<String, String> {
        self.as_maker(holder, reference, |s, r, x, m| s.digest_as(r, x, m)).await
    }
}

/// Workload identities' logins get roll-access workload tokens (`Access::mint_workload`): the
/// identity's scope, its addresses, for its minutes, owned by whoever made it.
pub struct Workloads(pub Arc<roll_access::Access>);

impl roll_secrets::Workloads for Workloads {
    fn mint(&self, name: &str, rule: roll_auth::scope::Rule, ip_allow: Option<Vec<String>>, minutes: u32, owner: Option<&str>) -> roll_secrets::Result<roll_secrets::Minted> {
        let within = roll_auth::scope::Scope(vec![rule]);
        let new = roll_access::store::NewGrant { name: name.chars().take(64).collect(), rules: within.clone(), expires_at: None, ip_allow: ip_allow.unwrap_or_default(), rate_per_minute: None, ask_otherwise: false, ..Default::default() };
        let (access, owner) = (self.0.clone(), owner.map(str::to_string));
        // Logins run on the store's blocking threads (`blocking` in roll-secrets' api.rs), inside
        // the runtime: never call this from an async task, where `block_on` panics.
        let made = tokio::runtime::Handle::current().block_on(async move { access.mint_workload(new, &within, minutes, owner.as_deref()).await });
        match made {
            Ok((g, secret)) => Ok(roll_secrets::Minted { id: g.id, secret, expires_at: g.expires_at.unwrap_or_default() }),
            Err(roll_access::Error::Forbidden(m)) => Err(Error::Forbidden(m)),
            Err(roll_access::Error::BadRequest(m)) => Err(Error::BadRequest(m)),
            Err(roll_access::Error::Conflict(m) | roll_access::Error::TooMany(m)) => Err(Error::TooMany(m)),
            Err(e) => Err(Error::Internal(format!("minting a workload token: {e}"))),
        }
    }

    /// Only a live workload grant, with why and an audit row (`false`: it had already ended).
    fn revoke(&self, id: &str) -> roll_secrets::Result<()> {
        self.0.revoke_workload(id, "its workload identity was deleted").map(|_| ()).map_err(|e| Error::Internal(format!("revoking a workload token: {e}")))
    }

    /// A gateway's token, made for the request's caller as `POST /v1/access/tokens` makes one.
    /// Refusals are said in the gateway form's words: roll-access's are about its own callers and
    /// fields.
    fn token(&self, ext: &axum::http::Extensions, name: &str, rule: roll_auth::scope::Rule) -> roll_secrets::Result<roll_secrets::Minted> {
        let new = roll_access::store::NewGrant { name: name.chars().take(64).collect(), rules: roll_auth::scope::Scope(vec![rule]), ..Default::default() };
        let (access, ext) = (self.0.clone(), ext.clone());
        // On the store's blocking threads, as `mint`.
        match tokio::runtime::Handle::current().block_on(async move { access.create_grant(&ext, new).await }) {
            Ok((g, secret)) => Ok(roll_secrets::Minted { id: g.id, secret, expires_at: g.expires_at.unwrap_or_default() }),
            Err(roll_access::Error::Forbidden(_) | roll_access::Error::Unauthorized(_)) => {
                Err(Error::Forbidden("roll couldn't tell whose this gateway's token would be: sign in, or add the gateway without a token and log it in through a workload identity".into()))
            }
            Err(roll_access::Error::BadRequest(m)) if m.starts_with("roll's policy") => {
                Err(Error::BadRequest("roll's token policy has every token expire, and a gateway's lasts until the gateway is deleted: add the gateway without a token and log it in through a workload identity".into()))
            }
            Err(roll_access::Error::BadRequest(m)) => Err(Error::BadRequest(m)),
            Err(roll_access::Error::Conflict(m) | roll_access::Error::TooMany(m)) => Err(Error::TooMany(m)),
            Err(e) => Err(Error::Internal(format!("making a gateway's token: {e}"))),
        }
    }

    fn revoke_token(&self, id: &str, why: &str) -> roll_secrets::Result<()> {
        match self.0.store.revoke(id) {
            Ok(_) | Err(roll_access::Error::NotFound(_)) => Ok(()),
            Err(e) => Err(Error::Internal(format!("revoking {id} ({why}): {e}"))),
        }
    }

    /// A decoy token for a decoy key: roll-access's own decoys, owned by whoever planted it.
    fn mint_decoy(&self, name: &str, rule: roll_auth::scope::Rule, owner: Option<&str>) -> roll_secrets::Result<roll_secrets::Minted> {
        match self.0.mint_decoy(name, roll_auth::scope::Scope(vec![rule]), owner.unwrap_or("operator"), owner) {
            Ok((g, secret)) => Ok(roll_secrets::Minted { id: g.id, secret, expires_at: 0 }),
            Err(roll_access::Error::BadRequest(m)) => Err(Error::BadRequest(m)),
            Err(roll_access::Error::Conflict(m) | roll_access::Error::TooMany(m)) => Err(Error::TooMany(m)),
            Err(e) => Err(Error::Internal(format!("minting a decoy token: {e}"))),
        }
    }

    fn revoke_decoy(&self, id: &str) -> roll_secrets::Result<()> {
        self.0.revoke_decoy(id, "its decoy key was changed or deleted").map(|_| ()).map_err(|e| Error::Internal(format!("revoking a decoy token: {e}")))
    }

    /// An incident's cut-off: `Access::revoke_many`, one write, all or none, an audit row per grant
    /// with why and who. Called from roll secrets' blocking threads, as `mint` is.
    fn revoke_many(&self, ids: &[String], why: &str, by: &str) -> roll_secrets::Result<()> {
        let (access, ids, why, by) = (self.0.clone(), ids.to_vec(), why.to_string(), by.to_string());
        match tokio::runtime::Handle::current().block_on(async move { access.revoke_many(ids, &why, &by).await }) {
            Ok(_) => Ok(()),
            Err(roll_access::Error::NotFound(m)) => Err(Error::NotFound(m)),
            Err(roll_access::Error::BadRequest(m)) => Err(Error::BadRequest(m)),
            Err(e) => Err(Error::Internal(format!("revoking tokens: {e}"))),
        }
    }

    /// Each token as roll-access answers it now: gone once revoked or expired, or (a person's own)
    /// once its person can't sign in.
    fn grants(&self) -> roll_secrets::Result<Vec<roll_auth::Grant>> {
        let list = self.0.store.list().map_err(|e| Error::Internal(format!("listing roll-access's tokens: {e}")))?;
        Ok(list.into_iter().filter(|g| g.revoked_at.is_none()).filter_map(|g| roll_auth::Grants::grant(&*self.0, &g.id)).collect())
    }
}

/// roll secrets for roll code's checks and sandboxes (`code_control::SecretEnv`), set once the
/// store is open (it opens after roll code starts).
#[derive(Clone, Default)]
pub struct ForCode(Arc<OnceLock<(Arc<Secrets>, Arc<roll_workspace::Workspace>)>>);

impl ForCode {
    pub fn set(&self, secrets: Arc<Secrets>, workspace: Arc<roll_workspace::Workspace>) {
        let _ = self.0.set((secrets, workspace));
    }

    /// `environment` (`env` or `env:/folder`) of `project` for `consumer`: never a protected
    /// environment, never with an unresolved reference. With `asker` (a person or a token), as
    /// far as their own rules reach, and the audit row is `<consumer> for <principal>`; the
    /// principal comes back with the values.
    async fn resolve(&self, project: String, environment: &str, consumer: &str, asker: Option<Asker>) -> anyhow::Result<(String, Vec<(String, String)>)> {
        let (secrets, _) = self.0.get().cloned().ok_or_else(|| anyhow::anyhow!("roll secrets is starting"))?;
        let (slug, path) = environment.split_once(':').unwrap_or((environment, "/"));
        let (slug, path, consumer) = (slug.to_string(), path.to_string(), consumer.to_string());
        tokio::task::spawn_blocking(move || {
            let (base, consumer) = match asker {
                // As the person or token that asked: managing a code owner isn't revealing a
                // project's values. Their rules decide references and imports too.
                Some(a) => {
                    let caller = a.caller(&secrets)?;
                    match caller.need(roll_secrets::Action::Reveal, &roll_secrets::Scope::at(&project, &slug, &path)) {
                        Ok(()) => {
                            let consumer = format!("{consumer} for {}", caller.principal);
                            (caller, consumer)
                        }
                        Err(roll_secrets::Error::Forbidden(_)) => return Err(code_control::Refused(format!("{} may not reveal {environment}", caller.principal, environment = if path == "/" { slug.clone() } else { format!("{slug}:{path}") })).into()),
                        Err(e) => return Err(e.into()),
                    }
                }
                None => (roll_secrets::Caller::consumer(&consumer), consumer),
            };
            // Never a protected environment: not asked for, nor through a reference, an import or
            // an ephemeral environment's base.
            // Not a runtime: a sandbox is a terminal, so sensitive keys stay out (checks too).
            let principal = base.principal.clone();
            let caller = secrets.untrusted(&project, &base.not_runtime())?;
            if !caller.allows(roll_secrets::Action::Reveal, &roll_secrets::Scope::at(&project, &slug, &path)) {
                anyhow::bail!("{slug} is protected: protected environments never go to checks or sandboxes");
            }
            let v = secrets.values(&project, &slug, &path, true, Some(&consumer), &caller).map_err(|e| match e {
                roll_secrets::Error::Forbidden(m) => code_control::Refused(m).into(),
                e => anyhow::Error::from(e),
            })?;
            if let Some(p) = v.problems.first() {
                anyhow::bail!("{} of {slug} didn't resolve ({}: {})", v.problems.len(), p.key, p.problem);
            }
            Ok((principal, v.values.into_iter().map(|(k, v)| (k, v.expose().to_string())).collect()))
        })
        .await?
    }

    fn workspace(&self) -> anyhow::Result<Arc<roll_workspace::Workspace>> {
        Ok(self.0.get().ok_or_else(|| anyhow::anyhow!("roll secrets is starting"))?.1.clone())
    }
}

/// Who asked roll code for an environment's values ([`code_control::Asker`], owned to cross onto
/// the store's blocking threads).
struct Asker {
    operator: bool,
    user: Option<roll_auth::SignedIn>,
    grant: Option<roll_auth::Grant>,
}

impl Asker {
    /// As roll secrets answers a request of theirs; one it wouldn't answer at all (a roll code
    /// owner token is no one to roll secrets) is refused.
    fn caller(&self, secrets: &Secrets) -> anyhow::Result<roll_secrets::Caller> {
        secrets.caller_of(self.operator, self.user.as_ref(), self.grant.as_ref()).map_err(|e| match e {
            roll_secrets::Error::Unauthorized(_) => code_control::Refused("copying secrets into a sandbox needs a session, the operator token or a roll token (rlt_…) that may reveal them".into()).into(),
            e => anyhow::Error::from(e),
        })
    }
}

#[async_trait::async_trait]
impl code_control::SecretEnv for ForCode {
    async fn missing(&self, repo: &str, keys: Vec<String>) -> anyhow::Result<Vec<(String, Vec<String>)>> {
        let ws = self.workspace()?;
        let (secrets, _) = self.0.get().cloned().ok_or_else(|| anyhow::anyhow!("roll secrets is starting"))?;
        let r = repo.to_string();
        tokio::task::spawn_blocking(move || {
            // Every project with the repo on its canvas that may reach it: a key missing from any of
            // their environments.
            let mut out: std::collections::BTreeMap<String, Vec<String>> = Default::default();
            for p in ws.projects_of(roll_workspace::Kind::Repo.as_str(), &r)?.into_iter().filter(|p| secrets.counts_for_repo(p, &r)) {
                for (k, envs) in secrets.keys_missing(&p, &keys)? {
                    out.entry(k).or_default().extend(envs);
                }
            }
            Ok(out.into_iter().collect())
        })
        .await?
    }

    async fn for_repo(&self, repo: &str, environment: &str, consumer: &str) -> anyhow::Result<Vec<(String, String)>> {
        let (secrets, ws) = self.0.get().cloned().ok_or_else(|| anyhow::anyhow!("roll secrets is starting"))?;
        let r = repo.to_string();
        // A person's project counts only for their own repositories: anyone may put any repository
        // on their canvas, and its checks and previews would get their values.
        let project = tokio::task::spawn_blocking(move || ws.projects_of(roll_workspace::Kind::Repo.as_str(), &r).map(|ps| ps.into_iter().find(|p| secrets.counts_for_repo(p, &r))))
            .await??
            .ok_or_else(|| anyhow::anyhow!("{repo} isn't on a workspace project's canvas, so it has no secrets"))?;
        Ok(self.resolve(project, environment, consumer, None).await?.1)
    }

    async fn for_owner(&self, owner: &str, project: &str, environment: &str, consumer: &str, who: code_control::Asker<'_>) -> anyhow::Result<code_control::Revealed> {
        let asker = Asker { operator: who.operator, user: who.user.cloned(), grant: who.grant.cloned() };
        let (secrets, ws) = self.0.get().cloned().ok_or_else(|| anyhow::anyhow!("roll secrets is starting"))?;
        let prefix = format!("{owner}/");
        // One of the owner's repositories on its canvas, which the project may reach.
        let has_repo = |p: &roll_workspace::Project| p.resources.iter().any(|x| x.kind == roll_workspace::Kind::Repo && x.r#ref.starts_with(&prefix) && secrets.counts_for_repo(&p.id, &x.r#ref));
        // Names are unique per account, not across them: the one named that holds the owner's repos.
        let named: Vec<roll_workspace::Project> = tokio::task::spawn_blocking(move || ws.list(roll_workspace::Scope::All))
            .await??
            .into_iter()
            .filter(|p| p.id == project || p.name == project)
            .collect();
        if named.is_empty() {
            anyhow::bail!("no project {project}");
        }
        let p = named.iter().find(|p| has_repo(p)).cloned().ok_or_else(|| anyhow::anyhow!("project {project} has none of {owner}'s repos on its canvas"))?;
        let (by, values) = self.resolve(p.id, environment, consumer, Some(asker)).await?;
        Ok(code_control::Revealed { by, values })
    }

    /// The first project with the repo on its canvas that may reach it and has validity checks on
    /// asks (and keeps, and audits, as `roll:code`); none: nothing is asked.
    async fn validity(&self, repo: &str, found: Vec<(String, Option<String>)>) -> anyhow::Result<Vec<code_control::model::CredentialValidity>> {
        let (secrets, ws) = self.0.get().cloned().ok_or_else(|| anyhow::anyhow!("roll secrets is starting"))?;
        let r = repo.to_string();
        tokio::task::spawn_blocking(move || {
            let projects = ws.projects_of(roll_workspace::Kind::Repo.as_str(), &r)?;
            let Some(p) = projects.into_iter().find(|p| secrets.counts_for_repo(p, &r) && secrets.validity_checks(p).unwrap_or(false)) else { return Ok(Vec::new()) };
            let probes = found.into_iter().map(|(value, pair)| roll_secrets::validity::Probe { value: roll_secrets::Value::new(value), pair: pair.map(roll_secrets::Value::new) }).collect();
            let checked = secrets.check_validity(&p, probes, &roll_secrets::Caller::consumer("roll:code"))?.unwrap_or_default();
            Ok(checked
                .into_iter()
                .map(|c| code_control::model::CredentialValidity {
                    fingerprint: c.fingerprint,
                    result: c.result.as_str().to_string(),
                    account: c.account,
                    checked_at: c.checked_at,
                    rotator: c.rotator.map(str::to_string),
                })
                .collect())
        })
        .await?
    }

    async fn leaks(&self, repo: &str, candidates: Vec<String>) -> anyhow::Result<Vec<(usize, String, String, String)>> {
        let (secrets, ws) = self.0.get().cloned().ok_or_else(|| anyhow::anyhow!("roll secrets is starting"))?;
        let r = repo.to_string();
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            for p in ws.projects_of(roll_workspace::Kind::Repo.as_str(), &r)?.into_iter().filter(|p| secrets.counts_for_repo(p, &r)) {
                out.extend(secrets.leak_matches(&p, &candidates)?.into_iter().map(|(i, l)| (i, l.key, l.environment, l.path)));
            }
            Ok(out)
        })
        .await?
    }

    async fn store_target(&self, repo: &str, asker: &code_control::SecretsAsker, prefer: Option<&str>) -> code_control::Result<code_control::StoreTarget> {
        let secrets = self.secrets()?;
        let (repo, asker, prefer) = (repo.to_string(), asker.clone(), prefer.map(str::to_string));
        let t = tokio::task::spawn_blocking(move || {
            let caller = secrets.caller_of(asker.operator, asker.user.as_ref(), asker.grant.as_ref())?;
            secrets.found_target(&repo, prefer.as_deref(), &caller)
        })
        .await
        .map_err(|e| code_control::Error::Internal(e.into()))?
        .map_err(for_code)?;
        Ok(code_control::StoreTarget { project: t.project, project_name: t.project_name, environments: t.environments, environment: t.environment })
    }

    async fn store(&self, repo: &str, asker: &code_control::SecretsAsker, c: code_control::StoreCredential) -> code_control::Result<code_control::StoredCredential> {
        let secrets = self.secrets()?;
        let (repo, asker) = (repo.to_string(), asker.clone());
        let s = tokio::task::spawn_blocking(move || {
            let caller = secrets.caller_of(asker.operator, asker.user.as_ref(), asker.grant.as_ref())?;
            secrets.store_found(&repo, c.environment.as_deref(), c.prefer.as_deref(), &c.path, &c.key, c.value.0, c.message, &caller)
        })
        .await
        .map_err(|e| code_control::Error::Internal(e.into()))?
        .map_err(for_code)?;
        Ok(code_control::StoredCredential { project: s.project, environment: s.environment, path: s.path, change: s.change, request: s.request })
    }
}

impl ForCode {
    fn secrets(&self) -> code_control::Result<Arc<Secrets>> {
        Ok(self.0.get().ok_or_else(|| code_control::Error::Unavailable("roll secrets is starting".into()))?.0.clone())
    }
}

/// roll secrets' answer as roll code's API answers it (the same statuses).
fn for_code(e: Error) -> code_control::Error {
    use code_control::Error as C;
    match e {
        Error::BadRequest(m) => C::Invalid(m),
        Error::Unauthorized(_) => C::Unauthorized,
        Error::Forbidden(m) => C::Forbidden(m),
        // "no environment staging" reads "environment staging not found".
        Error::NotFound(m) => C::NotFound(m.strip_prefix("no ").map(str::to_string).unwrap_or(m)),
        Error::Conflict("no_project", m) => C::Unprocessable(m),
        Error::Conflict(_, m) => C::Conflict(m),
        Error::Unavailable(m) => C::Unavailable(m),
        e => C::Internal(anyhow::anyhow!(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_search_branch_resolves_as_its_root_untrusted() {
        let holder = |copy| search_control::secret_refs::Holder { id: "srch_root".into(), copy, maker: "usr_ada".into() };
        assert_eq!(super::ForSearch::resource(&holder(false)).consumer(), "search:search:srch_root");
        assert_eq!(super::ForSearch::resource(&holder(true)).consumer(), "search:search:srch_root (untrusted)");
        // A branch can't tell a protected key from a missing one.
        let said = |copy, e| super::ForSearch::refusal(&super::ForSearch::resource(&holder(copy)), "roll://production/K", e);
        let protected = said(true, roll_secrets::Refusal::Protected("production is protected".into()));
        assert_eq!(protected, said(true, roll_secrets::Refusal::NoSuchKey("K".into())));
        assert_ne!(said(false, roll_secrets::Refusal::NoSuchKey("K".into())), protected);
    }

    #[test]
    fn only_roll_mcps_guard_keeps_the_guard_header() {
        let req = || super::Request::builder().uri("/v1/secrets/projects/p/leaks").header("x-roll-guard", "1").body(super::Body::empty()).unwrap();
        assert!(super::keep_guard_header(req()).headers().get("x-roll-guard").is_none(), "a caller's own header is dropped");
        let call = |tool: &str| roll_access::evaluate::McpCall { tool: tool.into(), args: Arc::new(json!({})), ip: None, elicits: false, client: None, quiet: false };
        let mut other = req();
        other.extensions_mut().insert(call("secrets_scan"));
        assert!(super::keep_guard_header(other).headers().get("x-roll-guard").is_none(), "another tool's isn't the guard");
        let mut guard = req();
        guard.extensions_mut().insert(call("guard"));
        assert!(super::keep_guard_header(guard).headers().get("x-roll-guard").is_some(), "the guard's stays");
    }

    use axum::http::HeaderMap;
    use serde_json::{Value as Json, json};
    use std::sync::{Arc, Mutex};
    struct NoClouds;
    impl roll_secrets::clouds::CloudTokens for NoClouds {
        fn gcp(&self, _: &str, _: &str, _: &roll_secrets::Value, _: u32) -> Result<(roll_secrets::Value, i64), String> {
            Err("no clouds here".into())
        }
        fn azure(&self, _: &str, _: &str, _: &str, _: &roll_secrets::Value) -> Result<(roll_secrets::Value, i64), String> {
            Err("no clouds here".into())
        }
    }

    /// roll apps' instance tokens through the real issuer, called as apps calls it
    /// (`SecretSource::issue(project, service, env, aud, ttl)`): the token's `sub` and
    /// `environment` name the instance's environment; an untrusted preview gets none. (An
    /// operator's project stands for every service: `Workspace::trusts`.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_apps_instance_token_names_its_environment_and_an_untrusted_preview_gets_none() {
        use apps_control::integrations::SecretSource;
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
        let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
        let project = ws.create("", "shop", "", None).unwrap().id;
        ws.place(roll_workspace::Scope::All, &project, roll_workspace::Kind::Service, "svc_web", 0.0, 0.0).unwrap();
        let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("secrets.db")), "secrets").unwrap();
        let opts = roll_secrets::Options {
            keyring: Ok(roll_secrets::Keyring::new(&"k".repeat(32), None).unwrap()),
            key_source: roll_secrets::KeySource::Env,
            links: super::links(super::Api::default()),
            syncs: Default::default(),
            open: true,
            trust_forwarded_for: false,
            web_url: None,
            deliver: None,
            grants: None,
            people: None,
            kms: None,
            sts: None,
            outside: None,
            directory: None,
            github: None,
            clouds: Some(roll_secrets::clouds::Clouds { issuer: "https://roll.test/v1/secrets/oidc".into(), tokens: Arc::new(NoClouds) }),
            workloads: None,
            gateways: None,
        };
        let secrets = Arc::new(roll_secrets::Secrets::new(db, ws.clone(), opts).unwrap());
        let router = roll_secrets::router(secrets.clone());
        for body in [json!({ "slug": "production" }), json!({ "slug": "pv-7", "base": "production" })] {
            let (st, e) = send(&router, "POST", &format!("/v1/secrets/projects/{project}/environments"), Some(body)).await;
            assert!(st == 201 || st == 200 || st == 409, "{st} {e}");
        }
        let apps = super::ForApps::default();
        apps.set(secrets, ws.clone());
        let hour = std::time::Duration::from_secs(3600);
        let token = apps.issue(&project, "svc_web", "production", "sts.amazonaws.com", hour).await.unwrap().expect("an issuer here");
        let claims: Json = serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(token.split('.').nth(1).unwrap()).unwrap()).unwrap();
        assert_eq!((claims["sub"].as_str(), claims["environment"].as_str(), claims["aud"].as_str()), (Some(format!("roll-apps:{project}:svc_web:production").as_str()), Some("production"), Some("sts.amazonaws.com")), "{claims}");
        assert!(apps.issue(&project, "svc_web", "pv-7", "sts.amazonaws.com", hour).await.is_err(), "an untrusted preview gets no token");
    }

    /// A search link takes whichever credential the cluster's flavor issues.
    #[test]
    fn a_search_key_is_an_api_key_or_an_opensearch_user() {
        let es = super::search_parts(&json!({ "id": "k1", "api_key": "abc" })).unwrap();
        assert_eq!(es.keys().collect::<Vec<_>>(), ["api_key"]);
        let os = super::search_parts(&json!({ "id": "roll-key-x", "username": "roll-key-x@logs", "password": "pw" })).unwrap();
        assert_eq!((os["user"].expose(), os["password"].expose()), ("roll-key-x@logs", "pw"));
        assert!(super::search_parts(&json!({ "id": "x" })).is_err());
    }

    /// roll queues in process (a broker with an AMQP listener, mq-control's routes behind the
    /// links' `Api`) and roll secrets with rolld's providers, a vhost `orders` and the operator's
    /// project `shop` with it on the canvas.
    struct Queues {
        broker: Arc<mq_core::Broker>,
        secrets: Arc<roll_secrets::Secrets>,
        ws: Arc<roll_workspace::Workspace>,
        project: String,
        _dir: tempfile::TempDir,
    }

    impl Queues {
        async fn start(people: Option<super::People>) -> Queues {
            let dir = tempfile::tempdir().unwrap();
            let broker = mq_core::Broker::start(mq_core::BrokerConfig::default(), mq_core::meta::MemMeta::new(), mq_core::log::MemStore::new()).await.unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let b = broker.clone();
            tokio::spawn(async move {
                let _ = mq_amqp::serve_listener(b, listener, mq_amqp::ServerConfig { listen: addr, amqp10: None }).await;
            });
            let endpoints = mq_control::Endpoints { amqp: Some(format!("amqp://{addr}")), ..Default::default() };
            let control = mq_control::QueuesControl::with_config(broker.clone(), endpoints, mq_control::ControlConfig::default()).unwrap();
            let router = mq_control::api::router(control);
            assert_eq!(send(&router, "POST", "/v1/queues/vhosts", Some(json!({ "name": "orders" }))).await.0, 201);
            let api = super::Api::default();
            api.set(router);
            let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
            let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
            if let Some(p) = &people {
                ws.set_owners(Arc::new(p.clone()));
            }
            let project = ws.create("", "shop", "", None).unwrap().id;
            ws.place(roll_workspace::Scope::All, &project, roll_workspace::Kind::Queue, "orders", 0.0, 0.0).unwrap();
            let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("secrets.db")), "secrets").unwrap();
            let opts = roll_secrets::Options {
                keyring: Ok(roll_secrets::Keyring::new(&"k".repeat(32), None).unwrap()),
                key_source: roll_secrets::KeySource::Env,
                links: super::links(api),
                syncs: Default::default(),
                open: true,
                trust_forwarded_for: false,
                web_url: None,
                deliver: None,
                grants: None,
                people: people.map(|p| Arc::new(p) as Arc<dyn roll_secrets::People>),
                kms: None,
                sts: None,
                outside: None,
                directory: None,
                github: None,
                clouds: None,
                workloads: None,
                gateways: None,
            };
            let secrets = Arc::new(roll_secrets::Secrets::new(db, ws.clone(), opts).unwrap());
            Queues { broker, secrets, ws, project, _dir: dir }
        }

        /// A request to the project's routes (`/environments/…`) as the operator.
        async fn call(&self, method: &str, path: &str, body: Option<Json>) -> (u16, Json) {
            send(&roll_secrets::router(self.secrets.clone()), method, &format!("/v1/secrets/projects/{}{path}", self.project), body).await
        }

        async fn ok(&self, method: &str, path: &str, body: Option<Json>) -> Json {
            let (s, v) = self.call(method, path, body).await;
            assert!((200..300).contains(&s), "{method} {path}: {s} {v}");
            v
        }

        /// `AMQP_URL` as `env` resolves it.
        async fn amqp_url(&self, env: &str) -> String {
            self.ok("GET", &format!("/environments/{env}/values"), None).await["values"]["AMQP_URL"].as_str().unwrap().to_string()
        }

        /// Users roll made on the broker, with their permissions on `orders`.
        fn users(&self) -> Vec<(String, Option<String>)> {
            let mut out: Vec<_> = self.broker.users().into_iter().filter(|u| u.name.starts_with("roll-")).map(|u| (u.name.clone(), self.broker.permission("orders", &u.name).map(|p| format!("{} {} {}", p.configure, p.write, p.read)))).collect();
            out.sort();
            out
        }
    }

    async fn send(router: &axum::Router, method: &str, path: &str, body: Option<Json>) -> (u16, Json) {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let req = axum::http::Request::builder().method(method).uri(path).header("content-type", "application/json").body(body.map_or(axum::body::Body::empty(), |b| axum::body::Body::from(b.to_string()))).unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Json::Null))
    }

    fn queue_link(field: &str, mode: &str, options: Json) -> Json {
        json!({ "link": { "kind": "queue", "ref": "orders", "field": field, "mode": mode, "options": options } })
    }

    /// Whether `url`'s user can publish to a queue `jobs` on its vhost (a broker confirm).
    async fn publishes(url: &str) -> bool {
        let conn = lapin::Connection::connect(url, lapin::ConnectionProperties::default()).await.unwrap();
        let ch = conn.create_channel().await.unwrap();
        ch.confirm_select(Default::default()).await.unwrap();
        match ch.basic_publish("".into(), "jobs".into(), Default::default(), b"job 1", Default::default()).await {
            Ok(confirm) => confirm.await.is_ok_and(|c| c.is_ack()),
            Err(_) => false,
        }
    }

    /// A queue links live (where it is), managed (a user of its own, rotated by replacing it) and
    /// dynamic (a user per lease, deleted when the lease ends).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_queue_links_live_managed_and_dynamic() {
        let q = Queues::start(None).await;
        let targets = q.ok("GET", "/links", None).await;
        assert_eq!(targets[0]["kind"], "queue", "{targets}");
        q.ok("PUT", "/environments/production/secrets/AMQP_VHOST", Some(queue_link("vhost", "live", json!({})))).await;
        q.ok("PUT", "/environments/production/secrets/AMQP_PORT", Some(queue_link("port", "live", json!({})))).await;
        q.ok("PUT", "/environments/production/secrets/AMQP_URL", Some(queue_link("url", "managed", json!({})))).await;
        let v = q.ok("GET", "/environments/production/values", None).await["values"].clone();
        assert_eq!(v["AMQP_VHOST"], "orders");
        assert!(v["AMQP_PORT"].as_str().unwrap().parse::<u16>().is_ok(), "{v}");
        let users = q.users();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].1.as_deref(), Some(".* .* .*"), "configure by default, on its vhost only");
        let first = users[0].0.clone();
        assert!(v["AMQP_URL"].as_str().unwrap().starts_with(&format!("amqp://{first}:")), "{v}");

        // Rotating makes a new user; the old one is a retired lease until its grace ends, then goes.
        q.ok("POST", "/environments/production/secrets/AMQP_URL/rotate", None).await;
        assert_eq!(q.users().len(), 2);
        let retired = q.ok("GET", "/leases", None).await[0]["id"].as_str().unwrap().to_string();
        q.ok("DELETE", &format!("/leases/{retired}"), None).await;
        let users = q.users();
        assert!(users.len() == 1 && users[0].0 != first, "{users:?}");

        // Dynamic: each resolve makes a user, deleted when its lease ends.
        q.ok("PUT", "/environments/staging/secrets/WORKER_AMQP_URL", Some(queue_link("url", "dynamic", json!({ "ttl_minutes": 15 })))).await;
        let r = q.ok("POST", "/environments/staging/resolve", Some(json!({ "consumer": "app:worker" }))).await;
        assert_eq!(q.users().len(), 2);
        q.ok("DELETE", &format!("/leases/{}", r["leases"][0].as_str().unwrap()), None).await;
        assert_eq!(q.users().len(), 1, "the lease's user is deleted");
    }

    /// A linked user connects with the URL alone, with only the access its link asked for.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_linked_queue_user_connects_with_only_the_access_it_asked_for() {
        let q = Queues::start(None).await;
        q.ok("PUT", "/environments/production/secrets/AMQP_URL", Some(queue_link("url", "managed", json!({})))).await;
        q.ok("PUT", "/environments/staging/secrets/AMQP_URL", Some(queue_link("url", "managed", json!({ "permission": "read" })))).await;
        let (writer, reader) = (q.amqp_url("production").await, q.amqp_url("staging").await);
        let conn = lapin::Connection::connect(&writer, lapin::ConnectionProperties::default()).await.unwrap();
        conn.create_channel().await.unwrap().queue_declare("jobs".into(), Default::default(), Default::default()).await.unwrap();
        assert!(publishes(&writer).await);
        assert!(!publishes(&reader).await, "a read link's user can't publish");
        let (s, e) = q.call("PUT", "/environments/development/secrets/AMQP_URL", Some(queue_link("url", "managed", json!({ "permission": "admin" })))).await;
        assert_eq!((s, e["error"].as_str()), (400, Some("a queue link's permission is read, write or configure")));
    }

    /// A person's project stands for a queue only where rolld trusts it (queues say no owner, so an
    /// admin's): a link to one on anyone else's canvas is refused.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_queue_the_project_isnt_trusted_for_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let config = roll_accounts::Config {
            api_url: "http://api.example.com".into(),
            web_url: "http://roll.example.com".into(),
            mail: roll_accounts::MailConfig::Off,
            github: None,
            google: None,
            secret_key: None,
            open: false,
            sso_allow_private: false,
        };
        let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("accounts.db")), "accounts").unwrap();
        let accounts = Arc::new(roll_accounts::Accounts::new(db, config, None).await.unwrap());
        let (_, invite, _) = accounts.invite("operator", "dev@example.com", false, None).unwrap();
        let mail = accounts.email_start("chal-test", "dev@example.com", Some(&invite)).unwrap().unwrap();
        let (user, _) = accounts.email_verify("chal-test", mail.subject.split(' ').next().unwrap()).unwrap();
        let q = Queues::start(Some(super::People { accounts: accounts.clone(), code: None, search: None })).await;
        let mine = q.ws.create(&user, "side-project", "", None).unwrap().id;
        q.ws.place(roll_workspace::Scope::Member(&user), &mine, roll_workspace::Kind::Queue, "orders", 0.0, 0.0).unwrap();
        let path = format!("/v1/secrets/projects/{mine}/environments/production/secrets/AMQP_URL");
        let (s, e) = send(&roll_secrets::router(q.secrets.clone()), "PUT", &path, Some(queue_link("url", "managed", json!({})))).await;
        assert!(s == 400 && e["error"].as_str().is_some_and(|m| m.starts_with("queue orders is on this project's canvas, but roll can't tell it's yours")), "{s} {e}");
        assert!(q.users().is_empty(), "nothing issued");
        // The same canvas once its person is an admin.
        accounts.update_user(&user, Some(true), None).unwrap();
        let (s, e) = send(&roll_secrets::router(q.secrets.clone()), "PUT", &path, Some(queue_link("url", "managed", json!({})))).await;
        assert!((200..300).contains(&s), "{s} {e}");
    }


    /// A service's deploy gets its project's environment as `app:<svc>`; where roll secrets has no
    /// such project or environment it gets nothing (a service needn't use secrets), never a failure.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_service_gets_its_projects_environment_and_nothing_where_there_is_none() {
        use apps_control::integrations::SecretSource;
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
        let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
        let project = ws.create("", "shop", "", None).unwrap().id;
        let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("secrets.db")), "secrets").unwrap();
        let opts = roll_secrets::Options {
            keyring: Ok(roll_secrets::Keyring::new(&"k".repeat(32), None).unwrap()),
            key_source: roll_secrets::KeySource::Env,
            links: Default::default(),
            syncs: Default::default(),
            open: true,
            trust_forwarded_for: false,
            web_url: None,
            deliver: None,
            grants: None,
            people: None,
            kms: None,
            sts: None,
            outside: None,
            directory: None,
            github: None,
            clouds: None,
            workloads: None,
            gateways: None,
        };
        let secrets = Arc::new(roll_secrets::Secrets::new(db, ws.clone(), opts).unwrap());
        let set = axum::http::Request::put(format!("/v1/secrets/projects/{project}/environments/production/secrets/DB_URL"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(json!({ "value": "postgres://db/shop" }).to_string()))
            .unwrap();
        assert!(roll_secrets::router(secrets.clone()).oneshot(set).await.unwrap().status().is_success());
        // A sensitive key reaches the service flagged, so apps seals it and never shows it.
        let ops = json!({ "ops": [{ "op": "set", "key": "PAYMENTS_KEY", "value": "sk_live_x", "tags": ["sensitive"] }] });
        let set = axum::http::Request::post(format!("/v1/secrets/projects/{project}/environments/production/changes")).header("content-type", "application/json").body(axum::body::Body::from(ops.to_string())).unwrap();
        assert!(roll_secrets::router(secrets.clone()).oneshot(set).await.unwrap().status().is_success());
        let apps = super::ForApps::default();
        apps.set(secrets.clone(), ws.clone());
        let got = apps.resolve(&project, "production", "svc_web").await.unwrap();
        assert_eq!(got.iter().map(|v| (v.name.as_str(), v.value.as_str(), v.linked, v.sensitive)).collect::<Vec<_>>(), [("DB_URL", "postgres://db/shop", false, false), ("PAYMENTS_KEY", "sk_live_x", false, true)]);
        assert!(apps.resolve(&project, "pr-9", "svc_web").await.unwrap().is_empty());
        assert!(apps.resolve("prj_gone", "production", "svc_web").await.unwrap().is_empty());
        // Seeing them in the clear in apps is roll secrets' call: the operator may, a stranger's
        // token for another project may not.
        let operator = apps_control::integrations::Asker { operator: true, user: None, grant: None };
        assert!(apps.may_reveal(operator, &project, "production").await);
        let rule = roll_auth::scope::Rule { product: "secrets".into(), resources: roll_auth::scope::Resources::Some(vec![roll_auth::scope::Selector::Project { project: "prj_other".into() }]), environments: None, paths: None, unprotected: false, actions: std::collections::BTreeMap::from([(roll_auth::scope::Action::Reveal, roll_auth::scope::Effect::Allow)]) };
        let grant = roll_auth::Grant { user: None, ask_otherwise: false, id: "tok_x".into(), name: "x".into(), kind: roll_auth::GrantKind::Token, scope: Arc::new(roll_auth::scope::Scope(vec![rule])) };
        assert!(!apps.may_reveal(apps_control::integrations::Asker { operator: true, user: None, grant: Some(&grant) }, &project, "production").await);
        // The read is the service's in the audit log.
        let audit = secrets.audit(&project, roll_secrets::AuditFilter::default(), &roll_secrets::Caller::operator()).unwrap();
        assert!(audit.iter().any(|a| a.actor == "app:svc_web"), "{audit:?}");
        // Apps learns a deploy's secrets changed since: the digest moves with a value, nothing
        // decrypted, and there's none for an environment roll secrets doesn't have.
        let before = apps.digest(&project, "production").await.unwrap().expect("a digest");
        let set = axum::http::Request::put(format!("/v1/secrets/projects/{project}/environments/production/secrets/DB_URL"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(json!({ "value": "postgres://db/shop2" }).to_string()))
            .unwrap();
        assert!(roll_secrets::router(secrets.clone()).oneshot(set).await.unwrap().status().is_success());
        assert_ne!(apps.digest(&project, "production").await.unwrap(), Some(before));
        assert_eq!(apps.digest(&project, "pr-9").await.unwrap(), None);
        // A variable's roll:// reference, read as the service (resolve_for): only once a project
        // that stands for it holds it on its canvas.
        let reference = "roll://production/DB_URL";
        assert!(apps.resolve_ref(&project, "production", "svc_web", reference).await.is_err(), "on no canvas");
        ws.place(roll_workspace::Scope::All, &project, roll_workspace::Kind::Service, "svc_web", 0.0, 0.0).unwrap();
        assert_eq!(apps.resolve_ref(&project, "production", "svc_web", reference).await.unwrap(), "postgres://db/shop2");
        let audit = secrets.audit(&project, roll_secrets::AuditFilter::default(), &roll_secrets::Caller::operator()).unwrap();
        assert!(audit.iter().any(|a| a.actor.contains("apps:service:svc_web")), "{audit:?}");
    }

    /// A decoy key's token, used where it was planted: roll-access refuses it and roll secrets
    /// raises `decoy_used` on the key; deleting the key revokes the token (rolld's wiring).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_decoy_keys_token_raises_decoy_used_and_dies_with_its_key() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
        let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
        let project = ws.create("", "shop", "", None).unwrap().id;
        let access = roll_access::Access::with_db(code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("access.db")), "access").unwrap(), Default::default(), Default::default()).unwrap();
        let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("secrets.db")), "secrets").unwrap();
        let opts = roll_secrets::Options {
            keyring: Ok(roll_secrets::Keyring::new(&"k".repeat(32), None).unwrap()),
            key_source: roll_secrets::KeySource::Env,
            links: Default::default(),
            syncs: Default::default(),
            open: true,
            trust_forwarded_for: false,
            web_url: None,
            deliver: None,
            grants: Some(access.clone() as Arc<dyn roll_auth::Grants>),
            people: None,
            kms: None,
            sts: None,
            outside: None,
            directory: None,
            github: None,
            clouds: None,
            workloads: Some(Arc::new(super::Workloads(access.clone()))),
            gateways: None,
        };
        let secrets = Arc::new(roll_secrets::Secrets::new(db, ws, opts).unwrap());
        super::tell_decoys(&access, secrets.clone());
        let call = |method: &str, path: &str, body: Option<Json>| {
            let req = axum::http::Request::builder().method(method).uri(format!("/v1/secrets/projects/{project}/environments/production{path}")).header("content-type", "application/json");
            let req = req.body(axum::body::Body::from(body.map(|b| b.to_string()).unwrap_or_default())).unwrap();
            roll_secrets::router(secrets.clone()).oneshot(req)
        };
        let made = call("POST", "/decoys", Some(json!({ "key": "OLD_DEPLOY_TOKEN", "kind": "rlt" }))).await.unwrap();
        assert_eq!(made.status(), axum::http::StatusCode::CREATED);
        let operator = roll_secrets::Caller::operator();
        let (value, _) = secrets.reveal(&project, "production", "/", "OLD_DEPLOY_TOKEN", false, &operator).unwrap();
        // roll-access's own decoy: never a grant a product acts on.
        let g = access.store.by_secret(value.expose()).unwrap().expect("a decoy grant");
        assert!(g.decoy);
        assert!(roll_auth::Grants::grant(&*access, &g.id).is_none());
        // The gate's decoy path.
        access.decoy_tripped(&g, Some("198.51.100.4".into()), "/v1/secrets/whoami", None);
        let mut first = None;
        for _ in 0..50 {
            first = secrets.project_health(&project, &operator, false).unwrap().into_iter().next();
            if first.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let first = first.expect("a finding");
        assert_eq!((first.issue, first.key.as_str()), (roll_secrets::health::Issue::DecoyUsed, "OLD_DEPLOY_TOKEN"));
        assert!(first.detail.contains("198.51.100.4"), "{}", first.detail);
        assert_eq!(first.decoy.as_ref().and_then(|d| d.used_route.as_deref()), Some("/v1/secrets/whoami"));
        assert!(call("DELETE", "/secrets/OLD_DEPLOY_TOKEN", None).await.unwrap().status().is_success());
        assert!(access.store.by_secret(value.expose()).unwrap().is_none());
        assert!(access.store.get(&g.id).unwrap().revoked_at.is_some());
    }

    /// A frozen folder stops roll's own readers, which do everything in-process: a service's
    /// deploy (`ForApps`) and a check or sandbox (`ForCode`, untrusted) both get 423 `frozen`.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_frozen_folder_stops_services_checks_and_sandboxes() {
        use apps_control::integrations::SecretSource;
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
        let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
        let project = ws.create("", "shop", "", None).unwrap().id;
        let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("secrets.db")), "secrets").unwrap();
        let opts = roll_secrets::Options {
            keyring: Ok(roll_secrets::Keyring::new(&"k".repeat(32), None).unwrap()),
            key_source: roll_secrets::KeySource::Env,
            links: Default::default(),
            syncs: Default::default(),
            open: true,
            trust_forwarded_for: false,
            web_url: None,
            deliver: None,
            grants: None,
            people: None,
            kms: None,
            sts: None,
            outside: None,
            directory: None,
            github: None,
            clouds: None,
            workloads: None,
            gateways: None,
        };
        let secrets = Arc::new(roll_secrets::Secrets::new(db, ws.clone(), opts).unwrap());
        let post = |method: &str, path: &str, body: Json| {
            axum::http::Request::builder().method(method).uri(format!("/v1/secrets/projects/{project}/environments/staging{path}")).header("content-type", "application/json").body(axum::body::Body::from(body.to_string())).unwrap()
        };
        let set = roll_secrets::router(secrets.clone()).oneshot(post("PUT", "/secrets/STRIPE_KEY", json!({ "value": "sk_test_fake_staging" }))).await.unwrap();
        assert!(set.status().is_success());
        let cut = roll_secrets::router(secrets.clone()).oneshot(post("POST", "/reach/cut-off?path=/", json!({ "why": "fake incident", "tokens": false, "freeze": true }))).await.unwrap();
        assert!(cut.status().is_success(), "{}", cut.status());
        let frozen = |e: anyhow::Error| matches!(e.downcast_ref::<roll_secrets::Error>(), Some(roll_secrets::Error::Frozen(_)));
        let apps = super::ForApps::default();
        apps.set(secrets.clone(), ws.clone());
        assert!(frozen(apps.resolve(&project, "staging", "svc_web").await.unwrap_err()), "a service's deploy");
        let code = super::ForCode::default();
        code.set(secrets.clone(), ws);
        assert!(frozen(code.resolve(project.clone(), "staging", "check:acme/web", None).await.unwrap_err()), "a check or a sandbox");
    }

    /// An incident's cut-off revokes through roll-access's own bulk revoke: every token that
    /// reaches the folder, 500 a call (one write each, all or none), each with why; none other.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cut_off_revokes_tokens_through_roll_access_500_at_a_time() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
        let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
        let project = ws.create("", "shop", "", None).unwrap().id;
        let access = roll_access::Access::open(&code_sql::Target::Sqlite(dir.path().join("access.db")), Default::default(), Default::default(), Default::default()).unwrap();
        let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("secrets.db")), "secrets").unwrap();
        let opts = roll_secrets::Options {
            keyring: Ok(roll_secrets::Keyring::new(&"k".repeat(32), None).unwrap()),
            key_source: roll_secrets::KeySource::Env,
            links: Default::default(),
            syncs: Default::default(),
            open: true,
            trust_forwarded_for: false,
            web_url: None,
            deliver: None,
            grants: Some(access.clone() as Arc<dyn roll_auth::Grants>),
            people: None,
            kms: None,
            sts: None,
            outside: None,
            directory: None,
            github: None,
            clouds: None,
            workloads: Some(Arc::new(super::Workloads(access.clone()))),
            gateways: None,
        };
        let secrets = Arc::new(roll_secrets::Secrets::new(db, ws, opts).unwrap());
        let token = |env: &str, n: usize| {
            let rule = roll_secrets::access::rule(&project, Some(vec![env.into()]), None, &[roll_secrets::Action::Read, roll_secrets::Action::Reveal]);
            let new = roll_access::store::NewGrant { name: format!("fake-{env}-{n}"), rules: roll_auth::scope::Scope(vec![rule]), ..Default::default() };
            access.store.create(&new, roll_auth::GrantKind::Token, "operator", None, None).unwrap().0.id
        };
        let reaching: Vec<String> = (0..501).map(|n| token("production", n)).collect();
        let other = token("staging", 0);
        let cut = axum::http::Request::post(format!("/v1/secrets/projects/{project}/environments/production/reach/cut-off?path=/"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(json!({ "why": "incident 9: a fake leak" }).to_string()))
            .unwrap();
        let resp = roll_secrets::router(secrets).oneshot(cut).await.unwrap();
        assert!(resp.status().is_success(), "{}", resp.status());
        let r: Json = serde_json::from_slice(&http_body_util::BodyExt::collect(resp.into_body()).await.unwrap().to_bytes()).unwrap();
        assert_eq!((r["tokens"].as_array().unwrap().len(), r["batches"].as_u64(), r["failed"].clone()), (501, Some(2), json!([])));
        for id in [&reaching[0], &reaching[500]] {
            let g = access.store.get(id).unwrap();
            assert!(g.revoked_at.is_some() && g.revoked_why.as_deref().is_some_and(|w| w.contains("incident 9")), "{g:?}");
        }
        assert!(access.store.get(&other).unwrap().revoked_at.is_none(), "a token that doesn't reach it stays");
    }

    /// A sandbox gets a project's environment only as far as whoever creates it may reveal it in
    /// roll secrets: managing the code owner isn't enough, and the audit row names them.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_sandbox_gets_an_environment_only_for_someone_who_may_reveal_it() {
        use code_control::SecretEnv;
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
        let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
        let project = ws.create("", "shop", "", None).unwrap().id;
        ws.place(roll_workspace::Scope::All, &project, roll_workspace::Kind::Repo, "acme/api", 0.0, 0.0).unwrap();
        let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("secrets.db")), "secrets").unwrap();
        let opts = roll_secrets::Options {
            keyring: Ok(roll_secrets::Keyring::new(&"k".repeat(32), None).unwrap()),
            key_source: roll_secrets::KeySource::Env,
            links: Default::default(),
            syncs: Default::default(),
            open: false,
            trust_forwarded_for: false,
            web_url: None,
            deliver: None,
            grants: None,
            people: None,
            kms: None,
            sts: None,
            outside: None,
            directory: None,
            github: None,
            clouds: None,
            workloads: None,
            gateways: None,
        };
        let secrets = Arc::new(roll_secrets::Secrets::new(db, ws.clone(), opts).unwrap());
        let mut set = axum::http::Request::put(format!("/v1/secrets/projects/{project}/environments/development/secrets/DB_URL"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(json!({ "value": "postgres://dev" }).to_string()))
            .unwrap();
        set.extensions_mut().insert(roll_auth::Operator);
        let r = roll_secrets::router(secrets.clone()).oneshot(set).await.unwrap();
        assert!(r.status().is_success(), "{}", r.status());
        let code = super::ForCode::default();
        code.set(secrets.clone(), ws);
        let grant = |id: &str, on: &str| {
            let rule = roll_auth::scope::Rule { product: "secrets".into(), resources: roll_auth::scope::Resources::Some(vec![roll_auth::scope::Selector::Project { project: on.into() }]), environments: None, paths: None, unprotected: false, actions: std::collections::BTreeMap::from([(roll_auth::scope::Action::Reveal, roll_auth::scope::Effect::Allow)]) };
            roll_auth::Grant { user: None, ask_otherwise: false, id: id.into(), name: id.into(), kind: roll_auth::GrantKind::Token, scope: Arc::new(roll_auth::scope::Scope(vec![rule])) }
        };
        let refused = |r: anyhow::Result<code_control::Revealed>| r.err().and_then(|e| e.downcast::<code_control::Refused>().ok()).map(|r| r.0);
        // A token with code manage but no reveal here, and a code owner token (no one to roll
        // secrets): refused, and nothing read.
        let other = grant("tok_other", "prj_other");
        let asker = code_control::Asker { operator: true, user: None, grant: Some(&other) };
        assert!(refused(code.for_owner("acme", "shop", "development", "sandbox:sbx_1", asker).await).is_some_and(|m| m.contains("may not reveal development")));
        assert!(refused(code.for_owner("acme", "shop", "development", "sandbox:sbx_1", code_control::Asker::default()).await).is_some());
        // A token that may reveal it, and the operator: the values, and who copied them.
        let mine = grant("tok_mine", &project);
        let got = code.for_owner("acme", "shop", "development", "sandbox:sbx_2", code_control::Asker { operator: true, user: None, grant: Some(&mine) }).await.unwrap();
        assert_eq!((got.by.as_str(), got.values), ("token:tok_mine", vec![("DB_URL".to_string(), "postgres://dev".to_string())]));
        let got = code.for_owner("acme", "shop", "development", "sandbox:sbx_3", code_control::Asker { operator: true, user: None, grant: None }).await.unwrap();
        assert_eq!(got.by, "operator");
        let audit = secrets.audit(&project, roll_secrets::AuditFilter::default(), &roll_secrets::Caller::operator()).unwrap();
        // The read is the person's or token's own, for the sandbox.
        let reads: Vec<(&str, &str)> = audit.iter().filter(|a| a.summary.contains("sandbox:")).map(|a| (a.actor.as_str(), a.summary.as_str())).collect();
        assert!(reads.contains(&("token:tok_mine", "resolved 1 value for sandbox:sbx_2 for token:tok_mine")), "{reads:?}");
        assert!(reads.contains(&("operator", "resolved 1 value for sandbox:sbx_3 for operator")), "{reads:?}");
        assert!(!reads.iter().any(|(_, s)| s.contains("sbx_1")), "a refused copy read nothing: {reads:?}");
    }

    /// An instance's token from roll's issuer goes only to a service its project stands for, in an
    /// environment roll secrets has, and never to an untrusted preview (a fork's code).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_service_gets_an_issuer_token_only_from_a_project_that_stands_for_it() {
        use apps_control::integrations::SecretSource;
        use tower::ServiceExt;
        struct Nobody;
        impl roll_workspace::Owners for Nobody {
            fn trusts(&self, _: &str, _: roll_workspace::Kind, _: &str) -> bool {
                false
            }
            fn owns(&self, _: &str, _: roll_workspace::Kind, _: &str) -> bool {
                false
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
        let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
        ws.set_owners(Arc::new(Nobody));
        let (operators, adas) = (ws.create("", "shop", "", None).unwrap().id, ws.create("usr_ada", "mine", "", None).unwrap().id);
        let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("secrets.db")), "secrets").unwrap();
        let clouds = roll_secrets::clouds::Clouds { issuer: "https://roll.example/v1/secrets/oidc".into(), tokens: Arc::new(super::clouds::RollClouds::new()) };
        let opts = roll_secrets::Options { keyring: Ok(roll_secrets::Keyring::new(&"k".repeat(32), None).unwrap()), key_source: roll_secrets::KeySource::Env, links: Default::default(), syncs: Default::default(), open: true, trust_forwarded_for: false, web_url: None, deliver: None, grants: None, people: None, kms: None, sts: None, outside: None, github: None, clouds: Some(clouds), workloads: None, directory: None, gateways: None };
        let secrets = Arc::new(roll_secrets::Secrets::new(db, ws.clone(), opts).unwrap());
        let apps = super::ForApps::default();
        apps.set(secrets.clone(), ws);
        let ttl = std::time::Duration::from_secs(900);
        let token = apps.issue(&operators, "svc_web", "production", "sts.amazonaws.com", ttl).await.unwrap().expect("a token");
        use base64::Engine as _;
        let claims: Json = serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(token.split('.').nth(1).unwrap()).unwrap()).unwrap();
        assert_eq!(claims["sub"], json!(format!("roll-apps:{operators}:svc_web:production")));
        // A person's project stands only for their own services.
        assert!(apps.issue(&adas, "svc_theirs", "production", "sts.amazonaws.com", ttl).await.is_err());
        // Previews: a fork's (untrusted) gets no token, a vetted one does, under its own slug; an
        // environment roll secrets doesn't have gets none either.
        for (slug, trusted) in [("pv-fork", false), ("pv-vetted", true)] {
            let new = axum::http::Request::post(format!("/v1/secrets/projects/{operators}/environments")).header("content-type", "application/json").body(axum::body::Body::from(json!({ "slug": slug, "base": "production", "trusted": trusted }).to_string())).unwrap();
            assert!(roll_secrets::router(secrets.clone()).oneshot(new).await.unwrap().status().is_success());
        }
        let refused = apps.issue(&operators, "svc_web", "pv-fork", "sts.amazonaws.com", ttl).await.unwrap_err();
        assert!(refused.to_string().contains("untrusted preview"), "{refused}");
        let token = apps.issue(&operators, "svc_web", "pv-vetted", "sts.amazonaws.com", ttl).await.unwrap().expect("a vetted preview's token");
        let claims: Json = serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(token.split('.').nth(1).unwrap()).unwrap()).unwrap();
        assert_eq!((claims["sub"].clone(), claims["environment"].clone()), (json!(format!("roll-apps:{operators}:svc_web:pv-vetted")), json!("pv-vetted")));
        assert!(apps.issue(&operators, "svc_web", "nowhere", "sts.amazonaws.com", ttl).await.is_err());
    }

    /// A customer's key wraps a project's data key: Encrypt and Decrypt name the key and carry the
    /// project in the encryption context, in the key's own region; a refusal is KMS's code.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_customer_key_wraps_with_the_project_as_its_context() {
        use base64::Engine as _;
        use roll_secrets::kms::ProjectKms;
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        let seen: Arc<Mutex<Vec<(String, String, Json)>>> = Default::default();
        let s = seen.clone();
        let app = axum::Router::new().fallback(move |headers: HeaderMap, body: axum::body::Bytes| {
            let s = s.clone();
            async move {
                let req: Json = serde_json::from_slice(&body).unwrap_or_default();
                let target = headers.get("x-amz-target").and_then(|t| t.to_str().ok()).unwrap_or_default().to_string();
                let region = headers.get("authorization").and_then(|a| a.to_str().ok()).and_then(|a| a.split('/').nth(2)).unwrap_or_default().to_string();
                s.lock().unwrap().push((target.clone(), region, req.clone()));
                let ok = req["KeyId"] == "arn:aws:kms:ap-south-1:999999999999:key/c1" && req["EncryptionContext"] == json!({ "roll": "secrets", "project": "prj_a" });
                match (target.as_str(), ok) {
                    ("TrentService.Encrypt", true) => (axum::http::StatusCode::OK, axum::Json(json!({ "KeyId": req["KeyId"], "CiphertextBlob": b64(b"wrapped") }))),
                    ("TrentService.Decrypt", true) if req["CiphertextBlob"] == json!(b64(b"wrapped")) => (axum::http::StatusCode::OK, axum::Json(json!({ "KeyId": req["KeyId"], "Plaintext": b64(b"v1:inner") }))),
                    _ => (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({ "__type": "AccessDeniedException", "message": "no" }))),
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conf = aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new("eu-west-1"))
            .endpoint_url(&base)
            .credentials_provider(aws_sdk_kms::config::SharedCredentialsProvider::new(aws_sdk_kms::config::Credentials::new("AKIATEST", "secret", None, None, "test")))
            .build();
        let kms = super::CustomerKms::new(Some(conf));
        let key = "arn:aws:kms:ap-south-1:999999999999:key/c1";
        let ct = kms.encrypt(key, "prj_a", b"v1:inner").unwrap();
        assert_eq!(kms.decrypt(key, "prj_a", &ct).unwrap().as_slice(), b"v1:inner");
        assert_eq!(kms.decrypt(key, "prj_other", &ct).unwrap_err(), "AccessDeniedException", "another project's context");
        let regions: Vec<String> = seen.lock().unwrap().iter().map(|(_, r, _)| r.clone()).collect();
        assert!(regions.iter().all(|r| r == "ap-south-1"), "the key's own region: {regions:?}");
    }

    /// A project's AWS role is assumed with its ExternalId and the session's name and duration;
    /// a refusal is STS's code.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_aws_role_is_assumed_with_the_projects_external_id() {
        use roll_secrets::aws::Sts;
        let seen: Arc<Mutex<Vec<String>>> = Default::default();
        let s = seen.clone();
        let app = axum::Router::new().fallback(move |body: axum::body::Bytes| {
            let s = s.clone();
            async move {
                let form = String::from_utf8_lossy(&body).to_string();
                s.lock().unwrap().push(form.clone());
                if form.contains("ExternalId=roll-secrets%3Aprj_a") && form.contains("RoleArn=arn%3Aaws%3Aiam%3A%3A123456789012%3Arole%2Froll") {
                    let xml = r#"<AssumeRoleResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/"><AssumeRoleResult><Credentials><AccessKeyId>ASIATEST</AccessKeyId><SecretAccessKey>secret</SecretAccessKey><SessionToken>token</SessionToken><Expiration>2030-01-01T00:00:00Z</Expiration></Credentials><AssumedRoleUser><Arn>arn:aws:sts::123456789012:assumed-role/roll/s</Arn><AssumedRoleId>AROA:s</AssumedRoleId></AssumedRoleUser></AssumeRoleResult></AssumeRoleResponse>"#;
                    (axum::http::StatusCode::OK, [("content-type", "text/xml")], xml.to_string())
                } else {
                    let xml = r#"<ErrorResponse><Error><Type>Sender</Type><Code>AccessDenied</Code><Message>no</Message></Error></ErrorResponse>"#;
                    (axum::http::StatusCode::FORBIDDEN, [("content-type", "text/xml")], xml.to_string())
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conf = aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new("eu-west-1"))
            .endpoint_url(&base)
            .credentials_provider(aws_sdk_sts::config::SharedCredentialsProvider::new(aws_sdk_sts::config::Credentials::new("AKIATEST", "secret", None, None, "test")))
            .build();
        let sts = super::RollSts::new(Some(conf));
        let s = sts.assume("arn:aws:iam::123456789012:role/roll", "roll-secrets:prj_a", "roll-app-web", 1200).unwrap();
        assert_eq!((s.access_key_id.expose(), s.session_token.expose(), s.expires_at), ("ASIATEST", "token", 1_893_456_000_000));
        let form = seen.lock().unwrap()[0].clone();
        assert!(form.contains("RoleSessionName=roll-app-web") && form.contains("DurationSeconds=1200"), "{form}");
        assert_eq!(sts.assume("arn:aws:iam::123456789012:role/roll", "roll-secrets:prj_other", "x", 900).err().as_deref(), Some("AccessDenied"));
    }

    /// A KMS that decrypts one ciphertext, and only with roll secrets' encryption context.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_deployment_key_comes_from_kms_with_roll_secrets_context() {
        use base64::Engine as _;
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        let seen: Arc<Mutex<Vec<Json>>> = Default::default();
        let s = seen.clone();
        let app = axum::Router::new().fallback(move |headers: HeaderMap, body: axum::body::Bytes| {
            let s = s.clone();
            async move {
                let req: Json = serde_json::from_slice(&body).unwrap_or_default();
                s.lock().unwrap().push(req.clone());
                let decrypt = headers.get("x-amz-target").is_some_and(|t| t == "TrentService.Decrypt");
                if decrypt && req["EncryptionContext"] == json!({ "roll": "secrets" }) && req["CiphertextBlob"] == json!(b64(b"sealed")) {
                    (axum::http::StatusCode::OK, axum::Json(json!({ "KeyId": "arn:aws:kms:eu-west-1:1:key/k", "Plaintext": b64(b"a-deployment-key-of-32-characters!!") })))
                } else {
                    (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({ "__type": "InvalidCiphertextException", "message": "no" })))
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conf = aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new("eu-west-1"))
            .endpoint_url(&base)
            .credentials_provider(aws_sdk_kms::config::SharedCredentialsProvider::new(aws_sdk_kms::config::Credentials::new("AKIATEST", "secret", None, None, "test")))
            .build();
        let key = super::kms_decrypt(&conf, &b64(b"sealed")).await.unwrap();
        assert_eq!(key.expose(), "a-deployment-key-of-32-characters!!");
        assert!(roll_secrets::Keyring::new(key.expose(), None).is_ok());
        let wrong = super::kms_decrypt(&conf, &b64(b"other")).await.unwrap_err();
        assert_eq!(wrong, "KMS couldn't decrypt the deployment key: InvalidCiphertextException");
    }
}
