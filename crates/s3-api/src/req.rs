//! One request: the mount prefix, routing, events' origin and CORS.

use crate::error::{ApiError, ApiResult};
use crate::{Api, PREFIX, bucket, extras, object, render, sign, tus};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use std::net::IpAddr;
use std::sync::Arc;

pub(crate) struct Req {
    pub api: Arc<Api>,
    pub method: Method,
    /// The path after the mount (`/storage/v1`), still percent-encoded, without its leading `/`.
    pub path: String,
    /// `/storage/v1` when the request came through it, else `""`: what URLs we hand out start
    /// with.
    pub mount: &'static str,
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub peer: Option<IpAddr>,
}

impl Req {
    /// A query parameter's (decoded) value.
    pub fn q(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn h(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// Traffic to the peer is inside roll (free egress).
    pub fn internal(&self) -> bool {
        self.peer.is_some_and(|ip| self.api.server.is_internal(ip))
    }

    /// This API's base URL as the client reached it (TUS upload URLs).
    pub fn base_url(&self) -> String {
        if let Some(u) = &self.api.cfg.public_url {
            return u.trim_end_matches('/').to_string();
        }
        let host = self.h("x-forwarded-host").filter(|_| self.internal()).or_else(|| self.h("host")).unwrap_or("localhost");
        let https = self.internal() && self.h("x-forwarded-proto").is_some_and(|p| p.trim().eq_ignore_ascii_case("https"));
        format!("{}://{host}{}", if https { "https" } else { "http" }, self.mount)
    }

    pub fn store(&self) -> &Arc<s3_store::Store> {
        self.api.store()
    }

    /// A checker of what `caller` may do, for this request.
    pub fn checker(&self, caller: &s3_server::app::AppCaller) -> s3_server::app::Checker {
        caller.checker(&self.api.server, self.peer, &self.headers)
    }
}

fn request_id() -> String {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("os randomness");
    hex::encode_upper(b)
}

/// `rest` of `path` after the segment(s) `first` (`Some("")` when nothing follows).
fn after<'a>(path: &'a str, first: &str) -> Option<&'a str> {
    let rest = path.strip_prefix(first)?;
    if rest.is_empty() {
        return Some("");
    }
    rest.strip_prefix('/')
}

pub(crate) fn decode(s: &str) -> ApiResult<String> {
    s3_proto::http::percent_decode_path(s).map_err(|_| ApiError::invalid("The path isn't valid UTF-8 after percent-decoding"))
}

/// `<bucket>/<key…>` (both decoded); the key `None` when absent or empty.
pub(crate) fn bucket_and_key(rest: &str) -> ApiResult<(String, Option<String>)> {
    let (b, k) = rest.split_once('/').unwrap_or((rest, ""));
    let bucket = decode(b)?;
    if bucket.is_empty() {
        return Err(ApiError::missing("bucketName"));
    }
    let key = decode(k)?;
    Ok((bucket, Some(key).filter(|k| !k.is_empty())))
}

/// `<bucket>/<key…>` where the key is required.
pub(crate) fn object_path(rest: &str) -> ApiResult<(String, String)> {
    match bucket_and_key(rest)? {
        (b, Some(k)) => Ok((b, k)),
        (_, None) => Err(ApiError::missing("*")),
    }
}

pub(crate) async fn dispatch(api: Arc<Api>, peer: Option<IpAddr>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let raw = parts.uri.path();
    let (mount, path) = match raw.strip_prefix(PREFIX) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => (PREFIX, rest),
        _ => ("", raw),
    };
    let r = Req {
        api: api.clone(),
        method: parts.method.clone(),
        path: path.trim_start_matches('/').to_string(),
        mount,
        query: s3_proto::http::parse_query(parts.uri.query().unwrap_or_default()),
        headers: parts.headers,
        peer,
    };
    let head = r.method == Method::HEAD;
    let origin = s3_store::Origin { source_ip: peer.map(|p| p.to_string()), request_id: Some(request_id()), ..Default::default() };
    let result = s3_store::events::with_origin(origin, route(&r, body)).await;
    let ok = result.is_ok();
    let mut resp = match result {
        Ok(resp) => resp,
        Err(e) => {
            if e.status >= 500 {
                tracing::warn!(status = e.status, code = e.code, message = e.message, method = %r.method, path = r.path, "storage API request failed");
            }
            e.into_response_for(head)
        }
    };
    if ok && !matches!(r.method, Method::GET | Method::HEAD | Method::OPTIONS) {
        api.server.notifier.wake();
    }
    cors(&r, &mut resp, false);
    resp
}

async fn route(r: &Req, body: Body) -> ApiResult<Response> {
    let p = r.path.as_str();
    let m = r.method.clone();
    if m == Method::OPTIONS {
        if after(p, "upload/resumable").is_some() {
            return Ok(tus::options(r));
        }
        let mut resp = Response::new(Body::empty());
        *resp.status_mut() = StatusCode::NO_CONTENT;
        cors(r, &mut resp, true);
        return Ok(resp);
    }
    match (&m, p) {
        (&Method::GET, "version") => return Ok(util_text(concat!("roll-storage-api/", env!("CARGO_PKG_VERSION")))),
        (&Method::GET | &Method::HEAD, "status" | "health") => return Ok(util_text("")),
        _ => {}
    }
    if let Some(rest) = after(p, "bucket") {
        return bucket::route(r, &m, rest, body).await;
    }
    if let Some(rest) = after(p, "object") {
        return object_route(r, &m, rest, body).await;
    }
    if let Some(rest) = after(p, "render/image") {
        return Ok(render::route(r, &m, rest).await);
    }
    if let Some(rest) = after(p, "upload/resumable") {
        return tus::route(r, &m, rest, body).await;
    }
    if let Some(rest) = after(p, "roll") {
        return extras::route(r, &m, rest, body).await;
    }
    Err(ApiError::new(404, "NotFound", "Not Found", format!("Route {} /{p} not found", m)))
}

fn util_text(s: &'static str) -> Response {
    let mut r = Response::new(Body::from(s));
    r.headers_mut().insert("content-type", HeaderValue::from_static("text/plain; charset=utf-8"));
    r
}

async fn object_route(r: &Req, m: &Method, rest: &str, body: Body) -> ApiResult<Response> {
    let get = *m == Method::GET;
    let head = *m == Method::HEAD;
    if *m == Method::POST {
        if let Some(b) = after(rest, "list") {
            return object::list(r, b, body).await;
        }
        if let Some(b) = after(rest, "list-v2") {
            return object::list_v2(r, b, body).await;
        }
        if rest == "move" {
            return object::move_object(r, body).await;
        }
        if rest == "copy" {
            return object::copy(r, body).await;
        }
    }
    if let Some(rest2) = after(rest, "upload/sign") {
        return match *m {
            Method::POST => sign::sign_upload(r, rest2).await,
            Method::PUT => sign::signed_upload(r, rest2, body).await,
            _ => Err(method_not_allowed(m)),
        };
    }
    if let Some(rest2) = after(rest, "sign") {
        return match *m {
            Method::POST if !rest2.contains('/') => sign::sign_many(r, rest2, body).await,
            Method::POST => sign::sign(r, rest2, body).await,
            Method::GET | Method::HEAD => sign::signed_download(r, rest2).await,
            _ => Err(method_not_allowed(m)),
        };
    }
    if let Some(rest2) = after(rest, "public").filter(|_| get || head) {
        return object::download(r, rest2, object::Access::Public).await;
    }
    if let Some(rest2) = after(rest, "authenticated").filter(|_| get || head) {
        return object::download(r, rest2, object::Access::Authenticated).await;
    }
    if let Some(rest2) = after(rest, "info").filter(|_| get) {
        if let Some(rest3) = after(rest2, "public") {
            return object::info(r, rest3, object::Access::Public).await;
        }
        if let Some(rest3) = after(rest2, "authenticated") {
            return object::info(r, rest3, object::Access::Authenticated).await;
        }
        return object::info(r, rest2, object::Access::Optional).await;
    }
    match *m {
        Method::GET | Method::HEAD => object::download(r, rest, object::Access::Optional).await,
        Method::POST => object::upload(r, rest, body, false).await,
        Method::PUT => object::upload(r, rest, body, true).await,
        Method::DELETE if !rest.contains('/') => object::delete_many(r, rest, body).await,
        Method::DELETE => object::delete_one(r, rest).await,
        _ => Err(method_not_allowed(m)),
    }
}

pub(crate) fn method_not_allowed(m: &Method) -> ApiError {
    ApiError::new(405, "MethodNotAllowed", "Method Not Allowed", format!("{m} isn't supported here"))
}

/// Headers browsers may read from our responses.
const EXPOSE: &str = "content-length, content-type, content-range, content-disposition, content-encoding, cache-control, etag, last-modified, expires, \
     x-robots-tag, location, tus-resumable, tus-version, tus-max-size, tus-extension, upload-offset, upload-length, upload-defer-length, upload-metadata, \
     upload-expires, x-request-id";
const ALLOW_HEADERS: &str = "authorization, x-client-info, apikey, content-type, cache-control, x-upsert, x-metadata, x-signature, range, if-none-match, \
     if-modified-since, tus-resumable, upload-length, upload-defer-length, upload-metadata, upload-offset, upload-concat, x-http-method-override, x-requested-with";

/// Open CORS, as Supabase's: any origin, every method, the API's headers.
pub(crate) fn cors(r: &Req, resp: &mut Response, preflight: bool) {
    let h = resp.headers_mut();
    h.insert("access-control-allow-origin", HeaderValue::from_static("*"));
    h.insert("access-control-expose-headers", HeaderValue::from_static(EXPOSE));
    if preflight {
        h.insert("access-control-allow-methods", HeaderValue::from_static("GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS"));
        let asked = r.h("access-control-request-headers").and_then(|v| HeaderValue::from_str(v).ok()).unwrap_or_else(|| HeaderValue::from_static(ALLOW_HEADERS));
        h.insert("access-control-allow-headers", asked);
        h.insert("access-control-max-age", HeaderValue::from_static("86400"));
    }
}
