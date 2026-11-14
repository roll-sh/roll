//! OpenSearch's `ApproximateScoreQuery` as far as clients can see it: the queries
//! `match_all` and numeric / date `range` build (`MatchAllQueryBuilder`, `NumberFieldMapper`,
//! `DateFieldMapper`) render as `ApproximateScoreQuery(originalQuery=…, approximationQuery=
//! Approximate(…))` in `_validate/query` explanations and as an `ApproximateScoreQuery` node
//! in profiles.
//!
//! The approximation itself (stopping a point-tree walk once enough hits are collected) is
//! something roll's engine already does on its own terms, so the wrapper only changes how the
//! query presents itself: weights (of the wrapped query rewritten), visits and cache identities
//! are the wrapped query's, and [`Query::as_any`] answers with the wrapped query, so every
//! engine shortcut that recognises a `MatchAllQuery` or a range by downcasting keeps working
//! unchanged. The wrapper itself never rewrites away, as under OpenSearch's search context.

use search_index::Schema;
use search_index::query::MatchAllQuery;
use search_index::query::range::{IndexOrDocValuesQuery, PointRangeQuery};
use search_index::search::{Query, QueryRef, QueryVisitor, ScoreMode, Searcher, Weight};
use std::any::Any;
use std::sync::Arc;

#[derive(Debug)]
pub struct ApproximateScoreQuery {
    original: QueryRef,
    /// The search's own query (`ContextIndexSearcher` set its search context on it); any
    /// other rewrites to the original query.
    top: bool,
}

impl ApproximateScoreQuery {
    /// `match_all`'s query.
    pub fn match_all() -> QueryRef {
        Arc::new(ApproximateScoreQuery { original: Arc::new(MatchAllQuery), top: false })
    }

    /// Wraps a numeric or date range when it is one OpenSearch approximates: a point range,
    /// alone or paired with its doc-values twin (fields that aren't indexed, empty ranges and
    /// anything else pass through).
    pub fn range(q: QueryRef) -> QueryRef {
        let any = q.as_any();
        if any.is::<IndexOrDocValuesQuery>() || any.is::<PointRangeQuery>() { Arc::new(ApproximateScoreQuery { original: q, top: false }) } else { q }
    }

    /// The query phase's query: an approximation at the top keeps presenting itself (in
    /// profiles), one inside another query rewrites to the query it wraps.
    pub fn mark_top(q: QueryRef) -> QueryRef {
        let any: &dyn Any = &*q;
        match any.downcast_ref::<ApproximateScoreQuery>() {
            Some(a) if !a.top => Arc::new(ApproximateScoreQuery { original: a.original.clone(), top: true }),
            _ => q,
        }
    }

    /// The approximation's text: the point range (`Approximate(field:[l TO u])`), or `*:*`.
    fn approximation(&self, schema: &Schema) -> String {
        let any = self.original.as_any();
        if let Some(iodv) = any.downcast_ref::<IndexOrDocValuesQuery>() {
            return iodv.index.lucene_string(schema);
        }
        self.original.lucene_string(schema)
    }
}

impl Query for ApproximateScoreQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> search_index::Result<Box<dyn Weight>> {
        // The query that runs is the original rewritten (`resolvedQuery`, rewritten in place).
        s.rewrite(&self.original)?.weight(s, mode, boost)
    }

    fn rewrite(&self, _s: &Searcher) -> search_index::Result<Option<QueryRef>> {
        // Under a search context `ApproximateScoreQuery.rewrite` rewrites only the query it
        // resolved to and returns itself: profiles, explanations and the count shortcut see the
        // wrapper around the original as parsed (a range covering every value stays a range).
        // Without one (nested in another query) it is the original query.
        if self.top { Ok(None) } else { Ok(Some(self.original.clone())) }
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        format!("ApproximateScoreQuery(originalQuery={}, approximationQuery=Approximate({}))", self.original.lucene_string(schema), self.approximation(schema))
    }

    fn visit(&self, v: &mut dyn QueryVisitor) {
        self.original.visit(v)
    }

    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        self.original.cache_identity(h)
    }

    fn profile_name(&self) -> String {
        "ApproximateScoreQuery".into()
    }

    fn as_any(&self) -> &dyn Any {
        self.original.as_any()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use search_index::query::MatchNoneQuery;

    #[test]
    fn match_all_presents_as_approximation_but_downcasts_to_match_all() {
        let q = ApproximateScoreQuery::match_all();
        let schema = Schema::default();
        assert_eq!(q.lucene_string(&schema), "ApproximateScoreQuery(originalQuery=*:*, approximationQuery=Approximate(*:*))");
        assert!(q.as_any().is::<MatchAllQuery>());
        assert_eq!(q.profile_name(), "ApproximateScoreQuery");
    }

    #[test]
    fn only_point_ranges_are_wrapped() {
        let none: QueryRef = Arc::new(MatchNoneQuery::new("empty range"));
        let out = ApproximateScoreQuery::range(none.clone());
        assert!(Arc::ptr_eq(&out, &none));
    }

    #[test]
    fn only_the_top_level_approximation_stays() {
        let s = search_index::Searcher::new(Vec::new(), Arc::new(Schema::default()), 0);
        let nested = ApproximateScoreQuery::match_all();
        assert!(nested.rewrite(&s).unwrap().is_some_and(|q| q.profile_name() != "ApproximateScoreQuery"));
        let top = ApproximateScoreQuery::mark_top(nested);
        assert!(top.rewrite(&s).unwrap().is_none());
        assert_eq!(top.profile_name(), "ApproximateScoreQuery");
    }
}
