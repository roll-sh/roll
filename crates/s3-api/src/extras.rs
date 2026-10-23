//! roll's extras under `/roll/...` (what `@roll/storage` adds to storage-js): versions, the
//! trash, branches, bucket policies and rule presets.
//!
//! - `POST /roll/versions/{bucket}` `{prefix, limit, cursor}`: versions (`s3:ListBucketVersions`)
//! - `POST /roll/trash/{bucket}` `{prefix, limit, cursor}`: the trash (`s3:ListBucket`)
//! - `POST /roll/trash/{bucket}/restore` `{path, deletedAt}`: restore (`s3:PutObject`)
//! - `GET /roll/branches/{bucket}`, `POST /roll/branches/{bucket}` `{name}` (service key)
//! - `GET|PUT|DELETE /roll/policy/{bucket}` (service key)
//! - `POST /roll/rules/{bucket}` `{preset}` (service key): set a preset's policy

use crate::auth;
use crate::error::{ApiError, ApiResult};
use crate::object::{bucket_for, info_view, write_denied};
use crate::req::{Req, decode, method_not_allowed};
use crate::util::{self, json_response, message};
use axum::body::Body;
use axum::http::Method;
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use s3_server::app::{AppCaller, PolicyError};
use s3_store::{Bucket, Class, VersionsQuery};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) async fn route(r: &Req, m: &Method, rest: &str, body: Body) -> ApiResult<Response> {
    let (what, rest) = rest.split_once('/').unwrap_or((rest, ""));
    let (bucket, sub) = rest.split_once('/').unwrap_or((rest, ""));
    let bucket = decode(bucket)?;
    if bucket.is_empty() {
        return Err(ApiError::missing("bucketName"));
    }
    let caller = auth::required(r).await?;
    let b = bucket_for(r, &caller, &bucket)?;
    match (what, m, sub) {
        ("versions", &Method::POST, "") => versions(r, &caller, &b, body).await,
        ("trash", &Method::POST, "") => trash(r, &caller, &b, body).await,
        ("trash", &Method::POST, "restore") => restore(r, &caller, &b, body).await,
        ("branches", &Method::GET, "") => branches(r, &caller, &b),
        ("branches", &Method::POST, "") => branch(r, &caller, &b, body).await,
        ("policy", &Method::GET, "") => {
            service(&caller)?;
            let p = s3_server::bucket_policy(r.store(), &b.name).map(|p| serde_json::from_str::<Value>(&p).unwrap_or(Value::String(p)));
            Ok(json_response(&json!({ "policy": p })))
        }
        ("policy", &Method::PUT, "") => {
            service(&caller)?;
            let doc = axum::body::to_bytes(body, s3_server::app::MAX_POLICY_BYTES).await.map_err(|_| ApiError::too_large("policy"))?;
            let doc = std::str::from_utf8(&doc).map_err(|_| ApiError::invalid("the policy must be JSON"))?;
            put_policy(r, &b, Some(doc)).await?;
            Ok(message("Successfully updated"))
        }
        ("policy", &Method::DELETE, "") => {
            service(&caller)?;
            put_policy(r, &b, None).await?;
            Ok(message("Successfully deleted"))
        }
        ("rules", &Method::POST, "") => {
            service(&caller)?;
            #[derive(Deserialize)]
            struct Rules {
                preset: String,
            }
            let q: Rules = util::read_json(body).await?;
            let policy = s3_server::app::preset_policy(&q.preset, &b.name).ok_or_else(|| ApiError::invalid(format!("unknown preset {} (public-read, authenticated-read, user-folders, owner-only)", q.preset)))?;
            put_policy(r, &b, Some(&policy.to_string())).await?;
            Ok(json_response(&json!({ "policy": policy })))
        }
        _ => Err(method_not_allowed(m)),
    }
}

fn service(caller: &AppCaller) -> ApiResult<()> {
    if caller.is_service() { Ok(()) } else { Err(ApiError::service_only()) }
}

async fn put_policy(r: &Req, b: &Bucket, doc: Option<&str>) -> ApiResult<()> {
    r.store().meter.request(&b.id, Class::A);
    s3_server::app::put_bucket_policy(r.store(), &b.name, doc).await.map_err(|e| match e {
        PolicyError::Invalid(m) => ApiError::new(400, "MalformedPolicy", "MalformedPolicy", m),
        PolicyError::Store(e) => e.into(),
    })
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Page {
    prefix: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}

fn cursor<T: serde::de::DeserializeOwned>(c: Option<&str>) -> ApiResult<Option<T>> {
    match c.filter(|c| !c.is_empty()) {
        None => Ok(None),
        Some(c) => URL_SAFE_NO_PAD.decode(c).ok().and_then(|b| serde_json::from_slice(&b).ok()).map(Some).ok_or_else(|| ApiError::invalid("invalid cursor")),
    }
}

fn encode_cursor(v: &Value) -> String {
    URL_SAFE_NO_PAD.encode(v.to_string())
}

async fn versions(r: &Req, caller: &AppCaller, b: &Bucket, body: Body) -> ApiResult<Response> {
    r.checker(caller).allows("s3:ListBucketVersions", &b.name, None).map_err(ApiError::from)?;
    let q: Page = util::read_json(body).await?;
    let after: Option<(String, String)> = cursor(q.cursor.as_deref())?;
    let (key_marker, version_id_marker) = after.map_or((None, None), |(k, v)| (Some(k), Some(v)));
    let page = r.store().list_versions(&b.name, &VersionsQuery { prefix: q.prefix.unwrap_or_default(), delimiter: None, key_marker, version_id_marker, max_keys: q.limit.unwrap_or(100).clamp(1, 1000) })?;
    r.store().meter.request(&b.id, Class::A);
    let versions: Vec<Value> = page
        .versions
        .iter()
        .map(|v| json!({ "name": v.key, "version_id": v.version_id, "is_latest": v.is_latest, "is_delete_marker": v.delete_marker, "size": v.size, "etag": s3_proto::http::quote_etag(&v.etag), "last_modified": util::iso(v.last_modified) }))
        .collect();
    let next = match (page.truncated, page.next_key_marker, page.next_version_id_marker) {
        (true, Some(k), v) => Some(encode_cursor(&json!([k, v.unwrap_or_default()]))),
        _ => None,
    };
    Ok(json_response(&json!({ "versions": versions, "next_cursor": next })))
}

async fn trash(r: &Req, caller: &AppCaller, b: &Bucket, body: Body) -> ApiResult<Response> {
    r.checker(caller).allows("s3:ListBucket", &b.name, None).map_err(ApiError::from)?;
    let q: Page = util::read_json(body).await?;
    let after: Option<(String, i64)> = cursor(q.cursor.as_deref())?;
    let (items, more) = r.store().list_trash(&b.name, q.prefix.as_deref().unwrap_or_default(), after.as_ref().map(|(k, at)| (k.as_str(), *at)), q.limit.unwrap_or(100).clamp(1, 1000))?;
    r.store().meter.request(&b.id, Class::A);
    let next = if more { items.last().map(|t| encode_cursor(&json!([t.key, t.deleted_at]))) } else { None };
    let items: Vec<Value> = items.iter().map(|t| json!({ "name": t.key, "size": t.size, "etag": s3_proto::http::quote_etag(&t.etag), "deleted_at": t.deleted_at, "expires_at": t.expires_at, "version_id": t.version_id })).collect();
    Ok(json_response(&json!({ "items": items, "next_cursor": next })))
}

async fn restore(r: &Req, caller: &AppCaller, b: &Bucket, body: Body) -> ApiResult<Response> {
    #[derive(Deserialize)]
    struct Restore {
        path: String,
        #[serde(rename = "deletedAt")]
        deleted_at: i64,
    }
    let q: Restore = util::read_json(body).await?;
    r.checker(caller).allows("s3:PutObject", &b.name, Some(&q.path)).map_err(write_denied)?;
    r.store().meter.request(&b.id, Class::A);
    let m = r.store().restore(&b.name, &q.path, q.deleted_at).await?;
    Ok(json_response(&info_view(b, &m)))
}

fn branches(r: &Req, caller: &AppCaller, b: &Bucket) -> ApiResult<Response> {
    if !caller.is_service() && !r.checker(caller).may_list(&b.name, "") {
        return Err(ApiError::no_such_bucket());
    }
    let out: Vec<Value> = b.branches.iter().filter_map(|n| r.store().bucket(n).ok()).map(|c| util::bucket_view(r.store(), &c)).collect();
    Ok(json_response(&Value::Array(out)))
}

async fn branch(r: &Req, caller: &AppCaller, b: &Bucket, body: Body) -> ApiResult<Response> {
    service(caller)?;
    #[derive(Deserialize)]
    struct Branch {
        name: String,
    }
    let q: Branch = util::read_json(body).await?;
    let c = r.store().branch_bucket(&b.name, &q.name).await.map_err(|e| match e {
        s3_store::StoreError::InvalidBucketName(_) => ApiError::invalid_bucket_name(),
        e => e.into(),
    })?;
    r.store().meter.request(&c.id, Class::A);
    Ok(json_response(&util::bucket_view(r.store(), &c)))
}
