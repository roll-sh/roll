//! The `elasticsearch` service: models that run on Elasticsearch's ML nodes (ELSER, E5, the
//! Elastic reranker). roll runs no ML nodes, so it answers like an Elasticsearch cluster without
//! any: the three preconfigured endpoints exist (and `semantic_text` fields default to ELSER),
//! and running them, or creating an endpoint of the service, fails as it fails there (captured
//! from Elasticsearch 9.5.5 with `node.roles` lacking `ml`). Nothing is ever made up.

use super::Config;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::xpack::inference::chunking::DEFAULT;
use crate::xpack::inference::{Endpoint, Request, TaskType};

const DEFAULTS: [(&str, TaskType, &str); 3] = [
    // Without ML nodes the platform-specific variants (`_linux-x86_64`) aren't chosen.
    (".elser-2-elasticsearch", TaskType::SparseEmbedding, ".elser_model_2"),
    (".multilingual-e5-small-elasticsearch", TaskType::TextEmbedding, ".multilingual-e5-small"),
    (".rerank-v1-elasticsearch", TaskType::Rerank, ".rerank-v1"),
];

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

/// The preconfigured endpoints.
pub fn defaults() -> Vec<Endpoint> {
    DEFAULTS.iter().map(|(id, t, m)| default(id, *t, m)).collect()
}

pub fn default_endpoint(id: &str) -> Option<Endpoint> {
    DEFAULTS.iter().find(|(i, _, _)| *i == id).map(|(i, t, m)| default(i, *t, m))
}

/// The model an endpoint of the service runs, without its platform suffix (usage stats).
pub fn model_of(ep: &Endpoint) -> String {
    let m = ep.service_settings.get("model_id").map(Value::to_es_string).unwrap_or_default();
    m.strip_suffix("_linux-x86_64").map(str::to_owned).unwrap_or(m)
}

/// What starting a model deployment answers with no ML node (429).
pub fn no_ml_nodes() -> EsError {
    EsError::status(429, "Could not start deployment because no ML nodes with sufficient capacity were found").caused_by(EsError::java(
        500,
        "illegal_state_exception",
        "Could not start deployment because no suitable nodes were found, allocation explanation [No ML nodes exist in the cluster]",
    ))
}

/// What running an endpoint of the service answers: its deployment can't start.
pub fn infer_error(_ep: &Endpoint, _req: &Request) -> EsError {
    no_ml_nodes()
}

/// Settings of a new endpoint, kept as given (`num_allocations`, `num_threads`, `model_id`,
/// `adaptive_allocations` first, in that order). Elasticsearch stores such an endpoint and then
/// fails to start its deployment ([`no_ml_nodes`]); so does roll.
pub fn parse(_task: TaskType, cfg: &mut Config) -> EsResult<(Map, Map, Map)> {
    let mut given = std::mem::take(&mut cfg.service_settings);
    let mut ss = Map::new();
    for k in ["num_allocations", "num_threads", "model_id", "adaptive_allocations"] {
        if let Some(v) = given.shift_remove(k) {
            ss.insert(k.into(), v);
        }
    }
    ss.extend(given);
    let ts = std::mem::take(&mut cfg.task_settings);
    Ok((ss, ts, Map::new()))
}
