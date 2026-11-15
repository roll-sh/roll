//! Shape doc values against Elasticsearch 9.5.5's own spatial code
//! (`tests/fixtures/aggs-shapes.json`, written by `tests/fixtures/oracle/Shapes.java`): for
//! random geo and cartesian geometries, the triangles Elasticsearch indexes (compared with
//! `shape_field::shape_triangles`), the stored centroid (`CentroidCalculator`, compared with
//! ours from the same geometry), the bounding box aggregations read, and the cells of
//! `GeoTileGridTiler` / `GeoHashGridTiler` / `GeoHexGridTiler` at several precisions, with and
//! without bounds; and the H3 hierarchy the hex tiler walks (rings, children, overlapping
//! non-children, parents).

use search_aggs::Json;
use search_aggs::bucket::composite::GeoBounds;
use search_aggs::bucket::geogrid::HashBounds;
use search_aggs::bucket::shape_grid::{HashTiler, TileTiler};
use search_aggs::bucket::shape_hex::{HexBounds, HexTiler};
use search_aggs::geo::TileBounds;
use search_aggs::shape_field::{CentroidCalculator, Coords, Geometry, ShapeValue, encode_centroid, shape_triangles};

fn d(j: &Json) -> f64 {
    f64::from_bits(u64::from_str_radix(j.as_str().unwrap(), 16).unwrap())
}

fn arr(j: &Json) -> &[Json] {
    j.as_array().unwrap()
}

fn i(j: &Json) -> i64 {
    match j {
        Json::UInt(u) => *u as i64,
        _ => j.as_i64().unwrap_or_else(|| j.as_f64().unwrap() as i64),
    }
}

fn pts(j: &Json) -> Vec<(f64, f64)> {
    arr(j).iter().map(|p| (d(&arr(p)[0]), d(&arr(p)[1]))).collect()
}

fn geometry(j: &Json) -> Geometry {
    let c = j.get("c");
    match j.get("type").and_then(Json::as_str).unwrap() {
        "point" => {
            let p = arr(c.unwrap());
            Geometry::Point(d(&p[0]), d(&p[1]))
        }
        "line" => Geometry::Line(pts(c.unwrap())),
        "multipoint" => Geometry::MultiPoint(pts(c.unwrap())),
        "rect" => {
            let p = arr(c.unwrap());
            Geometry::Rectangle { min_x: d(&p[0]), max_x: d(&p[1]), min_y: d(&p[2]), max_y: d(&p[3]) }
        }
        "polygon" => Geometry::Polygon(pts(j.get("shell").unwrap()), arr(j.get("holes").unwrap()).iter().map(pts).collect()),
        "collection" => Geometry::Collection(arr(j.get("parts").unwrap()).iter().map(geometry).collect()),
        t => panic!("{t}"),
    }
}

fn hex_bytes(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|k| u8::from_str_radix(&s[k..k + 2], 16).unwrap()).collect()
}

fn triangles(j: &Json) -> Vec<[u8; 28]> {
    arr(j).iter().map(|t| hex_bytes(t.as_str().unwrap()).try_into().unwrap()).collect()
}

/// The oracle's centroid header as our companion encoding.
fn centroid_bytes(c: &[Json]) -> [u8; 17] {
    let mut out = [0u8; 17];
    out[0..4].copy_from_slice(&(i(&c[0]) as i32).to_be_bytes());
    out[4..8].copy_from_slice(&(i(&c[1]) as i32).to_be_bytes());
    out[8] = i(&c[2]) as u8;
    out[9..17].copy_from_slice(&d(&c[3]).to_bits().to_be_bytes());
    out
}

fn same(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits() || a == b
}

fn check_cells(mut got: Vec<i64>, want: &Json, what: &str) {
    got.sort_unstable();
    match want {
        Json::Arr(w) => assert_eq!(got, w.iter().map(i).collect::<Vec<_>>(), "{what}"),
        o => {
            let sum = got.iter().fold(0i64, |s, &x| s.wrapping_add(x));
            let xor = got.iter().fold(0i64, |s, &x| s ^ x);
            assert_eq!((got.len() as i64, sum, xor), (i(o.get("n").unwrap()), i(o.get("sum").unwrap()), i(o.get("xor").unwrap())), "{what}");
        }
    }
}

#[test]
fn shapes_match_elasticsearch() {
    let path = format!("{}/tests/fixtures/aggs-shapes.json", env!("CARGO_MANIFEST_DIR"));
    let fx = Json::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
    let boxes: Vec<GeoBounds> = arr(fx.get("boxes").unwrap())
        .iter()
        .map(|b| {
            let b = arr(b);
            GeoBounds { top: d(&b[0]), left: d(&b[1]), bottom: d(&b[2]), right: d(&b[3]) }
        })
        .collect();
    for (coords, key) in [(Coords::Geo, "geo"), (Coords::Cartesian, "cartesian")] {
        for (n, s) in arr(fx.get(key).unwrap()).iter().enumerate() {
            let g = geometry(s.get("geom").unwrap());
            let tris = triangles(s.get("triangles").unwrap());
            assert_eq!(shape_triangles(&g, coords).unwrap(), tris, "{key} {n}: triangles of {g:?}");
            let c = arr(s.get("centroid").unwrap());
            let mut calc = CentroidCalculator::new();
            calc.add(&g).unwrap();
            assert_eq!(encode_centroid(&calc, coords).unwrap(), centroid_bytes(c), "{key} {n}: centroid of {g:?}");
            let dv = search_index::query::shape::doc_value(&tris);
            let v = ShapeValue::from_doc_value(&dv, Some(&centroid_bytes(c)), coords).unwrap().unwrap();
            let bb = v.bounding_box();
            let want = arr(s.get("bbox").unwrap());
            for (name, got, w) in [
                ("top", bb.top, &want[0]),
                ("bottom", bb.bottom, &want[1]),
                ("neg_left", bb.neg_left, &want[2]),
                ("neg_right", bb.neg_right, &want[3]),
                ("pos_left", bb.pos_left, &want[4]),
                ("pos_right", bb.pos_right, &want[5]),
            ] {
                assert!(same(got, d(w)), "{key} {n}: bbox {name} {got} vs {}", d(w));
            }
            if coords == Coords::Cartesian {
                continue;
            }
            let mut cells = Vec::new();
            for t in arr(s.get("tiles").unwrap()) {
                let t = arr(t);
                let (p, bi) = (i(&t[0]) as u32, i(&t[1]));
                let tb = (bi >= 0).then(|| {
                    let b = boxes[bi as usize];
                    TileBounds::new(p, b.top, b.left, b.bottom, b.right)
                });
                TileTiler { precision: p, bounds: tb.as_ref() }.cells(&v, &mut cells).unwrap();
                check_cells(cells.clone(), &t[2], &format!("geo {n}: geotile {p} box {bi} of {g:?}"));
            }
            for t in arr(s.get("hexes").unwrap()) {
                let t = arr(t);
                let (p, bi) = (i(&t[0]) as i32, i(&t[1]));
                let hb = (bi >= 0).then(|| {
                    let b = boxes[bi as usize];
                    HexBounds::new(p, b.top, b.left, b.bottom, b.right)
                });
                HexTiler { precision: p, bounds: hb.as_ref() }.cells(&v, &mut cells).unwrap();
                cells.sort_unstable();
                check_cells(cells.clone(), &t[2], &format!("geo {n}: geohex {p} box {bi} of {g:?}"));
            }
            for t in arr(s.get("hashes").unwrap()) {
                let t = arr(t);
                let (p, bi) = (i(&t[0]) as u32, i(&t[1]));
                let hb = (bi >= 0).then(|| HashBounds::new(boxes[bi as usize]));
                let valid = |c: i64| hb.is_none_or(|h| h.valid_hash(c));
                HashTiler { precision: p, valid: hb.is_some().then_some(&valid as &dyn Fn(i64) -> bool) }.cells(&v, &mut cells).unwrap();
                check_cells(cells.clone(), &t[2], &format!("geo {n}: geohash {p} box {bi} of {g:?}"));
            }
        }
    }
}

#[test]
fn h3_hierarchy_matches_elasticsearch() {
    use search_aggs::h3::ring;
    let path = format!("{}/tests/fixtures/aggs-shapes.json", env!("CARGO_MANIFEST_DIR"));
    let fx = Json::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
    for e in arr(fx.get("h3").unwrap()) {
        let e = arr(e);
        let h = i(&e[0]) as u64;
        let ints = |j: &Json| arr(j).iter().map(|x| i(x) as u64).collect::<Vec<_>>();
        assert_eq!((0..ring::ring_size(h)).map(|k| ring::ring(h, k)).collect::<Vec<_>>(), ints(&e[1]), "ring of {h:x}");
        if search_aggs::h3::resolution(h) < 15 {
            assert_eq!((0..ring::children_size(h)).map(|k| ring::child(h, k)).collect::<Vec<_>>(), ints(&e[2]), "children of {h:x}");
            assert_eq!((0..ring::no_children_size(h)).map(|k| ring::no_child_intersecting(h, k)).collect::<Vec<_>>(), ints(&e[3]), "no-children of {h:x}");
        }
        if search_aggs::h3::resolution(h) > 0 {
            assert_eq!(ring::parent(h), i(&e[4]) as u64, "parent of {h:x}");
        }
    }
    assert_eq!(ring::res0_cells().len(), 122);
}
