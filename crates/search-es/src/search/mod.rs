//! The search action (`TransportSearchAction`): request assembly from the body and URL
//! parameters, index and shard resolution (aliases with filters and routing, data streams,
//! `routing`, `preference`), the query phase on every shard (alias filters, nested root
//! filter, `post_filter`, `min_score`, `terminate_after`, `track_total_hits`, sorting,
//! `search_after`, aggregations), the reduce across shards (Lucene's shard-index tie break),
//! the fetch phase ([`hits`]) and the response shape.
//!
//! Shards are searched in shard order (index name, shard id), each with its point-in-time
//! searcher; the engine parallelizes inside a shard. Query conversion ([`query`]) is per shard
//! because term statistics are.

pub mod agg_scripts;
pub mod aggregate_metric;
pub mod aggs;
pub mod computed_sort;
pub mod contexts;
pub mod dfs;
pub mod dv_distance;
pub mod dv_terms;
pub mod extra;
pub mod field_usage;
pub mod fields;
pub mod geo;
pub mod highlight;
pub mod hits;
pub mod id_fielddata;
pub mod ids;
pub mod inner;
pub mod fusion;
pub mod join;
pub mod knn;
pub mod kql;
pub mod retriever;
pub mod runtime;
pub mod runtime_query;
pub mod misc;
pub mod painless_execute;
pub mod percolate;
pub mod percolate_candidates;
pub mod rank_docs;
pub mod profile;
pub mod query;
pub mod query_string;
pub mod range;
pub mod script_terms_sort;
pub mod scripts;
pub mod suggest;
pub mod sort;
pub mod term_stats;
pub mod text;
pub mod version_field;

use crate::cluster::Cluster;
use crate::engine::{IndexEngine, ShardEngine};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Number, Value};
use crate::meta::resolve::{self, IndicesOptions};
use crate::meta::{ClusterState, IndexMeta};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::search_source::{SearchSource, Sort, SortOrder, TrackTotalHits};
use hits::FetchOpts;
use query::{QueryCtx, arc};
use search_index::collector::top_docs::{TopScoreCollector, TotalRelation};
use search_index::collector::top_field::{FieldDoc, SortKey, SortValue, TopFieldCollector};
use search_index::collector::compose::{MinScore, MultiCollector, PostFilter, TerminateAfter};
use search_index::query::{BooleanQuery, MatchAllQuery, Occur};
use search_index::{Collector, QueryRef, Searcher};
use std::sync::Arc;

/// A search request, assembled.
#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub indices: Vec<String>,
    pub opts: IndicesOptions,
    pub source: SearchSource,
    pub routing: Option<String>,
    pub preference: Option<String>,
    pub dfs: bool,
    /// The `request_cache` parameter.
    pub request_cache: Option<bool>,
    pub total_as_int: bool,
    pub typed_keys: bool,
    pub allow_partial: bool,
    pub scroll: Option<crate::units::TimeValue>,
    pub include_named_queries_score: bool,
    /// `force_synthetic_source`: hits show the synthetic `_source` of stored-source indices.
    pub force_synthetic_source: bool,
    pub now_ms: i64,
    /// The fused documents of a compound retriever, per target (see [`fusion`]).
    pub fused: Option<Arc<fusion::Fused>>,
    /// Ranks only: the outcome's [`SearchOutcome::ranked`], no fetch phase (compound
    /// retrievers' children).
    pub rank_only: bool,
}

/// One shard to search.
#[derive(Clone)]
pub struct Target {
    pub idx: Arc<IndexMeta>,
    pub engine: Arc<IndexEngine>,
    pub shard: Arc<ShardEngine>,
    pub alias_filter: Option<crate::query::Query>,
    pub boost: f32,
    /// A point-in-time searcher (scrolls, PITs); `None` = the shard's current one.
    pub searcher: Option<Searcher>,
}

impl Target {
    /// The searcher this target searches (refreshing first when due).
    pub fn searcher(&self) -> EsResult<Searcher> {
        match &self.searcher {
            Some(s) => Ok(s.clone()),
            None => {
                self.shard.before_search(IndexEngine::refresh_interval(&self.idx))?;
                Ok(self.shard.searcher())
            }
        }
    }
}

/// One hit of a shard's query phase.
#[derive(Clone, Debug)]
pub struct ShardHit {
    pub doc: u32,
    pub score: f32,
    pub sort: Vec<SortValue>,
    /// The collapse group of the hit (`collapse`).
    pub group: Option<SortValue>,
}

/// A shard's query phase result.
pub struct ShardResult {
    pub target: usize,
    pub searcher: Searcher,
    pub query: QueryRef,
    pub hits: Vec<ShardHit>,
    pub total: u64,
    pub relation: TotalRelation,
    pub max_score: Option<f32>,
    pub aggs: Option<search_aggs::Aggs>,
    pub terminated_early: bool,
    pub named: Vec<(String, QueryRef)>,
    pub inner_hits: Vec<query::InnerHitsDef>,
    /// Set when the request asks for `profile`.
    pub profile: Option<profile::ShardProfile>,
    /// Per rescorer, the (sorted) docs its window rescored (explanations wrap only those).
    pub rescored: Vec<Vec<u32>>,
    /// `percolate` queries: per matching doc its slots, and what highlighting them needs.
    pub percolate: Vec<Arc<percolate::Percolated>>,
    /// The request's `timeout` passed while collecting: the results are partial.
    pub timed_out: bool,
    /// The search's runtime fields on this shard, for the scripts of the fetch phase.
    pub runtime: Option<Arc<runtime::RuntimeScope>>,
    /// The leaf weights highlighting builds to test which clauses match a hit, shared by
    /// the page's hits.
    pub match_weights: highlight::MatchWeights,
}

/// The outcome of a search before rendering.
pub struct SearchOutcome {
    pub took_ms: u64,
    pub shards_total: usize,
    pub shards_skipped: usize,
    /// Rendered shard failures (`shard`, `index`, `node`, `reason`).
    pub failures: Vec<Value>,
    pub total: Option<(u64, TotalRelation)>,
    pub max_score: Option<f32>,
    pub hits: Vec<Value>,
    pub aggs: Option<Value>,
    pub suggest: Option<Value>,
    pub terminated_early: Option<bool>,
    pub timed_out: bool,
    pub profile: Option<Value>,
    pub scroll_id: Option<String>,
    pub pit_id: Option<String>,
    /// The page's hits as (target, global doc, score), best first.
    pub ranked: Vec<(usize, u32, f32)>,
    /// A cross-cluster search's `_clusters` section.
    pub clusters: Option<Value>,
}

fn list(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect()
}

/// Assembles the request from the body and URL parameters (`RestSearchAction.parseSearchRequest`).
pub fn parse_request(ctx: &Ctx, body: Option<&Value>, raw: Option<&[u8]>) -> EsResult<SearchRequest> {
    let mut source = match body {
        Some(v @ Value::Obj(_)) => SearchSource::parse(v, raw)?,
        Some(_) => SearchSource::parse(&Value::Obj(Map::new()), None)?,
        None => SearchSource::default(),
    };
    ctx.warn_all(source.warnings.drain(..));
    if id_fielddata::used_by(&source) && id_fielddata::enabled(&ctx.cluster.state()) {
        ctx.warn(id_fielddata::DEPRECATION);
    }
    if let Some(q) = ctx.query("q") {
        let mut qs = Map::new();
        qs.insert("query".into(), Value::Str(q.to_string()));
        if let Some(df) = ctx.query("df") {
            qs.insert("default_field".into(), Value::Str(df.to_string()));
        }
        if let Some(a) = ctx.query("analyzer") {
            qs.insert("analyzer".into(), Value::Str(a.to_string()));
        }
        if let Some(o) = ctx.query("default_operator") {
            qs.insert("default_operator".into(), Value::Str(o.to_string()));
        }
        if let Some(l) = ctx.query("lenient") {
            qs.insert("lenient".into(), Value::Bool(crate::units::parse_bool(l)?));
        }
        if let Some(w) = ctx.query("analyze_wildcard") {
            qs.insert("analyze_wildcard".into(), Value::Bool(crate::units::parse_bool(w)?));
        }
        let v = obj! { "query_string" => Value::Obj(qs) };
        source.query = Some(crate::query::parse_query(&v, None, &[])?.0);
    }
    if ctx.has_query("from") {
        source.from = Some(ctx.int_param("from", 0)? as i32);
    }
    if ctx.has_query("size") {
        source.size = Some(ctx.int_param("size", 10)? as i32);
    }
    if let Some(s) = ctx.query("sort") {
        source.sort.clear();
        for part in list(s) {
            let (field, order) = match part.rsplit_once(':') {
                Some((f, o)) => (f.to_string(), Some(SortOrder::parse(o)?)),
                None => (part.clone(), None),
            };
            if field == "_score" {
                source.sort.push(Sort::Score { order: order.unwrap_or(SortOrder::Desc) });
            } else {
                source.sort.push(Sort::Field { field, order, missing: None, unmapped_type: None, mode: None, numeric_type: None, format: None, nested: None });
            }
        }
    }
    if let Some(fs) = crate::fetch::FetchSource::from_params(ctx.query("_source"), ctx.query("_source_includes"), ctx.query("_source_excludes"))? {
        source.source = Some(crate::search_source::FetchSource { fetch: fs.fetch, includes: fs.includes, excludes: fs.excludes, exclude_vectors: None });
    }
    if let Some(ev) = ctx.opt_bool_param("_source_exclude_vectors")? {
        source.source.get_or_insert_with(|| crate::search_source::FetchSource { fetch: true, includes: Vec::new(), excludes: Vec::new(), exclude_vectors: None }).exclude_vectors = Some(ev);
    }
    if let Some(sf) = ctx.query("stored_fields") {
        let l = list(sf);
        source.stored_fields = Some(crate::search_source::StoredFields { fields: if l.iter().any(|x| x == "_none_") { None } else { Some(l) } });
    }
    if let Some(dv) = ctx.query("docvalue_fields") {
        for f in list(dv) {
            source.docvalue_fields.push(crate::search_source::FieldAndFormat { field: f, format: None, include_unmapped: None });
        }
    }
    if let Some(t) = ctx.query("track_total_hits") {
        source.track_total_hits = Some(match t {
            "true" | "" => TrackTotalHits::Accurate,
            "false" => TrackTotalHits::Disabled,
            n => {
                let v = n.parse::<i32>().map_err(|_| EsError::illegal_argument(format!("Failed to parse int parameter [track_total_hits] with value [{n}]")))?;
                // `SearchSourceBuilder.trackTotalHitsUpTo`.
                if v < -1 {
                    return Err(EsError::illegal_argument(format!("[track_total_hits] parameter must be positive or equals to -1, got {v}")));
                }
                match v {
                    -1 => TrackTotalHits::Disabled,
                    i32::MAX => TrackTotalHits::Accurate,
                    n => TrackTotalHits::UpTo(n),
                }
            }
        });
    }
    for (p, slot) in [("explain", &mut source.explain), ("version", &mut source.version), ("seq_no_primary_term", &mut source.seq_no_primary_term)] {
        if let Some(b) = ctx.opt_bool_param(p)? {
            *slot = Some(b);
        }
    }
    if ctx.has_query("track_scores") {
        source.track_scores = ctx.bool_param("track_scores", false)?;
    }
    if ctx.has_query("terminate_after") {
        let t = ctx.int_param("terminate_after", 0)?;
        if t < 0 {
            return Err(EsError::illegal_argument("terminateAfter must be > 0"));
        }
        source.terminate_after = Some(t as i32);
    }
    if let Some(t) = ctx.query("timeout") {
        source.timeout = Some(crate::units::TimeValue::parse(t, "timeout")?);
    }
    retriever::apply(&mut source, ctx.query("sort").is_some())?;
    if ctx.has_query("search_type") && !source.knn.is_empty() {
        return Err(EsError::illegal_argument("cannot set [search_type] when using [knn] search, since the search type is determined automatically"));
    }
    let dfs = match ctx.query("search_type") {
        None | Some("query_then_fetch") => false,
        Some("dfs_query_then_fetch") => true,
        Some(o) => return Err(EsError::illegal_argument(format!("No search type for [{o}]"))),
    };
    let rest_total = ctx.bool_param("rest_total_hits_as_int", false)?;
    let scroll = match ctx.query("scroll") {
        Some(s) => Some(crate::units::TimeValue::parse(s, "scroll")?),
        None => None,
    };
    if let Some(f) = source.from.filter(|f| *f < 0) {
        return Err(EsError::illegal_argument(crate::search_source::negative_from(f)));
    }
    if let Some(sz) = source.size.filter(|s| *s < 0) {
        return Err(EsError::illegal_argument(format!("[size] parameter cannot be negative, found [{sz}]")));
    }
    if ctx.has_query("pre_filter_shard_size") && ctx.int_param("pre_filter_shard_size", 1)? < 1 {
        return Err(EsError::illegal_argument("preFilterShardSize must be >= 1"));
    }
    if ctx.has_query("max_concurrent_shard_requests") && ctx.int_param("max_concurrent_shard_requests", 5)? < 1 {
        return Err(EsError::illegal_argument("maxConcurrentShardRequests must be >= 1"));
    }
    if ctx.has_query("batched_reduce_size") && ctx.int_param("batched_reduce_size", 512)? < 2 {
        return Err(EsError::illegal_argument("batchedReduceSize must be >= 2"));
    }
    if rest_total && let Some(TrackTotalHits::UpTo(n)) = source.track_total_hits {
        return Err(EsError::illegal_argument(format!("[rest_total_hits_as_int] cannot be used if the tracking of total hits is not accurate, got {n}")));
    }
    let exprs = ctx.param("index").map(resolve::split_expressions).unwrap_or_default();
    let opts = ctx.indices_options(IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED)?;
    Ok(SearchRequest {
        indices: exprs,
        opts,
        source,
        routing: ctx.query("routing").filter(|r| !r.is_empty()).map(str::to_string),
        preference: ctx.query("preference").map(str::to_string),
        dfs,
        request_cache: ctx.opt_bool_param("request_cache")?,
        total_as_int: rest_total,
        typed_keys: ctx.bool_param("typed_keys", false)?,
        allow_partial: ctx.bool_param("allow_partial_search_results", true)?,
        scroll,
        include_named_queries_score: ctx.bool_param("include_named_queries_score", false)?,
        force_synthetic_source: ctx.bool_param("force_synthetic_source", false)?,
        fused: None,
        rank_only: false,
        now_ms: ctx.now_ms,
    })
}

/// `SearchRequest.validate` messages.
pub fn validate(req: &SearchRequest) -> Vec<String> {
    let s = &req.source;
    let mut errs = Vec::new();
    if req.scroll.is_some() {
        if s.from.is_some_and(|f| f > 0) {
            errs.push("using [from] is not allowed in a scroll context".into());
        }
        if s.size == Some(0) {
            errs.push("[size] cannot be [0] in a scroll context".into());
        }
        if s.track_total_hits == Some(TrackTotalHits::Disabled) {
            errs.push("disabling [track_total_hits] is not allowed in a scroll context".into());
        }
        if s.collapse.is_some() {
            errs.push("cannot use `collapse` in a scroll context".into());
        }
        if req.request_cache == Some(true) {
            errs.push("[request_cache] cannot be used in a scroll context".into());
        }
    }
    if s.search_after.is_some() && s.from.is_some_and(|f| f > 0) {
        errs.push("[from] parameter must be set to 0 when [search_after] is used".into());
    }
    if s.slice.is_some() && req.scroll.is_none() && s.pit.is_none() {
        errs.push("[slice] can only be used with [scroll] or [point-in-time] requests".into());
    }
    if s.sub_searches.len() >= 2 {
        errs.push("[sub_searches] requires [rank]".into());
    }
    // OpenSearch fails this on every shard instead (`SearchService.parseSource`).
    if !crate::opensearch::active() && s.stored_fields.as_ref().is_some_and(|sf| sf.fields.is_none()) && s.source.as_ref().is_some_and(|f| f.fetch) {
        errs.push("[stored_fields] cannot be disabled if [_source] is requested".into());
    }
    if stored_fields_disabled_with_fields(s) && !crate::opensearch::active() {
        errs.push("[stored_fields] cannot be disabled when using the [fields] option".into());
    }
    errs
}

/// `stored_fields: _none_` together with `fields`, which both dialects refuse.
fn stored_fields_disabled_with_fields(s: &crate::search_source::SearchSource) -> bool {
    !s.fields.is_empty() && s.stored_fields.as_ref().is_some_and(|sf| sf.fields.is_none())
}

/// The concrete indices of a search, with alias filters and search routing (`resolveIndices`
/// + `buildAliasFilter`).
#[allow(clippy::type_complexity)]
pub fn resolve_targets(cluster: &Cluster, st: &ClusterState, req: &SearchRequest) -> EsResult<Vec<(Arc<IndexMeta>, Option<crate::query::Query>, Option<String>)>> {
    let names = resolve::resolve(st, &req.indices, &req.opts, req.now_ms)?;
    let _ = cluster;
    let aliases = st.aliases();
    let all = req.indices.is_empty() || resolve::is_all(&req.indices);
    let mut out = Vec::new();
    for n in names {
        let Some(idx) = st.metadata.indices.get(&n).cloned() else { continue };
        if idx.is_closed() {
            // Searches forbid closed indices (also expanded by `expand_wildcards=closed`).
            if req.opts.ignore_unavailable {
                continue;
            }
            return Err(EsError::closed_index(&idx.name, &idx.uuid));
        }
        // Alias filters apply when the index is reached only through filtered aliases.
        let mut direct = all;
        let mut filters: Vec<crate::query::Query> = Vec::new();
        let mut routing: Option<String> = None;
        let mut via_alias = false;
        for e in &req.indices {
            if e == &n || (resolve::is_wildcard(e) && crate::xcontent::filter::glob_match(e, &n)) || st.metadata.data_streams.get(e.as_str()).is_some_and(|d| d.indices.contains(&n)) {
                direct = true;
            }
            // Data stream aliases filter per data stream.
            for (a, dsa) in &st.metadata.data_stream_aliases {
                if !(a == e || (resolve::is_wildcard(e) && crate::xcontent::filter::glob_match(e, a))) {
                    continue;
                }
                for ds in &dsa.data_streams {
                    if !st.metadata.data_streams.get(ds).is_some_and(|d| d.contains(&n)) {
                        continue;
                    }
                    via_alias = true;
                    match dsa.filters.get(ds) {
                        Some(f) => filters.push(crate::query::parse_stored(f)?),
                        None => direct = true,
                    }
                }
            }
            for (a, members) in aliases.iter() {
                if !(a == e || (resolve::is_wildcard(e) && crate::xcontent::filter::glob_match(e, a))) {
                    continue;
                }
                if let Some((_, am)) = members.iter().find(|(i, _)| i.name == n) {
                    via_alias = true;
                    match &am.filter {
                        Some(f) => filters.push(crate::query::parse_stored(f)?),
                        None => direct = true,
                    }
                    if let Some(r) = &am.search_routing {
                        routing = Some(match routing {
                            Some(prev) => format!("{prev},{r}"),
                            None => r.clone(),
                        });
                    }
                }
            }
        }
        let filter = if direct || !via_alias || filters.is_empty() {
            None
        } else if filters.len() == 1 {
            filters.pop()
        } else {
            let b = crate::query::compound::Bool { should: filters, ..Default::default() };
            Some(crate::query::Query::from(b))
        };
        out.push((idx, filter, routing));
    }
    out.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    Ok(out)
}

/// The shards a request reaches.
pub fn shard_targets(cluster: &Cluster, st: &ClusterState, req: &SearchRequest) -> EsResult<Vec<Target>> {
    let mut out = Vec::new();
    let pref = match &req.preference {
        Some(p) => Some(crate::meta::routing::Preference::parse(p, cluster.dialect == crate::cluster::Dialect::OpenSearch)?),
        None => None,
    };
    // `indices_boost` expressions resolve like index expressions (first matching entry wins).
    let mut boosts: Vec<(Vec<String>, f32)> = Vec::with_capacity(req.source.indices_boost.len());
    let remotes: Vec<&str> = req.source.indices_boost.iter().map(|(n, _)| n.as_str()).filter(|n| n.contains(':')).collect();
    if !remotes.is_empty() {
        return Err(EsError::illegal_argument(format!("Cross-cluster calls are not supported in this context but remote indices were requested: [{}]", remotes.join(", "))));
    }
    for (expr, b) in &req.source.indices_boost {
        match resolve::resolve(st, std::slice::from_ref(expr), &req.opts, req.now_ms) {
            Ok(names) => boosts.push((names, *b)),
            Err(e) if !req.opts.ignore_unavailable => return Err(e),
            Err(_) => {}
        }
    }
    for (idx, filter, alias_routing) in resolve_targets(cluster, st, req)? {
        if cluster.shards_failed(st, &idx) {
            continue;
        }
        let engine = cluster.engine.index(cluster, st, &idx)?;
        if req.routing.is_some() && crate::engine::tsdb::is_time_series(&idx) {
            return Err(crate::engine::tsdb::unsupported(&idx, "searching with a specified routing"));
        }
        let routing = req.routing.clone().or(alias_routing);
        let mut ids: Vec<u32> = match &routing {
            Some(r) => {
                let mut v = Vec::new();
                for part in r.split(',') {
                    v.extend(crate::meta::routing::search_shards_for(&idx, part.trim()));
                }
                v.sort_unstable();
                v.dedup();
                v
            }
            None => (0..idx.number_of_shards()).collect(),
        };
        if let Some(p) = &pref
            && let Some(s) = &p.shards
        {
            ids.retain(|i| s.contains(i));
        }
        let boost = boosts.iter().find(|(names, _)| names.iter().any(|n| *n == idx.name)).map(|(_, b)| *b).unwrap_or(1.0);
        for i in ids {
            out.push(Target { idx: idx.clone(), engine: engine.clone(), shard: engine.shard(i).clone(), alias_filter: filter.clone(), boost, searcher: None });
        }
    }
    Ok(out)
}

/// `index.max_result_window` checks.
pub(crate) fn check_window(idx: &IndexMeta, from: i64, size: i64, scroll: bool) -> EsResult<()> {
    let max = idx.settings.get("index.max_result_window").and_then(|v| v.parse::<i64>().ok()).unwrap_or(10_000);
    // A scroll can't set `from` (`validate`): there it is the offset a continuation reached,
    // and only the batch size counts.
    let window = if scroll { size } else { from + size };
    if window > max {
        let what = if scroll { "Batch size is too large, size must be less than or equal to" } else { "Result window is too large, from + size must be less than or equal to" };
        return Err(EsError::illegal_argument(if scroll {
            format!("{what}: [{max}] but was [{size}]. Scroll batch sizes cost as much memory as result windows so they are controlled by the [index.max_result_window] index level setting.")
        } else {
            format!("{what}: [{max}] but was [{}]. See the scroll api for a more efficient way to request large data sets. This limit can be set by changing the [index.max_result_window] index level setting.", from + size)
        }));
    }
    Ok(())
}

/// `DefaultSearchContext.preProcess`: rescoring needs the top docs by score
/// (`RescorePhase.validateSort`: `_score` descending, then at most a `_shard_doc` tie-break).
fn check_rescore_sort(src: &SearchSource) -> EsResult<()> {
    let shard_doc = |s: &Sort| matches!(s, Sort::Field { field, order, .. } if field == "_shard_doc" && *order != Some(SortOrder::Desc));
    let ok = match src.sort.as_slice() {
        [] | [Sort::Score { order: SortOrder::Desc }] => true,
        [Sort::Score { order: SortOrder::Desc }, d] => shard_doc(d) && !crate::opensearch::active(),
        _ => false,
    };
    if src.rescore.is_empty() || ok {
        return Ok(());
    }
    Err(EsError::illegal_argument("Cannot use [sort] option in conjunction with [rescore]."))
}

/// Per-index request limits checked on the shard (`DefaultSearchContext.preProcess`).
/// What a `slice` keeps of one shard.
enum SliceFilter {
    All,
    None,
    Docs(QueryRef),
}

/// `SliceBuilder.toFilter`: with several shards the slices are spread over them first (shard
/// `ti` of the request's `shards`), and only the shard's own sub-slices filter documents:
/// by `_id` hashes for scrolls, doc id ranges for points in time, or a numeric field's values.
fn slice_filter(qctx: &QueryCtx, sl: &crate::search_source::Slice, ti: usize, shards: usize, scroll: bool) -> EsResult<SliceFilter> {
    let (id, max) = (sl.id.max(0) as usize, sl.max.max(1) as usize);
    // Scrolls slice every shard alike (`SliceBuilder.withoutShardOptimization`); points in
    // time spread the slices over the shards. OpenSearch spreads both, and slices by `_id`
    // when no field is given (it has no doc id slicing).
    let os = crate::opensearch::active();
    let (id, max) = if shards == 1 || (scroll && !os) {
        (id, max)
    } else if max > shards {
        let target = id % shards;
        if target != ti {
            return Ok(SliceFilter::None);
        }
        let in_shard = max / shards + usize::from(max % shards > target);
        if in_shard == 1 {
            return Ok(SliceFilter::All);
        }
        (id / shards, in_shard)
    } else {
        return Ok(if id == ti % max { SliceFilter::All } else { SliceFilter::None });
    };
    let by = match sl.field.as_deref() {
        None if !scroll && !os => search_index::query::SliceBy::DocId,
        // `TermsSliceQuery` hashes the indexed `_id` term, which Elasticsearch stores encoded
        // (`Uid.encodeId`): the ids of the slice are picked by hashing that encoding.
        None | Some("_id") => {
            let mut ids: Vec<Vec<u8>> = Vec::new();
            for seg in qctx.searcher.segments() {
                let Some(terms) = seg.reader.terms(search_index::schema::ID_FIELD) else { continue };
                let mut it = terms.iter();
                while let Some(t) = it.next() {
                    let Ok(s) = std::str::from_utf8(t) else { continue };
                    let h = crate::meta::routing::murmur3_x86_32(&encode_uid(s), 7919);
                    if (h as i64).rem_euclid(max as i64) == id as i64 {
                        ids.push(t.to_vec());
                    }
                }
            }
            if ids.is_empty() {
                return Ok(SliceFilter::None);
            }
            return Ok(SliceFilter::Docs(arc(search_index::query::ConstantScoreQuery::new(arc(search_index::query::MultiTermQuery::set(search_index::schema::ID_FIELD, ids))))));
        }
        Some(field) => {
            let Some(f) = qctx.table.get(field) else { return Err(EsError::illegal_argument(format!("field {field} not found"))) };
            if !f.doc_values || !(f.kind.is_numeric() || f.kind.is_date() || f.kind == crate::engine::fields::Kind::Boolean) {
                return Err(EsError::illegal_argument(format!("cannot load numeric doc values on {field}")));
            }
            search_index::query::SliceBy::DocValues { field: f.number }
        }
    };
    let q = search_index::query::SliceQuery::new(by, id as u32, max as u32).map_err(crate::engine::convert::index_error)?;
    Ok(SliceFilter::Docs(arc(q)))
}

/// The `_id` slice `id` of `max` of a sliced scroll on one shard (by-query slices): `None`
/// when no document of the shard is in it.
pub(crate) fn id_slice_query(qctx: &QueryCtx, id: u32, max: u32) -> EsResult<Option<QueryRef>> {
    let sl = crate::search_source::Slice { field: None, id: id as i32, max: max as i32 };
    Ok(match slice_filter(qctx, &sl, 0, 1, true)? {
        SliceFilter::All => Some(arc(search_index::query::MatchAllQuery)),
        SliceFilter::None => None,
        SliceFilter::Docs(q) => Some(q),
    })
}

/// `Uid.encodeId`: positive numeric ids pack two digits a byte, URL-safe base64 ids are
/// stored decoded, anything else as UTF-8 behind a marker byte.
pub(crate) fn encode_uid(id: &str) -> Vec<u8> {
    if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
        let d = id.as_bytes();
        let mut out = Vec::with_capacity(1 + d.len().div_ceil(2));
        out.push(0xfe);
        for pair in d.chunks(2) {
            let hi = pair[0] - b'0';
            let lo = pair.get(1).map(|c| c - b'0').unwrap_or(0x0f);
            out.push((hi << 4) | lo);
        }
        return out;
    }
    if url_base64_without_padding(id) {
        use base64::Engine as _;
        if let Ok(mut b) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(id) {
            if b.first().is_some_and(|f| *f >= 0xfd) {
                b.insert(0, 0xfd);
            }
            return b;
        }
    }
    let mut out = Vec::with_capacity(id.len() + 1);
    out.push(0xff);
    out.extend_from_slice(id.as_bytes());
    out
}

/// `Uid.isURLBase64WithoutPadding`.
fn url_base64_without_padding(id: &str) -> bool {
    let last = id.as_bytes().last().copied();
    let ok_tail = match id.len() & 3 {
        0 => true,
        1 => false,
        2 => matches!(last, Some(b'A' | b'Q' | b'g' | b'w')),
        _ => matches!(last, Some(b'A' | b'E' | b'I' | b'M' | b'Q' | b'U' | b'Y' | b'c' | b'g' | b'k' | b'o' | b's' | b'w' | b'0' | b'4' | b'8')),
    };
    ok_tail && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// `DefaultSearchContext.preProcess`: a scroll's `slice.max` within `index.max_slices_per_scroll`.
fn check_slices(idx: &IndexMeta, src: &SearchSource, scroll: bool) -> EsResult<()> {
    let Some(sl) = &src.slice else { return Ok(()) };
    if scroll {
        check_slice_count(idx, sl.max as i64)?;
    }
    Ok(())
}

/// `index.max_slices_per_scroll` (default 1024).
pub fn max_slices_per_scroll(idx: &IndexMeta) -> i64 {
    idx.settings.get("index.max_slices_per_scroll").and_then(|v| v.parse::<i64>().ok()).unwrap_or(1024)
}

/// A sliced scroll of `max` slices over `idx` (also each by-query slice's scroll).
pub fn check_slice_count(idx: &IndexMeta, max: i64) -> EsResult<()> {
    let limit = max_slices_per_scroll(idx);
    if max > limit {
        return Err(EsError::illegal_argument(format!(
            "The number of slices [{max}] is too large. It must be less than [{limit}]. This limit can be set by changing the [index.max_slices_per_scroll] index level setting."
        )));
    }
    Ok(())
}

fn check_limits(idx: &IndexMeta, src: &SearchSource) -> EsResult<()> {
    let setting = |k: &str, d: i64| idx.settings.get(k).and_then(|v| v.parse::<i64>().ok()).unwrap_or(d);
    let max_result = setting("index.max_result_window", 10_000);
    let max_rescore = setting("index.max_rescore_window", max_result);
    for r in &src.rescore {
        let w = r.window_size.unwrap_or(10) as i64;
        if w > max_rescore {
            return Err(EsError::illegal_argument(format!(
                "Rescore window [{w}] is too large. It must be less than [{max_rescore}]. This prevents allocating massive heaps for storing the results to be rescored. This limit can be set by changing the [index.max_rescore_window] index level setting."
            )));
        }
    }
    let max_dv = setting("index.max_docvalue_fields_search", 100);
    if src.docvalue_fields.len() as i64 > max_dv {
        return Err(EsError::illegal_argument(format!(
            "Trying to retrieve too many docvalue_fields. Must be less than or equal to: [{max_dv}] but was [{}]. This limit can be set by changing the [index.max_docvalue_fields_search] index level setting.",
            src.docvalue_fields.len()
        )));
    }
    let max_sf = setting("index.max_script_fields", 32);
    if src.script_fields.len() as i64 > max_sf {
        return Err(EsError::illegal_argument(format!(
            "Trying to retrieve too many script_fields. Must be less than or equal to: [{max_sf}] but was [{}]. This limit can be set by changing the [index.max_script_fields] index level setting.",
            src.script_fields.len()
        )));
    }
    Ok(())
}

/// The query of one shard: the request's query, alias filter, nested root filter.
pub(crate) fn shard_query(qctx: &QueryCtx, t: &Target, src: &SearchSource) -> EsResult<QueryRef> {
    shard_query_knn(qctx, t, src, &[])
}

fn shard_query_knn(qctx: &QueryCtx, t: &Target, src: &SearchSource, knn: &[QueryRef]) -> EsResult<QueryRef> {
    let main = match (&src.query, knn.is_empty()) {
        (Some(q), true) => qctx.to_query(q)?,
        (None, true) => arc(MatchAllQuery),
        (q, false) => {
            let mut b = BooleanQuery::new();
            if let Some(q) = q {
                b = b.add(Occur::Should, qctx.to_query(q)?);
            }
            for k in knn {
                b = b.add(Occur::Should, k.clone());
            }
            if b.clauses.len() == 1 { b.clauses[0].query.clone() } else { arc(b) }
        }
    };
    shard_query_main(qctx, t, main)
}

/// The shard query around `main`: the nested root filter and alias filter, the index boost.
fn shard_query_main(qctx: &QueryCtx, t: &Target, main: QueryRef) -> EsResult<QueryRef> {
    // `DefaultSearchContext.buildFilteredQuery`: the non-nested filter first, and only where
    // the query (and the alias filter) might match nested documents.
    let alias = match &t.alias_filter {
        Some(af) => Some(qctx.to_filter(af)?),
        None => None,
    };
    let mut filters: Vec<QueryRef> = Vec::new();
    if qctx.table.nested_paths().next().is_some()
        && might_match_nested_docs(qctx.table, &main)
        && alias.as_ref().is_none_or(|a| might_match_nested_docs(qctx.table, a))
    {
        filters.push(search_index::query::nested::root_filter());
    }
    filters.extend(alias);
    let q = if filters.is_empty() {
        main
    } else {
        let mut b = BooleanQuery::new().add(Occur::Must, main);
        for f in filters {
            b = b.add(Occur::Filter, f);
        }
        arc(b)
    };
    Ok(if t.boost != 1.0 { arc(search_index::query::BoostQuery::new(q, t.boost)) } else { q })
}

/// `NestedHelper.mightMatchNestedDocs`: whether a built query may match nested documents
/// (anything it doesn't recognise may). Term, term-set and point-range queries look at their
/// field: a meta field may, an unmapped one can't, a mapped one may when it lives under a
/// nested object.
pub fn might_match_nested_docs(table: &crate::engine::fields::FieldTable, q: &QueryRef) -> bool {
    let field = |n: u32| -> bool {
        match table.by_number.get(&n) {
            Some(f) => f.name.starts_with('_') || f.nested_path.is_some(),
            // Engine-reserved fields (`_id`, `_routing`, …) aren't in the table: meta fields.
            None => true,
        }
    };
    // `ESToParentBlockJoinQuery.getPath() != null`: a nested query inside another nested one
    // joins to nested parents; a top-level one (joined to the root documents) can't match a
    // nested document.
    let join = |j: &search_index::query::nested::ToParentQuery| {
        !j.parent_filter.as_any().downcast_ref::<search_index::query::FieldExistsQuery>().is_some_and(|f| f.field == search_index::schema::PRIMARY_TERM_FIELD)
    };
    might_match(q, &field, &join)
}

/// `NestedHelper.mightMatchNonNestedDocs`: whether a built query may match documents other
/// than the nested documents of `path` (a `nested` query skips its child filter when it
/// can't). A field may when it is a meta field, lives outside `path`, below a deeper nested
/// object, or is copied into the parent or root (`include_in_parent` / `include_in_root`); an
/// unmapped one can't.
pub fn might_match_non_nested_docs(table: &crate::engine::fields::FieldTable, q: &QueryRef, path: &str) -> bool {
    let field = |n: u32| -> bool {
        let Some(f) = table.by_number.get(&n) else { return true };
        if f.name.starts_with('_') {
            return true;
        }
        match &f.nested_path {
            Some(p) if p.starts_with(path) => p != path || table.objects.get(p.as_str()).is_some_and(|o| o.include_in_parent || o.include_in_root),
            _ => true,
        }
    };
    might_match(q, &field, &|_| true)
}

/// The query walk both `NestedHelper` predicates share.
fn might_match(q: &QueryRef, field: &dyn Fn(u32) -> bool, join: &dyn Fn(&search_index::query::nested::ToParentQuery) -> bool) -> bool {
    use search_index::query::{IndexOrDocValuesQuery, MultiTermQuery, PointRangeQuery, TermQuery};
    let a = q.as_any();
    if let Some(c) = a.downcast_ref::<search_index::query::ConstantScoreQuery>() {
        return might_match(&c.query, field, join);
    }
    if let Some(b) = a.downcast_ref::<search_index::query::BoostQuery>() {
        return might_match(&b.query, field, join);
    }
    if a.is::<MatchAllQuery>() {
        return true;
    }
    if a.is::<search_index::query::MatchNoneQuery>() {
        return false;
    }
    if let Some(t) = a.downcast_ref::<TermQuery>() {
        return field(t.field);
    }
    if let Some(m) = a.downcast_ref::<MultiTermQuery>()
        && let search_index::query::multiterm::TermsMatcher::Set(terms) = &m.matcher
    {
        return !terms.is_empty() && field(m.field);
    }
    if let Some(r) = a.downcast_ref::<PointRangeQuery>() {
        return field(r.field);
    }
    if let Some(i) = a.downcast_ref::<IndexOrDocValuesQuery>() {
        return might_match(&i.index, field, join);
    }
    // Lucene's `IndexSortSortedNumericDocValuesRangeQuery` (integral `term` and `range`
    // queries) isn't one of the shapes `NestedHelper` looks into: it may match anything.
    if let Some(b) = a.downcast_ref::<BooleanQuery>() {
        return if b.clauses.iter().any(|c| c.occur.is_required()) {
            b.clauses.iter().filter(|c| c.occur.is_required()).all(|c| might_match(&c.query, field, join))
        } else {
            b.clauses.iter().filter(|c| c.occur == Occur::Should).any(|c| might_match(&c.query, field, join))
        };
    }
    if let Some(j) = a.downcast_ref::<search_index::query::nested::ToParentQuery>() {
        return join(j);
    }
    true
}

fn total_threshold(src: &SearchSource) -> Option<u64> {
    match src.track_total_hits {
        None => Some(10_000),
        Some(TrackTotalHits::Accurate) => Some(u64::MAX),
        Some(TrackTotalHits::Disabled) => None,
        Some(TrackTotalHits::UpTo(n)) => Some(n.max(0) as u64),
    }
}

/// `InnerHitContextBuilder.extractInnerHits`, which runs before the query is built: inner hits
/// are keyed by name (the `nested` path, or the join type, when unnamed) per level — the
/// query's, then each inner hit's own children — and a second definition under a name fails
/// the shard. `bool` contributes its `filter`, `must` and `should` clauses (not `must_not`:
/// no hits come from it), the other compound queries their inner queries.
pub(crate) fn check_inner_hit_names(src: &SearchSource) -> EsResult<()> {
    use crate::query::Query as Q;
    fn level(q: &Q, names: &mut Vec<String>) -> Result<(), String> {
        let entry = |ih: &Option<crate::query::joining::InnerHits>, default: &str, inner: &Q, names: &mut Vec<String>| -> Result<(), String> {
            let Some(ih) = ih else { return Ok(()) };
            let name = ih.name.clone().unwrap_or_else(|| default.to_string());
            if names.contains(&name) {
                return Err(name);
            }
            level(inner, &mut Vec::new())?;
            names.push(name);
            Ok(())
        };
        match q {
            Q::Bool(b) => b.filter.iter().chain(&b.must).chain(&b.should).try_for_each(|c| level(c, names)),
            Q::Boosting(b) => level(&b.positive, names).and_then(|_| level(&b.negative, names)),
            Q::ConstantScore(c) => level(&c.filter, names),
            Q::DisMax(d) => d.queries.iter().try_for_each(|c| level(c, names)),
            Q::FunctionScore(f) => level(&f.query, names),
            Q::Nested(n) => entry(&n.inner_hits, &n.path, &n.query, names),
            Q::HasChild(h) => entry(&h.inner_hits, &h.ty, &h.query, names),
            Q::HasParent(h) => entry(&h.inner_hits, &h.parent_type, &h.query, names),
            _ => Ok(()),
        }
    }
    // `SearchService.parseSource` extracts the query's and then the post filter's inner hits
    // into one map.
    let mut names = Vec::new();
    match src.query.iter().chain(&src.post_filter).try_for_each(|q| level(q, &mut names)) {
        Err(name) => Err(EsError::illegal_argument(format!("[inner_hits] already contains an entry for key [{name}]"))),
        Ok(()) => Ok(()),
    }
}

/// `QueryPhaseCollectorManager.shortcutTotalHitCount`: the total of a match-all, of a term
/// query on a reader without deletions, or of an exists query over doc values with points or
/// terms, read from the index instead of counted; `None` for anything else.
fn shortcut_total_hit_count(searcher: &Searcher, q: &QueryRef) -> EsResult<Option<u64>> {
    use search_index::query::{BoostQuery, ConstantScoreQuery, FieldExistsQuery, TermQuery};
    let ie = crate::engine::convert::index_error;
    let mut q = searcher.rewrite(q).map_err(ie)?;
    loop {
        let next = if let Some(c) = q.as_any().downcast_ref::<ConstantScoreQuery>() {
            c.query.clone()
        } else if let Some(b) = q.as_any().downcast_ref::<BoostQuery>() {
            b.query.clone()
        } else if let Some(d) = q.as_any().downcast_ref::<ids::Described>() {
            // Printed like Elasticsearch's query (`_field_names:f`), counted like it.
            d.inner.clone()
        } else {
            break;
        };
        q = next;
    }
    let deletions = searcher.segments().iter().any(|s| s.live.is_some());
    if q.as_any().is::<MatchAllQuery>() {
        return Ok(Some(searcher.num_docs() as u64));
    }
    if q.as_any().is::<TermQuery>() && !deletions {
        return searcher.count_if_cheap(&q).map_err(ie);
    }
    if let Some(e) = q.as_any().downcast_ref::<FieldExistsQuery>()
        && !deletions
    {
        let mut count = 0u64;
        for seg in searcher.segments() {
            let Some(fi) = seg.reader.field_info(e.field) else { continue };
            if fi.doc_values == search_codec::DocValuesType::None {
                return Ok(None);
            }
            if fi.points.is_some() {
                count += seg.reader.points(e.field).map_or(0, |p| p.doc_count() as u64);
            } else if fi.index != search_codec::IndexOptions::None {
                count += seg.reader.terms(e.field).map_or(0, |t| t.stats().doc_count as u64);
            } else {
                return Ok(None);
            }
        }
        return Ok(Some(count));
    }
    Ok(None)
}

/// Whether Elasticsearch's `PartialHitCountCollector` (the hit count of a `size: 0` search
/// with a numeric `track_total_hits`) terminates early on this shard, making its total a
/// lower bound. Before each segment and before each collected doc it stops once `limit` docs
/// were collected; segments whose count the weight knows without iterating (`Weight::count`,
/// only offered when no post filter or `min_score` stands between) are added whole without
/// counting as collected. So exactly `limit` matches read `gte` when the collected ones are
/// all found before the last segment starts, and `track_total_hits: 0` always does.
fn partial_count_stops(searcher: &Searcher, q: &QueryRef, post: Option<&QueryRef>, limit: u64) -> EsResult<bool> {
    // The nested root filter (`_primary_term` doc values) gives Lucene no count: on indices
    // with nested fields every segment is collected.
    let root_filtered = {
        let b = q.as_any().downcast_ref::<search_index::query::BoostQuery>().map_or(q, |b| &b.query);
        b.as_any().downcast_ref::<BooleanQuery>().is_some_and(|b| {
            b.clauses.iter().any(|c| {
                c.query.as_any().downcast_ref::<search_index::query::FieldExistsQuery>().is_some_and(|f| f.field == search_index::schema::PRIMARY_TERM_FIELD)
            })
        })
    };
    let counts_offered = post.is_none() && !root_filtered;
    let q = match post {
        Some(p) => arc(search_index::query::BooleanQuery::new().add(Occur::Must, q.clone()).add(Occur::Filter, p.clone())),
        None => q.clone(),
    };
    let w = searcher.create_weight(&q, search_index::search::ScoreMode::CompleteNoScores).map_err(crate::engine::convert::index_error)?;
    let segs = searcher.segments();
    let mut collected = 0u64;
    for i in 0..segs.len() {
        if collected >= limit {
            return Ok(true);
        }
        if counts_offered && w.count(&segs[i]).map_err(crate::engine::convert::index_error)?.is_some() {
            continue;
        }
        let mut c = search_index::collector::top_docs::TotalHitCountCollector::default();
        search_index::search::search_segments(&segs[i..=i], &*w, &mut c, &search_index::search::SearchControl::default()).map_err(crate::engine::convert::index_error)?;
        collected += c.total;
    }
    Ok(false)
}

thread_local! {
    /// Set by [`run_collect`] when the search's deadline stopped collection (partial results).
    static SHARD_TIMED_OUT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `SearchTimeoutException`: a timed-out shard of a search that allows no partial results.
fn search_timeout_error() -> EsError {
    EsError::new(429, "search_timeout_exception", "Time exceeded").with_meta("timed_out", true)
}

/// Whether an error is the search-index deadline (`TimeExceededException`).
fn is_time_exceeded(e: &EsError) -> bool {
    e.kind == "time_exceeded_exception"
}

/// When the query phase of `req` stops collecting (`QueryPhase.getTimeoutCheck`): the
/// request's `timeout`, else `search.default_search_timeout`; never for a scroll or for
/// `-1` (`NO_TIMEOUT`). Elasticsearch counts whole milliseconds and times out once the clock
/// is past `start + timeout` (`time > maxTime`), so `timeout=0` lets a search that takes less
/// than a millisecond finish.
fn search_deadline(st: &ClusterState, req: &SearchRequest) -> Option<std::time::Instant> {
    if req.scroll.is_some() {
        return None;
    }
    let nanos = match &req.source.timeout {
        Some(t) => t.nanos(),
        None => st.metadata.cluster_setting("search.default_search_timeout").and_then(|v| crate::units::TimeValue::parse(v, "search.default_search_timeout").ok())?.nanos(),
    };
    if nanos < 0 {
        return None;
    }
    let millis = (nanos / 1_000_000).min(i64::MAX as i128 / 2) as u64;
    std::time::Instant::now().checked_add(std::time::Duration::from_millis(millis.saturating_add(1)))
}

/// Runs the query phase on one shard.
///
/// `window` is the shard's request-window failure (`DefaultSearchContext.preProcess`), raised
/// once the query, aggregations and highlight queries are built, as Elasticsearch orders them.
///
/// The request's `timeout` (`QueryPhase` with `ContextIndexSearcher`'s timeout check) becomes
/// this thread's ambient search control, so collection and the scripts it runs (which poll it
/// in their loops) stop once it passes; what was collected until then is the shard's partial
/// result, `timed_out`.
#[allow(clippy::too_many_arguments)]
pub fn query_phase(cluster: &Cluster, st: &ClusterState, req: &SearchRequest, t: &Target, ti: usize, shards: usize, n: usize, after: Option<&[Value]>, aggs: Option<&search_aggs::AggRequest>, knn: &[QueryRef], window: Option<EsError>) -> EsResult<ShardResult> {
    let deadline = search_deadline(st, req);
    // The request's cancellation (its client went away) stays in force.
    let cancelled = search_index::search::ambient_control().and_then(|c| c.cancelled);
    // The runtime fields scripts read through `doc[...]` travel with the control.
    let scope = runtime::RuntimeScope::build(&cluster.scripts, cluster.state(), &t.idx, &t.engine.table(), t.shard.id as i32, req.source.runtime_mappings.as_ref())?;
    let ctl = (deadline.is_some() || cancelled.is_some() || scope.is_some()).then(|| search_index::search::SearchControl {
        deadline,
        cancelled,
        ext: scope.clone().map(|s| s as Arc<dyn std::any::Any + Send + Sync>),
    });
    let _ambient = search_index::search::set_ambient_control(ctl);
    SHARD_TIMED_OUT.with(|c| c.set(false));
    let r = query_phase_inner(cluster, st, req, t, ti, shards, n, after, aggs, knn, window);
    let timed_out = SHARD_TIMED_OUT.with(|c| c.replace(false));
    match r {
        Ok(mut sr) if timed_out => {
            if !req.allow_partial {
                return Err(search_timeout_error());
            }
            sr.timed_out = true;
            sr.runtime = scope;
            Ok(sr)
        }
        Ok(mut sr) => {
            sr.runtime = scope;
            Ok(sr)
        }
        Err(e) if is_time_exceeded(&e) => {
            if !req.allow_partial {
                return Err(search_timeout_error());
            }
            // Collection stopped where partial results can't be kept (a concurrent search, a
            // nested phase): the shard answers with no hits.
            let searcher = t.searcher()?;
            Ok(ShardResult {
                target: ti,
                searcher,
                query: query::match_none("timed out"),
                hits: Vec::new(),
                total: 0,
                relation: TotalRelation::EqualTo,
                max_score: None,
                aggs: None,
                terminated_early: false,
                named: Vec::new(),
                inner_hits: Vec::new(),
                profile: None,
                rescored: Vec::new(),
                percolate: Vec::new(),
                timed_out: true,
                runtime: None,
                match_weights: Default::default(),
            })
        }
        r => r,
    }
}

#[allow(clippy::too_many_arguments)]
fn query_phase_inner(cluster: &Cluster, st: &ClusterState, req: &SearchRequest, t: &Target, ti: usize, shards: usize, n: usize, after: Option<&[Value]>, aggs: Option<&search_aggs::AggRequest>, knn: &[QueryRef], window: Option<EsError>) -> EsResult<ShardResult> {
    let src = &req.source;
    // OpenSearch collapses only plain (not rescored) top hits.
    if crate::opensearch::active() && src.collapse.is_some() && !src.rescore.is_empty() {
        return Err(EsError::new(500, "search_exception", "cannot use `collapse` in conjunction with `rescore`"));
    }
    // OpenSearch's `FetchFieldsPhase` reads `fields` from `_source` only.
    if crate::opensearch::active() && !src.fields.is_empty() && src.size != Some(0) && t.idx.mapping.source.mode_name() == "disabled" {
        return Err(EsError::illegal_argument(format!(
            "Unable to retrieve the requested [fields] since _source is disabled in the mappings for index [{}]",
            t.idx.name
        )));
    }
    check_inner_hit_names(src)?;
    let profiler = src.profile.then(search_index::search::profile::Profiler::new);
    let searcher = match &profiler {
        Some(p) => t.searcher()?.with_profiler(Some(p.clone())),
        None => t.searcher()?,
    };
    let table = t.engine.table();
    let mut qctx = QueryCtx::new(cluster, st, &t.idx, &table, &searcher, req.now_ms);
    qctx.shard = t.shard.id;
    qctx.runtime_mappings = src.runtime_mappings.clone();
    qctx.alias_filter = t.alias_filter.clone();
    if let Some(rm) = &src.runtime_mappings {
        let names: Vec<&str> = rm.keys().map(String::as_str).collect();
        runtime::check_time_series(&t.idx, &names, true)?;
    }
    // Named top-level kNN searches match before the query's named queries.
    if !knn.is_empty() {
        let secs = knn::sections(src)?;
        if secs.len() == knn.len() {
            for (s, q) in secs.iter().zip(knn) {
                if let Some(n) = &s.common.name {
                    qctx.named.borrow_mut().push((n.clone(), q.clone()));
                }
            }
        }
    }
    let q = match &req.fused {
        // A compound retriever's rank docs; a request query (a collapsed group's inner hits)
        // restricts them.
        Some(f) => (|| {
            let rd = f.query(&qctx, ti, src.explain == Some(true))?;
            let main = match &src.query {
                Some(extra) => arc(BooleanQuery::new().add(Occur::Must, rd).add(Occur::Filter, qctx.to_filter(extra)?)),
                None => rd,
            };
            shard_query_main(&qctx, t, main)
        })(),
        None => shard_query_knn(&qctx, t, src, knn),
    }
    .map_err(|e| query::shard_exception(&t.idx, e))?;
    // OpenSearch's searcher sets its search context on the query it runs: only that one
    // approximates (`ApproximateScoreQuery.setContext`).
    let q = if crate::opensearch::active() { crate::opensearch::approximate::ApproximateScoreQuery::mark_top(q) } else { q };
    // OpenSearch slices by the shard's ordinal within its index (`SliceBuilder.toFilter`).
    let (slice_ord, slice_of) = if crate::opensearch::active() { (t.shard.id as usize, t.idx.number_of_shards() as usize) } else { (ti, shards) };
    let q = match &src.slice {
        Some(sl) => match slice_filter(&qctx, sl, slice_ord, slice_of, req.scroll.is_some())? {
            SliceFilter::All => q,
            SliceFilter::None => query::match_none("this shard is not part of the slice"),
            SliceFilter::Docs(f) => arc(BooleanQuery::new().add(Occur::Must, q).add(Occur::Filter, f)),
        },
        None => q,
    };
    if !src.knn.is_empty() {
        knn::inner_hits(&qctx, &knn::sections_with_inner(src)?)?;
    }
    // `SeqNoPrimaryTermPhase` on nested inner hits.
    if qctx.inner_hits.borrow().iter().any(|d| d.nested_path.is_some() && d.spec.seq_no_primary_term) {
        return Err(EsError::java(500, "unsupported_operation_exception", "nested documents are not assigned sequence numbers"));
    }
    // The shard request cache: size-0 results per shard until the next refresh.
    let cache_key = cacheable(req, t).then(|| {
        let mut k = request_cache_key(req, t);
        // A cached `_id` field aggregation must still fail once `_id` fielddata is turned off
        // (the shard resolves its fields before looking the cache up). Scripts read `_id`
        // while collecting, so a cached script aggregation keeps answering, as in OpenSearch.
        if id_fielddata::used_by(src) && id_fielddata::enabled(st) {
            k.push_str("|id_fielddata");
        }
        k
    });
    if let Some(key) = &cache_key {
        let cached = t.shard.request_cache.lock().get(key).cloned();
        // An entry an earlier instance of the shard had cached (a wake is no restart): computed
        // again below, counted as the hit it is.
        let warm = cached.as_ref().is_some_and(|a| a.is::<crate::engine::WarmCacheEntry>());
        if let Some(any) = cached
            && let Some(c) = any.downcast_ref::<CachedShard>()
        {
            t.shard.counters.request_cache_hit.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(ShardResult {
                target: ti,
                searcher: searcher.clone(),
                query: q,
                hits: Vec::new(),
                total: c.total,
                relation: c.relation,
                max_score: c.max_score,
                aggs: c.aggs.clone(),
                terminated_early: c.terminated_early,
                named: Vec::new(),
                inner_hits: Vec::new(),
                profile: None,
                rescored: Vec::new(),
                percolate: Vec::new(),
                timed_out: false,
                runtime: None,
                match_weights: Default::default(),
            });
        }
        let counter = if warm { &t.shard.counters.request_cache_hit } else { &t.shard.counters.request_cache_miss };
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    // `SearchService.parseSource` order: the post filter is built before the sort.
    let post: Option<QueryRef> = match &src.post_filter {
        Some(pf) => Some(qctx.to_filter(pf).map_err(|e| query::shard_exception(&t.idx, e))?),
        None => None,
    };
    let built = sort::build(&qctx, &src.sort)?;
    let threshold = total_threshold(src);
    let wanted = n;
    // `size: 0` collects no top docs at all (`QueryPhaseCollectorManager`'s `EmptyHits`): no
    // `from` window, no rescore window, no collapse.
    let empty_hits = src.size == Some(0);
    // Rescoring needs each shard's top `window_size` hits.
    let n = if empty_hits { 0 } else { src.rescore.iter().map(|r| r.window_size.unwrap_or(10).max(0) as usize).fold(n, usize::max) };
    let track_scores = src.track_scores;
    let c = &t.shard.counters;
    c.query_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if !src.stats.is_empty() {
        let mut g = c.groups.lock();
        for name in &src.stats {
            g.entry(name.clone()).or_insert((0, 0)).0 += 1;
        }
    }
    let started = std::time::Instant::now();
    if aggs.is_some()
        && let Some(raw) = &src.aggs
    {
        aggs::check_metric_scripts(st, raw)?;
    }
    let mut agg_profiler: Option<Arc<search_aggs::profile::Profiler>> = None;
    let mut agg_collector = match aggs {
        Some(a) => {
            let mut lookup = aggs::Lookup::new(cluster, st, t, &table, &searcher, req.now_ms).with_parent_fields(&src.fields);
            if !src.rescore.is_empty() {
                lookup = lookup.with_rescore(prepare_rescore(&QueryCtx::new(cluster, st, &t.idx, &table, &searcher, req.now_ms), src));
            }
            let max_buckets = st.metadata.cluster_setting("search.max_buckets").and_then(|v| v.parse::<usize>().ok());
            let opts = search_aggs::ShardOptions {
                max_buckets,
                // Top-level aggregations may answer from segment statistics only when the query
                // matches every live doc (`match_all`, nothing else filtering).
                matches_all: q.as_any().is::<MatchAllQuery>() && post.is_none() && src.min_score.is_none(),
                terminate_after: src.terminate_after.is_some_and(|t| t > 0),
                profiler: profiler.as_ref().map(|_| search_aggs::profile::Profiler::new()),
                budget: Some(cluster.breakers.request(cluster, st)),
                // OpenSearch (forked before 7.13) has no filter-by-filter aggregations.
                filter_by_filter: cluster.dialect != crate::cluster::Dialect::OpenSearch
                    && st.metadata.cluster_setting("search.aggs.rewrite_to_filter_by_filter") != Some("false"),
                // The query decides which docs are collected: date histograms count buckets
                // from the points tree under a range on their field, and OpenSearch profiles
                // report what its optimizations would do with it.
                query: (post.is_none() && src.min_score.is_none()).then(|| q.clone()),
            };
            agg_profiler = opts.profiler.clone();
            Some(a.collector(&lookup, &searcher, &opts).map_err(aggs::agg_error)?)
        }
        None => None,
    };
    if let Some(Value::Obj(hl)) = src.highlight.as_ref().map(|h| Value::Obj(h.clone())) {
        // Highlight queries are built with the search context (`HighlightBuilder.build`).
        let mut qs: Vec<&Value> = hl.get("highlight_query").into_iter().collect();
        if let Some(Value::Obj(fields)) = hl.get("fields") {
            qs.extend(fields.values().filter_map(|f| f.get("highlight_query")));
        }
        for hq in qs {
            let (parsed, _) = crate::query::parse_query(hq, None, &[])?;
            qctx.to_query(&parsed)?;
        }
    }
    if let Some(e) = window {
        return Err(e);
    }
    let mut terminated_early = false;
    // `SearchService.parseSource` builds the rescorers (their queries) before the collapse.
    if !src.rescore.is_empty() {
        for r in prepare_rescore(&qctx, src) {
            if !r.script {
                r.kind?;
            }
        }
    }
    let collapse = match &src.collapse {
        Some(c) => {
            let f = collapse_field(&qctx, &c.field)?;
            (!empty_hits).then_some(f)
        }
        None => None,
    };
    // `SearchContext.hasOnlySuggest` (no query, aggregations or knn): the query phase collects
    // nothing, so what only collection would fail on (a multi-valued collapse field) never
    // runs; the suggest phase answers.
    if src.suggest.is_some() && src.query.is_none() && src.knn.is_empty() && src.aggs.is_none() && profiler.is_none() {
        return Ok(ShardResult {
            target: ti,
            searcher: searcher.clone(),
            query: q,
            hits: Vec::new(),
            total: 0,
            relation: TotalRelation::EqualTo,
            max_score: None,
            aggs: None,
            terminated_early: false,
            named: Vec::new(),
            inner_hits: Vec::new(),
            profile: None,
            rescored: Vec::new(),
            percolate: Vec::new(),
            timed_out: false,
            runtime: None,
            match_weights: Default::default(),
        });
    }
    // OpenSearch's query phase whether it gets there or not (`QueryPhase`): the count
    // shortcut it would take, and whether the search runs in segment slices. Only profiles
    // and `terminated_early` show either.
    let os_concurrent = crate::opensearch::active() && crate::opensearch::profile::concurrent(st, &t.idx, src);
    let os_shortcut = crate::opensearch::active()
        && n == 0
        && post.is_none()
        && src.min_score.is_none()
        && src.terminate_after.is_none_or(|t| t == 0)
        && crate::opensearch::profile::count_shortcut(&searcher, &q);
    // Lucene's `IndexSearcher.rewrite(query, needsScores)`, which OpenSearch searches through,
    // wraps a query that needs no scores in a constant-score query: its profile shows that.
    let q = if profiler.is_some() && crate::opensearch::active() {
        let needs_scores = (n > 0 && (built.by_score || track_scores || built.keys.iter().any(|k| matches!(k, SortKey::Score { .. })) || src.sort.iter().any(|s| matches!(s, Sort::Script { .. }))))
            || src.min_score.is_some()
            || !src.rescore.is_empty()
            || agg_collector.as_ref().is_some_and(|a| a.score_mode().needs_scores());
        if needs_scores { q } else { arc(search_index::query::ConstantScoreQuery::new(q)) }
    } else {
        q
    };
    let mut pre_rescored: Option<Vec<Vec<u32>>> = None;
    // A scroll page continuing after this shard's last hit (`contexts::cursor_kind` admits
    // only plain score and `_doc` sorts, which the branches below continue).
    let scroll_page = contexts::page_after(ti);
    let scroll_after = match scroll_page {
        Some((kind, last)) => {
            if (kind == contexts::CursorKind::Score) != built.by_score {
                return Err(EsError::illegal_state("scroll continuation does not match the sort"));
            }
            last.filter(|_| kind == contexts::CursorKind::Score).map(|(score, doc)| search_index::collector::top_docs::ScoreDoc { doc, score })
        }
        None => None,
    };
    // `size: 0` with nothing else to collect: when every segment knows its match count
    // (`Weight::count`: match_all, a term without deletions, a fully covered range...) the
    // total needs no iteration (Elasticsearch's `shortcutTotalHitCount`); it is capped at the
    // `track_total_hits` threshold as the collectors would report it.
    // (`terminate_after` counts collected docs only: segments counted whole never reach it.)
    let cheap_total = if n == 0 && collapse.is_none() && profiler.is_none() && agg_collector.is_none() && post.is_none() && src.min_score.is_none() && after.is_none() {
        searcher.count_if_cheap(&q).map_err(crate::engine::convert::index_error)?
    } else {
        None
    };
    let (hits, total, relation, max_score) = if let Some(count) = cheap_total {
        let cap = threshold.unwrap_or(0);
        if count > cap { (Vec::new(), cap, TotalRelation::GreaterThanOrEqualTo, None) } else { (Vec::new(), count, TotalRelation::EqualTo, None) }
    } else if let Some(cf) = collapse.filter(|_| src.rescore_before_collapse && built.by_score) {
        // A rescorer retriever collapses its rescored window.
        let top = TopScoreCollector::new(n, threshold.unwrap_or(0).max(n as u64));
        let td = run_collect(&searcher, &q, top, post.as_ref(), src, agg_collector.as_mut(), &mut terminated_early, |c| c.top_docs())?;
        let first: Vec<ShardHit> = td.hits.iter().map(|h| ShardHit { doc: h.doc, score: h.score, sort: Vec::new(), group: None }).collect();
        let (rescored_hits, _, sets) = rescore(&qctx, &searcher, first, src)?;
        pre_rescored = Some(sets);
        let scored: QueryRef = arc(search_index::query::ScoreDocQuery::new(rescored_hits.iter().map(|h| (h.doc, h.score)).collect()));
        let mut top = search_index::collector::collapse::CollapseCollector::new(cf, vec![SortKey::Score { reverse: false }], wanted, wanted as u64, true).with_searcher(&searcher);
        searcher.search(&scored, &mut top).map_err(crate::engine::convert::index_error)?;
        let cd = top.top_docs();
        let hits: Vec<ShardHit> = cd.hits.iter().map(|h| ShardHit { doc: h.doc, score: h.score, sort: Vec::new(), group: Some(h.group.clone()) }).collect();
        let max = hits.iter().map(|h| h.score).filter(|s| !s.is_nan()).reduce(f32::max);
        (hits, td.total_hits, td.relation, max)
    } else if let Some(cf) = collapse {
        // `search_after` with `collapse` continues after a value of the collapse field, the
        // one sort: only groups past it remain.
        let outer_q = q.clone();
        let q = match (after, &src.collapse) {
            (Some(a), Some(c)) => {
                let field_sort = match src.sort.as_slice() {
                    [crate::search_source::Sort::Field { field, order, .. }] if *field == c.field => Some(*order == Some(SortOrder::Desc)),
                    _ => None,
                };
                let Some(desc) = field_sort.filter(|_| a.len() == 1) else {
                    if crate::opensearch::active() {
                        return Err(EsError::new(
                            500,
                            "search_exception",
                            "collapse field and sort field must be the same when use `collapse` in conjunction with `search_after`",
                        ));
                    }
                    return Err(EsError::illegal_argument(
                        "Cannot use [collapse] in conjunction with [search_after] unless the search is sorted on the same field. Multiple sort fields are not allowed.",
                    ));
                };
                let bound = a[0].clone();
                let range = crate::query::term_level::Range {
                    field: c.field.clone(),
                    from: (!desc).then(|| bound.clone()),
                    to: desc.then_some(bound),
                    include_lower: false,
                    include_upper: false,
                    time_zone: None,
                    format: None,
                    relation: None,
                    common: Default::default(),
                };
                let f = qctx.to_filter(&crate::query::Query::from(range)).map_err(|e| query::shard_exception(&t.idx, e))?;
                arc(BooleanQuery::new().add(Occur::Must, q.clone()).add(Occur::Filter, f))
            }
            _ => q.clone(),
        };
        let keys = if built.by_score { vec![SortKey::Score { reverse: false }] } else { built.keys.clone() };
        let top = search_index::collector::collapse::CollapseCollector::new(cf, keys, n, threshold.unwrap_or(0).max(n as u64), track_scores || built.by_score).with_searcher(&searcher);
        let td = run_collect(&searcher, &q, top, post.as_ref(), src, agg_collector.as_mut(), &mut terminated_early, |c| c.top_docs())?;
        let hits: Vec<ShardHit> = td
            .hits
            .iter()
            .map(|h| {
                let mut sort = if built.by_score { Vec::new() } else { h.values.clone() };
                sort::rescale(&mut sort, &built.formats);
                ShardHit { doc: h.doc, score: h.score, sort, group: Some(h.group.clone()) }
            })
            .collect();
        // The `search_after` bound only narrows the groups: totals count every match.
        let (total, relation) = if after.is_some() && threshold.is_some() {
            (searcher.count(&outer_q).map_err(crate::engine::convert::index_error)?, TotalRelation::EqualTo)
        } else {
            (td.total_hits, td.relation)
        };
        // `forCollapsing` tracks scores for a score sort or `track_scores` only: a field sort
        // that has `_score` as a tie-break reports no max score.
        (hits, total, relation, if built.by_score || track_scores { td.max_score } else { None })
    } else if built.by_score && profiler.is_none() && agg_collector.is_none() && post.is_none() && src.min_score.is_none() && src.terminate_after.is_none() && after.is_none() {
        // The concurrent fast path.
        let m = search_index::collector::compose::TopScoreManager { n, threshold: threshold.unwrap_or(0).max(n as u64), after: scroll_after, min_score: None };
        let td = sliced(&searcher, shards, &q).unwrap_or_else(|| searcher.clone()).search_concurrent(&q, &m, &Default::default()).map_err(crate::engine::convert::index_error)?;
        let max = td.max_score();
        let hits: Vec<ShardHit> = td.hits.iter().map(|h| ShardHit { doc: h.doc, score: h.score, sort: Vec::new(), group: None }).collect();
        (hits, td.total_hits, td.relation, max)
    } else if built.by_score {
        // A score-only sort is no sort (`SearchAfterBuilder.buildFieldDoc`).
        if after.is_some() {
            return Err(EsError::illegal_argument("Sort must contain at least one field."));
        }
        let mut top = TopScoreCollector::new(n, threshold.unwrap_or(0).max(n as u64));
        if let Some(a) = scroll_after {
            top = top.with_after(a);
        }
        let td = run_collect(&searcher, &q, top, post.as_ref(), src, agg_collector.as_mut(), &mut terminated_early, |c| c.top_docs())?;
        let max = td.max_score();
        let hits: Vec<ShardHit> = td.hits.iter().map(|h| ShardHit { doc: h.doc, score: h.score, sort: Vec::new(), group: None }).collect();
        (hits, td.total_hits, td.relation, max)
    } else {
        // `size: 0` collects no top docs, so `track_scores` scores nothing (a `min_score` still
        // does).
        let field_threshold = threshold.unwrap_or(0).max(n as u64);
        let field_scores = (track_scores && !empty_hits) || src.min_score.is_some();
        let mut field_after = None;
        if let Some((contexts::CursorKind::Doc, Some((_, d)))) = scroll_page {
            field_after = Some((vec![SortValue::Long(i64::from(d))], Some(d)));
        }
        if let Some(a) = after {
            if a.len() != built.keys.len() {
                return Err(EsError::illegal_argument(format!(
                    "search_after has {} value(s) but sort has {}.",
                    a.len(),
                    built.keys.len()
                )));
            }
            let mut vals = Vec::with_capacity(a.len());
            for (pos, ((v, k), f)) in a.iter().zip(&built.keys).zip(&built.formats).enumerate() {
                let field = match k {
                    SortKey::Field(fs) => table.by_number.get(&fs.field).map(|fi| fi.name.as_str()),
                    SortKey::GeoDistance(g) => table.by_number.get(&g.field).map(|fi| fi.name.as_str()),
                    // `ShardDocSortField` names its field; Lucene's `_doc` / `_score` don't.
                    _ if matches!(f, sort::ValueFormat::ShardDoc) => Some("_shard_doc"),
                    _ => None,
                };
                vals.push(sort::parse_after(v, k, f, req.now_ms, pos, field.unwrap_or("null"))?);
            }
            field_after = Some((vals, None));
        }
        let mut top = TopFieldCollector::new(built.keys.clone(), n, field_threshold, field_scores).with_searcher(&searcher);
        if let Some((vals, d)) = field_after {
            top = top.with_after(vals, d);
        }
        let td = run_collect(&searcher, &q, top, post.as_ref(), src, agg_collector.as_mut(), &mut terminated_early, |c| c.top_docs())?;
        let hits: Vec<ShardHit> = td
            .hits
            .iter()
            .map(|h: &FieldDoc| {
                let mut sort = h.values.clone();
                sort::rescale(&mut sort, &built.formats);
                ShardHit { doc: h.doc, score: h.score, sort, group: None }
            })
            .collect();
        // A field sort reports no max score unless `track_scores` is set.
        (hits, td.total_hits, td.relation, if track_scores { td.max_score } else { None })
    };
    // `terminate_after` stops collecting, but Elasticsearch takes the total of a search with
    // hits from `shortcutTotalHitCount` when the query allows (`QueryPhaseCollectorManager.WithHits`).
    let (total, relation) = match (src.terminate_after, threshold) {
        (Some(t), Some(limit)) if t > 0 && n > 0 && collapse.is_none() && post.is_none() && src.min_score.is_none() && profiler.is_none() => {
            match shortcut_total_hit_count(&searcher, &q)? {
                Some(c) if c > limit => (limit, TotalRelation::GreaterThanOrEqualTo),
                Some(c) => (c, TotalRelation::EqualTo),
                None => (total, relation),
            }
        }
        // `size: 0` with aggregations (`EmptyHits`): the hit count collector counts segments
        // `Weight.count` answers for whole while the aggregations collect; `terminate_after`
        // stops at the first segment after the one that reached it (`QueryPhaseCollector`
        // checks before each segment).
        (Some(t), Some(limit)) if t > 0 && n == 0 && agg_collector.is_some() && collapse.is_none() && post.is_none() && src.min_score.is_none() && after.is_none() => {
            match searcher.segment_counts_if_cheap(&q).map_err(crate::engine::convert::index_error)? {
                Some(counts) => {
                    let (mut c, mut collected) = (0u64, 0u64);
                    for m in counts {
                        if collected >= t as u64 {
                            break;
                        }
                        c += m;
                        collected = (collected + m).min(t as u64);
                    }
                    if c > limit { (limit, TotalRelation::GreaterThanOrEqualTo) } else { (c, TotalRelation::EqualTo) }
                }
                None => (total, relation),
            }
        }
        // A timed-out search keeps `shortcutTotalHitCount`'s total too: Elasticsearch reads it
        // before collecting, so the interruption doesn't shorten it.
        (_, Some(limit)) if n > 0 && SHARD_TIMED_OUT.with(std::cell::Cell::get) && collapse.is_none() && post.is_none() && src.min_score.is_none() && profiler.is_none() => {
            match shortcut_total_hit_count(&searcher, &q)? {
                Some(c) if c > limit => (limit, TotalRelation::GreaterThanOrEqualTo),
                Some(c) => (c, TotalRelation::EqualTo),
                None => (total, relation),
            }
        }
        _ => (total, relation),
    };
    // Exactly `track_total_hits` matches counted on a `size: 0` search: whether Elasticsearch's
    // collector stopped there (`gte`) depends on where the matches sit in the segments.
    let relation = match threshold {
        Some(limit)
            if n == 0
                && limit != u64::MAX
                && total == limit
                && relation == TotalRelation::EqualTo
                && collapse.is_none()
                && profiler.is_none()
                && src.terminate_after.is_none()
                && after.is_none()
                && src.min_score.is_none()
                && partial_count_stops(&searcher, &q, post.as_ref(), limit)? =>
        {
            TotalRelation::GreaterThanOrEqualTo
        }
        _ => relation,
    };
    // OpenSearch (3.2+): a search sorted on `_score` descending first reports the best score
    // as its max score; one sorted on anything else first only with `track_scores`.
    let max_score = if crate::opensearch::active() && !built.by_score && !(src.collapse.is_some() && src.rescore_before_collapse) {
        match built.keys.first() {
            Some(SortKey::Score { reverse: false }) => hits.iter().map(|h| h.score).filter(|s| !s.is_nan()).reduce(f32::max),
            _ if track_scores => max_score,
            _ => None,
        }
    } else {
        max_score
    };
    let (hits, max_score, rescored) = match pre_rescored {
        Some(sets) => (hits, max_score, sets),
        // A rescorer over a compound retriever rescores its rank docs only (they score above
        // zero; the rest match the children's queries but aren't ranked).
        None if !src.rescore.is_empty() && built.by_score && req.fused.is_some() => {
            let mut hits = hits;
            let tail = hits.split_off(hits.partition_point(|h| h.score > 0.0));
            let (mut out, max, sets) = rescore(&qctx, &searcher, hits, src)?;
            out.extend(tail);
            (out, max, sets)
        }
        // Elasticsearch's `QueryRescorer.combine` re-sorts the collapsed top docs' array in
        // place, so `RescorePhase.rewriteTopFieldGroups` pairs the doc now at rank `i` with
        // the group value of the doc that was at rank `i` before rescoring: the groups stay
        // where they were and the coordinator (and inner hits) merge by those.
        None if !src.rescore.is_empty() && built.by_score && collapse.is_some() && !crate::opensearch::active() => {
            let groups: Vec<Option<SortValue>> = hits.iter().map(|h| h.group.clone()).collect();
            let (mut out, max, sets) = rescore(&qctx, &searcher, hits, src)?;
            for (h, g) in out.iter_mut().zip(groups) {
                h.group = g;
            }
            (out, max, sets)
        }
        None if !src.rescore.is_empty() && built.by_score => rescore(&qctx, &searcher, hits, src)?,
        None => (hits, max_score, Vec::new()),
    };
    c.query_time_nanos.fetch_add(started.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
    let mut aggs_out = match agg_collector {
        Some(c) => Some(c.finish().map_err(aggs::agg_error)?),
        None => None,
    };
    // OpenSearch's concurrent search reduces a shard's aggregations on the shard first (only
    // their order changes; see `Aggs::opensearch_shard_order`).
    if os_concurrent
        && !searcher.segments().is_empty()
        && let Some(a) = aggs_out.as_mut()
    {
        let is_global = |name: &str| src.aggs.as_ref().and_then(|d| d.get(name)).and_then(Value::as_object).is_some_and(|d| d.contains_key("global"));
        a.opensearch_shard_order(&is_global);
    }
    let named = qctx.named.take();
    let inner = qctx.inner_hits.take();
    let profile = profiler.map(|p| {
        let nanos = started.elapsed().as_nanos() as u64;
        let agg_names: Vec<String> = src.aggs.as_ref().map(|a| a.keys().cloned().collect()).unwrap_or_default();
        let collector = if built.by_score { "TopScoreDocCollector" } else { "TopFieldCollector" };
        let field_type = |f: &str| table.get(f).map(|x| x.ty.clone());
        let aggregations = match &agg_profiler {
            Some(ap) => match aggs::from_json(&ap.to_json()) {
                Value::Arr(mut a) => {
                    if let Some(defs) = &src.aggs {
                        profile::fill_types(&mut a, defs, &field_type);
                    }
                    a
                }
                _ => Vec::new(),
            },
            None => src.aggs.as_ref().map(|a| profile::aggregations(a, &field_type, nanos, total)).unwrap_or_default(),
        };
        if crate::opensearch::active() {
            let is_global = |name: &String| src.aggs.as_ref().and_then(|a| a.get(name)).and_then(Value::as_object).is_some_and(|d| d.contains_key("global"));
            let max_docs: Vec<u32> = searcher.segments().iter().map(|s| s.max_doc()).collect();
            let processors = cluster.config.processors().unwrap_or_else(search_index::search::pool::cores);
            let phase = crate::opensearch::profile::Phase {
                concurrent: os_concurrent,
                slices: crate::opensearch::profile::slice_count(st, &t.idx, processors, &max_docs),
                size: n,
                by_score: built.by_score,
                paging: after.is_some(),
                exact_count: n == 0 && threshold == Some(u64::MAX) && !os_shortcut,
                post_filter: post.is_some(),
                min_score: src.min_score.is_some(),
                terminate_after: src.terminate_after.is_some_and(|t| t != 0),
                aggs: agg_names.iter().filter(|a| !is_global(a)).cloned().collect(),
                globals: agg_names.iter().filter(|a| is_global(a)).cloned().collect(),
            };
            let (query, rewrite) = profile::queries(&p);
            return profile::ShardProfile {
                searches: phase.searches(query, rewrite, nanos.max(1) as i64),
                aggregations: phase.aggregations(aggregations),
                dfs: req.dfs.then(|| profile::dfs_statistics(nanos / 8, u64::from(total > 0))),
            };
        }
        profile::ShardProfile {
            searches: vec![profile::search_entry(&p, collector, nanos, &agg_names)],
            aggregations,
            dfs: req.dfs.then(|| profile::dfs_statistics(nanos / 8, u64::from(total > 0))),
        }
    });
    // OpenSearch 3 searches aggregation requests segment-concurrently; with `size: 0` its hit
    // count collector manager (`EarlyTerminatingCollectorManager`) reports the shard as
    // `terminated_early` when it stops a slice: at once when the count isn't tracked or is
    // known up front (`shortcutTotalHitCount`), else once a slice collects past the
    // `track_total_hits` threshold — collecting, not counting: segments Lucene's
    // `Weight.count` answers for are never collected.
    if os_concurrent && n == 0 && collapse.is_none() && req.scroll.is_none() && !searcher.segments().is_empty() {
        let wrapped = post.is_some() || src.min_score.is_some();
        terminated_early = match threshold {
            None => true,
            _ if os_shortcut => true,
            Some(u64::MAX) => false,
            Some(_) => relation == TotalRelation::GreaterThanOrEqualTo && (wrapped || searcher.count_if_cheap(&q).ok().flatten().is_none()),
        };
    }
    if let Some(key) = cache_key.filter(|_| !SHARD_TIMED_OUT.with(|c| c.get())) {
        let entry = CachedShard { total, relation, max_score, aggs: aggs_out.clone(), terminated_early };
        let mut cache = t.shard.request_cache.lock();
        if cache.len() >= 1024 {
            cache.clear();
            t.shard.counters.request_cache_evictions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let bytes = key.len() as u64 + 256;
        // A warm entry's memory is counted already.
        if cache.insert(key, Arc::new(entry)).is_none_or(|old| !old.is::<crate::engine::WarmCacheEntry>()) {
            t.shard.counters.request_cache_bytes.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        }
    }
    Ok(ShardResult {
        target: ti,
        searcher: searcher.clone(),
        query: q,
        hits,
        total,
        relation,
        max_score,
        aggs: aggs_out,
        terminated_early,
        named,
        inner_hits: inner,
        profile,
        rescored,
        percolate: qctx.percolate.take(),
        timed_out: false,
        runtime: None,
        match_weights: Default::default(),
    })
}

/// A cached size-0 shard result.
struct CachedShard {
    total: u64,
    relation: TotalRelation,
    max_score: Option<f32>,
    aggs: Option<search_aggs::Aggs>,
    terminated_early: bool,
}

/// `IndicesService.canCache`: size 0, no scroll/dfs/profile, the parameter or
/// `index.requests.cache.enable`, and nothing that depends on the time or randomness.
fn cacheable(req: &SearchRequest, t: &Target) -> bool {
    let src = &req.source;
    // Rank docs (compound retrievers, OpenSearch's hybrid fusion) aren't part of the key.
    if req.request_cache == Some(false) || req.scroll.is_some() || req.dfs || src.profile || src.size.unwrap_or(10) != 0 || !src.knn.is_empty() || req.fused.is_some() {
        return false;
    }
    if req.request_cache.is_none() && t.idx.settings.get("index.requests.cache.enable") == Some("false") {
        return false;
    }
    let text = format!("{src:?}");
    !(text.contains("now") || text.contains("Script") || text.contains("random"))
}

fn request_cache_key(req: &SearchRequest, t: &Target) -> String {
    // The mapping and analysis settings decide how the request's queries are built
    // (`MappingLookup.cacheKey()`): an update to either busts the entries.
    let mut k = format!("{:?}|{:?}|{}|{}.{}", req.source, t.alias_filter, req.typed_keys, t.idx.mapping_version, t.idx.settings_version);
    // Terms lookups read other indices: key on every refresh anywhere.
    if k.contains("Some(TermsLookup") {
        k.push_str(&format!("|{}", crate::engine::refresh_epoch()));
    }
    k
}

/// Rescoring, rescorer after rescorer, over the top `window_size` hits:
/// - `QueryRescorer`: the window's scores combine the weighted original score with the
///   weighted rescore query score by `score_mode` (hits the rescore query doesn't match keep
///   their weighted original score); hits past the window keep their weighted original score.
/// - `ScriptRescorer`: the script's value (with `_score` the current score) replaces the
///   window's scores.
///
/// The window is re-sorted and the rest follows it. Returns the hits, the max score and, per
/// rescorer, the docs it rescored.
#[allow(clippy::type_complexity)]
/// Rescored hits (best first), their max score, and per rescorer the docs its window held.
pub(crate) type Rescored = (Vec<ShardHit>, Option<f32>, Vec<Vec<u32>>);

fn rescore(qctx: &QueryCtx, searcher: &Searcher, hits: Vec<ShardHit>, src: &SearchSource) -> EsResult<Rescored> {
    apply_rescore(searcher, hits, &prepare_rescore(qctx, src))
}

/// One `rescore` entry built against a shard (`RescoreContext`), so that `top_hits` can apply
/// the search's rescorers to its buckets after the query phase (`TopHitsAggregator`).
pub(crate) struct PreparedRescore {
    window: usize,
    query_weight: f32,
    rescore_query_weight: f32,
    score_mode: String,
    script: bool,
    /// Built up front; a script's error only surfaces when the window holds a hit.
    kind: EsResult<RescoreKind>,
}

enum RescoreKind {
    Query(QueryRef),
    Script(scripts::Painless),
}

pub(crate) fn prepare_rescore(qctx: &QueryCtx, src: &SearchSource) -> Vec<PreparedRescore> {
    // Elasticsearch extracts inner hits from the query and post filter only: a rescore
    // query's `inner_hits` are ignored (its named queries still count).
    let inner_hits = qctx.inner_hits.borrow().len();
    let prepared = src
        .rescore
        .iter()
        .map(|r| PreparedRescore {
            window: r.window_size.unwrap_or(10).max(0) as usize,
            query_weight: r.query_weight,
            rescore_query_weight: r.rescore_query_weight,
            score_mode: r.score_mode.clone(),
            script: r.script.is_some(),
            kind: match &r.script {
                Some(script) => extra::painless(qctx, script, search_painless::Context::Score).map(RescoreKind::Script).map_err(|e| query::shard_exception(qctx.idx, e)),
                None => qctx.to_query(&r.query).map(RescoreKind::Query).map_err(|e| query::shard_exception(qctx.idx, e)),
            },
        })
        .collect();
    qctx.inner_hits.borrow_mut().truncate(inner_hits);
    prepared
}

/// Rescores `hits` (best first) with each rescorer in turn: the hits after a rescorer's window
/// get its `query_weight`, the window is re-sorted (`QueryRescorer.combine`).
pub(crate) fn apply_rescore(searcher: &Searcher, mut hits: Vec<ShardHit>, prepared: &[PreparedRescore]) -> EsResult<Rescored> {
    let mut rescored = Vec::with_capacity(prepared.len());
    for r in prepared {
        let window = r.window.min(hits.len());
        let mut docs: Vec<u32> = hits[..window].iter().map(|h| h.doc).collect();
        docs.sort_unstable();
        rescored.push(docs);
        if r.script {
            if window == 0 {
                continue;
            }
            let p = match &r.kind {
                Ok(RescoreKind::Script(p)) => p,
                Err(e) => return Err(e.clone()),
                Ok(RescoreKind::Query(_)) => unreachable!("a script rescorer"),
            };
            for h in hits[..window].iter_mut() {
                h.score = script_rescore_value(p, searcher, h.doc, h.score)?;
            }
            hits[..window].sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc.cmp(&b.doc)));
            continue;
        }
        // `QueryRescorerBuilder.buildContext` builds the query with the request, hits or not.
        let rq = match &r.kind {
            Ok(RescoreKind::Query(q)) => q.clone(),
            Err(e) => return Err(e.clone()),
            Ok(RescoreKind::Script(_)) => unreachable!("a query rescorer"),
        };
        for h in hits[window..].iter_mut() {
            h.score *= r.query_weight;
        }
        if window == 0 {
            continue;
        }
        let docs: Vec<(u32, f32)> = hits[..window].iter().map(|h| (h.doc, 0.0)).collect();
        let restricted: QueryRef = arc(BooleanQuery::new().add(Occur::Must, rq).add(Occur::Filter, arc(search_index::query::ScoreDocQuery::new(docs))));
        let mut c = TopScoreCollector::new(window, u64::MAX);
        searcher.search(&restricted, &mut c).map_err(crate::engine::convert::index_error)?;
        let secondary: std::collections::HashMap<u32, f32> = c.top_docs().hits.iter().map(|h| (h.doc, h.score)).collect();
        for h in hits[..window].iter_mut() {
            let primary = h.score * r.query_weight;
            h.score = match secondary.get(&h.doc) {
                Some(&s) => combine_rescore(&r.score_mode, primary, s * r.rescore_query_weight),
                None => primary,
            };
        }
        // With hits past the window, `combine` sorts all of them (the rescored window can
        // fall below the rest, e.g. with `score_mode: min`).
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc.cmp(&b.doc)));
    }
    let max = hits.iter().map(|h| h.score).filter(|s| !s.is_nan()).reduce(f32::max);
    Ok((hits, max, rescored))
}

/// `QueryRescoreMode.combine`.
fn combine_rescore(mode: &str, primary: f32, secondary: f32) -> f32 {
    match mode {
        "multiply" => primary * secondary,
        "avg" => (primary + secondary) / 2.0,
        "max" => primary.max(secondary),
        "min" => primary.min(secondary),
        _ => primary + secondary,
    }
}

/// A script rescorer's score for one hit (`_score` is its current score).
fn script_rescore_value(p: &scripts::Painless, searcher: &Searcher, doc: u32, score: f32) -> EsResult<f32> {
    use search_index::query::ScoreScript as _;
    let Some((seg, local)) = searcher.segment_of(doc) else { return Ok(score) };
    let mut leaf = p.leaf(seg).map_err(crate::engine::convert::index_error)?;
    Ok(leaf.score(local, score).map_err(crate::engine::convert::index_error)? as f32)
}

/// A hit's explanation through the rescorers that rescored it (`QueryRescorer.explain`,
/// `ScriptRescorer.explain`).
fn explain_rescored(qctx: &QueryCtx, searcher: &Searcher, src: &SearchSource, rescored: &[Vec<u32>], doc: u32, mut e: search_index::Explanation) -> EsResult<search_index::Explanation> {
    use search_index::Explanation;
    for (r, docs) in src.rescore.iter().zip(rescored) {
        let in_window = docs.binary_search(&doc).is_ok();
        if let Some(script) = &r.script {
            if !in_window {
                continue;
            }
            let p = extra::painless(qctx, script, search_painless::Context::Score)?;
            let value = script_rescore_value(&p, searcher, doc, e.value.as_f64() as f32)?;
            let desc = format!("script score function, computed with script:\"{}\", computed as:", p.bound.describe);
            let score = Explanation::matched(e.value.as_f64() as f32, "_score: ", vec![e]);
            e = Explanation::matched(value, desc, vec![score]);
            continue;
        }
        let prim = if e.matched {
            let v = e.value.as_f64() as f32 * r.query_weight;
            Explanation::matched(v, "product of:", vec![e, Explanation::matched(r.query_weight, "primaryWeight", Vec::new())])
        } else {
            Explanation { matched: false, value: search_index::search::ExplValue::Float(0.0), description: "First pass did not match".into(), details: vec![e] }
        };
        e = prim;
        if in_window {
            let rq = qctx.to_query(&r.query)?;
            let re = searcher.explain(&rq, doc).map_err(crate::engine::convert::index_error)?;
            if re.matched {
                let sv = re.value.as_f64() as f32 * r.rescore_query_weight;
                let sec = Explanation::matched(sv, "product of:", vec![re, Explanation::matched(r.rescore_query_weight, "secondaryWeight", Vec::new())]);
                let desc = match r.score_mode.as_str() {
                    "multiply" => "product of:",
                    "avg" => "avg of:",
                    "max" => "max of:",
                    "min" => "min of:",
                    _ => "sum of:",
                };
                let combined = combine_rescore(&r.score_mode, e.value.as_f64() as f32, sv);
                e = Explanation::matched(combined, desc, vec![e, sec]);
            }
        }
    }
    Ok(e)
}

/// The fewest docs a slice of a concurrent shard search gets (Elasticsearch's
/// `MINIMUM_DOCS_PER_SLICE`).
const MIN_DOCS_PER_SLICE: u32 = 50_000;

/// Intra-shard concurrency (Elasticsearch's `search_worker` slices) for top hits by score of a
/// `function_score` or `script_score` query: those score every match (no score bound lets
/// collection skip any), so splitting the work divides the wall time while the CPU stays the
/// same. Queries with score bounds and field sorts prune so well serially that slices only
/// add work: on geonames (1.1M docs, 5 shards) slicing them doubled their CPU time and
/// lengthened every one.
///
/// When a request has fewer shards than the node has cores, each shard's view searches in up
/// to `2 * cores / shards` slices (capped at the core count) of at least
/// [`MIN_DOCS_PER_SLICE`] docs, so a request on one or a few shards still uses every core.
/// Twice the cores' worth of slices keeps every thread busy when shards end unevenly (slices
/// are claimed, not assigned). Hits and totals are those of a serial search (the merge breaks
/// ties on doc ids). `None`: search serially — one slice would do, a deadline is set (a
/// timed-out serial search keeps its partial hits), the dialect is OpenSearch (its scripts
/// read the dialect from a thread-local the slice threads don't have), or
/// `ROLL_PERF_SERIAL_SLICES` is set (for A/B measurements).
fn sliced(searcher: &Searcher, shards: usize, q: &QueryRef) -> Option<Searcher> {
    use search_index::query::{FunctionScoreQuery, ScriptScoreQuery};
    use search_index::search::{SearchLimits, pool};
    static SERIAL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let unbounded = q.as_any().is::<FunctionScoreQuery>() || q.as_any().is::<ScriptScoreQuery>();
    let cores = pool::cores();
    let k = (2 * cores).div_ceil(shards.max(1)).min(cores);
    if !unbounded || k <= 1 || searcher.max_doc() < 2 * MIN_DOCS_PER_SLICE || *SERIAL.get_or_init(|| std::env::var_os("ROLL_PERF_SERIAL_SLICES").is_some()) || crate::opensearch::active() {
        return None;
    }
    if search_index::search::ambient_control().is_some_and(|c| c.deadline.is_some()) {
        return None;
    }
    Some(searcher.with_limits(SearchLimits { parallelism: k, parallel_min_docs: MIN_DOCS_PER_SLICE, ..*searcher.limits() }))
}

/// Collects with the composition the request asks for (min score, post filter, aggregations,
/// terminate after).
#[allow(clippy::too_many_arguments)]
fn run_collect<C: Collector, R>(
    searcher: &Searcher,
    q: &QueryRef,
    top: C,
    post: Option<&QueryRef>,
    src: &SearchSource,
    aggs: Option<&mut search_aggs::AggCollector>,
    terminated: &mut bool,
    finish: impl FnOnce(C) -> R,
) -> EsResult<R> {
    let ie = crate::engine::convert::index_error;
    // Layers: post_filter (top only) → min_score (everything) → terminate_after (everything,
    // counting only the docs the top hits take, as `QueryPhaseCollector` does).
    let mut top = top;
    let r = {
        let mut pf;
        let top_ref: &mut dyn Collector = match post {
            Some(p) => {
                pf = PostFilter::new(searcher, p, ByRef(&mut top)).map_err(ie)?;
                &mut pf
            }
            None => &mut top,
        };
        // A lone collector runs as it is: a one-part multi collector costs a virtual call and
        // score bookkeeping per hit.
        let mut multi;
        let base: &mut dyn Collector = match aggs {
            Some(a) => {
                multi = MultiCollector::new(vec![top_ref, a]);
                &mut multi
            }
            None => top_ref,
        };
        let mut ms;
        let inner: &mut dyn Collector = match src.min_score {
            Some(m) => {
                ms = MinScore::new(ByRef(base), m);
                &mut ms
            }
            None => base,
        };
        match src.terminate_after {
            Some(t) if t > 0 => {
                let mut ta = TerminateAfter::new(ByRef(inner), t as u64).counting(searcher, src.min_score, post).map_err(ie)?;
                let r = searcher.search(q, &mut ta);
                *terminated = ta.terminated_early;
                r
            }
            _ => searcher.search(q, inner),
        }
    };
    match r {
        Ok(()) => {}
        Err(e) if e.is_terminated() => {}
        // `QueryPhase`: a timeout keeps what was collected.
        Err(search_index::Error::TimeExceeded) => SHARD_TIMED_OUT.with(|c| c.set(true)),
        Err(e) => return Err(ie(e)),
    }
    Ok(finish(top))
}

/// The collapse field (`CollapseBuilder.build`): keyword or numeric with doc values.
fn collapse_field(qctx: &QueryCtx, name: &str) -> EsResult<search_index::collector::collapse::CollapseField> {
    use crate::engine::fields::Kind;
    if name.is_empty() {
        return Err(EsError::illegal_argument("no collapse field specified"));
    }
    // Every check fails with an `IllegalArgumentException`; the field type's `collapseType()`
    // (Elasticsearch) or class (OpenSearch's `CollapseBuilder.build`: `KeywordFieldType` or
    // `NumberFieldType`) comes before doc values. Both take keywords and numbers only:
    // `unsigned_long` and `token_count` are numbers; `scaled_float`, dates, booleans, ips,
    // wildcards and constant keywords are not.
    let os = crate::opensearch::active();
    let unsupported = |name: &str, ty: &str| {
        EsError::illegal_argument(if os {
            format!("unknown type for collapse field `{name}`, only keywords and numbers are accepted")
        } else {
            format!("collapse is not supported for the field [{name}] of the type [{ty}]")
        })
    };
    let Some(f) = qctx.field(name) else {
        // Metadata fields have a type, just not a collapsible one.
        const BOTH: &[&str] = &["_id", "_index", "_seq_no", "_version", "_routing", "_ignored", "_source", "_doc_count", "_field_names", "_nested_path", "_data_stream_timestamp"];
        if BOTH.contains(&name) || (!os && matches!(name, "_tier" | "_index_mode")) {
            return Err(unsupported(name, name));
        }
        return Err(EsError::illegal_argument(format!("no mapping found for `{name}` in order to collapse on")));
    };
    let field = match f.kind {
        _ if f.flattened_key.is_some() && !os => {
            // A keyed `flattened` field is its root's `._keyed` field.
            let key = f.flattened_key.as_deref().unwrap_or_default();
            let root = name.strip_suffix(key).and_then(|r| r.strip_suffix('.')).unwrap_or(name);
            return Err(unsupported(&format!("{root}._keyed"), &f.ty));
        }
        Kind::Keyword => search_index::collector::collapse::CollapseField::Keyword(f.number),
        Kind::Long | Kind::Integer | Kind::Short | Kind::Byte | Kind::UnsignedLong | Kind::Double | Kind::Float | Kind::HalfFloat | Kind::TokenCount => {
            search_index::collector::collapse::CollapseField::Numeric(f.number)
        }
        _ => return Err(unsupported(name, &f.ty)),
    };
    if !f.doc_values {
        return Err(EsError::illegal_argument(format!("cannot collapse on field `{name}` without `doc_values`")));
    }
    Ok(field)
}

/// The collapse field's value of the hit's own document (`Null` without one).
fn own_collapse_value(searcher: &Searcher, table: &crate::engine::fields::FieldTable, field: &str, h: &ShardHit) -> EsResult<Option<SortValue>> {
    use crate::engine::fields::Kind;
    use search_index::collector::collapse::{CollapseCollector, CollapseField};
    let Some(f) = table.get(field) else { return Ok(h.group.clone()) };
    let cf = match f.kind {
        Kind::Keyword => CollapseField::Keyword(f.number),
        _ => CollapseField::Numeric(f.number),
    };
    let q: QueryRef = arc(search_index::query::ScoreDocQuery::new(vec![(h.doc, h.score)]));
    let mut top = CollapseCollector::new(cf, vec![SortKey::Score { reverse: false }], 1, 1, true).with_searcher(searcher);
    searcher.search(&q, &mut top).map_err(crate::engine::convert::index_error)?;
    Ok(Some(top.top_docs().hits.first().map_or(SortValue::Null, |x| x.group.clone())))
}

/// A borrowed collector as a collector (the engine's wrappers own theirs).
pub struct ByRef<'c>(pub &'c mut dyn Collector);

impl Collector for ByRef<'_> {
    fn score_mode(&self) -> search_index::ScoreMode {
        self.0.score_mode()
    }
    fn leaf<'a>(&'a mut self, seg: &search_index::SegmentContext) -> search_index::Result<Box<dyn search_index::LeafCollector + 'a>> {
        self.0.leaf(seg)
    }
}

/// Each target's shard request index (`SearchShardIterator` order: shard number, then index
/// name), which `_shard_doc` values encode.
fn shard_request_indices(targets: &[Target]) -> Vec<usize> {
    // `ShardId.compareTo` (shard number, index name, index uuid), then the cluster alias with
    // the local cluster first (`SearchShardIterator`); a remote shard's index is
    // `alias:index` here (local index names can't hold `:`).
    let key = |t: &Target| {
        let (alias, name) = t.idx.name.split_once(':').unwrap_or(("", t.idx.name.as_str()));
        (t.shard.id, name.to_string(), t.idx.uuid.clone(), alias.to_string())
    };
    let mut order: Vec<usize> = (0..targets.len()).collect();
    order.sort_by_cached_key(|&i| key(&targets[i]));
    let mut out = vec![0; targets.len()];
    for (rank, t) in order.into_iter().enumerate() {
        out[t] = rank;
    }
    out
}

/// The request's `search_after` as the shard with request index `me` sees it: `_shard_doc`
/// values become that shard's doc bound (`ShardDocSortField.setTopValue`).
fn shard_doc_after(after: &[Value], positions: &[usize], me: usize) -> Vec<Value> {
    let mut out = after.to_vec();
    for &p in positions {
        let parsed = match out.get(p) {
            Some(Value::Num(n)) => n.as_i64(),
            Some(Value::Str(s)) => s.parse::<i64>().ok(),
            _ => None,
        };
        if let Some(v) = parsed {
            out[p] = Value::from(sort::shard_doc_after(v, me));
        }
    }
    out
}

/// Field sorts where an `unsigned_long` field of one index meets another type in another
/// don't merge (`SearchPhaseController.validateMergeSortValueFormats`, in both).
fn check_unsigned_sort_mix(shard_formats: &[&[sort::ValueFormat]]) -> EsResult<()> {
    let unsigned = |f: &sort::ValueFormat| matches!(f, sort::ValueFormat::UnsignedLong | sort::ValueFormat::Unmapped(Some(crate::engine::fields::Kind::UnsignedLong)));
    let Some((first, rest)) = shard_formats.split_first() else { return Ok(()) };
    if rest.iter().any(|fs| fs.iter().zip(first.iter()).any(|(a, b)| unsigned(a) != unsigned(b))) {
        return Err(aggs::reduce_failure(EsError::illegal_argument(
            "Can't do sort across indices, as a field has [unsigned_long] type in one index, and different type in another index!",
        )));
    }
    Ok(())
}

fn cmp_hits(keys: &[SortKey], shard_doc: &[usize], a: (&ShardHit, usize), b: (&ShardHit, usize)) -> std::cmp::Ordering {
    if keys.is_empty() {
        return b.0.score.total_cmp(&a.0.score).then(a.1.cmp(&b.1)).then(a.0.doc.cmp(&b.0.doc));
    }
    if !shard_doc.is_empty() || keys.iter().any(|k| matches!(k, SortKey::Doc { .. })) {
        // `_shard_doc` values are global (shard index, doc) and `_doc` values the shard's doc
        // id: compare them as longs (`TopDocs.merge` compares the doc a shard reported, ties
        // going to the lower shard), every other key as usual.
        let mut o = std::cmp::Ordering::Equal;
        for (i, k) in keys.iter().enumerate() {
            o = if shard_doc.contains(&i) || matches!(k, SortKey::Doc { .. }) {
                let v = |h: &ShardHit| match h.sort.get(i) {
                    Some(SortValue::Long(l)) => *l,
                    _ => 0,
                };
                let c = v(a.0).cmp(&v(b.0));
                if matches!(k, SortKey::Doc { reverse: true }) { c.reverse() } else { c }
            } else {
                let one = |h: &ShardHit| FieldDoc { doc: 0, score: h.score, values: h.sort.get(i).cloned().into_iter().collect() };
                search_index::collector::top_field::compare_field_docs(std::slice::from_ref(k), &one(a.0), &one(b.0))
            };
            if o.is_ne() {
                break;
            }
        }
        return o.then(a.1.cmp(&b.1)).then(a.0.doc.cmp(&b.0.doc));
    }
    let fa = FieldDoc { doc: a.0.doc, score: a.0.score, values: a.0.sort.clone() };
    let fb = FieldDoc { doc: b.0.doc, score: b.0.score, values: b.0.sort.clone() };
    // Equal sort values tie-break on shard order, then doc.
    let mut keys2 = keys.to_vec();
    keys2.retain(|k| !matches!(k, SortKey::Doc { .. }) || true);
    let o = search_index::collector::top_field::compare_field_docs(&keys2, &FieldDoc { doc: 0, ..fa.clone() }, &FieldDoc { doc: 0, ..fb.clone() });
    o.then(a.1.cmp(&b.1)).then(a.0.doc.cmp(&b.0.doc))
}

/// Runs a search and renders the hits.
pub fn execute(cluster: &Cluster, req: &SearchRequest) -> EsResult<SearchOutcome> {
    let st = cluster.state();
    let errs = validate(req);
    if !errs.is_empty() {
        return Err(EsError::action_request_validation(&errs));
    }
    crate::meta::resolve::reject_remote(&req.indices)?;
    let targets = shard_targets(cluster, &st, req)?;
    note_fielddata(&req.source, &targets, id_fielddata::enabled(&st));
    execute_on(cluster, req, &targets)
}

/// `matched_fields` with `require_field_match: false` under the unified highlighter (the
/// default; `plain` ignores matched fields and `fvh` takes them).
fn highlight_shard_error(h: &Map) -> Option<EsError> {
    let global = h.get("require_field_match").and_then(Value::as_bool);
    let global_type = h.get("type").and_then(Value::as_str);
    let fields: Vec<&Value> = match h.get("fields") {
        Some(Value::Obj(m)) => m.values().collect(),
        Some(Value::Arr(a)) => a.iter().filter_map(|x| x.as_object().and_then(|m| m.values().next())).collect(),
        _ => Vec::new(),
    };
    for f in fields {
        let rfm = f.get("require_field_match").and_then(Value::as_bool).or(global).unwrap_or(true);
        let unified = f.get("type").and_then(Value::as_str).or(global_type).is_none_or(|t| t == "unified");
        if !rfm && unified && f.get("matched_fields").is_some() {
            return Some(EsError::illegal_argument("Matched fields are not supported when [require_field_match] is set to [false]"));
        }
    }
    None
}

/// Marks the text fields whose field data the request's sorts and aggregations load (for the
/// `fielddata` stats).
/// Records the field data a search loads (`id_fielddata`: `_id`'s may load).
fn note_fielddata(src: &SearchSource, targets: &[Target], id_fielddata: bool) {
    fn agg_fields(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Obj(m) => {
                // `execution_hint: map` collects terms per segment: no global ordinals.
                let map_hint = m.get("execution_hint").and_then(Value::as_str) == Some("map");
                for (k, c) in m {
                    if k == "field"
                        && let Value::Str(f) = c
                    {
                        if !map_hint {
                            out.push(f.clone());
                        }
                    } else {
                        agg_fields(c, out);
                    }
                }
            }
            Value::Arr(a) => a.iter().for_each(|x| agg_fields(x, out)),
            _ => {}
        }
    }
    let mut sorts: Vec<String> = src.sort.iter().filter_map(|s| match s {
        Sort::Field { field, .. } => Some(field.clone()),
        _ => None,
    }).collect();
    if src.docvalue_fields.iter().any(|f| f.field == "_id") {
        sorts.push("_id".into());
    }
    let mut aggs = Vec::new();
    if let Some(a) = &src.aggs {
        agg_fields(&Value::Obj(a.clone()), &mut aggs);
    }
    if !id_fielddata {
        sorts.retain(|f| f != "_id");
        aggs.retain(|f| f != "_id");
    }
    if sorts.is_empty() && aggs.is_empty() {
        return;
    }
    let mut seen: Vec<&str> = Vec::new();
    for t in targets {
        if seen.contains(&t.engine.uuid.as_str()) {
            continue;
        }
        seen.push(&t.engine.uuid);
        for f in &sorts {
            t.engine.load_fielddata(f, false);
        }
        for f in &aggs {
            t.engine.load_fielddata(f, true);
        }
    }
}

/// The query phases of a search's `n` shards, in shard order: in parallel on the search pool
/// when there are several (Elasticsearch runs a request's shards concurrently on its search
/// thread pool, so a single request is not bound to one core), else on this thread. The
/// request's thread-bound state (dialect, terms lookup cache, scroll continuation) follows
/// the work onto the workers; inside a pool worker (a search nested in a search), and with
/// `ROLL_PERF_SERIAL_SHARDS` set (for A/B measurements), the shards run in order here.
fn run_query_phases(n: usize, run: &(dyn Fn(usize) -> EsResult<ShardResult> + Sync)) -> Vec<EsResult<ShardResult>> {
    use search_index::search::pool;
    static SERIAL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let cores = pool::cores();
    if n <= 1 || cores <= 1 || pool::in_worker() || *SERIAL.get_or_init(|| std::env::var_os("ROLL_PERF_SERIAL_SHARDS").is_some()) {
        return (0..n).map(run).collect();
    }
    let slots: Vec<parking_lot::Mutex<Option<EsResult<ShardResult>>>> = (0..n).map(|_| parking_lot::Mutex::new(None)).collect();
    let opensearch = crate::opensearch::active();
    let lookups = extra::LookupScope::current();
    let page = contexts::page_state();
    // The request's cancellation (`_tasks/_cancel`, a closed connection) reaches every shard.
    let cancelled = search_index::search::ambient_control().and_then(|c| c.cancelled);
    pool::for_each_index(n, (n - 1).min(cores - 1), &|i| {
        let _lookups = extra::LookupScope::adopt(lookups.clone());
        let _ambient = cancelled.clone().map(|c| search_index::search::set_ambient_control(Some(search_index::search::SearchControl { deadline: None, cancelled: Some(c), ext: None })));
        let r = crate::opensearch::with(opensearch, || contexts::with_page(page.clone(), || run(i)));
        *slots[i].lock() = Some(r);
    });
    slots.into_iter().map(|s| s.into_inner().expect("every shard's query phase ran")).collect()
}

/// The checks of `DefaultSearchContext.preProcess` on one shard: the result window (which a
/// suggest-only request skips, `SearchContext.hasOnlySuggest`), sort with rescore, the
/// index limits and slices. They run where the shard's search context is created: the query
/// phase, or the DFS phase of `dfs_query_then_fetch`.
fn context_checks(t: &Target, req: &SearchRequest, from: usize, size: usize) -> EsResult<()> {
    let src = &req.source;
    // `preProcess` returns at once for a suggest-only request (an empty `suggest` too).
    let only_suggest = src.suggest.is_some() && src.query.is_none() && src.knn.is_empty() && src.aggs.is_none();
    if only_suggest {
        return Ok(());
    }
    check_window(&t.idx, from as i64, size as i64, req.scroll.is_some())?;
    check_rescore_sort(src)?;
    check_limits(&t.idx, src)?;
    check_slices(&t.idx, src, req.scroll.is_some())
}

/// [`context_checks`] with the request's own `from` and `size`.
pub(super) fn context_checks_of(t: &Target, req: &SearchRequest) -> EsResult<()> {
    let from = req.source.from.unwrap_or(0).max(0) as usize;
    let size = req.source.size.unwrap_or(10).max(0) as usize;
    context_checks(t, req, from, size)
}

/// Runs a search on given shard targets (scrolls and PITs pass their snapshots).
pub fn execute_on(cluster: &Cluster, req: &SearchRequest, targets: &[Target]) -> EsResult<SearchOutcome> {
    // The search's shard tasks (`_tasks`), under the request's task.
    let _tasks = crate::tasks::search_started(cluster, req, targets);
    // `dfs_query_then_fetch` over more than one shard (a single shard runs query_then_fetch):
    // a DFS phase, then the query phase Elasticsearch calls `dfs_query`.
    if req.dfs && targets.len() > 1 && targets.iter().all(|t| t.searcher.as_ref().is_none_or(|s| s.global_stats().is_none())) {
        let (dfs_targets, failures) = dfs::with_global_stats(cluster, req, targets)?;
        // Every shard failed to build the query: the DFS phase failed (shards that fail alone
        // fail again in the query phase, which reports them).
        if !failures.is_empty() && failures.len() == targets.len() {
            // OpenSearch names the phase `dfs_pre_query` (`SearchPhaseName.DFS_PRE_QUERY`).
            let phase = if crate::opensearch::active() { "dfs_pre_query" } else { "dfs" };
            return Err(all_shards_failed(&cluster.node.id, targets, &failures, "all shards failed").set_meta("phase", phase));
        }
        return execute_targets(cluster, req, &dfs_targets).map_err(|e| {
            let query_phase = e.kind == "search_phase_execution_exception" && e.meta().iter().any(|(k, v)| k == "phase" && v.as_str() == Some("query"));
            if query_phase { e.set_meta("phase", "dfs_query") } else { e }
        });
    }
    execute_targets(cluster, req, targets)
}

thread_local! {
    /// Shards the can-match phase skipped for the search running on this thread.
    static CAN_MATCH_SKIPPED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// [`execute_on`] after the can-match phase skipped `skipped` of the search's shards.
fn execute_after_can_match(cluster: &Cluster, req: &SearchRequest, targets: &[Target], skipped: usize) -> EsResult<SearchOutcome> {
    let prev = CAN_MATCH_SKIPPED.with(|c| c.replace(skipped));
    let out = execute_on(cluster, req, targets);
    CAN_MATCH_SKIPPED.with(|c| c.set(prev));
    out
}

fn execute_targets(cluster: &Cluster, req: &SearchRequest, targets: &[Target]) -> EsResult<SearchOutcome> {
    if let Some(f) = &req.source.fusion {
        return fusion::execute(cluster, req, targets, f);
    }
    // OpenSearch: a top-level `hybrid` query searches each sub-query, then fuses.
    if let Some(h) = req.source.query.as_ref().and_then(crate::opensearch::hybrid::top_level) {
        return crate::opensearch::hybrid::execute(cluster, req, targets, h);
    }
    if crate::opensearch::active() && req.source.query.as_ref().is_some_and(crate::opensearch::hybrid::nested_in_compound) {
        let e = crate::opensearch::hybrid::nested_error();
        let failures: Vec<(usize, EsError)> = (0..targets.len()).map(|i| (i, e.clone())).collect();
        if !failures.is_empty() {
            return Err(all_shards_failed(&cluster.node.id, targets, &failures, "all shards failed"));
        }
    }
    let started = std::time::Instant::now();
    let _lookups = extra::LookupScope::enter();
    let st = cluster.state();
    let src = &req.source;
    let from = src.from.unwrap_or(0).max(0) as usize;
    let size = src.size.unwrap_or(10).max(0) as usize;
    // A `diversify` retriever searches its window, then picks from it.
    let n = match &src.diversify {
        Some(d) => d.rank_window_size.max(0) as usize,
        None => from + size,
    };
    // OpenSearch's shards refuse `stored_fields: _none_` with `fields` as they parse the source.
    // (`SearchService.parseSource`: a requested `_source` is checked first.)
    let stored_none = src.stored_fields.as_ref().is_some_and(|sf| sf.fields.is_none());
    let source_requested = src.source.as_ref().is_some_and(|s| s.fetch);
    if crate::opensearch::active() && (stored_fields_disabled_with_fields(src) || (stored_none && source_requested)) {
        let e = EsError::new(
            500,
            "search_exception",
            if source_requested { "[stored_fields] cannot be disabled if [_source] is requested" } else { "[stored_fields] cannot be disabled when using the [fields] option" },
        );
        let failures: Vec<(usize, EsError)> = (0..targets.len()).map(|i| (i, e.clone())).collect();
        if !failures.is_empty() {
            return Err(all_shards_failed(&cluster.node.id, targets, &failures, "all shards failed"));
        }
    }
    // Highlighting options every shard rejects as it builds its highlighters.
    if let Some(h) = &src.highlight
        && let Some(e) = highlight_shard_error(h)
    {
        let failures: Vec<(usize, EsError)> = (0..targets.len()).map(|i| (i, e.clone())).collect();
        if !failures.is_empty() {
            return Err(all_shards_failed(&cluster.node.id, targets, &failures, "all shards failed"));
        }
    }
    // The result window is checked on each shard (failures come back as shard failures), in
    // the query phase a suggest-only request skips (`SearchContext.hasOnlySuggest`).
    let window: Vec<Option<EsError>> = targets.iter().map(|t| context_checks(t, req, from, size).err()).collect();
    let agg_req = match &src.aggs {
        Some(a) => {
            // Aggregations over runtime fields run their scripts.
            let any_runtime = src.runtime_mappings.is_some() || targets.iter().any(|t| !t.idx.mapping.runtime.is_empty());
            if any_runtime {
                let orig = a;
                let mut a = a.clone();
                // Indices where the name is a concrete field read it as such (the aggregation
                // host checks per shard); the request's runtime fields shadow concrete ones.
                let def = |name: &str| -> Option<Value> {
                    let requested = src.runtime_mappings.as_ref().is_some_and(|r| r.contains_key(name) || name.rsplit_once('.').is_some_and(|(p, _)| r.contains_key(p)));
                    // A mapping's runtime section shadows its concrete fields.
                    let runtime_in = |t: &Target| t.idx.mapping.runtime.contains_key(name) || name.rsplit_once('.').is_some_and(|(p, _)| t.idx.mapping.runtime.contains_key(p));
                    if !requested && targets.iter().all(|t| t.engine.table().get(name).is_some() && !runtime_in(t)) {
                        return None;
                    }
                    let mut d = targets.iter().find_map(|t| runtime::definition(&t.idx.mapping, src.runtime_mappings.as_ref(), name))?;
                    if requested && let Value::Obj(m) = &mut d {
                        m.insert(runtime::REQUEST_MARKER.into(), Value::Bool(true));
                    }
                    Some(d)
                };
                // `lookup` runtime fields have no field data.
                // Every shard fails building the aggregation.
                if let Some(rm) = &src.runtime_mappings
                    && let Err(e) = aggs::check_lookup_fields(&a, rm)
                {
                    let failures: Vec<(usize, EsError)> = (0..targets.len()).map(|i| (i, e.clone())).collect();
                    return Err(all_shards_failed(&cluster.node.id, targets, &failures, "all shards failed"));
                }
                if let Err(e) = aggs::check_top_metrics_runtime(&a, &def) {
                    let failures: Vec<(usize, EsError)> = (0..targets.len()).map(|i| (i, e.clone())).collect();
                    return Err(all_shards_failed(&cluster.node.id, targets, &failures, "all shards failed"));
                }
                aggs::rewrite_runtime(&mut a, &def);
                Some(aggs::parse(&a, src.aggs_raw.as_deref().map(|r| (r, orig)))?)
            } else {
                Some(aggs::parse(a, src.aggs_raw.as_deref().map(|r| (r, a)))?)
            }
        }
        None => None,
    };
    let after = src.search_after.as_deref();
    let knn_sections = knn::sections(src)?;
    let (knn_queries, knn_profiles) = match &req.fused {
        // The compound retriever's query is built per shard (`query_phase`).
        Some(_) => (vec![Vec::new(); targets.len()], vec![None; targets.len()]),
        None if knn_sections.is_empty() => (vec![Vec::new(); targets.len()], vec![None; targets.len()]),
        None => knn::dfs(cluster, &st, req.now_ms, targets, &knn_sections, src.profile)?,
    };
    let mut results: Vec<ShardResult> = Vec::with_capacity(targets.len());
    let mut failures: Vec<(usize, EsError)> = Vec::new();
    // `_shard_doc` sort keys: their values carry the shard request index.
    let shard_doc: Vec<usize> = src.sort.iter().enumerate().filter(|(_, s)| matches!(s, Sort::Field { field, .. } if field == "_shard_doc")).map(|(i, _)| i).collect();
    // Ties between shards break on this order too (`TopDocs.merge` by shard index).
    let request_index = shard_request_indices(targets);
    let run = |i: usize| -> EsResult<ShardResult> {
        let shard_after;
        let after = match (after, shard_doc.is_empty()) {
            (Some(a), false) => {
                shard_after = shard_doc_after(a, &shard_doc, request_index[i]);
                Some(shard_after.as_slice())
            }
            (a, _) => a,
        };
        query_phase(cluster, &st, req, &targets[i], i, targets.len(), n, after, agg_req.as_ref(), &knn_queries[i], window[i].clone())
    };
    let phases = run_query_phases(targets.len(), &run);
    for (i, phase) in phases.into_iter().enumerate() {
        match phase {
            Ok(mut r) => {
                for h in r.hits.iter_mut() {
                    for &p in &shard_doc {
                        if let Some(SortValue::Long(d)) = h.sort.get_mut(p) {
                            *d = sort::encode_shard_doc(request_index[i], *d);
                        }
                    }
                }
                if let (Some(p), Some(d)) = (r.profile.as_mut(), knn_profiles[i].clone()) {
                    // A dfs search with knn reports both the statistics and the knn phase.
                    p.dfs = Some(match (p.dfs.take(), d) {
                        (Some(Value::Obj(mut stats)), Value::Obj(knn)) => {
                            for (k, v) in knn {
                                stats.insert(k, v);
                            }
                            Value::Obj(stats)
                        }
                        (_, d) => d,
                    });
                }
                results.push(r)
            }
            Err(e) => failures.push((i, e)),
        }
    }
    // Failures are reported in shard request order (`ShardId` order: shard number, then index).
    failures.sort_by_key(|(i, _)| request_index[*i]);
    // Shards the can-match phase skipped count as successful: with one, the search stands.
    let skipped_ok = CAN_MATCH_SKIPPED.with(|c| c.get()) > 0;
    if !failures.is_empty() && ((results.is_empty() && !skipped_ok) || !req.allow_partial) {
        return Err(all_shards_failed(&cluster.node.id, targets, &failures, if results.is_empty() { "all shards failed" } else { "Partial shards failure" }));
    }
    // Reduce: merge hits in sort order (shard order breaks ties).
    let built_keys: Vec<SortKey> = if results.is_empty() {
        Vec::new()
    } else {
        let t = &targets[results[0].target];
        let table = t.engine.table();
        let mut qctx = QueryCtx::new(cluster, &st, &t.idx, &table, &results[0].searcher, req.now_ms);
        // Sorts on the request's runtime fields resolve them here too.
        qctx.runtime_mappings = src.runtime_mappings.clone();
        let b = sort::build(&qctx, &src.sort)?;
        if b.by_score { Vec::new() } else { sort::merge_keys(b.keys, &b.formats) }
    };
    let formats = if results.is_empty() {
        Vec::new()
    } else {
        let t = &targets[results[0].target];
        let table = t.engine.table();
        let mut qctx = QueryCtx::new(cluster, &st, &t.idx, &table, &results[0].searcher, req.now_ms);
        // Sorts on the request's runtime fields resolve them here too.
        qctx.runtime_mappings = src.runtime_mappings.clone();
        sort::build(&qctx, &src.sort)?.formats
    };
    // Each shard formats its own sort values (`SearchSortValues` take the shard's
    // `DocValueFormat`s): indices mapping a field differently (a long here, a double there)
    // print their own types.
    // One build per index (its shards share the mapping).
    let mut by_index: Vec<(&str, Option<Arc<Vec<sort::ValueFormat>>>)> = Vec::new();
    let mut own_formats: Vec<Option<Arc<Vec<sort::ValueFormat>>>> = results
        .iter()
        .map(|r| {
            let t = &targets[r.target];
            if formats.is_empty() || t.idx.name == targets[results[0].target].idx.name {
                return None;
            }
            if let Some((_, f)) = by_index.iter().find(|(n, _)| *n == t.idx.name) {
                return f.clone();
            }
            let table = t.engine.table();
            let mut qctx = QueryCtx::new(cluster, &st, &t.idx, &table, &r.searcher, req.now_ms);
            // Sorts on the request's runtime fields resolve them here too.
            qctx.runtime_mappings = src.runtime_mappings.clone();
            let f = sort::build(&qctx, &src.sort).ok().map(|b| Arc::new(b.formats));
            by_index.push((t.idx.name.as_str(), f.clone()));
            f
        })
        .collect();
    let mut all: Vec<(usize, ShardHit)> = Vec::new();
    for (ri, r) in results.iter().enumerate() {
        for h in &r.hits {
            all.push((ri, h.clone()));
        }
    }
    if results.iter().any(|r| !r.hits.is_empty()) && !formats.is_empty() {
        let shard_formats: Vec<&[sort::ValueFormat]> = (0..results.len()).map(|ri| own_formats[ri].as_deref().map_or(formats.as_slice(), Vec::as_slice)).collect();
        for (ri, h) in all.iter_mut() {
            sort::merge_values(&mut h.sort, shard_formats[*ri]);
        }
        // Both refuse to merge an `unsigned_long` with another type
        // (`SearchPhaseController.validateMergeSortValueFormats`).
        check_unsigned_sort_mix(&shard_formats)?;
        if cluster.dialect == crate::cluster::Dialect::OpenSearch {
            let with_hits: Vec<&[sort::ValueFormat]> = results.iter().zip(&shard_formats).filter(|(r, _)| !r.hits.is_empty()).map(|(_, f)| *f).collect();
            if let Some(e) = sort::merge_cast_failure(&with_hits) {
                return Err(aggs::reduce_failure(e));
            }
        } else {
            // `SortFieldValidation.validateAndMaybeRewrite`: the shards that collected hits must
            // agree on each key's sort type, except that numeric types mixing in a float or a
            // double merge (and print) as doubles. OpenSearch merges them as they are.
            let with_hits: Vec<&[sort::ValueFormat]> = results.iter().zip(&shard_formats).filter(|(r, _)| !r.hits.is_empty()).map(|(_, f)| *f).collect();
            let widen = sort::merge_widening(&with_hits, &src.sort).map_err(aggs::reduce_failure)?;
            if !widen.is_empty() {
                for (_, h) in all.iter_mut() {
                    sort::widen_values(&mut h.sort, &widen);
                }
                let owned: Vec<Option<Arc<Vec<sort::ValueFormat>>>> = shard_formats.iter().map(|f| Some(Arc::new(sort::widen_formats(f, &widen)))).collect();
                own_formats = owned;
            }
        }
    }
    all.sort_by(|a, b| cmp_hits(&built_keys, &shard_doc, (&a.1, request_index[results[a.0].target]), (&b.1, request_index[results[b.0].target])));
    if src.collapse.is_some() {
        // Each group once: its best head across shards.
        let mut seen: Vec<SortValue> = Vec::new();
        all.retain(|(_, h)| {
            let g = h.group.clone().unwrap_or(SortValue::Null);
            if seen.contains(&g) {
                false
            } else {
                seen.push(g);
                true
            }
        });
    }
    let page: Vec<(usize, ShardHit)> = match &src.diversify {
        Some(d) => {
            let window: Vec<(usize, ShardHit)> = all.into_iter().take(d.rank_window_size.max(0) as usize).collect();
            let vectors: Vec<Option<Vec<f32>>> = window
                .iter()
                .map(|(ri, h)| {
                    let r = &results[*ri];
                    hits::vector_of(&targets[r.target].engine.table(), &r.searcher, h.doc, &d.field)
                })
                .collect();
            d.mmr(&vectors).into_iter().map(|i| window[i].clone()).skip(from).take(size).collect()
        }
        // A compound retriever's hits are its fused documents only.
        None => match &req.fused {
            Some(f) => all.into_iter().take(f.cap).skip(from).take(size).collect(),
            None => all.into_iter().skip(from).take(size).collect(),
        },
    };
    let ranked: Vec<(usize, u32, f32)> = page.iter().map(|(ri, h)| (results[*ri].target, h.doc, h.score)).collect();
    let tracking = total_threshold(src);
    let mut total: u64 = results.iter().map(|r| r.total).sum();
    let mut relation = if results.iter().any(|r| r.relation == TotalRelation::GreaterThanOrEqualTo) { TotalRelation::GreaterThanOrEqualTo } else { TotalRelation::EqualTo };
    if let Some(th) = tracking
        && th != u64::MAX
        && total > th
    {
        total = th;
        relation = TotalRelation::GreaterThanOrEqualTo;
    }
    let max_score = results.iter().filter_map(|r| r.max_score).filter(|s| !s.is_nan()).reduce(f32::max);
    if req.rank_only {
        return Ok(SearchOutcome {
            took_ms: started.elapsed().as_millis() as u64,
            shards_total: targets.len(),
            shards_skipped: 0,
            failures: failures.iter().map(|(i, e)| shard_failure(&cluster.node.id, &targets[*i], e)).collect(),
            total: Some((total, relation)),
            max_score,
            hits: Vec::new(),
            aggs: None,
            suggest: None,
            terminated_early: None,
            timed_out: results.iter().any(|r| r.timed_out),
            profile: None,
            scroll_id: None,
            pit_id: None,
            ranked,
            clusters: None,
        });
    }
    let opts = FetchOpts {
        source: src.source.as_ref().map(crate::fetch::FetchSource::from_search),
        stored_fields: src.stored_fields.as_ref().map(|s| s.fields.clone()),
        docvalue_fields: src.docvalue_fields.clone(),
        fields: src.fields.clone(),
        version: src.version == Some(true),
        seq_no_primary_term: src.seq_no_primary_term == Some(true),
        force_synthetic: req.force_synthetic_source,
        id_fielddata: id_fielddata::enabled(&st),
        script_fields_only: !src.script_fields.is_empty(),
        raw_source: crate::json::raw::enabled(),
        keep_id: (!src.script_fields.is_empty() || src.highlight.is_some()) && cluster.dialect == crate::cluster::Dialect::Elasticsearch,
    };
    let mut rendered: Vec<(usize, Value)> = Vec::with_capacity(page.len());
    let mut fetch_times: Vec<profile::FetchTimes> = vec![Default::default(); results.len()];
    // A shard whose fetch fails is a shard failure: its hits drop out of the page.
    let mut fetch_failed: Vec<usize> = Vec::new();
    // Large pages render on several threads; the outcomes are then taken in page order, so
    // failures, their order and the hits kept are those of the serial loop below.
    let formats_of = |ri: usize| own_formats[ri].as_deref().map_or(formats.as_slice(), Vec::as_slice);
    let mut prefetched = fetch_parallel(&page, |ri, h| render_hit(cluster, &st, req, &targets[results[ri].target], &results[ri], h, &opts, formats_of(ri)));
    for (pos, (ri, h)) in page.iter().enumerate() {
        if fetch_failed.contains(ri) {
            continue;
        }
        let r = &results[*ri];
        let t = &targets[r.target];
        let at = std::time::Instant::now();
        let outcome = match prefetched.as_mut() {
            Some(p) => match p[pos].take() {
                Some((res, nanos)) => {
                    fetch_times[*ri].nanos += nanos;
                    res
                }
                // its unit stopped at an earlier failure of the same shard, skipped above
                None => continue,
            },
            None => render_hit(cluster, &st, req, t, r, h, &opts, formats_of(*ri)),
        };
        match outcome {
            Ok(v) => rendered.push((*ri, v)),
            // The expand phase runs on the coordinator after the fetch: no shard failed.
            Err(e) if is_expand_failure(&e) => return Err(e),
            Err(e) => {
                fetch_failed.push(*ri);
                failures.push((r.target, e));
                continue;
            }
        }
        if prefetched.is_none() {
            fetch_times[*ri].nanos += at.elapsed().as_nanos() as u64;
        }
        fetch_times[*ri].hits += 1;
    }
    if !fetch_failed.is_empty() {
        if fetch_failed.len() == results.len() || !req.allow_partial {
            let reason = if fetch_failed.len() == results.len() { "all shards failed" } else { "Partial shards failure" };
            let mut e = all_shards_failed(&cluster.node.id, targets, &failures, reason);
            // One shard runs query and fetch together; otherwise the fetch phase failed.
            if targets.len() > 1 {
                e = e.set_meta("phase", "fetch");
            }
            return Err(e);
        }
        rendered.retain(|(ri, _)| !fetch_failed.contains(ri));
    }
    let hits_out: Vec<Value> = rendered.into_iter().map(|(_, v)| v).collect();
    let profile_out = src.profile.then(|| {
        let stored_none = matches!(&opts.stored_fields, Some(None));
        let mut phases = vec!["FetchFieldsPhase", "StoredFieldsPhase"];
        if crate::search::hits::wants_source(&opts).fetch {
            phases.push("FetchSourcePhase");
        }
        if !opts.docvalue_fields.is_empty() {
            phases.push("FetchDocValuesPhase");
        }
        if src.explain == Some(true) {
            phases.push("ExplainPhase");
        }
        if src.highlight.is_some() {
            phases.push("HighlightPhase");
        }
        if opts.version {
            phases.push("FetchVersionPhase");
        }
        if opts.seq_no_primary_term {
            phases.push("SeqNoPrimaryTermPhase");
        }
        if !src.script_fields.is_empty() {
            phases.push("ScriptFieldsPhase");
        }
        if cluster.dialect == crate::cluster::Dialect::OpenSearch {
            let wants_source = crate::search::hits::wants_source(&opts).fetch;
            let mut top_hits = Vec::new();
            if let Some(a) = &src.aggs {
                profile::top_hits_names(a, &mut top_hits);
            }
            let by_score = src.sort.is_empty() || sort::build_is_score_only(&src.sort);
            let shards: Vec<Value> = results
                .iter()
                .enumerate()
                .filter_map(|(ri, r)| {
                    let t = &targets[r.target];
                    let p = r.profile.as_ref()?;
                    let ran = |ph: &str| match ph {
                        "ExplainPhase" => src.explain == Some(true),
                        "FetchDocValuesPhase" => !opts.docvalue_fields.is_empty(),
                        "ScriptFieldsPhase" => !src.script_fields.is_empty(),
                        "FetchSourcePhase" => wants_source && !stored_none,
                        "FetchFieldsPhase" => !src.fields.is_empty(),
                        "FetchVersionPhase" => opts.version,
                        "SeqNoPrimaryTermPhase" => opts.seq_no_primary_term,
                        "MatchedQueriesPhase" => !r.named.is_empty(),
                        "HighlightPhase" => src.highlight.is_some(),
                        "FetchScorePhase" => src.track_scores && !by_score,
                        _ => false,
                    };
                    let f = profile::OsFetch {
                        phases: profile::OS_FETCH_PHASES.iter().copied().filter(|p| ran(p)).collect(),
                        stored: !stored_none,
                        source: wants_source && !stored_none,
                        inner_hits: r.inner_hits.iter().map(|d| d.name.as_str()).collect(),
                        top_hits: top_hits.clone(),
                        matched: r.total > 0,
                    };
                    Some(profile::shard_os(&cluster.node.id, &t.idx.name, t.shard.id as usize, p, profile::fetch_os(&fetch_times[ri], &f)))
                })
                .collect();
            return obj! { "shards" => Value::Arr(shards) };
        }
        let shards: Vec<Value> = results
            .iter()
            .enumerate()
            .filter_map(|(ri, r)| {
                let t = &targets[r.target];
                let p = r.profile.as_ref()?;
                let mut ph = phases.clone();
                if !r.inner_hits.is_empty() {
                    ph.push("InnerHitsPhase");
                }
                let fetch = (fetch_times[ri].hits > 0).then(|| profile::fetch(&fetch_times[ri], stored_none, &ph));
                Some(profile::shard(&cluster.node.id, &t.idx.name, t.shard.id as usize, p, fetch))
            })
            .collect();
        obj! { "shards" => Value::Arr(shards), "request" => profile::request(src, &req.indices) }
    });
    for (i, t) in targets.iter().enumerate() {
        let fetched = results.iter().enumerate().any(|(ri, r)| r.target == i && fetch_times[ri].hits > 0);
        let notes = field_usage::Notes::search(&t.engine.table(), src, fetched);
        t.shard.counters.field_usage.lock().record(&notes.used);
    }
    for t in targets {
        t.shard.counters.fetch_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if !src.stats.is_empty() {
            let mut g = t.shard.counters.groups.lock();
            for name in &src.stats {
                g.entry(name.clone()).or_insert((0, 0)).1 += 1;
            }
        }
    }
    let aggs_out = match &agg_req {
        Some(a) => {
            // Reduced in shard request order (`QueryPhaseResultConsumer` sorts by shard index):
            // ties in `top_hits` and the rounding of float sums depend on it.
            let mut order: Vec<usize> = (0..results.len()).collect();
            order.sort_by_key(|&i| request_index[results[i].target]);
            let parts: Vec<search_aggs::Aggs> = order.iter().filter_map(|&i| results[i].aggs.take()).collect();
            let mut out = aggs::reduce(cluster, &st, a, parts, req.typed_keys).map_err(|e| with_query_failures(e, &cluster.node.id, targets, &failures))?;
            finish_top_hits(cluster, &st, req, targets, &results, &mut out)?;
            if let Some(raw) = &src.aggs
                && aggs::mentions_tsid(raw)
            {
                aggs::render_tsid_keys(raw, &mut out, req.typed_keys);
            }
            Some(out)
        }
        None => None,
    };
    // A request with only suggestions skips the query phase (no hits, total 0, and no
    // `terminated_early` either).
    let suggest_only = src.suggest.is_some() && src.query.is_none() && src.knn.is_empty() && src.aggs.is_none();
    // `SearchPhaseController.newSearchPhaseResults`: a request that only wants an accurate
    // count (size 0; no aggregations, suggestions, rank, knn or profile) reduces through
    // `CountOnlyQueryPhaseResultConsumer`, which always reports `terminated_early`.
    let count_only = cluster.dialect != crate::cluster::Dialect::OpenSearch
        && src.size == Some(0)
        && src.aggs.is_none()
        && src.suggest.is_none()
        && src.rank.is_none()
        && src.knn.is_empty()
        && !src.profile
        && (src.track_total_hits == Some(crate::search_source::TrackTotalHits::Accurate) || (src.track_total_hits.is_none() && (req.total_as_int || req.scroll.is_some())));
    let terminated_early = if suggest_only {
        None
    } else if src.terminate_after.is_some() || count_only {
        Some(results.iter().any(|r| r.terminated_early))
    } else if cluster.dialect == crate::cluster::Dialect::OpenSearch && results.iter().any(|r| r.terminated_early) {
        Some(true)
    } else {
        None
    };
    let suggest_out = match &src.suggest {
        Some(sg) => {
            // `collapse` asks the fetch phase for its field as a doc value field
            // (`CollapseContext`), which completion options are fetched through too.
            let mut sopts = opts.clone();
            if let Some(c) = &src.collapse
                && !sopts.docvalue_fields.iter().any(|f| f.field == c.field)
            {
                sopts.docvalue_fields.push(crate::search_source::FieldAndFormat { field: c.field.clone(), format: None, include_unmapped: None });
            }
            // Suggestions come from the query phase: a shard whose query phase failed gives none.
            let answered: Vec<Target> = targets.iter().enumerate().filter(|(i, _)| !failures.iter().any(|(f, _)| f == i)).map(|(_, t)| t.clone()).collect();
            let mut failed = Vec::new();
            let mut out = suggest::run(cluster, &answered, sg, req.typed_keys, &sopts, &mut failed)?;
            if !failed.is_empty() {
                let answered_at: Vec<usize> = (0..targets.len()).filter(|i| !failures.iter().any(|(f, _)| f == i)).collect();
                let fetch = failed.iter().any(|f| f.2);
                for (ai, e, _) in failed {
                    let ti = answered_at[ai];
                    if !failures.iter().any(|(f, _)| *f == ti) {
                        failures.push((ti, e));
                    }
                }
                failures.sort_by_key(|f| f.0);
                let none_left = failures.len() >= targets.len();
                if none_left || !req.allow_partial {
                    let mut e = all_shards_failed(&cluster.node.id, targets, &failures, if none_left { "all shards failed" } else { "Partial shards failure" });
                    // One shard runs query and fetch together.
                    if fetch && targets.len() > 1 {
                        e = e.set_meta("phase", "fetch");
                    }
                    return Err(e);
                }
            }
            if src.highlight.is_some() || src.explain == Some(true) || !src.script_fields.is_empty() || results.iter().any(|r| !r.inner_hits.is_empty()) {
                complete_completion_options(cluster, &st, req, targets, &results, &mut out, suggest_only)?;
            }
            Some(out)
        }
        None => None,
    };
    Ok(SearchOutcome {
        took_ms: started.elapsed().as_millis() as u64,
        shards_total: targets.len(),
        shards_skipped: 0,
        failures: failures.iter().map(|(i, e)| shard_failure(&cluster.node.id, &targets[*i], e)).collect(),
        // With no shard answering (the rest skipped), the empty reduction reports zero hits
        // even when totals aren't tracked.
        total: if results.is_empty() && skipped_ok {
            Some((0, TotalRelation::EqualTo))
        } else {
            tracking.map(|_| if suggest_only { (0, TotalRelation::EqualTo) } else { (total, relation) })
        },
        // Elasticsearch collects no top docs at all for `size: 0`, whatever `from` says.
        max_score: if suggest_only || (size == 0 && (!built_keys.is_empty() || src.collapse.is_some() || !src.rescore.is_empty() || (from > 0 && !crate::opensearch::active()))) {
            None
        } else if results.is_empty() && skipped_ok {
            // No shard answered the query phase (the rest were skipped): the empty reduction's
            // max score, as when every shard is skipped.
            Some(0.0)
        } else {
            max_score
        },
        hits: if suggest_only { Vec::new() } else { hits_out },
        aggs: aggs_out,
        suggest: suggest_out,
        terminated_early,
        timed_out: results.iter().any(|r| r.timed_out),
        profile: profile_out,
        scroll_id: None,
        pit_id: None,
        ranked,
        clusters: None,
    })
}

#[allow(clippy::too_many_arguments)]
/// Pages of at least this many hits render on several threads.
const PARALLEL_FETCH_MIN: usize = 64;

/// One page entry's fetch outcome and the time it took.
type Fetched = Option<(EsResult<Value>, u64)>;

/// A page position rendered by a unit of [`fetch_parallel`], with its outcome and time.
type UnitOutcome = (usize, EsResult<Value>, u64);

/// Renders a large page's hits on pool threads (Elasticsearch fetches each shard's hits on
/// that shard's thread): the page splits into units of one shard's hits, at least
/// [`PARALLEL_FETCH_MIN`] / 2 each, rendered in page order; a unit stops at its first failure
/// (the serial loop skips a failed shard's later hits). Returns one outcome per page entry
/// (`None` past a failure), or `None` to render serially (small pages, nested in a worker).
fn fetch_parallel(page: &[(usize, ShardHit)], render: impl Fn(usize, &ShardHit) -> EsResult<Value> + Sync) -> Option<Vec<Fetched>> {
    use search_index::search::pool;
    let cores = pool::cores();
    if page.len() < PARALLEL_FETCH_MIN || cores <= 1 || pool::in_worker() {
        return None;
    }
    // Units: runs of one shard's page positions, in page order.
    let mut by_shard: Vec<(usize, Vec<usize>)> = Vec::new();
    for (pos, (ri, _)) in page.iter().enumerate() {
        match by_shard.iter_mut().find(|(r, _)| r == ri) {
            Some((_, v)) => v.push(pos),
            None => by_shard.push((*ri, vec![pos])),
        }
    }
    let threads = cores.min(pool::threads().max(1)).max(1);
    let per = page.len().div_ceil(threads).max(PARALLEL_FETCH_MIN / 2);
    let units: Vec<&[usize]> = by_shard.iter().flat_map(|(_, v)| v.chunks(per)).collect();
    if units.len() <= 1 {
        return None;
    }
    let slots: Vec<parking_lot::Mutex<Vec<UnitOutcome>>> = (0..units.len()).map(|_| parking_lot::Mutex::new(Vec::new())).collect();
    let opensearch = crate::opensearch::active();
    let lookups = extra::LookupScope::current();
    let page_state = contexts::page_state();
    pool::for_each_index(units.len(), (units.len() - 1).min(cores - 1), &|u| {
        let _lookups = extra::LookupScope::adopt(lookups.clone());
        let out = crate::opensearch::with(opensearch, || {
            contexts::with_page(page_state.clone(), || {
                let mut out = Vec::with_capacity(units[u].len());
                for &pos in units[u] {
                    let (ri, h) = &page[pos];
                    let at = std::time::Instant::now();
                    let r = render(*ri, h);
                    let failed = r.is_err();
                    out.push((pos, r, at.elapsed().as_nanos() as u64));
                    if failed {
                        break;
                    }
                }
                out
            })
        });
        *slots[u].lock() = out;
    });
    let mut fetched: Vec<Fetched> = (0..page.len()).map(|_| None).collect();
    for s in slots {
        for (pos, r, nanos) in s.into_inner() {
            fetched[pos] = Some((r, nanos));
        }
    }
    Some(fetched)
}

#[allow(clippy::too_many_arguments)]
fn render_hit(cluster: &Cluster, st: &ClusterState, req: &SearchRequest, t: &Target, r: &ShardResult, h: &ShardHit, opts: &FetchOpts, formats: &[sort::ValueFormat]) -> EsResult<Value> {
    // Fetch-phase scripts read the search's runtime fields too.
    let _ambient = r.runtime.as_ref().map(|scope| {
        search_index::search::set_ambient_control(Some(search_index::search::SearchControl {
            deadline: None,
            cancelled: None,
            ext: Some(scope.clone() as Arc<dyn std::any::Any + Send + Sync>),
        }))
    });
    let src = &req.source;
    let table = t.engine.table();
    // The fetch phase builds its field fetchers (and fails) on shards with hits only.
    if !src.fields.is_empty() {
        fields::check_internal(&src.fields)?;
        fields::check_formats(&table, &src.fields)?;
    }
    let mut m = Map::new();
    let explain = src.explain == Some(true);
    if explain {
        m.insert("_shard".into(), Value::Str(format!("[{}][{}]", t.idx.name, t.shard.id)));
        m.insert("_node".into(), Value::Str(cluster.node.id.clone()));
    }
    m.insert("_index".into(), Value::Str(t.idx.name.clone()));
    let stored = r.searcher.stored(h.doc).map_err(crate::engine::convert::index_error)?;
    let id = hits::stored_id(&stored).unwrap_or_default();
    // `stored_fields: _none_` loads no stored fields at all, `_id` included; Elasticsearch's
    // script fields and highlight phases still ask for the id (OpenSearch's don't).
    if opts.stored_fields != Some(None) || ((!src.script_fields.is_empty() || src.highlight.is_some()) && cluster.dialect == crate::cluster::Dialect::Elasticsearch) {
        m.insert("_id".into(), Value::Str(id));
    }
    let score = if h.sort.is_empty() || src.track_scores || src.sort.iter().any(|s| matches!(s, Sort::Score { .. })) { Some(h.score) } else { None };
    hits::render_doc_stored(&table, Some(&t.idx), &r.searcher, h.doc, &stored, opts, &mut m, score, req.now_ms)?;
    if (!src.fields.is_empty() || !src.docvalue_fields.is_empty()) && (src.runtime_mappings.is_some() || !t.idx.mapping.runtime.is_empty()) {
        runtime_fields_of_hit(cluster, st, src, t, &table, &r.searcher, h.doc, &mut m)?;
    }
    if !src.script_fields.is_empty() {
        script_fields_of_hit(cluster, st, t, &table, &r.searcher, h.doc, h.score, &src.script_fields, &mut m)?;
    }
    // `_percolator_document_slot[_<name>]`: the documents each stored query matched.
    // One percolate query reports `_percolator_document_slot`; several, suffixed by name.
    for pc in &r.percolate {
        let field = match (&pc.name, r.percolate.len()) {
            (Some(n), 2..) => format!("_percolator_document_slot_{n}"),
            _ => "_percolator_document_slot".to_string(),
        };
        if let Some(s) = pc.slots(h.doc) {
            let mut add: Vec<(String, Value)> = vec![(field.clone(), Value::Arr(s.iter().map(|x| Value::from(*x as i64)).collect()))];
            for (slot, names) in pc.named(h.doc) {
                add.push((format!("{field}_{slot}_matched_queries"), Value::Arr(names.iter().cloned().map(Value::Str).collect())));
            }
            for (k, v) in add {
                match m.get_mut("fields") {
                    Some(Value::Obj(fm)) => {
                        fm.insert(k, v);
                    }
                    _ => {
                        let at = m.get_index_of("_source").map(|i| i + 1).unwrap_or(m.len());
                        let mut fm = Map::new();
                        fm.insert(k, v);
                        m.shift_insert(at, "fields".into(), Value::Obj(fm));
                    }
                }
            }
            // The hit's document fields are a hash map.
            if let Some(Value::Obj(fm)) = m.get_mut("fields") {
                crate::ingest::doc::java_hash_order(fm, false);
            }
        }
    }
    // After a rescore the hit's group may be another doc's (see `query_shard`): the fetched
    // field and the inner hits read the hit's own value, as Elasticsearch fetches them.
    let own_group = match (&src.collapse, &h.group) {
        (Some(c), Some(_)) if !src.rescore.is_empty() && !crate::opensearch::active() => own_collapse_value(&r.searcher, &t.engine.table(), &c.field, h)?,
        _ => h.group.clone(),
    };
    if let (Some(c), Some(g)) = (&src.collapse, &own_group) {
        let table2 = t.engine.table();
        let gv = match (g, table2.get(&c.field)) {
            (SortValue::Null, _) => Value::Null,
            (SortValue::Bytes(b), Some(f)) if f.kind == crate::engine::fields::Kind::Ip && b.len() == 16 => Value::Str(search_index::query::range::inet_to_string(b)),
            (SortValue::Bytes(b), _) => Value::Str(String::from_utf8_lossy(b).into_owned()),
            (SortValue::Long(l), Some(f)) => hits::format_numeric(f, *l, None).unwrap_or(Value::from(*l)),
            (v, _) => sort::render_value(v, &sort::ValueFormat::Long, h.doc, h.score),
        };
        // The collapse value is fetched like a doc value field: a hit without one gets none.
        let fv = Value::Arr(vec![gv.clone()]);
        match m.get_mut("fields") {
            _ if gv.is_null() => {}
            Some(Value::Obj(fm)) => {
                fm.insert(c.field.clone(), fv);
            }
            _ => {
                let at = m.get_index_of("_source").map(|i| i + 1).unwrap_or(m.len());
                let mut fm = Map::new();
                fm.insert(c.field.clone(), fv);
                m.shift_insert(at, "fields".into(), Value::Obj(fm));
            }
        }
        if !c.inner_hits.is_empty() {
            m.insert("inner_hits".into(), collapse_inner_hits(cluster, req, c, &gv)?);
        }
    }
    if let Some(hl) = &src.highlight {
        let mut spec = highlight::Spec::parse(hl)?;
        spec.weight_matches = highlight::weight_matches_supported(src);
        // The stored fields read at the top serve the highlighter too.
        let source = crate::fetch::source_bytes(&stored).map(crate::fetch::parse_source).transpose()?;
        let qctx = QueryCtx::new(cluster, st, &t.idx, &table, &r.searcher, req.now_ms);
        let id = m.get("_id").and_then(Value::as_str).unwrap_or_default().to_string();
        let mut out = highlight::highlight_hit(&qctx, &spec, &r.query, source.as_ref(), &id, &r.searcher, Some(h.doc), Some(&stored), Some(&r.match_weights))?;
        // Percolated documents are highlighted with the stored query that matched them.
        for pc in &r.percolate {
            out.extend(pc.highlight(cluster, st, &spec, h.doc, req.now_ms)?);
        }
        if !out.is_empty() {
            m.insert("highlight".into(), Value::Obj(out));
        }
    }
    if !h.sort.is_empty() {
        let vals: Vec<Value> = h.sort.iter().zip(formats).map(|(v, f)| sort::render_value(v, f, h.doc, h.score)).collect();
        m.insert("sort".into(), Value::Arr(vals));
    }
    if !r.named.is_empty() {
        let mut matched: Vec<(String, f32)> = Vec::new();
        for (name, q) in &r.named {
            if let Ok(e) = r.searcher.explain(q, h.doc)
                && e.is_match()
                && !matched.iter().any(|(n, _)| n == name)
            {
                matched.push((name.clone(), e.value()));
            }
        }
        if !matched.is_empty() {
            if req.include_named_queries_score {
                let mut mm = Map::new();
                for (n, s) in matched {
                    mm.insert(n, Value::Num(Number::Float(crate::fetch::java_float_as_double(s))));
                }
                m.insert("matched_queries".into(), Value::Obj(mm));
            } else {
                m.insert("matched_queries".into(), Value::Arr(matched.into_iter().map(|(n, _)| Value::Str(n)).collect()));
            }
        }
    }
    if explain {
        let mut e = r.searcher.explain(&r.query, h.doc).map_err(crate::engine::convert::index_error)?;
        if !r.rescored.is_empty() {
            let qctx = QueryCtx::new(cluster, st, &t.idx, &table, &r.searcher, req.now_ms);
            e = explain_rescored(&qctx, &r.searcher, src, &r.rescored, h.doc, e)?;
        }
        m.insert("_explanation".into(), explanation_json(&e));
    }
    if !r.inner_hits.is_empty() {
        let stored = r.searcher.stored(h.doc).map_err(crate::engine::convert::index_error)?;
        let source = crate::fetch::source_bytes(&stored).map(crate::fetch::parse_source).transpose()?;
        let qctx = QueryCtx::new(cluster, st, &t.idx, &table, &r.searcher, req.now_ms);
        let mut ih = inner::render(&qctx, t, r, h.doc, source.as_ref(), req.total_as_int)?;
        // `stored_fields: _none_` reaches nested inner hits (their sub-context reads the
        // search's stored fields context): they print no `_id` either.
        if req.source.stored_fields.as_ref().is_some_and(|sf| sf.fields.is_none())
            && let Value::Obj(defs) = &mut ih
        {
            for d in defs.values_mut() {
                let hits = match d {
                    Value::Obj(dm) => match dm.get_mut("hits") {
                        Some(Value::Obj(hm)) => hm.get_mut("hits"),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(Value::Arr(hits)) = hits {
                    for hit in hits.iter_mut() {
                        if let Value::Obj(hm) = hit
                            && hm.contains_key("_nested")
                        {
                            hm.shift_remove("_id");
                        }
                    }
                }
            }
        }
        // `ExpandSearchPhase` puts the collapse groups' inner hits into the same `HashMap`,
        // after the fetch phase's (a group of the same name replaces a nested one).
        if let (Some(Value::Obj(groups)), Value::Obj(nested)) = (m.shift_remove("inner_hits"), &mut ih) {
            let mut all = std::mem::take(nested);
            all.extend(groups);
            ih = Value::Obj(crate::json::java_hash_map(all, 16));
        }
        m.insert("inner_hits".into(), ih);
    }
    Ok(Value::Obj(m))
}

/// Completes the `top_hits` hits of the reduced aggregations: each carries a hidden marker
/// ([`aggs::TOP_HIT_MARKER`]) naming its shard and doc, and gets here what needs the search's
/// shard results: `highlight` against the search's query, `matched_queries` of its named
/// queries, `explain`, runtime fields of the top-level `fields`. `rest_total_hits_as_int`
/// applies to their totals too.
/// `ScriptFieldsPhase`: each script field's values into the hit's `fields` (after `_source`).
#[allow(clippy::too_many_arguments)]
fn script_fields_of_hit(cluster: &Cluster, st: &ClusterState, t: &Target, table: &Arc<crate::engine::fields::FieldTable>, searcher: &Searcher, doc: u32, score: f32, fields: &[crate::search_source::ScriptField], m: &mut Map) -> EsResult<()> {
    let Some((seg, local)) = searcher.segment_of(doc) else { return Ok(()) };
    let id_fielddata = id_fielddata::scripts(st);
    let mut sf = Map::new();
    for f in fields {
        let bound = cluster.scripts.compile(st, &f.script, search_painless::Context::Field)?;
        let p = scripts::Painless { bound, table: table.clone(), shard: t.shard.id as i32, index: t.idx.name.clone(), id_fielddata, counted: false };
        match scripts::script_field(&p, seg, local, score) {
            Ok(v) => {
                sf.insert(f.name.clone(), Value::Arr(v));
            }
            Err(_) if f.ignore_failure => {}
            Err(e) => return Err(e),
        }
    }
    if sf.is_empty() {
        return Ok(());
    }
    let pos = m.get_index_of("fields");
    match m.get_mut("fields") {
        Some(Value::Obj(fm)) => {
            for (k, v) in sf {
                fm.insert(k, v);
            }
        }
        _ => {
            let at = pos.unwrap_or_else(|| m.get_index_of("_source").map(|i| i + 1).unwrap_or(m.len()));
            m.shift_insert(at, "fields".into(), Value::Obj(sf));
        }
    }
    // `SearchHit.documentFields` is a hash map.
    if let Some(Value::Obj(fm)) = m.get_mut("fields") {
        crate::ingest::doc::java_hash_order(fm, false);
    }
    Ok(())
}

fn finish_top_hits(cluster: &Cluster, st: &ClusterState, req: &SearchRequest, targets: &[Target], results: &[ShardResult], out: &mut Value) -> EsResult<()> {
    match out {
        Value::Obj(m) => {
            if let Some(Value::Obj(mk)) = m.shift_remove(aggs::TOP_HIT_MARKER) {
                complete_top_hit(cluster, st, req, targets, results, &mk, m)?;
                return Ok(());
            }
            if req.total_as_int
                && let Some(Value::Obj(h)) = m.get_mut("hits")
                && h.contains_key("max_score")
                && matches!(h.get("hits"), Some(Value::Arr(_)))
                && let Some(v) = h.get("total").and_then(|t| t.get("value")).cloned()
            {
                h.insert("total".into(), v);
            }
            for (_, v) in m.iter_mut() {
                finish_top_hits(cluster, st, req, targets, results, v)?;
            }
        }
        Value::Arr(a) => {
            for v in a {
                finish_top_hits(cluster, st, req, targets, results, v)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Inserts `key` before the first of `before` present in `m` (the order hits print in).
fn insert_before(m: &mut Map, key: &str, v: Value, before: &[&str]) {
    match before.iter().find_map(|b| m.get_index_of(*b)) {
        Some(at) => {
            m.shift_insert(at, key.to_string(), v);
        }
        None => {
            m.insert(key.to_string(), v);
        }
    }
}

fn complete_top_hit(cluster: &Cluster, st: &ClusterState, req: &SearchRequest, targets: &[Target], results: &[ShardResult], mk: &Map, m: &mut Map) -> EsResult<()> {
    let index = mk.get("index").and_then(Value::as_str).unwrap_or_default();
    let shard = mk.get("shard").and_then(Value::as_i64).unwrap_or(-1);
    let Some(doc) = mk.get("doc").and_then(Value::as_i64).map(|d| d as u32) else { return Ok(()) };
    let Some(r) = results.iter().find(|r| targets[r.target].idx.name == index && targets[r.target].shard.id as i64 == shard) else { return Ok(()) };
    let t = &targets[r.target];
    let table = t.engine.table();
    let src = &req.source;
    let nested = m.contains_key("_nested");
    if !nested && mk.get("parent_fields").is_some() && !src.fields.is_empty() && (src.runtime_mappings.is_some() || !t.idx.mapping.runtime.is_empty()) {
        runtime_fields_of_hit(cluster, st, src, t, &table, &r.searcher, doc, m)?;
    }
    if let Some(Value::Obj(spec)) = mk.get("script_fields") {
        let fields = crate::search_source::script_fields_from_value(spec)?;
        let score = mk.get("score").and_then(Value::as_f64).map_or(f32::NAN, |s| s as f32);
        script_fields_of_hit(cluster, st, t, &table, &r.searcher, doc, score, &fields, m)?;
    }
    const AFTER_FIELDS: &[&str] = &["sort", "matched_queries", "_explanation", "inner_hits"];
    if let Some(Value::Obj(hl)) = mk.get("highlight") {
        let spec = highlight::Spec::parse(hl)?;
        let root = match m.get("_nested") {
            None => Some(doc),
            Some(_) => r.searcher.segment_of(doc).and_then(|(seg, local)| inner::nested_identity(&table, seg, local).map(|n| seg.base + n.root)),
        };
        if let Some(root) = root {
            let stored = r.searcher.stored(root).map_err(crate::engine::convert::index_error)?;
            let source = crate::fetch::source_bytes(&stored).map(crate::fetch::parse_source).transpose()?;
            let qctx = QueryCtx::new(cluster, st, &t.idx, &table, &r.searcher, req.now_ms);
            let id = m.get("_id").and_then(Value::as_str).unwrap_or_default().to_string();
            let out = highlight::highlight_hit(&qctx, &spec, &r.query, source.as_ref(), &id, &r.searcher, Some(root), Some(&stored), Some(&r.match_weights))?;
            if !out.is_empty() {
                insert_before(m, "highlight", Value::Obj(out), AFTER_FIELDS);
            }
        }
    }
    if !r.named.is_empty() {
        let mut matched: Vec<(String, f32)> = Vec::new();
        for (name, q) in &r.named {
            if let Ok(e) = r.searcher.explain(q, doc)
                && e.is_match()
                && !matched.iter().any(|(n, _)| n == name)
            {
                matched.push((name.clone(), e.value()));
            }
        }
        if !matched.is_empty() {
            let v = if req.include_named_queries_score {
                let mut mm = Map::new();
                for (n, s) in matched {
                    mm.insert(n, Value::Num(Number::Float(crate::fetch::java_float_as_double(s))));
                }
                Value::Obj(mm)
            } else {
                Value::Arr(matched.into_iter().map(|(n, _)| Value::Str(n)).collect())
            };
            insert_before(m, "matched_queries", v, &["_explanation", "inner_hits"]);
        }
    }
    if mk.get("explain").is_some() {
        let e = r.searcher.explain(&r.query, doc).map_err(crate::engine::convert::index_error)?;
        m.shift_insert(0, "_shard".into(), Value::Str(format!("[{}][{}]", t.idx.name, t.shard.id)));
        m.shift_insert(1, "_node".into(), Value::Str(cluster.node.id.clone()));
        insert_before(m, "_explanation", explanation_json(&e), &["inner_hits"]);
    }
    // `top_hits` hits get the query's `inner_hits` too (`TopHitsAggregator` runs the fetch
    // phase's inner hits sub-phase): OpenSearch 2.14+ and Elasticsearch 9.5.5 alike.
    if !nested && !r.inner_hits.is_empty() {
        let stored = r.searcher.stored(doc).map_err(crate::engine::convert::index_error)?;
        let source = crate::fetch::source_bytes(&stored).map(crate::fetch::parse_source).transpose()?;
        let qctx = QueryCtx::new(cluster, st, &t.idx, &table, &r.searcher, req.now_ms);
        m.insert("inner_hits".into(), inner::render(&qctx, t, r, doc, source.as_ref(), req.total_as_int)?);
    }
    Ok(())
}

/// Runtime fields the `fields` option asks for (names or patterns), computed for one hit and
/// added to its `fields`.
#[allow(clippy::too_many_arguments)]
fn runtime_fields_of_hit(cluster: &Cluster, st: &ClusterState, src: &SearchSource, t: &Target, table: &Arc<crate::engine::fields::FieldTable>, searcher: &Searcher, doc: u32, m: &mut Map) -> EsResult<()> {
    let has_lookup = src.runtime_mappings.as_ref().is_some_and(|r| r.values().any(|d| d.get("type").and_then(Value::as_str) == Some("lookup")));
    // The shard's scope (parsed and compiled once for all its hits), else one for this hit.
    let shard = t.shard.id as i32;
    let scope = match runtime::RuntimeScope::ambient().filter(|s| s.is_for(&t.idx.name, shard)) {
        Some(s) => Some(s),
        None => runtime::RuntimeScope::build(&cluster.scripts, cluster.state(), &t.idx, table, shard, src.runtime_mappings.as_ref())?,
    };
    let _ = st;
    if scope.is_none() && !has_lookup {
        return Ok(());
    }
    let Some((seg, local)) = searcher.segment_of(doc) else { return Ok(()) };
    let mut out = Map::new();
    lookup_fields_of_hit(cluster, src, table, searcher, doc, &mut out)?;
    // Runtime fields shadowing a concrete field the concrete fetch already printed: their
    // own values (or none) replace those.
    let mut shadowed_empty = Vec::new();
    if let Some(scope) = &scope {
        let rt = scope.fields();
        // `docvalue_fields` print values as doc values, sorted; `fields` too (keyword values
        // come from sorted binary doc values), in their own format: `fields` win.
        let specs = src.docvalue_fields.iter().chain(src.fields.iter());
        for spec in specs {
            let names = if spec.field.contains('*') { rt.matching(&spec.field) } else if rt.get(&spec.field).is_some() { vec![spec.field.clone()] } else { Vec::new() };
            for name in names {
                let Some(ev) = scope.get(&name) else { continue };
                let mut vals = ev.leaf(seg)?.values(local)?;
                runtime::sort_as_doc_values(&mut vals);
                if vals.is_empty() {
                    if table.get(&name).is_some() {
                        shadowed_empty.push(name);
                    }
                    continue;
                }
                out.insert(name, Value::Arr(runtime::render(&ev.field, &vals, spec.format.as_deref())?));
            }
        }
    }
    if let Some(Value::Obj(f)) = m.get_mut("fields") {
        for name in &shadowed_empty {
            if !out.contains_key(name) {
                f.shift_remove(name);
            }
        }
    }
    if out.is_empty() {
        return Ok(());
    }
    let pos = m.get_index_of("fields");
    let fields = match m.get_mut("fields") {
        Some(Value::Obj(f)) => f,
        _ => {
            let at = pos.unwrap_or_else(|| m.get_index_of("_source").map(|i| i + 1).unwrap_or(m.len()));
            m.shift_insert(at, "fields".into(), Value::Obj(Map::new()));
            match m.get_mut("fields") {
                Some(Value::Obj(f)) => f,
                _ => return Ok(()),
            }
        }
    };
    fields.extend(out);
    // The fields map is filled in field-name order, then printed in hash order.
    fields.sort_keys();
    crate::ingest::doc::java_hash_order(fields, false);
    Ok(())
}

/// `lookup` runtime fields (`LookupRuntimeFieldType`) of the request's `fields`: per value of
/// `input_field`, the `fetch_fields` of the first document of `target_index` whose
/// `target_field` holds it; values without a match are left out.
fn lookup_fields_of_hit(cluster: &Cluster, src: &SearchSource, table: &Arc<crate::engine::fields::FieldTable>, searcher: &Searcher, doc: u32, out: &mut Map) -> EsResult<()> {
    let Some(rm) = &src.runtime_mappings else { return Ok(()) };
    let lookups: Vec<(&String, &Map)> = rm.iter().filter_map(|(k, d)| d.as_object().filter(|m| m.get("type").and_then(Value::as_str) == Some("lookup")).map(|m| (k, m))).collect();
    if lookups.is_empty() {
        return Ok(());
    }
    let Some((seg, local)) = searcher.segment_of(doc) else { return Ok(()) };
    let stored = searcher.stored(doc).map_err(crate::engine::convert::index_error)?;
    let source = crate::fetch::source_bytes(&stored).map(crate::fetch::parse_source).transpose()?;
    for (name, def) in lookups {
        if !src.fields.iter().any(|f| f.field == *name || (f.field.contains('*') && crate::xcontent::filter::glob_match(&f.field, name))) {
            continue;
        }
        let input = def.get("input_field").map(Value::to_es_string).unwrap_or_default();
        let target_index = def.get("target_index").map(Value::to_es_string).unwrap_or_default();
        let target_field = def.get("target_field").map(Value::to_es_string).unwrap_or_default();
        let fetch = def.get("fetch_fields").cloned().unwrap_or(Value::Arr(Vec::new()));
        let mut got = Map::new();
        let mut ignored = Map::new();
        let spec = crate::search_source::FieldAndFormat { field: input.clone(), format: None, include_unmapped: None };
        fields::fetch_fields(table, source.as_ref(), std::slice::from_ref(&spec), &mut got, &mut ignored, seg, local)?;
        let values: Vec<Value> = match got.shift_remove(&input) {
            Some(Value::Arr(a)) => a,
            _ => Vec::new(),
        };
        let mut found = Vec::new();
        for v in values {
            let body = obj! {
                "size" => 1,
                "_source" => false,
                "query" => obj! { "term" => obj! { target_field.as_str() => v } },
                "fields" => fetch.clone(),
            };
            let req = SearchRequest {
                indices: vec![target_index.clone()],
                opts: IndicesOptions::STRICT_EXPAND_OPEN,
                source: SearchSource::parse(&body, None)?,
                routing: None,
                preference: None,
                dfs: false,
                request_cache: Some(false),
                total_as_int: false,
                typed_keys: false,
                allow_partial: false,
                scroll: None,
                include_named_queries_score: false,
                force_synthetic_source: false,
                now_ms: crate::time::now_millis(),
                fused: None,
                rank_only: false,
            };
            let o = execute(cluster, &req)?;
            if let Some(Value::Obj(f)) = o.hits.first().and_then(|h| h.get("fields")) {
                found.push(Value::Obj(f.clone()));
            }
        }
        if !found.is_empty() {
            out.insert(name.clone(), Value::Arr(found));
        }
    }
    Ok(())
}

/// `ExpandSearchPhase`'s failure: the group searches' error under a coordinator-level
/// "failed to expand hits" with no shard failures (and so no root causes).
fn expand_failure(e: EsError) -> EsError {
    EsError::new(e.status, "search_phase_execution_exception", "failed to expand hits")
        .with_meta("phase", "expand")
        .with_meta("grouped", true)
        .with_meta("failed_shards", Value::Arr(Vec::new()))
        .caused_by(e)
        .with_root_causes(Vec::new())
}

fn is_expand_failure(e: &EsError) -> bool {
    e.kind == "search_phase_execution_exception" && e.reason() == "failed to expand hits"
}

/// The `inner_hits` of a collapsed group: a search per inner hits definition restricted to
/// the group (`CollapseBuilder` expand phase).
fn collapse_inner_hits(cluster: &Cluster, req: &SearchRequest, c: &crate::search_source::Collapse, group: &Value) -> EsResult<Value> {
    let mut out = Map::new();
    for ih in &c.inner_hits {
        let Value::Obj(spec) = ih else { continue };
        let Some(name) = spec.get("name").and_then(Value::as_str) else {
            return Err(EsError::illegal_argument("[inner_hits] must have a [name] when used in [collapse]"));
        };
        let mut sub = req.clone();
        sub.scroll = None;
        // The group's documents among everything the compound retriever matches.
        if let Some(f) = &req.fused {
            sub.fused = Some(Arc::new(fusion::Fused { node: f.node.clone(), cap: usize::MAX }));
        }
        let mut body = Map::new();
        for (k, v) in spec {
            if k != "name" && k != "ignore_unmapped" {
                body.insert(k.clone(), v.clone());
            }
        }
        if !body.contains_key("size") {
            body.insert("size".into(), Value::from(3i64));
        }
        let filter = if group.is_null() {
            obj! { "bool" => obj! { "must_not" => obj! { "exists" => obj! { "field" => c.field.as_str() } } } }
        } else {
            obj! { "term" => obj! { c.field.as_str() => group.clone() } }
        };
        let mut s2 = SearchSource::parse(&Value::Obj(body), None)?;
        let base = req.source.query.clone();
        let f = crate::query::parse_query(&filter, None, &[])?.0;
        let mut b = crate::query::compound::Bool::default();
        if let Some(q) = base {
            b.must.push(q);
        }
        b.filter.push(f);
        s2.query = Some(crate::query::Query::from(b));
        // `ExpandSearchPhase` keeps the search's post filter on every group query.
        if s2.post_filter.is_none() {
            s2.post_filter = req.source.post_filter.clone();
        }
        s2.track_total_hits = Some(TrackTotalHits::Accurate);
        // The group queries run with the search's runtime fields (`ExpandSearchPhase`).
        s2.runtime_mappings = req.source.runtime_mappings.clone();
        sub.source = s2;
        let o = execute(cluster, &sub).map_err(expand_failure)?;
        let mut hits = Map::new();
        if let Some((t, rel)) = o.total {
            if req.total_as_int {
                hits.insert("total".into(), Value::from(t as i64));
            } else {
                hits.insert("total".into(), obj! { "value" => t as i64, "relation" => if rel == TotalRelation::EqualTo { "eq" } else { "gte" } });
            }
        }
        hits.insert("max_score".into(), match o.max_score { Some(s) if !s.is_nan() => Value::Num(Number::Float(crate::fetch::java_float_as_double(s))), _ => Value::Null });
        hits.insert("hits".into(), Value::Arr(o.hits));
        out.insert(name.to_string(), obj! { "hits" => Value::Obj(hits) });
    }
    // `SearchHit.innerHits` is a `HashMap` filled by name.
    Ok(Value::Obj(crate::json::java_hash_map(out, 16)))
}

/// An explanation as `_explanation` prints it.
pub fn explanation_json(e: &search_index::Explanation) -> Value {
    let mut m = Map::new();
    let v = e.value;
    m.insert(
        "value".into(),
        // Lucene's `Explanation.noMatch` carries `0f`: a float, printed `0.0`.
        if v.is_integral() && !e.matched {
            Value::Num(Number::Float(v.as_f64()))
        } else if v.is_integral() {
            Value::from(v.as_f64() as i64)
        } else {
            match v {
                search_index::search::ExplValue::Float(f) => Value::Num(Number::Float(crate::fetch::java_float_as_double(f))),
                other => Value::Num(Number::Float(other.as_f64())),
            }
        },
    );
    m.insert("description".into(), Value::Str(e.description.clone()));
    m.insert("details".into(), Value::Arr(e.details.iter().map(explanation_json).collect()));
    Value::Obj(m)
}

/// `SearchPhaseExecutionException` (`all shards failed`) with grouped shard failures.
/// One entry of `_shards.failures` / `failed_shards`.
pub fn shard_failure(node: &str, t: &Target, e: &EsError) -> Value {
    obj! {
        "shard" => t.shard.id as i64,
        "index" => t.idx.name.as_str(),
        "node" => node,
        "reason" => e.to_value(false),
    }
}

/// A failed final reduce (`aggs::reduce_failure`) lists the shards the query phase lost
/// (`AbstractSearchAsyncAction.onPhaseFailure` builds the exception with every shard
/// failure so far), grouped as `all_shards_failed` groups them.
fn with_query_failures(e: EsError, node: &str, targets: &[Target], failures: &[(usize, EsError)]) -> EsError {
    if failures.is_empty() || e.kind != "search_phase_execution_exception" {
        return e;
    }
    let mut items = Vec::new();
    let mut roots: Vec<EsError> = Vec::new();
    let mut seen: Vec<(String, String)> = Vec::new();
    for (i, f) in failures {
        let t = &targets[*i];
        let key = (t.idx.name.clone(), format!("{}:{}", f.kind, f.reason()));
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        items.push(shard_failure(node, t, f));
        roots.extend(f.guess_root_causes().into_iter().cloned());
    }
    e.with_meta("failed_shards", Value::Arr(items)).with_root_causes(roots)
}

pub fn all_shards_failed(node: &str, targets: &[Target], failures: &[(usize, EsError)], reason: &str) -> EsError {
    let mut items = Vec::new();
    let mut roots: Vec<EsError> = Vec::new();
    let mut seen: Vec<(String, String)> = Vec::new();
    for (i, e) in failures {
        let t = &targets[*i];
        // Grouped: one failure per (index, cause) like `ExceptionsHelper.groupBy`.
        let key = (t.idx.name.clone(), format!("{}:{}", e.kind, e.reason()));
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        items.push(shard_failure(node, t, e));
        for r in e.guess_root_causes() {
            roots.push(r.clone());
        }
    }
    let status = failures.iter().map(|(_, e)| e.status).max().unwrap_or(500);
    let mut err = EsError::new(status, "search_phase_execution_exception", reason).with_meta("phase", "query").with_meta("grouped", true).with_meta("failed_shards", Value::Arr(items));
    // `SearchPhaseExecutionException.getCause()` falls back to the first guessed root cause;
    // it is not rendered when that is the shard failure itself. A plain Java root is wrapped
    // (so it shows twice). Elasticsearch's "Partial shards failure" (partial results not
    // allowed) carries no cause and renders none.
    let partial = reason == "Partial shards failure" && !crate::opensearch::active();
    if let Some((_, first)) = failures.first().filter(|_| !partial) {
        if crate::opensearch::active() && first.es {
            // OpenSearch 3's `getEffectiveCause`: the first shard failure's own exception,
            // always rendered.
            err = err.caused_by(first.clone());
        } else if !first.es {
            err = err.caused_by(EsError::java(first.status, first.kind.clone(), first.reason()).caused_by(first.clone()));
        } else if let Some(root) = first.guess_root_causes().first()
            && !std::ptr::eq(*root, first)
        {
            err = err.caused_by((*root).clone());
        }
    }
    err.with_root_causes(roots)
}

/// With `seq_no_primary_term` on the request, Elasticsearch's fetch sub-contexts (`top_hits`,
/// `inner_hits` at any depth) print `_seq_no: -2` and `_primary_term: 0` (unassigned) on every
/// hit that doesn't load them itself. Walks a rendered `inner_hits` or `aggregations` tree and
/// adds them to the hits (objects with `_index` and `_id` inside a `hits.hits` array).
fn mark_unassigned_seq_no(v: &mut Value) {
    match v {
        Value::Obj(m) => {
            if let Some(Value::Obj(hits)) = m.get_mut("hits")
                && let Some(Value::Arr(list)) = hits.get_mut("hits")
            {
                for h in list.iter_mut() {
                    if let Value::Obj(hm) = h
                        && hm.contains_key("_index")
                        && hm.contains_key("_id")
                        && !hm.contains_key("_seq_no")
                    {
                        hm.insert("_seq_no".into(), Value::from(-2i64));
                        hm.insert("_primary_term".into(), Value::from(0i64));
                    }
                }
            }
            for (k, c) in m.iter_mut() {
                if !matches!(k.as_str(), "_source" | "fields" | "meta") {
                    mark_unassigned_seq_no(c);
                }
            }
        }
        Value::Arr(a) => a.iter_mut().for_each(mark_unassigned_seq_no),
        _ => {}
    }
}

/// Renders the outcome (`SearchResponse.toXContent`).
/// `ExceptionsHelper.groupBy`: one entry per index and cause (type and message), the first
/// shard's; `failed` still counts every shard.
pub fn group_failures(failures: &[Value]) -> Vec<Value> {
    let key = |f: &Value| {
        let s = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);
        let reason = f.get("reason");
        (s(f.get("index")), s(reason.and_then(|r| r.get("type"))), s(reason.and_then(|r| r.get("reason"))))
    };
    let mut seen = Vec::with_capacity(failures.len());
    let mut out = Vec::with_capacity(failures.len());
    for f in failures {
        let k = key(f);
        if !seen.contains(&k) {
            seen.push(k);
            out.push(f.clone());
        }
    }
    out
}

pub fn render(o: &SearchOutcome, req: &SearchRequest) -> Value {
    render_with(o, req, o.hits.clone())
}

/// [`render`] moving the hits out of `o` instead of copying them (a scroll page of 1000 hits
/// with their `_source` trees is a large copy).
pub fn render_take(o: &mut SearchOutcome, req: &SearchRequest) -> Value {
    let hits = std::mem::take(&mut o.hits);
    render_with(o, req, hits)
}

fn render_with(o: &SearchOutcome, req: &SearchRequest, top_hits: Vec<Value>) -> Value {
    let mut m = Map::new();
    if let Some(s) = &o.scroll_id {
        m.insert("_scroll_id".into(), Value::Str(s.clone()));
    }
    if let Some(p) = &o.pit_id {
        m.insert("pit_id".into(), Value::Str(p.clone()));
    }
    m.insert("took".into(), Value::from(o.took_ms as i64));
    m.insert("timed_out".into(), Value::Bool(o.timed_out));
    if let Some(t) = o.terminated_early {
        m.insert("terminated_early".into(), Value::Bool(t));
    }
    let mut shards = Map::new();
    shards.insert("total".into(), Value::from(o.shards_total as i64));
    shards.insert("successful".into(), Value::from((o.shards_total - o.failures.len()) as i64));
    shards.insert("skipped".into(), Value::from(o.shards_skipped as i64));
    shards.insert("failed".into(), Value::from(o.failures.len() as i64));
    if !o.failures.is_empty() {
        shards.insert("failures".into(), Value::Arr(group_failures(&o.failures)));
    }
    m.insert("_shards".into(), Value::Obj(shards));
    if let Some(c) = &o.clusters {
        m.insert("_clusters".into(), c.clone());
    }
    let mut hits = Map::new();
    if let Some((total, rel)) = o.total {
        if req.total_as_int {
            hits.insert("total".into(), Value::from(total as i64));
        } else {
            hits.insert(
                "total".into(),
                obj! { "value" => total as i64, "relation" => if rel == TotalRelation::EqualTo { "eq" } else { "gte" } },
            );
        }
    } else if req.total_as_int {
        hits.insert("total".into(), Value::from(-1i64));
    }
    hits.insert(
        "max_score".into(),
        match o.max_score {
            Some(s) if !s.is_nan() => Value::Num(Number::Float(crate::fetch::java_float_as_double(s))),
            _ => Value::Null,
        },
    );
    let unassigned_seq_no = req.source.seq_no_primary_term == Some(true);
    let mut top = top_hits;
    if unassigned_seq_no {
        for h in &mut top {
            if let Value::Obj(hm) = h
                && let Some(ih) = hm.get_mut("inner_hits")
            {
                mark_unassigned_seq_no(ih);
            }
        }
    }
    hits.insert("hits".into(), Value::Arr(top));
    m.insert("hits".into(), Value::Obj(hits));
    // `"aggs": {}` builds no aggregations, and Elasticsearch then prints none.
    if let Some(a) = &o.aggs
        && !matches!(a, Value::Obj(x) if x.is_empty())
    {
        let mut a = a.clone();
        if unassigned_seq_no {
            mark_unassigned_seq_no(&mut a);
        }
        m.insert("aggregations".into(), a);
    }
    // No suggester (`"suggest": {}` or only a global `text`): Elasticsearch prints nothing.
    if let Some(s) = &o.suggest
        && !matches!(s, Value::Obj(x) if x.is_empty())
    {
        let mut s = s.clone();
        // `Suggest` sorts its suggestions by name (`String.compareTo`), in both dialects.
        if let Value::Obj(sm) = &mut s {
            sm.sort_by(|a, _, b, _| a.encode_utf16().cmp(b.encode_utf16()));
        }
        m.insert("suggest".into(), s);
    }
    if let Some(p) = &o.profile {
        m.insert("profile".into(), p.clone());
    }
    Value::Obj(m)
}

/// `GET/POST /_search`, `/{index}/_search`.
pub async fn search(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let mut body = ctx.body()?;
    // OpenSearch: the search pipeline the request names, defines inline or its index defaults to.
    let pipeline = if ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch {
        let indices = ctx.param("index").map(resolve::split_expressions).unwrap_or_default();
        crate::opensearch::search_pipeline::resolve(ctx, &mut body, &indices)?
    } else {
        None
    };
    let mut req = parse_request(ctx, body.as_ref(), ctx.raw_json())?;
    crate::xpack::entsearch::query_rules::check_rulesets(ctx, body.as_ref())?;
    let phase_took = crate::opensearch::phase_took_requested(ctx)?;
    // OpenSearch's `verbose_pipeline` (the URL parameter wins over the body's).
    let verbose = match ctx.query("verbose_pipeline") {
        Some(_) => ctx.bool_param("verbose_pipeline", false)?,
        None => req.source.verbose_pipeline,
    };
    if verbose && pipeline.is_none() {
        return Err(EsError::illegal_argument("The 'verbose pipeline' option requires a search pipeline to be defined."));
    }
    let Some(pipeline) = pipeline else {
        // Hits carry their stored `_source` bytes as they are when the page goes straight out
        // (`json::raw`).
        let raw = ctx.cluster.dialect == crate::cluster::Dialect::Elasticsearch && ctx.query("filter_path").is_none();
        let mut out = crate::json::raw::with_enabled(raw, || run(ctx, req.clone()))?;
        let mut v = render_take(&mut out, &req);
        crate::opensearch::add_reduce_phases(ctx, &out, &req, &mut v)?;
        crate::opensearch::add_search_extras(&out, &req, phase_took, &mut v);
        return Reply::ok(v);
    };
    let mut rc = crate::opensearch::search_pipeline::RunContext::default();
    rc.verbose = verbose;
    pipeline.transform_request(&mut req, &mut rc)?;
    // The pipeline's phase-results processor fuses a top-level `hybrid` query's results.
    if let (Some(phase), Some(h)) = (pipeline.hybrid_phase(), req.source.query.as_mut().and_then(crate::opensearch::hybrid::top_level_mut)) {
        h.phase = Some(phase);
    }
    let out = run(ctx, req.clone())?;
    let mut v = render(&out, &req);
    crate::opensearch::add_reduce_phases(ctx, &out, &req, &mut v)?;
    pipeline.transform_response(&mut v, &mut rc)?;
    crate::opensearch::add_search_extras(&out, &req, phase_took, &mut v);
    Reply::ok(v)
}

/// `SearchRequest.rewrite`: a sorted point-in-time search gets a `_shard_doc` tiebreaker when
/// its sort doesn't end with one, unless `search_after` was given without a value for it.
fn add_pit_tiebreaker(src: &mut SearchSource) {
    if src.sort.is_empty() || src.search_after.as_ref().is_some_and(|a| a.len() != src.sort.len() + 1) {
        return;
    }
    if matches!(src.sort.last(), Some(Sort::Field { field, .. }) if field == "_shard_doc") {
        return;
    }
    src.sort.push(Sort::Field { field: "_shard_doc".into(), order: None, missing: None, unmapped_type: Some("long".into()), mode: None, numeric_type: None, format: None, nested: None });
}

/// Runs a parsed search request: plain, the first page of a scroll, or on a point in time.
pub fn run(ctx: &Ctx, mut req: SearchRequest) -> EsResult<SearchOutcome> {
    // OpenSearch: `_shard_doc` only breaks ties between the shards of a point in time.
    if ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch
        && req.source.pit.is_none()
        && req.source.sort.iter().any(|s| matches!(s, Sort::Field { field, .. } if field == "_shard_doc"))
    {
        return Err(EsError::action_request_validation(&["_shard_doc is only supported with point-in-time (PIT). Add a PIT or remove _shard_doc.".into()]));
    }
    if req.source.knn.iter().any(|m| m.contains_key("query_vector_builder")) || req.source.query.as_ref().is_some_and(knn::has_query_lookups) {
        knn::resolve_lookups(ctx, &mut req.source)?;
    }
    if let Some(q) = &req.source.query
        && crate::query::contains_query(q, "percolate")
    {
        percolate::check_indexed_documents(ctx, q)?;
    }
    if let Some(pit) = req.source.pit.clone() {
        if !req.indices.is_empty() {
            let msg = match ctx.cluster.dialect {
                crate::cluster::Dialect::OpenSearch => "[indices] cannot be used with point in time",
                crate::cluster::Dialect::Elasticsearch => "[indices] cannot be used with point in time. Do not specify any index with point in time.",
            };
            return Err(EsError::action_request_validation(&[msg.into()]));
        }
        if req.scroll.is_some() {
            return Err(EsError::action_request_validation(&["using [point in time] is not allowed in a scroll context".into()]));
        }
        let targets = crate::opensearch::slice_targets(ctx, &req, contexts::pit_targets(ctx, &pit)?);
        add_pit_tiebreaker(&mut req.source);
        let errs = validate(&req);
        if !errs.is_empty() {
            return Err(EsError::action_request_validation(&errs));
        }
        // Elasticsearch's can-match phase runs on point-in-time searches too.
        let total = targets.len();
        let (targets, skipped) = if ctx.cluster.dialect == crate::cluster::Dialect::Elasticsearch {
            crate::opensearch::can_match::filter(ctx, &req, targets)?
        } else {
            (targets, 0)
        };
        let mut out = if targets.is_empty() && skipped > 0 {
            crate::opensearch::can_match::all_skipped(total)
        } else {
            let mut out = execute_after_can_match(ctx.cluster, &req, &targets, skipped)?;
            out.shards_total = total;
            out.shards_skipped += skipped;
            out
        };
        out.pit_id = Some(pit.id.clone());
        return Ok(out);
    }
    {
        let st = ctx.state();
        if let Ok(names) = resolve::resolve(&st, &req.indices, &req.opts, req.now_ms) {
            crate::api::warn_system_access(ctx, &names);
        }
    }
    if req.scroll.is_some() {
        if req.source.track_total_hits.is_none() {
            req.source.track_total_hits = Some(TrackTotalHits::Accurate);
        }
        let st = ctx.state();
        let errs = validate(&req);
        if !errs.is_empty() {
            return Err(EsError::action_request_validation(&errs));
        }
        // OpenSearch leaves the shards outside a slice out of the request.
        let targets = crate::opensearch::slice_targets(ctx, &req, shard_targets(ctx.cluster, &st, &req)?);
        drop(st);
        let total = targets.len();
        // Elasticsearch pre-filters scrolls like any search; skipped shards get no scroll
        // context (when every shard is skipped, the scroll id holds none).
        let (mut targets, skipped) = if ctx.cluster.dialect == crate::cluster::Dialect::Elasticsearch {
            crate::opensearch::can_match::filter(ctx, &req, targets)?
        } else {
            (targets, 0)
        };
        for t in targets.iter_mut() {
            t.searcher = Some(t.searcher()?);
        }
        let mut out = if targets.is_empty() && skipped > 0 {
            crate::opensearch::can_match::all_skipped(total)
        } else {
            let mut out = execute_after_can_match(ctx.cluster, &req, &targets, skipped)?;
            out.shards_total = total;
            out.shards_skipped += skipped;
            out
        };
        let n = out.hits.len();
        contexts::start_scroll(ctx, &req, targets, &mut out, n)?;
        return Ok(out);
    }
    // The can-match phase may skip shards before the query phase.
    let st = ctx.state();
    let errs = validate(&req);
    if !errs.is_empty() {
        return Err(EsError::action_request_validation(&errs));
    }
    let ccs = if ctx.cluster.dialect == crate::cluster::Dialect::Elasticsearch { crate::remote::plan(ctx.cluster, &st, &req.indices)? } else { None };
    let targets = match &ccs {
        None => {
            crate::meta::resolve::reject_remote(&req.indices)?;
            shard_targets(ctx.cluster, &st, &req)?
        }
        Some(plan) => ccs_targets(ctx.cluster, &st, &req, plan)?,
    };
    drop(st);
    let total = targets.len();
    let per_cluster: Vec<(Option<String>, usize)> = match &ccs {
        Some(plan) => {
            let mut v: Vec<(Option<String>, usize)> = Vec::new();
            for t in &targets {
                let c = t.idx.name.split_once(':').map(|(a, _)| a.to_string()).filter(|a| plan.remote.iter().any(|(r, _)| r == a));
                match v.iter_mut().find(|(k, _)| *k == c) {
                    Some(e) => e.1 += 1,
                    None => v.push((c, 1)),
                }
            }
            v
        }
        None => Vec::new(),
    };
    let (targets, skipped) = crate::opensearch::can_match::filter(ctx, &req, targets)?;
    let mut out = if targets.is_empty() && skipped > 0 {
        crate::opensearch::can_match::all_skipped(total)
    } else {
        note_fielddata(&req.source, &targets, id_fielddata::enabled(&ctx.state()));
        let mut out = execute_after_can_match(ctx.cluster, &req, &targets, skipped)?;
        out.shards_total = total;
        out.shards_skipped += skipped;
        out
    };
    if let Some(plan) = &ccs {
        let mrt = ctx.query("ccs_minimize_roundtrips").is_none_or(|v| v != "false");
        ctx.cluster.ccs.record(plan, out.took_ms, out.failures.is_empty(), mrt);
        out.clusters = Some(clusters_section(plan, &per_cluster, &out));
    }
    Ok(out)
}

/// The shards a cross-cluster search runs on: the local expressions' and, for each loopback
/// remote, its expressions' shards with indices named `alias:index`.
fn ccs_targets(cluster: &Cluster, st: &ClusterState, req: &SearchRequest, plan: &crate::remote::Plan) -> EsResult<Vec<Target>> {
    let mut out = Vec::new();
    if let Some(local) = &plan.local {
        let mut r = req.clone();
        r.indices = local.clone();
        out.extend(shard_targets(cluster, st, &r)?);
    }
    for (alias, exprs) in &plan.remote {
        let mut r = req.clone();
        r.indices = exprs.clone();
        for mut t in shard_targets(cluster, st, &r)? {
            let mut idx = (*t.idx).clone();
            idx.name = format!("{alias}:{}", idx.name);
            t.idx = Arc::new(idx);
            out.push(t);
        }
    }
    Ok(out)
}

/// `SearchResponse.Clusters`: per cluster, its status, expressions, took and shards
/// (`shards`: how many of the search's shards each cluster had, `None` for the local one).
fn clusters_section(plan: &crate::remote::Plan, shards: &[(Option<String>, usize)], out: &SearchOutcome) -> Value {
    let cluster_of = |index: &str| -> Option<String> { index.split_once(':').map(|(a, _)| a.to_string()).filter(|a| plan.remote.iter().any(|(r, _)| r == a)) };
    let failed_in = |c: &Option<String>| out.failures.iter().filter(|f| &cluster_of(f.get("index").and_then(Value::as_str).unwrap_or("")) == c).count() as i64;
    let total_in = |c: &Option<String>| shards.iter().filter(|(k, _)| k == c).map(|(_, n)| *n as i64).sum::<i64>();
    let entry = |c: Option<String>, indices: String| {
        let (total, failed) = (total_in(&c), failed_in(&c));
        obj! {
            "status" => if failed > 0 { "partial" } else { "successful" },
            "indices" => indices,
            "took" => out.took_ms as i64,
            "timed_out" => out.timed_out,
            "_shards" => obj! { "total" => total, "successful" => total - failed, "skipped" => 0, "failed" => failed },
        }
    };
    let mut details = Map::new();
    let (mut successful, mut partial) = (0i64, 0i64);
    let mut add = |name: String, v: Value| {
        if v.get("status").and_then(Value::as_str) == Some("partial") {
            partial += 1;
        } else {
            successful += 1;
        }
        details.insert(name, v);
    };
    if let Some(local) = &plan.local {
        add("(local)".into(), entry(None, local.join(",")));
    }
    for (alias, exprs) in &plan.remote {
        add(alias.clone(), entry(Some(alias.clone()), exprs.join(",")));
    }
    for alias in &plan.skipped {
        details.insert(alias.clone(), obj! { "status" => "skipped", "indices" => "", "timed_out" => false });
    }
    obj! {
        "total" => successful + partial + plan.skipped.len() as i64,
        "successful" => successful,
        "skipped" => plan.skipped.len() as i64,
        "running" => 0,
        "partial" => partial,
        "failed" => 0,
        "details" => Value::Obj(details),
    }
}

/// `GET/POST /_msearch`: NDJSON header/body pairs, one response (or error) each.
/// `MultiSearchRequest.maxConcurrentSearchRequests` (also `_msearch/template`'s).
pub fn check_max_concurrent_searches(ctx: &Ctx<'_>) -> EsResult<()> {
    if ctx.has_query("max_concurrent_searches") && ctx.int_param("max_concurrent_searches", 0)? < 1 {
        return Err(EsError::illegal_argument("maxConcurrentSearchRequests must be positive"));
    }
    Ok(())
}

pub async fn msearch(ctx: &Ctx<'_>) -> EsResult<Reply> {
    check_max_concurrent_searches(ctx)?;
    let started = std::time::Instant::now();
    let data = ctx.body.clone();
    let mut lines: Vec<&[u8]> = data.split(|b| *b == b'\n').collect();
    if lines.last().is_some_and(|l| l.iter().all(|b| b.is_ascii_whitespace())) {
        lines.pop();
    } else if !data.is_empty() {
        return Err(EsError::illegal_argument("The msearch request must be terminated by a newline [\\n]"));
    }
    let mut responses = Vec::new();
    let mut i = 0;
    let os = ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch;
    // OpenSearch still skips an empty first line (deprecated), and checks every header key.
    if os && data.first() == Some(&b'\n') {
        i = 1;
        ctx.warn_dialect(
            None,
            Some("support for empty first line before any action metadata in msearch API is deprecated and will be removed in the next major version".into()),
        );
    }
    let default_index = ctx.path_param("index").map(str::to_string);
    let mut pairs: Vec<(Map, Option<Value>, Vec<u8>)> = Vec::new();
    while i < lines.len() {
        let header = lines[i];
        i += 1;
        let hv = match crate::json::parse(header)? {
            Some(Value::Obj(m)) => m,
            Some(_) => return Err(EsError::illegal_argument("msearch header must be an object")),
            None => Map::new(),
        };
        if os && let Some(k) = hv.keys().find(|k| !crate::opensearch::MSEARCH_HEADER_KEYS.contains(&k.as_str())) {
            return Err(EsError::illegal_argument(format!("key [{k}] is not supported in the metadata section")));
        }
        let Some(body) = lines.get(i) else { break };
        i += 1;
        let bv = crate::json::parse(body)?;
        pairs.push((hv, bv, body.to_vec()));
    }
    if pairs.is_empty() {
        return Err(EsError::action_request_validation(&["no requests added".into()]));
    }
    // `RestMultiSearchAction` parses every request first: a malformed one fails them all, and
    // so does any request's validation (`MultiSearchRequest.validate`).
    let mut reqs = Vec::with_capacity(pairs.len());
    for (h, b, raw) in pairs {
        let mut req = parse_request(ctx, b.as_ref(), Some(&raw))?;
        if let Some(a) = &req.source.aggs {
            aggs::parse_error(a, req.source.aggs_raw.as_deref())?;
        }
        req.indices = match h.get("index") {
            Some(Value::Str(s)) => resolve::split_expressions(s),
            Some(Value::Arr(a)) => a.iter().map(|v| v.to_es_string()).collect(),
            _ => default_index.as_deref().map(resolve::split_expressions).unwrap_or_default(),
        };
        let get = |k: &str| h.get(k).map(|v| v.to_es_string());
        req.opts = IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED.with_params(|k| get(k).or_else(|| ctx.query(k).map(str::to_string)))?;
        if let Some(r) = get("routing") {
            req.routing = Some(r);
        }
        if let Some(p) = get("preference") {
            req.preference = Some(p);
        }
        if let Some(Value::Str(st)) = h.get("search_type") {
            req.dfs = st == "dfs_query_then_fetch";
        }
        reqs.push(req);
    }
    let mut errs = Vec::new();
    for req in &reqs {
        errs.extend(validate(req));
        if let Some(a) = &req.source.aggs {
            errs.extend(aggs::validation_errors(a));
        }
        if req.source.pit.is_some() && !req.indices.is_empty() {
            errs.push("[indices] cannot be used with point in time. Do not specify any index with point in time.".to_string());
        }
    }
    if !errs.is_empty() {
        return Err(EsError::action_request_validation(&errs));
    }
    // Buffered items are charged to the request breaker until the response is sent
    // (Elasticsearch 9.5); a trip fails the whole request with that 429. OpenSearch has no
    // such accounting.
    let mut held = (ctx.cluster.dialect != crate::cluster::Dialect::OpenSearch)
        .then(|| crate::breaker::Reservation::new(ctx.cluster.breakers.request(ctx.cluster, &ctx.state())));
    for req in reqs {
        let r = run(ctx, req.clone()).map(|out| {
            // Elasticsearch 9.5.5's `_msearch` items don't mark the `top_hits` / `inner_hits`
            // hits of a `seq_no_primary_term` search as unassigned, as `_search` does.
            let mut rreq = req.clone();
            rreq.source.seq_no_primary_term = None;
            let mut v = render(&out, &rreq);
            if let Value::Obj(m) = &mut v {
                m.insert("status".into(), Value::from(200i64));
            }
            v
        });
        let item = match r {
            Ok(v) => {
                if let Some(h) = held.as_mut() {
                    h.add(crate::breaker::estimate_search_response(&v), "<msearch_response>")?;
                }
                v
            }
            Err(e) => {
                let error = crate::opensearch::item_error(ctx, &e, true);
                if let Some(h) = held.as_mut() {
                    h.add(crate::breaker::estimate_failure_item(&error), "<msearch_failure>")?;
                }
                obj! { "error" => error, "status" => e.status as i64 }
            }
        };
        responses.push(item);
    }
    drop(held);
    Reply::ok(obj! { "took" => started.elapsed().as_millis() as i64, "responses" => Value::Arr(responses) })
}

/// `RestActions.getQueryContent` (`_count`, `_explain`): an `XContentParseException` from the
/// query becomes a `ParsingException` "Failed to parse" at the same location, caused by it.
fn query_content_error(e: EsError) -> EsError {
    let reason = e.reason();
    let lc = match e.kind.as_ref() {
        "x_content_parse_exception" => reason.strip_prefix('[').and_then(|r| r.split_once(']')).map(|(lc, _)| lc),
        // `{}` as a query: the parser stands on the empty object's end, where the message says.
        "illegal_argument_exception" => reason.strip_prefix("query malformed, empty clause found at [").and_then(|r| r.strip_suffix(']')),
        _ => None,
    };
    let loc = lc.and_then(|lc| lc.split_once(':')).and_then(|(l, c)| Some((l.parse::<usize>().ok()?, c.parse::<usize>().ok()?)));
    match loc {
        Some((line, col)) => EsError::parsing(line, col, "Failed to parse").caused_by(e),
        None => e,
    }
}

/// `GET/POST /{index}/_explain/{id}`.
pub fn explain(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body()?;
    let expr = ctx.path_param("index").unwrap_or_default().to_string();
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    let mut src = SearchSource::default();
    if let Some(Value::Obj(m)) = &body {
        for (k, v) in m {
            match k.as_str() {
                "query" => src.query = Some(crate::query::parse_query(v, ctx.raw_json(), &[crate::json::locate::Seg::Key("query")]).map_err(query_content_error)?.0),
                other => return Err(EsError::parse(format!("request does not support [{other}]"))),
            }
        }
    }
    let mut req = parse_request(ctx, None, None)?;
    if src.query.is_some() {
        req.source.query = src.query;
    }
    if req.source.query.is_none() {
        return Err(EsError::action_request_validation(&["query is missing".into()]));
    }
    let idx = crate::api::document::single_index(ctx, &expr)?;
    let st = ctx.state();
    let engine = ctx.cluster.engine.index(ctx.cluster, &st, &idx)?;
    let routing = ctx.query("routing").map(str::to_string);
    crate::write::check_routing(&idx, &id, routing.as_deref())?;
    let shard = engine.shard(crate::meta::routing::shard_for(&idx, &id, routing.as_deref())).clone();
    shard.before_search(IndexEngine::refresh_interval(&idx))?;
    let searcher = shard.searcher();
    let table = engine.table();
    let qctx = QueryCtx::new(ctx.cluster, &st, &idx, &table, &searcher, ctx.now_ms);
    let t = Target { idx: idx.clone(), engine: engine.clone(), shard: shard.clone(), alias_filter: None, boost: 1.0, searcher: Some(searcher.clone()) };
    // `TransportExplainAction.shardOperation` looks the document up before it builds the
    // query: a missing document answers 404 whatever the query.
    let doc = find_doc(&searcher, &id);
    let mut m = Map::new();
    m.insert("_index".into(), Value::Str(idx.name.clone()));
    m.insert("_id".into(), Value::Str(id.clone()));
    let Some(doc) = doc else {
        m.insert("matched".into(), Value::Bool(false));
        return Ok(Reply::Body(404, Value::Obj(m)));
    };
    let q = shard_query(&qctx, &t, &req.source).map_err(|e| query::shard_exception(&idx, e))?;
    let e = searcher.explain(&q, doc).map_err(crate::engine::convert::index_error)?;
    m.insert("matched".into(), Value::Bool(e.is_match()));
    m.insert("explanation".into(), explanation_json(&e));
    let fs = crate::fetch::FetchSource::from_params(ctx.query("_source"), ctx.query("_source_includes"), ctx.query("_source_excludes"))?;
    let sf = ctx.query("stored_fields").map(list);
    if fs.is_some() || sf.is_some() {
        let stored = searcher.stored(doc).map_err(crate::engine::convert::index_error)?;
        let mut g = Map::new();
        let fs2 = match (&fs, &sf) {
            (Some(f), _) => f.clone(),
            (None, Some(s)) => crate::fetch::FetchSource { fetch: s.iter().any(|x| x == "_source"), ..Default::default() },
            _ => crate::fetch::FetchSource::default(),
        };
        g.insert("found".into(), Value::Bool(true));
        let view = crate::fetch::SourceView { idx: &idx, table: &table, force_synthetic: false, search: true };
        if let Some(src) = crate::fetch::render_source_view(&stored, &fs2, &view)? {
            g.insert("_source".into(), src);
        }
        if let Some(s) = &sf {
            let f = crate::fetch::stored_fields(&table, &stored, s);
            if !f.is_empty() {
                g.insert("fields".into(), Value::Obj(f));
            }
        }
        m.insert("get".into(), Value::Obj(g));
    }
    Reply::ok(Value::Obj(m))
}

/// The global doc id of `id` in a searcher (live docs only).
/// Completion suggestions are fetched like hits, so the fetch phase's highlighting runs on
/// them too, with the search query of the shard that holds the suggested document.
/// The fetch sub-phases completion options go through with the search's query (they are
/// fetched as hits): `highlight`, `explain` (`_shard`, `_node`, `_explanation`) and the
/// query's `inner_hits`.
/// The fetch sub-phases completion options go through with the hits (`FetchPhase` loads them
/// together): highlight, explain (not for a suggest-only search, which has no query), script
/// fields, inner hits.
fn complete_completion_options(cluster: &Cluster, st: &ClusterState, req: &SearchRequest, targets: &[Target], results: &[ShardResult], out: &mut Value, suggest_only: bool) -> EsResult<()> {
    let src = &req.source;
    let Value::Obj(sugs) = out else { return Ok(()) };
    let mut spec: Option<highlight::Spec> = None;
    for entries in sugs.values_mut() {
        let Value::Arr(entries) = entries else { continue };
        for e in entries {
            let Value::Obj(em) = e else { continue };
            let Some(Value::Arr(options)) = em.get_mut("options") else { continue };
            for o in options {
                let Value::Obj(m) = o else { continue };
                let (Some(index), Some(id)) = (m.get("_index").and_then(Value::as_str), m.get("_id").and_then(Value::as_str)) else { continue };
                let found = results.iter().find_map(|r| {
                    let t = &targets[r.target];
                    if t.idx.name != index {
                        return None;
                    }
                    find_doc(&r.searcher, id).map(|d| (r, t, d))
                });
                let Some((r, t, doc)) = found else { continue };
                let table = t.engine.table();
                let stored = r.searcher.stored(doc).map_err(crate::engine::convert::index_error)?;
                let source = crate::fetch::source_bytes(&stored).map(crate::fetch::parse_source).transpose()?;
                let qctx = QueryCtx::new(cluster, st, &t.idx, &table, &r.searcher, req.now_ms);
                if let Some(hl) = &src.highlight {
                    let spec = match &mut spec {
                        Some(s) => s,
                        None => {
                            let mut s = highlight::Spec::parse(hl)?;
                            s.weight_matches = highlight::weight_matches_supported(&req.source);
                            spec.insert(s)
                        }
                    };
                    let id = id.to_string();
                    let h = highlight::highlight_hit(&qctx, spec, &r.query, source.as_ref(), &id, &r.searcher, Some(doc), Some(&stored), Some(&r.match_weights))?;
                    if !h.is_empty() {
                        m.insert("highlight".into(), Value::Obj(h));
                    }
                }
                if src.explain == Some(true) && !suggest_only {
                    let e = r.searcher.explain(&r.query, doc).map_err(crate::engine::convert::index_error)?;
                    let at = m.get_index_of("_index").unwrap_or(0);
                    m.shift_insert(at, "_shard".into(), Value::Str(format!("[{}][{}]", t.idx.name, t.shard.id)));
                    m.shift_insert(at + 1, "_node".into(), Value::Str(cluster.node.id.clone()));
                    m.insert("_explanation".into(), explanation_json(&e));
                }
                if !src.script_fields.is_empty() {
                    let score = m.get("_score").and_then(Value::as_f64).unwrap_or(1.0) as f32;
                    script_fields_of_hit(cluster, st, t, &table, &r.searcher, doc, score, &src.script_fields, m)?;
                }
                if !r.inner_hits.is_empty() {
                    m.insert("inner_hits".into(), inner::render(&qctx, t, r, doc, source.as_ref(), req.total_as_int)?);
                }
            }
        }
    }
    Ok(())
}

pub fn find_doc(s: &Searcher, id: &str) -> Option<u32> {
    let q: QueryRef = arc(search_index::query::TermQuery::new(search_index::schema::ID_FIELD, id.as_bytes().to_vec()));
    let mut c = TopScoreCollector::new(1, 1);
    s.search(&q, &mut c).ok()?;
    c.top_docs().hits.first().map(|h| h.doc)
}

/// `GET/POST /_count`, `/{index}/_count`.
pub fn count(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body()?;
    let mut src = SearchSource::default();
    if let Some(Value::Obj(m)) = &body {
        for (k, v) in m {
            match k.as_str() {
                "query" => src.query = Some(crate::query::parse_query(v, ctx.raw_json(), &[crate::json::locate::Seg::Key("query")]).map_err(query_content_error)?.0),
                // `RestActions.getQueryContent`: located at the field name.
                other => {
                    let (l, c) = ctx.raw_json().and_then(|src| crate::json::locate::key_location(src, &[], other)).unwrap_or((1, 1));
                    return Err(EsError::parsing(l, c, format!("request does not support [{other}]")));
                }
            }
        }
    }
    let mut req = parse_request(ctx, None, None)?;
    if src.query.is_some() {
        req.source.query = src.query;
    }
    if let Some(m) = ctx.query("min_score") {
        // `paramAsFloat`.
        let v = crate::units::parse_java_double(m)
            .ok_or_else(|| EsError::illegal_argument(format!("Failed to parse float parameter [min_score] with value [{m}]")).caused_by(EsError::number_format(m)))?;
        req.source.min_score = Some(v as f32);
    }
    let st = ctx.state();
    let targets = shard_targets(ctx.cluster, &st, &req)?;
    let mut total = 0u64;
    let mut failures = Vec::new();
    let mut terminated_early = false;
    for (i, t) in targets.iter().enumerate() {
        let r = (|| -> EsResult<u64> {
            let searcher = t.searcher()?;
            let table = t.engine.table();
            let qctx = QueryCtx::new(ctx.cluster, &st, &t.idx, &table, &searcher, ctx.now_ms);
            check_inner_hit_names(&req.source)?;
            let q = shard_query(&qctx, t, &req.source).map_err(|e| query::shard_exception(&t.idx, e))?;
            match (req.source.min_score, req.source.terminate_after) {
                (None, None) => searcher.count(&q).map_err(crate::engine::convert::index_error),
                _ => {
                    let c = search_index::collector::top_docs::TotalHitCountCollector::default();
                    let mut terminated = false;
                    let total = run_collect(&searcher, &q, c, None, &req.source, None, &mut terminated, |c| c.total)?;
                    terminated_early |= terminated;
                    Ok(total)
                }
            }
        })();
        match r {
            Ok(n) => total += n,
            Err(e) => failures.push((i, e)),
        }
    }
    if !failures.is_empty() && failures.len() == targets.len() {
        return Err(all_shards_failed(&ctx.cluster.node.id, &targets, &failures, "all shards failed"));
    }
    let mut m = Map::new();
    // `RestCountAction`: reported whenever `terminate_after` is set.
    if req.source.terminate_after.is_some() {
        m.insert("terminated_early".into(), Value::Bool(terminated_early));
    }
    m.insert("count".into(), Value::from(total as i64));
    let ok = targets.len() - failures.len();
    let mut shards = obj! { "total" => targets.len() as i64, "successful" => ok as i64, "skipped" => 0, "failed" => failures.len() as i64 };
    if !failures.is_empty() {
        // As a search reports them: in shard request order, grouped by index and cause.
        let order = shard_request_indices(&targets);
        failures.sort_by_key(|(i, _)| order[*i]);
        let rendered: Vec<Value> = failures.iter().map(|(i, e)| shard_failure(&ctx.cluster.node.id, &targets[*i], e)).collect();
        if let Value::Obj(sm) = &mut shards {
            sm.insert("failures".into(), Value::Arr(group_failures(&rendered)));
        }
    }
    m.insert("_shards".into(), shards);
    Reply::ok(Value::Obj(m))
}
