//! Hooks for running many stateless brokers as one cluster (`mq-cluster` implements them).
//!
//! - [`Placement`] decides where a queue's actor runs. Single-node brokers have none and run
//!   every queue locally; a cluster gives each node a router per queue that forwards to the
//!   owning node and takes over (fencing the log with a new epoch) when that node is gone.
//! - [`Broker::apply_change`](crate::Broker::apply_change) applies the metadata change feed, so a
//!   declare on one node shows up on every other node.

use crate::error::Result;
use crate::log::LogStore;
use crate::queue::engine::QueueHost;
use crate::queue::{QueueConfig, QueueMeta, QueueRef, QueueType};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

/// Where a queue's actor runs.
pub enum Located {
    /// On this node: the actor itself (not the router), e.g. for stream readers.
    Local(QueueRef),
    /// On another node, reachable at these client listeners (`amqp`, `stream`, …).
    Remote { node: String, listeners: BTreeMap<String, SocketAddr> },
}

/// Everything needed to start a queue actor on any node.
#[derive(Clone)]
pub struct QueueStart {
    pub meta: QueueMeta,
    pub kind: QueueType,
    pub config: QueueConfig,
    /// Log key (`q/{vhost}/{name}/{generation}`); the same on every node.
    pub log_key: String,
    /// Whether the queue keeps a log (durable, not exclusive).
    pub durable: bool,
    pub host: Arc<dyn QueueHost>,
    /// Partition index of a partitioned queue (`x-partitions`), else 0.
    pub partition: u32,
}

impl QueueStart {
    /// Run the actor here, opening the log with `epoch` (fencing older owners).
    pub async fn spawn_local(&self, logs: &dyn LogStore, epoch: u64) -> Result<QueueRef> {
        let (meta, config, host) = (self.meta.clone(), self.config.clone(), self.host.clone());
        Ok(match self.kind {
            QueueType::Stream => {
                let log = logs.open(&self.log_key, epoch).await?;
                crate::queue::stream::spawn(meta, config, log, host).await?
            }
            _ => {
                let log = if self.durable { Some(logs.open(&self.log_key, epoch).await?) } else { None };
                crate::queue::engine::spawn(meta, config, log, host).await?
            }
        })
    }

    /// Whether a cluster may place this queue on another node. Exclusive and transient queues
    /// live with their connection's node, like RabbitMQ's classic queues.
    pub fn placeable(&self) -> bool {
        self.durable && self.meta.exclusive_owner.is_none()
    }
}

#[async_trait]
pub trait Placement: Send + Sync {
    /// Return the handle channels on this node use for the queue: a router to wherever it
    /// runs. Called for new queues and for queues learned from the change feed.
    async fn start(&self, start: QueueStart) -> Result<QueueRef>;
    /// The queue is gone cluster-wide (deleted, or its vhost was).
    fn forget(&self, vhost: &str, name: &str);
    /// Where the queue runs now (`None`: not placed by the cluster).
    fn locate(&self, _vhost: &str, _name: &str) -> Option<Located> {
        None
    }
}
