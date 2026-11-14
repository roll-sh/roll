//! t-digest and HdrHistogram results checked bit for bit against Elasticsearch's own
//! implementations (`elasticsearch-tdigest-9.5.5.jar`, `HdrHistogram-2.1.9.jar`), captured by
//! `tests/fixtures/oracle/Oracle.java` into `tests/fixtures/aggs-digest-oracle.json`.
//!
//! Each case regenerates its input with `java.util.Random(seed)`, builds the digest the way the
//! aggregations do (one shard, or three digests merged into a fresh one), and compares
//! quantiles, CDFs / percentile ranks and centroid counts.

use search_aggs::Json;
use search_aggs::metrics::hdr::DoubleHistogram;
use search_aggs::metrics::tdigest::TDigest;

struct JavaRandom(u64);

impl JavaRandom {
    fn new(seed: i64) -> JavaRandom {
        JavaRandom((seed as u64 ^ 0x5DEECE66D) & ((1 << 48) - 1))
    }
    fn next(&mut self, bits: u32) -> i32 {
        self.0 = (self.0.wrapping_mul(0x5DEECE66D).wrapping_add(0xB)) & ((1 << 48) - 1);
        (self.0 >> (48 - bits)) as i64 as i32
    }
    fn next_double(&mut self) -> f64 {
        ((((self.next(26) as i64) << 27) + self.next(27) as i64) as f64) * (1.0 / (1u64 << 53) as f64)
    }
    fn next_int(&mut self, bound: i32) -> i32 {
        let mut r = self.next(31);
        let m = bound - 1;
        if bound & m == 0 {
            return ((bound as i64 * r as i64) >> 31) as i32;
        }
        let mut u = r;
        loop {
            r = u % bound;
            if u - r + m >= 0 {
                return r;
            }
            u = self.next(31);
        }
    }
}

fn value(r: &mut JavaRandom, dist: i64) -> f64 {
    match dist {
        0 => r.next_double() * 1000.0,
        1 => (r.next_double() * 1000.0).floor() / 10.0,
        2 => {
            let u = r.next_double();
            u * u * u * 100000.0
        }
        3 => r.next_int(50) as f64,
        _ => r.next_double() * r.next_double() * 1e9 + 1e-3,
    }
}

fn bits(j: &Json) -> u64 {
    u64::from_str_radix(j.as_str().unwrap(), 16).unwrap()
}

fn load() -> Json {
    let path = format!("{}/tests/fixtures/aggs-digest-oracle.json", env!("CARGO_MANIFEST_DIR"));
    Json::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
}

const QS: [f64; 11] = [0.0, 0.001, 0.01, 0.05, 0.25, 0.5, 0.75, 0.95, 0.99, 0.999, 1.0];
const PS: [f64; 10] = [0.0, 1.0, 5.0, 25.0, 50.0, 75.0, 95.0, 99.0, 99.9, 100.0];

fn check(failures: &mut Vec<String>, label: &str, what: &str, got: f64, want: &Json) {
    let w = f64::from_bits(bits(want));
    if got.to_bits() != w.to_bits() && !(got.is_nan() && w.is_nan()) {
        failures.push(format!("{label} {what}: got {got:?} want {w:?}"));
    }
}

#[test]
fn tdigest_matches_elasticsearch() {
    let fx = load();
    let mut failures = Vec::new();
    let cases = fx.get("tdigest").unwrap().as_array().unwrap();
    for case in cases {
        let kind = case.get("kind").and_then(Json::as_str).unwrap();
        let c = case.get("c").and_then(Json::as_f64).unwrap();
        let n = case.get("n").and_then(Json::as_i64).unwrap() as usize;
        let dist = case.get("dist").and_then(Json::as_i64).unwrap();
        let parts = case.get("parts").and_then(Json::as_i64).unwrap() as usize;
        let seed = case.get("seed").and_then(Json::as_i64).unwrap();
        let label = format!("[{kind} c={c} n={n} dist={dist} parts={parts}]");
        if kind == "merging" {
            // Plain merging digests aren't used by aggregations on their own.
            continue;
        }
        let make = || TDigest::new(c, kind == "avl");
        let mut r = JavaRandom::new(seed);
        let mut ds: Vec<TDigest> = (0..parts).map(|_| make()).collect();
        for i in 0..n {
            ds[i % parts].add(value(&mut r, dist)).unwrap();
        }
        let sort_first = case.get("sort_first").and_then(Json::as_array).unwrap();
        if let Some(w) = sort_first.first() {
            check(&mut failures, &label, "quantile before compress", ds[0].quantile(0.5), w);
        }
        let mut d = if parts == 1 {
            let mut d = ds.pop().unwrap();
            d.compress();
            d
        } else {
            let mut d = make();
            for x in &mut ds {
                x.compress();
            }
            for x in &mut ds {
                d.merge(x).unwrap();
            }
            d.compress();
            d
        };
        let want_centroids = case.get("centroids").and_then(Json::as_i64).unwrap() as usize;
        let got_centroids = d.centroids().len();
        if got_centroids != want_centroids {
            failures.push(format!("{label} centroids: got {got_centroids} want {want_centroids}"));
        }
        let qs = case.get("q").and_then(Json::as_array).unwrap();
        for (q, w) in QS.iter().zip(qs) {
            let got = d.quantile(*q);
            check(&mut failures, &label, &format!("q{q}"), got, w);
        }
        let (lo, hi) = (d.min(), d.max());
        let cdfs = case.get("cdf").and_then(Json::as_array).unwrap();
        for (k, w) in cdfs.iter().enumerate() {
            let x = lo + (hi - lo) * k as f64 / 10.0;
            let got = d.cdf(x).unwrap();
            check(&mut failures, &label, &format!("cdf({x})"), got, w);
        }
    }
    if !failures.is_empty() {
        panic!("{} mismatches:\n{}", failures.len(), failures.iter().take(40).cloned().collect::<Vec<_>>().join("\n"));
    }
}

#[test]
fn hdr_matches_elasticsearch() {
    let fx = load();
    let mut failures = Vec::new();
    for case in fx.get("hdr").unwrap().as_array().unwrap() {
        let digits = case.get("nsd").and_then(Json::as_i64).unwrap() as u32;
        let n = case.get("n").and_then(Json::as_i64).unwrap() as usize;
        let dist = case.get("dist").and_then(Json::as_i64).unwrap();
        let parts = case.get("parts").and_then(Json::as_i64).unwrap() as usize;
        let seed = case.get("seed").and_then(Json::as_i64).unwrap();
        let label = format!("[hdr digits={digits} n={n} dist={dist} parts={parts}]");
        let mut r = JavaRandom::new(seed);
        let mut hs: Vec<DoubleHistogram> = (0..parts).map(|_| DoubleHistogram::new(digits)).collect();
        let mut err = None;
        for i in 0..n {
            if let Err(e) = hs[i % parts].record(value(&mut r, dist)) {
                err = Some(e);
                break;
            }
        }
        if let Some(want) = case.get("error").and_then(Json::as_str) {
            match err {
                Some(e) if e.reason == want => {}
                Some(e) => failures.push(format!("{label} error: got {}\n want {want}", e.reason)),
                None => failures.push(format!("{label} expected error {want}")),
            }
            continue;
        }
        if let Some(e) = err {
            failures.push(format!("{label} unexpected error {}", e.reason));
            continue;
        }
        let h = if parts == 1 {
            hs.pop().unwrap()
        } else {
            let mut h = DoubleHistogram::empty_like(&hs[0]);
            for x in &hs {
                h.add(x).unwrap();
            }
            h
        };
        let total = case.get("total").and_then(Json::as_i64).unwrap();
        if h.total_count() != total {
            failures.push(format!("{label} total: got {} want {total}", h.total_count()));
        }
        check(&mut failures, &label, "min", h.min_value(), case.get("min").unwrap());
        check(&mut failures, &label, "max", h.max_value(), case.get("max").unwrap());
        for (p, w) in PS.iter().zip(case.get("p").and_then(Json::as_array).unwrap()) {
            check(&mut failures, &label, &format!("p{p}"), h.value_at_percentile(*p), w);
        }
        let (lo, hi) = (h.min_value(), h.max_value());
        for (k, w) in case.get("rank").and_then(Json::as_array).unwrap().iter().enumerate() {
            let x = lo + (hi - lo) * k as f64 / 10.0;
            check(&mut failures, &label, &format!("rank({x})"), h.percentile_at_or_below(x), w);
        }
    }
    if !failures.is_empty() {
        panic!("{} mismatches:\n{}", failures.len(), failures.iter().take(40).cloned().collect::<Vec<_>>().join("\n"));
    }
}
