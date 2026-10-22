//! The Docker CLI, the same way pg-control drives Postgres computes.

use anyhow::{Context, bail};
use std::collections::VecDeque;
use std::process::Stdio;
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

/// Remove containers of one kind left over from a previous run of the daemon.
pub async fn remove_leftovers(kind: &str) {
    let filter = format!("label={LABEL}={kind}");
    if let Ok(ids) = docker(&["ps", "-aq", "--filter", &filter]).await {
        for id in ids.split_whitespace() {
            remove(id).await;
        }
    }
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
