//! Models the node runs itself: what a cluster asks of the node's embedding pool
//! (`search_server::embed`), which runs `multilingual-e5-small` as Elasticsearch's
//! `.multilingual-e5-small` deployment answers.
//!
//! The pool is the node's, shared by every cluster it serves: inputs queue per cluster and run
//! in turns, so a cluster past its queue is refused the way an Elasticsearch deployment with a
//! full queue refuses (429, `es_rejected_execution_exception`), and the CPU each cluster's
//! inputs took is metered to it.

use crate::error::EsError;
use async_trait::async_trait;
use std::time::Duration;

/// What can go wrong running a node model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmbedError {
    /// The operator turned the model off (`--search-embed-model off`): the endpoint answers as
    /// on a cluster without ML nodes.
    Off,
    /// The cluster's queue is full.
    QueueFull,
    /// More inputs, or a longer input, than one call may carry (the message says which).
    TooLarge(String),
    /// Not done within the request's timeout.
    TimedOut(Duration),
    /// The model couldn't be loaded or run (the message is the operator's, never a tenant's).
    Failed(String),
}

impl EmbedError {
    /// The error Elasticsearch answers in the same situation.
    pub fn to_es(&self) -> EsError {
        match self {
            EmbedError::Off => super::services::builtin::no_ml_nodes(),
            EmbedError::QueueFull => EsError::java(429, "es_rejected_execution_exception", "inference process queue is full. Unable to execute command"),
            EmbedError::TooLarge(m) => EsError::status(400, m.clone()),
            // The deployment's timeout (captured: `timeout [1ms] waiting for inference result`).
            EmbedError::TimedOut(t) => EsError::status(408, format!("timeout [{}] waiting for inference result", crate::units::TimeValue::millis(t.as_millis() as i64).human())),
            EmbedError::Failed(m) => EsError::status(500, format!("Failed to run the [{}] model: {m}", super::services::builtin::E5_MODEL)),
        }
    }
}

/// The node's embedding pool, as a cluster sees it.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Whether the model can run on this node (not turned off). An endpoint of a model the node
    /// can't run answers as Elasticsearch without ML nodes does.
    fn available(&self) -> bool;
    /// Embeds `texts` (prefixes already applied) for `cluster` (its name on the node), each to 384 unit-length floats, in order, waiting at most
    /// `timeout` in the queue and running.
    async fn embed(&self, cluster: &str, texts: Vec<String>, timeout: Duration) -> Result<Vec<Vec<f32>>, EmbedError>;
}

impl std::fmt::Debug for dyn Embedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Embedder")
    }
}
