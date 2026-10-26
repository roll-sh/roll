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

pub mod columnar;
pub mod exec;
pub mod fulltext;
pub mod functions;
pub mod parser;
pub mod profile;
pub mod spatial;
pub mod value;

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::async_results::{self, Entry, Kind};
use crate::xpack::store;
use value::V;

pub const VIEWS: &str = "esql.view";

/// The `EsqlCapabilities` `POST /_query` reports (`GET _capabilities`): the features and fixes
/// roll's engine implements, so clients and test suites that probe for them use them. Only
/// capabilities whose Elasticsearch YAML tests (x-pack `esql`) roll passes are claimed; the
/// rest (spatial types, union types, `TS`/PromQL, `FORK`, flattened, dense vectors, ...) stay
/// unclaimed until implemented.
pub const CAPABILITIES: &[&str] = &[
    "REPORT_ORIGINAL_TYPES",
    "agg_top",
    "agg_values_spatial",
    "base_conversion",
    "bucket_inclusive_upper_bound",
    "casting_operator",
    "columnar_index_modes",
    "documents_found_and_values_loaded",
    "enrich_strict_range_types",
    "fix_count_distinct_source_error",
    "fix_doubly_released_null_blocks_in_valuesourcereader",
    "fix_index_like_question_mark_wildcard",
    "fix_limit_truncation_with_zero_columns",
    "fix_no_columns",
    "fix_passthrough_field_caps_object_parent",
    "fix_set_wrong_line_column",
    "fn_reverse",
    "fn_st_geometrytype",
    "full_text_functions_disjunctions_compute_engine",
    "global_timezone_parameter_with_output",
    "handle_determinization_complexity",
    "join_lookup_v12",
    "loading_non_indexed_ip_fields",
    "lookup_join_on_multiple_fields",
    "match_additional_types",
    "match_phrase_function",
    "metadata_fields",
    "metadata_ignored_field",
    "optional_fields_fix_load_keyword_with_multifields",
    "optional_fields_unmapped_load_auto_cast_two_legged_punks",
    "optional_fields_unmapped_load_null_fallback",
    "optional_fields_v2",
    "query_monitoring",
    "query_params_multi_values",
    "query_settings_request_body",
    "sort_returning_source_ok",
    "to_lower_empty_string",
    "union_types",
    "union_types_fix_rename_resolution",
    "union_types_remove_fields",
    "usage_contains_took",
];

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

struct Request {
    query: String,
    columnar: bool,
    params: Vec<V>,
    named: Vec<(String, V)>,
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
    let mut params = Vec::new();
    let mut named = Vec::new();
    for p in m.get("params").and_then(Value::as_array).into_iter().flatten() {
        match p {
            Value::Obj(o) if o.len() == 1 && !o.contains_key("value") => {
                let (k, v) = o.iter().next().expect("one");
                named.push((k.clone(), param_value(v)));
            }
            other => params.push(param_value(other)),
        }
    }
    let keep_alive = match m.get("keep_alive").map(Value::to_es_string).or_else(|| ctx.query("keep_alive").map(str::to_string)) {
        Some(k) => crate::units::TimeValue::parse(&k, "keep_alive")?.as_millis(),
        None => KEEP_ALIVE_MS,
    };
    let time_zone = body_time_zone(ctx, m)?;
    Ok(Request {
        query: query.to_string(),
        columnar: m.get("columnar").and_then(Value::as_bool).unwrap_or(false) || ctx.query("columnar") == Some("true"),
        params,
        named,
        filter: m.get("filter").cloned(),
        keep_alive,
        keep_on_completion: m.get("keep_on_completion").and_then(Value::as_bool).unwrap_or(false) || ctx.query("keep_on_completion") == Some("true"),
        drop_null_columns: ctx.query("drop_null_columns") == Some("true"),
        locale: m.get("locale").and_then(Value::as_str).map(str::to_string),
        time_zone,
        profile: m.get("profile").and_then(Value::as_bool).unwrap_or(false),
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
                    "unmapped_fields" | "column_metadata" | "approximation" => {
                        let (l, c) = at(&[Seg::Key("settings"), Seg::Key(k)], Target::Name);
                        return Err(wrap(l, c, format!("Setting [{k}] is not exposed as a request body parameter")));
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
    let mut q = parser::parse(&req.query, &req.params, &req.named)?;
    q.locale = req.locale.clone();
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
    let result = exec::execute(ctx.cluster, q, req.filter.clone(), &mut w).await;
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
        m.insert("original_types".into(), Value::Arr(types.iter().map(|t| Value::from(t.as_str())).collect()));
        if types.len() > 1 {
            let numeric = |t: &str| matches!(t, "long" | "integer" | "short" | "byte" | "double" | "float" | "half_float" | "scaled_float" | "unsigned_long");
            let dates = types.iter().all(|t| matches!(t.as_str(), "date" | "date_nanos"));
            let cast = if types.iter().any(|t| t == "aggregate_metric_double") && types.iter().all(|t| t == "aggregate_metric_double" || numeric(t)) {
                Some("aggregate_metric_double")
            } else if dates {
                None
            } else {
                Some("keyword")
            };
            if let Some(cast) = cast {
                m.insert("suggested_cast".into(), cast.into());
            }
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
    // Running-query ids are `<node>:<task>`; anything else doesn't decode.
    if id.split_once(':').is_none_or(|(_, t)| t.parse::<u64>().is_err()) {
        return Err(EsError::illegal_argument(format!("invalid id: [{id}]")));
    }
    Err(EsError::resource_not_found(format!("query [{id}] not found")))
}

pub async fn put_view(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let Some(q) = body.get("query").and_then(Value::as_str).map(str::to_string) else {
        return Err(EsError::action_request_validation(&["[query] is required".into()]));
    };
    parser::parse(&q, &[], &[])?;
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let seq = st.metadata.xpack.get(VIEWS, &name).and_then(|v| v.get("seq")).and_then(Value::as_i64).unwrap_or(now);
            store::put(st, VIEWS, &name, obj! { "name" => name.clone(), "query" => q, "seq" => seq }, now).map(|_| ())
        })
        .await?;
    Reply::acknowledged()
}

pub fn get_view(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let names = ctx.list_param("name");
    let mut views: Vec<&Value> = st.metadata.xpack.iter(VIEWS).map(|(_, v)| v).collect();
    views.sort_by_key(|v| v.get("seq").and_then(Value::as_i64).unwrap_or(0));
    let mut out = Vec::new();
    match names {
        Some(ns) if !ns.is_empty() => {
            for n in ns {
                match views.iter().find(|v| v.get("name").and_then(Value::as_str) == Some(n.as_str())) {
                    Some(v) => out.push(obj! { "name" => n.clone(), "query" => v.get("query").cloned().unwrap_or(Value::Null) }),
                    None => return Err(EsError::resource_not_found(format!("view [{n}] not found"))),
                }
            }
        }
        _ => {
            for v in views {
                out.push(obj! { "name" => v.get("name").cloned().unwrap_or(Value::Null), "query" => v.get("query").cloned().unwrap_or(Value::Null) });
            }
        }
    }
    Reply::ok(obj! { "views" => Value::Arr(out) })
}

pub async fn delete_view(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    ctx.cluster
        .update(move |st| if store::remove(st, VIEWS, &name).is_some() { Ok(()) } else { Err(EsError::resource_not_found(format!("view [{name}] not found"))) })
        .await?;
    Reply::acknowledged()
}
