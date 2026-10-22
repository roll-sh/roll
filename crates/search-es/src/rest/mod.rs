//! The REST front door: an HTTP request in, an Elasticsearch response out.
//!
//! [`handle`] does what Elasticsearch's `RestController` and `BaseRestHandler` do before and
//! after an action runs:
//!
//! 1. parse `Accept`/`Content-Type` (media types, `compatible-with`), failing with
//!    `media_type_header_exception` like Elasticsearch;
//! 2. route through the spec-generated trie (400 `no handler found`, 405 with `Allow`, `OPTIONS`);
//! 3. check the body's content type (406) and reject unknown query parameters with
//!    Elasticsearch's "did you mean" text;
//! 4. run the API handler;
//! 5. render the reply as JSON/YAML/CBOR (`format`, `Accept`, request type), `pretty`, with
//!    `filter_path` (not applied to errors), plus `X-elastic-product`, `Warning` and
//!    `X-Opaque-Id` headers.

pub mod params;

use crate::cluster::{Cluster, Dialect};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::routes::{self, Api, BodySpec, Method, RouteMatch, Routed};
use crate::xcontent::{self, MediaType, XContentType, filter::FilterPath};
use bytes::Bytes;
use parking_lot::Mutex;

/// An HTTP request as the server received it.
#[derive(Clone, Debug, Default)]
pub struct EsRequest {
    pub method: String,
    /// Path and query string, not decoded (`/my-index/_search?q=a%20b`).
    pub uri: String,
    /// Header names in any case.
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl EsRequest {
    pub fn new(method: &str, uri: &str) -> EsRequest {
        EsRequest { method: method.to_string(), uri: uri.to_string(), headers: Vec::new(), body: Bytes::new() }
    }

    pub fn json(method: &str, uri: &str, body: impl Into<Bytes>) -> EsRequest {
        EsRequest {
            method: method.to_string(),
            uri: uri.to_string(),
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.into(),
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    fn header_values(&self, name: &str) -> Vec<&str> {
        self.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).collect()
    }
}

/// The response to send.
#[derive(Clone, Debug)]
pub struct EsResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    /// A `HEAD` response: send `Content-Length` of `body` but not the body.
    pub head: bool,
}

impl EsResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    pub fn body_json(&self) -> Value {
        crate::json::parse(&self.body).ok().flatten().unwrap_or(Value::Null)
    }
}

/// What a handler produces.
pub enum Reply {
    /// A JSON-ish body, rendered per the request's format.
    Body(u16, Value),
    /// No body (`HEAD` exists checks).
    Empty(u16),
    /// A `_cat` table, rendered per `format`/`v`/`h`/`s`/`help`.
    Table(crate::api::cat::Table),
    /// Pre-rendered bytes with a content type.
    Raw(u16, String, Bytes),
}

impl Reply {
    pub fn ok(v: Value) -> EsResult<Reply> {
        Ok(Reply::Body(200, v))
    }

    pub fn acknowledged() -> EsResult<Reply> {
        Ok(Reply::Body(200, crate::obj! { "acknowledged" => true }))
    }
}

/// The request as handlers see it.
pub struct Ctx<'a> {
    pub cluster: &'a Cluster,
    pub method: Method,
    pub api: Api,
    pub route: RouteMatch,
    /// The path without the query string (as Elasticsearch prints it in messages).
    pub path: String,
    pub uri: String,
    params: Vec<(String, String)>,
    pub body: Bytes,
    pub body_type: Option<XContentType>,
    pub request: &'a EsRequest,
    pub compat: Option<u8>,
    /// The `compatible-with` version the response's content type carries.
    pub response_compat: Option<u8>,
    warnings: Mutex<Vec<String>>,
    pub now_ms: i64,
    pub human: bool,
}

impl<'a> Ctx<'a> {
    /// A path parameter (`{index}`) or query parameter.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.route.param(name).or_else(|| self.params.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.as_str()))
    }

    pub fn path_param(&self, name: &str) -> Option<&str> {
        self.route.param(name)
    }

    pub fn query(&self, name: &str) -> Option<&str> {
        self.params.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn has_query(&self, name: &str) -> bool {
        self.params.iter().any(|(k, _)| k == name)
    }

    pub fn warn(&self, msg: impl Into<String>) {
        // Requests from Elastic products get no deprecation warnings, and the deprecations are
        // Elasticsearch's (OpenSearch has its own, see `warn_dialect`).
        if self.request.header("x-elastic-product-origin").is_some() || self.cluster.dialect == Dialect::OpenSearch {
            return;
        }
        let msg = msg.into();
        let mut w = self.warnings.lock();
        if !w.contains(&msg) {
            w.push(msg);
        }
    }

    /// A warning in the cluster's dialect (`None` skips it for that dialect).
    pub fn warn_dialect(&self, elasticsearch: Option<String>, opensearch: Option<String>) {
        let msg = match self.cluster.dialect {
            Dialect::Elasticsearch => elasticsearch,
            Dialect::OpenSearch => opensearch,
        };
        let Some(msg) = msg else { return };
        if self.request.header("x-elastic-product-origin").is_some() {
            return;
        }
        let mut w = self.warnings.lock();
        if !w.contains(&msg) {
            w.push(msg);
        }
    }

    pub fn warn_all(&self, msgs: impl IntoIterator<Item = String>) {
        for m in msgs {
            self.warn(m);
        }
    }

    /// The parsed body (or the `source` parameter), `None` if there is none.
    pub fn body(&self) -> EsResult<Option<Value>> {
        if !self.body.is_empty() {
            return xcontent::parse_body(&self.body, self.body_type.unwrap_or(XContentType::Json));
        }
        if let Some(src) = self.query("source") {
            let Some(ty) = self.query("source_content_type") else {
                return Err(EsError::action_request_validation(&["source and source_content_type parameters are required".into()]));
            };
            let ty = xcontent::parse_media_type(ty)
                .ok()
                .flatten()
                .and_then(|m| m.xcontent())
                .ok_or_else(|| EsError::action_request_validation(&[format!("Unknown value for source_content_type [{ty}]")]))?;
            return xcontent::parse_body(src.as_bytes(), ty);
        }
        Ok(None)
    }

    /// The body as an object (`{}` when the body isn't an object, as Elasticsearch's map readers).
    pub fn body_map(&self) -> EsResult<Option<Map>> {
        if !self.body.is_empty() && self.body_type.unwrap_or(XContentType::Json) == XContentType::Json {
            let map = crate::json::parse_object(&self.body)?;
            return Ok(self.body.iter().any(|b| !b.is_ascii_whitespace()).then_some(map));
        }
        Ok(self.body()?.map(|v| match v {
            Value::Obj(m) => m,
            _ => Map::new(),
        }))
    }

    /// The raw body bytes for locating errors (JSON only).
    pub fn raw_json(&self) -> Option<&[u8]> {
        (self.body_type.unwrap_or(XContentType::Json) == XContentType::Json && !self.body.is_empty()).then_some(&self.body[..])
    }

    pub fn state(&self) -> std::sync::Arc<crate::meta::ClusterState> {
        self.cluster.state()
    }
}

/// Handles one request.
pub async fn handle(cluster: &Cluster, req: EsRequest) -> EsResponse {
    let mut resp = handle_inner(cluster, &req).await;
    finish_headers(&mut resp, &req, cluster.dialect);
    resp
}

/// Renders an error that happened before a cluster was found (authentication, limits) the way
/// Elasticsearch renders errors for this request: content negotiation, `pretty`,
/// `error_trace`, `X-elastic-product` (elasticsearch dialect) and `X-Opaque-Id`.
pub fn error_response(e: &EsError, req: &EsRequest, dialect: Dialect) -> EsResponse {
    let query = req.uri.split_once('?').map(|(_, q)| q).unwrap_or("");
    let params = parse_query(query);
    let pretty = flag(&params, "pretty").unwrap_or(false);
    let trace = flag(&params, "error_trace").unwrap_or(false);
    let head = req.method.eq_ignore_ascii_case("HEAD");
    let neg = negotiate(req).ok();
    let mut resp = render_error(e, neg.as_ref(), pretty, trace, Vec::new(), head, dialect);
    finish_headers(&mut resp, req, dialect);
    resp
}

fn finish_headers(resp: &mut EsResponse, req: &EsRequest, dialect: Dialect) {
    match dialect {
        Dialect::Elasticsearch => resp.headers.insert(0, ("X-elastic-product".into(), "Elasticsearch".into())),
        Dialect::OpenSearch => {
            resp.headers.insert(0, ("X-OpenSearch-Version".into(), format!("OpenSearch/{} (opensearch)", crate::version::OPENSEARCH_VERSION)));
            // OpenSearch names the charset of its JSON responses.
            for (k, v) in resp.headers.iter_mut() {
                if k.eq_ignore_ascii_case("content-type") && v == "application/json" {
                    *v = "application/json; charset=UTF-8".into();
                }
            }
        }
    }
    if let Some(o) = req.header("x-opaque-id") {
        resp.headers.push(("X-Opaque-Id".into(), o.to_string()));
    }
}

/// OpenSearch prints exception metadata in another order than Elasticsearch: the index first,
/// then the resource, then the index UUID.
fn opensearch_error_order(v: &mut Value) {
    const FIRST: &[&str] = &["root_cause", "type", "reason", "index", "resource.id", "resource.type", "index_uuid"];
    match v {
        Value::Obj(m) => {
            if m.contains_key("type") && m.contains_key("reason") {
                let mut out = crate::json::Map::with_capacity(m.len());
                for k in FIRST {
                    if let Some(x) = m.shift_remove(*k) {
                        out.insert((*k).to_string(), x);
                    }
                }
                for (k, x) in std::mem::take(m) {
                    out.insert(k, x);
                }
                *m = out;
            }
            for x in m.values_mut() {
                opensearch_error_order(x);
            }
        }
        Value::Arr(a) => a.iter_mut().for_each(opensearch_error_order),
        _ => {}
    }
}

fn simple_error(status: u16, msg: &str) -> Value {
    crate::obj! { "error" => msg, "status" => status as i64 }
}

fn render_simple(status: u16, body: Value, extra: Vec<(String, String)>, head: bool) -> EsResponse {
    let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
    headers.extend(extra);
    EsResponse { status, headers, body: Bytes::from(body.to_json()), head }
}

/// `MediaTypeHeaderException`. Elasticsearch reports it as its own root cause even when caused by
/// an Elasticsearch exception, so it is built as a plain exception for root-cause guessing.
fn media_error(headers: &str, cause: EsError) -> EsError {
    EsError::java(400, "media_type_header_exception", format!("Invalid media-type value on headers [{headers}]")).caused_by(cause)
}

struct Negotiated {
    accept: Option<MediaType>,
    content: Option<MediaType>,
    compat: Option<u8>,
}

fn negotiate(req: &EsRequest) -> Result<Negotiated, EsError> {
    let one = |name: &str| -> Result<Option<&str>, EsError> {
        let vals = req.header_values(name);
        match vals.len() {
            0 => Ok(None),
            1 if vals[0].trim().is_empty() => Err(media_error(name, EsError::illegal_argument(format!("Header [{name}] cannot be empty.")))),
            1 => Ok(Some(vals[0])),
            _ if name == "Accept" => Ok(Some(vals[0])),
            _ => Err(media_error(name, EsError::illegal_argument(format!("Incorrect header [{name}]. Only one value should be provided")))),
        }
    };
    let accept = match one("Accept")? {
        Some(h) => xcontent::parse_accept(h).map_err(|e| media_error("Accept", EsError::illegal_argument(e)))?,
        None => None,
    };
    let content = match one("Content-Type")? {
        Some(h) => xcontent::parse_media_type(h).map_err(|e| media_error("Content-Type", EsError::illegal_argument(e)))?,
        None => None,
    };
    let has_content = !req.body.is_empty();
    let parse_v = |m: &Option<MediaType>| -> Result<Option<u8>, EsError> {
        match m {
            Some(mt) => mt.compatible_with().map_err(|e| media_error("Content-Type, Accept", EsError::java(400, "number_format_exception", e))),
            None => Ok(None),
        }
    };
    let av = parse_v(&accept)?;
    let cv = parse_v(&content)?;
    let shown = |m: &Option<MediaType>| m.as_ref().map(|m| m.original.clone()).unwrap_or_else(|| "null".into());
    let accept_v = av.unwrap_or(9);
    // roll accepts 7 (served as 8) in addition to Elasticsearch's 8 and 9.
    if !(7..=9).contains(&accept_v) {
        return Err(media_error(
            "Content-Type, Accept",
            EsError::status(400, format!("Accept version must be either version 9 or 8, but found {accept_v}. Accept={}", shown(&accept))),
        ));
    }
    if has_content {
        let content_v = cv.unwrap_or(9);
        if !(7..=9).contains(&content_v) {
            return Err(media_error(
                "Content-Type, Accept",
                EsError::status(400, format!("Content-Type version must be either version 9 or 8, but found {content_v}. Content-Type={}", shown(&content))),
            ));
        }
        if av.is_some() && cv.is_some() && content_v != accept_v {
            return Err(media_error(
                "Content-Type, Accept",
                EsError::status(
                    400,
                    format!(
                        "A compatible version is required on both Content-Type and Accept headers if either one has requested a compatible version and the compatible versions must match. Accept={}, Content-Type={}",
                        shown(&accept),
                        shown(&content)
                    ),
                ),
            ));
        }
    }
    let compat = av.or(if has_content { cv } else { None });
    Ok(Negotiated { accept, content, compat })
}

/// Parses `a=1&b=x%20y` leniently (malformed escapes kept), for rendering errors.
pub fn parse_query(q: &str) -> Vec<(String, String)> {
    split_query(q, &mut |s| Ok(routes::percent_decode(s, true))).unwrap_or_default()
}

/// `RestUtils.decodeQueryString`: `&` or `;` separate parameters, the first `=` splits name and
/// value, a name without `=` gets an empty value, `+` is a space; later values win like
/// Elasticsearch's map. Malformed escapes fail with `decodeComponent`'s message.
pub fn parse_query_strict(q: &str) -> Result<Vec<(String, String)>, String> {
    split_query(q, &mut |s| routes::decode_component(s, true))
}

fn split_query(q: &str, decode: &mut dyn FnMut(&str) -> Result<String, String>) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut name: Option<String> = None;
    let mut pos = 0;
    let b = q.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        if c == b'=' && name.is_none() {
            if pos != i {
                name = Some(decode(&q[pos..i])?);
            }
            pos = i + 1;
        } else if c == b'&' || c == b';' {
            match name.take() {
                None if pos != i => out.push((decode(&q[pos..i])?, String::new())),
                Some(n) => out.push((n, decode(&q[pos..i])?)),
                None => {}
            }
            pos = i + 1;
        }
    }
    if pos != b.len() {
        match name {
            None => out.push((decode(&q[pos..])?, String::new())),
            Some(n) => out.push((n, decode(&q[pos..])?)),
        }
    } else if let Some(n) = name {
        out.push((n, String::new()));
    }
    Ok(out)
}

/// `RestRequest.paramAsBoolean` for the framework flags: absent is `false`, empty is `true`,
/// anything but `true`/`false` fails.
fn flag(params: &[(String, String)], name: &str) -> Result<bool, EsError> {
    match params.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.as_str()) {
        None => Ok(false),
        Some("" | "true") => Ok(true),
        Some("false") => Ok(false),
        Some(v) => Err(EsError::illegal_argument(format!("Failed to parse value [{v}] as only [true] or [false] are allowed."))),
    }
}

async fn handle_inner(cluster: &Cluster, req: &EsRequest) -> EsResponse {
    let (raw_path, query) = req.uri.split_once('?').unwrap_or((&req.uri, ""));
    let raw_path = if raw_path.is_empty() { "/" } else { raw_path };
    let head_req = req.method.eq_ignore_ascii_case("HEAD");
    // What `RestRequest` creation and `RestController` check before routing: the query string
    // and path escapes, the channel's `pretty`/`human` flags, then `error_trace`.
    let params = match parse_query_strict(query) {
        Ok(p) => p,
        Err(msg) => {
            let e = EsError::java(400, "bad_parameter_exception", format!("java.lang.IllegalArgumentException: {msg}")).caused_by(EsError::illegal_argument(msg));
            return render_error(&e, negotiate(req).ok().as_ref(), false, false, Vec::new(), head_req, cluster.dialect);
        }
    };
    if let Err(msg) = routes::decode_component(raw_path, false) {
        return render_error(&EsError::illegal_argument(msg), negotiate(req).ok().as_ref(), false, false, Vec::new(), head_req, cluster.dialect);
    }
    let (pretty, human) = match (flag(&params, "pretty"), flag(&params, "human")) {
        (Ok(p), Ok(h)) => (p, h),
        (Err(e), _) | (_, Err(e)) => return render_error(&e, negotiate(req).ok().as_ref(), false, false, Vec::new(), head_req, cluster.dialect),
    };
    let error_trace = match flag(&params, "error_trace") {
        Ok(t) => t,
        Err(e) => return render_simple(400, simple_error(400, e.reason()), Vec::new(), head_req),
    };
    // Single-valued headers Elasticsearch refuses to see twice (`RestRequest.header`).
    if req.header_values("X-Opaque-Id").len() > 1 {
        let e = EsError::illegal_argument("multiple values for single-valued header [X-Opaque-Id].");
        return render_error(&e, negotiate(req).ok().as_ref(), pretty, error_trace, Vec::new(), head_req, cluster.dialect);
    }

    let neg = match negotiate(req) {
        Ok(n) => n,
        Err(e) => return render_error(&e, None, pretty, error_trace, Vec::new(), head_req, cluster.dialect),
    };

    let Some(method) = Method::parse(&req.method.to_ascii_uppercase()) else {
        let allowed = routes::allowed_methods(cluster.dialect, raw_path);
        let list: Vec<&str> = allowed.iter().map(|m| m.as_str()).collect();
        let msg = format!("Unexpected HTTP method: {}, allowed: [{}]", req.method, list.join(", "));
        return render_simple(405, simple_error(405, &msg), vec![("Allow".into(), list.join(","))], false);
    };

    let route = match routes::route_in(cluster.dialect, method, raw_path) {
        Routed::Found(r) => r,
        Routed::MethodNotAllowed(allowed) => {
            let list: Vec<&str> = allowed.iter().map(|m| m.as_str()).collect();
            if method == Method::Options {
                return EsResponse {
                    status: 200,
                    headers: vec![("Allow".into(), list.join(",")), ("content-type".into(), "text/plain; charset=UTF-8".into())],
                    body: Bytes::new(),
                    head: false,
                };
            }
            let msg = format!("Incorrect HTTP method for uri [{}] and method [{method}], allowed: [{}]", req.uri, list.join(", "));
            return render_simple(405, simple_error(405, &msg), vec![("Allow".into(), list.join(","))], head_req);
        }
        Routed::NotFound => {
            if method == Method::Options {
                return EsResponse { status: 200, headers: vec![("content-type".into(), "text/plain; charset=UTF-8".into())], body: Bytes::new(), head: false };
            }
            let msg = format!("no handler found for uri [{}] and method [{method}]", req.uri);
            return render_simple(400, crate::obj! { "error" => msg }, Vec::new(), head_req);
        }
    };

    // The body's content type.
    let mut body_type = None;
    if !req.body.is_empty() {
        let ct_headers = req.header_values("Content-Type");
        match neg.content.as_ref().and_then(|m| m.xcontent()) {
            Some(t) => body_type = Some(t),
            None => {
                let msg = if ct_headers.is_empty() {
                    "Content-Type header is missing".to_string()
                } else {
                    format!("Content-Type header [{}] is not supported", ct_headers.join(","))
                };
                return render_simple(406, simple_error(406, &msg), Vec::new(), head_req);
            }
        }
    }

    let path = routes::percent_decode(raw_path, false);
    let ctx = Ctx {
        cluster,
        method,
        api: route.api,
        path: path.clone(),
        uri: req.uri.clone(),
        route,
        params,
        body: req.body.clone(),
        body_type,
        request: req,
        compat: neg.compat,
        response_compat: compat_of(Some(&neg)),
        warnings: Mutex::new(Vec::new()),
        now_ms: crate::time::now_millis(),
        human,
    };

    let result = match params::check_params(&ctx) {
        Err(e) => Err(e),
        Ok(()) => {
            if !req.body.is_empty() && ctx.api.spec_in(cluster.dialect).body == BodySpec::None && !body_ignorable(ctx.api) {
                Err(EsError::illegal_argument(format!("request [{} {}] does not support having a body", method, path)))
            } else {
                crate::api::dispatch(&ctx).await
            }
        }
    };
    let warnings = warning_headers(&ctx.warnings.lock(), cluster.dialect);
    match result {
        // `GET /` prints pretty unless `pretty` is given (`RestMainAction`).
        Ok(reply) => {
            let pretty = pretty || (matches!(ctx.api, Api::Info | Api::Ping) && !ctx.has_query("pretty"));
            render_reply(&ctx, &neg, reply, pretty, warnings, head_req)
        }
        Err(e) => render_error(&e, Some(&neg), pretty, error_trace, warnings, head_req, cluster.dialect),
    }
}

/// APIs whose handlers read a body even though the spec doesn't declare one.
fn body_ignorable(api: Api) -> bool {
    matches!(api, Api::IndicesGet | Api::Info | Api::Ping)
}

fn warning_headers(warnings: &[String], dialect: Dialect) -> Vec<(String, String)> {
    let prefix = match dialect {
        Dialect::Elasticsearch => crate::version::warning_prefix(),
        Dialect::OpenSearch => crate::version::opensearch_warning_prefix(),
    };
    warnings.iter().map(|w| ("Warning".to_string(), format!("{prefix} \"{}\"", params::escape_warning(w)))).collect()
}

fn response_type(ctx_format: Option<&str>, neg: Option<&Negotiated>) -> XContentType {
    if let Some(f) = ctx_format
        && let Some(t) = XContentType::from_format(f)
    {
        return t;
    }
    if let Some(n) = neg {
        if let Some(t) = n.accept.as_ref().and_then(|a| a.xcontent()) {
            return t;
        }
        if let Some(t) = n.content.as_ref().and_then(|c| c.xcontent()) {
            return t;
        }
    }
    XContentType::Json
}

fn compat_of(neg: Option<&Negotiated>) -> Option<u8> {
    let n = neg?;
    // The response is versioned when the request asked for a version.
    n.accept.as_ref().and_then(|a| a.compatible_with().ok().flatten()).or(n.compat.filter(|_| n.accept.is_none()))
}

fn render_error(e: &EsError, neg: Option<&Negotiated>, pretty: bool, trace: bool, warnings: Vec<(String, String)>, head: bool, dialect: Dialect) -> EsResponse {
    let ty = response_type(None, neg);
    let ty = if ty == XContentType::Smile { XContentType::Json } else { ty };
    let mut v = e.to_body_with(trace, dialect == Dialect::OpenSearch);
    if dialect == Dialect::OpenSearch {
        opensearch_error_order(&mut v);
    }
    let body = xcontent::render(&v, ty, pretty);
    let mut headers = vec![("content-type".to_string(), ty.content_type(compat_of(neg)))];
    for (k, vs) in e.headers() {
        for v in vs {
            headers.push((k.clone(), v.clone()));
        }
    }
    headers.extend(warnings);
    EsResponse { status: e.status, headers, body: Bytes::from(body), head }
}

fn render_reply(ctx: &Ctx, neg: &Negotiated, reply: Reply, pretty: bool, warnings: Vec<(String, String)>, head: bool) -> EsResponse {
    let mut headers = Vec::new();
    let (status, body) = match reply {
        Reply::Empty(status) => {
            headers.push(("content-type".to_string(), "text/plain; charset=UTF-8".to_string()));
            (status, Bytes::new())
        }
        Reply::Raw(status, ct, bytes) => {
            headers.push(("content-type".to_string(), ct));
            (status, bytes)
        }
        Reply::Table(table) => match crate::api::cat::render(ctx, &table, neg.accept.as_ref()) {
            Ok((ct, bytes)) => {
                headers.push(("content-type".to_string(), ct));
                (200, bytes)
            }
            Err(e) => return render_error(&e, Some(neg), pretty, false, warnings, head, ctx.cluster.dialect),
        },
        Reply::Body(status, v) => {
            let ty = response_type(ctx.query("format"), Some(neg));
            let ty = if ty == XContentType::Smile { XContentType::Json } else { ty };
            let v = match ctx.query("filter_path").map(FilterPath::parse) {
                Some(Ok(Some(fp))) => fp.apply(v),
                Some(Err(msg)) => return render_error(&EsError::illegal_argument(msg), Some(neg), pretty, false, warnings, head, ctx.cluster.dialect),
                _ => v,
            };
            headers.push(("content-type".to_string(), ty.content_type(compat_of(Some(neg)))));
            (status, Bytes::from(xcontent::render(&v, ty, pretty)))
        }
    };
    headers.extend(warnings);
    EsResponse { status, headers, body, head }
}

#[cfg(test)]
mod tests;
