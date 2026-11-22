//! Migrating into roll from Elasticsearch or OpenSearch (`POST /v1/search/clusters/{id}/migrate/
//! inspect` and `POST /v1/search/clusters/{id}/migrate`), the dashboard's "Migrate from
//! Elasticsearch / OpenSearch".
//!
//! Everything the remote is asked goes through roll's server, never from the browser: search-es'
//! guarded client ([`search_es::api::remote`]: public addresses only unless the node allows
//! private ones, never one the node denies, no redirects, bounded answers). The remote's
//! credentials come in the request body, are used for this request alone and are never stored,
//! returned or logged; the reindex tasks the migration starts hold them in memory while they run
//! and never print them (their description names the host alone).
//!
//! These are roll's own calls to a remote a tenant named, so they're bounded on their own terms:
//! [`remote::CONTROL_SLOTS`] (one at a time per cluster, two per owner and four per node, taken in
//! turn: 429 past them, [`search_es::slots`]), only
//! the fixed paths below, answers of at most [`remote::CONTROL_ANSWER_LIMIT`] (definitions
//! [`DEFINITION_LIMIT`]) charged to the node's control budget (a slice of the reindex budget
//! tasks never draw from), [`DEADLINE`] for the whole call, and errors that never quote the
//! remote (its status and `error.type` at most) nor tell one transport failure from another.
//!
//! `inspect` (`GET /`, `/_cat/indices`, `/_alias`, `/_data_stream`) lists the remote's open
//! indices (documents, size, aliases) and says which names, and which of their aliases, roll
//! already has; it reads under the node's rules but not the cluster's whitelist. `migrate` checks
//! the remote answers (`GET /`), adds exactly its `host:port` to the cluster's
//! `reindex_whitelist`, then for each picked index, while a reindex task slot is free, copies its
//! mappings, settings and aliases (`/{index}/_mapping`, `/_settings`, `/_alias`) into a new index
//! and starts a reindex from remote into it (`wait_for_completion=false`). Indices past the free
//! slots come back `queued`, with nothing created: the dashboard starts them as earlier copies
//! end. Settings roll doesn't take are dropped and listed with the reason; an alias roll already
//! has is never joined.

use crate::control::{Control, ControlError, Result};
use crate::store::SearchCluster;
use search_es::api::by_query_request::Remote;
use search_es::api::remote::{self, Client, Rules};
use search_es::json::{Map as EsMap, Value as EsValue};
use search_es::EsRequest;
use serde::Deserialize;
use serde_json::{json, Value};

/// The most indices one `migrate` call takes.
pub const MAX_INDICES: usize = 100;
/// The most indices `inspect` lists (it says how many more there are).
pub const MAX_LISTED: usize = 10_000;
/// What one listed index costs in the answer `inspect` builds, besides its strings: a map of a
/// dozen entries (keys, values and the map's own nodes come to about 2 KiB).
const LISTED_ROW_BYTES: u64 = 2048;
/// What one string in that answer costs besides its bytes.
const STRING_BYTES: u64 = 48;
/// The most alias names one listed index carries (the row says how many more it has).
pub const MAX_ALIASES_LISTED: usize = 20;
/// The most data stream names `inspect` names.
pub const MAX_STREAMS_LISTED: usize = 100;
/// The columns `inspect` asks the remote's `_cat/indices` for, and keeps.
const LISTING_COLUMNS: &[&str] = &["index", "health", "status", "docs.count", "store.size"];
/// How long one `inspect` or `migrate` call may take, all its requests to the remote included:
/// `migrate` starts the copies (tasks) and returns, it never waits for one.
pub const DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);
/// The most one index definition (its mappings, its settings or its aliases) may be.
pub const DEFINITION_LIMIT: u64 = 4 * 1024 * 1024;
/// Index creations tried per index while dropping the settings roll refuses.
const MAX_ATTEMPTS: usize = 32;

/// The remote cluster and how to log in to it. Its `Debug` leaves the credentials out.
#[derive(Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// `https://host[:port][/prefix]`: Elastic Cloud's or OpenSearch's endpoint, or a
    /// self-hosted node's. Without a scheme, `https`; without a port, the scheme's.
    pub host: String,
    /// An Elasticsearch API key (its `encoded` value), sent as `Authorization: ApiKey …`.
    pub api_key: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source").field("host", &self.host).field("api_key", &self.api_key.as_ref().map(|_| "<<>>")).field("username", &self.username).finish_non_exhaustive()
    }
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct InspectRequest {
    pub source: Source,
}

/// One index to copy: the remote's name and, when it should differ, roll's.
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Pick {
    pub index: String,
    pub dest: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct MigrateRequest {
    pub source: Source,
    pub indices: Vec<Pick>,
    /// Each task's `requests_per_second` (unthrottled when absent).
    pub requests_per_second: Option<f64>,
    /// Documents per page read from the remote (1000 unless set; lower it for large documents).
    pub size: Option<u32>,
}

/// `host` as reindex's `source.remote.host` takes it: a scheme, a DNS name or an IP literal, a
/// port, and a path of plain segments, no trailing slash. Anything else (`*`, `?`, spaces, a
/// query, credentials) is refused: it would end up in the cluster's whitelist as a pattern.
pub fn normalize_host(host: &str) -> Result<String> {
    let h = host.trim().trim_end_matches('/');
    if h.is_empty() {
        return Err(ControlError::Invalid("host is required".into()));
    }
    let with_scheme = if h.contains("://") { h.to_string() } else { format!("https://{h}") };
    let url = reqwest::Url::parse(&with_scheme).map_err(|_| ControlError::Invalid(format!("[{}] isn't a URL", redact_userinfo(h))))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ControlError::Invalid(format!("the host must use http or https, not {}", url.scheme())));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ControlError::Invalid("put the username and password in their own fields, not in the URL".into()));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ControlError::Invalid("the host can't have a query or a fragment".into()));
    }
    let raw = url.host_str().ok_or_else(|| ControlError::Invalid("the URL names no host".into()))?;
    let host = raw.trim_end_matches('.');
    let ip = host.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>().is_ok();
    let name = !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'));
    if !ip && !name {
        return Err(ControlError::Invalid(format!("[{}] isn't a host name or an IP address", raw.chars().take(100).collect::<String>())));
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let path = url.path().trim_end_matches('/');
    if !path.is_empty() && !path.split('/').skip(1).all(|s| !s.is_empty() && s != "." && s != ".." && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"-._~".contains(&c))) {
        return Err(ControlError::Invalid("the URL's path holds characters a path prefix can't".into()));
    }
    Ok(format!("{}://{host}:{port}{path}", url.scheme()))
}

fn redact_userinfo(h: &str) -> String {
    match h.split_once("://") {
        Some((s, rest)) => match rest.split('/').next().and_then(|a| a.rfind('@')) {
            Some(at) => format!("{s}://{}", &rest[at + 1..]),
            None => h.to_string(),
        },
        None => h.rsplit_once('@').map_or(h, |(_, r)| r).to_string(),
    }
}

/// The reindex `source.remote` object for `s` (credentials included: it goes to the cluster in
/// process, never back to the caller).
fn remote_json(s: &Source) -> Result<Value> {
    let host = normalize_host(&s.host)?;
    let mut r = json!({ "host": host, "socket_timeout": "60s", "connect_timeout": "10s" });
    match (&s.api_key, &s.username) {
        (Some(_), Some(_)) => return Err(ControlError::Invalid("give an API key or a username and password, not both".into())),
        (Some(k), None) => {
            let k = k.trim();
            if k.is_empty() {
                return Err(ControlError::Invalid("api_key is empty".into()));
            }
            r["headers"] = json!({ "Authorization": format!("ApiKey {k}") });
        }
        (None, Some(u)) => {
            r["username"] = json!(u);
            r["password"] = json!(s.password.clone().unwrap_or_default());
        }
        (None, None) if s.password.is_some() => return Err(ControlError::Invalid("a password needs a username".into())),
        (None, None) => {}
    }
    Ok(r)
}

/// The search-es [`Remote`] for `s` (the same object reindex parses).
fn remote_of(s: &Source) -> Result<Remote> {
    let j = remote_json(s)?;
    let mut r = Remote::parse(j["host"].as_str().unwrap_or_default()).map_err(|e| ControlError::Invalid(e.reason().to_string()))?;
    r.username = j["username"].as_str().map(str::to_string);
    r.password = j["password"].as_str().map(str::to_string);
    if let Some(h) = j["headers"]["Authorization"].as_str() {
        r.headers.push(("Authorization".into(), h.to_string()));
    }
    r.connect_timeout = search_es::units::TimeValue::parse("10s", "connect_timeout").ok();
    r.socket_timeout = search_es::units::TimeValue::parse("30s", "socket_timeout").ok();
    Ok(r)
}

/// What the remote answered, as the caller sees it: a transport failure of any kind is one
/// message (no port scan reads which), a refusal its status and `error.type` (the client quotes
/// nothing more); a 429 of roll's own limits stays a 429.
fn remote_error(host: &str, e: search_es::EsError) -> ControlError {
    if e.status == 429 && e.kind == "es_rejected_execution_exception" {
        return ControlError::Busy(e.reason().to_string());
    }
    // roll's own limits are roll's errors, not the remote's.
    if e.kind == "circuit_breaking_exception" || e.reason().starts_with("Remote responded with a chunk that was too large") {
        return ControlError::Invalid(format!("the answer from {host} is larger than roll reads here at once (roll's limit)"));
    }
    let transport = matches!(&*e.kind, "connect_exception" | "unknown_host_exception" | "s_s_l_handshake_exception" | "socket_timeout_exception" | "i_o_exception");
    if transport {
        return ControlError::Invalid(format!("couldn't reach the remote at {host}"));
    }
    if e.kind == "status_exception" {
        return ControlError::Invalid(format!("the remote at {host} answered {}: {}", e.status, e.reason()));
    }
    ControlError::Invalid(format!("the remote at {host}: {}: {}", e.kind, e.reason()))
}

/// A small value of the remote's for an answer (a dropped setting's value, at most 1 KiB of it).
fn small(v: &EsValue) -> Value {
    let text = v.to_json();
    if text.len() > 1024 {
        return Value::from(format!("[{} bytes]", text.len()));
    }
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

/// `inspect`'s answer, serialized, and the charge for it, sent with [`Inspected::into_stream`].
pub struct Inspected {
    body: bytes::Bytes,
    held: search_es::breaker::Reservation,
}

/// How much of `inspect`'s answer one poll hands the connection.
const SEND_CHUNK: usize = 64 * 1024;
/// How long a client has to take `inspect`'s answer: past it the answer and its charge are given
/// back and the connection is closed.
const SEND_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

impl Inspected {
    /// The answer as a body stream that owns it and its charge: each poll hands on a copy of the
    /// next [`SEND_CHUNK`] (a slice would keep the whole answer alive), and the answer and its
    /// charge go once the last chunk is taken, the stream is dropped (the connection closed), or
    /// [`SEND_DEADLINE`] passes. A client that stops reading holds about one chunk past the
    /// charge, and the charge only until the deadline.
    pub fn into_stream(self) -> impl futures::Stream<Item = std::io::Result<bytes::Bytes>> + Send + 'static {
        let left = std::sync::Arc::new(parking_lot::Mutex::new(Some((self.body, self.held))));
        let reaper = std::sync::Arc::downgrade(&left);
        tokio::spawn(async move {
            tokio::time::sleep(SEND_DEADLINE).await;
            if let Some(left) = reaper.upgrade() {
                left.lock().take();
            }
        });
        let (mut off, mut ended) = (0, false);
        futures::stream::poll_fn(move |_| {
            if ended {
                return std::task::Poll::Ready(None);
            }
            let mut g = left.lock();
            let Some((body, _)) = g.as_ref() else {
                ended = true;
                return std::task::Poll::Ready(Some(Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "the answer wasn't read in time"))));
            };
            let end = (off + SEND_CHUNK).min(body.len());
            let chunk = bytes::Bytes::copy_from_slice(&body[off..end]);
            off = end;
            if off == body.len() {
                g.take();
                ended = true;
            }
            std::task::Poll::Ready(Some(Ok(chunk)))
        })
    }
}

/// The rows `inspect` answers with, from the remote's `_cat/indices` rows and `/_alias` answer:
/// at most [`MAX_ALIASES_LISTED`] alias names per index (and how many more), each string charged
/// at its length as it's added to `held`. A listing that names one index twice is refused, so no
/// part of the answer grows as the product of two counts the remote chose.
fn listing(rows: &[search_es::api::remote_page::CatRow], aliases: Option<&EsValue>, existing: &std::collections::HashSet<String>, held: &mut search_es::breaker::Reservation) -> search_es::EsResult<Vec<Value>> {
    let label = "migrate_listing";
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let col = |k: &str| r.iter().find(|(n, _)| n == k).and_then(|(_, v)| v.clone());
        let Some(index) = col("index") else { continue };
        if index.starts_with('.') {
            continue;
        }
        held.add(LISTED_ROW_BYTES + 2 * STRING_BYTES + 2 * index.len() as u64, label)?;
        if !seen.insert(index.clone()) {
            return Err(search_es::EsError::illegal_argument(format!("the remote lists index [{index}] twice")));
        }
        let (health, status) = (col("health"), col("status"));
        held.add(2 * STRING_BYTES + health.as_ref().map_or(0, |s| s.len()) as u64 + status.as_ref().map_or(0, |s| s.len()) as u64, label)?;
        let all = aliases.and_then(|a| a.get(&index)).and_then(|d| d.get("aliases")).and_then(EsValue::as_object);
        let total = all.map_or(0, |m| m.len());
        let (mut names, mut on_roll) = (Vec::new(), Vec::new());
        for name in all.into_iter().flat_map(|m| m.keys()).take(MAX_ALIASES_LISTED) {
            held.add(STRING_BYTES + name.len() as u64, label)?;
            if existing.contains(name) {
                held.add(STRING_BYTES + name.len() as u64, label)?;
                on_roll.push(name.clone());
            }
            names.push(name.clone());
        }
        let n = |k: &str| col(k).and_then(|v| v.parse::<i64>().ok()).map(Value::from).unwrap_or(Value::Null);
        out.push(json!({
            "index": index,
            "health": health,
            "status": status,
            "docs": n("docs.count"),
            "size_bytes": n("store.size"),
            "primary_size_bytes": Value::Null,
            "primaries": Value::Null,
            "replicas": Value::Null,
            "aliases": names,
            "aliases_more": total.saturating_sub(MAX_ALIASES_LISTED),
            "aliases_on_roll": on_roll,
            "exists": existing.contains(&index),
        }));
    }
    out.sort_by(|a, b| a["index"].as_str().cmp(&b["index"].as_str()));
    Ok(out)
}

/// What happened to one index of a `migrate` call so far (reported even when the call's
/// deadline cuts it short).
#[derive(Default)]
struct Progress {
    created: bool,
    task: Option<String>,
    /// The empty copy was being removed (every task slot was taken).
    removing: bool,
}

impl Control {
    /// The node's reindex-from-remote rules ([`Rules`]) from its node settings.
    fn remote_rules(&self) -> Result<Rules> {
        let settings = &self.cfg.node_settings;
        Rules::of(|k| settings.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.as_str())).map_err(|e| ControlError::Internal(anyhow::anyhow!("reindex from remote settings: {}", e.reason())))
    }

    /// A client for roll's own calls to the remote: the node's rules (the deny list, the
    /// blocklist, private addresses; not the cluster's whitelist), answers of at most `limit`
    /// charged to the node's control budget, quiet errors.
    async fn control_client(&self, s: &Source, limit: u64) -> Result<Client> {
        let r = remote_of(s)?;
        let host = normalize_host(&s.host)?;
        let budget = self.node.reindex().control_budget.clone();
        let rules = self.remote_rules()?.without_whitelist();
        // An address roll refuses (private, denied, blocked) reads like any remote it can't
        // reach: no oracle for which names lead where.
        let refused = |e: search_es::EsError| {
            let reason = e.reason();
            if reason.contains("private address") || reason.contains("may not reach") || reason.contains("blocked in") || reason.contains("not whitelisted") {
                ControlError::Invalid(format!("couldn't reach the remote at {host}"))
            } else {
                ControlError::Invalid(reason.to_string())
            }
        };
        rules.check_now(&r).await.map_err(refused)?;
        Client::new(&rules, &r, Some(budget)).map(|c| c.for_control(limit)).map_err(refused)
    }

    /// A place in the node's pool of control calls for cluster `c` (429 when it's taken).
    fn control_slot(&self, c: &SearchCluster) -> Result<remote::ControlSlot> {
        let owner = search_es::slots::owner_of(c.owner.as_deref(), &c.name);
        self.node.reindex().control_slot(&c.name, &owner).map_err(|e| ControlError::Busy(e.reason().to_string()))
    }

    /// Every index and alias name cluster `c` has (an alias of the remote's by one of these
    /// names isn't copied: the copy would join what's there).
    async fn roll_names(&self, c: &SearchCluster) -> Result<std::collections::HashSet<String>> {
        let (status, v) = self.call_json(c, "GET", "/_alias?expand_wildcards=all", None).await;
        if status != 200 {
            return Err(ControlError::Internal(anyhow::anyhow!("listing cluster {}'s aliases failed ({status})", c.name)));
        }
        let mut names = std::collections::HashSet::new();
        for (index, def) in v.as_object().into_iter().flatten() {
            names.insert(index.clone());
            names.extend(def["aliases"].as_object().into_iter().flatten().map(|(a, _)| a.clone()));
        }
        Ok(names)
    }

    /// One request on cluster `c` as its owner: status and parsed body.
    async fn call_json(&self, c: &SearchCluster, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        self.call_text(c, method, path, body.map(|b| b.to_string())).await
    }

    /// [`Control::call_json`] with a body already rendered.
    async fn call_text(&self, c: &SearchCluster, method: &str, path: &str, body: Option<String>) -> (u16, Value) {
        let mut req = EsRequest::new(method, path);
        if let Some(b) = body {
            req.headers.push(("Content-Type".into(), "application/json".into()));
            req.body = b.into();
        }
        let resp = self.call(c, req).await;
        (resp.status, serde_json::from_slice(&resp.body).unwrap_or(Value::Null))
    }

    /// Lists the remote's open indices with their documents, size and aliases, whether an
    /// index of the same name exists on cluster `id`, and which of its aliases do (those aren't
    /// copied). Within [`DEADLINE`].
    pub async fn migrate_inspect(&self, id: &str, s: &Source) -> Result<Inspected> {
        let c = self.get(id).await?;
        let _slot = self.control_slot(&c)?;
        tokio::time::timeout(DEADLINE, Box::pin(self.inspect_within(&c, s)))
            .await
            .map_err(|_| ControlError::Invalid(format!("the remote cluster didn't answer within {}s", DEADLINE.as_secs())))?
    }

    async fn inspect_within(&self, c: &SearchCluster, s: &Source) -> Result<Inspected> {
        let host = normalize_host(&s.host)?;
        let client = self.control_client(s, remote::CONTROL_ANSWER_LIMIT).await?;
        // The listing may be long (many indices): read with a larger cap, row by row, no tree.
        let lister = self.control_client(s, remote::CONTROL_LISTING_LIMIT).await?;
        let err = |e| remote_error(&host, e);
        let mut retries = 0;
        let info = client.info(&mut retries).await.map_err(err)?;
        let (cat, _rows_held) = lister.get_rows(&format!("/_cat/indices?format=json&bytes=b&expand_wildcards=open&h={}", LISTING_COLUMNS.join(",")), LISTING_COLUMNS, MAX_LISTED).await.map_err(err)?;
        let aliases = client.get("/_alias").await.ok();
        let streams = client.get("/_data_stream").await.ok();
        let existing = self.roll_names(c).await?;
        // Everything the answer holds is charged as it's built, and stays charged while it's sent
        // ([`Inspected::into_stream`]).
        let mut held = search_es::breaker::Reservation::new(self.node.reindex().control_budget.clone());
        let rows = listing(&cat.rows, aliases.as_ref().map(|a| &a.value), &existing, &mut held).map_err(err)?;
        let mut names = Vec::new();
        let all_streams = streams.as_ref().and_then(|v| v.value.get("data_streams")).and_then(EsValue::as_array).map(|a| a.as_slice()).unwrap_or_default();
        for d in all_streams.iter().take(MAX_STREAMS_LISTED) {
            if let Some(n) = d.get("name").and_then(EsValue::as_str) {
                held.add(STRING_BYTES + n.len() as u64, "migrate_listing").map_err(err)?;
                names.push(n.to_string());
            }
        }
        let answer = json!({
            "remote": {
                "host": host,
                "version": info.number,
                "distribution": info.distribution.unwrap_or_else(|| "elasticsearch".into()),
                "cluster_name": info.cluster_name,
            },
            "indices": rows,
            // Indices past the first [`MAX_LISTED`] the listing didn't keep.
            "more_indices": cat.total.saturating_sub(cat.rows.len()),
            // Data streams aren't migrated yet; named so nobody wonders where they went.
            "data_streams": names,
            "more_data_streams": all_streams.len().saturating_sub(MAX_STREAMS_LISTED),
        });
        // Serialized inside the charged scope, the text charged too.
        let body = serde_json::to_vec(&answer).map_err(|e| ControlError::Internal(e.into()))?;
        drop(answer);
        held.add(body.len() as u64, "migrate_listing").map_err(err)?;
        Ok(Inspected { body: body.into(), held })
    }

    /// Copies each picked index's definition into cluster `id` and starts its reindex from
    /// remote while task slots are free; the rest are `queued`. One result per index, in order
    /// (a failure in one doesn't stop the others).
    pub async fn migrate(&self, id: &str, req: &MigrateRequest) -> Result<Value> {
        let c = self.get(id).await?;
        if req.indices.is_empty() {
            return Err(ControlError::Invalid("pick at least one index".into()));
        }
        if req.indices.len() > MAX_INDICES {
            return Err(ControlError::Invalid(format!("at most {MAX_INDICES} indices at a time")));
        }
        if let Some(rps) = req.requests_per_second
            && !(rps > 0.0 && rps.is_finite())
        {
            return Err(ControlError::Invalid("requests_per_second must be a positive number".into()));
        }
        if req.size.is_some_and(|n| n == 0 || n > 10_000) {
            return Err(ControlError::Invalid("size must be between 1 and 10000".into()));
        }
        let remote = remote_json(&req.source)?;
        let host = remote["host"].as_str().unwrap_or_default().to_string();
        let _slot = self.control_slot(&c)?;
        let deadline = tokio::time::Instant::now() + DEADLINE;
        let client = self.control_client(&req.source, DEFINITION_LIMIT).await?;
        // The remote answers before its host is whitelisted.
        let mut retries = 0;
        tokio::time::timeout_at(deadline, client.info(&mut retries))
            .await
            .map_err(|_| ControlError::Invalid(format!("the remote cluster didn't answer within {}s", DEADLINE.as_secs())))?
            .map_err(|e| remote_error(&host, e))?;
        let c = self.whitelist_host(&c.id, &remote_of(&req.source)?).await?;
        let mut names = self.roll_names(&c).await?;
        let mut out = Vec::new();
        for p in &req.indices {
            let dest = p.dest.clone().filter(|d| !d.trim().is_empty()).unwrap_or_else(|| p.index.clone());
            let mut row = json!({ "index": p.index, "dest": dest });
            if tokio::time::Instant::now() >= deadline {
                row["queued"] = json!(true);
                row["reason"] = json!(format!("the call ran past {}s before this index", DEADLINE.as_secs()));
                out.push(row);
                continue;
            }
            let mut progress = Progress::default();
            let one = Box::pin(self.migrate_one(&c, &client, &host, &remote, p, &dest, req, &mut names, &mut progress));
            let r = tokio::time::timeout_at(deadline, one).await;
            match r {
                Ok(Ok(done)) => {
                    if let Value::Object(m) = done {
                        row.as_object_mut().expect("an object").extend(m);
                    }
                }
                Ok(Err(e)) => row["error"] = json!(e),
                Err(_) if progress.removing => {
                    row["error"] = json!(format!("not started (every task slot was taken), and the call ran past {}s while the empty index [{dest}] was being removed: it may remain; delete it, then copy it again", DEADLINE.as_secs()));
                }
                Err(_) if progress.created && progress.task.is_none() => {
                    // The deadline came while the copy was being started: look its task up.
                    match tokio::time::timeout(std::time::Duration::from_secs(5), self.running_copy(&c, &dest)).await {
                        Ok(Some(task)) => progress.task = Some(task),
                        _ => {
                            row["error"] = json!(format!(
                                "the call ran past {}s while the copy was being started: [{dest}] was created, and its copy may be running (see GET _tasks?actions=*reindex&detailed=true)",
                                DEADLINE.as_secs()
                            ))
                        }
                    }
                }
                Err(_) => row["error"] = json!(format!("the call ran past {}s while this index was being set up", DEADLINE.as_secs())),
            }
            // What actually happened, whatever cut it short.
            if progress.created {
                row["created"] = json!(true);
            }
            if let Some(t) = progress.task {
                row["task"] = json!(t);
            }
            out.push(row);
        }
        Ok(json!({ "indices": out }))
    }

    /// Adds `r`'s `host:port` to cluster `id`'s reindex whitelist (exactly that host, as the
    /// tasks the migration starts need it), read and written under the settings lock.
    async fn whitelist_host(&self, id: &str, r: &Remote) -> Result<SearchCluster> {
        let pattern = format!("{}:{}", r.host.to_ascii_lowercase().trim_end_matches('.'), r.port);
        self.update_with(id, crate::store::Caps::NONE, |c| {
            if c.reindex_whitelist.contains(&pattern) {
                return Ok(());
            }
            if c.reindex_whitelist.len() >= crate::control::MAX_WHITELIST {
                return Err(ControlError::Conflict(format!(
                    "cluster {}'s reindex_whitelist is full ({} entries): remove one with PATCH /v1/search/clusters/{{id}}",
                    c.name,
                    crate::control::MAX_WHITELIST
                )));
            }
            c.reindex_whitelist.push(pattern);
            Ok(())
        })
        .await
    }

    /// A running reindex task into `dest` on cluster `c`, if there is one (a copy an earlier
    /// call started).
    async fn running_copy(&self, c: &SearchCluster, dest: &str) -> Option<String> {
        let (status, v) = self.call_json(c, "GET", "/_tasks?actions=*reindex&detailed=true", None).await;
        if status != 200 {
            return None;
        }
        let suffix = format!(" to [{dest}]");
        for (node, n) in v["nodes"].as_object().into_iter().flatten() {
            for (id, t) in n["tasks"].as_object().into_iter().flatten() {
                if t["description"].as_str().is_some_and(|d| d.starts_with("reindex from [") && d.ends_with(&suffix)) {
                    let _ = node;
                    return Some(id.clone());
                }
            }
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    async fn migrate_one(
        &self,
        c: &SearchCluster,
        client: &Client,
        host: &str,
        remote: &Value,
        p: &Pick,
        dest: &str,
        req: &MigrateRequest,
        names: &mut std::collections::HashSet<String>,
        progress: &mut Progress,
    ) -> std::result::Result<Value, String> {
        if p.index.is_empty() || p.index.contains(['*', ',', '/']) || p.index.starts_with('_') {
            return Err(format!("[{}] isn't one index", p.index));
        }
        let (status, _) = self.call_json(c, "HEAD", &format!("/{}", urlencoding::encode(dest)), None).await;
        if status == 200 {
            // A copy an earlier call started (it timed out answering, say) is this one.
            if let Some(task) = self.running_copy(c, dest).await {
                progress.created = true;
                progress.task = Some(task);
                return Ok(json!({ "note": "already copying into it" }));
            }
            return Err(format!("index [{dest}] already exists on roll: give the copy another name"));
        }
        // Created only while a task slot is free for the cluster, in turn with every other
        // cluster on the node: otherwise nothing is made, and the index waits (keeping the
        // cluster's place in line while the dashboard asks again).
        let owner = search_es::slots::owner_of(c.owner.as_deref(), &c.name);
        let free = || self.node.get_loaded(&c.name).is_some() && self.node.reindex().has_room(&c.name, &owner);
        if !free() {
            return Ok(json!({ "queued": true, "reason": format!("the node's copy slots are taken (a cluster copies [{}] indices at a time, in turn with the node's other clusters)", self.node.reindex().max_per_cluster()) }));
        }
        let err = |e| match remote_error(host, e) {
            ControlError::Invalid(m) | ControlError::Busy(m) => m,
            e => e.to_string(),
        };
        let enc = remote::encode_index(&p.index);
        let mapping = client.get(&format!("/{enc}/_mapping")).await.map_err(err)?;
        let settings_def = client.get(&format!("/{enc}/_settings?flat_settings=true")).await.map_err(err)?;
        let alias_def = client.get(&format!("/{enc}/_alias")).await.map_err(err)?;
        // Borrowed from the charged trees, which stay held (and charged) until the index is
        // created: nothing of them is copied but the request body.
        fn only<'a>(v: &'a EsValue, index: &str) -> std::result::Result<&'a EsValue, String> {
            match v.as_object() {
                Some(m) if m.len() == 1 => Ok(m.values().next().expect("one")),
                _ => Err(format!("[{index}] names more than one index on the remote")),
            }
        }
        let one = |v| only(v, &p.index);
        let empty = EsValue::Obj(EsMap::new());
        let mappings = one(&mapping.value)?.get("mappings").unwrap_or(&empty);
        if mappings.as_object().is_some_and(|m| m.len() == 1 && m.values().all(|t| t.get("properties").is_some()) && !m.contains_key("properties") && !m.contains_key("dynamic")) {
            return Err("the index has a mapping type (Elasticsearch 6 or older): reindex it on the remote into a typeless index first".into());
        }
        let no_settings = EsMap::new();
        let settings = one(&settings_def.value)?.get("settings").and_then(EsValue::as_object).unwrap_or(&no_settings);
        let mut dropped = dropped_settings(settings);
        let mut removed: std::collections::HashSet<String> = dropped.iter().filter_map(|d| d["setting"].as_str().map(str::to_string)).collect();
        // An alias roll already has would take the copy in with whatever it points at (mixed
        // searches, writes refused or moved): never joined.
        let no_aliases = EsMap::new();
        let all_aliases = one(&alias_def.value)?.get("aliases").and_then(EsValue::as_object).unwrap_or(&no_aliases);
        let mut aliases: Vec<&String> = Vec::new();
        for (name, a) in all_aliases {
            if names.contains(name) {
                dropped.push(json!({ "setting": format!("aliases.{name}"), "value": small(a), "reason": "alias exists on roll: the copy isn't added to it" }));
            } else {
                aliases.push(name);
            }
        }
        // A pipeline the index names must exist on roll, or every document would fail.
        for key in ["index.default_pipeline", "index.final_pipeline"] {
            let Some(name) = settings.get(key).and_then(EsValue::as_str).filter(|_| !removed.contains(key)) else { continue };
            if name == "_none" {
                continue;
            }
            let (status, _) = self.call_json(c, "GET", &format!("/_ingest/pipeline/{}", urlencoding::encode(name)), None).await;
            if status != 200 {
                removed.insert(key.to_string());
                dropped.push(json!({ "setting": key, "value": name, "reason": format!("pipeline [{name}] doesn't exist on roll: create it, then set the setting again") }));
            }
        }
        let mut created = false;
        for _ in 0..MAX_ATTEMPTS {
            let body = create_body(settings, &removed, mappings, all_aliases, &aliases);
            let (status, v) = self.call_text(c, "PUT", &format!("/{}", urlencoding::encode(dest)), Some(body)).await;
            if status == 200 {
                created = true;
                // Reported even if the call's deadline cuts the rest short.
                progress.created = true;
                break;
            }
            let reason = v.pointer("/error/reason").and_then(Value::as_str).unwrap_or("creating the index failed").to_string();
            if let Some(key) = named_setting(&reason).filter(|k| settings.contains_key(k) && !removed.contains(k)) {
                dropped.push(json!({ "setting": key, "value": settings.get(&key).map(small), "reason": format!("roll refused it: {reason}") }));
                removed.insert(key);
                continue;
            }
            if !aliases.is_empty() && reason.contains("alias") {
                dropped.push(json!({ "setting": "aliases", "value": aliases, "reason": format!("roll refused them: {reason}") }));
                aliases.clear();
                continue;
            }
            return Err(format!("creating index [{dest}] failed: {reason}"));
        }
        if !created {
            return Err(format!("creating index [{dest}] failed: too many settings refused"));
        }
        let aliases: Vec<String> = aliases.into_iter().cloned().collect();
        drop((mapping, settings_def, alias_def));
        progress.created = true;
        // The next indices of this request see the copy and its aliases as roll's own.
        names.insert(dest.to_string());
        names.extend(aliases.iter().cloned());
        let mut source = json!({ "remote": remote, "index": p.index });
        if let Some(n) = req.size {
            source["size"] = json!(n);
        }
        let body = json!({ "source": source, "dest": { "index": dest } });
        // `refresh=true`: the copy is searchable (and counted on the Indices tab) as soon as its
        // task ends.
        let mut path = "/_reindex?wait_for_completion=false&refresh=true".to_string();
        if let Some(rps) = req.requests_per_second {
            path.push_str(&format!("&requests_per_second={rps}"));
        }
        let (status, v) = self.call_json(c, "POST", &path, Some(&body)).await;
        if status == 429 {
            // Another copy took the last slot meanwhile: the empty copy goes, the index waits.
            progress.removing = true;
            let (deleted, _) = self.call_json(c, "DELETE", &format!("/{}", urlencoding::encode(dest)), None).await;
            if deleted != 200 {
                return Err(format!("[{dest}] was created, but not started (every task slot was taken) and couldn't be removed ({deleted}): delete it, then copy it again"));
            }
            progress.created = false;
            names.remove(dest);
            return Ok(json!({ "queued": true, "reason": "every task slot was taken meanwhile" }));
        }
        let Some(task) = v["task"].as_str().filter(|_| status == 200) else {
            let reason = v.pointer("/error/reason").and_then(Value::as_str).unwrap_or("starting the reindex failed");
            return Err(format!("index [{dest}] was created, but its reindex didn't start: {reason}"));
        };
        progress.task = Some(task.to_string());
        Ok(json!({ "dropped": dropped, "aliases": aliases }))
    }
}

/// The setting an index-creation error names (`unknown setting [index.x] …`, `Failed to parse
/// value [v] for setting [index.x] …`).
fn named_setting(reason: &str) -> Option<String> {
    let mut rest = reason;
    while let Some(at) = rest.find('[') {
        let tail = &rest[at + 1..];
        let end = tail.find(']')?;
        let name = &tail[..end];
        if name.starts_with("index.") {
            return Some(name.to_string());
        }
        rest = &tail[end..];
    }
    None
}

/// Why a setting of the remote's index isn't copied, if it isn't.
fn drop_reason(key: &str) -> Option<&'static str> {
    const MADE_BY_THE_REMOTE: &[&str] = &[
        "index.uuid",
        "index.creation_date",
        "index.creation_date_string",
        "index.provided_name",
        "index.history.uuid",
        "index.verified_before_close",
        "index.version.",
        "index.resize.",
        "index.shrink.",
        "index.downsample.",
        "index.rollup.",
        "index.frozen",
        "index.search.throttled",
    ];
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| key == *p || (p.ends_with('.') && key.starts_with(p)));
    if starts(MADE_BY_THE_REMOTE) {
        return Some("set by the cluster that made the index");
    }
    if starts(&["index.routing.allocation.", "index.routing.rebalance."]) {
        return Some("roll places shards itself");
    }
    if starts(&["index.blocks."]) {
        return Some("a block would stop the copy: set it again once the copy is done");
    }
    if starts(&["index.lifecycle.", "index.plugins.index_state_management.", "index.opendistro.index_state_management."]) {
        return Some("lifecycle policies aren't copied: attach one on roll");
    }
    if starts(&["index.store."]) {
        return Some("roll keeps the index in its own storage");
    }
    None
}

/// The remote's (flat) settings roll doesn't copy, with why.
fn dropped_settings(settings: &EsMap) -> Vec<Value> {
    settings.iter().filter_map(|(k, v)| drop_reason(k).map(|reason| json!({ "setting": k, "value": small(v), "reason": reason }))).collect()
}

/// The index-creation body, rendered from the remote's trees as they are: its settings but
/// `removed`, its mappings, the aliases of `all` named in `keep`.
fn create_body(settings: &EsMap, removed: &std::collections::HashSet<String>, mappings: &EsValue, all: &EsMap, keep: &[&String]) -> String {
    let key = |k: &str| serde_json::to_string(k).expect("a string renders");
    let mut b = String::from(r#"{"settings":{"#);
    let mut first = true;
    for (k, v) in settings.iter().filter(|(k, _)| !removed.contains(k.as_str())) {
        if !std::mem::take(&mut first) {
            b.push(',');
        }
        b.push_str(&key(k));
        b.push(':');
        b.push_str(&v.to_json());
    }
    b.push_str(r#"},"mappings":"#);
    b.push_str(&mappings.to_json());
    b.push_str(r#","aliases":{"#);
    let mut first = true;
    for (k, v) in all.iter().filter(|(k, _)| keep.contains(k)) {
        if !std::mem::take(&mut first) {
            b.push(',');
        }
        b.push_str(&key(k));
        b.push(':');
        b.push_str(&v.to_json());
    }
    b.push_str("}}");
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_get_a_scheme_and_a_port() {
        assert_eq!(normalize_host("my-deployment.es.us-east-1.aws.elastic.cloud").unwrap(), "https://my-deployment.es.us-east-1.aws.elastic.cloud:443");
        assert_eq!(normalize_host("https://h.example:9243/").unwrap(), "https://h.example:9243");
        assert_eq!(normalize_host("http://h.example").unwrap(), "http://h.example:80");
        assert_eq!(normalize_host("https://h.example/es/").unwrap(), "https://h.example:443/es");
        assert_eq!(normalize_host("https://H.Example.:9243").unwrap(), "https://h.example:9243");
        assert_eq!(normalize_host("https://[2001:db8::1]:9200").unwrap(), "https://[2001:db8::1]:9200");
        let e = normalize_host("https://elastic:hunter2@h.example").unwrap_err().to_string();
        assert!(!e.contains("hunter2"), "{e}");
        assert!(normalize_host("ftp://h.example").is_err());
        // Nothing that would be a pattern in the whitelist, nor a path that isn't one.
        for bad in ["https://*", "https://*.example.com", "https://h?x.example", "https://h.example/a%20b", "https://h ex.example"] {
            assert!(normalize_host(bad).is_err(), "{bad}");
        }
        // Dot segments, also encoded, are resolved by the URL parser before anything is kept.
        assert_eq!(normalize_host("https://h.example/a/../b").unwrap(), "https://h.example:443/b");
        for dots in ["https://h.example/%2e%2e", "https://h.example/a/%2E%2e/b", "https://h.example/./x"] {
            let h = normalize_host(dots).unwrap_or_default().to_ascii_lowercase();
            assert!(!h.contains("..") && !h.contains("%2e") && !h.contains("/./"), "{dots}: {h}");
        }
    }

    #[test]
    fn credentials_become_basic_or_api_key() {
        let s = Source { host: "h.example".into(), api_key: Some("abc==".into()), ..Default::default() };
        assert_eq!(remote_json(&s).unwrap()["headers"]["Authorization"], "ApiKey abc==");
        let s = Source { host: "h.example".into(), api_key: Some("k".into()), username: Some("u".into()), ..Default::default() };
        assert!(remote_json(&s).is_err());
        let s = Source { host: "h.example".into(), username: Some("u".into()), password: Some("hunter2".into()), ..Default::default() };
        assert!(!format!("{s:?}").contains("hunter2"));
    }

    #[test]
    fn settings_roll_doesnt_take_are_dropped_with_a_reason() {
        let v = search_es::json::parse(
            br#"{"index.number_of_shards":"2","index.uuid":"x","index.version.created":"8000099","index.routing.allocation.include._tier_preference":"data_content","index.blocks.write":"true","index.lifecycle.name":"logs","index.analysis.analyzer.a.type":"standard"}"#,
        )
        .unwrap()
        .unwrap();
        let settings = v.as_object().unwrap();
        let dropped = dropped_settings(settings);
        assert_eq!(dropped.len(), 5);
        let removed = dropped.iter().filter_map(|d| d["setting"].as_str().map(str::to_string)).collect();
        let body: Value = serde_json::from_str(&create_body(settings, &removed, &EsValue::Obj(EsMap::new()), &EsMap::new(), &[])).unwrap();
        let mut kept: Vec<&String> = body["settings"].as_object().unwrap().keys().collect();
        kept.sort();
        assert_eq!(kept, ["index.analysis.analyzer.a.type", "index.number_of_shards"]);
        assert_eq!(named_setting("unknown setting [index.foo.bar] please check that any required plugins are installed").as_deref(), Some("index.foo.bar"));
        assert_eq!(named_setting("Failed to parse value [x] for setting [index.refresh_interval]").as_deref(), Some("index.refresh_interval"));
        assert_eq!(named_setting("no setting here [x]"), None);
    }

    /// `inspect`'s answer never grows as the product of two counts the remote chose: a listing
    /// that repeats an index is refused, and a large alias set costs at most
    /// [`MAX_ALIASES_LISTED`] names per row, charged while held, all given back after.
    #[test]
    fn a_listing_holds_no_product_of_remote_counts() {
        let node = remote::RemoteNode::from_settings(&[("roll.reindex.remote.memory".into(), "1mb".into())]);
        let budget = node.control_budget.clone();
        let mut many = String::from(r#"{"a":{"aliases":{"#);
        for i in 0..100_000 {
            many.push_str(&format!(r#"{}"alias-{i}":{{}}"#, if i == 0 { "" } else { "," }));
        }
        many.push_str("}}}");
        let aliases = search_es::json::parse(many.as_bytes()).unwrap().unwrap();
        let row = |i: &str| vec![("index".to_string(), Some(i.to_string()))];
        let existing = std::collections::HashSet::new();

        let same: Vec<_> = (0..10_000).map(|_| row("a")).collect();
        let mut held = search_es::breaker::Reservation::new(budget.clone());
        let e = listing(&same, Some(&aliases), &existing, &mut held).expect_err("refused");
        assert_eq!(e.reason(), "the remote lists index [a] twice");
        drop(held);
        assert_eq!(budget.used(), 0);

        let mut held = search_es::breaker::Reservation::new(budget.clone());
        let rows = listing(&[row("a"), row("b")], Some(&aliases), &existing, &mut held).unwrap();
        assert_eq!((rows[0]["aliases"].as_array().map(Vec::len), rows[0]["aliases_more"].as_u64()), (Some(MAX_ALIASES_LISTED), Some(100_000 - MAX_ALIASES_LISTED as u64)));
        let during = budget.used();
        assert!(during > 0 && during < 8 * 1024, "{during}");
        drop((rows, held));
        assert_eq!(budget.used(), 0);
    }

    /// `inspect`'s answer stays charged while the client doesn't read it, and is given back when
    /// the connection closes, when the send deadline passes (the connection is then closed), or
    /// with its last chunk; one poll hands on one chunk.
    #[tokio::test(start_paused = true)]
    async fn an_unread_answer_stays_charged_until_the_connection_goes() {
        use futures::StreamExt;
        let node = remote::RemoteNode::from_settings(&[]);
        let budget = node.control_budget.clone();
        let answer = || {
            let mut held = search_es::breaker::Reservation::new(budget.clone());
            held.add(1 << 20, "migrate_listing").unwrap();
            Inspected { body: vec![b'x'; 1 << 20].into(), held }
        };

        let mut s = Box::pin(answer().into_stream());
        assert_eq!(s.next().await.unwrap().unwrap().len(), SEND_CHUNK);
        tokio::time::sleep(SEND_DEADLINE / 2).await;
        assert_eq!(budget.used(), 1 << 20);
        drop(s);
        assert_eq!(budget.used(), 0);

        let mut s = Box::pin(answer().into_stream());
        tokio::time::sleep(SEND_DEADLINE + std::time::Duration::from_secs(1)).await;
        assert_eq!(budget.used(), 0);
        assert!(s.next().await.unwrap().is_err());
        assert!(s.next().await.is_none());

        let sent = answer().into_stream().fold(0, |n, c| async move { n + c.unwrap().len() }).await;
        assert_eq!((sent, budget.used()), (1 << 20, 0));
    }

    #[test]
    fn transport_failures_read_alike() {
        let host = "https://h.example:443";
        for kind in ["connect_exception", "unknown_host_exception", "s_s_l_handshake_exception", "socket_timeout_exception"] {
            let e = remote_error(host, search_es::EsError::java(500, kind, "Connection refused"));
            assert_eq!(e.to_string(), "couldn't reach the remote at https://h.example:443");
        }
    }
}
