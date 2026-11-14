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
pub mod netns;

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
    /// The private registry login its image pulls with, where the runtime pulls at each start
    /// (Fargate: the task's repository credentials). Never in `Debug`.
    pub registry: Option<RegistryLogin>,
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

/// A volume's files as a tar stream.
pub type VolumeStream = Box<dyn tokio::io::AsyncRead + Send + Unpin>;

/// How an instance's process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exit {
    pub code: Option<i32>,
    pub oom: bool,
}

impl Exit {
    /// Why it stopped, for a person: `memory_mb` is its limit.
    pub fn reason(&self, memory_mb: u32) -> String {
        match (self.oom, self.code) {
            (true, _) => format!("it ran out of memory (its limit is {memory_mb} MB)"),
            (false, Some(137)) => "it was killed (exit 137: out of memory, or a SIGKILL)".to_string(),
            (false, Some(c)) => format!("it exited with code {c}"),
            (false, None) => "it stopped".to_string(),
        }
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
    /// Whether a sleeping web instance's network accepted a connection since it went to sleep
    /// (its stop or suspend) or since it was last asked ([`netns`]: cheap, asked often): a
    /// caller rolld didn't see, one of the project's other services. False where the runtime
    /// can't tell (then only rolld's requests wake it).
    async fn take_callers(&self, _name: &str) -> bool {
        false
    }
    /// The container whose network a helper on this host may join to reach the instance from
    /// inside (`--network container:<it>`: its sandbox, or the instance itself). None where
    /// there's no such thing (Fargate, the fake).
    async fn network_container(&self, _name: &str) -> Option<String> {
        None
    }

    /// Connections to it now from the project's other services (not rolld's): dear, read only
    /// when it's about to sleep.
    async fn private_peers(&self, _name: &str) -> Option<u32> {
        None
    }
    /// Whether [`Runtime::suspend`] freezes (Docker's pause) rather than stops.
    fn can_suspend(&self) -> bool;
    /// Pull an image (with `login` for a private registry) and answer it pinned by digest
    /// (`repo@sha256:…`).
    async fn pull(&self, image: &str, log: &LogSink, login: Option<&RegistryLogin>) -> anyhow::Result<String>;
    /// What `image`'s tag points at now in its registry (`repo@sha256:…`), without pulling it:
    /// a manifest `HEAD` (watching a tag).
    async fn remote_digest(&self, image: &str, login: Option<&RegistryLogin>) -> anyhow::Result<String> {
        crate::registry::Registry::new(None)?.digest(image, login).await
    }
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
    /// How a stopped instance ended, when the runtime can say: its exit code, and whether it
    /// was killed for running out of memory.
    async fn exited(&self, _name: &str) -> Option<Exit> {
        None
    }
    async fn usage(&self, name: &str) -> Option<Usage>;
    /// Names of every instance this runtime has for `service` (or for every service), made by
    /// any process of this node.
    async fn list(&self, service: Option<&str>) -> Vec<String>;
    /// Make the project's private network if it's missing.
    async fn ensure_network(&self, name: &str, service: &str) -> anyhow::Result<()>;
    /// Remove a volume (an environment's teardown).
    async fn remove_volume(&self, name: &str);
    /// The files under `dir` in `image`, as a tar stream (a static site's, to serve without an
    /// instance).
    async fn export_files(&self, _image: &str, _dir: &str) -> anyhow::Result<VolumeStream> {
        anyhow::bail!("this runtime doesn't export files")
    }
    /// A volume's files as a tar stream, read-only (a backup).
    async fn read_volume(&self, _name: &str) -> anyhow::Result<VolumeStream> {
        anyhow::bail!("this runtime has no volume backups")
    }
    /// Replace a volume's files with a tar stream's (a restore): nothing may run on it meanwhile.
    async fn write_volume(&self, _name: &str, _tar: VolumeStream) -> anyhow::Result<()> {
        anyhow::bail!("this runtime has no volume backups")
    }
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
    /// The Dockerfile: relative to the context, or a path outside it (a copy with its cache
    /// mounts namespaced, [`Builder::shares_caches`]).
    pub dockerfile: String,
    /// The image to make: `roll-apps-<service>:<deployment>`.
    pub tag: String,
    pub cpu: f64,
    pub memory_mb: u32,
    pub timeout: Duration,
    /// The environment's variables, for the build's steps: BuildKit secrets, and for the tree's
    /// own Dockerfile also the build arguments it declares ([`build_args`]). Never in argv, a
    /// layer or a log, and never in the environment of roll's own processes but as those
    /// declared build arguments.
    pub env: Vec<(String, String)>,
    /// A hash of `env` (`ROLL_BUILD_ENV`): a changed value rebuilds the steps that read it.
    pub env_hash: String,
    /// The tree's own Dockerfile (it reads variables with `ARG`), not a generated one.
    pub own_dockerfile: bool,
    /// Use no cached layer and pull base images again (`--no-cache --pull`).
    pub no_cache: bool,
    /// [`apps_build::LAYERS_ARG`]'s value on a builder that shares caches: a secret per service
    /// that keeps its `RUN` steps' cached layers its own. Never in argv or a log.
    pub layers: Option<String>,
}

impl std::fmt::Debug for BuildJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildJob").field("tag", &self.tag).field("env", &self.env.iter().map(|(k, _)| k).collect::<Vec<_>>()).finish_non_exhaustive()
    }
}

/// Names that steer a process (the loader, Go's runtime, the shell, TLS trust, the docker CLI's
/// own settings): never set in the environment of a process of roll's, so never a build argument.
/// Every value still reaches the build as a BuildKit secret, which isn't an environment.
pub fn steers_a_process(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    const PREFIXES: [&str; 16] = ["DOCKER_", "BUILDKIT_", "BUILDX_", "COMPOSE_", "LD_", "DYLD_", "GCONV_", "MALLOC_", "SSL_", "XDG_", "BASH_FUNC_", "AWS_", "OTEL_", "GRPC_", "SSH_", "GIT_"];
    const NAMES: [&str; 30] = [
        "PATH", "HOME", "USER", "SHELL", "TMPDIR", "ENV", "BASH_ENV", "IFS", "CDPATH", "SHELLOPTS", "BASHOPTS", "PS4", "PROMPT_COMMAND", "HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY", "ALL_PROXY",
        "HOSTALIASES", "LOCPATH", "LOCALDOMAIN", "NLSPATH", "RES_OPTIONS", "TZDIR", "GODEBUG", "GOGC", "GOMAXPROCS", "GOMEMLIMIT", "GOTRACEBACK", "GORACE", "GOFLAGS",
    ];
    PREFIXES.iter().any(|p| n.starts_with(p)) || NAMES.contains(&n.as_str())
}

/// The build arguments of a tree's own Dockerfile: the job's variables its `ARG` lines declare,
/// minus names that steer a process. A generated Dockerfile takes none (it reads secrets).
pub fn build_args(job: &BuildJob) -> Vec<String> {
    if !job.own_dockerfile {
        return Vec::new();
    }
    let text = match std::fs::File::open(job.context.join(&job.dockerfile)) {
        Ok(f) => {
            let mut buf = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::Read::take(f, 1 << 20), &mut buf);
            buf
        }
        Err(_) => return Vec::new(),
    };
    let declared = declared_args(&text);
    job.env.iter().map(|(k, _)| k).filter(|k| declared.contains(k.as_str()) && !steers_a_process(k)).cloned().collect()
}

/// The names a Dockerfile's `ARG` lines declare (`ARG A`, `ARG B=default C`).
pub fn declared_args(dockerfile: &str) -> std::collections::HashSet<&str> {
    dockerfile
        .lines()
        .filter_map(|l| {
            let l = l.trim_start();
            (l.len() > 4 && l[..4].eq_ignore_ascii_case("ARG ")).then(|| &l[4..])
        })
        .flat_map(|rest| rest.split_whitespace())
        .map(|t| t.split('=').next().unwrap_or(t))
        .filter(|n| !n.is_empty() && n != &"\\")
        .collect()
}

#[async_trait]
pub trait Builder: Send + Sync {
    fn name(&self) -> &'static str;
    /// Build the image, its output going to `log`: the image reference instances run.
    async fn build(&self, job: &BuildJob, log: &LogSink, cancel: tokio_util::sync::CancellationToken) -> anyhow::Result<String>;
    /// Remove an image no deployment needs any more.
    async fn remove_image(&self, image: &str);
    /// Whether its builds share one BuildKit, cache mounts and all (one node's Docker): then
    /// each service's mounts are namespaced (`apps_build::isolate_caches`).
    fn shares_caches(&self) -> bool {
        false
    }
}

/// A free TCP port on loopback (to publish an instance's port on).
pub fn free_port() -> anyhow::Result<u16> {
    Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_declared_build_arguments_that_steer_nothing_reach_the_cli() {
        let dir = std::env::temp_dir().join(format!("roll-apps-args-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Dockerfile"), "FROM x\narg NPM_TOKEN\nARG A=1 LD_PRELOAD\nRUN echo $UNDECLARED\n").unwrap();
        let vars = ["NPM_TOKEN", "A", "LD_PRELOAD", "UNDECLARED"].map(|k| (k.to_string(), "v".to_string())).to_vec();
        let mut job = BuildJob {
            service: "svc".into(),
            deployment: "dep".into(),
            tag: "t".into(),
            context: dir.clone(),
            dockerfile: "Dockerfile".into(),
            cpu: 1.0,
            memory_mb: 512,
            timeout: Duration::from_secs(60),
            env: vars,
            env_hash: String::new(),
            own_dockerfile: true,
            no_cache: false,
            layers: None,
        };
        assert_eq!(build_args(&job), ["NPM_TOKEN", "A"]);
        job.own_dockerfile = false;
        assert!(build_args(&job).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
        for n in ["BASH_ENV", "ld_preload", "GODEBUG", "DOCKER_HOST", "SSL_CERT_FILE", "PATH"] {
            assert!(steers_a_process(n), "{n}");
        }
        for n in ["NPM_TOKEN", "GOOGLE_API_KEY", "LOCATION", "DATABASE_URL"] {
            assert!(!steers_a_process(n), "{n}");
        }
    }
}
