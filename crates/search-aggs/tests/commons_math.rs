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

/// A function under test, taking the case's arguments.
type Function = fn(&[f64]) -> f64;

/// Every function's outputs, bit for bit; the failures of all of them are reported together.
#[test]
fn functions_match_commons_math() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/aggs-commons-math.json");
    let fx = Json::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
    let functions: [(&str, Function); 7] = [
        ("exp", |a| cm::exp(a[0])),
        ("log", |a| cm::log(a[0])),
        ("log1p", |a| cm::log1p(a[0])),
        ("logGamma", |a| cm::log_gamma(a[0])),
        ("logBeta", |a| cm::log_beta(a[0], a[1])),
        ("regularizedBeta", |a| cm::regularized_beta(a[0], a[1], a[2])),
        ("tCdf", |a| cm::t_cdf(a[0], a[1])),
    ];
    let mut bad = Vec::new();
    for (name, f) in functions {
        let cases = fx.get(name).and_then(Json::as_array).unwrap();
        let before = bad.len();
        for c in cases {
            let v: Vec<f64> = c.as_array().unwrap().iter().map(bits).collect();
            let (args, want) = v.split_at(v.len() - 1);
            let got = f(args);
            if !same(got, want[0]) && bad.len() - before < 20 {
                bad.push(format!("{name}{args:?}: got {got:e} ({:x}) want {:e} ({:x})", got.to_bits(), want[0], want[0].to_bits()));
            }
        }
    }
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}
