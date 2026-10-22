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
pub mod aggs;
pub mod contexts;
pub mod dv_distance;
pub mod dv_terms;
pub mod extra;
pub mod fields;
pub mod geo;
pub mod highlight;
pub mod hits;
pub mod inner;
pub mod fusion;
pub mod join;
pub mod knn;
pub mod retriever;
pub mod runtime;
pub mod misc;
pub mod painless_execute;
pub mod percolate;
pub mod profile;
pub mod query;
pub mod query_string;
pub mod range;
pub mod scripts;
pub mod suggest;
pub mod sort;
pub mod text;

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
        return Err(EsError::illegal_argument(format!("[from] parameter cannot be negative but was [{f}]")));
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
    if !s.rescore.is_empty() && !s.sort.is_empty() && !matches!(s.sort.as_slice(), [Sort::Score { order: SortOrder::Desc }]) {
        errs.push("Cannot use [sort] option in conjunction with [rescore].".into());
    }
    if s.search_after.is_some() && s.from.is_some_and(|f| f > 0) {
        errs.push("[from] parameter must be set to 0 when [search_after] is used".into());
    }
    if s.slice.is_some() && req.scroll.is_none() && s.pit.is_none() {
        errs.push("[slice] can only be used with [scroll] or [point-in-time] requests".into());
    }
    errs
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
fn check_window(idx: &IndexMeta, from: i64, size: i64, scroll: bool) -> EsResult<()> {
    let max = idx.settings.get("index.max_result_window").and_then(|v| v.parse::<i64>().ok()).unwrap_or(10_000);
    if from + size > max {
        let what = if scroll { "Batch size is too large, size must be less than or equal to" } else { "Result window is too large, from + size must be less than or equal to" };
        return Err(EsError::illegal_argument(if scroll {
            format!("{what}: [{max}] but was [{size}]. Scroll batch sizes cost as much memory as result windows so they are controlled by the [index.max_result_window] index level setting.")
        } else {
            format!("{what}: [{max}] but was [{}]. See the scroll api for a more efficient way to request large data sets. This limit can be set by changing the [index.max_result_window] index level setting.", from + size)
        }));
    }
    Ok(())
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
    // time spread the slices over the shards.
    let (id, max) = if shards == 1 || scroll {
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
        None if !scroll => search_index::query::SliceBy::DocId,
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
fn encode_uid(id: &str) -> Vec<u8> {
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
fn shard_query(qctx: &QueryCtx, t: &Target, src: &SearchSource) -> EsResult<QueryRef> {
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
    let mut filters: Vec<QueryRef> = Vec::new();
    if let Some(af) = &t.alias_filter {
        filters.push(qctx.to_filter(af)?);
    }
    if qctx.table.nested_paths().next().is_some() {
        filters.push(search_index::query::nested::root_filter());
    }
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

fn total_threshold(src: &SearchSource) -> Option<u64> {
    match src.track_total_hits {
        None => Some(10_000),
        Some(TrackTotalHits::Accurate) => Some(u64::MAX),
        Some(TrackTotalHits::Disabled) => None,
        Some(TrackTotalHits::UpTo(n)) => Some(n.max(0) as u64),
    }
}

/// Runs the query phase on one shard.
#[allow(clippy::too_many_arguments)]
///
/// `window` is the shard's request-window failure (`DefaultSearchContext.preProcess`), raised
/// once the query, aggregations and highlight queries are built, as Elasticsearch orders them.
pub fn query_phase(cluster: &Cluster, st: &ClusterState, req: &SearchRequest, t: &Target, ti: usize, shards: usize, n: usize, after: Option<&[Value]>, aggs: Option<&search_aggs::AggRequest>, knn: &[QueryRef], window: Option<EsError>) -> EsResult<ShardResult> {
    let src = &req.source;
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
    let q = shard_query_knn(&qctx, t, src, knn).map_err(|e| query::shard_exception(&t.idx, e))?;
    let q = match &src.slice {
        Some(sl) => match slice_filter(&qctx, sl, ti, shards, req.scroll.is_some())? {
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
    let cache_key = cacheable(req, t).then(|| request_cache_key(req, t));
    if let Some(key) = &cache_key {
        let cached = t.shard.request_cache.lock().get(key).cloned();
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
            });
        }
        t.shard.counters.request_cache_miss.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let built = sort::build(&qctx, &src.sort)?;
    let post: Option<QueryRef> = match &src.post_filter {
        Some(pf) => Some(qctx.to_filter(pf).map_err(|e| query::shard_exception(&t.idx, e))?),
        None => None,
    };
    let threshold = total_threshold(src);
    let wanted = n;
    // Rescoring needs each shard's top `window_size` hits.
    let n = src.rescore.iter().map(|r| r.window_size.unwrap_or(10).max(0) as usize).fold(n, usize::max);
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
            let lookup = aggs::Lookup::new(cluster, st, t, &table, &searcher, req.now_ms).with_parent_fields(&src.fields);
            let max_buckets = st.metadata.cluster_setting("search.max_buckets").and_then(|v| v.parse::<usize>().ok());
            let opts = search_aggs::ShardOptions {
                max_buckets,
                // Top-level aggregations may answer from segment statistics only when the query
                // matches every live doc (`match_all`, nothing else filtering).
                matches_all: q.as_any().is::<MatchAllQuery>() && post.is_none() && src.min_score.is_none(),
                profiler: profiler.as_ref().map(|_| search_aggs::profile::Profiler::new()),
                ..Default::default()
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
    let collapse = match &src.collapse {
        Some(c) => Some(collapse_field(&qctx, &c.field)?),
        None => None,
    };
    let mut pre_rescored: Option<Vec<Vec<u32>>> = None;
    let (hits, total, relation, max_score) = if let Some(cf) = collapse.filter(|_| src.rescore_before_collapse && built.by_score) {
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
        (hits, total, relation, td.max_score)
    } else if built.by_score && profiler.is_none() && agg_collector.is_none() && post.is_none() && src.min_score.is_none() && src.terminate_after.is_none() && after.is_none() {
        // The concurrent fast path.
        let m = search_index::collector::compose::TopScoreManager { n, threshold: threshold.unwrap_or(0).max(n as u64), after: None, min_score: None };
        let td = searcher.search_concurrent(&q, &m, &Default::default()).map_err(crate::engine::convert::index_error)?;
        let max = td.max_score();
        let hits: Vec<ShardHit> = td.hits.iter().map(|h| ShardHit { doc: h.doc, score: h.score, sort: Vec::new(), group: None }).collect();
        (hits, td.total_hits, td.relation, max)
    } else if built.by_score {
        // A score-only sort is no sort (`SearchAfterBuilder.buildFieldDoc`).
        if after.is_some() {
            return Err(EsError::illegal_argument("Sort must contain at least one field."));
        }
        let top = TopScoreCollector::new(n, threshold.unwrap_or(0).max(n as u64));
        let td = run_collect(&searcher, &q, top, post.as_ref(), src, agg_collector.as_mut(), &mut terminated_early, |c| c.top_docs())?;
        let max = td.max_score();
        let hits: Vec<ShardHit> = td.hits.iter().map(|h| ShardHit { doc: h.doc, score: h.score, sort: Vec::new(), group: None }).collect();
        (hits, td.total_hits, td.relation, max)
    } else {
        let mut top = TopFieldCollector::new(built.keys.clone(), n, threshold.unwrap_or(0).max(n as u64), track_scores || src.min_score.is_some()).with_searcher(&searcher);
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
                    _ => None,
                };
                vals.push(sort::parse_after(v, k, f, req.now_ms, pos, field.unwrap_or("null"))?);
            }
            top = top.with_after(vals, None);
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
    let (hits, max_score, rescored) = match pre_rescored {
        Some(sets) => (hits, max_score, sets),
        None if !src.rescore.is_empty() && built.by_score => rescore(&qctx, &searcher, hits, src)?,
        None => (hits, max_score, Vec::new()),
    };
    c.query_time_nanos.fetch_add(started.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
    let aggs_out = match agg_collector {
        Some(c) => Some(c.finish().map_err(aggs::agg_error)?),
        None => None,
    };
    let named = qctx.named.take();
    let inner = qctx.inner_hits.take();
    let profile = profiler.map(|p| {
        let nanos = started.elapsed().as_nanos() as u64;
        let agg_names: Vec<String> = src.aggs.as_ref().map(|a| a.keys().cloned().collect()).unwrap_or_default();
        let collector = if built.by_score { "TopScoreDocCollector" } else { "TopFieldCollector" };
        let field_type = |f: &str| table.get(f).map(|x| x.ty.clone());
        profile::ShardProfile {
            searches: vec![profile::search_entry(&p, collector, nanos, &agg_names)],
            aggregations: match &agg_profiler {
                Some(ap) => match aggs::from_json(&ap.to_json()) {
                    Value::Arr(a) => a,
                    _ => Vec::new(),
                },
                None => src.aggs.as_ref().map(|a| profile::aggregations(a, &field_type, nanos, total)).unwrap_or_default(),
            },
            dfs: None,
        }
    });
    if let Some(key) = cache_key {
        let entry = CachedShard { total, relation, max_score, aggs: aggs_out.clone(), terminated_early };
        let mut cache = t.shard.request_cache.lock();
        if cache.len() >= 1024 {
            cache.clear();
            t.shard.counters.request_cache_evictions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        t.shard.counters.request_cache_bytes.fetch_add(key.len() as u64 + 256, std::sync::atomic::Ordering::Relaxed);
        cache.insert(key, Arc::new(entry));
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
    if req.request_cache == Some(false) || req.scroll.is_some() || req.dfs || src.profile || src.size.unwrap_or(10) != 0 || !src.knn.is_empty() {
        return false;
    }
    if req.request_cache.is_none() && t.idx.settings.get("index.requests.cache.enable") == Some("false") {
        return false;
    }
    let text = format!("{src:?}");
    !(text.contains("now") || text.contains("Script") || text.contains("random"))
}

fn request_cache_key(req: &SearchRequest, t: &Target) -> String {
    let mut k = format!("{:?}|{:?}|{}", req.source, t.alias_filter, req.typed_keys);
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
fn rescore(qctx: &QueryCtx, searcher: &Searcher, mut hits: Vec<ShardHit>, src: &SearchSource) -> EsResult<(Vec<ShardHit>, Option<f32>, Vec<Vec<u32>>)> {
    let mut rescored = Vec::with_capacity(src.rescore.len());
    for r in &src.rescore {
        let window = (r.window_size.unwrap_or(10).max(0) as usize).min(hits.len());
        let mut docs: Vec<u32> = hits[..window].iter().map(|h| h.doc).collect();
        docs.sort_unstable();
        rescored.push(docs);
        if let Some(script) = &r.script {
            if window == 0 {
                continue;
            }
            let p = extra::painless(qctx, script, search_painless::Context::Score).map_err(|e| query::shard_exception(qctx.idx, e))?;
            for h in hits[..window].iter_mut() {
                h.score = script_rescore_value(&p, searcher, h.doc, h.score)?;
            }
            hits[..window].sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc.cmp(&b.doc)));
            continue;
        }
        for h in hits[window..].iter_mut() {
            h.score *= r.query_weight;
        }
        if window == 0 {
            continue;
        }
        let rq = qctx.to_query(&r.query).map_err(|e| query::shard_exception(qctx.idx, e))?;
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
        hits[..window].sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc.cmp(&b.doc)));
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
    // Layers: post_filter (top only) → min_score (everything) → terminate_after (everything).
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
        let mut parts: Vec<&mut dyn Collector> = vec![top_ref];
        if let Some(a) = aggs {
            parts.push(a);
        }
        let mut multi = MultiCollector::new(parts);
        let mut ms;
        let inner: &mut dyn Collector = match src.min_score {
            Some(m) => {
                ms = MinScore::new(ByRef(&mut multi), m);
                &mut ms
            }
            None => &mut multi,
        };
        match src.terminate_after {
            Some(t) if t > 0 => {
                let mut ta = TerminateAfter::new(ByRef(inner), t as u64);
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
        Err(e) => return Err(ie(e)),
    }
    Ok(finish(top))
}

/// The collapse field (`CollapseBuilder.build`): keyword or numeric with doc values.
fn collapse_field(qctx: &QueryCtx, name: &str) -> EsResult<search_index::collector::collapse::CollapseField> {
    use crate::engine::fields::Kind;
    let Some(f) = qctx.field(name) else {
        return Err(query::qse(qctx, format!("no mapping found for `{name}` in order to collapse on")));
    };
    if !f.doc_values {
        return Err(query::qse(qctx, format!("cannot collapse on field `{name}` without `doc_values`")));
    }
    Ok(match f.kind {
        Kind::Keyword | Kind::Ip | Kind::Version | Kind::ConstantKeyword | Kind::Wildcard => search_index::collector::collapse::CollapseField::Keyword(f.number),
        k if k.is_numeric() || k.is_date() || k == Kind::Boolean => search_index::collector::collapse::CollapseField::Numeric(f.number),
        _ => return Err(query::qse(qctx, format!("collapse is not supported for the field [{name}] of the type [{}]", f.ty))),
    })
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

fn cmp_hits(keys: &[SortKey], a: (&ShardHit, usize), b: (&ShardHit, usize)) -> std::cmp::Ordering {
    if keys.is_empty() {
        return b.0.score.total_cmp(&a.0.score).then(a.1.cmp(&b.1)).then(a.0.doc.cmp(&b.0.doc));
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
    note_fielddata(&req.source, &targets);
    execute_on(cluster, req, &targets)
}

/// `matched_fields` with `require_field_match: false` (`HighlightBuilder.build`).
fn highlight_shard_error(h: &Map) -> Option<EsError> {
    let global = h.get("require_field_match").and_then(Value::as_bool);
    let fields: Vec<&Value> = match h.get("fields") {
        Some(Value::Obj(m)) => m.values().collect(),
        Some(Value::Arr(a)) => a.iter().filter_map(|x| x.as_object().and_then(|m| m.values().next())).collect(),
        _ => Vec::new(),
    };
    for f in fields {
        let rfm = f.get("require_field_match").and_then(Value::as_bool).or(global).unwrap_or(true);
        if !rfm && f.get("matched_fields").is_some() {
            return Some(EsError::illegal_argument("Matched fields are not supported when [require_field_match] is set to [false]"));
        }
    }
    None
}

/// Marks the text fields whose field data the request's sorts and aggregations load (for the
/// `fielddata` stats).
fn note_fielddata(src: &SearchSource, targets: &[Target]) {
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
    let sorts: Vec<String> = src.sort.iter().filter_map(|s| match s {
        Sort::Field { field, .. } => Some(field.clone()),
        _ => None,
    }).collect();
    let mut aggs = Vec::new();
    if let Some(a) = &src.aggs {
        agg_fields(&Value::Obj(a.clone()), &mut aggs);
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

/// Runs a search on given shard targets (scrolls and PITs pass their snapshots).
pub fn execute_on(cluster: &Cluster, req: &SearchRequest, targets: &[Target]) -> EsResult<SearchOutcome> {
    if let Some(f) = &req.source.fusion {
        return fusion::execute(cluster, req, targets, f);
    }
    let started = std::time::Instant::now();
    let st = cluster.state();
    let src = &req.source;
    let from = src.from.unwrap_or(0).max(0) as usize;
    let size = src.size.unwrap_or(10).max(0) as usize;
    // A `diversify` retriever searches its window, then picks from it.
    let n = match &src.diversify {
        Some(d) => d.rank_window_size.max(0) as usize,
        None => from + size,
    };
    // Highlighting options every shard rejects as it builds its highlighters.
    if let Some(h) = &src.highlight
        && let Some(e) = highlight_shard_error(h)
    {
        let failures: Vec<(usize, EsError)> = (0..targets.len()).map(|i| (i, e.clone())).collect();
        if !failures.is_empty() {
            return Err(all_shards_failed(&cluster.node.id, targets, &failures, "all shards failed"));
        }
    }
    // The result window is checked on each shard (failures come back as shard failures).
    let window: Vec<Option<EsError>> = targets.iter().map(|t| check_window(&t.idx, from as i64, size as i64, req.scroll.is_some()).and_then(|_| check_limits(&t.idx, src)).and_then(|_| check_slices(&t.idx, src, req.scroll.is_some())).err()).collect();
    let agg_req = match &src.aggs {
        Some(a) => {
            // Aggregations over runtime fields run their scripts.
            let any_runtime = src.runtime_mappings.is_some() || targets.iter().any(|t| !t.idx.mapping.runtime.is_empty());
            if any_runtime {
                let mut a = a.clone();
                let def = |name: &str| -> Option<Value> {
                    if targets.iter().any(|t| t.engine.table().get(name).is_some()) && src.runtime_mappings.as_ref().is_none_or(|r| !r.contains_key(name)) {
                        return None;
                    }
                    targets.iter().find_map(|t| runtime::definition(&t.idx.mapping, src.runtime_mappings.as_ref(), name))
                };
                // `lookup` runtime fields have no field data.
                // Every shard fails building the aggregation.
                if let Some(rm) = &src.runtime_mappings
                    && let Err(e) = aggs::check_lookup_fields(&a, rm)
                {
                    let failures: Vec<(usize, EsError)> = (0..targets.len()).map(|i| (i, e.clone())).collect();
                    return Err(all_shards_failed(&cluster.node.id, targets, &failures, "all shards failed"));
                }
                aggs::rewrite_runtime(&mut a, &def);
                Some(aggs::parse(&a)?)
            } else {
                Some(aggs::parse(a)?)
            }
        }
        None => None,
    };
    let after = src.search_after.as_deref();
    let knn_sections = knn::sections(src)?;
    let (knn_queries, knn_profiles) = match &req.fused {
        Some(f) => (f.per_target.clone(), vec![None; targets.len()]),
        None if knn_sections.is_empty() => (vec![Vec::new(); targets.len()], vec![None; targets.len()]),
        None => knn::dfs(cluster, &st, req.now_ms, targets, &knn_sections, src.profile)?,
    };
    let mut results: Vec<ShardResult> = Vec::with_capacity(targets.len());
    let mut failures: Vec<(usize, EsError)> = Vec::new();
    for (i, t) in targets.iter().enumerate() {
        match query_phase(cluster, &st, req, t, i, targets.len(), n, after, agg_req.as_ref(), &knn_queries[i], window[i].clone()) {
            Ok(mut r) => {
                if let (Some(p), Some(d)) = (r.profile.as_mut(), knn_profiles[i].clone()) {
                    p.dfs = Some(d);
                }
                results.push(r)
            }
            Err(e) => failures.push((i, e)),
        }
    }
    if !failures.is_empty() && (results.is_empty() || !req.allow_partial) {
        return Err(all_shards_failed(&cluster.node.id, targets, &failures, if results.is_empty() { "all shards failed" } else { "Partial shards failure" }));
    }
    // Reduce: merge hits in sort order (shard order breaks ties).
    let built_keys: Vec<SortKey> = if results.is_empty() {
        Vec::new()
    } else {
        let t = &targets[results[0].target];
        let table = t.engine.table();
        let qctx = QueryCtx::new(cluster, &st, &t.idx, &table, &results[0].searcher, req.now_ms);
        let b = sort::build(&qctx, &src.sort)?;
        if b.by_score { Vec::new() } else { b.keys }
    };
    let formats = if results.is_empty() {
        Vec::new()
    } else {
        let t = &targets[results[0].target];
        let table = t.engine.table();
        let qctx = QueryCtx::new(cluster, &st, &t.idx, &table, &results[0].searcher, req.now_ms);
        sort::build(&qctx, &src.sort)?.formats
    };
    let mut all: Vec<(usize, ShardHit)> = Vec::new();
    for (ri, r) in results.iter().enumerate() {
        for h in &r.hits {
            all.push((ri, h.clone()));
        }
    }
    all.sort_by(|a, b| cmp_hits(&built_keys, (&a.1, results[a.0].target), (&b.1, results[b.0].target)));
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
            timed_out: false,
            profile: None,
            scroll_id: None,
            pit_id: None,
            ranked,
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
    };
    let mut rendered: Vec<(usize, Value)> = Vec::with_capacity(page.len());
    let mut fetch_times: Vec<profile::FetchTimes> = vec![Default::default(); results.len()];
    // A shard whose fetch fails is a shard failure: its hits drop out of the page.
    let mut fetch_failed: Vec<usize> = Vec::new();
    for (ri, h) in &page {
        if fetch_failed.contains(ri) {
            continue;
        }
        let r = &results[*ri];
        let t = &targets[r.target];
        let at = std::time::Instant::now();
        match render_hit(cluster, &st, req, t, r, h, &opts, &formats) {
            Ok(v) => rendered.push((*ri, v)),
            // The expand phase runs on the coordinator after the fetch: no shard failed.
            Err(e) if is_expand_failure(&e) => return Err(e),
            Err(e) => {
                fetch_failed.push(*ri);
                failures.push((r.target, e));
                continue;
            }
        }
        fetch_times[*ri].nanos += at.elapsed().as_nanos() as u64;
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
        obj! { "shards" => Value::Arr(shards) }
    });
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
            let parts: Vec<search_aggs::Aggs> = results.iter_mut().filter_map(|r| r.aggs.take()).collect();
            let mut out = aggs::reduce(cluster, &st, a, parts, req.typed_keys)?;
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
    let terminated_early = if src.terminate_after.is_some() { Some(results.iter().any(|r| r.terminated_early)) } else { None };
    // A request with only suggestions skips the query phase (no hits, total 0).
    let suggest_only = src.suggest.is_some() && src.query.is_none() && src.knn.is_empty() && src.aggs.is_none();
    let suggest_out = match &src.suggest {
        Some(sg) => Some(suggest::run(cluster, targets, sg, req.typed_keys)?),
        None => None,
    };
    Ok(SearchOutcome {
        took_ms: started.elapsed().as_millis() as u64,
        shards_total: targets.len(),
        shards_skipped: 0,
        failures: failures.iter().map(|(i, e)| shard_failure(&cluster.node.id, &targets[*i], e)).collect(),
        total: tracking.map(|_| if suggest_only { (0, TotalRelation::EqualTo) } else { (total, relation) }),
        max_score: if suggest_only || (size == 0 && !built_keys.is_empty()) { None } else { max_score },
        hits: if suggest_only { Vec::new() } else { hits_out },
        aggs: aggs_out,
        suggest: suggest_out,
        terminated_early,
        timed_out: false,
        profile: profile_out,
        scroll_id: None,
        pit_id: None,
        ranked,
    })
}

#[allow(clippy::too_many_arguments)]
fn render_hit(cluster: &Cluster, st: &ClusterState, req: &SearchRequest, t: &Target, r: &ShardResult, h: &ShardHit, opts: &FetchOpts, formats: &[sort::ValueFormat]) -> EsResult<Value> {
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
    // `stored_fields: _none_` loads no stored fields at all, `_id` included.
    if opts.stored_fields != Some(None) {
        m.insert("_id".into(), Value::Str(id));
    }
    let score = if h.sort.is_empty() || src.track_scores || src.sort.iter().any(|s| matches!(s, Sort::Score { .. })) { Some(h.score) } else { None };
    hits::render_doc(&table, Some(&t.idx), &r.searcher, h.doc, opts, &mut m, score, req.now_ms)?;
    if !src.fields.is_empty() && (src.runtime_mappings.is_some() || !t.idx.mapping.runtime.is_empty()) {
        runtime_fields_of_hit(cluster, st, src, t, &table, &r.searcher, h.doc, &mut m)?;
    }
    if !src.script_fields.is_empty()
        && let Some((seg, local)) = r.searcher.segment_of(h.doc)
    {
        let mut sf = Map::new();
        for f in &src.script_fields {
            let bound = cluster.scripts.compile(st, &f.script, search_painless::Context::Field)?;
            let p = scripts::Painless { bound, table: table.clone(), shard: t.shard.id as i32, index: t.idx.name.clone() };
            match scripts::script_field(&p, seg, local, h.score) {
                Ok(v) => {
                    sf.insert(f.name.clone(), Value::Arr(v));
                }
                Err(_) if f.ignore_failure => {}
                Err(e) => return Err(e),
            }
        }
        if !sf.is_empty() {
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
        }
    }
    // `_percolator_document_slot[_<name>]`: the documents each stored query matched.
    // One percolate query reports `_percolator_document_slot`; several, suffixed by name.
    for pc in &r.percolate {
        let field = match (&pc.name, r.percolate.len()) {
            (Some(n), 2..) => format!("_percolator_document_slot_{n}"),
            _ => "_percolator_document_slot".to_string(),
        };
        if let Some(s) = pc.slots.get(&h.doc) {
            let mut add: Vec<(String, Value)> = vec![(field.clone(), Value::Arr(s.iter().map(|x| Value::from(*x as i64)).collect()))];
            for (slot, names) in pc.named.get(&h.doc).into_iter().flatten() {
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
    if let (Some(c), Some(g)) = (&src.collapse, &h.group) {
        let table2 = t.engine.table();
        let gv = match (g, table2.get(&c.field)) {
            (SortValue::Null, _) => Value::Null,
            (SortValue::Bytes(b), Some(f)) if f.kind == crate::engine::fields::Kind::Ip && b.len() == 16 => Value::Str(search_index::query::range::inet_to_string(b)),
            (SortValue::Bytes(b), _) => Value::Str(String::from_utf8_lossy(b).into_owned()),
            (SortValue::Long(l), Some(f)) => hits::format_numeric(f, *l, None).unwrap_or(Value::from(*l)),
            (v, _) => sort::render_value(v, &sort::ValueFormat::Long, h.doc, h.score),
        };
        let fv = if gv.is_null() { Value::Arr(vec![Value::Null]) } else { Value::Arr(vec![gv.clone()]) };
        match m.get_mut("fields") {
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
        let spec = highlight::Spec::parse(hl)?;
        let stored = r.searcher.stored(h.doc).map_err(crate::engine::convert::index_error)?;
        let source = crate::fetch::source_bytes(&stored).map(crate::fetch::parse_source).transpose()?;
        let qctx = QueryCtx::new(cluster, st, &t.idx, &table, &r.searcher, req.now_ms);
        let id = m.get("_id").and_then(Value::as_str).unwrap_or_default().to_string();
        let mut out = highlight::highlight_hit(&qctx, &spec, &r.query, source.as_ref(), &id, &r.searcher, Some(h.doc))?;
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
        m.insert("inner_hits".into(), inner::render(&qctx, t, r, h.doc, source.as_ref(), req.total_as_int)?);
    }
    Ok(Value::Obj(m))
}

/// Completes the `top_hits` hits of the reduced aggregations: each carries a hidden marker
/// ([`aggs::TOP_HIT_MARKER`]) naming its shard and doc, and gets here what needs the search's
/// shard results: `highlight` against the search's query, `matched_queries` of its named
/// queries, `explain`, runtime fields of the top-level `fields`. `rest_total_hits_as_int`
/// applies to their totals too.
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
            let out = highlight::highlight_hit(&qctx, &spec, &r.query, source.as_ref(), &id, &r.searcher, Some(root))?;
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
    Ok(())
}

/// Runtime fields the `fields` option asks for (names or patterns), computed for one hit and
/// added to its `fields`.
#[allow(clippy::too_many_arguments)]
fn runtime_fields_of_hit(cluster: &Cluster, st: &ClusterState, src: &SearchSource, t: &Target, table: &Arc<crate::engine::fields::FieldTable>, searcher: &Searcher, doc: u32, m: &mut Map) -> EsResult<()> {
    let rt = runtime::RuntimeFields::of(&t.idx.mapping, src.runtime_mappings.as_ref())?;
    let has_lookup = src.runtime_mappings.as_ref().is_some_and(|r| r.values().any(|d| d.get("type").and_then(Value::as_str) == Some("lookup")));
    if rt.is_empty() && !has_lookup {
        return Ok(());
    }
    let Some((seg, local)) = searcher.segment_of(doc) else { return Ok(()) };
    let requested = |n: &str| src.runtime_mappings.as_ref().is_some_and(|r| r.contains_key(n) || n.rsplit_once('.').is_some_and(|(p, _)| r.contains_key(p)));
    let mut out = Map::new();
    lookup_fields_of_hit(cluster, src, table, searcher, doc, &mut out)?;
    for spec in &src.fields {
        let names = if spec.field.contains('*') { rt.matching(&spec.field) } else if rt.get(&spec.field).is_some() { vec![spec.field.clone()] } else { Vec::new() };
        for name in names {
            if out.contains_key(&name) || (table.get(&name).is_some() && !requested(&name)) {
                continue;
            }
            let Some(f) = rt.get(&name).cloned() else { continue };
            let ev = runtime::Evaluator::new(&cluster.scripts, st, table, t.shard.id as i32, &t.idx.name, f.clone())?;
            let vals = ev.leaf(seg)?.values(local)?;
            if !vals.is_empty() {
                out.insert(name, Value::Arr(runtime::render(&f, &vals, spec.format.as_deref())?));
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
        s2.track_total_hits = Some(TrackTotalHits::Accurate);
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
    Ok(Value::Obj(out))
}

/// An explanation as `_explanation` prints it.
pub fn explanation_json(e: &search_index::Explanation) -> Value {
    let mut m = Map::new();
    let v = e.value;
    m.insert(
        "value".into(),
        if v.is_integral() {
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
    // (so it shows twice).
    if let Some((_, first)) = failures.first() {
        if !first.es {
            err = err.caused_by(EsError::java(first.status, first.kind.clone(), first.reason()).caused_by(first.clone()));
        } else if let Some(root) = first.guess_root_causes().first()
            && !std::ptr::eq(*root, first)
        {
            err = err.caused_by((*root).clone());
        }
    }
    err.with_root_causes(roots)
}

/// Renders the outcome (`SearchResponse.toXContent`).
pub fn render(o: &SearchOutcome, req: &SearchRequest) -> Value {
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
        shards.insert("failures".into(), Value::Arr(o.failures.clone()));
    }
    m.insert("_shards".into(), Value::Obj(shards));
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
    hits.insert("hits".into(), Value::Arr(o.hits.clone()));
    m.insert("hits".into(), Value::Obj(hits));
    if let Some(a) = &o.aggs {
        m.insert("aggregations".into(), a.clone());
    }
    if let Some(s) = &o.suggest {
        m.insert("suggest".into(), s.clone());
    }
    if let Some(p) = &o.profile {
        m.insert("profile".into(), p.clone());
    }
    Value::Obj(m)
}

/// `GET/POST /_search`, `/{index}/_search`.
pub async fn search(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?;
    let req = parse_request(ctx, body.as_ref(), ctx.raw_json())?;
    let out = run(ctx, req.clone())?;
    Reply::ok(render(&out, &req))
}

/// Runs a parsed search request: plain, the first page of a scroll, or on a point in time.
pub fn run(ctx: &Ctx, mut req: SearchRequest) -> EsResult<SearchOutcome> {
    if req.source.knn.iter().any(|m| m.contains_key("query_vector_builder")) {
        knn::resolve_lookups(ctx, &mut req.source)?;
    }
    if let Some(q) = &req.source.query
        && crate::query::contains_query(q, "percolate")
    {
        percolate::check_indexed_documents(ctx, q)?;
    }
    if let Some(pit) = req.source.pit.clone() {
        if !req.indices.is_empty() {
            return Err(EsError::action_request_validation(&["[indices] cannot be used with point in time. Do not specify any index with point in time.".into()]));
        }
        if req.scroll.is_some() {
            return Err(EsError::action_request_validation(&["using [point in time] is not allowed in a scroll context".into()]));
        }
        let targets = contexts::pit_targets(ctx, &pit)?;
        let errs = validate(&req);
        if !errs.is_empty() {
            return Err(EsError::action_request_validation(&errs));
        }
        let mut out = execute_on(ctx.cluster, &req, &targets)?;
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
        let mut targets = shard_targets(ctx.cluster, &st, &req)?;
        for t in targets.iter_mut() {
            t.searcher = Some(t.searcher()?);
        }
        let mut out = execute_on(ctx.cluster, &req, &targets)?;
        let n = out.hits.len();
        contexts::start_scroll(ctx, &req, targets, &mut out, n)?;
        return Ok(out);
    }
    execute(ctx.cluster, &req)
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
        if req.source.pit.is_some() && !req.indices.is_empty() {
            errs.push("[indices] cannot be used with point in time. Do not specify any index with point in time.".to_string());
        }
    }
    if !errs.is_empty() {
        return Err(EsError::action_request_validation(&errs));
    }
    for req in reqs {
        let r = run(ctx, req.clone()).map(|out| {
            let mut v = render(&out, &req);
            if let Value::Obj(m) = &mut v {
                m.insert("status".into(), Value::from(200i64));
            }
            v
        });
        responses.push(match r {
            Ok(v) => v,
            Err(e) => obj! { "error" => e.to_value_with_root(false), "status" => e.status as i64 },
        });
    }
    Reply::ok(obj! { "took" => started.elapsed().as_millis() as i64, "responses" => Value::Arr(responses) })
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
                "query" => src.query = Some(crate::query::parse_query(v, ctx.raw_json(), &[crate::json::locate::Seg::Key("query")])?.0),
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
    let q = shard_query(&qctx, &t, &req.source).map_err(|e| query::shard_exception(&idx, e))?;
    let doc = find_doc(&searcher, &id);
    let mut m = Map::new();
    m.insert("_index".into(), Value::Str(idx.name.clone()));
    m.insert("_id".into(), Value::Str(id.clone()));
    let Some(doc) = doc else {
        m.insert("matched".into(), Value::Bool(false));
        return Ok(Reply::Body(404, Value::Obj(m)));
    };
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
        let view = crate::fetch::SourceView { idx: &idx, table: &table, force_synthetic: false };
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
                "query" => src.query = Some(crate::query::parse_query(v, ctx.raw_json(), &[crate::json::locate::Seg::Key("query")])?.0),
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
    m.insert("_shards".into(), obj! { "total" => targets.len() as i64, "successful" => ok as i64, "skipped" => 0, "failed" => failures.len() as i64 });
    Reply::ok(Value::Obj(m))
}
