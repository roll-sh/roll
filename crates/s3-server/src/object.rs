//! Object operations: PUT, GET, HEAD, DELETE, copy, tagging (and routing to multipart).

use crate::auth::Need;
use crate::body::{Incoming, requested_checksum};
use crate::ctx::{Ctx, read_small};
use crate::error::{StoreResult, store_error};
use crate::multipart;
use crate::response::{self, Reply, set};
use crate::sse;
use crate::versioning;
use axum::body::Body;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures::Stream;
use s3_proto::checksum::Algorithm;
use s3_proto::http::{Conditions, Overrides, parse_copy_source_version, parse_range, parse_tagging_header, part_range, quote_etag, user_metadata, validate_key, validate_tags};
use s3_proto::time::http_date;
use s3_proto::{Code, S3Error, xml};
use s3_store::{Bucket, ByteStream, Checksum, Class, CopyOptions, Encryption, NewObject, ObjectHeaders, ObjectMeta, Store, WriteCondition};
use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// Largest single PUT (S3's limit).
pub(crate) const MAX_PUT: u64 = 5 << 30;

/// Subresources S3 has that we don't implement.
const UNSUPPORTED: [&str; 3] = ["torrent", "select", "renameObject"];

pub(crate) async fn route(ctx: &mut Ctx, body: Body) -> Reply {
    if let Some(sub) = UNSUPPORTED.iter().find(|s| ctx.has(s)) {
        return Err(S3Error::msg(Code::NotImplemented, format!("?{sub} is not supported")));
    }
    versioning::version_id(ctx)?;
    validate_key(ctx.key_name())?;
    // A key that can't see the bucket learns nothing about it (a policy may still let it in).
    let bucket = match ctx.store().bucket(ctx.bucket_name()) {
        Ok(b) => b,
        Err(e) => {
            return Err(match &ctx.auth {
                Some(a) if !a.key.covers(ctx.bucket_name()) => S3Error::new(Code::AccessDenied),
                _ => store_error(e),
            });
        }
    };
    match ctx.method.clone() {
        Method::GET if ctx.has("uploadId") => multipart::list_parts(ctx, &bucket),
        Method::GET if ctx.has("tagging") => get_tagging(ctx, &bucket),
        Method::GET if ctx.has("retention") => versioning::get_retention(ctx, &bucket),
        Method::GET if ctx.has("legal-hold") => versioning::get_legal_hold(ctx, &bucket),
        Method::GET if ctx.has("acl") => crate::acl::get_object_acl(ctx, &bucket),
        Method::GET if ctx.has("attributes") => crate::attributes::get_attributes(ctx, &bucket),
        Method::GET => get(ctx, &bucket, false).await,
        Method::HEAD => get(ctx, &bucket, true).await,
        Method::PUT if ctx.has("uploadId") || ctx.has("partNumber") => multipart::upload_part(ctx, &bucket, body).await,
        Method::PUT if ctx.has("tagging") => put_tagging(ctx, &bucket, body).await,
        Method::PUT if ctx.has("retention") => versioning::put_retention(ctx, &bucket, body).await,
        Method::PUT if ctx.has("legal-hold") => versioning::put_legal_hold(ctx, &bucket, body).await,
        Method::PUT if ctx.has("acl") => crate::acl::put_object_acl(ctx, &bucket, body).await,
        Method::PUT if ctx.h("x-amz-copy-source").is_some() => copy(ctx, &bucket).await,
        Method::PUT => put(ctx, &bucket, body).await,
        Method::POST if ctx.has("uploads") => multipart::create(ctx, &bucket).await,
        Method::POST if ctx.has("uploadId") => multipart::complete(ctx, &bucket, body).await,
        Method::POST if ctx.has("restore") => restore(ctx, &bucket),
        Method::DELETE if ctx.has("uploadId") => multipart::abort(ctx, &bucket).await,
        Method::DELETE if ctx.has("tagging") => delete_tagging(ctx, &bucket).await,
        Method::DELETE => delete(ctx, &bucket).await,
        _ => Err(S3Error::new(Code::MethodNotAllowed)),
    }
}

/// `Content-Encoding` without the transport-only `aws-chunked`.
fn content_encoding(ctx: &Ctx) -> Option<String> {
    let v = ctx.h("content-encoding")?;
    let kept: Vec<&str> = v.split(',').map(str::trim).filter(|e| !e.is_empty() && !e.eq_ignore_ascii_case("aws-chunked")).collect();
    (!kept.is_empty()).then(|| kept.join(","))
}

/// A write's ACL (`x-amz-acl`, `x-amz-grant-*`; `None` = private) and website redirect.
pub(crate) fn write_extras(ctx: &Ctx) -> Result<(Option<String>, Option<String>), S3Error> {
    Ok((crate::acl::for_new_object(ctx, ctx.bucket_name())?, crate::website::redirect_location(ctx)?))
}

/// The standard headers, user metadata, `x-amz-tagging`, ACL and website redirect of a write.
pub(crate) fn object_headers(ctx: &Ctx) -> Result<ObjectHeaders, S3Error> {
    let (acl, website_redirect) = write_extras(ctx)?;
    let pairs = ctx.header_pairs();
    let metadata = user_metadata(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))?;
    let tags = match ctx.h("x-amz-tagging") {
        Some(t) => parse_tagging_header(t)?.into_iter().collect(),
        None => BTreeMap::new(),
    };
    let s = |n: &str| ctx.h(n).map(str::to_string);
    Ok(ObjectHeaders {
        content_type: s("content-type"),
        cache_control: s("cache-control"),
        content_disposition: s("content-disposition"),
        content_encoding: content_encoding(ctx),
        content_language: s("content-language"),
        expires: s("expires"),
        metadata,
        tags,
        acl,
        website_redirect,
    })
}

/// `If-Match` / `If-None-Match: *` on a write.
pub(crate) fn write_condition(ctx: &Ctx) -> Result<WriteCondition, S3Error> {
    let c = Conditions::from_headers(|n| ctx.h(n));
    c.check_write(None).or_else(|e| if e.code == Code::NotImplemented { Err(e) } else { Ok(()) })?;
    Ok(WriteCondition { if_match: c.if_match, if_none_match: c.if_none_match })
}

/// Fail a conditional write before uploading anything (it's checked again atomically at
/// commit).
pub(crate) fn precheck(ctx: &Ctx, cond: &WriteCondition) -> Result<(), S3Error> {
    if cond.if_match.is_none() && cond.if_none_match.is_none() {
        return Ok(());
    }
    let current = match ctx.store().get_object(ctx.bucket_name(), ctx.key_name()) {
        Ok(m) => Some(m.etag),
        Err(s3_store::StoreError::NoSuchKey(_)) => None,
        Err(e) => return Err(store_error(e)),
    };
    let c = Conditions { if_match: cond.if_match.clone(), if_none_match: cond.if_none_match.clone(), ..Default::default() };
    c.check_write(current.as_deref())
}

pub(crate) fn checksum_of(c: &Checksum) -> Option<Algorithm> {
    Algorithm::parse(&c.algorithm)
}

/// The response headers of a stored checksum.
pub(crate) fn set_checksum(r: &mut Response, c: &Checksum) {
    if let Some(a) = checksum_of(c) {
        set(r, a.header(), &c.value);
        set(r, "x-amz-checksum-type", &c.kind);
    }
}

async fn put(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    ctx.require(&bucket.name, Need::Write)?;
    let headers = object_headers(ctx)?;
    let cond = write_condition(ctx)?;
    let lock = versioning::lock_headers(ctx, bucket)?;
    if !lock.is_empty() {
        versioning::require_integrity(ctx)?;
    }
    // Every object gets a checksum: the client's, or CRC64NVME (as S3 does by default).
    let want = requested_checksum(ctx)?.or(Some((Algorithm::Crc64Nvme, None)));
    let target = sse::target(ctx, bucket)?;
    let incoming = Incoming::new(ctx, body, want, None)?;
    let len = incoming.len().ok_or_else(|| S3Error::new(Code::MissingContentLength))?;
    if len > MAX_PUT {
        return Err(S3Error::new(Code::EntityTooLarge).with("ProposedSize", len.to_string()).with("MaxSizeAllowed", MAX_PUT.to_string()));
    }
    ctx.meter(&bucket.id, Class::A);
    precheck(ctx, &cond)?;
    let store = ctx.store().clone();
    let key = ctx.key_name().to_string();
    let result = incoming.result();
    let guard = InflightBlob::new(&store);
    let blob = guard.id.clone();
    let uploaded = store.put_blob_sse(&blob, &bucket.id, &key, incoming.into_stream(), len, headers.content_type.as_deref(), target.sse.as_ref()).await;
    let (up, finished) = match (uploaded, result.get()) {
        (Ok(u), Ok(f)) => (u, f),
        (Err(e), body) => return Err(body.err().unwrap_or_else(|| store_error(e))),
        (Ok(_), Err(e)) => {
            store.discard_blob(&blob).await;
            return Err(e);
        }
    };
    store.meter.bytes_in(&bucket.id, len);
    let checksum = finished.checksum.map(|(a, v)| Checksum { algorithm: a.as_str().into(), kind: "FULL_OBJECT".into(), value: v });
    let enc = target.encryption();
    let obj = NewObject { blob_id: blob, size: up.size, crc32c: up.crc32c, etag: sse::etag(enc.as_ref(), finished.digest.etag()), headers, checksum };
    let meta = store.commit_put_full(&bucket.name, &key, obj, &cond, &lock, enc.as_ref()).await.s3()?;
    drop(guard);
    let mut r = response::empty(StatusCode::OK);
    set(&mut r, "etag", &quote_etag(&meta.etag));
    versioning::set_version(&mut r, bucket, &meta.version_id, false);
    if let Some(c) = &meta.checksum {
        set_checksum(&mut r, c);
    }
    sse::set_headers(&mut r, enc.as_ref(), target.s3 || ctx.store().sse_s3_at_rest());
    Ok(r)
}

/// Response headers shared by GET and HEAD.
fn object_response(meta: &ObjectMeta, overrides: &Overrides, code: StatusCode, len: u64) -> Response {
    let mut r = response::empty(code);
    let h = &meta.headers;
    let pick = |o: &Option<String>, stored: &Option<String>| o.clone().or_else(|| stored.clone());
    set(&mut r, "content-type", pick(&overrides.content_type, &h.content_type).as_deref().unwrap_or("binary/octet-stream"));
    for (name, v) in [
        ("cache-control", pick(&overrides.cache_control, &h.cache_control)),
        ("content-disposition", pick(&overrides.content_disposition, &h.content_disposition)),
        ("content-encoding", pick(&overrides.content_encoding, &h.content_encoding)),
        ("content-language", pick(&overrides.content_language, &h.content_language)),
        ("expires", pick(&overrides.expires, &h.expires)),
    ] {
        if let Some(v) = v {
            set(&mut r, name, &v);
        }
    }
    for (k, v) in &h.metadata {
        set(&mut r, &format!("x-amz-meta-{k}"), v);
    }
    if !h.tags.is_empty() {
        set(&mut r, "x-amz-tagging-count", &h.tags.len().to_string());
    }
    if let Some(loc) = &h.website_redirect {
        set(&mut r, "x-amz-website-redirect-location", loc);
    }
    set(&mut r, "etag", &quote_etag(&meta.etag));
    set(&mut r, "last-modified", &http_date(meta.modified_at));
    set(&mut r, "accept-ranges", "bytes");
    set(&mut r, "content-length", &len.to_string());
    r
}

/// Releases an in-flight blob id however the request ends, including a client disconnect that
/// drops the handler mid-upload. Releasing never deletes anything: an uncommitted blob is left to
/// the orphan scan, which checks the metadata first, so a commit that did land is never undone.
pub(crate) struct InflightBlob {
    store: Arc<Store>,
    pub id: String,
}

impl InflightBlob {
    pub fn new(store: &Arc<Store>) -> Self {
        Self { id: store.new_blob(), store: store.clone() }
    }

    /// Guard an id the store registered itself (a multipart upload's blob while completing).
    pub fn adopt(store: &Arc<Store>, id: String) -> Self {
        Self { id, store: store.clone() }
    }
}

impl Drop for InflightBlob {
    fn drop(&mut self) {
        self.store.release_blob(&self.id);
    }
}

/// Counts the bytes handed to the client, metered as egress when the response is done.
struct Metered {
    inner: ByteStream,
    store: Arc<Store>,
    bucket_id: String,
    internal: bool,
    sent: u64,
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

pub(crate) async fn get(ctx: &mut Ctx, bucket: &Bucket, head: bool) -> Reply {
    ctx.require_object_read(bucket)?;
    let vid = versioning::version_id(ctx)?;
    let (meta, enc) = ctx.store().get_version_encryption(&bucket.name, ctx.key_name(), vid.as_deref()).s3()?;
    ctx.meter(&bucket.id, Class::B);
    let key = sse::key_for(ctx, enc.as_ref(), sse::Use::Read)?;
    let overrides = Overrides::from_query(ctx.query.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    if !overrides.is_empty() && ctx.auth.is_none() {
        return Err(S3Error::msg(Code::InvalidRequest, "Request specific response headers cannot be used for anonymous GET requests."));
    }
    if let Err(e) = Conditions::from_headers(|n| ctx.h(n)).check_read(&meta.etag, meta.modified_at) {
        if e.code == Code::NotModified {
            let mut r = response::empty(StatusCode::NOT_MODIFIED);
            set(&mut r, "etag", &quote_etag(&meta.etag));
            set(&mut r, "last-modified", &http_date(meta.modified_at));
            versioning::set_version(&mut r, bucket, &meta.version_id, vid.is_some());
            return Ok(r);
        }
        return Err(e);
    }
    let part = match ctx.q("partNumber") {
        Some(p) => Some(p.parse::<u32>().ok().filter(|n| (1..=10_000).contains(n)).ok_or_else(|| S3Error::msg(Code::InvalidArgument, "Part number must be an integer between 1 and 10000, inclusive").with("ArgumentName", "partNumber"))?),
        None => None,
    };
    let range = match part {
        Some(n) => {
            if ctx.h("range").is_some() {
                return Err(S3Error::msg(Code::InvalidRequest, "Cannot specify both Range header and partNumber query parameter"));
            }
            let bad = || S3Error::msg(Code::InvalidRange, "The requested partnumber is not satisfiable").with("PartNumberRequested", n.to_string()).with("ActualPartCount", meta.parts.max(1).to_string());
            if meta.parts == 0 {
                if n != 1 {
                    return Err(bad());
                }
                None
            } else {
                Some(part_range(&meta.part_sizes, n).ok_or_else(bad)?)
            }
        }
        None => parse_range(ctx.h("range"), meta.size)?,
    };
    let (code, len) = match range {
        Some((a, b)) => (StatusCode::PARTIAL_CONTENT, b - a + 1),
        None => (StatusCode::OK, meta.size),
    };
    let mut r = object_response(&meta, &overrides, code, len);
    versioning::set_version(&mut r, bucket, &meta.version_id, vid.is_some());
    versioning::set_lock(&mut r, &meta.lock);
    sse::set_headers(&mut r, enc.as_ref(), ctx.store().sse_s3_at_rest());
    if let Some((a, b)) = range {
        set(&mut r, "content-range", &s3_proto::http::content_range(a, b, meta.size));
    }
    if part.is_some() && meta.parts > 0 {
        set(&mut r, "x-amz-mp-parts-count", &meta.parts.to_string());
    }
    let whole = range.is_none() || (part.is_some() && meta.parts == 0);
    if whole
        && ctx.h("x-amz-checksum-mode").is_some_and(|m| m.eq_ignore_ascii_case("ENABLED"))
        && let Some(c) = &meta.checksum
    {
        set_checksum(&mut r, c);
    }
    if head || len == 0 {
        return Ok(r);
    }
    let (_, stream) = ctx.store().open_blob_sse(&meta, range.map(|(a, b)| a..b + 1), key.as_ref()).await.s3()?;
    let metered = Metered { inner: stream, store: ctx.store().clone(), bucket_id: bucket.id.clone(), internal: ctx.internal(), sent: 0 };
    let headers = std::mem::take(r.headers_mut());
    let mut out = Response::new(Body::from_stream(metered));
    *out.status_mut() = code;
    *out.headers_mut() = headers;
    Ok(out)
}

async fn delete(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Write)?;
    ctx.meter(&bucket.id, Class::Free);
    let if_match = ctx.h("if-match").map(str::to_string);
    let vid = versioning::version_id(ctx)?;
    let bypass = versioning::bypass(ctx)?;
    let o = ctx.store().delete_object_version(&bucket.name, ctx.key_name(), vid.as_deref(), if_match.as_deref(), bypass).await.s3()?;
    let mut r = response::empty(StatusCode::NO_CONTENT);
    if o.delete_marker {
        set(&mut r, "x-amz-delete-marker", "true");
    }
    if let Some(v) = &o.version_id {
        versioning::set_version(&mut r, bucket, v, vid.is_some());
    }
    Ok(r)
}

fn directive(ctx: &Ctx, name: &str) -> Result<bool, S3Error> {
    match ctx.h(name).map(str::trim) {
        None | Some("COPY") => Ok(false),
        Some("REPLACE") => Ok(true),
        Some(v) => Err(S3Error::msg(Code::InvalidArgument, format!("Unknown {name} value: {v}")).with("ArgumentName", name.to_string())),
    }
}

async fn copy(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Write)?;
    let (src_bucket, src_key, src_vid) = parse_copy_source_version(ctx.h("x-amz-copy-source").unwrap_or_default())?;
    ctx.require_source(&src_bucket, &src_key)?;
    let src = copy_source(ctx, &src_bucket, &src_key, src_vid.as_deref())?;
    let src_enc = ctx.store().blob_encryption(&src.blob_id).s3()?;
    let src_sse = sse::key_for(ctx, src_enc.as_ref(), sse::Use::CopySource)?;
    Conditions::copy_source_from_headers(|n| ctx.h(n)).check_copy_source(&src.etag, src.modified_at)?;
    let replace = directive(ctx, "x-amz-metadata-directive")?;
    let replace_tags = directive(ctx, "x-amz-tagging-directive")?;
    let lock = versioning::lock_headers(ctx, bucket)?;
    // A copy is encrypted as its own request says (or the bucket's default), never as its
    // source was: S3 doesn't carry encryption over.
    let target = sse::target(ctx, bucket)?;
    let encrypting = target.sse.is_some() || target.s3;
    // Copying an older version onto its own key is how S3 restores one.
    if src_bucket == bucket.name && src_key == ctx.key_name() && src_vid.is_none() && !replace && !replace_tags && ctx.h("x-amz-website-redirect-location").is_none() && !encrypting {
        return Err(S3Error::msg(
            Code::InvalidRequest,
            "This copy request is illegal because it is trying to copy an object to itself without changing the object's metadata, storage class, website redirect location or encryption attributes.",
        ));
    }
    // As on S3, a copy never takes the source's ACL or website redirect: they come from the
    // request (private and none by default), whatever the metadata directive.
    let headers = if replace {
        object_headers(ctx)?
    } else {
        let (acl, website_redirect) = write_extras(ctx)?;
        ObjectHeaders { acl, website_redirect, ..src.headers.clone() }
    };
    let headers = Some(headers);
    let tags = if replace_tags {
        let t = ctx.h("x-amz-tagging").map(parse_tagging_header).transpose()?.unwrap_or_default();
        Some(t.into_iter().collect())
    } else {
        None
    };
    let condition = write_condition(ctx)?;
    ctx.meter(&bucket.id, Class::A);
    // The exact version checked above is the one copied.
    let opts = CopyOptions { headers, tags, source_etag: Some(src.etag.clone()), condition, source_version: Some(src.version_id.clone()), lock };
    // The same encryption shares the blob (free); another one needs a new blob, copied inside
    // object storage.
    let meta = if Encryption::same(src_enc.as_ref(), target.sse.as_ref()) {
        ctx.store().copy_object(&src_bucket, &src_key, &bucket.name, ctx.key_name(), &opts).await.s3()?
    } else {
        ctx.store().copy_object_new_blob(&src_bucket, &src_key, &bucket.name, ctx.key_name(), &opts, src_sse.as_ref(), target.sse.as_ref()).await.s3()?
    };
    let checksum = meta.checksum.as_ref().and_then(|c| checksum_of(c).map(|a| (a, c.value.as_str())));
    let mut r = response::xml(StatusCode::OK, xml::copy_object_result(&meta.etag, meta.modified_at, checksum));
    if let Some(c) = &meta.checksum {
        set_checksum(&mut r, c);
    }
    versioning::set_version(&mut r, bucket, &meta.version_id, false);
    let src_versioned = src_vid.is_some() || ctx.store().bucket(&src_bucket).is_ok_and(|b| b.versioning.is_some());
    if src_versioned {
        set(&mut r, "x-amz-copy-source-version-id", &src.version_id);
    }
    sse::set_headers(&mut r, target.encryption().as_ref(), target.s3 || ctx.store().sse_s3_at_rest());
    Ok(r)
}

/// A copy's source version (`None`: the current one), as S3 reports a delete marker there.
pub(crate) fn copy_source(ctx: &Ctx, bucket: &str, key: &str, version_id: Option<&str>) -> Result<ObjectMeta, S3Error> {
    match ctx.store().get_object_version(bucket, key, version_id) {
        Err(s3_store::StoreError::DeleteMarker { specific: true, .. }) => Err(S3Error::msg(Code::InvalidRequest, "The source of a copy request may not specifically refer to a delete marker by version id.")),
        Err(s3_store::StoreError::DeleteMarker { .. }) => Err(S3Error::new(Code::NoSuchKey).with("Key", key.to_string())),
        r => r.s3(),
    }
}

fn get_tagging(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Read)?;
    ctx.meter(&bucket.id, Class::B);
    let vid = versioning::version_id(ctx)?;
    let (version, tags) = ctx.store().get_version_tags(&bucket.name, ctx.key_name(), vid.as_deref()).s3()?;
    let tags: Vec<(String, String)> = tags.into_iter().collect();
    let mut r = response::xml(StatusCode::OK, xml::tagging(&tags));
    versioning::set_version(&mut r, bucket, &version, vid.is_some());
    Ok(r)
}

async fn put_tagging(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    ctx.require(&bucket.name, Need::Write)?;
    ctx.meter(&bucket.id, Class::A);
    let vid = versioning::version_id(ctx)?;
    let raw = read_small(ctx, body, xml::MAX_XML_BYTES).await?;
    let tags = xml::parse_tagging(&raw)?;
    validate_tags(&tags)?;
    let m = ctx.store().put_version_tags(&bucket.name, ctx.key_name(), vid.as_deref(), tags.into_iter().collect()).await.s3()?;
    let mut r = response::empty(StatusCode::OK);
    versioning::set_version(&mut r, bucket, &m.version_id, vid.is_some());
    Ok(r)
}

async fn delete_tagging(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Write)?;
    ctx.meter(&bucket.id, Class::Free);
    let vid = versioning::version_id(ctx)?;
    let m = ctx.store().put_version_tags(&bucket.name, ctx.key_name(), vid.as_deref(), BTreeMap::new()).await.s3()?;
    let mut r = response::empty(StatusCode::NO_CONTENT);
    versioning::set_version(&mut r, bucket, &m.version_id, vid.is_some());
    Ok(r)
}

/// `RestoreObject`: nothing is ever archived, so there's nothing to restore (S3's answer for an
/// object that isn't in an archive storage class).
fn restore(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Write)?;
    ctx.meter(&bucket.id, Class::A);
    ctx.store().get_object(&bucket.name, ctx.key_name()).s3()?;
    Err(S3Error::msg(Code::InvalidObjectState, "Restore is not allowed for the object's current storage class").with("StorageClass", "STANDARD"))
}
