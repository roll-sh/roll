//! Docker helpers for the suites that run against the `roll/nosql` image (`drivers.rs`'s
//! multi-language drivers, `image.rs`, `tools.rs`). They need `ROLL_NOSQL_IMAGE` (built by
//! `deploy/nosql/build-local.sh`, or `docker build -f deploy/nosql/Dockerfile`) and skip
//! themselves without it.
//!
//! Containers run on Docker's default bridge and are reached by their IP (the host routes to it
//! on Linux), so every port is the container's own: ScyllaDB drivers connect to the shard-aware
//! port the server advertises, as they would in production. The shared reference containers
//! (`nosql-ref-*`) are reached the same way; they are never started, stopped or restarted here.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// The image under test, or `None` (the suite skips).
pub fn roll_image() -> Option<String> {
    let image = std::env::var("ROLL_NOSQL_IMAGE").ok().filter(|s| !s.is_empty())?;
    if !docker(&["image", "inspect", &image]).status.success() {
        panic!("ROLL_NOSQL_IMAGE={image} is not a local image (deploy/nosql/build-local.sh builds it)");
    }
    Some(image)
}

pub fn docker(args: &[&str]) -> Output {
    Command::new("docker").args(args).stdin(Stdio::null()).output().expect("docker")
}

fn docker_ok(args: &[&str]) -> String {
    let o = docker(args);
    assert!(o.status.success(), "docker {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// Waits until the machine has `gb` GB of available memory (it is shared; see contracts.md).
pub fn wait_mem(gb: u64) {
    loop {
        let avail = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|m| m.lines().find(|l| l.starts_with("MemAvailable:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()))
            .map(|kb| kb / 1_048_576);
        match avail {
            Some(a) if a < gb => {
                eprintln!("waiting for {gb} GB of free memory ({a} GB available)");
                std::thread::sleep(Duration::from_secs(10));
            }
            _ => return,
        }
    }
}

static SEQ: AtomicU32 = AtomicU32::new(0);

pub fn unique(prefix: &str) -> String {
    format!("{prefix}-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed))
}

/// A container this test started; removed (with its anonymous volumes) on drop.
pub struct Container {
    pub name: String,
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", "-v", &self.name]);
    }
}

/// `docker run -d` options for a roll container.
#[derive(Default, Clone)]
pub struct RunOpts<'a> {
    pub env: Vec<(&'a str, String)>,
    pub args: Vec<String>,
    /// `host_path:container_path[:ro]` bind mounts.
    pub mounts: Vec<String>,
    pub volume: Option<String>,
}

impl Container {
    /// Starts `image` detached with 1 GB of memory.
    pub fn run(image: &str, opts: &RunOpts) -> Container {
        wait_mem(6);
        let name = unique("roll-nosql-test");
        let mut a: Vec<String> = vec!["run".into(), "-d".into(), "--memory".into(), "1g".into(), "--name".into(), name.clone()];
        for (k, v) in &opts.env {
            a.push("-e".into());
            a.push(format!("{k}={v}"));
        }
        for m in &opts.mounts {
            a.push("-v".into());
            a.push(m.clone());
        }
        if let Some(v) = &opts.volume {
            a.push("-v".into());
            a.push(format!("{v}:/var/lib/roll"));
        }
        a.push(image.into());
        a.extend(opts.args.iter().cloned());
        let refs: Vec<&str> = a.iter().map(String::as_str).collect();
        docker_ok(&refs);
        Container { name }
    }

    pub fn ip(&self) -> String {
        ip_of(&self.name).unwrap_or_else(|| panic!("{} has no IP address; logs:\n{}", self.name, self.logs()))
    }

    pub fn logs(&self) -> String {
        let o = docker(&["logs", &self.name]);
        format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
    }

    /// Waits for the CQL port to accept connections; how long it took.
    pub fn wait_cql(&self, timeout: Duration) -> Duration {
        let start = Instant::now();
        loop {
            if let Some(ip) = ip_of(&self.name)
                && let Ok(addr) = format!("{ip}:9042").parse::<SocketAddr>()
                && TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
            {
                return start.elapsed();
            }
            if start.elapsed() > timeout || !self.running() {
                panic!("{} did not open CQL within {timeout:?}; logs:\n{}", self.name, self.logs());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn running(&self) -> bool {
        docker(&["inspect", "-f", "{{.State.Running}}", &self.name]).stdout.starts_with(b"true")
    }

    pub fn exec(&self, args: &[&str]) -> Output {
        let mut a = vec!["exec", &self.name];
        a.extend_from_slice(args);
        docker(&a)
    }

    /// `docker restart`, then waits for CQL again. The IP must not change (drivers keep it).
    pub fn restart(&self) {
        let ip = self.ip();
        docker_ok(&["restart", "-t", "10", &self.name]);
        assert_eq!(self.ip(), ip, "{} changed its IP address on restart", self.name);
        self.wait_cql(Duration::from_secs(60));
    }
}

/// The IP of a shared reference container: `nosql-ref-scylla` and the like, or the container
/// `ROLL_NOSQL_REF_SCYLLA_CONTAINER` (`…_CASSANDRA_…`, `…_SCYLLA_AUTH_…`, `…_CASSANDRA_AUTH_…`)
/// names (scripts/test-nosql.sh starts its own); `None` when it isn't running.
pub fn reference(default_name: &str) -> Option<String> {
    let var = format!("ROLL_NOSQL_REF_{}_CONTAINER", default_name.trim_start_matches("nosql-ref-").replace('-', "_").to_uppercase());
    let name = std::env::var(var).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| default_name.to_string());
    ip_of(&name)
}

/// The bridge IP of a running container.
pub fn ip_of(container: &str) -> Option<String> {
    let o = docker(&["inspect", "-f", "{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}", container]);
    if !o.status.success() {
        return None;
    }
    // A stopped container reports "invalid IP".
    String::from_utf8_lossy(&o.stdout).split_whitespace().find(|s| s.parse::<std::net::IpAddr>().is_ok()).map(str::to_string)
}

/// Builds `dir`'s Dockerfile as `tag` (layers are cached between runs).
pub fn build(dir: &Path, tag: &str, build_args: &[&str]) {
    wait_mem(8);
    let mut a = vec!["build".to_string(), "-q".into(), "--memory".into(), "2g".into(), "-t".into(), tag.into()];
    for b in build_args {
        a.push("--build-arg".into());
        a.push(b.to_string());
    }
    a.push(dir.display().to_string());
    let refs: Vec<&str> = a.iter().map(String::as_str).collect();
    docker_ok(&refs);
}

/// A check's outcome in a suite's output (`PASS name`, `FAIL name: why`, `SKIP name: why`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Fail(String),
    Skip(String),
}

impl Outcome {
    pub fn word(&self) -> &'static str {
        match self {
            Outcome::Pass => "PASS",
            Outcome::Fail(_) => "FAIL",
            Outcome::Skip(_) => "SKIP",
        }
    }
}

pub struct SuiteRun {
    pub checks: BTreeMap<String, Outcome>,
    pub output: String,
}

/// Runs a suite image (on the bridge, 1 GB) with `env`. When it prints `@@RESTART`, `restart`
/// is restarted and the suite is told to go on (a line on its stdin).
pub fn run_suite(image: &str, env: &[(String, String)], restart: Option<&Container>, timeout: Duration) -> SuiteRun {
    wait_mem(6);
    let name = unique("roll-nosql-suite");
    let mut a: Vec<String> = vec!["run".into(), "--rm".into(), "-i".into(), "--memory".into(), "1g".into(), "--name".into(), name.clone()];
    for (k, v) in env {
        a.push("-e".into());
        a.push(format!("{k}={v}"));
    }
    a.push(image.into());
    let mut child = Command::new("docker").args(&a).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("docker run");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
        s
    });
    let watchdog_name = name.clone();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done2 = done.clone();
    std::thread::spawn(move || {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if done2.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let _ = docker(&["rm", "-f", &watchdog_name]);
    });
    let mut checks = BTreeMap::new();
    let mut output = String::new();
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        output.push_str(&line);
        output.push('\n');
        if line.trim() == "@@RESTART" {
            match restart {
                Some(c) => c.restart(),
                None => panic!("{image} asked for a restart without RESTART=1"),
            }
            let _ = writeln!(stdin, "go");
            let _ = stdin.flush();
            continue;
        }
        let (word, rest) = line.split_once(' ').unwrap_or((&line, ""));
        let (check, why) = rest.split_once(": ").map(|(c, w)| (c, w.to_string())).unwrap_or((rest, String::new()));
        let outcome = match word {
            "PASS" => Outcome::Pass,
            "FAIL" => Outcome::Fail(why),
            "SKIP" => Outcome::Skip(why),
            _ => continue,
        };
        checks.insert(check.to_string(), outcome);
    }
    let _ = child.wait();
    done.store(true, Ordering::Relaxed);
    let e = err.join().unwrap_or_default();
    if !e.is_empty() {
        output.push_str("--- stderr\n");
        output.push_str(&e);
    }
    SuiteRun { checks, output }
}

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes the Docker tests of one test binary: one container set at a time (the machine is
/// shared and small).
pub fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}
