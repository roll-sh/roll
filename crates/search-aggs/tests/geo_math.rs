//! Java math behind geo cells against the JDK and Elasticsearch 9.5.5
//! (`tests/fixtures/aggs-geo-math.json`, written by `tests/fixtures/oracle/GeoMath.java`):
//! `StrictMath.expm1`, `ESSloppyMath.sinh` / `atan` and `GeoTileUtils.tileToLat`, bit for bit.

use search_aggs::Json;

fn d(j: &Json) -> f64 {
    f64::from_bits(u64::from_str_radix(j.as_str().unwrap(), 16).unwrap())
}

fn same(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
}

#[test]
fn geo_math_matches_java() {
    let path = format!("{}/tests/fixtures/aggs-geo-math.json", env!("CARGO_MANIFEST_DIR"));
    let fx = Json::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
    for row in fx.get("tile_to_lat").unwrap().as_array().unwrap() {
        let r = row.as_array().unwrap();
        let (z, y) = (r[0].as_i64().unwrap(), r[1].as_i64().unwrap());
        let got = search_aggs::geo::tile_to_lat(y, 1 << z);
        assert!(same(got, d(&r[2])), "tileToLat({y}, 2^{z}): {got} vs {}", d(&r[2]));
    }
    for row in fx.get("fns").unwrap().as_array().unwrap() {
        let r = row.as_array().unwrap();
        let x = d(&r[0]);
        assert!(same(search_aggs::fdlibm::expm1(x), d(&r[1])), "expm1({x})");
        if x.abs() < 22.0 {
            assert!(same(search_aggs::jmath::fast_sinh(x), d(&r[2])), "sinh({x})");
        }
        assert!(same(search_aggs::jmath::fast_atan(x), d(&r[3])), "atan({x})");
    }
}
