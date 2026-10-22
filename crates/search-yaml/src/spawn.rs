//! Isolated clusters as local processes: `--spawn "<command>"`.
//!
//! The cluster hook (`--cluster-hook`) needs a control plane that creates clusters over HTTP.
//! For a server that runs one cluster per process (roll's in-memory `search-dev`), starting a
//! process per file gives the same isolation without one: every file runs against a server that
//! has never seen another file, so a leak in the target's own cleanup can't cascade, and files run
//! in parallel (`--jobs N`, one process per worker at a time).
//!
//! The command is a template run by `sh -c` with `{port}` (a free loopback port picked for this
//! process) and `{name}` (a unique cluster name) substituted; the cluster is then reached at the
//! URL template (default `http://127.0.0.1:{port}`). It is ready once `GET /` gets any HTTP
//! answer. The shell runs in its own process group and stopping kills the whole group, so
//! wrappers (`env`, pipelines, scripts) don't leave servers behind. Output goes to a log file per
//! worker in the temporary directory, and its tail is part of the error when a start fails.
//!
//! Ports come from 20000–32767, below the ephemeral ranges of Linux (32768+), macOS and Windows
//! (49152+): a port picked there and released can't be taken meanwhile by an outgoing connection
//! or by another program binding port 0 (whose server the readiness check would then mistake for
//! ours). A process-wide cursor hands every start a different port, and each is checked bindable.
//! A server that still exits during startup (its port was taken after all) is retried on another
//! port a few times; a server must also still run after answering, so an answer from someone
//! else's server isn't taken for it.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

use crate::http::{Client, Endpoint};

/// How to start a cluster process.
#[derive(Debug, Clone)]
pub struct SpawnCommand {
    /// Run with `sh -c`; `{port}` and `{name}` are substituted.
    pub command: String,
    /// Where the started cluster listens; `{port}` and `{name}` are substituted.
    pub url: String,
    /// How long a process may take to answer `GET /`.
    pub ready_timeout: Duration,
}

impl SpawnCommand {
    pub fn new(command: impl Into<String>) -> SpawnCommand {
        SpawnCommand { command: command.into(), url: "http://127.0.0.1:{port}".into(), ready_timeout: Duration::from_secs(60) }
    }
}

/// A running cluster process; [`Spawned::stop`] ends it (dropping it kills it too).
#[derive(Debug)]
pub struct Spawned {
    child: tokio::process::Child,
    pub url: String,
    pub port: u16,
    pub log: PathBuf,
    /// The cluster name it was started with.
    pub name: String,
}

const START_ATTEMPTS: usize = 4;

fn substitute(template: &str, port: u16, name: &str) -> String {
    template.replace("{port}", &port.to_string()).replace("{name}", name)
}

const PORTS: std::ops::Range<u16> = 20000..32768;

/// A loopback port outside the ephemeral range that nothing listens on right now.
fn free_port() -> anyhow::Result<u16> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static CURSOR: AtomicU32 = AtomicU32::new(u32::MAX);
    let span = u32::from(PORTS.end - PORTS.start);
    if CURSOR.load(Ordering::Relaxed) == u32::MAX {
        // Different runner processes start at different places.
        let mut seed = u64::from(std::process::id())
            ^ std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
        let start = (crate::runner::splitmix64(&mut seed) % u64::from(span)) as u32;
        let _ = CURSOR.compare_exchange(u32::MAX, start, Ordering::Relaxed, Ordering::Relaxed);
    }
    for _ in 0..span {
        let n = CURSOR.fetch_add(1, Ordering::Relaxed) % span;
        let port = PORTS.start + n as u16;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    bail!("no free port in {}..{}", PORTS.start, PORTS.end)
}

fn log_tail(path: &PathBuf) -> String {
    let text = std::fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(20)..].join("\n");
    if tail.is_empty() { "(no output)".into() } else { tail }
}

impl SpawnCommand {
    /// Starts a process and waits until it answers HTTP. `slot` names the log file (one per
    /// worker, overwritten by each start).
    pub async fn start(&self, name: &str, slot: &str) -> anyhow::Result<Spawned> {
        let log = std::env::temp_dir().join(format!("search-yaml-{}-{slot}.log", std::process::id()));
        let mut last_err = None;
        for _ in 0..START_ATTEMPTS {
            let port = free_port()?;
            match self.start_on(port, name, &log).await {
                Ok(s) => return Ok(s),
                // Only a process that died may have lost a port race; a hung one won't improve.
                Err((e, exited)) => {
                    if !exited {
                        return Err(e);
                    }
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("could not start `{}`", self.command)))
    }

    async fn start_on(&self, port: u16, name: &str, log: &PathBuf) -> Result<Spawned, (anyhow::Error, bool)> {
        let command = substitute(&self.command, port, name);
        let url = substitute(&self.url, port, name);
        let out = std::fs::File::create(log).with_context(|| format!("creating {}", log.display())).map_err(|e| (e, false))?;
        let err = out.try_clone().map_err(|e| (e.into(), false))?;
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(&command).stdin(Stdio::null()).stdout(out).stderr(err).kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let child = cmd.spawn().with_context(|| format!("starting `{command}`")).map_err(|e| (e, false))?;
        let mut spawned = Spawned { child, url: url.clone(), port, log: log.clone(), name: name.to_owned() };
        let endpoint = Endpoint::parse(&url).map_err(|e| (e, false))?;
        let started = Instant::now();
        let mut client = Client::new(endpoint, Duration::from_secs(2));
        loop {
            if let Ok(Some(status)) = spawned.child.try_wait() {
                let e = anyhow::anyhow!("`{command}` exited with {status} before answering on {url}:\n{}", log_tail(log));
                spawned.kill_group();
                let _ = std::fs::remove_file(log);
                return Err((e, true));
            }
            if client.send("GET", "/", &[], None).await.is_ok() && matches!(spawned.child.try_wait(), Ok(None)) {
                return Ok(spawned);
            }
            if started.elapsed() > self.ready_timeout {
                spawned.kill_group();
                let _ = spawned.child.wait().await;
                let e = anyhow::anyhow!(
                    "`{command}` did not answer on {url} within {}s:\n{}",
                    self.ready_timeout.as_secs(),
                    log_tail(log)
                );
                let _ = std::fs::remove_file(log);
                return Err((e, false));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

impl Spawned {
    fn kill_group(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            // The shell leads its own group (process_group(0)): this reaches whatever it started.
            // SAFETY: kill(2) with a negative pid only sends a signal; no memory is involved.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }
        let _ = self.child.start_kill();
    }

    /// Kills the process (and its group) and waits for it. A process that had already exited
    /// (it crashed during the run) keeps its log, renamed so the worker's next process doesn't
    /// overwrite it; that path is returned. Otherwise the log is removed.
    pub async fn stop(mut self) -> Option<(std::process::ExitStatus, PathBuf)> {
        let crashed = self.child.try_wait().ok().flatten();
        self.kill_group();
        let _ = self.child.wait().await;
        match crashed {
            Some(status) => {
                let kept = self.log.with_file_name(format!("{}.crash.log", self.name));
                let _ = std::fs::rename(&self.log, &kept);
                Some((status, kept))
            }
            None => {
                let _ = std::fs::remove_file(&self.log);
                None
            }
        }
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        // `kill_on_drop` covers the shell; the group may hold more.
        if matches!(self.child.try_wait(), Ok(None)) {
            self.kill_group();
        }
    }
}

/// `true` when the URL template can address a spawned process.
pub fn check_template(url: &str) -> anyhow::Result<()> {
    if !url.contains("{port}") {
        bail!("the spawn URL {url} has no {{port}}");
    }
    Endpoint::parse(&substitute(url, 1, "x")).map(|_| ())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn substitution() {
        assert_eq!(substitute("srv --listen 127.0.0.1:{port} --cluster {name}", 9, "c1"), "srv --listen 127.0.0.1:9 --cluster c1");
        assert!(check_template("http://127.0.0.1:{port}").is_ok());
        assert!(check_template("http://127.0.0.1:9200").is_err());
        assert!(check_template("ftp://x:{port}").is_err());
    }

    #[test]
    fn free_ports_are_bindable() {
        let a = free_port().unwrap();
        let b = free_port().unwrap();
        assert!(PORTS.contains(&a) && PORTS.contains(&b));
        assert_ne!(a, b, "consecutive starts get different ports");
        // Anything else on the machine may take a port between the check and our bind, so the
        // claim is only that a returned port is (almost always) bindable right away.
        let bound = (0..5).any(|_| free_port().is_ok_and(|p| std::net::TcpListener::bind(("127.0.0.1", p)).is_ok()));
        assert!(bound, "free_port kept returning ports that could not be bound");
    }

    #[tokio::test]
    async fn a_process_that_exits_reports_its_output() {
        let mut s = SpawnCommand::new("echo starting {name} on {port}; echo broken >&2; exit 3");
        s.ready_timeout = Duration::from_secs(5);
        let e = s.start("yaml-c", "t-exit").await.unwrap_err().to_string();
        assert!(e.contains("exited with"), "{e}");
        assert!(e.contains("starting yaml-c on "), "{e}");
        assert!(e.contains("broken"), "{e}");
    }

    /// A process as `start` leaves it, without the readiness wait.
    fn spawn_raw(command: &str, name: &str) -> Spawned {
        let log = std::env::temp_dir().join(format!("search-yaml-{}-{name}.log", std::process::id()));
        let out = std::fs::File::create(&log).unwrap();
        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .process_group(0)
            .spawn()
            .unwrap();
        Spawned { child, url: String::new(), port: 0, log, name: name.into() }
    }

    #[tokio::test]
    async fn a_crash_keeps_the_log() {
        let mut p = spawn_raw("echo boom; exit 3", "t-crash");
        while matches!(p.child.try_wait(), Ok(None)) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let log = p.log.clone();
        let (status, kept) = p.stop().await.expect("reported as crashed");
        assert_eq!(status.code(), Some(3));
        assert!(std::fs::read_to_string(&kept).unwrap().contains("boom"));
        assert!(!log.exists());
        std::fs::remove_file(kept).unwrap();

        // A process still running when stopped was healthy: its log goes.
        let p = spawn_raw("sleep 30", "t-healthy");
        let log = p.log.clone();
        assert!(p.stop().await.is_none());
        assert!(!log.exists());
    }

    #[tokio::test]
    async fn a_silent_process_times_out_and_is_killed() {
        let mut s = SpawnCommand::new("sleep 30");
        s.ready_timeout = Duration::from_millis(300);
        let started = Instant::now();
        let e = s.start("c", "t-hang").await.unwrap_err().to_string();
        assert!(e.contains("did not answer"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
