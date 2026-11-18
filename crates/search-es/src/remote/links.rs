//! Remote clusters that are other roll search clusters: Elasticsearch 9's API key based remote
//! cluster security between two clusters of one owner (docs/search/README.md, "Cross-cluster
//! search").
//!
//! A remote whose seeds or proxy address name a roll search cluster ([`Links::target`]: its
//! name, or its endpoint under the node's domain) is a **link**. Only a cluster of the same
//! owner may be named ([`Links::may_link`]): `PUT _cluster/settings` refuses any other name,
//! a missing one alike, and every request checks again, since names are global and a deleted
//! cluster's name can be taken. The remote's credentials (`cluster.remote.<alias>.credentials`,
//! a `cross_cluster` API key of the remote) are a secure setting roll's control plane keeps
//! sealed and never returns.
//!
//! A link's requests run on the remote cluster's owner node through roll's own in-process call
//! or signed fleet forward ([`Links::send`]), never over the public endpoint, so IP allowlists
//! don't apply to them. Each carries the key and the local user's `remote_indices` privileges
//! for the alias ([`REMOTE_ACCESS_HEADER`]); the remote authorizes with the key's access
//! intersected with them, as Elasticsearch's remote cluster does.
//!
//! Searches run with `ccs_minimize_roundtrips` semantics: each remote runs its part as one
//! search and the local cluster merges the answers ([`search`]).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::{Remote, remotes, split_expression};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Number, Value};
use crate::meta::settings::Settings;
use crate::obj;
use crate::rest::{Ctx, EsRequest, EsResponse};
use crate::search::{SearchOutcome, SearchRequest};
use crate::search_source::{Sort, SortOrder};
use crate::xcontent::filter::glob_match;

/// The header a link's request carries to the remote cluster: who asks (`cluster`) and the
/// role descriptor sets of the local user's `remote_indices` privileges for the alias
/// (`roles`, each a `{name: descriptor}` map; the remote intersects them with the
/// `cross_cluster` key's access). Only roll's own callers can set it: the node drops it from
/// every request that didn't come from roll itself.
pub const REMOTE_ACCESS_HEADER: &str = "x-roll-remote-access";

/// The most of [`REMOTE_ACCESS_HEADER`] a remote reads.
pub const MAX_REMOTE_ACCESS: usize = 64 << 10;

/// How long a request waits for a linked remote cluster by default (a sleeping one wakes for
/// it): Elasticsearch's `cluster.remote.initial_connect_timeout`.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(30);

/// What a node gives its clusters to reach linked remotes (roll search's control plane).
#[async_trait]
pub trait Links: Send + Sync {
    /// The roll search cluster `address` (a seed or proxy address) names, when it names one:
    /// a cluster name, or the cluster's endpoint under the node's domain. `None`: a transport
    /// address, which roll can't connect to.
    fn target(&self, address: &str) -> Option<String>;
    /// Whether cluster `local` may name cluster `target` as a remote: both are one person's,
    /// or both the operator's. `false` for a cluster that doesn't exist alike.
    async fn may_link(&self, local: &str, target: &str) -> bool;
    /// Whether `local` holds credentials for its remote `alias`.
    async fn has_credentials(&self, local: &str, alias: &str) -> bool;
    /// Sends `req` to `target` as `local`'s remote `alias`, authenticated with that remote's
    /// credentials, on the target's owner. An error means the remote can't be reached (no
    /// credentials, the target gone or no longer `local`'s owner's); any answer the remote
    /// gives is `Ok`.
    async fn send(&self, local: &str, alias: &str, target: &str, req: EsRequest) -> Result<EsResponse, EsError>;
}

impl std::fmt::Debug for dyn Links {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Links")
    }
}

impl Remote {
    /// The roll cluster this remote links to, when its address names one.
    pub fn link(&self, links: &dyn Links) -> Option<String> {
        if self.proxy { self.proxy_address.as_deref().and_then(|a| links.target(a)) } else { self.seeds.iter().find_map(|s| links.target(s)) }
    }
}

/// Whether remote `r` is a link of `cluster` (its address names a roll search cluster).
pub fn is_link(cluster: &crate::cluster::Cluster, r: &Remote) -> bool {
    cluster.config.links.as_deref().is_some_and(|l| r.link(l).is_some())
}

/// Whether expression `e` names a linked remote cluster (`alias:index` whose alias pattern
/// matches a link).
pub fn names_link(ctx: &Ctx, e: &str) -> bool {
    let Some(links) = ctx.cluster.config.links.as_deref() else { return false };
    let Some((pattern, _)) = split_expression(e) else { return false };
    remotes(&ctx.state()).iter().any(|r| glob_match(pattern, &r.alias) && r.link(links).is_some())
}

/// How long a request waits for a linked remote (`cluster.remote.initial_connect_timeout`).
pub fn wait(cluster: &crate::cluster::Cluster) -> Duration {
    cluster
        .config
        .node_setting("cluster.remote.initial_connect_timeout")
        .and_then(|v| crate::units::TimeValue::parse(v, "cluster.remote.initial_connect_timeout").ok())
        .map(|t| t.as_millis())
        .filter(|ms| *ms > 0)
        .map_or(DEFAULT_WAIT, |ms| Duration::from_millis(ms as u64))
}

/// `PUT _cluster/settings`: a remote's seeds and proxy address may name a roll search cluster
/// only when it is one the cluster may link to. A missing cluster and another owner's answer
/// the same, so the answer tells nothing a 404 wouldn't.
pub async fn check_settings(cluster: &crate::cluster::Cluster, updates: &[&Settings]) -> EsResult<()> {
    let Some(links) = cluster.config.links.clone() else { return Ok(()) };
    for s in updates {
        for (k, _) in s.iter() {
            let Some(rest) = k.strip_prefix("cluster.remote.") else { continue };
            let (alias, addresses) = if let Some(a) = rest.strip_suffix(".seeds") {
                (a, s.get_list(k).unwrap_or_default())
            } else if let Some(a) = rest.strip_suffix(".proxy_address") {
                (a, s.get(k).map(|v| vec![v.to_string()]).unwrap_or_default())
            } else {
                continue;
            };
            for address in addresses {
                let Some(target) = links.target(&address) else { continue };
                if target == cluster.name || !links.may_link(&cluster.name, &target).await {
                    return Err(refused(alias, &target));
                }
            }
        }
    }
    Ok(())
}

/// The answer for a remote naming a cluster this one may not link to.
pub fn refused(alias: &str, target: &str) -> EsError {
    EsError::illegal_argument(format!(
        "remote cluster [{alias}]: no search cluster [{target}] to link to (a remote cluster must be another cluster of this cluster's owner)"
    ))
}

/// The role descriptors of the request's principal's `remote_indices` privileges for remote
/// `alias`, as sets the remote intersects (an API key's own and its owner's are two sets):
/// [`REMOTE_ACCESS_HEADER`]'s `roles`.
pub fn remote_roles(ctx: &Ctx, alias: &str) -> Vec<Value> {
    use crate::xpack::security::{self, privileges::Permission};
    let p = security::principal(ctx);
    if p.is_superuser() {
        // `ReservedRolesStore`'s superuser: every remote index, restricted ones read-only.
        return vec![obj! { "superuser" => obj! { "cluster" => Value::Arr(Vec::new()), "indices" => Value::Arr(vec![
            obj! { "names" => crate::arr!["*"], "privileges" => crate::arr!["all"], "allow_restricted_indices" => false },
            obj! { "names" => crate::arr!["*"], "privileges" => crate::arr!["monitor", "read", "read_cross_cluster", "view_index_metadata"], "allow_restricted_indices" => true },
        ]) } }];
    }
    let eff = security::effective(&security::file_roles(ctx.cluster), &ctx.state(), p);
    let set = |perms: &[Permission]| -> Value {
        let mut m = Map::new();
        for (i, perm) in perms.iter().enumerate() {
            let indices: Vec<Value> = perm
                .remote_indices
                .iter()
                .filter(|e| {
                    let clusters: Vec<String> = match e.get("clusters") {
                        Some(Value::Arr(a)) => a.iter().map(Value::to_es_string).collect(),
                        Some(Value::Str(s)) => vec![s.clone()],
                        _ => Vec::new(),
                    };
                    clusters.iter().any(|c| glob_match(c, alias))
                })
                .map(|e| {
                    let mut e = e.clone();
                    if let Some(m) = e.as_object_mut() {
                        m.shift_remove("clusters");
                    }
                    e
                })
                .collect();
            m.insert(format!("r{i}"), obj! { "cluster" => Value::Arr(Vec::new()), "indices" => Value::Arr(indices) });
        }
        Value::Obj(m)
    };
    let mut sets = vec![set(&eff.own)];
    sets.extend(eff.limited_by.iter().map(|l| set(l)));
    sets.extend(eff.also.iter().map(|l| set(l)));
    sets
}

/// The request to send remote `alias` for `method uri`: as the local principal (its
/// [`remote_roles`]), with the client's `X-Opaque-Id` and trace headers.
pub fn request(ctx: &Ctx, alias: &str, method: &str, uri: &str, body: Option<Value>) -> EsRequest {
    let mut req = match body {
        Some(b) => EsRequest::json(method, uri, b.to_json().into_bytes()),
        None => EsRequest::new(method, uri),
    };
    let p = crate::xpack::security::principal(ctx);
    let who = match &p.kind {
        crate::xpack::security::AuthKind::ApiKey(a) => format!("API key id [{}] of user [{}]", a.id, p.username),
        _ => {
            let mut roles = p.roles.clone();
            roles.sort();
            format!("user [{}] with assigned roles [{}]", p.username, roles.join(","))
        }
    };
    let access = obj! { "cluster" => ctx.cluster.name.clone(), "who" => who, "roles" => Value::Arr(remote_roles(ctx, alias)) };
    req.headers.push((REMOTE_ACCESS_HEADER.into(), access.to_json()));
    for h in ["x-opaque-id", "traceparent"] {
        if let Some(v) = ctx.request.header(h) {
            req.headers.push((h.into(), v.to_string()));
        }
    }
    req
}

/// Percent-encodes index expressions for a path segment (`,` stays the list separator).
pub fn path_segment(exprs: &[String]) -> String {
    let mut out = String::new();
    for (i, e) in exprs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        for b in e.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' | b'~' => out.push(b as char),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
    }
    if out.is_empty() { "_all".into() } else { out }
}

fn encode_query(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' | b'~' | b',' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The client's query string without `drop`'s parameters (`?…`, or empty).
pub fn query_string(ctx: &Ctx, drop: &[&str], add: &[(&str, &str)]) -> String {
    let mut parts: Vec<String> = ctx.query_pairs().iter().filter(|(k, _)| !drop.contains(&k.as_str())).map(|(k, v)| format!("{}={}", encode_query(k), encode_query(v))).collect();
    parts.extend(add.iter().map(|(k, v)| format!("{k}={}", encode_query(v))));
    if parts.is_empty() { String::new() } else { format!("?{}", parts.join("&")) }
}

/// An error a remote answered, as it rendered it.
pub fn error_of(status: u16, body: &Value) -> EsError {
    fn parse(status: u16, e: &Value) -> EsError {
        let kind = e.get("type").map(Value::to_es_string).unwrap_or_else(|| "exception".into());
        let mut out = match e.get("reason") {
            Some(Value::Null) | None => EsError::new(status, kind, "").with_null_reason(),
            Some(r) => EsError::new(status, kind, r.to_es_string()),
        };
        if let Some(m) = e.as_object() {
            for (k, v) in m {
                match k.as_str() {
                    "type" | "reason" | "root_cause" | "caused_by" | "suppressed" | "stack_trace" | "header" => {}
                    _ => out = out.with_meta(k.clone(), v.clone()),
                }
            }
        }
        if let Some(c) = e.get("caused_by") {
            out = out.caused_by(parse(status, c));
        }
        if let Some(Value::Arr(roots)) = e.get("root_cause") {
            out = out.with_root_causes(roots.iter().map(|r| parse(status, r)).collect());
        }
        out
    }
    match body.get("error") {
        Some(e @ Value::Obj(_)) => parse(status, e),
        Some(Value::Str(s)) => EsError::new(status, "exception", s.clone()),
        _ => EsError::new(status, "exception", format!("remote cluster answered {status}")),
    }
}

/// The remote leg of one request: the remote's answer, or why it couldn't be reached.
pub enum Answer {
    Ok(Value),
    /// The remote answered with an error.
    Failed(EsError),
    /// The remote couldn't be reached (gone, no credentials, timed out).
    Unavailable(EsError),
}

/// Sends `req` to linked remote `r` (`target`) and waits at most `wait` for its answer: run on
/// a task of its own, so legs to several remotes and the local part run at once.
pub fn spawn(links: Arc<dyn Links>, local: String, r: &Remote, target: String, req: EsRequest, wait: Duration) -> tokio::task::JoinHandle<Answer> {
    let alias = r.alias.clone();
    tokio::spawn(async move {
        match tokio::time::timeout(wait, links.send(&local, &alias, &target, req)).await {
            Err(_) => Answer::Unavailable(EsError::new(500, "connect_transport_exception", format!("Unable to connect to [{alias}]: no answer within [{}ms]", wait.as_millis()))),
            Ok(Err(e)) => Answer::Unavailable(e),
            Ok(Ok(resp)) => {
                let v = crate::json::parse(&resp.body).ok().flatten().unwrap_or(Value::Null);
                if resp.status < 300 { Answer::Ok(v) } else { Answer::Failed(error_of(resp.status, &v)) }
            }
        }
    })
}

/// [`spawn`] for the request's principal: refused here, as Elasticsearch's querying cluster
/// refuses it, when none of its `remote_indices` privileges names the remote.
pub fn leg(ctx: &Ctx, r: &Remote, target: String, req: EsRequest) -> tokio::task::JoinHandle<Answer> {
    let links = ctx.cluster.config.links.clone().expect("a link has links");
    let p = crate::xpack::security::principal(ctx);
    if !p.is_superuser() && remote_roles(ctx, &r.alias).iter().any(|set| set.as_object().is_none_or(|m| m.values().all(|d| d.get("indices").and_then(Value::as_array).is_none_or(|a| a.is_empty())))) {
        let e = EsError::security(
            403,
            format!(
                "action [{}] towards remote cluster [{}] is unauthorized for {} because no remote indices privileges apply for the target cluster",
                crate::xpack::security::privileges::action_of(ctx.api.name()).1,
                r.alias,
                crate::xpack::security::who(p)
            ),
        );
        return tokio::spawn(async move { Answer::Failed(e) });
    }
    spawn(links, ctx.cluster.name.clone(), r, target, req, wait(ctx.cluster))
}

/// Awaits a leg; a task that panicked counts as an unreachable remote.
pub async fn join(alias: &str, h: tokio::task::JoinHandle<Answer>) -> Answer {
    h.await.unwrap_or_else(|_| Answer::Unavailable(EsError::new(500, "connect_transport_exception", format!("Unable to connect to [{alias}]"))))
}

/// How an expression list splits over the cluster's linked remotes.
pub struct Split {
    /// Expressions for the local cluster (also those for remotes that aren't links: loopbacks
    /// and unreachable ones, which the local search plans as before).
    pub local: Vec<String>,
    /// Each linked remote the expressions name: the remote, the roll cluster it links to, and
    /// its expressions.
    pub legs: Vec<(Remote, String, Vec<String>)>,
    /// A cluster wildcard was used.
    pub wildcards: bool,
}

/// Splits `expressions` over the local cluster and the linked remotes they name; `None` when
/// they name none (or the cluster has no links).
pub fn split(ctx: &Ctx, expressions: &[String]) -> EsResult<Option<Split>> {
    let Some(links) = ctx.cluster.config.links.as_deref() else { return Ok(None) };
    if ctx.cluster.dialect != crate::cluster::Dialect::Elasticsearch || !expressions.iter().any(|e| split_expression(e).is_some()) {
        return Ok(None);
    }
    let configured = remotes(&ctx.state());
    let linked: Vec<(&Remote, String)> = configured.iter().filter_map(|r| r.link(links).map(|t| (r, t))).collect();
    if linked.is_empty() {
        return Ok(None);
    }
    let mut local = Vec::new();
    let mut legs: BTreeMap<String, (Remote, String, Vec<String>)> = BTreeMap::new();
    let mut wildcards = false;
    for e in expressions {
        let Some((pattern, index)) = split_expression(e) else {
            local.push(e.clone());
            continue;
        };
        wildcards |= pattern.contains('*');
        let mut any_other = false;
        let mut any = false;
        for r in configured.iter().filter(|r| glob_match(pattern, &r.alias)) {
            any = true;
            match linked.iter().find(|(l, _)| l.alias == r.alias) {
                Some((l, t)) => legs.entry(l.alias.clone()).or_insert_with(|| ((*l).clone(), t.clone(), Vec::new())).2.push(index.to_string()),
                None => any_other = true,
            }
        }
        // Loopbacks, unreachable remotes and unknown aliases: planned by the local search.
        if any_other || !any {
            local.push(e.clone());
        }
    }
    if legs.is_empty() {
        return Ok(None);
    }
    Ok(Some(Split { local, legs: legs.into_values().collect(), wildcards }))
}

/// Prefixes the index names in a remote's answer with its alias (`hits`, failures).
fn prefix_index(v: &mut Value, alias: &str) {
    if let Some(Value::Str(i)) = v.as_object_mut().and_then(|m| m.get_mut("_index"))
        && !i.contains(':')
    {
        *i = format!("{alias}:{i}");
    }
}

fn prefix_failure(v: &mut Value, alias: &str) {
    if let Some(Value::Str(i)) = v.as_object_mut().and_then(|m| m.get_mut("index"))
        && !i.contains(':')
    {
        *i = format!("{alias}:{i}");
    }
}

/// One cluster's figures for `_clusters.details`.
struct ClusterPart {
    name: String,
    indices: String,
    status: &'static str,
    took: i64,
    timed_out: bool,
    shards: Option<(i64, i64, i64, i64)>,
    failures: Vec<Value>,
}

impl ClusterPart {
    fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("status".into(), self.status.into());
        m.insert("indices".into(), self.indices.clone().into());
        if self.status != "skipped" {
            m.insert("took".into(), Value::from(self.took));
        }
        m.insert("timed_out".into(), Value::Bool(self.timed_out));
        if let Some((total, successful, skipped, failed)) = self.shards {
            m.insert("_shards".into(), obj! { "total" => total, "successful" => successful, "skipped" => skipped, "failed" => failed });
        }
        if !self.failures.is_empty() {
            m.insert("failures".into(), Value::Arr(self.failures.clone()));
        }
        Value::Obj(m)
    }
}

/// A shard failure of a whole cluster (`ShardSearchFailure` with no shard).
fn cluster_failure(e: &EsError) -> Value {
    obj! { "shard" => -1, "index" => Value::Null, "reason" => e.to_value(false) }
}

/// A search over linked remote clusters (and the local cluster, when the expressions name
/// local indices too): `None` when the request names no linked remote. Each remote runs its
/// part as one search (`ccs_minimize_roundtrips`) for `from + size` hits, the local part runs
/// here meanwhile, and the answers merge as `SearchResponseMerger` merges them: hits by score
/// or sort values, totals summed, shards and failures added up, aggregations reduced, and a
/// `_clusters` section. A remote that can't be reached or fails is skipped when its
/// `skip_unavailable` says so (the default) and fails the search otherwise.
pub async fn search(ctx: &Ctx<'_>, req: &SearchRequest, body: Option<&Value>) -> EsResult<Option<SearchOutcome>> {
    let Some(split) = split(ctx, &req.indices)? else { return Ok(None) };
    if req.scroll.is_some() {
        return Err(EsError::illegal_argument("roll can't scroll across linked remote clusters yet: search them without [scroll], or use [search_after]"));
    }
    let started = Instant::now();
    let from = req.source.from.unwrap_or(0).max(0) as i64;
    let size = req.source.size.unwrap_or(10).max(0) as i64;
    let window = from + size;
    let participants = split.legs.len() + usize::from(!split.local.is_empty());
    let has_aggs = req.source.aggs.as_ref().is_some_and(|a| !a.is_empty());
    // Several clusters' aggregations merge by type: everyone renders typed keys.
    let typed = has_aggs && participants > 1;
    let mut remote_body = match body {
        Some(Value::Obj(m)) => m.clone(),
        _ => Map::new(),
    };
    remote_body.insert("from".into(), Value::from(0i64));
    remote_body.insert("size".into(), Value::from(window));
    const DROP: &[&str] = &["from", "size", "typed_keys", "rest_total_hits_as_int", "ccs_minimize_roundtrips", "filter_path", "pretty", "human", "error_trace", "format", "scroll", "index"];
    let qs = query_string(ctx, DROP, if typed { &[("typed_keys", "true")] } else { &[] });
    let mut legs = Vec::new();
    for (r, target, exprs) in &split.legs {
        let uri = format!("/{}/_search{qs}", path_segment(exprs));
        let rq = request(ctx, &r.alias, "POST", &uri, Some(Value::Obj(remote_body.clone())));
        legs.push((r.clone(), exprs.join(","), leg(ctx, r, target.clone(), rq)));
    }
    // The local part, while the remotes run theirs.
    let local = if split.local.is_empty() {
        None
    } else {
        let mut lreq = req.clone();
        lreq.indices = split.local.clone();
        lreq.source.from = Some(0);
        lreq.source.size = Some(window as i32);
        lreq.typed_keys = typed || req.typed_keys;
        Some(crate::search::run(ctx, lreq)?)
    };
    let mut parts: Vec<ClusterPart> = Vec::new();
    let mut out = match local {
        Some(mut o) => {
            match o.clusters.take() {
                // Loopback or unreachable non-link remotes in the local part: their details.
                Some(c) => {
                    for (name, d) in c.get("details").and_then(Value::as_object).cloned().unwrap_or_default() {
                        let s = |k: &str| d.pointer(&["_shards", k]).and_then(Value::as_i64).unwrap_or(0);
                        parts.push(ClusterPart {
                            name,
                            indices: d.get("indices").map(Value::to_es_string).unwrap_or_default(),
                            status: match d.get("status").and_then(Value::as_str) {
                                Some("skipped") => "skipped",
                                Some("partial") => "partial",
                                Some("failed") => "failed",
                                _ => "successful",
                            },
                            took: d.get("took").and_then(Value::as_i64).unwrap_or(o.took_ms as i64),
                            timed_out: d.get("timed_out").and_then(Value::as_bool).unwrap_or(false),
                            shards: d.get("_shards").map(|_| (s("total"), s("successful"), s("skipped"), s("failed"))),
                            failures: d.get("failures").and_then(Value::as_array).cloned().unwrap_or_default(),
                        });
                    }
                }
                None => {
                    let failed = o.failures.len() as i64;
                    let total = o.shards_total as i64;
                    parts.push(ClusterPart {
                        name: "(local)".into(),
                        indices: split.local.join(","),
                        status: if failed > 0 { "partial" } else { "successful" },
                        took: o.took_ms as i64,
                        timed_out: o.timed_out,
                        shards: Some((total, total - failed, o.shards_skipped as i64, failed)),
                        failures: Vec::new(),
                    });
                }
            }
            o
        }
        None => crate::opensearch::can_match::all_skipped(0),
    };
    if split.local.is_empty() {
        out.total = Some((0, search_index::collector::top_docs::TotalRelation::EqualTo));
        out.max_score = None;
    }
    let mut aggs: Vec<Value> = Vec::new();
    if let Some(a) = out.aggs.take() {
        aggs.push(a);
    }
    let mut suggests: Vec<Value> = out.suggest.take().into_iter().collect();
    let mut hits: Vec<(usize, Value)> = std::mem::take(&mut out.hits).into_iter().map(|h| (0, h)).collect();
    let index_sorts: Vec<usize> = req.source.sort.iter().enumerate().filter(|(_, s)| matches!(s, Sort::Field { field, .. } if field == "_index")).map(|(i, _)| i).collect();
    for (i, (r, indices, h)) in legs.into_iter().enumerate() {
        let alias = r.alias.clone();
        let answer = join(&alias, h).await;
        let (v, e) = match answer {
            Answer::Ok(v) => (v, None),
            Answer::Failed(e) | Answer::Unavailable(e) => (Value::Null, Some(e)),
        };
        if let Some(e) = e {
            if !r.skip_unavailable {
                return Err(e);
            }
            parts.push(ClusterPart { name: alias, indices, status: "skipped", took: 0, timed_out: false, shards: None, failures: vec![cluster_failure(&e)] });
            continue;
        }
        let num = |p: &[&str]| v.pointer(p).and_then(Value::as_i64).unwrap_or(0);
        let (total, successful, skipped, failed) = (num(&["_shards", "total"]), num(&["_shards", "successful"]), num(&["_shards", "skipped"]), num(&["_shards", "failed"]));
        out.shards_total += total as usize;
        out.shards_skipped += skipped as usize;
        let mut failures: Vec<Value> = v.pointer(&["_shards", "failures"]).and_then(Value::as_array).cloned().unwrap_or_default();
        for f in &mut failures {
            prefix_failure(f, &alias);
        }
        // Grouped failures stand for every failed shard: as many entries as failed shards.
        let mut counted = failures.clone();
        while (counted.len() as i64) < failed {
            match failures.last() {
                Some(f) => counted.push(f.clone()),
                None => counted.push(obj! { "shard" => -1, "index" => Value::Null, "reason" => obj! { "type" => "exception", "reason" => "shard failure" } }),
            }
        }
        out.failures.extend(counted);
        let timed_out = v.get("timed_out").and_then(Value::as_bool).unwrap_or(false);
        out.timed_out |= timed_out;
        if let Some(t) = v.get("terminated_early").and_then(Value::as_bool) {
            out.terminated_early = Some(out.terminated_early.unwrap_or(false) || t);
        }
        if let Some(t) = v.pointer(&["hits", "total"]) {
            let (value, gte) = match t {
                Value::Obj(_) => (t.get("value").and_then(Value::as_i64).unwrap_or(0), t.get("relation").and_then(Value::as_str) == Some("gte")),
                _ => (t.as_i64().unwrap_or(0), false),
            };
            use search_index::collector::top_docs::TotalRelation;
            let (cur, rel) = out.total.unwrap_or((0, TotalRelation::EqualTo));
            out.total = Some((cur + value.max(0) as u64, if gte || rel != TotalRelation::EqualTo { TotalRelation::GreaterThanOrEqualTo } else { TotalRelation::EqualTo }));
        }
        if let Some(s) = v.pointer(&["hits", "max_score"]).and_then(Value::as_f64) {
            out.max_score = Some(out.max_score.map_or(s as f32, |m| if m.is_nan() { s as f32 } else { m.max(s as f32) }));
        }
        for mut h in v.pointer(&["hits", "hits"]).and_then(Value::as_array).cloned().unwrap_or_default() {
            prefix_index(&mut h, &alias);
            // A sort on `_index` sorts by the name the hit has here.
            for p in index_sorts.iter() {
                if let Some(Value::Str(s)) = h.as_object_mut().and_then(|m| m.get_mut("sort")).and_then(|s| match s {
                    Value::Arr(a) => a.get_mut(*p),
                    _ => None,
                }) && !s.contains(':')
                {
                    *s = format!("{alias}:{s}");
                }
            }
            hits.push((i + 1, h));
        }
        if let Some(a) = v.get("aggregations") {
            aggs.push(a.clone());
        }
        if let Some(s) = v.get("suggest") {
            suggests.push(s.clone());
        }
        if let Some(Value::Arr(shards)) = v.pointer(&["profile", "shards"]) {
            let mut p = out.profile.take().unwrap_or_else(|| obj! { "shards" => Value::Arr(Vec::new()) });
            if let Some(Value::Arr(list)) = p.as_object_mut().and_then(|m| m.get_mut("shards")) {
                for s in shards {
                    let mut s = s.clone();
                    if let Some(Value::Str(id)) = s.as_object_mut().and_then(|m| m.get_mut("id"))
                        && let Some(rest) = id.strip_prefix('[')
                        && let Some((node, after)) = rest.split_once("][")
                    {
                        *id = format!("[{node}][{alias}:{after}");
                    }
                    list.push(s);
                }
            }
            out.profile = Some(p);
        }
        parts.push(ClusterPart {
            name: alias,
            indices,
            status: if failed > 0 { "partial" } else { "successful" },
            took: v.get("took").and_then(Value::as_i64).unwrap_or(0),
            timed_out,
            shards: Some((total, successful, skipped, failed)),
            failures: Vec::new(),
        });
    }
    // Hits: best first, as the request sorts them; ties keep the clusters' order.
    sort_hits(&mut hits, &req.source.sort);
    let mut hits: Vec<Value> = hits.into_iter().map(|(_, h)| h).collect();
    if let Some(c) = &req.source.collapse {
        let mut seen: Vec<Value> = Vec::new();
        hits.retain(|h| {
            let key = h.pointer(&["fields", c.field.as_str()]).and_then(Value::as_array).and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
            if seen.iter().any(|s| *s == key) {
                return false;
            }
            seen.push(key);
            true
        });
    }
    out.hits = hits.into_iter().skip(from as usize).take(size as usize).collect();
    out.ranked = Vec::new();
    out.aggs = match aggs.len() {
        0 => None,
        1 if !typed => aggs.pop(),
        _ => {
            let request = req.source.aggs.clone().map(Value::Obj).unwrap_or_else(Value::object);
            let merged = super::merge::aggs(&aggs, &request)?;
            // `InternalAggregations` reduced across clusters come out of a `HashMap`.
            Some(match super::merge::untyped(merged.clone()) {
                Value::Obj(m) if !req.typed_keys => Value::Obj(crate::json::java_hash_map(m, 16)),
                _ => merged,
            })
        }
    };
    out.suggest = match suggests.len() {
        0 => None,
        1 => suggests.pop(),
        _ => Some(super::merge::suggest(&suggests, &req.source)?),
    };
    out.took_ms = started.elapsed().as_millis() as u64;
    // As Elasticsearch counts them: one per cluster that answered, plus the final one when the
    // local cluster took part or several did.
    let answered = parts.iter().filter(|p| p.status != "skipped").count();
    out.reduce_phases = Some(match answered {
        0 => 0,
        1 if split.local.is_empty() => 1,
        n => n + 1,
    });
    ctx.cluster.ccs.record_parts(parts.iter().map(|p| (p.name.as_str(), p.status == "skipped")), split.wildcards, out.took_ms, out.failures.is_empty(), ctx.query("ccs_minimize_roundtrips").is_none_or(|v| v != "false"));
    out.clusters = Some(clusters_section(&parts));
    Ok(Some(out))
}

/// `SearchResponse.Clusters` from each cluster's part.
fn clusters_section(parts: &[ClusterPart]) -> Value {
    let count = |s: &str| parts.iter().filter(|p| p.status == s).count() as i64;
    let mut details = Map::new();
    for p in parts {
        details.insert(p.name.clone(), p.to_value());
    }
    obj! {
        "total" => parts.len() as i64,
        "successful" => count("successful"),
        "skipped" => count("skipped"),
        "running" => 0,
        "partial" => count("partial"),
        "failed" => count("failed"),
        "details" => Value::Obj(details),
    }
}

/// Compares two values of a hit's `sort` array: numbers as numbers, strings by their UTF-8
/// bytes (`BytesRef`), missing (`null`) values last.
pub fn cmp_sort_value(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        (Value::Num(x), Value::Num(y)) => match (x, y) {
            (Number::Int(x), Number::Int(y)) => x.cmp(y),
            _ => x.as_f64().partial_cmp(&y.as_f64()).unwrap_or(Ordering::Equal),
        },
        (Value::Str(x), Value::Str(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => a.to_es_string().cmp(&b.to_es_string()),
    }
}

fn sort_hits(hits: &mut [(usize, Value)], sort: &[Sort]) {
    use std::cmp::Ordering;
    let by_score = sort.is_empty() || matches!(sort, [Sort::Score { order: SortOrder::Desc }]);
    if by_score {
        let score = |h: &Value| h.get("_score").and_then(Value::as_f64).unwrap_or(f64::NEG_INFINITY);
        hits.sort_by(|(ca, a), (cb, b)| score(b).partial_cmp(&score(a)).unwrap_or(Ordering::Equal).then(ca.cmp(cb)));
        return;
    }
    let desc: Vec<bool> = sort
        .iter()
        .map(|s| match s {
            Sort::Score { order } => *order == SortOrder::Desc,
            Sort::Field { field, order, .. } => order.map_or(field == "_score", |o| o == SortOrder::Desc),
            Sort::Script { order, .. } => order.is_some_and(|o| o == SortOrder::Desc),
            Sort::GeoDistance(m) => m.get("order").and_then(Value::as_str).is_some_and(|o| o.eq_ignore_ascii_case("desc")),
        })
        .collect();
    let values = |h: &Value| h.get("sort").and_then(Value::as_array).cloned().unwrap_or_default();
    hits.sort_by(|(ca, a), (cb, b)| {
        let (va, vb) = (values(a), values(b));
        for (i, d) in desc.iter().enumerate() {
            let (x, y) = (va.get(i).unwrap_or(&Value::Null), vb.get(i).unwrap_or(&Value::Null));
            // Missing values sort last in either order (`_last`).
            let o = match (x.is_null(), y.is_null()) {
                (false, false) if *d => cmp_sort_value(y, x),
                _ => cmp_sort_value(x, y),
            };
            if o != Ordering::Equal {
                return o;
            }
        }
        ca.cmp(cb)
    });
}
