//! Where instances run (docs/apps/README.md, "The runtime"): [`docker::Docker`] on this host (one
//! node and development), [`fargate::Fargate`] on the fleet (builds by [`codebuild::CodeBuild`],
//! settings in [`aws::FleetConfig`]) and [`fake::Fake`] in process (tests).
//!
//! An instance is made ahead of time ([`Runtime::create`]: the image pulled, the container
//! created, nothing running), so a wake is a start, not a pull. [`Runtime::suspend`] freezes a
//! running instance where the runtime can (memory kept, no CPU); [`Runtime::stop`] stops it and
//! keeps it created.

pub mod aws;
pub mod codebuild;
pub mod docker;
pub mod engine;
pub mod fake;
pub mod fargate;

use crate::logs::LogSink;
use async_trait::async_trait;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Every container, network, volume and image roll apps makes carries this label (the service
/// id), and every name starts with [`PREFIX`].
pub const LABEL: &str = "sh.roll.apps";
pub const PREFIX: &str = "roll-apps-";

#[derive(Clone)]
pub struct InstanceSpec {
    /// `roll-apps-<service>-<env>-<n>-<random>`.
    pub name: String,
    pub service: String,
    pub env: String,
    pub deployment: String,
    pub image: String,
    /// Run as `sh -c <command>` instead of the image's own command.
    pub command: Option<String>,
    pub vars: Vec<(String, String)>,
    /// The port the app listens on (none for workers and cron runs).
    pub port: Option<u16>,
    pub cpu: f64,
    pub memory_mb: u32,
    /// The project's private network for this environment, and the names it answers to there.
    pub network: Option<String>,
    pub aliases: Vec<String>,
    /// A named volume and where it's mounted.
    pub volume: Option<(String, String)>,
    /// After this long with no traffic the instance may end itself (Fargate's guard proxy: a
    /// task whose node went away still stops). None: never (min_instances, workers).
    pub idle_stop_s: Option<u32>,
}

impl std::fmt::Debug for InstanceSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstanceSpec")
            .field("name", &self.name)
            .field("image", &self.image)
            .field("vars", &self.vars.iter().map(|(k, _)| k).collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// A private registry's login for one pull; the password never shows in `Debug`.
#[derive(Clone)]
pub struct RegistryLogin {
    pub server: String,
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for RegistryLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryLogin").field("server", &self.server).field("username", &self.username).finish_non_exhaustive()
    }
}

/// What an instance uses: CPU time since it started and memory resident now.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub cpu_ns: u64,
    pub memory_bytes: u64,
}

#[async_trait]
pub trait Runtime: Send + Sync {
    /// `docker`, `fargate`, `fake`.
    fn name(&self) -> &'static str;
    /// Whether [`Runtime::suspend`] freezes (Docker's pause) rather than stops.
    fn can_suspend(&self) -> bool;
    /// Pull an image (with `login` for a private registry) and answer it pinned by digest
    /// (`repo@sha256:…`).
    async fn pull(&self, image: &str, log: &LogSink, login: Option<&RegistryLogin>) -> anyhow::Result<String>;
    /// Make the instance, ready to start; nothing runs yet.
    async fn create(&self, spec: &InstanceSpec) -> anyhow::Result<()>;
    /// Start a created (or stopped) instance and say where its port answers. Its output goes
    /// to `log` until it stops. It may start with more CPU than its limit (a startup boost).
    async fn start(&self, name: &str, log: LogSink) -> anyhow::Result<Option<SocketAddr>>;
    /// The instance is up: it runs within its CPU limit from here.
    async fn settle(&self, _name: &str) {}
    /// Freeze a running instance (memory kept). Only called when [`Runtime::can_suspend`].
    async fn suspend(&self, name: &str) -> anyhow::Result<()>;
    async fn resume(&self, name: &str) -> anyhow::Result<()>;
    /// Stop it within `grace` (SIGTERM, then SIGKILL); it stays created.
    async fn stop(&self, name: &str, grace: Duration) -> anyhow::Result<()>;
    /// Remove everything of the instance; idempotent.
    async fn remove(&self, name: &str);
    /// Wait for a started instance to exit: its exit code.
    async fn wait(&self, name: &str) -> anyhow::Result<i32>;
    async fn running(&self, name: &str) -> bool;
    async fn usage(&self, name: &str) -> Option<Usage>;
    /// Names of every instance this runtime has for `service` (or for every service), made by
    /// any process of this node.
    async fn list(&self, service: Option<&str>) -> Vec<String>;
    /// Make the project's private network if it's missing.
    async fn ensure_network(&self, name: &str, service: &str) -> anyhow::Result<()>;
    /// Remove a volume (an environment's teardown).
    async fn remove_volume(&self, name: &str);
    /// A URL to a roll database as instances reach it.
    fn reach(&self, url: &str) -> String;
    /// An interactive shell in a running instance (`bash`, else `sh`) on a terminal of
    /// `cols`×`rows`: what's written is typed, what's read is the screen.
    async fn shell(&self, _name: &str, _cols: u16, _rows: u16) -> anyhow::Result<Shell> {
        anyhow::bail!("this runtime has no shells")
    }
    async fn resize_shell(&self, _shell: &str, _cols: u16, _rows: u16) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A shell's terminal stream.
pub trait ShellStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> ShellStream for T {}

pub struct Shell {
    /// What [`Runtime::resize_shell`] takes.
    pub id: String,
    pub io: Box<dyn ShellStream>,
}

/// One build: a tree with its Dockerfile, made into an image.
pub struct BuildJob {
    pub service: String,
    pub deployment: String,
    /// The build context (the source tree, with the Dockerfile written in).
    pub context: PathBuf,
    /// The Dockerfile, relative to the context.
    pub dockerfile: String,
    /// The image to make: `roll-apps-<service>:<deployment>`.
    pub tag: String,
    pub cpu: f64,
    pub memory_mb: u32,
    pub timeout: Duration,
    /// The environment's variables, for the build's steps: secrets in a generated Dockerfile,
    /// build arguments and secrets for the tree's own. Never in argv, a layer or a log.
    pub env: Vec<(String, String)>,
    /// A hash of `env` (`ROLL_BUILD_ENV`): a changed value rebuilds the steps that read it.
    pub env_hash: String,
    /// The tree's own Dockerfile (it reads variables with `ARG`), not a generated one.
    pub own_dockerfile: bool,
}

impl std::fmt::Debug for BuildJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildJob").field("tag", &self.tag).field("env", &self.env.iter().map(|(k, _)| k).collect::<Vec<_>>()).finish_non_exhaustive()
    }
}

/// Names the `docker` CLI reads itself: as a build's variables they'd change how the CLI runs.
pub fn docker_reads(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    ["DOCKER_", "BUILDKIT_", "BUILDX_", "COMPOSE_"].iter().any(|p| n.starts_with(p)) || ["PATH", "HOME", "HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY", "ALL_PROXY"].contains(&n.as_str())
}

#[async_trait]
pub trait Builder: Send + Sync {
    fn name(&self) -> &'static str;
    /// Build the image, its output going to `log`: the image reference instances run.
    async fn build(&self, job: &BuildJob, log: &LogSink, cancel: tokio_util::sync::CancellationToken) -> anyhow::Result<String>;
    /// Remove an image no deployment needs any more.
    async fn remove_image(&self, image: &str);
}

/// A free TCP port on loopback (to publish an instance's port on).
pub fn free_port() -> anyhow::Result<u16> {
    Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}
