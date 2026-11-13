//! Writes into time series data streams (`DataStream.getWriteIndex(IndexRequest)`): a
//! `create` lands in the backing index whose `[start_time, end_time)` covers the document's
//! `@timestamp`, not necessarily the write index.
//!
//! - The timestamp is read from the top-level `@timestamp` of the (post-ingest) source the way
//!   Elasticsearch's filtered parser does (`getTimestampFromParser`): strings through
//!   `strict_date_optional_time_nanos||strict_date_optional_time||epoch_millis`, numbers as
//!   epoch millis, anything else a `parsing_exception` at the value's location; it is truncated
//!   to whole seconds (`getCanonicalTimestampBound`). Every problem is a `timestamp_error`
//!   (eligible for the failure store).
//! - With `data_stream.past_tsdb_index_creation_enabled`, a timestamp older than every backing
//!   index (but not in the future, and inside the lifecycle's eligible write window) gets a new
//!   past backing index first (`TransportPastTimeSeriesIndexCreationAction`): the gap to the
//!   previous index is filled when it is at most 1.3 intervals wide, otherwise the index is
//!   the interval-sized window (`data_streams.past_tsdb_index_interval`, default 1d) counted
//!   back from the next index's start that covers the timestamp. Past indices are prepended
//!   to the backing indices and bump the generation.

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::meta::ClusterState;
use crate::meta::data_streams::DataStream;
use crate::write::Target;
use std::sync::Arc;

const TIMESTAMP_FORMAT: &str = "strict_date_optional_time_nanos||strict_date_optional_time||epoch_millis";
const GAP_FILL_THRESHOLD: f64 = 1.3;

/// `DataStream.TimestampError`: not an `ElasticsearchException`, so it is its own root cause.
fn timestamp_error(reason: impl Into<String>) -> EsError {
    EsError::java(400, "timestamp_error", reason)
}

/// Whether writes to `ds` route by timestamp.
pub fn routes_by_timestamp(ds: &DataStream) -> bool {
    ds.index_mode.as_deref() == Some("time_series") && !ds.system
}

/// The canonical (whole-second) `@timestamp` of a document, in epoch millis.
pub fn document_timestamp(source: &[u8], value: &Value) -> EsResult<i64> {
    let wrap = |e: EsError| timestamp_error(format!("Error extracting data stream timestamp field: {}", e.reason())).caused_by(e);
    let ts = match value.get("@timestamp") {
        None => {
            let (line, col) = last_token(source);
            return Err(wrap(EsError::parsing(line, col, "Failed to parse object: expecting token of type [START_OBJECT] but found [null]")));
        }
        Some(Value::Str(s)) => {
            let f = crate::time::DateFormatter::new(TIMESTAMP_FORMAT).map_err(EsError::illegal_argument)?;
            match f.parse_detailed(s, None, false) {
                Ok(i) => i.millis(),
                Err(e) => return Err(wrap(e.to_es())),
            }
        }
        Some(Value::Num(n)) => match n {
            crate::json::Number::Int(i) => *i,
            other => other.as_f64() as i64,
        },
        Some(other) => {
            let token = match other {
                Value::Obj(_) => "START_OBJECT",
                Value::Arr(_) => "START_ARRAY",
                Value::Null => "VALUE_NULL",
                Value::Bool(_) => "VALUE_BOOLEAN",
                _ => "VALUE_EMBEDDED_OBJECT",
            };
            let (line, col) = value_location(source, "@timestamp").unwrap_or((1, 1));
            return Err(wrap(EsError::parsing(
                line,
                col,
                format!("Failed to parse object: expecting token of type [VALUE_STRING] or [VALUE_NUMBER] but found [{token}]"),
            )));
        }
    };
    Ok(ts.div_euclid(1000) * 1000)
}

/// Jackson's location (1-based line and column) of the last token of a document.
fn last_token(src: &[u8]) -> (usize, usize) {
    let end = src.iter().rposition(|b| !b.is_ascii_whitespace()).unwrap_or(0);
    location(src, end)
}

fn location(src: &[u8], at: usize) -> (usize, usize) {
    let head = &src[..at.min(src.len())];
    let line = head.iter().filter(|&&b| b == b'\n').count() + 1;
    let start = head.iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
    let col = String::from_utf8_lossy(&head[start..]).chars().count() + 1;
    (line, col)
}

/// Where the value of the top-level `key` starts in a JSON object.
fn value_location(src: &[u8], key: &str) -> Option<(usize, usize)> {
    value_location_path(src, key)
}

/// Jackson's location (1-based line and column) of the first value whose object-key path
/// (keys joined with `.`, array levels transparent) is `path`.
pub fn value_location_path(src: &[u8], path: &str) -> Option<(usize, usize)> {
    // Per open container: whether it is an object, and the key read last in it.
    let mut stack: Vec<(bool, Option<String>)> = Vec::new();
    let mut expect_key = false;
    let current = |stack: &[(bool, Option<String>)]| -> String {
        let keys: Vec<&str> = stack.iter().filter_map(|(o, k)| if *o { k.as_deref() } else { None }).collect();
        keys.join(".")
    };
    let mut i = 0;
    while i < src.len() {
        let b = src[i];
        let value_start = !expect_key && !matches!(b, b'}' | b']' | b',' | b':') && !b.is_ascii_whitespace() && stack.last().is_some_and(|(o, k)| !*o || k.is_some());
        if value_start && current(&stack) == path {
            return Some(location(src, i));
        }
        match b {
            b'{' | b'[' => {
                stack.push((b == b'{', None));
                expect_key = b == b'{';
                i += 1;
            }
            b'}' | b']' => {
                stack.pop();
                if let Some((true, k)) = stack.last_mut() {
                    *k = None;
                }
                expect_key = false;
                i += 1;
            }
            b',' => {
                if let Some((o, k)) = stack.last_mut() {
                    if *o {
                        *k = None;
                    }
                    expect_key = *o;
                }
                i += 1;
            }
            b':' => i += 1,
            b'"' => {
                let start = i;
                i += 1;
                while i < src.len() && src[i] != b'"' {
                    if src[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
                if expect_key {
                    let raw = &src[start..i.min(src.len())];
                    let name = crate::json::parse(raw).ok().flatten().and_then(|v| v.as_str().map(str::to_string));
                    if let Some((_, k)) = stack.last_mut() {
                        *k = name;
                    }
                    expect_key = false;
                } else if let Some((true, k)) = stack.last_mut() {
                    *k = None;
                }
            }
            b if b.is_ascii_whitespace() => i += 1,
            _ => {
                // A scalar: skip it; the key it belonged to is done.
                while i < src.len() && !matches!(src[i], b',' | b'}' | b']') && !src[i].is_ascii_whitespace() {
                    i += 1;
                }
                if let Some((true, k)) = stack.last_mut() {
                    *k = None;
                }
            }
        }
    }
    None
}

fn bounds(st: &ClusterState, index: &str) -> Option<(i64, i64)> {
    let idx = st.metadata.indices.get(index)?;
    if !crate::engine::tsdb::is_time_series(idx) {
        return None;
    }
    let start = crate::meta::index_settings::parse_date_setting(idx.settings.get("index.time_series.start_time")?).ok()?;
    let end = crate::meta::index_settings::parse_date_setting(idx.settings.get("index.time_series.end_time")?).ok()?;
    Some((start, end))
}

/// `selectTimeSeriesWriteIndex`: the newest backing index whose range covers `ms`.
pub fn select(st: &ClusterState, ds: &DataStream, ms: i64) -> Option<String> {
    ds.indices.iter().rev().find(|i| bounds(st, i).is_some_and(|(s, e)| s <= ms && ms < e)).cloned()
}

/// The error for a timestamp no backing index covers.
pub fn outside(st: &ClusterState, ds: &DataStream, ms: i64) -> EsError {
    let shown = crate::time::DateFormatter::new("strict_date_optional_time").map(|f| f.format_millis(ms)).unwrap_or_default();
    let ranges: String = ds
        .indices
        .iter()
        .filter_map(|i| st.metadata.indices.get(i))
        .map(|i| format!("[{},{}]", i.settings.get("index.time_series.start_time").unwrap_or("null"), i.settings.get("index.time_series.end_time").unwrap_or("null")))
        .collect();
    timestamp_error(format!("the document timestamp [{shown}] is outside of ranges of currently writable indices [{ranges}]"))
}

/// `TimeSeriesEligibleWriteWindowLocator.getEligibleWriteWindowStart`: the oldest timestamp
/// the lifecycle still lets a past index be created for (`None`: unbounded).
fn eligible_window_start(st: &ClusterState, ds: &DataStream, now_ms: i64) -> Option<i64> {
    let lc = ds.lifecycle.as_ref()?;
    if lc.get("enabled").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let parse = |v: &Value, key: &str| v.as_str().and_then(|s| crate::units::TimeValue::parse(s, key).ok());
    let window = if let Some(Value::Arr(rounds)) = lc.get("downsampling") {
        rounds.first().and_then(|r| r.get("after")).and_then(|v| parse(v, "after"))
    } else if let Some(f) = lc.get("frozen_after") {
        parse(f, "frozen_after")
    } else {
        crate::api::data_streams::effective_retention(st, ds, lc).map(|(r, _)| r)
    }?;
    Some(now_ms - window.as_millis())
}

/// The `[start, end)` of the past backing index covering `ms`, or `None` when `ms` may not
/// get one (in the future, outside the eligible window, or no time series index to anchor to).
pub fn past_window(st: &ClusterState, ds: &DataStream, ms: i64, now_ms: i64) -> Option<(i64, i64)> {
    if ms > now_ms {
        return None;
    }
    if let Some(w) = eligible_window_start(st, ds, now_ms)
        && w > 0
        && ms < w
    {
        return None;
    }
    let interval = st
        .metadata
        .cluster_setting("data_streams.past_tsdb_index_interval")
        .and_then(|v| crate::units::TimeValue::parse(v, "data_streams.past_tsdb_index_interval").ok())
        .map(|t| t.as_millis())
        .unwrap_or(86_400_000)
        .max(1);
    past_window_in(&ds.indices.iter().filter_map(|i| bounds(st, i)).collect::<Vec<_>>(), ms, interval)
}

/// [`past_window`] over the backing indices' `[start, end)` ranges.
fn past_window_in(ranges: &[(i64, i64)], ms: i64, interval: i64) -> Option<(i64, i64)> {
    // Contiguous covered windows, oldest first.
    let mut windows = ranges.to_vec();
    windows.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (s, e) in windows.into_iter().rev() {
        match merged.last_mut() {
            Some(last) if last.0 == e => last.0 = s,
            _ => merged.push((s, e)),
        }
    }
    merged.reverse();
    let mut previous: Option<(i64, i64)> = None;
    let mut next = None;
    for w in &merged {
        if w.1 <= ms {
            previous = Some(*w);
        } else {
            next = Some(*w);
            break;
        }
    }
    let next = next?;
    if next.0 <= ms {
        return None;
    }
    Some(match previous {
        Some(p) if (next.0 - p.1) as f64 <= interval as f64 * GAP_FILL_THRESHOLD => (p.1, next.0),
        _ => {
            let jumps = (next.0 - ms - 1) / interval;
            let end = next.0 - jumps * interval;
            let start = end - interval;
            (previous.map_or(start, |p| start.max(p.1)), end)
        }
    })
}

/// Routes a `create` into a time series data stream to the backing index covering its
/// timestamp, creating a past backing index when enabled. Other writes keep their target.
pub async fn route(cluster: &Cluster, target: Target, create: bool, source: &[u8], value: &Value, now_ms: i64) -> EsResult<Target> {
    let Some(ds_name) = target.data_stream.clone() else { return Ok(target) };
    let st = cluster.state();
    let Some(ds) = st.metadata.data_streams.get(&ds_name).cloned() else { return Ok(target) };
    // Writes straight to a backing index, and index (not create) operations, use the target.
    if !create || !routes_by_timestamp(&ds) || ds.write_index() != Some(target.index.name.as_str()) {
        return Ok(target);
    }
    let ms = document_timestamp(source, value)?;
    let with_index = |st: &ClusterState, name: &str| -> EsResult<Target> {
        let index = st.metadata.indices.get(name).cloned().ok_or_else(|| EsError::index_not_found(name))?;
        Ok(Target { index, alias_routing: target.alias_routing.clone(), alias: target.alias.clone(), data_stream: Some(ds_name.clone()) })
    };
    if let Some(name) = select(&st, &ds, ms) {
        return with_index(&st, &name);
    }
    let enabled = st.metadata.cluster_setting("data_stream.past_tsdb_index_creation_enabled") == Some("true");
    if enabled && past_window(&st, &ds, ms, now_ms).is_some() {
        let n = ds_name.clone();
        cluster
            .update(move |st| {
                let Some(ds) = st.metadata.data_streams.get(&n).cloned() else { return Ok(()) };
                if select(st, &ds, ms).is_some() {
                    return Ok(());
                }
                let Some((start, end)) = past_window(st, &ds, ms, now_ms) else { return Ok(()) };
                create_past_index(st, &ds, start, end, now_ms)
            })
            .await?;
        let st = cluster.state();
        if let Some(ds) = st.metadata.data_streams.get(&ds_name)
            && let Some(name) = select(&st, ds, ms)
        {
            return with_index(&st, &name);
        }
    }
    Err(outside(&st, &ds, ms))
}

/// `MetadataCreateDataStreamService.createPastBackingIndex`.
fn create_past_index(st: &mut ClusterState, ds: &Arc<DataStream>, start: i64, end: i64, now_ms: i64) -> EsResult<()> {
    let generation = ds.generation + 1;
    let name = crate::meta::data_streams::backing_index_name(crate::meta::data_streams::BACKING_INDEX_PREFIX, &ds.name, generation, start);
    let Some(template) = crate::meta::templates::find_composable(&st.metadata, &ds.name, false).map(|(_, t)| t.clone()) else {
        return Err(EsError::illegal_argument(format!("no matching index template found for data stream [{}]", ds.name)));
    };
    crate::api::indices::create_backing_index(
        st,
        &name,
        crate::api::indices::BackingOf { data_stream: &ds.name, template: &template, settings: &ds.settings, mappings: ds.mappings.as_ref(), past: Some((start, end)) },
        now_ms,
    )?;
    let mut updated = (**ds).clone();
    updated.indices.insert(0, name);
    updated.generation = generation;
    st.metadata.data_streams.insert(ds.name.clone(), Arc::new(updated));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(s: &str) -> (Vec<u8>, Value) {
        (s.as_bytes().to_vec(), crate::json::parse(s.as_bytes()).unwrap().unwrap())
    }

    #[test]
    fn timestamp_extraction() {
        let (b, v) = doc(r#"{"@timestamp":"2021-04-28T18:50:04.467Z","k":"a"}"#);
        assert_eq!(document_timestamp(&b, &v).unwrap(), 1_619_635_804_000);
        let (b, v) = doc(r#"{"@timestamp":1700000000123}"#);
        assert_eq!(document_timestamp(&b, &v).unwrap(), 1_700_000_000_000);
        let (b, v) = doc(r#"{"k":"a"}"#);
        let e = document_timestamp(&b, &v).unwrap_err();
        assert_eq!(e.kind, "timestamp_error");
        assert_eq!(e.reason(), "Error extracting data stream timestamp field: Failed to parse object: expecting token of type [START_OBJECT] but found [null]");
        let (b, v) = doc(r#"{"@timestamp":{"a":1},"k":"a"}"#);
        let e = document_timestamp(&b, &v).unwrap_err();
        assert!(e.reason().ends_with("but found [START_OBJECT]"), "{}", e.reason());
        assert_eq!(value_location(&b, "@timestamp"), Some((1, 15)));
        let (b, _) = doc("{\"k\": {\"@timestamp\": 1},\n \"@timestamp\" : [1]}");
        assert_eq!(value_location(&b, "@timestamp"), Some((2, 17)));
        let (b, _) = doc(r#"{"x":1,"a":{"b":"v"}}"#);
        assert_eq!(value_location_path(&b, "a.b"), Some((1, 17)));
        let (b, _) = doc(r#"{"a":[{"c":1},{"b":true}],"a.b":2}"#);
        assert_eq!(value_location_path(&b, "a.b"), Some((1, 20)));
        assert_eq!(value_location_path(&b, "zz"), None);
        let (b, v) = doc(r#"{"@timestamp":"wrong"}"#);
        assert_eq!(
            document_timestamp(&b, &v).unwrap_err().reason(),
            "Error extracting data stream timestamp field: failed to parse date field [wrong] with format [strict_date_optional_time_nanos||strict_date_optional_time||epoch_millis]"
        );
    }

    #[test]
    fn past_windows() {
        const H: i64 = 3_600_000;
        const D: i64 = 24 * H;
        // One index [day 10 09:00, day 12); a timestamp on day 7 02:00 jumps back whole days
        // from day 10 09:00.
        let idx = [(10 * D + 9 * H, 12 * D)];
        assert_eq!(past_window_in(&idx, 7 * D + 2 * H, D), Some((6 * D + 9 * H, 7 * D + 9 * H)));
        // Covered: no window.
        assert_eq!(past_window_in(&idx, 11 * D, D), None);
        // A gap of at most 1.3 intervals is filled whole.
        let two = [(D, 2 * D), (3 * D + 6 * H, 5 * D)];
        assert_eq!(past_window_in(&two, 2 * D + H, D), Some((2 * D, 3 * D + 6 * H)));
        // A wider gap: interval windows, clipped at the previous index's end.
        let wide = [(D, 2 * D), (5 * D, 6 * D)];
        assert_eq!(past_window_in(&wide, 2 * D + H, D), Some((2 * D, 3 * D)));
        assert_eq!(past_window_in(&wide, 4 * D, D), Some((4 * D, 5 * D)));
        // Contiguous indices merge into one window.
        let contiguous = [(D, 2 * D), (2 * D, 3 * D)];
        assert_eq!(past_window_in(&contiguous, D / 2, D), Some((0, D)));
    }
}
