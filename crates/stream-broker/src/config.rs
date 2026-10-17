use crate::topic_config::Defaults;
use pg_storage::StorageConfig;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    /// Root for `stream.db` and local partition data.
    pub home: PathBuf,
    /// Kafka listener.
    pub listen: SocketAddr,
    /// Host and port clients are told to connect to (Metadata, FindCoordinator).
    pub advertised_host: String,
    pub advertised_port: u16,
    pub node_id: i32,
    pub rack: Option<String>,
    /// Durable storage for sealed data; shared with Postgres (keys live under `streams/`).
    pub storage: StorageConfig,
    /// Serve TLS on the Kafka listener with this PEM certificate chain and key.
    pub tls: Option<TlsFiles>,
    /// Connections that don't authenticate use this stream (id or name). Without it SASL is
    /// required.
    pub default_stream: Option<String>,
    /// fsync before acknowledging `acks=all` and exposing data. Off only for benchmarks/tests.
    pub fsync: bool,
    pub topics: Defaults,
    pub upload: UploadConfig,
    pub groups: GroupConfig,
    pub txn: TxnConfig,
    /// Largest request frame accepted (`socket.request.max.bytes`).
    pub max_request_bytes: usize,
    /// Pipelined requests per connection.
    pub max_in_flight: usize,
    /// Partitions idle this long are closed.
    pub idle_unload_ms: i64,
    /// Closing a partition also drops its local copies of data object storage has, so idle
    /// partitions take no disk. Reads of them then come from object storage until they're active.
    pub evict_on_unload: bool,
    /// Local disk for partition data across all open partitions (0 = no limit beyond each topic's
    /// `local.retention.*`). Over it, the oldest uploaded segments go first. Data not yet in object
    /// storage is never dropped, so this is a target rather than a hard cap.
    pub local_disk_bytes: u64,
    /// How often retention, compaction and local cleanup run.
    pub clean_interval_ms: u64,
    /// Objects replaced by compaction are deleted this long after the swap, so in-flight reads
    /// of them finish.
    pub compaction_grace_ms: u64,
    /// Bytes of object-storage chunks cached in memory for reads below the local range.
    pub remote_cache_bytes: usize,
    /// Recent batches kept in memory per partition…
    pub tail_cache_bytes: usize,
    /// …and across all open partitions (each gets at most an equal share).
    pub tail_cache_total_bytes: usize,
    pub pricing: Pricing,
    /// Base URL of the HTTP API, shown in connection details.
    pub http_url: String,
    /// Require an API key on the HTTP data plane even when the control plane is open.
    pub http_require_auth: bool,
}

#[derive(Clone, Debug)]
pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Clone, Debug)]
pub struct UploadConfig {
    /// Upload the active segment once this much is pending…
    pub min_bytes: u64,
    /// …or the oldest pending byte is this old (the most a lost host can lose).
    pub max_age_ms: i64,
    /// Largest chunk object.
    pub max_bytes: u64,
    pub interval_ms: u64,
    pub concurrency: usize,
}

#[derive(Clone, Debug)]
pub struct GroupConfig {
    /// Wait this long for more members when an empty group's first member joins.
    pub initial_rebalance_delay_ms: u64,
    pub min_session_timeout_ms: i32,
    pub max_session_timeout_ms: i32,
    /// Offsets of empty groups expire after this long.
    pub offsets_retention_ms: i64,
    pub max_size: usize,
}

#[derive(Clone, Debug)]
pub struct TxnConfig {
    pub max_timeout_ms: i32,
    /// Forget transactional ids unused for this long.
    pub id_expiration_ms: i64,
    /// Idempotent producer state on partitions expires after this long without writes.
    pub producer_id_expiration_ms: i64,
}

/// AWS list prices for the cost estimate (us-east-1).
#[derive(Clone, Debug, serde::Serialize)]
pub struct Pricing {
    /// S3 Standard, $/GB-month.
    pub usd_per_gb_month_s3: f64,
    /// gp3 for the local hot set, $/GB-month.
    pub usd_per_gb_month_local: f64,
    /// S3 PUT and GET requests, $ per 1000.
    pub usd_per_1k_put: f64,
    pub usd_per_1k_get: f64,
    /// Broker compute share per GB moved (m7g.large at $0.0816/h sustaining ~100 MB/s).
    pub usd_per_gb_transfer: f64,
    /// MSK provisioned for comparison: 3× kafka.m7g.large brokers + 3× replicated gp3.
    pub msk_broker_usd_per_hour: f64,
    pub msk_brokers: u32,
    pub msk_storage_usd_per_gb_month: f64,
}

impl Default for Pricing {
    fn default() -> Self {
        Self {
            usd_per_gb_month_s3: 0.023,
            usd_per_gb_month_local: 0.08,
            usd_per_1k_put: 0.005,
            usd_per_1k_get: 0.0004,
            usd_per_gb_transfer: 0.000_23,
            msk_broker_usd_per_hour: 0.204,
            msk_brokers: 3,
            msk_storage_usd_per_gb_month: 0.10,
        }
    }
}

impl Config {
    pub fn local(home: PathBuf) -> Self {
        Self {
            storage: StorageConfig::Local(home.join("objects")),
            home: home.join("streams"),
            listen: "127.0.0.1:9092".parse().unwrap(),
            advertised_host: "127.0.0.1".into(),
            advertised_port: 9092,
            node_id: 1,
            rack: None,
            tls: None,
            default_stream: None,
            fsync: true,
            topics: Defaults::default(),
            upload: UploadConfig {
                min_bytes: 8 << 20,
                max_age_ms: 10_000,
                max_bytes: 32 << 20,
                interval_ms: 1_000,
                concurrency: 16,
            },
            groups: GroupConfig {
                initial_rebalance_delay_ms: 1_000,
                min_session_timeout_ms: 1_000,
                max_session_timeout_ms: 1_800_000,
                offsets_retention_ms: 7 * 86_400_000,
                max_size: 10_000,
            },
            txn: TxnConfig {
                max_timeout_ms: 900_000,
                id_expiration_ms: 7 * 86_400_000,
                producer_id_expiration_ms: 86_400_000,
            },
            max_request_bytes: 100 << 20,
            max_in_flight: 64,
            idle_unload_ms: 600_000,
            evict_on_unload: true,
            local_disk_bytes: 0,
            clean_interval_ms: 30_000,
            compaction_grace_ms: 60_000,
            remote_cache_bytes: 256 << 20,
            tail_cache_bytes: 1 << 20,
            tail_cache_total_bytes: 256 << 20,
            pricing: Pricing::default(),
            http_url: "http://127.0.0.1:7070".into(),
            http_require_auth: false,
        }
    }

    pub fn partition_dir(&self, topic_id: &uuid::Uuid, partition: i32) -> PathBuf {
        self.home.join("data").join(topic_id.simple().to_string()).join(partition.to_string())
    }

    pub fn topic_dir(&self, topic_id: &uuid::Uuid) -> PathBuf {
        self.home.join("data").join(topic_id.simple().to_string())
    }
}

/// Object key of a chunk: `streams/<stream>/<topic id>/<partition>/<first>-<last>.kb`. The offsets
/// in the name let the catalog be rebuilt from a listing.
pub fn chunk_key(stream: &str, topic_id: &uuid::Uuid, partition: i32, first: i64, last: i64) -> String {
    format!("streams/{stream}/{}/{partition}/{first:020}-{last:020}.kb", topic_id.simple())
}

/// Key of a chunk written by compaction (a new object, so the one it replaces stays readable
/// until it's deleted).
pub fn compacted_chunk_key(stream: &str, topic_id: &uuid::Uuid, partition: i32, first: i64, last: i64, generation: i64) -> String {
    format!("streams/{stream}/{}/{partition}/{first:020}-{last:020}.c{generation}.kb", topic_id.simple())
}

pub fn is_compacted_key(key: &str) -> bool {
    key.rsplit('/').next().is_some_and(|name| name.contains(".c"))
}

pub fn topic_prefix(stream: &str, topic_id: &uuid::Uuid) -> String {
    format!("streams/{stream}/{}/", topic_id.simple())
}

pub fn stream_prefix(stream: &str) -> String {
    format!("streams/{stream}/")
}
