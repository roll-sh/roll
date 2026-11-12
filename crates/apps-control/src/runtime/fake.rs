//! Instances as HTTP servers in this process, for tests: deterministic, no Docker.
//!
//! A fake instance answers every request with JSON naming its instance, deployment and the
//! path, plus the variables listed in `FAKE_ECHO` (comma-separated). Settings through its
//! variables: `FAKE_UNHEALTHY=1` answers 500 on every path, `FAKE_BOOT_MS` delays its first
//! answer, `FAKE_EXIT=<code>` makes it a job that exits with that code (cron runs, workers).
//! The fake builder fails a build whose tree has a file `FAIL_BUILD`.

use super::{BuildJob, Builder, InstanceSpec, Runtime, Usage};
use crate::logs::LogSink;
use async_trait::async_trait;
use axum::extract::Request;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FakeState {
    Created,
    Running,
    Paused,
    Stopped,
}

struct FakeInstance {
    spec: InstanceSpec,
    state: FakeState,
    server: Option<tokio::task::JoinHandle<()>>,
    exited: watch::Sender<Option<i32>>,
}

#[derive(Default)]
pub struct Counts {
    pub creates: AtomicU64,
    pub starts: AtomicU64,
    pub suspends: AtomicU64,
    pub resumes: AtomicU64,
    pub stops: AtomicU64,
    pub pulls: AtomicU64,
    pub builds: AtomicU64,
}

#[derive(Default)]
pub struct Fake {
    instances: Mutex<HashMap<String, FakeInstance>>,
    networks: Mutex<Vec<String>>,
    pub counts: Counts,
}

impl Fake {
    pub fn new() -> Arc<Fake> {
        Arc::new(Fake::default())
    }

    pub fn state(&self, name: &str) -> Option<FakeState> {
        self.instances.lock().get(name).map(|i| i.state)
    }

    /// Instances (name, deployment, state) of a service, by name.
    pub fn instances(&self, service: &str) -> Vec<(String, String, FakeState)> {
        let mut v: Vec<_> = self.instances.lock().values().filter(|i| i.spec.service == service).map(|i| (i.spec.name.clone(), i.spec.deployment.clone(), i.state)).collect();
        v.sort();
        v
    }

    /// The process dies on its own (killed, out of memory): its port stops answering.
    pub fn crash(&self, name: &str) {
        if let Some(i) = self.instances.lock().get_mut(name) {
            if let Some(s) = i.server.take() {
                s.abort();
            }
            i.state = FakeState::Stopped;
            i.exited.send_replace(Some(137));
        }
    }

    pub fn networks(&self) -> Vec<String> {
        self.networks.lock().clone()
    }

    pub fn count(c: &AtomicU64) -> u64 {
        c.load(Ordering::SeqCst)
    }
}

fn var<'a>(spec: &'a InstanceSpec, k: &str) -> Option<&'a str> {
    spec.vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
}

async fn serve(spec: InstanceSpec, listener: tokio::net::TcpListener, log: LogSink) {
    let unhealthy = var(&spec, "FAKE_UNHEALTHY") == Some("1");
    let boot = var(&spec, "FAKE_BOOT_MS").and_then(|v| v.parse().ok()).map(Duration::from_millis);
    let echo: Vec<(String, String)> = var(&spec, "FAKE_ECHO")
        .map(|names| names.split(',').filter_map(|n| var(&spec, n).map(|v| (n.to_string(), v.to_string()))).collect())
        .unwrap_or_default();
    if let Some(b) = boot {
        tokio::time::sleep(b).await;
    }
    log.line(format!("listening on {}", spec.port.unwrap_or(0)));
    let (name, dep) = (spec.name.clone(), spec.deployment.clone());
    let app = axum::Router::new().fallback(move |req: Request| {
        let (name, dep, echo) = (name.clone(), dep.clone(), echo.clone());
        async move {
            let status = if unhealthy { axum::http::StatusCode::INTERNAL_SERVER_ERROR } else { axum::http::StatusCode::OK };
            let vars: serde_json::Map<String, serde_json::Value> = echo.into_iter().map(|(k, v)| (k, v.into())).collect();
            let host = req.headers().get("x-forwarded-host").and_then(|h| h.to_str().ok()).unwrap_or("").to_string();
            (status, axum::Json(serde_json::json!({ "instance": name, "deployment": dep, "path": req.uri().path(), "vars": vars, "forwarded_host": host })))
        }
    });
    let _ = axum::serve(listener, app).await;
}

#[async_trait]
impl Runtime for Fake {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn can_suspend(&self) -> bool {
        true
    }

    async fn pull(&self, image: &str, log: &LogSink) -> anyhow::Result<String> {
        self.counts.pulls.fetch_add(1, Ordering::SeqCst);
        log.line(format!("pulled {image}"));
        Ok(format!("{image}@sha256:{}", "0".repeat(64)))
    }

    async fn create(&self, spec: &InstanceSpec) -> anyhow::Result<()> {
        self.counts.creates.fetch_add(1, Ordering::SeqCst);
        let mut m = self.instances.lock();
        anyhow::ensure!(!m.contains_key(&spec.name), "{} exists", spec.name);
        m.insert(spec.name.clone(), FakeInstance { spec: spec.clone(), state: FakeState::Created, server: None, exited: watch::channel(None).0 });
        Ok(())
    }

    async fn start(&self, name: &str, log: LogSink) -> anyhow::Result<Option<SocketAddr>> {
        self.counts.starts.fetch_add(1, Ordering::SeqCst);
        let spec = {
            let m = self.instances.lock();
            let i = m.get(name).ok_or_else(|| anyhow::anyhow!("no instance {name}"))?;
            i.spec.clone()
        };
        // A job: `FAKE_EXIT`, or a command `exit <code>` (a pre-deploy command, a cron run).
        let exit = spec.command.as_deref().and_then(|c| c.strip_prefix("exit ")).and_then(|c| c.trim().parse::<i32>().ok());
        if let Some(code) = exit.or_else(|| var(&spec, "FAKE_EXIT").and_then(|c| c.parse::<i32>().ok())) {
            log.line("job ran");
            let mut m = self.instances.lock();
            let i = m.get_mut(name).expect("exists");
            i.state = FakeState::Stopped;
            i.exited.send_replace(Some(code));
            return Ok(None);
        }
        let addr = match spec.port {
            Some(_) => {
                let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let addr = l.local_addr()?;
                let server = tokio::spawn(serve(spec.clone(), l, log));
                let mut m = self.instances.lock();
                let i = m.get_mut(name).expect("exists");
                i.server = Some(server);
                Some(addr)
            }
            None => None,
        };
        let mut m = self.instances.lock();
        let i = m.get_mut(name).expect("exists");
        i.state = FakeState::Running;
        i.exited.send_replace(None);
        Ok(addr)
    }

    async fn suspend(&self, name: &str) -> anyhow::Result<()> {
        self.counts.suspends.fetch_add(1, Ordering::SeqCst);
        if let Some(i) = self.instances.lock().get_mut(name) {
            i.state = FakeState::Paused;
        }
        Ok(())
    }

    async fn resume(&self, name: &str) -> anyhow::Result<()> {
        self.counts.resumes.fetch_add(1, Ordering::SeqCst);
        if let Some(i) = self.instances.lock().get_mut(name) {
            i.state = FakeState::Running;
        }
        Ok(())
    }

    async fn stop(&self, name: &str, _grace: Duration) -> anyhow::Result<()> {
        self.counts.stops.fetch_add(1, Ordering::SeqCst);
        if let Some(i) = self.instances.lock().get_mut(name) {
            if let Some(s) = i.server.take() {
                s.abort();
            }
            i.state = FakeState::Stopped;
            i.exited.send_replace(Some(137));
        }
        Ok(())
    }

    async fn remove(&self, name: &str) {
        if let Some(mut i) = self.instances.lock().remove(name)
            && let Some(s) = i.server.take()
        {
            s.abort();
        }
    }

    async fn wait(&self, name: &str) -> anyhow::Result<i32> {
        let mut rx = {
            let m = self.instances.lock();
            m.get(name).ok_or_else(|| anyhow::anyhow!("no instance {name}"))?.exited.subscribe()
        };
        loop {
            if let Some(code) = *rx.borrow_and_update() {
                return Ok(code);
            }
            rx.changed().await?;
        }
    }

    async fn running(&self, name: &str) -> bool {
        matches!(self.state(name), Some(FakeState::Running | FakeState::Paused))
    }

    async fn usage(&self, name: &str) -> Option<Usage> {
        match self.state(name)? {
            FakeState::Running => Some(Usage { cpu_ns: 1_000_000, memory_bytes: 64 << 20 }),
            _ => None,
        }
    }

    async fn list(&self, service: Option<&str>) -> Vec<String> {
        self.instances.lock().values().filter(|i| service.is_none_or(|s| i.spec.service == s)).map(|i| i.spec.name.clone()).collect()
    }

    async fn ensure_network(&self, name: &str, _service: &str) -> anyhow::Result<()> {
        let mut n = self.networks.lock();
        if !n.iter().any(|x| x == name) {
            n.push(name.to_string());
        }
        Ok(())
    }

    async fn remove_volume(&self, _name: &str) {}

    fn reach(&self, url: &str) -> String {
        url.to_string()
    }
}

#[async_trait]
impl Builder for Fake {
    fn name(&self) -> &'static str {
        "fake"
    }

    async fn build(&self, job: &BuildJob, log: &LogSink, _cancel: tokio_util::sync::CancellationToken) -> anyhow::Result<String> {
        self.counts.builds.fetch_add(1, Ordering::SeqCst);
        log.line(format!("#1 building {}", job.tag));
        if job.context.join("FAIL_BUILD").exists() {
            log.line("#2 ERROR: the build failed on purpose");
            anyhow::bail!("the build failed");
        }
        anyhow::ensure!(job.context.join(&job.dockerfile).exists(), "no Dockerfile in the context");
        log.line("#2 DONE");
        Ok(job.tag.clone())
    }

    async fn remove_image(&self, _image: &str) {}
}
