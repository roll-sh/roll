use pg_storage::StorageConfig;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    /// Root for state (`state.db`), the local data cache and the WAL spool.
    pub home: PathBuf,
    /// Postgres image template for computes; `{v}` is the major version.
    pub image: String,
    /// Major version for new projects (16, 17 and 18 are supported).
    pub default_pg_version: u32,
    /// Host and port clients use to reach the proxy (written into connection strings).
    pub public_host: String,
    pub public_port: u16,
    /// How computes reach this daemon's internal API (WAL for restores and replicas).
    pub internal_url: String,
    /// Shared secret for the internal API.
    pub internal_token: String,
    /// Memory per compute unit. Neon/AWS sizing is 1 CU = 1 vCPU + 4 GiB; laptops get less.
    pub mem_mb_per_cu: u64,
    /// Durable storage: S3 (or compatible) in production, a local directory in dev.
    pub storage: StorageConfig,
    /// Drop a suspended branch's local data (keeping it in object storage) after this long.
    pub evict_after_s: i64,
    /// Take a fresh base image this often while a branch is active, to keep restores fast.
    pub base_every_s: i64,
    /// …or after this much WAL since the last one.
    pub base_every_wal_bytes: u64,
    pub defaults: EndpointDefaults,
    pub pricing: Pricing,
}

#[derive(Clone, Debug)]
pub struct EndpointDefaults {
    pub min_cu: f64,
    pub max_cu: f64,
    pub suspend_timeout_s: i64,
    /// Point-in-time restore window for new projects.
    pub history_retention_s: i64,
}

/// AWS list prices used for the cost estimate (us-east-1, on-demand, Graviton).
#[derive(Clone, Debug, serde::Serialize)]
pub struct Pricing {
    /// m7g.large is 2 vCPU / 8 GiB at $0.0816/h → $0.0408 per (1 vCPU + 4 GiB).
    pub usd_per_cu_hour: f64,
    /// Local data of active branches on gp3 EBS, $/GB-month.
    pub usd_per_gb_month: f64,
    /// Everything durable (WAL, base images, evicted branches) on S3 Standard, $/GB-month.
    pub usd_per_gb_month_s3: f64,
}

impl Config {
    pub fn local(home: PathBuf) -> Self {
        Self {
            storage: StorageConfig::Local(home.join("objects")),
            home,
            image: "postgres:{v}-alpine".into(),
            default_pg_version: 18,
            public_host: "127.0.0.1".into(),
            public_port: 55432,
            internal_url: "http://host.docker.internal:7070".into(),
            internal_token: String::new(),
            mem_mb_per_cu: 1024,
            evict_after_s: 24 * 3600,
            base_every_s: 24 * 3600,
            base_every_wal_bytes: 1 << 30,
            defaults: EndpointDefaults {
                min_cu: 0.25,
                max_cu: 2.0,
                suspend_timeout_s: 300,
                history_retention_s: 7 * 86_400,
            },
            pricing: Pricing { usd_per_cu_hour: 0.0408, usd_per_gb_month: 0.08, usd_per_gb_month_s3: 0.023 },
        }
    }

    /// A branch data directory (local cache of a primary), by directory name.
    pub fn data_path(&self, name: &str) -> PathBuf {
        self.home.join("data").join(name)
    }

    /// A read replica's data directory, by directory name.
    pub fn replica_path(&self, name: &str) -> PathBuf {
        self.home.join("replicas").join(name)
    }

    /// Local WAL spool for a branch.
    pub fn spool_dir(&self, branch_id: &str) -> PathBuf {
        self.home.join("wal").join(branch_id)
    }

    pub fn image(&self, pg_version: u32) -> String {
        self.image.replace("{v}", &pg_version.to_string())
    }

    pub fn mem_mb(&self, cu: f64) -> u64 {
        (cu * self.mem_mb_per_cu as f64).round() as u64
    }
}
