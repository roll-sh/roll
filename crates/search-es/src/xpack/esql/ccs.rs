//! ES|QL across clusters: `FROM <alias>:<index>` over linked remote clusters
//! (docs/search/README.md, "Cross-cluster search", and `remote::links`).
//!
//! Each cluster runs its **part** of the query, as Elasticsearch sends each cluster the plan up
//! to its first pipeline breaker: the source, the commands that keep working row by row
//! (`WHERE`, `EVAL`, `KEEP`, `DISSECT`, `ENRICH` in `_remote` or `_any` mode, `LOOKUP JOIN`...),
//! then the breaker's own part ([`Cut`]): `STATS` reduced partially (its groups' accumulator
//! states, merged here exactly as one cluster's would be), `SORT ... | LIMIT n` and `LIMIT n`
//! applied there and again here. Anything else (`INLINE STATS`, `FORK`, `ENRICH _coordinator`)
//! runs here over every part's rows. The local part runs in process; a linked remote's runs on
//! its owner node through the link ([`super::super::super::remote::links::Links::send`]),
//! authorized there as the link's `cross_cluster` key intersected with the caller's
//! `remote_indices` privileges, exactly as `_search` across clusters is (document and field
//! level security included: the part reads through the remote's own search as that principal).
//!
//! A remote's part is the query's own text, cut ([`part_text`]): its `FROM` names that
//! cluster's indices, the commands it doesn't run are blanked, so every position it reports is
//! the query's. It runs as a `_query` with [`PART_HEADER`] (honored only on a link's own
//! request) and answers [`PART_FIELD`]: typed columns and rows, or `STATS` states, decoded here
//! as hostile input ([`decode_answer`]): held on the request breaker before parsing, one level of
//! multi-values, every size bounded by the bytes that came, sketches validated.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::remote::Remote;
use crate::remote::links::{self, Links};
use crate::rest::{Ctx, EsRequest};

use super::exec::{Col, Table};
use super::parser::{Cmd, Query, Span};
use super::value::{Acc, DataType, V};

/// The header that makes a link's `_query` one cluster's part (its JSON [`Part`]). Honored only
/// on a link's own request ([`links::is_link_call`]): a client asking gets an ordinary query.
pub const PART_HEADER: &str = "x-roll-esql-part";

/// The answer field holding a part's result.
pub const PART_FIELD: &str = "__roll_esql_part";

/// The error type a part answers when an enrich policy it was told to have is missing (its
/// reason: the names, comma separated). Only parts answer it.
const MISSING_POLICY: &str = "roll_missing_enrich_policy";

/// What one decoded element of a part's answer costs at most: its parsed JSON value and the
/// value decoded from it, whatever few bytes it took to send.
const PER_ELEMENT: u64 = (std::mem::size_of::<Value>() + std::mem::size_of::<V>() + 16) as u64;

/// The most columns a part's answer may have beyond its schema's: what its cluster's own lookup
/// indices add.
const MAX_RESOURCE_COLUMNS: usize = 10_000;

/// The cluster action a remote's `ENRICH` needs (`remote_cluster: monitor_enrich`).
pub const RESOLVE_POLICY: &str = "cluster:monitor/xpack/enrich/esql/resolve_policy";

/// The prefix of the columns a part carries its sort keys in, when an `ENRICH _remote` after
/// its `SORT ... | LIMIT` may overwrite them: the coordinating cluster sorts on these.
pub(super) const SORT_CARRY: &str = "$ccs_sort";

/// How a part ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Rows (after `LIMIT`, `SORT ... | LIMIT`, or everything up to a breaker that runs here).
    Rows,
    /// The last command is a `STATS`, answered as its groups' states.
    Stats,
}

/// What a cluster's part is told.
#[derive(Clone, Debug)]
pub struct Part {
    /// The remote's alias here (`None`: the local part): `_index` and missing indices are named
    /// `alias:index`.
    pub alias: Option<String>,
    pub mode: Mode,
    /// Every cluster's source fields as the coordinating cluster resolved them: a part has all
    /// of them, in this order and of these types (a field this cluster doesn't map reads null).
    pub schema: Vec<(String, DataType)>,
    /// Enrich policies the part's cluster must have.
    pub policies: Vec<String>,
    /// The part ends `SORT ... | LIMIT n | ENRICH _remote:...`: it carries its sort keys'
    /// values ([`SORT_CARRY`]) from before the `ENRICH`.
    pub sort_keys: bool,
    /// A rows part without a limit: the most it may answer (its rows' size as [`row_bytes`]
    /// counts it), what the coordinating cluster's request breaker has left.
    pub max_bytes: Option<u64>,
}

impl Part {
    /// The part as a remote's request carries it, in its body ([`PART_FIELD`]): a schema of
    /// thousands of fields fits there, never in a header a fleet forward caps.
    fn to_value(&self) -> Value {
        obj! {
            "alias" => self.alias.clone().map_or(Value::Null, Value::Str),
            "mode" => if self.mode == Mode::Stats { "stats" } else { "rows" },
            "schema" => Value::Arr(self.schema.iter().map(|(n, t)| crate::arr![n.clone(), t.name()]).collect()),
            "policies" => Value::Arr(self.policies.iter().cloned().map(Value::Str).collect()),
            "sort_keys" => self.sort_keys,
            "max_bytes" => self.max_bytes.map_or(Value::Null, |b| Value::from(b.min(i64::MAX as u64) as i64)),
        }
    }

    fn of_value(v: &Value) -> Option<Part> {
        let mode = match v.get("mode").and_then(Value::as_str)? {
            "stats" => Mode::Stats,
            "rows" => Mode::Rows,
            _ => return None,
        };
        let mut schema = Vec::new();
        for c in v.get("schema").and_then(Value::as_array)? {
            schema.push((pair(c, 0).and_then(Value::as_str)?.to_string(), pair(c, 1).and_then(Value::as_str).and_then(type_named)?));
        }
        let mut policies = Vec::new();
        for p in v.get("policies").and_then(Value::as_array)? {
            policies.push(p.as_str()?.to_string());
        }
        let alias = v.get("alias").and_then(Value::as_str).map(str::to_string);
        let max_bytes = v.get("max_bytes").and_then(Value::as_i64).map(|b| b.max(0) as u64);
        Some(Part { alias, mode, schema, policies, sort_keys: v.get("sort_keys").and_then(Value::as_bool).unwrap_or(false), max_bytes })
    }
}

/// What a row costs the coordinating cluster once answered, counted as it counts an answer
/// before parsing it: its encoded bytes, and per value they may hold, a parsed and decoded one.
pub fn row_bytes(r: &[V]) -> u64 {
    let text = Value::Arr(r.iter().map(encode_v).collect()).to_json();
    (text.len() as u64).saturating_add(json_elements(text.as_bytes()).saturating_mul(PER_ELEMENT))
}

/// A part's rows past [`Part::max_bytes`].
pub fn too_large(cap: u64, rows: usize) -> EsError {
    EsError::new(
        429,
        "circuit_breaking_exception",
        format!("[remote_cluster_response] Data too large: this cluster's part of the query would answer more than [{cap}b] ([{rows}] rows so far), the most the coordinating cluster can hold; a LIMIT the clusters can apply before the commands that run there bounds it"),
    )
}

/// A remote's part request: the query fields the client sent (`forwarded`), the part's text, and
/// the part itself in the body; the header only marks it (honored on a link's request alone).
fn part_request(template: &EsRequest, forwarded: &Map, text: &str, part: &Part) -> EsRequest {
    let mut body = forwarded.clone();
    body.insert("query".into(), Value::Str(text.to_string()));
    body.insert(PART_FIELD.into(), part.to_value());
    let mut req = EsRequest::json("POST", "/_query", Value::Obj(body).to_json().into_bytes());
    req.headers.extend(template.headers.iter().cloned());
    req.headers.push((PART_HEADER.into(), "1".into()));
    req
}

/// A `STATS` part's result: its input columns and each group's key and states.
#[derive(Debug, Default)]
pub struct Partial {
    pub input: Vec<Col>,
    pub groups: Vec<(Vec<V>, Vec<Acc>)>,
}

/// One cluster's figures for `_clusters.details`.
#[derive(Clone, Debug)]
pub struct ClusterInfo {
    pub name: String,
    pub indices: String,
    pub status: &'static str,
    /// `None`: the query's own `took` (the local cluster).
    pub took: Option<i64>,
    pub shards: i64,
    pub failures: Vec<Value>,
}

impl ClusterInfo {
    fn to_value(&self, took: i64) -> Value {
        let mut m = Map::new();
        m.insert("status".into(), self.status.into());
        m.insert("indices".into(), self.indices.clone().into());
        m.insert("took".into(), Value::from(self.took.unwrap_or(took)));
        let ok = if self.status == "successful" { self.shards } else { 0 };
        m.insert("_shards".into(), obj! { "total" => self.shards, "successful" => ok, "skipped" => 0, "failed" => 0 });
        if !self.failures.is_empty() {
            m.insert("failures".into(), Value::Arr(self.failures.clone()));
        }
        Value::Obj(m)
    }
}

/// `_clusters` (`EsqlExecutionInfo`): `(local)` first, then the remotes by alias.
pub fn clusters_section(parts: &[ClusterInfo], took: i64) -> Value {
    let count = |s: &str| parts.iter().filter(|p| p.status == s).count() as i64;
    let mut details = Map::new();
    for p in parts {
        details.insert(p.name.clone(), p.to_value(took));
    }
    obj! {
        "total" => parts.len() as i64,
        "successful" => count("successful"),
        "running" => 0,
        "skipped" => count("skipped"),
        "partial" => count("partial"),
        "failed" => count("failed"),
        "details" => Value::Obj(details),
    }
}

/// A linked remote the request may reach.
struct Linked {
    remote: Remote,
    target: String,
    /// The request carrying the caller's remote access (body and part header added per query).
    template: EsRequest,
    /// Why the caller may not reach this remote: no `remote_indices` naming it (Elasticsearch's
    /// 403 for a query over remotes alone; with local indices too, the remote is skipped).
    refusal: Option<EsError>,
}

/// The linked remotes a query names, each with its expressions there.
type Legs<'a> = Vec<(&'a Linked, Vec<String>)>;

/// Each cluster's part, by alias and expressions, as it ended.
type Outcomes = Vec<(String, String, Outcome)>;

/// What a query needs to send parts to linked remotes, taken from its request.
pub struct Ccs {
    links: Arc<dyn Links>,
    local: String,
    wait: Duration,
    configured: Vec<Remote>,
    linked: Vec<Linked>,
    /// The request's own fields every part gets: `params`, `filter`, `locale`, `time_zone`.
    forwarded: Map,
    /// The request's parameters as parsed: each part's text is parsed with them here, as its
    /// cluster will parse it, before it's sent.
    params: Vec<(Option<String>, super::parser::Param)>,
}

tokio::task_local! {
    static CCS: Arc<Ccs>;
    static PART: Arc<Part>;
}

impl Ccs {
    /// The request's remotes, when its cluster has links (Elasticsearch flavor) and the request
    /// isn't itself a part.
    pub fn of(ctx: &Ctx, body: &Value, params: &[(Option<String>, super::parser::Param)]) -> Option<Arc<Ccs>> {
        let links = ctx.cluster.config.links.clone()?;
        if ctx.cluster.dialect != crate::cluster::Dialect::Elasticsearch {
            return None;
        }
        let configured = crate::remote::remotes(&ctx.state());
        let p = crate::xpack::security::principal(ctx);
        let linked: Vec<Linked> = configured
            .iter()
            .filter_map(|r| {
                let target = r.link(&*links)?;
                let refusal = links::refusal(ctx, r).map(|_| {
                    EsError::security(
                        403,
                        format!(
                            "action [indices:data/read/esql/resolve_fields] towards remote cluster [{}] is unauthorized for {} because no remote indices privileges apply for the target cluster",
                            r.alias,
                            crate::xpack::security::who(p)
                        ),
                    )
                });
                Some(Linked { remote: r.clone(), target, template: links::request(ctx, &r.alias, "POST", "/_query", None), refusal })
            })
            .collect();
        if linked.is_empty() {
            return None;
        }
        let mut forwarded = Map::new();
        for k in ["params", "filter", "locale", "time_zone"] {
            if let Some(v) = body.get(k) {
                forwarded.insert(k.into(), v.clone());
            }
        }
        // `settings.time_zone` is the same setting.
        if let Some(z) = body.get("settings").and_then(|s| s.get("time_zone")) {
            forwarded.insert("time_zone".into(), z.clone());
        }
        Some(Arc::new(Ccs { links, local: links::local_ref(ctx.cluster).to_string(), wait: links::wait(ctx.cluster), configured, linked, forwarded, params: params.to_vec() }))
    }

    /// Whether expression `e` names only linked remotes ([`links::names_link`]'s rule).
    fn names_link(&self, e: &str) -> bool {
        let Some((pattern, _)) = crate::remote::split_expression(e) else { return false };
        let mut matched = self.configured.iter().filter(|r| crate::xcontent::filter::glob_match(pattern, &r.alias)).peekable();
        matched.peek().is_some() && matched.all(|r| self.linked.iter().any(|l| l.remote.alias == r.alias))
    }

    /// Splits `sources` into the local part's and each linked remote's (in alias order);
    /// `None` when they name no linked remote. An exclusion of a remote's (`-far:x`) is that
    /// remote's own (`-x`), as Elasticsearch applies it.
    fn split(&self, sources: &[String]) -> Option<(Vec<String>, Legs<'_>)> {
        let mut local = Vec::new();
        let mut legs: Legs<'_> = Vec::new();
        for e in sources {
            let (exclude, bare) = match e.strip_prefix('-') {
                Some(b) if crate::remote::split_expression(b).is_some() => (true, b),
                _ => (false, e.as_str()),
            };
            if !self.names_link(bare) {
                local.push(e.clone());
                continue;
            }
            let (pattern, index) = crate::remote::split_expression(bare).expect("names a link");
            let index = if exclude { format!("-{index}") } else { index.to_string() };
            for l in self.linked.iter().filter(|l| crate::xcontent::filter::glob_match(pattern, &l.remote.alias)) {
                match legs.iter_mut().find(|(x, _)| x.remote.alias == l.remote.alias) {
                    Some((_, exprs)) => exprs.push(index.clone()),
                    None if !exclude => legs.push((l, vec![index.clone()])),
                    None => {}
                }
            }
        }
        if legs.is_empty() {
            return None;
        }
        legs.sort_by(|a, b| a.0.remote.alias.cmp(&b.0.remote.alias));
        Some((local, legs))
    }

    /// A query naming linked remotes that [`run`] doesn't cut into parts (`FORK`, subqueries,
    /// views, `TS`, `SET unmapped_fields`...) reads the remotes' rows here, through cross-cluster
    /// search. A command reading each cluster's own policy or lookup index would read this
    /// cluster's there, so it's refused: with Elasticsearch's placement error where it has one
    /// (each `FORK` branch checked after the commands before it), else roll's. An `ENRICH` in
    /// `_coordinator` mode, or in `_any` mode after a `LIMIT`, `STATS`, `FORK` or
    /// `ENRICH _coordinator`, is this cluster's in Elasticsearch too, and runs.
    pub fn check_fallback(&self, src: &str, spans: &[Span], cmds: &[Cmd]) -> EsResult<()> {
        if let Some(Cmd::Union { branches, .. }) = cmds.first() {
            for b in branches {
                crate::stack::maybe_grow(|| self.check_fallback(&b.src, &b.spans, &b.cmds))?;
            }
        }
        if !self.reads_remote(cmds) {
            return Ok(());
        }
        check_placement(src, spans, cmds)?;
        for (f, c) in cmds.iter().enumerate() {
            let Cmd::Fork { branches, spans: at, .. } = c else { continue };
            for (b, b_spans) in branches.iter().zip(at) {
                let joined: Vec<Cmd> = cmds[..f].iter().chain(b).cloned().collect();
                let joined_spans: Vec<Span> = if spans.len() >= f { spans[..f].iter().chain(b_spans).copied().collect() } else { Vec::new() };
                check_placement(src, &joined_spans, &joined)?;
                refuse_resources(src, &joined)?;
            }
        }
        refuse_resources(src, cmds)
    }

    /// Whether the rows `cmds` start from include a linked remote's.
    fn reads_remote(&self, cmds: &[Cmd]) -> bool {
        match cmds.first() {
            Some(Cmd::From { sources, .. } | Cmd::Ts { sources, .. }) => self.split(sources).is_some(),
            Some(Cmd::Union { branches, .. }) => branches.iter().any(|b| crate::stack::maybe_grow(|| self.reads_remote(&b.cmds))),
            _ => false,
        }
    }
}

/// Refuses the first command of `cmds`, a pipeline over remote rows read here
/// ([`Ccs::check_fallback`]), that would read this cluster's policy or lookup index where
/// Elasticsearch reads each cluster's.
fn refuse_resources(src: &str, cmds: &[Cmd]) -> EsResult<()> {
    const WHY: &str = "roll reads the remote clusters' rows here in a query with FORK, subqueries, views, TS or SET unmapped_fields";
    let mut coordinator = false;
    for c in cmds.iter().skip(1) {
        match c {
            Cmd::Limit(_) | Cmd::Stats { .. } | Cmd::Fork { .. } => coordinator = true,
            Cmd::Enrich { policy, at, .. } => match enrich_mode(policy) {
                "_coordinator" => coordinator = true,
                "_any" if coordinator => {}
                _ => return Err(super::exec::verification_at(src, *at, &format!("ENRICH [{policy}] can't use each cluster's own policy: {WHY}"))),
            },
            Cmd::LookupJoin { index_at, .. } => return Err(super::exec::verification_at(src, *index_at, &format!("LOOKUP JOIN can't use each cluster's own lookup index: {WHY}"))),
            _ => {}
        }
    }
    Ok(())
}

/// Runs `f` with `ccs` as the query's remotes.
pub async fn scope<F: std::future::Future>(ccs: Option<Arc<Ccs>>, f: F) -> F::Output {
    match ccs {
        Some(c) => CCS.scope(c, f).await,
        None => f.await,
    }
}

/// Runs `f` as a cluster's part.
pub async fn part_scope<F: std::future::Future>(part: Option<Arc<Part>>, f: F) -> F::Output {
    match part {
        Some(p) => PART.scope(p, f).await,
        None => f.await,
    }
}

/// The running query's remotes.
pub fn current() -> Option<Arc<Ccs>> {
    CCS.try_with(Arc::clone).ok()
}

/// The part the running query is, if it is one.
pub fn part() -> Option<Arc<Part>> {
    PART.try_with(Arc::clone).ok()
}

/// The part a link's request asks to run ([`PART_HEADER`], the part in the body's
/// [`PART_FIELD`], taken out of it); `None` for anyone else's request, whose body keeps the
/// field (and fails as an unknown one).
pub fn requested(ctx: &Ctx, body: &mut Value) -> EsResult<Option<Arc<Part>>> {
    if ctx.request.header(PART_HEADER).is_none() || !links::is_link_call(ctx) {
        return Ok(None);
    }
    let v = body.as_object_mut().and_then(|m| m.shift_remove(PART_FIELD)).unwrap_or(Value::Null);
    // A remote's part always names the alias it's reached by: every check keyed on it applies.
    Part::of_value(&v).filter(|p| p.alias.is_some()).map(|p| Some(Arc::new(p))).ok_or_else(|| EsError::illegal_argument("an ES|QL part roll can't read"))
}

/// A remote part's `FROM` naming ES|QL views: Elasticsearch's 400 (views on remote clusters
/// aren't queried across clusters). `sources` are the remote's own expressions.
pub fn check_remote_views(st: &crate::meta::ClusterState, alias: &str, sources: &[String]) -> EsResult<()> {
    let mut matched: Vec<String> = Vec::new();
    for (name, _) in st.metadata.xpack.iter(super::VIEWS) {
        let hit = sources.iter().filter(|s| !s.starts_with('-')).any(|s| crate::xcontent::filter::glob_match(s, name));
        let excluded = sources.iter().filter_map(|s| s.strip_prefix('-')).any(|s| crate::xcontent::filter::glob_match(s, name));
        if hit && !excluded {
            matched.push(name.clone());
        }
    }
    if matched.is_empty() {
        return Ok(());
    }
    matched.sort();
    let named: Vec<String> = matched.iter().map(|v| format!("{alias}:{v}")).collect();
    let excluding: Vec<String> = matched.iter().map(|v| format!("{alias}:-{v}")).collect();
    Err(EsError::new(
        400,
        "remote_view_not_supported_exception",
        format!("ES|QL queries with remote views are not supported. Matched [{}]. Remove them from the query pattern or exclude them with [{}] if matched by a wildcard.", named.join(", "), excluding.join(", ")),
    ))
}

/// Every type, by its [`DataType::name`].
const TYPES: &[DataType] = &[
    DataType::Null,
    DataType::Boolean,
    DataType::Integer,
    DataType::Long,
    DataType::UnsignedLong,
    DataType::Double,
    DataType::Keyword,
    DataType::Text,
    DataType::Date,
    DataType::DateNanos,
    DataType::Ip,
    DataType::Version,
    DataType::GeoPoint,
    DataType::CartesianPoint,
    DataType::GeoShape,
    DataType::CartesianShape,
    DataType::DatePeriod,
    DataType::TimeDuration,
    DataType::Source,
    DataType::AggregateMetricDouble,
    DataType::CounterLong,
    DataType::CounterInteger,
    DataType::CounterDouble,
    DataType::Histogram,
    DataType::ExponentialHistogram,
    DataType::TDigest,
    DataType::DenseVector,
    DataType::Tsid,
    DataType::Flattened,
    DataType::Geohash,
    DataType::Geotile,
    DataType::Geohex,
    DataType::Unsupported,
];

/// Element `i` of a two-element array.
fn pair(v: &Value, i: usize) -> Option<&Value> {
    v.as_array().filter(|a| a.len() == 2).and_then(|a| a.get(i))
}

fn type_named(n: &str) -> Option<DataType> {
    TYPES.iter().copied().find(|t| t.name() == n)
}

/// Where the parts stop and what runs here after them.
#[derive(Debug)]
pub struct Cut {
    pub mode: Mode,
    /// The query's commands (indices into it, after the source) every part runs.
    pub part: Vec<usize>,
    /// A `LIMIT n` the parts end with that the query text doesn't hold (the implicit one).
    pub append_limit: Option<usize>,
    /// What runs here over every part's result (after merging `STATS` states, for
    /// [`Mode::Stats`]).
    pub rest: Vec<Cmd>,
    /// The `STATS` the parts reduce partially.
    pub stats: Option<Cmd>,
    /// The parts carry their sort keys' values ([`Part::sort_keys`]).
    pub sort_keys: bool,
    /// The parts end in a `LIMIT` (or a `STATS`): their answers are bounded. Otherwise a part's
    /// rows are capped at what this cluster's request breaker can hold ([`Part::max_bytes`]).
    pub bounded: bool,
}

/// Commands that keep one row per row and read nothing but that row: a `LIMIT` after them may
/// run before them (`ENRICH` in any mode adds columns, never rows), as Elasticsearch pushes it.
fn commutes_with_limit(c: &Cmd) -> bool {
    matches!(c, Cmd::Eval(_) | Cmd::Keep(_) | Cmd::Drop(_) | Cmd::Rename(_) | Cmd::Dissect { .. } | Cmd::Grok { .. } | Cmd::Enrich { .. })
}

fn enrich_mode(policy: &str) -> &str {
    super::parser::split_policy(policy).0
}

/// Commands a part runs row by row (each cluster with its own policies and lookup indices).
fn runs_in_parts(c: &Cmd) -> bool {
    match c {
        Cmd::Where(_) | Cmd::Eval(_) | Cmd::Keep(_) | Cmd::Drop(_) | Cmd::Rename(_) | Cmd::MvExpand(..) | Cmd::Dissect { .. } | Cmd::Grok { .. } | Cmd::Sample(_) | Cmd::LookupJoin { .. } => true,
        Cmd::Enrich { policy, .. } => enrich_mode(policy) != "_coordinator",
        _ => false,
    }
}

/// Commands between a `SORT` and its `LIMIT` that each part runs too: they keep one row per
/// row (an `ENRICH` before the limit uses each cluster's policy, as Elasticsearch runs it).
fn between_sort_and_limit(c: &Cmd) -> bool {
    match c {
        Cmd::Eval(_) | Cmd::Keep(_) | Cmd::Drop(_) | Cmd::Rename(_) => true,
        Cmd::Enrich { policy, .. } => enrich_mode(policy) != "_coordinator",
        _ => false,
    }
}

/// Commands after a part's `LIMIT` that it still runs, when an `ENRICH _remote` comes later
/// among them: they keep one row per row and read nothing but that row.
fn after_limit(c: &Cmd) -> bool {
    match c {
        Cmd::Eval(_) | Cmd::Keep(_) | Cmd::Drop(_) | Cmd::Rename(_) | Cmd::Dissect { .. } | Cmd::Grok { .. } => true,
        Cmd::Enrich { policy, .. } => enrich_mode(policy) == "_remote",
        _ => false,
    }
}

/// The run of commands from `from` a part runs after its limit: [`after_limit`] commands up to
/// the last `ENRICH _remote` among them (none: nothing).
fn trailing_remote(cmds: &[Cmd], from: usize) -> usize {
    let run = cmds[from..].iter().take_while(|c| after_limit(c)).count();
    cmds[from..from + run].iter().rposition(|c| matches!(c, Cmd::Enrich { .. })).map_or(from, |p| from + p + 1)
}

/// Where `cmds` (the source first; `parsed` of them are the text's, a last one may be the
/// implicit `LIMIT`) split into the parts and the rest.
pub fn cut(cmds: &[Cmd], parsed: usize) -> Cut {
    let mut part = Vec::new();
    let mut i = 1;
    while i < cmds.len() && runs_in_parts(&cmds[i]) {
        part.push(i);
        i += 1;
    }
    let synthetic = |k: usize| k >= parsed;
    match cmds.get(i) {
        Some(c @ Cmd::Stats { .. }) => {
            part.push(i);
            Cut { mode: Mode::Stats, part, append_limit: None, rest: cmds[i + 1..].to_vec(), stats: Some(c.clone()), sort_keys: false, bounded: true }
        }
        Some(Cmd::Limit(n)) => {
            let mut append_limit = None;
            let after = if synthetic(i) {
                append_limit = Some(*n);
                i + 1
            } else {
                part.push(i);
                let end = trailing_remote(cmds, i + 1);
                part.extend(i + 1..end);
                end
            };
            let mut rest = vec![Cmd::Limit(*n)];
            rest.extend(cmds[after..].iter().cloned());
            Cut { mode: Mode::Rows, part, append_limit, rest, stats: None, sort_keys: false, bounded: true }
        }
        // `SORT ... | (EVAL, KEEP, ENRICH...)* | LIMIT n`: every part sorts, runs the commands
        // between and limits (and an `ENRICH _remote` after), then the rows sort again here.
        Some(Cmd::Sort(keys)) if matches!(cmds.get(i + 1 + cmds[i + 1..].iter().take_while(|c| between_sort_and_limit(c)).count()), Some(Cmd::Limit(_))) => {
            let j = i + 1 + cmds[i + 1..].iter().take_while(|c| between_sort_and_limit(c)).count();
            let Some(Cmd::Limit(n)) = cmds.get(j) else { unreachable!() };
            part.extend(i..j);
            let mut append_limit = None;
            let after = if synthetic(j) {
                append_limit = Some(*n);
                j + 1
            } else {
                part.push(j);
                let end = trailing_remote(cmds, j + 1);
                part.extend(j + 1..end);
                end
            };
            // Commands the parts ran after their sort may drop or overwrite a key: they carry the
            // keys' values from the sort, and the rows sort on those here.
            let sort_keys = j > i + 1 || after > j + 1;
            let mut rest = if sort_keys {
                let carried: Vec<super::parser::SortKey> = keys
                    .iter()
                    .enumerate()
                    .map(|(k, key)| super::parser::SortKey { expr: super::parser::Expr::new(super::parser::ExprKind::Col(format!("{SORT_CARRY}{k}")), key.expr.start, key.expr.end), asc: key.asc, nulls_first: key.nulls_first })
                    .collect();
                let names = (0..keys.len()).map(|k| (format!("{SORT_CARRY}{k}"), 0)).collect();
                vec![Cmd::Sort(carried), Cmd::Limit(*n), Cmd::Drop(names)]
            } else {
                vec![Cmd::Sort(keys.clone()), Cmd::Limit(*n)]
            };
            rest.extend(cmds[after..].iter().cloned());
            Cut { mode: Mode::Rows, part, append_limit, rest, stats: None, sort_keys, bounded: true }
        }
        // Anything else runs here over the parts' rows. A `LIMIT` the commands up to it commute
        // with (the implicit one too) still bounds the parts; without one they're capped.
        _ => {
            let k = i + cmds[i..].iter().take_while(|c| commutes_with_limit(c)).count();
            let limit = match cmds.get(k) {
                Some(Cmd::Limit(n)) => Some(*n),
                _ => None,
            };
            Cut { mode: Mode::Rows, part, append_limit: limit, rest: cmds[i..].to_vec(), stats: None, sort_keys: false, bounded: limit.is_some() }
        }
    }
}

/// Commands left to the coordinating cluster that would read its own resources where
/// Elasticsearch reads each cluster's: an `ENRICH _remote` or `LOOKUP JOIN`, or an `ENRICH` in
/// `_any` mode before any `LIMIT`, `STATS` or `ENRICH _coordinator` (after one, it's the
/// coordinator's, as there). Refused with Elasticsearch's error, naming the command the parts
/// stopped at (a `SORT ... | LIMIT` by its `SORT`, as Elasticsearch names a top-n); never run
/// here.
pub fn check_left_here(src: &str, spans: &[Span], cmds: &[Cmd], cut: &Cut) -> EsResult<()> {
    let Some(first) = (1..cmds.len()).find(|k| !cut.part.contains(k)) else { return Ok(()) };
    let breaker = (1..cmds.len()).find(|&k| !runs_in_parts(&cmds[k])).unwrap_or(first);
    let after = spans.get(breaker).map(|s| cmd_text(src, s)).unwrap_or_default();
    // A `LIMIT` or `STATS` the parts ran is behind everything left here.
    let mut coordinators = cut.part.iter().any(|&k| matches!(cmds[k], Cmd::Limit(_) | Cmd::Stats { .. }));
    for (k, c) in cmds.iter().enumerate().skip(first) {
        if cut.part.contains(&k) {
            continue;
        }
        match c {
            Cmd::Limit(_) | Cmd::Stats { .. } => coordinators = true,
            Cmd::Enrich { policy, at: e, .. } => match enrich_mode(policy) {
                "_coordinator" => coordinators = true,
                "_any" if coordinators => {}
                mode => {
                    let limit = cmds[breaker..k].iter().position(|c| matches!(c, Cmd::Limit(_))).map(|l| breaker + l);
                    if mode == "_remote" && limit.is_some_and(|l| cmds[l..k].iter().any(|c| matches!(c, Cmd::MvExpand(..)))) {
                        return Err(super::exec::verification_at(src, *e, "MV_EXPAND after LIMIT is incompatible with remote ENRICH"));
                    }
                    return Err(super::exec::verification_at(src, *e, &format!("ENRICH with remote policy can't be executed after {after}")));
                }
            },
            Cmd::LookupJoin { .. } => {
                let start = spans.get(k).map_or(0, |s| s.start);
                return Err(super::exec::verification_at(src, start, &format!("LOOKUP JOIN with remote indices can't be executed after {after}")));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Whether a part's commands (`cmds`, as the part parsed them) are a shape the coordinating
/// cluster cuts for `mode` ([`cut`]): the source naming this cluster's own indices, commands
/// that run row by row, then exactly one `STATS` ([`Mode::Stats`]), or at most a `LIMIT` (after
/// a `SORT` and the commands between them) and the commands after it up to an `ENRICH _remote`
/// ([`Mode::Rows`]). Whatever the request says, a remote runs only such a part.
pub fn check_shape(cmds: &[Cmd], mode: Mode) -> Result<(), String> {
    let Some(Cmd::From { sources, .. }) = cmds.first() else { return Err("its source isn't a FROM".into()) };
    if sources.iter().any(|s| crate::remote::split_expression(s.trim_start_matches('-')).is_some()) {
        return Err("its FROM names another cluster's indices".into());
    }
    let i = 1 + cmds[1..].iter().take_while(|c| runs_in_parts(c)).count();
    let tail = &cmds[i..];
    let ok = match mode {
        Mode::Stats => matches!(tail, [Cmd::Stats { .. }]),
        Mode::Rows => {
            let from_limit = match tail {
                [] => None,
                [Cmd::Limit(_), ..] => Some(0),
                [Cmd::Sort(_), rest @ ..] => {
                    let m = rest.iter().take_while(|c| between_sort_and_limit(c)).count();
                    matches!(rest.get(m), Some(Cmd::Limit(_))).then_some(1 + m).or(Some(usize::MAX))
                }
                _ => Some(usize::MAX),
            };
            match from_limit {
                None => true,
                Some(usize::MAX) => false,
                Some(l) => trailing_remote(tail, l + 1) == tail.len(),
            }
        }
    };
    if ok { Ok(()) } else { Err(format!("its commands aren't a {} part", if mode == Mode::Stats { "STATS" } else { "rows" })) }
}

/// What a remote's part may run, decided from the query it parsed (never from what the request
/// says about it): a shape [`check_shape`] admits, Elasticsearch's placement rules, and for every
/// `ENRICH` it runs (and every policy it was told to have) `monitor_enrich` on this cluster as
/// the link's principal, then the policies themselves.
pub fn admit_part(ctx: &Ctx, q: &Query, part: &Part) -> EsResult<()> {
    check_shape(&q.cmds, part.mode).map_err(|why| EsError::illegal_argument(format!("an ES|QL part this cluster won't run: {why}")))?;
    check_placement(&q.src, &q.spans, &q.cmds)?;
    let mut needed: Vec<String> = policies(&q.cmds).into_iter().map(|(name, _, _)| name).chain(part.policies.iter().cloned()).collect();
    needed.sort();
    needed.dedup();
    if needed.is_empty() {
        return Ok(());
    }
    crate::xpack::security::check_cluster_action(ctx, RESOLVE_POLICY)?;
    let st = ctx.state();
    let missing: Vec<&str> = needed.iter().filter(|p| st.metadata.xpack.get(crate::xpack::enrich::POLICIES, p).is_none()).map(String::as_str).collect();
    if missing.is_empty() { Ok(()) } else { Err(EsError::new(400, MISSING_POLICY, missing.join(","))) }
}

/// The query text a remote's part runs: the query's own, its `FROM` naming `sources` (padded to
/// the same length, so the commands keep their positions), the commands it doesn't run blanked,
/// cut after the last it runs. `None` when that can't keep the positions (a source command
/// across lines too short for the new one, a parameter in a blanked command).
pub fn part_text(src: &str, spans: &[Span], sources: &[String], metadata: &[String], keep: &[usize], append_limit: Option<usize>) -> Option<String> {
    let from = spans.first()?;
    let region = src.get(from.start..from.end)?;
    if region.contains('?') || sources.iter().any(|s| s.contains('"') || s.contains('\\')) {
        return None;
    }
    let quoted: Vec<String> = sources.iter().map(|s| format!("\"{s}\"")).collect();
    let mut head = format!("FROM {}", quoted.join(","));
    if !metadata.is_empty() {
        head.push_str(" METADATA ");
        head.push_str(&metadata.join(","));
    }
    if head.len() > region.len() || region.as_bytes()[..head.len()].contains(&b'\n') {
        return None;
    }
    let blank = |s: &str| -> String { s.chars().map(|c| if c == '\n' { '\n' } else { ' ' }).collect() };
    let mut out = String::with_capacity(src.len());
    out.push_str(&src[..from.start]);
    out.push_str(&head);
    out.push_str(&blank(&region[head.len()..]));
    let last = keep.iter().copied().max().unwrap_or(0);
    let mut prev = from.end;
    for (k, s) in spans.iter().enumerate().skip(1) {
        if k > last {
            break;
        }
        let piece = src.get(prev..s.end)?;
        if keep.contains(&k) {
            out.push_str(piece);
        } else {
            if piece.contains('?') {
                return None;
            }
            out.push_str(&blank(piece));
        }
        prev = s.end;
    }
    if let Some(n) = append_limit {
        out.push_str(&format!(" | LIMIT {n}"));
    }
    Some(out)
}

/// The text of command `k` (`STATS ...`) and where it starts, as Elasticsearch names a command in
/// its verification messages (`[STATS c=COUNT(*) BY tag]@1:24`).
fn cmd_text(src: &str, s: &Span) -> String {
    let (l, c) = super::parser::location(src, s.start);
    format!("[{}]@{l}:{c}", src[s.start..s.end].trim_end())
}

/// Elasticsearch's checks on where commands that read each cluster's own resources may go in a
/// query over remote clusters: `ENRICH _remote` not after `STATS`, `INLINE STATS`, `FORK` or
/// `ENRICH _coordinator`, `LOOKUP JOIN` not after those, a `LIMIT` or a `SORT`. (Elasticsearch
/// drops an `INLINE STATS` whose columns nothing reads first; roll refuses it as one that's read.)
pub fn check_placement(src: &str, spans: &[Span], cmds: &[Cmd]) -> EsResult<()> {
    let mut limited: Option<usize> = None;
    let mut coordinator: Option<usize> = None;
    for (k, c) in cmds.iter().enumerate().skip(1) {
        let Some(span) = spans.get(k) else { break };
        let after = |b: Option<usize>| b.and_then(|b| spans.get(b)).map(|s| cmd_text(src, s));
        match c {
            // `LOOKUP JOIN`'s error names the nearest command before it that stops it.
            Cmd::Stats { .. } | Cmd::InlineStats { .. } | Cmd::Fork { .. } => {
                limited = Some(k);
                coordinator.get_or_insert(k);
            }
            Cmd::Limit(_) | Cmd::Sort(_) => {
                limited = Some(k);
            }
            Cmd::Enrich { policy, at, .. } => match enrich_mode(policy) {
                "_coordinator" => {
                    coordinator.get_or_insert(k);
                    limited = Some(k);
                }
                "_remote" => {
                    if let Some(t) = after(coordinator) {
                        return Err(super::exec::verification_at(src, *at, &format!("ENRICH with remote policy can't be executed after {t}")));
                    }
                }
                _ => {}
            },
            Cmd::LookupJoin { .. } => {
                if let Some(t) = after(limited) {
                    return Err(super::exec::verification_at(src, span.start, &format!("LOOKUP JOIN with remote indices can't be executed after {t}")));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The enrich policies (name, mode, where in the text) of `cmds`.
fn policies(cmds: &[Cmd]) -> Vec<(String, String, usize)> {
    cmds.iter()
        .filter_map(|c| match c {
            Cmd::Enrich { policy, policy_at, .. } => {
                let (mode, name) = super::parser::split_policy(policy);
                Some((name.to_string(), mode.to_string(), *policy_at))
            }
            _ => None,
        })
        .collect()
}

/// Encodes a value for a part's answer: lossless and typed (`["i", 1]` an integer, `["d",
/// "<bits>"]` a double, `["t", ms]` a date, `["p", "ip"]`, `["u", "digits"]`, `["m", [...]]` one
/// level of multi-values; nulls, booleans, longs and strings as themselves).
pub fn encode_v(v: &V) -> Value {
    match v {
        V::Null => Value::Null,
        V::Bool(b) => Value::Bool(*b),
        V::Int(i) => crate::arr!["i", *i as i64],
        V::Long(l) => Value::from(*l),
        V::ULong(u) => crate::arr!["u", u.to_string()],
        V::Double(d) => crate::arr!["d", d.to_bits().to_string()],
        V::Str(s) => Value::Str(s.clone()),
        V::Date(d) => crate::arr!["t", *d],
        V::Ip(s) => crate::arr!["p", s.clone()],
        V::Multi(vs) => crate::arr!["m", Value::Arr(vs.iter().map(encode_v).collect())],
    }
}

/// [`encode_v`]'s inverse; nested multi-values and anything else are refused.
pub fn decode_v(v: &Value, multi: bool) -> Result<V, String> {
    Ok(match v {
        Value::Null => V::Null,
        Value::Bool(b) => V::Bool(*b),
        Value::Str(s) => V::Str(s.clone()),
        Value::Num(_) => V::Long(v.as_i64().filter(|_| v.as_number().is_some_and(|n| n.is_integer())).ok_or("a number that isn't a long")?),
        Value::Arr(a) if a.len() == 2 => {
            let tag = a[0].as_str().ok_or("an untagged value")?;
            let int = || a[1].as_i64().filter(|_| a[1].as_number().is_some_and(|n| n.is_integer())).ok_or("a bad number");
            let text = || a[1].as_str().ok_or("a bad text");
            match tag {
                "i" => V::Int(i32::try_from(int()?).map_err(|_| "an integer out of range")?),
                "u" => V::ULong(text()?.parse().map_err(|_| "a bad unsigned long")?),
                "d" => V::Double(f64::from_bits(text()?.parse().map_err(|_| "a bad double")?)),
                "t" => V::Date(int()?),
                "p" => V::Ip(text()?.to_string()),
                "m" if multi => {
                    let mut out = Vec::new();
                    for x in a[1].as_array().ok_or("bad multi-values")? {
                        out.push(decode_v(x, false)?);
                    }
                    V::Multi(out)
                }
                _ => return Err("a value of an unknown kind".into()),
            }
        }
        _ => return Err("a value of an unknown kind".into()),
    })
}

fn f64_bits(f: f64) -> Value {
    Value::Str(f.to_bits().to_string())
}

fn bits_f64(v: &Value) -> Result<f64, String> {
    v.as_str().and_then(|s| s.parse::<u64>().ok()).map(f64::from_bits).ok_or_else(|| "a bad double".into())
}

/// Encodes an aggregation state.
pub fn encode_acc(a: &Acc) -> Value {
    let opt = |x: &Option<f64>| x.map_or(Value::Null, f64_bits);
    match a {
        Acc::Count(n) => crate::arr!["c", *n],
        Acc::Amd { part } => crate::arr!["amd", Value::Arr(part.iter().map(opt).collect())],
        Acc::Points(p) => crate::arr!["pts", Value::Arr(p.iter().map(|(t, v)| crate::arr![*t, encode_v(v)]).collect())],
        Acc::Distinct { hll, ty } => {
            let (p, lc, regs) = hll.parts();
            let regs = regs.map_or(Value::Null, |r| {
                use base64::Engine as _;
                Value::Str(base64::engine::general_purpose::STANDARD.encode(r))
            });
            crate::arr!["hll", ty.name(), p as i64, Value::Arr(lc.into_iter().map(|x| Value::from(x as i64)).collect()), regs]
        }
        Acc::Sum { int, float, comp, any, overflow } => crate::arr!["sum", *int, f64_bits(*float), f64_bits(*comp), *any, *overflow],
        Acc::Avg { int, float, comp, n } => crate::arr!["avg", int.to_string(), f64_bits(*float), f64_bits(*comp), *n],
        Acc::MinMax(v) => crate::arr!["mm", v.as_ref().map_or(Value::Null, encode_v)],
        Acc::Collect(vs) => crate::arr!["col", Value::Arr(vs.iter().map(encode_v).collect())],
        Acc::Weighted(s, t) => crate::arr!["w", f64_bits(*s), f64_bits(*t)],
        Acc::Present(n, any) => crate::arr!["pr", *n, *any],
    }
}

/// [`encode_acc`]'s inverse, every part checked (a sketch through [`search_aggs`]'s validation).
pub fn decode_acc(v: &Value) -> Result<Acc, String> {
    let a = v.as_array().ok_or("a bad state")?;
    let at = |i: usize| a.get(i).ok_or_else(|| "a short state".to_string());
    let int = |i: usize| -> Result<i64, String> { at(i)?.as_i64().ok_or_else(|| "a bad number".into()) };
    let flag = |i: usize| -> Result<bool, String> { at(i)?.as_bool().ok_or_else(|| "a bad flag".into()) };
    let list = |i: usize| -> Result<&Vec<Value>, String> { at(i)?.as_array().ok_or_else(|| "a bad list".into()) };
    Ok(match at(0)?.as_str().unwrap_or_default() {
        "c" => Acc::Count(int(1)?),
        "amd" => {
            let l = list(1)?;
            if l.len() != 4 {
                return Err("a bad aggregate metric state".into());
            }
            let mut part = [None; 4];
            for (p, x) in part.iter_mut().zip(l) {
                *p = if x.is_null() { None } else { Some(bits_f64(x)?) };
            }
            Acc::Amd { part }
        }
        "pts" => {
            let mut out = Vec::new();
            for p in list(1)? {
                let p = p.as_array().filter(|p| p.len() == 2).ok_or("a bad point")?;
                out.push((p[0].as_i64().ok_or("a bad timestamp")?, decode_v(&p[1], true)?));
            }
            Acc::Points(out)
        }
        "hll" => {
            let ty = at(1)?.as_str().and_then(type_named).ok_or("a bad type")?;
            let p = u32::try_from(int(2)?).map_err(|_| "a bad precision")?;
            let mut lc = Vec::new();
            for x in list(3)? {
                lc.push(x.as_i64().and_then(|x| u32::try_from(x).ok()).ok_or("a bad hash")?);
            }
            let regs = match at(4)? {
                Value::Null => None,
                Value::Str(s) => {
                    use base64::Engine as _;
                    // Registers are 2^p bytes, p at most 18: anything longer is refused before decoding.
                    if s.len() > (1usize << 18) / 3 * 4 + 4 {
                        return Err("registers too long".into());
                    }
                    Some(base64::engine::general_purpose::STANDARD.decode(s).map_err(|_| "bad registers")?)
                }
                _ => return Err("bad registers".into()),
            };
            let hll = search_aggs::metrics::cardinality::Hll::from_parts(p, lc, regs).ok_or("a sketch roll doesn't make")?;
            Acc::Distinct { hll: Box::new(hll), ty }
        }
        "sum" => Acc::Sum { int: int(1)?, float: bits_f64(at(2)?)?, comp: bits_f64(at(3)?)?, any: flag(4)?, overflow: flag(5)? },
        "avg" => Acc::Avg { int: at(1)?.as_str().and_then(|s| s.parse().ok()).ok_or("a bad sum")?, float: bits_f64(at(2)?)?, comp: bits_f64(at(3)?)?, n: int(4)? },
        "mm" => Acc::MinMax(if at(1)?.is_null() { None } else { Some(decode_v(at(1)?, true)?) }),
        "col" => {
            let mut out = Vec::new();
            for x in list(1)? {
                out.push(decode_v(x, true)?);
            }
            Acc::Collect(out)
        }
        "w" => Acc::Weighted(bits_f64(at(1)?)?, bits_f64(at(2)?)?),
        "pr" => Acc::Present(int(1)?, flag(2)?),
        _ => return Err("a state of an unknown kind".into()),
    })
}

/// A part's result as it answers it ([`PART_FIELD`]), with the enrich fields of each policy it
/// checked on this cluster ([`enrich_fields`]).
pub fn encode_answer(table: &Table, mode: Mode, partial: Option<&Partial>, shards: i64, enrich: &[(String, Vec<String>)]) -> Value {
    let cols = |cols: &[Col]| Value::Arr(cols.iter().map(|c| crate::arr![c.name.clone(), c.ty.name()]).collect());
    let mut m = Map::new();
    m.insert("shards".into(), Value::from(shards));
    m.insert("enrich".into(), Value::Arr(enrich.iter().map(|(n, f)| crate::arr![n.clone(), Value::Arr(f.iter().cloned().map(Value::Str).collect())]).collect()));
    match mode {
        Mode::Stats => {
            m.insert("columns".into(), cols(partial.map_or(&table.cols, |p| &p.input)));
            let groups = partial.into_iter().flat_map(|p| &p.groups).map(|(k, a)| crate::arr![Value::Arr(k.iter().map(encode_v).collect()), Value::Arr(a.iter().map(encode_acc).collect())]).collect();
            m.insert("groups".into(), Value::Arr(groups));
        }
        Mode::Rows => {
            m.insert("columns".into(), cols(&table.cols));
            m.insert("rows".into(), Value::Arr(table.rows.iter().map(|r| Value::Arr(r.iter().map(encode_v).collect())).collect()));
        }
    }
    Value::Obj(m)
}

/// What a remote's part answered.
#[derive(Debug, Default)]
pub struct Answer {
    pub table: Table,
    pub partial: Option<Partial>,
    pub shards: i64,
    pub took: i64,
    pub documents_found: i64,
    pub values_loaded: i64,
    /// Each policy it checked, with its enrich fields there.
    pub enrich: Vec<(String, Vec<String>)>,
}

/// The enrich fields on this cluster of each policy a part's commands run or it was told to have
/// ([`admit_part`] checked them all): its [`Answer::enrich`].
pub fn enrich_fields(st: &crate::meta::ClusterState, cmds: &[Cmd], part: &Part) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for name in policies(cmds).into_iter().map(|(name, _, _)| name).chain(part.policies.iter().cloned()) {
        if !out.iter().any(|(n, _)| *n == name)
            && let Some(f) = super::exec::policy_fields(st, &name)
        {
            out.push((name, f));
        }
    }
    out
}

/// Decodes a part's answer (already parsed, its bytes held on the breaker): every row exactly as
/// wide as its columns and every value of its column's type ([`super::value::fits`]), values and
/// states checked as [`decode_v`] and [`decode_acc`] check them (and a `STATS` part's states
/// against the coordinating cluster's aggregations, when they merge).
/// `max_columns`: the most columns it may have (its schema's, and what its cluster's own lookup
/// indices may add).
pub fn decode_answer(v: &Value, mode: Mode, max_columns: usize) -> Result<Answer, String> {
    let p = v.get(PART_FIELD).ok_or("no part in the answer")?;
    let columns = p.get("columns").and_then(Value::as_array).ok_or("no columns")?;
    if columns.len() > max_columns {
        return Err(format!("more than [{max_columns}] columns"));
    }
    let mut cols = Vec::new();
    for c in columns {
        let (Some(n), Some(t)) = (pair(c, 0).and_then(Value::as_str), pair(c, 1).and_then(Value::as_str).and_then(type_named)) else { return Err("a bad column".into()) };
        cols.push(Col { name: n.to_string(), ty: t });
    }
    let num = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0).max(0);
    let mut out = Answer { shards: p.get("shards").and_then(Value::as_i64).unwrap_or(0).clamp(0, i32::MAX as i64), took: num("took"), documents_found: num("documents_found"), values_loaded: num("values_loaded"), ..Answer::default() };
    for e in p.get("enrich").and_then(Value::as_array).ok_or("no enrich fields")? {
        let (Some(name), Some(fields)) = (pair(e, 0).and_then(Value::as_str), pair(e, 1).and_then(Value::as_array)) else { return Err("bad enrich fields".into()) };
        let fields = fields.iter().map(|f| f.as_str().map(str::to_string)).collect::<Option<Vec<String>>>().ok_or("bad enrich fields")?;
        out.enrich.push((name.to_string(), fields));
    }
    match mode {
        Mode::Rows => {
            let mut rows = Vec::new();
            for r in p.get("rows").and_then(Value::as_array).ok_or("no rows")? {
                let r = r.as_array().filter(|r| r.len() == cols.len()).ok_or("a row of another width")?;
                let mut row = Vec::with_capacity(r.len());
                for (x, c) in r.iter().zip(&cols) {
                    let v = decode_v(x, true)?;
                    if !super::value::fits(c.ty, &v) {
                        return Err(format!("a value that isn't of its column's type [{}]", c.ty.name()));
                    }
                    row.push(v);
                }
                rows.push(row);
            }
            out.table = Table { cols, rows };
        }
        Mode::Stats => {
            let mut groups = Vec::new();
            for g in p.get("groups").and_then(Value::as_array).ok_or("no groups")? {
                let g = g.as_array().filter(|g| g.len() == 2).ok_or("a bad group")?;
                let mut key = Vec::new();
                for x in g[0].as_array().ok_or("a bad key")? {
                    key.push(decode_v(x, false)?);
                }
                let mut accs = Vec::new();
                for x in g[1].as_array().ok_or("bad states")? {
                    accs.push(decode_acc(x)?);
                }
                groups.push((key, accs));
            }
            out.partial = Some(Partial { input: cols, groups });
        }
    }
    Ok(out)
}

/// The concrete indices' shard count on `cluster` (`_clusters.details.*._shards.total`).
pub fn shards_of(cluster: &crate::cluster::Cluster, indices: Option<&[String]>) -> i64 {
    let st = cluster.state();
    indices.into_iter().flatten().filter_map(|i| st.metadata.indices.get(i.as_str())).map(|m| m.number_of_shards() as i64).sum()
}

/// A remote's part, sent. Aborted when it's dropped unawaited (the local part failed, the client
/// went away, an error ended the query first): no leg outlives its query.
struct Leg {
    alias: String,
    skip_unavailable: bool,
    indices: String,
    handle: tokio::task::JoinHandle<Result<crate::rest::EsResponse, EsError>>,
}

impl Drop for Leg {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// One cluster's part's outcome.
enum Outcome {
    Done(Answer),
    /// Skipped (`skip_unavailable`), with why.
    Skipped(EsError),
}

/// Whether a remote's failed part may be skipped (`skip_unavailable`): it couldn't be reached or
/// authenticated, or its indices are missing (an index outside what the link may read answers as
/// missing). Anything else is the query's own error, as Elasticsearch fails it (a remote denying
/// `ENRICH` its `monitor_enrich` too).
fn skippable(e: &EsError) -> bool {
    match e.kind.as_ref() {
        "connect_transport_exception" | "index_not_found_exception" => true,
        "verification_exception" => e.reason().starts_with("Unknown index ["),
        "security_exception" => !e.reason().contains(RESOLVE_POLICY),
        _ => matches!(e.status, 502 | 503),
    }
}

/// How many values a JSON text holds at most (its separators and openers outside strings),
/// counted before it's parsed.
fn json_elements(b: &[u8]) -> u64 {
    let (mut n, mut in_str, mut escaped) = (1u64, false, false);
    for &c in b {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else {
            match c {
                b'"' => in_str = true,
                b',' | b'[' | b'{' | b':' => n += 1,
                _ => {}
            }
        }
    }
    n
}

/// Runs query `q` (its source naming linked remotes) as parts on every cluster it names and
/// merges them; `None` when its shape can't be cut into parts (the caller then reads the
/// remotes' rows through cross-cluster search, as before).
pub async fn run(cluster: &crate::cluster::Cluster, ccs: &Ccs, q: &Query, filter: Option<Value>, w: &mut super::value::Warnings) -> EsResult<Option<(Table, super::exec::Stats)>> {
    let started = Instant::now();
    let Some(Cmd::From { sources, metadata, at }) = q.cmds.first() else { return Ok(None) };
    let Some((local_sources, legs)) = ccs.split(sources) else { return Ok(None) };
    // A `METADATA` name that isn't a metadata field is the query's error (Elasticsearch's),
    // before anything is cut from it: a part's text only ever names the closed set of fields.
    if let Some(e) = super::exec::metadata_problems(&q.src, *at, metadata) {
        return Err(e);
    }
    // Only the query's own text (its commands as parsed, maybe the implicit limit after them),
    // with every cluster's columns its mappings' (`SET unmapped_fields` makes them each part's).
    if q.sql || q.spans.is_empty() || q.spans.len() + 1 < q.cmds.len() || q.settings.unmapped != super::parser::Unmapped::Default {
        ccs.check_fallback(&q.src, &q.spans, &q.cmds)?;
        return Ok(None);
    }
    let cut = cut(&q.cmds, q.spans.len());
    let remote_sources: Vec<Vec<String>> = legs.iter().map(|(_, e)| e.clone()).collect();
    let Some(texts) = remote_sources.iter().map(|s| part_text(&q.src, &q.spans, s, metadata, &cut.part, cut.append_limit)).collect::<Option<Vec<String>>>() else {
        ccs.check_fallback(&q.src, &q.spans, &q.cmds)?;
        return Ok(None);
    };
    check_placement(&q.src, &q.spans, &q.cmds)?;
    check_left_here(&q.src, &q.spans, &q.cmds, &cut)?;
    // Each part's text, parsed as its cluster will parse it, must be exactly the commands cut
    // for it (never trusted to be: a remote also decides from what it parsed, [`admit_part`]).
    let part_cmds: Vec<Cmd> = cut.part.iter().map(|&k| q.cmds[k].clone()).collect();
    let mut planned: Vec<Cmd> = part_cmds.clone();
    if let Some(n) = cut.append_limit {
        planned.push(Cmd::Limit(n));
    }
    for (text, exprs) in texts.iter().zip(&remote_sources) {
        let same = super::parser::parse_with(text, &ccs.params).is_ok_and(|p| {
            matches!(p.cmds.first(), Some(Cmd::From { sources: s, metadata: m, .. }) if s == exprs && m == metadata)
                && p.cmds.len() == planned.len() + 1
                && p.cmds[1..].iter().zip(&planned).all(|(a, b)| same_command(a, b))
                && check_shape(&p.cmds, cut.mode).is_ok()
        });
        if !same {
            return Err(EsError::illegal_state("roll couldn't cut this query into parts for its remote clusters"));
        }
    }
    // A caller no `remote_indices` lets reach a remote: a query over remotes alone is refused.
    if local_sources.is_empty()
        && let Some(e) = legs.iter().find_map(|(l, _)| l.refusal.clone())
    {
        return Err(e);
    }

    // Every cluster's columns, as one source of them all (of every cluster that resolves its
    // expressions, when one doesn't).
    let mut groups: Vec<Vec<String>> = Vec::new();
    if !local_sources.is_empty() {
        groups.push(local_sources.clone());
    }
    for (l, exprs) in &legs {
        groups.push(exprs.iter().map(|e| format!("{}:{e}", l.remote.alias)).collect());
    }
    let (schema, complete) = super::exec::source_schema(cluster, &q.src, sources, &groups, *at).await?;
    let text = q.src.clone();
    let res = super::exec::load_resources(cluster, &text, &q.cmds[1..]).await?;
    let tz = q.settings.time_zone.as_deref().and_then(|z| crate::time::Zone::parse(z).ok()).filter(|z| *z != crate::time::Zone::UTC);
    let src = super::exec::Src { text: &text, sql: false, locale: q.locale.as_deref(), res: &res, tz };
    // The parts' commands, compiled here first: the query's own errors (an unknown column, a
    // bad aggregation) are the query's, before any cluster runs it. `expected`: the columns the
    // parts' row-by-row commands make. Only over every cluster's columns: a schema of the local
    // ones alone (a remote's field capabilities failed) would refuse the remotes' fields.
    let pre = part_cmds.iter().take_while(|c| runs_in_parts(c)).count();
    let verified = if complete { super::exec::verify_part(&src, &schema, metadata, &part_cmds, pre)? } else { None };
    // A partial `STATS`'s input: the columns it reads ([`super::exec::stats_reads`]).
    let expected_input: Option<Vec<Col>> = match (&cut.stats, &verified) {
        (Some(st), Some(v)) => Some(super::exec::stats_reads(st, &v.prefix).into_iter().map(|i| v.prefix[i].clone()).collect()),
        _ => None,
    };
    // A rows part's answer: its columns, then the carried sort keys.
    let expected_rows: Option<Vec<Col>> = verified.as_ref().filter(|_| cut.mode == Mode::Rows).map(|v| {
        let mut cols: Vec<Col> = v.out.iter().filter(|c| !c.name.starts_with(super::fulltext::HIDDEN)).cloned().collect();
        if cut.sort_keys {
            cols.extend(v.sort_types.iter().enumerate().map(|(k, ty)| Col { name: format!("{SORT_CARRY}{k}"), ty: *ty }));
        }
        cols
    });

    // Enrich policies: `_any` must be here and on every part's cluster, `_remote` on every
    // part's cluster, `_coordinator` here (as one cluster's query checks it).
    let used = policies(&q.cmds);
    let mut missing: Vec<(String, usize, Vec<String>)> = Vec::new();
    let mut note = |name: &str, at: usize, cluster_name: &str| match missing.iter_mut().find(|(n, _, _)| n == name) {
        Some((_, _, cs)) => {
            if !cs.iter().any(|c| c == cluster_name) {
                cs.push(cluster_name.to_string());
            }
        }
        None => missing.push((name.to_string(), at, vec![cluster_name.to_string()])),
    };
    {
        let st = cluster.state();
        for (name, mode, at) in &used {
            if (mode == "_any" || (mode == "_remote" && !local_sources.is_empty())) && st.metadata.xpack.get(crate::xpack::enrich::POLICIES, name).is_none() {
                note(name, *at, "_local");
            }
        }
    }
    let remote_policies: Vec<String> = {
        let mut v: Vec<String> = used.iter().filter(|(_, m, _)| m != "_coordinator").map(|(n, _, _)| n.clone()).collect();
        v.sort();
        v.dedup();
        v
    };

    let schema_cols: Vec<(String, DataType)> = schema.iter().map(|c| (c.name.clone(), c.ty)).collect();
    // A rows part without a limit answers at most its share of what this cluster can hold.
    let max_bytes = (!cut.bounded).then(|| {
        let b = cluster.breakers.request(cluster, &cluster.state());
        b.limit().saturating_sub(b.used()) / legs.len().max(1) as u64
    });
    // A lookup index's field the clusters map with different types is a union type: its use
    // after the join is Elasticsearch's 400, before any part runs (`lk`, `far:lk`).
    for (k, c) in part_cmds.iter().enumerate() {
        let Cmd::LookupJoin { index, .. } = c else { continue };
        let mut named: Vec<String> = Vec::new();
        if !local_sources.is_empty() {
            named.push(index.clone());
        }
        named.extend(legs.iter().filter(|(l, _)| l.refusal.is_none()).map(|(l, _)| format!("{}:{index}", l.remote.alias)));
        let mut by_field: std::collections::HashMap<String, super::exec::UnionTypes> = std::collections::HashMap::new();
        for n in &named {
            // One that doesn't resolve is its part's to report.
            let Ok(cols) = super::exec::index_fields(cluster, &q.src, n).await else { continue };
            for col in cols {
                let types = by_field.entry(col.name).or_default();
                match types.iter_mut().find(|(t, _)| t == col.ty.name()) {
                    Some((_, at)) => at.push(n.clone()),
                    None => types.push((col.ty.name().to_string(), vec![n.clone()])),
                }
            }
        }
        by_field.retain(|_, types| types.len() > 1);
        by_field.values_mut().for_each(|types| types.sort_by(|a, b| super::exec::es_type_name(&a.0).cmp(super::exec::es_type_name(&b.0))));
        let after: Vec<Cmd> = part_cmds[k + 1..].iter().chain(&cut.rest).cloned().collect();
        if let Some((name, at)) = super::exec::first_use(&after, &by_field) {
            return Err(super::exec::ambiguous_at(&q.src, at, name, &by_field[name]));
        }
    }
    let mut sent: Vec<Leg> = Vec::new();
    let mut refused: Vec<(String, bool, String, EsError)> = Vec::new();
    for ((l, exprs), text) in legs.iter().zip(&texts) {
        let indices = exprs.join(",");
        if l.refusal.is_some() {
            // With local indices too, Elasticsearch skips such a remote as one without them.
            let named: Vec<String> = exprs.iter().map(|e| format!("{}:{e}", l.remote.alias)).collect();
            refused.push((l.remote.alias.clone(), l.remote.skip_unavailable, indices, EsError::new(400, "verification_exception", format!("Unknown index [{}]", named.join(",")))));
            continue;
        }
        let part = Part { alias: Some(l.remote.alias.clone()), mode: cut.mode, schema: schema_cols.clone(), policies: remote_policies.clone(), sort_keys: cut.sort_keys, max_bytes };
        let req = part_request(&l.template, &ccs.forwarded, text, &part);
        let (links, local, alias, target, wait) = (ccs.links.clone(), ccs.local.clone(), l.remote.alias.clone(), l.target.clone(), ccs.wait);
        let handle = tokio::spawn(async move {
            match tokio::time::timeout(wait, links.send(&local, &alias, &target, req)).await {
                Err(_) => Err(EsError::new(502, "connect_transport_exception", format!("Unable to connect to [{alias}]: no answer within [{}ms]", wait.as_millis()))),
                Ok(Err(e)) => Err(e.with_status(502)),
                Ok(Ok(r)) => Ok(r),
            }
        });
        sent.push(Leg { alias: l.remote.alias.clone(), skip_unavailable: l.remote.skip_unavailable, indices, handle });
    }

    // The local part, while the remotes run theirs (dropped legs abort if it fails).
    let local = if local_sources.is_empty() {
        None
    } else {
        let mut cmds = vec![Cmd::From { sources: local_sources.clone(), metadata: metadata.clone(), at: *at }];
        cmds.extend(part_cmds.iter().cloned());
        if let Some(n) = cut.append_limit {
            cmds.push(Cmd::Limit(n));
        }
        let lq = Query { src: q.src.clone(), cmds, sql: false, locale: q.locale.clone(), settings: q.settings.clone(), spans: Vec::new() };
        let part = Arc::new(Part { alias: None, mode: cut.mode, schema: schema_cols.clone(), policies: Vec::new(), sort_keys: cut.sort_keys, max_bytes: None });
        let (t, s) = PART.scope(part, super::exec::execute_part(cluster, lq, filter.clone(), w)).await?;
        // Defensive: the cut only makes `STATS` parts the executor answers with states.
        if cut.mode == Mode::Stats && s.partial.is_none() {
            return Err(EsError::illegal_state("the local cluster's part answered STATS without its partial states"));
        }
        Some((t, s))
    };

    // The remotes' answers, held on the request breaker before they're read: their bytes and,
    // per value they may hold, what it costs parsed and decoded.
    let mut held = crate::breaker::Reservation::new(cluster.breakers.request(cluster, &cluster.state()));
    let mut outcomes: Outcomes = Vec::new();
    let fail = |alias: &str, skip: bool, e: EsError, indices: &str, outcomes: &mut Outcomes| -> EsResult<()> {
        if !skip {
            return Err(e);
        }
        outcomes.push((alias.to_string(), indices.to_string(), Outcome::Skipped(e)));
        Ok(())
    };
    for (alias, skip, indices, e) in refused {
        fail(&alias, skip, e, &indices, &mut outcomes)?;
    }
    for mut leg in sent {
        let resp = match (&mut leg.handle).await {
            Ok(r) => r,
            Err(_) => Err(EsError::new(502, "connect_transport_exception", format!("Unable to connect to [{}]", leg.alias))),
        };
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                fail(&leg.alias, leg.skip_unavailable, e, &leg.indices, &mut outcomes)?;
                continue;
            }
        };
        held.add((resp.body.len() as u64).saturating_add(json_elements(&resp.body).saturating_mul(PER_ELEMENT)), "<remote_cluster_response>")?;
        let v = crate::json::parse(&resp.body).ok().flatten().unwrap_or(Value::Null);
        if resp.status >= 300 {
            let e = links::error_of(resp.status, &v);
            if e.kind == MISSING_POLICY {
                for name in e.reason().split(',') {
                    let Some((_, _, at)) = used.iter().find(|(n, m, _)| n == name && m != "_coordinator") else {
                        return Err(EsError::illegal_state(format!("remote cluster [{}] reported missing an enrich policy the query doesn't use", leg.alias)));
                    };
                    note(name, *at, &leg.alias);
                }
                continue;
            }
            if !skippable(&e) {
                return Err(e);
            }
            fail(&leg.alias, leg.skip_unavailable, e, &leg.indices, &mut outcomes)?;
            continue;
        }
        for (k, val) in &resp.headers {
            if k.eq_ignore_ascii_case("warning")
                && let Some(msg) = warning_text(val)
            {
                w.add(msg);
            }
        }
        match decode_answer(&v, cut.mode, schema.len() + metadata.len() + MAX_RESOURCE_COLUMNS) {
            Ok(a) => outcomes.push((leg.alias.clone(), leg.indices.clone(), Outcome::Done(a))),
            Err(why) => return Err(unreadable(&leg.alias, &why)),
        }
    }
    if let Some((name, at, mut cs)) = missing.into_iter().next() {
        cs.sort_by(|a, b| (a != "_local", a).cmp(&(b != "_local", b)));
        return Err(super::exec::verification_at(&q.src, at, &format!("cannot find enrich policy [{name}] on clusters [{}]", cs.join(", "))));
    }
    // A policy's enrich fields are the same on every cluster that runs it, as Elasticsearch
    // requires; then a part's columns are the ones planned for it, but for the types of what its
    // cluster's own policies and lookup indices give it.
    for (name, mode, at) in used.iter().filter(|(_, m, _)| m != "_coordinator") {
        let mut sets: Vec<Vec<String>> = Vec::new();
        if (mode == "_any" || !local_sources.is_empty())
            && let Some(f) = super::exec::policy_fields(&cluster.state(), name)
        {
            sets.push(f);
        }
        for (alias, _, o) in &outcomes {
            if let Outcome::Done(a) = o {
                let Some((_, f)) = a.enrich.iter().find(|(n, _)| n == name) else { return Err(unreadable(alias, &format!("no enrich fields of policy [{name}]"))) };
                sets.push(f.clone());
            }
        }
        let mut differ: Vec<&str> = sets.iter().flatten().filter(|f| !sets.iter().all(|s| s.contains(*f))).map(String::as_str).collect();
        differ.sort_unstable();
        differ.dedup();
        if !differ.is_empty() {
            return Err(super::exec::verification_at(&q.src, *at, &format!("enrich policy [{name}] has different enrich fields across clusters; these fields are missing in some policies: [{}]", differ.join(", "))));
        }
    }
    let varies = varies(&part_cmds, &res);
    for (alias, _, o) in &outcomes {
        if let Outcome::Done(a) = o {
            planned_columns(a, &cut, &part_cmds, expected_rows.as_deref(), &varies).map_err(|why| unreadable(alias, &why))?;
        }
    }

    // Merged: the local part first, then the remotes by alias.
    let mut stats = super::exec::Stats::default();
    let mut infos: Vec<ClusterInfo> = Vec::new();
    // Each part's rows, labelled with where its own resources came from (`lk`, `far:lk`).
    let resource = part_cmds
        .iter()
        .rev()
        .find_map(|c| match c {
            Cmd::LookupJoin { index, .. } => Some(index.clone()),
            Cmd::Enrich { policy, .. } => Some(super::parser::split_policy(policy).1.to_string()),
            _ => None,
        })
        .unwrap_or_default();
    let mut tables: Vec<(Table, String)> = Vec::new();
    let mut partials: Vec<Partial> = Vec::new();
    if let Some((t, s)) = local {
        infos.push(ClusterInfo { name: "(local)".into(), indices: local_sources.join(","), status: "successful", took: None, shards: shards_of(cluster, s.indices.as_deref()), failures: Vec::new() });
        stats.documents_found = stats.documents_found.saturating_add(s.documents_found);
        stats.values_loaded = stats.values_loaded.saturating_add(s.values_loaded);
        stats.indices = s.indices.clone();
        stats.fields = s.fields.clone();
        stats.original_types = s.original_types.clone();
        stats.col_meta = s.col_meta.clone();
        match s.partial {
            Some(p) => partials.push(p),
            None => tables.push((t, resource.clone())),
        }
    }
    for (alias, indices, o) in outcomes {
        match o {
            Outcome::Done(a) => {
                let label = format!("{alias}:{resource}");
                infos.push(ClusterInfo { name: alias, indices, status: "successful", took: Some(a.took), shards: a.shards, failures: Vec::new() });
                stats.documents_found = stats.documents_found.saturating_add(a.documents_found);
                stats.values_loaded = stats.values_loaded.saturating_add(a.values_loaded);
                match a.partial {
                    Some(p) => partials.push(p),
                    None => tables.push((a.table, label)),
                }
            }
            Outcome::Skipped(e) => infos.push(ClusterInfo {
                name: alias,
                indices,
                status: "skipped",
                took: Some(started.elapsed().as_millis() as i64),
                shards: 0,
                failures: vec![obj! { "shard" => -1, "index" => Value::Null, "reason" => e.to_value(false) }],
            }),
        }
    }
    let remotes_from = usize::from(infos.first().is_some_and(|i| i.name == "(local)"));
    infos[remotes_from..].sort_by(|a, b| a.name.cmp(&b.name));

    // The columns the parts make when none answered (every one skipped).
    let empty_cols = || -> EsResult<Vec<Col>> {
        if let Some(c) = &expected_rows {
            return Ok(c.clone());
        }
        let mut cols = super::exec::part_columns(&src, &schema, metadata, &part_cmds[..pre])?;
        if cut.sort_keys {
            cols.extend((0..part_cmds.iter().find_map(|c| if let Cmd::Sort(k) = c { Some(k.len()) } else { None }).unwrap_or(0)).map(|k| Col { name: format!("{SORT_CARRY}{k}"), ty: DataType::Null }));
        }
        Ok(cols)
    };
    let table = match &cut.stats {
        Some(Cmd::Stats { aggs, by }) => {
            // The final reduce: every part's states into one grouper over the query's columns,
            // each part's exactly those (as the coordinating cluster compiled them, else the
            // first part's when it can't: a policy only the remotes have).
            let input = match (&expected_input, partials.first()) {
                (Some(e), _) => e.clone(),
                (None, Some(p)) => p.input.clone(),
                (None, None) => empty_cols()?,
            };
            let mut g = super::exec::Grouper::new(&src, aggs, by, &input)?;
            for p in partials {
                if p.input.len() != input.len() || p.input.iter().zip(&input).any(|(a, b)| a.name != b.name || a.ty != b.ty) {
                    // Types its cluster's own policy or lookup index gave it: the query's error.
                    if let Some((a, b)) = p.input.iter().zip(&input).find(|(a, b)| a.ty != b.ty)
                        && p.input.len() == input.len()
                        && p.input.iter().zip(&input).all(|(a, b)| a.name == b.name)
                        && varies.allows(&a.name)
                    {
                        return Err(EsError::new(400, "verification_exception", format!("Column [{}] is [{}] on one cluster and [{}] on another, from their own enrich policies or lookup indices: roll can't merge STATS over it", a.name, b.ty.name(), a.ty.name())));
                    }
                    return Err(EsError::illegal_state("a cluster answered STATS over other columns than the query's: roll can't merge them"));
                }
                held.add((p.groups.len() as u64).saturating_mul(64), "<remote_cluster_response>")?;
                for (key, accs) in p.groups {
                    g.merge_group(&key, accs).map_err(|why| EsError::illegal_state(format!("a cluster's STATS state roll can't merge: {why}")))?;
                }
            }
            g.finish(w)?
        }
        _ => {
            if tables.is_empty() {
                Table { cols: empty_cols()?, rows: Vec::new() }
            } else {
                let mut original = std::mem::take(&mut stats.original_types);
                let (t, unions) = super::exec::union_parts(tables, &mut original, &|n| varies.allows(n))?;
                stats.original_types = original;
                // A sort key the clusters' own resources typed differently can't order the rows.
                if let Some(Cmd::Sort(keys)) = part_cmds.iter().find(|c| matches!(c, Cmd::Sort(_)))
                    && let Some((key, u)) = keys.iter().enumerate().find_map(|(k, key)| unions.get(&format!("{SORT_CARRY}{k}")).map(|u| (key, u)))
                {
                    let (from, to) = (key.expr.start, key.expr.end);
                    return Err(super::exec::ambiguous_at(&q.src, from, q.src.get(from..to).unwrap_or_default(), u));
                }
                // The commands after the merge may only convert such a column ([`super::exec::Resources::set_unions`]).
                src.res.set_unions(unions);
                t
            }
        }
    };
    let t = super::exec::run_rest(&src, table, &cut.rest, w)?;
    stats.rows_emitted = t.rows.len() as i64;
    stats.col_meta.extend(std::mem::take(&mut *src.res.col_meta.lock()));
    stats.approx = std::mem::take(&mut *src.res.approx.lock());
    stats.clusters = Some(infos);
    drop(held);
    Ok(Some((t, stats)))
}

/// Whether `a` (a part's command as its cluster parsed the text) is `b` (as cut here): the
/// same command, its `ENRICH` mode and `LIMIT` the same.
fn same_command(a: &Cmd, b: &Cmd) -> bool {
    match (a, b) {
        (Cmd::Enrich { policy: x, .. }, Cmd::Enrich { policy: y, .. }) => x == y,
        (Cmd::Limit(x), Cmd::Limit(y)) => x == y,
        _ => std::mem::discriminant(a) == std::mem::discriminant(b),
    }
}

/// What a part's answer may hold other than the columns this cluster compiled for it, because
/// its commands read its own cluster's policies and lookup indices.
enum Varies {
    Nothing,
    /// These columns' types: an `ENRICH`'s fields (the same on every cluster, checked first) and
    /// what's computed from them, each cluster's policy index mapping them its own way.
    Types(std::collections::HashSet<String>),
    /// Any column: a `LOOKUP JOIN` adds its lookup index's fields as that cluster maps them.
    Anything,
}

impl Varies {
    fn allows(&self, name: &str) -> bool {
        match self {
            Varies::Nothing => false,
            Varies::Types(names) => names.contains(name),
            Varies::Anything => true,
        }
    }
}

/// Which of a part's columns its cluster's own resources decide ([`Varies`]); `res`: this
/// cluster's, for the fields of a policy an `ENRICH` adds without `WITH`.
fn varies(part_cmds: &[Cmd], res: &super::exec::Resources) -> Varies {
    let mut names = std::collections::HashSet::new();
    let mut enriched = false;
    let reads = |e: &super::parser::Expr, names: &std::collections::HashSet<String>| {
        let mut r = std::collections::HashSet::new();
        super::exec::refs(e, &mut r);
        r.iter().any(|n| names.contains(n))
    };
    for c in part_cmds {
        match c {
            Cmd::LookupJoin { .. } => return Varies::Anything,
            Cmd::Enrich { policy, with, .. } => {
                enriched = true;
                match with {
                    Some(w) => names.extend(w.iter().map(|(n, ..)| n.clone())),
                    None => names.extend(res.enrich_columns(super::parser::split_policy(policy).1).unwrap_or_default().iter().map(|c| c.name.clone())),
                }
            }
            Cmd::Eval(list) => {
                for n in list {
                    if reads(&n.expr, &names) {
                        names.insert(n.name.clone());
                    } else {
                        names.remove(&n.name);
                    }
                }
            }
            Cmd::Rename(list) => {
                for (from, to, _) in list {
                    if names.remove(from) {
                        names.insert(to.clone());
                    } else {
                        names.remove(to);
                    }
                }
            }
            Cmd::Sort(keys) => {
                let carried: Vec<String> = keys.iter().enumerate().filter(|(_, k)| reads(&k.expr, &names)).map(|(k, _)| format!("{SORT_CARRY}{k}")).collect();
                names.extend(carried);
            }
            _ => {}
        }
    }
    if enriched { Varies::Types(names) } else { Varies::Nothing }
}

/// A remote's part answer roll can't read: hostile, or another version's.
fn unreadable(alias: &str, why: &str) -> EsError {
    EsError::illegal_state(format!("remote cluster [{alias}] answered a part roll can't read: {why}"))
}

/// A rows part's answer has the columns planned for it: exactly `expected` (names and types,
/// the carried sort keys last) when this cluster compiled the part's commands, but for the types
/// [`Varies`] lets its cluster decide (a `LOOKUP JOIN`: any columns); else at least the carried
/// sort keys. Full-text columns are never answered. Differing types merge as a union type.
fn planned_columns(a: &Answer, cut: &Cut, part_cmds: &[Cmd], expected: Option<&[Col]>, varies: &Varies) -> Result<(), String> {
    if cut.mode != Mode::Rows {
        return Ok(());
    }
    let cols = &a.table.cols;
    if cols.iter().any(|c| c.name.starts_with(super::fulltext::HIDDEN)) {
        return Err("a full-text column".into());
    }
    if let Some(expected) = expected.filter(|_| !matches!(varies, Varies::Anything)) {
        if cols.len() != expected.len() || cols.iter().zip(expected).any(|(c, e)| c.name != e.name || (c.ty != e.ty && !varies.allows(&c.name))) {
            return Err("columns other than the query's".into());
        }
        return Ok(());
    }
    if cut.sort_keys {
        let keys = part_cmds.iter().find_map(|c| if let Cmd::Sort(k) = c { Some(k.len()) } else { None }).unwrap_or(0);
        if (0..keys).any(|k| !cols.iter().any(|c| c.name == format!("{SORT_CARRY}{k}"))) {
            return Err("no sort keys".into());
        }
    }
    Ok(())
}

/// The text of a `Warning` header (`299 Elasticsearch-... "text"`).
fn warning_text(h: &str) -> Option<String> {
    let start = h.find('"')?;
    let end = h.rfind('"')?;
    (end > start).then(|| h[start + 1..end].replace("\\\"", "\"").replace("\\\\", "\\"))
}

/// `FROM` expressions naming remote clusters that aren't configured: Elasticsearch's 404.
pub fn check_aliases(st: &crate::meta::ClusterState, sources: &[String]) -> EsResult<()> {
    let configured = crate::remote::remotes(st);
    for e in sources {
        let Some((pattern, _)) = crate::remote::split_expression(e.trim_start_matches('-')) else { continue };
        if !pattern.contains('*') && !configured.iter().any(|r| r.alias == pattern) {
            return Err(EsError::new(404, "no_such_remote_cluster_exception", format!("no such remote cluster: [{pattern}]")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(q: &str) -> Query {
        super::super::parser::parse(q, &[V::Long(1)], &[]).unwrap()
    }

    /// A remote's part keeps every position of the query: its `FROM` names the remote's own
    /// indices, the commands it doesn't run are blanked, and what runs after the breaker is gone.
    #[test]
    fn parts_keep_the_querys_positions() {
        let q = parse("FROM items,far:items METADATA _index | WHERE n > 1 | SORT n | EVAL x = 1 | LIMIT 5 | ENRICH _remote:p | STATS c = COUNT(*)");
        let c = cut(&q.cmds, q.spans.len());
        // Commands between `SORT` and `LIMIT`, and the `ENRICH _remote` after: all in the parts,
        // which carry the sort keys; here the rows sort on those, then `STATS`.
        assert_eq!((c.mode, c.part.clone(), c.append_limit, c.sort_keys, c.rest.len()), (Mode::Rows, vec![1, 2, 3, 4, 5], None, true, 4), "{c:?}");
        let q = parse("FROM items,far:items METADATA _index | WHERE n > 1 | SORT n | LIMIT 5 | ENRICH _remote:p | STATS c = COUNT(*)");
        let c = cut(&q.cmds, q.spans.len());
        assert_eq!((c.mode, c.part.as_slice()), (Mode::Rows, [1, 2, 3, 4].as_slice()));
        // The `ENRICH _remote` the parts run after their limit may overwrite `n`: they carry it.
        assert!(c.sort_keys && matches!(c.rest.as_slice(), [Cmd::Sort(k), Cmd::Limit(5), Cmd::Drop(_), Cmd::Stats { .. }] if matches!(&k[0].expr.kind, super::super::parser::ExprKind::Col(n) if n == "$ccs_sort0")), "{:?}", c.rest);
        let t = part_text(&q.src, &q.spans, &["items".into()], &["_index".into()], &c.part, None).unwrap();
        assert_eq!(t, "FROM \"items\" METADATA _index         | WHERE n > 1 | SORT n | LIMIT 5 | ENRICH _remote:p");
        assert_eq!(t.find("ENRICH"), q.src.find("ENRICH"));

        let q = parse("FROM far:items\n| WHERE n > ?\n| STATS c = COUNT(*) BY tag\n| SORT tag");
        let c = cut(&q.cmds, q.spans.len());
        assert_eq!((c.mode, c.part.as_slice(), c.rest.len()), (Mode::Stats, [1, 2].as_slice(), 1));
        let t = part_text(&q.src, &q.spans, &["items".into()], &[], &c.part, None).unwrap();
        assert_eq!(t, "FROM \"items\"  \n| WHERE n > ?\n| STATS c = COUNT(*) BY tag");

        // The implicit limit: appended to the parts, kept here.
        let mut q = parse("FROM far:items | EVAL a = 1");
        q.cmds.push(Cmd::Limit(1000));
        let c = cut(&q.cmds, q.spans.len());
        assert_eq!((c.part.clone(), c.append_limit), (vec![1], Some(1000)));
        assert_eq!(part_text(&q.src, &q.spans, &["items".into()], &[], &c.part, c.append_limit).unwrap(), "FROM \"items\"   | EVAL a = 1 | LIMIT 1000");

        // Cluster patterns are one source.
        let q = parse("FROM *:logs-*,items | LIMIT 1");
        assert!(matches!(&q.cmds[0], Cmd::From { sources, .. } if sources == &["*:logs-*", "items"]), "{:?}", q.cmds[0]);
        let q = parse("FROM far:*,-far:many,far:-x | LIMIT 1");
        assert!(matches!(&q.cmds[0], Cmd::From { sources, .. } if sources == &["far:*", "-far:many", "far:-x"]), "{:?}", q.cmds[0]);

        // `ENRICH _coordinator` and what follows run here, over the parts' rows.
        // The limit commutes with it, so the parts take it too.
        let q = parse("FROM far:items | WHERE a | ENRICH _coordinator:p | LIMIT 3");
        let c = cut(&q.cmds, q.spans.len());
        assert_eq!((c.mode, c.part.as_slice(), c.append_limit, c.bounded, c.rest.len()), (Mode::Rows, [1].as_slice(), Some(3), true, 2));
        // Nothing to bound the parts with: they're capped instead.
        let q = parse("FROM far:items | INLINE STATS m = MAX(n) | LIMIT 3");
        let c = cut(&q.cmds, q.spans.len());
        assert_eq!((c.append_limit, c.bounded), (None, false));
        // A `LIMIT` and the commands after it up to an `ENRICH _remote`.
        let q = parse("FROM far:items | LIMIT 3 | EVAL x = 1 | ENRICH _remote:p | KEEP x | ENRICH p");
        let c = cut(&q.cmds, q.spans.len());
        assert_eq!((c.part.as_slice(), c.rest.len()), ([1, 2, 3].as_slice(), 3));
        assert!(check_shape(&parse("FROM items | LIMIT 3 | EVAL x = 1 | ENRICH _remote:p").cmds, Mode::Rows).is_ok());
    }

    /// A command that reads a cluster's own resources never runs on the coordinating cluster:
    /// in the parts, or refused with Elasticsearch's error.
    #[test]
    fn resources_of_each_cluster_stay_in_its_part() {
        // `SORT | KEEP | LIMIT | ENRICH _remote` runs in each part.
        let q = parse("FROM far:items | SORT n | KEEP n, tag | LIMIT 5 | ENRICH _remote:p");
        let c = cut(&q.cmds, q.spans.len());
        assert_eq!((c.part.as_slice(), c.sort_keys), ([1, 2, 3, 4].as_slice(), true));
        assert!(check_left_here(&q.src, &q.spans, &q.cmds, &c).is_ok());
        assert!(check_shape(&parse("FROM items | SORT n | KEEP n, tag | LIMIT 5 | ENRICH _remote:p").cmds, Mode::Rows).is_ok());
        for (q, reason) in [
            ("FROM far:items | ENRICH _coordinator:p | LOOKUP JOIN lk ON tag", "Found 1 problem\nline 1:42: LOOKUP JOIN with remote indices can't be executed after [ENRICH _coordinator:p]@1:18"),
            ("FROM far:items | INLINE STATS m = MAX(n) | ENRICH _remote:p | KEEP n, m, label", "Found 1 problem\nline 1:44: ENRICH with remote policy can't be executed after [INLINE STATS m = MAX(n)]@1:18"),
            ("FROM far:items | SORT n DESC | LOOKUP JOIN lk ON tag | KEEP n, lab | LIMIT 2", "Found 1 problem\nline 1:32: LOOKUP JOIN with remote indices can't be executed after [SORT n DESC]@1:18"),
        ] {
            let q = parse(q);
            assert_eq!(check_placement(&q.src, &q.spans, &q.cmds).unwrap_err().reason(), reason);
        }
        // Whatever escapes the placement rules is refused rather than run here, naming the
        // command the parts stopped at as Elasticsearch does (a top-n by its `SORT`).
        for (q, reason) in [
            ("FROM far:items | INLINE STATS m = MAX(n) | ENRICH p | KEEP n, m, label", "Found 1 problem\nline 1:44: ENRICH with remote policy can't be executed after [INLINE STATS m = MAX(n)]@1:18"),
            ("FROM far:items | SORT n | LIMIT 5 | WHERE n > 1 | ENRICH _remote:p", "Found 1 problem\nline 1:51: ENRICH with remote policy can't be executed after [SORT n]@1:18"),
            ("FROM far:items | SORT n | LIMIT 5 | MV_EXPAND tag | ENRICH _remote:p", "Found 1 problem\nline 1:53: MV_EXPAND after LIMIT is incompatible with remote ENRICH"),
        ] {
            let q = parse(q);
            let c = cut(&q.cmds, q.spans.len());
            assert_eq!(check_left_here(&q.src, &q.spans, &q.cmds, &c).unwrap_err().reason(), reason);
        }
        let q = parse("FROM far:items | LIMIT 2 | ENRICH p");
        let c = cut(&q.cmds, q.spans.len());
        assert!(check_left_here(&q.src, &q.spans, &q.cmds, &c).is_ok(), "after a LIMIT `_any` is the coordinator's");
    }

    /// A remote runs only the shapes the coordinating cluster cuts, as it parsed them: a STATS
    /// part ends in exactly one `STATS`, a rows part in at most `LIMIT`/`SORT | LIMIT` and the
    /// `ENRICH _remote`s after them, its source this cluster's own; and a quoted `METADATA` name
    /// (one that could hold a `|`) is never a metadata field.
    #[test]
    fn a_remote_runs_only_the_parts_shapes() {
        let shape = |q: &str, mode| check_shape(&parse(q).cmds, mode);
        assert!(shape("FROM items | WHERE n > 1 | STATS c = COUNT(*)", Mode::Stats).is_ok());
        assert!(shape("FROM items | STATS c = COUNT(*) | LIMIT 1", Mode::Stats).is_err());
        assert!(shape("FROM items | LIMIT 1", Mode::Stats).is_err());
        assert!(shape("FROM items METADATA _index | ENRICH _remote:p | LIMIT 5", Mode::Rows).is_ok());
        assert!(shape("FROM items | SORT n | LIMIT 2 | ENRICH _remote:p", Mode::Rows).is_ok());
        assert!(shape("FROM items | LIMIT 2 | EVAL x = 1", Mode::Rows).is_err());
        assert!(shape("FROM items | ENRICH _coordinator:p | LIMIT 1", Mode::Rows).is_err());
        assert!(shape("FROM far:items | LIMIT 1", Mode::Rows).is_err());
        let q = parse("FROM far:items METADATA `_index | ENRICH _remote:tag_label ON tag WITH tag = label` | LIMIT 5");
        let Cmd::From { metadata, at, .. } = &q.cmds[0] else { unreachable!() };
        let e = super::super::exec::metadata_problems(&q.src, *at, metadata).unwrap();
        assert!(e.reason().starts_with("Found 2 problems\nline 1:1: unresolved metadata fields: [?`_index | ENRICH"), "{}", e.reason());
    }

    /// Elasticsearch's placement errors for commands reading each cluster's own resources.
    #[test]
    fn remote_enrich_and_lookup_after_a_breaker_are_refused() {
        let q = parse("FROM items,far:items | STATS c=COUNT(*) BY tag | ENRICH _remote:tag_label | SORT tag");
        let e = check_placement(&q.src, &q.spans, &q.cmds).unwrap_err();
        assert_eq!(e.reason(), "Found 1 problem\nline 1:50: ENRICH with remote policy can't be executed after [STATS c=COUNT(*) BY tag]@1:24");
        let q = parse("FROM items,far:items | ENRICH _coordinator:tag_label | ENRICH _remote:tag_label | LIMIT 1");
        assert_eq!(check_placement(&q.src, &q.spans, &q.cmds).unwrap_err().reason(), "Found 1 problem\nline 1:56: ENRICH with remote policy can't be executed after [ENRICH _coordinator:tag_label]@1:24");
        let q = parse("FROM items,far:items | STATS c=COUNT(*) BY tag | LOOKUP JOIN lk ON tag | SORT tag");
        assert_eq!(check_placement(&q.src, &q.spans, &q.cmds).unwrap_err().reason(), "Found 1 problem\nline 1:50: LOOKUP JOIN with remote indices can't be executed after [STATS c=COUNT(*) BY tag]@1:24");
        let q = parse("FROM items,far:items | SORT n | LIMIT 2 | LOOKUP JOIN lk ON tag | KEEP n, lab");
        assert_eq!(check_placement(&q.src, &q.spans, &q.cmds).unwrap_err().reason(), "Found 1 problem\nline 1:43: LOOKUP JOIN with remote indices can't be executed after [LIMIT 2]@1:33");
        let q = parse("FROM items,far:items | SORT n | LIMIT 2 | ENRICH _remote:tag_label");
        assert!(check_placement(&q.src, &q.spans, &q.cmds).is_ok());
        // The mode is read in any case; anything else is Elasticsearch's parse error.
        let q = parse("FROM items,far:items | ENRICH _REMOTE:tag_label | ENRICH _Coordinator:p");
        assert!(matches!(&q.cmds[1..], [Cmd::Enrich { policy: a, .. }, Cmd::Enrich { policy: b, .. }] if a == "_remote:tag_label" && b == "_coordinator:p"), "{:?}", q.cmds);
        let e = super::super::parser::parse("FROM items,far:items | ENRICH foo:p", &[], &[]).unwrap_err();
        assert_eq!((e.kind.as_ref(), e.reason()), ("parsing_exception", "line 1:31: Unrecognized value [foo], ENRICH policy qualifier needs to be one of [_ANY, _COORDINATOR, _REMOTE]"));
    }

    /// Values and states round-trip exactly; a crafted answer (nested multi-values, an unknown
    /// tag, a row of another width, a sketch no node makes) is refused, never trusted.
    #[test]
    fn answers_round_trip_and_crafted_ones_are_refused() {
        let vs = [V::Null, V::Bool(true), V::Int(-3), V::Long(i64::MIN), V::ULong(u64::MAX), V::Double(-0.0), V::Double(0.1), V::Str("x".into()), V::Date(5), V::Ip("::1".into()), V::Multi(vec![V::Long(1), V::Str("a".into())])];
        for v in &vs {
            let back = decode_v(&encode_v(v), true).unwrap();
            assert_eq!(format!("{back:?}"), format!("{v:?}"));
        }
        let mut hll = search_aggs::metrics::cardinality::Hll::new(14);
        (0..5000u64).for_each(|i| hll.collect(i.wrapping_mul(0x9e37_79b9_7f4a_7c15)));
        let accs = [
            Acc::Count(7),
            Acc::Sum { int: 3, float: 0.5, comp: 1e-20, any: true, overflow: false },
            Acc::Avg { int: i128::from(i64::MAX) * 3, float: 1.5, comp: 0.0, n: 9 },
            Acc::MinMax(Some(V::Date(3))),
            Acc::Collect(vec![V::Double(1.0), V::Multi(vec![V::Int(1)])]),
            Acc::Distinct { hll: Box::new(hll), ty: DataType::Keyword },
            Acc::Amd { part: [Some(1.0), None, Some(3.0), Some(2.0)] },
            Acc::Points(vec![(1, V::Long(2))]),
            Acc::Weighted(1.0, 2.0),
            Acc::Present(2, true),
        ];
        for a in &accs {
            let back = decode_acc(&encode_acc(a)).unwrap();
            assert_eq!(format!("{back:?}"), format!("{a:?}"));
        }
        let nested = crate::arr!["m", crate::arr![crate::arr!["m", crate::arr![1]]]];
        assert!(decode_v(&nested, true).is_err());
        assert!(decode_v(&crate::arr!["z", 1], true).is_err());
        assert!(decode_acc(&crate::arr!["hll", "keyword", 14, crate::arr![i64::from(u32::MAX - 1)], Value::Null]).is_err());
        let wide = obj! { PART_FIELD => obj! { "columns" => crate::arr![crate::arr!["a", "long"]], "rows" => crate::arr![crate::arr![1, 2]] } };
        assert!(decode_answer(&wide, Mode::Rows, 10).is_err());
        let bad_type = obj! { PART_FIELD => obj! { "columns" => crate::arr![crate::arr!["a", "nope"]], "rows" => crate::arr![] } };
        assert!(decode_answer(&bad_type, Mode::Rows, 10).is_err());
        let text_in_long = obj! { PART_FIELD => obj! { "columns" => crate::arr![crate::arr!["a", "long"]], "rows" => crate::arr![crate::arr!["x"]] } };
        assert!(decode_answer(&text_in_long, Mode::Rows, 10).is_err());
        let one = obj! { PART_FIELD => obj! { "columns" => crate::arr![crate::arr!["a", "long"]], "rows" => crate::arr![], "enrich" => crate::arr![] } };
        assert!(decode_answer(&one, Mode::Rows, 1).is_ok() && decode_answer(&one, Mode::Rows, 0).is_err(), "at most the columns it may have");
        // Every value's cost is counted before parsing, however small it was to send.
        assert_eq!(json_elements(b"[1,1,1]"), 4);
        assert_eq!(json_elements(br#"["a,b",[]]"#), 4);
    }

    /// The part goes in the request's body: a schema of thousands of fields leaves the headers a
    /// fleet forward carries (16 KiB at most) as small as the link's own.
    #[test]
    fn a_wide_schema_travels_in_the_body() {
        let part = Part { alias: Some("far".into()), mode: Mode::Stats, schema: (0..1000).map(|i| (format!("service.instance.field_{i}"), DataType::Keyword)).collect(), policies: vec!["p".into()], sort_keys: false, max_bytes: None };
        let template = EsRequest::new("POST", "/_query");
        let req = part_request(&template, &Map::new(), "FROM \"items\" | STATS c = COUNT(*)", &part);
        assert!(req.headers.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>() < 64, "{:?}", req.headers);
        let body = crate::json::parse(&req.body).unwrap().unwrap();
        let back = Part::of_value(body.get(PART_FIELD).unwrap()).unwrap();
        assert_eq!((back.schema.len(), back.alias.as_deref(), back.mode), (1000, Some("far"), Mode::Stats));
    }

    /// Two clusters' states merged answer what one cluster accumulating every row does.
    #[test]
    fn merged_states_equal_one_clusters() {
        let rows: Vec<V> = (0..40).map(|i| if i % 7 == 0 { V::Double(i as f64 * 0.25) } else { V::Long(i) }).collect();
        for name in ["count", "sum", "avg", "min", "max", "count_distinct", "values", "median", "std_dev", "present"] {
            let mut one = Acc::for_spec(name, DataType::Long, &[]);
            let (mut a, mut b) = (Acc::for_spec(name, DataType::Long, &[]), Acc::for_spec(name, DataType::Long, &[]));
            for (i, v) in rows.iter().enumerate() {
                one.add(name, v, false);
                if i < 23 { a.add(name, v, false) } else { b.add(name, v, false) }
            }
            let b = decode_acc(&encode_acc(&b)).unwrap();
            a.merge(name, b).unwrap();
            let ty = super::super::value::agg_type(name, DataType::Long);
            assert_eq!(format!("{:?}", a.finish(name, ty, &[])), format!("{:?}", one.finish(name, ty, &[])), "{name}");
        }
        assert!(Acc::Count(1).merge("count", Acc::Collect(Vec::new())).is_err());
    }
}
