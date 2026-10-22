//! Renders the captured `_render/template` fixtures (`tests/fixtures/mustache.json`, produced by
//! `tests/fixtures/capture_mustache.py` against Elasticsearch 9.5.5) and compares the output
//! (parsed as JSON, as the API returns it) or the `script_exception` with Elasticsearch's.

#![allow(clippy::result_large_err)]

use std::path::Path;

use search_painless::{Context, Engine, Settings};
use serde_json::Value as Json;

#[test]
fn render_template() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mustache.json");
    let cases: Vec<Json> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let engine = Engine::new(Settings { max_compilations_rate: None, ..Settings::default() });
    let mut failures = Vec::new();
    for c in &cases {
        let req = &c["request"];
        if req.get("options").is_some() {
            continue;
        }
        let src = req["source"].as_str().unwrap();
        let resp = &c["response"];
        let rendered = engine.compile_template(src, Context::Template, &[]).and_then(|t| t.render_json(&req["params"]));
        let ok = match (resp.get("template_output"), &rendered) {
            (Some(want), Ok(out)) => serde_json::from_str::<Json>(out).ok().as_ref() == Some(want),
            (Some(_), Err(_)) => false,
            (None, _) => match resp["error"]["type"].as_str() {
                Some("script_exception") => match &rendered {
                    Err(e) => {
                        let mut want = resp["error"].clone();
                        want.as_object_mut().unwrap().remove("root_cause");
                        let mut have = e.to_response()["error"].clone();
                        have.as_object_mut().unwrap().remove("root_cause");
                        have == want
                    }
                    Ok(_) => false,
                },
                // the output is not JSON
                Some("x_content_parse_exception") => matches!(&rendered, Ok(out) if serde_json::from_str::<Json>(out).is_err()),
                // not a search body (or not JSON): the search layer rejects it
                _ => rendered.is_ok(),
            },
        };
        if !ok {
            failures.push(format!("{src}\n  want: {resp}\n  got:  {:?}", rendered.map_err(|e| e.to_response())));
        }
    }
    for f in &failures {
        eprintln!("---- {f}");
    }
    assert!(failures.is_empty(), "mustache.json: {} of {} cases differ from Elasticsearch", failures.len(), cases.len());
}
