//! The query APIs over security objects (`_security/_query/user`, `/role`, `/api_key`): the
//! subset of the Query DSL Elasticsearch accepts there (`match_all`, `ids`, `term(s)`,
//! `prefix`, `wildcard`, `match`, `exists`, `range`, `simple_query_string`, `bool`), evaluated
//! over the objects' JSON, plus `sort`, `from`, `size` and `search_after`.
//!
//! The objects are few (they live in the cluster metadata), so a linear scan with an
//! interpreted query is the right tool; each API supplies a field accessor that maps the field
//! names Elasticsearch allows to values of the stored object.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use std::cmp::Ordering;

/// Reads a field of an object (`None`: the field doesn't exist for this object).
pub type Fields<'a> = &'a dyn Fn(&Value, &str) -> Vec<Value>;

/// A parsed `_query` request.
pub struct QueryRequest {
    pub query: Option<Value>,
    pub from: usize,
    pub size: usize,
    pub sort: Vec<(String, bool)>,
    pub search_after: Option<Vec<Value>>,
    /// `aggs` (the API key query aggregates its matches: [`aggregate`]).
    pub aggs: Option<Map>,
}

impl QueryRequest {
    pub fn parse(body: Option<&Value>, what: &str) -> EsResult<QueryRequest> {
        let mut r = QueryRequest { query: None, from: 0, size: 10, sort: Vec::new(), search_after: None, aggs: None };
        let Some(Value::Obj(m)) = body else { return Ok(r) };
        for (k, v) in m {
            match k.as_str() {
                "query" => r.query = Some(v.clone()),
                "from" => r.from = v.as_i64().filter(|n| *n >= 0).ok_or_else(|| EsError::illegal_argument("[from] parameter cannot be negative"))? as usize,
                "size" => r.size = v.as_i64().filter(|n| *n >= 0).ok_or_else(|| EsError::illegal_argument("[size] parameter cannot be negative"))? as usize,
                "sort" => r.sort = parse_sort(v)?,
                "search_after" => r.search_after = v.as_array().cloned(),
                "aggs" | "aggregations" => r.aggs = v.as_object().cloned(),
                "with_limited_by" | "with_profile_uid" | "typed_keys" => {}
                other => {
                    return Err(EsError::x_content_parse_bare(format!("[{what}] unknown field [{other}]")));
                }
            }
        }
        Ok(r)
    }
}

fn parse_sort(v: &Value) -> EsResult<Vec<(String, bool)>> {
    let one = |e: &Value| -> EsResult<(String, bool)> {
        match e {
            Value::Str(s) => Ok((s.clone(), true)),
            Value::Obj(m) if m.len() == 1 => {
                let (k, o) = m.iter().next().expect("one entry");
                let order = match o {
                    Value::Str(s) => s.as_str(),
                    Value::Obj(om) => om.get("order").and_then(Value::as_str).unwrap_or("asc"),
                    _ => "asc",
                };
                Ok((k.clone(), order != "desc"))
            }
            _ => Err(EsError::illegal_argument("malformed sort")),
        }
    };
    match v {
        Value::Arr(a) => a.iter().map(one).collect(),
        other => Ok(vec![one(other)?]),
    }
}

/// A page of hits, each with its sort values.
pub type Page = Vec<(Value, Vec<Value>)>;

/// Runs the request over `objects`: (total, the page with each hit's sort values).
pub fn run(req: &QueryRequest, objects: Vec<(String, Value)>, fields: Fields) -> EsResult<(usize, Page)> {
    let mut hits = Vec::new();
    for (id, obj) in objects {
        if let Some(q) = &req.query
            && !matches(q, &id, &obj, fields)?
        {
            continue;
        }
        hits.push((id, obj));
    }
    let total = hits.len();
    let mut keyed: Vec<(Vec<Value>, Value)> = hits
        .into_iter()
        .map(|(_, o)| {
            let keys = req.sort.iter().map(|(f, _)| fields(&o, f).into_iter().next().unwrap_or(Value::Null)).collect();
            (keys, o)
        })
        .collect();
    if !req.sort.is_empty() {
        keyed.sort_by(|a, b| {
            for (i, (_, asc)) in req.sort.iter().enumerate() {
                let c = cmp_values(&a.0[i], &b.0[i]);
                let c = if *asc { c } else { c.reverse() };
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        });
    }
    if let Some(after) = &req.search_after {
        keyed.retain(|(k, _)| {
            for (i, (_, asc)) in req.sort.iter().enumerate() {
                let Some(a) = after.get(i) else { return true };
                let c = cmp_values(&k[i], a);
                let c = if *asc { c } else { c.reverse() };
                if c != Ordering::Equal {
                    return c == Ordering::Greater;
                }
            }
            false
        });
    }
    let page = keyed.into_iter().skip(req.from).take(req.size).map(|(k, o)| (o, k)).collect();
    Ok((total, page))
}

fn cmp_values(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        (Value::Num(x), Value::Num(y)) => x.as_f64().partial_cmp(&y.as_f64()).unwrap_or(Ordering::Equal),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => a.to_es_string().cmp(&b.to_es_string()),
    }
}

fn unsupported(kind: &str) -> EsError {
    EsError::illegal_argument(format!("Query type [{kind}] is not currently supported in this context"))
}

/// `{field: value}` or `{field: {value|query: v, ..}}`.
fn field_and<'a>(q: &'a Value, inner: &str) -> Option<(&'a str, &'a Value)> {
    let m = q.as_object()?;
    let (f, v) = m.iter().find(|(k, _)| !matches!(k.as_str(), "boost" | "_name"))?;
    match v {
        Value::Obj(o) => Some((f.as_str(), o.get(inner).or_else(|| o.get("value")).unwrap_or(v))),
        other => Some((f.as_str(), other)),
    }
}

fn case_insensitive(q: &Value) -> bool {
    q.as_object().and_then(|m| m.values().next()).and_then(|v| v.get("case_insensitive")).and_then(Value::as_bool).unwrap_or(false)
}

fn eq_value(field_value: &Value, wanted: &Value, ci: bool) -> bool {
    match (field_value, wanted) {
        (Value::Num(a), Value::Num(b)) => a.as_f64() == b.as_f64(),
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Bool(a), Value::Str(b)) => a.to_string() == *b,
        (Value::Num(a), Value::Str(b)) => b.parse::<f64>().is_ok_and(|b| a.as_f64() == b),
        _ => {
            let (a, b) = (field_value.to_es_string(), wanted.to_es_string());
            if ci { a.eq_ignore_ascii_case(&b) } else { a == b }
        }
    }
}

fn millis_of(v: &Value) -> Option<f64> {
    match v {
        Value::Num(n) => Some(n.as_f64()),
        Value::Str(s) => s.parse::<f64>().ok().or_else(|| {
            let f = crate::time::DateFormatter::new("strict_date_optional_time||epoch_millis").ok()?;
            f.parse_millis(s).ok().map(|m| m as f64)
        }),
        _ => None,
    }
}

/// Whether an object matches a query.
pub fn matches(q: &Value, id: &str, obj: &Value, fields: Fields) -> EsResult<bool> {
    let Some(m) = q.as_object() else { return Err(EsError::parse("query malformed, must start with start_object")) };
    let Some((kind, body)) = m.iter().next() else { return Ok(true) };
    Ok(match kind.as_str() {
        "match_all" => true,
        "ids" => body.get("values").and_then(Value::as_array).is_some_and(|a| a.iter().any(|v| v.as_str() == Some(id))),
        "term" => {
            let Some((f, v)) = field_and(body, "value") else { return Ok(false) };
            let ci = case_insensitive(body);
            fields(obj, f).iter().any(|x| eq_value(x, v, ci))
        }
        "terms" => {
            let Some(bm) = body.as_object() else { return Ok(false) };
            let Some((f, v)) = bm.iter().find(|(k, _)| k.as_str() != "boost") else { return Ok(false) };
            let wanted = v.as_array().cloned().unwrap_or_default();
            fields(obj, f).iter().any(|x| wanted.iter().any(|w| eq_value(x, w, false)))
        }
        "prefix" => {
            let Some((f, v)) = field_and(body, "value") else { return Ok(false) };
            let ci = case_insensitive(body);
            let p = v.to_es_string();
            fields(obj, f).iter().any(|x| {
                let s = x.to_es_string();
                if ci { s.to_lowercase().starts_with(&p.to_lowercase()) } else { s.starts_with(&p) }
            })
        }
        "wildcard" => {
            let Some((f, v)) = field_and(body, "value") else { return Ok(false) };
            let ci = case_insensitive(body);
            let p = v.to_es_string();
            fields(obj, f).iter().any(|x| {
                let s = x.to_es_string();
                if ci { crate::xcontent::filter::glob_match(&p.to_lowercase(), &s.to_lowercase()) } else { crate::xcontent::filter::glob_match(&p, &s) }
            })
        }
        "match" => {
            let Some((f, v)) = field_and(body, "query") else { return Ok(false) };
            let wanted = v.to_es_string().to_lowercase();
            let terms: Vec<&str> = wanted.split_whitespace().collect();
            fields(obj, f).iter().any(|x| {
                let s = x.to_es_string().to_lowercase();
                s == wanted || terms.iter().any(|t| s.split(|c: char| !c.is_alphanumeric()).any(|w| w == *t))
            })
        }
        "simple_query_string" => {
            let text = body.get("query").map(Value::to_es_string).unwrap_or_default().to_lowercase();
            let in_fields: Vec<String> = body.get("fields").and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_else(|| vec!["*".into()]);
            let mut vals = Vec::new();
            for f in &in_fields {
                vals.extend(fields(obj, f));
            }
            text.split_whitespace().any(|t| {
                let t = t.trim_end_matches('*');
                vals.iter().any(|v| v.to_es_string().to_lowercase().contains(t))
            })
        }
        "exists" => {
            let f = body.get("field").map(Value::to_es_string).unwrap_or_default();
            fields(obj, &f).iter().any(|v| !v.is_null())
        }
        "range" => {
            let Some(bm) = body.as_object() else { return Ok(false) };
            let Some((f, Value::Obj(r))) = bm.iter().next() else { return Ok(false) };
            fields(obj, f).iter().any(|x| {
                let Some(xv) = millis_of(x) else { return false };
                r.iter().all(|(op, b)| {
                    let Some(bv) = millis_of(b) else { return true };
                    match op.as_str() {
                        "gt" => xv > bv,
                        "gte" => xv >= bv,
                        "lt" => xv < bv,
                        "lte" => xv <= bv,
                        _ => true,
                    }
                })
            })
        }
        "bool" => {
            let Some(bm) = body.as_object() else { return Ok(false) };
            let list = |k: &str| -> Vec<Value> {
                match bm.get(k) {
                    Some(Value::Arr(a)) => a.clone(),
                    Some(v @ Value::Obj(_)) => vec![v.clone()],
                    _ => Vec::new(),
                }
            };
            for c in list("must").iter().chain(list("filter").iter()) {
                if !matches(c, id, obj, fields)? {
                    return Ok(false);
                }
            }
            for c in list("must_not") {
                if matches(&c, id, obj, fields)? {
                    return Ok(false);
                }
            }
            let should = list("should");
            if !should.is_empty() {
                let required = bm.get("minimum_should_match").and_then(Value::as_i64).unwrap_or(if list("must").is_empty() && list("filter").is_empty() { 1 } else { 0 });
                let mut n = 0;
                for c in &should {
                    if matches(c, id, obj, fields)? {
                        n += 1;
                    }
                }
                return Ok(n >= required);
            }
            true
        }
        other => return Err(unsupported(other)),
    })
}

/// A field accessor over plain object paths (`metadata.a.b`), with `*` matching any field.
pub fn path_values(obj: &Value, field: &str) -> Vec<Value> {
    if field == "*" {
        return obj.as_object().map(|m| m.values().filter(|v| !matches!(v, Value::Obj(_) | Value::Arr(_))).cloned().collect()).unwrap_or_default();
    }
    let mut cur = vec![obj.clone()];
    for part in field.split('.') {
        let mut next = Vec::new();
        for v in cur {
            match v.get(part) {
                Some(Value::Arr(a)) => next.extend(a.iter().cloned()),
                Some(x) => next.push(x.clone()),
                None => {}
            }
        }
        cur = next;
    }
    cur
}

/// Adds the `_sort` values to a hit (when the request sorted).
pub fn with_sort(mut v: Value, keys: Vec<Value>, sorted: bool) -> Value {
    if sorted && let Value::Obj(m) = &mut v {
        m.insert("_sort".into(), Value::Arr(keys));
    }
    v
}

/// The `aggregations` of the objects the request's query matches (`ApiKeyAggregationsBuilder`'s
/// subset that roll evaluates over the objects: `terms`, `composite` of `terms` sources, and
/// `value_count`).
pub fn aggregate(req: &QueryRequest, aggs: &Map, objects: &[(String, Value)], fields: Fields) -> EsResult<Value> {
    let mut hits = Vec::new();
    for (id, obj) in objects {
        if req.query.as_ref().map(|q| matches(q, id, obj, fields)).transpose()?.unwrap_or(true) {
            hits.push(obj);
        }
    }
    let field_of = |body: &Value| body.get("field").map(Value::to_es_string).unwrap_or_default();
    let size_of = |body: &Value| body.get("size").and_then(Value::as_i64).unwrap_or(10).max(0) as usize;
    let mut out = Map::new();
    for (name, agg) in aggs {
        let Some((ty, body)) = agg.as_object().and_then(|m| m.iter().find(|(k, _)| !matches!(k.as_str(), "meta" | "aggs" | "aggregations"))) else { continue };
        let result = match ty.as_str() {
            "terms" => {
                let f = field_of(body);
                let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
                for h in &hits {
                    for v in fields(h, &f) {
                        *counts.entry(v.to_es_string()).or_default() += 1;
                    }
                }
                let mut buckets: Vec<(String, usize)> = counts.into_iter().collect();
                buckets.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                let size = size_of(body);
                let other: usize = buckets.iter().skip(size).map(|b| b.1).sum();
                buckets.truncate(size);
                crate::obj! {
                    "doc_count_error_upper_bound" => 0,
                    "sum_other_doc_count" => other,
                    "buckets" => Value::Arr(buckets.into_iter().map(|(k, c)| crate::obj! { "key" => k, "doc_count" => c }).collect()),
                }
            }
            "composite" => {
                let mut sources: Vec<(String, String)> = Vec::new();
                for src in body.get("sources").and_then(Value::as_array).into_iter().flatten() {
                    let Some((sname, sdef)) = src.as_object().and_then(|m| m.iter().next()) else { continue };
                    let Some(t) = sdef.get("terms") else {
                        return Err(EsError::illegal_argument(format!("Unsupported API Keys agg [{name}] of type [composite]")));
                    };
                    sources.push((sname.clone(), field_of(t)));
                }
                // Every combination of the sources' values of each object.
                let mut counts: std::collections::BTreeMap<Vec<String>, usize> = std::collections::BTreeMap::new();
                for h in &hits {
                    let mut keys: Vec<Vec<String>> = vec![Vec::new()];
                    for (_, f) in &sources {
                        let vals: Vec<String> = fields(h, f).iter().map(Value::to_es_string).collect();
                        keys = keys.into_iter().flat_map(|k| vals.iter().map(move |v| [k.clone(), vec![v.clone()]].concat())).collect();
                    }
                    for k in keys {
                        *counts.entry(k).or_default() += 1;
                    }
                }
                let after: Option<Vec<String>> = body.get("after").map(|a| sources.iter().map(|(n, _)| a.get(n).map(Value::to_es_string).unwrap_or_default()).collect());
                let key_obj = |k: &[String]| Value::Obj(sources.iter().zip(k).map(|((n, _), v)| (n.clone(), Value::from(v.clone()))).collect());
                let buckets: Vec<(Vec<String>, usize)> = counts.into_iter().filter(|(k, _)| after.as_ref().is_none_or(|a| k > a)).take(size_of(body)).collect();
                let mut m = Map::new();
                if let Some((last, _)) = buckets.last() {
                    m.insert("after_key".into(), key_obj(last));
                }
                m.insert("buckets".into(), Value::Arr(buckets.iter().map(|(k, c)| crate::obj! { "key" => key_obj(k), "doc_count" => *c }).collect()));
                Value::Obj(m)
            }
            "value_count" => {
                let f = field_of(body);
                crate::obj! { "value" => hits.iter().map(|h| fields(h, &f).len()).sum::<usize>() }
            }
            other => return Err(EsError::illegal_argument(format!("Unsupported API Keys agg [{name}] of type [{other}]"))),
        };
        out.insert(name.clone(), result);
    }
    Ok(Value::Obj(out))
}

/// `{"total": n, "count": m, <key>: [..]}`.
pub fn response(total: usize, key: &str, hits: Vec<Value>) -> Value {
    let mut m = Map::new();
    m.insert("total".into(), total.into());
    m.insert("count".into(), hits.len().into());
    m.insert(key.into(), Value::Arr(hits));
    Value::Obj(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obj;

    fn objs() -> Vec<(String, Value)> {
        vec![
            ("a".into(), obj! { "name" => "admin_role", "n" => 3 }),
            ("b".into(), obj! { "name" => "beta", "n" => 1 }),
            ("c".into(), obj! { "name" => "Gamma ray", "n" => 2 }),
        ]
    }

    fn q(v: Value) -> QueryRequest {
        QueryRequest::parse(Some(&v), "query").unwrap()
    }

    #[test]
    fn queries_and_sorts() {
        let f: Fields = &path_values;
        let (total, page) = run(&q(obj! { "query" => obj! { "match_all" => obj! {} }, "sort" => crate::arr!["n"] }), objs(), f).unwrap();
        assert_eq!(total, 3);
        assert_eq!(page.iter().map(|(o, _)| o.get("name").unwrap().to_es_string()).collect::<Vec<_>>(), ["beta", "Gamma ray", "admin_role"]);
        let (total, _) = run(&q(obj! { "query" => obj! { "ids" => obj! { "values" => crate::arr!["b"] } } }), objs(), f).unwrap();
        assert_eq!(total, 1);
        let (total, _) = run(&q(obj! { "query" => obj! { "prefix" => obj! { "name" => "a" } } }), objs(), f).unwrap();
        assert_eq!(total, 1);
        let (total, _) = run(&q(obj! { "query" => obj! { "range" => obj! { "n" => obj! { "gte" => 2 } } } }), objs(), f).unwrap();
        assert_eq!(total, 2);
        let (total, _) = run(&q(obj! { "query" => obj! { "bool" => obj! { "must_not" => obj! { "term" => obj! { "name" => "beta" } } } } }), objs(), f).unwrap();
        assert_eq!(total, 2);
        let (_, page) = run(&q(obj! { "sort" => crate::arr![obj! { "n" => "desc" }], "size" => 1, "search_after" => crate::arr![3] }), objs(), f).unwrap();
        assert_eq!(page[0].0.get("n"), Some(&Value::from(2)));
        let e = run(&q(obj! { "query" => obj! { "fuzzy" => obj! {} } }), objs(), f).err().unwrap();
        assert_eq!(e.reason(), "Query type [fuzzy] is not currently supported in this context");
    }
}
