//! Image transformations, as Supabase serves them: `GET /render/image/{public|authenticated|sign}
//! /{bucket}/{key}?width=&height=&resize=cover|contain|fill&quality=&format=`.
//!
//! - **Parameters** are Supabase's (`s3_image::Params`): `width` and `height` 1–2500 px (0 or
//!   empty: not given), `resize` (cover by default), `quality` 20–100 (80), and `format`:
//!   `origin` keeps the source's format, `webp`, `avif`, `jpeg` and `png` convert, and without
//!   one the image is WebP when the client's `Accept` lists it (`Vary: Accept`), as on Supabase.
//! - **Authorization** is the matching object route's: `public` is anonymous (the bucket's
//!   public flag or policy), `authenticated` the caller's key or JWT through the one evaluator
//!   (`s3:GetObject`), `sign` a signed URL's token, whose `transformations` claim *is* the
//!   transformation (the query can't change it: a different one is refused, and a tampered token
//!   fails its signature).
//! - **Rendering** is `s3_server::render::render` (authorization-agnostic): the derived-image
//!   cache keyed by the source blob, bounded CPU, SSE-C refused, image limits, and metering
//!   (a class B request, egress, Requester Pays, the origin image counted once a month). Through
//!   the CDN its bytes are an origin pull.

use crate::error::{ApiError, ApiResult};
use crate::object::{bucket_for, read_denied, version_param};
use crate::req::{Req, object_path};
use crate::sign::{DOWNLOAD, verify_token};
use crate::{auth, util};
use axum::http::{HeaderValue, Method, header};
use axum::response::Response;
use s3_image::{Format, Params, Resize};
use s3_proto::{Code, S3Error};
use s3_server::app::AppCaller;
use s3_store::Bucket;
use serde_json::Value;

/// How a render request is authorized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderMode {
    Public,
    Authenticated,
    Signed,
}

/// Query parameters that are transformation options.
const OPTIONS: [&str; 5] = ["width", "height", "resize", "quality", "format"];

/// Whether the query asks for a transformation at all.
pub(crate) fn has_options(q: &[(String, String)]) -> bool {
    q.iter().any(|(k, _)| OPTIONS.contains(&k.as_str()))
}

/// The transformation options in a query (`?width=200&format=webp`).
pub(crate) fn query_params(q: &[(String, String)]) -> ApiResult<Params> {
    Params::from_query(q.iter().map(|(k, v)| (k.as_str(), v.as_str()))).map_err(|e| ApiError::invalid(e.to_string()))
}

/// The canonical spec of `p` (what a signed URL's `transformations` claim holds): only what
/// differs from the defaults, so `Params::parse_spec(&spec(p)) == p`.
pub(crate) fn spec(p: &Params) -> String {
    let mut out = Vec::new();
    if let Some(w) = p.width {
        out.push(format!("width:{w}"));
    }
    if let Some(h) = p.height {
        out.push(format!("height:{h}"));
    }
    if p.resize != Resize::default() {
        out.push(format!("resize:{}", p.resize.as_str()));
    }
    if let Some(q) = p.quality {
        out.push(format!("quality:{q}"));
    }
    if p.format != Format::default() {
        out.push(format!("format:{}", p.format.as_str()));
    }
    out.join(",")
}

/// A transformation's failure as Supabase shapes errors.
pub(crate) fn render_error(e: S3Error) -> ApiError {
    match e.code {
        Code::SlowDown => ApiError::new(503, "SlowDown", "Too Many Requests", e.message),
        Code::InvalidArgument | Code::InvalidRequest => ApiError::new(400, "InvalidImage", "InvalidImage", e.message),
        _ => e.into(),
    }
}

/// What to render, once authorized.
pub(crate) struct Target<'a> {
    pub bucket: &'a Bucket,
    pub key: &'a str,
    pub version: Option<&'a str>,
    pub params: Params,
    /// Requester Pays: who pays (`None`: the bucket's owner).
    pub payer: Option<String>,
    /// Only this blob (an immutable URL's).
    pub expect_blob: Option<&'a str>,
    /// A signed URL's expiry (Unix seconds), sent as `Expires`.
    pub expires_at_s: Option<i64>,
}

/// Render `t` and answer with it (`?download=` honoured, headers only for HEAD).
pub(crate) async fn respond(r: &Req, t: Target<'_>) -> ApiResult<Response> {
    let req = s3_server::render::RenderRequest {
        bucket: &t.bucket.name,
        key: t.key,
        version: t.version,
        params: t.params,
        accept: r.h("accept"),
        if_none_match: r.h("if-none-match"),
        internal: r.internal(),
        payer: t.payer.as_deref(),
        cdn: r.cdn,
        expect_blob: t.expect_blob,
        unbilled: false,
    };
    let mut resp = s3_server::render::render(&r.api.server, &req).await.map_err(render_error)?.into_response();
    let h = resp.headers_mut();
    if let Some(d) = r.q("download")
        && let Ok(v) = HeaderValue::from_str(&util::content_disposition(d))
    {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    if let Some(exp) = t.expires_at_s
        && let Ok(v) = HeaderValue::from_str(&s3_proto::time::http_date(exp * 1000))
    {
        h.insert(header::EXPIRES, v);
    }
    h.insert("x-robots-tag", HeaderValue::from_static("none"));
    if r.method == Method::HEAD {
        // Nothing is sent: nothing is metered as egress.
        *resp.body_mut() = axum::body::Body::empty();
    }
    Ok(resp)
}

pub(crate) async fn route(r: &Req, m: &Method, rest: &str) -> ApiResult<Response> {
    if !matches!(*m, Method::GET | Method::HEAD) {
        return Err(crate::req::method_not_allowed(m));
    }
    let (mode, path) = if let Some(p) = rest.strip_prefix("public/") {
        (RenderMode::Public, p)
    } else if let Some(p) = rest.strip_prefix("authenticated/") {
        (RenderMode::Authenticated, p)
    } else if let Some(p) = rest.strip_prefix("sign/") {
        (RenderMode::Signed, p)
    } else {
        return Err(ApiError::new(404, "NotFound", "Not Found", "Route not found"));
    };
    let (bucket, key) = object_path(path)?;
    match mode {
        RenderMode::Public | RenderMode::Authenticated => {
            let caller = if mode == RenderMode::Public { AppCaller::anonymous() } else { auth::required(r).await? };
            let params = query_params(&r.query)?;
            let b = bucket_for(r, &caller, &bucket)?;
            let checker = r.checker(&caller);
            checker.allows("s3:GetObject", &b.name, Some(&key)).map_err(|e| read_denied(e, &caller))?;
            r.cdn_allows(&b)?;
            let payer = checker.payer(&b.id);
            respond(r, Target { bucket: &b, key: &key, version: version_param(r)?, params, payer, expect_blob: None, expires_at_s: None }).await
        }
        RenderMode::Signed => {
            let token = r.q("token").filter(|t| !t.is_empty()).ok_or_else(|| ApiError::missing("token"))?;
            let claims = verify_token(r, token, &format!("{bucket}/{key}"), DOWNLOAD).await?;
            let bound = claims.get("transformations").and_then(Value::as_str).unwrap_or_default();
            let params = Params::parse_spec(bound).map_err(|_| ApiError::invalid_signature("Invalid signature"))?;
            // The transformation is the token's: the URL can't ask for another one.
            if has_options(&r.query) && query_params(&r.query)? != params {
                return Err(ApiError::invalid_signature("The transformation is part of the signed URL; sign a URL for this one"));
            }
            let b = r.store().bucket(&bucket)?;
            r.cdn_allows(&b)?;
            let version = claims.get("versionId").and_then(Value::as_str);
            let exp = claims.get("exp").and_then(Value::as_i64);
            respond(r, Target { bucket: &b, key: &key, version, params, payer: None, expect_blob: None, expires_at_s: exp }).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_round_trip() {
        let q = |s: &str| query_params(&s3_proto::http::parse_query(s)).unwrap();
        for (query, want) in [
            ("width=100&resize=cover&format=webp&quality=80", "width:100,quality:80,format:webp"),
            ("height=50&width=60&resize=contain", "width:60,height:50,resize:contain"),
            ("format=origin", "format:origin"),
            ("width=0&cacheNonce=3", ""),
            ("", ""),
        ] {
            let p = q(query);
            assert_eq!(spec(&p), want, "{query}");
            assert_eq!(Params::parse_spec(&spec(&p)).unwrap(), p, "{query}");
        }
        for bad in ["width=0x", "width=2501", "resize=stretch", "quality=5", "format=gif", "width=1&width=2"] {
            assert!(query_params(&s3_proto::http::parse_query(bad)).is_err(), "{bad}");
        }
        assert!(has_options(&s3_proto::http::parse_query("a=1&height=3")));
        assert!(!has_options(&s3_proto::http::parse_query("token=x&download=")));
    }
}
