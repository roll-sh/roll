//! Objects: uploads, downloads, info, lists, moves, copies and deletes (Supabase's
//! `/object/...` routes).

use crate::auth;
use crate::error::{ApiError, ApiResult};
use crate::req::{Req, bucket_and_key, object_path};
use crate::util::{self, Metered, json_response, message, object_view};
use axum::body::Body;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use s3_proto::{Code, S3Error};
use s3_server::app::{AppCaller, Checker};
use s3_store::{Bucket, Class, CopyOptions, ListQuery, ObjectHeaders, ObjectMeta, StoreError, WriteCondition};
use serde::Deserialize;
use serde_json::{Value, json};

/// How a read route authenticates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    /// `/object/public/...`: no key; the bucket's public flag or policy must allow anyone.
    Public,
    /// `/object/authenticated/...`: a key is required.
    Authenticated,
    /// `/object/...`: a key if one is sent, else anonymous.
    Optional,
}

/// Most objects one delete takes (Supabase's).
const MAX_DELETE: usize = 1000;
/// Most entries a listing sorts by something other than name.
const MAX_SORT_SCAN: usize = 10_000;

async fn caller_for(r: &Req, access: Access) -> ApiResult<AppCaller> {
    match access {
        Access::Public => Ok(AppCaller::anonymous()),
        Access::Authenticated => auth::required(r).await,
        Access::Optional => auth::optional(r).await,
    }
}

/// The bucket, if the caller may address it at all: one of its project's (anyone's, for
/// anonymous requests and signed URLs).
pub(crate) fn bucket_for(r: &Req, caller: &AppCaller, name: &str) -> ApiResult<Bucket> {
    let b = r.store().bucket(name)?;
    match caller.account() {
        Some(a) if a != b.account_id => Err(ApiError::no_such_bucket()),
        _ => Ok(b),
    }
}

/// A refused read: anonymous callers don't learn about private buckets, others don't learn
/// about objects they can't see (Supabase's row-level security hides the row).
fn read_denied(e: S3Error, caller: &AppCaller) -> ApiError {
    match e.code {
        Code::AccessDenied if caller.is_anonymous() => ApiError::no_such_bucket(),
        Code::AccessDenied => ApiError::no_such_key(),
        _ => e.into(),
    }
}

/// A refused write.
pub(crate) fn write_denied(e: S3Error) -> ApiError {
    match e.code {
        Code::AccessDenied => ApiError::rls(),
        _ => e.into(),
    }
}

fn valid_key(key: &str) -> ApiResult<()> {
    if key.is_empty() || key.len() > 1024 || key.contains('\0') {
        return Err(ApiError::invalid_key(key));
    }
    Ok(())
}

fn version_param(r: &Req) -> ApiResult<Option<&str>> {
    match r.q("versionId").filter(|v| !v.is_empty()) {
        Some(v) if !s3_proto::versioning::valid_version_id(v) => Err(ApiError::invalid(format!("Invalid versionId {v}"))),
        v => Ok(v),
    }
}

// ---- Reads ----

pub(crate) async fn download(r: &Req, rest: &str, access: Access) -> ApiResult<Response> {
    let (bucket, key) = object_path(rest)?;
    let caller = caller_for(r, access).await?;
    let b = bucket_for(r, &caller, &bucket)?;
    r.checker(&caller).allows("s3:GetObject", &b.name, Some(&key)).map_err(|e| read_denied(e, &caller))?;
    let meta = r.store().get_object_version(&b.name, &key, version_param(r)?)?;
    serve(r, &b, &meta, None).await
}

/// Send an object (or its headers, for HEAD): conditional (`If-None-Match`), ranged,
/// `?download=`, metered.
pub(crate) async fn serve(r: &Req, b: &Bucket, meta: &ObjectMeta, expires_at_s: Option<i64>) -> ApiResult<Response> {
    let store = r.store();
    store.meter.request(&b.id, Class::B);
    let etag = s3_proto::http::quote_etag(&meta.etag);
    let mut resp = Response::new(Body::empty());
    let set = |resp: &mut Response, k: header::HeaderName, v: &str| {
        if let Ok(v) = HeaderValue::from_str(v) {
            resp.headers_mut().insert(k, v);
        }
    };
    set(&mut resp, header::ETAG, &etag);
    set(&mut resp, header::LAST_MODIFIED, &s3_proto::time::http_date(meta.modified_at));
    set(&mut resp, header::CACHE_CONTROL, util::cache_control(meta));
    if let Some(inm) = r.h("if-none-match")
        && inm.split(',').map(|t| s3_proto::http::unquote_etag(t.trim())).any(|t| t == "*" || t == meta.etag)
    {
        *resp.status_mut() = StatusCode::NOT_MODIFIED;
        return Ok(resp);
    }
    let range = s3_proto::http::parse_range(r.h("range"), meta.size).map_err(|_| ApiError::new(416, "InvalidRange", "invalid_range", "invalid range provided").real())?;
    set(&mut resp, header::CONTENT_TYPE, util::content_type(meta));
    set(&mut resp, header::ACCEPT_RANGES, "bytes");
    set(&mut resp, header::HeaderName::from_static("x-robots-tag"), "none");
    if let Some(e) = &meta.headers.content_encoding {
        set(&mut resp, header::CONTENT_ENCODING, e);
    }
    if let Some(d) = r.q("download") {
        set(&mut resp, header::CONTENT_DISPOSITION, &util::content_disposition(d));
    }
    if let Some(exp) = expires_at_s {
        set(&mut resp, header::EXPIRES, &s3_proto::time::http_date(exp * 1000));
    }
    let (len, open) = match range {
        Some((a, z)) => {
            *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
            set(&mut resp, header::CONTENT_RANGE, &s3_proto::http::content_range(a, z, meta.size));
            (z - a + 1, Some(a..z + 1))
        }
        None => (meta.size, None),
    };
    set(&mut resp, header::CONTENT_LENGTH, &len.to_string());
    if r.method == Method::HEAD {
        return Ok(resp);
    }
    let (_, stream) = store.open_blob(meta, open).await?;
    let metered = Metered { inner: stream, store: store.clone(), bucket_id: b.id.clone(), internal: r.internal(), sent: 0 };
    *resp.body_mut() = Body::from_stream(metered);
    Ok(resp)
}

pub(crate) async fn info(r: &Req, rest: &str, access: Access) -> ApiResult<Response> {
    let (bucket, key) = object_path(rest)?;
    let caller = caller_for(r, access).await?;
    let b = bucket_for(r, &caller, &bucket)?;
    r.checker(&caller).allows("s3:GetObject", &b.name, Some(&key)).map_err(|e| read_denied(e, &caller))?;
    let m = r.store().get_object_version(&b.name, &key, version_param(r)?)?;
    r.store().meter.request(&b.id, Class::B);
    let mut resp = json_response(&info_view(&b, &m));
    if let Ok(v) = HeaderValue::from_str(&s3_proto::http::quote_etag(&m.etag)) {
        resp.headers_mut().insert(header::ETAG, v);
    }
    Ok(resp)
}

/// Object info as Supabase's `/object/info` returns it (plus the owner).
pub(crate) fn info_view(b: &Bucket, m: &ObjectMeta) -> Value {
    let at = util::iso(m.modified_at);
    json!({
        "id": util::object_id(&b.id, m),
        "name": m.key,
        "version": util::version_of(b, m),
        "bucket_id": b.name,
        "size": m.size,
        "content_type": util::content_type(m),
        "cache_control": util::cache_control(m),
        "content_encoding": m.headers.content_encoding,
        "etag": s3_proto::http::quote_etag(&m.etag),
        "metadata": util::user_metadata(m),
        "last_modified": at,
        "created_at": at,
        "owner": m.headers.owner,
        "owner_id": m.headers.owner,
        "is_versioned": b.versioning.is_some(),
        "is_delete_marker": false,
    })
}

// ---- Writes ----

pub(crate) async fn upload(r: &Req, rest: &str, body: Body, put: bool) -> ApiResult<Response> {
    let (bucket, key) = object_path(rest)?;
    valid_key(&key)?;
    let caller = auth::required(r).await?;
    let b = bucket_for(r, &caller, &bucket)?;
    let upsert = put || r.h("x-upsert").is_some_and(|v| v.trim().eq_ignore_ascii_case("true"));
    r.checker(&caller).allows("s3:PutObject", &b.name, Some(&key)).map_err(write_denied)?;
    let meta = write_body(r, &b, &key, caller.sub().map(str::to_string), upsert, body).await?;
    Ok(json_response(&json!({ "Id": util::object_id(&b.id, &meta), "Key": format!("{}/{}", b.name, key) })))
}

fn bad_body(e: StoreError) -> ApiError {
    match e {
        StoreError::InvalidArgument(m) => ApiError::invalid(format!("The upload was incomplete: {m}")),
        e => e.into(),
    }
}

/// Store a request's body (raw, or a form's file) as `key`, through the S3 API's write path.
/// `upsert: false` refuses an existing key (`409`).
pub(crate) async fn write_body(r: &Req, b: &Bucket, key: &str, owner: Option<String>, upsert: bool, body: Body) -> ApiResult<ObjectMeta> {
    let store = r.store();
    let limits = store.bucket_limits(&b.name)?;
    let max = limits.file_size_limit.map_or(r.api.cfg.max_upload, |l| l.min(r.api.cfg.max_upload));
    let cond = if upsert { WriteCondition::none() } else { WriteCondition { if_none_match: Some("*".into()), ..Default::default() } };
    if !upsert && store.get_object(&b.name, key).is_ok() {
        return Err(ApiError::key_exists());
    }
    let ct = r.h("content-type").unwrap_or_default().to_string();
    let (headers, stream, len, _spool) = if ct.to_ascii_lowercase().starts_with("multipart/form-data") {
        let f = crate::form::read(body, &ct, &r.api.cfg.spool_dir, max).await?;
        let content_type = f.field("contentType").map(str::to_string).or_else(|| f.file_content_type.clone()).unwrap_or_else(|| "application/octet-stream".into());
        let cache = f.field("cacheControl").filter(|c| !c.is_empty()).map(|c| format!("max-age={c}")).unwrap_or_else(|| "no-cache".into());
        let um = match f.field("metadata").or(f.field("userMetadata")) {
            Some(t) => serde_json::from_str::<Value>(t).ok(),
            None => None,
        };
        let headers = ObjectHeaders {
            content_type: Some(content_type),
            cache_control: Some(cache),
            content_encoding: f.field("contentEncoding").filter(|e| !e.is_empty()).map(str::to_string),
            metadata: util::encode_user_metadata(um.as_ref())?,
            owner,
            ..Default::default()
        };
        let file = tokio::fs::File::open(f.file.path()).await.map_err(|_| ApiError::internal())?;
        (headers, util::file_stream(file), f.len, Some(f.file))
    } else {
        let um = util::header_user_metadata(r.h("x-metadata"))?;
        let headers = ObjectHeaders {
            content_type: Some(if ct.is_empty() { "application/octet-stream".into() } else { ct.clone() }),
            cache_control: Some(r.h("cache-control").filter(|c| !c.is_empty()).unwrap_or("no-cache").to_string()),
            content_encoding: r.h("content-encoding").filter(|e| !e.is_empty()).map(str::to_string),
            metadata: util::encode_user_metadata(um.as_ref())?,
            owner,
            ..Default::default()
        };
        limits.check_type(key, headers.content_type.as_deref())?;
        match r.h("content-length").map(|v| v.trim().parse::<u64>()) {
            Some(Ok(len)) => {
                if len > max {
                    return Err(ApiError::too_large("object"));
                }
                (headers, util::body_stream(body), len, None)
            }
            Some(Err(_)) => return Err(ApiError::invalid("invalid Content-Length")),
            None => {
                let (file, len) = spool(r, body, max).await?;
                let f = tokio::fs::File::open(file.path()).await.map_err(|_| ApiError::internal())?;
                (headers, util::file_stream(f), len, Some(file))
            }
        }
    };
    limits.check(key, Some(len), headers.content_type.as_deref())?;
    store.meter.request(&b.id, Class::A);
    let meta = s3_server::put_object(store, b, key, headers, stream, len, &cond).await.map_err(bad_body)?;
    store.meter.bytes_in(&b.id, len);
    Ok(meta)
}

/// Spool a body of unknown length (at most `max` bytes).
async fn spool(r: &Req, body: Body, max: u64) -> ApiResult<(tempfile::NamedTempFile, u64)> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;
    let tmp = tempfile::NamedTempFile::new_in(&r.api.cfg.spool_dir).map_err(|e| {
        tracing::error!(error = %e, "creating a spool file failed");
        ApiError::internal()
    })?;
    let mut out = tokio::fs::File::from_std(tmp.reopen().map_err(|_| ApiError::internal())?);
    let mut s = body.into_data_stream();
    let mut len = 0u64;
    while let Some(chunk) = s.next().await {
        let chunk = chunk.map_err(|_| ApiError::invalid("the upload was interrupted"))?;
        len += chunk.len() as u64;
        if len > max {
            return Err(ApiError::too_large("object"));
        }
        out.write_all(&chunk).await.map_err(|_| ApiError::internal())?;
    }
    out.flush().await.map_err(|_| ApiError::internal())?;
    Ok((tmp, len))
}

// ---- Deletes ----

pub(crate) async fn delete_one(r: &Req, rest: &str) -> ApiResult<Response> {
    let (bucket, key) = object_path(rest)?;
    let caller = auth::required(r).await?;
    let b = bucket_for(r, &caller, &bucket)?;
    r.checker(&caller).allows("s3:DeleteObject", &b.name, Some(&key)).map_err(|e| read_denied(e, &caller))?;
    let version = version_param(r)?;
    r.store().get_object_version(&b.name, &key, version)?;
    r.store().delete_object_version(&b.name, &key, version, None, false).await?;
    Ok(message("Successfully deleted"))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum DeleteItem {
    Path(String),
    Version {
        path: String,
        #[serde(rename = "versionId")]
        version_id: String,
    },
}

#[derive(Deserialize)]
struct DeleteMany {
    prefixes: Vec<DeleteItem>,
}

/// `DELETE /object/{bucket}` with `{"prefixes": [...]}`: the objects that were deleted (the
/// ones the caller may not delete, or that don't exist, are left out, as Supabase does).
pub(crate) async fn delete_many(r: &Req, rest: &str, body: Body) -> ApiResult<Response> {
    let (bucket, _) = bucket_and_key(rest)?;
    let caller = auth::required(r).await?;
    let b = bucket_for(r, &caller, &bucket)?;
    let req: DeleteMany = util::read_json(body).await?;
    if req.prefixes.is_empty() {
        return Err(ApiError::invalid("body/prefixes must NOT have fewer than 1 items"));
    }
    if req.prefixes.len() > MAX_DELETE {
        return Err(ApiError::invalid(format!("body/prefixes must NOT have more than {MAX_DELETE} items")));
    }
    let checker = r.checker(&caller);
    let store = r.store();
    let mut plain = Vec::new();
    let mut out = Vec::new();
    for item in req.prefixes {
        let (path, version) = match item {
            DeleteItem::Path(p) => (p, None),
            DeleteItem::Version { path, version_id } => (path, Some(version_id)),
        };
        if checker.allows("s3:DeleteObject", &b.name, Some(&path)).is_err() {
            continue;
        }
        let Ok(m) = store.get_object_version(&b.name, &path, version.as_deref()) else { continue };
        match version {
            Some(v) => {
                if store.delete_object_version(&b.name, &path, Some(&v), None, false).await.is_ok() {
                    out.push(object_view(&b, &m, &path));
                }
            }
            None => plain.push((path, m)),
        }
    }
    if !plain.is_empty() {
        let keys: Vec<String> = plain.iter().map(|(k, _)| k.clone()).collect();
        let results = store.delete_objects(&b.name, &keys).await?;
        for ((key, m), (_, res)) in plain.iter().zip(results) {
            if matches!(res, Ok(true)) {
                out.push(object_view(&b, m, key));
            }
        }
    }
    Ok(json_response(&Value::Array(out)))
}

// ---- Lists ----

#[derive(Deserialize, Default)]
struct SortBy {
    column: Option<String>,
    order: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ListBody {
    prefix: Option<String>,
    limit: Option<u64>,
    offset: Option<u64>,
    #[serde(rename = "sortBy")]
    sort_by: Option<SortBy>,
    search: Option<String>,
}

enum Entry {
    Folder(String),
    File(Box<ObjectMeta>),
}

impl Entry {
    fn key(&self) -> &str {
        match self {
            Entry::Folder(p) => p,
            Entry::File(m) => &m.key,
        }
    }
}

/// Entries under `prefix` (folders by `/`), in key order, that the caller may see: everything
/// when it may list the prefix, else the files it may read and the folders it may list.
/// Stops once `want` are collected (or a scan cap is reached).
fn collect(r: &Req, checker: &Checker, b: &Bucket, prefix: &str, delimiter: Option<&str>, start_after: Option<String>, want: usize) -> ApiResult<(Vec<Entry>, bool)> {
    let full = checker.may_list(&b.name, prefix);
    let mut out = Vec::new();
    let mut after = start_after;
    let mut scanned = 0usize;
    loop {
        let page = r.store().list_objects(&b.name, &ListQuery { prefix: prefix.to_string(), delimiter: delimiter.map(str::to_string), start_after: after.clone(), max_keys: 1000 })?;
        let mut entries: Vec<Entry> = page.prefixes.into_iter().map(Entry::Folder).chain(page.objects.into_iter().map(|m| Entry::File(Box::new(m)))).collect();
        entries.sort_by(|a, c| a.key().cmp(c.key()));
        for e in entries {
            scanned += 1;
            let visible = full
                || match &e {
                    Entry::Folder(p) => checker.may_list(&b.name, p),
                    Entry::File(m) => checker.allows("s3:GetObject", &b.name, Some(&m.key)).is_ok(),
                };
            if visible {
                out.push(e);
                if out.len() >= want {
                    return Ok((out, true));
                }
            }
        }
        if !page.truncated || scanned >= MAX_SORT_SCAN * 10 {
            return Ok((out, page.truncated));
        }
        after = page.next;
    }
}

/// `POST /object/list/{bucket}`: Supabase's `search` (folders and files directly under the
/// prefix, names relative to it).
pub(crate) async fn list(r: &Req, rest: &str, body: Body) -> ApiResult<Response> {
    let (bucket, _) = bucket_and_key(rest)?;
    let caller = auth::required(r).await?;
    let b = bucket_for(r, &caller, &bucket)?;
    let q: ListBody = util::read_json(body).await?;
    let mut prefix = q.prefix.unwrap_or_default();
    if !prefix.is_empty() && !prefix.ends_with('/') {
        prefix.push('/');
    }
    let full = format!("{prefix}{}", q.search.unwrap_or_default());
    let limit = q.limit.unwrap_or(100).clamp(1, 1500) as usize;
    let offset = q.offset.unwrap_or(0).min(MAX_SORT_SCAN as u64) as usize;
    let sort = q.sort_by.unwrap_or_default();
    let column = sort.column.unwrap_or_else(|| "name".into());
    let desc = sort.order.is_some_and(|o| o.eq_ignore_ascii_case("desc"));
    let by_name = column == "name" && !desc;
    let want = if by_name { offset + limit } else { MAX_SORT_SCAN };
    let checker = r.checker(&caller);
    r.store().meter.request(&b.id, Class::A);
    let (mut entries, _) = collect(r, &checker, &b, &full, Some("/"), None, want)?;
    if !by_name {
        // Folders first, then files, each in the order asked for (Supabase's).
        let time = |e: &Entry| match e {
            Entry::File(m) => m.modified_at,
            Entry::Folder(_) => 0,
        };
        entries.sort_by(|a, c| {
            let rank = |e: &Entry| matches!(e, Entry::File(_));
            let ord = if column == "name" { a.key().cmp(c.key()) } else { time(a).cmp(&time(c)).then_with(|| a.key().cmp(c.key())) };
            rank(a).cmp(&rank(c)).then(if desc { ord.reverse() } else { ord })
        });
    }
    let out: Vec<Value> = entries
        .iter()
        .skip(offset)
        .take(limit)
        .map(|e| match e {
            Entry::Folder(p) => json!({ "name": p[prefix.len()..].trim_end_matches('/'), "id": null, "updated_at": null, "created_at": null, "last_accessed_at": null, "metadata": null }),
            Entry::File(m) => object_view(&b, m, &m.key[prefix.len()..]),
        })
        .collect();
    Ok(json_response(&Value::Array(out)))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ListV2Body {
    prefix: Option<String>,
    cursor: Option<String>,
    with_delimiter: Option<bool>,
    limit: Option<u64>,
}

/// `POST /object/list-v2/{bucket}`: cursor pagination in key order; folders and objects apart,
/// with full keys.
pub(crate) async fn list_v2(r: &Req, rest: &str, body: Body) -> ApiResult<Response> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let (bucket, _) = bucket_and_key(rest)?;
    let caller = auth::required(r).await?;
    let b = bucket_for(r, &caller, &bucket)?;
    let q: ListV2Body = util::read_json(body).await?;
    let prefix = q.prefix.unwrap_or_default();
    let limit = q.limit.unwrap_or(1000).clamp(1, 1000) as usize;
    let after = match q.cursor.filter(|c| !c.is_empty()) {
        Some(c) => Some(b64.decode(&c).ok().and_then(|v| String::from_utf8(v).ok()).ok_or_else(|| ApiError::invalid("invalid cursor"))?),
        None => None,
    };
    let checker = r.checker(&caller);
    r.store().meter.request(&b.id, Class::A);
    let delimiter = q.with_delimiter.unwrap_or(false).then_some("/");
    let (entries, more) = collect(r, &checker, &b, &prefix, delimiter, after, limit + 1)?;
    let has_next = entries.len() > limit || (more && entries.len() == limit + 1);
    let shown = &entries[..entries.len().min(limit)];
    let mut folders = Vec::new();
    let mut objects = Vec::new();
    for e in shown {
        match e {
            Entry::Folder(p) => folders.push(json!({ "key": p, "name": p })),
            Entry::File(m) => {
                let mut v = object_view(&b, m, &m.key);
                v["key"] = json!(m.key);
                objects.push(v);
            }
        }
    }
    let mut v = json!({ "hasNext": has_next, "folders": folders, "objects": objects });
    if has_next && let Some(last) = shown.last() {
        v["nextCursor"] = json!(b64.encode(last.key()));
    }
    Ok(json_response(&v))
}

// ---- Move and copy ----

#[derive(Deserialize)]
struct MoveBody {
    #[serde(rename = "bucketId")]
    bucket_id: String,
    #[serde(rename = "sourceKey")]
    source_key: String,
    #[serde(rename = "destinationKey")]
    destination_key: String,
    #[serde(rename = "destinationBucket", default)]
    destination_bucket: Option<String>,
    #[serde(rename = "sourceVersionId", default)]
    source_version_id: Option<String>,
}

/// Authorize reading (and, for a move, deleting) the source and writing the destination.
fn authorize_transfer(checker: &Checker, caller: &AppCaller, src: (&str, &str), dst: (&str, &str), delete_source: bool) -> ApiResult<()> {
    checker.allows("s3:GetObject", src.0, Some(src.1)).map_err(|e| read_denied(e, caller))?;
    if delete_source {
        checker.allows("s3:DeleteObject", src.0, Some(src.1)).map_err(|e| read_denied(e, caller))?;
    }
    checker.allows("s3:PutObject", dst.0, Some(dst.1)).map_err(write_denied)
}

pub(crate) async fn move_object(r: &Req, body: Body) -> ApiResult<Response> {
    let caller = auth::required(r).await?;
    let q: MoveBody = util::read_json(body).await?;
    valid_key(&q.destination_key)?;
    let src = bucket_for(r, &caller, &q.bucket_id)?;
    let dst = match q.destination_bucket.as_deref().filter(|d| !d.is_empty() && *d != src.name) {
        Some(d) => bucket_for(r, &caller, d)?,
        None => src.clone(),
    };
    let checker = r.checker(&caller);
    authorize_transfer(&checker, &caller, (&src.name, &q.source_key), (&dst.name, &q.destination_key), true)?;
    let store = r.store();
    let version = q.source_version_id.as_deref().filter(|v| !v.is_empty());
    store.get_object_version(&src.name, &q.source_key, version)?;
    if src.name == dst.name && q.source_key == q.destination_key && version.is_none() {
        let m = store.get_object(&src.name, &q.source_key)?;
        return Ok(json_response(&json!({ "message": "Successfully moved", "Id": util::object_id(&dst.id, &m), "Key": q.destination_key })));
    }
    if store.get_object(&dst.name, &q.destination_key).is_ok() {
        return Err(ApiError::key_exists());
    }
    store.meter.request(&dst.id, Class::A);
    let none = s3_store::RenameCondition::default();
    let must_not_exist = s3_store::RenameCondition { if_none_match: Some("*".into()), ..Default::default() };
    let m = if src.name == dst.name && version.is_none() {
        store.rename_object(&src.name, &q.source_key, &q.destination_key, &none, &must_not_exist).await?
    } else {
        let opts = CopyOptions { condition: WriteCondition { if_none_match: Some("*".into()), ..Default::default() }, source_version: version.map(str::to_string), ..Default::default() };
        let m = store.copy_object(&src.name, &q.source_key, &dst.name, &q.destination_key, &opts).await?;
        store.delete_object_version(&src.name, &q.source_key, version, None, false).await?;
        m
    };
    Ok(json_response(&json!({ "message": "Successfully moved", "Id": util::object_id(&dst.id, &m), "Key": q.destination_key })))
}

#[derive(Deserialize, Default)]
struct CopyMetadata {
    #[serde(rename = "cacheControl")]
    cache_control: Option<String>,
    #[serde(rename = "contentEncoding")]
    content_encoding: Option<String>,
    mimetype: Option<String>,
}

#[derive(Deserialize)]
struct CopyBody {
    #[serde(rename = "bucketId")]
    bucket_id: String,
    #[serde(rename = "sourceKey")]
    source_key: String,
    #[serde(rename = "destinationKey")]
    destination_key: String,
    #[serde(rename = "destinationBucket", default)]
    destination_bucket: Option<String>,
    #[serde(rename = "sourceVersionId", default)]
    source_version_id: Option<String>,
    #[serde(rename = "copyMetadata", default)]
    copy_metadata: Option<Value>,
    #[serde(default)]
    metadata: Option<CopyMetadata>,
}

/// `POST /object/copy`: a server-side copy (the destination shares the source's blob). The copy
/// belongs to the caller, as on Supabase.
pub(crate) async fn copy(r: &Req, body: Body) -> ApiResult<Response> {
    let caller = auth::required(r).await?;
    let q: CopyBody = util::read_json(body).await?;
    valid_key(&q.destination_key)?;
    let src = bucket_for(r, &caller, &q.bucket_id)?;
    let dst = match q.destination_bucket.as_deref().filter(|d| !d.is_empty()) {
        Some(d) => bucket_for(r, &caller, d)?,
        None => src.clone(),
    };
    let checker = r.checker(&caller);
    authorize_transfer(&checker, &caller, (&src.name, &q.source_key), (&dst.name, &q.destination_key), false)?;
    let copy_metadata = match &q.copy_metadata {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) if s == "true" || s == "false" => s == "true",
        Some(Value::Number(n)) if n.as_u64() == Some(0) || n.as_u64() == Some(1) => n.as_u64() == Some(1),
        _ => return Err(ApiError::invalid("body/copyMetadata must be boolean")),
    };
    let store = r.store();
    let version = q.source_version_id.as_deref().filter(|v| !v.is_empty());
    let source = store.get_object_version(&src.name, &q.source_key, version)?;
    let upsert = r.h("x-upsert").is_some_and(|v| v.trim().eq_ignore_ascii_case("true"));
    if !upsert && store.get_object(&dst.name, &q.destination_key).is_ok() {
        return Err(ApiError::key_exists());
    }
    let mut headers = source.headers.clone();
    headers.tags = Default::default();
    headers.owner = caller.sub().map(str::to_string).or(source.headers.owner.clone());
    if !copy_metadata {
        headers.metadata = util::encode_user_metadata(util::header_user_metadata(r.h("x-metadata"))?.as_ref())?;
    } else if let Some(um) = util::header_user_metadata(r.h("x-metadata"))? {
        headers.metadata = util::encode_user_metadata(Some(&um))?;
    }
    if let Some(m) = q.metadata {
        if let Some(c) = m.cache_control {
            headers.cache_control = Some(c);
        }
        if let Some(e) = m.content_encoding {
            headers.content_encoding = Some(e);
        }
        if let Some(t) = m.mimetype {
            headers.content_type = Some(t);
        }
    }
    let condition = if upsert { WriteCondition::none() } else { WriteCondition { if_none_match: Some("*".into()), ..Default::default() } };
    let opts = CopyOptions { headers: Some(headers), condition, source_version: version.map(str::to_string), ..Default::default() };
    store.meter.request(&dst.id, Class::A);
    let m = store.copy_object(&src.name, &q.source_key, &dst.name, &q.destination_key, &opts).await?;
    let mut v = object_view(&dst, &m, &q.destination_key);
    v["Id"] = json!(util::object_id(&dst.id, &m));
    v["Key"] = json!(format!("{}/{}", dst.name, q.destination_key));
    Ok(json_response(&v))
}
