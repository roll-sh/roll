//! Instances as containers on this host, through the `docker` CLI (one node and development).
//!
//! - A container is created at deploy time with its port published on a loopback port chosen
//!   then, so a wake is `docker start` and nothing else (no create, no `docker port`).
//! - Variables go in as values of the Engine API's create call (`Env`): never argv, never a file,
//!   and multi-line values work. Over the CLI (a daemon that isn't a local socket) they go in
//!   through an `--env-file` written 0600 and removed right after the create.
//! - Capabilities no app needs are dropped ([`CAP_DROP`]); `no-new-privileges`, a pids limit and
//!   memory without swap bound the rest.
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
    /// Container name → (full id, published host port, CPU limit in nano-CPUs).
    containers: HashMap<String, (String, Option<u16>, Option<i64>)>,
    /// Instance → its sandbox's network, while the sandbox runs.
    sandboxes: HashMap<String, Sandbox>,
    /// Instance → its waker: listening, with the connections its network had accepted when it
    /// was armed (`Some`), or not armed (`None`). Absent: not known (an earlier process's).
    wakers: HashMap<String, Option<u64>>,
    /// Instance → the connections its network had accepted when it went to sleep, or when it
    /// was last asked about callers ([`super::Runtime::take_callers`]).
    marks: HashMap<String, u64>,
}

/// Where to read who's at an instance's port: its sandbox's process (`/proc/<pid>/net`), the
/// app's port, and the host's addresses on its network.
#[derive(Clone)]
struct Sandbox {
    /// Its container's id: what its pid's cgroup names while it's still the sandbox.
    id: String,
    pid: u32,
    port: u16,
    host: Vec<std::net::IpAddr>,
}

impl Sandbox {
    /// Whether its pid is still the sandbox's (after a Docker restart it may be another
    /// process's, and its numbers another network's): its cgroup names the container.
    fn alive(&self) -> bool {
        std::fs::read_to_string(format!("/proc/{}/cgroup", self.pid)).is_ok_and(|c| c.contains(&self.id))
    }

    /// Its `tcp` and `tcp6` tables (empty when the sandbox is gone).
    fn tables(&self) -> (String, String) {
        let read = |f: &str| std::fs::read_to_string(format!("/proc/{}/net/{f}", self.pid)).unwrap_or_default();
        (read("tcp"), read("tcp6"))
    }

    /// Whether something listens on the app's port (the app, or the waker): a dear read.
    fn listening(&self) -> bool {
        let (tcp, tcp6) = self.tables();
        super::netns::listening(&tcp, &tcp6, self.port)
    }

    /// The connections its network has accepted: a cheap read.
    fn accepted(&self) -> Option<u64> {
        super::netns::passive_opens(&std::fs::read_to_string(format!("/proc/{}/net/snmp", self.pid)).ok()?)
    }
}

pub struct Docker {
    known: Mutex<Known>,
    /// Where env files are written for the moment of a create.
    scratch: PathBuf,
    /// The daemon's socket, when it's on this host.
    engine: Option<Engine>,
    /// A web instance's network lives in a sandbox container made with it ([`sandbox_name`]):
    /// a wake starts only the app, which joins the sandbox's network namespace, so Docker sets
    /// no network up on the wake's path. `ROLL_APPS_NETNS=0` turns it off.
    netns: bool,
}

/// The sandbox holding `instance`'s network, its aliases and its published port.
pub fn sandbox_name(instance: &str) -> String {
    format!("{instance}-net")
}

/// The waker in `instance`'s sandbox: it listens on the app's port while the app is stopped,
/// takes the first private caller, and hands that connection to the app once it listens.
pub fn waker_name(instance: &str) -> String {
    format!("{instance}-wake")
}

impl Docker {
    pub fn new(scratch: PathBuf) -> Docker {
        // Env files a stopped process left behind (they hold variables' values).
        let _ = std::fs::remove_dir_all(&scratch);
        let netns = std::env::var("ROLL_APPS_NETNS").map_or(true, |v| v != "0");
        Docker { known: Mutex::new(Known::default()), scratch, engine: Engine::local(), netns }
    }

    /// With or without network sandboxes, whatever `ROLL_APPS_NETNS` says (a measurement
    /// compares the two).
    pub fn with_netns(mut self, on: bool) -> Docker {
        self.netns = on;
        self
    }

    async fn id_and_port(&self, name: &str) -> anyhow::Result<(String, Option<u16>)> {
        self.known_of(name).await.map(|(id, port, _)| (id, port))
    }

    async fn known_of(&self, name: &str) -> anyhow::Result<(String, Option<u16>, Option<i64>)> {
        if let Some(k) = self.known.lock().containers.get(name).cloned() {
            return Ok(k);
        }
        // Made by an earlier process of this node: ask docker.
        if let Some(e) = &self.engine {
            let i = e.inspect(name).await?;
            self.known.lock().containers.insert(name.to_string(), (i.id.clone(), i.host_port, i.cpu_nanos));
            return Ok((i.id, i.host_port, i.cpu_nanos));
        }
        let id = docker(&["inspect", "-f", "{{.Id}}", name]).await?;
        let ports = docker(&["inspect", "-f", "{{range $p, $b := .HostConfig.PortBindings}}{{range $b}}{{.HostPort}} {{end}}{{end}}", name]).await?;
        let port = ports.split_whitespace().next().and_then(|p| p.parse().ok());
        let cpu = docker(&["inspect", "-f", "{{index .Config.Labels \"sh.roll.apps.cpu\"}}", name]).await.ok().and_then(|c| c.parse().ok());
        self.known.lock().containers.insert(name.to_string(), (id.clone(), port, cpu));
        Ok((id, port, cpu))
    }

    /// `instance`'s sandbox, if it has one that runs (looked up once, then remembered).
    async fn sandbox(&self, instance: &str) -> Option<Sandbox> {
        let cached = self.known.lock().sandboxes.get(instance).cloned();
        if let Some(s) = cached {
            if s.alive() {
                return Some(s);
            }
            self.known.lock().sandboxes.remove(instance);
        }
        let i = self.engine.as_ref()?.inspect(&sandbox_name(instance)).await.ok()?;
        let s = Sandbox { id: i.id, pid: i.pid?, port: i.exposed?, host: i.gateways };
        self.known.lock().sandboxes.insert(instance.to_string(), s.clone());
        Some(s)
    }

    /// Before the app starts, the waker lets go of its port (unless it holds a caller: then it
    /// has already, and hands that caller to the app). rolld calls it once through the published
    /// port: accepting closes its listener in a millisecond, where killing its container takes
    /// Docker some 70. The empty call it then hands the app is closed at once. A waker that
    /// doesn't let go is killed. What it knows comes from the cheap count of accepted
    /// connections; only a waker an earlier process armed costs a read of the socket table.
    async fn quiet_waker(&self, engine: &Engine, instance: &str) {
        let Some(sb) = self.sandbox(instance).await else { return };
        let known = self.known.lock().wakers.get(instance).copied();
        let armed_at = match known {
            Some(None) => return,
            Some(Some(at)) => at,
            None if !sb.listening() => {
                self.known.lock().wakers.insert(instance.to_string(), None);
                return;
            }
            None => sb.accepted().unwrap_or(0),
        };
        self.known.lock().wakers.insert(instance.to_string(), None);
        let Some(now) = sb.accepted() else { return };
        // It accepted a caller since it was armed: its listener is closed already.
        if now > armed_at {
            return;
        }
        let free = |within: Duration| {
            let sb = sb.clone();
            async move {
                let until = std::time::Instant::now() + within;
                while sb.listening() {
                    if std::time::Instant::now() >= until {
                        return false;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                true
            }
        };
        let poked = match self.id_and_port(instance).await {
            Ok((_, Some(hp))) => tokio::time::timeout(Duration::from_millis(200), tokio::net::TcpStream::connect(("127.0.0.1", hp))).await.is_ok_and(|c| c.is_ok()),
            _ => false,
        };
        // Taken: the count moves when it accepts, and accepting closed its listener.
        if poked {
            let until = std::time::Instant::now() + Duration::from_millis(200);
            while std::time::Instant::now() < until {
                if sb.accepted().is_some_and(|a| a > now) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        let _ = engine.kill(&waker_name(instance)).await;
        // Its socket goes with its process: wait for that, so the app's bind never fails.
        free(Duration::from_secs(2)).await;
    }

    /// Asleep: the waker listens for private callers, armed with the count of accepted
    /// connections now (one more is its caller). Waited for (off any request's path, a socket
    /// table read every 25 ms, some 150 ms in all), so a request right after the sleep finds it
    /// listening and frees the port with a poke, not a kill.
    async fn arm_waker(&self, engine: &Engine, instance: &str) {
        let Some(sb) = self.sandbox(instance).await else { return };
        let w = waker_name(instance);
        let _ = engine.kill(&w).await;
        let at = sb.accepted().unwrap_or(0);
        self.known.lock().marks.insert(instance.to_string(), at);
        if let Err(e) = engine.start(&w).await {
            tracing::debug!(instance, error = %e, "apps: the waker didn't start");
            self.known.lock().wakers.insert(instance.to_string(), None);
            return;
        }
        self.known.lock().wakers.insert(instance.to_string(), Some(at));
        let until = std::time::Instant::now() + Duration::from_secs(2);
        while !sb.listening() {
            if std::time::Instant::now() >= until {
                // Not bound yet on a busy host: the next start looks before it trusts it.
                self.known.lock().wakers.remove(instance);
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
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

    async fn pull(&self, image: &str, log: &LogSink, login: Option<&super::RegistryLogin>) -> anyhow::Result<String> {
        log.line(format!("pulling {image}"));
        // A private registry's login goes to dockerd with this pull only (never `docker login`,
        // which would keep it in this host's docker config for everyone).
        if let Some(login) = login {
            let Some(engine) = &self.engine else { bail!("a private registry needs Docker on a local socket") };
            log.line(format!("logging in to {} as {}", login.server, login.username));
            engine.pull(image, login, log).await?;
            let digest = docker(&["image", "inspect", "-f", "{{if .RepoDigests}}{{index .RepoDigests 0}}{{else}}{{.Id}}{{end}}", image]).await?;
            return Ok(if digest.is_empty() { image.to_string() } else { digest });
        }
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
        let host_port = match spec.port {
            Some(_) => Some(free_port()?),
            None => None,
        };
        if let Some(engine) = &self.engine {
            // A web instance on a network: its network first, in a sandbox that runs from now on.
            if self.netns
                && let (Some(_), Some(_)) = (spec.port, &spec.network)
            {
                let sandbox = sandbox_name(&spec.name);
                let made = async {
                    engine.create(&sandbox, &sandbox_config(spec, host_port)).await?;
                    engine.start(&sandbox).await
                }
                .await;
                match made {
                    Ok(()) => match engine.create(&spec.name, &joined(engine_config(spec, None)?, &sandbox)).await {
                        Ok(id) => {
                            self.known.lock().containers.insert(spec.name.clone(), (id, host_port, Some(nanos(spec.cpu))));
                            // Without a waker a stopped instance only wakes through rolld.
                            if let Err(e) = engine.create(&waker_name(&spec.name), &waker_config(spec, &sandbox)).await {
                                tracing::debug!(instance = %spec.name, error = %e, "apps: no waker; private callers can't wake it");
                            }
                            return Ok(());
                        }
                        Err(e) => {
                            let _ = docker(&["rm", "-f", "-v", &sandbox]).await;
                            if !e.to_string().contains("No such image") {
                                return Err(e);
                            }
                        }
                    },
                    // No sandbox (its image isn't here yet, say): the instance holds its own
                    // network, as before.
                    Err(e) => {
                        tracing::debug!(sandbox, error = %e, "apps: no network sandbox; the instance holds its own");
                        let _ = docker(&["rm", "-f", "-v", &sandbox]).await;
                    }
                }
            }
            match engine.create(&spec.name, &engine_config(spec, host_port)?).await {
                Ok(id) => {
                    self.known.lock().containers.insert(spec.name.clone(), (id, host_port, Some(nanos(spec.cpu))));
                    return Ok(());
                }
                // An image this host no longer has: the CLI pulls it on create.
                Err(e) if e.to_string().contains("No such image") => {}
                Err(e) => return Err(e),
            }
        }
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
            // Started with the boost; `settle` sets the limit once it answers.
            "--cpus".into(),
            format!("{}", boost(spec.cpu)),
            "--label".into(),
            format!("{LABEL}.cpu={}", nanos(spec.cpu)),
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
        for c in CAP_DROP {
            args.extend(["--cap-drop".into(), c.into()]);
        }
        if let (Some(p), Some(hp)) = (spec.port, host_port) {
            args.extend(["--publish".into(), format!("127.0.0.1:{hp}:{p}")]);
        }
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
        self.known.lock().containers.insert(spec.name.clone(), (id, host_port, Some(nanos(spec.cpu))));
        Ok(())
    }

    async fn start(&self, name: &str, log: LogSink) -> anyhow::Result<Option<SocketAddr>> {
        let now = chrono::Utc::now();
        let (o, e) = (log.stream(Stream::Stdout), log.stream(Stream::Stderr));
        if let Some(engine) = &self.engine {
            self.quiet_waker(engine, name).await;
            // Running: its own connections aren't callers; the next sleep marks it again.
            self.known.lock().marks.remove(name);
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

    async fn settle(&self, name: &str) {
        let Ok((_, _, Some(cpu))) = self.known_of(name).await else { return };
        let r = match &self.engine {
            Some(e) => e.set_cpus(name, cpu).await,
            None => docker(&["update", "--cpus", &format!("{}", cpu as f64 / 1e9), name]).await.map(|_| ()),
        };
        if let Err(e) = r {
            tracing::warn!(name, error = %e, "apps: an instance's CPU limit couldn't be set after its start");
        }
    }

    async fn suspend(&self, name: &str) -> anyhow::Result<()> {
        // Marked before the pause: a connection in between is answered, not missed.
        if let Some(n) = self.sandbox(name).await.and_then(|sb| sb.accepted()) {
            self.known.lock().marks.insert(name.to_string(), n);
        }
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
        let stopped = match &self.engine {
            Some(e) => e.stop(name, secs).await,
            None => docker_within(&["stop", "-t", &secs.to_string(), name], Some(Duration::from_secs(secs + 60))).await.map(|_| ()),
        };
        if let (Ok(()), Some(engine)) = (&stopped, &self.engine) {
            self.arm_waker(engine, name).await;
        }
        // The next start gets the boost again (set now, off the wake's path).
        if let (Ok(()), Ok((_, _, Some(cpu)))) = (&stopped, self.known_of(name).await) {
            let boosted = nanos(boost(cpu as f64 / 1e9));
            let _ = match &self.engine {
                Some(e) => e.set_cpus(name, boosted).await,
                None => docker(&["update", "--cpus", &format!("{}", boosted as f64 / 1e9), name]).await.map(|_| ()),
            };
        }
        stopped
    }

    async fn remove(&self, name: &str) {
        let _ = docker(&["rm", "-f", "-v", name, &waker_name(name), &sandbox_name(name)]).await;
        let mut k = self.known.lock();
        k.containers.remove(name);
        k.sandboxes.remove(name);
        k.wakers.remove(name);
        k.marks.remove(name);
    }

    async fn network_container(&self, name: &str) -> Option<String> {
        if self.sandbox(name).await.is_some() {
            return Some(sandbox_name(name));
        }
        self.running(name).await.then(|| name.to_string())
    }

    async fn take_callers(&self, name: &str) -> bool {
        let Some(sb) = self.sandbox(name).await else { return false };
        let Some(now) = sb.accepted() else {
            // Its sandbox went (or another process took the pid): look it up again next time.
            self.known.lock().sandboxes.remove(name);
            return false;
        };
        let mut k = self.known.lock();
        // Not marked (an earlier process put it to sleep): from now on.
        let mark = k.marks.entry(name.to_string()).or_insert(now);
        std::mem::replace(mark, now) < now
    }

    async fn private_peers(&self, name: &str) -> Option<u32> {
        let sb = self.sandbox(name).await?;
        let (tcp, tcp6) = sb.tables();
        (!tcp.is_empty() || !tcp6.is_empty()).then(|| super::netns::private_peers(&tcp, &tcp6, sb.port, &sb.host))
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

    async fn exited(&self, name: &str) -> Option<super::Exit> {
        let i = self.engine.as_ref()?.inspect(name).await.ok()?;
        (!i.running).then_some(super::Exit { code: i.exit_code, oom: i.oom_killed })
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

    async fn read_volume(&self, name: &str) -> anyhow::Result<super::VolumeStream> {
        anyhow::ensure!(name.starts_with(super::PREFIX), "{name} isn't a roll apps volume");
        let mount = format!("{name}:/data:ro");
        let mut child = Command::new("docker")
            .args(["run", "--rm", "--network", "none", "--label", &format!("{LABEL}=backup"), "-v", &mount, VOLUME_IMAGE, "tar", "c", "-C", "/data", "."])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("running docker for a volume's backup")?;
        let out = child.stdout.take().context("no stdout")?;
        Ok(Box::new(ChildReader { child: Some(child), out, exited: None }))
    }

    async fn export_files(&self, image: &str, dir: &str) -> anyhow::Result<super::VolumeStream> {
        anyhow::ensure!(dir.starts_with('/') && !dir.contains(".."), "{dir} isn't an absolute path");
        // The image's own tar (a static plan's is busybox), no network, its entrypoint replaced.
        let mut child = Command::new("docker")
            .args(["run", "--rm", "--network", "none", "--label", &format!("{LABEL}=export"), "--entrypoint", "tar", image, "c", "-C", dir, "."])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("running docker to export files")?;
        let out = child.stdout.take().context("no stdout")?;
        Ok(Box::new(ChildReader { child: Some(child), out, exited: None }))
    }

    async fn write_volume(&self, name: &str, mut tar: super::VolumeStream) -> anyhow::Result<()> {
        anyhow::ensure!(name.starts_with(super::PREFIX), "{name} isn't a roll apps volume");
        let mount = format!("{name}:/data");
        let mut child = Command::new("docker")
            .args(["run", "--rm", "-i", "--network", "none", "--label", &format!("{LABEL}=restore"), "-v", &mount, VOLUME_IMAGE, "sh", "-c", RESTORE])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("running docker for a volume's restore")?;
        let mut stdin = child.stdin.take().context("no stdin")?;
        tokio::io::copy(&mut tar, &mut stdin).await.context("streaming the backup in")?;
        drop(stdin);
        let out = child.wait_with_output().await?;
        anyhow::ensure!(out.status.success(), "the restore failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        Ok(())
    }

    fn reach(&self, url: &str) -> String {
        for_container(url)
    }

    async fn shell(&self, name: &str, cols: u16, rows: u16) -> anyhow::Result<super::Shell> {
        let Some(engine) = &self.engine else { bail!("a shell needs Docker on a local socket") };
        // bash when the image has it, else sh; a login shell, so the image's profile applies.
        let cmd = ["sh", "-c", "if command -v bash >/dev/null 2>&1; then exec bash -l; else exec sh -l; fi"];
        let (id, io) = engine.exec_tty(name, &cmd, &["TERM=xterm-256color"], cols, rows).await?;
        Ok(super::Shell { id, io: Box::new(hyper_util::rt::TokioIo::new(io)) })
    }

    async fn resize_shell(&self, shell: &str, cols: u16, rows: u16) -> anyhow::Result<()> {
        match &self.engine {
            Some(e) => e.resize(shell, cols, rows).await,
            None => Ok(()),
        }
    }
}

/// A start's CPU: 4× the limit, at least 1 and at most 4 cores (Cloud Run's startup CPU boost):
/// a boot throttled to a quarter of a core is measurably slower, and the limit applies once the
/// instance answers (`settle`). What it uses is metered either way.
fn boost(cpu: f64) -> f64 {
    (cpu * 4.0).clamp(1.0, 4.0).max(cpu)
}

fn nanos(cpu: f64) -> i64 {
    (cpu * 1e9).round() as i64
}

/// Capabilities Docker grants by default that no app here needs: raw sockets (spoofing on the
/// project network), device nodes, the audit log and file capabilities. What images commonly use
/// (binding low ports, chown and setuid in entrypoints) stays.
pub const CAP_DROP: [&str; 4] = ["NET_RAW", "MKNOD", "AUDIT_WRITE", "SETFCAP"];

/// The Engine API's create config for an instance: the same container the CLI path makes.
/// Half a CPU for the sandbox and the waker: they use none once started, but their start runs
/// in their cgroup, and at a twentieth of a CPU it took 800 ms instead of 170.
///
/// The sandbox's container: busybox asleep, holding the instance's network, its aliases on the
/// project network and its published port; nothing else, and allowed nothing else.
fn sandbox_config(spec: &InstanceSpec, host_port: Option<u16>) -> serde_json::Value {
    use serde_json::json;
    let mut host = json!({
        "Memory": 16i64 << 20,
        "MemorySwap": 16i64 << 20,
        "NanoCpus": 500_000_000i64,
        "PidsLimit": 8,
        "SecurityOpt": ["no-new-privileges"],
        "CapDrop": ["ALL"],
        "ReadonlyRootfs": true,
        "RestartPolicy": {"Name": "no"},
        "Init": true,
        "ExtraHosts": ["host.docker.internal:host-gateway"],
        "LogConfig": {"Type": "none", "Config": {}},
    });
    let mut config = json!({
        "Image": VOLUME_IMAGE,
        "Cmd": ["sleep", "2147483647"],
        "Labels": { LABEL: spec.service, format!("{LABEL}.env"): spec.env, format!("{LABEL}.role"): "network" },
    });
    if let (Some(p), Some(hp)) = (spec.port, host_port) {
        let port = format!("{p}/tcp");
        config["ExposedPorts"] = json!({ &port: {} });
        host["PortBindings"] = json!({ &port: [{"HostIp": "127.0.0.1", "HostPort": hp.to_string()}] });
    }
    if let Some(net) = &spec.network {
        host["NetworkMode"] = json!(net);
        config["NetworkingConfig"] = json!({"EndpointsConfig": { net: {"Aliases": spec.aliases} }});
    }
    config["HostConfig"] = host;
    config
}

/// The waker's container, in the sandbox's network: busybox's `nc` listens once on the app's
/// port (accepting closes its listener, so the app can bind while the caller is held), waits up
/// to a minute for the app to listen, then joins the caller to it. Nothing else: no host, no
/// capabilities, a read-only root, 16 MiB, half a CPU.
fn waker_config(spec: &InstanceSpec, sandbox: &str) -> serde_json::Value {
    use serde_json::json;
    let p = spec.port.unwrap_or(80);
    let relay = format!("i=0; until nc -z 127.0.0.1 {p}; do i=$((i+1)); [ $i -gt 3000 ] && exit 1; sleep 0.02; done; exec nc 127.0.0.1 {p}");
    json!({
        "Image": VOLUME_IMAGE,
        "Cmd": ["nc", "-l", "-p", p.to_string(), "-e", "sh", "-c", relay],
        "Labels": { LABEL: spec.service, format!("{LABEL}.env"): spec.env, format!("{LABEL}.role"): "wake" },
        "HostConfig": {
            "NetworkMode": format!("container:{sandbox}"),
            "Memory": 16i64 << 20,
            "MemorySwap": 16i64 << 20,
            "NanoCpus": 500_000_000i64,
            "PidsLimit": 8,
            "SecurityOpt": ["no-new-privileges"],
            "CapDrop": ["ALL"],
            "ReadonlyRootfs": true,
            "RestartPolicy": {"Name": "no"},
            "LogConfig": {"Type": "none", "Config": {}},
        },
    })
}

/// An instance's config joined to its sandbox's network: no network, port or hosts of its own
/// (Docker refuses those with another container's network).
fn joined(mut config: serde_json::Value, sandbox: &str) -> serde_json::Value {
    if let Some(o) = config.as_object_mut() {
        o.remove("ExposedPorts");
        o.remove("NetworkingConfig");
    }
    if let Some(h) = config["HostConfig"].as_object_mut() {
        h.remove("PortBindings");
        h.remove("ExtraHosts");
        h.insert("NetworkMode".into(), serde_json::json!(format!("container:{sandbox}")));
    }
    config
}

fn engine_config(spec: &InstanceSpec, host_port: Option<u16>) -> anyhow::Result<serde_json::Value> {
    use serde_json::json;
    let mut env = Vec::with_capacity(spec.vars.len());
    for (k, v) in &spec.vars {
        if v.contains('\0') {
            bail!("variable {k} holds a NUL byte");
        }
        env.push(format!("{k}={v}"));
    }
    let mut host = json!({
        "Memory": (spec.memory_mb as i64) << 20,
        "MemorySwap": (spec.memory_mb as i64) << 20,
        "NanoCpus": nanos(boost(spec.cpu)),
        "PidsLimit": 4096,
        "SecurityOpt": ["no-new-privileges"],
        "CapDrop": CAP_DROP,
        "RestartPolicy": {"Name": "no"},
        "Init": true,
        "ExtraHosts": ["host.docker.internal:host-gateway"],
        "LogConfig": {"Type": "local", "Config": {}},
    });
    let mut config = json!({
        "Image": spec.image,
        "Env": env,
        "Labels": {
            LABEL: spec.service,
            format!("{LABEL}.env"): spec.env,
            format!("{LABEL}.deployment"): spec.deployment,
            format!("{LABEL}.cpu"): nanos(spec.cpu).to_string(),
        },
    });
    if let (Some(p), Some(hp)) = (spec.port, host_port) {
        let port = format!("{p}/tcp");
        config["ExposedPorts"] = json!({ &port: {} });
        host["PortBindings"] = json!({ &port: [{"HostIp": "127.0.0.1", "HostPort": hp.to_string()}] });
    }
    if let Some(net) = &spec.network {
        host["NetworkMode"] = json!(net);
        config["NetworkingConfig"] = json!({"EndpointsConfig": { net: {"Aliases": spec.aliases} }});
    }
    if let Some((vol, mount)) = &spec.volume {
        host["Binds"] = json!([format!("{vol}:{mount}")]);
    }
    if let Some(cmd) = &spec.command {
        config["Entrypoint"] = json!(["sh"]);
        config["Cmd"] = json!(["-c", cmd]);
    }
    config["HostConfig"] = host;
    Ok(config)
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

/// What the `docker` CLI of a build keeps of rolld's environment: how to reach the daemon and
/// where its configuration is, nothing a build's variables could set.
const CLI_ENV: [&str; 8] = ["PATH", "HOME", "DOCKER_HOST", "DOCKER_CONFIG", "DOCKER_CONTEXT", "DOCKER_CERT_PATH", "DOCKER_TLS_VERIFY", "XDG_RUNTIME_DIR"];

/// The small image that reads or writes a volume for a backup or a restore.
const VOLUME_IMAGE: &str = "busybox:1.37";
/// A restore: the backup (the tar on stdin) extracts into a staging directory on the volume, and
/// only once it extracted whole do the old files go and the new ones move in; a cut-short or
/// broken tar leaves the volume as it was (exit 3).
const RESTORE: &str = r#"set -e; S=/data/.roll-restore; rm -rf "$S"; mkdir "$S"; if ! tar x -C "$S"; then rm -rf "$S"; exit 3; fi; find /data -mindepth 1 -maxdepth 1 ! -name .roll-restore -exec rm -rf {} +; find "$S" -mindepth 1 -maxdepth 1 -exec mv {} /data/ \;; rmdir "$S""#;

type Exited = std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<std::process::ExitStatus>> + Send>>;

/// A child's stdout, read while the child (killed when this drops) runs. Its end waits for the
/// child: one that failed (a daemon error, an image it couldn't pull) is a read error, so a
/// backup that stopped short is never stored as a good one.
struct ChildReader {
    child: Option<tokio::process::Child>,
    out: tokio::process::ChildStdout,
    exited: Option<Exited>,
}

impl tokio::io::AsyncRead for ChildReader {
    fn poll_read(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<std::io::Result<()>> {
        use std::task::Poll;
        if self.exited.is_none() {
            let before = buf.filled().len();
            match std::pin::Pin::new(&mut self.out).poll_read(cx, buf) {
                Poll::Ready(Ok(())) if buf.filled().len() == before => {
                    let mut child = self.child.take().expect("the child, until its end");
                    self.exited = Some(Box::pin(async move { child.wait().await }));
                }
                other => return other,
            }
        }
        match self.exited.as_mut().expect("set above").as_mut().poll(cx) {
            Poll::Ready(Ok(s)) if s.success() => Poll::Ready(Ok(())),
            Poll::Ready(Ok(s)) => Poll::Ready(Err(std::io::Error::other(format!("reading the volume failed ({s})")))),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Builds with BuildKit on this host (`docker build`), at most `slots` at once: one tenant's
/// burst of pushes queues instead of taking every CPU and all memory of the node.
pub struct DockerBuilder {
    slots: tokio::sync::Semaphore,
}

impl DockerBuilder {
    pub fn new(at_once: usize) -> DockerBuilder {
        DockerBuilder { slots: tokio::sync::Semaphore::new(at_once) }
    }
}

#[async_trait]
impl Builder for DockerBuilder {
    fn name(&self) -> &'static str {
        "docker"
    }

    async fn build(&self, job: &BuildJob, log: &LogSink, cancel: tokio_util::sync::CancellationToken) -> anyhow::Result<String> {
        let _slot = match self.slots.try_acquire() {
            Ok(s) => s,
            Err(_) => {
                log.line("waiting for another build on this node to finish");
                tokio::select! {
                    s = self.slots.acquire() => s?,
                    _ = cancel.cancelled() => bail!("canceled"),
                }
            }
        };
        let dockerfile = job.context.join(&job.dockerfile);
        // Values never reach argv, and never the environment of the CLI but as the build
        // arguments the tree's own Dockerfile declares: each is a BuildKit secret read from a
        // file only rolld can read (a 0700 directory, removed when the build ends).
        let secrets = tempfile::Builder::new().prefix("roll-apps-build-").tempdir().context("a directory for the build's secrets")?;
        let args = super::build_args(job);
        let mut cmd = Command::new("docker");
        cmd.env_clear();
        for k in CLI_ENV {
            if let Some(v) = std::env::var_os(k) {
                cmd.env(k, v);
            }
        }
        cmd.env("DOCKER_BUILDKIT", "1")
            .args(["build", "--progress", "plain", "--file"])
            .arg(&dockerfile)
            .args(["--tag", &job.tag, "--label", &format!("{LABEL}={}", job.service), "--label", &format!("{LABEL}.deployment={}", job.deployment)])
            // Links (a database) are reached the way instances reach them.
            .args(["--add-host", "host.docker.internal:host-gateway"]);
        for (i, (k, v)) in job.env.iter().enumerate() {
            let file = secrets.path().join(i.to_string());
            write_private(&file, v.as_bytes()).await?;
            cmd.arg("--secret").arg(format!("id={k},src={}", file.display()));
        }
        // `--build-arg K` (no `=`) reads the value from the CLI's environment.
        for k in &args {
            let v = job.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str()).unwrap_or_default();
            cmd.env(k, v).arg("--build-arg").arg(k);
        }
        if !job.env.is_empty() && !job.own_dockerfile {
            cmd.arg("--build-arg").arg(format!("ROLL_BUILD_ENV={}", job.env_hash));
        }
        if let Some(v) = &job.layers {
            cmd.env(apps_build::LAYERS_ARG, v).arg("--build-arg").arg(apps_build::LAYERS_ARG);
        }
        if job.no_cache {
            cmd.args(["--no-cache", "--pull"]);
        }
        let mut child = cmd
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

    fn shares_caches(&self) -> bool {
        true
    }

    async fn remove_image(&self, image: &str) {
        if image.starts_with(super::PREFIX) {
            let _ = docker(&["image", "rm", image]).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_instance_config_carries_values_verbatim_and_publishes_only_on_loopback() {
        let pem = "-----BEGIN KEY-----\nabc\n-----END KEY-----";
        let spec = InstanceSpec {
            name: "roll-apps-x".into(),
            service: "svc_x".into(),
            env: "production".into(),
            deployment: "dep_x".into(),
            image: "img".into(),
            command: None,
            vars: vec![("TLS_KEY".into(), pem.into())],
            port: Some(8080),
            cpu: 0.5,
            memory_mb: 512,
            network: Some("roll-apps-net-p-production".into()),
            aliases: vec!["web".into()],
            volume: None,
            idle_stop_s: None,
            registry: None,
        };
        let c = engine_config(&spec, Some(41234)).unwrap();
        assert_eq!(c["Env"][0], format!("TLS_KEY={pem}"));
        assert_eq!(c["HostConfig"]["PortBindings"]["8080/tcp"][0]["HostIp"], "127.0.0.1");
        assert_eq!(c["HostConfig"]["CapDrop"][0], "NET_RAW");
        assert_eq!(c["NetworkingConfig"]["EndpointsConfig"]["roll-apps-net-p-production"]["Aliases"][0], "web");
        assert_eq!(c["Labels"]["sh.roll.apps"], "svc_x");
        let nul = InstanceSpec { vars: vec![("A".into(), "a\0b".into())], ..spec };
        assert!(engine_config(&nul, None).is_err());
    }
}
