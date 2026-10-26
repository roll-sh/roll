//! The `percolate` query (`PercolateQueryBuilder`) over `percolator` fields.
//!
//! Documents of a `percolator` field store a query; the field keeps it (as the JSON of the
//! parsed query) in binary doc values, checked against the mapping when the document is
//! indexed (an unmapped field fails it, as Elasticsearch's strict query parsing does).
//!
//! Percolating indexes the given documents (`document`, `documents`, or an indexed document by
//! `index`/`id`) into a private one-shard in-memory index under the index's mapping (dynamic
//! fields included, for this request only) — Elasticsearch's `MemoryIndex` — then runs every
//! stored query of the shard against it. A stored query matches when it matches any of the
//! documents; its score is its best score there (constant in filter context), and the
//! documents it matched are reported per hit as `_percolator_document_slot[_<name>]`.

use super::query::{QueryCtx, arc, qse};
use crate::cluster::Cluster;
use crate::engine::IndexEngine;
use crate::engine::fields::{FieldTable, Kind};
use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::meta::{ClusterState, IndexMeta};
use crate::query::specialized::Percolate;
use search_index::{QueryRef, ScoreMode};
use std::collections::HashMap;
use std::sync::Arc;

/// A query of a percolator document, checked against the index's mapping.
pub fn check_query(cluster: &Cluster, st: &ClusterState, idx: &IndexMeta, table: &Arc<FieldTable>, engine: &IndexEngine, json: &str) -> EsResult<()> {
    let v = crate::json::parse(json.as_bytes()).map_err(|e| EsError::parse(e.to_string()))?.unwrap_or(Value::Null);
    let (q, _) = crate::query::parse_query(&v, None, &[])?;
    let searcher = engine.shard(0).shard.searcher();
    let qctx = QueryCtx::new(cluster, st, idx, table, &searcher, crate::time::now_millis());
    qctx.strict_unmapped.set(true);
    qctx.to_query(&q)?;
    if let Some(f) = qctx.unmapped.take() {
        return Err(qse(&qctx, format!("No field mapping can be found for the field with name [{f}]")));
    }
    Ok(())
}

/// Documents indexed into a private in-memory index under `idx`'s mapping, with ids `0..n`
/// (their slots). Dynamic fields map for the index built here only.
pub struct MemoryIndex {
    pub meta: Arc<IndexMeta>,
    pub engine: Arc<IndexEngine>,
}

impl MemoryIndex {
    pub fn build(cluster: &Cluster, st: &ClusterState, idx: &IndexMeta, docs: &[Value]) -> EsResult<MemoryIndex> {
        let mut meta = idx.clone();
        meta.settings.set("index.number_of_shards", "1");
        // `MemoryIndex` searchers score with Lucene's plain BM25 (no `k1 + 1` boost).
        if meta.settings.get("index.similarity.default.type").is_none_or(|t| t == "BM25") {
            meta.settings.set("index.similarity.default.legacy", "false");
        }
        let mctx = meta.mapping_ctx();
        let mut parsed = Vec::with_capacity(docs.len());
        for (slot, d) in docs.iter().enumerate() {
            let source = bytes::Bytes::from(d.to_json());
            let id = slot.to_string();
            let mut done = None;
            for _ in 0..8 {
                let p = crate::document::parse_document(&meta.mapping, &mctx, d, Some(&source), Some(&id))?;
                match &p.dynamic_update {
                    Some(u) => meta.mapping = Arc::new(meta.mapping.merge(u, &mctx, crate::mapping::MergeReason::MappingUpdate)?),
                    None => {
                        done = Some(p);
                        break;
                    }
                }
            }
            parsed.push((id, source, done.ok_or_else(|| EsError::illegal_state("mapping updates did not converge"))?));
        }
        let meta = Arc::new(meta);
        let private = crate::engine::Engine::with_storage(Arc::new(crate::engine::MemoryShards));
        let engine = private.index(cluster, st, &meta)?;
        let table = engine.table();
        let shard = engine.shard(0).clone();
        for ((id, source, p), d) in parsed.into_iter().zip(docs) {
            let stored = crate::write::stored_source(&meta, &table, &source, d);
            let doc = crate::engine::convert::to_indexable(&table, &p, &crate::engine::convert::DocMeta { id: &id, routing: None, tsid: None, source: stored })?;
            shard.shard.index(search_index::IndexOp::new(doc)).map_err(crate::engine::convert::index_error)?;
        }
        shard.wrote();
        shard.refresh()?;
        Ok(MemoryIndex { meta, engine })
    }
}

/// What a `percolate` query matched on a shard, kept for the fetch phase.
pub struct Percolated {
    pub name: Option<String>,
    /// Per matching stored-query doc: the slots of the documents it matched.
    pub slots: HashMap<u32, Vec<u32>>,
    /// Per matching doc: per slot, the stored query's named queries that matched it.
    pub named: HashMap<u32, Vec<(u32, Vec<String>)>>,
    queries: HashMap<u32, crate::query::Query>,
    mem: MemoryIndex,
    docs: Vec<Value>,
    /// Memory index doc -> slot.
    slot_of: HashMap<u32, u32>,
}

impl std::fmt::Debug for Percolated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Percolated").field("name", &self.name).field("matches", &self.slots.len()).finish()
    }
}

impl Percolated {
    /// `PercolatorHighlightSubFetchPhase`: the documents the stored query of `doc` matched,
    /// highlighted with that query; with several documents, fields are prefixed by the slot.
    pub fn highlight(&self, cluster: &Cluster, st: &ClusterState, spec: &super::highlight::Spec, doc: u32, now: i64) -> EsResult<crate::json::Map> {
        let mut out = crate::json::Map::new();
        let (Some(slots), Some(q)) = (self.slots.get(&doc), self.queries.get(&doc)) else { return Ok(out) };
        let table = self.mem.engine.table();
        let searcher = self.mem.engine.shard(0).shard.searcher();
        let qctx = QueryCtx::new(cluster, st, &self.mem.meta, &table, &searcher, now);
        let query = qctx.to_query(q)?;
        for slot in slots {
            let Some((&mdoc, _)) = self.slot_of.iter().find(|(_, s)| *s == slot) else { continue };
            let Some(source) = self.docs.get(*slot as usize) else { continue };
            let hl = super::highlight::highlight_hit(&qctx, spec, &query, Some(source), &slot.to_string(), &searcher, Some(mdoc))?;
            for (k, v) in hl {
                let key = if self.docs.len() == 1 { k } else { format!("{slot}_{k}") };
                out.insert(key, v);
            }
        }
        Ok(out)
    }
}

/// The source of the document a `percolate` query names by `index` / `id`.
fn indexed_document(ctx: &QueryCtx, p: &Percolate) -> EsResult<Value> {
    let index = p.index.clone().unwrap_or_default();
    let id = p.id.clone().unwrap_or_default();
    let missing = || EsError::resource_not_found(format!("indexed document [{index}/{id}] couldn't be found"));
    let Some(meta) = crate::write::resolve_existing(ctx.st, &index) else { return Err(EsError::index_not_found(&index)) };
    let engine = ctx.cluster.engine.index(ctx.cluster, ctx.st, &meta)?;
    let shard = crate::meta::routing::shard_for(&meta, &id, p.routing.as_deref());
    let doc = engine.shard(shard).shard.get(id.as_bytes(), true).map_err(crate::engine::convert::index_error)?.ok_or_else(missing)?;
    if p.version.is_some_and(|v| v != doc.version) {
        return Err(EsError::new(409, "version_conflict_engine_exception", format!("[{id}]: version conflict, current version [{}] is different than the one provided [{}]", doc.version, p.version.unwrap_or(0))));
    }
    let src = crate::fetch::source_bytes(&doc.fields).ok_or_else(missing)?;
    crate::fetch::parse_source(src)
}

/// `PercolateQueryBuilder.doToQuery`: the stored queries matching the documents.
pub fn percolate(ctx: &QueryCtx, p: &Percolate) -> EsResult<QueryRef> {
    let Some(f) = ctx.table.get(&p.field).cloned() else {
        return Err(qse(ctx, format!("field [{}] does not exist", p.field)));
    };
    if f.kind != Kind::Percolator {
        return Err(qse(ctx, format!("expected field [{}] to be of type [percolator], but is of type [{}]", p.field, f.ty)));
    }
    let docs: Vec<Value> = if p.documents.is_empty() && p.id.is_some() { vec![indexed_document(ctx, p)?] } else { p.documents.clone() };
    if docs.is_empty() {
        return Ok(super::query::match_none("no documents to percolate"));
    }
    let mem = MemoryIndex::build(ctx.cluster, ctx.st, ctx.idx, &docs)?;
    let mem_table = mem.engine.table();
    let mem_searcher = mem.engine.shard(0).shard.searcher();
    let root = search_index::query::nested::root_filter();
    // Memory doc -> slot (root documents carry their slot as `_id`).
    let mut slot_of: HashMap<u32, u32> = HashMap::new();
    for seg in mem_searcher.segments() {
        for d in 0..seg.reader.max_doc() {
            if let Ok(stored) = mem_searcher.stored(seg.base + d)
                && let Some(id) = super::hits::stored_id(&stored)
                && let Ok(s) = id.parse::<u32>()
            {
                slot_of.insert(seg.base + d, s);
            }
        }
    }
    let ie = crate::engine::convert::index_error;
    let mut matches: Vec<(u32, f32)> = Vec::new();
    let mut slots: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut queries: HashMap<u32, crate::query::Query> = HashMap::new();
    let mut named: HashMap<u32, Vec<(u32, Vec<String>)>> = HashMap::new();
    // Stored queries whose extracted terms the documents lack can't match: verify only the
    // candidates (see `percolate_candidates`).
    let all = super::percolate_candidates::adds_fields(ctx.table, &mem_table);
    let key = candidates_key(ctx, f.number);
    for seg in ctx.searcher.segments() {
        if seg.reader.binary(f.number).is_none() {
            continue;
        }
        let stored = match seg.core.cache_get::<super::percolate_candidates::StoredQueries>("percolator", key) {
            Some(s) => s,
            None => {
                let s = Arc::new(super::percolate_candidates::StoredQueries::build(ctx, seg, f.number));
                seg.core.cache_put("percolator", key, s.clone(), s.heap_bytes(), 4);
                s
            }
        };
        for i in stored.candidates(&mem_searcher, all) {
            let entry = &stored.queries[i as usize];
            let d = entry.doc;
            if !seg.is_live(d) {
                continue;
            }
            let parsed = entry.query.clone();
            let qctx = QueryCtx::new(ctx.cluster, ctx.st, &mem.meta, &mem_table, &mem_searcher, ctx.now_ms);
            let Ok(stored_q) = qctx.to_query(&parsed) else { continue };
            let q: QueryRef = arc(search_index::query::BooleanQuery::new().add(search_index::query::Occur::Must, stored_q).add(search_index::query::Occur::Filter, root.clone()));
            let w = mem_searcher.create_weight(&q, ScoreMode::Complete).map_err(ie)?;
            let mut best: Option<f32> = None;
            let mut hit_slots = Vec::new();
            let mut hit_docs = Vec::new();
            for mseg in mem_searcher.segments() {
                if let Some(mut s) = w.scorer(mseg).map_err(ie)? {
                    let mut md = s.next();
                    while md != search_codec::NO_MORE_DOCS {
                        let score = search_index::Scorable::score(&mut *s);
                        best = Some(best.map_or(score, |b| b.max(score)));
                        if let Some(slot) = slot_of.get(&(mseg.base + md)) {
                            hit_slots.push(*slot);
                            hit_docs.push((*slot, mseg.base + md));
                        }
                        md = s.next();
                    }
                }
            }
            if let Some(score) = best {
                // Named queries of the stored query, per matched document.
                let names = qctx.named.take();
                if !names.is_empty() {
                    hit_docs.sort_unstable();
                    let mut per_slot = Vec::new();
                    for (slot, mdoc) in &hit_docs {
                        let mut matched: Vec<String> = Vec::new();
                        for (n, nq) in &names {
                            if !matched.contains(n) && mem_searcher.explain(nq, *mdoc).is_ok_and(|e| e.is_match()) {
                                matched.push(n.clone());
                            }
                        }
                        if !matched.is_empty() {
                            per_slot.push((*slot, matched));
                        }
                    }
                    named.insert(seg.base + d, per_slot);
                }
                hit_slots.sort_unstable();
                matches.push((seg.base + d, score));
                slots.insert(seg.base + d, hit_slots);
                queries.insert(seg.base + d, parsed);
            }
        }
    }
    ctx.percolate.borrow_mut().push(Arc::new(Percolated { name: p.name.clone(), slots, named, queries, mem, docs, slot_of }));
    if matches.is_empty() {
        return Ok(super::query::match_none("no stored query matches"));
    }
    Ok(arc(search_index::query::ScoreDocQuery::lucene(matches)))
}

/// The segment cache key of a percolator field's stored queries: their conversion depends on
/// the index's mapping, analysis settings and synonym sets.
fn candidates_key(ctx: &QueryCtx, field: u32) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (field, &ctx.idx.uuid, ctx.idx.mapping_version, ctx.idx.settings_version, ctx.st.metadata.synonyms_version).hash(&mut h);
    h.finish()
}

/// The coordinator's rewrite of `percolate` queries naming an indexed document
/// (`PercolateQueryBuilder.doRewrite`): a missing document fails the request itself.
pub fn check_indexed_documents(ctx: &crate::rest::Ctx, q: &crate::query::Query) -> EsResult<()> {
    fn walk(v: &Value, out: &mut Vec<crate::json::Map>) {
        match v {
            Value::Obj(m) => {
                for (k, x) in m {
                    if k == "percolate"
                        && let Value::Obj(p) = x
                        && p.contains_key("id")
                    {
                        out.push(p.clone());
                    }
                    walk(x, out);
                }
            }
            Value::Arr(a) => a.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    walk(&q.to_value(), &mut found);
    for p in found {
        let index = p.get("index").map(Value::to_es_string).unwrap_or_default();
        let id = p.get("id").map(Value::to_es_string).unwrap_or_default();
        let idx = crate::api::document::single_index(ctx, &index)?;
        let args = crate::api::document::GetArgs { routing: p.get("routing").map(Value::to_es_string), realtime: true, ..Default::default() };
        if crate::api::document::get_one(ctx, &idx, &id, &args)?.is_none() {
            return Err(EsError::resource_not_found(format!("indexed document [{index}/{id}] couldn't be found")));
        }
    }
    Ok(())
}
