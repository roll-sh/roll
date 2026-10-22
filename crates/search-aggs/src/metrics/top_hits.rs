//! `top_hits`: the best documents of each bucket, fetched through [`TopHitsFetcher`].
//!
//! Shards keep, per bucket, the `from + size` best documents (by score, or by the request's
//! `sort` with ties on the doc id) and the bucket's doc count, then run the fetch phase for them
//! (`_source`, `fields`, `highlight`… are the fetcher's job, with the whole `top_hits` object as
//! its spec). Reduces merge the shard lists like Lucene's `TopDocs.merge` (ties: shard order,
//! then position) and keep the `[from, from + size)` window; hit `sort` values are printed raw,
//! as Elasticsearch does after its reduce.

use std::sync::Arc;

use search_index::{SegmentContext, Searcher};

use crate::collect::{Aggregator, Batch, BindCtx};
use crate::error::{AggError, Result};
use crate::field::{FetchHit, TopHitsFetcher};
use crate::internal::{AggValue, Internal, Prop, ReduceCtx, RenderCtx};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::request::{AggDef, PathElement};
use crate::sort::{DocSort, SegmentSort, SortKey};
use crate::xparser::{Token, XParser};

/// A parsed `top_hits`.
#[derive(Clone, Debug)]
pub struct TopHitsSpec {
    pub from: u32,
    pub size: u32,
    pub sort: Option<Json>,
    pub track_scores: bool,
    /// The whole definition, for the fetcher (`_source`, `fields`, `highlight`, …).
    pub body: Json,
}

/// `TopHitsAggregationBuilder.parse`: a hand-written loop with Elasticsearch's "Unknown key"
/// errors; values it doesn't interpret are kept in the body for the fetch phase.
pub fn parse(p: &mut XParser<'_>, name: &str) -> Result<TopHitsSpec> {
    let mut spec = TopHitsSpec { from: 0, size: 3, sort: None, track_scores: false, body: Json::obj() };
    let mut field = String::new();
    let unknown = |p: &XParser<'_>, t: Token, field: &str| p.parsing_error(format!("Unknown key for a {t} in [{field}]."));
    while let Some(t) = p.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => field = p.current_name().unwrap_or("").to_string(),
            t if t.is_value() => {
                match field.as_str() {
                    "from" => {
                        let v = p.int_value()?;
                        if v < 0 {
                            return Err(AggError::illegal_argument(format!("[from] must be greater than or equal to 0. Found [{v}] in [{name}]")));
                        }
                        spec.from = v as u32;
                    }
                    "size" => {
                        let v = p.int_value()?;
                        if v < 0 {
                            return Err(AggError::illegal_argument(format!("[size] must be greater than or equal to 0. Found [{v}] in [{name}]")));
                        }
                        spec.size = v as u32;
                    }
                    "version" | "seq_no_primary_term" | "explain" => {
                        p.boolean_value()?;
                    }
                    "track_scores" => spec.track_scores = p.boolean_value()?,
                    "_source" | "stored_fields" => {}
                    "sort" => spec.sort = Some(Json::Str(p.text()?)),
                    _ => return Err(unknown(p, t, &field)),
                }
                spec.body.set(&field, p.value().clone());
            }
            Token::StartObject => {
                let v = p.value().clone();
                match field.as_str() {
                    "_source" | "highlight" => p.skip_children(),
                    "script_fields" => check_script_fields(p)?,
                    "sort" => {
                        spec.sort = Some(v.clone());
                        p.skip_children();
                    }
                    _ => return Err(unknown(p, t, &field)),
                }
                spec.body.set(&field, v);
            }
            Token::StartArray => {
                match field.as_str() {
                    "stored_fields" | "docvalue_fields" | "fields" | "_source" => {}
                    "sort" => spec.sort = Some(p.value().clone()),
                    _ => return Err(unknown(p, t, &field)),
                }
                spec.body.set(&field, p.value().clone());
                p.skip_children();
            }
            t => return Err(unknown(p, t, &field)),
        }
    }
    Ok(spec)
}

/// The shape checks of `script_fields` (each a `{"script": …, "ignore_failure": …}` object).
fn check_script_fields(p: &mut XParser<'_>) -> Result<()> {
    loop {
        match p.next_token() {
            Some(Token::EndObject) | None => return Ok(()),
            Some(Token::FieldName) => {}
            Some(_) => continue,
        }
        let t = p.next_token().unwrap_or(Token::EndObject);
        if t != Token::StartObject {
            return Err(p.parsing_error(format!("Expected [START_OBJECT] in [{}] but found [{t}]", p.current_name().unwrap_or(""))));
        }
        let mut field = String::new();
        loop {
            match p.next_token() {
                Some(Token::EndObject) | None => break,
                Some(Token::FieldName) => field = p.current_name().unwrap_or("").to_string(),
                Some(t) if t.is_value() => match field.as_str() {
                    "script" => {
                        crate::script::Script::parse(p)?;
                    }
                    "ignore_failure" => {
                        p.boolean_value()?;
                    }
                    _ => return Err(p.parsing_error(format!("Unknown key for a {t} in [{field}]."))),
                },
                Some(Token::StartObject) if field == "script" => {
                    crate::script::Script::parse(p)?;
                }
                Some(t) => return Err(p.parsing_error(format!("Unknown key for a {t} in [{field}]."))),
            }
        }
    }
}

/// One hit of a result.
#[derive(Clone, Debug)]
pub struct TopHit {
    pub key: SortKey,
    pub score: f32,
    /// The rendered hit (from the fetcher).
    pub json: Json,
}

/// `InternalTopHits`.
#[derive(Clone, Debug)]
pub struct TopHits {
    pub from: u32,
    pub size: u32,
    pub total: u64,
    pub max_score: f32,
    /// `None`: sorted by score.
    pub sort: Option<Arc<DocSort>>,
    pub hits: Vec<TopHit>,
}

impl Internal for TopHits {
    fn type_name(&self) -> &'static str {
        "top_hits"
    }
    fn class_name(&self) -> &'static str {
        "InternalTopHits"
    }
    fn must_reduce_single(&self) -> bool {
        true
    }
    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let first = &list[0];
        let (from, size) = (first.from, first.size);
        let (wfrom, wsize) = if ctx.is_final() { (from as usize, size as usize) } else { (0, (from + size) as usize) };
        let sort = first.sort.clone();
        let mut total = 0u64;
        let mut max_score = f32::NAN;
        let mut all: Vec<(usize, usize, TopHit)> = Vec::new();
        for (shard, t) in list.into_iter().enumerate() {
            total += t.total;
            if !t.max_score.is_nan() {
                max_score = if max_score.is_nan() { t.max_score } else { max_score.max(t.max_score) };
            }
            for (pos, h) in t.hits.into_iter().enumerate() {
                all.push((shard, pos, h));
            }
        }
        all.sort_by(|a, b| {
            let ord = match &sort {
                Some(s) => s.compare(&a.2.key, &b.2.key),
                None => b.2.score.total_cmp(&a.2.score),
            };
            ord.then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1))
        });
        let hits: Vec<TopHit> = all.into_iter().skip(wfrom).take(wsize).map(|(_, _, h)| h).collect();
        Ok(TopHits { from, size, total, max_score, sort, hits })
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        let mut hits = Json::obj();
        hits.push("total", Json::obj().with("value", Json::Int(self.total as i64)).with("relation", Json::Str("eq".into())));
        hits.push("max_score", if self.max_score.is_nan() { Json::Null } else { Json::Float32(self.max_score) });
        hits.push("hits", Json::Arr(self.hits.iter().map(|h| h.json.clone()).collect()));
        out.push("hits", hits);
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        if path.len() != 1 {
            return Err(AggError::illegal_argument(format!(
                "property paths for top_hits [{name}] can only contain a single field in _source, score or sort values, got {}",
                crate::internal::java_list(path)
            )));
        }
        let lower = path[0].to_lowercase();
        let tokens: Vec<&str> = lower.split([':', '>', '.']).collect();
        if self.hits.len() > 1 {
            return Err(AggError::illegal_argument(format!("property paths for top_hits [{name}] require configuring it with size to 1")));
        }
        let Some(top) = self.hits.first() else {
            return Err(AggError::new(500, "array_index_out_of_bounds_exception", "Index 0 out of bounds for length 0"));
        };
        match tokens[0] {
            "_sort" => {
                if let Some(Json::Arr(vals)) = top.json.get("sort") {
                    if vals.len() != 1 {
                        return Err(AggError::illegal_argument(format!(
                            "property path for top_hits [\" + getName() + \"] requires a single sort value, got {}",
                            vals.len()
                        )));
                    }
                    return Ok(match &vals[0] {
                        Json::Int(i) => Prop::Num(*i as f64),
                        Json::Float(f) => Prop::Num(*f),
                        Json::Float32(f) => Prop::Num(*f as f64),
                        other => Prop::Value(other.clone()),
                    });
                }
            }
            "_score" => return Ok(Prop::Num(top.score as f64)),
            "_source" => {
                if let (Some(src), Some(key)) = (top.json.get("_source"), tokens.get(1))
                    && let Some(v) = src.get(key)
                    && !matches!(v, Json::Null)
                {
                    return Ok(match v.as_f64() {
                        Some(n) => Prop::Num(n),
                        None => Prop::Value(v.clone()),
                    });
                }
            }
            _ => {}
        }
        Err(AggError::illegal_argument(format!("path not supported for [{name}]: {}", crate::internal::java_list(path))))
    }
    fn ram_bytes(&self) -> usize {
        64 + self.hits.len() * 256
    }
}

/// Binds `top_hits`.
pub fn create(def: &AggDef, spec: &TopHitsSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let window = spec.from as u64 + spec.size as u64;
    let max = ctx.lookup.max_inner_result_window();
    if window > max as u64 {
        return Err(AggError::illegal_argument(format!(
            "Top hits result window is too large, the top hits aggregator [{}]'s from + size must be less than or equal to: [{max}] but was [{window}]. This limit can be set by changing the [index.max_inner_result_window] index level setting.",
            def.name
        )));
    }
    let sort = match &spec.sort {
        None => None,
        Some(s) => {
            let keys = ctx.lookup.hit_sort(s)?;
            let sort = DocSort::new(&keys)?;
            if sort.is_score_only() || sort.is_empty() { None } else { Some(Arc::new(sort)) }
        }
    };
    let top_n = (window as usize).min(ctx.searcher.max_doc() as usize);
    Ok(Box::new(TopHitsAgg {
        spec: spec.clone(),
        sort,
        top_n,
        fetcher: ctx.lookup.fetcher(),
        searcher: ctx.searcher.clone(),
        seg: None,
        base: 0,
        buckets: Vec::new(),
        tracker: ctx.tracker(def),
    }))
}

#[derive(Clone, Debug)]
struct Entry {
    key: SortKey,
    score: f32,
    doc: u32,
}

#[derive(Default)]
struct Bucket {
    total: u64,
    max_score: f32,
    /// A heap with the worst entry first.
    heap: Vec<Entry>,
}

struct TopHitsAgg {
    spec: TopHitsSpec,
    sort: Option<Arc<DocSort>>,
    top_n: usize,
    fetcher: Option<Arc<dyn TopHitsFetcher>>,
    searcher: Searcher,
    seg: Option<SegmentSort>,
    base: u32,
    buckets: Vec<Option<Bucket>>,
    tracker: MemTracker,
}

/// Whether `a` sorts after `b` (worse).
fn worse(sort: &Option<Arc<DocSort>>, a: &Entry, b: &Entry) -> bool {
    let ord = match sort {
        Some(s) => s.compare(&a.key, &b.key),
        None => b.score.total_cmp(&a.score),
    };
    ord.then(a.doc.cmp(&b.doc)) == std::cmp::Ordering::Greater
}

fn sift_up(h: &mut [Entry], mut i: usize, sort: &Option<Arc<DocSort>>) {
    while i > 0 {
        let parent = (i - 1) / 2;
        if worse(sort, &h[i], &h[parent]) {
            h.swap(i, parent);
            i = parent;
        } else {
            break;
        }
    }
}

fn sift_down(h: &mut [Entry], mut i: usize, sort: &Option<Arc<DocSort>>) {
    loop {
        let (l, r) = (2 * i + 1, 2 * i + 2);
        let mut m = i;
        if l < h.len() && worse(sort, &h[l], &h[m]) {
            m = l;
        }
        if r < h.len() && worse(sort, &h[r], &h[m]) {
            m = r;
        }
        if m == i {
            break;
        }
        h.swap(i, m);
        i = m;
    }
}

impl TopHitsAgg {
    fn build_one(&self, bucket: Option<Bucket>) -> Result<AggValue> {
        let Some(mut b) = bucket else {
            return Ok(self.empty());
        };
        let sort = self.sort.clone();
        b.heap.sort_by(|x, y| {
            if worse(&sort, x, y) {
                std::cmp::Ordering::Greater
            } else if worse(&sort, y, x) {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        });
        let tracks = self.sort.is_none() || self.spec.track_scores;
        let max_score = match &self.sort {
            None => b.heap.first().map_or(f32::NAN, |e| e.score),
            Some(_) if self.spec.track_scores => b.max_score,
            Some(_) => f32::NAN,
        };
        let fetch: Vec<FetchHit> = b
            .heap
            .iter()
            .map(|e| FetchHit {
                doc: e.doc,
                score: if tracks { e.score } else { f32::NAN },
                sort: self.sort.as_ref().map(|s| s.render(&e.key)).unwrap_or_default(),
            })
            .collect();
        let rendered = match &self.fetcher {
            Some(f) => f.fetch(&self.searcher, &fetch, &self.spec.body)?,
            None => fetch
                .iter()
                .map(|h| {
                    let mut o = Json::obj();
                    o.push("_score", if h.score.is_nan() { Json::Null } else { Json::Float32(h.score) });
                    if !h.sort.is_empty() {
                        o.push("sort", Json::Arr(h.sort.clone()));
                    }
                    o
                })
                .collect(),
        };
        let hits = b
            .heap
            .into_iter()
            .zip(rendered)
            .map(|(e, json)| TopHit { key: e.key, score: if tracks { e.score } else { f32::NAN }, json })
            .collect();
        Ok(AggValue::TopHits(Box::new(TopHits { from: self.spec.from, size: self.spec.size, total: b.total, max_score, sort: self.sort.clone(), hits })))
    }

    fn empty(&self) -> AggValue {
        AggValue::TopHits(Box::new(TopHits {
            from: self.spec.from,
            size: self.spec.size,
            total: 0,
            max_score: f32::NAN,
            sort: self.sort.clone(),
            hits: Vec::new(),
        }))
    }
}

impl Aggregator for TopHitsAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.seg = self.sort.as_ref().map(|s| s.segment(seg));
        self.base = seg.base;
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        let sort = self.sort.clone();
        for i in 0..b.len() {
            let ord = b.ords[i] as usize;
            if self.buckets.len() <= ord {
                self.buckets.resize_with(ord + 1, || None);
                self.tracker.ensure(self.buckets.len() * 64)?;
            }
            let score = b.score(i);
            let bucket = self.buckets[ord].get_or_insert_with(|| Bucket { total: 0, max_score: f32::NAN, heap: Vec::new() });
            bucket.total += 1;
            if !score.is_nan() {
                bucket.max_score = if bucket.max_score.is_nan() { score } else { bucket.max_score.max(score) };
            }
            if self.top_n == 0 {
                continue;
            }
            let doc = self.base + b.docs[i];
            let key = match (&sort, &mut self.seg) {
                (Some(s), Some(reader)) => reader.key(s, b.docs[i], score),
                _ => SortKey::new(),
            };
            let e = Entry { key, score, doc };
            if bucket.heap.len() < self.top_n {
                bucket.heap.push(e);
                let n = bucket.heap.len() - 1;
                sift_up(&mut bucket.heap, n, &sort);
            } else if worse(&sort, &bucket.heap[0], &e) {
                bucket.heap[0] = e;
                sift_down(&mut bucket.heap, 0, &sort);
            }
        }
        Ok(())
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        let mut out = Vec::with_capacity(owning.len());
        for &o in owning {
            let b = self.buckets.get_mut(o as usize).and_then(Option::take);
            out.push(self.build_one(b)?);
        }
        Ok(out)
    }

    fn build_empty(&self) -> AggValue {
        self.empty()
    }

    fn needs_scores(&self) -> bool {
        match &self.sort {
            None => true,
            Some(s) => s.needs_scores() || self.spec.track_scores,
        }
    }

    /// `Aggregator.bucketComparator`'s default: top hits can't order buckets.
    fn validate_sort(&self, _name: &str, _key: Option<&str>, _rest: &[PathElement]) -> Result<()> {
        Err(AggError::illegal_argument(
            "Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end.",
        ))
    }
}
