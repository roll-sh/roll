//! Compound retrievers (`rrf`, `linear`; `CompoundRetrieverBuilder`): each child retriever
//! runs as a search of `rank_window_size` hits on the same shards (ranks only, no fetch);
//! their rankings fuse into scores per document ([`Fusion::combine_detailed`]); the request
//! then runs once more with, per shard, a [`RankDocsQuery`] of those documents as the query:
//! the fused documents are the hits, best first, while `hits.total` and aggregations cover
//! everything the children match (see [`super::rank_docs`]).
//!
//! Children may be compound themselves: they rank recursively ([`rank`]) and keep their own
//! fused documents, so explanations nest as Elasticsearch's do. Ties order by shard, then
//! document (Elasticsearch's `RankDoc` order).

use super::query::QueryCtx;
use super::rank_docs::{RankDoc, RankDocsQuery, RankKind, SimilarityQuery, SumOf, TopQuery};
use super::retriever::{Fusion, FusionKind};
use super::{SearchOutcome, SearchRequest, Target};
use crate::cluster::Cluster;
use crate::error::EsResult;
use crate::json::Value;
use search_index::QueryRef;
use std::sync::Arc;

/// A child of a fused ranking, as the final phase sees it.
#[derive(Debug)]
pub enum Child {
    /// A `standard` retriever: its `topDocsQuery`.
    Query(Value),
    /// An already parsed query (an OpenSearch `hybrid` query's sub-query).
    Parsed(crate::query::Query),
    /// A retriever whose matches are its ranked documents (`knn`): (target, doc, score), best
    /// first, and the vector field.
    Ranked { ranked: Vec<(usize, u32, f32)>, field: String },
    /// A nested compound retriever.
    Compound(Node),
    /// A `pinned` retriever's child: its ranked documents (from the `pinned` query), its own
    /// query (explained) and the `pinned` query (what it matches). Under a `rule` retriever
    /// (`rule`), the `rule` query is what explains.
    Wrapped { ranked: Vec<(usize, u32, f32)>, organic: Value, pinned: Value, rule: bool },
    /// A child less the documents a `rule` retriever's rules exclude (a query matching them).
    Excluding { inner: Box<Child>, excluded: Value },
}

/// A compound retriever's fused ranking.
#[derive(Debug)]
pub struct Node {
    pub kind: RankKind,
    pub names: Vec<Option<String>>,
    /// Best first: (target, doc), fused score, per child (position, score).
    pub fused: Vec<super::retriever::FusedDoc<(usize, u32)>>,
    pub children: Vec<Child>,
}

/// The fused ranking the final phase runs with.
#[derive(Debug)]
pub struct Fused {
    pub node: Arc<Node>,
    /// The fused documents' count: the hits stop there.
    pub cap: usize,
}

impl Node {
    /// The rank docs of target `ti`.
    fn docs(&self, ti: usize) -> Vec<RankDoc> {
        self.fused
            .iter()
            .enumerate()
            .filter(|(_, d)| d.key.0 == ti)
            .map(|(i, d)| RankDoc { doc: d.key.1, score: d.score, rank: i as u32 + 1, per_child: d.per_child.clone() })
            .collect()
    }

    /// `RankDocsRetrieverBuilder.explainQuery` (`explain`) or the top query whose sources are
    /// the children's `topDocsQuery`.
    fn top(&self, qctx: &QueryCtx, ti: usize, explain: bool) -> EsResult<TopQuery> {
        let mut sources = Vec::with_capacity(self.children.len());
        for c in &self.children {
            sources.push(c.source(qctx, ti, explain)?);
        }
        Ok(TopQuery::new(self.docs(ti), self.kind.clone(), sources, self.names.clone()))
    }
}

impl Child {
    /// What the child contributes on target `ti`: `explainQuery` when explaining, else
    /// `topDocsQuery`.
    fn source(&self, qctx: &QueryCtx, ti: usize, explain: bool) -> EsResult<QueryRef> {
        Ok(match self {
            Child::Query(v) => {
                let (q, _) = crate::query::parse_query(v, None, &[])?;
                qctx.to_query(&q)?
            }
            Child::Parsed(q) => qctx.to_query(q)?,
            Child::Ranked { ranked, field } => {
                let mine: Vec<(usize, &(usize, u32, f32))> = ranked.iter().enumerate().filter(|(_, r)| r.0 == ti).collect();
                let docs = mine.iter().map(|(i, r)| RankDoc { doc: r.1, score: r.2, rank: *i as u32 + 1, per_child: Vec::new() }).collect();
                let sources = if explain {
                    let mut sim: Vec<(u32, f32)> = mine.iter().map(|(_, r)| (r.1, r.2)).collect();
                    sim.sort_by_key(|x| x.0);
                    vec![Arc::new(SimilarityQuery { docs: Arc::new(sim), field: field.clone() }) as QueryRef]
                } else {
                    Vec::new()
                };
                Arc::new(TopQuery::new(docs, RankKind::Plain, sources, vec![None]))
            }
            Child::Compound(node) if explain => Arc::new(node.top(qctx, ti, true)?),
            // `PinnedRetrieverBuilderWrapper.explainQuery`: the child's rank docs over its query;
            // under a `rule` retriever, over the `rule` query (`QueryRuleRetrieverBuilder`).
            Child::Wrapped { ranked, organic, pinned, rule } if explain => {
                let docs = ranked.iter().enumerate().filter(|(_, r)| r.0 == ti).map(|(i, r)| RankDoc { doc: r.1, score: r.2, rank: i as u32 + 1, per_child: Vec::new() }).collect();
                let source = if *rule {
                    let (q, _) = crate::query::parse_query(pinned, None, &[])?;
                    Arc::new(SumOf(qctx.to_query(&q)?)) as QueryRef
                } else {
                    let (q, _) = crate::query::parse_query(organic, None, &[])?;
                    qctx.to_query(&q)?
                };
                Arc::new(TopQuery::new(docs, RankKind::Plain, vec![source], vec![None]))
            }
            Child::Excluding { inner, .. } if explain => inner.source(qctx, ti, true)?,
            Child::Excluding { inner, excluded } => {
                let (q, _) = crate::query::parse_query(excluded, None, &[])?;
                let b = search_index::query::BooleanQuery::new()
                    .add(search_index::query::Occur::Must, inner.source(qctx, ti, false)?)
                    .add(search_index::query::Occur::MustNot, qctx.to_query(&q)?);
                Arc::new(b)
            }
            Child::Wrapped { pinned, .. } => {
                let (q, _) = crate::query::parse_query(pinned, None, &[])?;
                qctx.to_query(&q)?
            }
            // `RankDocsRetrieverBuilder.topDocsQuery`: what any of its children match.
            Child::Compound(node) => {
                let mut b = search_index::query::BooleanQuery::new();
                for c in &node.children {
                    b = b.add(search_index::query::Occur::Should, c.source(qctx, ti, false)?);
                }
                Arc::new(b)
            }
        })
    }
}

impl Fused {
    /// The final phase's query on target `ti`.
    pub fn query(&self, qctx: &QueryCtx, ti: usize, explain: bool) -> EsResult<QueryRef> {
        Ok(Arc::new(RankDocsQuery::new(self.node.top(qctx, ti, explain)?)))
    }
}

/// Ranks each child of `f` and fuses the rankings.
pub fn rank(cluster: &Cluster, req: &SearchRequest, targets: &[Target], f: &Fusion) -> EsResult<Node> {
    let mut ranked: Vec<Vec<((usize, u32), f32)>> = Vec::with_capacity(f.children.len());
    let mut children = Vec::with_capacity(f.children.len());
    for (i, child) in f.children.iter().enumerate() {
        if let Some(inner) = &child.fusion {
            let node = crate::stack::maybe_grow(|| rank(cluster, req, targets, inner))?;
            // The child's search returns at most this retriever's window of its rank docs.
            ranked.push(node.fused.iter().take(f.window.max(0) as usize).map(|d| (d.key, d.score)).collect());
            children.push(Child::Compound(node));
            continue;
        }
        let tail = f.tails.get(i).cloned().flatten();
        let window = f.window.max(0) as usize;
        let mut creq = req.clone();
        creq.source = child.clone();
        creq.source.from = Some(0);
        creq.source.size = Some(window.min(i32::MAX as usize) as i32);
        creq.source.track_total_hits = Some(crate::search_source::TrackTotalHits::Disabled);
        creq.source.explain = None;
        creq.source.profile = false;
        // `LinearRetrieverBuilder.finalizeSourceBuilder`: sorted children still score.
        if matches!(f.kind, FusionKind::Linear { .. }) {
            creq.source.track_scores = true;
        }
        creq.fused = None;
        creq.rank_only = true;
        let out = super::execute_on(cluster, &creq, targets)?;
        ranked.push(out.ranked.iter().take(window).map(|(t, d, s)| ((*t, *d), *s)).collect());
        children.push(match tail {
            Some(v) if matches!(f.kind, FusionKind::Pinned { organic: Some(_), .. }) => {
                let FusionKind::Pinned { organic: Some(o), rule, .. } = &f.kind else { unreachable!() };
                Child::Wrapped { ranked: out.ranked, organic: o.clone(), pinned: v, rule: rule.is_some() }
            }
            Some(v) => Child::Query(v),
            None => {
                let field = child.knn.first().and_then(|k| k.get("field")).map(Value::to_es_string).unwrap_or_default();
                Child::Ranked { ranked: out.ranked, field }
            }
        });
    }
    if let FusionKind::Pinned { pins, organic: None, rule } = &f.kind {
        let mut pins = pins.clone();
        if let Some(rule) = rule {
            // `rule` over a compound retriever: its rules run on the child's rank docs.
            let actions = crate::xpack::entsearch::query_rules::actions(&cluster.state(), &rule.ruleset_ids, &rule.match_criteria)?;
            pins = actions.pins();
            let excluded = actions.excluded();
            if !excluded.is_empty() {
                let gone: std::collections::HashSet<(usize, u32)> = find_docs(targets, &excluded)?.into_iter().map(|(d, _)| d).collect();
                if let Some(first) = ranked.first_mut() {
                    first.retain(|(d, _)| !gone.contains(d));
                }
                if let Some(Child::Compound(node)) = children.first_mut() {
                    node.fused.retain(|d| !gone.contains(&d.key));
                }
                let inner = children.remove(0);
                children.insert(0, Child::Excluding { inner: Box::new(inner), excluded: docs_query(&excluded) });
            }
        }
        if !pins.is_empty() {
            // The pinned documents rank ahead of the child's (`PinnedQueryBuilder`); they also
            // match, child or not.
            ranked.push(pinned_docs(targets, &pins)?);
            let ids: Vec<Value> = pins.iter().map(|(_, id)| Value::Str(id.clone())).collect();
            children.push(Child::Query(crate::obj! { "ids" => crate::obj! { "values" => Value::Arr(ids) } }));
        }
    }
    let kind = match &f.kind {
        FusionKind::Pinned { rule: Some(rule), .. } => RankKind::Rule(rule.description()),
        FusionKind::Pinned { .. } => RankKind::Pinned,
        FusionKind::Rrf { rank_constant, .. } => RankKind::Rrf { rank_constant: *rank_constant },
        FusionKind::Linear { weights, normalizers } => RankKind::Linear { weights: weights.clone(), normalizers: normalizers.iter().map(|n| n.name()).collect() },
    };
    let mut names = f.names.clone();
    names.resize(children.len(), None);
    Ok(Node { kind, names, fused: f.combine_detailed(&ranked), children })
}

/// The pinned documents with their pin scores (the floats right above the organic cap,
/// highest first): `(target, doc)`, score.
fn pinned_docs(targets: &[Target], pins: &[(Option<String>, String)]) -> EsResult<Vec<((usize, u32), f32)>> {
    let top = super::extra::MAX_ORGANIC_SCORE.to_bits() + 1 + pins.len() as u32;
    Ok(find_docs(targets, pins)?.into_iter().map(|(d, i)| (d, f32::from_bits(top - i as u32))).collect())
}

/// A query matching the documents `docs` names (`(index, id)`).
fn docs_query(docs: &[(Option<String>, String)]) -> Value {
    let should: Vec<Value> = docs
        .iter()
        .map(|(index, id)| {
            let mut filter = vec![crate::obj! { "ids" => crate::obj! { "values" => crate::arr![id.clone()] } }];
            if let Some(i) = index {
                filter.push(crate::obj! { "term" => crate::obj! { "_index" => i.clone() } });
            }
            crate::obj! { "bool" => crate::obj! { "filter" => Value::Arr(filter) } }
        })
        .collect();
    crate::obj! { "bool" => crate::obj! { "should" => Value::Arr(should) } }
}

/// The live documents `docs` names (`(index, id)`), in that order: `(target, doc)` and the
/// position in `docs`.
fn find_docs(targets: &[Target], docs: &[(Option<String>, String)]) -> EsResult<Vec<((usize, u32), usize)>> {
    let ie = crate::engine::convert::index_error;
    let mut out = Vec::new();
    for (pos, (index, id)) in docs.iter().enumerate() {
        let q: QueryRef = Arc::new(search_index::query::TermQuery::new(search_index::schema::ID_FIELD, id.as_bytes().to_vec()));
        for (ti, t) in targets.iter().enumerate() {
            if index.as_deref().is_some_and(|i| i != t.idx.name) {
                continue;
            }
            let searcher = t.searcher()?;
            let w = searcher.create_weight(&q, search_index::ScoreMode::CompleteNoScores).map_err(ie)?;
            for seg in searcher.segments() {
                let Some(mut sc) = w.scorer(seg).map_err(ie)? else { continue };
                let mut d = sc.next();
                while d != search_codec::NO_MORE_DOCS {
                    if seg.is_live(d) {
                        out.push(((ti, seg.base + d), pos));
                        break;
                    }
                    d = sc.next();
                }
            }
        }
    }
    Ok(out)
}

/// `RetrieverBuilder.hasMinScore` over the retriever tree.
fn has_min_score(f: &Fusion) -> bool {
    f.min_score.is_some() || f.children.iter().any(|c| c.min_score.is_some() || c.fusion.as_deref().is_some_and(has_min_score))
}

/// Runs a request whose source is a compound retriever.
pub fn execute(cluster: &Cluster, req: &SearchRequest, targets: &[Target], f: &Fusion) -> EsResult<SearchOutcome> {
    let started = std::time::Instant::now();
    let node = rank(cluster, req, targets, f)?;
    let top = node.fused.first().map(|d| d.score);
    let mut freq = req.clone();
    freq.fused = Some(Arc::new(Fused { cap: node.fused.len(), node: Arc::new(node) }));
    let mut src = req.source.clone();
    src.fusion = None;
    src.knn.clear();
    src.query = None;
    // `min_score` keeps fused documents scoring at least that, and drops the rest; one on a
    // descendant keeps the rank docs only (`RankDocsRetrieverBuilder.hasMinScore`).
    if has_min_score(f) {
        src.min_score = Some(f.min_score.unwrap_or(f32::from_bits(1)));
    }
    freq.source = src;
    let mut out = super::execute_on(cluster, &freq, targets)?;
    out.took_ms = started.elapsed().as_millis() as u64;
    if let Some(top) = top {
        out.max_score = Some(top.max(f32::from_bits(1)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use crate::search::retriever::{Fusion, FusionKind, Normalizer};

    fn fusion(kind: FusionKind) -> Fusion {
        Fusion { kind, window: 10, min_score: None, children: Vec::new(), names: Vec::new(), tails: Vec::new() }
    }

    #[test]
    fn rrf_sums_reciprocal_ranks() {
        let f = fusion(FusionKind::Rrf { rank_constant: 1, weights: vec![] });
        // Child 1 ranks 2, 1, 5; child 2 ranks 4, 5, 3.
        let a = vec![(2, 1.0f32), (1, 0.5), (5, 0.5)];
        let b = vec![(4, 1.0f32), (5, 0.5), (3, 0.3)];
        let out = f.combine(&[a, b]);
        let ids: Vec<i32> = out.iter().map(|(k, _)| *k).collect();
        // 5: 1/4 + 1/3; 2 and 4: 1/2; 1 and 3: 1/3 (ties by key).
        assert_eq!(ids, vec![5, 2, 4, 1, 3]);
        assert!((out[0].1 - (0.25 + 1.0 / 3.0)).abs() < 1e-6);
    }

    #[test]
    fn linear_minmax_and_weights() {
        let f = fusion(FusionKind::Linear { weights: vec![2.0, 1.0], normalizers: vec![Normalizer::MinMax, Normalizer::None] });
        let a = vec![(2, 3.0f32), (1, 1.0), (5, 1.0)];
        let b = vec![(4, 1.0f32), (3, 0.33), (5, 0.33)];
        let out = f.combine(&[a, b]);
        assert_eq!(out[0], (2, 2.0));
        assert_eq!(out[1], (4, 1.0));
        // Docs 1: min of child 1 and absent from child 2.
        assert_eq!(out.iter().find(|(k, _)| *k == 1).unwrap().1, 0.0);
    }

    #[test]
    fn minmax_flat_scores_are_one() {
        let mut s = vec![0.5f32, 0.5];
        Normalizer::MinMax.apply(&mut s);
        assert_eq!(s, vec![1.0, 1.0]);
        let mut s = vec![3.0f32, 4.0];
        Normalizer::L2Norm.apply(&mut s);
        assert!((s[0] - 0.6).abs() < 1e-6 && (s[1] - 0.8).abs() < 1e-6);
    }
}
