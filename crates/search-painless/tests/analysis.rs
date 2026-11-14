//! Runs the captured analysis-predicate fixtures (`tests/fixtures/analysis.json`, produced by
//! `tests/fixtures/capture_analysis.py` against Elasticsearch 9.5.5): a `predicate_token_filter`
//! script over the tokens of an `_analyze` request. The tokens come from Elasticsearch's own
//! analysis (term, position, offsets, type, position length, keyword flag); the harness runs the
//! script on each the way `AnalysisPredicateScript` does and compares the kept terms, or the
//! error, with Elasticsearch's answer.

use std::path::Path;

use search_painless::es::misc::{analysis_token, Token};
use search_painless::{Context, Engine, NoHost, ScriptError, Settings};
use serde_json::{json, Value as Json};

fn fixture(name: &str) -> Json {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn error_body(e: &ScriptError) -> Json {
    e.to_response()["error"].clone()
}

fn run_case(engine: &Engine, case: &Json) -> Result<Json, Json> {
    let src = case["request"]["script"]["source"].as_str().unwrap();
    let script = engine.compile(src, Context::Analysis).map_err(|e| error_body(&e))?;
    let mut inst = script.instance();
    let mut kept = Vec::new();
    let mut prev = -1i64;
    for t in case["tokens"].as_array().into_iter().flatten() {
        let position = t["position"].as_i64().unwrap();
        let token = Token {
            term: t["token"].as_str().unwrap().to_string(),
            position: position as i32,
            position_increment: (position - prev) as i32,
            position_length: t.get("positionLength").and_then(|p| p.as_i64()).unwrap_or(1) as i32,
            start_offset: t["start_offset"].as_i64().unwrap() as i32,
            end_offset: t["end_offset"].as_i64().unwrap() as i32,
            type_: t["type"].as_str().unwrap().to_string(),
            keyword: t["keyword"].as_bool().unwrap_or(false),
        };
        prev = position;
        inst.set_var("token", analysis_token(token));
        if inst.execute_bool(&mut NoHost).map_err(|e| error_body(&e))? {
            kept.push(json!(t["token"]));
        }
    }
    Ok(Json::Array(kept))
}

#[test]
fn predicate_token_filter() {
    let engine = Engine::new(Settings { max_compilations_rate: None, ..Settings::default() });
    let cases = fixture("analysis.json");
    let mut failures = Vec::new();
    for c in cases.as_array().unwrap() {
        let src = c["request"]["script"]["source"].as_str().unwrap();
        let resp = &c["response"];
        let want = match resp.get("tokens") {
            Some(tokens) => Ok(Json::Array(tokens.as_array().unwrap().iter().map(|t| t["token"].clone()).collect())),
            None => Err(resp["error"].clone()),
        };
        let have = run_case(&engine, c);
        if want != have {
            failures.push(format!("{src}\n  want: {want:?}\n  got:  {have:?}"));
        }
    }
    for f in &failures {
        eprintln!("---- {f}");
    }
    assert!(failures.is_empty(), "{} of {} analysis cases differ from Elasticsearch", failures.len(), cases.as_array().unwrap().len());
}
