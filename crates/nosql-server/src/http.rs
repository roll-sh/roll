//! The data API the dashboard uses: CQL over JSON, the schema, and table pages.
//!
//! - `POST /cql` with `{"query": "…", "keyspace"?, "page_state"?, "page_size"?, "trace"?}` runs
//!   one statement. Rows come back twice: `rows` as cqlsh prints the values, `values` as JSON
//!   (`SELECT JSON`'s encoding). `page_state` (base64) fetches the next page.
//! - `GET /schema`: keyspaces with their replication, tables (columns, keys, options), types,
//!   views and indexes; `describe` holds `DESCRIBE SCHEMA` when the engine supports it.
//! - `GET /tables/{keyspace}/{table}?page_size=&page_state=`: a page of `SELECT *`.
//! - Auth: `Authorization: Bearer <db>_<secret>` or `?_token=<db>_<secret>` (a database's
//!   [`DbSpec::tokens`](crate::node::DbSpec::tokens)); a directory without tokens is open.
//!
//! Errors are `{"error": "…"}` (plus `code` and `code_name` for CQL errors) with 400, 401,
//! 404, 500 or 503, as in `redis_server::http`. Follows its auth and CORS handling.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::body::Bytes as AxBytes;
use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use nosql_engine as eng;
use nosql_types::CqlType;
use serde::Deserialize;
use serde_json::{Value as Json, json};
use tokio::net::TcpListener;

use crate::conn::{Answer, Call, call, call_traced};
use crate::directory::{Directory, Lease, Metrics};

/// Rows per page unless the request says otherwise, and the most it may ask for.
pub const DEFAULT_PAGE_SIZE: i32 = 100;
pub const MAX_PAGE_SIZE: i32 = 10_000;

#[derive(Clone)]
pub struct HttpState {
    pub dir: Arc<dyn Directory>,
}

pub fn router(dir: Arc<dyn Directory>) -> Router {
    Router::new()
        .route("/cql", post(cql).options(preflight))
        .route("/schema", get(schema).options(preflight))
        .route("/tables/{keyspace}/{table}", get(table_page).options(preflight))
        .fallback(fallback)
        .layer(axum::extract::DefaultBodyLimit::max(4 << 20))
        .with_state(HttpState { dir })
}

pub async fn serve_http(listener: TcpListener, dir: Arc<dyn Directory>) -> anyhow::Result<()> {
    axum::serve(listener, router(dir).into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

/// Start the API on an ephemeral port (tests, embedding).
pub async fn start(dir: Arc<dyn Directory>) -> anyhow::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { serve_http(listener, dir).await });
    Ok(addr)
}

/// The client's address, when the server was started with connect info.
pub struct Peer(pub Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for Peer {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Peer(parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0)))
    }
}

fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    cors((status, axum::Json(json!({ "error": msg.into() }))).into_response())
}

fn cors(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, header::HeaderValue::from_static("*"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, header::HeaderValue::from_static("authorization, content-type"));
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, header::HeaderValue::from_static("GET, POST, OPTIONS"));
    r
}

async fn preflight() -> Response {
    cors(StatusCode::NO_CONTENT.into_response())
}

async fn fallback(method: Method) -> Response {
    if method == Method::OPTIONS {
        return cors(StatusCode::NO_CONTENT.into_response());
    }
    error(StatusCode::NOT_FOUND, "not found")
}

fn bearer(h: &HeaderMap) -> Option<String> {
    let v = h.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = v.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        return Some(rest.trim().to_string());
    }
    if scheme.eq_ignore_ascii_case("basic") {
        // `user:password`: the password is the token.
        let raw = B64.decode(rest.trim()).ok()?;
        let s = String::from_utf8(raw).ok()?;
        return Some(s.split_once(':').map_or(s.clone(), |(_, p)| p.to_string()));
    }
    None
}

/// The lease of the database the request's token grants.
async fn authorize(st: &HttpState, headers: &HeaderMap, query_token: Option<&str>) -> Result<Lease, Response> {
    let token = bearer(headers).or_else(|| query_token.map(str::to_string));
    let Some(spec) = st.dir.route_token(token.as_deref()).await else {
        return Err(error(StatusCode::UNAUTHORIZED, "Unauthorized"));
    };
    st.dir.open(&spec).await.map_err(|e| error(StatusCode::SERVICE_UNAVAILABLE, e.to_string()))
}

#[derive(Debug, Default, Deserialize)]
pub struct CqlRequest {
    pub query: String,
    #[serde(default)]
    pub keyspace: Option<String>,
    #[serde(default)]
    pub page_state: Option<String>,
    #[serde(default)]
    pub page_size: Option<i32>,
    #[serde(default)]
    pub trace: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    pub page_size: Option<i32>,
    #[serde(default)]
    pub page_state: Option<String>,
    #[serde(default)]
    pub _token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct TokenQuery {
    #[serde(default)]
    pub _token: Option<String>,
}

/// The engine session of an HTTP request: the database's first platform role, if any.
fn session(lease: &Lease, peer: Option<SocketAddr>, keyspace: Option<String>) -> eng::Session {
    let client = eng::ClientInfo { address: peer, driver_name: Some("roll http".into()), protocol_version: 4, ..Default::default() };
    let mut s = lease.db().connect(client);
    s.state.user = lease.spec.credentials.first().map(|c| c.role.clone());
    s.state.keyspace = keyspace;
    s
}

fn json_response(lease: &Lease, status: StatusCode, body: &Json) -> Response {
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    Metrics::add(&lease.metrics.bytes_out, bytes.len() as u64);
    if status.is_client_error() || status.is_server_error() {
        Metrics::add(&lease.metrics.errors, 1);
    }
    cors((status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response())
}

fn count(lease: &Lease, body_len: usize) {
    let m = &lease.metrics;
    Metrics::add(&m.http_requests, 1);
    Metrics::add(&m.requests, 1);
    Metrics::add(&m.bytes_in, body_len as u64);
}

async fn cql(
    State(st): State<HttpState>,
    Peer(peer): Peer,
    Query(q): Query<TokenQuery>,
    headers: HeaderMap,
    body: AxBytes,
) -> Response {
    let lease = match authorize(&st, &headers, q._token.as_deref()).await {
        Ok(l) => l,
        Err(r) => return r,
    };
    count(&lease, body.len());
    let req: CqlRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return json_response(&lease, StatusCode::BAD_REQUEST, &json!({ "error": format!("invalid request: {e}") })),
    };
    let paging_state = match decode_page_state(req.page_state.as_deref()) {
        Ok(p) => p,
        Err(r) => return json_response(&lease, StatusCode::BAD_REQUEST, &r),
    };
    let params = eng::QueryParams { page_size: Some(page_size(req.page_size)), paging_state, ..Default::default() };
    let started = Instant::now();
    let s = session(&lease, peer, req.keyspace.clone());
    let (s, answer) = call_traced(s, Call::Query(req.query.clone(), params), req.trace).await;
    let (status, mut body) = render(&lease, answer);
    if let Some(o) = body.as_object_mut() {
        o.insert("elapsed_ms".into(), json!(started.elapsed().as_secs_f64() * 1000.0));
        if let Some(ks) = s.and_then(|s| s.state.keyspace) {
            o.insert("keyspace".into(), json!(ks));
        }
    }
    json_response(&lease, status, &body)
}

fn page_size(n: Option<i32>) -> i32 {
    match n {
        Some(n) if n > 0 => n.min(MAX_PAGE_SIZE),
        _ => DEFAULT_PAGE_SIZE,
    }
}

fn decode_page_state(s: Option<&str>) -> Result<Option<Bytes>, Json> {
    match s {
        None | Some("") => Ok(None),
        Some(s) => B64.decode(s).map(|b| Some(Bytes::from(b))).map_err(|_| json!({ "error": "invalid page_state" })),
    }
}

/// An engine answer as JSON.
fn render(lease: &Lease, answer: Answer) -> (StatusCode, Json) {
    let dialect = lease.db().dialect();
    match answer {
        Answer::Response(Ok(r)) => {
            let mut out = match &r.body {
                eng::ResponseBody::Void => json!({ "kind": "void" }),
                eng::ResponseBody::SetKeyspace(ks) => json!({ "kind": "set_keyspace", "keyspace": ks }),
                eng::ResponseBody::SchemaChange(c) => json!({
                    "kind": "schema_change",
                    "schema_change": {
                        "change": c.change.name(),
                        "target": c.target.name(),
                        "keyspace": c.keyspace,
                        "name": c.name,
                        "arg_types": c.arg_types,
                    }
                }),
                eng::ResponseBody::Rows(rows) => rows_json(rows, dialect),
            };
            if let Some(o) = out.as_object_mut() {
                o.insert("warnings".into(), json!(r.warnings));
                o.insert("tracing_id".into(), json!(r.tracing_id.map(|t| nosql_types::timeuuid::format_uuid(&t))));
            }
            (StatusCode::OK, out)
        }
        Answer::Response(Err(e)) | Answer::Prepared(Err(e)) | Answer::Auth(Err(e)) => {
            let status = if e.code == eng::ErrorCode::Server { StatusCode::INTERNAL_SERVER_ERROR } else { StatusCode::BAD_REQUEST };
            let code = e.code.code();
            (
                status,
                json!({
                    "error": e.message,
                    "code": code,
                    "code_name": nosql_proto::code::name(code),
                    "warnings": e.warnings,
                }),
            )
        }
        Answer::Prepared(Ok(_)) | Answer::Auth(Ok(_)) => (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": "unexpected answer" })),
    }
}

/// Columns with their CQL types, rows as cqlsh shows them and as JSON values.
pub fn rows_json(rows: &eng::Rows, dialect: nosql_types::Dialect) -> Json {
    let cols = &rows.metadata.columns;
    let columns: Vec<Json> = cols
        .iter()
        .map(|c| json!({ "name": c.name, "type": nosql_types::cql_name_in(&c.ty, dialect), "keyspace": c.keyspace, "table": c.table }))
        .collect();
    let mut shown = Vec::with_capacity(rows.rows.len());
    let mut values = Vec::with_capacity(rows.rows.len());
    for row in &rows.rows {
        let mut s = Vec::with_capacity(row.len());
        let mut v = Vec::with_capacity(row.len());
        for (i, cell) in row.iter().enumerate() {
            let ty = cols.get(i).map(|c| &c.ty);
            match (cell, ty) {
                (Some(b), Some(ty)) => {
                    s.push(Json::String(nosql_types::to_cqlsh(ty, b)));
                    v.push(json_value(ty, b, dialect));
                }
                (Some(b), None) => {
                    s.push(Json::String(format!("0x{}", hex(b))));
                    v.push(Json::Null);
                }
                (None, _) => {
                    s.push(Json::Null);
                    v.push(Json::Null);
                }
            }
        }
        shown.push(Json::Array(s));
        values.push(Json::Array(v));
    }
    json!({
        "kind": "rows",
        "columns": columns,
        "rows": shown,
        "values": values,
        "page_state": rows.paging_state.as_ref().map(|p| B64.encode(p)),
    })
}

/// A value in `SELECT JSON`'s encoding; numbers JSON can't hold exactly stay strings.
fn json_value(ty: &CqlType, b: &[u8], dialect: nosql_types::Dialect) -> Json {
    let Ok(text) = nosql_types::to_json(ty, b, dialect) else { return Json::String(format!("0x{}", hex(b))) };
    match serde_json::from_str::<Json>(&text) {
        Ok(Json::Number(n)) if n.to_string() != text => Json::String(text),
        Ok(v) => v,
        Err(_) => Json::String(text),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

async fn schema(State(st): State<HttpState>, Peer(peer): Peer, Query(q): Query<TokenQuery>, headers: HeaderMap) -> Response {
    let lease = match authorize(&st, &headers, q._token.as_deref()).await {
        Ok(l) => l,
        Err(r) => return r,
    };
    count(&lease, 0);
    let mut body = schema_json(lease.db());
    let (_, answer) = call(session(&lease, peer, None), Call::Query("DESCRIBE SCHEMA".into(), eng::QueryParams::default())).await;
    let describe = match answer {
        Answer::Response(Ok(r)) => r.rows().map(describe_text),
        _ => None,
    };
    if let Some(o) = body.as_object_mut() {
        o.insert("describe".into(), json!(describe));
    }
    json_response(&lease, StatusCode::OK, &body)
}

/// `DESCRIBE` rows (`keyspace_name, type, name, create_statement`) joined as cqlsh prints them.
fn describe_text(rows: &eng::Rows) -> String {
    let idx = rows.metadata.columns.iter().position(|c| c.name == "create_statement").unwrap_or(rows.metadata.columns.len().saturating_sub(1));
    let mut out = String::new();
    for row in &rows.rows {
        if let Some(Some(b)) = row.get(idx) {
            out.push_str(&String::from_utf8_lossy(b));
            out.push_str("\n\n");
        }
    }
    out
}

/// The database's schema from the engine's snapshot.
pub fn schema_json(db: &eng::Database) -> Json {
    let schema = db.schema();
    let d = db.dialect();
    let keyspaces: Vec<Json> = schema
        .keyspaces
        .values()
        .map(|ks| {
            let tables: Vec<Json> = ks.tables.values().filter(|t| t.view.is_none()).map(|t| table_json(t, d)).collect();
            let views: Vec<Json> = ks
                .tables
                .values()
                .filter_map(|t| {
                    let v = t.view.as_ref()?;
                    let mut j = table_json(t, d);
                    if let Some(o) = j.as_object_mut() {
                        o.insert("base_table".into(), json!(v.base_table_name));
                        o.insert("where_clause".into(), json!(v.where_clause));
                    }
                    Some(j)
                })
                .collect();
            let types: Vec<Json> = ks
                .types
                .values()
                .map(|u| {
                    let fields: Vec<Json> = u.fields.iter().map(|(n, t)| json!({ "name": n, "type": nosql_types::cql_name_in(t, d) })).collect();
                    json!({ "name": u.name, "fields": fields })
                })
                .collect();
            let functions: Vec<Json> =
                ks.functions.iter().map(|f| json!({ "name": f.name, "argument_types": f.argument_types, "return_type": f.return_type, "language": f.language })).collect();
            let aggregates: Vec<Json> =
                ks.aggregates.iter().map(|a| json!({ "name": a.name, "argument_types": a.argument_types, "return_type": a.return_type })).collect();
            json!({
                "name": ks.name,
                "system": ks.is_builtin(),
                "durable_writes": ks.durable_writes,
                "replication": ks.replication.as_map(),
                "tablets": ks.tablets,
                "tables": tables,
                "views": views,
                "types": types,
                "functions": functions,
                "aggregates": aggregates,
            })
        })
        .collect();
    json!({
        "dialect": d.name(),
        "version": nosql_types::timeuuid::format_uuid(&schema.version),
        "keyspaces": keyspaces,
    })
}

fn table_json(t: &eng::schema::Table, d: nosql_types::Dialect) -> Json {
    let columns: Vec<Json> = t
        .columns
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "type": nosql_types::cql_name_in(&c.ty, d),
                "kind": c.kind.name(),
                "position": c.position,
                "clustering_order": c.clustering_order(d),
            })
        })
        .collect();
    let pk: Vec<&str> = t.partition_key_columns().map(|c| c.name.as_str()).collect();
    let ck: Vec<&str> = t.clustering_columns().map(|c| c.name.as_str()).collect();
    let indexes: Vec<Json> = t.indexes.iter().map(|i| json!({ "name": i.name, "kind": i.kind, "options": i.options })).collect();
    json!({
        "name": t.name,
        "id": nosql_types::timeuuid::format_uuid(&t.id),
        "columns": columns,
        "partition_key": pk,
        "clustering_key": ck,
        "indexes": indexes,
        "options": {
            "comment": t.params.comment,
            "compaction": t.params.compaction,
            "compression": t.params.compression,
            "default_time_to_live": t.params.default_time_to_live,
            "gc_grace_seconds": t.params.gc_grace_seconds,
        },
    })
}

async fn table_page(
    State(st): State<HttpState>,
    Peer(peer): Peer,
    Path((keyspace, table)): Path<(String, String)>,
    Query(q): Query<PageQuery>,
    headers: HeaderMap,
) -> Response {
    let lease = match authorize(&st, &headers, q._token.as_deref()).await {
        Ok(l) => l,
        Err(r) => return r,
    };
    count(&lease, 0);
    if lease.db().schema().table(&keyspace, &table).is_none() {
        return json_response(&lease, StatusCode::NOT_FOUND, &json!({ "error": format!("table {keyspace}.{table} does not exist") }));
    }
    let paging_state = match decode_page_state(q.page_state.as_deref()) {
        Ok(p) => p,
        Err(r) => return json_response(&lease, StatusCode::BAD_REQUEST, &r),
    };
    let d = lease.db().dialect();
    let text = format!("SELECT * FROM {}.{}", nosql_types::quote_identifier(&keyspace, d), nosql_types::quote_identifier(&table, d));
    let params = eng::QueryParams { page_size: Some(page_size(q.page_size)), paging_state, ..Default::default() };
    let (_, answer) = call(session(&lease, peer, None), Call::Query(text, params)).await;
    let (status, body) = render(&lease, answer);
    json_response(&lease, status, &body)
}
