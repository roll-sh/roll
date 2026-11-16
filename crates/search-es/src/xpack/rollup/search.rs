//! `_rollup_search` (`TransportRollupSearchAction`, `RollupJobIdentifierUtils`,
//! `RollupRequestTranslator`, `RollupResponseTranslator`).
//!
//! The target splits into live indices and at most one rollup index (an index whose mapping
//! `_meta._rollup` holds jobs). Live indices alone are an ordinary search. Otherwise each
//! top-level aggregation picks the best job able to answer it (date histogram intervals that
//! divide the requested one, in the same zone rules; histogram intervals likewise; terms and
//! metrics on the job's fields; ties broken by Elasticsearch's comparator), the aggregations
//! are rewritten to the rolled fields (each bucket aggregation gains a `<name>._count` sum, an
//! `avg` becomes a sum and a count) under a `filter_<name>` wrapper, the query's fields are
//! rewritten likewise, and one search per chosen job runs on the rollup index through the core
//! search API (as the caller), filtered to that job's documents.
//!
//! The responses are unrolled as Elasticsearch does: a bucket's doc count is its `_count` sum
//! (buckets with none are dropped, a bucket an earlier job already counted counts zero), sums
//! with a count become averages, `_count` helpers disappear, live buckets win over rolled ones
//! with the same key; then the results of every job and the live indices are merged and the
//! final ordering, `min_doc_count` and `size` applied. The response has no hits (total 0).
//!
//! The JSON-level merge reproduces Elasticsearch's reduce for what rollup supports, with two
//! approximations when more than one response is merged: a date histogram's empty buckets are
//! the ones each response filled itself (gaps between two responses' ranges stay unfilled),
//! and a live index's `avg` (whose sum and count a response doesn't show) keeps the rolled
//! average where both have one.

use super::*;
use config::{JobCaps, calendar_unit_millis, same_rules};
use std::collections::{HashMap, HashSet};

/// Marks a rolled bucket that had no documents: kept as Elasticsearch's final reduce re-adds
/// it (with the rolled empty sub-aggregations) where `min_doc_count` is 0.
const GAP: &str = "\u{1}roll_gap";
/// An unrolled average's sum and count, until the final render.
const AVG_SUM: &str = "\u{1}roll_sum";
const AVG_COUNT: &str = "\u{1}roll_count";

/// The bucket aggregations rollup unrolls (their `typed_keys` names).
const UNROLLED_MULTI: [&str; 4] = ["date_histogram", "histogram", "sterms", "lterms"];

fn runtime(reason: String) -> EsError {
    EsError::java(500, "runtime_exception", reason)
}

/// `type#name` of a `typed_keys` response.
fn split(k: &str) -> (&str, &str) {
    k.split_once('#').unwrap_or(("", k))
}

/// The aggregation named `name` in a `typed_keys` map.
fn find<'a>(m: Option<&'a Map>, name: &str) -> Option<(&'a str, &'a Value)> {
    m?.iter().find(|(k, _)| split(k).1 == name).map(|(k, v)| (split(k).0, v))
}

/// The aggregation entries of an aggregation or bucket object (`typed_keys` names).
fn sub_aggs(v: &Value) -> Map {
    v.as_object().map(|m| m.iter().filter(|(k, x)| k.contains('#') && x.as_object().is_some()).map(|(k, x)| (k.clone(), x.clone())).collect()).unwrap_or_default()
}

/// A Java class name for messages (`getClass().getSimpleName()` / `AggregationBuilder`).
fn class_name(ty: &str, suffix: &str) -> String {
    let base = match ty {
        "dterms" => return "DoubleTerms".into(),
        "value_count" => return format!("{}ValueCount", if suffix.is_empty() { "Internal" } else { "" }) + suffix,
        _ => ty,
    };
    let camel: String = base.split('_').map(|p| {
        let mut c = p.chars();
        c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
    }).collect();
    camel + suffix
}

/// An aggregation definition: its type, parameters, sub-aggregations and `meta`.
struct AggDef<'a> {
    name: &'a str,
    ty: &'a str,
    params: Map,
    subs: Map,
    meta: Option<&'a Value>,
}

fn agg_def<'a>(name: &'a str, v: &'a Value) -> EsResult<AggDef<'a>> {
    let m = v.as_object().ok_or_else(|| EsError::parse(format!("Expected [START_OBJECT] under [{name}], but got a [{}] in [{name}]", v.kind_name())))?;
    let mut def = AggDef { name, ty: "", params: Map::new(), subs: Map::new(), meta: None };
    for (k, x) in m {
        match k.as_str() {
            "aggs" | "aggregations" => def.subs = x.as_object().cloned().unwrap_or_default(),
            "meta" => def.meta = Some(x),
            ty => {
                def.ty = ty;
                def.params = x.as_object().cloned().unwrap_or_default();
            }
        }
    }
    Ok(def)
}

fn field_of(def: &AggDef) -> String {
    def.params.get("field").and_then(Value::as_str).unwrap_or_default().to_string()
}

/// `DateHistogramInterval.estimateMillis` of a cap's interval.
fn estimate_millis(s: &str) -> i64 {
    calendar_unit_millis(s).unwrap_or_else(|| crate::units::TimeValue::parse(s, "DateHistogramInterval#estimateMillis").map_or(i64::MAX, |t| t.as_millis()))
}

fn cap_interval(agg: &Map) -> Option<String> {
    ["interval", "calendar_interval", "fixed_interval"].iter().find_map(|k| agg.get(*k).map(Value::to_es_string))
}

/// `RollupJobIdentifierUtils.COMPARATOR`: larger date intervals first, then larger histogram
/// intervals (jobs without one first), then fewer terms.
fn compare_caps(a: &JobCaps, b: &JobCaps) -> std::cmp::Ordering {
    fn weights(c: &JobCaps) -> (i64, f32, i64) {
        let (mut time, mut histo, mut n, mut terms) = (i64::MAX, 0f32, 0i64, 0i64);
        for aggs in c.fields.values() {
            for agg in aggs {
                match agg.get("agg").and_then(Value::as_str) {
                    Some("date_histogram") => time = cap_interval(agg).map_or(i64::MAX, |i| estimate_millis(&i)),
                    Some("histogram") => {
                        histo += agg.get("interval").and_then(Value::as_i64).unwrap_or(0) as f32;
                        n += 1;
                    }
                    Some("terms") => terms += 1,
                    _ => {}
                }
            }
        }
        (time, if n == 0 { 0.0 } else { histo / n as f32 }, terms)
    }
    use std::cmp::Ordering;
    if a.job_id == b.job_id {
        return Ordering::Equal;
    }
    let (ta, ha, sa) = weights(a);
    let (tb, hb, sb) = weights(b);
    if ta != tb {
        return tb.cmp(&ta);
    }
    if ha != hb {
        if ha == 0.0 {
            return Ordering::Less;
        }
        if hb == 0.0 {
            return Ordering::Greater;
        }
        return hb.partial_cmp(&ha).unwrap_or(Ordering::Equal);
    }
    sa.cmp(&sb)
}

fn top_caps(mut caps: Vec<&JobCaps>) -> JobCaps {
    caps.sort_by(|a, b| compare_caps(a, b));
    caps[0].clone()
}

fn no_job(def: &AggDef, what: &str) -> EsError {
    EsError::illegal_argument(format!("There is not a rollup job that has a [{}] agg {what} which also satisfies all requirements of query.", def.ty))
}

fn parse_interval_millis(s: &str, setting: &str) -> EsResult<i64> {
    crate::units::TimeValue::parse(s, setting).map(|t| t.as_millis())
}

/// `validateCalendarInterval`: the requested unit is at least the configured one.
fn calendar_ok(request: &str, config: Option<String>) -> bool {
    match (calendar_unit_millis(request), config.as_deref().and_then(calendar_unit_millis)) {
        (Some(r), Some(c)) => r >= c,
        _ => false,
    }
}

/// `validateFixedInterval`: the requested interval is a multiple of the configured one.
fn fixed_ok(request: &str, config: Option<String>) -> EsResult<bool> {
    let Some(config) = config else { return Ok(false) };
    let c = parse_interval_millis(&config, "date_histo.config.interval")?;
    let r = parse_interval_millis(request, "date_histo.request.interval")?;
    Ok(c > 0 && r >= c && r % c == 0)
}

/// `findBestJobs`: the jobs able to answer an aggregation tree, one per leaf.
fn find_best(def: &AggDef, caps: &[&JobCaps], best: &mut Vec<JobCaps>) -> EsResult<()> {
    let field = field_of(def);
    let mut local: Vec<&JobCaps> = Vec::new();
    match def.ty {
        "date_histogram" => {
            let source_tz = def.params.get("time_zone").map(Value::to_es_string).unwrap_or_else(|| "UTC".to_string());
            for cap in caps {
                for agg in cap.aggs(&field) {
                    if agg.get("agg").and_then(Value::as_str) != Some("date_histogram") {
                        continue;
                    }
                    let tz = agg.get("time_zone").map(Value::to_es_string).unwrap_or_else(|| "UTC".to_string());
                    if !same_rules(&tz, &source_tz) {
                        continue;
                    }
                    let get = |k: &str| agg.get(k).map(Value::to_es_string);
                    if let Some(req) = def.params.get("calendar_interval").map(Value::to_es_string) {
                        if calendar_ok(&req, get("calendar_interval")) || calendar_ok(&req, get("interval")) {
                            local.push(cap);
                        }
                    } else if let Some(req) = def.params.get("fixed_interval").map(Value::to_es_string) {
                        if fixed_ok(&req, get("fixed_interval"))? || fixed_ok(&req, get("interval"))? {
                            local.push(cap);
                        }
                    } else {
                        return Err(EsError::illegal_argument("An interval of some variety must be configured on the date_histogram aggregation."));
                    }
                    break;
                }
            }
            if local.is_empty() {
                return Err(no_job(def, &format!("on field [{field}]")));
            }
        }
        "histogram" => {
            let interval = def.params.get("interval").and_then(Value::as_f64).unwrap_or(0.0);
            for cap in caps {
                if let Some(agg) = cap.aggs(&field).iter().find(|a| a.get("agg").and_then(Value::as_str) == Some("histogram")) {
                    let ci = agg.get("interval").and_then(Value::as_i64).unwrap_or(0) as f64;
                    if ci <= interval && ci != 0.0 && interval % ci == 0.0 {
                        local.push(cap);
                    }
                }
            }
            if local.is_empty() {
                return Err(no_job(def, &format!("on field [{field}]")));
            }
        }
        "terms" => {
            local = caps.iter().copied().filter(|c| c.aggs(&field).iter().any(|a| a.get("agg").and_then(Value::as_str) == Some("terms"))).collect();
            if local.is_empty() {
                return Err(no_job(def, &format!("on field [{field}]")));
            }
        }
        "min" | "max" | "sum" | "avg" | "value_count" => {
            local = caps.iter().copied().filter(|c| c.aggs(&field).iter().any(|a| a.get("agg").and_then(Value::as_str) == Some(def.ty))).collect();
            if local.is_empty() {
                return Err(no_job(def, &format!("with name [{}]", def.name)));
            }
            push_best(best, top_caps(local));
            return Ok(());
        }
        other => return Err(unsupported(def.name, other)),
    }
    if def.subs.is_empty() {
        push_best(best, top_caps(local));
    } else {
        for (name, sub) in &def.subs {
            find_best(&agg_def(name, sub)?, &local, best)?;
        }
    }
    Ok(())
}

fn push_best(best: &mut Vec<JobCaps>, caps: JobCaps) {
    if !best.iter().any(|b| b.job_id == caps.job_id) {
        best.push(caps);
    }
}

fn unsupported(name: &str, ty: &str) -> EsError {
    EsError::illegal_argument(format!(
        "Unable to translate aggregation tree into Rollup.  Aggregation [{name}] is of type [{}] which is currently unsupported.",
        class_name(ty, "AggregationBuilder")
    ))
}

fn copy(from: &Map, to: &mut Map, keys: &[&str]) {
    for k in keys {
        if let Some(v) = from.get(*k) {
            to.insert((*k).to_string(), v.clone());
        }
    }
}

/// `RollupRequestTranslator.translateAggregation`: the rolled aggregations of one definition.
fn translate(def: &AggDef) -> EsResult<Vec<(String, Value)>> {
    let field = field_of(def);
    let rolled = |kind: &str, extra: &str| format!("{field}.{kind}.{extra}");
    let bucket = |ty: &str, params: Map, meta: bool| -> EsResult<Vec<(String, Value)>> {
        let mut subs = Map::new();
        for (name, sub) in &def.subs {
            for (k, v) in translate(&agg_def(name, sub)?)? {
                subs.insert(k, v);
            }
        }
        subs.insert(format!("{}._count", def.name), obj! { "sum" => obj! { "field" => rolled(ty, "_count") } });
        let mut body = Map::new();
        body.insert(ty.to_string(), Value::Obj(params));
        body.insert("aggs".into(), Value::Obj(subs));
        if meta && let Some(m) = def.meta {
            body.insert("meta".into(), m.clone());
        }
        Ok(vec![(def.name.to_string(), Value::Obj(body))])
    };
    match def.ty {
        "date_histogram" => {
            let mut p = Map::new();
            p.insert("field".into(), rolled("date_histogram", "timestamp").into());
            copy(&def.params, &mut p, &["calendar_interval", "fixed_interval"]);
            p.insert("time_zone".into(), def.params.get("time_zone").cloned().unwrap_or_else(|| "UTC".into()));
            copy(&def.params, &mut p, &["offset", "extended_bounds", "format", "min_doc_count", "order"]);
            bucket("date_histogram", p, true)
        }
        "histogram" => {
            let mut p = Map::new();
            p.insert("field".into(), rolled("histogram", "value").into());
            copy(&def.params, &mut p, &["interval", "offset"]);
            if let Some(Value::Obj(b)) = def.params.get("extended_bounds")
                && b.get("min").is_some()
                && b.get("max").is_some()
            {
                p.insert("extended_bounds".into(), Value::Obj(b.clone()));
            }
            copy(&def.params, &mut p, &["min_doc_count", "order"]);
            bucket("histogram", p, true)
        }
        "terms" => {
            let mut p = Map::new();
            copy(&def.params, &mut p, &["value_type"]);
            p.insert("field".into(), rolled("terms", "value").into());
            copy(
                &def.params,
                &mut p,
                &["include", "exclude", "collect_mode", "min_doc_count", "execution_hint", "order", "shard_min_doc_count", "shard_size", "show_term_doc_count_error", "size"],
            );
            bucket("terms", p, false)
        }
        "avg" => Ok(vec![
            (format!("{}.value", def.name), obj! { "sum" => obj! { "field" => rolled("avg", "value") } }),
            (format!("{}._count", def.name), obj! { "sum" => obj! { "field" => rolled("avg", "_count") } }),
        ]),
        "min" | "max" | "sum" | "value_count" => {
            let mut p = def.params.clone();
            p.insert("field".into(), rolled(def.ty, "value").into());
            let mut body = Map::new();
            body.insert(def.ty.to_string(), Value::Obj(p));
            if let Some(m) = def.meta {
                body.insert("meta".into(), m.clone());
            }
            Ok(vec![(def.name.to_string(), Value::Obj(body))])
        }
        other => Err(unsupported(def.name, other)),
    }
}

/// `rewriteFieldName`: the rolled field a query on `field` reads, which must be unambiguous.
fn rewrite_field(caps: &[JobCaps], builder: &str, field: &str) -> EsResult<String> {
    let mut lists: Vec<Vec<String>> = Vec::new();
    for cap in caps.iter().filter(|c| c.fields.contains_key(field)) {
        let list: Vec<String> = cap
            .aggs(field)
            .iter()
            .filter_map(|a| a.get("agg").and_then(Value::as_str))
            .filter(|t| matches!(*t, "terms" | "date_histogram" | "histogram"))
            .map(|t| if t == "date_histogram" { format!("{field}.{t}.timestamp") } else { format!("{field}.{t}.value") })
            .collect();
        if !lists.contains(&list) {
            lists.push(list);
        }
    }
    let names: Vec<String> = lists.into_iter().flatten().collect();
    match names.len() {
        0 => Err(EsError::illegal_argument(format!("Field [{field}] in [{builder}] query is not available in selected rollup indices, cannot query."))),
        1 => Ok(names.into_iter().next().unwrap_or_default()),
        _ => Err(EsError::illegal_argument(format!(
            "Ambiguous field name resolution when mapping to rolled fields.  Field name [{field}] was mapped to: [{}].",
            names.join(",")
        ))),
    }
}

/// The field a single-field query (`term`, `terms`, `range`) names.
fn query_field(m: &Map) -> Option<(&String, &Value)> {
    m.iter().find(|(k, _)| !matches!(k.as_str(), "boost" | "_name"))
}

/// `rewriteQuery`: the query on the rolled fields (only the query types rollup supports).
fn rewrite_query(q: Option<&Value>, caps: &[JobCaps]) -> EsResult<Value> {
    let Some(Value::Obj(q)) = q else { return Ok(obj! { "match_all" => obj! {} }) };
    let Some((ty, body)) = q.iter().next() else { return Ok(obj! { "match_all" => obj! {} }) };
    let inner = body.as_object().cloned().unwrap_or_default();
    let list = |v: Option<&Value>| -> Vec<Value> {
        match v {
            Some(Value::Arr(a)) => a.clone(),
            Some(o @ Value::Obj(_)) => vec![o.clone()],
            _ => Vec::new(),
        }
    };
    Ok(match ty.as_str() {
        "bool" => {
            let mut out = Map::new();
            for clause in ["must", "must_not", "should", "filter"] {
                let rewritten: Vec<Value> = list(inner.get(clause)).iter().map(|c| rewrite_query(Some(c), caps)).collect::<EsResult<_>>()?;
                if !rewritten.is_empty() {
                    out.insert(clause.into(), Value::Arr(rewritten));
                }
            }
            obj! { "bool" => Value::Obj(out) }
        }
        "constant_score" => obj! { "constant_score" => obj! { "filter" => rewrite_query(inner.get("filter"), caps)? } },
        // Elasticsearch passes the negative query as the positive one and vice versa.
        "boosting" => {
            let mut out = Map::new();
            out.insert("positive".into(), rewrite_query(inner.get("negative"), caps)?);
            out.insert("negative".into(), rewrite_query(inner.get("positive"), caps)?);
            if let Some(b) = inner.get("negative_boost") {
                out.insert("negative_boost".into(), b.clone());
            }
            obj! { "boosting" => Value::Obj(out) }
        }
        "dis_max" => {
            let qs: Vec<Value> = list(inner.get("queries")).iter().map(|c| rewrite_query(Some(c), caps)).collect::<EsResult<_>>()?;
            obj! { "dis_max" => obj! { "queries" => Value::Arr(qs) } }
        }
        "range" => {
            let (field, params) = query_field(&inner).ok_or_else(|| EsError::parse("[range] query malformed, no field"))?;
            let f = rewrite_field(caps, "range", field)?;
            let mut p = Map::new();
            if let Some(pm) = params.as_object() {
                copy(pm, &mut p, &["gte", "gt", "lte", "lt", "from", "to", "include_lower", "include_upper", "time_zone", "format"]);
            }
            let mut m = Map::new();
            m.insert(f, Value::Obj(p));
            obj! { "range" => Value::Obj(m) }
        }
        "term" => {
            let (field, v) = query_field(&inner).ok_or_else(|| EsError::parse("[term] query malformed, no field"))?;
            let f = rewrite_field(caps, "term", field)?;
            let value = match v {
                Value::Obj(o) => o.get("value").cloned().unwrap_or_default(),
                other => other.clone(),
            };
            let mut m = Map::new();
            m.insert(f, value);
            obj! { "term" => Value::Obj(m) }
        }
        "terms" => {
            let (field, v) = query_field(&inner).ok_or_else(|| EsError::parse("[terms] query malformed, no field"))?;
            let f = rewrite_field(caps, "term", field)?;
            let mut m = Map::new();
            m.insert(f, v.clone());
            obj! { "terms" => Value::Obj(m) }
        }
        "match_all" => Value::Obj(q.clone()),
        other => return Err(EsError::illegal_argument(format!("Unsupported Query in search request: [{other}]"))),
    })
}

/// `validateSearchRequest`: what a search over rollup data can't do.
fn validate(ctx: &Ctx, body: &Map) -> EsResult<()> {
    let size = match ctx.query("size") {
        Some(s) => crate::units::parse_java_int(s).map(i64::from),
        None => body.get("size").and_then(Value::as_i64),
    };
    if size != Some(0) {
        return Err(EsError::illegal_argument("Rollup does not support returning search hits, please try again with [size: 0]."));
    }
    let checks: [(&str, &str); 3] =
        [("post_filter", "Rollup search does not support post filtering."), ("suggest", "Rollup search does not support suggestors."), ("highlight", "Rollup search does not support highlighting.")];
    for (key, reason) in checks {
        if body.contains_key(key) {
            return Err(EsError::illegal_argument(reason));
        }
    }
    if body.get("profile").and_then(Value::as_bool) == Some(true) {
        return Err(EsError::illegal_argument("Rollup search does not support profiling at the moment."));
    }
    if body.get("explain").and_then(Value::as_bool) == Some(true) || ctx.bool_param("explain", false)? {
        return Err(EsError::illegal_argument("Rollup search does not support explaining."));
    }
    Ok(())
}

/// The value of a sum aggregation as Java's `(long) double`.
fn sum_count(v: &Value) -> i64 {
    v.get("value").and_then(Value::as_f64).map_or(0, |f| if f.is_nan() { 0 } else { f as i64 })
}

/// `getAggCount`: the `_count` sum that goes with a bucket aggregation or a sum (`-1`: none).
fn agg_count(ty: &str, name: &str, aggs: Option<&Map>) -> i64 {
    let path = match ty {
        "date_histogram" | "histogram" | "sterms" | "lterms" => format!("{name}._count"),
        "sum" => format!("{}._count", name.replace(".value", "")),
        _ => return -1,
    };
    find(aggs, &path).map_or(-1, |(_, v)| sum_count(v))
}

fn bucket_key(b: &Value) -> String {
    b.get("key").map(Value::to_json).unwrap_or_default()
}

fn buckets(v: Option<&Value>) -> &[Value] {
    v.and_then(|v| v.get("buckets")).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// `unrollAgg` over an aggregation list.
fn unroll_list(rolled: &Map, original: Option<&Map>, current: Option<&Map>) -> EsResult<Map> {
    let mut out = Map::new();
    for (k, v) in rolled {
        let (ty, name) = split(k);
        if name.ends_with("._count") {
            continue;
        }
        let count = if v.get("buckets").is_some() { -1 } else { agg_count(ty, name, Some(rolled)) };
        let (nk, nv) = unroll_agg(k, v, find(original, name).map(|x| x.1), find(current, name).map(|x| x.1), count)?;
        out.insert(nk, nv);
    }
    Ok(out)
}

/// `unrollAgg` of one result.
fn unroll_agg(key: &str, v: &Value, original: Option<&Value>, current: Option<&Value>, count: i64) -> EsResult<(String, Value)> {
    let (ty, name) = split(key);
    if v.get("buckets").is_some() {
        if !UNROLLED_MULTI.contains(&ty) {
            return Err(runtime(format!(
                "Unable to unroll aggregation tree.  Aggregation [{name}] is of type [{}] which is currently unsupported.",
                class_name(ty, "")
            )));
        }
        let original_keys: HashSet<String> = buckets(original).iter().map(bucket_key).collect();
        let current_buckets: HashMap<String, &Value> = buckets(current).iter().map(|b| (bucket_key(b), b)).collect();
        let mut out = Vec::new();
        for b in buckets(Some(v)) {
            let k = bucket_key(b);
            if original_keys.contains(&k) {
                continue;
            }
            let bm = b.as_object();
            let subs = sub_aggs(b);
            let mut n = agg_count(ty, name, Some(&subs));
            if n <= 0 {
                if b.get("doc_count").and_then(Value::as_i64) == Some(0) {
                    let mut gap = bm.cloned().unwrap_or_default();
                    gap.insert(GAP.into(), true.into());
                    out.push(Value::Obj(gap));
                }
                continue;
            }
            let cur = current_buckets.get(&k).copied();
            if cur.and_then(|c| c.get("doc_count")).and_then(Value::as_i64).is_some_and(|c| c != 0) {
                n = 0;
            }
            let orig_bucket = buckets(original).iter().find(|ob| bucket_key(ob) == k);
            let mut nb = Map::new();
            for (fk, fv) in bm.into_iter().flatten() {
                if fk.contains('#') {
                    continue;
                }
                nb.insert(fk.clone(), if fk == "doc_count" { n.into() } else { fv.clone() });
            }
            for (sk, sv) in &subs {
                let (sty, sname) = split(sk);
                if sname.ends_with("._count") {
                    continue;
                }
                let c = agg_count(sty, sname, Some(&subs));
                let o = orig_bucket.map(sub_aggs);
                let cu = cur.map(sub_aggs);
                let (nk, nv) = unroll_agg(sk, sv, find(o.as_ref(), sname).map(|x| x.1), find(cu.as_ref(), sname).map(|x| x.1), c)?;
                nb.insert(nk, nv);
            }
            out.push(Value::Obj(nb));
        }
        let mut agg = v.as_object().cloned().unwrap_or_default();
        agg.insert("buckets".into(), Value::Arr(out));
        return Ok((key.to_string(), Value::Obj(agg)));
    }
    match ty {
        "max" | "min" => Ok((key.to_string(), v.clone())),
        "sum" if count != -1 => {
            let sum = v.get("value").and_then(Value::as_f64).unwrap_or(0.0);
            let mut m = Map::new();
            m.insert(AVG_SUM.into(), sum.into());
            m.insert(AVG_COUNT.into(), count.into());
            Ok((format!("avg#{}", name.replace(".value", "")), Value::Obj(m)))
        }
        "sum" => Ok((key.to_string(), v.clone())),
        other => Err(runtime(format!("Unable to unroll metric.  Aggregation [{name}] is of type [{}] which is currently unsupported.", class_name(other, "")))),
    }
}

fn num(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(Value::as_f64)
}

/// The reduce of two results of one aggregation (Elasticsearch's `InternalAggregations.reduce`
/// for the types rollup produces).
fn reduce_agg(ty: &str, a: &Value, b: &Value) -> Value {
    if a.get("buckets").is_some() {
        let mut out = a.as_object().cloned().unwrap_or_default();
        let mut list: Vec<Value> = buckets(Some(a)).to_vec();
        for bb in buckets(Some(b)) {
            let k = bucket_key(bb);
            match list.iter_mut().find(|x| bucket_key(x) == k) {
                Some(ab) => *ab = reduce_bucket(ab, bb),
                None => list.push(bb.clone()),
            }
        }
        for f in ["sum_other_doc_count", "doc_count_error_upper_bound"] {
            if let (Some(x), Some(y)) = (a.get(f).and_then(Value::as_i64), b.get(f).and_then(Value::as_i64)) {
                out.insert(f.into(), (x + y).into());
            }
        }
        out.insert("buckets".into(), Value::Arr(list));
        return Value::Obj(out);
    }
    let pick = |f: fn(f64, f64) -> f64| -> Value {
        let v = match (num(a, "value"), num(b, "value")) {
            (Some(x), Some(y)) => Value::from(f(x, y)),
            (Some(x), None) => Value::from(x),
            (None, Some(y)) => Value::from(y),
            (None, None) => Value::Null,
        };
        let mut m = a.as_object().cloned().unwrap_or_default();
        m.insert("value".into(), v);
        m.shift_remove("value_as_string");
        Value::Obj(m)
    };
    match ty {
        "max" => pick(f64::max),
        "min" => pick(f64::min),
        "sum" => pick(|x, y| x + y),
        "avg" => match (num(a, AVG_SUM), num(b, AVG_SUM)) {
            (Some(x), Some(y)) => {
                let c = a.get(AVG_COUNT).and_then(Value::as_i64).unwrap_or(0) + b.get(AVG_COUNT).and_then(Value::as_i64).unwrap_or(0);
                let mut m = Map::new();
                m.insert(AVG_SUM.into(), (x + y).into());
                m.insert(AVG_COUNT.into(), c.into());
                Value::Obj(m)
            }
            (None, Some(_)) => b.clone(),
            _ => a.clone(),
        },
        _ => a.clone(),
    }
}

fn is_gap(b: &Value) -> bool {
    b.get(GAP).is_some()
}

fn reduce_bucket(a: &Value, b: &Value) -> Value {
    if is_gap(b) {
        return a.clone();
    }
    if is_gap(a) {
        return b.clone();
    }
    let mut out = a.as_object().cloned().unwrap_or_default();
    let n = a.get("doc_count").and_then(Value::as_i64).unwrap_or(0) + b.get("doc_count").and_then(Value::as_i64).unwrap_or(0);
    out.insert("doc_count".into(), n.into());
    let merged = reduce_maps(sub_aggs(a), &sub_aggs(b));
    for (k, v) in merged {
        out.insert(k, v);
    }
    Value::Obj(out)
}

/// Reduces `b` into `a`, aggregation by aggregation (matched by name).
fn reduce_maps(mut a: Map, b: &Map) -> Map {
    for (k, vb) in b {
        let (ty, name) = split(k);
        match a.iter().find(|(ka, _)| split(ka).1 == name).map(|(ka, _)| ka.clone()) {
            Some(ka) => {
                let merged = reduce_agg(ty, &a[&ka], vb);
                a.insert(ka, merged);
            }
            None => {
                a.insert(k.clone(), vb.clone());
            }
        }
    }
    a
}

/// A bucket order (`order`): the sort keys of a request's bucket aggregation.
fn order_of(params: &Map, default: &[(&str, bool)]) -> Vec<(String, bool)> {
    let pairs = |m: &Map| m.iter().map(|(k, v)| (k.clone(), v.as_str() != Some("desc"))).collect::<Vec<_>>();
    match params.get("order") {
        Some(Value::Obj(m)) => pairs(m),
        Some(Value::Arr(a)) => a.iter().filter_map(Value::as_object).flat_map(pairs).collect(),
        _ => default.iter().map(|(k, asc)| (k.to_string(), *asc)).collect(),
    }
}

fn sort_value(b: &Value, key: &str) -> Option<f64> {
    match key {
        "_count" => b.get("doc_count").and_then(Value::as_f64),
        "_key" | "_term" => b.get("key").and_then(Value::as_f64),
        path => {
            let (agg, metric) = path.split_once('.').unwrap_or((path, "value"));
            let subs = sub_aggs(b);
            let (_, v) = find(Some(&subs), agg)?;
            match (num(v, AVG_SUM), v.get(AVG_COUNT).and_then(Value::as_i64)) {
                (Some(s), Some(c)) => (c > 0).then(|| s / c as f64),
                _ => num(v, metric),
            }
        }
    }
}

fn compare_buckets(a: &Value, b: &Value, order: &[(String, bool)]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for (key, asc) in order {
        let ord = if matches!(key.as_str(), "_key" | "_term") && a.get("key").is_some_and(Value::is_string) {
            a.get("key").and_then(Value::as_str).cmp(&b.get("key").and_then(Value::as_str))
        } else {
            let (x, y) = (sort_value(a, key), sort_value(b, key));
            x.partial_cmp(&y).unwrap_or(Ordering::Equal)
        };
        let ord = if *asc { ord } else { ord.reverse() };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

trait IsString {
    fn is_string(&self) -> bool;
}

impl IsString for Value {
    fn is_string(&self) -> bool {
        matches!(self, Value::Str(_))
    }
}

/// The final reduce's ordering, `min_doc_count` and `size`, then the render: averages from
/// their sums and counts, keyed buckets, and no `typed_keys` prefixes unless asked.
fn finalize(aggs: Map, defs: &Map, typed_keys: bool) -> Map {
    let mut out = Map::new();
    for (k, mut v) in aggs {
        let (ty, name) = split(&k);
        let def = defs.get(name).and_then(|d| agg_def(name, d).ok());
        let params = def.as_ref().map(|d| d.params.clone()).unwrap_or_default();
        let sub_defs = def.as_ref().map(|d| d.subs.clone()).unwrap_or_default();
        if let Value::Obj(m) = &mut v {
            if let Some(Value::Arr(list)) = m.get("buckets").cloned() {
                let terms = matches!(ty, "sterms" | "lterms");
                let default_min = if terms { 1 } else { 0 };
                let min_doc_count = params.get("min_doc_count").and_then(Value::as_i64).unwrap_or(default_min);
                let mut list: Vec<Value> = list.into_iter().filter(|b| b.get("doc_count").and_then(Value::as_i64).unwrap_or(0) >= min_doc_count).collect();
                let default_order: &[(&str, bool)] = if terms { &[("_count", false), ("_key", true)] } else { &[("_key", true)] };
                let mut order = order_of(&params, default_order);
                if terms && !order.iter().any(|(k, _)| matches!(k.as_str(), "_key" | "_term")) {
                    order.push(("_key".into(), true));
                }
                list.sort_by(|a, b| compare_buckets(a, b, &order));
                if terms {
                    let size = params.get("size").and_then(Value::as_i64).unwrap_or(10).max(0) as usize;
                    list.truncate(size);
                }
                let rendered: Vec<Value> = list
                    .into_iter()
                    .map(|b| {
                        let mut bm = b.as_object().cloned().unwrap_or_default();
                        bm.shift_remove(GAP);
                        let subs: Map = bm.iter().filter(|(k, x)| k.contains('#') && x.as_object().is_some()).map(|(k, x)| (k.clone(), x.clone())).collect();
                        let fin = finalize(subs, &sub_defs, typed_keys);
                        let mut nb = Map::new();
                        for (bk, bv) in bm {
                            if !bk.contains('#') {
                                nb.insert(bk, bv);
                            }
                        }
                        nb.extend(fin);
                        Value::Obj(nb)
                    })
                    .collect();
                let keyed = params.get("keyed").and_then(Value::as_bool) == Some(true) && !terms;
                if keyed {
                    let mut km = Map::new();
                    for b in rendered {
                        let key = b.get("key_as_string").map(Value::to_es_string).or_else(|| b.get("key").map(Value::to_es_string)).unwrap_or_default();
                        km.insert(key, b);
                    }
                    m.insert("buckets".into(), Value::Obj(km));
                } else {
                    m.insert("buckets".into(), Value::Arr(rendered));
                }
            } else if let (Some(s), Some(c)) = (num(&v, AVG_SUM), v.get(AVG_COUNT).and_then(Value::as_i64)) {
                v = obj! { "value" => if c == 0 { Value::Null } else { Value::from(s / c as f64) } };
            }
        }
        let key = if typed_keys { k.clone() } else { name.to_string() };
        out.insert(key, v);
    }
    out
}

/// `mergeFinalResponse`: shards and timings of every response, no hits.
fn merge_final(live: Option<&Value>, rolled: &[Value], aggs: Option<Map>, total_as_int: bool) -> Value {
    let int = |v: &Value, p: &[&str]| v.pointer(p).and_then(Value::as_i64).unwrap_or(0);
    let mut took: i64 = rolled.iter().map(|r| int(r, &["took"])).sum();
    let mut timed_out = rolled.iter().any(|r| r.get("timed_out").and_then(Value::as_bool) == Some(true));
    let mut terminated = rolled.iter().any(|r| r.get("terminated_early").and_then(Value::as_bool) == Some(true));
    let phases = |r: &Value| r.get("num_reduce_phases").and_then(Value::as_i64).unwrap_or(1);
    let mut reduce_phases: i64 = rolled.iter().map(phases).sum();
    let mut shards = [0i64; 3];
    for r in rolled.iter().chain(live) {
        for (i, k) in ["total", "successful", "skipped"].iter().enumerate() {
            shards[i] += int(r, &["_shards", k]);
        }
    }
    if let Some(l) = live {
        took = took.max(int(l, &["took"]));
        timed_out = timed_out && l.get("timed_out").and_then(Value::as_bool) == Some(true);
        terminated = terminated && l.get("terminated_early").and_then(Value::as_bool) == Some(true);
        reduce_phases += phases(l);
    }
    let mut m = Map::new();
    m.insert("took".into(), took.into());
    m.insert("timed_out".into(), timed_out.into());
    m.insert("terminated_early".into(), terminated.into());
    if reduce_phases != 1 {
        m.insert("num_reduce_phases".into(), reduce_phases.into());
    }
    m.insert("_shards".into(), obj! { "total" => shards[0], "successful" => shards[1], "skipped" => shards[2], "failed" => 0 });
    let total = if total_as_int { Value::from(0i64) } else { obj! { "value" => 0, "relation" => "eq" } };
    m.insert("hits".into(), obj! { "total" => total, "max_score" => 0.0, "hits" => crate::arr![] });
    if let Some(a) = aggs.filter(|a| !a.is_empty()) {
        m.insert("aggregations".into(), Value::Obj(a));
    }
    Value::Obj(m)
}

/// The query parameters a plain search of the live indices keeps (the response's).
fn response_params(ctx: &Ctx, typed: bool) -> String {
    let mut out = Vec::new();
    if typed {
        out.push("typed_keys=true".to_string());
    } else if let Some(v) = ctx.query("typed_keys") {
        out.push(format!("typed_keys={}", enc(v)));
    }
    if let Some(v) = ctx.query("rest_total_hits_as_int") {
        out.push(format!("rest_total_hits_as_int={}", enc(v)));
    }
    if out.is_empty() { String::new() } else { format!("?{}", out.join("&")) }
}

/// `POST {index}/_rollup_search`.
pub async fn rollup_search(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.warn(DEPRECATION);
    let mut body = ctx.body_map()?.unwrap_or_default();
    if let Some(s) = ctx.query("size").and_then(crate::units::parse_java_int) {
        body.insert("size".into(), i64::from(s).into());
    }
    let st = ctx.state();
    let exprs: Vec<String> = ctx.path_param("index").unwrap_or("_all").split(',').map(str::to_string).collect();
    let opts = crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED.with_params(|k| ctx.query(k).map(str::to_string))?;
    let names = crate::meta::resolve::resolve(&st, &exprs, &opts, ctx.now_ms)?;
    if names.is_empty() {
        return Err(EsError::illegal_argument("Must specify at least one concrete index."));
    }
    let (mut live, mut rollup, mut caps) = (Vec::new(), Vec::new(), Vec::new());
    for name in &names {
        let jobs = st.metadata.indices.get(name).map(|idx| config::index_jobs(idx)).unwrap_or_default();
        if jobs.is_empty() {
            live.push(name.clone());
        } else {
            rollup.push(name.clone());
            caps.extend(jobs.iter().map(JobConfig::caps));
        }
    }
    drop(st);
    if rollup.len() > 1 {
        return Err(EsError::illegal_argument(format!(
            "RollupSearch currently only supports searching one rollup index at a time. Found the following rollup indices: [{}]",
            rollup.join(", ")
        )));
    }
    let caller = internal::Caller::of(ctx);
    let cluster = ctx.cluster;
    let search = |indices: &[String], typed: bool, body: Value| {
        let uri = format!("/{}/_search{}", enc(&indices.join(",")), response_params(ctx, typed));
        let caller = caller.clone();
        async move { internal::as_caller(caller, internal::call(cluster, "POST", &uri, Some(&body))).await }
    };
    if rollup.is_empty() {
        return Reply::ok(search(&live, false, Value::Obj(body)).await?);
    }
    validate(ctx, &body)?;
    let typed_keys = ctx.bool_param("typed_keys", false)?;
    let total_as_int = ctx.bool_param("rest_total_hits_as_int", false)?;
    let defs: Map = body.get("aggs").or_else(|| body.get("aggregations")).and_then(Value::as_object).cloned().unwrap_or_default();
    if defs.is_empty() {
        let live_resp = if live.is_empty() { None } else { Some(search(&live, true, Value::Obj(body.clone())).await?) };
        let rolled = search(&rollup, true, Value::Obj(body)).await?;
        return Reply::ok(merge_final(live_resp.as_ref(), &[rolled], None, total_as_int));
    }
    // The best jobs for every aggregation, and the rolled request.
    let cap_refs: Vec<&JobCaps> = caps.iter().collect();
    let mut best: Vec<JobCaps> = Vec::new();
    let mut rolled_aggs = Map::new();
    for (name, d) in &defs {
        let def = agg_def(name, d)?;
        find_best(&def, &cap_refs, &mut best)?;
    }
    for (name, d) in &defs {
        let def = agg_def(name, d)?;
        let mut subs = Map::new();
        for (k, v) in translate(&def)? {
            subs.insert(k, v);
        }
        rolled_aggs.insert(format!("filter_{name}"), obj! { "filter" => obj! { "bool" => obj! {} }, "aggs" => Value::Obj(subs) });
    }
    best.sort_by(|a, b| a.job_id.cmp(&b.job_id));
    let rewritten = rewrite_query(body.get("query"), &best)?;
    let live_resp = if live.is_empty() { None } else { Some(search(&live, true, Value::Obj(body.clone())).await?) };
    let mut rolled = Vec::new();
    for cap in &best {
        let query = obj! {
            "bool" => obj! {
                "must" => crate::arr![rewritten.clone()],
                "filter" => crate::arr![
                    obj! { "term" => obj! { "_rollup.id" => cap.job_id.clone() } },
                    obj! { "terms" => obj! { "_rollup.version" => crate::arr![1, 2] } },
                ],
            },
        };
        let req = obj! { "size" => 0, "query" => query, "aggs" => Value::Obj(rolled_aggs.clone()) };
        rolled.push(search(&rollup, true, req).await?);
    }
    // `doCombineResponse`.
    let live_aggs = live_resp.as_ref().and_then(|l| l.get("aggregations")).and_then(Value::as_object).cloned();
    let missing = rolled.iter().filter(|r| r.get("aggregations").and_then(Value::as_object).is_none_or(Map::is_empty)).count();
    if missing == rolled.len() {
        return Reply::ok(merge_final(live_resp.as_ref(), &rolled, None, total_as_int));
    }
    if missing > 0 {
        return Err(runtime("Expected to find aggregations in rollup response, but none found.".into()));
    }
    let mut current = Map::new();
    for r in &rolled {
        let mut unrolled = Map::new();
        for (k, filter) in r.get("aggregations").and_then(Value::as_object).into_iter().flatten() {
            if split(k).0 != "filter" {
                return Err(runtime(format!("Expected [{}] to be a FilterAggregation, but was [{}]", split(k).1, class_name(split(k).0, ""))));
            }
            unrolled.extend(unroll_list(&sub_aggs(filter), live_aggs.as_ref(), Some(&current))?);
        }
        current = reduce_maps(current, &unrolled);
    }
    if let Some(l) = live_aggs.as_ref().filter(|l| !l.is_empty()) {
        current = reduce_maps(current, l);
    }
    Reply::ok(merge_final(live_resp.as_ref(), &rolled, Some(finalize(current, &defs, typed_keys)), total_as_int))
}
