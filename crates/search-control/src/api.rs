//! Control-plane HTTP API for roll search (`/v1/search/...`), merged into rolld's API.
//!
//! Errors are `{"error": "..."}` with 400 (invalid), 404 (no such cluster), 409 (name taken,
//! busy), 413 (a console request over the cluster's body limit) or 500, like the other control
//! planes. A cluster's password appears only in the
//! responses that create it (create, branch) or change it (rotate). Creating a cluster answers
//! in the shape `search-yaml run --cluster-hook` expects (`url`, `username`, `password`, `id`).
//!
//! rolld puts the API behind its operator gate (roll-auth), like every product's. On a fleet,
//! calls about one cluster run on its owner (`crate::cluster::on_owner`), and so does creating a
//! branch (on its parent's owner).

use crate::control::{ClusterOptions, ConsoleRequest, ConsoleResponse, Control, ControlError, Usage};
use crate::store::SearchCluster;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use tower_http::cors::CorsLayer;

type AppState = Arc<Control>;
type ApiResult<T> = Result<Json<T>, ControlError>;

impl IntoResponse for ControlError {
    fn into_response(self) -> Response {
        let status = match &self {
            ControlError::NotFound(_) => StatusCode::NOT_FOUND,
            ControlError::Invalid(_) => StatusCode::BAD_REQUEST,
            ControlError::Conflict(_) => StatusCode::CONFLICT,
            ControlError::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            ControlError::Internal(e) => {
                tracing::warn!(error = %e, "search control error");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

pub fn router(control: Arc<Control>) -> Router {
    Router::new()
        .route("/v1/search", get(info))
        .route("/v1/search/clusters", get(list).post(create))
        .route("/v1/search/clusters/{id}", get(show).patch(update).delete(remove))
        .route("/v1/search/clusters/{id}/branches", get(branches).post(branch))
        .route("/v1/search/clusters/{id}/password", post(rotate))
        .route("/v1/search/clusters/{id}/unload", post(unload))
        .route("/v1/search/clusters/{id}/indices", get(indices))
        .route("/v1/search/clusters/{id}/request", post(request))
        .route("/v1/search/clusters/{id}/usage", get(usage))
        .route("/v1/search/clusters/{id}/keys", post(create_key))
        .route("/v1/search/clusters/{id}/keys/{key}", axum::routing::delete(delete_key))
        .route("/v1/search/clusters/{id}/indices/{index}", get(index_get).put(index_create).delete(index_delete))
        .route("/v1/search/clusters/{id}/indices/{index}/mapping", axum::routing::put(index_mapping))
        .route("/v1/search/clusters/{id}/indices/{index}/search", post(index_search))
        .route("/v1/search/clusters/{id}/indices/{index}/docs", post(doc_create))
        .route("/v1/search/clusters/{id}/indices/{index}/docs/{doc}", get(doc_get).put(doc_put).delete(doc_delete))
        .route("/v1/search/clusters/{id}/indices/{index}/bulk", post(index_bulk))
        .route("/v1/search/clusters/{id}/indices/{index}/delete_by_query", post(index_delete_by_query))
        // On a fleet, calls about one cluster run on its owner.
        .layer(axum::middleware::from_fn({
            let c = control.clone();
            move |req: Request, next: Next| crate::cluster::on_owner(c.clone(), req, next)
        }))
        // The dashboard calls it from the browser, like the other control planes.
        .layer(CorsLayer::permissive())
        .with_state(control)
}

/// Forwards an API call to the owner of cluster `name` when that is another node of the fleet
/// (`None`: run it here).
async fn forward(c: &Control, name: &str, method: Method, path: &str, headers: &HeaderMap, body: bytes::Bytes) -> Option<Response> {
    let cluster = c.cluster_leases()?.cluster().clone();
    let node = match cluster.route_async(&crate::cluster::key(name)).await {
        Ok(roll_cluster::Route::Forward(n)) => n,
        _ => return None,
    };
    let mut r = axum::http::Request::builder().method(method).uri(path);
    for (k, v) in headers {
        r = r.header(k, v);
    }
    let req = r.body(Body::from(body)).ok()?;
    Some(cluster.forward(&node, req, 0).await)
}

async fn info(State(c): State<AppState>) -> Json<Value> {
    let stats = c.data.stats();
    Json(json!({
        "version": search_es::version::ES_VERSION,
        "opensearch_version": search_es::version::OPENSEARCH_VERSION,
        "lucene_version": search_es::version::LUCENE_VERSION,
        "region": c.cfg.region,
        "http": format!("{}:{}", c.cfg.public_host, c.cfg.http_port),
        "tls": c.cfg.tls,
        "domain": c.cfg.domain,
        "storage": c.objects().description,
        "history_retention_s": c.cfg.history_retention.as_secs(),
        "defaults": c.cfg.defaults,
        "pricing": c.cfg.pricing,
        "loaded": c.node.loaded().len(),
        "cache": { "used_bytes": stats.cache.used, "budget_bytes": stats.cache.budget },
    }))
}

#[derive(Serialize)]
struct BranchRef {
    id: String,
    name: String,
    parent_at: Option<i64>,
    created_at: i64,
}

#[derive(Serialize)]
struct ClusterView {
    #[serde(flatten)]
    cluster: SearchCluster,
    #[serde(flatten)]
    connection: Value,
    status: Value,
    /// Name of the parent (`parent_id` stays when the parent is deleted; this doesn't).
    parent_name: Option<String>,
    branches: Vec<BranchRef>,
    /// Only when created or rotated.
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}

async fn view(c: &Control, mut d: SearchCluster, with_password: bool) -> Result<ClusterView, ControlError> {
    // The catalog's figures are metering's (refreshed every `stats_interval` while loaded); a
    // cluster loaded here shows its live index and document counts. Stored bytes stay the
    // billed object-store figure.
    if let Some(l) = c.node.get_loaded(&d.name) {
        let f = crate::meter::engine_figures(&l.cluster);
        d.indices = f.indices as i64;
        if f.complete() {
            d.docs = f.docs as i64;
        }
    }
    let branches = c
        .store
        .children(&d.id)
        .await?
        .into_iter()
        .map(|b| BranchRef { id: b.id, name: b.name, parent_at: b.parent_at, created_at: b.created_at })
        .collect();
    let parent_name = match &d.parent_id {
        Some(p) => c.store.get(p).await?.map(|p| p.name),
        None => None,
    };
    Ok(ClusterView {
        connection: c.connection(&d),
        status: c.status(&d).await,
        parent_name,
        branches,
        password: with_password.then(|| d.password.clone()),
        cluster: d,
    })
}

async fn views(c: &Control, list: Vec<SearchCluster>) -> Result<Vec<ClusterView>, ControlError> {
    let mut out = Vec::with_capacity(list.len());
    for d in list {
        out.push(view(c, d, false).await?);
    }
    Ok(out)
}

async fn list(State(c): State<AppState>) -> ApiResult<Vec<ClusterView>> {
    let list = c.list().await?;
    Ok(Json(views(&c, list).await?))
}

#[derive(Deserialize)]
struct CreateReq {
    name: String,
    /// Branch of this cluster (id or name)…
    parent: Option<String>,
    /// …as of this Unix second (default now).
    at: Option<i64>,
    #[serde(flatten)]
    opts: ClusterOptions,
}

async fn create(State(c): State<AppState>, method: Method, headers: HeaderMap, body: bytes::Bytes) -> Result<Response, ControlError> {
    let r: CreateReq = serde_json::from_slice(&body).map_err(|e| ControlError::Invalid(format!("invalid request: {e}")))?;
    let d = match &r.parent {
        Some(p) => {
            // A branch is cut on the node holding the parent, which commits it first.
            let parent = c.get(p).await?;
            if let Some(resp) = forward(&c, &parent.name, method, "/v1/search/clusters", &headers, body.clone()).await {
                return Ok(resp);
            }
            c.branch(p, &r.name, r.at, r.opts).await?
        }
        None if r.at.is_some() => return Err(ControlError::Invalid("at needs a parent".into())),
        None => c.create(&r.name, r.opts).await?,
    };
    Ok(Json(view(&c, d, true).await?).into_response())
}

async fn show(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<ClusterView> {
    let d = c.get(&id).await?;
    Ok(Json(view(&c, d, false).await?))
}

async fn update(State(c): State<AppState>, Path(id): Path<String>, Json(o): Json<ClusterOptions>) -> ApiResult<ClusterView> {
    let d = c.update(&id, o).await?;
    Ok(Json(view(&c, d, false).await?))
}

async fn remove(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    let d = c.get(&id).await?;
    c.delete(&d.id).await?;
    Ok(Json(json!({ "deleted": d.id, "name": d.name })))
}

#[derive(Deserialize)]
struct BranchReq {
    name: String,
    /// Unix seconds; default now.
    at: Option<i64>,
    #[serde(flatten)]
    opts: ClusterOptions,
}

async fn branch(State(c): State<AppState>, Path(id): Path<String>, method: Method, headers: HeaderMap, body: bytes::Bytes) -> Result<Response, ControlError> {
    let r: BranchReq = serde_json::from_slice(&body).map_err(|e| ControlError::Invalid(format!("invalid request: {e}")))?;
    let parent = c.get(&id).await?;
    if let Some(resp) = forward(&c, &parent.name, method, &format!("/v1/search/clusters/{id}/branches"), &headers, body.clone()).await {
        return Ok(resp);
    }
    let d = c.branch(&id, &r.name, r.at, r.opts).await?;
    Ok(Json(view(&c, d, true).await?).into_response())
}

async fn branches(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Vec<ClusterView>> {
    let p = c.get(&id).await?;
    let list = c.store.children(&p.id).await?;
    Ok(Json(views(&c, list).await?))
}

async fn rotate(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<ClusterView> {
    let d = c.rotate_password(&id).await?;
    Ok(Json(view(&c, d, true).await?))
}

async fn unload(State(c): State<AppState>, Path(id): Path<String>, method: Method, headers: HeaderMap) -> Result<Response, ControlError> {
    let d = c.get(&id).await?;
    if let Some(resp) = forward(&c, &d.name, method, &format!("/v1/search/clusters/{id}/unload"), &headers, bytes::Bytes::new()).await {
        return Ok(resp);
    }
    Ok(Json(json!({ "unloaded": c.unload(&id).await? })).into_response())
}

async fn indices(State(c): State<AppState>, Path(id): Path<String>, method: Method, headers: HeaderMap) -> Result<Response, ControlError> {
    let d = c.get(&id).await?;
    if let Some(resp) = forward(&c, &d.name, method, &format!("/v1/search/clusters/{id}/indices"), &headers, bytes::Bytes::new()).await {
        return Ok(resp);
    }
    Ok(Json(c.indices(&id).await?).into_response())
}

/// The console request is read up to the cluster's own body limit (`max_body_mb`, like a
/// client talking to it directly) plus room for its JSON envelope, instead of axum's 2 MB
/// default: a body sent as a JSON string grows by its escapes. The cluster then enforces its
/// limit exactly, with Elasticsearch's 413.
async fn request(State(c): State<AppState>, Path(id): Path<String>, method: Method, headers: HeaderMap, body: Body) -> Result<Response, ControlError> {
    let cluster = c.get(&id).await?;
    let limit = console_limit(cluster.max_body_mb);
    let bytes = read_limited(body, limit).await.map_err(|e| match e {
        Some(e) => ControlError::Invalid(format!("reading the request failed: {e}")),
        None => ControlError::TooLarge(format!("the request is larger than cluster {}'s body limit allows ({} MiB, max_body_mb)", cluster.name, cluster.max_body_mb)),
    })?;
    let r: ConsoleRequest = serde_json::from_slice(&bytes).map_err(|e| ControlError::Invalid(format!("invalid request: {e}")))?;
    if let Some(resp) = forward(&c, &cluster.name, method, &format!("/v1/search/clusters/{id}/request"), &headers, bytes.clone()).await {
        return Ok(resp);
    }
    let out: ConsoleResponse = c.request(&cluster.id, r).await?;
    Ok(Json(out).into_response())
}

/// Bytes past the limit read (and dropped) before answering 413, and for how long.
const DRAIN_BYTES: usize = 64 << 20;
const DRAIN_FOR: std::time::Duration = std::time::Duration::from_secs(10);

/// Reads `body` up to `limit` bytes; `Err(None)` when it is longer. An oversized body is still
/// read to its end (dropping the rest, within [`DRAIN_BYTES`] and [`DRAIN_FOR`]) before the
/// 413 goes out: answering while the client is still uploading closes the connection under it,
/// and the client then sees a reset instead of the reason.
async fn read_limited(body: Body, limit: usize) -> Result<bytes::Bytes, Option<String>> {
    use http_body_util::BodyExt;
    let mut body = body;
    let mut buf = bytes::BytesMut::new();
    let mut over = 0usize;
    let deadline = tokio::time::Instant::now() + DRAIN_FOR;
    loop {
        let frame = if over == 0 {
            body.frame().await
        } else {
            match tokio::time::timeout_at(deadline, body.frame()).await {
                Ok(f) => f,
                Err(_) => return Err(None),
            }
        };
        let Some(frame) = frame else { break };
        let frame = frame.map_err(|e| Some(e.to_string()))?;
        let Ok(data) = frame.into_data() else { continue };
        if over == 0 && buf.len() + data.len() <= limit {
            buf.extend_from_slice(&data);
            continue;
        }
        if over == 0 {
            buf = bytes::BytesMut::new();
        }
        over += data.len();
        if over > DRAIN_BYTES {
            return Err(None);
        }
    }
    if over > 0 { Err(None) } else { Ok(buf.freeze()) }
}

/// Bytes of a console request for a cluster whose bodies may be `max_body_mb` MiB: twice that
/// (escaped quotes and newlines double in a JSON string) plus 1 MiB for the rest.
pub fn console_limit(max_body_mb: i64) -> usize {
    ((max_body_mb.max(1) as usize) << 21).saturating_add(1 << 20)
}

// --- per index: one route per Elasticsearch operation, so the cluster and index are in the
// path (roll's permission catalogue classifies requests by route and method). Each answers
// with Elasticsearch's own status and body; query strings (`refresh`, `routing`, `size`, …)
// pass through.

/// What a per-index route sends: Elasticsearch's method and path under the index, and the
/// kind of body it takes.
#[derive(Clone, Copy, PartialEq)]
enum EsBody {
    None,
    Json,
    Ndjson,
}

/// An index (or, for reads, an index expression: `logs-*`, `a,b`) as a path segment.
fn index_segment(index: &str, read: bool) -> Result<String, ControlError> {
    let bad = index.is_empty() || index.starts_with('_') || index.contains('/') || (!read && (index.contains(',') || index.contains('*')));
    if bad {
        return Err(ControlError::Invalid(format!("invalid index name [{index}]")));
    }
    Ok(urlencoding::encode(index).into_owned())
}

#[allow(clippy::too_many_arguments)]
async fn es_route(c: &Control, id: &str, method: Method, api_path: String, es_method: &str, es_path: String, query: Option<&str>, headers: &HeaderMap, body: Body, kind: EsBody) -> Result<Response, ControlError> {
    let cluster = c.get(id).await?;
    let bytes = match kind {
        EsBody::None => bytes::Bytes::new(),
        _ => read_limited(body, (cluster.max_body_mb.max(1) as usize) << 20).await.map_err(|e| match e {
            Some(e) => ControlError::Invalid(format!("reading the request failed: {e}")),
            None => ControlError::TooLarge(format!("the request is larger than cluster {}'s body limit allows ({} MiB, max_body_mb)", cluster.name, cluster.max_body_mb)),
        })?,
    };
    if kind == EsBody::Json && !bytes.is_empty() && serde_json::from_slice::<serde::de::IgnoredAny>(&bytes).is_err() {
        return Err(ControlError::Invalid("the body isn't JSON".into()));
    }
    let q = query.map(|q| format!("?{q}")).unwrap_or_default();
    if let Some(resp) = forward(c, &cluster.name, method, &format!("{api_path}{q}"), headers, bytes.clone()).await {
        return Ok(resp);
    }
    let ct = match kind {
        EsBody::Ndjson => Some("application/x-ndjson"),
        _ => Some("application/json"),
    };
    let (status, ct, body) = c.es(&cluster.id, es_method, &format!("{es_path}{q}"), ct, bytes).await?;
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    Ok((status, [(axum::http::header::CONTENT_TYPE, ct)], body).into_response())
}

fn api_index_path(id: &str, index: &str, rest: &str) -> String {
    format!("/v1/search/clusters/{}/indices/{}{rest}", urlencoding::encode(id), urlencoding::encode(index))
}

macro_rules! index_route {
    ($name:ident, $es_method:literal, $rest:literal, $es_rest:literal, $read:expr, $kind:expr) => {
        async fn $name(State(c): State<AppState>, Path((id, index)): Path<(String, String)>, method: Method, uri: axum::http::Uri, headers: HeaderMap, body: Body) -> Result<Response, ControlError> {
            let seg = index_segment(&index, $read)?;
            es_route(&c, &id, method, api_index_path(&id, &index, $rest), $es_method, format!("/{seg}{}", $es_rest), uri.query(), &headers, body, $kind).await
        }
    };
}

index_route!(index_get, "GET", "", "", true, EsBody::None);
index_route!(index_create, "PUT", "", "", false, EsBody::Json);
index_route!(index_delete, "DELETE", "", "", false, EsBody::None);
index_route!(index_mapping, "PUT", "/mapping", "/_mapping", false, EsBody::Json);
index_route!(index_search, "POST", "/search", "/_search", true, EsBody::Json);
index_route!(doc_create, "POST", "/docs", "/_doc", false, EsBody::Json);
index_route!(index_bulk, "POST", "/bulk", "/_bulk", false, EsBody::Ndjson);
index_route!(index_delete_by_query, "POST", "/delete_by_query", "/_delete_by_query", true, EsBody::Json);

macro_rules! doc_route {
    ($name:ident, $es_method:literal, $kind:expr) => {
        async fn $name(State(c): State<AppState>, Path((id, index, doc)): Path<(String, String, String)>, method: Method, uri: axum::http::Uri, headers: HeaderMap, body: Body) -> Result<Response, ControlError> {
            let seg = index_segment(&index, false)?;
            let d = urlencoding::encode(&doc).into_owned();
            es_route(&c, &id, method, api_index_path(&id, &index, &format!("/docs/{d}")), $es_method, format!("/{seg}/_doc/{d}"), uri.query(), &headers, body, $kind).await
        }
    };
}

doc_route!(doc_get, "GET", EsBody::None);
doc_route!(doc_put, "PUT", EsBody::Json);
doc_route!(doc_delete, "DELETE", EsBody::None);

#[derive(Deserialize)]
struct NewKey {
    name: String,
    expires_in_s: Option<i64>,
}

async fn create_key(State(c): State<AppState>, Path(id): Path<String>, Json(k): Json<NewKey>) -> ApiResult<Value> {
    Ok(Json(c.create_key(&id, &k.name, k.expires_in_s).await?))
}

async fn delete_key(State(c): State<AppState>, Path((id, key)): Path<(String, String)>) -> Result<StatusCode, ControlError> {
    c.delete_key(&id, &key).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct UsageQuery {
    days: Option<i64>,
}

async fn usage(State(c): State<AppState>, Path(id): Path<String>, Query(q): Query<UsageQuery>) -> ApiResult<Usage> {
    Ok(Json(c.usage(&id, q.days.unwrap_or(30).clamp(1, 366)).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A streamed body of `n` chunks of `size` bytes; `sent` counts the chunks taken from it.
    fn chunks(n: usize, size: usize, sent: Arc<AtomicUsize>) -> Body {
        let s = futures_util::stream::iter((0..n).map(move |_| {
            sent.fetch_add(1, Ordering::SeqCst);
            Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b'x'; size]))
        }));
        Body::from_stream(s)
    }

    #[tokio::test]
    async fn bodies_within_the_limit_are_read_whole() {
        let sent = Arc::new(AtomicUsize::new(0));
        let b = read_limited(chunks(4, 1000, sent.clone()), 4000).await.unwrap();
        assert_eq!(b.len(), 4000);
        assert_eq!(read_limited(Body::empty(), 0).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn an_oversized_body_is_read_to_its_end_before_the_refusal() {
        let sent = Arc::new(AtomicUsize::new(0));
        assert_eq!(read_limited(chunks(10, 1000, sent.clone()), 4500).await.unwrap_err(), None);
        assert_eq!(sent.load(Ordering::SeqCst), 10, "the rest was drained, so the client sees the 413");
        // Past the drain allowance it stops reading (the connection then closes).
        let sent = Arc::new(AtomicUsize::new(0));
        let n = (DRAIN_BYTES >> 20) + 50;
        assert_eq!(read_limited(chunks(n, 1 << 20, sent.clone()), 1 << 20).await.unwrap_err(), None);
        assert!(sent.load(Ordering::SeqCst) < n);
    }

}
