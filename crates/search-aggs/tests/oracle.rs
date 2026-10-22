//! Randomized oracle tests: aggregations over random documents spread over 1–3 shards of 1–4
//! segments each, checked against a brute-force computation over the same documents, and
//! against each other (the shard and segment layout must not change an exact aggregation).

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::{TestIndex, run};
use search_aggs::Json;

/// xorshift64*: deterministic test data without a dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

const DAY: i64 = 86_400_000;
const T0: i64 = 1_600_000_000_000;

#[derive(Clone, Debug)]
struct Doc {
    k: Vec<String>,
    n: Vec<i64>,
    d: Option<f64>,
    t: Option<i64>,
}

impl Doc {
    fn to_json(&self) -> Json {
        let mut o = Json::obj();
        if !self.k.is_empty() {
            o.push("k", Json::Arr(self.k.iter().map(|s| Json::Str(s.clone())).collect()));
        }
        if !self.n.is_empty() {
            o.push("n", Json::Arr(self.n.iter().map(|&v| Json::Int(v)).collect()));
        }
        if let Some(d) = self.d {
            o.push("d", Json::Float(d));
        }
        if let Some(t) = self.t {
            o.push("t", Json::Int(t));
        }
        o
    }
}

fn docs(seed: u64, count: usize) -> Vec<Doc> {
    let mut r = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    (0..count)
        .map(|_| {
            let k = (0..r.below(3)).map(|_| format!("k{:02}", r.below(30))).collect();
            let n = (0..r.below(3)).map(|_| r.below(200) as i64 - 50).collect();
            let d = r.chance(85).then(|| (r.below(100_000) as f64) / 100.0 - 300.0);
            let t = r.chance(90).then(|| T0 + (r.below(60 * DAY as u64)) as i64);
            Doc { k, n, d, t }
        })
        .collect()
}

fn index() -> TestIndex {
    let mapping =
        Json::parse(r#"{"properties":{"k":{"type":"keyword"},"n":{"type":"long"},"d":{"type":"double"},"t":{"type":"date"}}}"#).unwrap();
    TestIndex::from_mapping(&mapping)
}

/// Runs `aggs` over `docs` dealt round-robin to `shards` shards of `segments` segments.
fn run_layout(docs: &[Doc], shards: usize, segments: usize, aggs: &str) -> Json {
    let index = index();
    let mut per: Vec<Vec<(String, Json)>> = vec![Vec::new(); shards];
    for (i, d) in docs.iter().enumerate() {
        per[i % shards].push((i.to_string(), d.to_json()));
    }
    let built: Vec<search_index::Shard> = per.iter().map(|p| index.shard(p, segments)).collect();
    let searchers: Vec<search_index::Searcher> = built.iter().map(|s| s.searcher()).collect();
    run(&index, &searchers, &format!(r#"{{"aggs":{aggs}}}"#), false).unwrap()
}

const LAYOUTS: [(usize, usize); 4] = [(1, 1), (1, 4), (3, 1), (3, 3)];

fn buckets(r: &Json, name: &str) -> Vec<Json> {
    r.get(name).and_then(|a| a.get("buckets")).and_then(Json::as_array).map(|a| a.to_vec()).unwrap_or_default()
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

#[test]
fn terms_histograms_and_metrics_match_brute_force() {
    let aggs = r#"{
        "k": {"terms": {"field": "k", "size": 100}, "aggs": {"s": {"sum": {"field": "d"}}, "c": {"value_count": {"field": "n"}}}},
        "n": {"terms": {"field": "n", "size": 1000, "order": {"_key": "asc"}}},
        "h": {"histogram": {"field": "n", "interval": 7}},
        "dh": {"date_histogram": {"field": "t", "calendar_interval": "week"}},
        "fh": {"date_histogram": {"field": "t", "fixed_interval": "6h", "min_doc_count": 1}},
        "r": {"range": {"field": "d", "ranges": [{"to": -100}, {"from": -100, "to": 250.5}, {"from": 250.5}]}},
        "st": {"stats": {"field": "d"}},
        "vc": {"value_count": {"field": "k"}},
        "card": {"cardinality": {"field": "k"}},
        "cn": {"cardinality": {"field": "n"}},
        "f": {"filters": {"filters": {"a": {"term": {"k": "k03"}}, "b": {"range": {"n": {"gte": 100}}}}}}
    }"#;
    for seed in 1..=4u64 {
        let docs = docs(seed, 300 + 100 * seed as usize);
        let mut first: Option<Json> = None;
        for (shards, segments) in LAYOUTS {
            let r = run_layout(&docs, shards, segments, aggs);
            check_brute_force(&docs, &r, seed, shards, segments);
            // Exact aggregations don't depend on the layout (sums only up to rounding).
            match &first {
                None => first = Some(r),
                Some(f) => {
                    for name in ["n", "h", "dh", "fh", "r", "f", "vc", "card", "cn"] {
                        assert_eq!(f.get(name), r.get(name), "seed {seed} layout {shards}x{segments} agg {name}");
                    }
                }
            }
        }
    }
}

fn check_brute_force(docs: &[Doc], r: &Json, seed: u64, shards: usize, segments: usize) {
    let ctx = format!("seed {seed} layout {shards}x{segments}");
    // terms on k: docs per term, sum of d and count of n values of those docs.
    let mut k: BTreeMap<&str, (u64, f64, u64)> = BTreeMap::new();
    for d in docs {
        let distinct: BTreeSet<&str> = d.k.iter().map(String::as_str).collect();
        for t in distinct {
            let e = k.entry(t).or_default();
            e.0 += 1;
            e.1 += d.d.unwrap_or(0.0);
            e.2 += d.n.len() as u64;
        }
    }
    let mut want: Vec<(&str, (u64, f64, u64))> = k.into_iter().collect();
    want.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(b.0)));
    let got = buckets(r, "k");
    assert_eq!(got.len(), want.len(), "{ctx}: terms k");
    for (g, (key, (count, sum, vc))) in got.iter().zip(&want) {
        assert_eq!(g.get("key").and_then(Json::as_str), Some(*key), "{ctx}: terms k order");
        assert_eq!(g.get("doc_count").and_then(Json::as_i64), Some(*count as i64), "{ctx}: {key}");
        assert!(close(g.get("s").unwrap().get("value").unwrap().as_f64().unwrap(), *sum), "{ctx}: sum under {key}");
        assert_eq!(g.get("c").unwrap().get("value").and_then(Json::as_i64), Some(*vc as i64), "{ctx}: count under {key}");
    }
    // terms on n, by key.
    let mut n: BTreeMap<i64, u64> = BTreeMap::new();
    for d in docs {
        for v in d.n.iter().copied().collect::<BTreeSet<_>>() {
            *n.entry(v).or_default() += 1;
        }
    }
    let got: Vec<(i64, i64)> = buckets(r, "n").iter().map(|b| (b.get("key").unwrap().as_i64().unwrap(), b.get("doc_count").unwrap().as_i64().unwrap())).collect();
    let want: Vec<(i64, i64)> = n.iter().map(|(&k, &c)| (k, c as i64)).collect();
    assert_eq!(got, want, "{ctx}: terms n");
    // histogram on n (interval 7): docs per bucket, empty buckets between.
    let mut h: BTreeMap<i64, u64> = BTreeMap::new();
    for d in docs {
        for b in d.n.iter().map(|&v| (v as f64 / 7.0).floor() as i64).collect::<BTreeSet<_>>() {
            *h.entry(b).or_default() += 1;
        }
    }
    let got: Vec<(f64, i64)> = buckets(r, "h").iter().map(|b| (b.get("key").unwrap().as_f64().unwrap(), b.get("doc_count").unwrap().as_i64().unwrap())).collect();
    if let (Some((&lo, _)), Some((&hi, _))) = (h.first_key_value(), h.last_key_value()) {
        let want: Vec<(f64, i64)> = (lo..=hi).map(|b| ((b * 7) as f64, h.get(&b).copied().unwrap_or(0) as i64)).collect();
        assert_eq!(got, want, "{ctx}: histogram");
    }
    // date_histogram by fixed 6h with min_doc_count 1.
    let mut fh: BTreeMap<i64, u64> = BTreeMap::new();
    for d in docs {
        if let Some(t) = d.t {
            *fh.entry(t.div_euclid(6 * 3_600_000) * 6 * 3_600_000).or_default() += 1;
        }
    }
    let got: Vec<(i64, i64)> = buckets(r, "fh").iter().map(|b| (b.get("key").unwrap().as_i64().unwrap(), b.get("doc_count").unwrap().as_i64().unwrap())).collect();
    assert_eq!(got, fh.iter().map(|(&k, &c)| (k, c as i64)).collect::<Vec<_>>(), "{ctx}: fixed date histogram");
    // date_histogram by calendar week (Monday): total count and bucket spacing.
    let dh = buckets(r, "dh");
    let total: i64 = dh.iter().map(|b| b.get("doc_count").unwrap().as_i64().unwrap()).sum();
    assert_eq!(total, docs.iter().filter(|d| d.t.is_some()).count() as i64, "{ctx}: weekly total");
    for w in dh.windows(2) {
        assert_eq!(w[1].get("key").unwrap().as_i64().unwrap() - w[0].get("key").unwrap().as_i64().unwrap(), 7 * DAY, "{ctx}: weekly keys");
    }
    // range on d.
    let ranges = [(f64::NEG_INFINITY, -100.0), (-100.0, 250.5), (250.5, f64::INFINITY)];
    let got: Vec<i64> = buckets(r, "r").iter().map(|b| b.get("doc_count").unwrap().as_i64().unwrap()).collect();
    let want: Vec<i64> = ranges.iter().map(|&(lo, hi)| docs.iter().filter(|d| d.d.is_some_and(|v| v >= lo && v < hi)).count() as i64).collect();
    assert_eq!(got, want, "{ctx}: range");
    // stats on d.
    let vals: Vec<f64> = docs.iter().filter_map(|d| d.d).collect();
    let st = r.get("st").unwrap();
    assert_eq!(st.get("count").and_then(Json::as_i64), Some(vals.len() as i64), "{ctx}: stats count");
    assert_eq!(st.get("min").and_then(Json::as_f64), vals.iter().copied().reduce(f64::min), "{ctx}: stats min");
    assert_eq!(st.get("max").and_then(Json::as_f64), vals.iter().copied().reduce(f64::max), "{ctx}: stats max");
    assert!(close(st.get("sum").unwrap().as_f64().unwrap(), vals.iter().sum()), "{ctx}: stats sum");
    // value_count (keyword doc values are a set per doc; numeric ones keep duplicates) and
    // cardinality (exact below the precision threshold).
    let vc: i64 = docs.iter().map(|d| d.k.iter().collect::<BTreeSet<_>>().len() as i64).sum();
    assert_eq!(r.get("vc").unwrap().get("value").and_then(Json::as_i64), Some(vc), "{ctx}: value_count");
    let distinct_k: BTreeSet<&String> = docs.iter().flat_map(|d| d.k.iter()).collect();
    assert_eq!(r.get("card").unwrap().get("value").and_then(Json::as_i64), Some(distinct_k.len() as i64), "{ctx}: cardinality k");
    let distinct_n: BTreeSet<i64> = docs.iter().flat_map(|d| d.n.iter().copied()).collect();
    assert_eq!(r.get("cn").unwrap().get("value").and_then(Json::as_i64), Some(distinct_n.len() as i64), "{ctx}: cardinality n");
    // filters.
    let f = r.get("f").unwrap().get("buckets").unwrap();
    let a = docs.iter().filter(|d| d.k.iter().any(|k| k == "k03")).count() as i64;
    let b = docs.iter().filter(|d| d.n.iter().any(|&v| v >= 100)).count() as i64;
    assert_eq!(f.get("a").unwrap().get("doc_count").and_then(Json::as_i64), Some(a), "{ctx}: filter a");
    assert_eq!(f.get("b").unwrap().get("doc_count").and_then(Json::as_i64), Some(b), "{ctx}: filter b");
}

#[test]
fn composite_pages_cover_every_pair_once() {
    for seed in 5..=7u64 {
        let docs = docs(seed, 400);
        let mut want: BTreeMap<(String, i64), i64> = BTreeMap::new();
        for d in docs.iter() {
            let ks: BTreeSet<&String> = d.k.iter().collect();
            let ns: BTreeSet<i64> = d.n.iter().copied().collect();
            for k in &ks {
                for &n in &ns {
                    *want.entry(((*k).clone(), n)).or_default() += 1;
                }
            }
        }
        for (shards, segments) in LAYOUTS {
            let mut got: BTreeMap<(String, i64), i64> = BTreeMap::new();
            let mut after = String::new();
            let mut pages = 0;
            loop {
                let aggs = format!(r#"{{"c":{{"composite":{{"size":37,"sources":[{{"k":{{"terms":{{"field":"k"}}}}}},{{"n":{{"terms":{{"field":"n","order":"desc"}}}}}}]{after}}}}}}}"#);
                let r = run_layout(&docs, shards, segments, &aggs);
                let page = buckets(&r, "c");
                if page.is_empty() {
                    break;
                }
                pages += 1;
                for b in &page {
                    let key = b.get("key").unwrap();
                    let k = (key.get("k").unwrap().as_str().unwrap().to_string(), key.get("n").unwrap().as_i64().unwrap());
                    assert!(got.insert(k, b.get("doc_count").unwrap().as_i64().unwrap()).is_none(), "seed {seed}: a pair came twice");
                }
                let ak = r.get("c").unwrap().get("after_key").unwrap();
                after = format!(r#","after":{}"#, ak.to_json_string());
            }
            assert!(pages > 1, "seed {seed}: paged");
            assert_eq!(got, want, "seed {seed} layout {shards}x{segments}");
        }
    }
}

#[test]
fn truncated_terms_bound_their_errors() {
    for seed in 8..=12u64 {
        let docs = docs(seed, 600);
        let mut truth: BTreeMap<String, i64> = BTreeMap::new();
        for d in &docs {
            for k in d.k.iter().collect::<BTreeSet<_>>() {
                *truth.entry(k.clone()).or_default() += 1;
            }
        }
        let total: i64 = truth.values().sum();
        for (shards, segments) in [(3, 1), (3, 2)] {
            let r = run_layout(&docs, shards, segments, r#"{"t":{"terms":{"field":"k","size":5,"shard_size":6,"show_term_doc_count_error":true}}}"#);
            let t = r.get("t").unwrap();
            let other = t.get("sum_other_doc_count").unwrap().as_i64().unwrap();
            let error = t.get("doc_count_error_upper_bound").unwrap().as_i64().unwrap();
            let got = buckets(&r, "t");
            assert_eq!(got.len(), 5);
            let mut returned = 0;
            for b in &got {
                let key = b.get("key").unwrap().as_str().unwrap();
                let count = b.get("doc_count").unwrap().as_i64().unwrap();
                let bucket_error = b.get("doc_count_error_upper_bound").unwrap().as_i64().unwrap();
                let real = truth[key];
                assert!(count <= real && real <= count + bucket_error, "seed {seed}: {key} {count}+{bucket_error} vs {real}");
                returned += count;
            }
            assert_eq!(returned + other, total, "seed {seed}: counts add up");
            // No term left out has more docs than the last returned bucket plus the error.
            let last = got.last().unwrap().get("doc_count").unwrap().as_i64().unwrap();
            let shown: BTreeSet<&str> = got.iter().map(|b| b.get("key").unwrap().as_str().unwrap()).collect();
            for (k, &c) in &truth {
                if !shown.contains(k.as_str()) {
                    assert!(c <= last + error, "seed {seed}: {k} ({c}) beyond the error bound");
                }
            }
        }
    }
}

/// Shard results reduced in batches (partial reduces, then a final one over the partial
/// results) must equal one final reduce over every shard result.
#[test]
fn batched_partial_reduces_match_a_single_reduce() {
    let aggs = r#"{
        "k": {"terms": {"field": "k", "size": 100}, "aggs": {"h": {"histogram": {"field": "n", "interval": 50}}, "m": {"max": {"field": "d"}}}},
        "lt": {"terms": {"field": "n", "size": 3, "shard_size": 200}},
        "dh": {"date_histogram": {"field": "t", "calendar_interval": "day", "min_doc_count": 0}},
        "adh": {"auto_date_histogram": {"field": "t", "buckets": 7}},
        "card": {"cardinality": {"field": "n"}},
        "st": {"stats": {"field": "n"}},
        "c": {"composite": {"size": 20, "sources": [{"k": {"terms": {"field": "k"}}}]}},
        "rt": {"rare_terms": {"field": "k", "max_doc_count": 20}},
        "r": {"range": {"field": "d", "ranges": [{"to": 0}, {"from": 0}]}, "aggs": {"vc": {"value_count": {"field": "k"}}}},
        "mx": {"max_bucket": {"buckets_path": "dh>_count"}}
    }"#;
    let index = index();
    let docs = docs(21, 900);
    let shards = 5;
    let mut per: Vec<Vec<(String, Json)>> = vec![Vec::new(); shards];
    for (i, d) in docs.iter().enumerate() {
        per[i % shards].push((i.to_string(), d.to_json()));
    }
    let built: Vec<search_index::Shard> = per.iter().map(|p| index.shard(p, 2)).collect();
    let body = format!(r#"{{"aggs":{aggs}}}"#);
    let req = search_aggs::parse(&Json::parse(aggs).unwrap(), &search_aggs::ParseOptions::default()).unwrap();
    let lookup = common::Lookup { index: &index, now: 1_700_000_000_000, scripts: None };
    let query = common::top_query(&lookup, &Json::parse(&body).unwrap());
    let parts: Vec<search_aggs::Aggs> =
        built.iter().map(|s| req.run(&lookup, &s.searcher(), &query, &search_aggs::ShardOptions::default()).unwrap()).collect();
    let final_opts = search_aggs::ReduceOptions::default();
    let partial_opts = search_aggs::ReduceOptions { final_reduce: false, ..search_aggs::ReduceOptions::default() };
    let direct = search_aggs::reduce(parts.clone(), &req, &final_opts).unwrap().to_json(true);
    for batch in [2, 3] {
        let mut partials = Vec::new();
        for chunk in parts.chunks(batch) {
            partials.push(search_aggs::reduce(chunk.to_vec(), &req, &partial_opts).unwrap());
        }
        let batched = search_aggs::reduce(partials, &req, &ReduceOptionsBatched::with(&final_opts)).unwrap().to_json(true);
        for name in ["sterms#k", "lterms#lt", "date_histogram#dh", "auto_date_histogram#adh", "cardinality#card", "composite#c", "srareterms#rt", "range#r", "bucket_metric_value#mx"] {
            assert!(direct.get(name).is_some(), "{name} missing");
            assert_eq!(direct.get(name), batched.get(name), "batches of {batch}: {name}");
        }
        let (a, b) = (direct.get("stats#st").unwrap(), batched.get("stats#st").unwrap());
        for f in ["count", "min", "max", "sum"] {
            assert_eq!(a.get(f), b.get(f), "batches of {batch}: stats {f}");
        }
    }
}

/// The final reduce after batched partial reduces (`has_batched_result`).
struct ReduceOptionsBatched;

impl ReduceOptionsBatched {
    fn with(o: &search_aggs::ReduceOptions) -> search_aggs::ReduceOptions {
        search_aggs::ReduceOptions { has_batched_result: true, ..o.clone() }
    }
}

/// The request breaker: a small budget trips on a high-cardinality aggregation, everything is
/// given back afterwards, and `search.max_buckets` caps the final reduce.
#[test]
fn memory_budget_and_max_buckets() {
    let index = index();
    let docs: Vec<(String, Json)> = (0..20_000).map(|i| (i.to_string(), Json::parse(&format!(r#"{{"n":[{i}],"k":["v{i}"]}}"#)).unwrap())).collect();
    let shard = index.shard(&docs, 2);
    let searcher = shard.searcher();
    let lookup = common::Lookup { index: &index, now: 0, scripts: None };
    let query = common::top_query(&lookup, &Json::obj());
    let parse = |a: &str| search_aggs::parse(&Json::parse(a).unwrap(), &search_aggs::ParseOptions::default()).unwrap();
    let budget = search_aggs::MemoryBudget::new(64 * 1024);
    let opts = search_aggs::ShardOptions { budget: Some(budget.clone()), ..Default::default() };
    for aggs in [r#"{"t":{"terms":{"field":"n","size":10}}}"#, r#"{"t":{"terms":{"field":"k"}}}"#, r#"{"h":{"histogram":{"field":"n","interval":1}}}"#] {
        let e = parse(aggs).run(&lookup, &searcher, &query, &opts).unwrap_err();
        assert_eq!(e.kind, "circuit_breaking_exception", "{aggs}");
        assert_eq!(budget.used(), 0, "{aggs}: reservations released");
    }
    let big = search_aggs::MemoryBudget::new(512 << 20);
    let opts = search_aggs::ShardOptions { budget: Some(big.clone()), ..Default::default() };
    let req = parse(r#"{"h":{"histogram":{"field":"n","interval":1}}}"#);
    let part = req.run(&lookup, &searcher, &query, &opts).unwrap();
    assert_eq!(big.used(), 0);
    let limited = search_aggs::ReduceOptions { max_buckets: 10_000, ..search_aggs::ReduceOptions::default() };
    let e = search_aggs::reduce(vec![part.clone()], &req, &limited).unwrap_err();
    assert_eq!(e.kind, "too_many_buckets_exception");
    let r = search_aggs::reduce(vec![part], &req, &search_aggs::ReduceOptions::default()).unwrap();
    assert_eq!(r.to_json(false).get("h").unwrap().get("buckets").unwrap().as_array().unwrap().len(), 20_000);
}

/// One doc's `long_range` and `date_range` values (inclusive bounds).
type RangeDoc = (Vec<(i64, i64)>, Vec<(i64, i64)>);

/// Range fields: a doc lands once in every histogram bucket any of its ranges overlaps (after
/// `hard_bounds` clamp the ranges), whatever the layout.
#[test]
fn range_histograms_match_brute_force() {
    let mapping = Json::parse(r#"{"properties":{"r":{"type":"long_range"},"t":{"type":"date_range"}}}"#).unwrap();
    let index = TestIndex::from_mapping(&mapping);
    let aggs = r#"{
        "h": {"histogram": {"field": "r", "interval": 6, "offset": 2}},
        "hb": {"histogram": {"field": "r", "interval": 10, "hard_bounds": {"min": -15, "max": 95}}},
        "dh": {"date_histogram": {"field": "t", "fixed_interval": "2d"}},
        "vc": {"value_count": {"field": "r"}},
        "m": {"missing": {"field": "t"}},
        "c": {"cardinality": {"field": "r"}}
    }"#;
    for seed in 1..=4u64 {
        let mut rng = Rng(seed.wrapping_mul(0x2545_f491) | 1);
        // Per doc: long ranges (inclusive bounds) and date ranges.
        let mut docs: Vec<RangeDoc> = Vec::new();
        for _ in 0..250 + 50 * seed as usize {
            let longs = (0..rng.below(4))
                .map(|_| {
                    let a = rng.below(200) as i64 - 60;
                    (a, a + rng.below(40) as i64)
                })
                .collect();
            let dates = (0..rng.below(3))
                .map(|_| {
                    let a = T0 + rng.below(40 * DAY as u64) as i64;
                    (a, a + rng.below(9 * DAY as u64) as i64)
                })
                .collect();
            docs.push((longs, dates));
        }
        let json = |(l, t): &RangeDoc| {
            let mut o = Json::obj();
            let ranges = |v: &[(i64, i64)]| Json::Arr(v.iter().map(|&(a, b)| Json::parse(&format!(r#"{{"gte":{a},"lte":{b}}}"#)).unwrap()).collect());
            if !l.is_empty() {
                o.push("r", ranges(l));
            }
            if !t.is_empty() {
                o.push("t", ranges(t));
            }
            o
        };
        let union = |ranges: &[(i64, i64)], key: &dyn Fn(i64) -> i64| -> BTreeSet<i64> { ranges.iter().flat_map(|&(a, b)| key(a)..=key(b)).collect() };
        let count = |keys: &mut BTreeMap<i64, i64>, set: BTreeSet<i64>| {
            for k in set {
                *keys.entry(k).or_default() += 1;
            }
        };
        let (mut h, mut hb, mut dh) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
        let mut distinct: BTreeSet<Vec<(i64, i64)>> = BTreeSet::new();
        for (l, t) in &docs {
            count(&mut h, union(l, &|v| (v - 2).div_euclid(6)));
            let clamped: Vec<(i64, i64)> = l.iter().map(|&(a, b)| (a.max(-15), b.min(95))).collect();
            count(&mut hb, union(&clamped, &|v| v.div_euclid(10)));
            count(&mut dh, union(t, &|v| v.div_euclid(2 * DAY)));
            if !l.is_empty() {
                let mut s: Vec<(i64, i64)> = l.clone();
                s.sort();
                s.dedup();
                distinct.insert(s);
            }
        }
        let dense = |m: &BTreeMap<i64, i64>, scale: i64, off: i64| -> Vec<(i64, i64)> {
            let (Some((&lo, _)), Some((&hi, _))) = (m.first_key_value(), m.last_key_value()) else { return Vec::new() };
            (lo..=hi).map(|k| (k * scale + off, m.get(&k).copied().unwrap_or(0))).collect()
        };
        for (shards, segments) in LAYOUTS {
            let mut per: Vec<Vec<(String, Json)>> = vec![Vec::new(); shards];
            for (i, d) in docs.iter().enumerate() {
                per[i % shards].push((i.to_string(), json(d)));
            }
            let built: Vec<search_index::Shard> = per.iter().map(|p| index.shard(p, segments)).collect();
            let searchers: Vec<search_index::Searcher> = built.iter().map(|s| s.searcher()).collect();
            let r = run(&index, &searchers, &format!(r#"{{"aggs":{aggs}}}"#), false).unwrap();
            let ctx = format!("seed {seed} layout {shards}x{segments}");
            let got = |name: &str| -> Vec<(i64, i64)> {
                buckets(&r, name).iter().map(|b| (b.get("key").unwrap().as_f64().unwrap() as i64, b.get("doc_count").unwrap().as_i64().unwrap())).collect()
            };
            assert_eq!(got("h"), dense(&h, 6, 2), "{ctx}: histogram");
            assert_eq!(got("hb"), dense(&hb, 10, 0), "{ctx}: hard bounds");
            assert_eq!(got("dh"), dense(&dh, 2 * DAY, 0), "{ctx}: date histogram");
            let value = |name: &str, field: &str| r.get(name).unwrap().get(field).unwrap().as_i64().unwrap();
            assert_eq!(value("vc", "value"), docs.iter().filter(|d| !d.0.is_empty()).count() as i64, "{ctx}: value_count");
            assert_eq!(value("m", "doc_count"), docs.iter().filter(|d| d.1.is_empty()).count() as i64, "{ctx}: missing");
            assert_eq!(value("c", "value"), distinct.len() as i64, "{ctx}: cardinality");
        }
    }
}

/// An unbounded range can't enumerate its buckets: Elasticsearch adds buckets until its
/// circuit breaker trips (or, from `-Infinity`, never finishes); roll refuses up front.
#[test]
fn unbounded_ranges_break_instead_of_hanging() {
    let mapping = Json::parse(r#"{"properties":{"r":{"type":"long_range"},"d":{"type":"double_range"},"t":{"type":"date_range"}}}"#).unwrap();
    let index = TestIndex::from_mapping(&mapping);
    let docs = vec![("0".to_string(), Json::parse(r#"{"r":{"gte":5},"d":{"lte":3.5},"t":{"gte":0}}"#).unwrap())];
    let shard = index.shard(&docs, 1);
    let searchers = vec![shard.searcher()];
    for aggs in [
        r#"{"h":{"histogram":{"field":"r","interval":1}}}"#,
        r#"{"h":{"histogram":{"field":"d","interval":1}}}"#,
        r#"{"h":{"date_histogram":{"field":"t","fixed_interval":"1ms"}}}"#,
    ] {
        let e = run(&index, &searchers, &format!(r#"{{"aggs":{aggs}}}"#), false).unwrap_err();
        assert_eq!(e.kind, "circuit_breaking_exception", "{aggs}");
        assert!(e.reason.contains("<agg [h]>"), "{}", e.reason);
    }
    // Bounded by hard_bounds, the same ranges are fine.
    let r = run(&index, &searchers, r#"{"aggs":{"h":{"histogram":{"field":"d","interval":1,"hard_bounds":{"min":-2,"max":10}}}}}"#, false).unwrap();
    assert_eq!(buckets(&r, "h").len(), 6);
}

/// `time_series` and counter `rate` with a stored `_tsid`: documents indexed in any order and
/// spread over segments give what documents indexed in `_tsid` order (newest first) give —
/// Elasticsearch's visiting order is rebuilt from the `_tsid` and `@timestamp` values.
#[test]
fn time_series_order_comes_from_tsid() {
    let mut rng = Rng(77);
    // Series: (host, tsid); samples: (series, timestamp, counter).
    let series: Vec<(String, String)> = (0..12).map(|i| (format!("h{i:02}"), format!("{:08x}", rng.next() as u32))).collect();
    let mut samples = Vec::new();
    for s in 0..series.len() {
        let mut counter = rng.below(1000) as i64;
        let mut t = T0;
        for _ in 0..1 + rng.below(30) {
            t += 1000 * (1 + rng.below(600) as i64);
            counter = if rng.chance(8) { rng.below(40) as i64 } else { counter + rng.below(500) as i64 };
            samples.push((s, t, counter));
        }
    }
    let doc = |&(s, t, c): &(usize, i64, i64), with_tsid: bool| {
        let mut o = Json::obj().with("host", Json::Str(series[s].0.clone())).with("@timestamp", Json::Int(t)).with("bytes", Json::Int(c));
        if with_tsid {
            o.push("_tsid", Json::Str(series[s].1.clone()));
        }
        o
    };
    let aggs = r#"{"ts":{"time_series":{"keyed":false,"size":7},"aggs":{"r":{"rate":{"field":"bytes"}},"d":{"date_histogram":{"field":"@timestamp","fixed_interval":"5m"},"aggs":{"r":{"rate":{"field":"bytes","unit":"minute"}}}}}}}"#;
    let body = format!(r#"{{"aggs":{aggs}}}"#);
    // Reference: `_tsid` order, newest first, one segment, no stored `_tsid`.
    let mapping = |tsid: bool| {
        let extra = if tsid { r#","_tsid":{"type":"keyword"}"# } else { "" };
        Json::parse(&format!(
            r#"{{"properties":{{"host":{{"type":"keyword","time_series_dimension":true}},"@timestamp":{{"type":"date"}},"bytes":{{"type":"long","time_series_metric":"counter"}}{extra}}}}}"#
        ))
        .unwrap()
    };
    let mut ordered = samples.clone();
    ordered.sort_by(|a, b| series[a.0].1.cmp(&series[b.0].1).then(b.1.cmp(&a.1)));
    let ref_index = TestIndex::from_mapping(&mapping(false));
    let ref_docs: Vec<(String, Json)> = ordered.iter().enumerate().map(|(i, s)| (i.to_string(), doc(s, false))).collect();
    let ref_shard = ref_index.shard(&ref_docs, 1);
    let want = run(&ref_index, &[ref_shard.searcher()], &body, false).unwrap();
    assert_eq!(want.get("ts").unwrap().get("buckets").unwrap().as_array().unwrap().len(), 7);
    // Shuffled docs over several segments, with a stored `_tsid`.
    let index = TestIndex::from_mapping(&mapping(true));
    for seed in 1..=3u64 {
        let mut shuffled = samples.clone();
        let mut r = Rng(seed * 31 + 7);
        for i in (1..shuffled.len()).rev() {
            shuffled.swap(i, r.below(i as u64 + 1) as usize);
        }
        let docs: Vec<(String, Json)> = shuffled.iter().enumerate().map(|(i, s)| (i.to_string(), doc(s, true))).collect();
        let shard = index.shard(&docs, 1 + seed as usize);
        let got = run(&index, &[shard.searcher()], &body, false).unwrap();
        // Rendered: empty buckets' rates are NaN, which never equals itself.
        assert_eq!(got.to_json_string(), want.to_json_string(), "seed {seed}");
    }
}

/// `geo_centroid`, `geotile_grid`, `geohash_grid`, `geohex_grid` and `geo_bounds` over
/// `geo_shape` values:
/// per doc, its cells (the tilers on the doc's own shape value) and its stored centroid; the
/// aggregations over 1–3 shards of 1–4 segments must count each cell once per doc and combine
/// centroids by dimension, per shard first, as Elasticsearch's reduce does.
#[test]
fn shapes_match_brute_force() {
    use search_aggs::bucket::shape_grid::{HashTiler, TileTiler};
    use search_aggs::shape_field::{CentroidCalculator, Coords, Geometry, ShapeValue, encode_centroid, shape_triangles};
    let mapping = Json::parse(r#"{"properties":{"s":{"type":"geo_shape"},"k":{"type":"keyword"}}}"#).unwrap();
    let index = TestIndex::from_mapping(&mapping);
    let aggs = r#"{
        "c": {"geo_centroid": {"field": "s"}},
        "t": {"geotile_grid": {"field": "s", "precision": 5, "size": 100000}},
        "h": {"geohash_grid": {"field": "s", "precision": 2, "size": 100000}},
        "x": {"geohex_grid": {"field": "s", "precision": 1, "size": 100000}},
        "b": {"geo_bounds": {"field": "s"}}
    }"#;
    for seed in 1..=3u64 {
        let mut rng = Rng(seed.wrapping_mul(0x51_7cc1_b727) | 1);
        let mut f = |lo: f64, hi: f64| lo + (rng.below(1_000_000) as f64 / 1_000_000.0) * (hi - lo);
        let mut docs: Vec<Option<Geometry>> = Vec::new();
        for i in 0..160 {
            let (cx, cy, r) = (f(-150.0, 150.0), f(-70.0, 70.0), f(0.05, 6.0));
            docs.push(match i % 5 {
                0 => None,
                1 => Some(Geometry::Point(cx, cy)),
                2 => Some(Geometry::Line(vec![(cx - r, cy), (cx, cy + r / 2.0), (cx + r, cy - r / 3.0)])),
                _ => Some(Geometry::Polygon(vec![(cx - r, cy - r), (cx + r, cy - r), (cx + r * 0.8, cy + r), (cx - r * 0.7, cy + r * 0.9), (cx - r, cy - r)], vec![])),
            });
        }
        let json = |g: &Option<Geometry>| -> Json {
            let mut o = Json::obj();
            let p = |&(x, y): &(f64, f64)| Json::Arr(vec![Json::Float(x), Json::Float(y)]);
            match g {
                Some(Geometry::Point(x, y)) => o.push("s", Json::parse(&format!(r#"{{"type":"Point","coordinates":[{x},{y}]}}"#)).unwrap()),
                Some(Geometry::Line(pts)) => o.push("s", Json::obj().with("type", "LineString").with("coordinates", Json::Arr(pts.iter().map(p).collect()))),
                Some(Geometry::Polygon(shell, _)) => {
                    o.push("s", Json::obj().with("type", "Polygon").with("coordinates", Json::Arr(vec![Json::Arr(shell.iter().map(p).collect())])))
                }
                _ => {}
            }
            o.push("k", Json::Str("x".into()));
            o
        };
        // Each doc's value as the aggregations read it (through the JSON round trip).
        let values: Vec<Option<ShapeValue>> = docs
            .iter()
            .map(|g| {
                let g = g.as_ref()?;
                let src = json(&Some(g.clone()));
                let s = src.get("s").unwrap();
                let g = match s.get("type").and_then(Json::as_str).unwrap() {
                    "Point" => {
                        let c = s.get("coordinates").unwrap().as_array().unwrap();
                        Geometry::Point(c[0].as_f64().unwrap(), c[1].as_f64().unwrap())
                    }
                    _ => g.clone(),
                };
                let mut calc = CentroidCalculator::new();
                calc.add(&g).unwrap();
                let dv = search_index::query::shape::doc_value(&shape_triangles(&g, Coords::Geo).unwrap());
                ShapeValue::from_doc_value(&dv, Some(&encode_centroid(&calc, Coords::Geo).unwrap()), Coords::Geo).unwrap()
            })
            .collect();
        let mut tiles: BTreeMap<i64, i64> = BTreeMap::new();
        let mut hashes: BTreeMap<i64, i64> = BTreeMap::new();
        let mut hexes: BTreeMap<i64, i64> = BTreeMap::new();
        let mut cells = Vec::new();
        for v in values.iter().flatten() {
            TileTiler { precision: 5, bounds: None }.cells(v, &mut cells).unwrap();
            cells.sort_unstable();
            cells.dedup();
            for &c in &cells {
                *tiles.entry(c).or_default() += 1;
            }
            HashTiler { precision: 2, valid: None }.cells(v, &mut cells).unwrap();
            cells.sort_unstable();
            cells.dedup();
            for &c in &cells {
                *hashes.entry(c).or_default() += 1;
            }
            search_aggs::bucket::shape_hex::HexTiler { precision: 1, bounds: None }.cells(v, &mut cells).unwrap();
            cells.sort_unstable();
            cells.dedup();
            for &c in &cells {
                *hexes.entry(c).or_default() += 1;
            }
        }
        for (shards, segments) in LAYOUTS {
            let mut per: Vec<Vec<(String, Json)>> = vec![Vec::new(); shards];
            for (i, d) in docs.iter().enumerate() {
                per[i % shards].push((i.to_string(), json(d)));
            }
            let built: Vec<search_index::Shard> = per.iter().map(|p| index.shard(p, segments)).collect();
            let searchers: Vec<search_index::Searcher> = built.iter().map(|s| s.searcher()).collect();
            let r = run(&index, &searchers, &format!(r#"{{"aggs":{aggs}}}"#), false).unwrap();
            let ctx = format!("seed {seed} layout {shards}x{segments}");
            let cells_of = |name: &str, key: &dyn Fn(&str) -> i64| -> BTreeMap<i64, i64> {
                buckets(&r, name).iter().map(|b| (key(b.get("key").unwrap().as_str().unwrap()), b.get("doc_count").unwrap().as_i64().unwrap())).collect()
            };
            let tile_key = |s: &str| search_aggs::geo::geotile_parse(s).unwrap();
            let hash_key = |s: &str| search_aggs::geo::geohash_long_of(s.as_bytes());
            assert_eq!(cells_of("t", &tile_key), tiles, "{ctx}: geotile cells");
            assert_eq!(cells_of("h", &hash_key), hashes, "{ctx}: geohash cells");
            let hex_key = |s: &str| search_aggs::h3::string_to_h3(s).unwrap() as i64;
            assert_eq!(cells_of("x", &hex_key), hexes, "{ctx}: geohex cells");
            // Centroid: per shard the highest dimension's weighted sums, then the reduce.
            let mut parts = Vec::new();
            for s in 0..shards {
                let (mut lat, mut lon, mut w, mut t, mut n) = (0.0, 0.0, 0.0, 0u8, 0u64);
                for (i, v) in values.iter().enumerate() {
                    let Some(v) = v.as_ref().filter(|_| i % shards == s) else { continue };
                    n += 1;
                    let (x, y) = v.centroid_xy();
                    let vt = v.centroid.shape_type as u8;
                    if vt > t {
                        (lat, lon, w, t) = (v.centroid.weight * y, v.centroid.weight * x, v.centroid.weight, vt);
                    } else if vt == t {
                        lat += v.centroid.weight * y;
                        lon += v.centroid.weight * x;
                        w += v.centroid.weight;
                    }
                }
                if n > 0 {
                    parts.push((lat, lon, w, t, n));
                }
            }
            let top = parts.iter().map(|p| p.3).max().unwrap();
            let best: Vec<_> = parts.iter().filter(|p| p.3 == top).collect();
            let (lat, lon, w): (f64, f64, f64) = best.iter().fold((0.0, 0.0, 0.0), |a, p| (a.0 + p.0, a.1 + p.1, a.2 + p.2));
            let count: u64 = if shards == 1 { parts[0].4 } else { best.iter().map(|p| p.4).sum() };
            let c = r.get("c").unwrap();
            assert_eq!(c.get("count").unwrap().as_i64().unwrap() as u64, count, "{ctx}: centroid count");
            let loc = c.get("location").unwrap();
            assert!(close(loc.get("lat").unwrap().as_f64().unwrap(), lat / w), "{ctx}: centroid lat");
            assert!(close(loc.get("lon").unwrap().as_f64().unwrap(), lon / w), "{ctx}: centroid lon");
            let top_lat = values.iter().flatten().map(|v| v.bounding_box().top).fold(f64::NEG_INFINITY, f64::max);
            let got_top = r.get("b").unwrap().get("bounds").unwrap().get("top_left").unwrap().get("lat").unwrap().as_f64().unwrap();
            assert_eq!(got_top, top_lat, "{ctx}: bounds top");
        }
    }
}
