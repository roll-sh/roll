//! `opencode serve`: one HTTP server per provider (and per extra environment), shared by every
//! session (T3's `OpenCode2Server.ts`, `OpenCodeServerOwner.ts` and `OpenCodeRuntime.ts`'s
//! `startOpenCodeServerProcess`).
//!
//! - The server runs in its own process group with a password roll makes and keeps in memory
//!   (`OPENCODE_PASSWORD`; an inherited 1.x `OPENCODE_SERVER_PASSWORD` is dropped) and
//!   `OPENCODE_CONFIG_CONTENT` defaulting to `{}`. Its URL comes from the `server listening on`
//!   line within 30 s, and only `GET /api/info` answering `{version, pid}` within 5 s proves it
//!   ready.
//! - It is borrowed: each session (and each status probe) holds a [`Lease`]; 30 s after the last
//!   one goes it gets SIGTERM, then SIGKILL a second later.
//! - One `GET /api/event` stream per server, subscribed before anything else; its events go to
//!   the session they name. A session's subagent sessions (`session.created` with a `parentID`
//!   it owns) go to the same session. When the stream ends or goes silent, every session hears
//!   [`Inbound::Closed`] and the server is not lent again.

use super::http::{self, Client};
use super::wire;
use anyhow::{Context, anyhow, bail};
use base64::Engine;
use rand::RngCore;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// T3's `OPENCODE_SERVER_IDLE_TTL`.
const IDLE: Duration = Duration::from_secs(30);
const LISTEN_TIMEOUT: Duration = Duration::from_secs(30);
/// T3's `INFO_TIMEOUT`.
const INFO_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_GRACE: Duration = Duration::from_secs(1);
const STDERR_TAIL: usize = 16 << 10;
const MAX_LINE: usize = 64 << 10;
/// A fresh server lists no agents or models for its first moments (T3 retries both).
const WARMUP: Duration = Duration::from_secs(5);
const WARMUP_STEP: Duration = Duration::from_millis(250);

/// How to start the server. No `Debug`: its environment can hold API keys.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Spec {
    pub binary: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// What a session hears from the event stream.
#[derive(Debug)]
pub enum Inbound {
    Event(wire::Event),
    /// The stream ended; no more events follow.
    Closed(String),
}

/// The servers of one provider, by how they start.
#[derive(Default)]
pub struct Pool {
    slots: Mutex<HashMap<Spec, Arc<Slot>>>,
}

#[derive(Default)]
struct Slot {
    starting: tokio::sync::Mutex<()>,
    state: Mutex<SlotState>,
}

#[derive(Default)]
struct SlotState {
    server: Option<Arc<Server>>,
    borrowers: usize,
    idle: Option<tokio::task::JoinHandle<()>>,
}

/// A borrowed server; dropping it gives it back.
pub struct Lease {
    pub server: Arc<Server>,
    slot: Arc<Slot>,
}

impl Pool {
    pub async fn acquire(&self, spec: Spec) -> anyhow::Result<Lease> {
        let slot = self.slots.lock().unwrap().entry(spec.clone()).or_default().clone();
        let _starting = slot.starting.lock().await;
        {
            let mut st = slot.state.lock().unwrap();
            if let Some(idle) = st.idle.take() {
                idle.abort();
            }
            if let Some(server) = st.server.clone().filter(|s| s.alive()) {
                st.borrowers += 1;
                return Ok(Lease { server, slot: slot.clone() });
            }
            st.server = None;
            st.borrowers = 0;
        }
        let server = Server::start(&spec).await?;
        let mut st = slot.state.lock().unwrap();
        st.server = Some(server.clone());
        st.borrowers = 1;
        Ok(Lease { server, slot: slot.clone() })
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut st = self.slot.state.lock().unwrap();
        if !st.server.as_ref().is_some_and(|s| Arc::ptr_eq(s, &self.server)) {
            return;
        }
        st.borrowers = st.borrowers.saturating_sub(1);
        if st.borrowers > 0 {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            st.server = None;
            return;
        };
        let (slot, server) = (Arc::downgrade(&self.slot), Arc::downgrade(&self.server));
        st.idle = Some(runtime.spawn(async move {
            tokio::time::sleep(IDLE).await;
            let (Some(slot), Some(server)) = (slot.upgrade(), server.upgrade()) else { return };
            let idle = {
                let mut st = slot.state.lock().unwrap();
                let current = st.server.as_ref().is_some_and(|s| Arc::ptr_eq(s, &server));
                if current && st.borrowers == 0 { st.server.take() } else { None }
            };
            if let Some(server) = idle {
                server.stop().await;
            }
        }));
    }
}

/// Each agent's own path rules, by agent id.
pub type AgentPaths = HashMap<String, Vec<wire::Rule>>;

/// A running `opencode serve`.
pub struct Server {
    pub client: Client,
    process: Process,
    routes: Mutex<HashMap<String, Route>>,
    dead: AtomicBool,
    tail: Arc<Mutex<VecDeque<u8>>>,
    windows: tokio::sync::Mutex<HashMap<String, Arc<HashMap<String, u64>>>>,
    agents: tokio::sync::Mutex<HashMap<String, Arc<AgentPaths>>>,
}

#[derive(Clone)]
struct Route {
    /// The roll session's own OpenCode session.
    root: String,
    tx: mpsc::UnboundedSender<Inbound>,
}

impl Server {
    /// The free port is found by binding one and letting it go, so another process can take it
    /// before `opencode serve` binds it: a server that exits before it listens gets another port
    /// (three tries in all).
    async fn start(spec: &Spec) -> anyhow::Result<Arc<Server>> {
        let mut tries = 1;
        loop {
            match Self::start_once(spec).await {
                Err(e) if tries < 3 && e.is::<ExitedEarly>() => {
                    tracing::debug!(target: "agent_host::opencode", "retrying on another port: {e}");
                    tries += 1;
                }
                r => return r,
            }
        }
    }

    async fn start_once(spec: &Spec) -> anyhow::Result<Arc<Server>> {
        let port = std::net::TcpListener::bind(("127.0.0.1", 0)).and_then(|l| l.local_addr()).context("finding a free port")?.port();
        let mut secret = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut secret);
        let password = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
        let mut cmd = Command::new(&spec.binary);
        cmd.args(&spec.args)
            .args(["serve", "--hostname=127.0.0.1", &format!("--port={port}")])
            .env_remove("OPENCODE_SERVER_PASSWORD")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if !spec.env.iter().any(|(k, _)| k == "OPENCODE_CONFIG_CONTENT") && std::env::var_os("OPENCODE_CONFIG_CONTENT").is_none() {
            cmd.env("OPENCODE_CONFIG_CONTENT", "{}");
        }
        // roll's own password is the only one (`OPENCODE_PASSWORD` overrides a 1.x
        // `OPENCODE_SERVER_PASSWORD`, which is dropped).
        for (k, v) in spec.env.iter().filter(|(k, _)| k != "OPENCODE_SERVER_PASSWORD" && k != "OPENCODE_PASSWORD") {
            cmd.env(k, v);
        }
        cmd.env("OPENCODE_PASSWORD", &password);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().with_context(|| format!("starting {} serve", spec.binary.display()))?;
        let process = Process { pgid: child.id().map(|p| p as i32), child: tokio::sync::Mutex::new(None) };
        let stdout = child.stdout.take().context("opencode stdout")?;
        let stderr = child.stderr.take().context("opencode stderr")?;
        *process.child.lock().await = Some(child);
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        tokio::spawn(stderr_loop(stderr, tail.clone()));
        let mut stdout = BufReader::new(stdout);
        let url = match tokio::time::timeout(LISTEN_TIMEOUT, listening(&mut stdout)).await {
            Ok(Ok(url)) => url,
            Ok(Err(e)) => return Err(ExitedEarly(format!("{e}{}", tail_text(&tail).map(|t| format!(": {t}")).unwrap_or_default())).into()),
            Err(_) => bail!("opencode serve didn't say where it listens within {}s", LISTEN_TIMEOUT.as_secs()),
        };
        tokio::spawn(drain(stdout));
        let client = Client::new(&url, &password)?;
        let info = tokio::time::timeout(INFO_TIMEOUT, client.get::<wire::Info>("/api/info")).await;
        match info {
            Ok(Ok(_)) => {}
            Ok(Err(e)) if e.status() == Some(401) => bail!("the OpenCode server rejected the server password"),
            Ok(Err(http::HttpError::Transport { .. })) => bail!("couldn't reach the OpenCode server"),
            Ok(Err(e)) if e.status().is_some() => bail!("the OpenCode server returned HTTP {}", e.status().unwrap_or(0)),
            Ok(Err(_)) => bail!("the server is not an OpenCode 2 server"),
            Err(_) => bail!("timed out waiting for the OpenCode server"),
        }
        let events = client.events().await.map_err(|e| anyhow!("subscribing to OpenCode's events: {e}"))?;
        let server = Arc::new(Server {
            client,
            process,
            routes: Mutex::default(),
            dead: AtomicBool::new(false),
            tail,
            windows: Default::default(),
            agents: Default::default(),
        });
        tokio::spawn(read_events(Arc::downgrade(&server), events));
        Ok(server)
    }

    pub fn alive(&self) -> bool {
        !self.dead.load(Ordering::Relaxed)
    }

    /// Send the events of `session` (and of the subagent sessions it starts) to `tx`.
    pub fn register(&self, session: &str, tx: mpsc::UnboundedSender<Inbound>) {
        self.routes.lock().unwrap().insert(session.to_string(), Route { root: session.to_string(), tx });
    }

    pub fn unregister(&self, session: &str) {
        self.routes.lock().unwrap().retain(|_, r| r.root != session);
    }

    fn route(&self, event: wire::Event) {
        let mut routes = self.routes.lock().unwrap();
        if event.kind == "session.created"
            && let (Some(child), Some(parent)) = (event.session(), event.data.get("parentID").and_then(|p| p.as_str()))
            && let Some(route) = routes.get(parent).cloned()
        {
            routes.insert(child.to_string(), route);
        }
        if let Some(route) = event.session().and_then(|s| routes.get(s)) {
            let _ = route.tx.send(Inbound::Event(event));
        }
    }

    fn close_routes(&self, why: &str) {
        self.dead.store(true, Ordering::Relaxed);
        let routes = std::mem::take(&mut *self.routes.lock().unwrap());
        let mut told = std::collections::HashSet::new();
        for route in routes.into_values() {
            if told.insert(route.root.clone()) {
                let _ = route.tx.send(Inbound::Closed(why.to_string()));
            }
        }
    }

    /// The models of `dir` (T3's `makeOpenCode2ModelLoader`: a cold server lists a partial
    /// catalog, so read until two reads agree, at most [`WARMUP`]).
    pub async fn models(&self, dir: &str) -> Result<Vec<wire::Model>, http::HttpError> {
        let path = format!("/api/model?{}", http::location(dir));
        let deadline = tokio::time::Instant::now() + WARMUP;
        let mut last: Option<(Vec<String>, Vec<wire::Model>)> = None;
        loop {
            let read: Result<wire::Data<Vec<wire::Model>>, _> = self.client.get(&path).await;
            let models = match read {
                Ok(page) => page.data,
                Err(e) => return last.map(|(_, m)| m).filter(|m| !m.is_empty()).ok_or(e),
            };
            let mut print: Vec<String> = models.iter().map(|m| format!("{}/{}:{}", m.provider_id, m.id, m.variants.len())).collect();
            print.sort();
            let settled = !models.is_empty() && last.as_ref().is_some_and(|(p, _)| *p == print);
            if settled || tokio::time::Instant::now() + WARMUP_STEP > deadline {
                return Ok(models);
            }
            last = Some((print, models));
            tokio::time::sleep(WARMUP_STEP).await;
        }
    }

    /// Each model's context window in `dir`, by slug; read once per directory.
    pub async fn windows(&self, dir: &str) -> Arc<HashMap<String, u64>> {
        let mut cache = self.windows.lock().await;
        if let Some(known) = cache.get(dir) {
            return known.clone();
        }
        let read: Result<wire::Data<Vec<wire::Model>>, _> = self.client.get(&format!("/api/model?{}", http::location(dir))).await;
        let windows: HashMap<String, u64> = match read {
            Ok(page) => page.data.iter().filter_map(|m| m.window().map(|w| (format!("{}/{}", m.provider_id, m.id), w))).collect(),
            Err(e) => {
                tracing::debug!(target: "agent_host::opencode", "reading models: {e}");
                return Arc::default();
            }
        };
        let windows = Arc::new(windows);
        cache.insert(dir.to_string(), windows.clone());
        windows
    }

    /// Each agent's own path rules in `dir` (T3's `agentPaths`: allows of `edit` and
    /// `external_directory` for particular paths), read once per directory.
    pub async fn agent_paths(&self, dir: &str) -> Arc<AgentPaths> {
        let mut cache = self.agents.lock().await;
        if let Some(known) = cache.get(dir) {
            return known.clone();
        }
        let path = format!("/api/agent?{}", http::location(dir));
        let deadline = tokio::time::Instant::now() + WARMUP;
        let agents = loop {
            match self.client.get::<wire::Data<Vec<wire::Agent>>>(&path).await {
                Ok(page) if !page.data.is_empty() => break page.data,
                Ok(_) if tokio::time::Instant::now() + WARMUP_STEP < deadline => tokio::time::sleep(WARMUP_STEP).await,
                other => {
                    if let Err(e) = other {
                        tracing::warn!(target: "agent_host::opencode", "couldn't list OpenCode's agents; their path rules are skipped: {e}");
                    }
                    return Arc::default();
                }
            }
        };
        let paths: AgentPaths = agents
            .into_iter()
            .map(|a| {
                let own = a
                    .permissions
                    .into_iter()
                    .filter(|r| r.effect == wire::Effect::Allow && r.resource != "*" && (r.action == "edit" || r.action == "external_directory"))
                    .collect();
                (a.id, own)
            })
            .collect();
        let paths = Arc::new(paths);
        cache.insert(dir.to_string(), paths.clone());
        paths
    }

    /// SIGTERM the group, SIGKILL it a second later, and reap.
    pub async fn stop(&self) {
        self.dead.store(true, Ordering::Relaxed);
        let Some(mut child) = self.process.child.lock().await.take() else { return };
        signal_group(self.process.pgid, Signal::Term);
        if tokio::time::timeout(STOP_GRACE, child.wait()).await.is_err() {
            signal_group(self.process.pgid, Signal::Kill);
            let _ = child.kill().await;
        }
        signal_group(self.process.pgid, Signal::Kill);
    }

    fn stderr(&self) -> Option<String> {
        tail_text(&self.tail)
    }
}

struct Process {
    child: tokio::sync::Mutex<Option<Child>>,
    pgid: Option<i32>,
}

impl Drop for Process {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.get_mut().take() {
            signal_group(self.pgid, Signal::Kill);
            let _ = child.start_kill();
        }
    }
}

enum Signal {
    Term,
    Kill,
}

fn signal_group(pgid: Option<i32>, signal: Signal) {
    #[cfg(unix)]
    if let Some(pgid) = pgid.filter(|&p| p > 0) {
        let sig = match signal {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        };
        // SAFETY: kill(2) with a negative pid signals the process group made for the server
        // (`process_group(0)`, so its id is the child's pid); it touches no memory.
        unsafe {
            libc::kill(-pgid, sig);
        }
    }
    #[cfg(not(unix))]
    let _ = (pgid, signal);
}

async fn read_events(server: Weak<Server>, mut events: http::Events) {
    let why = loop {
        match events.next().await {
            Ok(Some(data)) => {
                let Some(server) = server.upgrade() else { return };
                match http::parse_event(&data) {
                    Some(event) => server.route(event),
                    None => tracing::debug!(target: "agent_host::opencode", "skipped an event roll can't read"),
                }
            }
            Ok(None) => break "the OpenCode server ended its event stream".to_string(),
            Err(e) => break e.to_string(),
        }
    };
    let Some(server) = server.upgrade() else { return };
    let why = match server.stderr() {
        Some(stderr) => format!("{why}: {stderr}"),
        None => why,
    };
    server.close_routes(&why);
}

/// Read stdout until `server listening on <url>` (T3: `/server listening on\s+(https?:\/\/\S+)/i`).
/// `opencode serve` ended (or its output did) before it said where it listens.
#[derive(Debug)]
struct ExitedEarly(String);

impl std::fmt::Display for ExitedEarly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ExitedEarly {}

async fn listening(stdout: &mut (impl AsyncBufRead + Unpin)) -> anyhow::Result<String> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if !read_line(stdout, &mut line).await? {
            bail!("opencode serve exited before it listened");
        }
        let text = String::from_utf8_lossy(&line);
        tracing::debug!(target: "agent_host::opencode", "stdout: {}", text.trim_end());
        if let Some(url) = listening_url(&text) {
            return Ok(url);
        }
    }
}

fn listening_url(line: &str) -> Option<String> {
    let at = line.to_ascii_lowercase().find("server listening on")?;
    let url = line[at + "server listening on".len()..].split_whitespace().next()?;
    (url.starts_with("http://") || url.starts_with("https://")).then(|| url.to_string())
}

async fn drain(mut stdout: impl AsyncBufRead + Unpin) {
    let mut line = Vec::new();
    loop {
        line.clear();
        match read_line(&mut stdout, &mut line).await {
            Ok(true) => tracing::debug!(target: "agent_host::opencode", "stdout: {}", String::from_utf8_lossy(&line).trim_end()),
            _ => return,
        }
    }
}

/// One line, at most [`MAX_LINE`] bytes kept (the rest of a longer line is skipped).
async fn read_line(r: &mut (impl AsyncBufRead + Unpin), buf: &mut Vec<u8>) -> std::io::Result<bool> {
    let mut any = false;
    loop {
        let chunk = r.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(any);
        }
        any = true;
        let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i, true),
            None => (chunk.len(), false),
        };
        let room = MAX_LINE.saturating_sub(buf.len());
        buf.extend_from_slice(&chunk[..take.min(room)]);
        r.consume(take + usize::from(done));
        if done {
            return Ok(true);
        }
    }
}

async fn stderr_loop(mut stderr: impl AsyncRead + Unpin, tail: Arc<Mutex<VecDeque<u8>>>) {
    let mut buf = [0u8; 4096];
    while let Ok(n) = stderr.read(&mut buf).await {
        if n == 0 {
            break;
        }
        tracing::debug!(target: "agent_host::opencode", "stderr: {}", String::from_utf8_lossy(&buf[..n]).trim_end());
        let mut tail = tail.lock().unwrap();
        tail.extend(&buf[..n]);
        let excess = tail.len().saturating_sub(STDERR_TAIL);
        tail.drain(..excess);
    }
}

fn tail_text(tail: &Mutex<VecDeque<u8>>) -> Option<String> {
    let tail = tail.lock().unwrap();
    let text = String::from_utf8_lossy(&tail.iter().copied().collect::<Vec<_>>()).trim().to_string();
    (!text.is_empty()).then_some(text)
}
