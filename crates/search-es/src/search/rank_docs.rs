//! The queries a compound retriever's final phase runs (Elasticsearch's `RankDocsQuery` and its
//! `TopQuery`), with their explanations.
//!
//! A compound retriever (`rrf`, `linear`) first ranks each child on its own and fuses the
//! rankings into "rank docs" (see [`super::fusion`]). The final search then runs, per shard, a
//! [`RankDocsQuery`]: its top part ([`TopQuery`]) matches exactly the shard's rank docs and
//! scores them with their fused score; its tail (a disjunction of what each child matches)
//! is a filter, so totals and aggregations cover every document a child matches while the
//! hits are the fused documents, best first (tail-only matches score 0).
//!
//! The top part keeps the children's queries ("sources") only to explain a hit: the rank doc's
//! own explanation (`RRFRankDoc.explain`, `LinearRankDoc.explain`, `RankDoc.explain`) wraps
//! each source's explanation of the document. Sources are converted per shard, so a source is
//! whatever that child contributes there: a `standard` child's query, a `knn` child's rank docs
//! (their explanation wraps the vector similarity), or a nested compound child's own
//! [`TopQuery`].
//!
//! Shapes, profile names and texts follow Elasticsearch 9.5.5: the profile tree reads
//! `RankDocsQuery` > (`TopQuery`, the tail `BooleanQuery`), and a rank-docs-only query rewrites
//! to its `TopQuery`.

use search_index::query::{BooleanQuery, Occur, ScoreDocQuery};
use search_index::search::{Explanation, Query, QueryRef, ScoreMode, Searcher, SegmentContext, Weight};
use search_index::util::java::float_to_string;
use std::any::Any;
use std::sync::Arc;

/// How a rank doc explains its score.
#[derive(Clone, Debug, PartialEq)]
pub enum RankKind {
    /// `RankDoc`: a single ranking (a `knn` retriever's results).
    Plain,
    /// `PinnedRankDoc`: a pinned document (scored above every organic one) explains as such.
    Pinned,
    /// `RRFRankDoc`.
    Rrf { rank_constant: i64 },
    /// `LinearRankDoc`: per child its weight and normalizer name.
    Linear { weights: Vec<f32>, normalizers: Vec<&'static str> },
    /// OpenSearch's hybrid query (`HybridQueryWeight.explain`): the sub-queries' explanations
    /// under "combined score of:", valued at the best matching one.
    Hybrid,
}

/// One rank doc of a shard.
#[derive(Clone, Debug, PartialEq)]
pub struct RankDoc {
    /// The shard searcher's global doc id.
    pub doc: u32,
    pub score: f32,
    /// 1-based rank in the fused results (all shards).
    pub rank: u32,
    /// Per child: `rrf` its 0-based position, `linear` its normalized score; `None` when the
    /// child didn't rank the doc.
    pub per_child: Vec<Option<(u32, f32)>>,
}

/// `RankDocsQuery.TopQuery`: matches the rank docs, scored by their fused score.
#[derive(Clone, Debug)]
pub struct TopQuery {
    /// Sorted by doc.
    pub docs: Arc<Vec<RankDoc>>,
    pub kind: Arc<RankKind>,
    pub sources: Vec<QueryRef>,
    pub names: Vec<Option<String>>,
}

impl TopQuery {
    pub fn new(mut docs: Vec<RankDoc>, kind: RankKind, sources: Vec<QueryRef>, names: Vec<Option<String>>) -> TopQuery {
        docs.sort_by_key(|d| d.doc);
        docs.dedup_by_key(|d| d.doc);
        TopQuery { docs: Arc::new(docs), kind: Arc::new(kind), sources, names }
    }

    /// The rank doc's explanation over its sources' explanations.
    fn explain_doc(&self, d: &RankDoc, sources: Vec<Explanation>) -> Explanation {
        let alias = |i: usize| match self.names.get(i).and_then(|n| n.as_deref()) {
            Some(n) => format!("at index [{i}] [{n}]"),
            None => format!("at index [{i}]"),
        };
        match &*self.kind {
            RankKind::Hybrid => {
                let best = sources.iter().filter(|e| e.is_match()).map(|e| f64::from(e.value())).reduce(f64::max);
                match best {
                    Some(max) => Explanation::matched(max.max(0.0), "combined score of:", sources),
                    None => Explanation::no_match("no matching clause", sources),
                }
            }
            RankKind::Pinned if d.score > super::extra::MAX_ORGANIC_SCORE => Explanation::matched(d.score, "Pinned document, original explanation:", sources),
            RankKind::Plain | RankKind::Pinned => Explanation::matched(
                d.rank as i32,
                format!("doc [{}] with an original score of [{}] is at rank [{}] from the following source queries.", d.doc, float_to_string(d.score), d.rank),
                sources,
            ),
            RankKind::Rrf { rank_constant } => {
                let mut details = Vec::with_capacity(sources.len());
                let mut ranks = Vec::with_capacity(sources.len());
                for (i, src) in sources.into_iter().enumerate() {
                    match d.per_child.get(i).copied().flatten() {
                        None => {
                            ranks.push("0".to_string());
                            details.push(Explanation::no_match(format!("rrf score: [0], result not found in query {}", alias(i)), vec![]));
                        }
                        Some((pos, _)) => {
                            let rank = pos as i64 + 1;
                            ranks.push(rank.to_string());
                            let rrf = 1f32 / (rank + rank_constant) as f32;
                            details.push(Explanation::matched(
                                rank as i32,
                                format!(
                                    "rrf score: [{}], for rank [{rank}] in query {} computed as [1 / ({rank} + {rank_constant})], for matching query with score",
                                    float_to_string(rrf),
                                    alias(i)
                                ),
                                vec![src],
                            ));
                        }
                    }
                }
                Explanation::matched(
                    d.score,
                    format!(
                        "rrf score: [{}] computed for initial ranks [{}] with rankConstant: [{rank_constant}] as sum of [1 / (rank + rankConstant)] for each query",
                        float_to_string(d.score),
                        ranks.join(", ")
                    ),
                    details,
                )
            }
            RankKind::Linear { weights, normalizers } => {
                let mut details = Vec::with_capacity(sources.len());
                let mut normalized = Vec::with_capacity(sources.len());
                for (i, src) in sources.into_iter().enumerate() {
                    let ns = d.per_child.get(i).copied().flatten().map_or(0.0, |(_, s)| s);
                    normalized.push(float_to_string(ns));
                    let w = weights.get(i).copied().unwrap_or(1.0);
                    let normalizer = normalizers.get(i).copied().unwrap_or("none");
                    if ns > 0.0 {
                        details.push(Explanation::matched(
                            w * ns,
                            format!(
                                "weighted score: [{}] in query {} computed as [{} * {}] using score normalizer [{normalizer}] for original matching query with score:",
                                float_to_string(w * ns),
                                alias(i),
                                float_to_string(w),
                                float_to_string(ns)
                            ),
                            vec![src],
                        ));
                    } else {
                        details.push(Explanation::no_match(format!("weighted score: [0], result not found in query {}", alias(i)), vec![]));
                    }
                }
                let ws: Vec<String> = weights.iter().map(|w| float_to_string(*w)).collect();
                Explanation::matched(
                    d.score,
                    format!(
                        "weighted linear combination score: [{}] computed for normalized scores [{}] and weights [{}] as sum of (weight[i] * score[i]) for each query.",
                        float_to_string(d.score),
                        normalized.join(", "),
                        ws.join(", ")
                    ),
                    details,
                )
            }
        }
    }
}

struct TopWeight {
    query: TopQuery,
    inner: Box<dyn Weight>,
    sources: Vec<Box<dyn Weight>>,
}

impl Weight for TopWeight {
    fn scorer(&self, seg: &SegmentContext) -> search_index::Result<Option<Box<dyn search_index::search::Scorer>>> {
        self.inner.scorer(seg)
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> search_index::Result<Explanation> {
        let docs = &self.query.docs;
        let Ok(i) = docs.binary_search_by_key(&(seg.base + doc), |d| d.doc) else {
            return Ok(Explanation::no_match(format!("doc not found in top {} rank docs", docs.len()), vec![]));
        };
        let mut sources = Vec::with_capacity(self.sources.len());
        for w in &self.sources {
            sources.push(w.explain(seg, doc)?);
        }
        Ok(self.query.explain_doc(&docs[i], sources))
    }

    fn count(&self, seg: &SegmentContext) -> search_index::Result<Option<u32>> {
        self.inner.count(seg)
    }
}

impl Query for TopQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> search_index::Result<Box<dyn Weight>> {
        // A rank doc always scores above zero, so it stays apart from tail-only matches.
        let scored: Vec<(u32, f32)> = self.docs.iter().map(|d| (d.doc, d.score.max(f32::from_bits(1)))).collect();
        let inner = Arc::new(ScoreDocQuery::new(scored)).weight(s, mode, boost)?;
        let mut sources = Vec::with_capacity(self.sources.len());
        for q in &self.sources {
            sources.push(q.weight(s, mode, boost)?);
        }
        Ok(Box::new(TopWeight { query: self.clone(), inner, sources }))
    }

    fn rewrite(&self, s: &Searcher) -> search_index::Result<Option<QueryRef>> {
        let mut changed = false;
        let mut sources = Vec::with_capacity(self.sources.len());
        for q in &self.sources {
            let r = s.rewrite(q)?;
            changed |= !Arc::ptr_eq(&r, q);
            sources.push(r);
        }
        Ok(changed.then(|| Arc::new(TopQuery { sources, ..self.clone() }) as QueryRef))
    }

    /// `TopQuery.toString`: the rank docs (clause deduplication tells queries apart by it).
    fn lucene_string(&self, _schema: &search_index::Schema) -> String {
        format!("TopQuery{{rank_docs:{}}}", rank_docs_string(&self.docs))
    }

    fn profile_name(&self) -> String {
        "TopQuery".into()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `Arrays.toString` of `RankDoc`s.
fn rank_docs_string(docs: &[RankDoc]) -> String {
    let parts: Vec<String> = docs.iter().map(|d| format!("RankDoc{{_rank={}, _doc={}, _score={}}}", d.rank, d.doc, float_to_string(d.score))).collect();
    format!("[{}]", parts.join(", "))
}

/// `RankDocsQuery`: the top docs scored, every tail match counted.
#[derive(Clone, Debug)]
pub struct RankDocsQuery {
    pub top: Arc<TopQuery>,
    /// What the children match (`None`: only the rank docs match).
    pub tail: Option<QueryRef>,
}

impl RankDocsQuery {
    /// The final-phase query: the rank docs plus every match of their sources.
    pub fn new(top: TopQuery) -> RankDocsQuery {
        if *top.kind == RankKind::Hybrid && !top.sources.is_empty() {
            let tail = crate::opensearch::hybrid::union(top.sources.clone());
            return RankDocsQuery { top: Arc::new(top), tail: Some(tail) };
        }
        let tail = (!top.sources.is_empty()).then(|| {
            let mut b = BooleanQuery::new();
            for s in &top.sources {
                b = b.add(Occur::Should, s.clone());
            }
            Arc::new(b) as QueryRef
        });
        RankDocsQuery { top: Arc::new(top), tail }
    }
}

struct RankDocsWeight {
    top: Box<dyn Weight>,
    combined: Box<dyn Weight>,
}

impl Weight for RankDocsWeight {
    fn scorer(&self, seg: &SegmentContext) -> search_index::Result<Option<Box<dyn search_index::search::Scorer>>> {
        self.combined.scorer(seg)
    }

    fn scorer_supplier(&self, seg: &SegmentContext) -> search_index::Result<Option<Box<dyn search_index::search::ScorerSupplier>>> {
        self.combined.scorer_supplier(seg)
    }

    fn bulk_scorer(&self, seg: &SegmentContext, mode: ScoreMode) -> search_index::Result<Option<Box<dyn search_index::search::BulkScorer>>> {
        self.combined.bulk_scorer(seg, mode)
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> search_index::Result<Explanation> {
        self.top.explain(seg, doc)
    }

    fn count(&self, seg: &SegmentContext) -> search_index::Result<Option<u32>> {
        self.combined.count(seg)
    }
}

impl Query for RankDocsQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> search_index::Result<Box<dyn Weight>> {
        let Some(tail) = &self.tail else { return self.top.weight(s, mode, boost) };
        // Built directly (not through the searcher), as `RankDocsQuery.createWeight` does: a
        // profile shows the top query and the tail as this query's children.
        let top = self.top.weight(s, mode, boost)?;
        let combined = BooleanQuery::new().add(Occur::Should, self.top.clone() as QueryRef).add(Occur::Filter, tail.clone());
        let combined = combined.weight(s, mode, boost)?;
        Ok(Box::new(RankDocsWeight { top, combined }))
    }

    fn rewrite(&self, s: &Searcher) -> search_index::Result<Option<QueryRef>> {
        let Some(tail) = &self.tail else { return Ok(Some(self.top.clone() as QueryRef)) };
        let top = s.rewrite(&(self.top.clone() as QueryRef))?;
        let new_tail = s.rewrite(tail)?;
        let top_changed = !Arc::ptr_eq(&top, &(self.top.clone() as QueryRef));
        if !top_changed && Arc::ptr_eq(&new_tail, tail) {
            return Ok(None);
        }
        let top = match top.as_any().downcast_ref::<TopQuery>() {
            Some(t) => Arc::new(t.clone()),
            None => self.top.clone(),
        };
        Ok(Some(Arc::new(RankDocsQuery { top, tail: Some(new_tail) })))
    }

    fn lucene_string(&self, _schema: &search_index::Schema) -> String {
        format!("RankDocsQuery{{rank_docs:{}}}", rank_docs_string(&self.top.docs))
    }

    /// The tail's terms (highlighting): the top query is a leaf.
    fn visit(&self, v: &mut dyn search_index::search::QueryVisitor) {
        if let Some(t) = &self.tail {
            t.visit(v);
        }
    }

    fn profile_name(&self) -> String {
        "RankDocsQuery".into()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A `knn` child's source when explaining (`ExactKnnQueryBuilder` → `DenseVectorQuery`): the
/// documents it ranked, each with its similarity.
#[derive(Clone, Debug)]
pub struct SimilarityQuery {
    /// Sorted by doc.
    pub docs: Arc<Vec<(u32, f32)>>,
    pub field: String,
}

struct SimilarityWeight {
    q: SimilarityQuery,
    inner: Box<dyn Weight>,
    boost: f32,
}

impl Weight for SimilarityWeight {
    fn scorer(&self, seg: &SegmentContext) -> search_index::Result<Option<Box<dyn search_index::search::Scorer>>> {
        self.inner.scorer(seg)
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> search_index::Result<Explanation> {
        Ok(match self.q.docs.binary_search_by_key(&(seg.base + doc), |d| d.0) {
            Ok(i) => {
                let s = self.q.docs[i].1;
                Explanation::leaf(s * self.boost, format!("found vector with calculated similarity: {}", float_to_string(s)))
            }
            Err(_) => Explanation::no_match(format!("Document not found in vector values for field: {}", self.q.field), vec![]),
        })
    }
}

impl Query for SimilarityQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> search_index::Result<Box<dyn Weight>> {
        let inner = Arc::new(ScoreDocQuery::new(self.docs.to_vec())).weight(s, mode, boost)?;
        Ok(Box::new(SimilarityWeight { q: self.clone(), inner, boost }))
    }

    fn lucene_string(&self, _schema: &search_index::Schema) -> String {
        let docs: Vec<String> = self.docs.iter().map(|(d, s)| format!("{d}:{}", float_to_string(*s))).collect();
        format!("ExactKnnQuery[{}][{}]", self.field, docs.join(", "))
    }

    fn profile_name(&self) -> String {
        "DenseVectorQuery.Floats".into()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(doc: u32, score: f32, rank: u32, per_child: Vec<Option<(u32, f32)>>) -> RankDoc {
        RankDoc { doc, score, rank, per_child }
    }

    #[test]
    fn rrf_explanation_texts() {
        // From Elasticsearch's 500_rrf_retriever_explain: ranks [2, 1], rank constant 1.
        let q = TopQuery::new(vec![], RankKind::Rrf { rank_constant: 1 }, vec![], vec![None, Some("my_top_knn".into())]);
        let d = doc(7, 0.8333334, 1, vec![Some((1, 3.0)), Some((0, 0.9))]);
        let e = q.explain_doc(&d, vec![Explanation::leaf(3.0f32, "weight(text:term)"), Explanation::leaf(1.0f32, "knn")]);
        assert_eq!(e.description, "rrf score: [0.8333334] computed for initial ranks [2, 1] with rankConstant: [1] as sum of [1 / (rank + rankConstant)] for each query");
        assert_eq!(e.details[0].value, 2i32.into());
        assert_eq!(
            e.details[0].description,
            "rrf score: [0.33333334], for rank [2] in query at index [0] computed as [1 / (2 + 1)], for matching query with score"
        );
        assert_eq!(e.details[1].description, "rrf score: [0.5], for rank [1] in query at index [1] [my_top_knn] computed as [1 / (1 + 1)], for matching query with score");
        let d = doc(7, 0.5, 3, vec![Some((0, 3.0)), None]);
        let e = q.explain_doc(&d, vec![Explanation::leaf(3.0f32, "w"), Explanation::leaf(0.0f32, "x")]);
        assert!(e.description.contains("initial ranks [1, 0]"));
        assert!(!e.details[1].matched);
        assert_eq!(e.details[1].description, "rrf score: [0], result not found in query at index [1] [my_top_knn]");
        assert!(e.details[1].details.is_empty());
    }

    #[test]
    fn linear_and_plain_explanation_texts() {
        let kind = RankKind::Linear { weights: vec![10.0, 20.0], normalizers: vec!["minmax", "none"] };
        let q = TopQuery::new(vec![], kind, vec![], vec![Some("my_standard_retriever".into()), None]);
        let d = doc(1, 20.0, 1, vec![None, Some((0, 1.0))]);
        let e = q.explain_doc(&d, vec![Explanation::leaf(0.0f32, "a"), Explanation::leaf(5.0f32, "b")]);
        assert_eq!(
            e.description,
            "weighted linear combination score: [20.0] computed for normalized scores [0.0, 1.0] and weights [10.0, 20.0] as sum of (weight[i] * score[i]) for each query."
        );
        assert_eq!(e.details[0].description, "weighted score: [0], result not found in query at index [0] [my_standard_retriever]");
        assert_eq!(
            e.details[1].description,
            "weighted score: [20.0] in query at index [1] computed as [20.0 * 1.0] using score normalizer [none] for original matching query with score:"
        );
        let q = TopQuery::new(vec![], RankKind::Plain, vec![], vec![]);
        let e = q.explain_doc(&doc(4, 0.25, 2, vec![]), vec![]);
        assert_eq!(e.description, "doc [4] with an original score of [0.25] is at rank [2] from the following source queries.");
        assert_eq!(e.value, 2i32.into());
    }
}
