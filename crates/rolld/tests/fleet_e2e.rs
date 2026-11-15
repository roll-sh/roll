//! roll cloud end to end: every product through a local fleet (deploy/cloud/local: three `rolld`
//! nodes from the fleet image with the cloud's node settings, DynamoDB Local, Postgres as the system
//! database, MinIO as S3, HAProxy in front as the NLB with PROXY protocol v2 and `/healthz` checks),
//! under steady load while nodes die and the fleet is rolled.
//!
//! Run it with `scripts/test-fleet.sh` (`pnpm test:fleet`), which builds the image, brings the
//! stack up, runs this test (`ROLL_E2E=1`, `--features fleet-e2e`) and tears everything down.
//! Without `ROLL_E2E=1` it skips. It drives the stack from this machine through the load
//! balancer's ports on 127.0.0.1, by the edge's hostnames (`*.roll.localhost`, which resolve to
//! loopback), with real clients:
//!
//! | product | client | writer (every acknowledged write recorded) | reader |
//! | --- | --- | --- | --- |
//! | code | HTTPS API on `api.`, git over `git.` | API landings, `git push` to `refs/land/main` | refs and trees; fetches |
//! | redis | RESP over TLS, SNI names the database | `SET` on a `sync` database | `GET` of acknowledged keys |
//! | nosql | CQL (nosql-proto's driver) over TLS, SNI names the database | `INSERT` on a `sync` database | `SELECT` of acknowledged rows |
//! | streams | librdkafka, SASL_SSL, `acks=all`, idempotent | records | a consumer group |
//! | storage | aws-sdk-s3, path and virtual-host style | `PutObject`, multipart uploads | `GetObject` of acknowledged keys |
//! | tasks | HTTPS on `tasks.` | publishes with a `dedup_id` | pull and ack |
//! | queues | lapin over AMQPS, the URL the API gave for a vhost (SNI `<vhost>.mq.…`) | publishes with confirms to a quorum queue | a consumer with manual acks |
//! | search | the Elasticsearch REST API on `<cluster>.search.…`, basic auth | `PUT` of documents by id | realtime `GET` of acknowledged documents |
//!
//! Under that load: (a) SIGKILL the node holding the most leases, then start it again; (b) a rolling
//! restart, each node in turn stopped with SIGTERM (it must drain and exit 0), started again and
//! healthy before the next, as the deploy pipeline does; (c) a fourth node joins, takes its share,
//! and is stopped with SIGTERM.
//!
//! Then: every acknowledged write is readable; nothing is duplicated where the product promises
//! exactly once (a code landing on its branch, a Kafka record from the idempotent producer, a
//! storage key), and at-least-once deliveries (tasks, queues) are deduplicable by message id; every
//! client-visible error is one the product's docs allow; no operation failed after the client's
//! retry policy. A table prints writes, acknowledgements, verified writes, errors by kind and the
//! longest stall per product; the test fails on any problem after printing it.
//!
//! The client retry policy (what the products' docs recommend, as in `code_cluster_e2e.rs`): up to
//! 12 attempts within 90 s, 30 s per attempt, backoff from 250 ms doubling to 4 s (or the answer's
//! `Retry-After`, at most 4 s); a reconnect after a dropped connection. Kafka retries inside
//! librdkafka (`message.timeout.ms` 90 s); a record whose delivery failed is sent again.
//!
//! `ROLL_E2E_KILL=<lease key>` (e.g. `tasks:cell:default`) makes the SIGKILL hit that lease's
//! holder rather than the node holding the most leases, to reproduce a finding.

use aws_sdk_s3::primitives::ByteStream;
use aws_smithy_runtime_api::client::dns::{DnsFuture, ResolveDns};
use futures_util::StreamExt;
use nosql_proto::client::{Client as CqlClient, ClientConfig as CqlConfig, ClientError, TlsConfig};
use nosql_proto::{QueryParams, ResultBody};
use parking_lot::Mutex;
use rdkafka::ClientContext;
use rdkafka::config::ClientConfig as KafkaConfig;
use rdkafka::consumer::{BaseConsumer, Consumer, ConsumerContext};
use rdkafka::error::KafkaError;
use rdkafka::message::Message;
use rdkafka::producer::{BaseRecord, DeliveryResult, Producer, ProducerContext, ThreadedProducer};
use rdkafka::types::RDKafkaErrorCode;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufStream};
use tokio::net::TcpStream;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};

/// deploy/cloud/local/docker-compose.yml's operator token.
const TOKEN: &str = "roll-cloud-e2e-operator-token-0123456789";
const DOMAIN: &str = "roll.localhost";
const SITE: &str = "rollsite.localhost";
const REGION: &str = "us-east-1";
const NODES: [&str; 4] = ["node1", "node2", "node3", "node4"];
/// Above the nodes' ROLL_DRAIN_TIMEOUT (30) plus the edge's drain delay (5): a stopping node
/// hands everything over and exits by itself.
const STOP_TIMEOUT_S: u64 = 60;

// The client retry policy (module docs).
const MAX_ATTEMPTS: usize = 12;
const OP_DEADLINE: Duration = Duration::from_secs(90);
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);

fn e2e() -> bool {
    std::env::var("ROLL_E2E").is_ok_and(|v| v == "1")
}

fn port(var: &str, default: u16) -> u16 {
    std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}

/// A small pseudo-random sequence (no `rand` among rolld's dev-dependencies).
struct Lcg(u64);

impl Lcg {
    fn new() -> Self {
        Lcg(now_ms() as u64 | 1)
    }
    fn below(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n.max(1)
    }
}

// ── docker ──

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker").args(args).output().expect("docker runs");
    assert!(out.status.success(), "docker {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn docker_try(args: &[&str]) -> Option<String> {
    let out = Command::new("docker").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn container(i: usize) -> String {
    format!("roll-cloud-e2e-{}", NODES[i])
}

fn compose_file() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/cloud/local/docker-compose.yml")
}

/// `docker compose` on the local fleet's file (the environment, ports and certificates included,
/// comes from scripts/test-fleet.sh).
fn compose(args: &[&str]) -> String {
    let file = compose_file();
    let mut all = vec!["compose", "-f", file.to_str().unwrap(), "--profile", "scale"];
    all.extend_from_slice(args);
    docker(&all)
}

// ── products and the scoreboard ──

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
enum Product {
    Code,
    Redis,
    Nosql,
    Kafka,
    Storage,
    Tasks,
    Queues,
    Search,
}

impl Product {
    fn name(self) -> &'static str {
        match self {
            Product::Code => "code",
            Product::Redis => "redis",
            Product::Nosql => "nosql",
            Product::Kafka => "streams",
            Product::Storage => "storage",
            Product::Tasks => "tasks",
            Product::Queues => "queues",
            Product::Search => "search",
        }
    }

    /// The client-visible errors this product's docs allow during a handoff or a crash. The first
    /// word of a kind is matched: `connect` (the connection or its TLS handshake failed: nothing
    /// was sent), `closed` (a connection dropped: the client reconnects), an HTTP status, a RESP
    /// error code, a Kafka error.
    ///
    /// - code (docs/code/http-api.md, "In a cluster"): reconnects, `503` with `Retry-After`
    ///   (nothing ran), and `502` for a write already forwarded to an owner that died (it may have
    ///   run: a landing sent again conflicts on its own paths);
    /// - redis ("Redis on the fleet"): connections close between commands; `-LOADING` past 13 s;
    /// - nosql (docs/nosql/architecture.md, "On a fleet"): connections close, drivers reconnect;
    /// - streams (crates/stream-broker/README.md, "On the fleet"): reconnects, and
    ///   `NOT_LEADER_OR_FOLLOWER`, `NOT_COORDINATOR`, `NOT_ENOUGH_REPLICAS` (retriable) when a
    ///   stream is lost; librdkafka retries them and refreshes its metadata;
    /// - storage (docs/storage/architecture.md, "Many nodes"): reconnects, `503` with
    ///   `Retry-After`, and `500 InternalError` for a request whose owner died while serving it
    ///   (the SDK retries it);
    /// - tasks (deploy/aws/README.md, "On the roll cloud fleet"): reconnects, `503` with
    ///   `Retry-After`, and `502` for a write whose connection broke at a dead leader (retried
    ///   with its `dedup_id`);
    /// - search (docs/search/architecture.md, "On the fleet"): reconnects, `503` with
    ///   `Retry-After` (no owner yet), and `502` for a write forwarded to an owner that died
    ///   (retried: a document `PUT` by id is idempotent);
    /// - queues (docs/queues/architecture.md, "Many nodes"): a connection closed (its node
    ///   stopped or died) and the reconnect; a publish in flight while its queue moves waits for
    ///   the new owner and is confirmed there, so a nack or a confirm timeout is never allowed.
    fn allows(self, kind: &str) -> bool {
        let k = kind.split_whitespace().next().unwrap_or_default();
        match self {
            Product::Storage => matches!(k, "connect" | "closed" | "503" | "500"),
            Product::Code | Product::Tasks | Product::Search => matches!(k, "connect" | "closed" | "503" | "502"),
            Product::Redis => matches!(k, "connect" | "closed" | "LOADING"),
            Product::Nosql => matches!(k, "connect" | "closed"),
            Product::Kafka => matches!(k, "reconnect" | "NotLeaderForPartition" | "NotCoordinator" | "NotEnoughReplicas" | "LeaderNotAvailable"),
            Product::Queues => matches!(k, "connect" | "closed"),
        }
    }
}

#[derive(Default)]
struct Stat {
    /// Writes the client started (logical operations, not attempts).
    writes: u64,
    acked: u64,
    verified: u64,
    /// Failed attempts (and other client-visible errors) by kind.
    errors: BTreeMap<String, u64>,
    /// When each write was acknowledged.
    acks: Vec<Instant>,
    /// Lines for the report.
    notes: Vec<String>,
}

#[derive(Clone, Copy)]
struct Ports {
    https: u16,
    redis: u16,
    kafka: u16,
    nosql: u16,
    amqps: u16,
    ddb: u16,
    stats: u16,
}

struct World {
    ports: Ports,
    certs: PathBuf,
    ca_pem: String,
    roots: rustls::RootCertStore,
    http: reqwest::Client,
    stop: AtomicBool,
    t0: Instant,
    stats: Mutex<BTreeMap<Product, Stat>>,
    /// Every client-visible error: when, which product, what.
    failures: Mutex<Vec<(Instant, Product, String)>>,
    problems: Mutex<Vec<String>>,
}

impl World {
    fn running(&self) -> bool {
        !self.stop.load(Ordering::SeqCst)
    }

    fn stat<T>(&self, p: Product, f: impl FnOnce(&mut Stat) -> T) -> T {
        f(self.stats.lock().entry(p).or_default())
    }

    fn write(&self, p: Product) {
        self.stat(p, |s| s.writes += 1);
    }

    fn ack(&self, p: Product) {
        let now = Instant::now();
        self.stat(p, |s| {
            s.acked += 1;
            s.acks.push(now);
        });
    }

    fn error(&self, p: Product, kind: &str) {
        self.failures.lock().push((Instant::now(), p, kind.to_string()));
        self.stat(p, |s| *s.errors.entry(kind.to_string()).or_default() += 1);
    }

    fn note(&self, p: Product, line: String) {
        eprintln!("{}: {line}", p.name());
        self.stat(p, |s| s.notes.push(line));
    }

    fn problem(&self, p: Product, msg: String) {
        let line = format!("{}: {msg}", p.name());
        eprintln!("PROBLEM: {line}");
        self.problems.lock().push(line);
    }

    fn at(&self, t: Instant) -> f64 {
        t.saturating_duration_since(self.t0).as_secs_f64()
    }

    fn api(&self, path: &str) -> String {
        format!("https://api.{DOMAIN}:{}{path}", self.ports.https)
    }

    fn tasks(&self, path: &str) -> String {
        format!("https://tasks.{REGION}.{DOMAIN}:{}{path}", self.ports.https)
    }

    fn search(&self, path: &str) -> String {
        format!("https://{SEARCH_CLUSTER}.search.{REGION}.{DOMAIN}:{}{path}", self.ports.https)
    }

    fn tls(&self) -> Arc<rustls::ClientConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        Arc::new(rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap().with_root_certificates(self.roots.clone()).with_no_client_auth())
    }
}

// ── the retry policy ──

fn backoff(attempt: usize, retry_after: Option<u64>) -> Duration {
    let exp = Duration::from_millis(250 << attempt.min(4)).min(Duration::from_secs(4));
    retry_after.map(|s| Duration::from_secs(s).min(Duration::from_secs(4))).unwrap_or(exp)
}

/// One attempt's outcome.
enum Step<T> {
    Done(T),
    /// Retry after counting this error kind (and the answer's `Retry-After`).
    Retry(String, Option<u64>),
    /// Stop: the operation failed in a way no retry fixes.
    Fail(String),
}

/// Run `f` under the retry policy; every failed attempt is counted against `p`.
async fn retry<T, F, Fut>(w: &World, p: Product, mut f: F) -> Result<T, String>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Step<T>>,
{
    let started = Instant::now();
    let mut last = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 && started.elapsed() > OP_DEADLINE {
            break;
        }
        let (kind, retry_after) = match tokio::time::timeout(ATTEMPT_TIMEOUT + Duration::from_secs(5), f(attempt)).await {
            Ok(Step::Done(t)) => return Ok(t),
            Ok(Step::Fail(e)) => return Err(e),
            Ok(Step::Retry(k, ra)) => (k, ra),
            Err(_) => ("timeout".to_string(), None),
        };
        w.error(p, &kind);
        last = kind;
        tokio::time::sleep(backoff(attempt, retry_after)).await;
    }
    Err(format!("no success within the retry policy ({:.1}s; last: {last})", started.elapsed().as_secs_f64()))
}

struct Answer {
    status: u16,
    retry_after: Option<u64>,
    /// `x-roll-code-unavailable`: nothing ran.
    unavailable: bool,
    body: Value,
    text: String,
}

fn transport(e: &reqwest::Error) -> String {
    // A connect that timed out (the load balancer still sending to a node that just died, as an
    // NLB does until its health check notices) is a connect error: nothing was sent.
    if e.is_connect() {
        "connect".into()
    } else if e.is_timeout() {
        "timeout".into()
    } else {
        "closed".into()
    }
}

async fn send(rb: reqwest::RequestBuilder) -> Result<Answer, String> {
    let r = rb.timeout(ATTEMPT_TIMEOUT).send().await.map_err(|e| transport(&e))?;
    let status = r.status().as_u16();
    let unavailable = r.headers().contains_key("x-roll-code-unavailable");
    let retry_after = r.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
    let bytes = r.bytes().await.map_err(|e| transport(&e))?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Ok(Answer { status, retry_after, unavailable, body, text })
}

/// A request that must succeed (setup and checks): retried on transport errors and 5xx, never
/// counted against a product.
async fn ok(w: &World, method: &str, url: &str, body: Option<Value>) -> Value {
    let started = Instant::now();
    let mut last = String::new();
    for attempt in 0..MAX_ATTEMPTS * 2 {
        if started.elapsed() > OP_DEADLINE * 2 {
            break;
        }
        let mut rb = w.http.request(method.parse().unwrap(), url).bearer_auth(TOKEN);
        if let Some(b) = &body {
            rb = rb.json(b);
        }
        match send(rb).await {
            Ok(a) if (200..300).contains(&a.status) => return a.body,
            Ok(a) if a.status >= 500 => last = format!("{} {}", a.status, a.text),
            Ok(a) => panic!("{method} {url}: {} {}", a.status, a.text),
            Err(e) => last = e,
        }
        tokio::time::sleep(backoff(attempt, None)).await;
    }
    panic!("{method} {url} failed after retries: {last}");
}

// ── roll code ──

#[derive(Clone, Debug)]
struct CodeAck {
    repo: String,
    /// `land <op>` or `push <op>`.
    op: String,
    changeset: Option<String>,
    recovered: bool,
}

#[derive(Default)]
struct CodeState {
    acks: Mutex<Vec<CodeAck>>,
    /// Every landing sent, by repo.
    sent: Mutex<HashMap<String, Vec<String>>>,
}

/// Four repos (one API writer each, and a pusher on the first): with the other products that's
/// nine tenant leases, enough that placement usually gives every node, the scale-out's too, some.
const CODE_REPOS: [&str; 4] = ["busy", "calm", "cool", "deep"];
const CODE_OWNER: &str = "fleet";

/// One API landing of a file only this operation writes (`code_cluster_e2e.rs`'s scheme: a retry
/// of a landing that did land conflicts on its own file, which the client takes as landed).
async fn code_land(w: &World, c: &CodeState, repo: &str, op: &str, base: &str) {
    let path = format!("ops/{op}.txt");
    let message = format!("land {op}");
    c.sent.lock().entry(repo.into()).or_default().push(message.clone());
    w.write(Product::Code);
    let maybe = AtomicBool::new(false);
    let url = w.api(&format!("/v1/code/repos/{CODE_OWNER}/{repo}/land"));
    let r = retry(w, Product::Code, |attempt| {
        let body = json!({ "base": base, "message": message, "files": { &path: { "content": format!("{op} attempt {attempt}\n") } } });
        let (maybe, path, url) = (&maybe, &path, &url);
        async move {
            match send(w.http.post(url).bearer_auth(TOKEN).json(&body)).await {
                Ok(a) if a.status == 200 && a.body["status"] == "landed" => Step::Done(a.body["changeset"].as_str().map(str::to_string)),
                Ok(a) if a.status == 409 && a.body["paths"].as_array().is_some_and(|p| p.iter().any(|x| x == path.as_str())) => {
                    if maybe.load(Ordering::SeqCst) {
                        Step::Done(None)
                    } else {
                        Step::Fail(format!("conflict on its own file though no earlier attempt can have landed: {}", a.text))
                    }
                }
                Ok(a) if a.status >= 500 => {
                    maybe.fetch_or(!a.unavailable, Ordering::SeqCst);
                    Step::Retry(a.status.to_string(), a.retry_after)
                }
                Ok(a) => Step::Fail(format!("{} {}", a.status, a.text)),
                Err(k) => {
                    maybe.fetch_or(k != "connect", Ordering::SeqCst);
                    Step::Retry(k, None)
                }
            }
        }
    })
    .await;
    match r {
        Ok(changeset) => {
            w.ack(Product::Code);
            let recovered = changeset.is_none();
            c.acks.lock().push(CodeAck { repo: repo.into(), op: message, changeset, recovered });
        }
        Err(e) => w.problem(Product::Code, format!("{repo} {op}: {e}")),
    }
}

async fn code_writer(w: Arc<World>, c: Arc<CodeState>, repo: &'static str, k: usize, base: String) {
    let mut i = 0;
    while w.running() {
        code_land(&w, &c, repo, &format!("{repo}-w{k}-{i}"), &base).await;
        i += 1;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

struct GitOut {
    ok: bool,
    out: String,
}

fn git_url(w: &World, repo: &str) -> String {
    format!("https://git:{TOKEN}@git.{DOMAIN}:{}/{CODE_OWNER}/{repo}.git", w.ports.https)
}

/// git with an isolated configuration, trusting the fleet's CA and sending `git.<domain>` to the
/// load balancer.
async fn git(w: &World, home: &Path, dir: &Path, args: &[&str]) -> GitOut {
    let resolve = format!("http.curloptResolve=git.{DOMAIN}:{}:127.0.0.1", w.ports.https);
    let ca = format!("http.sslCAInfo={}", w.certs.join("ca.crt").display());
    let mut c = tokio::process::Command::new("git");
    c.args(["-c", "init.defaultBranch=main", "-c", "user.name=Agent", "-c", "user.email=agent@example.com", "-c", &resolve, "-c", &ca])
        .args(["-c", "advice.detachedHead=false", "-c", "http.lowSpeedLimit=1000", "-c", "http.lowSpeedTime=20"])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(60), c.output()).await {
        Ok(Ok(o)) => GitOut { ok: o.status.success(), out: format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)) },
        Ok(Err(e)) => GitOut { ok: false, out: format!("spawn: {e}") },
        Err(_) => GitOut { ok: false, out: "timed out".into() },
    }
}

fn git_not_sent(out: &str) -> bool {
    out.contains("Failed to connect") || out.contains("Couldn't connect") || out.contains("Connection refused")
}

fn git_kind(out: &str) -> String {
    if git_not_sent(out) {
        "connect".into()
    } else if let Some(i) = out.find("returned error: ") {
        out[i + 16..].chars().take(3).collect()
    } else if out.contains("timed out") {
        "timeout".into()
    } else if ["SSL", "TLS", "unexpected eof", "Empty reply", "Recv failure", "reset", "RPC failed", "remote end hung up", "early EOF"].iter().any(|s| out.contains(s)) {
        "closed".into()
    } else {
        format!("git {}", out.lines().find(|l| l.contains("fatal") || l.contains("error")).unwrap_or("failed").chars().take(80).collect::<String>())
    }
}

/// Fetch main into `dir` (cloning it the first time) under the retry policy.
async fn git_fetch(w: &World, home: &Path, repo: &str, dir: &Path) -> bool {
    let url = git_url(w, repo);
    let r = retry(w, Product::Code, |_| async {
        let o = if dir.join(".git").exists() {
            let f = git(w, home, dir, &["fetch", "-q", &url, "main"]).await;
            if f.ok { git(w, home, dir, &["reset", "-q", "--hard", "FETCH_HEAD"]).await } else { f }
        } else {
            let _ = std::fs::remove_dir_all(dir);
            git(w, home, home, &["clone", "-q", &url, dir.to_str().unwrap()]).await
        };
        if o.ok { Step::Done(()) } else { Step::Retry(git_kind(&o.out), None) }
    })
    .await;
    if let Err(e) = &r {
        w.problem(Product::Code, format!("fetch of {repo}: {e}"));
    }
    r.is_ok()
}

/// One `git push HEAD:refs/land/main` adding `git/<op>.txt` on the fetched main.
async fn git_push(w: &World, c: &CodeState, home: &Path, dir: &Path, repo: &str, op: &str) {
    let path = format!("git/{op}.txt");
    let message = format!("push {op}");
    c.sent.lock().entry(repo.into()).or_default().push(message.clone());
    w.write(Product::Code);
    let base = git(w, home, dir, &["rev-parse", "HEAD"]).await.out.trim().to_string();
    let url = git_url(w, repo);
    let maybe = AtomicBool::new(false);
    let r = retry(w, Product::Code, |attempt| {
        let (path, message, base, url, maybe) = (&path, &message, &base, &url, &maybe);
        async move {
            for args in [&["reset", "-q", "--hard", base.as_str()][..], &["clean", "-qfd"][..]] {
                let o = git(w, home, dir, args).await;
                if !o.ok {
                    return Step::Fail(format!("git {args:?}: {}", o.out));
                }
            }
            std::fs::create_dir_all(dir.join("git")).unwrap();
            std::fs::write(dir.join(path), format!("{op} attempt {attempt}\n")).unwrap();
            for args in [&["add", "-A"][..], &["commit", "-qm", message.as_str()][..]] {
                let o = git(w, home, dir, args).await;
                if !o.ok {
                    return Step::Fail(format!("git {args:?}: {}", o.out));
                }
            }
            let o = git(w, home, dir, &["push", "-q", url, "HEAD:refs/land/main"]).await;
            if o.ok {
                return Step::Done(false);
            }
            if o.out.contains("conflict") {
                return if o.out.contains(path.as_str()) && maybe.load(Ordering::SeqCst) {
                    Step::Done(true)
                } else {
                    Step::Fail(format!("unexpected push conflict (an earlier attempt may have landed: {}): {}", maybe.load(Ordering::SeqCst), o.out))
                };
            }
            maybe.fetch_or(!git_not_sent(&o.out), Ordering::SeqCst);
            Step::Retry(git_kind(&o.out), None)
        }
    })
    .await;
    match r {
        Ok(recovered) => {
            w.ack(Product::Code);
            c.acks.lock().push(CodeAck { repo: repo.into(), op: message, changeset: None, recovered });
        }
        Err(e) => w.problem(Product::Code, format!("{repo} {op}: push: {e}")),
    }
}

async fn code_pusher(w: Arc<World>, c: Arc<CodeState>, scratch: PathBuf, repo: &'static str) {
    let home = scratch.join(format!("pusher-{repo}"));
    std::fs::create_dir_all(&home).unwrap();
    let dir = home.join("w");
    let mut i = 0;
    while w.running() {
        // A retry must start from a commit that isn't on the branch: each push starts from a fresh fetch.
        if git_fetch(&w, &home, repo, &dir).await {
            git_push(&w, &c, &home, &dir, repo, &format!("{repo}-g-{i}")).await;
        }
        i += 1;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

async fn code_reader(w: Arc<World>) {
    let mut i = 0usize;
    while w.running() {
        let repo = CODE_REPOS[i % CODE_REPOS.len()];
        let path = if i.is_multiple_of(2) { format!("/v1/code/repos/{CODE_OWNER}/{repo}/refs") } else { format!("/v1/code/repos/{CODE_OWNER}/{repo}/tree/main/") };
        let url = w.api(&path);
        let r = retry(&w, Product::Code, |_| async {
            match send(w.http.get(&url).bearer_auth(TOKEN)).await {
                Ok(a) if a.status == 200 => Step::Done(()),
                Ok(a) if a.status >= 500 => Step::Retry(a.status.to_string(), a.retry_after),
                Ok(a) => Step::Fail(format!("{} {}", a.status, a.text)),
                Err(k) => Step::Retry(k, None),
            }
        })
        .await;
        if let Err(e) = r {
            w.problem(Product::Code, format!("GET {path}: {e}"));
        }
        i += 1;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// A repo's main, oldest first: (changeset, git commit, first line of the message).
async fn code_log(w: &World, repo: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let q = cursor.as_ref().map(|c| format!("&cursor={c}")).unwrap_or_default();
        let v = ok(w, "GET", &w.api(&format!("/v1/code/repos/{CODE_OWNER}/{repo}/log?rev=main&limit=500{q}")), None).await;
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

/// Every acknowledged landing is on its branch exactly once, in the API's log and in git (a clone
/// through `git.`), and nothing landed that wasn't sent.
async fn code_verify(w: &World, c: &CodeState, scratch: &Path) {
    let acks = c.acks.lock().clone();
    let mut verified = 0;
    for repo in CODE_REPOS {
        let log = code_log(w, repo).await;
        let sent: HashSet<String> = c.sent.lock().get(repo).cloned().unwrap_or_default().into_iter().collect();
        let mut by_msg: HashMap<&str, usize> = HashMap::new();
        let mut by_id: HashMap<&str, usize> = HashMap::new();
        for (id, _, msg) in &log {
            *by_msg.entry(msg).or_default() += 1;
            *by_id.entry(id).or_default() += 1;
        }
        for (msg, n) in &by_msg {
            if *msg != "init" && !sent.contains(*msg) {
                w.problem(Product::Code, format!("{repo}: {msg:?} is on main but was never sent"));
            }
            if *n != 1 {
                w.problem(Product::Code, format!("{repo}: {msg:?} landed {n} times"));
            }
        }
        // git agrees: the first-parent history is the API's log, each operation's file is added once.
        let home = scratch.join(format!("verify-{repo}"));
        std::fs::create_dir_all(&home).unwrap();
        let dest = home.join("clone");
        if !git_fetch(w, &home, repo, &dest).await {
            continue;
        }
        let head = git(w, &home, &dest, &["rev-parse", "HEAD"]).await.out.trim().to_string();
        let api_head = ok(w, "GET", &w.api(&format!("/v1/code/repos/{CODE_OWNER}/{repo}")), None).await["head_git"].as_str().unwrap_or_default().to_string();
        if head != api_head {
            w.problem(Product::Code, format!("{repo}: git's head {head} isn't the API's head_git {api_head}"));
        }
        let fp: Vec<String> = git(w, &home, &dest, &["rev-list", "--first-parent", "--reverse", "HEAD"]).await.out.lines().map(str::to_string).collect();
        let api_commits: Vec<String> = log.iter().map(|(_, c, _)| c.clone()).collect();
        if fp != api_commits {
            w.problem(Product::Code, format!("{repo}: git's first-parent history ({} commits) isn't the API's log ({})", fp.len(), api_commits.len()));
        }
        let added = git(w, &home, &dest, &["log", "--first-parent", "--diff-filter=A", "--name-only", "--format=", "HEAD"]).await.out;
        let mut adds: HashMap<String, usize> = HashMap::new();
        for p in added.lines().filter(|l| l.starts_with("ops/") || l.starts_with("git/")) {
            *adds.entry(p.to_string()).or_default() += 1;
        }
        for a in acks.iter().filter(|a| a.repo == repo) {
            let (kind, op) = a.op.split_once(' ').unwrap();
            let file = format!("{}/{op}.txt", if kind == "land" { "ops" } else { "git" });
            let on_main = by_msg.get(a.op.as_str()).copied() == Some(1);
            let in_git = adds.get(&file).copied() == Some(1);
            let cs_ok = a.changeset.as_ref().is_none_or(|cs| by_id.get(cs.as_str()).copied() == Some(1));
            if on_main && in_git && cs_ok {
                verified += 1;
            } else {
                w.problem(Product::Code, format!("{repo}: acknowledged {a:?} isn't on main exactly once (log {on_main}, git {in_git}, changeset {cs_ok})"));
            }
        }
        w.note(Product::Code, format!("{repo}: {} changesets on main, {} acknowledged landings ({} recovered after an ambiguous attempt)", log.len(), acks.iter().filter(|a| a.repo == repo).count(), acks.iter().filter(|a| a.repo == repo && a.recovered).count()));
    }
    w.stat(Product::Code, |s| s.verified = verified);
}

// ── redis ──

type Tls = tokio_rustls::client::TlsStream<TcpStream>;

#[derive(Debug)]
enum Reply {
    Simple(String),
    Error(String),
    Bulk(Option<Vec<u8>>),
    /// An integer or an array: this client never needs their contents.
    Other,
}

/// A minimal RESP2 client over TLS.
struct Resp {
    io: BufStream<Tls>,
}

async fn tls_connect(w: &World, port: u16, server_name: &str) -> Result<Tls, String> {
    let tcp = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(("127.0.0.1", port))).await.map_err(|_| "connect".to_string())?.map_err(|_| "connect".to_string())?;
    let name = ServerName::try_from(server_name.to_string()).map_err(|e| format!("bad server name {server_name}: {e}"))?;
    tokio::time::timeout(Duration::from_secs(10), tokio_rustls::TlsConnector::from(w.tls()).connect(name, tcp)).await.map_err(|_| "connect".to_string())?.map_err(|_| "connect".to_string())
}

impl Resp {
    async fn connect(w: &World, r: &RedisState) -> Result<Self, String> {
        let io = tls_connect(w, w.ports.redis, &format!("{}.redis.{REGION}.{DOMAIN}", r.id)).await?;
        let mut c = Resp { io: BufStream::new(io) };
        match c.cmd(&[b"AUTH", b"default", r.password.as_bytes()]).await {
            Ok(Reply::Simple(s)) if s == "OK" => Ok(c),
            Ok(Reply::Error(e)) => Err(e.split_whitespace().next().unwrap_or("ERR").to_string()),
            Ok(other) => Err(format!("AUTH answered {other:?}")),
            // Closed before AUTH was answered (a node the database just left closes it): reconnect.
            Err(_) => Err("closed".into()),
        }
    }

    async fn send(&mut self, args: &[&[u8]]) -> std::io::Result<()> {
        let mut out = format!("*{}\r\n", args.len()).into_bytes();
        for a in args {
            out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
            out.extend_from_slice(a);
            out.extend_from_slice(b"\r\n");
        }
        self.io.write_all(&out).await?;
        self.io.flush().await
    }

    async fn read(&mut self) -> std::io::Result<Reply> {
        let mut line = String::new();
        if self.io.read_line(&mut line).await? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let line = line.trim_end_matches(['\r', '\n']);
        let (t, rest) = line.split_at(1.min(line.len()));
        let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad reply line {line:?}"));
        Ok(match t {
            "+" => Reply::Simple(rest.into()),
            "-" => Reply::Error(rest.into()),
            ":" => {
                rest.parse::<i64>().map_err(|_| bad())?;
                Reply::Other
            }
            "$" => {
                let n: i64 = rest.parse().map_err(|_| bad())?;
                if n < 0 {
                    Reply::Bulk(None)
                } else {
                    let mut buf = vec![0; n as usize + 2];
                    self.io.read_exact(&mut buf).await?;
                    buf.truncate(n as usize);
                    Reply::Bulk(Some(buf))
                }
            }
            "*" => {
                let n: i64 = rest.parse().map_err(|_| bad())?;
                for _ in 0..n.max(0) {
                    Box::pin(self.read()).await?;
                }
                Reply::Other
            }
            _ => return Err(bad()),
        })
    }

    async fn cmd(&mut self, args: &[&[u8]]) -> std::io::Result<Reply> {
        self.send(args).await?;
        self.read().await
    }
}

struct RedisState {
    id: String,
    password: String,
    /// Acknowledged writes: key → value.
    acks: Mutex<Vec<(String, String)>>,
}

/// One command on a shared connection (reconnecting first if it dropped), as one attempt.
async fn redis_attempt(w: &World, r: &RedisState, conn: &tokio::sync::Mutex<Option<Resp>>, args: &[&[u8]]) -> Result<Reply, String> {
    let mut g = conn.lock().await;
    if g.is_none() {
        *g = Some(Resp::connect(w, r).await?);
    }
    match g.as_mut().unwrap().cmd(args).await {
        Ok(Reply::Error(e)) => {
            *g = None;
            Err(e.split_whitespace().next().unwrap_or("ERR").to_string())
        }
        Ok(reply) => Ok(reply),
        Err(_) => {
            *g = None;
            Err("closed".into())
        }
    }
}

async fn redis_writer(w: Arc<World>, r: Arc<RedisState>) {
    let conn = tokio::sync::Mutex::new(None);
    let nonce = now_ms() % 100_000;
    let mut n = 0u64;
    while w.running() {
        let (key, val) = (format!("fleet:{n}"), format!("{n}-{nonce}"));
        w.write(Product::Redis);
        let res = retry(&w, Product::Redis, |_| async {
            match redis_attempt(&w, &r, &conn, &[b"SET", key.as_bytes(), val.as_bytes()]).await {
                Ok(Reply::Simple(s)) if s == "OK" => Step::Done(()),
                Ok(other) => Step::Fail(format!("SET answered {other:?}")),
                Err(k) => Step::Retry(k, None),
            }
        })
        .await;
        match res {
            Ok(()) => {
                w.ack(Product::Redis);
                r.acks.lock().push((key, val));
            }
            Err(e) => w.problem(Product::Redis, format!("SET {key}: {e}")),
        }
        n += 1;
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Reads back random acknowledged keys while the load runs: an acknowledged write that reads as
/// missing (even for a moment) is a problem.
async fn redis_reader(w: Arc<World>, r: Arc<RedisState>) {
    let conn = tokio::sync::Mutex::new(None);
    let mut rng = Lcg::new();
    while w.running() {
        let pick = {
            let acks = r.acks.lock();
            (!acks.is_empty()).then(|| acks[rng.below(acks.len())].clone())
        };
        if let Some((key, val)) = pick {
            let res = retry(&w, Product::Redis, |_| async {
                match redis_attempt(&w, &r, &conn, &[b"GET", key.as_bytes()]).await {
                    Ok(Reply::Bulk(v)) => Step::Done(v),
                    Ok(other) => Step::Fail(format!("GET answered {other:?}")),
                    Err(k) => Step::Retry(k, None),
                }
            })
            .await;
            match res {
                Ok(Some(v)) if v == val.as_bytes() => {}
                Ok(v) => w.problem(Product::Redis, format!("acknowledged {key} read back as {:?}", v.map(|v| String::from_utf8_lossy(&v).to_string()))),
                Err(e) => w.problem(Product::Redis, format!("GET {key}: {e}")),
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn redis_verify(w: &World, r: &RedisState) {
    let acks = r.acks.lock().clone();
    let mut verified = 0;
    let mut missing = Vec::new();
    for chunk in acks.chunks(200) {
        let mut got = None;
        for attempt in 0..MAX_ATTEMPTS {
            let res: Result<Vec<Reply>, String> = async {
                let mut c = Resp::connect(w, r).await?;
                for (k, _) in chunk {
                    c.send(&[b"GET", k.as_bytes()]).await.map_err(|e| e.to_string())?;
                }
                let mut out = Vec::new();
                for _ in chunk {
                    out.push(c.read().await.map_err(|e| e.to_string())?);
                }
                Ok(out)
            }
            .await;
            match res {
                Ok(v) => {
                    got = Some(v);
                    break;
                }
                Err(e) => {
                    eprintln!("redis verify: {e}; retrying");
                    tokio::time::sleep(backoff(attempt, None)).await;
                }
            }
        }
        let Some(replies) = got else {
            w.problem(Product::Redis, "couldn't read the acknowledged keys back".into());
            return;
        };
        for ((k, v), reply) in chunk.iter().zip(replies) {
            match reply {
                Reply::Bulk(Some(b)) if b == v.as_bytes() => verified += 1,
                other => missing.push(format!("{k} = {other:?}")),
            }
        }
    }
    if !missing.is_empty() {
        w.problem(Product::Redis, format!("{} acknowledged writes lost or changed, e.g. {:?}", missing.len(), &missing[..missing.len().min(5)]));
    }
    w.stat(Product::Redis, |s| s.verified = verified);
}

// ── nosql ──

struct NosqlState {
    server_name: String,
    user: String,
    password: String,
    acks: Mutex<Vec<(i64, String)>>,
}

fn cql_kind(e: &ClientError, connecting: bool) -> String {
    // A dropped connection shows as an I/O error, from the socket or from the framing reading it
    // (TLS without close_notify), or as `Closed`.
    let dropped = matches!(e, ClientError::Io(_) | ClientError::Closed | ClientError::Codec(nosql_proto::CodecError::Io(_)));
    match e {
        _ if dropped && connecting => "connect".into(),
        _ if dropped => "closed".into(),
        ClientError::Tls(_) => "connect".into(),
        ClientError::Timeout => "timeout".into(),
        ClientError::Server(b) => format!("server 0x{:04x} {}", b.code, b.message.chars().take(60).collect::<String>()),
        ClientError::Auth(m) => format!("auth {}", m.chars().take(60).collect::<String>()),
        other => format!("client {}", other.to_string().chars().take(60).collect::<String>()),
    }
}

async fn cql_connect(w: &World, s: &NosqlState) -> Result<CqlClient, String> {
    let tls = TlsConfig::with_roots(w.roots.clone(), &s.server_name).map_err(|e| format!("tls config: {e}"))?;
    let cfg = CqlConfig::new(format!("127.0.0.1:{}", w.ports.nosql)).tls(tls).credentials(&s.user, &s.password);
    CqlClient::connect(cfg).await.map_err(|e| cql_kind(&e, true))
}

/// One CQL statement on a shared connection (reconnecting first if it dropped), as one attempt.
async fn cql_attempt(w: &World, s: &NosqlState, conn: &tokio::sync::Mutex<Option<CqlClient>>, q: &str) -> Result<ResultBody, String> {
    let mut g = conn.lock().await;
    if g.is_none() {
        *g = Some(cql_connect(w, s).await?);
    }
    match g.as_ref().unwrap().query(q, QueryParams::default()).await {
        Ok(r) => Ok(r),
        Err(e) => {
            // Anything but an answer from the server leaves the connection in doubt: start over.
            if !matches!(e, ClientError::Server(_)) {
                *g = None;
            }
            Err(cql_kind(&e, false))
        }
    }
}

fn first_text(r: ResultBody) -> Option<String> {
    match r {
        ResultBody::Rows(rows) => rows.to_vec().into_iter().next().and_then(|row| row.into_iter().next().flatten()).map(|b| String::from_utf8_lossy(&b).to_string()),
        _ => None,
    }
}

async fn nosql_writer(w: Arc<World>, s: Arc<NosqlState>) {
    let conn = tokio::sync::Mutex::new(None);
    let nonce = now_ms() % 100_000;
    let mut n = 0i64;
    while w.running() {
        let val = format!("{n}-{nonce}");
        let q = format!("INSERT INTO fleet.t (k, v) VALUES ({n}, '{val}')");
        w.write(Product::Nosql);
        let res = retry(&w, Product::Nosql, |_| async {
            match cql_attempt(&w, &s, &conn, &q).await {
                Ok(_) => Step::Done(()),
                Err(k) => Step::Retry(k, None),
            }
        })
        .await;
        match res {
            Ok(()) => {
                w.ack(Product::Nosql);
                s.acks.lock().push((n, val));
            }
            Err(e) => w.problem(Product::Nosql, format!("INSERT {n}: {e}")),
        }
        n += 1;
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn nosql_reader(w: Arc<World>, s: Arc<NosqlState>) {
    let conn = tokio::sync::Mutex::new(None);
    let mut rng = Lcg::new();
    while w.running() {
        let pick = {
            let acks = s.acks.lock();
            (!acks.is_empty()).then(|| acks[rng.below(acks.len())].clone())
        };
        if let Some((k, val)) = pick {
            let q = format!("SELECT v FROM fleet.t WHERE k = {k}");
            let res = retry(&w, Product::Nosql, |_| async {
                match cql_attempt(&w, &s, &conn, &q).await {
                    Ok(r) => Step::Done(first_text(r)),
                    Err(e) => Step::Retry(e, None),
                }
            })
            .await;
            match res {
                Ok(Some(v)) if v == val => {}
                Ok(v) => w.problem(Product::Nosql, format!("acknowledged row {k} read back as {v:?}")),
                Err(e) => w.problem(Product::Nosql, format!("SELECT {k}: {e}")),
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn nosql_verify(w: &World, s: &NosqlState) {
    let acks = s.acks.lock().clone();
    let mut client = None;
    for attempt in 0..MAX_ATTEMPTS {
        match cql_connect(w, s).await {
            Ok(c) => {
                client = Some(c);
                break;
            }
            Err(e) => {
                eprintln!("nosql verify: connect: {e}");
                tokio::time::sleep(backoff(attempt, None)).await;
            }
        }
    }
    let Some(c) = client else {
        w.problem(Product::Nosql, "couldn't connect to read the acknowledged rows back".into());
        return;
    };
    let c = &c;
    let results: Vec<(i64, String, Result<Option<String>, String>)> = futures_util::stream::iter(acks)
        .map(|(k, v)| async move {
            let q = format!("SELECT v FROM fleet.t WHERE k = {k}");
            let mut last = Err(String::new());
            for attempt in 0..4 {
                last = c.query(&q, QueryParams::default()).await.map(first_text).map_err(|e| cql_kind(&e, false));
                if last.is_ok() {
                    break;
                }
                tokio::time::sleep(backoff(attempt, None)).await;
            }
            (k, v, last)
        })
        .buffer_unordered(32)
        .collect()
        .await;
    let mut verified = 0;
    let mut missing = Vec::new();
    for (k, v, r) in results {
        match r {
            Ok(Some(got)) if got == v => verified += 1,
            other => missing.push(format!("{k} = {other:?}")),
        }
    }
    if !missing.is_empty() {
        w.problem(Product::Nosql, format!("{} acknowledged rows lost or changed, e.g. {:?}", missing.len(), &missing[..missing.len().min(5)]));
    }
    w.stat(Product::Nosql, |st| st.verified = verified);
}

// ── streams (Kafka) ──

#[derive(Default)]
struct KafkaState {
    /// Acknowledged records: n → payload.
    acked: Mutex<HashMap<u64, String>>,
    payloads: Mutex<HashMap<u64, String>>,
    /// Records whose delivery failed, to send again.
    failed: Mutex<Vec<u64>>,
    /// Records sent more than once (after a failed delivery): duplicates of these are the client's.
    resent: Mutex<HashSet<u64>>,
    sent: AtomicU64,
    done: AtomicU64,
    consumed: AtomicU64,
}

#[derive(Clone)]
struct KafkaCtx {
    w: Arc<World>,
    k: Arc<KafkaState>,
}

fn kafka_kind(e: &KafkaError) -> String {
    match e.rdkafka_error_code() {
        Some(RDKafkaErrorCode::BrokerTransportFailure | RDKafkaErrorCode::AllBrokersDown | RDKafkaErrorCode::Resolve | RDKafkaErrorCode::BrokerNotAvailable) => "reconnect".into(),
        Some(c) => format!("{c:?}"),
        None => e.to_string(),
    }
}

impl ClientContext for KafkaCtx {
    fn error(&self, error: KafkaError, _reason: &str) {
        self.w.error(Product::Kafka, &kafka_kind(&error));
    }
}

impl ConsumerContext for KafkaCtx {}

impl ProducerContext for KafkaCtx {
    type DeliveryOpaque = ();
    fn delivery(&self, r: &DeliveryResult<'_>, _: ()) {
        self.k.done.fetch_add(1, Ordering::SeqCst);
        let n_of = |key: Option<&[u8]>| key.and_then(|k| std::str::from_utf8(k).ok()).and_then(|k| k.parse::<u64>().ok());
        match r {
            Ok(m) => {
                if let Some(n) = n_of(m.key()) {
                    let payload = m.payload().map(|p| String::from_utf8_lossy(p).to_string()).unwrap_or_default();
                    if self.k.acked.lock().insert(n, payload).is_none() {
                        self.w.ack(Product::Kafka);
                    }
                }
            }
            Err((e, m)) => {
                self.w.error(Product::Kafka, &kafka_kind(e));
                if let Some(n) = n_of(m.key()) {
                    self.k.failed.lock().push(n);
                }
            }
        }
    }
}

struct KafkaSetup {
    bootstrap: String,
    user: String,
    secret: String,
    topic: String,
}

fn kafka_config(w: &World, s: &KafkaSetup) -> KafkaConfig {
    let mut c = KafkaConfig::new();
    c.set("bootstrap.servers", &s.bootstrap)
        .set("broker.address.family", "v4")
        .set("security.protocol", "SASL_SSL")
        .set("ssl.ca.pem", &w.ca_pem)
        .set("sasl.mechanisms", "SCRAM-SHA-256")
        .set("sasl.username", &s.user)
        .set("sasl.password", &s.secret)
        .set("reconnect.backoff.ms", "100")
        .set("reconnect.backoff.max.ms", "1000");
    c
}

fn kafka_producer(w: &World, s: &KafkaSetup, ctx: KafkaCtx) -> ThreadedProducer<KafkaCtx> {
    let mut c = kafka_config(w, s);
    c.set("enable.idempotence", "true").set("acks", "all").set("linger.ms", "5").set("message.timeout.ms", OP_DEADLINE.as_millis().to_string());
    c.create_with_context(ctx).expect("kafka producer")
}

/// The producer thread: one record every 10 ms (at most 200 in flight), failed deliveries sent
/// again; then a flush.
fn kafka_writer(w: Arc<World>, k: Arc<KafkaState>, s: Arc<KafkaSetup>) {
    let ctx = KafkaCtx { w: w.clone(), k: k.clone() };
    let mut p = kafka_producer(&w, &s, ctx.clone());
    let nonce = now_ms() % 100_000;
    let mut n = 0u64;
    let send = |p: &ThreadedProducer<KafkaCtx>, n: u64, payload: &str| {
        let key = n.to_string();
        loop {
            match p.send(BaseRecord::to(&s.topic).key(&key).payload(payload)) {
                Ok(()) => {
                    k.sent.fetch_add(1, Ordering::SeqCst);
                    return;
                }
                Err((KafkaError::MessageProduction(RDKafkaErrorCode::QueueFull), _)) => std::thread::sleep(Duration::from_millis(5)),
                Err((e, _)) => {
                    w.error(Product::Kafka, &kafka_kind(&e));
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    };
    let mut flushing_since: Option<Instant> = None;
    loop {
        if let Some((code, reason)) = p.client().fatal_error() {
            // A fatal idempotent-producer error: the client must start a new producer, and what the
            // old one had in flight is sent again (it may then appear twice).
            w.problem(Product::Kafka, format!("the idempotent producer hit a fatal error: {code:?} {reason}"));
            let pending: Vec<u64> = k.payloads.lock().keys().copied().filter(|n| !k.acked.lock().contains_key(n)).collect();
            p = kafka_producer(&w, &s, ctx.clone());
            k.failed.lock().extend(pending);
        }
        let failed: Vec<u64> = std::mem::take(&mut *k.failed.lock());
        for f in failed {
            if k.acked.lock().contains_key(&f) {
                continue;
            }
            k.resent.lock().insert(f);
            let payload = k.payloads.lock()[&f].clone();
            send(&p, f, &payload);
        }
        let in_flight = k.sent.load(Ordering::SeqCst).saturating_sub(k.done.load(Ordering::SeqCst));
        if w.running() {
            if in_flight < 200 {
                let payload = format!("{n}-{nonce}");
                k.payloads.lock().insert(n, payload.clone());
                w.write(Product::Kafka);
                send(&p, n, &payload);
                n += 1;
            }
        } else {
            // Stopped: wait until every record is acknowledged or has failed for good.
            let since = *flushing_since.get_or_insert_with(Instant::now);
            let pending = k.payloads.lock().len() - k.acked.lock().len();
            if pending == 0 || since.elapsed() > OP_DEADLINE * 2 {
                if pending > 0 {
                    w.problem(Product::Kafka, format!("{pending} records never acknowledged after the writers stopped"));
                }
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = p.flush(Duration::from_secs(10));
}

/// A consumer group reading while the load runs.
fn kafka_reader(w: Arc<World>, k: Arc<KafkaState>, s: Arc<KafkaSetup>) {
    let mut c = kafka_config(&w, &s);
    c.set("group.id", "fleet-reader").set("auto.offset.reset", "earliest").set("enable.auto.commit", "true");
    let consumer: BaseConsumer<KafkaCtx> = c.create_with_context(KafkaCtx { w: w.clone(), k: k.clone() }).expect("kafka consumer");
    consumer.subscribe(&[&s.topic]).unwrap();
    while w.running() {
        match consumer.poll(Duration::from_millis(500)) {
            Some(Ok(_)) => {
                k.consumed.fetch_add(1, Ordering::Relaxed);
            }
            Some(Err(e)) => w.error(Product::Kafka, &kafka_kind(&e)),
            None => {}
        }
    }
}

/// Reads the whole topic from the start: every acknowledged record is there, once (records the
/// client sent twice after a failed delivery may appear twice: that's the client, not the broker).
fn kafka_verify(w: &World, k: &KafkaState, s: &KafkaSetup) {
    let mut c = kafka_config(w, s);
    c.set("group.id", "fleet-verify").set("enable.auto.commit", "false");
    let consumer: BaseConsumer = c.create().expect("kafka consumer");
    let md = consumer.fetch_metadata(Some(&s.topic), Duration::from_secs(30)).expect("metadata");
    let partitions: Vec<i32> = md.topics()[0].partitions().iter().map(|p| p.id()).collect();
    let mut tpl = rdkafka::TopicPartitionList::new();
    let mut high = HashMap::new();
    for p in &partitions {
        let (_, h) = consumer.fetch_watermarks(&s.topic, *p, Duration::from_secs(30)).expect("watermarks");
        high.insert(*p, h);
        tpl.add_partition_offset(&s.topic, *p, rdkafka::Offset::Beginning).unwrap();
    }
    consumer.assign(&tpl).unwrap();
    let mut seen: HashMap<u64, Vec<String>> = HashMap::new();
    let mut next: HashMap<i32, i64> = partitions.iter().map(|p| (*p, 0)).collect();
    let started = Instant::now();
    while next.iter().any(|(p, o)| *o < high[p]) && started.elapsed() < Duration::from_secs(120) {
        if let Some(Ok(m)) = consumer.poll(Duration::from_millis(500)) {
            next.insert(m.partition(), m.offset() + 1);
            if let Some(n) = m.key().and_then(|k| std::str::from_utf8(k).ok()).and_then(|k| k.parse().ok()) {
                seen.entry(n).or_default().push(m.payload().map(|p| String::from_utf8_lossy(p).to_string()).unwrap_or_default());
            }
        }
    }
    let acked = k.acked.lock().clone();
    let resent = k.resent.lock().clone();
    let mut verified = 0;
    let mut missing = Vec::new();
    for (n, payload) in &acked {
        match seen.get(n) {
            Some(v) if v.contains(payload) => verified += 1,
            other => missing.push(format!("{n} = {other:?}")),
        }
    }
    let dups: Vec<u64> = seen.iter().filter(|(n, v)| v.len() > 1 && !resent.contains(n)).map(|(n, _)| *n).collect();
    let client_dups = seen.iter().filter(|(n, v)| v.len() > 1 && resent.contains(n)).count();
    if !missing.is_empty() {
        w.problem(Product::Kafka, format!("{} acknowledged records missing, e.g. {:?}", missing.len(), &missing[..missing.len().min(5)]));
    }
    if !dups.is_empty() {
        w.problem(Product::Kafka, format!("{} records appear more than once though the idempotent producer sent each once, e.g. {:?}", dups.len(), &dups[..dups.len().min(5)]));
    }
    w.note(Product::Kafka, format!("{} records in the topic ({} partitions); the consumer group read {}; {} records sent again after a failed delivery ({client_dups} of them twice in the topic)", seen.values().map(Vec::len).sum::<usize>(), partitions.len(), k.consumed.load(Ordering::Relaxed), resent.len()));
    w.stat(Product::Kafka, |st| st.verified = verified);
}

// ── storage (S3) ──

/// Every hostname to the load balancer on 127.0.0.1 (the URL carries the port).
#[derive(Clone, Debug)]
struct Loopback;

impl ResolveDns for Loopback {
    fn resolve_dns<'a>(&'a self, _name: &'a str) -> DnsFuture<'a> {
        DnsFuture::ready(Ok(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]))
    }
}

fn s3_client(w: &World, key: &str, secret: &str, path_style: bool) -> aws_sdk_s3::Client {
    use aws_smithy_http_client::tls::{self, TlsContext, TrustStore, rustls_provider::CryptoMode};
    let tls = TlsContext::builder().with_trust_store(TrustStore::empty().with_pem_certificate(w.ca_pem.as_bytes())).build().unwrap();
    let http = aws_smithy_http_client::Builder::new().tls_provider(tls::Provider::Rustls(CryptoMode::AwsLc)).tls_context(tls).build_with_resolver(Loopback);
    let conf = aws_sdk_s3::Config::builder()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .region(aws_sdk_s3::config::Region::new(REGION))
        .endpoint_url(format!("https://s3.{REGION}.{SITE}:{}", w.ports.https))
        .force_path_style(path_style)
        .credentials_provider(aws_sdk_s3::config::Credentials::new(key, secret, None, None, "fleet"))
        .http_client(http)
        // The retry policy is the test's (every attempt counted), not the SDK's.
        .retry_config(aws_sdk_s3::config::retry::RetryConfig::disabled())
        .timeout_config(aws_sdk_s3::config::timeout::TimeoutConfig::builder().connect_timeout(Duration::from_secs(5)).operation_attempt_timeout(ATTEMPT_TIMEOUT).build())
        .build();
    aws_sdk_s3::Client::from_conf(conf)
}

/// An S3 error's kind and its `Retry-After`; `None` for answers no retry fixes (4xx).
fn s3_kind<E: std::fmt::Debug>(e: &aws_sdk_s3::error::SdkError<E, aws_smithy_runtime_api::http::Response>) -> (String, Option<u64>, bool) {
    use aws_sdk_s3::error::SdkError;
    match e {
        SdkError::TimeoutError(_) => ("timeout".into(), None, true),
        SdkError::DispatchFailure(d) => {
            let text = format!("{d:?}");
            let kind = if text.contains("Error(Connect") || text.contains("ConnectError") {
                "connect"
            } else if d.is_timeout() {
                "timeout"
            } else {
                "closed"
            };
            (kind.into(), None, true)
        }
        SdkError::ResponseError(_) => ("closed".into(), None, true),
        SdkError::ServiceError(s) => {
            let raw = s.raw();
            let status = raw.status().as_u16();
            let ra = raw.headers().get("retry-after").and_then(|v| v.parse().ok());
            (status.to_string(), ra, status >= 500)
        }
        other => (format!("sdk {:?}", other).chars().take(80).collect(), None, false),
    }
}

/// An S3 call as one attempt of the retry policy.
fn s3_step<T, E: std::fmt::Debug>(r: Result<T, aws_sdk_s3::error::SdkError<E, aws_smithy_runtime_api::http::Response>>) -> Step<T> {
    match r {
        Ok(t) => Step::Done(t),
        Err(e) => match s3_kind(&e) {
            (k, ra, true) => Step::Retry(k, ra),
            (k, _, false) => Step::Fail(format!("{k}: {e:?}").chars().take(400).collect()),
        },
    }
}

const BUCKET: &str = "fleet";

struct StorageState {
    path: aws_sdk_s3::Client,
    vhost: aws_sdk_s3::Client,
    /// Acknowledged small objects: key → body.
    acks: Mutex<Vec<(String, String)>>,
    /// Acknowledged multipart objects: key → n (the body is `multipart_body(n)`).
    multipart: Mutex<Vec<(String, u64)>>,
    /// Every key sent.
    sent: Mutex<HashSet<String>>,
}

fn multipart_body(n: u64, part: i32) -> Vec<u8> {
    let len = if part == 1 { 5 * 1024 * 1024 } else { 1024 };
    let pattern = format!("mp{n}-p{part}|");
    pattern.as_bytes().iter().copied().cycle().take(len).collect()
}

async fn s3_get(c: &aws_sdk_s3::Client, key: &str) -> Step<Vec<u8>> {
    match c.get_object().bucket(BUCKET).key(key).send().await {
        Ok(o) => match o.body.collect().await {
            Ok(b) => Step::Done(b.into_bytes().to_vec()),
            Err(_) => Step::Retry("closed".into(), None),
        },
        Err(e) => s3_step(Err(e)),
    }
}

async fn storage_writer(w: Arc<World>, s: Arc<StorageState>) {
    let nonce = now_ms() % 100_000;
    let mut n = 0u64;
    while w.running() {
        let key = format!("w/{n}");
        let body = format!("{n}-{nonce}");
        // Alternate path style and virtual-host style.
        let c = if n.is_multiple_of(2) { &s.path } else { &s.vhost };
        s.sent.lock().insert(key.clone());
        w.write(Product::Storage);
        let res = retry(&w, Product::Storage, |_| async { s3_step(c.put_object().bucket(BUCKET).key(&key).body(ByteStream::from(body.clone().into_bytes())).send().await) }).await;
        match res {
            Ok(_) => {
                w.ack(Product::Storage);
                s.acks.lock().push((key, body));
            }
            Err(e) => w.problem(Product::Storage, format!("PUT {key}: {e}")),
        }
        n += 1;
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

/// A multipart upload (a 5 MiB part and a small one) every 10 s, by virtual-host style.
async fn storage_multipart(w: Arc<World>, s: Arc<StorageState>) {
    let mut n = 0u64;
    let c = &s.vhost;
    while w.running() {
        let key = format!("mp/{n}");
        s.sent.lock().insert(key.clone());
        w.write(Product::Storage);
        let res: Result<(), String> = async {
            let up = retry(&w, Product::Storage, |_| async { s3_step(c.create_multipart_upload().bucket(BUCKET).key(&key).send().await) }).await?;
            let id = up.upload_id().unwrap_or_default().to_string();
            let mut parts = Vec::new();
            for part in [1, 2] {
                let body = multipart_body(n, part);
                let out = retry(&w, Product::Storage, |_| async { s3_step(c.upload_part().bucket(BUCKET).key(&key).upload_id(&id).part_number(part).body(ByteStream::from(body.clone())).send().await) }).await?;
                parts.push(aws_sdk_s3::types::CompletedPart::builder().part_number(part).set_e_tag(out.e_tag().map(str::to_string)).build());
            }
            let done = aws_sdk_s3::types::CompletedMultipartUpload::builder().set_parts(Some(parts)).build();
            let ambiguous = AtomicBool::new(false);
            retry(&w, Product::Storage, |_| async {
                match c.complete_multipart_upload().bucket(BUCKET).key(&key).upload_id(&id).multipart_upload(done.clone()).send().await {
                    Ok(_) => Step::Done(()),
                    // An earlier attempt completed it: the upload is gone and the object is there.
                    Err(e) if ambiguous.load(Ordering::SeqCst) && s3_kind(&e).0 == "404" => match s3_get(c, &key).await {
                        Step::Done(b) if b.len() == 5 * 1024 * 1024 + 1024 => Step::Done(()),
                        _ => Step::Fail(format!("complete answered 404 and the object isn't there: {e:?}")),
                    },
                    Err(e) => {
                        let (k, _, retryable) = s3_kind(&e);
                        ambiguous.fetch_or(retryable && k != "connect", Ordering::SeqCst);
                        s3_step(Err(e))
                    }
                }
            })
            .await
        }
        .await;
        match res {
            Ok(()) => {
                w.ack(Product::Storage);
                s.multipart.lock().push((key, n));
            }
            Err(e) => w.problem(Product::Storage, format!("multipart {key}: {e}")),
        }
        n += 1;
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn storage_reader(w: Arc<World>, s: Arc<StorageState>) {
    let mut rng = Lcg::new();
    let mut i = 0u64;
    while w.running() {
        let pick = {
            let acks = s.acks.lock();
            (!acks.is_empty()).then(|| acks[rng.below(acks.len())].clone())
        };
        if let Some((key, body)) = pick {
            let c = if i.is_multiple_of(2) { &s.vhost } else { &s.path };
            match retry(&w, Product::Storage, |_| s3_get(c, &key)).await {
                Ok(b) if b == body.as_bytes() => {}
                Ok(b) => w.problem(Product::Storage, format!("acknowledged {key} read back as {:?}", String::from_utf8_lossy(&b))),
                Err(e) => w.problem(Product::Storage, format!("GET {key}: {e}")),
            }
        }
        i += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn storage_verify(w: &World, s: &StorageState) {
    let acks = s.acks.lock().clone();
    let multipart = s.multipart.lock().clone();
    let c = &s.path;
    let mut verified = 0;
    let mut missing = Vec::new();
    let small: Vec<(String, bool)> = futures_util::stream::iter(acks)
        .map(|(key, body)| async move {
            let ok = matches!(retry(w, Product::Storage, |_| s3_get(c, &key)).await, Ok(b) if b == body.as_bytes());
            (key, ok)
        })
        .buffer_unordered(16)
        .collect()
        .await;
    for (key, good) in small {
        if good { verified += 1 } else { missing.push(key) }
    }
    for (key, n) in &multipart {
        let want: Vec<u8> = [multipart_body(*n, 1), multipart_body(*n, 2)].concat();
        match retry(w, Product::Storage, |_| s3_get(c, key)).await {
            Ok(b) if b == want => verified += 1,
            _ => missing.push(key.clone()),
        }
    }
    if !missing.is_empty() {
        w.problem(Product::Storage, format!("{} acknowledged objects lost or changed, e.g. {:?}", missing.len(), &missing[..missing.len().min(5)]));
    }
    // The listing: each key once, nothing that wasn't sent.
    let mut listed: HashMap<String, usize> = HashMap::new();
    let mut token: Option<String> = None;
    loop {
        let page = match retry(w, Product::Storage, |_| async { s3_step(c.list_objects_v2().bucket(BUCKET).set_continuation_token(token.clone()).send().await) }).await {
            Ok(p) => p,
            Err(e) => {
                w.problem(Product::Storage, format!("ListObjectsV2: {e}"));
                break;
            }
        };
        for o in page.contents() {
            *listed.entry(o.key().unwrap_or_default().to_string()).or_default() += 1;
        }
        match page.next_continuation_token() {
            Some(t) if page.is_truncated() == Some(true) => token = Some(t.to_string()),
            _ => break,
        }
    }
    let sent = s.sent.lock().clone();
    let twice: Vec<&String> = listed.iter().filter(|(_, n)| **n > 1).map(|(k, _)| k).collect();
    let unknown: Vec<&String> = listed.keys().filter(|k| !sent.contains(*k)).collect();
    if !twice.is_empty() {
        w.problem(Product::Storage, format!("{} keys listed more than once, e.g. {:?}", twice.len(), &twice[..twice.len().min(5)]));
    }
    if !unknown.is_empty() {
        w.problem(Product::Storage, format!("{} keys listed that were never sent, e.g. {:?}", unknown.len(), &unknown[..unknown.len().min(5)]));
    }
    w.note(Product::Storage, format!("{} keys listed; {} multipart uploads acknowledged", listed.len(), multipart.len()));
    w.stat(Product::Storage, |st| st.verified = verified);
}

// ── tasks ──

#[derive(Default)]
struct TasksState {
    /// Acknowledged publishes: op → message id.
    acks: Mutex<HashMap<u64, String>>,
    /// Each publish's attempts (outcome, and the answer when there was one), for the report on
    /// anything lost.
    trail: Mutex<HashMap<u64, Vec<String>>>,
    /// Deliveries: op → message ids delivered (one per delivery).
    delivered: Mutex<HashMap<u64, Vec<String>>>,
    /// The consumer runs until this is set (after the publishers stop and everything arrived).
    consumer_stop: AtomicBool,
}

const QUEUE: &str = "fleet";

async fn tasks_writer(w: Arc<World>, t: Arc<TasksState>) {
    let mut n = 0u64;
    let url = w.tasks("/v1/tasks/publish");
    while w.running() {
        let body = json!({ "queue": QUEUE, "body": format!("op-{n}"), "dedup_id": format!("fleet-{n}") });
        w.write(Product::Tasks);
        let trail = Mutex::new(Vec::new());
        let res = retry(&w, Product::Tasks, |_| async {
            let sent = w.at(Instant::now());
            let r = send(w.http.post(&url).bearer_auth(TOKEN).json(&body)).await;
            trail.lock().push(match &r {
                Ok(a) => format!("+{sent:.2}s {} {}", a.status, a.text.trim().chars().take(200).collect::<String>()),
                Err(k) => format!("+{sent:.2}s {k}"),
            });
            match r {
                Ok(a) if a.status == 201 || a.status == 200 => Step::Done(a.body["message_id"].as_str().unwrap_or_default().to_string()),
                Ok(a) if a.status >= 500 => Step::Retry(a.status.to_string(), a.retry_after),
                Ok(a) => Step::Fail(format!("{} {}", a.status, a.text)),
                Err(k) => Step::Retry(k, None),
            }
        })
        .await;
        t.trail.lock().insert(n, trail.into_inner());
        match res {
            Ok(id) => {
                w.ack(Product::Tasks);
                t.acks.lock().insert(n, id);
            }
            Err(e) => w.problem(Product::Tasks, format!("publish op-{n}: {e}")),
        }
        n += 1;
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// Pulls and acks until told to stop.
async fn tasks_consumer(w: Arc<World>, t: Arc<TasksState>) {
    let pull = w.tasks(&format!("/v1/tasks/queues/{QUEUE}/pull"));
    let ack = w.tasks(&format!("/v1/tasks/queues/{QUEUE}/ack"));
    while !t.consumer_stop.load(Ordering::SeqCst) {
        let res = retry(&w, Product::Tasks, |_| async {
            match send(w.http.post(&pull).bearer_auth(TOKEN).json(&json!({ "max": 50, "visibility": "30s", "wait": "2s" }))).await {
                Ok(a) if a.status == 200 => Step::Done(a.body["messages"].as_array().cloned().unwrap_or_default()),
                Ok(a) if a.status >= 500 => Step::Retry(a.status.to_string(), a.retry_after),
                Ok(a) => Step::Fail(format!("{} {}", a.status, a.text)),
                Err(k) => Step::Retry(k, None),
            }
        })
        .await;
        let messages = match res {
            Ok(m) => m,
            Err(e) => {
                w.problem(Product::Tasks, format!("pull: {e}"));
                continue;
            }
        };
        if messages.is_empty() {
            continue;
        }
        let mut receipts = Vec::new();
        for m in &messages {
            let body = m["body"].as_str().unwrap_or_default();
            match body.strip_prefix("op-").and_then(|n| n.parse::<u64>().ok()) {
                Some(n) => t.delivered.lock().entry(n).or_default().push(m["id"].as_str().unwrap_or_default().to_string()),
                None => w.problem(Product::Tasks, format!("a delivery with an unknown body: {m}")),
            }
            receipts.push(m["receipt"].clone());
        }
        let first = AtomicBool::new(true);
        let res = retry(&w, Product::Tasks, |_| async {
            let first = first.swap(false, Ordering::SeqCst);
            match send(w.http.post(&ack).bearer_auth(TOKEN).json(&json!({ "receipts": receipts }))).await {
                Ok(a) if a.status == 200 => Step::Done(()),
                // A retry of an ack that did apply: the receipts are spent.
                Ok(a) if a.status == 409 && !first => Step::Done(()),
                Ok(a) if a.status >= 500 => Step::Retry(a.status.to_string(), a.retry_after),
                Ok(a) => Step::Fail(format!("{} {}", a.status, a.text)),
                Err(k) => Step::Retry(k, None),
            }
        })
        .await;
        if let Err(e) = res {
            // Not lost: the messages come back after their visibility timeout.
            w.problem(Product::Tasks, format!("ack: {e}"));
        }
    }
}

/// Every acknowledged publish was delivered, as the message id the publish returned (duplicate
/// deliveries are allowed: at least once, deduplicable by that id). What happened to a missing
/// one (its publish attempts, and what the API says about its message) goes in the report.
async fn tasks_verify(w: &World, t: &TasksState) {
    let acks = t.acks.lock().clone();
    let delivered = t.delivered.lock().clone();
    let mut verified = 0;
    let mut missing = Vec::new();
    let mut other_ids = Vec::new();
    for (n, id) in &acks {
        match delivered.get(n) {
            Some(ids) if ids.iter().all(|d| d == id) => verified += 1,
            Some(ids) => other_ids.push(format!("op-{n}: published as {id}, delivered as {ids:?}")),
            None => missing.push(*n),
        }
    }
    if !missing.is_empty() {
        missing.sort_unstable();
        w.problem(Product::Tasks, format!("{} acknowledged publishes never delivered, e.g. op-{:?}", missing.len(), &missing[..missing.len().min(5)]));
        for n in missing.iter().take(5) {
            let id = &acks[n];
            let state = send(w.http.get(w.tasks(&format!("/v1/tasks/messages/{id}"))).bearer_auth(TOKEN)).await.map(|a| format!("{} {}", a.status, a.text)).unwrap_or_else(|e| e);
            let trail = t.trail.lock().get(n).cloned().unwrap_or_default();
            w.note(Product::Tasks, format!("lost op-{n}: published as {id}; attempts {trail:?}; GET /v1/tasks/messages/{id} → {state}"));
        }
    }
    if !other_ids.is_empty() {
        w.problem(Product::Tasks, format!("{} publishes delivered under another message id (dedup_id didn't deduplicate), e.g. {:?}", other_ids.len(), &other_ids[..other_ids.len().min(3)]));
    }
    let redelivered = delivered.values().filter(|v| v.len() > 1).count();
    w.note(Product::Tasks, format!("{} messages delivered, {redelivered} more than once (at least once; deduplicable by message id)", delivered.len()));
    w.stat(Product::Tasks, |st| st.verified = verified);
}

// ── search ──

const SEARCH_CLUSTER: &str = "fleet";
const SEARCH_INDEX: &str = "events";
const SEARCH_WRITERS: u64 = 2;

/// A search cluster (its password from the control plane) and an index in it, reached by the
/// edge's hostname for the cluster.
struct SearchState {
    password: String,
    /// Acknowledged documents (ids; writer `k` writes `k << 40 | n`).
    acks: Mutex<Vec<u64>>,
}

/// One search request under the retry policy: 2xx is done, 5xx and transport errors retry.
async fn search_call(w: &World, s: &SearchState, method: reqwest::Method, path: &str, body: &Value, p: Option<Product>) -> Result<Value, String> {
    let url = w.search(path);
    let attempt = |_| async {
        let mut rb = w.http.request(method.clone(), &url).basic_auth(SEARCH_CLUSTER, Some(&s.password));
        // A GET with a body is refused, as Elasticsearch refuses it.
        if !body.is_null() {
            rb = rb.json(body);
        }
        match send(rb).await {
            Ok(a) if (200..300).contains(&a.status) => Step::Done(a.body),
            Ok(a) if a.status >= 500 => Step::Retry(a.status.to_string(), a.retry_after),
            Ok(a) => Step::Fail(format!("{} {}", a.status, a.text)),
            Err(k) => Step::Retry(k, None),
        }
    };
    match p {
        Some(p) => retry(w, p, attempt).await,
        // Setup and checks: retried, never counted against the product.
        None => {
            let started = Instant::now();
            let mut last = String::new();
            for n in 0..MAX_ATTEMPTS * 2 {
                match attempt(n).await {
                    Step::Done(v) => return Ok(v),
                    Step::Fail(e) => return Err(e),
                    Step::Retry(k, _) if started.elapsed() >= OP_DEADLINE * 2 => return Err(k),
                    Step::Retry(k, ra) => {
                        last = k;
                        tokio::time::sleep(backoff(n, ra)).await;
                    }
                }
            }
            Err(format!("no success within the retry policy (last: {last})"))
        }
    }
}

async fn search_writer(w: Arc<World>, s: Arc<SearchState>, k: u64) {
    let mut n = 0u64;
    while w.running() {
        let id = k << 40 | n;
        w.write(Product::Search);
        // A retry of a write that did apply overwrites the same document (`200`).
        match search_call(&w, &s, reqwest::Method::PUT, &format!("/{SEARCH_INDEX}/_doc/{id}"), &json!({ "n": id, "writer": k }), Some(Product::Search)).await {
            Ok(_) => {
                w.ack(Product::Search);
                s.acks.lock().push(id);
            }
            Err(e) => w.problem(Product::Search, format!("index {id}: {e}")),
        }
        n += 1;
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Realtime `GET`s of acknowledged documents: each one must be found.
async fn search_reader(w: Arc<World>, s: Arc<SearchState>) {
    let mut rng = Lcg::new();
    while w.running() {
        let id = {
            let acks = s.acks.lock();
            (!acks.is_empty()).then(|| acks[rng.below(acks.len())])
        };
        if let Some(id) = id {
            match search_call(&w, &s, reqwest::Method::GET, &format!("/{SEARCH_INDEX}/_doc/{id}"), &Value::Null, Some(Product::Search)).await {
                Ok(d) if d["found"] == true && d["_source"]["n"] == id => {}
                Ok(d) => w.problem(Product::Search, format!("acknowledged document {id} read back as {d}")),
                // A 404 for an acknowledged id fails the attempt: it is lost.
                Err(e) => w.problem(Product::Search, format!("GET {id}: {e}")),
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Every acknowledged document is there (`_mget`, realtime), and a search after a refresh counts
/// at least as many.
async fn search_verify(w: &World, s: &SearchState) {
    let acks = s.acks.lock().clone();
    let mut verified = 0u64;
    let mut missing = Vec::new();
    for chunk in acks.chunks(500) {
        let ids: Vec<String> = chunk.iter().map(u64::to_string).collect();
        match search_call(w, s, reqwest::Method::POST, &format!("/{SEARCH_INDEX}/_mget"), &json!({ "ids": ids }), None).await {
            Ok(r) => {
                for d in r["docs"].as_array().cloned().unwrap_or_default() {
                    if d["found"] == true {
                        verified += 1;
                    } else {
                        missing.push(d["_id"].as_str().unwrap_or_default().to_string());
                    }
                }
            }
            Err(e) => w.problem(Product::Search, format!("_mget: {e}")),
        }
    }
    if !missing.is_empty() {
        w.problem(Product::Search, format!("{} acknowledged documents missing, e.g. {:?}", missing.len(), &missing[..missing.len().min(5)]));
    }
    let _ = search_call(w, s, reqwest::Method::POST, &format!("/{SEARCH_INDEX}/_refresh"), &json!({}), None).await;
    match search_call(w, s, reqwest::Method::GET, &format!("/{SEARCH_INDEX}/_count"), &Value::Null, None).await {
        Ok(c) if c["count"].as_u64().is_some_and(|n| n >= acks.len() as u64) => w.note(Product::Search, format!("{} documents searchable after a refresh, {} acknowledged", c["count"], acks.len())),
        Ok(c) => w.problem(Product::Search, format!("a search counts {} documents, {} were acknowledged", c["count"], acks.len())),
        Err(e) => w.problem(Product::Search, format!("_count: {e}")),
    }
    w.stat(Product::Search, |st| st.verified = verified);
}

// ── queues ──

/// A vhost with its own user, a quorum queue in it, and the vhost's AMQPS URL as the API gave it
/// (`<vhost>.mq.<region>.<domain>`; the edge listens on the published port here, so the URL works
/// as it is).
struct QueuesState {
    url: String,
    /// Confirmed publishes (message ids).
    acks: Mutex<HashSet<String>>,
    /// Every message id published (confirmed or not).
    sent: Mutex<HashSet<String>>,
    /// Deliveries: message id → how many times.
    delivered: Mutex<HashMap<String, u32>>,
    /// The consumer runs until this is set (after the publishers stop and everything arrived).
    consumer_stop: AtomicBool,
}

const MQ_VHOST: &str = "fleet";
const MQ_QUEUE: &str = "orders";
const MQ_PUBLISHERS: usize = 2;

type Amqp = (lapin::Connection, lapin::Channel);

/// An AMQPS connection and a channel through the load balancer: `connect` if it never got as far
/// as an open channel (nothing was sent).
async fn amqp_open(w: &World, q: &QueuesState, confirms: bool) -> Result<Amqp, String> {
    let tls = lapin::tcp::OwnedTLSConfig { identity: None, cert_chain: Some(w.ca_pem.clone()) };
    let runtime = lapin::runtime::default_runtime().map_err(|e| format!("runtime: {e}"))?;
    let connect = lapin::Connection::connect_with_config(&q.url, lapin::ConnectionProperties::default(), tls, runtime);
    let conn = tokio::time::timeout(Duration::from_secs(10), connect).await.map_err(|_| "connect".to_string())?.map_err(|_| "connect".to_string())?;
    let ch = conn.create_channel().await.map_err(|_| "connect".to_string())?;
    if confirms {
        ch.confirm_select(lapin::options::ConfirmSelectOptions::default()).await.map_err(|_| "connect".to_string())?;
    }
    Ok((conn, ch))
}

/// One publish, confirmed, on a shared connection (opened again if it dropped), as one attempt;
/// its steps go in `trail` (for the report on a slow one).
async fn amqp_publish(w: &World, q: &QueuesState, conn: &tokio::sync::Mutex<Option<Amqp>>, id: &str, trail: &Mutex<Vec<String>>) -> Result<(), String> {
    let mut g = conn.lock().await;
    if g.as_ref().is_some_and(|(c, ch)| !c.status().connected() || !ch.status().connected()) {
        *g = None;
    }
    if g.is_none() {
        trail.lock().push(format!("+{:.2}s connecting", w.at(Instant::now())));
        *g = Some(amqp_open(w, q, true).await?);
        trail.lock().push(format!("+{:.2}s connected", w.at(Instant::now())));
    }
    let ch = g.as_ref().unwrap().1.clone();
    let props = lapin::BasicProperties::default().with_delivery_mode(2).with_message_id(id.into());
    let published = ch.basic_publish("".into(), MQ_QUEUE.into(), lapin::options::BasicPublishOptions::default(), id.as_bytes(), props).await;
    let confirm = match published {
        Ok(c) => c,
        Err(_) => {
            *g = None;
            return Err("closed".into());
        }
    };
    match tokio::time::timeout(ATTEMPT_TIMEOUT, confirm).await {
        Ok(Ok(c)) if c.is_ack() => Ok(()),
        Ok(Ok(_)) => Err("nack".into()),
        Ok(Err(_)) => {
            *g = None;
            Err("closed".into())
        }
        Err(_) => {
            *g = None;
            Err("timeout".into())
        }
    }
}

/// Publishes with confirms, one at a time, recording every confirmed message id. A publish that
/// failed is sent again with the same id (at least once).
async fn queues_publisher(w: Arc<World>, q: Arc<QueuesState>, k: usize) {
    let conn = tokio::sync::Mutex::new(None);
    let mut n = 0u64;
    while w.running() {
        let id = format!("p{k}-{n}");
        q.sent.lock().insert(id.clone());
        w.write(Product::Queues);
        let (started, trail) = (Instant::now(), Mutex::new(Vec::new()));
        let res = retry(&w, Product::Queues, |_| async {
            let r = amqp_publish(&w, &q, &conn, &id, &trail).await;
            trail.lock().push(format!("+{:.2}s {}", w.at(Instant::now()), r.as_ref().err().map_or("confirmed", String::as_str)));
            match r {
                Ok(()) => Step::Done(()),
                Err(k) => Step::Retry(k, None),
            }
        })
        .await;
        if started.elapsed() > Duration::from_secs(5) {
            w.note(Product::Queues, format!("publish {id} took {:.1}s from +{:.2}s: {:?}", started.elapsed().as_secs_f64(), w.at(started), trail.lock()));
        }
        match res {
            Ok(()) => {
                w.ack(Product::Queues);
                q.acks.lock().insert(id);
            }
            Err(e) => w.problem(Product::Queues, format!("publish {id}: {e}")),
        }
        n += 1;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Consumes with manual acks (prefetch 100) until told to stop, reconnecting when its
/// connection closes; a delivery is recorded before it's acked.
async fn queues_consumer(w: Arc<World>, q: Arc<QueuesState>) {
    while !q.consumer_stop.load(Ordering::SeqCst) {
        let opened = retry(&w, Product::Queues, |_| async {
            let (conn, ch) = match amqp_open(&w, &q, false).await {
                Ok(c) => c,
                Err(k) => return Step::Retry(k, None),
            };
            if ch.basic_qos(100, lapin::options::BasicQosOptions::default()).await.is_err() {
                return Step::Retry("closed".into(), None);
            }
            match ch.basic_consume(MQ_QUEUE.into(), "".into(), lapin::options::BasicConsumeOptions::default(), lapin::types::FieldTable::default()).await {
                Ok(c) => Step::Done((conn, c)),
                Err(_) => Step::Retry("closed".into(), None),
            }
        })
        .await;
        let (_conn, mut consumer) = match opened {
            Ok(c) => c,
            Err(e) => {
                w.problem(Product::Queues, format!("consume: {e}"));
                continue;
            }
        };
        while !q.consumer_stop.load(Ordering::SeqCst) {
            let d = match tokio::time::timeout(Duration::from_secs(1), consumer.next()).await {
                Err(_) => continue,
                Ok(Some(Ok(d))) => d,
                Ok(Some(Err(_)) | None) => {
                    w.error(Product::Queues, "closed");
                    break;
                }
            };
            match d.properties.message_id().as_ref().map(|m| m.as_str().to_string()) {
                Some(id) if q.sent.lock().contains(&id) => *q.delivered.lock().entry(id).or_default() += 1,
                other => w.problem(Product::Queues, format!("a delivery with an unknown message id: {other:?}")),
            }
            if d.ack(lapin::options::BasicAckOptions::default()).await.is_err() {
                // Not lost: an unacked delivery comes back when its connection closes.
                w.error(Product::Queues, "closed");
                break;
            }
        }
    }
}

/// Every confirmed publish was delivered (at least once; duplicates carry the same message id).
fn queues_verify(w: &World, q: &QueuesState) {
    let acks = q.acks.lock().clone();
    let delivered = q.delivered.lock().clone();
    let mut missing: Vec<&String> = acks.iter().filter(|id| !delivered.contains_key(*id)).collect();
    if !missing.is_empty() {
        missing.sort();
        w.problem(Product::Queues, format!("{} confirmed publishes never delivered, e.g. {:?}", missing.len(), &missing[..missing.len().min(5)]));
    }
    let twice = delivered.values().filter(|n| **n > 1).count();
    let unconfirmed = delivered.keys().filter(|id| !acks.contains(*id)).count();
    w.note(Product::Queues, format!("{} messages delivered, {twice} more than once (at least once; deduplicable by message id), {unconfirmed} never confirmed to their publisher", delivered.len()));
    w.stat(Product::Queues, |st| st.verified = (acks.len() - missing.len()) as u64);
}

// ── the fleet ──

async fn healthy(i: usize) -> bool {
    docker_try(&["inspect", "-f", "{{.State.Health.Status}}", &container(i)]).is_some_and(|s| s == "healthy")
}

/// Is node `i` in the load balancer's rotation (HAProxy's stats)?
async fn lb_up(w: &World, i: usize) -> bool {
    let url = format!("http://127.0.0.1:{}/stats;csv", w.ports.stats);
    let Ok(r) = w.http.get(url).timeout(Duration::from_secs(3)).send().await else { return false };
    let text = r.text().await.unwrap_or_default();
    text.lines().any(|l| {
        let f: Vec<&str> = l.split(',').collect();
        f.len() > 17 && f[0] == "https" && f[1] == NODES[i] && f[17].starts_with("UP")
    })
}

async fn wait_healthy(w: &World, i: usize, within: Duration) -> Duration {
    let t = Instant::now();
    loop {
        if healthy(i).await && lb_up(w, i).await {
            return t.elapsed();
        }
        assert!(t.elapsed() < within, "{} never became healthy and in rotation", NODES[i]);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The leases in DynamoDB: (key, holder, free).
async fn leases(ddb: &aws_sdk_dynamodb::Client) -> Vec<(String, String, bool)> {
    use aws_sdk_dynamodb::types::AttributeValue;
    let mut out = Vec::new();
    let mut start = None;
    loop {
        let r = ddb.query().table_name("roll-cluster").key_condition_expression("pk = :pk").expression_attribute_values(":pk", AttributeValue::S("lease".into())).set_exclusive_start_key(start.take()).send().await;
        let Ok(r) = r else { return out };
        for item in r.items() {
            let s = |k: &str| item.get(k).and_then(|v| v.as_s().ok()).cloned().unwrap_or_default();
            out.push((s("sk"), s("holder"), item.get("free").and_then(|v| v.as_bool().ok()).copied().unwrap_or(false)));
        }
        match r.last_evaluated_key() {
            Some(k) => start = Some(k.clone()),
            None => break,
        }
    }
    out
}

/// Held tenant leases per node (singletons left out).
async fn lease_counts(ddb: &aws_sdk_dynamodb::Client) -> BTreeMap<String, Vec<String>> {
    let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (key, holder, free) in leases(ddb).await {
        if !free && !key.starts_with("singleton:") {
            m.entry(holder).or_default().push(key);
        }
    }
    m
}

/// Tenant leases held somewhere other than where placement (rendezvous hashing over `live`) puts
/// them: (key, holder, preferred).
async fn misplaced(ddb: &aws_sdk_dynamodb::Client, live: &[&str]) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for (key, holder, free) in leases(ddb).await {
        if free || key.starts_with("singleton:") {
            continue;
        }
        if let Some(want) = roll_cluster::placement::rendezvous(&key, live.iter().copied())
            && want != holder
        {
            out.push((key, holder, want.to_string()));
        }
    }
    out
}

/// Wait until every tenant lease is on its preferred node among `live` (rebalancing runs every
/// 10 s): a problem if that takes longer than `within`.
async fn wait_placed(w: &World, ddb: &aws_sdk_dynamodb::Client, tl: &mut Timeline, live: &[&str], within: Duration, after: &str) {
    let t = Instant::now();
    loop {
        let off = misplaced(ddb, live).await;
        if off.is_empty() {
            tl.add(w, format!("every lease on its preferred node {:.1}s after {after}: {}", t.elapsed().as_secs_f64(), summary(&lease_counts(ddb).await)));
            return;
        }
        if t.elapsed() > within {
            let line = format!("fleet: {} leases still not on their preferred node {:.0}s after {after} (key, holder, preferred): {off:?}", off.len(), within.as_secs_f64());
            tl.add(w, line.clone());
            w.problems.lock().push(line);
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn summary(m: &BTreeMap<String, Vec<String>>) -> String {
    m.iter().map(|(n, ks)| format!("{n} {}", ks.len())).collect::<Vec<_>>().join(", ")
}

struct Timeline {
    lines: Vec<(f64, String)>,
}

impl Timeline {
    fn add(&mut self, w: &World, line: String) {
        let at = w.at(Instant::now());
        eprintln!("[{at:7.2}s] {line}");
        self.lines.push((at, line));
    }
}

/// SIGTERM node `i` (the deploy's stop): it must drain and exit 0 before the stop timeout, and
/// hold no lease afterwards.
async fn stop_node(w: &World, ddb: &aws_sdk_dynamodb::Client, tl: &mut Timeline, i: usize) {
    let name = container(i);
    tl.add(w, format!("SIGTERM {} (stop timeout {STOP_TIMEOUT_S}s); holds: {}", NODES[i], lease_counts(ddb).await.get(NODES[i]).map_or(0, Vec::len)));
    let t = Instant::now();
    let n = name.clone();
    tokio::task::spawn_blocking(move || docker(&["stop", "-t", &STOP_TIMEOUT_S.to_string(), &n])).await.unwrap();
    let took = t.elapsed();
    let exit = docker(&["inspect", "-f", "{{.State.ExitCode}}", &name]);
    let left = lease_counts(ddb).await.get(NODES[i]).cloned().unwrap_or_default();
    tl.add(w, format!("{} exited ({exit}) {:.1}s after SIGTERM; tenant leases still naming it: {}", NODES[i], took.as_secs_f64(), left.len()));
    if exit != "0" {
        w.problems.lock().push(format!("fleet: {} exited {exit} after SIGTERM (killed at the stop timeout, or crashed while draining)", NODES[i]));
    }
    if !left.is_empty() {
        w.problems.lock().push(format!("fleet: {} still held {} leases after draining: {:?}", NODES[i], left.len(), &left[..left.len().min(5)]));
    }
}

async fn start_node(w: &World, tl: &mut Timeline, i: usize) {
    docker(&["start", &container(i)]);
    let t = wait_healthy(w, i, Duration::from_secs(120)).await;
    tl.add(w, format!("{} started, healthy and in rotation after {:.1}s", NODES[i], t.as_secs_f64()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn every_product_keeps_every_acknowledged_write_through_kill_rolling_restart_and_scaling() {
    if !e2e() {
        eprintln!("skipped: set ROLL_E2E=1 and bring the local fleet up (scripts/test-fleet.sh does both)");
        return;
    }
    let certs = PathBuf::from(std::env::var("ROLL_E2E_CERTS").expect("ROLL_E2E_CERTS: the directory deploy/cloud/local/gen-certs.sh wrote"));
    let ca_pem = std::fs::read_to_string(certs.join("ca.crt")).expect("ca.crt");
    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
        roots.add(c.unwrap()).unwrap();
    }
    let ports = Ports {
        https: port("ROLL_E2E_HTTPS_PORT", 28443),
        redis: port("ROLL_E2E_REDIS_PORT", 26379),
        kafka: port("ROLL_E2E_KAFKA_PORT", 29092),
        nosql: port("ROLL_E2E_NOSQL_PORT", 29042),
        amqps: port("ROLL_E2E_AMQPS_PORT", 25671),
        ddb: port("ROLL_E2E_DDB_PORT", 28000),
        stats: port("ROLL_E2E_LB_STATS_PORT", 28404),
    };
    let lb = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
    let mut hb = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .tls_certs_only([reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap()]);
    for h in [format!("api.{DOMAIN}"), format!("tasks.{REGION}.{DOMAIN}"), format!("{SEARCH_CLUSTER}.search.{REGION}.{DOMAIN}")] {
        hb = hb.resolve(&h, lb);
    }
    let scratch = tempfile::tempdir().unwrap();
    let w = Arc::new(World {
        ports,
        certs,
        ca_pem,
        roots,
        http: hb.build().unwrap(),
        stop: AtomicBool::new(false),
        t0: Instant::now(),
        stats: Mutex::default(),
        failures: Mutex::default(),
        problems: Mutex::default(),
    });
    let ddb = aws_sdk_dynamodb::Client::from_conf(
        aws_sdk_dynamodb::Config::builder()
            .behavior_version(aws_sdk_dynamodb::config::BehaviorVersion::latest())
            .region(aws_sdk_dynamodb::config::Region::new(REGION))
            .endpoint_url(format!("http://127.0.0.1:{}", ports.ddb))
            .credentials_provider(aws_sdk_dynamodb::config::Credentials::new("roll", "roll", None, None, "fleet"))
            .build(),
    );

    // The stack is up: every node healthy and in the load balancer's rotation.
    for i in 0..3 {
        wait_healthy(&w, i, Duration::from_secs(180)).await;
    }
    let info = ok(&w, "GET", &w.api("/v1/info"), None).await;
    let products: HashSet<String> = info["products"].as_array().unwrap().iter().filter_map(|p| p.as_str().map(str::to_string)).collect();
    eprintln!("fleet up: {info}");
    for p in ["code", "redis", "nosql", "streams", "storage", "tasks", "queues", "search"] {
        assert!(products.contains(p), "{p} is off on the fleet (ROLL_PRODUCTS): {info}");
    }

    // ── setup, through the edge ──
    let mut tasks = Vec::new();
    // code: an owner, two repos with a first landing each (the base of every API landing).
    ok(&w, "POST", &w.api("/v1/code/owners"), Some(json!({ "name": CODE_OWNER, "kind": "org" }))).await;
    let code = Arc::new(CodeState::default());
    let mut bases = HashMap::new();
    for repo in CODE_REPOS {
        ok(&w, "POST", &w.api("/v1/code/repos"), Some(json!({ "owner": CODE_OWNER, "name": repo, "source": "native" }))).await;
        let init = ok(&w, "POST", &w.api(&format!("/v1/code/repos/{CODE_OWNER}/{repo}/land")), Some(json!({ "message": "init", "files": { "README.md": { "content": format!("# {repo}\n") } } }))).await;
        bases.insert(repo, init["changeset"].as_str().unwrap().to_string());
    }
    // redis: a database with `sync` durability (a reply waits until the write is in S3).
    let db = ok(&w, "POST", &w.api("/v1/redis/databases"), Some(json!({ "name": "fleet", "durability": "sync" }))).await;
    let (id, password) = db["rest_token"].as_str().unwrap().rsplit_once('_').unwrap();
    let redis = Arc::new(RedisState { id: id.into(), password: password.into(), acks: Mutex::default() });
    // nosql: a `sync` database, a keyspace and a table.
    let db = ok(&w, "POST", &w.api("/v1/nosql/databases"), Some(json!({ "name": "fleet", "durability": "sync" }))).await;
    let nosql = Arc::new(NosqlState {
        server_name: format!("{}.nosql.{REGION}.{DOMAIN}", db["id"].as_str().unwrap()),
        user: db["credentials"]["username"].as_str().unwrap().into(),
        password: db["credentials"]["password"].as_str().unwrap().into(),
        acks: Mutex::default(),
    });
    {
        let conn = tokio::sync::Mutex::new(None);
        for q in ["CREATE KEYSPACE IF NOT EXISTS fleet WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}", "CREATE TABLE IF NOT EXISTS fleet.t (k bigint PRIMARY KEY, v text)"] {
            let r = retry(&w, Product::Nosql, |_| async {
                match cql_attempt(&w, &nosql, &conn, q).await {
                    Ok(_) => Step::Done(()),
                    Err(k) => Step::Retry(k, None),
                }
            })
            .await;
            r.unwrap_or_else(|e| panic!("{q}: {e}"));
        }
        // Setup errors aren't the load's.
        w.stat(Product::Nosql, |s| s.errors.clear());
    }
    // streams: a stream (its first key is the SASL login) and a topic with three partitions.
    let st = ok(&w, "POST", &w.api("/v1/streams"), Some(json!({ "name": "fleet" }))).await;
    let stream_id = st["id"].as_str().unwrap().to_string();
    ok(&w, "POST", &w.api(&format!("/v1/streams/{stream_id}/topics")), Some(json!({ "name": "events", "partitions": 3 }))).await;
    let kafka_setup = Arc::new(KafkaSetup {
        bootstrap: format!("fleet.kafka.{REGION}.{DOMAIN}:{}", ports.kafka),
        user: st["connect"]["username"].as_str().unwrap().into(),
        secret: st["connect"]["secret"].as_str().unwrap().into(),
        topic: "events".into(),
    });
    let kafka = Arc::new(KafkaState::default());
    // storage: a key from the control plane, a bucket.
    let key = ok(&w, "POST", &w.api("/v1/storage/keys"), Some(json!({ "name": "fleet", "permission": "admin" }))).await;
    let (ak, sk) = (key["id"].as_str().unwrap(), key["secret_access_key"].as_str().unwrap());
    let storage = Arc::new(StorageState { path: s3_client(&w, ak, sk, true), vhost: s3_client(&w, ak, sk, false), acks: Mutex::default(), multipart: Mutex::default(), sent: Mutex::default() });
    retry(&w, Product::Storage, |_| async { s3_step(storage.path.create_bucket().bucket(BUCKET).send().await) }).await.expect("CreateBucket");
    let tasks_state = Arc::new(TasksState::default());
    // queues: a vhost with its own user, and a quorum queue; clients use the AMQPS URL as given.
    let vh = ok(&w, "POST", &w.api("/v1/queues/vhosts"), Some(json!({ "name": MQ_VHOST, "credentials": true }))).await;
    let url = vh["credentials"]["urls"]["amqps"].as_str().unwrap_or_default().to_string();
    assert!(url.starts_with("amqps://") && url.contains(&format!("@{MQ_VHOST}.mq.{REGION}.{DOMAIN}:{}/", ports.amqps)), "the vhost's AMQPS URL names its public hostname: {vh}");
    ok(&w, "POST", &w.api(&format!("/v1/queues/vhosts/{MQ_VHOST}/queues")), Some(json!({ "name": MQ_QUEUE, "type": "quorum" }))).await;
    let queues = Arc::new(QueuesState { url, acks: Mutex::default(), sent: Mutex::default(), delivered: Mutex::default(), consumer_stop: AtomicBool::new(false) });
    // search: a cluster (its URL names the edge's hostname for it) and an index.
    let sc = ok(&w, "POST", &w.api("/v1/search/clusters"), Some(json!({ "name": SEARCH_CLUSTER }))).await;
    assert!(sc["url"].as_str().is_some_and(|u| u.contains(&format!("{SEARCH_CLUSTER}.search.{REGION}.{DOMAIN}"))), "the cluster's URL names its public hostname: {sc}");
    let search = Arc::new(SearchState { password: sc["password"].as_str().unwrap().into(), acks: Mutex::default() });
    search_call(&w, &search, reqwest::Method::PUT, &format!("/{SEARCH_INDEX}"), &json!({ "settings": { "number_of_shards": 2 } }), None).await.expect("create the search index");
    eprintln!("setup done in {:.1}s", w.at(Instant::now()));

    // ── traffic ──
    for repo in CODE_REPOS {
        tasks.push(tokio::spawn(code_writer(w.clone(), code.clone(), repo, 0, bases[repo].clone())));
    }
    tasks.push(tokio::spawn(code_pusher(w.clone(), code.clone(), scratch.path().to_path_buf(), "busy")));
    tasks.push(tokio::spawn(code_reader(w.clone())));
    tasks.push(tokio::spawn(redis_writer(w.clone(), redis.clone())));
    tasks.push(tokio::spawn(redis_reader(w.clone(), redis.clone())));
    tasks.push(tokio::spawn(nosql_writer(w.clone(), nosql.clone())));
    tasks.push(tokio::spawn(nosql_reader(w.clone(), nosql.clone())));
    tasks.push(tokio::spawn(storage_writer(w.clone(), storage.clone())));
    tasks.push(tokio::spawn(storage_multipart(w.clone(), storage.clone())));
    tasks.push(tokio::spawn(storage_reader(w.clone(), storage.clone())));
    tasks.push(tokio::spawn(tasks_writer(w.clone(), tasks_state.clone())));
    let consumer = tokio::spawn(tasks_consumer(w.clone(), tasks_state.clone()));
    for k in 0..MQ_PUBLISHERS {
        tasks.push(tokio::spawn(queues_publisher(w.clone(), queues.clone(), k)));
    }
    let mq_consumer = tokio::spawn(queues_consumer(w.clone(), queues.clone()));
    for k in 0..SEARCH_WRITERS {
        tasks.push(tokio::spawn(search_writer(w.clone(), search.clone(), k)));
    }
    tasks.push(tokio::spawn(search_reader(w.clone(), search.clone())));
    let kafka_threads = {
        let (w1, k1, s1) = (w.clone(), kafka.clone(), kafka_setup.clone());
        let (w2, k2, s2) = (w.clone(), kafka.clone(), kafka_setup.clone());
        [std::thread::spawn(move || kafka_writer(w1, k1, s1)), std::thread::spawn(move || kafka_reader(w2, k2, s2))]
    };
    let traffic_from = Instant::now();
    let mut tl = Timeline { lines: Vec::new() };
    tl.add(&w, "traffic starts".into());
    // Each disruption from its start until the fleet is whole again, for the report.
    let mut phases: Vec<(&str, Instant, Instant)> = Vec::new();
    tokio::time::sleep(Duration::from_secs(20)).await;
    tl.add(&w, format!("leases: {}", summary(&lease_counts(&ddb).await)));

    // (a) SIGKILL the node holding the most leases, then start it again (ECS replacing it).
    let counts = lease_counts(&ddb).await;
    // `ROLL_E2E_KILL=<lease key>`: kill that lease's holder instead (to reproduce a finding).
    let chosen = std::env::var("ROLL_E2E_KILL").ok().and_then(|k| (0..3).find(|i| counts.get(NODES[*i]).is_some_and(|ks| ks.contains(&k))));
    let victim = chosen.unwrap_or_else(|| (0..3).max_by_key(|i| counts.get(NODES[*i]).map_or(0, Vec::len)).unwrap());
    let held = counts.get(NODES[victim]).cloned().unwrap_or_default();
    let kill_at = Instant::now();
    docker(&["kill", "-s", "KILL", &container(victim)]);
    tl.add(&w, format!("SIGKILL {} (holds {} leases: {:?})", NODES[victim], held.len(), held));
    let moved = loop {
        let now = lease_counts(&ddb).await;
        let still = now.get(NODES[victim]).map_or(0, Vec::len);
        if still == 0 {
            break kill_at.elapsed();
        }
        if kill_at.elapsed() > Duration::from_secs(60) {
            w.problems.lock().push(format!("fleet: {still} leases of the killed {} weren't taken over within 60 s", NODES[victim]));
            break kill_at.elapsed();
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    tl.add(&w, format!("every lease of {} taken over {:.1}s after the kill; leases: {}", NODES[victim], moved.as_secs_f64(), summary(&lease_counts(&ddb).await)));
    tokio::time::sleep(Duration::from_secs(5)).await;
    start_node(&w, &mut tl, victim).await;
    wait_placed(&w, &ddb, &mut tl, &NODES[..3], Duration::from_secs(45), "the killed node came back").await;
    phases.push(("SIGKILL", kill_at, Instant::now()));
    tokio::time::sleep(Duration::from_secs(10)).await;

    // (b) Rolling restart: each node in turn, healthy before the next (the deploy pipeline).
    let roll_at = Instant::now();
    for i in 0..3 {
        stop_node(&w, &ddb, &mut tl, i).await;
        start_node(&w, &mut tl, i).await;
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    wait_placed(&w, &ddb, &mut tl, &NODES[..3], Duration::from_secs(45), "the rolling restart").await;
    phases.push(("rolling restart", roll_at, Instant::now()));
    tokio::time::sleep(Duration::from_secs(5)).await;

    // (c) Scale out to four nodes and back to three.
    let scale_at = Instant::now();
    compose(&["up", "-d", "--no-deps", "node4"]);
    let t = wait_healthy(&w, 3, Duration::from_secs(120)).await;
    tl.add(&w, format!("node4 joined, healthy and in rotation after {:.1}s", t.as_secs_f64()));
    wait_placed(&w, &ddb, &mut tl, &NODES, Duration::from_secs(60), "node4 joined").await;
    if !lease_counts(&ddb).await.contains_key(NODES[3]) {
        // Placement over this run's ids gave node4 nothing: the scale-out only tested joining.
        tl.add(&w, "note: placement gave node4 no lease this run".into());
    }
    tokio::time::sleep(Duration::from_secs(10)).await;
    stop_node(&w, &ddb, &mut tl, 3).await;
    docker(&["rm", "-v", &container(3)]);
    wait_placed(&w, &ddb, &mut tl, &NODES[..3], Duration::from_secs(45), "scaling back to three").await;
    phases.push(("scale out and in", scale_at, Instant::now()));
    tokio::time::sleep(Duration::from_secs(10)).await;

    // ── stop, drain, verify ──
    // Stalls are measured until here, not until the writers finish their last operation (one
    // may be sleeping between multipart uploads).
    let traffic_to = Instant::now();
    w.stop.store(true, Ordering::SeqCst);
    for t in tasks {
        t.await.unwrap();
    }
    tl.add(&w, "writers stopped".into());
    for t in kafka_threads {
        t.join().unwrap();
    }
    // The tasks consumer runs until every acknowledged publish arrived (or 90 s).
    let t = Instant::now();
    loop {
        let all = {
            let (acks, got) = (tasks_state.acks.lock(), tasks_state.delivered.lock());
            acks.keys().all(|n| got.contains_key(n))
        };
        if all || t.elapsed() > OP_DEADLINE {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    tasks_state.consumer_stop.store(true, Ordering::SeqCst);
    consumer.await.unwrap();
    // The queues consumer too, until every confirmed publish arrived (or 90 s).
    let t2 = Instant::now();
    while t2.elapsed() < OP_DEADLINE && !queues.acks.lock().iter().all(|id| queues.delivered.lock().contains_key(id)) {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    queues.consumer_stop.store(true, Ordering::SeqCst);
    mq_consumer.await.unwrap();
    tl.add(&w, format!("consumers drained ({:.1}s)", t.elapsed().as_secs_f64()));

    code_verify(&w, &code, scratch.path()).await;
    redis_verify(&w, &redis).await;
    nosql_verify(&w, &nosql).await;
    {
        let (w2, k2, s2) = (w.clone(), kafka.clone(), kafka_setup.clone());
        tokio::task::spawn_blocking(move || kafka_verify(&w2, &k2, &s2)).await.unwrap();
    }
    storage_verify(&w, &storage).await;
    tasks_verify(&w, &tasks_state).await;
    queues_verify(&w, &queues);
    search_verify(&w, &search).await;

    // ── the scoreboard ──
    eprintln!("\n=== roll cloud fleet e2e: {:.0}s of traffic ===", (traffic_to - traffic_from).as_secs_f64());
    eprintln!("timeline (seconds since the test started):");
    for (at, line) in &tl.lines {
        eprintln!("  {at:7.2}  {line}");
    }
    let phase_of = |t: Instant| phases.iter().find(|(_, a, b)| t + Duration::from_secs(1) >= *a && t <= *b + Duration::from_secs(2)).map_or("steady", |p| p.0);
    eprintln!("\n{:<8} {:>7} {:>7} {:>8}  {:<26} errors by kind (! = not allowed by the product's docs)", "product", "writes", "acked", "verified", "max stall");
    let stats = w.stats.lock();
    let mut disallowed = Vec::new();
    // The longest gap between two acknowledged writes, per product and per phase (by its start).
    let mut stalls: BTreeMap<(&str, Product), f64> = BTreeMap::new();
    for (p, s) in stats.iter() {
        let mut acks = s.acks.clone();
        acks.sort();
        let mut stall = (Duration::ZERO, traffic_from);
        let mut prev = traffic_from;
        for a in acks.iter().copied().filter(|a| *a <= traffic_to).chain([traffic_to]) {
            let gap = a.saturating_duration_since(prev);
            if gap > stall.0 {
                stall = (gap, prev);
            }
            let e = stalls.entry((phase_of(prev), *p)).or_default();
            *e = e.max(gap.as_secs_f64());
            prev = prev.max(a);
        }
        let errors: Vec<String> = s
            .errors
            .iter()
            .map(|(k, n)| {
                let bad = !p.allows(k);
                if bad {
                    disallowed.push(format!("{}: {n} × {k}", p.name()));
                }
                format!("{}{k} ×{n}", if bad { "!" } else { "" })
            })
            .collect();
        let stall_s = format!("{:.1}s ({} at +{:.0}s)", stall.0.as_secs_f64(), phase_of(stall.1), w.at(stall.1));
        eprintln!("{:<8} {:>7} {:>7} {:>8}  {:<26} {}", p.name(), s.writes, s.acked, s.verified, stall_s, if errors.is_empty() { "none".into() } else { errors.join(", ") });
    }
    eprintln!("\nlongest stall by phase (seconds between two acknowledged writes):");
    for phase in ["steady", "SIGKILL", "rolling restart", "scale out and in"] {
        let line: Vec<String> = stalls.iter().filter(|((ph, _), _)| *ph == phase).map(|((_, p), s)| format!("{} {s:.1}", p.name())).collect();
        eprintln!("  {phase:<17} {}", line.join(", "));
    }
    eprintln!("\nerrors by disruption (seconds since it began):");
    {
        let failures = w.failures.lock();
        let mut groups: BTreeMap<(&str, Product, &str), (u64, f64, f64)> = BTreeMap::new();
        for (at, p, kind) in failures.iter() {
            let phase = phases.iter().find(|(_, a, b)| *at + Duration::from_secs(1) >= *a && *at <= *b + Duration::from_secs(2));
            let since = phase.map_or(w.at(*at), |(_, a, _)| at.saturating_duration_since(*a).as_secs_f64());
            let g = groups.entry((phase.map_or("steady", |p| p.0), *p, kind.as_str())).or_insert((0, f64::MAX, 0.0));
            *g = (g.0 + 1, g.1.min(since), g.2.max(since));
        }
        for ((phase, p, kind), (n, a, b)) in &groups {
            eprintln!("  {phase:<17} {:<8} {n:5} × {kind} (+{a:.1}..+{b:.1})", p.name());
        }
    }
    for (p, s) in stats.iter() {
        for n in &s.notes {
            eprintln!("{}: {n}", p.name());
        }
    }

    let mut problems = w.problems.lock().clone();
    for (p, s) in stats.iter() {
        if s.verified < s.acked {
            problems.push(format!("{}: {} of {} acknowledged writes verified", p.name(), s.verified, s.acked));
        }
        if s.acked < 50 {
            problems.push(format!("{}: only {} writes acknowledged: not enough load to judge", p.name(), s.acked));
        }
    }
    for d in disallowed {
        problems.push(format!("{d}: an error kind the product's docs don't allow"));
    }
    drop(stats);
    assert!(problems.is_empty(), "{} problems:\n  {}", problems.len(), problems.join("\n  "));
}
