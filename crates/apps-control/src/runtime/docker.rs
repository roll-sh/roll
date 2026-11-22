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
    /// Instance → its sidecars' containers ([`InstanceSpec::sidecars`]), by name.
    sidecars: HashMap<String, Vec<String>>,
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
    /// Links to this host's loopback, served to containers through relays ([`super::links`]):
    /// with the daemon on this host, except on macOS (Docker Desktop's `host.docker.internal`
    /// reaches the host's loopback itself) or with `ROLL_APPS_LINK_RELAY=0`.
    links: Option<super::links::Links>,
    /// Egress proxies for closed environments ([`super::egress`]), served to them through relays
    /// as links are (so only where links are).
    egress: Option<super::egress::Proxies>,
    /// How rolld reaches closed web instances ([`super::ingress`]): their networks have no
    /// address of the host's, so their sandboxes answer on a unix socket instead.
    ingress: Option<super::ingress::Ingress>,
}

/// The sandbox holding `instance`'s network, its aliases and its published port.
pub fn sandbox_name(instance: &str) -> String {
    format!("{instance}-net")
}

/// A container's last log lines, for an error: what looks like a key (44 characters of base64)
/// is left out.
async fn docker_logs_tail(name: &str) -> String {
    let out = Command::new("docker").args(["logs", "--tail", "5", name]).stdin(Stdio::null()).output().await.map(|o| format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))).unwrap_or_default();
    let words: Vec<String> = out.split_whitespace().map(|w| if w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '+' && c != '/' && c != '=').len() >= 43 { "[redacted]".to_string() } else { w.to_string() }).collect();
    let line = words.join(" ");
    line.chars().take(300).collect()
}

/// The waker in `instance`'s sandbox: it listens on the app's port while the app is stopped,
/// takes the first private caller, and hands that connection to the app once it listens.
pub fn waker_name(instance: &str) -> String {
    format!("{instance}-wake")
}

impl Docker {
    /// [`super::Runtime::create`]'s container (and its sandbox and waker).
    async fn create_container(&self, spec: &InstanceSpec) -> anyhow::Result<()> {
        if let (Some(links), Some(net)) = (&self.links, &spec.network) {
            for port in super::links::ports(&spec.vars) {
                ensure_relay(links, net, port).await?;
            }
        }
        // A closed instance publishes nothing (Docker publishes no port from a network with no
        // route out): it's reached at its address there, and the host at the network's gateway.
        let host_port = match spec.port {
            Some(_) if !spec.closed => Some(free_port()?),
            _ => None,
        };
        // Other ports a domain sends to: published on loopback as the main one is.
        let mut extra = Vec::new();
        if host_port.is_some() {
            for p in spec.ports.iter().filter(|p| Some(**p) != spec.port) {
                extra.push((*p, free_port()?));
            }
        }
        let host_gateway = match (&spec.network, spec.closed) {
            (Some(net), true) => gateway(net).await,
            _ => Some("host-gateway".to_string()),
        };
        if let Some(engine) = &self.engine {
            // A web instance on a network: its network first, in a sandbox that runs from now on.
            if self.netns
                && let (Some(_), Some(_)) = (spec.port, &spec.network)
            {
                let sandbox = sandbox_name(&spec.name);
                // A closed one answers rolld on a socket ([`super::ingress`]).
                let ingress = match (self.ingress_for(spec), &self.ingress) {
                    (true, Some(i)) => Some(i.prepare(&sandbox).context("the closed sandbox's socket directory")?),
                    _ => None,
                };
                let ing = ingress.as_ref().map(|(d, u, g)| (d.as_path(), *u, *g));
                let made = async {
                    let config = sandbox_config(spec, host_port, &extra, host_gateway.as_deref(), ing);
                    match engine.create(&sandbox, &config).await {
                        // The relay's image isn't here yet: pulled once.
                        Err(e) if ing.is_some() && e.to_string().contains("No such image") => {
                            docker_within(&["pull", "-q", super::links::IMAGE], Some(Duration::from_secs(300))).await?;
                            engine.create(&sandbox, &config).await?;
                        }
                        r => {
                            r?;
                        }
                    }
                    engine.start(&sandbox).await
                }
                .await;
                match made {
                    Ok(()) => match engine.create(&spec.name, &joined(engine_config(spec, None, &[], None)?, &sandbox)).await {
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
                    // A closed one has no other way to be reached: no instance without it.
                    Err(e) if ing.is_some() => {
                        let _ = docker(&["rm", "-f", "-v", &sandbox]).await;
                        return Err(e.context("the closed instance's network sandbox"));
                    }
                    // No sandbox (its image isn't here yet, say): the instance holds its own
                    // network, as before.
                    Err(e) => {
                        tracing::debug!(sandbox, error = %e, "apps: no network sandbox; the instance holds its own");
                        let _ = docker(&["rm", "-f", "-v", &sandbox]).await;
                    }
                }
            }
            match engine.create(&spec.name, &engine_config(spec, host_port, &extra, host_gateway.as_deref())?).await {
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
            "--log-driver".into(),
            "local".into(),
        ];
        if let Some(g) = &host_gateway {
            args.extend(["--add-host".into(), format!("host.docker.internal:{g}")]);
        }
        if let (Some(p), true) = (spec.port, spec.closed) {
            args.extend(["--label".into(), format!("{LABEL}.closed={p}")]);
        }
        for c in CAP_DROP {
            args.extend(["--cap-drop".into(), c.into()]);
        }
        if let (Some(p), Some(hp)) = (spec.port, host_port) {
            args.extend(["--publish".into(), format!("127.0.0.1:{hp}:{p}"), "--label".into(), format!("{LABEL}.port={p}")]);
            for (cp, hp) in &extra {
                args.extend(["--publish".into(), format!("127.0.0.1:{hp}:{cp}")]);
            }
            // A UDP service's port over UDP too, on any free loopback port (the relay reads it).
            if spec.udp {
                args.extend(["--publish".into(), format!("127.0.0.1::{p}/udp")]);
            }
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
        if let Some(h) = &spec.health {
            let s = |d: Duration| format!("{}ms", d.as_millis());
            args.extend([
                "--health-cmd".into(),
                h.command.clone(),
                "--health-interval".into(),
                s(h.every),
                "--health-timeout".into(),
                s(h.every.min(Duration::from_secs(30))),
                "--health-retries".into(),
                h.failures.to_string(),
                "--health-start-period".into(),
                s(h.start),
            ]);
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


    pub fn new(scratch: PathBuf) -> Docker {
        // Env files a stopped process left behind (they hold variables' values).
        let _ = std::fs::remove_dir_all(&scratch);
        let netns = std::env::var("ROLL_APPS_NETNS").map_or(true, |v| v != "0");
        let engine = Engine::local();
        let relay = engine.is_some() && !cfg!(target_os = "macos") && std::env::var("ROLL_APPS_LINK_RELAY").map_or(true, |v| v != "0");
        let links = relay.then(|| super::links::Links::new(scratch.with_file_name("links")));
        // Listed hosts on private addresses too: local development and tests only.
        let private = std::env::var("ROLL_APPS_EGRESS_ALLOW_PRIVATE").is_ok_and(|v| v == "1");
        let egress = relay.then(|| super::egress::Proxies::new(scratch.with_file_name("egress"), private));
        let ingress = relay.then(|| super::ingress::Ingress::new(scratch.with_file_name("ingress")));
        Docker { known: Mutex::new(Known::default()), scratch, engine, netns, links, egress, ingress }
    }

    /// With or without network sandboxes, whatever `ROLL_APPS_NETNS` says (a measurement
    /// compares the two).
    pub fn with_netns(mut self, on: bool) -> Docker {
        self.netns = on;
        self
    }

    /// Where a started instance answers: its published port on loopback, or a closed one's
    /// address on its network (looked up at each start: it changes); none for a worker.
    async fn address(&self, name: &str, host_port: Option<u16>) -> anyhow::Result<Option<SocketAddr>> {
        if let Some(p) = host_port {
            return Ok(Some(SocketAddr::from(([127, 0, 0, 1], p))));
        }
        if let Some(a) = self.through_ingress(name).await? {
            return Ok(Some(a));
        }
        if let Some(e) = &self.engine {
            // In a sandbox the instance has no network of its own: the sandbox's address is its.
            if let Some(a) = e.inspect(name).await?.closed {
                return Ok(Some(a));
            }
            return Ok(e.inspect(&sandbox_name(name)).await.ok().and_then(|s| s.closed));
        }
        let out = docker(&["inspect", "-f", "{{index .Config.Labels \"sh.roll.apps.closed\"}} {{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}", name]).await?;
        let mut f = out.split_whitespace();
        Ok(f.next().and_then(|p| p.parse::<u16>().ok()).and_then(|p| Some(SocketAddr::new(f.next()?.parse().ok()?, p))))
    }

    /// A closed instance's loopback address through its sandbox's socket, when it has one.
    async fn through_ingress(&self, name: &str) -> anyhow::Result<Option<SocketAddr>> {
        match &self.ingress {
            Some(i) if i.has(&sandbox_name(name)) => Ok(Some(i.serve(&sandbox_name(name)).await?)),
            _ => Ok(None),
        }
    }

    /// Whether `spec`'s sandbox answers rolld on a socket ([`super::ingress`]): a closed web
    /// instance in a sandbox, on a node that closes egress.
    fn ingress_for(&self, spec: &InstanceSpec) -> bool {
        spec.closed && spec.port.is_some() && self.netns && self.ingress.is_some()
    }

    async fn id_and_port(&self, name: &str) -> anyhow::Result<(String, Option<u16>)> {
        self.known_of(name).await.map(|(id, port, _)| (id, port))
    }

    async fn known_of(&self, name: &str) -> anyhow::Result<(String, Option<u16>, Option<i64>)> {
        if let Some(k) = self.known.lock().containers.get(name).cloned() {
            return Ok(k);
        }
        // Made by an earlier process of this node: ask docker. An instance in a sandbox
        // publishes nothing itself; its port is the sandbox's.
        let (id, mut port, cpu) = self.inspect_known(name).await?;
        let mut sure = true;
        if port.is_none() {
            match self.inspect_known(&sandbox_name(name)).await {
                Ok((_, p, _)) => port = p,
                // No sandbox (a worker, or networks of its own) is an answer; a failed look isn't.
                Err(e) => sure = e.to_string().to_ascii_lowercase().contains("no such"),
            }
        }
        if sure {
            self.known.lock().containers.insert(name.to_string(), (id.clone(), port, cpu));
        }
        Ok((id, port, cpu))
    }

    async fn inspect_known(&self, name: &str) -> anyhow::Result<(String, Option<u16>, Option<i64>)> {
        if let Some(e) = &self.engine {
            let i = e.inspect(name).await?;
            return Ok((i.id, i.host_port, i.cpu_nanos));
        }
        let id = docker(&["inspect", "-f", "{{.Id}}", name]).await?;
        let ports = docker(&["inspect", "-f", "{{range $p, $b := .HostConfig.PortBindings}}{{range $b}}{{.HostPort}} {{end}}{{end}}", name]).await?;
        let port = ports.split_whitespace().next().and_then(|p| p.parse().ok());
        let cpu = docker(&["inspect", "-f", "{{index .Config.Labels \"sh.roll.apps.cpu\"}}", name]).await.ok().and_then(|c| c.parse().ok());
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
        // On a network with no route out its containers have no gateway of their own; the host's
        // connections still come from the network's (its IPAM gateway).
        let mut host = i.gateways;
        if host.is_empty()
            && let Some(net) = &i.network
            && let Some(g) = gateway(net).await.and_then(|g| g.parse().ok())
        {
            host.push(g);
        }
        // Reached through its socket ([`super::ingress`]), rolld's connections come from the
        // sandbox's own `socat`, on loopback.
        if self.ingress.as_ref().is_some_and(|g| g.has(&sandbox_name(instance))) {
            host.extend([std::net::IpAddr::from([127, 0, 0, 1]), std::net::IpAddr::from(std::net::Ipv6Addr::LOCALHOST)]);
        }
        let s = Sandbox { id: i.id, pid: i.pid?, port: i.exposed?, host };
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
        // Its published port, a closed sandbox's socket, or its address on its network.
        let at = match self.id_and_port(instance).await {
            Ok((_, Some(hp))) => Some(SocketAddr::from(([127, 0, 0, 1], hp))),
            _ => match self.through_ingress(instance).await.ok().flatten() {
                Some(a) => Some(a),
                None => engine.inspect(&sandbox_name(instance)).await.ok().and_then(|s| s.closed),
            },
        };
        let poked = match at {
            Some(a) => tokio::time::timeout(Duration::from_millis(200), tokio::net::TcpStream::connect(a)).await.is_ok_and(|c| c.is_ok()),
            None => false,
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

    /// An instance's sidecars' containers, as created or adopted here.
    fn sidecars_of(&self, instance: &str) -> Vec<String> {
        self.known.lock().sidecars.get(instance).cloned().unwrap_or_default()
    }

    /// Its sidecars, in its network namespace: its sandbox's when it has one (always up), else
    /// its own. Made with it, never started yet.
    async fn create_sidecars(&self, spec: &InstanceSpec) -> anyhow::Result<()> {
        let Some(engine) = &self.engine else { return Ok(()) };
        if spec.sidecars.is_empty() {
            return Ok(());
        }
        let holder = match engine.inspect(&sandbox_name(&spec.name)).await {
            Ok(_) => sandbox_name(&spec.name),
            Err(_) => spec.name.clone(),
        };
        let mut made = Vec::new();
        for sc in &spec.sidecars {
            let name = super::sidecar_name(&spec.name, &sc.name);
            let config = sidecar_config(spec, sc, &holder)?;
            let r = match engine.create(&name, &config).await {
                // Pulled when the deployment was made; a host that's lost it since pulls it again.
                Err(e) if e.to_string().contains("No such image") => {
                    docker_within(&["pull", "-q", &sc.image], Some(Duration::from_secs(300))).await?;
                    engine.create(&name, &config).await
                }
                r => r,
            };
            r.with_context(|| format!("sidecar {}", sc.name))?;
            made.push(name);
        }
        self.known.lock().sidecars.insert(spec.name.clone(), made);
        Ok(())
    }

    /// After the app started: each sidecar again, so it joins the namespace as it is now (one
    /// whose holder restarted keeps a dead one), its output in the instance's log under its own name.
    async fn start_sidecars(&self, engine: &Engine, instance: &str, since: chrono::DateTime<chrono::Utc>, log: &LogSink) -> anyhow::Result<()> {
        for sc in self.sidecars_of(instance) {
            let _ = engine.kill(&sc).await;
            engine.start(&sc).await.with_context(|| format!("starting {sc}"))?;
            let l = log.renamed(&sc);
            self.follow_logs(&sc, since, l.stream(Stream::Stdout), l.stream(Stream::Stderr));
        }
        Ok(())
    }

    async fn cgroup_usage(id: &str) -> Option<Usage> {
        let dir = Self::cgroup_dir(id)?;
        let stat = tokio::fs::read_to_string(dir.join("cpu.stat")).await.ok()?;
        let usec: u64 = stat.lines().find_map(|l| l.strip_prefix("usage_usec ")).and_then(|v| v.trim().parse().ok())?;
        let mem: u64 = tokio::fs::read_to_string(dir.join("memory.current")).await.ok()?.trim().parse().ok()?;
        Some(Usage { cpu_ns: usec * 1000, memory_bytes: mem })
    }

    fn cgroup_dir(id: &str) -> Option<PathBuf> {
        [format!("/sys/fs/cgroup/system.slice/docker-{id}.scope"), format!("/sys/fs/cgroup/docker/{id}")]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.is_dir())
    }
}

/// A link's relay for `port` on `network`, running (made, or started again after a stop).
async fn ensure_relay(links: &super::links::Links, network: &str, port: u16) -> anyhow::Result<()> {
    let args = super::links::relay_args(network, port, &links.socket_dir(port));
    run_relay(&super::links::relay_name(network, port), &args).await.map_err(|e| e.context(format!("the link relay for port {port}")))
}

/// Relay `name` running: made with `args`, or started again after a stop.
async fn run_relay(name: &str, args: &[String]) -> anyhow::Result<()> {
    match docker(&["inspect", "-f", "{{.State.Running}}", name]).await {
        Ok(r) if r.trim() == "true" => Ok(()),
        Ok(_) => docker(&["start", name]).await.map(drop),
        Err(_) => match docker(&args.iter().map(String::as_str).collect::<Vec<_>>()).await {
            Ok(_) => Ok(()),
            // Another instance's create made it meanwhile.
            Err(e) if e.to_string().contains("already in use") => Ok(()),
            Err(e) => Err(e),
        },
    }
}

/// A network's gateway: where its containers reach the host (`host.docker.internal` on a closed
/// network, which has no route to Docker's default one).
async fn gateway(network: &str) -> Option<String> {
    // An isolated gateway's is no address (Docker says `invalid IP`): the host isn't there.
    docker(&["network", "inspect", "-f", "{{range .IPAM.Config}}{{.Gateway}} {{end}}", network]).await.ok()?.split_whitespace().find(|g| g.parse::<std::net::IpAddr>().is_ok()).map(str::to_string)
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

impl Docker {
    /// Follow a container's output from `since` until it stops (the logs request ends with it).
    fn follow_logs(&self, name: &str, since: chrono::DateTime<chrono::Utc>, o: LogSink, e: LogSink) {
        if let Some(engine) = &self.engine {
            let (engine, name) = (engine.clone(), name.to_string());
            tokio::spawn(async move {
                let since = (since.timestamp(), since.timestamp_subsec_nanos());
                if let Err(err) = engine.follow(&name, since, o.writer(), e.writer()).await {
                    tracing::debug!(%name, "following logs: {err:#}");
                }
            });
            return;
        }
        let since = since.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
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
    }
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
        if !spec.sidecars.is_empty() && self.engine.is_none() {
            bail!("sidecars need Docker on a local socket");
        }
        self.create_container(spec).await?;
        if let Err(e) = self.create_sidecars(spec).await {
            self.remove(&spec.name).await;
            return Err(e);
        }
        // Its variables kept as files, in before it ever starts (a tar from the root, so their
        // folders are made too). Readable by the app's user, whoever it runs as.
        if !spec.files.is_empty() {
            let mut b = tar::Builder::new(Vec::new());
            for (path, contents) in &spec.files {
                let mut h = tar::Header::new_gnu();
                h.set_size(contents.len() as u64);
                h.set_mode(0o444);
                h.set_mtime(0);
                h.set_cksum();
                b.append_data(&mut h, path.trim_start_matches('/'), contents.as_bytes())?;
            }
            let tar = b.into_inner()?;
            if let Err(e) = self.copy_to(&spec.name, "/", Box::new(std::io::Cursor::new(tar))).await {
                let _ = docker(&["rm", "-f", &spec.name]).await;
                return Err(e.context("writing its files"));
            }
        }
        Ok(())
    }
    async fn udp_address(&self, name: &str, port: u16) -> Option<SocketAddr> {
        for c in [sandbox_name(name), name.to_string()] {
            let hp = match &self.engine {
                Some(e) => e.inspect(&c).await.ok().and_then(|i| i.published_udp.get(&port).copied()),
                None => docker(&["port", &c, &format!("{port}/udp")]).await.ok().and_then(|o| o.lines().next()?.rsplit_once(':')?.1.trim().parse().ok()),
            };
            if let Some(hp) = hp {
                return Some(SocketAddr::from(([127, 0, 0, 1], hp)));
            }
        }
        None
    }

    async fn port_address(&self, name: &str, port: u16) -> Option<SocketAddr> {
        // Published on the sandbox when it has one, else on the instance.
        for c in [sandbox_name(name), name.to_string()] {
            let hp = match &self.engine {
                Some(e) => e.inspect(&c).await.ok().and_then(|i| i.published.get(&port).copied()),
                None => docker(&["port", &c, &format!("{port}/tcp")]).await.ok().and_then(|o| o.lines().next()?.rsplit_once(':')?.1.trim().parse().ok()),
            };
            if let Some(hp) = hp {
                return Some(SocketAddr::from(([127, 0, 0, 1], hp)));
            }
        }
        None
    }

    async fn start(&self, name: &str, log: LogSink) -> anyhow::Result<Option<SocketAddr>> {
        let now = chrono::Utc::now();
        let (o, e) = (log.stream(Stream::Stdout), log.stream(Stream::Stderr));
        if let Some(engine) = &self.engine {
            self.quiet_waker(engine, name).await;
            // Running: its own connections aren't callers; the next sleep marks it again.
            self.known.lock().marks.remove(name);
            engine.start(name).await?;
            self.start_sidecars(engine, name, now, &log).await?;
        } else {
            docker(&["start", name]).await?;
        }
        let (_, port) = self.id_and_port(name).await?;
        self.follow_logs(name, now, o, e);
        self.address(name, port).await
    }

    async fn adopt(&self, spec: &InstanceSpec, log: LogSink) -> Option<super::Adopted> {
        // Running (or frozen), and of the deployment its name says.
        let fmt = format!("{{{{.State.Running}}}} {{{{.State.Paused}}}} {{{{index .Config.Labels \"{LABEL}.deployment\"}}}}");
        let out = docker(&["inspect", "-f", &fmt, &spec.name]).await.ok()?;
        let mut f = out.split_whitespace();
        let (running, paused, deployment) = (f.next()? == "true", f.next()? == "true", f.next().unwrap_or(""));
        if !running || deployment != spec.deployment {
            return None;
        }
        let (_, port) = self.id_and_port(&spec.name).await.ok()?;
        let addr = self.address(&spec.name, port).await.ok()?;
        let now = chrono::Utc::now();
        self.follow_logs(&spec.name, now, log.stream(Stream::Stdout), log.stream(Stream::Stderr));
        // Its sidecars, as an earlier process made them.
        let sidecars: Vec<String> = spec.sidecars.iter().map(|s| super::sidecar_name(&spec.name, &s.name)).collect();
        for sc in &sidecars {
            let l = log.renamed(sc);
            self.follow_logs(sc, now, l.stream(Stream::Stdout), l.stream(Stream::Stderr));
        }
        if !sidecars.is_empty() {
            self.known.lock().sidecars.insert(spec.name.clone(), sidecars);
        }
        Some(super::Adopted { addr, paused })
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
        let paused = match &self.engine {
            Some(e) => e.pause(name).await,
            None => docker(&["pause", name]).await.map(|_| ()),
        };
        // Frozen with it: a sidecar meters nothing while the app doesn't run.
        if let (Ok(()), Some(e)) = (&paused, &self.engine) {
            for sc in self.sidecars_of(name) {
                let _ = e.pause(&sc).await;
            }
        }
        paused
    }

    async fn resume(&self, name: &str) -> anyhow::Result<()> {
        if let Some(e) = &self.engine {
            for sc in self.sidecars_of(name) {
                let _ = e.unpause(&sc).await;
            }
        }
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
            // Its sidecars after it (a proxy outlives the app's last requests), then the waker.
            for sc in self.sidecars_of(name) {
                let _ = engine.stop(&sc, secs).await;
            }
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
        // Its sidecars, known or not (an earlier process's): by the label naming their instance.
        let filter = format!("label={LABEL}.of={name}");
        let mut sidecars: Vec<String> = docker(&["ps", "-aq", "--filter", &filter]).await.map(|s| s.lines().map(str::to_string).collect()).unwrap_or_default();
        sidecars.extend(self.sidecars_of(name));
        if !sidecars.is_empty() {
            let mut args = vec!["rm", "-f", "-v"];
            args.extend(sidecars.iter().map(String::as_str));
            let _ = docker(&args).await;
        }
        let _ = docker(&["rm", "-f", "-v", name, &waker_name(name), &sandbox_name(name)]).await;
        if let Some(i) = &self.ingress {
            i.forget(&sandbox_name(name));
        }
        let mut k = self.known.lock();
        k.containers.remove(name);
        k.sandboxes.remove(name);
        k.wakers.remove(name);
        k.marks.remove(name);
        k.sidecars.remove(name);
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

    async fn listeners(&self, name: &str) -> Option<Vec<SocketAddr>> {
        let sb = self.sandbox(name).await?;
        let (tcp, tcp6) = sb.tables();
        (!tcp.is_empty() || !tcp6.is_empty()).then(|| super::netns::listeners(&tcp, &tcp6))
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

    async fn unhealthy(&self, name: &str) -> bool {
        if let Some(e) = &self.engine {
            return e.inspect(name).await.is_ok_and(|i| i.health.as_deref() == Some("unhealthy"));
        }
        docker(&["inspect", "-f", "{{if .State.Health}}{{.State.Health.Status}}{{end}}", name]).await.is_ok_and(|s| s == "unhealthy")
    }

    async fn exited(&self, name: &str) -> Option<super::Exit> {
        let i = self.engine.as_ref()?.inspect(name).await.ok()?;
        (!i.running).then_some(super::Exit { code: i.exit_code, oom: i.oom_killed })
    }

    async fn usage(&self, name: &str) -> Option<Usage> {
        let (id, _) = self.id_and_port(name).await.ok()?;
        let mut u = Self::cgroup_usage(&id).await?;
        // Its sidecars' are the instance's: metered with it.
        for sc in self.sidecars_of(name) {
            if let Ok((id, _)) = self.id_and_port(&sc).await
                && let Some(s) = Self::cgroup_usage(&id).await
            {
                u.cpu_ns += s.cpu_ns;
                u.memory_bytes += s.memory_bytes;
            }
        }
        Some(u)
    }

    async fn list(&self, service: Option<&str>) -> Vec<String> {
        let filter = match service {
            Some(s) => format!("label={LABEL}={s}"),
            None => format!("label={LABEL}"),
        };
        listed(docker(&["ps", "-a", "--filter", &filter, "--format", LISTED]).await)
    }

    async fn list_env(&self, service: &str, env: &str) -> Vec<String> {
        let (s, e) = (format!("label={LABEL}={service}"), format!("label={LABEL}.env={env}"));
        listed(docker(&["ps", "-a", "--filter", &s, "--filter", &e, "--format", LISTED]).await)
    }

    async fn ensure_network(&self, name: &str, service: &str, closed: bool) -> anyhow::Result<()> {
        // One already there must be what's asked for: a closed environment never joins a network
        // with a route out (nor an open one loses its own).
        if let Ok(internal) = docker(&["network", "inspect", "-f", "{{.Internal}}", name]).await {
            if (internal.trim() == "true") != closed {
                bail!("network {name} exists {} a route out; a {} environment can't use it", if closed { "with" } else { "without" }, if closed { "closed" } else { "open" });
            }
            return Ok(());
        }
        let label = format!("{LABEL}={service}");
        let mut args = vec!["network", "create", "--label", &label];
        if closed {
            args.push("--internal");
        }
        // No address of the host's on it either (Docker 28's isolated gateway): its code reaches
        // none of the host's services. rolld then reaches its web instances through their
        // sandboxes' sockets ([`super::ingress`]), so only where those are.
        if closed && self.netns && self.ingress.is_some() {
            args.extend(["-o", "com.docker.network.bridge.gateway_mode_ipv4=isolated", "-o", "com.docker.network.bridge.gateway_mode_ipv6=isolated"]);
        }
        args.push(name);
        match docker(&args).await {
            Ok(_) => Ok(()),
            // Another wake made it meanwhile.
            Err(e) if e.to_string().contains("already exists") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn remove_volume(&self, name: &str) {
        let _ = docker(&["volume", "rm", "-f", name]).await;
    }

    async fn ensure_peer(&self, spec: &super::PeerSpec) -> anyhow::Result<()> {
        anyhow::ensure!(spec.name.starts_with(super::PREFIX), "{} isn't a roll apps peer", spec.name);
        let digest = spec.digest();
        let forwarders: Vec<String> = spec.forwards.iter().map(|(_, port, _)| format!("{}-{port}", spec.name)).collect();
        // As asked, running, its config in place (a restart of Docker loses its tmpfs) and every
        // forwarder up: nothing to do.
        let running = |name: &str, want: String| {
            let name = name.to_string();
            async move { docker(&["inspect", "-f", &format!("{{{{.State.Running}}}} {{{{index .Config.Labels \"{LABEL}.peer\"}}}}"), &name]).await.is_ok_and(|o| o.trim() == want) }
        };
        let tailscale = spec.kind == crate::peering::PeerKind::Tailscale;
        let secret_file = if tailscale { "/run/ts/key" } else { "/run/wg/wg0.conf" };
        if running(&spec.name, format!("true {digest}")).await && docker(&["exec", &spec.name, "test", "-s", secret_file]).await.is_ok() {
            let mut all = true;
            for f in &forwarders {
                all &= running(f, format!("true {digest}")).await;
            }
            if all {
                return Ok(());
            }
        }
        self.remove_peer(&spec.name, &spec.service, &spec.env).await;
        let labels = [format!("{LABEL}={}", spec.service), format!("{LABEL}.env={}", spec.env), format!("{LABEL}.role=peer"), format!("{LABEL}.peer={digest}")];
        let (addrs, routes, mtu) = (format!("ADDRS={}", spec.addresses.join(" ")), format!("ROUTES={}", spec.routes.join(" ")), format!("MTU={}", spec.mtu.map(|m| m.to_string()).unwrap_or_default()));
        let mut args: Vec<&str> = vec!["create", "--name", &spec.name, "--network", &spec.network];
        for (alias, _, _) in &spec.forwards {
            args.extend(["--network-alias", alias]);
        }
        for l in &labels {
            args.extend(["--label", l]);
        }
        let (ts_args, ts_host) = (format!("TS_EXTRA_ARGS=--accept-routes --shields-up{}", spec.login_server.as_deref().map(|u| format!(" --login-server={u}")).unwrap_or_default()), format!("TS_HOSTNAME=roll-{}-{}", spec.service.trim_start_matches("svc_"), spec.env));
        if tailscale {
            // A Tailscale node in userspace: no capability at all, no tun, its key and state only
            // in memory (an ephemeral node), routes reached through its SOCKS5 server.
            args.extend(["--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--read-only", "--tmpfs", "/run/ts:size=64k,mode=0700", "--tmpfs", "/tmp:size=16m", "--tmpfs", "/var/lib/tailscale:size=16m"]);
            args.extend(["--memory", "128m", "--pids-limit", "128", "-e", "TS_AUTHKEY=file:/run/ts/key", "-e", "TS_USERSPACE=true", "-e", "TS_STATE_DIR=/var/lib/tailscale", "-e", "TS_SOCKS5_SERVER=127.0.0.1:1055", "-e", "TS_SOCKET=/tmp/tailscaled.sock", "-e", "TS_ACCEPT_DNS=false", "-e", &ts_host, "-e", &ts_args]);
            args.extend(["--entrypoint", "sh", super::peering_image::TAILSCALE, "-c", super::peering_image::TAILSCALE_SCRIPT]);
        } else {
            // The tunnel in a namespace of its own: NET_ADMIN there and nothing else, no root
            // filesystem to write, its config only in memory.
            args.extend(["--cap-drop", "ALL", "--cap-add", "NET_ADMIN", "--security-opt", "no-new-privileges", "--read-only", "--tmpfs", "/run/wg:size=64k,mode=0700"]);
            args.extend(["--memory", "64m", "--pids-limit", "64", "-e", &addrs, "-e", &routes, "-e", &mtu, "--entrypoint", "sh", super::peering_image::WIREGUARD, "-c", super::peering_image::SCRIPT]);
        }
        docker_within(&args, Some(Duration::from_secs(300))).await.context("the private network's peer")?;
        if let Some(out) = &spec.out_network {
            self.ensure_network(out, &spec.service, false).await?;
            docker(&["network", "connect", out, &spec.name]).await.context("the peer's way out")?;
        }
        docker(&["start", &spec.name]).await?;
        // Its config through stdin into its tmpfs: never on disk, in argv or in an env.
        let mut child = Command::new("docker")
            .args(["exec", "-i", &spec.name, "sh", "-c", if tailscale { "umask 077; cat > /run/ts/.k && mv /run/ts/.k /run/ts/key" } else { "umask 077; cat > /run/wg/.c && mv /run/wg/.c /run/wg/wg0.conf" }])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        {
            use tokio::io::AsyncWriteExt;
            let mut stdin = child.stdin.take().context("no stdin")?;
            stdin.write_all(spec.config.as_bytes()).await?;
        }
        let out = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await.context("writing the peer's config")??;
        anyhow::ensure!(out.status.success(), "the peer's config wasn't written");
        // Up once its interface is, or (Tailscale) once it has its tailnet address; else it says
        // why it stopped.
        let began = std::time::Instant::now();
        let within = Duration::from_secs(if tailscale { 60 } else { 15 });
        loop {
            let up = match tailscale {
                // (Its daemon's socket is in its tmpfs: the CLI's default isn't writable there.)
                true => docker(&["exec", &spec.name, "tailscale", "--socket=/tmp/tailscaled.sock", "ip", "-4"]).await,
                false => docker(&["exec", &spec.name, "ip", "link", "show", "wg0", "up"]).await,
            };
            if up.is_ok_and(|o| !o.trim().is_empty()) {
                break;
            }
            if !running(&spec.name, format!("true {digest}")).await || began.elapsed() > within {
                let why = docker_logs_tail(&spec.name).await;
                self.remove_peer(&spec.name, &spec.service, &spec.env).await;
                anyhow::bail!("the private network's peer didn't come up: {why}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        for ((_, port, target), name) in spec.forwards.iter().zip(&forwarders) {
            // Tailscale's routes through its SOCKS5 server (a MagicDNS name resolved there too).
            let connect = match tailscale {
                true => format!("SOCKS5-CONNECT:127.0.0.1:1055:{target}"),
                false => format!("TCP:{target}"),
            };
            let (listen, net) = (format!("TCP-LISTEN:{port},fork,reuseaddr"), format!("container:{}", spec.name));
            let mut args: Vec<&str> = vec!["run", "-d", "--name", name, "--network", &net];
            for l in &labels {
                args.extend(["--label", l]);
            }
            args.extend(["--cap-drop", "ALL", "--cap-add", "NET_BIND_SERVICE", "--security-opt", "no-new-privileges", "--read-only", "--memory", "32m", "--pids-limit", "64", super::links::IMAGE, &listen, &connect]);
            docker_within(&args, Some(Duration::from_secs(300))).await.context("a private network route's forwarder")?;
        }
        Ok(())
    }

    async fn remove_peer(&self, name: &str, service: &str, env: &str) {
        if !name.starts_with(super::PREFIX) {
            return;
        }
        // The environment's peer and forwarders, by its labels (a name prefix would take
        // `staging-2`'s along with `staging`'s).
        let (s, e, r) = (format!("label={LABEL}={service}"), format!("label={LABEL}.env={env}"), format!("label={LABEL}.role=peer"));
        let Ok(listed) = docker(&["ps", "-a", "--filter", &s, "--filter", &e, "--filter", &r, "--format", "{{.Names}}"]).await else { return };
        for n in listed.lines().map(str::trim).filter(|n| !n.is_empty()) {
            let _ = docker(&["rm", "-f", n]).await;
        }
        // A closed environment's peer's way out goes with it.
        self.remove_network(&format!("{name}-out")).await;
    }

    async fn remove_network(&self, name: &str) {
        // Only one no container is on, stopped ones included (a stopped instance's start would
        // fail on a network gone, and Docker only refuses with running ones).
        if !name.starts_with(super::PREFIX) {
            return;
        }
        let Ok(on) = docker(&["ps", "-a", "--filter", &format!("network={name}"), "--format", "{{.Names}}"]).await else { return };
        // Its link and egress relays go with it: they serve only this network.
        let (relays, others): (Vec<&str>, Vec<&str>) = on.lines().filter(|l| !l.trim().is_empty()).partition(|n| n.starts_with(super::links::NAME_PREFIX) || n.starts_with(super::egress::NAME_PREFIX));
        if others.is_empty() {
            for r in relays {
                let _ = docker(&["rm", "-f", r]).await;
            }
            let _ = docker(&["network", "rm", name]).await;
        }
    }

    async fn volume_used(&self, name: &str) -> Option<u64> {
        if !name.starts_with(super::PREFIX) {
            return None;
        }
        // Never `-v` a volume that isn't there (Docker would make an empty one).
        docker(&["volume", "inspect", name]).await.ok()?;
        let mount = format!("{name}:/data:ro");
        let out = docker(&["run", "--rm", "--network", "none", "--label", &format!("{LABEL}=usage"), "-v", &mount, VOLUME_IMAGE, "du", "-skx", "/data"]).await.ok()?;
        out.split_whitespace().next()?.parse::<u64>().ok().map(|kib| kib * 1024)
    }

    async fn exec_output(&self, name: &str, command: &str, timeout: Duration, max: usize) -> anyhow::Result<super::ExecOutput> {
        use tokio::io::AsyncReadExt;
        anyhow::ensure!(name.starts_with(super::PREFIX), "{name} isn't a roll apps instance");
        // Killed in the container at its timeout, with everything it started: ending `docker exec`
        // here leaves the command running, and `timeout` kills only its own child. It runs in a
        // process group of its own (`setsid`, busybox's and util-linux's) that a watcher kills
        // whole (`kill -9 -<group>`, the one spelling both dash and busybox take); without
        // `setsid`, just its shell. The command comes on stdin (never in the host's process list or
        // Docker's exec record) and runs as `$c`, never spliced into the script; it reads no stdin
        // of its own.
        let secs = timeout.as_secs().max(1).to_string();
        let wrap = r#"c=$(cat); if command -v setsid >/dev/null 2>&1; then setsid sh -c "$c" </dev/null & p=$!; setsid sh -c 'sleep "$0"; kill -9 -"$1"' "$0" "$p" >/dev/null 2>&1 & w=$!; else sh -c "$c" </dev/null & p=$!; (sleep "$0"; kill -9 "$p") >/dev/null 2>&1 & w=$!; fi; wait "$p"; r=$?; kill -9 -"$w" 2>/dev/null || kill -9 "$w" 2>/dev/null; exit "$r""#;
        let mut child = Command::new("docker")
            .args(["exec", "-i", name, "sh", "-c", wrap, &secs])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("running docker exec")?;
        {
            use tokio::io::AsyncWriteExt;
            let mut stdin = child.stdin.take().context("no stdin")?;
            stdin.write_all(command.as_bytes()).await?;
        }
        let (out, err) = (child.stdout.take().context("no stdout")?, child.stderr.take().context("no stderr")?);
        // Each stream read to its end (so the command never blocks on a full pipe), kept to `max`
        // as it comes: what was read stays even if the streams never end (a process left holding
        // them).
        type Kept = std::sync::Arc<parking_lot::Mutex<(Vec<u8>, bool)>>;
        fn read_kept(mut r: impl tokio::io::AsyncRead + Unpin + Send + 'static, max: usize, kept: Kept) -> tokio::task::JoinHandle<()> {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16 << 10];
                while let Ok(n) = r.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let mut k = kept.lock();
                    let room = max.saturating_sub(k.0.len());
                    k.0.extend_from_slice(&buf[..n.min(room)]);
                    k.1 |= n > room;
                }
            })
        }
        let (o, e): (Kept, Kept) = Default::default();
        let (ro, re) = (read_kept(out, max, o.clone()), read_kept(err, max, e.clone()));
        let began = std::time::Instant::now();
        let ended = tokio::time::timeout(timeout + Duration::from_secs(5), async {
            let _ = tokio::join!(ro, re);
            child.wait().await
        })
        .await;
        // Killed at its timeout (in the container), or still holding its streams: no exit code.
        let code = match ended {
            Ok(Ok(status)) => status.code().filter(|_| began.elapsed() < timeout),
            _ => None,
        };
        let (o, e) = (std::mem::take(&mut *o.lock()), std::mem::take(&mut *e.lock()));
        Ok(super::ExecOutput { code, stdout: o.0, stderr: e.0, truncated: o.1 || e.1 })
    }

    async fn copy_from(&self, name: &str, path: &str) -> anyhow::Result<super::VolumeStream> {
        anyhow::ensure!(name.starts_with(super::PREFIX), "{name} isn't a roll apps instance");
        let mut child = Command::new("docker")
            .args(["cp", &format!("{name}:{path}"), "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("running docker cp")?;
        let out = child.stdout.take().context("no stdout")?;
        Ok(Box::new(ChildReader { child: Some(child), out, exited: None }))
    }

    async fn copy_to(&self, name: &str, dir: &str, mut tar: super::VolumeStream) -> anyhow::Result<()> {
        anyhow::ensure!(name.starts_with(super::PREFIX), "{name} isn't a roll apps instance");
        let mut child = Command::new("docker")
            .args(["cp", "-", &format!("{name}:{dir}")])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("running docker cp")?;
        let mut stdin = child.stdin.take().context("no stdin")?;
        tokio::io::copy(&mut tar, &mut stdin).await?;
        drop(stdin);
        let out = child.wait_with_output().await?;
        anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr).trim().chars().take(300).collect::<String>());
        Ok(())
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

    fn intercepted(&self, network: &str) -> Option<super::egress::Intercepted> {
        self.egress.as_ref().and_then(|p| p.intercepted(network))
    }

    fn set_interception(&self, network: &str, i: Option<std::sync::Arc<dyn roll_intercept::Interception>>) {
        if let Some(p) = &self.egress {
            p.set_interception(network, i);
        }
    }

    fn closes_egress(&self) -> bool {
        self.egress.is_some()
    }

    async fn egress_proxy(&self, network: &str, _service: &str, hosts: &[String]) -> anyhow::Result<String> {
        let Some(proxies) = &self.egress else { bail!("egress proxies need the Docker daemon on this host") };
        let digest = proxies.serve(network, hosts)?;
        let name = super::egress::relay_name(network, &digest);
        let args = super::links::relay_args_as(&name, network, &super::egress::alias(&digest), super::egress::PORT, &proxies.socket_dir(&digest), "egress");
        run_relay(&name, &args).await.map_err(|e| e.context("the egress proxy's relay"))?;
        Ok(super::egress::proxy_url(network, hosts))
    }

    async fn build_egress(&self, project: &str) -> anyhow::Result<Option<super::BuildEgress>> {
        if self.egress.is_none() {
            return Ok(None);
        }
        let network = build_network_name(project);
        // No route out, and no address of the host's on it (Docker 28's isolated gateway): a
        // step reaches neither the host's services nor its metadata, only the relay.
        if docker(&["network", "inspect", "-f", "{{.Internal}}", &network]).await.is_err() {
            let label = format!("{LABEL}.project={project}");
            let made = docker(&["network", "create", "--internal", "--label", &label, "-o", "com.docker.network.bridge.gateway_mode_ipv4=isolated", "-o", "com.docker.network.bridge.gateway_mode_ipv6=isolated", &network]).await;
            if let Err(e) = made
                && !e.to_string().contains("already exists")
            {
                return Err(e.context("a closed build's network (Docker 28 or later: its isolated gateway)"));
            }
        }
        let hosts = [super::egress::ANY_PUBLIC.to_string()];
        self.egress_proxy(&network, "", &hosts).await?;
        let relay = super::egress::relay_name(&network, &super::egress::digest(&network, &hosts));
        let ip = docker(&["inspect", "-f", &format!("{{{{(index .NetworkSettings.Networks \"{network}\").IPAddress}}}}"), &relay]).await?;
        let ip = ip.trim().parse::<std::net::IpAddr>().map_err(|_| anyhow::anyhow!("the closed build's proxy has no address on {network}"))?;
        Ok(Some(super::BuildEgress { network: network.clone(), proxy: format!("http://{ip}:{}", super::egress::PORT), builder: network }))
    }

    async fn remove_build_egress(&self, project: &str) {
        let name = build_network_name(project);
        // Its builder's container and its cache go with it (`buildx rm`), then its relay.
        let _ = docker(&["buildx", "rm", "-f", &name]).await;
        self.remove_network(&name).await;
    }

    async fn sweep_build_networks(&self, projects: &std::collections::HashSet<String>) {
        let ours: std::collections::HashSet<String> = projects.iter().map(|p| build_network_name(p)).collect();
        let prefix = format!("{}build-", super::PREFIX);
        let Ok(listed) = docker(&["network", "ls", "--filter", &format!("label={LABEL}.project"), "--format", "{{.Name}}\t{{.CreatedAt}}"]).await else { return };
        let now = chrono::Utc::now();
        for line in listed.lines() {
            let Some((name, created)) = line.split_once('\t') else { continue };
            if !name.starts_with(&prefix) || ours.contains(name) {
                continue;
            }
            // `2026-11-20 22:53:33.596282026 +0100 CET`: one made in the last ten minutes may be
            // a build's that hasn't started its proxy yet.
            let made = created.rsplit_once(' ').and_then(|(t, _)| chrono::DateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S%.f %z").ok());
            if made.is_none_or(|m| now.signed_duration_since(m) < chrono::Duration::minutes(10)) {
                continue;
            }
            match docker(&["ps", "--filter", &format!("network={name}"), "--format", "{{.Names}}"]).await {
                Ok(running) if running.trim().is_empty() => {}
                _ => continue,
            }
            let _ = docker(&["buildx", "rm", "-f", name]).await;
            self.remove_network(name).await;
        }
    }

    fn egress_refused(&self, network: &str, hosts: &[String]) -> Vec<super::egress::Refused> {
        self.egress.as_ref().map(|p| p.refused(network, hosts)).unwrap_or_default()
    }

    fn reach(&self, url: &str) -> String {
        match &self.links {
            Some(l) => l.reach(url),
            None => for_container(url),
        }
    }

    async fn shell(&self, name: &str, command: Option<&str>, cols: u16, rows: u16) -> anyhow::Result<super::Shell> {
        let Some(engine) = &self.engine else { bail!("a shell needs Docker on a local socket") };
        // bash when the image has it, else sh; a login shell, so the image's profile applies. A
        // command (`rails console`) runs as `sh -c` would.
        let login = "if command -v bash >/dev/null 2>&1; then exec bash -l; else exec sh -l; fi";
        let cmd = ["sh", "-c", command.unwrap_or(login)];
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
/// The sandbox holding an instance's network. `ingress` (a closed web instance's, with the owner
/// to run as): `socat` joins each caller on `<dir>/s.sock` to the app's port in the sandbox's
/// network namespace, instead of only sleeping.
fn sandbox_config(spec: &InstanceSpec, host_port: Option<u16>, extra: &[(u16, u16)], host_gateway: Option<&str>, ingress: Option<(&std::path::Path, u32, u32)>) -> serde_json::Value {
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
        "ExtraHosts": host_gateway.map(|g| vec![format!("host.docker.internal:{g}")]).unwrap_or_default(),
        "LogConfig": {"Type": "none", "Config": {}},
    });
    let mut config = json!({
        "Image": VOLUME_IMAGE,
        "Cmd": ["sleep", "2147483647"],
        "Labels": { LABEL: spec.service, format!("{LABEL}.env"): spec.env, format!("{LABEL}.role"): "network" },
    });
    // The app's port: published on loopback, or (a closed network publishes nothing) only
    // exposed, the sandbox reached at its address there.
    if let Some(p) = spec.port {
        let port = format!("{p}/tcp");
        config["ExposedPorts"] = json!({ &port: {} });
        if let Some(hp) = host_port {
            host["PortBindings"] = json!({ &port: [{"HostIp": "127.0.0.1", "HostPort": hp.to_string()}] });
            config["Labels"][format!("{LABEL}.port")] = json!(p.to_string());
            for (cp, hp) in extra {
                let k = format!("{cp}/tcp");
                config["ExposedPorts"][&k] = json!({});
                host["PortBindings"][&k] = json!([{"HostIp": "127.0.0.1", "HostPort": hp.to_string()}]);
            }
            // A UDP service's port over UDP too, on loopback (any free port: the relay reads it).
            if spec.udp {
                let k = format!("{p}/udp");
                config["ExposedPorts"][&k] = json!({});
                host["PortBindings"][&k] = json!([{"HostIp": "127.0.0.1", "HostPort": ""}]);
            }
        }
        if spec.closed {
            config["Labels"][format!("{LABEL}.closed")] = json!(p.to_string());
        }
    }
    if let (Some((dir, uid, gid)), Some(p)) = (ingress, spec.port) {
        use super::ingress::{MOUNT, SOCKET};
        config["Image"] = json!(super::links::IMAGE);
        // A fork a caller: room for a few hundred at once.
        config["Cmd"] = json!([format!("UNIX-LISTEN:{MOUNT}/{SOCKET},fork,unlink-early,mode=600"), format!("TCP:127.0.0.1:{p}")]);
        config["User"] = json!(format!("{uid}:{gid}"));
        host["Binds"] = json!([format!("{}:{MOUNT}", dir.display())]);
        host["Memory"] = json!(64i64 << 20);
        host["MemorySwap"] = json!(64i64 << 20);
        host["PidsLimit"] = json!(512);
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
/// `docker ps`'s names with their role: an instance's sidecars are left out of lists (they go
/// with it, [`Runtime::remove`]), so nothing mistakes one for an instance of its own.
const LISTED: &str = "{{.Names}} {{.Label \"sh.roll.apps.role\"}}";

fn listed(out: anyhow::Result<String>) -> Vec<String> {
    out.map(|s| s.lines().filter_map(|l| l.split_once(' ').map_or(Some(l), |(n, role)| (role.trim() != "sidecar").then_some(n))).filter(|n| !n.is_empty()).map(str::to_string).collect()).unwrap_or_default()
}

/// A sidecar's container: its own image, command, settings and size, in `holder`'s network
/// namespace, hardened as the instance is, restarted if it fails.
fn sidecar_config(spec: &InstanceSpec, sc: &super::SidecarSpec, holder: &str) -> anyhow::Result<serde_json::Value> {
    use serde_json::json;
    let mut env = Vec::with_capacity(sc.vars.len());
    for (k, v) in &sc.vars {
        if v.contains('\0') {
            bail!("sidecar {}: variable {k} holds a NUL byte", sc.name);
        }
        env.push(format!("{k}={v}"));
    }
    let mut config = json!({
        "Image": sc.image,
        "Env": env,
        "Labels": {
            LABEL: spec.service,
            format!("{LABEL}.env"): spec.env,
            format!("{LABEL}.deployment"): spec.deployment,
            format!("{LABEL}.role"): "sidecar",
            format!("{LABEL}.of"): spec.name,
        },
        "HostConfig": {
            "Memory": (sc.memory_mb as i64) << 20,
            "MemorySwap": (sc.memory_mb as i64) << 20,
            "NanoCpus": nanos(sc.cpu),
            "PidsLimit": 1024,
            "SecurityOpt": ["no-new-privileges"],
            "CapDrop": CAP_DROP,
            "RestartPolicy": {"Name": "on-failure", "MaximumRetryCount": 5},
            "Init": true,
            "LogConfig": {"Type": "local", "Config": {}},
            "NetworkMode": format!("container:{holder}"),
        },
    });
    if let Some(cmd) = &sc.command {
        config["Entrypoint"] = json!(["sh"]);
        config["Cmd"] = json!(["-c", cmd]);
    }
    Ok(config)
}

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

fn engine_config(spec: &InstanceSpec, host_port: Option<u16>, extra: &[(u16, u16)], host_gateway: Option<&str>) -> anyhow::Result<serde_json::Value> {
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
        "ExtraHosts": host_gateway.map(|g| vec![format!("host.docker.internal:{g}")]).unwrap_or_default(),
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
    if let (Some(p), true) = (spec.port, spec.closed) {
        config["Labels"][format!("{LABEL}.closed")] = json!(p.to_string());
    }
    if let (Some(p), Some(hp)) = (spec.port, host_port) {
        let port = format!("{p}/tcp");
        config["ExposedPorts"] = json!({ &port: {} });
        host["PortBindings"] = json!({ &port: [{"HostIp": "127.0.0.1", "HostPort": hp.to_string()}] });
        config["Labels"][format!("{LABEL}.port")] = json!(p.to_string());
        for (cp, hp) in extra {
            let k = format!("{cp}/tcp");
            config["ExposedPorts"][&k] = json!({});
            host["PortBindings"][&k] = json!([{"HostIp": "127.0.0.1", "HostPort": hp.to_string()}]);
        }
        if spec.udp {
            let k = format!("{p}/udp");
            config["ExposedPorts"][&k] = json!({});
            host["PortBindings"][&k] = json!([{"HostIp": "127.0.0.1", "HostPort": ""}]);
        }
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
    if let Some(h) = &spec.health {
        config["Healthcheck"] = health_config(h);
    }
    config["HostConfig"] = host;
    Ok(config)
}

/// Docker's health check for a worker's `health_command` (nanoseconds; a check may take as long
/// as the interval, at most 30 s).
fn health_config(h: &super::HealthCheck) -> serde_json::Value {
    let ns = |d: Duration| d.as_nanos() as i64;
    serde_json::json!({
        "Test": ["CMD-SHELL", h.command],
        "Interval": ns(h.every),
        "Timeout": ns(h.every.min(Duration::from_secs(30))),
        "Retries": h.failures,
        "StartPeriod": ns(h.start),
    })
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

/// A project's closed builds' network, and the name of their builder on it.
pub fn build_network_name(project: &str) -> String {
    format!("{}build-{}", super::PREFIX, project.strip_prefix("prj_").unwrap_or(project))
}

/// The BuildKit that runs a closed build's builder: the one the daemon's own builds use.
const BUILDKIT_IMAGE: &str = "moby/buildkit:v0.34.0";

/// The project's closed builder on its network, made once for `proxy` (BuildKit pulls base
/// images through it too); one for an older proxy address goes.
async fn closed_builder(egress: &super::BuildEgress) -> anyhow::Result<()> {
    let wanted = format!("env.HTTPS_PROXY={}", egress.proxy);
    if let Ok(info) = docker(&["buildx", "inspect", &egress.builder]).await {
        // `Driver Options: … env.HTTPS_PROXY="http://…"`.
        if info.contains(&format!("env.HTTPS_PROXY=\"{}\"", egress.proxy)) {
            return Ok(());
        }
        let _ = docker(&["buildx", "rm", "-f", &egress.builder]).await;
    }
    let net = format!("network={}", egress.network);
    let http = format!("env.HTTP_PROXY={}", egress.proxy);
    let image = format!("image={BUILDKIT_IMAGE}");
    let args = ["buildx", "create", "--name", &egress.builder, "--driver", "docker-container", "--driver-opt", &net, "--driver-opt", &wanted, "--driver-opt", &http, "--driver-opt", &image];
    match docker(&args).await {
        Ok(_) => Ok(()),
        Err(e) if e.to_string().contains("existing instance") => Ok(()),
        Err(e) => Err(e.context("a closed build's builder")),
    }
}

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
    /// One more, for a build that isn't a preview's when every slot is taken (Vercel's
    /// prioritized production builds): ten pull requests' builds don't hold up a hotfix.
    reserved: tokio::sync::Semaphore,
    scanner: super::scan::Scanner,
}

impl DockerBuilder {
    pub fn new(at_once: usize) -> DockerBuilder {
        DockerBuilder { slots: tokio::sync::Semaphore::new(at_once), reserved: tokio::sync::Semaphore::new(1), scanner: Default::default() }
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
            Err(_) if job.priority => match self.reserved.try_acquire() {
                Ok(s) => s,
                Err(_) => {
                    log.line("waiting for another build on this node to finish");
                    tokio::select! {
                        s = self.slots.acquire() => s?,
                        s = self.reserved.acquire() => s?,
                        _ = cancel.cancelled() => bail!("canceled"),
                    }
                }
            },
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
        cmd.env("DOCKER_BUILDKIT", "1");
        match &job.egress {
            // A closed build: on its project's builder, whose network reaches only the proxy to
            // public hosts (its steps get it as the proxy variables every package manager reads),
            // and loaded into the daemon's images after.
            Some(e) => {
                closed_builder(e).await?;
                cmd.args(["buildx", "build", "--builder", &e.builder, "--load", "--progress", "plain", "--file"]).arg(&dockerfile);
                for k in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
                    cmd.arg("--build-arg").arg(format!("{k}={}", e.proxy));
                }
            }
            // Links (a database) are reached the way instances reach them.
            None => {
                cmd.args(["build", "--progress", "plain", "--file"]).arg(&dockerfile).args(["--add-host", "host.docker.internal:host-gateway"]);
            }
        }
        cmd.args(["--tag", &job.tag, "--label", &format!("{LABEL}={}", job.service), "--label", &format!("{LABEL}.deployment={}", job.deployment)]);
        if let Some(t) = &job.target {
            cmd.args(["--target", t]);
        }
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

    async fn scan(&self, image: &str, service: &str, log: &LogSink) -> anyhow::Result<Option<super::Scanned>> {
        let (found, sbom) = self.scanner.scan(image, service, |n| log.line(n)).await?;
        Ok(Some(super::Scanned { scanner: super::scan::SCANNER.to_string(), found, sbom }))
    }

    async fn load(&self, archive: &std::path::Path, tag: &str, log: &LogSink) -> anyhow::Result<String> {
        anyhow::ensure!(tag.starts_with(super::PREFIX), "{tag} isn't a roll apps image name");
        let a = archive.to_string_lossy().to_string();
        let out = docker_within(&["load", "-q", "-i", &a], Some(Duration::from_secs(600))).await.map_err(|e| anyhow::anyhow!("loading the image failed: {e}"))?;
        // Its names were taken out: what's loaded is an image id.
        let id = out.lines().filter_map(|l| l.trim().strip_prefix("Loaded image ID: ")).next_back().map(str::to_string).ok_or_else(|| anyhow::anyhow!("docker loaded no image by id: {}", out.trim()))?;
        docker(&["tag", &id, tag]).await?;
        log.line(format!("loaded {} as {tag}", id.chars().take(19).collect::<String>()));
        Ok(tag.to_string())
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
            ports: vec![9000],
            udp: false,
            cpu: 0.5,
            memory_mb: 512,
            network: Some("roll-apps-net-p-production".into()),
            closed: false,
            aliases: vec!["web".into()],
            volume: None,
            idle_stop_s: None,
            registry: None,
            files: vec![],
            health: None,
            sidecars: vec![],
        };
        let c = engine_config(&spec, Some(41234), &[(9000, 41235)], Some("host-gateway")).unwrap();
        assert_eq!((c["HostConfig"]["PortBindings"]["9000/tcp"][0]["HostPort"].as_str(), c["Labels"]["sh.roll.apps.port"].as_str()), (Some("41235"), Some("8080")));
        assert_eq!(c["Env"][0], format!("TLS_KEY={pem}"));
        assert_eq!(c["HostConfig"]["PortBindings"]["8080/tcp"][0]["HostIp"], "127.0.0.1");
        assert_eq!(c["HostConfig"]["CapDrop"][0], "NET_RAW");
        assert_eq!(c["NetworkingConfig"]["EndpointsConfig"]["roll-apps-net-p-production"]["Aliases"][0], "web");
        assert_eq!(c["Labels"]["sh.roll.apps"], "svc_x");
        let nul = InstanceSpec { vars: vec![("A".into(), "a\0b".into())], ..spec };
        assert!(engine_config(&nul, None, &[], None).is_err());
    }
}
