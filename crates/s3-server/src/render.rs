//! Image transformations: an object, resized, compressed and converted on the fly
//! (`s3-image`), with the result cached as a derived blob (`s3_store::derived`).
//!
//! [`render`] is the entry point for any API (Supabase's `/render/image/...`); it doesn't
//! authorize anything: the caller has already decided the requester may read the object. roll's
//! S3 API offers it as an extension, `GET /<bucket>/<key>?x-roll-transform=width:300,format:webp`,
//! authorized exactly like GetObject (`s3:GetObject`, or `s3:GetObjectVersion` with a
//! `versionId`).
//!
//! - **Cache**: keyed by the source blob and the resolved transformation ([`Plan::key`]). Blobs
//!   never change, so a cached image is never stale; an overwritten object is a new blob, with
//!   new entries. Concurrent requests for the same image compute it once (the others wait for
//!   it, then read the cache).
//! - **CPU**: transformations run on the blocking pool, at most `Config::image_concurrency` at a
//!   time (a request waits for a slot up to [`SLOT_WAIT`], then gets `SlowDown`), each within
//!   `Config::image_limits` (input bytes, pixels, output size, time), so they can't starve the
//!   S3 path. The source is read with its size checked first (25 MiB by default), into memory,
//!   inside the slot: memory is bounded by slots × (input + decoded pixels).
//! - **Billing**: a source blob counts once per bucket per month, however many variants are made
//!   (`Store::record_transform`); only successful renders count. Requests and egress are metered
//!   like a GetObject.
//! - **Encryption**: SSE-C objects are refused (roll never holds their keys). A derived image of
//!   an SSE-KMS object is encrypted with the same KMS key.
//! - **Animated** GIF, WebP and APNG are served unchanged unless a format conversion is asked for
//!   (then the first frame is converted); they aren't cached (the source is already a blob).
//! - **Headers**: `Content-Type` of the output; `ETag` identifying the derived image (the source
//!   blob and the transformation: outputs are deterministic, so it survives the cache entry being
//!   made again); `Cache-Control` a year and `immutable` when the version was named (the bytes
//!   can never change), else the object's own `Cache-Control`, else an hour; `Vary: Accept` when
//!   the format came from `Accept`; `x-roll-transform-cache: hit | miss`.

use crate::Server;
use crate::ctx::Ctx;
use crate::error::StoreResult;
use crate::response::Reply;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use s3_image::{ImageError, Params, Plan, Rendered};
use s3_proto::http::quote_etag;
use s3_proto::time::http_date;
use s3_proto::{Code, S3Error};
use s3_store::{ByteStream, Class, Derived, Encryption, NewDerived, ObjectMeta, Store, StoreError};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long a request waits for a transformation slot before `SlowDown`.
pub const SLOT_WAIT: Duration = Duration::from_secs(10);

/// The default number of concurrent transformations: half the cores, at most 4 (each may hold
/// a 25 MiB input and a 200 MB decoded image).
pub fn default_concurrency() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get() / 2).clamp(1, 4)
}

/// The transformation slots and the images being made.
pub(crate) struct Images {
    slots: Arc<tokio::sync::Semaphore>,
    /// One computation per (source blob, transformation) at a time.
    making: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Images {
    pub(crate) fn new(concurrency: usize) -> Self {
        Self { slots: Arc::new(tokio::sync::Semaphore::new(concurrency.max(1))), making: Mutex::new(HashMap::new()) }
    }
}

/// What to render.
#[derive(Clone, Debug, Default)]
pub struct RenderRequest<'a> {
    pub bucket: &'a str,
    pub key: &'a str,
    /// A specific version (`None`: the current one).
    pub version: Option<&'a str>,
    pub params: Params,
    /// The request's `Accept` header (format negotiation when `params.format` is `Auto`).
    pub accept: Option<&'a str>,
    /// The request's `If-None-Match`: a match answers 304 without reading anything.
    pub if_none_match: Option<&'a str>,
    /// The requester is inside roll (its egress is free).
    pub internal: bool,
    /// Requester Pays: the requester's account, billed for the request and egress.
    pub payer: Option<&'a str>,
    /// Fetched by the CDN (a miss at the edge): its bytes are an origin pull, not egress.
    pub cdn: bool,
}

/// A rendered image: status 200 with a body, or 304 without.
pub struct Rendition {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Option<ByteStream>,
}

impl Rendition {
    /// As an HTTP response.
    pub fn into_response(self) -> Response {
        let mut r = Response::new(match self.body {
            Some(b) => Body::from_stream(b),
            None => Body::empty(),
        });
        *r.status_mut() = self.status;
        *r.headers_mut() = self.headers;
        r
    }
}

/// The S3 error for a transformation's failure.
pub fn image_error(e: ImageError) -> S3Error {
    match e {
        ImageError::InvalidParams(m) => S3Error::msg(Code::InvalidArgument, m).with("ArgumentName", "x-roll-transform"),
        ImageError::TooLarge(m) | ImageError::Unsupported(m) | ImageError::Corrupt(m) => S3Error::msg(Code::InvalidRequest, m),
        ImageError::Timeout => S3Error::msg(Code::SlowDown, "the image took too long to transform; try a smaller one"),
        ImageError::Internal(m) => {
            tracing::error!(error = m, "image transformation failed");
            S3Error::new(Code::InternalError)
        }
    }
}

/// The derived image's ETag: a hash of the source blob and the transformation.
fn etag_of(source_blob: &str, plan: &Plan) -> String {
    let mut h = Sha256::new();
    h.update(source_blob.as_bytes());
    h.update(b"\n");
    h.update(plan.canonical().as_bytes());
    hex::encode(&h.finalize()[..16])
}

/// `If-None-Match` lists `etag` (or is `*`).
fn none_match(header: Option<&str>, etag: &str) -> bool {
    header.is_some_and(|h| h.split(',').map(|t| t.trim().trim_start_matches("W/").trim_matches('"')).any(|t| t == "*" || t == etag))
}

/// Render `req.key` of `req.bucket` (the caller has authorized reading it).
pub async fn render(server: &Server, req: &RenderRequest<'_>) -> Result<Rendition, S3Error> {
    let store = &server.store;
    let bucket = store.bucket(req.bucket).s3()?;
    let (meta, enc) = store.get_version_encryption(req.bucket, req.key, req.version).s3()?;
    store.meter.request_by(&bucket.id, req.payer, Class::B);
    if matches!(enc, Some(Encryption::Customer { .. })) {
        return Err(S3Error::msg(Code::InvalidRequest, "Objects encrypted with a customer-provided key (SSE-C) can't be transformed."));
    }
    let limits = &server.cfg.image_limits;
    if meta.size > limits.max_input_bytes as u64 {
        return Err(S3Error::msg(Code::InvalidRequest, format!("Images over {} MiB can't be transformed.", limits.max_input_bytes >> 20)));
    }
    let plan = req.params.resolve(req.accept);
    let etag = etag_of(&meta.blob_id, &plan);
    let mut headers = HeaderMap::new();
    set(&mut headers, "etag", &quote_etag(&etag));
    set(&mut headers, "last-modified", &http_date(meta.modified_at));
    let cache_control = match (req.version, &meta.headers.cache_control) {
        (Some(_), _) => "max-age=31536000, immutable".to_string(),
        (None, Some(c)) => c.clone(),
        (None, None) => "max-age=3600".to_string(),
    };
    set(&mut headers, "cache-control", &cache_control);
    if plan.negotiated {
        set(&mut headers, "vary", "Accept");
    }
    if none_match(req.if_none_match, &etag) {
        return Ok(Rendition { status: StatusCode::NOT_MODIFIED, headers, body: None });
    }

    let pkey = plan.key();
    let (content_type, len, stream, hit, made) = match lookup(store, &meta.blob_id, &pkey)? {
        Some(d) => {
            let (len, s) = store.open_derived(&d).await.s3()?;
            (d.content_type, len, s, true, false)
        }
        None => match make(server, &bucket.id, &meta, &plan, &pkey).await? {
            Made::Cached(d) => {
                let (len, s) = store.open_derived(&d).await.s3()?;
                (d.content_type, len, s, true, false)
            }
            Made::New { bytes, content_type, transformed } => (content_type, bytes.len() as u64, s3_store::bytes_body(bytes), false, transformed),
        },
    };
    if let Err(e) = store.record_transform(&bucket.id, &meta.blob_id, made).await {
        // Billing is durable: a render that can't be counted fails.
        return Err(crate::error::store_error(e));
    }
    set(&mut headers, "content-type", &content_type);
    set(&mut headers, "content-length", &len.to_string());
    set(&mut headers, "x-roll-transform-cache", if hit { "hit" } else { "miss" });
    let body = crate::object::Metered::new(stream, store.clone(), &bucket.id, req.internal, req.payer.map(str::to_string), req.cdn).boxed();
    Ok(Rendition { status: StatusCode::OK, headers, body: Some(body) })
}

fn set(h: &mut HeaderMap, name: &'static str, v: &str) {
    if let Ok(v) = HeaderValue::from_str(v) {
        h.insert(name, v);
    }
}

fn lookup(store: &Store, source: &str, pkey: &str) -> Result<Option<Derived>, S3Error> {
    store.derived(source, pkey).s3()
}

enum Made {
    /// Someone else made it meanwhile.
    Cached(Derived),
    /// Made now: serve these bytes (`transformed`: a new image, not the source passed through).
    New { bytes: Bytes, content_type: String, transformed: bool },
}

/// A request's place in line for one image; the last one out (finished, failed or gone because
/// its client left) removes the entry.
struct Flight<'a> {
    images: &'a Images,
    key: String,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        let mut making = self.images.making.lock().unwrap();
        // Ours and the map's: nobody else is waiting.
        if making.get(&self.key).is_some_and(|l| Arc::ptr_eq(l, &self.lock) && Arc::strong_count(l) <= 2) {
            making.remove(&self.key);
        }
    }
}

/// Make the image (once, however many requests want it at the same time) and cache it.
async fn make(server: &Server, bucket_id: &str, meta: &ObjectMeta, plan: &Plan, pkey: &str) -> Result<Made, S3Error> {
    let key = format!("{}/{pkey}", meta.blob_id);
    let lock = server.images.making.lock().unwrap().entry(key.clone()).or_default().clone();
    let flight = Flight { images: &server.images, key, lock };
    let _turn = flight.lock.lock().await;
    make_locked(server, bucket_id, meta, plan, pkey).await
}

async fn make_locked(server: &Server, bucket_id: &str, meta: &ObjectMeta, plan: &Plan, pkey: &str) -> Result<Made, S3Error> {
    let store = &server.store;
    if let Some(d) = lookup(store, &meta.blob_id, pkey)? {
        return Ok(Made::Cached(d));
    }
    let slot = match tokio::time::timeout(SLOT_WAIT, server.images.slots.clone().acquire_owned()).await {
        Ok(Ok(p)) => p,
        _ => return Err(S3Error::msg(Code::SlowDown, "Too many images are being transformed; retry shortly.")),
    };
    let input = read_source(store, meta, server.cfg.image_limits.max_input_bytes).await?;
    let limits = server.cfg.image_limits.clone();
    let p = *plan;
    // The slot moves into the blocking task: a request that gives up doesn't free it while the
    // work still runs.
    let rendered = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        s3_image::transform(&input, &p, &limits).map(|r| (r, input))
    })
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "image transformation panicked");
        S3Error::new(Code::InternalError)
    })?
    .map_err(image_error)?;
    let (bytes, content_type) = match rendered {
        (Rendered::Original { format }, input) => return Ok(Made::New { bytes: Bytes::from(input), content_type: format.content_type().to_string(), transformed: false }),
        (Rendered::Image { bytes, format, .. }, _) => (Bytes::from(bytes), format.content_type().to_string()),
    };
    let sse = store.derived_encryption(&meta.blob_id).s3()?;
    let blob = crate::object::InflightBlob::new(store);
    let up = match store.put_derived_blob(&blob.id, bucket_id, &meta.blob_id, pkey, bytes.clone(), &content_type, sse.as_ref()).await {
        Ok(u) => u,
        Err(e) => {
            // The image is good; only caching it failed.
            tracing::warn!(error = %e, "storing a derived image failed");
            return Ok(Made::New { bytes, content_type, transformed: true });
        }
    };
    let new = NewDerived { source_blob: meta.blob_id.clone(), params: pkey.to_string(), blob_id: blob.id.clone(), size: up.size, crc32c: up.crc32c, content_type: content_type.clone() };
    match store.commit_derived(&new, sse.as_ref()).await {
        Ok((_, true)) => {}
        Ok((_, false)) | Err(StoreError::NotFound(_)) => store.discard_blob(&blob.id).await,
        Err(e) => {
            // Not recorded: the orphan scan removes the blob.
            tracing::warn!(error = %e, "recording a derived image failed");
        }
    }
    drop(blob);
    Ok(Made::New { bytes, content_type, transformed: true })
}

/// The source's bytes, at most `max` of them.
async fn read_source(store: &Arc<Store>, meta: &ObjectMeta, max: usize) -> Result<Vec<u8>, S3Error> {
    if meta.size == 0 {
        return Err(image_error(ImageError::Unsupported("the object is empty".into())));
    }
    let (_, mut stream) = store.open_blob_sse(meta, None, None).await.s3()?;
    let mut buf = Vec::with_capacity(meta.size.min(max as u64) as usize);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            tracing::warn!(error = %e, key = meta.key, "reading an image to transform failed");
            S3Error::new(Code::InternalError)
        })?;
        if buf.len() + chunk.len() > max {
            return Err(image_error(ImageError::TooLarge(format!("images over {} MiB can't be transformed", max >> 20))));
        }
        buf.extend_from_slice(&chunk);
    }
    if buf.len() as u64 != meta.size {
        tracing::error!(key = meta.key, blob = meta.blob_id, read = buf.len(), size = meta.size, "an object's blob has the wrong size");
        return Err(S3Error::new(Code::InternalError));
    }
    Ok(buf)
}

/// The S3 extension: `GET /<bucket>/<key>?x-roll-transform=<spec>`, authorized like GetObject.
pub(crate) async fn get(ctx: &mut Ctx, bucket: &s3_store::Bucket) -> Reply {
    ctx.require_object_read(bucket)?;
    let spec = ctx.q("x-roll-transform").unwrap_or_default().to_string();
    let params = Params::parse_spec(&spec).map_err(image_error)?;
    let vid = crate::versioning::version_id(ctx)?;
    let payer = ctx.payer(&bucket.id);
    let req = RenderRequest {
        bucket: &bucket.name,
        key: ctx.key_name(),
        version: vid.as_deref(),
        params,
        accept: ctx.h("accept"),
        if_none_match: ctx.h("if-none-match"),
        internal: ctx.internal(),
        payer: payer.as_deref(),
        cdn: ctx.cdn.is_some(),
    };
    Ok(render(&ctx.server, &req).await?.into_response())
}
