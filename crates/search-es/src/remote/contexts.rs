//! Scrolls and points in time over linked remote clusters (docs/search/README.md, "Cross-cluster
//! search").
//!
//! Each cluster keeps its own part: the local cluster a scroll or point in time of its own,
//! each linked remote one opened through the link, with its own keep-alive. The coordinating
//! cluster keeps them under one opaque id ([`is_ccs`]: a marker and random bytes, nothing a
//! client can read or forge), together with who opened it: the id answers only on this cluster,
//! and only to the same caller (anyone else gets the missing-context answer, as Elasticsearch's
//! `SecuritySearchOperationListener` gives). Parts' ids never leave roll.
//!
//! - **Scroll**: each part scrolls on its own, `size` hits a page. The coordinator keeps the
//!   hits a part sent but no page has returned yet, and asks a part for more only when it holds
//!   fewer than `size`, so every page is the `size` best hits left over all clusters, exactly
//!   as one cluster's scroll pages through them. `_search/scroll` and `DELETE _search/scroll`
//!   fan out to the parts.
//! - **Point in time**: `POST <local>,<alias>:<index>/_pit` opens one on each cluster; a search
//!   with the id runs on each part's own point in time and merges as any search over linked
//!   clusters does. `search_after` pages exactly: the `_shard_doc` tiebreaker of each hit
//!   carries its part above the shard ([`tag_shard_doc`]), and each part continues after the
//!   last hit's place in that order. `DELETE _pit` closes every part.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::links::{self, Answer, Leg, Split};
use super::{Remote, remotes};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::search::{SearchOutcome, SearchRequest};
use crate::search_source::{PointInTime, Sort, SortOrder};
use crate::units::TimeValue;

const MARKER: &[u8] = b"\x01ccs";

/// The most scrolls and points in time over linked clusters a cluster keeps.
const MAX_CONTEXTS: usize = 500;

/// Whether `id` is a scroll or point in time over linked clusters (minted here).
pub fn is_ccs(id: &str) -> bool {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(id.trim_end_matches('=')).is_ok_and(|b| b.starts_with(MARKER) && b.len() == MARKER.len() + 16)
}

fn new_id() -> String {
    use base64::Engine as _;
    let mut b = MARKER.to_vec();
    b.extend_from_slice(&rand::random::<[u8; 16]>());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// One cluster's part.
#[derive(Clone)]
struct Part {
    /// `None`: the local cluster's. Else the remote's alias and the cluster it linked to.
    remote: Option<(String, String)>,
    /// The part's own scroll or point-in-time id.
    id: String,
    /// Scroll: hits it sent that no page returned yet, best first.
    buffer: VecDeque<Value>,
    /// Scroll: it has nothing more.
    done: bool,
}

/// A scroll's figures from its first page, which every page reports.
#[derive(Clone)]
struct ScrollState {
    req: SearchRequest,
    size: usize,
    total: Option<(u64, search_index::collector::top_docs::TotalRelation)>,
    max_score: Option<f32>,
    shards: (usize, usize),
}

#[derive(Clone)]
struct CcsContext {
    owner: String,
    expires: Instant,
    keep_alive: Duration,
    parts: Vec<Part>,
    /// `None`: a point in time.
    scroll: Option<ScrollState>,
}

/// A cluster's scrolls and points in time over linked clusters.
#[derive(Default)]
pub struct CcsContexts {
    map: Mutex<HashMap<String, CcsContext>>,
}

impl CcsContexts {
    fn purge(map: &mut HashMap<String, CcsContext>) {
        let now = Instant::now();
        map.retain(|_, c| c.expires > now);
    }

    fn open(&self, c: CcsContext) -> EsResult<String> {
        let mut map = self.map.lock();
        Self::purge(&mut map);
        if map.len() >= MAX_CONTEXTS {
            return Err(EsError::new(500, "exception", format!("Trying to create too many scroll contexts. Must be less than or equal to: [{MAX_CONTEXTS}].")));
        }
        let id = new_id();
        map.insert(id.clone(), c);
        Ok(id)
    }

    /// The context `id` names, when it's `owner`'s and alive.
    fn get(&self, id: &str, owner: &str) -> Option<CcsContext> {
        let mut map = self.map.lock();
        Self::purge(&mut map);
        map.get(id).filter(|c| c.owner == owner).cloned()
    }

    fn put(&self, id: &str, c: CcsContext) {
        self.map.lock().insert(id.to_string(), c);
    }

    fn remove(&self, id: &str, owner: &str) -> Option<CcsContext> {
        let mut map = self.map.lock();
        Self::purge(&mut map);
        if map.get(id).is_some_and(|c| c.owner == owner) { map.remove(id) } else { None }
    }

    /// Every scroll of `owner` (`DELETE _search/scroll/_all`).
    fn remove_scrolls(&self, owner: &str) -> Vec<CcsContext> {
        let mut map = self.map.lock();
        let ids: Vec<String> = map.iter().filter(|(_, c)| c.owner == owner && c.scroll.is_some()).map(|(k, _)| k.clone()).collect();
        ids.iter().filter_map(|k| map.remove(k)).collect()
    }
}

/// Who a context is bound to: the request's user (its realm) or API key.
fn caller(ctx: &Ctx) -> String {
    let p = crate::xpack::security::principal(ctx);
    match &p.kind {
        crate::xpack::security::AuthKind::ApiKey(a) => format!("api_key:{}", a.id),
        _ => format!("{}:{}", p.realm_name, p.username),
    }
}

/// The remote `alias` as the cluster has it now, when it still links to `target`.
fn current_remote(ctx: &Ctx, alias: &str, target: &str) -> Option<Remote> {
    let links = ctx.cluster.config.links.as_deref()?;
    remotes(&ctx.state()).into_iter().find(|r| r.alias == alias && r.link(links).as_deref() == Some(target))
}

/// Sends a part's request: a leg to its remote, or a call on this cluster as the client.
fn send_part(ctx: &Ctx, part: &Part, method: &str, uri: &str, body: Option<Value>) -> PartCall {
    match &part.remote {
        Some((alias, target)) => match current_remote(ctx, alias, target) {
            Some(r) => {
                let rq = links::request(ctx, alias, method, uri, body);
                PartCall::Remote(r.clone(), links::leg(ctx, &r, target.clone(), rq))
            }
            None => PartCall::Gone(alias.clone()),
        },
        None => PartCall::Local(method.to_string(), uri.to_string(), body),
    }
}

enum PartCall {
    Remote(Remote, tokio::task::JoinHandle<Answer>),
    /// The remote isn't configured (or linked to that cluster) anymore.
    Gone(String),
    Local(String, String, Option<Value>),
}

/// What a part answered: its answer, or the error and whether the search may go on without it.
async fn finish_part(ctx: &Ctx<'_>, call: PartCall) -> Result<Value, (EsError, bool)> {
    match call {
        PartCall::Remote(r, h) => match links::join(&r.alias, h).await {
            Answer::Ok(v) => Ok(v),
            Answer::Failed(e) | Answer::Unavailable(e) => Err((e, r.skip_unavailable)),
        },
        PartCall::Gone(alias) => Err((EsError::new(404, "no_such_remote_cluster_exception", format!("no such remote cluster: [{alias}]")), true)),
        PartCall::Local(method, uri, body) => {
            let caller = crate::xpack::internal::Caller::of(ctx);
            crate::xpack::internal::as_caller(caller, crate::xpack::internal::call(ctx.cluster, &method, &uri, body.as_ref())).await.map_err(|e| (e, false))
        }
    }
}

/// Closes every part (each cluster's scroll or point in time): how many shard contexts went.
async fn close_parts(ctx: &Ctx<'_>, parts: &[Part], pit: bool) -> usize {
    let mut calls = Vec::new();
    let mut freed = 0;
    for p in parts {
        let call = match (pit, &p.remote) {
            (false, None) => {
                // This cluster's own scroll.
                freed += ctx.cluster.contexts.close_shards(&p.id);
                continue;
            }
            (true, _) => send_part(ctx, p, "DELETE", "/_pit", Some(obj! { "id" => p.id.clone() })),
            (false, Some(_)) => send_part(ctx, p, "DELETE", "/_search/scroll", Some(obj! { "scroll_id" => crate::arr![p.id.clone()] })),
        };
        calls.push(call);
    }
    for call in calls {
        if let Ok(v) = finish_part(ctx, call).await {
            freed += v.get("num_freed").and_then(Value::as_i64).unwrap_or(0).max(0) as usize;
        }
    }
    freed
}

// --- Scroll -------------------------------------------------------------------------------

/// The first page of a scroll over linked clusters: each part starts its own scroll for
/// `size` hits, and the page is the best `size` of them all; the rest wait for later pages.
pub async fn start_scroll(ctx: &Ctx<'_>, req: &SearchRequest, body: Option<&Value>, split: Split) -> EsResult<SearchOutcome> {
    let started = Instant::now();
    let ka = crate::search::contexts::check_keep_alive(ctx, req.scroll.unwrap_or(TimeValue::minutes(1)))?;
    let ka_text = ctx.query("scroll").map(str::to_string).unwrap_or_else(|| format!("{}ms", ka.as_millis()));
    let size = req.source.size.unwrap_or(10).max(0) as usize;
    let has_aggs = links::has_aggs(req);
    let mut body = links::part_body(body, size as i64);
    if let Value::Obj(m) = &mut body {
        m.shift_remove("from");
    }
    let qs = links::query_string(ctx, links::PART_DROP, &[("scroll", &ka_text)]);
    let mut legs = Vec::new();
    for (r, target, exprs) in &split.legs {
        let uri = format!("/{}/_search{qs}", links::path_segment(exprs));
        let mut rq = links::request(ctx, &r.alias, "POST", &uri, Some(body.clone()));
        if has_aggs {
            rq.headers.push((links::PARTIAL_AGGS_HEADER.into(), "1".into()));
        }
        legs.push(Leg { remote: r.clone(), indices: exprs.join(","), handle: links::leg(ctx, r, target.clone(), rq) });
    }
    let targets: Vec<(String, String)> = split.legs.iter().map(|(r, t, _)| (r.alias.clone(), t.clone())).collect();
    let mut local = if split.local.is_empty() {
        None
    } else {
        let mut lreq = req.clone();
        lreq.indices = split.local.clone();
        lreq.source.size = Some(size as i32);
        lreq.partial_aggs = has_aggs;
        Some(crate::search::run(ctx, lreq)?)
    };
    let local_id = local.as_mut().and_then(|o| o.scroll_id.take());
    let m = Box::pin(links::merge(ctx, req, &req.source.sort, None, local, &split.local.join(","), legs, split.wildcards, started)).await?;
    let mut out = m.out;
    // Part `k` of the merge: 0 the local one, `i + 1` leg `i`.
    let mut parts: Vec<(usize, Part)> = Vec::new();
    if let Some(id) = local_id {
        parts.push((0, Part { remote: None, id, buffer: VecDeque::new(), done: false }));
    }
    for (i, answer) in m.answers.iter().enumerate() {
        if let Some(id) = answer.as_ref().and_then(|v| v.get("_scroll_id")).and_then(Value::as_str) {
            parts.push((i + 1, Part { remote: Some(targets[i].clone()), id: id.to_string(), buffer: VecDeque::new(), done: false }));
        }
    }
    let mut sent = vec![0usize; m.answers.len() + 1];
    let mut hits = Vec::new();
    for (k, h) in m.hits {
        sent[k] += 1;
        if hits.len() < size {
            hits.push(h);
        } else if let Some((_, p)) = parts.iter_mut().find(|(pk, _)| *pk == k) {
            p.buffer.push_back(h);
        }
    }
    for (k, p) in parts.iter_mut() {
        p.done = sent[*k] < size;
    }
    out.hits = hits;
    let state = ScrollState { req: req.clone(), size, total: out.total, max_score: out.max_score, shards: (out.shards_total, out.shards_skipped) };
    let id = ctx.cluster.ccs_contexts.open(CcsContext { owner: caller(ctx), expires: Instant::now() + ka, keep_alive: ka, parts: parts.into_iter().map(|(_, p)| p).collect(), scroll: Some(state) })?;
    out.scroll_id = Some(id);
    Ok(out)
}

/// The scroll id and keep-alive of a `_search/scroll` request, as
/// [`crate::search::contexts::scroll`] reads them.
fn scroll_request(ctx: &Ctx) -> EsResult<(Option<String>, Option<String>)> {
    let body = ctx.body_map()?.unwrap_or_default();
    let id = body.get("scroll_id").map(Value::to_es_string).or_else(|| ctx.param("scroll_id").map(str::to_string));
    let ka = body.get("scroll").map(Value::to_es_string).or_else(|| ctx.query("scroll").map(str::to_string));
    Ok((id, ka))
}

/// `GET/POST /_search/scroll`: a scroll over linked clusters here, any other one as before.
pub async fn scroll(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let Ok((Some(id), ka)) = scroll_request(ctx) else { return crate::search::contexts::scroll(ctx) };
    if !is_ccs(&id) {
        return crate::search::contexts::scroll(ctx);
    }
    let owner = caller(ctx);
    let Some(mut c) = ctx.cluster.ccs_contexts.get(&id, &owner).filter(|c| c.scroll.is_some()) else { return Err(crate::search::contexts::missing(&id)) };
    let keep = match &ka {
        Some(k) => Some(crate::search::contexts::check_keep_alive(ctx, TimeValue::parse(k, "scroll")?)?),
        None => None,
    };
    let state = c.scroll.clone().expect("a scroll");
    let size = state.size;
    // Each part that holds fewer than a page asks its cluster for its next one.
    let mut calls = Vec::new();
    for (k, p) in c.parts.iter().enumerate() {
        if p.done || p.buffer.len() >= size {
            continue;
        }
        let mut body = obj! { "scroll_id" => p.id.clone() };
        if let (Some(k), Value::Obj(m)) = (&ka, &mut body) {
            m.insert("scroll".into(), k.clone().into());
        }
        calls.push((k, send_part(ctx, p, "POST", "/_search/scroll", Some(body))));
    }
    let mut failures = Vec::new();
    for (k, call) in calls {
        match finish_part(ctx, call).await {
            Ok(mut v) => {
                let p = &mut c.parts[k];
                if let Some(id) = v.get("_scroll_id").and_then(Value::as_str) {
                    p.id = id.to_string();
                }
                let hits = match v.as_object_mut().and_then(|m| m.get_mut("hits")).and_then(Value::as_object_mut).and_then(|h| h.shift_remove("hits")) {
                    Some(Value::Arr(a)) => a,
                    _ => Vec::new(),
                };
                p.done = hits.len() < size;
                for mut h in hits {
                    if let Some((alias, _)) = &p.remote {
                        links::prefix_index(&mut h, alias);
                    }
                    p.buffer.push_back(h);
                }
            }
            Err((e, skip)) => {
                if !skip {
                    return Err(e);
                }
                c.parts[k].done = true;
                failures.push(obj! { "shard" => -1, "index" => Value::Null, "reason" => e.to_value(false) });
            }
        }
    }
    // The page: the best `size` hits left over every part (each part's are in order already).
    let mut all: Vec<(usize, Value)> = Vec::new();
    for (k, p) in c.parts.iter_mut().enumerate() {
        all.extend(p.buffer.drain(..).map(|h| (k, h)));
    }
    links::sort_hits(&mut all, &state.req.source.sort);
    let mut page = Vec::with_capacity(size);
    for (k, h) in all {
        if page.len() < size {
            page.push(h);
        } else {
            c.parts[k].buffer.push_back(h);
        }
    }
    let mut out = crate::opensearch::can_match::all_skipped(state.shards.0);
    out.shards_skipped = state.shards.1;
    out.failures = failures;
    out.total = state.total;
    out.max_score = state.max_score;
    out.hits = page;
    // Elasticsearch: a continuation without a keep-alive is the last one.
    match keep {
        None if ctx.cluster.dialect == crate::cluster::Dialect::Elasticsearch => {
            ctx.cluster.ccs_contexts.remove(&id, &owner);
            close_parts(ctx, &c.parts, false).await;
        }
        keep => {
            if let Some(k) = keep {
                c.keep_alive = k;
            }
            c.expires = Instant::now() + c.keep_alive;
            ctx.cluster.ccs_contexts.put(&id, c);
            out.scroll_id = Some(id);
        }
    }
    Reply::ok(crate::search::render_take(&mut out, &state.req))
}

/// `DELETE /_search/scroll`: scrolls over linked clusters close each part; any other as before.
pub async fn clear_scroll(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let ids = crate::search::contexts::clear_scroll_ids(ctx)?;
    let owner = caller(ctx);
    let (ccs, plain): (Vec<String>, Vec<String>) = ids.iter().cloned().partition(|i| is_ccs(i));
    let mut contexts: Vec<CcsContext> = ccs.iter().filter_map(|id| ctx.cluster.ccs_contexts.remove(id, &owner)).filter(|c| c.scroll.is_some()).collect();
    if ids.iter().any(|i| i == "_all") {
        contexts.extend(ctx.cluster.ccs_contexts.remove_scrolls(&owner));
    }
    let mut freed = if plain.is_empty() { 0 } else { crate::search::contexts::free_scrolls(ctx, &plain)? };
    for c in &contexts {
        freed += close_parts(ctx, &c.parts, false).await;
    }
    Ok(crate::search::contexts::cleared(freed, &ids))
}

// --- Point in time ------------------------------------------------------------------------

/// `POST /{index}/_pit`: over linked clusters, a point in time on each; any other as before.
pub async fn open_pit(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let exprs = ctx.param("index").map(crate::meta::resolve::split_expressions).unwrap_or_default();
    let (Some(ka_text), Some(split)) = (ctx.query("keep_alive"), links::split(ctx, &exprs)?) else { return crate::search::contexts::open_pit(ctx) };
    let ka = crate::search::contexts::check_keep_alive(ctx, TimeValue::parse(ka_text, "keep_alive")?)?;
    let body = ctx.body()?;
    let qs = links::query_string(ctx, &["index", "pretty", "human", "error_trace", "filter_path", "format"], &[]);
    let started = Instant::now();
    // (name in `_clusters`, its expressions, the part, its call)
    let mut calls = Vec::new();
    if !split.local.is_empty() {
        let part = Part { remote: None, id: String::new(), buffer: VecDeque::new(), done: false };
        let uri = format!("/{}/_pit{qs}", links::path_segment(&split.local));
        calls.push(("(local)".to_string(), split.local.join(","), part, PartCall::Local("POST".into(), uri, body.clone())));
    }
    for (r, target, exprs) in &split.legs {
        let part = Part { remote: Some((r.alias.clone(), target.clone())), id: String::new(), buffer: VecDeque::new(), done: false };
        let uri = format!("/{}/_pit{qs}", links::path_segment(exprs));
        let call = send_part(ctx, &part, "POST", &uri, body.clone());
        calls.push((r.alias.clone(), exprs.join(","), part, call));
    }
    let (mut total, mut successful, mut skipped) = (0i64, 0i64, 0i64);
    let mut parts = Vec::new();
    let mut details = Map::new();
    for (name, indices, mut part, call) in calls {
        match finish_part(ctx, call).await {
            Ok(v) => {
                let n = |k: &str| v.pointer(&["_shards", k]).and_then(Value::as_i64).unwrap_or(0);
                total += n("total");
                successful += n("successful");
                skipped += n("skipped");
                details.insert(name, obj! {
                    "status" => "successful", "indices" => indices, "took" => started.elapsed().as_millis() as i64, "timed_out" => false,
                    "_shards" => obj! { "total" => n("total"), "successful" => n("successful"), "skipped" => n("skipped"), "failed" => n("failed") },
                });
                part.id = v.get("id").map(Value::to_es_string).unwrap_or_default();
                parts.push(part);
            }
            Err((e, skip)) => {
                if !skip {
                    close_parts(ctx, &parts, true).await;
                    return Err(e);
                }
                details.insert(name, obj! { "status" => "skipped", "indices" => indices, "timed_out" => false, "failures" => crate::arr![obj! { "shard" => -1, "index" => Value::Null, "reason" => e.to_value(false) }] });
            }
        }
    }
    let count = |s: &str| details.values().filter(|d| d.get("status").and_then(Value::as_str) == Some(s)).count() as i64;
    let clusters = obj! { "total" => details.len() as i64, "successful" => count("successful"), "skipped" => count("skipped"), "running" => 0, "partial" => 0, "failed" => 0, "details" => Value::Obj(details.clone()) };
    let id = ctx.cluster.ccs_contexts.open(CcsContext { owner: caller(ctx), expires: Instant::now() + ka, keep_alive: ka, parts, scroll: None })?;
    Reply::ok(obj! { "id" => id, "_shards" => obj! { "total" => total, "successful" => successful, "skipped" => skipped, "failed" => total - successful - skipped }, "_clusters" => clusters })
}

/// `DELETE /_pit`: a point in time over linked clusters closes on each; any other as before.
pub async fn close_pit(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    let Some(id) = body.get("id").map(Value::to_es_string).filter(|i| is_ccs(i)) else { return crate::search::contexts::close_pit(ctx) };
    let freed = match ctx.cluster.ccs_contexts.remove(&id, &caller(ctx)).filter(|c| c.scroll.is_none()) {
        Some(c) => close_parts(ctx, &c.parts, true).await,
        None => 0,
    };
    // As Elasticsearch answers a point in time that's gone.
    Ok(Reply::Body(if freed > 0 { 200 } else { 404 }, obj! { "succeeded" => freed > 0, "num_freed" => freed as i64 }))
}

/// How many bits of a `_shard_doc` value a point in time over linked clusters gives the part:
/// the shard request index sits in bits 32..56, the part above it.
const PART_SHIFT: u32 = 56;

/// Folds part `part` into the `_shard_doc` value at `pos` of a hit's `sort`, so hits of every
/// cluster order by part, then shard, then document, and a `search_after` names its place in
/// that order.
pub(crate) fn tag_shard_doc(hit: &mut Value, pos: usize, part: usize) {
    if let Some(Value::Arr(sort)) = hit.as_object_mut().and_then(|m| m.get_mut("sort"))
        && let Some(v) = sort.get_mut(pos)
        && let Some(n) = v.as_i64()
    {
        *v = Value::from(((part as i64) << PART_SHIFT) | (n & ((1i64 << PART_SHIFT) - 1)));
    }
}

/// A `search_after` for part `part` from one whose `_shard_doc` value at `pos` names a place
/// in the tagged order: the same value within its own part, everything (`-1`) in the parts
/// after it and nothing in the parts before (reversed for a descending tiebreaker).
fn after_for(after: &[Value], pos: usize, part: usize, desc: bool) -> Vec<Value> {
    let mut out = after.to_vec();
    if let Some(n) = out.get(pos).and_then(Value::as_i64) {
        let at = (n >> PART_SHIFT) as usize;
        let none_after = (1i64 << PART_SHIFT) - 1;
        out[pos] = Value::from(match at.cmp(&part) {
            std::cmp::Ordering::Equal => n & none_after,
            std::cmp::Ordering::Less if !desc => -1,
            std::cmp::Ordering::Less => none_after,
            std::cmp::Ordering::Greater if !desc => none_after,
            std::cmp::Ordering::Greater => -1,
        });
    }
    out
}

/// A search on a point in time over linked clusters: each part searches its own point in time
/// for `from + size` hits (continuing after `search_after` in its own place), and the answers
/// merge as any search over linked clusters.
pub async fn pit_search(ctx: &Ctx<'_>, req: &SearchRequest, body: Option<&Value>) -> EsResult<SearchOutcome> {
    let started = Instant::now();
    let pit = req.source.pit.clone().expect("a point in time");
    if !req.indices.is_empty() {
        return Err(EsError::action_request_validation(&["[indices] cannot be used with point in time. Do not specify any index with point in time.".into()]));
    }
    let owner = caller(ctx);
    let Some(mut c) = ctx.cluster.ccs_contexts.get(&pit.id, &owner).filter(|c| c.scroll.is_none()) else { return Err(crate::search::contexts::missing(&pit.id)) };
    if let Some(k) = pit.keep_alive {
        c.keep_alive = crate::search::contexts::check_keep_alive(ctx, k)?;
    }
    // The order every part sorts in, with the tiebreaker a sorted point in time gets.
    let mut source = req.source.clone();
    crate::search::add_pit_tiebreaker(&mut source);
    let tiebreaker = source.sort.iter().position(|s| matches!(s, Sort::Field { field, .. } if field == "_shard_doc"));
    let desc = tiebreaker.is_some_and(|p| matches!(&source.sort[p], Sort::Field { order: Some(SortOrder::Desc), .. }));
    let from = req.source.from.unwrap_or(0).max(0) as i64;
    let size = req.source.size.unwrap_or(10).max(0) as i64;
    let has_aggs = links::has_aggs(req);
    let qs = links::query_string(ctx, links::PART_DROP, &[]);
    let keep_alive = pit.keep_alive.map(|k| format!("{}ms", k.as_millis())).or_else(|| body.and_then(|b| b.pointer(&["pit", "keep_alive"])).map(Value::to_es_string));
    let local_part = c.parts.iter().position(|p| p.remote.is_none());
    let mut legs = Vec::new();
    let mut remote_parts = Vec::new();
    // Merge part numbers: 0 the local part, then the remote parts in order.
    let mut number = 1;
    for (i, p) in c.parts.iter().enumerate() {
        let Some((alias, target)) = &p.remote else { continue };
        let part_no = number;
        number += 1;
        let mut b = match links::part_body(body, from + size) {
            Value::Obj(m) => m,
            _ => Map::new(),
        };
        let mut pit_body = obj! { "id" => p.id.clone() };
        if let (Some(k), Value::Obj(m)) = (&keep_alive, &mut pit_body) {
            m.insert("keep_alive".into(), k.clone().into());
        }
        b.insert("pit".into(), pit_body);
        if let (Some(pos), Some(after)) = (tiebreaker, &req.source.search_after) {
            b.insert("search_after".into(), Value::Arr(after_for(after, pos, part_no, desc)));
        }
        let Some(r) = current_remote(ctx, alias, target) else {
            return Err(EsError::new(404, "no_such_remote_cluster_exception", format!("no such remote cluster: [{alias}]")));
        };
        let mut rq = links::request(ctx, alias, "POST", &format!("/_search{qs}"), Some(Value::Obj(b)));
        if has_aggs {
            rq.headers.push((links::PARTIAL_AGGS_HEADER.into(), "1".into()));
        }
        legs.push(Leg { remote: r.clone(), indices: String::new(), handle: links::leg(ctx, &r, target.clone(), rq) });
        remote_parts.push(i);
    }
    let local = match local_part {
        Some(i) => {
            let mut lreq = req.clone();
            lreq.source.pit = Some(PointInTime { id: c.parts[i].id.clone(), keep_alive: pit.keep_alive });
            lreq.source.from = Some(0);
            lreq.source.size = Some((from + size) as i32);
            lreq.partial_aggs = has_aggs;
            if let (Some(pos), Some(after)) = (tiebreaker, &req.source.search_after) {
                lreq.source.search_after = Some(after_for(after, pos, 0, desc));
            }
            Some(crate::search::run(ctx, lreq)?)
        }
        None => None,
    };
    let m = Box::pin(links::merge(ctx, req, &source.sort, tiebreaker, local, "", legs, false, started)).await?;
    let mut out = m.out;
    out.hits = m.hits.into_iter().skip(from as usize).take(size as usize).map(|(_, h)| h).collect();
    // A part's point in time may come back under a new id.
    for (i, answer) in remote_parts.into_iter().zip(m.answers) {
        if let Some(id) = answer.as_ref().and_then(|v| v.get("pit_id")).and_then(Value::as_str) {
            c.parts[i].id = id.to_string();
        }
    }
    c.expires = Instant::now() + c.keep_alive;
    ctx.cluster.ccs_contexts.put(&pit.id, c);
    out.pit_id = Some(pit.id);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_opaque_and_marked() {
        let id = new_id();
        assert!(is_ccs(&id));
        assert_ne!(id, new_id());
        assert!(!is_ccs("abc") && !is_ccs(""));
    }

    #[test]
    fn search_after_continues_in_each_parts_place() {
        let mut hit = obj! { "sort" => crate::arr![5, crate::search::sort::encode_shard_doc(1, 7)] };
        tag_shard_doc(&mut hit, 1, 2);
        let after = hit.get("sort").and_then(Value::as_array).cloned().unwrap();
        // Its own part: the same place; parts after it: everything with the same sort value;
        // parts before: nothing.
        assert_eq!(after_for(&after, 1, 2, false)[1], Value::from(crate::search::sort::encode_shard_doc(1, 7)));
        assert_eq!(after_for(&after, 1, 3, false)[1], Value::from(-1));
        assert_eq!(after_for(&after, 1, 0, false)[1], Value::from((1i64 << PART_SHIFT) - 1));
        assert_eq!(after_for(&after, 1, 0, true)[1], Value::from(-1));
        assert_eq!(after_for(&after, 1, 0, false)[0], Value::from(5));
    }
}
