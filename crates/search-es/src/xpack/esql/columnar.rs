//! The direct columnar source of `FROM`: reads the needed fields straight from the shards' doc
//! values, in batches of rows, instead of going through the search API's `fields` (which parses
//! `_source` per hit and renders every value as JSON first).
//!
//! The query (the pushed-down leading `WHERE` plus the request `filter`) is built per shard
//! exactly as a search builds it (alias filters, the nested root filter), the matching docs
//! are collected per segment, and each batch of docs is decoded column by column with the
//! codec's batch readers (`get_batch` / `ords_batch`: one dispatch per batch). Keyword ordinals
//! of low-cardinality segments are decoded once and reused.
//!
//! Values come out exactly as Elasticsearch's block loaders produce them: multi-values in
//! doc-values order (ascending, deduplicated for keyword/ip/version — version by precedence),
//! floats widened from their stored bits (`float`, `half_float`), scaled floats divided by the
//! scaling factor. Text fields and fields without doc values are read from the stored
//! `_source` with the search API's `fields` extraction (values in source order), once per doc
//! for all such columns. [`plan`] declines (and the caller falls back to the search API)
//! whenever a needed column isn't plainly readable: `ignore_above` keywords, runtime fields,
//! `date_nanos`, `unsigned_long`, other field types, `_source`-read columns of indices whose
//! source is synthetic or disabled, metadata other than `_index` and `_version`, full-text
//! functions, strict-columnar indices, remote indices.
//!
//! [`scan_top`] runs `SORT ... | LIMIT n` the way Elasticsearch pushes a top-n into Lucene: one
//! pass over the key columns keeps the best `n` docs, a second reads only those docs whole.

use super::exec::{Col, Stats};
use super::value::{DataType, V};
use crate::cluster::Cluster;
use crate::engine::fields::{FieldInfo, Kind};
use crate::error::EsResult;
use crate::json::Value;
use crate::search::Target;
use search_codec::points::encode;
use search_index::{Collector, LeafCollector, QueryRef, Scorable, ScoreMode, SegmentContext};
use std::sync::Arc;

/// Docs decoded per batch handed to the pipeline.
const BATCH: usize = 1024;
/// Segments whose keyword dictionaries are at most this big decode each ordinal once.
const ORD_CACHE_MAX: u64 = 1 << 16;

/// How one column's doc values turn into values.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Conv {
    Bool,
    Int,
    Long,
    Double,
    Float,
    Half,
    /// `scaled_float`: the stored long divided by the factor (`ScaledFloatFieldMapper`'s block
    /// loader).
    Scaled(f64),
    Date,
    Keyword,
    Version,
    Ip,
}

/// One needed column of one target index.
#[derive(Clone, Debug)]
enum ColSpec {
    /// Not mapped in this index (or not needed): always null.
    Null,
    Const(V),
    /// The engine field and its conversion.
    Dv(u32, Conv),
    /// `_index`.
    Index,
    /// `_version`.
    Version,
    /// Read from the stored `_source` as the search API's `fields` reads it (text, fields
    /// without doc values): the field and its `fields` request.
    Fetch(String, DataType),
}

/// A planned direct scan: per target shard the column specs, in row order.
pub(super) struct Plan {
    targets: Vec<(Target, Arc<Vec<ColSpec>>)>,
    width: usize,
}

/// The conversion for a field as `ty` (the column type the query resolved), or `None` when the
/// field can't be read from doc values alone.
fn conv_of(table: &crate::engine::fields::FieldTable, f: &FieldInfo, ty: DataType, stored_source: bool) -> Option<ColSpec> {
    if f.flattened_key.is_some() || f.nested_path.is_some() {
        return None;
    }
    // Counters read as the numbers they are.
    let ty = match ty {
        DataType::CounterLong => DataType::Long,
        DataType::CounterInteger => DataType::Integer,
        DataType::CounterDouble => DataType::Double,
        t => t,
    };
    if matches!(f.kind, Kind::Text | Kind::MatchOnlyText)
        && ty == DataType::Text
        && let Some(spec) = text_delegate(table, f)
    {
        return spec;
    }
    if f.kind == Kind::ConstantKeyword {
        return (ty == DataType::Keyword).then(|| f.constant_value.clone().map_or(ColSpec::Null, |c| ColSpec::Const(V::Str(c))));
    }
    let from_source = match (f.kind, ty) {
        (Kind::Text | Kind::MatchOnlyText, DataType::Text) => true,
        (Kind::Keyword, DataType::Keyword) => !f.doc_values && f.ignore_above.is_none(),
        (Kind::GeoPoint, DataType::GeoPoint) | (Kind::Point, DataType::CartesianPoint) | (Kind::GeoShape, DataType::GeoShape) | (Kind::Shape, DataType::CartesianShape) => true,
        (Kind::Long, DataType::Long) | (Kind::Integer | Kind::Short | Kind::Byte, DataType::Integer) | (Kind::Double, DataType::Double) | (Kind::Boolean, DataType::Boolean) | (Kind::Date, DataType::Date) | (Kind::Ip, DataType::Ip) => !f.doc_values,
        _ => false,
    };
    if from_source {
        return stored_source.then(|| ColSpec::Fetch(f.name.clone(), ty));
    }
    if !f.doc_values {
        return None;
    }
    let conv = match (f.kind, ty) {
        (Kind::Boolean, DataType::Boolean) => Conv::Bool,
        (Kind::Integer | Kind::Short | Kind::Byte, DataType::Integer) => Conv::Int,
        (Kind::Long, DataType::Long) => Conv::Long,
        (Kind::Double, DataType::Double) => Conv::Double,
        (Kind::Float, DataType::Double) => Conv::Float,
        (Kind::HalfFloat, DataType::Double) => Conv::Half,
        (Kind::ScaledFloat(sf), DataType::Double) => Conv::Scaled(sf),
        (Kind::Date, DataType::Date) => Conv::Date,
        (Kind::Keyword, DataType::Keyword) if f.ignore_above.is_none() => Conv::Keyword,
        (Kind::Version, DataType::Version) => Conv::Version,
        (Kind::Ip, DataType::Ip) => Conv::Ip,
        _ => return None,
    };
    Some(ColSpec::Dv(f.number, conv))
}

/// How Elasticsearch's `TextFieldMapper.blockLoader` reads a text field through doc values:
/// its own (`doc_values: true`), else an unstored text field loads its first keyword multi-field without a normalizer (doc
/// values: sorted, deduplicated) unless that sets `ignore_above`; a text multi-field of a
/// keyword without a normalizer loads the keyword. `Some(None)`: a keyword this source
/// can't read (stored-only, `ignore_above` on the parent), so the search API reads it;
/// `None`: no keyword applies (the text comes from `_source`).
fn text_delegate(table: &crate::engine::fields::FieldTable, f: &FieldInfo) -> Option<Option<ColSpec>> {
    let usable = |k: &FieldInfo| k.kind == Kind::Keyword && k.normalizer.is_none() && (k.doc_values || k.store);
    // Text with its own doc values (`doc_values: true`) reads them.
    if f.doc_values {
        return Some(Some(ColSpec::Dv(f.number, Conv::Keyword)));
    }
    if !f.store {
        let mut subs: Vec<&std::sync::Arc<FieldInfo>> = table.fields.values().filter(|k| k.parent.as_deref() == Some(f.name.as_str()) && usable(k)).collect();
        subs.sort_by(|a, b| a.name.cmp(&b.name));
        if let Some(k) = subs.first()
            && k.ignore_above.is_none()
        {
            return Some(k.doc_values.then_some(ColSpec::Dv(k.number, Conv::Keyword)));
        }
    }
    let parent = f.parent.as_deref().and_then(|p| table.get(p)).filter(|k| usable(k))?;
    Some((parent.doc_values && parent.ignore_above.is_none()).then_some(ColSpec::Dv(parent.number, Conv::Keyword)))
}

/// Plans a direct scan of `index` for `cols` (`fields`: the source columns by position, then
/// `metadata`), or `None` when some needed column needs the search API.
#[allow(clippy::too_many_arguments)]
pub(super) fn plan(
    cluster: &Cluster,
    index: &str,
    cols: &[Col],
    fields: &[String],
    metadata: &[String],
    needed: Option<&std::collections::HashSet<String>>,
    hidden: usize,
    now_ms: i64,
) -> Option<Plan> {
    if hidden > 0 || index.contains(':') || metadata.iter().any(|m| m != "_index" && m != "_version") {
        return None;
    }
    let st = cluster.state();
    let req = crate::search::SearchRequest {
        indices: crate::meta::resolve::split_expressions(index),
        opts: crate::meta::resolve::IndicesOptions { ignore_unavailable: true, ..crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED },
        source: crate::search_source::SearchSource::default(),
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
        now_ms,
        fused: None,
        rank_only: false,
        security: None,
    };
    let targets = crate::search::shard_targets(cluster, &st, &req).ok()?;
    let mut by_index: Vec<(String, Arc<Vec<ColSpec>>)> = Vec::new();
    let mut out = Vec::with_capacity(targets.len());
    for t in targets {
        let specs = match by_index.iter().find(|(n, _)| *n == t.idx.name) {
            Some((_, s)) => s.clone(),
            None => {
                let mode = t.idx.settings.get("index.mode").and_then(crate::meta::index_mode::IndexMode::parse);
                if mode.is_some_and(|m| m.is_strict_columnar()) {
                    return None;
                }
                let table = t.table();
                let stored_source = crate::engine::synthetic::source_mode(&t.idx) == crate::engine::synthetic::SourceMode::Stored;
                let mut specs = Vec::with_capacity(cols.len());
                for (i, name) in fields.iter().enumerate() {
                    if needed.is_some_and(|n| !n.contains(name)) || cols[i].ty == DataType::Null {
                        specs.push(ColSpec::Null);
                        continue;
                    }
                    match table.get(name) {
                        // An `unsupported` column (a union type among them) reads what this
                        // index maps, for the conversions that resolve it.
                        Some(f) if cols[i].ty == DataType::Unsupported => match DataType::of_mapping(&f.ty) {
                            DataType::Unsupported => specs.push(ColSpec::Null),
                            native => specs.push(conv_of(&table, f, native, stored_source)?),
                        },
                        Some(f) => specs.push(conv_of(&table, f, cols[i].ty, stored_source)?),
                        // Runtime fields are computed by the search API.
                        None if table.mapping.runtime.contains_key(name.as_str()) => return None,
                        None => specs.push(ColSpec::Null),
                    }
                }
                for m in metadata {
                    specs.push(if m == "_index" { ColSpec::Index } else { ColSpec::Version });
                }
                let specs = Arc::new(specs);
                by_index.push((t.idx.name.clone(), specs.clone()));
                specs
            }
        };
        out.push((t, specs));
    }
    Some(Plan { targets: out, width: fields.len() + metadata.len() })
}

/// Room a row keeps beyond its columns for the values `EVAL`s append, so they don't move it.
const ROW_SPARE: usize = 4;

fn new_row(width: usize) -> Vec<V> {
    let mut r = Vec::with_capacity(width + ROW_SPARE);
    r.resize(width, V::Null);
    r
}

/// Collects the matching docs of every segment (`limit`: at most that many in all).
struct Docs {
    per_seg: Vec<(u32, Vec<u32>)>,
    limit: Option<usize>,
    total: usize,
}

struct DocsLeaf<'a> {
    out: &'a mut Vec<u32>,
    total: &'a mut usize,
    limit: Option<usize>,
}

impl LeafCollector for DocsLeaf<'_> {
    fn collect(&mut self, doc: u32, _: &mut dyn Scorable) -> search_index::Result<()> {
        if self.limit.is_some_and(|l| *self.total >= l) {
            return Err(search_index::Error::CollectionTerminated);
        }
        self.out.push(doc);
        *self.total += 1;
        Ok(())
    }

    fn collect_block(&mut self, docs: &[u32], _: f32) -> search_index::Result<()> {
        let room = self.limit.map_or(docs.len(), |l| l.saturating_sub(*self.total).min(docs.len()));
        self.out.extend_from_slice(&docs[..room]);
        *self.total += room;
        if room < docs.len() {
            return Err(search_index::Error::CollectionTerminated);
        }
        Ok(())
    }
}

impl Collector for Docs {
    fn score_mode(&self) -> ScoreMode {
        ScoreMode::CompleteNoScores
    }

    fn leaf<'a>(&'a mut self, seg: &SegmentContext) -> search_index::Result<Box<dyn LeafCollector + 'a>> {
        if self.limit.is_some_and(|l| self.total >= l) {
            return Err(search_index::Error::CollectionTerminated);
        }
        self.per_seg.push((seg.ord, Vec::new()));
        let out = &mut self.per_seg.last_mut().expect("pushed").1;
        Ok(Box::new(DocsLeaf { out, total: &mut self.total, limit: self.limit }))
    }
}

/// Per segment, per column: what the column reads.
enum Reader<'r> {
    Null,
    Const(V),
    Index(&'r str),
    Version,
    Nums(&'r search_codec::SortedNumericDocValues, Conv),
    /// Filled by [`fill_fetched`].
    Fetch,
    Ords(&'r search_codec::SortedSetDocValues, Conv, OrdCache),
}

/// Decoded ordinals of a small dictionary. The table is allocated only once a segment has
/// decoded [`ORD_CACHE_AFTER`] values: a scan that reads a handful of docs (`LIMIT 1`, the
/// top-n second pass) decodes them directly instead of zeroing a 64k-entry table per
/// segment and column.
struct OrdCache {
    slots: Vec<Option<V>>,
    /// The dictionary size when it is small enough to cache (0: never cached).
    len: usize,
    /// Values decoded without the table so far.
    direct: u32,
}

/// Uncached decodes before a segment's table is built.
const ORD_CACHE_AFTER: u32 = 64;

impl OrdCache {
    fn new(len: u64) -> OrdCache {
        OrdCache { slots: Vec::new(), len: if len <= ORD_CACHE_MAX { len as usize } else { 0 }, direct: 0 }
    }

    /// The table slot of `ord`, once the table is worth building.
    fn slot(&mut self, ord: u64) -> Option<&mut Option<V>> {
        if self.slots.is_empty() {
            if self.len == 0 || self.direct < ORD_CACHE_AFTER {
                self.direct += 1;
                return None;
            }
            self.slots = vec![None; self.len];
        }
        self.slots.get_mut(ord as usize)
    }
}

fn num(conv: Conv, v: i64) -> V {
    match conv {
        Conv::Bool => V::Bool(v != 0),
        Conv::Int => V::Int(v as i32),
        Conv::Double => V::Double(encode::sortable_f64(v)),
        Conv::Float => V::Double(encode::sortable_f32(v as i32) as f64),
        Conv::Half => V::Double(encode::sortable_f16(v as i16) as f64),
        Conv::Scaled(sf) => V::Double(v as f64 / sf),
        Conv::Date => V::Date(v),
        _ => V::Long(v),
    }
}

fn term(conv: Conv, bytes: &[u8]) -> V {
    match conv {
        Conv::Ip => V::Ip(search_index::query::range::inet_to_string(bytes)),
        // `version` doc values hold the precedence-ordered encoding.
        Conv::Version => V::Str(search_aggs::version_string::decode(bytes)),
        _ => V::Str(String::from_utf8_lossy(bytes).into_owned()),
    }
}

/// Scratch buffers reused across batches.
#[derive(Default)]
struct Scratch {
    found: Vec<u32>,
    counts: Vec<u32>,
    longs: Vec<i64>,
    ords: Vec<u64>,
    bytes: Vec<u8>,
}

fn many(mut vals: Vec<V>, conv: Conv) -> V {
    if conv == Conv::Version {
        vals.sort_by(|a, b| match (a, b) {
            (V::Str(x), V::Str(y)) => super::functions::version_cmp(x, y),
            _ => std::cmp::Ordering::Equal,
        });
        vals.dedup();
    }
    match vals.len() {
        0 => V::Null,
        1 => vals.pop().expect("one"),
        _ => V::Multi(vals),
    }
}

/// The value of ordinal `ord`, decoded once per segment for small dictionaries.
fn ord_value(dv: &search_codec::SortedSetDocValues, conv: Conv, cache: &mut OrdCache, ord: u64, bytes: &mut Vec<u8>) -> V {
    if let Some(slot) = cache.slot(ord) {
        if let Some(v) = slot {
            return v.clone();
        }
        bytes.clear();
        dv.lookup_ord(ord, bytes);
        let v = term(conv, bytes);
        *slot = Some(v.clone());
        return v;
    }
    bytes.clear();
    dv.lookup_ord(ord, bytes);
    term(conv, bytes)
}

/// Fills column `c` of `rows` (one per doc of `docs`); returns the values loaded.
fn fill(r: &mut Reader<'_>, seg: &SegmentContext, docs: &[u32], rows: &mut [Vec<V>], c: usize, sc: &mut Scratch) -> i64 {
    let mut loaded = 0;
    match r {
        Reader::Null | Reader::Fetch => {}
        Reader::Const(v) => {
            for row in rows.iter_mut() {
                row[c] = v.clone();
            }
            loaded = rows.len() as i64;
        }
        Reader::Index(name) => {
            for row in rows.iter_mut() {
                row[c] = V::Str((*name).to_string());
            }
            loaded = rows.len() as i64;
        }
        Reader::Version => {
            for (row, d) in rows.iter_mut().zip(docs) {
                row[c] = V::Long(crate::search::hits::meta_dv(seg, *d, search_index::schema::VERSION_FIELD));
            }
            loaded = rows.len() as i64;
        }
        Reader::Nums(dv, conv) => {
            sc.found.clear();
            sc.counts.clear();
            sc.longs.clear();
            dv.get_batch(docs, &mut sc.found, &mut sc.counts, &mut sc.longs);
            let (mut di, mut vi) = (0usize, 0usize);
            for (fi, &doc) in sc.found.iter().enumerate() {
                while docs[di] != doc {
                    di += 1;
                }
                let n = sc.counts[fi] as usize;
                let vals = &sc.longs[vi..vi + n];
                vi += n;
                rows[di][c] = match vals {
                    [] => V::Null,
                    [one] => num(*conv, *one),
                    _ => V::Multi(vals.iter().map(|v| num(*conv, *v)).collect()),
                };
                loaded += 1;
            }
        }
        Reader::Ords(dv, conv, cache) => {
            sc.found.clear();
            sc.counts.clear();
            sc.ords.clear();
            dv.ords_batch(docs, &mut sc.found, &mut sc.counts, &mut sc.ords);
            let (mut di, mut oi) = (0usize, 0usize);
            for (fi, &doc) in sc.found.iter().enumerate() {
                while docs[di] != doc {
                    di += 1;
                }
                let n = sc.counts[fi] as usize;
                let (start, end) = (oi, oi + n);
                oi = end;
                rows[di][c] = match n {
                    0 => V::Null,
                    1 => ord_value(dv, *conv, cache, sc.ords[start], &mut sc.bytes),
                    _ => {
                        let vals: Vec<V> = (start..end).map(|i| ord_value(dv, *conv, cache, sc.ords[i], &mut sc.bytes)).collect();
                        many(vals, *conv)
                    }
                };
                loaded += 1;
            }
        }
    }
    loaded
}

/// A planned target with its searcher and shard query.
type Prepared = (Target, Arc<Vec<ColSpec>>, search_index::Searcher, QueryRef);

/// Builds every shard's query first, so a failure falls back before any row is emitted.
/// `None`: a shard query could not be built here (the caller falls back to the search API,
/// which reports the error as a search does).
fn prepare(cluster: &Cluster, targets: Vec<(Target, Arc<Vec<ColSpec>>)>, query: Option<&Value>, now_ms: i64) -> Option<Vec<Prepared>> {
    let parsed = match query {
        Some(q) => Some(crate::query::parse_query(q, None, &[]).ok()?.0),
        None => None,
    };
    let st = cluster.state();
    let mut prepared = Vec::with_capacity(targets.len());
    for (t, specs) in targets {
        if cluster.shards_failed(&st, &t.idx) {
            continue;
        }
        let searcher = t.searcher().ok()?;
        let table = t.table();
        let q = {
            let qctx = crate::search::query::QueryCtx::new(cluster, &st, &t.idx, &table, &searcher, now_ms);
            let src = crate::search_source::SearchSource { query: parsed.clone(), ..Default::default() };
            crate::search::shard_query(&qctx, &t, &src).ok()?
        };
        prepared.push((t, specs, searcher, q));
    }
    Some(prepared)
}

/// The matching docs of one prepared shard, per segment.
fn collect(p: &Prepared, limit: Option<usize>) -> EsResult<Docs> {
    let mut docs = Docs { per_seg: Vec::new(), limit, total: 0 };
    match p.2.search(&p.3, &mut docs) {
        Ok(()) => {}
        Err(e) if e.is_terminated() => {}
        Err(e) => return Err(crate::engine::convert::index_error(e)),
    }
    Ok(docs)
}

/// The readers of one segment (`only`: just these columns, the rest read as null).
fn readers<'r>(specs: &[ColSpec], seg: &'r SegmentContext, index: &'r str, only: Option<&[usize]>) -> Vec<Reader<'r>> {
    specs
        .iter()
        .enumerate()
        .map(|(c, s)| match s {
            _ if only.is_some_and(|o| !o.contains(&c)) => Reader::Null,
            ColSpec::Null => Reader::Null,
            ColSpec::Const(v) => Reader::Const(v.clone()),
            ColSpec::Index => Reader::Index(index),
            ColSpec::Version => Reader::Version,
            ColSpec::Fetch(..) => Reader::Fetch,
            ColSpec::Dv(n, conv @ (Conv::Keyword | Conv::Version | Conv::Ip)) => match seg.reader.sorted_set(*n) {
                Some(dv) => Reader::Ords(dv, *conv, OrdCache::new(dv.value_count())),
                None => Reader::Null,
            },
            ColSpec::Dv(n, conv) => match seg.reader.sorted_numeric(*n) {
                Some(dv) => Reader::Nums(dv, *conv),
                None => Reader::Null,
            },
        })
        .collect()
}

/// The `_source` columns of a target (`only`: just these), and the `fields` request reading them.
struct Fetched {
    cols: Vec<(usize, String, DataType)>,
    specs: Vec<crate::search_source::FieldAndFormat>,
}

impl Fetched {
    fn new(specs: &[ColSpec], only: Option<&[usize]>) -> Fetched {
        let mut f = Fetched { cols: Vec::new(), specs: Vec::new() };
        for (c, s) in specs.iter().enumerate() {
            if let ColSpec::Fetch(name, ty) = s
                && only.is_none_or(|o| o.contains(&c))
            {
                f.cols.push((c, name.clone(), *ty));
                // Dates come back as the search API's `epoch_millis`, as the fallback asks.
                let format = (*ty == DataType::Date).then(|| "epoch_millis".to_string());
                f.specs.push(crate::search_source::FieldAndFormat { field: name.clone(), format, include_unmapped: None });
            }
        }
        f
    }

    /// Fills the `_source` columns of `rows` (one per doc of `docs`); returns the values loaded.
    fn fill(&self, t: &Target, searcher: &search_index::Searcher, seg: &SegmentContext, docs: &[u32], rows: &mut [Vec<V>]) -> EsResult<i64> {
        if self.cols.is_empty() {
            return Ok(0);
        }
        let table = t.table();
        let mut loaded = 0;
        let (mut out, mut ignored) = (crate::json::Map::new(), crate::json::Map::new());
        for (row, &d) in rows.iter_mut().zip(docs) {
            let stored = searcher.stored(seg.base + d).map_err(crate::engine::convert::index_error)?;
            let Some(bytes) = crate::fetch::source_bytes(&stored) else { continue };
            let source = crate::fetch::parse_source(bytes)?;
            out.clear();
            ignored.clear();
            crate::search::fields::fetch_fields(&table, Some(&source), &self.specs, &mut out, &mut ignored, seg, d)?;
            for (c, name, ty) in &self.cols {
                // Values without doc values keep the source's order.
                let v = out.get(name.as_str()).map_or(V::Null, |v| super::value::from_field_in(v, *ty, true));
                if !v.is_null() {
                    loaded += 1;
                }
                row[*c] = v;
            }
        }
        Ok(loaded)
    }
}

/// Runs the planned scan: `query` narrows it; `limit` bounds the docs read; `sink` takes each
/// batch of rows and returns `false` to stop. `Ok(false)`: a shard query could not be built
/// here (the caller falls back to the search API, which reports the error as a search does).
pub(super) fn scan(cluster: &Cluster, plan: Plan, query: Option<&Value>, limit: Option<usize>, now_ms: i64, stats: &mut Stats, sink: &mut dyn FnMut(Vec<Vec<V>>) -> EsResult<bool>) -> EsResult<bool> {
    let Some(prepared) = prepare(cluster, plan.targets, query, now_ms) else { return Ok(false) };
    let mut seen = 0usize;
    let mut sc = Scratch::default();
    for p in &prepared {
        let limit = limit.map(|l| l.saturating_sub(seen));
        if limit == Some(0) {
            break;
        }
        let docs = collect(p, limit)?;
        seen += docs.total;
        let (t, specs, searcher, _) = p;
        for (ord, seg_docs) in docs.per_seg {
            if seg_docs.is_empty() {
                continue;
            }
            let seg = &searcher.segments()[ord as usize];
            let mut readers = readers(specs, seg, &t.idx.name, None);
            let fetched = Fetched::new(specs, None);
            for chunk in seg_docs.chunks(BATCH) {
                let mut rows: Vec<Vec<V>> = (0..chunk.len()).map(|_| new_row(plan.width)).collect();
                for (c, r) in readers.iter_mut().enumerate() {
                    stats.values_loaded += fill(r, seg, chunk, &mut rows, c, &mut sc);
                }
                stats.values_loaded += fetched.fill(t, searcher, seg, chunk, &mut rows)?;
                stats.documents_found += rows.len() as i64;
                if !sink(rows)? {
                    return Ok(true);
                }
            }
        }
    }
    Ok(true)
}

/// A row's sort key, or `None` when the row is filtered out.
pub(super) type KeyFn<'a> = dyn FnMut(&[V]) -> EsResult<Option<Vec<V>>> + 'a;

/// `SORT ... | LIMIT n` selected during the scan (see [`scan_top`]).
pub(super) struct TopSel<'a> {
    pub n: usize,
    /// The columns (plan positions) `key` reads.
    pub read: Vec<usize>,
    /// A row's sort key, or `None` when the row is filtered out.
    pub key: &'a mut KeyFn<'a>,
    pub cmp: &'a dyn Fn(&[V], &[V]) -> std::cmp::Ordering,
}

/// One candidate of [`scan_top`]: key, arrival, prepared target, segment, doc.
type Candidate = (Vec<V>, u64, u32, u32, u32);

/// Keeps the best `n` candidates (ties in arrival order) and returns the worst kept key.
fn trim(buf: &mut Vec<Candidate>, n: usize, cmp: &dyn Fn(&[V], &[V]) -> std::cmp::Ordering) -> Option<Vec<V>> {
    if buf.len() > n {
        buf.select_nth_unstable_by(n - 1, |a, b| cmp(&a.0, &b.0).then(a.1.cmp(&b.1)));
        buf.truncate(n);
    }
    (buf.len() == n).then(|| buf.iter().max_by(|a, b| cmp(&a.0, &b.0).then(a.1.cmp(&b.1))).map(|c| c.0.clone())).flatten()
}

/// `SORT ... | LIMIT n` straight from doc values, as Elasticsearch pushes a top-n to Lucene:
/// the first pass reads only the columns the sort key (and any leading `WHERE`) needs and keeps
/// the best `n` docs, skipping every doc that can't beat the current `n`-th; the second reads
/// the whole rows of just those docs. Returns them in scan order (the caller's stable sort then
/// orders ties exactly as a full sort would), or `None` when a shard query can't be built here.
/// Only the second pass counts towards `documents_found` / `values_loaded`, as the pushed-down
/// top-n reports in Elasticsearch.
pub(super) fn scan_top(cluster: &Cluster, plan: Plan, query: Option<&Value>, now_ms: i64, stats: &mut Stats, sel: TopSel<'_>) -> EsResult<Option<Vec<Vec<V>>>> {
    let n = sel.n;
    let Some(prepared) = prepare(cluster, plan.targets, query, now_ms) else { return Ok(None) };
    if n == 0 {
        return Ok(Some(Vec::new()));
    }
    let mut sc = Scratch::default();
    let mut buf: Vec<Candidate> = Vec::new();
    let mut bound: Option<Vec<V>> = None;
    let mut seq = 0u64;
    let mut rows: Vec<Vec<V>> = Vec::new();
    let cap = n.saturating_mul(2).max(1024);
    for (ti, p) in prepared.iter().enumerate() {
        let docs = collect(p, None)?;
        let (t, specs, searcher, _) = p;
        for (ord, seg_docs) in docs.per_seg {
            if seg_docs.is_empty() {
                continue;
            }
            let seg = &searcher.segments()[ord as usize];
            let mut readers = readers(specs, seg, &t.idx.name, Some(&sel.read));
            let fetched = Fetched::new(specs, Some(&sel.read));
            for chunk in seg_docs.chunks(BATCH) {
                if rows.len() < chunk.len() {
                    rows.resize_with(chunk.len(), || vec![V::Null; plan.width]);
                }
                let batch = &mut rows[..chunk.len()];
                for &c in &sel.read {
                    for r in batch.iter_mut() {
                        r[c] = V::Null;
                    }
                    fill(&mut readers[c], seg, chunk, batch, c, &mut sc);
                }
                fetched.fill(t, searcher, seg, chunk, batch)?;
                for (r, &doc) in batch.iter().zip(chunk) {
                    let s = seq;
                    seq += 1;
                    let Some(k) = (sel.key)(r)? else { continue };
                    // A later doc only wins with a strictly better key.
                    if bound.as_ref().is_some_and(|b| (sel.cmp)(&k, b) != std::cmp::Ordering::Less) {
                        continue;
                    }
                    buf.push((k, s, ti as u32, ord, doc));
                    if buf.len() >= cap {
                        bound = trim(&mut buf, n, sel.cmp);
                    }
                }
            }
        }
    }
    trim(&mut buf, n, sel.cmp);
    buf.sort_unstable_by_key(|c| c.1);
    let mut out: Vec<Vec<V>> = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        let (ti, ord) = (buf[i].2, buf[i].3);
        let j = i + buf[i..].iter().take_while(|c| c.2 == ti && c.3 == ord).count();
        let docs: Vec<u32> = buf[i..j].iter().map(|c| c.4).collect();
        let (t, specs, searcher, _) = &prepared[ti as usize];
        let seg = &searcher.segments()[ord as usize];
        let mut readers = readers(specs, seg, &t.idx.name, None);
        let mut rows: Vec<Vec<V>> = (0..docs.len()).map(|_| new_row(plan.width)).collect();
        for (c, r) in readers.iter_mut().enumerate() {
            stats.values_loaded += fill(r, seg, &docs, &mut rows, c, &mut sc);
        }
        stats.values_loaded += Fetched::new(specs, None).fill(t, searcher, seg, &docs, &mut rows)?;
        stats.documents_found += rows.len() as i64;
        out.extend(rows);
        i = j;
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ord_cache_is_built_only_for_repeated_reads() {
        let mut c = OrdCache::new(10);
        for _ in 0..ORD_CACHE_AFTER {
            assert!(c.slot(3).is_none());
        }
        assert!(c.slots.is_empty(), "a short read allocates nothing");
        *c.slot(3).expect("table built") = Some(V::Long(1));
        assert_eq!(c.slots.len(), 10);
        assert_eq!(c.slot(3).cloned(), Some(Some(V::Long(1))));
        // Large dictionaries never get a table.
        let mut big = OrdCache::new(ORD_CACHE_MAX + 1);
        for _ in 0..(ORD_CACHE_AFTER * 2) {
            assert!(big.slot(1).is_none());
        }
        assert!(big.slots.is_empty());
    }

    #[test]
    fn numeric_conversions_match_block_loaders() {
        assert_eq!(num(Conv::Float, encode::f32_sortable(1.1f32) as i64), V::Double(1.100000023841858));
        assert_eq!(num(Conv::Double, encode::f64_sortable(-2.5)), V::Double(-2.5));
        assert_eq!(num(Conv::Scaled(100.0), 1037), V::Double(10.37));
        assert_eq!(num(Conv::Bool, 1), V::Bool(true));
        assert_eq!(num(Conv::Int, -3), V::Int(-3));
        assert_eq!(num(Conv::Date, 86_400_000), V::Date(86_400_000));
        assert_eq!(num(Conv::Long, i64::MIN), V::Long(i64::MIN));
    }

    #[test]
    fn versions_sort_by_precedence() {
        let v = many(vec![V::Str("1.10.0".into()), V::Str("1.2.0".into()), V::Str("1.2.0".into())], Conv::Version);
        assert_eq!(v, V::Multi(vec![V::Str("1.2.0".into()), V::Str("1.10.0".into())]));
        assert_eq!(many(vec![V::Str("a".into())], Conv::Keyword), V::Str("a".into()));
        assert_eq!(many(Vec::new(), Conv::Keyword), V::Null);
    }

    #[test]
    fn ips_render_like_es() {
        let mut b = [0u8; 16];
        b[10] = 0xff;
        b[11] = 0xff;
        b[12..].copy_from_slice(&[10, 0, 0, 1]);
        assert_eq!(term(Conv::Ip, &b), V::Ip("10.0.0.1".into()));
    }
}
