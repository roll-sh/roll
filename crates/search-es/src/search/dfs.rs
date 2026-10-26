//! `dfs_query_then_fetch`: every shard scores with term and field statistics summed over all
//! the shards the search targets, as Elasticsearch's DFS phase collects them
//! (`DfsPhase` → `AggregatedDfs`) and its query phase reads them back (`CachedDfSource`).
//!
//! Elasticsearch's DFS phase builds the query's weight on every shard through a searcher that
//! records the statistics the weight asks for, sums them on the coordinator and ships the sums
//! back. Roll runs the same recording pass in process ([`Recorder`]): it learns which fields
//! each shard's weights request statistics for — Lucene's `PhraseWeight`, for one, asks only
//! when one of its terms exists on the shard, so an empty shard's documents are left out of
//! the field's document count. Term statistics are summed lazily over every shard (a shard
//! where a term is absent contributes zeros either way) and cached for the request.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;
use search_index::Searcher;
use search_index::similarity::{CollectionStats, TermStats};

use super::Target;
use crate::error::EsResult;

/// Statistics summed over the views of every shard of one search.
pub struct DfsStats {
    views: Vec<Searcher>,
    /// Per view, the fields its DFS-phase weights asked statistics for.
    requested: Vec<HashSet<u32>>,
    terms: Mutex<HashMap<(u32, Vec<u8>), TermStats>>,
    fields: Mutex<HashMap<u32, Option<CollectionStats>>>,
}

impl search_index::StatsSource for DfsStats {
    fn term_stats(&self, field: u32, term: &[u8]) -> Option<TermStats> {
        let key = (field, term.to_vec());
        if let Some(t) = self.terms.lock().get(&key) {
            return Some(*t);
        }
        let mut t = TermStats { doc_freq: 0, total_term_freq: 0 };
        for v in &self.views {
            let s = v.term_stats(field, term);
            t.doc_freq += s.doc_freq;
            t.total_term_freq += s.total_term_freq;
        }
        self.terms.lock().insert(key, t);
        Some(t)
    }

    fn collection_stats(&self, field: u32) -> Option<Option<CollectionStats>> {
        if let Some(c) = self.fields.lock().get(&field) {
            return Some(*c);
        }
        let mut sum: Option<CollectionStats> = None;
        // A field no recorded weight asked for (asked for only after the DFS phase): every shard.
        let any = self.requested.iter().any(|r| r.contains(&field));
        for (v, r) in self.views.iter().zip(&self.requested) {
            if any && !r.contains(&field) {
                continue;
            }
            if let Some(c) = v.collection_stats(field) {
                let s = sum.get_or_insert(CollectionStats { max_doc: 0, doc_count: 0, sum_total_term_freq: 0, sum_doc_freq: 0 });
                s.max_doc += c.max_doc;
                s.doc_count += c.doc_count;
                s.sum_total_term_freq += c.sum_total_term_freq;
                s.sum_doc_freq += c.sum_doc_freq;
            }
        }
        self.fields.lock().insert(field, sum);
        Some(sum)
    }
}

/// The DFS phase's recording searcher: the view's own statistics, noting the fields asked for.
#[derive(Default)]
struct Recorder {
    fields: Mutex<HashSet<u32>>,
}

impl search_index::StatsSource for Recorder {
    fn term_stats(&self, _field: u32, _term: &[u8]) -> Option<TermStats> {
        None
    }

    fn collection_stats(&self, field: u32) -> Option<Option<CollectionStats>> {
        self.fields.lock().insert(field);
        None
    }
}

/// Shards (by target position) whose query failed to build in the DFS phase.
pub type DfsFailures = Vec<(usize, crate::error::EsError)>;

/// The targets with their current views pinned and scoring with the summed statistics, and the
/// shards whose query failed to build (Elasticsearch fails those in its DFS phase).
pub fn with_global_stats(cluster: &crate::cluster::Cluster, req: &super::SearchRequest, targets: &[Target]) -> EsResult<(Vec<Target>, DfsFailures)> {
    let st = cluster.state();
    let mut views = Vec::with_capacity(targets.len());
    let mut requested = Vec::with_capacity(targets.len());
    let mut failures = Vec::new();
    for (i, t) in targets.iter().enumerate() {
        let view = t.searcher()?.with_global_stats(None);
        let rec = Arc::new(Recorder::default());
        let recording = view.with_global_stats(Some(rec.clone() as Arc<dyn search_index::StatsSource>));
        let table = t.engine.table();
        let qctx = super::query::QueryCtx::new(cluster, &st, &t.idx, &table, &recording, req.now_ms);
        // Inner hits are extracted before the query is built (`SearchService.parseSource`).
        if let Err(e) = super::check_inner_hit_names(&req.source) {
            failures.push((i, e));
            requested.push(std::mem::take(&mut *rec.fields.lock()));
            views.push(view);
            continue;
        }
        match super::shard_query(&qctx, t, &req.source) {
            // The DFS phase parses the whole source but the aggregations
            // (`SearchService.createContext`): the post filter and the sort fail there too.
            Ok(q) => match req.source.post_filter.as_ref().map_or(Ok(()), |pf| qctx.to_filter(pf).map(|_| ()).map_err(|e| super::query::shard_exception(&t.idx, e)))
                .and_then(|_| if req.source.sort.is_empty() { Ok(()) } else { super::sort::build(&qctx, &req.source.sort).map(|_| ()) })
                // The rescorers' queries are built there as well.
                .and_then(|_| super::prepare_rescore(&qctx, &req.source).into_iter().filter(|r| !r.script).try_for_each(|r| r.kind.map(|_| ())))
            {
                // Then the context's `preProcess` checks (window, sort with rescore, limits).
                Ok(()) => match super::context_checks_of(t, req) {
                    Ok(()) => {
                        let _ = recording.create_weight(&q, search_index::ScoreMode::Complete);
                    }
                    Err(e) => failures.push((i, e)),
                },
                Err(e) => failures.push((i, e)),
            },
            Err(e) => failures.push((i, super::query::shard_exception(&t.idx, e))),
        }
        requested.push(std::mem::take(&mut *rec.fields.lock()));
        views.push(view);
    }
    let stats: Arc<dyn search_index::StatsSource> = Arc::new(DfsStats { views: views.clone(), requested, terms: Mutex::default(), fields: Mutex::default() });
    let targets = targets
        .iter()
        .zip(views)
        .map(|(t, v)| {
            let mut t = t.clone();
            t.searcher = Some(v.with_global_stats(Some(stats.clone())));
            t
        })
        .collect();
    Ok((targets, failures))
}

#[cfg(test)]
mod tests {
    use crate::json::Value;
    use crate::obj;

    fn failure(index: &str, shard: i64, ty: &str, reason: &str) -> Value {
        obj! { "shard" => shard, "index" => index, "node" => "n", "reason" => obj! { "type" => ty, "reason" => reason } }
    }

    // Shard failures print once per index and cause (`ExceptionsHelper.groupBy`); not a DFS
    // concern, but this module's tests are where the search coordinator's helpers live.
    #[test]
    fn shard_failures_group_by_index_and_cause() {
        let fs = vec![
            failure("a", 0, "illegal_argument_exception", "x"),
            failure("a", 2, "illegal_argument_exception", "x"),
            failure("a", 1, "illegal_argument_exception", "y"),
            failure("b", 0, "illegal_argument_exception", "x"),
            failure("a", 3, "query_shard_exception", "x"),
        ];
        let g = super::super::group_failures(&fs);
        let shards: Vec<(String, i64)> = g.iter().map(|f| (f.get("index").and_then(Value::as_str).unwrap().to_string(), f.get("shard").and_then(Value::as_i64).unwrap())).collect();
        assert_eq!(shards, vec![("a".into(), 0), ("a".into(), 1), ("b".into(), 0), ("a".into(), 3)]);
    }
}
