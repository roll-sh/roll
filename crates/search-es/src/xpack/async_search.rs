//! Async search: `POST <index>/_async_search`, `GET _async_search/<id>`,
//! `GET _async_search/status/<id>`, `DELETE _async_search/<id>`.
//!
//! The search runs through the core search API ([`internal`]) with the submit request's
//! parameters (minus the async ones), so it has exactly the search API's semantics and errors.
//! It always runs to completion before submit answers (roll's searches don't outlive their
//! request), so a response is never partial; `wait_for_completion_timeout` only bounds how long
//! a client is prepared to wait. Results kept with `keep_on_completion` live in
//! [`super::async_results`] until `keep_alive` (5 days by default) passes or they are deleted.
//! Ids have Elasticsearch's format and malformed ones fail the same way.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::units::TimeValue;
use crate::xpack::async_results::{self, Entry, Kind};
use crate::xpack::{enc, internal};

const DEFAULT_KEEP_ALIVE_MS: i64 = 5 * 24 * 3600 * 1000;

/// Parameters of the submit request that aren't search parameters.
const ASYNC_PARAMS: &[&str] = &["keep_on_completion", "keep_alive", "wait_for_completion_timeout", "error_trace", "filter_path", "pretty", "human", "format"];

fn keep_alive(ctx: &Ctx) -> EsResult<i64> {
    let ka = ctx.time_param("keep_alive", TimeValue::millis(DEFAULT_KEEP_ALIVE_MS))?;
    if ctx.has_query("keep_alive") && ka.as_millis() < 1000 {
        return Err(EsError::action_request_validation(&[format!("[keep_alive] must be greater or equals than 1 second, got:{}", ctx.query("keep_alive").unwrap_or_default())]));
    }
    Ok(ka.as_millis())
}

/// The search's query string: the submit request's parameters minus the async ones.
pub fn forwarded_query(ctx: &Ctx, drop: &[&str]) -> String {
    let q = ctx.uri.split_once('?').map_or("", |(_, q)| q);
    let mut out = String::new();
    for (k, v) in crate::rest::parse_query(q) {
        if drop.contains(&k.as_str()) {
            continue;
        }
        out.push(if out.is_empty() { '?' } else { '&' });
        out.push_str(&enc(&k));
        out.push('=');
        out.push_str(&enc(&v));
    }
    out
}

pub async fn submit(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let keep_alive = keep_alive(ctx)?;
    let keep = ctx.bool_param("keep_on_completion", false)?;
    ctx.time_param("wait_for_completion_timeout", TimeValue::seconds(1))?;
    let mut errors = vec![];
    if ctx.has_query("scroll") {
        if ctx.opt_bool_param("request_cache")? != Some(false) {
            errors.push("[request_cache] cannot be used in a scroll context".to_string());
        }
        errors.push("[scroll] queries are not supported".to_string());
    }
    if !errors.is_empty() {
        return Err(EsError::action_request_validation(&errors));
    }
    let path = match ctx.path_param("index") {
        Some(i) => format!("/{}/_search", enc(i)),
        None => "/_search".to_string(),
    };
    let uri = path + &forwarded_query(ctx, ASYNC_PARAMS);
    let body = ctx.body()?;
    let start = ctx.now_ms;
    let result = internal::call(ctx.cluster, "POST", &uri, body.as_ref()).await;
    let id = async_results::async_id(&ctx.cluster.node.id);
    let mut m = Map::new();
    let status;
    match result {
        Ok(resp) => {
            let took = resp.get("took").and_then(Value::as_i64).unwrap_or(0);
            m.insert("is_partial".into(), false.into());
            m.insert("is_running".into(), false.into());
            m.insert("start_time_in_millis".into(), start.into());
            m.insert("expiration_time_in_millis".into(), (start + keep_alive).into());
            m.insert("completion_time_in_millis".into(), (start + took).into());
            m.insert("response".into(), resp);
            status = 200;
        }
        Err(e) => {
            status = e.status;
            m.insert("is_partial".into(), true.into());
            m.insert("is_running".into(), false.into());
            m.insert("start_time_in_millis".into(), start.into());
            m.insert("expiration_time_in_millis".into(), (start + keep_alive).into());
            let wrapped = EsError::new(e.status, "status_exception", "error while executing search").caused_by(e);
            m.insert("error".into(), wrapped.to_value(false));
        }
    }
    if keep {
        ctx.cluster.xpack.results.put(
            id.clone(),
            Entry { kind: Kind::Search, response: Value::Obj(m.clone()), start_ms: start, expiration_ms: start + keep_alive, bytes: 0, extra: Value::from(status as i64) },
            ctx.now_ms,
        );
        let mut with_id = Map::new();
        with_id.insert("id".into(), id.into());
        with_id.extend(m);
        m = with_id;
    }
    Ok(Reply::Body(status, Value::Obj(m)))
}

fn stored(ctx: &Ctx) -> EsResult<(String, Entry)> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    async_results::check_id(&id)?;
    match ctx.cluster.xpack.results.get(&id, Kind::Search, ctx.now_ms) {
        Some(e) => Ok((id, e)),
        None => Err(EsError::resource_not_found(id)),
    }
}

fn with_id(id: &str, v: &Value, expiration: i64) -> Map {
    let mut m = Map::new();
    m.insert("id".into(), id.into());
    if let Value::Obj(src) = v {
        for (k, val) in src {
            if k == "expiration_time_in_millis" {
                m.insert(k.clone(), expiration.into());
            } else {
                m.insert(k.clone(), val.clone());
            }
        }
    }
    m
}

pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    ctx.time_param("wait_for_completion_timeout", TimeValue::millis(-1))?;
    let (id, mut e) = stored(ctx)?;
    if ctx.has_query("keep_alive") {
        let ka = keep_alive(ctx)?;
        e.expiration_ms = e.expiration_ms.max(ctx.now_ms + ka);
        ctx.cluster.xpack.results.touch(&id, e.expiration_ms);
    }
    let status = e.extra.as_i64().unwrap_or(200) as u16;
    Ok(Reply::Body(status, Value::Obj(with_id(&id, &e.response, e.expiration_ms))))
}

pub fn status(ctx: &Ctx) -> EsResult<Reply> {
    keep_alive(ctx)?;
    let (id, e) = stored(ctx)?;
    let r = &e.response;
    let mut m = Map::new();
    m.insert("id".into(), id.into());
    m.insert("is_running".into(), false.into());
    for k in ["is_partial", "start_time_in_millis", "expiration_time_in_millis", "completion_time_in_millis"] {
        if let Some(v) = r.get(k) {
            m.insert(k.into(), if k == "expiration_time_in_millis" { e.expiration_ms.into() } else { v.clone() });
        }
    }
    let status = e.extra.as_i64().unwrap_or(200);
    if let Some(shards) = r.get("response").and_then(|x| x.get("_shards")) {
        m.insert("_shards".into(), shards.clone());
    }
    if let Some(err) = r.get("error") {
        m.insert("error".into(), err.clone());
    }
    m.insert("completion_status".into(), status.into());
    Reply::ok(Value::Obj(m))
}

pub fn delete(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    async_results::check_id(&id)?;
    if ctx.cluster.xpack.results.remove(&id, Kind::Search) {
        Reply::ok(obj! { "acknowledged" => true })
    } else {
        Err(EsError::resource_not_found(id))
    }
}
