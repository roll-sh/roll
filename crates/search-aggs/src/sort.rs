//! Document sorting for `top_hits` and `top_metrics`: Lucene's `SortField` semantics over doc
//! values.
//!
//! A sort is a list of [`HitSort`] keys. Each document gets a [`SortKey`] per segment: numeric
//! fields pick a value by `mode` (`min` ascending and `max` descending by default) and replace
//! missing values the way Elasticsearch does (`_last` ascending is the type's maximum, so the
//! printed sort value of such a hit is that maximum); keyword and ip fields compare term bytes
//! and keep missing values `null`, first or last whatever the order. Ties fall back to the
//! global doc id (shards) or to shard and position order (reduce). A key on a field inside
//! `nested` objects reads the values of the hit's nested children (the docs of the path
//! between the previous parent and the hit, matching the sort's `filter`), as Lucene's
//! block-join sort fields do.

use std::cmp::Ordering;

use search_index::SegmentContext;
use smallvec::SmallVec;

use crate::error::{AggError, Result};
use crate::field::{HitSort, SortMissing, SortMode, SourceType};
use crate::format::{ip_to_string, parse_ip};
use crate::json::Json;
use crate::values::{Decode, DvNum, DvOrds};
use search_index::collector::top_field::NestedSort;
use search_codec::NO_MORE_DOCS;
use search_index::util::bitset::FixedBitSet;

/// One value of a sort key.
#[derive(Clone, Debug, PartialEq)]
pub enum SortVal {
    Score(f32),
    Doc(u32),
    Long(i64),
    Double(f64),
    Float(f32),
    Bytes(Vec<u8>),
    /// A missing string value.
    Null,
}

/// The values of every sort field of a document.
pub type SortKey = SmallVec<[SortVal; 2]>;

/// How a numeric sort field reads.
#[derive(Clone, Copy, Debug, PartialEq)]
enum NumKind {
    Long,
    Double,
    Float,
}

/// How a numeric key's stored values convert before comparing (`numeric_type`).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Conv {
    /// As stored: longs stay raw (`unsigned_long` sign-flipped and `date_nanos` in nanoseconds,
    /// as Lucene's `SortedNumericSortField` compares them); others decode.
    Native,
    /// Floating values cast to long (`FieldData.castToLong`).
    Truncate,
    /// `DateUtils.toMilliSeconds` (`date_nanos` sorted as `date`).
    NanosToMillis,
    /// `DateUtils.toNanoSeconds` (`date` sorted as `date_nanos`).
    MillisToNanos,
}

impl Conv {
    #[inline]
    fn long(self, decode: Decode, v: i64) -> i64 {
        match (self, decode) {
            (Conv::NanosToMillis, _) => v / 1_000_000,
            (Conv::MillisToNanos, _) => v.saturating_mul(1_000_000),
            (Conv::Native, Decode::Long | Decode::UnsignedLong | Decode::Nanos) => v,
            (Conv::Native | Conv::Truncate, d) => d.to_f64(v) as i64,
        }
    }

    /// `getDoubleValues`: `date_nanos` casts its raw nanoseconds.
    #[inline]
    fn double(decode: Decode, v: i64) -> f64 {
        if decode == Decode::Nanos { v as f64 } else { decode.to_f64(v) }
    }
}

/// A resolved sort field.
#[derive(Clone, Debug)]
enum KeySpec {
    Score,
    Doc,
    Num { field: u32, decode: Decode, kind: NumKind, conv: Conv, mode: SortMode, missing: f64, missing_long: i64 },
    Bytes { field: u32, ip: bool, mode: SortMode, missing_first: bool, missing_value: Option<Vec<u8>> },
    Computed(std::sync::Arc<dyn crate::field::ComputedSort>),
}

/// A sort bound to an index: what each key reads and how it orders.
#[derive(Clone, Debug)]
pub struct DocSort {
    keys: Vec<(KeySpec, bool)>,
    /// Per key: the nested level its values come from.
    nested: Vec<Option<NestedSort>>,
}

impl DocSort {
    /// Resolves keys (`missing` values parsed, defaults applied).
    pub fn new(sorts: &[HitSort]) -> Result<DocSort> {
        let mut keys = Vec::with_capacity(sorts.len());
        let nested = sorts.iter().map(|s| if let HitSort::Field { nested, .. } = s { nested.clone() } else { None }).collect();
        for s in sorts {
            keys.push(match s {
                HitSort::Score { reverse } => (KeySpec::Score, *reverse),
                HitSort::Doc { reverse } => (KeySpec::Doc, *reverse),
                HitSort::Computed { sort, reverse } => (KeySpec::Computed(sort.clone()), *reverse),
                HitSort::Field { field, reverse, missing, mode, numeric_type, .. } => {
                    let reverse = *reverse;
                    let mode = mode.unwrap_or(if reverse { SortMode::Max } else { SortMode::Min });
                    match field.source {
                        SourceType::Keyword | SourceType::Ip => {
                            let ip = field.source == SourceType::Ip;
                            if matches!(mode, SortMode::Sum | SortMode::Avg | SortMode::Median) {
                                return Err(AggError::illegal_argument(format!(
                                    "Sort mode [{}] is not supported for field [{}]",
                                    format!("{mode:?}").to_uppercase(),
                                    field.name
                                )));
                            }
                            let (missing_first, missing_value) = match missing {
                                SortMissing::Last => (false, None),
                                SortMissing::First => (true, None),
                                SortMissing::Value(v) => {
                                    let text = match v {
                                        Json::Str(s) => s.clone(),
                                        other => other.to_java_string(),
                                    };
                                    let bytes = if ip { parse_ip(&text)?.to_vec() } else { text.into_bytes() };
                                    (false, Some(bytes))
                                }
                            };
                            (KeySpec::Bytes { field: field.number, ip, mode, missing_first, missing_value }, reverse)
                        }
                        SourceType::None | SourceType::GeoPoint | SourceType::CartesianPoint | SourceType::Range(_) => {
                            return Err(AggError::illegal_argument(format!(
                                "can't sort on field [{}] of type [{}]",
                                field.name, field.type_name
                            )));
                        }
                        src => {
                            use crate::field::{NumericType, SortNumericType};
                            let decode = Decode::of(src);
                            let floating = matches!(src, SourceType::Numeric(t) if !t.is_integral());
                            let (kind, conv) = match numeric_type {
                                // `unsigned_long` sorts its raw sign-flipped longs.
                                None => match src {
                                    SourceType::Numeric(NumericType::Float | NumericType::HalfFloat) => (NumKind::Float, Conv::Native),
                                    SourceType::Numeric(t) if !t.is_integral() => (NumKind::Double, Conv::Native),
                                    _ => (NumKind::Long, Conv::Native),
                                },
                                Some(SortNumericType::Double) => (NumKind::Double, Conv::Native),
                                Some(SortNumericType::Date) if decode == Decode::Nanos => (NumKind::Long, Conv::NanosToMillis),
                                Some(SortNumericType::DateNanos) if matches!(src, SourceType::Date { nanos: false }) => (NumKind::Long, Conv::MillisToNanos),
                                Some(_) => (NumKind::Long, if floating { Conv::Truncate } else { Conv::Native }),
                            };
                            // Missing values: the type's extreme on the side `_last`/`_first` asks for.
                            let high = match missing {
                                SortMissing::Last => !reverse,
                                SortMissing::First => reverse,
                                SortMissing::Value(_) => false,
                            };
                            let (missing_d, missing_l) = match missing {
                                SortMissing::Value(v) => {
                                    let d = match v {
                                        Json::Str(s) => crate::xparser::java_parse_double(s)?,
                                        other => other.as_f64().ok_or_else(|| AggError::illegal_argument(format!("invalid missing value [{other}]")))?,
                                    };
                                    (d, d as i64)
                                }
                                _ => match kind {
                                    // `integer`, `short` and `byte` sort as Lucene `INT`s (a
                                    // `numeric_type` sorts through a long comparator).
                                    NumKind::Long if numeric_type.is_none() && matches!(src, SourceType::Numeric(NumericType::Integer | NumericType::Short | NumericType::Byte)) => {
                                        let l = if high { i64::from(i32::MAX) } else { i64::from(i32::MIN) };
                                        (l as f64, l)
                                    }
                                    NumKind::Long => {
                                        let l = if high { i64::MAX } else { i64::MIN };
                                        (l as f64, l)
                                    }
                                    NumKind::Double => (if high { f64::INFINITY } else { f64::NEG_INFINITY }, 0),
                                    NumKind::Float => (if high { f64::INFINITY } else { f64::NEG_INFINITY }, 0),
                                },
                            };
                            (KeySpec::Num { field: field.number, decode, kind, conv, mode, missing: missing_d, missing_long: missing_l }, reverse)
                        }
                    }
                }
            });
        }
        Ok(DocSort { keys, nested })
    }

    /// Whether this is the default score order (`[_score]` descending): `top_hits` then sorts
    /// like an unsorted search (no `sort` values on hits).
    pub fn is_score_only(&self) -> bool {
        self.keys.len() == 1 && matches!(self.keys[0], (KeySpec::Score, false))
    }

    pub fn needs_scores(&self) -> bool {
        self.keys.iter().any(|(k, _)| match k {
            KeySpec::Score => true,
            KeySpec::Computed(c) => c.needs_score(),
            _ => false,
        })
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Opens the per-segment readers (keys with a nested level read no values; see
    /// [`segment_in`](Self::segment_in)).
    pub fn segment(&self, seg: &SegmentContext) -> SegmentSort {
        let nested = self.keys.iter().map(|_| None).collect();
        self.open(seg, nested)
    }

    /// Opens the per-segment readers, with the parent and child bit sets of nested keys.
    pub fn segment_in(&self, searcher: &search_index::Searcher, seg: &SegmentContext) -> Result<SegmentSort> {
        let mut nested = Vec::with_capacity(self.keys.len());
        for n in &self.nested {
            nested.push(match n {
                None => None,
                Some(n) => {
                    let parents = search_index::query::nested::segment_bits(searcher, seg, &n.parent_filter).map_err(AggError::from)?;
                    let children = search_index::query::nested::segment_bits(searcher, seg, &n.child_filter).map_err(AggError::from)?;
                    Some(NestedBits { parents, children, max_children: n.max_children })
                }
            });
        }
        Ok(self.open(seg, nested))
    }

    fn open(&self, seg: &SegmentContext, nested: Vec<Option<NestedBits>>) -> SegmentSort {
        let readers = self
            .keys
            .iter()
            .map(|(k, _)| match k {
                KeySpec::Score => Reader::Score,
                KeySpec::Doc => Reader::Doc,
                KeySpec::Num { field, .. } => Reader::Num(DvNum::open(seg, *field)),
                KeySpec::Bytes { field, .. } => Reader::Ords(DvOrds::open(seg, *field)),
                KeySpec::Computed(c) => match c.segment(seg) {
                    Ok(s) => Reader::Computed(s),
                    Err(e) => {
                        search_index::search::defer_error(search_index::Error::Es { kind: e.kind.clone(), reason: e.reason.clone(), status: e.status });
                        Reader::Doc
                    }
                },
            })
            .collect();
        SegmentSort { readers, nested, base: seg.base, nums: Vec::new(), bytes: Vec::new(), children: Vec::new() }
    }

    /// Orders two keys (`Less` sorts first).
    pub fn compare(&self, a: &SortKey, b: &SortKey) -> Ordering {
        for (i, (spec, reverse)) in self.keys.iter().enumerate() {
            let (x, y) = (&a[i], &b[i]);
            let ord = match (x, y) {
                (SortVal::Null, SortVal::Null) => Ordering::Equal,
                (SortVal::Null, _) | (_, SortVal::Null) => {
                    let first = matches!(spec, KeySpec::Bytes { missing_first: true, .. });
                    let x_null = matches!(x, SortVal::Null);
                    // Missing strings stay first/last regardless of the order.
                    let ord = if x_null == first { Ordering::Less } else { Ordering::Greater };
                    if ord != Ordering::Equal {
                        return ord;
                    }
                    continue;
                }
                _ => {
                    let natural = cmp_val(x, y);
                    if *reverse { natural.reverse() } else { natural }
                }
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    }

    /// The sort values as hits print them after a reduce (raw values).
    pub fn render(&self, key: &SortKey) -> Vec<Json> {
        key.iter()
            .zip(&self.keys)
            .map(|(v, (spec, _))| match v {
                SortVal::Score(s) => Json::Float32(*s),
                SortVal::Doc(d) => Json::Int(*d as i64),
                SortVal::Long(l) => Json::Int(*l),
                SortVal::Double(d) => Json::Float(*d),
                SortVal::Float(f) => Json::Float32(*f),
                SortVal::Bytes(b) => match spec {
                    KeySpec::Bytes { ip: true, .. } => Json::Str(ip_to_string(b)),
                    KeySpec::Computed(c) => c.render_bytes(b),
                    _ => Json::Str(String::from_utf8_lossy(b).into_owned()),
                },
                SortVal::Null => Json::Null,
            })
            .collect()
    }
}

/// Natural order of two values of the same key (`Double.compare` for doubles; scores sort
/// descending).
fn cmp_val(x: &SortVal, y: &SortVal) -> Ordering {
    match (x, y) {
        (SortVal::Score(a), SortVal::Score(b)) => b.total_cmp(a),
        (SortVal::Doc(a), SortVal::Doc(b)) => a.cmp(b),
        (SortVal::Long(a), SortVal::Long(b)) => a.cmp(b),
        (SortVal::Double(a), SortVal::Double(b)) => java_double_cmp(*a, *b),
        (SortVal::Float(a), SortVal::Float(b)) => java_double_cmp(*a as f64, *b as f64),
        (SortVal::Bytes(a), SortVal::Bytes(b)) => a.cmp(b),
        _ => Ordering::Equal,
    }
}

/// `Double.compare` (NaN greatest, -0.0 < 0.0).
fn java_double_cmp(a: f64, b: f64) -> Ordering {
    let canon = |v: f64| if v.is_nan() { f64::NAN } else { v };
    canon(a).total_cmp(&canon(b))
}

enum Reader {
    Score,
    Doc,
    Num(DvNum),
    Ords(DvOrds),
    Computed(Box<dyn crate::field::SegmentComputedSort>),
}

/// A nested key's bit sets in one segment.
struct NestedBits {
    parents: std::sync::Arc<FixedBitSet>,
    children: std::sync::Arc<FixedBitSet>,
    max_children: Option<usize>,
}

/// Sort readers of one segment.
pub struct SegmentSort {
    readers: Vec<Reader>,
    nested: Vec<Option<NestedBits>>,
    base: u32,
    nums: Vec<i64>,
    bytes: Vec<u8>,
    children: Vec<u32>,
}

/// The nested children of parent `parent` (they precede it, after the previous parent) that
/// match the child filter, at most `max_children`.
fn children_of(n: &NestedBits, parent: u32, out: &mut Vec<u32>) {
    out.clear();
    if parent == 0 {
        return;
    }
    let start = n.parents.prev_set_bit(parent - 1).map_or(0, |p| p + 1);
    let limit = n.max_children.unwrap_or(usize::MAX);
    let mut c = n.children.next_set_bit(start);
    while c != NO_MORE_DOCS && c < parent && out.len() < limit {
        out.push(c);
        c = if c + 1 >= n.children.len() { NO_MORE_DOCS } else { n.children.next_set_bit(c + 1) };
    }
}

impl SegmentSort {
    /// The key of segment doc `doc`.
    pub fn key(&mut self, sort: &DocSort, doc: u32, score: f32) -> SortKey {
        let mut key = SortKey::new();
        for ((r, (spec, _)), nested) in self.readers.iter_mut().zip(&sort.keys).zip(&self.nested) {
            // A nested key reads the values of every matching child (`MultiValueMode.select`
            // over the children's values).
            match nested {
                Some(n) => children_of(n, doc, &mut self.children),
                None => {
                    self.children.clear();
                    self.children.push(doc);
                }
            }
            key.push(match (r, spec) {
                (Reader::Computed(c), _) => c.value(doc, score),
                (Reader::Score, _) => SortVal::Score(score),
                (Reader::Doc, _) => SortVal::Doc(self.base + doc),
                (Reader::Num(dv), KeySpec::Num { decode, kind, conv, mode, missing, missing_long, .. }) => {
                    self.nums.clear();
                    let nums = &mut self.nums;
                    for &c in &self.children {
                        dv.for_each(c, |v| nums.push(v));
                    }
                    if self.children.len() > 1 {
                        // Values of several docs: `num_value` expects them in ascending order.
                        let (d, c) = (*decode, *conv);
                        if *kind == NumKind::Long {
                            nums.sort_unstable_by_key(|&v| c.long(d, v));
                        } else {
                            nums.sort_unstable_by(|&a, &b| java_double_cmp(Conv::double(d, a), Conv::double(d, b)));
                        }
                    }
                    num_value(&self.nums, *decode, *kind, *conv, *mode, *missing, *missing_long)
                }
                (Reader::Ords(ords), KeySpec::Bytes { mode, missing_value, .. }) => {
                    let mut pick: Option<u64> = None;
                    for &c in &self.children {
                        ords.for_each(c, |o| {
                            pick = Some(match (pick, mode) {
                                (None, _) => o,
                                (Some(p), SortMode::Max) => p.max(o),
                                (Some(p), _) => p.min(o),
                            })
                        });
                    }
                    match pick {
                        Some(o) => {
                            ords.lookup(o, &mut self.bytes);
                            SortVal::Bytes(self.bytes.clone())
                        }
                        None => match missing_value {
                            Some(v) => SortVal::Bytes(v.clone()),
                            None => SortVal::Null,
                        },
                    }
                }
                _ => SortVal::Null,
            });
        }
        key
    }
}

/// A numeric field's sort value by `mode`, or the missing replacement.
fn num_value(raw: &[i64], decode: Decode, kind: NumKind, conv: Conv, mode: SortMode, missing: f64, missing_long: i64) -> SortVal {
    if raw.is_empty() {
        return match kind {
            NumKind::Long => SortVal::Long(missing_long),
            NumKind::Double => SortVal::Double(missing),
            NumKind::Float => SortVal::Float(missing as f32),
        };
    }
    match kind {
        NumKind::Long => {
            let vals = raw.iter().map(|&v| conv.long(decode, v));
            SortVal::Long(match mode {
                SortMode::Min => vals.min().unwrap_or(0),
                SortMode::Max => vals.max().unwrap_or(0),
                SortMode::Sum => vals.fold(0i64, |a, v| a.wrapping_add(v)),
                SortMode::Avg => {
                    let n = raw.len() as i64;
                    let sum = vals.fold(0i64, |a, v| a.wrapping_add(v));
                    // MultiValueMode.AVG on longs: Math.round(sum / count) as a double.
                    ((sum as f64) / n as f64).round() as i64
                }
                SortMode::Median => {
                    let v: Vec<i64> = vals.collect();
                    let n = v.len();
                    if n.is_multiple_of(2) { ((v[n / 2 - 1] + v[n / 2]) as f64 / 2.0).round() as i64 } else { v[n / 2] }
                }
            })
        }
        _ => {
            let vals: SmallVec<[f64; 4]> = raw.iter().map(|&v| Conv::double(decode, v)).collect();
            let d = match mode {
                SortMode::Min => vals[0],
                SortMode::Max => vals[vals.len() - 1],
                SortMode::Sum => vals.iter().sum(),
                SortMode::Avg => vals.iter().sum::<f64>() / vals.len() as f64,
                SortMode::Median => {
                    let n = vals.len();
                    if n.is_multiple_of(2) { (vals[n / 2 - 1] + vals[n / 2]) / 2.0 } else { vals[n / 2] }
                }
            };
            if kind == NumKind::Float { SortVal::Float(d as f32) } else { SortVal::Double(d) }
        }
    }
}
