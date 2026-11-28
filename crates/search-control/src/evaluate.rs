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
//! Judgments come in the request, or from a stored list (`judgments_list`, [`crate::judgments`]).

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
    /// A search application whose own template renders each query (`_render_query`: its
    /// dictionary, its defaults), with an experiment's variant's params over them; `template` is
    /// unused then.
    pub application: Option<Application>,
}

/// A configuration that evaluates a search application, or one variant of its experiment.
#[derive(Clone, Debug, PartialEq)]
pub struct Application {
    pub name: String,
    /// `(experiment, variant)`.
    pub variant: Option<(String, String)>,
}

/// What an application's template calls the query unless the configuration says
/// (Elasticsearch's default template's).
pub const APPLICATION_QUERY_PARAM: &str = "query_string";

/// A parsed `evaluate` body.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub judgments: Vec<Judgment>,
    /// The stored list the judgments come from (`judgments_list`): `judgments` is empty until the
    /// route reads it.
    pub list: Option<String>,
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

pub(crate) fn parse_judgment(v: &Value, i: usize) -> Result<Judgment, String> {
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
    only(m, &format!("[configurations][{i}]"), &["name", "request", "template", "application", "variant", "query_param"])?;
    let name = m.get("name").and_then(Value::as_str).filter(|n| !n.is_empty() && n.len() <= 64).ok_or_else(|| bad(format!("[configurations][{i}][name] is 1 to 64 characters")))?;
    let application = match (m.get("application"), m.get("variant")) {
        (None, None) => None,
        (Some(Value::String(a)), v) if !a.is_empty() && a.len() <= 255 => {
            let variant = match v {
                None | Some(Value::Null) => None,
                Some(Value::Object(o)) => match (o.get("experiment").and_then(Value::as_str), o.get("variant").and_then(Value::as_str), o.len()) {
                    (Some(e), Some(v), 2) if !e.is_empty() && !v.is_empty() => Some((e.to_string(), v.to_string())),
                    _ => return Err(bad(format!("[configurations][{i}][variant] is {{experiment, variant}}"))),
                },
                Some(_) => return Err(bad(format!("[configurations][{i}][variant] is {{experiment, variant}}"))),
            };
            if m.contains_key("request") || m.contains_key("template") {
                return Err(bad(format!("[configurations][{i}] evaluates an [application] with its own template: no [request] or [template] beside it")));
            }
            Some(Application { name: a.clone(), variant })
        }
        (Some(_), _) => return Err(bad(format!("[configurations][{i}][application] is a search application's name"))),
        (None, Some(_)) => return Err(bad(format!("[configurations][{i}][variant] needs the [application] it's a variant of"))),
    };
    let query_param = match m.get("query_param") {
        None | Some(Value::Null) if application.is_some() => APPLICATION_QUERY_PARAM.to_string(),
        None | Some(Value::Null) => QUERY_PARAM.to_string(),
        Some(Value::String(p)) if !p.is_empty() && p.len() <= 64 => p.clone(),
        Some(_) => return Err(bad(format!("[configurations][{i}][query_param] is a parameter name"))),
    };
    if let Some(app) = application {
        return Ok(Configuration { name: name.to_string(), template: Value::Null, params: Map::new(), query_param, application: Some(app) });
    }
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
    Ok(Configuration { name: name.to_string(), template, params, query_param, application: None })
}

/// The judged queries of a `judgments` list (`[{query, ratings: [{_id, rating, _index?}]}]`, at
/// most [`MAX_QUERIES`] of [`MAX_RATINGS`]).
pub(crate) fn parse_judgments(v: Option<&Value>) -> Result<Vec<Judgment>, String> {
    let Some(js) = v.and_then(Value::as_array).filter(|j| !j.is_empty()) else {
        return Err(bad("[judgments] is a list of {query, ratings}"));
    };
    if js.len() > MAX_QUERIES {
        return Err(bad(format!("[judgments] holds {} queries: at most {MAX_QUERIES}", js.len())));
    }
    js.iter().enumerate().map(|(i, j)| parse_judgment(j, i)).collect()
}

/// Parses an `evaluate` body: `{judgments: [{query, ratings: [{_id, rating, _index?}]}] |
/// judgments_list, configurations: [{name, request | template, query_param?}] (one or two),
/// metric?}`, the metric `_rank_eval`'s (by default nDCG at 10).
pub fn parse(body: &[u8]) -> Result<Spec, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| bad(format!("the body isn't JSON: {e}")))?;
    let Some(m) = v.as_object() else { return Err(bad("the body is an object {judgments, configurations, metric?}")) };
    only(m, "the body", &["judgments", "judgments_list", "configurations", "metric"])?;
    let (judgments, list) = match (m.get("judgments"), m.get("judgments_list")) {
        (j, None) => (parse_judgments(j)?, None),
        (None, Some(Value::String(name))) => {
            crate::judgments::index_name(name).map_err(bad)?;
            (Vec::new(), Some(name.clone()))
        }
        (None, Some(_)) => return Err(bad("[judgments_list] is a stored judgment list's name")),
        (Some(_), Some(_)) => return Err(bad("the judgments come in [judgments] or from a stored [judgments_list], one of them")),
    };
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
    Ok(Spec { judgments, list, configurations, metric })
}

/// Whether `index` names one index, alias or data stream (no pattern, list or exclusion): a
/// rating without an `_index` is about a document of it only then.
pub fn names_one(index: &str) -> bool {
    !index.contains(['*', ',', '?']) && !index.starts_with('-')
}

/// Refuses a rating with no `_index` when the evaluated index is a pattern or list: its hits
/// come back under their own index names, so the rating would match nothing.
pub fn check_ratings_name(spec: &Spec, index: &str) -> Result<(), String> {
    if names_one(index) {
        return Ok(());
    }
    match spec.judgments.iter().enumerate().find(|(_, j)| j.ratings.iter().any(|r| r.0.is_empty())) {
        Some((i, _)) => Err(bad(format!("[{index}] isn't one index: each rating names its document's [_index] ([judgments][{i}] has one that doesn't)"))),
        None => Ok(()),
    }
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

/// The `_rank_eval` body that scores already rendered searches (an application's configuration:
/// `rendered[i]` is judged query `i` through the application's template), a rating without an
/// `_index` about a document of `index`.
pub fn rank_eval_rendered(spec: &Spec, rendered: Vec<Value>, index: &str) -> Value {
    let requests: Vec<Value> = spec
        .judgments
        .iter()
        .zip(rendered)
        .enumerate()
        .map(|(i, (j, body))| {
            let ratings: Vec<Value> = j.ratings.iter().map(|(ix, id, r)| json!({ "_index": if ix.is_empty() { index } else { ix.as_str() }, "_id": id, "rating": r })).collect();
            json!({ "id": format!("q{i}"), "request": body, "ratings": ratings })
        })
        .collect();
    json!({ "metric": spec.metric, "requests": requests })
}

/// The params that render judged query `query` through application `app` (its configuration's
/// `query_param`), with the variant's params over them: `Err` names what the stored application
/// lacks (the experiment, the variant) or a variant that brings a template of its own, which
/// evaluates as a `template` configuration.
pub fn application_params(c: &Configuration, app: &Value, query: &str) -> Result<Map<String, Value>, String> {
    let mut params = Map::new();
    params.insert(c.query_param.clone(), Value::String(query.to_string()));
    let Some(Application { name, variant: Some((exp, var)) }) = &c.application else { return Ok(params) };
    let e = app["experiments"].as_array().and_then(|es| es.iter().find(|e| e["id"] == exp.as_str())).ok_or_else(|| bad(format!("[{name}] has no experiment [{exp}]")))?;
    let v = e["variants"].as_array().and_then(|vs| vs.iter().find(|v| v["name"] == var.as_str())).ok_or_else(|| bad(format!("experiment [{exp}] has no variant [{var}]")))?;
    if v.get("template").is_some_and(|t| !t.is_null()) {
        return Err(bad(format!("variant [{var}] brings its own template: evaluate it as a [template] configuration")));
    }
    if let Some(p) = v["params"].as_object() {
        for (k, x) in p {
            params.insert(k.clone(), x.clone());
        }
    }
    Ok(params)
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
    if let Some(l) = &spec.list {
        out["judgments_list"] = json!(l);
    }
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
    fn an_application_variants_params_go_over_the_query() {
        let s = parse(br#"{"judgments": [{"query": "x", "ratings": [{"_id": "1", "rating": 1}]}], "configurations": [{"name": "app"
            , "application": "shop"}, {"name": "b", "application": "shop", "variant": {"experiment": "e1", "variant": "boosted"}}]}"#)
        .unwrap();
        assert_eq!(s.configurations[0].query_param, "query_string");
        let app = json!({ "experiments": [{ "id": "e1", "variants": [{ "name": "boosted", "params": { "boost": 3 } }, { "name": "t", "template": { "script": { "source": "{}" } } }] }] });
        assert_eq!(application_params(&s.configurations[0], &app, "red").unwrap(), json!({ "query_string": "red" }).as_object().unwrap().clone());
        assert_eq!(application_params(&s.configurations[1], &app, "red").unwrap(), json!({ "query_string": "red", "boost": 3 }).as_object().unwrap().clone());
        let mut t = s.configurations[1].clone();
        t.application = Some(Application { name: "shop".into(), variant: Some(("e1".into(), "t".into())) });
        assert!(application_params(&t, &app, "red").unwrap_err().contains("own template"));
        t.application = Some(Application { name: "shop".into(), variant: Some(("e2".into(), "t".into())) });
        assert!(application_params(&t, &app, "red").unwrap_err().contains("no experiment [e2]"));
        let body = rank_eval_rendered(&s, vec![json!({ "query": { "match_all": {} } })], "shop");
        assert_eq!(body["requests"][0]["request"], json!({ "query": { "match_all": {} } }));
        assert!(parse(br#"{"judgments": [{"query": "x", "ratings": [{"_id": "1", "rating": 1}]}], "configurations": [{"name": "a", "application": "shop", "request": {}}]}"#).unwrap_err().contains("no [request]"));
    }

    #[test]
    fn a_pattern_needs_each_ratings_index() {
        let s = spec();
        assert!(check_ratings_name(&s, "products").is_ok());
        let e = check_ratings_name(&s, "products-*").unwrap_err();
        assert!(e.contains("[judgments][0]"), "{e}");
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
