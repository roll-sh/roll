//! Docker end-to-end: roll code as a cluster of three real `rolld --code-only` containers
//! (deploy/code/Dockerfile, dev profile) on one private network, with Postgres
//! (`postgres:18-alpine`) for metadata and SeaweedFS for S3 (docs/code/cluster.md). Skipped
//! unless `ROLL_E2E=1`; `ROLL_E2E_KEEP=1` leaves the containers running.
//!
//! While traffic runs through all three nodes (concurrent API landings on several repos, real
//! `git push` to `refs/land/main`, `git clone` and `fetch`, reads), the test SIGKILLs the owner of
//! the busiest repo (and starts it again), rolls another node with SIGTERM (it must drain, exit 0
//! and come back), and restarts the Postgres container (the nodes must reconnect without
//! restarting). Then it asserts:
//!
//! - every acknowledged landing is on its branch exactly once: its changeset id appears once in
//!   the API log, and its operation's file is added by exactly one first-parent commit in a clone.
//!   No operation landed twice, acknowledged or not, and nothing landed that wasn't sent;
//! - the API log and git agree (the same first-parent commits), and a clone through each node has
//!   the same head, which is the API's `head_git`;
//! - every changeset that landed after the repo's webhook was created reached the webhook
//!   receiver, through exactly one delivery id (the land forwarder queued each landing once), and
//!   signed with the webhook's secret. A delivery id may arrive more than once only when an
//!   attempt was sent but its success couldn't be recorded: its node died or was stopping, or
//!   Postgres was down. So every repeated delivery id's first arrival must fall inside one of
//!   those disruption windows (the disruption, 2 s either side);
//! - `/v1/code/health` is 200 on every node;
//! - no operation failed after the client retry policy below (API landings, git pushes, clones and
//!   fetches, reads). Failed attempts before a retry are reported by cause.
//!
//! The client retry policy (what an agent or CI job talking to roll code through a load balancer
//! would do): at most 12 attempts per operation within 90 s; each attempt has a 30 s timeout (a
//! node holds a request for up to ~13 s while its repo moves) and goes to the next node, as a load
//! balancer without stickiness would; connection errors, timeouts and 5xx answers are retried
//! after 250 ms doubling to 4 s (or the answer's `Retry-After`, at most 4 s). Landings are made
//! idempotent by their content: each operation adds one file that only it writes, from a fixed
//! base, with the attempt number in it. A retry of an operation whose earlier attempt did land
//! can't land again: it conflicts on that file (a 409 naming it, or a git push rejected with it),
//! which the client takes as "an earlier attempt landed". Such a conflict when every earlier
//! attempt provably did nothing (connection refused, or a 503 with `x-roll-code-unavailable`) is a
//! failure.
//!
//! The client reaches the nodes at their addresses on the cluster's network (not through published
//! ports, whose docker-proxy accepts connections for a stopped container and then drops them). The
//! webhook receiver is a small Python server in a container on that network: this host's firewall
//! drops traffic from containers to the host, so a receiver in the test process couldn't be
//! reached. The test reads what it recorded over a published port.

use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const IMAGE: &str = "roll-code:e2e";
const TOKEN: &str = "e2e-admin-token-0123456789abcdef";
const CLUSTER_SECRET: &str = "e2e cluster secret, more than 16 bytes";
const SECRET_KEY: &str = "e2e-secret-key-0123456789abcdef0123456789";
const HOOK_SECRET: &str = "e2e-webhook-secret-0123456789";
const BUCKET: &str = "roll-code-e2e";
const NODES: [&str; 3] = ["n1", "n2", "n3"];
/// Repos and their API writers: the first is the busiest.
const REPOS: [(&str, usize); 4] = [("busy", 6), ("alpha", 2), ("beta", 2), ("gamma", 2)];
/// Repos that also get `git push`es (one pusher each).
const PUSHED: [&str; 2] = ["busy", "beta"];
const DRAIN_TIMEOUT_S: u64 = 30;

// The client retry policy (module docs).
const MAX_ATTEMPTS: usize = 12;
const OP_DEADLINE: Duration = Duration::from_secs(90);
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_TIMEOUT: Duration = Duration::from_secs(60);
/// Pause between one writer's landings, so a run lands thousands of changesets rather than
/// hundreds of thousands.
const LAND_PACE: Duration = Duration::from_millis(120);

/// The webhook receiver: records each POST (delivery id, event, changeset, signature check,
/// arrival time) and answers 200 at once; `GET` returns everything recorded.
const RECEIVER: &str = r#"
import hashlib, hmac, json, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
SECRET = sys.argv[1].encode()
lock = threading.Lock()
receipts = []
class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a):
        pass
    def reply(self, body, ctype):
        self.send_response(200)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length") or 0))
        ts = self.headers.get("x-roll-timestamp", "")
        want = "sha256=" + hmac.new(SECRET, ts.encode() + b"." + body, hashlib.sha256).hexdigest()
        try:
            doc = json.loads(body)
        except Exception:
            doc = {}
        data = doc.get("data") or {}
        r = {"delivery": self.headers.get("x-roll-delivery"), "event": self.headers.get("x-roll-event"),
             "repo": (doc.get("repo") or {}).get("full_name"), "changeset": data.get("changeset"), "landed_at": data.get("at"),
             "sig_ok": hmac.compare_digest(self.headers.get("x-roll-signature-256", ""), want),
             "at_ms": int(time.time() * 1000)}
        with lock:
            receipts.append(r)
        self.reply(b"ok", "text/plain")
    def do_GET(self):
        with lock:
            out = json.dumps(receipts).encode()
        self.reply(out, "application/json")
ThreadingHTTPServer(("0.0.0.0", 8080), H).serve_forever()
"#;

fn e2e() -> bool {
    std::env::var("ROLL_E2E").is_ok_and(|v| v == "1")
}

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker").args(args).output().expect("docker runs");
    assert!(out.status.success(), "docker {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn docker_try(args: &[&str]) -> Option<String> {
    let out = Command::new("docker").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn docker_ok(args: &[&str]) -> bool {
    docker_try(args).is_some()
}

/// A container's whole log (rolld logs to stdout, docker to stderr).
fn docker_logs(name: &str, tail: Option<usize>) -> String {
    let tail = tail.map(|n| n.to_string()).unwrap_or_else(|| "all".into());
    match Command::new("docker").args(["logs", "--tail", &tail, name]).output() {
        Ok(o) => format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)),
        Err(e) => format!("docker logs: {e}"),
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

// ── the cluster ──

/// Everything the run created, removed on drop (even when the test fails).
struct Env {
    tag: String,
    /// Each node's address on the cluster's network (it can change when a node restarts). The
    /// client talks to the nodes there, as a load balancer in the same network would: a published
    /// port goes through docker-proxy, which accepts connections for a stopped container and then
    /// drops them, which no real network does.
    addrs: Mutex<[String; 3]>,
    hooks_port: u16,
    s3_port: u16,
    dir: tempfile::TempDir,
}

impl Env {
    fn name(&self, what: &str) -> String {
        format!("roll-ce2e-{}-{what}", self.tag)
    }

    fn node(&self, i: usize) -> String {
        self.name(NODES[i])
    }

    fn net(&self) -> String {
        self.name("net")
    }

    fn start_node(&self, i: usize) {
        let n = NODES[i];
        let envs = [
            format!("ROLL_CODE_DATABASE=postgres://roll:roll@{}:5432/roll?sslmode=disable", self.name("pg")),
            "ROLL_CODE_DB_CONNECTIONS=8".into(),
            format!("ROLL_CODE_CLUSTER_SECRET={CLUSTER_SECRET}"),
            format!("ROLL_CODE_ADVERTISE_URL=http://{}:7070", self.node(i)),
            format!("ROLL_CODE_NODE_ID={n}"),
            format!("ROLL_CODE_DRAIN_TIMEOUT={DRAIN_TIMEOUT_S}"),
            format!("ROLL_CODE_TOKEN={TOKEN}"),
            format!("ROLL_SECRET_KEY={SECRET_KEY}"),
            format!("ROLL_CODE_PUBLIC_URL=http://{}:7070", self.node(i)),
            format!("ROLL_S3_BUCKET={BUCKET}"),
            format!("ROLL_S3_ENDPOINT=http://{}:8333", self.name("s3")),
            "AWS_REGION=us-east-1".into(),
            "AWS_ACCESS_KEY_ID=e2e".into(),
            "AWS_SECRET_ACCESS_KEY=e2e".into(),
            "ROLL_CODE_WEBHOOK_ALLOW_PRIVATE=1".into(),
            "RUST_LOG=info".into(),
        ];
        let (name, net) = (self.node(i), self.net());
        let mut args = vec!["run", "-d", "--name", &name, "--hostname", n, "--network", &net];
        for e in &envs {
            args.extend(["-e", e.as_str()]);
        }
        args.push(IMAGE);
        docker(&args);
        self.refresh(i);
    }

    /// Start node `i` again (after a kill or a stop): same container, same pinned id.
    fn restart_node(&self, i: usize) {
        docker(&["start", &self.node(i)]);
        self.refresh(i);
    }

    /// Look up node `i`'s address on the network.
    fn refresh(&self, i: usize) {
        let ip = docker(&["inspect", "-f", "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}", &self.node(i)]);
        assert!(!ip.is_empty(), "{} has no address", self.node(i));
        self.addrs.lock()[i] = ip;
    }

    fn url(&self, i: usize) -> String {
        format!("http://{}:7070", self.addrs.lock()[i])
    }

    fn git_url(&self, i: usize, repo: &str) -> String {
        format!("http://git:{TOKEN}@{}:7070/git/acme/{repo}.git", self.addrs.lock()[i])
    }

    fn psql(&self, sql: &str) -> Option<String> {
        docker_try(&["exec", &self.name("pg"), "psql", "-U", "roll", "-d", "roll", "-At", "-c", sql])
    }

    /// The node holding `key`'s lease now (on the database's clock), if any.
    fn lease_holder(&self, key: &str) -> Option<String> {
        let sql = format!(
            "SELECT node_id FROM code_control.code_leases WHERE key = '{key}' \
             AND expires_at > CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT)"
        );
        self.psql(&sql).filter(|s| !s.is_empty())
    }

    /// Unexpired repo leases `node` holds.
    fn leases_of(&self, node: &str) -> Option<usize> {
        let sql = format!(
            "SELECT count(*) FROM code_control.code_leases WHERE node_id = '{node}' AND key NOT LIKE 'singleton:%' \
             AND expires_at > CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT)"
        );
        self.psql(&sql)?.parse().ok()
    }

    fn pg_ready(&self) -> bool {
        docker_ok(&["exec", &self.name("pg"), "pg_isready", "-U", "roll", "-h", "127.0.0.1"])
    }

    /// `State.StartedAt` and `RestartCount` of a container.
    fn started(&self, name: &str) -> String {
        docker(&["inspect", "-f", "{{.State.StartedAt}} {{.RestartCount}}", name])
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let mut names: Vec<String> = (0..3).map(|i| self.node(i)).collect();
        names.extend(["pg", "s3", "hooks"].map(|w| self.name(w)));
        if std::thread::panicking() {
            for n in &names[..3] {
                eprintln!("──── {n} (last 200 lines) ────\n{}", docker_logs(n, Some(200)));
            }
        }
        if std::env::var("ROLL_E2E_KEEP").is_ok() {
            eprintln!("ROLL_E2E_KEEP: leaving {names:?} and {}", self.net());
            return;
        }
        for n in &names {
            let _ = docker_ok(&["rm", "-f", "-v", n]);
        }
        let _ = docker_ok(&["network", "rm", &self.net()]);
    }
}

// ── the client ──

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
enum Kind {
    Land,
    Push,
    Clone,
    Read,
}

#[derive(Default, Debug)]
struct KindStats {
    ops: u64,
    attempts: u64,
    retried: u64,
    failed: u64,
    recovered: u64,
    /// Failed attempts by cause.
    causes: BTreeMap<String, u64>,
}

/// A landing the client got an answer for: an API landing or a git push.
#[derive(Clone, Debug)]
struct Ack {
    repo: String,
    /// The message: `land <op>` or `push <op>`.
    op: String,
    /// The changeset now on the branch (API landings answered 200).
    changeset: Option<String>,
    git_commit: Option<String>,
    /// An earlier attempt landed (the retry conflicted on the operation's own file).
    recovered: bool,
    /// When the attempt that got the answer was sent, and when the answer came.
    sent: Instant,
    done: Instant,
}

/// Failed attempts of one kind: how many, and the first and last of them, in seconds since the
/// disruption they fell in began.
type Span = (u64, f64, f64);

struct Answer {
    status: u16,
    /// `x-roll-code-unavailable`: nothing ran.
    unavailable: bool,
    retry_after: Option<u64>,
    body: Value,
}

enum CallError {
    /// The connection was refused: nothing ran.
    Connect(String),
    /// Anything else (a reset, a timeout, a broken body): it may have run.
    Other(String),
}

struct World {
    env: Env,
    http: reqwest::Client,
    stop: AtomicBool,
    t0: Instant,
    stats: Mutex<HashMap<Kind, KindStats>>,
    acks: Mutex<Vec<Ack>>,
    /// Every landing sent, by repo (messages).
    sent: Mutex<HashMap<String, Vec<String>>>,
    problems: Mutex<Vec<String>>,
    /// Every failed attempt: when, what, through which node, why.
    failures: Mutex<Vec<(Instant, Kind, usize, String)>>,
}

impl World {
    async fn call(&self, node: usize, method: &str, path: &str, body: Option<&Value>) -> Result<Answer, CallError> {
        let mut b = self.http.request(method.parse().unwrap(), format!("{}{path}", self.env.url(node))).bearer_auth(TOKEN).timeout(ATTEMPT_TIMEOUT);
        if let Some(v) = body {
            b = b.header("content-type", "application/json").body(v.to_string());
        }
        let r = b.send().await.map_err(|e| if e.is_connect() { CallError::Connect(chain(&e)) } else { CallError::Other(chain(&e)) })?;
        let status = r.status().as_u16();
        let unavailable = r.headers().contains_key("x-roll-code-unavailable");
        let retry_after = r.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
        let bytes = r.bytes().await.map_err(|e| CallError::Other(format!("body: {}", chain(&e))))?;
        let body = serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
        Ok(Answer { status, unavailable, retry_after, body })
    }

    /// A request that must succeed (setup and checks), with the retry policy.
    async fn ok(&self, node: usize, method: &str, path: &str, body: Option<Value>) -> Value {
        let started = Instant::now();
        let mut last = String::new();
        for attempt in 0..MAX_ATTEMPTS {
            if started.elapsed() > OP_DEADLINE {
                break;
            }
            match self.call((node + attempt) % 3, method, path, body.as_ref()).await {
                Ok(a) if (200..300).contains(&a.status) => return a.body,
                Ok(a) if a.status >= 500 => last = format!("{} {}", a.status, a.body),
                Ok(a) => panic!("{method} {path}: {} {}", a.status, a.body),
                Err(CallError::Connect(e) | CallError::Other(e)) => last = e,
            }
            tokio::time::sleep(backoff(attempt, None)).await;
        }
        panic!("{method} {path} failed after retries: {last}");
    }

    fn stat(&self, kind: Kind, f: impl FnOnce(&mut KindStats)) {
        f(self.stats.lock().entry(kind).or_default());
    }

    /// A failed attempt through `node` (retried, unless the retry policy is spent).
    fn cause(&self, kind: Kind, node: usize, cause: impl Into<String>) {
        let cause = cause.into();
        self.failures.lock().push((Instant::now(), kind, node, cause.clone()));
        self.stat(kind, |s| *s.causes.entry(cause).or_default() += 1);
    }

    /// The full text of the last failed attempt, kept for the report.
    fn detail(&self, e: &str) {
        if let Some(f) = self.failures.lock().last_mut() {
            f.3 = format!("{}\t{e}", f.3);
        }
    }

    fn problem(&self, p: String) {
        eprintln!("PROBLEM: {p}");
        self.problems.lock().push(p);
    }

    fn at(&self, t: Instant) -> f64 {
        t.saturating_duration_since(self.t0).as_secs_f64()
    }
}

fn backoff(attempt: usize, retry_after: Option<u64>) -> Duration {
    let exp = Duration::from_millis(250 << attempt.min(4)).min(Duration::from_secs(4));
    retry_after.map(|s| Duration::from_secs(s).min(Duration::from_secs(4))).unwrap_or(exp)
}

/// A short cause for a failed attempt.
fn cause_of(a: &Answer) -> String {
    format!("{}{}", a.status, if a.unavailable { " unavailable" } else { "" })
}

/// An error with its sources, for the report (reqwest's own message doesn't say what failed).
fn chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        out.push_str(&format!(": {c}"));
        cur = c.source();
    }
    out
}

/// What went wrong with a request that may have reached the node.
fn transport_cause(e: &str) -> String {
    let e = e.to_ascii_lowercase();
    let what = if e.contains("timed out") {
        "timeout"
    } else if e.contains("closed before message completed") {
        "closed before the response"
    } else if e.contains("reset") {
        "reset"
    } else if e.starts_with("body") {
        "body cut"
    } else {
        "connection"
    };
    what.to_string()
}

/// One API landing, with the retry policy (module docs). `spread` picks the first node.
async fn land_op(w: &World, repo: &str, op: &str, base: &str, spread: usize) {
    let path = format!("ops/{op}.txt");
    let message = format!("land {op}");
    w.sent.lock().entry(repo.to_string()).or_default().push(message.clone());
    w.stat(Kind::Land, |s| s.ops += 1);
    let started = Instant::now();
    // Did an attempt fail in a way that may have landed it (anything but "nothing ran")?
    let mut maybe_landed = false;
    let mut last = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 && started.elapsed() > OP_DEADLINE {
            break;
        }
        if attempt == 1 {
            w.stat(Kind::Land, |s| s.retried += 1);
        }
        let node = (spread + attempt) % 3;
        let body = json!({ "base": base, "message": message, "files": { &path: { "content": format!("{op} attempt {attempt}\n") } } });
        let sent = Instant::now();
        w.stat(Kind::Land, |s| s.attempts += 1);
        let retry_after = match w.call(node, "POST", &format!("/v1/code/repos/acme/{repo}/land"), Some(&body)).await {
            Ok(a) if a.status == 200 && a.body["status"] == "landed" => {
                let s = |k: &str| a.body[k].as_str().map(str::to_string);
                let ack = Ack { repo: repo.into(), op: message, changeset: s("changeset"), git_commit: s("git_commit"), recovered: false, sent, done: Instant::now() };
                w.acks.lock().push(ack);
                return;
            }
            Ok(a) if a.status == 409 && a.body["paths"].as_array().is_some_and(|p| p.iter().any(|x| x == path.as_str())) => {
                if maybe_landed {
                    w.stat(Kind::Land, |s| s.recovered += 1);
                    w.acks.lock().push(Ack { repo: repo.into(), op: message, changeset: None, git_commit: None, recovered: true, sent, done: Instant::now() });
                } else {
                    w.stat(Kind::Land, |s| s.failed += 1);
                    w.problem(format!("{repo} {op}: conflict on its own file though no earlier attempt can have landed: {}", a.body));
                }
                return;
            }
            Ok(a) if a.status >= 500 => {
                maybe_landed |= !a.unavailable;
                w.cause(Kind::Land, node, cause_of(&a));
                last = format!("{} {}", a.status, a.body);
                a.retry_after
            }
            Ok(a) => {
                w.stat(Kind::Land, |s| s.failed += 1);
                w.problem(format!("{repo} {op}: attempt {attempt} via {}: {} {}", NODES[node], a.status, a.body));
                return;
            }
            Err(CallError::Connect(e)) => {
                w.cause(Kind::Land, node, "connect");
                last = e;
                None
            }
            Err(CallError::Other(e)) => {
                maybe_landed = true;
                w.cause(Kind::Land, node, transport_cause(&e));
                w.detail(&e);
                last = e;
                None
            }
        };
        tokio::time::sleep(backoff(attempt, retry_after)).await;
    }
    w.stat(Kind::Land, |s| s.failed += 1);
    w.problem(format!("{repo} {op}: no answer after the retry policy ({:?}): {last}", started.elapsed()));
}

async fn land_writer(w: Arc<World>, repo: &'static str, writer: usize, base: String) {
    let mut i = 0usize;
    while !w.stop.load(Ordering::SeqCst) {
        land_op(&w, repo, &format!("{repo}-w{writer}-{i}"), &base, writer + i).await;
        i += 1;
        tokio::time::sleep(LAND_PACE).await;
    }
}

// ── git ──

struct GitOut {
    ok: bool,
    out: String,
}

/// Run git in `dir` with an isolated configuration; killed after GIT_TIMEOUT.
async fn git(home: &Path, dir: &Path, args: &[&str]) -> GitOut {
    let mut c = tokio::process::Command::new("git");
    c.args(["-c", "init.defaultBranch=main", "-c", "user.name=Agent", "-c", "user.email=agent@example.com"])
        .args(["-c", "advice.detachedHead=false", "-c", "http.lowSpeedLimit=1000", "-c", "http.lowSpeedTime=20"])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .kill_on_drop(true);
    match tokio::time::timeout(GIT_TIMEOUT, c.output()).await {
        Ok(Ok(o)) => GitOut { ok: o.status.success(), out: format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)) },
        Ok(Err(e)) => GitOut { ok: false, out: format!("spawn: {e}") },
        Err(_) => GitOut { ok: false, out: "timed out".into() },
    }
}

/// Did a failed git command provably not reach a node?
fn git_not_sent(out: &str) -> bool {
    out.contains("Failed to connect") || out.contains("Couldn't connect") || out.contains("Connection refused")
}

fn git_cause(out: &str) -> String {
    if git_not_sent(out) {
        "connect".into()
    } else if let Some(i) = out.find("returned error: ") {
        format!("http {}", out[i + 16..].chars().take(3).collect::<String>())
    } else if out.contains("timed out") {
        "timeout".into()
    } else {
        out.lines().find(|l| l.contains("fatal") || l.contains("error")).unwrap_or("other").chars().take(100).collect()
    }
}

/// `git clone` (or, with `fetch`, fetch main into an existing clone and reset to it), with the
/// retry policy.
async fn clone_op(w: &World, home: &Path, repo: &str, dest: &Path, spread: usize, fetch: bool) -> bool {
    w.stat(Kind::Clone, |s| s.ops += 1);
    let started = Instant::now();
    let mut last = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 && started.elapsed() > OP_DEADLINE {
            break;
        }
        if attempt == 1 {
            w.stat(Kind::Clone, |s| s.retried += 1);
        }
        let node = (spread + attempt) % 3;
        let url = w.env.git_url(node, repo);
        w.stat(Kind::Clone, |s| s.attempts += 1);
        let o = if fetch {
            let f = git(home, dest, &["fetch", "-q", &url, "main"]).await;
            if f.ok { git(home, dest, &["reset", "-q", "--hard", "FETCH_HEAD"]).await } else { f }
        } else {
            let _ = std::fs::remove_dir_all(dest);
            git(home, home, &["clone", "-q", &url, dest.to_str().unwrap()]).await
        };
        if o.ok {
            return true;
        }
        w.cause(Kind::Clone, node, git_cause(&o.out));
        last = o.out;
        tokio::time::sleep(backoff(attempt, None)).await;
    }
    w.stat(Kind::Clone, |s| s.failed += 1);
    w.problem(format!("{} {repo} failed after the retry policy: {last}", if fetch { "fetch" } else { "clone" }));
    false
}

/// One `git push HEAD:refs/land/main` of a commit adding `git/<op>.txt`, with the retry policy
/// (each attempt is a new commit on the same base, the attempt number in the file).
async fn push_op(w: &World, home: &Path, dir: &Path, repo: &str, op: &str, spread: usize) {
    let path = format!("git/{op}.txt");
    let message = format!("push {op}");
    w.sent.lock().entry(repo.to_string()).or_default().push(message.clone());
    w.stat(Kind::Push, |s| s.ops += 1);
    let base = git(home, dir, &["rev-parse", "HEAD"]).await.out.trim().to_string();
    let started = Instant::now();
    let mut maybe_landed = false;
    let mut last = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 && started.elapsed() > OP_DEADLINE {
            break;
        }
        if attempt == 1 {
            w.stat(Kind::Push, |s| s.retried += 1);
        }
        assert!(git(home, dir, &["reset", "-q", "--hard", &base]).await.ok);
        std::fs::create_dir_all(dir.join("git")).unwrap();
        std::fs::write(dir.join(&path), format!("{op} attempt {attempt}\n")).unwrap();
        assert!(git(home, dir, &["add", "-A"]).await.ok);
        assert!(git(home, dir, &["commit", "-qm", &message]).await.ok);
        let node = (spread + attempt) % 3;
        let url = w.env.git_url(node, repo);
        let sent = Instant::now();
        w.stat(Kind::Push, |s| s.attempts += 1);
        let o = git(home, dir, &["push", "-q", &url, "HEAD:refs/land/main"]).await;
        if o.ok {
            w.acks.lock().push(Ack { repo: repo.into(), op: message, changeset: None, git_commit: None, recovered: false, sent, done: Instant::now() });
            return;
        }
        if o.out.contains("conflict") {
            if o.out.contains(&path) && maybe_landed {
                w.stat(Kind::Push, |s| s.recovered += 1);
                w.acks.lock().push(Ack { repo: repo.into(), op: message, changeset: None, git_commit: None, recovered: true, sent, done: Instant::now() });
            } else {
                // Every operation writes its own file: only an earlier attempt can conflict.
                w.stat(Kind::Push, |s| s.failed += 1);
                w.problem(format!("{repo} {op}: unexpected push conflict (an earlier attempt may have landed: {maybe_landed}): {}", o.out));
            }
            return;
        }
        maybe_landed |= !git_not_sent(&o.out);
        w.cause(Kind::Push, node, git_cause(&o.out));
        last = o.out;
        tokio::time::sleep(backoff(attempt, None)).await;
    }
    w.stat(Kind::Push, |s| s.failed += 1);
    w.problem(format!("{repo} {op}: push failed after the retry policy: {last}"));
}

/// Fetch main (through one node), push a commit on it (through another), until stopped.
async fn git_pusher(w: Arc<World>, repo: &'static str, p: usize) {
    let home = w.env.dir.path().join(format!("pusher-{repo}"));
    std::fs::create_dir_all(&home).unwrap();
    let dir = home.join("w");
    if !clone_op(&w, &home, repo, &dir, p, false).await {
        return;
    }
    let mut i = 0usize;
    while !w.stop.load(Ordering::SeqCst) {
        // A stale base would land too (the server rebases), but a retry must start from a commit
        // that isn't on the branch, so each push starts from a fresh fetch.
        if clone_op(&w, &home, repo, &dir, p + i + 1, true).await {
            push_op(&w, &home, &dir, repo, &format!("{repo}-g{p}-{i}"), p + i).await;
        }
        i += 1;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Fresh clones of every repo through every node in turn, until stopped.
async fn cloner(w: Arc<World>) {
    let home = w.env.dir.path().join("cloner");
    std::fs::create_dir_all(&home).unwrap();
    let mut i = 0usize;
    while !w.stop.load(Ordering::SeqCst) {
        let repo = REPOS[i % REPOS.len()].0;
        let dest = home.join(format!("c{i}"));
        if clone_op(&w, &home, repo, &dest, i, false).await {
            let fsck = git(&home, &dest, &["fsck", "--connectivity-only"]).await;
            if !fsck.ok {
                w.problem(format!("clone of {repo}: git fsck failed: {}", fsck.out));
            }
        }
        let _ = std::fs::remove_dir_all(&dest);
        i += 1;
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
}

/// Reads (refs, the root tree) of every repo through every node in turn, until stopped.
async fn reader(w: Arc<World>) {
    let mut i = 0usize;
    while !w.stop.load(Ordering::SeqCst) {
        let repo = REPOS[i % REPOS.len()].0;
        let path = if i.is_multiple_of(2) { format!("/v1/code/repos/acme/{repo}/refs") } else { format!("/v1/code/repos/acme/{repo}/tree/main/") };
        w.stat(Kind::Read, |s| s.ops += 1);
        let started = Instant::now();
        let mut done = false;
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 && started.elapsed() > OP_DEADLINE {
                break;
            }
            if attempt == 1 {
                w.stat(Kind::Read, |s| s.retried += 1);
            }
            w.stat(Kind::Read, |s| s.attempts += 1);
            let node = (i + attempt) % 3;
            let retry_after = match w.call(node, "GET", &path, None).await {
                Ok(a) if a.status == 200 => {
                    done = true;
                    break;
                }
                Ok(a) if a.status >= 500 => {
                    w.cause(Kind::Read, node, cause_of(&a));
                    a.retry_after
                }
                Ok(a) => {
                    w.problem(format!("GET {path}: {} {}", a.status, a.body));
                    break;
                }
                Err(CallError::Connect(_)) => {
                    w.cause(Kind::Read, node, "connect");
                    None
                }
                Err(CallError::Other(e)) => {
                    w.cause(Kind::Read, node, transport_cause(&e));
                    w.detail(&e);
                    None
                }
            };
            tokio::time::sleep(backoff(attempt, retry_after)).await;
        }
        if !done {
            w.stat(Kind::Read, |s| s.failed += 1);
            w.problem(format!("GET {path} failed after the retry policy"));
        }
        i += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ── checks ──

async fn health(w: &World, i: usize) -> Option<(u16, Value)> {
    let r = w.http.get(format!("{}/v1/code/health", w.env.url(i))).timeout(Duration::from_secs(3)).send().await.ok()?;
    let s = r.status().as_u16();
    Some((s, r.json().await.unwrap_or(Value::Null)))
}

/// Wait until node `i` answers 200 on its health check and sees `nodes` live nodes.
async fn wait_healthy(w: &World, i: usize, nodes: u64, within: Duration) -> Duration {
    let t = Instant::now();
    loop {
        if let Some((200, v)) = health(w, i).await
            && v["cluster"]["nodes"].as_u64() == Some(nodes)
        {
            return t.elapsed();
        }
        assert!(t.elapsed() < within, "{} never became healthy with {nodes} nodes: {:?}", NODES[i], health(w, i).await);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A repo's main, oldest first: (changeset, git commit, first line of the message).
async fn api_log(w: &World, node: usize, repo: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let q = cursor.as_ref().map(|c| format!("&cursor={c}")).unwrap_or_default();
        let v = w.ok(node, "GET", &format!("/v1/code/repos/acme/{repo}/log?rev=main&limit=500{q}"), None).await;
        for item in v["items"].as_array().unwrap() {
            let s = |k: &str| item[k].as_str().unwrap_or_default().to_string();
            out.push((s("id"), s("git_commit"), item["message"].as_str().unwrap_or_default().lines().next().unwrap_or_default().to_string()));
        }
        match v["cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    out.reverse();
    out
}

async fn receipts(w: &World) -> Option<Vec<Value>> {
    let url = format!("http://127.0.0.1:{}/receipts", w.env.hooks_port);
    let r = w.http.get(url).timeout(Duration::from_secs(10)).send().await.ok()?;
    r.json::<Vec<Value>>().await.ok()
}

struct Timeline {
    lines: Vec<(f64, String)>,
}

impl Timeline {
    fn add(&mut self, w: &World, at: Instant, line: String) {
        eprintln!("[{:7.2}s] {line}", w.at(at));
        self.lines.push((w.at(at), line));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn code_cluster_keeps_every_landing_through_kill_rolling_restart_and_postgres_restart() {
    if !e2e() {
        eprintln!("skipped: set ROLL_E2E=1 (needs Docker)");
        return;
    }
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let built = Instant::now();
    docker(&["build", "-q", "--build-arg", "PROFILE=dev", "-f", &format!("{root}/deploy/code/Dockerfile"), "-t", IMAGE, root]);
    eprintln!("image built in {:.1}s", built.elapsed().as_secs_f64());

    let tag = format!("{:x}{:05x}", std::process::id(), now_ms() % 0xfffff);
    let env = Env { tag, addrs: Mutex::default(), hooks_port: free_port(), s3_port: free_port(), dir: tempfile::tempdir().unwrap() };
    docker(&["network", "create", &env.net()]);
    let (pg, s3, hooks, net) = (env.name("pg"), env.name("s3"), env.name("hooks"), env.net());
    docker(&[
        "run", "-d", "--name", &pg, "--network", &net, "-e", "POSTGRES_USER=roll", "-e", "POSTGRES_PASSWORD=roll", "-e", "POSTGRES_DB=roll",
        "postgres:18-alpine", "-c", "max_connections=300",
    ]);
    let s3_port = format!("127.0.0.1:{}:8333", env.s3_port);
    docker(&["run", "-d", "--name", &s3, "--network", &net, "-p", &s3_port, "chrislusf/seaweedfs:latest", "server", "-s3"]);
    let hooks_port = format!("127.0.0.1:{}:8080", env.hooks_port);
    docker(&["run", "-d", "--name", &hooks, "--network", &net, "-p", &hooks_port, "python:3.12-slim", "python", "-u", "-c", RECEIVER, HOOK_SECRET]);
    let http = reqwest::Client::builder().no_proxy().connect_timeout(Duration::from_secs(3)).redirect(reqwest::redirect::Policy::none()).build().unwrap();

    let t = Instant::now();
    while !env.pg_ready() {
        assert!(t.elapsed() < Duration::from_secs(60), "postgres didn't start");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    // The bucket (SeaweedFS takes anonymous S3 requests).
    let t = Instant::now();
    loop {
        let r = http.put(format!("http://127.0.0.1:{}/{BUCKET}", env.s3_port)).timeout(Duration::from_secs(5)).send().await;
        if r.is_ok_and(|r| r.status().is_success() || r.status().as_u16() == 409) {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(60), "SeaweedFS never created the bucket");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    for i in 0..3 {
        env.start_node(i);
    }
    let w = Arc::new(World {
        env,
        http,
        stop: AtomicBool::new(false),
        t0: Instant::now(),
        stats: Mutex::default(),
        acks: Mutex::default(),
        sent: Mutex::default(),
        problems: Mutex::default(),
        failures: Mutex::default(),
    });
    for i in 0..3 {
        wait_healthy(&w, i, 3, Duration::from_secs(90)).await;
    }
    let t = Instant::now();
    while receipts(&w).await.is_none() {
        assert!(t.elapsed() < Duration::from_secs(30), "the webhook receiver never answered");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // Repos, each with one landing (the base of every API landing) and a webhook for `landed`.
    w.ok(0, "POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" }))).await;
    let mut ids = HashMap::new();
    let mut bases = HashMap::new();
    let mut before_hooks: HashMap<String, HashSet<String>> = HashMap::new();
    let hook_url = format!("http://{}:8080/hook", w.env.name("hooks"));
    for (k, (repo, _)) in REPOS.iter().enumerate() {
        let r = w.ok(k, "POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": repo, "source": "native" }))).await;
        ids.insert(*repo, r["id"].as_str().unwrap().to_string());
        let files = json!({ "README.md": { "content": format!("# {repo}\n") } });
        let init = w.ok(k + 1, "POST", &format!("/v1/code/repos/acme/{repo}/land"), Some(json!({ "message": "init", "files": files }))).await;
        bases.insert(*repo, init["changeset"].as_str().unwrap().to_string());
        let hook = json!({ "url": hook_url, "events": ["landed"], "secret": HOOK_SECRET });
        w.ok(k + 2, "POST", &format!("/v1/code/repos/acme/{repo}/webhooks"), Some(hook)).await;
        before_hooks.insert(repo.to_string(), api_log(&w, k, repo).await.into_iter().map(|(id, _, _)| id).collect());
    }

    // Traffic through every node until stopped.
    let mut tasks = Vec::new();
    for (repo, writers) in REPOS {
        for k in 0..writers {
            tasks.push(tokio::spawn(land_writer(w.clone(), repo, k, bases[repo].clone())));
        }
    }
    for (p, repo) in PUSHED.iter().enumerate() {
        tasks.push(tokio::spawn(git_pusher(w.clone(), repo, p)));
    }
    tasks.push(tokio::spawn(cloner(w.clone())));
    tasks.push(tokio::spawn(reader(w.clone())));
    let traffic_from = Instant::now();
    let mut tl = Timeline { lines: Vec::new() };
    tl.add(&w, traffic_from, "traffic starts".into());
    // Disruptions on the wall clock, 2 s either side: a webhook delivery id may arrive twice only
    // if its first arrival is inside one of them.
    let mut windows: Vec<(i64, i64, &str)> = Vec::new();
    // Each disruption from its start until everything it touched is back, for the error report.
    let mut phases: Vec<(&str, Instant, Instant)> = Vec::new();
    tokio::time::sleep(Duration::from_secs(10)).await;

    // 1. SIGKILL the owner of the busiest repo.
    let busy = &ids["busy"];
    let victim = loop {
        if let Some(n) = w.env.lease_holder(busy) {
            break NODES.iter().position(|x| *x == n).expect("a known node");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let kill_wall = now_ms();
    let kill_at = Instant::now();
    docker(&["kill", "-s", "KILL", &w.env.node(victim)]);
    // Times are measured from when the kill was sent; "after the kill" means sent once `docker
    // kill` returned (the process is gone by then).
    let killed = Instant::now();
    tl.add(&w, kill_at, format!("SIGKILL {} (owner of acme/busy; `docker kill` returned after {:.2}s)", NODES[victim], (killed - kill_at).as_secs_f64()));
    let (moved_to, moved_after) = loop {
        if let Some(n) = w.env.lease_holder(busy)
            && n != NODES[victim]
        {
            break (n, kill_at.elapsed());
        }
        assert!(killed.elapsed() < Duration::from_secs(40), "acme/busy never got a new owner after the kill");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    tl.add(&w, kill_at + moved_after, format!("acme/busy lease taken by {moved_to}, {:.2}s after the kill", moved_after.as_secs_f64()));
    // Served again: the first landing on acme/busy sent after the kill that was answered.
    let t = Instant::now();
    let served_after = loop {
        let first = w.acks.lock().iter().filter(|a| a.repo == "busy" && a.sent > killed).map(|a| a.done).min();
        if let Some(d) = first {
            break d - kill_at;
        }
        assert!(t.elapsed() < Duration::from_secs(60), "no landing on acme/busy succeeded after the kill");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    tl.add(&w, kill_at + served_after, format!("acme/busy served again: the first landing sent after the kill answered {:.2}s after it", served_after.as_secs_f64()));
    windows.push((kill_wall - 2000, now_ms() + 2000, "SIGKILL"));
    tokio::time::sleep(Duration::from_secs(3)).await;
    w.env.restart_node(victim);
    let restarted = Instant::now();
    let back = wait_healthy(&w, victim, 3, Duration::from_secs(60)).await;
    tl.add(&w, restarted + back, format!("{} started again (same pinned id), healthy after {:.2}s", NODES[victim], back.as_secs_f64()));
    phases.push(("SIGKILL", kill_at, restarted + back));
    tokio::time::sleep(Duration::from_secs(8)).await;

    // 2. Rolling restart: SIGTERM another node (the one owning most repos); it drains, exits 0,
    // and starts again.
    let rolled = (0..3).filter(|i| *i != victim).max_by_key(|i| w.env.leases_of(NODES[*i]).unwrap_or(0)).unwrap();
    let owned = w.env.leases_of(NODES[rolled]).unwrap_or(0);
    let stop_wall = now_ms();
    let stop_at = Instant::now();
    let stop_timeout = DRAIN_TIMEOUT_S + 30;
    tl.add(&w, stop_at, format!("SIGTERM {} (owns {owned} repos; stop timeout {stop_timeout}s)", NODES[rolled]));
    let name = w.env.node(rolled);
    let stopper = tokio::task::spawn_blocking(move || {
        docker(&["stop", "-t", &stop_timeout.to_string(), &name]);
        Instant::now()
    });
    // Its health check fails while it drains (it may be over before we look).
    let mut saw_draining = false;
    while !stopper.is_finished() {
        if let Some((503, v)) = health(&w, rolled).await
            && v["cluster"]["draining"] == true
        {
            saw_draining = true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let drained = stopper.await.unwrap() - stop_at;
    windows.push((stop_wall - 2000, now_ms() + 2000, "SIGTERM"));
    let exit = docker(&["inspect", "-f", "{{.State.ExitCode}}", &w.env.node(rolled)]);
    let logs = docker_logs(&w.env.node(rolled), None);
    tl.add(&w, stop_at + drained, format!("{} drained and exited ({exit}) {:.2}s after SIGTERM; health showed draining: {saw_draining}", NODES[rolled], drained.as_secs_f64()));
    assert_eq!(exit, "0", "a node stopped with SIGTERM exits by itself after draining (not killed at the stop timeout)");
    assert!(logs.contains("roll code: drained"), "the node logged its drain:\n{}", &logs[logs.len().saturating_sub(3000)..]);
    for l in logs.lines().filter(|l| l.contains("roll code: drain") || l.contains("in flight") || l.contains("shut down") || l.contains("WARN")) {
        eprintln!("    {} log: {l}", NODES[rolled]);
    }
    assert!(drained < Duration::from_secs(DRAIN_TIMEOUT_S + 5), "draining took {drained:?}");
    assert_eq!(w.env.leases_of(NODES[rolled]), Some(0), "a drained node holds no repo");
    w.env.restart_node(rolled);
    let restarted = Instant::now();
    let back = wait_healthy(&w, rolled, 3, Duration::from_secs(60)).await;
    tl.add(&w, restarted + back, format!("{} started again, healthy after {:.2}s", NODES[rolled], back.as_secs_f64()));
    phases.push(("SIGTERM", stop_at, restarted + back));
    tokio::time::sleep(Duration::from_secs(8)).await;

    // 3. Restart Postgres: the nodes reconnect without restarting.
    let node_starts: Vec<String> = (0..3).map(|i| w.env.started(&w.env.node(i))).collect();
    let pg_wall = now_ms();
    let pg_at = Instant::now();
    tl.add(&w, pg_at, "restart Postgres (docker restart -t 10)".into());
    docker(&["restart", "-t", "10", &w.env.name("pg")]);
    let pg_back = loop {
        if w.env.pg_ready() {
            break pg_at.elapsed();
        }
        assert!(pg_at.elapsed() < Duration::from_secs(60), "postgres didn't come back");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    tl.add(&w, pg_at + pg_back, format!("Postgres accepting connections {:.2}s after the restart began", pg_back.as_secs_f64()));
    let healthy_after = loop {
        let mut all = true;
        for i in 0..3 {
            all &= matches!(health(&w, i).await, Some((200, _)));
        }
        if all {
            break pg_at.elapsed();
        }
        assert!(pg_at.elapsed() < Duration::from_secs(90), "the nodes never became healthy after the Postgres restart");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    tl.add(&w, pg_at + healthy_after, format!("every node healthy again {:.2}s after the restart began", healthy_after.as_secs_f64()));
    let pg_up = pg_at + pg_back;
    let t = Instant::now();
    let (first_after, all_after) = loop {
        let (first, per_repo) = {
            let acks = w.acks.lock();
            let first = acks.iter().filter(|a| a.sent > pg_up).map(|a| a.done).min();
            let per_repo: Option<Vec<Instant>> = REPOS.iter().map(|(r, _)| acks.iter().filter(|a| a.repo == *r && a.sent > pg_up).map(|a| a.done).min()).collect();
            (first, per_repo)
        };
        if let (Some(f), Some(all)) = (first, per_repo) {
            break (f - pg_at, all.into_iter().max().unwrap() - pg_at);
        }
        assert!(t.elapsed() < Duration::from_secs(60), "some repo never landed again after the Postgres restart");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    windows.push((pg_wall - 2000, now_ms() + 2000, "Postgres restart"));
    tl.add(&w, pg_at + all_after, format!("landings recovered: the first {:.2}s, every repo {:.2}s after the restart began", first_after.as_secs_f64(), all_after.as_secs_f64()));
    phases.push(("Postgres restart", pg_at, pg_at + all_after));
    let node_starts_after: Vec<String> = (0..3).map(|i| w.env.started(&w.env.node(i))).collect();
    assert_eq!(node_starts, node_starts_after, "no node restarted because Postgres did");

    tokio::time::sleep(Duration::from_secs(8)).await;
    w.stop.store(true, Ordering::SeqCst);
    for t in tasks {
        t.await.unwrap();
    }
    let stopped = Instant::now();
    tl.add(&w, stopped, "traffic stopped".into());

    // ── the report ──
    let acks = w.acks.lock().clone();
    eprintln!("\n=== roll code cluster e2e: {:.0}s of traffic ===", (stopped - traffic_from).as_secs_f64());
    eprintln!("timeline (seconds since the nodes started):");
    for (at, line) in &tl.lines {
        eprintln!("  {at:7.2}  {line}");
    }
    let busy_max = acks.iter().filter(|a| a.repo == "busy").map(|a| a.done - a.sent).max().unwrap_or_default();
    eprintln!("longest successful attempt on acme/busy: {:.2}s", busy_max.as_secs_f64());
    eprintln!("client operations (failed = still failing after the retry policy):");
    {
        let stats = w.stats.lock();
        let mut kinds: Vec<_> = stats.iter().collect();
        kinds.sort_by_key(|(k, _)| **k);
        for (k, s) in kinds {
            let rate = if s.ops == 0 { 0.0 } else { s.failed as f64 / s.ops as f64 * 100.0 };
            eprintln!(
                "  {k:?}: {} ops, {} attempts, {} retried, {} recovered (an earlier attempt had landed), {} failed ({rate:.2}%); failed attempts by cause: {:?}",
                s.ops, s.attempts, s.retried, s.recovered, s.failed, s.causes
            );
        }
    }

    eprintln!("failed attempts (each retried) by disruption, through which node, why:");
    {
        let failures = w.failures.lock();
        let mut groups: BTreeMap<(usize, &str), BTreeMap<String, Span>> = BTreeMap::new();
        let mut samples: BTreeMap<(usize, &str), Vec<String>> = BTreeMap::new();
        for (at, kind, node, cause) in failures.iter() {
            let phase = phases.iter().enumerate().find(|(_, (_, a, b))| *at + Duration::from_secs(1) >= *a && *at <= *b + Duration::from_secs(2));
            let key = phase.map(|(i, (name, _, _))| (i, *name)).unwrap_or((usize::MAX, "outside any disruption"));
            let (short, detail) = cause.split_once('\t').unwrap_or((cause, ""));
            let since = phase.map(|(_, (_, a, _))| at.saturating_duration_since(*a).as_secs_f64()).unwrap_or(w.at(*at));
            let g = groups.entry(key).or_default().entry(format!("{kind:?} via {}: {short}", NODES[*node])).or_insert((0, f64::MAX, 0.0));
            *g = (g.0 + 1, g.1.min(since), g.2.max(since));
            let s = samples.entry(key).or_default();
            if !detail.is_empty() && s.len() < 4 {
                s.push(format!("+{since:.2}s {kind:?} via {}: {detail}", NODES[*node]));
            }
        }
        for (key, causes) in &groups {
            eprintln!("  {} (seconds since it began):", key.1);
            for (cause, (n, first, last)) in causes {
                eprintln!("    {n:4} × {cause} (+{first:.2}..+{last:.2})");
            }
            for line in samples.get(key).into_iter().flatten() {
                eprintln!("      e.g. {line}");
            }
        }
    }

    // ── every acknowledged landing is on its branch exactly once ──
    let mut logs = Vec::new();
    for (k, (repo, _)) in REPOS.iter().enumerate() {
        let log = api_log(&w, k, repo).await;
        let mut by_id: HashMap<&str, usize> = HashMap::new();
        let mut by_msg: HashMap<&str, usize> = HashMap::new();
        for (id, _, msg) in &log {
            *by_id.entry(id).or_default() += 1;
            *by_msg.entry(msg).or_default() += 1;
        }
        let sent: HashSet<String> = w.sent.lock().get(*repo).cloned().unwrap_or_default().into_iter().collect();
        for (msg, n) in &by_msg {
            if *msg != "init" {
                assert!(sent.contains(*msg), "acme/{repo}: {msg:?} is on main but was never sent");
            }
            assert_eq!(*n, 1, "acme/{repo}: {msg:?} landed {n} times");
        }
        let mine: Vec<&Ack> = acks.iter().filter(|a| a.repo == *repo).collect();
        for a in &mine {
            assert_eq!(by_msg.get(a.op.as_str()).copied(), Some(1), "acme/{repo}: acknowledged {a:?} isn't on main exactly once");
            if let Some(cs) = &a.changeset {
                assert_eq!(by_id.get(cs.as_str()).copied(), Some(1), "acme/{repo}: acknowledged changeset {cs} ({}) isn't on main once", a.op);
            }
        }
        // git agrees: a clone through each node has the same head, the API's head_git, and the
        // API's history; each operation's file was added by exactly one first-parent commit.
        let home = w.env.dir.path().join(format!("final-{repo}"));
        std::fs::create_dir_all(&home).unwrap();
        let mut heads = Vec::new();
        for (i, n) in NODES.iter().enumerate() {
            let dest = home.join(n);
            let o = git(&home, &home, &["clone", "-q", &w.env.git_url(i, repo), dest.to_str().unwrap()]).await;
            assert!(o.ok, "final clone of acme/{repo} through {n}: {}", o.out);
            heads.push(git(&home, &dest, &["rev-parse", "HEAD"]).await.out.trim().to_string());
        }
        assert!(heads.iter().all(|h| *h == heads[0]), "acme/{repo}: the nodes serve different heads: {heads:?}");
        let api_head = w.ok(k, "GET", &format!("/v1/code/repos/acme/{repo}"), None).await["head_git"].as_str().unwrap_or_default().to_string();
        assert_eq!(api_head, heads[0], "acme/{repo}: the API's head_git is the cloned head");
        let dest = home.join(NODES[0]);
        let fp: Vec<String> = git(&home, &dest, &["rev-list", "--first-parent", "--reverse", "HEAD"]).await.out.lines().map(str::to_string).collect();
        let api_commits: Vec<String> = log.iter().map(|(_, c, _)| c.clone()).collect();
        assert_eq!(fp, api_commits, "acme/{repo}: git's first-parent history is the API's log");
        let fp_set: HashSet<&str> = fp.iter().map(String::as_str).collect();
        for a in &mine {
            if let Some(c) = &a.git_commit {
                assert!(fp_set.contains(c.as_str()), "acme/{repo}: acknowledged commit {c} ({}) isn't on main", a.op);
            }
        }
        let added = git(&home, &dest, &["log", "--first-parent", "--diff-filter=A", "--name-only", "--format=", "HEAD"]).await.out;
        let mut adds: HashMap<&str, usize> = HashMap::new();
        for p in added.lines().filter(|l| l.starts_with("ops/") || l.starts_with("git/")) {
            *adds.entry(p).or_default() += 1;
        }
        for (p, n) in &adds {
            assert_eq!(*n, 1, "acme/{repo}: {p} was added {n} times");
        }
        for a in &mine {
            let (kind, op) = a.op.split_once(' ').unwrap();
            let p = format!("{}/{op}.txt", if kind == "land" { "ops" } else { "git" });
            assert_eq!(adds.get(p.as_str()).copied(), Some(1), "acme/{repo}: {p} of acknowledged {a:?} isn't added exactly once");
        }
        let n_api = mine.iter().filter(|a| a.op.starts_with("land ")).count();
        eprintln!(
            "acme/{repo}: {} changesets on main; {n_api} API landings and {} pushes acknowledged ({} of them recovered), each on main exactly once; head {} through every node",
            log.len(),
            mine.len() - n_api,
            mine.iter().filter(|a| a.recovered).count(),
            &heads[0][..12]
        );
        logs.push((repo.to_string(), log));
    }

    // ── webhooks: every landing after the webhook was created, through one delivery id ──
    let after_hooks: HashSet<(String, String)> = logs
        .iter()
        .flat_map(|(repo, log)| log.iter().filter(|(id, _, _)| !before_hooks[repo].contains(id)).map(move |(id, _, _)| (format!("acme/{repo}"), id.clone())))
        .collect();
    let on_main: HashSet<(String, String)> = logs.iter().flat_map(|(repo, log)| log.iter().map(move |(id, _, _)| (format!("acme/{repo}"), id.clone()))).collect();
    let key = |r: &Value| (r["repo"].as_str().unwrap_or_default().to_string(), r["changeset"].as_str().unwrap_or_default().to_string());
    let t = Instant::now();
    let (got, missing) = loop {
        let got = receipts(&w).await.unwrap_or_default();
        let seen: HashSet<(String, String)> = got.iter().filter(|r| r["event"] == "landed").map(key).collect();
        let missing: Vec<&(String, String)> = after_hooks.iter().filter(|k| !seen.contains(*k)).collect();
        if missing.is_empty() || t.elapsed() > Duration::from_secs(180) {
            break (got, missing.into_iter().cloned().collect::<Vec<_>>());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    let caught_up = t.elapsed();
    assert!(missing.is_empty(), "{} landings never reached the webhook (waited {caught_up:?}), e.g. {:?}", missing.len(), &missing[..missing.len().min(5)]);
    let landed: Vec<&Value> = got.iter().filter(|r| r["event"] == "landed").collect();
    assert!(landed.iter().all(|r| r["sig_ok"] == true), "every delivery is signed with the webhook's secret");
    let mut ids_per_cs: HashMap<(String, String), HashSet<String>> = HashMap::new();
    let mut arrivals: HashMap<String, Vec<i64>> = HashMap::new();
    for r in &landed {
        let k = key(r);
        assert!(on_main.contains(&k), "a delivery for {k:?}, which isn't on main");
        let did = r["delivery"].as_str().unwrap_or_default().to_string();
        ids_per_cs.entry(k).or_default().insert(did.clone());
        arrivals.entry(did).or_default().push(r["at_ms"].as_i64().unwrap_or_default());
    }
    for (cs, dids) in &ids_per_cs {
        assert_eq!(dids.len(), 1, "{cs:?} was queued as {} deliveries: {dids:?}", dids.len());
    }
    let repeated: Vec<(&String, &Vec<i64>)> = arrivals.iter().filter(|(_, at)| at.len() > 1).collect();
    for (did, at) in &repeated {
        let first = *at.iter().min().unwrap();
        let why = windows.iter().find(|(a, b, _)| (*a..=*b).contains(&first));
        eprintln!("delivery {did} arrived {} times, first during {:?}", at.len(), why.map(|w| w.2));
        assert!(why.is_some(), "delivery {did} arrived {} times, first at {first}, outside every disruption window {windows:?}", at.len());
    }
    // From landing (`data.at`, whole seconds) to first arrival.
    let mut lags: Vec<i64> = landed
        .iter()
        .filter_map(|r| Some(r["at_ms"].as_i64()? - r["landed_at"].as_i64()? * 1000))
        .collect();
    lags.sort_unstable();
    let pct = |p: f64| lags.get(((lags.len() as f64 - 1.0) * p) as usize).copied().unwrap_or_default() as f64 / 1000.0;
    eprintln!(
        "webhooks: {} `landed` deliveries received for {} changesets (every one of the {} landings after the webhooks were created); {} delivery ids arrived more than once; \
         landing to delivery p50 {:.1}s, p99 {:.1}s, max {:.1}s (landing times are whole seconds)",
        landed.len(),
        ids_per_cs.len(),
        after_hooks.len(),
        repeated.len(),
        pct(0.5),
        pct(0.99),
        pct(1.0),
    );
    eprintln!("{} changesets on main in all; {} acknowledged landings", on_main.len(), acks.len());

    // ── health, and no operation failed ──
    for (i, n) in NODES.iter().enumerate() {
        let h = health(&w, i).await;
        assert!(matches!(h, Some((200, _))), "{n} health: {h:?}");
    }
    let problems = w.problems.lock().clone();
    assert!(problems.is_empty(), "{} operations failed or misbehaved: {problems:#?}", problems.len());
    let stats = w.stats.lock();
    for (k, s) in stats.iter() {
        assert_eq!(s.failed, 0, "{k:?}: operations failed after the retry policy");
    }
    assert!(stats[&Kind::Land].ops > 500, "enough landings ran: {:?}", stats[&Kind::Land]);
    assert!(stats[&Kind::Push].ops > 20, "enough pushes ran: {:?}", stats[&Kind::Push]);
}
