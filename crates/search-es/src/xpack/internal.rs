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
use crate::json::{Map, Value};
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
    /// Set inside [`index_only`]: the one index a `_bulk` may write, and only by `index`.
    static INDEX_ONLY: String;
    static CALLER: Caller;
    static ACTING: crate::xpack::security::Principal;
    /// Set inside [`as_principal`]: a job's internal calls are its own, never the caller's.
    static JOB: ();
}

/// Runs `f` with its internal calls that carry no credentials made as `principal` (when there
/// is one): a watch runs as the user who stored it, as Elasticsearch runs it with the stored
/// security headers. Only the user and roles are kept, never credentials.
pub async fn as_principal<F: Future>(principal: Option<crate::xpack::security::Principal>, f: F) -> F::Output {
    // The job's own identity, never that of the request that set it off (an `_execute`, a
    // `_start`): there is no caller inside.
    let f = JOB.scope((), f);
    match principal {
        Some(p) => ACTING.scope(p, f).await,
        None => f.await,
    }
}

/// Runs `f` with every `_bulk` it makes restricted to `index` actions into `index` (a connector's
/// sync: whatever its body says, it can't delete, update or write elsewhere). It only takes
/// capability away, so nothing outside can widen it.
pub async fn index_only<F: Future>(index: String, f: F) -> F::Output {
    INDEX_ONLY.scope(index, f).await
}

/// The index a `_bulk` is restricted to ([`index_only`]), if any.
pub fn bulk_index_only() -> Option<String> {
    INDEX_ONLY.try_with(Clone::clone).ok()
}

/// The user a background job (a watch, a rollup job) runs as ([`as_principal`]), kept with
/// it: `None` for a superuser (the job runs with the cluster's authority). Only the user and
/// its roles (an API key's role descriptors), never credentials.
pub fn stored_user(p: &crate::xpack::security::Principal) -> Option<Value> {
    if p.is_superuser() {
        return None;
    }
    let mut m = Map::new();
    m.insert("username".into(), p.username.clone().into());
    m.insert("roles".into(), Value::Arr(p.roles.iter().map(Value::from).collect()));
    m.insert("realm_name".into(), p.realm_name.into());
    m.insert("realm_type".into(), p.realm_type.into());
    if let crate::xpack::security::AuthKind::ApiKey(a) = &p.kind {
        m.insert("api_key".into(), crate::obj! { "id" => a.id.clone(), "name" => a.name.clone(), "role_descriptors" => a.role_descriptors.clone(), "limited_by" => a.limited_by.clone(), "cross_cluster" => a.cross_cluster });
    }
    Some(Value::Obj(m))
}

/// The principal [`stored_user`] kept.
pub fn principal_of(u: &Value) -> Option<crate::xpack::security::Principal> {
    const REALMS: [&str; 8] = ["reserved", "default_native", "native", "default_file", "file", "_es_api_key", "_service_account", "__attach"];
    let known = |s: Option<&str>, d: &'static str| -> &'static str { s.and_then(|s| REALMS.iter().find(|r| **r == s).copied()).unwrap_or(d) };
    let kind = match u.get("api_key") {
        Some(a) => crate::xpack::security::AuthKind::ApiKey(Box::new(crate::xpack::security::ApiKeyAuth {
            id: a.get("id").map(Value::to_es_string).unwrap_or_default(),
            name: a.get("name").cloned().unwrap_or(Value::Null),
            role_descriptors: a.get("role_descriptors").cloned().unwrap_or(Value::Null),
            limited_by: a.get("limited_by").cloned().unwrap_or(Value::Null),
            cross_cluster: a.get("cross_cluster").and_then(Value::as_bool).unwrap_or(false),
            remote: None,
        })),
        None => crate::xpack::security::AuthKind::Realm,
    };
    // Built from the owner's defaults, so fields this doesn't keep stay neutral.
    let mut p = crate::xpack::security::Principal::owner();
    p.username = u.get("username")?.to_es_string();
    p.roles = u.get("roles").and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default();
    p.full_name = Value::Null;
    p.email = Value::Null;
    p.metadata = Value::object();
    p.realm_name = known(u.get("realm_name").and_then(Value::as_str), "default_native");
    p.realm_type = known(u.get("realm_type").and_then(Value::as_str), "native");
    p.kind = kind;
    Some(p)
}

/// Why the principal a job stored ([`principal_of`]) may no longer act, checked before each of
/// its runs with the lookups authentication makes: its API key is gone, invalidated or expired;
/// its file, native or reserved user (an OpenSearch internal user, on that dialect) is gone or
/// disabled. `None`: it may act. Service accounts, other realms and roll's index scopes
/// ([`INDEX_SCOPE_KEY`](crate::xpack::security::INDEX_SCOPE_KEY)) pass: nothing in the cluster
/// revokes them. The reason names the user or key, never a credential.
pub fn principal_problem(cluster: &Cluster, p: &crate::xpack::security::Principal) -> Option<String> {
    use crate::xpack::security::{AuthKind, INDEX_SCOPE_KEY, api_keys, opensearch, users};
    if let AuthKind::ApiKey(a) = &p.kind {
        if a.id == INDEX_SCOPE_KEY {
            return None;
        }
        let st = cluster.state();
        let key = format!("API key [{}] of user [{}], which this job runs as,", a.id, p.username);
        let Some(k) = st.metadata.xpack.get(api_keys::KEYS, &a.id) else { return Some(format!("{key} no longer exists")) };
        if k.get("invalidated").and_then(Value::as_bool) == Some(true) {
            return Some(format!("{key} has been invalidated"));
        }
        if k.get("expiration").and_then(Value::as_i64).is_some_and(|exp| exp <= crate::time::now_millis()) {
            return Some(format!("{key} has expired"));
        }
        return None;
    }
    let gone = match (cluster.dialect, p.realm_type) {
        (crate::cluster::Dialect::OpenSearch, _) => opensearch::lookup_user(cluster, &p.username).is_none(),
        (_, "native" | "file" | "reserved") => users::lookup_user(cluster, &p.username).is_none(),
        _ => false,
    };
    gone.then(|| format!("user [{}], which this job runs as, no longer exists or is disabled", p.username))
}

/// The error a job's run fails with when [`principal_problem`] finds its principal can no longer
/// act (`None`: no principal, or one that can). The 401 marks it: a job's internal calls carry no
/// credentials, so nothing else they return is one.
pub fn principal_refusal(cluster: &Cluster, p: Option<&crate::xpack::security::Principal>) -> Option<EsError> {
    principal_problem(cluster, p?).map(|reason| EsError::security(401, reason))
}

/// Marks a [`call_system`] request, which keeps the cluster's authority inside
/// [`as_principal`] too. A client sending it gains nothing: outside `as_principal` it changes
/// nothing.
pub const SYSTEM_HEADER: &str = "x-roll-internal-system";

/// The principal an internal request without credentials runs as ([`as_principal`]).
pub fn acting_principal(req: &EsRequest) -> Option<crate::xpack::security::Principal> {
    if req.header("authorization").is_some() || req.header(crate::xpack::security::AUTHENTICATED_HEADER).is_some() || req.header(SYSTEM_HEADER).is_some() {
        return None;
    }
    ACTING.try_with(Clone::clone).ok()
}

impl Caller {
    /// The client of `ctx`, unless it may read everything anyway.
    pub fn of(ctx: &crate::rest::Ctx<'_>) -> Option<Caller> {
        if ctx.principal.get()?.is_superuser() {
            return None;
        }
        Caller::client(ctx)
    }

    /// The client of `ctx`, a superuser too: for calls whose result belongs to whoever sent
    /// them (a point in time or scroll is bound to its opener).
    pub fn client(ctx: &crate::rest::Ctx<'_>) -> Option<Caller> {
        use crate::xpack::security::{AUTHENTICATED_HEADER, INDEX_SCOPE_HEADER, RUN_AS_HEADER};
        ctx.principal.get()?;
        // The headers that made it this principal: the node's authentication (narrowed to some
        // indices on a per-index route) or the client's credentials, and its run-as user.
        // A linked remote cluster's request keeps its remote access too (only roll's own calls
        // reach here with it), or its key alone would be refused.
        let headers = [AUTHENTICATED_HEADER, INDEX_SCOPE_HEADER, "Authorization", RUN_AS_HEADER, crate::remote::links::REMOTE_ACCESS_HEADER]
            .into_iter()
            .filter(|h| *h != "Authorization" || ctx.request.header(AUTHENTICATED_HEADER).is_none())
            .filter_map(|h| ctx.request.header(h).map(|v| (h.to_string(), v.to_string())))
            .collect();
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
    if JOB.try_with(|_| ()).is_ok() {
        return None;
    }
    CALLER.try_with(Caller::clone).ok()
}

/// Sends `method uri` with a JSON body; the parsed response on 2xx, else its error.
pub fn call<'a>(cluster: &'a Cluster, method: &str, uri: &str, body: Option<&Value>) -> Call<'a> {
    let mut req = EsRequest::new(method, uri);
    if let Some(b) = body {
        req.body = bytes::Bytes::from(b.to_json());
        req.headers.push(("Content-Type".into(), "application/json".into()));
    }
    // The caller is read when the call runs, so `as_caller(c, call(…))` calls as `c` too.
    Box::pin(async move {
        if let Some(c) = caller() {
            req.headers.extend(c.headers);
        }
        send(cluster, req).await
    })
}

/// [`call`] with the cluster's own authority, also inside [`as_caller`] and [`as_principal`].
pub fn call_system<'a>(cluster: &'a Cluster, method: &str, uri: &str, body: Option<&Value>) -> Call<'a> {
    let mut req = EsRequest::new(method, uri);
    req.headers.push((SYSTEM_HEADER.into(), "true".into()));
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
    if let Some(user) = ctx.request.header(crate::xpack::security::RUN_AS_HEADER) {
        req.headers.push((crate::xpack::security::RUN_AS_HEADER.into(), user.to_string()));
    }
    if let Some(access) = ctx.request.header(crate::remote::links::REMOTE_ACCESS_HEADER) {
        req.headers.push((crate::remote::links::REMOTE_ACCESS_HEADER.into(), access.to_string()));
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
        // As the caller too ([`call`]): a bulk write is authorized per item as the client.
        if let Some(c) = caller() {
            req.headers.extend(c.headers);
        }
        let resp = crate::rest::handle(cluster, req).await;
        let v = crate::json::parse(&resp.body).ok().flatten().unwrap_or(Value::Null);
        if resp.status < 300 { Ok(v) } else { Err(error_from_body(resp.status, &v)) }
    })
}

/// [`call_ndjson`] with the cluster's own authority (`POST <uri>`).
pub fn call_ndjson_system<'a>(cluster: &'a Cluster, uri: &str, body: String) -> Call<'a> {
    let mut req = EsRequest::new("POST", uri);
    req.headers.push((SYSTEM_HEADER.into(), "true".into()));
    req.body = bytes::Bytes::from(body);
    req.headers.push(("Content-Type".into(), "application/x-ndjson".into()));
    send(cluster, req)
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

    #[test]
    fn stored_users_round_trip() {
        use crate::xpack::security::{AuthKind, Principal};
        let mut p = Principal::owner();
        p.username = "watcher_manager".into();
        p.roles = vec!["watcher_manager".into()];
        p.realm_name = "default_file";
        p.realm_type = "file";
        let back = principal_of(&stored_user(&p).unwrap()).unwrap();
        assert_eq!((back.username.as_str(), back.roles.clone(), back.realm_name, back.kind), ("watcher_manager", p.roles.clone(), "default_file", AuthKind::Realm));
        // A superuser's job runs with the cluster's authority.
        assert!(stored_user(&Principal::owner()).is_none());
    }
}
