//! PPL execution: the source scan, the commands over rows, and `explain`.
//!
//! The source's indices are resolved through `_mapping` into a [`Fields`] table: every path
//! with its PPL type, the top-level fields in the order the plugin lists them (a Java
//! `HashMap` over the merged `properties`) and the metadata fields. Documents come from a
//! scroll over the indices (`_source`, in index order); the `search` command's terms and
//! `earliest`/`latest` become its `query_string`/`range` query, so they keep OpenSearch's
//! query semantics. The rows then flow through the commands in memory. Expressions are
//! compiled once per command against the current columns ([`Scope`]): name resolution,
//! type checking and the plugin's analysis errors happen there, evaluation is a walk over the
//! compiled tree with no lookups.
//!
//! Like the plugin, the final result is capped at `plugins.query.size_limit` rows
//! (`LogicalSystemLimit`), while aggregations see every document.

use super::func;
use super::parser::{self, Agg, Cmd, E, Expr, Group, Op, Query, Search};
use super::value::{self, Key, Ty, Val};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::xpack::internal;
use chrono::{Datelike, NaiveDateTime};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct Col {
    pub name: String,
    pub ty: Ty,
    /// Metadata fields (`_id`, `_index`, ...): readable but not printed until `fields` names
    /// them.
    pub hidden: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Table {
    pub cols: Vec<Col>,
    pub rows: Vec<Vec<Val>>,
    /// The source's field paths, for reading inside struct columns.
    pub fields: Arc<Fields>,
}

impl Table {
    pub fn visible(mut self) -> Table {
        if self.cols.iter().any(|c| c.hidden) {
            let keep: Vec<bool> = self.cols.iter().map(|c| !c.hidden).collect();
            self.cols.retain(|c| !c.hidden);
            for r in &mut self.rows {
                let mut i = 0;
                r.retain(|_| {
                    i += 1;
                    keep[i - 1]
                });
            }
        }
        self
    }
}

/// Timings for `profile: true`.
#[derive(Default)]
pub struct Profile {
    pub analyze: std::time::Duration,
    pub execute: std::time::Duration,
    pub total: std::time::Duration,
}

impl Profile {
    /// The plugin's `profile` object (milliseconds rounded to two decimals; a phase that ran
    /// reports at least 0.01).
    pub fn to_value(&self, rows: usize) -> Value {
        let ms = |d: std::time::Duration| (d.as_secs_f64() * 1000.0 * 100.0).round() / 100.0;
        let tiny = |d: std::time::Duration| ms(d).max(0.01);
        crate::obj! {
            "summary" => crate::obj! { "total_time_ms" => tiny(self.total) },
            "phases" => crate::obj! {
                "analyze" => crate::obj! { "time_ms" => tiny(self.analyze) },
                "optimize" => crate::obj! { "time_ms" => tiny(self.analyze) },
                "execute" => crate::obj! { "time_ms" => tiny(self.execute) },
                "format" => crate::obj! { "time_ms" => 0.0 }
            },
            "plan" => crate::obj! { "node" => "CalciteEnumerableIndexScan", "time_ms" => tiny(self.execute), "rows" => rows as i64 },
            "thread_pool" => "sql-worker"
        }
    }
}

// ------------------------------------------------------------------------------ fields

/// One mapped path.
#[derive(Clone, Debug)]
pub struct Field {
    pub ty: Ty,
    pub mtype: String,
    /// The date formats (`format`), for parsing `_source` dates.
    pub format: Option<String>,
    /// A `.keyword` sub-field exists.
    pub keyword: bool,
    /// For an `alias` field, the path it reads.
    pub alias: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Fields {
    /// Every path, parents before their children (top-level in `HashMap` order).
    pub paths: indexmap::IndexMap<String, Field>,
    /// The top-level fields (the default columns).
    pub top: Vec<String>,
    /// The concrete indices.
    pub indices: Vec<String>,
}

pub const META: &[(&str, Ty)] = &[("_id", Ty::Str), ("_index", Ty::Str), ("_score", Ty::Float), ("_maxscore", Ty::Float), ("_sort", Ty::Long), ("_routing", Ty::Str)];

impl Fields {
    pub fn get(&self, path: &str) -> Option<&Field> {
        self.paths.get(path)
    }

    fn available(&self) -> Vec<Value> {
        self.paths.keys().map(|k| Value::Str(k.clone())).chain(META.iter().map(|(m, _)| Value::Str((*m).into()))).collect()
    }
}

/// Merges the `_mapping` response of the source's indices.
fn fields_of(mappings: &Value) -> Fields {
    let mut merged = Map::new();
    let mut indices = Vec::new();
    if let Value::Obj(m) = mappings {
        for (index, body) in m {
            indices.push(index.clone());
            if let Some(Value::Obj(props)) = body.pointer(&["mappings", "properties"]) {
                merge_props(&mut merged, props);
            }
        }
    }
    let mut f = Fields { indices, ..Default::default() };
    let merged = sized_hash_map(merged);
    f.top = merged.keys().cloned().collect();
    walk(&mut f, "", &merged);
    // Aliases read as their target's type.
    let aliases: Vec<(String, String)> = f.paths.iter().filter_map(|(k, v)| v.alias.clone().map(|t| (k.clone(), t))).collect();
    for (k, target) in aliases {
        if let Some(t) = f.paths.get(&target).cloned() {
            let a = f.paths.get_mut(&k).expect("alias");
            a.ty = t.ty;
            a.mtype = t.mtype;
            a.format = t.format;
            a.keyword = t.keyword;
        }
    }
    f
}

/// The plugin reads `properties` into `new HashMap<>(size)` in the mapping's (alphabetical)
/// order, which fixes the order fields list in.
fn sized_hash_map(mut m: Map) -> Map {
    m.sort_keys();
    let n = m.len();
    crate::json::java_hash_map(m, n)
}

fn merge_props(into: &mut Map, props: &Map) {
    for (k, v) in props {
        match into.get_mut(k) {
            None => {
                into.insert(k.clone(), v.clone());
            }
            Some(existing) => {
                let (a, b) = (mtype(existing), mtype(v));
                if a == "object" && b == "object" {
                    if let (Some(Value::Obj(sub)), Some(Value::Obj(add))) = (existing.as_object_mut().and_then(|o| o.get_mut("properties")), v.get("properties")) {
                        merge_props(sub, add);
                    }
                } else if a != b && Ty::of_mapping(a) == Ty::of_mapping(b) && Ty::of_mapping(a) != Ty::Str {
                    // `date` and `date_nanos` (or two integer widths) still read as one type: the
                    // first index's mapping stands for both.
                } else if a != b && Ty::of_mapping(a).is_numeric() && Ty::of_mapping(b).is_numeric() {
                    if Ty::widen(Ty::of_mapping(a), Ty::of_mapping(b)) == Ty::of_mapping(b) {
                        *existing = v.clone();
                    }
                } else if a != b {
                    // Conflicting types: text and keyword read as a string without the keyword
                    // sub-field; anything else becomes a plain text field.
                    let mut t = Map::new();
                    t.insert("type".into(), Value::Str(if a == "object" || b == "object" { "object".into() } else { "text".into() }));
                    if a == "object" || b == "object" {
                        let obj = if a == "object" { existing.clone() } else { v.clone() };
                        *existing = obj;
                    } else {
                        *existing = Value::Obj(t);
                    }
                } else if a == "text"
                    && existing.pointer(&["fields", "keyword"]).is_some()
                    && v.pointer(&["fields", "keyword"]).is_none()
                    && let Some(o) = existing.as_object_mut()
                {
                    o.shift_remove("fields");
                }
            }
        }
    }
}

fn mtype(v: &Value) -> &str {
    v.get("type").and_then(Value::as_str).unwrap_or("object")
}

fn walk(f: &mut Fields, prefix: &str, props: &Map) {
    for (k, v) in props {
        let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        let mt = mtype(v).to_string();
        let alias = if mt == "alias" { v.get("path").and_then(Value::as_str).map(str::to_string) } else { None };
        let field = Field { ty: Ty::of_mapping(&mt), mtype: mt, format: v.get("format").and_then(Value::as_str).map(str::to_string), keyword: v.pointer(&["fields", "keyword"]).is_some(), alias };
        f.paths.insert(path.clone(), field);
        if let Some(Value::Obj(sub)) = v.get("properties") {
            walk(f, &path, &sized_hash_map(sub.clone()));
        }
    }
}

// ------------------------------------------------------------------------------ reading _source

/// Reads `path` out of a `_source` object: nested objects, flattened dotted keys, or a mix.
pub fn extract<'v>(src: &'v Value, path: &str) -> Option<&'v Value> {
    if let Some(v) = src.get(path) {
        return Some(v);
    }
    let Value::Obj(m) = src else { return None };
    for (k, v) in m {
        if let Some(rest) = path.strip_prefix(k.as_str()).and_then(|r| r.strip_prefix('.'))
            && let Some(found) = extract(v, rest)
        {
            return Some(found);
        }
    }
    None
}

/// A `_source` object with dotted keys expanded into objects and merged (`{"log.json": {...}}`
/// reads as `{"log": {"json": {...}}}`), as the plugin reads documents. Keys with an empty
/// path segment (`.`, `a..b`, `.a`, `a.`) name no field and are dropped.
pub fn unflatten(m: &Map) -> Map {
    let mut out = Map::new();
    for (k, v) in m {
        let v = match v {
            Value::Obj(inner) => Value::Obj(unflatten(inner)),
            Value::Arr(a) => Value::Arr(a.iter().map(|x| if let Value::Obj(o) = x { Value::Obj(unflatten(o)) } else { x.clone() }).collect()),
            other => other.clone(),
        };
        if !k.contains('.') {
            merge_into(&mut out, k, v);
            continue;
        }
        let parts: Vec<&str> = k.split('.').collect();
        if parts.iter().any(|p| p.is_empty()) {
            continue;
        }
        let mut nested = v;
        for p in parts[1..].iter().rev() {
            let mut o = Map::new();
            o.insert((*p).to_string(), nested);
            nested = Value::Obj(o);
        }
        merge_into(&mut out, parts[0], nested);
    }
    out
}

fn merge_into(out: &mut Map, k: &str, v: Value) {
    match (out.get_mut(k), v) {
        (Some(Value::Obj(existing)), Value::Obj(add)) => {
            for (k2, v2) in add {
                merge_into(existing, &k2, v2);
            }
        }
        (Some(_), Value::Obj(add)) if add.is_empty() => {}
        (Some(slot), v) if !matches!(v, Value::Obj(ref o) if o.is_empty()) => *slot = v,
        (Some(_), _) => {}
        (None, v) => {
            out.insert(k.to_string(), v);
        }
    }
}

/// Converts a `_source` value of a mapped path to a cell.
pub fn from_source(v: &Value, path: &str, fields: &Fields) -> Val {
    let f = fields.get(path);
    let ty = f.map_or(Ty::Undefined, |f| f.ty);
    convert(v, ty, f.and_then(|f| f.format.as_deref()), path, fields)
}

fn convert(v: &Value, ty: Ty, format: Option<&str>, path: &str, fields: &Fields) -> Val {
    match v {
        Value::Null => Val::Null,
        Value::Arr(a) if ty != Ty::Array && ty != Ty::GeoPoint => match a.len() {
            0 => Val::Null,
            _ => Val::Arr(a.iter().map(|x| convert(x, ty, format, path, fields)).collect()),
        },
        _ => match ty {
            Ty::Bool => match v {
                Value::Bool(b) => Val::Bool(*b),
                Value::Str(s) => s.parse().map(Val::Bool).unwrap_or(Val::Null),
                _ => Val::Null,
            },
            Ty::Byte | Ty::Short | Ty::Int | Ty::Long => match v {
                Value::Num(n) => n.as_i64().map(Val::Int).unwrap_or_else(|| Val::Int(n.as_f64() as i64)),
                Value::Str(s) => s.trim().parse::<i64>().map(Val::Int).or_else(|_| s.trim().parse::<f64>().map(|f| Val::Int(f as i64))).unwrap_or(Val::Null),
                _ => Val::Null,
            },
            Ty::Float | Ty::Double => match v {
                Value::Num(n) => Val::Float(n.as_f64()),
                Value::Str(s) => s.trim().parse().map(Val::Float).unwrap_or(Val::Null),
                _ => Val::Null,
            },
            Ty::Str | Ty::Ip => match v {
                Value::Str(s) => Val::Str(s.clone()),
                Value::Obj(_) => Val::Json(v.clone()),
                other => Val::Str(other.to_es_string()),
            },
            Ty::Timestamp | Ty::Date | Ty::Time => date_cell(v, format).unwrap_or(Val::Null),
            Ty::Struct => match v {
                Value::Obj(m) => Val::Json(struct_value(m, path, fields)),
                // A scalar where the mapping has an object (dynamic: false, a malformed path).
                _ => Val::Null,
            },
            Ty::Array => match v {
                Value::Arr(a) => Val::Json(Value::Arr(a.iter().map(|x| if let Value::Obj(m) = x { struct_value(m, path, fields) } else { x.clone() }).collect())),
                Value::Obj(m) => Val::Json(Value::Arr(vec![struct_value(m, path, fields)])),
                other => Val::Json(other.clone()),
            },
            _ => match v {
                Value::Bool(b) => Val::Bool(*b),
                Value::Num(n) => n.as_i64().map(Val::Int).unwrap_or(Val::Float(n.as_f64())),
                Value::Str(s) => Val::Str(s.clone()),
                other => Val::Json(other.clone()),
            },
        },
    }
}

/// An object cell: its mapped children converted (dates printed), in `HashMap` order.
fn struct_value(m: &Map, path: &str, fields: &Fields) -> Value {
    let mut out = Map::new();
    for (k, v) in m {
        let sub = format!("{path}.{k}");
        let cell = match fields.get(&sub) {
            Some(f) => convert(v, f.ty, f.format.as_deref(), &sub, fields).to_json(f.ty),
            None => v.clone(),
        };
        out.insert(k.clone(), cell);
    }
    Value::Obj(crate::json::java_hash_map(out, 16))
}

fn date_cell(v: &Value, format: Option<&str>) -> Option<Val> {
    let fmt = format.unwrap_or("strict_date_optional_time||epoch_millis");
    let text = match v {
        Value::Num(n) => {
            if fmt.contains("epoch_second") && !fmt.contains("epoch_millis") {
                return value::ts_from_millis((n.as_f64() * 1000.0) as i64).map(Val::Ts);
            }
            return value::ts_from_millis(n.as_f64() as i64).map(Val::Ts);
        }
        Value::Str(s) => s,
        _ => return None,
    };
    let f = crate::time::DateFormatter::new(fmt).ok()?;
    let i = f.parse(text, None, false).ok()?;
    chrono::DateTime::from_timestamp(i.secs, i.nanos).map(|d| Val::Ts(d.naive_utc()))
}

// ------------------------------------------------------------------------------ settings

#[derive(Clone)]
pub struct Settings {
    pub size_limit: usize,
    /// `plugins.ppl.syntax.legacy.preferred`: integer division truncates.
    pub legacy: bool,
    pub now: NaiveDateTime,
    /// The query text (field errors report positions in it).
    pub src: Arc<str>,
}

fn settings(cluster: &Cluster, now_ms: i64, src: &Arc<str>) -> Settings {
    let st = cluster.state();
    let get = |k: &str| st.metadata.cluster_setting(k).map(str::to_string);
    Settings {
        size_limit: get("plugins.query.size_limit").and_then(|v| v.parse().ok()).unwrap_or(10_000),
        legacy: get("plugins.ppl.syntax.legacy.preferred").is_none_or(|v| v != "false"),
        now: value::ts_from_millis(now_ms).unwrap_or_default(),
        src: src.clone(),
    }
}

// ------------------------------------------------------------------------------ running

pub async fn run(cluster: &Cluster, q: &Query, now_ms: i64, prof: &mut Profile) -> EsResult<Table> {
    let s = settings(cluster, now_ms, &q.text);
    let started = std::time::Instant::now();
    let mut t = run_query(cluster, q, &s).await?;
    prof.execute = started.elapsed();
    prof.analyze = prof.execute / 4;
    t.rows.truncate(s.size_limit);
    Ok(t.visible())
}

/// A sub-query (`join`, `lookup`): every row, metadata columns dropped.
pub async fn run_all(cluster: &Cluster, q: &Query, s: &Settings) -> EsResult<Table> {
    Ok(run_query(cluster, q, s).await?.visible())
}

fn run_query<'a>(cluster: &'a Cluster, q: &'a Query, s: &'a Settings) -> std::pin::Pin<Box<dyn std::future::Future<Output = EsResult<Table>> + Send + 'a>> {
    Box::pin(async move {
        let (mut t, consumed) = source(cluster, &q.source, &q.cmds, s).await?;
        let mut first = true;
        for (i, c) in q.cmds.iter().enumerate() {
            // A filter the scan's query applied exactly (with the query's semantics: `NOT
            // x = false` keeps documents without `x`, as the plugin's pushed-down filter does).
            if consumed.get(i).copied().unwrap_or(false) {
                continue;
            }
            t = match c {
                // Right on the scan, the plugin pushes `stats` down as an aggregation: sums add
                // in doubles there.
                Cmd::Stats { aggs, by, bucket_nullable } if first => stats(t, aggs, by, *bucket_nullable, s, true)?,
                c => command(cluster, t, c, s).await?,
            };
            first = false;
        }
        Ok(t)
    })
}

/// Percent-encodes an index expression for a request path.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.*,:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Resolves and scans the source. `cmds` are the commands that follow it: their leading
/// filters and row limit go into the scan ([`pushdown`]).
async fn source(cluster: &Cluster, src: &parser::Source, cmds: &[Cmd], s: &Settings) -> EsResult<(Table, Vec<bool>)> {
    let names = src.indices.join(",");
    let mappings = match internal::call(cluster, "GET", &format!("/{}/_mapping?expand_wildcards=open&allow_no_indices=false", encode(&names)), None).await {
        Ok(v) => v,
        Err(e) if e.kind == "index_not_found_exception" => {
            let missing = e.meta().iter().find(|(k, _)| k == "index").and_then(|(_, v)| v.as_str().map(str::to_string)).unwrap_or_else(|| names.clone());
            return Err(index_not_found(&missing));
        }
        Err(e) => return Err(e),
    };
    let fields = Arc::new(fields_of(&mappings));
    if fields.indices.is_empty() {
        return Err(index_not_found(&names));
    }
    let mut cols: Vec<Col> = fields.top.iter().map(|n| Col { name: n.clone(), ty: fields.paths[n].ty, hidden: false }).collect();
    for (m, ty) in META {
        cols.push(Col { name: (*m).into(), ty: *ty, hidden: true });
    }
    // `plugins.calcite.pushdown.enabled: false` runs every command over the full scan.
    let pushdown_on = cluster.state().metadata.cluster_setting("plugins.calcite.pushdown.enabled").is_none_or(|v| v != "false");
    let pd = if pushdown_on { pushdown(cmds, &fields) } else { Pushdown { consumed: vec![false; cmds.len()], ..Default::default() } };
    let mut filters = pd.filters.clone();
    if let Some(f) = &src.filter {
        filters.insert(0, search_query(f)?);
    }
    let query = match filters.len() {
        0 => crate::obj! { "match_all" => Value::object() },
        1 => filters.pop().expect("one"),
        _ => crate::obj! { "bool" => crate::obj! { "filter" => Value::Arr(filters) } },
    };
    let mut rows = Vec::new();
    let top = fields.top.clone();
    let on_hit = |hit: &Value| {
        let srcv = match hit.get("_source") {
            Some(Value::Obj(m)) => Value::Obj(unflatten(m)),
            _ => Value::Null,
        };
        let mut row: Vec<Val> = top
            .iter()
            .map(|n| {
                let at = fields.paths[n].alias.as_deref().map_or_else(|| srcv.get(n), |t| extract(&srcv, t));
                at.map_or(Val::Null, |v| from_source(v, n, &fields))
            })
            .collect();
        row.push(hit.get("_id").and_then(Value::as_str).map_or(Val::Null, |s| Val::Str(s.into())));
        row.push(hit.get("_index").and_then(Value::as_str).map_or(Val::Null, |s| Val::Str(s.into())));
        row.push(Val::Null);
        row.push(Val::Null);
        row.push(Val::Null);
        row.push(hit.get("_routing").and_then(Value::as_str).map_or(Val::Null, |s| Val::Str(s.into())));
        rows.push(row);
    };
    let target = prune(cluster, &names, &fields, &pd).await?;
    if let Some(target) = target {
        scan(cluster, &target, &fields.indices, &query, pd.limit.map(|n| n.min(s.size_limit)), on_hit).await?;
    }
    Ok((Table { cols, rows, fields }, pd.consumed))
}

fn index_not_found(name: &str) -> EsError {
    let mut ctx = Map::new();
    ctx.insert("stage".into(), Value::Str("analyzing".into()));
    ctx.insert("index_name".into(), Value::Str(name.into()));
    ctx.insert("stage_description".into(), Value::Str("Parsing and validating the query".into()));
    super::error(
        404,
        "IndexNotFoundException",
        format!("no such index [{name}]"),
        vec![
            ("context", Value::Obj(ctx)),
            ("location", Value::Arr(vec![Value::Str("while preparing and validating the query plan".into()), Value::Str("while fetching index mappings".into())])),
            ("code", Value::Str("INDEX_NOT_FOUND".into())),
        ],
    )
}

/// What the commands right after the source let the scan do.
#[derive(Debug, Default)]
struct Pushdown {
    /// Queries every row the pipeline keeps matches (a superset filter: the commands still run).
    filters: Vec<Value>,
    /// The most rows the pipeline can use (`head` after only pushed filters).
    limit: Option<usize>,
    /// A range on `@timestamp` among the pushed filters (for index pruning).
    timestamp_range: Option<Value>,
    /// The commands (by position) the query applies exactly, which don't run again.
    consumed: Vec<bool>,
}

fn pushdown(cmds: &[Cmd], fields: &Fields) -> Pushdown {
    let mut pd = Pushdown { consumed: vec![false; cmds.len()], ..Default::default() };
    let mut exact = true;
    // After `fields`, a filter still runs (it may name a field the projection dropped).
    let mut projected = false;
    for (i, c) in cmds.iter().enumerate() {
        match c {
            Cmd::Where(e) => match to_dsl(e, fields) {
                Some((q, x)) => {
                    pd.consumed[i] = x && exact && !projected;
                    exact &= x;
                    if let Some(r) = timestamp_range(&q) {
                        pd.timestamp_range = Some(r);
                    }
                    pd.filters.push(q);
                }
                None => exact = false,
            },
            Cmd::Fields { .. } => projected = true,
            Cmd::Head { n, from } => {
                if exact {
                    pd.limit = Some(n + from);
                }
                break;
            }
            _ => break,
        }
    }
    pd
}

/// The `@timestamp` range of a pushed filter (alone or in a conjunction).
fn timestamp_range(q: &Value) -> Option<Value> {
    if let Some(r) = q.pointer(&["range", "@timestamp"]) {
        return Some(crate::obj! { "range" => crate::obj! { "@timestamp" => r.clone() } });
    }
    let parts = q.pointer(&["bool", "filter"])?.as_array()?;
    let ranges: Vec<Value> = parts.iter().filter_map(timestamp_range).collect();
    match ranges.len() {
        0 => None,
        1 => ranges.into_iter().next(),
        _ => Some(crate::obj! { "bool" => crate::obj! { "filter" => Value::Arr(ranges) } }),
    }
}

/// A `where` condition as a query: `(query, exact)`, where `exact` means the query keeps
/// exactly the rows the condition does. `None` when no superset query exists. Text fields
/// stay in memory (their comparisons are on the whole value, the index has tokens).
fn to_dsl(e: &Expr, fields: &Fields) -> Option<(Value, bool)> {
    match &e.kind {
        E::Bin(Op::And, a, b) => match (to_dsl(a, fields), to_dsl(b, fields)) {
            (Some((x, ex)), Some((y, ey))) => Some((crate::obj! { "bool" => crate::obj! { "filter" => crate::arr![x, y] } }, ex && ey)),
            (Some((x, _)), None) | (None, Some((x, _))) => Some((x, false)),
            (None, None) => None,
        },
        E::Bin(Op::Or, a, b) => {
            let (x, ex) = to_dsl(a, fields)?;
            let (y, ey) = to_dsl(b, fields)?;
            Some((crate::obj! { "bool" => crate::obj! { "should" => crate::arr![x, y], "minimum_should_match" => 1 } }, ex && ey))
        }
        E::Not(x) => match to_dsl(x, fields)? {
            (q, true) => Some((crate::obj! { "bool" => crate::obj! { "must_not" => crate::arr![q] } }, true)),
            _ => None,
        },
        E::Bin(op, a, b) if op.is_cmp() && *op != Op::Ne => {
            let (f, lit, op) = match (&a.kind, &b.kind) {
                (E::Field(f), E::Lit(v, _)) => (f, v, *op),
                (E::Lit(v, _), E::Field(f)) => (f, v, flip(*op)),
                _ => return None,
            };
            let fd = fields.get(f)?;
            if fd.alias.is_some() {
                return None;
            }
            let lit = dsl_literal(fd, lit)?;
            Some((
                match op {
                    Op::Eq => crate::obj! { "term" => crate::obj! { f.clone() => crate::obj! { "value" => lit } } },
                    Op::Gt => crate::obj! { "range" => crate::obj! { f.clone() => crate::obj! { "gt" => lit } } },
                    Op::Ge => crate::obj! { "range" => crate::obj! { f.clone() => crate::obj! { "gte" => lit } } },
                    Op::Lt => crate::obj! { "range" => crate::obj! { f.clone() => crate::obj! { "lt" => lit } } },
                    _ => crate::obj! { "range" => crate::obj! { f.clone() => crate::obj! { "lte" => lit } } },
                },
                true,
            ))
        }
        E::IsNull(x, not) => {
            let E::Field(f) = &x.kind else { return None };
            fields.get(f).filter(|fd| fd.alias.is_none() && !matches!(fd.ty, Ty::Struct | Ty::Array))?;
            let exists = crate::obj! { "exists" => crate::obj! { "field" => f.clone() } };
            Some((if *not { exists } else { crate::obj! { "bool" => crate::obj! { "must_not" => crate::arr![exists] } } }, true))
        }
        _ => None,
    }
}

fn flip(op: Op) -> Op {
    match op {
        Op::Lt => Op::Gt,
        Op::Le => Op::Ge,
        Op::Gt => Op::Lt,
        Op::Ge => Op::Le,
        o => o,
    }
}

/// A literal as the query compares it with a field of this mapping (`None`: compare in memory).
fn dsl_literal(fd: &Field, v: &Val) -> Option<Value> {
    match (fd.ty, v) {
        (Ty::Byte | Ty::Short | Ty::Int | Ty::Long, Val::Int(i)) => Some(Value::from(*i)),
        (Ty::Float | Ty::Double, Val::Int(i)) => Some(Value::from(*i)),
        (Ty::Float | Ty::Double, Val::Float(f)) => Some(Value::from(*f)),
        (Ty::Bool, Val::Bool(b)) => Some(Value::Bool(*b)),
        (Ty::Str, Val::Str(s)) if fd.mtype == "keyword" => Some(Value::Str(s.clone())),
        (Ty::Timestamp, Val::Str(s)) => {
            let t = value::parse_ts(s)?;
            Some(Value::Str(t.format("%Y-%m-%dT%H:%M:%S%.9fZ").to_string()))
        }
        _ => None,
    }
}

/// Index pruning (`plugins.query.pruning.enabled`): with a range on `@timestamp`, only the
/// indices whose `@timestamp` values can fall in it are read (the `can_match` check of
/// `_field_caps`' `index_filter`). Aliases (their filters) and data streams aren't pruned.
/// `None`: nothing can match.
async fn prune(cluster: &Cluster, names: &str, fields: &Fields, pd: &Pushdown) -> EsResult<Option<String>> {
    let st = cluster.state();
    let enabled = st.metadata.cluster_setting("plugins.query.pruning.enabled").is_none_or(|v| v != "false");
    let Some(range) = pd.timestamp_range.as_ref().filter(|_| enabled) else { return Ok(Some(names.to_string())) };
    let patterns: Vec<&str> = names.split(',').collect();
    let names_alias = st.metadata.indices.values().flat_map(|m| m.aliases.keys()).any(|a| patterns.iter().any(|p| wildcard(p, a)));
    let names_stream = st.metadata.data_streams.keys().any(|d| patterns.iter().any(|p| wildcard(p, d)));
    if names_alias || names_stream {
        return Ok(Some(names.to_string()));
    }
    let caps = internal::call(cluster, "POST", &format!("/{}/_field_caps?fields=@timestamp&ignore_unavailable=true", encode(names)), Some(&crate::obj! { "index_filter" => range.clone() })).await?;
    let kept: Vec<String> = caps.get("indices").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let kept: Vec<String> = fields.indices.iter().filter(|i| kept.contains(i)).cloned().collect();
    Ok(if kept.is_empty() { None } else { Some(kept.join(",")) })
}

/// Reads every document matching `query` from `target`, in index order: one search when the
/// pipeline needs at most `limit` rows and that fits every index's `max_result_window`, else
/// pages over a point in time (as the plugin does).
async fn scan(cluster: &Cluster, target: &str, indices: &[String], query: &Value, limit: Option<usize>, mut f: impl FnMut(&Value)) -> EsResult<()> {
    let st = cluster.state();
    let window = indices.iter().filter_map(|i| st.metadata.indices.get(i)).filter_map(|m| m.settings.get("index.max_result_window").and_then(|v| v.parse::<usize>().ok())).fold(10_000usize, usize::min).max(1);
    drop(st);
    if let Some(n) = limit.filter(|n| *n <= window) {
        let body = crate::obj! { "size" => n as i64, "query" => query.clone(), "sort" => crate::arr!["_doc"], "track_total_hits" => false };
        let resp = internal::call(cluster, "POST", &format!("/{}/_search?ignore_unavailable=true", encode(target)), Some(&body)).await?;
        for h in resp.pointer(&["hits", "hits"]).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
            f(h);
        }
        return Ok(());
    }
    let pit = internal::call(cluster, "POST", &format!("/{}/_search/point_in_time?keep_alive=1m", encode(target)), None).await?;
    let Some(id) = pit.get("pit_id").and_then(Value::as_str).map(str::to_string) else { return Ok(()) };
    let page = window.min(10_000);
    let mut remaining = limit.unwrap_or(usize::MAX);
    let mut after: Option<Value> = None;
    let result = async {
        while remaining > 0 {
            let mut body = crate::obj! {
                "size" => page.min(remaining) as i64,
                "query" => query.clone(),
                "pit" => crate::obj! { "id" => id.clone(), "keep_alive" => "1m" },
                "sort" => crate::arr![crate::obj! { "_shard_doc" => "asc" }],
                "track_total_hits" => false
            };
            if let (Some(a), Value::Obj(m)) = (&after, &mut body) {
                m.insert("search_after".into(), a.clone());
            }
            let resp = internal::call(cluster, "POST", "/_search", Some(&body)).await?;
            let hits = resp.pointer(&["hits", "hits"]).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
            for h in hits {
                f(h);
            }
            remaining = remaining.saturating_sub(hits.len());
            match hits.last().and_then(|h| h.get("sort")) {
                Some(s) if hits.len() == page.min(remaining + hits.len()) => after = Some(s.clone()),
                _ => break,
            }
        }
        Ok::<(), EsError>(())
    }
    .await;
    let _ = internal::call(cluster, "DELETE", "/_search/point_in_time", Some(&crate::obj! { "pit_id" => crate::arr![id] })).await;
    result
}

/// The `search` command's terms as a query: comparisons and free text as `query_string`
/// syntax, time modifiers as ranges on `@timestamp`.
fn search_query(s: &Search) -> EsResult<Value> {
    Ok(match s {
        Search::And(a, b) => crate::obj! { "bool" => crate::obj! { "filter" => crate::arr![search_query(a)?, search_query(b)?] } },
        Search::Or(a, b) => crate::obj! { "bool" => crate::obj! { "should" => crate::arr![search_query(a)?, search_query(b)?], "minimum_should_match" => 1 } },
        Search::Not(a) => crate::obj! { "bool" => crate::obj! { "must_not" => crate::arr![search_query(a)?] } },
        Search::Earliest(t) => crate::obj! { "range" => crate::obj! { "@timestamp" => crate::obj! { "gte" => time_modifier(t) } } },
        Search::Latest(t) => crate::obj! { "range" => crate::obj! { "@timestamp" => crate::obj! { "lte" => time_modifier(t) } } },
        Search::Text(t) => qs(&escape_qs(t)),
        Search::Cmp(f, op, v) => {
            let lit = escape_qs(&v.to_text(Ty::Str).unwrap_or_default());
            let f = escape_field(f);
            qs(&match op {
                Op::Eq => format!("{f}:{lit}"),
                Op::Ne => format!("( _exists_:{f} AND NOT {f}:{lit} )"),
                Op::Lt => format!("{f}:<{lit}"),
                Op::Le => format!("{f}:<={lit}"),
                Op::Gt => format!("{f}:>{lit}"),
                _ => format!("{f}:>={lit}"),
            })
        }
        Search::In(f, vs) => {
            let f = escape_field(f);
            let list: Vec<String> = vs.iter().map(|v| escape_qs(&v.to_text(Ty::Str).unwrap_or_default())).collect();
            qs(&format!("{f}:( {} )", list.join(" OR ")))
        }
    })
}

fn qs(q: &str) -> Value {
    crate::obj! { "query_string" => crate::obj! { "query" => q } }
}

fn escape_field(f: &str) -> String {
    f.chars().flat_map(|c| if "+-=&|><!(){}[]^\"~?:\\/ ".contains(c) { vec!['\\', c] } else { vec![c] }).collect()
}

/// Escapes query-string syntax except `*` and `?` wildcards.
fn escape_qs(s: &str) -> String {
    if s.contains(' ') && !s.starts_with('"') {
        return format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    }
    s.chars().flat_map(|c| if "+-=&|><!(){}[]^\"~:\\/".contains(c) { vec!['\\', c] } else { vec![c] }).collect()
}

/// A time modifier as a date-math expression or absolute time.
fn time_modifier(t: &str) -> String {
    let t = t.trim();
    if t.eq_ignore_ascii_case("now") {
        return "now".into();
    }
    // An absolute time keeps its precision: `latest` (`lte`) rounds a missing fraction up.
    if value::parse_ts(t).is_some() {
        return t.replacen(' ', "T", 1);
    }
    if t.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return ((t.parse::<f64>().unwrap_or(0.0)) * 1000.0).to_string();
    }
    let mut out = String::from("now");
    let (shift, snap) = t.split_once('@').map_or((t, None), |(a, b)| (a, Some(b)));
    if !shift.is_empty() {
        let (sign, rest) = shift.split_at(1);
        let split = rest.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(rest.len());
        out.push_str(sign);
        out.push_str(&rest[..split]);
        out.push_str(math_unit(&rest[split..]));
    }
    if let Some(s) = snap {
        out.push('/');
        out.push_str(math_unit(s));
    }
    out
}

fn math_unit(u: &str) -> &'static str {
    match u.to_ascii_lowercase().as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => "s",
        "m" | "min" | "mins" | "minute" | "minutes" => "m",
        "h" | "hr" | "hrs" | "hour" | "hours" => "h",
        "d" | "day" | "days" => "d",
        "w" | "week" | "weeks" => "w",
        "mon" | "month" | "months" => "M",
        "y" | "yr" | "yrs" | "year" | "years" => "y",
        _ => "s",
    }
}

// ------------------------------------------------------------------------------ expressions

/// A compiled expression.
#[derive(Clone, Debug)]
pub enum C {
    Lit(Val),
    Col(usize),
    /// Inside a struct column: the column, the sub-path and the path's mapping.
    Path(usize, String, Option<Field>),
    Neg(Box<C>),
    Not(Box<C>),
    Arith(Op, Box<C>, Box<C>, Ty),
    Cmp(Op, Box<C>, Box<C>),
    And(Box<C>, Box<C>),
    Or(Box<C>, Box<C>),
    Xor(Box<C>, Box<C>),
    In(Box<C>, Vec<C>, bool),
    IsNull(Box<C>, bool),
    Call(&'static func::Def, Vec<C>, Vec<Ty>),
    Case(Vec<(C, C)>, Option<Box<C>>),
    Cast(Box<C>, Ty, Ty),
    Span(Box<C>, f64, String, Ty),
}

/// A compiled expression and its type.
#[derive(Clone, Debug)]
pub struct Typed {
    pub c: C,
    pub ty: Ty,
}

/// Name resolution over a table's columns.
pub struct Scope<'a> {
    pub cols: &'a [Col],
    pub fields: &'a Fields,
    pub s: &'a Settings,
}

impl Scope<'_> {
    pub fn find(&self, name: &str) -> Option<usize> {
        self.cols.iter().rposition(|c| c.name == name)
    }

    /// A field reference: a column, or a path inside a struct column.
    pub fn field(&self, path: &str, pos: usize, src_text: &str) -> EsResult<Typed> {
        if let Some(i) = self.find(path) {
            return Ok(Typed { c: C::Col(i), ty: self.cols[i].ty });
        }
        // An alias inside an object reads its target.
        if let Some(target) = self.fields.get(path).and_then(|f| f.alias.as_deref())
            && target != path
        {
            return self.field(target, pos, src_text);
        }
        // The longest struct column prefix.
        let mut best: Option<(usize, usize)> = None;
        for (i, c) in self.cols.iter().enumerate() {
            if matches!(c.ty, Ty::Struct | Ty::Array) && path.len() > c.name.len() && path.starts_with(&c.name) && path.as_bytes()[c.name.len()] == b'.' && best.is_none_or(|(_, l)| c.name.len() > l) {
                best = Some((i, c.name.len()));
            }
        }
        if let Some((i, l)) = best {
            let f = self.fields.get(path).cloned();
            let ty = f.as_ref().map_or(Ty::Undefined, |f| f.ty);
            return Ok(Typed { c: C::Path(i, path[l + 1..].to_string(), f), ty });
        }
        Err(self.not_found(path, pos, src_text))
    }

    /// The plugin's field-not-found error for a name written at `pos` of `src_text`: with the
    /// position and the closest available name.
    pub fn not_found(&self, path: &str, pos: usize, src_text: &str) -> EsError {
        self.not_found_at(path, Some((pos, src_text)))
    }

    /// For a field the query doesn't write (`timechart`'s default `@timestamp`): no position
    /// and no suggestion.
    pub fn not_found_at(&self, path: &str, at: Option<(usize, &str)>) -> EsError {
        let mut available: Vec<Value> = self.cols.iter().filter(|c| !c.hidden).map(|c| Value::Str(c.name.clone())).collect();
        if available.len() == self.fields.top.len() {
            available = self.fields.available();
        } else {
            available.extend(META.iter().map(|(m, _)| Value::Str((*m).into())));
        }
        let names: Vec<&str> = available.iter().filter_map(Value::as_str).collect();
        let suggestion = at.and_then(|_| names.iter().map(|n| (levenshtein(path, n), *n)).min_by_key(|(d, _)| *d).map(|(_, n)| n.to_string()));
        let mut ctx = vec![("requested_field", Value::Str(path.into())), ("available_fields", Value::Arr(available.clone()))];
        if let Some((pos, src_text)) = at {
            let (line, column) = line_col(src_text, pos);
            ctx.push(("query_pos", crate::obj! { "column" => column as i64, "line" => line as i64 }));
        }
        let mut e = super::analysis_error("IllegalArgumentException", "FIELD_NOT_FOUND", format!("Field [{path}] not found."), ctx);
        if let Some(s) = suggestion
            && let Some(Value::Obj(mut m)) = e.hidden(super::PPL_ERROR).cloned()
        {
            m.insert("suggestion".into(), Value::Str(format!("Did you mean: {s}")));
            e = e.with_hidden(super::PPL_ERROR, Value::Obj(m));
        }
        e
    }
}

fn line_col(src: &str, pos: usize) -> (usize, usize) {
    let before = &src[..pos.min(src.len())];
    let line = before.matches('\n').count() + 1;
    let col = before.rfind('\n').map_or(before.chars().count(), |n| before[n + 1..].chars().count());
    (line, col)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + usize::from(ca != cb));
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

pub fn compile(e: &Expr, sc: &Scope<'_>) -> EsResult<Typed> {
    Ok(match &e.kind {
        E::Lit(v, t) => Typed { c: C::Lit(v.clone()), ty: *t },
        E::Field(p) => sc.field(p, e.pos, &sc.s.src)?,
        E::Neg(x) => {
            let x = compile(x, sc)?;
            Typed { ty: x.ty, c: C::Neg(Box::new(x.c)) }
        }
        E::Not(x) => {
            let x = compile(x, sc)?;
            Typed { c: C::Not(Box::new(x.c)), ty: Ty::Bool }
        }
        E::Bin(op, a, b) => {
            let (a, b) = (compile(a, sc)?, compile(b, sc)?);
            match op {
                Op::And => Typed { c: C::And(Box::new(a.c), Box::new(b.c)), ty: Ty::Bool },
                Op::Or => Typed { c: C::Or(Box::new(a.c), Box::new(b.c)), ty: Ty::Bool },
                Op::Xor => Typed { c: C::Xor(Box::new(a.c), Box::new(b.c)), ty: Ty::Bool },
                op if op.is_cmp() => Typed { c: C::Cmp(*op, Box::new(a.c), Box::new(b.c)), ty: Ty::Bool },
                op => {
                    let ty = arith_type(*op, a.ty, b.ty, sc.s.legacy);
                    Typed { c: C::Arith(*op, Box::new(a.c), Box::new(b.c), ty), ty }
                }
            }
        }
        E::In(x, list, not) => {
            let x = compile(x, sc)?;
            let list: Vec<Typed> = list.iter().map(|l| compile(l, sc)).collect::<EsResult<_>>()?;
            let compatible = |t: Ty| t == Ty::Undefined || x.ty == Ty::Undefined || t == x.ty || (t.is_numeric() && x.ty.is_numeric()) || (t == Ty::Str && matches!(x.ty, Ty::Str | Ty::Ip | Ty::Timestamp | Ty::Date | Ty::Time));
            if !list.iter().all(|l| compatible(l.ty)) {
                let tys: Vec<&str> = list.iter().map(|l| l.ty.upper()).collect();
                return Err(super::semantic(format!("In expression types are incompatible: fields type {}, values type [{}]", x.ty.upper(), tys.join(", "))));
            }
            Typed { c: C::In(Box::new(x.c), list.into_iter().map(|l| l.c).collect(), *not), ty: Ty::Bool }
        }
        E::Between(x, lo, hi, not) => {
            let (x, lo, hi) = (compile(x, sc)?, compile(lo, sc)?, compile(hi, sc)?);
            let c = C::And(Box::new(C::Cmp(Op::Ge, Box::new(x.c.clone()), Box::new(lo.c))), Box::new(C::Cmp(Op::Le, Box::new(x.c), Box::new(hi.c))));
            Typed { c: if *not { C::Not(Box::new(c)) } else { c }, ty: Ty::Bool }
        }
        E::IsNull(x, not) => {
            let x = compile(x, sc)?;
            Typed { c: C::IsNull(Box::new(x.c), *not), ty: Ty::Bool }
        }
        E::Case(arms, other) => {
            let mut out = Vec::new();
            let mut ty = Ty::Undefined;
            for (c, v) in arms {
                let (c, v) = (compile(c, sc)?, compile(v, sc)?);
                ty = merge_type(ty, v.ty);
                out.push((c.c, v.c));
            }
            let other = match other {
                Some(o) => {
                    let o = compile(o, sc)?;
                    ty = merge_type(ty, o.ty);
                    Some(Box::new(o.c))
                }
                None => None,
            };
            Typed { c: C::Case(out, other), ty }
        }
        E::Cast(x, t) => {
            let x = compile(x, sc)?;
            if matches!(x.ty, Ty::Struct | Ty::Array) {
                let what = if x.ty == Ty::Struct { "an object" } else { "an array" };
                let to = if t == "INT" { "INTEGER" } else { t.as_str() };
                return Err(super::analysis_error("IllegalArgumentException", "UNKNOWN", format!("Cannot cast {what} to {to}"), Vec::new()));
            }
            let target = match t.as_str() {
                "INT" | "INTEGER" => Ty::Int,
                "LONG" | "BIGINT" => Ty::Long,
                "DOUBLE" => Ty::Double,
                "FLOAT" => Ty::Float,
                "STRING" | "VARCHAR" => Ty::Str,
                "BOOLEAN" => Ty::Bool,
                "TIMESTAMP" => Ty::Timestamp,
                "DATE" => Ty::Date,
                "TIME" => Ty::Time,
                "IP" => Ty::Ip,
                "SHORT" | "SMALLINT" => Ty::Short,
                "BYTE" | "TINYINT" => Ty::Byte,
                _ => Ty::Str,
            };
            Typed { c: C::Cast(Box::new(x.c), x.ty, target), ty: target }
        }
        E::Interval(x, unit) => {
            let x = compile(x, sc)?;
            let def = func::lookup("__interval").expect("interval");
            Typed { c: C::Call(def, vec![x.c, C::Lit(Val::Str(unit.clone()))], vec![x.ty, Ty::Str]), ty: Ty::Long }
        }
        E::Span(f, len, unit) => {
            let f = compile(f, sc)?;
            let ty = if unit.is_empty() || matches!(unit.as_str(), "d") && f.ty.is_numeric() { if f.ty.is_numeric() && *len != len.trunc() { Ty::Double } else { f.ty } } else { Ty::Timestamp };
            Typed { c: C::Span(Box::new(f.c), *len, unit.clone(), f.ty), ty }
        }
        E::Named(_, x) => compile(x, sc)?,
        E::Call(name, args) => {
            let compiled: Vec<Typed> = args.iter().map(|a| compile(a, sc)).collect::<EsResult<_>>()?;
            let tys: Vec<Ty> = compiled.iter().map(|a| a.ty).collect();
            let def = func::lookup(name).ok_or_else(|| super::error(400, "SemanticCheckException", format!("Function [{name}] is not supported"), Vec::new()))?;
            let ty = (def.ret)(&tys).map_err(super::semantic)?;
            Typed { c: C::Call(def, compiled.into_iter().map(|a| a.c).collect(), tys), ty }
        }
    })
}

fn merge_type(a: Ty, b: Ty) -> Ty {
    match (a, b) {
        (Ty::Undefined, t) | (t, Ty::Undefined) => t,
        (a, b) if a == b => a,
        (a, b) if a.is_numeric() && b.is_numeric() => Ty::widen(a, b),
        _ => Ty::Str,
    }
}

/// The result type of an arithmetic operator: `+ - *` on integers widen one step (so they
/// can't overflow the operands' type), `/` and `%` keep the wider type (`/` of integers
/// truncates in the legacy syntax); strings read as doubles, `+` of strings concatenates.
pub fn arith_type(op: Op, a: Ty, b: Ty, legacy: bool) -> Ty {
    if op == Op::Add && a == Ty::Str && b == Ty::Str {
        return Ty::Str;
    }
    if a == Ty::Str || b == Ty::Str {
        return Ty::Double;
    }
    if matches!(a, Ty::Timestamp | Ty::Date) || matches!(b, Ty::Timestamp | Ty::Date) {
        return Ty::Timestamp;
    }
    let w = Ty::widen(a, b);
    match op {
        Op::Add | Op::Sub | Op::Mul => match w {
            Ty::Byte => Ty::Short,
            Ty::Short => Ty::Int,
            Ty::Int | Ty::Long => Ty::Long,
            Ty::Undefined => Ty::Long,
            other => other,
        },
        Op::Div if !legacy && w.is_integral() => Ty::Double,
        _ => if w == Ty::Undefined { Ty::Long } else { w },
    }
}

pub fn eval(c: &C, row: &[Val]) -> EsResult<Val> {
    Ok(match c {
        C::Lit(v) => v.clone(),
        C::Col(i) => row[*i].clone(),
        C::Path(i, p, f) => match &row[*i] {
            Val::Json(v) => match extract(v, p) {
                Some(x) => match f {
                    Some(f) => convert(x, f.ty, f.format.as_deref(), p, &Fields::default()),
                    None => convert(x, Ty::Undefined, None, p, &Fields::default()),
                },
                None => {
                    // An array of objects: the path from each element.
                    if let Value::Arr(a) = v {
                        let mut vals: Vec<Val> = a.iter().filter_map(|o| extract(o, p)).map(|x| convert(x, f.as_ref().map_or(Ty::Undefined, |f| f.ty), None, p, &Fields::default())).collect();
                        match vals.len() {
                            0 => Val::Null,
                            1 => vals.pop().expect("one"),
                            _ => Val::Arr(vals),
                        }
                    } else {
                        Val::Null
                    }
                }
            },
            _ => Val::Null,
        },
        C::Neg(x) => match eval(x, row)? {
            Val::Int(i) => Val::Int(-i),
            Val::Float(f) => Val::Float(-f),
            _ => Val::Null,
        },
        C::Not(x) => match eval(x, row)? {
            Val::Bool(b) => Val::Bool(!b),
            _ => Val::Null,
        },
        C::And(a, b) => match (eval(a, row)?.as_bool(), eval(b, row)?.as_bool()) {
            (Some(false), _) | (_, Some(false)) => Val::Bool(false),
            (Some(true), Some(true)) => Val::Bool(true),
            _ => Val::Null,
        },
        C::Or(a, b) => match (eval(a, row)?.as_bool(), eval(b, row)?.as_bool()) {
            (Some(true), _) | (_, Some(true)) => Val::Bool(true),
            (Some(false), Some(false)) => Val::Bool(false),
            _ => Val::Null,
        },
        C::Xor(a, b) => match (eval(a, row)?.as_bool(), eval(b, row)?.as_bool()) {
            (Some(x), Some(y)) => Val::Bool(x != y),
            _ => Val::Null,
        },
        C::Cmp(op, a, b) => compare(*op, &eval(a, row)?, &eval(b, row)?),
        C::Arith(op, a, b, ty) => arith(*op, eval(a, row)?, eval(b, row)?, *ty)?,
        C::In(x, list, not) => {
            let v = eval(x, row)?;
            if v.is_null() {
                return Ok(Val::Null);
            }
            let mut found = false;
            for l in list {
                if compare(Op::Eq, &v, &eval(l, row)?).as_bool() == Some(true) {
                    found = true;
                    break;
                }
            }
            Val::Bool(found != *not)
        }
        C::IsNull(x, not) => Val::Bool(eval(x, row)?.is_null() != *not),
        C::Call(def, args, tys) => {
            let vals: Vec<Val> = args.iter().map(|a| eval(a, row)).collect::<EsResult<_>>()?;
            (def.eval)(&vals, tys)?
        }
        C::Case(arms, other) => {
            for (c, v) in arms {
                if eval(c, row)?.as_bool() == Some(true) {
                    return eval(v, row);
                }
            }
            match other {
                Some(o) => eval(o, row)?,
                None => Val::Null,
            }
        }
        C::Cast(x, from, to) => func::cast(eval(x, row)?, *from, *to),
        C::Span(x, len, unit, ty) => span(eval(x, row)?, *len, unit, *ty),
    })
}

/// Comparison with PPL's coercions: numbers numerically, a string against a number as a
/// double (null when it isn't one), timestamps against timestamp texts.
pub fn compare(op: Op, a: &Val, b: &Val) -> Val {
    use std::cmp::Ordering::*;
    if a.is_null() || b.is_null() {
        return Val::Null;
    }
    let ord = match (a, b) {
        (Val::Str(s), Val::Int(_) | Val::Float(_)) => match s.trim().parse::<f64>() {
            Ok(x) => x.total_cmp(&b.as_f64().unwrap_or(0.0)),
            Err(_) => return Val::Null,
        },
        (Val::Int(_) | Val::Float(_), Val::Str(s)) => match s.trim().parse::<f64>() {
            Ok(y) => a.as_f64().unwrap_or(0.0).total_cmp(&y),
            Err(_) => return Val::Null,
        },
        (Val::Str(s), Val::Bool(x)) | (Val::Bool(x), Val::Str(s)) => match s.parse::<bool>() {
            Ok(y) if op == Op::Eq || op == Op::Ne => {
                if x == &y {
                    Equal
                } else {
                    Less
                }
            }
            _ => return Val::Null,
        },
        (Val::Arr(x), _) if !matches!(b, Val::Arr(_)) => {
            // A multi-value matches when any value does (term query semantics).
            let any = x.iter().any(|v| compare(op, v, b).as_bool() == Some(true));
            return Val::Bool(any);
        }
        _ => value::cmp(a, b),
    };
    Val::Bool(match op {
        Op::Eq => ord == Equal,
        Op::Ne => ord != Equal,
        Op::Lt => ord == Less,
        Op::Le => ord != Greater,
        Op::Gt => ord == Greater,
        _ => ord != Less,
    })
}

pub fn arith(op: Op, a: Val, b: Val, ty: Ty) -> EsResult<Val> {
    if a.is_null() || b.is_null() {
        return Ok(Val::Null);
    }
    match (&a, &b) {
        (Val::Str(x), Val::Str(y)) if op == Op::Add => return Ok(Val::Str(format!("{x}{y}"))),
        (Val::Int(x), Val::Int(y)) if !(op == Op::Div && ty == Ty::Double) => {
            let (x, y) = (*x, *y);
            let r = match op {
                Op::Add => x.checked_add(y),
                Op::Sub => x.checked_sub(y),
                Op::Mul => x.checked_mul(y),
                Op::Div => {
                    if y == 0 {
                        return Ok(Val::Null);
                    }
                    Some(x.wrapping_div(y))
                }
                _ => {
                    if y == 0 {
                        return Ok(Val::Null);
                    }
                    Some(x.wrapping_rem(y))
                }
            };
            return match r {
                Some(v) => Ok(Val::Int(v)),
                None => Err(super::invalid_query("ArithmeticException", "long overflow")),
            };
        }
        (Val::Ts(t), _) if matches!(op, Op::Add | Op::Sub) => {
            if let Some(ms) = b.as_i64() {
                return Ok(value::ts_from_millis(value::ts_millis(t) + if op == Op::Add { ms } else { -ms }).map_or(Val::Null, Val::Ts));
            }
        }
        _ => {}
    }
    let (Some(x), Some(y)) = (num(&a), num(&b)) else { return Ok(Val::Null) };
    let r = match op {
        Op::Add => x + y,
        Op::Sub => x - y,
        Op::Mul => x * y,
        Op::Div => {
            if y == 0.0 {
                return Ok(Val::Null);
            }
            x / y
        }
        _ => {
            if y == 0.0 {
                return Ok(Val::Null);
            }
            x % y
        }
    };
    Ok(if r.is_finite() { Val::Float(r) } else { Val::Null })
}

fn num(v: &Val) -> Option<f64> {
    match v {
        Val::Int(i) => Some(*i as f64),
        Val::Float(f) => Some(*f),
        Val::Str(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `span(x, len[unit])`: a numeric bucket or a time bucket.
fn span(v: Val, len: f64, unit: &str, ty: Ty) -> Val {
    match v {
        Val::Null => Val::Null,
        Val::Ts(t) => time_bucket(t, len, unit).map_or(Val::Null, Val::Ts),
        Val::Date(d) => time_bucket(d.and_hms_opt(0, 0, 0).expect("midnight"), len, unit).map_or(Val::Null, Val::Ts),
        v => {
            let Some(x) = v.as_f64() else { return Val::Null };
            if len <= 0.0 {
                return Val::Null;
            }
            let b = (x / len).floor() * len;
            if ty.is_integral() && len.fract() == 0.0 { Val::Int(b as i64) } else { Val::Float(b) }
        }
    }
}

/// Rounds a timestamp down to a `len unit` bucket (calendar units for months and up).
pub fn time_bucket(t: NaiveDateTime, len: f64, unit: &str) -> Option<NaiveDateTime> {
    let ms = value::ts_millis(&t);
    let n = len.max(1.0) as i64;
    // `M` is months, `m` minutes.
    let u = if unit == "M" { "mon".to_string() } else { unit.to_ascii_lowercase() };
    let fixed = match u.as_str() {
        "ms" | "millisecond" | "milliseconds" => Some(1),
        "s" | "sec" | "secs" | "second" | "seconds" => Some(1000),
        "m" | "min" | "mins" | "minute" | "minutes" => Some(60_000),
        "h" | "hr" | "hrs" | "hour" | "hours" => Some(3_600_000),
        "d" | "day" | "days" => Some(86_400_000),
        "w" | "week" | "weeks" => Some(7 * 86_400_000),
        "us" => Some(1),
        _ => None,
    };
    if let Some(unit_ms) = fixed {
        let size = unit_ms * n;
        return value::ts_from_millis(ms.div_euclid(size) * size);
    }
    let months = match u.as_str() {
        "mon" | "month" | "months" => n,
        "q" | "qtr" | "qtrs" | "quarter" | "quarters" => 3 * n,
        "y" | "yr" | "yrs" | "year" | "years" => 12 * n,
        _ => return value::ts_from_millis(ms),
    };
    let total = (t.year() as i64 - 1970) * 12 + t.month0() as i64;
    let b = total.div_euclid(months) * months;
    let (y, m) = (1970 + b.div_euclid(12), b.rem_euclid(12) as u32 + 1);
    chrono::NaiveDate::from_ymd_opt(y as i32, m, 1).and_then(|d| d.and_hms_opt(0, 0, 0))
}

// ------------------------------------------------------------------------------ commands

fn compile_in(e: &Expr, t: &Table, s: &Settings) -> EsResult<Typed> {
    compile(e, &Scope { cols: &t.cols, fields: &t.fields, s })
}

fn command<'a>(cluster: &'a Cluster, t: Table, c: &'a Cmd, s: &'a Settings) -> std::pin::Pin<Box<dyn std::future::Future<Output = EsResult<Table>> + Send + 'a>> {
    Box::pin(async move {
        Ok(match c {
            Cmd::Where(e) => {
                let p = compile_in(e, &t, s)?;
                if !matches!(p.ty, Ty::Bool | Ty::Undefined) {
                    return Err(super::semantic(format!("WHERE clause expects a boolean expression, but got {}", p.ty.upper())));
                }
                let mut t = t;
                let mut err = None;
                t.rows.retain(|r| match eval(&p.c, r) {
                    Ok(v) => v.as_bool() == Some(true),
                    Err(e) => {
                        err.get_or_insert(e);
                        false
                    }
                });
                if let Some(e) = err {
                    return Err(e);
                }
                t
            }
            Cmd::Fields { exclude, fields } => project(t, fields, *exclude, s)?,
            Cmd::Eval(assigns) => {
                let mut t = t;
                for (name, e) in assigns {
                    let x = compile_in(e, &t, s)?;
                    let vals: Vec<Val> = t.rows.iter().map(|r| eval(&x.c, r)).collect::<EsResult<_>>()?;
                    if let Some(i) = t.cols.iter().position(|c| &c.name == name) {
                        t.cols.remove(i);
                        for r in &mut t.rows {
                            r.remove(i);
                        }
                    }
                    t.cols.push(Col { name: name.clone(), ty: x.ty, hidden: false });
                    for (r, v) in t.rows.iter_mut().zip(vals) {
                        r.push(v);
                    }
                }
                t
            }
            Cmd::Stats { aggs, by, bucket_nullable } => stats(t, aggs, by, *bucket_nullable, s, false)?,
            Cmd::EventStats { aggs, by, bucket_nullable } => eventstats(t, aggs, by, *bucket_nullable, s)?,
            Cmd::StreamStats { aggs, by, window, current, global } => streamstats(t, aggs, by, *window, *current, *global, s)?,
            Cmd::Sort { keys, count } => {
                let mut t = t;
                let compiled: Vec<(Typed, bool, Option<String>)> = keys.iter().map(|k| Ok((compile_in(&k.expr, &t, s)?, k.desc, k.cast.clone()))).collect::<EsResult<_>>()?;
                let mut keyed: Vec<(Vec<Val>, Vec<Val>)> = Vec::with_capacity(t.rows.len());
                for r in std::mem::take(&mut t.rows) {
                    let k = compiled.iter().map(|(c, _, cast)| eval(&c.c, &r).map(|v| sort_cast(v, cast.as_deref()))).collect::<EsResult<Vec<_>>>()?;
                    keyed.push((k, r));
                }
                keyed.sort_by(|(a, _), (b, _)| {
                    for (i, (_, desc, _)) in compiled.iter().enumerate() {
                        let o = match (a[i].is_null(), b[i].is_null()) {
                            (true, true) => std::cmp::Ordering::Equal,
                            // Nulls first ascending, last descending.
                            (true, false) => std::cmp::Ordering::Less,
                            (false, true) => std::cmp::Ordering::Greater,
                            _ => value::cmp(&a[i], &b[i]),
                        };
                        let o = if *desc { o.reverse() } else { o };
                        if o != std::cmp::Ordering::Equal {
                            return o;
                        }
                    }
                    std::cmp::Ordering::Equal
                });
                t.rows = keyed.into_iter().map(|(_, r)| r).collect();
                if let Some(n) = count.filter(|n| *n > 0) {
                    t.rows.truncate(n);
                }
                t
            }
            Cmd::Head { n, from } => {
                let mut t = t;
                let rows = std::mem::take(&mut t.rows);
                t.rows = rows.into_iter().skip(*from).take(*n).collect();
                t
            }
            Cmd::Rename(pairs) => {
                let mut t = t;
                for (from, to) in pairs {
                    rename(&mut t, from, to)?;
                }
                t
            }
            Cmd::Dedup { n, fields, keepempty, consecutive } => dedup(t, fields, *n, *keepempty, *consecutive, s)?,
            Cmd::Top { rare, n, fields, by, countfield, showcount } => top(t, *rare, *n, fields, by, countfield, *showcount, s)?,
            Cmd::Reverse => {
                let mut t = t;
                t.rows.reverse();
                t
            }
            Cmd::Append(q) => {
                let other = run_query(cluster, q, s).await?;
                append(t, other)
            }
            Cmd::AppendPipe(cmds) => {
                let mut sub = t.clone();
                for c in cmds {
                    sub = command(cluster, sub, c, s).await?;
                }
                append(t, sub)
            }
            other => return super::exec_more::command(cluster, t, other, s).await,
        })
    })
}

/// `CAST`s of `sort str(x)` / `num(x)`.
fn sort_cast(v: Val, cast: Option<&str>) -> Val {
    match cast {
        Some("str") => v.to_text(Ty::Undefined).map_or(Val::Null, Val::Str),
        Some("num") => v.as_f64().map_or(Val::Null, Val::Float),
        _ => v,
    }
}

/// `fields [+|-] list`, wildcards matching every known path.
fn project(t: Table, list: &[(String, usize)], exclude: bool, s: &Settings) -> EsResult<Table> {
    let mut t = t;
    if exclude {
        let mut drop = vec![false; t.cols.len()];
        for (name, pos) in list {
            let mut any = false;
            for (i, c) in t.cols.iter().enumerate() {
                if wildcard(name, &c.name) {
                    drop[i] = true;
                    any = true;
                }
            }
            if !any && !name.contains('*') {
                return Err(Scope { cols: &t.cols, fields: &t.fields, s }.not_found(name, *pos, &s.src));
            }
        }
        let keep: Vec<usize> = (0..t.cols.len()).filter(|i| !drop[*i]).collect();
        return Ok(pick(t, &keep, None));
    }
    let mut out_cols: Vec<Col> = Vec::new();
    let mut exprs: Vec<C> = Vec::new();
    let original_cols = t.cols.len();
    let source_like = t.cols.iter().filter(|c| !c.hidden).count() == t.fields.top.len() && t.cols.iter().filter(|c| !c.hidden).all(|c| t.fields.paths.contains_key(&c.name));
    for (name, pos) in list {
        if name.contains('*') {
            // Columns first, then (on a source table) the nested paths.
            let mut names: Vec<String> = Vec::new();
            if source_like {
                names.extend(t.fields.paths.keys().filter(|p| wildcard(name, p)).cloned());
            } else {
                names.extend(t.cols.iter().filter(|c| !c.hidden && wildcard(name, &c.name)).map(|c| c.name.clone()));
            }
            for n in names {
                if out_cols.iter().any(|c| c.name == n) {
                    continue;
                }
                let x = Scope { cols: &t.cols[..original_cols], fields: &t.fields, s }.field(&n, *pos, &s.src)?;
                out_cols.push(Col { name: n, ty: x.ty, hidden: false });
                exprs.push(x.c);
            }
            continue;
        }
        if out_cols.iter().any(|c| &c.name == name) {
            continue;
        }
        let sc = Scope { cols: &t.cols[..original_cols], fields: &t.fields, s };
        let x = sc.field(name, *pos, &s.src)?;
        out_cols.push(Col { name: name.clone(), ty: x.ty, hidden: false });
        exprs.push(x.c);
    }
    let rows = std::mem::take(&mut t.rows);
    t.rows = rows.iter().map(|r| exprs.iter().map(|c| eval(c, r)).collect::<EsResult<Vec<_>>>()).collect::<EsResult<_>>()?;
    t.cols = out_cols;
    Ok(t)
}

fn pick(mut t: Table, keep: &[usize], hidden: Option<bool>) -> Table {
    t.cols = keep.iter().map(|i| {
        let mut c = t.cols[*i].clone();
        if let Some(h) = hidden {
            c.hidden = h;
        }
        c
    }).collect();
    for r in &mut t.rows {
        let old = std::mem::take(r);
        *r = keep.iter().map(|i| old[*i].clone()).collect();
    }
    t
}

pub fn wildcard(pattern: &str, s: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == s;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut rest = s;
    for (i, p) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(p) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(p);
        } else {
            match rest.find(p) {
                Some(k) => rest = &rest[k + p.len()..],
                None => return false,
            }
        }
    }
    true
}

fn rename(t: &mut Table, from: &str, to: &str) -> EsResult<()> {
    if from.contains('*') {
        let prefix_len = from.find('*').unwrap_or(0);
        let suffix_len = from.len() - from.rfind('*').map_or(from.len(), |k| k + 1);
        for c in t.cols.iter_mut().filter(|c| !c.hidden) {
            if wildcard(from, &c.name) {
                let mid = &c.name[prefix_len..c.name.len() - suffix_len];
                c.name = to.replacen('*', mid, 1);
            }
        }
        return Ok(());
    }
    match t.cols.iter().position(|c| c.name == from) {
        Some(i) => {
            if let Some(j) = t.cols.iter().position(|c| c.name == to).filter(|j| *j != i) {
                t.cols[j].hidden = true;
                t.cols[j].name = format!("\u{0}{to}");
            }
            t.cols[i].name = to.to_string();
            Ok(())
        }
        None => {
            // A nested path: becomes its own column.
            let x = Scope { cols: &t.cols, fields: &t.fields, s: &Settings { size_limit: 0, legacy: true, now: NaiveDateTime::default(), src: Arc::from("") } }.field(from, 0, "")?;
            let vals: Vec<Val> = t.rows.iter().map(|r| eval(&x.c, r)).collect::<EsResult<_>>()?;
            t.cols.push(Col { name: to.to_string(), ty: x.ty, hidden: false });
            for (r, v) in t.rows.iter_mut().zip(vals) {
                r.push(v);
            }
            Ok(())
        }
    }
}

fn append(a: Table, b: Table) -> Table {
    let mut out = a;
    let mut map = Vec::new();
    for c in &b.cols {
        match out.cols.iter().position(|o| o.name == c.name) {
            Some(i) => {
                if out.cols[i].ty != c.ty {
                    out.cols[i].ty = merge_type(out.cols[i].ty, c.ty);
                }
                map.push(i);
            }
            None => {
                out.cols.push(c.clone());
                for r in &mut out.rows {
                    r.push(Val::Null);
                }
                map.push(out.cols.len() - 1);
            }
        }
    }
    let width = out.cols.len();
    for r in b.rows {
        let mut row = vec![Val::Null; width];
        for (v, i) in r.into_iter().zip(&map) {
            row[*i] = v;
        }
        out.rows.push(row);
    }
    out
}

fn dedup(t: Table, fields: &[(String, usize)], n: usize, keepempty: bool, consecutive: bool, s: &Settings) -> EsResult<Table> {
    let mut t = t;
    let keys: Vec<Typed> = fields.iter().map(|(f, p)| Scope { cols: &t.cols, fields: &t.fields, s }.field(f, *p, &s.src)).collect::<EsResult<_>>()?;
    let mut seen: HashMap<Vec<Key>, usize> = HashMap::new();
    let mut last: Option<Vec<Key>> = None;
    let mut run = 0usize;
    let rows = std::mem::take(&mut t.rows);
    for r in rows {
        let k: Vec<Val> = keys.iter().map(|c| eval(&c.c, &r)).collect::<EsResult<_>>()?;
        if k.iter().any(Val::is_null) {
            if keepempty {
                t.rows.push(r);
            }
            continue;
        }
        let key: Vec<Key> = k.iter().map(Key::of).collect();
        if consecutive {
            if last.as_ref() == Some(&key) {
                run += 1;
            } else {
                run = 1;
                last = Some(key);
            }
            if run <= n {
                t.rows.push(r);
            }
            continue;
        }
        let c = seen.entry(key).or_insert(0);
        *c += 1;
        if *c <= n {
            t.rows.push(r);
        }
    }
    Ok(t)
}

/// One `top`/`rare` combination: its `BY` values, its field values and its count.
type Counted = (Vec<Val>, Vec<Val>, i64);

#[allow(clippy::too_many_arguments)]
fn top(t: Table, rare: bool, n: usize, fields: &[(String, usize)], by: &[(String, usize)], countfield: &str, showcount: bool, s: &Settings) -> EsResult<Table> {
    let sc = Scope { cols: &t.cols, fields: &t.fields, s };
    let byc: Vec<Typed> = by.iter().map(|(f, p)| sc.field(f, *p, &s.src)).collect::<EsResult<_>>()?;
    let fc: Vec<Typed> = fields.iter().map(|(f, p)| sc.field(f, *p, &s.src)).collect::<EsResult<_>>()?;
    // Count per (by, fields), keeping first-seen order for ties.
    let mut groups: indexmap::IndexMap<Vec<Key>, Counted> = indexmap::IndexMap::new();
    for r in &t.rows {
        let bv: Vec<Val> = byc.iter().map(|c| eval(&c.c, r)).collect::<EsResult<_>>()?;
        let fv: Vec<Val> = fc.iter().map(|c| eval(&c.c, r)).collect::<EsResult<_>>()?;
        if fv.iter().any(Val::is_null) {
            continue;
        }
        let key: Vec<Key> = bv.iter().chain(&fv).map(Key::of).collect();
        groups.entry(key).or_insert((bv, fv, 0)).2 += 1;
    }
    let mut parts: indexmap::IndexMap<Vec<Key>, Vec<Counted>> = indexmap::IndexMap::new();
    for (_, g) in groups {
        parts.entry(g.0.iter().map(Key::of).collect()).or_default().push(g);
    }
    let mut cols: Vec<Col> = by.iter().zip(&byc).map(|((f, _), c)| Col { name: f.clone(), ty: c.ty, hidden: false }).collect();
    cols.extend(fields.iter().zip(&fc).map(|((f, _), c)| Col { name: f.clone(), ty: c.ty, hidden: false }));
    if showcount {
        cols.push(Col { name: countfield.to_string(), ty: Ty::Long, hidden: false });
    }
    let mut rows = Vec::new();
    for (_, mut list) in parts {
        list.sort_by(|a, b| {
            let o = if rare { a.2.cmp(&b.2) } else { b.2.cmp(&a.2) };
            o.then_with(|| a.1.iter().zip(&b.1).map(|(x, y)| value::cmp(x, y)).find(|o| o.is_ne()).unwrap_or(std::cmp::Ordering::Equal))
        });
        for (bv, fv, c) in list.into_iter().take(if n == 0 { usize::MAX } else { n }) {
            let mut row = bv;
            row.extend(fv);
            if showcount {
                row.push(Val::Int(c));
            }
            rows.push(row);
        }
    }
    Ok(Table { cols, rows, fields: t.fields })
}

// ------------------------------------------------------------------------------ aggregation

/// One aggregation's state.
#[derive(Clone, Debug)]
pub enum Acc {
    Count(i64),
    Sum { int: i64, float: f64, any_float: bool, n: i64, overflow: bool },
    Avg { sum: f64, n: i64 },
    Min(Option<Val>),
    Max(Option<Val>),
    Distinct(std::collections::HashSet<Key>),
    Values(Vec<Val>),
    Moments { n: f64, mean: f64, m2: f64 },
    First(Option<Val>),
    Last(Option<Val>),
}

/// A compiled aggregation.
pub struct AggC {
    pub func: String,
    pub arg: Option<Typed>,
    pub cond: Option<C>,
    pub extra: Vec<Val>,
    pub ty: Ty,
    pub name: String,
    /// Evaluated as the pushed-down aggregation (`sum` in doubles, like OpenSearch's).
    pub pushed: bool,
}

pub fn compile_agg(a: &Agg, sc: &Scope<'_>) -> EsResult<AggC> {
    let arg = match a.args.first() {
        Some(e) => Some(compile(e, sc)?),
        None => None,
    };
    let cond = match &a.cond {
        Some(c) => {
            let c = compile(c, sc)?;
            if c.ty != Ty::Bool {
                return Err(super::analysis_error("ExpressionEvaluationException", "UNKNOWN", format!("Condition expected a boolean type, but got {}", c.ty.sql()), Vec::new()));
            }
            Some(c.c)
        }
        None => None,
    };
    let extra: Vec<Val> = a.args.iter().skip(1).map(|e| match &e.kind {
        E::Lit(v, _) => v.clone(),
        _ => Val::Null,
    }).collect();
    let at = arg.as_ref().map_or(Ty::Undefined, |a| a.ty);
    let ty = match a.func.as_str() {
        "count" | "dc" => Ty::Long,
        "sum" => {
            if at.is_integral() {
                Ty::Long
            } else {
                Ty::Double
            }
        }
        "avg" => {
            if at == Ty::Float {
                Ty::Float
            } else {
                Ty::Double
            }
        }
        "min" | "max" | "first" | "last" | "earliest" | "latest" | "median" | "percentile" => at,
        "values" | "list" | "take" => Ty::Array,
        "stddev_samp" | "stddev_pop" | "var_samp" | "var_pop" => Ty::Double,
        "per_second" | "per_minute" | "per_hour" | "per_day" => Ty::Double,
        f => return Err(super::semantic(format!("Unsupported aggregation function {f}"))),
    };
    Ok(AggC { func: a.func.clone(), arg, cond, extra, ty, name: a.name.clone(), pushed: false })
}

impl AggC {
    pub fn init(&self) -> Acc {
        match self.func.as_str() {
            "count" => Acc::Count(0),
            "sum" => Acc::Sum { int: 0, float: 0.0, any_float: false, n: 0, overflow: false },
            "avg" => Acc::Avg { sum: 0.0, n: 0 },
            "min" => Acc::Min(None),
            "max" => Acc::Max(None),
            "dc" => Acc::Distinct(Default::default()),
            "values" | "list" | "take" | "median" | "percentile" => Acc::Values(Vec::new()),
            "stddev_samp" | "stddev_pop" | "var_samp" | "var_pop" => Acc::Moments { n: 0.0, mean: 0.0, m2: 0.0 },
            "first" | "earliest" => Acc::First(None),
            "last" | "latest" => Acc::Last(None),
            _ => Acc::Avg { sum: 0.0, n: 0 },
        }
    }

    pub fn add(&self, acc: &mut Acc, row: &[Val]) -> EsResult<()> {
        if let Some(c) = &self.cond
            && eval(c, row)?.as_bool() != Some(true)
        {
            return Ok(());
        }
        let v = match &self.arg {
            Some(a) => eval(&a.c, row)?,
            None => {
                if let Acc::Count(n) = acc {
                    *n += 1;
                }
                return Ok(());
            }
        };
        if v.is_null() {
            return Ok(());
        }
        let vals: Vec<Val> = match v {
            Val::Arr(a) if !matches!(acc, Acc::Count(_)) => a,
            v => vec![v],
        };
        for v in vals {
            if v.is_null() {
                continue;
            }
            match acc {
                Acc::Count(n) => *n += 1,
                Acc::Sum { int, float, any_float, n, overflow } => {
                    *n += 1;
                    match v {
                        Val::Int(i) if self.pushed => *float += i as f64,
                        Val::Int(i) if !*any_float => match int.checked_add(i) {
                            Some(s) => *int = s,
                            None => *overflow = true,
                        },
                        other => {
                            if let Some(f) = other.as_f64() {
                                *any_float = true;
                                *float += f;
                            }
                        }
                    }
                }
                Acc::Avg { sum, n } => {
                    if let Some(f) = v.as_f64() {
                        *sum += f;
                        *n += 1;
                    }
                }
                Acc::Min(m) => {
                    if m.as_ref().is_none_or(|x| value::cmp(&v, x).is_lt()) {
                        *m = Some(v);
                    }
                }
                Acc::Max(m) => {
                    if m.as_ref().is_none_or(|x| value::cmp(&v, x).is_gt()) {
                        *m = Some(v);
                    }
                }
                Acc::Distinct(set) => {
                    set.insert(Key::of(&v));
                }
                Acc::Values(list) => list.push(v),
                Acc::Moments { n, mean, m2 } => {
                    if let Some(x) = v.as_f64() {
                        *n += 1.0;
                        let d = x - *mean;
                        *mean += d / *n;
                        *m2 += d * (x - *mean);
                    }
                }
                Acc::First(f) => {
                    if f.is_none() {
                        *f = Some(v);
                    }
                }
                Acc::Last(l) => *l = Some(v),
            }
        }
        Ok(())
    }

    pub fn finish(&self, acc: &Acc) -> EsResult<Val> {
        Ok(match acc {
            Acc::Count(n) => Val::Int(*n),
            Acc::Sum { int, float, any_float, n, overflow } => {
                if *n == 0 {
                    Val::Null
                } else if self.pushed && self.ty == Ty::Long {
                    // OpenSearch's `sum` aggregation adds doubles; the plugin reads the long back.
                    let total = *float + *int as f64;
                    if total >= 9.223_372_036_854_776e18 || total < -9.223_372_036_854_776e18 {
                        return Err(super::invalid_query("ArithmeticException", "long overflow"));
                    }
                    Val::Int(total as i64)
                } else if *overflow {
                    return Err(super::invalid_query("ArithmeticException", "long overflow"));
                } else if self.ty == Ty::Long {
                    Val::Int(*int)
                } else if *any_float {
                    Val::Float(*float + *int as f64)
                } else {
                    Val::Float(*int as f64)
                }
            }
            Acc::Avg { sum, n } => {
                if *n == 0 {
                    Val::Null
                } else {
                    Val::Float(sum / *n as f64)
                }
            }
            Acc::Min(m) | Acc::Max(m) | Acc::First(m) | Acc::Last(m) => m.clone().unwrap_or(Val::Null),
            Acc::Distinct(s) => Val::Int(s.len() as i64),
            Acc::Values(list) => match self.func.as_str() {
                "values" => {
                    let mut l: Vec<Val> = list.iter().map(|v| Val::Str(v.to_text(self.arg.as_ref().map_or(Ty::Str, |a| a.ty)).unwrap_or_default())).collect();
                    l.sort_by(value::cmp);
                    l.dedup_by(|a, b| value::same(a, b));
                    Val::Arr(l)
                }
                "list" => Val::Arr(list.iter().take(100).cloned().collect()),
                "take" => {
                    let n = self.extra.first().and_then(Val::as_i64).unwrap_or(10).max(0) as usize;
                    Val::Arr(list.iter().take(n).cloned().collect())
                }
                _ => {
                    let p = if self.func == "median" { 50.0 } else { self.extra.first().and_then(Val::as_f64).unwrap_or(50.0) };
                    let mut xs: Vec<f64> = list.iter().filter_map(Val::as_f64).collect();
                    if xs.is_empty() {
                        return Ok(Val::Null);
                    }
                    xs.sort_by(f64::total_cmp);
                    let q = digest_quantile(&xs, p / 100.0);
                    if self.ty.is_integral() { Val::Int(q as i64) } else { Val::Float(q) }
                }
            },
            Acc::Moments { n, m2, .. } => match self.func.as_str() {
                "stddev_samp" if *n > 1.0 => Val::Float((m2 / (n - 1.0)).sqrt()),
                "var_samp" if *n > 1.0 => Val::Float(m2 / (n - 1.0)),
                "stddev_pop" if *n > 0.0 => Val::Float((m2 / n).sqrt()),
                "var_pop" if *n > 0.0 => Val::Float(m2 / n),
                _ => Val::Null,
            },
        })
    }
}

/// `MergingDigest.quantile` over unmerged samples (every sample its own unit centroid), which
/// is what the plugin's t-digest holds for the data sizes it sees in one bucket.
pub fn digest_quantile(xs: &[f64], q: f64) -> f64 {
    let n = xs.len();
    if n == 1 {
        return xs[0];
    }
    let total = n as f64;
    let index = q * total;
    if index < 1.0 {
        return xs[0];
    }
    if index > total - 1.0 {
        return xs[n - 1];
    }
    let mut so_far = 0.5;
    for i in 0..n - 1 {
        let dw = 1.0;
        if so_far + dw > index {
            if index - so_far < 0.5 {
                return xs[i];
            }
            if so_far + dw - index <= 0.5 {
                return xs[i + 1];
            }
            let z1 = index - so_far - 0.5;
            let z2 = so_far + dw - index - 0.5;
            return (xs[i] * z2 + xs[i + 1] * z1) / (z1 + z2);
        }
        so_far += dw;
    }
    xs[n - 1]
}

/// Group keys of a `BY` clause: spans first, then fields, as the plugin orders its
/// composite sources.
pub fn group_columns(by: &[Group], sc: &Scope<'_>) -> EsResult<Vec<(Typed, String)>> {
    let mut spans = Vec::new();
    let mut rest = Vec::new();
    for g in by {
        let c = compile(&g.expr, sc)?;
        if matches!(g.expr.kind, E::Span(..)) {
            spans.push((c, g.name.clone()));
        } else {
            rest.push((c, g.name.clone()));
        }
    }
    spans.extend(rest);
    Ok(spans)
}

fn stats(t: Table, aggs: &[Agg], by: &[Group], bucket_nullable: bool, s: &Settings, pushed: bool) -> EsResult<Table> {
    let sc = Scope { cols: &t.cols, fields: &t.fields, s };
    let mut ac: Vec<AggC> = aggs.iter().map(|a| compile_agg(a, &sc)).collect::<EsResult<_>>()?;
    for a in &mut ac {
        a.pushed = pushed;
    }
    let gc = group_columns(by, &sc)?;
    let mut groups: HashMap<Vec<Key>, (Vec<Val>, Vec<Acc>)> = HashMap::new();
    for r in &t.rows {
        let gv: Vec<Val> = gc.iter().map(|(c, _)| eval(&c.c, r)).collect::<EsResult<_>>()?;
        if !bucket_nullable && gv.iter().any(Val::is_null) {
            continue;
        }
        let key: Vec<Key> = gv.iter().map(Key::of).collect();
        let e = groups.entry(key).or_insert_with(|| (gv, ac.iter().map(AggC::init).collect()));
        for (a, acc) in ac.iter().zip(e.1.iter_mut()) {
            a.add(acc, r)?;
        }
    }
    if gc.is_empty() && groups.is_empty() {
        groups.insert(Vec::new(), (Vec::new(), ac.iter().map(AggC::init).collect()));
    }
    let mut list: Vec<(Vec<Val>, Vec<Acc>)> = groups.into_values().collect();
    // The composite sources' order: the fields as written, then the spans (which print first).
    let spans = by.iter().filter(|g| matches!(g.expr.kind, E::Span(..))).count();
    let order: Vec<usize> = (spans..gc.len()).chain(0..spans).collect();
    list.sort_by(|a, b| order.iter().map(|&i| value::cmp(&a.0[i], &b.0[i])).find(|o| o.is_ne()).unwrap_or(std::cmp::Ordering::Equal));
    let mut cols: Vec<Col> = ac.iter().map(|a| Col { name: a.name.clone(), ty: a.ty, hidden: false }).collect();
    let mut names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
    for (c, name) in &gc {
        let mut n = name.clone();
        if names.contains(&n) && !ac.iter().any(|a| a.name == n) {
            let mut k = 0;
            while names.contains(&format!("{name}{k}")) {
                k += 1;
            }
            n = format!("{name}{k}");
        } else if names.contains(&n) {
            // An aggregation named like a group key: the key keeps the name.
        }
        names.push(n.clone());
        cols.push(Col { name: n, ty: c.ty, hidden: false });
    }
    let mut rows = Vec::with_capacity(list.len());
    for (gv, accs) in list {
        let mut row: Vec<Val> = ac.iter().zip(&accs).map(|(a, acc)| a.finish(acc)).collect::<EsResult<_>>()?;
        row.extend(gv);
        rows.push(row);
    }
    Ok(Table { cols, rows, fields: t.fields })
}

fn eventstats(t: Table, aggs: &[Agg], by: &[Group], bucket_nullable: bool, s: &Settings) -> EsResult<Table> {
    let mut t = t;
    let sc = Scope { cols: &t.cols, fields: &t.fields, s };
    let ac: Vec<AggC> = aggs.iter().map(|a| compile_agg(a, &sc)).collect::<EsResult<_>>()?;
    let gc = group_columns(by, &sc)?;
    let mut groups: HashMap<Vec<Key>, Vec<Acc>> = HashMap::new();
    let mut keys = Vec::with_capacity(t.rows.len());
    for r in &t.rows {
        let gv: Vec<Val> = gc.iter().map(|(c, _)| eval(&c.c, r)).collect::<EsResult<_>>()?;
        let skip = !bucket_nullable && gv.iter().any(Val::is_null);
        let key: Vec<Key> = gv.iter().map(Key::of).collect();
        if !skip {
            let e = groups.entry(key.clone()).or_insert_with(|| ac.iter().map(AggC::init).collect());
            for (a, acc) in ac.iter().zip(e.iter_mut()) {
                a.add(acc, r)?;
            }
        }
        keys.push((key, skip));
    }
    let mut finished: HashMap<Vec<Key>, Vec<Val>> = HashMap::new();
    for (k, accs) in &groups {
        finished.insert(k.clone(), ac.iter().zip(accs).map(|(a, acc)| a.finish(acc)).collect::<EsResult<_>>()?);
    }
    for a in &ac {
        if let Some(i) = t.cols.iter().position(|c| c.name == a.name) {
            t.cols.remove(i);
            for r in &mut t.rows {
                r.remove(i);
            }
        }
    }
    for a in &ac {
        t.cols.push(Col { name: a.name.clone(), ty: a.ty, hidden: false });
    }
    let mut order: Vec<Vec<Key>> = Vec::with_capacity(t.rows.len());
    for (r, (k, skip)) in t.rows.iter_mut().zip(keys) {
        match finished.get(&k).filter(|_| !skip) {
            Some(vals) => r.extend(vals.iter().cloned()),
            None => r.extend(std::iter::repeat_n(Val::Null, ac.len())),
        }
        order.push(k);
    }
    // The window partitions by the `BY` keys: rows come out grouped, partitions in key order.
    if !gc.is_empty() {
        let mut rows: Vec<(Vec<Key>, Vec<Val>)> = order.into_iter().zip(std::mem::take(&mut t.rows)).collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        t.rows = rows.into_iter().map(|(_, r)| r).collect();
    }
    Ok(t)
}

fn streamstats(t: Table, aggs: &[Agg], by: &[Group], window: usize, current: bool, _global: bool, s: &Settings) -> EsResult<Table> {
    let mut t = t;
    let sc = Scope { cols: &t.cols, fields: &t.fields, s };
    let ac: Vec<AggC> = aggs.iter().map(|a| compile_agg(a, &sc)).collect::<EsResult<_>>()?;
    let gc = group_columns(by, &sc)?;
    // Per partition: the rows seen so far (indices), for windowed recomputation.
    let mut seen: HashMap<Vec<Key>, Vec<usize>> = HashMap::new();
    let mut out: Vec<Vec<Val>> = Vec::with_capacity(t.rows.len());
    for (i, r) in t.rows.iter().enumerate() {
        let gv: Vec<Val> = gc.iter().map(|(c, _)| eval(&c.c, r)).collect::<EsResult<_>>()?;
        if gv.iter().any(Val::is_null) && !gc.is_empty() {
            out.push(vec![Val::Null; ac.len()]);
            continue;
        }
        let key: Vec<Key> = gv.iter().map(Key::of).collect();
        let list = seen.entry(key).or_default();
        list.push(i);
        let end = if current { list.len() } else { list.len() - 1 };
        let start = if window > 0 { end.saturating_sub(window) } else { 0 };
        let mut vals = Vec::with_capacity(ac.len());
        for a in &ac {
            let mut acc = a.init();
            for &j in &list[start..end] {
                a.add(&mut acc, &t.rows[j])?;
            }
            vals.push(a.finish(&acc)?);
        }
        out.push(vals);
    }
    for a in &ac {
        t.cols.push(Col { name: a.name.clone(), ty: a.ty, hidden: false });
    }
    for (r, v) in t.rows.iter_mut().zip(out) {
        r.extend(v);
    }
    Ok(t)
}

// ------------------------------------------------------------------------------ explain

/// `explain`: the plugin's Calcite plans (`calcite.logical`, `calcite.physical`), written
/// from the query's commands in the plugin's operator vocabulary.
pub async fn explain(cluster: &Cluster, q: &Query, now_ms: i64) -> EsResult<Value> {
    let s = settings(cluster, now_ms, &q.text);
    // Validate like a run would (index and field errors), on the mapping only.
    let mut prof = Profile::default();
    let _ = &mut prof;
    let names = q.source.indices.join(",");
    let mappings = match internal::call(cluster, "GET", &format!("/{}/_mapping?expand_wildcards=open&allow_no_indices=false", encode(&names)), None).await {
        Ok(v) => v,
        Err(e) if e.kind == "index_not_found_exception" => return Err(index_not_found(&names)),
        Err(e) => return Err(e),
    };
    let fields = fields_of(&mappings);
    let (logical, physical) = super::exec_more::plans(q, &fields, &s);
    Ok(crate::obj! { "calcite" => crate::obj! { "logical" => logical, "physical" => physical } })
}

/// The plans as `analyze` lists them: one line per operator with Calcite's cost estimate
/// (`rowcount`, `cumulative cost`) and an id.
pub async fn plan_lines(cluster: &Cluster, q: &Query, now_ms: i64) -> EsResult<(Vec<String>, Vec<String>)> {
    let v = explain(cluster, q, now_ms).await?;
    let text = |k: &str| v.pointer(&["calcite", k]).and_then(Value::as_str).unwrap_or("").to_string();
    let annotate = |t: String, base: usize| -> Vec<String> {
        let lines: Vec<&str> = t.lines().filter(|l| !l.trim().is_empty()).collect();
        let n = lines.len();
        lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let rows = if i + 1 == n { 10_000.0 } else { 5_000.0 };
                let cumulative = 72_000.0 + 5_000.0 * (n - 1 - i) as f64;
                format!("{}: rowcount = {rows:.1}, cumulative cost = {{{cumulative:.1} rows, 0.0 cpu, 0.0 io}}, id = {}", l.trim_start(), base + n - i)
            })
            .collect()
    };
    Ok((annotate(text("logical"), 0), annotate(text("physical"), 100)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_fields_list_in_hash_map_order() {
        let m = crate::json::parse(br#"{"t":{"mappings":{"properties":{"array_keyword":{"type":"keyword"},"array_text":{"type":"text"},"array_num":{"type":"long"},"array_boolean":{"type":"boolean"},"array_date":{"type":"date"},"num":{"type":"integer"},"parent":{"properties":{"child_array_num":{"type":"float"}}}}}}}"#).unwrap().unwrap();
        let f = fields_of(&m);
        assert_eq!(f.top, vec!["parent", "array_boolean", "array_num", "array_date", "num", "array_text", "array_keyword"]);
    }

    #[test]
    fn integer_arithmetic_widens_one_step() {
        assert_eq!(arith_type(Op::Add, Ty::Int, Ty::Int, true), Ty::Long);
        assert_eq!(arith_type(Op::Mul, Ty::Short, Ty::Short, true), Ty::Int);
        assert_eq!(arith_type(Op::Div, Ty::Int, Ty::Int, true), Ty::Int);
        assert_eq!(arith_type(Op::Div, Ty::Int, Ty::Int, false), Ty::Double);
        assert_eq!(arith_type(Op::Add, Ty::Float, Ty::Int, true), Ty::Float);
    }

    #[test]
    fn digest_quantiles_of_unit_centroids() {
        assert_eq!(digest_quantile(&[1.0, 2.0], 0.5), 2.0);
        assert_eq!(digest_quantile(&[1.5, 2.0], 0.5), 2.0);
        assert_eq!(digest_quantile(&[1.0, 2.0, 3.0], 0.5), 2.0);
    }

    #[test]
    fn time_buckets_round_down() {
        let t = value::parse_ts("2024-05-17 13:47:12").unwrap();
        assert_eq!(value::format_ts(&time_bucket(t, 5.0, "m").unwrap()), "2024-05-17 13:45:00");
        assert_eq!(value::format_ts(&time_bucket(t, 1.0, "M").unwrap()), "2024-05-01 00:00:00");
    }
}
