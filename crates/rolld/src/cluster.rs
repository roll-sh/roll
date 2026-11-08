//! The node's cluster settings and handle (docs/cloud/README.md, "Node settings"; docs/code/cluster.md).
//!
//! Every product takes its leases from one `roll_cluster::Cluster`: the one roll code joined when
//! the node is part of a cluster, one joined here when roll code is off (`ROLL_PRODUCTS`; the
//! dynamodb backend only, since postgres keeps leases in code's database), else a node alone on
//! the in-process backend, which owns whatever it asks for. The roll code flags (`--code-cluster-secret`, `--code-node-id`,
//! `--code-advertise-url`, `--code-drain-timeout`) stay as aliases of these, so deploy/code keeps
//! working.

use clap::{Args, ValueEnum};
use roll_cluster::{Cluster, ClusterConfig, DynamoDbConfig, Memory};
use std::sync::Arc;
use std::time::Duration;

/// Where membership and leases live.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum BackendKind {
    /// One node: everything is this node's.
    Memory,
    /// roll code's Postgres (`--code-database`).
    Postgres,
    /// A DynamoDB table (`--cluster-table`).
    Dynamodb,
}

#[derive(Args, Clone, Debug)]
pub struct ClusterArgs {
    /// Where membership and leases live (default: postgres with a cluster secret, else memory).
    #[arg(id = "roll_cluster_backend", long = "cluster-backend", env = "ROLL_CLUSTER_BACKEND", value_enum)]
    pub backend: Option<BackendKind>,
    /// The DynamoDB table of the dynamodb backend.
    #[arg(id = "roll_cluster_table", long = "cluster-table", env = "ROLL_CLUSTER_TABLE", default_value = "roll-cluster")]
    pub table: String,
    /// Another DynamoDB endpoint (DynamoDB Local); default is AWS in `AWS_REGION`.
    #[arg(id = "roll_cluster_dynamodb_endpoint", long = "cluster-dynamodb-endpoint", env = "ROLL_CLUSTER_DYNAMODB_ENDPOINT")]
    pub dynamodb_endpoint: Option<String>,
    /// Join a cluster: the secret every node signs internal hops with (at least 16 bytes, the
    /// same on every node). `--code-cluster-secret` is an alias.
    #[arg(id = "roll_cluster_secret", long = "cluster-secret", env = "ROLL_CLUSTER_SECRET", hide_env_values = true)]
    pub secret: Option<String>,
    /// This node's id (default: random per process). A restarted process with the same id waits
    /// until the previous one looks dead. `--code-node-id` is an alias.
    #[arg(id = "roll_cluster_node_id", long = "node-id", env = "ROLL_NODE_ID")]
    pub node_id: Option<String>,
    /// The availability zone this node runs in: recorded, and used to spread standbys.
    #[arg(id = "roll_cluster_zone", long = "zone", env = "ROLL_ZONE")]
    pub zone: Option<String>,
    /// Where the other nodes reach this one (`http://<private ip>:<API port>`).
    /// `--code-advertise-url` is an alias.
    #[arg(id = "roll_cluster_advertise_url", long = "advertise-url", env = "ROLL_ADVERTISE_URL")]
    pub advertise_url: Option<String>,
    /// Seconds a stopping node (SIGTERM) waits for its leases to move to other nodes.
    /// `--code-drain-timeout` is an alias.
    #[arg(id = "roll_cluster_drain_timeout", long = "drain-timeout", env = "ROLL_DRAIN_TIMEOUT")]
    pub drain_timeout_s: Option<u64>,
    /// The system database every node shares (a Postgres URL): catalogs and accounts for every
    /// product. roll code keeps its data there unless `--code-database` names another.
    #[arg(id = "roll_system_database", long = "system-database", env = "ROLL_SYSTEM_DATABASE", hide_env_values = true)]
    pub system_database: Option<String>,
}

/// How long a stopping cluster node keeps serving after its health check starts failing, so the
/// load balancer stops sending to it first (two failed checks 5 s apart, plus margin).
pub const DEREGISTER: Duration = Duration::from_secs(15);

/// The cluster settings as roll code applies them: `secret`, node id and URL from these flags or
/// their roll code aliases.
pub struct Resolved {
    pub backend: BackendKind,
    pub secret: Option<String>,
    pub node_id: Option<String>,
    pub advertise_url: Option<String>,
}

impl ClusterArgs {
    /// Merge with roll code's aliases (these flags win when both are set).
    pub fn resolve(&self, code_secret: Option<&str>, code_node_id: Option<&str>, code_url: Option<&str>) -> anyhow::Result<Resolved> {
        let nonempty = |s: Option<&str>| s.filter(|s| !s.is_empty()).map(str::to_string);
        let secret = nonempty(self.secret.as_deref()).or(nonempty(code_secret));
        let backend = self.backend.unwrap_or(if secret.is_some() { BackendKind::Postgres } else { BackendKind::Memory });
        match (backend, &secret) {
            (BackendKind::Memory, Some(_)) => anyhow::bail!("ROLL_CLUSTER_BACKEND=memory is a single node; it takes no cluster secret"),
            (BackendKind::Postgres | BackendKind::Dynamodb, None) => anyhow::bail!("a cluster (ROLL_CLUSTER_BACKEND={backend:?}) needs ROLL_CLUSTER_SECRET"),
            _ => {}
        }
        Ok(Resolved {
            backend,
            secret,
            node_id: nonempty(self.node_id.as_deref()).or(nonempty(code_node_id)),
            advertise_url: nonempty(self.advertise_url.as_deref()).or(nonempty(code_url)),
        })
    }

    /// The backend roll code's cluster runs on.
    pub fn code_backend(&self, kind: BackendKind) -> code_control::ClusterBackend {
        match kind {
            BackendKind::Dynamodb => {
                let mut c = DynamoDbConfig::new(self.table.clone());
                c.endpoint = self.dynamodb_endpoint.clone().filter(|e| !e.is_empty());
                code_control::ClusterBackend::DynamoDb(c)
            }
            BackendKind::Memory | BackendKind::Postgres => code_control::ClusterBackend::Database,
        }
    }

    pub fn zone(&self) -> Option<String> {
        self.zone.clone().filter(|z| !z.is_empty())
    }
}

/// The cluster every product on this node takes its leases from: roll code's when the node is
/// part of a cluster, one of its own when roll code is off, else a node alone in memory.
pub fn handle(code: Option<&code_control::Code>, args: &crate::code::CodeArgs, default_url: &str) -> anyhow::Result<Arc<Cluster>> {
    if let Some(c) = code.and_then(|c| c.cluster()) {
        return Ok(c.clone());
    }
    let r = args.cluster.resolve(args.code_cluster_secret.as_deref(), args.code_node_id.as_deref(), args.code_advertise_url.as_deref())?;
    if let (None, Some(secret)) = (code, r.secret) {
        anyhow::ensure!(
            r.backend == BackendKind::Dynamodb,
            "ROLL_CLUSTER_BACKEND={:?} keeps leases in roll code's database: with code off (ROLL_PRODUCTS), use dynamodb",
            r.backend
        );
        let url = r.advertise_url.unwrap_or_else(|| default_url.to_string());
        let mut cfg = ClusterConfig::new(r.node_id.unwrap_or_else(ClusterConfig::random_node_id), url.trim_end_matches('/'), secret.into_bytes());
        cfg.zone = args.cluster.zone();
        let code_control::ClusterBackend::DynamoDb(d) = args.cluster.code_backend(r.backend) else { unreachable!("dynamodb") };
        // Joining blocks on the backend.
        return Ok(tokio::task::block_in_place(|| Cluster::join(Box::new(roll_cluster::DynamoDb::connect(d)?), cfg))?);
    }
    let id = r.node_id.unwrap_or_else(ClusterConfig::random_node_id);
    // Nothing is forwarded on one node; the secret only has to be valid.
    let mut cfg = ClusterConfig::new(id, default_url.trim_end_matches('/'), ClusterConfig::random_node_id().into_bytes());
    cfg.zone = args.cluster.zone();
    Ok(Cluster::join(Box::new(Memory::solo()), cfg)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(flatten)]
        c: ClusterArgs,
    }

    #[test]
    fn secrets_pick_the_backend_and_code_flags_are_aliases() {
        let t = T::parse_from(["t"]);
        assert_eq!(t.c.resolve(None, None, None).unwrap().backend, BackendKind::Memory);
        let r = t.c.resolve(Some("0123456789abcdef"), Some("n1"), Some("http://h:1")).unwrap();
        assert_eq!((r.backend, r.node_id.as_deref(), r.advertise_url.as_deref()), (BackendKind::Postgres, Some("n1"), Some("http://h:1")));
        let t = T::parse_from(["t", "--cluster-backend", "dynamodb", "--cluster-secret", "0123456789abcdef", "--node-id", "i-1", "--drain-timeout", "90"]);
        let r = t.c.resolve(None, Some("ignored"), None).unwrap();
        assert_eq!((r.backend, r.node_id.as_deref(), t.c.drain_timeout_s), (BackendKind::Dynamodb, Some("i-1"), Some(90)));
        assert!(matches!(t.c.code_backend(r.backend), code_control::ClusterBackend::DynamoDb(d) if d.table == "roll-cluster"));
        assert!(T::parse_from(["t", "--cluster-backend", "dynamodb"]).c.resolve(None, None, None).is_err(), "a cluster needs a secret");
        assert!(T::parse_from(["t", "--cluster-backend", "memory"]).c.resolve(Some("0123456789abcdef"), None, None).is_err());
    }
}
