//! Searches as tasks, the way Elasticsearch's `TaskManager` shows them.
//!
//! Every search-family request registers a task for as long as it runs: `_search` and `_count`
//! as `indices:data/read/search`, `_search/scroll` as `indices:data/read/scroll`, `_msearch`,
//! search templates, point-in-time opens and the ES|QL / SQL / EQL queries under their own
//! action names. Under it, each shard a search touches gets a `[phase/query]` child for the
//! duration of the search (`shardId[[index][n]]`), and a search a parent runs (an `_msearch`
//! item, a template's search) gets an `indices:data/read/search` child of its own, so
//! `_tasks?group_by=parents` shows the tree Elasticsearch shows.
//!
//! The task's cancellation flag *is* the search's [`search_index::search::SearchControl`] flag:
//! `_tasks/_cancel`, a client that disconnects ([`crate::rest::handle_shared`]) and a node
//! shutting down ([`crate::cluster::TaskRegistry::cancel_all`]) all set it, and collectors and
//! scripts already poll it. Children share their parent's flag, which is Elasticsearch's ban
//! on a cancelled parent's children.
//!
//! Cost on the search path: one registry insert per task, and nothing rendered. A task's
//! description (`indices[..], search_type[..], source[..]`) is built only when a detailed
//! listing asks for it ([`crate::cluster::Describe`]), by parsing the request again; the
//! current task lives in a thread-local, so the search code below the REST layer needs no
//! extra arguments.

use crate::cluster::{Cluster, Describe, NewTask};
use crate::json::{Map, Value};
use crate::rest::{Ctx, EsRequest};
use crate::routes::Api;
use crate::search::{SearchRequest, Target};
use crate::search_source::{FetchSource, SearchSource, Sort, SortOrder, TrackTotalHits};
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// The reason a task cancelled through `_tasks/_cancel` reports.
pub const USER_CANCEL_REASON: &str = "by user request";

/// What kind of request a task runs, which decides the children its searches get.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `_search`, `_count`: shard children directly under it.
    Search,
    /// `_search/scroll`: scroll-phase shard children.
    Scroll,
    /// Anything that runs searches of its own (`_msearch`, templates, ES|QL, …): each search
    /// gets an `indices:data/read/search` child first.
    Parent,
}

/// The task a REST API registers: its action, kind and whether it can be cancelled.
pub fn action_of(api: Api) -> Option<(&'static str, Kind, bool)> {
    Some(match api {
        Api::Search | Api::Count => ("indices:data/read/search", Kind::Search, true),
        Api::Scroll => ("indices:data/read/scroll", Kind::Scroll, true),
        Api::Msearch => ("indices:data/read/msearch", Kind::Parent, true),
        // `TransportSearchTemplateAction` runs a plain, non-cancellable task.
        Api::SearchTemplate => ("indices:data/read/search/template", Kind::Parent, false),
        Api::MsearchTemplate => ("indices:data/read/msearch/template", Kind::Parent, true),
        Api::OpenPointInTime => ("indices:data/read/open_point_in_time", Kind::Search, true),
        Api::EsqlQuery => ("indices:data/read/esql", Kind::Parent, true),
        Api::SqlQuery => ("indices:data/read/sql", Kind::Parent, true),
        Api::EqlSearch => ("indices:data/read/eql", Kind::Parent, true),
        _ => return None,
    })
}

/// The task running on this thread.
#[derive(Clone)]
struct Current {
    /// The cluster the task belongs to (its registry), by address.
    cluster: usize,
    id: u64,
    kind: Kind,
    flag: Arc<AtomicBool>,
    headers: Arc<[(String, String)]>,
}

thread_local! {
    static CURRENT: RefCell<Option<Current>> = const { RefCell::new(None) };
}

fn cluster_key(c: &Cluster) -> usize {
    c as *const Cluster as usize
}

/// Restores the thread's previous task when dropped.
struct Restore(Option<Current>);

impl Drop for Restore {
    fn drop(&mut self) {
        let prev = self.0.take();
        CURRENT.with(|c| *c.borrow_mut() = prev);
    }
}

fn enter(cur: Current) -> Restore {
    Restore(CURRENT.with(|c| c.borrow_mut().replace(cur)))
}

/// Whether the search running on this thread was cancelled (its task's flag).
pub fn cancelled() -> bool {
    CURRENT.with(|c| c.borrow().as_ref().is_some_and(|c| c.flag.load(std::sync::atomic::Ordering::Relaxed)))
}

/// The top-level task of a REST request; ends when dropped.
pub struct RequestTask<'a> {
    cluster: &'a Cluster,
    id: u64,
    _restore: Restore,
    _ambient: Option<search_index::search::AmbientGuard>,
}

impl Drop for RequestTask<'_> {
    fn drop(&mut self) {
        self.cluster.tasks.end(self.id);
    }
}

/// Headers a task keeps from its request (`X-Opaque-Id`, `traceparent`).
pub fn task_headers(req: &EsRequest) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(v) = req.header("x-opaque-id") {
        out.push(("X-Opaque-Id".to_string(), v.to_string()));
    }
    if let Some(v) = req.header("traceparent") {
        out.push(("trace.id".to_string(), v.split('-').nth(1).unwrap_or(v).to_string()));
    }
    out
}

impl<'a> RequestTask<'a> {
    /// Registers the task of a search-family request (`None` for other APIs). Its
    /// cancellation flag is the one already guarding this thread's searches (a client
    /// disconnect sets it), or a new one that becomes the thread's search control.
    pub fn start(ctx: &'a Ctx<'_>) -> Option<RequestTask<'a>> {
        let (action, kind, cancellable) = action_of(ctx.api)?;
        let cluster: &'a Cluster = ctx.cluster;
        let (flag, ambient) = match search_index::search::ambient_control().and_then(|c| c.cancelled) {
            Some(f) => (f, None),
            None => {
                let f = Arc::new(AtomicBool::new(false));
                let ctl = search_index::search::SearchControl { deadline: None, cancelled: Some(f.clone()), ext: None };
                (f, Some(search_index::search::set_ambient_control(Some(ctl))))
            }
        };
        let headers = task_headers(ctx.request);
        let describe = describer(ctx);
        let id = cluster.tasks.start_detached(NewTask {
            action: action.into(),
            description: String::new(),
            describe: Some(describe),
            parent: None,
            headers: headers.clone(),
            cancellable,
            cancelled: Some(flag.clone()),
        });
        let restore = enter(Current { cluster: cluster_key(cluster), id, kind, flag, headers: headers.into() });
        Some(RequestTask { cluster, id, _restore: restore, _ambient: ambient })
    }
}

/// The children a search registered for its shards (and, under a parent task, the search's
/// own task); they end when dropped.
pub struct SearchTasks<'a> {
    cluster: &'a Cluster,
    ids: Vec<u64>,
    _restore: Option<Restore>,
    _ambient: Option<search_index::search::AmbientGuard>,
}

impl Drop for SearchTasks<'_> {
    fn drop(&mut self) {
        self.cluster.tasks.end_all(&self.ids);
    }
}

/// Registers the tasks of one search over `targets` under the task running on this thread
/// (nothing when there is none: internal searches). Called where a search fans out to its
/// shards.
pub fn search_started<'a>(cluster: &'a Cluster, req: &SearchRequest, targets: &[Target]) -> Option<SearchTasks<'a>> {
    let cur = CURRENT.with(|c| c.borrow().clone()).filter(|c| c.cluster == cluster_key(cluster))?;
    let mut ids = Vec::with_capacity(targets.len() + 1);
    let mut restore = None;
    let (parent, kind) = match cur.kind {
        Kind::Search | Kind::Scroll => (cur.id, cur.kind),
        Kind::Parent => {
            // A search a parent runs (an `_msearch` item): its own task, described now (the
            // parent's request isn't this search's).
            let id = cluster.tasks.start_detached(NewTask {
                action: "indices:data/read/search".into(),
                description: search_description(req, cluster.dialect),
                describe: None,
                parent: Some(cur.id),
                headers: cur.headers.to_vec(),
                cancellable: true,
                cancelled: Some(cur.flag.clone()),
            });
            ids.push(id);
            restore = Some(enter(Current { id, kind: Kind::Search, ..cur.clone() }));
            (id, Kind::Search)
        }
    };
    let (action, scroll) = match kind {
        Kind::Scroll if targets.len() == 1 => ("indices:data/read/search[phase/query+fetch/scroll]", true),
        Kind::Scroll => ("indices:data/read/search[phase/query/scroll]", true),
        _ => ("indices:data/read/search[phase/query]", false),
    };
    let keep_alive = req.scroll.map(|s| s.human());
    for (i, t) in targets.iter().enumerate() {
        let description = if scroll {
            format!("id[{}], scroll[{}]", i + 1, keep_alive.as_deref().unwrap_or("null"))
        } else {
            format!("shardId[[{}][{}]]", t.idx.name, t.shard.id)
        };
        ids.push(cluster.tasks.start_detached(NewTask {
            action: action.into(),
            description,
            describe: None,
            parent: Some(parent),
            headers: cur.headers.to_vec(),
            cancellable: true,
            cancelled: Some(cur.flag.clone()),
        }));
    }
    Some(SearchTasks { cluster, ids, _restore: restore, _ambient: None })
}

/// Registers the search a by-query run (task `parent`) scans its documents with: an
/// `indices:data/read/search` child of it, described like the scroll Elasticsearch opens, with
/// a `[phase/query]` child per shard, all ended when the guard drops. While it lives, the
/// thread's task is the search (scripts poll its cancellation).
pub fn scan_started<'a>(ctx: &Ctx<'a>, parent: u64, req: &SearchRequest, targets: &[Target]) -> SearchTasks<'a> {
    let cluster = ctx.cluster;
    let tasks = &cluster.tasks;
    let headers = task_headers(ctx.request);
    let cancelled = tasks.get(parent).map(|t| t.cancelled);
    let search = tasks.start_detached(NewTask {
        action: "indices:data/read/search".into(),
        description: search_description(req, cluster.dialect),
        describe: None,
        parent: Some(parent),
        headers: headers.clone(),
        cancellable: true,
        cancelled: cancelled.clone(),
    });
    let mut ids = Vec::with_capacity(targets.len() + 1);
    ids.push(search);
    for t in targets {
        ids.push(tasks.start_detached(NewTask {
            action: "indices:data/read/search[phase/query]".into(),
            description: format!("shardId[[{}][{}]]", t.idx.name, t.shard.id),
            describe: None,
            parent: Some(search),
            headers: headers.clone(),
            cancellable: true,
            cancelled: cancelled.clone(),
        }));
    }
    // The scan (and the scripts it runs) sees the run's cancellation: `_tasks/_cancel` of the
    // run stops it, as it stops Elasticsearch's scroll.
    let ambient = match (&cancelled, search_index::search::ambient_control()) {
        (Some(flag), None) => Some(search_index::search::set_ambient_control(Some(search_index::search::SearchControl { deadline: None, cancelled: Some(flag.clone()), ext: None }))),
        _ => None,
    };
    let restore = cancelled.map(|flag| enter(Current { cluster: cluster_key(cluster), id: search, kind: Kind::Search, flag, headers: headers.into() }));
    SearchTasks { cluster, ids, _restore: restore, _ambient: ambient }
}

/// The description of the request `ctx` runs, built when a listing asks for it.
fn describer(ctx: &Ctx) -> Describe {
    let api = ctx.api;
    let method = ctx.method;
    let uri = ctx.uri.clone();
    let body = ctx.body.clone();
    let content_type = ctx.request.header("content-type").map(str::to_string);
    Describe(Arc::new(move |cluster: &Cluster| {
        let mut headers = Vec::with_capacity(1);
        if let Some(ct) = &content_type {
            headers.push(("content-type".to_string(), ct.clone()));
        }
        let req = EsRequest { method: method.as_str().to_string(), uri: uri.clone(), headers, body: body.clone() };
        crate::rest::with_ctx(cluster, &req, |ctx| describe(ctx, api)).unwrap_or_default()
    }))
}

/// `getDescription()` of the request `ctx` holds.
fn describe(ctx: &Ctx, api: Api) -> String {
    let dialect = ctx.cluster.dialect;
    let body = || ctx.body().ok().flatten();
    let str_field = |k: &str| body().and_then(|b| b.get(k).and_then(|v| v.as_str().map(str::to_string)));
    match api {
        Api::Search => match crate::search::parse_request(ctx, body().as_ref(), ctx.raw_json()) {
            Ok(req) => search_description(&req, dialect),
            Err(_) => String::new(),
        },
        Api::Count => {
            let mut req = match crate::search::parse_request(ctx, None, None) {
                Ok(r) => r,
                Err(_) => return String::new(),
            };
            if let Some(q) = body().and_then(|b| b.get("query").cloned())
                && let Ok((q, _)) = crate::query::parse_query(&q, None, &[])
            {
                req.source.query = Some(q);
            }
            req.source.size = Some(0);
            req.source.track_total_hits = Some(TrackTotalHits::Accurate);
            if let Some(m) = ctx.query("min_score").and_then(|v| v.parse::<f32>().ok()) {
                req.source.min_score = Some(m);
            }
            search_description(&req, dialect)
        }
        Api::Scroll => {
            let id = ctx.param("scroll_id").map(str::to_string).or_else(|| str_field("scroll_id")).unwrap_or_else(|| "null".into());
            let scroll = ctx.query("scroll").map(str::to_string).or_else(|| str_field("scroll"));
            let scroll = scroll.and_then(|s| crate::units::TimeValue::parse(&s, "scroll").ok()).map_or_else(|| "null".to_string(), |t| t.human());
            format!("scrollId[{id}], scroll[{scroll}]")
        }
        Api::Msearch => {
            let items = msearch_items(ctx);
            let parts: Vec<String> = items.iter().map(|r| search_description(r, dialect)).collect();
            format!("requests[{}]: {}", parts.len(), parts.join(" | "))
        }
        Api::MsearchTemplate => format!("requests[{}]", ctx.body.split(|b| *b == b'\n').filter(|l| !l.iter().all(u8::is_ascii_whitespace)).count() / 2),
        Api::OpenPointInTime => {
            let indices = ctx.param("index").unwrap_or_default();
            let keep_alive = ctx.query("keep_alive").and_then(|s| crate::units::TimeValue::parse(s, "keep_alive").ok()).map_or_else(|| "null".to_string(), |t| t.human());
            format!("open search context: indices [{indices}] keep_alive [{keep_alive}]")
        }
        Api::EsqlQuery => str_field("query").unwrap_or_default(),
        Api::SqlQuery => {
            let filter = body().and_then(|b| b.get("filter").cloned()).and_then(|f| crate::query::parse_query(&f, None, &[]).ok()).map(|(q, _)| q.to_value().to_pretty_json());
            format!("SQL [{}][{}]", str_field("query").unwrap_or_default(), filter.as_deref().unwrap_or("null"))
        }
        Api::EqlSearch => format!("indices[{}], {}", ctx.param("index").unwrap_or_default(), str_field("query").unwrap_or_default()),
        _ => String::new(),
    }
}

/// The searches of an `_msearch` body, as far as they parse.
pub(crate) fn msearch_items(ctx: &Ctx) -> Vec<SearchRequest> {
    let default_index = ctx.path_param("index").map(crate::meta::resolve::split_expressions).unwrap_or_default();
    let mut out = Vec::new();
    let mut lines = ctx.body.split(|b| *b == b'\n').filter(|l| !l.iter().all(u8::is_ascii_whitespace));
    while let (Some(h), Some(b)) = (lines.next(), lines.next()) {
        let header = crate::json::parse(h).ok().flatten().unwrap_or(Value::Obj(Map::new()));
        let body = crate::json::parse(b).ok().flatten();
        let source = match &body {
            Some(v @ Value::Obj(_)) => SearchSource::parse(v, Some(b)).unwrap_or_default(),
            _ => SearchSource::default(),
        };
        let Ok(mut req) = crate::search::parse_request(ctx, None, None) else { continue };
        req.source = source;
        req.indices = match header.get("index") {
            Some(Value::Str(s)) => crate::meta::resolve::split_expressions(s),
            Some(Value::Arr(a)) => a.iter().map(|v| v.to_es_string()).collect(),
            _ => default_index.clone(),
        };
        req.dfs = header.get("search_type").and_then(Value::as_str) == Some("dfs_query_then_fetch");
        req.routing = header.get("routing").map(|v| v.to_es_string());
        req.preference = header.get("preference").map(|v| v.to_es_string());
        out.push(req);
    }
    out
}

/// `SearchRequest.buildDescription`.
pub fn search_description(req: &SearchRequest, dialect: crate::cluster::Dialect) -> String {
    let mut s = String::with_capacity(128);
    s.push_str("indices[");
    s.push_str(&req.indices.join(","));
    s.push_str("], search_type[");
    s.push_str(if req.dfs { "DFS_QUERY_THEN_FETCH" } else { "QUERY_THEN_FETCH" });
    s.push(']');
    if let Some(scroll) = &req.scroll {
        s.push_str(", scroll[");
        s.push_str(&scroll.human());
        s.push(']');
    }
    s.push_str(", source[");
    s.push_str(&source_value(&req.source, dialect).to_json());
    s.push(']');
    if let Some(r) = &req.routing {
        s.push_str(", routing[");
        s.push_str(r);
        s.push(']');
    }
    if let Some(p) = &req.preference {
        s.push_str(", preference[");
        s.push_str(p);
        s.push(']');
    }
    s
}

/// `SearchSourceBuilder.toXContent`: the parsed sections in its order, with the defaults it
/// prints. Sections roll keeps as JSON (aggregations, highlight, suggest, knn, retrievers)
/// print as given.
pub fn source_value(src: &SearchSource, dialect: crate::cluster::Dialect) -> Value {
    let mut m = Map::new();
    let q = |q: &crate::query::Query| q.to_value_in(dialect);
    if let Some(f) = src.from {
        m.insert("from".into(), f.into());
    }
    if let Some(s) = src.size {
        m.insert("size".into(), s.into());
    }
    if let Some(t) = src.timeout.filter(|t| t.duration >= 0) {
        m.insert("timeout".into(), t.string_rep().into());
    }
    if let Some(t) = src.terminate_after {
        m.insert("terminate_after".into(), t.into());
    }
    if let Some(r) = &src.retriever {
        m.insert("retriever".into(), Value::Obj(r.clone()));
    }
    if let Some(query) = &src.query {
        m.insert("query".into(), q(query));
    } else if !src.sub_searches.is_empty() {
        m.insert("sub_searches".into(), Value::Arr(src.sub_searches.iter().map(|s| crate::obj! { "query" => q(s) }).collect()));
    }
    if let Some(p) = &src.post_filter {
        m.insert("post_filter".into(), q(p));
    }
    if !src.knn.is_empty() {
        m.insert("knn".into(), Value::Arr(src.knn.iter().cloned().map(Value::Obj).collect()));
    }
    if let Some(r) = &src.rank {
        m.insert("rank".into(), Value::Obj(r.clone()));
    }
    if let Some(s) = src.min_score {
        m.insert("min_score".into(), crate::query::xparser::float_value(s));
    }
    for (k, v) in [("version", src.version), ("seq_no_primary_term", src.seq_no_primary_term), ("explain", src.explain)] {
        if let Some(v) = v {
            m.insert(k.into(), v.into());
        }
    }
    if src.profile {
        m.insert("profile".into(), true.into());
    }
    if let Some(fs) = &src.source {
        m.insert("_source".into(), fetch_source_value(fs, dialect));
    }
    if let Some(sf) = &src.stored_fields {
        let v = match &sf.fields {
            None => Value::from("_none_"),
            Some(f) if f.len() == 1 => Value::from(f[0].clone()),
            Some(f) => Value::Arr(f.iter().cloned().map(Value::from).collect()),
        };
        m.insert("stored_fields".into(), v);
    }
    let field_and_format = |f: &crate::search_source::FieldAndFormat| {
        let mut o = Map::new();
        o.insert("field".into(), f.field.clone().into());
        if let Some(fmt) = &f.format {
            o.insert("format".into(), fmt.clone().into());
        }
        if let Some(u) = f.include_unmapped {
            o.insert("include_unmapped".into(), u.into());
        }
        Value::Obj(o)
    };
    if !src.docvalue_fields.is_empty() {
        m.insert("docvalue_fields".into(), Value::Arr(src.docvalue_fields.iter().map(field_and_format).collect()));
    }
    if !src.fields.is_empty() {
        m.insert("fields".into(), Value::Arr(src.fields.iter().map(field_and_format).collect()));
    }
    if !src.script_fields.is_empty() {
        let mut o = Map::new();
        for f in &src.script_fields {
            o.insert(f.name.clone(), crate::obj! { "script" => f.script.to_value(), "ignore_failure" => f.ignore_failure });
        }
        m.insert("script_fields".into(), Value::Obj(o));
    }
    if !src.sort.is_empty() {
        m.insert("sort".into(), Value::Arr(src.sort.iter().map(sort_value).collect()));
    }
    if src.track_scores {
        m.insert("track_scores".into(), true.into());
    }
    if let Some(t) = src.track_total_hits {
        let n: i64 = match t {
            TrackTotalHits::Disabled => -1,
            TrackTotalHits::Accurate => i32::MAX as i64,
            TrackTotalHits::UpTo(n) => n as i64,
        };
        m.insert("track_total_hits".into(), n.into());
    }
    if let Some(a) = &src.search_after {
        m.insert("search_after".into(), Value::Arr(a.clone()));
    }
    if let Some(s) = &src.slice {
        let mut o = Map::new();
        if let Some(f) = &s.field {
            o.insert("field".into(), f.clone().into());
        }
        o.insert("id".into(), s.id.into());
        o.insert("max".into(), s.max.into());
        m.insert("slice".into(), Value::Obj(o));
    }
    if !src.indices_boost.is_empty() {
        let v = src.indices_boost.iter().map(|(i, b)| {
            let mut o = Map::new();
            o.insert(i.clone(), crate::query::xparser::float_value(*b));
            Value::Obj(o)
        });
        m.insert("indices_boost".into(), Value::Arr(v.collect()));
    }
    if let Some(a) = &src.aggs {
        m.insert("aggregations".into(), Value::Obj(a.clone()));
    }
    if let Some(h) = &src.highlight {
        m.insert("highlight".into(), Value::Obj(h.clone()));
    }
    if let Some(s) = &src.suggest {
        m.insert("suggest".into(), Value::Obj(s.clone()));
    }
    if !src.rescore.is_empty() {
        let v = src.rescore.iter().map(|r| {
            let mut o = Map::new();
            if let Some(w) = r.window_size {
                o.insert("window_size".into(), w.into());
            }
            match &r.script {
                Some(s) => o.insert("script".into(), crate::obj! { "script" => s.to_value() }),
                None => o.insert(
                    "query".into(),
                    crate::obj! {
                        "rescore_query" => q(&r.query),
                        "query_weight" => crate::query::xparser::float_value(r.query_weight),
                        "rescore_query_weight" => crate::query::xparser::float_value(r.rescore_query_weight),
                        "score_mode" => r.score_mode.clone(),
                    },
                ),
            };
            Value::Obj(o)
        });
        m.insert("rescore".into(), Value::Arr(v.collect()));
    }
    if !src.stats.is_empty() {
        m.insert("stats".into(), Value::Arr(src.stats.iter().cloned().map(Value::from).collect()));
    }
    if let Some(e) = src.ext.as_ref().filter(|e| !e.is_empty()) {
        m.insert("ext".into(), Value::Obj(e.clone()));
    }
    if let Some(c) = &src.collapse {
        let mut o = Map::new();
        o.insert("field".into(), c.field.clone().into());
        match c.inner_hits.len() {
            0 => {}
            1 => {
                o.insert("inner_hits".into(), c.inner_hits[0].clone());
            }
            _ => {
                o.insert("inner_hits".into(), Value::Arr(c.inner_hits.clone()));
            }
        }
        if let Some(n) = c.max_concurrent_group_searches {
            o.insert("max_concurrent_group_searches".into(), n.into());
        }
        m.insert("collapse".into(), Value::Obj(o));
    }
    if let Some(p) = &src.pit {
        let mut o = Map::new();
        o.insert("id".into(), p.id.clone().into());
        if let Some(k) = &p.keep_alive {
            o.insert("keep_alive".into(), k.string_rep().into());
        }
        m.insert("pit".into(), Value::Obj(o));
    }
    if let Some(r) = src.runtime_mappings.as_ref().filter(|r| !r.is_empty()) {
        m.insert("runtime_mappings".into(), Value::Obj(r.clone()));
    }
    Value::Obj(m)
}

/// `FetchSourceContext.toXContent`.
fn fetch_source_value(fs: &FetchSource, dialect: crate::cluster::Dialect) -> Value {
    if !fs.fetch {
        return false.into();
    }
    let mut o = Map::new();
    if let Some(ev) = fs.exclude_vectors {
        o.insert("exclude_vectors".into(), ev.into());
    }
    // OpenSearch's `FetchSourceContext.toXContent` leaves out empty lists.
    let os = dialect == crate::cluster::Dialect::OpenSearch;
    if !os || !fs.includes.is_empty() {
        o.insert("includes".into(), Value::Arr(fs.includes.iter().cloned().map(Value::from).collect()));
    }
    if !os || !fs.excludes.is_empty() {
        o.insert("excludes".into(), Value::Arr(fs.excludes.iter().cloned().map(Value::from).collect()));
    }
    Value::Obj(o)
}

fn order_str(o: SortOrder) -> &'static str {
    match o {
        SortOrder::Asc => "asc",
        SortOrder::Desc => "desc",
    }
}

/// A sort builder's `toXContent`.
fn sort_value(s: &Sort) -> Value {
    match s {
        Sort::Score { order } => crate::obj! { "_score" => crate::obj! { "order" => order_str(*order) } },
        Sort::Field { field, order, missing, unmapped_type, mode, numeric_type, format, .. } => {
            let mut o = Map::new();
            o.insert("order".into(), order_str(order.unwrap_or(SortOrder::Asc)).into());
            if let Some(m) = missing {
                o.insert("missing".into(), m.clone());
            }
            if let Some(u) = unmapped_type {
                o.insert("unmapped_type".into(), u.clone().into());
            }
            if let Some(m) = mode {
                o.insert("mode".into(), m.clone().into());
            }
            if let Some(n) = numeric_type {
                o.insert("numeric_type".into(), n.clone().into());
            }
            if let Some(f) = format {
                o.insert("format".into(), f.clone().into());
            }
            let mut m = Map::new();
            m.insert(field.clone(), Value::Obj(o));
            Value::Obj(m)
        }
        Sort::Script { script, ty, order, mode, .. } => {
            let mut o = Map::new();
            o.insert("script".into(), script.to_value());
            o.insert("type".into(), ty.clone().into());
            o.insert("order".into(), order_str(order.unwrap_or(SortOrder::Asc)).into());
            if let Some(m) = mode {
                o.insert("mode".into(), m.clone().into());
            }
            crate::obj! { "_script" => Value::Obj(o) }
        }
        Sort::GeoDistance(m) => crate::obj! { "_geo_distance" => Value::Obj(m.clone()) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(body: &str) -> SearchSource {
        let v = crate::json::parse(body.as_bytes()).unwrap().unwrap();
        SearchSource::parse(&v, Some(body.as_bytes())).unwrap()
    }

    fn req(indices: &[&str], body: &str) -> SearchRequest {
        SearchRequest {
            indices: indices.iter().map(|s| s.to_string()).collect(),
            opts: crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED,
            source: source(body),
            routing: None,
            preference: None,
            dfs: false,
            request_cache: None,
            total_as_int: false,
            typed_keys: false,
            allow_partial: true,
            scroll: None,
            include_named_queries_score: false,
            force_synthetic_source: false,
            now_ms: 0,
            fused: None,
            rank_only: false,
        }
    }

    /// Descriptions captured from Elasticsearch 9.5.5 (`_tasks?detailed` while each ran).
    #[test]
    fn descriptions_match_elasticsearch() {
        let es = crate::cluster::Dialect::Elasticsearch;
        let script = r#"{"query":{"script_score":{"query":{"match_all":{}},"script":{"source":"long s=0; return s % 7;"}}}}"#;
        assert_eq!(
            search_description(&req(&["slow"], script), es),
            r#"indices[slow], search_type[QUERY_THEN_FETCH], source[{"query":{"script_score":{"query":{"match_all":{"boost":1.0}},"script":{"source":"long s=0; return s % 7;","lang":"painless"}}}}]"#
        );
        let mut scrolled = req(&["slow"], r#"{"size":1,"query":{"match_all":{}}}"#);
        scrolled.scroll = Some(crate::units::TimeValue::parse("1m", "scroll").unwrap());
        assert_eq!(search_description(&scrolled, es), r#"indices[slow], search_type[QUERY_THEN_FETCH], scroll[1m], source[{"size":1,"query":{"match_all":{"boost":1.0}}}]"#);
        // A by-query run's scroll search.
        let dbq = req(&["slow"], r#"{"size":1000,"query":{"match_all":{}},"version":false,"seq_no_primary_term":true,"_source":false,"sort":[{"_doc":{"order":"asc"}}]}"#);
        assert_eq!(
            search_description(&dbq, es),
            r#"indices[slow], search_type[QUERY_THEN_FETCH], source[{"size":1000,"query":{"match_all":{"boost":1.0}},"version":false,"seq_no_primary_term":true,"_source":false,"sort":[{"_doc":{"order":"asc"}}]}]"#
        );
        // `_count`: size 0 and accurate totals, printed as Integer.MAX_VALUE.
        let mut count = req(&["slow"], r#"{"query":{"match_all":{}}}"#);
        count.source.size = Some(0);
        count.source.track_total_hits = Some(TrackTotalHits::Accurate);
        assert_eq!(search_description(&count, es), r#"indices[slow], search_type[QUERY_THEN_FETCH], source[{"size":0,"query":{"match_all":{"boost":1.0}},"track_total_hits":2147483647}]"#);
        assert_eq!(search_description(&req(&[], r#"{"size":0}"#), es), r#"indices[], search_type[QUERY_THEN_FETCH], source[{"size":0}]"#);
    }
}
