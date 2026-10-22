//! Runs the captured scripted-similarity fixtures (`tests/fixtures/similarity.json`, produced
//! by `tests/fixtures/capture_similarity.py` against Elasticsearch 9.5.5).
//!
//! For every hit the explanation gives the inputs of each term's similarity (query boost,
//! field and term statistics, frequency, decoded length); the harness computes the weight
//! script and the score script from them the way `ScriptedSimilarity` does and compares each
//! term's score, the weight and the hit's summed score with Elasticsearch's. Index creation
//! first validates a similarity by scoring sample statistics (`SimilarityService`); a script
//! that fails there (a compile or runtime error) fails the index creation, and the harness
//! reproduces that run. Validation rejecting the scores themselves (negative, or not monotonic)
//! is the index service's business and is not compared.

// the helpers pass the engine's own `ScriptError` through
#![allow(clippy::result_large_err)]

use std::path::Path;

use search_painless::es::misc::{similarity_doc, similarity_field, similarity_query, similarity_term};
use search_painless::{Context, Engine, NoHost, Script, ScriptError, Settings, Value};
use serde_json::Value as Json;

fn fixture(name: &str) -> Json {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

struct Stats {
    boost: f32,
    doc_count: i64,
    sum_doc_freq: i64,
    sum_total_term_freq: i64,
    doc_freq: i64,
    total_term_freq: i64,
}

/// `computeWeight`: 1 without a weight script.
fn weight(script: Option<&Script>, s: &Stats) -> Result<f64, ScriptError> {
    let Some(script) = script else { return Ok(1.0) };
    let mut inst = script.instance();
    inst.set_var("query", similarity_query(s.boost));
    inst.set_var("field", similarity_field(s.doc_count, s.sum_doc_freq, s.sum_total_term_freq));
    inst.set_var("term", similarity_term(s.doc_freq, s.total_term_freq));
    inst.execute_f64(&mut NoHost)
}

/// `SimScorer.score(freq, norm)`: the script's double as a float.
fn score(script: &Script, w: f64, s: &Stats, freq: f32, length: i32) -> Result<f32, ScriptError> {
    let mut inst = script.instance();
    inst.set_var("weight", Value::Double(w));
    inst.set_var("query", similarity_query(s.boost));
    inst.set_var("field", similarity_field(s.doc_count, s.sum_doc_freq, s.sum_total_term_freq));
    inst.set_var("term", similarity_term(s.doc_freq, s.total_term_freq));
    inst.set_var("doc", similarity_doc(length, freq));
    Ok(inst.execute_f64(&mut NoHost)? as f32)
}

/// `SimilarityService.validateSimilarity`'s three runs over sample statistics, for the script
/// errors they raise.
fn validate(weight_script: Option<&Script>, script: &Script) -> Result<(), ScriptError> {
    let s = Stats { boost: 2.0, doc_count: 1100, sum_doc_freq: 2000, sum_total_term_freq: 3000, doc_freq: 100, total_term_freq: 130 };
    for _ in 0..2 {
        let w = weight(weight_script, &s)?;
        for freq in 1..=10 {
            score(script, w, &s, freq as f32, 20)?;
        }
    }
    let w = weight(weight_script, &s)?;
    for length in 1..=10 {
        score(script, w, &s, 1.0, length)?;
    }
    Ok(())
}

fn error_json(e: &ScriptError) -> Json {
    e.to_response()["error"].clone()
}

fn same_score(a: f32, b: &Json) -> bool {
    match b.as_f64() {
        Some(x) => a == x as f32,
        None => a.is_nan(),
    }
}

#[test]
fn scripted_similarity() {
    let engine = Engine::new(Settings { max_compilations_rate: None, ..Settings::default() });
    let cases = fixture("similarity.json");
    let mut failures = Vec::new();
    let mut compared = 0;
    for c in cases.as_array().unwrap() {
        let req = &c["request"];
        let src = req["script"].as_str().unwrap();
        let compiled = engine.compile(src, Context::Similarity);
        let weight_compiled = req.get("weight_script").map(|w| engine.compile(w.as_str().unwrap(), Context::SimilarityWeight));
        let Some(hits) = c.get("hits") else {
            // index creation failed: compiling the weight script, the script, or validating
            let want = &c["response"]["error"];
            if want["type"] != "script_exception" {
                continue;
            }
            compared += 1;
            let have = match (weight_compiled, compiled) {
                (Some(Err(e)), _) | (_, Err(e)) => Some(error_json(&e)),
                (w, Ok(s)) => {
                    let w = w.map(|w| w.unwrap());
                    validate(w.as_deref(), &s).err().map(|e| error_json(&e))
                }
            };
            if have.as_ref() != Some(want) {
                failures.push(format!("{src}\n  want: {want}\n  got:  {have:?}"));
            }
            continue;
        };
        let script = compiled.unwrap_or_else(|e| panic!("{src}: {e:?}"));
        let weight_script = weight_compiled.map(|w| w.unwrap());
        for hit in hits.as_array().unwrap() {
            compared += 1;
            let mut sum = 0f64;
            for t in hit["terms"].as_array().unwrap() {
                let n = |k: &str| t[k].as_f64().unwrap();
                let s = Stats {
                    boost: n("query.boost") as f32,
                    doc_count: n("field.docCount") as i64,
                    sum_doc_freq: n("field.sumDocFreq") as i64,
                    sum_total_term_freq: n("field.sumTotalTermFreq") as i64,
                    doc_freq: n("term.docFreq") as i64,
                    total_term_freq: n("term.totalTermFreq") as i64,
                };
                let w = weight(weight_script.as_deref(), &s).unwrap();
                if w as f32 != n("weight") as f32 {
                    failures.push(format!("{src}: weight {} vs {}", w as f32, n("weight")));
                }
                let got = score(&script, w, &s, n("doc.freq") as f32, n("doc.length") as i32).unwrap();
                if !same_score(got, &t["score"]) {
                    failures.push(format!("{src}: term score {got} vs {}", t["score"]));
                }
                sum += got as f64;
            }
            if !same_score(sum as f32, &hit["score"]) {
                failures.push(format!("{src}: doc {} score {} vs {}", hit["id"], sum as f32, hit["score"]));
            }
        }
    }
    for f in &failures {
        eprintln!("---- {f}");
    }
    assert!(failures.is_empty(), "{} of {compared} similarity results differ from Elasticsearch", failures.len());
}
