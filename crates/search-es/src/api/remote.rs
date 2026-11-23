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
//! The remote is a URL a tenant chose, so roll's rule for those applies (AGENTS.md), through the
//! node's egress check ([`crate::egress`]): names resolve to public addresses only, private IP
//! literals and `localhost` are refused, no redirect is followed, no
//! proxy is used, each read times out and a request's whole exchange is capped at 10 minutes. The
//! node setting [`ALLOW_PRIVATE`] lifts the address check (local development, tests). HTTPS is
//! verified against the platform's roots plus `reindex.ssl.certificate_authorities` (PEM files),
//! with `reindex.ssl.verification_mode` (`full`, `certificate`, `none`).
//!
//! The node's deny lists apply as to every outbound call ([`crate::egress`]): the operator's
//! networks and roll's own addresses are never reached, whatever name leads there.
//!
//! What a node spends on it is bounded by its operator, never by a tenant ([`RemoteNode`]): at
//! most [`MAX_PER_CLUSTER`] tasks per cluster, [`MAX_PER_OWNER`] per owner across their clusters
//! and [`MAX_PER_NODE`] per node run at once, shared in turn ([`crate::slots`]: more get a 429,
//! and keep their place in line while they ask again), every answer is read to at most [`BUFFER_LIMIT`] (16 MiB) and charged, three times
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
use crate::egress::{Egress, Refused, list, normal_host};
use crate::slots::{Caps, Full, Permit, Slots};
use std::sync::Arc;
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
/// Remote reindex tasks one owner's clusters may run at once on a node (2 unless set).
pub const MAX_PER_OWNER: &str = "roll.reindex.remote.max_per_owner";
/// Bytes a node's remote answers may hold at once, across its clusters (256mb unless set).
pub const MEMORY: &str = "roll.reindex.remote.memory";
/// How long one remote reindex task may run (24h unless set).
pub const MAX_DURATION: &str = "roll.reindex.remote.max_duration";
/// The longest scroll or point-in-time id kept from a remote.
pub const MAX_ID_BYTES: usize = 4096;
/// roll's own calls to a remote (a migration's listing and index definitions) at once: per node,
/// per owner and per cluster.
pub const CONTROL_SLOTS: Caps = Caps { node: 4, owner: 2, cluster: 1 };
/// The most one answer to a control call may hold.
pub const CONTROL_ANSWER_LIMIT: u64 = 1024 * 1024;
/// The most a remote's index listing (`_cat/indices`, scanned without a tree) may hold: about
/// 60,000 indices.
pub const CONTROL_LISTING_LIMIT: u64 = 8 * 1024 * 1024;
/// How long one control call's exchange may take.
pub const CONTROL_EXCHANGE: Duration = Duration::from_secs(30);
/// The control calls' slice of [`MEMORY`] (tasks never draw from it, nor they from the tasks').
const CONTROL_MEMORY: u64 = 32 * 1024 * 1024;
/// What an answer's parsed tree may cost per byte of structure: a node and its slot for every
/// value, a key for every member.
const TREE_BYTES_PER_TOKEN: u64 = 2 * std::mem::size_of::<Value>() as u64;

const DEFAULT_BUFFER_LIMIT: u64 = 16 * 1024 * 1024;
const DEFAULT_MEMORY: u64 = 256 * 1024 * 1024;
const DEFAULT_MAX_DURATION: Duration = Duration::from_secs(24 * 3600);
/// What one kept `_cat` entry (a column's name and value) costs besides the answer's bytes.
const CAT_ENTRY_BYTES: u64 = 160;
/// What one hit's decoded envelope (`_index`, `_id`, `_routing`) costs besides the page's bytes.
const HIT_OVERHEAD: u64 = 256;
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
/// runs at once, roll's own calls to remotes, the memory their answers hold and how long a task
/// runs. Read from the node's settings when the node starts; no tenant setting changes it.
pub struct RemoteNode {
    /// Remote reindex tasks ([`MAX_PER_NODE`], [`MAX_PER_OWNER`], [`MAX_PER_CLUSTER`]).
    pub tasks: Arc<Slots>,
    /// roll's own calls ([`CONTROL_SLOTS`]).
    pub control: Arc<Slots>,
    /// What the tasks' answers hold ([`MEMORY`] less the control slice).
    pub budget: Arc<MemoryBudget>,
    /// What roll's own calls to remotes hold ([`CONTROL_MEMORY`] of [`MEMORY`]).
    pub control_budget: Arc<MemoryBudget>,
    pub max_duration: Duration,
}

impl std::fmt::Debug for RemoteNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteNode")
            .field("tasks", &self.tasks)
            .field("control", &self.control)
            .field("memory", &self.budget.limit())
            .field("control_memory", &self.control_budget.limit())
            .finish_non_exhaustive()
    }
}

/// A running remote reindex task's slot, given back when dropped.
pub type RemoteSlot = Permit;
/// A control call's slot, given back when dropped.
pub type ControlSlot = Permit;

impl RemoteNode {
    /// The node's limits from its settings.
    pub fn from_settings(settings: &[(String, String)]) -> Arc<RemoteNode> {
        let get = |k: &str| settings.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        let count = |k: &str, d: usize| get(k).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(d);
        let memory = get(MEMORY).and_then(|v| crate::units::ByteSize::parse(v, MEMORY).ok()).map_or(DEFAULT_MEMORY, |b| b.0.max(1) as u64);
        let max_duration = get(MAX_DURATION)
            .and_then(|v| crate::units::TimeValue::parse(v, MAX_DURATION).ok())
            .map_or(DEFAULT_MAX_DURATION, |t| Duration::from_millis(t.as_millis().max(1) as u64));
        let control = CONTROL_MEMORY.min(memory / 8).max(1);
        let tasks = Caps { node: count(MAX_PER_NODE, 4), owner: count(MAX_PER_OWNER, 2), cluster: count(MAX_PER_CLUSTER, 2) };
        Arc::new(RemoteNode {
            tasks: Slots::new(tasks),
            control: Slots::new(CONTROL_SLOTS),
            budget: MemoryBudget::new((memory - control).max(1)),
            control_budget: MemoryBudget::new(control),
            max_duration,
        })
    }

    /// Whether one more task of `cluster` (of `owner`, [`crate::slots::owner_of`]) would get a
    /// slot now (roll's migration creates a copy only then); if not, the cluster keeps its place
    /// in line while it asks again.
    pub fn has_room(&self, cluster: &str, owner: &str) -> bool {
        self.tasks.ask(cluster, owner)
    }

    /// The most tasks one cluster runs at once.
    pub fn max_per_cluster(&self) -> usize {
        self.tasks.caps().cluster
    }

    /// A slot for one control call of `cluster` (of `owner`), or a 429.
    pub fn control_slot(&self, cluster: &str, owner: &str) -> EsResult<ControlSlot> {
        self.control.try_take(cluster, owner).map_err(|f| {
            rejected(match f {
                Full::Cluster(_) => "a migration call of this cluster is running: wait for it".into(),
                Full::Owner(n) => format!("your clusters run [{n}] migration calls on this node, the most they may run at once"),
                Full::Node(n) => format!("the node runs [{n}] migration calls, the most it may run at once"),
                Full::Queue => "other clusters are waiting for the node's migration calls: try again shortly".into(),
            })
        })
    }

    /// A slot for one remote reindex task of `cluster` (of `owner`, [`crate::slots::owner_of`]),
    /// or a 429 when the cluster, its owner or the node runs as many as it may (or others waited
    /// first).
    pub fn acquire(&self, cluster: &str, owner: &str) -> EsResult<RemoteSlot> {
        self.tasks.try_take(cluster, owner).map_err(|f| {
            rejected(match f {
                Full::Cluster(n) => format!("the cluster runs [{n}] reindex from remote tasks, the most it may run at once"),
                Full::Owner(n) => format!("the clusters of this cluster's owner run [{n}] reindex from remote tasks on this node, the most they may run at once"),
                Full::Node(n) => format!("the node runs [{n}] reindex from remote tasks, the most it may run at once"),
                Full::Queue => "other clusters are waiting for the node's reindex from remote slots: try again shortly".into(),
            })
        })
    }
}

fn rejected(why: String) -> EsError {
    EsError::new(429, "es_rejected_execution_exception", format!("rejected execution of reindex from remote: {why}"))
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
    egress: Egress,
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
        let allow_private = setting(ALLOW_PRIVATE).is_some_and(|v| v.eq_ignore_ascii_case("true"));
        Ok(Rules {
            egress: Egress::of(&setting, allow_private).map_err(EsError::illegal_argument)?,
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

    /// [`Rules::check`], with an address written out checked against what the node's own names
    /// resolve to now ([`crate::egress::DENY_NAMES`], resolved again if stale): for callers about to connect.
    pub async fn check_now(&self, r: &Remote) -> EsResult<()> {
        self.check(r)?;
        let url = reqwest::Url::parse(&format!("{}://{}:{}/", r.scheme, r.host, r.port)).map_err(|_| EsError::illegal_argument(format!("[host] [{}:{}] is not a valid host", r.host, r.port)))?;
        self.egress.check_literal_now(url.host_str().unwrap_or_default()).await.map_err(|e| refusal(e, &format!("{}:{}", r.host, r.port)))
    }

    /// `ReindexValidator.checkRemoteWhitelist`, then roll's address rule.
    pub fn check(&self, r: &Remote) -> EsResult<()> {
        if !matches!(r.scheme.as_str(), "http" | "https") {
            return Err(EsError::illegal_argument(format!("[host] must use the http or https scheme but was [{}]", r.scheme)));
        }
        // A header that would name another host than the one connected to.
        if let Some((k, _)) = r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("host") || k.starts_with(':')) {
            return Err(EsError::illegal_argument(format!("[headers] can't set [{k}]: the remote is the one [host] names")));
        }
        let target = format!("{}:{}", r.host, r.port);
        // The host exactly as the client connects to it (percent-decoded, IDNA-mapped, lower
        // case, an address in its canonical form), without a trailing dot: what the lists match.
        let url = reqwest::Url::parse(&format!("{}://{target}/", r.scheme)).map_err(|_| EsError::illegal_argument(format!("[host] [{target}] is not a valid host")))?;
        let host = url.host_str().map(normal_host).filter(|h| !h.is_empty()).ok_or_else(|| EsError::illegal_argument(format!("[host] [{target}] is not a valid host")))?;
        // The URL connected to is the one the lists match: the port named, nothing after the host
        // (a `?`, `#` or `\` in the host would move the port or the path).
        if url.port_or_known_default() != Some(r.port) || url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
            return Err(EsError::illegal_argument(format!("[host] [{target}] is not a valid host")));
        }
        let normal = format!("{host}:{}", r.port);
        let matches = |list: &[String]| list.iter().any(|p| crate::xcontent::filter::glob_match(&normal_host(p), &normal));
        // `ReindexValidator.checkRemoteWhitelist`: unset or empty, every remote is refused.
        let allowed = self.whitelist.as_deref().is_some_and(matches) && !matches(&self.blocklist);
        if !allowed {
            return Err(EsError::illegal_argument(if self.blocklist.is_empty() {
                format!("[{target}] not whitelisted in {WHITELIST}")
            } else {
                format!("[{target}] either not whitelisted in {WHITELIST} or blocked in {BLOCKLIST}")
            }));
        }
        // An address written out never reaches the resolver: checked here, in any of its forms.
        self.egress.check_url(&url).map_err(|e| refusal(e, &target))
    }
}

/// How reindex from remote says the node's egress rules refused `target`.
fn refusal(e: Refused, target: &str) -> EsError {
    match e {
        Refused::Private(_) => EsError::illegal_argument(format!("[{target}] is a private address, which reindex from remote may not connect to unless [{ALLOW_PRIVATE}] is set")),
        Refused::Denied(_) => EsError::illegal_argument(Refused::Denied(target.to_string()).to_string()),
        Refused::NotReady => EsError::java(503, "connect_exception", crate::egress::NOT_READY),
        Refused::Unresolved(_) => EsError::java(500, "unknown_host_exception", format!("{target}: Name or service not known")),
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
    /// Never quote what the remote answered, credentials or not (roll's own calls).
    quiet: bool,
    limit: u64,
    /// One request's whole exchange.
    exchange: Duration,
    budget: Option<Arc<MemoryBudget>>,
}

/// An answer read whole (within the limit), with the breaker bytes it holds.
struct Body {
    status: u16,
    bytes: Bytes,
    held: Option<Reservation>,
}

/// A parsed answer and what it's charged for, given back when it's dropped.
pub struct Charged {
    pub value: Value,
    _held: Option<Reservation>,
}

/// An upper bound of what `b`'s tree costs: every value a node and a slot, every member a key.
fn tree_cost(b: &[u8]) -> u64 {
    let tokens = b.iter().filter(|c| matches!(c, b'{' | b'[' | b',' | b':')).count() as u64 + 1;
    tokens * TREE_BYTES_PER_TOKEN + b.len() as u64
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
        b = b.dns_resolver(rules.egress.resolver());
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
        Ok(Client {
            base: format!("{host}{prefix}"),
            host,
            http,
            socket_timeout: socket_timeout.min(MAX_EXCHANGE),
            secrets,
            credentials,
            quiet: false,
            limit: rules.buffer_limit,
            exchange: MAX_EXCHANGE,
            budget,
        })
    }

    /// For roll's own calls (a migration's listing and definitions): answers of at most
    /// `limit` bytes, exchanges of at most [`CONTROL_EXCHANGE`], and errors that never quote the
    /// remote's answer (its status and `error.type` only) nor tell one transport failure from
    /// another.
    pub fn for_control(mut self, limit: u64) -> Client {
        self.limit = self.limit.min(limit);
        self.exchange = CONTROL_EXCHANGE;
        self.quiet = true;
        self
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
        if lower.contains("private address") || lower.contains("may not reach") {
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
        let mut rb = self.http.request(method.clone(), &url).timeout(self.exchange);
        if let Some(b) = body {
            rb = rb.header(reqwest::header::CONTENT_TYPE, "application/json; charset=UTF-8").body(b.to_string());
        }
        let mut resp = rb.send().await.map_err(|e| self.transport_error(&e))?;
        let read = async {
        let status = resp.status().as_u16();
        if let Some(n) = resp.content_length().filter(|n| *n > self.limit) {
            return Err(too_large(n, self.limit));
        }
        let mut held = self.budget.clone().map(Reservation::new);
        let mut bytes = bytes::BytesMut::with_capacity(resp.content_length().unwrap_or(4096).min(self.limit).min(1 << 20) as usize);
        while let Some(chunk) = resp.chunk().await.map_err(|e| self.transport_error(&e))? {
            let n = (bytes.len() + chunk.len()) as u64;
            if n > self.limit {
                return Err(too_large(n, self.limit));
            }
            if let Some(h) = held.as_mut() {
                h.add(chunk.len() as u64, BREAKER_LABEL)?;
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Body { status, bytes: bytes.freeze(), held })
        };
        read.await
    }

    /// `wrapExceptionToPreserveStatus`: the remote's answer as the reason, its status kept.
    /// After a request that carried credentials, the answer is left out (its status stays).
    fn status_error(&self, method: &reqwest::Method, path_query: &str, b: &Body) -> EsError {
        let text = if self.credentials || self.quiet {
            // The error's type alone (a name, not text the remote chose freely).
            let kind = (b.bytes.len() <= 64 * 1024)
                .then(|| crate::json::parse(&b.bytes).ok().flatten())
                .flatten()
                .and_then(|v| v.pointer(&["error", "type"]).and_then(Value::as_str).map(str::to_string))
                .filter(|t| t.len() <= 100 && t.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'));
            let why = if self.credentials { "the request carried credentials" } else { "roll doesn't quote a remote's answer here" };
            match kind {
                Some(k) => format!("[the remote's answer isn't shown: {why}; error type [{k}], {} bytes]", b.bytes.len()),
                None => format!("[the remote's answer isn't shown: {why}; {} bytes]", b.bytes.len()),
            }
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

    /// One request answered with a 2xx; a rejected one (429) is sent again with Elasticsearch's
    /// backoff, each retry counted in `retries`.
    async fn checked(&self, method: &reqwest::Method, path_query: &str, body: Option<&str>, retries: &mut u64) -> EsResult<Body> {
        let mut attempt = 0u32;
        loop {
            let b = self.exchange(method, path_query, body).await?;
            if b.status == 429 && attempt < MAX_RETRIES {
                // `BackoffPolicy.exponentialBackoff`: start + 10 * (e^(0.8 * n) - 1) ms.
                let wait = RETRY_INITIAL_MS + 10 * (((0.8 * attempt as f64).exp() as u64).saturating_sub(1));
                attempt += 1;
                *retries += 1;
                tokio::time::sleep(Duration::from_millis(wait)).await;
                continue;
            }
            if !(200..300).contains(&b.status) {
                return Err(self.status_error(method, path_query, &b));
            }
            return Ok(b);
        }
    }

    /// One request whose answer is JSON, parsed into a tree: its cost ([`tree_cost`]) is charged
    /// before the tree is built and held until it's dropped. For small answers (`GET /`, a point
    /// in time's id, a migration's listing); a page of hits is scanned instead ([`page`](Self::page)).
    async fn json(&self, method: reqwest::Method, path_query: &str, body: Option<&str>, retries: &mut u64) -> EsResult<Charged> {
        let mut b = self.checked(&method, path_query, body, retries).await?;
        if let Some(h) = b.held.as_mut() {
            h.add(tree_cost(&b.bytes), BREAKER_LABEL)?;
        }
        match crate::json::parse(&b.bytes) {
            // Any JSON: `_cat` answers an array; callers check the shape they need.
            Ok(Some(v)) => Ok(Charged { value: v, _held: b.held }),
            Ok(None) => Err(not_elasticsearch(None)),
            Err(e) => {
                let detail = if self.credentials || self.quiet { "the answer isn't valid JSON".to_string() } else { self.scrub(&e.to_string()) };
                Err(not_elasticsearch(Some(EsError::java(500, "x_content_parse_exception", detail))))
            }
        }
    }

    /// A page of hits, scanned without a tree ([`super::remote_page`]): at most `max_hits` hits
    /// (the `size` asked for), each charged as it's read; the page's bytes and its hits are held
    /// while the page is.
    async fn page(&self, method: reqwest::Method, path_query: &str, body: Option<&str>, max_hits: usize, retries: &mut u64) -> EsResult<(super::remote_page::ScannedPage, Option<Reservation>)> {
        let mut b = self.checked(&method, path_query, body, retries).await?;
        let mut budget = super::remote_page::HitBudget { max_hits, per_hit: HIT_OVERHEAD, held: b.held.as_mut() };
        let page = super::remote_page::scan(&b.bytes, MAX_ID_BYTES, &mut budget).map_err(|e| match &*e.kind {
            "illegal_argument_exception" | "circuit_breaking_exception" | "illegal_state_exception" => e,
            _ => not_elasticsearch(Some(e)),
        })?;
        Ok((page, b.held))
    }

    /// `GET /`: the remote's version (`lookupRemoteVersion`).
    pub async fn info(&self, retries: &mut u64) -> EsResult<RemoteInfo> {
        let charged = self.json(reqwest::Method::GET, "/", None, retries).await?;
        let v = &charged.value;
        let number = v.pointer(&["version", "number"]).and_then(Value::as_str).ok_or_else(|| not_elasticsearch(None))?.to_string();
        let version = Version::parse(&number).ok_or_else(|| not_elasticsearch(None))?;
        let s = |p: &[&str]| v.pointer(p).and_then(Value::as_str).map(|s| self.scrub(s).chars().take(256).collect::<String>());
        Ok(RemoteInfo { version, number, distribution: s(&["version", "distribution"]), cluster_name: s(&["cluster_name"]) })
    }

    /// One `GET` whose answer is JSON (the migration's listing and index definitions), its tree
    /// charged while it's held.
    pub async fn get(&self, path_query: &str) -> EsResult<Charged> {
        let mut retries = 0;
        self.json(reqwest::Method::GET, path_query, None, &mut retries).await
    }

    /// One `GET` of a `_cat` API in `format=json`, read row by row without a tree
    /// ([`super::remote_page::scan_rows`]): the answer's bytes and each row charged while held.
    pub async fn get_rows(&self, path_query: &str, columns: &[&str], max_rows: usize) -> EsResult<(super::remote_page::CatRows, Option<Reservation>)> {
        let mut retries = 0;
        let mut b = self.checked(&reqwest::Method::GET, path_query, None, &mut retries).await?;
        let rows = super::remote_page::scan_rows(&b.bytes, b.held.as_mut(), columns, CAT_ENTRY_BYTES, max_rows).map_err(|e| match &*e.kind {
            "circuit_breaking_exception" => e,
            _ => not_elasticsearch(Some(e)),
        })?;
        Ok((rows, b.held))
    }

    /// `scheme://host:port`, as errors name the remote.
    pub fn host(&self) -> &str {
        &self.host
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
            let opened = s.client.json(reqwest::Method::POST, &path, None, &mut s.retries).await?;
            let id = opened.value.get("id").and_then(Value::as_str).ok_or_else(|| EsError::illegal_state("open point-in-time response must contain [id] field"))?;
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
        let (page, held) = match &self.cursor {
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
                self.client.page(reqwest::Method::POST, path, Some(&Value::Obj(body).to_json()), self.size as usize, &mut self.retries).await?
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
                self.client.page(reqwest::Method::POST, &format!("/{}_search{q}", self.indices), Some(&body.to_json()), self.size as usize, &mut self.retries).await?
            }
            Cursor::Scroll(Some(id)) => {
                let keep_alive = if self.version < NANOS_SINCE {
                    format!("{}ms", self.keep_alive.as_millis())
                } else {
                    format!("{}nanos", (self.keep_alive.as_millis().max(0) as u64).saturating_mul(1_000_000))
                };
                let body = obj! { "scroll_id" => id.clone() };
                self.client.page(reqwest::Method::POST, &format!("/_search/scroll?scroll={keep_alive}"), Some(&body.to_json()), self.size as usize, &mut self.retries).await?
            }
        };
        // A page that failed on some shards (`allow_partial_search_results=false` keeps a recent
        // remote from sending one; an older one may): the reindex stops rather than skip
        // documents. Its reason is the remote's text: left out once credentials were sent.
        if let Some((n, reason)) = &page.shard_failures {
            let reason = if self.client.credentials { "[the remote's reason isn't shown: the request carried credentials]".to_string() } else { self.client.scrub(reason) };
            return Err(EsError::new(500, "search_phase_execution_exception", format!("remote search failed on [{n}] shards: {reason}")));
        }
        if !page.has_hits {
            return Err(not_elasticsearch(None));
        }
        match &mut self.cursor {
            Cursor::Pit { id, after } => {
                if let Some(p) = page.pit_id {
                    *id = p;
                }
                *after = page.last_sort;
                if after.is_none() && !page.hits.is_empty() {
                    return Err(not_elasticsearch(Some(EsError::illegal_state("Expected value in sort array"))));
                }
            }
            Cursor::Scroll(id) => {
                if let Some(s) = page.scroll_id {
                    *id = Some(s);
                }
            }
        }
        if page.hits.is_empty() {
            self.done = true;
        }
        let docs = page.hits.into_iter().map(|h| RemoteDoc { index: h.index, id: h.id, routing: h.routing, version: h.version, source: h.source }).collect();
        let total = if first { page.total } else { None };
        Ok(Page { docs, total, _held: held })
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

    /// The lists match the host the client connects to: percent-encoded, fullwidth and IDNA
    /// spellings of a blocked name are blocked; a header can't name another host.
    #[test]
    fn hosts_match_as_the_client_connects_to_them() {
        let r = rules(&[(WHITELIST, "*:*"), (BLOCKLIST, "*.search.example.com:*")]);
        for h in ["victim%2Esearch%2Eexample%2Ecom", "victim\u{FF0E}search\u{FF0E}example\u{FF0E}com", "victim.\u{17F}earch.example.com", "VICTIM.Search.Example.COM.", "victim.search.example.com"] {
            let e = r.check(&remote(h, 443)).err().map(|e| e.reason().to_string()).unwrap_or_default();
            assert!(e.ends_with("blocked in reindex.remote.blocklist"), "{h}: {e}");
        }
        assert!(r.check(&remote("other.example.com", 443)).is_ok());
        // A host that would connect to another port or path than the lists matched.
        for h in ["good.example?", "good.example#", "good.example\\x", "good.example/x"] {
            assert!(r.check(&remote(h, 9200)).err().is_some_and(|e| e.reason().ends_with("is not a valid host")), "{h}");
        }
        for k in ["Host", "host", ":authority"] {
            let mut x = remote("other.example.com", 443);
            x.headers = vec![(k.into(), "victim.search.example.com".into())];
            assert!(r.check(&x).err().is_some_and(|e| e.reason().contains("can't set")), "{k}");
        }
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
    fn tasks_per_cluster_per_owner_and_per_node_are_capped() {
        let node = RemoteNode::from_settings(&[]);
        let held = [node.acquire("a", "alice").unwrap(), node.acquire("a", "alice").unwrap()];
        let e = node.acquire("a", "alice").unwrap_err();
        assert_eq!((e.status, e.reason()), (429, "rejected execution of reindex from remote: the cluster runs [2] reindex from remote tasks, the most it may run at once"));
        // Alice's second cluster: her clusters hold her share of the node.
        assert!(node.acquire("a2", "alice").unwrap_err().reason().contains("of this cluster's owner run [2]"));
        let (b1, _b2) = (node.acquire("b", "bob").unwrap(), node.acquire("b", "bob").unwrap());
        assert!(node.acquire("c", "carol").unwrap_err().reason().contains("the node runs [4]"));
        assert_eq!(node.tasks.held("c"), 0, "a refusal holds nothing");
        // A task of Bob's ends: Alice's clusters are still at her share, and Bob can't take back
        // the slot Carol waits for.
        drop(b1);
        assert!(node.acquire("a2", "alice").unwrap_err().reason().contains("owner"));
        assert!(node.acquire("b", "bob").unwrap_err().reason().contains("waiting"));
        let _c = node.acquire("c", "carol").unwrap();
        drop(held);
        assert_eq!((node.tasks.held("a"), node.tasks.held("b"), node.tasks.held("c")), (0, 1, 1));
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
