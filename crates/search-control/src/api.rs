//! Control-plane HTTP API for roll search (`/v1/search/...`), merged into rolld's API.
//!
//! Errors are `{"error": "..."}` with 400 (invalid), 404 (no such cluster), 409 (name taken,
//! busy, past a person's cap), 413 (a console request over the cluster's body limit) or 500, like the other control
//! planes. A cluster's password appears only in the
//! responses that create it (create, branch) or change it (rotate). Creating a cluster answers
//! in the shape `search-yaml run --cluster-hook` expects (`url`, `username`, `password`, `id`).
//!
//! rolld's gate (roll-auth) lets the operator and signed-in people in (`Access::User`), and
//! search scopes them ([`Caller`], docs/accounts.md "Phase 2"): the operator and admins see every
//! cluster; anyone else sees, creates and branches only their own (`SearchCluster::owner`), and
//! every `/v1/search/clusters/{id}/…` call about another's answers 404, as for a cluster that
//! doesn't exist, and their clusters' `status` doesn't say which node of the fleet holds them. A person's creates, branches and switches to always-on are held to the node's
//! per-person caps (`SearchConfig::person_caps`), counted in the write's own transaction. A person's own token reaches search as them (roll-access hands it on with
//! `SignedIn`, without `Operator`). On a fleet, calls about one cluster run on its owner
//! (`crate::cluster::on_owner`), and so does creating a branch (on its parent's owner); the
//! check is made from the catalog before the call is forwarded and again on the owner, behind
//! its own gate.

use crate::control::{ClusterOptions, ConsoleAnswer, ConsoleRequest, Control, ControlError, Usage};
use crate::store::{Caps, SearchCluster};
use axum::body::Body;
use axum::extract::{FromRequestParts, Path, Query, Request, State};
use axum::http::{Extensions, HeaderMap, Method, StatusCode, request::Parts};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use search_server::limits::{ByteBudget, Full, Reservation};
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
            ControlError::Forbidden(_) => StatusCode::FORBIDDEN,
            ControlError::Busy(_) => StatusCode::TOO_MANY_REQUESTS,
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
        .route("/v1/search/clusters/{id}/restore", post(restore))
        .route("/v1/search/clusters/{id}/indices", get(indices))
        .route("/v1/search/clusters/{id}/request", post(request))
        .route("/v1/search/clusters/{id}/usage", get(usage))
        .route("/v1/search/clusters/{id}/metrics", get(metrics))
        .route("/v1/search/clusters/{id}/slowlog", get(slowlog))
        .route("/v1/search/clusters/{id}/audit", get(audit))
        .route("/v1/search/clusters/{id}/analytics", get(analytics))
        .route("/v1/search/clusters/{id}/keys", post(create_key))
        .route("/v1/search/clusters/{id}/repositories/{name}", axum::routing::put(put_repository))
        .route("/v1/search/clusters/{id}/keys/{key}", axum::routing::delete(delete_key))
        .route("/v1/search/clusters/{id}/migrate/inspect", post(migrate_inspect))
        .route("/v1/search/clusters/{id}/migrate", post(migrate))
        .route("/v1/search/clusters/{id}/browser-keys", get(browser_keys).post(create_browser_key))
        .route("/v1/search/clusters/{id}/browser-keys/{key}", axum::routing::delete(revoke_browser_key))
        .route("/v1/search/clusters/{id}/remotes", get(remotes))
        .route("/v1/search/clusters/{id}/remotes/{alias}", axum::routing::put(link_remote).delete(unlink_remote))
        .route("/v1/search/clusters/{id}/indices/{index}", get(index_get).put(index_create).delete(index_delete))
        .route("/v1/search/clusters/{id}/indices/{index}/mapping", axum::routing::put(index_mapping).get(index_mapping_get))
        .route("/v1/search/clusters/{id}/indices/{index}/settings", get(index_settings).put(index_settings_put))
        .route("/v1/search/clusters/{id}/indices/{index}/explain/{doc}", post(doc_explain))
        .route("/v1/search/clusters/{id}/synonyms", get(synonyms_list))
        .route("/v1/search/clusters/{id}/synonyms/{set}", get(synonyms_get).put(synonyms_put).delete(synonyms_delete))
        .route("/v1/search/clusters/{id}/synonyms/{set}/{rule}", get(synonym_rule_get).put(synonym_rule_put).delete(synonym_rule_delete))
        .route("/v1/search/clusters/{id}/query_rules", get(rulesets_list))
        .route("/v1/search/clusters/{id}/query_rules/{ruleset}", get(ruleset_get).put(ruleset_put).delete(ruleset_delete))
        .route("/v1/search/clusters/{id}/query_rules/{ruleset}/test", post(ruleset_test))
        .route("/v1/search/clusters/{id}/indices/{index}/refresh", post(index_refresh))
        .route("/v1/search/clusters/{id}/indices/{index}/analyze", post(index_analyze))
        .route("/v1/search/clusters/{id}/indices/{index}/search", post(index_search))
        .route("/v1/search/clusters/{id}/indices/{index}/docs", post(doc_create))
        .route("/v1/search/clusters/{id}/indices/{index}/docs/{doc}", get(doc_get).put(doc_put).delete(doc_delete))
        .route("/v1/search/clusters/{id}/indices/{index}/bulk", post(index_bulk))
        .route("/v1/search/clusters/{id}/indices/{index}/delete_by_query", post(index_delete_by_query))
        .route("/v1/search/clusters/{id}/indices/{index}/suggestions", get(suggestions))
        .route("/v1/search/clusters/{id}/indices/{index}/suggestions/rebuild", post(rebuild_suggestions))
        // Calls about one cluster: the caller's own, then on its owner.
        .layer(axum::middleware::from_fn_with_state(control.clone(), per_cluster))
        // The dashboard calls it from the browser, like the other control planes.
        .layer(CorsLayer::permissive())
        .with_state(control)
}

/// Who is asking. The gate lets the operator, admins and signed-in people through; a request with
/// neither `Operator` nor `SignedIn` comes from an open daemon (no `ROLL_ADMIN_TOKEN`, loopback
/// only) or from rolld's own in-process calls, and sees everything, as before owners.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    /// The operator, an admin, or an open daemon: every cluster; what it creates has no owner.
    All,
    /// A signed-in person who isn't an admin (`usr_…`), or their own token: their clusters only.
    Person(String),
}

impl Caller {
    pub fn of(ext: &Extensions) -> Caller {
        if ext.get::<roll_auth::Operator>().is_some() {
            return Caller::All;
        }
        match ext.get::<roll_auth::SignedIn>() {
            Some(s) if !s.admin => Caller::Person(s.user.clone()),
            _ => Caller::All,
        }
    }

    /// Whether `c` is one the caller may see (anyone else's answers 404).
    pub fn sees(&self, c: &SearchCluster) -> bool {
        match self {
            Caller::All => true,
            Caller::Person(u) => c.owner.as_deref() == Some(u.as_str()),
        }
    }

    /// The caps the caller's creates, branches and settings are held to: the node's per-person
    /// ones for a person (counted toward the cluster's owner), none for the operator and admins.
    fn caps(&self, c: &Control) -> Caps {
        match self {
            Caller::All => Caps::NONE,
            Caller::Person(_) => c.cfg.person_caps,
        }
    }

    /// The owner a cluster the caller creates gets.
    fn owner(&self) -> Option<&str> {
        match self {
            Caller::All => None,
            Caller::Person(u) => Some(u),
        }
    }

    /// `id_or_name`, if the caller may see it: another's is "no such cluster".
    async fn get(&self, c: &Control, id_or_name: &str) -> Result<SearchCluster, ControlError> {
        let d = c.get(id_or_name).await?;
        if self.sees(&d) { Ok(d) } else { Err(no_such(id_or_name)) }
    }
}

fn no_deleted(id: &str) -> ControlError {
    ControlError::NotFound(format!("no deleted search cluster {id}"))
}

fn no_such(id_or_name: &str) -> ControlError {
    ControlError::NotFound(format!("no search cluster {id_or_name}"))
}

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Caller::of(&parts.extensions))
    }
}

/// Every `/v1/search/clusters/{id}/…` call: the cluster is resolved from the catalog and must be
/// the caller's (else 404), then the call runs on the cluster's owner. Each node a call reaches
/// checks it, so the check holds wherever it lands.
async fn per_cluster(State(c): State<AppState>, req: Request, next: Next) -> Response {
    let rest = req.uri().path().strip_prefix("/v1/search/clusters/").map(|r| r.split_once('/').unwrap_or((r, "")));
    let id = rest.map(|(id, _)| id).filter(|s| !s.is_empty());
    let Some(id) = id.and_then(|id| urlencoding::decode(id).ok().map(|s| s.into_owned())) else { return next.run(req).await };
    // A deleted cluster has no owner node: its restore runs where it lands, once the caller
    // may see it (theirs, or the operator's and admins').
    if rest.is_some_and(|(_, r)| r == "restore") {
        return match c.get_deleted(&id).await {
            Ok(d) if Caller::of(req.extensions()).sees(&d.cluster) => next.run(req).await,
            Ok(_) => no_deleted(&id).into_response(),
            Err(e) => e.into_response(),
        };
    }
    let d = match Caller::of(req.extensions()).get(&c, &id).await {
        Ok(d) => d,
        // A key revoked while its cluster is deleted is kept for the restore
        // (`Control::delete_key`): it too runs where it lands, for whoever may see the cluster.
        Err(ControlError::NotFound(_)) if req.method() == Method::DELETE && rest.is_some_and(|(_, r)| r.starts_with("keys/") || r.starts_with("browser-keys/")) => {
            return match c.get_deleted(&id).await {
                Ok(d) if Caller::of(req.extensions()).sees(&d.cluster) => next.run(req).await,
                _ => no_such(&id).into_response(),
            };
        }
        Err(e) => return e.into_response(),
    };
    crate::cluster::on_owner(&c, &d.name, req, next).await
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

/// The node's versions, endpoint, defaults and pricing, and for the operator where its data is
/// and its node-wide figures (clusters loaded, the cache), which a person doesn't get.
async fn info(State(c): State<AppState>, caller: Caller) -> Json<Value> {
    let mut v = json!({
        "version": search_es::version::ES_VERSION,
        "opensearch_version": search_es::version::OPENSEARCH_VERSION,
        "lucene_version": search_es::version::LUCENE_VERSION,
        "region": c.cfg.region,
        "http": format!("{}:{}", c.cfg.public_host, c.cfg.http_port),
        "tls": c.cfg.tls,
        "domain": c.cfg.domain,
        "history_retention_s": c.cfg.history_retention.as_secs(),
        "deleted_retention_s": c.cfg.deleted_retention.as_secs(),
        "defaults": c.cfg.defaults,
        "pricing": c.cfg.pricing,
    });
    let caps = caller.caps(&c);
    if caps != Caps::NONE {
        // What the person may own (0: no cap), for "3 of 50 clusters".
        v["caps"] = json!({ "clusters": caps.clusters, "always_on": caps.always_on });
    }
    if caller == Caller::All {
        let stats = c.data.stats();
        v["storage"] = json!(c.objects().description);
        v["loaded"] = json!(c.node.loaded().len());
        v["cache"] = json!({ "used_bytes": stats.cache.used, "budget_bytes": stats.cache.budget });
    }
    Json(v)
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

async fn view(c: &Control, caller: &Caller, mut d: SearchCluster, with_password: bool) -> Result<ClusterView, ControlError> {
    // The catalog's figures are metering's (refreshed every `stats_interval` while loaded); a
    // cluster loaded here shows its live index and document counts. Stored bytes stay the
    // billed object-store figure.
    if let Some(l) = c.node.get_loaded(&d.name) {
        let f = crate::meter::engine_figures(&l.cluster);
        d.indices = f.indices as i64;
        if f.complete_for_display() {
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
    let mut status = c.status(&d).await;
    // Which node of the fleet holds it is the operator's (and admins') to know.
    if *caller != Caller::All
        && let Some(o) = status.as_object_mut()
    {
        o.remove("node");
    }
    Ok(ClusterView {
        connection: c.connection(&d),
        status,
        parent_name,
        branches,
        password: with_password.then(|| d.password.clone()),
        cluster: d,
    })
}

async fn views(c: &Control, caller: &Caller, list: Vec<SearchCluster>) -> Result<Vec<ClusterView>, ControlError> {
    let mut out = Vec::with_capacity(list.len());
    for d in list {
        out.push(view(c, caller, d, false).await?);
    }
    Ok(out)
}

#[derive(Deserialize)]
struct ListQuery {
    /// Recently deleted clusters instead, kept for restoring.
    #[serde(default)]
    deleted: bool,
}

async fn list(State(c): State<AppState>, caller: Caller, Query(q): Query<ListQuery>) -> Result<Response, ControlError> {
    if q.deleted {
        return Ok(Json(c.list_deleted(caller.owner()).await?).into_response());
    }
    let list = match &caller {
        Caller::All => c.list().await?,
        Caller::Person(u) => c.store.list_owned(u).await?,
    };
    Ok(Json(views(&c, &caller, list).await?).into_response())
}

#[derive(Deserialize, Default)]
struct RestoreReq {
    /// Restore under this name instead of the one it had (that one is taken).
    name: Option<String>,
}

/// `POST /v1/search/clusters/{id}/restore`: a deleted cluster back as it was, within its window.
async fn restore(State(c): State<AppState>, caller: Caller, Path(id): Path<String>, body: bytes::Bytes) -> ApiResult<ClusterView> {
    let r: RestoreReq = if body.iter().all(u8::is_ascii_whitespace) {
        RestoreReq::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ControlError::Invalid(format!("invalid request: {e}")))?
    };
    let d = c.restore(&id, r.name.as_deref(), caller.caps(&c)).await?;
    Ok(Json(view(&c, &caller, d, false).await?))
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

async fn create(State(c): State<AppState>, caller: Caller, method: Method, headers: HeaderMap, body: bytes::Bytes) -> Result<Response, ControlError> {
    let r: CreateReq = serde_json::from_slice(&body).map_err(|e| ControlError::Invalid(format!("invalid request: {e}")))?;
    let d = match &r.parent {
        Some(p) => {
            // A branch is cut on the node holding the parent, which commits it first. It is the
            // parent owner's.
            let parent = caller.get(&c, p).await?;
            // The owner gets the parent checked here by its id: a cluster that took its name in
            // between is never branched in its place.
            let mut forwarded: Value = serde_json::from_slice(&body).map_err(|e| ControlError::Invalid(format!("invalid request: {e}")))?;
            forwarded["parent"] = json!(parent.id);
            let forwarded = bytes::Bytes::from(serde_json::to_vec(&forwarded).map_err(|e| ControlError::Internal(e.into()))?);
            let mut headers = headers;
            headers.remove(axum::http::header::CONTENT_LENGTH);
            if let Some(resp) = forward(&c, &parent.name, method, "/v1/search/clusters", &headers, forwarded).await {
                return Ok(resp);
            }
            c.branch(&parent, &r.name, r.at, r.opts, caller.caps(&c)).await?
        }
        None if r.at.is_some() => return Err(ControlError::Invalid("at needs a parent".into())),
        None => c.create(&r.name, r.opts, caller.owner(), caller.caps(&c)).await?,
    };
    Ok(Json(view(&c, &caller, d, true).await?).into_response())
}

async fn show(State(c): State<AppState>, caller: Caller, Path(id): Path<String>) -> ApiResult<ClusterView> {
    let d = c.get(&id).await?;
    Ok(Json(view(&c, &caller, d, false).await?))
}

async fn update(State(c): State<AppState>, caller: Caller, Path(id): Path<String>, Json(o): Json<ClusterOptions>) -> ApiResult<ClusterView> {
    let d = c.update(&id, o, caller.caps(&c)).await?;
    Ok(Json(view(&c, &caller, d, false).await?))
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

async fn branch(State(c): State<AppState>, caller: Caller, Path(id): Path<String>, method: Method, headers: HeaderMap, body: bytes::Bytes) -> Result<Response, ControlError> {
    let r: BranchReq = serde_json::from_slice(&body).map_err(|e| ControlError::Invalid(format!("invalid request: {e}")))?;
    let parent = c.get(&id).await?;
    if let Some(resp) = forward(&c, &parent.name, method, &format!("/v1/search/clusters/{id}/branches"), &headers, body.clone()).await {
        return Ok(resp);
    }
    let d = c.branch(&parent, &r.name, r.at, r.opts, caller.caps(&c)).await?;
    Ok(Json(view(&c, &caller, d, true).await?).into_response())
}

async fn branches(State(c): State<AppState>, caller: Caller, Path(id): Path<String>) -> ApiResult<Vec<ClusterView>> {
    let p = c.get(&id).await?;
    let list = c.store.children(&p.id).await?.into_iter().filter(|b| caller.sees(b)).collect();
    Ok(Json(views(&c, &caller, list).await?))
}

async fn rotate(State(c): State<AppState>, caller: Caller, Path(id): Path<String>) -> ApiResult<ClusterView> {
    let d = c.rotate_password(&id).await?;
    Ok(Json(view(&c, &caller, d, true).await?))
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
async fn request(State(c): State<AppState>, Path(id): Path<String>, method: Method, headers: HeaderMap, ext: Extensions, body: Body) -> Result<Response, ControlError> {
    let cluster = c.get(&id).await?;
    // Charged to the node's body budget while it's read and handed on.
    let mut held = Reservation::none();
    let bytes = read_limited(body, console_limit(cluster.max_body_mb), declared(&headers), c.node.body_budget(), &mut held).await.map_err(|e| read_error(&cluster, e))?;
    let r = ConsoleRequest::parse(&bytes)?;
    console_reveal(&cluster, &r, &ext)?;
    if let Some(resp) = forward(&c, &cluster.name, method, &format!("/v1/search/clusters/{id}/request"), &headers, bytes.clone()).await {
        return Ok(resp);
    }
    let send = r.body_in(&bytes)?;
    // A string body (NDJSON) was decoded into a copy of its own: charged before the read bytes go.
    if !bytes.as_ptr_range().contains(&send.as_ptr()) {
        c.node.body_budget().grow(&mut held, send.len()).map_err(|f| read_error(&cluster, ReadError::Busy(f)))?;
    }
    let (m, p, h) = (r.method.clone(), r.path.clone(), r.headers.clone());
    drop(r);
    drop(bytes);
    // The node charges what it runs from here.
    drop(held);
    match c.request(&cluster.id, &m, &p, h, send).await? {
        ConsoleAnswer::Whole(out) => Ok(Json(out).into_response()),
        // A streamed answer (server-sent events) goes on as the cluster sends it, with its status
        // and content type: the console's envelope would have to wait for its end.
        ConsoleAnswer::Stream { status, headers, body } => {
            use futures::StreamExt;
            let body = body.take().map_or_else(Body::empty, |s| Body::from_stream(s.map(Ok::<_, std::convert::Infallible>)));
            let mut resp = Response::new(body);
            *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
            if let Some(v) = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-type")).and_then(|(_, v)| axum::http::HeaderValue::from_str(v).ok()) {
                resp.headers_mut().insert(axum::http::header::CONTENT_TYPE, v);
            }
            resp.headers_mut().insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
            Ok(resp)
        }
    }
}

/// A grant's console request whose routed API answers or sets a credential (a new API key, a
/// connector's secrets, a user's password) needs `reveal` on the cluster too, as `POST …/keys`
/// does: the gate says whether the grant has it on this cluster (the catalogue row's `capable`,
/// `roll_auth::Capable`; docs/permissions.md). The operator and people on their own clusters
/// carry no grant.
fn console_reveal(cluster: &SearchCluster, r: &ConsoleRequest, ext: &Extensions) -> Result<(), ControlError> {
    if roll_auth::Grant::on(ext).is_none() || !r.handles_credentials(cluster) {
        return Ok(());
    }
    match ext.get::<roll_auth::Capable>().is_some_and(|c| c.has(roll_auth::scope::Action::Reveal)) {
        true => Ok(()),
        // An approval is the gate's to ask for, before the request reaches a product: a rule
        // that would ask for reveal isn't asked here (docs/permissions.md).
        false => Err(ControlError::Forbidden(format!(
            "{} {} answers or sets a credential: it needs reveal allowed on cluster {} (a rule that asks for reveal isn't asked here; use POST …/keys, or a token that may reveal)",
            r.method.trim(),
            r.path.trim(),
            cluster.name
        ))),
    }
}

/// Why a body wasn't read ([`read_limited`]).
#[derive(Debug, PartialEq)]
enum ReadError {
    Failed(String),
    /// Longer than the limit.
    TooLarge,
    /// The node's body budget is full.
    Busy(Full),
}

/// The request's `Content-Length`, when it gives one.
fn declared(headers: &HeaderMap) -> Option<usize> {
    headers.get(axum::http::header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse().ok())
}

fn read_error(cluster: &SearchCluster, e: ReadError) -> ControlError {
    match e {
        ReadError::Failed(e) => ControlError::Invalid(format!("reading the request failed: {e}")),
        ReadError::TooLarge => ControlError::TooLarge(format!("the request is larger than cluster {}'s body limit allows ({} MiB, max_body_mb)", cluster.name, cluster.max_body_mb)),
        ReadError::Busy(f) => ControlError::Busy(format!("request bodies in flight on this node are at its limit ({} of {} bytes): retry", f.used, f.limit)),
    }
}

/// Bytes past the limit read (and dropped) before answering 413, and for how long.
const DRAIN_BYTES: usize = 64 << 20;
const DRAIN_FOR: std::time::Duration = std::time::Duration::from_secs(10);

/// Reads `body` up to `limit` bytes, charging the buffer it keeps (its capacity, not just its
/// length) to `budget` in `held` as it grows; `TooLarge` when it is longer. A `declared` length
/// within the limit sizes the buffer exactly up front. An oversized body is still read to its end (dropping
/// the rest, within [`DRAIN_BYTES`] and [`DRAIN_FOR`]) before the 413 goes out: answering while
/// the client is still uploading closes the connection under it, and the client then sees a
/// reset instead of the reason.
async fn read_limited(body: Body, limit: usize, declared: Option<usize>, budget: &Arc<ByteBudget>, held: &mut Reservation) -> Result<bytes::Bytes, ReadError> {
    use http_body_util::BodyExt;
    let mut body = body;
    let mut buf = bytes::BytesMut::new();
    if let Some(n) = declared.filter(|n| *n <= limit) {
        budget.grow(held, n).map_err(ReadError::Busy)?;
        buf.reserve(n);
    }
    let mut over = 0usize;
    let deadline = tokio::time::Instant::now() + DRAIN_FOR;
    loop {
        let frame = if over == 0 {
            body.frame().await
        } else {
            match tokio::time::timeout_at(deadline, body.frame()).await {
                Ok(f) => f,
                Err(_) => return Err(ReadError::TooLarge),
            }
        };
        let Some(frame) = frame else { break };
        let frame = frame.map_err(|e| ReadError::Failed(e.to_string()))?;
        let Ok(data) = frame.into_data() else { continue };
        if over == 0 && buf.len() + data.len() <= limit {
            if buf.capacity() - buf.len() < data.len() {
                buf.reserve(data.len());
                // `held` is what the buffer held before: its growth is charged.
                budget.grow(held, buf.capacity().saturating_sub(held.bytes())).map_err(ReadError::Busy)?;
            }
            buf.extend_from_slice(&data);
            continue;
        }
        if over == 0 {
            buf = bytes::BytesMut::new();
            *held = Reservation::none();
        }
        over += data.len();
        if over > DRAIN_BYTES {
            return Err(ReadError::TooLarge);
        }
    }
    if over > 0 { Err(ReadError::TooLarge) } else { Ok(buf.freeze()) }
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
async fn es_route(c: &Control, id: &str, method: Method, api_path: String, es_method: &str, es_path: String, scope: Option<&str>, query: Option<&str>, headers: &HeaderMap, body: Body, kind: EsBody) -> Result<Response, ControlError> {
    let cluster = c.get(id).await?;
    let mut held = Reservation::none();
    let bytes = match kind {
        EsBody::None => bytes::Bytes::new(),
        _ => read_limited(body, (cluster.max_body_mb.max(1) as usize) << 20, declared(headers), c.node.body_budget(), &mut held).await.map_err(|e| read_error(&cluster, e))?,
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
    // The node charges what it runs from here.
    drop(held);
    let (status, ct, body) = c.es(&cluster.id, es_method, &format!("{es_path}{q}"), scope, ct, bytes).await?;
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
            es_route(&c, &id, method, api_index_path(&id, &index, $rest), $es_method, format!("/{seg}{}", $es_rest), Some(index.as_str()), uri.query(), &headers, body, $kind).await
        }
    };
}

index_route!(index_get, "GET", "", "", true, EsBody::None);
index_route!(index_create, "PUT", "", "", false, EsBody::Json);
index_route!(index_delete, "DELETE", "", "", false, EsBody::None);
index_route!(index_mapping, "PUT", "/mapping", "/_mapping", false, EsBody::Json);
index_route!(index_mapping_get, "GET", "/mapping", "/_mapping", true, EsBody::None);
index_route!(index_settings, "GET", "/settings", "/_settings", true, EsBody::None);
// The engine refuses what nobody may set through `_settings` (private, internal and managed
// settings) to this route as to any client; the index scope keeps it to the one index.
index_route!(index_settings_put, "PUT", "/settings", "/_settings", false, EsBody::Json);
index_route!(index_refresh, "POST", "/refresh", "/_refresh", true, EsBody::None);
index_route!(index_analyze, "POST", "/analyze", "/_analyze", false, EsBody::Json);
index_route!(index_search, "POST", "/search", "/_search", true, EsBody::Json);
index_route!(doc_create, "POST", "/docs", "/_doc", false, EsBody::Json);
index_route!(index_bulk, "POST", "/bulk", "/_bulk", false, EsBody::Ndjson);
index_route!(index_delete_by_query, "POST", "/delete_by_query", "/_delete_by_query", true, EsBody::Json);

/// One document's route: `…/indices/{index}/<rest>/{doc}` to `/{index}/<es_rest>/{doc}`.
macro_rules! doc_route {
    ($name:ident, $es_method:literal, $rest:literal, $es_rest:literal, $kind:expr) => {
        async fn $name(State(c): State<AppState>, Path((id, index, doc)): Path<(String, String, String)>, method: Method, uri: axum::http::Uri, headers: HeaderMap, body: Body) -> Result<Response, ControlError> {
            let seg = index_segment(&index, false)?;
            let d = urlencoding::encode(&doc).into_owned();
            es_route(&c, &id, method, api_index_path(&id, &index, &format!("/{}/{d}", $rest)), $es_method, format!("/{seg}/{}/{d}", $es_rest), Some(index.as_str()), uri.query(), &headers, body, $kind).await
        }
    };
}

doc_route!(doc_get, "GET", "docs", "_doc", EsBody::None);
doc_route!(doc_put, "PUT", "docs", "_doc", EsBody::Json);
doc_route!(doc_delete, "DELETE", "docs", "_doc", EsBody::None);
doc_route!(doc_explain, "POST", "explain", "_explain", EsBody::Json);

// --- synonyms sets and query rulesets: cluster-wide objects (an index's analyzer names a set by
// `synonyms_set`; a `rule` query or retriever names rulesets), so they run as the cluster's
// owner, on paths these routes build: `/_synonyms/…` and `/_query_rules/…` only.

/// A route under `/v1/search/clusters/{id}/` for Elasticsearch's `es_api` (`_synonyms`,
/// `_query_rules`) with up to two names after it (`/{set}/{rule}`, `/{ruleset}`) and an
/// optional last segment (`_test`).
macro_rules! cluster_route {
    ($name:ident, $es_method:literal, $api:literal, $es_api:literal, 0, $kind:expr) => {
        async fn $name(State(c): State<AppState>, Path(id): Path<String>, method: Method, uri: axum::http::Uri, headers: HeaderMap, body: Body) -> Result<Response, ControlError> {
            let api_path = format!("/v1/search/clusters/{}/{}", urlencoding::encode(&id), $api);
            es_route(&c, &id, method, api_path, $es_method, format!("/{}", $es_api), None, uri.query(), &headers, body, $kind).await
        }
    };
    ($name:ident, $es_method:literal, $api:literal, $es_api:literal, 1, $suffix:literal, $es_suffix:literal, $kind:expr) => {
        async fn $name(State(c): State<AppState>, Path((id, a)): Path<(String, String)>, method: Method, uri: axum::http::Uri, headers: HeaderMap, body: Body) -> Result<Response, ControlError> {
            let a = name_segment(&a)?;
            let api_path = format!("/v1/search/clusters/{}/{}/{a}{}", urlencoding::encode(&id), $api, $suffix);
            es_route(&c, &id, method, api_path, $es_method, format!("/{}/{a}{}", $es_api, $es_suffix), None, uri.query(), &headers, body, $kind).await
        }
    };
    ($name:ident, $es_method:literal, $api:literal, $es_api:literal, 2, $kind:expr) => {
        async fn $name(State(c): State<AppState>, Path((id, a, b)): Path<(String, String, String)>, method: Method, uri: axum::http::Uri, headers: HeaderMap, body: Body) -> Result<Response, ControlError> {
            let (a, b) = (name_segment(&a)?, name_segment(&b)?);
            let api_path = format!("/v1/search/clusters/{}/{}/{a}/{b}", urlencoding::encode(&id), $api);
            es_route(&c, &id, method, api_path, $es_method, format!("/{}/{a}/{b}", $es_api), None, uri.query(), &headers, body, $kind).await
        }
    };
}

/// A synonyms set's, rule's or ruleset's id as a path segment: never empty, never one of
/// Elasticsearch's own names (`_…`), which would reach another API, and never `.` or `..`, which
/// a path normalized anywhere on the way would read as a dot segment (`/_synonyms/../books`).
fn name_segment(name: &str) -> Result<String, ControlError> {
    if name.is_empty() || name.starts_with('_') || name == "." || name == ".." {
        return Err(ControlError::Invalid(format!("invalid id [{name}]")));
    }
    Ok(urlencoding::encode(name).into_owned())
}

cluster_route!(synonyms_list, "GET", "synonyms", "_synonyms", 0, EsBody::None);
cluster_route!(synonyms_get, "GET", "synonyms", "_synonyms", 1, "", "", EsBody::None);
cluster_route!(synonyms_put, "PUT", "synonyms", "_synonyms", 1, "", "", EsBody::Json);
cluster_route!(synonyms_delete, "DELETE", "synonyms", "_synonyms", 1, "", "", EsBody::None);
cluster_route!(synonym_rule_get, "GET", "synonyms", "_synonyms", 2, EsBody::None);
cluster_route!(synonym_rule_put, "PUT", "synonyms", "_synonyms", 2, EsBody::Json);
cluster_route!(synonym_rule_delete, "DELETE", "synonyms", "_synonyms", 2, EsBody::None);
cluster_route!(rulesets_list, "GET", "query_rules", "_query_rules", 0, EsBody::None);
cluster_route!(ruleset_get, "GET", "query_rules", "_query_rules", 1, "", "", EsBody::None);
cluster_route!(ruleset_put, "PUT", "query_rules", "_query_rules", 1, "", "", EsBody::Json);
cluster_route!(ruleset_delete, "DELETE", "query_rules", "_query_rules", 1, "", "", EsBody::None);
cluster_route!(ruleset_test, "POST", "query_rules", "_query_rules", 1, "/test", "/_test", EsBody::Json);

/// `PUT /v1/search/clusters/{id}/repositories/{name}`: `PUT _snapshot/{name}` (Elasticsearch's
/// body and query), the only way a repository's credentials can be roll secrets references: the
/// caller must be able to reveal each, and is kept as their maker ([`crate::secret_refs`]). The
/// cluster's answer, unchanged.
async fn put_repository(State(c): State<AppState>, Path((id, name)): Path<(String, String)>, uri: axum::http::Uri, ext: Extensions, body: bytes::Bytes) -> Result<Response, ControlError> {
    let who = crate::secret_refs::Who::of(&ext);
    let (status, ct, body) = c.put_repository(&id, &name, uri.query(), body, &who).await?;
    let mut r = Response::new(Body::from(body));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    if let Ok(v) = axum::http::HeaderValue::from_str(&ct) {
        r.headers_mut().insert(axum::http::header::CONTENT_TYPE, v);
    }
    Ok(r)
}

#[derive(Deserialize)]
struct NewKey {
    name: String,
    expires_in_s: Option<i64>,
    /// `admin` (default), `data`, `read` or `append` ([`crate::control::KeyAccess`]).
    access: Option<String>,
    /// Index names or patterns the key reaches (default every index; not with `admin`).
    indices: Option<Vec<String>>,
}

async fn create_key(State(c): State<AppState>, Path(id): Path<String>, Json(k): Json<NewKey>) -> ApiResult<Value> {
    Ok(Json(c.create_key(&id, &k.name, k.expires_in_s, k.access.as_deref(), k.indices).await?))
}

async fn delete_key(State(c): State<AppState>, Path((id, key)): Path<(String, String)>) -> Result<StatusCode, ControlError> {
    c.delete_key(&id, &key).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Lists an Elasticsearch or OpenSearch cluster's indices for a migration into cluster `id`
/// (`crate::migrate`): fetched by roll, never by the browser; the credentials aren't kept.
async fn migrate_inspect(State(c): State<AppState>, Path(id): Path<String>, Json(b): Json<crate::migrate::InspectRequest>) -> Result<Response, ControlError> {
    // The answer goes out in chunks, charged until it's sent (or the connection closes).
    let answer = c.migrate_inspect(&id, &b.source).await?;
    Ok(([(axum::http::header::CONTENT_TYPE, "application/json")], Body::from_stream(answer.into_stream())).into_response())
}

/// Copies the picked indices' definitions into cluster `id` and starts a reindex from remote
/// for each (`wait_for_completion=false`: the answer lists the tasks).
async fn migrate(State(c): State<AppState>, Path(id): Path<String>, Json(b): Json<crate::migrate::MigrateRequest>) -> ApiResult<Value> {
    Ok(Json(c.migrate(&id, &b).await?))
}

/// `GET /v1/search/clusters/{id}/browser-keys`: the keys web pages ship, without their secrets.
async fn browser_keys(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    Ok(Json(json!({ "keys": c.browser_keys(&id).await? })))
}

/// `POST /v1/search/clusters/{id}/browser-keys`: a key a web page can ship (read-only, one
/// application or named indices, an optional filter, expiry and rate limit); shown once.
async fn create_browser_key(State(c): State<AppState>, Path(id): Path<String>, Json(k): Json<crate::control::NewBrowserKey>) -> ApiResult<Value> {
    Ok(Json(c.create_browser_key(&id, k).await?))
}

/// `DELETE /v1/search/clusters/{id}/browser-keys/{key}`: 204, or 202 on a deleted cluster (the
/// restore revokes it).
async fn revoke_browser_key(State(c): State<AppState>, Path((id, key)): Path<(String, String)>) -> Result<Response, ControlError> {
    Ok(match c.revoke_browser_key(&id, &key).await? {
        crate::control::Revoked::Now => StatusCode::NO_CONTENT.into_response(),
        crate::control::Revoked::AtRestore => {
            (StatusCode::ACCEPTED, Json(json!({ "revoked": "at restore", "note": "the cluster is deleted: restoring it revokes the key before it answers" }))).into_response()
        }
    })
}

/// The remote clusters a cluster holds credentials for (never the credentials).
async fn remotes(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    Ok(Json(json!({ "remotes": c.remotes(&id).await? })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkRemote {
    /// The remote cluster: another of the caller's own (id or name).
    cluster: String,
    skip_unavailable: Option<bool>,
    /// What the cross-cluster API key roll makes may search (Elasticsearch's `access`).
    access: Option<Value>,
    /// A cross-cluster API key the caller made on the remote instead (its `encoded` value).
    api_key: Option<String>,
}

/// Links a cluster to another of the same owner as its remote `alias` (cross-cluster search).
/// A remote the caller can't see, or another person's, answers 404 as a missing one does.
async fn link_remote(State(c): State<AppState>, caller: Caller, Path((id, alias)): Path<(String, String)>, Json(b): Json<LinkRemote>) -> ApiResult<Value> {
    if b.access.is_some() && b.api_key.is_some() {
        return Err(ControlError::Invalid("give access (roll makes the key) or api_key (yours), not both".into()));
    }
    Ok(Json(c.link_remote(&id, &alias, &b.cluster, b.skip_unavailable, b.access, b.api_key.as_deref(), |t| caller.sees(t)).await?))
}

async fn unlink_remote(State(c): State<AppState>, Path((id, alias)): Path<(String, String)>) -> Result<StatusCode, ControlError> {
    c.unlink_remote(&id, &alias).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct UsageQuery {
    days: Option<i64>,
}

async fn usage(State(c): State<AppState>, Path(id): Path<String>, Query(q): Query<UsageQuery>) -> ApiResult<Usage> {
    Ok(Json(c.usage(&id, q.days.unwrap_or(30).clamp(1, 366)).await?))
}

#[derive(Deserialize)]
struct MetricsQuery {
    range: Option<String>,
}

/// Request rate, latency percentiles, errors and rejections by kind (`?range=1h|24h|14d`). Runs
/// on the cluster's owner (`per_cluster`), which adds what it recorded since the last pass.
async fn metrics(State(c): State<AppState>, Path(id): Path<String>, Query(q): Query<MetricsQuery>) -> ApiResult<Value> {
    Ok(Json(c.metrics(&id, q.range.as_deref().unwrap_or("1h")).await?))
}

/// The search and indexing slow logs, newest first, as Elasticsearch's JSON lines.
async fn slowlog(State(c): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    Ok(Json(c.slowlog(&id).await?))
}

#[derive(Deserialize)]
struct AuditParams {
    /// Unix milliseconds, inclusive.
    from: Option<i64>,
    /// Unix milliseconds, exclusive.
    to: Option<i64>,
    /// Event names or categories, comma-separated.
    event: Option<String>,
    size: Option<usize>,
}

/// The cluster's security audit log, newest first (the caller's own cluster: `per_cluster`).
async fn audit(State(c): State<AppState>, Path(id): Path<String>, Query(q): Query<AuditParams>) -> ApiResult<Value> {
    let events = q.event.as_deref().unwrap_or_default().split(',').map(str::trim).filter(|e| !e.is_empty()).map(str::to_string).collect();
    let query = search_server::audit::AuditQuery { from_ms: q.from, to_ms: q.to, events, size: q.size.unwrap_or(100) };
    Ok(Json(c.audit(&id, &query).await?))
}

#[derive(Deserialize)]
struct AnalyticsQuery {
    index: Option<String>,
    range: Option<String>,
}

/// Search analytics (`?index=&range=24h|7d|30d`): top queries, queries with no results and
/// click-through. Runs on the cluster's owner (`per_cluster`), which adds what it recorded since
/// the last pass.
async fn analytics(State(c): State<AppState>, Path(id): Path<String>, Query(q): Query<AnalyticsQuery>) -> ApiResult<Value> {
    let index = q.index.as_deref().filter(|i| !i.is_empty());
    Ok(Json(c.analytics(&id, index, q.range.as_deref().unwrap_or("24h")).await?))
}

/// An index's query suggestions: settings, last build, heaviest suggestions (`crate::suggestions`).
async fn suggestions(State(c): State<AppState>, Path((id, index)): Path<(String, String)>) -> ApiResult<Value> {
    index_segment(&index, false)?;
    Ok(Json(c.suggestions(&id, &index).await?))
}

/// Rebuilds an index's suggestions index now.
async fn rebuild_suggestions(State(c): State<AppState>, Path((id, index)): Path<(String, String)>) -> ApiResult<Value> {
    index_segment(&index, false)?;
    Ok(Json(Box::pin(c.rebuild_suggestions(&id, &index)).await?))
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

    async fn read(body: Body, limit: usize, budget: &Arc<ByteBudget>) -> Result<bytes::Bytes, ReadError> {
        read_limited(body, limit, None, budget, &mut Reservation::none()).await
    }

    #[tokio::test]
    async fn bodies_within_the_limit_are_read_whole() {
        let (sent, budget) = (Arc::new(AtomicUsize::new(0)), ByteBudget::new(usize::MAX));
        let b = read(chunks(4, 1000, sent.clone()), 4000, &budget).await.unwrap();
        assert_eq!(b.len(), 4000);
        assert_eq!(read(Body::empty(), 0, &budget).await.unwrap().len(), 0);
        assert_eq!(budget.used(), 0, "given back");
        // Charged while it's read: a body past what the node has left is refused as it arrives.
        let node = ByteBudget::new(1500);
        let mut other = node.reserve(1000).unwrap();
        assert!(matches!(read(chunks(4, 1000, sent.clone()), 4000, &node).await, Err(ReadError::Busy(_))));
        other.shrink(1000);
        assert_eq!(node.used(), 0);
    }

    #[tokio::test]
    async fn an_oversized_body_is_read_to_its_end_before_the_refusal() {
        let (sent, budget) = (Arc::new(AtomicUsize::new(0)), ByteBudget::new(usize::MAX));
        assert_eq!(read(chunks(10, 1000, sent.clone()), 4500, &budget).await.unwrap_err(), ReadError::TooLarge);
        assert_eq!(sent.load(Ordering::SeqCst), 10, "the rest was drained, so the client sees the 413");
        // Past the drain allowance it stops reading (the connection then closes).
        let sent = Arc::new(AtomicUsize::new(0));
        let n = (DRAIN_BYTES >> 20) + 50;
        assert_eq!(read(chunks(n, 1 << 20, sent.clone()), 1 << 20, &budget).await.unwrap_err(), ReadError::TooLarge);
        assert!(sent.load(Ordering::SeqCst) < n);
    }

}
