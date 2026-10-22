//! Single-bucket aggregations: `filter`, `global`, `missing`, `nested`, `reverse_nested`,
//! `sampler`, `diversified_sampler` — and their result type.
//!
//! A single-bucket aggregation's bucket ordinal is its owning ordinal, so per-ordinal doc counts
//! are one dense array and sub-aggregations receive the same ordinals.
//!
//! - `filter` advances the filter's scorer along the (ascending) docs it is given.
//! - `nested` maps a parent doc to its block of children (the child path's docs between the
//!   previous parent and itself, from cached per-segment bit sets); `reverse_nested` maps a
//!   child to the next parent, collecting each parent once per bucket and segment as
//!   Elasticsearch does.
//! - `sampler` and `diversified_sampler` defer their sub-aggregations: they keep, per owning
//!   bucket, the best-scoring docs (`shard_size`; at most `max_docs_per_value` per value,
//!   with Lucene's `DiversifiedTopDocsCollector` rules) and replay them in doc order once
//!   collection ends.

use std::sync::Arc;

use search_codec::NO_MORE_DOCS;
use search_index::util::bitset::FixedBitSet;
use search_index::{QueryRef, ScoreMode, Scorer, Searcher, SegmentContext, Weight};

use crate::collect::{Aggregator, Batch, BindCtx, Subs, grow, single_bucket_validate_sort};
use crate::error::{AggError, Result};
use crate::field::NestedLookup;
use crate::internal::{AggValue, Aggs, Internal, Prop, ReduceCtx, RenderCtx};
use crate::json::Json;
use crate::request::{AggDef, PathElement};
use crate::values::{BoundVs, BytesReader, DocCounter, GeoReader, NumReader, VsOpts, VsSpec, VsType, bind};
use crate::xparser::{Field, Token, Tokens, XParser};

/// Which single-bucket aggregation produced a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SingleKind {
    Filter,
    Global,
    Missing,
    Nested,
    ReverseNested,
    Sampler,
    Children,
    Parent,
    RandomSampler,
}

impl SingleKind {
    pub fn type_name(self) -> &'static str {
        match self {
            SingleKind::Filter => "filter",
            SingleKind::Global => "global",
            SingleKind::Missing => "missing",
            SingleKind::Nested => "nested",
            SingleKind::ReverseNested => "reverse_nested",
            SingleKind::Sampler => "sampler",
            SingleKind::Children => "children",
            SingleKind::Parent => "parent",
            SingleKind::RandomSampler => "random_sampler",
        }
    }

    fn class_name(self) -> &'static str {
        match self {
            SingleKind::Filter => "InternalFilter",
            SingleKind::Global => "InternalGlobal",
            SingleKind::Missing => "InternalMissing",
            SingleKind::Nested => "InternalNested",
            SingleKind::ReverseNested => "InternalReverseNested",
            SingleKind::Sampler => "InternalSampler",
            SingleKind::Children => "InternalChildren",
            SingleKind::Parent => "InternalParent",
            SingleKind::RandomSampler => "InternalRandomSampler",
        }
    }
}

/// A single-bucket result.
#[derive(Clone, Debug)]
pub struct SingleBucket {
    pub kind: SingleKind,
    pub doc_count: u64,
    pub aggs: Aggs,
    /// `random_sampler` settings (rendered, and applied to the sub-aggregations at the final
    /// reduce).
    pub sampler: Option<crate::bucket::random_sampler::SamplerInfo>,
}

impl Internal for SingleBucket {
    fn type_name(&self) -> &'static str {
        self.kind.type_name()
    }
    fn class_name(&self) -> &'static str {
        self.kind.class_name()
    }
    fn must_reduce_single(&self) -> bool {
        true
    }
    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let kind = list[0].kind;
        let sampler = list[0].sampler;
        let doc_count = list.iter().map(|s| s.doc_count).sum();
        let mut aggs = Aggs::reduce_hashed(list.into_iter().map(|s| s.aggs).collect(), ctx)?;
        if let Some(info) = &sampler
            && ctx.is_final()
        {
            crate::bucket::random_sampler::finalize_aggs(&mut aggs, info.probability)?;
        }
        Ok(SingleBucket { sampler, kind, doc_count, aggs })
    }
    fn render(&self, out: &mut Json, rc: &RenderCtx) {
        if let Some(info) = &self.sampler {
            out.push("seed", Json::Int(info.seed as i64));
            if let Some(s) = info.shard_seed {
                out.push("shard_seed", Json::Int(s as i64));
            }
            out.push("probability", Json::Float(info.probability));
        }
        out.push("doc_count", Json::from(self.doc_count));
        self.aggs.render_into(out, rc);
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        // InternalSingleBucketAggregation.getProperty: `_count` or a sub-aggregation.
        if path[0] == "_count" {
            if path.len() > 1 {
                return Err(AggError::illegal_argument("_count must be the last element in the path"));
            }
            return Ok(Prop::Num(self.doc_count as f64));
        }
        let agg = self
            .aggs
            .get(&path[0])
            .ok_or_else(|| AggError::illegal_argument(format!("Cannot find an aggregation named [{}] in [{name}]", path[0])))?;
        if path.len() == 1 { Ok(Prop::Agg(agg)) } else { agg.value.property(&agg.name, &path[1..]) }
    }
    fn ram_bytes(&self) -> usize {
        64 + self.aggs.ram_bytes()
    }
}

/// The parsed single-bucket aggregations.
#[derive(Clone, Debug)]
pub enum SingleSpec {
    Filter(Json),
    Global,
    Missing(VsSpec),
    Nested { path: String },
    ReverseNested { path: Option<String> },
    Sampler { shard_size: i32 },
    Diversified { vs: VsSpec, shard_size: i32, max_docs_per_value: i32, execution_hint: Option<String> },
}

impl SingleSpec {
    pub fn type_name(&self) -> &'static str {
        match self {
            SingleSpec::Filter(_) => "filter",
            SingleSpec::Global => "global",
            SingleSpec::Missing(_) => "missing",
            SingleSpec::Nested { .. } => "nested",
            SingleSpec::ReverseNested { .. } => "reverse_nested",
            SingleSpec::Sampler { .. } => "sampler",
            SingleSpec::Diversified { .. } => "diversified_sampler",
        }
    }
}

/// `nested` / `reverse_nested` custom parsers.
pub fn parse_path_agg(p: &mut XParser<'_>, name: &str, nested: bool) -> Result<SingleSpec> {
    let mut path = None;
    let mut cur = String::new();
    loop {
        match p.next_token() {
            Some(Token::EndObject) | None => break,
            Some(Token::FieldName) => cur = p.current_name().unwrap_or("").to_string(),
            Some(Token::String) => {
                if cur == "path" {
                    path = Some(p.text()?);
                } else {
                    return Err(p.parsing_error(format!("Unknown key for a VALUE_STRING in [{name}]: [{cur}].")));
                }
            }
            Some(t) => return Err(p.parsing_error(format!("Unexpected token {t} in [{name}]."))),
        }
    }
    if nested {
        let path = path.ok_or_else(|| p.parsing_error(format!("Missing [path] field for nested aggregation [{name}]")))?;
        Ok(SingleSpec::Nested { path })
    } else {
        Ok(SingleSpec::ReverseNested { path })
    }
}

/// `SamplerAggregationBuilder.parse`.
pub fn parse_sampler(p: &mut XParser<'_>, name: &str) -> Result<SingleSpec> {
    let mut shard_size = 100;
    let mut cur = String::new();
    loop {
        match p.next_token() {
            Some(Token::EndObject) | None => break,
            Some(Token::FieldName) => cur = p.current_name().unwrap_or("").to_string(),
            Some(Token::Number) if cur == "shard_size" => shard_size = p.int_value()?,
            Some(_) => return Err(p.parsing_error(format!("Unsupported property \"{cur}\" for aggregation \"{name}"))),
        }
    }
    Ok(SingleSpec::Sampler { shard_size })
}

pub fn parse_diversified(p: &mut XParser<'_>, name: &str) -> Result<SingleSpec> {
    let opts = VsOpts::new(true, false, false);
    let mut fields = VsSpec::fields(opts);
    fields.extend_from_slice(&[
        Field::new("shard_size", Tokens::INT),
        Field::new("max_docs_per_value", Tokens::INT),
        Field::new("execution_hint", Tokens::STRING),
    ]);
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    let mut vs = VsSpec::default();
    let mut shard_size = 100;
    let mut max_docs_per_value = 1;
    let mut execution_hint = None;
    p.object("diversified_sampler", &fields, |p, i| {
        let f = names[i];
        if vs.parse_field(p, f)? {
            return Ok(());
        }
        match f {
            "shard_size" => {
                let v = p.int_value()?;
                if v < 0 {
                    return Err(AggError::illegal_argument(format!("[shardSize] must be greater than or equal to 0. Found [{v}] in [{name}]")));
                }
                shard_size = v;
            }
            "max_docs_per_value" => {
                let v = p.int_value()?;
                if v < 0 {
                    return Err(AggError::illegal_argument(format!(
                        "[maxDocsPerValue] must be greater than or equal to 0. Found [{v}] in [{name}]"
                    )));
                }
                max_docs_per_value = v;
            }
            _ => execution_hint = Some(p.text()?),
        }
        Ok(())
    })?;
    vs.check_required(opts)?;
    Ok(SingleSpec::Diversified { vs, shard_size, max_docs_per_value, execution_hint })
}

/// What decides membership in the bucket (one per aggregator: variant sizes don't matter).
#[allow(clippy::large_enum_variant)]
enum Mode {
    Filter { weight: Box<dyn Weight>, scorer: Option<Box<dyn Scorer>>, last: (u32, bool) },
    Global,
    Missing { vs: BoundVs, reader: MissingReader },
    Nested { child: QueryRef, parent: QueryRef, bits: Option<(Arc<FixedBitSet>, Arc<FixedBitSet>)> },
    ReverseNested { parent: QueryRef, bits: Option<Arc<FixedBitSet>>, last: hashbrown::HashMap<u32, u32> },
    /// A nested path that is unmapped: nothing is collected.
    Empty,
}

enum MissingReader {
    None,
    Num(NumReader),
    Bytes(BytesReader),
    Geo(GeoReader),
}

/// The plain single-bucket aggregator.
pub struct SingleAgg {
    kind: SingleKind,
    mode: Mode,
    counts: Vec<u64>,
    subs: Subs,
    searcher: Searcher,
    doc_counter: DocCounter,
    doc_count_field: Option<u32>,
    children: Vec<u32>,
}

pub fn create(def: &AggDef, spec: &SingleSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let (kind, mode) = match spec {
        SingleSpec::Filter(q) => {
            let query = ctx.lookup.query(q)?;
            let weight = ctx.searcher.create_weight(&query, ScoreMode::CompleteNoScores)?;
            (SingleKind::Filter, Mode::Filter { weight, scorer: None, last: (u32::MAX, false) })
        }
        SingleSpec::Global => {
            if !ctx.parents.is_empty() {
                return Err(AggError::execution(format!(
                    "Aggregation [{}] cannot have a global sub-aggregation [{}]. Global aggregations can only be defined as top level aggregations",
                    ctx.parents.last().copied().unwrap_or(""),
                    def.name
                )));
            }
            (SingleKind::Global, Mode::Global)
        }
        SingleSpec::Missing(vs) => {
            let b = bind(vs, ctx.lookup, VsType::Keyword, false)?;
            if (matches!(b.vs_type, VsType::CartesianPoint | VsType::GeoShape | VsType::CartesianShape | VsType::Histogram | VsType::AggregateMetric | VsType::ExponentialHistogram) && !b.unmapped) || b.is_counter() {
                return Err(b.unsupported("missing"));
            }
            (SingleKind::Missing, Mode::Missing { vs: b, reader: MissingReader::None })
        }
        SingleSpec::Nested { path } => {
            let mode = match ctx.lookup.nested(path) {
                NestedLookup::Nested(n) => {
                    let parent = match &ctx.nested_path {
                        None => ctx.lookup.root_filter(),
                        Some(p) => match ctx.lookup.nested(p) {
                            NestedLookup::Nested(pn) => pn.child_filter,
                            _ => ctx.lookup.root_filter(),
                        },
                    };
                    Mode::Nested { child: n.child_filter, parent, bits: None }
                }
                _ => Mode::Empty,
            };
            let scope = if matches!(mode, Mode::Empty) { ctx.nested_path.clone() } else { Some(path.clone()) };
            let prev = std::mem::replace(&mut ctx.nested_path, scope);
            let agg_name = if matches!(mode, Mode::Empty) { ctx.nested_agg.clone() } else { Some(def.name.clone()) };
            let prev_agg = std::mem::replace(&mut ctx.nested_agg, agg_name);
            let subs = Subs::bind(def, ctx);
            ctx.nested_path = prev;
            ctx.nested_agg = prev_agg;
            return finish(SingleKind::Nested, mode, subs?, ctx);
        }
        SingleSpec::ReverseNested { path } => {
            if !ctx.parents.contains(&"nested") {
                return Err(AggError::illegal_argument(format!(
                    "Reverse nested aggregation [{}] can only be used inside a [nested] aggregation",
                    def.name
                )));
            }
            if let (Some(p), Some(cur)) = (path, &ctx.nested_path)
                && cur != p
                && !cur.starts_with(&format!("{p}."))
            {
                return Err(AggError::illegal_argument(format!(
                    "Reverse nested path [{p}] is not a parent of the current nested scope [{cur}]"
                )));
            }
            let (mode, new_scope) = match path {
                None => (Mode::ReverseNested { parent: ctx.lookup.root_filter(), bits: None, last: hashbrown::HashMap::new() }, None),
                Some(p) => match ctx.lookup.nested(p) {
                    NestedLookup::Nested(n) => (Mode::ReverseNested { parent: n.child_filter, bits: None, last: hashbrown::HashMap::new() }, Some(p.clone())),
                    _ => (Mode::Empty, ctx.nested_path.clone()),
                },
            };
            let prev = std::mem::replace(&mut ctx.nested_path, new_scope);
            let subs = Subs::bind(def, ctx);
            ctx.nested_path = prev;
            return finish(SingleKind::ReverseNested, mode, subs?, ctx);
        }
        SingleSpec::Sampler { shard_size } => {
            let subs = Subs::bind(def, ctx)?;
            return Ok(Box::new(SamplerAgg::new(*shard_size, None, 0, subs, ctx)));
        }
        SingleSpec::Diversified { vs, shard_size, max_docs_per_value, execution_hint } => {
            if let Some(h) = execution_hint
                && !matches!(h.as_str(), "map" | "global_ordinals" | "bytes_hash")
            {
                return Err(AggError::illegal_argument(format!(
                    "Unknown `execution_hint`: [{h}], expected any of [map, global_ordinals, bytes_hash]"
                )));
            }
            let b = bind(vs, ctx.lookup, VsType::Numeric, false)?;
            if (!b.unmapped && !matches!(b.vs_type, VsType::Numeric | VsType::Date | VsType::Boolean | VsType::Keyword | VsType::Ip)) || b.is_counter() {
                return Err(b.unsupported("diversified_sampler"));
            }
            let subs = Subs::bind(def, ctx)?;
            return Ok(Box::new(SamplerAgg::new(*shard_size, Some(b), *max_docs_per_value, subs, ctx)));
        }
    };
    let subs = Subs::bind(def, ctx)?;
    finish(kind, mode, subs, ctx)
}

fn finish(kind: SingleKind, mode: Mode, subs: Subs, ctx: &BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    Ok(Box::new(SingleAgg {
        kind,
        mode,
        counts: Vec::new(),
        subs,
        searcher: ctx.searcher.clone(),
        doc_counter: DocCounter::none(),
        doc_count_field: ctx.doc_count_field,
        children: Vec::new(),
    }))
}

impl SingleAgg {
    #[inline]
    fn count(&mut self, ord: u32, doc: u32) {
        if self.counts.len() <= ord as usize {
            grow(&mut self.counts, ord, 0);
        }
        self.counts[ord as usize] += self.doc_counter.count(doc);
    }
}

impl Aggregator for SingleAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.doc_counter = DocCounter::open(seg, self.doc_count_field);
        match &mut self.mode {
            Mode::Filter { weight, scorer, last } => {
                *scorer = weight.scorer(seg)?;
                *last = (u32::MAX, false);
            }
            Mode::Missing { vs, reader } => {
                *reader = if vs.unmapped && vs.missing.is_none() {
                    MissingReader::None
                } else {
                    match vs.vs_type {
                        VsType::Numeric | VsType::Date | VsType::Boolean => MissingReader::Num(vs.numeric_reader(seg)?),
                        VsType::GeoPoint => MissingReader::Geo(vs.geo_reader(seg)?),
                        _ => MissingReader::Bytes(vs.bytes_reader(seg)?),
                    }
                };
            }
            Mode::Nested { child, parent, bits } => {
                let p = search_index::query::nested::segment_bits(&self.searcher, seg, parent)?;
                let c = search_index::query::nested::segment_bits(&self.searcher, seg, child)?;
                *bits = Some((p, c));
            }
            Mode::ReverseNested { parent, bits, last } => {
                *bits = Some(search_index::query::nested::segment_bits(&self.searcher, seg, parent)?);
                last.clear();
            }
            Mode::Global | Mode::Empty => {}
        }
        self.subs.set_segment(seg)
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        let mut mode = std::mem::replace(&mut self.mode, Mode::Empty);
        let r = self.collect_with(&mut mode, b);
        self.mode = mode;
        r
    }

    fn finish_segment(&mut self) -> Result<()> {
        self.subs.finish_segment()
    }

    fn post_collection(&mut self) -> Result<()> {
        self.subs.post_collection()
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        let aggs = self.subs.build(owning)?;
        Ok(owning
            .iter()
            .zip(aggs)
            .map(|(&o, aggs)| AggValue::Single(Box::new(SingleBucket { sampler: None, kind: self.kind, doc_count: self.counts.get(o as usize).copied().unwrap_or(0), aggs })))
            .collect())
    }

    fn build_empty(&self) -> AggValue {
        AggValue::Single(Box::new(SingleBucket { sampler: None, kind: self.kind, doc_count: 0, aggs: self.subs.build_empty() }))
    }

    fn needs_scores(&self) -> bool {
        self.subs.needs_scores()
    }

    fn validate_sort(&self, name: &str, key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        single_bucket_validate_sort(name, &self.subs, key, rest)
    }

    fn sort_value(&self, _key: Option<&str>, rest: &[PathElement], ord: u32) -> f64 {
        if rest.is_empty() { self.counts.get(ord as usize).copied().unwrap_or(0) as f64 } else { self.subs.sort_value(rest, ord) }
    }

    fn sort_is_count(&self, _key: Option<&str>, rest: &[PathElement]) -> bool {
        rest.is_empty() || self.subs.sort_is_count(rest)
    }
}

impl SingleAgg {
    fn collect_with(&mut self, mode: &mut Mode, b: Batch<'_>) -> Result<()> {
        match mode {
            Mode::Empty => {}
            Mode::Global => {
                for i in 0..b.len() {
                    self.count(b.ords[i], b.docs[i]);
                }
                if !self.subs.is_empty() {
                    self.subs.collect_direct(b)?;
                }
            }
            Mode::Filter { scorer, last, .. } => {
                let Some(sc) = scorer.as_mut() else { return Ok(()) };
                for i in 0..b.len() {
                    let doc = b.docs[i];
                    let hit = if last.0 == doc {
                        last.1
                    } else {
                        let mut d = sc.doc();
                        if d == u32::MAX || d < doc {
                            d = sc.advance(doc);
                        }
                        let h = d == doc;
                        *last = (doc, h);
                        h
                    };
                    if hit {
                        self.count(b.ords[i], doc);
                        if !self.subs.is_empty() {
                            self.subs.push(doc, b.ords[i], b.score(i))?;
                        }
                    }
                }
            }
            Mode::Missing { reader, .. } => {
                for i in 0..b.len() {
                    let doc = b.docs[i];
                    let has = match reader {
                        MissingReader::None => false,
                        MissingReader::Num(r) => !r.doubles(doc, b.score(i))?.is_empty(),
                        MissingReader::Bytes(r) => r.load(doc, b.score(i))? > 0,
                        MissingReader::Geo(r) => !r.points(doc).is_empty(),
                    };
                    if !has {
                        self.count(b.ords[i], doc);
                        if !self.subs.is_empty() {
                            self.subs.push(doc, b.ords[i], b.score(i))?;
                        }
                    }
                }
            }
            Mode::Nested { bits, .. } => {
                let Some((parents, children)) = bits.as_ref() else { return Ok(()) };
                let (parents, children) = (parents.clone(), children.clone());
                for i in 0..b.len() {
                    let p = b.docs[i];
                    if p == 0 {
                        continue;
                    }
                    let prev = parents.prev_set_bit(p - 1);
                    let start = prev.map_or(0, |x| x + 1);
                    self.children.clear();
                    let mut c = children.next_set_bit(start);
                    while c != NO_MORE_DOCS && c < p {
                        self.children.push(c);
                        c = if c + 1 >= children.len() { NO_MORE_DOCS } else { children.next_set_bit(c + 1) };
                    }
                    let owning = b.ords[i];
                    for k in 0..self.children.len() {
                        let cd = self.children[k];
                        self.count(owning, cd);
                        if !self.subs.is_empty() {
                            self.subs.push(cd, owning, b.score(i))?;
                        }
                    }
                }
            }
            Mode::ReverseNested { bits, last, .. } => {
                let Some(parents) = bits.as_ref() else { return Ok(()) };
                let parents = parents.clone();
                for i in 0..b.len() {
                    let child = b.docs[i];
                    let parent = parents.next_set_bit(child);
                    if parent == NO_MORE_DOCS {
                        continue;
                    }
                    let owning = b.ords[i];
                    let collect = match last.get(&owning) {
                        Some(&l) => parent > l,
                        None => true,
                    };
                    if collect {
                        last.insert(owning, parent);
                        self.count(owning, parent);
                        if !self.subs.is_empty() {
                            self.subs.push(parent, owning, b.score(i))?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// A sampled doc.
#[derive(Clone, Copy, Debug)]
struct Hit {
    score: f32,
    /// Global doc id.
    doc: u32,
    key: i64,
}

/// `ScoreDocKeyQueue.lessThan`: worse hits first (lower score, then higher doc).
fn worse(a: &Hit, b: &Hit) -> bool {
    if a.score == b.score { a.doc > b.doc } else { a.score < b.score }
}

/// Lucene's `PriorityQueue` semantics over a small vector (sizes are `shard_size`).
#[derive(Default, Clone)]
struct HitQueue {
    hits: Vec<Hit>,
    cap: usize,
}

impl HitQueue {
    fn new(cap: usize) -> HitQueue {
        HitQueue { hits: Vec::new(), cap }
    }
    fn top(&self) -> Option<&Hit> {
        self.hits.iter().reduce(|a, b| if worse(b, a) { b } else { a })
    }
    fn pop(&mut self) -> Option<Hit> {
        let i = (0..self.hits.len()).reduce(|a, b| if worse(&self.hits[b], &self.hits[a]) { b } else { a })?;
        Some(self.hits.swap_remove(i))
    }
    /// `insertWithOverflow`: returns the evicted hit (the new one if not competitive).
    fn insert_with_overflow(&mut self, h: Hit) -> Option<Hit> {
        if self.hits.len() < self.cap {
            self.hits.push(h);
            return None;
        }
        if self.cap == 0 {
            return Some(h);
        }
        let top = *self.top().expect("full");
        if worse(&top, &h) {
            self.pop();
            self.hits.push(h);
            Some(top)
        } else {
            Some(h)
        }
    }
    fn remove(&mut self, h: &Hit) {
        if let Some(i) = self.hits.iter().position(|x| x.doc == h.doc) {
            self.hits.swap_remove(i);
        }
    }
}

/// Per owning bucket: Lucene's `TopScoreDocCollector` (plain sampler) or
/// `DiversifiedTopDocsCollector`.
#[derive(Clone)]
struct BucketSample {
    global: HitQueue,
    per_key: hashbrown::HashMap<i64, HitQueue>,
}

/// `sampler` / `diversified_sampler`.
struct SamplerAgg {
    shard_size: usize,
    /// The diversifying values source (`diversified_sampler`).
    diversify: Option<BoundVs>,
    max_per_value: usize,
    samples: Vec<Option<BucketSample>>,
    subs: Subs,
    searcher: Searcher,
    seg_base: u32,
    num: Option<NumReader>,
    bytes: Option<BytesReader>,
    replayed: bool,
}

impl SamplerAgg {
    fn new(shard_size: i32, diversify: Option<BoundVs>, max_per_value: i32, subs: Subs, ctx: &BindCtx<'_>) -> SamplerAgg {
        SamplerAgg {
            shard_size: shard_size.max(0) as usize,
            diversify,
            max_per_value: max_per_value.max(0) as usize,
            samples: Vec::new(),
            subs,
            searcher: ctx.searcher.clone(),
            seg_base: 0,
            num: None,
            bytes: None,
            replayed: false,
        }
    }

    fn key_of(&mut self, doc: u32, score: f32) -> Result<i64> {
        if let Some(r) = self.num.as_mut() {
            let v = r.longs(doc, score)?;
            if v.len() > 1 {
                return Err(AggError::illegal_argument("Sample diversifying key must be a single valued-field"));
            }
            return Ok(v.first().copied().unwrap_or(0));
        }
        if let Some(r) = self.bytes.as_mut() {
            let n = r.load(doc, score)?;
            if n > 1 {
                return Err(AggError::illegal_argument("Sample diversifying key must be a single valued-field"));
            }
            if n == 0 {
                return Ok(0);
            }
            let (h1, _) = crate::util::murmur3_x64_128(r.value(0), 0);
            return Ok(h1 as i64);
        }
        Ok(0)
    }
}

impl Aggregator for SamplerAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        if self.subs.is_empty() {
            return Err(AggError::illegal_argument("Sampler aggregation must be used with child aggregations."));
        }
        self.seg_base = seg.base;
        if let Some(vs) = &self.diversify {
            if vs.vs_type.is_numeric() {
                self.num = Some(vs.numeric_reader(seg)?);
            } else {
                self.bytes = Some(vs.bytes_reader(seg)?);
            }
        }
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        for i in 0..b.len() {
            let ord = b.ords[i] as usize;
            if self.samples.len() <= ord {
                self.samples.resize(ord + 1, None);
            }
            let score = if b.scores.is_empty() { 1.0 } else { b.scores[i] };
            let mut hit = Hit { score, doc: self.seg_base + b.docs[i], key: 0 };
            let cap = self.shard_size;
            let per = self.max_per_value;
            let diversify = self.diversify.is_some();
            let sample = self.samples[ord].get_or_insert_with(|| BucketSample { global: HitQueue::new(cap), per_key: hashbrown::HashMap::new() });
            if !diversify {
                sample.global.insert_with_overflow(hit);
                continue;
            }
            // DiversifiedTopDocsCollector.insert.
            if sample.global.hits.len() >= cap && sample.global.top().is_some_and(|t| worse(&hit, t)) {
                continue;
            }
            let key = self.key_of(b.docs[i], score)?;
            hit.key = key;
            let sample = self.samples[ord].as_mut().expect("sample");
            let q = sample.per_key.entry(key).or_insert_with(|| HitQueue::new(per));
            match q.insert_with_overflow(hit) {
                Some(o) if o.doc == hit.doc => {}
                None => {
                    if let Some(g) = sample.global.insert_with_overflow(hit)
                        && let Some(kq) = sample.per_key.get_mut(&g.key)
                    {
                        kq.pop();
                        if kq.hits.is_empty() {
                            sample.per_key.remove(&g.key);
                        }
                    }
                }
                Some(o) => {
                    sample.global.remove(&o);
                    sample.global.hits.push(hit);
                }
            }
        }
        Ok(())
    }

    fn post_collection(&mut self) -> Result<()> {
        if self.replayed {
            return self.subs.post_collection();
        }
        self.replayed = true;
        // Replay the sampled docs, segment by segment, in doc order.
        let mut all: Vec<(u32, u32, f32)> = Vec::new();
        for (ord, s) in self.samples.iter().enumerate() {
            if let Some(s) = s {
                for h in &s.global.hits {
                    all.push((h.doc, ord as u32, h.score));
                }
            }
        }
        all.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let segs: Vec<SegmentContext> = self.searcher.segments().to_vec();
        let mut i = 0;
        for seg in &segs {
            let end = seg.base + seg.max_doc();
            let start = i;
            while i < all.len() && all[i].0 < end {
                i += 1;
            }
            if start == i {
                continue;
            }
            self.subs.set_segment(seg)?;
            for &(doc, ord, score) in &all[start..i] {
                self.subs.push(doc - seg.base, ord, score)?;
            }
            self.subs.finish_segment()?;
        }
        self.subs.post_collection()
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        let aggs = self.subs.build(owning)?;
        Ok(owning
            .iter()
            .zip(aggs)
            .map(|(&o, aggs)| {
                let n = self.samples.get(o as usize).and_then(|s| s.as_ref()).map_or(0, |s| s.global.hits.len()) as u64;
                AggValue::Single(Box::new(SingleBucket { sampler: None, kind: SingleKind::Sampler, doc_count: n, aggs }))
            })
            .collect())
    }

    fn build_empty(&self) -> AggValue {
        AggValue::Single(Box::new(SingleBucket { sampler: None, kind: SingleKind::Sampler, doc_count: 0, aggs: self.subs.build_empty() }))
    }

    fn needs_scores(&self) -> bool {
        true
    }

    fn validate_sort(&self, name: &str, key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        single_bucket_validate_sort(name, &self.subs, key, rest)
    }

    fn sort_value(&self, _key: Option<&str>, rest: &[PathElement], ord: u32) -> f64 {
        if rest.is_empty() {
            self.samples.get(ord as usize).and_then(|s| s.as_ref()).map_or(0, |s| s.global.hits.len()) as f64
        } else {
            self.subs.sort_value(rest, ord)
        }
    }

    fn sort_is_count(&self, _key: Option<&str>, rest: &[PathElement]) -> bool {
        rest.is_empty() || self.subs.sort_is_count(rest)
    }
}
