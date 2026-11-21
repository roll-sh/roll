//! The `elasticsearch` service: models that run on Elasticsearch's ML nodes (ELSER, E5, the
//! Elastic reranker).
//!
//! roll runs one of them itself: `multilingual-e5-small`, on the node's embedding pool
//! ([`super::super::local::Embedder`], `search_server::embed`), answering as Elasticsearch
//! 9.5.5's `.multilingual-e5-small` deployment does (checked against one: the `passage: ` and
//! `query: ` prefixes, the 512-token cut, unit-length 384-float embeddings; compatibility.md,
//! "Inference"). The preconfigured `.multilingual-e5-small-elasticsearch` endpoint and endpoints
//! a tenant creates with `model_id` `.multilingual-e5-small` (or its `_linux-x86_64` build) run
//! there.
//!
//! The other models need ML nodes roll doesn't run, so they answer like an Elasticsearch cluster
//! without any: the preconfigured endpoints exist (and `semantic_text` fields default to ELSER),
//! and running them, or creating an endpoint of them, fails as it fails there (captured from
//! Elasticsearch 9.5.5 with `node.roles` lacking `ml`). So does E5 on a node whose operator
//! turned the model off. Nothing is ever made up.

use super::Config;
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::xpack::inference::chunking::DEFAULT;
use crate::xpack::inference::results::Results;
use crate::xpack::inference::{Endpoint, Request, TaskType};

/// The E5 model, as Elasticsearch names it.
pub const E5_MODEL: &str = ".multilingual-e5-small";
/// Elasticsearch's build of it for Linux on x86-64, which it picks on such ML nodes.
pub const E5_MODEL_X86: &str = ".multilingual-e5-small_linux-x86_64";
/// The preconfigured E5 endpoint.
pub const E5_ENDPOINT: &str = ".multilingual-e5-small-elasticsearch";
/// E5's embeddings: dimensions and similarity.
pub const E5_DIMS: i64 = 384;
/// What the model puts before a document's text (`prefix_strings.ingest`) and a query's
/// (`prefix_strings.search`).
pub const PASSAGE: &str = "passage: ";
pub const QUERY: &str = "query: ";

const DEFAULTS: [(&str, TaskType, &str); 3] = [
    // Without ML nodes the platform-specific variants (`_linux-x86_64`) aren't chosen.
    (".elser-2-elasticsearch", TaskType::SparseEmbedding, ".elser_model_2"),
    (E5_ENDPOINT, TaskType::TextEmbedding, E5_MODEL),
    (".rerank-v1-elasticsearch", TaskType::Rerank, ".rerank-v1"),
];

/// Whether the node runs E5: an embedder that isn't turned off.
fn runs_e5(cluster: &Cluster) -> bool {
    cluster.config.embedder.as_ref().is_some_and(|e| e.available())
}

fn default(id: &str, task: TaskType, model: &str) -> Endpoint {
    let Value::Obj(service_settings) = obj!(
        "num_threads" => 1,
        "model_id" => model,
        "adaptive_allocations" => obj! { "enabled" => true, "min_number_of_allocations" => 0, "max_number_of_allocations" => 32 },
    ) else {
        unreachable!()
    };
    let mut task_settings = Map::new();
    if task == TaskType::Rerank {
        task_settings.insert("return_documents".into(), true.into());
    }
    Endpoint {
        id: id.to_string(),
        task,
        service: "elasticsearch".into(),
        service_settings,
        task_settings,
        chunking: (task != TaskType::Rerank).then_some(DEFAULT),
        secrets: Map::new(),
    }
}

/// The preconfigured endpoints. E5's names `.multilingual-e5-small`, the float build roll runs
/// (Elasticsearch names its `_linux-x86_64` build on Linux x86-64 ML nodes, the float build
/// elsewhere).
pub fn defaults() -> Vec<Endpoint> {
    DEFAULTS.iter().map(|(id, t, m)| default(id, *t, m)).collect()
}

pub fn default_endpoint(id: &str) -> Option<Endpoint> {
    DEFAULTS.iter().find(|(i, _, _)| *i == id).map(|(i, t, m)| default(i, *t, m))
}

/// Whether `id` is a preconfigured endpoint's (reserved).
pub fn is_default(id: &str) -> bool {
    DEFAULTS.iter().any(|(i, _, _)| *i == id)
}

/// The model an endpoint of the service runs, without its platform suffix (usage stats).
pub fn model_of(ep: &Endpoint) -> String {
    let m = ep.service_settings.get("model_id").map(Value::to_es_string).unwrap_or_default();
    m.strip_suffix("_linux-x86_64").map(str::to_owned).unwrap_or(m)
}

/// Whether an endpoint runs E5: the `elasticsearch` service, a text embedding endpoint of
/// either build of the model.
pub fn is_e5(ep: &Endpoint) -> bool {
    ep.service == "elasticsearch" && ep.task == TaskType::TextEmbedding && model_of(ep) == E5_MODEL
}

/// What starting a model deployment answers with no ML node (429).
pub fn no_ml_nodes() -> EsError {
    EsError::status(429, "Could not start deployment because no ML nodes with sufficient capacity were found").caused_by(EsError::java(
        500,
        "illegal_state_exception",
        "Could not start deployment because no suitable nodes were found, allocation explanation [No ML nodes exist in the cluster]",
    ))
}

/// Whether creating an endpoint of `ep`'s model starts a deployment roll can run.
pub fn deploys(cluster: &Cluster, ep: &Endpoint) -> bool {
    is_e5(ep) && runs_e5(cluster)
}

/// Runs an endpoint of the service: E5 on the node's pool, anything else fails as its
/// deployment can't start.
pub async fn infer(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Results> {
    let Some(embedder) = cluster.config.embedder.as_ref().filter(|e| e.available() && is_e5(ep)) else {
        return Err(no_ml_nodes());
    };
    // `prefix_strings`: documents (`ingest`, and what `semantic_text` embeds at ingest) get the
    // passage prefix, queries the query prefix, any other input type none (captured).
    use super::super::local::InputKind;
    let kind = match req.input_type.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("ingest" | "internal_ingest") => InputKind::Ingest,
        Some("search" | "internal_search") => InputKind::Search,
        _ => InputKind::Other,
    };
    let tenant = super::super::local::Tenant { cluster: &cluster.config.name, owner: cluster.config.owner.as_deref() };
    let vectors = embedder.embed(tenant, kind, req.input.clone(), req.timeout).await.map_err(|e| e.to_es())?;
    if vectors.len() != req.input.len() {
        return Err(EsError::java(500, "illegal_state_exception", format!("The model returned [{}] embeddings for [{}] inputs", vectors.len(), req.input.len())));
    }
    Ok(Results::Floats(vectors))
}

/// Settings of a new endpoint, kept as given (`num_allocations`, `num_threads`, `model_id`,
/// `adaptive_allocations` first, in that order). Elasticsearch stores such an endpoint and then
/// starts its deployment: roll's E5 is there already; other models fail ([`no_ml_nodes`]).
///
/// An E5 endpoint is checked as `ElasticsearchInternalServiceSettings` checks one (captured):
/// `num_allocations` or `adaptive_allocations` is required, and other settings are unknown.
pub fn parse(task: TaskType, cfg: &mut Config) -> EsResult<(Map, Map, Map)> {
    let mut given = std::mem::take(&mut cfg.service_settings);
    let mut ss = Map::new();
    for k in ["num_allocations", "num_threads", "model_id", "adaptive_allocations"] {
        if let Some(v) = given.shift_remove(k) {
            ss.insert(k.into(), v);
        }
    }
    let e5 = task == TaskType::TextEmbedding && ss.get("model_id").map(Value::to_es_string).is_some_and(|m| m == E5_MODEL || m == E5_MODEL_X86);
    if e5 {
        if !ss.contains_key("num_allocations") && !ss.contains_key("adaptive_allocations") {
            return Err(crate::xpack::inference::settings::validation(&[
                "[service_settings] does not contain one of the required settings [num_allocations, adaptive_allocations]".into(),
            ]));
        }
        // What's left is reported unknown by `Config::finish`.
        cfg.service_settings = given;
    } else {
        ss.extend(given);
    }
    let ts = std::mem::take(&mut cfg.task_settings);
    Ok((ss, ts, Map::new()))
}
