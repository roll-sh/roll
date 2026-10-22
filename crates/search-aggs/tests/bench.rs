//! Ignored benchmarks over millions of documents: `terms`, `date_histogram` and `cardinality`
//! collection and reduce, timed per run.
//!
//! `cargo test -p search-aggs --release --test bench -- --ignored --nocapture`
//! (`BENCH_DOCS` sets the document count, 2 000 000 by default; `BENCH_SHARDS` the shard count).

mod common;

use std::time::Instant;

use common::{TestIndex, run};
use search_aggs::Json;

fn layout() -> (usize, usize) {
    let docs = std::env::var("BENCH_DOCS").ok().and_then(|v| v.parse().ok()).unwrap_or(2_000_000);
    let shards = std::env::var("BENCH_SHARDS").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    (docs, shards)
}

/// Builds the shards: a keyword with 10 000 terms (skewed), a long, a date over a year.
fn build() -> (TestIndex, Vec<search_index::Shard>) {
    let (count, shards) = layout();
    let mapping = Json::parse(r#"{"properties":{"k":{"type":"keyword"},"n":{"type":"long"},"t":{"type":"date"}}}"#).unwrap();
    let index = TestIndex::from_mapping(&mapping);
    let started = Instant::now();
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_f491_4f6c_dd1d)
    };
    let mut per: Vec<Vec<(String, Json)>> = vec![Vec::with_capacity(count / shards + 1); shards];
    for i in 0..count {
        let r = next();
        // Roughly Zipfian terms: the square of a uniform pick.
        let u = (r % 10_000) as f64 / 10_000.0;
        let term = (u * u * 10_000.0) as u64;
        let mut o = Json::obj();
        o.push("k", Json::Str(format!("term-{term:05}")));
        o.push("n", Json::Int((r >> 20) as i64 % 1_000_000));
        o.push("t", Json::Int(1_600_000_000_000 + ((r >> 8) % 31_536_000_000) as i64));
        per[i % shards].push((i.to_string(), o));
    }
    let built = per.iter().map(|p| index.shard(p, 4)).collect();
    eprintln!("indexed {count} docs into {shards} shard(s) in {:?}", started.elapsed());
    (index, built)
}

fn time(index: &TestIndex, shards: &[search_index::Shard], label: &str, aggs: &str) {
    let searchers: Vec<search_index::Searcher> = shards.iter().map(|s| s.searcher()).collect();
    let body = format!(r#"{{"aggs":{aggs}}}"#);
    // One warm-up run, then the timed ones.
    run(index, &searchers, &body, false).unwrap();
    let runs = 3;
    let started = Instant::now();
    for _ in 0..runs {
        run(index, &searchers, &body, false).unwrap();
    }
    eprintln!("{label}: {:?} per run", started.elapsed() / runs);
}

#[test]
#[ignore]
fn bench_terms_date_histogram_cardinality() {
    let (index, shards) = build();
    time(&index, &shards, "terms (keyword, size 10)", r#"{"t":{"terms":{"field":"k"}}}"#);
    time(&index, &shards, "terms (keyword, size 1000)", r#"{"t":{"terms":{"field":"k","size":1000}}}"#);
    time(&index, &shards, "terms (long)", r#"{"t":{"terms":{"field":"n"}}}"#);
    time(&index, &shards, "terms + avg", r#"{"t":{"terms":{"field":"k"},"aggs":{"a":{"avg":{"field":"n"}}}}}"#);
    time(&index, &shards, "date_histogram (day)", r#"{"h":{"date_histogram":{"field":"t","calendar_interval":"day"}}}"#);
    time(&index, &shards, "date_histogram (hour) + max", r#"{"h":{"date_histogram":{"field":"t","fixed_interval":"1h"},"aggs":{"m":{"max":{"field":"n"}}}}}"#);
    time(&index, &shards, "global: collection pipeline only", r#"{"g":{"global":{}}}"#);
    time(&index, &shards, "max (date): doc values baseline", r#"{"m":{"max":{"field":"t"}}}"#);
    time(&index, &shards, "histogram (long, 1000 buckets)", r#"{"h":{"histogram":{"field":"n","interval":1000}}}"#);
    time(&index, &shards, "stats (long)", r#"{"s":{"stats":{"field":"n"}}}"#);
    time(&index, &shards, "percentiles (long)", r#"{"p":{"percentiles":{"field":"n"}}}"#);
    time(&index, &shards, "range (long, 5 ranges)", r#"{"r":{"range":{"field":"n","ranges":[{"to":1000},{"from":1000,"to":10000},{"from":10000,"to":100000},{"from":100000,"to":500000},{"from":500000}]}}}"#);
    time(&index, &shards, "composite (keyword, day), 1000/page", r#"{"c":{"composite":{"size":1000,"sources":[{"k":{"terms":{"field":"k"}}},{"d":{"date_histogram":{"field":"t","calendar_interval":"day"}}}]}}}"#);
    time(&index, &shards, "terms (keyword) > date_histogram (month)", r#"{"t":{"terms":{"field":"k"},"aggs":{"d":{"date_histogram":{"field":"t","calendar_interval":"month"}}}}}"#);
    time(&index, &shards, "cardinality (keyword)", r#"{"c":{"cardinality":{"field":"k"}}}"#);
    time(&index, &shards, "cardinality (long)", r#"{"c":{"cardinality":{"field":"n"}}}"#);
    time(&index, &shards, "cardinality (long, 100)", r#"{"c":{"cardinality":{"field":"n","precision_threshold":100}}}"#);
}
