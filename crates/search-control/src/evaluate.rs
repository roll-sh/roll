//! Offline relevance evaluation (docs/search/relevance-experiments.md, section 1):
//! `POST /v1/search/clusters/{id}/indices/{index}/evaluate` scores one or two search
//! configurations against a judgment list with the cluster's own `_rank_eval`, and compares the
//! two query by query: which queries got better, which worse. It's what a pipeline runs before a
//! relevance change ships (`roll search evaluate --fail-below`), and what the Relevance tab shows.
//!
//! A configuration is a search body with `{{query}}` where each judged query goes (`request`), or
//! a search template (`template`: a stored one by `id`, or an inline `source`, with `params`).
//! Each becomes one `_rank_eval` call on the index, its requests the judged queries rendered
//! through it, so the cluster checks and runs them as it runs any search of that index.
//!
//! Nothing here stores anything: judgments come in the request.

use serde_json::{Map, Value, json};

/// Judged queries one evaluation takes.
pub const MAX_QUERIES: usize = 500;
/// Rated documents one judged query takes.
pub const MAX_RATINGS: usize = 200;
/// Queries listed as better or worse in a comparison.
pub const LIST_ROWS: usize = 50;
/// What a configuration's template parameter holding the query is called unless it says.
pub const QUERY_PARAM: &str = "query";

/// One judged query: its text and the rated documents (`index` empty: the evaluated index).
#[derive(Clone, Debug, PartialEq)]
pub struct Judgment {
    pub query: String,
    pub ratings: Vec<(String, String, i64)>,
}

/// One configuration: a name and the template each judged query is rendered through.
#[derive(Clone, Debug, PartialEq)]
pub struct Configuration {
    pub name: String,
    /// `_rank_eval`'s `templates[].template`: `{source}` or `{id}`.
    pub template: Value,
    /// Parameters every query's rendering gets, under the query's own.
    pub params: Map<String, Value>,
    pub query_param: String,
}

/// A parsed `evaluate` body.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub judgments: Vec<Judgment>,
    pub configurations: Vec<Configuration>,
    /// `_rank_eval`'s `metric`.
    pub metric: Value,
}

fn bad(m: impl Into<String>) -> String {
    m.into()
}

fn only(m: &Map<String, Value>, what: &str, keys: &[&str]) -> Result<(), String> {
    match m.keys().find(|k| !keys.contains(&k.as_str())) {
        Some(k) => Err(bad(format!("{what} has no field [{k}] (it takes {})", keys.join(", ")))),
        None => Ok(()),
    }
}

fn parse_judgment(v: &Value, i: usize) -> Result<Judgment, String> {
    let Some(m) = v.as_object() else { return Err(bad(format!("[judgments][{i}] is an object {{query, ratings}}"))) };
    only(m, &format!("[judgments][{i}]"), &["query", "ratings"])?;
    let query = m.get("query").and_then(Value::as_str).filter(|q| !q.is_empty() && q.len() <= 1024).ok_or_else(|| bad(format!("[judgments][{i}][query] is text of 1 to 1,024 bytes")))?;
    let Some(rs) = m.get("ratings").and_then(Value::as_array).filter(|r| !r.is_empty()) else {
        return Err(bad(format!("[judgments][{i}][ratings] is a list of {{_id, rating, _index?}}")));
    };
    if rs.len() > MAX_RATINGS {
        return Err(bad(format!("[judgments][{i}] rates {} documents: at most {MAX_RATINGS}", rs.len())));
    }
    let mut ratings = Vec::with_capacity(rs.len());
    for (j, r) in rs.iter().enumerate() {
        let Some(r) = r.as_object() else { return Err(bad(format!("[judgments][{i}][ratings][{j}] is an object {{_id, rating, _index?}}"))) };
        only(r, &format!("[judgments][{i}][ratings][{j}]"), &["_id", "rating", "_index"])?;
        let id = r.get("_id").and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= 512).ok_or_else(|| bad(format!("[judgments][{i}][ratings][{j}][_id] is a document id")))?;
        let rating = r.get("rating").and_then(Value::as_i64).filter(|n| (0..=100).contains(n)).ok_or_else(|| bad(format!("[judgments][{i}][ratings][{j}][rating] is a whole number from 0 to 100")))?;
        let index = match r.get("_index") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) if !s.is_empty() && s.len() <= 255 => s.clone(),
            Some(_) => return Err(bad(format!("[judgments][{i}][ratings][{j}][_index] is an index name"))),
        };
        ratings.push((index, id.to_string(), rating));
    }
    Ok(Judgment { query: query.to_string(), ratings })
}

fn parse_configuration(v: &Value, i: usize) -> Result<Configuration, String> {
    let Some(m) = v.as_object() else { return Err(bad(format!("[configurations][{i}] is an object {{name, request | template}}"))) };
    only(m, &format!("[configurations][{i}]"), &["name", "request", "template", "query_param"])?;
    let name = m.get("name").and_then(Value::as_str).filter(|n| !n.is_empty() && n.len() <= 64).ok_or_else(|| bad(format!("[configurations][{i}][name] is 1 to 64 characters")))?;
    let query_param = match m.get("query_param") {
        None | Some(Value::Null) => QUERY_PARAM.to_string(),
        Some(Value::String(p)) if !p.is_empty() && p.len() <= 64 => p.clone(),
        Some(_) => return Err(bad(format!("[configurations][{i}][query_param] is a parameter name"))),
    };
    let (template, params) = match (m.get("request"), m.get("template")) {
        (Some(Value::Object(r)), None) => (json!({ "source": Value::Object(r.clone()) }), Map::new()),
        (None, Some(Value::Object(t))) => {
            only(t, &format!("[configurations][{i}][template]"), &["id", "source", "params"])?;
            let source = match (t.get("id"), t.get("source")) {
                (Some(Value::String(id)), None) if !id.is_empty() => json!({ "id": id }),
                (None, Some(s @ (Value::Object(_) | Value::String(_)))) => json!({ "source": s }),
                _ => return Err(bad(format!("[configurations][{i}][template] names a stored template by [id] or gives its [source], one of them"))),
            };
            let params = match t.get("params") {
                None | Some(Value::Null) => Map::new(),
                Some(Value::Object(p)) => p.clone(),
                Some(_) => return Err(bad(format!("[configurations][{i}][template][params] is an object"))),
            };
            (source, params)
        }
        _ => return Err(bad(format!("[configurations][{i}] gives a search body with {{{{query}}}} ([request]) or a search template ([template]), one of them"))),
    };
    Ok(Configuration { name: name.to_string(), template, params, query_param })
}

/// Parses an `evaluate` body: `{judgments: [{query, ratings: [{_id, rating, _index?}]}],
/// configurations: [{name, request | template, query_param?}] (one or two), metric?}`, the
/// metric `_rank_eval`'s (by default nDCG at 10).
pub fn parse(body: &[u8]) -> Result<Spec, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| bad(format!("the body isn't JSON: {e}")))?;
    let Some(m) = v.as_object() else { return Err(bad("the body is an object {judgments, configurations, metric?}")) };
    only(m, "the body", &["judgments", "configurations", "metric"])?;
    let Some(js) = m.get("judgments").and_then(Value::as_array).filter(|j| !j.is_empty()) else {
        return Err(bad("[judgments] is a list of {query, ratings}"));
    };
    if js.len() > MAX_QUERIES {
        return Err(bad(format!("[judgments] holds {} queries: at most {MAX_QUERIES}", js.len())));
    }
    let judgments = js.iter().enumerate().map(|(i, j)| parse_judgment(j, i)).collect::<Result<Vec<_>, _>>()?;
    let Some(cs) = m.get("configurations").and_then(Value::as_array).filter(|c| (1..=2).contains(&c.len())) else {
        return Err(bad("[configurations] is a list of one or two {name, request | template}"));
    };
    let configurations = cs.iter().enumerate().map(|(i, c)| parse_configuration(c, i)).collect::<Result<Vec<_>, _>>()?;
    if configurations.len() == 2 && configurations[0].name == configurations[1].name {
        return Err(bad("the two configurations need names of their own"));
    }
    let metric = match m.get("metric") {
        None | Some(Value::Null) => json!({ "dcg": { "k": 10, "normalize": true } }),
        Some(Value::Object(o)) if o.len() == 1 => Value::Object(o.clone()),
        Some(_) => return Err(bad("[metric] names one of _rank_eval's metrics: {\"dcg\": {\"k\": 10, \"normalize\": true}}, precision, recall, mean_reciprocal_rank or expected_reciprocal_rank")),
    };
    Ok(Spec { judgments, configurations, metric })
}

/// The `_rank_eval` body that scores configuration `c` on `index`: one templated request per
/// judged query (ids `q0`, `q1` … in judgment order), its ratings on `index` unless they name one.
pub fn rank_eval_body(spec: &Spec, c: &Configuration, index: &str) -> Value {
    let requests: Vec<Value> = spec
        .judgments
        .iter()
        .enumerate()
        .map(|(i, j)| {
            let mut params = c.params.clone();
            params.insert(c.query_param.clone(), Value::String(j.query.clone()));
            let ratings: Vec<Value> = j
                .ratings
                .iter()
                .map(|(ix, id, r)| json!({ "_index": if ix.is_empty() { index } else { ix.as_str() }, "_id": id, "rating": r }))
                .collect();
            json!({ "id": format!("q{i}"), "template_id": "configuration", "params": params, "ratings": ratings })
        })
        .collect();
    json!({ "metric": spec.metric, "templates": [{ "id": "configuration", "template": c.template }], "requests": requests })
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

/// One configuration's result from its `_rank_eval` answer: its score, and each judged query's
/// score, unrated hits and failure.
fn configuration_json(spec: &Spec, name: &str, answer: &Value) -> (Value, Vec<Option<f64>>) {
    let mut scores = Vec::with_capacity(spec.judgments.len());
    let queries: Vec<Value> = spec
        .judgments
        .iter()
        .enumerate()
        .map(|(i, j)| {
            let id = format!("q{i}");
            let d = &answer["details"][&id];
            let score = d["metric_score"].as_f64();
            scores.push(score);
            let mut q = json!({ "query": j.query, "score": score.map(round4) });
            if let Some(n) = d["unrated_docs"].as_array().map(Vec::len) {
                q["unrated"] = n.into();
            }
            if let Some(f) = answer["failures"].get(&id) {
                let reason = f["error"]["reason"].as_str().or_else(|| f["error"]["type"].as_str()).unwrap_or("the search failed");
                q["failure"] = reason.into();
            }
            q
        })
        .collect();
    let failed = queries.iter().filter(|q| q.get("failure").is_some()).count();
    let score = answer["metric_score"].as_f64().filter(|s| s.is_finite()).map(round4);
    (json!({ "name": name, "score": score, "failed": failed, "queries": queries }), scores)
}

/// The answer: each configuration's score and queries, and with two, the candidate (the second)
/// against the baseline (the first): the difference in score, and the queries that got better
/// or worse, the largest change first.
pub fn answer(spec: &Spec, answers: &[Value]) -> Value {
    let mut configurations = Vec::new();
    let mut per_query = Vec::new();
    for (c, a) in spec.configurations.iter().zip(answers) {
        let (v, s) = configuration_json(spec, &c.name, a);
        configurations.push(v);
        per_query.push(s);
    }
    let mut out = json!({ "metric": spec.metric, "queries": spec.judgments.len(), "configurations": configurations });
    if let [a, b] = per_query.as_slice() {
        let mut changed: Vec<(usize, f64, f64)> = Vec::new();
        let mut unchanged = 0usize;
        for (i, (x, y)) in a.iter().zip(b).enumerate() {
            match (x, y) {
                (Some(x), Some(y)) if (y - x).abs() >= 1e-9 => changed.push((i, *x, *y)),
                (Some(_), Some(_)) => unchanged += 1,
                _ => {}
            }
        }
        let row = |(i, x, y): &(usize, f64, f64)| json!({ "query": spec.judgments[*i].query, "baseline": round4(*x), "candidate": round4(*y), "difference": round4(y - x) });
        let mut better: Vec<&(usize, f64, f64)> = changed.iter().filter(|(_, x, y)| y > x).collect();
        let mut worse: Vec<&(usize, f64, f64)> = changed.iter().filter(|(_, x, y)| y < x).collect();
        better.sort_by(|p, q| (q.2 - q.1).total_cmp(&(p.2 - p.1)));
        worse.sort_by(|p, q| (p.2 - p.1).total_cmp(&(q.2 - q.1)));
        let (sa, sb) = (out["configurations"][0]["score"].as_f64(), out["configurations"][1]["score"].as_f64());
        out["comparison"] = json!({
            "baseline": spec.configurations[0].name,
            "candidate": spec.configurations[1].name,
            "difference": sa.zip(sb).map(|(a, b)| round4(b - a)),
            "better": better.len(),
            "worse": worse.len(),
            "unchanged": unchanged,
            "better_queries": better.iter().take(LIST_ROWS).map(|r| row(r)).collect::<Vec<_>>(),
            "worse_queries": worse.iter().take(LIST_ROWS).map(|r| row(r)).collect::<Vec<_>>(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        parse(
            br#"{"judgments": [{"query": "red shoes", "ratings": [{"_id": "1", "rating": 3}, {"_id": "2", "rating": 0, "_index": "other"}]}, {"query": "blue", "ratings": [{"_id": "3", "rating": 1}]}],
                 "configurations": [{"name": "a", "request": {"query": {"match": {"title": "{{query}}"}}}}, {"name": "b", "template": {"id": "t1", "params": {"boost": 2}}, "query_param": "q"}]}"#,
        )
        .unwrap()
    }

    #[test]
    fn bodies_render_each_query_through_its_configuration() {
        let s = spec();
        assert_eq!(s.metric, json!({ "dcg": { "k": 10, "normalize": true } }));
        let b = rank_eval_body(&s, &s.configurations[1], "products");
        assert_eq!(b["templates"][0]["template"], json!({ "id": "t1" }));
        assert_eq!(b["requests"][0]["params"], json!({ "boost": 2, "q": "red shoes" }));
        assert_eq!(b["requests"][0]["ratings"][0], json!({ "_index": "products", "_id": "1", "rating": 3 }));
        assert_eq!(b["requests"][0]["ratings"][1]["_index"], "other");
        let a = rank_eval_body(&s, &s.configurations[0], "products");
        assert_eq!(a["templates"][0]["template"]["source"]["query"]["match"]["title"], "{{query}}");
        assert_eq!(a["requests"][1]["params"], json!({ "query": "blue" }));
    }

    #[test]
    fn two_configurations_compare_query_by_query() {
        let s = spec();
        let a = json!({ "metric_score": 0.5, "details": { "q0": { "metric_score": 0.2, "unrated_docs": [] }, "q1": { "metric_score": 0.8, "unrated_docs": [{}] } }, "failures": {} });
        let b = json!({ "metric_score": 0.6, "details": { "q0": { "metric_score": 0.9, "unrated_docs": [] } }, "failures": { "q1": { "error": { "type": "x", "reason": "boom" } } } });
        let out = answer(&s, &[a, b]);
        assert_eq!(out["configurations"][0]["queries"][1]["unrated"], 1);
        assert_eq!(out["configurations"][1]["failed"], 1);
        assert_eq!(out["configurations"][1]["queries"][1]["failure"], "boom");
        let c = &out["comparison"];
        assert_eq!((c["better"].as_u64(), c["worse"].as_u64(), c["unchanged"].as_u64()), (Some(1), Some(0), Some(0)));
        assert_eq!(c["better_queries"][0], json!({ "query": "red shoes", "baseline": 0.2, "candidate": 0.9, "difference": 0.7 }));
        assert_eq!(c["difference"], 0.1);
    }

    #[test]
    fn bodies_are_refused_with_what_they_take() {
        for (body, says) in [
            (r#"{"judgments": [], "configurations": [{"name": "a", "request": {}}]}"#, "[judgments]"),
            (r#"{"judgments": [{"query": "x", "ratings": [{"_id": "1", "rating": 1}]}], "configurations": []}"#, "[configurations]"),
            (r#"{"judgments": [{"query": "x", "ratings": [{"_id": "1", "rating": 1}]}], "configurations": [{"name": "a", "request": {}, "template": {"id": "t"}}]}"#, "one of them"),
            (r#"{"judgments": [{"query": "x", "ratings": [{"_id": "1", "rating": 101}]}], "configurations": [{"name": "a", "request": {}}]}"#, "0 to 100"),
            (r#"{"judgments": [{"query": "x", "ratings": [{"_id": "1", "rating": 1}]}], "configurations": [{"name": "a", "request": {}}, {"name": "a", "request": {}}]}"#, "names of their own"),
            (r#"{"judgments": [{"query": "x", "ratings": [{"_id": "1", "rating": 1}]}], "configurations": [{"name": "a", "request": {}}], "extra": 1}"#, "no field [extra]"),
        ] {
            let e = parse(body.as_bytes()).unwrap_err();
            assert!(e.contains(says), "{body}: {e}");
        }
    }
}
