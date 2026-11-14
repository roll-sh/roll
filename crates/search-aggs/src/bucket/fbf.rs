//! Filter-by-filter execution (Elasticsearch's `FilterByFilterAggregator`).
//!
//! A top-level `filters` aggregation, and a top-level `terms` aggregation over an indexed
//! keyword field with at most [`MAX_ORDS_TO_TRY_FILTERS`] terms
//! (`StringTermsAggregatorFromFilters`), don't look at the docs the query collects: under a
//! query that matches every live doc, each bucket is just its filter's matches, so the
//! aggregator walks the filters itself when a segment starts and tells the collector it needs
//! none of the segment's docs ([`Aggregator::segment_done`](crate::collect::Aggregator::segment_done)).
//! Without sub-aggregations a filter whose weight can count a segment without deletions or
//! `_doc_count` values (a term query: its doc freq) costs O(1) per segment and filter; otherwise
//! its matches are iterated and, with sub-aggregations, fed to them one filter at a time (each
//! filter's docs ascend, so the sub-aggregations start the segment over per filter, as
//! Elasticsearch builds a leaf collector per filter).
//!
//! The rules for when this applies are Elasticsearch's (`FilterByFilterAggregator.AdapterBuilder`):
//! the aggregation is top-level, has no `other` bucket, the request isn't a `time_series` one,
//! its sub-aggregations don't read scores, `search.aggs.rewrite_to_filter_by_filter` is on, and
//! with two or more filters the index has no `_doc_count` field. Elasticsearch also needs the
//! filter to merge with the top-level query into a single non-boolean query; roll takes the
//! shortcut only under a `match_all` top-level query, where that always holds. The profile
//! output (types, `delegate_debug`, `segments_counted_in_constant_time`) follows.

use search_index::{ScoreMode, SegmentContext, Weight};

use crate::collect::{BindCtx, Subs};
use crate::error::Result;
use crate::json::Json;
use crate::values::DocCounter;

/// `TermsAggregatorFactory.MAX_ORDS_TO_TRY_FILTERS`.
pub const MAX_ORDS_TO_TRY_FILTERS: u64 = 1000;

/// Whether an aggregation bound in `ctx` with these sub-aggregations and `filters` filters may
/// run filter by filter.
pub fn eligible(ctx: &BindCtx<'_>, subs: &Subs, filters: usize) -> bool {
    ctx.filter_by_filter
        && ctx.matches_all
        && ctx.is_top_level()
        && ctx.cardinality.is_one()
        && !ctx.time_series_request
        && !subs.needs_scores()
        && (filters <= 1 || !has_doc_count_field(ctx))
}

/// `AggregationContext.hasDocCountField`: some segment has `_doc_count` values.
fn has_doc_count_field(ctx: &BindCtx<'_>) -> bool {
    ctx.doc_count_field.is_some() && ctx.searcher.segments().iter().any(|seg| !DocCounter::open(seg, ctx.doc_count_field).is_constant())
}

/// What [`Fbf::collect_segment`] reports: `(filter, doc, count, sub-aggregations)`.
pub type Emit<'a> = dyn FnMut(usize, Option<u32>, u64, &mut Subs) -> Result<()> + 'a;

/// One filter: its weight and how it prints in the profile.
pub struct FbfFilter {
    weight: Box<dyn Weight>,
    query: String,
    constant: u32,
}

/// The filters and the per-segment counters of `FilterByFilterAggregator`.
pub struct Fbf {
    filters: Vec<FbfFilter>,
    segments_counted: u32,
    segments_collected: u32,
    segments_with_deleted_docs: u32,
    segments_with_doc_count_field: u32,
}

impl Fbf {
    /// Builds the filters' weights (and their profile strings, Lucene's `toString` of the
    /// rewritten query).
    pub fn new(ctx: &BindCtx<'_>, queries: Vec<search_index::QueryRef>) -> Result<Fbf> {
        let schema = ctx.searcher.schema();
        let mut filters = Vec::with_capacity(queries.len());
        for q in queries {
            let query = ctx.searcher.rewrite(&q)?.lucene_string(schema);
            filters.push(FbfFilter { weight: ctx.searcher.create_weight(&q, ScoreMode::CompleteNoScores)?, query, constant: 0 });
        }
        Ok(Fbf { filters, segments_counted: 0, segments_collected: 0, segments_with_deleted_docs: 0, segments_with_doc_count_field: 0 })
    }

    pub fn len(&self) -> usize {
        self.filters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }

    /// Runs every filter over `seg`, reporting `emit(filter, doc, count, subs)`: with
    /// sub-aggregations once per live match (`subs` started the segment for that filter; the
    /// callee pushes the doc), otherwise once per filter with its whole count (`doc` is `None`).
    pub fn collect_segment(
        &mut self,
        seg: &SegmentContext,
        doc_count_field: Option<u32>,
        subs: &mut Subs,
        emit: &mut Emit<'_>,
    ) -> Result<()> {
        let counter = DocCounter::open(seg, doc_count_field);
        if seg.has_deletions() {
            self.segments_with_deleted_docs += 1;
        }
        if !counter.is_constant() {
            self.segments_with_doc_count_field += 1;
        }
        let with_subs = !subs.is_empty();
        if with_subs {
            self.segments_collected += 1;
        } else {
            self.segments_counted += 1;
        }
        for (k, f) in self.filters.iter_mut().enumerate() {
            if !with_subs
                && !seg.has_deletions()
                && counter.is_constant()
                && let Some(c) = f.weight.count(seg)?
            {
                f.constant += 1;
                if c > 0 {
                    emit(k, None, c as u64, subs)?;
                }
                continue;
            }
            let Some(mut s) = f.weight.scorer(seg)? else { continue };
            if with_subs {
                subs.set_segment(seg)?;
            }
            let mut total = 0u64;
            let mut doc = s.next();
            while doc != search_codec::NO_MORE_DOCS {
                if seg.is_live(doc) {
                    if with_subs {
                        emit(k, Some(doc), counter.count(doc), subs)?;
                    } else {
                        total += counter.count(doc);
                    }
                }
                doc = s.next();
            }
            if with_subs {
                subs.finish_segment()?;
            } else if total > 0 {
                emit(k, None, total, subs)?;
            }
        }
        Ok(())
    }

    /// `FilterByFilterAggregator.collectDebugInfo`.
    pub fn debug(&self, o: &mut Json) {
        o.push("segments_with_doc_count_field", Json::from(self.segments_with_doc_count_field as u64));
        o.push("segments_with_deleted_docs", Json::from(self.segments_with_deleted_docs as u64));
        let filters = self
            .filters
            .iter()
            .map(|f| {
                let mut m = Json::obj();
                m.push("query", Json::Str(f.query.clone()));
                m.push("segments_counted_in_constant_time", Json::from(f.constant as u64));
                m
            })
            .collect();
        o.push("filters", Json::Arr(filters));
        o.push("segments_counted", Json::from(self.segments_counted as u64));
        o.push("segments_collected", Json::from(self.segments_collected as u64));
    }
}
