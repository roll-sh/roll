//! `_script` and `_geo_distance` keys of `top_hits` and `top_metrics` sorts.
//!
//! Aggregations sort their documents themselves ([`search_aggs::sort`]), comparing values of
//! any type directly, so these keys are just per-document computations
//! ([`search_aggs::field::ComputedSort`]): a `number` script's double, a `string` script's
//! text, a `version` script's encoded version, or a document's distance to the sort's origins
//! (by `mode`, `Infinity` without a point, as `GeoDistanceSortBuilder` does). Script failures are
//! deferred to the shard, carrying the whole script error.

use std::sync::Arc;

use search_aggs::field::{ComputedSort, SegmentComputedSort};
use search_aggs::json::Json;
use search_aggs::sort::SortVal;
use search_index::SegmentContext;
use search_index::collector::top_field::{GeoDistanceSort, GeoDistanceType, SortMode};

use super::scripts::{Leaf, Painless};

/// A `_script` sort key.
pub struct ScriptKey {
    pub painless: Painless,
    /// `number`, `string` or `version`.
    pub kind: &'static str,
}

impl std::fmt::Debug for ScriptKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ScriptKey({})", self.kind)
    }
}

fn defer(e: crate::error::EsError) {
    let reason = format!("{}{}", e.reason(), super::scripts::park_script_error(e.clone()));
    search_index::search::defer_error(search_index::Error::Es { kind: std::borrow::Cow::Owned(e.kind.to_string()), reason, status: e.status });
}

impl ComputedSort for ScriptKey {
    fn kind(&self) -> &str {
        self.kind
    }

    fn segment(&self, seg: &SegmentContext) -> search_aggs::Result<Box<dyn SegmentComputedSort>> {
        let host = self.painless.host(seg);
        let leaf = Leaf::from_bound(&self.painless.bound, host).ok_or_else(|| search_aggs::AggError::illegal_argument("sort scripts must be painless"))?;
        Ok(Box::new(ScriptLeaf { leaf, kind: self.kind }))
    }

    fn render_bytes(&self, b: &[u8]) -> Json {
        Json::Str(if self.kind == "version" { super::version_field::decode(b) } else { String::from_utf8_lossy(b).into_owned() })
    }

    fn needs_score(&self) -> bool {
        true
    }
}

struct ScriptLeaf {
    leaf: Leaf,
    kind: &'static str,
}

impl SegmentComputedSort for ScriptLeaf {
    fn value(&mut self, doc: u32, score: f32) -> SortVal {
        let score = if score.is_nan() { 0.0 } else { f64::from(score) };
        if self.kind == "number" {
            return match self.leaf.run_f64(doc, score) {
                Ok(v) => SortVal::Double(v),
                Err(e) => {
                    defer(crate::scripting::script_error(e));
                    SortVal::Null
                }
            };
        }
        match self.leaf.run(doc, score) {
            Ok(v) if v.is_null() => SortVal::Null,
            Ok(v) => {
                let s = v.to_java_string();
                if self.kind == "version" {
                    match super::version_field::encode(&s) {
                        Ok(b) => SortVal::Bytes(b),
                        Err(e) => {
                            defer(e);
                            SortVal::Null
                        }
                    }
                } else {
                    SortVal::Bytes(s.into_bytes())
                }
            }
            Err(e) => {
                defer(crate::scripting::script_error(e));
                SortVal::Null
            }
        }
    }
}

/// A `_geo_distance` sort key.
#[derive(Debug)]
pub struct GeoKey(pub GeoDistanceSort);

impl ComputedSort for GeoKey {
    fn kind(&self) -> &str {
        "number"
    }

    fn segment(&self, seg: &SegmentContext) -> search_aggs::Result<Box<dyn SegmentComputedSort>> {
        Ok(Box::new(GeoLeaf { dv: seg.reader.sorted_numeric(self.0.field).cloned(), sort: Arc::new(self.0.clone()) }))
    }
}

struct GeoLeaf {
    dv: Option<search_codec::SortedNumericDocValues>,
    sort: Arc<GeoDistanceSort>,
}

impl SegmentComputedSort for GeoLeaf {
    fn value(&mut self, doc: u32, _score: f32) -> SortVal {
        let g = &self.sort;
        let mode = g.mode.unwrap_or(if g.reverse { SortMode::Max } else { SortMode::Min });
        let mut picked: Option<f64> = None;
        let (mut sum, mut n) = (0.0f64, 0usize);
        if let Some(dv) = &self.dv {
            for v in dv.values(doc) {
                let (lat, lon) = search_index::util::geo::decode_geo_dv(v);
                for &(olat, olon) in &g.points {
                    let m = match g.distance_type {
                        GeoDistanceType::Arc => search_index::util::geo::haversin_meters(olat, olon, lat, lon),
                        GeoDistanceType::Plane => search_index::util::geo::plane_meters(olat, olon, lat, lon),
                    };
                    let d = if g.unit_meters == 1.0 { m } else { m / g.unit_meters };
                    sum += d;
                    n += 1;
                    picked = Some(match (picked, mode) {
                        (None, _) => d,
                        (Some(p), SortMode::Max) => p.max(d),
                        (Some(p), _) => p.min(d),
                    });
                }
            }
        }
        match (picked, mode) {
            (None, _) => SortVal::Double(f64::INFINITY),
            (Some(_), SortMode::Avg) => SortVal::Double(sum / n as f64),
            (Some(_), SortMode::Sum) => SortVal::Double(sum),
            (Some(p), _) => SortVal::Double(p),
        }
    }
}
