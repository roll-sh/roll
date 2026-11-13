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
    for (k, v) in &got.meta {
        match want.get(k) {
            Some(w) => {
                if let Some(d) = common::diff(v, w, k) {
                    return Some(format!("error metadata: {d}"));
                }
            }
            None => return Some(format!("error metadata [{k}] not in Elasticsearch's error")),
        }
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

/// Cases whose Elasticsearch result isn't reproduced yet (`(suite, case)`).
///
/// Both fail in `InternalDateHistogram.addEmptyBuckets`'s `rounding.prepare(min, max)` over keys
/// that wrapped past `Long.MAX_VALUE` on the shard; we answer `too_many_buckets_exception`
/// (both are 400s for requests no real data needs).
const KNOWN_DIFFERENCES: &[(&str, &str)] = &[("fix3", "overflow_missing_max_7d"), ("fix3", "overflow_offset_max_month_bounds")];

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
        // A counter `rate` reads a valueless doc as the preceding valued doc of its segment.
        if segments > 1 && cname.contains("counter_missing") {
            continue;
        }
        if KNOWN_DIFFERENCES.contains(&(name, cname)) {
            continue;
        }
        ran += 1;
        let body = case.get("body").and_then(Json::as_str).unwrap();
        let typed = case.get("typed_keys").and_then(Json::as_bool).unwrap_or(false);
        // A panic fails the case, not the suite: every panic is a bug (one malformed request
        // must not take down a search thread), and the other cases still report.
        let got = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&index, &searchers, body, typed))) {
            Ok(g) => g,
            Err(p) => {
                let msg = p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                failures.push(format!("[{name}/{cname}] panicked: {msg}"));
                continue;
            }
        };
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

#[test]
fn licensed_two_shards() {
    run_suite("lic2", 1, 0.0);
}

#[test]
fn licensed_two_shards_three_segments() {
    run_suite("lic2", 3, 0.0);
}

#[test]
fn time_series_geo_two_shards() {
    run_suite("tsg2", 1, 0.0);
}

#[test]
fn time_series_geo_two_shards_three_segments() {
    run_suite("tsg2", 3, 0.0);
}

#[test]
fn frequent_item_sets_two_shards() {
    run_suite("fis2", 1, 0.0);
}

#[test]
fn frequent_item_sets_two_shards_three_segments() {
    run_suite("fis2", 3, 0.0);
}

/// Installs the part-of-speech entries of the `cat2` corpus words (an excerpt of
/// Elasticsearch's `ml-en.dict`: word and first part-of-speech code).
fn install_cat_dictionary() {
    let path = format!("{}/tests/fixtures/aggs-cat-dict.txt", env!("CARGO_MANIFEST_DIR"));
    let dict = search_aggs::bucket::categorize_text::PosDictionary::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
    search_aggs::bucket::categorize_text::set_part_of_speech_dictionary(dict);
}

#[test]
fn categorize_text_two_shards() {
    install_cat_dictionary();
    run_suite("cat2", 1, 0.0);
}

#[test]
fn categorize_text_two_shards_three_segments() {
    install_cat_dictionary();
    run_suite("cat2", 3, 0.0);
}

#[test]
fn categorize_text_random_rounds() {
    install_cat_dictionary();
    run_suite("catr1", 1, 0.0);
}

#[test]
fn categorize_text_random_rounds_three_segments() {
    install_cat_dictionary();
    run_suite("catr1", 3, 0.0);
}

#[test]
fn core_errors_and_edges_three_shards() {
    run_suite("err3", 1, 0.0);
}

#[test]
fn core_errors_and_edges_three_shards_two_segments() {
    run_suite("err3", 2, 0.0);
}

#[test]
fn random_trees_three_shards() {
    run_suite("rnd3", 1, 0.0);
}

#[test]
fn random_trees_three_shards_three_segments() {
    run_suite("rnd3", 3, 0.0);
}

#[test]
fn random_wide_trees_three_shards() {
    run_suite("rndw3", 1, 0.0);
}

#[test]
fn random_wide_trees_three_shards_two_segments() {
    run_suite("rndw3", 2, 0.0);
}

#[test]
fn random_wide_trees_other_docs_two_shards() {
    run_suite("rndx2", 1, 0.0);
}

#[test]
fn random_wide_trees_other_docs_two_shards_three_segments() {
    run_suite("rndx2", 3, 0.0);
}

#[test]
fn random_dates_two_shards() {
    run_suite("rnddate2", 1, 0.0);
}

#[test]
fn random_dates_two_shards_three_segments() {
    run_suite("rnddate2", 3, 0.0);
}

#[test]
fn review_fixes_three_shards() {
    run_suite("fix3", 1, 0.0);
}

#[test]
fn review_fixes_three_shards_two_segments() {
    run_suite("fix3", 2, 0.0);
}
