//! The parsed aggregation tree (Elasticsearch's `AggregatorFactories.Builder`), bucket orders and
//! aggregation paths.
//!
//! An [`AggRequest`] is index-independent: it holds what [`crate::parse`] read, in request order.
//! Binding it to a shard ([`AggRequest::collector`](crate::AggRequest::collector)) resolves fields
//! and builds aggregators; reducing uses it only for the pipeline tree.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::error::{AggError, Result};
use crate::json::Json;
use crate::xparser::{Token, XParser};

/// A parsed `aggs` object.
#[derive(Clone, Debug, Default)]
pub struct AggRequest {
    pub(crate) aggs: Vec<Arc<AggDef>>,
    pub(crate) pipelines: Vec<Arc<PipelineDef>>,
    /// Deprecation warnings raised while parsing.
    pub warnings: Vec<String>,
}

/// One aggregation definition.
#[derive(Debug)]
pub struct AggDef {
    pub name: String,
    pub type_name: &'static str,
    pub meta: Option<Arc<Json>>,
    pub kind: crate::parse::AggKind,
    pub subs: Vec<Arc<AggDef>>,
    /// Pipeline aggregations declared in this aggregation's `aggs` block.
    pub pipelines: Vec<Arc<PipelineDef>>,
}

/// One pipeline aggregation definition.
#[derive(Debug)]
pub struct PipelineDef {
    pub name: String,
    pub type_name: &'static str,
    pub meta: Option<Arc<Json>>,
    pub kind: crate::pipeline::PipelineKind,
}

impl PipelineDef {
    /// The `buckets_path` values (one per variable).
    pub fn buckets_paths(&self) -> Vec<String> {
        self.kind.buckets_paths()
    }
}

impl AggRequest {
    pub fn is_empty(&self) -> bool {
        self.aggs.is_empty() && self.pipelines.is_empty()
    }

    /// Names of the top-level aggregations (pipelines included), in request order.
    pub fn names(&self) -> Vec<&str> {
        self.aggs.iter().map(|a| a.name.as_str()).chain(self.pipelines.iter().map(|p| p.name.as_str())).collect()
    }
}

/// One element of an aggregation path (`agg[key].metric`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathElement {
    pub full: String,
    pub name: String,
    pub key: Option<String>,
    pub metric: Option<String>,
}

/// `AggregationPath`: `agg1>agg2[key]>agg3.metric`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AggPath {
    pub elements: Vec<PathElement>,
    pub text: String,
}

/// `AggregationErrors.invalidPathElement`.
fn invalid_element(element: &str, path: &str) -> AggError {
    AggError::illegal_argument(format!("Invalid path element [{element}] in path [{path}]"))
}

impl AggPath {
    /// `AggregationPath.parse`.
    pub fn parse(path: &str) -> Result<AggPath> {
        // Strings.tokenizeToStringArray trims tokens and drops empty ones.
        let elements: Vec<&str> = path.split('>').map(str::trim).filter(|s| !s.is_empty()).collect();
        let mut out = Vec::with_capacity(elements.len());
        for (i, element) in elements.iter().enumerate() {
            let last = i + 1 == elements.len();
            let key_index = element.rfind('[');
            if last {
                let metric_index = element.rfind('.');
                if let Some(ki) = key_index {
                    if ki == 0 || ki + 3 > element.len() {
                        return Err(invalid_element(element, path));
                    }
                    let end = element.rfind(']');
                    let Some(end) = end.filter(|e| *e >= ki) else { return Err(invalid_element(element, path)) };
                    if metric_index.is_none() && end != element.len() - 1 {
                        return Err(invalid_element(element, path));
                    }
                    let metric = match metric_index {
                        Some(mi) if mi > end => Some(element[mi + 1..].to_string()),
                        _ => None,
                    };
                    out.push(PathElement {
                        full: element.to_string(),
                        name: element[..ki].to_string(),
                        key: Some(element[ki + 1..end].to_string()),
                        metric,
                    });
                    continue;
                }
                match metric_index {
                    None => out.push(PathElement { full: element.to_string(), name: element.to_string(), key: None, metric: None }),
                    Some(mi) => {
                        if mi == 0 || mi + 2 > element.len() {
                            return Err(invalid_element(element, path));
                        }
                        out.push(PathElement {
                            full: element.to_string(),
                            name: element[..mi].to_string(),
                            key: None,
                            metric: Some(element[mi + 1..].to_string()),
                        });
                    }
                }
            } else {
                match key_index {
                    Some(ki) => {
                        if ki == 0 || ki + 3 > element.len() {
                            return Err(invalid_element(element, path));
                        }
                        if !element.ends_with(']') {
                            return Err(invalid_element(element, path));
                        }
                        out.push(PathElement {
                            full: element.to_string(),
                            name: element[..ki].to_string(),
                            key: Some(element[ki + 1..element.len() - 1].to_string()),
                            metric: None,
                        });
                    }
                    None => out.push(PathElement { full: element.to_string(), name: element.to_string(), key: None, metric: None }),
                }
            }
        }
        Ok(AggPath { elements: out, text: path.to_string() })
    }

    /// `getPathElementsAsStringList`: names, `'key'`s and metrics flattened.
    pub fn as_string_list(&self) -> Vec<String> {
        let mut out = Vec::new();
        for e in &self.elements {
            out.push(e.name.clone());
            if let Some(k) = &e.key {
                out.push(k.clone());
            }
            if let Some(m) = &e.metric {
                out.push(m.clone());
            }
        }
        out
    }
}

impl std::fmt::Display for AggPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts: Vec<&str> = self.elements.iter().map(|e| e.full.as_str()).collect();
        f.write_str(&parts.join(">"))
    }
}

/// A bucket order (`BucketOrder`): simple orders, sub-aggregation paths, compounds.
#[derive(Clone, Debug, PartialEq)]
pub enum BucketOrder {
    Count { asc: bool },
    Key { asc: bool },
    Agg { path: AggPath, asc: bool },
    /// A compound order (the last element is a key order: the tie-breaker).
    Compound(Vec<BucketOrder>),
}

impl BucketOrder {
    pub const COUNT_DESC: BucketOrder = BucketOrder::Count { asc: false };
    pub const KEY_ASC: BucketOrder = BucketOrder::Key { asc: true };

    /// `BucketOrder.compound(orders)` with the `_key asc` tie-breaker unless the last is a key
    /// order.
    pub fn compound(orders: Vec<BucketOrder>) -> BucketOrder {
        let mut v = Vec::new();
        for o in orders {
            match o {
                BucketOrder::Compound(inner) => v.extend(inner),
                o => v.push(o),
            }
        }
        if !v.last().is_some_and(|o| matches!(o, BucketOrder::Key { .. })) {
            v.push(BucketOrder::KEY_ASC);
        }
        BucketOrder::Compound(v)
    }

    /// The single elements (a compound's list, or itself).
    pub fn elements(&self) -> &[BucketOrder] {
        match self {
            BucketOrder::Compound(v) => v,
            o => std::slice::from_ref(o),
        }
    }

    /// `InternalOrder.isKeyOrder`: a key order, or a compound starting with one.
    pub fn is_key_order(&self) -> bool {
        matches!(self.elements().first(), Some(BucketOrder::Key { .. }))
    }

    /// `isKeyAsc` of a key order.
    pub fn is_key_asc(&self) -> bool {
        matches!(self.elements().first(), Some(BucketOrder::Key { asc: true }))
    }

    /// `InternalOrder.isCountDesc`.
    pub fn is_count_desc(&self) -> bool {
        matches!(self.elements().first(), Some(BucketOrder::Count { asc: false }))
    }

    /// Whether any element orders by a sub-aggregation.
    pub fn has_agg_order(&self) -> bool {
        self.elements().iter().any(|o| matches!(o, BucketOrder::Agg { .. }))
    }

    /// The order as `toXContent` prints it.
    pub fn to_json(&self) -> Json {
        match self {
            BucketOrder::Count { asc } => Json::obj().with("_count", if *asc { "asc" } else { "desc" }),
            BucketOrder::Key { asc } => Json::obj().with("_key", if *asc { "asc" } else { "desc" }),
            BucketOrder::Agg { path, asc } => Json::obj().with(path.to_string(), if *asc { "asc" } else { "desc" }),
            BucketOrder::Compound(v) => Json::Arr(v.iter().map(|o| o.to_json()).collect()),
        }
    }

    /// `InternalOrder.Parser.parseOrderParam` (the parser is on the order object's start).
    pub fn parse_one(p: &mut XParser<'_>) -> Result<BucketOrder> {
        let mut key: Option<String> = None;
        let mut asc = false;
        loop {
            match p.next_token() {
                Some(Token::EndObject) | None => break,
                Some(Token::FieldName) => key = p.current_name().map(str::to_string),
                Some(Token::String) => {
                    let dir = p.text()?;
                    if dir.eq_ignore_ascii_case("asc") {
                        asc = true;
                    } else if dir.eq_ignore_ascii_case("desc") {
                        asc = false;
                    } else {
                        return Err(p.parsing_error(format!("Unknown order direction [{dir}]")));
                    }
                }
                Some(t) => return Err(p.parsing_error(format!("Unexpected token [{t}] for [order]"))),
            }
        }
        let Some(key) = key else { return Err(p.parsing_error("Must specify at least one field for [order]")) };
        Ok(match key.as_str() {
            "_key" => BucketOrder::Key { asc },
            "_count" => BucketOrder::Count { asc },
            other => BucketOrder::Agg { path: AggPath::parse(other)?, asc },
        })
    }

    /// `declareObjectArray(order, parseOrderParam)` then `BucketOrder.compound(list)` the way
    /// terms-like builders set it: a single order keeps a tie-breaker only via compound.
    pub fn parse_list(p: &mut XParser<'_>) -> Result<Vec<BucketOrder>> {
        if p.token() == Token::StartObject {
            return Ok(vec![BucketOrder::parse_one(p)?]);
        }
        let mut out = Vec::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndArray => break,
                Token::StartObject => out.push(BucketOrder::parse_one(p)?),
                other => return Err(AggError::illegal_state(format!("expected value but got [{other}]"))),
            }
        }
        Ok(out)
    }

    /// The direction multiplier of a simple order.
    pub fn is_asc(&self) -> bool {
        match self {
            BucketOrder::Count { asc } | BucketOrder::Key { asc } | BucketOrder::Agg { asc, .. } => *asc,
            BucketOrder::Compound(v) => v.first().is_some_and(|o| o.is_asc()),
        }
    }
}

/// Compares two doubles as `Double.compare` does (NaN largest, -0.0 < 0.0).
#[inline]
pub fn java_double_compare(a: f64, b: f64) -> Ordering {
    if a < b {
        Ordering::Less
    } else if a > b {
        Ordering::Greater
    } else {
        let (x, y) = (java_double_bits(a), java_double_bits(b));
        x.cmp(&y)
    }
}

/// `Double.doubleToLongBits` (NaN canonicalized).
#[inline]
fn java_double_bits(a: f64) -> i64 {
    if a.is_nan() { 0x7ff8000000000000 } else { a.to_bits() as i64 }
}

/// `gap_policy` of pipeline aggregations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GapPolicy {
    InsertZeros,
    #[default]
    Skip,
    KeepValues,
}

impl GapPolicy {
    pub fn parse(p: &XParser<'_>) -> Result<GapPolicy> {
        let t = p.text()?;
        match t.as_str() {
            "insert_zeros" => Ok(GapPolicy::InsertZeros),
            "skip" => Ok(GapPolicy::Skip),
            "keep_values" => Ok(GapPolicy::KeepValues),
            _ => Err(p.parsing_error(format!("Invalid gap policy: [{t}], accepted values: [insert_zeros, skip, keep_values]"))),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            GapPolicy::InsertZeros => "insert_zeros",
            GapPolicy::Skip => "skip",
            GapPolicy::KeepValues => "keep_values",
        }
    }

    /// `processValue(docCount, value)`.
    pub fn process(self, doc_count: u64, value: f64) -> f64 {
        match self {
            GapPolicy::InsertZeros => {
                if value.is_infinite() || value.is_nan() || doc_count == 0 {
                    0.0
                } else {
                    value
                }
            }
            GapPolicy::Skip => {
                if value.is_infinite() || doc_count == 0 {
                    f64::NAN
                } else {
                    value
                }
            }
            GapPolicy::KeepValues => {
                if value.is_infinite() || value.is_nan() {
                    f64::NAN
                } else {
                    value
                }
            }
        }
    }

    pub fn is_skippable(self) -> bool {
        !matches!(self, GapPolicy::InsertZeros)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        let p = AggPath::parse("a>b[k]>c.max").unwrap();
        assert_eq!(p.as_string_list(), vec!["a", "b", "k", "c", "max"]);
        let p = AggPath::parse("a['x'].avg").unwrap();
        assert_eq!(p.elements[0].key.as_deref(), Some("'x'"));
        assert_eq!(p.elements[0].metric.as_deref(), Some("avg"));
        assert!(AggPath::parse("a.").is_err());
        assert_eq!(AggPath::parse(".a").unwrap_err().reason, "Invalid path element [.a] in path [.a]");
        let p = AggPath::parse("the_percentiles[99.9]").unwrap();
        assert_eq!(p.elements[0].key.as_deref(), Some("99.9"));
    }

    #[test]
    fn compound_tiebreak() {
        let o = BucketOrder::compound(vec![BucketOrder::COUNT_DESC]);
        assert_eq!(o.elements().len(), 2);
        assert!(o.is_count_desc());
        let k = BucketOrder::compound(vec![BucketOrder::Key { asc: false }]);
        assert_eq!(k.elements().len(), 1);
        assert_eq!(java_double_compare(f64::NAN, 1.0), Ordering::Greater);
        assert_eq!(java_double_compare(-0.0, 0.0), Ordering::Less);
    }
}
