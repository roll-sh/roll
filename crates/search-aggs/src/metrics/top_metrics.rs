//! `top_metrics`: the metric values of the best documents of each bucket by one sort.
//!
//! Each bucket keeps its `size` best documents (an equal sort value is not competitive, so the
//! first one seen stays) with the first value of every metric field, read when the document
//! enters. The reduce merges the shard lists by sort value. Values print as their field's format
//! (strings for dates, booleans, ips; raw numbers and terms otherwise), as Elasticsearch's
//! `SortValue` does.

use search_index::SegmentContext;

use crate::collect::{Aggregator, Batch, BindCtx};
use crate::error::{AggError, Result};
use crate::field::HitSort;
use crate::format::DocValueFormat;
use crate::internal::{AggValue, Internal, Prop, ReduceCtx, RenderCtx, java_list};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::request::{AggDef, PathElement};
use crate::sort::{DocSort, SegmentSort, SortKey, SortVal};
use crate::util::LucenePq;
use crate::values::{BoundVs, BytesReader, NumReader, VsSpec, VsType, bind, field_format};
use crate::xparser::{Field, Tokens, XParser};

/// One gathered or heaped document: its sort key and metric values.
type Slot = Option<(SortKey, Vec<Option<MetricVal>>)>;

/// A parsed `top_metrics`.
#[derive(Clone, Debug)]
pub struct TopMetricsSpec {
    pub sort: Json,
    pub size: usize,
    pub metrics: Vec<VsSpec>,
}

/// How many sorts a `sort` value holds (`SortBuilder.fromXContent`).
fn sort_count(v: &Json) -> usize {
    match v {
        Json::Arr(a) => a.iter().map(sort_count).sum(),
        Json::Obj(o) => o.len(),
        _ => 1,
    }
}

/// `TopMetricsAggregationBuilder.PARSER` (a constructing parser: checks run at the end).
pub fn parse(p: &mut XParser<'_>) -> Result<TopMetricsSpec> {
    let fields = [Field::new("sort", Tokens::OBJECT_ARRAY_OR_STRING), Field::new("size", Tokens::INT), Field::new("metrics", Tokens::OBJECT_ARRAY)];
    let (mut sort, mut size, mut metrics): (Option<Json>, Option<i32>, Option<Vec<VsSpec>>) = (None, None, None);
    p.object("top_metrics", &fields, |p, i| {
        match i {
            0 => sort = Some(p.take_value()),
            1 => size = Some(p.int_value()?),
            _ => {
                metrics = Some(p.array_items(|p| {
                    let f = crate::metrics::simple::parse_field_config(p, true)?;
                    Ok(f.to_vs())
                })?)
            }
        }
        Ok(())
    })?;
    let mut missing = Vec::new();
    if sort.is_none() {
        missing.push("sort");
    }
    if metrics.is_none() {
        missing.push("metrics");
    }
    if !missing.is_empty() {
        return Err(AggError::illegal_argument(format!("Required [{}]", missing.join(", "))));
    }
    let wrap = |e: AggError| AggError::new(400, "x_content_parse_exception", "Failed to build [top_metrics] after last required field arrived").caused_by(e);
    let size = size.unwrap_or(1);
    if size < 1 {
        return Err(wrap(AggError::illegal_argument(format!("[size] must be more than 0 but was [{size}]"))));
    }
    let sort = sort.expect("checked");
    if sort_count(&sort) != 1 {
        return Err(wrap(AggError::illegal_argument("[sort] must contain exactly one sort")));
    }
    Ok(TopMetricsSpec { sort, size: size as usize, metrics: metrics.expect("checked") })
}

/// A metric value (`SortValue`) with its format.
#[derive(Clone, Debug, PartialEq)]
pub enum MetricVal {
    Long(i64),
    Double(f64),
    Bytes(Vec<u8>),
}

fn render_val(v: &MetricVal, f: &DocValueFormat) -> Json {
    match (v, f) {
        (MetricVal::Long(l), DocValueFormat::Raw) => Json::Int(*l),
        (MetricVal::Double(d), DocValueFormat::Raw) => Json::Float(*d),
        (MetricVal::Bytes(b), DocValueFormat::Raw) => Json::Str(String::from_utf8_lossy(b).into_owned()),
        (MetricVal::Long(l), f) => Json::Str(f.long_string(*l)),
        (MetricVal::Double(d), f) => Json::Str(f.double_string(*d)),
        (MetricVal::Bytes(b), f) => match f.format_bytes(b) {
            Json::Str(s) => Json::Str(s),
            o => Json::Str(o.to_java_string()),
        },
    }
}

/// One top document.
#[derive(Clone, Debug)]
pub struct TopMetric {
    pub sort: SortVal,
    pub metrics: Vec<Option<MetricVal>>,
}

/// `InternalTopMetrics`.
#[derive(Clone, Debug)]
pub struct TopMetrics {
    pub size: usize,
    pub reverse: bool,
    pub sort_format: DocValueFormat,
    pub names: Vec<String>,
    pub formats: Vec<DocValueFormat>,
    pub top: Vec<TopMetric>,
}

fn sort_val_json(v: &SortVal, f: &DocValueFormat) -> Json {
    let m = match v {
        SortVal::Long(l) => MetricVal::Long(*l),
        SortVal::Double(d) => MetricVal::Double(*d),
        SortVal::Float(x) => MetricVal::Double(*x as f64),
        SortVal::Score(s) => MetricVal::Double(*s as f64),
        SortVal::Doc(d) => MetricVal::Long(*d as i64),
        SortVal::Bytes(b) => MetricVal::Bytes(b.clone()),
        SortVal::Null => return Json::Null,
    };
    render_val(&m, f)
}

/// `SortValue.compareTo` (doubles with `Double.compare`).
fn cmp_sort(a: &SortVal, b: &SortVal) -> std::cmp::Ordering {
    let num = |v: &SortVal| match v {
        SortVal::Long(l) => Some(*l as f64),
        SortVal::Double(d) => Some(*d),
        SortVal::Float(f) => Some(*f as f64),
        SortVal::Score(s) => Some(*s as f64),
        SortVal::Doc(d) => Some(*d as f64),
        _ => None,
    };
    match (a, b) {
        (SortVal::Long(x), SortVal::Long(y)) => x.cmp(y),
        (SortVal::Bytes(x), SortVal::Bytes(y)) => x.cmp(y),
        _ => match (num(a), num(b)) {
            (Some(x), Some(y)) => crate::request::java_double_compare(x, y),
            _ => std::cmp::Ordering::Equal,
        },
    }
}

impl Internal for TopMetrics {
    fn type_name(&self) -> &'static str {
        "top_metrics"
    }
    fn class_name(&self) -> &'static str {
        "InternalTopMetrics"
    }
    fn can_lead(&self) -> bool {
        !self.top.is_empty()
    }
    fn reduce(list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let leads: Vec<TopMetrics> = list.into_iter().filter(|t| !t.top.is_empty()).collect();
        if leads.len() <= 1 {
            return Ok(leads.into_iter().next().unwrap_or(TopMetrics { size: 0, reverse: false, sort_format: DocValueFormat::Raw, names: Vec::new(), formats: Vec::new(), top: Vec::new() }));
        }
        let first = leads[0].clone();
        let mul = if first.reverse { -1 } else { 1 };
        // A priority queue of (result, position) by the current entry's sort value.
        let less = |a: &(usize, usize), b: &(usize, usize)| {
            let c = cmp_sort(&leads[a.0].top[a.1].sort, &leads[b.0].top[b.1].sort);
            (c as i32) * mul < 0
        };
        let mut pq = LucenePq::new(leads.len());
        for i in 0..leads.len() {
            pq.add((i, 0usize), &less);
        }
        let mut merged = Vec::with_capacity(first.size);
        while merged.len() < first.size {
            let Some(&(r, pos)) = pq.top() else { break };
            merged.push(leads[r].top[pos].clone());
            if pos + 1 < leads[r].top.len() {
                if let Some(t) = pq.top_mut() {
                    t.1 += 1;
                }
                pq.update_top(&less);
            } else {
                pq.pop(&less);
            }
        }
        Ok(TopMetrics { top: merged, ..first })
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        let top = self
            .top
            .iter()
            .map(|t| {
                let mut m = Json::obj();
                for ((name, v), f) in self.names.iter().zip(&t.metrics).zip(&self.formats) {
                    m.push(name.clone(), v.as_ref().map_or(Json::Null, |v| render_val(v, f)));
                }
                Json::obj().with("sort", Json::Arr(vec![sort_val_json(&t.sort, &self.sort_format)])).with("metrics", m)
            })
            .collect();
        out.push("top", Json::Arr(top));
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        let unsupported = || AggError::illegal_argument(format!("path not supported for [{name}]: {}", java_list(path)));
        if path.len() != 1 {
            return Err(unsupported());
        }
        let Some(i) = self.names.iter().position(|n| *n == path[0]) else { return Err(unsupported()) };
        let Some(t) = self.top.first() else { return Ok(Prop::Value(Json::Null)) };
        Ok(match &t.metrics[i] {
            None => Prop::Num(f64::NAN),
            Some(MetricVal::Long(l)) => Prop::Num(*l as f64),
            Some(MetricVal::Double(d)) => Prop::Num(*d),
            Some(MetricVal::Bytes(b)) => Prop::Value(Json::Str(String::from_utf8_lossy(b).into_owned())),
        })
    }
}

/// Binds `top_metrics`.
pub fn create(def: &AggDef, spec: &TopMetricsSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let max = 10;
    if spec.size > max {
        return Err(AggError::illegal_argument(format!(
            "[top_metrics.size] must not be more than [{max}] but was [{}]. This limit can be set by changing the [index.top_metrics_max_size] index level setting.",
            spec.size
        )));
    }
    let mut metrics = Vec::with_capacity(spec.metrics.len());
    for m in &spec.metrics {
        let vs = bind(m, ctx.lookup, VsType::Numeric, false)?;
        if !vs.unmapped && !matches!(vs.vs_type, VsType::Numeric | VsType::Date | VsType::Boolean | VsType::Keyword | VsType::Ip) {
            return Err(vs.unsupported("top_metrics"));
        }
        metrics.push(Metric { name: m.field.clone().unwrap_or_default(), vs, num: None, bytes: None });
    }
    let keys = ctx.lookup.hit_sort(&spec.sort)?;
    if let Some(HitSort::Field { field, .. }) = keys.first()
        && !field.source.is_numeric_like()
    {
        return Err(AggError::illegal_argument(format!(
            "error building sort for field [{}] of type [{}] in index [{}]: only supported on numeric fields",
            field.name,
            field.type_name,
            ctx.lookup.index_name()
        )));
    }
    let (reverse, sort_format) = match keys.first() {
        Some(HitSort::Field { field, reverse, .. }) => (*reverse, field_format(field, None, None, ctx.lookup)?),
        Some(HitSort::Score { reverse }) => (!*reverse, DocValueFormat::Raw),
        Some(HitSort::Doc { reverse }) => (*reverse, DocValueFormat::Raw),
        None => (false, DocValueFormat::Raw),
    };
    Ok(Box::new(TopMetricsAgg {
        size: spec.size,
        reverse,
        sort: DocSort::new(&keys)?,
        sort_format,
        seg_sort: None,
        metrics,
        buckets: Vec::new(),
        tracker: ctx.tracker(def),
    }))
}

struct Metric {
    name: String,
    vs: BoundVs,
    num: Option<NumReader>,
    bytes: Option<BytesReader>,
}

/// One bucket of a `BucketedSort`: slots filled from the end ("gathering"), then a heap with
/// the worst entry at the root that a strictly better document replaces.
struct BucketSlots {
    slots: Vec<Slot>,
    next: usize,
    heap: bool,
}

struct TopMetricsAgg {
    size: usize,
    reverse: bool,
    sort: DocSort,
    sort_format: DocValueFormat,
    seg_sort: Option<SegmentSort>,
    metrics: Vec<Metric>,
    /// Per bucket: Elasticsearch's `BucketedSort` slots.
    buckets: Vec<Option<BucketSlots>>,
    tracker: MemTracker,
}

impl TopMetricsAgg {
    fn load_metrics(&mut self, doc: u32, score: f32) -> Result<Vec<Option<MetricVal>>> {
        let mut out = Vec::with_capacity(self.metrics.len());
        for m in &mut self.metrics {
            let v = if let Some(r) = m.num.as_mut() {
                // MultiValueMode.AVG: the mean (rounded for longs).
                if m.vs.is_integral() || m.vs.vs_type != VsType::Numeric {
                    let v = r.longs(doc, score)?;
                    match v.len() {
                        0 => None,
                        1 => Some(MetricVal::Long(v[0])),
                        n => {
                            let total = v.iter().fold(0i64, |a, &x| a.wrapping_add(x));
                            Some(MetricVal::Long(crate::format::java_round(total as f64 / n as f64)))
                        }
                    }
                } else {
                    let v = r.doubles(doc, score)?;
                    (!v.is_empty()).then(|| MetricVal::Double(v.iter().sum::<f64>() / v.len() as f64))
                }
            } else if let Some(r) = m.bytes.as_mut() {
                let n = r.load(doc, score)?;
                (n > 0).then(|| MetricVal::Bytes(r.value(0).to_vec()))
            } else {
                None
            };
            out.push(v);
        }
        Ok(out)
    }

    fn result(&self, entries: Vec<(SortKey, Vec<Option<MetricVal>>)>) -> AggValue {
        AggValue::TopMetrics(Box::new(TopMetrics {
            size: self.size,
            reverse: self.reverse,
            sort_format: self.sort_format.clone(),
            names: self.metrics.iter().map(|m| m.name.clone()).collect(),
            formats: self.metrics.iter().map(|m| m.vs.format.clone()).collect(),
            top: entries.into_iter().map(|(k, metrics)| TopMetric { sort: k.into_iter().next().unwrap_or(SortVal::Null), metrics }).collect(),
        }))
    }
}

/// `BucketedSort.downHeap`: the worst entry rises to the root.
fn down_heap(sort: &DocSort, slots: &mut [Slot], mut parent: usize) {
    let n = slots.len();
    let better = |slots: &[Slot], a: usize, b: usize| -> bool {
        // betterThan(a, b): a sorts strictly before b.
        match (&slots[a], &slots[b]) {
            (Some(x), Some(y)) => sort.compare(&x.0, &y.0) == std::cmp::Ordering::Less,
            _ => false,
        }
    };
    loop {
        let mut worst = parent;
        let left = parent * 2 + 1;
        if left < n {
            if better(slots, worst, left) {
                worst = left;
            }
            let right = left + 1;
            if right < n && better(slots, worst, right) {
                worst = right;
            }
        }
        if worst == parent {
            break;
        }
        slots.swap(worst, parent);
        parent = worst;
    }
}

impl Aggregator for TopMetricsAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.seg_sort = Some(self.sort.segment(seg));
        for m in &mut self.metrics {
            m.num = None;
            m.bytes = None;
            if m.vs.unmapped && m.vs.missing.is_none() {
                continue;
            }
            match m.vs.vs_type {
                VsType::Keyword | VsType::Ip => m.bytes = Some(m.vs.bytes_reader(seg)?),
                _ => m.num = Some(m.vs.numeric_reader(seg)?),
            }
        }
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        let size = self.size;
        for i in 0..b.len() {
            let (doc, score) = (b.docs[i], b.score(i));
            let key = self.seg_sort.as_mut().expect("segment").key(&self.sort, doc, score);
            let ord = b.ords[i] as usize;
            if self.buckets.len() <= ord {
                self.buckets.resize_with(ord + 1, || None);
                self.tracker.ensure(self.buckets.len() * (32 + size * 64))?;
            }
            let bucket = self.buckets[ord].get_or_insert_with(|| BucketSlots { slots: vec![None; size], next: size - 1, heap: false });
            if bucket.heap {
                // docBetterThan(root): strictly better than the worst kept.
                let better = matches!(&bucket.slots[0], Some((k, _)) if self.sort.compare(&key, k) == std::cmp::Ordering::Less);
                if better {
                    let metrics = self.load_metrics(doc, score)?;
                    let bucket = self.buckets[ord].as_mut().expect("bucket");
                    bucket.slots[0] = Some((key, metrics));
                    down_heap(&self.sort, &mut bucket.slots, 0);
                }
                continue;
            }
            let at = bucket.next;
            let metrics = self.load_metrics(doc, score)?;
            let bucket = self.buckets[ord].as_mut().expect("bucket");
            bucket.slots[at] = Some((key, metrics));
            if at == 0 {
                bucket.heap = true;
                for parent in (0..size / 2).rev() {
                    down_heap(&self.sort, &mut bucket.slots, parent);
                }
            } else {
                bucket.next = at - 1;
            }
        }
        Ok(())
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        let mut out = Vec::with_capacity(owning.len());
        for &o in owning {
            let entries = match self.buckets.get_mut(o as usize).and_then(Option::take) {
                None => Vec::new(),
                Some(b) => {
                    // getValues: the filled slots in array order, then a stable sort.
                    let start = if b.heap { 0 } else { b.next + 1 };
                    let mut v: Vec<(SortKey, Vec<Option<MetricVal>>)> = b.slots.into_iter().skip(start).flatten().collect();
                    v.sort_by(|x, y| self.sort.compare(&x.0, &y.0));
                    v
                }
            };
            out.push(self.result(entries));
        }
        Ok(out)
    }

    fn build_empty(&self) -> AggValue {
        let mut v = self.result(Vec::new());
        if let AggValue::TopMetrics(t) = &mut v {
            // buildEmptyAggregation: size 0, ascending.
            t.size = 0;
            t.reverse = false;
        }
        v
    }

    fn needs_scores(&self) -> bool {
        self.sort.needs_scores() || self.metrics.iter().any(|m| m.vs.script.as_ref().is_some_and(|s| s.needs_score()))
    }

    fn validate_sort(&self, name: &str, key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        crate::collect::multi_value_validate_sort(name, key, rest, |k| self.metrics.iter().any(|m| m.name == k))
    }

    fn sort_value(&self, key: Option<&str>, _rest: &[PathElement], ord: u32) -> f64 {
        let Some(k) = key else { return f64::NAN };
        let Some(i) = self.metrics.iter().position(|m| m.name == k) else { return f64::NAN };
        let best = self.buckets.get(ord as usize).and_then(Option::as_ref).and_then(|b| {
            b.slots.iter().flatten().min_by(|x, y| self.sort.compare(&x.0, &y.0))
        });
        match best.and_then(|(_, m)| m[i].clone()) {
            Some(MetricVal::Long(l)) => l as f64,
            Some(MetricVal::Double(d)) => d,
            _ => f64::NAN,
        }
    }
}
