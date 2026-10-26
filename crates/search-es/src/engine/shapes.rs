//! `geo_shape` and `shape` values to indexed triangles (Elasticsearch's `GeoShapeIndexer` and
//! `CartesianShapeIndexer` over Lucene's `LatLonShape` / `XYShape` encodings): points and lines
//! as degenerate triangles, polygons tessellated (self-intersections checked for geo), envelopes
//! as quantized polygons, collections flattened. Geo polygons crossing the dateline are split at
//! ±180° the way the indexer normalizes them.

use crate::error::{EsError, EsResult};
use crate::query::geo::Geometry;
use search_index::query::shape::{self, PackedTriangle};
use search_index::util::tessellator::GeoPolygon;

fn err(e: search_index::Error) -> EsError {
    EsError::illegal_argument(e.to_string())
}

/// Triangles of one value.
pub fn triangles(g: &Geometry, cartesian: bool) -> EsResult<Vec<PackedTriangle>> {
    let mut out = Vec::new();
    collect(g, cartesian, &mut out)?;
    Ok(out)
}

fn xy(c: &[f64]) -> (f64, f64) {
    (c.first().copied().unwrap_or(0.0), c.get(1).copied().unwrap_or(0.0))
}

fn polygon_of(rings: &[Vec<Vec<f64>>], cartesian: bool) -> EsResult<GeoPolygon> {
    let ring = |r: &Vec<Vec<f64>>| -> (Vec<f64>, Vec<f64>) {
        let xs: Vec<f64> = r.iter().map(|c| xy(c).0).collect();
        let ys: Vec<f64> = r.iter().map(|c| xy(c).1).collect();
        (xs, ys)
    };
    let Some(outer) = rings.first() else {
        return Err(EsError::illegal_argument("at least one ring required"));
    };
    let mut holes = Vec::new();
    for h in &rings[1..] {
        let (xs, ys) = ring(h);
        holes.push(if cartesian {
            GeoPolygon::xy(&xs.iter().map(|v| *v as f32).collect::<Vec<_>>(), &ys.iter().map(|v| *v as f32).collect::<Vec<_>>(), Vec::new()).map_err(err)?
        } else {
            GeoPolygon::new(ys, xs, Vec::new()).map_err(err)?
        });
    }
    let (xs, ys) = ring(outer);
    if cartesian {
        GeoPolygon::xy(&xs.iter().map(|v| *v as f32).collect::<Vec<_>>(), &ys.iter().map(|v| *v as f32).collect::<Vec<_>>(), holes).map_err(err)
    } else {
        GeoPolygon::new(ys, xs, holes).map_err(err)
    }
}

fn collect(g: &Geometry, cartesian: bool, out: &mut Vec<PackedTriangle>) -> EsResult<()> {
    match g {
        Geometry::Point(c) => {
            let (x, y) = xy(c);
            out.extend(if cartesian { shape::xy_point_triangles(x as f32, y as f32) } else { shape::point_triangles(y, x) }.map_err(err)?);
        }
        Geometry::MultiPoint(ps) => {
            for c in ps {
                collect(&Geometry::Point(c.clone()), cartesian, out)?;
            }
        }
        Geometry::LineString(cs) => {
            let xs: Vec<f64> = cs.iter().map(|c| xy(c).0).collect();
            let ys: Vec<f64> = cs.iter().map(|c| xy(c).1).collect();
            if cartesian {
                let xf: Vec<f32> = xs.iter().map(|v| *v as f32).collect();
                let yf: Vec<f32> = ys.iter().map(|v| *v as f32).collect();
                out.extend(shape::xy_line_triangles(&xf, &yf).map_err(err)?);
            } else {
                out.extend(shape::line_triangles(&ys, &xs).map_err(err)?);
            }
        }
        Geometry::MultiLineString(ls) => {
            for l in ls {
                collect(&Geometry::LineString(l.clone()), cartesian, out)?;
            }
        }
        Geometry::Polygon(rings) => {
            let p = polygon_of(rings, cartesian)?;
            out.extend(shape::polygon_triangles(&p, !cartesian).map_err(err)?);
        }
        Geometry::MultiPolygon(ps) => {
            for p in ps {
                collect(&Geometry::Polygon(p.clone()), cartesian, out)?;
            }
        }
        Geometry::GeometryCollection(gs) => {
            for g in gs {
                crate::stack::maybe_grow(|| collect(g, cartesian, out))?;
            }
        }
        Geometry::Envelope { min_x, max_x, max_y, min_y } => {
            if !cartesian && min_x > max_x {
                // Crosses the dateline: two boxes.
                collect(&Geometry::Envelope { min_x: *min_x, max_x: 180.0, max_y: *max_y, min_y: *min_y }, cartesian, out)?;
                return collect(&Geometry::Envelope { min_x: -180.0, max_x: *max_x, max_y: *max_y, min_y: *min_y }, cartesian, out);
            }
            let ring = vec![vec![*min_x, *min_y], vec![*max_x, *min_y], vec![*max_x, *max_y], vec![*min_x, *max_y], vec![*min_x, *min_y]];
            let p = polygon_of(&[ring], cartesian)?;
            out.extend(shape::polygon_triangles(&p, false).map_err(err)?);
        }
        Geometry::Circle { .. } => {
            return Err(EsError::illegal_argument("invalid shape type found [Circle] while indexing shape"));
        }
    }
    Ok(())
}

/// The value as the centroid calculator sees it ([`search_aggs::shape_field::Geometry`]):
/// `(x, y)` coordinates (`(lon, lat)` for `geo_shape`) as the document gives them.
pub fn centroid_geometry(g: &Geometry) -> search_aggs::shape_field::Geometry {
    use search_aggs::shape_field::Geometry as A;
    let pts = |cs: &[Vec<f64>]| -> Vec<(f64, f64)> { cs.iter().map(|c| xy(c)).collect() };
    match g {
        Geometry::Point(c) => {
            let (x, y) = xy(c);
            A::Point(x, y)
        }
        Geometry::MultiPoint(ps) => A::MultiPoint(pts(ps)),
        Geometry::LineString(cs) => A::Line(pts(cs)),
        Geometry::MultiLineString(ls) => A::MultiLine(ls.iter().map(|l| pts(l)).collect()),
        Geometry::Polygon(rings) => match rings.split_first() {
            Some((shell, holes)) => A::Polygon(pts(shell), holes.iter().map(|h| pts(h)).collect()),
            None => A::Collection(Vec::new()),
        },
        Geometry::MultiPolygon(ps) => A::MultiPolygon(
            ps.iter()
                .filter_map(|rings| rings.split_first().map(|(shell, holes)| (pts(shell), holes.iter().map(|h| pts(h)).collect())))
                .collect(),
        ),
        Geometry::GeometryCollection(gs) => A::Collection(gs.iter().map(centroid_geometry).collect()),
        Geometry::Envelope { min_x, max_x, max_y, min_y } => A::Rectangle { min_x: *min_x, max_x: *max_x, min_y: *min_y, max_y: *max_y },
        Geometry::Circle { .. } => A::Circle,
    }
}
