//! Aggregations through `search-aggs`: request parsing (roll JSON → aggs JSON), the per-shard
//! [`Lookup`] (`FieldLookup`: field types and doc-values encodings, filter queries, nested and
//! join relations, `now`), the reduce and the rendering back.

use super::query::QueryCtx;
use super::Target;
use crate::cluster::Cluster;
use crate::engine::fields::{FieldTable, Kind, NESTED_PATH_FIELD};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Number, Value};
use crate::meta::ClusterState;
use search_aggs::{AggField, AggRequest, FieldLookup, Json, NestedLookup, NestedPath, NumericType, SourceType};
use search_index::query::TermQuery;
use search_index::{QueryRef, Searcher};
use std::sync::Arc;

/// roll JSON → aggs JSON (order kept).
pub fn to_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Num(Number::Int(i)) => Json::Int(*i),
        Value::Num(Number::UInt(u)) => Json::UInt(*u),
        Value::Num(Number::Big(s)) => Json::Float(s.parse().unwrap_or(f64::NAN)),
        Value::Num(Number::Float(f)) => Json::Float(*f),
        Value::Str(s) => Json::Str(s.clone()),
        Value::Arr(a) => Json::Arr(a.iter().map(|x| crate::stack::maybe_grow(|| to_json(x))).collect()),
        Value::Obj(m) => Json::Obj(m.iter().map(|(k, x)| (k.clone(), crate::stack::maybe_grow(|| to_json(x)))).collect()),
    }
}

/// aggs JSON → roll JSON.
pub fn from_json(j: &Json) -> Value {
    match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Int(i) => Value::Num(Number::Int(*i)),
        Json::UInt(u) => Value::Num(Number::UInt(*u)),
        Json::Float(f) => {
            if f.is_finite() {
                Value::Num(Number::Float(*f))
            } else {
                Value::Str(crate::json::write::java_double(*f))
            }
        }
        Json::Float32(f) => {
            if f.is_finite() {
                Value::Num(Number::Float(crate::fetch::java_float_as_double(*f)))
            } else {
                Value::Str(crate::json::write::java_float(*f))
            }
        }
        Json::Str(s) => Value::Str(s.clone()),
        Json::Arr(a) => Value::Arr(a.iter().map(|x| crate::stack::maybe_grow(|| from_json(x))).collect()),
        Json::Obj(o) => {
            let mut m = Map::with_capacity(o.len());
            for (k, x) in o {
                m.insert(k.clone(), crate::stack::maybe_grow(|| from_json(x)));
            }
            Value::Obj(m)
        }
    }
}

/// An aggregation error as an [`EsError`].
pub fn agg_error(e: search_aggs::AggError) -> EsError {
    let search_aggs::AggError { kind, mut reason, status, meta, caused_by } = e;
    if kind == "script_exception" {
        let (full, text) = super::scripts::take_script_error(&reason);
        if let Some(full) = full {
            return full;
        }
        reason = text.to_string();
    }
    let mut out = if search_analysis::is_java_exception(&kind) || kind == "class_cast_exception" || kind == "arithmetic_exception" {
        EsError::java(status, kind, reason)
    } else {
        EsError::new(status, kind, reason)
    };
    for (k, v) in meta {
        out = out.with_meta(k, from_json(&v));
    }
    if let Some(c) = caused_by {
        out = out.caused_by(agg_error(*c));
    }
    out
}

/// Rewrites aggregations over runtime fields (`field: <runtime field>` of the targets'
/// mappings or the request's `runtime_mappings`) into value scripts standing for them.
/// Aggregations may not read `lookup` runtime fields (`LookupRuntimeFieldType.fielddataBuilder`).
pub fn check_lookup_fields(aggs: &Map, rm: &Map) -> EsResult<()> {
    fn walk(v: &Value, rm: &Map) -> EsResult<()> {
        match v {
            Value::Obj(m) => {
                if let Some(f) = m.get("field").and_then(Value::as_str)
                    && rm.get(f).and_then(|d| d.get("type")).and_then(Value::as_str) == Some("lookup")
                {
                    return Err(EsError::illegal_argument(format!("Fielddata is not supported on field [{f}] of type [lookup]")));
                }
                m.values().try_for_each(|x| walk(x, rm))
            }
            Value::Arr(a) => a.iter().try_for_each(|x| walk(x, rm)),
            _ => Ok(()),
        }
    }
    aggs.values().try_for_each(|x| walk(x, rm))
}

/// `top_metrics` reads keyword metrics through segment ordinals, which runtime keyword fields
/// don't have (`TopMetricsAggregator.MetricValues`).
pub fn check_top_metrics_runtime(aggs: &Map, def: &dyn Fn(&str) -> Option<Value>) -> EsResult<()> {
    for (_, body) in aggs {
        let Value::Obj(b) = body else { continue };
        for (k, v) in b {
            let Value::Obj(m) = v else { continue };
            if k == "aggs" || k == "aggregations" {
                check_top_metrics_runtime(m, def)?;
                continue;
            }
            if k != "top_metrics" {
                continue;
            }
            let metrics: Vec<&Value> = match m.get("metrics") {
                Some(Value::Arr(a)) => a.iter().collect(),
                Some(o) => vec![o],
                None => Vec::new(),
            };
            for metric in metrics {
                if let Some(f) = metric.get("field").and_then(Value::as_str)
                    && let Some(d) = def(f)
                    && d.get("type").and_then(Value::as_str) == Some("keyword")
                {
                    return Err(EsError::illegal_argument(format!(
                        "top_metrics can only collect bytes that have segment ordinals but Field [{f}] of type [keyword] does not"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Aggregations whose users can't give a `script`: a runtime field's script reaches them under
/// search-aggs' internal key instead.
const UNSCRIPTABLE: &[&str] = &["geo_bounds", "geohash_grid", "geotile_grid", "geohex_grid", "ip_range", "ip_prefix"];

/// Aggregations whose values source takes a `format`.
const FORMATTABLE: &[&str] = &[
    "terms", "rare_terms", "significant_terms", "histogram", "date_histogram", "auto_date_histogram", "range", "date_range", "min", "max", "avg", "sum", "stats",
    "extended_stats", "percentiles", "percentile_ranks", "median_absolute_deviation",
];

pub fn rewrite_runtime(aggs: &mut Map, def: &dyn Fn(&str) -> Option<Value>) {
    for (_, body) in aggs.iter_mut() {
        let Value::Obj(b) = body else { continue };
        for (k, v) in b.iter_mut() {
            let Value::Obj(m) = v else { continue };
            if k == "aggs" || k == "aggregations" {
                rewrite_runtime(m, def);
                continue;
            }
            let field = m.get("field").and_then(Value::as_str).map(str::to_string);
            if let Some(name) = field
                && let Some(d) = def(&name)
                && let Some(f) = super::runtime::from_agg_params(&crate::obj! { super::runtime::AGG_MARKER => name.as_str(), "def" => d.clone() })
            {
                m.shift_remove("field");
                if let Value::Obj(sm) = super::runtime::agg_script(&f, &d) {
                    for (sk, sv) in sm {
                        if !(sk == "value_type" && m.contains_key("value_type")) {
                            let key = if sk == "script" && UNSCRIPTABLE.contains(&k.as_str()) { search_aggs::values::RUNTIME_SCRIPT.to_string() } else { sk };
                            m.insert(key, sv);
                        }
                    }
                }
                // A date field's values print in its own format unless the aggregation sets
                // one (`DateScriptFieldType.docValueFormat`).
                if f.ty == super::runtime::RtType::Date
                    && FORMATTABLE.contains(&k.as_str())
                    && !m.contains_key("format")
                    && let Some(fmt) = d.get("format").and_then(Value::as_str)
                {
                    m.insert("format".into(), Value::from(fmt));
                }
            }
            // `composite` sources name fields one level down.
            if k == "composite"
                && let Some(Value::Arr(sources)) = m.get_mut("sources")
            {
                for src in sources.iter_mut() {
                    if let Value::Obj(sm) = src {
                        for (_, inner) in sm.iter_mut() {
                            if let Value::Obj(im) = inner {
                                let mut wrapped = Map::new();
                                wrapped.insert("x".into(), Value::Obj(std::mem::take(im)));
                                rewrite_runtime(&mut wrapped, def);
                                if let Some(Value::Obj(back)) = wrapped.shift_remove("x") {
                                    *im = back;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Parses the `aggs` section. `raw` is the body the request's own section (`original`, before
/// any rewrite) came from: a section that fails parses again over it, so the error locates
/// itself in the body as Elasticsearch's does.
pub fn parse(aggs: &Map, raw: Option<(&crate::search_source::AggsRaw, &Map)>) -> EsResult<AggRequest> {
    let mut aggs = std::borrow::Cow::Borrowed(aggs);
    if mentions_tsid(aggs.as_ref()) {
        tsid_after_keys(aggs.to_mut())?;
    }
    let j = to_json(&Value::Obj(aggs.into_owned()));
    let check = |q: &Json, path: &[search_aggs::xparser::Seg]| check_agg_query(q, path, None);
    let req = match search_aggs::parse(&j, &search_aggs::ParseOptions { check_query: Some(&check), ..Default::default() }) {
        Ok(r) => r,
        Err(e) => {
            if let Some((r, original)) = raw {
                let check = |q: &Json, path: &[search_aggs::xparser::Seg]| check_agg_query(q, path, Some(&r.body));
                let opts = search_aggs::ParseOptions {
                    src: Some(&r.body),
                    base: vec![search_aggs::xparser::Seg::Key(r.key.clone())],
                    check_query: Some(&check),
                    ..Default::default()
                };
                if let Err(located) = search_aggs::parse(&to_json(&Value::Obj(original.clone())), &opts) {
                    return Err(agg_error(located));
                }
            }
            return Err(agg_error(e));
        }
    };
    let errs = req.validate();
    if !errs.is_empty() {
        return Err(EsError::action_request_validation(&errs));
    }
    Ok(req)
}

/// The syntax error of an `aggs` section, located in the body it came from when known: the
/// REST layer parses aggregations with the rest of the body (`_msearch` parses every body
/// before running any).
pub fn parse_error(aggs: &Map, raw: Option<&crate::search_source::AggsRaw>) -> EsResult<()> {
    let j = to_json(&Value::Obj(aggs.clone()));
    let src = raw.map(|r| r.body.as_slice());
    let check = |q: &Json, path: &[search_aggs::xparser::Seg]| check_agg_query(q, path, src);
    let opts = match raw {
        Some(r) => search_aggs::ParseOptions { src, base: vec![search_aggs::xparser::Seg::Key(r.key.clone())], check_query: Some(&check), ..Default::default() },
        None => search_aggs::ParseOptions { check_query: Some(&check), ..Default::default() },
    };
    search_aggs::parse(&j, &opts).map(|_| ()).map_err(agg_error)
}

/// Parses a query embedded in an aggregation (`filter`, `filters`, `adjacency_matrix`) as
/// Elasticsearch does while it parses the request: a malformed one fails the whole request,
/// located in the body, before any shard runs.
fn check_agg_query(q: &Json, path: &[search_aggs::xparser::Seg], src: Option<&[u8]>) -> search_aggs::Result<()> {
    let segs: Vec<crate::json::locate::Seg<'_>> = path
        .iter()
        .map(|s| match s {
            search_aggs::xparser::Seg::Key(k) => crate::json::locate::Seg::Key(k.as_str()),
            search_aggs::xparser::Seg::Index(i) => crate::json::locate::Seg::Index(*i),
        })
        .collect();
    crate::query::parse_query(&from_json(q), src, &segs).map(|_| ()).map_err(es_to_agg_error)
}

/// The inverse of [`agg_error`], for query errors raised inside aggregation parsing.
fn es_to_agg_error(e: EsError) -> search_aggs::AggError {
    let mut out = search_aggs::AggError::new(e.status, e.kind.clone(), e.reason().to_string());
    out.meta = e.meta().iter().map(|(k, v)| (k.to_string(), to_json(v))).collect();
    out.caused_by = e.caused_by.map(|c| Box::new(es_to_agg_error(*c)));
    out
}

/// `AggregatorFactories.Builder.validate` messages of an `aggs` section that parses (a
/// section that doesn't parse fails on its own later): `_msearch` validates every request
/// before running any.
pub fn validation_errors(aggs: &Map) -> Vec<String> {
    match search_aggs::parse(&to_json(&Value::Obj(aggs.clone())), &search_aggs::ParseOptions::default()) {
        Ok(req) => req.validate(),
        Err(_) => Vec::new(),
    }
}

/// Whether an aggregation tree names the `_tsid` field anywhere (the fast path skips the
/// `_tsid` key rewriting otherwise).
pub fn mentions_tsid(aggs: &Map) -> bool {
    fn any(v: &Value) -> bool {
        match v {
            Value::Str(s) => s == "_tsid",
            Value::Arr(a) => a.iter().any(any),
            Value::Obj(m) => m.values().any(any),
            _ => false,
        }
    }
    aggs.values().any(any)
}

/// The `_tsid` sources of a `composite` definition, by source name.
fn composite_tsid_sources(def: &Map) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(Value::Arr(sources)) = def.get("sources") {
        for s in sources {
            if let Value::Obj(m) = s {
                for (name, src) in m {
                    if src.get("terms").and_then(|t| t.get("field")).and_then(Value::as_str) == Some("_tsid") {
                        out.push(name.clone());
                    }
                }
            }
        }
    }
    out
}

/// Each aggregation of a tree: `(name, type, definition, sub-aggregations)`.
fn each_agg(aggs: &Map) -> impl Iterator<Item = (&String, &str, &Map, Option<&Map>)> {
    aggs.iter().filter_map(|(name, body)| {
        let Value::Obj(b) = body else { return None };
        let subs = b.get("aggs").or_else(|| b.get("aggregations")).and_then(Value::as_object);
        let (ty, def) = b.iter().find(|(k, _)| !matches!(k.as_str(), "aggs" | "aggregations" | "meta"))?;
        Some((name, ty.as_str(), def.as_object()?, subs))
    })
}

/// `composite.after` values of `_tsid` sources: base64url → the hex doc values.
fn tsid_after_keys(aggs: &mut Map) -> EsResult<()> {
    for (_, body) in aggs.iter_mut() {
        let Value::Obj(b) = body else { continue };
        for (k, def) in b.iter_mut() {
            let Value::Obj(def) = def else { continue };
            if k == "aggs" || k == "aggregations" {
                tsid_after_keys(def)?;
            } else if k == "composite" {
                let names = composite_tsid_sources(def);
                if let Some(Value::Obj(after)) = def.get_mut("after") {
                    for n in names {
                        if let Some(Value::Str(s)) = after.get_mut(&n) {
                            *s = String::from_utf8(crate::engine::tsdb::tsid_parse(s)?).unwrap_or_default();
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Prints the keys of `terms`-like aggregations and `composite` sources over `_tsid` as
/// Elasticsearch's `TIME_SERIES_ID` format does (base64url), walking the request next to the
/// rendered response.
pub fn render_tsid_keys(aggs: &Map, out: &mut Value, typed_keys: bool) {
    let Value::Obj(out) = out else { return };
    for (name, ty, def, subs) in each_agg(aggs) {
        let key = if typed_keys { out.keys().find(|k| k.split_once('#').is_some_and(|(_, n)| n == name)).cloned() } else { Some(name.clone()) };
        let Some(Value::Obj(res)) = key.and_then(|k| out.get_mut(&k)) else { continue };
        let tsid_terms = matches!(ty, "terms" | "rare_terms" | "significant_terms") && def.get("field").and_then(Value::as_str) == Some("_tsid");
        let composite = if ty == "composite" { composite_tsid_sources(def) } else { Vec::new() };
        // `multi_terms`: the positions of `_tsid` terms in the key (and its `|`-joined string).
        let multi: Vec<usize> = match (ty, def.get("terms")) {
            ("multi_terms", Some(Value::Arr(terms))) => {
                terms.iter().enumerate().filter(|(_, t)| t.get("field").and_then(Value::as_str) == Some("_tsid")).map(|(i, _)| i).collect()
            }
            _ => Vec::new(),
        };
        let fix = |v: &mut Value| {
            if let Value::Str(s) = v {
                *s = crate::engine::tsdb::tsid_display(s.as_bytes());
            }
        };
        if !composite.is_empty()
            && let Some(Value::Obj(after)) = res.get_mut("after_key")
        {
            for n in &composite {
                if let Some(v) = after.get_mut(n) {
                    fix(v);
                }
            }
        }
        let mut each_bucket = |b: &mut Value| {
            if let Value::Obj(bm) = b {
                if tsid_terms && let Some(k) = bm.get_mut("key") {
                    fix(k);
                }
                if !multi.is_empty()
                    && let Some(Value::Arr(parts)) = bm.get_mut("key")
                {
                    let mut swaps = Vec::new();
                    for &i in &multi {
                        if let Some(Value::Str(hex)) = parts.get_mut(i) {
                            let shown = crate::engine::tsdb::tsid_display(hex.as_bytes());
                            swaps.push((std::mem::replace(hex, shown.clone()), shown));
                        }
                    }
                    if let Some(Value::Str(ks)) = bm.get_mut("key_as_string") {
                        for (hex, shown) in swaps {
                            *ks = ks.replacen(&hex, &shown, 1);
                        }
                    }
                }
                if !composite.is_empty()
                    && let Some(Value::Obj(km)) = bm.get_mut("key")
                {
                    for n in &composite {
                        if let Some(v) = km.get_mut(n) {
                            fix(v);
                        }
                    }
                }
                if let Some(subs) = subs {
                    let mut tmp = Value::Obj(std::mem::take(bm));
                    render_tsid_keys(subs, &mut tmp, typed_keys);
                    if let Value::Obj(m) = tmp {
                        *bm = m;
                    }
                }
            }
        };
        match res.get_mut("buckets") {
            Some(Value::Arr(a)) => a.iter_mut().for_each(&mut each_bucket),
            Some(Value::Obj(m)) => {
                if tsid_terms {
                    let old = std::mem::take(m);
                    for (k, mut v) in old {
                        each_bucket(&mut v);
                        m.insert(crate::engine::tsdb::tsid_display(k.as_bytes()), v);
                    }
                } else {
                    m.values_mut().for_each(&mut each_bucket);
                }
            }
            _ => {
                // Single-bucket aggregations: their sub-aggregations sit in the result itself.
                if let Some(subs) = subs {
                    let mut tmp = Value::Obj(std::mem::take(res));
                    render_tsid_keys(subs, &mut tmp, typed_keys);
                    if let Value::Obj(m) = tmp {
                        *res = m;
                    }
                }
            }
        }
    }
}

/// `search.aggs.only_allowed_metric_scripts`: every `scripted_metric` script must be listed in
/// `search.aggs.allowed_inline_metric_scripts` (by source) or
/// `search.aggs.allowed_stored_metric_scripts` (by id). Checked per shard, in the order
/// `ScriptedMetricAggregationBuilder` builds them.
pub fn check_metric_scripts(st: &ClusterState, aggs: &Map) -> EsResult<()> {
    let md = &st.metadata;
    if md.cluster_setting("search.aggs.only_allowed_metric_scripts") != Some("true") {
        return Ok(());
    }
    let list = |k: &str| md.transient.get_list(k).or_else(|| md.persistent.get_list(k)).unwrap_or_default();
    let inline = list("search.aggs.allowed_inline_metric_scripts");
    let stored = list("search.aggs.allowed_stored_metric_scripts");
    fn walk(aggs: &Map, inline: &[String], stored: &[String]) -> EsResult<()> {
        for (name, ty, body, subs) in each_agg(aggs) {
            if ty == "scripted_metric" {
                for field in ["init_script", "map_script", "combine_script", "reduce_script"] {
                    let allowed = match body.get(field) {
                        None => true,
                        Some(Value::Str(src)) => inline.contains(src),
                        Some(Value::Obj(o)) => match (o.get("id").and_then(Value::as_str), o.get("source")) {
                            (Some(id), _) => stored.iter().any(|s| s == id),
                            (None, Some(Value::Str(src))) => inline.contains(src),
                            _ => false,
                        },
                        Some(_) => false,
                    };
                    if !allowed {
                        return Err(EsError::illegal_argument(format!("[{field}] contains not allowed script: [{name}]")));
                    }
                }
            }
            if let Some(subs) = subs {
                walk(subs, inline, stored)?;
            }
        }
        Ok(())
    }
    walk(aggs, &inline, &stored)
}

/// The final reduce, rendered.
pub fn reduce(cluster: &Cluster, st: &ClusterState, req: &AggRequest, parts: Vec<search_aggs::Aggs>, typed_keys: bool) -> EsResult<Value> {
    let max_buckets = st.metadata.cluster_setting("search.max_buckets").and_then(|v| v.parse::<usize>().ok()).unwrap_or(65536);
    let host: Arc<dyn search_aggs::ScriptHost> = Arc::new(super::agg_scripts::Host::new(cluster, None, 0, String::new()));
    let opts = search_aggs::ReduceOptions { final_reduce: true, max_buckets, scripts: Some(host), budget: Some(cluster.breakers.request(cluster, st)), ..Default::default() };
    let out = search_aggs::reduce(parts, req, &opts).map_err(|e| reduce_failure(agg_error(e)))?;
    Ok(from_json(&out.to_json(typed_keys)))
}

/// A failure of the final reduce. Elasticsearch reduces at the start of the fetch phase, on
/// the coordinator, so no shard failed: `search_phase_execution_exception` for phase `fetch`
/// with no shard failures (and so no root causes), the cause's status.
pub fn reduce_failure(cause: EsError) -> EsError {
    EsError::new(cause.status, "search_phase_execution_exception", "")
        .with_meta("phase", "fetch")
        .with_meta("grouped", true)
        .with_meta("failed_shards", Value::Arr(Vec::new()))
        .with_root_causes(Vec::new())
        .caused_by(cause)
}

/// One shard's view of its index for aggregations.
pub struct Lookup<'a> {
    cluster: &'a Cluster,
    st: &'a ClusterState,
    table: Arc<FieldTable>,
    index: String,
    uuid: String,
    now: i64,
    shard: (u32, u32),
    idx: Arc<crate::meta::IndexMeta>,
    searcher: Searcher,
    alias_filter: Option<crate::query::Query>,
    parent_fields: Arc<Vec<crate::search_source::FieldAndFormat>>,
    rescore: Option<Arc<Vec<super::PreparedRescore>>>,
    security: Option<crate::xpack::security::dls::ReadSecurity>,
}

impl<'a> Lookup<'a> {
    /// The search's rescorers, which `top_hits` sorted by score applies to its buckets.
    pub(crate) fn with_rescore(mut self, rescore: Vec<super::PreparedRescore>) -> Self {
        self.rescore = (!rescore.is_empty()).then(|| Arc::new(rescore));
        self
    }

    /// The search's top-level `fields`: `top_hits` without its own `fields` fetches them.
    pub fn with_parent_fields(mut self, fields: &[crate::search_source::FieldAndFormat]) -> Self {
        self.parent_fields = Arc::new(fields.to_vec());
        self
    }

    pub fn new(cluster: &'a Cluster, st: &'a ClusterState, t: &Target, table: &Arc<FieldTable>, searcher: &Searcher, now: i64) -> Lookup<'a> {
        Lookup {
            cluster,
            st,
            table: table.clone(),
            index: t.idx.name.clone(),
            uuid: t.idx.uuid.clone(),
            now,
            shard: (t.shard.id, t.idx.number_of_shards()),
            idx: t.idx.clone(),
            searcher: searcher.clone(),
            alias_filter: t.alias_filter.clone(),
            parent_fields: Arc::new(Vec::new()),
            rescore: None,
            security: t.security.clone(),
        }
    }
}

fn numeric(k: Kind) -> Option<NumericType> {
    Some(match k {
        Kind::Long => NumericType::Long,
        Kind::Integer | Kind::TokenCount => NumericType::Integer,
        Kind::Short => NumericType::Short,
        Kind::Byte => NumericType::Byte,
        Kind::UnsignedLong => NumericType::UnsignedLong,
        Kind::Double => NumericType::Double,
        Kind::Float => NumericType::Float,
        Kind::HalfFloat => NumericType::HalfFloat,
        Kind::ScaledFloat(sf) => NumericType::ScaledFloat(sf),
        _ => return None,
    })
}

impl Lookup<'_> {
    /// A sort's `nested` object (`NestedSortBuilder`): the innermost level's path docs
    /// (with its `filter`) as children, its parent level's docs as parents.
    /// A `_script` or `_geo_distance` key of a `top_hits` / `top_metrics` sort.
    /// A hit sort on a runtime field (it shadows a mapped field of the name).
    fn runtime_sort(&self, field: &str, reverse: bool) -> search_aggs::Result<Option<search_aggs::HitSort>> {
        let err = |e: EsError| search_aggs::AggError::new(e.status, e.kind.to_string(), e.reason().to_string());
        let ctx = QueryCtx::new(self.cluster, self.st, &self.idx, &self.table, &self.searcher, self.now);
        let rf = super::runtime::RuntimeScope::ambient().filter(|s| s.index_name() == self.idx.name).and_then(|s| s.field(field).cloned()).or_else(|| ctx.runtime_field(field));
        let Some(rf) = rf.filter(|f| f.ty != super::runtime::RtType::GeoPoint) else { return Ok(None) };
        let ev = super::runtime::Evaluator::new(&self.cluster.scripts, self.st, &self.table, self.shard.0 as i32, &self.idx.name, rf).map_err(err)?.counted();
        Ok(Some(search_aggs::HitSort::Computed { sort: Arc::new(super::computed_sort::RuntimeKey { ev: Arc::new(ev), reverse }), reverse }))
    }

    fn computed_sort(&self, item: &Json) -> search_aggs::Result<search_aggs::HitSort> {
        let err = |e: EsError| search_aggs::AggError::new(e.status, e.kind.to_string(), e.reason().to_string());
        let sorts = crate::search_source::parse_sorts_value(&from_json(item)).map_err(err)?;
        let ctx = QueryCtx::new(self.cluster, self.st, &self.idx, &self.table, &self.searcher, self.now);
        match sorts.into_iter().next() {
            Some(crate::search_source::Sort::Script { script, ty, order, .. }) => {
                let reverse = order == Some(crate::search_source::SortOrder::Desc);
                let (kind, context) = match ty.as_str() {
                    "string" => ("string", search_painless::Context::StringSort),
                    "version" => ("version", search_painless::Context::BytesRefSort),
                    _ => ("number", search_painless::Context::NumberSort),
                };
                let painless = super::extra::painless(&ctx, &script, context).map_err(err)?;
                Ok(search_aggs::HitSort::Computed { sort: Arc::new(super::computed_sort::ScriptKey { painless, kind }), reverse })
            }
            Some(crate::search_source::Sort::GeoDistance(m)) => match super::sort::geo_distance(&ctx, &m).map_err(err)?.0 {
                search_index::collector::top_field::SortKey::GeoDistance(g) => {
                    let reverse = g.reverse;
                    Ok(search_aggs::HitSort::Computed { sort: Arc::new(super::computed_sort::GeoKey(g)), reverse })
                }
                _ => Ok(search_aggs::HitSort::Doc { reverse: false }),
            },
            _ => Err(search_aggs::AggError::illegal_argument("malformed sort")),
        }
    }

    fn nested_sort(&self, n: &Json) -> search_aggs::Result<search_index::collector::top_field::NestedSort> {
        let mut levels = Vec::new();
        let mut cur = Some(n);
        while let Some(l) = cur {
            let path = l.get("path").and_then(Json::as_str).unwrap_or_default();
            if !self.table.objects.get(path).is_some_and(|o| o.nested) {
                return Err(search_aggs::AggError::new(400, "query_shard_exception", format!("[nested] nested object under path [{path}] is not of nested type")));
            }
            let filter = l.get("filter").map(|f| FieldLookup::query(self, f)).transpose()?;
            levels.push((path, filter));
            cur = l.get("nested");
        }
        let max_children = n.get("max_children").and_then(Json::as_i64).map(|m| m.max(0) as usize);
        Ok(super::sort::nested_chain(None, &levels, max_children))
    }
}

impl FieldLookup for Lookup<'_> {
    fn field(&self, name: &str) -> search_aggs::Result<Option<AggField>> {
        let Some(f) = self.table.get(name) else {
            if name == "_id" && crate::engine::tsdb::is_time_series(&self.idx) {
                return Err(search_aggs::AggError::illegal_argument("Fielddata is not supported on [_id] field in [time_series] indices"));
            }
            // `IdFieldMapper`: `_id` fielddata is off unless `indices.id_field_data.enabled`
            // (Elasticsearch's default is false; OpenSearch's true).
            if name == "_id" {
                if !super::id_fielddata::enabled(self.st) {
                    return Err(search_aggs::AggError::illegal_argument(super::id_fielddata::DISABLED));
                }
                return Ok(Some(super::id_fielddata::agg_field()));
            }
            if name == "_tsid" && crate::engine::tsdb::is_time_series(&self.idx) {
                // Hex doc values: keys come out hex and are printed base64url by
                // [`render_tsid_keys`].
                return Ok(Some(AggField::new("_tsid", "_tsid", SourceType::Keyword, crate::engine::fields::TSID_FIELD)));
            }
            if name == "_ignored" {
                // Sorted-set doc values of the ignored field names (8.15+).
                return Ok(Some(AggField::new("_ignored", "_ignored", SourceType::Keyword, crate::engine::fields::IGNORED_FIELD)));
            }
            // `root.key` of a `flattened` field: that key's values of the `._keyed` field.
            if let Some(k) = self.table.keyed(name) {
                if !k.doc_values {
                    return Err(search_aggs::AggError::illegal_argument(format!(
                        "Can't load fielddata on [{name}] because fielddata is unsupported on fields of type [{}]. Use doc values instead.",
                        k.ty
                    )));
                }
                // `flattened`, or OpenSearch's `flat_object` (named in unsupported-type errors).
                let mut a = AggField::new(name, k.ty.as_str(), SourceType::Keyword, k.number);
                a.flattened_key = k.flattened_key.clone();
                a.nested_path = k.nested_path.clone();
                return Ok(Some(a));
            }
            // `ConstantFieldType`s: one value for the whole shard.
            let constant = match name {
                "_index" => Some(self.idx.name.clone()),
                "_index_mode" => Some(crate::meta::index_mode::IndexMode::of(&self.idx.settings).name().to_string()),
                _ => None,
            };
            if let Some(c) = constant {
                let mut a = AggField::new(name, name, SourceType::Keyword, u32::MAX);
                a.constant = Some(c);
                return Ok(Some(a));
            }
            return Ok(None);
        };
        // OpenSearch's `FlatObjectFieldType.docValueFormat` refuses the root field.
        if f.ty == "flat_object" {
            return Err(search_aggs::AggError::illegal_argument(format!("Field [{name}] of type [flat_object] does not support doc_value in root field")));
        }
        if f.kind == Kind::ConstantKeyword {
            let mut a = AggField::new(f.name.clone(), f.ty.clone(), SourceType::Keyword, u32::MAX);
            a.constant = f.constant_value.clone();
            // Without a value no document has one: the field aggregates like an unmapped one.
            return Ok(a.constant.is_some().then_some(a));
        }
        let mut number = f.number;
        let source = match f.kind {
            k if numeric(k).is_some() => SourceType::Numeric(numeric(k).unwrap()),
            Kind::Date => SourceType::Date { nanos: false },
            Kind::DateNanos => SourceType::Date { nanos: true },
            Kind::Boolean => SourceType::Boolean,
            Kind::Keyword | Kind::Version | Kind::Wildcard | Kind::CountedKeyword | Kind::Flattened | Kind::Join => SourceType::Keyword,
            Kind::Ip => SourceType::Ip,
            Kind::GeoPoint => SourceType::GeoPoint,
            Kind::Point => SourceType::CartesianPoint,
            Kind::GeoShape => SourceType::GeoShape { centroid: f.subfield("centroid") },
            Kind::Shape => SourceType::CartesianShape { centroid: f.subfield("centroid") },
            Kind::Histogram => match f.ty.as_str() {
                "exponential_histogram" => SourceType::ExponentialHistogram,
                // `TDigestFieldMapper.TDigestFieldType.fielddataBuilder` (aggregations come with ES|QL).
                "tdigest" => {
                    return Err(search_aggs::AggError::new(500, "unsupported_operation_exception", "The [tdigest] field does not support aggregations"));
                }
                _ => SourceType::Histogram,
            },
            Kind::IntegerRange => SourceType::Range(search_aggs::RangeType::Integer),
            Kind::LongRange => SourceType::Range(search_aggs::RangeType::Long),
            Kind::FloatRange => SourceType::Range(search_aggs::RangeType::Float),
            Kind::DoubleRange => SourceType::Range(search_aggs::RangeType::Double),
            Kind::DateRange => SourceType::Range(search_aggs::RangeType::Date),
            Kind::IpRange => SourceType::Range(search_aggs::RangeType::Ip),
            Kind::AggregateMetric => SourceType::AggregateMetric(search_aggs::histogram_field::AggregateMetricFields {
                min: f.subfield("min"),
                max: f.subfield("max"),
                sum: f.subfield("sum"),
                value_count: f.subfield("value_count"),
            }),
            Kind::Text | Kind::MatchOnlyText | Kind::SearchAsYouType => match f.subfield("fielddata") {
                Some(fd) => {
                    number = fd;
                    SourceType::Keyword
                }
                // `doc_values: true` keeps whole values, aggregated like a keyword.
                None if f.doc_values => SourceType::Keyword,
                // search-aggs reports the fielddata error when it binds a values source;
                // `significant_text` needs none.
                None if f.kind == Kind::Text || (f.kind == Kind::MatchOnlyText && crate::opensearch::active()) => SourceType::None,
                None => {
                    return Err(search_aggs::AggError::illegal_argument(fielddata_disabled(&f.name, &self.index)));
                }
            },
            _ => SourceType::None,
        };
        if !f.doc_values && !matches!(source, SourceType::None) && f.kind != Kind::ConstantKeyword && !f.kind.is_text() {
            return Err(search_aggs::AggError::illegal_argument(format!(
                "Can't load fielddata on [{}] because fielddata is unsupported on fields of type [{}]. Use doc values instead.",
                f.name, f.ty
            )));
        }
        let mut a = AggField::new(f.name.clone(), f.ty.clone(), source, number);
        a.date_format = f.mapper.raw("format").and_then(Value::as_str).map(str::to_string);
        a.nested_path = f.nested_path.clone();
        a.points = f.index && (f.kind.is_numeric() || f.kind.is_date());
        a.indexed = f.index && f.kind == Kind::Keyword && number == f.number;
        a.counter = f.metric.as_deref() == Some("counter");
        a.position = f.metric.as_deref() == Some("position") && crate::engine::tsdb::is_time_series(&self.idx);
        if f.kind == Kind::CountedKeyword {
            a.counts = f.subfield("count");
        }
        Ok(Some(a))
    }

    fn index_name(&self) -> &str {
        &self.index
    }

    fn query(&self, query: &Json) -> search_aggs::Result<QueryRef> {
        let v = from_json(query);
        let q = crate::query::parse_query(&v, None, &[]).map_err(|e| search_aggs::AggError::new(e.status, e.kind.to_string(), e.reason().to_string()))?.0;
        let ctx = QueryCtx::new(self.cluster, self.st, &self.idx, &self.table, &self.searcher, self.now);
        ctx.to_query(&q).map_err(|e| search_aggs::AggError::new(e.status, e.kind.to_string(), e.reason().to_string()))
    }

    /// `children` / `parent`: the child type's parent type, its `<join>#<parent>` field and the
    /// two type filters (`ChildrenAggregationBuilder.resolveConfig`).
    fn join(&self, child_type: Option<&str>, parent_type: Option<&str>) -> Option<search_aggs::field::JoinRelation> {
        let joiner = crate::join::Joiner::of(&self.idx.mapping)?;
        let (child, parent) = match (child_type, parent_type) {
            (Some(c), _) => (c.to_string(), joiner.parent_of(c)?.to_string()),
            (None, Some(p)) if joiner.is_parent(p) => (joiner.children(p).first()?.clone(), p.to_string()),
            _ => return None,
        };
        let f = self.table.get(&joiner.field)?;
        let join_field = f.subfield(&parent)?;
        let term = |t: &str| -> QueryRef { Arc::new(search_index::query::TermQuery::new(f.number, t.as_bytes().to_vec())) };
        Some(search_aggs::field::JoinRelation { join_field, parent_filter: term(&parent), child_filter: term(&child) })
    }

    fn global_query(&self) -> QueryRef {
        // `buildFilteredQuery(match_all)`: non-nested docs plus the search's alias filter.
        // OpenSearch builds it from a `match_all`, filtered to the root docs only when the
        // index has nested docs: the same docs, but its profile shows the query.
        let root = if crate::opensearch::active() {
            let all: QueryRef = Arc::new(search_index::query::MatchAllQuery);
            if self.table.objects.values().any(|o| o.nested) {
                Arc::new(search_index::query::BooleanQuery::new().add(search_index::query::Occur::Must, all).add(search_index::query::Occur::Filter, self.root_filter()))
            } else {
                all
            }
        } else if self.table.objects.values().any(|o| o.nested) {
            self.root_filter()
        } else {
            // `SearchExecutionContext.filterQuery` adds the non-nested filter only to indices
            // with nested fields.
            Arc::new(search_index::query::MatchAllQuery)
        };
        let Some(af) = &self.alias_filter else { return root };
        let ctx = QueryCtx::new(self.cluster, self.st, &self.idx, &self.table, &self.searcher, self.now);
        match ctx.to_filter(af) {
            Ok(f) => Arc::new(search_index::query::BooleanQuery::new().add(search_index::query::Occur::Filter, f).add(search_index::query::Occur::Filter, root)),
            Err(_) => root,
        }
    }

    fn nested(&self, path: &str) -> NestedLookup {
        match self.table.objects.get(path) {
            Some(o) if o.nested => {
                let parent_filter: QueryRef = match &o.parent_nested {
                    None => search_index::query::nested::root_filter(),
                    Some(p) => Arc::new(TermQuery::new(NESTED_PATH_FIELD, p.as_bytes().to_vec())),
                };
                NestedLookup::Nested(NestedPath { child_filter: Arc::new(TermQuery::new(NESTED_PATH_FIELD, path.as_bytes().to_vec())), parent_filter })
            }
            Some(_) => NestedLookup::NotNested,
            None => NestedLookup::NotFound,
        }
    }

    fn now_millis(&self) -> i64 {
        self.now
    }

    fn field_date_formatter(&self, field: &str, pattern: &str) -> search_aggs::Result<Arc<dyn search_aggs::DateFormatter>> {
        let locale = self.table.get(field).and_then(|f| f.date_format.as_ref().map(|d| d.locale));
        match locale {
            // search-aggs' printer knows English and `Locale.ROOT` texts; other locales print
            // through the mapping's formatter.
            Some(l @ crate::time::DateLocale::Cldr(_)) => match crate::time::DateFormatter::new(pattern) {
                Ok(f) => Ok(Arc::new(LocaleDates { pattern: pattern.to_string(), fmt: f.with_locale(l) })),
                Err(_) => self.date_formatter(pattern),
            },
            _ => self.date_formatter(pattern),
        }
    }

    fn date_formatter(&self, pattern: &str) -> search_aggs::Result<Arc<dyn search_aggs::DateFormatter>> {
        let printer = search_aggs::format::builtin_date_formatter(pattern)?;
        match crate::time::DateFormatter::new(pattern) {
            Ok(parser) => Ok(Arc::new(EsDates { printer, parser })),
            Err(_) => Ok(printer),
        }
    }

    fn fetcher(&self) -> Option<Arc<dyn search_aggs::TopHitsFetcher>> {
        Some(Arc::new(Fetcher {
            table: self.table.clone(),
            index: self.index.clone(),
            idx: self.idx.clone(),
            shard: self.shard.0,
            parent_fields: self.parent_fields.clone(),
            rescore: self.rescore.clone(),
            id_fielddata: super::id_fielddata::enabled(self.st),
            security: self.security.clone(),
        }))
    }

    fn top_metrics_max_size(&self) -> u32 {
        self.idx.settings.get("index.top_metrics_max_size").and_then(|v| v.parse().ok()).unwrap_or(10)
    }

    fn max_inner_result_window(&self) -> u32 {
        self.idx.settings.get("index.max_inner_result_window").and_then(|v| v.parse().ok()).unwrap_or(100)
    }

    fn hit_sort(&self, sort: &Json) -> search_aggs::Result<Vec<search_aggs::HitSort>> {
        use search_aggs::{HitSort, SortMissing, SortMode};
        let items: Vec<Json> = match sort {
            Json::Arr(a) => a.clone(),
            other => vec![other.clone()],
        };
        let mut out = Vec::new();
        for it in items {
            let (name, opts): (String, Option<Json>) = match &it {
                Json::Str(s) => (s.clone(), None),
                Json::Obj(o) if o.len() == 1 => (o[0].0.clone(), Some(o[0].1.clone())),
                _ => return Err(search_aggs::AggError::illegal_argument("malformed sort")),
            };
            let order = opts.as_ref().and_then(|o| match o {
                Json::Str(s) => Some(s.clone()),
                Json::Obj(_) => o.get("order").and_then(Json::as_str).map(str::to_string),
                _ => None,
            });
            let reverse_default = name == "_score";
            let reverse = match order.as_deref() {
                Some("desc") => true,
                Some("asc") => false,
                _ => reverse_default,
            };
            match name.as_str() {
                "_score" => out.push(HitSort::Score { reverse: !reverse }),
                "_doc" => out.push(HitSort::Doc { reverse }),
                "_script" | "_geo_distance" | "_geoDistance" => out.push(self.computed_sort(&it)?),
                field => {
                    if let Some(key) = self.runtime_sort(field, reverse)? {
                        out.push(key);
                        continue;
                    }
                    let Some(f) = self.field(field)? else {
                        return Err(search_aggs::AggError::new(400, "query_shard_exception", format!("No mapping found for [{field}] in order to sort on")));
                    };
                    if matches!(f.source, SourceType::None) && f.type_name == "text" {
                        return Err(search_aggs::AggError::illegal_argument(fielddata_disabled(&f.name, &self.index)));
                    }
                    let missing = match opts.as_ref().and_then(|o| o.get("missing")) {
                        Some(Json::Str(s)) if s == "_first" => SortMissing::First,
                        Some(Json::Str(s)) if s == "_last" => SortMissing::Last,
                        Some(v) => SortMissing::Value(v.clone()),
                        None => SortMissing::Last,
                    };
                    let mode = match opts.as_ref().and_then(|o| o.get("mode")).and_then(Json::as_str) {
                        Some("min") => Some(SortMode::Min),
                        Some("max") => Some(SortMode::Max),
                        Some("sum") => Some(SortMode::Sum),
                        Some("avg") => Some(SortMode::Avg),
                        Some("median") => Some(SortMode::Median),
                        _ => None,
                    };
                    // Inside a `nested` aggregation the hits are the nested docs themselves.
                    let nested = match opts.as_ref().and_then(|o| o.get("nested")) {
                        Some(n) => Some(self.nested_sort(n)?),
                        None => None,
                    };
                    let numeric_type = opts.as_ref().and_then(|o| o.get("numeric_type")).and_then(Json::as_str).and_then(search_aggs::SortNumericType::parse);
                    out.push(HitSort::Field { field: f, reverse, missing, mode, nested, numeric_type });
                }
            }
        }
        Ok(out)
    }

    fn scripts(&self) -> Option<Arc<dyn search_aggs::ScriptHost>> {
        Some(Arc::new(super::agg_scripts::Host::new(self.cluster, Some(self.table.clone()), self.shard.0 as i32, self.index.clone())))
    }

    fn time_series(&self) -> Option<search_aggs::TimeSeriesInfo> {
        if !crate::engine::tsdb::is_time_series(&self.idx) {
            return None;
        }
        let mut names: Vec<&String> = self.table.fields.iter().filter(|(n, f)| f.dimension && f.name == **n).map(|(n, _)| n).collect();
        names.sort();
        let mut dimensions = Vec::with_capacity(names.len());
        for n in names {
            if let Ok(Some(f)) = self.field(n) {
                dimensions.push(f);
            }
        }
        let timestamp_field = self.table.get("@timestamp").map(|f| f.number);
        Some(search_aggs::TimeSeriesInfo { dimensions, tsid_field: Some(crate::engine::fields::TSID_FIELD), timestamp_field })
    }

    fn doc_count_field(&self) -> Option<u32> {
        Some(crate::engine::fields::DOC_COUNT_FIELD)
    }

    fn downsampling_interval(&self) -> Option<String> {
        self.idx.settings.get("index.downsample.interval").filter(|s| !s.is_empty()).map(str::to_string)
    }

    fn text_terms(&self) -> Option<Arc<dyn search_aggs::TextTerms>> {
        Some(Arc::new(TextSource { table: self.table.clone() }))
    }

    fn shard_id(&self) -> (u32, u32) {
        self.shard
    }

    fn index_uuid(&self) -> Option<&str> {
        Some(&self.uuid)
    }
}

/// Dates for aggregations: printing by search-aggs' formatter, parsing (`date_range` and
/// `range` bounds, `extended_bounds`, composite `after` keys) by the Java-compatible parser the
/// mappings and queries use, with its date math and its exact errors.
#[derive(Debug)]
struct EsDates {
    printer: Arc<dyn search_aggs::DateFormatter>,
    parser: crate::time::DateFormatter,
}

fn es_zone(zone: &search_aggs::TimeZone) -> crate::time::Zone {
    match zone {
        search_aggs::TimeZone::Fixed { offset_secs, .. } => crate::time::Zone::Offset(*offset_secs),
        search_aggs::TimeZone::Named(tz) => crate::time::Zone::Named(*tz),
    }
}

/// A date format in a field's non-English `locale` (month and day names in its language).
#[derive(Debug)]
struct LocaleDates {
    pattern: String,
    fmt: crate::time::DateFormatter,
}

impl search_aggs::DateFormatter for LocaleDates {
    fn pattern(&self) -> &str {
        &self.pattern
    }

    fn format(&self, millis: i64, nanos: u32, zone: &search_aggs::TimeZone) -> String {
        let i = crate::time::Instant::from_millis(millis);
        self.fmt.format(crate::time::Instant { secs: i.secs, nanos: i.nanos + nanos }, &es_zone(zone))
    }

    fn parse(&self, text: &str, now_millis: i64, round_up: bool, zone: &search_aggs::TimeZone) -> search_aggs::Result<i64> {
        crate::time::parse_date_math(text, now_millis, round_up, Some(&es_zone(zone)), &self.fmt).map(|i| i.millis()).map_err(|e| to_agg_error(&e.to_es()))
    }
}

/// An [`EsError`] for search-aggs (causes kept).
fn to_agg_error(e: &EsError) -> search_aggs::AggError {
    let mut out = search_aggs::AggError::new(e.status, e.kind.to_string(), e.reason().to_string());
    if let Some(c) = &e.caused_by {
        out = out.caused_by(to_agg_error(c));
    }
    out
}

impl search_aggs::DateFormatter for EsDates {
    fn pattern(&self) -> &str {
        self.printer.pattern()
    }

    fn format(&self, millis: i64, nanos: u32, zone: &search_aggs::TimeZone) -> String {
        self.printer.format(millis, nanos, zone)
    }

    fn try_format(&self, millis: i64, nanos: u32, zone: &search_aggs::TimeZone) -> Result<String, String> {
        self.printer.try_format(millis, nanos, zone)
    }

    fn parse(&self, text: &str, now_millis: i64, round_up: bool, zone: &search_aggs::TimeZone) -> search_aggs::Result<i64> {
        let z = match zone {
            search_aggs::TimeZone::Fixed { offset_secs, .. } => crate::time::Zone::Offset(*offset_secs),
            search_aggs::TimeZone::Named(tz) => crate::time::Zone::Named(*tz),
        };
        crate::time::parse_date_math(text, now_millis, round_up, Some(&z), &self.parser).map(|i| i.millis()).map_err(|e| to_agg_error(&e.to_es()))
    }
}

/// `TextFieldMapper`'s fielddata error (OpenSearch's doesn't name the field and index first).
pub(crate) fn fielddata_disabled(field: &str, index: &str) -> String {
    fielddata_disabled_in(crate::opensearch::active(), field, index)
}

/// [`fielddata_disabled`] for a known dialect.
pub(crate) fn fielddata_disabled_in(opensearch: bool, field: &str, index: &str) -> String {
    let tail = format!(
        "Text fields are not optimised for operations that require per-document field data like aggregations and sorting, so these operations are disabled by default. Please use a keyword field instead. Alternatively, set fielddata=true on [{field}] in order to load field data by uninverting the inverted index. Note that this can use significant memory."
    );
    if opensearch { tail } else { format!("Fielddata is disabled on [{field}] in [{index}]. {tail}") }
}

/// `significant_text` and `categorize_text`: `_source` values re-analyzed with the field's
/// index analyzer (`SignificantTextAggregatorFactory` reads `_source`, never term vectors).
struct TextSource {
    table: Arc<FieldTable>,
}

impl TextSource {
    /// The index-time analyzer: text fields' analyzer, keyword-like fields' normalizer (or the
    /// keyword analyzer). `None` for fields without `TextSearchInfo`.
    fn analyzer(&self, field: &str) -> Option<search_analysis::Analyzer> {
        let f = self.table.get(field)?;
        if f.kind.is_text() {
            return f.analyzer.clone().or_else(|| self.table.analysis.analyzer("standard"));
        }
        if f.kind.is_keyword_like() {
            return match &f.normalizer {
                Some(n) => Some(n.analyzer().clone()),
                None => self.table.analysis.analyzer("keyword"),
            };
        }
        None
    }

    fn source(seg: &search_index::SegmentContext, doc: u32) -> search_aggs::Result<Option<Map>> {
        let err = |e: String| search_aggs::AggError::new(500, "exception", e);
        seg.ensure_stored().map_err(|e| err(e.to_string()))?;
        let stored = seg.reader.stored().document(doc).map_err(|e| err(e.to_string()))?;
        let Some(b) = crate::fetch::source_bytes(&stored) else { return Ok(None) };
        match crate::fetch::parse_source(b).map_err(|e| err(e.reason().to_string()))? {
            Value::Obj(m) => Ok(Some(m)),
            _ => Ok(None),
        }
    }
}

impl search_aggs::TextTerms for TextSource {
    fn analyzed(&self, field: &str) -> bool {
        self.analyzer(field).is_some()
    }

    fn tokens(&self, seg: &search_index::SegmentContext, doc: u32, field: &str, source_fields: &[String]) -> search_aggs::Result<Vec<Vec<Vec<u8>>>> {
        let mut chars = 0;
        self.tokens_counting(seg, doc, field, source_fields, &mut chars)
    }

    fn tokens_counting(&self, seg: &search_index::SegmentContext, doc: u32, field: &str, source_fields: &[String], chars: &mut u64) -> search_aggs::Result<Vec<Vec<Vec<u8>>>> {
        let Some(analyzer) = self.analyzer(field) else { return Ok(Vec::new()) };
        let Some(src) = Self::source(seg, doc)? else { return Ok(Vec::new()) };
        // Without explicit `source_fields`, `SearchExecutionContext.sourcePath`: a multi-field
        // reads its parent's values, a `copy_to` target its own and its sources'.
        let resolved: Vec<String>;
        let source_fields = if source_fields.len() == 1 && source_fields[0] == field {
            let base = self.table.get(field).and_then(|f| f.parent.clone()).unwrap_or_else(|| field.to_string());
            let mut v = Vec::new();
            if let Some(src) = self.table.copy_sources.get(&base) {
                v.extend(src.iter().cloned());
            }
            v.push(base);
            resolved = v;
            &resolved[..]
        } else {
            source_fields
        };
        let mut out = Vec::new();
        let mut raw = Vec::new();
        for sf in source_fields {
            raw.clear();
            crate::engine::source::extract_values(&src, sf, &mut raw);
            for v in &raw {
                let text = match v {
                    Value::Null => continue,
                    Value::Str(s) => std::borrow::Cow::Borrowed(s.as_str()),
                    other => std::borrow::Cow::Owned(other.to_es_string()),
                };
                *chars += text.encode_utf16().count() as u64;
                let mut terms = Vec::new();
                analyzer.analyze(&text, &mut |t| terms.push(t.term.as_bytes().to_vec()));
                out.push(terms);
            }
        }
        Ok(out)
    }

    fn doc_freq(&self, searcher: &Searcher, field: &str, term: &[u8]) -> search_aggs::Result<u64> {
        Ok(match self.table.get(field) {
            Some(f) => searcher.term_stats(f.number, term).doc_freq,
            None => 0,
        })
    }

    fn term_query(&self, field: &str, term: &[u8]) -> search_aggs::Result<QueryRef> {
        let number = self.table.get(field).map(|f| f.number).unwrap_or(u32::MAX);
        Ok(Arc::new(TermQuery::new(number, term.to_vec())))
    }

    fn source_strings(&self, seg: &search_index::SegmentContext, doc: u32, field: &str) -> search_aggs::Result<Vec<String>> {
        let Some(src) = Self::source(seg, doc)? else { return Ok(Vec::new()) };
        let mut raw = Vec::new();
        crate::engine::source::extract_values(&src, field, &mut raw);
        Ok(raw.into_iter().filter(|v| !v.is_null()).map(|v| v.to_es_string()).collect())
    }
}

/// `top_hits` fetch: hits rendered like search hits from the shard's searcher.
struct Fetcher {
    table: Arc<FieldTable>,
    index: String,
    idx: Arc<crate::meta::IndexMeta>,
    shard: u32,
    parent_fields: Arc<Vec<crate::search_source::FieldAndFormat>>,
    rescore: Option<Arc<Vec<super::PreparedRescore>>>,
    /// [`super::id_fielddata::enabled`] when the search started.
    id_fielddata: bool,
    security: Option<crate::xpack::security::dls::ReadSecurity>,
}

/// The hidden key a rendered `top_hits` hit carries to the coordinator, which adds what needs
/// the search's query (highlight, explanation, matched queries) and runtime fields; see
/// `search::finish_top_hits`. Never left in a response.
pub const TOP_HIT_MARKER: &str = "__roll_top_hit";

impl search_aggs::TopHitsFetcher for Fetcher {
    fn rescores(&self) -> bool {
        self.rescore.is_some()
    }

    fn rescore(&self, searcher: &Searcher, hits: &mut Vec<(u32, f32)>) -> search_aggs::Result<()> {
        let Some(r) = &self.rescore else { return Ok(()) };
        let shard_hits = hits.iter().map(|&(doc, score)| super::ShardHit { doc, score, sort: Vec::new(), group: None }).collect();
        let (out, _, _) = super::apply_rescore(searcher, shard_hits, r).map_err(|e| search_aggs::AggError::new(e.status, e.kind.to_string(), e.reason().to_string()))?;
        *hits = out.into_iter().map(|h| (h.doc, h.score)).collect();
        Ok(())
    }

    fn fetch(&self, searcher: &Searcher, hits: &[search_aggs::FetchHit], spec: &Json) -> search_aggs::Result<Vec<Json>> {
        let spec_v = from_json(spec);
        let get = |k: &str| spec_v.get(k);
        let fetch_err = |e: EsError| search_aggs::AggError::new(e.status, e.kind.to_string(), e.reason().to_string());
        let source = match get("_source") {
            Some(v) => Some(crate::fetch::FetchSource::from_value(v).map_err(fetch_err)?),
            None => None,
        };
        let list = |k: &str| -> Vec<crate::search_source::FieldAndFormat> {
            match get(k) {
                Some(Value::Arr(a)) => a
                    .iter()
                    .map(|x| match x {
                        Value::Obj(m) => crate::search_source::FieldAndFormat {
                            field: m.get("field").and_then(Value::as_str).unwrap_or_default().to_string(),
                            format: m.get("format").and_then(Value::as_str).map(str::to_string),
                            include_unmapped: None,
                        },
                        other => crate::search_source::FieldAndFormat { field: other.to_es_string(), format: None, include_unmapped: None },
                    })
                    .collect(),
                Some(Value::Str(s)) => vec![crate::search_source::FieldAndFormat { field: s.clone(), format: None, include_unmapped: None }],
                _ => Vec::new(),
            }
        };
        let stored_fields = match get("stored_fields") {
            Some(Value::Arr(a)) => Some(Some(a.iter().map(|v| v.to_es_string()).collect::<Vec<_>>())),
            Some(Value::Str(s)) if s == "_none_" => Some(None),
            Some(Value::Str(s)) => Some(Some(vec![s.clone()])),
            _ => None,
        };
        let own_fields = get("fields").is_some();
        let opts = super::hits::FetchOpts {
            source,
            stored_fields,
            docvalue_fields: list("docvalue_fields"),
            fields: if own_fields { list("fields") } else { self.parent_fields.to_vec() },
            version: get("version").and_then(Value::as_bool).unwrap_or(false),
            seq_no_primary_term: get("seq_no_primary_term").and_then(Value::as_bool).unwrap_or(false),
            force_synthetic: false,
            id_fielddata: self.id_fielddata,
            script_fields_only: get("script_fields").and_then(Value::as_object).is_some_and(|m| !m.is_empty()),
            raw_source: false,
            keep_id: false,
            security: self.security.clone(),
        };
        let ie = |e| fetch_err(crate::engine::convert::index_error(e));
        let mut marker = Map::new();
        marker.insert("index".into(), Value::Str(self.index.clone()));
        marker.insert("shard".into(), Value::from(self.shard as i64));
        if let Some(h) = get("highlight") {
            marker.insert("highlight".into(), h.clone());
        }
        if get("explain").and_then(Value::as_bool) == Some(true) {
            marker.insert("explain".into(), Value::Bool(true));
        }
        if let Some(sf @ Value::Obj(_)) = get("script_fields") {
            marker.insert("script_fields".into(), sf.clone());
        }
        if !own_fields {
            marker.insert("parent_fields".into(), Value::Bool(true));
        }
        let mut out = Vec::with_capacity(hits.len());
        for h in hits {
            let mut m = Map::new();
            m.insert("_index".into(), Value::Str(self.index.clone()));
            let score = if h.score.is_nan() { None } else { Some(h.score) };
            let nested = searcher.segment_of(h.doc).and_then(|(seg, local)| super::inner::nested_identity(&self.table, seg, local).map(|n| (seg.base, n)));
            match nested {
                Some((base, n)) => {
                    // A nested document: the root's `_id`, its `_nested` identity, and its slice
                    // of the root's `_source`.
                    let root = base + n.root;
                    let stored = searcher.stored(root).map_err(ie)?;
                    m.insert("_id".into(), Value::Str(super::hits::stored_id(&stored).unwrap_or_default()));
                    m.insert("_nested".into(), n.to_value());
                    m.insert("_score".into(), score.map(|s| Value::Num(Number::Float(crate::fetch::java_float_as_double(s)))).unwrap_or(Value::Null));
                    let fs = super::hits::wants_source(&opts);
                    if fs.fetch
                        && let Some(b) = crate::fetch::source_bytes(&stored)
                    {
                        // Source filters apply to the root document, by full paths, before
                        // the nested object is cut out of it.
                        let src = match (fs.filter(), crate::fetch::parse_source(b).map_err(fetch_err)?) {
                            (Some(f), Value::Obj(om)) => Value::Obj(f.apply(&om)),
                            (_, v) => v,
                        };
                        if let Some(v) = n.source_of(&src) {
                            m.insert("_source".into(), v);
                        }
                    }
                }
                None => {
                    let stored = searcher.stored(h.doc).map_err(ie)?;
                    // `stored_fields: _none_` loads no `_id`, unless Elasticsearch's script
                    // fields phase asks for it.
                    if opts.stored_fields != Some(None) || (opts.script_fields_only && !crate::opensearch::active()) {
                        m.insert("_id".into(), Value::Str(super::hits::stored_id(&stored).unwrap_or_default()));
                    }
                    super::hits::render_doc_stored(&self.table, Some(&self.idx), searcher, h.doc, &stored, &opts, &mut m, score, 0).map_err(fetch_err)?;
                }
            }
            if !h.sort.is_empty() {
                m.insert("sort".into(), Value::Arr(h.sort.iter().map(from_json).collect()));
            }
            let mut mk = marker.clone();
            mk.insert("doc".into(), Value::from(h.doc as i64));
            if let Some(s) = score {
                mk.insert("score".into(), Value::Num(Number::Float(s as f64)));
            }
            m.insert(TOP_HIT_MARKER.into(), Value::Obj(mk));
            out.push(to_json(&Value::Obj(m)));
        }
        Ok(out)
    }
}
