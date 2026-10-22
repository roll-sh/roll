//! Distance feature queries (Lucene's `LongDistanceFeatureQuery` and
//! `LatLonPointDistanceFeatureQuery`; Elasticsearch's `distance_feature` query on `date`,
//! `date_nanos` and `geo_point` fields).
//!
//! Every doc with a value scores `weight · pivot / (pivot + distance)`, where the distance is
//! from the origin to the doc's closest value (numbers: absolute difference; geo points:
//! haversine meters). Iteration starts over the field's doc values. Under top-k, once the
//! collector's minimum competitive score is known, the largest distance that can still compete
//! is found by bisection and, when the docs within it are few enough (estimated from the BKD
//! tree, under an eighth of the current iterator's cost), the iterator becomes those docs: a
//! range of the points (numbers) or the circle's bounding box (geo). As in Lucene, these
//! refreshes are sampled once the collector has updated the minimum many times.

use std::any::Any;
use std::sync::Arc;

use search_codec::points::encode::{encode_latitude, encode_longitude, i32_from, i64_from};
use search_codec::{DocSetCursor, DocValuesReader, NO_MORE_DOCS, NumericDocValues, PointVisitor, Relation, SegmentReader, SortedNumericDocValues};

use crate::error::{Error, Result};
use crate::query::basic::{BoostQuery, SetScorer};
use crate::query::geo::{EARTH_MEAN_RADIUS_METERS, bbox_of_circle, check_lat, check_lon};
use crate::schema::Schema;
use crate::search::{Explanation, Query, QueryRef, Scorable, ScoreMode, Scorer, ScorerSupplier, Searcher, SegmentContext, Weight};
use crate::util::bitset::DocIdSetBuilder;
use crate::util::geo::{decode_geo_dv, haversin_meters, haversin_sort_key};
use crate::util::java::double_to_string;

const MIN_SKIP_INTERVAL: u32 = 32;
const MAX_SKIP_INTERVAL: u32 = 8192;

/// Where distances are measured from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Origin {
    /// A `long` field (dates in millis or nanos); the pivot is in the same unit.
    Long { origin: i64, pivot: i64 },
    /// A `geo_point` field; the pivot is in meters.
    Geo { lat: f64, lon: f64, pivot: f64 },
}

/// Scores docs by the distance of their closest value to an origin.
#[derive(Clone, Debug, PartialEq)]
pub struct DistanceFeatureQuery {
    pub field: u32,
    pub origin: Origin,
}

impl DistanceFeatureQuery {
    /// Lucene's `LongField.newDistanceFeatureQuery` (a weight other than 1 wraps the query in
    /// a boost).
    pub fn long(field: u32, weight: f32, origin: i64, pivot: i64) -> Result<QueryRef> {
        if pivot <= 0 {
            return Err(Error::illegal(format!("pivotDistance must be > 0, got {pivot}")));
        }
        Ok(boosted(DistanceFeatureQuery { field, origin: Origin::Long { origin, pivot } }, weight))
    }

    /// Lucene's `LatLonPoint.newDistanceFeatureQuery` (pivot in meters).
    pub fn geo(field: u32, weight: f32, lat: f64, lon: f64, pivot: f64) -> Result<QueryRef> {
        check_lat(lat)?;
        check_lon(lon)?;
        if pivot.is_nan() || pivot <= 0.0 {
            return Err(Error::illegal(format!("pivotDistance must be > 0, got {}", double_to_string(pivot))));
        }
        Ok(boosted(DistanceFeatureQuery { field, origin: Origin::Geo { lat, lon, pivot } }, weight))
    }
}

fn boosted(q: DistanceFeatureQuery, weight: f32) -> QueryRef {
    let q: QueryRef = Arc::new(q);
    if weight != 1.0 { Arc::new(BoostQuery::new(q, weight)) } else { q }
}

/// A field's values: sorted numeric, or single numeric.
#[derive(Clone)]
enum Values {
    Sorted(SortedNumericDocValues),
    Single(NumericDocValues),
}

impl Values {
    fn of(reader: &SegmentReader, field: u32) -> Option<Values> {
        match reader.doc_values(field)? {
            DocValuesReader::SortedNumeric(v) => Some(Values::Sorted(v.clone())),
            DocValuesReader::Numeric(v) => Some(Values::Single(v.clone())),
            _ => None,
        }
    }

    fn cursor(&self) -> DocSetCursor {
        match self {
            Values::Sorted(v) => v.docs().cursor(),
            Values::Single(v) => v.cursor(),
        }
    }

    /// The doc's value closest to the origin, given the cursor index when known.
    fn select(&self, o: &Origin, doc: u32, index: Option<u32>) -> Option<i64> {
        match self {
            Values::Single(v) => match index {
                Some(i) => Some(v.value_at(i)),
                None => v.get(doc),
            },
            Values::Sorted(v) => {
                let mut it = match index {
                    Some(i) => v.values_at(i),
                    None => v.values(doc),
                };
                let count = it.len();
                let first = it.next()?;
                if count == 1 {
                    return Some(first);
                }
                match *o {
                    Origin::Long { origin, .. } => {
                        // Ascending values: the last below the origin or the first at or above,
                        // whichever is closer (unsigned differences: they may overflow).
                        if first >= origin {
                            return Some(first);
                        }
                        let mut prev = first;
                        for next in it {
                            if next >= origin {
                                let below = origin.wrapping_sub(prev) as u64;
                                let above = next.wrapping_sub(origin) as u64;
                                return Some(if below < above { prev } else { next });
                            }
                            prev = next;
                        }
                        Some(prev)
                    }
                    Origin::Geo { lat, lon, .. } => {
                        let key = |v: i64| {
                            let (a, b) = decode_geo_dv(v);
                            haversin_sort_key(lat, lon, a, b)
                        };
                        let (mut best, mut best_key) = (first, key(first));
                        for next in it {
                            let k = key(next);
                            if k < best_key {
                                best = next;
                                best_key = k;
                            }
                        }
                        Some(best)
                    }
                }
            }
        }
    }
}

/// Distance of a selected value from the origin (numbers: saturating at `i64::MAX`).
fn distance(o: &Origin, v: i64) -> f64 {
    match *o {
        Origin::Long { origin, .. } => {
            let d = v.max(origin).wrapping_sub(v.min(origin));
            (if d < 0 { i64::MAX } else { d }) as f64
        }
        Origin::Geo { lat, lon, .. } => {
            let (a, b) = decode_geo_dv(v);
            haversin_meters(lat, lon, a, b)
        }
    }
}

fn pivot_of(o: &Origin) -> f64 {
    match *o {
        Origin::Long { pivot, .. } => pivot as f64,
        Origin::Geo { pivot, .. } => pivot,
    }
}

#[inline]
fn score_of(boost: f32, pivot: f64, distance: f64) -> f32 {
    (boost as f64 * (pivot / (pivot + distance))) as f32
}

struct DistanceWeight {
    q: DistanceFeatureQuery,
    boost: f32,
    field_name: String,
}

struct DistanceSupplier {
    scorer: DistanceScorer,
}

impl ScorerSupplier for DistanceSupplier {
    fn cost(&self) -> u64 {
        self.scorer.dv_cost
    }
    fn get(mut self: Box<Self>, lead_cost: u64) -> Result<Box<dyn Scorer>> {
        self.scorer.lead_cost = lead_cost;
        Ok(Box::new(self.scorer))
    }
}

impl DistanceWeight {
    fn scorer_impl(&self, seg: &SegmentContext) -> Option<DistanceScorer> {
        // No points in the segment: no data (Lucene).
        seg.reader.points(self.q.field)?;
        let values = Values::of(&seg.reader, self.q.field)?;
        let cursor = values.cursor();
        let dv_cost = match &values {
            Values::Sorted(v) => v.docs().len() as u64,
            Values::Single(v) => v.docs().len() as u64,
        };
        let max_distance = match self.q.origin {
            Origin::Long { .. } => i64::MAX as f64,
            Origin::Geo { .. } => EARTH_MEAN_RADIUS_METERS * std::f64::consts::PI,
        };
        Some(DistanceScorer {
            origin: self.q.origin,
            pivot: pivot_of(&self.q.origin),
            boost: self.boost,
            field: self.q.field,
            reader: seg.reader.clone(),
            values,
            it: It::Values(cursor),
            doc: u32::MAX,
            dv_cost,
            lead_cost: u64::MAX,
            max_distance,
            calls: 0,
            skip_interval: MIN_SKIP_INTERVAL,
            fails: 0,
        })
    }
}

impl Weight for DistanceWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        Ok(self.scorer_impl(seg).map(|s| Box::new(s) as Box<dyn Scorer>))
    }

    fn scorer_supplier(&self, seg: &SegmentContext) -> Result<Option<Box<dyn ScorerSupplier>>> {
        Ok(self.scorer_impl(seg).map(|scorer| Box::new(DistanceSupplier { scorer }) as Box<dyn ScorerSupplier>))
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        let o = self.q.origin;
        let value = Values::of(&seg.reader, self.q.field).and_then(|v| v.select(&o, doc, None));
        let Some(v) = value else {
            return Ok(Explanation::no_match(format!("Document {doc} doesn't have a value for field {}", self.field_name), vec![]));
        };
        let d = distance(&o, v);
        let score = score_of(self.boost, pivot_of(&o), d);
        let w = Explanation::leaf(self.boost, "weight");
        Ok(match o {
            Origin::Long { origin, pivot } => Explanation::matched(
                score,
                "Distance score, computed as weight * pivotDistance / (pivotDistance + abs(value - origin)) from:",
                vec![w, Explanation::leaf(pivot, "pivotDistance"), Explanation::leaf(origin, "origin"), Explanation::leaf(v, "current value")],
            ),
            Origin::Geo { lat, lon, pivot } => {
                let (a, b) = decode_geo_dv(v);
                Explanation::matched(
                    score,
                    "Distance score, computed as weight * pivotDistance / (pivotDistance + abs(distance)) from:",
                    vec![
                        w,
                        Explanation::leaf(pivot, "pivotDistance"),
                        Explanation::leaf(lat, "originLat"),
                        Explanation::leaf(lon, "originLon"),
                        Explanation::leaf(a, "current lat"),
                        Explanation::leaf(b, "current lon"),
                        Explanation::leaf(d, "distance"),
                    ],
                )
            }
        })
    }
}

/// What the scorer iterates: the docs with values, then possibly a pruned set.
enum It {
    Values(DocSetCursor),
    Set(SetScorer),
    Empty,
}

struct DistanceScorer {
    origin: Origin,
    pivot: f64,
    boost: f32,
    field: u32,
    reader: SegmentReader,
    values: Values,
    it: It,
    doc: u32,
    dv_cost: u64,
    lead_cost: u64,
    /// Largest distance that can still compete.
    max_distance: f64,
    calls: u32,
    skip_interval: u32,
    fails: u32,
}

impl DistanceScorer {
    fn score_at(&self, d: f64) -> f32 {
        score_of(self.boost, self.pivot, d)
    }

    /// Lucene's `computeMaxDistance`: the largest distance scoring at least `min` (bisection;
    /// integral for numbers).
    fn compute_max_distance(&self, min: f32, prev: f64) -> f64 {
        if self.score_at(prev) >= min {
            return prev;
        }
        match self.origin {
            Origin::Long { .. } => {
                let (mut lo, mut hi) = (0i64, prev as i64);
                while hi - lo > 1 {
                    let mid = ((lo as u64 + hi as u64) >> 1) as i64;
                    if self.score_at(mid as f64) >= min {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                lo as f64
            }
            Origin::Geo { .. } => {
                let (mut lo, mut hi) = (0f64, prev);
                while hi - lo > 1.0 {
                    let mid = (lo + hi) / 2.0;
                    if self.score_at(mid) >= min {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                lo
            }
        }
    }

    fn it_cost(&self) -> u64 {
        match &self.it {
            It::Values(_) => self.dv_cost,
            It::Set(s) => s.cost(),
            It::Empty => 0,
        }
    }

    fn update_skip_interval(&mut self, success: bool) {
        if self.calls > 256 {
            if success {
                self.skip_interval = (self.skip_interval / 2).max(MIN_SKIP_INTERVAL);
                self.fails = 0;
            } else if self.fails >= 3 {
                self.skip_interval = (self.skip_interval * 2).min(MAX_SKIP_INTERVAL);
                self.fails = 0;
            } else {
                self.fails += 1;
            }
        }
    }
}

impl Scorable for DistanceScorer {
    fn score(&mut self) -> f32 {
        let index = match &self.it {
            It::Values(c) => Some(c.index()),
            _ => None,
        };
        match self.values.select(&self.origin, self.doc, index) {
            Some(v) => self.score_at(distance(&self.origin, v)),
            None => 0.0,
        }
    }

    fn set_min_competitive_score(&mut self, min: f32) {
        if min > self.boost {
            self.it = It::Empty;
            return;
        }
        self.calls += 1;
        let sampled_out = match self.origin {
            Origin::Long { .. } => self.calls > 256 && (self.calls & (self.skip_interval - 1)) != self.skip_interval - 1,
            Origin::Geo { .. } => self.calls > 256 && (self.calls & 0x1f) != 0x1f,
        };
        if sampled_out {
            return;
        }
        let prev = self.max_distance;
        self.max_distance = self.compute_max_distance(min, prev);
        if self.max_distance == prev {
            return;
        }
        let Some(points) = self.reader.points(self.field) else { return };
        let after = self.doc;
        let mut v = match self.origin {
            Origin::Long { origin, .. } => {
                let md = self.max_distance as i64;
                let lo = origin.checked_sub(md).unwrap_or(i64::MIN);
                let hi = origin.checked_add(md).unwrap_or(i64::MAX);
                Box::new(LongBox { lo, hi, after, b: DocIdSetBuilder::new(self.reader.max_doc()) }) as Box<dyn BoxVisitor>
            }
            Origin::Geo { lat, lon, .. } => {
                let (min_lat, max_lat, min_lon, max_lon) = bbox_of_circle(lat, lon, self.max_distance);
                Box::new(GeoBox {
                    min_lat: encode_latitude(min_lat),
                    max_lat: encode_latitude(max_lat),
                    min_lon: encode_longitude(min_lon),
                    max_lon: encode_longitude(max_lon),
                    dateline: min_lon > max_lon,
                    after,
                    b: DocIdSetBuilder::new(self.reader.max_doc()),
                })
            }
        };
        let threshold = self.lead_cost.min(self.it_cost()) >> 3;
        if points.estimate_point_count(v.as_visitor()) >= threshold {
            // Not selective enough to be worth materializing.
            self.update_skip_interval(false);
            return;
        }
        points.intersect(v.as_visitor());
        // The set holds only docs after the current one; `doc()` stays until the next move.
        self.it = It::Set(SetScorer::new(Arc::new(v.build()), 0.0));
        self.update_skip_interval(true);
    }
}

impl Scorer for DistanceScorer {
    fn doc(&self) -> u32 {
        self.doc
    }

    fn next(&mut self) -> u32 {
        self.doc = match &mut self.it {
            It::Values(c) => c.next(),
            It::Set(s) => s.next(),
            It::Empty => NO_MORE_DOCS,
        };
        self.doc
    }

    fn advance(&mut self, t: u32) -> u32 {
        self.doc = match &mut self.it {
            It::Values(c) => c.advance(t),
            It::Set(s) => s.advance(t),
            It::Empty => NO_MORE_DOCS,
        };
        self.doc
    }

    fn cost(&self) -> u64 {
        self.it_cost()
    }

    fn max_score(&mut self, _upto: u32) -> f32 {
        self.boost
    }
}

/// A point visitor collecting the docs after the current one inside a box.
trait BoxVisitor {
    fn as_visitor(&mut self) -> &mut dyn PointVisitor;
    fn build(self: Box<Self>) -> crate::util::bitset::DocIdSet;
}

struct LongBox {
    lo: i64,
    hi: i64,
    after: u32,
    b: DocIdSetBuilder,
}

impl LongBox {
    fn later(&self, doc: u32) -> bool {
        self.after == u32::MAX || doc > self.after
    }
}

impl PointVisitor for LongBox {
    fn compare(&mut self, min: &[u8], max: &[u8]) -> Relation {
        let (a, b) = (i64_from(&min[..8]), i64_from(&max[..8]));
        if a > self.hi || b < self.lo {
            Relation::CellOutsideQuery
        } else if a < self.lo || b > self.hi {
            Relation::CellCrossesQuery
        } else {
            Relation::CellInsideQuery
        }
    }
    fn visit(&mut self, doc: u32) {
        if self.later(doc) {
            self.b.add(doc);
        }
    }
    fn visit_value(&mut self, doc: u32, packed: &[u8]) {
        let v = i64_from(&packed[..8]);
        if self.later(doc) && v >= self.lo && v <= self.hi {
            self.b.add(doc);
        }
    }
    fn grow(&mut self, n: usize) {
        self.b.grow(n);
    }
}

impl BoxVisitor for LongBox {
    fn as_visitor(&mut self) -> &mut dyn PointVisitor {
        self
    }
    fn build(self: Box<Self>) -> crate::util::bitset::DocIdSet {
        self.b.build()
    }
}

struct GeoBox {
    min_lat: i32,
    max_lat: i32,
    min_lon: i32,
    max_lon: i32,
    dateline: bool,
    after: u32,
    b: DocIdSetBuilder,
}

impl GeoBox {
    fn lon_out(&self, lon: i32) -> bool {
        if self.dateline { lon < self.min_lon && lon > self.max_lon } else { lon > self.max_lon || lon < self.min_lon }
    }
}

impl PointVisitor for GeoBox {
    /// Lucene's comparison, conservative across the dateline.
    fn compare(&mut self, min: &[u8], max: &[u8]) -> Relation {
        let (lat_lo, lat_hi) = (i32_from(&min[..4]), i32_from(&max[..4]));
        if lat_lo > self.max_lat || lat_hi < self.min_lat {
            return Relation::CellOutsideQuery;
        }
        let mut crosses = lat_lo < self.min_lat || lat_hi > self.max_lat;
        let (lon_lo, lon_hi) = (i32_from(&min[4..8]), i32_from(&max[4..8]));
        if self.dateline {
            if lon_lo > self.max_lon && lon_hi < self.min_lon {
                return Relation::CellOutsideQuery;
            }
            crosses |= lon_lo < self.max_lon || lon_hi > self.min_lon;
        } else {
            if lon_lo > self.max_lon || lon_hi < self.min_lon {
                return Relation::CellOutsideQuery;
            }
            crosses |= lon_lo < self.min_lon || lon_hi > self.max_lon;
        }
        if crosses { Relation::CellCrossesQuery } else { Relation::CellInsideQuery }
    }
    fn visit(&mut self, doc: u32) {
        if self.after == u32::MAX || doc > self.after {
            self.b.add(doc);
        }
    }
    fn visit_value(&mut self, doc: u32, packed: &[u8]) {
        if !(self.after == u32::MAX || doc > self.after) {
            return;
        }
        let lat = i32_from(&packed[..4]);
        if lat > self.max_lat || lat < self.min_lat || self.lon_out(i32_from(&packed[4..8])) {
            return;
        }
        self.b.add(doc);
    }
    fn grow(&mut self, n: usize) {
        self.b.grow(n);
    }
}

impl BoxVisitor for GeoBox {
    fn as_visitor(&mut self) -> &mut dyn PointVisitor {
        self
    }
    fn build(self: Box<Self>) -> crate::util::bitset::DocIdSet {
        self.b.build()
    }
}

impl Query for DistanceFeatureQuery {
    fn weight(&self, s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        Ok(Box::new(DistanceWeight { q: self.clone(), boost, field_name: s.schema().name(self.field) }))
    }

    /// Lucene prints the `toString` argument (empty) where the field name would go.
    fn lucene_string(&self, _schema: &Schema) -> String {
        match self.origin {
            Origin::Long { origin, pivot } => format!("LongDistanceFeatureQuery(field=,origin={origin},pivotDistance={pivot})"),
            Origin::Geo { lat, lon, pivot } => format!(
                "LatLonPointDistanceFeatureQuery(field=,originLat={},originLon={},pivotDistance={})",
                double_to_string(lat),
                double_to_string(lon),
                double_to_string(pivot)
            ),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances_and_scores() {
        let o = Origin::Long { origin: 10, pivot: 1 };
        assert_eq!(distance(&o, i64::MIN), i64::MAX as f64);
        assert_eq!(distance(&o, 7), 3.0);
        assert_eq!(score_of(2.0, 5.0, 5.0), 1.0);
    }

    #[test]
    fn validates_like_lucene() {
        assert_eq!(DistanceFeatureQuery::long(1, 1.0, 0, 0).unwrap_err().to_string(), "pivotDistance must be > 0, got 0");
        assert_eq!(DistanceFeatureQuery::geo(1, 1.0, 0.0, 0.0, -1.0).unwrap_err().to_string(), "pivotDistance must be > 0, got -1.0");
        assert_eq!(DistanceFeatureQuery::geo(1, 1.0, 91.0, 0.0, 1.0).unwrap_err().to_string(), "invalid latitude 91.0; must be between -90.0 and 90.0");
    }
}
