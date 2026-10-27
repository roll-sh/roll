//! roll code inside rolld: its database, its cluster and its git memory budget.
//!
//! Alone, roll code keeps both of its databases in SQLite under `--home`. With
//! `--code-database postgres://…` they live in Postgres (schemas `code_store` and
//! `code_control`), and with `--code-cluster-secret` too the node joins a cluster of identical
//! nodes that share that database and S3 (docs/code/cluster.md).

use clap::Args;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Args, Clone, Debug)]
pub struct CodeArgs {
    /// Keep roll code's databases in this Postgres (schemas `code_store` and `code_control`)
    /// instead of SQLite under --home. Every node of a cluster uses the same one.
    #[arg(long = "code-database", env = "ROLL_CODE_DATABASE", hide_env_values = true)]
    pub code_database: Option<String>,
    /// Postgres connections roll code opens at most, per database.
    #[arg(long = "code-db-connections", env = "ROLL_CODE_DB_CONNECTIONS", default_value_t = 16)]
    pub code_db_connections: usize,
    /// More CA certificates (PEM) to verify roll code's Postgres with, besides the system roots
    /// (e.g. the RDS bundle). TLS itself follows the URL's `sslmode`.
    #[arg(long = "code-db-ca", env = "ROLL_CODE_DB_CA")]
    pub code_db_ca: Option<PathBuf>,
    /// Run roll code as one node of a cluster: the secret every node signs internal requests
    /// with (at least 16 bytes). Needs --code-database and --s3-bucket.
    #[arg(long = "code-cluster-secret", env = "ROLL_CODE_CLUSTER_SECRET", hide_env_values = true)]
    pub code_cluster_secret: Option<String>,
    /// The URL other nodes reach this one's API at (default: http://<public host>:<API port>).
    #[arg(long = "code-advertise-url", env = "ROLL_CODE_ADVERTISE_URL")]
    pub code_advertise_url: Option<String>,
    /// This node's id in the cluster (default: random per process).
    #[arg(long = "code-node-id", env = "ROLL_CODE_NODE_ID")]
    pub code_node_id: Option<String>,
    /// How long a node shutting down (SIGTERM) waits for its repos to move to other nodes, in
    /// seconds.
    #[arg(long = "code-drain-timeout", env = "ROLL_CODE_DRAIN_TIMEOUT", default_value_t = 30)]
    pub code_drain_timeout_s: u64,
    /// Memory every git request together may hold, in MiB.
    #[arg(long = "code-git-memory-budget", env = "ROLL_CODE_GIT_MEMORY_BUDGET", default_value_t = 1024)]
    pub code_git_memory_budget_mb: u64,
    /// Most one git request may hold, in MiB (default: three quarters of the budget).
    #[arg(long = "code-git-max-request-memory", env = "ROLL_CODE_GIT_MAX_REQUEST_MEMORY")]
    pub code_git_max_request_memory_mb: Option<u64>,
    /// How long a git request waits for memory before it fails as busy, in seconds.
    #[arg(long = "code-git-memory-wait-secs", env = "ROLL_CODE_GIT_MEMORY_WAIT_SECS", default_value_t = 30)]
    pub code_git_memory_wait_s: u64,
}

impl CodeArgs {
    /// Apply these flags to roll code's configuration.
    pub fn apply(&self, config: &mut code_control::Config, public_host: &str, api: SocketAddr) -> anyhow::Result<()> {
        config.git_memory_budget_bytes = self.code_git_memory_budget_mb.max(1) << 20;
        config.git_max_request_memory_bytes = self.code_git_max_request_memory_mb.map(|m| m.max(1) << 20);
        config.git_memory_wait = Duration::from_secs(self.code_git_memory_wait_s);
        if let Some(url) = self.code_database.as_deref().filter(|u| !u.is_empty()) {
            let mut d = code_control::DatabaseConfig::new(url);
            d.connections = self.code_db_connections.max(1);
            d.ca = self.code_db_ca.clone();
            config.database = Some(d);
        }
        let Some(secret) = self.code_cluster_secret.as_deref().filter(|s| !s.is_empty()) else {
            anyhow::ensure!(
                self.code_advertise_url.is_none() && self.code_node_id.is_none(),
                "--code-advertise-url and --code-node-id need --code-cluster-secret"
            );
            return Ok(());
        };
        anyhow::ensure!(config.database.is_some(), "a roll code cluster (--code-cluster-secret) needs --code-database");
        let url = self.code_advertise_url.clone().unwrap_or_else(|| format!("http://{public_host}:{}", api.port()));
        let id = self.code_node_id.clone().unwrap_or_else(code_cluster::ClusterConfig::random_node_id);
        let cluster = code_cluster::ClusterConfig::new(id, url.trim_end_matches('/'), secret.as_bytes().to_vec());
        cluster.validate()?;
        config.cluster = Some(cluster);
        Ok(())
    }

    pub fn drain_timeout(&self) -> Duration {
        Duration::from_secs(self.code_drain_timeout_s)
    }
}
