//! `string_stats`: counts, lengths, Shannon entropy and the character distribution of string
//! values.
//!
//! Lengths and characters are UTF-16 code units, as in Java. The entropy is a compensated sum
//! over the characters sorted by descending count — a stable sort of a Java `HashMap`'s
//! iteration order, which also orders the `distribution` object — so the character maps follow
//! Java's bucket layout ([`JavaHashMapModel`]) to come out in the same order with the same bits.

use search_index::SegmentContext;

use crate::collect::{Aggregator, Batch, BindCtx};
use crate::error::{AggError, Result};
use crate::format::DocValueFormat;
use crate::internal::{AggValue, CompensatedSum, Internal, Prop, ReduceCtx, RenderCtx, java_list};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::request::{AggDef, PathElement};
use crate::util::JavaHashMapModel;
use crate::values::{BoundVs, BytesReader, VsOpts, VsSpec, VsType, bind};
use crate::xparser::{Field, Tokens, XParser};

/// A parsed `string_stats`.
#[derive(Clone, Debug)]
pub struct StringStatsSpec {
    pub vs: VsSpec,
    pub show_distribution: bool,
}

pub fn parse(p: &mut XParser<'_>) -> Result<StringStatsSpec> {
    let opts = VsOpts::new(true, true, false);
    let mut fields = VsSpec::fields(opts);
    fields.push(Field::new("show_distribution", Tokens::BOOLEAN));
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    let mut spec = StringStatsSpec { vs: VsSpec::default(), show_distribution: false };
    p.object("string_stats", &fields, |p, i| {
        if !spec.vs.parse_field(p, names[i])? {
            spec.show_distribution = p.boolean_value()?;
        }
        Ok(())
    })?;
    spec.vs.check_required(opts)?;
    Ok(spec)
}

/// `Maps.capacity(expectedSize)`.
pub(crate) fn map_capacity(n: usize) -> usize {
    if n < 2 { n + 1 } else { (n as f64 / 0.75 + 1.0) as usize }
}

/// `InternalStringStats`.
#[derive(Clone, Debug)]
pub struct StringStats {
    pub count: u64,
    pub total_length: u64,
    pub min_length: i32,
    pub max_length: i32,
    /// Character counts in the Java map's iteration order.
    pub occurrences: Vec<(u16, u64)>,
    pub show_distribution: bool,
    pub format: DocValueFormat,
}

impl StringStats {
    fn avg_length(&self) -> f64 {
        self.total_length as f64 / self.count as f64
    }

    /// `getDistribution()`: by descending count (stable), probability = count / total length.
    fn distribution(&self) -> Vec<(u16, f64)> {
        let mut v: Vec<(u16, u64)> = self.occurrences.clone();
        v.sort_by_key(|e| std::cmp::Reverse(e.1));
        v.into_iter().map(|(c, n)| (c, n as f64 / self.total_length as f64)).collect()
    }

    fn entropy(&self) -> f64 {
        let mut k = CompensatedSum::default();
        for (_, p) in self.distribution() {
            if p > 0.0 {
                k.add(p * (p.ln() / 2f64.ln()));
            }
        }
        -k.value
    }

    fn value(&self, name: &str) -> Result<Prop<'static>> {
        Ok(match name {
            "count" => Prop::Num(self.count as f64),
            "min_length" => Prop::Num(self.min_length as f64),
            "max_length" => Prop::Num(self.max_length as f64),
            "avg_length" => Prop::Num(self.avg_length()),
            "entropy" => Prop::Num(self.entropy()),
            _ => return Err(AggError::illegal_argument(format!("Unknown value [{name}] in string stats aggregation"))),
        })
    }
}

fn char_key(c: u16) -> String {
    String::from_utf16_lossy(&[c])
}

impl Internal for StringStats {
    fn type_name(&self) -> &'static str {
        "string_stats"
    }
    fn class_name(&self) -> &'static str {
        "InternalStringStats"
    }
    fn reduce(list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let mut out = StringStats { count: 0, total_length: 0, min_length: i32::MAX, max_length: i32::MIN, occurrences: Vec::new(), ..list[0].clone() };
        // `new HashMap<>()` merged shard by shard.
        let mut model = JavaHashMapModel::new(None);
        let mut index: hashbrown::HashMap<u16, usize> = hashbrown::HashMap::new();
        let mut sums: Vec<(u16, u64)> = Vec::new();
        for s in &list {
            out.count += s.count;
            out.min_length = out.min_length.min(s.min_length);
            out.max_length = out.max_length.max(s.max_length);
            out.total_length += s.total_length;
            for &(c, n) in &s.occurrences {
                match index.get(&c) {
                    Some(&i) => sums[i].1 += n,
                    None => {
                        index.insert(c, sums.len());
                        model.put_new(sums.len(), JavaHashMapModel::spread(c as i32));
                        sums.push((c, n));
                    }
                }
            }
        }
        out.occurrences = model.order().into_iter().map(|i| sums[i]).collect();
        Ok(out)
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        out.push("count", Json::from(self.count));
        if self.count > 0 {
            out.push("min_length", Json::Int(self.min_length as i64));
            out.push("max_length", Json::Int(self.max_length as i64));
            out.push("avg_length", Json::Float(self.avg_length()));
            out.push("entropy", Json::Float(self.entropy()));
            let dist = self.distribution();
            if self.show_distribution {
                let mut d = Json::obj();
                for (c, p) in &dist {
                    d.push(char_key(*c), Json::Float(*p));
                }
                out.push("distribution", d);
            }
            if !self.format.is_raw() {
                out.push("min_length_as_string", self.format.format_long(self.min_length as i64));
                out.push("max_as_string", self.format.format_long(self.max_length as i64));
                out.push("avg_length_as_string", self.format.format_double(self.avg_length()));
                out.push("entropy_string", self.format.format_double(self.entropy()));
                if self.show_distribution {
                    let mut d = Json::obj();
                    for (c, p) in &dist {
                        d.push(char_key(*c), Json::Str(self.format.double_string(*p)));
                    }
                    out.push("distribution_string", d);
                }
            }
        } else {
            out.push("min_length", Json::Null);
            out.push("max_length", Json::Null);
            out.push("avg_length", Json::Null);
            out.push("entropy", Json::Float(0.0));
            if self.show_distribution {
                out.push("distribution", Json::Null);
            }
        }
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        if path.len() == 1 {
            return self.value(&path[0]);
        }
        Err(AggError::illegal_argument(format!("path not supported for [{name}]: {}", java_list(path))))
    }
}

/// Binds `string_stats`.
pub fn create(def: &AggDef, spec: &StringStatsSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let vs = bind(&spec.vs, ctx.lookup, VsType::Keyword, false)?;
    if vs.is_counter() {
        return Err(vs.unsupported("string_stats"));
    }
    if !vs.unmapped && vs.vs_type != VsType::Keyword {
        return Err(vs.unsupported("string_stats"));
    }
    Ok(Box::new(StringStatsAgg {
        show_distribution: spec.show_distribution,
        format: vs.format.clone(),
        vs,
        reader: None,
        buckets: Vec::new(),
        chars: Vec::new(),
        char_index: hashbrown::HashMap::new(),
        char_model: JavaHashMapModel::new(None),
        tracker: ctx.tracker(def),
    }))
}

#[derive(Clone, Default)]
struct Bucket {
    count: u64,
    total: u64,
    min: i32,
    max: i32,
    /// Per distinct character (aggregator-wide index): occurrences.
    occ: Vec<u64>,
}

struct StringStatsAgg {
    show_distribution: bool,
    format: DocValueFormat,
    vs: BoundVs,
    reader: Option<BytesReader>,
    buckets: Vec<Option<Bucket>>,
    /// Distinct characters in first-seen order (the aggregator's `HashMap<Character, …>`).
    chars: Vec<u16>,
    char_index: hashbrown::HashMap<u16, usize>,
    char_model: JavaHashMapModel,
    tracker: MemTracker,
}

impl StringStatsAgg {
    fn result(&self, b: Option<&Bucket>) -> AggValue {
        let Some(b) = b else {
            return AggValue::StringStats(Box::new(StringStats {
                count: 0,
                total_length: 0,
                min_length: i32::MAX,
                max_length: i32::MIN,
                occurrences: Vec::new(),
                show_distribution: self.show_distribution,
                format: self.format.clone(),
            }));
        };
        // Maps.newMapWithExpectedSize(chars), filled in the aggregator map's order.
        let mut model = JavaHashMapModel::new(Some(map_capacity(self.chars.len())));
        let mut entries: Vec<(u16, u64)> = Vec::new();
        for i in self.char_model.order() {
            let n = b.occ.get(i).copied().unwrap_or(0);
            if n > 0 {
                model.put_new(entries.len(), JavaHashMapModel::spread(self.chars[i] as i32));
                entries.push((self.chars[i], n));
            }
        }
        AggValue::StringStats(Box::new(StringStats {
            count: b.count,
            total_length: b.total,
            min_length: b.min,
            max_length: b.max,
            occurrences: model.order().into_iter().map(|i| entries[i]).collect(),
            show_distribution: self.show_distribution,
            format: self.format.clone(),
        }))
    }
}

impl Aggregator for StringStatsAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.reader = if self.vs.unmapped && self.vs.missing.is_none() { None } else { Some(self.vs.bytes_reader(seg)?) };
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        let Some(mut reader) = self.reader.take() else { return Ok(()) };
        let r = (|| {
            for i in 0..b.len() {
                let n = reader.load(b.docs[i], b.score(i))?;
                if n == 0 {
                    continue;
                }
                let ord = b.ords[i] as usize;
                if self.buckets.len() <= ord {
                    self.buckets.resize_with(ord + 1, || None);
                    self.tracker.ensure(self.buckets.len() * 64 + self.chars.len() * self.buckets.len() * 8)?;
                }
                let bucket = self.buckets[ord].get_or_insert_with(|| Bucket { min: i32::MAX, max: i32::MIN, ..Default::default() });
                bucket.count += n as u64;
                for j in 0..n {
                    let raw = reader.value(j);
                    if raw.is_empty() {
                        continue;
                    }
                    let text = match self.format.format_bytes(raw) {
                        Json::Str(s) => s,
                        other => other.to_java_string(),
                    };
                    let units: Vec<u16> = text.encode_utf16().collect();
                    let len = units.len() as i32;
                    bucket.total += len as u64;
                    bucket.min = bucket.min.min(len);
                    bucket.max = bucket.max.max(len);
                    for c in units {
                        let idx = match self.char_index.get(&c) {
                            Some(&x) => x,
                            None => {
                                let x = self.chars.len();
                                self.chars.push(c);
                                self.char_index.insert(c, x);
                                self.char_model.put_new(x, JavaHashMapModel::spread(c as i32));
                                x
                            }
                        };
                        if bucket.occ.len() <= idx {
                            bucket.occ.resize(idx + 1, 0);
                        }
                        bucket.occ[idx] += 1;
                    }
                }
            }
            Ok(())
        })();
        self.reader = Some(reader);
        r
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        Ok(owning.iter().map(|&o| self.result(self.buckets.get(o as usize).and_then(Option::as_ref))).collect())
    }

    fn build_empty(&self) -> AggValue {
        self.result(None)
    }

    fn needs_scores(&self) -> bool {
        self.vs.script.as_ref().is_some_and(|s| s.needs_score())
    }

    fn validate_sort(&self, _name: &str, _key: Option<&str>, _rest: &[PathElement]) -> Result<()> {
        Err(AggError::illegal_argument(
            "Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end.",
        ))
    }
}
