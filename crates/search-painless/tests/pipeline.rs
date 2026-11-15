//! Runs the captured pipeline-aggregation fixtures (`tests/fixtures/pipeline.json`, produced by
//! `tests/fixtures/capture_pipeline.py` against Elasticsearch 9.5.5): `bucket_script`,
//! `bucket_selector` and `moving_fn` over a daily `date_histogram` with `sum` and `max`
//! sub-aggregations. The bucket values come from Elasticsearch's own response; the harness binds
//! them the way the aggregation framework does (`buckets_path` variables in `params`, the window
//! in `values`, gap policies) and compares each script result with what Elasticsearch computed.

use std::path::Path;

use search_painless::{Context, Engine, ScriptError, Settings, Value};
use serde_json::{json, Value as Json};

fn fixture(name: &str) -> Json {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn error_body(e: &ScriptError) -> Json {
    let mut body = e.to_response()["error"].clone();
    body.as_object_mut().unwrap().remove("root_cause");
    body
}

/// A bucket's value for a `buckets_path` (`_count` or a metric sub-aggregation); `None` when
/// the metric has no value.
fn path_value(bucket: &Json, path: &str) -> Option<f64> {
    if path == "_count" {
        return bucket["doc_count"].as_f64();
    }
    bucket[path]["value"].as_f64()
}

/// The value a gap policy gives a bucket, `None` when the bucket is skipped.
fn resolve(bucket: &Json, path: &str, policy: &str) -> Option<f64> {
    let empty = bucket["doc_count"].as_u64() == Some(0);
    match (path_value(bucket, path), policy) {
        (_, "insert_zeros") if empty => Some(0.0),
        (None, "insert_zeros") => Some(0.0),
        (None, _) => None,
        (Some(_), "skip") if empty => None,
        (Some(v), _) => Some(v),
    }
}

fn number(v: &Value) -> Json {
    match v.as_f64() {
        Some(x) if x.is_finite() => json!(x),
        _ => Json::Null,
    }
}

fn doubles(v: &[f64]) -> Value {
    Value::double_array(v.to_vec())
}

/// Our buckets: `Ok(list of (key, pipeline value or None))`, or the script error.
fn run_case(engine: &Engine, req: &Json, all: &Json) -> Result<Json, Json> {
    let pipe = &req["aggs"]["h"]["aggs"]["p"];
    let buckets = all["aggregations"]["h"]["buckets"].as_array().unwrap();
    let (kind, spec) = pipe.as_object().unwrap().iter().next().unwrap();
    let script = &spec["script"];
    let src = script["source"].as_str().unwrap();
    let user = script.get("params").cloned().unwrap_or(json!({}));
    let policy = spec.get("gap_policy").and_then(|g| g.as_str()).unwrap_or("skip");
    let ctx = match kind.as_str() {
        "bucket_script" => Context::BucketAggregation,
        "bucket_selector" => Context::AggregationSelector,
        _ => Context::MovingFunction,
    };
    let compiled = engine.compile(src, ctx).map_err(|e| error_body(&e))?;
    let mut inst = compiled.instance();
    let mut out = Vec::new();
    match kind.as_str() {
        "bucket_script" | "bucket_selector" => {
            let paths = spec["buckets_path"].as_object().unwrap();
            for b in buckets {
                let mut params = user.clone();
                let mut skip = false;
                for (var, path) in paths {
                    let path = path.as_str().unwrap();
                    if kind == "bucket_selector" {
                        // selectors see every bucket; a missing value is NaN (or 0 with insert_zeros)
                        let v = path_value(b, path).unwrap_or(if policy == "insert_zeros" { 0.0 } else { f64::NAN });
                        params[var] = if v.is_nan() { Json::Null } else { json!(v) };
                        continue;
                    }
                    match resolve(b, path, policy) {
                        Some(v) => params[var] = json!(v),
                        None => skip = true,
                    }
                }
                let key = b["key_as_string"].clone();
                if skip {
                    // a skipped bucket gets no value (bucket_script) or is dropped (bucket_selector)
                    out.push(if kind == "bucket_script" { json!([key, null]) } else { json!([key, false]) });
                    continue;
                }
                inst.set_params(&params);
                // JSON has no NaN: bind the selector's missing values directly
                if kind == "bucket_selector" {
                    let mut p = Value::from_json_map(&params);
                    if let Value::Map(m) = &mut p {
                        for (var, path) in paths {
                            if params[var].is_null() && path_value(b, path.as_str().unwrap()).is_none() {
                                m.insert_raw(Value::str(var), Value::Double(f64::NAN));
                            }
                        }
                    }
                    inst.set_params_value(p);
                }
                let v = inst.execute(&mut search_painless::NoHost).map_err(|e| error_body(&e))?;
                if kind == "bucket_script" {
                    // a null result leaves the bucket without a value
                    out.push(json!([key, if v.is_null() { Json::Null } else { json!({ "value": number(&v) }) }]));
                } else {
                    out.push(json!([key, matches!(v, Value::Bool(true))]));
                }
            }
        }
        _ => {
            let path = spec["buckets_path"].as_str().unwrap();
            let window = spec["window"].as_i64().unwrap();
            let shift = spec.get("shift").and_then(|s| s.as_i64()).unwrap_or(0);
            let values: Vec<f64> = buckets.iter().filter_map(|b| resolve(b, path, policy)).collect();
            let n = values.len() as i64;
            let mut i = 0i64;
            inst.set_params(&user);
            for b in buckets {
                let key = b["key_as_string"].clone();
                if resolve(b, path, policy).is_none() {
                    out.push(json!([key, null]));
                    continue;
                }
                let from = (i - window + shift).clamp(0, n);
                let to = (i + shift).clamp(0, n);
                let win = if from < to { &values[from as usize..to as usize] } else { &[][..] };
                inst.set_var("values", doubles(win));
                let v = inst.execute(&mut search_painless::NoHost).map_err(|e| error_body(&e))?;
                out.push(json!([key, { "value": number(&v) }]));
                i += 1;
            }
        }
    }
    Ok(Json::Array(out))
}

/// Elasticsearch's answer in the same shape.
fn expected(req: &Json, resp: &Json, all: &Json) -> Json {
    let pipe = &req["aggs"]["h"]["aggs"]["p"];
    let kind = pipe.as_object().unwrap().keys().next().unwrap().clone();
    let buckets = resp["aggregations"]["h"]["buckets"].as_array().unwrap();
    if kind == "bucket_selector" {
        let kept: Vec<&Json> = buckets.iter().map(|b| &b["key_as_string"]).collect();
        return Json::Array(all["aggregations"]["h"]["buckets"].as_array().unwrap().iter().map(|b| json!([b["key_as_string"], kept.contains(&&b["key_as_string"])])).collect());
    }
    Json::Array(buckets.iter().map(|b| json!([b["key_as_string"], b.get("p").cloned().unwrap_or(Json::Null)])).collect())
}

#[test]
fn pipeline_aggregations() {
    let engine = Engine::new(Settings { max_compilations_rate: None, ..Settings::default() });
    let cases = fixture("pipeline.json");
    let mut failures = Vec::new();
    for c in cases.as_array().unwrap() {
        let req = &c["request"];
        let resp = &c["response"];
        let all = c.get("all").unwrap_or(resp);
        let want = if c["status"] == 200 {
            Ok(expected(req, resp, all))
        } else {
            Err(resp["error"]["caused_by"].clone())
        };
        // an erroring case still needs the buckets to drive the script: take them from a
        // sibling case over the same histogram
        let source_buckets = if c["status"] == 200 { all.clone() } else { cases[0]["response"].clone() };
        let got = run_case(&engine, req, &source_buckets);
        if got != want {
            failures.push(format!("{}\n  want: {:?}\n  got:  {:?}", req["aggs"]["h"]["aggs"]["p"], want, got));
        }
    }
    for f in &failures {
        eprintln!("---- {f}");
    }
    assert!(failures.is_empty(), "pipeline.json: {} of {} cases differ from Elasticsearch", failures.len(), cases.as_array().unwrap().len());
}
