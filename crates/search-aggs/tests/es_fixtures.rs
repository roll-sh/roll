//! Responses captured from Elasticsearch 9.5.5 (`tests/fixtures/aggs-*.json`, regenerate with
//! `gen_fixtures.py`) replayed over search-index shards holding the same documents in the same
//! shard placement and order; every aggregation must render identically (numbers bit-exact),
//! errors must have the same type and reason.
//!
//! Each suite also runs with every shard split into several segments (results must not depend
//! on the segment layout). `FIXTURE_CASE=<name>` runs one case; `FIXTURE_SHOW=1` prints the
//! responses that differ.

mod common;

use common::{TestIndex, diff_rel, run};
use search_aggs::Json;

fn load(name: &str) -> Json {
    let path = format!("{}/tests/fixtures/aggs-{name}.json", env!("CARGO_MANIFEST_DIR"));
    Json::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Compares our error with Elasticsearch's (shard failures are compared to the shard's reason).
fn error_diff(got: &search_aggs::AggError, want: &Json) -> Option<String> {
    let want = if want.get("type").and_then(Json::as_str) == Some("search_phase_execution_exception") {
        // Shard failures carry the reason; reduce failures (no failed shard) the cause.
        want.get("failed_shards")
            .and_then(|f| f.as_array())
            .and_then(|a| a.first())
            .and_then(|f| f.get("reason"))
            .or_else(|| want.get("caused_by"))
            .unwrap_or(want)
    } else {
        want
    };
    let wt = want.get("type").and_then(Json::as_str).unwrap_or("");
    let wr = without_identity_hashes(want.get("reason").and_then(Json::as_str).unwrap_or(""));
    if got.kind != wt || without_identity_hashes(&got.reason) != wr {
        return Some(format!("error: got [{}] {}\n  want [{wt}] {wr}", got.kind, got.reason));
    }
    if let (Some(c), Some(wc)) = (&got.caused_by, want.get("caused_by")) {
        return error_diff(c, wc);
    }
    None
}

/// Java identity hash codes (`Object.toString()`'s `@1b6d3586`) can't be reproduced: compare
/// them as `@0`.
fn without_identity_hashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if c == '@' && chars.peek().is_some_and(char::is_ascii_hexdigit) {
            while chars.peek().is_some_and(char::is_ascii_hexdigit) {
                chars.next();
            }
            out.push('0');
        }
    }
    out
}

/// `rel`: relative tolerance for doubles (0: reduces follow Elasticsearch's merge order, so
/// even multi-shard sums match to the bit).
fn run_suite(name: &str, segments: usize, rel: f64) {
    let fx = load(name);
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
            index.shard(&list, segments)
        })
        .collect();
    let searchers: Vec<search_index::Searcher> = shards.iter().map(|s| s.searcher()).collect();
    let only = std::env::var("FIXTURE_CASE").ok();
    let mut failures = Vec::new();
    let mut ran = 0;
    for case in fx.get("cases").unwrap().as_array().unwrap() {
        let cname = case.get("name").and_then(Json::as_str).unwrap();
        if only.as_deref().is_some_and(|o| o != cname) {
            continue;
        }
        // The random sampler restarts its random stream in every segment: its sample depends on
        // the segment layout, which only the one-segment runs share with Elasticsearch.
        if segments > 1 && cname.starts_with("random_sampler") && !cname.contains("_err_") && cname != "random_sampler_all" {
            continue;
        }
        ran += 1;
        let body = case.get("body").and_then(Json::as_str).unwrap();
        let typed = case.get("typed_keys").and_then(Json::as_bool).unwrap_or(false);
        let got = run(&index, &searchers, body, typed);
        match (got, case.get("response"), case.get("error")) {
            (Ok(g), Some(want), _) => {
                if let Some(d) = diff_rel(&g, want, "aggregations", rel) {
                    if std::env::var("FIXTURE_SHOW").is_ok() {
                        eprintln!("[{name}/{cname}] got: {g}");
                    }
                    failures.push(format!("[{name}/{cname}] {d}"));
                }
            }
            (Err(e), _, Some(want)) => {
                if let Some(d) = error_diff(&e, want) {
                    failures.push(format!("[{name}/{cname}] {d}"));
                }
            }
            (Ok(g), None, Some(want)) => failures.push(format!("[{name}/{cname}] expected error {want}, got {g}")),
            (Err(e), Some(_), _) => failures.push(format!("[{name}/{cname}] unexpected error {}: {}", e.kind, e.reason)),
            _ => failures.push(format!("[{name}/{cname}] malformed fixture")),
        }
    }
    if !failures.is_empty() {
        panic!("{} of {ran} cases failed:\n{}", failures.len(), failures.join("\n"));
    }
}

#[test]
fn main_single_shard() {
    run_suite("main1", 1, 0.0);
}

#[test]
fn main_three_shards() {
    run_suite("main3", 1, 0.0);
}

#[test]
fn main_single_shard_four_segments() {
    run_suite("main1", 4, 0.0);
}

#[test]
fn main_three_shards_three_segments() {
    run_suite("main3", 3, 0.0);
}

#[test]
fn join_two_shards() {
    run_suite("join2", 1, 0.0);
}

#[test]
fn join_two_shards_three_segments() {
    run_suite("join2", 3, 0.0);
}

#[test]
fn bulk_three_shards() {
    run_suite("bulk3", 1, 0.0);
}

#[test]
fn bulk_three_shards_three_segments() {
    run_suite("bulk3", 3, 0.0);
}

#[test]
fn range_fields_two_shards() {
    run_suite("range2", 1, 0.0);
}

#[test]
fn range_fields_two_shards_three_segments() {
    run_suite("range2", 3, 0.0);
}

#[test]
fn time_series_two_shards() {
    run_suite("ts2", 1, 0.0);
}

#[test]
fn time_series_two_shards_three_segments() {
    run_suite("ts2", 3, 0.0);
}

#[test]
fn pre_aggregated_two_shards() {
    run_suite("pre2", 1, 0.0);
}

#[test]
fn pre_aggregated_two_shards_three_segments() {
    run_suite("pre2", 3, 0.0);
}

#[test]
fn exponential_histograms_two_shards() {
    run_suite("exp2", 1, 0.0);
}

#[test]
fn exponential_histograms_two_shards_three_segments() {
    run_suite("exp2", 3, 0.0);
}

#[test]
fn shapes_two_shards() {
    run_suite("shape2", 1, 0.0);
}

#[test]
fn shapes_two_shards_three_segments() {
    run_suite("shape2", 3, 0.0);
}
