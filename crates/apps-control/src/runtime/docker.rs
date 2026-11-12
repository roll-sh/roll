//! Instances as containers on this host, through the `docker` CLI (one node and development).
//!
//! - A container is created at deploy time with its port published on a loopback port chosen
//!   then, so a wake is `docker start` and nothing else (no create, no `docker port`).
//! - Variables go in through an `--env-file` written 0600 and removed right after the create, so
//!   they never show in a process list.
//! - Suspending is `docker pause` (the cgroup freezer): memory stays, CPU stops, and `unpause`
//!   answers in milliseconds.
//! - CPU and memory come straight from the container's cgroup (`cpu.stat`, `memory.current`),
//!   which costs a file read, not a `docker stats` call.
//! - A wake's calls (start, unpause, logs, inspect, wait, stop) go straight to the daemon's socket
//!   ([`Engine`]) when it's local: no `docker` process per call. Everything made at deploy time
//!   (pull, create, build, networks) stays on the CLI.

use super::engine::Engine;
use super::{BuildJob, Builder, InstanceSpec, LABEL, Runtime, Usage, free_port};
use crate::logs::{LogSink, Stream, pump};
use anyhow::{Context, bail};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// A `docker` call that should be quick: a hung daemon fails it after a minute.
pub async fn docker(args: &[&str]) -> anyhow::Result<String> {
    docker_within(args, Some(Duration::from_secs(60))).await
}

pub async fn docker_within(args: &[&str], within: Option<Duration>) -> anyhow::Result<String> {
    tracing::debug!(?args, "docker");
    let run = Command::new("docker").args(args).stdin(Stdio::null()).kill_on_drop(true).output();
    let out = match within {
        Some(d) => tokio::time::timeout(d, run).await.map_err(|_| anyhow::anyhow!("docker {} didn't finish within {} s", args.first().unwrap_or(&""), d.as_secs()))?,
        None => run.await,
    }
    .context("running docker")?;
    if !out.status.success() {
        bail!("docker {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub async fn available() -> bool {
    docker(&["info", "--format", "{{.ServerVersion}}"]).await.is_ok()
}

#[derive(Default)]
struct Known {
    /// Container name → (full id, published host port).
    containers: HashMap<String, (String, Option<u16>)>,
}

pub struct Docker {
    known: Mutex<Known>,
    /// Where env files are written for the moment of a create.
    scratch: PathBuf,
    /// The daemon's socket, when it's on this host.
    engine: Option<Engine>,
}

impl Docker {
    pub fn new(scratch: PathBuf) -> Docker {
        // Env files a stopped process left behind (they hold variables' values).
        let _ = std::fs::remove_dir_all(&scratch);
        Docker { known: Mutex::new(Known::default()), scratch, engine: Engine::local() }
    }

    async fn id_and_port(&self, name: &str) -> anyhow::Result<(String, Option<u16>)> {
        if let Some(k) = self.known.lock().containers.get(name).cloned() {
            return Ok(k);
        }
        // Made by an earlier process of this node: ask docker.
        if let Some(e) = &self.engine {
            let i = e.inspect(name).await?;
            self.known.lock().containers.insert(name.to_string(), (i.id.clone(), i.host_port));
            return Ok((i.id, i.host_port));
        }
        let id = docker(&["inspect", "-f", "{{.Id}}", name]).await?;
        let ports = docker(&["inspect", "-f", "{{range $p, $b := .HostConfig.PortBindings}}{{range $b}}{{.HostPort}} {{end}}{{end}}", name]).await?;
        let port = ports.split_whitespace().next().and_then(|p| p.parse().ok());
        self.known.lock().containers.insert(name.to_string(), (id.clone(), port));
        Ok((id, port))
    }

    fn cgroup_dir(id: &str) -> Option<PathBuf> {
        [format!("/sys/fs/cgroup/system.slice/docker-{id}.scope"), format!("/sys/fs/cgroup/docker/{id}")]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.is_dir())
    }
}

/// Rewrite loopback hosts in a URL for a container (`host.docker.internal`).
pub fn for_container(url: &str) -> String {
    let mut out = url.to_string();
    for host in ["127.0.0.1", "localhost", "[::1]"] {
        for sep in ["@", "//"] {
            out = out.replace(&format!("{sep}{host}:"), &format!("{sep}host.docker.internal:"));
            out = out.replace(&format!("{sep}{host}/"), &format!("{sep}host.docker.internal/"));
        }
    }
    out
}

#[async_trait]
impl Runtime for Docker {
    fn name(&self) -> &'static str {
        "docker"
    }

    fn can_suspend(&self) -> bool {
        true
    }

    async fn pull(&self, image: &str, log: &LogSink) -> anyhow::Result<String> {
        log.line(format!("pulling {image}"));
        let mut child = Command::new("docker")
            .args(["pull", image])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("running docker pull")?;
        let out = tokio::spawn(pump(child.stdout.take().expect("piped"), log.clone()));
        let err = tokio::spawn(pump(child.stderr.take().expect("piped"), log.stream(Stream::System)));
        let status = child.wait().await?;
        let _ = (out.await, err.await);
        if !status.success() {
            bail!("docker pull {image} failed");
        }
        let digest = docker(&["image", "inspect", "-f", "{{if .RepoDigests}}{{index .RepoDigests 0}}{{else}}{{.Id}}{{end}}", image]).await?;
        Ok(if digest.is_empty() { image.to_string() } else { digest })
    }

    async fn create(&self, spec: &InstanceSpec) -> anyhow::Result<()> {
        let mut args: Vec<String> = vec![
            "create".into(),
            "--name".into(),
            spec.name.clone(),
            "--label".into(),
            format!("{LABEL}={}", spec.service),
            "--label".into(),
            format!("{LABEL}.env={}", spec.env),
            "--label".into(),
            format!("{LABEL}.deployment={}", spec.deployment),
            "--memory".into(),
            format!("{}m", spec.memory_mb),
            "--memory-swap".into(),
            format!("{}m", spec.memory_mb),
            "--cpus".into(),
            format!("{}", spec.cpu),
            "--pids-limit".into(),
            "4096".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
            "--restart".into(),
            "no".into(),
            // A tiny init as PID 1: signals reach the app (a stop takes milliseconds, not the
            // grace period) and zombies are reaped.
            "--init".into(),
            "--add-host".into(),
            "host.docker.internal:host-gateway".into(),
            "--log-driver".into(),
            "local".into(),
        ];
        let host_port = match spec.port {
            Some(p) => {
                let hp = free_port()?;
                args.extend(["--publish".into(), format!("127.0.0.1:{hp}:{p}")]);
                Some(hp)
            }
            None => None,
        };
        if let Some(net) = &spec.network {
            args.extend(["--network".into(), net.clone()]);
            for a in &spec.aliases {
                args.extend(["--network-alias".into(), a.clone()]);
            }
        }
        if let Some((vol, mount)) = &spec.volume {
            args.extend(["--volume".into(), format!("{vol}:{mount}")]);
        }
        // The env file: KEY=VALUE lines; docker reads values verbatim (no quoting), so values
        // with a newline can't go this way.
        tokio::fs::create_dir_all(&self.scratch).await?;
        let env_path = self.scratch.join(format!("{}.env", spec.name));
        let mut env = String::new();
        for (k, v) in &spec.vars {
            if v.contains('\n') {
                bail!("variable {k} has a line break, which a container's environment can't take from an env file");
            }
            env.push_str(&format!("{k}={v}\n"));
        }
        write_private(&env_path, env.as_bytes()).await?;
        args.extend(["--env-file".into(), env_path.display().to_string()]);
        match &spec.command {
            Some(cmd) => args.extend(["--entrypoint".into(), "sh".into(), spec.image.clone(), "-c".into(), cmd.clone()]),
            None => args.push(spec.image.clone()),
        }
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let made = docker(&argv).await;
        let _ = tokio::fs::remove_file(&env_path).await;
        let id = made?;
        self.known.lock().containers.insert(spec.name.clone(), (id, host_port));
        Ok(())
    }

    async fn start(&self, name: &str, log: LogSink) -> anyhow::Result<Option<SocketAddr>> {
        let now = chrono::Utc::now();
        let (o, e) = (log.stream(Stream::Stdout), log.stream(Stream::Stderr));
        if let Some(engine) = &self.engine {
            engine.start(name).await?;
            let (_, port) = self.id_and_port(name).await?;
            // Follow its output until it stops (the logs request ends with the container).
            let (engine, name) = (engine.clone(), name.to_string());
            tokio::spawn(async move {
                let since = (now.timestamp(), now.timestamp_subsec_nanos());
                if let Err(err) = engine.follow(&name, since, o.writer(), e.writer()).await {
                    tracing::debug!(%name, "following logs: {err:#}");
                }
            });
            return Ok(port.map(|p| SocketAddr::from(([127, 0, 0, 1], p))));
        }
        let since = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        docker(&["start", name]).await?;
        let (_, port) = self.id_and_port(name).await?;
        // Follow its output until it stops (`docker logs -f` ends with the container).
        if let Ok(mut child) = Command::new("docker")
            .args(["logs", "-f", "--since", &since, name])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            let out = child.stdout.take().expect("piped");
            let err = child.stderr.take().expect("piped");
            tokio::spawn(async move {
                let a = tokio::spawn(pump(out, o));
                let b = tokio::spawn(pump(err, e));
                let _ = (a.await, b.await);
                let _ = child.wait().await;
            });
        }
        Ok(port.map(|p| SocketAddr::from(([127, 0, 0, 1], p))))
    }

    async fn suspend(&self, name: &str) -> anyhow::Result<()> {
        match &self.engine {
            Some(e) => e.pause(name).await,
            None => docker(&["pause", name]).await.map(|_| ()),
        }
    }

    async fn resume(&self, name: &str) -> anyhow::Result<()> {
        match &self.engine {
            Some(e) => e.unpause(name).await,
            None => docker(&["unpause", name]).await.map(|_| ()),
        }
    }

    async fn stop(&self, name: &str, grace: Duration) -> anyhow::Result<()> {
        // A paused container must thaw before it can take SIGTERM.
        let _ = self.resume(name).await;
        let secs = grace.as_secs().max(1);
        match &self.engine {
            Some(e) => e.stop(name, secs).await,
            None => docker_within(&["stop", "-t", &secs.to_string(), name], Some(Duration::from_secs(secs + 60))).await.map(|_| ()),
        }
    }

    async fn remove(&self, name: &str) {
        let _ = docker(&["rm", "-f", "-v", name]).await;
        self.known.lock().containers.remove(name);
    }

    async fn wait(&self, name: &str) -> anyhow::Result<i32> {
        if let Some(e) = &self.engine {
            return e.wait(name).await;
        }
        let code = docker_within(&["wait", name], None).await?;
        Ok(code.trim().parse().unwrap_or(-1))
    }

    async fn running(&self, name: &str) -> bool {
        if let Some(e) = &self.engine {
            return e.inspect(name).await.is_ok_and(|i| i.running);
        }
        docker(&["inspect", "-f", "{{.State.Running}}", name]).await.is_ok_and(|s| s == "true")
    }

    async fn usage(&self, name: &str) -> Option<Usage> {
        let (id, _) = self.id_and_port(name).await.ok()?;
        let dir = Self::cgroup_dir(&id)?;
        let stat = tokio::fs::read_to_string(dir.join("cpu.stat")).await.ok()?;
        let usec: u64 = stat.lines().find_map(|l| l.strip_prefix("usage_usec ")).and_then(|v| v.trim().parse().ok())?;
        let mem: u64 = tokio::fs::read_to_string(dir.join("memory.current")).await.ok()?.trim().parse().ok()?;
        Some(Usage { cpu_ns: usec * 1000, memory_bytes: mem })
    }

    async fn list(&self, service: Option<&str>) -> Vec<String> {
        let filter = match service {
            Some(s) => format!("label={LABEL}={s}"),
            None => format!("label={LABEL}"),
        };
        docker(&["ps", "-a", "--filter", &filter, "--format", "{{.Names}}"]).await.map(|s| s.lines().map(str::to_string).collect()).unwrap_or_default()
    }

    async fn ensure_network(&self, name: &str, service: &str) -> anyhow::Result<()> {
        if docker(&["network", "inspect", "-f", "{{.Name}}", name]).await.is_ok() {
            return Ok(());
        }
        match docker(&["network", "create", "--label", &format!("{LABEL}={service}"), name]).await {
            Ok(_) => Ok(()),
            // Another wake made it meanwhile.
            Err(e) if e.to_string().contains("already exists") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn remove_volume(&self, name: &str) {
        let _ = docker(&["volume", "rm", "-f", name]).await;
    }

    fn reach(&self, url: &str) -> String {
        for_container(url)
    }
}

async fn write_private(path: &std::path::Path, data: &[u8]) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut o = tokio::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    o.mode(0o600);
    let mut f = o.open(path).await?;
    f.write_all(data).await?;
    f.flush().await?;
    Ok(())
}

/// Builds with BuildKit on this host (`docker build`).
pub struct DockerBuilder;

#[async_trait]
impl Builder for DockerBuilder {
    fn name(&self) -> &'static str {
        "docker"
    }

    async fn build(&self, job: &BuildJob, log: &LogSink, cancel: tokio_util::sync::CancellationToken) -> anyhow::Result<String> {
        let dockerfile = job.context.join(&job.dockerfile);
        let mut child = Command::new("docker")
            .env("DOCKER_BUILDKIT", "1")
            .args(["build", "--progress", "plain", "--file"])
            .arg(&dockerfile)
            .args(["--tag", &job.tag, "--label", &format!("{LABEL}={}", job.service), "--label", &format!("{LABEL}.deployment={}", job.deployment)])
            .arg(&job.context)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("running docker build")?;
        let out = tokio::spawn(pump(child.stdout.take().expect("piped"), log.clone()));
        let err = tokio::spawn(pump(child.stderr.take().expect("piped"), log.clone()));
        let status = tokio::select! {
            s = child.wait() => s?,
            _ = tokio::time::sleep(job.timeout) => {
                let _ = child.kill().await;
                bail!("the build took longer than {} minutes", job.timeout.as_secs() / 60);
            }
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                bail!("canceled");
            }
        };
        let _ = (out.await, err.await);
        if !status.success() {
            bail!("the build failed (docker build exited with {})", status.code().unwrap_or(-1));
        }
        // The tag is the deployment's own (`roll-apps-<service>:<deployment>`), never moved.
        docker(&["image", "inspect", "-f", "{{.Id}}", &job.tag]).await?;
        Ok(job.tag.clone())
    }

    async fn remove_image(&self, image: &str) {
        if image.starts_with(super::PREFIX) {
            let _ = docker(&["image", "rm", image]).await;
        }
    }
}
