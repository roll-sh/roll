//! Instances as HTTP servers in this process, for tests: deterministic, no Docker.
//!
//! A fake instance answers every request with JSON naming its instance, deployment and the
//! path, plus the variables listed in `FAKE_ECHO` (comma-separated). Settings through its
//! variables: `FAKE_UNHEALTHY=1` answers 500 on every path, `FAKE_BOOT_MS` delays its first
//! answer, `FAKE_SLOW_MS` delays every answer (requests pile up; one still waiting when the instance
//! stops answers 502, as a killed process's would; `/sleep/<ms>` is one such request), `FAKE_MISSING=<prefix>` answers
//! 404 for paths under it, `FAKE_EXIT=<code>` makes it a
//! job that exits with that code (cron runs, workers). The fake builder fails a build whose tree
//! has a file `FAIL_BUILD`, and takes as many milliseconds as a file `SLOW_BUILD` says.

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
    /// Stopped: its requests in flight fail, as a killed process's do.
    killed: tokio_util::sync::CancellationToken,
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
    /// Who the last pull logged in as (a private registry).
    pub pulled_as: Mutex<Option<String>>,
    /// Instances that answer 500 from now on (a running app that broke: [`Fake::sicken`]).
    sick: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Instances the kernel killed for their memory ([`Fake::oom`]).
    oom: Mutex<std::collections::HashSet<String>>,
    /// Instances busy at this share of their CPU ([`Fake::set_busy`]), from when it was set.
    busy: Mutex<HashMap<String, (f64, std::time::Instant)>>,
    /// Volumes' contents (what a backup reads and a restore writes).
    volumes: Mutex<HashMap<String, Vec<u8>>>,
    /// What a tag points at in its registry ([`Fake::move_tag`]); zeros when unset.
    tags: Mutex<HashMap<String, String>>,
    /// Connections each instance's network accepted, and how many are open from other services
    /// ([`Fake::connect`], [`Fake::set_private`]).
    accepted: Mutex<HashMap<String, (u64, u32)>>,
}

impl Fake {
    pub fn new() -> Arc<Fake> {
        Arc::new(Fake::default())
    }

    /// `name` uses `share` of its CPU from now on (its usage's CPU time grows at that rate).
    pub fn set_busy(&self, name: &str, share: f64) {
        self.busy.lock().insert(name.to_string(), (share, std::time::Instant::now()));
    }

    /// Another service connects to `name`'s port while it sleeps (what Docker counts in its
    /// sandbox).
    pub fn connect(&self, name: &str) {
        self.accepted.lock().entry(name.to_string()).or_default().0 += 1;
    }

    /// `n` connections from other services are open to `name` now.
    pub fn set_private(&self, name: &str, n: u32) {
        self.accepted.lock().entry(name.to_string()).or_default().1 = n;
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

    /// The app breaks while it runs: every path answers 500 from now on, its port still open.
    pub fn sicken(&self, name: &str) {
        self.sick.lock().insert(name.to_string());
    }

    /// Someone pushed `image`'s tag again: it points at `digest` (`sha256:…`) now.
    pub fn move_tag(&self, image: &str, digest: &str) {
        self.tags.lock().insert(image.to_string(), digest.to_string());
    }

    fn pinned(&self, image: &str) -> String {
        let digest = self.tags.lock().get(image).cloned().unwrap_or_else(|| format!("sha256:{}", "0".repeat(64)));
        format!("{image}@{digest}")
    }

    /// What a volume holds, as the app wrote it.
    pub fn put_volume(&self, name: &str, data: &[u8]) {
        self.volumes.lock().insert(name.to_string(), data.to_vec());
    }

    pub fn volume(&self, name: &str) -> Option<Vec<u8>> {
        self.volumes.lock().get(name).cloned()
    }

    /// The kernel kills it for its memory limit: [`Fake::crash`], reported as out of memory.
    pub fn oom(&self, name: &str) {
        self.oom.lock().insert(name.to_string());
        self.crash(name);
    }

    /// The process ends by itself with `code` (0: done with its work).
    pub fn exit(&self, name: &str, code: i32) {
        self.crash(name);
        if let Some(i) = self.instances.lock().get_mut(name) {
            i.exited.send_replace(Some(code));
        }
    }

    /// The process dies on its own (killed, out of memory): its port stops answering.
    pub fn crash(&self, name: &str) {
        if let Some(i) = self.instances.lock().get_mut(name) {
            if let Some(s) = i.server.take() {
                s.abort();
            }
            i.killed.cancel();
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

async fn serve(spec: InstanceSpec, listener: tokio::net::TcpListener, log: LogSink, sick: Arc<Mutex<std::collections::HashSet<String>>>, killed: tokio_util::sync::CancellationToken) {
    let unhealthy = var(&spec, "FAKE_UNHEALTHY") == Some("1");
    let boot = var(&spec, "FAKE_BOOT_MS").and_then(|v| v.parse().ok()).map(Duration::from_millis);
    let slow = var(&spec, "FAKE_SLOW_MS").and_then(|v| v.parse().ok()).map(Duration::from_millis);
    let missing = var(&spec, "FAKE_MISSING").map(str::to_string);
    let echo: Vec<(String, String)> = var(&spec, "FAKE_ECHO")
        .map(|names| names.split(',').filter_map(|n| var(&spec, n).map(|v| (n.to_string(), v.to_string()))).collect())
        .unwrap_or_default();
    if let Some(b) = boot {
        tokio::time::sleep(b).await;
    }
    log.line(format!("listening on {}", spec.port.unwrap_or(0)));
    let (name, dep) = (spec.name.clone(), spec.deployment.clone());
    let app = axum::Router::new().fallback(move |req: Request| {
        let (name, dep, echo, sick, missing, killed) = (name.clone(), dep.clone(), echo.clone(), sick.clone(), missing.clone(), killed.clone());
        async move {
            use axum::response::IntoResponse;
            if missing.as_deref().is_some_and(|m| req.uri().path().starts_with(m)) {
                return (axum::http::StatusCode::NOT_FOUND, axum::Json(serde_json::json!({ "path": req.uri().path() }))).into_response();
            }
            if req.uri().path() == "/page" {
                return ([(axum::http::header::CONTENT_TYPE, "text/html")], format!("<html><body><h1>{dep}</h1></body></html>")).into_response();
            }
            // As a gRPC server answers: data, then its status in a trailer.
            if req.uri().path() == "/trailers" {
                let mut t = axum::http::HeaderMap::new();
                t.insert("grpc-status", axum::http::HeaderValue::from_static("0"));
                let frames = futures::stream::iter([Ok::<_, std::convert::Infallible>(hyper::body::Frame::data(bytes::Bytes::from_static(b"ok"))), Ok(hyper::body::Frame::trailers(t))]);
                return axum::response::Response::new(axum::body::Body::new(http_body_util::StreamBody::new(frames)));
            }
            // `/sleep/<ms>`: one slow request (a long upload, a report) on a quick app.
            let slow = req.uri().path().strip_prefix("/sleep/").and_then(|ms| ms.parse().ok()).map(Duration::from_millis).or(slow);
            if let Some(s) = slow {
                tokio::select! {
                    _ = tokio::time::sleep(s) => {}
                    _ = killed.cancelled() => return (axum::http::StatusCode::BAD_GATEWAY, "killed mid-request").into_response(),
                }
            }
            let status = if unhealthy || sick.lock().contains(&name) { axum::http::StatusCode::INTERNAL_SERVER_ERROR } else { axum::http::StatusCode::OK };
            let vars: serde_json::Map<String, serde_json::Value> = echo.into_iter().map(|(k, v)| (k, v.into())).collect();
            let host = req.headers().get("x-forwarded-host").and_then(|h| h.to_str().ok()).unwrap_or("").to_string();
            let cookie = req.headers().get("cookie").and_then(|h| h.to_str().ok()).unwrap_or("").to_string();
            let version = format!("{:?}", req.version());
            let te = req.headers().get("te").and_then(|h| h.to_str().ok()).unwrap_or("").to_string();
            (status, axum::Json(serde_json::json!({ "instance": name, "deployment": dep, "path": req.uri().path(), "vars": vars, "forwarded_host": host, "cookie": cookie, "version": version, "te": te }))).into_response()
        }
    });
    let _ = axum::serve(listener, app).await;
}

#[async_trait]
impl Runtime for Fake {
    fn name(&self) -> &'static str {
        "fake"
    }

    async fn take_callers(&self, name: &str) -> bool {
        self.accepted.lock().get_mut(name).is_some_and(|a| std::mem::take(&mut a.0) > 0)
    }

    async fn private_peers(&self, name: &str) -> Option<u32> {
        Some(self.accepted.lock().get(name).map_or(0, |a| a.1))
    }

    fn can_suspend(&self) -> bool {
        true
    }

    async fn pull(&self, image: &str, log: &LogSink, login: Option<&super::RegistryLogin>) -> anyhow::Result<String> {
        *self.pulled_as.lock() = login.map(|l| l.username.clone());
        self.counts.pulls.fetch_add(1, Ordering::SeqCst);
        log.line(format!("pulled {image}"));
        Ok(self.pinned(image))
    }

    async fn remote_digest(&self, image: &str, _login: Option<&super::RegistryLogin>) -> anyhow::Result<String> {
        Ok(self.pinned(image))
    }

    async fn create(&self, spec: &InstanceSpec) -> anyhow::Result<()> {
        self.counts.creates.fetch_add(1, Ordering::SeqCst);
        let mut m = self.instances.lock();
        anyhow::ensure!(!m.contains_key(&spec.name), "{} exists", spec.name);
        m.insert(spec.name.clone(), FakeInstance { spec: spec.clone(), state: FakeState::Created, server: None, exited: watch::channel(None).0, killed: Default::default() });
        Ok(())
    }

    async fn start(&self, name: &str, log: LogSink) -> anyhow::Result<Option<SocketAddr>> {
        self.counts.starts.fetch_add(1, Ordering::SeqCst);
        if let Some(a) = self.accepted.lock().get_mut(name) {
            a.0 = 0;
        }
        let spec = {
            let m = self.instances.lock();
            let i = m.get(name).ok_or_else(|| anyhow::anyhow!("no instance {name}"))?;
            i.spec.clone()
        };
        // A job: `FAKE_EXIT`, or a command `exit <code>` (a pre-deploy command, a cron run).
        let exit = spec.command.as_deref().and_then(|c| c.strip_prefix("exit ")).and_then(|c| c.trim().parse::<i32>().ok());
        if let Some(code) = exit.or_else(|| var(&spec, "FAKE_EXIT").and_then(|c| c.parse::<i32>().ok())) {
            // A job's task says which it is.
            log.line(match var(&spec, "ROLL_TASK_INDEX") {
                Some(i) => format!("job ran as task {i} of {}", var(&spec, "ROLL_TASK_COUNT").unwrap_or("?")),
                None => "job ran".to_string(),
            });
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
                let killed = tokio_util::sync::CancellationToken::new();
                let server = tokio::spawn(serve(spec.clone(), l, log, self.sick.clone(), killed.clone()));
                let mut m = self.instances.lock();
                let i = m.get_mut(name).expect("exists");
                i.server = Some(server);
                i.killed = killed;
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
        if let Some(a) = self.accepted.lock().get_mut(name) {
            a.0 = 0;
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
        if let Some(a) = self.accepted.lock().get_mut(name) {
            a.0 = 0;
        }
        if let Some(i) = self.instances.lock().get_mut(name) {
            if let Some(s) = i.server.take() {
                s.abort();
            }
            i.killed.cancel();
            i.state = FakeState::Stopped;
            i.exited.send_replace(Some(137));
        }
        Ok(())
    }

    async fn remove(&self, name: &str) {
        if let Some(mut i) = self.instances.lock().remove(name) {
            i.killed.cancel();
            if let Some(s) = i.server.take() {
                s.abort();
            }
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

    /// A static site: an index, a script, and a symlink that must never be served.
    async fn export_files(&self, _image: &str, _dir: &str) -> anyhow::Result<super::VolumeStream> {
        let mut b = tar::Builder::new(Vec::new());
        let files: [(&str, &[u8]); 5] = [
            ("./index.html", b"<!doctype html><title>fake site</title>"),
            ("./assets/app.js", b"start()"),
            ("./404.html", b"<h1>not here</h1>"),
            ("./_redirects", b"/old /assets/app.js 308\n/app/* /index.html 200\n"),
            ("./_headers", b"/assets/*\n  Cache-Control: public, max-age=31536000, immutable\n"),
        ];
        for (name, body) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            h.set_cksum();
            b.append_data(&mut h, name, body)?;
        }
        let mut l = tar::Header::new_gnu();
        l.set_entry_type(tar::EntryType::Symlink);
        l.set_size(0);
        l.set_cksum();
        b.append_link(&mut l, "./passwd", "/etc/passwd")?;
        Ok(Box::new(std::io::Cursor::new(b.into_inner()?)))
    }

    async fn read_volume(&self, name: &str) -> anyhow::Result<super::VolumeStream> {
        let data = self.volumes.lock().get(name).cloned().unwrap_or_default();
        Ok(Box::new(std::io::Cursor::new(data)))
    }

    async fn write_volume(&self, name: &str, mut tar: super::VolumeStream) -> anyhow::Result<()> {
        let mut data = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut tar, &mut data).await?;
        self.volumes.lock().insert(name.to_string(), data);
        Ok(())
    }

    async fn running(&self, name: &str) -> bool {
        matches!(self.state(name), Some(FakeState::Running | FakeState::Paused))
    }

    async fn exited(&self, name: &str) -> Option<super::Exit> {
        let m = self.instances.lock();
        let i = m.get(name)?;
        let code = *i.exited.borrow();
        (i.state == FakeState::Stopped).then_some(super::Exit { code, oom: self.oom.lock().contains(name) })
    }

    async fn usage(&self, name: &str) -> Option<Usage> {
        match self.state(name)? {
            FakeState::Running => {
                let cpu = self.instances.lock().get(name).map_or(0.0, |i| i.spec.cpu);
                let grown = self.busy.lock().get(name).map_or(0.0, |(share, since)| share * cpu * since.elapsed().as_secs_f64() * 1e9);
                Some(Usage { cpu_ns: 1_000_000 + grown as u64, memory_bytes: 64 << 20 })
            }
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

    /// A shell that says which instance it's in, then echoes what it's sent.
    async fn shell(&self, name: &str, cols: u16, rows: u16) -> anyhow::Result<super::Shell> {
        anyhow::ensure!(self.state(name) == Some(FakeState::Running), "{name} isn't running");
        let (ours, theirs) = tokio::io::duplex(64 << 10);
        let banner = format!("fake shell in {name} ({cols}x{rows})\r\n");
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut r, mut w) = tokio::io::split(theirs);
            if w.write_all(banner.as_bytes()).await.is_err() {
                return;
            }
            let mut buf = [0u8; 1024];
            while let Ok(n) = r.read(&mut buf).await {
                if n == 0 || w.write_all(&buf[..n]).await.is_err() {
                    return;
                }
            }
        });
        Ok(super::Shell { id: format!("exec-{name}"), io: Box::new(ours) })
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
        if let Some(ms) = std::fs::read_to_string(job.context.join("SLOW_BUILD")).ok().and_then(|t| t.trim().parse().ok()) {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
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
