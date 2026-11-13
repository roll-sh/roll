//! ES|QL: `POST _query` (and its async variants), the running-queries APIs and views
//! (`_query/view`), over roll's indices.
//!
//! [`parser`] turns a query into commands (and `SET` settings), [`exec`] runs them (scanning
//! indices through [`columnar`]'s doc-values reader or the search API), [`functions`] and
//! [`value`] hold the types, functions and aggregations. Supported: `FROM` (with
//! `METADATA`), `ROW`, `SHOW INFO`, `WHERE`, `EVAL`, `STATS ... BY`, `SORT`, `LIMIT`, `KEEP`,
//! `DROP`, `RENAME`, `MV_EXPAND`, the common scalar, date, string, multi-value and conversion
//! functions, and the aggregations `COUNT`, `COUNT_DISTINCT`, `SUM`, `AVG`, `MIN`, `MAX`,
//! `MEDIAN`, `PERCENTILE`, `VALUES`, `STD_DEV`, `MEDIAN_ABSOLUTE_DEVIATION`, `TOP`. Responses
//! come as JSON (row-wise or `columnar`), `txt`, `csv`, `tsv` or the other xcontent formats.

pub mod capabilities;
pub mod columnar;
pub mod datasources;
pub mod exec;
pub mod fulltext;
pub mod functions;
pub mod parser;
pub mod profile;
pub mod spatial;
pub mod telemetry;
pub mod value;

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::async_results::{self, Entry, Kind};
use crate::xpack::store;
use value::V;

pub const VIEWS: &str = "esql.view";

pub use capabilities::CAPABILITIES;

/// Five days, Elasticsearch's default `keep_alive`.
const KEEP_ALIVE_MS: i64 = 5 * 24 * 3600 * 1000;

fn param_value(v: &Value) -> V {
    match v {
        Value::Null => V::Null,
        Value::Bool(b) => V::Bool(*b),
        Value::Str(s) => V::Str(s.clone()),
        Value::Num(_) => match v.as_i64() {
            Some(i) if v.as_number().is_some_and(|n| n.is_integer()) => i32::try_from(i).map(V::Int).unwrap_or(V::Long(i)),
            _ => V::Double(v.as_f64().unwrap_or(0.0)),
        },
        // An empty list is null; a one-element list its element.
        Value::Arr(a) if a.is_empty() => V::Null,
        Value::Arr(a) if a.len() == 1 => param_value(&a[0]),
        Value::Arr(a) => V::Multi(a.iter().map(param_value).collect()),
        Value::Obj(o) => o.get("value").map(param_value).unwrap_or(V::Null),
    }
}

/// `params`: values (`1`, `{"value": 1}`), named values (`{"n": 1}`) and named identifiers or
/// patterns (`{"f": {"identifier": "x"}}`), with Elasticsearch's parse errors.
fn parse_params(ctx: &Ctx, m: &Map, async_api: bool) -> EsResult<Vec<(Option<String>, parser::Param)>> {
    use crate::json::locate::{Seg, Target, locate};
    let Some(list) = m.get("params").and_then(Value::as_array) else { return Ok(Vec::new()) };
    let parser_name = if async_api { "esql/async_query" } else { "esql/query" };
    let raw = ctx.raw_json();
    let at = |path: &[Seg<'_>]| raw.and_then(|src| locate(src, path, Target::Value)).unwrap_or((1, 1));
    // Errors are located at the end of the `params` array, like the parser leaves it.
    let end = || {
        let (l, c) = at(&[Seg::Key("params")]);
        let text = raw.map(|r| String::from_utf8_lossy(r).into_owned()).unwrap_or_default();
        let start = text.lines().take(l - 1).map(|x| x.len() + 1).sum::<usize>() + c - 1;
        let close = matching_close(text.as_bytes(), start).unwrap_or(start);
        crate::xpack::esql::parser::location(&text, close)
    };
    let fail = |msg: String| {
        let (l, c) = end();
        EsError::x_content_parse(l, c, format!("[{parser_name}] failed to parse field [params]")).caused_by(EsError::x_content_parse_bare(format!("Failed to parse params: {msg}")))
    };
    let mut out = Vec::with_capacity(list.len());
    let (mut named, mut unnamed) = (None, None);
    for (i, p) in list.iter().enumerate() {
        match p {
            Value::Obj(o) if o.len() == 1 && !o.contains_key("value") => {
                let (k, v) = o.iter().next().expect("one");
                let (l, c) = at(&[Seg::Key("params"), Seg::Index(i)]);
                let param = match v {
                    Value::Obj(attrs) if !attrs.is_empty() => {
                        if let Some(bad) = attrs.keys().find(|a| !matches!(a.as_str(), "value" | "identifier" | "pattern")) {
                            return Err(fail(format!("[{l}:{c}] [{bad}] is not a valid param attribute, a valid attribute is any of VALUE, IDENTIFIER, PATTERN")));
                        }
                        if attrs.len() > 1 {
                            let names: Vec<&str> = attrs.keys().map(String::as_str).collect();
                            return Err(fail(format!("[{l}:{c}] [{k}] has multiple param attributes [{}], only one of VALUE, IDENTIFIER, PATTERN can be defined in a param", names.join(", "))));
                        }
                        let (a, x) = attrs.iter().next().expect("one");
                        match (a.as_str(), x) {
                            ("value", x) => parser::Param::Value(param_value(x)),
                            (_, Value::Arr(items)) => {
                                let shown: Vec<String> = items.iter().map(Value::to_es_string).collect();
                                return Err(fail(format!("[{l}:{c}] {k}={{{a}=[{}]}} parameter is multivalued, only VALUE parameters can be multivalued", shown.join(", "))));
                            }
                            (kind, Value::Str(s)) => {
                                if kind == "identifier" {
                                    parser::Param::Identifier(s.clone())
                                } else {
                                    parser::Param::Pattern(s.clone())
                                }
                            }
                            (kind, other) => {
                                let up = kind.to_ascii_uppercase();
                                return Err(fail(format!("[{l}:{c}] [{}] is not a valid value for {up} parameter, a valid value for {up} parameter is a string", other.to_es_string())));
                            }
                        }
                    }
                    v => parser::Param::Value(param_value(v)),
                };
                named.get_or_insert_with(|| p.clone());
                out.push((Some(k.clone()), param));
            }
            other => {
                unnamed.get_or_insert_with(|| other.clone());
                out.push((None, parser::Param::Value(param_value(other))));
            }
        }
    }
    if let (Some(n), Some(u)) = (&named, &unnamed) {
        let show = |v: &Value| match v {
            Value::Obj(o) => o.iter().map(|(k, v)| format!("{k}:{}", match v { Value::Obj(x) => x.values().map(Value::to_es_string).collect::<Vec<_>>().join(","), x => x.to_es_string() })).collect::<Vec<_>>().join(","),
            x => x.to_es_string(),
        };
        return Err(fail(format!("Params cannot contain both named and unnamed parameters; got [{{{}}}] and [{{{}}}]", show(n), show(u))));
    }
    Ok(out)
}

/// The offset of the bracket closing the one at `open`.
fn matching_close(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut i = open;
    while i < b.len() {
        let c = b[i];
        if in_str {
            if c == b'\\' {
                i += 1;
            } else if c == b'"' {
                in_str = false;
            }
        } else if c == b'"' {
            in_str = true;
        } else if c == b'[' || c == b'{' {
            depth += 1;
        } else if c == b']' || c == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

struct Request {
    query: String,
    columnar: bool,
    /// The `params`, in request order, each with its name if it has one.
    params: Vec<(Option<String>, parser::Param)>,
    filter: Option<Value>,
    keep_alive: i64,
    keep_on_completion: bool,
    drop_null_columns: bool,
    /// `locale`: the language of `DATE_FORMAT`'s texts.
    locale: Option<String>,
    /// `time_zone` (or `settings.time_zone`): the zone dates are computed and printed in.
    time_zone: Option<String>,
    /// `profile`: the response carries [`profile::build`]'s section.
    profile: bool,
    /// `settings.column_metadata`.
    column_metadata: bool,
}

fn parse_request(ctx: &Ctx, body: &Value, async_api: bool) -> EsResult<Request> {
    let Some(m) = body.as_object() else { return Err(EsError::action_request_validation(&["[query] is required".into()])) };
    for k in m.keys() {
        let ok = matches!(
            k.as_str(),
            "query" | "columnar" | "params" | "filter" | "locale" | "time_zone" | "settings" | "profile" | "tables" | "include_ccs_metadata" | "include_execution_metadata" | "allow_partial_results" | "project_routing"
        ) || (async_api && matches!(k.as_str(), "keep_alive" | "keep_on_completion" | "wait_for_completion_timeout"));
        if !ok {
            return Err(crate::xpack::unknown_field(ctx, if async_api { "esql/async_query" } else { "esql/query" }, k));
        }
    }
    let Some(query) = m.get("query").and_then(Value::as_str) else {
        return Err(EsError::action_request_validation(&["[query] is required".into()]));
    };
    let params = parse_params(ctx, m, async_api)?;
    let keep_alive = match m.get("keep_alive").map(Value::to_es_string).or_else(|| ctx.query("keep_alive").map(str::to_string)) {
        Some(k) => crate::units::TimeValue::parse(&k, "keep_alive")?.as_millis(),
        None => KEEP_ALIVE_MS,
    };
    let time_zone = body_time_zone(ctx, m)?;
    Ok(Request {
        query: query.to_string(),
        columnar: m.get("columnar").and_then(Value::as_bool).unwrap_or(false) || ctx.query("columnar") == Some("true"),
        params,
        filter: m.get("filter").cloned(),
        keep_alive,
        keep_on_completion: m.get("keep_on_completion").and_then(Value::as_bool).unwrap_or(false) || ctx.query("keep_on_completion") == Some("true"),
        drop_null_columns: ctx.query("drop_null_columns") == Some("true"),
        locale: m.get("locale").and_then(Value::as_str).map(str::to_string),
        time_zone,
        profile: m.get("profile").and_then(Value::as_bool).unwrap_or(false),
        column_metadata: m.get("settings").and_then(|s| s.get("column_metadata")).is_some_and(|v| v.as_bool() == Some(true) || v.as_str() == Some("true")),
    })
}

/// The request's time zone: top-level `time_zone` or `settings.time_zone` (the query settings
/// a request body may carry, `QuerySettings` exposed to the body; the others are `SET`-only).
fn body_time_zone(ctx: &Ctx, m: &Map) -> EsResult<Option<String>> {
    use crate::json::locate::{Seg, Target, locate};
    let parser = if ctx.api == crate::routes::Api::EsqlAsyncQuery { "esql/async_query" } else { "esql/query" };
    let at = |path: &[Seg<'_>], t: Target| ctx.raw_json().and_then(|src| locate(src, path, t)).unwrap_or((1, 1));
    let valid = |z: &str| crate::time::Zone::parse(z).is_ok();
    let top = match m.get("time_zone") {
        Some(v) => {
            let z = v.to_es_string();
            if !valid(&z) {
                let (l, c) = at(&[Seg::Key("time_zone")], Target::Value);
                return Err(EsError::x_content_parse(l, c, format!("[{parser}] failed to parse field [time_zone]")).caused_by(EsError::illegal_argument(format!("Invalid time zone [{z}]"))));
            }
            Some(z)
        }
        None => None,
    };
    let mut nested = None;
    match m.get("settings") {
        None | Some(Value::Null) => {}
        Some(Value::Obj(s)) => {
            for (k, v) in s {
                let wrap = |l: usize, c: usize, msg: String| EsError::x_content_parse(l, c, format!("[{parser}] failed to parse field [settings]")).caused_by(EsError::x_content_parse(l, c, msg));
                match k.as_str() {
                    "time_zone" => {
                        let z = v.to_es_string();
                        if !valid(&z) {
                            let (l, c) = at(&[Seg::Key("settings"), Seg::Key("time_zone")], Target::Value);
                            return Err(wrap(l, c, format!("Failed to parse value for setting [time_zone]: Invalid time zone [{z}]")));
                        }
                        nested = Some(z);
                    }
                    "unmapped_fields" => {
                        let (l, c) = at(&[Seg::Key("settings"), Seg::Key(k)], Target::Name);
                        return Err(wrap(l, c, format!("Setting [{k}] is not exposed as a request body parameter")));
                    }
                    // A hint exact results satisfy.
                    "approximation" => {}
                    "column_metadata" => {
                        if !matches!(v, Value::Bool(_)) && !matches!(v.as_str(), Some("true" | "false")) {
                            let (l, c) = at(&[Seg::Key("settings"), Seg::Key(k)], Target::Value);
                            return Err(wrap(l, c, format!("Failed to parse value for setting [column_metadata]: Failed to parse value [{}] as only [true] or [false] are allowed.", v.to_es_string())));
                        }
                    }
                    _ => {
                        let (l, c) = at(&[Seg::Key("settings"), Seg::Key(k)], Target::Name);
                        return Err(wrap(l, c, format!("Unknown setting [{k}] under [settings]")));
                    }
                }
            }
        }
        Some(other) => {
            let (l, c) = at(&[Seg::Key("settings")], Target::Value);
            let kind = match other {
                Value::Num(_) => "VALUE_NUMBER",
                Value::Bool(_) => "VALUE_BOOLEAN",
                Value::Arr(_) => "START_ARRAY",
                _ => "VALUE_STRING",
            };
            return Err(EsError::x_content_parse(l, c, format!("[{parser}] settings doesn't support values of type: {kind}")));
        }
    }
    match (top, nested) {
        (Some(a), Some(b)) if a != b => Err(EsError::illegal_argument(
            "Setting [time_zone] has conflicting values at the top level of the request body and under [settings]; specify it in only one place, or with the same value in both.",
        )),
        (a, b) => Ok(a.or(b)),
    }
}

/// Runs a request: the JSON response and the table (for text formats).
async fn run(ctx: &Ctx<'_>, req: &Request) -> EsResult<(Map, exec::Table)> {
    let started = std::time::Instant::now();
    let start_ms = ctx.now_ms;
    let kibana = ctx.request.header("x-elastic-product-origin").is_some_and(|o| o.eq_ignore_ascii_case("kibana"));
    let telemetry = &ctx.cluster.xpack.esql_telemetry;
    let mut q = match parser::parse_with(&req.query, &req.params) {
        Ok(q) => q,
        Err(e) => {
            telemetry.record(kibana, None, true, 0);
            return Err(e);
        }
    };
    q.locale = req.locale.clone();
    // `settings.column_metadata` in the body, like `SET column_metadata`.
    if req.column_metadata {
        q.settings.column_metadata = true;
    }
    if q.settings.time_zone.is_none() {
        q.settings.time_zone = req.time_zone.clone();
    }
    let tz = q.settings.time_zone.as_deref().and_then(|z| crate::time::Zone::parse(z).ok()).filter(|z| *z != crate::time::Zone::UTC);
    let unmapped = match q.settings.unmapped {
        parser::Unmapped::Default => "default",
        parser::Unmapped::Nullify => "nullify",
        parser::Unmapped::Load => "load",
    };
    let mut w = value::Warnings::default();
    let shape = if req.profile { profile::Shape::of(&q) } else { profile::Shape::default() };
    let used = telemetry::Used::of(&q, &ctx.state());
    let column_metadata = q.settings.column_metadata;
    let result = exec::execute(ctx.cluster, q, req.filter.clone(), &mut w).await;
    telemetry.record(kibana, Some(&used), result.is_err(), started.elapsed().as_millis() as i64);
    // Warnings recorded before a failure (the implicit limit's, added while analyzing) go out
    // with the error, as Elasticsearch's do.
    for msg in w.list {
        ctx.warn(msg);
    }
    let (mut table, stats) = result?;
    let mut out = Map::new();
    let took = started.elapsed();
    out.insert("took".into(), (took.as_millis() as i64).into());
    out.insert("is_partial".into(), false.into());
    out.insert("completion_time_in_millis".into(), (start_ms + took.as_millis() as i64).into());
    out.insert("documents_found".into(), stats.documents_found.into());
    out.insert("values_loaded".into(), stats.values_loaded.into());
    out.insert("rows_emitted".into(), stats.rows_emitted.into());
    out.insert("bytes_read".into(), 0.into());
    out.insert("read_nanos".into(), 0.into());
    out.insert("cpu_nanos".into(), (took.as_nanos() as i64).into());
    out.insert("start_time_in_millis".into(), start_ms.into());
    out.insert("expiration_time_in_millis".into(), (start_ms + req.keep_alive).into());
    if req.drop_null_columns {
        out.insert("all_columns".into(), exec::columns_json(&table));
        let keep = exec::non_null_columns(&table);
        table = exec::Table {
            cols: keep.iter().map(|i| table.cols[*i].clone()).collect(),
            rows: table.rows.iter().map(|r| keep.iter().map(|i| r[*i].clone()).collect()).collect(),
        };
    }
    let mut columns = exec::columns_json(&table);
    union_types(&mut columns, &stats.original_types);
    if column_metadata && let Value::Arr(cols) = &mut columns {
        for c in cols.iter_mut() {
            if let Value::Obj(m) = c
                && let Some((_, meta)) = stats.col_meta.iter().rev().find(|(n, _)| Some(n.as_str()) == m.get("name").and_then(Value::as_str))
            {
                m.insert("_meta".into(), meta.clone());
            }
        }
    }
    if let Some(all) = out.get_mut("all_columns") {
        union_types(all, &stats.original_types);
    }
    out.insert("columns".into(), columns);
    out.insert("values".into(), exec::values_json_in(&table, req.columnar, tz.as_ref()));
    if req.profile {
        out.insert("profile".into(), profile::build(ctx.cluster, start_ms, took.as_nanos() as i64, unmapped, &shape, &stats, &table));
    }
    Ok((out, table))
}

/// Adds `original_types` (and, for several, Elasticsearch's `suggested_cast`) to the
/// `unsupported` source columns of a `columns` array.
fn union_types(columns: &mut Value, original: &[(String, Vec<String>)]) {
    let Value::Arr(cols) = columns else { return };
    for c in cols {
        let Value::Obj(m) = c else { continue };
        if m.get("type").and_then(Value::as_str) != Some("unsupported") {
            continue;
        }
        let Some((_, types)) = original.iter().find(|(n, _)| Some(n.as_str()) == m.get("name").and_then(Value::as_str)) else { continue };
        let mut names: Vec<&str> = types.iter().map(|t| exec::es_type_name(t)).collect();
        names.sort();
        m.insert("original_types".into(), Value::Arr(names.iter().map(|t| Value::from(*t)).collect()));
        if types.len() > 1 {
            let numeric = |t: &str| matches!(t, "long" | "integer" | "short" | "byte" | "double" | "float" | "half_float" | "scaled_float" | "unsigned_long");
            let cast = if types.iter().any(|t| t == "aggregate_metric_double") && types.iter().all(|t| t == "aggregate_metric_double" || numeric(t)) {
                "aggregate_metric_double"
            } else if types.iter().all(|t| matches!(t.as_str(), "date" | "date_nanos")) {
                "date_nanos"
            } else {
                "keyword"
            };
            m.insert("suggested_cast".into(), cast.into());
        }
    }
}

/// The response in the requested format (`txt`, `csv`, `tsv` are rendered here).
fn reply(ctx: &Ctx, out: Map, table: &exec::Table) -> EsResult<Reply> {
    let format = ctx.query("format").map(str::to_ascii_lowercase).or_else(|| {
        ctx.request.header("accept").and_then(|a| {
            let a = a.to_ascii_lowercase();
            if a.starts_with("text/plain") {
                Some("txt".into())
            } else if a.starts_with("text/csv") {
                Some("csv".into())
            } else if a.starts_with("text/tab-separated-values") {
                Some("tsv".into())
            } else {
                None
            }
        })
    });
    match format.as_deref() {
        Some(f @ ("txt" | "csv" | "tsv")) => {
            let delimiter = ctx.query("delimiter").and_then(|d| d.chars().next()).unwrap_or(',');
            if f != "csv" && ctx.query("delimiter").is_some() {
                return Err(EsError::illegal_argument(format!("parameter: [delimiter] can only be used with the format [csv] for request [{}]", ctx.path)));
            }
            let names: Vec<String> = table.cols.iter().map(|c| c.name.clone()).collect();
            let rows: Vec<Vec<String>> = table.rows.iter().map(|r| r.iter().enumerate().map(|(i, v)| exec::cell_text(v, table.cols[i].ty, f)).collect()).collect();
            let header = !(f == "csv" && ctx.query("header") == Some("absent"));
            let body = exec::render_text(&names, &rows, f, delimiter, header);
            let ct = match f {
                "txt" => "text/plain; charset=UTF-8",
                "csv" => "text/csv; charset=utf-8; header=present",
                _ => "text/tab-separated-values; charset=utf-8",
            };
            Ok(Reply::Raw(200, ct.into(), bytes::Bytes::from(body)))
        }
        Some(f) if !matches!(f, "json" | "yaml" | "cbor" | "smile") => Err(EsError::illegal_argument(format!("Invalid format [{f}], valid values are [csv, json, tsv, txt, yaml, cbor, smile, arrow]"))),
        _ => Reply::ok(Value::Obj(out)),
    }
}

pub async fn query(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let req = parse_request(ctx, &body, false)?;
    let (out, table) = run(ctx, &req).await?;
    reply(ctx, out, &table)
}

fn with_id(id: &str, running: bool, out: &Map) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), id.into());
    m.insert("is_running".into(), running.into());
    for (k, v) in out {
        m.insert(k.clone(), v.clone());
    }
    Value::Obj(m)
}

pub async fn async_query(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let req = parse_request(ctx, &body, true)?;
    let (out, table) = run(ctx, &req).await?;
    // Every query completes before answering; it is kept only when asked to be.
    if !req.keep_on_completion {
        let mut m = Map::with_capacity(out.len() + 1);
        m.insert("is_running".into(), false.into());
        m.extend(out);
        return reply(ctx, m, &table);
    }
    let id = async_results::async_id(&ctx.cluster.node.id);
    ctx.cluster.xpack.results.put(
        id.clone(),
        Entry { kind: Kind::Esql, response: Value::Obj(out.clone()), start_ms: ctx.now_ms, expiration_ms: ctx.now_ms + req.keep_alive, bytes: 0, extra: Value::Null },
        ctx.now_ms,
    );
    Reply::ok(with_id(&id, false, &out))
}

fn not_found(id: &str) -> EsError {
    EsError::resource_not_found(id.to_string())
}

pub fn async_get(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default();
    // An id that doesn't decode is a bad request, before any lookup.
    async_results::check_id(id)?;
    let Some(e) = ctx.cluster.xpack.results.get(id, Kind::Esql, ctx.now_ms) else { return Err(not_found(id)) };
    if let Some(k) = ctx.query("keep_alive") {
        let ms = crate::units::TimeValue::parse(k, "keep_alive")?.as_millis();
        ctx.cluster.xpack.results.touch(id, ctx.now_ms + ms);
    }
    let out = e.response.as_object().cloned().unwrap_or_default();
    Reply::ok(with_id(id, false, &out))
}

pub fn async_stop(ctx: &Ctx) -> EsResult<Reply> {
    async_get(ctx)
}

pub fn async_delete(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default();
    async_results::check_id(id)?;
    if ctx.cluster.xpack.results.remove(id, Kind::Esql) {
        Reply::acknowledged()
    } else {
        Err(not_found(id))
    }
}

/// Queries run to completion inside their request, so none is ever listed as running.
pub fn list_queries(_ctx: &Ctx) -> EsResult<Reply> {
    Reply::ok(obj! { "queries" => obj! {} })
}

pub fn get_query(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default();
    // Ids are `AsyncExecutionId`s; the task they name runs on its node.
    let task = async_results::task_of(id)?;
    if let Some((node, _)) = task.split_once(':')
        && node != ctx.cluster.node.id
    {
        return Err(EsError::new(500, "exception", format!("task [{task}] belongs to the node [{node}] which isn't part of the cluster and there is no record of the task")));
    }
    Err(EsError::resource_not_found(format!("query [{id}] not found")))
}

/// Whether `name` is an ES|QL view (index, alias and data stream names can't take it).
pub fn is_view(st: &crate::meta::ClusterState, name: &str) -> bool {
    st.metadata.xpack.get(VIEWS, name).is_some()
}

/// `PUT _query/view/{name}`: Elasticsearch's request parsing and validation, then the name
/// conflicts with indices, aliases and data streams.
pub async fn put_view(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    let body = datasources::Body::read(ctx, "view", &[("query", true)])?;
    let q = body.required("query")?;
    let more = if q.trim().is_empty() { vec!["view query is missing or empty".to_string()] } else { Vec::new() };
    datasources::validate_name_with("view", &name, more)?;
    // A view's query takes no parameters and no `SET`.
    if let Err(e) = parser::parse(&q, &[], &[]) {
        let reason = e.reason().replace("'promql', 'set', 'show'", "'promql', 'show'");
        return Err(if reason == e.reason() { e } else { EsError::new(400, "parsing_exception", reason).caused_by(EsError::java(400, "input_mismatch_exception", "").with_null_reason()) });
    }
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let conflict = |what: &str| Err(EsError::resource_already_exists(format!("view [{name}] cannot be created, an existing {what} with that name is present")));
            if st.metadata.indices.contains_key(&name) {
                return conflict("concrete index");
            }
            if st.metadata.data_streams.contains_key(&name) {
                return conflict("data_stream");
            }
            if st.has_alias(&name) || st.metadata.data_stream_aliases.contains_key(&name) {
                return conflict("alias");
            }
            let seq = match st.metadata.xpack.get(VIEWS, &name).and_then(|v| v.get("seq")).and_then(Value::as_i64) {
                Some(s) => s,
                None => st.metadata.xpack.iter(VIEWS).filter_map(|(_, v)| v.get("seq").and_then(Value::as_i64)).max().unwrap_or(0) + 1,
            };
            store::put(st, VIEWS, &name, obj! { "name" => name.clone(), "query" => q, "seq" => seq }, now).map(|_| ())
        })
        .await?;
    Reply::acknowledged()
}

/// `GET _query/view[/{name}]`: views in Elasticsearch's `HashMap` order; every concrete name
/// must exist, wildcards may match nothing.
pub fn get_view(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let all = datasources::hash_ordered(&st, VIEWS);
    let name_of = |v: &Value| v.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
    let view = |v: &Value| obj! { "name" => name_of(v), "query" => v.get("query").cloned().unwrap_or(Value::Null) };
    let out: Vec<Value> = match ctx.list_param("name").filter(|n| !n.is_empty()) {
        None => all.iter().map(|v| view(v)).collect(),
        Some(names) => {
            if let Some(missing) = names.iter().find(|n| !n.contains('*') && *n != "_all" && !all.iter().any(|v| name_of(v) == **n)) {
                return Err(EsError::resource_not_found(format!("view [{missing}] not found")));
            }
            all.iter().filter(|v| names.iter().any(|p| p == "_all" || crate::xcontent::filter::glob_match(p, &name_of(v)))).map(|v| view(v)).collect()
        }
    };
    Reply::ok(obj! { "views" => Value::Arr(out) })
}

/// `DELETE _query/view/{name}`: names, wildcards and `_all`; nothing is deleted when a
/// concrete name is missing.
pub async fn delete_view(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let names: Vec<String> = ctx.list_param("name").unwrap_or_default();
    ctx.cluster
        .update(move |st| {
            if let Some(n) = names.iter().find(|n| !n.contains('*') && *n != "_all" && st.metadata.xpack.get(VIEWS, n).is_none()) {
                return Err(EsError::resource_not_found(format!("view [{n}] not found")));
            }
            let doomed: Vec<String> = st.metadata.xpack.iter(VIEWS).map(|(k, _)| k.clone()).filter(|k| names.iter().any(|p| p == "_all" || crate::xcontent::filter::glob_match(p, k))).collect();
            for k in doomed {
                store::remove(st, VIEWS, &k);
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}
