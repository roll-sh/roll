//! Instances as HTTP servers in this process, for tests: deterministic, no Docker.
//!
//! A fake instance answers every request with JSON naming its instance, deployment and the
//! path, plus the variables listed in `FAKE_ECHO` (comma-separated). Settings through its
//! variables: `FAKE_UNHEALTHY=1` answers 500 on every path, `FAKE_BOOT_MS` delays its first
//! answer, `FAKE_SLOW_MS` delays every answer (requests pile up; one still waiting when the instance
//! stops answers 502, as a killed process's would; `/sleep/<ms>` is one such request), `FAKE_MISSING=<prefix>` answers
//! 404 for paths under it, `FAKE_CACHE=<value>` answers with it as `Cache-Control` (and `Cache-Tag: <the path's first
//! segment>, all`), and counts
//! the requests it answers at `/count`, `FAKE_EXIT=<code>` makes it a
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
    /// Where it answers while running.
    addr: Option<SocketAddr>,
    /// Where its other ports answer ([`InstanceSpec::ports`]), while running.
    extra: HashMap<u16, SocketAddr>,
    /// An app that listens elsewhere: its port held, bound but not listening, so a connection is
    /// refused and no other test's server on this machine can be handed the port and answer.
    reserved: Option<tokio::net::TcpSocket>,
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
    /// One runtime for several nodes (as Fargate is): `shares_instances`.
    pub shared: std::sync::atomic::AtomicBool,
    instances: Mutex<HashMap<String, FakeInstance>>,
    networks: Mutex<Vec<String>>,
    /// Networks made with no route out, and the egress proxies asked for (network, hosts).
    closed: Mutex<Vec<String>>,
    pub proxies: Mutex<Vec<(String, Vec<String>)>>,
    /// What the egress proxies say they refused ([`Fake::refuse`]).
    refused: Mutex<Vec<super::egress::Refused>>,
    /// Tars written into instances (`copy_to`), by instance and directory, read back as they were.
    copied: Mutex<HashMap<(String, String), Vec<u8>>>,
    /// What a volume reports it takes, over its contents' length ([`Fake::set_volume_used`]).
    volume_used: Mutex<HashMap<String, u64>>,
    pub counts: Counts,
    /// Who the last pull logged in as (a private registry).
    pub pulled_as: Mutex<Option<String>>,
    /// Each build's `no_cache` and its Dockerfile, in order.
    pub built: Mutex<Vec<(bool, String)>>,
    /// Each build's variables' names, in order.
    pub build_vars: Mutex<Vec<Vec<String>>>,
    /// Each build's closed network, in order (none: the default one).
    pub build_networks: Mutex<Vec<Option<String>>>,
    /// The manifests of the image archives it loaded.
    pub loaded: Mutex<Vec<String>>,
    /// Images removed ([`Builder::remove_image`]).
    pub removed: Mutex<Vec<String>>,
    /// Instances that answer 500 from now on (a running app that broke: [`Fake::sicken`]).
    sick: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Instances the kernel killed for their memory ([`Fake::oom`]).
    oom: Mutex<std::collections::HashSet<String>>,
    /// Instances busy at this share of their CPU ([`Fake::set_busy`]), from when it was set.
    busy: Mutex<HashMap<String, (f64, std::time::Instant)>>,
    /// Instances whose health command fails from now on ([`Fake::fail_health`]).
    failing: Mutex<std::collections::HashSet<String>>,
    /// Memory an instance says it uses ([`Fake::set_memory`]; else 64 MiB).
    memory: Mutex<HashMap<String, u64>>,
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

    /// Instance `name`'s health command fails from now on (a consumer that hung), until it starts
    /// again.
    pub fn fail_health(&self, name: &str) {
        self.failing.lock().insert(name.to_string());
    }

    /// Instance `name` uses this many bytes of memory from now on.
    pub fn set_memory(&self, name: &str, bytes: u64) {
        self.memory.lock().insert(name.to_string(), bytes);
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

    /// What instance `name` was made from.
    pub fn spec(&self, name: &str) -> Option<InstanceSpec> {
        self.instances.lock().get(name).map(|i| i.spec.clone())
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

    /// `name` reports `bytes` used, whatever it holds (a full disk without writing one).
    pub fn set_volume_used(&self, name: &str, bytes: u64) {
        self.volume_used.lock().insert(name.to_string(), bytes);
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

    /// Its port stops answering and roll isn't told (no exit to wait on, still running to the
    /// runtime's eye): what a request finds before the runtime notices a kill.
    pub fn vanish(&self, name: &str) {
        if let Some(i) = self.instances.lock().get_mut(name) {
            if let Some(s) = i.server.take() {
                s.abort();
            }
            i.killed.cancel();
        }
    }

    pub fn networks(&self) -> Vec<String> {
        self.networks.lock().clone()
    }

    /// The networks made with no route out.
    pub fn closed_networks(&self) -> Vec<String> {
        self.closed.lock().clone()
    }

    /// An egress proxy refused `host` (as one would, an instance having tried it).
    pub fn refuse(&self, host: &str, private: bool) {
        self.refused.lock().push(super::egress::Refused { host: host.to_string(), private });
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
    let cache = var(&spec, "FAKE_CACHE").and_then(|v| axum::http::HeaderValue::from_str(v).ok());
    let answered = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let echo: Vec<(String, String)> = var(&spec, "FAKE_ECHO")
        .map(|names| names.split(',').filter_map(|n| var(&spec, n).map(|v| (n.to_string(), v.to_string()))).collect())
        .unwrap_or_default();
    if let Some(b) = boot {
        tokio::time::sleep(b).await;
    }
    log.line(format!("listening on {}", spec.port.unwrap_or(0)));
    let (name, dep, kill, port) = (spec.name.clone(), spec.deployment.clone(), killed.clone(), spec.port);
    let app = axum::Router::new().fallback(move |req: Request| {
        let (name, dep, echo, sick, missing, killed, cache, answered) = (name.clone(), dep.clone(), echo.clone(), sick.clone(), missing.clone(), kill.clone(), cache.clone(), answered.clone());
        async move {
            use axum::response::IntoResponse;
            if req.uri().path() == "/count" {
                return answered.load(std::sync::atomic::Ordering::SeqCst).to_string().into_response();
            }
            answered.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
            let traceparent = req.headers().get("traceparent").and_then(|h| h.to_str().ok()).unwrap_or("").to_string();
            let mut r = (status, axum::Json(serde_json::json!({ "instance": name, "deployment": dep, "port": port, "path": req.uri().path(), "vars": vars, "forwarded_host": host, "cookie": cookie, "version": version, "te": te, "traceparent": traceparent }))).into_response();
            if let Some(c) = cache {
                r.headers_mut().insert(axum::http::header::CACHE_CONTROL, c);
                let seg = req.uri().path().trim_start_matches('/').split('/').next().unwrap_or("").to_string();
                if let Ok(t) = axum::http::HeaderValue::from_str(&format!("{seg}, all")) {
                    r.headers_mut().insert("cache-tag", t);
                }
            }
            r
        }
    });
    // Served by hand so a kill ends every connection too, kept-alive ones included, as the
    // kernel closes a killed process's sockets (axum::serve's connections outlive its loop).
    loop {
        let sock = tokio::select! {
            r = listener.accept() => match r {
                Ok((s, _)) => s,
                Err(_) => continue,
            },
            _ = killed.cancelled() => return,
        };
        let (app, killed) = (app.clone(), killed.clone());
        tokio::spawn(async move {
            let svc = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
                use tower::ServiceExt;
                app.clone().oneshot(req.map(axum::body::Body::new))
            });
            let builder = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
            let conn = builder.serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(sock), svc);
            tokio::select! {
                _ = conn => {}
                _ = killed.cancelled() => {}
            }
        });
    }
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
        m.insert(spec.name.clone(), FakeInstance { spec: spec.clone(), state: FakeState::Created, server: None, addr: None, extra: HashMap::new(), reserved: None, exited: watch::channel(None).0, killed: Default::default() });
        Ok(())
    }

    async fn start(&self, name: &str, log: LogSink) -> anyhow::Result<Option<SocketAddr>> {
        self.counts.starts.fetch_add(1, Ordering::SeqCst);
        if let Some(a) = self.accepted.lock().get_mut(name) {
            a.0 = 0;
        }
        // A fresh process isn't the one that hung: its health starts over, as Docker's does.
        self.failing.lock().remove(name);
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
            // An app that binds elsewhere (`FAKE_LISTEN=127.0.0.1:3000`): nothing on its port.
            Some(_) if var(&spec, "FAKE_LISTEN").is_some() => {
                let s = tokio::net::TcpSocket::new_v4()?;
                s.bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
                let addr = s.local_addr()?;
                self.instances.lock().get_mut(name).expect("exists").reserved = Some(s);
                Some(addr)
            }
            Some(main) => {
                let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let addr = l.local_addr()?;
                let killed = tokio_util::sync::CancellationToken::new();
                // Its other ports: the same app, saying which port answered.
                let mut extra = HashMap::new();
                for p in spec.ports.iter().filter(|p| **p != main) {
                    let l2 = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                    extra.insert(*p, l2.local_addr()?);
                    tokio::spawn(serve(InstanceSpec { port: Some(*p), ..spec.clone() }, l2, log.clone(), self.sick.clone(), killed.clone()));
                }
                let server = tokio::spawn(serve(spec.clone(), l, log, self.sick.clone(), killed.clone()));
                let mut m = self.instances.lock();
                let i = m.get_mut(name).expect("exists");
                i.server = Some(server);
                i.killed = killed;
                i.extra = extra;
                Some(addr)
            }
            None => None,
        };
        let mut m = self.instances.lock();
        let i = m.get_mut(name).expect("exists");
        i.state = FakeState::Running;
        i.addr = addr;
        i.exited.send_replace(None);
        Ok(addr)
    }

    async fn port_address(&self, name: &str, port: u16) -> Option<SocketAddr> {
        let m = self.instances.lock();
        let i = m.get(name)?;
        (i.state == FakeState::Running).then(|| i.extra.get(&port).copied()).flatten()
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

    async fn listeners(&self, name: &str) -> Option<Vec<SocketAddr>> {
        let m = self.instances.lock();
        let at = var(&m.get(name)?.spec, "FAKE_LISTEN")?.to_string();
        Some(at.split(',').filter_map(|a| a.trim().parse().ok()).collect())
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
    async fn export_files(&self, image: &str, _dir: &str) -> anyhow::Result<super::VolumeStream> {
        let mut b = tar::Builder::new(Vec::new());
        let big = "console.log('roll');\n".repeat(200);
        // A fingerprinted chunk of this build's own, as Vite names them.
        let h = image.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, c| (h ^ c as u64).wrapping_mul(0x0100_0000_01b3));
        let chunk = format!("./assets/chunk-{:08x}z9.js", h as u32);
        let files: [(&str, &[u8]); 10] = [
            (chunk.as_str(), b"chunk()"),
            // A real image, for image optimization (64×48 PNG).
            ("./assets/hero.png", include_bytes!("fake-hero.png")),
            ("./assets/big.js", big.as_bytes()),
            ("./assets/pre.css", b"body{color:#000}"),
            ("./assets/pre.css.br", b"BROTLI-BYTES"),
            ("./index.html", b"<!doctype html><title>fake site</title>"),
            ("./assets/app.js", b"start()"),
            ("./404.html", b"<h1>not here</h1>"),
            ("./_redirects", b"/old /assets/app.js 308\n/app/* /index.html 200\n/api/* service:api/v1/:splat 200\n"),
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

    async fn volume_used(&self, name: &str) -> Option<u64> {
        if let Some(b) = self.volume_used.lock().get(name) {
            return Some(*b);
        }
        self.volumes.lock().get(name).map(|v| v.len() as u64)
    }

    async fn copy_from(&self, name: &str, path: &str) -> anyhow::Result<super::VolumeStream> {
        anyhow::ensure!(self.state(name) == Some(FakeState::Running), "{name} isn't running");
        let got = self.copied.lock().get(&(name.to_string(), path.to_string())).cloned();
        let bytes = got.ok_or_else(|| anyhow::anyhow!("no such file or directory: {path}"))?;
        Ok(Box::new(std::io::Cursor::new(bytes)))
    }

    async fn copy_to(&self, name: &str, dir: &str, mut tar: super::VolumeStream) -> anyhow::Result<()> {
        anyhow::ensure!(self.state(name) == Some(FakeState::Running), "{name} isn't running");
        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut tar, &mut buf).await?;
        self.copied.lock().insert((name.to_string(), dir.to_string()), buf);
        Ok(())
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

    async fn unhealthy(&self, name: &str) -> bool {
        self.instances.lock().get(name).is_some_and(|i| i.spec.health.is_some() && i.state == FakeState::Running) && self.failing.lock().contains(name)
    }

    async fn adopt(&self, spec: &InstanceSpec, _log: LogSink) -> Option<super::Adopted> {
        let m = self.instances.lock();
        let i = m.get(&spec.name).filter(|i| i.spec.deployment == spec.deployment)?;
        match i.state {
            FakeState::Running => Some(super::Adopted { addr: i.addr, paused: false }),
            FakeState::Paused => Some(super::Adopted { addr: i.addr, paused: true }),
            _ => None,
        }
    }

    fn shares_instances(&self) -> bool {
        self.shared.load(Ordering::SeqCst)
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
                Some(Usage { cpu_ns: 1_000_000 + grown as u64, memory_bytes: self.memory.lock().get(name).copied().unwrap_or(64 << 20) })
            }
            _ => None,
        }
    }

    async fn list(&self, service: Option<&str>) -> Vec<String> {
        self.instances.lock().values().filter(|i| service.is_none_or(|s| i.spec.service == s)).map(|i| i.spec.name.clone()).collect()
    }

    async fn list_env(&self, service: &str, env: &str) -> Vec<String> {
        self.instances.lock().values().filter(|i| i.spec.service == service && i.spec.env == env).map(|i| i.spec.name.clone()).collect()
    }

    async fn ensure_network(&self, name: &str, _service: &str, closed: bool) -> anyhow::Result<()> {
        let mut n = self.networks.lock();
        if !n.iter().any(|x| x == name) {
            n.push(name.to_string());
            if closed {
                self.closed.lock().push(name.to_string());
            }
        }
        Ok(())
    }

    fn closes_egress(&self) -> bool {
        true
    }

    async fn egress_proxy(&self, network: &str, _service: &str, hosts: &[String]) -> anyhow::Result<String> {
        self.proxies.lock().push((network.to_string(), hosts.to_vec()));
        Ok(super::egress::proxy_url(network, hosts))
    }

    async fn build_egress(&self, project: &str) -> anyhow::Result<Option<super::BuildEgress>> {
        let network = format!("roll-apps-build-{}", project.strip_prefix("prj_").unwrap_or(project));
        Ok(Some(super::BuildEgress { network: network.clone(), proxy: "http://192.0.2.1:3128".into(), builder: network }))
    }

    fn egress_refused(&self, _network: &str, _hosts: &[String]) -> Vec<super::egress::Refused> {
        self.refused.lock().clone()
    }

    async fn remove_network(&self, name: &str) {
        self.networks.lock().retain(|x| x != name);
        self.closed.lock().retain(|x| x != name);
    }

    async fn remove_volume(&self, name: &str) {
        self.volumes.lock().remove(name);
    }

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

    /// Records the archive's manifest (what it would load).
    async fn load(&self, archive: &std::path::Path, tag: &str, log: &LogSink) -> anyhow::Result<String> {
        use std::io::Read;
        let mut manifest = String::new();
        for e in tar::Archive::new(std::fs::File::open(archive)?).entries()? {
            let mut e = e?;
            if e.path()?.to_string_lossy() == "manifest.json" {
                e.read_to_string(&mut manifest)?;
            }
        }
        self.loaded.lock().push(manifest);
        log.line(format!("loaded as {tag}"));
        Ok(tag.to_string())
    }

    async fn build(&self, job: &BuildJob, log: &LogSink, cancel: tokio_util::sync::CancellationToken) -> anyhow::Result<String> {
        self.counts.builds.fetch_add(1, Ordering::SeqCst);
        log.line(format!("#1 building {}", job.tag));
        if let Some(ms) = std::fs::read_to_string(job.context.join("SLOW_BUILD")).ok().and_then(|t| t.trim().parse().ok()) {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(ms)) => {}
                _ = cancel.cancelled() => anyhow::bail!("canceled"),
            }
        }
        if job.context.join("FAIL_BUILD").exists() {
            log.line("#2 ERROR: the build failed on purpose");
            anyhow::bail!("the build failed");
        }
        let dockerfile = std::fs::read_to_string(job.context.join(&job.dockerfile)).map_err(|_| anyhow::anyhow!("no Dockerfile in the context"))?;
        self.built.lock().push((job.no_cache, dockerfile));
        self.build_vars.lock().push(job.env.iter().map(|(k, _)| k.clone()).collect());
        self.build_networks.lock().push(job.egress.as_ref().map(|e| e.network.clone()));
        log.line("#2 DONE");
        Ok(job.tag.clone())
    }

    async fn remove_image(&self, image: &str) {
        self.removed.lock().push(image.to_string());
    }

    fn shares_caches(&self) -> bool {
        true
    }
}
