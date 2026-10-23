//! Shared pieces: JSON in and out, Supabase's object shapes, user metadata, streams.

use crate::error::{ApiError, ApiResult};
use axum::body::Body;
use axum::http::{HeaderValue, header};
use axum::response::Response;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use s3_store::{Bucket, ByteStream, ObjectMeta, Store};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// Largest JSON request body.
pub(crate) const MAX_JSON: usize = 1 << 20;

/// The S3 user-metadata entry an object's Supabase `user_metadata` (any JSON object) is kept in.
pub(crate) const USER_METADATA: &str = "user-metadata";

pub(crate) fn iso(ms: i64) -> String {
    s3_proto::time::iso8601_ms(ms)
}

pub(crate) fn json_response(v: &Value) -> Response {
    let mut r = Response::new(Body::from(v.to_string()));
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    r
}

pub(crate) fn message(m: &str) -> Response {
    json_response(&json!({ "message": m }))
}

/// A JSON body (at most [`MAX_JSON`] bytes). An empty body reads as `{}`.
pub(crate) async fn read_json<T: serde::de::DeserializeOwned>(body: Body) -> ApiResult<T> {
    let bytes = axum::body::to_bytes(body, MAX_JSON).await.map_err(|_| ApiError::too_large("request body"))?;
    let bytes: &[u8] = if bytes.iter().all(u8::is_ascii_whitespace) { b"{}" } else { &bytes };
    serde_json::from_slice(bytes).map_err(|e| ApiError::invalid(format!("body is not valid: {e}")))
}

/// A stable id for an object version, shaped like Supabase's UUIDs: a hash of the bucket, the
/// key and the version's time and ETag (every write makes a new one).
pub(crate) fn object_id(bucket_id: &str, m: &ObjectMeta) -> String {
    let mut h = Sha256::new();
    h.update(bucket_id.as_bytes());
    h.update([0]);
    h.update(m.key.as_bytes());
    h.update([0]);
    h.update(m.modified_at.to_be_bytes());
    h.update(m.etag.as_bytes());
    if m.version_id != "null" {
        h.update(m.version_id.as_bytes());
    }
    let d = h.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let x = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &x[..8], &x[8..12], &x[12..16], &x[16..20], &x[20..])
}

/// The object's version as Supabase shows it: roll's version id in a versioned bucket, the
/// object's id otherwise.
pub(crate) fn version_of(bucket: &Bucket, m: &ObjectMeta) -> String {
    if m.version_id != "null" { m.version_id.clone() } else { object_id(&bucket.id, m) }
}

/// JSON text with every non-ASCII character escaped (`\uXXXX`), so it fits an HTTP header.
pub(crate) fn ascii_json(v: &Value) -> String {
    let s = v.to_string();
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for u in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

/// Supabase's `user_metadata` (a JSON object) as S3 user metadata: one entry, the object as
/// JSON text. `null` or absent: none.
pub(crate) fn encode_user_metadata(v: Option<&Value>) -> ApiResult<BTreeMap<String, String>> {
    let mut m = BTreeMap::new();
    match v {
        None | Some(Value::Null) => {}
        Some(o @ Value::Object(_)) => {
            let text = ascii_json(o);
            if text.len() + USER_METADATA.len() > s3_store::validate::MAX_METADATA_BYTES {
                return Err(ApiError::too_large("user_metadata"));
            }
            m.insert(USER_METADATA.to_string(), text);
        }
        Some(_) => return Err(ApiError::invalid("metadata must be a JSON object")),
    }
    Ok(m)
}

/// An object's `user_metadata`: what the developer API stored, else its S3 user metadata as a
/// map of strings (`null` without any).
pub(crate) fn user_metadata(m: &ObjectMeta) -> Value {
    if let Some(v) = m.headers.metadata.get(USER_METADATA).and_then(|t| serde_json::from_str::<Value>(t).ok()).filter(Value::is_object) {
        return v;
    }
    if m.headers.metadata.is_empty() {
        return Value::Null;
    }
    json!(m.headers.metadata)
}

/// `x-metadata`: base64 of a JSON object.
pub(crate) fn header_user_metadata(h: Option<&str>) -> ApiResult<Option<Value>> {
    use base64::Engine;
    let Some(h) = h.map(str::trim).filter(|h| !h.is_empty()) else { return Ok(None) };
    let raw = base64::engine::general_purpose::STANDARD.decode(h).or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(h)).map_err(|_| ApiError::invalid("x-metadata must be base64-encoded JSON"))?;
    let v: Value = serde_json::from_slice(&raw).map_err(|_| ApiError::invalid("x-metadata must be base64-encoded JSON"))?;
    Ok(Some(v))
}

pub(crate) fn content_type(m: &ObjectMeta) -> &str {
    m.headers.content_type.as_deref().filter(|c| !c.is_empty()).unwrap_or("application/octet-stream")
}

pub(crate) fn cache_control(m: &ObjectMeta) -> &str {
    m.headers.cache_control.as_deref().filter(|c| !c.is_empty()).unwrap_or("no-cache")
}

/// Supabase's `metadata` of an object (what its storage backend reported).
pub(crate) fn file_metadata(m: &ObjectMeta) -> Value {
    let mut v = json!({
        "eTag": s3_proto::http::quote_etag(&m.etag),
        "size": m.size,
        "mimetype": content_type(m),
        "cacheControl": cache_control(m),
        "lastModified": iso(m.modified_at),
        "contentLength": m.size,
        "httpStatusCode": 200,
    });
    if let Some(e) = &m.headers.content_encoding {
        v["contentEncoding"] = json!(e);
    }
    v
}

/// An object as Supabase's object rows look (`name` as given: the full key, or relative to a
/// listing's prefix).
pub(crate) fn object_view(bucket: &Bucket, m: &ObjectMeta, name: &str) -> Value {
    let at = iso(m.modified_at);
    json!({
        "name": name,
        "bucket_id": bucket.name,
        "owner": m.headers.owner,
        "owner_id": m.headers.owner,
        "id": object_id(&bucket.id, m),
        "version": version_of(bucket, m),
        "updated_at": at,
        "created_at": at,
        "last_accessed_at": at,
        "metadata": file_metadata(m),
        "user_metadata": user_metadata(m),
    })
}

/// A bucket as Supabase shows it.
pub(crate) fn bucket_view(store: &Store, b: &Bucket) -> Value {
    let limits = store.bucket_limits(&b.name).unwrap_or_default();
    let versioning = match b.versioning.as_deref() {
        Some("Enabled") => "ENABLED",
        Some("Suspended") => "SUSPENDED",
        _ => "DISABLED",
    };
    json!({
        "id": b.name,
        "name": b.name,
        "owner": "",
        "public": b.public,
        "type": "STANDARD",
        "file_size_limit": limits.file_size_limit,
        "allowed_mime_types": limits.allowed_mime_types,
        "versioning_status": versioning,
        "created_at": iso(b.created_at),
        "updated_at": iso(b.created_at),
    })
}

/// `Content-Disposition` for `?download=` (Supabase's: empty means `attachment;`).
pub(crate) fn content_disposition(download: &str) -> String {
    if download.is_empty() {
        return "attachment;".into();
    }
    let fallback: String = download.chars().map(|c| if c.is_ascii_graphic() && !matches!(c, '"' | '\\' | ';' | ',') || c == ' ' { c } else { '_' }).collect();
    format!("attachment; filename={fallback}; filename*=UTF-8''{}", s3_proto::http::uri_encode(download, true))
}

/// Counts bytes handed to the client, metered as egress when the response ends.
pub(crate) struct Metered {
    pub inner: ByteStream,
    pub store: Arc<Store>,
    pub bucket_id: String,
    pub internal: bool,
    pub sent: u64,
}

impl Stream for Metered {
    type Item = std::io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let p = self.inner.as_mut().poll_next(cx);
        if let Poll::Ready(Some(Ok(b))) = &p {
            self.sent += b.len() as u64;
        }
        p
    }
}

impl Drop for Metered {
    fn drop(&mut self) {
        self.store.meter.egress(&self.bucket_id, self.sent, self.internal);
    }
}

/// A file's bytes as a stream (64 KiB reads).
pub(crate) fn file_stream(file: tokio::fs::File) -> ByteStream {
    use tokio::io::AsyncReadExt;
    futures::stream::unfold(Some(file), |f| async move {
        let mut f = f?;
        let mut buf = vec![0u8; 64 << 10];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(Bytes::from(buf)), Some(f)))
            }
            Err(e) => Some((Err(e), None)),
        }
    })
    .boxed()
}

/// A request body as the store's byte stream.
pub(crate) fn body_stream(body: Body) -> ByteStream {
    body.into_data_stream().map(|r| r.map_err(std::io::Error::other)).boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_and_metadata() {
        assert_eq!(ascii_json(&json!({"a": "\u{e9}\u{1f600}"})), "{\"a\":\"\\u00e9\\ud83d\\ude00\"}");
        let m = encode_user_metadata(Some(&json!({"userId": "x", "n": 1}))).unwrap();
        let text = m.get(USER_METADATA).unwrap();
        assert_eq!(serde_json::from_str::<Value>(text).unwrap(), json!({"userId": "x", "n": 1}));
        assert!(encode_user_metadata(Some(&json!([1]))).is_err());
        assert!(encode_user_metadata(Some(&json!({"big": "x".repeat(3000)}))).is_err());
        assert_eq!(content_disposition(""), "attachment;");
        assert_eq!(content_disposition("a b.txt"), "attachment; filename=a b.txt; filename*=UTF-8''a%20b.txt");
    }
}
