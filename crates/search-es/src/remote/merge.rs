//! Merging the aggregations several clusters answered (a search over linked remote clusters,
//! [`super::links::search`]), from their rendered, typed (`typed_keys`) results.
//!
//! Elasticsearch merges remote clusters' partially reduced `InternalAggregations`; roll's
//! clusters answer each other in JSON, which keeps what these need: bucket aggregations merge
//! their buckets by key (counts added, sub-aggregations merged the same way), `min`, `max`,
//! `sum`, `value_count` and `stats` merge their values. Anything else (an `avg`, a
//! `cardinality`, percentiles: their mergeable state isn't in the JSON) is refused with a 400
//! naming it, never answered wrong.

use std::cmp::Ordering;

use super::links::cmp_sort_value;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Number, Value};

fn unsupported(name: &str, ty: &str) -> EsError {
    EsError::illegal_argument(format!(
        "roll can't merge aggregation [{name}] of type [{ty}] across clusters yet: run it on one cluster, or search the clusters one at a time"
    ))
}

/// The request of aggregation `name` in an `aggs` object: `(type, body, sub-aggregations)`.
fn request_of<'a>(request: &'a Value, name: &str) -> (Option<&'a Value>, Value) {
    let Some(r) = request.get(name) else { return (None, Value::object()) };
    let subs = r.get("aggs").or_else(|| r.get("aggregations")).cloned().unwrap_or_else(Value::object);
    let body = r.as_object().and_then(|m| m.iter().find(|(k, _)| !matches!(k.as_str(), "aggs" | "aggregations" | "meta")).map(|(_, v)| v));
    (body, subs)
}

/// Merges the typed aggregations of several clusters (`parts`, each `{type#name: result}`).
pub fn aggs(parts: &[Value], request: &Value) -> EsResult<Value> {
    let mut keys: Vec<String> = Vec::new();
    for p in parts {
        for k in p.as_object().into_iter().flat_map(|m| m.keys()) {
            if !keys.contains(k) {
                keys.push(k.clone());
            }
        }
    }
    let mut out = Map::new();
    for k in keys {
        let Some((ty, name)) = k.split_once('#') else { continue };
        let values: Vec<&Value> = parts.iter().filter_map(|p| p.get(&k)).collect();
        let (body, subs) = request_of(request, name);
        let merged = agg(name, ty, &values, body, &subs)?;
        // Unmapped terms take the mapped clusters' type.
        let ty = if ty == "umterms" {
            parts.iter().flat_map(|p| p.as_object().into_iter().flat_map(|m| m.keys())).filter_map(|k| k.split_once('#')).find(|(t, n)| *n == name && *t != "umterms").map_or(ty, |(t, _)| t)
        } else {
            ty
        };
        let key = format!("{ty}#{name}");
        if out.contains_key(&key) {
            continue;
        }
        out.insert(key, merged);
    }
    Ok(Value::Obj(out))
}

/// The sub-aggregations (`type#name` keys) of several buckets or single-bucket results.
fn subs_of(values: &[&Value], subs: &Value) -> EsResult<Map> {
    let parts: Vec<Value> = values
        .iter()
        .map(|v| Value::Obj(v.as_object().map(|m| m.iter().filter(|(k, _)| k.contains('#')).map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default()))
        .collect();
    match aggs(&parts, subs)? {
        Value::Obj(m) => Ok(m),
        _ => Ok(Map::new()),
    }
}

fn num(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(Value::as_f64)
}

fn int(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(Value::as_i64).unwrap_or(0)
}

fn float(f: f64) -> Value {
    Value::Num(Number::Float(f))
}

fn agg(name: &str, ty: &str, values: &[&Value], body: Option<&Value>, subs: &Value) -> EsResult<Value> {
    let meta = values.iter().find_map(|v| v.get("meta")).cloned();
    let mut m = Map::new();
    if let Some(meta) = meta {
        m.insert("meta".into(), meta);
    }
    match ty {
        "min" | "max" => {
            let pick = values
                .iter()
                .filter(|v| v.get("value").is_some_and(|x| !x.is_null()))
                .min_by(|a, b| {
                    let o = num(a, "value").partial_cmp(&num(b, "value")).unwrap_or(Ordering::Equal);
                    if ty == "min" { o } else { o.reverse() }
                });
            match pick {
                Some(p) => {
                    m.insert("value".into(), p.get("value").cloned().unwrap_or(Value::Null));
                    if let Some(s) = p.get("value_as_string") {
                        m.insert("value_as_string".into(), s.clone());
                    }
                }
                None => {
                    m.insert("value".into(), Value::Null);
                }
            }
        }
        "sum" => {
            m.insert("value".into(), float(values.iter().filter_map(|v| num(v, "value")).sum()));
        }
        "value_count" => {
            m.insert("value".into(), Value::from(values.iter().map(|v| int(v, "value")).sum::<i64>()));
        }
        "stats" => {
            let count: i64 = values.iter().map(|v| int(v, "count")).sum();
            let min = values.iter().filter_map(|v| num(v, "min")).fold(None, |a: Option<f64>, x| Some(a.map_or(x, |a| a.min(x))));
            let max = values.iter().filter_map(|v| num(v, "max")).fold(None, |a: Option<f64>, x| Some(a.map_or(x, |a| a.max(x))));
            let sum: f64 = values.iter().filter_map(|v| num(v, "sum")).sum();
            m.insert("count".into(), Value::from(count));
            m.insert("min".into(), min.map_or(Value::Null, float));
            m.insert("max".into(), max.map_or(Value::Null, float));
            m.insert("avg".into(), if count > 0 { float(sum / count as f64) } else { Value::Null });
            m.insert("sum".into(), float(sum));
        }
        "filter" | "missing" | "global" | "nested" | "reverse_nested" | "sampler" | "diversified_sampler" | "random_sampler" => {
            m.insert("doc_count".into(), Value::from(values.iter().map(|v| int(v, "doc_count")).sum::<i64>()));
            m.extend(subs_of(values, subs)?);
        }
        "filters" | "range" | "date_range" | "ip_range" | "geo_distance" => {
            m.insert("buckets".into(), positional(name, ty, values, subs)?);
        }
        "sterms" | "lterms" | "dterms" | "umterms" => terms(name, ty, values, body, subs, &mut m)?,
        "histogram" | "date_histogram" => histogram(name, ty, values, body, subs, &mut m)?,
        _ => return Err(unsupported(name, ty)),
    }
    Ok(Value::Obj(m))
}

/// Buckets that line up by position or key (`filters`, ranges): counts added.
fn positional(name: &str, ty: &str, values: &[&Value], subs: &Value) -> EsResult<Value> {
    let lists: Vec<&Value> = values.iter().filter_map(|v| v.get("buckets")).collect();
    let Some(first) = lists.first() else { return Ok(Value::Arr(Vec::new())) };
    let merge_bucket = |template: &Value, others: Vec<&Value>| -> EsResult<Value> {
        let mut b = Map::new();
        for (k, v) in template.as_object().into_iter().flatten() {
            if k != "doc_count" && !k.contains('#') {
                b.insert(k.clone(), v.clone());
            }
        }
        b.insert("doc_count".into(), Value::from(others.iter().map(|v| int(v, "doc_count")).sum::<i64>()));
        b.extend(subs_of(&others, subs)?);
        Ok(Value::Obj(b))
    };
    match first {
        Value::Obj(keyed) => {
            let mut out = Map::new();
            for k in keyed.keys() {
                let others: Vec<&Value> = lists.iter().filter_map(|l| l.get(k)).collect();
                out.insert(k.clone(), merge_bucket(&keyed[k], others)?);
            }
            Ok(Value::Obj(out))
        }
        Value::Arr(list) => {
            let mut out = Vec::new();
            for (i, t) in list.iter().enumerate() {
                let others: Vec<&Value> = lists.iter().filter_map(|l| l.as_array().and_then(|a| a.get(i))).collect();
                if others.len() != lists.len() {
                    return Err(unsupported(name, ty));
                }
                out.push(merge_bucket(t, others)?);
            }
            Ok(Value::Arr(out))
        }
        _ => Err(unsupported(name, ty)),
    }
}

/// Buckets merged by `key`, in first-seen order: `(key, buckets with that key)`.
fn by_key<'a>(values: &[&'a Value]) -> Option<Vec<(Value, Vec<&'a Value>)>> {
    let mut out: Vec<(Value, Vec<&'a Value>)> = Vec::new();
    for v in values {
        let buckets = v.get("buckets")?.as_array()?;
        for b in buckets {
            let key = b.get("key")?.clone();
            match out.iter_mut().find(|(k, _)| cmp_sort_value(k, &key) == Ordering::Equal) {
                Some((_, list)) => list.push(b),
                None => out.push((key, vec![b])),
            }
        }
    }
    Some(out)
}

fn merged_bucket(key: &Value, list: &[&Value], subs: &Value) -> EsResult<(Value, i64)> {
    let mut b = Map::new();
    b.insert("key".into(), key.clone());
    if let Some(s) = list.iter().find_map(|x| x.get("key_as_string")) {
        b.insert("key_as_string".into(), s.clone());
    }
    let count: i64 = list.iter().map(|x| int(x, "doc_count")).sum();
    b.insert("doc_count".into(), Value::from(count));
    if list.iter().any(|x| x.get("doc_count_error_upper_bound").is_some()) {
        b.insert("doc_count_error_upper_bound".into(), Value::from(list.iter().map(|x| int(x, "doc_count_error_upper_bound")).sum::<i64>()));
    }
    b.extend(subs_of(list, subs)?);
    Ok((Value::Obj(b), count))
}

/// `terms`: counts added per key, ordered by the request's `order` (counts and keys), cut to
/// its `size`; what's cut counts in `sum_other_doc_count`.
fn terms(name: &str, ty: &str, values: &[&Value], body: Option<&Value>, subs: &Value, m: &mut Map) -> EsResult<()> {
    let body = body.cloned().unwrap_or_else(Value::object);
    let size = body.get("size").and_then(Value::as_i64).unwrap_or(10).max(0) as usize;
    let min_doc_count = body.get("min_doc_count").and_then(Value::as_i64).unwrap_or(1);
    // [(by count, descending)]: `_count desc, _key asc` by default.
    let mut order: Vec<(bool, bool)> = Vec::new();
    let entries: Vec<(String, String)> = match body.get("order") {
        None => Vec::new(),
        Some(Value::Obj(o)) => o.iter().map(|(k, v)| (k.clone(), v.to_es_string())).collect(),
        Some(Value::Arr(a)) => a.iter().filter_map(Value::as_object).flat_map(|o| o.iter().map(|(k, v)| (k.clone(), v.to_es_string()))).collect(),
        Some(_) => return Err(unsupported(name, ty)),
    };
    for (k, dir) in entries {
        let desc = dir.eq_ignore_ascii_case("desc");
        match k.as_str() {
            "_count" => order.push((true, desc)),
            "_key" | "_term" => order.push((false, desc)),
            _ => return Err(unsupported(name, ty)),
        }
    }
    if order.is_empty() {
        order.push((true, true));
    }
    if order.iter().all(|(by_count, _)| *by_count) {
        order.push((false, false));
    }
    let groups = by_key(values).ok_or_else(|| unsupported(name, ty))?;
    let mut buckets: Vec<(Value, i64, Value)> = Vec::new();
    for (key, list) in groups {
        let (b, count) = merged_bucket(&key, &list, subs)?;
        if count >= min_doc_count {
            buckets.push((key, count, b));
        }
    }
    buckets.sort_by(|a, b| {
        for (by_count, desc) in &order {
            let o = if *by_count { a.1.cmp(&b.1) } else { cmp_sort_value(&a.0, &b.0) };
            let o = if *desc { o.reverse() } else { o };
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    });
    let other: i64 = values.iter().map(|v| int(v, "sum_other_doc_count")).sum::<i64>() + buckets.iter().skip(size).map(|b| b.1).sum::<i64>();
    buckets.truncate(size);
    m.insert("doc_count_error_upper_bound".into(), Value::from(values.iter().map(|v| int(v, "doc_count_error_upper_bound")).sum::<i64>()));
    m.insert("sum_other_doc_count".into(), Value::from(other));
    m.insert("buckets".into(), Value::Arr(buckets.into_iter().map(|b| b.2).collect()));
    Ok(())
}

/// A fixed interval in milliseconds (`1d`, `90m`, `30s`, `500ms`).
fn fixed_ms(s: &str) -> Option<f64> {
    let units: [(&str, f64); 6] = [("ms", 1.0), ("s", 1e3), ("m", 6e4), ("h", 3.6e6), ("d", 8.64e7), ("w", 6.048e8)];
    let (digits, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: f64 = digits.parse().ok()?;
    units.iter().find(|(u, _)| *u == unit).map(|(_, f)| n * f)
}

/// `histogram` and `date_histogram` (key order): counts added per key; with `min_doc_count:
/// 0` (the default) the empty buckets between the clusters' ranges are filled in.
fn histogram(name: &str, ty: &str, values: &[&Value], body: Option<&Value>, subs: &Value, m: &mut Map) -> EsResult<()> {
    let body = body.cloned().unwrap_or_else(Value::object);
    if body.get("order").is_some() || body.get("keyed").and_then(Value::as_bool) == Some(true) {
        return Err(unsupported(name, ty));
    }
    let min_doc_count = body.get("min_doc_count").and_then(Value::as_i64).unwrap_or(0);
    let groups = by_key(values).ok_or_else(|| unsupported(name, ty))?;
    let mut buckets: Vec<(f64, Value)> = Vec::new();
    for (key, list) in groups {
        let (b, _) = merged_bucket(&key, &list, subs)?;
        buckets.push((key.as_f64().unwrap_or(f64::NAN), b));
    }
    buckets.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
    if min_doc_count == 0 && buckets.len() > 1 {
        let step = match ty {
            "histogram" => body.get("interval").and_then(Value::as_f64),
            _ => body.get("fixed_interval").or_else(|| body.get("interval")).and_then(|v| v.as_str()).and_then(fixed_ms),
        };
        let gap = |a: f64, b: f64, step: Option<f64>| match step {
            Some(s) if s > 0.0 => (b - a) > s * 1.5,
            _ => false,
        };
        let has_gap = buckets.windows(2).any(|w| gap(w[0].0, w[1].0, step));
        if has_gap {
            // Empty buckets need a key as the cluster renders it: plain numeric keys only.
            let plain = buckets.iter().all(|(_, b)| b.get("key_as_string").is_none());
            let Some(step) = step.filter(|_| plain) else { return Err(unsupported(name, ty)) };
            let mut filled: Vec<(f64, Value)> = Vec::new();
            for (k, b) in buckets {
                if let Some((last, _)) = filled.last() {
                    let mut next = *last + step;
                    while next < k - step * 0.5 {
                        let mut e = Map::new();
                        e.insert("key".into(), if ty == "histogram" { float(next) } else { Value::from(next as i64) });
                        e.insert("doc_count".into(), Value::from(0i64));
                        e.extend(subs_of(&[], subs)?);
                        filled.push((next, Value::Obj(e)));
                        next += step;
                        if filled.len() > 65_536 {
                            return Err(unsupported(name, ty));
                        }
                    }
                }
                filled.push((k, b));
            }
            buckets = filled;
        } else if step.is_none() {
            // A calendar interval: clusters' ranges that don't overlap may leave a gap roll
            // can't fill without the calendar.
            let ranges: Vec<(f64, f64)> = values
                .iter()
                .filter_map(|v| {
                    let keys: Vec<f64> = v.get("buckets")?.as_array()?.iter().filter_map(|b| b.get("key").and_then(Value::as_f64)).collect();
                    Some((keys.iter().cloned().fold(f64::INFINITY, f64::min), keys.iter().cloned().fold(f64::NEG_INFINITY, f64::max)))
                })
                .filter(|(a, b)| a <= b)
                .collect();
            let disjoint = ranges.iter().enumerate().any(|(i, a)| ranges.iter().skip(i + 1).any(|b| a.1 < b.0 || b.1 < a.0));
            if disjoint {
                return Err(unsupported(name, ty));
            }
        }
    }
    m.insert("buckets".into(), Value::Arr(buckets.into_iter().filter(|(_, b)| int(b, "doc_count") >= min_doc_count).map(|(_, b)| b).collect()));
    Ok(())
}

/// Drops the `type#` prefixes of a merged result's aggregation names (a request without
/// `typed_keys`).
pub fn untyped(v: Value) -> Value {
    fn level(m: Map) -> Map {
        m.into_iter()
            .map(|(k, v)| match k.split_once('#') {
                Some((_, name)) => (name.to_string(), agg(v)),
                None => (k, v),
            })
            .collect()
    }
    fn agg(v: Value) -> Value {
        let Value::Obj(m) = v else { return v };
        let m: Map = m
            .into_iter()
            .map(|(k, v)| {
                if k == "buckets" {
                    let v = match v {
                        Value::Arr(a) => Value::Arr(a.into_iter().map(|b| match b {
                            Value::Obj(b) => Value::Obj(level(b)),
                            b => b,
                        }).collect()),
                        Value::Obj(o) => Value::Obj(o.into_iter().map(|(k, b)| (k, match b {
                            Value::Obj(b) => Value::Obj(level(b)),
                            b => b,
                        })).collect()),
                        v => v,
                    };
                    (k, v)
                } else if let Some((_, name)) = k.split_once('#') {
                    (name.to_string(), agg(v))
                } else {
                    (k, v)
                }
            })
            .collect();
        Value::Obj(m)
    }
    match v {
        Value::Obj(m) => Value::Obj(level(m)),
        v => v,
    }
}

/// Suggestions from several clusters: not merged yet.
pub fn suggest(_parts: &[Value], _source: &crate::search_source::SearchSource) -> EsResult<Value> {
    Err(EsError::illegal_argument("roll can't merge suggestions across clusters yet: run the suggester on one cluster"))
}
