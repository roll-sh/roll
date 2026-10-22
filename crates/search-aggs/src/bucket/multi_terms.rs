//! `multi_terms`: buckets per combination of several fields' (or scripts') values.
//!
//! Each document yields the cartesian product of its sources' distinct values (a source without
//! values excludes the document). Shards keep the top `shard_size` combinations by the order
//! (`_count desc` with `_key asc` as tie-breaker by default), report the rest in
//! `sum_other_doc_count` and return their buckets sorted by key; the reduce is `terms`'
//! (doc count errors included), on keys compared term by term.

use std::cmp::Ordering;
use std::sync::Arc;

use search_index::SegmentContext;

use crate::bucket::terms::{TermKey, Terms, TermsBucket, TermsConfig, TermsKind, adjust_thresholds, compare_discard_nan, order_of, validate_order};
use crate::collect::{Aggregator, Batch, BindCtx, Subs, grow};
use crate::error::{AggError, Result};
use crate::format::{DocValueFormat, TimeZone};
use crate::internal::AggValue;
use crate::memory::MemTracker;
use crate::request::{AggDef, BucketOrder, PathElement};
use crate::values::{BoundVs, BytesReader, DocCounter, NumReader, ValueType, VsSpec, VsType, bind};
use crate::xparser::{Field, Token, Tokens, XParser};

/// One `terms` entry (`MultiValuesSourceFieldConfig`: field or script, missing, time zone,
/// value type, format).
#[derive(Clone, Debug, Default)]
pub struct TermSource {
    pub vs: VsSpec,
}

/// A parsed `multi_terms`.
#[derive(Clone, Debug)]
pub struct MultiTermsSpec {
    pub terms: Vec<TermSource>,
    pub size: usize,
    pub shard_size: Option<usize>,
    pub min_doc_count: u64,
    pub shard_min_doc_count: u64,
    pub order: BucketOrder,
    pub show_term_doc_count_error: bool,
}

fn parse_term_source(p: &mut XParser<'_>) -> Result<TermSource> {
    let fields = [
        Field::new("field", Tokens::STRING),
        Field::new("missing", Tokens::VALUE),
        Field::new("script", Tokens::OBJECT_OR_STRING),
        Field::new("time_zone", Tokens::LONG),
        Field::new("value_type", Tokens::STRING),
        Field::new("format", Tokens::STRING),
    ];
    let mut vs = VsSpec::default();
    p.object("field_config", &fields, |p, i| {
        match i {
            0 => vs.field = Some(p.text()?),
            1 => vs.missing = p.object_text()?,
            2 => vs.script = Some(crate::script::Script::parse(p)?),
            3 => vs.time_zone = Some(if p.token() == Token::String { TimeZone::parse(&p.text()?)? } else { TimeZone::of_hours(p.int_value()? as i64)? }),
            4 => vs.value_type = ValueType::lenient_parse(&p.text()?),
            _ => vs.format = Some(p.text()?),
        }
        Ok(())
    })?;
    let empty = vs.field.as_deref().is_none_or(str::is_empty);
    if empty && vs.script.is_none() {
        return Err(AggError::illegal_argument("[field] and [script] cannot both be null.  Please specify one or the other."));
    }
    if !empty && vs.script.is_some() {
        return Err(AggError::illegal_argument("[field] and [script] cannot both be configured.  Please specify one or the other."));
    }
    Ok(TermSource { vs })
}

/// `MultiTermsAggregationBuilder.PARSER`.
pub fn parse(p: &mut XParser<'_>, name: &str) -> Result<MultiTermsSpec> {
    let fields = [
        Field::new("show_term_doc_count_error", Tokens::BOOLEAN),
        Field::new("terms", Tokens::OBJECT_ARRAY),
        Field::new("shard_size", Tokens::INT),
        Field::new("min_doc_count", Tokens::LONG),
        Field::new("shard_min_doc_count", Tokens::LONG),
        Field::new("size", Tokens::INT),
        Field::new("order", Tokens::OBJECT_ARRAY),
        Field::new("collect_mode", Tokens::STRING),
    ];
    let mut spec = MultiTermsSpec {
        terms: Vec::new(),
        size: 10,
        shard_size: None,
        min_doc_count: 1,
        shard_min_doc_count: 0,
        order: BucketOrder::compound(vec![BucketOrder::COUNT_DESC]),
        show_term_doc_count_error: false,
    };
    p.object("multi_terms", &fields, |p, i| {
        match i {
            0 => spec.show_term_doc_count_error = p.boolean_value()?,
            1 => {
                let list = p.array_items(parse_term_source)?;
                if list.len() < 2 {
                    return Err(AggError::illegal_argument(format!(
                        "The [terms] parameter in the aggregation [{name}] must be present and have at least 2 fields or scripts.{}",
                        if list.len() == 1 { " For a single field user terms aggregation." } else { "" }
                    )));
                }
                if list.len() > 128 {
                    return Err(AggError::illegal_argument(format!(
                        "The [terms] parameter in the aggregation [{name}] cannot have more than 128 fields or scripts. Found [{}].",
                        list.len()
                    )));
                }
                spec.terms = list;
            }
            2 => {
                let v = p.int_value()?;
                if v <= 0 {
                    return Err(AggError::illegal_argument(format!("[shardSize] must be greater than 0. Found [{v}] in [{name}]")));
                }
                spec.shard_size = Some(v as usize);
            }
            3 => {
                let v = p.long_value()?;
                if v < 1 {
                    return Err(AggError::illegal_argument(format!("[minDocCount] must be greater than or equal to 1. Found [{v}] in [{name}]")));
                }
                spec.min_doc_count = v as u64;
            }
            4 => {
                let v = p.long_value()?;
                if v < 0 {
                    return Err(AggError::illegal_argument(format!("[shardMinDocCount] must be greater than or equal to 0. Found [{v}] in [{name}]")));
                }
                spec.shard_min_doc_count = v as u64;
            }
            5 => {
                let v = p.int_value()?;
                if v <= 0 {
                    return Err(AggError::illegal_argument(format!("[size] must be greater than 0. Found [{v}] in [{name}]")));
                }
                spec.size = v as usize;
            }
            6 => spec.order = order_of(BucketOrder::parse_list(p)?),
            _ => {
                crate::bucket::terms::CollectMode::parse(&p.text()?)?;
            }
        }
        Ok(())
    })?;
    Ok(spec)
}

/// How one source reads.
enum Reader {
    None,
    Bytes(BytesReader),
    Num(NumReader),
}

struct Source {
    vs: BoundVs,
    bytes: bool,
    double: bool,
    reader: Reader,
    vals: Vec<TermKey>,
}

impl Source {
    fn load(&mut self, doc: u32, score: f32) -> Result<()> {
        self.vals.clear();
        match &mut self.reader {
            Reader::None => {}
            Reader::Bytes(r) => {
                let n = r.load(doc, score)?;
                for i in 0..n {
                    let v = r.value(i);
                    if matches!(self.vals.last(), Some(TermKey::Bytes(p)) if p.as_slice() == v) {
                        continue;
                    }
                    self.vals.push(TermKey::Bytes(v.to_vec()));
                }
            }
            Reader::Num(r) => {
                if self.double {
                    for &v in r.doubles(doc, score)? {
                        if !matches!(self.vals.last(), Some(TermKey::Double(p)) if *p == v) {
                            self.vals.push(TermKey::Double(v));
                        }
                    }
                } else {
                    for &v in r.longs(doc, score)? {
                        if !matches!(self.vals.last(), Some(TermKey::Long(p)) if *p == v) {
                            self.vals.push(TermKey::Long(v));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Binds `multi_terms`.
pub fn create(def: &AggDef, spec: &MultiTermsSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    if spec.terms.is_empty() {
        // No `terms` at all: Elasticsearch fails on the first source of an empty list.
        return Err(AggError::new(500, "index_out_of_bounds_exception", "Index 0 out of bounds for length 0"));
    }
    let mut sources = Vec::with_capacity(spec.terms.len());
    for t in &spec.terms {
        let vs = bind(&t.vs, ctx.lookup, VsType::Keyword, false)?;
        let (bytes, double) = match vs.vs_type {
            VsType::Keyword | VsType::Ip => (true, false),
            VsType::Numeric | VsType::Date | VsType::Boolean => (false, !vs.is_integral()),
            _ if vs.unmapped => (true, false),
            // `ExponentialHistogramFieldType`'s values are read as strings, which it refuses.
            VsType::GeoShape | VsType::CartesianShape => {
                return Err(AggError::new(500, "unsupported_operation_exception", "scripts and term aggs are not supported by geo_shape or shape doc values"));
            }
            VsType::ExponentialHistogram => {
                return Err(AggError::new(500, "unsupported_operation_exception", "String representation of doc values for [exponential_histogram] fields is not supported"));
            }
            _ => return Err(vs.unsupported("multi_terms")),
        };
        sources.push(Source { vs, bytes, double, reader: Reader::None, vals: Vec::new() });
    }
    let subs = Subs::bind(def, ctx)?;
    validate_order(&spec.order, &subs)?;
    let (size, shard_size, shard_min_doc_count) = adjust_thresholds(spec.size, spec.shard_size, spec.min_doc_count, spec.shard_min_doc_count, &spec.order);
    let cfg = Arc::new(TermsConfig {
        order: spec.order.clone(),
        size,
        shard_size,
        min_doc_count: spec.min_doc_count,
        shard_min_doc_count,
        show_term_doc_count_error: spec.show_term_doc_count_error,
        format: DocValueFormat::Raw,
    });
    Ok(Box::new(MultiTermsAgg {
        cfg,
        formats: sources.iter().map(|s| s.vs.format.clone()).collect(),
        sources,
        index: hashbrown::HashMap::new(),
        idbuf: Vec::new(),
        keys: Vec::new(),
        counts: Vec::new(),
        subs,
        doc_counter: DocCounter::none(),
        doc_count_field: ctx.doc_count_field,
        tracker: ctx.tracker(def),
    }))
}

struct MultiTermsAgg {
    cfg: Arc<TermsConfig>,
    formats: Vec<DocValueFormat>,
    sources: Vec<Source>,
    /// Owning ordinal + key identity bytes → bucket ordinal.
    index: hashbrown::HashMap<Vec<u8>, u32>,
    idbuf: Vec<u8>,
    /// Per ordinal: owning ordinal and key.
    keys: Vec<(u32, Vec<TermKey>)>,
    counts: Vec<u64>,
    subs: Subs,
    doc_counter: DocCounter,
    doc_count_field: Option<u32>,
    tracker: MemTracker,
}

fn key_cmp(a: &[TermKey], b: &[TermKey]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let c = x.cmp_key(y);
        if c != Ordering::Equal {
            return c;
        }
    }
    Ordering::Equal
}

impl MultiTermsAgg {
    fn add(&mut self, owning: u32, key: &[TermKey], doc: u32, score: f32) -> Result<()> {
        self.idbuf.clear();
        self.idbuf.extend_from_slice(&owning.to_le_bytes());
        for part in key {
            part.identity(&mut self.idbuf);
        }
        let ord = match self.index.get(self.idbuf.as_slice()) {
            Some(&o) => o,
            None => {
                let o = self.keys.len() as u32;
                self.index.insert(self.idbuf.clone(), o);
                self.keys.push((owning, key.to_vec()));
                grow(&mut self.counts, o, 0);
                self.tracker.ensure(self.keys.len() * (64 + key.len() * 32))?;
                o
            }
        };
        self.counts[ord as usize] += self.doc_counter.count(doc);
        self.subs.push(doc, ord, score)
    }

    /// `partiallyBuiltBucketComparator`.
    fn cmp(&self, a: u32, b: u32) -> Ordering {
        for o in self.cfg.order.elements() {
            let c = match o {
                BucketOrder::Count { asc } => {
                    let c = self.counts[a as usize].cmp(&self.counts[b as usize]);
                    if *asc { c } else { c.reverse() }
                }
                BucketOrder::Key { asc } => {
                    let c = key_cmp(&self.keys[a as usize].1, &self.keys[b as usize].1);
                    if *asc { c } else { c.reverse() }
                }
                BucketOrder::Agg { path, asc } => {
                    let (x, y) = (self.subs.sort_value(&path.elements, a), self.subs.sort_value(&path.elements, b));
                    if self.subs.sort_is_count(&path.elements) {
                        let c = (x as u64).cmp(&(y as u64));
                        if *asc { c } else { c.reverse() }
                    } else {
                        compare_discard_nan(x, y, *asc)
                    }
                }
                BucketOrder::Compound(_) => Ordering::Equal,
            };
            if c != Ordering::Equal {
                return c;
            }
        }
        Ordering::Equal
    }

    fn result(&self, buckets: Vec<TermsBucket>, other: i64) -> AggValue {
        let key_order = self.cfg.order.is_key_order();
        AggValue::Terms(Box::new(Terms {
            kind: TermsKind::Multi,
            buckets,
            reduce_order: if key_order { self.cfg.order.clone() } else { BucketOrder::KEY_ASC },
            cfg: self.cfg.clone(),
            // InternalMultiTerms shard results carry a doc count error of 0 (not null), which
            // the reduce then uses instead of the last bucket's count.
            doc_count_error: Some(0),
            other_doc_count: other,
            formats: self.formats.clone(),
        }))
    }
}

impl Aggregator for MultiTermsAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        for s in &mut self.sources {
            s.reader = if s.vs.unmapped && s.vs.missing.is_none() {
                Reader::None
            } else if s.bytes {
                Reader::Bytes(s.vs.bytes_reader(seg)?)
            } else {
                Reader::Num(s.vs.numeric_reader(seg)?)
            };
        }
        self.doc_counter = DocCounter::open(seg, self.doc_count_field);
        self.subs.set_segment(seg)
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        for i in 0..b.len() {
            let (doc, score) = (b.docs[i], b.score(i));
            let mut any_empty = false;
            for s in &mut self.sources {
                s.load(doc, score)?;
                if s.vals.is_empty() {
                    any_empty = true;
                    break;
                }
            }
            if any_empty {
                continue;
            }
            // Cartesian product, first source outermost.
            let n = self.sources.len();
            let mut idx = vec![0usize; n];
            loop {
                let key: Vec<TermKey> = (0..n).map(|j| self.sources[j].vals[idx[j]].clone()).collect();
                self.add(b.ords[i], &key, doc, score)?;
                let mut d = n;
                let mut done = true;
                while d > 0 {
                    d -= 1;
                    idx[d] += 1;
                    if idx[d] < self.sources[d].vals.len() {
                        done = false;
                        break;
                    }
                    idx[d] = 0;
                }
                if done {
                    break;
                }
            }
        }
        Ok(())
    }

    fn finish_segment(&mut self) -> Result<()> {
        self.subs.finish_segment()
    }

    fn post_collection(&mut self) -> Result<()> {
        self.subs.post_collection()
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        let pos: hashbrown::HashMap<u32, usize> = owning.iter().enumerate().map(|(i, &o)| (o, i)).collect();
        let mut per: Vec<Vec<u32>> = vec![Vec::new(); owning.len()];
        for (ord, (o, _)) in self.keys.iter().enumerate() {
            if let Some(&i) = pos.get(o) {
                per[i].push(ord as u32);
            }
        }
        let shard_size = self.cfg.shard_size;
        let mut selected = Vec::with_capacity(owning.len());
        for list in per {
            let mut other: i64 = list.iter().map(|&o| self.counts[o as usize] as i64).sum();
            let mut keep: Vec<u32> = list.into_iter().filter(|&o| self.counts[o as usize] >= self.cfg.shard_min_doc_count).collect();
            if keep.len() > shard_size {
                keep.select_nth_unstable_by(shard_size - 1, |&a, &b| self.cmp(a, b));
                keep.truncate(shard_size);
            }
            keep.sort_by(|&a, &b| self.cmp(a, b));
            for &o in &keep {
                other -= self.counts[o as usize] as i64;
            }
            selected.push((keep, other));
        }
        let all: Vec<u32> = selected.iter().flat_map(|(k, _)| k.iter().copied()).collect();
        let mut sub_aggs = self.subs.build(&all)?.into_iter();
        let mut out = Vec::with_capacity(owning.len());
        for (keep, other) in selected {
            let mut buckets: Vec<TermsBucket> = keep
                .iter()
                .map(|&o| TermsBucket {
                    key: TermKey::Multi(self.keys[o as usize].1.clone()),
                    doc_count: self.counts[o as usize],
                    doc_count_error: 0,
                    aggs: sub_aggs.next().unwrap_or_default(),
                })
                .collect();
            if !self.cfg.order.is_key_order() {
                buckets.sort_by(|a, b| a.key.cmp_key(&b.key));
            }
            out.push(self.result(buckets, other));
        }
        Ok(out)
    }

    fn build_empty(&self) -> AggValue {
        let mut v = self.result(Vec::new(), 0);
        if let AggValue::Terms(t) = &mut v {
            t.reduce_order = self.cfg.order.clone();
        }
        v
    }

    fn needs_scores(&self) -> bool {
        self.subs.needs_scores() || self.sources.iter().any(|s| s.vs.script.as_ref().is_some_and(|x| x.needs_score()))
    }

    fn validate_sort(&self, _name: &str, _key: Option<&str>, _rest: &[PathElement]) -> Result<()> {
        Err(AggError::illegal_argument(
            "Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end.",
        ))
    }
}
