//! `geo_point` runtime fields in geo queries and sorts (`GeoPointScriptFieldType`).
//!
//! A runtime field's points are encoded as `GeoPointFieldScript.emit` encodes them (quantized
//! to the doc values encoding), then:
//!
//! - `geo_bounding_box`, `geo_distance` and `geo_shape` relate them to the query's geometries
//!   as Lucene's `Component2D` does (`GeoPointScriptFieldGeoShapeQuery`): `intersects` when
//!   any point is inside, `within` when all are, `disjoint` when none is (a document without
//!   points matches none of them), `contains` only for point geometries;
//! - `distance_feature` scores `pivot / (pivot + d)`, `d` the nearest point's arc distance
//!   (`GeoPointScriptFieldDistanceFeatureQuery`), on documents with points;
//! - `_geo_distance` sorts on the distances' `mode` (min ascending, max descending), documents
//!   without points last.

use super::geo::{quantize_lat, quantize_lon};
use super::query::{QueryCtx, arc};
use super::runtime::{Evaluator, EvalLeaf, Predicate, RtField, RtValue};
use crate::error::EsResult;
use search_index::QueryRef;
use search_index::SegmentContext;
use search_index::collector::top_field::{GeoDistanceType, SortMode};
use search_index::query::{GeoShape, ShapeRelation};
use search_index::util::geo2d::Component;
use std::sync::Arc;

/// What a geo query on a runtime field matches.
#[derive(Debug)]
pub struct GeoPredicate {
    /// The query's geometries (`None`: none that can hold a point).
    component: Option<Component>,
    relation: ShapeRelation,
    /// Every geometry is a point (`contains` can hold).
    points_only: bool,
    describe: String,
}

impl GeoPredicate {
    pub fn new(relation: ShapeRelation, shapes: &[GeoShape]) -> GeoPredicate {
        let parts: Vec<Component> = shapes.iter().filter_map(component).collect();
        let component = match parts.len() {
            0 => None,
            1 => parts.into_iter().next(),
            _ => Some(Component::tree(parts)),
        };
        let describe = shapes.iter().map(GeoShape::lucene_string).collect::<Vec<_>>().join(", ");
        GeoPredicate { component, relation, points_only: !shapes.is_empty() && shapes.iter().all(|s| matches!(s, GeoShape::Point(..))), describe }
    }

    pub fn describe(&self, field: &str) -> String {
        format!("GeoPointScriptFieldGeoShapeQuery(field={field}, relation={}, geometries=[{}])", self.relation.lucene_name(), self.describe)
    }

    pub fn matches(&self, values: &[RtValue]) -> bool {
        let mut pts = points(values).peekable();
        if pts.peek().is_none() {
            return false;
        }
        let Some(c) = &self.component else { return self.relation == ShapeRelation::Disjoint };
        let mut inside = pts.map(|(lat, lon)| c.contains(lon, lat));
        match self.relation {
            ShapeRelation::Intersects => inside.any(|b| b),
            ShapeRelation::Contains => self.points_only && inside.any(|b| b),
            ShapeRelation::Within => inside.all(|b| b),
            ShapeRelation::Disjoint => !inside.any(|b| b),
        }
    }
}

/// A shape as `LatLonGeometry.create` makes its component.
fn component(s: &GeoShape) -> Option<Component> {
    Some(match s {
        GeoShape::Point(lat, lon) => Component::point(*lat, *lon),
        GeoShape::Rectangle(a, b, c, d) => Component::rectangle(*a, *b, *c, *d),
        GeoShape::Line(pts) if pts.len() >= 2 => {
            let lats: Vec<f64> = pts.iter().map(|p| p.0).collect();
            let lons: Vec<f64> = pts.iter().map(|p| p.1).collect();
            Component::line(&lats, &lons)
        }
        GeoShape::Line(_) => return None,
        GeoShape::Polygon(p) => Component::polygon(&p.to_geo_polygon()?),
        GeoShape::Circle { lat, lon, radius } => Component::circle(*lat, *lon, *radius),
    })
}

/// A doc's points as the script's encoding leaves them.
fn points(values: &[RtValue]) -> impl Iterator<Item = (f64, f64)> + '_ {
    values.iter().filter_map(|v| match v {
        RtValue::Geo(lat, lon) => Some((quantize_lat(*lat), quantize_lon(*lon))),
        _ => None,
    })
}

/// A geo query on runtime field `f`.
pub fn shape_query(ctx: &QueryCtx, f: Arc<RtField>, relation: ShapeRelation, shapes: &[GeoShape]) -> EsResult<QueryRef> {
    ctx.runtime_query(f, Predicate::Geo(GeoPredicate::new(relation, shapes)))
}

/// `distance_feature` on runtime field `f`.
pub fn distance_feature(ctx: &QueryCtx, f: Arc<RtField>, lat: f64, lon: f64, pivot: f64) -> EsResult<QueryRef> {
    let ev = Arc::new(Evaluator::new(&ctx.cluster.scripts, ctx.st, ctx.table, ctx.shard as i32, &ctx.idx.name, f.clone())?);
    let query = ctx.runtime_query(f, Predicate::Exists)?;
    let script = Arc::new(DistanceScore(Arc::new(Distance { ev, lat, lon, pivot })));
    Ok(arc(search_index::query::function_score::ScriptScoreQuery { query, script, min_score: None }))
}

struct Distance {
    ev: Arc<Evaluator>,
    lat: f64,
    lon: f64,
    pivot: f64,
}

struct DistanceScore(Arc<Distance>);

impl Distance {
    fn nearest(&self, values: &[RtValue]) -> Option<f64> {
        points(values).map(|(lat, lon)| search_index::util::geo::haversin_meters(self.lat, self.lon, lat, lon)).reduce(f64::min)
    }
}

struct DistanceScoreLeaf {
    s: Arc<Distance>,
    leaf: EvalLeaf,
}

impl search_index::query::function_score::ScoreScript for DistanceScore {
    fn needs_score(&self) -> bool {
        false
    }
    fn leaf(&self, seg: &SegmentContext) -> search_index::Result<Box<dyn search_index::query::function_score::LeafScoreScript>> {
        Ok(Box::new(DistanceScoreLeaf { s: self.0.clone(), leaf: self.0.ev.leaf(seg).map_err(super::scripts::park_error)? }))
    }
    fn describe(&self) -> String {
        let d = &self.0;
        format!("GeoPointScriptFieldDistanceFeatureQuery(field={}, origin=POINT({} {}), pivot={})", d.ev.field.name, d.lon, d.lat, d.pivot)
    }
}

impl search_index::query::function_score::LeafScoreScript for DistanceScoreLeaf {
    fn score(&mut self, doc: u32, _sub: f32) -> search_index::Result<f64> {
        let vals = self.leaf.values(doc).map_err(super::scripts::park_error)?;
        Ok(match self.s.nearest(&vals) {
            Some(d) => (self.s.pivot / (self.s.pivot + d)) as f32 as f64,
            None => 0.0,
        })
    }
}

/// `_geo_distance` on runtime field `f`.
pub fn sort_key(ev: Arc<Evaluator>, points: Vec<(f64, f64)>, distance_type: GeoDistanceType, unit_meters: f64, reverse: bool, mode: Option<SortMode>) -> search_index::collector::top_field::SortKey {
    use search_index::collector::top_field::{ScriptSort, SortKey};
    let mode = mode.unwrap_or(if reverse { SortMode::Max } else { SortMode::Min });
    let script = Arc::new(GeoSortScript(Arc::new(GeoSort { ev, points, distance_type, unit_meters, mode })));
    SortKey::Script(ScriptSort { script, reverse, mode, nested: None })
}

struct GeoSort {
    ev: Arc<Evaluator>,
    points: Vec<(f64, f64)>,
    distance_type: GeoDistanceType,
    unit_meters: f64,
    mode: SortMode,
}

struct GeoSortScript(Arc<GeoSort>);

struct GeoSortLeaf {
    s: Arc<GeoSort>,
    leaf: EvalLeaf,
    dists: Vec<f64>,
}

impl search_index::collector::top_field::SortScript for GeoSortScript {
    fn leaf(&self, seg: &SegmentContext) -> search_index::Result<Box<dyn search_index::collector::top_field::LeafSortScript>> {
        Ok(Box::new(GeoSortLeaf { s: self.0.clone(), leaf: self.0.ev.leaf(seg).map_err(super::scripts::park_error)?, dists: Vec::new() }))
    }
}

impl search_index::collector::top_field::LeafSortScript for GeoSortLeaf {
    fn number(&mut self, doc: u32) -> search_index::Result<f64> {
        let vals = self.leaf.values(doc).map_err(super::scripts::park_error)?;
        let s = &self.s;
        self.dists.clear();
        for (lat, lon) in points(&vals) {
            for &(olat, olon) in &s.points {
                let m = match s.distance_type {
                    GeoDistanceType::Arc => search_index::util::geo::haversin_meters(olat, olon, lat, lon),
                    GeoDistanceType::Plane => search_index::util::geo::plane_meters(olat, olon, lat, lon),
                };
                self.dists.push(m / s.unit_meters);
            }
        }
        if self.dists.is_empty() {
            return Ok(f64::INFINITY);
        }
        let n = self.dists.len() as f64;
        Ok(match s.mode {
            SortMode::Min => self.dists.iter().copied().fold(f64::INFINITY, f64::min),
            SortMode::Max => self.dists.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            SortMode::Sum => self.dists.iter().sum(),
            SortMode::Avg => self.dists.iter().sum::<f64>() / n,
            SortMode::Median => {
                self.dists.sort_unstable_by(f64::total_cmp);
                let k = self.dists.len();
                if k % 2 == 1 { self.dists[k / 2] } else { (self.dists[k / 2 - 1] + self.dists[k / 2]) / 2.0 }
            }
        })
    }
}
