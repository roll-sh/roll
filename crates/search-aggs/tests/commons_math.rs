//! `search_aggs::commons_math` against Commons Math 3.6.1 itself: thousands of inputs
//! (`tests/fixtures/aggs-commons-math.json`, from `fixtures/oracle/CommonsMath.java`), every
//! output compared bit for bit.

use search_aggs::Json;
use search_aggs::commons_math as cm;

fn bits(v: &Json) -> f64 {
    f64::from_bits(u64::from_str_radix(v.as_str().unwrap(), 16).unwrap())
}

fn same(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
}

fn fixture() -> Json {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/aggs-commons-math.json");
    Json::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn check(name: &str, f: &dyn Fn(&[f64]) -> f64) {
    let fx = fixture();
    let cases = fx.get(name).and_then(Json::as_array).unwrap();
    let mut bad = Vec::new();
    for c in cases {
        let v: Vec<f64> = c.as_array().unwrap().iter().map(bits).collect();
        let (args, want) = v.split_at(v.len() - 1);
        let got = f(args);
        if !same(got, want[0]) {
            bad.push(format!("{name}{args:?}: got {got:e} ({:x}) want {:e} ({:x})", got.to_bits(), want[0], want[0].to_bits()));
        }
    }
    assert!(bad.is_empty(), "{} of {} differ:\n{}", bad.len(), cases.len(), bad[..bad.len().min(20)].join("\n"));
}

#[test]
fn fast_math_exp() {
    check("exp", &|a| cm::exp(a[0]));
}

#[test]
fn fast_math_log() {
    check("log", &|a| cm::log(a[0]));
}

#[test]
fn fast_math_log1p() {
    check("log1p", &|a| cm::log1p(a[0]));
}

#[test]
fn log_gamma() {
    check("logGamma", &|a| cm::log_gamma(a[0]));
}

#[test]
fn log_beta() {
    check("logBeta", &|a| cm::log_beta(a[0], a[1]));
}

#[test]
fn regularized_beta() {
    check("regularizedBeta", &|a| cm::regularized_beta(a[0], a[1], a[2]));
}

#[test]
fn t_distribution_cdf() {
    check("tCdf", &|a| cm::t_cdf(a[0], a[1]));
}
