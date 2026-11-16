//! Ingest pipelines on the write path: the request's `pipeline` (or the index's
//! `index.default_pipeline`), then `index.final_pipeline`, run before a document is parsed.
//! A pipeline may rewrite the document, its metadata (`_index`, `_id`, `_routing`) or drop it.

pub mod doc;
pub mod jregex;
pub mod pipeline;
pub mod processors;
pub mod registered_domain;
pub mod simulate;
pub mod user_agent;

use crate::cluster::Cluster;
use crate::error::EsResult;
use crate::json::Value;
use crate::write::{Target, WriteArgs};
use bytes::Bytes;

/// What the pipelines decided.
pub enum Outcome {
    /// Index this document (possibly rewritten, possibly into another index), with the write
    /// arguments the pipelines' metadata changes give (`_id`, `_routing`, `_version`, …;
    /// `None` when no pipeline ran).
    Index(Target, Value, Bytes, Option<Box<Ingested>>),
    /// A `drop` processor dropped it (the index name for the response).
    Dropped(String),
}

/// What ingest changed about a write.
pub struct Ingested {
    /// The write arguments with the document's metadata changes.
    pub args: WriteArgs,
    /// The pipelines the write ran (request or default, final, and those of reroute targets).
    pub executed: Vec<String>,
    /// The indices the pipelines' reroutes created.
    pub created: Vec<String>,
}

/// Runs the pipelines that apply to a write into `target`.
/// `clock` collects the time pipelines took (see `Ctx::ingest_nanos`).
#[allow(clippy::too_many_arguments)]
pub async fn run_for_write(cluster: &Cluster, target: Target, args: &WriteArgs, id: &str, value: Value, source: Bytes, now_ms: i64, clock: Option<&std::sync::atomic::AtomicU64>) -> EsResult<Outcome> {
    let request_pipeline = args.pipeline.as_deref().filter(|p| *p != "_none");
    let default = target.index.settings.get("index.default_pipeline").filter(|p| *p != "_none").map(str::to_string);
    let fin = target.index.settings.get("index.final_pipeline").filter(|p| *p != "_none").map(str::to_string);
    let first = match args.pipeline.as_deref() {
        Some("_none") => None,
        Some(p) => Some(p.to_string()),
        None => default,
    };
    let _ = request_pipeline;
    if first.is_none() && fin.is_none() {
        return Ok(Outcome::Index(target, value, source, None));
    }
    let started = std::time::Instant::now();
    let out = pipeline::run_write(cluster, target, args, id, value, first, fin, now_ms, source.len()).await;
    if let Some(clock) = clock {
        clock.fetch_add(started.elapsed().as_nanos() as u64 + 1, std::sync::atomic::Ordering::Relaxed);
    }
    out
}
