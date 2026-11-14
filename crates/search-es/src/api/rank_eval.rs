//! `GET|POST /{index}/_rank_eval` (`TransportRankEvalAction`): runs each rated request as a
//! search (or renders it from a template first), joins its hits with the request's ratings and
//! scores the ranking with the chosen metric; the overall score is the mean of the requests'.
//!
//! Metrics (Elasticsearch's formulas): `precision` (relevant over retrieved, optionally ignoring
//! unrated hits), `recall` (relevant retrieved over relevant rated), `mean_reciprocal_rank`
//! (1 / rank of the first relevant hit), `dcg` (Σ (2^rating - 1) / log2(rank + 1), optionally
//! normalized by the ideal ordering) and `expected_reciprocal_rank` (cascade model over
//! `maximum_relevance`). Each metric forces the search's `size` to its `k`.
//!
//! A request that fails to search is reported under `failures`; malformed specifications fail
//! the whole call with the parse errors Elasticsearch's object parsers give.

use crate::api::Reply;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Number, Value};
use crate::obj;
use crate::rest::Ctx;

/// A metric and its parameters.
#[derive(Clone, Debug, PartialEq)]
enum Metric {
    Precision { k: i64, threshold: i64, ignore_unlabeled: bool },
    Recall { k: i64, threshold: i64 },
    Mrr { k: i64, threshold: i64 },
    Dcg { k: i64, normalize: bool, unknown: Option<i64> },
    Err { k: i64, max_relevance: i64, unknown: Option<i64> },
}

/// One hit joined with its rating.
struct Rated {
    hit: Value,
    index: String,
    id: String,
    rating: Option<i64>,
}

fn failed_field(parser: &str, field: &str, cause: EsError) -> EsError {
    EsError::x_content_parse(1, 1, format!("[{parser}] failed to parse field [{field}]")).caused_by(cause)
}

/// `ConstructingObjectParser` failures of a rated request (`Failed to build [request] ...`).
fn request_error(cause: EsError) -> EsError {
    failed_field("rank_eval", "requests", EsError::x_content_parse_bare("Failed to build [request] after last required field arrived").caused_by(cause))
}

fn int(m: &Map, k: &str, parser: &str) -> EsResult<Option<i64>> {
    match m.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Num(n)) => n.as_i64().map(Some).ok_or_else(|| failed_field(parser, k, EsError::illegal_argument(format!("[{k}] must be an integer")))),
        Some(Value::Str(s)) => s.parse::<i64>().map(Some).map_err(|_| failed_field(parser, k, EsError::number_format(s))),
        Some(_) => Err(failed_field(parser, k, EsError::illegal_argument(format!("[{k}] must be an integer")))),
    }
}

fn parse_metric(v: &Value) -> EsResult<Metric> {
    let Value::Obj(m) = v else { return Err(failed_field("rank_eval", "metric", EsError::parsing(1, 1, "metric must be an object"))) };
    let Some((name, body)) = m.iter().next() else { return Err(failed_field("rank_eval", "metric", EsError::parsing(1, 1, "metric must name one metric"))) };
    let empty = Map::new();
    let b = body.as_object().unwrap_or(&empty);
    let allowed: &[&str] = match name.as_str() {
        "precision" => &["k", "relevant_rating_threshold", "ignore_unlabeled"],
        "recall" | "mean_reciprocal_rank" => &["k", "relevant_rating_threshold"],
        "dcg" => &["k", "normalize", "unknown_doc_rating"],
        "expected_reciprocal_rank" => &["k", "maximum_relevance", "unknown_doc_rating"],
        other => {
            return Err(failed_field("rank_eval", "metric", EsError::new(400, "named_object_not_found_exception", format!("[1:1] unknown field [{other}]"))));
        }
    };
    for k in b.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(failed_field("rank_eval", "metric", EsError::x_content_parse(1, 1, format!("[{name}] unknown field [{k}]"))));
        }
    }
    let k = int(b, "k", name)?.unwrap_or(10);
    let threshold = int(b, "relevant_rating_threshold", name)?.unwrap_or(1);
    Ok(match name.as_str() {
        "precision" => Metric::Precision { k, threshold, ignore_unlabeled: b.get("ignore_unlabeled").and_then(Value::as_bool).unwrap_or(false) },
        "recall" => Metric::Recall { k, threshold },
        "mean_reciprocal_rank" => Metric::Mrr { k, threshold },
        "dcg" => Metric::Dcg { k, normalize: b.get("normalize").and_then(Value::as_bool).unwrap_or(false), unknown: int(b, "unknown_doc_rating", name)? },
        _ => {
            let Some(max_relevance) = int(b, "maximum_relevance", name)? else {
                return Err(failed_field("rank_eval", "metric", EsError::illegal_argument("Required [maximum_relevance]")));
            };
            Metric::Err { k, max_relevance, unknown: int(b, "unknown_doc_rating", name)? }
        }
    })
}

impl Metric {
    fn k(&self) -> i64 {
        match self {
            Metric::Precision { k, .. } | Metric::Recall { k, .. } | Metric::Mrr { k, .. } | Metric::Dcg { k, .. } | Metric::Err { k, .. } => *k,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Metric::Precision { .. } => "precision",
            Metric::Recall { .. } => "recall",
            Metric::Mrr { .. } => "mean_reciprocal_rank",
            Metric::Dcg { .. } => "dcg",
            Metric::Err { .. } => "expected_reciprocal_rank",
        }
    }

    /// The metric's score of one ranking and its `metric_details`; `ratings` are all of the
    /// request's ratings (recall and the ideal DCG read them).
    fn evaluate(&self, hits: &[Rated], ratings: &[(String, String, i64)]) -> (f64, Map) {
        let mut d = Map::new();
        let score = match self {
            Metric::Precision { threshold, ignore_unlabeled, .. } => {
                let (mut relevant, mut retrieved) = (0i64, 0i64);
                for h in hits {
                    match h.rating {
                        Some(r) => {
                            retrieved += 1;
                            if r >= *threshold {
                                relevant += 1;
                            }
                        }
                        None if !ignore_unlabeled => retrieved += 1,
                        None => {}
                    }
                }
                d.insert("relevant_docs_retrieved".into(), Value::from(relevant));
                d.insert("docs_retrieved".into(), Value::from(retrieved));
                if retrieved > 0 { relevant as f64 / retrieved as f64 } else { 0.0 }
            }
            Metric::Recall { threshold, .. } => {
                let retrieved = hits.iter().filter(|h| h.rating.is_some_and(|r| r >= *threshold)).count() as i64;
                let relevant = ratings.iter().filter(|r| r.2 >= *threshold).count() as i64;
                d.insert("relevant_docs_retrieved".into(), Value::from(retrieved));
                d.insert("relevant_docs".into(), Value::from(relevant));
                if relevant > 0 { retrieved as f64 / relevant as f64 } else { 0.0 }
            }
            Metric::Mrr { threshold, .. } => {
                let first = hits.iter().position(|h| h.rating.is_some_and(|r| r >= *threshold)).map(|p| p as i64 + 1).unwrap_or(-1);
                d.insert("first_relevant".into(), Value::from(first));
                if first == -1 { 0.0 } else { 1.0 / first as f64 }
            }
            Metric::Dcg { normalize, unknown, .. } => {
                let ranked: Vec<Option<i64>> = hits.iter().map(|h| h.rating.or(*unknown)).collect();
                let unrated = hits.iter().filter(|h| h.rating.is_none()).count() as i64;
                let dcg = compute_dcg(&ranked);
                let mut result = dcg;
                d.insert("dcg".into(), Value::Num(Number::Float(dcg)));
                if *normalize {
                    let mut all: Vec<i64> = ratings.iter().map(|r| r.2).collect();
                    all.sort_unstable_by(|a, b| b.cmp(a));
                    all.truncate(ranked.len());
                    let ideal = compute_dcg(&all.into_iter().map(Some).collect::<Vec<_>>());
                    result = if ideal != 0.0 { dcg / ideal } else { 0.0 };
                    d.insert("ideal_dcg".into(), Value::Num(Number::Float(ideal)));
                    d.insert("normalized_dcg".into(), Value::Num(Number::Float(result)));
                }
                d.insert("unrated_docs".into(), Value::from(unrated));
                result
            }
            Metric::Err { k, max_relevance, unknown } => {
                let top = &hits[..hits.len().min((*k).max(0) as usize)];
                let unrated = top.iter().filter(|h| h.rating.is_none()).count() as i64;
                let two_pow_max = 2f64.powi(*max_relevance as i32);
                let (mut p, mut err) = (1.0f64, 0.0f64);
                for (i, h) in top.iter().enumerate() {
                    if let Some(r) = h.rating.or(*unknown) {
                        let prob = (2f64.powi(r as i32) - 1.0) / two_pow_max;
                        err += p * prob / (i + 1) as f64;
                        p *= 1.0 - prob;
                    }
                }
                d.insert("unrated_docs".into(), Value::from(unrated));
                err
            }
        };
        (score, d)
    }
}

/// `DiscountedCumulativeGain.computeDCG`.
fn compute_dcg(ratings: &[Option<i64>]) -> f64 {
    let mut dcg = 0.0;
    for (i, r) in ratings.iter().enumerate() {
        if let Some(r) = r {
            dcg += (2f64.powi(*r as i32) - 1.0) / ((i as f64 + 2.0).ln() / std::f64::consts::LN_2);
        }
    }
    dcg
}

fn json_double(v: f64) -> Value {
    if v.is_nan() { Value::Str("NaN".into()) } else { Value::Num(Number::Float(v)) }
}

/// A parsed rated request.
struct RatedRequest {
    id: String,
    body: Value,
    ratings: Vec<(String, String, i64)>,
    summary_fields: Vec<String>,
}

fn parse_request(r: &Value, templates: &Map, ctx: &Ctx) -> EsResult<RatedRequest> {
    let Value::Obj(m) = r else { return Err(request_error(EsError::illegal_argument("a rated request must be an object"))) };
    for k in m.keys() {
        if !matches!(k.as_str(), "id" | "request" | "ratings" | "params" | "summary_fields" | "template_id") {
            return Err(failed_field("rank_eval", "requests", EsError::x_content_parse(1, 1, format!("[request] unknown field [{k}]"))));
        }
    }
    let id = m.get("id").map(Value::to_es_string).ok_or_else(|| request_error(EsError::illegal_argument("Required [id]")))?;
    let Some(Value::Arr(rs)) = m.get("ratings") else { return Err(request_error(EsError::illegal_argument("Required [ratings]"))) };
    let mut ratings = Vec::with_capacity(rs.len());
    for x in rs {
        let get = |k: &str| x.get(k).filter(|v| !v.is_null());
        let index = get("_index").map(Value::to_es_string).ok_or_else(|| request_error(EsError::illegal_argument("Index must be set for each rated document")))?;
        let doc = get("_id").map(Value::to_es_string).ok_or_else(|| request_error(EsError::illegal_argument("DocId must be set for each rated document")))?;
        let rating = get("rating").and_then(Value::as_i64).ok_or_else(|| request_error(EsError::illegal_argument("Required [rating]")))?;
        ratings.push((index, doc, rating));
    }
    let params = m.get("params").and_then(Value::as_object).filter(|p| !p.is_empty());
    let request = m.get("request");
    if params.is_some() && request.is_some() {
        return Err(request_error(EsError::illegal_argument(
            "Ambiguous rated request: Set both, verbatim test request and test request template parameters.",
        )));
    }
    if params.is_none() && request.is_none() {
        return Err(request_error(EsError::illegal_argument("Need to set at least test request or test request template parameters.")));
    }
    let template_id = m.get("template_id").map(Value::to_es_string);
    if params.is_some() && template_id.is_none() {
        return Err(request_error(EsError::illegal_argument("If template parameters are supplied need to set id of template to apply them to too.")));
    }
    let body = match (request, template_id) {
        (Some(b), _) => b.clone(),
        (None, Some(t)) => {
            let Some(Value::Obj(tm)) = templates.get(&t) else {
                return Err(EsError::illegal_argument(format!("Template with id [{t}] not found")));
            };
            let mut tb = tm.clone();
            tb.insert("params".into(), Value::Obj(params.cloned().unwrap_or_default()));
            crate::search::misc::render_template(ctx, &tb, None)?
        }
        (None, None) => Value::Obj(Map::new()),
    };
    // `RatedRequest.validateEvaluatedQuery`.
    for (k, msg) in [
        ("suggest", "Query in rated requests should not contain a suggest section."),
        ("aggs", "Query in rated requests should not contain aggregations."),
        ("aggregations", "Query in rated requests should not contain aggregations."),
        ("highlight", "Query in rated requests should not contain a highlighter section."),
    ] {
        if body.get(k).is_some() {
            return Err(request_error(EsError::illegal_argument(msg)));
        }
    }
    // Booleans as the search source parses them (`"true"` too).
    let on = |k: &str| matches!(body.get(k), Some(Value::Bool(true))) || matches!(body.get(k), Some(Value::Str(s)) if s == "true");
    if on("explain") {
        return Err(request_error(EsError::illegal_argument("Query in rated requests should not use explain.")));
    }
    if on("profile") {
        return Err(request_error(EsError::illegal_argument("Query in rated requests should not use profile.")));
    }
    let summary_fields = match m.get("summary_fields") {
        Some(Value::Arr(a)) => a.iter().map(Value::to_es_string).collect(),
        _ => Vec::new(),
    };
    Ok(RatedRequest { id, body, ratings, summary_fields })
}

/// `_rank_eval`.
pub fn rank_eval(ctx: &Ctx) -> EsResult<Reply> {
    let Some(Value::Obj(spec)) = ctx.body()? else {
        return Err(EsError::action_request_validation(&["request body or source parameter is required".into()]));
    };
    for k in spec.keys() {
        if !matches!(k.as_str(), "requests" | "metric" | "templates" | "max_concurrent_searches") {
            return Err(EsError::x_content_parse(1, 1, format!("[rank_eval] unknown field [{k}]")));
        }
    }
    let metric = parse_metric(spec.get("metric").ok_or_else(|| EsError::illegal_argument("Required [metric]"))?)?;
    let mut templates = Map::new();
    if let Some(Value::Arr(ts)) = spec.get("templates") {
        for t in ts {
            let id = t.get("id").map(Value::to_es_string).ok_or_else(|| EsError::illegal_argument("Required [id]"))?;
            let Some(Value::Obj(tm)) = t.get("template") else { return Err(EsError::illegal_argument("Required [template]")) };
            templates.insert(id, Value::Obj(tm.clone()));
        }
    }
    let Some(Value::Arr(reqs)) = spec.get("requests") else { return Err(EsError::illegal_argument("Required [requests]")) };
    let mut rated = Vec::with_capacity(reqs.len());
    for r in reqs {
        let rr = parse_request(r, &templates, ctx)?;
        // Each search parses now: a malformed one fails the whole request.
        let mut body = rr.body.clone();
        if let Value::Obj(bm) = &mut body {
            bm.insert("size".into(), Value::from(metric.k()));
            let src = if rr.summary_fields.is_empty() {
                Value::Bool(false)
            } else {
                obj! { "includes" => Value::Arr(rr.summary_fields.iter().cloned().map(Value::Str).collect()), "excludes" => Value::Arr(Vec::new()) }
            };
            bm.insert("_source".into(), src);
        }
        let req = crate::search::parse_request(ctx, Some(&body), None)
            .map_err(|e| failed_field("rank_eval", "requests", failed_field("request", "request", e)))?;
        rated.push((rr, req));
    }
    let mut details: Vec<(String, Value, f64)> = Vec::new();
    let mut failures: Vec<(String, Value)> = Vec::new();
    for (rr, req) in rated {
        let outcome = crate::search::run(ctx, req.clone());
        let resp = match outcome {
            Ok(o) => crate::search::render(&o, &req),
            Err(e) => {
                failures.push((rr.id.clone(), obj! { "error" => e.to_value_with_root(false) }));
                continue;
            }
        };
        let hits: Vec<Value> = resp.get("hits").and_then(|h| h.get("hits")).and_then(Value::as_array).cloned().unwrap_or_default();
        let joined: Vec<Rated> = hits
            .into_iter()
            .map(|h| {
                let index = h.get("_index").map(Value::to_es_string).unwrap_or_default();
                let id = h.get("_id").map(Value::to_es_string).unwrap_or_default();
                let rating = rr.ratings.iter().find(|r| r.0 == index && r.1 == id).map(|r| r.2);
                Rated { hit: h, index, id, rating }
            })
            .collect();
        let (score, metric_details) = metric.evaluate(&joined, &rr.ratings);
        // Expected reciprocal rank reports only the hits it looked at.
        let shown: &[Rated] = match metric {
            Metric::Err { k, .. } => &joined[..joined.len().min(k.max(0) as usize)],
            _ => &joined,
        };
        let unrated: Vec<Value> = shown.iter().filter(|h| h.rating.is_none()).map(|h| obj! { "_index" => h.index.clone(), "_id" => h.id.clone() }).collect();
        let hits_out: Vec<Value> = shown.iter().map(|h| obj! { "hit" => h.hit.clone(), "rating" => h.rating.map(Value::from).unwrap_or(Value::Null) }).collect();
        let mut md = Map::new();
        md.insert(metric.name().into(), Value::Obj(metric_details));
        details.push((
            rr.id.clone(),
            obj! {
                "metric_score" => json_double(score),
                "unrated_docs" => Value::Arr(unrated),
                "hits" => Value::Arr(hits_out),
                "metric_details" => Value::Obj(md),
            },
            score,
        ));
    }
    let overall = if details.is_empty() { f64::NAN } else { details.iter().map(|d| d.2).sum::<f64>() / details.len() as f64 };
    // `RankEvalResponse` keeps details and failures in hash maps.
    let dm: Map = details.into_iter().map(|(k, v, _)| (k, v)).collect();
    let fm: Map = failures.into_iter().collect();
    Reply::ok(obj! {
        "metric_score" => json_double(overall),
        "details" => Value::Obj(crate::json::java_hash_map_expected(dm)),
        "failures" => Value::Obj(crate::json::java_hash_map_expected(fm)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rated(ratings: &[Option<i64>]) -> Vec<Rated> {
        ratings.iter().enumerate().map(|(i, r)| Rated { hit: Value::Null, index: "i".into(), id: i.to_string(), rating: *r }).collect()
    }

    #[test]
    fn dcg_matches_elasticsearch() {
        // Ratings 1, 3, 0 (doc ranks 1..3): ES reports dcg 5.416508275000202, ideal 7.630929753571458.
        let m = Metric::Dcg { k: 10, normalize: true, unknown: None };
        let all = vec![("i".into(), "0".into(), 1), ("i".into(), "1".into(), 3), ("i".into(), "2".into(), 0)];
        let (score, d) = m.evaluate(&rated(&[Some(1), Some(3), Some(0)]), &all);
        assert_eq!(d.get("dcg").and_then(Value::as_f64), Some(5.416508275000202));
        assert_eq!(d.get("ideal_dcg").and_then(Value::as_f64), Some(7.630929753571458));
        assert_eq!(score, 0.7098097413968654);
    }

    #[test]
    fn err_and_precision() {
        let m = Metric::Err { k: 2, max_relevance: 3, unknown: None };
        let (score, _) = m.evaluate(&rated(&[Some(1), Some(3)]), &[]);
        assert_eq!(score, 0.5078125);
        let p = Metric::Precision { k: 10, threshold: 1, ignore_unlabeled: true };
        let (score, d) = p.evaluate(&rated(&[Some(3), None, Some(0)]), &[]);
        assert_eq!(score, 0.5);
        assert_eq!(d.get("docs_retrieved").and_then(Value::as_i64), Some(2));
    }

    #[test]
    fn mrr_first_relevant() {
        let m = Metric::Mrr { k: 10, threshold: 1 };
        let (score, d) = m.evaluate(&rated(&[None, Some(3), Some(0)]), &[]);
        assert_eq!(score, 0.5);
        assert_eq!(d.get("first_relevant").and_then(Value::as_i64), Some(2));
    }
}
