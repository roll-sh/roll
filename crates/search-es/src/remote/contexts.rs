//! Scrolls and points in time over linked remote clusters (docs/search/README.md, "Cross-cluster
//! search").
//!
//! Each cluster keeps its own part: the local cluster a scroll or point in time of its own,
//! each linked remote one opened through the link, with its own keep-alive. The coordinating
//! cluster keeps them under one opaque id ([`is_ccs`]: a marker and random bytes, nothing a
//! client can read or forge), together with who opened it: the id answers only on this cluster,
//! and only to the same caller (anyone else gets the missing-context answer, as Elasticsearch's
//! `SecuritySearchOperationListener` gives). Parts' ids never leave roll. The local part is the
//! caller's own scroll or point in time (`search::contexts`, run in process as the caller), so a
//! search on it is authorized for the caller every time (`security::authorize_pit`).
//!
//! - **Scroll**: each part scrolls on its own, `size` hits a page. The coordinator keeps the
//!   hits a part sent but no page has returned yet (held on the request breaker until the
//!   scroll goes), and asks a part for more only when it holds fewer than `size`, so every page
//!   is the `size` best hits left over all clusters, exactly as one cluster's scroll pages
//!   through them. A part it doesn't need is still kept alive as long as the scroll: a remote
//!   one through the link's keep-alive call ([`links::SCROLL_KEEP_ALIVE_HEADER`]), which
//!   fetches nothing. `_search/scroll` and `DELETE _search/scroll` fan out to the parts.
//! - **Point in time**: `POST <local>,<alias>:<index>/_pit` opens one on each cluster; a search
//!   with the id runs on each part's own point in time and merges as any search over linked
//!   clusters does. `search_after` pages exactly: the `_shard_doc` tiebreaker of each hit
//!   carries its part above the shard ([`tag_shard_doc`]), and each part continues after the
//!   last hit's place in that order. `DELETE _pit` closes every part.
//!
//! A context's state is behind a lock of its own: pages of one scroll run one at a time, and a
//! clear or close waits for the page in flight, then closes the parts with their last ids.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::links::{self, Answer, Leg, Split};
use super::{Remote, remotes};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::search::contexts::owner_of;
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
struct Part {
    /// `None`: the local cluster's. Else the remote's alias and the cluster it linked to.
    remote: Option<(String, String)>,
    /// The part's own scroll or point-in-time id.
    id: String,
    /// Scroll: hits it sent that no page returned yet, best first.
    buffer: VecDeque<Value>,
    /// Scroll: it has nothing more.
    done: bool,
    /// When its own scroll expires unless it's called again.
    expires: Instant,
}

impl Part {
    fn new(remote: Option<(String, String)>, id: String, expires: Instant) -> Part {
        Part { remote, id, buffer: VecDeque::new(), done: false, expires }
    }
}

/// A scroll's figures from its first page, which every page reports.
struct ScrollState {
    req: SearchRequest,
    size: usize,
    total: Option<(u64, search_index::collector::top_docs::TotalRelation)>,
    max_score: Option<f32>,
    shards: (usize, usize),
}

/// What a context's pages change.
struct State {
    keep_alive: Duration,
    parts: Vec<Part>,
    /// The parts' buffered hits, held on the request breaker until the context goes.
    held: Option<crate::breaker::Reservation>,
}

struct Entry {
    owner: String,
    /// `None`: a point in time.
    scroll: Option<Arc<ScrollState>>,
    /// A point in time's local part (it never changes): authorized on every search.
    local_pit: Option<String>,
    expires: Mutex<Instant>,
    /// Taken out of the map: a page that was waiting for the state finds it gone.
    removed: AtomicBool,
    state: tokio::sync::Mutex<State>,
}

impl Entry {
    fn new(owner: String, scroll: Option<ScrollState>, local_pit: Option<String>, keep_alive: Duration, parts: Vec<Part>, held: Option<crate::breaker::Reservation>) -> Entry {
        Entry {
            owner,
            scroll: scroll.map(Arc::new),
            local_pit,
            expires: Mutex::new(Instant::now() + keep_alive),
            removed: AtomicBool::new(false),
            state: tokio::sync::Mutex::new(State { keep_alive, parts, held }),
        }
    }
}

/// A cluster's scrolls and points in time over linked clusters.
#[derive(Default)]
pub struct CcsContexts {
    map: Mutex<HashMap<String, Arc<Entry>>>,
}

impl CcsContexts {
    /// Drops expired contexts (their breaker reservation with them; their parts expire on
    /// their own clusters).
    fn purge(map: &mut HashMap<String, Arc<Entry>>) {
        let now = Instant::now();
        map.retain(|_, e| {
            let live = *e.expires.lock() > now;
            if !live {
                e.removed.store(true, AtomicOrdering::Release);
            }
            live
        });
    }

    fn open(&self, e: Entry) -> EsResult<String> {
        let mut map = self.map.lock();
        Self::purge(&mut map);
        if map.len() >= MAX_CONTEXTS {
            return Err(EsError::new(500, "exception", format!("Trying to create too many scroll contexts. Must be less than or equal to: [{MAX_CONTEXTS}].")));
        }
        let id = new_id();
        let e = Arc::new(e);
        if e.scroll.is_some() {
            release_at_expiry(&e);
        }
        map.insert(id.clone(), e);
        Ok(id)
    }

    /// The context `id` names, when it's `owner`'s and alive.
    fn get(&self, id: &str, owner: &str) -> Option<Arc<Entry>> {
        let mut map = self.map.lock();
        Self::purge(&mut map);
        map.get(id).filter(|e| e.owner == owner).cloned()
    }

    /// Takes `owner`'s context `id` out (a page in flight finishes, then finds it gone).
    fn remove(&self, id: &str, owner: &str) -> Option<Arc<Entry>> {
        let mut map = self.map.lock();
        Self::purge(&mut map);
        if !map.get(id).is_some_and(|e| e.owner == owner) {
            return None;
        }
        let e = map.remove(id)?;
        e.removed.store(true, AtomicOrdering::Release);
        Some(e)
    }

    /// Every scroll of `owner` (`DELETE _search/scroll/_all`).
    fn remove_scrolls(&self, owner: &str) -> Vec<Arc<Entry>> {
        let mut map = self.map.lock();
        let ids: Vec<String> = map.iter().filter(|(_, e)| e.owner == owner && e.scroll.is_some()).map(|(k, _)| k.clone()).collect();
        let out: Vec<Arc<Entry>> = ids.iter().filter_map(|k| map.remove(k)).collect();
        for e in &out {
            e.removed.store(true, AtomicOrdering::Release);
        }
        out
    }

    /// The local part of `owner`'s point in time `id` over linked clusters (`None`: no such
    /// point in time, or it has no local part).
    pub fn local_pit(&self, id: &str, owner: &str) -> Option<String> {
        self.get(id, owner).filter(|e| e.scroll.is_none()).and_then(|e| e.local_pit.clone())
    }
}

/// Releases a scroll's buffered hits from the request breaker when it expires (each page moves
/// its expiry), not at the next call that purges: an abandoned scroll mustn't hold the breaker
/// against other searches. Holds the context only while checking.
fn release_at_expiry(e: &Arc<Entry>) {
    let weak = Arc::downgrade(e);
    tokio::spawn(async move {
        loop {
            let Some(e) = weak.upgrade() else { return };
            if e.removed.load(AtomicOrdering::Acquire) {
                return;
            }
            let at = *e.expires.lock();
            if at <= Instant::now() {
                e.removed.store(true, AtomicOrdering::Release);
                e.state.lock().await.held = None;
                return;
            }
            drop(e);
            tokio::time::sleep_until(at.into()).await;
        }
    });
}

/// The remote `alias` as the cluster has it now, when it still links to `target`.
fn current_remote(ctx: &Ctx, alias: &str, target: &str) -> Option<Remote> {
    let links = ctx.cluster.config.links.as_deref()?;
    remotes(&ctx.state()).into_iter().find(|r| r.alias == alias && r.link(links).as_deref() == Some(target))
}

/// A remote part's request, running.
enum PartCall {
    Remote(Remote, tokio::task::JoinHandle<Answer>),
    /// The remote isn't configured (or linked to that cluster) anymore.
    Gone(String),
}

/// Sends a remote part's request (`header`: one of the link's own).
fn send_remote(ctx: &Ctx, alias: &str, target: &str, method: &str, uri: &str, body: Option<Value>, header: Option<&str>) -> PartCall {
    match current_remote(ctx, alias, target) {
        Some(r) => {
            let mut rq = links::request(ctx, alias, method, uri, body);
            if let Some(h) = header {
                rq.headers.push((h.into(), "1".into()));
            }
            PartCall::Remote(r.clone(), links::leg(ctx, &r, target.to_string(), rq))
        }
        None => PartCall::Gone(alias.to_string()),
    }
}

/// What a part answered: its answer, or the error and whether the search may go on without it.
type PartAnswer = Result<Value, (EsError, bool)>;

/// What a remote part answered.
async fn finish_part(call: PartCall) -> PartAnswer {
    match call {
        PartCall::Remote(r, h) => match links::join(&r.alias, h).await {
            Answer::Ok(v) => Ok(v),
            Answer::Failed(e) | Answer::Unavailable(e) => Err((e, r.skip_unavailable)),
        },
        PartCall::Gone(alias) => Err((EsError::new(404, "no_such_remote_cluster_exception", format!("no such remote cluster: [{alias}]")), true)),
    }
}

/// Closes every part (each cluster's scroll or point in time): how many shard contexts went.
async fn close_parts(ctx: &Ctx<'_>, parts: &[Part], pit: bool) -> usize {
    let owner = owner_of(ctx);
    let mut calls = Vec::new();
    let mut freed = 0;
    for p in parts {
        match &p.remote {
            // This cluster's own, the caller's.
            None => freed += ctx.cluster.contexts.close_shards(&p.id, &owner),
            Some((alias, target)) => calls.push(if pit {
                send_remote(ctx, alias, target, "DELETE", "/_pit", Some(obj! { "id" => p.id.clone() }), None)
            } else {
                send_remote(ctx, alias, target, "DELETE", "/_search/scroll", Some(obj! { "scroll_id" => crate::arr![p.id.clone()] }), None)
            }),
        }
    }
    for call in calls {
        if let Ok(v) = Box::pin(finish_part(call)).await {
            freed += v.get("num_freed").and_then(Value::as_i64).unwrap_or(0).max(0) as usize;
        }
    }
    freed
}

/// Holds the parts' buffered hits on the request breaker (the last reservation released).
fn hold_buffers(ctx: &Ctx, st: &mut State) -> EsResult<()> {
    let mut buf = Vec::new();
    let bytes: u64 = st.parts.iter().flat_map(|p| p.buffer.iter()).map(|h| crate::breaker::estimate_hit(h, &mut buf)).sum();
    st.held = None;
    let mut r = crate::breaker::Reservation::new(ctx.cluster.breakers.request(ctx.cluster, &ctx.state()));
    r.add(bytes, "<cross_cluster_scroll>")?;
    st.held = Some(r);
    Ok(())
}

/// The keep-alive a part's request carries.
fn ka_text(d: Duration) -> String {
    format!("{}ms", d.as_millis())
}

// --- Scroll -------------------------------------------------------------------------------

/// The first page of a scroll over linked clusters: each part starts its own scroll for
/// `size` hits, and the page is the best `size` of them all; the rest wait for later pages.
pub async fn start_scroll(ctx: &Ctx<'_>, req: &SearchRequest, body: Option<&Value>, split: Split) -> EsResult<SearchOutcome> {
    let started = Instant::now();
    let ka = crate::search::contexts::check_keep_alive(ctx, req.scroll.unwrap_or(TimeValue::minutes(1)))?;
    let ka_text = ctx.query("scroll").map(str::to_string).unwrap_or_else(|| ka_text(ka));
    let size = req.source.size.unwrap_or(10).max(0) as usize;
    let has_aggs = links::has_aggs(req);
    let mut body = links::part_body(body, size as i64);
    if let Value::Obj(m) = &mut body {
        m.shift_remove("from");
    }
    let qs = links::query_string(ctx, links::PART_DROP, &[("scroll", ka_text.as_str())]);
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
    let expires = Instant::now() + ka;
    // Part `k` of the merge: 0 the local one, `i + 1` leg `i`.
    let mut parts: Vec<(usize, Part)> = Vec::new();
    if let Some(id) = local_id {
        parts.push((0, Part::new(None, id, expires)));
    }
    for (i, answer) in m.answers.iter().enumerate() {
        if let Some(id) = answer.as_ref().and_then(|v| v.get("_scroll_id")).and_then(Value::as_str) {
            parts.push((i + 1, Part::new(Some(targets[i].clone()), id.to_string(), expires)));
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
    let mut st = State { keep_alive: ka, parts: parts.into_iter().map(|(_, p)| p).collect(), held: None };
    if let Err(e) = hold_buffers(ctx, &mut st) {
        Box::pin(close_parts(ctx, &st.parts, false)).await;
        return Err(e);
    }
    let state = ScrollState { req: req.clone(), size, total: out.total, max_score: out.max_score, shards: (out.shards_total, out.shards_skipped) };
    let e = Entry::new(owner_of(ctx), Some(state), None, ka, std::mem::take(&mut st.parts), st.held.take());
    out.scroll_id = Some(ctx.cluster.ccs_contexts.open(e)?);
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

/// A link's keep-alive call for this cluster's part of a scroll: extends it, fetches nothing.
fn keep_alive_only(ctx: &Ctx, id: &str, ka: Option<&str>) -> EsResult<Reply> {
    let keep = crate::search::contexts::check_keep_alive(ctx, TimeValue::parse(ka.unwrap_or("1m"), "scroll")?)?;
    match ctx.cluster.contexts.with(id, &owner_of(ctx), Some(keep), |_| ()) {
        Some(()) => Reply::ok(obj! { "_scroll_id" => id.to_string() }),
        None => Err(crate::search::contexts::missing(id)),
    }
}

/// `GET/POST /_search/scroll`: a scroll over linked clusters here, any other one as before.
pub async fn scroll(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let Ok((Some(id), ka)) = scroll_request(ctx) else { return crate::search::contexts::scroll(ctx) };
    if !is_ccs(&id) {
        if links::is_link_call(ctx) && ctx.request.header(links::SCROLL_KEEP_ALIVE_HEADER).is_some() {
            return keep_alive_only(ctx, &id, ka.as_deref());
        }
        return crate::search::contexts::scroll(ctx);
    }
    let owner = owner_of(ctx);
    let Some(e) = ctx.cluster.ccs_contexts.get(&id, &owner).filter(|e| e.scroll.is_some()) else { return Err(crate::search::contexts::missing(&id)) };
    let keep = match &ka {
        Some(k) => Some(crate::search::contexts::check_keep_alive(ctx, TimeValue::parse(k, "scroll")?)?),
        None => None,
    };
    let state = e.scroll.clone().expect("a scroll");
    let size = state.size;
    // One page at a time; a clear that came first leaves nothing.
    let mut st = e.state.lock().await;
    if e.removed.load(AtomicOrdering::Acquire) {
        return Err(crate::search::contexts::missing(&id));
    }
    let last = keep.is_none() && ctx.cluster.dialect == crate::cluster::Dialect::Elasticsearch;
    let ka_now = keep.unwrap_or(st.keep_alive);
    let ka_sent = ka.clone().unwrap_or_else(|| ka_text(ka_now));
    let now = Instant::now();
    // The scroll lives until `horizon`: a part short of a page fetches its next one, and one
    // that would expire before then (and isn't fetched) is kept alive without fetching.
    let horizon = now + ka_now;
    let mut fetches = Vec::new();
    let mut local_fetch = None;
    let mut keep_alives = Vec::new();
    for (k, p) in st.parts.iter().enumerate() {
        if p.done {
            continue;
        }
        let fetch = p.buffer.len() < size;
        if !fetch && (last || p.expires > horizon) {
            continue;
        }
        match (&p.remote, fetch) {
            (None, true) => local_fetch = Some(k),
            (None, false) => {
                if ctx.cluster.contexts.with(&p.id, &owner, Some(ka_now), |_| ()).is_none() {
                    return Err(crate::search::contexts::missing(&id));
                }
            }
            (Some((alias, target)), fetch) => {
                let mut body = obj! { "scroll_id" => p.id.clone() };
                if let (false, Value::Obj(m)) = (last, &mut body) {
                    m.insert("scroll".into(), ka_sent.clone().into());
                }
                let header = (!fetch).then_some(links::SCROLL_KEEP_ALIVE_HEADER);
                let call = send_remote(ctx, alias, target, "POST", "/_search/scroll", Some(body), header);
                if fetch { fetches.push((k, call)) } else { keep_alives.push((k, call)) }
            }
        }
    }
    let mut failures = Vec::new();
    let mut answers: Vec<(usize, PartAnswer)> = Vec::new();
    if let Some(k) = local_fetch {
        let ka = (!last).then_some(ka_sent.as_str());
        answers.push((k, crate::search::contexts::scroll_by(ctx, &st.parts[k].id, ka).map_err(|e| (e, false))));
    }
    for (k, call) in fetches {
        answers.push((k, Box::pin(finish_part(call)).await));
    }
    for (k, answer) in answers {
        match answer {
            Ok(mut v) => {
                let p = &mut st.parts[k];
                if let Some(id) = v.get("_scroll_id").and_then(Value::as_str) {
                    p.id = id.to_string();
                }
                let hits = match v.as_object_mut().and_then(|m| m.get_mut("hits")).and_then(Value::as_object_mut).and_then(|h| h.shift_remove("hits")) {
                    Some(Value::Arr(a)) => a,
                    _ => Vec::new(),
                };
                p.done = hits.len() < size;
                p.expires = now + ka_now;
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
                st.parts[k].done = true;
                failures.push(obj! { "shard" => -1, "index" => Value::Null, "reason" => e.to_value(false) });
            }
        }
    }
    for (k, call) in keep_alives {
        match Box::pin(finish_part(call)).await {
            Ok(_) => st.parts[k].expires = now + ka_now,
            Err((e, skip)) => {
                if !skip {
                    return Err(e);
                }
                st.parts[k].done = true;
                failures.push(obj! { "shard" => -1, "index" => Value::Null, "reason" => e.to_value(false) });
            }
        }
    }
    // The page: the best `size` hits left over every part (each part's are in order already).
    let mut all: Vec<(usize, Value)> = Vec::new();
    for (k, p) in st.parts.iter_mut().enumerate() {
        all.extend(p.buffer.drain(..).map(|h| (k, h)));
    }
    links::sort_hits(&mut all, &state.req.source.sort);
    let mut page = Vec::with_capacity(size);
    for (k, h) in all {
        if page.len() < size {
            page.push(h);
        } else {
            st.parts[k].buffer.push_back(h);
        }
    }
    let mut out = crate::opensearch::can_match::all_skipped(state.shards.0);
    out.shards_skipped = state.shards.1;
    out.failures = failures;
    out.total = state.total;
    out.max_score = state.max_score;
    out.hits = page;
    if last {
        ctx.cluster.ccs_contexts.remove(&id, &owner);
        Box::pin(close_parts(ctx, &st.parts, false)).await;
    } else {
        if let Err(err) = hold_buffers(ctx, &mut st) {
            ctx.cluster.ccs_contexts.remove(&id, &owner);
            Box::pin(close_parts(ctx, &st.parts, false)).await;
            return Err(err);
        }
        st.keep_alive = ka_now;
        *e.expires.lock() = now + ka_now;
        out.scroll_id = Some(id);
    }
    Reply::ok(crate::search::render_take(&mut out, &state.req))
}

/// `DELETE /_search/scroll`: scrolls over linked clusters close each part; any other as before.
pub async fn clear_scroll(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let ids = crate::search::contexts::clear_scroll_ids(ctx)?;
    let owner = owner_of(ctx);
    let (ccs, plain): (Vec<String>, Vec<String>) = ids.iter().cloned().partition(|i| is_ccs(i));
    let mut entries: Vec<Arc<Entry>> = ccs.iter().filter_map(|id| ctx.cluster.ccs_contexts.get(id, &owner).filter(|e| e.scroll.is_some()).and_then(|_| ctx.cluster.ccs_contexts.remove(id, &owner))).collect();
    if ids.iter().any(|i| i == "_all") {
        entries.extend(ctx.cluster.ccs_contexts.remove_scrolls(&owner));
    }
    let mut freed = if plain.is_empty() { 0 } else { crate::search::contexts::free_scrolls(ctx, &plain)? };
    for e in &entries {
        // After the page in flight, with the parts' last ids.
        let st = e.state.lock().await;
        freed += Box::pin(close_parts(ctx, &st.parts, false)).await;
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
    let expires = Instant::now() + ka;
    let mut calls = Vec::new();
    for (r, target, exprs) in &split.legs {
        let uri = format!("/{}/_pit{qs}", links::path_segment(exprs));
        let call = send_remote(ctx, &r.alias, target, "POST", &uri, body.clone(), None);
        calls.push((r.alias.clone(), target.clone(), exprs.join(","), call));
    }
    let (mut total, mut successful, mut skipped) = (0i64, 0i64, 0i64);
    let mut parts = Vec::new();
    let mut details = Map::new();
    let mut add = |name: String, indices: String, v: &Value, details: &mut Map| {
        let n = |k: &str| v.pointer(&["_shards", k]).and_then(Value::as_i64).unwrap_or(0);
        total += n("total");
        successful += n("successful");
        skipped += n("skipped");
        details.insert(name, obj! {
            "status" => "successful", "indices" => indices, "took" => started.elapsed().as_millis() as i64, "timed_out" => false,
            "_shards" => obj! { "total" => n("total"), "successful" => n("successful"), "skipped" => n("skipped"), "failed" => n("failed") },
        });
    };
    // The local part: the caller's own point in time, opened here as the caller.
    let mut local_pit = None;
    if !split.local.is_empty() {
        let mut lreq = crate::search::parse_request(ctx, None, None)?;
        lreq.indices = split.local.clone();
        let v = crate::search::contexts::open_pit_for(ctx, lreq, ka, body.as_ref().and_then(|b| b.get("index_filter")))?;
        add("(local)".into(), split.local.join(","), &v, &mut details);
        let id = v.get("id").map(Value::to_es_string).unwrap_or_default();
        local_pit = Some(id.clone());
        parts.push(Part::new(None, id, expires));
    }
    for (alias, target, indices, call) in calls {
        match Box::pin(finish_part(call)).await {
            Ok(v) => {
                add(alias.clone(), indices, &v, &mut details);
                parts.push(Part::new(Some((alias, target)), v.get("id").map(Value::to_es_string).unwrap_or_default(), expires));
            }
            Err((e, skip)) => {
                if !skip {
                    Box::pin(close_parts(ctx, &parts, true)).await;
                    return Err(e);
                }
                details.insert(alias, obj! { "status" => "skipped", "indices" => indices, "timed_out" => false, "failures" => crate::arr![obj! { "shard" => -1, "index" => Value::Null, "reason" => e.to_value(false) }] });
            }
        }
    }
    let count = |s: &str| details.values().filter(|d| d.get("status").and_then(Value::as_str) == Some(s)).count() as i64;
    let clusters = obj! { "total" => details.len() as i64, "successful" => count("successful"), "skipped" => count("skipped"), "running" => 0, "partial" => 0, "failed" => 0, "details" => Value::Obj(details.clone()) };
    let id = ctx.cluster.ccs_contexts.open(Entry::new(owner_of(ctx), None, local_pit, ka, parts, None))?;
    Reply::ok(obj! { "id" => id, "_shards" => obj! { "total" => total, "successful" => successful, "skipped" => skipped, "failed" => total - successful - skipped }, "_clusters" => clusters })
}

/// `DELETE /_pit`: a point in time over linked clusters closes on each; any other as before.
pub async fn close_pit(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    let Some(id) = body.get("id").map(Value::to_es_string).filter(|i| is_ccs(i)) else { return crate::search::contexts::close_pit(ctx) };
    let owner = owner_of(ctx);
    let freed = match ctx.cluster.ccs_contexts.get(&id, &owner).filter(|e| e.scroll.is_none()).and_then(|_| ctx.cluster.ccs_contexts.remove(&id, &owner)) {
        Some(e) => {
            let st = e.state.lock().await;
            Box::pin(close_parts(ctx, &st.parts, true)).await
        }
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
    let owner = owner_of(ctx);
    let Some(e) = ctx.cluster.ccs_contexts.get(&pit.id, &owner).filter(|e| e.scroll.is_none()) else { return Err(crate::search::contexts::missing(&pit.id)) };
    // As Elasticsearch, a search extends the point in time only when it sends a keep-alive.
    let keep = match pit.keep_alive {
        Some(k) => Some(crate::search::contexts::check_keep_alive(ctx, k)?),
        None => None,
    };
    // The parts as they are now (a point in time holds no buffers): searched without the lock.
    let parts: Vec<(Option<(String, String)>, String)> = {
        let st = e.state.lock().await;
        if e.removed.load(AtomicOrdering::Acquire) {
            return Err(crate::search::contexts::missing(&pit.id));
        }
        st.parts.iter().map(|p| (p.remote.clone(), p.id.clone())).collect()
    };
    // The order every part sorts in, with the tiebreaker a sorted point in time gets.
    let mut source = req.source.clone();
    crate::search::add_pit_tiebreaker(&mut source);
    let tiebreaker = source.sort.iter().position(|s| matches!(s, Sort::Field { field, .. } if field == "_shard_doc"));
    let desc = tiebreaker.is_some_and(|p| matches!(&source.sort[p], Sort::Field { order: Some(SortOrder::Desc), .. }));
    let from = req.source.from.unwrap_or(0).max(0) as i64;
    let size = req.source.size.unwrap_or(10).max(0) as i64;
    links::check_window(from + size)?;
    let has_aggs = links::has_aggs(req);
    let qs = links::query_string(ctx, links::PART_DROP, &[]);
    let keep_alive = pit.keep_alive.map(|k| format!("{}ms", k.as_millis())).or_else(|| body.and_then(|b| b.pointer(&["pit", "keep_alive"])).map(Value::to_es_string));
    let local_part = parts.iter().position(|(remote, _)| remote.is_none());
    let mut legs = Vec::new();
    let mut remote_parts = Vec::new();
    // Merge part numbers: 0 the local part, then the remote parts in order.
    let mut number = 1;
    for (i, (remote, part_id)) in parts.iter().enumerate() {
        let Some((alias, target)) = remote else { continue };
        let part_no = number;
        number += 1;
        let mut b = match links::part_body(body, from + size) {
            Value::Obj(m) => m,
            _ => Map::new(),
        };
        let mut pit_body = obj! { "id" => part_id.clone() };
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
            lreq.source.pit = Some(PointInTime { id: parts[i].1.clone(), keep_alive: pit.keep_alive });
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
    let mut new_ids = Vec::new();
    for (i, answer) in remote_parts.into_iter().zip(m.answers) {
        if let Some(id) = answer.as_ref().and_then(|v| v.get("pit_id")).and_then(Value::as_str) {
            new_ids.push((i, id.to_string()));
        }
    }
    let mut st = e.state.lock().await;
    for (i, id) in new_ids {
        if let Some(p) = st.parts.get_mut(i) {
            p.id = id;
        }
    }
    // Kept alive from when the search started, as each part's own point in time is.
    if let Some(k) = keep {
        st.keep_alive = k;
        *e.expires.lock() = started + k;
    }
    drop(st);
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
