//! Services as containers (until roll apps provides [`Apps`]): code-preview's [`Isolation`]
//! (Docker here, a Fargate task on the fleet). A deploy costs nothing; the first request to a
//! service's host starts its container, the request waits until it answers, and after the
//! project's `sleep_after` without requests it stops again.

use crate::engine::Engine;
use crate::ports::{Apps, EnvSource, Target};
use crate::{ServiceState, Store};
use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::StreamExt;
use code_preview::isolation::{Isolation, Job, Kind as JobKind};
use code_preview::preview::{answers, is_upgrade, page, strip_hop_by_hop};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tracing::{info, warn};

/// Puts a revision's tree into a prepared job's container (roll code's pooled work trees).
#[async_trait]
pub trait Trees: Send + Sync {
    async fn prepare(&self, iso: &dyn Isolation, job: &Job, repo: &str, revision: &str) -> anyhow::Result<()>;
    /// Run a check-like job on a revision's tree (seeds): a pooled work tree, leased while the
    /// job's container is made.
    /// On `network` (the preview's own) when one is given.
    async fn run(&self, iso: &Arc<dyn Isolation>, spec: &code_preview::config::CheckSpec, repo: &str, revision: &str, env: &[(String, String)], network: Option<code_preview::isolation::Network>) -> anyhow::Result<code_preview::runner::RunOutput>;
}

#[derive(Clone, Debug, PartialEq)]
enum Phase {
    Stopped,
    Starting,
    Running { addr: SocketAddr },
    /// Idle past `sleep_after`, its container paused (`docker pause`) since `since`: its memory
    /// stays, it uses no CPU, and a request thaws it in tens of milliseconds where a start takes
    /// seconds. Asleep to everyone else (status, cost); stopped for good after
    /// [`RuntimeConfig::warm_for`].
    Frozen { addr: SocketAddr, since: Instant },
    Failed(String),
}

struct Live {
    phase: watch::Sender<Phase>,
    /// Bumped by every stop: a start that began before it must not leave a container running.
    stops: std::sync::atomic::AtomicU64,
    container: Mutex<Option<String>>,
    last_request: Mutex<Instant>,
    started: Mutex<Option<Instant>>,
    sleep_after: Mutex<Duration>,
    /// Held by a start (from making its container to running or failing) and by a stop, so the
    /// two never overlap on the container's name: a start after a sleep waits for its removal,
    /// and a sleep during a start (it bumps `stops` first) waits, then the start gives up.
    lifecycle: tokio::sync::Mutex<()>,
}

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    /// Services answer on `<host label>.<domain>`.
    pub domain: String,
    pub scheme: String,
    /// How long a request waits for a starting service before a "starting" page.
    pub wait: Duration,
    /// How long a service may take to answer before it counts as failed.
    pub start_timeout: Duration,
    /// The dashboard (`https://roll.example`): a private preview's 401 page links there, where a
    /// signed-in person is sent on with the preview's link.
    pub dashboard: Option<String>,
    /// A headless browser for screenshots: an image, and a shell command in it that writes a
    /// PNG of `$URL` to `$OUT` (`chrome --headless --screenshot`).
    pub screenshot_image: String,
    pub screenshot_command: String,
    /// How long an idle service stays frozen before it's stopped (zero: stopped at once, as
    /// before). Docker only: Fargate has no pause.
    pub warm_for: Duration,
    /// The most services frozen on this node at once (their memory stays): the oldest are stopped
    /// past it.
    pub warm_max: usize,
    /// How often a signed-in visitor's session (`access: team`) asks again whether its person may
    /// still open the project.
    pub visitor_recheck: Duration,
    /// Where each preview's interception socket is made (`<dir>/<preview>/intercept.sock`, mounted
    /// into its egress container). `None`: nothing is intercepted here.
    pub intercept_dir: Option<std::path::PathBuf>,
    /// Local development only (`ROLL_PREVIEWS_INTERCEPT_ALLOW_PRIVATE`): intercepted connections
    /// may dial a private address and trust this root too (a local TLS echo host for the live
    /// check). Plain egress is never touched by it.
    pub intercept_private: Option<std::path::PathBuf>,
}

/// Playwright's image has a headless Chromium shell (its path names its version).
pub const SCREENSHOT_IMAGE: &str = "mcr.microsoft.com/playwright:v1.64.1-noble";
/// One load of the page that writes the PNG to `$OUT` and its timings to `$OUT.json`
/// (`shot.js`, in `$ROLL_SHOT_JS`). A command of an operator's own only needs to write `$OUT`.
pub const SCREENSHOT_COMMAND: &str = r#"CHROME="$(ls -d /ms-playwright/chromium_headless_shell-*/chrome-headless-shell-linux64/chrome-headless-shell | head -1)" node -e "$ROLL_SHOT_JS""#;
const SHOT_JS: &str = include_str!("shot.js");
/// The screenshot image with `playwright-core` added (Firefox and WebKit are in the base image,
/// but only Playwright drives them): built once per node from [`BROWSERS_DOCKERFILE`] when a
/// project asks for other browsers (`github.browsers`).
pub const BROWSERS_IMAGE: &str = "roll-previews-shot:1.64.1";
/// Built `FROM` the configured screenshot image (an operator's mirror of it too). `playwright-core`
/// pinned to the image's Playwright, with no install scripts run.
const BROWSERS_DOCKERFILE: &str = "FROM {image}\nRUN npm install -g --no-audit --no-fund --ignore-scripts playwright-core@1.64.1 && npm cache clean --force\nENV NODE_PATH=/usr/lib/node_modules\n";
/// How long building [`BROWSERS_IMAGE`] may take (a registry that doesn't answer isn't waited on).
const BROWSERS_BUILD: Duration = Duration::from_secs(180);
const BROWSERS_JS: &str = include_str!("browsers.js");
/// axe-core's WCAG rules for the screenshot's session (vendored, MPL-2.0): 640 KB, past what one
/// environment variable may hold, so it goes in on the container's stdin.
const AXE_JS: &str = include_str!("../vendor/axe-core/axe.min.js");

/// `docker` with `input` on its stdin, written while its output is read.
async fn docker_with_input(args: &[&str], input: impl Into<Vec<u8>>, max: usize) -> anyhow::Result<String> {
    let input: Vec<u8> = input.into();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut child = tokio::process::Command::new("docker")
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("docker's stdin"))?;
    let feed = tokio::spawn(async move {
        let _ = stdin.write_all(&input).await;
    });
    // What the container writes is read up to `max` (its page's code runs in there): never more.
    let (mut stdout, mut stderr) = (child.stdout.take().ok_or_else(|| anyhow::anyhow!("docker's stdout"))?, child.stderr.take().ok_or_else(|| anyhow::anyhow!("docker's stderr"))?);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let (mut stdout, mut stderr) = ((&mut stdout).take(max as u64 + 1), (&mut stderr).take(64 << 10));
    let (read, _) = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
    read?;
    if out.len() > max {
        let _ = child.kill().await;
        anyhow::bail!("docker {}: over {max} bytes of output", args.first().unwrap_or(&""));
    }
    let status = child.wait().await?;
    let _ = feed.await;
    anyhow::ensure!(status.success(), "docker {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&err).trim());
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        RuntimeConfig {
            domain: "preview.localhost".into(),
            scheme: "http".into(),
            wait: Duration::from_secs(60),
            start_timeout: Duration::from_secs(600),
            dashboard: None,
            screenshot_image: SCREENSHOT_IMAGE.into(),
            screenshot_command: SCREENSHOT_COMMAND.into(),
            warm_for: Duration::from_secs(8 * 3600),
            warm_max: 32,
            visitor_recheck: Duration::from_secs(300),
            intercept_dir: None,
            intercept_private: None,
        }
    }
}

/// How long the bases of projects' own preview domains are kept before they're read again.
const DOMAINS_FOR: Duration = Duration::from_secs(10);

pub struct Runtime {
    cfg: RuntimeConfig,
    /// Projects' own preview domains' bases, and when they were read (every request is matched
    /// against them, so the store is read at most every [`DOMAINS_FOR`]).
    domains: Mutex<(Option<std::time::Instant>, Arc<std::collections::HashSet<String>>)>,
    iso: Arc<dyn Isolation>,
    trees: Arc<dyn Trees>,
    store: Arc<Store>,
    engine: OnceLock<Weak<Engine>>,
    /// In a cluster, a preview's services run on the holder of its lease (`preview:<id>`):
    /// requests for their hosts are forwarded there.
    cluster: OnceLock<Arc<roll_cluster::Cluster>>,
    me: Weak<Runtime>,
    live: Mutex<HashMap<(String, String), Arc<Live>>>,
    client: Client<HttpConnector, Body>,
    /// When building [`BROWSERS_IMAGE`] last failed (a node without npm's registry, say): not
    /// tried again for [`BROWSERS_RETRY`], so screenshots don't each wait on a build that fails.
    browsers_failed: Mutex<Option<std::time::Instant>>,
    /// One build of [`BROWSERS_IMAGE`] at a time (screenshots may run side by side).
    browsers_building: tokio::sync::Mutex<()>,
    /// Held while a preview's egress proxy is checked and (re)made: its services start at once,
    /// and two makes under the one name would refuse each other (or one remove the other's).
    egress_making: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// What tells a preview's interception (rolld, from roll secrets), set once.
    intercepts: OnceLock<Arc<dyn crate::ports::Intercepts>>,
    /// Each preview's interception socket being served here: what it covers, and its listener.
    intercepting: Mutex<HashMap<String, (String, tokio::task::AbortHandle)>>,
    /// The latest requests to each preview's services through this node (newest last), at most
    /// [`REQUESTS_KEPT`] a preview, gone with its last service.
    requests: Mutex<HashMap<String, std::collections::VecDeque<crate::RequestLine>>>,
}

/// How many of a preview's latest requests are kept.
pub const REQUESTS_KEPT: usize = 200;

const BROWSERS_RETRY: Duration = Duration::from_secs(3600);

/// A repo's cache volume.
pub fn cache_name(repo: &str) -> String {
    format!("roll-previews-cache-{}", crate::label_part(repo, 40))
}

/// The longest line [`Runtime::timed_lines`] keeps (bytes of its text; a stamp is ~31 more).
const TIMED_LINE_MAX: usize = 16 << 10;
/// The most of a stream's output [`Runtime::timed_lines`] holds (its newest lines), and reads.
const TIMED_KEEP_MAX: usize = 4 << 20;
const TIMED_READ_MAX: u64 = 64 << 20;
/// How long [`Runtime::timed_lines`] reads before it gives up (nothing: no groups).
const TIMED_READ_FOR: Duration = Duration::from_secs(10);

/// One stream of `docker logs --timestamps` (`2026-11-18T10:00:00.123456789Z text`) as `(ms,
/// stderr, text)`, oldest first: read a line at a time, a line's bytes past [`TIMED_LINE_MAX`]
/// dropped as they come (its text then cut on a character), its newest lines up to `keep` bytes
/// held. A line without a stamp takes the one before's.
async fn read_timed<R: tokio::io::AsyncRead + Unpin>(r: R, stderr: bool, keep: usize) -> Vec<(i64, bool, String)> {
    use tokio::io::AsyncBufReadExt;
    let mut r = tokio::io::BufReader::new(tokio::io::AsyncReadExt::take(r, TIMED_READ_MAX));
    let (mut out, mut held, mut last) = (std::collections::VecDeque::new(), 0, 0);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // Up to the newline, holding at most a stamp and TIMED_LINE_MAX of text.
        let mut ended = false;
        while !ended {
            let Ok(chunk) = r.fill_buf().await else { break };
            if chunk.is_empty() {
                break;
            }
            let (n, nl) = match chunk.iter().position(|b| *b == b'\n') {
                Some(i) => (i + 1, true),
                None => (chunk.len(), false),
            };
            let room = (TIMED_LINE_MAX + 64).saturating_sub(buf.len());
            buf.extend_from_slice(&chunk[..(n - nl as usize).min(room)]);
            r.consume(n);
            ended = nl;
        }
        if buf.is_empty() && !ended {
            break;
        }
        let l = String::from_utf8_lossy(&buf);
        let l: &str = &l;
        let (ts, text) = l.split_once(' ').unwrap_or((l, ""));
        let text = match humantime::parse_rfc3339(ts) {
            Ok(t) => {
                last = t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
                text
            }
            Err(_) => l,
        };
        let mut end = text.len().min(TIMED_LINE_MAX);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        held += end;
        out.push_back((last, stderr, text[..end].to_string()));
        while held > keep {
            held -= out.pop_front().map(|l| l.2.len()).unwrap_or(held);
        }
    }
    out.into()
}

/// A service's stdout and stderr as one, by their stamps (each stream's lines in their order),
/// its last `lines`.
fn merge_timed(mut out: Vec<(i64, bool, String)>, err: Vec<(i64, bool, String)>, lines: usize) -> Vec<(i64, bool, String)> {
    out.extend(err);
    out.sort_by_key(|l| l.0);
    let skip = out.len().saturating_sub(lines);
    out.drain(..skip);
    out
}

/// The relay a forwarded connection goes through: busybox's `nc`.
const FORWARD_IMAGE: &str = "alpine:3.20";

/// A forwarded connection's relay: its stdio, both ways; its container removed when it's dropped
/// (a killed `docker run` client leaves the container running).
struct Relay {
    _child: tokio::process::Child,
    name: String,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
}

impl Drop for Relay {
    fn drop(&mut self) {
        let name = std::mem::take(&mut self.name);
        if let Ok(h) = tokio::runtime::Handle::try_current() {
            // Again a moment later: a connection dropped before Docker had made the container
            // removed nothing the first time.
            h.spawn(async move {
                code_preview::docker::remove(&name).await;
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                code_preview::docker::remove(&name).await;
            });
        }
    }
}

impl tokio::io::AsyncRead for Relay {
    fn poll_read(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stdout).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for Relay {
    fn poll_write(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.stdin).poll_write(cx, buf)
    }
    fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stdin).poll_flush(cx)
    }
    fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stdin).poll_shutdown(cx)
    }
}

/// A child's stdout that keeps the child (killed when dropped) for as long as it's read.
struct ChildOut {
    _child: tokio::process::Child,
    out: tokio::process::ChildStdout,
}

impl tokio::io::AsyncRead for ChildOut {
    fn poll_read(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.out).poll_read(cx, buf)
    }
}

/// The container of a service: every one roll previews makes starts with `roll-previews-`.
pub fn container_name(host: &str) -> String {
    format!("roll-previews-{host}")
}

impl Runtime {
    pub fn new(cfg: RuntimeConfig, iso: Arc<dyn Isolation>, trees: Arc<dyn Trees>, store: Arc<Store>) -> Arc<Runtime> {
        Arc::new_cyclic(|me| Runtime {
            domains: Mutex::new((None, Arc::new(std::collections::HashSet::new()))),
            cfg,
            iso,
            trees,
            store,
            engine: OnceLock::new(),
            cluster: OnceLock::new(),
            me: me.clone(),
            live: Mutex::new(HashMap::new()),
            browsers_failed: Mutex::new(None),
            browsers_building: tokio::sync::Mutex::new(()),
            egress_making: Mutex::new(HashMap::new()),
            intercepts: OnceLock::new(),
            intercepting: Mutex::new(HashMap::new()),
            requests: Mutex::new(HashMap::new()),
            client: Client::builder(TokioExecutor::new()).build_http(),
        })
    }

    /// Connect to the engine (which builds services' environments) and start the idle reaper.
    pub fn attach(self: &Arc<Self>, engine: &Arc<Engine>) {
        let _ = self.engine.set(Arc::downgrade(engine));
        let this = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(15));
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(rt) = this.upgrade() else { return };
                rt.reap().await;
            }
        });
    }

    /// What tells each preview's interception (docs/previews.md, "Keys a preview never holds").
    pub fn set_intercepts(&self, i: Arc<dyn crate::ports::Intercepts>) {
        let _ = self.intercepts.set(i);
    }

    /// Serve preview hosts on their preview's holder, and stop a preview's containers here when
    /// its lease moves to another node.
    pub fn set_cluster(self: &Arc<Self>, cluster: Arc<roll_cluster::Cluster>) {
        let mut ownership = cluster.subscribe_ownership();
        let _ = self.cluster.set(cluster);
        let this = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(o) = ownership.recv().await {
                let roll_cluster::Ownership::Lost { key } = o else { continue };
                let Some(preview) = key.strip_prefix("preview:") else { continue };
                let Some(rt) = this.upgrade() else { return };
                let lives: Vec<((String, String), Arc<Live>)> = rt.live.lock().iter().filter(|((p, _), _)| p == preview).map(|(k, l)| (k.clone(), l.clone())).collect();
                for ((p, s), live) in lives {
                    info!(preview = %p, service = %s, "the preview moved to another node; stopping its service here");
                    rt.stop(&p, &s, &live, None).await;
                    rt.forget(&(p, s), live);
                }
                // Its request log is the new holder's now.
                rt.requests.lock().remove(preview);
            }
        });
    }

    fn engine(&self) -> anyhow::Result<Arc<Engine>> {
        self.engine.get().and_then(Weak::upgrade).ok_or_else(|| anyhow::anyhow!("the preview runtime isn't attached"))
    }

    fn live(&self, preview: &str, service: &str) -> Arc<Live> {
        self.live
            .lock()
            .entry((preview.to_string(), service.to_string()))
            .or_insert_with(|| {
                Arc::new(Live {
                    phase: watch::channel(Phase::Stopped).0,
                    stops: std::sync::atomic::AtomicU64::new(0),
                    container: Mutex::new(None),
                    last_request: Mutex::new(Instant::now()),
                    started: Mutex::new(None),
                    sleep_after: Mutex::new(Duration::from_secs(600)),
                    lifecycle: tokio::sync::Mutex::new(()),
                })
            })
            .clone()
    }

    /// Drop a stopped service's entry, unless something took it meanwhile (a request whose start
    /// must stay serialised with the stop: a fresh entry would start a second container under
    /// the same name).
    fn forget(&self, key: &(String, String), live: Arc<Live>) {
        let mut map = self.live.lock();
        // Held by the map and by `live`, nothing else.
        if map.get(key).is_some_and(|l| Arc::ptr_eq(l, &live)) && Arc::strong_count(&live) == 2 {
            map.remove(key);
        }
    }

    async fn stop(&self, preview: &str, service: &str, live: &Live, why: Option<String>) {
        live.stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _turn = live.lifecycle.lock().await;
        let name = live.container.lock().take();
        if let Some(name) = name {
            self.iso.release(&name).await;
        }
        let ran = live.started.lock().take().map(|t| t.elapsed().as_millis() as i64).unwrap_or(0);
        let was_running = matches!(*live.phase.borrow(), Phase::Running { .. } | Phase::Starting);
        // `Starting` here is a start asked for after this stop (none runs while it holds the
        // lock): it's queued behind it, so its waiters keep waiting for it.
        live.phase.send_if_modified(|p| {
            if matches!(p, Phase::Starting) {
                return false;
            }
            *p = Phase::Stopped;
            true
        });
        if (was_running || ran > 0)
            && let Ok(e) = self.engine()
        {
            e.observed(preview, service, ServiceState::Sleeping, why, ran).await;
        }
    }

    async fn reap(&self) {
        let warm = self.iso.name() == "docker" && !self.cfg.warm_for.is_zero() && self.cfg.warm_max > 0;
        let all: Vec<((String, String), Arc<Live>)> = self.live.lock().iter().map(|(k, l)| (k.clone(), l.clone())).collect();
        let (mut idle, mut frozen) = (Vec::new(), Vec::new());
        for (key, live) in all {
            let phase = live.phase.borrow().clone();
            match phase {
                Phase::Running { .. } if live.last_request.lock().elapsed() > *live.sleep_after.lock() => idle.push((key, live)),
                Phase::Frozen { since, .. } => frozen.push((since, key, live)),
                _ => {}
            }
        }
        // Frozen long enough, or past the node's room (oldest first, leaving room for those
        // about to freeze): stopped for good.
        frozen.sort_by_key(|(since, ..)| *since);
        let over = frozen.len().saturating_sub(self.cfg.warm_max.saturating_sub(if warm { idle.len() } else { 0 }));
        for (i, (since, (p, s), live)) in frozen.into_iter().enumerate() {
            if !warm || i < over || since.elapsed() > self.cfg.warm_for {
                info!(preview = %p, service = %s, "preview service frozen long enough; stopping");
                self.stop(&p, &s, &live, None).await;
            }
        }
        for ((p, s), live) in idle {
            if warm && self.freeze(&p, &s, &live).await {
                continue;
            }
            info!(preview = %p, service = %s, "preview service idle; stopping");
            self.stop(&p, &s, &live, None).await;
        }
    }

    /// Freeze an idle service (`docker pause`) instead of stopping it: reported asleep, as a stop
    /// is. `false`: it couldn't be (the caller stops it). A request that raced the pause (it saw
    /// `Running` and went to the container) thaws it again at once.
    async fn freeze(&self, preview: &str, service: &str, live: &Live) -> bool {
        let turn = live.lifecycle.lock().await;
        let phase = live.phase.borrow().clone();
        let Phase::Running { addr } = phase else { return true };
        let checked = Instant::now();
        // A request since the reaper looked: not idle after all.
        if live.last_request.lock().elapsed() <= *live.sleep_after.lock() {
            return true;
        }
        let Some(name) = live.container.lock().clone() else { return false };
        if code_preview::docker::docker(&["pause", &name]).await.is_err() {
            return false;
        }
        live.phase.send_replace(Phase::Frozen { addr, since: Instant::now() });
        let raced = *live.last_request.lock() >= checked;
        if raced {
            if code_preview::docker::docker(&["unpause", &name]).await.is_ok() {
                live.phase.send_replace(Phase::Running { addr });
                return true;
            }
            return false;
        }
        let ran = live.started.lock().take().map(|t| t.elapsed().as_millis() as i64).unwrap_or(0);
        drop(turn);
        info!(%preview, %service, "preview service idle; frozen");
        if let Ok(e) = self.engine() {
            e.observed(preview, service, ServiceState::Sleeping, None, ran).await;
        }
        true
    }

    /// Thaw a frozen service (`docker unpause`): where it answers, or `None` when it isn't frozen
    /// any more or won't thaw (its container removed; the caller starts it as usual).
    async fn thaw(&self, preview: &str, service: &str, live: &Live) -> Option<SocketAddr> {
        let turn = live.lifecycle.lock().await;
        let phase = live.phase.borrow().clone();
        let Phase::Frozen { addr, .. } = phase else { return None };
        let name = live.container.lock().clone();
        let thawed = match &name {
            Some(name) => code_preview::docker::docker(&["unpause", name]).await.is_ok(),
            None => false,
        };
        if !thawed {
            let name = live.container.lock().take();
            if let Some(name) = name {
                self.iso.release(&name).await;
            }
            live.phase.send_replace(Phase::Stopped);
            return None;
        }
        *live.started.lock() = Some(Instant::now());
        live.phase.send_replace(Phase::Running { addr });
        drop(turn);
        if let Ok(e) = self.engine() {
            e.observed(preview, service, ServiceState::Running, None, 0).await;
        }
        Some(addr)
    }

    /// The container a terminal in the service goes into: started (or thawed) if it isn't
    /// running, waited for up to 60 s, and used from then on (as a request would).
    pub async fn shell_container(self: &Arc<Self>, preview: &str, service: &str) -> anyhow::Result<String> {
        match self.ready(preview, service, Duration::from_secs(60)).await {
            Ok(_) => {}
            Err(Unavailable::Starting) => anyhow::bail!("{service} is still starting: try again in a moment"),
            Err(Unavailable::NotFound) => anyhow::bail!("{service} isn't running on this node"),
            Err(Unavailable::Failed(e)) => anyhow::bail!("{service} didn't start: {e}"),
        }
        let live = self.live(preview, service);
        let name = live.container.lock().clone();
        name.ok_or_else(|| anyhow::anyhow!("{service} isn't running on this node"))
    }

    /// The service is in use through a terminal: its idle clock starts again.
    pub fn hold(&self, preview: &str, service: &str) {
        if let Some(live) = self.live.lock().get(&(preview.to_string(), service.to_string())) {
            *live.last_request.lock() = Instant::now();
        }
        if let Ok(e) = self.engine() {
            e.touched(preview);
        }
    }

    /// Where a service answers, starting it if needed; waits up to `wait`.
    async fn ready(self: &Arc<Self>, preview: &str, service: &str, wait: Duration) -> Result<SocketAddr, Unavailable> {
        let live = self.live(preview, service);
        *live.last_request.lock() = Instant::now();
        if let Ok(e) = self.engine() {
            e.touched(preview);
        }
        let mut rx = live.phase.subscribe();
        // Clone first: a borrow held through the match would deadlock `send_replace`.
        let mut phase = rx.borrow_and_update().clone();
        if matches!(phase, Phase::Frozen { .. }) {
            if let Some(addr) = self.thaw(preview, service, &live).await {
                return Ok(addr);
            }
            phase = rx.borrow_and_update().clone();
        }
        let start = match phase {
            Phase::Running { addr } | Phase::Frozen { addr, .. } => return Ok(addr),
            Phase::Starting => false,
            Phase::Stopped | Phase::Failed(_) => {
                live.phase.send_replace(Phase::Starting);
                true
            }
        };
        if start {
            let (this, p, s, l) = (self.clone(), preview.to_string(), service.to_string(), live.clone());
            tokio::spawn(async move { this.start(&p, &s, &l).await });
        }
        let waited = tokio::time::timeout(wait, async {
            loop {
                if rx.changed().await.is_err() {
                    return Err(Unavailable::NotFound);
                }
                match rx.borrow_and_update().clone() {
                    Phase::Running { addr } => return Ok(addr),
                    Phase::Failed(e) => return Err(Unavailable::Failed(e)),
                    Phase::Stopped => return Err(Unavailable::NotFound),
                    Phase::Starting | Phase::Frozen { .. } => {}
                }
            }
        });
        waited.await.unwrap_or(Err(Unavailable::Starting))
    }

    async fn start(&self, preview: &str, service: &str, live: &Live) {
        let _turn = live.lifecycle.lock().await;
        // Another start (queued behind the same stop) got there first.
        if matches!(*live.phase.borrow(), Phase::Running { .. }) {
            return;
        }
        live.phase.send_replace(Phase::Starting);
        match tokio::time::timeout(self.cfg.start_timeout, self.start_inner(preview, service, live)).await {
            Ok(Ok(addr)) => {
                *live.started.lock() = Some(Instant::now());
                live.phase.send_replace(Phase::Running { addr });
                if let Ok(e) = self.engine() {
                    e.observed(preview, service, ServiceState::Running, None, 0).await;
                }
            }
            failed => {
                let e = match failed {
                    Ok(Err(e)) => format!("{e:#}"),
                    _ => format!("didn't answer within {}s", self.cfg.start_timeout.as_secs()),
                };
                let container = live.container.lock().clone();
                let tail = match container {
                    Some(name) => self.iso.logs_tail(&name, 30).await,
                    None => String::new(),
                };
                let name = live.container.lock().take();
                if let Some(name) = name {
                    self.iso.release(&name).await;
                }
                live.started.lock().take();
                let error = if tail.trim().is_empty() { e.clone() } else { format!("{e}\n\n{}", tail.trim_end()) };
                // Its output's tail is its code's words: masked as its logs are before it's kept
                // (the preview's view, the pull request's comment), or left out when the masks
                // can't be read.
                let error = match self.engine() {
                    Ok(en) => en.mask_text(preview, &error).await.unwrap_or(e),
                    Err(_) => e,
                };
                warn!(%preview, %service, error = %error.lines().next().unwrap_or(""), "preview service failed to start");
                live.phase.send_replace(Phase::Failed(error.clone()));
                if let Ok(en) = self.engine() {
                    en.observed(preview, service, ServiceState::Failed, Some(error.chars().take(4000).collect()), 0).await;
                }
            }
        }
    }

    async fn start_inner(&self, preview: &str, service: &str, live: &Live) -> anyhow::Result<SocketAddr> {
        let stops = live.stops.load(std::sync::atomic::Ordering::SeqCst);
        let engine = self.engine()?;
        let (t, env) = engine.env(preview, service).await?;
        *live.sleep_after.lock() = t.sleep_after;
        let name = container_name(&t.host);
        self.iso.remove(&name).await;
        *live.container.lock() = Some(name.clone());
        let reach = |v: &str| reach_local(&self.iso.reach(v), self.iso.name() == "docker");
        let job = Job {
            kind: JobKind::Preview,
            name: name.clone(),
            image: t.spec.image.clone(),
            run: t.spec.run.clone(),
            env: env.into_iter().map(|(k, v)| (k, reach(&v))).collect(),
            memory_mb: t.spec.memory_mb,
            cpus: t.spec.cpus,
            port: Some(t.spec.port),
            change: Some(preview.to_string()),
            // A service lives while it's visited: on Fargate it stops by itself a little after
            // `sleep_after` without a byte, if this node is gone and can't stop it.
            deadline: t.sleep_after + Duration::from_secs(120),
            // Package caches shared by every preview of the repo: a second preview installs from
            // what the first downloaded.
            cache: Some(cache_name(&t.repo)),
            // The project limits what its previews' code reaches: the preview's own network.
            network: if t.egress == crate::settings::EgressRule::Roll && self.iso.name() == "docker" { Some(preview_network(preview).await?) } else { None },
        };
        // Hosts outside roll it may still reach: through the preview's egress proxy, the one way
        // out of its network (code that ignores the variables still can't get out).
        let mut job = job;
        // No hosts listed (any more): no proxy left on the network with an old list.
        if job.network.is_some() && t.egress_hosts.is_empty() {
            self.drop_egress_proxy(preview).await;
        }
        if let Some(n) = job.network.as_ref().filter(|_| !t.egress_hosts.is_empty()) {
            self.egress_proxy(preview, &n.name, &t.egress_hosts).await?;
            self.trust_intercept_ca(preview, &mut job);
            let proxy = format!("http://{EGRESS_ALIAS}:3128");
            for k in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
                job.env.push((k.into(), proxy.clone()));
            }
            for k in ["NO_PROXY", "no_proxy"] {
                job.env.push((k.into(), "host.docker.internal,localhost,127.0.0.1".into()));
            }
        }
        self.trees.prepare(&*self.iso, &job, &t.repo, &t.revision).await?;
        let addr = self.iso.start(&name, t.spec.port).await?;
        // Stopped (slept, redeployed, removed) while it was starting: don't leave it running.
        if live.stops.load(std::sync::atomic::Ordering::SeqCst) != stops {
            self.iso.release(&name).await;
            anyhow::bail!("stopped while starting");
        }
        loop {
            // A stop (a sleep, a new deploy) while it installs and boots: give up now, not once
            // it answers, so the stop isn't held behind a whole install.
            if live.stops.load(std::sync::atomic::Ordering::SeqCst) != stops {
                self.iso.release(&name).await;
                anyhow::bail!("stopped while starting");
            }
            if !self.iso.running(&name).await {
                anyhow::bail!("the service's command exited");
            }
            if answers(addr).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        info!(%preview, %service, %addr, "preview service running");
        Ok(addr)
    }

    /// An earlier push its preview no longer keeps, by the host it had (under roll's preview
    /// domain): who it was and the current service's URL, for a page saying so.
    pub fn gone_for_host(&self, host: &str) -> Option<crate::store::GonePush> {
        let host = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(h, _)| h);
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if let Some(label) = host.strip_suffix(&self.cfg.domain).and_then(|h| h.strip_suffix('.')) {
            return self.store.gone_push(label).ok().flatten().filter(|_| !label.is_empty() && !label.contains('.'));
        }
        // Under a project's own preview domain: only that project's previews' pushes.
        let (label, base) = host.split_once('.')?;
        if label.is_empty() || !self.domain_bases().contains(base) {
            return None;
        }
        self.store.gone_domain_push(label, base).ok().flatten()
    }

    /// The preview and service a request's host names, if it's a preview host.
    pub fn service_for_host(&self, host: &str) -> Option<(String, String)> {
        let host = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(h, _)| h);
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if let Some(label) = host.strip_suffix(&self.cfg.domain).and_then(|h| h.strip_suffix('.')) {
            if label.contains('.') || label.is_empty() {
                return None;
            }
            return self.store.service_for_host(label).ok().flatten();
        }
        // Under a project's own preview domain (`<label>.preview.acme.dev`): only that
        // project's previews.
        let (label, base) = host.split_once('.')?;
        if label.is_empty() || !self.domain_bases().contains(base) {
            return None;
        }
        self.store.service_for_domain_host(label, base).ok().flatten()
    }

    /// The bases of projects' own preview domains, read again after [`DOMAINS_FOR`].
    fn domain_bases(&self) -> Arc<std::collections::HashSet<String>> {
        let mut d = self.domains.lock();
        if d.0.is_none_or(|at| at.elapsed() > DOMAINS_FOR) {
            if let Ok(bases) = self.store.domain_bases() {
                d.1 = Arc::new(bases.into_iter().collect());
            }
            d.0 = Some(std::time::Instant::now());
        }
        d.1.clone()
    }

    /// The notes left on `path` of the preview's pages, as JSON (the latest 50; for whoever has
    /// the button: link access or a team session, through roll).
    async fn feedback_notes(&self, preview: &str, path: &str) -> Response {
        let here = match self.engine() {
            Ok(e) => e.notes_of(preview, path).await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        ([(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "private, no-store")], axum::Json(here)).into_response()
    }

    /// A link's way in: its token becomes a cookie (`cookie`, for `max_age` seconds) and the
    /// visitor is sent to the same URL without `param`.
    fn enter(&self, req: &Request, param: &str, cookie: &str, value: &str, max_age: i64) -> Response {
        let mut r = self.without(req, param);
        let secure = if self.cfg.scheme == "https" { "; Secure" } else { "" };
        let set = format!("{cookie}={value}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Lax{secure}");
        if let Ok(c) = HeaderValue::from_str(&set) {
            r.headers_mut().insert(header::SET_COOKIE, c);
        }
        r
    }

    /// A redirect to the URL `req` asked for without the query parameter `param`.
    fn without(&self, req: &Request, param: &str) -> Response {
        let query = req.uri().query().unwrap_or("");
        let prefix = format!("{param}=");
        let rest: Vec<&str> = query.split('&').filter(|kv| !kv.starts_with(&prefix) && !kv.is_empty()).collect();
        // Only a path on this host: `//evil.example/` would be another site.
        let path = format!("/{}", req.uri().path().trim_start_matches(['/', '\\']));
        let location = if rest.is_empty() { path } else { format!("{path}?{}", rest.join("&")) };
        let mut r = (StatusCode::FOUND, "").into_response();
        if let Ok(l) = HeaderValue::from_str(&location) {
            r.headers_mut().insert(header::LOCATION, l);
            r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        }
        r
    }

    /// Where a visitor's feedback goes (the preview in roll's dashboard), when they get the button:
    /// they came in through roll ([`ThroughRoll`]: the link's cookie, or a team session), the
    /// preview is a change's, the rules have `feedback` on and roll's dashboard is known. The
    /// note is posted there, by whoever is signed in to roll, as them.
    fn feedback_target(&self, preview: &str, req: &Request) -> Option<String> {
        let web = self.cfg.dashboard.as_deref()?;
        req.extensions().get::<ThroughRoll>()?;
        let row = self.store.row(preview).ok()?;
        row.change_id.as_ref()?;
        self.store.settings(&row.project_id).ok()?.feedback.then(|| format!("{}/dashboard/projects/{}/previews?preview={preview}", web.trim_end_matches('/'), row.project_id))
    }

    /// Link access: a request carries the preview's token in its query (it gets a cookie and is
    /// sent to the same URL without it) or in that cookie. Team access: a session of the
    /// preview's own, from a ticket roll's dashboard gave a person signed in (`visit.rs`); the
    /// link's token lets nobody in (a URL carrying it is sent on without it). Either way a share
    /// link and the CI bypass do; anything else is refused. `None`: let it through.
    async fn gate(&self, preview: &str, service: &str, req: &mut Request) -> Option<Response> {
        let access = match self.store.access(preview) {
            Ok(Some(a)) => a,
            Ok(None) => return None,
            Err(_) => return Some((StatusCode::SERVICE_UNAVAILABLE, "try again").into_response()),
        };
        if !access.required() {
            return None;
        }
        // Automation (end-to-end tests in CI): the project's bypass secret in a header, for every
        // request (no cookie, no redirect).
        let given = req.headers().get(BYPASS_HEADER).and_then(|v| v.to_str().ok());
        if let (Some(want), Some(given)) = (&access.bypass, given)
            && same(&crate::store::bypass_hash(given), want)
        {
            return None;
        }
        let team = access.mode == crate::settings::Access::Team;
        let query = req.uri().query().unwrap_or("").to_string();
        let given = |name: &str| query.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == name).map(|(_, v)| v);
        if team {
            // A ticket from roll's dashboard, coming back: the session it opens.
            if req.uri().path() == crate::visit::PATH {
                return Some(self.land_visit(preview, req));
            }
            // An old link's token opens nothing here and never reaches the app: the same URL
            // without it.
            if given("roll_access").is_some() {
                return Some(self.without(req, "roll_access"));
            }
            // Any of the cookies by that name (a sibling preview can set junk on the parent
            // domain, and the browser sends both): one live session lets them in.
            for t in take_cookies(req, &crate::visit::cookie(preview)) {
                if self.visitor(preview, &t).await {
                    req.extensions_mut().insert(ThroughRoll);
                    return None;
                }
            }
        }
        let token = access.link_token().unwrap_or_default().to_string();
        let cookie = format!("roll_access_{preview}");
        if has_access_cookie(preview, &token, req) {
            req.extensions_mut().insert(ThroughRoll);
            return None;
        }
        // A share link (someone outside the team, until it ends), checked again at every request
        // so a revoked or ended one stops at once.
        let share_cookie = format!("roll_share_{preview}");
        if cookie_value(req, &share_cookie).is_some_and(|t| self.store.share_ends(preview, t).ok().flatten().is_some()) {
            return None;
        }
        if !token.is_empty() && given("roll_access").is_some_and(|v| same(v, &token)) {
            return Some(self.enter(req, "roll_access", &cookie, &token, 2_592_000));
        }
        if let Some(t) = given("roll_share")
            && let Ok(Some(ends)) = self.store.share_ends(preview, t)
        {
            let now = self.store.now_ms().unwrap_or(ends);
            // Who followed it isn't known (nobody signed in): that it was, once a day.
            let _ = self.store.share_opened(preview, t, now.div_euclid(86_400_000), now);
            let left = ((ends - now) / 1000).max(1);
            return Some(self.enter(req, "roll_share", &share_cookie, t, left));
        }
        if team {
            return Some(self.sign_in(preview, req));
        }
        let through = match (&self.cfg.dashboard, self.store.row(preview)) {
            (Some(web), Ok(row)) => Some(format!(
                "{}/dashboard/projects/{}/previews?preview={preview}&go={}",
                web.trim_end_matches('/'),
                row.project_id,
                service
            )),
            _ => None,
        };
        Some(private_page(through.as_deref()))
    }

    /// A visitor with no session of a team-access preview's: a page load goes to roll's
    /// dashboard, which signs them in and sends them back with a ticket; anything else is a 401
    /// that says where.
    fn sign_in(&self, preview: &str, req: &Request) -> Response {
        let Some(web) = self.cfg.dashboard.as_deref() else {
            return roll_page(StatusCode::UNAUTHORIZED, "This preview is private", "<p>It lets in people signed in to roll, and this node doesn't know where roll is.</p>");
        };
        let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
        let back = format!("{}://{host}{}", self.cfg.scheme, req.uri().path_and_query().map_or("/", |p| p.as_str()));
        let url = format!("{}/dashboard/previews/visit?preview={}&to={}", web.trim_end_matches('/'), crate::visit::encode(preview), crate::visit::encode(&back));
        let get = matches!(*req.method(), axum::http::Method::GET | axum::http::Method::HEAD);
        if get
            && is_page_load(req)
            && let Ok(l) = HeaderValue::from_str(&url)
        {
            let mut r = StatusCode::FOUND.into_response();
            r.headers_mut().insert(header::LOCATION, l);
            r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return r;
        }
        roll_page(StatusCode::UNAUTHORIZED, "This preview is private", &format!("<p><a href=\"{}\">Sign in to roll</a> to open it: it lets in people who can open its project.</p>", escape(&url)))
    }

    /// `/__roll/visit?ticket=…&to=…` on the preview's own host: the ticket traded for a session
    /// cookie (host-only, 12 hours), then on to `to` on the same site.
    fn land_visit(&self, preview: &str, req: &Request) -> Response {
        let mut ticket = None;
        let mut to = "/".to_string();
        for pair in req.uri().query().unwrap_or("").split('&') {
            if let Some(v) = pair.strip_prefix("ticket=") {
                ticket = Some(percent_decode(v));
            } else if let Some(v) = pair.strip_prefix("to=") {
                to = crate::visit::local_path(&percent_decode(v));
            }
        }
        let expired = || roll_page(StatusCode::UNAUTHORIZED, "This sign-in link has expired", "<p>Open the page again: roll signs you in and sends you back.</p>");
        // Where it goes, checked before the ticket is used.
        let Ok(location) = HeaderValue::from_str(&to) else { return expired() };
        let Ok(now) = self.store.now_ms() else { return (StatusCode::SERVICE_UNAVAILABLE, "try again").into_response() };
        let Some(ticket) = ticket else { return expired() };
        let session = format!("rls_{}", crate::store::new_token());
        match self.store.open_visit_session(&crate::store::bypass_hash(&ticket), &crate::store::bypass_hash(&session), preview, now + crate::visit::SESSION_S * 1000, now) {
            Ok(Some(_)) => {}
            Ok(None) => return expired(),
            Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, "try again").into_response(),
        }
        let secure = if self.cfg.scheme == "https" { "; Secure" } else { "" };
        let set = format!("{}={session}; Path=/; Max-Age={}; HttpOnly; SameSite=Lax{secure}", crate::visit::cookie(preview), crate::visit::SESSION_S);
        let mut r = StatusCode::FOUND.into_response();
        let Ok(c) = HeaderValue::from_str(&set) else { return expired() };
        r.headers_mut().insert(header::LOCATION, location);
        r.headers_mut().insert(header::SET_COOKIE, c);
        r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        r
    }

    /// Whether `token` is a live session in `preview` whose person may still open the project
    /// and whose roll session still signs them in, asked again (off the async workers: it reads
    /// roll's accounts) once it was last asked `visitor_recheck` ago; a session that may not
    /// ends now.
    async fn visitor(&self, preview: &str, token: &str) -> bool {
        let hash = crate::store::bypass_hash(token);
        let (Ok(Some(s)), Ok(now)) = (self.store.visit_session(&hash, preview), self.store.now_ms()) else { return false };
        if s.expires_at <= now {
            return false;
        }
        if s.checked_at <= now - self.cfg.visitor_recheck.as_millis() as i64 {
            let Ok(engine) = self.engine() else { return false };
            let user = s.user.clone();
            let may = tokio::task::spawn_blocking(move || engine.may_open(&s.user, &s.roll_session, &s.project)).await.unwrap_or(false);
            if !may {
                let _ = self.store.end_visit_session(&hash);
                return false;
            }
            let _ = self.store.visit_checked(&hash, preview, &user, now);
        }
        true
    }

    /// [`Runtime::proxy_to`], kept in the preview's request log.
    async fn proxy(self: &Arc<Self>, preview: &str, service: &str, req: Request, feedback: bool) -> Response {
        let started = std::time::Instant::now();
        let (method, path) = (req.method().to_string(), req.uri().path().chars().take(300).collect::<String>());
        let resp = self.proxy_to(preview, service, req, feedback).await;
        // Not after its services went (removed, or moved to another node meanwhile).
        if !self.live.lock().keys().any(|(p, _)| p == preview) {
            return resp;
        }
        let line = crate::RequestLine { at: self.store.now_ms().unwrap_or(0), service: service.into(), method, path, status: resp.status().as_u16(), ms: started.elapsed().as_millis() as u64 };
        let mut all = self.requests.lock();
        let kept = all.entry(preview.to_string()).or_default();
        kept.push_back(line);
        if kept.len() > REQUESTS_KEPT {
            kept.pop_front();
        }
        resp
    }

    async fn proxy_to(self: &Arc<Self>, preview: &str, service: &str, mut req: Request, feedback: bool) -> Response {
        let mut retried = false;
        let addr = loop {
            let addr = match self.ready(preview, service, self.cfg.wait).await {
                Ok(a) => a,
                // Stopped while it woke (a sleep or a deploy landing then) and the preview is still
                // there: the waking page, whose refresh starts what runs now.
                Err(u) if u.overtaken() && self.store.row(preview).is_ok_and(|r| r.desired == crate::Desired::Active && r.state != crate::State::AwaitingApproval) => {
                    return Unavailable::Starting.into_response();
                }
                Err(u) => return u.into_response(),
            };
            // A container that died underneath us: start it again, once.
            if retried || answers(addr).await {
                break addr;
            }
            retried = true;
            // Only the container it reached: if that one was already replaced (a deploy stopped
            // it, a new one is starting), stopping now would kill the new one.
            let live = self.live(preview, service);
            if matches!(*live.phase.borrow(), Phase::Running { addr: a } if a == addr) {
                self.stop(preview, service, &live, Some("the container stopped answering".into())).await;
            }
        };
        let pq = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
        let Ok(uri) = format!("http://{addr}{pq}").parse::<Uri>() else {
            return (StatusCode::BAD_REQUEST, "bad request").into_response();
        };
        let upgrade = is_upgrade(req.headers());
        let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut req));
        let host = req.headers().get(header::HOST).cloned();
        *req.uri_mut() = uri;
        *req.version_mut() = axum::http::Version::HTTP_11;
        strip_hop_by_hop(req.headers_mut(), upgrade);
        // The service never sees the project's secret.
        req.headers_mut().remove(BYPASS_HEADER);
        if let Some(h) = host {
            req.headers_mut().insert(HeaderName::from_static("x-forwarded-host"), h);
        }
        req.headers_mut().insert(HeaderName::from_static("x-forwarded-proto"), HeaderValue::from_str(&self.cfg.scheme).unwrap_or(HeaderValue::from_static("http")));
        // A page that gets the feedback button is asked for whole and uncompressed.
        if feedback {
            req.headers_mut().insert(header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        }
        let mut resp = match self.client.request(req).await {
            Ok(r) => r,
            Err(e) => return (StatusCode::BAD_GATEWAY, format!("the preview didn't answer: {e}")).into_response(),
        };
        if resp.status() == StatusCode::SWITCHING_PROTOCOLS
            && let Some(client_upgrade) = client_upgrade
        {
            let upstream = hyper::upgrade::on(&mut resp);
            tokio::spawn(async move {
                if let (Ok(a), Ok(b)) = tokio::join!(client_upgrade, upstream) {
                    let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(a), &mut TokioIo::new(b)).await;
                }
            });
        } else {
            strip_hop_by_hop(resp.headers_mut(), false);
        }
        let resp = resp.map(Body::new);
        if feedback { add_feedback(resp) } else { resp }
    }

    #[cfg(test)]
    pub(crate) fn pretend_running(&self, preview: &str, service: &str, addr: SocketAddr) {
        let live = self.live(preview, service);
        *live.started.lock() = Some(Instant::now());
        live.phase.send_replace(Phase::Running { addr });
    }
}

/// A request [`Runtime::gate`] let in as someone who came through roll: link access's cookie, or
/// a live session of a team-access preview (a person signed in to roll). Not a share link, the CI
/// bypass or a public preview: those get no feedback button.
#[derive(Clone, Copy)]
struct ThroughRoll;

/// Where a preview's pages load the feedback button from (its own host, so `script-src 'self'`
/// allows it), and the tag that loads it.
pub const FEEDBACK_PATH: &str = "/__roll/feedback.js";
/// The feedback already left on a page (`?path=`), for the button to list.
pub const FEEDBACK_NOTES_PATH: &str = "/__roll/feedback.json";
pub(crate) const FEEDBACK_TAG: &[u8] = b"<script src=\"/__roll/feedback.js\" defer></script>";
/// Adds the feedback button's tag to a page as it streams: before the first `</body>` (matched
/// across chunk boundaries, in any case), or at the end of a page without one. Chunks pass on as
/// they come, so streamed rendering stays streamed and memory stays one chunk.
struct Tagger {
    /// The last bytes of what's been read, held back in case `</body>` starts in them.
    held: Vec<u8>,
    done: bool,
}

impl Tagger {
    const CLOSE: &'static [u8] = b"</body>";

    fn new() -> Tagger {
        Tagger { held: Vec::new(), done: false }
    }

    /// What to send on after `chunk`.
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.done {
            return chunk.to_vec();
        }
        let mut buf = std::mem::take(&mut self.held);
        buf.extend_from_slice(chunk);
        if let Some(at) = buf.windows(Self::CLOSE.len()).position(|w| w.eq_ignore_ascii_case(Self::CLOSE)) {
            self.done = true;
            buf.splice(at..at, FEEDBACK_TAG.iter().copied());
            return buf;
        }
        let keep = (Self::CLOSE.len() - 1).min(buf.len());
        self.held = buf.split_off(buf.len() - keep);
        buf
    }

    /// What's left at the end of the page.
    fn finish(&mut self) -> Vec<u8> {
        let mut rest = std::mem::take(&mut self.held);
        if !self.done {
            self.done = true;
            rest.extend_from_slice(FEEDBACK_TAG);
        }
        rest
    }
}

/// An HTML page from the service with the feedback button added as it streams; anything
/// compressed or not HTML passes through as it came.
fn add_feedback(resp: Response) -> Response {
    let html = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.trim_start().to_ascii_lowercase().starts_with("text/html"));
    let plain = resp.headers().get(header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()).is_none_or(|e| e.eq_ignore_ascii_case("identity"));
    if resp.status() != StatusCode::OK || !html || !plain {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    // This visitor's page now (a shared cache mustn't hand it to anyone else), and not the
    // service's bytes any more.
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::ETAG);
    parts.headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    let stream = futures::stream::unfold((body.into_data_stream(), Tagger::new(), false), |(mut s, mut t, ended)| async move {
        if ended {
            return None;
        }
        match s.next().await {
            Some(Ok(c)) => Some((Ok::<_, axum::Error>(Bytes::from(t.push(&c))), (s, t, false))),
            Some(Err(e)) => Some((Err(e), (s, t, true))),
            None => Some((Ok(Bytes::from(t.finish())), (s, t, true))),
        }
    });
    Response::from_parts(parts, Body::from_stream(stream))
}

/// The feedback button's script, sending notes to `target` (the preview in roll's dashboard).
fn feedback_script(target: &str) -> Response {
    let js = include_str!("feedback.js").replace("__ROLL_TARGET__", &serde_json::to_string(target).unwrap_or_else(|_| "\"\"".into()));
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8"), (header::CACHE_CONTROL, "private, max-age=300")], js).into_response()
}

/// The biggest screenshot kept (a 1280×800 PNG of an ordinary page is 10–200 KiB).
pub const SCREENSHOT_MAX: usize = 2 << 20;
/// A screenshot run's report lines (speed, checks and words of the first page; then the other
/// pages'), each at most this, and everything it writes: every PNG it can take, in base64, and
/// those lines.
const SHOT_REPORT_MAX: usize = 4 << 20;
const SHOT_OUTPUT_MAX: usize = (crate::settings::MAX_PAGES + 2) * SCREENSHOT_MAX * 4 / 3 + 3 * SHOT_REPORT_MAX + crate::store::CROPS_PER_RUN * crate::store::CROP_MAX * 4 / 3 + (64 << 10);

/// The header automation sends a project's bypass secret in (docs/previews.md, "Safety").
pub const BYPASS_HEADER: &str = "x-roll-preview-bypass";

/// A private preview's answer to a request without its link: where to get in, if roll knows.
/// `%xx` and `+` in a query value, as a browser's `URLSearchParams` writes them.
fn percent_decode(v: &str) -> String {
    let b = v.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => match (b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The value of cookie `name` on `req`.
fn cookie_value<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers().get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).filter_map(|kv| kv.trim().split_once('=')).find(|(k, _)| *k == name).map(|(_, v)| v)
}

/// Whether `req` carries the preview's link access cookie.
fn has_access_cookie(preview: &str, token: &str, req: &Request) -> bool {
    !token.is_empty() && cookie_value(req, &format!("roll_access_{preview}")).is_some_and(|v| same(v, token))
}

/// Every cookie `name` taken off `req` (a browser sends one per domain and path that set it):
/// their values, and the rest of the cookies kept.
fn take_cookies(req: &mut Request, name: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut kept = Vec::new();
    for v in req.headers().get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()) {
        for c in v.split(';').map(str::trim).filter(|c| !c.is_empty()) {
            match c.split_once('=') {
                Some((k, v)) if k == name => found.push(v.to_string()),
                _ => kept.push(c.to_string()),
            }
        }
    }
    if found.is_empty() {
        return found;
    }
    req.headers_mut().remove(header::COOKIE);
    if !kept.is_empty()
        && let Ok(v) = HeaderValue::from_str(&kept.join("; "))
    {
        req.headers_mut().insert(header::COOKIE, v);
    }
    found
}

/// A page load: a browser says so (`Sec-Fetch-Dest: document`, so not a fetch, a fragment or an
/// iframe); without that header, one that asks for HTML.
fn is_page_load(req: &Request) -> bool {
    let header_of = |h: &str| req.headers().get(h).and_then(|v| v.to_str().ok()).map(str::to_ascii_lowercase);
    match header_of("sec-fetch-dest") {
        Some(dest) => dest == "document",
        None => header_of("accept").is_some_and(|a| a.contains("text/html")),
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn private_page(through: Option<&str>) -> Response {
    let action = match through {
        Some(u) => format!("<p><a href=\"{}\">Open it through roll</a>: sign in, and you're sent on with its link.</p>", escape(u)),
        None => "<p>Open it from its link in roll.</p>".into(),
    };
    roll_page(StatusCode::UNAUTHORIZED, "This preview is private", &action)
}

/// roll's own answer on a preview's host: a title and a line (`body` is HTML), dark first.
fn roll_page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{title} · roll</title><style>\
         :root{{color-scheme:dark light;--bg:#000;--fg:#ededed;--muted:#8f8f8f;--accent:#4c8dff}}\
         @media (prefers-color-scheme:light){{:root{{--bg:#fff;--fg:#111;--muted:#666}}}}\
         body{{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--fg);\
         font:14px/1.5 ui-sans-serif,system-ui,sans-serif}}main{{max-width:520px;padding:24px}}\
         h1{{font-size:20px;font-weight:600;letter-spacing:-0.04em;margin:0 0 8px}}p{{color:var(--muted);margin:0}}\
         a{{color:var(--accent)}}</style></head><body><main><h1>{title}</h1>{body}</main></body></html>"
    );
    (status, [(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], html).into_response()
}

/// Equal, in time that doesn't depend on where they differ.
pub(crate) fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// On Docker, a container reaches the host as `host.docker.internal`: rewrite loopback hosts
/// in URLs and `host:port` lists (`KAFKA_BOOTSTRAP_SERVERS`) too.
fn reach_local(v: &str, docker: bool) -> String {
    if !docker {
        return v.to_string();
    }
    // A bare host (`SMTP_HOST=127.0.0.1`).
    if ["127.0.0.1", "localhost", "0.0.0.0", "[::1]"].contains(&v) {
        return "host.docker.internal".into();
    }
    let mut out = v.to_string();
    for local in ["127.0.0.1", "localhost", "0.0.0.0", "[::1]"] {
        for (pre, post) in [("//", ":"), ("@", ":"), ("", ":")] {
            let needle = format!("{pre}{local}{post}");
            if pre.is_empty() {
                // A bare `host:port` (at the start, or after a comma).
                let parts: Vec<String> = out
                    .split(',')
                    .map(|p| match p.trim().strip_prefix(&format!("{local}:")) {
                        Some(rest) if rest.bytes().all(|b| b.is_ascii_digit()) && !rest.is_empty() => format!("host.docker.internal:{rest}"),
                        _ => p.to_string(),
                    })
                    .collect();
                out = parts.join(",");
            } else {
                out = out.replace(&needle, &format!("{pre}host.docker.internal{post}"));
            }
        }
    }
    out
}

enum Unavailable {
    NotFound,
    Starting,
    Failed(String),
}

/// A start that a stop, a newer deploy or a lost approval overtook: not the service failing.
fn superseded(e: &str) -> bool {
    e.starts_with("stopped while starting") || e.starts_with("the change moved on") || e.contains("isn't approved")
}

impl Unavailable {
    /// Stopped before it was up, by something other than the service.
    fn overtaken(&self) -> bool {
        match self {
            Unavailable::NotFound => true,
            Unavailable::Failed(e) => superseded(e),
            Unavailable::Starting => false,
        }
    }
}

impl IntoResponse for Unavailable {
    fn into_response(self) -> Response {
        match self {
            Unavailable::NotFound => page(StatusCode::NOT_FOUND, "No preview here", "This preview was deleted, or it is waiting for approval.", false),
            Unavailable::Starting => {
                let mut r = page(StatusCode::SERVICE_UNAVAILABLE, "Waking the preview…", "This page refreshes when it's ready.", true);
                r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
                r
            }
            // A visitor sees why, never its output (that's in its logs, for who can read them).
            Unavailable::Failed(e) => page(StatusCode::BAD_GATEWAY, "The preview didn't start", e.lines().next().unwrap_or(""), false),
        }
    }
}

#[async_trait]
impl Apps for Runtime {
    async fn deploy(&self, t: &Target) -> anyhow::Result<Option<String>> {
        // Whatever runs serves an older revision (or older URLs): the next request starts the
        // new one.
        let live = self.live(&t.preview, &t.service);
        *live.sleep_after.lock() = t.sleep_after;
        let running = !matches!(*live.phase.borrow(), Phase::Stopped);
        if running {
            self.stop(&t.preview, &t.service, &live, None).await;
        }
        Ok(None)
    }

    async fn wake(&self, preview: &str, service: &str) -> anyhow::Result<()> {
        let me = self.me.upgrade().ok_or_else(|| anyhow::anyhow!("the preview runtime is stopping"))?;
        match me.ready(preview, service, self.cfg.start_timeout).await {
            Ok(_) | Err(Unavailable::Starting) => Ok(()),
            Err(Unavailable::Failed(e)) => Err(anyhow::anyhow!(e)),
            Err(Unavailable::NotFound) => Err(anyhow::anyhow!("it stopped while starting")),
        }
    }

    async fn sleep(&self, preview: &str, service: &str) {
        let live = self.live.lock().get(&(preview.to_string(), service.to_string())).cloned();
        if let Some(live) = live {
            self.stop(preview, service, &live, None).await;
        }
    }

    fn requests(&self, preview: &str) -> Vec<crate::RequestLine> {
        self.requests.lock().get(preview).map(|k| k.iter().rev().cloned().collect()).unwrap_or_default()
    }

    async fn remove(&self, preview: &str, service: &str) -> anyhow::Result<()> {
        let key = (preview.to_string(), service.to_string());
        // Its request log goes with its last service.
        if !self.live.lock().keys().any(|(p, s)| p == preview && s != service) {
            self.requests.lock().remove(preview);
        }
        let live = self.live.lock().get(&key).cloned();
        if let Some(live) = live {
            self.stop(preview, service, &live, None).await;
            self.forget(&key, live);
        }
        if let Ok(Some(host)) = self.store.service_host(preview, service) {
            // Whichever node started it.
            self.iso.remove(&container_name(&host)).await;
        }
        // The preview's own network goes with its last container (refused while one's attached),
        // its egress proxy first when no service of the preview is left.
        if self.iso.name() == "docker" {
            let others = self.live.lock().keys().any(|(p, s)| p == preview && s != service);
            if !others {
                code_preview::docker::remove(&egress_name(preview)).await;
                self.egress_making.lock().remove(preview);
            }
            let _ = code_preview::docker::docker(&["network", "rm", &network_name(preview)]).await;
        }
        // The repo's cache goes with its last preview service.
        if let Ok(Some(svc)) = self.store.services(preview).map(|v| v.into_iter().find(|s| s.name == service))
            && !self.store.repo_has_other_services(&svc.repo, preview).unwrap_or(true)
        {
            self.iso.drop_cache(&cache_name(&svc.repo)).await;
        }
        Ok(())
    }

    async fn logs(&self, preview: &str, service: &str, lines: usize) -> Vec<String> {
        let Ok(Some(host)) = self.store.services(preview).map(|v| v.into_iter().find(|s| s.name == service).map(|s| s.host)) else { return Vec::new() };
        self.iso.logs_tail(&container_name(&host), lines.clamp(1, 2000)).await.lines().map(str::to_string).collect()
    }

    async fn follow(&self, preview: &str, service: &str, lines: usize) -> Option<tokio::sync::mpsc::Receiver<(bool, String)>> {
        let host = self.store.services(preview).ok()?.into_iter().find(|s| s.name == service)?.host;
        self.iso.follow(&container_name(&host), lines.clamp(1, 2000)).await
    }

    /// `docker cp <container>:<path> -`, read as it comes; the process goes when the stream does.
    async fn read_files(&self, preview: &str, service: &str, path: &str) -> anyhow::Result<Option<crate::ports::FileStream>> {
        let me = self.me.upgrade().ok_or_else(|| anyhow::anyhow!("the preview runtime is stopping"))?;
        let name = me.shell_container(preview, service).await?;
        let mut child = tokio::process::Command::new("docker")
            .args(["cp", &format!("{name}:{path}"), "-"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let out = child.stdout.take().ok_or_else(|| anyhow::anyhow!("docker cp gave no output"))?;
        self.hold(preview, service);
        Ok(Some(Box::new(ChildOut { _child: child, out })))
    }

    /// A relay in the service's network namespace (`--network container:<it>`, as screenshots
    /// join it): busybox's `nc` to `127.0.0.1:<port>` there, its stdio the connection. Removed
    /// when the connection is dropped.
    async fn forward(&self, preview: &str, service: &str, port: u16) -> anyhow::Result<Option<Box<dyn crate::ports::ShellIo>>> {
        let me = self.me.upgrade().ok_or_else(|| anyhow::anyhow!("the preview runtime is stopping"))?;
        let name = me.shell_container(preview, service).await?;
        let relay = format!("roll-previews-fwd-{}", &crate::store::new_token()[..12]);
        // Labelled as the runtime's other helpers: a crash's leftovers go with them.
        let mut args: Vec<String> = vec!["run".into(), "--rm".into(), "-i".into(), "--name".into(), relay.clone(), "--label".into(), format!("{}=preview", code_preview::docker::LABEL)];
        if let Some(l) = code_preview::docker::home_label() {
            args.extend(["--label".into(), l]);
        }
        args.extend(["--network", &format!("container:{name}"), "--memory", "32m", "--cpus", "0.25", "--pids-limit", "16", "--cap-drop", "ALL", "--security-opt", "no-new-privileges", FORWARD_IMAGE, "nc", "127.0.0.1", &port.to_string()].map(String::from));
        let mut child = tokio::process::Command::new("docker")
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let (stdin, stdout) = (child.stdin.take(), child.stdout.take());
        let (Some(stdin), Some(stdout)) = (stdin, stdout) else { anyhow::bail!("the relay took no input") };
        self.hold(preview, service);
        Ok(Some(Box::new(Relay { _child: child, name: relay, stdin, stdout })))
    }

    /// `docker cp - <container>:<dir>` with the tar on its stdin.
    async fn write_files(&self, preview: &str, service: &str, dir: &str, tar: bytes::Bytes) -> anyhow::Result<bool> {
        use tokio::io::AsyncWriteExt;
        let me = self.me.upgrade().ok_or_else(|| anyhow::anyhow!("the preview runtime is stopping"))?;
        let name = me.shell_container(preview, service).await?;
        let mut child = tokio::process::Command::new("docker")
            .args(["cp", "-", &format!("{name}:{dir}")])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("docker cp took no input"))?;
        stdin.write_all(&tar).await?;
        drop(stdin);
        let out = tokio::time::timeout(Duration::from_secs(120), child.wait_with_output()).await.map_err(|_| anyhow::anyhow!("writing the files took over 2 minutes"))??;
        self.hold(preview, service);
        if !out.status.success() {
            let said = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!("{}", said.trim().lines().last().unwrap_or("docker cp failed").chars().take(300).collect::<String>());
        }
        Ok(true)
    }

    async fn drain_from(&self, preview: &str, service: &str, since: &str) -> Option<tokio::sync::mpsc::Receiver<(bool, String)>> {
        let host = self.store.services(preview).ok()?.into_iter().find(|s| s.name == service)?.host;
        self.iso.follow_since(&container_name(&host), since).await
    }

    /// Its running container's use, from Docker (a sleeping or frozen service has none).
    async fn metrics(&self, preview: &str, service: &str) -> Option<(f64, u64)> {
        if self.iso.name() != "docker" {
            return None;
        }
        let live = self.live.lock().get(&(preview.to_string(), service.to_string())).cloned()?;
        if !matches!(*live.phase.borrow(), Phase::Running { .. }) {
            return None;
        }
        let name = live.container.lock().clone()?;
        let out = code_preview::docker::docker(&["stats", "--no-stream", "--format", "{{.CPUPerc}}|{{.MemUsage}}", &name]).await.ok()?;
        crate::parse_docker_stats(&out)
    }

    /// The service's page as a browser first shows it (a PNG): the browser runs in the service
    /// container's own network (it reaches nothing the service can't), bounded in memory, CPU and
    /// time, and goes when done.
    async fn probe(&self, preview: &str, service: &str, path: &str) -> anyhow::Result<Option<(u16, String)>> {
        let me = self.me.upgrade().ok_or_else(|| anyhow::anyhow!("the preview runtime is stopping"))?;
        // Never long in a converge slot: a service still starting (installing, booting) or whose
        // start a stop or a push overtook isn't failing; the engine asks again later.
        let later = |why: String| -> anyhow::Result<Option<(u16, String)>> { Err(crate::ports::StillDeploying(why).into()) };
        let addr = match me.ready(preview, service, Duration::from_secs(5)).await {
            Ok(a) => a,
            Err(Unavailable::Starting) => return later("starting".into()),
            Err(Unavailable::Failed(e)) if superseded(&e) => return later(e),
            Err(Unavailable::NotFound) => return later("it was stopped while starting".into()),
            Err(Unavailable::Failed(e)) => anyhow::bail!("the service didn't start: {e}"),
        };
        let req = axum::http::Request::get(format!("http://{addr}{path}")).header(header::USER_AGENT, "roll-previews-health").body(Body::empty())?;
        let resp = tokio::time::timeout(Duration::from_secs(10), self.client.request(req)).await.map_err(|_| anyhow::anyhow!("no answer in 10 s"))??;
        let status = resp.status().as_u16();
        // The first line of what it said: at most 4 KiB, read for at most 2 s.
        let mut stream = Body::new(resp.into_body()).into_data_stream();
        let mut said = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while said.len() < 4096
                && let Some(Ok(chunk)) = stream.next().await
            {
                said.extend_from_slice(&chunk);
            }
        })
        .await;
        // Cut wide here: the engine masks it whole and then cuts it to 200 (a value isn't left half shown).
        let first = String::from_utf8_lossy(&said[..said.len().min(4096)]).lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().filter(|c| !c.is_control()).take(2000).collect();
        Ok(Some((status, first)))
    }

    async fn screenshot(&self, preview: &str, service: &str, opts: &crate::ports::ShotOptions) -> anyhow::Result<Option<crate::ports::Shot>> {
        if self.iso.name() != "docker" {
            return Ok(None);
        }
        let Some(host) = self.store.services(preview)?.into_iter().find(|s| s.name == service).map(|s| s.host) else { return Ok(None) };
        let me = self.me.upgrade().ok_or_else(|| anyhow::anyhow!("the preview runtime is stopping"))?;
        match me.ready(preview, service, self.cfg.wait).await {
            Ok(_) => {}
            Err(Unavailable::Failed(e)) => anyhow::bail!("the service didn't start: {e}"),
            Err(_) => anyhow::bail!("the service didn't start in time"),
        }
        let name = container_name(&host);
        // The service's own port, as its container says (`sh.roll.code.port`: with `egress: roll`
        // it's on a network of its own and publishes nothing), else the one the runtime published
        // (a container made before the label), never whatever else its image exposes.
        let said = code_preview::docker::docker(&["inspect", "-f", &format!("{{{{index .Config.Labels \"{}.port\"}}}} {{{{range $p, $_ := .HostConfig.PortBindings}}}}{{{{$p}}}} {{{{end}}}}", code_preview::docker::LABEL), &name]).await?;
        let port = said.split_whitespace().find_map(|p| p.split('/').next().and_then(|p| p.parse::<u16>().ok())).ok_or_else(|| anyhow::anyhow!("the service says no port"))?;
        self.screenshot_in(&name, port, opts).await.map(Some)
    }

    async fn seed(&self, job: &crate::ports::SeedJob) -> anyhow::Result<(i32, String)> {
        self.seed_job(job).await
    }

    fn status(&self, preview: &str, service: &str) -> Option<ServiceState> {
        self.service_status(preview, service)
    }

    async fn egress_refused(&self, preview: &str) -> Vec<String> {
        if self.iso.name() != "docker" {
            return Vec::new();
        }
        let tail = code_preview::docker::logs_tail(&egress_name(preview), 200).await;
        let mut out: Vec<String> = Vec::new();
        for h in tail.lines().filter_map(|l| l.strip_prefix("refused ")).map(|h| h.trim().chars().take(262).collect::<String>()) {
            if !h.is_empty() && !out.contains(&h) {
                out.push(h);
            }
        }
        out
    }

    fn warm(&self, preview: &str, service: &str) -> bool {
        self.live.lock().get(&(preview.to_string(), service.to_string())).is_some_and(|l| matches!(*l.phase.borrow(), Phase::Frozen { .. }))
    }
}

impl Runtime {
    /// A service's output over the last `minutes` as `(ms, from stderr, text)`, oldest first, at
    /// most its last `lines` (each cut at 16 KiB, at most 4 MiB a stream, read for at most 10 s):
    /// what its errors are read from. Only Docker stamps its lines: on another backend nothing
    /// (an old crash in its tail would read as new).
    pub async fn timed_lines(&self, preview: &str, service: &str, minutes: i64, lines: usize) -> Vec<(i64, bool, String)> {
        let Ok(Some(host)) = self.store.services(preview).map(|v| v.into_iter().find(|s| s.name == service).map(|s| s.host)) else { return Vec::new() };
        if self.iso.name() != "docker" {
            return Vec::new();
        }
        let (since, tail, name) = (format!("{minutes}m"), lines.to_string(), container_name(&host));
        let args = ["logs", "--timestamps", "--since", &since, "--tail", &tail, &name];
        let spawned = tokio::process::Command::new("docker").args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true).spawn();
        let Ok(mut child) = spawned else { return Vec::new() };
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else { return Vec::new() };
        let read = async { tokio::join!(read_timed(stdout, false, TIMED_KEEP_MAX), read_timed(stderr, true, TIMED_KEEP_MAX)) };
        // Past its time `child` is dropped, which kills it.
        let Ok((out, err)) = tokio::time::timeout(TIMED_READ_FOR, read).await else { return Vec::new() };
        merge_timed(out, err, lines)
    }

    /// A screenshot run in container `name`'s own network (its page at `127.0.0.1:<port>`, so the
    /// page's code reaches only what the service does): this runtime's containers, and roll
    /// apps' instances (rolld's `RollApps`, through roll apps' `reachable`). The container must
    /// be running and kept so for the run.
    pub async fn screenshot_in(&self, name: &str, port: u16, opts: &crate::ports::ShotOptions) -> anyhow::Result<crate::ports::Shot> {
        use base64::Engine as _;
        anyhow::ensure!(self.iso.name() == "docker", "screenshots need Docker");
        // The browser image once, in the background task screenshots run in (a cold pull isn't
        // bounded by the screenshot's 90 s).
        if code_preview::docker::docker(&["image", "inspect", "--format", "{{.Id}}", &self.cfg.screenshot_image]).await.is_err() {
            code_preview::docker::docker(&["pull", "--quiet", &self.cfg.screenshot_image]).await?;
        }
        let shot = format!("roll-previews-shot-{}", name.trim_start_matches("roll-previews-"));
        let mut args: Vec<String> = vec!["run".into(), "--rm".into(), "-i".into(), "--name".into(), shot.clone(), "--network".into(), format!("container:{name}")];
        // Cleaned up with the runtime's own containers after a crash.
        args.extend(["--label".into(), format!("{}=preview", code_preview::docker::LABEL)]);
        if let Some(l) = code_preview::docker::home_label() {
            args.extend(["--label".into(), l]);
        }
        // The page is the preview's code: no capabilities, no new privileges, nothing written but /tmp.
        args.extend(["--cap-drop".into(), "ALL".into(), "--security-opt".into(), "no-new-privileges".into(), "--read-only".into(), "--tmpfs".into(), "/tmp:rw,size=256m".into()]);
        args.extend(code_preview::docker::limits(1024, 1.0));
        args.extend(["--env".into(), format!("URL=http://127.0.0.1:{port}/"), "--env".into(), "OUT=/tmp/s.png".into(), "--env".into(), "HOME=/tmp".into()]);
        args.extend(["--env".into(), format!("ROLL_SHOT_JS={SHOT_JS}"), "--env".into(), "AXE=/tmp/axe.js".into()]);
        let pages: Vec<&String> = opts
            .pages
            .iter()
            .filter(|p| crate::settings::page_path_ok(p) || crate::settings::width_entry(p).is_some() || crate::settings::state_entry(p).is_some())
            .take(crate::settings::MAX_PAGES)
            .collect();
        args.extend(["--env".into(), format!("PAGES={}", serde_json::to_string(&pages)?)]);
        let hide: Vec<&String> = opts.hide.iter().filter(|s| crate::settings::selector_ok(s)).take(crate::settings::MAX_HIDE).collect();
        args.extend(["--env".into(), format!("HIDE={}", serde_json::to_string(&hide)?)]);
        if let Some(path) = opts.openapi.as_deref().filter(|p| crate::settings::page_path_ok(p)) {
            args.extend(["--env".into(), format!("OPENAPI={path}")]);
        }
        // Each state's page and steps, by name (`shot.js` reaches them before the capture).
        let states: std::collections::BTreeMap<&str, serde_json::Value> =
            opts.states
                .iter()
                .filter(|s| crate::settings::state_name_ok(&s.name) && crate::settings::page_path_ok(&s.page) && s.steps.len() <= crate::settings::MAX_STEPS && s.steps.iter().all(crate::settings::step_ok))
                .map(|s| (s.name.as_str(), serde_json::json!({ "page": s.page, "steps": s.steps })))
                .collect();
        if !states.is_empty() {
            args.extend(["--env".into(), format!("STATES={}", serde_json::to_string(&states)?)]);
        }
        let endpoints: Vec<&String> = opts.endpoints.iter().filter(|p| crate::settings::page_path_ok(p)).take(crate::settings::MAX_ENDPOINTS).collect();
        if !endpoints.is_empty() {
            args.extend(["--env".into(), format!("ENDPOINTS={}", serde_json::to_string(&endpoints)?)]);
        }
        // The notes' elements to crop, checked again (they come from the preview's code).
        let crops: Vec<serde_json::Value> = opts
            .crops
            .iter()
            .filter(|c| crate::settings::page_path_ok(&c.path) && crate::settings::selector_ok(&c.selector) && !c.note.is_empty() && c.note.len() <= 64)
            .take(crate::store::CROPS_PER_RUN)
            .map(|c| serde_json::json!({ "note": c.note, "path": c.path, "selector": c.selector }))
            .collect();
        if !crops.is_empty() {
            args.extend(["--env".into(), format!("CROPS={}", serde_json::to_string(&crops)?)]);
        }
        // The PNG on the first line, the timings (if the command wrote them) on the second, the
        // phone-width PNG (if it took one) on the third, then the other pages: what each said,
        // and their PNGs a line each.
        args.extend([
            self.cfg.screenshot_image.clone(),
            "sh".into(),
            "-c".into(),
            format!(
                "cat > /tmp/axe.js; {} >/dev/null 2>&1 && base64 -w0 /tmp/s.png && echo && (cat /tmp/s.png.json 2>/dev/null || true) && echo && (base64 -w0 /tmp/s.png.phone.png 2>/dev/null || true) \
                 && echo && (cat /tmp/s.png.pages.json 2>/dev/null || true) && for n in 0 1 2 3 4 5 6 7 8 9; do echo; base64 -w0 /tmp/s.png.$n.png 2>/dev/null || true; done \
                 && echo && (cat /tmp/s.png.crops.json 2>/dev/null || true) && for n in 0 1 2 3 4 5 6 7 8 9; do echo; base64 -w0 /tmp/s.png.crop.$n.png 2>/dev/null || true; done",
                self.cfg.screenshot_command
            ),
        ]);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        // Each other page is taken and checked too (accessibility, links up to 20 s, SEO, words).
        // A page state's steps take up to about 25 s more.
        let state_pages = pages.iter().filter(|p| crate::settings::state_entry(p).is_some()).count() as u64;
        // A crop loads its page again: about 15 s each.
        let within = Duration::from_secs(90 + 30 * pages.len() as u64 + 25 * state_pages + 15 * crops.len() as u64 + if endpoints.is_empty() { 0 } else { 25 });
        let chromium = std::time::Instant::now();
        // The other browsers the project asked for, in a run of their own alongside Chromium's (a
        // container each in the service's network): the slot is held for the longer, not both.
        let browsers_run = async {
            let started = std::time::Instant::now();
            let r = if opts.browsers.is_empty() { None } else { Some(self.other_browsers(name, port, opts).await) };
            (r, started.elapsed().as_millis() as u64)
        };
        let (run, (more, browsers_ms)) = tokio::join!(tokio::time::timeout(within, docker_with_input(&argv, AXE_JS.as_bytes(), SHOT_OUTPUT_MAX)), browsers_run);
        let out = match run {
            Ok(Ok(out)) => out,
            // Its container outlives a docker CLI that gave up on it (too much output).
            Ok(Err(e)) => {
                code_preview::docker::remove(&shot).await;
                return Err(e);
            }
            Err(_) => {
                code_preview::docker::remove(&shot).await;
                anyhow::bail!("the screenshot took over {} s", within.as_secs());
            }
        };
        let mut lines = out.split('\n');
        // The report lines are the page's to forge: one too long to be ours reads as nothing.
        fn report(l: Option<&str>) -> &str {
            l.filter(|l| l.len() <= SHOT_REPORT_MAX).unwrap_or("")
        }
        let (b64, speed, phone) = (lines.next().unwrap_or(""), report(lines.next()), lines.next().unwrap_or(""));
        let said = report(lines.next());
        let b64e = base64::engine::general_purpose::STANDARD;
        let png = b64e.decode(b64.trim()).map_err(|e| anyhow::anyhow!("the screenshot isn't a PNG: {e}"))?;
        anyhow::ensure!(png.starts_with(b"\x89PNG") && png.len() <= SCREENSHOT_MAX, "the screenshot isn't a PNG of at most {SCREENSHOT_MAX} bytes");
        let phone = b64e.decode(phone.trim()).ok().filter(|p| p.starts_with(b"\x89PNG") && p.len() <= SCREENSHOT_MAX);
        // Ten page lines, then what the crops said and their PNGs.
        let page_lines: Vec<&str> = lines.by_ref().take(10).collect();
        let crops_said = report(lines.next());
        let asked: Vec<&str> = crops.iter().filter_map(|c| c["note"].as_str()).collect();
        let mut shot = crate::ports::Shot {
            png,
            speed: crate::PageSpeed::parse(speed),
            health: crate::PageHealth::parse(speed),
            a11y: crate::PageA11y::parse(speed),
            text: crate::page_text(speed),
            links: crate::PageLinks::parse(speed),
            api: crate::openapi::lines(speed),
            phone,
            pages: page_shots(&pages, said, page_lines.into_iter()),
            crops: note_crops(&asked, crops_said, lines),
        };
        // The other browsers', best effort: Chromium's screenshot stands.
        match more {
            Some(Ok(more)) => shot.pages.extend(more),
            Some(Err(e)) => tracing::warn!(container = name, error = %e, "previews: other browsers' screenshots"),
            None => {}
        }
        // How long it took, and the other browsers' run within it.
        tracing::info!(container = name, pages = pages.len(), run_ms = chromium.elapsed().as_millis() as u64, browsers_ms, "previews: screenshot run");
        Ok(shot)
    }

    /// The first page in the other browsers `opts.browsers` names (`browser:<name>` pages), in the
    /// service's network as Chromium's run is, locked down the same way.
    async fn other_browsers(&self, name: &str, port: u16, opts: &crate::ports::ShotOptions) -> anyhow::Result<Vec<crate::ports::PageShot>> {
        use base64::Engine as _;
        let building = self.browsers_building.lock().await;
        if code_preview::docker::docker(&["image", "inspect", "--format", "{{.Id}}", BROWSERS_IMAGE]).await.is_err() {
            if let Some(at) = *self.browsers_failed.lock() {
                anyhow::ensure!(at.elapsed() >= BROWSERS_RETRY, "building {BROWSERS_IMAGE} failed {} min ago; tried again within the hour", at.elapsed().as_secs() / 60);
            }
            let label = format!("{}=preview", code_preview::docker::LABEL);
            let dockerfile = BROWSERS_DOCKERFILE.replace("{image}", &self.cfg.screenshot_image);
            let built = match tokio::time::timeout(BROWSERS_BUILD, docker_with_input(&["build", "--quiet", "--label", &label, "-t", BROWSERS_IMAGE, "-"], dockerfile, 1 << 20)).await {
                Ok(r) => r,
                Err(_) => Err(anyhow::anyhow!("building {BROWSERS_IMAGE} took over {} s", BROWSERS_BUILD.as_secs())),
            };
            if let Err(e) = built {
                *self.browsers_failed.lock() = Some(std::time::Instant::now());
                return Err(e);
            }
            *self.browsers_failed.lock() = None;
        }
        // Only the build is one at a time: the runs go side by side.
        drop(building);
        let browsers: Vec<&String> = opts.browsers.iter().filter(|b| crate::settings::BROWSERS.contains(&b.as_str())).collect();
        let hide: Vec<&String> = opts.hide.iter().filter(|s| crate::settings::selector_ok(s)).take(crate::settings::MAX_HIDE).collect();
        let run = format!("roll-previews-browsers-{}", name.trim_start_matches("roll-previews-"));
        let mut args: Vec<String> = vec!["run".into(), "--rm".into(), "-i".into(), "--name".into(), run.clone(), "--network".into(), format!("container:{name}")];
        args.extend(["--label".into(), format!("{}=preview", code_preview::docker::LABEL)]);
        if let Some(l) = code_preview::docker::home_label() {
            args.extend(["--label".into(), l]);
        }
        args.extend(["--cap-drop".into(), "ALL".into(), "--security-opt".into(), "no-new-privileges".into(), "--read-only".into(), "--tmpfs".into(), "/tmp:rw,size=512m".into()]);
        args.extend(code_preview::docker::limits(1024, 1.0));
        args.extend(["--env".into(), format!("URL=http://127.0.0.1:{port}/"), "--env".into(), "HOME=/tmp".into()]);
        args.extend(["--env".into(), format!("BROWSERS={}", serde_json::to_string(&browsers)?), "--env".into(), format!("HIDE={}", serde_json::to_string(&hide)?)]);
        args.extend(["--env".into(), format!("ROLL_BROWSERS_JS={BROWSERS_JS}"), BROWSERS_IMAGE.into(), "sh".into(), "-c".into(), r#"node -e "$ROLL_BROWSERS_JS""#.into()]);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let within = Duration::from_secs(30 + 40 * browsers.len() as u64);
        let max = browsers.len() * (SCREENSHOT_MAX * 4 / 3 + 4096);
        let out = match tokio::time::timeout(within, docker_with_input(&argv, Vec::new(), max)).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                code_preview::docker::remove(&run).await;
                return Err(e);
            }
            Err(_) => {
                code_preview::docker::remove(&run).await;
                anyhow::bail!("the other browsers took over {} s", within.as_secs());
            }
        };
        #[derive(serde::Deserialize)]
        struct Line {
            browser: String,
            #[serde(default)]
            errors: u32,
            png: String,
        }
        let mut pages: Vec<crate::ports::PageShot> = Vec::new();
        for l in out.lines().filter_map(|l| serde_json::from_str::<Line>(l).ok()) {
            let Ok(png) = base64::engine::general_purpose::STANDARD.decode(l.png.trim()) else { continue };
            let path = format!("browser:{}", l.browser);
            if png.starts_with(b"\x89PNG") && png.len() <= SCREENSHOT_MAX && browsers.iter().any(|b| **b == l.browser) && !pages.iter().any(|p| p.path == path) {
                pages.push(crate::ports::PageShot { path, png, errors: l.errors.min(10_000), report: Default::default() });
            }
        }
        Ok(pages)
    }

    async fn seed_job(&self, job: &crate::ports::SeedJob) -> anyhow::Result<(i32, String)> {
        use code_preview::runner::Outcome;
        let docker = self.iso.name() == "docker";
        let env: Vec<(String, String)> = job.env.iter().map(|(k, v)| (k.clone(), reach_local(&self.iso.reach(v), docker))).collect();
        let spec = code_preview::config::CheckSpec {
            image: job.seed.image.clone(),
            run: job.seed.run.clone(),
            timeout: Duration::from_secs(job.seed.timeout_s as u64),
            memory_mb: job.seed.memory_mb,
            cpus: 1.0,
            env: Default::default(),
            // A seed's variables come from the preview (`job.env`), not a secrets environment.
            secrets: None,
        };
        // Held to what the preview's services reach: on its own network under Roll only, the
        // listed hosts through its egress proxy (a fork's migrations can't post its data out).
        let mut env = env;
        let network = if job.egress == crate::settings::EgressRule::Roll && docker { Some(preview_network(&job.preview).await?) } else { None };
        if let Some(n) = network.as_ref().filter(|_| !job.egress_hosts.is_empty()) {
            self.egress_proxy(&job.preview, &n.name, &job.egress_hosts).await?;
            let proxy = format!("http://{EGRESS_ALIAS}:3128");
            for k in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
                env.push((k.into(), proxy.clone()));
            }
            for k in ["NO_PROXY", "no_proxy"] {
                env.push((k.into(), "host.docker.internal,localhost,127.0.0.1".into()));
            }
        }
        let out = self.trees.run(&self.iso, &spec, &job.repo, &job.revision, &env, network).await?;
        match out.result {
            Outcome::Exited(code) => Ok((code, out.log)),
            Outcome::TimedOut => Ok((124, format!("timed out after {}s\n{}", job.seed.timeout_s, out.log))),
            Outcome::Error(e) => Err(anyhow::anyhow!(e)),
        }
    }

    fn service_status(&self, preview: &str, service: &str) -> Option<ServiceState> {
        let live = self.live.lock().get(&(preview.to_string(), service.to_string())).cloned()?;
        let phase = live.phase.borrow().clone();
        Some(match phase {
            Phase::Stopped => ServiceState::Sleeping,
            Phase::Starting => ServiceState::Deploying,
            Phase::Running { .. } => ServiceState::Running,
            Phase::Frozen { .. } => ServiceState::Sleeping,
            Phase::Failed(_) => ServiceState::Failed,
        })
    }
}

/// The notes' crops a screenshot run took: which note each line is (`[{note, n}]`, each `n` the
/// crop's line), and the PNGs a line each. Only notes that were asked for, each once, each a PNG
/// within [`crate::store::CROP_MAX`].
fn note_crops<'a>(asked: &[&str], said: &str, pngs: impl Iterator<Item = &'a str>) -> Vec<(String, Vec<u8>)> {
    #[derive(serde::Deserialize)]
    struct Said {
        note: String,
        n: usize,
    }
    use base64::Engine as _;
    let said: Vec<Said> = serde_json::from_str(said.trim()).unwrap_or_default();
    let pngs: Vec<&str> = pngs.take(crate::store::CROPS_PER_RUN).collect();
    let b64e = base64::engine::general_purpose::STANDARD;
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    for s in said {
        if !asked.contains(&s.note.as_str()) || out.iter().any(|(n, _)| *n == s.note) {
            continue;
        }
        let Some(png) = pngs.get(s.n).and_then(|l| b64e.decode(l.trim()).ok()) else { continue };
        if png.starts_with(b"\x89PNG") && png.len() <= crate::store::CROP_MAX {
            out.push((s.note, png));
        }
    }
    out
}

const EGRESS_JS: &str = include_str!("egress.js");
/// The egress proxy's name on a preview's network.
const EGRESS_ALIAS: &str = "roll-egress";

fn egress_name(preview: &str) -> String {
    format!("roll-previews-egress-{}", preview.trim_start_matches("pv_"))
}

impl Runtime {
    /// The preview's egress proxy running with `hosts` (made again when they changed): on the
    /// preview's network as [`EGRESS_ALIAS`] and on Docker's default one, its way out.
    /// Remove the preview's egress proxy (its project lists no hosts any more), under the same
    /// lock as making it.
    async fn drop_egress_proxy(&self, preview: &str) {
        let turn = self.egress_making.lock().entry(preview.to_string()).or_default().clone();
        let _turn = turn.lock().await;
        code_preview::docker::remove(&egress_name(preview)).await;
        self.stop_intercepting(preview);
    }

    /// A preview being intercepted: its code trusts its interception CA too (the CA's PEM as a
    /// variable, written to a file and added to the system's bundle before the command runs):
    /// `NODE_EXTRA_CA_CERTS`, and `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE` and `CURL_CA_BUNDLE` as the
    /// system's bundle plus it, so every other host still verifies as before. A runtime with a
    /// store of its own (Java's keystore) doesn't read these.
    fn trust_intercept_ca(&self, preview: &str, job: &mut code_preview::isolation::Job) {
        let Some(dir) = self.cfg.intercept_dir.as_ref().filter(|_| self.intercepting.lock().contains_key(preview)) else { return };
        let Ok(pem) = std::fs::read_to_string(dir.join(preview).join("ca.pem")) else { return };
        job.env.push(("ROLL_INTERCEPT_CA".into(), pem));
        job.run = format!("{TRUST_INTERCEPT_CA}\n{}", job.run);
    }

    fn stop_intercepting(&self, preview: &str) {
        if let Some((_, h)) = self.intercepting.lock().remove(preview) {
            h.abort();
        }
        if let Some(d) = &self.cfg.intercept_dir {
            let _ = std::fs::remove_dir_all(d.join(preview));
        }
    }

    /// The preview's interception, if it has one and this node intercepts: its socket served (made
    /// again when what it covers changed) and what its egress container needs (the directory to
    /// mount, the hosts, its owner to run as).
    async fn intercepting(&self, preview: &str) -> anyhow::Result<Option<(std::path::PathBuf, String, String)>> {
        let (Some(dir), Some(source)) = (self.cfg.intercept_dir.clone(), self.intercepts.get().cloned()) else { return Ok(None) };
        let p = preview.to_string();
        let Some(i) = tokio::task::spawn_blocking(move || source.of(&p)).await?? else {
            self.stop_intercepting(preview);
            return Ok(None);
        };
        let covered = serde_json::to_string(&i.hosts)?;
        // A new CA with the same hosts is a new listener too: the old one's CA has ended.
        let made = format!("{covered}#{}", i.id);
        let dir = dir.join(preview);
        let user = {
            use std::os::unix::fs::{DirBuilderExt, MetadataExt};
            let fresh = self.intercepting.lock().get(preview).is_none_or(|(c, h)| *c != made || h.is_finished());
            if fresh {
                // The directory stays (a running egress container has it mounted): only its socket
                // is made again.
                if let Some((_, h)) = self.intercepting.lock().remove(preview) {
                    h.abort();
                }
                std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir.join("sock"))?;
                std::fs::write(dir.join("ca.pem"), &i.ca_pem)?;
                let sock = dir.join("sock").join("intercept.sock");
                let _ = std::fs::remove_file(&sock);
                let listener = tokio::net::UnixListener::bind(&sock)?;
                let engine = self.engine.get().cloned();
                let id = preview.to_string();
                let done: Arc<dyn Fn(String, anyhow::Result<crate::intercept::Served>) + Send + Sync> = Arc::new(move |host, out| {
                    let line = match out {
                        Ok(s) if s.blocked.is_empty() && s.swapped == 0 => return,
                        Ok(s) if s.blocked.is_empty() => format!("egress to {host}: {} request(s), {} key(s) swapped in", s.requests, s.swapped),
                        Ok(s) => format!("egress to {host}: {} request(s), {} key(s) swapped in, refused: {}", s.requests, s.swapped, s.blocked.join("; ")),
                        // Its CA ended (a key's bound hosts changed): a converge makes the new one
                        // and restarts the services with it.
                        Err(e) if format!("{e:#}").contains("interception CA has ended") => {
                            if let Some(e) = engine.as_ref().and_then(Weak::upgrade) {
                                let _ = e.store.update(&id, |row| row.next_at = 0);
                                e.kick();
                            }
                            format!("egress to {}: refused: a key's bound hosts changed; restarting with a new interception certificate", if host.is_empty() { "?" } else { &host })
                        }
                        Err(e) => format!("egress to {}: refused: {e:#}", if host.is_empty() { "?" } else { &host }),
                    };
                    if let Some(e) = engine.as_ref().and_then(Weak::upgrade) {
                        let _ = e.store.log(&id, &line);
                    }
                });
                let (upstream, dial) = match &self.cfg.intercept_private {
                    Some(root) => (crate::intercept::with_root(root)?, crate::intercept::any_dial()),
                    None => (crate::intercept::system_upstream(), crate::intercept::public_dial()),
                };
                let task = tokio::spawn(crate::intercept::listen(listener, i.interception, upstream, dial, done));
                self.intercepting.lock().insert(preview.to_string(), (made.clone(), task.abort_handle()));
            }
            let m = std::fs::metadata(dir.join("sock"))?;
            format!("{}:{}", m.uid(), m.gid())
        };
        Ok(Some((dir, covered, user)))
    }

    async fn egress_proxy(&self, preview: &str, network: &str, hosts: &[String]) -> anyhow::Result<()> {
        let hosts: Vec<&String> = hosts.iter().filter(|h| crate::settings::egress_host_ok(h)).take(crate::settings::MAX_EGRESS_HOSTS).collect();
        let turn = self.egress_making.lock().entry(preview.to_string()).or_default().clone();
        let _turn = turn.lock().await;
        // Under the turn: two starts at once make one socket and one listener.
        let intercept = self.intercepting(preview).await?;
        let list = serde_json::to_string(&(&hosts, intercept.as_ref().map(|(_, c, _)| c)))?;
        let digest = format!("{:016x}", list.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)));
        let name = egress_name(preview);
        let running = code_preview::docker::docker(&["inspect", "-f", "{{.State.Running}} {{index .Config.Labels \"sh.roll.egress\"}}", &name]).await.ok();
        if running.as_deref() == Some(format!("true {digest}").as_str()) {
            return Ok(());
        }
        code_preview::docker::remove(&name).await;
        let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), name.clone(), "--network".into(), network.into(), "--network-alias".into(), EGRESS_ALIAS.into()];
        args.extend(["--label".into(), format!("{}=preview", code_preview::docker::LABEL), "--label".into(), format!("sh.roll.egress={digest}")]);
        if let Some(l) = code_preview::docker::home_label() {
            args.extend(["--label".into(), l]);
        }
        args.extend(["--cap-drop".into(), "ALL".into(), "--security-opt".into(), "no-new-privileges".into(), "--read-only".into(), "--tmpfs".into(), "/tmp:rw,size=16m".into()]);
        args.extend(code_preview::docker::limits(128, 0.25));
        // Hosts a placeholder's key is bound to go to rolld's socket: mounted, and the proxy runs as
        // the socket's owner (it can't reach it as a root with no capabilities otherwise).
        if let Some((dir, covered, user)) = &intercept {
            args.extend(["--user".into(), user.clone(), "-v".into(), format!("{}:/roll:ro", dir.join("sock").display()), "--env".into(), format!("INTERCEPT={covered}")]);
        }
        args.extend(["--env".into(), format!("HOSTS={}", serde_json::to_string(&hosts)?), "--env".into(), format!("ROLL_EGRESS_JS={EGRESS_JS}"), self.cfg.screenshot_image.clone(), "sh".into(), "-c".into(), r#"node -e "$ROLL_EGRESS_JS""#.into()]);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        code_preview::docker::docker(&argv).await?;
        // Its way out: Docker's default network too.
        code_preview::docker::docker(&["network", "connect", "bridge", &name]).await?;
        Ok(())
    }
}

/// What a service's command runs after when its preview is intercepted (`trust_intercept_ca`).
const TRUST_INTERCEPT_CA: &str = r#"if [ -n "$ROLL_INTERCEPT_CA" ]; then printf '%s\n' "$ROLL_INTERCEPT_CA" > /tmp/roll-intercept-ca.pem; b=/tmp/roll-ca-bundle.pem; : > "$b"; for f in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt /etc/ssl/cert.pem; do if [ -r "$f" ]; then cat "$f" >> "$b"; break; fi; done; cat /tmp/roll-intercept-ca.pem >> "$b"; export NODE_EXTRA_CA_CERTS=/tmp/roll-intercept-ca.pem SSL_CERT_FILE="$b" REQUESTS_CA_BUNDLE="$b" CURL_CA_BUNDLE="$b"; fi"#;

/// The network a preview whose project limits egress runs on (docs/previews.md, "Egress").
fn network_name(preview: &str) -> String {
    format!("roll-previews-net-{}", preview.trim_start_matches("pv_"))
}

/// The preview's own network, made `--internal` (no route out) if it isn't there yet, with its
/// gateway (where its containers reach roll's products).
async fn preview_network(preview: &str) -> anyhow::Result<code_preview::isolation::Network> {
    let name = network_name(preview);
    let gateway = |name: String| async move { code_preview::docker::docker(&["network", "inspect", "-f", "{{range .IPAM.Config}}{{.Gateway}} {{end}}", &name]).await.ok().and_then(|g| g.split_whitespace().next().map(str::to_string)) };
    if let Some(g) = gateway(name.clone()).await {
        return Ok(code_preview::isolation::Network { name, gateway: g });
    }
    let mut args: Vec<String> = vec!["network".into(), "create".into(), "--internal".into(), "--label".into(), format!("{}=preview", code_preview::docker::LABEL)];
    if let Some(l) = code_preview::docker::home_label() {
        args.extend(["--label".into(), l]);
    }
    args.push(name.clone());
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    // Two starts at once may both make it: the second's "already exists" is fine.
    let _ = code_preview::docker::docker(&argv).await;
    let g = gateway(name.clone()).await.ok_or_else(|| anyhow::anyhow!("couldn't make the preview's network {name}"))?;
    Ok(code_preview::isolation::Network { name, gateway: g })
}

/// The other pages a screenshot run took: what it said about each (`[{path, errors}]`, in order)
/// and their PNGs, one line each. Only paths that were asked for, each once.
fn page_shots<'a>(asked: &[&String], said: &str, pngs: impl Iterator<Item = &'a str>) -> Vec<crate::ports::PageShot> {
    use base64::Engine as _;
    #[derive(serde::Deserialize)]
    struct Said {
        path: String,
        #[serde(default)]
        errors: u32,
        #[serde(default)]
        a11y: Option<serde_json::Value>,
        #[serde(default)]
        seo: Option<serde_json::Value>,
        #[serde(default)]
        links: Option<serde_json::Value>,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        unstable: bool,
        #[serde(default)]
        step: Option<String>,
    }
    let said: Vec<Said> = serde_json::from_str(said.trim()).unwrap_or_default();
    let mut out: Vec<crate::ports::PageShot> = Vec::new();
    for (s, b64) in said.into_iter().zip(pngs).take(asked.len()) {
        let Ok(png) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else { continue };
        if png.starts_with(b"\x89PNG") && png.len() <= SCREENSHOT_MAX && asked.iter().any(|a| **a == s.path) && !out.iter().any(|p| p.path == s.path) {
            // Read as the first page's are (the same bounds), from what this page's run wrote.
            let report = crate::PageReport {
                a11y: s.a11y.and_then(|a| crate::PageA11y::parse(&serde_json::json!({ "a11y": a, "seo": s.seo }).to_string())),
                links: s.links.and_then(|l| crate::PageLinks::parse(&serde_json::json!({ "links": l }).to_string())),
                text: s.text.and_then(|t| crate::page_text(&serde_json::json!({ "text": t }).to_string())),
                unstable: s.unstable,
                // A state's step that found nothing (the page's own words in it: one bounded line).
                step: s.step.map(|t| t.chars().filter(|c| !c.is_control()).take(200).collect()),
            };
            out.push(crate::ports::PageShot { path: s.path, png, errors: s.errors.min(10_000), report });
        }
    }
    out
}

/// Middleware for the API listener: requests for a preview service's host go to it, the rest
/// pass through.
pub async fn route(State(rt): State<Arc<Runtime>>, req: Request, next: Next) -> Response {
    match named(&rt, &req) {
        Some((preview, service)) => serve_named(rt, preview, service, req).await,
        None => match gone(&rt, &req) {
            Some(page) => page,
            None => next.run(req).await,
        },
    }
}

/// An earlier push its preview no longer keeps: a page saying so (410), with the way to the
/// current one, rather than a 404 that reads as a broken preview. It names no more than the link
/// did (the service and its push), and the current link is the preview's own, behind its gate.
fn gone(rt: &Runtime, req: &Request) -> Option<Response> {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).map(str::to_string).or_else(|| req.uri().host().map(str::to_string))?;
    let crate::store::GonePush { service, revision, current_url: current, .. } = rt.gone_for_host(&host)?;
    let short: String = revision.strip_prefix("cs_").unwrap_or(&revision).chars().take(10).collect();
    let now = current.map_or_else(|| "<p>The preview no longer runs this service.</p>".to_string(), |u| format!("<p><a href=\"{}\">Open the latest push</a>.</p>", escape(&u)));
    let mut r = roll_page(
        StatusCode::GONE,
        "This push isn't kept any more",
        &format!("<p>This link was <code>{}</code> at an earlier push (<code>{}</code>). A preview keeps the last {} pushes of each service; this one was replaced since.</p>{now}", escape(&service), escape(&short), crate::store::PINNED_KEEP),
    );
    r.headers_mut().insert(header::HeaderName::from_static("x-robots-tag"), header::HeaderValue::from_static("noindex"));
    Some(r)
}

/// A request roll apps' ingress hands on (a name under a project's own preview domain that no
/// `pv-` environment holds): the preview service it names, or a 404.
pub async fn serve_host(rt: Arc<Runtime>, req: Request) -> Response {
    match named(&rt, &req) {
        Some((preview, service)) => serve_named(rt, preview, service, req).await,
        None => gone(&rt, &req).unwrap_or_else(|| (StatusCode::NOT_FOUND, "no such preview").into_response()),
    }
}

/// The preview and service a request's Host names.
fn named(rt: &Runtime, req: &Request) -> Option<(String, String)> {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).map(str::to_string).or_else(|| req.uri().host().map(str::to_string));
    host.and_then(|h| rt.service_for_host(&h))
}

async fn serve_named(rt: Arc<Runtime>, preview: String, service: String, req: Request) -> Response {
    let mut resp = serve(rt, preview, service, req).await;
    // Kept out of search engines, whatever the service says (as Vercel's previews are): a public
    // preview, a share link or an earlier push's URL is never what a search should land on.
    resp.headers_mut().insert(header::HeaderName::from_static("x-robots-tag"), header::HeaderValue::from_static("noindex"));
    resp
}

/// A request for a preview's service: through its gate, then to the service.
async fn serve(rt: Arc<Runtime>, preview: String, service: String, mut req: Request) -> Response {
    if let Some(r) = rt.gate(&preview, &service, &mut req).await {
        return r;
    }
    // The feedback button: its script, and pages (a browser's GET for HTML) that load it.
    let target = rt.feedback_target(&preview, &req);
    // The link's token is roll's: the app never sees it, whichever way they came in.
    take_cookies(&mut req, &format!("roll_access_{preview}"));
    if req.uri().path() == FEEDBACK_PATH
        && let Some(t) = &target
    {
        return feedback_script(t);
    }
    if req.uri().path() == FEEDBACK_NOTES_PATH && target.is_some() {
        let path = req.uri().query().unwrap_or("").split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == "path").map(|(_, v)| percent_decode(v)).unwrap_or_default();
        return rt.feedback_notes(&preview, &path).await;
    }
    let feedback = target.is_some() && req.method() == axum::http::Method::GET && is_page_load(&req);
    match rt.cluster.get().cloned() {
        Some(cluster) => {
            let rt2 = rt.clone();
            cluster
                .dispatch(&crate::engine::preview_key(&preview.clone()), req, move |req, guard| async move {
                    let resp = rt2.proxy(&preview, &service, req, feedback).await;
                    drop(guard);
                    resp
                })
                .await
        }
        None => rt.proxy(&preview, &service, req, feedback).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// `docker logs --timestamps` is read as stamped lines: a line with no stamp takes the one
    /// before's, a long one is cut on a character while it's read, only the newest lines up to the
    /// cap are held, and stdout and stderr interleave by their stamps.
    #[tokio::test]
    async fn a_services_stamped_output_is_read_bounded() {
        let long = format!("2026-11-18T10:00:02Z {}é", "x".repeat(TIMED_LINE_MAX - 1));
        let out = format!("2026-11-18T10:00:01.5Z GET / 200\nno stamp here\n{long}\n2026-11-18T10:00:04Z last");
        let out = read_timed(out.as_bytes(), false, 1 << 20).await;
        let t = |ms: i64| 1_794_996_000_000 + ms;
        assert_eq!(out.iter().map(|l| l.0).collect::<Vec<_>>(), [t(1_500), t(1_500), t(2_000), t(4_000)]);
        assert_eq!((out[1].2.as_str(), out[2].2.len(), out[3].2.as_str()), ("no stamp here", TIMED_LINE_MAX - 1, "last"));
        let err = read_timed(&b"2026-11-18T10:00:03Z boom\n"[..], true, 1 << 20).await;
        let merged = merge_timed(out.clone(), err, 3);
        assert_eq!(merged.iter().map(|l| (l.1, &l.2[..l.2.len().min(4)])).collect::<Vec<_>>(), [(false, "xxxx"), (true, "boom"), (false, "last")]);
        // Held up to the cap: the newest lines.
        let kept = read_timed(&b"2026-11-18T10:00:01Z aaaa\n2026-11-18T10:00:02Z bbbb\n2026-11-18T10:00:03Z cccc\n"[..], false, 8).await;
        assert_eq!(kept.iter().map(|l| l.2.as_str()).collect::<Vec<_>>(), ["bbbb", "cccc"]);
    }

    struct NoTrees;

    #[async_trait]
    impl Trees for NoTrees {
        async fn prepare(&self, _: &dyn Isolation, _: &Job, _: &str, _: &str) -> anyhow::Result<()> {
            anyhow::bail!("not in this test")
        }
        async fn run(&self, _: &Arc<dyn Isolation>, _: &code_preview::config::CheckSpec, _: &str, _: &str, _: &[(String, String)], _: Option<code_preview::isolation::Network>) -> anyhow::Result<code_preview::runner::RunOutput> {
            anyhow::bail!("not in this test")
        }
    }

    /// A preview service's host reaches its container; other hosts reach the API.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn service_hosts_reach_their_containers() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&code_sql::Target::Sqlite(dir.path().join("p.db")), Default::default()).unwrap());
        let (row, _) = store
            .create(&crate::store::New {
                project_id: "prj_1".into(),
                key: "pr1".into(),
                project_name: "shop".into(),
                repo: "acme/web".into(),
                change_id: Some("chg_1".into()),
                branch: None,
                number: Some(1),
                title: "t".into(),
                github_pr: Some(1),
                head: "cs_1".into(),
                trusted: true,
                state: crate::State::Ready,
                expires_at: None,
                created_by: "roll".into(),
            })
            .unwrap();
        store.want_service(&row.id, "web", "acme/web", "cs_1", 512, &|h| format!("http://{h}.preview.test")).unwrap();
        let rt = Runtime::new(
            RuntimeConfig { domain: "preview.test".into(), dashboard: Some("https://roll.test".into()), ..RuntimeConfig::default() },
            Arc::new(code_preview::docker::Docker),
            Arc::new(NoTrees),
            store,
        );
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up = upstream.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = upstream.accept().await else { return };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                    let secret = if head.to_ascii_lowercase().contains("x-roll-preview-bypass") { " and the bypass secret" } else { "" };
                    let (kind, body) = match path.as_str() {
                        "/page" => ("text/html", format!("<html><BODY><p>asked for {}</p></BODY></html>", if head.to_ascii_lowercase().contains("accept-encoding: identity") { "identity" } else { "any" })),
                        _ => ("text/plain", format!("service saw {path}{secret}")),
                    };
                    let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
                });
            }
        });
        rt.pretend_running(&row.id, "web", up);
        let app = axum::Router::new().fallback(|| async { "api" }).layer(axum::middleware::from_fn_with_state(rt.clone(), route));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let get = |host: &'static str| async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(format!("GET /hello HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).await.unwrap();
            out
        };
        // Link access (the default): no token, no preview; the link's token turns into a cookie.
        let locked = get("web-pr1-shop.preview.test:7070").await;
        assert!(locked.starts_with("HTTP/1.1 401") && locked.contains("x-robots-tag: noindex"), "{locked}");
        let token = rt.store.access(&row.id).unwrap().unwrap().token;
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("GET /hello?roll_access={token}&x=1 HTTP/1.1\r\nHost: web-pr1-shop.preview.test\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        assert!(out.starts_with("HTTP/1.1 302") && out.contains("location: /hello?x=1"), "{out}");
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("GET /hello HTTP/1.1\r\nHost: web-pr1-shop.preview.test\r\nCookie: a=b; roll_access_{}={token}\r\nConnection: close\r\n\r\n", row.id).as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        assert!(out.ends_with("service saw /hello"), "{out}");
        assert!(out.contains("x-robots-tag: noindex"), "never indexed, whatever the service says: {out}");
        // Automation: the project's bypass secret in a header, which the service never sees.
        let raw = |head: String| async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(head.as_bytes()).await.unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).await.unwrap();
            out
        };
        let with = |secret: &str| format!("GET /hello HTTP/1.1\r\nHost: web-pr1-shop.preview.test\r\nX-Roll-Preview-Bypass: {secret}\r\nConnection: close\r\n\r\n");
        // The feedback button: on a page a visitor through roll asked for as HTML (asked for whole),
        // its script from the preview's own host; nothing for anyone else.
        let page = |accept: &str| format!("GET /page HTTP/1.1\r\nHost: web-pr1-shop.preview.test\r\nAccept: {accept}\r\nCookie: roll_access_{}={token}\r\nConnection: close\r\n\r\n", row.id);
        let html = raw(page("text/html,*/*")).await;
        assert!(html.contains(r#"<p>asked for identity</p><script src="/__roll/feedback.js" defer></script></BODY></html>"#), "{html}");
        assert!(html.to_ascii_lowercase().contains("cache-control: private"), "only this visitor's page: {html}");
        assert!(raw(page("application/json")).await.ends_with("<p>asked for any</p></BODY></html>"), "only a browser's page");
        let framed = format!("GET /page HTTP/1.1\r\nHost: web-pr1-shop.preview.test\r\nAccept: text/html\r\nSec-Fetch-Dest: iframe\r\nCookie: roll_access_{}={token}\r\nConnection: close\r\n\r\n", row.id);
        assert!(!raw(framed).await.contains("feedback.js"), "not in an iframe or a fragment");
        let script = raw(format!("GET /__roll/feedback.js HTTP/1.1\r\nHost: web-pr1-shop.preview.test\r\nCookie: roll_access_{}={token}\r\nConnection: close\r\n\r\n", row.id)).await;
        assert!(script.contains("text/javascript") && script.contains(&format!(r#""https://roll.test/dashboard/projects/prj_1/previews?preview={}""#, row.id)), "{script}");
        assert!(raw("GET /__roll/feedback.js HTTP/1.1\r\nHost: web-pr1-shop.preview.test\r\nConnection: close\r\n\r\n".into()).await.starts_with("HTTP/1.1 401"));
        // A share link: in through its token (a cookie of its own, only for its life), out once
        // it's revoked.
        let (share, stoken) = rt.store.create_share(&row.id, rt.store.now_ms().unwrap() + 3_600_000, "ada").unwrap();
        let entered = raw(format!("GET /hello?roll_share={stoken} HTTP/1.1\r\nHost: web-pr1-shop.preview.test\r\nConnection: close\r\n\r\n")).await;
        assert!(entered.starts_with("HTTP/1.1 302") && entered.contains(&format!("roll_share_{}={stoken}", row.id)) && entered.contains("Max-Age=3"), "{entered}");
        let with_share = format!("GET /hello HTTP/1.1\r\nHost: web-pr1-shop.preview.test\r\nCookie: roll_share_{}={stoken}\r\nConnection: close\r\n\r\n", row.id);
        assert!(raw(with_share.clone()).await.ends_with("service saw /hello"));
        rt.store.revoke_share(&row.id, &share.id).unwrap();
        assert!(raw(with_share).await.starts_with("HTTP/1.1 401"), "revoked: out at once");
        // The notes already left on a page: for the same visitors only.
        let notes = raw(format!("GET /__roll/feedback.json?path=%2F HTTP/1.1\r\nHost: web-pr1-shop.preview.test\r\nCookie: roll_access_{}={token}\r\nConnection: close\r\n\r\n", row.id)).await;
        assert!(notes.contains("application/json") && notes.ends_with("[]"), "{notes}");
        assert!(raw("GET /__roll/feedback.json?path=%2F HTTP/1.1\r\nHost: web-pr1-shop.preview.test\r\nConnection: close\r\n\r\n".into()).await.starts_with("HTTP/1.1 401"));
        rt.store.put_settings("prj_1", &crate::Settings { feedback: false, ..Default::default() }).unwrap();
        assert!(raw(page("text/html")).await.ends_with("<p>asked for any</p></BODY></html>"), "the rule turns it off");
        let (secret, _) = rt.store.rotate_bypass("prj_1").unwrap();
        assert!(raw(with(&secret)).await.ends_with("service saw /hello"));
        assert!(raw(with("rpb_wrong")).await.starts_with("HTTP/1.1 401"));
        rt.store.rotate_bypass("prj_1").unwrap();
        assert!(raw(with(&secret)).await.starts_with("HTTP/1.1 401"), "a rotated secret stops working");
        let (secret, _) = rt.store.rotate_bypass("prj_1").unwrap();
        rt.store.update(&row.id, |r| r.trusted = false).unwrap();
        assert!(raw(with(&secret)).await.starts_with("HTTP/1.1 401"), "a fork's preview never opens to the secret");
        rt.store.update(&row.id, |r| r.trusted = true).unwrap();
        rt.store.put_settings("prj_1", &crate::Settings { access: crate::settings::Access::Public, ..Default::default() }).unwrap();
        assert!(get("web-pr1-shop.preview.test:7070").await.ends_with("service saw /hello"), "public previews need no link");
        assert!(get("localhost").await.ends_with("api"));
        assert!(get("nope-pr1-shop.preview.test").await.ends_with("api"), "an unknown label isn't a preview");
        // An earlier push no longer kept: its host says so (410, the latest linked), however the
        // Host header writes it (a port, a final dot, capitals); nothing else answers for it.
        let url = |h: &str| format!("http://{h}.preview.test");
        let mut evicted = Vec::new();
        for rev in ["cs_a1", "cs_a2", "cs_a3", "cs_a4"] {
            evicted.extend(rt.store.pin(&row.id, "web", rev, &url).unwrap());
        }
        let short = evicted[0].strip_prefix("web@cs_").unwrap().to_ascii_uppercase();
        let host: &'static str = Box::leak(format!("WEB-{short}-PR1-SHOP.preview.test.:7070").into_boxed_str());
        let gone = get(host).await;
        assert!(gone.starts_with("HTTP/1.1 410") && gone.contains("http://web-pr1-shop.preview.test") && gone.contains("x-robots-tag: noindex"), "{gone}");
    }

    /// The tag goes in as the page streams: before `</body>` even when a chunk splits it, at the
    /// end of a page without one, and once.
    #[test]
    fn a_screenshot_run_s_pages_are_only_those_asked_for_each_once() {
        use base64::Engine as _;
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        let (png, junk) = (b64(b"\x89PNG\r\n\x1a\n..."), b64(b"not a png"));
        let asked = [&"/pricing".to_string(), &"/about".to_string()];
        // What the browser container printed: what it said of each page, then their PNGs in order.
        let run = |said: &str, pngs: &[&String]| page_shots(&asked, said, pngs.iter().map(|s| s.as_str())).into_iter().map(|p| (p.path, p.errors)).collect::<Vec<_>>();
        let pricing = || ("/pricing".to_string(), 2);
        assert_eq!(run(r#"[{"path":"/pricing","errors":2},{"path":"/about"}]"#, &[&png, &junk]), [pricing()], "not a PNG");
        assert_eq!(run(r#"[{"path":"/admin"},{"path":"/pricing","errors":2}]"#, &[&png, &png]), [pricing()], "not asked for");
        assert_eq!(run(r#"[{"path":"/pricing","errors":2},{"path":"/pricing"}]"#, &[&png, &png]), [pricing()], "once");
        assert_eq!(run(r#"[{"path":"/pricing","errors":99999}]"#, &[&png]), [("/pricing".to_string(), 10_000)], "bounded");
        assert_eq!(run("not json", &[&png]), []);
    }

    #[test]
    fn the_feedback_tag_goes_in_as_the_page_streams() {
        let run = |chunks: &[&str]| {
            let mut t = Tagger::new();
            let mut out: Vec<u8> = chunks.iter().flat_map(|c| t.push(c.as_bytes())).collect();
            out.extend(t.finish());
            String::from_utf8(out).unwrap()
        };
        let tag = std::str::from_utf8(FEEDBACK_TAG).unwrap();
        assert_eq!(run(&["<p>a</p></bo", "dy></html>"]), format!("<p>a</p>{tag}</body></html>"));
        assert_eq!(run(&["<p>a</p>", "</BODY>", "<script>late()</script></body>"]), format!("<p>a</p>{tag}</BODY><script>late()</script></body>"));
        assert_eq!(run(&["<p>fragment</p>"]), format!("<p>fragment</p>{tag}"));
        assert_eq!(run(&["a", "b", "c"]), format!("abc{tag}"));
    }

    #[test]
    fn query_values_are_decoded_as_browsers_encode_them() {
        assert_eq!(percent_decode("%2Fcheckout%3Fstep%3D2"), "/checkout?step=2");
        assert_eq!(percent_decode("a+b%20c"), "a b c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn loopback_hosts_are_rewritten_for_docker() {
        assert_eq!(reach_local("postgres://u:p@127.0.0.1:55432/main", true), "postgres://u:p@host.docker.internal:55432/main");
        assert_eq!(reach_local("http://localhost:7070/x", true), "http://host.docker.internal:7070/x");
        assert_eq!(reach_local("127.0.0.1:9092,localhost:9093", true), "host.docker.internal:9092,host.docker.internal:9093");
        assert_eq!(reach_local("db.roll.sh:5432", true), "db.roll.sh:5432");
        assert_eq!((reach_local("127.0.0.1", true), reach_local("10.0.0.1", true)), ("host.docker.internal".to_string(), "10.0.0.1".to_string()));
        assert_eq!(reach_local("http://127.0.0.1:1", false), "http://127.0.0.1:1");
    }
}
