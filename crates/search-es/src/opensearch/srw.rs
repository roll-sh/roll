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
//! **Stats** (`…/stats`, [`Stats`]): the plugin's seven counters, since the cluster loaded, as its
//! node's `timestamped_event_counter`s (`include_metadata`), and the cluster version.
//!
//! **An experiment a node stopped running** (unloaded, or taken over) stays `PROCESSING` on the
//! plugin. roll marks it `ERROR` when its cluster loads again ([`fail_interrupted`]): nothing runs
//! it any more.
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
            | Api::SearchRelevanceStats
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
        Api::SearchRelevanceStats => stats(ctx),
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

/// `GET|POST …/{collection}/_search`: a search of the store.
async fn search(ctx: &Ctx<'_>, s: Store) -> EsResult<Reply> {
    let body = ctx.body()?;
    Reply::ok(internal::call_system(ctx.cluster, "POST", &format!("/{}/_search", s.index), body.as_ref()).await?)
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
    let size = m.get("querySetSize").and_then(Value::as_i64).unwrap_or(20).clamp(1, 1000) as usize;
    let body = obj! { "size" => 0, "aggs" => obj! { "q" => obj! { "terms" => obj! { "field" => "user_query", "size" => 10_000 } } } };
    let v = internal::call_system(ctx.cluster, "POST", &format!("/{UBI_QUERIES}/_search"), Some(&body)).await?;
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
            ctx.cluster.srw.count(Stat::ImportJudgments);
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
            let ratings = ubi_ratings(ctx.cluster, &index, max_rank, &start, &end).await?;
            write(ctx.cluster, Some(JUDGMENTS), JUDGMENTS.index, &id, &base("COMPLETED", metadata, ratings)).await?;
            ctx.cluster.srw.count(Stat::UbiJudgments);
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

/// The ratings COEC makes of UBI events (`impression` and `click`, each with its query's text,
/// document and position), as the plugin stores them: per query, `{docId, rating}` with the rating
/// a string to three decimals.
async fn ubi_ratings(cluster: &Cluster, index: &str, max_rank: u32, start: &str, end: &str) -> EsResult<Value> {
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
    let mut events: Vec<(String, String, u32, bool)> = Vec::new();
    let mut after: Option<Value> = None;
    loop {
        let mut body = obj! {
            "size" => 5000, "query" => obj! { "bool" => obj! { "filter" => Value::Arr(filter.clone()) } },
            "sort" => Value::Arr(vec![obj! { "_doc" => "asc" }]),
            "_source" => Value::Arr(vec![Value::from("user_query"), Value::from("query_id"), Value::from("action_name"), Value::from("event_attributes")]),
        };
        if let (Some(a), Value::Obj(b)) = (after.take(), &mut body) {
            b.insert("search_after".into(), a);
        }
        let v = internal::call_system(cluster, "POST", &format!("/{}/_search", crate::xpack::enc(index)), Some(&body)).await?;
        let hits = v.pointer(&["hits", "hits"]).and_then(Value::as_array).cloned().unwrap_or_default();
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
            if pos >= 1 {
                events.push((q.to_string(), d, pos as u32, a == "click"));
            }
        }
        if hits.len() < 5000 || events.len() >= MAX_EVENTS {
            break;
        }
        after = hits.last().and_then(|h| h.get("sort")).cloned();
    }
    let mut order: Vec<String> = Vec::new();
    let mut cells: std::collections::HashMap<(String, String, u32), (u64, u64)> = std::collections::HashMap::new();
    for (q, d, p, click) in events {
        if !order.contains(&q) {
            order.push(q.clone());
        }
        let c = cells.entry((q, d, p)).or_default();
        if click {
            c.1 += 1;
        } else {
            c.0 += 1;
        }
    }
    let judged = crate::analytics::coec(cells.iter().map(|((q, d, p), (i, c))| (q.as_str(), d.as_str(), *p, *i, *c)), max_rank, |_| true);
    let out: Vec<Value> = order
        .iter()
        .filter_map(|q| {
            let mut rs: Vec<&crate::analytics::Judged> = judged.iter().filter(|j| &j.query == q).collect();
            if rs.is_empty() {
                return None;
            }
            rs.sort_by(|a, b| a.doc.cmp(&b.doc));
            let ratings: Vec<Value> = rs.iter().map(|j| obj! { "docId" => j.doc.clone(), "rating" => format!("{:.3}", j.coec) }).collect();
            Some(obj! { "query" => q.clone(), "ratings" => Value::Arr(ratings) })
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
    write(ctx.cluster, Some(EXPERIMENTS), EXPERIMENTS.index, &id, &doc).await?;
    ctx.cluster.srw.count(Stat::Experiments);
    ctx.cluster.srw.count(if ty == "PAIRWISE_COMPARISON" { Stat::Pairwise } else { Stat::Pointwise });
    let run = Run {
        id: id.clone(),
        ty,
        queries: qs.get("querySetQueries").and_then(Value::as_array).map(|a| a.iter().filter_map(|q| q.get("queryText").and_then(Value::as_str).map(str::to_string)).collect()).unwrap_or_default(),
        confs,
        judgments,
        ratings: lists,
        size,
        owner: Some(crate::xpack::security::principal(ctx).clone()),
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
}

/// Runs `run` and stores its results and status in `doc`.
async fn finish(cluster: &Cluster, run: Run, doc: &mut Value) {
    let outcome = match run.ty.as_str() {
        "PAIRWISE_COMPARISON" => pairwise(cluster, &run).await,
        _ => pointwise(cluster, &run).await,
    };
    if let Value::Obj(d) = doc {
        match outcome {
            Ok(results) => {
                d.insert("status".into(), Value::from("COMPLETED"));
                d.insert("results".into(), results);
            }
            Err(e) => {
                d.insert("status".into(), Value::from("ERROR"));
                d.insert("results".into(), Value::Arr(vec![obj! { "error" => e.reason().to_string() }]));
            }
        }
    }
    let _ = write(cluster, Some(EXPERIMENTS), EXPERIMENTS.index, &run.id, doc).await;
}

/// The ids configuration `conf` finds for query `q`: its query text with `%SearchText%` replaced
/// (as JSON string content), `size` hits, run as the experiment's owner.
async fn search_ids(cluster: &Cluster, run: &Run, conf: &Map, q: &str) -> EsResult<Vec<String>> {
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
    let uri = format!("/{}/_search", crate::xpack::enc(index));
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

async fn pointwise(cluster: &Cluster, run: &Run) -> EsResult<Value> {
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
            write(cluster, Some(RESULTS), RESULTS.index, &eval_id, &result).await?;
            let variant = obj! {
                "id" => variant_id.clone(), "timestamp" => timestamp(now), "type" => run.ty.clone(), "status" => "COMPLETED", "experimentId" => run.id.clone(),
                "parameters" => obj! { "searchConfigId" => cid.clone(), "searchPipeline" => "" }, "results" => obj! { "evaluationResultId" => eval_id.clone() },
            };
            write(cluster, Some(VARIANTS), VARIANTS.index, &variant_id, &variant).await?;
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

// ------------------------------------------------------------------------------ stats

/// One of the plugin's counters, in the order its stats answer lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stat {
    ImportJudgments,
    LlmJudgments,
    UbiJudgments,
    HybridOptimizer,
    Pairwise,
    Experiments,
    Pointwise,
}

/// (group, name) of each [`Stat`].
const STATS: [(Stat, &str, &str); 7] = [
    (Stat::ImportJudgments, "judgments", "import_judgment_rating_generations"),
    (Stat::LlmJudgments, "judgments", "llm_judgment_rating_generations"),
    (Stat::UbiJudgments, "judgments", "ubi_judgment_rating_generations"),
    (Stat::HybridOptimizer, "experiments", "experiment_hybrid_optimizer_executions"),
    (Stat::Pairwise, "experiments", "experiment_pairwise_comparison_executions"),
    (Stat::Experiments, "experiments", "experiment_executions"),
    (Stat::Pointwise, "experiments", "experiment_pointwise_evaluation_executions"),
];
/// The info statistic.
const CLUSTER_VERSION: &str = "cluster_version";
/// The window a counter's `trailing_interval_value` counts (its last complete one).
const INTERVAL_MS: i64 = 5 * 60_000;
/// The setting that turns the stats route off.
const STATS_ENABLED: &str = "plugins.search_relevance.stats_enabled";

/// One `timestamped_event_counter`: its value, its last event, and the events of its current and
/// previous windows.
#[derive(Clone, Copy, Debug, Default)]
struct Counter {
    value: u64,
    last_ms: Option<i64>,
    window: i64,
    in_window: u64,
    in_previous: u64,
}

/// The Workbench's counters since the cluster loaded.
#[derive(Debug, Default)]
pub struct Stats {
    counters: parking_lot::Mutex<[Counter; 7]>,
}

impl Stats {
    pub fn count(&self, stat: Stat) {
        let now = crate::time::now_millis();
        let i = STATS.iter().position(|(s, _, _)| *s == stat).unwrap_or(0);
        let mut c = self.counters.lock();
        let c = &mut c[i];
        let window = now.div_euclid(INTERVAL_MS);
        if window != c.window {
            c.in_previous = if window == c.window + 1 { c.in_window } else { 0 };
            c.in_window = 0;
            c.window = window;
        }
        c.value += 1;
        c.in_window += 1;
        c.last_ms = Some(now);
    }

    /// (value, events in the last complete window, minutes since the last event) of each counter.
    fn read(&self, now: i64) -> [(u64, u64, i64); 7] {
        let c = self.counters.lock();
        let window = now.div_euclid(INTERVAL_MS);
        std::array::from_fn(|i| {
            let c = c[i];
            let trailing = if c.window == window { c.in_previous } else if c.window + 1 == window { c.in_window } else { 0 };
            (c.value, trailing, (now - c.last_ms.unwrap_or(0)).max(0) / 60_000)
        })
    }
}

/// `GET …/stats[/{stat}]` and `GET …/{node_id}/stats[/{stat}]`: the counters of this node (the
/// only one), under `all_nodes` and the node's id; `stat` keeps only those named.
fn stats(ctx: &Ctx<'_>) -> EsResult<Reply> {
    if ctx.state().metadata.cluster_setting(STATS_ENABLED) == Some("false") {
        return Ok(Reply::Raw(403, "text/plain; charset=UTF-8".into(), bytes::Bytes::from_static(b"Search Relevance Workbench stats is disabled")));
    }
    let wanted: Option<Vec<String>> = ctx.path_param("stat").map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect());
    if let Some(w) = &wanted {
        let known: std::collections::BTreeSet<String> = STATS.iter().map(|(_, _, n)| n.to_string()).chain([CLUSTER_VERSION.to_string()]).collect();
        let unknown: std::collections::BTreeSet<String> = w.iter().filter(|x| !known.contains(*x)).cloned().collect();
        if !unknown.is_empty() {
            return Err(iae(crate::rest::params::unrecognized(&ctx.path, &unknown, &known, "stat")));
        }
    }
    let keep = |name: &str| wanted.as_ref().is_none_or(|w| w.iter().any(|x| x == name));
    let flag = |k: &str| matches!(ctx.query(k), Some("true") | Some(""));
    let (metadata, flat) = (flag("include_metadata"), flag("flat_stat_paths"));
    let now = crate::time::now_millis();
    let values = ctx.cluster.srw.read(now);
    let mut groups = Map::new();
    for (i, (_, group, name)) in STATS.iter().enumerate() {
        if !keep(name) {
            continue;
        }
        let (value, trailing, minutes) = values[i];
        let v = if metadata {
            obj! { "value" => value, "stat_type" => "timestamped_event_counter", "trailing_interval_value" => trailing, "minutes_since_last_event" => minutes }
        } else {
            Value::from(value)
        };
        if flat {
            groups.insert(format!("{group}.{name}"), v);
        } else if let Some(Value::Obj(g)) = groups.get_mut(*group) {
            g.insert(name.to_string(), v);
        } else {
            let mut g = Map::new();
            g.insert(name.to_string(), v);
            groups.insert(group.to_string(), Value::Obj(g));
        }
    }
    let mut info = Map::new();
    if keep(CLUSTER_VERSION) {
        let version = Value::from(crate::version::OPENSEARCH_VERSION);
        info.insert(CLUSTER_VERSION.into(), if metadata { obj! { "value" => version, "stat_type" => "info_string" } } else { version });
    }
    let mut nodes = Map::new();
    nodes.insert(ctx.cluster.node.id.clone(), Value::Obj(groups.clone()));
    Reply::ok(obj! {
        "_nodes" => obj! { "total" => 1, "successful" => 1, "failed" => 0 },
        "cluster_name" => ctx.cluster.name.clone(),
        "info" => Value::Obj(info),
        "all_nodes" => Value::Obj(groups),
        "nodes" => Value::Obj(nodes),
    })
}

// ------------------------------------------------------------------------------ interrupted runs

/// Marks `ERROR` the experiments still `PROCESSING` when the cluster loads: their runs ended with
/// the node that ran them (behind the answer), and nothing runs them again.
pub fn fail_interrupted(cluster: &Cluster) {
    if cluster.dialect != crate::cluster::Dialect::OpenSearch || !cluster.state().metadata.indices.contains_key(EXPERIMENTS.index) {
        return;
    }
    let Some(c) = cluster.handle() else { return };
    tokio::spawn(async move {
        let _ = fail_interrupted_now(&c).await;
    });
}

/// [`fail_interrupted`]'s work: how many it marked.
pub async fn fail_interrupted_now(cluster: &Cluster) -> EsResult<usize> {
    let body = obj! { "size" => LIST_SIZE, "query" => obj! { "term" => obj! { "status" => "PROCESSING" } } };
    let v = internal::call_system(cluster, "POST", &format!("/{}/_search", EXPERIMENTS.index), Some(&body)).await?;
    let mut n = 0;
    for h in v.pointer(&["hits", "hits"]).and_then(Value::as_array).cloned().unwrap_or_default() {
        let (Some(id), Some(Value::Obj(mut doc))) = (h.get("_id").and_then(Value::as_str).map(str::to_string), h.get("_source").cloned()) else { continue };
        doc.insert("status".into(), Value::from("ERROR"));
        write(cluster, None, EXPERIMENTS.index, &id, &Value::Obj(doc)).await?;
        n += 1;
    }
    Ok(n)
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

    /// A run its node stopped (left `PROCESSING`) is marked `ERROR` when the cluster loads; a
    /// finished one isn't touched.
    #[tokio::test]
    async fn an_interrupted_experiment_is_marked_error() {
        let cfg = crate::ClusterConfig { dialect: crate::cluster::Dialect::OpenSearch, unsealed_secrets: true, ..crate::ClusterConfig::new("t") };
        let c = Cluster::open(cfg, std::sync::Arc::new(crate::MemoryStore::new())).await.unwrap();
        for (id, status) in [("a", "PROCESSING"), ("b", "COMPLETED")] {
            write(&c, Some(EXPERIMENTS), EXPERIMENTS.index, id, &obj! { "id" => id, "status" => status, "type" => "POINTWISE_EVALUATION" }).await.unwrap();
        }
        assert_eq!(fail_interrupted_now(&c).await.unwrap(), 1);
        for (id, status) in [("a", "ERROR"), ("b", "COMPLETED")] {
            let doc = read(&c, EXPERIMENTS, id).await.unwrap().unwrap();
            assert_eq!(doc.get("status").and_then(Value::as_str), Some(status), "{id}");
        }
    }

    #[test]
    fn pairwise_metrics_are_the_plugins() {
        let a = ids(&["1", "2", "3"]);
        assert!(pairwise_metrics(&a, &a).iter().all(|(_, v)| *v == 1.0));
        let v = pairwise_metrics(&ids(&["1", "2", "3", "4"]), &ids(&["6", "5", "4", "3"]));
        assert_eq!(v.iter().map(|(_, x)| *x).collect::<Vec<_>>(), [0.33, 0.03, 0.11, 0.5]);
    }
}
