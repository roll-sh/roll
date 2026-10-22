//! Cartesian `shape` and `point` fields against Elasticsearch 9.5.5: every relation of
//! envelopes, polygons (with holes), points, multi-points, lines, multi-polygons and circles,
//! on a `shape` field, an indexed `point` field with doc values and a doc-values-only `point`
//! field (`fixtures/xy_shapes.json`, from `fixtures/gen_xy_shapes.py`), including the profile
//! descriptions; plus a randomized check that BKD pruning, doc values and brute force agree.

mod common;

use std::sync::Arc;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use search_codec::{DocValuesType, PointsSpec};
use search_index::doc::{DocValue, IndexableDoc, IndexableField};
use search_index::query::shape::{PackedTriangle, SHAPE_POINTS, polygon_triangles, xy_line_triangles, xy_point_triangles};
use search_index::query::xy::{self, XyGeometry};
use search_index::query::{ConstantScoreQuery, ShapeRelation};
use search_index::schema::FieldSchema;
use search_index::util::geo2d::Component;
use search_index::util::tessellator::GeoPolygon;
use search_index::{IndexOp, QueryRef, Schema, Searcher, Shard};
use serde_json::Value;

const S: u32 = 10;
const P: u32 = 11;
const PN: u32 = 12;
const SN: u32 = 13;
/// Shape doc values without the bounding box header (a bare triangle concatenation).
const SN_BARE: u32 = 14;

fn schema() -> Schema {
    Schema::new()
        .with_field(FieldSchema::new(S, "s").with_points(SHAPE_POINTS))
        .unwrap()
        .with_field(FieldSchema::new(P, "p").with_points(PointsSpec::new(2, 4)).with_doc_values(DocValuesType::SortedNumeric))
        .unwrap()
        .with_field(FieldSchema::new(PN, "pn").with_doc_values(DocValuesType::SortedNumeric))
        .unwrap()
        .with_field(FieldSchema::new(SN, "sn").with_doc_values(DocValuesType::Binary))
        .unwrap()
        .with_field(FieldSchema::new(SN_BARE, "sn_bare").with_doc_values(DocValuesType::Binary))
        .unwrap()
}

fn f(v: &Value) -> f32 {
    v.as_f64().unwrap() as f32
}

fn coords(v: &Value) -> (Vec<f32>, Vec<f32>) {
    let pts = v.as_array().unwrap();
    (pts.iter().map(|p| f(&p[0])).collect(), pts.iter().map(|p| f(&p[1])).collect())
}

fn polygon(rings: &Value) -> GeoPolygon {
    let rings = rings.as_array().unwrap();
    let holes = rings[1..]
        .iter()
        .map(|r| {
            let (x, y) = coords(r);
            GeoPolygon::xy(&x, &y, vec![]).unwrap()
        })
        .collect();
    let (x, y) = coords(&rings[0]);
    GeoPolygon::xy(&x, &y, holes).unwrap()
}

/// `(min_x, max_x, min_y, max_y)` of a GeoJSON envelope `[[minX, maxY], [maxX, minY]]`.
fn envelope(c: &Value) -> (f32, f32, f32, f32) {
    (f(&c[0][0]), f(&c[1][0]), f(&c[1][1]), f(&c[0][1]))
}

/// Elasticsearch's `CartesianShapeIndexer`.
fn index_triangles(g: &Value) -> Vec<PackedTriangle> {
    let c = &g["coordinates"];
    match g["type"].as_str().unwrap() {
        "Point" => xy_point_triangles(f(&c[0]), f(&c[1])).unwrap(),
        "LineString" => {
            let (x, y) = coords(c);
            xy_line_triangles(&x, &y).unwrap()
        }
        "Polygon" => polygon_triangles(&polygon(c), true).unwrap(),
        "MultiPolygon" => c.as_array().unwrap().iter().flat_map(|p| polygon_triangles(&polygon(p), true).unwrap()).collect(),
        "Envelope" => {
            let (a, b, cc, d) = envelope(c);
            polygon_triangles(&GeoPolygon::xy(&[a, b, b, a, a], &[cc, cc, d, d, cc], vec![]).unwrap(), false).unwrap()
        }
        t => panic!("{t}"),
    }
}

/// Elasticsearch's `LuceneGeometriesUtils.toXYGeometry`.
fn query_geometries(g: &Value) -> Vec<XyGeometry> {
    let c = &g["coordinates"];
    match g["type"].as_str().unwrap() {
        "Point" => vec![XyGeometry::Point { x: f(&c[0]), y: f(&c[1]) }],
        "MultiPoint" => c.as_array().unwrap().iter().map(|p| XyGeometry::Point { x: f(&p[0]), y: f(&p[1]) }).collect(),
        "LineString" => {
            let (xs, ys) = coords(c);
            vec![XyGeometry::Line { xs, ys }]
        }
        "Polygon" => vec![XyGeometry::Polygon(polygon(c))],
        "MultiPolygon" => c.as_array().unwrap().iter().map(|p| XyGeometry::Polygon(polygon(p))).collect(),
        "Envelope" => {
            let (min_x, max_x, min_y, max_y) = envelope(c);
            vec![XyGeometry::Rectangle { min_x, max_x, min_y, max_y }]
        }
        "Circle" => vec![XyGeometry::Circle { x: f(&c[0]), y: f(&c[1]), radius: g["radius"].as_str().unwrap().parse::<f32>().unwrap() }],
        t => panic!("{t}"),
    }
}

fn relation(s: &str) -> ShapeRelation {
    match s {
        "intersects" => ShapeRelation::Intersects,
        "within" => ShapeRelation::Within,
        "disjoint" => ShapeRelation::Disjoint,
        _ => ShapeRelation::Contains,
    }
}

fn hits(s: &Searcher, ids: &std::collections::HashMap<u32, String>, q: &QueryRef) -> Vec<u64> {
    let mut col = search_index::collector::TopScoreCollector::new(10_000, u64::MAX);
    s.search(q, &mut col).unwrap();
    let mut got: Vec<u64> = col.top_docs().hits.iter().map(|h| ids[&h.doc].parse().unwrap()).collect();
    got.sort_unstable();
    got
}

#[test]
fn cartesian_queries_match_elasticsearch() {
    let fx: Value = serde_json::from_str(include_str!("fixtures/xy_shapes.json")).unwrap();
    let docs = fx["docs"].as_array().unwrap();
    let points = fx["points"].as_array().unwrap();
    for layout in ["one segment", "many segments"] {
        let shard = Shard::in_memory(schema());
        for (i, g) in docs.iter().enumerate() {
            let mut d = IndexableDoc::new(i.to_string().into_bytes());
            let tris = index_triangles(g);
            for t in &tris {
                d = d.field(IndexableField::Point { field: S, packed: t.to_vec() });
            }
            d = d.doc_value(SN, DocValue::Binary(search_index::query::shape::doc_value(&tris)));
            for p in points[i].as_array().unwrap() {
                let (x, y) = (f(&p[0]), f(&p[1]));
                d = d.point(P, xy::point_bytes(x, y).unwrap().to_vec()).doc_value(P, DocValue::SortedNumeric(xy::dv_value(x, y).unwrap()));
                d = d.doc_value(PN, DocValue::SortedNumeric(xy::dv_value(x, y).unwrap()));
            }
            shard.index(IndexOp::new(d)).unwrap();
            if layout == "many segments" && i % 41 == 0 {
                shard.refresh().unwrap();
            }
        }
        shard.refresh().unwrap();
        let s = shard.searcher();
        let ids = common::ids(&s);
        let mut failures = Vec::new();
        let (mut checked, mut described) = (0, 0);
        for c in fx["cases"].as_array().unwrap() {
            let geoms = query_geometries(&c["q"]);
            let rel = relation(c["relation"].as_str().unwrap());
            let queries: [(&str, QueryRef); 4] = [
                ("s", xy::shape_query(S, rel, geoms.clone(), true, true).unwrap()),
                ("sn", xy::shape_query(SN, rel, geoms.clone(), false, true).unwrap()),
                ("p", xy::point_shape_query(P, rel, geoms.clone(), true, true).unwrap()),
                ("pn", xy::point_shape_query(PN, rel, geoms.clone(), false, true).unwrap()),
            ];
            for (name, q) in queries {
                let want: Vec<u64> = c[name].as_array().unwrap_or_else(|| panic!("{name}: {c}")).iter().map(|v| v.as_u64().unwrap()).collect();
                let got = hits(&s, &ids, &q);
                checked += 1;
                if got != want {
                    let extra: Vec<&u64> = got.iter().filter(|g| !want.contains(g)).collect();
                    let missing: Vec<&u64> = want.iter().filter(|w| !got.contains(w)).collect();
                    failures.push(format!("{name} {} {}: extra {extra:?} missing {missing:?}", c["relation"], c["q"]));
                }
                if let Some(d) = c.get(format!("{name}_description")) {
                    let wrapped: QueryRef = Arc::new(ConstantScoreQuery::new(q.clone()));
                    let ours = s.rewrite(&wrapped).unwrap().lucene_string(s.schema());
                    described += 1;
                    if ours != d.as_str().unwrap() {
                        failures.push(format!("{name} description:\n  ours {ours}\n  es   {}", d.as_str().unwrap()));
                    }
                }
            }
        }
        assert!(checked > 1300 && described > 180, "{checked} {described}");
        assert!(failures.is_empty(), "{layout}: {} of {checked} differ:\n{}", failures.len(), failures.join("\n"));
    }
}

/// A random cartesian geometry around the origin.
fn random_geometry(r: &mut StdRng) -> XyGeometry {
    let c = |r: &mut StdRng| r.gen_range(-60.0f32..60.0);
    match r.gen_range(0..5) {
        0 => XyGeometry::Point { x: c(r).round(), y: c(r).round() },
        1 => {
            let n = r.gen_range(2..5);
            XyGeometry::Line { xs: (0..n).map(|_| c(r)).collect(), ys: (0..n).map(|_| c(r)).collect() }
        }
        2 => {
            let (cx, cy, n) = (c(r), c(r), r.gen_range(3..9));
            let rad = r.gen_range(5.0f32..40.0);
            let (mut xs, mut ys) = (Vec::new(), Vec::new());
            for i in 0..n {
                let a = (i as f32 + r.gen_range(-0.3f32..0.3)) * std::f32::consts::TAU / n as f32;
                let rr = rad * r.gen_range(0.5f32..1.0);
                xs.push(cx + rr * a.cos());
                ys.push(cy + rr * a.sin());
            }
            xs.push(xs[0]);
            ys.push(ys[0]);
            XyGeometry::Polygon(GeoPolygon::xy(&xs, &ys, vec![]).unwrap())
        }
        3 => {
            let (x, y) = (c(r).round(), c(r).round());
            XyGeometry::Rectangle { min_x: x, max_x: x + r.gen_range(1..50) as f32, min_y: y, max_y: y + r.gen_range(1..50) as f32 }
        }
        _ => XyGeometry::Circle { x: c(r), y: c(r), radius: r.gen_range(1.0f32..50.0) },
    }
}

/// Brute force of a point relation: a doc's values against the geometries' components.
fn point_oracle(values: &[(f32, f32)], rel: ShapeRelation, geoms: &[XyGeometry]) -> bool {
    let c = Component::tree(geoms.iter().map(|g| g.component()).collect());
    let inside = |&(x, y): &(f32, f32)| c.contains(x as f64, y as f64);
    match rel {
        ShapeRelation::Intersects => values.iter().any(inside),
        ShapeRelation::Disjoint => !values.is_empty() && !values.iter().any(inside),
        ShapeRelation::Within => values.iter().any(inside) && values.iter().all(inside),
        ShapeRelation::Contains => {
            geoms.iter().all(|g| matches!(g, XyGeometry::Point { .. }))
                && geoms.iter().all(|g| {
                    let one = g.component();
                    values.iter().any(|&(x, y)| one.contains(x as f64, y as f64))
                })
        }
    }
}

#[test]
fn random_point_queries_agree_with_brute_force() {
    let seeds: u64 = std::env::var("XY_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(6);
    for seed in 0..seeds {
        let mut r = StdRng::seed_from_u64(seed);
        let shard = Shard::in_memory(schema());
        let n = r.gen_range(50..400);
        let mut values: Vec<Vec<(f32, f32)>> = Vec::new();
        for i in 0..n {
            let mut d = IndexableDoc::new(i.to_string().into_bytes());
            let k = if r.gen_bool(0.1) { 0 } else { r.gen_range(1..4) };
            let mut vs = Vec::new();
            for _ in 0..k {
                // Integer grids put values on query edges and corners.
                let (x, y) = if r.gen_bool(0.5) { (r.gen_range(-60..60) as f32, r.gen_range(-60..60) as f32) } else { (r.gen_range(-70.0f32..70.0), r.gen_range(-70.0f32..70.0)) };
                d = d.point(P, xy::point_bytes(x, y).unwrap().to_vec()).doc_value(P, DocValue::SortedNumeric(xy::dv_value(x, y).unwrap()));
                d = d.doc_value(PN, DocValue::SortedNumeric(xy::dv_value(x, y).unwrap()));
                vs.push((x, y));
            }
            values.push(vs);
            shard.index(IndexOp::new(d)).unwrap();
            if r.gen_bool(0.02) {
                shard.refresh().unwrap();
            }
        }
        // Some deletes.
        for (i, v) in values.iter_mut().enumerate() {
            if r.gen_bool(0.05) {
                shard.delete(search_index::DeleteOp::new(i.to_string().into_bytes())).unwrap();
                *v = vec![(f32::NAN, f32::NAN)];
            }
        }
        shard.refresh().unwrap();
        let s = shard.searcher();
        let ids = common::ids(&s);
        for _ in 0..60 {
            let k = r.gen_range(1..3);
            let geoms: Vec<XyGeometry> = (0..k).map(|_| random_geometry(&mut r)).collect();
            let rel = [ShapeRelation::Intersects, ShapeRelation::Within, ShapeRelation::Disjoint, ShapeRelation::Contains][r.gen_range(0..4)];
            let want: Vec<u64> = values
                .iter()
                .enumerate()
                .filter(|(_, v)| !(v.len() == 1 && v[0].0.is_nan()) && point_oracle(v, rel, &geoms))
                .map(|(i, _)| i as u64)
                .collect();
            for (name, q) in [
                ("bkd", xy::point_shape_query(P, rel, geoms.clone(), true, false).unwrap()),
                ("both", xy::point_shape_query(P, rel, geoms.clone(), true, true).unwrap()),
                ("dv", xy::point_shape_query(PN, rel, geoms.clone(), false, true).unwrap()),
            ] {
                assert_eq!(hits(&s, &ids, &q), want, "seed {seed} {name} {rel:?} {geoms:?}");
            }
        }
    }
}

/// Shape queries over random shapes: BKD-pruned results equal a scan of every doc's triangles.
#[test]
fn random_shape_queries_agree_with_triangle_scan() {
    use search_index::query::shape::XyShapeQuery;
    let seeds: u64 = std::env::var("XY_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(6);
    for seed in 0..seeds {
        let mut r = StdRng::seed_from_u64(1000 + seed);
        let shard = Shard::in_memory(schema());
        let n = r.gen_range(30..250);
        let mut shapes = Vec::new();
        for i in 0..n {
            let g = random_geometry(&mut r);
            let tris = match &g {
                XyGeometry::Point { x, y } => xy_point_triangles(*x, *y).unwrap(),
                XyGeometry::Line { xs, ys } => xy_line_triangles(xs, ys).unwrap(),
                XyGeometry::Polygon(p) => match polygon_triangles(p, false) {
                    Ok(t) => t,
                    Err(_) => continue,
                },
                XyGeometry::Rectangle { min_x, max_x, min_y, max_y } => {
                    let p = GeoPolygon::xy(&[*min_x, *max_x, *max_x, *min_x, *min_x], &[*min_y, *min_y, *max_y, *max_y, *min_y], vec![]).unwrap();
                    polygon_triangles(&p, false).unwrap()
                }
                XyGeometry::Circle { .. } => continue,
            };
            let mut d = IndexableDoc::new(i.to_string().into_bytes());
            for t in &tris {
                d = d.field(IndexableField::Point { field: S, packed: t.to_vec() });
            }
            let dv = search_index::query::shape::doc_value(&tris);
            let bare: Vec<u8> = tris.concat();
            assert_eq!(search_index::query::shape::doc_value_triangles(&dv).collect::<Vec<_>>(), bare.chunks_exact(28).collect::<Vec<_>>());
            assert!(search_index::query::shape::doc_value_bbox(&dv).is_some());
            assert!(search_index::query::shape::doc_value_bbox(&bare).is_none());
            d = d.doc_value(SN, DocValue::Binary(dv));
            d = d.doc_value(SN_BARE, DocValue::Binary(bare.into()));
            shard.index(IndexOp::new(d)).unwrap();
            shapes.push((i as u64, tris));
            if r.gen_bool(0.03) {
                shard.refresh().unwrap();
            }
        }
        shard.refresh().unwrap();
        let s = shard.searcher();
        let ids = common::ids(&s);
        // A one-doc index per shape evaluates the same rules without multi-doc BKD pruning.
        let singles: Vec<(u64, Searcher)> = shapes
            .iter()
            .map(|(id, tris)| {
                let one = Shard::in_memory(schema());
                let mut d = IndexableDoc::new(b"x".to_vec());
                for t in tris {
                    d = d.field(IndexableField::Point { field: S, packed: t.to_vec() });
                }
                one.index(IndexOp::new(d)).unwrap();
                one.refresh().unwrap();
                (*id, one.searcher())
            })
            .collect();
        for _ in 0..40 {
            let geoms = vec![random_geometry(&mut r)];
            for rel in [ShapeRelation::Intersects, ShapeRelation::Within, ShapeRelation::Disjoint, ShapeRelation::Contains] {
                let q: QueryRef = Arc::new(XyShapeQuery::new(S, geoms.clone(), rel).unwrap());
                let got = hits(&s, &ids, &q);
                let want: Vec<u64> = singles.iter().filter(|(_, one)| one.count(&q).unwrap() == 1).map(|(id, _)| *id).collect();
                assert_eq!(got, want, "seed {seed} {rel:?} {geoms:?}");
                let dv: QueryRef = Arc::new(search_index::query::ShapeDvQuery::xy(SN, rel, geoms.clone()).unwrap());
                assert_eq!(hits(&s, &ids, &dv), want, "doc values: seed {seed} {rel:?} {geoms:?}");
                let bare: QueryRef = Arc::new(search_index::query::ShapeDvQuery::xy(SN_BARE, rel, geoms.clone()).unwrap());
                assert_eq!(hits(&s, &ids, &bare), want, "bare doc values: seed {seed} {rel:?} {geoms:?}");
            }
        }
    }
}

