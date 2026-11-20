//! Reindex from a remote cluster (`RemoteReindexingUtils`, `RemotePitPaginatedHitSource`,
//! `RemoteScrollablePaginatedHitSource`): the source documents are read from another
//! Elasticsearch-compatible cluster over HTTP or HTTPS, as Elasticsearch 9.5.5 reads them:
//!
//! 1. `GET /` for the remote's version;
//! 2. a remote of 7.10 or later is read through a point in time: `POST /{indices}/_pit`, then
//!    `POST /_search` pages sorted by `_shard_doc` (`_doc` before 7.12) with `search_after`, then
//!    `DELETE /_pit`;
//! 3. an older remote (and OpenSearch, whose `2.x`/`3.x` version numbers read as old
//!    Elasticsearch ones, as they do to Elasticsearch) is read with a scroll:
//!    `POST /{indices}/_search?scroll=5m&sort=_doc:asc&…`, `POST /_search/scroll` until a page
//!    comes back empty, `DELETE /_search/scroll`.
//!
//! Pages are read one at a time and written before the next is fetched, so a migration of any
//! size holds one page in memory.
//!
//! A remote must match `reindex.remote.whitelist`, as in Elasticsearch: refused unless
//! whitelisted (an empty or unset whitelist refuses every remote). The whitelist is the
//! cluster's own ([`crate::Cluster::reindex_whitelist`], which roll's API sets as Elastic Cloud's
//! user settings set it, and which roll's migration adds its host to), else the node setting.
//! The node's operator rules apply on top: `reindex.remote.blocklist` refuses what it matches.
//!
//! The remote is a URL a tenant chose, so roll's rule for those applies (AGENTS.md): names
//! resolve to public addresses only ([`roll_net::PublicOnly`]), private IP literals and
//! `localhost` are refused ([`roll_net::literal_private_host`]), no redirect is followed, no
//! proxy is used, each read times out and a request's whole exchange is capped at 10 minutes. The
//! node setting [`ALLOW_PRIVATE`] lifts the address check (local development, tests). HTTPS is
//! verified against the platform's roots plus `reindex.ssl.certificate_authorities` (PEM files),
//! with `reindex.ssl.verification_mode` (`full`, `certificate`, `none`).
//!
//! What a node spends on it is bounded by its operator, never by a tenant ([`RemoteNode`]): at
//! most [`MAX_PER_CLUSTER`] tasks per cluster and [`MAX_PER_NODE`] per node run at once (more get
//! a 429), every answer is read to at most [`BUFFER_LIMIT`] (16 MiB) and charged, three times
//! over for the parsed documents and their re-serialized sources, to a node-wide budget
//! ([`MEMORY`]) while it's held, a scroll or point-in-time id is at most [`MAX_ID_BYTES`], and a
//! task runs at most [`MAX_DURATION`] (24 hours).
//!
//! Credentials (`password`, `headers`) never leave this module except on the wire to the remote:
//! [`Remote`]'s `Debug` and the task description leave them out. An error never quotes what a
//! remote answered to a request that carried credentials (a hostile remote could echo them in a
//! form no scrub recognizes): it gives the status alone. Transport failures have the credentials
//! replaced by `::es_redacted::`.

use super::by_query_request::{ByScroll, Remote};
use crate::breaker::Reservation;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use bytes::Bytes;
use search_aggs::memory::MemoryBudget;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Lets reindex from remote reach private addresses (loopback, private ranges, `localhost`).
pub const ALLOW_PRIVATE: &str = "roll.reindex.remote.allow_private_hosts";
/// The most one answer of the remote may hold (a byte size; Elasticsearch's REST client buffers
/// at most 100 MiB).
pub const BUFFER_LIMIT: &str = "roll.reindex.remote.buffer_limit";
pub const WHITELIST: &str = "reindex.remote.whitelist";
pub const BLOCKLIST: &str = "reindex.remote.blocklist";
pub const SSL_CERTIFICATE_AUTHORITIES: &str = "reindex.ssl.certificate_authorities";
pub const SSL_VERIFICATION_MODE: &str = "reindex.ssl.verification_mode";
/// Remote reindex tasks one cluster may run at once (2 unless set).
pub const MAX_PER_CLUSTER: &str = "roll.reindex.remote.max_per_cluster";
/// Remote reindex tasks one node may run at once, across its clusters (4 unless set).
pub const MAX_PER_NODE: &str = "roll.reindex.remote.max_per_node";
/// Bytes a node's remote answers may hold at once, across its clusters (256mb unless set).
pub const MEMORY: &str = "roll.reindex.remote.memory";
/// How long one remote reindex task may run (24h unless set).
pub const MAX_DURATION: &str = "roll.reindex.remote.max_duration";
/// The longest scroll or point-in-time id kept from a remote.
pub const MAX_ID_BYTES: usize = 4096;

const DEFAULT_BUFFER_LIMIT: u64 = 16 * 1024 * 1024;
const DEFAULT_MEMORY: u64 = 256 * 1024 * 1024;
const DEFAULT_MAX_DURATION: Duration = Duration::from_secs(24 * 3600);
/// An answer costs its raw bytes, its parsed documents and their re-serialized sources.
const CHARGE_FACTOR: u64 = 3;
/// `RemoteParseContext.REMOTE_RESPONSE_BREAKER_LABEL`.
const BREAKER_LABEL: &str = "reindex_remote_response";
/// `cluster.reindex.pit.keep_alive`'s default.
const PIT_KEEP_ALIVE_NANOS: u64 = 15 * 60 * 1_000_000_000;
/// A request's whole exchange, whatever `socket_timeout` (which bounds each read) allows: a
/// remote that drips bytes can't hold a task forever.
const MAX_EXCHANGE: Duration = Duration::from_secs(600);
/// `AbstractBulkByScrollRequest`'s defaults: 500ms, 11 retries of a rejected (429) search.
const RETRY_INITIAL_MS: u64 = 500;
const MAX_RETRIES: u32 = 11;
/// What replaces a credential in anything quoted from the remote.
const REDACTED: &str = "::es_redacted::";

/// What reindex from remote may use on one node, across every cluster it serves: the tasks it
/// runs at once, the memory their answers hold and how long a task runs. Read from the node's
/// settings when the node starts; no tenant setting changes it.
pub struct RemoteNode {
    running: AtomicUsize,
    max_per_node: usize,
    max_per_cluster: usize,
    /// What the node's remote answers hold ([`MEMORY`]).
    pub budget: Arc<MemoryBudget>,
    pub max_duration: Duration,
}

impl std::fmt::Debug for RemoteNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteNode")
            .field("running", &self.running.load(Ordering::Relaxed))
            .field("max_per_node", &self.max_per_node)
            .field("max_per_cluster", &self.max_per_cluster)
            .field("memory", &self.budget.limit())
            .finish_non_exhaustive()
    }
}

impl RemoteNode {
    /// The node's limits from its settings.
    pub fn from_settings(settings: &[(String, String)]) -> Arc<RemoteNode> {
        let get = |k: &str| settings.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        let count = |k: &str, d: usize| get(k).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(d);
        let memory = get(MEMORY).and_then(|v| crate::units::ByteSize::parse(v, MEMORY).ok()).map_or(DEFAULT_MEMORY, |b| b.0.max(1) as u64);
        let max_duration = get(MAX_DURATION)
            .and_then(|v| crate::units::TimeValue::parse(v, MAX_DURATION).ok())
            .map_or(DEFAULT_MAX_DURATION, |t| Duration::from_millis(t.as_millis().max(1) as u64));
        Arc::new(RemoteNode { running: AtomicUsize::new(0), max_per_node: count(MAX_PER_NODE, 4), max_per_cluster: count(MAX_PER_CLUSTER, 2), budget: MemoryBudget::new(memory), max_duration })
    }

    /// A slot for one remote reindex task of the cluster whose running count is `cluster`, or
    /// a 429 when the cluster or the node runs as many as it may.
    pub fn acquire(self: &Arc<Self>, cluster: &Arc<AtomicUsize>) -> EsResult<RemoteSlot> {
        let take = |c: &AtomicUsize, max: usize| c.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < max).then_some(n + 1)).is_ok();
        if !take(cluster, self.max_per_cluster) {
            return Err(rejected(format!("the cluster runs [{}] reindex from remote tasks, the most it may run at once", self.max_per_cluster)));
        }
        if !take(&self.running, self.max_per_node) {
            cluster.fetch_sub(1, Ordering::SeqCst);
            return Err(rejected(format!("the node runs [{}] reindex from remote tasks, the most it may run at once", self.max_per_node)));
        }
        Ok(RemoteSlot { node: self.clone(), cluster: cluster.clone() })
    }
}

fn rejected(why: String) -> EsError {
    EsError::new(429, "es_rejected_execution_exception", format!("rejected execution of reindex from remote: {why}"))
}

/// One running remote reindex task's place in [`RemoteNode`]'s counts, given back when dropped.
pub struct RemoteSlot {
    node: Arc<RemoteNode>,
    cluster: Arc<AtomicUsize>,
}

impl std::fmt::Debug for RemoteSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RemoteSlot")
    }
}

impl Drop for RemoteSlot {
    fn drop(&mut self) {
        self.node.running.fetch_sub(1, Ordering::SeqCst);
        self.cluster.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One document read from the remote cluster.
pub struct RemoteDoc {
    pub index: String,
    pub id: String,
    pub routing: Option<String>,
    pub version: i64,
    pub source: Option<Bytes>,
}

/// How the node lets reindex from remote connect (its operator's settings).
pub struct Rules {
    allow_private: bool,
    whitelist: Option<Vec<String>>,
    blocklist: Vec<String>,
    buffer_limit: u64,
    roots: Vec<reqwest::Certificate>,
    verification: Verification,
}

#[derive(Clone, Copy, PartialEq)]
enum Verification {
    Full,
    Certificate,
    None,
}

/// A list setting as node settings hold it: `a,b` or `[a, b]`.
fn list(v: &str) -> Vec<String> {
    v.trim().trim_start_matches('[').trim_end_matches(']').split(',').map(str::trim).filter(|p| !p.is_empty()).map(str::to_string).collect()
}

impl Rules {
    /// The rules from node settings (`setting(key)`).
    pub fn of<'a>(setting: impl Fn(&str) -> Option<&'a str>) -> EsResult<Rules> {
        let buffer_limit = match setting(BUFFER_LIMIT) {
            Some(v) => crate::units::ByteSize::parse(v, BUFFER_LIMIT)?.0.max(1) as u64,
            None => DEFAULT_BUFFER_LIMIT,
        };
        let mut roots = Vec::new();
        for path in setting(SSL_CERTIFICATE_AUTHORITIES).map(list).unwrap_or_default() {
            let pem = std::fs::read(&path).map_err(|e| EsError::illegal_argument(format!("failed to read [{SSL_CERTIFICATE_AUTHORITIES}] file [{path}]: {e}")))?;
            let certs = reqwest::Certificate::from_pem_bundle(&pem)
                .map_err(|e| EsError::illegal_argument(format!("failed to parse [{SSL_CERTIFICATE_AUTHORITIES}] file [{path}]: {e}")))?;
            roots.extend(certs);
        }
        let verification = match setting(SSL_VERIFICATION_MODE).map(str::trim) {
            None | Some("full") => Verification::Full,
            Some("certificate") => Verification::Certificate,
            Some("none") => Verification::None,
            Some(v) => return Err(EsError::illegal_argument(format!("unknown verification mode [{v}] for [{SSL_VERIFICATION_MODE}]"))),
        };
        Ok(Rules {
            allow_private: setting(ALLOW_PRIVATE).is_some_and(|v| v.eq_ignore_ascii_case("true")),
            whitelist: setting(WHITELIST).map(list),
            blocklist: setting(BLOCKLIST).map(list).unwrap_or_default(),
            buffer_limit,
            roots,
            verification,
        })
    }

    /// The rules for `cluster`: its node's settings, with the cluster's own whitelist when it
    /// has one ([`crate::Cluster::reindex_whitelist`]).
    pub fn of_cluster(cluster: &crate::Cluster) -> EsResult<Rules> {
        let mut rules = Rules::of(|k| cluster.config.node_setting(k))?;
        if let Some(w) = cluster.reindex_whitelist.read().clone() {
            rules.whitelist = Some(w);
        }
        Ok(rules)
    }

    /// Without the whitelist check: roll's migration lists a remote's indices with the
    /// credentials its owner gave before any task reads from it. The operator's rules (the
    /// blocklist, private addresses) still apply.
    pub fn without_whitelist(mut self) -> Rules {
        self.whitelist = Some(vec!["*".into()]);
        self
    }

    /// `ReindexValidator.checkRemoteWhitelist`, then roll's address rule.
    pub fn check(&self, r: &Remote) -> EsResult<()> {
        if !matches!(r.scheme.as_str(), "http" | "https") {
            return Err(EsError::illegal_argument(format!("[host] must use the http or https scheme but was [{}]", r.scheme)));
        }
        let target = format!("{}:{}", r.host, r.port);
        let matches = |list: &[String]| list.iter().any(|p| crate::xcontent::filter::glob_match(p, &target));
        // `ReindexValidator.checkRemoteWhitelist`: unset or empty, every remote is refused.
        let allowed = self.whitelist.as_deref().is_some_and(matches) && !matches(&self.blocklist);
        if !allowed {
            return Err(EsError::illegal_argument(if self.blocklist.is_empty() {
                format!("[{target}] not whitelisted in {WHITELIST}")
            } else {
                format!("[{target}] either not whitelisted in {WHITELIST} or blocked in {BLOCKLIST}")
            }));
        }
        if !self.allow_private {
            let url = reqwest::Url::parse(&format!("{}://{target}/", r.scheme)).map_err(|_| EsError::illegal_argument(format!("[host] [{target}] is not a valid host")))?;
            if roll_net::literal_private_host(&url) {
                return Err(EsError::illegal_argument(format!("[{target}] is a private address, which reindex from remote may not connect to unless [{ALLOW_PRIVATE}] is set")));
            }
        }
        Ok(())
    }
}

/// A remote's version (`GET /`'s `version.number`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    /// `Version.fromString` with a build qualifier (`-SNAPSHOT`, `-rc1`) dropped.
    fn parse(s: &str) -> Option<Version> {
        let core = s.split('-').next()?;
        let mut parts = core.split('.').map(|p| p.parse::<u32>().ok());
        let v = Version(parts.next()??, parts.next()??, parts.next().unwrap_or(Some(0))?);
        Some(v)
    }
}

/// What the remote said about itself (`GET /`).
pub struct RemoteInfo {
    pub version: Version,
    pub number: String,
    pub distribution: Option<String>,
    pub cluster_name: Option<String>,
}

fn not_elasticsearch(cause: Option<EsError>) -> EsError {
    let e = EsError::new(500, "exception", "Error parsing the response, remote is likely not an Elasticsearch instance");
    match cause {
        Some(c) => e.caused_by(c),
        None => e,
    }
}

/// `entity content is too long [n] for the configured buffer limit [limit]`.
fn too_large(n: u64, limit: u64) -> EsError {
    EsError::illegal_argument("Remote responded with a chunk that was too large. Use a smaller batch size.")
        .caused_by(EsError::java(500, "content_too_long_exception", format!("entity content is too long [{n}] for the configured buffer limit [{limit}]")))
}

fn innermost(e: &(dyn std::error::Error + 'static)) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(x) = src {
        s = x.to_string();
        src = x.source();
    }
    s
}

/// A connection to one remote cluster: the guarded client, its credentials and its limits.
pub struct Client {
    http: reqwest::Client,
    /// `scheme://host:port[pathPrefix]`.
    base: String,
    /// `scheme://host:port`, as errors name the host.
    host: String,
    socket_timeout: Duration,
    /// Values that must never appear in what roll prints: the password, the `Authorization`
    /// it makes, and every header value (whole and its longer words).
    secrets: Vec<String>,
    /// The requests carry credentials: no error quotes what the remote answered.
    credentials: bool,
    limit: u64,
    budget: Option<Arc<MemoryBudget>>,
}

/// An answer read whole (within the limit), with the breaker bytes it holds.
struct Body {
    status: u16,
    bytes: Vec<u8>,
    held: Option<Reservation>,
}

impl Client {
    /// A client for `r` under `rules`; `budget` is the breaker the answers are charged to.
    pub fn new(rules: &Rules, r: &Remote, budget: Option<Arc<MemoryBudget>>) -> EsResult<Client> {
        rules.check(r)?;
        let ms = |t: &crate::units::TimeValue| Duration::from_millis(t.as_millis().max(1) as u64);
        // Elasticsearch's 1s connect timeout covers TCP alone; here it covers the TLS handshake
        // too, so an HTTPS remote gets 5s unless the request says otherwise.
        let connect = r.connect_timeout.as_ref().map(ms).unwrap_or(if r.scheme == "https" { Duration::from_secs(5) } else { Duration::from_secs(1) });
        let socket_timeout = r.socket_timeout.as_ref().map(ms).unwrap_or(Duration::from_secs(30));
        let mut secrets: Vec<String> = Vec::new();
        let mut headers = reqwest::header::HeaderMap::new();
        for (k, v) in &r.headers {
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes()).map_err(|_| EsError::illegal_argument(format!("[headers] holds an invalid header name [{k}]")))?;
            let mut value = reqwest::header::HeaderValue::from_str(v).map_err(|_| EsError::illegal_argument(format!("[headers] holds an invalid value for header [{k}]")))?;
            value.set_sensitive(true);
            headers.append(name, value);
            secrets.push(v.clone());
            secrets.extend(v.split_whitespace().filter(|w| w.len() >= 8).map(str::to_string));
        }
        // A header the request sets wins over `username`/`password`, as Apache's client leaves
        // an `Authorization` it already has alone.
        if let Some(u) = &r.username
            && !headers.contains_key(reqwest::header::AUTHORIZATION)
        {
            use base64::Engine as _;
            let token = base64::engine::general_purpose::STANDARD.encode(format!("{u}:{}", r.password.as_deref().unwrap_or("")));
            let mut value = reqwest::header::HeaderValue::from_str(&format!("Basic {token}")).map_err(|_| EsError::illegal_argument("[username] holds characters a header can't carry"))?;
            value.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, value);
            secrets.push(token);
        }
        if let Some(p) = &r.password {
            secrets.push(p.clone());
        }
        let credentials = r.username.is_some() || r.password.is_some() || !r.headers.is_empty();
        secrets.retain(|s| !s.is_empty());
        // Longest first, so a value containing another is replaced whole.
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        secrets.dedup();
        let mut b = reqwest::Client::builder()
            .connect_timeout(connect)
            .read_timeout(socket_timeout.min(MAX_EXCHANGE))
            .timeout(MAX_EXCHANGE)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .http1_only()
            .user_agent(concat!("roll-search/", env!("CARGO_PKG_VERSION"), " (reindex)"))
            .default_headers(headers);
        if !rules.allow_private {
            b = b.dns_resolver(Arc::new(roll_net::PublicOnly));
        }
        if !rules.roots.is_empty() {
            b = b.tls_certs_merge(rules.roots.iter().cloned());
        }
        match rules.verification {
            Verification::Full => {}
            Verification::Certificate => b = b.tls_danger_accept_invalid_hostnames(true),
            Verification::None => b = b.tls_danger_accept_invalid_certs(true),
        }
        let http = b.build().map_err(|e| EsError::illegal_state(format!("building the remote client failed: {}", innermost(&e))))?;
        let prefix = r.path_prefix.as_deref().unwrap_or("").trim_end_matches('/');
        let host = format!("{}://{}:{}", r.scheme, r.host, r.port);
        Ok(Client { base: format!("{host}{prefix}"), host, http, socket_timeout: socket_timeout.min(MAX_EXCHANGE), secrets, credentials, limit: rules.buffer_limit, budget })
    }

    /// `s` with every credential replaced.
    fn scrub(&self, s: &str) -> String {
        let mut out = s.to_string();
        for secret in &self.secrets {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), REDACTED);
            }
        }
        out
    }

    fn transport_error(&self, e: &reqwest::Error) -> EsError {
        if e.is_timeout() {
            return EsError::java(500, "socket_timeout_exception", format!("{} milliseconds timeout on connection http-outgoing-0 [ACTIVE]", self.socket_timeout.as_millis()));
        }
        let detail = self.scrub(&innermost(e));
        let lower = detail.to_ascii_lowercase();
        if lower.contains("certificate") || lower.contains("handshake") || lower.contains("tls") {
            return EsError::java(500, "s_s_l_handshake_exception", detail);
        }
        if lower.contains("private address") {
            return EsError::java(500, "connect_exception", detail);
        }
        if lower.contains("dns error") || lower.contains("failed to lookup") || lower.contains("name or service not known") {
            let host = self.host.split("://").nth(1).and_then(|h| h.rsplit_once(':')).map(|(h, _)| h).unwrap_or_default();
            return EsError::java(500, "unknown_host_exception", format!("{host}: Name or service not known"));
        }
        if e.is_connect() {
            return EsError::java(500, "connect_exception", "Connection refused");
        }
        EsError::java(500, "i_o_exception", detail)
    }

    /// Sends one request and reads its answer, at most [`Rules`]' buffer limit of it, charged to
    /// the breaker while it's held.
    async fn exchange(&self, method: &reqwest::Method, path_query: &str, body: Option<&str>) -> EsResult<Body> {
        let url = format!("{}{path_query}", self.base);
        let mut rb = self.http.request(method.clone(), &url);
        if let Some(b) = body {
            rb = rb.header(reqwest::header::CONTENT_TYPE, "application/json; charset=UTF-8").body(b.to_string());
        }
        let mut resp = rb.send().await.map_err(|e| self.transport_error(&e))?;
        let status = resp.status().as_u16();
        if let Some(n) = resp.content_length().filter(|n| *n > self.limit) {
            return Err(too_large(n, self.limit));
        }
        let mut held = self.budget.clone().map(Reservation::new);
        let mut bytes = Vec::with_capacity(resp.content_length().unwrap_or(4096).min(self.limit).min(1 << 20) as usize);
        while let Some(chunk) = resp.chunk().await.map_err(|e| self.transport_error(&e))? {
            let n = (bytes.len() + chunk.len()) as u64;
            if n > self.limit {
                return Err(too_large(n, self.limit));
            }
            if let Some(h) = held.as_mut() {
                h.add(chunk.len() as u64 * CHARGE_FACTOR, BREAKER_LABEL)?;
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Body { status, bytes, held })
    }

    /// `wrapExceptionToPreserveStatus`: the remote's answer as the reason, its status kept.
    /// After a request that carried credentials, the answer is left out (its status stays).
    fn status_error(&self, method: &reqwest::Method, path_query: &str, b: &Body) -> EsError {
        let text = if self.credentials {
            format!("[the remote's answer isn't shown: the request carried credentials; {} bytes]", b.bytes.len())
        } else {
            self.scrub(&String::from_utf8_lossy(&b.bytes))
        };
        let code = reqwest::StatusCode::from_u16(b.status).ok();
        let reason = code.and_then(|c| c.canonical_reason());
        let no_body = matches!(b.status, 100..=199 | 204 | 304);
        let message = if no_body { "No error body.".to_string() } else { format!("body={text}") };
        let (status, message) = match reason {
            Some(_) => (b.status, message),
            None => (500, format!("Couldn't extract status [{}]. {message}", b.status)),
        };
        let line = format!("HTTP/1.1 {}{}", b.status, reason.map(|r| format!(" {r}")).unwrap_or_default());
        let cause = EsError::java(b.status, "response_exception", format!("method [{method}], host [{}], URI [{path_query}], status line [{line}]\n{text}", self.host));
        EsError::new(status, "status_exception", message).caused_by(cause)
    }

    /// One request whose answer is JSON; a rejected one (429) is sent again with Elasticsearch's
    /// backoff, each retry counted in `retries`.
    async fn json(&self, method: reqwest::Method, path_query: &str, body: Option<&str>, retries: &mut u64) -> EsResult<(Value, Option<Reservation>)> {
        let mut attempt = 0u32;
        loop {
            let b = self.exchange(&method, path_query, body).await?;
            if b.status == 429 && attempt < MAX_RETRIES {
                // `BackoffPolicy.exponentialBackoff`: start + 10 * (e^(0.8 * n) - 1) ms.
                let wait = RETRY_INITIAL_MS + 10 * (((0.8 * attempt as f64).exp() as u64).saturating_sub(1));
                attempt += 1;
                *retries += 1;
                tokio::time::sleep(Duration::from_millis(wait)).await;
                continue;
            }
            if !(200..300).contains(&b.status) {
                return Err(self.status_error(&method, path_query, &b));
            }
            return match crate::json::parse(&b.bytes) {
                // Any JSON: `_cat` answers an array; callers check the shape they need.
                Ok(Some(v)) => Ok((v, b.held)),
                Ok(None) => Err(not_elasticsearch(None)),
                Err(e) => {
                    let detail = if self.credentials { "the answer isn't valid JSON".to_string() } else { self.scrub(&e.to_string()) };
                    Err(not_elasticsearch(Some(EsError::java(500, "x_content_parse_exception", detail))))
                }
            };
        }
    }

    /// `GET /`: the remote's version (`lookupRemoteVersion`).
    pub async fn info(&self, retries: &mut u64) -> EsResult<RemoteInfo> {
        let (v, _) = self.json(reqwest::Method::GET, "/", None, retries).await?;
        let number = v.pointer(&["version", "number"]).and_then(Value::as_str).ok_or_else(|| not_elasticsearch(None))?.to_string();
        let version = Version::parse(&number).ok_or_else(|| not_elasticsearch(None))?;
        let s = |p: &[&str]| v.pointer(p).and_then(Value::as_str).map(|s| self.scrub(s).chars().take(256).collect::<String>());
        Ok(RemoteInfo { version, number, distribution: s(&["version", "distribution"]), cluster_name: s(&["cluster_name"]) })
    }

    /// One `GET` whose answer is JSON (the migration's listing and index definitions).
    pub async fn get(&self, path_query: &str) -> EsResult<Value> {
        let mut retries = 0;
        Ok(self.json(reqwest::Method::GET, path_query, None, &mut retries).await?.0)
    }
}

/// `URLEncoder.encode(s, UTF_8)`.
pub fn encode_index(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'-' | b'*' | b'_' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn order_name(o: crate::search_source::SortOrder) -> &'static str {
    match o {
        crate::search_source::SortOrder::Asc => "asc",
        crate::search_source::SortOrder::Desc => "desc",
    }
}

/// One page of the remote's documents.
pub struct Page {
    pub docs: Vec<RemoteDoc>,
    /// The first page's `hits.total`.
    pub total: Option<u64>,
    /// The page's bytes on the breaker, given back when the page is dropped.
    _held: Option<Reservation>,
}

enum Cursor {
    /// A scroll: its id once the first page came.
    Scroll(Option<String>),
    /// A point in time and the last hit's sort values.
    Pit { id: String, after: Option<Value> },
}

/// The remote's documents, page by page.
pub struct RemoteSource {
    client: Client,
    version: Version,
    cursor: Cursor,
    first: bool,
    done: bool,
    indices: String,
    size: u64,
    query: Value,
    source: Value,
    fetch_version: bool,
    /// Sorts the request named (`field:order`, deprecated in reindex), before the tiebreaker.
    sorts: Vec<(String, &'static str)>,
    /// The scroll's keep-alive (`scroll` of the request, 5m by default).
    keep_alive: crate::units::TimeValue,
    /// Rejected requests sent again (`retries.search`).
    pub retries: u64,
}

const PIT_SINCE: Version = Version(7, 10, 0);
const SHARD_DOC_SINCE: Version = Version(7, 12, 0);
const PIT_PARTIAL_SINCE: Version = Version(8, 16, 0);
const EXCLUDE_VECTORS_SINCE: Version = Version(9, 1, 0);
const PARTIAL_SINCE: Version = Version(6, 3, 0);
const NANOS_SINCE: Version = Version(5, 0, 0);

impl RemoteSource {
    /// Connects to the remote, learns its version and, for a point-in-time read, opens one.
    /// `limit` is `max_docs`: no page asks for more.
    pub async fn open(client: Client, req: &ByScroll, remote: &Remote, limit: Option<u64>) -> EsResult<RemoteSource> {
        let mut retries = 0;
        let info = client.info(&mut retries).await?;
        let src = &req.search.source;
        let mut size = req.batch();
        if let Some(l) = limit {
            size = size.min(l.max(1));
        }
        let indices: Vec<String> = req.search.indices.iter().map(|i| encode_index(i)).collect();
        let sorts = src
            .sort
            .iter()
            .filter_map(|s| match s {
                crate::search_source::Sort::Field { field, order, .. } => Some((field.clone(), order_name(order.unwrap_or(crate::search_source::SortOrder::Asc)))),
                crate::search_source::Sort::Score { order } => Some(("_score".to_string(), order_name(*order))),
                _ => None,
            })
            .collect();
        let filter = src.source.as_ref().filter(|f| !f.includes.is_empty() || !f.excludes.is_empty());
        let strings = |v: &[String]| Value::Arr(v.iter().map(|x| Value::Str(x.clone())).collect());
        let source = match (filter, info.version >= EXCLUDE_VECTORS_SINCE) {
            (Some(f), true) => obj! { "exclude_vectors" => false, "includes" => strings(&f.includes), "excludes" => strings(&f.excludes) },
            (Some(f), false) => obj! { "includes" => strings(&f.includes), "excludes" => strings(&f.excludes) },
            (None, true) => obj! { "exclude_vectors" => false, "includes" => Value::Arr(vec![]), "excludes" => Value::Arr(vec![]) },
            (None, false) => Value::Bool(true),
        };
        let keep_alive = req.search.scroll.unwrap_or_else(|| crate::units::TimeValue::parse("5m", "scroll").expect("5m is a time value"));
        let mut s = RemoteSource {
            version: info.version,
            cursor: Cursor::Scroll(None),
            first: true,
            done: false,
            indices: if indices.is_empty() { String::new() } else { format!("{}/", indices.join(",")) },
            size,
            query: remote.query.clone().unwrap_or_else(|| obj! { "match_all" => obj! { "boost" => 1.0 } }),
            source,
            fetch_version: src.version == Some(true),
            sorts,
            keep_alive,
            retries,
            client,
        };
        if s.version >= PIT_SINCE {
            let partial = if s.version >= PIT_PARTIAL_SINCE { "allow_partial_search_results=false&" } else { "" };
            let path = format!("/{}_pit?{partial}keep_alive=15m", s.indices);
            let (v, _) = s.client.json(reqwest::Method::POST, &path, None, &mut s.retries).await?;
            let id = v.get("id").and_then(Value::as_str).ok_or_else(|| EsError::illegal_state("open point-in-time response must contain [id] field"))?;
            s.cursor = Cursor::Pit { id: bounded_id(id, "point in time")?, after: None };
        }
        Ok(s)
    }

    /// The remote's version.
    pub fn version(&self) -> Version {
        self.version
    }

    /// The next page; an empty one once the remote has nothing more.
    pub async fn next(&mut self) -> EsResult<Page> {
        if self.done {
            return Ok(Page { docs: Vec::new(), total: None, _held: None });
        }
        let first = std::mem::replace(&mut self.first, false);
        let (v, held) = match &self.cursor {
            Cursor::Pit { id, after } => {
                let keep_alive = if first { Value::from("15m") } else { Value::from(format!("{PIT_KEEP_ALIVE_NANOS}nanos")) };
                let tiebreak = if self.version >= SHARD_DOC_SINCE { "_shard_doc" } else { "_doc" };
                let mut sort: Vec<Value> = self.sorts.iter().map(|(f, o)| obj! { f.as_str() => obj! { "order" => *o } }).collect();
                sort.push(obj! { tiebreak => obj! { "order" => "asc" } });
                let mut body = Map::new();
                body.insert("pit".into(), obj! { "id" => id.clone(), "keep_alive" => keep_alive });
                body.insert("size".into(), Value::from(self.size as i64));
                body.insert("version".into(), Value::Bool(self.fetch_version));
                body.insert("sort".into(), Value::Arr(sort));
                body.insert("query".into(), self.query.clone());
                body.insert("_source".into(), self.source.clone());
                if let Some(a) = after {
                    body.insert("search_after".into(), a.clone());
                }
                body.insert("track_total_hits".into(), Value::from(if first { i32::MAX as i64 } else { -1 }));
                let path = if self.version >= PARTIAL_SINCE { "/_search?allow_partial_search_results=false" } else { "/_search" };
                self.client.json(reqwest::Method::POST, path, Some(&Value::Obj(body).to_json()), &mut self.retries).await?
            }
            Cursor::Scroll(None) => {
                let keep_alive = if self.version < NANOS_SINCE { format!("{}ms", self.keep_alive.as_millis()) } else { self.keep_alive.string_rep() };
                let mut sorts: Vec<String> = self.sorts.iter().map(|(f, o)| format!("{f}:{o}")).collect();
                if sorts.is_empty() {
                    sorts.push("_doc:asc".into());
                }
                let mut q = format!("?scroll={keep_alive}&sort={}&size={}", encode_index(&sorts.join(",")), self.size);
                if self.version >= PARTIAL_SINCE {
                    q.push_str("&allow_partial_search_results=false");
                }
                q.push_str(&format!("&version={}", self.fetch_version));
                let body = obj! { "query" => self.query.clone(), "_source" => self.source.clone() };
                self.client.json(reqwest::Method::POST, &format!("/{}_search{q}", self.indices), Some(&body.to_json()), &mut self.retries).await?
            }
            Cursor::Scroll(Some(id)) => {
                let keep_alive = if self.version < NANOS_SINCE {
                    format!("{}ms", self.keep_alive.as_millis())
                } else {
                    format!("{}nanos", (self.keep_alive.as_millis().max(0) as u64).saturating_mul(1_000_000))
                };
                let body = obj! { "scroll_id" => id.clone() };
                self.client.json(reqwest::Method::POST, &format!("/_search/scroll?scroll={keep_alive}"), Some(&body.to_json()), &mut self.retries).await?
            }
        };
        if let Some(fail) = shard_failure(&v) {
            return Err(fail);
        }
        let docs = self.hits(&v)?;
        match &mut self.cursor {
            Cursor::Pit { id, after } => {
                if let Some(p) = v.get("pit_id").and_then(Value::as_str) {
                    *id = bounded_id(p, "point in time")?;
                }
                *after = v.pointer(&["hits", "hits"]).and_then(Value::as_array).and_then(|h| h.last()).and_then(|h| h.get("sort")).cloned();
                if after.is_none() && !docs.is_empty() {
                    return Err(not_elasticsearch(Some(EsError::illegal_state("Expected value in sort array"))));
                }
            }
            Cursor::Scroll(id) => {
                if let Some(s) = v.get("_scroll_id").and_then(Value::as_str) {
                    *id = Some(bounded_id(s, "scroll")?);
                }
            }
        }
        if docs.is_empty() {
            self.done = true;
        }
        let total = if first { total_hits(&v) } else { None };
        Ok(Page { docs, total, _held: held })
    }

    fn hits(&self, v: &Value) -> EsResult<Vec<RemoteDoc>> {
        let Some(hits) = v.pointer(&["hits", "hits"]) else { return Err(not_elasticsearch(None)) };
        let Value::Arr(hits) = hits else { return Err(not_elasticsearch(None)) };
        let mut out = Vec::with_capacity(hits.len());
        for h in hits {
            let s = |k: &str| h.get(k).and_then(Value::as_str).map(str::to_string);
            let (Some(index), Some(id)) = (s("_index"), s("_id")) else { return Err(not_elasticsearch(None)) };
            let routing = s("_routing").or_else(|| {
                h.get("fields").and_then(|f| f.get("_routing")).and_then(|r| match r {
                    Value::Arr(a) => a.first().and_then(Value::as_str).map(str::to_string),
                    Value::Str(x) => Some(x.clone()),
                    _ => None,
                })
            });
            let source = match h.get("_source") {
                None | Some(Value::Null) => None,
                Some(src @ Value::Obj(_)) => Some(Bytes::from(src.to_json())),
                Some(_) => return Err(not_elasticsearch(Some(EsError::java(500, "x_content_parse_exception", "[hit] failed to parse [_source]")))),
            };
            out.push(RemoteDoc { index, id, routing, version: h.get("_version").and_then(Value::as_i64).unwrap_or(-1), source });
        }
        Ok(out)
    }

    /// Releases the remote's search context (`DELETE /_pit` or `DELETE /_search/scroll`), best
    /// effort, as Elasticsearch logs and moves on.
    pub async fn close(&mut self) {
        let mut retries = 0;
        match &self.cursor {
            Cursor::Pit { id, .. } => {
                let body = obj! { "id" => id.clone() }.to_json();
                let _ = self.client.json(reqwest::Method::DELETE, "/_pit", Some(&body), &mut retries).await;
            }
            Cursor::Scroll(Some(id)) => {
                let body = obj! { "scroll_id" => Value::Arr(vec![Value::Str(id.clone())]) }.to_json();
                let _ = self.client.json(reqwest::Method::DELETE, "/_search/scroll", Some(&body), &mut retries).await;
            }
            Cursor::Scroll(None) => {}
        }
        self.done = true;
    }
}

/// A scroll or point-in-time id the remote gave, kept only up to [`MAX_ID_BYTES`].
fn bounded_id(id: &str, what: &str) -> EsResult<String> {
    if id.len() > MAX_ID_BYTES {
        return Err(EsError::illegal_argument(format!("the remote's {what} id is [{}] bytes, longer than the [{MAX_ID_BYTES}] roll keeps", id.len())));
    }
    Ok(id.to_string())
}

/// `hits.total`: a number, or `{"value": n}` (7.0 and later).
fn total_hits(v: &Value) -> Option<u64> {
    let n = match v.pointer(&["hits", "total"])? {
        Value::Obj(m) => m.get("value").and_then(Value::as_i64),
        t => t.as_i64(),
    };
    n.map(|n| n.max(0) as u64)
}

/// A page that failed on some shards (`allow_partial_search_results=false` keeps a recent
/// remote from sending one; an older one may): the reindex stops rather than skip documents.
fn shard_failure(v: &Value) -> Option<EsError> {
    let failures = v.pointer(&["_shards", "failures"]).and_then(Value::as_array).filter(|f| !f.is_empty())?;
    let reason = failures.first().and_then(|f| f.pointer(&["reason", "reason"]).or_else(|| f.get("reason"))).and_then(Value::as_str).unwrap_or("shard failure");
    Some(EsError::new(500, "search_phase_execution_exception", format!("remote search failed on [{}] shards: {reason}", failures.len())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(host: &str, port: u16) -> Remote {
        Remote { scheme: "http".into(), host: host.into(), port, ..Default::default() }
    }

    fn rules(settings: &[(&str, &str)]) -> Rules {
        Rules::of(|k| settings.iter().rev().find(|(n, _)| *n == k).map(|(_, v)| *v)).unwrap()
    }

    #[test]
    fn whitelist_patterns() {
        let r = rules(&[(WHITELIST, "127.0.0.1:*"), (ALLOW_PRIVATE, "true")]);
        assert!(r.check(&remote("127.0.0.1", 9200)).is_ok());
        assert!(rules(&[(WHITELIST, "10.0.0.1:9200, 127.0.0.1:*"), (ALLOW_PRIVATE, "true")]).check(&remote("127.0.0.1", 1)).is_ok());
        let e = r.check(&remote("okremote", 9200)).unwrap_err();
        assert_eq!(e.reason(), "[okremote:9200] not whitelisted in reindex.remote.whitelist");
        // Set but empty: every remote is refused, as in Elasticsearch.
        assert!(rules(&[(WHITELIST, "")]).check(&remote("example.com", 9200)).is_err());
        let both = rules(&[(WHITELIST, "*:*"), (BLOCKLIST, "bad.example:*")]);
        assert_eq!(both.check(&remote("bad.example", 9200)).unwrap_err().reason(), "[bad.example:9200] either not whitelisted in reindex.remote.whitelist or blocked in reindex.remote.blocklist");
        assert!(both.check(&remote("good.example", 9200)).is_ok());
    }

    #[test]
    fn whitelisted_hosts_only_private_ones_with_the_flag() {
        // Unset, as in Elasticsearch: every remote is refused.
        assert_eq!(rules(&[]).check(&remote("search.example.com", 443)).unwrap_err().reason(), "[search.example.com:443] not whitelisted in reindex.remote.whitelist");
        let r = rules(&[(WHITELIST, "*:*")]);
        assert!(r.check(&remote("search.example.com", 443)).is_ok());
        assert!(rules(&[]).without_whitelist().check(&remote("search.example.com", 443)).is_ok());
        for h in ["127.0.0.1", "10.1.2.3", "localhost", "[::1]", "169.254.169.254"] {
            let e = r.check(&remote(h, 9200)).unwrap_err();
            assert!(e.reason().contains("is a private address"), "{h}: {}", e.reason());
        }
        assert!(rules(&[(WHITELIST, "*:*"), (ALLOW_PRIVATE, "true")]).check(&remote("127.0.0.1", 9200)).is_ok());
        let mut ftp = remote("example.com", 21);
        ftp.scheme = "ftp".into();
        assert!(r.check(&ftp).is_err());
    }

    #[test]
    fn credentials_are_scrubbed() {
        let mut r = remote("example.com", 443);
        r.scheme = "https".into();
        r.username = Some("elastic".into());
        r.password = Some("hunter2-secret".into());
        r.headers = vec![("Authorization".into(), "ApiKey c2VjcmV0LWtleS12YWx1ZQ==".into())];
        let c = Client::new(&rules(&[(WHITELIST, "*:*")]), &r, None).unwrap();
        let echoed = "got ApiKey c2VjcmV0LWtleS12YWx1ZQ== and hunter2-secret";
        let s = c.scrub(echoed);
        assert!(!s.contains("hunter2") && !s.contains("c2VjcmV0"), "{s}");
        let dbg = format!("{r:?}");
        assert!(!dbg.contains("hunter2") && !dbg.contains("c2VjcmV0"), "{dbg}");
    }

    #[test]
    fn tasks_per_cluster_and_per_node_are_capped() {
        let node = RemoteNode::from_settings(&[]);
        let (a, b) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let held = [node.acquire(&a).unwrap(), node.acquire(&a).unwrap()];
        let e = node.acquire(&a).unwrap_err();
        assert_eq!((e.status, e.reason()), (429, "rejected execution of reindex from remote: the cluster runs [2] reindex from remote tasks, the most it may run at once"));
        let more = [node.acquire(&b).unwrap(), node.acquire(&b).unwrap()];
        let c = Arc::new(AtomicUsize::new(0));
        assert!(node.acquire(&c).unwrap_err().reason().contains("the node runs [4]"));
        assert_eq!(c.load(Ordering::SeqCst), 0, "a refusal holds nothing");
        drop(held);
        assert!(node.acquire(&a).is_ok());
        drop(more);
        assert_eq!(node.running.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn versions() {
        assert_eq!(Version::parse("9.5.5"), Some(Version(9, 5, 5)));
        assert_eq!(Version::parse("8.0.0-SNAPSHOT"), Some(Version(8, 0, 0)));
        assert_eq!(Version::parse("7.0.0-rc1"), Some(Version(7, 0, 0)));
        assert_eq!(Version::parse("nope"), None);
        assert!(Version(7, 10, 0) >= PIT_SINCE && Version(7, 9, 3) < PIT_SINCE);
    }

    #[test]
    fn index_names_are_url_encoded() {
        assert_eq!(encode_index("logs-*"), "logs-*");
        assert_eq!(encode_index("a b,c"), "a+b%2Cc");
        assert_eq!(encode_index("<x-{now}>"), "%3Cx-%7Bnow%7D%3E");
    }
}
