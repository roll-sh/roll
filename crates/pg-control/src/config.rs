use pg_storage::StorageConfig;
use std::path::PathBuf;

/// The compute images, by major version: exact minors, pinned by their multi-arch index digest.
/// Moving one is a deliberate change (`pnpm test:pg` checks the decoy against it).
pub const IMAGES: [(u32, &str); 3] = [
    (16, "postgres:16.15-alpine@sha256:f4610795587492cd4fb887c2dc3de301b813278bb6f616992061141b5f15ac36"),
    (17, "postgres:17.11-alpine@sha256:eee6c6e1b0894fa4fe0075eb054af9fd1cf39a35321c2d4110626cfe506411fb"),
    (18, "postgres:18.7-alpine@sha256:ff64c9e3bdf8e2120107d3559de8db314e8305f7e38c5105903c6295876738ff"),
];

#[derive(Clone, Debug)]
pub struct Config {
    /// Root for state (`state.db`), the local data cache and the WAL spool.
    pub home: PathBuf,
    /// Postgres image template for computes (`{v}` is the major version); empty: [`IMAGES`].
    /// A decoy role (roll secrets' Postgres decoys) is answered by the proxy only on an image
    /// whose answers before a password roll knows (`pg_proxy::decoy::learn`); on any other its
    /// logins go to Postgres, waking a suspended compute. Use a `tag@sha256:` reference: a tag
    /// that moves under a suspended endpoint is told apart only at its next start.
    pub image: String,
    /// Major version for new projects (16, 17 and 18 are supported).
    pub default_pg_version: u32,
    /// Host and port clients use to reach the proxy (written into connection strings).
    pub public_host: String,
    pub public_port: u16,
    /// Behind roll cloud's edge: connection strings name `<endpoint>.<domain>` over TLS (the
    /// server name picks the endpoint) instead of `public_host` and `options=endpoint=`.
    pub public_domain: Option<String>,
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
            image: String::new(),
            default_pg_version: 18,
            public_host: "127.0.0.1".into(),
            public_domain: None,
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
        if self.image.is_empty() {
            return IMAGES.iter().find(|(v, _)| *v == pg_version).map_or_else(|| format!("postgres:{pg_version}-alpine"), |(_, i)| i.to_string());
        }
        self.image.replace("{v}", &pg_version.to_string())
    }

    pub fn mem_mb(&self, cu: f64) -> u64 {
        (cu * self.mem_mb_per_cu as f64).round() as u64
    }
}
