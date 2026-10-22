//! Control-plane HTTP API for storage (`/v1/storage/...`), merged into rolld's API.

use crate::control::{Control, ControlError, Usage, UsageAll, info};
use axum::body::Body;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::extract::FromRequest;
use axum::extract::rejection::JsonRejection;
use axum::{Json, Router};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use s3_store::{BucketOptions, BucketUpdate, ByteStream, ObjectHeaders, Store};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tower_http::cors::CorsLayer;

type AppState = Arc<Control>;
type ApiResult<T> = Result<Json<T>, ControlError>;

impl IntoResponse for ControlError {
    fn into_response(self) -> Response {
        let status = match &self {
            ControlError::NotFound(_) => StatusCode::NOT_FOUND,
            ControlError::Invalid(_) => StatusCode::BAD_REQUEST,
            ControlError::Conflict(_) => StatusCode::CONFLICT,
            ControlError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            ControlError::Internal(e) => {
                tracing::error!(error = %e, "storage control plane error");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

/// `Json` whose rejections are `{"error": ...}` 400s like every other error.
pub(crate) struct JsonBody<T>(pub T);

impl<S: Send + Sync, T: serde::de::DeserializeOwned> FromRequest<S> for JsonBody<T> {
    type Rejection = ControlError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(Self(v)),
            Err(e) => Err(ControlError::Invalid(json_error(e))),
        }
    }
}

fn json_error(e: JsonRejection) -> String {
    e.body_text()
}

/// `Query` whose rejections (`?limit=-1`) are `{"error": ...}` 400s too.
struct Params<T>(T);

impl<S: Send + Sync, T: serde::de::DeserializeOwned> FromRequestParts<S> for Params<T> {
    type Rejection = ControlError;

    async fn from_request_parts(parts: &mut axum::http::request::Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(v)) => Ok(Self(v)),
            Err(e) => Err(ControlError::Invalid(e.body_text())),
        }
    }
}

pub fn router(control: Arc<Control>) -> Router {
    Router::new()
        .route("/v1/storage", get(show_info))
        .route("/v1/storage/usage", get(usage_all))
        .route("/v1/storage/buckets", get(list_buckets).post(create_bucket))
        .route("/v1/storage/buckets/{name}", get(show_bucket).patch(update_bucket).delete(delete_bucket))
        .route("/v1/storage/buckets/{name}/branches", post(branch))
        .route("/v1/storage/buckets/{name}/objects", get(list_objects))
        .route("/v1/storage/buckets/{name}/versions", get(list_versions))
        .route("/v1/storage/buckets/{name}/object", get(object_info).delete(delete_object))
        .route("/v1/storage/buckets/{name}/delete", post(delete_many))
        .route("/v1/storage/buckets/{name}/download", get(download))
        .route("/v1/storage/buckets/{name}/upload", put(upload))
        .route("/v1/storage/buckets/{name}/presign", post(presign))
        .route("/v1/storage/buckets/{name}/trash", get(trash))
        .route("/v1/storage/buckets/{name}/trash/restore", post(restore))
        .route("/v1/storage/buckets/{name}/usage", get(usage))
        .route("/v1/storage/keys", get(list_keys).post(create_key))
        .route("/v1/storage/keys/{id}", delete(delete_key))
        .merge(crate::roles::routes())
        .merge(crate::targets::routes())
        // The dashboard calls it from the browser, like the other control-plane APIs.
        .layer(CorsLayer::permissive())
        .with_state(control)
}

async fn show_info(State(c): State<AppState>) -> ApiResult<Value> {
    Ok(Json(c.info()?))
}

async fn list_buckets(State(c): State<AppState>) -> ApiResult<Vec<Value>> {
    Ok(Json(c.store.buckets()?.iter().map(|b| c.bucket_view(b)).collect()))
}

#[derive(Deserialize)]
struct CreateBucket {
    name: String,
    #[serde(flatten)]
    opts: BucketOptions,
}

async fn create_bucket(State(c): State<AppState>, JsonBody(r): JsonBody<CreateBucket>) -> ApiResult<Value> {
    let b = c.store.create_bucket(&r.name, r.opts).await?;
    Ok(Json(c.bucket_view(&b)))
}

async fn show_bucket(State(c): State<AppState>, Path(name): Path<String>) -> ApiResult<Value> {
    Ok(Json(c.bucket_view(&c.store.bucket(&name)?)))
}

/// `PATCH /buckets/{name}`: the store's settings, the bucket's tags (replaced whole; `{}`
/// removes them), notifications, and the settings in [`crate::settings`]. Everything is checked
/// before anything is applied.
#[derive(Deserialize)]
struct BucketPatch {
    #[serde(flatten)]
    update: BucketUpdate,
    tags: Option<BTreeMap<String, String>>,
    notifications: Option<NotificationsPatch>,
    #[serde(flatten)]
    settings: crate::settings::SettingsPatch,
}

/// `notifications` in a bucket `PATCH`: the configurations (replaced whole; omitted:
/// unchanged), and the webhook secret.
#[derive(Deserialize)]
struct NotificationsPatch {
    configurations: Option<Vec<s3_store::NotificationTarget>>,
    event_bridge: Option<bool>,
    /// S3's `x-amz-skip-destination-validation`.
    #[serde(default)]
    skip_destination_validation: bool,
    /// Replace the bucket's webhook signing secret.
    #[serde(default)]
    rotate_webhook_secret: bool,
}

async fn update_bucket(State(c): State<AppState>, Path(name): Path<String>, JsonBody(p): JsonBody<BucketPatch>) -> ApiResult<Value> {
    let b = c.store.bucket(&name)?;
    if let Some(t) = &p.tags {
        s3_store::config::validate_bucket_tags(t.iter().map(|(k, v)| (k.as_str(), v.as_str())))?;
    }
    let mut settings = c.prepare_settings(&b, p.update.versioning.as_deref(), p.update.public, p.settings)?;
    // Buckets outside roll are checked over the network (their versioning), before anything.
    c.check_replication_targets(&mut settings).await?;
    // Notifications next (their destinations are checked by sending them a test event): a
    // refused configuration leaves the rest of the PATCH unapplied.
    if let Some(n) = p.notifications {
        if n.configurations.is_some() || n.event_bridge.is_some() {
            let current = c.store.notifications(&name)?.unwrap_or_default();
            let cfg = s3_store::NotificationConfig {
                configurations: n.configurations.unwrap_or(current.configurations),
                event_bridge: n.event_bridge.unwrap_or(current.event_bridge),
            };
            c.set_notifications(&name, cfg, n.skip_destination_validation).await?;
        }
        if n.rotate_webhook_secret {
            c.store.webhook_secret(&name, true).await?;
        }
    }
    c.apply_settings(&name, &settings, true).await?;
    c.store.update_bucket(&name, p.update).await?;
    if let Some(t) = &p.tags {
        c.store.set_bucket_tags(&name, t).await?;
    }
    c.apply_settings(&name, &settings, false).await?;
    Ok(Json(c.bucket_view(&c.store.bucket(&name)?)))
}

#[derive(Deserialize)]
struct ForceQuery {
    #[serde(default)]
    force: bool,
}

async fn delete_bucket(State(c): State<AppState>, Path(name): Path<String>, Params(q): Params<ForceQuery>) -> ApiResult<Value> {
    c.store.delete_bucket(&name, q.force).await?;
    Ok(Json(json!({ "deleted": name })))
}

#[derive(Deserialize)]
struct BranchReq {
    name: String,
}

async fn branch(State(c): State<AppState>, Path(name): Path<String>, JsonBody(r): JsonBody<BranchReq>) -> ApiResult<Value> {
    let b = c.store.branch_bucket(&name, &r.name).await?;
    Ok(Json(c.bucket_view(&b)))
}

#[derive(Deserialize)]
struct ListQ {
    #[serde(default)]
    prefix: String,
    delimiter: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

async fn list_objects(State(c): State<AppState>, Path(name): Path<String>, Params(q): Params<ListQ>) -> ApiResult<Value> {
    Ok(Json(c.list_objects(&name, &q.prefix, q.delimiter.as_deref(), q.cursor.as_deref().filter(|s| !s.is_empty()), q.limit.unwrap_or(200))?))
}

async fn list_versions(State(c): State<AppState>, Path(name): Path<String>, Params(q): Params<ListQ>) -> ApiResult<Value> {
    Ok(Json(c.list_versions(&name, &q.prefix, q.cursor.as_deref().filter(|s| !s.is_empty()), q.limit.unwrap_or(200))?))
}

#[derive(Deserialize)]
struct KeyQ {
    key: Option<String>,
    /// A version of the object (`null` or an id); none: the current one.
    version_id: Option<String>,
}

impl KeyQ {
    fn key(&self) -> Result<String, ControlError> {
        self.key.clone().filter(|k| !k.is_empty()).ok_or_else(|| ControlError::Invalid("missing ?key=".into()))
    }

    fn version(&self) -> Result<Option<&str>, ControlError> {
        match self.version_id.as_deref().filter(|v| !v.is_empty()) {
            Some(v) if !s3_proto::versioning::valid_version_id(v) => Err(ControlError::Invalid(format!("invalid version id {v}"))),
            v => Ok(v),
        }
    }
}

async fn object_info(State(c): State<AppState>, Path(name): Path<String>, Params(q): Params<KeyQ>) -> ApiResult<Value> {
    Ok(Json(info(&c.store.get_object_version(&name, &q.key()?, q.version()?)?)))
}

async fn delete_object(State(c): State<AppState>, Path(name): Path<String>, Params(q): Params<KeyQ>) -> ApiResult<Value> {
    let key = q.key()?;
    let o = c.delete_object(&name, &key, q.version()?).await?;
    let mut v = json!({ "deleted": key });
    // Versions are only mentioned where there are any.
    if let Some(id) = o.version_id {
        v["version_id"] = json!(id);
        v["delete_marker"] = json!(o.delete_marker);
    }
    Ok(Json(v))
}

#[derive(Deserialize)]
struct DeleteMany {
    keys: Option<Vec<String>>,
    prefix: Option<String>,
}

async fn delete_many(State(c): State<AppState>, Path(name): Path<String>, JsonBody(r): JsonBody<DeleteMany>) -> ApiResult<Value> {
    Ok(Json(json!({ "deleted": c.delete_many(&name, r.keys, r.prefix).await? })))
}

/// Counts bytes sent, metered when the download ends. The control plane can't tell where its
/// caller is, so this is counted as traffic inside roll (free), like the dashboard's own.
struct Metered {
    inner: ByteStream,
    store: Arc<Store>,
    bucket_id: String,
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
        self.store.meter.egress(&self.bucket_id, self.sent, true);
    }
}

/// `attachment` with the key's last segment as the file name (ASCII fallback plus RFC 5987).
fn attachment(key: &str) -> String {
    let name = key.rsplit('/').find(|s| !s.is_empty()).unwrap_or("download");
    let ascii: String = name.chars().map(|ch| if ch.is_ascii_graphic() && ch != '"' && ch != '\\' { ch } else { '_' }).collect();
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{}", s3_proto::http::uri_encode(name, true))
}

async fn download(State(c): State<AppState>, Path(name): Path<String>, Params(q): Params<KeyQ>) -> Result<Response, ControlError> {
    let key = q.key()?;
    let b = c.store.bucket(&name)?;
    let meta = c.store.get_object_version(&name, &key, q.version()?)?;
    let (len, stream) = c.store.open_blob(&meta, None).await?;
    c.store.meter.request(&b.id, s3_store::Class::B);
    let body = Body::from_stream(Metered { inner: stream, store: c.store.clone(), bucket_id: b.id, sent: 0 });
    let mut r = Response::new(body);
    let h = r.headers_mut();
    let set = |h: &mut HeaderMap, n: header::HeaderName, v: &str| {
        if let Ok(v) = v.parse() {
            h.insert(n, v);
        }
    };
    set(h, header::CONTENT_TYPE, meta.headers.content_type.as_deref().unwrap_or("application/octet-stream"));
    set(h, header::CONTENT_LENGTH, &len.to_string());
    set(h, header::CONTENT_DISPOSITION, &attachment(&key));
    set(h, header::ETAG, &s3_proto::http::quote_etag(&meta.etag));
    Ok(r)
}

/// The object headers of an upload: the standard ones, `x-amz-meta-*` and `x-amz-tagging`.
fn upload_headers(h: &HeaderMap) -> Result<ObjectHeaders, ControlError> {
    let s = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
    let pairs: Vec<(String, String)> = h.iter().filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))).collect();
    let metadata = s3_proto::http::user_metadata(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str()))).map_err(|e| ControlError::Invalid(e.message.clone()))?;
    let tags: BTreeMap<String, String> = match s("x-amz-tagging") {
        Some(t) => s3_proto::http::parse_tagging_header(&t).map_err(|e| ControlError::Invalid(e.message.clone()))?.into_iter().collect(),
        None => BTreeMap::new(),
    };
    Ok(ObjectHeaders {
        content_type: s("content-type"),
        cache_control: s("cache-control"),
        content_disposition: s("content-disposition"),
        content_encoding: s("content-encoding"),
        content_language: s("content-language"),
        expires: s("expires"),
        metadata,
        tags,
        ..Default::default()
    })
}

async fn upload(State(c): State<AppState>, Path(name): Path<String>, Params(q): Params<KeyQ>, headers: HeaderMap, body: Body) -> ApiResult<Value> {
    let key = q.key()?;
    // Without a Content-Length, only a body the server knows is empty (no body at all) will do.
    let len: u64 = match headers.get(header::CONTENT_LENGTH) {
        Some(v) => v.to_str().ok().and_then(|v| v.trim().parse().ok()).ok_or_else(|| ControlError::Invalid("invalid Content-Length".into()))?,
        None => axum::body::HttpBody::size_hint(&body).exact().ok_or_else(|| ControlError::Invalid("Content-Length is required".into()))?,
    };
    if len > 5 << 30 {
        return Err(ControlError::Invalid("uploads here are limited to 5 GiB; use the S3 API's multipart upload for larger objects".into()));
    }
    let oh = upload_headers(&headers)?;
    let stream = body.into_data_stream().map(|r| r.map_err(std::io::Error::other)).boxed();
    let meta = c.upload(&name, &key, oh, stream, len).await?;
    Ok(Json(info(&meta)))
}

#[derive(Deserialize)]
struct PresignReq {
    key: String,
    method: Option<String>,
    expires_s: Option<i64>,
}

async fn presign(State(c): State<AppState>, Path(name): Path<String>, JsonBody(r): JsonBody<PresignReq>) -> ApiResult<Value> {
    let method = r.method.unwrap_or_else(|| "GET".into()).to_ascii_uppercase();
    let (url, expires_at) = c.presign(&name, &r.key, &method, r.expires_s.unwrap_or(3600))?;
    Ok(Json(json!({ "url": url, "method": method, "expires_at": expires_at })))
}

async fn trash(State(c): State<AppState>, Path(name): Path<String>, Params(q): Params<ListQ>) -> ApiResult<Value> {
    Ok(Json(c.list_trash(&name, &q.prefix, q.cursor.as_deref().filter(|s| !s.is_empty()), q.limit.unwrap_or(200))?))
}

#[derive(Deserialize)]
struct RestoreReq {
    key: String,
    deleted_at: i64,
}

async fn restore(State(c): State<AppState>, Path(name): Path<String>, JsonBody(r): JsonBody<RestoreReq>) -> ApiResult<Value> {
    Ok(Json(info(&c.store.restore(&name, &r.key, r.deleted_at).await?)))
}

#[derive(Deserialize)]
struct DaysQ {
    days: Option<i64>,
}

async fn usage(State(c): State<AppState>, Path(name): Path<String>, Params(q): Params<DaysQ>) -> ApiResult<Usage> {
    Ok(Json(c.usage(&name, q.days.unwrap_or(30))?))
}

async fn usage_all(State(c): State<AppState>, Params(q): Params<DaysQ>) -> ApiResult<UsageAll> {
    Ok(Json(c.usage_all(q.days.unwrap_or(30))?))
}

async fn list_keys(State(c): State<AppState>) -> ApiResult<Value> {
    Ok(Json(json!(c.keys()?)))
}

#[derive(Deserialize)]
struct CreateKey {
    name: String,
    permission: String,
    buckets: Option<Vec<String>>,
}

async fn create_key(State(c): State<AppState>, JsonBody(r): JsonBody<CreateKey>) -> ApiResult<Value> {
    let k = c.create_key(&r.name, &r.permission, r.buckets).await?;
    let mut v = json!(k);
    v["secret_access_key"] = json!(k.secret);
    Ok(Json(v))
}

async fn delete_key(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    c.delete_key(&id).await?;
    Ok(Json(json!({ "deleted": id })))
}
