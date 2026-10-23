//! Signed URLs: download URLs anyone can use until they expire, and upload URLs for one key.
//!
//! A token is an HS256 JWT (Supabase's format) signed with a key derived from the deployment's
//! API key: `{url: "<bucket>/<key>", scope: "download" | "upload", iat, exp}`, plus `versionId`
//! for a version's download, and `owner` and `upsert` for uploads. It's checked in constant time
//! and bound to its path and scope. Signing checks the caller may read (or write) the object;
//! using the URL needs nothing else, as on Supabase.

use crate::auth;
use crate::error::{ApiError, ApiResult};
use crate::object::{bucket_for, serve, write_body, write_denied};
use crate::req::{Req, bucket_and_key, object_path};
use crate::util::{self, json_response};
use axum::body::Body;
use axum::response::Response;
use s3_server::app::AppCaller;
use s3_store::Class;
use s3_store::api::jwt;
use serde::Deserialize;
use serde_json::{Map, Value, json};

pub(crate) const DOWNLOAD: &str = "download";
pub(crate) const UPLOAD: &str = "upload";
/// How long a signed upload URL lasts (Supabase's default).
const UPLOAD_EXPIRY_S: i64 = 2 * 3600;
/// The longest a signed download URL may last.
const MAX_EXPIRY_S: i64 = 10 * 365 * 86_400;

pub(crate) async fn sign_token(r: &Req, mut claims: Map<String, Value>, expires_in: i64) -> ApiResult<String> {
    let now = r.store().now().div_euclid(1000);
    claims.insert("iat".into(), json!(now));
    claims.insert("exp".into(), json!(now + expires_in));
    let key = r.store().api_url_key().await?;
    Ok(jwt::sign_hs256(&key, &Value::Object(claims)))
}

/// A token's claims, if it's ours, for `url`, of `scope`, and not expired.
pub(crate) async fn verify_token(r: &Req, token: &str, url: &str, scope: &str) -> ApiResult<Map<String, Value>> {
    let key = r.store().api_url_key().await?;
    let claims = jwt::verify_hs256(&key, token).map_err(|e| ApiError::invalid_jwt(e.to_string()))?;
    let got = claims.get("scope").and_then(Value::as_str);
    // Tokens without a scope are downloads (Supabase's older tokens), never uploads.
    let scope_ok = match scope {
        UPLOAD => got == Some(UPLOAD),
        _ => got == Some(DOWNLOAD) || (got.is_none() && !claims.contains_key("upsert") && !claims.contains_key("owner")),
    };
    if !scope_ok {
        return Err(ApiError::invalid_signature(format!("Token is not scoped for {scope}")));
    }
    if claims.get("url").and_then(Value::as_str) != Some(url) {
        return Err(ApiError::invalid_signature("Invalid signature"));
    }
    let now = r.store().now().div_euclid(1000);
    if jwt::check_times(&claims, now, 0).is_err() {
        return Err(ApiError::expired_token());
    }
    Ok(claims)
}

fn expires_in(v: &Value) -> ApiResult<i64> {
    match v.as_f64() {
        Some(f) if f >= 1.0 && f.fract() == 0.0 && f <= MAX_EXPIRY_S as f64 => Ok(f as i64),
        _ => Err(ApiError::invalid(format!("expiresIn must be a whole number of seconds from 1 to {MAX_EXPIRY_S}"))),
    }
}

#[derive(Deserialize)]
struct SignBody {
    #[serde(rename = "expiresIn")]
    expires_in: Value,
    #[serde(default)]
    transform: Option<Map<String, Value>>,
    #[serde(rename = "versionId", default)]
    version_id: Option<String>,
}

/// The transformations a signed render URL carries (Supabase's `transformations` claim).
fn transformations(t: &Map<String, Value>) -> String {
    let mut out = Vec::new();
    for (k, name) in [("width", "width"), ("height", "height"), ("resize", "resize"), ("quality", "quality"), ("format", "format")] {
        if let Some(v) = t.get(k).filter(|v| !v.is_null()) {
            let s = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
            out.push(format!("{name}:{s}"));
        }
    }
    out.join(",")
}

/// `POST /object/sign/{bucket}/{key}`: `{"signedURL": "/object/sign/<bucket>/<key>?token=…"}`.
pub(crate) async fn sign(r: &Req, rest: &str, body: Body) -> ApiResult<Response> {
    let (bucket, key) = object_path(rest)?;
    let caller = auth::required(r).await?;
    let b = bucket_for(r, &caller, &bucket)?;
    let q: SignBody = util::read_json(body).await?;
    let exp = expires_in(&q.expires_in)?;
    let version = q.version_id.filter(|v| !v.is_empty());
    readable(r, &caller, &b.name, &key, version.as_deref())?;
    r.store().meter.request(&b.id, Class::B);
    let url = format!("{}/{key}", b.name);
    let mut claims = Map::new();
    let render = q.transform.as_ref().map(transformations).filter(|t| !t.is_empty());
    if let Some(t) = &render {
        claims.insert("transformations".into(), json!(t));
    }
    claims.insert("url".into(), json!(url));
    claims.insert("scope".into(), json!(DOWNLOAD));
    if let Some(v) = &version {
        claims.insert("versionId".into(), json!(v));
    }
    let token = sign_token(r, claims, exp).await?;
    let path = if render.is_some() { "render/image" } else { "object" };
    Ok(json_response(&json!({ "signedURL": format!("/{path}/sign/{url}?token={token}") })))
}

/// The caller may read the object, which exists.
fn readable(r: &Req, caller: &AppCaller, bucket: &str, key: &str, version: Option<&str>) -> ApiResult<()> {
    r.checker(caller).allows("s3:GetObject", bucket, Some(key)).map_err(|_| ApiError::no_such_key())?;
    r.store().get_object_version(bucket, key, version)?;
    Ok(())
}

#[derive(Deserialize)]
struct SignManyBody {
    #[serde(rename = "expiresIn")]
    expires_in: Value,
    paths: Vec<String>,
}

/// `POST /object/sign/{bucket}` with `{expiresIn, paths}`: one entry per path, with an error
/// for those that don't exist or the caller can't read.
pub(crate) async fn sign_many(r: &Req, rest: &str, body: Body) -> ApiResult<Response> {
    let (bucket, _) = bucket_and_key(rest)?;
    let caller = auth::required(r).await?;
    let b = bucket_for(r, &caller, &bucket)?;
    let q: SignManyBody = util::read_json(body).await?;
    let exp = expires_in(&q.expires_in)?;
    if q.paths.is_empty() || q.paths.len() > 1000 {
        return Err(ApiError::invalid("paths must have 1 to 1000 items"));
    }
    r.store().meter.request(&b.id, Class::B);
    let mut out = Vec::with_capacity(q.paths.len());
    for path in q.paths {
        if readable(r, &caller, &b.name, &path, None).is_err() {
            out.push(json!({ "error": "Either the object does not exist or you do not have access to it", "path": path, "signedURL": null }));
            continue;
        }
        let url = format!("{}/{path}", b.name);
        let mut claims = Map::new();
        claims.insert("url".into(), json!(url));
        claims.insert("scope".into(), json!(DOWNLOAD));
        let token = sign_token(r, claims, exp).await?;
        out.push(json!({ "error": null, "path": path, "signedURL": format!("/object/sign/{url}?token={token}") }));
    }
    Ok(json_response(&Value::Array(out)))
}

/// `GET /object/sign/{bucket}/{key}?token=`: the object, with nothing but the token.
pub(crate) async fn signed_download(r: &Req, rest: &str) -> ApiResult<Response> {
    let (bucket, key) = object_path(rest)?;
    let token = r.q("token").filter(|t| !t.is_empty()).ok_or_else(|| ApiError::missing("token"))?;
    let claims = verify_token(r, token, &format!("{bucket}/{key}"), DOWNLOAD).await?;
    let b = r.store().bucket(&bucket)?;
    let version = claims.get("versionId").and_then(Value::as_str);
    let meta = r.store().get_object_version(&b.name, &key, version)?;
    let exp = claims.get("exp").and_then(Value::as_i64);
    serve(r, &b, &meta, exp).await
}

/// `POST /object/upload/sign/{bucket}/{key}`: `{"url": "/object/upload/sign/…?token=…", "token"}`,
/// for a caller that may write the key. The upload made with it belongs to that caller.
pub(crate) async fn sign_upload(r: &Req, rest: &str) -> ApiResult<Response> {
    let (bucket, key) = object_path(rest)?;
    let caller = auth::required(r).await?;
    let b = bucket_for(r, &caller, &bucket)?;
    let upsert = r.h("x-upsert").is_some_and(|v| v.trim().eq_ignore_ascii_case("true"));
    r.checker(&caller).allows("s3:PutObject", &b.name, Some(&key)).map_err(write_denied)?;
    if !upsert && r.store().get_object(&b.name, &key).is_ok() {
        return Err(ApiError::key_exists());
    }
    r.store().meter.request(&b.id, Class::B);
    let url = format!("{}/{key}", b.name);
    let mut claims = Map::new();
    if let Some(owner) = caller.sub() {
        claims.insert("owner".into(), json!(owner));
    }
    claims.insert("url".into(), json!(url));
    claims.insert("upsert".into(), json!(upsert));
    claims.insert("scope".into(), json!(UPLOAD));
    let token = sign_token(r, claims, UPLOAD_EXPIRY_S).await?;
    Ok(json_response(&json!({ "url": format!("/object/upload/sign/{url}?token={token}"), "token": token })))
}

/// `PUT /object/upload/sign/{bucket}/{key}?token=`: the upload, as the signer.
pub(crate) async fn signed_upload(r: &Req, rest: &str, body: Body) -> ApiResult<Response> {
    let (bucket, key) = object_path(rest)?;
    let token = r.q("token").filter(|t| !t.is_empty()).ok_or_else(|| ApiError::missing("token"))?;
    let claims = verify_token(r, token, &format!("{bucket}/{key}"), UPLOAD).await?;
    let b = r.store().bucket(&bucket)?;
    let owner = claims.get("owner").and_then(Value::as_str).map(str::to_string);
    let upsert = claims.get("upsert").and_then(Value::as_bool).unwrap_or(false);
    let meta = write_body(r, &b, &key, owner, upsert, body).await?;
    Ok(json_response(&json!({ "Id": util::object_id(&b.id, &meta), "Key": format!("{}/{key}", b.name) })))
}
