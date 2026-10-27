//! Buckets (Supabase's `/bucket` routes). Creating, changing, emptying and deleting buckets
//! needs the service key (Supabase's default: only `service_role` may write `storage.buckets`);
//! other callers see the buckets they may list (and public ones).

use crate::auth;
use crate::error::{ApiError, ApiResult};
use crate::object::bucket_for;
use crate::req::{Req, decode, method_not_allowed};
use crate::util::{self, json_response, message};
use axum::body::Body;
use axum::http::Method;
use axum::response::Response;
use s3_store::{BucketLimits, BucketOptions, BucketUpdate, Class, StoreError};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

pub(crate) async fn route(r: &Req, m: &Method, rest: &str, body: Body) -> ApiResult<Response> {
    if rest.is_empty() {
        return match *m {
            Method::GET => list(r).await,
            Method::POST => create(r, body).await,
            _ => Err(method_not_allowed(m)),
        };
    }
    let (id, sub) = rest.split_once('/').unwrap_or((rest, ""));
    let id = decode(id)?;
    match (m, sub) {
        (&Method::GET, "") => get(r, &id).await,
        (&Method::PUT, "") => update(r, &id, body).await,
        (&Method::DELETE, "") => delete(r, &id).await,
        (&Method::POST, "empty") => empty(r, &id).await,
        _ => Err(method_not_allowed(m)),
    }
}

async fn list(r: &Req) -> ApiResult<Response> {
    let caller = auth::required(r).await?;
    let account = caller.account().unwrap_or_default().to_string();
    let checker = r.checker(&caller);
    let mut buckets: Vec<_> = r.store().buckets_of(&account)?.into_iter().filter(|b| caller.is_service() || b.public || checker.may_list(&b.name, "")).collect();
    if let Some(s) = r.q("search").filter(|s| !s.is_empty()) {
        let s = s.to_lowercase();
        buckets.retain(|b| b.name.contains(&s));
    }
    match r.q("sortColumn").unwrap_or("name") {
        "created_at" | "updated_at" => buckets.sort_by_key(|b| (b.created_at, b.name.clone())),
        _ => buckets.sort_by(|a, b| a.name.cmp(&b.name)),
    }
    if r.q("sortOrder").is_some_and(|o| o.eq_ignore_ascii_case("desc")) {
        buckets.reverse();
    }
    let offset = r.q("offset").and_then(|o| o.parse().ok()).unwrap_or(0usize);
    let limit = r.q("limit").and_then(|o| o.parse().ok()).unwrap_or(usize::MAX);
    let out: Vec<Value> = buckets.iter().skip(offset).take(limit).map(|b| util::bucket_view(r.store(), b)).collect();
    Ok(json_response(&Value::Array(out)))
}

async fn get(r: &Req, id: &str) -> ApiResult<Response> {
    let caller = auth::required(r).await?;
    let b = bucket_for(r, &caller, id)?;
    if !(caller.is_service() || b.public || r.checker(&caller).may_list(&b.name, "")) {
        return Err(ApiError::no_such_bucket());
    }
    Ok(json_response(&util::bucket_view(r.store(), &b)))
}

/// A field that may be absent, `null`, or a value.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct BucketBody {
    id: Option<String>,
    name: Option<String>,
    public: Option<bool>,
    #[serde(rename = "type")]
    kind: Option<String>,
    versioning_status: Option<String>,
    #[serde(deserialize_with = "present")]
    file_size_limit: Option<Value>,
    #[serde(deserialize_with = "present")]
    allowed_mime_types: Option<Value>,
}

/// The limits a body asks for, over `current`.
fn limits_from(body: &BucketBody, mut current: BucketLimits) -> ApiResult<BucketLimits> {
    if let Some(v) = &body.file_size_limit {
        current.file_size_limit = s3_store::api::parse_size(v).map_err(ApiError::invalid)?;
    }
    if let Some(v) = &body.allowed_mime_types {
        current.allowed_mime_types = match v {
            Value::Null => None,
            Value::Array(a) => {
                let list: Vec<String> = a.iter().map(|x| x.as_str().map(str::to_string).ok_or_else(|| ApiError::invalid("allowed_mime_types must be an array of strings"))).collect::<ApiResult<_>>()?;
                let n = s3_store::api::normalize_mime_types(&list).map_err(|bad| ApiError::mime(format!("mime type {bad} is not supported")))?;
                Some(n).filter(|n| !n.is_empty())
            }
            _ => return Err(ApiError::invalid("allowed_mime_types must be an array of strings")),
        };
    }
    Ok(current)
}

fn versioning(v: Option<&str>) -> ApiResult<Option<String>> {
    Ok(match v {
        None => None,
        Some("ENABLED") => Some("Enabled".into()),
        Some("SUSPENDED") => Some("Suspended".into()),
        Some("DISABLED") => None,
        Some(o) => return Err(ApiError::invalid(format!("versioning_status must be ENABLED, SUSPENDED or DISABLED (got {o})"))),
    })
}

fn creating_error(e: StoreError) -> ApiError {
    match e {
        StoreError::InvalidBucketName(_) => ApiError::invalid_bucket_name(),
        e => e.into(),
    }
}

async fn create(r: &Req, body: Body) -> ApiResult<Response> {
    let caller = auth::required(r).await?;
    if !caller.is_service() {
        return Err(ApiError::service_only());
    }
    let account = caller.account().unwrap_or_default().to_string();
    let q: BucketBody = util::read_json(body).await?;
    let name = q.id.clone().or(q.name.clone()).filter(|n| !n.is_empty()).ok_or_else(|| ApiError::invalid("body must have required property 'name'"))?;
    if name.trim() != name {
        return Err(ApiError::invalid_bucket_name());
    }
    if q.kind.as_deref().is_some_and(|k| k != "STANDARD") {
        return Err(ApiError::new(409, "FeatureNotEnabled", "FeatureNotEnabled", "The feature iceberg_catalog is not enabled for this resource"));
    }
    let limits = limits_from(&q, BucketLimits::default())?;
    let versioning = versioning(q.versioning_status.as_deref())?;
    let store = r.store();
    let b = store.create_bucket_in(&account, &name, BucketOptions { public: q.public, ..Default::default() }).await.map_err(creating_error)?;
    store.meter.request(&b.id, Class::A);
    if versioning.is_some() {
        store.update_bucket(&name, BucketUpdate { versioning, ..Default::default() }).await?;
    }
    if !limits.is_empty() {
        store.set_bucket_limits(&name, &limits).await?;
    }
    Ok(json_response(&json!({ "name": name })))
}

fn service_bucket(r: &Req, caller: &s3_server::app::AppCaller, id: &str) -> ApiResult<s3_store::Bucket> {
    let b = bucket_for(r, caller, id)?;
    if !caller.is_service() {
        return Err(ApiError::service_only());
    }
    Ok(b)
}

async fn update(r: &Req, id: &str, body: Body) -> ApiResult<Response> {
    let caller = auth::required(r).await?;
    let b = service_bucket(r, &caller, id)?;
    let q: BucketBody = util::read_json(body).await?;
    if q.public.is_none() && q.file_size_limit.is_none() && q.allowed_mime_types.is_none() && q.versioning_status.is_none() {
        return Err(ApiError::invalid("No content provided"));
    }
    let store = r.store();
    let limits = limits_from(&q, store.bucket_limits(&b.name)?)?;
    let versioning = versioning(q.versioning_status.as_deref())?;
    if versioning.is_none() && q.versioning_status.as_deref() == Some("DISABLED") && b.versioning.is_some() {
        return Err(ApiError::invalid("Versioning can't be disabled once enabled; suspend it (SUSPENDED)"));
    }
    store.meter.request(&b.id, Class::A);
    if q.public.is_some() || versioning.is_some() {
        store.update_bucket(&b.name, BucketUpdate { public: q.public, versioning, ..Default::default() }).await?;
    }
    if q.file_size_limit.is_some() || q.allowed_mime_types.is_some() {
        store.set_bucket_limits(&b.name, &limits).await?;
    }
    Ok(message("Successfully updated"))
}

async fn delete(r: &Req, id: &str) -> ApiResult<Response> {
    let caller = auth::required(r).await?;
    let b = service_bucket(r, &caller, id)?;
    r.store().delete_bucket(&b.name, false).await?;
    Ok(message("Successfully deleted"))
}

/// `POST /bucket/{id}/empty`: every object goes to the trash (restorable for the bucket's
/// `recover_days`). Small buckets are empty when this answers; big ones keep emptying in the
/// background, as Supabase queues it.
async fn empty(r: &Req, id: &str) -> ApiResult<Response> {
    let caller = auth::required(r).await?;
    let b = service_bucket(r, &caller, id)?;
    let store = r.store().clone();
    let name = b.name.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = store.delete_prefix(&name, "").await {
            tracing::warn!(bucket = name, error = %e, "emptying a bucket failed");
        }
    });
    let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
    Ok(message("Successfully emptied"))
}
