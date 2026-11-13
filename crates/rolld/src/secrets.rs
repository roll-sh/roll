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
use roll_secrets::links::{Field, Issued, Link, LinkProvider, Links, Mode};
use roll_secrets::{Error, KeySource, Keyring, Secrets, Value};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use tower::ServiceExt;

mod syncs;

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
        let router = self.0.get().ok_or_else(|| Error::Link("the product APIs aren't up yet".into()))?.clone();
        let mut req = Request::builder().method(method).uri(path).header("content-type", "application/json");
        req.extensions_mut().map(|e| e.insert(roll_auth::Operator));
        let req = req.body(body.map_or(Body::empty(), |b| Body::from(b.to_string()))).map_err(|e| Error::Internal(e.to_string()))?;
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

    /// Delete a product's resource; one that's already gone (404) is done.
    fn delete_gone(&self, path: &str) -> roll_secrets::Result<()> {
        match self.call_status(Method::DELETE, path, None)? {
            (s, _) if s.is_success() || s == axum::http::StatusCode::NOT_FOUND => Ok(()),
            (s, v) => Err(Error::Link(format!("DELETE {}: {} {}", path.split('?').next().unwrap_or(path), s.as_u16(), v.get("error").and_then(Json::as_str).unwrap_or(s.as_str())))),
        }
    }
}

/// The deployment key: `ROLL_SECRET_KEY_KMS` (a KMS ciphertext, decrypted at startup), else
/// `ROLL_SECRET_KEY`, else (one node) a generated key file.
fn keyring(home: &Path, shared: bool) -> (Result<Keyring, String>, KeySource) {
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
    if let Some(k) = env("ROLL_SECRET_KEY") {
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

pub fn open(args: &crate::Args, home: &Path, workspace: Arc<roll_workspace::Workspace>, open: bool, api: Api, grants: Option<Arc<dyn roll_auth::Grants>>) -> anyhow::Result<Arc<Secrets>> {
    let system = args.code.cluster.system_database.as_deref().filter(|u| !u.is_empty());
    let target = match system {
        Some(url) => code_sql::Target::Postgres { url: url.to_string(), schema: "secrets".into() },
        None => code_sql::Target::Sqlite(home.join("secrets.db")),
    };
    let (keyring, key_source) = keyring(home, system.is_some());
    if let Err(why) = &keyring {
        tracing::error!("roll secrets is unavailable: {why}");
    }
    let pg = code_sql::PgOptions { max_connections: args.db_budget()?.secrets, ca: args.code.code_db_ca.clone(), ..Default::default() };
    let web_url = std::env::var("ROLL_WEB_URL").ok().filter(|u| !u.trim().is_empty());
    // Syncs' webhooks go to private addresses only where roll code's may (local development).
    let allow_private = args.code_webhook_allow_private.unwrap_or(open);
    let opts = roll_secrets::Options { keyring, key_source, links: links(api.clone()), syncs: syncs::kinds(api.clone(), allow_private), open, trust_forwarded_for: args.code_trust_forwarded_for, web_url, deliver: Some(Arc::new(syncs::AuditDeliver::new(allow_private, api))), grants };
    // Opening blocks on the database.
    let secrets = Arc::new(tokio::task::block_in_place(|| Secrets::open(&target, pg, workspace.clone(), opts))?);
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
    Ok(secrets)
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

/// The link providers of every product rolld serves.
fn links(api: Api) -> Links {
    let mut l = Links::default();
    l.register(Arc::new(Postgres(api.clone())));
    l.register(Arc::new(Redis(api.clone())));
    l.register(Arc::new(Stream(api.clone())));
    l.register(Arc::new(Bucket(api.clone())));
    l.register(Arc::new(Nosql(api.clone())));
    l.register(Arc::new(Search(api)));
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
    /// Ends the role's sessions and drops it. What it made (tables from a migration) is given to
    /// `roll` first, so revoking a credential never drops data.
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

/// Search clusters (roll search): where clients reach one (live), and API keys of their own
/// (managed or dynamic) from `POST /v1/search/clusters/{id}/keys`; never the cluster's own
/// password, which rotating would take from everyone else.
struct Search(Api);

impl LinkProvider for Search {
    fn kind(&self) -> &str {
        "search"
    }
    fn fields(&self) -> &[Field] {
        const F: &[Field] = &[
            live("url", "URL", false, "ELASTICSEARCH_URL"),
            live("host", "Host", false, "ELASTICSEARCH_HOST"),
            live("port", "Port", false, "ELASTICSEARCH_PORT"),
            live("username", "Cluster name", false, "ELASTICSEARCH_USERNAME"),
            issued("api_key", "API key (Authorization: ApiKey …)", true, "ELASTICSEARCH_API_KEY", "key"),
        ];
        F
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
        let mut body = json!({ "name": label.chars().take(64).collect::<String>() });
        // A dynamic key expires by itself a little after its lease, should a revoke fail.
        if let Some(m) = ttl {
            body["expires_in_s"] = json!((m + 5) * 60);
        }
        let k = self.0.call(Method::POST, &format!("/v1/search/clusters/{}/keys", enc(r#ref)), Some(body))?;
        let id = text(&k, "/id")?;
        Ok(Issued { parts: BTreeMap::from([("api_key".to_string(), text(&k, "/api_key")?)]), handle: json!({ "id": id.expose() }) })
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

/// roll secrets for roll code's checks and sandboxes (`code_control::SecretEnv`), set once the
/// store is open (it opens after roll code starts).
#[derive(Clone, Default)]
pub struct ForCode(Arc<OnceLock<(Arc<Secrets>, Arc<roll_workspace::Workspace>)>>);

impl ForCode {
    pub fn set(&self, secrets: Arc<Secrets>, workspace: Arc<roll_workspace::Workspace>) {
        let _ = self.0.set((secrets, workspace));
    }

    /// `environment` (`env` or `env:/folder`) of `project`, for `consumer`: never a protected
    /// environment, never with an unresolved reference.
    async fn resolve(&self, project: String, environment: &str, consumer: &str) -> anyhow::Result<Vec<(String, String)>> {
        let (secrets, _) = self.0.get().cloned().ok_or_else(|| anyhow::anyhow!("roll secrets is starting"))?;
        let (slug, path) = environment.split_once(':').unwrap_or((environment, "/"));
        let (slug, path, consumer) = (slug.to_string(), path.to_string(), consumer.to_string());
        tokio::task::spawn_blocking(move || {
            // Never a protected environment: not asked for, nor through a reference, an import or
            // an ephemeral environment's base.
            let caller = secrets.untrusted(&project, &roll_secrets::Caller::consumer(&consumer))?;
            if !caller.allows(roll_secrets::Action::Reveal, &roll_secrets::Scope::env(&project, &slug)) {
                anyhow::bail!("{slug} is protected: protected environments never go to checks or sandboxes");
            }
            let v = secrets.values(&project, &slug, &path, true, Some(&consumer), &caller)?;
            if let Some(p) = v.problems.first() {
                anyhow::bail!("{} of {slug} didn't resolve ({}: {})", v.problems.len(), p.key, p.problem);
            }
            Ok(v.values.into_iter().map(|(k, v)| (k, v.expose().to_string())).collect())
        })
        .await?
    }

    fn workspace(&self) -> anyhow::Result<Arc<roll_workspace::Workspace>> {
        Ok(self.0.get().ok_or_else(|| anyhow::anyhow!("roll secrets is starting"))?.1.clone())
    }
}

#[async_trait::async_trait]
impl code_control::SecretEnv for ForCode {
    async fn missing(&self, repo: &str, keys: Vec<String>) -> anyhow::Result<Vec<(String, Vec<String>)>> {
        let ws = self.workspace()?;
        let (secrets, _) = self.0.get().cloned().ok_or_else(|| anyhow::anyhow!("roll secrets is starting"))?;
        let r = repo.to_string();
        tokio::task::spawn_blocking(move || {
            // Every project with the repo on its canvas: a key missing from any of their environments.
            let mut out: std::collections::BTreeMap<String, Vec<String>> = Default::default();
            for p in ws.list()?.into_iter().filter(|p| p.resources.iter().any(|x| x.kind == roll_workspace::Kind::Repo && x.r#ref == r)) {
                for (k, envs) in secrets.keys_missing(&p.id, &keys)? {
                    out.entry(k).or_default().extend(envs);
                }
            }
            Ok(out.into_iter().collect())
        })
        .await?
    }

    async fn for_repo(&self, repo: &str, environment: &str, consumer: &str) -> anyhow::Result<Vec<(String, String)>> {
        let ws = self.workspace()?;
        let r = repo.to_string();
        let project = tokio::task::spawn_blocking(move || ws.list())
            .await??
            .into_iter()
            .find(|p| p.resources.iter().any(|x| x.kind == roll_workspace::Kind::Repo && x.r#ref == r))
            .ok_or_else(|| anyhow::anyhow!("{repo} isn't on a workspace project's canvas, so it has no secrets"))?;
        self.resolve(project.id, environment, consumer).await
    }

    async fn for_owner(&self, owner: &str, project: &str, environment: &str, consumer: &str) -> anyhow::Result<Vec<(String, String)>> {
        let ws = self.workspace()?;
        let prefix = format!("{owner}/");
        let p = tokio::task::spawn_blocking(move || ws.list())
            .await??
            .into_iter()
            .find(|p| p.id == project || p.name == project)
            .ok_or_else(|| anyhow::anyhow!("no project {project}"))?;
        if !p.resources.iter().any(|x| x.kind == roll_workspace::Kind::Repo && x.r#ref.starts_with(&prefix)) {
            anyhow::bail!("project {} has none of {owner}'s repos on its canvas", p.name);
        }
        self.resolve(p.id, environment, consumer).await
    }

    async fn leaks(&self, repo: &str, candidates: Vec<String>) -> anyhow::Result<Vec<(usize, String, String, String)>> {
        let (secrets, ws) = self.0.get().cloned().ok_or_else(|| anyhow::anyhow!("roll secrets is starting"))?;
        let r = repo.to_string();
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            for p in ws.list()?.into_iter().filter(|p| p.resources.iter().any(|x| x.kind == roll_workspace::Kind::Repo && x.r#ref == r)) {
                out.extend(secrets.leak_matches(&p.id, &candidates)?.into_iter().map(|(i, l)| (i, l.key, l.environment, l.path)));
            }
            Ok(out)
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderMap;
    use serde_json::{Value as Json, json};
    use std::sync::{Arc, Mutex};

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
