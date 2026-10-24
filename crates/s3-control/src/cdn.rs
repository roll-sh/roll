//! The CDN in the control plane: the deployment's Cloudflare settings (`/v1/storage/cdn`), what
//! the edge Worker needs configured, the margin view, and object URLs (`/buckets/{name}/url`):
//! CDN URLs when the bucket has the CDN on, S3 URLs otherwise.

use crate::api::JsonBody;
use crate::control::{Control, ControlError, Result};
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use s3_store::cdn::{CdnMode, CdnSettings, CdnSettingsPatch};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

type AppState = Arc<Control>;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/storage/cdn", get(show).patch(update))
        .route("/v1/storage/cdn/worker", get(worker))
        .route("/v1/storage/buckets/{name}/url", post(url))
}

const GB: f64 = 1e9;

impl Control {
    /// The settings as shown: never the API token.
    pub fn cdn_view(&self, s: &CdnSettings) -> Value {
        json!({
            "base_url": s.base(),
            "zone_id": s.zone_id,
            "api_token_set": s.api_token.is_some(),
            "purge": s.purge,
            "routes": s.routes,
            "mutable_ttl_s": s.mutable_ttl_s,
            "purge_batch": s.purge_batch,
            "purge_per_minute": s.purge_per_minute,
        })
    }

    /// Cached egress against its cost over `days`: bytes the CDN served, bytes it pulled from
    /// roll (cache misses), the byte hit ratio, and what was billed for it.
    pub fn cdn_margin(&self, days: i64) -> Result<Value> {
        let days = days.clamp(1, 366);
        let since = s3_store::day_of(self.store.now() - (days - 1) * 86_400_000);
        let rows = self.store.usage(None, &since)?;
        let cached: u64 = rows.iter().map(|r| r.egress_cached).sum();
        let origin: u64 = rows.iter().map(|r| r.egress_cdn_origin).sum();
        let hit = if cached > 0 { (1.0 - origin as f64 / cached as f64).max(0.0) } else { 0.0 };
        Ok(json!({
            "days": days,
            "cached_bytes": cached,
            "origin_pull_bytes": origin,
            "byte_hit_ratio": (hit * 1e4).round() / 1e4,
            "billed_usd": (cached as f64 / GB * self.cfg.pricing.usd_per_gb_egress_cached * 1e6).round() / 1e6,
        }))
    }

    /// `cdn` and `public_url` of a bucket view, and `api_public_url`: where the developer API
    /// serves the bucket's public objects (`<url>/<key>`, image renditions under
    /// `render/image/public/`), the CDN's when the bucket has it on.
    pub(crate) fn cdn_bucket_view(&self, b: &s3_store::Bucket, v: &mut Value) {
        let cdn = self.store.bucket_cdn(&b.name).ok().flatten().unwrap_or_default();
        v["cdn"] = json!(cdn);
        let routing = self.store.cdn_routing().ok();
        let base = routing.as_ref().and_then(|s| s.base().map(str::to_string));
        v["public_url"] = match &base {
            Some(base) if cdn.enabled => json!(format!("{base}/{}", b.name)),
            _ => v["url"].clone(),
        };
        let api_on_cdn = cdn.enabled && routing.as_ref().is_some_and(|s| s.routes.iter().any(|r| r.api && r.kind == "bucket-key"));
        let bucket = s3_proto::http::uri_encode(&b.name, true);
        v["api_public_url"] = match (&base, self.api_endpoint.get()) {
            (Some(base), _) if api_on_cdn => json!(format!("{base}{}object/public/{bucket}", s3_store::cdn::API_PREFIX)),
            (_, Some(api)) => json!(format!("{}/object/public/{bucket}", api.trim_end_matches('/'))),
            _ => Value::Null,
        };
        v["api_cdn"] = json!(api_on_cdn);
    }

    /// The developer API's base URL through the CDN (`<CDN>/storage/v1`), if it's routed there.
    pub(crate) fn api_cdn_url(&self) -> Option<String> {
        let s = self.store.cdn_routing().ok()?;
        let base = s.base()?.to_string();
        s.routes.iter().any(|r| r.api && r.kind != "none").then(|| format!("{base}{}", s3_store::cdn::API_PREFIX.trim_end_matches('/')))
    }
}

async fn show(State(c): State<AppState>) -> Result<Json<Value>> {
    let s = c.store.cdn_settings()?;
    let mut v = c.cdn_view(&s);
    v["worker"] = s3_server::cdn::worker_env(&c.store, &c.cfg.endpoint, c.api_endpoint.get().map(String::as_str), false).await?;
    v["margin"] = c.cdn_margin(30)?;
    if let Some(server) = c.server.get() {
        v["stats"] = json!(server.cdn.stats());
        v["cache"] = json!(server.cache_stats());
        v["origin_secret_set"] = json!(server.cfg.cdn.origin_secret.is_some());
    }
    Ok(Json(v))
}

async fn update(State(c): State<AppState>, JsonBody(p): JsonBody<CdnSettingsPatch>) -> Result<Json<Value>> {
    c.store.update_cdn_settings(p).await?;
    Ok(Json(c.cdn_view(&c.store.cdn_settings()?)))
}

#[derive(Deserialize)]
struct WorkerQ {
    #[serde(default)]
    secrets: bool,
}

/// What to configure the Worker with; `?secrets=true` includes the presigning key's secret.
async fn worker(State(c): State<AppState>, Query(q): Query<WorkerQ>) -> Result<Json<Value>> {
    Ok(Json(s3_server::cdn::worker_env(&c.store, &c.cfg.endpoint, c.api_endpoint.get().map(String::as_str), q.secrets).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UrlReq {
    key: String,
    #[serde(default)]
    version_id: Option<String>,
    /// An immutable CDN URL (default: the bucket's CDN mode).
    #[serde(default)]
    immutable: Option<bool>,
    /// Presign for this long (default: only when the bucket isn't public).
    #[serde(default)]
    expires_s: Option<i64>,
}

/// `POST /buckets/{name}/url`: the URL to give out for an object.
async fn url(State(c): State<AppState>, acct: crate::api::Acct, Path(name): Path<String>, JsonBody(r): JsonBody<UrlReq>) -> Result<Json<Value>> {
    let b = c.bucket_in(&acct.0, &name)?;
    let meta = c.store.get_object_version(&name, &r.key, r.version_id.as_deref().filter(|v| !v.is_empty()))?;
    if let Some(e) = r.expires_s
        && !(1..=604_800).contains(&e)
    {
        return Err(ControlError::Invalid("expires_s must be between 1 and 604800 (7 days)".into()));
    }
    let public = b.public || s3_server::bucket_policy(&c.store, &name).is_some_and(|p| s3_server::validate_bucket_policy(&p, &name).unwrap_or(false));
    let presign = r.expires_s.or(if public { None } else { Some(3600) });
    let owned_by_default = b.account_id == c.store.default_account_id();
    if presign.is_some() && !owned_by_default {
        return Err(ControlError::Invalid("signed URLs are only available for the default account's buckets; presign with the account's own keys".into()));
    }
    let cdn = c.store.bucket_cdn(&name)?.unwrap_or_default();
    if cdn.enabled {
        let immutable = r.immutable.unwrap_or(cdn.mode == CdnMode::Immutable);
        if let Some(u) = s3_server::cdn::object_url(&c.store, &name, &meta, immutable, presign, &c.cfg.region).await? {
            return Ok(Json(json!({ "url": u.url, "cdn": true, "immutable": u.immutable, "expires_at": u.expires_at })));
        }
    }
    let (url, expires_at) = match presign {
        Some(e) => {
            let (u, at) = c.presign(&name, &r.key, "GET", e)?;
            (u, Some(at))
        }
        None => (format!("{}/{name}/{}", c.cfg.endpoint.trim_end_matches('/'), s3_proto::http::uri_encode(&r.key, false)), None),
    };
    Ok(Json(json!({ "url": url, "cdn": false, "immutable": false, "expires_at": expires_at })))
}
