//! OpenSearch's Search Relevance Workbench (`_plugins/_search_relevance/*`, the bundled
//! `opensearch-search-relevance` plugin): query sets, search configurations, judgment lists and
//! experiments. Checked against OpenSearch 3.9.0 (`tests/fixtures/rest/os_srw.json`); the design
//! is docs/search/relevance-experiments.md, section 1.
//!
//! **Stores.** As the plugin does, every resource is a document in a plain index with one shard,
//! made on first use with the plugin's mapping ([`Store`]): a client reading those indices sees
//! the plugin's documents, and they branch, snapshot and restore with the cluster. A `GET` answers
//! the store's own search response (newest first, or the one document), as the plugin does.
//!
//! **What runs.**
//! - Query sets: `manual` (`PUT`), and `topn`, `random` and `pptss` sampled from UBI's
//!   `ubi_queries` (`POST`).
//! - Judgments: `IMPORT_JUDGMENT`, and `UBI_JUDGMENT` (`clickModel: coec`) from UBI's
//!   `ubi_events` through the COEC that roll's own judgments from clicks use
//!   ([`crate::analytics::coec`]).
//! - Experiments: `POINTWISE_EVALUATION` and `PAIRWISE_COMPARISON`, one search per query and
//!   configuration as the user who made the experiment, after the answer when the node runs
//!   background work. Their metrics are the plugin's (`Coverage`, `Precision`, `Recall`, `MAP`,
//!   `MRR`, `DCG`, `NDCG`; `jaccard`, `rbo50`, `rbo90`, `frequencyWeighted`), not `_rank_eval`'s.
//!
//! **Left out** (400, saying so): `LLM_JUDGMENT` (an ML Commons model; documents would go to a
//! model provider), `HYBRID_OPTIMIZER` (neural-search hybrid pipelines), scheduled experiments.
//! SRW's own quirks aren't copied: storing a `PUT` query set it refuses, a 500 null pointer for a
//! missing field (roll names the field, 400).

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::routes::Api;
use crate::xpack::internal;

const QUERY_SETS: Store = Store {
    index: "search-relevance-queryset",
    mapping: r#"{"_meta":{"schema_version":1},"properties":{"description":{"type":"text"},"id":{"type":"keyword"},"name":{"type":"keyword"},"querySetQueries":{"type":"nested","properties":{"customFields":{"type":"object","dynamic":"true"},"queryText":{"type":"text"}}},"sampling":{"type":"keyword"},"timestamp":{"type":"date","format":"strict_date_time"}}}"#,
};
const CONFIGS: Store = Store {
    index: "search-relevance-search-config",
    mapping: r#"{"_meta":{"schema_version":1},"properties":{"description":{"type":"text"},"id":{"type":"keyword"},"index":{"type":"keyword"},"name":{"type":"keyword"},"query":{"type":"text","fields":{"keyword":{"type":"keyword","ignore_above":256}}},"queryBody":{"type":"text"},"searchPipeline":{"type":"keyword"},"timestamp":{"type":"date","format":"strict_date_time"}}}"#,
};
const JUDGMENTS: Store = Store {
    index: "search-relevance-judgment",
    mapping: r#"{"_meta":{"schema_version":0},"properties":{"id":{"type":"keyword"},"judgmentRatings":{"type":"nested","properties":{"query":{"type":"text"},"ratings":{"type":"nested","properties":{"docId":{"type":"text"},"rating":{"type":"float"}}}}},"metadata":{"type":"object","dynamic":"false"},"name":{"type":"keyword"},"status":{"type":"text","fields":{"keyword":{"type":"keyword","ignore_above":256}}},"timestamp":{"type":"date","format":"strict_date_time"},"type":{"type":"keyword"}}}"#,
};
const EXPERIMENTS: Store = Store {
    index: ".plugins-search-relevance-experiment",
    mapping: r#"{"_meta":{"schema_version":1},"properties":{"description":{"type":"text"},"id":{"type":"keyword"},"inputSignature":{"dynamic":"false","properties":{"judgment_list":{"type":"keyword","ignore_above":128},"query_set":{"type":"keyword","ignore_above":128},"search_configurations":{"type":"keyword","ignore_above":128}}},"isScheduled":{"type":"keyword"},"judgmentList":{"type":"keyword"},"name":{"type":"text","fields":{"keyword":{"type":"keyword"}}},"querySetId":{"type":"keyword"},"results":{"type":"object","dynamic":"false"},"scheduledExperimentJobId":{"type":"keyword"},"searchConfigurationList":{"type":"keyword"},"size":{"type":"keyword"},"status":{"type":"keyword"},"timestamp":{"type":"date","format":"strict_date_time"},"type":{"type":"keyword"}}}"#,
};
const VARIANTS: Store = Store {
    index: "search-relevance-experiment-variant",
    mapping: r#"{"_meta":{"schema_version":0},"properties":{"experimentId":{"type":"keyword"},"id":{"type":"keyword"},"parameters":{"type":"object","dynamic":"false"},"results":{"type":"nested","properties":{"details":{"type":"text","fields":{"keyword":{"type":"keyword","ignore_above":256}}},"evaluationResultId":{"type":"keyword"}}},"status":{"type":"keyword"},"timestamp":{"type":"date","format":"strict_date_time"},"type":{"type":"keyword"}}}"#,
};
/// Evaluation results (`judgmentIds` is mapped dynamically, as the plugin's first result maps it).
const RESULTS: Store = Store {
    index: "search-relevance-evaluation-result",
    mapping: r#"{"_meta":{"schema_version":0},"properties":{"documentIds":{"type":"keyword"},"experimentId":{"type":"keyword"},"experimentVariantId":{"type":"keyword"},"experimentVariantParameters":{"type":"keyword"},"id":{"type":"keyword"},"judgmentList":{"type":"keyword"},"metrics":{"type":"nested","properties":{"metric":{"type":"keyword"},"value":{"type":"float"}}},"scheduledRunId":{"type":"keyword"},"searchConfigurationId":{"type":"keyword"},"searchText":{"type":"keyword"},"timestamp":{"type":"date","format":"strict_date_time"}}}"#,
};

/// The setting that turns the Workbench off (a plain-text 403 for every route).
const ENABLED: &str = "plugins.search_relevance.workbench_enabled";
/// Hits a collection's `GET` answers.
const LIST_SIZE: usize = 1000;
/// UBI events one click judgment reads at most.
const MAX_EVENTS: usize = 200_000;
/// The setting bounding a query set's queries (the plugin's; past it a plain-text 403).
const QUERY_SET_MAXIMUM: &str = "plugins.search_relevance.query_set.maximum";
/// Experiments one cluster runs at once (more answer 429).
const MAX_RUNNING: usize = 4;
/// The task an experiment's run is registered as (it keeps the cluster loaded while it runs).
const EXPERIMENT_ACTION: &str = "cluster:admin/opensearch/search_relevance/experiment/run";
/// The default UBI events and queries indices.
const UBI_EVENTS: &str = "ubi_events";
const UBI_QUERIES: &str = "ubi_queries";

/// One of the plugin's indices and the mapping it makes it with.
#[derive(Clone, Copy)]
struct Store {
    index: &'static str,
    mapping: &'static str,
}

/// Whether this module handles an API.
pub fn implemented(api: Api) -> bool {
    matches!(
        api,
        Api::SearchRelevancePutQuerySets
            | Api::SearchRelevancePostQuerySets
            | Api::SearchRelevanceGetQuerySets
            | Api::SearchRelevanceDeleteQuerySets
            | Api::SearchRelevanceSearchQuerySets
            | Api::SearchRelevancePutSearchConfigurations
            | Api::SearchRelevanceGetSearchConfigurations
            | Api::SearchRelevanceDeleteSearchConfigurations
            | Api::SearchRelevanceSearchSearchConfigurations
            | Api::SearchRelevancePutJudgments
            | Api::SearchRelevanceGetJudgments
            | Api::SearchRelevanceDeleteJudgments
            | Api::SearchRelevanceSearchJudgments
            | Api::SearchRelevancePutExperiments
            | Api::SearchRelevanceGetExperiments
            | Api::SearchRelevanceDeleteExperiments
            | Api::SearchRelevanceSearchExperiments
    )
}

pub async fn dispatch(ctx: &Ctx<'_>) -> EsResult<Reply> {
    if ctx.state().metadata.cluster_setting(ENABLED) == Some("false") {
        return Ok(Reply::Raw(403, "text/plain; charset=UTF-8".into(), bytes::Bytes::from_static(b"Search Relevance Workbench is disabled")));
    }
    match ctx.api {
        Api::SearchRelevancePutQuerySets => put_query_set(ctx).await,
        Api::SearchRelevancePostQuerySets => sample_query_set(ctx).await,
        Api::SearchRelevancePutSearchConfigurations => put_config(ctx).await,
        Api::SearchRelevancePutJudgments => put_judgment(ctx).await,
        Api::SearchRelevancePutExperiments => put_experiment(ctx).await,
        Api::SearchRelevanceGetQuerySets => get(ctx, QUERY_SETS, "query_set_id").await,
        Api::SearchRelevanceGetSearchConfigurations => get(ctx, CONFIGS, "search_configuration_id").await,
        Api::SearchRelevanceGetJudgments => get(ctx, JUDGMENTS, "judgment_id").await,
        Api::SearchRelevanceGetExperiments => get(ctx, EXPERIMENTS, "experiment_id").await,
        Api::SearchRelevanceDeleteQuerySets => delete(ctx, QUERY_SETS, "query_set_id").await,
        Api::SearchRelevanceDeleteSearchConfigurations => delete(ctx, CONFIGS, "search_configuration_id").await,
        Api::SearchRelevanceDeleteJudgments => delete(ctx, JUDGMENTS, "judgment_id").await,
        Api::SearchRelevanceDeleteExperiments => delete(ctx, EXPERIMENTS, "experiment_id").await,
        Api::SearchRelevanceSearchQuerySets => search(ctx, QUERY_SETS).await,
        Api::SearchRelevanceSearchSearchConfigurations => search(ctx, CONFIGS).await,
        Api::SearchRelevanceSearchJudgments => search(ctx, JUDGMENTS).await,
        Api::SearchRelevanceSearchExperiments => search(ctx, EXPERIMENTS).await,
        other => Err(EsError::not_implemented(other.name())),
    }
}

// ------------------------------------------------------------------------------ errors and bodies

fn srw(status: u16, msg: impl Into<String>) -> EsError {
    EsError::new(status, "search_relevance_exception", msg)
}

fn iae(msg: impl Into<String>) -> EsError {
    EsError::java(400, "illegal_argument_exception", msg)
}

/// A field roll requires where the plugin answers a null pointer.
fn missing(field: &str) -> EsError {
    iae(format!("[{field}] is required"))
}

fn body_of(ctx: &Ctx) -> EsResult<Map> {
    match ctx.body()? {
        Some(Value::Obj(m)) => Ok(m),
        None => Err(EsError::new(400, "parse_exception", "request body is required")),
        Some(_) => Err(EsError::new(400, "parsing_exception", "Failed to parse object: expecting token of type [START_OBJECT]")),
    }
}

fn text(m: &Map, k: &str) -> Option<String> {
    m.get(k).and_then(Value::as_str).map(str::to_string)
}

/// `UUID.randomUUID().toString()`.
fn uuid() -> String {
    super::random_uuid()
}

/// Java's `Instant.toString()` of `ms` as the plugin stores it (`strict_date_time`).
fn timestamp(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(|| ms.to_string(), |d| d.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}

fn sha256_hex(text: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

// ------------------------------------------------------------------------------ stores

/// Makes the store when it doesn't exist yet (one shard, as the plugin makes it).
async fn ensure(cluster: &Cluster, s: Store) -> EsResult<()> {
    if cluster.state().metadata.indices.contains_key(s.index) {
        return Ok(());
    }
    let mapping = crate::json::parse(s.mapping.as_bytes()).ok().flatten().unwrap_or(Value::Obj(Map::new()));
    let body = obj! { "settings" => obj! { "index.number_of_shards" => "1", "index.auto_expand_replicas" => "0-1" }, "mappings" => mapping };
    match internal::call_system(cluster, "PUT", &format!("/{}", s.index), Some(&body)).await {
        Ok(_) => Ok(()),
        Err(e) if e.kind == "resource_already_exists_exception" => Ok(()),
        Err(e) => Err(e),
    }
}

/// Writes document `id` of store `index` (made on first use), refreshed.
async fn write(cluster: &Cluster, s: Option<Store>, index: &str, id: &str, doc: &Value) -> EsResult<Value> {
    if let Some(s) = s {
        ensure(cluster, s).await?;
    }
    internal::call_system(cluster, "PUT", &format!("/{index}/_doc/{}?refresh=true", crate::xpack::enc(id)), Some(doc)).await
}

/// Document `id`'s `_source` in store `s`, if it's there.
async fn read(cluster: &Cluster, s: Store, id: &str) -> EsResult<Option<Map>> {
    if !cluster.state().metadata.indices.contains_key(s.index) {
        return Ok(None);
    }
    let body = obj! { "query" => obj! { "term" => obj! { "_id" => id } } };
    let v = internal::call_system(cluster, "POST", &format!("/{}/_search", s.index), Some(&body)).await?;
    Ok(v.pointer(&["hits", "hits"]).and_then(Value::as_array).and_then(|h| h.first()).and_then(|h| h.get("_source")).and_then(Value::as_object).cloned())
}

/// `GET …/{collection}` (the store's search response, newest first) and `GET …/{collection}/{id}`
/// (one hit, or 404).
async fn get(ctx: &Ctx<'_>, s: Store, param: &str) -> EsResult<Reply> {
    let exists = ctx.state().metadata.indices.contains_key(s.index);
    match ctx.path_param(param) {
        None => {
            if !exists {
                // The plugin's answer for a store not made yet.
                let shards = obj! { "total" => 0, "successful" => 0, "skipped" => 0, "failed" => 0 };
                let hits = obj! { "total" => obj! { "value" => 0, "relation" => "eq" }, "max_score" => 0.0, "hits" => Value::Arr(Vec::new()) };
                return Reply::ok(obj! { "took" => 0, "timed_out" => false, "_shards" => shards, "hits" => hits });
            }
            let body = obj! { "query" => obj! { "match_all" => obj! {} }, "sort" => Value::Arr(vec![obj! { "timestamp" => obj! { "order" => "desc" } }]), "size" => LIST_SIZE };
            Reply::ok(internal::call_system(ctx.cluster, "POST", &format!("/{}/_search", s.index), Some(&body)).await?)
        }
        Some(id) => {
            if !exists {
                return Err(srw(500, "Failed to get document").caused_by(EsError::index_not_found(s.index)));
            }
            let body = obj! { "query" => obj! { "term" => obj! { "_id" => id } } };
            let v = internal::call_system(ctx.cluster, "POST", &format!("/{}/_search", s.index), Some(&body)).await?;
            if v.pointer(&["hits", "hits"]).and_then(Value::as_array).is_none_or(|h| h.is_empty()) {
                return Err(EsError::new(404, "resource_not_found_exception", format!("Document not found: {id}")));
            }
            Reply::ok(v)
        }
    }
}

/// `DELETE …/{collection}/{id}`: the delete response (404 with `result: not_found`). The store is
/// made first if it isn't there, as the plugin's delete makes it.
async fn delete(ctx: &Ctx<'_>, s: Store, param: &str) -> EsResult<Reply> {
    let id = ctx.path_param(param).unwrap_or_default().to_string();
    // A deleted experiment's run stops, and stores nothing.
    if s.index == EXPERIMENTS.index {
        cancel_run(ctx.cluster, &id);
    }
    ensure(ctx.cluster, s).await?;
    let uri = format!("/{}/_doc/{}?refresh=true", s.index, crate::xpack::enc(&id));
    match internal::call_system(ctx.cluster, "DELETE", &uri, None).await {
        Ok(v) => {
            let status = if v.get("result").and_then(Value::as_str) == Some("not_found") { 404 } else { 200 };
            Ok(Reply::Body(status, v))
        }
        // The delete response of a missing document, which the plugin passes on with its 404.
        Err(e) if e.status == 404 && e.kind != "index_not_found_exception" => {
            let shards = obj! { "total" => 1, "successful" => 1, "failed" => 0 };
            Ok(Reply::Body(404, obj! { "_index" => s.index, "_id" => id, "_version" => 1, "result" => "not_found", "forced_refresh" => true, "_shards" => shards, "_seq_no" => 0, "_primary_term" => 1 }))
        }
        Err(e) => Err(e),
    }
}

/// `GET|POST …/{collection}/_search`: a search of the store, run as the plugin runs it (the store
/// is its own). Nothing in it may read another index's documents: a terms lookup, an indexed
/// shape, a stored percolated document, `more_like_this` documents or a `wrapper` would read them
/// with the store's authority.
async fn search(ctx: &Ctx<'_>, s: Store) -> EsResult<Reply> {
    let body = ctx.body()?;
    if let Some(Value::Obj(m)) = &body
        && crate::query::reads::reads_elsewhere(m)
    {
        return Err(iae("a search of the Search Relevance Workbench's store reads only the store: no terms lookup, indexed shape, stored percolated document, more_like_this documents or wrapper"));
    }
    Reply::ok(internal::call_system(ctx.cluster, "POST", &format!("/{}/_search", s.index), body.as_ref()).await?)
}

/// The query set limit (`plugins.search_relevance.query_set.maximum`, 1000 by default).
fn query_set_maximum(ctx: &Ctx) -> usize {
    ctx.state().metadata.cluster_setting(QUERY_SET_MAXIMUM).and_then(|v| v.parse().ok()).unwrap_or(1000)
}

/// The plugin's answer past the query set limit.
fn over_limit() -> Reply {
    Reply::Raw(403, "text/plain; charset=UTF-8".into(), bytes::Bytes::from_static(b"Query Set Limit Exceeded."))
}

// ------------------------------------------------------------------------------ query sets

fn query_set_doc(id: &str, name: &str, description: &str, sampling: &str, now: i64, queries: Vec<String>) -> Value {
    let qs: Vec<Value> = queries.into_iter().map(|q| obj! { "queryText" => q, "customFields" => obj! {} }).collect();
    obj! { "id" => id, "name" => name, "description" => description, "sampling" => sampling, "timestamp" => timestamp(now), "querySetQueries" => Value::Arr(qs) }
}

fn created(kind: &str, id: &str) -> EsResult<Reply> {
    let mut m = Map::new();
    m.insert(format!("{kind}_id"), Value::from(id));
    m.insert(format!("{kind}_result"), Value::from("CREATED"));
    Reply::ok(Value::Obj(m))
}

/// `PUT …/query_sets`: a `manual` set of the queries listed.
async fn put_query_set(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let m = body_of(ctx)?;
    let sampling = text(&m, "sampling").unwrap_or_else(|| "manual".into());
    if sampling != "manual" {
        return Err(srw(400, format!("Support sampling as manual only. sampling: {sampling}")));
    }
    let name = text(&m, "name").ok_or_else(|| missing("name"))?;
    let description = text(&m, "description").unwrap_or_default();
    let Some(list) = m.get("querySetQueries").and_then(Value::as_array) else { return Err(missing("querySetQueries")) };
    let queries: Vec<String> = list.iter().filter_map(|q| q.get("queryText").and_then(Value::as_str).map(str::to_string)).collect();
    if queries.len() > query_set_maximum(ctx) {
        return Ok(over_limit());
    }
    let id = uuid();
    write(ctx.cluster, Some(QUERY_SETS), QUERY_SETS.index, &id, &query_set_doc(&id, &name, &description, &sampling, ctx.now_ms, queries)).await?;
    created("query_set", &id)
}

/// `POST …/query_sets`: `querySetSize` queries sampled from `ubi_queries`' `user_query`: `topn`
/// (the most frequent), `random`, or `pptss` (probability proportional to frequency).
async fn sample_query_set(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let m = body_of(ctx)?;
    if !ctx.state().metadata.indices.contains_key(UBI_QUERIES) {
        return Err(srw(409, "UBI queries index does not exist"));
    }
    let sampling = text(&m, "sampling").unwrap_or_default();
    if !["topn", "random", "pptss"].contains(&sampling.as_str()) {
        return Err(srw(400, format!("Unknown sampler type: {sampling}")));
    }
    let name = text(&m, "name").ok_or_else(|| missing("name"))?;
    let description = text(&m, "description").unwrap_or_default();
    let size = m.get("querySetSize").and_then(Value::as_i64).unwrap_or(20).max(1) as usize;
    if size > query_set_maximum(ctx) {
        return Ok(over_limit());
    }
    // Read as the caller, as the plugin reads it: `ubi_queries` is the caller's to read or not.
    let body = obj! { "size" => 0, "aggs" => obj! { "q" => obj! { "terms" => obj! { "field" => "user_query", "size" => 10_000 } } } };
    let v = internal::call_as(ctx, ctx.cluster, "POST", &format!("/{UBI_QUERIES}/_search"), Some(&body)).await?;
    let counts: Vec<(String, u64)> = v
        .pointer(&["aggregations", "q", "buckets"])
        .and_then(Value::as_array)
        .map(|b| b.iter().filter_map(|b| Some((b.get("key")?.as_str()?.to_string(), b.get("doc_count")?.as_i64()?.max(0) as u64))).collect())
        .unwrap_or_default();
    let queries = sample(&sampling, counts, size);
    let id = uuid();
    write(ctx.cluster, Some(QUERY_SETS), QUERY_SETS.index, &id, &query_set_doc(&id, &name, &description, &sampling, ctx.now_ms, queries)).await?;
    created("query_set", &id)
}

/// `size` of the distinct queries with their counts (most frequent first, ties by text).
fn sample(sampling: &str, mut counts: Vec<(String, u64)>, size: usize) -> Vec<String> {
    use rand::Rng;
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    match sampling {
        "topn" => counts.into_iter().take(size).map(|(q, _)| q).collect(),
        "random" => {
            let mut rng = rand::thread_rng();
            let mut out = Vec::new();
            while !counts.is_empty() && out.len() < size {
                let i = rng.gen_range(0..counts.len());
                out.push(counts.swap_remove(i).0);
            }
            out
        }
        _ => {
            // Without replacement, each pick weighted by its count.
            let mut rng = rand::thread_rng();
            let mut out = Vec::new();
            while !counts.is_empty() && out.len() < size {
                let total: u64 = counts.iter().map(|c| c.1.max(1)).sum();
                let mut at = rng.gen_range(0..total);
                let i = counts.iter().position(|c| {
                    let w = c.1.max(1);
                    if at < w {
                        true
                    } else {
                        at -= w;
                        false
                    }
                });
                out.push(counts.remove(i.unwrap_or(0)).0);
            }
            out
        }
    }
}

// ------------------------------------------------------------------------------ search configurations

/// `PUT …/search_configurations`: `{name, index, query, searchPipeline?, description?}`, the query
/// a search body as text with `%SearchText%` where each query goes.
async fn put_config(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let m = body_of(ctx)?;
    let name = text(&m, "name").ok_or_else(|| missing("name"))?;
    let index = text(&m, "index").ok_or_else(|| missing("index"))?;
    let query = text(&m, "query").ok_or_else(|| missing("query"))?;
    if crate::meta::resolve::resolve(&ctx.state(), std::slice::from_ref(&index), &crate::meta::resolve::IndicesOptions::LENIENT_EXPAND_OPEN, ctx.now_ms).unwrap_or_default().is_empty() {
        return Err(srw(400, format!("Index [{index}] does not exist in the cluster")));
    }
    let id = uuid();
    let doc = obj! {
        "id" => id.clone(), "name" => name, "timestamp" => timestamp(ctx.now_ms), "index" => index, "query" => query,
        "searchPipeline" => text(&m, "searchPipeline").unwrap_or_default(), "description" => text(&m, "description").unwrap_or_default(),
    };
    write(ctx.cluster, Some(CONFIGS), CONFIGS.index, &id, &doc).await?;
    created("search_configuration", &id)
}

// ------------------------------------------------------------------------------ judgments

/// `PUT …/judgments`: `IMPORT_JUDGMENT` stores the ratings given; `UBI_JUDGMENT` builds them from
/// UBI's events (COEC) behind the answer.
async fn put_judgment(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let m = body_of(ctx)?;
    let ty = text(&m, "type").unwrap_or_default();
    let name = text(&m, "name").ok_or_else(|| missing("name"))?;
    let id = uuid();
    let now = ctx.now_ms;
    let base = |status: &str, metadata: Value, ratings: Value| {
        obj! { "id" => id.clone(), "timestamp" => timestamp(now), "name" => name.clone(), "status" => status, "type" => ty.clone(), "metadata" => metadata, "judgmentRatings" => ratings }
    };
    match ty.as_str() {
        "IMPORT_JUDGMENT" => {
            let ratings = m.get("judgmentRatings").cloned().ok_or_else(|| missing("judgmentRatings"))?;
            write(ctx.cluster, Some(JUDGMENTS), JUDGMENTS.index, &id, &base("COMPLETED", obj! {}, ratings)).await?;
        }
        "UBI_JUDGMENT" => {
            let model = text(&m, "clickModel").unwrap_or_default();
            let max_rank = m.get("maxRank").and_then(Value::as_i64).unwrap_or(20).clamp(1, 1000) as u32;
            let events = text(&m, "ubiEventsIndex");
            let (start, end) = (text(&m, "startDate").unwrap_or_default(), text(&m, "endDate").unwrap_or_default());
            let index = events.clone().unwrap_or_else(|| UBI_EVENTS.into());
            if !has_ubi_fields(ctx.cluster, &index) {
                return Err(match events {
                    None => srw(400, format!("No 'ubiEventsIndex' parameter was provided and the default UBI events index [{UBI_EVENTS}] does not exist or is missing required UBI event fields (query_id, action_name, event_attributes.object.object_id). Ingest UBI events data into [{UBI_EVENTS}], or set the 'ubiEventsIndex' parameter to an existing UBI events index.")),
                    Some(i) => srw(400, format!("The UBI events index [{i}] does not exist or is missing required UBI event fields (query_id, action_name, event_attributes.object.object_id).")),
                });
            }
            let metadata = obj! { "clickModel" => model.clone(), "endDate" => end.clone(), "ubiEventsIndex" => index.clone(), "startDate" => start.clone(), "maxRank" => max_rank };
            if model != "coec" {
                let doc = base("ERROR", obj! { "error" => format!("Unsupported click model: {model}") }, Value::Arr(Vec::new()));
                write(ctx.cluster, Some(JUDGMENTS), JUDGMENTS.index, &id, &doc).await?;
                return Reply::ok(obj! { "judgment_id" => id });
            }
            let ratings = ubi_ratings(ctx, &index, max_rank, &start, &end).await?;
            write(ctx.cluster, Some(JUDGMENTS), JUDGMENTS.index, &id, &base("COMPLETED", metadata, ratings)).await?;
        }
        "LLM_JUDGMENT" => return Err(iae("LLM_JUDGMENT isn't run by roll: it needs an ML Commons model, and it would send documents to a model provider")),
        _ => {
            return Err(iae("Invalid or missing judgment type").caused_by(iae(format!("No enum constant org.opensearch.searchrelevance.model.JudgmentType.{ty}"))));
        }
    }
    Reply::ok(obj! { "judgment_id" => id })
}

/// Whether `index` exists with the UBI event fields the plugin requires.
fn has_ubi_fields(cluster: &Cluster, index: &str) -> bool {
    let st = cluster.state();
    let Some(meta) = st.metadata.indices.get(index) else { return false };
    let m = meta.mapping_json(false);
    let has = |path: &[&str]| {
        let mut at = &m;
        for p in path {
            match at.get("properties").and_then(|x| x.get(p)) {
                Some(x) => at = x,
                None => return false,
            }
        }
        true
    };
    has(&["query_id"]) && has(&["action_name"]) && has(&["event_attributes", "object", "object_id"])
}

/// One query's UBI events: (document, position) → (impressions, clicks).
type EventCells = std::collections::HashMap<(String, u32), (u64, u64)>;

/// The ratings COEC makes of UBI events (`impression` and `click`, each with its query's text,
/// document and position), as the plugin stores them: per query in the order first met,
/// `{docId, rating}` with the rating a string to three decimals. The events are read as the
/// caller (the index is theirs to read or not), through a scroll, at most [`MAX_EVENTS`].
async fn ubi_ratings(ctx: &Ctx<'_>, index: &str, max_rank: u32, start: &str, end: &str) -> EsResult<Value> {
    let mut filter = vec![obj! { "terms" => obj! { "action_name" => Value::Arr(vec![Value::from("impression"), Value::from("click")]) } }];
    if !start.is_empty() || !end.is_empty() {
        let mut range = Map::new();
        if !start.is_empty() {
            range.insert("gte".into(), Value::from(start));
        }
        if !end.is_empty() {
            range.insert("lte".into(), Value::from(end));
        }
        filter.push(obj! { "range" => obj! { "timestamp" => Value::Obj(range) } });
    }
    let body = obj! {
        "size" => 5000, "query" => obj! { "bool" => obj! { "filter" => Value::Arr(filter) } },
        "_source" => Value::Arr(vec![Value::from("user_query"), Value::from("action_name"), Value::from("event_attributes")]),
    };
    let mut page = internal::call_as(ctx, ctx.cluster, "POST", &format!("/{}/_search?scroll=1m", crate::xpack::enc(index)), Some(&body)).await?;
    // Per query (in the order first met): its cells, (document, position) → (impressions, clicks).
    let mut order: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut queries: Vec<(String, EventCells)> = Vec::new();
    let mut read = 0usize;
    let scroll_id = loop {
        let hits = page.pointer(&["hits", "hits"]).and_then(Value::as_array).cloned().unwrap_or_default();
        for h in &hits {
            let s = h.get("_source");
            let get = |p: &[&str]| s.and_then(|s| s.pointer(p));
            let (Some(q), Some(d), Some(pos), Some(a)) = (
                get(&["user_query"]).and_then(Value::as_str),
                get(&["event_attributes", "object", "object_id"]).map(Value::to_es_string),
                get(&["event_attributes", "position", "ordinal"]).and_then(Value::as_i64),
                get(&["action_name"]).and_then(Value::as_str),
            ) else {
                continue;
            };
            if pos < 1 || pos > i64::from(u32::MAX) {
                continue;
            }
            let at = *order.entry(q.to_string()).or_insert_with(|| {
                queries.push((q.to_string(), std::collections::HashMap::new()));
                queries.len() - 1
            });
            let c = queries[at].1.entry((d, pos as u32)).or_default();
            if a == "click" { c.1 += 1 } else { c.0 += 1 }
        }
        read += hits.len();
        let id = page.get("_scroll_id").and_then(Value::as_str).map(str::to_string);
        if hits.is_empty() || read >= MAX_EVENTS {
            break id;
        }
        let Some(id) = id else { break None };
        page = internal::call_as(ctx, ctx.cluster, "POST", "/_search/scroll", Some(&obj! { "scroll" => "1m", "scroll_id" => id })).await?;
    };
    if let Some(id) = scroll_id {
        let _ = internal::call_as(ctx, ctx.cluster, "DELETE", "/_search/scroll", Some(&obj! { "scroll_id" => id })).await;
    }
    let cells = queries.iter().flat_map(|(q, cs)| cs.iter().map(move |((d, p), (i, c))| (q.as_str(), d.as_str(), *p, *i, *c)));
    let judged = crate::analytics::coec(cells, max_rank, |_| true);
    let mut by_query: Vec<Vec<&crate::analytics::Judged>> = vec![Vec::new(); queries.len()];
    for j in &judged {
        if let Some(i) = order.get(&j.query) {
            by_query[*i].push(j);
        }
    }
    let out: Vec<Value> = queries
        .iter()
        .zip(by_query)
        .filter(|(_, rs)| !rs.is_empty())
        .map(|((q, _), mut rs)| {
            rs.sort_by(|a, b| a.doc.cmp(&b.doc));
            let ratings: Vec<Value> = rs.iter().map(|j| obj! { "docId" => j.doc.clone(), "rating" => format!("{:.3}", j.coec) }).collect();
            obj! { "query" => q.clone(), "ratings" => Value::Arr(ratings) }
        })
        .collect();
    Ok(Value::Arr(out))
}

// ------------------------------------------------------------------------------ experiments

/// `PUT …/experiments`: `{querySetId, searchConfigurationList, judgmentList?, size, type}`, run
/// behind the answer.
async fn put_experiment(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let m = body_of(ctx)?;
    let ty = text(&m, "type").unwrap_or_default();
    match ty.as_str() {
        "POINTWISE_EVALUATION" | "PAIRWISE_COMPARISON" => {}
        "HYBRID_OPTIMIZER" => return Err(iae("HYBRID_OPTIMIZER experiments aren't run by roll yet: they need neural-search hybrid query pipelines")),
        _ => return Err(iae("Invalid or missing experiment type")),
    }
    let ids = |k: &str| -> Vec<String> { m.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default() };
    let qs_id = text(&m, "querySetId").ok_or_else(|| missing("querySetId"))?;
    let configs = ids("searchConfigurationList");
    match (ty.as_str(), configs.len()) {
        ("POINTWISE_EVALUATION", n) if n != 1 => return Err(srw(400, "POINTWISE_EVALUATION requires exactly 1 search configuration")),
        ("PAIRWISE_COMPARISON", n) if n != 2 => return Err(srw(400, "PAIRWISE_COMPARISON requires exactly 2 search configurations")),
        _ => {}
    }
    let judgments = ids("judgmentList");
    let size = m.get("size").and_then(Value::as_i64).unwrap_or(10).clamp(1, 1000) as usize;
    let Some(qs) = read(ctx.cluster, QUERY_SETS, &qs_id).await? else { return Err(srw(400, format!("QuerySet [{qs_id}] does not exist"))) };
    let mut confs = Vec::new();
    for c in &configs {
        match read(ctx.cluster, CONFIGS, c).await? {
            Some(s) => confs.push((c.clone(), s)),
            None => return Err(srw(400, format!("SearchConfig [{c}] does not exist"))),
        }
    }
    let mut lists = Vec::new();
    for j in &judgments {
        match read(ctx.cluster, JUDGMENTS, j).await? {
            Some(s) => lists.push(s),
            None => return Err(srw(400, format!("Judgment [{j}] does not exist"))),
        }
    }
    if ty == "POINTWISE_EVALUATION" && lists.is_empty() {
        return Err(iae("POINTWISE_EVALUATION needs a judgmentList"));
    }
    let id = uuid();
    let signature = obj! {
        "query_set" => sha256_hex(&Value::Obj(qs.clone()).to_string()),
        "judgment_list" => sha256_hex(&Value::Arr(lists.iter().cloned().map(Value::Obj).collect()).to_string()),
        "search_configurations" => sha256_hex(&Value::Arr(confs.iter().map(|(_, c)| Value::Obj(c.clone())).collect()).to_string()),
    };
    let mut doc = obj! {
        "id" => id.clone(), "timestamp" => timestamp(ctx.now_ms), "name" => format!("{ty}-{}", &id[..8]), "type" => ty.clone(), "status" => "PROCESSING",
        "querySetId" => qs_id, "searchConfigurationList" => Value::Arr(configs.into_iter().map(Value::from).collect()),
        "judgmentList" => Value::Arr(judgments.iter().cloned().map(Value::from).collect()), "size" => size,
        "isScheduled" => false, "scheduledExperimentJobId" => Value::Null, "results" => Value::Arr(Vec::new()), "inputSignature" => signature,
    };
    if running(ctx.cluster) >= MAX_RUNNING {
        return Err(EsError::new(429, "rejected_execution_exception", format!("{MAX_RUNNING} Search Relevance Workbench experiments are running on this cluster: start another when one ends")));
    }
    write(ctx.cluster, Some(EXPERIMENTS), EXPERIMENTS.index, &id, &doc).await?;
    // Registered as a task until it ends: it keeps the cluster loaded, and a delete (or the node
    // going) cancels it.
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task = ctx.cluster.tasks.start_detached(crate::cluster::NewTask {
        action: EXPERIMENT_ACTION.into(),
        description: run_description(&id),
        describe: None,
        parent: None,
        headers: Vec::new(),
        cancellable: true,
        cancelled: Some(cancelled.clone()),
    });
    let run = Run {
        id: id.clone(),
        ty,
        queries: qs.get("querySetQueries").and_then(Value::as_array).map(|a| a.iter().filter_map(|q| q.get("queryText").and_then(Value::as_str).map(str::to_string)).collect()).unwrap_or_default(),
        confs,
        judgments,
        ratings: lists,
        size,
        owner: Some(crate::xpack::security::principal(ctx).clone()),
        cancelled,
        task,
    };
    match ctx.cluster.handle() {
        Some(cluster) => {
            tokio::spawn(async move {
                let mut doc = doc;
                finish(&cluster, run, &mut doc).await;
            });
        }
        None => finish(ctx.cluster, run, &mut doc).await,
    }
    created("experiment", &id)
}

/// One experiment to run.
struct Run {
    id: String,
    ty: String,
    queries: Vec<String>,
    /// (id, stored configuration).
    confs: Vec<(String, Map)>,
    judgments: Vec<String>,
    ratings: Vec<Map>,
    size: usize,
    owner: Option<crate::xpack::security::Principal>,
    /// Its task's cancellation (a delete, the node going).
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    task: u64,
}

/// The task description an experiment's run is registered under.
fn run_description(id: &str) -> String {
    format!("search relevance experiment [{id}]")
}

/// Experiments running on the cluster.
fn running(cluster: &Cluster) -> usize {
    cluster.tasks.list().iter().filter(|t| t.action == EXPERIMENT_ACTION).count()
}

/// Cancels experiment `id`'s run, if one is running.
fn cancel_run(cluster: &Cluster, id: &str) {
    let description = run_description(id);
    for t in cluster.tasks.list().iter().filter(|t| t.action == EXPERIMENT_ACTION && t.description == description) {
        t.cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Ends the run's task however its future ends.
struct Ending<'a>(&'a Cluster, u64);

impl Drop for Ending<'_> {
    fn drop(&mut self) {
        self.0.tasks.end(self.1);
    }
}

/// What a run makes besides its experiment's own document: evaluation results and variants.
#[derive(Default)]
struct Written {
    results: Vec<(String, Value)>,
    variants: Vec<(String, Value)>,
}

/// Runs `run` and stores its results and status in `doc`. A cancelled run stores nothing (a
/// deleted experiment stays deleted; one the node stopped is marked when the cluster loads
/// again). The experiment is updated in place, only while it exists, before what its results
/// point at is written, in one bulk with one refresh.
async fn finish(cluster: &Cluster, run: Run, doc: &mut Value) {
    let _ending = Ending(cluster, run.task);
    let mut written = Written::default();
    let outcome = match run.ty.as_str() {
        "PAIRWISE_COMPARISON" => pairwise(cluster, &run).await,
        _ => pointwise(cluster, &run, &mut written).await,
    };
    if run.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let Value::Obj(d) = doc else { return };
    match outcome {
        Ok(results) => {
            d.insert("status".into(), Value::from("COMPLETED"));
            d.insert("results".into(), results);
        }
        Err(e) => {
            d.insert("status".into(), Value::from("ERROR"));
            d.insert("results".into(), Value::Arr(vec![obj! { "error" => e.reason().to_string() }]));
            written = Written::default();
        }
    }
    let uri = format!("/{}/_update/{}?refresh=true", EXPERIMENTS.index, crate::xpack::enc(&run.id));
    if internal::call_system(cluster, "POST", &uri, Some(&obj! { "doc" => Value::Obj(d.clone()) })).await.is_err() {
        return;
    }
    if written.results.is_empty() && written.variants.is_empty() {
        return;
    }
    if ensure(cluster, RESULTS).await.is_err() || ensure(cluster, VARIANTS).await.is_err() {
        return;
    }
    let mut ndjson = String::new();
    for (index, docs) in [(RESULTS.index, &written.results), (VARIANTS.index, &written.variants)] {
        for (id, doc) in docs {
            ndjson.push_str(&obj! { "index" => obj! { "_index" => index, "_id" => id.clone() } }.to_json());
            ndjson.push('\n');
            ndjson.push_str(&doc.to_json());
            ndjson.push('\n');
        }
    }
    let _ = internal::call_ndjson_system(cluster, "/_bulk?refresh=true", ndjson).await;
}

/// The ids configuration `conf` finds for query `q`: its query text with `%SearchText%` replaced
/// (as JSON string content), `size` hits, run as the experiment's owner.
async fn search_ids(cluster: &Cluster, run: &Run, conf: &Map, q: &str) -> EsResult<Vec<String>> {
    if run.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(EsError::new(400, "task_cancelled_exception", "cancelled"));
    }
    // Its maker may have lost their access since: each search is theirs, checked as it runs.
    if let Some(e) = internal::principal_refusal(cluster, run.owner.as_ref()) {
        return Err(e);
    }
    let index = conf.get("index").and_then(Value::as_str).unwrap_or_default();
    let template = conf.get("query").and_then(Value::as_str).unwrap_or_default();
    let escaped = Value::from(q).to_string();
    let text = template.replace("%SearchText%", &escaped[1..escaped.len() - 1]);
    let mut body = match crate::json::parse(text.as_bytes()) {
        Ok(Some(Value::Obj(b))) => b,
        _ => return Err(iae(format!("the search configuration's query isn't a JSON object for [{q}]"))),
    };
    body.insert("size".into(), Value::from(run.size));
    body.insert("_source".into(), Value::from(false));
    let pipeline = conf.get("searchPipeline").and_then(Value::as_str).filter(|p| !p.is_empty());
    let uri = match pipeline {
        Some(p) => format!("/{}/_search?search_pipeline={}", crate::xpack::enc(index), crate::xpack::enc(p)),
        None => format!("/{}/_search", crate::xpack::enc(index)),
    };
    let v = internal::as_principal(run.owner.clone(), internal::call(cluster, "POST", &uri, Some(&Value::Obj(body)))).await?;
    Ok(v.pointer(&["hits", "hits"]).and_then(Value::as_array).map(|h| h.iter().filter_map(|h| h.get("_id").and_then(Value::as_str).map(str::to_string)).collect()).unwrap_or_default())
}

/// The judgment lists' ratings of query `q`: document → rating (a later list's wins).
fn ratings_of(run: &Run, q: &str) -> Vec<(String, f64)> {
    let mut out: Vec<(String, f64)> = Vec::new();
    for l in &run.ratings {
        for jr in l.get("judgmentRatings").and_then(Value::as_array).into_iter().flatten() {
            if jr.get("query").and_then(Value::as_str) != Some(q) {
                continue;
            }
            for r in jr.get("ratings").and_then(Value::as_array).into_iter().flatten() {
                let Some(d) = r.get("docId").map(Value::to_es_string) else { continue };
                let rating = r.get("rating").and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0.0);
                match out.iter_mut().find(|(x, _)| *x == d) {
                    Some(e) => e.1 = rating,
                    None => out.push((d, rating)),
                }
            }
        }
    }
    out
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// The plugin's pointwise metrics of `found` (best first) against `rated`, at `k`.
pub fn pointwise_metrics(found: &[String], rated: &[(String, f64)], k: usize) -> Vec<(String, f64)> {
    let rating = |d: &str| rated.iter().find(|(x, _)| x == d).map(|(_, r)| *r);
    let max = rated.iter().map(|(_, r)| *r).fold(f64::NEG_INFINITY, f64::max);
    let relevant = |r: f64| max > 0.0 && r >= max;
    let relevant_total = rated.iter().filter(|(_, r)| relevant(*r)).count();
    let n = found.len();
    let judged = found.iter().filter(|d| rating(d).is_some()).count();
    let mut hits = 0usize;
    let mut ap = 0.0;
    let mut rr = 0.0;
    let mut dcg = 0.0;
    for (i, d) in found.iter().enumerate() {
        let r = rating(d).unwrap_or(0.0);
        if relevant(r) {
            hits += 1;
            ap += hits as f64 / (i + 1) as f64;
            if rr == 0.0 {
                rr = 1.0 / (i + 1) as f64;
            }
        }
        dcg += (2f64.powf(r) - 1.0) / ((i + 2) as f64).log2();
    }
    let mut ideal: Vec<f64> = rated.iter().map(|(_, r)| *r).collect();
    ideal.sort_by(|a, b| b.total_cmp(a));
    let idcg: f64 = ideal.iter().take(k).enumerate().map(|(i, r)| (2f64.powf(*r) - 1.0) / ((i + 2) as f64).log2()).sum();
    let ratio = |a: f64, b: f64| if b > 0.0 { a / b } else { 0.0 };
    vec![
        (format!("Coverage@{k}"), round2(ratio(judged as f64, n as f64))),
        (format!("Precision@{k}"), round2(ratio(hits as f64, n as f64))),
        (format!("Recall@{k}"), round2(ratio(hits as f64, relevant_total as f64))),
        (format!("MAP@{k}"), round2(ratio(ap, relevant_total as f64))),
        ("MRR".to_string(), round2(rr)),
        (format!("DCG@{k}"), round2(dcg)),
        (format!("NDCG@{k}"), round2(ratio(dcg, idcg))),
    ]
}

async fn pointwise(cluster: &Cluster, run: &Run, written: &mut Written) -> EsResult<Value> {
    let mut results = Vec::new();
    for q in &run.queries {
        let rated = ratings_of(run, q);
        for (cid, conf) in &run.confs {
            let found = search_ids(cluster, run, conf, q).await?;
            let metrics: Vec<Value> = pointwise_metrics(&found, &rated, run.size).into_iter().map(|(m, v)| obj! { "metric" => m, "value" => v }).collect();
            let (eval_id, variant_id) = (uuid(), uuid());
            let now = crate::time::now_millis();
            let result = obj! {
                "id" => eval_id.clone(), "timestamp" => timestamp(now), "searchConfigurationId" => cid.clone(), "searchText" => q.clone(),
                "judgmentIds" => Value::Arr(run.judgments.iter().cloned().map(Value::from).collect()),
                "documentIds" => Value::Arr(found.into_iter().map(Value::from).collect()), "metrics" => Value::Arr(metrics),
                "experimentId" => run.id.clone(), "experimentVariantId" => variant_id.clone(),
            };
            written.results.push((eval_id.clone(), result));
            let variant = obj! {
                "id" => variant_id.clone(), "timestamp" => timestamp(now), "type" => run.ty.clone(), "status" => "COMPLETED", "experimentId" => run.id.clone(),
                "parameters" => obj! { "searchConfigId" => cid.clone(), "searchPipeline" => conf.get("searchPipeline").cloned().unwrap_or(Value::from("")) }, "results" => obj! { "evaluationResultId" => eval_id.clone() },
            };
            written.variants.push((variant_id.clone(), variant));
            results.push(obj! { "evaluationId" => eval_id, "queryText" => q.clone(), "searchConfigurationId" => cid.clone() });
        }
    }
    Ok(Value::Arr(results))
}

/// Rank-biased overlap of `a` and `b` at persistence `p` over the longer list's depth `k`,
/// normalized by the most it can be at that depth (`1 - p^k`), as the plugin computes it.
fn rbo(a: &[String], b: &[String], p: f64) -> f64 {
    let depth = a.len().max(b.len());
    if depth == 0 {
        return 1.0;
    }
    let mut sum = 0.0;
    for d in 1..=depth {
        let sa: std::collections::HashSet<&String> = a.iter().take(d).collect();
        let overlap = b.iter().take(d).filter(|x| sa.contains(x)).count();
        sum += p.powi(d as i32 - 1) * overlap as f64 / d as f64;
    }
    (1.0 - p) * sum / (1.0 - p.powi(depth as i32))
}

/// The plugin's pairwise metrics of two result lists.
pub fn pairwise_metrics(a: &[String], b: &[String]) -> Vec<(String, f64)> {
    let sa: std::collections::HashSet<&String> = a.iter().collect();
    let sb: std::collections::HashSet<&String> = b.iter().collect();
    let union = sa.union(&sb).count();
    let inter = sa.intersection(&sb).count();
    let jaccard = if union == 0 { 1.0 } else { inter as f64 / union as f64 };
    // Each document counted once per list it's in: the shared ones over all of them.
    let all = sa.len() + sb.len();
    let frequency = if all == 0 { 1.0 } else { (2 * inter) as f64 / all as f64 };
    vec![
        ("jaccard".into(), round2(jaccard)),
        ("rbo50".into(), round2(rbo(a, b, 0.5))),
        ("rbo90".into(), round2(rbo(a, b, 0.9))),
        ("frequencyWeighted".into(), round2(frequency)),
    ]
}

async fn pairwise(cluster: &Cluster, run: &Run) -> EsResult<Value> {
    let mut results = Vec::new();
    for q in &run.queries {
        let mut lists = Vec::new();
        for (cid, conf) in &run.confs {
            lists.push((cid.clone(), search_ids(cluster, run, conf, q).await?));
        }
        let metrics = match lists.as_slice() {
            [(_, a), (_, b)] => pairwise_metrics(a, b),
            _ => Vec::new(),
        };
        let snapshots: Vec<Value> = lists.into_iter().map(|(cid, ids)| obj! { "searchConfigurationId" => cid, "docIds" => Value::Arr(ids.into_iter().map(Value::from).collect()) }).collect();
        results.push(obj! {
            "snapshots" => Value::Arr(snapshots),
            "metrics" => Value::Arr(metrics.into_iter().map(|(m, v)| obj! { "metric" => m, "value" => v }).collect()),
            "query_text" => q.clone(),
        });
    }
    Ok(Value::Arr(results))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// OpenSearch 3.9.0's numbers for these hits and ratings (see the module doc's fixture).
    #[test]
    fn pointwise_metrics_are_the_plugins() {
        let get = |found: &[&str], rated: &[(&str, f64)], k| pointwise_metrics(&ids(found), &rated.iter().map(|(d, r)| (d.to_string(), *r)).collect::<Vec<_>>(), k);
        let v = get(&["1", "2", "3", "4", "5", "6"], &[("1", 3.0), ("2", 1.0)], 10);
        assert_eq!(v.iter().map(|(_, x)| *x).collect::<Vec<_>>(), [0.33, 0.17, 1.0, 1.0, 1.0, 7.63, 1.0]);
        let v = get(&["1", "2", "3"], &[("1", 0.0), ("2", 1.0), ("3", 2.0), ("4", 3.0), ("9", 3.0)], 3);
        assert_eq!(v.iter().map(|(_, x)| *x).collect::<Vec<_>>(), [1.0, 0.0, 0.0, 0.0, 0.0, 2.13, 0.16]);
        let v = get(&["3", "1", "2"], &[("3", 0.5), ("1", 1.4)], 3);
        assert_eq!(v.iter().map(|(_, x)| *x).collect::<Vec<_>>(), [0.67, 0.33, 1.0, 0.5, 0.5, 1.45, 0.76]);
        let v = get(&["1", "2", "3"], &[("1", 3.0), ("2", 2.9), ("3", 1.0)], 3);
        assert_eq!(v.iter().map(|(_, x)| *x).collect::<Vec<_>>(), [1.0, 0.33, 1.0, 1.0, 1.0, 11.58, 1.0]);
    }

    /// A running experiment is a task until it ends (a task keeps its node from unloading the
    /// cluster: `Loaded::pinned`), and deleting it cancels it: it stores nothing and stays deleted.
    #[tokio::test(flavor = "current_thread")]
    async fn a_running_experiment_is_a_task_a_delete_cancels() {
        let cfg = crate::ClusterConfig { dialect: crate::cluster::Dialect::OpenSearch, unsealed_secrets: true, ..crate::ClusterConfig::new("t") };
        let c = std::sync::Arc::new(Cluster::open(cfg, std::sync::Arc::new(crate::MemoryStore::new())).await.unwrap());
        c.enable_background_tasks();
        let call = |m: &str, u: &str, b: &str| {
            let (c, req) = (c.clone(), crate::EsRequest::json(m, u, b.to_string()));
            async move {
                let r = crate::handle(&c, req).await;
                (r.status, crate::json::parse(&r.body).ok().flatten().unwrap_or(Value::Null))
            }
        };
        let get = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_else(|| panic!("{v}")).to_string();
        assert_eq!(call("PUT", "/p/_doc/1?refresh=true", r#"{"t":"a"}"#).await.0, 201);
        let qs = get(&call("PUT", "/_plugins/_search_relevance/query_sets", r#"{"name":"q","description":"d","sampling":"manual","querySetQueries":[{"queryText":"a"}]}"#).await.1, "query_set_id");
        let conf = get(&call("PUT", "/_plugins/_search_relevance/search_configurations", r#"{"name":"c","index":"p","query":"{\"query\":{\"match_all\":{}}}"}"#).await.1, "search_configuration_id");
        let body = format!(r#"{{"querySetId":"{qs}","searchConfigurationList":["{conf}","{conf}"],"size":3,"type":"PAIRWISE_COMPARISON"}}"#);
        let status = |id: String| {
            let c = c.clone();
            async move { read(&c, EXPERIMENTS, &id).await.unwrap().and_then(|d| d.get("status").and_then(Value::as_str).map(str::to_string)) }
        };
        let settle = || async {
            while running(&c) > 0 {
                tokio::task::yield_now().await;
            }
        };
        // Spawned behind the answer: on this one thread it hasn't run yet, and its task is there.
        let kept = get(&call("PUT", "/_plugins/_search_relevance/experiments", &body).await.1, "experiment_id");
        assert_eq!(running(&c), 1, "registered before it runs");
        assert!(!c.tasks.is_empty());
        settle().await;
        assert_eq!(status(kept).await.as_deref(), Some("COMPLETED"));
        let gone = get(&call("PUT", "/_plugins/_search_relevance/experiments", &body).await.1, "experiment_id");
        assert_eq!(call("DELETE", &format!("/_plugins/_search_relevance/experiments/{gone}"), "").await.0, 200);
        settle().await;
        assert_eq!(status(gone).await, None, "a deleted experiment stays deleted");
    }

    #[test]
    fn pairwise_metrics_are_the_plugins() {
        let a = ids(&["1", "2", "3"]);
        assert!(pairwise_metrics(&a, &a).iter().all(|(_, v)| *v == 1.0));
        let v = pairwise_metrics(&ids(&["1", "2", "3", "4"]), &ids(&["6", "5", "4", "3"]));
        assert_eq!(v.iter().map(|(_, x)| *x).collect::<Vec<_>>(), [0.33, 0.03, 0.11, 0.5]);
    }
}
