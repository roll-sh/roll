//! EQL: `POST <index>/_eql/search`, `GET _eql/search/<id>`, `GET _eql/search/status/<id>`,
//! `DELETE _eql/search/<id>`.
//!
//! [`parser`] parses Elasticsearch's EQL grammar; [`exec`] verifies the query against the
//! indices' mappings and runs event queries, sequences (with `by`, `maxspan`, `until`,
//! `runs`, missing events) and samples through the core search API (point in time,
//! `search_after`), with Elasticsearch's ordering and limit semantics. A search always runs to
//! completion before it answers (roll's searches don't outlive their request); with
//! `keep_on_completion` the response is kept in [`super::async_results`] under an
//! Elasticsearch-format id for `keep_alive` (5 days by default).

pub mod exec;
pub mod parser;

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::units::TimeValue;
use crate::xpack::async_results::{self, Entry, Kind};

const DEFAULT_KEEP_ALIVE_MS: i64 = 5 * 24 * 3600 * 1000;

const FIELDS: &[&str] = &[
    "filter",
    "timestamp_field",
    "tiebreaker_field",
    "event_category_field",
    "size",
    "fetch_size",
    "query",
    "wait_for_completion_timeout",
    "keep_alive",
    "keep_on_completion",
    "result_position",
    "fields",
    "runtime_mappings",
    "max_samples_per_key",
    "allow_partial_search_results",
    "allow_partial_sequence_results",
];

fn field_error(ctx: &Ctx, key: &str, cause: EsError) -> EsError {
    let (l, c) = crate::xpack::value_loc(ctx, key);
    EsError::x_content_parse(l, c, format!("[eql/search] failed to parse field [{key}]")).caused_by(cause)
}

fn str_field(ctx: &Ctx, m: &Map, key: &str) -> EsResult<Option<String>> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Str(s)) => Ok(Some(s.clone())),
        Some(v @ (Value::Num(_) | Value::Bool(_))) => Ok(Some(v.to_es_string())),
        Some(_) => {
            let (l, c) = crate::xpack::value_loc(ctx, key);
            Err(EsError::x_content_parse(l, c, format!("[eql/search] {key} doesn't support values of type: START_OBJECT")))
        }
    }
}

fn int_field(ctx: &Ctx, m: &Map, key: &str, default: i64) -> EsResult<i64> {
    match m.get(key) {
        None => Ok(default),
        Some(Value::Num(n)) => match n.as_i64() {
            Some(i) if i32::try_from(i).is_ok() => Ok(i),
            Some(_) => Err(field_error(ctx, key, EsError::illegal_argument(format!("Value [{}] is out of range for an integer", n.as_f64() as i64)))),
            None => Ok(n.as_f64() as i32 as i64),
        },
        Some(Value::Str(s)) => s.trim().parse::<i32>().map(i64::from).map_err(|_| field_error(ctx, key, EsError::number_format(s))),
        Some(_) => Err(field_error(ctx, key, EsError::illegal_argument(format!("[{key}] must be a number")))),
    }
}

fn bool_field(ctx: &Ctx, m: &Map, key: &str) -> EsResult<Option<bool>> {
    match m.get(key) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(Value::Str(s)) => crate::units::parse_bool(s).map(Some).map_err(|e| field_error(ctx, key, e)),
        Some(_) => Err(field_error(ctx, key, EsError::illegal_argument(format!("[{key}] must be a boolean")))),
    }
}

/// A parsed search request.
struct Request {
    query: String,
    settings: exec::Settings,
    keep_alive_ms: i64,
    keep_on_completion: bool,
}

fn parse_request(ctx: &Ctx) -> EsResult<Request> {
    let Some(body) = ctx.body()? else { return Err(EsError::parse("request body or source parameter is required")) };
    let Some(m) = body.as_object() else { return Err(EsError::parse("request body or source parameter is required")) };
    for k in m.keys() {
        if !FIELDS.contains(&k.as_str()) {
            return Err(crate::xpack::unknown_field(ctx, "eql/search", k));
        }
    }
    let query = str_field(ctx, m, "query")?.unwrap_or_default();
    let size = int_field(ctx, m, "size", 10)?;
    let fetch_size = int_field(ctx, m, "fetch_size", 1000)?;
    let max_samples = int_field(ctx, m, "max_samples_per_key", 1)?;
    let timestamp_field = str_field(ctx, m, "timestamp_field")?.unwrap_or_else(|| "@timestamp".into());
    let event_category_field = str_field(ctx, m, "event_category_field")?.unwrap_or_else(|| "event.category".into());
    let tiebreaker_field = str_field(ctx, m, "tiebreaker_field")?;
    let tail = match str_field(ctx, m, "result_position")?.as_deref() {
        None | Some("tail") => true,
        Some("head") => false,
        Some(other) => {
            return Err(field_error(ctx, "result_position", EsError::illegal_argument(format!("result position needs to be 'head' or 'tail', received '{other}'"))));
        }
    };
    let mut keep_alive = match str_field(ctx, m, "keep_alive")? {
        Some(k) => Some(TimeValue::parse(&k, "keep_alive").map_err(|e| field_error(ctx, "keep_alive", e))?),
        None => None,
    };
    if let Some(k) = m.get("wait_for_completion_timeout").map(Value::to_es_string) {
        TimeValue::parse(&k, "wait_for_completion_timeout").map_err(|e| field_error(ctx, "wait_for_completion_timeout", e))?;
    }
    let mut keep_on_completion = bool_field(ctx, m, "keep_on_completion")?.unwrap_or(false);
    bool_field(ctx, m, "allow_partial_search_results")?;
    bool_field(ctx, m, "allow_partial_sequence_results")?;
    if ctx.has_query("wait_for_completion_timeout") {
        ctx.time_param("wait_for_completion_timeout", TimeValue::MINUS_ONE)?;
    }
    if ctx.has_query("keep_alive") {
        keep_alive = Some(ctx.time_param("keep_alive", TimeValue::MINUS_ONE)?);
    }
    keep_on_completion = ctx.bool_param("keep_on_completion", keep_on_completion)?;
    ctx.bool_param("ccs_minimize_roundtrips", true)?;
    ctx.bool_param("allow_partial_search_results", true)?;
    ctx.bool_param("allow_partial_sequence_results", false)?;
    let mut errors = Vec::new();
    if query.is_empty() {
        errors.push("query is null or empty".to_string());
    }
    if timestamp_field.is_empty() {
        errors.push("@timestamp field is null or empty".to_string());
    }
    if event_category_field.is_empty() {
        errors.push("event category field is null or empty".to_string());
    }
    if size < 0 {
        errors.push("size must be greater than or equal to 0".to_string());
    }
    if fetch_size < 2 {
        errors.push("fetch size must be greater than 1".to_string());
    }
    if let Some(k) = &keep_alive
        && k.as_millis() < 60_000
    {
        errors.push(format!("[keep_alive] must be greater than 1 minute, got:{}", k.human()));
    }
    if let Some(Value::Obj(rm)) = m.get("runtime_mappings") {
        for (name, def) in rm {
            match def {
                Value::Obj(d) if d.get("type").is_none_or(Value::is_null) => {
                    errors.push(format!("No type specified for runtime field [{name}]"));
                    break;
                }
                Value::Obj(_) => {}
                _ => {
                    errors.push(format!("Expected map for runtime field [{name}] definition but got [{}]", def.to_es_string()));
                    break;
                }
            }
        }
    }
    if max_samples < 1 {
        errors.push("max_samples_per_key must be greater than 0".to_string());
    }
    if !errors.is_empty() {
        return Err(EsError::action_request_validation(&errors));
    }
    let index = ctx.path_param("index").unwrap_or("_all").to_string();
    let settings = exec::Settings {
        index,
        timestamp_field,
        tiebreaker_field,
        event_category_field,
        size: size as i32,
        fetch_size: fetch_size as usize,
        filter: m.get("filter").cloned(),
        runtime_mappings: m.get("runtime_mappings").cloned(),
        fields: m.get("fields").cloned(),
        tail,
        max_samples_per_key: max_samples as usize,
        ignore_unavailable: ctx.bool_param("ignore_unavailable", true)?,
        expand_wildcards: ctx.query("expand_wildcards").unwrap_or("open").to_string(),
    };
    ctx.bool_param("allow_no_indices", true)?;
    Ok(Request { query, settings, keep_alive_ms: keep_alive.map_or(DEFAULT_KEEP_ALIVE_MS, |k| k.as_millis()), keep_on_completion })
}

fn response(took: i64, out: exec::Output) -> Map {
    let (kind, list) = match out {
        exec::Output::Events(e) => ("events", e),
        exec::Output::Sequences(s) => ("sequences", s),
    };
    let mut m = Map::new();
    m.insert("is_partial".into(), false.into());
    m.insert("is_running".into(), false.into());
    m.insert("took".into(), took.into());
    m.insert("timed_out".into(), false.into());
    m.insert(
        "hits".into(),
        obj! {
            "total" => obj! { "value" => list.len(), "relation" => "eq" },
            kind => Value::Arr(list),
        },
    );
    m
}

fn with_id(id: &str, m: &Map) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), id.into());
    out.extend(m.iter().map(|(k, v)| (k.clone(), v.clone())));
    Value::Obj(out)
}

pub async fn search(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let req = parse_request(ctx)?;
    let stmt = parser::parse(&req.query)?;
    let started = std::time::Instant::now();
    let out = exec::run(ctx.cluster, &req.query, &stmt, &req.settings).await?;
    let m = response(started.elapsed().as_millis() as i64, out);
    if !req.keep_on_completion {
        return Reply::ok(Value::Obj(m));
    }
    let id = async_results::async_id(&ctx.cluster.node.id);
    ctx.cluster.xpack.results.put(
        id.clone(),
        Entry { kind: Kind::Eql, response: Value::Obj(m.clone()), start_ms: ctx.now_ms, expiration_ms: ctx.now_ms + req.keep_alive_ms, bytes: 0, extra: Value::Null },
        ctx.now_ms,
    );
    Reply::ok(with_id(&id, &m))
}

fn stored(ctx: &Ctx) -> EsResult<(String, Entry)> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    async_results::check_id(&id)?;
    match ctx.cluster.xpack.results.get(&id, Kind::Eql, ctx.now_ms) {
        Some(e) => Ok((id, e)),
        None => Err(EsError::resource_not_found(id)),
    }
}

pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    ctx.time_param("wait_for_completion_timeout", TimeValue::MINUS_ONE)?;
    let (id, e) = stored(ctx)?;
    if ctx.has_query("keep_alive") {
        let ka = ctx.time_param("keep_alive", TimeValue::MINUS_ONE)?;
        ctx.cluster.xpack.results.touch(&id, ctx.now_ms + ka.as_millis());
    }
    let m = e.response.as_object().cloned().unwrap_or_default();
    Reply::ok(with_id(&id, &m))
}

pub fn status(ctx: &Ctx) -> EsResult<Reply> {
    let (id, e) = stored(ctx)?;
    Reply::ok(obj! {
        "id" => id,
        "is_running" => false,
        "is_partial" => false,
        "expiration_time_in_millis" => e.expiration_ms,
        "completion_status" => 200,
    })
}

pub fn delete(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    async_results::check_id(&id)?;
    if ctx.cluster.xpack.results.remove(&id, Kind::Eql) {
        Reply::acknowledged()
    } else {
        Err(EsError::resource_not_found(id))
    }
}
