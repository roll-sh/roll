//! Data stream failure stores on the write path (`FailureStoreDocumentConverter`): a document
//! that fails in ingest or while it is parsed and indexed into a data stream whose failure
//! store is enabled is not rejected; a failure document (`@timestamp`, the original
//! `document`, the `error` with its pipeline trace) goes to the data stream's failure index,
//! which is created on the first failure (and on the first failure after a lazy rollover),
//! and the response says `failure_store: used`. Version conflicts are never redirected.

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::meta::data_streams::{FAILURE_STORE_PREFIX, backing_index_name};
use crate::write::{WriteArgs, WriteOutcome};
use bytes::Bytes;
use std::sync::Arc;

/// What a failed write to a target does with its failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Store {
    /// Not a data stream.
    None,
    /// A data stream without a failure store (`failure_store: not_enabled`).
    Disabled,
    /// The data stream whose failure store takes the failure.
    Enabled(String),
}

/// The failure store a failed write to `expr` would go to.
pub fn target(cluster: &Cluster, expr: &str) -> Store {
    let st = cluster.state();
    let name = expr.split("::").next().unwrap_or(expr);
    let ds = match st.metadata.data_streams.get(name) {
        Some(ds) => ds.clone(),
        None => {
            // A data stream alias, or a backing index written to directly (not redirected).
            match st.metadata.data_stream_aliases.get(name).and_then(|a| a.write_data_stream.clone()).and_then(|w| st.metadata.data_streams.get(&w).cloned()) {
                Some(ds) => ds,
                None => return Store::None,
            }
        }
    };
    if crate::api::data_streams::failure_store_enabled(&st, &ds) { Store::Enabled(ds.name.clone()) } else { Store::Disabled }
}

/// Whether a write failure is a document failure the failure store takes.
pub fn eligible(e: &EsError) -> bool {
    e.kind != "version_conflict_engine_exception" && e.status != 404 && e.status != 429
}

/// `Exception.getClass().getSimpleName()` for an exception type name.
fn class_name(kind: &str) -> String {
    kind.split('_').map(|w| {
        let mut c = w.chars();
        match c.next() {
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            None => String::new(),
        }
    }).collect()
}

/// The failure document (`FailureStoreDocumentConverter.transformFailedRequest`).
fn failure_doc(ds: &str, id: Option<&str>, routing: Option<&str>, source: &Value, e: &EsError, ingest: Option<&crate::ingest::pipeline::IngestFailure>, now_ms: i64) -> Value {
    let mut document = Map::new();
    if let Some(id) = id {
        document.insert("id".into(), Value::from(id));
    }
    if let Some(r) = routing {
        document.insert("routing".into(), Value::from(r));
    }
    document.insert("index".into(), Value::from(ds));
    document.insert("source".into(), source.clone());
    let mut error = Map::new();
    error.insert("type".into(), Value::from(e.kind.as_ref()));
    error.insert("message".into(), Value::from(e.reason()));
    let mut trace = format!("{}: {}\n", class_name(&e.kind), e.reason());
    let mut cause = e.caused_by.as_deref();
    while let Some(c) = cause {
        trace.push_str(&format!("Caused by: {}: {}\n", class_name(&c.kind), c.reason()));
        cause = c.caused_by.as_deref();
    }
    error.insert("stack_trace".into(), Value::Str(trace));
    if let Some(f) = ingest {
        error.insert("pipeline_trace".into(), Value::Arr(f.pipelines.iter().cloned().map(Value::Str).collect()));
        if let Some(p) = f.pipelines.last() {
            error.insert("pipeline".into(), Value::from(p.as_str()));
        }
        if let Some(t) = &f.processor_tag {
            error.insert("processor_tag".into(), Value::from(t.as_str()));
        }
        if !f.processor_type.is_empty() {
            error.insert("processor_type".into(), Value::from(f.processor_type.as_str()));
        }
    }
    let mut m = Map::new();
    m.insert("@timestamp".into(), Value::Str(crate::time::java_instant_string(now_ms)));
    m.insert("document".into(), Value::Obj(document));
    m.insert("error".into(), Value::Obj(error));
    Value::Obj(m)
}

/// The data stream's failure write index, created when the store has none yet or is marked
/// for a lazy rollover.
/// `fresh`: the data stream was created by this write, and its failure store starts with it
/// (same generation).
async fn failure_index(cluster: &Cluster, ds: &str, fresh: bool, now_ms: i64) -> EsResult<String> {
    let name = ds.to_string();
    cluster
        .update(move |st| {
            let d = st.metadata.data_streams.get(&name).cloned().ok_or_else(|| EsError::index_not_found(&name))?;
            if let Some(w) = d.failure_write_index()
                && !d.failure_rollover_on_write
            {
                return Ok(w.to_string());
            }
            let generation = if fresh && d.failure_indices.is_empty() && d.generation == 1 { 1 } else { d.generation + 1 };
            let index = backing_index_name(FAILURE_STORE_PREFIX, &name, generation, now_ms);
            let template = match crate::meta::templates::find_composable(&st.metadata, &name, d.hidden) {
                Some((_, t)) if t.data_stream.is_some() => t.clone(),
                _ => return Err(EsError::illegal_argument(format!("no matching index template found for data stream [{name}]"))),
            };
            crate::api::indices::create_failure_index(st, &index, &template, &d.settings, now_ms)?;
            let mut nd = (*d).clone();
            nd.failure_indices.push(index.clone());
            nd.generation = generation;
            nd.failure_rollover_on_write = false;
            st.metadata.data_streams.insert(name.clone(), Arc::new(nd));
            Ok(index)
        })
        .await
}

/// Indexes the failure document of a failed write into `ds`'s failure store.
#[allow(clippy::too_many_arguments)]
pub async fn redirect(
    cluster: &Cluster,
    ds: &str,
    id: Option<&str>,
    routing: Option<&str>,
    source: &Value,
    e: &EsError,
    ingest: Option<&crate::ingest::pipeline::IngestFailure>,
    doc_index: Option<&str>,
    fresh: bool,
    now_ms: i64,
) -> EsResult<WriteOutcome> {
    let index = failure_index(cluster, ds, fresh, now_ms).await?;
    // The document names the data stream it was first written to (a reroute may have sent its
    // failure to another data stream's failure store).
    let doc_index = doc_index.unwrap_or(ds);
    let doc = failure_doc(doc_index, id, routing, source, e, ingest, now_ms);
    let bytes = Bytes::from(doc.to_json());
    let idx = cluster.state().metadata.indices.get(&index).cloned().ok_or_else(|| EsError::index_not_found(&index))?;
    let fid = crate::meta::names::time_based_id();
    let prepared = crate::write::prepare(cluster, idx, &fid, None, &bytes, &doc, None).await?;
    let args = WriteArgs { create: true, internal: true, ..Default::default() };
    let mut out = crate::write::apply_index(prepared, &args, true, &fid)?;
    out.failure_store = Some("used");
    Ok(out)
}
