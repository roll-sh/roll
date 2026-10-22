//! Feature queries (Lucene's `FeatureField` / `FeatureQuery`): Elasticsearch's `rank_feature`
//! query and the token weights of `sparse_vector`.
//!
//! A feature is a term (the feature name) of a field indexed with frequencies and no norms,
//! whose term frequency encodes a positive float: the top 16 bits of its IEEE representation
//! (`bits >>> 15`, so 9 significant mantissa bits). Index one with [`feature_token`]. The
//! query scores a doc's feature value `S` with one of Lucene's functions — linear `w·S`,
//! logarithm `w·ln(a + S)`, saturation `w·S/(S + k)`, sigmoid `w·S^a/(S^a + k^a)` — where the
//! weight `w` is the query boost. Each function grows with the frequency, so the postings'
//! block-max impacts bound scores and top-k searches skip blocks like term queries do (the
//! scorer *is* the term scorer, with the function as its similarity). Saturation without a
//! pivot uses the feature's mean value over the index, computed on rewrite.

use std::any::Any;
use std::sync::Arc;

use bytes::Bytes;

use crate::doc::PreToken;
use crate::error::{Error, Result};
use crate::query::basic::BoostQuery;
use crate::query::term::{Norms, TermQuery, TermScorer, TermStates, term_text};
use crate::schema::Schema;
use crate::search::{Explanation, Query, QueryRef, QueryVisitor, ScoreMode, Scorer, Searcher, SegmentContext, Weight};
use crate::similarity::SimScorer;
use crate::util::java::float_to_string;
use search_codec::PostingsFlags;

/// Lucene's `FeatureField.MAX_FREQ`: the frequency of `Float.MAX_VALUE`.
const MAX_FREQ: u32 = f32::MAX.to_bits() >> 15;

/// Lucene's `FeatureField.MAX_WEIGHT` (`Long.SIZE`).
const MAX_WEIGHT: f32 = 64.0;

/// The token that indexes feature `name` with `value` (Lucene `FeatureField`): the value must
/// be a finite positive normal float.
pub fn feature_token(field_name: &str, name: impl Into<Bytes>, value: f32) -> Result<PreToken> {
    let name = name.into();
    if !value.is_finite() {
        return Err(Error::illegal(format!(
            "featureValue must be finite, got: {} for feature {} on field {field_name}",
            float_to_string(value),
            term_text(&name)
        )));
    }
    if value < f32::MIN_POSITIVE {
        return Err(Error::illegal(format!(
            "featureValue must be a positive normal float, got: {} for feature {} on field {field_name} which is less than the minimum positive normal float: {}",
            float_to_string(value),
            term_text(&name),
            float_to_string(f32::MIN_POSITIVE)
        )));
    }
    let mut t = PreToken::new(name, 1, 0, 0);
    t.freq = value.to_bits() >> 15;
    Ok(t)
}

/// Lucene's `FeatureField.decodeFeatureValue`.
pub fn decode_feature_value(freq: f32) -> f32 {
    if freq > MAX_FREQ as f32 {
        // Max-score computations pass huge frequencies.
        return f32::MAX;
    }
    f32::from_bits((freq as u32) << 15)
}

/// How a feature value becomes a score.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FeatureFunction {
    /// `w · S`
    Linear,
    /// `w · ln(scaling_factor + S)`
    Log { scaling_factor: f32 },
    /// `w · S / (S + pivot)`; `None`: the feature's mean value (resolved on rewrite).
    Saturation { pivot: Option<f32> },
    /// `w · S^exp / (S^exp + pivot^exp)`
    Sigmoid { pivot: f32, exp: f32 },
}

impl FeatureFunction {
    /// Lucene's `toString`.
    fn describe(&self) -> String {
        match self {
            FeatureFunction::Linear => "LinearFunction".into(),
            FeatureFunction::Log { scaling_factor } => format!("LogFunction(scalingFactor={})", float_to_string(*scaling_factor)),
            FeatureFunction::Saturation { pivot } => {
                format!("SaturationFunction(pivot={})", pivot.map_or_else(|| "null".into(), float_to_string))
            }
            FeatureFunction::Sigmoid { pivot, exp } => {
                format!("SigmoidFunction(pivot={}, a={})", float_to_string(*pivot), float_to_string(*exp))
            }
        }
    }
}

/// A resolved feature function with its weight (the similarity of a feature query's scorer).
#[derive(Clone, Debug)]
pub struct FeatureSim {
    func: FeatureFunction,
    weight: f32,
    /// `pivot^exp` (sigmoid), in double as Lucene keeps it.
    pivot_pa: f64,
}

impl FeatureSim {
    fn new(func: FeatureFunction, weight: f32) -> FeatureSim {
        let pivot_pa = match func {
            FeatureFunction::Sigmoid { pivot, exp } => (pivot as f64).powf(exp as f64),
            _ => 0.0,
        };
        FeatureSim { func, weight, pivot_pa }
    }

    /// The score of a doc whose feature frequency is `freq`.
    #[inline]
    pub fn score(&self, freq: f32) -> f32 {
        let f = decode_feature_value(freq);
        let w = self.weight;
        match self.func {
            FeatureFunction::Linear => w * f,
            FeatureFunction::Log { scaling_factor } => (w as f64 * ((scaling_factor + f) as f64).ln()) as f32,
            FeatureFunction::Saturation { pivot } => {
                let k = pivot.unwrap_or(1.0);
                // `1 - k / (S + k)` rather than `S / (S + k)`: never decreases with S despite
                // rounding.
                w * (1f32 - k / (f + k))
            }
            FeatureFunction::Sigmoid { exp, .. } => (w as f64 * (1.0 - self.pivot_pa / ((f as f64).powf(exp as f64) + self.pivot_pa))) as f32,
        }
    }

    /// Lucene's `FeatureFunction.explain`.
    fn explain(&self, field: &str, feature: &str, freq: u32) -> Explanation {
        let s = decode_feature_value(freq as f32);
        let score = self.score(freq as f32);
        let w = Explanation::leaf(self.weight, "w, weight of this function");
        let sv = Explanation::leaf(s, "S, feature value");
        let pivot_desc = "k, pivot feature value that would give a score contribution equal to w/2";
        match self.func {
            FeatureFunction::Linear => Explanation::matched(
                score,
                format!("Linear function on the {field} field for the {feature} feature, computed as w * S from:"),
                vec![w, sv],
            ),
            FeatureFunction::Log { scaling_factor } => Explanation::matched(
                score,
                format!("Log function on the {field} field for the {feature} feature, computed as w * log(a + S) from:"),
                vec![w, Explanation::leaf(scaling_factor, "a, scaling factor"), sv],
            ),
            FeatureFunction::Saturation { pivot } => Explanation::matched(
                score,
                format!("Saturation function on the {field} field for the {feature} feature, computed as w * S / (S + k) from:"),
                vec![w, Explanation::leaf(pivot.unwrap_or(1.0), pivot_desc), sv],
            ),
            FeatureFunction::Sigmoid { pivot, exp } => Explanation::matched(
                score,
                format!("Sigmoid function on the {field} field for the {feature} feature, computed as w * S^a / (S^a + k^a) from:"),
                vec![
                    w,
                    Explanation::leaf(pivot, pivot_desc),
                    Explanation::leaf(exp, "a, exponent, higher values make the function grow slower before k and faster after k"),
                    sv,
                ],
            ),
        }
    }
}

/// Scores the docs having a feature by its value.
#[derive(Clone, Debug, PartialEq)]
pub struct FeatureQuery {
    pub field: u32,
    pub feature: Vec<u8>,
    pub function: FeatureFunction,
}

fn check_weight(weight: f32) -> Result<()> {
    if weight <= 0.0 || weight > MAX_WEIGHT || weight.is_nan() {
        return Err(Error::illegal(format!("weight must be in (0, {}], got: {}", float_to_string(MAX_WEIGHT), float_to_string(weight))));
    }
    Ok(())
}

fn boosted(q: FeatureQuery, weight: f32) -> QueryRef {
    let q: QueryRef = Arc::new(q);
    if weight != 1.0 { Arc::new(BoostQuery::new(q, weight)) } else { q }
}

impl FeatureQuery {
    /// Lucene's `FeatureField.newLinearQuery`.
    pub fn linear(field: u32, feature: impl Into<Vec<u8>>, weight: f32) -> Result<QueryRef> {
        check_weight(weight)?;
        Ok(boosted(FeatureQuery { field, feature: feature.into(), function: FeatureFunction::Linear }, weight))
    }

    /// Lucene's `FeatureField.newLogQuery`.
    pub fn log(field: u32, feature: impl Into<Vec<u8>>, weight: f32, scaling_factor: f32) -> Result<QueryRef> {
        check_weight(weight)?;
        if scaling_factor < 1.0 || !scaling_factor.is_finite() {
            return Err(Error::illegal(format!("scalingFactor must be >= 1, got: {}", float_to_string(scaling_factor))));
        }
        Ok(boosted(FeatureQuery { field, feature: feature.into(), function: FeatureFunction::Log { scaling_factor } }, weight))
    }

    /// Lucene's `FeatureField.newSaturationQuery` (`pivot: None`: the feature's mean value).
    pub fn saturation(field: u32, feature: impl Into<Vec<u8>>, weight: f32, pivot: Option<f32>) -> Result<QueryRef> {
        check_weight(weight)?;
        if let Some(p) = pivot
            && (p <= 0.0 || !p.is_finite())
        {
            return Err(Error::illegal(format!("pivot must be > 0, got: {}", float_to_string(p))));
        }
        Ok(boosted(FeatureQuery { field, feature: feature.into(), function: FeatureFunction::Saturation { pivot } }, weight))
    }

    /// Lucene's `FeatureField.newSigmoidQuery`.
    pub fn sigmoid(field: u32, feature: impl Into<Vec<u8>>, weight: f32, pivot: f32, exp: f32) -> Result<QueryRef> {
        check_weight(weight)?;
        if pivot <= 0.0 || !pivot.is_finite() {
            return Err(Error::illegal(format!("pivot must be > 0, got: {}", float_to_string(pivot))));
        }
        if exp <= 0.0 || !exp.is_finite() {
            return Err(Error::illegal(format!("exp must be > 0, got: {}", float_to_string(exp))));
        }
        Ok(boosted(FeatureQuery { field, feature: feature.into(), function: FeatureFunction::Sigmoid { pivot, exp } }, weight))
    }
}

/// Lucene's `FeatureField.computePivotFeatureValue`: the decoded mean frequency.
pub fn pivot_feature_value(s: &Searcher, field: u32, feature: &[u8]) -> f32 {
    let ts = TermStates::build(s, field, feature);
    if ts.doc_freq == 0 {
        // Never used for scoring: any legal value.
        return 1.0;
    }
    decode_feature_value((ts.total_term_freq as f64 / ts.doc_freq as f64) as f32)
}

struct FeatureWeight {
    field: u32,
    field_name: String,
    feature: Vec<u8>,
    desc: String,
    function: String,
    states: TermStates,
    sim: Arc<SimScorer>,
    fsim: FeatureSim,
    mode: ScoreMode,
}

impl FeatureWeight {
    fn scorer_impl(&self, seg: &SegmentContext) -> Option<TermScorer> {
        let st = self.states.get(seg)?;
        let terms = seg.reader.terms(self.field)?;
        Some(TermScorer::new(terms.postings(st, PostingsFlags::FREQS), Norms::None, self.sim.clone(), self.mode))
    }
}

impl Weight for FeatureWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        seg.ensure_postings(self.field)?;
        Ok(self.scorer_impl(seg).map(|s| Box::new(s) as Box<dyn Scorer>))
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        let desc = format!("weight({} in {doc}) [{}]", self.desc, self.function);
        if seg.reader.terms(self.field).is_none() {
            return Ok(Explanation::no_match(format!("{desc}. Field {} doesn't exist.", self.field_name), vec![]));
        }
        let feature = term_text(&self.feature);
        seg.ensure_postings(self.field)?;
        let Some(mut s) = self.scorer_impl(seg) else {
            return Ok(Explanation::no_match(format!("{desc}. Feature {feature} doesn't exist."), vec![]));
        };
        if s.advance(doc) != doc {
            return Ok(Explanation::no_match(format!("{desc}. Feature {feature} isn't set."), vec![]));
        }
        Ok(self.fsim.explain(&self.field_name, &feature, s.freq_u32()))
    }
}

impl Query for FeatureQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        if !mode.needs_scores() {
            // Features are terms: let the term query serve filters and counts.
            return TermQuery::new(self.field, self.feature.clone()).weight(s, mode, boost);
        }
        let function = match self.function {
            FeatureFunction::Saturation { pivot: None } => {
                FeatureFunction::Saturation { pivot: Some(pivot_feature_value(s, self.field, &self.feature)) }
            }
            f => f,
        };
        let fsim = FeatureSim::new(function, boost);
        let q = FeatureQuery { function, ..self.clone() };
        Ok(Box::new(FeatureWeight {
            field: self.field,
            field_name: s.schema().name(self.field),
            feature: self.feature.clone(),
            desc: q.lucene_string(s.schema()),
            function: function.describe(),
            states: TermStates::build(s, self.field, &self.feature),
            sim: Arc::new(SimScorer::Feature(Box::new(fsim.clone()))),
            fsim,
            mode,
        }))
    }

    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        if let FeatureFunction::Saturation { pivot: None } = self.function {
            let pivot = pivot_feature_value(s, self.field, &self.feature);
            return Ok(Some(Arc::new(FeatureQuery { function: FeatureFunction::Saturation { pivot: Some(pivot) }, ..self.clone() })));
        }
        Ok(None)
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        format!("FeatureQuery(field={}, feature={}, function={})", schema.name(self.field), term_text(&self.feature), self.function.describe())
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        v.term(self.field, &self.feature);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_decodes_like_lucene() {
        let t = feature_token("_feature", "pagerank", 10.0).unwrap();
        assert_eq!(t.freq, 10f32.to_bits() >> 15);
        assert_eq!(decode_feature_value(t.freq as f32), 10.0);
        // 9 significant bits: 1.001 rounds down.
        let t = feature_token("_feature", "x", 1.001).unwrap();
        assert_eq!(decode_feature_value(t.freq as f32), 1.0);
        assert_eq!(decode_feature_value(f32::MAX), f32::MAX);
        assert_eq!(MAX_FREQ, 65279);
        assert_eq!(
            feature_token("_feature", "x", 0.0).unwrap_err().to_string(),
            "featureValue must be a positive normal float, got: 0.0 for feature x on field _feature which is less than the minimum positive normal float: 1.1754944E-38"
        );
        assert_eq!(feature_token("_feature", "x", f32::INFINITY).unwrap_err().to_string(), "featureValue must be finite, got: Infinity for feature x on field _feature");
    }

    #[test]
    fn functions_score_like_lucene() {
        let f = 4f32.to_bits() >> 15;
        let s = |func, w| FeatureSim::new(func, w).score(f as f32);
        assert_eq!(s(FeatureFunction::Linear, 2.0), 8.0);
        assert_eq!(s(FeatureFunction::Log { scaling_factor: 1.0 }, 1.0), (5f64.ln()) as f32);
        assert_eq!(s(FeatureFunction::Saturation { pivot: Some(4.0) }, 2.0), 1.0);
        assert_eq!(s(FeatureFunction::Sigmoid { pivot: 4.0, exp: 2.0 }, 1.0), 0.5);
        // Scores never decrease with the feature value (impacts depend on it).
        for func in [
            FeatureFunction::Linear,
            FeatureFunction::Log { scaling_factor: 3.0 },
            FeatureFunction::Saturation { pivot: Some(0.5) },
            FeatureFunction::Sigmoid { pivot: 7.0, exp: 0.6 },
        ] {
            let sim = FeatureSim::new(func, 1.5);
            let mut last = 0f32;
            for freq in (1..MAX_FREQ).step_by(97).chain([MAX_FREQ, u32::MAX]) {
                let v = sim.score(freq as f32);
                assert!(v >= last, "{func:?} at {freq}");
                last = v;
            }
        }
        assert_eq!(FeatureQuery::linear(3, "x", 65.0).unwrap_err().to_string(), "weight must be in (0, 64.0], got: 65.0");
        assert_eq!(FeatureQuery::log(3, "x", 1.0, 0.5).unwrap_err().to_string(), "scalingFactor must be >= 1, got: 0.5");
        assert_eq!(FeatureQuery::saturation(3, "x", 1.0, Some(-1.0)).unwrap_err().to_string(), "pivot must be > 0, got: -1.0");
        assert_eq!(FeatureQuery::sigmoid(3, "x", 1.0, 1.0, 0.0).unwrap_err().to_string(), "exp must be > 0, got: 0.0");
    }
}
