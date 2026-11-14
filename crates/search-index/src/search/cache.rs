//! The filter cache (Lucene's `LRUQueryCache` with `UsageTrackingQueryCachingPolicy`, which
//! Elasticsearch enables): the doc sets of non-scoring clauses that recur across searches are
//! kept per segment and reused.
//!
//! A query's identity is [`Query::cache_identity`]: an
//! exact structural hash (two independent 64-bit hashes, one keying the entry and one checked
//! on hit), so only queries whose fields pin down their doc set qualify — terms, phrases,
//! prefixes, ranges, term sets, points, doc values, geo, exists and any boolean / dis_max /
//! constant_score / boost over those. As in Lucene, term queries, exists, match_all/none and
//! empty compounds are never cached (iterating them is as fast as a bit set); costly ones
//! (multi-term, term sets, point queries) are cached once seen twice among the shard's last
//! 256 filters, compounds four times and the rest five. Segments must hold at least 10 000
//! docs and 3% of the searcher's docs (smaller ones are cheap to evaluate and churn), and a
//! clause far costlier than the conjunction's lead (250×) is not materialized. Sets live in
//! the segment's core cache, so they go away with the segment, count towards the shard's
//! memory, and are bounded per segment (oldest first). Deletions are applied by the search
//! loop, so a set stays valid for every snapshot holding the segment.

use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::error::Result;
use crate::query::basic::{FieldExistsQuery, MatchAllQuery, MatchNoneQuery};
use crate::query::boolean::BooleanQuery;
use crate::query::dismax::DisMaxQuery;
use crate::query::multiterm::MultiTermQuery;
use crate::query::range::{PointInSetQuery, PointRangeQuery};
use crate::query::term::TermQuery;
use crate::search::{
    SegmentCore, BulkScorer, DefaultBulkScorer, Explanation, Query, QueryRef, ReadyScorer, ScoreMode, Scorer, ScorerSupplier, Searcher, SegmentContext,
    Weight,
};
use crate::util::bitset::{DocIdSet, DocIdSetBuilder};

/// Recent filters of a shard and the caching thresholds.
pub struct QueryCachePolicy {
    history: Mutex<History>,
    /// Segments smaller than this are never cached.
    pub min_segment_docs: u32,
    /// Nor segments with less than this share of the searcher's docs.
    pub min_segment_ratio: f32,
}

#[derive(Default)]
struct History {
    ring: VecDeque<u64>,
    counts: HashMap<u64, u32>,
}

const HISTORY: usize = 256;

impl Default for QueryCachePolicy {
    fn default() -> Self {
        QueryCachePolicy { history: Mutex::new(History::default()), min_segment_docs: 10_000, min_segment_ratio: 0.03 }
    }
}

impl QueryCachePolicy {
    /// Records one use of `key`; returns how often it was seen recently (this one included).
    fn record(&self, key: u64) -> u32 {
        let mut h = self.history.lock();
        h.ring.push_back(key);
        *h.counts.entry(key).or_default() += 1;
        if h.ring.len() > HISTORY
            && let Some(old) = h.ring.pop_front()
            && let Some(c) = h.counts.get_mut(&old)
        {
            *c -= 1;
            if *c == 0 {
                h.counts.remove(&old);
            }
        }
        h.counts.get(&key).copied().unwrap_or(0)
    }
}

/// A filter's cache identity and how often it must recur before it is cached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheKey {
    pub key: u64,
    /// A second, independent hash checked on hit.
    pub check: u64,
    pub min_frequency: u32,
}

/// Lucene's `UsageTrackingQueryCachingPolicy.shouldNeverCache`.
fn never_cache(q: &dyn Query) -> bool {
    let a = q.as_any();
    a.is::<TermQuery>()
        || a.is::<FieldExistsQuery>()
        || a.is::<MatchAllQuery>()
        || a.is::<MatchNoneQuery>()
        || a.downcast_ref::<BooleanQuery>().is_some_and(|b| b.clauses.is_empty())
        || a.downcast_ref::<DisMaxQuery>().is_some_and(|d| d.disjuncts.is_empty())
}

/// Lucene's `isCostly`: building their doc set is the expensive part.
fn is_costly(q: &dyn Query) -> bool {
    let a = q.as_any();
    a.is::<MultiTermQuery>() || a.is::<PointRangeQuery>() || a.is::<PointInSetQuery>()
}

/// The cache identity of a query when it may be cached (`None`: never cache it).
pub fn cache_key(q: &QueryRef) -> Option<CacheKey> {
    if never_cache(q.as_ref()) {
        return None;
    }
    let mut h1 = std::collections::hash_map::DefaultHasher::new();
    if !q.cache_identity(&mut h1) {
        return None;
    }
    let mut h2 = foldhash::quality::FixedState::with_seed(0x5eed_cafe_f00d_d00d).build_hasher();
    q.cache_identity(&mut h2);
    let a = q.as_any();
    let min_frequency = if is_costly(q.as_ref()) {
        2
    } else if a.is::<BooleanQuery>() || a.is::<DisMaxQuery>() {
        4
    } else {
        5
    };
    Some(CacheKey { key: h1.finish(), check: h2.finish(), min_frequency })
}

/// Per-segment bound of cached filters.
const MAX_PER_SEGMENT: usize = 16;

/// Lucene's `LRUQueryCache.skipCacheFactor`: a clause costing more than this many times the
/// conjunction's lead is iterated rather than materialized.
const SKIP_CACHE_FACTOR: u64 = 250;

/// A cached doc set with the identity check.
struct Cached {
    check: u64,
    set: Arc<DocIdSet>,
}

/// Wraps a non-scoring weight with the cache.
pub struct CachingWeight {
    inner: Box<dyn Weight>,
    key: CacheKey,
    hot: bool,
    max_doc: u32,
    min_segment_docs: u32,
    min_segment_ratio: f32,
}

/// `inner` wrapped with the filter cache when the searcher has one and the query qualifies.
pub fn maybe_cache(s: &Searcher, q: &QueryRef, inner: Box<dyn Weight>) -> Box<dyn Weight> {
    let Some(policy) = s.query_cache() else { return inner };
    let Some(key) = cache_key(q) else { return inner };
    let hot = policy.record(key.key) >= key.min_frequency;
    Box::new(CachingWeight {
        inner,
        key,
        hot,
        max_doc: s.max_doc(),
        min_segment_docs: policy.min_segment_docs,
        min_segment_ratio: policy.min_segment_ratio,
    })
}

impl CachingWeight {
    fn eligible(&self, seg: &SegmentContext) -> bool {
        let md = seg.reader.max_doc();
        self.hot && md >= self.min_segment_docs && md as f32 >= self.min_segment_ratio * self.max_doc as f32
    }

    fn lookup(&self, seg: &SegmentContext) -> Option<Arc<DocIdSet>> {
        seg.core.cache_get::<Cached>("query-cache", self.key.key).filter(|c| c.check == self.key.check).map(|c| c.set.clone())
    }

    fn build(&self, seg: &SegmentContext, scorer: Option<Box<dyn Scorer>>) -> Result<Arc<DocIdSet>> {
        build(&seg.core, seg.max_doc(), self.key, scorer)
    }

    fn set_scorer(set: Arc<DocIdSet>) -> Option<Box<dyn Scorer>> {
        if matches!(*set, DocIdSet::Empty) {
            None
        } else {
            Some(Box::new(crate::query::basic::SetScorer::new(set, 0.0)))
        }
    }
}

/// Materializes a clause's docs on a segment and caches them.
fn build(core: &SegmentCore, max_doc: u32, key: CacheKey, scorer: Option<Box<dyn Scorer>>) -> Result<Arc<DocIdSet>> {
    let set = match scorer {
        None => Arc::new(DocIdSet::Empty),
        Some(mut sc) => {
            let mut b = DocIdSetBuilder::new(max_doc);
            loop {
                let d = sc.next();
                if d == search_codec::NO_MORE_DOCS {
                    break;
                }
                b.add(d);
            }
            if let Some(e) = crate::search::take_deferred_error() {
                return Err(e);
            }
            Arc::new(b.build())
        }
    };
    let bytes = set.ram_bytes().max(16);
    core.cache_put("query-cache", key.key, Arc::new(Cached { check: key.check, set: set.clone() }), bytes, MAX_PER_SEGMENT);
    Ok(set)
}

/// Builds the cached set on `get` unless the clause is far costlier than the lead.
struct CachingSupplier {
    core: Arc<SegmentCore>,
    max_doc: u32,
    key: CacheKey,
    inner: Box<dyn ScorerSupplier>,
}

impl ScorerSupplier for CachingSupplier {
    fn cost(&self) -> u64 {
        self.inner.cost()
    }

    fn get(self: Box<Self>, lead_cost: u64) -> Result<Box<dyn Scorer>> {
        let cost = self.inner.cost();
        if lead_cost != u64::MAX && cost / SKIP_CACHE_FACTOR > lead_cost {
            return self.inner.get(lead_cost);
        }
        let (core, max_doc, key) = (self.core.clone(), self.max_doc, self.key);
        let sc = self.inner.get(u64::MAX)?;
        let set = build(&core, max_doc, key, Some(sc))?;
        Ok(Box::new(crate::query::basic::SetScorer::new(set, 0.0)))
    }
}

impl Weight for CachingWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        if let Some(set) = self.lookup(seg) {
            return Ok(CachingWeight::set_scorer(set));
        }
        if !self.eligible(seg) {
            return self.inner.scorer(seg);
        }
        let set = self.build(seg, self.inner.scorer(seg)?)?;
        Ok(CachingWeight::set_scorer(set))
    }

    fn scorer_supplier(&self, seg: &SegmentContext) -> Result<Option<Box<dyn ScorerSupplier>>> {
        if let Some(set) = self.lookup(seg) {
            return Ok(CachingWeight::set_scorer(set).map(|s| Box::new(ReadyScorer(s)) as Box<dyn ScorerSupplier>));
        }
        let Some(inner) = self.inner.scorer_supplier(seg)? else {
            if self.eligible(seg) {
                self.build(seg, None)?;
            }
            return Ok(None);
        };
        if !self.eligible(seg) {
            return Ok(Some(inner));
        }
        Ok(Some(Box::new(CachingSupplier { core: seg.core.clone(), max_doc: seg.max_doc(), key: self.key, inner })))
    }

    fn bulk_scorer(&self, seg: &SegmentContext, mode: ScoreMode) -> Result<Option<Box<dyn BulkScorer>>> {
        if self.lookup(seg).is_none() && !self.eligible(seg) {
            return self.inner.bulk_scorer(seg, mode);
        }
        Ok(self.scorer(seg)?.map(|scorer| Box::new(DefaultBulkScorer::new(scorer)) as Box<dyn BulkScorer>))
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        self.inner.explain(seg, doc)
    }

    fn count(&self, seg: &SegmentContext) -> Result<Option<u32>> {
        if seg.live.is_none()
            && let Some(set) = self.lookup(seg)
        {
            return Ok(Some(set.cost() as u32));
        }
        self.inner.count(seg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::basic::ConstantScoreQuery;
    use crate::query::boolean::BooleanQuery;
    use crate::query::phrase::PhraseQuery;
    use crate::query::range::PointRangeQuery;

    fn t(f: u32, s: &str) -> QueryRef {
        Arc::new(TermQuery::new(f, s.as_bytes().to_vec()))
    }

    #[test]
    fn keys_follow_lucenes_policy() {
        // Never: terms, exists, match_all/none, empty compounds.
        assert!(cache_key(&t(1, "a")).is_none());
        assert!(cache_key(&(Arc::new(FieldExistsQuery { field: 1 }) as QueryRef)).is_none());
        assert!(cache_key(&(Arc::new(BooleanQuery::new()) as QueryRef)).is_none());
        // Costly after 2 uses, compounds after 4, the rest after 5.
        let range: QueryRef = Arc::new(PointRangeQuery::long(2, 0, 10));
        assert_eq!(cache_key(&range).unwrap().min_frequency, 2);
        let prefix: QueryRef = Arc::new(MultiTermQuery::prefix(1, b"ab".to_vec()));
        assert_eq!(cache_key(&prefix).unwrap().min_frequency, 2);
        let b: QueryRef = Arc::new(BooleanQuery::new().must(t(1, "a")).filter(range.clone()));
        assert_eq!(cache_key(&b).unwrap().min_frequency, 4);
        let ph: QueryRef = Arc::new(PhraseQuery::new(1, vec![b"a".to_vec(), b"b".to_vec()], 0));
        assert_eq!(cache_key(&ph).unwrap().min_frequency, 5);
        // Uncacheable inside makes the compound uncacheable.
        let wild: QueryRef = Arc::new(MultiTermQuery::wildcard(1, "a*b", false).unwrap());
        assert!(cache_key(&wild).is_none());
        assert!(cache_key(&(Arc::new(BooleanQuery::new().must(t(1, "a")).should(wild)) as QueryRef)).is_none());
    }

    #[test]
    fn identities_are_exact() {
        let k = |q: QueryRef| cache_key(&q).map(|k| (k.key, k.check));
        let ph = |terms: &[&str], slop| -> QueryRef { Arc::new(PhraseQuery::new(1, terms.iter().map(|t| t.as_bytes().to_vec()).collect(), slop)) };
        // Same text, different structure.
        assert_ne!(k(ph(&["a b", "c"], 0)), k(ph(&["a", "b c"], 0)));
        assert_ne!(k(ph(&["a", "b"], 0)), k(ph(&["a", "b"], 1)));
        assert_eq!(k(ph(&["a", "b"], 0)), k(ph(&["a", "b"], 0)));
        let bq = |occ: bool| -> QueryRef {
            let b = BooleanQuery::new().must(ph(&["a", "b"], 0));
            Arc::new(if occ { b.must(t(1, "c")) } else { b.must_not(t(1, "c")) })
        };
        assert_ne!(k(bq(true)), k(bq(false)));
        assert_eq!(k(bq(true)), k(bq(true)));
        // Fields matter; boosts and constant_score don't change the doc set's identity
        // beyond their wrapper tag.
        assert_ne!(k(ph(&["a", "b"], 0)), k(Arc::new(PhraseQuery::new(2, vec![b"a".to_vec(), b"b".to_vec()], 0))));
        let boosted: QueryRef = Arc::new(crate::query::basic::BoostQuery::new(ph(&["a", "b"], 0), 3.0));
        assert_eq!(k(boosted), k(ph(&["a", "b"], 0)));
        assert!(k(Arc::new(ConstantScoreQuery::new(ph(&["a", "b"], 0)))).is_some());
    }
}
