//! One request: addressing, auth, routing and the common response headers.

use crate::auth::{self, Auth};
use crate::response::{self, Reply};
use crate::{Server, bucket, cors, object, post};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use s3_proto::http::{parse_query, percent_decode_utf8};
use s3_proto::{Code, S3Error};
use s3_store::{Class, Store};
use std::net::IpAddr;
use std::sync::Arc;

pub(crate) struct Ctx {
    pub server: Arc<Server>,
    pub method: Method,
    /// As on the request line (percent-encoded), and the raw query.
    pub raw_path: String,
    pub raw_query: String,
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub bucket: Option<String>,
    pub key: Option<String>,
    /// The bucket came from the `Host` header (virtual-host style).
    pub vhost: bool,
    pub peer: Option<IpAddr>,
    pub auth: Option<Auth>,
    /// Addressed to the bucket's website endpoint (`<bucket>.<website_domain>`).
    pub website: bool,
    pub(crate) access_cache: crate::access::AccessCache,
    pub(crate) started: std::time::Instant,
}

impl Ctx {
    pub fn store(&self) -> &Arc<Store> {
        &self.server.store
    }

    /// A query parameter's (decoded) value.
    pub fn q(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// Whether a query parameter (subresource) is present.
    pub fn has(&self, name: &str) -> bool {
        self.query.iter().any(|(k, _)| k == name)
    }

    pub fn h(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// Every header as text (lossy), for signing and metadata.
    pub fn header_pairs(&self) -> Vec<(String, String)> {
        self.headers.iter().map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned())).collect()
    }

    pub fn bucket_name(&self) -> &str {
        self.bucket.as_deref().unwrap_or_default()
    }

    pub fn key_name(&self) -> &str {
        self.key.as_deref().unwrap_or_default()
    }

    /// `/bucket/key`, for error bodies.
    pub fn resource(&self) -> String {
        match (&self.bucket, &self.key) {
            (Some(b), Some(k)) => format!("/{b}/{k}"),
            (Some(b), None) => format!("/{b}"),
            _ => "/".into(),
        }
    }

    pub fn internal(&self) -> bool {
        self.peer.is_some_and(|ip| self.server.is_internal(ip))
    }

    pub fn meter(&self, bucket_id: &str, class: Class) {
        self.store().meter.request(bucket_id, class);
    }

    /// Unix seconds now (the store's clock, so tests can move it).
    pub fn now_s(&self) -> i64 {
        self.store().now().div_euclid(1000)
    }
}

fn request_id() -> String {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("os randomness");
    hex::encode_upper(b)
}

/// Bucket and key from the `Host` header (virtual-host style, `true`) or the path.
fn address(server: &Server, headers: &HeaderMap, raw_path: &str) -> Result<(Option<String>, Option<String>, bool), S3Error> {
    let path = raw_path.strip_prefix('/').unwrap_or(raw_path);
    if let Some(b) = crate::website::host_bucket(server, headers) {
        return Ok((Some(b), Some(percent_decode_utf8(path)?), true));
    }
    if let Some(domain) = &server.cfg.virtual_host_domain {
        // Host names are case-insensitive.
        let host = headers.get("host").and_then(|h| h.to_str().ok()).unwrap_or_default().to_ascii_lowercase();
        let host = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|c| c.is_ascii_digit())).map_or(host.as_str(), |(h, _)| h);
        if let Some(b) = host.strip_suffix(domain.to_ascii_lowercase().as_str()).and_then(|h| h.strip_suffix('.')).filter(|b| !b.is_empty()) {
            let key = Some(percent_decode_utf8(path)?).filter(|k| !k.is_empty());
            return Ok((Some(b.to_string()), key, true));
        }
    }
    if path.is_empty() {
        return Ok((None, None, false));
    }
    let (b, k) = path.split_once('/').unwrap_or((path, ""));
    let bucket = percent_decode_utf8(b)?;
    let key = Some(percent_decode_utf8(k)?).filter(|k| !k.is_empty());
    Ok((Some(bucket), key, false))
}

pub(crate) async fn dispatch(server: Arc<Server>, peer: Option<IpAddr>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let raw_path = parts.uri.path().to_string();
    let raw_query = parts.uri.query().unwrap_or_default().to_string();
    let head = parts.method == Method::HEAD;
    let request_id = request_id();
    let addressed = address(&server, &parts.headers, &raw_path);
    let website = crate::website::host_bucket(&server, &parts.headers).is_some();
    let mut ctx = Ctx {
        server,
        method: parts.method,
        query: parse_query(&raw_query),
        raw_path,
        raw_query,
        headers: parts.headers,
        bucket: None,
        key: None,
        vhost: false,
        peer,
        auth: None,
        website,
        access_cache: Default::default(),
        started: std::time::Instant::now(),
    };
    let result = match addressed {
        Ok((b, k, vhost)) => {
            ctx.bucket = b;
            ctx.key = k;
            ctx.vhost = vhost;
            // Who's asking, for the events the request's changes record.
            let origin = s3_store::Origin { source_ip: peer.map(|p| p.to_string()), request_id: Some(request_id.clone()), ..Default::default() };
            s3_store::events::with_origin(origin, run(&mut ctx, body)).await
        }
        Err(e) => Err(e),
    };
    let mut error_code = None;
    let mut resp = match result {
        Ok(r) => r,
        Err(e) => {
            error_code = Some(e.code);
            if e.code == Code::InternalError || e.code == Code::ServiceUnavailable {
                tracing::warn!(code = e.code.as_str(), message = e.message, method = %ctx.method, resource = ctx.resource(), "s3 request failed");
            }
            let e = if e.resource.is_none() { e.with_resource(ctx.resource()) } else { e };
            response::error(&e, &request_id, head)
        }
    };
    response::set(&mut resp, "x-amz-request-id", &request_id);
    response::set(&mut resp, "x-amz-id-2", &request_id);
    response::set(&mut resp, "server", "roll");
    if ctx.method != Method::OPTIONS {
        cors::decorate(&ctx, &mut resp);
    }
    crate::logging::record(&ctx, &resp, &request_id, error_code);
    resp
}

fn is_form_post(ctx: &Ctx) -> bool {
    ctx.method == Method::POST && ctx.key.is_none() && ctx.bucket.is_some() && ctx.h("content-type").is_some_and(|c| c.to_ascii_lowercase().starts_with("multipart/form-data"))
}

async fn run(ctx: &mut Ctx, body: Body) -> Reply {
    if ctx.website {
        return Ok(crate::website::serve(ctx).await);
    }
    if ctx.method == Method::OPTIONS {
        return cors::preflight(ctx);
    }
    if is_form_post(ctx) {
        return post::upload(ctx, body).await;
    }
    reject_unsupported_headers(ctx)?;
    ctx.auth = auth::authenticate(ctx)?;
    if let Some(a) = &ctx.auth {
        ctx.store().touch_key(&a.key.id);
    }
    crate::owner::check(ctx)?;
    let principal = ctx.auth.as_ref().map_or_else(|| "anonymous".to_string(), |a| a.key.id.clone());
    s3_store::events::set_origin(|o| o.principal = Some(principal));
    match (&ctx.bucket, &ctx.key) {
        (None, _) => match ctx.method {
            Method::GET => bucket::list_buckets(ctx),
            _ => Err(S3Error::new(Code::MethodNotAllowed)),
        },
        (Some(_), None) => bucket::route(ctx, body).await,
        (Some(_), Some(_)) => object::route(ctx, body).await,
    }
}

/// Header values S3 itself refuses.
fn reject_unsupported_headers(ctx: &Ctx) -> Result<(), S3Error> {
    if let Some(rp) = ctx.h("x-amz-request-payer")
        && !rp.eq_ignore_ascii_case("requester")
    {
        return Err(S3Error::new(Code::InvalidArgument).with("ArgumentName", "x-amz-request-payer"));
    }
    if let Some(class) = ctx.h("x-amz-storage-class") {
        const CLASSES: [&str; 9] = ["STANDARD", "REDUCED_REDUNDANCY", "STANDARD_IA", "ONEZONE_IA", "INTELLIGENT_TIERING", "GLACIER", "DEEP_ARCHIVE", "GLACIER_IR", "OUTPOSTS"];
        if !CLASSES.contains(&class.trim()) {
            return Err(S3Error::new(Code::InvalidStorageClass).with("StorageClassRequested", class.to_string()));
        }
    }
    Ok(())
}

/// Read a small request body (XML) completely, refusing anything over `limit` bytes. Its
/// `Content-MD5` and `x-amz-checksum-*` headers are checked against it.
pub(crate) async fn read_small(ctx: &Ctx, body: Body, limit: usize) -> Result<bytes::Bytes, S3Error> {
    read_small_as(ctx, body, limit, crate::body::requested_checksum(ctx)?).await
}

/// [`read_small`] for CompleteMultipartUpload, whose `x-amz-checksum-*` headers are the whole
/// object's checksum, not the body's (`Content-MD5` is still checked).
pub(crate) async fn read_small_object_checksum(ctx: &Ctx, body: Body, limit: usize) -> Result<bytes::Bytes, S3Error> {
    read_small_as(ctx, body, limit, None).await
}

async fn read_small_as(ctx: &Ctx, body: Body, limit: usize, checksum: Option<(s3_proto::checksum::Algorithm, Option<String>)>) -> Result<bytes::Bytes, S3Error> {
    use futures::StreamExt;
    let mut decoded = crate::body::Incoming::new(ctx, body, checksum, Some(limit as u64))?;
    let mut out = Vec::new();
    while let Some(chunk) = decoded.next().await {
        let chunk = chunk.map_err(|_| decoded.error().unwrap_or_else(|| S3Error::new(Code::IncompleteBody)))?;
        if out.len() + chunk.len() > limit {
            return Err(S3Error::new(Code::MaxMessageLengthExceeded));
        }
        out.extend_from_slice(&chunk);
    }
    let f = decoded.finished()?;
    f.digest.verify_content_md5(ctx.h("content-md5"))?;
    Ok(out.into())
}

pub(crate) fn no_content() -> Reply {
    Ok(response::empty(StatusCode::NO_CONTENT))
}
