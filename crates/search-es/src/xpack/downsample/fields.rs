//! What downsampling does with each field of the source mapping (`TimeSeriesFields`), and the
//! target index's mapping (`TransportDownsampleAction.createDownsampleIndexMapping`).
//!
//! Fields fall in three groups, read from doc values in the time series' order:
//!
//! * **dimensions** (`time_series_dimension`, and the keys a `flattened` field lists in
//!   `time_series_dimensions`): copied from the series' newest document;
//! * **metrics** (`time_series_metric`): gauges become `min`/`max`/`sum`/`value_count`
//!   (`aggregate`) or keep their last value (`last_value`), counters keep the bucket's oldest
//!   value (cumulative, with reset points) or their sum (delta temporality);
//! * **labels**: every other aggregatable field, which keeps its last value.
//!
//! A multi-field can stand in for its parent: a parent that isn't aggregatable (a `text` field)
//! is read through an aggregatable sub-field, and a dimension sub-field makes its parent a
//! dimension. A metric declared on a multi-field is an error.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};

/// How a field's values are read for a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Read {
    /// `docvalue_fields` of this field (formatted as the field's doc value format).
    DocValues(String),
    /// The `_source` value at this path (pre-aggregated and structured values).
    Source(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Metric {
    Gauge,
    Counter,
    /// A pre-aggregated gauge (`aggregate_metric_double`): the sub-metrics it keeps.
    Aggregate(Vec<String>),
    /// `time_series_metric: histogram` on an `exponential_histogram`.
    ExponentialHistogram,
    /// `time_series_metric: histogram` on a `histogram` (`values`) or `tdigest` (`centroids`),
    /// merged as a t-digest: the name of its values array.
    TDigest(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Role {
    Dimension,
    Metric(Metric),
    /// Last value: doc values, or a structured value from `_source` (`flattened`, histograms,
    /// `aggregate_metric_double` without a metric type).
    Label,
}

/// A field of the downsampled documents.
#[derive(Clone, Debug)]
pub(super) struct Field {
    /// The name it is written under (the full path, as one dotted key).
    pub name: String,
    pub role: Role,
    pub read: Read,
}

/// The source mapping's fields, grouped.
#[derive(Debug, Default)]
pub(super) struct Fields {
    pub fields: Vec<Field>,
}

impl Fields {
    pub fn dimensions(&self) -> impl Iterator<Item = &Field> {
        self.fields.iter().filter(|f| f.role == Role::Dimension)
    }

    pub fn has_dimensions(&self) -> bool {
        self.dimensions().next().is_some()
    }

    /// Every distinct `docvalue_fields` entry the scan requests.
    pub fn doc_value_fields(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for f in &self.fields {
            if let Read::DocValues(n) = &f.read
                && !out.contains(n)
            {
                out.push(n.clone());
            }
        }
        out
    }

    /// Every `_source` path the scan requests.
    pub fn source_paths(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for f in &self.fields {
            if let Read::Source(n) = &f.read
                && !out.contains(n)
            {
                out.push(n.clone());
            }
        }
        out
    }
}

fn str_param<'a>(def: &'a Map, k: &str) -> Option<&'a str> {
    def.get(k).and_then(Value::as_str)
}

fn bool_param(def: &Map, k: &str) -> Option<bool> {
    match def.get(k) {
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::Str(s)) => s.parse().ok(),
        _ => None,
    }
}

fn is_dimension(def: &Map) -> bool {
    bool_param(def, "time_series_dimension") == Some(true) && str_param(def, "type") != Some("passthrough")
}

fn metric_type(def: &Map) -> Option<&str> {
    str_param(def, "time_series_metric")
}

/// Whether a field has doc values a label can be read from (`MappedFieldType.isAggregatable`).
fn aggregatable(def: &Map) -> bool {
    let dv = |d| bool_param(def, "doc_values").unwrap_or(d);
    match str_param(def, "type").unwrap_or("object") {
        "text" | "annotated_text" => bool_param(def, "fielddata").unwrap_or(false),
        "match_only_text" | "search_as_you_type" | "completion" | "rank_feature" | "rank_features" | "sparse_vector" | "pattern_text"
        | "dense_vector" | "percolator" | "semantic_text" | "object" | "nested" | "alias" | "passthrough" => false,
        "binary" => dv(false),
        "histogram" | "exponential_histogram" | "tdigest" | "constant_keyword" | "aggregate_metric_double" => true,
        _ => dv(true),
    }
}

/// Labels read whole values from `_source` rather than formatted doc values.
fn structured(ty: &str) -> bool {
    matches!(ty, "flattened" | "histogram" | "exponential_histogram" | "tdigest" | "aggregate_metric_double")
}

/// Collects the fields of a mapping's `properties` (`TimeSeriesFields.Collector`).
pub(super) fn collect(props: &Map, timestamp: &str) -> EsResult<Fields> {
    let mut c = Collector { timestamp, out: Fields::default() };
    c.walk(props, "")?;
    Ok(c.out)
}

struct Collector<'a> {
    timestamp: &'a str,
    out: Fields,
}

impl Collector<'_> {
    fn walk(&mut self, props: &Map, prefix: &str) -> EsResult<()> {
        for (name, def) in props {
            let Value::Obj(def) = def else { continue };
            let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
            if let Some(Value::Obj(sub)) = def.get("properties") {
                self.walk(sub, &path)?;
                continue;
            }
            self.track(&path, def);
            if let Some(Value::Obj(multi)) = def.get("fields") {
                for (sub, sub_def) in multi {
                    let Value::Obj(sub_def) = sub_def else { continue };
                    self.track_multi_field(&path, &format!("{path}.{sub}"), sub_def)?;
                }
            }
        }
        Ok(())
    }

    fn position(&self, name: &str) -> Option<usize> {
        self.out.fields.iter().position(|f| f.name == name)
    }

    fn track(&mut self, path: &str, def: &Map) {
        let ty = str_param(def, "type").unwrap_or("object");
        if ty == "flattened"
            && let Some(Value::Arr(keys)) = def.get("time_series_dimensions")
        {
            for k in keys.iter().filter_map(Value::as_str) {
                let name = format!("{path}.{k}");
                self.out.fields.push(Field { name: name.clone(), role: Role::Dimension, read: Read::DocValues(name) });
            }
            return;
        }
        let role = if is_dimension(def) {
            Role::Dimension
        } else if let Some(m) = metric_type(def) {
            Role::Metric(match (m, ty) {
                (_, "aggregate_metric_double") => Metric::Aggregate(sub_metrics(def)),
                (_, "exponential_histogram") => Metric::ExponentialHistogram,
                (_, "histogram") => Metric::TDigest("values"),
                (_, "tdigest") => Metric::TDigest("centroids"),
                ("counter", _) => Metric::Counter,
                _ => Metric::Gauge,
            })
        } else if path != self.timestamp && aggregatable(def) && !matches!(ty, "object" | "nested" | "alias") {
            Role::Label
        } else {
            return;
        };
        let read = match &role {
            Role::Metric(Metric::Aggregate(_) | Metric::ExponentialHistogram | Metric::TDigest(_)) => Read::Source(path.to_string()),
            Role::Label if structured(ty) => Read::Source(path.to_string()),
            _ => Read::DocValues(path.to_string()),
        };
        self.out.fields.push(Field { name: path.to_string(), role, read });
    }

    fn track_multi_field(&mut self, parent: &str, path: &str, def: &Map) -> EsResult<()> {
        if metric_type(def).is_some() {
            return Err(EsError::illegal_argument(format!(
                "Downsampling failed because index mapping contains time series metrics as a multi field [{path}]"
            )));
        }
        let existing = self.position(parent);
        if existing.is_some_and(|i| matches!(self.out.fields[i].role, Role::Dimension | Role::Metric(_))) {
            return Ok(());
        }
        if is_dimension(def) {
            if let Some(i) = existing {
                self.out.fields.remove(i);
            }
            self.out.fields.push(Field { name: parent.to_string(), role: Role::Dimension, read: Read::DocValues(path.to_string()) });
        } else if existing.is_none() && aggregatable(def) {
            self.out.fields.push(Field { name: parent.to_string(), role: Role::Label, read: Read::DocValues(path.to_string()) });
        }
        Ok(())
    }
}

/// The sub-metrics an `aggregate_metric_double` keeps, in `min, max, sum, value_count` order.
fn sub_metrics(def: &Map) -> Vec<String> {
    let listed: Vec<&str> = def.get("metrics").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    GAUGE_METRICS.iter().filter(|m| listed.contains(m)).map(|m| m.to_string()).collect()
}

pub(super) const GAUGE_METRICS: [&str; 4] = ["min", "max", "sum", "value_count"];

/// The target mapping: the source's top-level parameters (runtime fields, dynamic templates,
/// `subobjects`…), its properties with `@timestamp` recording the interval, and gauges turned
/// into `aggregate_metric_double` by the `aggregate` method.
pub(super) fn target_mapping(source: &Map, timestamp: &str, aggregate: bool, interval: &str) -> Map {
    let mut out = Map::new();
    for (k, v) in source {
        if k == "properties" {
            if let Value::Obj(p) = v {
                out.insert(k.clone(), Value::Obj(target_properties(p, "", timestamp, aggregate, interval)));
            }
        } else {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

fn target_properties(props: &Map, prefix: &str, timestamp: &str, aggregate: bool, interval: &str) -> Map {
    let mut out = Map::new();
    for (name, def) in props {
        let Value::Obj(def) = def else { continue };
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        let mut m = Map::new();
        if path == timestamp {
            m.insert("type".into(), def.get("type").cloned().unwrap_or_else(|| "date".into()));
            for k in ["format", "ignore_malformed"] {
                if let Some(v) = def.get(k) {
                    m.insert(k.into(), v.clone());
                }
            }
            m.insert("meta".into(), crate::obj! { "fixed_interval" => interval, "time_zone" => "UTC" });
        } else if aggregate && metric_type(def) == Some("gauge") && str_param(def, "type") != Some("aggregate_metric_double") {
            m.insert("type".into(), "aggregate_metric_double".into());
            m.insert("metrics".into(), Value::from(GAUGE_METRICS.to_vec()));
            m.insert("time_series_metric".into(), "gauge".into());
        } else {
            for (k, v) in def {
                if k == "properties" {
                    if let Value::Obj(sub) = v {
                        m.insert(k.clone(), Value::Obj(target_properties(sub, &path, timestamp, aggregate, interval)));
                    }
                } else {
                    m.insert(k.clone(), v.clone());
                }
            }
        }
        out.insert(name.clone(), Value::Obj(m));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Map {
        crate::json::parse(s.as_bytes()).unwrap().unwrap().as_object().unwrap().clone()
    }

    #[test]
    fn multi_fields_stand_in_for_their_parents() {
        let props = parse(
            r#"{"@timestamp":{"type":"date"},"dimension":{"type":"keyword","time_series_dimension":true},
            "label-dimension":{"type":"keyword","fields":{"dimension":{"type":"keyword","time_series_dimension":true}}},
            "label-text":{"type":"text","fields":{"keyword":{"type":"keyword"},"other":{"type":"keyword"}}},
            "notes":{"type":"text"},"metric":{"type":"long","time_series_metric":"gauge"}}"#,
        );
        let f = collect(&props, "@timestamp").unwrap();
        let got: Vec<(String, Role, Read)> = f.fields.iter().map(|f| (f.name.clone(), f.role.clone(), f.read.clone())).collect();
        assert_eq!(
            got,
            vec![
                ("dimension".into(), Role::Dimension, Read::DocValues("dimension".into())),
                ("label-dimension".into(), Role::Dimension, Read::DocValues("label-dimension.dimension".into())),
                ("label-text".into(), Role::Label, Read::DocValues("label-text.keyword".into())),
                ("metric".into(), Role::Metric(Metric::Gauge), Read::DocValues("metric".into())),
            ]
        );
        let bad = parse(r#"{"cpu":{"type":"long","fields":{"gauge":{"type":"long","time_series_metric":"gauge"}}}}"#);
        assert_eq!(
            collect(&bad, "@timestamp").unwrap_err().reason(),
            "Downsampling failed because index mapping contains time series metrics as a multi field [cpu.gauge]"
        );
    }

    #[test]
    fn target_mapping_turns_gauges_into_aggregate_metrics() {
        let m = parse(
            r#"{"subobjects":false,"runtime":{"r":{"type":"keyword"}},"properties":{"@timestamp":{"type":"date_nanos","format":"uuuu-MM-dd","doc_values":true},
            "g":{"type":"double","time_series_metric":"gauge"},"c":{"type":"long","time_series_metric":"counter"},
            "a":{"type":"aggregate_metric_double","metrics":["min","sum","value_count"],"default_metric":"min","time_series_metric":"gauge"}}}"#,
        );
        assert_eq!(
            Value::Obj(target_mapping(&m, "@timestamp", true, "1h")).to_json(),
            r#"{"subobjects":false,"runtime":{"r":{"type":"keyword"}},"properties":{"@timestamp":{"type":"date_nanos","format":"uuuu-MM-dd","meta":{"fixed_interval":"1h","time_zone":"UTC"}},"g":{"type":"aggregate_metric_double","metrics":["min","max","sum","value_count"],"time_series_metric":"gauge"},"c":{"type":"long","time_series_metric":"counter"},"a":{"type":"aggregate_metric_double","metrics":["min","sum","value_count"],"default_metric":"min","time_series_metric":"gauge"}}}"#
        );
        let last = target_mapping(&m, "@timestamp", false, "1h");
        assert_eq!(last["properties"].pointer(&["g", "type"]).and_then(Value::as_str), Some("double"));
    }
}
