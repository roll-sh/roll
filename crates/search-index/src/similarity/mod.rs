//! Similarities: how a term's frequency in a doc and the collection statistics become a score.
//!
//! Every similarity Elasticsearch offers (`index.similarity.*` / a field's `similarity`) is
//! here with Lucene 10's arithmetic, float for float, and its explanation texts:
//!
//! - `BM25` — Elasticsearch's default is Lucene's `LegacyBM25Similarity`: plain BM25 whose
//!   query boost is multiplied by `k1 + 1` (so scores match Elasticsearch, `boost` 2.2 in
//!   explanations). Scoring uses a 256-entry cache of `1 / (k1 * (1 - b + b * dl / avgdl))` per
//!   norm byte and the monotonic form `w - w / (1 + freq * normInverse)`.
//! - `DFR`, `IB`, `DFI`, `LMDirichlet`, `LMJelinekMercer` — Lucene's `SimilarityBase` family,
//!   computed in doubles from `BasicStats` and cast to float.
//! - `boolean` — the query boost, nothing else.
//! - `scripted` — a callback trait ([`ScriptedSimilarity`]) that `search-es` backs with Painless.
//!
//! Norms: every similarity encodes field length with `SmallFloat.intToByte4`; with
//! `discount_overlaps` (default) stacked tokens don't count, and fields indexed with docs only
//! count unique terms (Lucene 8+).

use std::sync::Arc;

use search_codec::{byte4_to_int, int_to_byte4};

use crate::search::explain::Explanation;
use crate::util::java::float_to_string;

/// Statistics of a field across the searched segments (Lucene `CollectionStatistics`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CollectionStats {
    pub max_doc: u64,
    /// Docs with at least one term in the field.
    pub doc_count: u64,
    pub sum_total_term_freq: u64,
    pub sum_doc_freq: u64,
}

/// Statistics of one term (Lucene `TermStatistics`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermStats {
    pub doc_freq: u64,
    pub total_term_freq: u64,
}

/// What indexing measured of one field of one doc (Lucene `FieldInvertState`).
#[derive(Clone, Copy, Debug, Default)]
pub struct InvertState {
    /// Tokens (sum of term frequencies).
    pub length: u32,
    /// Tokens with a position increment of 0.
    pub num_overlap: u32,
    pub unique_terms: u32,
    /// The field indexes docs only (no freqs).
    pub docs_only: bool,
}

/// Lucene's `BasicModel`s of DFR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BasicModel {
    G,
    IF,
    In,
    Ine,
}

/// Lucene's DFR `AfterEffect`s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterEffect {
    B,
    L,
}

/// Term frequency normalizations of DFR and IB.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Normalization {
    None,
    H1(f32),
    H2(f32),
    H3(f32),
    Z(f32),
}

/// IB distributions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Distribution {
    LL,
    SPL,
}

/// IB lambdas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lambda {
    DF,
    TTF,
}

/// DFI independence measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Independence {
    Standardized,
    Saturated,
    ChiSquared,
}

/// Inputs of a scripted similarity (Elasticsearch's `ScriptedSimilarity` objects).
#[derive(Clone, Copy, Debug)]
pub struct ScriptStats {
    /// `query.boost`
    pub boost: f32,
    /// `field.docCount`, `field.sumDocFreq`, `field.sumTotalTermFreq`
    pub doc_count: u64,
    pub sum_doc_freq: u64,
    pub sum_total_term_freq: u64,
    /// `term.docFreq`, `term.totalTermFreq`
    pub doc_freq: u64,
    pub total_term_freq: u64,
}

/// A similarity implemented by scripts (`type: scripted`). `search-es` implements it with
/// Painless (`weight_script` and `script`).
pub trait ScriptedSimilarity: Send + Sync {
    /// The weight script; `None` when there is none (the weight is then the query boost).
    fn weight(&self, stats: &ScriptStats) -> Option<f64>;
    /// The score script for one doc: `doc.length` and `doc.freq`.
    fn score(&self, weight: f64, stats: &ScriptStats, doc_length: u32, freq: f32) -> f64;
    /// Description used in explanations (`ScriptedSimilarity(weightScript=[…], script=[…])`).
    fn describe(&self) -> String;
    fn discount_overlaps(&self) -> bool {
        true
    }
}

/// A similarity with its parameters.
#[derive(Clone)]
pub enum Similarity {
    /// `legacy`: Elasticsearch's `BM25` (boost × (k1 + 1)); false: Lucene's plain `BM25Similarity`.
    Bm25 { k1: f32, b: f32, discount_overlaps: bool, legacy: bool },
    Dfr { basic_model: BasicModel, after_effect: AfterEffect, normalization: Normalization, discount_overlaps: bool },
    Ib { distribution: Distribution, lambda: Lambda, normalization: Normalization, discount_overlaps: bool },
    Dfi { independence: Independence, discount_overlaps: bool },
    LmDirichlet { mu: f32, discount_overlaps: bool },
    LmJelinekMercer { lambda: f32, discount_overlaps: bool },
    Boolean,
    Scripted(Arc<dyn ScriptedSimilarity>),
}

impl std::fmt::Debug for Similarity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_lucene_string())
    }
}

impl Default for Similarity {
    fn default() -> Similarity {
        Similarity::bm25()
    }
}

/// Lucene's `SmallFloat.byte4ToInt` for every norm byte.
fn length_table() -> &'static [f32; 256] {
    static T: std::sync::OnceLock<[f32; 256]> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0f32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            *v = byte4_to_int(i as u8) as f32;
        }
        t
    })
}

/// Decoded field length of a norm byte.
#[inline]
pub fn norm_length(norm: u8) -> f32 {
    length_table()[norm as usize]
}

fn log2(x: f64) -> f64 {
    x.ln() / std::f64::consts::LN_2
}

impl Similarity {
    /// Elasticsearch's default similarity (`BM25`, k1 1.2, b 0.75).
    pub fn bm25() -> Similarity {
        Similarity::Bm25 { k1: 1.2, b: 0.75, discount_overlaps: true, legacy: true }
    }

    pub fn discount_overlaps(&self) -> bool {
        match self {
            Similarity::Bm25 { discount_overlaps, .. }
            | Similarity::Dfr { discount_overlaps, .. }
            | Similarity::Ib { discount_overlaps, .. }
            | Similarity::Dfi { discount_overlaps, .. }
            | Similarity::LmDirichlet { discount_overlaps, .. }
            | Similarity::LmJelinekMercer { discount_overlaps, .. } => *discount_overlaps,
            Similarity::Boolean => true,
            Similarity::Scripted(s) => s.discount_overlaps(),
        }
    }

    /// The norm byte of a field (Lucene `Similarity.computeNorm`).
    pub fn compute_norm(&self, st: &InvertState) -> u8 {
        let n = if st.docs_only {
            st.unique_terms
        } else if self.discount_overlaps() {
            st.length - st.num_overlap
        } else {
            st.length
        };
        int_to_byte4(n)
    }

    /// Lucene's `toString` of the similarity.
    pub fn to_lucene_string(&self) -> String {
        match self {
            Similarity::Bm25 { k1, b, legacy, .. } => {
                if *legacy {
                    format!("LegacyBM25Similarity(k1={},b={})", float_to_string(*k1), float_to_string(*b))
                } else {
                    format!("BM25(k1={},b={})", float_to_string(*k1), float_to_string(*b))
                }
            }
            Similarity::Dfr { basic_model, after_effect, normalization, .. } => {
                format!("DFR {}{}{}", basic_name(*basic_model), ae_name(*after_effect), norm_name(normalization))
            }
            Similarity::Ib { distribution, lambda, normalization, .. } => format!(
                "IB {}-{}{}",
                match distribution {
                    Distribution::LL => "LL",
                    Distribution::SPL => "SPL",
                },
                match lambda {
                    Lambda::DF => "D",
                    Lambda::TTF => "L",
                },
                norm_name(normalization)
            ),
            Similarity::Dfi { independence, .. } => format!("DFI({})", indep_name(*independence)),
            Similarity::LmDirichlet { mu, .. } => format!("LM Dirichlet({mu:.6})"),
            Similarity::LmJelinekMercer { lambda, .. } => format!("LM Jelinek-Mercer({lambda:.6})"),
            Similarity::Boolean => "BooleanSimilarity".into(),
            Similarity::Scripted(s) => s.describe(),
        }
    }

    /// A scorer for one query term (or several terms scored as one, e.g. a phrase).
    pub fn scorer(&self, boost: f32, coll: &CollectionStats, terms: &[TermStats]) -> SimScorer {
        match self {
            Similarity::Bm25 { k1, b, legacy, .. } => {
                let boost = if *legacy { boost * (1.0 + *k1) } else { boost };
                SimScorer::Bm25(Box::new(Bm25Scorer::new(boost, *k1, *b, coll, terms)))
            }
            Similarity::Boolean => SimScorer::Boolean(boost),
            Similarity::Scripted(s) => {
                let subs = terms
                    .iter()
                    .map(|t| {
                        let st = ScriptStats {
                            boost,
                            doc_count: coll.doc_count,
                            sum_doc_freq: coll.sum_doc_freq,
                            sum_total_term_freq: coll.sum_total_term_freq,
                            doc_freq: t.doc_freq,
                            total_term_freq: t.total_term_freq,
                        };
                        let w = s.weight(&st);
                        (st, w)
                    })
                    .collect();
                SimScorer::Scripted(s.clone(), subs)
            }
            _ => {
                let model = BaseModel::of(self);
                let subs = terms.iter().map(|t| BaseScorer::new(model, boost as f64, coll, t)).collect();
                SimScorer::Base(subs)
            }
        }
    }
}

fn basic_name(m: BasicModel) -> &'static str {
    match m {
        BasicModel::G => "G",
        BasicModel::IF => "I(F)",
        BasicModel::In => "I(n)",
        BasicModel::Ine => "I(ne)",
    }
}

fn ae_name(a: AfterEffect) -> &'static str {
    match a {
        AfterEffect::B => "B",
        AfterEffect::L => "L",
    }
}

fn norm_name(n: &Normalization) -> String {
    match n {
        Normalization::None => String::new(),
        Normalization::H1(_) => "1".into(),
        Normalization::H2(_) => "2".into(),
        Normalization::H3(mu) => format!("3({})", float_to_string(*mu)),
        Normalization::Z(z) => format!("Z({})", float_to_string(*z)),
    }
}

fn indep_name(i: Independence) -> &'static str {
    match i {
        Independence::Standardized => "Standardized",
        Independence::Saturated => "Saturated",
        Independence::ChiSquared => "ChiSquared",
    }
}

/// Scores one term (or a group of terms) of one field.
pub enum SimScorer {
    Bm25(Box<Bm25Scorer>),
    Base(Vec<BaseScorer>),
    Boolean(f32),
    Scripted(Arc<dyn ScriptedSimilarity>, Vec<(ScriptStats, Option<f64>)>),
    /// A feature function (`query::feature`): the frequency encodes the feature value.
    Feature(Box<crate::query::feature::FeatureSim>),
}

impl SimScorer {
    /// Score of a doc with term frequency `freq` and norm byte `norm` (1 without norms).
    #[inline]
    pub fn score(&self, freq: f32, norm: u8) -> f32 {
        match self {
            SimScorer::Bm25(s) => s.score(freq, norm),
            SimScorer::Base(subs) => {
                if subs.len() == 1 {
                    subs[0].score(freq, norm)
                } else {
                    let mut sum = 0f64;
                    for s in subs {
                        sum += s.score(freq, norm) as f64;
                    }
                    sum as f32
                }
            }
            SimScorer::Boolean(b) => *b,
            SimScorer::Scripted(s, subs) => {
                let len = norm_length(norm) as u32;
                let mut sum = 0f64;
                for (st, w) in subs {
                    // Without a weight script the weight is 1 (the boost is `query.boost`).
                    let w = w.unwrap_or(1.0);
                    sum += s.score(w, st, len, freq);
                }
                sum as f32
            }
            SimScorer::Feature(f) => f.score(freq),
        }
    }

    /// Scores a batch: `out[i] = score(freqs[i], norms[i])`, BM25 without per-doc dispatch.
    #[inline]
    pub fn score_batch(&self, freqs: &[u32], norms: &[u8], out: &mut [f32]) {
        let n = out.len();
        let (freqs, norms) = (&freqs[..n], &norms[..n]);
        match self {
            SimScorer::Bm25(b) => {
                let w = b.weight;
                for i in 0..n {
                    let ni = b.cache[norms[i] as usize];
                    out[i] = w - w / (1f32 + freqs[i] as f32 * ni);
                }
            }
            _ => {
                for i in 0..n {
                    out[i] = self.score(freqs[i] as f32, norms[i]);
                }
            }
        }
    }

    /// The Java class name of the scorer (span explanations print it).
    pub fn class_name(&self) -> &'static str {
        match self {
            SimScorer::Bm25(_) => "BM25Scorer",
            SimScorer::Base(subs) if subs.len() == 1 => "BasicSimScorer",
            SimScorer::Base(_) => "MultiSimScorer",
            SimScorer::Boolean(_) => "BooleanWeight",
            SimScorer::Scripted(..) => "ScriptedSimScorer",
            SimScorer::Feature(_) => "FeatureSimScorer",
        }
    }

    /// Whether scores never decrease with freq and never increase with norm, so impacts give
    /// valid upper bounds (true for every built-in similarity, as Lucene requires).
    pub fn is_monotonic(&self) -> bool {
        !matches!(self, SimScorer::Scripted(..))
    }

    /// The explanation of [`score`](Self::score); `freq` is the frequency's own explanation.
    pub fn explain(&self, freq: Explanation, norm: u8) -> Explanation {
        match self {
            SimScorer::Bm25(s) => s.explain(freq, norm),
            SimScorer::Base(subs) => {
                if subs.len() == 1 {
                    subs[0].explain(freq, norm)
                } else {
                    let details: Vec<Explanation> = subs.iter().map(|s| s.explain(freq.clone(), norm)).collect();
                    Explanation::matched(self.score(freq.value(), norm), "sum of:", details)
                }
            }
            SimScorer::Boolean(b) => {
                let q = Explanation::leaf(*b, "boost, query boost");
                Explanation::matched(*b, "score(BooleanWeight), computed from:", vec![q])
            }
            SimScorer::Scripted(s, subs) => {
                let len = norm_length(norm) as u32;
                let mut details = Vec::new();
                for (st, w) in subs {
                    let wv = w.unwrap_or(1.0);
                    let score = s.score(wv, st, len, freq.value());
                    let d = vec![
                        Explanation::leaf(wv as f32, "weight"),
                        Explanation::leaf(st.boost, "query.boost"),
                        Explanation::leaf(st.doc_count as i64, "field.docCount"),
                        Explanation::leaf(st.sum_doc_freq as i64, "field.sumDocFreq"),
                        Explanation::leaf(st.sum_total_term_freq as i64, "field.sumTotalTermFreq"),
                        Explanation::leaf(st.doc_freq as i64, "term.docFreq"),
                        Explanation::leaf(st.total_term_freq as i64, "term.totalTermFreq"),
                        Explanation::leaf(freq.value(), "doc.freq"),
                        Explanation::leaf(len as i32, "doc.length"),
                    ];
                    details.push(Explanation::matched(
                        score as f32,
                        format!("score from {} computed from:", s.describe()),
                        d,
                    ));
                }
                if details.len() == 1 {
                    details.pop().unwrap()
                } else {
                    Explanation::matched(self.score(freq.value(), norm), "Sum of:", details)
                }
            }
            // Feature queries explain through their own weight.
            SimScorer::Feature(f) => Explanation::matched(f.score(freq.value()), "feature function of:", vec![freq]),
        }
    }
}

/// Lucene 10 `BM25Similarity.BM25Scorer`.
pub struct Bm25Scorer {
    boost: f32,
    k1: f32,
    b: f32,
    idf: Explanation,
    avgdl: f32,
    weight: f32,
    cache: [f32; 256],
}

impl Bm25Scorer {
    fn new(boost: f32, k1: f32, b: f32, coll: &CollectionStats, terms: &[TermStats]) -> Bm25Scorer {
        let idf = if terms.len() == 1 {
            bm25_idf_explain(coll, &terms[0])
        } else {
            let mut sum = 0f64;
            let details: Vec<Explanation> = terms
                .iter()
                .map(|t| {
                    let e = bm25_idf_explain(coll, t);
                    sum += e.value() as f64;
                    e
                })
                .collect();
            Explanation::matched(sum as f32, "idf, sum of:", details)
        };
        let avgdl = (coll.sum_total_term_freq as f64 / coll.doc_count as f64) as f32;
        let lt = length_table();
        let mut cache = [0f32; 256];
        for (i, c) in cache.iter_mut().enumerate() {
            *c = 1f32 / (k1 * ((1.0 - b) + b * lt[i] / avgdl));
        }
        let weight = boost * idf.value();
        Bm25Scorer { boost, k1, b, idf, avgdl, weight, cache }
    }

    #[inline(always)]
    pub fn score(&self, freq: f32, norm: u8) -> f32 {
        let ni = self.cache[norm as usize];
        self.weight - self.weight / (1f32 + freq * ni)
    }

    /// The per-norm cache (`1 / (k1 * (1 - b + b * dl / avgdl))`), for bulk scoring.
    #[inline]
    pub fn norm_inverse(&self, norm: u8) -> f32 {
        self.cache[norm as usize]
    }

    #[inline]
    pub fn weight(&self) -> f32 {
        self.weight
    }

    fn explain(&self, freq: Explanation, norm: u8) -> Explanation {
        let mut subs = Vec::new();
        if self.boost != 1.0 {
            subs.push(Explanation::leaf(self.boost, "boost"));
        }
        subs.push(self.idf.clone());
        let fv = freq.value();
        let desc = format!("score(freq={}), computed as boost * idf * tf from:", freq.value.to_java_string());
        let doclen = norm_length(norm);
        let mut tf = vec![freq];
        tf.push(Explanation::leaf(self.k1, "k1, term saturation parameter"));
        tf.push(Explanation::leaf(self.b, "b, length normalization parameter"));
        if norm > 39 {
            tf.push(Explanation::leaf(doclen, "dl, length of field (approximate)"));
        } else {
            tf.push(Explanation::leaf(doclen, "dl, length of field"));
        }
        tf.push(Explanation::leaf(self.avgdl, "avgdl, average length of field"));
        let ni = 1f32 / (self.k1 * ((1.0 - self.b) + self.b * doclen / self.avgdl));
        subs.push(Explanation::matched(
            1f32 - 1f32 / (1f32 + fv * ni),
            "tf, computed as freq / (freq + k1 * (1 - b + b * dl / avgdl)) from:",
            tf,
        ));
        let ni = self.cache[norm as usize];
        Explanation::matched(self.weight - self.weight / (1f32 + fv * ni), desc, subs)
    }
}

fn bm25_idf(doc_freq: u64, doc_count: u64) -> f32 {
    (1.0 + (doc_count as f64 - doc_freq as f64 + 0.5) / (doc_freq as f64 + 0.5)).ln() as f32
}

fn bm25_idf_explain(coll: &CollectionStats, t: &TermStats) -> Explanation {
    let idf = bm25_idf(t.doc_freq, coll.doc_count);
    Explanation::matched(
        idf,
        "idf, computed as log(1 + (N - n + 0.5) / (n + 0.5)) from:",
        vec![
            Explanation::leaf(t.doc_freq as i64, "n, number of documents containing term"),
            Explanation::leaf(coll.doc_count as i64, "N, total number of documents with field"),
        ],
    )
}

/// The models of Lucene's `SimilarityBase` family.
#[derive(Clone, Copy, Debug)]
enum BaseModel {
    Dfr(BasicModel, AfterEffect, Normalization),
    Ib(Distribution, Lambda, Normalization),
    Dfi(Independence),
    LmDirichlet(f32),
    LmJelinekMercer(f32),
}

impl BaseModel {
    fn of(s: &Similarity) -> BaseModel {
        match s {
            Similarity::Dfr { basic_model, after_effect, normalization, .. } => {
                BaseModel::Dfr(*basic_model, *after_effect, *normalization)
            }
            Similarity::Ib { distribution, lambda, normalization, .. } => {
                BaseModel::Ib(*distribution, *lambda, *normalization)
            }
            Similarity::Dfi { independence, .. } => BaseModel::Dfi(*independence),
            Similarity::LmDirichlet { mu, .. } => BaseModel::LmDirichlet(*mu),
            Similarity::LmJelinekMercer { lambda, .. } => BaseModel::LmJelinekMercer(*lambda),
            _ => unreachable!("not a SimilarityBase model"),
        }
    }

    fn class_name(&self) -> &'static str {
        match self {
            BaseModel::Dfr(..) => "DFRSimilarity",
            BaseModel::Ib(..) => "IBSimilarity",
            BaseModel::Dfi(_) => "DFISimilarity",
            BaseModel::LmDirichlet(_) => "LMDirichletSimilarity",
            BaseModel::LmJelinekMercer(_) => "LMJelinekMercerSimilarity",
        }
    }
}

/// Lucene's `BasicStats` plus the LM collection probability.
#[derive(Clone, Copy, Debug)]
struct BasicStats {
    n_docs: i64,
    field_tokens: i64,
    avgfl: f64,
    doc_freq: i64,
    ttf: i64,
    boost: f64,
    coll_prob: f64,
}

/// One term's scorer of the `SimilarityBase` family.
pub struct BaseScorer {
    model: BaseModel,
    st: BasicStats,
}

impl BaseScorer {
    fn new(model: BaseModel, boost: f64, coll: &CollectionStats, t: &TermStats) -> BaseScorer {
        let mut st = BasicStats {
            n_docs: coll.doc_count as i64,
            field_tokens: coll.sum_total_term_freq as i64,
            avgfl: coll.sum_total_term_freq as f64 / coll.doc_count as f64,
            doc_freq: t.doc_freq as i64,
            ttf: t.total_term_freq as i64,
            boost,
            coll_prob: 0.0,
        };
        if matches!(model, BaseModel::LmDirichlet(_) | BaseModel::LmJelinekMercer(_)) {
            st.coll_prob = (st.ttf as f64 + 1.0) / (st.field_tokens as f64 + 1.0);
        }
        BaseScorer { model, st }
    }

    #[inline]
    fn score(&self, freq: f32, norm: u8) -> f32 {
        self.score_f64(freq as f64, norm_length(norm) as f64) as f32
    }

    fn score_f64(&self, freq: f64, len: f64) -> f64 {
        let st = &self.st;
        match self.model {
            BaseModel::Dfr(bm, ae, nm) => {
                let tfn = tfn(nm, st, freq, len);
                let ae1 = ae_times_1p_tfn(ae, st);
                st.boost * basic_score(bm, st, tfn, ae1)
            }
            BaseModel::Ib(d, l, nm) => st.boost * dist_score(d, tfn(nm, st, freq, len), lambda(l, st) as f64),
            BaseModel::Dfi(ind) => {
                let expected = (st.ttf as f64 + 1.0) * len / (st.field_tokens as f64 + 1.0);
                if freq <= expected {
                    return 0.0;
                }
                st.boost * log2(indep_score(ind, freq, expected) + 1.0)
            }
            BaseModel::LmDirichlet(mu) => {
                let mu = mu as f64;
                let s = st.boost * ((1.0 + freq / (mu * st.coll_prob)).ln() + (mu / (len + mu)).ln());
                if s > 0.0 { s } else { 0.0 }
            }
            BaseModel::LmJelinekMercer(lambda) => {
                // `1 - lambda` is float arithmetic in Lucene.
                let one_minus = (1.0f32 - lambda) as f64;
                st.boost * (1.0 + (one_minus * freq / len) / (lambda as f64 * st.coll_prob)).ln()
            }
        }
    }

    fn explain(&self, freq: Explanation, norm: u8) -> Explanation {
        let st = &self.st;
        let len = norm_length(norm) as f64;
        let name = self.model.class_name();
        let fstr = freq.value.to_java_string();
        match self.model {
            BaseModel::Dfr(bm, ae, nm) => {
                let f = freq.value.as_f64();
                let mut subs = Vec::new();
                if st.boost != 1.0 {
                    subs.push(Explanation::leaf(st.boost as f32, "boost, query boost"));
                }
                let t = tfn(nm, st, f, len);
                let ae1 = ae_times_1p_tfn(ae, st);
                subs.push(norm_explain(nm, st, f, len));
                subs.push(basic_explain(bm, st, t, ae1));
                subs.push(ae_explain(ae, st, t));
                Explanation::matched(
                    self.score_f64(f, len) as f32,
                    format!(
                        "score({name}, freq={fstr}), computed as boost * basicModel.score(stats, tfn) * afterEffect.score(stats, tfn) from:"
                    ),
                    subs,
                )
            }
            BaseModel::Ib(d, l, nm) => {
                let f = freq.value.as_f64();
                let mut subs = Vec::new();
                if st.boost != 1.0 {
                    subs.push(Explanation::leaf(st.boost as f32, "boost, query boost"));
                }
                let ne = norm_explain(nm, st, f, len);
                let le = lambda_explain(l, st);
                let dv = dist_score(d, ne.value() as f64, le.value() as f64) as f32;
                subs.push(ne);
                subs.push(le);
                subs.push(Explanation::leaf(
                    dv,
                    match d {
                        Distribution::LL => "DistributionLL",
                        Distribution::SPL => "DistributionSPL",
                    },
                ));
                Explanation::matched(
                    self.score_f64(f, len) as f32,
                    format!(
                        "score({name}, freq={fstr}), computed as boost * distribution.score(stats, normalization.tfn(stats, freq, docLen), lambda.lambda(stats)) from:"
                    ),
                    subs,
                )
            }
            BaseModel::Dfi(ind) => {
                let f = freq.value.as_f64();
                let expected = (st.ttf as f64 + 1.0) * len / (st.field_tokens as f64 + 1.0);
                if f <= expected {
                    return Explanation::leaf(0f32, format!("score({name}, freq={fstr}), equals to 0"));
                }
                let ee = Explanation::matched(
                    expected as f32,
                    "expected, computed as (F + 1) * dl / (T + 1) from:",
                    vec![
                        Explanation::leaf(st.ttf, "F, total number of occurrences of term across all docs"),
                        Explanation::leaf(len as f32, "dl, length of field"),
                        Explanation::leaf(st.field_tokens, "T, total number of tokens in the field"),
                    ],
                );
                let measure = indep_score(ind, f, expected);
                let me = Explanation::matched(
                    measure as f32,
                    "measure, computed as independence.score(freq, expected) from:",
                    vec![freq, ee],
                );
                Explanation::matched(
                    self.score_f64(f, len) as f32,
                    format!("score({name}, freq={fstr}), computed as boost * log2(measure + 1) from:"),
                    vec![Explanation::leaf(st.boost as f32, "boost, query boost"), me],
                )
            }
            BaseModel::LmDirichlet(mu) => {
                let f = freq.value.as_f64();
                let mut subs = Vec::new();
                if st.boost != 1.0 {
                    subs.push(Explanation::leaf(st.boost as f32, "query boost"));
                }
                let p = st.coll_prob;
                subs.push(Explanation::leaf(mu, "mu"));
                let m = mu as f64;
                subs.push(Explanation::matched(
                    (1.0 + f / (m * p)).ln() as f32,
                    "term weight, computed as log(1 + freq /(mu * P)) from:",
                    vec![
                        Explanation::leaf(f as f32, "freq, number of occurrences of term in the document"),
                        Explanation::leaf(p as f32, "P, probability that the current term is generated by the collection"),
                    ],
                ));
                subs.push(Explanation::leaf((m / (len + m)).ln() as f32, "document norm, computed as log(mu / (dl + mu))"));
                subs.push(Explanation::leaf(len as f32, "dl, length of field"));
                subs.push(Explanation::leaf(p as f32, "collection probability"));
                Explanation::matched(
                    self.score_f64(f, len) as f32,
                    format!("score({name}, freq={fstr}), computed as boost * (term weight + document norm) from:"),
                    subs,
                )
            }
            BaseModel::LmJelinekMercer(lambda) => {
                let f = freq.value.as_f64();
                let mut subs = Vec::new();
                if st.boost != 1.0 {
                    subs.push(Explanation::leaf(st.boost as f32, "boost"));
                }
                subs.push(Explanation::leaf(lambda, "lambda"));
                let p = st.coll_prob;
                subs.push(Explanation::leaf(p as f32, "P, probability that the current term is generated by the collection"));
                subs.push(Explanation::leaf(f as f32, "freq, number of occurrences of term in the document"));
                subs.push(Explanation::leaf(len as f32, "dl, length of field"));
                subs.push(Explanation::leaf(p as f32, "collection probability"));
                Explanation::matched(
                    self.score_f64(f, len) as f32,
                    format!(
                        "score({name}, freq={fstr}), computed as boost * log(1 + ((1 - lambda) * freq / dl) /(lambda * P)) from:"
                    ),
                    subs,
                )
            }
        }
    }
}

fn tfn(n: Normalization, st: &BasicStats, tf: f64, len: f64) -> f64 {
    match n {
        Normalization::None => tf,
        Normalization::H1(c) => tf * c as f64 * (st.avgfl / len),
        Normalization::H2(c) => tf * log2(1.0 + c as f64 * st.avgfl / len),
        Normalization::H3(mu) => {
            // Lucene adds 1F to longs: `mu * (F+1F)/(T+1F)` is float arithmetic.
            let m = mu * ((st.ttf as f32 + 1f32) / (st.field_tokens as f32 + 1f32));
            (tf + m as f64) / (len + mu as f64) * mu as f64
        }
        Normalization::Z(z) => tf * (st.avgfl / len).powf(z as f64),
    }
}

fn norm_explain(n: Normalization, st: &BasicStats, tf: f64, len: f64) -> Explanation {
    let v = tfn(n, st, tf, len) as f32;
    let tfe = Explanation::leaf(tf as f32, "tf, number of occurrences of term in the document");
    let avg = Explanation::leaf(st.avgfl as f32, "avgfl, average length of field across all documents");
    let fl = Explanation::leaf(len as f32, "fl, field length of the document");
    match n {
        Normalization::None => Explanation::leaf(1i32, "no normalization"),
        Normalization::H1(c) => Explanation::matched(
            v,
            "NormalizationH1, computed as tf * c * (avgfl / fl) from:",
            vec![tfe, Explanation::leaf(c, "c, hyper-parameter"), avg, fl],
        ),
        Normalization::H2(c) => Explanation::matched(
            v,
            "NormalizationH2, computed as tf * log2(1 + c * avgfl / fl) from:",
            vec![tfe, Explanation::leaf(c, "c, hyper-parameter"), avg, fl],
        ),
        Normalization::H3(mu) => Explanation::matched(
            v,
            "NormalizationH3, computed as (tf + mu * ((F+1) / (T+1))) / (fl + mu) * mu from:",
            vec![
                tfe,
                Explanation::leaf(mu, "mu, smoothing parameter"),
                Explanation::leaf(st.ttf as f32, "F,  total number of occurrences of term across all documents"),
                Explanation::leaf(st.field_tokens as f32, "T, total number of tokens of the field across all documents"),
                fl,
            ],
        ),
        Normalization::Z(z) => Explanation::matched(
            v,
            "NormalizationZ, computed as tf * Math.pow(avgfl / fl, z) from:",
            vec![tfe, avg, fl, Explanation::leaf(z, "z, relates to specificity of the language")],
        ),
    }
}

fn basic_score(m: BasicModel, st: &BasicStats, tfn: f64, ae1: f64) -> f64 {
    match m {
        BasicModel::G => {
            let f = st.ttf as f64 + 1.0;
            let n = st.n_docs as f64;
            let lambda = f / (n + f);
            let a = log2(lambda + 1.0);
            let b = log2((1.0 + lambda) / lambda);
            (b - (b - a) / (1.0 + tfn)) * ae1
        }
        BasicModel::IF => {
            let a = log2(1.0 + (st.n_docs + 1) as f64 / (st.ttf as f64 + 0.5));
            a * ae1 * (1.0 - 1.0 / (1.0 + tfn))
        }
        BasicModel::In => {
            let a = log2((st.n_docs + 1) as f64 / (st.doc_freq as f64 + 0.5));
            a * ae1 * (1.0 - 1.0 / (1.0 + tfn))
        }
        BasicModel::Ine => {
            let n = st.n_docs as f64;
            let ne = n * (1.0 - ((st.n_docs - 1) as f64 / n).powf(st.ttf as f64));
            let a = log2((st.n_docs + 1) as f64 / (ne + 0.5));
            a * ae1 * (1.0 - 1.0 / (1.0 + tfn))
        }
    }
}

fn basic_explain(m: BasicModel, st: &BasicStats, tfn: f64, ae1: f64) -> Explanation {
    let v = (basic_score(m, st, tfn, ae1) * (1.0 + tfn) / ae1) as f32;
    let tfe = Explanation::leaf(tfn as f32, "tfn, normalized term frequency");
    match m {
        BasicModel::G => {
            let f = st.ttf as f64 + 1.0;
            let n = st.n_docs as f64;
            let lambda = f / (n + f);
            Explanation::matched(
                v,
                "BasicModelG, computed as log2(lambda + 1) + tfn * log2((1 + lambda) / lambda) from:",
                vec![
                    tfe,
                    Explanation::matched(
                        lambda as f32,
                        "lambda, computed as F / (N + F) from:",
                        vec![
                            Explanation::leaf(f as f32, "F, total number of occurrences of term across all docs + 1"),
                            Explanation::leaf(n as f32, "N, total number of documents with field"),
                        ],
                    ),
                ],
            )
        }
        BasicModel::IF => Explanation::matched(
            v,
            "BasicModelIF, computed as tfn * log2(1 + (N + 1) / (F + 0.5)) from:",
            vec![
                tfe,
                Explanation::leaf(st.n_docs, "N, total number of documents with field"),
                Explanation::leaf(st.ttf, "F, total number of occurrences of term across all documents"),
            ],
        ),
        BasicModel::In => Explanation::matched(
            v,
            "BasicModelIn, computed as tfn * log2((N + 1) / (n + 0.5)) from:",
            vec![
                tfe,
                Explanation::leaf(st.n_docs, "N, total number of documents with field"),
                Explanation::leaf(st.doc_freq, "n, number of documents containing term"),
            ],
        ),
        BasicModel::Ine => {
            let f = st.ttf as f64;
            let n = st.n_docs as f64;
            let ne = n * (1.0 - ((n - 1.0) / n).powf(f));
            Explanation::matched(
                v,
                "BasicModelIne, computed as tfn * log2((N + 1) / (ne + 0.5)) from:",
                vec![
                    tfe,
                    Explanation::matched(
                        ne as f32,
                        "ne, computed as N * (1 - Math.pow((N - 1) / N, F)) from:",
                        vec![
                            Explanation::leaf(f as f32, "F, total number of occurrences of term across all docs"),
                            Explanation::leaf(n as f32, "N, total number of documents with field"),
                        ],
                    ),
                ],
            )
        }
    }
}

fn ae_times_1p_tfn(a: AfterEffect, st: &BasicStats) -> f64 {
    match a {
        AfterEffect::B => {
            let f = st.ttf + 1;
            let n = st.doc_freq + 1;
            (f as f64 + 1.0) / n as f64
        }
        AfterEffect::L => 1.0,
    }
}

fn ae_explain(a: AfterEffect, st: &BasicStats, tfn: f64) -> Explanation {
    let v = (ae_times_1p_tfn(a, st) / (1.0 + tfn)) as f32;
    let tfe = Explanation::leaf(tfn as f32, "tfn, normalized term frequency");
    match a {
        AfterEffect::B => Explanation::matched(
            v,
            "AfterEffectB, computed as (F + 1) / (n * (tfn + 1)) from:",
            vec![
                tfe.clone(),
                Explanation::leaf(st.ttf, "F, total number of occurrences of term across all documents + 1"),
                Explanation::leaf(st.doc_freq, "n, number of documents containing term + 1"),
                tfe,
            ],
        ),
        AfterEffect::L => Explanation::matched(v, "AfterEffectL, computed as 1 / (tfn + 1) from:", vec![tfe]),
    }
}

fn lambda(l: Lambda, st: &BasicStats) -> f32 {
    match l {
        Lambda::DF => {
            let v = ((st.doc_freq as f64 + 1.0) / (st.n_docs as f64 + 1.0)) as f32;
            if v == 1.0 { next_down_f32(v) } else { v }
        }
        Lambda::TTF => {
            let v = ((st.ttf as f64 + 1.0) / (st.n_docs as f64 + 1.0)) as f32;
            if v == 1.0 { next_up_f32(v) } else { v }
        }
    }
}

fn lambda_explain(l: Lambda, st: &BasicStats) -> Explanation {
    let v = lambda(l, st);
    match l {
        Lambda::DF => Explanation::matched(
            v,
            "LambdaDF, computed as (n + 1) / (N + 1) from:",
            vec![
                Explanation::leaf(st.doc_freq, "n, number of documents containing term"),
                Explanation::leaf(st.n_docs, "N, total number of documents with field"),
            ],
        ),
        Lambda::TTF => Explanation::matched(
            v,
            "LambdaTTF, computed as (F + 1) / (N + 1) from:",
            vec![
                Explanation::leaf(st.ttf, "F, total number of occurrences of term across all documents"),
                Explanation::leaf(st.n_docs, "N, total number of documents with field"),
            ],
        ),
    }
}

fn dist_score(d: Distribution, tfn: f64, lambda: f64) -> f64 {
    match d {
        Distribution::LL => -(lambda / (tfn + lambda)).ln(),
        Distribution::SPL => {
            let mut q = 1.0 - 1.0 / (tfn + 1.0);
            if q == 1.0 {
                q = next_down_f64(1.0);
            }
            let mut pow = lambda.powf(q);
            if pow == lambda {
                pow = if lambda < 1.0 { next_up_f64(lambda) } else { next_down_f64(lambda) };
            }
            -((pow - lambda) / (1.0 - lambda)).ln()
        }
    }
}

fn indep_score(i: Independence, freq: f64, expected: f64) -> f64 {
    match i {
        Independence::Standardized => (freq - expected) / expected.sqrt(),
        Independence::Saturated => (freq - expected) / expected,
        Independence::ChiSquared => (freq - expected) * (freq - expected) / expected,
    }
}

fn next_up_f32(v: f32) -> f32 {
    if v.is_nan() || v == f32::INFINITY {
        return v;
    }
    if v == 0.0 {
        return f32::from_bits(1);
    }
    let b = v.to_bits();
    f32::from_bits(if v > 0.0 { b + 1 } else { b - 1 })
}

fn next_down_f32(v: f32) -> f32 {
    -next_up_f32(-v)
}

fn next_up_f64(v: f64) -> f64 {
    if v.is_nan() || v == f64::INFINITY {
        return v;
    }
    if v == 0.0 {
        return f64::from_bits(1);
    }
    let b = v.to_bits();
    f64::from_bits(if v > 0.0 { b + 1 } else { b - 1 })
}

fn next_down_f64(v: f64) -> f64 {
    -next_up_f64(-v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coll() -> CollectionStats {
        CollectionStats { max_doc: 3, doc_count: 3, sum_total_term_freq: 13, sum_doc_freq: 11 }
    }

    /// Values from the reference Elasticsearch 9.5.5 `_explain` (index of three docs:
    /// "the quick brown fox jumps over the lazy dog", "quick quick fox", "lazy").
    #[test]
    fn bm25_matches_elasticsearch() {
        let s = Similarity::bm25();
        let sc = s.scorer(1.0, &coll(), &[TermStats { doc_freq: 2, total_term_freq: 3 }]);
        let norm = int_to_byte4(3);
        assert_eq!(sc.score(2.0, norm), 0.7074792);
        assert_eq!(sc.score(1.0, norm), 0.5376842);
        let e = sc.explain(Explanation::leaf(2.0f32, "freq, occurrences of term within document"), norm);
        assert_eq!(e.value(), 0.7074792);
        assert_eq!(e.description, "score(freq=2.0), computed as boost * idf * tf from:");
        assert_eq!(e.details[0].value(), 2.2);
        assert_eq!(e.details[1].value(), 0.47000363);
        assert_eq!(e.details[2].value(), 0.68421054);
        assert_eq!(e.details[2].details[4].value(), 4.3333335);
        // Phrase: idf summed over the terms.
        let sc = s.scorer(
            1.0,
            &coll(),
            &[TermStats { doc_freq: 2, total_term_freq: 3 }, TermStats { doc_freq: 2, total_term_freq: 2 }],
        );
        assert_eq!(sc.score(1.0, norm), 1.0753684);
    }

    #[test]
    fn base_family_is_finite() {
        let sims = [
            Similarity::Dfr {
                basic_model: BasicModel::G,
                after_effect: AfterEffect::L,
                normalization: Normalization::H2(1.0),
                discount_overlaps: true,
            },
            Similarity::Ib {
                distribution: Distribution::SPL,
                lambda: Lambda::DF,
                normalization: Normalization::H3(800.0),
                discount_overlaps: true,
            },
            Similarity::Dfi { independence: Independence::ChiSquared, discount_overlaps: true },
            Similarity::LmDirichlet { mu: 2000.0, discount_overlaps: true },
            Similarity::LmJelinekMercer { lambda: 0.1, discount_overlaps: true },
            Similarity::Boolean,
        ];
        for s in sims {
            let sc = s.scorer(1.0, &coll(), &[TermStats { doc_freq: 2, total_term_freq: 3 }]);
            for f in 1..5 {
                let v = sc.score(f as f32, int_to_byte4(3));
                assert!(v.is_finite() && v >= 0.0, "{s:?} {v}");
                let e = sc.explain(Explanation::leaf(f as f32, "freq"), int_to_byte4(3));
                assert_eq!(e.value(), v, "{s:?}");
            }
        }
    }
}
