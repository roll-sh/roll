//! Internal REST calls: x-pack features built on the core APIs (ES|QL and SQL reading indices,
//! transforms writing their destination, datafeed previews, enrich policy execution) run them
//! through [`crate::rest::handle`], so they get exactly the core APIs' semantics and errors.
//!
//! An internal call carries no credentials, so it runs with the cluster's own authority: the
//! outer request was authorized for its API before it got here, as Elasticsearch runs these
//! as its internal users. Inside [`as_caller`] (ES|QL and SQL, which read whatever indices
//! their query names) a call carries the client's credentials instead, so it is authorized,
//! and reads under document and field level security, as that client; [`call_system`] stays
//! the cluster's own (ENRICH's policy index). The future is boxed because the call re-enters
//! the dispatcher that is running it.

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::rest::EsRequest;
use std::future::Future;
use std::pin::Pin;

pub type Call<'a> = Pin<Box<dyn Future<Output = EsResult<Value>> + Send + 'a>>;

/// The client an ES|QL or SQL request runs for, when its reads must be authorized as it (not
/// the cluster's owner or a superuser).
#[derive(Clone, Debug)]
pub struct Caller {
    /// The headers that make an internal call that client.
    headers: Vec<(String, String)>,
    /// Its document and field level security (`None`: it reads documents unrestricted).
    pub security: Option<crate::xpack::security::dls::ReadSecurity>,
}

tokio::task_local! {
    static CALLER: Caller;
}

impl Caller {
    /// The client of `ctx`, unless it may read everything anyway.
    pub fn of(ctx: &crate::rest::Ctx<'_>) -> Option<Caller> {
        use crate::xpack::security::{AUTHENTICATED_HEADER, INDEX_SCOPE_HEADER};
        let p = ctx.principal.get()?;
        if p.is_superuser() {
            return None;
        }
        let mut headers = Vec::new();
        match ctx.request.header(AUTHENTICATED_HEADER) {
            // The node authenticated the owner, narrowed to some indices (a per-index route).
            Some(user) => {
                headers.push((AUTHENTICATED_HEADER.to_string(), user.to_string()));
                headers.push((INDEX_SCOPE_HEADER.to_string(), ctx.request.header(INDEX_SCOPE_HEADER)?.to_string()));
            }
            None => headers.push(("Authorization".to_string(), ctx.request.header("authorization")?.to_string())),
        }
        Some(Caller { headers, security: crate::xpack::security::dls::ReadSecurity::of(ctx) })
    }
}

/// Runs `f` with its internal calls made as `caller` (when there is one).
pub async fn as_caller<F: Future>(caller: Option<Caller>, f: F) -> F::Output {
    match caller {
        Some(c) => CALLER.scope(c, f).await,
        None => f.await,
    }
}

/// The client the running request's internal calls are made as ([`as_caller`]).
pub fn caller() -> Option<Caller> {
    CALLER.try_with(Caller::clone).ok()
}

/// Sends `method uri` with a JSON body; the parsed response on 2xx, else its error.
pub fn call<'a>(cluster: &'a Cluster, method: &str, uri: &str, body: Option<&Value>) -> Call<'a> {
    let mut req = EsRequest::new(method, uri);
    if let Some(b) = body {
        req.body = bytes::Bytes::from(b.to_json());
        req.headers.push(("Content-Type".into(), "application/json".into()));
    }
    let _ = CALLER.try_with(|c| req.headers.extend(c.headers.iter().cloned()));
    send(cluster, req)
}

/// [`call`] with the cluster's own authority, also inside [`as_caller`].
pub fn call_system<'a>(cluster: &'a Cluster, method: &str, uri: &str, body: Option<&Value>) -> Call<'a> {
    let mut req = EsRequest::new(method, uri);
    if let Some(b) = body {
        req.body = bytes::Bytes::from(b.to_json());
        req.headers.push(("Content-Type".into(), "application/json".into()));
    }
    send(cluster, req)
}

fn send(cluster: &Cluster, req: EsRequest) -> Call<'_> {
    Box::pin(async move {
        let resp = crate::rest::handle(cluster, req).await;
        let v = if resp.body.is_empty() { Value::Null } else { crate::json::parse(&resp.body).ok().flatten().unwrap_or(Value::Null) };
        if resp.status < 300 { Ok(v) } else { Err(error_from_body(resp.status, &v)) }
    })
}

/// [`call`] as the client that sent `ctx`'s request (its `Authorization` header): the call is
/// authorized, and reads under document and field level security, as that client.
pub fn call_as<'a>(ctx: &crate::rest::Ctx<'_>, cluster: &'a Cluster, method: &str, uri: &str, body: Option<&Value>) -> Call<'a> {
    let mut req = EsRequest::new(method, uri);
    if let Some(b) = body {
        req.body = bytes::Bytes::from(b.to_json());
        req.headers.push(("Content-Type".into(), "application/json".into()));
    }
    if ctx.request.header(crate::xpack::security::AUTHENTICATED_HEADER).is_none()
        && let Some(a) = ctx.request.header("authorization")
    {
        req.headers.push(("Authorization".into(), a.to_string()));
    }
    Box::pin(async move {
        let resp = crate::rest::handle(cluster, req).await;
        let v = if resp.body.is_empty() { Value::Null } else { crate::json::parse(&resp.body).ok().flatten().unwrap_or(Value::Null) };
        if resp.status < 300 { Ok(v) } else { Err(error_from_body(resp.status, &v)) }
    })
}

/// Sends an NDJSON body (`_bulk`).
pub fn call_ndjson<'a>(cluster: &'a Cluster, method: &str, uri: &str, body: String) -> Call<'a> {
    let mut req = EsRequest::new(method, uri);
    req.body = bytes::Bytes::from(body);
    req.headers.push(("Content-Type".into(), "application/x-ndjson".into()));
    Box::pin(async move {
        let resp = crate::rest::handle(cluster, req).await;
        let v = crate::json::parse(&resp.body).ok().flatten().unwrap_or(Value::Null);
        if resp.status < 300 { Ok(v) } else { Err(error_from_body(resp.status, &v)) }
    })
}

/// Plain Java exceptions (not `ElasticsearchException`s), which root-cause guessing treats
/// differently.
const JAVA: &[&str] = &[
    "illegal_argument_exception",
    "illegal_state_exception",
    "number_format_exception",
    "unsupported_operation_exception",
    "null_pointer_exception",
    "array_index_out_of_bounds_exception",
    "class_cast_exception",
    "arithmetic_exception",
    "date_time_exception",
    "date_time_parse_exception",
];

/// Rebuilds an [`EsError`] from a rendered error body (`{"error": {...}, "status": N}`).
pub fn error_from_body(status: u16, v: &Value) -> EsError {
    match v.get("error") {
        Some(e @ Value::Obj(_)) => rebuild(status, e),
        Some(Value::Str(s)) => EsError::illegal_argument(s.clone()).with_status(status),
        _ => EsError::new(status, "exception", format!("request failed with status [{status}]")),
    }
}

fn rebuild(status: u16, e: &Value) -> EsError {
    let kind = e.get("type").and_then(Value::as_str).unwrap_or("exception").to_string();
    let reason = e.get("reason").and_then(Value::as_str).map(str::to_string);
    let mut err = if JAVA.contains(&kind.as_str()) { EsError::java(status, kind, reason.clone().unwrap_or_default()) } else { EsError::new(status, kind, reason.clone().unwrap_or_default()) };
    if reason.is_none() {
        err = err.with_null_reason();
    }
    if let Some(m) = e.as_object() {
        for (k, val) in m {
            match k.as_str() {
                "type" | "reason" | "root_cause" | "caused_by" | "stack_trace" | "suppressed" | "header" => {}
                _ => err = err.with_meta(k.clone(), val.clone()),
            }
        }
        if let Some(Value::Obj(h)) = m.get("header") {
            for (k, val) in h {
                let vals = match val {
                    Value::Arr(a) => a.iter().map(Value::to_es_string).collect(),
                    other => vec![other.to_es_string()],
                };
                err = err.with_header_values(k.clone(), vals);
            }
        }
    }
    if let Some(c @ Value::Obj(_)) = e.get("caused_by") {
        err = err.caused_by(rebuild(status, c));
    }
    // Keep the rendered root causes when they aren't what the rebuilt chain would guess
    // (shard failures name theirs).
    if let Some(Value::Arr(roots)) = e.get("root_cause").filter(|r| r.as_array().is_some_and(|a| !a.is_empty())) {
        let rebuilt: Vec<EsError> = roots.iter().filter(|r| matches!(r, Value::Obj(_))).map(|r| rebuild(status, r)).collect();
        let guessed: Vec<String> = err.guess_root_causes().iter().map(|r| r.kind.to_string()).collect();
        if rebuilt.iter().map(|r| r.kind.to_string()).collect::<Vec<_>>() != guessed {
            err = err.with_root_causes(rebuilt);
        }
    }
    err
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obj;

    #[test]
    fn rebuilds_errors() {
        let body = crate::json::parse(
            br#"{"error":{"root_cause":[],"type":"index_not_found_exception","reason":"no such index [x]","resource.type":"index_or_alias","resource.id":"x","index_uuid":"_na_","index":"x"},"status":404}"#,
        )
        .unwrap()
        .unwrap();
        let e = error_from_body(404, &body);
        assert_eq!(e, EsError::index_not_found("x"));
        let e = error_from_body(400, &obj! { "error" => obj! { "type" => "illegal_argument_exception", "reason" => "bad" } });
        assert_eq!(e, EsError::illegal_argument("bad"));
    }
}
