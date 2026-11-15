//! What a transform writes: the composite search a checkpoint pages through, the documents
//! it makes of the buckets, and the destination mappings Elasticsearch deduces.

use super::*;

/// The type of an aggregation body (`{"avg": {...}, "aggs": {...}}`).
pub(super) fn agg_kind(a: &Value) -> Option<&str> {
    a.as_object()?.keys().map(String::as_str).find(|k| !matches!(*k, "aggs" | "aggregations" | "meta"))
}

/// The search of a transform's sources: missing indices are skipped (an unattended transform
/// or a wildcard matching nothing reads no documents rather than failing).
pub(super) fn search_uri(t: &Value) -> String {
    format!("/{}/_search?ignore_unavailable=true&allow_no_indices=true", enc(&source_indices(t).join(",")))
}

/// The field types of the source indices (`field_caps`), by field name.
pub(super) async fn field_types(cluster: &Cluster, t: &Value, fields: &[String]) -> Map {
    if fields.is_empty() {
        return Map::new();
    }
    let uri = format!("/{}/_field_caps?fields={}&ignore_unavailable=true&allow_no_indices=true", enc(&source_indices(t).join(",")), enc(&fields.join(",")));
    // The source's runtime fields count as fields of the source.
    let body = t.pointer(&["source", "runtime_mappings"]).map(|r| obj! { "runtime_mappings" => r.clone() });
    let Ok(caps) = internal::call(cluster, "POST", &uri, body.as_ref()).await else { return Map::new() };
    let mut out = Map::new();
    if let Some(Value::Obj(f)) = caps.get("fields") {
        for (name, types) in f {
            if let Some(t) = types.as_object().and_then(|t| t.keys().next()) {
                out.insert(name.clone(), Value::from(t.as_str()));
            }
        }
    }
    out
}

/// The single field an aggregation reads (`field` of its body).
pub(super) fn agg_field(agg: &Value) -> Option<(String, String)> {
    let (kind, body) = agg.as_object()?.iter().find(|(k, _)| k.as_str() != "aggs" && k.as_str() != "aggregations" && k.as_str() != "meta")?;
    let field = match kind.as_str() {
        "top_metrics" => body.pointer(&["metrics", "field"]).or_else(|| body.get("metrics").and_then(Value::as_array).and_then(|a| a.first()).and_then(|m| m.get("field")))?,
        _ => body.get("field")?,
    };
    Some((kind.clone(), field.to_es_string()))
}

/// Elasticsearch's target mapping of an aggregation's output (`TransformAggregations`).
pub(super) fn agg_target(kind: &str, source_type: Option<&str>) -> Option<&'static str> {
    Some(match kind {
        "avg" | "median_absolute_deviation" | "sum" | "weighted_avg" | "percentiles" | "stats" | "boxplot" | "extended_stats" => "double",
        "cardinality" | "value_count" | "range" | "filter" | "missing" => "long",
        "geo_bounds" | "geo_line" => "geo_shape",
        "geo_centroid" => "geo_point",
        "terms" | "rare_terms" => "flattened",
        "max" | "min" | "top_metrics" => match source_type? {
            "scaled_float" => "float",
            "aggregate_metric_double" => "double",
            t => leak(t),
        },
        _ => return None,
    })
}

/// Field types are a small closed set; interned to `'static`.
pub(super) fn leak(t: &str) -> &'static str {
    const KNOWN: &[&str] = &[
        "long", "integer", "short", "byte", "double", "float", "half_float", "unsigned_long", "date", "date_nanos", "keyword", "constant_keyword", "wildcard", "ip", "boolean",
        "version", "text", "match_only_text",
    ];
    KNOWN.iter().find(|k| **k == t).copied().unwrap_or("double")
}

/// Mappings of the destination index Elasticsearch deduces for a pivot.
pub(super) async fn deduced_mappings(cluster: &Cluster, t: &Value) -> Map {
    let mut props = Map::new();
    let Some(pivot) = t.get("pivot") else { return props };
    let mut fields = Vec::new();
    if let Some(Value::Obj(groups)) = pivot.get("group_by") {
        for g in groups.values() {
            if let Some(Value::Obj(inner)) = g.as_object().and_then(|m| m.values().next())
                && let Some(f) = inner.get("field")
            {
                fields.push(f.to_es_string());
            }
        }
    }
    let aggs = pivot.get("aggregations").or_else(|| pivot.get("aggs")).and_then(Value::as_object).cloned().unwrap_or_default();
    for a in aggs.values() {
        if let Some((_, f)) = agg_field(a) {
            fields.push(f);
        }
    }
    let types = field_types(cluster, t, &fields).await;
    if let Some(Value::Obj(groups)) = pivot.get("group_by") {
        for (name, g) in groups {
            let Some((kind, inner)) = g.as_object().and_then(|m| m.iter().next()) else { continue };
            let field_type = inner.get("field").and_then(|f| types.get(&f.to_es_string())).map(Value::to_es_string);
            let ty = match kind.as_str() {
                "date_histogram" => Some(if field_type.as_deref() == Some("date_nanos") { "date_nanos" } else { "date" }.to_string()),
                "histogram" => field_type.or_else(|| Some("double".to_string())),
                "geotile_grid" => Some("geo_shape".to_string()),
                _ => field_type,
            };
            if let Some(ty) = ty {
                props.insert(name.clone(), obj! { "type" => ty });
            }
        }
    }
    for (name, a) in &aggs {
        let (kind, ty) = match agg_field(a) {
            Some((kind, f)) => {
                let st = types.get(&f).map(Value::to_es_string);
                (kind.clone(), agg_target(&kind, st.as_deref()))
            }
            None => {
                let kind = a.as_object().and_then(|m| m.keys().find(|k| k.as_str() != "aggs" && k.as_str() != "aggregations")).cloned().unwrap_or_default();
                let ty = agg_target(&kind, None);
                (kind, ty)
            }
        };
        // Object outputs map as an object plus a double per leaf, named like the output keys
        // (`TransformAggregations.getAggregationOutputTypes`).
        let leaves: Vec<String> = match kind.as_str() {
            "percentiles" => percents_of(a).into_iter().map(percent_key).collect(),
            "stats" => STATS_LEAVES.iter().map(|l| l.to_string()).collect(),
            "extended_stats" => STATS_LEAVES.iter().chain(EXTENDED_STATS_LEAVES).map(|l| l.to_string()).collect(),
            "boxplot" => BOXPLOT_LEAVES.iter().map(|l| l.to_string()).collect(),
            _ => Vec::new(),
        };
        if !leaves.is_empty() {
            props.insert(name.clone(), obj! { "type" => "object" });
            if kind == "extended_stats" {
                props.insert(format!("{name}.std_deviation_bounds"), obj! { "type" => "object" });
            }
            for l in leaves {
                props.insert(format!("{name}.{l}"), obj! { "type" => "double" });
            }
            continue;
        }
        if let Some(ty) = ty {
            props.insert(name.clone(), obj! { "type" => ty });
        }
    }
    // Dotted output names map their parents as objects.
    let parents: Vec<String> = props.keys().flat_map(|k| k.match_indices('.').map(|(i, _)| k[..i].to_string()).collect::<Vec<_>>()).collect();
    for p in parents {
        props.entry(p).or_insert_with(|| obj! { "type" => "object" });
    }
    // `SchemaUtil.deduceMappings` collects the field types in a `HashMap`.
    crate::ingest::doc::java_hash_order(&mut props, false);
    props
}

pub(super) fn generated_dest(id: &str, now: i64, props: Map) -> Value {
    obj! {
        "mappings" => obj! {
            "_meta" => obj! {
                "_transform" => obj! { "transform" => id, "version" => obj! { "created" => CONFIG_VERSION }, "creation_date_in_millis" => now },
                "created_by" => "transform",
            },
            "properties" => Value::Obj(props),
        },
        "settings" => obj! { "index" => obj! { "number_of_shards" => "1", "auto_expand_replicas" => "0-1" } },
        "aliases" => obj! {},
    }
}

/// The order of a pivot's composite sources, which is the order of its output: Elasticsearch
/// keeps the groups in a `HashMap` and puts date histograms first, then histograms, then the
/// others (a stable sort, so ties keep the hash order).
pub(super) fn source_order(groups: &Map) -> Map {
    let mut hashed = groups.clone();
    crate::ingest::doc::java_hash_order(&mut hashed, false);
    let rank = |g: &Value| match g.as_object().and_then(|m| m.keys().next()).map(String::as_str) {
        Some("date_histogram") => 0,
        Some("histogram") => 1,
        _ => 2,
    };
    let mut entries: Vec<(String, Value)> = hashed.into_iter().collect();
    entries.sort_by_key(|(_, g)| rank(g));
    entries.into_iter().collect()
}

/// The composite aggregation a checkpoint pages through.
pub(super) fn composite_request(t: &Value, size: i64, after: Option<&Value>) -> Value {
    composite_request_over(t, t.pointer(&["source", "query"]).cloned().unwrap_or_else(|| obj! { "match_all" => obj! {} }), size, after)
}

/// [`composite_request`] over the documents `query` matches.
pub(super) fn composite_request_over(t: &Value, query: Value, size: i64, after: Option<&Value>) -> Value {
    let mut sources = Vec::new();
    let mut sub = Map::new();
    if let Some(pivot) = t.get("pivot") {
        if let Some(Value::Obj(groups)) = pivot.get("group_by") {
            for (name, g) in &source_order(groups) {
                let mut g = g.clone();
                if let Some(Value::Obj(inner)) = g.as_object_mut().and_then(|m| m.values_mut().next()).map(|v| v as &mut Value) {
                    inner.entry("missing_bucket".to_string()).or_insert(Value::from(true));
                }
                let mut m = Map::new();
                m.insert(name.clone(), g);
                sources.push(Value::Obj(m));
            }
        }
        if let Some(Value::Obj(a)) = pivot.get("aggregations").or_else(|| pivot.get("aggs")) {
            sub = a.clone();
        }
    } else if let Some(latest) = t.get("latest") {
        for k in latest.get("unique_key").and_then(Value::as_array).into_iter().flatten() {
            let f = k.to_es_string();
            let mut m = Map::new();
            m.insert(f.clone(), obj! { "terms" => obj! { "field" => f, "missing_bucket" => true } });
            sources.push(Value::Obj(m));
        }
        let sort = latest.get("sort").map(Value::to_es_string).unwrap_or_default();
        let mut so = Map::new();
        so.insert(sort, obj! { "order" => "desc" });
        sub.insert("_transform_latest".into(), obj! { "top_hits" => obj! { "size" => 1, "sort" => crate::arr![Value::Obj(so)] } });
    }
    let mut comp = Map::new();
    comp.insert("size".into(), size.into());
    comp.insert("sources".into(), Value::Arr(sources));
    if let Some(a) = after {
        comp.insert("after".into(), a.clone());
    }
    let mut agg = Map::new();
    agg.insert("composite".into(), Value::Obj(comp));
    if !sub.is_empty() {
        agg.insert("aggregations".into(), Value::Obj(sub));
    }
    obj! {
        "size" => 0,
        "query" => query,
        "runtime_mappings" => t.pointer(&["source", "runtime_mappings"]).cloned().unwrap_or_else(Value::object),
        "aggregations" => obj! { "_transform" => Value::Obj(agg) },
    }
}

/// Sets `a.b.c` in a document as nested objects, as transforms write dotted names.
pub(super) fn set_path(doc: &mut Map, name: &str, v: Value) {
    match name.split_once('.') {
        Some((head, rest)) => {
            let e = doc.entry(head.to_string()).or_insert_with(Value::object);
            if !matches!(e, Value::Obj(_)) {
                *e = Value::object();
            }
            if let Value::Obj(m) = e {
                set_path(m, rest, v);
            }
        }
        None => {
            doc.insert(name.to_string(), v);
        }
    }
}

/// `String.hashCode()` spread as `HashMap.hash` does.
pub(super) fn java_string_hash(s: &str) -> u32 {
    let h = s.encode_utf16().fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(c as u32));
    h ^ (h >> 16)
}

pub(super) const STATS_LEAVES: [&str; 5] = ["count", "min", "max", "avg", "sum"];

pub(super) const EXTENDED_STATS_LEAVES: &[&str] = &[
    "sum_of_squares",
    "variance",
    "variance_population",
    "variance_sampling",
    "std_deviation",
    "std_deviation_population",
    "std_deviation_sampling",
    "std_deviation_bounds.upper",
    "std_deviation_bounds.lower",
    "std_deviation_bounds.upper_population",
    "std_deviation_bounds.lower_population",
    "std_deviation_bounds.upper_sampling",
    "std_deviation_bounds.lower_sampling",
];

pub(super) const BOXPLOT_LEAVES: [&str; 7] = ["min", "max", "q1", "q2", "q3", "lower", "upper"];

/// A `percentiles` aggregation's percents (Elasticsearch's defaults when not given).
pub(super) fn percents_of(a: &Value) -> Vec<f64> {
    match a.pointer(&["percentiles", "percents"]) {
        Some(Value::Arr(ps)) => ps.iter().filter_map(Value::as_f64).collect(),
        _ => vec![1.0, 5.0, 25.0, 50.0, 75.0, 95.0, 99.0],
    }
}

/// `OutputFieldNameConverter.fromDouble`: whole percents without a fraction (`50`), others
/// with `_` for the decimal point (`99_5`), so they are no dotted paths.
pub(super) fn percent_key(p: f64) -> String {
    if p == (p as i64) as f64 { (p as i64).to_string() } else { crate::json::write::java_double(p).replace('.', "_") }
}

/// The value a pivot aggregation writes for its response `r`.
pub(super) fn agg_output(kind: &str, r: &Value) -> Value {
    match kind {
        "percentiles" => {
            // Ascending percents; a NaN (no data) is written as null.
            let mut out: Vec<(f64, Value)> = match r.get("values") {
                Some(Value::Obj(m)) => m.iter().filter_map(|(k, v)| Some((k.parse::<f64>().ok()?, v.clone()))).collect(),
                Some(Value::Arr(a)) => a.iter().filter_map(|e| Some((e.get("key")?.as_f64()?, e.get("value")?.clone()))).collect(),
                _ => return Value::Null,
            };
            out.sort_by(|a, b| a.0.total_cmp(&b.0));
            Value::Obj(out.into_iter().map(|(p, v)| (percent_key(p), if v.as_f64().is_some_and(f64::is_finite) { v } else { Value::Null })).collect())
        }
        "stats" | "extended_stats" | "boxplot" | "geo_bounds" | "geo_line" => match r {
            Value::Obj(m) => Value::Obj(
                m.iter()
                    .filter(|(k, _)| !k.ends_with("_as_string") && k.as_str() != "meta")
                    // `stats` writes its count as a double, like its other leaves.
                    .map(|(k, v)| (k.clone(), if kind == "stats" && k == "count" { v.as_f64().map_or(v.clone(), Value::from) } else { v.clone() }))
                    .collect(),
            ),
            other => other.clone(),
        },
        "geo_centroid" => r.get("location").cloned().unwrap_or(Value::Null),
        "filter" | "missing" => r.get("doc_count").cloned().unwrap_or(Value::from(0)),
        "range" => match r.get("buckets") {
            Some(Value::Arr(a)) => Value::Obj(a.iter().filter_map(|b| Some((b.get("key")?.to_es_string(), b.get("doc_count")?.clone()))).collect()),
            _ => Value::Null,
        },
        "terms" | "rare_terms" => match r.get("buckets") {
            Some(Value::Arr(a)) => Value::Obj(a.iter().filter_map(|b| Some((b.get("key")?.to_es_string(), b.get("doc_count")?.clone()))).collect()),
            _ => Value::Null,
        },
        "top_metrics" => r.pointer(&["top"]).and_then(Value::as_array).and_then(|a| a.first()).and_then(|t| t.get("metrics")).cloned().unwrap_or(Value::Null),
        "scripted_metric" => r.get("value").cloned().unwrap_or(Value::Null),
        _ => match r.get("value_as_string") {
            // Dates (min/max over date fields) are written formatted.
            Some(s) if r.get("value").is_some_and(|v| !v.is_null()) => s.clone(),
            _ => r.get("value").cloned().unwrap_or(Value::Null),
        },
    }
}

/// One output document per composite bucket. `types` are the deduced destination mappings:
/// numbers written to an integral field drop their fraction
/// (`dropFloatingPointComponentIfTypeRequiresIt`).
pub(super) fn bucket_docs(t: &Value, buckets: &[Value], types: &Map) -> Vec<(String, Value)> {
    let aggs = t.get("pivot").and_then(|p| p.get("aggregations").or_else(|| p.get("aggs"))).and_then(Value::as_object).cloned().unwrap_or_default();
    let mut out = Vec::with_capacity(buckets.len());
    for b in buckets {
        let key = b.get("key").cloned().unwrap_or(Value::Null);
        let id = doc_id(&key);
        if t.get("latest").is_some() {
            let src = b.pointer(&["_transform_latest", "hits", "hits"]).and_then(Value::as_array).and_then(|h| h.first()).and_then(|h| h.get("_source")).cloned();
            // Fields with a leading `_` are internal to Elasticsearch: the latest document is
            // written without them.
            if let Some(Value::Obj(mut src)) = src {
                src.retain(|k, _| !k.starts_with('_'));
                out.push((id, Value::Obj(src)));
            }
            continue;
        }
        // Numbers written to an integral field drop their fraction.
        let fit = |name: &str, v: Value| -> Value {
            let integral = types.get(name).and_then(|m| m.get("type")).and_then(Value::as_str).is_some_and(|t| matches!(t, "long" | "integer" | "short" | "byte" | "unsigned_long"));
            match v.as_f64() {
                Some(d) if integral && d.is_finite() && matches!(v, Value::Num(_)) => Value::from(d.floor() as i64),
                _ => v,
            }
        };
        let mut doc = Map::new();
        if let Value::Obj(k) = &key {
            for (name, v) in k {
                set_path(&mut doc, name, fit(name, date_key(t, name, v)));
            }
        }
        for (name, a) in &aggs {
            let kind = agg_kind(a).unwrap_or_default();
            let r = b.get(name).cloned().unwrap_or(Value::Null);
            set_path(&mut doc, name, fit(name, agg_output(kind, &r)));
        }
        // Elasticsearch builds the documents as `HashMap`s (`AggregationResultUtils`), but a
        // percentiles object is a `LinkedHashMap` in ascending percent order.
        crate::ingest::doc::java_hash_order(&mut doc, true);
        for (name, a) in &aggs {
            if a.get("percentiles").is_some() {
                let r = b.get(name).cloned().unwrap_or(Value::Null);
                set_path(&mut doc, name, agg_output("percentiles", &r));
            }
            // `std_deviation_bounds` is a `HashMap` sized for its six entries (8 buckets).
            if a.get("extended_stats").is_some()
                && let Some(Value::Obj(bounds)) = doc.get_mut(name.as_str()).and_then(Value::as_object_mut).and_then(|m| m.get_mut("std_deviation_bounds"))
            {
                let mut e: Vec<(usize, String, Value)> = std::mem::take(bounds).into_iter().enumerate().map(|(i, (k, v))| (i, k, v)).collect();
                e.sort_by_key(|(i, k, _)| (java_string_hash(k) & 7, *i));
                bounds.extend(e.into_iter().map(|(_, k, v)| (k, v)));
            }
        }
        out.push((id, Value::Obj(doc)));
    }
    out
}

/// `date_histogram` group keys are written as UTC dates with milliseconds
/// (`yyyy-MM-dd'T'HH:mm:ss.SSSZ`), whatever the histogram's time zone.
pub(super) fn date_key(t: &Value, name: &str, v: &Value) -> Value {
    let is_date = t.pointer(&["pivot", "group_by", name, "date_histogram"]).is_some();
    match (is_date, v.as_i64()) {
        (true, Some(ms)) => Value::from(utc_millis_string(ms)),
        _ => v.clone(),
    }
}

pub(super) fn utc_millis_string(ms: i64) -> String {
    let s = crate::time::java_instant_string(ms);
    // `Instant.toString` drops a zero fraction: put it back.
    match s.strip_suffix('Z') {
        Some(head) if !head.contains('.') => format!("{head}.000Z"),
        _ => s,
    }
}

/// A stable id for a bucket key: URL-safe base64 of a 24-byte digest, the length of
/// Elasticsearch's transform document ids.
pub(super) fn doc_id(key: &Value) -> String {
    use base64::Engine;
    let d = crate::xpack::security::digest(&[b"transform", key.to_json().as_bytes()]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&d[..24])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_types() {
        assert_eq!(agg_target("max", Some("float")), Some("float"));
        assert_eq!(agg_target("max", Some("scaled_float")), Some("float"));
        assert_eq!(agg_target("avg", Some("integer")), Some("double"));
        assert_eq!(agg_target("value_count", None), Some("long"));
        assert_eq!(agg_target("scripted_metric", None), None);
    }

    #[test]
    fn dotted_output() {
        let mut d = Map::new();
        set_path(&mut d, "a.b", Value::from(1));
        set_path(&mut d, "a.c", Value::from(2));
        assert_eq!(Value::Obj(d).to_json(), r#"{"a":{"b":1,"c":2}}"#);
    }

    #[test]
    fn stable_ids() {
        let k = obj! { "pop" => 13.0 };
        assert_eq!(doc_id(&k), doc_id(&k.clone()));
        assert_eq!(doc_id(&k).len(), 32);
        assert_ne!(doc_id(&k), doc_id(&obj! { "pop" => 14.0 }));
    }

    #[test]
    fn outputs() {
        assert_eq!(agg_output("max", &obj! { "value" => 1.5 }), Value::from(1.5));
        assert_eq!(agg_output("max", &obj! { "value" => 1.0e12, "value_as_string" => "2001-09-09T01:46:40.000Z" }), Value::from("2001-09-09T01:46:40.000Z"));
        assert_eq!(agg_output("filter", &obj! { "doc_count" => 3 }), Value::from(3));
        assert_eq!(percent_key(50.0), "50");
        assert_eq!(percent_key(99.5), "99_5");
        assert_eq!(percent_key(0.001), "0_001");
        let p = agg_output("percentiles", &obj! { "values" => obj! { "99.5" => 98.51, "1.0" => 1.98, "50.0" => "NaN" } });
        assert_eq!(p, obj! { "1" => 1.98, "50" => Value::Null, "99_5" => 98.51 });
        assert_eq!(p.as_object().unwrap().keys().collect::<Vec<_>>(), ["1", "50", "99_5"]);
        assert_eq!(agg_output("stats", &obj! { "count" => 50, "min" => 1.0 }), obj! { "count" => 50.0, "min" => 1.0 });
    }
}
