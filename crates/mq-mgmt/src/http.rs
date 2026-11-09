//! HTTP plumbing shared by the API handlers: errors in RabbitMQ's format, requests, query
//! parameters, Basic auth, and list post-processing (filtering, sorting, pagination, columns).

use crate::stats::Ages;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use bytes::Bytes;
use mq_core::AmqpError;
use mq_core::auth::User;
use mq_core::error::{ACCESS_REFUSED, INTERNAL_ERROR, NOT_ALLOWED, NOT_FOUND};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde_json::{Map, Value, json};
use std::cmp::Ordering;

/// An error response: `{"error": ..., "reason": ...}`.
#[derive(Debug)]
pub(crate) struct ApiError {
    pub status: StatusCode,
    pub error: String,
    pub reason: String,
    pub body: Option<Value>,
}

pub(crate) type ApiResult<T = Response> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, error: &str, reason: impl Into<String>) -> Self {
        ApiError { status, error: error.into(), reason: reason.into(), body: None }
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "Object Not Found", "Not Found")
    }

    pub fn bad_request(reason: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", reason)
    }

    pub fn unauthorized(reason: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "not_authorized", reason)
    }

    pub fn internal(reason: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_server_error", reason.to_string())
    }

    pub fn method_not_allowed() -> Self {
        Self::new(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed", "Method Not Allowed")
    }

    /// A failed health check: 503 with the given body.
    pub fn unavailable(body: Value) -> Self {
        ApiError { status: StatusCode::SERVICE_UNAVAILABLE, error: String::new(), reason: String::new(), body: Some(body) }
    }

    pub fn vhost_denied() -> Self {
        Self::unauthorized("User not authorised to access virtual host")
    }
}

impl From<AmqpError> for ApiError {
    fn from(e: AmqpError) -> Self {
        match e.code {
            NOT_FOUND => ApiError::not_found(),
            ACCESS_REFUSED | NOT_ALLOWED => ApiError::unauthorized(e.text),
            INTERNAL_ERROR => ApiError::internal(e.text),
            _ => ApiError::bad_request(e.text),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::internal(format!("{e:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = self.body.unwrap_or_else(|| json!({"error": self.error, "reason": self.reason}));
        let mut r = (self.status, json_body(&body)).into_response();
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if self.status == StatusCode::UNAUTHORIZED {
            r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"RabbitMQ Management\""));
        }
        r
    }
}

fn json_body(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "{}".into())
}

/// 200 with a JSON body.
pub(crate) fn ok(v: Value) -> Response {
    let mut r = (StatusCode::OK, json_body(&v)).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

pub(crate) fn no_content() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

/// 201 for a new object, 204 for an update (RabbitMQ's PUT semantics).
pub(crate) fn created_or_updated(created: bool) -> Response {
    if created { StatusCode::CREATED.into_response() } else { StatusCode::NO_CONTENT.into_response() }
}

/// `GET`-like or not: the handlers dispatch on this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum M {
    Get,
    Put,
    Post,
    Delete,
    Other,
}

/// A parsed, authenticated API request.
pub(crate) struct Req {
    pub method: M,
    pub segs: Vec<String>,
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub user: User,
}

impl Req {
    pub fn q(&self, k: &str) -> Option<&str> {
        self.query.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }

    pub fn flag(&self, k: &str) -> bool {
        matches!(self.q(k), Some("true" | "1" | ""))
    }

    /// The JSON body (`{}` when empty).
    pub fn json(&self) -> ApiResult<Value> {
        if self.body.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(Value::Object(Map::new()));
        }
        let v: Value = serde_json::from_slice(&self.body).map_err(|e| ApiError::bad_request(format!("failed_to_parse_json: {e}")))?;
        if !v.is_object() {
            return Err(ApiError::bad_request("not_json_object"));
        }
        Ok(v)
    }

    pub fn ages(&self) -> Ages {
        let age = |a: &str, i: &str| -> Option<(u64, u64)> {
            let age = self.q(a)?.parse().ok()?;
            Some((age, self.q(i).and_then(|v| v.parse().ok()).unwrap_or(5)))
        };
        Ages { msg: age("msg_rates_age", "msg_rates_incr"), lengths: age("lengths_age", "lengths_incr"), data: age("data_rates_age", "data_rates_incr") }
    }

    pub fn disable_stats(&self) -> bool {
        self.flag("disable_stats")
    }
}

/// Split and percent-decode the path after `/api/`.
pub(crate) fn segments(path: &str) -> Vec<String> {
    let rest = path.strip_prefix("/api").unwrap_or(path);
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() {
        return vec![];
    }
    rest.split('/').map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned()).collect()
}

/// Parse `a=1&b=2` (form encoding: `+` is a space).
pub(crate) fn parse_query(q: Option<&str>) -> Vec<(String, String)> {
    let Some(q) = q else { return vec![] };
    let dec = |s: &str| percent_decode_str(&s.replace('+', " ")).decode_utf8_lossy().into_owned();
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (dec(k), dec(v)),
            None => (dec(p), String::new()),
        })
        .collect()
}

const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

/// Percent-encode one path segment.
pub(crate) fn enc(s: &str) -> String {
    utf8_percent_encode(s, SEGMENT).to_string()
}

/// `Authorization: Basic ...` → (user, password).
pub(crate) fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = v.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let raw = base64::engine::general_purpose::STANDARD.decode(rest.trim()).ok()?;
    let s = String::from_utf8(raw).ok()?;
    let (u, p) = s.split_once(':')?;
    Some((u.to_string(), p.to_string()))
}

pub(crate) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Look up `a.b.c` in a JSON object.
fn path<'a>(v: &'a Value, p: &str) -> Option<&'a Value> {
    p.split('.').try_fold(v, |v, k| v.get(k))
}

fn compare(a: Option<&Value>, b: Option<&Value>) -> Ordering {
    match (a, b) {
        (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64().unwrap_or(0.0).total_cmp(&y.as_f64().unwrap_or(0.0)),
        (Some(Value::String(x)), Some(Value::String(y))) => x.cmp(y),
        (Some(Value::Bool(x)), Some(Value::Bool(y))) => x.cmp(y),
        (None | Some(Value::Null), None | Some(Value::Null)) => Ordering::Equal,
        (None | Some(Value::Null), _) => Ordering::Less,
        (_, None | Some(Value::Null)) => Ordering::Greater,
        (Some(x), Some(y)) => x.to_string().cmp(&y.to_string()),
    }
}

/// Keep only the requested (possibly dotted) columns.
pub(crate) fn prune(v: &Value, columns: &[&str]) -> Value {
    let Value::Object(_) = v else { return v.clone() };
    let mut out = Value::Object(Map::new());
    for c in columns {
        let Some(found) = path(v, c) else { continue };
        let parts: Vec<&str> = c.split('.').collect();
        let mut cur = &mut out;
        for (i, p) in parts.iter().enumerate() {
            let Value::Object(m) = cur else { break };
            if i + 1 == parts.len() {
                m.insert((*p).to_string(), found.clone());
                break;
            }
            cur = m.entry((*p).to_string()).or_insert_with(|| Value::Object(Map::new()));
        }
    }
    out
}

/// Drop statistics (`disable_stats=true`), keeping queue totals when `enable_queue_totals`.
fn strip_stats(v: &mut Value, keep_totals: bool) {
    let Value::Object(m) = v else { return };
    const TOTALS: &[&str] = &["messages", "messages_ready", "messages_unacknowledged"];
    m.retain(|k, _| {
        if k == "message_stats" || k.ends_with("_details") || k == "garbage_collection" {
            return false;
        }
        if TOTALS.contains(&k.as_str()) {
            return keep_totals;
        }
        !(k.starts_with("messages_") || k.starts_with("message_bytes") || k == "memory" || k == "reductions")
    });
}

/// Apply `columns` / `disable_stats` to a single object.
pub(crate) fn shape(req: &Req, mut v: Value) -> Value {
    if req.disable_stats() {
        strip_stats(&mut v, req.flag("enable_queue_totals"));
    }
    match req.q("columns") {
        Some(c) if !c.is_empty() => prune(&v, &c.split(',').map(str::trim).collect::<Vec<_>>()),
        _ => v,
    }
}

/// Render a list, applying name filtering, sorting, pagination and column selection.
pub(crate) fn list(req: &Req, mut items: Vec<Value>) -> ApiResult {
    let total = items.len();
    if let Some(name) = req.q("name").filter(|n| !n.is_empty()) {
        if req.flag("use_regex") {
            let re = regex::RegexBuilder::new(name).case_insensitive(true).build().map_err(|e| ApiError::bad_request(format!("invalid regex: {e}")))?;
            items.retain(|i| i.get("name").and_then(|n| n.as_str()).is_some_and(|n| re.is_match(n)));
        } else {
            let needle = name.to_lowercase();
            items.retain(|i| i.get("name").and_then(|n| n.as_str()).is_some_and(|n| n.to_lowercase().contains(&needle)));
        }
    }
    if let Some(key) = req.q("sort").filter(|s| !s.is_empty()) {
        items.sort_by(|a, b| compare(path(a, key), path(b, key)));
    }
    if req.flag("sort_reverse") {
        items.reverse();
    }
    let filtered = items.len();
    let paged = req.q("page").is_some();
    if paged {
        let page: usize = req.q("page").and_then(|p| p.parse().ok()).filter(|p| *p >= 1).ok_or_else(|| ApiError::bad_request("page must be a positive integer"))?;
        let page_size: usize = match req.q("page_size") {
            None => 100,
            Some(s) => s.parse().ok().filter(|n| *n >= 1).ok_or_else(|| ApiError::bad_request("page_size must be a positive integer"))?,
        };
        if page_size > 500 {
            return Err(ApiError::bad_request("Page size must be no greater than 500"));
        }
        let page_count = filtered.div_ceil(page_size);
        if page > page_count.max(1) {
            return Err(ApiError::bad_request("Page out of range"));
        }
        let start = (page - 1) * page_size;
        let items: Vec<Value> = items.into_iter().skip(start).take(page_size).map(|i| shape(req, i)).collect();
        return Ok(ok(json!({
            "filtered_count": filtered,
            "item_count": items.len(),
            "items": items,
            "page": page,
            "page_count": page_count,
            "page_size": page_size,
            "total_count": total,
        })));
    }
    Ok(ok(Value::Array(items.into_iter().map(|i| shape(req, i)).collect())))
}

/// RabbitMQ's timestamp format: `2024-05-01T12:00:00.000+00:00`.
pub(crate) fn fmt_time(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}+00:00", rem / 3600, rem % 3600 / 60, rem % 60, ms % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_segments_decode() {
        assert_eq!(segments("/api/queues/%2F/a%20b"), vec!["queues", "/", "a b"]);
        assert_eq!(segments("/api/bindings/%2F/e//q/x/"), vec!["bindings", "/", "e", "", "q", "x"]);
        assert!(segments("/api").is_empty());
        assert_eq!(enc("/"), "%2F");
        assert_eq!(enc("a.b-c_d~"), "a.b-c_d~");
    }

    #[test]
    fn query_and_columns() {
        let q = parse_query(Some("columns=name,message_stats.publish&x=a+b%2F"));
        assert_eq!(q[1], ("x".into(), "a b/".into()));
        let v = json!({"name": "q", "durable": true, "message_stats": {"publish": 3, "ack": 1}});
        assert_eq!(prune(&v, &["name", "message_stats.publish"]), json!({"name": "q", "message_stats": {"publish": 3}}));
    }

    #[test]
    fn timestamps() {
        assert_eq!(fmt_time(0), "1970-01-01T00:00:00.000+00:00");
        assert_eq!(fmt_time(1_714_564_800_123), "2024-05-01T12:00:00.123+00:00");
    }
}
