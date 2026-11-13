//! Downsampling: `POST /<source>/_downsample/<target>` (`indices.downsample`).
//!
//! As Elasticsearch's `TransportDownsampleAction`, the source must be a write-blocked time
//! series index. The target is created in one cluster state update with the source's time
//! series settings, the downsampling settings (`index.downsample.status: started`, interval,
//! method, origin and source), every other source setting it doesn't already have from the
//! index templates (not the pipelines, the write block or the ILM policy; the source's tier
//! preference wins), and the mapping [`fields::target_mapping`] makes. It is hidden and has
//! no replicas while it fills.
//!
//! The source is read in its index sort's order (`_tsid` ascending, `@timestamp` descending),
//! a page at a time with `search_after` (pages stop early in each segment, which is sorted the
//! same way), and [`sampler::Sampler`] turns it into documents bucket by bucket, written with
//! `_bulk` as they come: memory holds one page, one bucket and one bulk request, whatever the
//! size of the source, and the request yields between pages so the node keeps serving. Then
//! the target is write-blocked, gets the source's replicas and visibility back, is refreshed
//! and marked `index.downsample.status: success`.
//!
//! A target already marked `success` acknowledges at once; one still `started` (a downsample
//! that failed or was cut short) is filled again: documents get the same `_id`s, so the
//! result is the same.

mod fields;
mod sampler;

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::meta::IndexMeta;
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::{enc, internal};
use fields::{Fields, Read};
use sampler::{Clock, Doc, Method, Sampler};
use std::sync::Arc;

const BODY_FIELDS: [&str; 2] = ["fixed_interval", "sampling_method"];
const TIMESTAMP: &str = "@timestamp";
/// Source documents per scan page.
const PAGE: usize = 1000;
/// A bulk request is sent at this many documents or bytes (`DOWNSAMPLE_BULK_ACTIONS`,
/// `DOWNSAMPLE_BULK_SIZE`).
const BULK_DOCS: usize = 10_000;
const BULK_BYTES: usize = 1 << 20;
const TIER_PREFERENCE: &str = "index.routing.allocation.include._tier_preference";

/// Source settings a target never copies (`FORBIDDEN_SETTINGS`, and those Elasticsearch marks
/// `NotCopyableOnResize` or that belong to the source index alone).
fn copyable(key: &str) -> bool {
    const SKIP: &[&str] = &["index.default_pipeline", "index.final_pipeline", "index.hidden", "index.frozen", "index.verified_before_close", "index.recovery.type", "index.store.type"];
    const SKIP_PREFIX: &[&str] = &["index.blocks.", "index.lifecycle.", "index.store.snapshot.", "index.resize.", "index.shrink.", "index.routing.allocation.initial_recovery."];
    !SKIP.contains(&key) && !SKIP_PREFIX.iter().any(|p| key.starts_with(p))
}

pub async fn downsample(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let source = ctx.path_param("index").unwrap_or_default().to_string();
    let target = ctx.path_param("target_index").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    if let Some(k) = body.keys().find(|k| !BODY_FIELDS.contains(&k.as_str())) {
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
    // `canShortCircuit`: a finished target acknowledges, an index that isn't a downsample
    // target already exists; a started one is filled again.
    let resume = match st.metadata.indices.get(&target) {
        None => false,
        Some(t) => match t.settings.get("index.downsample.status") {
            Some("success") => return Reply::acknowledged(),
            Some("started") => true,
            _ => return Err(EsError::index_already_exists(&target, &t.uuid)),
        },
    };
    drop(st);

    let mapping = internal::call(ctx.cluster, "GET", &format!("/{}/_mapping", enc(&source)), None).await?;
    let source_mapping = mapping.pointer(&[&source, "mappings"]).and_then(Value::as_object).cloned().unwrap_or_default();
    let props = source_mapping.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
    let ts_def = props.get(TIMESTAMP).and_then(Value::as_object).cloned().unwrap_or_default();
    validate_downsampled_source(&ts_def, &src, &interval, interval_ms, method)?;
    let fields = fields::collect(&props, TIMESTAMP)?;
    if !fields.has_dimensions() {
        return Err(EsError::action_request_validation(&[format!("Index [{source}] does not contain any dimension fields")]));
    }

    let task_id = format!("downsample-{target}-0-{interval}");
    if !resume {
        let mapping = fields::target_mapping(&source_mapping, TIMESTAMP, method == Method::Aggregate, &interval);
        create_target(ctx, &src, &target, mapping, &interval, method).await?;
    }
    {
        let st = ctx.state();
        if let Some(t) = st.metadata.indices.get(&target)
            && ctx.cluster.shards_failed(&st, t)
        {
            return Err(EsError::new(500, "exception", format!("downsample task [{task_id}] failed")));
        }
    }

    let clock = clock(&ts_def)?;
    let start_ms = src.settings.get("index.time_series.start_time").and_then(|s| crate::time::DateFormatter::new("strict_date_optional_time").ok()?.parse_millis(s).ok()).unwrap_or(i64::MIN);
    let temporality = src.settings.get("index.time_series.temporality_field").filter(|s| !s.is_empty());
    let sampler = Sampler::new(&fields, method, &clock, interval_ms, start_ms, temporality);
    let mut writer = Writer { ctx, target: &target, task_id: &task_id, body: String::new(), docs: 0 };
    scan(ctx, &source, &fields, sampler, &mut writer).await?;
    writer.flush().await?;
    finish(ctx, &src, &target).await?;
    Reply::acknowledged()
}

/// A downsampled source (its `@timestamp` records an interval) only downsamples to a multiple
/// of its interval, with the same method and time zone (`validateDownsamplingConfiguration`).
fn validate_downsampled_source(ts_def: &Map, src: &IndexMeta, interval: &str, interval_ms: i64, method: Method) -> EsResult<()> {
    let Some(Value::Obj(meta)) = ts_def.get("meta") else { return Ok(()) };
    let mut errors = Vec::new();
    if let Some(src_interval) = meta.get("fixed_interval").and_then(Value::as_str) {
        let src_ms = crate::units::TimeValue::parse(src_interval, "fixed_interval").map(|t| t.as_millis()).unwrap_or(0);
        if src_ms > 0 && interval_ms <= src_ms {
            errors.push(format!("Source index is a downsampled index. Downsampling interval [{interval}] must be greater than the source index interval [{src_interval}]."));
        } else if src_ms > 0 && interval_ms % src_ms != 0 {
            errors.push(format!("Source index is a downsampled index. Downsampling interval [{interval}] must be a multiple of the source index interval [{src_interval}]."));
        }
        let src_method = src.settings.get("index.downsample.sampling_method").filter(|m| !m.is_empty()).unwrap_or("aggregate");
        if src_method != method.name() {
            errors.push(format!(
                "Source index is a downsampled index. Downsampling method [{}] is not compatible with the source index downsampling method [{src_method}].",
                method.name()
            ));
        }
    }
    if let Some(tz) = meta.get("time_zone").and_then(Value::as_str)
        && tz != "UTC"
    {
        errors.push(format!("Source index is a downsampled index. Downsampling timezone [UTC] cannot be different than the source index timezone [{tz}]."));
    }
    if errors.is_empty() { Ok(()) } else { Err(EsError::action_request_validation(&errors)) }
}

/// The timestamp's resolution and its doc value format.
fn clock(ts_def: &Map) -> EsResult<Clock> {
    let nanos = ts_def.get("type").and_then(Value::as_str) == Some("date_nanos");
    let default = if nanos { "strict_date_optional_time_nanos||epoch_millis" } else { "strict_date_optional_time||epoch_millis" };
    let format = ts_def.get("format").and_then(Value::as_str).unwrap_or(default);
    let format = crate::time::DateFormatter::new(format).map_err(EsError::illegal_argument)?;
    Ok(Clock { nanos, format })
}

/// Creates the target index in one cluster state update (`createDownsampleIndex` and
/// `copyIndexMetadata`).
async fn create_target(ctx: &Ctx<'_>, src: &Arc<IndexMeta>, target: &str, mapping: Map, interval: &str, method: Method) -> EsResult<()> {
    use crate::meta::settings::SettingValue;
    let s = &src.settings;
    let mut req = crate::api::indices::CreateRequest::default();
    let set = &mut req.system;
    set.set("index.hidden", "true");
    set.set("index.number_of_shards", s.get("index.number_of_shards").unwrap_or("1"));
    set.set("index.number_of_replicas", "0");
    set.set("index.refresh_interval", "-1");
    set.set("index.downsample.status", "started");
    set.set("index.downsample.interval", interval);
    set.set("index.downsample.sampling_method", method.name());
    set.set("index.mode", "time_series");
    for k in ["index.time_series.start_time", "index.time_series.end_time", "index.mapping.total_fields.limit", "index.mapping.ignore_malformed"] {
        if let Some(v) = s.get(k) {
            set.set(k, v);
        }
    }
    for k in ["index.routing_path", "index.dimensions"] {
        if let Some(l) = s.get_list(k).filter(|l| !l.is_empty()) {
            set.insert(k, SettingValue::List(l));
        }
    }
    req.mappings = Some(mapping);
    let (source, target) = (src.clone(), target.to_string());
    let (now, scripts) = (ctx.now_ms, ctx.cluster.scripts.clone());
    let warnings = ctx
        .cluster
        .update(move |st| {
            let w = crate::api::indices::create_index(st, &target, &target, req, now)?;
            let Some(created) = st.metadata.indices.get(&target) else { return Err(EsError::index_not_found(&target)) };
            crate::search::runtime::check_mapping_scripts(&scripts, st, &created.mapping)?;
            let mut idx = (**created).clone();
            let t = &mut idx.settings;
            for (k, v) in source.settings.iter() {
                if k == TIER_PREFERENCE || (copyable(k) && !t.contains(k)) {
                    t.insert(k.clone(), v.clone());
                }
            }
            let origin = |k: &str| source.settings.get(k).filter(|v| !v.is_empty()).map(str::to_string);
            if origin("index.downsample.origin.name").is_none() || origin("index.downsample.origin.uuid").is_none() {
                t.set("index.downsample.origin.name", source.name.clone());
                t.set("index.downsample.origin.uuid", source.uuid.clone());
            }
            t.set("index.downsample.source.name", source.name.clone());
            t.set("index.downsample.source.uuid", source.uuid.clone());
            idx.settings_version += 1;
            st.metadata.indices.insert(target.clone(), Arc::new(idx));
            Ok(w)
        })
        .await?;
    ctx.warn_all(warnings);
    Ok(())
}

/// Reads the source in the index sort's order and samples it.
async fn scan(ctx: &Ctx<'_>, source: &str, fields: &Fields, mut sampler: Sampler<'_>, writer: &mut Writer<'_, '_>) -> EsResult<()> {
    let mut includes = vec!["_doc_count".to_string()];
    includes.extend(fields.source_paths());
    let mut query = Map::new();
    query.insert("size".into(), Value::from(PAGE));
    query.insert("track_total_hits".into(), Value::Bool(false));
    query.insert("sort".into(), Value::from(vec![obj! { "_tsid" => "asc" }, obj! { TIMESTAMP => "desc" }]));
    query.insert("_source".into(), obj! { "includes" => Value::from(includes) });
    query.insert("docvalue_fields".into(), Value::from(fields.doc_value_fields()));
    let uri = format!("/{}/_search", enc(source));
    let mut out = Vec::new();
    loop {
        let page = internal::call(ctx.cluster, "POST", &uri, Some(&Value::Obj(query.clone()))).await?;
        let hits = page.pointer(&["hits", "hits"]).and_then(Value::as_array).cloned().unwrap_or_default();
        for h in &hits {
            if let Some(doc) = read_doc(h, fields) {
                sampler.push(doc, &mut out);
            }
        }
        for d in out.drain(..) {
            writer.add(d).await?;
        }
        match hits.last().and_then(|h| h.get("sort")) {
            Some(after) if hits.len() == PAGE => query.insert("search_after".into(), after.clone()),
            _ => break,
        };
    }
    sampler.finish(&mut out);
    for d in out {
        writer.add(d).await?;
    }
    Ok(())
}

/// A search hit as the sampler sees it.
fn read_doc(hit: &Value, fields: &Fields) -> Option<Doc> {
    let sort = hit.get("sort").and_then(Value::as_array)?;
    let tsid = sort.first()?.to_es_string();
    let timestamp = sort.get(1)?.as_i64()?;
    let empty = Map::new();
    let dv = hit.get("fields").and_then(Value::as_object).unwrap_or(&empty);
    let src = hit.get("_source").and_then(Value::as_object).unwrap_or(&empty);
    let doc_count = extract(src, "_doc_count").and_then(Value::as_i64).unwrap_or(1);
    let values = fields
        .fields
        .iter()
        .map(|f| match &f.read {
            Read::DocValues(n) => dv.get(n).filter(|v| v.as_array().is_some_and(|a| !a.is_empty())).cloned(),
            Read::Source(p) => extract(src, p).filter(|v| !v.is_null()).cloned(),
        })
        .collect();
    Some(Doc { tsid, timestamp, doc_count, values })
}

/// The value at a dotted path of a source document, through objects or dotted keys.
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

/// Batches the downsampled documents into `_bulk` requests.
struct Writer<'c, 'r> {
    ctx: &'r Ctx<'c>,
    target: &'r str,
    task_id: &'r str,
    body: String,
    docs: usize,
}

impl Writer<'_, '_> {
    async fn add(&mut self, doc: Map) -> EsResult<()> {
        self.body.push_str("{\"index\":{}}\n");
        self.body.push_str(&Value::Obj(doc).to_json());
        self.body.push('\n');
        self.docs += 1;
        if self.docs >= BULK_DOCS || self.body.len() >= BULK_BYTES {
            self.flush().await?;
        }
        Ok(())
    }

    /// A rejected document fails the downsample, as it fails the shard task.
    async fn flush(&mut self) -> EsResult<()> {
        if self.docs == 0 {
            return Ok(());
        }
        let body = std::mem::take(&mut self.body);
        self.docs = 0;
        let failed = || EsError::new(500, "exception", format!("downsample task [{}] failed", self.task_id));
        let r = internal::call_ndjson(self.ctx.cluster, "POST", &format!("/{}/_bulk", enc(self.target)), body).await.map_err(|_| failed())?;
        if r.get("errors").and_then(Value::as_bool) == Some(true) {
            return Err(failed());
        }
        Ok(())
    }
}

/// Write-blocks the target, gives it the source's replicas and visibility back, refreshes it
/// and marks it done (`updateTargetIndexSettingStep` and the refresh step).
async fn finish(ctx: &Ctx<'_>, src: &Arc<IndexMeta>, target: &str) -> EsResult<()> {
    let mut settings = Map::new();
    settings.insert("index.blocks.write".into(), Value::Bool(true));
    if let Some(r) = src.settings.get("index.number_of_replicas").filter(|r| *r != "0") {
        settings.insert("index.number_of_replicas".into(), Value::from(r));
    }
    if src.settings.get("index.hidden") != Some("true") {
        let v = if src.settings.contains("index.hidden") { Value::Bool(false) } else { Value::Null };
        settings.insert("index.hidden".into(), v);
    }
    internal::call(ctx.cluster, "PUT", &format!("/{}/_settings", enc(target)), Some(&Value::Obj(settings))).await?;
    internal::call(ctx.cluster, "POST", &format!("/{}/_refresh", enc(target)), None).await?;
    let target = target.to_string();
    ctx.cluster
        .update(move |st| {
            let Some(idx) = st.metadata.indices.get(&target) else {
                return Err(EsError::illegal_state(format!("Failed to update downsample status because [{target}] does not exist")));
            };
            if idx.settings.get("index.downsample.status") == Some("success") {
                return Ok(());
            }
            let mut idx = (**idx).clone();
            idx.settings.set("index.downsample.status", "success");
            idx.settings_version += 1;
            st.metadata.indices.insert(target, Arc::new(idx));
            Ok(())
        })
        .await
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
    fn settings_a_target_never_copies() {
        for k in ["index.default_pipeline", "index.final_pipeline", "index.blocks.write", "index.lifecycle.name", "index.hidden"] {
            assert!(!copyable(k), "{k}");
        }
        for k in ["index.codec", TIER_PREFERENCE, "index.routing.allocation.include.zone", "index.dimensions"] {
            assert!(copyable(k), "{k}");
        }
    }
}
