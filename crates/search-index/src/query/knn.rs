//! Approximate and exact k-nearest-neighbour search over vector fields (Elasticsearch `knn`).
//!
//! The query is resolved while rewriting, as in Lucene: each segment searches its HNSW graph
//! (or scans, for flat fields, exact requests and selective filters) for its best `k`, the
//! results are merged into the global top `k`, and the query becomes a fixed set of
//! `(doc, score)` pairs — Lucene's `DocAndScoreQuery`, whose explanation reads "within top N
//! docs" (Elasticsearch's top-level `knn` section uses `KnnScoreDocQuery` instead: "within top
//! k documents"; see [`ScoreDocQuery`]). Filters follow Lucene's strategy: a filter matching at most `k`
//! docs of a segment is searched exactly; otherwise the graph search visits at most as many
//! vectors as the filter accepts and falls back to exact search when that limit is hit.
//! Quantized fields are rescored with exact float similarities by the codec.

use std::any::Any;
use std::sync::Arc;

use search_codec::{NO_MORE_DOCS, VectorQuery};

use crate::error::{Error, Result};
use crate::query::basic::MatchNoneQuery;
use crate::query::nested::segment_bits;
use crate::util::bitset::FixedBitSet;
use crate::schema::Schema;
use crate::search::{Explanation, Query, QueryRef, Scorable, ScoreMode, Scorer, Searcher, SegmentContext, Weight};
use crate::util::java::float_to_string;

/// A query vector.
#[derive(Clone, Debug, PartialEq)]
pub enum QueryVector {
    Float(Vec<f32>),
    Byte(Vec<i8>),
    /// Packed bits (`element_type: bit`).
    Bits(Vec<u8>),
}

impl QueryVector {
    fn as_ref(&self) -> VectorQuery<'_> {
        match self {
            QueryVector::Float(v) => VectorQuery::Float(v),
            QueryVector::Byte(v) => VectorQuery::Byte(v),
            QueryVector::Bits(v) => VectorQuery::Bits(v),
        }
    }

    fn first(&self) -> String {
        match self {
            QueryVector::Float(v) => v.first().map_or("".into(), |x| crate::util::java::float_to_string(*x)),
            QueryVector::Byte(v) => v.first().map_or("".into(), |x| x.to_string()),
            QueryVector::Bits(v) => v.first().map_or("".into(), |x| x.to_string()),
        }
    }
}

/// k-NN over one vector field.
#[derive(Clone, Debug)]
pub struct KnnQuery {
    pub field: u32,
    pub vector: QueryVector,
    pub k: usize,
    /// Graph search beam (`num_candidates`, at least `k`).
    pub num_candidates: usize,
    pub filter: Option<QueryRef>,
    /// Minimum *score* a hit needs (Elasticsearch's `similarity`, converted to a score by the
    /// caller for the field's similarity function).
    pub min_score: Option<f32>,
    /// Scan every vector instead of searching the graph.
    pub exact: bool,
    /// Elasticsearch's `rescore_vector.oversample`: gather `ceil(k · oversample)` candidates
    /// (at most 10 000; the beam grows to match), rescore them with full-precision vectors and
    /// keep the best `k`. Hits are always rescored exactly here, so this only widens the
    /// candidate pool of quantized fields.
    pub oversample: Option<f32>,
    /// Vectors of nested docs (Lucene's `DiversifyingChildren*KnnVectorQuery`): the parents
    /// of the nested path's level. Hits are the best child of each of the top `k` parents
    /// (wrap the query in a `ToParentQuery` with `max` to score the parents).
    pub parent_filter: Option<QueryRef>,
}

impl KnnQuery {
    pub fn new(field: u32, vector: QueryVector, k: usize, num_candidates: usize) -> KnnQuery {
        KnnQuery { field, vector, k, num_candidates: num_candidates.max(k), filter: None, min_score: None, exact: false, oversample: None, parent_filter: None }
    }

    /// Searches every segment and returns the global top `k` as `(global doc, score)`.
    pub fn search(&self, s: &Searcher) -> Result<Vec<(u32, f32)>> {
        let (k, num_candidates) = match self.oversample {
            Some(o) if o > 0.0 => {
                let k = ((self.k as f64 * o as f64).ceil() as usize).min(10_000).max(self.k);
                (k, self.num_candidates.max(k))
            }
            _ => (self.k, self.num_candidates),
        };
        let mut all: Vec<(u32, f32)> = Vec::new();
        let filter = match &self.filter {
            Some(f) => {
                let r = s.rewrite(f)?;
                if r.as_any().is::<MatchNoneQuery>() {
                    return Ok(Vec::new());
                }
                Some(r)
            }
            None => None,
        };
        let parent_filter = match &self.parent_filter {
            Some(p) => Some(s.rewrite(p)?),
            None => None,
        };
        for seg in s.segments() {
            let Some(vr) = seg.reader.vectors(self.field) else { continue };
            if vr.is_empty() {
                continue;
            }
            let q = vr.prepare(self.vector.as_ref()).map_err(|e| Error::illegal(e.to_string()))?;
            let live = seg.live.clone();
            let bits = match &filter {
                Some(f) => Some(segment_bits(s, seg, f)?),
                None => None,
            };
            let accept = |d: u32| live.as_ref().is_none_or(|l| l.is_live(d)) && bits.as_ref().is_none_or(|b| b.get(d));
            // Accepted docs when filtered: filtered live docs with a vector.
            let cost = match &bits {
                Some(b) => {
                    let mut cost = 0u32;
                    let mut c = vr.docs().cursor();
                    loop {
                        let d = c.next();
                        if d == NO_MORE_DOCS {
                            break;
                        }
                        if b.get(d) && live.as_ref().is_none_or(|l| l.is_live(d)) {
                            cost += 1;
                        }
                    }
                    if cost == 0 {
                        continue;
                    }
                    Some(cost)
                }
                None => None,
            };
            let accept_opt: Option<&dyn Fn(u32) -> bool> = if bits.is_some() || live.is_some() { Some(&accept) } else { None };
            // One search of `kk` hits (beam `nc`): exact when asked or when the filter accepts
            // few docs, else the graph, falling back to exact when the visit limit hits.
            let run = |kk: usize, nc: usize| -> search_codec::vectors::KnnResult {
                match cost {
                    Some(cost) if self.exact || cost as usize <= kk => vr.exact_search(&q, kk, accept_opt),
                    Some(cost) => {
                        let r = vr.search_filtered(&q, kk, nc, &accept, cost, cost as usize + 1);
                        if r.complete { r } else { vr.exact_search(&q, kk, accept_opt) }
                    }
                    None if self.exact => vr.exact_search(&q, kk, accept_opt),
                    None => {
                        let r = vr.search(&q, kk, nc, accept_opt, usize::MAX);
                        if r.complete { r } else { vr.exact_search(&q, kk, Some(&accept)) }
                    }
                }
            };
            let res = match &parent_filter {
                None => run(k, num_candidates),
                Some(pf) => {
                    let parents = segment_bits(s, seg, pf)?;
                    diversified(vr, &q, k, num_candidates, &accept, &parents, self.exact, &run)
                }
            };
            for h in res.hits {
                if self.min_score.is_none_or(|m| h.score >= m) {
                    all.push((seg.base + h.doc, h.score));
                }
            }
        }
        all.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        all.truncate(self.k);
        all.sort_by_key(|x| x.0);
        Ok(all)
    }
}

/// The best child per parent, best parents first (score descending, then doc), at most `k`.
fn diversify(hits: impl Iterator<Item = (u32, f32)>, parents: &FixedBitSet, k: usize) -> Vec<search_codec::vectors::KnnHit> {
    // parent -> (child, score)
    let mut best: std::collections::HashMap<u32, (u32, f32)> = std::collections::HashMap::new();
    for (doc, score) in hits {
        let p = parents.next_set_bit(doc);
        let e = best.entry(p).or_insert((doc, score));
        // Ties keep the first child (Lucene keeps a strictly better score only).
        if score > e.1 || (score == e.1 && doc < e.0) {
            *e = (doc, score);
        }
    }
    let mut out: Vec<search_codec::vectors::KnnHit> = best.into_values().map(|(doc, score)| search_codec::vectors::KnnHit { doc, score }).collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc.cmp(&b.doc)));
    out.truncate(k);
    out
}

/// Diversified k-NN in one segment: exact over every accepted child, or graph searches that
/// widen until `k` distinct parents come back (or the graph is exhausted).
#[allow(clippy::too_many_arguments)]
fn diversified(
    vr: &search_codec::vectors::VectorsReader,
    q: &search_codec::vectors::PreparedQuery,
    k: usize,
    num_candidates: usize,
    accept: &dyn Fn(u32) -> bool,
    parents: &FixedBitSet,
    exact: bool,
    run: &dyn Fn(usize, usize) -> search_codec::vectors::KnnResult,
) -> search_codec::vectors::KnnResult {
    let total = vr.docs().len() as usize;
    let scan = || {
        let mut c = vr.docs().cursor();
        let mut hits = Vec::new();
        loop {
            let d = c.next();
            if d == NO_MORE_DOCS {
                break;
            }
            if accept(d) {
                hits.push((d, vr.score(q, c.index())));
            }
        }
        let n = hits.len();
        search_codec::vectors::KnnResult { hits: diversify(hits.into_iter(), parents, k), visited: n, complete: true }
    };
    if exact {
        return scan();
    }
    let mut kk = num_candidates.max(k);
    loop {
        let r = run(kk, kk);
        let found = r.hits.len();
        let hits = diversify(r.hits.iter().map(|h| (h.doc, h.score)), parents, k);
        if hits.len() >= k || found < kk || kk >= total {
            return search_codec::vectors::KnnResult { hits, visited: r.visited, complete: true };
        }
        if kk >= 4 * num_candidates.max(k) * 16 {
            return scan();
        }
        kk = (kk * 4).min(total);
    }
}

impl Query for KnnQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let r = self.search(s)?;
        ScoreDocQuery::lucene(r).weight(s, mode, boost)
    }

    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>> {
        let r = self.search(s)?;
        if r.is_empty() {
            return Ok(Some(Arc::new(MatchNoneQuery::new(""))));
        }
        Ok(Some(Arc::new(ScoreDocQuery::lucene(r))))
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        let mut b = format!("KnnFloatVectorQuery:{}[{},...][{}]", schema.name(self.field), self.vector.first(), self.k);
        if let Some(f) = &self.filter {
            b.push_str(&format!("[{}]", f.lucene_string(schema)));
        }
        b
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A fixed set of docs with scores: Elasticsearch's `KnnScoreDocQuery` (the top-level `knn`
/// section's merged hits; [`ScoreDocQuery::new`]) or Lucene's `DocAndScoreQuery` (what a `knn`
/// query rewrites to; [`ScoreDocQuery::lucene`]). They differ only in their texts.
#[derive(Clone, Debug)]
pub struct ScoreDocQuery {
    /// Sorted by global doc id.
    pub docs: Arc<Vec<(u32, f32)>>,
    /// Lucene's `DocAndScoreQuery` texts ("within top N docs").
    pub lucene: bool,
}

impl ScoreDocQuery {
    /// Elasticsearch's `KnnScoreDocQuery`: explanations read "within top k documents".
    pub fn new(docs: Vec<(u32, f32)>) -> ScoreDocQuery {
        ScoreDocQuery { docs: Arc::new(Self::sorted(docs)), lucene: false }
    }

    /// Lucene's `DocAndScoreQuery`: explanations read "within top N docs" (N hits).
    pub fn lucene(docs: Vec<(u32, f32)>) -> ScoreDocQuery {
        ScoreDocQuery { docs: Arc::new(Self::sorted(docs)), lucene: true }
    }

    fn sorted(mut docs: Vec<(u32, f32)>) -> Vec<(u32, f32)> {
        docs.sort_by_key(|x| x.0);
        docs.dedup_by_key(|x| x.0);
        docs
    }
}

struct ScoreDocWeight {
    docs: Arc<Vec<(u32, f32)>>,
    boost: f32,
    lucene: bool,
}

struct ScoreDocScorer {
    docs: Arc<Vec<(u32, f32)>>,
    base: u32,
    idx: usize,
    end: usize,
    doc: u32,
    boost: f32,
}

impl Scorable for ScoreDocScorer {
    fn score(&mut self) -> f32 {
        self.docs[self.idx].1 * self.boost
    }
}

impl Scorer for ScoreDocScorer {
    fn doc(&self) -> u32 {
        self.doc
    }
    fn next(&mut self) -> u32 {
        if self.doc != u32::MAX {
            self.idx += 1;
        }
        self.doc = if self.idx < self.end { self.docs[self.idx].0 - self.base } else { NO_MORE_DOCS };
        self.doc
    }
    fn advance(&mut self, t: u32) -> u32 {
        loop {
            let d = self.next();
            if d >= t {
                return d;
            }
        }
    }
    fn cost(&self) -> u64 {
        (self.end - self.idx) as u64
    }
    fn max_score(&mut self, _upto: u32) -> f32 {
        self.docs[self.idx.min(self.end.saturating_sub(1))..self.end].iter().map(|x| x.1).fold(0.0, f32::max) * self.boost
    }
}

impl Weight for ScoreDocWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let lo = self.docs.partition_point(|x| x.0 < seg.base);
        let hi = self.docs.partition_point(|x| x.0 < seg.base + seg.max_doc());
        if lo == hi {
            return Ok(None);
        }
        Ok(Some(Box::new(ScoreDocScorer { docs: self.docs.clone(), base: seg.base, idx: lo, end: hi, doc: u32::MAX, boost: self.boost })))
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        let found = self.docs.binary_search_by_key(&(seg.base + doc), |x| x.0);
        Ok(match (found, self.lucene) {
            (Ok(i), false) => Explanation::leaf(self.docs[i].1 * self.boost, "within top k documents"),
            (Err(_), false) => Explanation::no_match("not in top k documents", vec![]),
            (Ok(i), true) => Explanation::leaf(self.docs[i].1 * self.boost, format!("within top {} docs", self.docs.len())),
            (Err(_), true) => Explanation::no_match(format!("not in top {} docs", self.docs.len()), vec![]),
        })
    }

    fn count(&self, seg: &SegmentContext) -> Result<Option<u32>> {
        let lo = self.docs.partition_point(|x| x.0 < seg.base);
        let hi = self.docs.partition_point(|x| x.0 < seg.base + seg.max_doc());
        Ok(Some((hi - lo) as u32))
    }
}

impl Query for ScoreDocQuery {
    fn weight(&self, _s: &Searcher, _mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        Ok(Box::new(ScoreDocWeight { docs: self.docs.clone(), boost, lucene: self.lucene }))
    }

    fn lucene_string(&self, _schema: &Schema) -> String {
        match (self.lucene, self.docs.first()) {
            (true, Some(&(d, sc))) => {
                let max = self.docs.iter().map(|x| x.1).fold(f32::NEG_INFINITY, f32::max);
                format!("DocAndScoreQuery[{d},...][{},...],{}", float_to_string(sc), float_to_string(max))
            }
            _ => "ScoreAndDocQuery".into(),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
