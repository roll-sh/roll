//! OpenSearch's can-match pre-filter phase (`CanMatchPreFilterSearchPhase`): before the query
//! phase, every shard is asked whether the query can match anything there, and the shards
//! that can't are skipped (`_shards.skipped`).
//!
//! It runs when `TransportSearchAction.shouldPreFilterSearchShards` says so: a
//! `query_then_fetch` search whose query may rewrite to `match_none`
//! (`SearchService.canRewriteToMatchNone`: a query other than `match_all`, no suggesters, no
//! top-level aggregation that must see every document) or whose primary sort is a field sort,
//! over more shards than `pre_filter_shard_size` (default 128; 1 when a target index is
//! read-only or the primary sort is a field sort). OpenSearch's shards build that sort in this
//! phase, so a sort every shard rejects fails the search here (`phase: can_match`).
//!
//! A shard can't match when the query rewrites to `match_none` on it the way OpenSearch's
//! query builders rewrite against a shard (`Rewriteable.rewrite` with a `QueryShardContext`):
//! a `range` on a date field whose indexed values all lie outside it (or that has none),
//! a `range`, `term`, `terms`, `prefix` or `wildcard` on a field the index doesn't map, terms
//! on `_index` that don't name the index, and `bool` / `constant_score` around those. Only
//! these builder-level rewrites count: roll's Lucene-level rewrites know more, but OpenSearch
//! would still query those shards. When every shard would be skipped the first one still runs,
//! so the response has real (empty) results.
//!
//! Elasticsearch 9.5.5 runs the same phase with these differences (found by the differential
//! suite): shards whose sort fails count as matching (the query phase fails them), and its
//! builders rewrite a few more queries (`exists` / `fuzzy` on unmapped
//! fields, `boosting`, `function_score`, `script_score`, `bool` with only `match_none` shoulds
//! or a `match_all` `must_not`). When every shard can be skipped, it queries one only if the
//! request has aggregations; otherwise the response is empty with all shards skipped
//! ([`all_skipped`]).

use crate::json::Value;
use crate::query::Query;
use crate::search::query::QueryCtx;
use crate::search::{SearchRequest, Target};

/// The targets to query and how many were skipped. Targets keep their order.
pub fn filter(ctx: &crate::rest::Ctx, req: &SearchRequest, targets: Vec<Target>) -> crate::error::EsResult<(Vec<Target>, usize)> {
    let es = ctx.cluster.dialect == crate::cluster::Dialect::Elasticsearch;
    // A `dfs_query_then_fetch` search is never pre-filtered: DFS fans out to every shard.
    if targets.len() < 2 || req.dfs {
        return Ok((targets, 0));
    }
    // `TransportSearchAction.shouldPreFilterSearchShards` also pre-filters every search whose
    // primary sort is a field sort (`FieldSortBuilder.getPrimaryFieldSortOrNull`; `_doc` and
    // `_shard_doc` are field sorts too).
    let field_sort = matches!(req.source.sort.first(), Some(crate::search_source::Sort::Field { .. }));
    let pre_filter = if ctx.has_query("pre_filter_shard_size") {
        ctx.int_param("pre_filter_shard_size", 128)?.max(1) as usize
    } else if field_sort || targets.iter().any(|t| read_only(&t.idx)) {
        1
    } else {
        128
    };
    let rewritable = req.source.query.as_ref().is_some_and(|q| can_rewrite_to_match_none(req, q));
    if pre_filter >= targets.len() || !(rewritable || field_sort) {
        return Ok((targets, 0));
    }
    let st = ctx.state();
    // OpenSearch's shards build the primary field sort while they answer (`getMinMaxOrNull`):
    // when it fails on every shard the search fails in this phase (Elasticsearch counts such
    // shards as matching and fails them in the query phase).
    if !es && field_sort {
        let mut failures = Vec::new();
        for (i, t) in targets.iter().enumerate() {
            let Ok(searcher) = t.searcher() else { continue };
            let table = t.table();
            let mut qctx = QueryCtx::new(ctx.cluster, &st, &t.idx, &table, &searcher, req.now_ms);
            qctx.runtime_mappings = req.source.runtime_mappings.clone();
            if let Err(e) = crate::search::sort::build(&qctx, &req.source.sort[..1]) {
                failures.push((i, e));
            }
        }
        if failures.len() == targets.len() {
            return Err(crate::search::all_shards_failed(&ctx.cluster.node.id, &targets, &failures, "all shards failed").set_meta("phase", "can_match"));
        }
    }
    let Some(q) = req.source.query.as_ref().filter(|_| rewritable) else { return Ok((targets, 0)) };
    let mut keep: Vec<bool> = Vec::with_capacity(targets.len());
    for t in &targets {
        let Ok(searcher) = t.searcher() else {
            keep.push(true);
            continue;
        };
        let table = t.table();
        let mut qctx = QueryCtx::new(ctx.cluster, &st, &t.idx, &table, &searcher, req.now_ms);
        qctx.runtime_mappings = req.source.runtime_mappings.clone();
        let alias_none = t.alias_filter.as_ref().is_some_and(|f| matches!(rewrite(&qctx, f, es), Rewritten::None));
        keep.push(!alias_none && !matches!(rewrite(&qctx, q, es), Rewritten::None));
    }
    // OpenSearch always queries one shard. Elasticsearch only when the request has
    // aggregations (`requireAtLeastOneMatch`); otherwise every shard is skipped and the
    // response is empty (see `all_skipped`).
    if !keep.iter().any(|k| *k) && (!es || req.source.aggs.is_some()) {
        keep[0] = true;
    }
    let skipped = keep.iter().filter(|k| !**k).count();
    if skipped == 0 {
        return Ok((targets, 0));
    }
    let kept = targets.into_iter().zip(keep).filter_map(|(t, k)| k.then_some(t)).collect();
    Ok((kept, skipped))
}

/// Elasticsearch's response when the can-match phase skipped every shard and no aggregation
/// needs one queried: no shard ran, so there is nothing but an exact total of 0 and a
/// `max_score` of 0 (whatever `track_total_hits` asked for).
pub fn all_skipped(shards: usize) -> crate::search::SearchOutcome {
    crate::search::SearchOutcome {
        took_ms: 0,
        shards_total: shards,
        shards_skipped: shards,
        failures: Vec::new(),
        total: Some((0, search_index::collector::top_docs::TotalRelation::EqualTo)),
        max_score: Some(0.0),
        hits: Vec::new(),
        aggs: None,
        suggest: None,
        terminated_early: None,
        timed_out: false,
        profile: None,
        scroll_id: None,
        pit_id: None,
        ranked: Vec::new(),
        clusters: None,
    }
}

/// `index.blocks.write` / `index.blocks.read_only` (`hasReadOnlyIndices`: a write block).
fn read_only(idx: &crate::meta::IndexMeta) -> bool {
    ["index.blocks.write", "index.blocks.read_only", "index.blocks.read_only_allow_delete"].iter().any(|k| idx.settings.get(k) == Some("true"))
}

/// `SearchService.canRewriteToMatchNone`.
fn can_rewrite_to_match_none(req: &SearchRequest, q: &Query) -> bool {
    if matches!(q, Query::MatchAll(_)) || req.source.suggest.is_some() {
        return false;
    }
    let Some(aggs) = &req.source.aggs else { return true };
    // `AggregatorFactories.Builder.mustVisitAllDocs`: a top-level `global`, or `terms` with
    // `min_doc_count: 0`.
    !aggs.values().any(|a| {
        let Value::Obj(m) = a else { return false };
        m.contains_key("global") || m.get("terms").and_then(|t| t.get("min_doc_count")).and_then(Value::as_i64) == Some(0)
    })
}

/// What a query builder's shard rewrite tells about it.
enum Rewritten {
    /// `MatchNoneQueryBuilder`.
    None,
    /// Anything else.
    Some,
}

/// `es`: Elasticsearch 9.5.5's builders, which rewrite a few more queries than OpenSearch's.
fn rewrite(qctx: &QueryCtx, q: &Query, es: bool) -> Rewritten {
    // Runtime fields (the request's `runtime_mappings`, the mapping's `runtime` section) are
    // mapped too: they aren't in the field table.
    let runtime = |field: &str| qctx.runtime_field(field).is_some();
    let unmapped = |field: &str| qctx.field(field).is_none() && !field.starts_with('_') && !runtime(field);
    let is_none = |q: &Query| matches!(rewrite(qctx, q, es), Rewritten::None);
    let none = match q {
        Query::MatchNone(_) => true,
        // `BoolQueryBuilder.doRewrite`: only `should` clauses, all `match_none`; or a `must` /
        // `filter` that is `match_none`, or a `must_not` that is `match_all` (an empty `bool`
        // rewrites to `match_all`).
        Query::Boosting(b) if es => is_none(&b.positive),
        Query::FunctionScore(f) if es => is_none(&f.query),
        Query::ScriptScore(s) if es => is_none(&s.query),
        Query::Fuzzy(f) if es => unmapped(&f.field),
        // `ExistsQueryBuilder.doRewrite`: no mapped field at or below the path.
        Query::Exists(e) if es && runtime(&e.field) => false,
        Query::Exists(e) if es => qctx.exists_query(&e.field).ok().is_some_and(|lq| lq.as_any().is::<search_index::query::MatchNoneQuery>()),
        Query::Bool(b) => bool_none(qctx, b, es),
        Query::ConstantScore(c) => is_none(&c.filter),
        // A runtime field (it may shadow a concrete one) knows no bounds without running.
        Query::Range(r) if runtime(&r.field) => false,
        Query::Range(r) => match qctx.field(&r.field) {
            // `RangeQueryBuilder.getRelation`: unmapped fields are disjoint; only date fields
            // know their bounds (`DateFieldType.isFieldWithinQuery`).
            None => !r.field.starts_with('_'),
            // `DateFieldType.isFieldWithinQuery`: a shard without points is disjoint from any
            // range, unbounded ones too. A field that isn't indexed has no points to tell
            // (doc values only): it may match anywhere.
            Some(f) if f.kind.is_date() && !f.index => false,
            Some(f) if f.kind.is_date() && qctx.searcher.segments().iter().all(|s| s.reader.points(f.number).is_none_or(|p| p.size() == 0)) => true,
            Some(f) if f.kind.is_date() => crate::search::range::range_query(qctx, r)
                .ok()
                .and_then(|lq| qctx.searcher.rewrite(&lq).ok())
                .is_some_and(|lq| lq.as_any().is::<search_index::query::MatchNoneQuery>()),
            Some(_) => false,
        },
        // `ConstantFieldType`: a term-level query on a `constant_keyword` field rewrites to
        // `match_all` or `match_none` from the mapped value alone.
        Query::Term(_) | Query::Terms(_) | Query::Prefix(_) | Query::Wildcard(_) if es && constant_keyword(qctx, q) => {
            let none = |lq: crate::error::EsResult<search_index::QueryRef>| lq.ok().is_some_and(|lq| lq.as_any().is::<search_index::query::MatchNoneQuery>());
            match (q, qctx.field(constant_field(q))) {
                // `TermsQueryBuilder`: none of the values is the field's.
                (Query::Terms(t), Some(f)) => t.values.as_ref().is_some_and(|vs| vs.iter().all(|v| none(qctx.term_query(&f, v, false)))),
                _ => none(qctx.to_query(q)),
            }
        }
        Query::Term(t) => {
            if t.field == "_index" {
                index_mismatch(qctx, std::slice::from_ref(&t.value), t.case_insensitive)
            } else {
                unmapped(&t.field)
            }
        }
        Query::Terms(t) => {
            if t.field == "_index"
                && let Some(values) = &t.values
            {
                index_mismatch(qctx, values, false)
            } else {
                unmapped(&t.field)
            }
        }
        Query::Prefix(p) => unmapped(&p.field),
        Query::Wildcard(w) => unmapped(&w.field),
        _ => false,
    };
    if none { Rewritten::None } else { Rewritten::Some }
}

/// The field of a term-level query.
fn constant_field(q: &Query) -> &str {
    match q {
        Query::Term(t) => &t.field,
        Query::Terms(t) => &t.field,
        Query::Prefix(p) => &p.field,
        Query::Wildcard(w) => &w.field,
        _ => "",
    }
}

/// Whether `q` is a term-level query on a `constant_keyword` field.
fn constant_keyword(qctx: &QueryCtx, q: &Query) -> bool {
    let field = constant_field(q);
    !field.is_empty() && qctx.field(field).is_some_and(|f| f.kind == crate::engine::fields::Kind::ConstantKeyword)
}

/// `BoolQueryBuilder.doRewrite` against a shard: whether the `bool` becomes `match_none`.
fn bool_none(qctx: &QueryCtx, b: &crate::query::compound::Bool, es: bool) -> bool {
    let is_none = |q: &Query| matches!(rewrite(qctx, q, es), Rewritten::None);
    if es {
        // Only `match_none` shoulds; a `must` / `filter` that is `match_none`; or a `must_not`
        // that is `match_all` (an empty `bool` rewrites to `match_all`).
        let all = |q: &Query| match q {
            Query::MatchAll(_) => true,
            Query::Bool(x) => x.must.is_empty() && x.filter.is_empty() && x.must_not.is_empty() && x.should.is_empty(),
            _ => false,
        };
        return (b.must.is_empty() && b.filter.is_empty() && b.must_not.is_empty() && !b.should.is_empty() && b.should.iter().all(is_none))
            || b.must.iter().chain(&b.filter).any(is_none)
            || b.must_not.iter().any(all);
    }
    // OpenSearch: a lone `should` stands for the whole query; a `must` or `filter` that
    // rewrites to `match_none` makes all of it `match_none`.
    if b.must.is_empty() && b.filter.is_empty() && b.must_not.is_empty() && b.should.len() == 1 {
        is_none(&b.should[0])
    } else {
        b.must.iter().chain(&b.filter).any(is_none)
    }
}

/// Whether a `bool` rewrites to `match_none` on this shard (`Rewriteable.rewrite` with the
/// shard's context, before any clause becomes a Lucene query): the query phase then builds
/// none of its clauses, so their errors never surface — on every search, not only in the
/// can-match phase.
pub fn bool_matches_none(qctx: &QueryCtx, b: &crate::query::compound::Bool) -> bool {
    bool_none(qctx, b, qctx.cluster.dialect == crate::cluster::Dialect::Elasticsearch)
}

/// A `term(s)` on `_index` none of whose values name this index — through an alias, a data
/// stream or a wildcard too, as the query itself matches (`SearchExecutionContext.indexMatches`).
fn index_mismatch(qctx: &QueryCtx, values: &[Value], case_insensitive: bool) -> bool {
    !values.iter().any(|v| match v.as_str() {
        Some(s) if case_insensitive => qctx.index_matches(&s.to_lowercase()),
        Some(s) => qctx.index_matches(s),
        None => true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(body: &str) -> SearchRequest {
        let v = crate::json::parse::parse(body.as_bytes()).unwrap().unwrap();
        let source = crate::search_source::SearchSource::parse(&v, None).unwrap();
        SearchRequest {
            indices: Vec::new(),
            opts: crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED,
            source,
            routing: None,
            preference: None,
            dfs: false,
            request_cache: None,
            total_as_int: false,
            typed_keys: false,
            allow_partial: true,
            scroll: None,
            include_named_queries_score: false,
            force_synthetic_source: false,
            now_ms: 0,
            fused: None,
            rank_only: false,
            security: None,
        }
    }

    fn rewritable(body: &str) -> bool {
        let r = req(body);
        let q = r.source.query.clone().unwrap();
        can_rewrite_to_match_none(&r, &q)
    }

    #[test]
    fn only_queries_that_can_rewrite_to_match_none_prefilter() {
        assert!(!rewritable(r#"{"query":{"match_all":{}}}"#));
        assert!(rewritable(r#"{"query":{"range":{"d":{"gte":"2016-02-01"}}}}"#));
        // OpenSearch's 140_pre_filter_search_shards: global and min_doc_count 0 see every doc.
        assert!(!rewritable(r#"{"query":{"range":{"d":{"gte":1}}},"aggs":{"a":{"global":{}}}}"#));
        assert!(!rewritable(r#"{"query":{"range":{"d":{"gte":1}}},"aggs":{"a":{"terms":{"field":"_index","min_doc_count":0}}}}"#));
        assert!(rewritable(r#"{"query":{"range":{"d":{"gte":1}}},"aggs":{"a":{"terms":{"field":"_index"}}}}"#));
        assert!(!rewritable(r#"{"query":{"range":{"d":{"gte":1}}},"suggest":{"s":{"text":"x","term":{"field":"f"}}}}"#));
    }
}
