//! `cardinality`: HyperLogLog++ exactly as Elasticsearch implements it, so counts match to the
//! unit.
//!
//! Values hash like Elasticsearch's `CardinalityAggregator`: byte strings (keywords, IPs) with
//! `MurmurHash3.hash128(…, seed 0).h1`, longs with `BitMixer.mix64`, doubles with
//! `mix64(doubleToLongBits)`. Each bucket starts in linear-counting mode — a set of 32-bit
//! encodings of the hashes at precision 25 — and switches to `2^p` registers once the set holds
//! more than `0.75 · 2^p / 4` entries. The estimate is the paper's: linear counting below the
//! precision's threshold, the bias-corrected raw estimate above. Both states depend only on
//! the set of values seen, so shard merges are exact unions.
//!
//! Ordinal fields hash each distinct term of a segment once (a per-segment hash cache), so a
//! low-cardinality keyword costs one array lookup per value.

use search_index::SegmentContext;

use crate::collect::{Aggregator, Batch, BindCtx, single_value_validate_sort};
use crate::error::{AggError, Result};
use crate::internal::{AggValue, Internal, MultiBuckets, Prop, ReduceCtx, RenderCtx, single_value_property};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::metrics::hll_tables::{BIAS_DATA, RAW_ESTIMATE_DATA};
use crate::request::{AggDef, PathElement};
use crate::util::{mix64, murmur3_x64_128};
use crate::values::{BoundVs, BytesReader, DvOrds, NumReader, VsOpts, VsSpec, VsType, bind};
use crate::xparser::{Field, Tokens, XParser};

const P2: u32 = 25;
const BIAS_K: usize = 6;
const THRESHOLDS: [u64; 15] = [10, 20, 40, 80, 220, 400, 900, 1800, 3100, 6500, 11500, 20000, 50000, 120000, 350000];
pub const DEFAULT_PRECISION: u32 = 14;

/// `HyperLogLogPlusPlus.precisionFromThreshold`.
pub fn precision_from_threshold(count: u64) -> u32 {
    let entries = (count as f64 / 0.75).ceil() as u64;
    let bytes = entries.saturating_mul(4);
    let bits = if bytes == 0 { 1 } else { 64 - bytes.leading_zeros() };
    bits.clamp(4, 18)
}

/// `linearCounting(m, v)`.
fn linear_counting(m: u64, v: u64) -> u64 {
    crate::format::java_round(m as f64 * (m as f64 / v as f64).ln()) as u64
}

/// `AbstractLinearCounting.encodeHash`.
#[inline]
fn encode_hash(hash: u64, p: u32) -> u32 {
    let e = hash >> (64 - P2);
    let mask = (1u64 << (P2 - p)) - 1;
    if e & mask == 0 {
        let run_len = 1 + (hash << P2).leading_zeros().min(64 - P2) as u64;
        ((e << 7) | (run_len << 1) | 1) as u32
    } else {
        (e << 1) as u32
    }
}

#[inline]
fn decode_run_len(encoded: u32, p: u32) -> u8 {
    if encoded & 1 == 1 {
        (((encoded >> 1) & 0x3f) + (P2 - p)) as u8
    } else {
        let bits = (encoded << (31 + p - P2)) as i32 as u32;
        (1 + bits.leading_zeros()) as u8
    }
}

#[inline]
fn decode_index(encoded: u32, p: u32) -> usize {
    let index = if encoded & 1 == 1 { (encoded >> 7) as u64 } else { (encoded >> 1) as u64 };
    (index >> (P2 - p)) as usize
}

/// One bucket's HyperLogLog++ state.
#[derive(Clone, Debug, PartialEq)]
pub struct Hll {
    p: u32,
    /// Linear counting: an open-addressing set of encoded hashes (0 = empty).
    lc: Vec<u32>,
    lc_size: usize,
    /// Registers (`2^p` run lengths) once upgraded.
    regs: Option<Vec<u8>>,
}

impl Hll {
    pub fn new(p: u32) -> Hll {
        Hll { p, lc: Vec::new(), lc_size: 0, regs: None }
    }

    pub fn precision(&self) -> u32 {
        self.p
    }

    fn lc_threshold(&self) -> usize {
        let capacity = (1usize << self.p) / 4;
        (capacity as f32 * 0.75) as usize
    }

    #[inline]
    pub fn collect(&mut self, hash: u64) {
        if let Some(r) = &mut self.regs {
            let index = (hash >> (64 - self.p)) as usize;
            let run_len = (1 + (hash << self.p).leading_zeros().min(64 - self.p)) as u8;
            if r[index] < run_len {
                r[index] = run_len;
            }
            return;
        }
        let e = encode_hash(hash, self.p);
        self.add_encoded(e);
    }

    fn add_encoded(&mut self, e: u32) {
        if let Some(r) = &mut self.regs {
            let i = decode_index(e, self.p);
            let rl = decode_run_len(e, self.p);
            if r[i] < rl {
                r[i] = rl;
            }
            return;
        }
        if self.lc.is_empty() {
            self.lc = vec![0; 32];
        } else if self.lc_size > self.lc.len() * 3 / 4 {
            let mut bigger = vec![0u32; self.lc.len() * 2];
            let mask = bigger.len() - 1;
            for &v in &self.lc {
                if v != 0 {
                    let mut pos = v as usize & mask;
                    while bigger[pos] != 0 {
                        pos = (pos + 1) & mask;
                    }
                    bigger[pos] = v;
                }
            }
            self.lc = bigger;
        }
        let mask = self.lc.len() - 1;
        let mut pos = e as usize & mask;
        while self.lc[pos] != 0 {
            if self.lc[pos] == e {
                return;
            }
            pos = (pos + 1) & mask;
        }
        self.lc[pos] = e;
        self.lc_size += 1;
        if self.lc_size > self.lc_threshold() {
            self.upgrade();
        }
    }

    fn upgrade(&mut self) {
        let mut regs = vec![0u8; 1 << self.p];
        for &v in &self.lc {
            if v != 0 {
                let i = decode_index(v, self.p);
                let rl = decode_run_len(v, self.p);
                if regs[i] < rl {
                    regs[i] = rl;
                }
            }
        }
        self.lc = Vec::new();
        self.lc_size = 0;
        self.regs = Some(regs);
    }

    /// Merges another state of the same precision.
    pub fn merge(&mut self, o: &Hll) {
        match &o.regs {
            None => {
                for &v in &o.lc {
                    if v != 0 {
                        self.add_encoded(v);
                    }
                }
            }
            Some(or) => {
                if self.regs.is_none() {
                    self.upgrade();
                }
                let r = self.regs.as_mut().expect("registers");
                for (a, b) in r.iter_mut().zip(or.iter()) {
                    if *a < *b {
                        *a = *b;
                    }
                }
            }
        }
    }

    /// The estimate.
    pub fn cardinality(&self) -> u64 {
        match &self.regs {
            None => {
                let m = 1u64 << P2;
                linear_counting(m, m - self.lc_size as u64)
            }
            Some(regs) => hll_cardinality(regs, self.p),
        }
    }

    pub fn ram_bytes(&self) -> usize {
        self.lc.capacity() * 4 + self.regs.as_ref().map_or(0, |r| r.len()) + 48
    }
}

fn hll_cardinality(regs: &[u8], p: u32) -> u64 {
    let m = 1u64 << p;
    let alpha = match p {
        4 => 0.673,
        5 => 0.697,
        _ => 0.7213 / (1.0 + 1.079 / m as f64),
    };
    let alpha_mm = alpha * m as f64 * m as f64;
    let mut inverse_sum = 0.0f64;
    let mut zeros = 0u64;
    for &r in regs {
        inverse_sum += 1.0 / (1u64 << r) as f64;
        if r == 0 {
            zeros += 1;
        }
    }
    let e1 = alpha_mm / inverse_sum;
    let e2 = if e1 <= 5.0 * m as f64 { e1 - estimate_bias(e1, p) } else { e1 };
    let h = if zeros != 0 { linear_counting(m, zeros) } else { crate::format::java_round(e2) as u64 };
    if h <= THRESHOLDS[(p - 4) as usize] { h } else { crate::format::java_round(e2) as u64 }
}

fn estimate_bias(e: f64, p: u32) -> f64 {
    let raw = RAW_ESTIMATE_DATA[(p - 4) as usize];
    let bias = BIAS_DATA[(p - 4) as usize];
    let mut weights = [0.0f64; BIAS_K];
    let mut index = bias.len() as i64 - BIAS_K as i64;
    for i in 0..raw.len() {
        let w = 1.0 / (raw[i] - e).abs();
        let j = i % BIAS_K;
        if w.is_infinite() {
            return bias[i];
        } else if weights[j] >= w {
            index = i as i64 - BIAS_K as i64;
            break;
        }
        weights[j] = w;
    }
    let mut weight_sum = 0.0;
    let mut bias_sum = 0.0;
    for i in 0..BIAS_K {
        let w = weights[i];
        let b = bias[(index + i as i64) as usize];
        bias_sum += w * b;
        weight_sum += w;
    }
    bias_sum / weight_sum
}

/// Hash of a byte string (`MurmurHash3.hash128(bytes, 0).h1`).
#[inline]
pub fn hash_bytes(b: &[u8]) -> u64 {
    murmur3_x64_128(b, 0).0
}

/// A parsed `cardinality`.
#[derive(Clone, Debug)]
pub struct CardinalitySpec {
    pub vs: VsSpec,
    pub precision_threshold: Option<i64>,
    pub execution_hint: Option<String>,
}

pub fn parse(p: &mut XParser<'_>, name: &str) -> Result<CardinalitySpec> {
    let opts = VsOpts::new(true, false, false);
    let mut fields = VsSpec::fields(opts);
    fields.push(Field::new("precision_threshold", Tokens::LONG));
    fields.push(Field::new("execution_hint", Tokens::STRING_OR_NULL));
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    let mut s = CardinalitySpec { vs: VsSpec::default(), precision_threshold: None, execution_hint: None };
    p.object("cardinality", &fields, |p, i| {
        let f = names[i];
        if s.vs.parse_field(p, f)? {
            return Ok(());
        }
        if f == "precision_threshold" {
            let v = p.long_value()?;
            if v < 0 {
                return Err(AggError::illegal_argument(format!(
                    "[precisionThreshold] must be greater than or equal to 0. Found [{v}] in [{name}]"
                )));
            }
            s.precision_threshold = Some(v);
        } else {
            s.execution_hint = p.text_or_null()?;
        }
        Ok(())
    })?;
    s.vs.check_required(opts)?;
    Ok(s)
}

pub fn create(def: &AggDef, spec: &CardinalitySpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    if let Some(h) = &spec.execution_hint
        && !matches!(h.to_uppercase().as_str(), "GLOBAL_ORDINALS" | "SEGMENT_ORDINALS" | "DIRECT" | "SAVE_MEMORY_HEURISTIC" | "SAVE_TIME_HEURISTIC")
    {
        return Err(AggError::illegal_argument(format!(
            "Invalid execution mode for cardinality aggregation.  Got [{h}]expected one of [global_ordinal, segment_ordinal, direct]"
        )));
    }
    let vs = bind(&spec.vs, ctx.lookup, VsType::Keyword, false)?;
    if matches!(vs.vs_type, VsType::CartesianPoint | VsType::GeoShape | VsType::CartesianShape | VsType::Histogram | VsType::AggregateMetric | VsType::ExponentialHistogram) && !vs.unmapped {
        return Err(vs.unsupported("cardinality"));
    }
    let precision = spec.precision_threshold.map(|t| precision_from_threshold(t as u64)).unwrap_or(DEFAULT_PRECISION);
    Ok(Box::new(CardinalityAgg {
        precision,
        reader: Reader::None,
        vs,
        hlls: Vec::new(),
        ord_hashes: Vec::new(),
        single_owner: ctx.cardinality.is_one(),
        seen: Vec::new(),
        obatch: Default::default(),
        nbatch: Default::default(),
        tracker: ctx.tracker(def),
        ram: 0,
    }))
}

enum Reader {
    None,
    Ords(DvOrds),
    Num { r: NumReader, double: bool },
    Bytes(BytesReader),
}

struct CardinalityAgg {
    precision: u32,
    reader: Reader,
    vs: BoundVs,
    hlls: Vec<Option<Box<Hll>>>,
    /// Per segment ordinal: its term's hash (0 = not computed yet).
    ord_hashes: Vec<u64>,
    /// Collects into owning ordinal 0 only: ordinals are gathered in `seen` (a bitset) and
    /// hashed once each when the segment ends (Elasticsearch's `OrdinalsCollector`).
    single_owner: bool,
    seen: Vec<u64>,
    obatch: crate::values::OrdBatch,
    nbatch: crate::values::NumBatch,
    tracker: MemTracker,
    ram: usize,
}

impl CardinalityAgg {
    #[inline]
    fn hll(&mut self, ord: u32) -> &mut Hll {
        let o = ord as usize;
        if self.hlls.len() <= o {
            self.hlls.resize_with(o + 1, || None);
        }
        let p = self.precision;
        self.hlls[o].get_or_insert_with(|| Box::new(Hll::new(p)))
    }

    fn account(&mut self) -> Result<()> {
        let ram: usize = self.hlls.iter().flatten().map(|h| h.ram_bytes()).sum::<usize>() + self.ord_hashes.capacity() * 8;
        if ram > self.ram + 65536 {
            self.ram = ram;
            self.tracker.ensure(ram)?;
        }
        Ok(())
    }
}

impl Aggregator for CardinalityAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.reader = if self.vs.unmapped && self.vs.missing.is_none() {
            Reader::None
        } else if self.vs.has_ordinals() && self.vs.missing.is_none() {
            let dv = DvOrds::open(seg, self.vs.field.as_ref().map(|f| f.number).unwrap_or(0));
            self.ord_hashes.clear();
            self.ord_hashes.resize(dv.value_count() as usize, 0);
            if self.single_owner {
                self.seen.clear();
                self.seen.resize((dv.value_count() as usize).div_ceil(64), 0);
            }
            Reader::Ords(dv)
        } else if self.vs.vs_type.is_numeric() {
            Reader::Num { r: self.vs.numeric_reader(seg)?, double: !self.vs.is_integral() }
        } else {
            Reader::Bytes(self.vs.bytes_reader(seg)?)
        };
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        let mut reader = std::mem::replace(&mut self.reader, Reader::None);
        let res = (|| {
            match &mut reader {
                Reader::None => {}
                Reader::Ords(dv) if self.single_owner => {
                    dv.load_batch(b.docs, &mut self.obatch);
                    for &o in &self.obatch.vals {
                        self.seen[(o >> 6) as usize] |= 1u64 << (o & 63);
                    }
                    return Ok(());
                }
                Reader::Ords(dv) => {
                    let mut term = Vec::new();
                    dv.load_batch(b.docs, &mut self.obatch);
                    let batch = std::mem::take(&mut self.obatch);
                    for (i, ords) in batch.docs() {
                        for &o in ords {
                            let mut h = self.ord_hashes[o as usize];
                            if h == 0 {
                                dv.lookup(o, &mut term);
                                h = hash_bytes(&term);
                                // 0 marks "not computed"; a real 0 hash is re-computed each time.
                                self.ord_hashes[o as usize] = h;
                            }
                            self.hll(b.ords[i]).collect(h);
                        }
                    }
                    self.obatch = batch;
                }
                Reader::Num { r, double } if r.raw().is_some() => {
                    let (dv, decode) = r.raw().expect("raw");
                    dv.load_batch(b.docs, &mut self.nbatch);
                    let batch = std::mem::take(&mut self.nbatch);
                    for (i, raw) in batch.docs() {
                        let hll = self.hll(b.ords[i]);
                        for &rv in raw {
                            let h = if *double {
                                let v = decode.to_f64(rv);
                                mix64(if v.is_nan() { 0x7ff8000000000000u64 } else { v.to_bits() })
                            } else {
                                mix64(decode.to_i64(rv) as u64)
                            };
                            hll.collect(h);
                        }
                    }
                    self.nbatch = batch;
                }
                Reader::Num { r, double } => {
                    for i in 0..b.len() {
                        if *double {
                            let vals = r.doubles(b.docs[i], b.score(i))?;
                            let n = vals.len();
                            let mut hs: smallvec::SmallVec<[u64; 4]> = smallvec::SmallVec::new();
                            for &v in &vals[..n] {
                                let bits = if v.is_nan() { 0x7ff8000000000000u64 } else { v.to_bits() };
                                hs.push(mix64(bits));
                            }
                            for h in hs {
                                self.hll(b.ords[i]).collect(h);
                            }
                        } else {
                            let vals = r.longs(b.docs[i], b.score(i))?;
                            let hs: smallvec::SmallVec<[u64; 4]> = vals.iter().map(|&v| mix64(v as u64)).collect();
                            for h in hs {
                                self.hll(b.ords[i]).collect(h);
                            }
                        }
                    }
                }
                Reader::Bytes(r) => {
                    for i in 0..b.len() {
                        let n = r.load(b.docs[i], b.score(i))?;
                        for j in 0..n {
                            let h = hash_bytes(r.value(j));
                            self.hll(b.ords[i]).collect(h);
                        }
                    }
                }
            }
            self.account()
        })();
        self.reader = reader;
        res
    }

    fn finish_segment(&mut self) -> Result<()> {
        if !self.single_owner {
            return Ok(());
        }
        let Reader::Ords(dv) = &self.reader else { return Ok(()) };
        let dv = dv.clone();
        let mut term = Vec::new();
        let seen = std::mem::take(&mut self.seen);
        for (w, &word) in seen.iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                let o = (w as u64) * 64 + bits.trailing_zeros() as u64;
                bits &= bits - 1;
                dv.lookup(o, &mut term);
                let h = hash_bytes(&term);
                self.hll(0).collect(h);
            }
        }
        self.seen = seen;
        self.account()
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        Ok(owning
            .iter()
            .map(|&o| {
                let hll = self.hlls.get(o as usize).and_then(|h| h.as_ref()).map(|h| (**h).clone()).unwrap_or_else(|| Hll::new(self.precision));
                AggValue::Cardinality(Box::new(Cardinality { hll: Some(hll) }))
            })
            .collect())
    }

    fn build_empty(&self) -> AggValue {
        AggValue::Cardinality(Box::new(Cardinality { hll: if self.vs.unmapped && self.vs.missing.is_none() { None } else { Some(Hll::new(self.precision)) } }))
    }

    fn needs_scores(&self) -> bool {
        self.vs.script.as_ref().is_some_and(|s| s.needs_score())
    }

    fn validate_sort(&self, name: &str, key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        single_value_validate_sort(name, key, rest)
    }

    fn sort_value(&self, _key: Option<&str>, _rest: &[PathElement], ord: u32) -> f64 {
        self.hlls.get(ord as usize).and_then(|h| h.as_ref()).map_or(0.0, |h| h.cardinality() as f64)
    }
}

/// `InternalCardinality`.
#[derive(Clone, Debug)]
pub struct Cardinality {
    /// `None` for an unmapped field.
    pub hll: Option<Hll>,
}

impl Cardinality {
    pub fn value(&self) -> u64 {
        self.hll.as_ref().map_or(0, |h| h.cardinality())
    }
}

impl Internal for Cardinality {
    fn type_name(&self) -> &'static str {
        "cardinality"
    }
    fn class_name(&self) -> &'static str {
        "InternalCardinality"
    }
    fn reduce(list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let mut reduced: Option<Hll> = None;
        for c in &list {
            if let Some(h) = &c.hll {
                match &mut reduced {
                    None => {
                        let mut n = Hll::new(h.precision());
                        n.merge(h);
                        reduced = Some(n);
                    }
                    Some(r) => {
                        if r.precision() != h.precision() {
                            return Err(AggError::illegal_argument(""));
                        }
                        r.merge(h);
                    }
                }
            }
        }
        Ok(Cardinality { hll: reduced })
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        out.push("value", Json::from(self.value()));
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        single_value_property(name, self.value() as f64, path)
    }
    fn ram_bytes(&self) -> usize {
        self.hll.as_ref().map_or(16, |h| h.ram_bytes())
    }
}

/// The HyperLogLog state a `cumulative_cardinality` path resolves to in a bucket.
pub fn bucket_hll<'a, M: MultiBuckets + ?Sized>(m: &'a M, agg_name: &str, i: usize, path: &[String]) -> Result<Option<&'a Hll>> {
    let prop = match crate::internal::bucket_property(agg_name, m.doc_count(i), || m.key(i), m.aggs(i), path) {
        Ok(p) => p,
        Err(e) if e.kind == "invalid_aggregation_path_exception" => return Ok(None),
        Err(e) => return Err(e),
    };
    match prop {
        Prop::Agg(a) => match &a.value {
            AggValue::Cardinality(c) => Ok(c.hll.as_ref()),
            other => Err(AggError::invalid_path(format!(
                "buckets_path must reference a cardinality aggregation, got: [{}] at aggregation [{}]",
                other.class_name(),
                path.first().cloned().unwrap_or_default()
            ))),
        },
        _ => Err(AggError::invalid_path(format!(
            "buckets_path must reference a cardinality aggregation, got: [Double] at aggregation [{}]",
            path.first().cloned().unwrap_or_default()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precision_thresholds() {
        assert_eq!(precision_from_threshold(3000), 14);
        assert_eq!(precision_from_threshold(0), 4);
        assert_eq!(precision_from_threshold(40000), 18);
        assert_eq!(precision_from_threshold(100), 10);
    }

    #[test]
    fn small_counts_are_exact() {
        let mut h = Hll::new(14);
        for i in 0..1000u64 {
            h.collect(mix64(i));
        }
        assert_eq!(h.cardinality(), 1000);
        let mut h2 = Hll::new(14);
        for i in 500..1500u64 {
            h2.collect(mix64(i));
        }
        h.merge(&h2);
        assert_eq!(h.cardinality(), 1500);
    }

    #[test]
    fn large_counts_are_close() {
        let mut h = Hll::new(14);
        for i in 0..200_000u64 {
            h.collect(mix64(i));
        }
        assert!(h.regs.is_some());
        let c = h.cardinality() as f64;
        assert!((c - 200_000.0).abs() / 200_000.0 < 0.03, "{c}");
    }
}
