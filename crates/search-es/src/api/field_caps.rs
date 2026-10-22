//! `GET|POST [<index>]/_field_caps`: the capabilities of fields across indices, from their
//! mappings (`FieldCapabilitiesFetcher` per index, `FieldCapabilitiesResponse` merging).
//!
//! Per index, every field is reported with the type family Elasticsearch reports
//! (`constant_keyword` and `wildcard` are `keyword`, `match_only_text` is `text`, …), whether it
//! is searchable (indexed, or queryable through doc values) and aggregatable (doc values, or
//! field data), its `meta` and its time series role; objects and the metadata fields are
//! included, and the parent objects of every matched field are added (unless `-parent`). Then
//! the indices merge per field and type: a type found in only some indices lists them, and
//! disagreements are listed as `non_searchable_indices` and the like.
//!
//! `include_empty_fields=false` keeps only fields with values; until the engine reports field
//! presence no field has any.
//!
//! Fields come out in the bucket order of the `HashMap` Elasticsearch collects them in; fields
//! sharing a bucket may come out in a different order than Elasticsearch's, whose tie order
//! follows several intermediate hash collections.

use crate::error::{EsError, EsResult};
use crate::json::locate::{Target, locate};
use crate::json::{Map, Value};
use crate::mapping::{FieldMapper, Mapping, MappingCtx, MapperRef};
use crate::meta::IndexMeta;
use crate::meta::index_mode::IndexMode;
use crate::meta::resolve::IndicesOptions;
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xcontent::filter::glob_match;
use std::collections::{BTreeMap, BTreeSet};

/// One field's capabilities in one index.
#[derive(Clone, Debug, PartialEq)]
struct Cap {
    ty: String,
    metadata: bool,
    searchable: bool,
    aggregatable: bool,
    /// An object or nested parent (removed by `-parent`).
    parent: bool,
    /// Inside a nested object (removed by `-nested`).
    nested: bool,
    /// A multi-field (removed by `-multifield`).
    multi: bool,
    dimension: bool,
    metric: Option<String>,
    meta: BTreeMap<String, String>,
}

impl Cap {
    fn new(ty: &str, searchable: bool, aggregatable: bool) -> Cap {
        Cap {
            ty: ty.to_string(),
            metadata: false,
            searchable,
            aggregatable,
            parent: false,
            nested: false,
            multi: false,
            dimension: false,
            metric: None,
            meta: BTreeMap::new(),
        }
    }
}

/// Metadata fields every index reports: name, type, searchable, aggregatable.
const METADATA: &[(&str, &str, bool, bool)] = &[
    ("_ignored_source", "_ignored_source", false, false),
    ("_feature", "_feature", false, false),
    ("_ignored", "_ignored", true, true),
    ("_version", "_version", false, true),
    ("_routing", "_routing", true, false),
    ("_doc_count", "integer", false, false),
    ("_inference_fields", "_inference_fields", false, false),
    ("_tier", "keyword", true, true),
    ("_seq_no", "_seq_no", true, true),
    ("_source", "_source", false, false),
    ("_id", "_id", true, false),
    ("_index", "_index", true, true),
    ("_data_stream_timestamp", "_data_stream_timestamp", false, false),
    ("_index_mode", "_index_mode", true, true),
    ("_nested_path", "_nested_path", true, false),
    ("_field_names", "_field_names", true, false),
];

/// The family type field caps report for a mapper type.
fn family(ty: &str) -> &str {
    match ty {
        "constant_keyword" | "wildcard" => "keyword",
        "match_only_text" | "pattern_text" => "text",
        "token_count" => "integer",
        other => other,
    }
}

/// Searchable and aggregatable for a concrete field.
fn flags(f: &FieldMapper, ctx: &MappingCtx) -> (bool, bool) {
    let index = |d: bool| if f.def().param_index("index").is_some() { f.bool_param("index", ctx) } else { d };
    let dv = |d: bool| if f.def().param_index("doc_values").is_some() { f.bool_param("doc_values", ctx) } else { d };
    match f.ty {
        "text" | "annotated_text" => (index(true), f.bool_param("fielddata", ctx)),
        "match_only_text" | "search_as_you_type" | "completion" | "rank_feature" | "rank_features" | "sparse_vector" => (true, false),
        "pattern_text" => (true, false),
        "dense_vector" => (index(true), false),
        "binary" => (false, dv(false)),
        "percolator" => (false, false),
        "histogram" => (false, true),
        "constant_keyword" | "wildcard" | "flattened" | "join" | "aggregate_metric_double" | "counted_keyword" | "shape" | "point" | "geo_shape"
        | "version" => (true, true),
        _ => {
            let (i, d) = (index(true), dv(true));
            // Elasticsearch queries unindexed fields through their doc values; OpenSearch
            // reports only indexed fields as searchable.
            let searchable = if ctx.dialect == crate::cluster::Dialect::OpenSearch { i } else { i || d };
            (searchable, d)
        }
    }
}

/// Every field of one index with its capabilities, in mapping path order.
fn index_fields(idx: &IndexMeta, runtime_request: &Map) -> Vec<(String, Cap)> {
    let mode = IndexMode::of(&idx.settings);
    let ctx = idx.mapping_ctx();
    let mapping: &Mapping = &idx.mapping;
    let mut out: Vec<(String, Cap)> = Vec::new();
    for (name, ty, s, a) in METADATA {
        // `_routing` and `_id` aggregate when kept in doc values.
        let a = match *name {
            "_routing" => mapping.routing_doc_values.unwrap_or(ctx.columnar.is_some()),
            "_id" => mapping.id_mode.as_deref() == Some("columnar") || (mapping.id_mode.is_none() && ctx.columnar.is_some()),
            _ => *a,
        };
        let mut c = Cap::new(ty, *s, a);
        c.metadata = true;
        out.push((name.to_string(), c));
    }
    if mode == IndexMode::TimeSeries {
        for (name, s, a) in [("_tsid", false, true), ("_ts_routing_hash", true, true)] {
            let mut c = Cap::new(name, s, a);
            c.metadata = true;
            out.push((name.to_string(), c));
        }
    }
    let paths = mapping.field_paths();
    let nested_paths: Vec<&String> = paths.iter().filter(|(_, m)| matches!(m, MapperRef::Object(o) if o.is_nested())).map(|(p, _)| p).collect();
    let in_nested = |p: &str| nested_paths.iter().any(|n| p.starts_with(&format!("{n}.")));
    let disabled: Vec<&String> = paths.iter().filter(|(_, m)| matches!(m, MapperRef::Object(o) if !o.is_enabled())).map(|(p, _)| p).collect();
    for (path, m) in &paths {
        if disabled.iter().any(|d| path == *d || path.starts_with(&format!("{d}."))) {
            continue;
        }
        let multi = path.rsplit_once('.').is_some_and(|(parent, _)| matches!(mapping.lookup(parent), Some(MapperRef::Field(_))));
        let mut cap = match m {
            MapperRef::Object(o) => {
                let mut c = Cap::new(if o.is_nested() { "nested" } else { "object" }, false, false);
                c.parent = true;
                c
            }
            MapperRef::Alias(a) => match mapping.lookup(&a.path) {
                Some(MapperRef::Field(f)) => {
                    let (s, ag) = flags(f, &ctx);
                    Cap::new(family(f.ty), s, ag)
                }
                _ => continue,
            },
            MapperRef::Field(f) => {
                let (s, ag) = flags(f, &ctx);
                let mut c = Cap::new(family(f.ty), s, ag);
                if let Some(Value::Obj(meta)) = f.raw("meta") {
                    for (k, v) in meta {
                        c.meta.insert(k.clone(), v.to_es_string());
                    }
                }
                if mode == IndexMode::TimeSeries {
                    c.dimension = f.bool_param("time_series_dimension", &ctx);
                    c.metric = f.raw("time_series_metric").and_then(Value::as_str).map(str::to_string);
                }
                c
            }
        };
        // `-nested` drops nested objects and everything inside them.
        cap.nested = in_nested(path) || matches!(m, MapperRef::Object(o) if o.is_nested());
        cap.multi = multi;
        out.push((path.clone(), cap.clone()));
        // Sub-fields some types add.
        if let MapperRef::Field(f) = m {
            let sub = |name: &str, ty: &str, s: bool, a: bool| {
                let mut c = Cap::new(ty, s, a);
                c.nested = cap.nested;
                (format!("{path}{name}"), c)
            };
            match f.ty {
                "search_as_you_type" => {
                    let max = f.raw("max_shingle_size").and_then(Value::as_i64).unwrap_or(3);
                    for n in 2..=max {
                        out.push(sub(&format!("._{n}gram"), "search_as_you_type", true, false));
                    }
                    out.push(sub("._index_prefix", "prefix", true, false));
                }
                "percolator" => {
                    out.push(sub(".extracted_terms", "keyword", true, false));
                    out.push(sub(".extraction_result", "keyword", true, false));
                    out.push(sub(".minimum_should_match_field", "integer", true, true));
                    out.push(sub(".query_builder_field", "binary", true, true));
                    out.push(sub(".range_field", "ip_range", true, false));
                }
                "pattern_text" => out.push(sub(".template_id", "keyword", true, true)),
                "counted_keyword" => out.push((format!("{path}_count"), Cap::new("binary", true, true))),
                "join" => {
                    if let Some(Value::Obj(rel)) = f.raw("relations") {
                        for parent in rel.keys() {
                            out.push((format!("{path}#{parent}"), Cap::new("parent", true, true)));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    // Runtime fields (the request's win over the mapping's) shadow mapped ones.
    let mut runtime: Vec<(String, String, Option<Map>)> = Vec::new();
    for (name, rf) in &mapping.runtime {
        runtime.push((name.clone(), rf.ty.clone(), rf.params.get("fields").and_then(Value::as_object).cloned()));
    }
    for (name, def) in runtime_request {
        let ty = def.get("type").map(Value::to_es_string).unwrap_or_default();
        runtime.retain(|(n, _, _)| n != name);
        runtime.push((name.clone(), ty, def.get("fields").and_then(Value::as_object).cloned()));
    }
    for (name, ty, fields) in runtime {
        out.retain(|(p, _)| *p != name);
        if ty == "composite" {
            let mut c = Cap::new("object", false, false);
            c.parent = true;
            out.push((name.clone(), c));
            for (sub, def) in fields.unwrap_or_default() {
                let sty = def.get("type").map(Value::to_es_string).unwrap_or_default();
                let path = format!("{name}.{sub}");
                out.retain(|(p, _)| *p != path);
                out.push((path, Cap::new(&sty, true, true)));
            }
        } else {
            out.push((name, Cap::new(&ty, true, true)));
        }
    }
    out
}

/// The `filters` parameter.
#[derive(Default)]
struct Filters {
    only_metadata: bool,
    no_metadata: bool,
    no_nested: bool,
    no_multifield: bool,
    no_parent: bool,
}

fn parse_filters(raw: Option<&str>) -> EsResult<Filters> {
    let mut f = Filters::default();
    for item in raw.unwrap_or("").split(',').filter(|s| !s.is_empty()) {
        match item {
            "+metadata" => f.only_metadata = true,
            "-metadata" => f.no_metadata = true,
            "-nested" => f.no_nested = true,
            "-multifield" => f.no_multifield = true,
            "-parent" => f.no_parent = true,
            other => return Err(EsError::illegal_argument(format!("Unknown field caps filter [{other}]"))),
        }
    }
    Ok(f)
}

/// The fields of one index the request selects, with their parents.
fn select(fields: Vec<(String, Cap)>, patterns: &[String], filters: &Filters, types: &[String]) -> Vec<(String, Cap)> {
    let matches = |name: &str| patterns.iter().any(|p| p == "*" || glob_match(p, name));
    let kept = |c: &Cap| {
        !(filters.only_metadata && !c.metadata)
            && !(filters.no_metadata && c.metadata)
            && !(filters.no_nested && c.nested)
            && !(filters.no_multifield && c.multi)
            && (types.is_empty() || types.contains(&c.ty))
    };
    let lookup: BTreeMap<&str, &Cap> = fields.iter().map(|(n, c)| (n.as_str(), c)).collect();
    let mut out: Vec<(String, Cap)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (name, cap) in &fields {
        if !matches(name) || !kept(cap) || (filters.no_parent && cap.parent) {
            continue;
        }
        if seen.insert(name.clone()) {
            out.push((name.clone(), cap.clone()));
        }
        if filters.no_parent || cap.metadata {
            continue;
        }
        // Parent objects of a matched field are reported too.
        let mut p = name.as_str();
        while let Some((parent, _)) = p.rsplit_once('.') {
            if let Some(pc) = lookup.get(parent).filter(|c| c.parent && kept(c))
                && seen.insert(parent.to_string())
            {
                out.push((parent.to_string(), (*pc).clone()));
            }
            p = parent;
        }
    }
    out
}

/// Whether an index can match `index_filter` (the coordinating `can_match` check): queries on
/// `_index`, `_index_mode` and `_tier` are decided from metadata, a query on a field the index
/// doesn't map matches nothing, and anything else may match (deciding it needs the data).
fn can_match(q: &Value, idx: &IndexMeta) -> bool {
    let Some((kind, body)) = q.as_object().and_then(|m| m.iter().next()) else { return true };
    let meta_value = |field: &str| -> Option<String> {
        match field {
            "_index" => Some(idx.name.clone()),
            "_index_mode" => Some(IndexMode::of(&idx.settings).name().to_string()),
            "_tier" => Some(
                idx.settings
                    .get("index.routing.allocation.include._tier_preference")
                    .and_then(|t| t.split(',').next())
                    .unwrap_or("data_content")
                    .to_string(),
            ),
            _ => None,
        }
    };
    let field_and_value = || -> Option<(String, Value)> {
        let (f, v) = body.as_object()?.iter().next()?;
        let v = match v {
            Value::Obj(m) => m.get("value").or_else(|| m.get("query")).cloned().unwrap_or(Value::Null),
            other => other.clone(),
        };
        Some((f.clone(), v))
    };
    let mapped = |f: &str| f.starts_with('_') || idx.mapping.lookup(f).is_some() || idx.mapping.runtime.contains_key(f);
    match kind.as_str() {
        "match_all" => true,
        "match_none" => false,
        "bool" => {
            let list = |k: &str| -> Vec<Value> {
                match body.get(k) {
                    Some(Value::Arr(a)) => a.clone(),
                    Some(v @ Value::Obj(_)) => vec![v.clone()],
                    _ => Vec::new(),
                }
            };
            let must = list("must").into_iter().chain(list("filter")).all(|c| can_match(&c, idx));
            let should = list("should");
            let min_should = body.get("minimum_should_match").and_then(Value::as_i64).unwrap_or(if list("must").is_empty() && list("filter").is_empty() { 1 } else { 0 });
            let should_ok = should.is_empty() || min_should == 0 || should.iter().filter(|c| can_match(c, idx)).count() as i64 >= min_should;
            // A must_not clause can only rule an index out when it surely matches everything.
            let must_not_all = list("must_not").iter().any(|c| matches!(c.as_object().and_then(|m| m.keys().next()).map(String::as_str), Some("match_all")));
            must && should_ok && !must_not_all
        }
        "term" | "match" | "match_phrase" => match field_and_value() {
            Some((f, v)) => match meta_value(&f) {
                Some(actual) => actual == v.to_es_string(),
                None => mapped(&f),
            },
            None => true,
        },
        "terms" => match body.as_object().and_then(|m| m.iter().find(|(k, _)| k.as_str() != "boost")) {
            Some((f, Value::Arr(vals))) => match meta_value(f) {
                Some(actual) => vals.iter().any(|v| v.to_es_string() == actual),
                None => mapped(f),
            },
            _ => true,
        },
        "prefix" | "wildcard" => match field_and_value() {
            Some((f, v)) => match meta_value(&f) {
                Some(actual) => {
                    let pattern = v.to_es_string();
                    if kind == "prefix" { actual.starts_with(&pattern) } else { glob_match(&pattern, &actual) }
                }
                None => mapped(&f),
            },
            None => true,
        },
        "exists" => body.get("field").and_then(Value::as_str).is_none_or(mapped),
        "range" => body.as_object().and_then(|m| m.keys().next()).is_none_or(|f| mapped(f)),
        _ => true,
    }
}

/// `Maps.newMapWithExpectedSize(n)`: a `HashMap` whose table fits `n` entries.
fn hash_map_for(m: Map) -> Map {
    let n = m.len();
    crate::json::java_hash_map(m, (n as f64 / 0.75) as usize + 1)
}

pub fn field_caps(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body_map()?;
    let mut patterns: Vec<String> = Vec::new();
    let mut runtime_request = Map::new();
    let mut index_filter: Option<Value> = None;
    if let Some(b) = &body {
        for (k, v) in b {
            match k.as_str() {
                "fields" => match v {
                    Value::Arr(a) => patterns.extend(a.iter().map(Value::to_es_string)),
                    other => patterns.extend(other.to_es_string().split(',').map(str::to_string)),
                },
                "runtime_mappings" => {
                    if let Value::Obj(m) = v {
                        runtime_request = m.clone();
                    }
                }
                "index_filter" => index_filter = Some(v.clone()),
                other => {
                    let (l, c) = ctx.raw_json().and_then(|r| locate(r, &[crate::json::locate::Seg::Key(other)], Target::Name)).unwrap_or((1, 2));
                    return Err(EsError::x_content_parse(l, c, format!("[field_caps_request] unknown field [{other}]")));
                }
            }
        }
    }
    if let Some(q) = ctx.query("fields") {
        if body.as_ref().is_some_and(|b| b.contains_key("fields")) {
            return Err(EsError::illegal_argument(
                "can't specify a request body and [fields] request parameter, either specify a request body or the [fields] request parameter",
            ));
        }
        patterns.extend(q.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }
    if patterns.is_empty() {
        return Err(EsError::action_request_validation(&["no fields specified".into()]));
    }
    let filters = parse_filters(ctx.query("filters"))?;
    let types: Vec<String> = ctx.query("types").map(|t| t.split(',').map(str::to_string).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    let include_unmapped = ctx.bool_param("include_unmapped", false)?;
    let include_empty = ctx.bool_param("include_empty_fields", true)?;
    let names = super::resolve_indices(ctx, IndicesOptions::STRICT_EXPAND_OPEN)?;
    let st = ctx.state();
    let indices: Vec<&IndexMeta> = names
        .iter()
        .filter_map(|n| st.metadata.indices.get(n).map(|i| i.as_ref()))
        .filter(|i| index_filter.as_ref().is_none_or(|f| can_match(f, i)))
        .collect();
    // field -> type -> (index, cap)
    let mut merged: BTreeMap<String, BTreeMap<String, Vec<(String, Cap)>>> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for idx in &indices {
        let fields = select(index_fields(idx, &runtime_request), &patterns, &filters, &types);
        for (name, cap) in fields {
            // No field has values until the engine says otherwise.
            if !include_empty && !cap.metadata {
                continue;
            }
            if !merged.contains_key(&name) {
                order.push(name.clone());
            }
            merged.entry(name).or_default().entry(cap.ty.clone()).or_default().push((idx.name.clone(), cap));
        }
    }
    let mut index_names: Vec<String> = indices.iter().map(|i| i.name.clone()).collect();
    index_names.sort();
    let mut fields = Map::new();
    for name in order {
        let by_type = &merged[&name];
        let present: BTreeSet<&String> = by_type.values().flatten().map(|(i, _)| i).collect();
        let unmapped: Vec<String> = if include_unmapped { index_names.iter().filter(|i| !present.contains(i)).cloned().collect() } else { Vec::new() };
        let multiple = by_type.len() > 1 || !unmapped.is_empty();
        let mut types_out = Map::new();
        for (ty, entries) in by_type {
            let mut list: Vec<String> = entries.iter().map(|(i, _)| i.clone()).collect();
            list.sort();
            let all = |f: fn(&Cap) -> bool| entries.iter().all(|(_, c)| f(c));
            let searchable = all(|c| c.searchable);
            let aggregatable = all(|c| c.aggregatable);
            let not = |f: fn(&Cap) -> bool| -> Vec<String> {
                let mut v: Vec<String> = entries.iter().filter(|(_, c)| !f(c)).map(|(i, _)| i.clone()).collect();
                v.sort();
                v
            };
            let mut m = Map::new();
            m.insert("type".into(), Value::from(ty.clone()));
            m.insert("metadata_field".into(), all(|c| c.metadata).into());
            m.insert("searchable".into(), searchable.into());
            m.insert("aggregatable".into(), aggregatable.into());
            m.insert("inference".into(), false.into());
            let dims = entries.iter().filter(|(_, c)| c.dimension).count();
            if dims == entries.len() {
                m.insert("time_series_dimension".into(), true.into());
            }
            let metrics: BTreeSet<&Option<String>> = entries.iter().map(|(_, c)| &c.metric).collect();
            if metrics.len() == 1
                && let Some(Some(metric)) = metrics.iter().next()
            {
                m.insert("time_series_metric".into(), Value::from(metric.clone()));
            }
            if multiple {
                m.insert("indices".into(), Value::from(list.clone()));
            }
            if !searchable && entries.iter().any(|(_, c)| c.searchable) {
                m.insert("non_searchable_indices".into(), Value::from(not(|c| c.searchable)));
            }
            if !aggregatable && entries.iter().any(|(_, c)| c.aggregatable) {
                m.insert("non_aggregatable_indices".into(), Value::from(not(|c| c.aggregatable)));
            }
            if dims > 0 && dims < entries.len() {
                m.insert("non_dimension_indices".into(), Value::from(not(|c| c.dimension)));
            }
            if metrics.len() > 1 {
                m.insert("metric_conflicts_indices".into(), Value::from(list.clone()));
            }
            let mut meta: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for (_, c) in entries {
                for (k, v) in &c.meta {
                    meta.entry(k.clone()).or_default().insert(v.clone());
                }
            }
            if !meta.is_empty() {
                let mut mm = Map::new();
                for (k, vs) in meta {
                    mm.insert(k, Value::from(vs.into_iter().collect::<Vec<_>>()));
                }
                m.insert("meta".into(), Value::Obj(mm));
            }
            types_out.insert(ty.clone(), Value::Obj(m));
        }
        if !unmapped.is_empty() {
            types_out.insert(
                "unmapped".into(),
                obj! {
                    "type" => "unmapped", "metadata_field" => false, "searchable" => false, "aggregatable" => false,
                    "inference" => false, "indices" => unmapped,
                },
            );
        }
        fields.insert(name, Value::Obj(hash_map_for(types_out)));
    }
    Reply::ok(obj! { "indices" => index_names, "fields" => Value::Obj(hash_map_for(fields)) })
}
