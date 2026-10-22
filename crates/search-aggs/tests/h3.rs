//! The H3 port against Elasticsearch's H3 library itself (`fixtures/aggs-h3-*.json`, from
//! `fixtures/oracle/H3*.java`): fdlibm, the library's `FastMath`, and cells.

mod common;

use std::collections::BTreeMap;

use search_aggs::Json;
use search_aggs::{fdlibm, h3};

fn bits(v: &Json) -> f64 {
    f64::from_bits(u64::from_str_radix(v.as_str().unwrap(), 16).unwrap())
}

fn same(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
}

fn math() -> Json {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/aggs-h3-math.json");
    Json::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn check(fx: &Json, name: &str, f: &dyn Fn(&[f64]) -> Option<f64>) {
    let cases = fx.get(name).and_then(Json::as_array).unwrap();
    let mut bad = Vec::new();
    let mut ran = 0;
    for c in cases {
        let v: Vec<f64> = c.as_array().unwrap().iter().map(bits).collect();
        let (args, want) = v.split_at(v.len() - 1);
        let Some(got) = f(args) else { continue };
        ran += 1;
        if !same(got, want[0]) {
            bad.push(format!("{name}{args:?}: got {got:e} want {:e}", want[0]));
        }
    }
    assert!(ran > cases.len() / 2, "{name}: only {ran} of {} ran", cases.len());
    assert!(bad.is_empty(), "{} of {ran} differ:\n{}", bad.len(), bad[..bad.len().min(10)].join("\n"));
}

/// `StrictMath` arguments the fdlibm port reduces (no Payne-Hanek path).
const MEDIUM: f64 = 823_549.0;

#[test]
fn fdlibm_matches_strict_math() {
    let fx = math();
    check(&fx, "strict_sin", &|a| (a[0].abs() < MEDIUM).then(|| fdlibm::sin(a[0])));
    check(&fx, "strict_cos", &|a| (a[0].abs() < MEDIUM).then(|| fdlibm::cos(a[0])));
    check(&fx, "strict_tan", &|a| Some(fdlibm::tan(a[0])));
    check(&fx, "strict_asin", &|a| Some(fdlibm::asin(a[0])));
    check(&fx, "strict_atan", &|a| Some(fdlibm::atan(a[0])));
    check(&fx, "strict_pow84", &|a| Some(fdlibm::pow(a[0], 1.0 / 84.0)));
}

#[test]
fn fast_math_matches_elasticsearch_h3() {
    let fx = math();
    check(&fx, "fast_sin", &|a| Some(h3::fast_math::sin(a[0])));
    check(&fx, "fast_cos", &|a| Some(h3::fast_math::cos(a[0])));
    check(&fx, "fast_sin_big", &|a| Some(h3::fast_math::sin(a[0])));
    check(&fx, "fast_tan", &|a| Some(h3::fast_math::tan(a[0])));
    check(&fx, "fast_asin", &|a| Some(h3::fast_math::asin(a[0])));
    check(&fx, "fast_acos", &|a| Some(h3::fast_math::acos(a[0])));
    check(&fx, "fast_atan", &|a| Some(h3::fast_math::atan(a[0])));
    check(&fx, "fast_atan2", &|a| Some(h3::fast_math::atan2(a[0], a[1])));
}

fn cells() -> Json {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/aggs-h3-cells.json");
    Json::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn geo_to_h3_matches_elasticsearch() {
    let fx = cells();
    let points = fx.get("points").and_then(Json::as_array).unwrap();
    let mut bad = Vec::new();
    for p in points {
        let p = p.as_array().unwrap();
        let (lat, lon) = (bits(&p[0]), bits(&p[1]));
        let res = p[2].as_i64().unwrap() as i32;
        let want = p[3].as_str().unwrap();
        let got = h3::h3_to_string(h3::geo_to_h3(lat, lon, res));
        if got != want {
            bad.push(format!("({lat}, {lon}) res {res}: got {got} want {want}"));
        }
        assert_eq!(h3::string_to_h3(want).map(h3::h3_to_string).as_deref(), Some(want));
    }
    assert!(bad.is_empty(), "{} of {} differ:\n{}", bad.len(), points.len(), bad[..bad.len().min(10)].join("\n"));
}

#[test]
fn cell_boundaries_and_bounds_match_elasticsearch() {
    let fx = cells();
    let list = fx.get("cells").and_then(Json::as_array).unwrap();
    let mut bad = Vec::new();
    for c in list {
        let h = h3::string_to_h3(c.get("h").unwrap().as_str().unwrap()).unwrap();
        assert_eq!(h3::is_pentagon(h), c.get("pentagon").unwrap().as_bool().unwrap());
        let center = h3::h3_to_lat_lng(h);
        let wc: Vec<f64> = c.get("center").unwrap().as_array().unwrap().iter().map(bits).collect();
        if !same(center.lat, wc[0]) || !same(center.lon, wc[1]) {
            bad.push(format!("{}: center {center:?} want {wc:?}", h3::h3_to_string(h)));
        }
        let want: Vec<f64> = c.get("boundary").unwrap().as_array().unwrap().iter().map(bits).collect();
        let got: Vec<f64> = h3::h3_to_geo_boundary(h).points.iter().flat_map(|p| [p.lat, p.lon]).collect();
        if got.len() != want.len() || got.iter().zip(&want).any(|(a, b)| !same(*a, *b)) {
            bad.push(format!("{}: boundary {got:?}\n  want {want:?}", h3::h3_to_string(h)));
        }
        let wb: Vec<f64> = c.get("bounds").unwrap().as_array().unwrap().iter().map(bits).collect();
        let (t, l, b, r) = h3::cell_bounds(h);
        if [t, l, b, r].iter().zip(&wb).any(|(a, w)| !same(*a, *w)) {
            bad.push(format!("{}: bounds {:?} want {wb:?}", h3::h3_to_string(h), (t, l, b, r)));
        }
    }
    assert!(bad.is_empty(), "{} of {} differ:\n{}", bad.len(), list.len(), bad[..bad.len().min(8)].join("\n"));
}

/// `geohex_grid` end to end: per precision, docs per cell (multi-valued docs once per cell),
/// ordered by count then cell id descending, and the `bounds` filter (a point inside the box,
/// or a cell whose bounding box intersects it).
#[test]
fn geohex_grid_buckets_points_by_cell() {
    let mapping = Json::parse(r#"{"properties":{"g":{"type":"geo_point"}}}"#).unwrap();
    let index = common::TestIndex::from_mapping(&mapping);
    let mut state = 0x1234_5678u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    // Clustered points (so cells repeat), some docs with two points.
    let mut docs = Vec::new();
    let mut points: Vec<Vec<(f64, f64)>> = Vec::new();
    for i in 0..600 {
        let n = if i % 7 == 0 { 2 } else { 1 };
        let mut ps = Vec::new();
        for _ in 0..n {
            let (clat, clon) = [(48.85, 2.35), (-33.9, 151.2), (40.7, -74.0), (64.1, -21.9)][(next() * 4.0) as usize];
            ps.push((clat + (next() - 0.5) * 4.0, clon + (next() - 0.5) * 6.0));
        }
        let arr = Json::Arr(ps.iter().map(|&(lat, lon)| Json::obj().with("lat", Json::Float(lat)).with("lon", Json::Float(lon))).collect());
        docs.push((i.to_string(), Json::obj().with("g", arr)));
        points.push(ps);
    }
    let shards: Vec<search_index::Shard> = [0, 1].iter().map(|s| index.shard(&docs.iter().skip(*s).step_by(2).cloned().collect::<Vec<_>>(), 2)).collect();
    let searchers: Vec<search_index::Searcher> = shards.iter().map(|s| s.searcher()).collect();
    // Doc values hold encoded points: bucket what the index stores.
    let stored = |lat: f64, lon: f64| search_index::util::geo::decode_geo_dv(search_index::util::geo::encode_geo_dv(lat, lon));
    for precision in [0, 1, 2, 3, 4] {
        for bounded in [false, true] {
            let (top, left, bottom, right) = (55.0, -30.0, 35.0, 10.0);
            let bounds = if bounded { format!(r#","bounds":{{"top_left":{{"lat":{top},"lon":{left}}},"bottom_right":{{"lat":{bottom},"lon":{right}}}}}"#) } else { String::new() };
            let body = format!(r#"{{"aggs":{{"h":{{"geohex_grid":{{"field":"g","precision":{precision},"size":5000{bounds}}}}}}}}}"#);
            let got = common::run(&index, &searchers, &body, false).unwrap();
            let mut want: BTreeMap<u64, u64> = BTreeMap::new();
            for ps in &points {
                let mut cells: Vec<u64> = ps
                    .iter()
                    .filter_map(|&(lat, lon)| {
                        let (lat, lon) = stored(lat, lon);
                        let h = h3::geo_to_h3(lat, lon, precision);
                        if !bounded {
                            return Some(h);
                        }
                        let inside = lat <= top && lat >= bottom && lon >= left && lon <= right;
                        let (ct, cl, cb, cr) = h3::cell_bounds(h);
                        let overlaps = |min_x: f64, max_x: f64| left < max_x && right > min_x;
                        let boxed = top > cb && bottom < ct && if cl > cr { overlaps(-180.0, cr) || overlaps(cl, 180.0) } else { overlaps(cl, cr) };
                        (inside || boxed).then_some(h)
                    })
                    .collect();
                cells.sort_unstable();
                cells.dedup();
                for c in cells {
                    *want.entry(c).or_default() += 1;
                }
            }
            let mut want: Vec<(u64, u64)> = want.into_iter().collect();
            want.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
            let buckets: Vec<(u64, u64)> = got
                .get("h")
                .unwrap()
                .get("buckets")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(|b| (h3::string_to_h3(b.get("key").unwrap().as_str().unwrap()).unwrap(), b.get("doc_count").unwrap().as_i64().unwrap() as u64))
                .collect();
            assert_eq!(buckets, want, "precision {precision} bounded {bounded}");
            assert!(!buckets.is_empty());
        }
    }
    let e = common::run(&index, &searchers, r#"{"aggs":{"h":{"geohex_grid":{"field":"g","precision":16}}}}"#, false).unwrap_err();
    assert_eq!(e.reason, "[1:54] [geohex_grid] failed to parse field [precision]");
    assert_eq!(e.caused_by.as_ref().map(|c| c.reason.as_str()), Some("Invalid geohex aggregation precision of 16. Must be between 0 and 15"));
}
