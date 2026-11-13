//! The APIs only the opensearch dialect has (OpenSearch 3.9.0's REST spec): its point-in-time
//! API (`/{index}/_search/point_in_time`, `_all` listing and deletion), search pipelines
//! (`/_search/pipeline` and their request and response processors), `_upgrade`, and the
//! workload management statistics table (`_list/wlm_stats`), and the k-NN plugin's node APIs
//! (`_plugins/_knn/stats`, `warmup`, `clear_cache`; see `spec/opensearch/plugins`), the SQL
//! plugin's PPL engine ([`ppl`]: `_plugins/_ppl`, its explain and grammar APIs, and
//! `_plugins/_query/settings`); plus the
//! dialect's field-type and query differences that need more than a branch where they occur
//! (`flat_object` sub-fields, `knn_vector`).
//!
//! The route tables already know these APIs (they come from OpenSearch's spec), so this
//! module is only their handlers, dispatched from [`crate::api::dispatch`] like the x-pack
//! table. Everything here answers the way OpenSearch 3.9.0 answers (texts, status codes, field
//! order), checked against a real OpenSearch in Docker; the elasticsearch dialect never
//! reaches it because its route table has none of these APIs.

pub mod approximate;
pub mod bitmap;
pub mod can_match;
pub mod cron;
pub mod flat_object;
pub mod geo;
pub mod hybrid;
pub mod ism;
pub mod ism_run;
pub mod jobs;
pub mod knn;
pub mod knn_api;
pub mod pit;
pub mod ppl;
pub mod profile;
pub mod search_pipeline;
pub mod upgrade;
pub mod wlm;

use crate::error::EsResult;
use crate::rest::{Ctx, Reply};
use crate::routes::Api;

/// Whether this module handles an API.
pub fn implemented(api: Api) -> bool {
    matches!(
        api,
        Api::CreatePit
            | Api::GetAllPits
            | Api::DeletePit
            | Api::DeleteAllPits
            | Api::IndicesUpgrade
            | Api::IndicesGetUpgrade
            | Api::WlmStatsList
            | Api::SearchPipelinePut
            | Api::SearchPipelineGet
            | Api::SearchPipelineDelete
            | Api::KnnStats
            | Api::KnnWarmup
            | Api::KnnClearCache
            | Api::Ppl
            | Api::PplExplain
            | Api::PplGrammar
            | Api::QuerySettings
    ) || ism::implemented(api)
}

pub async fn dispatch(ctx: &Ctx<'_>) -> EsResult<Reply> {
    match ctx.api {
        Api::CreatePit => pit::create(ctx),
        Api::GetAllPits => pit::list(ctx),
        Api::DeletePit => pit::delete(ctx),
        Api::DeleteAllPits => pit::delete_all(ctx),
        Api::IndicesUpgrade => upgrade::upgrade(ctx),
        Api::IndicesGetUpgrade => upgrade::status(ctx),
        Api::WlmStatsList => wlm::list(ctx),
        Api::SearchPipelinePut => search_pipeline::put(ctx).await,
        Api::SearchPipelineGet => search_pipeline::get(ctx),
        Api::SearchPipelineDelete => search_pipeline::delete(ctx).await,
        Api::KnnStats => knn_api::stats(ctx),
        Api::KnnWarmup => knn_api::warmup(ctx),
        Api::KnnClearCache => knn_api::clear_cache(ctx),
        Api::Ppl => ppl::ppl(ctx).await,
        Api::PplExplain => ppl::explain(ctx).await,
        Api::PplGrammar => ppl::grammar(ctx),
        Api::QuerySettings => ppl::settings(ctx).await,
        api if ism::implemented(api) => ism::dispatch(ctx).await,
        other => Err(crate::error::EsError::not_implemented(other.name())),
    }
}

thread_local! {
    /// Set while a request of an opensearch-dialect cluster is being handled on this thread
    /// (see [`Scoped`]): parsers whose grammar differs between the dialects read it.
    static OPENSEARCH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the request being handled on this thread speaks the opensearch dialect. Parsers
/// deep in the query DSL read this instead of taking the dialect through every call.
pub fn active() -> bool {
    OPENSEARCH.with(|f| f.get())
}

/// Runs `f` with [`active`] set to `on`, restoring the previous value after.
/// search-aggs keeps its own flag (it can't see this crate), set alongside.
pub fn with<R>(on: bool, f: impl FnOnce() -> R) -> R {
    let prev = OPENSEARCH.with(|c| c.replace(on));
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            OPENSEARCH.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(prev);
    search_aggs::dialect::with(on, f)
}

/// A future that sets [`active`] for the duration of each of its polls. A task can move
/// between threads at any await, and other tasks run between its polls, so the flag is set
/// per poll and restored after, never left behind.
pub struct Scoped<F> {
    inner: std::pin::Pin<Box<F>>,
}

impl<F: std::future::Future> Scoped<F> {
    pub fn new(inner: F) -> Scoped<F> {
        Scoped { inner: Box::pin(inner) }
    }
}

impl<F: std::future::Future> std::future::Future for Scoped<F> {
    type Output = F::Output;

    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<F::Output> {
        let inner = self.inner.as_mut();
        with(true, || inner.poll(cx))
    }
}

/// `RestSearchAction`'s `phase_took` (default `search.phase_took_enabled`); the dialect also
/// takes (and validates) `cancel_after_time_interval`, a cancellation deadline roll's searches
/// never outlive.
pub fn phase_took_requested(ctx: &Ctx) -> EsResult<bool> {
    if ctx.cluster.dialect != crate::cluster::Dialect::OpenSearch {
        return Ok(false);
    }
    ctx.time_param("cancel_after_time_interval", crate::units::TimeValue::ZERO)?;
    let default = ctx.state().metadata.cluster_setting("search.phase_took_enabled") == Some("true");
    ctx.bool_param("phase_took", default)
}

/// What an opensearch-dialect search response adds after rendering: `phase_took` (right after
/// `took`; roll's phases are not timed apart, the query phase gets the time) and the UBI
/// plugin's `ext.ubi.query_id` for a request that sent `ext.ubi`.
pub fn add_search_extras(out: &crate::search::SearchOutcome, req: &crate::search::SearchRequest, phase_took: bool, v: &mut crate::json::Value) {
    let crate::json::Value::Obj(m) = v else { return };
    if phase_took {
        let took = out.took_ms as i64;
        let phases = crate::obj! { "dfs_pre_query" => 0, "query" => took, "fetch" => 0, "dfs_query" => 0, "expand" => 0, "can_match" => 0 };
        let mut rebuilt = crate::json::Map::with_capacity(m.len() + 1);
        for (k, x) in std::mem::take(m) {
            let after_took = k == "took";
            rebuilt.insert(k, x);
            if after_took {
                rebuilt.insert("phase_took".into(), phases.clone());
            }
        }
        *m = rebuilt;
    }
    if let Some(ubi) = req.source.ext.as_ref().and_then(|e| e.get("ubi")) {
        let id = ubi.get("query_id").and_then(crate::json::Value::as_str).map(str::to_string).unwrap_or_else(random_uuid);
        m.insert("ext".into(), crate::obj! { "ubi" => crate::obj! { "query_id" => id } });
    }
}

/// `UUID.randomUUID().toString()`.
fn random_uuid() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// A search request's `derived` fields (`DerivedFieldParser`) as the runtime fields that compute
/// them: `float` runs as `double`; `text` as `keyword` (its values are not analyzed).
pub fn derived_fields(defs: crate::json::Map) -> EsResult<crate::json::Map> {
    let mut out = crate::json::Map::with_capacity(defs.len());
    for (name, def) in defs {
        let crate::json::Value::Obj(mut m) = def else {
            return Err(crate::error::EsError::illegal_argument(format!("[derived] field [{name}] must be an object")));
        };
        let ty = m.get("type").and_then(crate::json::Value::as_str).unwrap_or("").to_string();
        let runtime = match ty.as_str() {
            "boolean" | "date" | "geo_point" | "ip" | "keyword" | "long" | "double" => ty.clone(),
            "float" => "double".to_string(),
            "text" => "keyword".to_string(),
            _ => return Err(crate::error::EsError::illegal_argument(format!("Type [{ty}] isn't supported in Derived field context."))),
        };
        m.insert("type".into(), crate::json::Value::from(runtime));
        for key in ["prefilter_field", "ignore_malformed", "properties"] {
            m.shift_remove(key);
        }
        out.insert(name, crate::json::Value::Obj(m));
    }
    Ok(out)
}

/// The search request `ext` sections the bundled plugins register (`SearchExtSpec`s): the
/// neural-search `rerank`, ml-commons' `ml_inference` and UBI's `ubi`; any other name is an
/// unknown named object. `agent_steps_summary` is a response-only section.
pub fn parse_ext(p: &mut crate::query::XParser<'_>) -> EsResult<crate::json::Map> {
    if let crate::json::Value::Obj(m) = p.value() {
        for (name, _) in m {
            match name.as_str() {
                "rerank" | "ml_inference" | "ubi" => {}
                "agent_steps_summary" => {
                    return Err(crate::error::EsError::java(500, "unsupported_operation_exception", "AgentStepsSearchExtBuilder should not be parsed from request"));
                }
                other => {
                    let (l, c) = p.location_of(&[crate::json::locate::Seg::Key(other)]);
                    return Err(crate::error::EsError::java(400, "named_object_not_found_exception", format!("[{l}:{c}] unknown field [{other}]")));
                }
            }
        }
    }
    p.map()
}

/// Reorders every object of `v` the way the default-sized `HashMap`s of
/// `XContentHelper.convertToMap(…, ordered = false)` iterate them.
pub fn hash_maps(v: &mut crate::json::Value) {
    match v {
        crate::json::Value::Obj(m) => {
            for x in m.values_mut() {
                hash_maps(x);
            }
            *m = crate::json::java_hash_map(std::mem::take(m), 16);
        }
        crate::json::Value::Arr(a) => a.iter_mut().for_each(hash_maps),
        _ => {}
    }
}

/// The `_msearch` header keys OpenSearch's `MultiSearchRequest.readMultiLineFormat` accepts.
pub const MSEARCH_HEADER_KEYS: &[&str] = &[
    "index",
    "indices",
    "search_type",
    "searchType",
    "ccs_minimize_roundtrips",
    "ccsMinimizeRoundtrips",
    "request_cache",
    "requestCache",
    "preference",
    "routing",
    "allow_partial_search_results",
    "expand_wildcards",
    "expandWildcards",
    "ignore_unavailable",
    "ignoreUnavailable",
    "allow_no_indices",
    "allowNoIndices",
    "ignore_throttled",
    "ignoreThrottled",
    "cancel_after_time_interval",
    "cancelAfterTimeInterval",
    "phase_took",
];

/// Whether the per-item errors of a multi-item API (`_bulk`, `_mget`, `_msearch`,
/// `_mtermvectors`) carry their `stack_trace`: OpenSearch renders them with the request's
/// `error_trace`, Elasticsearch only ever traces the top-level error.
pub fn item_trace(ctx: &Ctx) -> bool {
    ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch && ctx.bool_param("error_trace", false).unwrap_or(false)
}

/// A per-item error of a multi-item API as the dialect prints it: traced per [`item_trace`],
/// with `root_cause` for the read APIs (`with_root`), and OpenSearch's metadata order.
pub fn item_error(ctx: &Ctx, e: &crate::error::EsError, with_root: bool) -> crate::json::Value {
    let trace = item_trace(ctx);
    let mut v = if with_root { e.to_value_with_root(trace) } else { e.to_value(trace) };
    if ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch {
        crate::rest::opensearch_error_order(&mut v);
    }
    v
}

/// OpenSearch routes a sliced search only to the shards of its slice
/// (`SliceBuilder.shardMatches`, applied when the shard iterators are built): with at least as
/// many slices as shards each slice goes to shard `id % shards`, otherwise shard `s` belongs
/// to slice `s % max`. The other shards aren't part of the request (`_shards.total` doesn't
/// count them).
pub fn slice_targets(ctx: &Ctx, req: &crate::search::SearchRequest, targets: Vec<crate::search::Target>) -> Vec<crate::search::Target> {
    let Some(sl) = req.source.slice.as_ref().filter(|_| ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch) else { return targets };
    let (id, max) = (sl.id.max(0) as u32, sl.max.max(1) as u32);
    targets
        .into_iter()
        .filter(|t| {
            let shards = t.idx.number_of_shards().max(1);
            if max >= shards { id % shards == t.shard.id } else { t.shard.id % max == id }
        })
        .collect()
}

/// How many reduce phases OpenSearch's `QueryPhaseResultConsumer` runs for `results` shard
/// results with `batched_reduce_size` `batch`: a partial merge whenever a result arrives to a
/// full buffer (a pending partial result takes one slot), then the final reduce.
pub fn reduce_phases(results: usize, batch: usize) -> usize {
    let (mut buffer, mut partial, mut merges) = (0usize, false, 0usize);
    for _ in 0..results {
        if buffer + usize::from(partial) >= batch {
            partial = true;
            merges += 1;
            buffer = 0;
        }
        buffer += 1;
    }
    merges + 1
}

/// `SearchResponse.toXContent`'s `num_reduce_phases` (after `terminated_early`, before
/// `_shards`), written when not 1. Only searches that aggregate or return hits batch their
/// results (a scroll's first page too). Elasticsearch 9 reduces its shard results on the data nodes and
/// has no such field.
pub fn add_reduce_phases(ctx: &Ctx, out: &crate::search::SearchOutcome, req: &crate::search::SearchRequest, v: &mut crate::json::Value) -> EsResult<()> {
    if ctx.cluster.dialect != crate::cluster::Dialect::OpenSearch {
        return Ok(());
    }
    let has_aggs = req.source.aggs.as_ref().is_some_and(|a| !a.is_empty());
    if !has_aggs && req.source.size == Some(0) {
        return Ok(());
    }
    let batch = ctx.int_param("batched_reduce_size", 512)?.max(2) as usize;
    let results = out.shards_total.saturating_sub(out.shards_skipped + out.failures.len());
    let phases = reduce_phases(results, batch);
    if phases == 1 {
        return Ok(());
    }
    let crate::json::Value::Obj(m) = v else { return Ok(()) };
    let at = m.get_index_of("_shards").unwrap_or(m.len());
    m.shift_insert(at, "num_reduce_phases".into(), crate::json::Value::from(phases as i64));
    Ok(())
}

/// OpenSearch's terms lookup by query: the `path` values of every document of `l.index` the
/// lookup's query matches (`TermsQueryBuilder.fetch`), refused when more documents match than
/// one fetch may return (`min(max_terms_count, max_result_window, max_clause_count)`).
pub fn lookup_by_query(ctx: &crate::search::query::QueryCtx, l: &crate::query::term_level::TermsLookup, q: &crate::query::Query) -> EsResult<Vec<crate::json::Value>> {
    use crate::json::Value;
    let st = ctx.st;
    let setting = |idx: Option<&crate::meta::IndexMeta>, key: &str, default: i64| {
        idx.and_then(|i| i.settings.get(key)).and_then(|v| v.parse::<i64>().ok()).unwrap_or(default)
    };
    let idx = st.metadata.indices.get(&l.index).cloned();
    let max_terms = setting(idx.as_deref(), "index.max_terms_count", 65536);
    let max_window = setting(idx.as_deref(), "index.max_result_window", 10000);
    let max_clauses = st.metadata.cluster_setting("indices.query.bool.max_clause_count").and_then(|v| v.parse::<i64>().ok()).unwrap_or(1024);
    let fetch = max_terms.min(max_window).min(max_clauses).max(0);
    let mut source = crate::search_source::SearchSource { query: Some(q.clone()), size: Some(fetch as i32), ..Default::default() };
    source.source = Some(crate::search_source::FetchSource { fetch: true, includes: vec![l.path.clone()], excludes: Vec::new(), exclude_vectors: None });
    let req = crate::search::SearchRequest {
        indices: vec![l.index.clone()],
        opts: crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED,
        source,
        routing: l.routing.clone(),
        preference: None,
        dfs: false,
        request_cache: Some(false),
        total_as_int: false,
        typed_keys: false,
        allow_partial: false,
        scroll: None,
        include_named_queries_score: false,
        force_synthetic_source: false,
        now_ms: ctx.now_ms,
        fused: None,
        rank_only: false,
        security: None,
    };
    let out = crate::search::execute(ctx.cluster, &req)?;
    let total = out.total.map(|(t, _)| t as i64).unwrap_or(0);
    if total > fetch {
        return Err(crate::error::EsError::illegal_argument(format!("Terms lookup subquery total hits [{total}] exceed fetch limit [{fetch}]; filter may be incomplete.")));
    }
    if out.hits.len() as i64 > max_terms {
        return Err(crate::error::EsError::illegal_argument(format!(
            "Terms lookup subquery result count [{}] exceeds allowed max_terms_count [{max_terms}]",
            out.hits.len()
        )));
    }
    let mut terms = Vec::new();
    for h in &out.hits {
        if let Some(Value::Obj(src)) = h.get("_source") {
            let mut vals = Vec::new();
            crate::engine::source::extract_values(src, &l.path, &mut vals);
            terms.extend(vals.into_iter().filter(|v| !v.is_null()).cloned());
        }
    }
    Ok(terms)
}

/// OpenSearch's `KeywordFieldType.termQuery`: a keyword term scores a constant unless the
/// field sets `use_similarity` (which roll's mappings don't accept yet), where Elasticsearch
/// scores it with BM25. Other fields' term queries are unchanged.
pub fn keyword_term(f: &crate::engine::fields::FieldInfo, q: search_index::QueryRef) -> search_index::QueryRef {
    if active() && f.kind == crate::engine::fields::Kind::Keyword {
        std::sync::Arc::new(search_index::query::ConstantScoreQuery::new(q))
    } else {
        q
    }
}

/// OpenSearch's `DocumentParser` field name rules: no empty names, no names of dots only, no
/// empty path segment (a leading or doubled dot; a trailing one is dropped as Java's `split`
/// drops it). Whitespace-only names are allowed. Failures are a `mapper_parsing_exception`
/// "failed to parse" caused by the rule.
pub fn check_field_name(name: &str) -> EsResult<()> {
    let failed = |msg: String| crate::error::EsError::mapper_parsing("failed to parse").caused_by(crate::error::EsError::illegal_argument(msg));
    if name.is_empty() {
        return Err(failed("field name cannot be an empty string".into()));
    }
    if name.contains('.') {
        let mut parts: Vec<&str> = name.split('.').collect();
        while parts.last().is_some_and(|p| p.is_empty()) {
            parts.pop();
        }
        if parts.is_empty() {
            return Err(failed("field name cannot contain only the character [.]".into()));
        }
        if parts.iter().any(|p| p.is_empty()) {
            return Err(failed(format!("object field starting or ending with a [.] makes object resolution ambiguous: [{name}]")));
        }
    }
    Ok(())
}

/// OpenSearch's error for a document value written to a field alias: an
/// `IllegalArgumentException`, which `DocumentParser` wraps as "failed to parse". `None` in
/// the elasticsearch dialect (a located `document_parsing_exception` there).
pub fn alias_write_error(full: &str) -> Option<crate::error::EsError> {
    use crate::error::EsError;
    active().then(|| EsError::mapper_parsing("failed to parse").caused_by(EsError::illegal_argument(format!("Cannot write to a field alias [{full}]."))))
}

/// `reason` without a leading `[line:col] ` location (Elasticsearch's `DocumentParsingException`
/// and `XContentParseException` texts carry one; OpenSearch's mapper exceptions don't).
pub fn strip_location(reason: &str) -> &str {
    let Some(rest) = reason.strip_prefix('[') else { return reason };
    let Some((loc, tail)) = rest.split_once("] ") else { return reason };
    let Some((l, c)) = loc.split_once(':') else { return reason };
    if !l.is_empty() && !c.is_empty() && l.bytes().all(|b| b.is_ascii_digit()) && c.bytes().all(|b| b.is_ascii_digit()) { tail } else { reason }
}

/// OpenSearch's append-only indices (`index.append_only.enabled`,
/// `TransportBulkAction.addFailureIfAppendOnlyIndexAndOpsDeleteOrUpdate`): updates, deletes and
/// `index` operations with a caller-chosen id fail with a `validation_exception`; `create`
/// and auto-id writes pass. `op` is the operation as Java prints its `OpType` (`INDEX`,
/// `UPDATE`, `DELETE`), `expr` the request's index expression (named as given).
pub fn append_only_violation(st: &crate::meta::ClusterState, expr: &str, op: &str, id: Option<&str>) -> Option<crate::error::EsError> {
    if st.dialect != crate::cluster::Dialect::OpenSearch {
        return None;
    }
    let idx = crate::write::resolve_existing(st, expr)?;
    if idx.settings.get("index.append_only.enabled") != Some("true") {
        return None;
    }
    let msg = match (op, id) {
        ("UPDATE" | "DELETE", _) => format!("Operation [{op}] is not allowed as setting `index.append_only.enabled` is enabled for this index: {expr}"),
        ("INDEX", Some(id)) => format!("Operation [INDEX] is not allowed with a custom document id {id} as setting `index.append_only.enabled` is enabled for this index: {expr}"),
        _ => return None,
    };
    let mut e = crate::error::EsError::action_request_validation(&[msg]);
    e.kind = "validation_exception".into();
    Some(e)
}

/// `IndexMetadata`'s check that bulk adaptive shard selection is only enabled on an
/// append-only index (creation, settings updates and templates applied at creation).
pub fn check_adaptive_shard_selection(index: &str, settings: &crate::meta::settings::Settings) -> EsResult<()> {
    if settings.get("index.bulk.adaptive_shard_selection.enabled") == Some("true") && settings.get("index.append_only.enabled") != Some("true") {
        return Err(crate::error::EsError::illegal_argument(format!(
            "index [{index}] is not append-only index, bulk adaptive shard selection is enabled, which is not supported. Please disable bulk adaptive shard selection or set index to append-only index."
        )));
    }
    Ok(())
}

/// `String.hashCode` (UTF-16 units, wrapping).
fn java_hash(s: &str) -> i32 {
    s.encode_utf16().fold(0i32, |h, u| h.wrapping_mul(31).wrapping_add(i32::from(u)))
}

/// The order a `java.util.HashMap` iterates `keys` in (`Arrays.toString(map.keySet())` in
/// OpenSearch's "doesn't support one or more provided configuration parameters" messages):
/// by bucket — `(h ^ h >>> 16) & (capacity - 1)` — then insertion order within a bucket.
/// `entries` is how many entries the map held at its largest (the map parsed from the
/// request), which fixes its capacity: 16, doubled whenever it passed 0.75 of it.
pub(crate) fn java_hashmap_order<'a>(keys: &[&'a str], entries: usize) -> Vec<&'a str> {
    let mut cap = 16usize;
    while entries > cap * 3 / 4 {
        cap *= 2;
    }
    let bucket = |k: &str| {
        let h = java_hash(k);
        ((h ^ ((h as u32) >> 16) as i32) as u32 as usize) & (cap - 1)
    };
    let mut out: Vec<(usize, usize, &str)> = keys.iter().enumerate().map(|(i, k)| (bucket(k), i, *k)).collect();
    out.sort_unstable();
    out.into_iter().map(|(_, _, k)| k).collect()
}

/// A value as `XContentHelper.convertToMap(..., ordered = false)` gives it back: every object a
/// default-sized `HashMap`.
pub(crate) fn hash_ordered(v: crate::json::Value) -> crate::json::Value {
    use crate::json::Value;
    match v {
        Value::Obj(m) => {
            let m: crate::json::Map = m.into_iter().map(|(k, v)| (k, hash_ordered(v))).collect();
            Value::Obj(crate::json::java_hash_map(m, 16))
        }
        Value::Arr(a) => Value::Arr(a.into_iter().map(hash_ordered).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduce_phases_match_the_result_consumer() {
        // OpenSearch's 120_batch_reduce_size: 5 shards, batched_reduce_size 2.
        assert_eq!(reduce_phases(5, 2), 4);
        assert_eq!(reduce_phases(0, 2), 1);
        assert_eq!(reduce_phases(1, 2), 1);
        assert_eq!(reduce_phases(2, 2), 1);
        assert_eq!(reduce_phases(3, 2), 2);
        assert_eq!(reduce_phases(512, 512), 1);
        assert_eq!(reduce_phases(513, 512), 2);
        // Each later merge takes batch - 1 new results (the partial result holds a slot).
        assert_eq!(reduce_phases(7, 3), 3);
    }

    #[test]
    fn strips_only_a_location() {
        assert_eq!(strip_location("[1:6] failed to parse"), "failed to parse");
        assert_eq!(strip_location("[12:345] x"), "x");
        assert_eq!(strip_location("[a:1] x"), "[a:1] x");
        assert_eq!(strip_location("[1:] x"), "[1:] x");
        assert_eq!(strip_location("[geo_point] field"), "[geo_point] field");
        assert_eq!(strip_location("no location"), "no location");
    }

    #[test]
    fn hashmap_order_matches_java() {
        // `new HashMap<>()` with these keys put in this order iterates as Java prints it.
        assert_eq!(java_hash("invalid_field"), "invalid_field".encode_utf16().fold(0i32, |h, u| h.wrapping_mul(31).wrapping_add(u as i32)));
        assert_eq!(java_hash(""), 0);
        assert_eq!(java_hash("a"), 97);
        // Java: Map.of-free HashMap {b, a, c} iterates a, b, c (buckets 97, 98, 99 & 15).
        assert_eq!(java_hashmap_order(&["b", "a", "c"], 3), vec!["a", "b", "c"]);
        // "x" (120 & 15 = 8) comes before "q" (113 & 15 = 1)? No: bucket 1 first.
        assert_eq!(java_hashmap_order(&["x", "q"], 2), vec!["q", "x"]);
        // A 13-entry map has 32 buckets: "a" (97 & 31 = 1) before "p" (112 & 31 = 16).
        assert_eq!(java_hashmap_order(&["p", "a"], 13), vec!["a", "p"]);
        assert_eq!(java_hashmap_order(&["p", "a"], 2), vec!["p", "a"]);
    }
}
