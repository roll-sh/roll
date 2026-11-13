//! The Docker CLI, the same way pg-control drives Postgres computes, and [`Docker`]: checks and
//! previews as containers on this host (one node and development; the fleet uses
//! [`crate::fargate`]).

use crate::isolation::{Isolation, Job, Kind};
use anyhow::{Context, bail};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tracing::debug;

/// Every container roll code starts carries this label (`check` or `preview`), so leftovers
/// from a crash can be found and removed.
pub const LABEL: &str = "sh.roll.code";

pub async fn docker(args: &[&str]) -> anyhow::Result<String> {
    debug!(?args, "docker");
    let out = Command::new("docker").args(args).stdin(Stdio::null()).output().await.context("running docker")?;
    if !out.status.success() {
        bail!("docker {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub async fn available() -> bool {
    docker(&["info", "--format", "{{.ServerVersion}}"]).await.is_ok()
}

pub async fn remove(name: &str) {
    let _ = docker(&["rm", "-f", name]).await;
}

/// The daemon whose containers these are (`sh.roll.code.home=<its home>`): several daemons
/// share a host in development, and one must never remove another's containers.
static HOME: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Label every container this process makes with its home, and clean up only those.
pub fn set_home(home: &Path) {
    let _ = HOME.set(home.display().to_string());
}

/// `sh.roll.code.home=<home>` for containers this daemon makes, when [`set_home`] was called.
pub fn home_label() -> Option<String> {
    HOME.get().map(|h| format!("{LABEL}.home={h}"))
}

/// Remove containers of one kind left over from a previous run of the daemon (of this home,
/// when [`set_home`] was called).
pub async fn remove_leftovers(kind: &str) {
    let filter = format!("label={LABEL}={kind}");
    let mut args = vec!["ps".to_string(), "-aq".into(), "--filter".into(), filter];
    if let Some(h) = home_label() {
        args.extend(["--filter".into(), format!("label={h}")]);
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    if let Ok(ids) = docker(&argv).await {
        for id in ids.split_whitespace() {
            remove(id).await;
        }
    }
}

/// Make a cache volume (once): world-writable, so an image that runs as another user can use it.
async fn ensure_cache(name: &str) -> anyhow::Result<()> {
    if docker(&["volume", "inspect", name]).await.is_ok() {
        return Ok(());
    }
    let mut labels = vec!["volume".to_string(), "create".into(), "--label".into(), format!("{LABEL}=cache")];
    if let Some(h) = home_label() {
        labels.extend(["--label".into(), h]);
    }
    labels.push(name.to_string());
    let argv: Vec<&str> = labels.iter().map(String::as_str).collect();
    docker(&argv).await?;
    let mount = format!("{name}:/c");
    let helper = format!("{name}-init");
    let r = docker(&["run", "--rm", "--name", &helper, "-v", &mount, "alpine:3", "chmod", "1777", "/c"]).await;
    if r.is_err() {
        remove(&helper).await;
    }
    r.map(|_| ())
}

pub async fn running(name: &str) -> bool {
    docker(&["inspect", "-f", "{{.State.Running}}", name]).await.is_ok_and(|s| s == "true")
}

/// The host port a container's `port/tcp` is published on.
pub async fn host_port(name: &str, port: u16) -> anyhow::Result<u16> {
    let out = docker(&["port", name, &format!("{port}/tcp")]).await?;
    out.lines()
        .filter_map(|l| l.rsplit_once(':').and_then(|(_, p)| p.trim().parse().ok()))
        .next()
        .with_context(|| format!("container {name} doesn't publish port {port}"))
}

pub async fn logs_tail(name: &str, lines: usize) -> String {
    let n = lines.to_string();
    match Command::new("docker").args(["logs", "--tail", &n, name]).stdin(Stdio::null()).output().await {
        Ok(out) => {
            let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&out.stderr));
            s
        }
        Err(_) => String::new(),
    }
}

/// The longest line `follow` passes on; the rest of a longer one is dropped.
const FOLLOW_LINE_MAX: usize = 16 << 10;

/// `docker logs -f` of a container, stdout and stderr, as lines (each at most
/// [`FOLLOW_LINE_MAX`]). The process goes when the receiver is dropped or the container ends.
pub fn follow(name: &str, tail: usize) -> Option<tokio::sync::mpsc::Receiver<String>> {
    let mut child = Command::new("docker")
        .args(["logs", "--follow", "--tail", &tail.to_string(), name])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    let (out, err) = (child.stdout.take()?, child.stderr.take()?);
    tokio::spawn(lines_to(out, tx.clone()));
    tokio::spawn(lines_to(err, tx.clone()));
    tokio::spawn(async move {
        tokio::select! {
            _ = child.wait() => {}
            _ = tx.closed() => { let _ = child.kill().await; }
        }
    });
    Some(rx)
}

/// Lines from `r` into `tx`, a long line cut at [`FOLLOW_LINE_MAX`], until either end goes.
async fn lines_to(mut r: impl AsyncRead + Unpin, tx: tokio::sync::mpsc::Sender<String>) {
    let (mut buf, mut line) = ([0u8; 8192], Vec::new());
    loop {
        let n = match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            if b == b'\n' {
                if tx.send(String::from_utf8_lossy(&line).trim_end_matches('\r').to_string()).await.is_err() {
                    return;
                }
                line.clear();
            } else if line.len() < FOLLOW_LINE_MAX {
                line.push(b);
            }
        }
    }
    if !line.is_empty() {
        let _ = tx.send(String::from_utf8_lossy(&line).into_owned()).await;
    }
}

/// Container-side resource limits: memory without swap, CPUs, and a process cap.
pub fn limits(memory_mb: u32, cpus: f64) -> Vec<String> {
    let mem = format!("{memory_mb}m");
    vec![
        "--memory".into(),
        mem.clone(),
        "--memory-swap".into(),
        mem,
        "--cpus".into(),
        format!("{cpus}"),
        "--pids-limit".into(),
        "1024".into(),
    ]
}

/// The last `cap` bytes of a stream: logs can be any size, a summary can't.
#[derive(Default)]
pub struct Tail {
    buf: VecDeque<u8>,
    cap: usize,
    dropped: bool,
}

impl Tail {
    pub fn new(cap: usize) -> Tail {
        Tail { buf: VecDeque::with_capacity(cap.min(64 << 10)), cap, dropped: false }
    }

    pub fn push(&mut self, data: &[u8]) {
        let data = if data.len() > self.cap {
            self.dropped = true;
            &data[data.len() - self.cap..]
        } else {
            data
        };
        let over = (self.buf.len() + data.len()).saturating_sub(self.cap);
        if over > 0 {
            self.dropped = true;
            self.buf.drain(..over);
        }
        self.buf.extend(data);
    }

    pub fn text(&self) -> String {
        let bytes: Vec<u8> = self.buf.iter().copied().collect();
        let s = String::from_utf8_lossy(&bytes);
        // Start at a line boundary when the beginning was cut.
        let s = if self.dropped { s.split_once('\n').map_or(&*s, |(_, rest)| rest) } else { &s };
        s.trim_end().to_string()
    }
}

/// Read a child's output into a shared tail until EOF.
pub async fn drain(mut r: impl AsyncRead + Unpin, tail: std::sync::Arc<parking_lot::Mutex<Tail>>) {
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => tail.lock().push(&buf[..n]),
        }
    }
}

/// Containers on this host through the `docker` CLI. A tree is copied in (`docker cp`, never
/// bind-mounted, so a run can't write to the host and leaves nothing root-owned behind); data
/// URLs on loopback are rewritten to `host.docker.internal`.
pub struct Docker;

#[async_trait]
impl Isolation for Docker {
    fn name(&self) -> &'static str {
        "docker"
    }

    async fn prepare(&self, job: &Job, tree: &Path) -> anyhow::Result<()> {
        let workdir = job.kind.workdir();
        let mut args: Vec<String> = vec!["create".into(), "--name".into(), job.name.clone(), "--label".into(), format!("{LABEL}={}", job.kind.as_str())];
        if let Some(c) = &job.change {
            args.extend(["--label".into(), format!("{LABEL}.change={c}")]);
        }
        if let Some(h) = home_label() {
            args.extend(["--label".into(), h]);
        }
        if let (Kind::Preview, Some(port)) = (job.kind, job.port) {
            args.extend(["--publish".into(), format!("127.0.0.1::{port}")]);
        }
        args.extend(["--workdir".into(), workdir.into(), "--add-host".into(), "host.docker.internal:host-gateway".into()]);
        args.extend(limits(job.memory_mb, job.cpus));
        let mut env: Vec<(String, String)> = Vec::new();
        if let Some(cache) = &job.cache {
            ensure_cache(cache).await?;
            args.extend(["--volume".into(), format!("{cache}:{}", crate::isolation::CACHE_DIR)]);
            // The cache's variables first: the job's own env wins.
            env.extend(crate::isolation::CACHE_ENV.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        }
        env.extend(job.env.iter().cloned());
        for (k, v) in &env {
            args.push("--env".into());
            args.push(format!("{k}={v}"));
        }
        args.extend(["--entrypoint".into(), "sh".into(), job.image.clone(), "-c".into(), job.run.clone()]);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        docker(&argv).await?;
        docker(&["cp", &format!("{}/.", tree.display()), &format!("{}:{workdir}", job.name)]).await?;
        Ok(())
    }

    async fn attach(&self, name: &str, tail: Arc<Mutex<Tail>>) -> anyhow::Result<i32> {
        let mut child = Command::new("docker")
            .args(["start", "--attach", name])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let out = tokio::spawn(drain(child.stdout.take().expect("piped"), tail.clone()));
        let err = tokio::spawn(drain(child.stderr.take().expect("piped"), tail));
        let status = child.wait().await?;
        let _ = out.await;
        let _ = err.await;
        Ok(status.code().unwrap_or(-1))
    }

    async fn start(&self, name: &str, port: u16) -> anyhow::Result<SocketAddr> {
        docker(&["start", name]).await?;
        Ok(SocketAddr::from(([127, 0, 0, 1], host_port(name, port).await?)))
    }

    async fn running(&self, name: &str) -> bool {
        running(name).await
    }

    async fn logs_tail(&self, name: &str, lines: usize) -> String {
        logs_tail(name, lines).await
    }

    async fn follow(&self, name: &str, tail: usize) -> Option<tokio::sync::mpsc::Receiver<String>> {
        follow(name, tail)
    }

    async fn remove(&self, name: &str) {
        remove(name).await
    }

    /// Containers are this host's: the same as [`Isolation::remove`].
    async fn release(&self, name: &str) {
        remove(name).await
    }

    async fn remove_leftovers(&self, kind: Kind) {
        remove_leftovers(kind.as_str()).await
    }

    fn reach(&self, url: &str) -> String {
        crate::data::for_container(url)
    }

    async fn drop_cache(&self, name: &str) {
        let _ = docker(&["volume", "rm", "-f", name]).await;
    }
}

pub fn random_suffix() -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut b = [0u8; 10];
    getrandom::fill(&mut b).expect("system randomness");
    b.iter().map(|x| ALPHABET[(*x as usize) % ALPHABET.len()] as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_end_from_a_line_boundary() {
        let mut t = Tail::new(16);
        t.push(b"first line\nsecond\n");
        t.push(b"third\n");
        assert_eq!(t.text(), "second\nthird");
        let mut t = Tail::new(8);
        t.push(&b"x".repeat(100));
        assert_eq!(t.buf.len(), 8);
        let mut t = Tail::new(100);
        t.push(b"all of it\n");
        assert_eq!(t.text(), "all of it");
    }
}
