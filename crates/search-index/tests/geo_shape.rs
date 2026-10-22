//! `geo_shape` support: the tessellator against Lucene 10.5's `Tessellator` (fixture
//! `tessellation.txt`: 219 polygons — convex, concave, z-order sized, with holes, sharing
//! vertices, collinear, self-intersecting — tessellated by `TessOracle.java` from
//! `gen_tessellation.py`'s output; long outputs are stored as an FNV-1a hash).
//!
//! Regenerate: `python3 -I gen_tessellation.py 7 > cases.txt`, split by the `C`/`N` prefix
//! (self-intersection checks on/off), run `java -cp classes:lucene-core.jar TessOracle in.txt
//! [check]`, and join each input with its output line.

mod common;

use search_index::util::tessellator::{GeoPolygon, tessellate};

fn ring(s: &str) -> (Vec<f64>, Vec<f64>) {
    let mut lats = Vec::new();
    let mut lons = Vec::new();
    for p in s.split(',') {
        let mut it = p.split_whitespace();
        lons.push(it.next().unwrap().parse::<f64>().unwrap());
        lats.push(it.next().unwrap().parse::<f64>().unwrap());
    }
    (lats, lons)
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn run(input: &str, check: bool) -> String {
    let mut rings = input.split('|');
    let (lats, lons) = ring(rings.next().unwrap());
    let mut holes = Vec::new();
    for r in rings {
        let (a, b) = ring(r);
        match GeoPolygon::new(a, b, vec![]) {
            Ok(h) => holes.push(h),
            Err(e) => return format!("ERR {}", reason(&e)),
        }
    }
    let p = match GeoPolygon::new(lats, lons, holes) {
        Ok(p) => p,
        Err(e) => return format!("ERR {}", reason(&e)),
    };
    match tessellate(&p, check) {
        Ok(ts) => {
            let mut s = format!("OK {} ", ts.len());
            for t in &ts {
                for v in 0..3 {
                    s.push_str(&format!("{} {} ", t.x[v], t.y[v]));
                }
                let f: Vec<&str> = t.from_polygon.iter().map(|&b| if b { "1" } else { "0" }).collect();
                s.push_str(&f.join(" "));
                s.push(';');
            }
            s
        }
        Err(e) => format!("ERR {}", reason(&e)),
    }
}

fn reason(e: &search_index::Error) -> String {
    match e {
        search_index::Error::Es { reason, .. } => reason.clone(),
        other => other.to_string(),
    }
}

#[test]
fn tessellation_matches_lucene() {
    let mut failures = Vec::new();
    let mut n = 0;
    for line in include_str!("fixtures/tessellation.txt").lines() {
        let (case, want) = line.split_once('\t').unwrap();
        let (mode, input) = case.split_at(2);
        let got = run(input, mode == "C ");
        let ok = if let Some(h) = want.strip_prefix("HASH ") {
            let (count, hash) = h.split_once(' ').unwrap();
            got.starts_with(&format!("OK {count} ")) && format!("{:016x}", fnv(&got)) == hash
        } else {
            got == want
        };
        n += 1;
        if !ok {
            failures.push(format!("{case}\n  ours {}\n  want {}", &got[..got.len().min(300)], &want[..want.len().min(300)]));
        }
    }
    assert!(n > 200);
    assert!(failures.is_empty(), "{} of {n} differ:\n{}", failures.len(), failures.join("\n"));
}

// ---------------------------------------------------------------- queries vs Elasticsearch

use std::sync::Arc;

use search_index::doc::{IndexableDoc, IndexableField};
use search_index::query::ShapeRelation;
use search_index::query::shape::{
    GeoShapeQuery, PackedTriangle, QueryGeometry, SHAPE_POINTS, line_triangles, point_triangles, polygon_triangles, quantize_lat, quantize_lon,
};
use search_index::schema::FieldSchema;
use search_index::{IndexOp, QueryRef, Schema, Shard};
use serde_json::Value;

const S: u32 = 10;

fn coords(v: &Value) -> (Vec<f64>, Vec<f64>) {
    let pts = v.as_array().unwrap();
    let lons = pts.iter().map(|p| p[0].as_f64().unwrap()).collect();
    let lats = pts.iter().map(|p| p[1].as_f64().unwrap()).collect();
    (lats, lons)
}

fn polygon(rings: &Value, q: bool) -> GeoPolygon {
    let qa = |v: Vec<f64>, f: fn(f64) -> f64| if q { v.into_iter().map(f).collect() } else { v };
    let rings = rings.as_array().unwrap();
    let holes = rings[1..]
        .iter()
        .map(|r| {
            let (a, o) = coords(r);
            GeoPolygon::new(qa(a, quantize_lat), qa(o, quantize_lon), vec![]).unwrap()
        })
        .collect();
    let (a, o) = coords(&rings[0]);
    GeoPolygon::new(qa(a, quantize_lat), qa(o, quantize_lon), holes).unwrap()
}

/// Elasticsearch's `GeoShapeIndexer` for the fixture's shapes.
fn index_triangles(g: &Value) -> Vec<PackedTriangle> {
    let c = &g["coordinates"];
    match g["type"].as_str().unwrap() {
        "Point" => point_triangles(c[1].as_f64().unwrap(), c[0].as_f64().unwrap()).unwrap(),
        "LineString" => {
            let (a, o) = coords(c);
            line_triangles(&a, &o).unwrap()
        }
        "Polygon" => polygon_triangles(&polygon(c, false), true).unwrap(),
        "MultiPolygon" => c.as_array().unwrap().iter().flat_map(|p| polygon_triangles(&polygon(p, false), true).unwrap()).collect(),
        "Envelope" => {
            // The quantized box as a polygon.
            let (min_lon, max_lat, max_lon, min_lat) = (c[0][0].as_f64().unwrap(), c[0][1].as_f64().unwrap(), c[1][0].as_f64().unwrap(), c[1][1].as_f64().unwrap());
            let (a, b, x, y) = (quantize_lat(min_lat), quantize_lat(max_lat), quantize_lon(min_lon), quantize_lon(max_lon));
            polygon_triangles(&GeoPolygon::new(vec![a, a, b, b, a], vec![x, y, y, x, x], vec![]).unwrap(), false).unwrap()
        }
        t => panic!("{t}"),
    }
}

/// A fixture radius (`"200km"`) in meters.
fn radius(g: &Value) -> f64 {
    let r = g["radius"].as_str().unwrap();
    r.strip_suffix("km").unwrap().parse::<f64>().unwrap() * 1000.0
}

/// Elasticsearch's quantized query geometries.
fn query_geometries(g: &Value) -> Vec<QueryGeometry> {
    let c = &g["coordinates"];
    match g["type"].as_str().unwrap() {
        "Circle" => vec![QueryGeometry::Circle { lat: quantize_lat(c[1].as_f64().unwrap()), lon: quantize_lon(c[0].as_f64().unwrap()), radius: radius(g) }],
        "Point" => vec![QueryGeometry::Point { lat: quantize_lat(c[1].as_f64().unwrap()), lon: quantize_lon(c[0].as_f64().unwrap()) }],
        "LineString" => {
            let (a, o) = coords(c);
            vec![QueryGeometry::Line { lats: a.into_iter().map(quantize_lat).collect(), lons: o.into_iter().map(quantize_lon).collect() }]
        }
        "Polygon" => vec![QueryGeometry::Polygon(polygon(c, true))],
        "MultiPolygon" => c.as_array().unwrap().iter().map(|p| QueryGeometry::Polygon(polygon(p, true))).collect(),
        "Envelope" => vec![QueryGeometry::Rectangle {
            min_lat: quantize_lat(c[1][1].as_f64().unwrap()),
            max_lat: quantize_lat(c[0][1].as_f64().unwrap()),
            min_lon: quantize_lon(c[0][0].as_f64().unwrap()),
            max_lon: quantize_lon(c[1][0].as_f64().unwrap()),
        }],
        t => panic!("{t}"),
    }
}

#[test]
fn shape_queries_match_elasticsearch() {
    let fx: Value = serde_json::from_str(include_str!("fixtures/geo_shapes.json")).unwrap();
    const SN: u32 = 12;
    let schema = Schema::new()
        .with_field(FieldSchema::new(S, "s").with_points(SHAPE_POINTS))
        .unwrap()
        .with_field(FieldSchema::new(SN, "sn").with_doc_values(search_codec::DocValuesType::Binary))
        .unwrap();
    for layout in ["one segment", "many segments"] {
        let shard = Shard::in_memory(schema.clone());
        for (i, g) in fx["docs"].as_array().unwrap().iter().enumerate() {
            let mut d = IndexableDoc::new(i.to_string().into_bytes());
            let tris = index_triangles(g);
            for t in &tris {
                d = d.field(IndexableField::Point { field: S, packed: t.to_vec() });
            }
            d = d.doc_value(SN, search_index::DocValue::Binary(search_index::query::shape::doc_value(&tris)));
            shard.index(IndexOp::new(d)).unwrap();
            if layout == "many segments" && i % 37 == 0 {
                shard.refresh().unwrap();
            }
        }
        shard.refresh().unwrap();
        let s = shard.searcher();
        let ids = common::ids(&s);
        let mut failures = Vec::new();
        let mut checked = 0;
        for c in fx["cases"].as_array().unwrap() {
            let Some(want) = c.get("ids") else { continue };
            let relation = match c["relation"].as_str().unwrap() {
                "intersects" => ShapeRelation::Intersects,
                "within" => ShapeRelation::Within,
                "disjoint" => ShapeRelation::Disjoint,
                _ => ShapeRelation::Contains,
            };
            let want: Vec<u64> = want.as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
            let index: QueryRef = Arc::new(GeoShapeQuery::new(S, query_geometries(&c["q"]), relation));
            let dv: QueryRef = Arc::new(search_index::query::ShapeDvQuery::geo(SN, relation, query_geometries(&c["q"])));
            for (name, q) in [("index", index), ("doc values", dv)] {
                let mut col = search_index::collector::TopScoreCollector::new(10_000, u64::MAX);
                s.search(&q, &mut col).unwrap();
                let mut got: Vec<u64> = col.top_docs().hits.iter().map(|h| ids[&h.doc].parse().unwrap()).collect();
                got.sort_unstable();
                checked += 1;
                if got != want {
                    let extra: Vec<&u64> = got.iter().filter(|g| !want.contains(g)).collect();
                    let missing: Vec<&u64> = want.iter().filter(|w| !got.contains(w)).collect();
                    failures.push(format!("{name} {} {}: extra {extra:?} missing {missing:?}", c["relation"], c["q"]));
                }
            }
        }
        assert!(checked > 250);
        assert!(failures.is_empty(), "{layout}: {} of {checked} differ:\n{}", failures.len(), failures.join("\n"));
    }
}

/// The same geometries against a multi-valued `geo_point` field (`LatLonPoint.newGeometryQuery`).
#[test]
fn point_shape_queries_match_elasticsearch() {
    use search_index::query::{GeoPointShapeQuery, GeoShape};
    use search_index::query::geo::Polygon;
    const P: u32 = 11;
    let fx: Value = serde_json::from_str(include_str!("fixtures/geo_shapes.json")).unwrap();
    let schema = Schema::new().with_field(FieldSchema::new(P, "p").with_points(search_codec::PointsSpec::new(2, 4))).unwrap();
    let shard = Shard::in_memory(schema);
    for (i, pts) in fx["points"].as_array().unwrap().iter().enumerate() {
        let mut d = IndexableDoc::new(i.to_string().into_bytes());
        for p in pts.as_array().unwrap() {
            let (lon, lat) = (p[0].as_f64().unwrap(), p[1].as_f64().unwrap());
            d = d.field(IndexableField::Point { field: P, packed: search_codec::points::encode::lat_lon_bytes(lat, lon).to_vec() });
        }
        shard.index(IndexOp::new(d)).unwrap();
        if i % 50 == 0 {
            shard.refresh().unwrap();
        }
    }
    shard.refresh().unwrap();
    let s = shard.searcher();
    let ids = common::ids(&s);
    let ring = |r: &Value| -> Vec<(f64, f64)> {
        r.as_array().unwrap().iter().map(|p| (quantize_lat(p[1].as_f64().unwrap()), quantize_lon(p[0].as_f64().unwrap()))).collect()
    };
    let poly = |rings: &Value| -> GeoShape {
        let rs = rings.as_array().unwrap();
        GeoShape::Polygon(Polygon { outer: ring(&rs[0]), holes: rs[1..].iter().map(ring).collect() })
    };
    let mut failures = Vec::new();
    let mut checked = 0;
    for c in fx["point_cases"].as_array().unwrap() {
        let Some(want) = c.get("ids") else { continue };
        let g = &c["q"];
        let co = &g["coordinates"];
        let shapes = match g["type"].as_str().unwrap() {
            "Point" => vec![GeoShape::Point(quantize_lat(co[1].as_f64().unwrap()), quantize_lon(co[0].as_f64().unwrap()))],
            "Circle" => vec![GeoShape::Circle { lat: quantize_lat(co[1].as_f64().unwrap()), lon: quantize_lon(co[0].as_f64().unwrap()), radius: radius(g) }],
            "LineString" => vec![GeoShape::Line(ring(co))],
            "Polygon" => vec![poly(co)],
            "MultiPolygon" => co.as_array().unwrap().iter().map(poly).collect(),
            "Envelope" => vec![GeoShape::Rectangle(
                quantize_lat(co[1][1].as_f64().unwrap()),
                quantize_lat(co[0][1].as_f64().unwrap()),
                quantize_lon(co[0][0].as_f64().unwrap()),
                quantize_lon(co[1][0].as_f64().unwrap()),
            )],
            t => panic!("{t}"),
        };
        let relation = match c["relation"].as_str().unwrap() {
            "intersects" => ShapeRelation::Intersects,
            "within" => ShapeRelation::Within,
            "disjoint" => ShapeRelation::Disjoint,
            _ => ShapeRelation::Contains,
        };
        let q: QueryRef = Arc::new(GeoPointShapeQuery { field: P, shapes, relation });
        let mut col = search_index::collector::TopScoreCollector::new(10_000, u64::MAX);
        s.search(&q, &mut col).unwrap();
        let mut got: Vec<u64> = col.top_docs().hits.iter().map(|h| ids[&h.doc].parse().unwrap()).collect();
        got.sort_unstable();
        let want: Vec<u64> = want.as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
        checked += 1;
        if got != want {
            let extra: Vec<&u64> = got.iter().filter(|g| !want.contains(g)).collect();
            let missing: Vec<&u64> = want.iter().filter(|w| !got.contains(w)).collect();
            failures.push(format!("{} {}: extra {extra:?} missing {missing:?}", c["relation"], g));
        }
    }
    assert!(checked > 250);
    assert!(failures.is_empty(), "{} of {checked} differ:\n{}", failures.len(), failures.join("\n"));
}
