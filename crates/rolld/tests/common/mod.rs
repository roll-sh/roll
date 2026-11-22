//! Starting rolld in a test: every listener on `127.0.0.1:0`, so the OS picks each port as rolld
//! binds it, and the addresses read back from `--addr-file` (`src/listen.rs`). A port the test
//! picked and let go before rolld bound it could be taken by another test binary in between.
#![allow(dead_code)] // each test binary uses its own part of it

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// rolld's own listeners (the edge's are [`EDGE`]).
pub const LISTENERS: &[&str] = &[
    "api",
    "proxy",
    "redis",
    "redis-http",
    "kafka",
    "s3",
    "storage-api",
    "queues-amqp",
    "queues-management",
    "queues-stomp",
    "queues-web-stomp",
    "queues-mqtt",
    "queues-web-mqtt",
    "queues-stream",
    "queues-http",
    "nosql-cql",
    "nosql-cql-shard-aware",
    "nosql-alternator",
    "nosql-rest",
    "search-http",
    "apps-listen",
    "previews-smtp",
];

/// The edge's public listeners (`ROLL_EDGE=1`).
pub const EDGE: &[&str] = &["edge-https", "edge-pg", "edge-redis", "edge-kafka", "edge-nosql", "edge-amqps", "edge-mqtts", "edge-stomps", "edge-mq-stream"];

/// How long rolld may take to bind its listeners (a loaded machine, a cold Docker).
const STARTUP: Duration = Duration::from_secs(120);

/// Each of `listeners` (flag names, without `--`) on `127.0.0.1:0`, rolld's stderr to a file, and
/// `--addr-file`, both in `dir`. A file left by an earlier start in `dir` is removed first.
pub fn on_any_port(cmd: &mut Command, listeners: &[&str], dir: &Path) -> Started {
    for name in listeners {
        cmd.arg(format!("--{name}")).arg("127.0.0.1:0");
    }
    let addr_file = dir.join("rolld-addrs.json");
    let _ = std::fs::remove_file(&addr_file);
    let stderr = dir.join("rolld.stderr");
    cmd.arg("--addr-file").arg(&addr_file).stderr(std::fs::File::create(&stderr).expect("rolld's stderr file"));
    Started { addr_file, stderr }
}

/// Where a rolld being started writes its addresses and its stderr (unless the test pipes it).
pub struct Started {
    pub addr_file: PathBuf,
    pub stderr: PathBuf,
}

impl Started {
    /// Wait until rolld has written its addresses, failing with its stderr's tail if it exits
    /// first or takes longer than [`STARTUP`].
    pub async fn addrs(&self, child: &mut Child) -> Addrs {
        self.addrs_or(child, || self.tail()).await
    }

    /// [`Started::addrs`], failing with `output` (rolld's output, where the test reads it).
    pub async fn addrs_or(&self, child: &mut Child, output: impl Fn() -> String) -> Addrs {
        let started = Instant::now();
        loop {
            if let Ok(bytes) = std::fs::read(&self.addr_file) {
                return Addrs(serde_json::from_slice(&bytes).expect("rolld's addr file is JSON"));
            }
            if let Some(status) = child.try_wait().unwrap() {
                panic!("rolld exited ({status}) before it listened:\n{}", output());
            }
            assert!(started.elapsed() < STARTUP, "rolld didn't listen within {STARTUP:?}:\n{}", output());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The last lines rolld wrote to stderr.
    pub fn tail(&self) -> String {
        let text = std::fs::read_to_string(&self.stderr).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(60)..].join("\n")
    }
}

/// A rolld's listeners, by flag name: `{"api": "127.0.0.1:41234", …}`, and `"sealed": true`
/// while a sealed start waits for its shares.
pub struct Addrs(HashMap<String, serde_json::Value>);

impl Addrs {
    /// `name`'s address (`127.0.0.1:41234`); panics when rolld doesn't listen on it.
    pub fn addr(&self, name: &str) -> String {
        self.get(name).unwrap_or_else(|| panic!("rolld doesn't listen on {name}: {:?}", self.0))
    }

    pub fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).and_then(|v| v.as_str()).map(String::from)
    }

    pub fn port(&self, name: &str) -> u16 {
        self.addr(name).rsplit_once(':').and_then(|(_, p)| p.parse().ok()).expect("a port")
    }

    /// `http://<name's address>`.
    pub fn url(&self, name: &str) -> String {
        format!("http://{}", self.addr(name))
    }

    pub fn sealed(&self) -> bool {
        self.0.get("sealed").and_then(|v| v.as_bool()).unwrap_or(false)
    }
}
