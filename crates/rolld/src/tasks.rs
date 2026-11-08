//! roll tasks in rolld: the API (`/v1/tasks`, hosted tenants at `/t/<tenant>/v1/tasks`, their
//! management at `/_roll/tenants`) on every node, and the edge's tasks host.
//!
//! The data is a log store cell (`tasks::store::log`) in the node's object storage under
//! `tasks/`; its leader is the holder of the lease `tasks:cell:default` and the other nodes
//! forward to it (`tasks::fleet`, docs/cloud/README.md "Ownership keys"). A node alone leads from
//! the start, with its log in the local object directory (`<home>/objects/tasks`), and imports the
//! SQLite database earlier versions kept (`<home>/tasks.db`) once, on its first start.

use ::tasks::fleet::{NodeConfig, TasksNode};
use ::tasks::store::log::{LogConfig, import_sqlite};
use clap::Args;
use object_store::ObjectStore;
use pg_storage::StorageConfig;
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

#[derive(Args, Clone, Debug)]
pub struct TasksArgs {
    /// Token for the tasks API (`/v1/tasks`, `/_roll/tenants`); without one it's open, like the
    /// rest of the local API.
    #[arg(long = "tasks-token", env = "ROLL_TASKS_TOKEN", hide_env_values = true)]
    pub token: Option<String>,
    /// Minimum time between two log commits (each is one object write): API writes wait up to
    /// this long for theirs (default: 100 on S3, 5 in a local directory).
    #[arg(long = "tasks-commit-ms", env = "ROLL_TASKS_COMMIT_MS")]
    pub commit_ms: Option<u64>,
    /// Delivery outcomes, leases and workflow steps are committed within this long; a crash
    /// loses at most this much of them, which become redeliveries.
    #[arg(long = "tasks-relaxed-ms", env = "ROLL_TASKS_RELAXED_MS", default_value_t = 5000)]
    pub relaxed_ms: u64,
    /// Refuse deliveries to private and link-local addresses (default: on in a cluster).
    #[arg(long = "tasks-block-private", env = "ROLL_TASKS_BLOCK_PRIVATE", value_parser = clap::builder::BoolishValueParser::new())]
    pub block_private: Option<bool>,
}

/// roll tasks on this node.
pub struct Tasks {
    pub node: Arc<TasksNode>,
}

impl Tasks {
    /// Every tasks request, on any node (they run on the cell's leader): for the edge.
    pub fn router(&self) -> axum::Router {
        self.node.router()
    }

    /// Tasks' routes and the one other nodes forward to, for the API router.
    pub fn api_routes(&self) -> axum::Router {
        self.node.api_routes()
    }

    /// Drain deliveries and commit everything if this node still leads the cell.
    pub async fn shutdown(&self) {
        self.node.shutdown().await;
    }
}

/// The log's objects and key prefix in the node's object storage.
fn log_objects(storage: &StorageConfig) -> anyhow::Result<(Arc<dyn ObjectStore>, String)> {
    Ok(match storage {
        StorageConfig::Local(dir) => {
            std::fs::create_dir_all(dir)?;
            // Commits are on disk when a write is acknowledged.
            (Arc::new(object_store::local::LocalFileSystem::new_with_prefix(dir)?.with_fsync(true)), "tasks".into())
        }
        StorageConfig::S3 { bucket, region, endpoint, prefix } => {
            let mut b = object_store::aws::AmazonS3Builder::from_env().with_bucket_name(bucket);
            if let Some(r) = region {
                b = b.with_region(r);
            }
            if let Some(e) = endpoint {
                b = b.with_endpoint(e).with_allow_http(e.starts_with("http://"));
            }
            let prefix = match prefix.as_deref().map(|p| p.trim_matches('/')).filter(|p| !p.is_empty()) {
                Some(p) => format!("{p}/tasks"),
                None => "tasks".into(),
            };
            (Arc::new(b.build()?), prefix)
        }
    })
}

/// Take part in serving roll tasks on `cluster`: on a node alone the cell is open when this
/// returns (and an error opening it fails startup); in a cluster it opens whenever this node gets
/// the cell's lease.
pub async fn start(args: &TasksArgs, home: &Path, storage: &StorageConfig, cluster: Arc<roll_cluster::Cluster>) -> anyhow::Result<Tasks> {
    let single = cluster.backend_name() == "memory";
    let local = matches!(storage, StorageConfig::Local(_));
    if !single && local {
        anyhow::bail!("a tasks cluster needs S3 (--s3-bucket): a local directory isn't shared between nodes");
    }
    let (objects, prefix) = log_objects(storage)?;
    let mut log = LogConfig::new(objects, prefix, home.join("tasks").join("cache"));
    log.node = cluster.node_id().to_string();
    // The shards the SQLite database had (an imported one keeps its lanes).
    log.shards = 4;
    log.commit_ms = args.commit_ms.unwrap_or(if local { 5 } else { 100 });
    log.relaxed_ms = args.relaxed_ms;
    let mut engine = ::tasks::Config {
        token: args.token.clone().filter(|t| !t.is_empty()),
        block_private_urls: args.block_private.unwrap_or(!single),
        ..::tasks::Config::default()
    };
    if !single {
        // A handoff waits for deliveries in flight this long (within the cluster's 30 s
        // handoff timeout, and short enough for held requests to ride it out).
        engine.drain_ms = 5_000;
    }

    let old = home.join("tasks.db");
    if old.exists() {
        if !single {
            warn!(path = %old.display(), "tasks: a cluster node doesn't import a local tasks database; it stays where it is");
        } else if import_sqlite(&log, ::tasks::store::log::DEFAULT_TENANT, &old).await? {
            let kept = home.join("tasks.db.imported");
            std::fs::rename(&old, &kept)?;
            for suffix in ["-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", old.display()));
            }
            info!(from = %old.display(), kept = %kept.display(), "tasks: imported the SQLite database into the log store");
        } else {
            warn!(path = %old.display(), "tasks: the log store already has data, so the old tasks database wasn't imported; remove it once you've checked");
        }
    }

    let mut cfg = NodeConfig::new(log, engine);
    cfg.single = single;
    let node = TasksNode::start(cluster, cfg).await?;
    info!(single, "tasks: log store cell");
    Ok(Tasks { node })
}
