//! `time_series` (aggregations module): one bucket per time series of a `time_series`-mode
//! index, keyed by the series' dimension values.
//!
//! A series is identified like Elasticsearch's `TimeSeriesAggregator` does it, by a "legacy"
//! `_tsid` built from the document's dimension values ([`FieldLookup::time_series`]): a VInt
//! count, then per dimension present, in name order, the name (VInt length + UTF-8) and the
//! value — `'l'` + big-endian long for numeric, date and boolean dimensions (raw doc values),
//! `'s'` + VInt length + bytes for keyword and ip dimensions (an ip's 16-byte encoding: the key
//! then decodes it as text, like Elasticsearch). Buckets come out sorted by those bytes, which
//! orders series by dimension count first.
//!
//! Elasticsearch visits series in `_tsid` order, so `size` keeps the series with the smallest
//! stored `_tsid`s ([`TimeSeriesInfo::tsid_field`]; first seen without one) before sorting.
//! Reduces merge the key-sorted lists, stopping at `size` (so a `size` of 0 still yields one
//! bucket, as in Elasticsearch). On an index that isn't in `time_series` mode nothing is
//! collected.

use std::sync::Arc;

use search_index::SegmentContext;

use crate::collect::{Aggregator, Batch, BindCtx, Subs, grow};
use crate::error::{AggError, Result};
use crate::field::{SourceType, TimeSeriesInfo};
use crate::internal::{AggValue, Aggs, Internal, MultiBuckets, Prop, ReduceCtx, RenderCtx, multi_bucket_property};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::request::{AggDef, PathElement};
use crate::util::BytesOrdMap;
use crate::values::{DocCounter, DvNum, DvOrds};
use crate::xparser::{Field, Tokens, XParser};

/// `MultiBucketConsumerService.DEFAULT_MAX_BUCKETS`, the default `size`.
pub const DEFAULT_SIZE: i32 = 65536;

/// A parsed `time_series`.
#[derive(Clone, Debug)]
pub struct TimeSeriesSpec {
    pub keyed: bool,
    pub size: i32,
}

/// `TimeSeriesAggregationBuilder.PARSER`.
pub fn parse(p: &mut XParser<'_>) -> Result<TimeSeriesSpec> {
    let fields = [Field::new("keyed", Tokens::BOOLEAN), Field::new("size", Tokens::INT)];
    let mut spec = TimeSeriesSpec { keyed: true, size: DEFAULT_SIZE };
    p.object("time_series", &fields, |p, i| {
        if i == 0 {
            spec.keyed = p.boolean_value()?;
        } else {
            spec.size = p.int_value()?;
        }
        Ok(())
    })?;
    Ok(spec)
}

/// One dimension's doc values in a segment.
enum DimReader {
    Num(DvNum),
    Bytes(DvOrds),
}

struct Dim {
    name: String,
    number: u32,
    numeric: bool,
    reader: DimReader,
}

/// A dimension present on a doc: its index, a numeric value or the byte range of its term in
/// `SeriesKeys::term`.
type Part = (usize, Option<i64>, (usize, usize));

/// Builds documents' legacy `_tsid`s from their dimension values.
pub(crate) struct SeriesKeys {
    dims: Vec<Dim>,
    key: Vec<u8>,
    term: Vec<u8>,
    scratch: Vec<u8>,
}

impl SeriesKeys {
    pub(crate) fn new(dimensions: &[crate::field::AggField]) -> SeriesKeys {
        let mut dims: Vec<Dim> = dimensions
            .iter()
            .map(|f| Dim {
                name: f.name.clone(),
                number: f.number,
                numeric: matches!(f.source, SourceType::Numeric(_) | SourceType::Date { .. } | SourceType::Boolean),
                reader: DimReader::Num(DvNum::None),
            })
            .collect();
        dims.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        SeriesKeys { dims, key: Vec::new(), term: Vec::new(), scratch: Vec::new() }
    }

    pub(crate) fn set_segment(&mut self, seg: &SegmentContext) {
        for d in &mut self.dims {
            d.reader = if d.numeric { DimReader::Num(DvNum::open(seg, d.number)) } else { DimReader::Bytes(DvOrds::open(seg, d.number)) };
        }
    }

    /// `TimeSeriesIdFieldMapper.buildLegacyTsid` of `doc`.
    pub(crate) fn build(&mut self, doc: u32) -> Result<&[u8]> {
        let mut parts: smallvec::SmallVec<[Part; 8]> = smallvec::SmallVec::new();
        self.term.clear();
        for (i, d) in self.dims.iter().enumerate() {
            match &d.reader {
                DimReader::Num(dv) => {
                    let mut first = None;
                    dv.for_each(doc, |v| {
                        if first.is_none() {
                            first = Some(v);
                        }
                    });
                    if let Some(v) = first {
                        parts.push((i, Some(v), (0, 0)));
                    }
                }
                DimReader::Bytes(dv) => {
                    let mut first = None;
                    dv.for_each(doc, |o| {
                        if first.is_none() {
                            first = Some(o);
                        }
                    });
                    if let Some(o) = first {
                        let start = self.term.len();
                        dv.lookup(o, &mut self.scratch);
                        self.term.extend_from_slice(&self.scratch);
                        parts.push((i, None, (start, self.term.len())));
                    }
                }
            }
        }
        if parts.is_empty() {
            return Err(AggError::illegal_argument("Dimension fields are missing."));
        }
        self.key.clear();
        write_vint(&mut self.key, parts.len() as u32);
        for (i, num, (s, e)) in parts {
            let name = self.dims[i].name.as_bytes();
            write_vint(&mut self.key, name.len() as u32);
            self.key.extend_from_slice(name);
            match num {
                Some(v) => {
                    self.key.push(b'l');
                    self.key.extend_from_slice(&v.to_be_bytes());
                }
                None => {
                    self.key.push(b's');
                    write_vint(&mut self.key, (e - s) as u32);
                    self.key.extend_from_slice(&self.term[s..e]);
                }
            }
        }
        Ok(&self.key)
    }
}

/// Binds `time_series`.
pub fn create(def: &AggDef, spec: &TimeSeriesSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let info = ctx.lookup.time_series();
    let keys = SeriesKeys::new(info.as_ref().map(|i: &TimeSeriesInfo| i.dimensions.as_slice()).unwrap_or(&[]));
    let subs = Subs::bind(def, ctx)?;
    Ok(Box::new(TimeSeriesAgg {
        cfg: Arc::new(TsConfig { keyed: spec.keyed, size: spec.size }),
        active: info.is_some(),
        tsid_field: info.as_ref().and_then(|i| i.tsid_field),
        tsid: DvOrds::None,
        keys,
        ords: BytesOrdMap::new(),
        counts: Vec::new(),
        min_tsid: Vec::new(),
        subs,
        doc_counter: DocCounter::none(),
        doc_count_field: ctx.doc_count_field,
        tracker: ctx.tracker(def),
    }))
}

#[derive(Debug)]
pub struct TsConfig {
    pub keyed: bool,
    pub size: i32,
}

struct TimeSeriesAgg {
    cfg: Arc<TsConfig>,
    /// The index is in `time_series` mode.
    active: bool,
    tsid_field: Option<u32>,
    tsid: DvOrds,
    keys: SeriesKeys,
    ords: BytesOrdMap,
    counts: Vec<u64>,
    /// Per bucket: the smallest stored `_tsid` seen (the order Elasticsearch visits series in).
    min_tsid: Vec<Option<Vec<u8>>>,
    subs: Subs,
    doc_counter: DocCounter,
    doc_count_field: Option<u32>,
    tracker: MemTracker,
}

fn write_vint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

impl Aggregator for TimeSeriesAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.doc_counter = DocCounter::open(seg, self.doc_count_field);
        self.keys.set_segment(seg);
        self.tsid = match self.tsid_field {
            Some(f) => DvOrds::open(seg, f),
            None => DvOrds::None,
        };
        self.subs.set_segment(seg)
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        let mut tsid = Vec::new();
        for i in 0..b.len() {
            let doc = b.docs[i];
            let key = self.keys.build(doc)?;
            let (ord, new) = self.ords.add(b.ords[i], key);
            if new {
                grow(&mut self.counts, ord, 0);
                grow(&mut self.min_tsid, ord, None);
                self.tracker.ensure(self.ords.ram_bytes() + self.counts.len() * 32)?;
            }
            self.counts[ord as usize] += self.doc_counter.count(doc);
            if self.tsid_field.is_some() {
                let mut first = None;
                self.tsid.for_each(doc, |o| {
                    if first.is_none() {
                        first = Some(o);
                    }
                });
                if let Some(o) = first {
                    self.tsid.lookup(o, &mut tsid);
                    let slot = &mut self.min_tsid[ord as usize];
                    if slot.as_ref().is_none_or(|m| tsid < *m) {
                        *slot = Some(tsid.clone());
                    }
                }
            }
            if !self.subs.is_empty() {
                self.subs.push(doc, ord, b.score(i))?;
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
        for ord in 0..self.ords.len() as u32 {
            if let Some(&i) = pos.get(&self.ords.owning(ord)) {
                per[i].push(ord);
            }
        }
        // `if (buckets.size() >= size) break` after adding: at least one bucket.
        let keep = self.cfg.size.max(1) as usize;
        for list in &mut per {
            // Visiting order: smallest `_tsid` first (first seen otherwise).
            list.sort_by(|&a, &b| self.min_tsid[a as usize].cmp(&self.min_tsid[b as usize]).then(a.cmp(&b)));
            list.truncate(keep);
            list.sort_by(|&a, &b| self.ords.get_bytes(a).cmp(self.ords.get_bytes(b)));
        }
        let all: Vec<u32> = per.iter().flatten().copied().collect();
        let mut aggs = self.subs.build(&all)?.into_iter();
        Ok(per
            .into_iter()
            .map(|list| {
                let buckets = list
                    .into_iter()
                    .map(|o| TsBucket { key: self.ords.get_bytes(o).to_vec(), doc_count: self.counts[o as usize], aggs: aggs.next().unwrap_or_default() })
                    .collect();
                AggValue::TimeSeries(Box::new(TimeSeries { cfg: self.cfg.clone(), keyed: self.cfg.keyed, buckets }))
            })
            .collect())
    }

    fn build_empty(&self) -> AggValue {
        // `buildEmptyAggregation` is never keyed.
        AggValue::TimeSeries(Box::new(TimeSeries { cfg: self.cfg.clone(), keyed: false, buckets: Vec::new() }))
    }

    fn needs_scores(&self) -> bool {
        self.subs.needs_scores()
    }

    fn validate_sort(&self, _name: &str, _key: Option<&str>, _rest: &[PathElement]) -> Result<()> {
        Err(AggError::illegal_argument(
            "Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end.",
        ))
    }
}

/// A series bucket.
#[derive(Clone, Debug)]
pub struct TsBucket {
    /// The legacy `_tsid` bytes.
    pub key: Vec<u8>,
    pub doc_count: u64,
    pub aggs: Aggs,
}

/// `InternalTimeSeries`.
#[derive(Clone, Debug)]
pub struct TimeSeries {
    pub cfg: Arc<TsConfig>,
    pub keyed: bool,
    pub buckets: Vec<TsBucket>,
}

fn read_vint(b: &[u8], pos: &mut usize) -> Option<usize> {
    let mut v: u32 = 0;
    for shift in (0..35).step_by(7) {
        let x = *b.get(*pos)?;
        *pos += 1;
        v |= ((x & 0x7f) as u32) << shift;
        if x & 0x80 == 0 {
            return Some(v as usize);
        }
    }
    None
}

/// `RoutingPathFields.decodeAsMap`: `(name, value)` in key order.
fn decode_key(b: &[u8]) -> Vec<(String, Json)> {
    let mut out = Vec::new();
    let mut pos = 0;
    let Some(n) = read_vint(b, &mut pos) else { return out };
    for _ in 0..n {
        let Some(len) = read_vint(b, &mut pos) else { break };
        let Some(name) = b.get(pos..pos + len) else { break };
        pos += len;
        let name = String::from_utf8_lossy(name).into_owned();
        let Some(&ty) = b.get(pos) else { break };
        pos += 1;
        let value = match ty {
            b's' => {
                let Some(len) = read_vint(b, &mut pos) else { break };
                let Some(v) = b.get(pos..pos + len) else { break };
                pos += len;
                // Ip dimensions hold binary; Elasticsearch decodes them leniently too.
                Json::Str(String::from_utf8_lossy(v).into_owned())
            }
            b'l' => {
                let Some(v) = b.get(pos..pos + 8) else { break };
                pos += 8;
                Json::Int(i64::from_be_bytes(v.try_into().unwrap_or_default()))
            }
            _ => break,
        };
        out.push((name, value));
    }
    out
}

/// `AbstractMap.toString()` of the decoded key: `{host=a, zone=3}`.
fn key_string(fields: &[(String, Json)]) -> String {
    let parts: Vec<String> = fields
        .iter()
        .map(|(k, v)| match v {
            Json::Str(s) => format!("{k}={s}"),
            other => format!("{k}={}", other.to_java_string()),
        })
        .collect();
    format!("{{{}}}", parts.join(", "))
}

impl Internal for TimeSeries {
    fn type_name(&self) -> &'static str {
        "time_series"
    }

    fn class_name(&self) -> &'static str {
        "InternalTimeSeries"
    }

    fn must_reduce_single(&self) -> bool {
        true
    }

    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let cfg = list[0].cfg.clone();
        let keyed = list[0].keyed;
        let size = cfg.size.max(1) as usize;
        let lists: Vec<Vec<TsBucket>> = list.into_iter().map(|t| t.buckets).collect();
        let groups = crate::util::lucene_merge_groups(lists, |a, b| a.key < b.key);
        let mut buckets = Vec::new();
        for group in groups {
            ctx.consume_buckets(1)?;
            let key = group[0].key.clone();
            let doc_count = group.iter().map(|b| b.doc_count).sum();
            let aggs = Aggs::reduce_lists(group.into_iter().map(|b| b.aggs).collect(), ctx)?;
            buckets.push(TsBucket { key, doc_count, aggs });
            if buckets.len() >= size {
                break;
            }
        }
        Ok(TimeSeries { cfg, keyed, buckets })
    }

    fn render(&self, out: &mut Json, rc: &RenderCtx) {
        let bucket = |b: &TsBucket, fields: &[(String, Json)]| {
            let mut o = Json::obj();
            let mut key = Json::obj();
            for (k, v) in fields {
                key.push(k, v.clone());
            }
            o.push("key", key);
            o.push("doc_count", Json::from(b.doc_count));
            b.aggs.render_into(&mut o, rc);
            o
        };
        if self.keyed {
            let mut obj = Json::obj();
            for b in &self.buckets {
                let fields = decode_key(&b.key);
                obj.push(key_string(&fields), bucket(b, &fields));
            }
            out.push("buckets", obj);
        } else {
            out.push("buckets", Json::Arr(self.buckets.iter().map(|b| bucket(b, &decode_key(&b.key))).collect()));
        }
    }

    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_bucket_property(self, name, path)
    }

    fn ram_bytes(&self) -> usize {
        64 + self.buckets.iter().map(|b| 48 + b.key.len() + b.aggs.ram_bytes()).sum::<usize>()
    }
}

impl MultiBuckets for TimeSeries {
    fn len(&self) -> usize {
        self.buckets.len()
    }
    fn doc_count(&self, i: usize) -> u64 {
        self.buckets[i].doc_count
    }
    fn key(&self, i: usize) -> Json {
        let mut key = Json::obj();
        for (k, v) in decode_key(&self.buckets[i].key) {
            key.push(&k, v);
        }
        key
    }
    fn key_string(&self, i: usize) -> String {
        key_string(&decode_key(&self.buckets[i].key))
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
        let mut slots: Vec<Option<TsBucket>> = self.buckets.drain(..).map(Some).collect();
        self.buckets = order.iter().map(|&i| slots[i].take().expect("bucket")).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_decode_and_print_like_java_maps() {
        let mut k = Vec::new();
        write_vint(&mut k, 2);
        for (name, v) in [("host", None), ("zone", Some(-1i64))] {
            write_vint(&mut k, name.len() as u32);
            k.extend_from_slice(name.as_bytes());
            match v {
                None => {
                    k.push(b's');
                    write_vint(&mut k, 1);
                    k.push(b'a');
                }
                Some(v) => {
                    k.push(b'l');
                    k.extend_from_slice(&v.to_be_bytes());
                }
            }
        }
        let fields = decode_key(&k);
        assert_eq!(fields, vec![("host".to_string(), Json::str("a")), ("zone".to_string(), Json::Int(-1))]);
        assert_eq!(key_string(&fields), "{host=a, zone=-1}");
    }
}
