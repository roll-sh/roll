//! `distance_feature` on `date`, `date_nanos` and `geo_point` fields that have doc values but
//! no points (`index: false`). Lucene's distance feature queries need the BKD tree, so
//! Elasticsearch falls back to `LongScriptFieldDistanceFeatureQuery` /
//! `GeoPointScriptFieldDistanceFeatureQuery` over the doc values; this is the same scoring:
//! `boost · pivot / (pivot + distance)` from the value closest to the origin, every doc with a
//! value matching. There is no competitive-score skipping (no index to skip with).

use std::any::Any;
use std::sync::Arc;

use search_codec::{DocValuesReader, NO_MORE_DOCS, SortedNumericDocValues};
use search_index::query::distance_feature::Origin;
use search_index::search::{Explanation, Query, QueryRef, Scorable, ScoreMode, Scorer, Searcher, SegmentContext, Weight};
use search_index::util::geo::{decode_geo_dv, haversin_meters};
use search_index::util::java::double_to_string;
use search_index::{Result, Schema};

/// Distance feature scoring from sorted-numeric doc values.
#[derive(Clone, Debug, PartialEq)]
pub struct DocValuesDistanceFeatureQuery {
    pub field: u32,
    pub origin: Origin,
}

impl DocValuesDistanceFeatureQuery {
    pub fn query(field: u32, origin: Origin) -> QueryRef {
        Arc::new(DocValuesDistanceFeatureQuery { field, origin })
    }
}

fn long_distance(origin: i64, v: i64) -> i64 {
    let d = v.max(origin).wrapping_sub(v.min(origin));
    // Doesn't fit a signed long: clamped, as Elasticsearch does.
    if d < 0 { i64::MAX } else { d }
}

fn geo_distance(lat: f64, lon: f64, v: i64) -> f64 {
    let (a, b) = decode_geo_dv(v);
    haversin_meters(lat, lon, a, b)
}

/// The doc's smallest distance and the value it came from.
fn closest(o: &Origin, values: impl Iterator<Item = i64>) -> Option<(f64, i64)> {
    let mut best: Option<(f64, i64)> = None;
    for v in values {
        let d = match *o {
            Origin::Long { origin, .. } => long_distance(origin, v) as f64,
            Origin::Geo { lat, lon, .. } => geo_distance(lat, lon, v),
        };
        if best.is_none_or(|(b, _)| d < b) {
            best = Some((d, v));
        }
    }
    best
}

fn pivot(o: &Origin) -> f64 {
    match *o {
        Origin::Long { pivot, .. } => pivot as f64,
        Origin::Geo { pivot, .. } => pivot,
    }
}

fn score(boost: f32, pivot: f64, distance: f64) -> f32 {
    (boost as f64 * (pivot / (pivot + distance))) as f32
}

struct DvDistanceScorer {
    values: SortedNumericDocValues,
    docs: search_codec::DocSetCursor,
    origin: Origin,
    pivot: f64,
    boost: f32,
    cost: u64,
    doc: u32,
}

impl Scorable for DvDistanceScorer {
    fn score(&mut self) -> f32 {
        let d = closest(&self.origin, self.values.values_at(self.docs.index())).map_or(f64::INFINITY, |(d, _)| d);
        score(self.boost, self.pivot, d)
    }
}

impl Scorer for DvDistanceScorer {
    fn doc(&self) -> u32 {
        self.doc
    }
    fn next(&mut self) -> u32 {
        self.doc = self.docs.next();
        self.doc
    }
    fn advance(&mut self, t: u32) -> u32 {
        self.doc = if t == NO_MORE_DOCS { NO_MORE_DOCS } else { self.docs.advance(t) };
        self.doc
    }
    fn cost(&self) -> u64 {
        self.cost
    }
    fn max_score(&mut self, _upto: u32) -> f32 {
        self.boost
    }
}

struct DvDistanceWeight {
    q: DocValuesDistanceFeatureQuery,
    boost: f32,
}

impl DvDistanceWeight {
    fn values(&self, seg: &SegmentContext) -> Option<SortedNumericDocValues> {
        match seg.reader.doc_values(self.q.field)? {
            DocValuesReader::SortedNumeric(v) => Some(v.clone()),
            _ => None,
        }
    }
}

impl Weight for DvDistanceWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let Some(values) = self.values(seg) else { return Ok(None) };
        let docs = values.docs().cursor();
        Ok(Some(Box::new(DvDistanceScorer {
            cost: values.docs().len() as u64,
            values,
            docs,
            origin: self.q.origin,
            pivot: pivot(&self.q.origin),
            boost: self.boost,
            doc: u32::MAX,
        })))
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        let found = self.values(seg).and_then(|v| closest(&self.q.origin, v.values(doc)));
        let Some((d, v)) = found else {
            return Ok(Explanation::no_match(format!("Document {doc} doesn't have a value for field"), vec![]));
        };
        let s = score(self.boost, pivot(&self.q.origin), d);
        Ok(match self.q.origin {
            Origin::Long { origin, pivot } => Explanation::matched(
                s,
                "Distance score, computed as weight * pivot / (pivot + abs(value - origin)) from:",
                vec![
                    Explanation::leaf(self.boost, "weight"),
                    Explanation::leaf(pivot, "pivot"),
                    Explanation::leaf(origin, "origin"),
                    Explanation::leaf(v, "current value"),
                ],
            ),
            Origin::Geo { lat, lon, pivot } => {
                let (a, b) = decode_geo_dv(v);
                Explanation::matched(
                    s,
                    "Distance score, computed as weight * pivotDistance / (pivotDistance + abs(distance)) from:",
                    vec![
                        Explanation::leaf(self.boost, "weight"),
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

impl Query for DocValuesDistanceFeatureQuery {
    fn weight(&self, _s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        Ok(Box::new(DvDistanceWeight { q: self.clone(), boost }))
    }
    fn lucene_string(&self, schema: &Schema) -> String {
        let f = schema.name(self.field);
        match self.origin {
            Origin::Long { origin, pivot } => format!("{f}:LongScriptFieldDistanceFeatureQuery(origin={origin},pivot={pivot})"),
            Origin::Geo { lat, lon, pivot } => format!(
                "{f}:GeoPointScriptFieldDistanceFeatureQuery(lat={},lon={},pivot={})",
                double_to_string(lat),
                double_to_string(lon),
                double_to_string(pivot)
            ),
        }
    }
    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        h.write(format!("dv_distance_feature{self:?}").as_bytes());
        true
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
