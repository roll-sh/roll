//! `geo_line` against Elasticsearch's own `GeoLineBucketedSort` (`fixtures/aggs-geo-line.json`,
//! from `fixtures/oracle/GeoLines.java`): which points survive a full bucket among tied sort
//! values, their order, the rendered coordinates, and the out-of-bounds failures Elasticsearch
//! hits on documents without a point. The reference node's license doesn't serve `geo_line`
//! over REST, so the oracle runs its classes directly.

mod common;

use common::{TestIndex, run};
use search_aggs::Json;
use search_codec::points::encode;

#[test]
fn geo_line_matches_elasticsearch_bucketed_sort() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/aggs-geo-line.json");
    let cases = Json::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mapping = Json::parse(r#"{"properties":{"p":{"type":"geo_point"},"t":{"type":"double"}}}"#).unwrap();
    let index = TestIndex::from_mapping(&mapping);
    let mut checked = (0, 0);
    for (c, case) in cases.as_array().unwrap().iter().enumerate() {
        let size = case.get("size").unwrap().as_i64().unwrap();
        let desc = case.get("desc").unwrap().as_bool().unwrap();
        let docs: Vec<(String, Json)> = case
            .get("docs")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let d = d.as_array().unwrap();
                let mut o = Json::obj();
                if let Some(t) = d[0].as_f64() {
                    o.push("t", Json::Float(t));
                }
                if let (Some(lat), Some(lon)) = (d[1].as_i64(), d[2].as_i64()) {
                    let (lat, lon) = (encode::decode_latitude(lat as i32), encode::decode_longitude(lon as i32));
                    // The doc value must be the oracle's encoded point exactly.
                    assert_eq!(encode::encode_latitude(lat) as i64, d[1].as_i64().unwrap());
                    o.push("p", Json::obj().with("lat", Json::Float(lat)).with("lon", Json::Float(lon)));
                }
                (i.to_string(), o)
            })
            .collect();
        let n = docs.len();
        let shard = index.shard(&docs, 1);
        let searchers = vec![shard.searcher()];
        let body = format!(
            r#"{{"aggs":{{"g":{{"geo_line":{{"point":{{"field":"p"}},"sort":{{"field":"t"}},"size":{size},"sort_order":"{}","include_sort":true}}}}}}}}"#,
            if desc { "desc" } else { "asc" }
        );
        let got = run(&index, &searchers, &body, false);
        if let Some(err) = case.get("error").and_then(Json::as_str) {
            let e = got.expect_err("Elasticsearch failed");
            assert_eq!((e.kind.as_ref(), e.reason.as_str()), ("array_index_out_of_bounds_exception", err), "case {c}");
            checked.1 += 1;
            continue;
        }
        let got = got.unwrap_or_else(|e| panic!("case {c}: {e:?}"));
        let g = got.get("g").unwrap();
        let geometry = g.get("geometry").unwrap();
        assert_eq!(geometry.get("type"), case.get("type"), "case {c}");
        assert_eq!(geometry.get("coordinates"), case.get("coordinates"), "case {c}: coordinates");
        let props = g.get("properties").unwrap();
        assert_eq!(props.get("sort_values"), case.get("sort_values"), "case {c}: sort values");
        assert_eq!(props.get("complete").and_then(Json::as_bool), Some(n as i64 <= size), "case {c}");
        checked.0 += 1;
    }
    assert!(checked.0 > 250 && checked.1 > 0, "{checked:?}");
}
