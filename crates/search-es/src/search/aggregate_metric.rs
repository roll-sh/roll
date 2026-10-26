//! `aggregate_metric_double` at search time (`AggregateMetricDoubleFieldType`).
//!
//! A value is stored as one doc-values-only numeric sub-field per declared metric (`<field>.min`,
//! `.max`, `.sum` as sortable doubles, `.value_count` as an integer; see `engine::convert`).
//! Queries and sorts on the field itself delegate the way Elasticsearch 9.5.5 does: a field with
//! a single metric acts as that metric's numeric field; a field with `sum` and `value_count`
//! acts as their average (computed per document from both doc values, like
//! `AggregateMetricAverageScript`); any other field matches nothing and can't be sorted.
//! `exists` looks at the first declared metric. Aggregations read the sub-fields directly
//! (`search_aggs::histogram_field::AggregateMetricReader`).

use super::query::{QueryCtx, arc, match_none};
use crate::engine::fields::{FieldInfo, Kind};
use crate::error::{EsError, EsResult};
use crate::json::Value;
use search_aggs::values::DvNum;
use search_index::collector::top_field::{LeafSortScript, ScriptSort, SortKey, SortMode, SortScript};
use search_index::query::{ConstantScoreQuery, FilterScript, LeafFilterScript, ScriptQuery};
use search_index::{QueryRef, SegmentContext};
use std::sync::Arc;

/// How the field answers queries and sorts.
pub enum Delegate {
    /// Its only metric's numeric field.
    Single(Box<FieldInfo>),
    /// `sum / value_count` per document: the engine fields of both.
    Avg { sum: u32, count: u32 },
    /// Nothing to delegate to.
    None,
}

/// The declared metrics of `f`, in `Metric` order.
pub fn metrics(f: &FieldInfo) -> Vec<&'static str> {
    crate::document::values::aggregate_metrics(&f.mapper)
}

/// The numeric field one metric of `f` stands for: a doc-values-only `double` (`integer` for
/// `value_count`).
pub fn metric_field(f: &FieldInfo, metric: &str) -> Option<FieldInfo> {
    let number = f.subfield(metric)?;
    let mut m = f.clone();
    m.number = number;
    let (kind, ty) = if metric == "value_count" { (Kind::Integer, "integer") } else { (Kind::Double, "double") };
    m.kind = kind;
    m.ty = ty.to_string();
    m.index = false;
    m.doc_values = true;
    m.store = false;
    m.subfields = Vec::new();
    Some(m)
}

pub fn delegate(f: &FieldInfo) -> Delegate {
    let declared = metrics(f);
    if let [single] = declared.as_slice() {
        return metric_field(f, single).map_or(Delegate::None, |m| Delegate::Single(Box::new(m)));
    }
    match (declared.contains(&"sum") && declared.contains(&"value_count"), f.subfield("sum"), f.subfield("value_count")) {
        (true, Some(sum), Some(count)) => Delegate::Avg { sum, count },
        _ => Delegate::None,
    }
}

/// `exists`: docs with a value for the first declared metric.
pub fn exists_field(f: &FieldInfo) -> Option<u32> {
    metrics(f).first().and_then(|m| f.subfield(m))
}

fn no_docs() -> QueryRef {
    match_none("aggregate_metric_double without a single metric or [sum, value_count]")
}

/// `termQuery` / `termsQuery` (`values` of a `term` query hold one value).
pub fn terms(ctx: &QueryCtx, f: &FieldInfo, values: &[Value], single_term: bool) -> EsResult<QueryRef> {
    if single_term && values.iter().any(Value::is_null) {
        return Err(EsError::illegal_argument("Cannot search for null."));
    }
    match delegate(f) {
        Delegate::Single(m) if single_term => ctx.term_query(&m, &values[0], false),
        Delegate::Single(m) => super::range::numeric_terms(ctx, &m, values),
        Delegate::Avg { sum, count } => {
            let wanted = values.iter().map(super::range::parse_double_value).collect::<EsResult<Vec<f64>>>()?;
            Ok(avg_query(f, sum, count, AvgPredicate::In(wanted)))
        }
        Delegate::None => Ok(no_docs()),
    }
}

/// `rangeQuery` over the average: bounds parse as doubles.
pub fn avg_range(f: &FieldInfo, sum: u32, count: u32, from: Option<&Value>, to: Option<&Value>, include_lower: bool, include_upper: bool) -> EsResult<QueryRef> {
    let lo = from.map(super::range::parse_double_value).transpose()?;
    let hi = to.map(super::range::parse_double_value).transpose()?;
    Ok(avg_query(f, sum, count, AvgPredicate::Range { lo, hi, include_lower, include_upper }))
}

fn avg_query(f: &FieldInfo, sum: u32, count: u32, pred: AvgPredicate) -> QueryRef {
    arc(ConstantScoreQuery::new(arc(ScriptQuery { script: Arc::new(AvgFilter { name: f.name.clone(), sum, count, pred: Arc::new(pred) }) })))
}

/// What a document's average must satisfy.
enum AvgPredicate {
    In(Vec<f64>),
    Range { lo: Option<f64>, hi: Option<f64>, include_lower: bool, include_upper: bool },
}

impl AvgPredicate {
    fn matches(&self, v: f64) -> bool {
        match self {
            AvgPredicate::In(list) => list.contains(&v),
            AvgPredicate::Range { lo, hi, include_lower, include_upper } => {
                let above = lo.is_none_or(|l| if *include_lower { v >= l } else { v > l });
                let below = hi.is_none_or(|h| if *include_upper { v <= h } else { v < h });
                above && below
            }
        }
    }
}

/// The per-document average of the `sum` and `value_count` doc values (`NaN` for a zero count).
struct Avg {
    sum: DvNum,
    count: DvNum,
}

impl Avg {
    fn open(seg: &SegmentContext, sum: u32, count: u32) -> Avg {
        Avg { sum: DvNum::open(seg, sum), count: DvNum::open(seg, count) }
    }

    fn get(&self, doc: u32) -> Option<f64> {
        let mut s = None;
        self.sum.for_each(doc, |v| s = s.or(Some(search_codec::points::encode::sortable_f64(v))));
        let mut c = None;
        self.count.for_each(doc, |v| c = c.or(Some(v)));
        match (s, c) {
            (Some(s), Some(c)) => Some(if c == 0 { f64::NAN } else { s / c as f64 }),
            _ => None,
        }
    }
}

struct AvgFilter {
    name: String,
    sum: u32,
    count: u32,
    pred: Arc<AvgPredicate>,
}

struct AvgFilterLeaf {
    avg: Avg,
    pred: Arc<AvgPredicate>,
}

impl LeafFilterScript for AvgFilterLeaf {
    fn matches(&mut self, doc: u32) -> search_index::Result<bool> {
        Ok(self.avg.get(doc).is_some_and(|v| self.pred.matches(v)))
    }
}

impl FilterScript for AvgFilter {
    fn leaf(&self, seg: &SegmentContext) -> search_index::Result<Box<dyn LeafFilterScript>> {
        Ok(Box::new(AvgFilterLeaf { avg: Avg::open(seg, self.sum, self.count), pred: self.pred.clone() }))
    }

    fn describe(&self) -> String {
        format!("AggregateMetricAverageScript[{}]", self.name)
    }
}

/// The sort key of a sort on the field, or the error Elasticsearch gives.
pub fn sort_key(f: &FieldInfo, reverse: bool, missing: Option<&Value>) -> EsResult<Result<SortKey, Box<FieldInfo>>> {
    match delegate(f) {
        // `SortedNumericSortField` on the metric, which sets no missing value: docs without one
        // sort as 0.
        Delegate::Single(m) => Ok(Err(m)),
        Delegate::Avg { sum, count } => {
            let missing = match missing {
                None => f64::INFINITY,
                Some(Value::Str(s)) if s == "_last" => f64::INFINITY,
                Some(Value::Str(s)) if s == "_first" => f64::NEG_INFINITY,
                Some(v) => super::range::parse_double_value(v)?,
            };
            // `_last` / `_first` follow the order; a number is used as is.
            let missing = if missing.is_infinite() && reverse { -missing } else { missing };
            Ok(Ok(SortKey::Script(ScriptSort { script: Arc::new(AvgSort { sum, count, missing }), reverse, mode: SortMode::Min, nested: None })))
        }
        Delegate::None => Err(EsError::illegal_argument(format!(
            "Sorting by field [{}] of type [aggregate_metric_double] is not supported unless it has configured a single metric or the [sum, value_count] metrics",
            f.name
        ))),
    }
}

struct AvgSort {
    sum: u32,
    count: u32,
    missing: f64,
}

struct AvgSortLeaf {
    avg: Avg,
    missing: f64,
}

impl SortScript for AvgSort {
    fn leaf(&self, seg: &SegmentContext) -> search_index::Result<Box<dyn LeafSortScript>> {
        Ok(Box::new(AvgSortLeaf { avg: Avg::open(seg, self.sum, self.count), missing: self.missing }))
    }
}

impl LeafSortScript for AvgSortLeaf {
    fn number(&mut self, doc: u32) -> search_index::Result<f64> {
        Ok(self.avg.get(doc).unwrap_or(self.missing))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn average_predicates() {
        let r = AvgPredicate::Range { lo: Some(2.5), hi: Some(5.0), include_lower: true, include_upper: false };
        assert!(r.matches(2.5) && r.matches(4.9) && !r.matches(5.0) && !r.matches(f64::NAN));
        let open = AvgPredicate::Range { lo: None, hi: Some(1.0), include_lower: false, include_upper: true };
        assert!(open.matches(-1e300) && open.matches(1.0) && !open.matches(1.1));
        let set = AvgPredicate::In(vec![2.5, 7.5]);
        assert!(set.matches(7.5) && !set.matches(7.0));
    }
}
