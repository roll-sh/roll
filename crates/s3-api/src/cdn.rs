//! The developer API behind the CDN (Cloudflare, the Worker in `apps/cdn-worker`).
//!
//! - **Origin requests.** The Worker reaches this API with `X-Roll-CDN-Auth: <shared secret>`
//!   (checked by `s3_server::cdn::Cdn::origin_request`; a wrong one is refused). Such a request
//!   is anonymous or carries a signed URL's token: an `apikey` or `Authorization` it has is
//!   dropped, only the cacheable reads are answered (`/object/public`, `/object/sign`,
//!   `/render/image/public`, `/render/image/sign`, `/v/`), the bucket must have the CDN on, its
//!   bytes are metered as an origin pull, and the response says how long the edge keeps it
//!   (`CDN-Cache-Control`).
//! - **Immutable URLs**, `/v/<token>/<bucket>/<key>` (the object) and
//!   `/v/<token>/<bucket>/<key>?width=…` (a rendition): public reads that only serve the
//!   object whose current blob the token names (`s3_store::cdn::immutable_token`), with
//!   `Cache-Control: public, max-age=31536000, immutable`. An overwrite makes a new blob, so a
//!   new URL: these never need purging (a delete still purges the object's tag at the edge).
//! - **URLs** (`POST /roll/url/{bucket}`): the URL to hand out for an object, the CDN's when the
//!   bucket has it on, optionally immutable or of a rendition.

use crate::error::{ApiError, ApiResult};
use crate::object::{bucket_for, read_denied, serve};
use crate::render::{self, Target, has_options, query_params};
use crate::req::{Req, object_path};
use axum::http::{HeaderValue, Method, header};
use axum::response::Response;
use s3_server::app::AppCaller;
use s3_store::cdn::{CdnMode, immutable_token};
use serde::Deserialize;
use serde_json::{Map, Value, json};

const IMMUTABLE: &str = "public, max-age=31536000, immutable";

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether a CDN origin request may use this route (`p`: the path after the mount).
pub(crate) fn routable(m: &Method, p: &str) -> bool {
    match *m {
        Method::OPTIONS => true,
        Method::GET | Method::HEAD => ["object/public/", "object/sign/", "render/image/public/", "render/image/sign/", "v/"].iter().any(|r| p.starts_with(r)),
        _ => false,
    }
}

/// How the edge may keep a response to the path `p` (after the mount).
pub(crate) fn mode_of(p: &str) -> CdnMode {
    if p.starts_with("v/") { CdnMode::Immutable } else { CdnMode::Mutable }
}

/// `GET /v/<token>/<bucket>/<key>[?<transformation>]`.
pub(crate) async fn immutable(r: &Req, m: &Method, rest: &str) -> ApiResult<Response> {
    if !matches!(*m, Method::GET | Method::HEAD) {
        return Err(crate::req::method_not_allowed(m));
    }
    let (token, path) = rest.split_once('/').ok_or_else(ApiError::no_such_key)?;
    let (bucket, key) = object_path(path)?;
    let caller = AppCaller::anonymous();
    let b = bucket_for(r, &caller, &bucket)?;
    let checker = r.checker(&caller);
    checker.allows("s3:GetObject", &b.name, Some(&key)).map_err(|e| read_denied(e, &caller))?;
    r.cdn_allows(&b)?;
    let meta = r.store().get_object(&b.name, &key)?;
    if !ct_eq(token, &immutable_token(&meta.blob_id)) {
        return Err(ApiError::no_such_key());
    }
    let mut resp = if has_options(&r.query) {
        let params = query_params(&r.query)?;
        let t = Target { bucket: &b, key: &key, version: None, params, payer: checker.payer(&b.id), expect_blob: Some(&meta.blob_id), expires_at_s: None };
        render::respond(r, t).await?
    } else {
        serve(r, &b, &meta, None).await?
    };
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(IMMUTABLE));
    Ok(resp)
}

#[derive(Deserialize)]
struct UrlReq {
    path: String,
    /// Transformation options, as `createSignedUrl`'s `transform`.
    #[serde(default)]
    transform: Option<Map<String, Value>>,
    /// An immutable URL of the object's current version (default: the bucket's CDN mode).
    #[serde(default)]
    immutable: Option<bool>,
}

/// `POST /roll/url/{bucket}` `{path, transform?, immutable?}`: the public URL to hand out, for a
/// caller that may read the object: `{publicUrl, cdn, immutable}`. Through the CDN when the
/// bucket has it on; immutable URLs name the object's current blob, so they're never stale.
pub(crate) async fn url(r: &Req, caller: &AppCaller, b: &s3_store::Bucket, body: axum::body::Body) -> ApiResult<Response> {
    let q: UrlReq = crate::util::read_json(body).await?;
    r.checker(caller).allows("s3:GetObject", &b.name, Some(&q.path)).map_err(|e| read_denied(e, caller))?;
    let meta = r.store().get_object(&b.name, &q.path)?;
    r.store().meter.request(&b.id, s3_store::Class::B);
    let transform = match &q.transform {
        Some(t) => transform_query(t)?,
        None => String::new(),
    };
    let cdn = r.store().bucket_cdn(&b.name)?.filter(|c| c.enabled);
    let immutable = q.immutable.unwrap_or(cdn.is_some_and(|c| c.mode == CdnMode::Immutable));
    let blob = immutable.then_some(meta.blob_id.as_str());
    if cdn.is_some()
        && let Some(u) = s3_server::cdn::api_url(r.store(), &b.name, &q.path, blob, &transform)?
    {
        return Ok(crate::util::json_response(&json!({ "publicUrl": u, "cdn": true, "immutable": immutable })));
    }
    // No CDN: this API's own URLs (immutable ones work here too).
    let object = format!("{}/{}", s3_proto::http::uri_encode(&b.name, true), s3_proto::http::uri_encode(&q.path, false));
    let query = if transform.is_empty() { String::new() } else { format!("?{transform}") };
    let path = match (blob, transform.is_empty()) {
        (Some(blob), _) => format!("/v/{}/{object}{query}", immutable_token(blob)),
        (None, true) => format!("/object/public/{object}"),
        (None, false) => format!("/render/image/public/{object}{query}"),
    };
    Ok(crate::util::json_response(&json!({ "publicUrl": format!("{}{path}", r.base_url()), "cdn": false, "immutable": immutable })))
}

/// A `transform` object as a render query (`width=200&format=webp`), checked.
fn transform_query(t: &Map<String, Value>) -> ApiResult<String> {
    let mut pairs = Vec::new();
    for k in ["width", "height", "resize", "quality", "format"] {
        match t.get(k) {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) => pairs.push((k.to_string(), s.clone())),
            Some(v @ Value::Number(_)) => pairs.push((k.to_string(), v.to_string())),
            Some(_) => return Err(ApiError::invalid(format!("transform.{k} must be a number or a string"))),
        }
    }
    query_params(&pairs)?;
    Ok(pairs.iter().map(|(k, v)| format!("{k}={}", s3_proto::http::uri_encode(v, true))).collect::<Vec<_>>().join("&"))
}
