//! Where customer code runs (docs/cloud/README.md, "Isolated work"). Checks and previews make
//! their container through an [`Isolation`]: [`crate::docker::Docker`] (the `docker` CLI on this
//! host: one node and development) or [`crate::fargate::Fargate`] (each run its own ECS task on
//! Fargate: the fleet, where no customer code runs on a node). `ROLL_CODE_RUNNER` picks one.
//!
//! What stays the same whichever runs it lives in the callers ([`crate::runner`],
//! [`crate::preview`]): a fork's code waits for `approve-run`, a check's timeout covers its run
//! and then everything it made is removed, and the tree comes from `Code::checkout`, read only
//! while [`Isolation::prepare`] copies it (never bind-mounted, never written into).

use crate::docker::Tail;
use async_trait::async_trait;
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Check,
    Preview,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Check => "check",
            Kind::Preview => "preview",
        }
    }

    /// Where the tree goes in the container, and its working directory.
    pub fn workdir(self) -> &'static str {
        match self {
            Kind::Check => "/workspace",
            Kind::Preview => "/app",
        }
    }
}

/// One run of customer code: what `roll.toml` asked for, and what roll adds.
#[derive(Clone)]
pub struct Job {
    pub kind: Kind,
    /// Unique among the runs going on: `roll-check-<random>`, `roll-preview-<change>`.
    pub name: String,
    pub image: String,
    /// Run as `sh -c <run>` in [`Kind::workdir`].
    pub run: String,
    /// `roll.toml`'s env and roll's (data URLs hold passwords).
    pub env: Vec<(String, String)>,
    pub memory_mb: u32,
    pub cpus: f64,
    /// A preview's app port.
    pub port: Option<u16>,
    /// The change a preview is for.
    pub change: Option<String>,
    /// The longest it may live, however its runner fares (a check's timeout plus preparing).
    pub deadline: Duration,
    /// A cache kept between runs (package downloads, build outputs), by name: mounted at
    /// [`CACHE_DIR`] where the isolation can keep one (Docker: a named volume).
    pub cache: Option<String>,
}

/// Where a job's cache is mounted, and the package managers' variables that point into it.
pub const CACHE_DIR: &str = "/roll-cache";
pub const CACHE_ENV: [(&str, &str); 9] = [
    ("npm_config_cache", "/roll-cache/npm"),
    ("npm_config_store_dir", "/roll-cache/pnpm"),
    ("YARN_CACHE_FOLDER", "/roll-cache/yarn"),
    ("BUN_INSTALL_CACHE_DIR", "/roll-cache/bun"),
    ("PIP_CACHE_DIR", "/roll-cache/pip"),
    ("UV_CACHE_DIR", "/roll-cache/uv"),
    ("CARGO_HOME", "/roll-cache/cargo"),
    ("GOMODCACHE", "/roll-cache/go/mod"),
    ("GOCACHE", "/roll-cache/go/build"),
];

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("kind", &self.kind)
            .field("name", &self.name)
            .field("image", &self.image)
            .field("env", &self.env.iter().map(|(k, _)| k).collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

#[async_trait]
pub trait Isolation: Send + Sync {
    /// `docker` or `fargate`.
    fn name(&self) -> &'static str;
    /// Make the job's container and copy `tree` into its working directory. Nothing runs yet, and
    /// once this returns the tree isn't needed any more.
    async fn prepare(&self, job: &Job, tree: &Path) -> anyhow::Result<()>;
    /// Run a prepared check to its end, its output going into `tail`: the exit code. A caller
    /// that gives up (a timeout) drops this and calls [`Isolation::remove`].
    async fn attach(&self, name: &str, tail: Arc<Mutex<Tail>>) -> anyhow::Result<i32>;
    /// Start a prepared preview whose app listens on `port`: where this node reaches it.
    async fn start(&self, name: &str, port: u16) -> anyhow::Result<SocketAddr>;
    async fn running(&self, name: &str) -> bool;
    /// The end of a job's output (why a preview didn't start).
    async fn logs_tail(&self, name: &str, lines: usize) -> String;
    /// Stop and remove everything made for the job of this name, prepared or running, by any
    /// node; idempotent.
    async fn remove(&self, name: &str);
    /// Stop and remove what this node made for the job, leaving what another node runs under
    /// the same name (a preview whose repo moved: its new owner may already run one).
    async fn release(&self, name: &str);
    /// Remove what a previous process of this node left of one kind.
    async fn remove_leftovers(&self, kind: Kind);
    /// A data URL (Postgres, Redis) as the job's container reaches it.
    fn reach(&self, url: &str) -> String;
    /// Drop a job cache ([`Job::cache`]); nothing where caches aren't kept.
    async fn drop_cache(&self, _name: &str) {}
}
