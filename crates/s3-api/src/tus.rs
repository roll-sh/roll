//! Resumable uploads: TUS 1.0 at `/upload/resumable` (creation, creation-with-upload,
//! creation-defer-length, termination, expiration), as Supabase serves them, with
//! `Upload-Metadata` `bucketName`, `objectName`, `contentType`, `cacheControl` and `metadata`.
//!
//! Each upload is one multipart upload of the store (so it completes into one blob, as every
//! object is) plus its `api_uploads` row (`s3_store::api`). Bytes arrive in any sizes; they're
//! buffered on local disk in `<spool>/tus/<id>.<part>` until a part is full (the store's minimum
//! part size, 5 MiB, or more for objects over ~48 GiB), then that part is uploaded to object
//! storage and recorded durably, and its buffer file deleted. The offset is the recorded parts'
//! bytes plus the current buffer file's: after a restart (the same disk) an upload resumes where
//! it was, and on a new node from its last recorded part (the client sends the rest again, as
//! TUS clients do after a `HEAD`). A buffer file is named by its part number, so one whose part
//! was recorded just before a crash is ignored. When the last byte arrives the upload completes;
//! without `x-upsert: true` an existing object makes it fail with 409.
//!
//! Signed resumable uploads (`/upload/resumable/sign`) authorize with the `x-signature` header
//! (a signed upload URL's token) instead of a key.

use crate::auth;
use crate::error::{ApiError, ApiResult};
use crate::object::{bucket_for, write_denied};
use crate::req::{Req, cors, method_not_allowed};
use crate::util;
use crate::Api;
use axum::body::Body;
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::Response;
use futures::StreamExt;
use md5::{Digest, Md5};
use s3_store::{ApiUpload, ApiUploadInfo, Class, NewApiUpload, ObjectHeaders, StoreError, WriteCondition};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const VERSION: &str = "1.0.0";
const EXTENSIONS: &str = "creation,creation-with-upload,creation-defer-length,termination,expiration";
const OFFSET_TYPE: &str = "application/offset+octet-stream";

pub(crate) fn buffer_dir(spool: &Path) -> PathBuf {
    spool.join("tus")
}

fn buffer_path(api: &Api, id: &str, part: u32) -> PathBuf {
    buffer_dir(&api.cfg.spool_dir).join(format!("{id}.{part}"))
}

/// Remove every buffer file of an upload.
async fn drop_buffers(api: &Api, id: &str) {
    let dir = buffer_dir(&api.cfg.spool_dir);
    let Ok(mut rd) = tokio::fs::read_dir(&dir).await else { return };
    let prefix = format!("{id}.");
    while let Ok(Some(e)) = rd.next_entry().await {
        if e.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = tokio::fs::remove_file(e.path()).await;
        }
    }
}

fn set(resp: &mut Response, name: &'static str, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        resp.headers_mut().insert(name, v);
    }
}

fn empty(status: StatusCode) -> Response {
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = status;
    set(&mut resp, "tus-resumable", VERSION);
    resp
}

fn err(status: u16, code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::new(status, code, "TusError", message).real()
}

/// TUS errors carry their real status.
fn real(e: ApiError) -> ApiError {
    ApiError { http: e.status, ..e }
}

pub(crate) fn options(r: &Req) -> Response {
    let mut resp = empty(StatusCode::NO_CONTENT);
    set(&mut resp, "tus-version", VERSION);
    set(&mut resp, "tus-extension", EXTENSIONS);
    set(&mut resp, "tus-max-size", &r.api.cfg.max_resumable.to_string());
    cors(r, &mut resp, true);
    resp
}

pub(crate) async fn route(r: &Req, m: &Method, rest: &str, body: Body) -> ApiResult<Response> {
    let (signed, id) = match rest {
        "" => (false, None),
        "sign" => (true, None),
        s => match s.strip_prefix("sign/") {
            Some(id) => (true, Some(id)),
            None => (false, Some(s)),
        },
    };
    let id = id.map(|i| i.trim_end_matches('/')).filter(|i| !i.is_empty());
    // Clients behind proxies that only pass GET and POST.
    let method = match (m, r.h("x-http-method-override")) {
        (&Method::POST, Some(o)) => Method::from_bytes(o.trim().to_ascii_uppercase().as_bytes()).unwrap_or(Method::POST),
        _ => m.clone(),
    };
    if r.h("tus-resumable").map(str::trim) != Some(VERSION) {
        let mut e = err(412, "TusError", "Unsupported version: send Tus-Resumable: 1.0.0").into_response_for(method == Method::HEAD);
        set(&mut e, "tus-version", VERSION);
        set(&mut e, "tus-resumable", VERSION);
        return Ok(e);
    }
    let result = match (&method, id) {
        (&Method::POST, None) => create(r, signed, body).await,
        (&Method::HEAD, Some(id)) => head(r, signed, id).await,
        (&Method::PATCH, Some(id)) => patch(r, signed, id, body).await,
        (&Method::DELETE, Some(id)) => terminate(r, signed, id).await,
        _ => Err(method_not_allowed(&method)),
    };
    match result {
        Ok(resp) => Ok(resp),
        Err(e) => {
            let mut resp = real(e).into_response_for(method == Method::HEAD);
            set(&mut resp, "tus-resumable", VERSION);
            Ok(resp)
        }
    }
}

/// `Upload-Metadata`: `key base64value` pairs, comma-separated.
fn parse_metadata(h: Option<&str>) -> ApiResult<BTreeMap<String, String>> {
    use base64::Engine;
    let mut out = BTreeMap::new();
    let Some(h) = h else { return Ok(out) };
    for pair in h.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once(' ').unwrap_or((pair, ""));
        let v = base64::engine::general_purpose::STANDARD.decode(v.trim()).map_err(|_| err(400, "InvalidParameter", "Invalid Upload-Metadata header"))?;
        out.insert(k.to_string(), String::from_utf8(v).map_err(|_| err(400, "InvalidParameter", "Invalid Upload-Metadata header"))?);
    }
    Ok(out)
}

/// Who writes: an authorized caller (its user's `sub` owns the object), or a signed upload
/// token's signer.
struct Writer {
    owner: Option<String>,
    upsert: bool,
    account: Option<String>,
}

/// Authorize writing `bucket/key`.
async fn writer(r: &Req, signed: bool, bucket: &str, key: &str, upsert_header: bool) -> ApiResult<(Writer, s3_store::Bucket)> {
    if signed {
        let token = r.h("x-signature").filter(|t| !t.is_empty()).ok_or_else(|| ApiError::invalid_signature("Missing x-signature header"))?;
        let claims = crate::sign::verify_token(r, token, &format!("{bucket}/{key}"), crate::sign::UPLOAD).await?;
        let b = r.store().bucket(bucket)?;
        let owner = claims.get("owner").and_then(|v| v.as_str()).map(str::to_string);
        let upsert = claims.get("upsert").and_then(|v| v.as_bool()).unwrap_or(false);
        return Ok((Writer { owner, upsert, account: None }, b));
    }
    let caller = auth::required(r).await?;
    let b = bucket_for(r, &caller, bucket)?;
    r.checker(&caller).allows("s3:PutObject", &b.name, Some(key)).map_err(|e| write_denied(e).with_http(403))?;
    Ok((Writer { owner: caller.sub().map(str::to_string), upsert: upsert_header, account: caller.account().map(str::to_string) }, b))
}

/// An upload the request may continue.
async fn load(r: &Req, signed: bool, id: &str, writing: bool) -> ApiResult<ApiUpload> {
    let up = r.store().api_upload(id)?.ok_or_else(|| err(404, "NoSuchUpload", "Upload not found"))?;
    if signed {
        let token = r.h("x-signature").filter(|t| !t.is_empty()).ok_or_else(|| ApiError::invalid_signature("Missing x-signature header"))?;
        crate::sign::verify_token(r, token, &format!("{}/{}", up.bucket, up.upload.key), crate::sign::UPLOAD).await?;
        return Ok(up);
    }
    let caller = auth::required(r).await?;
    if caller.account() != Some(up.account.as_str()) {
        return Err(err(404, "NoSuchUpload", "Upload not found"));
    }
    if writing {
        r.checker(&caller).allows("s3:PutObject", &up.bucket, Some(&up.upload.key)).map_err(|e| write_denied(e).with_http(403))?;
    }
    Ok(up)
}

/// What's been received: the recorded parts and the current buffer.
struct Progress {
    committed: u64,
    next_part: u32,
    buffered: u64,
}

impl Progress {
    fn offset(&self) -> u64 {
        self.committed + self.buffered
    }
}

async fn progress(api: &Api, up: &ApiUpload) -> ApiResult<Progress> {
    let (parts, _) = api.store().list_parts(&up.bucket, &up.id, 0, 10_000)?;
    let committed = parts.iter().map(|p| p.size).sum();
    let next_part = parts.iter().map(|p| p.part_number).max().unwrap_or(0) + 1;
    let buffered = tokio::fs::metadata(buffer_path(api, &up.id, next_part)).await.map(|m| m.len()).unwrap_or(0);
    Ok(Progress { committed, next_part, buffered })
}

fn expires(resp: &mut Response, up: &ApiUpload) {
    set(resp, "upload-expires", &s3_proto::time::http_date(up.expires_at));
}

fn lock(api: &Api, id: &str) -> Arc<tokio::sync::Mutex<()>> {
    api.tus_locks.lock().unwrap().entry(id.to_string()).or_default().clone()
}

fn unlock(api: &Api, id: &str) {
    api.tus_locks.lock().unwrap().remove(id);
}

/// Read and drop what's left of a refused request's body (at most [`DRAIN_MAX`]), so a client
/// still sending it (tus-js-client creates with data) reads the answer instead of a reset
/// connection.
async fn drain(body: Body) {
    let mut s = body.into_data_stream();
    let mut n = 0usize;
    while let Some(Ok(c)) = s.next().await {
        n += c.len();
        if n > DRAIN_MAX {
            return;
        }
    }
}

/// Most bytes of a refused request's body read before answering.
const DRAIN_MAX: usize = 64 << 20;

async fn create(r: &Req, signed: bool, body: Body) -> ApiResult<Response> {
    let with_body = r.h("content-type").is_some_and(|c| c.trim().eq_ignore_ascii_case(OFFSET_TYPE));
    let (mut resp, up, length) = match start(r, signed).await {
        Ok(x) => x,
        Err(e) => {
            if with_body {
                drain(body).await;
            }
            return Err(e);
        }
    };
    if with_body || length == Some(0) {
        let l = lock(&r.api, &up.id);
        let _held = l.lock().await;
        let offset = receive(r, &up, Progress { committed: 0, next_part: 1, buffered: 0 }, if with_body { Some(body) } else { None }).await?;
        set(&mut resp, "upload-offset", &offset.to_string());
    }
    Ok(resp)
}

/// Check and record a new upload: the `201` answer, the upload, its length.
async fn start(r: &Req, signed: bool) -> ApiResult<(Response, ApiUpload, Option<u64>)> {
    let api = &r.api;
    let store = r.store();
    let meta = parse_metadata(r.h("upload-metadata"))?;
    let bucket = meta.get("bucketName").filter(|b| !b.is_empty()).ok_or_else(|| err(400, "InvalidRequest", "Metadata header is required: bucketName"))?.clone();
    let key = meta.get("objectName").filter(|k| !k.is_empty()).ok_or_else(|| err(400, "InvalidRequest", "Metadata header is required: objectName"))?.clone();
    if key.len() > 1024 {
        return Err(ApiError::invalid_key(&key));
    }
    let length = match (r.h("upload-length"), r.h("upload-defer-length")) {
        (Some(l), _) => Some(l.trim().parse::<u64>().map_err(|_| err(400, "InvalidRequest", "Invalid Upload-Length"))?),
        (None, Some("1")) => None,
        _ => return Err(err(400, "InvalidRequest", "Upload-Length or Upload-Defer-Length: 1 is required")),
    };
    let upsert_header = r.h("x-upsert").is_some_and(|v| v.trim().eq_ignore_ascii_case("true"));
    let (w, b) = writer(r, signed, &bucket, &key, upsert_header).await?;
    if length.is_some_and(|l| l > api.cfg.max_resumable) {
        return Err(err(413, "EntityTooLarge", "The object exceeded the maximum allowed size"));
    }
    let content_type = meta.get("contentType").filter(|c| !c.is_empty()).cloned().unwrap_or_else(|| "application/octet-stream".into());
    let limits = store.bucket_limits(&b.name)?;
    limits.check(&key, length, Some(&content_type)).map_err(|e| ApiError::from(e).real())?;
    if !w.upsert && store.get_object(&b.name, &key).is_ok() {
        return Err(ApiError::key_exists().real());
    }
    let cache_control = match meta.get("cacheControl").map(|c| c.trim()) {
        Some(c) if !c.is_empty() && c.bytes().all(|x| x.is_ascii_digit()) => format!("max-age={c}"),
        _ => "no-cache".into(),
    };
    let user_metadata = meta.get("metadata").and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok());
    let headers = ObjectHeaders {
        content_type: Some(content_type),
        cache_control: Some(cache_control),
        content_encoding: meta.get("contentEncoding").filter(|e| !e.is_empty()).cloned(),
        metadata: util::encode_user_metadata(user_metadata.as_ref())?,
        owner: w.owner.clone(),
        ..Default::default()
    };
    let min = store.cfg.min_part_size.max(1);
    let part_size = length.map_or(min, |l| min.max(l.div_ceil(10_000)));
    let expires_at = store.now() + api.cfg.resumable_expiry.as_millis() as i64;
    let info = ApiUploadInfo { upsert: w.upsert, metadata: r.h("upload-metadata").map(str::to_string), signed };
    let sse = s3_server::app::default_sse(store, &b);
    store.meter.request(&b.id, Class::A);
    let created = store.create_api_upload(&b.name, &key, headers, sse.as_ref(), NewApiUpload { account: w.account.unwrap_or_else(|| b.account_id.clone()), length, part_size, expires_at, info }).await.map_err(|e| ApiError::from(e).real())?;
    let id = created.id.clone();
    let location = format!("{}/upload/resumable{}/{id}", r.base_url(), if signed { "/sign" } else { "" });
    let up = store.api_upload(&id)?.ok_or_else(ApiError::internal)?;
    let mut resp = empty(StatusCode::CREATED);
    set(&mut resp, "location", &location);
    expires(&mut resp, &up);
    Ok((resp, up, length))
}

async fn head(r: &Req, signed: bool, id: &str) -> ApiResult<Response> {
    let up = load(r, signed, id, false).await?;
    let p = progress(&r.api, &up).await?;
    let mut resp = empty(StatusCode::OK);
    set(&mut resp, "upload-offset", &p.offset().to_string());
    match up.length {
        Some(l) => set(&mut resp, "upload-length", &l.to_string()),
        None => set(&mut resp, "upload-defer-length", "1"),
    }
    if let Some(m) = &up.info.metadata {
        set(&mut resp, "upload-metadata", m);
    }
    set(&mut resp, "cache-control", "no-store");
    expires(&mut resp, &up);
    Ok(resp)
}

async fn patch(r: &Req, signed: bool, id: &str, body: Body) -> ApiResult<Response> {
    let l = lock(&r.api, id);
    let held = l.lock().await;
    match check_patch(r, signed, id).await {
        Ok((up, p)) => {
            let new_offset = receive(r, &up, p, Some(body)).await?;
            drop(held);
            let mut resp = empty(StatusCode::NO_CONTENT);
            set(&mut resp, "upload-offset", &new_offset.to_string());
            expires(&mut resp, &up);
            Ok(resp)
        }
        Err(e) => {
            drop(held);
            drain(body).await;
            Err(e)
        }
    }
}

/// A PATCH may go on: the upload and where it is.
async fn check_patch(r: &Req, signed: bool, id: &str) -> ApiResult<(ApiUpload, Progress)> {
    if !r.h("content-type").is_some_and(|c| c.trim().eq_ignore_ascii_case(OFFSET_TYPE)) {
        return Err(err(415, "InvalidContentType", format!("Content-Type must be {OFFSET_TYPE}")));
    }
    let offset: u64 = r.h("upload-offset").and_then(|o| o.trim().parse().ok()).ok_or_else(|| err(400, "InvalidRequest", "Upload-Offset is required"))?;
    let mut up = load(r, signed, id, true).await?;
    if up.expires_at < r.store().now() {
        let _ = r.store().abort_upload(id).await;
        drop_buffers(&r.api, id).await;
        return Err(err(410, "Gone", "The upload has expired"));
    }
    let p = progress(&r.api, &up).await?;
    if offset != p.offset() {
        return Err(err(409, "Conflict", format!("Upload-Offset {offset} doesn't match the upload's offset {}", p.offset())));
    }
    if up.length.is_none()
        && let Some(l) = r.h("upload-length")
    {
        let l: u64 = l.trim().parse().map_err(|_| err(400, "InvalidRequest", "Invalid Upload-Length"))?;
        if l < p.offset() || l > r.api.cfg.max_resumable {
            return Err(err(400, "InvalidRequest", "Invalid Upload-Length"));
        }
        r.store().bucket_limits(&up.bucket)?.check_size(&up.upload.key, l).map_err(|e| ApiError::from(e).real())?;
        r.store().set_api_upload_length(id, l).await?;
        up.length = Some(l);
    }
    Ok((up, p))
}

/// Take `body`'s bytes into the upload from `p` on (parts as they fill up), and complete the
/// upload once it has all its bytes. Returns the new offset.
async fn receive(r: &Req, up: &ApiUpload, mut p: Progress, body: Option<Body>) -> ApiResult<u64> {
    let api = &r.api;
    let store = r.store();
    let limits = store.bucket_limits(&up.bucket)?;
    let max = up.length.unwrap_or(api.cfg.max_resumable).min(limits.file_size_limit.unwrap_or(u64::MAX));
    let mut received = 0u64;
    let result: ApiResult<()> = async {
        let Some(body) = body else { return Ok(()) };
        let open = |n: u32| {
            let path = buffer_path(api, &up.id, n);
            async move { tokio::fs::OpenOptions::new().create(true).append(true).open(path).await.map_err(|e| {
                tracing::error!(error = %e, "opening a resumable upload's buffer failed");
                ApiError::internal()
            }) }
        };
        let mut file = open(p.next_part).await?;
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else { break };
            if p.offset() + chunk.len() as u64 > max {
                return Err(if up.length.is_some_and(|l| p.offset() + chunk.len() as u64 > l) { err(400, "InvalidRequest", "The body is longer than Upload-Length") } else { err(413, "EntityTooLarge", "The object exceeded the maximum allowed size") });
            }
            let mut rest: &[u8] = &chunk;
            while !rest.is_empty() {
                let room = (up.part_size - p.buffered) as usize;
                let take = room.min(rest.len());
                file.write_all(&rest[..take]).await.map_err(|_| ApiError::internal())?;
                p.buffered += take as u64;
                received += take as u64;
                rest = &rest[take..];
                if p.buffered == up.part_size && up.length.is_none_or(|l| p.offset() < l) {
                    file.sync_data().await.map_err(|_| ApiError::internal())?;
                    drop(file);
                    flush(api, up, p.next_part).await?;
                    p.committed += p.buffered;
                    p.buffered = 0;
                    p.next_part += 1;
                    file = open(p.next_part).await?;
                }
            }
        }
        file.sync_data().await.map_err(|_| ApiError::internal())?;
        Ok(())
    }
    .await;
    if received > 0 {
        store.meter.request(&up.upload.bucket_id, Class::A);
        store.meter.bytes_in(&up.upload.bucket_id, received);
    }
    result?;
    if up.length == Some(p.offset()) {
        finish(r, up, &p).await?;
    }
    Ok(p.offset())
}

/// Upload part `n` from its buffer file and record it.
async fn flush(api: &Api, up: &ApiUpload, n: u32) -> ApiResult<()> {
    let path = buffer_path(api, &up.id, n);
    let io = |_| ApiError::internal();
    let mut f = tokio::fs::File::open(&path).await.map_err(io)?;
    let mut md5 = Md5::new();
    let mut buf = vec![0u8; 64 << 10];
    let mut len = 0u64;
    loop {
        let k = f.read(&mut buf).await.map_err(io)?;
        if k == 0 {
            break;
        }
        md5.update(&buf[..k]);
        len += k as u64;
    }
    let etag = hex::encode(md5.finalize());
    let store = api.store();
    let file = tokio::fs::File::open(&path).await.map_err(io)?;
    let data = store.put_part_data(&up.upload, n, util::file_stream(file), len).await?;
    store.commit_part(&up.upload, n, &data, &etag, None).await?;
    let _ = tokio::fs::remove_file(&path).await;
    Ok(())
}

/// All the bytes are in: the last part, then the object.
async fn finish(r: &Req, up: &ApiUpload, p: &Progress) -> ApiResult<()> {
    let api = &r.api;
    let store = r.store();
    let cond = if up.info.upsert { WriteCondition::none() } else { WriteCondition { if_none_match: Some("*".into()), ..Default::default() } };
    let refused = |e: StoreError| ApiError::from(e).real();
    if up.length == Some(0) {
        // Nothing to upload in parts: an empty object, written like any PUT.
        store.abort_upload(&up.id).await?;
        drop_buffers(api, &up.id).await;
        unlock(api, &up.id);
        let b = store.bucket(&up.bucket)?;
        s3_server::put_object(store, &b, &up.upload.key, up.upload.headers.clone(), s3_store::bytes_body(bytes::Bytes::new()), 0, &cond).await.map_err(refused)?;
        return Ok(());
    }
    if p.buffered > 0 {
        flush(api, up, p.next_part).await?;
    }
    let (parts, _) = store.list_parts(&up.bucket, &up.id, 0, 10_000)?;
    let list: Vec<(u32, String)> = parts.iter().map(|p| (p.part_number, p.etag.clone())).collect();
    let result = store.complete_upload(&up.bucket, &up.id, &list, None, &cond).await;
    store.release_blob(&up.upload.blob_id);
    match result {
        Ok(_) => {}
        Err(StoreError::PreconditionFailed) => {
            let _ = store.abort_upload(&up.id).await;
            drop_buffers(api, &up.id).await;
            unlock(api, &up.id);
            return Err(ApiError::key_exists().real());
        }
        Err(e) => {
            if matches!(e, StoreError::Refused { .. }) {
                drop_buffers(api, &up.id).await;
            }
            return Err(refused(e));
        }
    }
    drop_buffers(api, &up.id).await;
    unlock(api, &up.id);
    Ok(())
}

async fn terminate(r: &Req, signed: bool, id: &str) -> ApiResult<Response> {
    let l = lock(&r.api, id);
    let _held = l.lock().await;
    load(r, signed, id, true).await?;
    r.store().abort_upload(id).await?;
    drop_buffers(&r.api, id).await;
    unlock(&r.api, id);
    Ok(empty(StatusCode::NO_CONTENT))
}

/// Abort expired uploads.
pub(crate) async fn sweep(api: &Api) -> usize {
    let store = api.store();
    let ids = match store.expired_api_uploads(store.now(), 1000) {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = %e, "listing expired resumable uploads failed");
            return 0;
        }
    };
    let mut n = 0;
    for id in ids {
        let l = lock(api, &id);
        let _held = l.lock().await;
        match store.abort_upload(&id).await {
            Ok(()) | Err(StoreError::NoSuchUpload(_)) => {
                drop_buffers(api, &id).await;
                n += 1;
            }
            Err(e) => tracing::warn!(error = %e, upload = id, "aborting an expired resumable upload failed"),
        }
        unlock(api, &id);
    }
    n
}
