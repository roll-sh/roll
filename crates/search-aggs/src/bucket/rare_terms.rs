//! `rare_terms`: the terms that appear in at most `max_doc_count` documents.
//!
//! Shards count every term, keep those at or under `max_doc_count` as buckets and put the others
//! in a "seen too often" set. The reduce sums the buckets per term and drops a term when its
//! total exceeds the limit or when any shard's set holds it (it was frequent there).
//! Elasticsearch's set switches to a scaling cuckoo filter past 10,000 terms, accepting false
//! positives (`precision`); this one stays exact, so it can only keep a rare term Elasticsearch
//! would have dropped by a filter collision. Buckets are ordered by count, then key, ascending.

use std::sync::Arc;

use search_index::SegmentContext;

use crate::bucket::include::{IncludeExclude, LongFilter, StringFilter};
use crate::bucket::terms::TermKey;
use crate::collect::{Aggregator, Batch, BindCtx, Subs, grow};
use crate::error::{AggError, Result};
use crate::format::DocValueFormat;
use crate::internal::{AggValue, Aggs, Internal, MultiBuckets, Prop, ReduceCtx, RenderCtx, multi_bucket_property};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::request::{AggDef, PathElement};
use crate::values::{BoundVs, BytesReader, DocCounter, NumReader, VsOpts, VsSpec, VsType, bind};
use crate::xparser::{Field, Tokens, XParser};

/// A parsed `rare_terms`.
#[derive(Clone, Debug)]
pub struct RareTermsSpec {
    pub vs: VsSpec,
    pub max_doc_count: u64,
    pub precision: f64,
    pub include_exclude: Option<IncludeExclude>,
}

/// `RareTermsAggregationBuilder.PARSER`.
pub fn parse(p: &mut XParser<'_>, name: &str) -> Result<RareTermsSpec> {
    let opts = VsOpts::new(true, true, false);
    let mut fields = VsSpec::fields(opts);
    fields.extend_from_slice(&[
        Field::new("max_doc_count", Tokens::LONG),
        Field::new("include", Tokens::OBJECT_ARRAY_OR_STRING),
        Field::new("exclude", Tokens::STRING_ARRAY),
        Field::new("precision", Tokens::FLOAT),
    ]);
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    let mut spec = RareTermsSpec { vs: VsSpec::default(), max_doc_count: 1, precision: 0.001, include_exclude: None };
    p.object("rare_terms", &fields, |p, i| {
        let f = names[i];
        if spec.vs.parse_field(p, f)? {
            return Ok(());
        }
        match f {
            "max_doc_count" => {
                let v = p.long_value()?;
                if v <= 0 {
                    return Err(AggError::illegal_argument(format!("[max_doc_count] must be greater than 0. Found [{v}] in [{name}]")));
                }
                if v > 100 {
                    return Err(AggError::illegal_argument(format!("[max_doc_count] must be smallerthan 100in [{name}]")));
                }
                spec.max_doc_count = v as u64;
            }
            "include" => {
                let inc = IncludeExclude::parse_include(p)?;
                spec.include_exclude = IncludeExclude::merge(Some(inc), spec.include_exclude.take())?;
            }
            "exclude" => {
                let exc = IncludeExclude::parse_exclude(p)?;
                spec.include_exclude = IncludeExclude::merge(spec.include_exclude.take(), Some(exc))?;
            }
            _ => {
                let v = p.double_value()?;
                if v < 0.00001 {
                    return Err(AggError::illegal_argument("[precision] must be greater than 0.00001"));
                }
                spec.precision = v;
            }
        }
        Ok(())
    })?;
    spec.vs.check_required(opts)?;
    Ok(spec)
}

/// Which result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RareKind {
    Str,
    Long,
    Unmapped,
}

/// A rare terms bucket.
#[derive(Clone, Debug)]
pub struct RareBucket {
    pub key: TermKey,
    pub doc_count: u64,
    pub aggs: Aggs,
}

/// `StringRareTerms` / `LongRareTerms` / `UnmappedRareTerms`.
#[derive(Clone, Debug)]
pub struct RareTerms {
    pub kind: RareKind,
    pub max_doc_count: u64,
    pub format: DocValueFormat,
    pub buckets: Vec<RareBucket>,
    /// Terms seen in more than `max_doc_count` docs somewhere (identity bytes).
    pub frequent: Arc<hashbrown::HashSet<Vec<u8>>>,
}

fn identity(k: &TermKey) -> Vec<u8> {
    let mut v = Vec::new();
    k.identity(&mut v);
    v
}

/// `count asc, key asc`.
fn rare_order(a: &RareBucket, b: &RareBucket) -> std::cmp::Ordering {
    a.doc_count.cmp(&b.doc_count).then_with(|| a.key.cmp_key(&b.key))
}

impl RareTerms {
    fn key_string(&self, k: &TermKey) -> String {
        match k {
            TermKey::Bytes(b) => match self.format.format_bytes(b) {
                Json::Str(s) => s,
                other => other.to_java_string(),
            },
            TermKey::Long(l) => self.format.long_string(*l),
            other => format!("{other:?}"),
        }
    }
}

impl Internal for RareTerms {
    fn type_name(&self) -> &'static str {
        match self.kind {
            RareKind::Str => "srareterms",
            RareKind::Long => "lrareterms",
            // UnmappedRareTerms reports `sterms` as its type.
            RareKind::Unmapped => "sterms",
        }
    }
    fn class_name(&self) -> &'static str {
        match self.kind {
            RareKind::Str => "StringRareTerms",
            RareKind::Long => "LongRareTerms",
            RareKind::Unmapped => "UnmappedRareTerms",
        }
    }
    fn must_reduce_single(&self) -> bool {
        true
    }
    fn can_lead(&self) -> bool {
        self.kind != RareKind::Unmapped
    }
    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let lead = list.iter().find(|t| t.kind != RareKind::Unmapped).cloned();
        let Some(mut out) = lead else { return Ok(list.into_iter().next().expect("one")) };
        let mut frequent: hashbrown::HashSet<Vec<u8>> = hashbrown::HashSet::new();
        let mut index: hashbrown::HashMap<Vec<u8>, usize> = hashbrown::HashMap::new();
        let mut groups: Vec<(TermKey, u64, Vec<Aggs>)> = Vec::new();
        for t in list {
            if t.kind == RareKind::Unmapped {
                continue;
            }
            if t.kind != out.kind {
                return Err(crate::internal::reduce_type_mismatch(""));
            }
            frequent.extend(t.frequent.iter().cloned());
            for b in t.buckets {
                let id = identity(&b.key);
                match index.get(&id) {
                    Some(&i) => {
                        groups[i].1 += b.doc_count;
                        groups[i].2.push(b.aggs);
                    }
                    None => {
                        index.insert(id, groups.len());
                        groups.push((b.key, b.doc_count, vec![b.aggs]));
                    }
                }
            }
        }
        let mut rare = Vec::new();
        for (key, doc_count, aggs) in groups {
            let id = identity(&key);
            if doc_count <= out.max_doc_count && !frequent.contains(&id) {
                let aggs = Aggs::reduce_lists(aggs, ctx)?;
                ctx.consume_buckets(1)?;
                rare.push(RareBucket { key, doc_count, aggs });
            } else if doc_count > out.max_doc_count {
                frequent.insert(id);
            }
        }
        rare.sort_by(rare_order);
        out.buckets = rare;
        out.frequent = Arc::new(frequent);
        Ok(out)
    }
    fn render(&self, out: &mut Json, rc: &RenderCtx) {
        let buckets = self
            .buckets
            .iter()
            .map(|b| {
                let mut o = Json::obj();
                match &b.key {
                    TermKey::Long(l) => {
                        o.push("key", Json::Int(*l));
                        if !self.format.is_raw() {
                            o.push("key_as_string", Json::Str(self.format.long_string(*l)));
                        }
                    }
                    k => o.push("key", Json::Str(self.key_string(k))),
                }
                o.push("doc_count", Json::from(b.doc_count));
                b.aggs.render_into(&mut o, rc);
                o
            })
            .collect();
        out.push("buckets", Json::Arr(buckets));
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_bucket_property(self, name, path)
    }
    fn ram_bytes(&self) -> usize {
        64 + self.buckets.iter().map(|b| 48 + b.aggs.ram_bytes()).sum::<usize>() + self.frequent.len() * 24
    }
}

impl MultiBuckets for RareTerms {
    fn len(&self) -> usize {
        self.buckets.len()
    }
    fn doc_count(&self, i: usize) -> u64 {
        self.buckets[i].doc_count
    }
    fn key(&self, i: usize) -> Json {
        match &self.buckets[i].key {
            TermKey::Long(l) => Json::Int(*l),
            k => Json::Str(self.key_string(k)),
        }
    }
    fn key_string(&self, i: usize) -> String {
        self.key_string(&self.buckets[i].key)
    }
    fn aggs(&self, i: usize) -> &Aggs {
        &self.buckets[i].aggs
    }
    fn aggs_mut(&mut self, i: usize) -> &mut Aggs {
        &mut self.buckets[i].aggs
    }
    fn retain_buckets(&mut self, keep: &[bool]) {
        let mut i = 0;
        self.buckets.retain(|_| {
            let k = keep[i];
            i += 1;
            k
        });
    }
    fn reorder(&mut self, order: &[usize]) {
        let mut slots: Vec<Option<RareBucket>> = self.buckets.drain(..).map(Some).collect();
        self.buckets = order.iter().map(|&i| slots[i].take().expect("bucket")).collect();
    }
}

/// Binds `rare_terms`.
pub fn create(def: &AggDef, spec: &RareTermsSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let vs = bind(&spec.vs, ctx.lookup, VsType::Keyword, false)?;
    if vs.is_counter() {
        return Err(vs.unsupported("rare_terms"));
    }
    let subs = Subs::bind(def, ctx)?;
    if vs.unmapped && vs.missing.is_none() {
        return Ok(Box::new(RareAgg::new(RareKind::Unmapped, spec, vs, None, None, subs, ctx, def)));
    }
    match vs.vs_type {
        VsType::Keyword | VsType::Ip => {
            if let Some(ie) = &spec.include_exclude
                && ie.is_regex_based()
                && !vs.format.is_raw()
            {
                return Err(AggError::illegal_argument(format!(
                    "Aggregation [{}] cannot support regular expression style include/exclude settings as they can only be applied to string fields. Use an array of values for include/exclude clauses",
                    def.name
                )));
            }
            let filter = match &spec.include_exclude {
                Some(ie) => Some(ie.string_filter(&vs.format)?),
                None => None,
            };
            Ok(Box::new(RareAgg::new(RareKind::Str, spec, vs, filter, None, subs, ctx, def)))
        }
        VsType::Numeric | VsType::Date | VsType::Boolean => {
            if let Some(ie) = &spec.include_exclude
                && ie.is_regex_based()
            {
                return Err(AggError::illegal_argument(format!(
                    "Aggregation [{}] cannot support regular expression style include/exclude settings as they can only be applied to string fields. Use an array of numeric values for include/exclude clauses used to filter numeric fields",
                    def.name
                )));
            }
            if !vs.is_integral() {
                return Err(AggError::illegal_argument("RareTerms aggregation does not support floating point fields."));
            }
            let filter = match &spec.include_exclude {
                Some(ie) => Some(ie.long_filter(&vs.format)?),
                None => None,
            };
            Ok(Box::new(RareAgg::new(RareKind::Long, spec, vs, None, filter, subs, ctx, def)))
        }
        _ => Err(vs.unsupported("rare_terms")),
    }
}

struct RareAgg {
    kind: RareKind,
    max_doc_count: u64,
    vs: BoundVs,
    sfilter: Option<StringFilter>,
    lfilter: Option<LongFilter>,
    bytes: Option<BytesReader>,
    nums: Option<NumReader>,
    /// `(owning, term identity)` → bucket ordinal.
    index: hashbrown::HashMap<(u32, Vec<u8>), u32>,
    keys: Vec<(u32, TermKey)>,
    counts: Vec<u64>,
    subs: Subs,
    doc_counter: DocCounter,
    doc_count_field: Option<u32>,
    tracker: MemTracker,
}

impl RareAgg {
    #[allow(clippy::too_many_arguments)]
    fn new(
        kind: RareKind,
        spec: &RareTermsSpec,
        vs: BoundVs,
        sfilter: Option<StringFilter>,
        lfilter: Option<LongFilter>,
        subs: Subs,
        ctx: &BindCtx<'_>,
        def: &AggDef,
    ) -> RareAgg {
        RareAgg {
            kind,
            max_doc_count: spec.max_doc_count,
            vs,
            sfilter,
            lfilter,
            bytes: None,
            nums: None,
            index: hashbrown::HashMap::new(),
            keys: Vec::new(),
            counts: Vec::new(),
            subs,
            doc_counter: DocCounter::none(),
            doc_count_field: ctx.doc_count_field,
            tracker: ctx.tracker(def),
        }
    }

    fn add(&mut self, owning: u32, key: TermKey, doc: u32, score: f32) -> Result<()> {
        let id = identity(&key);
        let ord = match self.index.get(&(owning, id.clone())) {
            Some(&o) => o,
            None => {
                let o = self.keys.len() as u32;
                self.index.insert((owning, id), o);
                self.keys.push((owning, key));
                grow(&mut self.counts, o, 0);
                self.tracker.ensure(self.keys.len() * 96)?;
                o
            }
        };
        self.counts[ord as usize] += self.doc_counter.count(doc);
        self.subs.push(doc, ord, score)
    }

    fn result(&self, buckets: Vec<RareBucket>, frequent: hashbrown::HashSet<Vec<u8>>) -> AggValue {
        AggValue::RareTerms(Box::new(RareTerms {
            kind: self.kind,
            max_doc_count: self.max_doc_count,
            format: self.vs.format.clone(),
            buckets,
            frequent: Arc::new(frequent),
        }))
    }
}

impl Aggregator for RareAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        match self.kind {
            RareKind::Str => self.bytes = Some(self.vs.bytes_reader(seg)?),
            RareKind::Long => self.nums = Some(self.vs.numeric_reader(seg)?),
            RareKind::Unmapped => {}
        }
        self.doc_counter = DocCounter::open(seg, self.doc_count_field);
        self.subs.set_segment(seg)
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        for i in 0..b.len() {
            let (doc, score) = (b.docs[i], b.score(i));
            let mut keys: smallvec::SmallVec<[TermKey; 2]> = smallvec::SmallVec::new();
            if let Some(r) = self.bytes.as_mut() {
                let n = r.load(doc, score)?;
                for j in 0..n {
                    let v = r.value(j);
                    if j > 0 && r.value(j - 1) == v {
                        continue;
                    }
                    if self.sfilter.as_ref().is_some_and(|f| !f.accept(v)) {
                        continue;
                    }
                    keys.push(TermKey::Bytes(v.to_vec()));
                }
            } else if let Some(r) = self.nums.as_mut() {
                let mut prev: Option<i64> = None;
                for &v in r.longs(doc, score)? {
                    if prev == Some(v) {
                        continue;
                    }
                    prev = Some(v);
                    if self.lfilter.as_ref().is_some_and(|f| !f.accept(v)) {
                        continue;
                    }
                    keys.push(TermKey::Long(v));
                }
            }
            for k in keys {
                self.add(b.ords[i], k, doc, score)?;
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
        let mut kept: Vec<Vec<u32>> = vec![Vec::new(); owning.len()];
        let mut frequent: Vec<hashbrown::HashSet<Vec<u8>>> = vec![hashbrown::HashSet::new(); owning.len()];
        for (ord, (o, key)) in self.keys.iter().enumerate() {
            let Some(&i) = pos.get(o) else { continue };
            if self.counts[ord] <= self.max_doc_count {
                kept[i].push(ord as u32);
            } else {
                frequent[i].insert(identity(key));
            }
        }
        let all: Vec<u32> = kept.iter().flatten().copied().collect();
        let mut aggs = self.subs.build(&all)?.into_iter();
        let mut out = Vec::with_capacity(owning.len());
        for (list, freq) in kept.into_iter().zip(frequent) {
            let mut buckets: Vec<RareBucket> = list
                .into_iter()
                .map(|o| RareBucket { key: self.keys[o as usize].1.clone(), doc_count: self.counts[o as usize], aggs: aggs.next().unwrap_or_default() })
                .collect();
            buckets.sort_by(rare_order);
            out.push(self.result(buckets, freq));
        }
        Ok(out)
    }

    fn build_empty(&self) -> AggValue {
        let mut v = self.result(Vec::new(), hashbrown::HashSet::new());
        if let AggValue::RareTerms(r) = &mut v {
            // buildEmptyAggregation passes 0 as the max doc count.
            r.max_doc_count = 0;
        }
        v
    }

    fn needs_scores(&self) -> bool {
        self.subs.needs_scores() || self.vs.script.as_ref().is_some_and(|s| s.needs_score())
    }

    fn validate_sort(&self, _name: &str, _key: Option<&str>, _rest: &[PathElement]) -> Result<()> {
        Err(AggError::illegal_argument(
            "Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end.",
        ))
    }
}
