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

/// How many times its size a part's answer is held on the request breaker before it's parsed
/// (JSON, then values, then rows).
const DECODED_PER_BYTE: u64 = 6;

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
    /// The request carrying the caller's remote access (body and part header added per query),
    /// or why the caller may not reach this remote (no `remote_indices` naming it).
    template: Result<EsRequest, EsError>,
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
}

tokio::task_local! {
    static CCS: Arc<Ccs>;
    static PART: Arc<Part>;
}

impl Ccs {
    /// The request's remotes, when its cluster has links (Elasticsearch flavor) and the request
    /// isn't itself a part.
    pub fn of(ctx: &Ctx, body: &Value) -> Option<Arc<Ccs>> {
        let links = ctx.cluster.config.links.clone()?;
        if ctx.cluster.dialect != crate::cluster::Dialect::Elasticsearch {
            return None;
        }
        let configured = crate::remote::remotes(&ctx.state());
        let linked: Vec<Linked> = configured
            .iter()
            .filter_map(|r| {
                let target = r.link(&*links)?;
                let template = match links::refusal(ctx, r) {
                    Some(e) => Err(e),
                    None => Ok(links::request(ctx, &r.alias, "POST", "/_query", None)),
                };
                Some(Linked { remote: r.clone(), target, template })
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
        Some(Arc::new(Ccs { links, local: links::local_ref(ctx.cluster).to_string(), wait: links::wait(ctx.cluster), configured, linked, forwarded }))
    }

    /// Whether expression `e` names only linked remotes ([`links::names_link`]'s rule).
    fn names_link(&self, e: &str) -> bool {
        let Some((pattern, _)) = crate::remote::split_expression(e) else { return false };
        let mut matched = self.configured.iter().filter(|r| crate::xcontent::filter::glob_match(pattern, &r.alias)).peekable();
        matched.peek().is_some() && matched.all(|r| self.linked.iter().any(|l| l.remote.alias == r.alias))
    }

    /// Splits `sources` into the local part's and each linked remote's (in alias order);
    /// `None` when they name no linked remote.
    fn split(&self, sources: &[String]) -> Option<(Vec<String>, Legs<'_>)> {
        let mut local = Vec::new();
        let mut legs: Legs<'_> = Vec::new();
        for e in sources {
            if !self.names_link(e) {
                local.push(e.clone());
                continue;
            }
            let (pattern, index) = crate::remote::split_expression(e).expect("names a link");
            for l in self.linked.iter().filter(|l| crate::xcontent::filter::glob_match(pattern, &l.remote.alias)) {
                match legs.iter_mut().find(|(x, _)| x.remote.alias == l.remote.alias) {
                    Some((_, exprs)) => exprs.push(index.to_string()),
                    None => legs.push((l, vec![index.to_string()])),
                }
            }
        }
        if legs.is_empty() {
            return None;
        }
        legs.sort_by(|a, b| a.0.remote.alias.cmp(&b.0.remote.alias));
        Some((local, legs))
    }
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

/// The part a link's request asks to run ([`PART_HEADER`]); `None` for anyone else's.
pub fn requested(ctx: &Ctx) -> EsResult<Option<Arc<Part>>> {
    let Some(h) = ctx.request.header(PART_HEADER) else { return Ok(None) };
    if !links::is_link_call(ctx) {
        return Ok(None);
    }
    let bad = || EsError::illegal_argument("an ES|QL part roll can't read");
    if h.len() > links::MAX_REMOTE_ACCESS * 16 {
        return Err(bad());
    }
    let v = crate::json::parse(h.as_bytes()).ok().flatten().ok_or_else(bad)?;
    let mode = match v.get("mode").and_then(Value::as_str) {
        Some("stats") => Mode::Stats,
        Some("rows") => Mode::Rows,
        _ => return Err(bad()),
    };
    let mut schema = Vec::new();
    for c in v.get("schema").and_then(Value::as_array).into_iter().flatten() {
        let (Some(n), Some(t)) = (pair(c, 0).and_then(Value::as_str), pair(c, 1).and_then(Value::as_str).and_then(type_named)) else { return Err(bad()) };
        schema.push((n.to_string(), t));
    }
    let policies = v.get("policies").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
    let alias = v.get("alias").and_then(Value::as_str).map(str::to_string);
    Ok(Some(Arc::new(Part { alias, mode, schema, policies })))
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
}

fn enrich_mode(policy: &str) -> &str {
    match policy.split_once(':') {
        Some((m, _)) => m,
        None => "_any",
    }
}

/// Commands a part runs row by row (each cluster with its own policies and lookup indices).
fn runs_in_parts(c: &Cmd) -> bool {
    match c {
        Cmd::Where(_) | Cmd::Eval(_) | Cmd::Keep(_) | Cmd::Drop(_) | Cmd::Rename(_) | Cmd::MvExpand(..) | Cmd::Dissect { .. } | Cmd::Grok { .. } | Cmd::Sample(_) | Cmd::LookupJoin { .. } => true,
        Cmd::Enrich { policy, .. } => enrich_mode(policy) != "_coordinator",
        _ => false,
    }
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
    // `ENRICH _remote` right after the breaker still runs in the parts (it keeps one row per
    // row, so the limit commutes with it).
    let trailing_remote = |from: usize, part: &mut Vec<usize>| -> usize {
        let mut k = from;
        while let Some(Cmd::Enrich { policy, .. }) = cmds.get(k) {
            if enrich_mode(policy) != "_remote" || synthetic(k) {
                break;
            }
            part.push(k);
            k += 1;
        }
        k
    };
    match cmds.get(i) {
        Some(c @ Cmd::Stats { .. }) => {
            part.push(i);
            Cut { mode: Mode::Stats, part, append_limit: None, rest: cmds[i + 1..].to_vec(), stats: Some(c.clone()) }
        }
        Some(Cmd::Limit(n)) => {
            let mut append_limit = None;
            let after = if synthetic(i) {
                append_limit = Some(*n);
                i + 1
            } else {
                part.push(i);
                trailing_remote(i + 1, &mut part)
            };
            let mut rest = vec![Cmd::Limit(*n)];
            rest.extend(cmds[after..].iter().cloned());
            Cut { mode: Mode::Rows, part, append_limit, rest, stats: None }
        }
        Some(Cmd::Sort(keys)) if cmds.get(i + 1).is_some_and(|c| matches!(c, Cmd::Limit(_))) => {
            // `SORT ... | LIMIT n`: every part sorts and limits, then again here.
            let Some(Cmd::Limit(n)) = cmds.get(i + 1) else { unreachable!() };
            part.push(i);
            let mut append_limit = None;
            let after = if synthetic(i + 1) {
                append_limit = Some(*n);
                i + 2
            } else {
                part.push(i + 1);
                trailing_remote(i + 2, &mut part)
            };
            let mut rest = vec![Cmd::Sort(keys.clone()), Cmd::Limit(*n)];
            rest.extend(cmds[after..].iter().cloned());
            Cut { mode: Mode::Rows, part, append_limit, rest, stats: None }
        }
        _ => Cut { mode: Mode::Rows, part, append_limit: None, rest: cmds[i..].to_vec(), stats: None },
    }
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
/// query over remote clusters: `ENRICH _remote` not after `STATS` or `ENRICH _coordinator`,
/// `LOOKUP JOIN` not after `STATS` or `LIMIT`.
pub fn check_placement(src: &str, spans: &[Span], cmds: &[Cmd]) -> EsResult<()> {
    let mut limited: Option<usize> = None;
    let mut coordinator: Option<usize> = None;
    for (k, c) in cmds.iter().enumerate().skip(1) {
        let Some(span) = spans.get(k) else { break };
        let after = |b: Option<usize>| b.and_then(|b| spans.get(b)).map(|s| cmd_text(src, s));
        match c {
            Cmd::Stats { .. } => {
                limited.get_or_insert(k);
                coordinator.get_or_insert(k);
            }
            Cmd::Limit(_) => {
                limited.get_or_insert(k);
            }
            Cmd::Enrich { policy, at, .. } => match enrich_mode(policy) {
                "_coordinator" => {
                    coordinator.get_or_insert(k);
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
            Cmd::Enrich { policy, policy_at, .. } => Some((policy.rsplit(':').next().unwrap_or(policy).to_string(), enrich_mode(policy).to_string(), *policy_at)),
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

/// A part's result as it answers it ([`PART_FIELD`]).
pub fn encode_answer(table: &Table, mode: Mode, partial: Option<&Partial>, shards: i64) -> Value {
    let cols = |cols: &[Col]| Value::Arr(cols.iter().map(|c| crate::arr![c.name.clone(), c.ty.name()]).collect());
    let mut m = Map::new();
    m.insert("shards".into(), Value::from(shards));
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
}

/// Decodes a part's answer (already parsed, its bytes held on the breaker): every row exactly as
/// wide as its columns, values and states checked as [`decode_v`] and [`decode_acc`] check them.
pub fn decode_answer(v: &Value, mode: Mode) -> Result<Answer, String> {
    let p = v.get(PART_FIELD).ok_or("no part in the answer")?;
    let mut cols = Vec::new();
    for c in p.get("columns").and_then(Value::as_array).ok_or("no columns")? {
        let (Some(n), Some(t)) = (pair(c, 0).and_then(Value::as_str), pair(c, 1).and_then(Value::as_str).and_then(type_named)) else { return Err("a bad column".into()) };
        cols.push(Col { name: n.to_string(), ty: t });
    }
    let num = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0).max(0);
    let mut out = Answer { shards: p.get("shards").and_then(Value::as_i64).unwrap_or(0).clamp(0, i32::MAX as i64), took: num("took"), documents_found: num("documents_found"), values_loaded: num("values_loaded"), ..Answer::default() };
    match mode {
        Mode::Rows => {
            let mut rows = Vec::new();
            for r in p.get("rows").and_then(Value::as_array).ok_or("no rows")? {
                let r = r.as_array().filter(|r| r.len() == cols.len()).ok_or("a row of another width")?;
                let mut row = Vec::with_capacity(r.len());
                for x in r {
                    row.push(decode_v(x, true)?);
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

/// Checks the enrich policies a part was told to have; the error names the missing ones.
pub fn check_part_policies(cluster: &crate::cluster::Cluster, part: &Part) -> EsResult<()> {
    let st = cluster.state();
    let missing: Vec<&str> = part.policies.iter().filter(|p| st.metadata.xpack.get(crate::xpack::enrich::POLICIES, p).is_none()).map(String::as_str).collect();
    if missing.is_empty() { Ok(()) } else { Err(EsError::new(400, MISSING_POLICY, missing.join(","))) }
}

/// A remote's part, sent.
struct Leg {
    alias: String,
    skip_unavailable: bool,
    indices: String,
    handle: tokio::task::JoinHandle<Result<crate::rest::EsResponse, EsError>>,
}

/// One cluster's part's outcome.
enum Outcome {
    Done(Answer),
    /// Skipped (`skip_unavailable`), with why.
    Skipped(EsError),
}

/// Runs query `q` (its source naming linked remotes) as parts on every cluster it names and
/// merges them; `None` when its shape can't be cut into parts (the caller then reads the
/// remotes' rows through cross-cluster search, as before).
pub async fn run(cluster: &crate::cluster::Cluster, ccs: &Ccs, q: &Query, filter: Option<Value>, w: &mut super::value::Warnings) -> EsResult<Option<(Table, super::exec::Stats)>> {
    let started = Instant::now();
    let Some(Cmd::From { sources, metadata, at }) = q.cmds.first() else { return Ok(None) };
    let Some((local_sources, legs)) = ccs.split(sources) else { return Ok(None) };
    // Only the query's own text (its commands as parsed, maybe the implicit limit after them).
    if q.sql || q.spans.is_empty() || q.spans.len() + 1 < q.cmds.len() {
        return Ok(None);
    }
    let cut = cut(&q.cmds, q.spans.len());
    let remote_sources: Vec<Vec<String>> = legs.iter().map(|(_, e)| e.clone()).collect();
    let texts: Vec<String> = match remote_sources.iter().map(|s| part_text(&q.src, &q.spans, s, metadata, &cut.part, cut.append_limit)).collect::<Option<Vec<_>>>() {
        Some(t) => t,
        None => return Ok(None),
    };
    check_placement(&q.src, &q.spans, &q.cmds)?;

    // Every cluster's columns, as one source of them all.
    let schema = super::exec::source_schema(cluster, &q.src, sources, &local_sources, *at).await?;

    // Enrich policies: `_any` must be here and on every part's cluster, `_remote` on every
    // part's cluster, `_coordinator` here (as one cluster's query checks it).
    let used = policies(&q.cmds);
    let st = cluster.state();
    let here = |p: &str| st.metadata.xpack.get(crate::xpack::enrich::POLICIES, p).is_some();
    let mut missing: Vec<(String, usize, Vec<String>)> = Vec::new();
    let mut note = |name: &str, at: usize, cluster_name: &str| match missing.iter_mut().find(|(n, _, _)| n == name) {
        Some((_, _, cs)) => {
            if !cs.iter().any(|c| c == cluster_name) {
                cs.push(cluster_name.to_string());
            }
        }
        None => missing.push((name.to_string(), at, vec![cluster_name.to_string()])),
    };
    for (name, mode, at) in &used {
        if (mode == "_any" || (mode == "_remote" && !local_sources.is_empty())) && !here(name) {
            note(name, *at, "_local");
        }
    }
    drop(st);
    let remote_policies: Vec<String> = {
        let mut v: Vec<String> = used.iter().filter(|(_, m, _)| m != "_coordinator").map(|(n, _, _)| n.clone()).collect();
        v.sort();
        v.dedup();
        v
    };

    // The parts' header: what every part is told.
    let schema_json = Value::Arr(schema.iter().map(|c| crate::arr![c.name.clone(), c.ty.name()]).collect());
    let mode_name = if cut.mode == Mode::Stats { "stats" } else { "rows" };
    let mut sent: Vec<Leg> = Vec::new();
    let mut refused: Vec<(String, bool, String, EsError)> = Vec::new();
    for ((l, exprs), text) in legs.iter().zip(&texts) {
        let indices = exprs.join(",");
        let template = match &l.template {
            Ok(t) => t.clone(),
            Err(e) => {
                refused.push((l.remote.alias.clone(), l.remote.skip_unavailable, indices, e.clone()));
                continue;
            }
        };
        let mut body = ccs.forwarded.clone();
        body.insert("query".into(), Value::Str(text.clone()));
        let mut req = EsRequest::json("POST", "/_query", Value::Obj(body).to_json().into_bytes());
        req.headers.extend(template.headers.iter().cloned());
        let header = obj! { "alias" => l.remote.alias.clone(), "mode" => mode_name, "schema" => schema_json.clone(), "policies" => Value::Arr(remote_policies.iter().cloned().map(Value::Str).collect()) };
        req.headers.push((PART_HEADER.into(), header.to_json()));
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

    // The local part, while the remotes run theirs.
    let local_part = Arc::new(Part { alias: None, mode: cut.mode, schema: schema.iter().map(|c| (c.name.clone(), c.ty)).collect(), policies: Vec::new() });
    let local = if local_sources.is_empty() {
        None
    } else {
        let mut cmds = vec![Cmd::From { sources: local_sources.clone(), metadata: metadata.clone(), at: *at }];
        cmds.extend(cut.part.iter().map(|&k| q.cmds[k].clone()));
        if let Some(n) = cut.append_limit {
            cmds.push(Cmd::Limit(n));
        }
        let lq = Query { src: q.src.clone(), cmds, sql: false, locale: q.locale.clone(), settings: q.settings.clone(), spans: Vec::new() };
        let started = Instant::now();
        let (t, s) = PART.scope(local_part.clone(), super::exec::execute_part(cluster, lq, filter.clone(), w)).await?;
        Some((t, s, started.elapsed().as_millis() as i64))
    };

    // The remotes' answers, held on the request breaker before they're read.
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
    for leg in sent {
        let resp = match leg.handle.await {
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
        held.add(resp.body.len() as u64 * DECODED_PER_BYTE, "<remote_cluster_response>")?;
        let v = crate::json::parse(&resp.body).ok().flatten().unwrap_or(Value::Null);
        if resp.status >= 300 {
            let e = links::error_of(resp.status, &v);
            if e.kind == MISSING_POLICY {
                for name in e.reason().split(',') {
                    if let Some((_, _, at)) = used.iter().find(|(n, _, _)| n == name) {
                        note(name, *at, &leg.alias);
                    }
                }
                continue;
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
        match decode_answer(&v, cut.mode) {
            Ok(a) => outcomes.push((leg.alias.clone(), leg.indices.clone(), Outcome::Done(a))),
            Err(why) => {
                let e = EsError::illegal_state(format!("remote cluster [{}] answered a part roll can't read: {why}", leg.alias));
                fail(&leg.alias, leg.skip_unavailable, e, &leg.indices, &mut outcomes)?;
            }
        }
    }
    if let Some((name, at, mut cs)) = missing.into_iter().next() {
        cs.sort_by(|a, b| (a != "_local", a).cmp(&(b != "_local", b)));
        return Err(super::exec::verification_at(&q.src, at, &format!("cannot find enrich policy [{name}] on clusters [{}]", cs.join(", "))));
    }

    // Merged: the local part first, then the remotes by alias.
    let mut stats = super::exec::Stats::default();
    let mut infos: Vec<ClusterInfo> = Vec::new();
    let mut tables: Vec<Table> = Vec::new();
    let mut partials: Vec<Partial> = Vec::new();
    if let Some((t, s, _)) = local {
        infos.push(ClusterInfo { name: "(local)".into(), indices: local_sources.join(","), status: "successful", took: None, shards: shards_of(cluster, s.indices.as_deref()), failures: Vec::new() });
        stats.documents_found += s.documents_found;
        stats.values_loaded += s.values_loaded;
        stats.indices = s.indices.clone();
        stats.fields = s.fields.clone();
        stats.original_types = s.original_types.clone();
        stats.col_meta = s.col_meta.clone();
        if let Some(p) = s.partial {
            partials.push(p);
        } else {
            tables.push(t);
        }
    }
    for (alias, indices, o) in outcomes {
        match o {
            Outcome::Done(a) => {
                infos.push(ClusterInfo { name: alias, indices, status: "successful", took: Some(a.took), shards: a.shards, failures: Vec::new() });
                stats.documents_found += a.documents_found;
                stats.values_loaded += a.values_loaded;
                match a.partial {
                    Some(p) => partials.push(p),
                    None => tables.push(a.table),
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

    let text = q.src.clone();
    let res = super::exec::load_resources(cluster, &text, &cut.rest).await?;
    let tz = q.settings.time_zone.as_deref().and_then(|z| crate::time::Zone::parse(z).ok()).filter(|z| *z != crate::time::Zone::UTC);
    let src = super::exec::Src { text: &text, sql: false, locale: q.locale.as_deref(), res: &res, tz };
    // The parts' row-by-row commands, for the columns when no part answered.
    let prefix: Vec<Cmd> = cut.part.iter().map(|&k| q.cmds[k].clone()).filter(runs_in_parts).collect();
    let table = match &cut.stats {
        Some(Cmd::Stats { aggs, by }) => {
            // The final reduce: every part's states into one grouper over the same columns.
            let input = match partials.first() {
                Some(p) => p.input.clone(),
                None => super::exec::part_columns(&src, &schema, metadata, &prefix).unwrap_or_default(),
            };
            let mut g = super::exec::Grouper::new(&src, aggs, by, &input)?;
            for p in partials {
                if p.input.iter().map(|c| (&c.name, c.ty)).ne(input.iter().map(|c| (&c.name, c.ty))) {
                    return Err(EsError::illegal_state("clusters answered STATS over columns of different types: roll can't merge them"));
                }
                // Groups are created in order: their keys and states, held while merging.
                held.add(p.groups.len() as u64 * 64, "<remote_cluster_response>")?;
                for (key, accs) in p.groups {
                    g.merge_group(&key, accs).map_err(|why| EsError::illegal_state(format!("a cluster's STATS state roll can't merge: {why}")))?;
                }
            }
            g.finish(w)?
        }
        _ => {
            if tables.is_empty() {
                Table { cols: super::exec::part_columns(&src, &schema, metadata, &prefix).unwrap_or_default(), rows: Vec::new() }
            } else {
                let mut original = std::mem::take(&mut stats.original_types);
                let t = super::exec::union_parts(tables, &mut original)?;
                stats.original_types = original;
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
        // Commands between `SORT` and `LIMIT`: the parts stop before the `SORT`.
        assert_eq!((c.mode, c.part.clone(), c.rest.len()), (Mode::Rows, vec![1], 5), "{c:?}");
        let q = parse("FROM items,far:items METADATA _index | WHERE n > 1 | SORT n | LIMIT 5 | ENRICH _remote:p | STATS c = COUNT(*)");
        let c = cut(&q.cmds, q.spans.len());
        assert_eq!((c.mode, c.part.as_slice()), (Mode::Rows, [1, 2, 3, 4].as_slice()));
        assert!(matches!(c.rest.as_slice(), [Cmd::Sort(_), Cmd::Limit(5), Cmd::Stats { .. }]), "{:?}", c.rest);
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

        // `ENRICH _coordinator` and what follows run here, over the parts' rows.
        let q = parse("FROM far:items | WHERE a | ENRICH _coordinator:p | LIMIT 3");
        let c = cut(&q.cmds, q.spans.len());
        assert_eq!((c.mode, c.part.as_slice(), c.rest.len()), (Mode::Rows, [1].as_slice(), 2));
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
        assert!(decode_answer(&wide, Mode::Rows).is_err());
        let bad_type = obj! { PART_FIELD => obj! { "columns" => crate::arr![crate::arr!["a", "nope"]], "rows" => crate::arr![] } };
        assert!(decode_answer(&bad_type, Mode::Rows).is_err());
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
