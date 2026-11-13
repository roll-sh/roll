//! Robustness over hostile requests and reduce layouts, on the Elasticsearch fixture suites:
//!
//! * a scalar-mutation fuzzer: every scalar of every fixture request (one at a time) is
//!   replaced by extreme or malformed values (`Long.MIN/MAX`, NaN, empty and `>` paths, empty
//!   arrays, huge doubles …). In debug builds Rust checks integer overflow, so any arithmetic
//!   that doesn't wrap or check like Java's shows up as a panic. Errors are fine; panics (and
//!   allocations over 1 GiB, which `tests/common` turns into aborts) are bugs. `FUZZ_FULL=1`
//!   tries every value at every position instead of a rotating subset.
//! * batched reduces: Elasticsearch reduces shard results in batches (`batched_reduce_size`)
//!   with partial reduces (`final_reduce: false`) followed by a final one with
//!   `has_batched_result`. A partial reduce of two shards followed by the final reduce must
//!   give the same response as one final reduce, for every order-insensitive fixture case.
//! * reduce memory: the reduce charges its inputs to the budget, partial reduces included.

mod common;

use std::sync::Arc;

use common::{TestIndex, diff_rel, run, run_with};
use search_aggs::Json;
use search_index::Searcher;

struct Suite {
    index: TestIndex,
    searchers: Vec<Searcher>,
    cases: Vec<(String, String, bool, bool)>, // name, body, typed_keys, ES answered with an error
}

fn load(name: &str) -> Suite {
    let path = format!("{}/tests/fixtures/aggs-{name}.json", env!("CARGO_MANIFEST_DIR"));
    let fx = Json::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut index = TestIndex::from_mapping(fx.get("mapping").unwrap());
    index.name = format!("aggs-fx-{name}");
    let docs = fx.get("docs").unwrap().as_array().unwrap();
    let shards: Vec<search_index::Shard> = fx
        .get("placement")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|ids| {
            let list: Vec<(String, Json)> = ids
                .as_array()
                .unwrap()
                .iter()
                .map(|i| {
                    let i = i.as_i64().unwrap() as usize;
                    (i.to_string(), docs[i].clone())
                })
                .collect();
            index.shard(&list, 1)
        })
        .collect();
    let searchers = shards.iter().map(|s| s.searcher()).collect();
    let cases = fx
        .get("cases")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c.get("name").and_then(Json::as_str).unwrap().to_string(),
                c.get("body").and_then(Json::as_str).unwrap().to_string(),
                c.get("typed_keys").and_then(Json::as_bool).unwrap_or(false),
                c.get("error").is_some(),
            )
        })
        .collect();
    Suite { index, searchers, cases }
}

/// Paths (object keys / array indices) of every scalar under `aggs`.
fn scalar_paths(v: &Json, path: &mut Vec<Result<String, usize>>, out: &mut Vec<Vec<Result<String, usize>>>) {
    match v {
        Json::Obj(m) => {
            for (k, x) in m {
                path.push(Ok(k.clone()));
                scalar_paths(x, path, out);
                path.pop();
            }
        }
        Json::Arr(a) => {
            for (i, x) in a.iter().enumerate() {
                path.push(Err(i));
                scalar_paths(x, path, out);
                path.pop();
            }
        }
        _ => out.push(path.clone()),
    }
}

fn at<'a>(v: &'a mut Json, path: &[Result<String, usize>]) -> &'a mut Json {
    let mut cur = v;
    for p in path {
        cur = match (cur, p) {
            (Json::Obj(m), Ok(k)) => &mut m.iter_mut().find(|(x, _)| x == k).unwrap().1,
            (Json::Arr(a), Err(i)) => &mut a[*i],
            _ => unreachable!(),
        };
    }
    cur
}

fn mutations() -> Vec<Json> {
    vec![
        Json::Int(i64::MIN),
        Json::Int(i64::MAX),
        Json::Int(i64::MIN + 1),
        Json::Int(-1),
        Json::Int(0),
        Json::Int(i32::MAX as i64 + 1),
        Json::Float(1e300),
        Json::Float(-1e300),
        Json::Float(4.9e-324),
        Json::Str("NaN".into()),
        Json::Str("Infinity".into()),
        Json::Str("-Infinity".into()),
        Json::Str(String::new()),
        Json::Str(">".into()),
        Json::Str(".".into()),
        Json::Str("a>".into()),
        Json::Str("9223372036854775807".into()),
        Json::Str("-9223372036854775808ms".into()),
        Json::Str("1000000000d".into()),
        Json::Arr(Vec::new()),
        Json::Obj(Vec::new()),
        Json::Null,
    ]
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
}

/// Where the last panic happened (set by the fuzzer's panic hook).
static LAST_PANIC: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

fn fuzz_suite(name: &str, failures: &mut Vec<String>) -> usize {
    let suite = load(name);
    let full = std::env::var("FUZZ_FULL").is_ok();
    let values = mutations();
    let mut runs = 0;
    let mut k = 0usize;
    for (cname, body, typed, _) in &suite.cases {
        let parsed = Json::parse(body).unwrap();
        let Some(aggs) = parsed.get("aggs") else { continue };
        let mut paths = Vec::new();
        scalar_paths(aggs, &mut Vec::new(), &mut paths);
        for path in &paths {
            // A rotating third of the values per position keeps the default run short while
            // still covering every value at every kind of position across the suite.
            let picked: Vec<&Json> = if full { values.iter().collect() } else { values.iter().skip(k % 3).step_by(3).collect() };
            k += 1;
            for v in picked {
                let mut req = parsed.clone();
                *at(req.get_mut("aggs").unwrap(), path) = v.clone();
                let text = req.to_string();
                runs += 1;
                if let Err(p) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&suite.index, &suite.searchers, &text, *typed))) {
                    let location = LAST_PANIC.lock().unwrap().clone();
                    // The test index's own query parser (filters, background filters) isn't
                    // the crate under test: search-es parses queries in production.
                    if location.contains("tests/common") {
                        continue;
                    }
                    failures.push(format!("[{name}/{cname}] {location}: {}\n  body={text}", panic_message(&*p)));
                }
            }
        }
    }
    runs
}

#[test]
fn mutated_requests_never_panic() {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        *LAST_PANIC.lock().unwrap() = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
    }));
    let mut failures = Vec::new();
    let mut runs = 0;
    for suite in ["fix3", "err3"] {
        runs += fuzz_suite(suite, &mut failures);
    }
    std::panic::set_hook(hook);
    failures.dedup();
    assert!(failures.is_empty(), "{} of {runs} mutated requests panicked:\n{}", failures.len(), failures.join("\n"));
}

/// Elasticsearch's own batching over these shard counts never happens (batches hold 512
/// results), so these cases legitimately depend on the reduce order: digests merge
/// order-sensitively, variable_width merges clusters greedily, and terms-like aggregations
/// pick a different shard_size-limited candidate set or error bound once partially reduced.
fn order_sensitive(body: &str) -> bool {
    ["percentile", "median_absolute_deviation", "boxplot", "variable_width", "significant", "terms", "top_metrics", "top_hits", "sampler", "categorize_text", "frequent_item_sets", "geo_line", "scripted_metric"]
        .iter()
        .any(|t| body.contains(&format!("\"{t}")))
}

/// Objects with their keys sorted: a reduce's multi-result order follows Java `HashMap`
/// iteration, which depends on the order results were merged in (so it legitimately differs
/// between batch layouts, in Elasticsearch too).
fn sorted_keys(v: &Json) -> Json {
    match v {
        Json::Obj(m) => {
            let mut m: Vec<(String, Json)> = m.iter().map(|(k, x)| (k.clone(), sorted_keys(x))).collect();
            m.sort_by(|a, b| a.0.cmp(&b.0));
            Json::Obj(m)
        }
        Json::Arr(a) => Json::Arr(a.iter().map(sorted_keys).collect()),
        other => other.clone(),
    }
}

/// `batched_reduce_size: 2`: the first two shard results are partially reduced, then the
/// final reduce merges that partial result with the remaining shards.
fn run_batched(index: &TestIndex, searchers: &[Searcher], body_text: &str, typed: bool) -> Result<Json, search_aggs::AggError> {
    let body = Json::parse(body_text).unwrap();
    let aggs = body.get("aggs").cloned().unwrap_or(Json::obj());
    let opts = search_aggs::ParseOptions { src: Some(body_text.as_bytes()), base: vec![search_aggs::xparser::Seg::Key("aggs".into())], ..Default::default() };
    let req = search_aggs::parse(&aggs, &opts)?;
    if !req.validate().is_empty() {
        return run(index, searchers, body_text, typed);
    }
    let lookup = common::lookup(index);
    let q = common::top_query(&lookup, &body);
    let mut shards = Vec::new();
    for s in searchers {
        shards.push(req.run(&lookup, s, &q, &search_aggs::ShardOptions::default())?);
    }
    let rest = shards.split_off(2.min(shards.len()));
    let partial = search_aggs::ReduceOptions { final_reduce: false, ..Default::default() };
    let mut inputs = vec![search_aggs::reduce(shards, &req, &partial)?];
    inputs.extend(rest);
    let last = search_aggs::ReduceOptions { has_batched_result: true, ..Default::default() };
    Ok(search_aggs::reduce(inputs, &req, &last)?.to_json(typed))
}

#[test]
fn batched_reduces_match_single_reduces() {
    let mut failures = Vec::new();
    let mut compared = 0;
    for name in ["main3", "err3", "rnd3", "rnddate2", "fix3"] {
        let suite = load(name);
        for (cname, body, typed, es_error) in &suite.cases {
            if *es_error || order_sensitive(body) {
                continue;
            }
            let single = run(&suite.index, &suite.searchers, body, *typed);
            let batched = run_batched(&suite.index, &suite.searchers, body, *typed);
            compared += 1;
            match (single, batched) {
                (Ok(a), Ok(b)) => {
                    if let Some(d) = diff_rel(&sorted_keys(&b), &sorted_keys(&a), "aggregations", 1e-12) {
                        failures.push(format!("[{name}/{cname}] {d}"));
                    }
                }
                (Err(a), Err(b)) if a.kind == b.kind && a.reason == b.reason => {}
                (a, b) => failures.push(format!("[{name}/{cname}] single {:?} vs batched {:?}", a.err(), b.err())),
            }
        }
    }
    assert!(compared > 300, "only {compared} cases compared");
    assert!(failures.is_empty(), "{} of {compared} batched reduces differ:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn reduces_charge_the_budget() {
    let suite = load("fix3");
    let body = r#"{"aggs":{"h":{"histogram":{"field":"n","interval":1},"aggs":{"k":{"terms":{"field":"k"}}}}}}"#;
    let tight = search_aggs::ReduceOptions { budget: Some(search_aggs::MemoryBudget::new(1024)), ..Default::default() };
    let e = run_with(&suite.index, &suite.searchers, body, false, None, &tight).unwrap_err();
    assert_eq!(e.kind, "circuit_breaking_exception");
    assert!(e.reason.contains("[<reduce_aggs>]"), "{}", e.reason);
    let partial = search_aggs::ReduceOptions { final_reduce: false, ..tight.clone() };
    assert_eq!(run_with(&suite.index, &suite.searchers, body, false, None, &partial).unwrap_err().kind, "circuit_breaking_exception");
    // Everything is released afterwards, and a roomy budget passes.
    let budget = search_aggs::MemoryBudget::new(64 << 20);
    let roomy = search_aggs::ReduceOptions { budget: Some(Arc::clone(&budget)), ..Default::default() };
    run_with(&suite.index, &suite.searchers, body, false, None, &roomy).unwrap();
    assert_eq!(budget.used(), 0);
}

/// Elasticsearch's JVM dies on these (so they have no fixture); they must answer here.
#[test]
fn requests_that_kill_elasticsearch_answer() {
    let suite = load("fix3");
    for body in [
        r#"{"aggs":{"h":{"auto_date_histogram":{"field":"t","buckets":5,"missing":9223372036854775807}}}}"#,
        r#"{"aggs":{"h":{"auto_date_histogram":{"field":"t","buckets":5,"missing":-9223372036854775808}}}}"#,
        r#"{"aggs":{"p":{"percentiles":{"field":"d","tdigest":{"compression":1e300}}}}}"#,
        r#"{"aggs":{"p":{"boxplot":{"field":"d","compression":"Infinity"}}}}"#,
    ] {
        let _ = run(&suite.index, &suite.searchers, body, false);
    }
}
