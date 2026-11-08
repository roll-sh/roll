//! roll code inside rolld: its database, its cluster and its git memory budget.
//!
//! Alone, roll code keeps both of its databases in SQLite under `--home`. With
//! `--code-database postgres://…` they live in Postgres (schemas `code_store` and
//! `code_control`); without it, in the system database (`ROLL_SYSTEM_DATABASE`) when there is
//! one, and with a cluster secret (`ROLL_CLUSTER_SECRET`, or
//! `--code-cluster-secret`) too the node joins a cluster of identical nodes that share that
//! database and S3 (docs/code/cluster.md). Leases and membership are in that database, or in
//! DynamoDB with `ROLL_CLUSTER_BACKEND=dynamodb` (crate::cluster); code's data stays in Postgres.

use clap::Args;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Args, Clone, Debug)]
pub struct CodeArgs {
    /// Keep roll code's databases in this Postgres (schemas `code_store` and `code_control`)
    /// instead of SQLite under --home. Every node of a cluster uses the same one. Default: the
    /// system database (`ROLL_SYSTEM_DATABASE`), if set.
    #[arg(long = "code-database", env = "ROLL_CODE_DATABASE", hide_env_values = true)]
    pub code_database: Option<String>,
    /// Postgres connections roll code opens at most, per database.
    #[arg(long = "code-db-connections", env = "ROLL_CODE_DB_CONNECTIONS", default_value_t = 16)]
    pub code_db_connections: usize,
    /// More CA certificates (PEM) to verify roll code's Postgres with, besides the system roots
    /// (e.g. the RDS bundle). TLS itself follows the URL's `sslmode`.
    #[arg(long = "code-db-ca", env = "ROLL_CODE_DB_CA")]
    pub code_db_ca: Option<PathBuf>,
    /// Alias of `--cluster-secret` (`ROLL_CLUSTER_SECRET`). Needs --code-database and --s3-bucket.
    #[arg(long = "code-cluster-secret", env = "ROLL_CODE_CLUSTER_SECRET", hide_env_values = true)]
    pub code_cluster_secret: Option<String>,
    /// Alias of `--advertise-url` (default: http://<public host>:<API port>).
    #[arg(long = "code-advertise-url", env = "ROLL_CODE_ADVERTISE_URL")]
    pub code_advertise_url: Option<String>,
    /// Alias of `--node-id` (default: random per process).
    #[arg(long = "code-node-id", env = "ROLL_CODE_NODE_ID")]
    pub code_node_id: Option<String>,
    /// Alias of `--drain-timeout` (default 30 s).
    #[arg(long = "code-drain-timeout", env = "ROLL_CODE_DRAIN_TIMEOUT")]
    pub code_drain_timeout_s: Option<u64>,
    #[command(flatten)]
    pub cluster: crate::cluster::ClusterArgs,
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
        if let Some(url) = self.database() {
            let mut d = code_control::DatabaseConfig::new(url);
            d.connections = self.code_db_connections.max(1);
            d.ca = self.code_db_ca.clone();
            config.database = Some(d);
        }
        let r = self.cluster.resolve(self.code_cluster_secret.as_deref(), self.code_node_id.as_deref(), self.code_advertise_url.as_deref())?;
        let Some(secret) = r.secret else {
            anyhow::ensure!(
                self.code_advertise_url.is_none() && self.code_node_id.is_none(),
                "--code-advertise-url and --code-node-id need a cluster secret"
            );
            return Ok(());
        };
        anyhow::ensure!(config.database.is_some(), "a roll code cluster needs ROLL_SYSTEM_DATABASE or ROLL_CODE_DATABASE: code's data is in Postgres");
        let url = r.advertise_url.unwrap_or_else(|| self.default_url(public_host, api));
        let id = r.node_id.unwrap_or_else(roll_cluster::ClusterConfig::random_node_id);
        let mut cluster = roll_cluster::ClusterConfig::new(id, url.trim_end_matches('/'), secret.into_bytes());
        cluster.zone = self.cluster.zone();
        cluster.validate()?;
        config.cluster = Some(cluster);
        config.cluster_backend = self.cluster.code_backend(r.backend);
        Ok(())
    }

    /// Code's Postgres: `--code-database`, else the system database.
    fn database(&self) -> Option<&str> {
        [&self.code_database, &self.cluster.system_database].into_iter().find_map(|s| s.as_deref().filter(|u| !u.is_empty()))
    }

    /// Where other nodes reach this one unless told: the API on the public host.
    pub fn default_url(&self, public_host: &str, api: SocketAddr) -> String {
        format!("http://{public_host}:{}", api.port())
    }

    /// `ROLL_DRAIN_TIMEOUT` (or its alias `ROLL_CODE_DRAIN_TIMEOUT`), 30 s by default.
    pub fn drain_timeout(&self) -> Duration {
        Duration::from_secs(self.cluster.drain_timeout_s.or(self.code_drain_timeout_s).unwrap_or(30))
    }
}
