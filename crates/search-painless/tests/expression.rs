//! Runs the captured `lang: expression` fixtures (`tests/fixtures/expression.json`, produced by
//! `tests/fixtures/capture_expression.py` against Elasticsearch 9.5.5 over the documents in
//! `painless-expr.docs.json`): each case is evaluated document by document with a mock host, the
//! way the search layer drives it (one instance per segment), and compared with
//! Elasticsearch's field values, scores, sort values, aggregations and filtered hits; a failure
//! is compared with the shard failure's `script_exception`.

// script errors are returned by value, as the library does
#![allow(clippy::result_large_err)]

use std::path::Path;

use search_painless::{Context, Engine, ScriptError, Settings, Value};
use serde_json::{json, Value as Json};

#[path = "support/mock_host.rs"]
mod mock_host;
use mock_host::MockHost;

const INDEX: &str = "painless-expr";

fn fixture(name: &str) -> Json {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A double as Elasticsearch writes it in a response (`"NaN"`, `"Infinity"` as strings).
fn num(x: f64) -> Json {
    if x.is_nan() {
        json!("NaN")
    } else if x.is_infinite() {
        json!(if x > 0.0 { "Infinity" } else { "-Infinity" })
    } else {
        json!(x)
    }
}

/// The shard failure's `script_exception` (or the top-level error).
fn shard_error(resp: &Json) -> Json {
    let e = &resp["error"];
    let r = e.get("failed_shards").and_then(|f| f[0].get("reason")).cloned().unwrap_or_else(|| e.clone());
    // queries wrap the script's failure ("failed to create query: …"): the search layer's part
    if r["type"] == "query_shard_exception" { r["caused_by"].clone() } else { r }
}

fn host(mapping: &Json, doc: &Json, seq: usize) -> MockHost {
    MockHost { index: INDEX.into(), mapping: mapping.clone(), doc: doc.clone(), score: 1.0, seq_no: seq as i64, search: true }
}

/// What one case gives against the documents, in the shape [`expected`] extracts from the
/// response, or the script error.
fn run(engine: &Engine, case: &Json, mapping: &Json, docs: &[Json]) -> Result<Json, ScriptError> {
    let ctx_name = case["context"].as_str().unwrap();
    let ctx = match ctx_name {
        "field" => Context::Field,
        "score" => Context::Score,
        "sort" => Context::NumberSort,
        "aggs" | "value" => Context::Aggs,
        "filter" => Context::Filter,
        other => panic!("context {other}"),
    };
    let script = &case["script"];
    let expr = engine.compile_expression(script["source"].as_str().unwrap(), ctx)?;
    let mut inst = expr.instance();
    inst.set_params(script.get("params").unwrap_or(&json!({})));
    let mut out = Vec::new();
    for (i, d) in docs.iter().enumerate() {
        let mut h = host(mapping, d, i);
        if ctx_name == "value" {
            // a value script: once per value of `l`; the document's first value decides
            let mut vals: Vec<f64> = match &d["l"] {
                Json::Array(a) => a.iter().filter_map(|x| x.as_f64()).collect(),
                Json::Null => Vec::new(),
                x => vec![x.as_f64().unwrap()],
            };
            vals.sort_by(f64::total_cmp);
            inst.set_document(i as i32);
            let mut s = 0.0;
            for v in vals {
                inst.set_var("_value", Value::Double(v));
                s += inst.execute_f64(&mut h)?;
            }
            out.push(s);
        } else if ctx == Context::Filter {
            out.push(if inst.execute_bool(&mut h)? { 1.0 } else { 0.0 });
        } else {
            out.push(inst.execute_f64(&mut h)?);
        }
    }
    let ids: Vec<String> = (1..=docs.len()).map(|i| i.to_string()).collect();
    Ok(match ctx_name {
        "field" => json!(out.iter().map(|x| json!([num(*x)])).collect::<Vec<_>>()),
        "score" => json!(out.iter().map(|x| (*x as f32) as f64).collect::<Vec<_>>()),
        "sort" => json!(ids.iter().zip(&out).map(|(id, x)| (id.clone(), num(*x))).collect::<Vec<_>>()),
        "aggs" => {
            let mn = out.iter().copied().fold(f64::INFINITY, f64::min);
            let mx = out.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            json!({ "mn": num(mn), "mx": num(mx) })
        }
        "value" => json!({ "s": num(out.iter().sum()) }),
        _ => json!(ids.iter().zip(&out).filter(|(_, x)| **x != 0.0).map(|(id, _)| id.clone()).collect::<Vec<_>>()),
    })
}

/// Elasticsearch's answer in the shape [`run`] produces.
fn expected(case: &Json) -> Result<Json, Json> {
    let resp = &case["response"];
    if resp.get("error").is_some() {
        return Err(shard_error(resp));
    }
    let hits = resp["hits"]["hits"].as_array().cloned().unwrap_or_default();
    Ok(match case["context"].as_str().unwrap() {
        "field" => json!(hits.iter().map(|h| h["fields"]["v"].clone()).collect::<Vec<_>>()),
        "score" => json!(hits.iter().map(|h| h["_score"].clone()).collect::<Vec<_>>()),
        "sort" => {
            let mut v: Vec<(String, Json)> = hits.iter().map(|h| (h["_id"].as_str().unwrap().to_string(), h["sort"][0].clone())).collect();
            v.sort_by(|a, b| a.0.cmp(&b.0));
            json!(v)
        }
        "aggs" => json!({ "mn": resp["aggregations"]["mn"]["value"], "mx": resp["aggregations"]["mx"]["value"] }),
        "value" => json!({ "s": resp["aggregations"]["s"]["value"] }),
        _ => json!(hits.iter().map(|h| h["_id"].clone()).collect::<Vec<_>>()),
    })
}

/// JSON equality with numbers compared as doubles (Elasticsearch writes `1.0`, we may write `1`).
fn same(want: &Json, have: &Json) -> bool {
    match (want, have) {
        (Json::Number(a), Json::Number(b)) => a.as_f64() == b.as_f64(),
        (Json::Array(a), Json::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same(x, y)),
        (Json::Object(a), Json::Object(b)) => a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).is_some_and(|w| same(v, w))),
        _ => want == have,
    }
}

#[test]
fn expressions() {
    check("expression.json");
}

/// Parser error paths, number edge cases, operator precedence and associativity, parameters
/// named like specials, and more field types and contexts (`expression2.json`).
#[test]
fn expressions_more() {
    check("expression2.json");
}

fn check(name: &str) {
    let engine = Engine::new(Settings { max_compilations_rate: None, ..Settings::default() });
    let cases = fixture(name);
    let mapping = fixture("painless-expr.mapping.json");
    let docs = fixture("painless-expr.docs.json");
    let docs = docs.as_array().unwrap();
    let mut failures = Vec::new();
    for case in cases.as_array().unwrap() {
        let src = case["script"]["source"].as_str().unwrap();
        let ctx = case["context"].as_str().unwrap();
        let got = run(&engine, case, &mapping, docs);
        let ok = match (expected(case), &got) {
            (Ok(w), Ok(h)) => same(&w, h),
            (Err(w), Err(e)) => w == e.to_json(),
            // `script_score` rejects negative scores (the search layer's check, not the script's)
            (Err(w), Ok(h)) if w["reason"].as_str().is_some_and(|r| r.contains("Must be a non-negative score")) => {
                h.as_array().unwrap().iter().any(|x| x.as_f64().unwrap() < 0.0)
            }
            _ => false,
        };
        if !ok {
            let want = expected(case).unwrap_or_else(|e| e);
            let have = match &got {
                Ok(h) => h.clone(),
                Err(e) => e.to_json(),
            };
            failures.push(format!("[{ctx}] {src}\n  want: {want}\n  got:  {have}"));
        }
    }
    for f in &failures {
        eprintln!("---- {f}");
    }
    assert!(failures.is_empty(), "{name}: {} of {} expression cases differ from Elasticsearch", failures.len(), cases.as_array().unwrap().len());
}

/// Contexts the engine does not support fail to compile with Elasticsearch's message; the
/// bucket contexts read their variables from `params`; `double_values` evaluates over
/// supplied values.
#[test]
fn contexts_and_bucket_scripts() {
    let engine = Engine::new(Settings { max_compilations_rate: None, ..Settings::default() });
    let e = engine.compile_expression("1", Context::Update).unwrap_err();
    assert_eq!(e.to_json(), json!({ "type": "illegal_argument_exception", "reason": "expression engine does not know how to handle script context [update]" }));
    let b = engine.compile_expression("a / b * 100", Context::BucketAggregation).unwrap();
    let mut i = b.instance();
    i.set_params(&json!({ "a": 3, "b": 4.0 }));
    assert_eq!(i.execute(&mut search_painless::NoHost).unwrap().as_f64(), Some(75.0));
    i.set_params(&json!({ "a": 3, "c": 1 }));
    let err = i.execute(&mut search_painless::NoHost).unwrap_err();
    assert!(err.to_json()["reason"].as_str().unwrap().ends_with("The variable [c] does not exist in the executable expressions script."));
    let s = engine.compile_expression("total > 10", Context::AggregationSelector).unwrap();
    let mut i = s.instance();
    i.set_params(&json!({ "total": 11 }));
    assert_eq!(i.execute(&mut search_painless::NoHost).unwrap().as_f64(), None);
    assert!(i.execute_bool(&mut search_painless::NoHost).unwrap());
    let dv = engine.compile_expression("doc['a'] + sqrt(x)", Context::DoubleValues).unwrap();
    assert_eq!(dv.variables(), ["doc['a']", "x"]);
    assert_eq!(dv.evaluate(&[1.0, 16.0]), 5.0);
    assert!(engine.compile_expression("_score * doc['f']", Context::Score).unwrap().needs_score());
}
