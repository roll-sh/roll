//! Parallel document preparation for `_bulk`.
//!
//! Turning an index/create item into an engine document (JSON parse, the mapping walk and the
//! conversion to an `IndexableDoc`) is pure, CPU-bound and about half of a bulk request's
//! server time, while applying it to the shard must stay sequential: Elasticsearch runs a
//! shard's items in request order, so sequence numbers, versions and same-id outcomes follow
//! that order. This module prepares the eligible items ahead of the sequential loop, in
//! parallel on the warm search pool workers (`search_index::search::pool::for_each_index`) and
//! the calling thread; the loop then only routes and applies them.
//!
//! The parallel step never decides anything the sequential path would decide differently. An
//! item qualifies only when nothing before its apply can change or observe it: its target is a
//! concrete index (no alias, data stream, date math or auto-creation), no ingest pipeline runs,
//! the index is not a time series one, and parsing needs no dynamic mapping update. Anything
//! else (including every failure, so error texts come from the one sequential path) is left
//! for the normal path. A prepared item is used only if, when its turn comes, the index
//! metadata is still the very `Arc` it was prepared against: an earlier item's dynamic mapping
//! update, or any settings change, invalidates it and it is prepared again — in parallel again
//! for the rest of the request ([`Ahead::take`]), a bounded number of times.

use std::sync::Arc;

use crate::api::bulk::Item;
use crate::api::document::parse_doc;
use crate::cluster::Cluster;
use crate::engine::convert::{self, DocMeta};
use crate::meta::IndexMeta;
use crate::write::{self, Action, Prepared};

/// A bulk item to prepare: its position and its target index.
type Work<'a> = (usize, &'a Target);

/// An index items are prepared for, with what its documents share.
struct Target {
    idx: Arc<IndexMeta>,
    engine: Arc<crate::engine::IndexEngine>,
    /// The bytes-to-fields fast path for its mapping, when it has one.
    plan: Option<crate::document::fast::FastPlan>,
}

/// Items below this are prepared inline by the normal path: thread start-up would cost more.
const MIN_ITEMS: usize = 64;
/// Items per parallel chunk (and per helper thread at least).
const PER_THREAD: usize = 32;
/// Re-preparations per request after invalidations (each dynamic mapping update causes one).
const MAX_ROUNDS: usize = 4;

/// Prepared documents for a bulk request's items, by item position.
pub struct Ahead {
    slots: Vec<Option<Prepared>>,
    rounds: usize,
}

impl Ahead {
    /// Prepares the eligible items of `items` (`ids[i]`: the id item `i` is written with).
    pub fn new(cluster: &Cluster, items: &[Item], ids: &[Option<String>]) -> Ahead {
        let mut a = Ahead { slots: (0..items.len()).map(|_| None).collect(), rounds: 0 };
        a.fill(cluster, items, ids, 0);
        a
    }

    /// The prepared document of item `i`, if it is still valid for `idx` (the index the item's
    /// target resolves to now). An invalid one triggers a new round for the items after it.
    pub fn take(&mut self, cluster: &Cluster, items: &[Item], ids: &[Option<String>], i: usize, idx: &Arc<IndexMeta>) -> Option<Prepared> {
        let p = self.slots.get_mut(i)?.take()?;
        if Arc::ptr_eq(&p.index, idx) {
            return Some(p);
        }
        // The index changed under the prepared items (a dynamic mapping update): prepare the
        // rest again against the new metadata.
        if self.rounds < MAX_ROUNDS {
            self.rounds += 1;
            self.fill(cluster, items, ids, i);
            if let Some(p) = self.slots[i].take()
                && Arc::ptr_eq(&p.index, idx)
            {
                return Some(p);
            }
        }
        None
    }

    fn fill(&mut self, cluster: &Cluster, items: &[Item], ids: &[Option<String>], from: usize) {
        for s in &mut self.slots[from..] {
            *s = None;
        }
        let st = cluster.state();
        // The indexes of the eligible items (usually one), and (item, index) of each.
        let mut targets: Vec<Target> = Vec::new();
        let mut items_at: Vec<(usize, usize)> = Vec::new();
        for (i, it) in items.iter().enumerate().skip(from) {
            if !matches!(it.action, Action::Index | Action::Create) || ids[i].as_deref().is_none_or(str::is_empty) {
                continue;
            }
            let a = &it.args;
            if a.require_alias || a.require_data_stream || a.dynamic_templates.as_ref().is_some_and(|d| !d.is_empty()) {
                continue;
            }
            if a.pipeline.as_deref().is_some_and(|p| p != "_none") {
                continue;
            }
            let Some(expr) = it.index.as_deref() else { continue };
            let Some(idx) = st.metadata.indices.get(expr) else { continue };
            let t = match targets.iter().position(|t| Arc::ptr_eq(&t.idx, idx)) {
                Some(t) => t,
                None => {
                    if !plain_index(&st, idx, a.pipeline.is_some()) {
                        continue;
                    }
                    let Ok(e) = cluster.engine.index(cluster, &st, idx) else { continue };
                    let plan = if *NO_FAST.get_or_init(|| std::env::var_os("ROLL_PERF_NO_FAST_DOCS").is_some()) { None } else { crate::document::fast::FastPlan::new(idx) };
                    targets.push(Target { idx: idx.clone(), engine: e, plan });
                    targets.len() - 1
                }
            };
            items_at.push((i, t));
        }
        static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        static NO_FAST: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        // `ROLL_PERF_AHEAD_ALL` prepares every eligible item ahead, however small the request
        // (to run the YAML suites through this path and the document fast path).
        static ALL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let all = *ALL.get_or_init(|| std::env::var_os("ROLL_PERF_AHEAD_ALL").is_some());
        if (items_at.len() < MIN_ITEMS && !all) || *DISABLED.get_or_init(|| std::env::var_os("ROLL_PERF_NO_AHEAD").is_some()) {
            return;
        }
        let cores = search_index::search::pool::cores();
        if cores.min(items_at.len() / PER_THREAD) <= 1 && !all {
            return;
        }
        let work: Vec<Work<'_>> = items_at.iter().map(|&(i, t)| (i, &targets[t])).collect();
        // Chunks of `PER_THREAD` items claimed by the calling thread and warm search pool
        // workers (no thread start-up, no fresh per-thread allocator heaps per request); small
        // chunks balance uneven documents.
        let chunks: Vec<&[Work<'_>]> = work.chunks(PER_THREAD).collect();
        let results: Vec<parking_lot::Mutex<Vec<(usize, Prepared)>>> = chunks.iter().map(|_| parking_lot::Mutex::new(Vec::new())).collect();
        // The request's dialect follows the work onto the workers (document grammar differs).
        let opensearch = crate::opensearch::active();
        search_index::search::pool::for_each_index(chunks.len(), cores - 1, &|c| {
            crate::opensearch::with(opensearch, || {
                let part = chunks[c];
                let mut out = Vec::with_capacity(part.len());
                // The mapping context of the chunk's index (building one per document cost
                // more than parsing a small document).
                let mut ctx: Option<(*const Target, crate::mapping::MappingCtx)> = None;
                for &(i, t) in part {
                    let it = &items[i];
                    let id = ids[i].as_deref().unwrap_or_default();
                    let cx = match &mut ctx {
                        Some((p, cx)) if std::ptr::eq(*p, t) => cx,
                        slot => &mut slot.insert((t as *const Target, t.idx.mapping_ctx())).1,
                    };
                    // A panic leaves the item to the sequential path, which reports it.
                    let p = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prepare_one(t, cx, id, it.args.routing.as_deref(), &it.data)));
                    if let Ok(Some(p)) = p {
                        out.push((i, p));
                    }
                }
                *results[c].lock() = out;
            })
        });
        let results = results.into_iter().map(parking_lot::Mutex::into_inner);
        for (i, p) in results.into_iter().flatten() {
            self.slots[i] = Some(p);
        }
    }
}

/// Whether writes to `idx` (named directly) go straight to it: no data stream, no time series,
/// no ingest pipeline (`request_pipeline`: the request names `_none`).
fn plain_index(st: &crate::meta::ClusterState, idx: &IndexMeta, request_pipeline: bool) -> bool {
    if st.metadata.data_stream_of(&idx.name).is_some() || crate::engine::tsdb::is_time_series(idx) || idx.mapping.data_stream_timestamp == Some(true) {
        return false;
    }
    // `_none` on the request skips the default pipeline, never the final one.
    let none = |k: &str| idx.settings.get(k).is_none_or(|p| p == "_none");
    none("index.final_pipeline") && (request_pipeline || none("index.default_pipeline"))
}

/// The engine document of one item, when it needs nothing the sequential path must do first
/// (`write::prepare` without its dynamic-update, vector-dims and percolator branches).
/// `ctx`: the index's mapping context (reused across a chunk's documents).
fn prepare_one(t: &Target, ctx: &mut crate::mapping::MappingCtx, id: &str, routing: Option<&str>, source: &bytes::Bytes) -> Option<Prepared> {
    let (engine, idx) = (&t.engine, &t.idx);
    let table = engine.table();
    if table.key.0 < idx.mapping_version {
        return None;
    }
    // Plain documents of a plain mapping go from bytes to fields directly (`document::fast`);
    // the rest (and every failure) through the general parser.
    let (parsed, stored) = match t.plan.as_ref().and_then(|plan| Some((plan.parse(source)?, plan.source))) {
        Some((parsed, src)) => (parsed, (src == crate::document::fast::SourcePlan::Keep).then(|| source.clone())),
        None => {
            let value = parse_doc(source).ok()?;
            ctx.join_routing = Some(routing.is_some());
            ctx.warnings.borrow_mut().clear();
            let parsed = crate::document::parse_document(&idx.mapping, ctx, &value, Some(source), Some(id)).ok()?;
            if parsed.dynamic_update.is_some() {
                return None;
            }
            for f in parsed.fields.iter().chain(parsed.nested.iter().flat_map(|n| n.fields.iter())) {
                match &f.value {
                    // Vectors may set `dims` on the mapping; percolator queries check the mapping.
                    crate::document::FieldValue::DenseVector(_) | crate::document::FieldValue::ByteVector(_) | crate::document::FieldValue::Percolator(..) => return None,
                    _ => {}
                }
            }
            let stored = write::stored_source(idx, &table, source, &value);
            (parsed, stored)
        }
    };
    let meta = DocMeta { id, routing, source: stored, tsid: None };
    let doc = convert::to_indexable(&table, &parsed, &meta).ok()?;
    let shard_id = crate::meta::routing::shard_for(idx, id, routing);
    let shard = engine.shard(shard_id).clone();
    Some(Prepared { engine: engine.clone(), shard, index: idx.clone(), doc, id: id.to_string(), tsdb: None, source: source.clone(), routing: routing.map(str::to_string) })
}
