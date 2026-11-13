//! Downsampling: `POST /<source>/_downsample/<target>` (`indices.downsample`).
//!
//! Like Elasticsearch's `TransportDownsampleAction`, the source must be a write-blocked time
//! series index; the target is a new time series index with the source's dimensions, where
//! every time series gets one document per `fixed_interval` bucket (UTC):
//!
//! * `aggregate` (default): gauges become `aggregate_metric_double` (`min`, `max`, `sum`,
//!   `value_count`; a downsampled source's pre-aggregated gauges are merged), counters keep
//!   their first value in the bucket, other fields (labels) their last, and `_doc_count`
//!   counts the source documents (summing a downsampled source's counts).
//! * `last_value`: every metric and label keeps its last value.
//!
//! The source is read in one pass (a scroll in `_doc` order), grouping in memory by time
//! series and bucket, so memory is proportional to the output, not the input. The target is
//! created with the source's settings and transformed mapping, filled through `_bulk` (time
//! series ids are generated from dimensions and timestamp like for any time series write),
//! refreshed, then marked `index.downsample.status: success` and write-blocked. A request for
//! a target that already finished is acknowledged again, as Elasticsearch does.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::{enc, internal};
use std::collections::HashMap;
use std::sync::Arc;

const FIELDS: [&str; 2] = ["fixed_interval", "sampling_method"];
const PAGE: usize = 1000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Method {
    Aggregate,
    LastValue,
}

impl Method {
    fn name(self) -> &'static str {
        match self {
            Method::Aggregate => "aggregate",
            Method::LastValue => "last_value",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Dimension,
    Gauge,
    Counter,
    Label,
}

/// A leaf field of the source mapping and what downsampling does with it.
#[derive(Debug)]
struct Field {
    path: String,
    role: Role,
}

/// Walks a mapping's `properties`, collecting leaf fields (multi-fields are read through their
/// parent, so they are skipped).
fn collect_fields(props: &Map, prefix: &str, out: &mut Vec<Field>) {
    for (name, def) in props {
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        if let Some(Value::Obj(sub)) = def.get("properties") {
            collect_fields(sub, &path, out);
            continue;
        }
        let ty = def.get("type").and_then(Value::as_str).unwrap_or("object");
        if matches!(ty, "object" | "nested" | "alias") || path == "@timestamp" {
            continue;
        }
        let role = if def.get("time_series_dimension").and_then(Value::as_bool) == Some(true) {
            Role::Dimension
        } else {
            match def.get("time_series_metric").and_then(Value::as_str) {
                Some("gauge") => Role::Gauge,
                Some("counter") => Role::Counter,
                _ => Role::Label,
            }
        };
        out.push(Field { path, role });
    }
}

/// The target mapping: gauges become `aggregate_metric_double` (aggregate method) and
/// `@timestamp` records the interval.
fn target_properties(props: &Map, prefix: &str, method: Method, interval: &str) -> Map {
    let mut out = Map::new();
    for (name, def) in props {
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        let mut def = def.clone();
        if let Value::Obj(m) = &mut def {
            if let Some(Value::Obj(sub)) = m.get("properties") {
                let sub = target_properties(sub, &path, method, interval);
                m.insert("properties".into(), Value::Obj(sub));
            } else if path == "@timestamp" {
                m.insert("meta".into(), obj! { "fixed_interval" => interval, "time_zone" => "UTC" });
            } else if method == Method::Aggregate
                && m.get("time_series_metric").and_then(Value::as_str) == Some("gauge")
                && m.get("type").and_then(Value::as_str) != Some("aggregate_metric_double")
            {
                m.clear();
                m.insert("type".into(), "aggregate_metric_double".into());
                m.insert("metrics".into(), Value::from(vec!["min", "max", "sum", "value_count"]));
                m.insert("time_series_metric".into(), "gauge".into());
            }
        }
        out.insert(name.clone(), def);
    }
    out
}

/// The value at a dotted path of a source document, through nested objects or dotted keys.
fn extract<'a>(src: &'a Map, path: &str) -> Option<&'a Value> {
    if let Some(v) = src.get(path) {
        return Some(v);
    }
    let mut from = 0;
    while let Some(i) = path[from..].find('.') {
        let cut = from + i;
        if let Some(Value::Obj(sub)) = src.get(&path[..cut])
            && let Some(v) = extract(sub, &path[cut + 1..])
        {
            return Some(v);
        }
        from = cut + 1;
    }
    None
}

/// Inserts `v` at a dotted path, creating objects on the way.
fn insert_path(doc: &mut Map, path: &str, v: Value) {
    match path.split_once('.') {
        None => {
            doc.insert(path.to_string(), v);
        }
        Some((head, rest)) => {
            let e = doc.entry(head.to_string()).or_insert_with(|| Value::Obj(Map::new()));
            if !matches!(e, Value::Obj(_)) {
                *e = Value::Obj(Map::new());
            }
            if let Value::Obj(m) = e {
                insert_path(m, rest, v);
            }
        }
    }
}

#[derive(Clone, Copy)]
struct GaugeAcc {
    min: f64,
    max: f64,
    sum: f64,
    count: i64,
}

impl GaugeAcc {
    const EMPTY: GaugeAcc = GaugeAcc { min: f64::INFINITY, max: f64::NEG_INFINITY, sum: 0.0, count: 0 };

    fn add_value(&mut self, v: &Value) {
        match v {
            Value::Num(n) => {
                let x = n.as_f64();
                self.min = self.min.min(x);
                self.max = self.max.max(x);
                self.sum += x;
                self.count += 1;
            }
            Value::Str(s) => {
                if let Ok(x) = s.parse::<f64>() {
                    self.add_value(&Value::from(x));
                }
            }
            Value::Arr(a) => a.iter().for_each(|x| self.add_value(x)),
            // A downsampled source's pre-aggregated gauge.
            Value::Obj(m) => {
                let f = |k: &str| m.get(k).and_then(Value::as_f64);
                if let Some(x) = f("min") {
                    self.min = self.min.min(x);
                }
                if let Some(x) = f("max") {
                    self.max = self.max.max(x);
                }
                if let Some(x) = f("sum") {
                    self.sum += x;
                }
                self.count += m.get("value_count").and_then(Value::as_i64).unwrap_or(0);
            }
            _ => {}
        }
    }

    fn to_value(self) -> Value {
        obj! { "min" => self.min, "max" => self.max, "sum" => self.sum, "value_count" => self.count }
    }
}

/// One output document being built.
struct Bucket {
    doc_count: i64,
    dims: Vec<Value>,
    gauges: Vec<GaugeAcc>,
    /// Per non-gauge (or last-value) field: the timestamp it was seen at and its value.
    firsts: Vec<Option<(i64, Value)>>,
    lasts: Vec<Option<(i64, Value)>>,
}

struct Plan {
    method: Method,
    fields: Vec<Field>,
    dims: Vec<usize>,
}

impl Plan {
    fn accumulate(&self, b: &mut Bucket, src: &Map, ts: i64) {
        b.doc_count += src.get("_doc_count").and_then(Value::as_i64).unwrap_or(1);
        for (i, f) in self.fields.iter().enumerate() {
            let Some(v) = extract(src, &f.path) else { continue };
            if v.is_null() {
                continue;
            }
            match (f.role, self.method) {
                (Role::Dimension, _) => {}
                (Role::Gauge, Method::Aggregate) => b.gauges[i].add_value(v),
                (Role::Counter, Method::Aggregate) => {
                    if b.firsts[i].as_ref().is_none_or(|(t, _)| ts < *t) {
                        b.firsts[i] = Some((ts, v.clone()));
                    }
                }
                _ => {
                    if b.lasts[i].as_ref().is_none_or(|(t, _)| ts >= *t) {
                        b.lasts[i] = Some((ts, v.clone()));
                    }
                }
            }
        }
    }

    fn render(&self, b: Bucket, bucket_ms: i64, fmt: &crate::time::DateFormatter) -> Map {
        let mut paths: Vec<(&str, Value)> = Vec::with_capacity(self.fields.len());
        for (k, &i) in self.dims.iter().enumerate() {
            paths.push((&self.fields[i].path, b.dims[k].clone()));
        }
        let Bucket { gauges, mut firsts, mut lasts, .. } = b;
        for (i, f) in self.fields.iter().enumerate() {
            let v = match (f.role, self.method) {
                (Role::Dimension, _) => continue,
                (Role::Gauge, Method::Aggregate) => (gauges[i].count > 0).then(|| gauges[i].to_value()),
                (Role::Counter, Method::Aggregate) => firsts[i].take().map(|(_, v)| v),
                _ => lasts[i].take().map(|(_, v)| v),
            };
            if let Some(v) = v {
                paths.push((&f.path, v));
            }
        }
        paths.sort_by(|a, b| a.0.cmp(b.0));
        let mut doc = Map::new();
        doc.insert("@timestamp".into(), Value::Str(fmt.format_millis(bucket_ms)));
        doc.insert("_doc_count".into(), Value::from(b.doc_count));
        for (p, v) in paths {
            insert_path(&mut doc, p, v);
        }
        doc
    }
}

fn bad_interval_message(target: &str, source: &str, multiple: bool) -> String {
    if multiple {
        format!("Source index is a downsampled index. Downsampling interval [{target}] must be a multiple of the source index interval [{source}].")
    } else {
        format!("Source index is a downsampled index. Downsampling interval [{target}] must be greater than the source index interval [{source}].")
    }
}

pub async fn downsample(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let source = ctx.path_param("index").unwrap_or_default().to_string();
    let target = ctx.path_param("target_index").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    if let Some(k) = body.keys().find(|k| !FIELDS.contains(&k.as_str())) {
        return Err(crate::xpack::unknown_field(ctx, "downsample/action/config", k));
    }
    let Some(interval) = body.get("fixed_interval").map(Value::to_es_string) else {
        return Err(EsError::illegal_argument("Required [fixed_interval]"));
    };
    let interval_ms = crate::units::TimeValue::parse(&interval, "createRounding")
        .map_err(|e| EsError::x_content_parse_bare("Failed to build [downsample/action/config] after last required field arrived").caused_by(e))?
        .as_millis();
    let method = match body.get("sampling_method").map(Value::to_es_string).as_deref() {
        None | Some("aggregate") => Method::Aggregate,
        Some("last_value") => Method::LastValue,
        Some(other) => {
            let (l, c) = crate::xpack::value_loc(ctx, "sampling_method");
            return Err(EsError::x_content_parse(l, c, "[downsample/action/config] failed to parse field [sampling_method]").caused_by(EsError::illegal_argument(
                format!("Sampling method [{other}] is not one of the accepted methods [aggregate, last_value]."),
            )));
        }
    };
    if interval_ms <= 0 {
        return Err(EsError::illegal_argument(format!("failed to parse setting [createRounding] with value [{interval}] as a time value: zero or negative intervals are not supported")));
    }
    crate::meta::names::validate_index_name(&target)?;

    let st = ctx.state();
    let Some(src) = st.metadata.indices.get(&source).cloned() else {
        return Err(EsError::index_not_found_bare(&source));
    };
    if !crate::engine::tsdb::is_time_series(&src) {
        return Err(EsError::new(500, "exception", format!("Rollup requires setting [index.mode=time_series] for index [{source}]")));
    }
    if src.settings.get("index.blocks.write") != Some("true") {
        return Err(EsError::new(500, "exception", format!("Downsample requires setting [index.blocks.write = true] for index [{source}]")));
    }
    if let Some(src_interval) = src.settings.get("index.downsample.interval").filter(|s| !s.is_empty()) {
        let src_ms = crate::units::TimeValue::parse(src_interval, "index.downsample.interval").map(|t| t.as_millis()).unwrap_or(0);
        if src_ms > 0 && interval_ms <= src_ms {
            return Err(EsError::action_request_validation(&[bad_interval_message(&interval, src_interval, false)]));
        }
        if src_ms > 0 && interval_ms % src_ms != 0 {
            return Err(EsError::action_request_validation(&[bad_interval_message(&interval, src_interval, true)]));
        }
    }
    if let Some(existing) = st.metadata.indices.get(&target) {
        if existing.settings.get("index.downsample.status") == Some("success") && existing.settings.get("index.downsample.source.name") == Some(&source) {
            return Reply::acknowledged();
        }
        return Err(EsError::index_already_exists(&target, &existing.uuid));
    }
    drop(st);

    // The source mapping and the fields' roles.
    let mapping = internal::call(ctx.cluster, "GET", &format!("/{}/_mapping", enc(&source)), None).await?;
    let props = mapping.pointer(&[&source, "mappings", "properties"]).and_then(Value::as_object).cloned().unwrap_or_default();
    let mut fields = Vec::new();
    collect_fields(&props, "", &mut fields);
    let dims: Vec<usize> = fields.iter().enumerate().filter(|(_, f)| f.role == Role::Dimension).map(|(i, _)| i).collect();
    let plan = Plan { method, fields, dims };
    let ts_format = props.get("@timestamp").and_then(|d| d.get("format")).and_then(Value::as_str).unwrap_or("strict_date_optional_time||epoch_millis").to_string();
    let parser = crate::time::DateFormatter::new(&ts_format).or_else(|_| crate::time::DateFormatter::new("strict_date_optional_time||epoch_millis")).map_err(EsError::illegal_argument)?;
    let printer = crate::time::DateFormatter::new("strict_date_optional_time").expect("built-in format");

    // One pass over the source.
    let mut buckets: HashMap<(String, i64), Bucket> = HashMap::new();
    let mut page = internal::call(
        ctx.cluster,
        "POST",
        &format!("/{}/_search?scroll=1m", enc(&source)),
        Some(&obj! { "size" => PAGE, "sort" => Value::from(vec!["_doc"]) }),
    )
    .await?;
    let mut scroll_id = page.get("_scroll_id").map(Value::to_es_string);
    loop {
        let hits = page.pointer(&["hits", "hits"]).and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
        if hits == 0 {
            break;
        }
        if let Some(Value::Arr(hits)) = page.pointer(&["hits", "hits"]) {
            for h in hits {
                let Some(Value::Obj(doc)) = h.get("_source") else { continue };
                let Some(ts) = extract(doc, "@timestamp").and_then(|v| parser.parse_millis(&v.to_es_string()).ok()) else { continue };
                let bucket_ms = ts.div_euclid(interval_ms) * interval_ms;
                let dim_values: Vec<Value> = plan.dims.iter().map(|&i| extract(doc, &plan.fields[i].path).cloned().unwrap_or(Value::Null)).collect();
                let key = (Value::Arr(dim_values.clone()).to_json(), bucket_ms);
                let n = plan.fields.len();
                let b = buckets.entry(key).or_insert_with(|| Bucket {
                    doc_count: 0,
                    dims: dim_values,
                    gauges: vec![GaugeAcc::EMPTY; n],
                    firsts: vec![None; n],
                    lasts: vec![None; n],
                });
                plan.accumulate(b, doc, ts);
            }
        }
        if hits < PAGE {
            break;
        }
        let Some(id) = scroll_id.clone() else { break };
        page = internal::call(ctx.cluster, "POST", "/_search/scroll", Some(&obj! { "scroll" => "1m", "scroll_id" => id })).await?;
        if let Some(id) = page.get("_scroll_id") {
            scroll_id = Some(id.to_es_string());
        }
    }
    if let Some(id) = scroll_id {
        let _ = internal::call(ctx.cluster, "DELETE", "/_search/scroll", Some(&obj! { "scroll_id" => id })).await;
    }

    // The target index: the source's time series settings and the transformed mapping.
    let mut settings = Map::new();
    for k in ["index.number_of_shards", "index.number_of_replicas", "index.mode", "index.time_series.start_time", "index.time_series.end_time", "index.codec"] {
        if let Some(v) = src.settings.get(k) {
            settings.insert(k.into(), v.into());
        }
    }
    if let Some(rp) = src.settings.get_list("index.routing_path") {
        settings.insert("index.routing_path".into(), Value::from(rp));
    }
    let mut mappings = Map::new();
    mappings.insert("properties".into(), Value::Obj(target_properties(&props, "", method, &interval)));
    internal::call(ctx.cluster, "PUT", &format!("/{}", enc(&target)), Some(&obj! { "settings" => Value::Obj(settings), "mappings" => Value::Obj(mappings) })).await?;

    let mut out: Vec<((String, i64), Bucket)> = buckets.into_iter().collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    let mut nd = String::new();
    let mut pending = 0usize;
    let result: EsResult<()> = async {
        for ((_, bucket_ms), b) in out {
            nd.push_str("{\"create\":{}}\n");
            nd.push_str(&Value::Obj(plan.render(b, bucket_ms, &printer)).to_json());
            nd.push('\n');
            pending += 1;
            if pending == PAGE {
                bulk(ctx, &target, std::mem::take(&mut nd)).await?;
                pending = 0;
            }
        }
        if pending > 0 {
            bulk(ctx, &target, nd).await?;
        }
        internal::call(ctx.cluster, "POST", &format!("/{}/_refresh", enc(&target)), None).await?;
        Ok(())
    }
    .await;
    if let Err(e) = result {
        let _ = internal::call(ctx.cluster, "DELETE", &format!("/{}", enc(&target)), None).await;
        return Err(e);
    }

    // Mark it done (private settings, so through the metadata).
    let origin_name = src.settings.get("index.downsample.origin.name").filter(|s| !s.is_empty()).unwrap_or(&source).to_string();
    let origin_uuid = src.settings.get("index.downsample.origin.uuid").filter(|s| !s.is_empty()).unwrap_or(&src.uuid).to_string();
    let (source_name, source_uuid) = (source.clone(), src.uuid.clone());
    let target_name = target.clone();
    ctx.cluster
        .update(move |st| {
            let Some(idx) = st.metadata.indices.get(&target_name) else { return Err(EsError::index_not_found(&target_name)) };
            let mut idx = (**idx).clone();
            let s = &mut idx.settings;
            s.set("index.blocks.write", "true");
            s.set("index.refresh_interval", "-1");
            s.set("index.downsample.interval", interval.clone());
            s.set("index.downsample.sampling_method", method.name());
            s.set("index.downsample.origin.name", origin_name);
            s.set("index.downsample.origin.uuid", origin_uuid);
            s.set("index.downsample.source.name", source_name);
            s.set("index.downsample.source.uuid", source_uuid);
            s.set("index.downsample.status", "success");
            idx.settings_version += 1;
            idx.version += 1;
            st.metadata.indices.insert(target_name, Arc::new(idx));
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

async fn bulk(ctx: &Ctx<'_>, target: &str, nd: String) -> EsResult<()> {
    let r = internal::call_ndjson(ctx.cluster, "POST", &format!("/{}/_bulk", enc(target)), nd).await?;
    if r.get("errors").and_then(Value::as_bool) == Some(true)
        && let Some(Value::Arr(items)) = r.get("items")
        && let Some(err) = items.iter().find_map(|i| i.get("create").and_then(|c| c.get("error")))
    {
        let status = items.iter().find_map(|i| i.pointer(&["create", "status"]).and_then(Value::as_i64)).unwrap_or(500) as u16;
        return Err(internal::error_from_body(status, &obj! { "error" => err.clone() }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Value {
        crate::json::parse(s.as_bytes()).unwrap().unwrap()
    }

    #[test]
    fn extracts_nested_and_dotted_paths() {
        let v = parse(r#"{"a":{"b":{"c":1}},"x.y":2,"p":{"q.r":3}}"#);
        let m = v.as_object().unwrap();
        assert_eq!(extract(m, "a.b.c"), Some(&Value::from(1i64)));
        assert_eq!(extract(m, "x.y"), Some(&Value::from(2i64)));
        assert_eq!(extract(m, "p.q.r"), Some(&Value::from(3i64)));
        assert_eq!(extract(m, "a.z"), None);
    }

    #[test]
    fn gauges_merge_values_arrays_and_aggregates() {
        let mut g = GaugeAcc::EMPTY;
        g.add_value(&parse("[1.5, 2]"));
        g.add_value(&Value::from(3i64));
        assert_eq!(g.to_value().to_json(), r#"{"min":1.5,"max":3.0,"sum":6.5,"value_count":3}"#);
        g.add_value(&parse(r#"{"min":0.5,"max":10,"sum":20,"value_count":4}"#));
        assert_eq!((g.min, g.max, g.sum, g.count), (0.5, 10.0, 26.5, 7));
    }

    /// The documents a real Elasticsearch 9.5.5 wrote for the same input (fixed_interval 1h).
    #[test]
    fn aggregate_bucket_matches_elasticsearch() {
        let props = parse(
            r#"{"@timestamp":{"type":"date"},"metricset":{"type":"keyword","time_series_dimension":true},
            "k8s":{"properties":{"pod":{"properties":{"uid":{"type":"keyword","time_series_dimension":true},"name":{"type":"keyword"},
            "value":{"type":"integer"},"network":{"properties":{"tx":{"type":"long","time_series_metric":"gauge"},
            "sent":{"type":"long","time_series_metric":"counter"}}}}}}}}"#,
        );
        let mut fields = Vec::new();
        collect_fields(props.as_object().unwrap(), "", &mut fields);
        let dims = fields.iter().enumerate().filter(|(_, f)| f.role == Role::Dimension).map(|(i, _)| i).collect::<Vec<_>>();
        let plan = Plan { method: Method::Aggregate, fields, dims };
        let n = plan.fields.len();
        let docs = [
            (r#"{"metricset":"pod","k8s":{"pod":{"name":"cat","uid":"947e4ced","value":1,"network":{"tx":2001818691,"sent":10}}}}"#, 1_619_635_804_467i64),
            (r#"{"metricset":"pod","k8s":{"pod":{"name":"cat2","uid":"947e4ced","value":2,"network":{"tx":2005177954,"sent":20}}}}"#, 1_619_635_824_467),
        ];
        let mut b = Bucket { doc_count: 0, dims: Vec::new(), gauges: vec![GaugeAcc::EMPTY; n], firsts: vec![None; n], lasts: vec![None; n] };
        for (d, ts) in docs.iter().rev() {
            let v = parse(d);
            if b.dims.is_empty() {
                b.dims = plan.dims.iter().map(|&i| extract(v.as_object().unwrap(), &plan.fields[i].path).cloned().unwrap()).collect();
            }
            plan.accumulate(&mut b, v.as_object().unwrap(), *ts);
        }
        let fmt = crate::time::DateFormatter::new("strict_date_optional_time").unwrap();
        let doc = plan.render(b, 1_619_632_800_000, &fmt);
        assert_eq!(
            Value::Obj(doc).to_json(),
            r#"{"@timestamp":"2021-04-28T18:00:00.000Z","_doc_count":2,"k8s":{"pod":{"name":"cat2","network":{"sent":10,"tx":{"min":2.001818691E9,"max":2.005177954E9,"sum":4.006996645E9,"value_count":2}},"uid":"947e4ced","value":2}},"metricset":"pod"}"#
        );
    }

    #[test]
    fn target_mapping_turns_gauges_into_aggregate_metrics() {
        let props = parse(r#"{"@timestamp":{"type":"date"},"g":{"type":"double","time_series_metric":"gauge"},"c":{"type":"long","time_series_metric":"counter"}}"#);
        let out = target_properties(props.as_object().unwrap(), "", Method::Aggregate, "1h");
        assert_eq!(
            Value::Obj(out).to_json(),
            r#"{"@timestamp":{"type":"date","meta":{"fixed_interval":"1h","time_zone":"UTC"}},"g":{"type":"aggregate_metric_double","metrics":["min","max","sum","value_count"],"time_series_metric":"gauge"},"c":{"type":"long","time_series_metric":"counter"}}"#
        );
        let out = target_properties(props.as_object().unwrap(), "", Method::LastValue, "1h");
        assert_eq!(out.get("g").unwrap().get("type").and_then(Value::as_str), Some("double"));
    }
}
