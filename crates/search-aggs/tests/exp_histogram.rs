//! `search_aggs::exp_histogram` and the `HistogramUnionState` port against Elasticsearch 9.5.5's
//! own exponential histogram library (`tests/fixtures/aggs-exp-histograms.json`, written by
//! `tests/fixtures/oracle/ExpHistograms.java`): index math at every scale, merges with small and
//! default bucket limits (downscaling, zero buckets collapsing buckets), quantiles, ranks, and
//! union states mixing a t-digest with exponential histograms.
//!
//! Elasticsearch computes bucket bounds with `Math.exp` / `Math.log`, which HotSpot replaces on
//! x86-64 with Intel LIBM stubs that are not correctly rounded (about 0.2% of `exp` results are
//! one ulp off; the platform's functions here are correctly rounded nearly always). Doubles must
//! therefore agree to within 2 ulps, and at most 2% of them may differ at all; indices, counts
//! and scales must be identical.

use search_aggs::Json;
use search_aggs::exp_histogram::{
    self, ExpHistogram, ExpHistogramValue, Merger, compute_index, lower_boundary, point_of_least_relative_error, upper_boundary,
};
use search_aggs::metrics::union_state::UnionState;

fn fixture() -> Json {
    let path = format!("{}/tests/fixtures/aggs-exp-histograms.json", env!("CARGO_MANIFEST_DIR"));
    Json::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn d(j: &Json) -> f64 {
    f64::from_bits(u64::from_str_radix(j.as_str().unwrap(), 16).unwrap())
}

fn i(j: &Json) -> i64 {
    j.as_i64().unwrap_or_else(|| j.as_f64().unwrap() as i64)
}

fn arr(j: &Json) -> &[Json] {
    j.as_array().unwrap()
}

fn pairs(j: &Json) -> Vec<(i64, u64)> {
    arr(j).iter().map(|p| (i(&arr(p)[0]), i(&arr(p)[1]) as u64)).collect()
}

static INEXACT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static COMPARED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Equal, or within 2 ulps (counted).
fn same(a: f64, b: f64) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    COMPARED.fetch_add(1, Relaxed);
    if a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan()) || (a == b) {
        return true;
    }
    if a.is_finite() && b.is_finite() && a.signum() == b.signum() && (a.to_bits() as i64 - b.to_bits() as i64).abs() <= 2 {
        INEXACT.fetch_add(1, Relaxed);
        return true;
    }
    false
}

fn assert_mostly_exact() {
    use std::sync::atomic::Ordering::Relaxed;
    let (n, k) = (COMPARED.load(Relaxed), INEXACT.load(Relaxed));
    assert!(k * 50 <= n, "{k} of {n} doubles differ by an ulp or two");
}


/// Compares a histogram with the oracle's rendering of one.
fn check(h: &ExpHistogram, want: &Json, what: &str) {
    let z = want.get("zero").unwrap();
    assert_eq!(h.scale as i64, i(want.get("scale").unwrap()), "{what}: scale");
    assert_eq!(h.negative, pairs(want.get("neg").unwrap()), "{what}: negative buckets");
    assert_eq!(h.positive, pairs(want.get("pos").unwrap()), "{what}: positive buckets");
    assert_eq!(h.zero.count as i64, i(z.get("count").unwrap()), "{what}: zero count");
    assert_eq!((h.zero.index(), h.zero.scale() as i64), (i(z.get("index").unwrap()), i(z.get("zscale").unwrap())), "{what}: zero threshold");
    for (name, got, k) in [("zero threshold", h.zero.zero_threshold(), z.get("t")), ("sum", h.sum, want.get("sum")), ("min", h.min, want.get("min")), ("max", h.max, want.get("max"))] {
        let w = d(k.unwrap());
        assert!(same(got, w), "{what}: {name} {got:e} vs {w:e}");
    }
}

/// An oracle input, through the doc-value encoding (estimates included).
fn input(spec: &Json) -> ExpHistogram {
    let opt = |k: &str| spec.get(k).filter(|v| !matches!(v, Json::Null)).map(d);
    let v = ExpHistogramValue {
        scale: i(spec.get("scale").unwrap()) as i32,
        zero_threshold: d(spec.get("t").unwrap()),
        zero_count: i(spec.get("zc").unwrap()) as u64,
        negative: pairs(spec.get("neg").unwrap()),
        positive: pairs(spec.get("pos").unwrap()),
        sum: opt("sum"),
        min: opt("min"),
        max: opt("max"),
    };
    let mut h = ExpHistogram::empty();
    exp_histogram::decode_into(&exp_histogram::encode(&v), &mut h).unwrap();
    check(&h, spec.get("built").unwrap(), "input");
    h
}

const QS: [f64; 12] = [0.0, 0.001, 0.01, 0.1, 0.25, 0.333, 0.5, 0.75, 0.9, 0.99, 0.999, 1.0];

#[test]
fn index_math_matches_elasticsearch_at_every_scale() {
    let fx = fixture();
    for row in arr(fx.get("index").unwrap()) {
        let r = arr(row);
        let (scale, v) = (i(&r[0]) as i32, d(&r[1]));
        assert_eq!(compute_index(v, scale), i(&r[2]), "computeIndex({v:e}, {scale})");
    }
    for row in arr(fx.get("bounds").unwrap()) {
        let r = arr(row);
        let (scale, idx) = (i(&r[0]) as i32, i(&r[1]));
        assert!(same(lower_boundary(idx, scale), d(&r[2])), "lower({idx}, {scale})");
        assert!(same(upper_boundary(idx, scale), d(&r[3])), "upper({idx}, {scale})");
        assert!(same(point_of_least_relative_error(idx, scale), d(&r[4])), "center({idx}, {scale})");
    }
    assert_mostly_exact();
}

#[test]
fn merges_quantiles_and_ranks_match_elasticsearch() {
    let fx = fixture();
    for (n, m) in arr(fx.get("merges").unwrap()).iter().enumerate() {
        let mut merger = Merger::new(i(m.get("limit").unwrap()) as usize);
        for spec in arr(m.get("inputs").unwrap()) {
            merger.add(&input(spec));
        }
        let res = merger.get();
        check(res, m.get("result").unwrap(), &format!("merge {n}"));
        for (q, want) in QS.iter().zip(arr(m.get("quantiles").unwrap())) {
            let got = exp_histogram::quantile(res, *q);
            assert!(same(got, d(want)), "merge {n} quantile {q}: {got} vs {}", d(want));
        }
        for r in arr(m.get("ranks").unwrap()) {
            let r = arr(r);
            let x = d(&r[0]);
            assert_eq!(exp_histogram::estimate_rank(res, x, false), i(&r[1]) as u64, "merge {n} rank({x})");
            assert_eq!(exp_histogram::estimate_rank(res, x, true), i(&r[2]) as u64, "merge {n} rank({x}, inclusive)");
        }
    }
    assert_mostly_exact();
}

#[test]
fn union_states_match_elasticsearch() {
    let fx = fixture();
    for (n, u) in arr(fx.get("unions").unwrap()).iter().enumerate() {
        let compression = u.get("compression").unwrap().as_f64().unwrap();
        let mut state = UnionState::new(compression, false);
        for v in arr(u.get("values").unwrap()) {
            state.add_value(d(v)).unwrap();
        }
        for spec in arr(u.get("exps").unwrap()) {
            state.add_exp(&input(spec));
        }
        state.compress();
        assert_eq!(state.size(), i(u.get("size").unwrap()) as u64, "union {n} size");
        assert!(same(state.min(), d(u.get("min").unwrap())), "union {n} min");
        assert!(same(state.max(), d(u.get("max").unwrap())), "union {n} max");
        for (q, want) in QS.iter().zip(arr(u.get("quantiles").unwrap())) {
            let got = state.quantile(*q);
            assert!(same(got, d(want)), "union {n} quantile {q}: {got} vs {}", d(want));
        }
        for c in arr(u.get("cdf").unwrap()) {
            let c = arr(c);
            let got = state.cdf(d(&c[0])).unwrap();
            assert!(same(got, d(&c[1])), "union {n} cdf({}): {got} vs {}", d(&c[0]), d(&c[1]));
        }
        let want: Vec<(f64, u64)> = arr(u.get("centroids").unwrap()).iter().map(|c| (d(&arr(c)[0]), i(&arr(c)[1]) as u64)).collect();
        let got = state.centroids();
        assert_eq!(got.len(), want.len(), "union {n} centroids");
        for (g, w) in got.iter().zip(&want) {
            assert!(g.1 == w.1 && same(g.0, w.0), "union {n} centroid {g:?} vs {w:?}");
        }
    }
    assert_mostly_exact();
}
