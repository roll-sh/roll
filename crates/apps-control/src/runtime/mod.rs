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
pub mod egress;
pub mod ingress;
pub mod engine;
pub mod fake;
pub mod fargate;
pub mod links;
pub mod netns;
pub mod scan;

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
    /// Other ports of the app a custom domain sends to (published as `port` is; never a closed
    /// instance's), each reached through [`Runtime::port_address`].
    pub ports: Vec<u16>,
    /// `port` speaks UDP (`run.udp`): published over UDP too, reached through
    /// [`Runtime::udp_address`].
    pub udp: bool,
    pub cpu: f64,
    pub memory_mb: u32,
    /// The project's private network for this environment, and the names it answers to there.
    pub network: Option<String>,
    /// Its network has no route out (`egress` `roll`): it's reached at its address there, never
    /// through a published port.
    pub closed: bool,
    pub aliases: Vec<String>,
    /// A named volume and where it's mounted.
    pub volume: Option<(String, String)>,
    /// After this long with no traffic the instance may end itself (Fargate's guard proxy: a
    /// task whose node went away still stops). None: never (min_instances, workers).
    pub idle_stop_s: Option<u32>,
    /// Variables kept as files (`files` on the environment's variables): (absolute path,
    /// contents), written into the instance before it starts. Never in `Debug`.
    pub files: Vec<(String, String)>,
    /// The private registry login its image pulls with, where the runtime pulls at each start
    /// (Fargate: the task's repository credentials). Never in `Debug`.
    pub registry: Option<RegistryLogin>,
    /// A worker's health check, run by the runtime in the instance ([`Runtime::unhealthy`]).
    pub health: Option<HealthCheck>,
    /// Containers beside it in its network namespace ([`crate::model::Run::sidecars`]),
    /// started after it and frozen, stopped and removed with it.
    pub sidecars: Vec<SidecarSpec>,
}

/// A sidecar as the runtime starts it: its image pinned, its variables resolved.
#[derive(Clone, PartialEq)]
pub struct SidecarSpec {
    pub name: String,
    pub image: String,
    pub command: Option<String>,
    /// Its env settings and the variables it named. Never in `Debug`.
    pub vars: Vec<(String, String)>,
    pub cpu: f64,
    pub memory_mb: u32,
}

impl std::fmt::Debug for SidecarSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidecarSpec").field("name", &self.name).field("image", &self.image).field("vars", &self.vars.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>()).finish_non_exhaustive()
    }
}

impl InstanceSpec {
    /// What its sidecars are given, CPUs and MB: metered and scaled on with the instance.
    pub fn sidecars_size(&self) -> (f64, u32) {
        self.sidecars.iter().fold((0.0, 0), |(c, m), s| (c + s.cpu, m + s.memory_mb))
    }
}

/// A sidecar's container: `<instance>-sc-<name>`.
pub fn sidecar_name(instance: &str, sidecar: &str) -> String {
    format!("{instance}-sc-{sidecar}")
}

/// A command run in an instance every `every`, healthy on exit 0; `failures` in a row make it
/// unhealthy. `start` is the grace before failures count.
#[derive(Clone, Debug, PartialEq)]
pub struct HealthCheck {
    pub command: String,
    pub every: Duration,
    pub failures: u32,
    pub start: Duration,
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
    /// Where a started instance answers on another of its ports ([`InstanceSpec::ports`]).
    async fn port_address(&self, _name: &str, _port: u16) -> Option<SocketAddr> {
        None
    }
    /// Where a UDP service's instance takes its datagrams on this host (its port published over
    /// UDP on loopback). None: not published so (a runtime without UDP).
    async fn udp_address(&self, _name: &str, _port: u16) -> Option<SocketAddr> {
        None
    }
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
    /// Whether its health check ([`InstanceSpec::health`]) failed `failures` times in a row. A
    /// runtime that runs no checks answers false.
    async fn unhealthy(&self, _name: &str) -> bool {
        false
    }
    /// Take on `spec.name`, still running from another process (a node that died, rolld before
    /// a restart) and of `spec.deployment` (its label or tag says so), as this process's own,
    /// its output followed into `log` from now. None: not running here, another deployment's,
    /// or a runtime that can't (it's removed, and a fresh one made). For
    /// [`crate::live::EnvLive::adopt`].
    async fn adopt(&self, _spec: &InstanceSpec, _log: LogSink) -> Option<Adopted> {
        None
    }
    /// A service's registry login changed (`login`), or went (None: removed, its registry
    /// changed, or the service deleted), wherever the runtime keeps a copy (Fargate: its Secrets
    /// Manager secret, and the instances made with it).
    async fn registry_login_changed(&self, _service: &str, _login: Option<&RegistryLogin>) {}
    /// Everything the runtime keeps for a service beyond its instances (its registry login, its
    /// deployments' values), the service being deleted.
    async fn forget_service(&self, service: &str) {
        self.registry_login_changed(service, None).await;
    }
    /// Clean up what a node that died left behind outside its instances (Fargate: environment
    /// files no task will read), and what's kept for deployments outside `in_use` (the ids of
    /// those that may still run). Every ten minutes, on every node (it only deletes).
    async fn sweep(&self, _in_use: &std::collections::HashSet<String>) {}
    /// Where `name`'s app listens, when the runtime can see (Docker: its sandbox's sockets):
    /// what to tell a user whose app doesn't answer on its port.
    async fn listeners(&self, _name: &str) -> Option<Vec<SocketAddr>> {
        None
    }
    /// Forget `name` without stopping it: another node keeps it now ([`Self::shares_instances`]).
    async fn release(&self, _name: &str) {}
    /// Whether what one node made, another can see and run (Fargate; not one host's Docker per
    /// node): a node giving a service up then leaves its instances for the next to keep.
    fn shares_instances(&self) -> bool {
        false
    }
    /// How a stopped instance ended, when the runtime can say: its exit code, and whether it
    /// was killed for running out of memory.
    async fn exited(&self, _name: &str) -> Option<Exit> {
        None
    }
    async fn usage(&self, name: &str) -> Option<Usage>;
    /// Names of every instance this runtime has for `service` (or for every service), made by
    /// any process of this node.
    async fn list(&self, service: Option<&str>) -> Vec<String>;
    /// Names of everything this runtime has for `service`'s environment `env` (instances, their
    /// sandboxes and wakers, runs), whoever made them: what an environment's teardown removes
    /// before its network goes. None where a teardown of the live state is enough.
    async fn list_env(&self, _service: &str, _env: &str) -> Vec<String> {
        Vec::new()
    }
    /// Make the project's private network if it's missing; `closed`: one with no route out
    /// ([`Runtime::closes_egress`]).
    async fn ensure_network(&self, name: &str, service: &str, closed: bool) -> anyhow::Result<()>;
    /// Whether it runs environments with no way out but roll (`egress` `roll`): a network with
    /// no route out, and [`Runtime::egress_proxy`] for the hosts they may still reach.
    fn closes_egress(&self) -> bool {
        false
    }
    /// The egress proxy for `hosts` serving closed `network`: the URL its instances use as
    /// `HTTP_PROXY`/`HTTPS_PROXY` ([`egress`]).
    async fn egress_proxy(&self, _network: &str, _service: &str, _hosts: &[String]) -> anyhow::Result<String> {
        anyhow::bail!("this runtime has no egress proxy")
    }
    /// Where a closed environment of `project` builds ([`BuildEgress`]): a network of the
    /// project's own with no route out and no way to the host, whose only way out is an egress
    /// proxy to public hosts. None: this runtime doesn't close builds.
    async fn build_egress(&self, _project: &str) -> anyhow::Result<Option<BuildEgress>> {
        Ok(None)
    }
    /// The interception (roll secrets' placeholders swapped for the hosts its keys are bound to)
    /// a closed network's egress proxies run for those hosts, or none.
    fn set_interception(&self, _network: &str, _i: Option<std::sync::Arc<dyn roll_intercept::Interception>>) {}
    /// The project's closed builds' builder and network, once it has no service left.
    async fn remove_build_egress(&self, _project: &str) {}
    /// Closed builds' networks nothing runs on any more (a process that stopped without
    /// deleting its projects left them, its proxy since stopped): gone with their builders, so
    /// they don't use up the host's address pools. A rolld still building has its proxy running
    /// on its network; one that builds again makes it again. Never one of `projects` (this
    /// node's projects with a closed environment: a build may be setting it up).
    async fn sweep_build_networks(&self, _projects: &std::collections::HashSet<String>) {}
    /// The hosts closed `network`'s egress proxy for `hosts` refused lately (none: it hasn't, or
    /// isn't here).
    /// What a closed network's interception did on this node (none: nothing, or no proxy here).
    fn intercepted(&self, _network: &str) -> Option<egress::Intercepted> {
        None
    }
    fn egress_refused(&self, _network: &str, _hosts: &[String]) -> Vec<egress::Refused> {
        Vec::new()
    }
    /// A project environment's network nothing uses any more (its last service there went):
    /// best effort, left if anything is still attached.
    async fn remove_network(&self, _name: &str) {}
    /// Remove a volume (an environment's teardown).
    async fn remove_volume(&self, name: &str);
    /// The files under `dir` in `image`, as a tar stream (a static site's, to serve without an
    /// instance).
    async fn export_files(&self, _image: &str, _dir: &str) -> anyhow::Result<VolumeStream> {
        anyhow::bail!("this runtime doesn't export files")
    }
    /// The bytes a volume's files take (how full it is), measured; none when this runtime can't
    /// say or the volume doesn't exist.
    async fn volume_used(&self, _name: &str) -> Option<u64> {
        None
    }
    /// What's at `path` in instance `name` (a file or a directory), as a tar stream (`roll apps
    /// cp`, Docker's `docker cp`).
    async fn copy_from(&self, _name: &str, _path: &str) -> anyhow::Result<VolumeStream> {
        anyhow::bail!("this runtime can't copy files out of an instance")
    }
    /// A tar's files written under `dir` in instance `name`.
    async fn copy_to(&self, _name: &str, _dir: &str, _tar: VolumeStream) -> anyhow::Result<()> {
        anyhow::bail!("this runtime can't copy files into an instance")
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
    /// A terminal in instance `name`: a login shell, or `command` (`sh -c`) when one is given.
    async fn shell(&self, _name: &str, _command: Option<&str>, _cols: u16, _rows: u16) -> anyhow::Result<Shell> {
        anyhow::bail!("this runtime has no shells")
    }
    /// `sh -c <command>` in a running container, without a terminal: its exit code and output,
    /// each stream kept to `max` bytes (the rest read and dropped), killed after `timeout`.
    async fn exec_output(&self, _name: &str, _command: &str, _timeout: std::time::Duration, _max: usize) -> anyhow::Result<ExecOutput> {
        anyhow::bail!("this runtime can't run a command in a container")
    }
    async fn resize_shell(&self, _shell: &str, _cols: u16, _rows: u16) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A shell's terminal stream.
pub trait ShellStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> ShellStream for T {}

/// What [`Runtime::exec_output`] ran to.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExecOutput {
    /// None: killed at its timeout.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// More than `max` bytes on a stream: the rest was dropped.
    pub truncated: bool,
}

pub struct Shell {
    /// What [`Runtime::resize_shell`] takes.
    pub id: String,
    pub io: Box<dyn ShellStream>,
}

/// One build: a tree with its Dockerfile, made into an image.
/// A closed build's network ([`Runtime::build_egress`]): its builder runs there, and its steps
/// reach out only through `proxy` (an address, not a name: BuildKit's steps resolve with
/// public DNS, not the network's).
#[derive(Clone, Debug, PartialEq)]
pub struct BuildEgress {
    pub network: String,
    /// `http://<address>:3128`.
    pub proxy: String,
    /// The buildx builder that runs on `network` (the project's own).
    pub builder: String,
}

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
    /// The Dockerfile's stage to build (`--target`; checked to be a stage's name).
    pub target: Option<String>,
    /// [`apps_build::LAYERS_ARG`]'s value on a builder that shares caches: a secret per service
    /// that keeps its `RUN` steps' cached layers its own. Never in argv or a log.
    pub layers: Option<String>,
    /// A closed environment's build: on this network, through its proxy, not the default one.
    pub egress: Option<BuildEgress>,
    /// Not a preview's (Vercel's prioritized production builds): a builder with a queue lets it
    /// past a burst of previews' builds.
    pub priority: bool,
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

/// An instance [`Runtime::adopt`] took on: where it answers (None for a worker), and whether
/// it's frozen (suspended, asleep).
#[derive(Clone, Copy, Debug)]
pub struct Adopted {
    pub addr: Option<SocketAddr>,
    pub paused: bool,
}

#[async_trait]
pub trait Builder: Send + Sync {
    fn name(&self) -> &'static str;
    /// Build the image, its output going to `log`: the image reference instances run.
    async fn build(&self, job: &BuildJob, log: &LogSink, cancel: tokio_util::sync::CancellationToken) -> anyhow::Result<String>;
    /// Remove an image no deployment needs any more.
    async fn remove_image(&self, image: &str);
    /// An image built elsewhere, its archive's names already taken out
    /// ([`crate::image_upload::untagged`]), loaded and tagged `tag`: the image reference.
    async fn load(&self, _archive: &std::path::Path, _tag: &str, _log: &LogSink) -> anyhow::Result<String> {
        anyhow::bail!("this node's builder can't take an uploaded image yet: push it to a registry and deploy it by name")
    }
    /// The known vulnerabilities in an image this node has (a pinned scanner, offline, with no
    /// access to anything but the image): None when this builder doesn't scan.
    async fn scan(&self, _image: &str, _service: &str, _log: &LogSink) -> anyhow::Result<Option<Scanned>> {
        Ok(None)
    }
    /// Whether its builds share one BuildKit, cache mounts and all (one node's Docker): then
    /// each service's mounts are namespaced (`apps_build::isolate_caches`).
    fn shares_caches(&self) -> bool {
        false
    }
    /// Whether a build gets the CPU and memory its job asks (`build.size`), so its minutes are
    /// metered by that size; one node's BuildKit takes no per-build limit.
    fn sizes_builds(&self) -> bool {
        false
    }
}

/// What a scan found, and what scanned it (`trivy 0.76.1`).
pub struct Scanned {
    pub scanner: String,
    pub found: Vec<crate::model::Vulnerability>,
    /// Its CycloneDX SBOM (JSON), when the scanner makes one.
    pub sbom: Option<Vec<u8>>,
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
            target: None,
            layers: None,
            egress: None,
            priority: true,
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
