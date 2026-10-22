//! Compound retrievers (`rrf`, `linear`; `CompoundRetrieverBuilder`): each child retriever
//! runs as a search of `rank_window_size` hits on the same shards (ranks only, no fetch);
//! their rankings fuse into scores per document ([`Fusion::combine`]); the request
//! then runs once more with those documents injected per shard as score-doc queries, and with
//! the children's queries at boost 0 as the query, so `hits.total` and aggregations cover
//! everything the children match while the hits are the fused documents, best first.
//!
//! Children may be compound themselves: a child search with a fusion of its own runs the
//! same way. Ties order by shard, then document (Elasticsearch's `RankDoc` order).

use super::retriever::Fusion;
use super::{SearchOutcome, SearchRequest, Target};
use crate::cluster::Cluster;
use crate::error::EsResult;
use crate::search_source::SearchSource;
use search_index::QueryRef;
use std::sync::Arc;

/// The fused documents, per target, and how many hits may come from them.
#[derive(Debug)]
pub struct Fused {
    /// Per target: one score-doc query of the target's fused documents.
    pub per_target: Vec<Vec<QueryRef>>,
    /// The fused documents' count: the hits stop there.
    pub cap: usize,
}

/// The queries a child matches documents with (for the total and aggregations).
fn child_queries(s: &SearchSource, out: &mut Vec<crate::json::Value>) {
    if let Some(f) = &s.fusion {
        for c in &f.children {
            child_queries(c, out);
        }
        return;
    }
    if let Some(q) = &s.query {
        out.push(q.to_value());
    }
}

/// Runs a request whose source is a compound retriever.
pub fn execute(cluster: &Cluster, req: &SearchRequest, targets: &[Target], f: &Fusion) -> EsResult<SearchOutcome> {
    let started = std::time::Instant::now();
    // Each child: its window of hits, ranked.
    let mut ranked: Vec<Vec<((usize, u32), f32)>> = Vec::with_capacity(f.children.len());
    for child in &f.children {
        let mut creq = req.clone();
        creq.source = child.clone();
        creq.source.from = Some(0);
        creq.source.size = Some(f.window.max(0) as i32);
        creq.source.track_total_hits = Some(crate::search_source::TrackTotalHits::Disabled);
        creq.fused = None;
        creq.rank_only = true;
        let out = super::execute_on(cluster, &creq, targets)?;
        ranked.push(out.ranked.into_iter().map(|(t, d, s)| ((t, d), s)).collect());
    }
    let fused = f.combine(&ranked);
    // The fused documents per target; a doc with a zero fused score stays a hit.
    let mut per_target: Vec<Vec<(u32, f32)>> = vec![Vec::new(); targets.len()];
    for ((t, d), s) in &fused {
        per_target[*t].push((*d, if *s > 0.0 { *s } else { f32::from_bits(1) }));
    }
    let per_target: Vec<Vec<QueryRef>> =
        per_target.into_iter().map(|docs| if docs.is_empty() { Vec::new() } else { vec![super::query::arc(search_index::query::ScoreDocQuery::new(docs)) as QueryRef] }).collect();
    let mut freq = req.clone();
    freq.fused = Some(Arc::new(Fused { per_target, cap: fused.len() }));
    let mut src = req.source.clone();
    src.fusion = None;
    src.knn.clear();
    let mut qs = Vec::new();
    for c in &f.children {
        child_queries(c, &mut qs);
    }
    src.query = if qs.is_empty() {
        None
    } else {
        let v = crate::obj! { "bool" => crate::obj! { "should" => crate::json::Value::Arr(qs), "boost" => 0.0 } };
        Some(crate::query::parse_query(&v, None, &[])?.0)
    };
    // `min_score` keeps fused documents scoring at least that, and drops the rest.
    if let Some(m) = f.min_score {
        src.min_score = Some(m);
    }
    freq.source = src;
    let mut out = super::execute_on(cluster, &freq, targets)?;
    out.took_ms = started.elapsed().as_millis() as u64;
    if let Some(top) = fused.first() {
        out.max_score = Some(top.1.max(f32::from_bits(1)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use crate::search::retriever::{Fusion, FusionKind, Normalizer};

    fn fusion(kind: FusionKind) -> Fusion {
        Fusion { kind, window: 10, min_score: None, children: Vec::new() }
    }

    #[test]
    fn rrf_sums_reciprocal_ranks() {
        let f = fusion(FusionKind::Rrf { rank_constant: 1 });
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
