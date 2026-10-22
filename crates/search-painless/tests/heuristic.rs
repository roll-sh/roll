//! Runs the captured `script_heuristic` fixtures (`tests/fixtures/heuristic.json`, produced by
//! `tests/fixtures/capture_heuristic.py` against Elasticsearch 9.5.5): `significant_terms`
//! scoring every candidate term with a script.
//!
//! The candidates and their counts come from Elasticsearch (a baseline run scoring each 1).
//! Like `ScriptHeuristic`, the harness builds one params map — the user params plus
//! `LongAccessor`s for `_subset_freq`, `_superset_freq`, `_subset_size` and `_superset_size` —
//! updates the accessors per term and runs the script; terms scoring above zero are the
//! buckets, compared (key and score) with Elasticsearch's. A failing script fails the shard:
//! its `script_exception` is compared with the shard failure.

use std::path::Path;

use search_painless::es::misc::{long_accessor, set_long_accessor};
use search_painless::value::Value;
use search_painless::{Context, Engine, NoHost, Settings};
use serde_json::{json, Value as Json};

fn fixture(name: &str) -> Json {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Our buckets `[(key, score)]` sorted by key, or the script error.
fn run_case(engine: &Engine, case: &Json) -> Result<Vec<(String, f64)>, Json> {
    let req = &case["request"];
    let src = req["script"].as_str().unwrap();
    let script = engine.compile(src, Context::ScriptHeuristic).map_err(|e| e.to_json())?;
    let cand = &case["candidates"];
    let (subset_freq, superset_freq) = (long_accessor(0), long_accessor(0));
    let subset_size = long_accessor(cand["doc_count"].as_i64().unwrap());
    let superset_size = long_accessor(cand["bg_count"].as_i64().unwrap());
    let params = match Value::from_json_map(req.get("params").unwrap_or(&json!({}))) {
        Value::Map(m) => m,
        _ => unreachable!(),
    };
    for (k, v) in [("_subset_freq", &subset_freq), ("_superset_freq", &superset_freq), ("_subset_size", &subset_size), ("_superset_size", &superset_size)] {
        params.insert_raw(Value::str(k), v.clone());
    }
    let mut inst = script.instance();
    inst.set_params_value(Value::Map(params.clone()));
    let mut out = Vec::new();
    for b in cand["buckets"].as_array().unwrap() {
        set_long_accessor(&subset_freq, b["doc_count"].as_i64().unwrap());
        set_long_accessor(&superset_freq, b["bg_count"].as_i64().unwrap());
        let score = inst.execute_f64(&mut NoHost).map_err(|e| e.to_json())?;
        if score > 0.0 {
            out.push((b["key"].as_str().unwrap().to_string(), score));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

#[test]
fn script_heuristic() {
    let engine = Engine::new(Settings { max_compilations_rate: None, ..Settings::default() });
    let cases = fixture("heuristic.json");
    let mut failures = Vec::new();
    for c in cases.as_array().unwrap() {
        let src = c["request"]["script"].as_str().unwrap();
        let resp = &c["response"];
        let want = match resp.get("aggregations") {
            Some(a) => {
                let mut v: Vec<(String, f64)> =
                    a["s"]["buckets"].as_array().unwrap().iter().map(|b| (b["key"].as_str().unwrap().to_string(), b["score"].as_f64().unwrap())).collect();
                v.sort_by(|a, b| a.0.cmp(&b.0));
                Ok(v)
            }
            None => Err(resp["error"]["failed_shards"][0]["reason"].clone()),
        };
        let have = run_case(&engine, c);
        if want != have {
            failures.push(format!("{src}\n  want: {want:?}\n  got:  {have:?}"));
        }
    }
    for f in &failures {
        eprintln!("---- {f}");
    }
    assert!(failures.is_empty(), "{} of {} script_heuristic cases differ from Elasticsearch", failures.len(), cases.as_array().unwrap().len());
}
