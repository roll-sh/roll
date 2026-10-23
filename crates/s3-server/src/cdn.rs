//! Cloudflare in front of roll storage: the origin side of the CDN, purges, cached-egress
//! reports and CDN URLs. The edge itself is the Worker in `apps/cdn-worker`.
//!
//! - **Origin mode.** A request with `X-Roll-CDN-Auth` comes from the Worker; it must carry the
//!   deployment's shared secret (`ROLL_CDN_ORIGIN_SECRET`), or it's refused. Such a request is
//!   an ordinary S3 read otherwise (same access checks, same class B request), except that:
//!   - the bucket must have the CDN on;
//!   - an immutable URL (`/v/<token>/<bucket>/<key>`) only serves the object whose blob the
//!     token names (`cdn::immutable_token`), with `Cache-Control: public, max-age=31536000,
//!     immutable`; a mutable one gets `CDN-Cache-Control: public, max-age=<mutable_ttl_s>`;
//!   - a presigned URL the edge verified comes in `X-Roll-CDN-Presigned` (its query) and
//!     `X-Roll-CDN-Host` (the host it was signed for), and is verified again here as S3 would;
//!   - the bytes sent are origin pulls (`Meter::cdn_origin`): our cost, not internet egress.
//! - **Purges** are outbox rows (`s3_store::cdn`), sent here in batches to Cloudflare's
//!   `POST /zones/{zone}/purge_cache` (files, tags or prefixes), rate-limited, and retried
//!   with the outbox's backoff until they succeed.
//! - **Cached egress** comes from the Worker: `POST /_roll/cdn/usage` with the shared secret,
//!   `{"batch": id, "entries": [{"bucket", "bytes", …}]}`, counted once per batch id.

use crate::ctx::Ctx;
use crate::notify::Failure;
use crate::response::{self, set};
use axum::body::Body;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use s3_proto::http::uri_encode;
use s3_proto::{Code, S3Error};
use s3_store::cdn::{BucketCdn, CdnMode, CdnSettings, PurgeMode, PurgeTask, bucket_tag, immutable_token, object_tag};
use s3_store::{ObjectMeta, PendingEvent, Store};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const AUTH_HEADER: &str = "x-roll-cdn-auth";
const HOST_HEADER: &str = "x-roll-cdn-host";
const PRESIGNED_HEADER: &str = "x-roll-cdn-presigned";
const TOKEN_HEADER: &str = "x-roll-cdn-token";
/// Where the Worker reports usage.
pub const USAGE_PATH: &str = "/_roll/cdn/usage";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// The access key CDN presigned URLs are signed with (the edge holds its secret). Never listed.
pub const CDN_KEY_ID: &str = "RKROLLCDNPRESIGN0000";
const CDN_KEY_NAME: &str = "roll CDN (presigned CDN URLs, verified at the edge)";
/// Most purge API calls one dispatch pass makes (the rest wait for the next).
const MAX_CALLS_PER_PASS: usize = 50;
const MAX_USAGE_BODY: usize = 1 << 20;

#[derive(Clone, Debug)]
pub struct CdnConfig {
    /// The secret the Worker sends in `X-Roll-CDN-Auth` (`ROLL_CDN_ORIGIN_SECRET`); `None`:
    /// requests claiming to come from the CDN are refused.
    pub origin_secret: Option<String>,
    /// Cloudflare's API (tests point it at a mock).
    pub cloudflare_api: String,
    pub api_timeout: Duration,
}

impl Default for CdnConfig {
    fn default() -> Self {
        Self { origin_secret: None, cloudflare_api: "https://api.cloudflare.com/client/v4".into(), api_timeout: Duration::from_secs(30) }
    }
}

/// Counters since start.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct CdnStats {
    /// Requests from the CDN (cache misses at the edge).
    pub origin_requests: u64,
    pub purge_calls: u64,
    pub purge_failures: u64,
    /// URLs, tags and prefixes purged.
    pub purged: u64,
    pub usage_batches: u64,
    /// Batches the Worker sent again (counted once).
    pub usage_duplicates: u64,
}

#[derive(Default)]
struct Counters {
    origin_requests: AtomicU64,
    purge_calls: AtomicU64,
    purge_failures: AtomicU64,
    purged: AtomicU64,
    usage_batches: AtomicU64,
    usage_duplicates: AtomicU64,
}

/// The CDN's server side: purges and counters.
pub struct Cdn {
    pub cfg: CdnConfig,
    http: reqwest::Client,
    /// When the next purge call may go (the rate limit).
    next_call: tokio::sync::Mutex<Instant>,
    counters: Counters,
}

/// What made a request a CDN one.
#[derive(Clone, Debug, Default)]
pub(crate) struct CdnRequest {
    /// An immutable URL's token.
    pub token: Option<String>,
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Cdn {
    pub fn new(cfg: CdnConfig) -> Arc<Self> {
        let http = reqwest::Client::builder().timeout(cfg.api_timeout).redirect(reqwest::redirect::Policy::none()).user_agent("roll-storage-cdn/1").build().expect("an HTTP client");
        Arc::new(Self { cfg, http, next_call: tokio::sync::Mutex::new(Instant::now()), counters: Counters::default() })
    }

    pub fn stats(&self) -> CdnStats {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let c = &self.counters;
        CdnStats {
            origin_requests: l(&c.origin_requests),
            purge_calls: l(&c.purge_calls),
            purge_failures: l(&c.purge_failures),
            purged: l(&c.purged),
            usage_batches: l(&c.usage_batches),
            usage_duplicates: l(&c.usage_duplicates),
        }
    }

    /// Whether `headers` carry the shared secret.
    fn authorized(&self, headers: &HeaderMap) -> bool {
        let given = headers.get(AUTH_HEADER).and_then(|v| v.to_str().ok()).unwrap_or_default();
        self.cfg.origin_secret.as_deref().is_some_and(|s| !s.is_empty() && ct_eq(s, given))
    }

    /// A request from the Worker (it has `X-Roll-CDN-Auth`): checked and made into the S3
    /// request it stands for. Returns the path to address it by, and what's CDN about it.
    /// `None`: not a CDN request.
    pub(crate) fn prepare(&self, store: &Store, method: &Method, headers: &mut HeaderMap, raw_query: &mut String, raw_path: &str) -> Result<Option<(String, CdnRequest)>, S3Error> {
        if !headers.contains_key(AUTH_HEADER) {
            return Ok(None);
        }
        if !self.authorized(headers) {
            return Err(S3Error::msg(Code::AccessDenied, "This request claims to come from the CDN, but its credentials are wrong."));
        }
        if !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
            return Err(S3Error::new(Code::MethodNotAllowed));
        }
        self.counters.origin_requests.fetch_add(1, Ordering::Relaxed);
        headers.remove(AUTH_HEADER);
        // Tokens of another API (`?token=`) are that API's to check; here they mean nothing.
        headers.remove(TOKEN_HEADER);
        let cdn_host = headers.remove(HOST_HEADER);
        if let Some(q) = headers.remove(PRESIGNED_HEADER) {
            // The presigned URL the edge verified, checked again as S3 checks one: its query,
            // for the host it was signed for.
            *raw_query = q.to_str().map_err(|_| S3Error::new(Code::InvalidRequest))?.to_string();
            match cdn_host {
                Some(h) => {
                    headers.insert("host", h);
                }
                None => return Err(S3Error::msg(Code::InvalidRequest, "a presigned CDN request needs X-Roll-CDN-Host")),
            }
        }
        let mut req = CdnRequest::default();
        let mut path = raw_path.to_string();
        let settings = store.cdn_routing().map_err(crate::error::store_error)?;
        for r in settings.routes.iter().filter(|r| r.kind == "immutable") {
            if let Some(rest) = raw_path.strip_prefix(r.prefix.as_str()) {
                let (token, rest) = rest.split_once('/').ok_or_else(|| S3Error::new(Code::NoSuchKey))?;
                req.token = Some(token.to_string());
                path = format!("/{rest}");
                break;
            }
        }
        Ok(Some((path, req)))
    }

    /// `POST /_roll/cdn/usage`: a batch of what the edge served.
    pub(crate) async fn usage(&self, store: &Store, headers: &HeaderMap, body: Body) -> Response {
        let reply = |code: StatusCode, v: Value| {
            let mut r = Response::new(Body::from(v.to_string()));
            *r.status_mut() = code;
            set(&mut r, "content-type", "application/json");
            r
        };
        if !self.authorized(headers) {
            return reply(StatusCode::FORBIDDEN, json!({ "error": "wrong or missing X-Roll-CDN-Auth" }));
        }
        let Ok(bytes) = axum::body::to_bytes(body, MAX_USAGE_BODY).await else {
            return reply(StatusCode::PAYLOAD_TOO_LARGE, json!({ "error": "a usage batch is at most 1 MiB" }));
        };
        #[derive(serde::Deserialize)]
        struct Entry {
            bucket: String,
            bytes: u64,
        }
        #[derive(serde::Deserialize)]
        struct Batch {
            batch: String,
            entries: Vec<Entry>,
        }
        let b: Batch = match serde_json::from_slice(&bytes) {
            Ok(b) => b,
            Err(e) => return reply(StatusCode::BAD_REQUEST, json!({ "error": e.to_string() })),
        };
        let entries: Vec<(String, u64)> = b.entries.into_iter().map(|e| (e.bucket, e.bytes)).collect();
        match store.record_cdn_usage(&b.batch, &entries).await {
            Ok(fresh) => {
                let c = if fresh { &self.counters.usage_batches } else { &self.counters.usage_duplicates };
                c.fetch_add(1, Ordering::Relaxed);
                reply(StatusCode::OK, json!({ "accepted": true, "duplicate": !fresh }))
            }
            Err(s3_store::StoreError::InvalidArgument(m)) => reply(StatusCode::BAD_REQUEST, json!({ "error": m })),
            Err(e) => {
                tracing::warn!(error = %e, "recording CDN usage failed");
                reply(StatusCode::SERVICE_UNAVAILABLE, json!({ "error": "try again" }))
            }
        }
    }

    /// Send the purges of these outbox rows. Each row's outcome: `Ok(true)` purged (delete
    /// it), or why it must be retried.
    pub(crate) async fn purge(&self, store: &Store, rows: Vec<PendingEvent>) -> Vec<(PendingEvent, Result<bool, Failure>)> {
        let settings = match store.cdn_settings() {
            Ok(s) => s,
            Err(e) => return rows.into_iter().map(|r| (r, Err(Failure::Retry(e.to_string())))).collect(),
        };
        // Without a CDN hostname nothing can be cached anywhere: nothing to purge.
        let Some(host) = settings.host() else { return rows.into_iter().map(|r| (r, Ok(true))).collect() };
        if !settings.can_purge() {
            return rows.into_iter().map(|r| (r, Err(Failure::Retry("CDN purges need the Cloudflare zone id and API token (PATCH /v1/storage/cdn)".into())))).collect();
        }
        // What each row needs purged, by kind ("files", "tags", "prefixes").
        let mut wants: Vec<Vec<(&'static str, String)>> = Vec::with_capacity(rows.len());
        for r in &rows {
            let task: PurgeTask = serde_json::from_str(&r.body).unwrap_or_default();
            wants.push(purge_items(&settings, &host, &task, &r.key));
        }
        let mut by_kind: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();
        for (k, v) in wants.iter().flatten() {
            by_kind.entry(k).or_default().insert(v.clone());
        }
        // Send them in batches; remember what failed (or wasn't sent this pass).
        let mut failed: BTreeMap<(&'static str, String), String> = BTreeMap::new();
        let mut calls = 0;
        for (kind, items) in &by_kind {
            let items: Vec<&String> = items.iter().collect();
            for batch in items.chunks(settings.purge_batch.max(1) as usize) {
                let outcome = if calls >= MAX_CALLS_PER_PASS { Err("deferred to the next pass (purge calls per pass)".to_string()) } else { self.call(&settings, kind, batch).await };
                calls += 1;
                if let Err(why) = outcome {
                    for i in batch {
                        failed.insert((kind, (*i).clone()), why.clone());
                    }
                }
            }
        }
        rows.into_iter()
            .zip(wants)
            .map(|(r, items)| {
                let why = items.iter().find_map(|(k, v)| failed.get(&(*k, v.clone())));
                let outcome = match why {
                    Some(w) => Err(Failure::Retry(w.clone())),
                    None => Ok(true),
                };
                (r, outcome)
            })
            .collect()
    }

    /// One purge call (waiting for the rate limit first).
    async fn call(&self, s: &CdnSettings, kind: &str, items: &[&String]) -> Result<(), String> {
        {
            let mut next = self.next_call.lock().await;
            let now = Instant::now();
            if *next > now {
                tokio::time::sleep(*next - now).await;
            }
            *next = Instant::now().max(*next) + Duration::from_millis(60_000 / s.purge_per_minute.max(1) as u64);
        }
        self.counters.purge_calls.fetch_add(1, Ordering::Relaxed);
        let zone = s.zone_id.as_deref().unwrap_or_default();
        let token = s.api_token.as_ref().map(|t| t.expose().to_string()).unwrap_or_default();
        let url = format!("{}/zones/{zone}/purge_cache", self.cfg.cloudflare_api.trim_end_matches('/'));
        let result = async {
            let resp = self.http.post(&url).bearer_auth(token).json(&json!({ kind: items })).send().await.map_err(|e| format!("Cloudflare's purge API: {}", without_url(&e)))?;
            let status = resp.status();
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            if status.is_success() && body["success"].as_bool() == Some(true) {
                return Ok(());
            }
            let errors = body["errors"].as_array().map(|e| e.iter().map(|x| format!("{} {}", x["code"], x["message"].as_str().unwrap_or_default())).collect::<Vec<_>>().join("; ")).unwrap_or_default();
            Err(format!("Cloudflare's purge API answered {status}: {errors}"))
        }
        .await;
        match &result {
            Ok(()) => {
                self.counters.purged.fetch_add(items.len() as u64, Ordering::Relaxed);
            }
            Err(e) => {
                self.counters.purge_failures.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(error = e, kind, n = items.len(), "a CDN purge failed; it will be retried");
            }
        }
        result
    }
}

/// A reqwest error without its URL (it never holds the token, but keep logs plain).
fn without_url(e: &reqwest::Error) -> String {
    let s = e.to_string();
    match e.url() {
        Some(u) => s.replace(u.as_str(), "<url>"),
        None => s,
    }
}

/// The path of an object under a `<prefix><bucket>/<key>` route (S3's encoding, as the Worker's
/// cache keys use).
fn bucket_key_path(prefix: &str, bucket: &str, key: &str) -> String {
    format!("{prefix}{}/{}", uri_encode(bucket, true), uri_encode(key, false))
}

/// What a purge row needs purged: `("files", url)`, `("tags", tag)` or `("prefixes", prefix)`.
pub(crate) fn purge_items(s: &CdnSettings, host: &str, task: &PurgeTask, key: &str) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if task.all {
        // Every URL form of the bucket: its tag, and the prefixes of the routes it's under.
        out.push(("tags", bucket_tag(&task.bucket)));
        for r in &s.routes {
            match r.kind.as_str() {
                "s3" | "bucket-key" => out.push(("prefixes", format!("{host}{}{}/", r.prefix, uri_encode(&task.bucket, true)))),
                "website" if r.bucket.as_deref() == Some(task.bucket.as_str()) => out.push(("prefixes", format!("{host}{}", r.prefix))),
                _ => {}
            }
        }
        return out;
    }
    if s.purge == PurgeMode::Tags {
        out.push(("tags", object_tag(&task.bucket, key)));
        return out;
    }
    for r in &s.routes {
        let path = match r.kind.as_str() {
            "s3" | "bucket-key" => bucket_key_path(&r.prefix, &task.bucket, key),
            "website" if r.bucket.as_deref() == Some(task.bucket.as_str()) => format!("{}{}", r.prefix, uri_encode(key, false)),
            _ => continue,
        };
        let url = format!("https://{host}{path}");
        if r.credentialed() {
            out.push(("files", format!("{url}?roll-cdn-ns=private")));
        }
        out.push(("files", url));
    }
    out
}

// ---- Serving the CDN ----

/// Check a CDN request for `meta` of `bucket` and set its caching headers on `r`.
pub(crate) fn check(ctx: &Ctx, bucket: &s3_store::Bucket, meta: &ObjectMeta) -> Result<CdnMode, S3Error> {
    let Some(req) = &ctx.cdn else { return Ok(CdnMode::Mutable) };
    let cdn: BucketCdn = ctx.store().bucket_cdn(&bucket.name).map_err(crate::error::store_error)?.unwrap_or_default();
    if !cdn.enabled {
        return Err(S3Error::msg(Code::AccessDenied, "The CDN isn't enabled for this bucket."));
    }
    match &req.token {
        Some(t) if !ct_eq(t, &immutable_token(&meta.blob_id)) => Err(S3Error::new(Code::NoSuchKey).with("Key", meta.key.clone())),
        Some(_) => Ok(CdnMode::Immutable),
        None => Ok(CdnMode::Mutable),
    }
}

/// The caching headers of a CDN response.
pub(crate) fn set_headers(ctx: &Ctx, r: &mut Response, mode: CdnMode) {
    if ctx.cdn.is_none() {
        return;
    }
    if mode == CdnMode::Immutable {
        set(r, "cache-control", IMMUTABLE);
        set(r, "cdn-cache-control", IMMUTABLE);
        return;
    }
    let own = r.headers().get("cache-control").and_then(|v| v.to_str().ok()).unwrap_or_default().to_ascii_lowercase();
    if own.contains("no-store") || own.contains("private") {
        set(r, "cdn-cache-control", "no-store");
        return;
    }
    let ttl = ctx.store().cdn_routing().map(|s| s.mutable_ttl_s).unwrap_or(60);
    set(r, "cdn-cache-control", &format!("public, max-age={ttl}"));
}

// ---- URLs ----

/// The CDN presigning key (created on first use): `(id, secret)`.
pub async fn presign_key(store: &Store) -> s3_store::Result<(String, String)> {
    if let Some(k) = store.access_key(CDN_KEY_ID)? {
        return Ok((k.id, k.secret));
    }
    let mut b = [0u8; 30];
    getrandom::fill(&mut b).expect("os randomness");
    let secret = hex::encode(b);
    let k = store.ensure_key(CDN_KEY_ID, &secret, CDN_KEY_NAME).await?;
    Ok((k.id, k.secret))
}

/// A CDN URL for an object.
#[derive(Clone, Debug, serde::Serialize)]
pub struct CdnUrl {
    pub url: String,
    pub immutable: bool,
    /// Presigned: when it stops working (Unix ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

/// The CDN URL of `meta` in `bucket`: immutable (its blob's) or by key; presigned with the
/// CDN key for `presign_s` seconds when given (`None`: the deployment has no CDN).
pub async fn object_url(store: &Store, bucket: &str, meta: &ObjectMeta, immutable: bool, presign_s: Option<i64>, region: &str) -> s3_store::Result<Option<CdnUrl>> {
    let s = store.cdn_routing()?;
    let Some(base) = s.base() else { return Ok(None) };
    let path = match immutable {
        true => {
            let Some(r) = s.routes.iter().find(|r| r.kind == "immutable") else { return Ok(None) };
            format!("{}{}/{}/{}", r.prefix, immutable_token(&meta.blob_id), bucket, meta.key)
        }
        false => {
            let Some(r) = s.routes.iter().find(|r| r.kind == "s3") else { return Ok(None) };
            format!("{}{}/{}", r.prefix, bucket, meta.key)
        }
    };
    let Some(expires_s) = presign_s else {
        return Ok(Some(CdnUrl { url: format!("{base}{}", uri_encode(&path, false)), immutable, expires_at: None }));
    };
    let (id, secret) = presign_key(store).await?;
    let now = store.now();
    let url = s3_proto::sigv4::presign_url(&s3_proto::sigv4::Presign { method: "GET", endpoint: base, path: &path, query: &[], access_key: &id, secret: &secret, region, now: now / 1000, expires_s });
    Ok(Some(CdnUrl { url, immutable, expires_at: Some(now + expires_s * 1000) }))
}

/// What the Worker needs configured (`wrangler.toml` vars and secrets), for `origin` (how the
/// Worker reaches roll). Secrets are only named, except the presigning key's when `secrets`.
pub async fn worker_env(store: &Store, origin: &str, secrets: bool) -> s3_store::Result<Value> {
    let s = store.cdn_routing()?;
    let routes = serde_json::to_string(&s.routes)?;
    let mut vars = json!({
        "ROLL_ORIGIN": origin,
        "ROLL_ROUTES": routes,
        "ROLL_CDN_DEFAULT_TTL": s.mutable_ttl_s.to_string(),
        "ROLL_CDN_PRESIGN_KEY_ID": CDN_KEY_ID,
    });
    let mut secret_names = vec!["ROLL_CDN_ORIGIN_SECRET (= rolld's ROLL_CDN_ORIGIN_SECRET)", "ROLL_CDN_PRESIGN_SECRET", "ROLL_CDN_JWT_SECRET (optional: signed-URL tokens of the storage REST API)"];
    if secrets {
        let (_, secret) = presign_key(store).await?;
        vars["ROLL_CDN_PRESIGN_SECRET"] = json!(secret);
        secret_names.retain(|n| !n.starts_with("ROLL_CDN_PRESIGN_SECRET"));
    }
    Ok(json!({ "vars": vars, "secrets": secret_names }))
}

/// The response to a request the CDN made that this module answers itself (usage reports);
/// `None`: not one.
pub(crate) async fn intercept(server: &crate::Server, method: &Method, path: &str, headers: &HeaderMap, body: Body) -> Result<Response, Body> {
    if path == USAGE_PATH {
        if *method != Method::POST {
            let mut r = response::empty(StatusCode::METHOD_NOT_ALLOWED);
            set(&mut r, "allow", "POST");
            return Ok(r);
        }
        return Ok(server.cdn.usage(&server.store, headers, body).await);
    }
    Err(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use s3_store::cdn::CdnRoute;

    #[test]
    fn purge_urls() {
        let mut s = CdnSettings { base_url: Some("https://cdn.example.com/".into()), ..Default::default() };
        s.routes.push(CdnRoute { prefix: "/storage/v1/object/public/".into(), kind: "bucket-key".into(), auth: Some("public".into()), ..Default::default() });
        s.routes.push(CdnRoute { prefix: "/".into(), kind: "website".into(), bucket: Some("site".into()), ..Default::default() });
        let host = s.host().unwrap();
        let task = PurgeTask { bucket: "b".into(), all: false };
        let items = purge_items(&s, &host, &task, "a b/c+d.txt");
        let files: Vec<&str> = items.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(
            files,
            [
                "https://cdn.example.com/b/a%20b/c%2Bd.txt?roll-cdn-ns=private",
                "https://cdn.example.com/b/a%20b/c%2Bd.txt",
                "https://cdn.example.com/storage/v1/object/public/b/a%20b/c%2Bd.txt",
            ]
        );
        let site = purge_items(&s, &host, &PurgeTask { bucket: "site".into(), all: false }, "index.html");
        assert!(site.iter().any(|(_, v)| v == "https://cdn.example.com/index.html"));
        s.purge = PurgeMode::Tags;
        assert_eq!(purge_items(&s, &host, &task, "k"), [("tags", object_tag("b", "k"))]);
        let all = purge_items(&s, &host, &PurgeTask { bucket: "b".into(), all: true }, "");
        assert_eq!(all, [("tags", bucket_tag("b")), ("prefixes", "cdn.example.com/b/".into()), ("prefixes", "cdn.example.com/storage/v1/object/public/b/".into())]);
    }
}
