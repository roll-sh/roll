//! The data stream lifecycle at work: Elasticsearch's `DataStreamLifecycleService`, run by the
//! cluster's job scheduler ([`crate::opensearch::jobs`]) on Elasticsearch-flavor clusters.
//!
//! A pass goes over every data stream whose data lifecycle or failures lifecycle is enabled,
//! as `DataStreamLifecycleService.run` does: it rolls the write index (and the failure store's)
//! over with the `cluster.lifecycle.default.rollover` conditions (`max_age=auto` derived from
//! the effective retention), then deletes the backing and failure indices whose generation
//! date (`index.lifecycle.origination_date`, else the rollover time, else the creation time)
//! is older than the effective retention. The write indices, indices with
//! `index.lifecycle.skip`, indices an ILM policy manages (`index.lifecycle.prefer_ilm`), time
//! series indices still within their time bounds and indices being downsampled are left
//! alone. Rollovers and deletions go through roll's own `_rollover` and `DELETE` handlers with
//! the cluster's authority (Elasticsearch's data stream lifecycle origin). A failed operation
//! is kept per index in the error store (`dsl.error`: first occurrence, last recorded,
//! message, retry count) that `_lifecycle/explain` and `_lifecycle/stats` print, cleared by
//! the next success. Force merging and downsampling rounds are not run (roll merges segments
//! itself; see docs/search/compatibility.md).
//!
//! **When passes run.** Elasticsearch polls every `data_streams.lifecycle.poll_interval`
//! (5m). A loaded roll cluster polls at that interval only while a write could change an
//! outcome (a size or document-count condition, a recorded error); the time-based work (a
//! write index reaching its rollover `max_age`, an index's retention running out) is due at a
//! known instant ([`next_due`]), which is also when a sleeping cluster is woken. A `max_age`
//! that came due without a rollover (the index is empty: `min_docs=1`) is recorded once
//! (`dsl.checked`), so it doesn't wake the cluster again: only a write, which loads the
//! cluster anyway, can make that rollover happen.

use super::data_streams::{automatic_max_age, effective_retention, failures_effective_retention, failures_lifecycle_of, time_setting};
use crate::cluster::{Cluster, Dialect};
use crate::json::{Map, Value};
use crate::meta::data_streams::DataStream;
use crate::meta::{ClusterState, IndexMeta};
use crate::obj;
use crate::units::TimeValue;
use crate::xpack::{enc, internal, store};

/// The error store: per index name, `{uuid, first, last, message, retry_count}`.
pub const ERRORS: &str = "dsl.error";
/// Per rollover target (`<data stream>` or `<data stream>::failures`): the write index whose
/// `max_age` came due without a rollover, and when that was found.
const CHECKED: &str = "dsl.checked";

const DEFAULT_ROLLOVER: &str = "max_age=auto,max_primary_shard_size=50gb,min_docs=1,max_primary_shard_docs=200000000";
/// `DataStreamLifecycleErrorStore.MAX_ERROR_MESSAGE_LENGTH`.
const MAX_ERROR_MESSAGE_LENGTH: usize = 1000;

/// `data_streams.lifecycle.poll_interval` in ms.
pub fn poll_interval(st: &ClusterState) -> i64 {
    time_setting(st, "data_streams.lifecycle.poll_interval").map(|t| t.as_millis()).filter(|t| *t > 0).unwrap_or(300_000)
}

fn enabled(l: &Value) -> bool {
    l.get("enabled").and_then(Value::as_bool).unwrap_or(true)
}

/// `DataStream.getDataLifecycleForIndex`: the data lifecycle for a backing index, the failures
/// lifecycle for a failure index.
fn lifecycle_for_index(ds: &DataStream, index: &str) -> Option<Value> {
    if ds.indices.iter().any(|i| i == index) {
        ds.lifecycle.clone()
    } else if ds.failure_indices.iter().any(|i| i == index) {
        failures_lifecycle_of(ds)
    } else {
        None
    }
}

/// `DataStream.isIndexManagedByDataStreamLifecycle`: its lifecycle is enabled, and an ILM
/// policy on the index wins unless `index.lifecycle.prefer_ilm` is false.
pub(crate) fn managed_by_dsl(ds: &DataStream, idx: &IndexMeta) -> bool {
    if !lifecycle_for_index(ds, &idx.name).is_some_and(|l| enabled(&l)) {
        return false;
    }
    let has_ilm = idx.settings.get("index.lifecycle.name").is_some_and(|p| !p.is_empty());
    !has_ilm || idx.settings.get("index.lifecycle.prefer_ilm") == Some("false")
}

/// `DataStream.getGenerationLifecycleDate`: none for a write index; else the origination date,
/// else the rollover time, else the creation time.
pub(crate) fn generation_date(ds: &DataStream, idx: &IndexMeta) -> Option<i64> {
    if ds.write_index() == Some(idx.name.as_str()) || ds.failure_write_index() == Some(idx.name.as_str()) {
        return None;
    }
    let origination = idx.settings.get("index.lifecycle.origination_date").and_then(|v| v.parse::<i64>().ok()).filter(|d| *d != -1);
    Some(origination.or_else(|| idx.rollover_info.get(&ds.name).map(|r| r.time)).unwrap_or_else(|| idx.creation_date()))
}

fn skipped(idx: &IndexMeta) -> bool {
    idx.settings.get("index.lifecycle.skip") == Some("true")
}

/// The error recorded for `idx`, as `_lifecycle/explain` prints it (`ErrorEntry`).
pub(crate) fn error_of(st: &ClusterState, idx: &IndexMeta) -> Option<Value> {
    let e = st.metadata.xpack.get(ERRORS, &idx.name)?;
    if e.get("uuid").and_then(Value::as_str) != Some(idx.uuid.as_str()) {
        return None;
    }
    Some(obj! {
        "first_occurrence_millis" => e.get("first").cloned().unwrap_or(Value::from(0)),
        "message" => e.get("message").cloned().unwrap_or(Value::Null),
        "last_recorded_millis" => e.get("last").cloned().unwrap_or(Value::from(0)),
        "retry_count" => e.get("retry_count").cloned().unwrap_or(Value::from(0)),
    })
}

/// A retention that deletes something (a configured `-1` keeps everything).
fn positive(r: Option<(TimeValue, &'static str)>) -> Option<TimeValue> {
    r.map(|(t, _)| t).filter(|t| t.as_millis() > 0)
}

/// The data and failures retentions in effect for `ds` (`None`: keep forever, or no enabled
/// lifecycle).
fn retentions(st: &ClusterState, ds: &DataStream) -> (Option<TimeValue>, Option<TimeValue>) {
    let data = ds.lifecycle.as_ref().filter(|l| enabled(l)).and_then(|l| positive(effective_retention(st, ds, l)));
    let failures = failures_lifecycle_of(ds).filter(enabled).and_then(|l| positive(failures_effective_retention(st, ds, &l)));
    (data, failures)
}

/// `RolloverConfiguration.resolveRolloverConditions`: the `cluster.lifecycle.default.rollover`
/// conditions as a `_rollover` body's `conditions`, `max_age=auto` resolved from the retention;
/// and the `max_age` in ms.
fn rollover_conditions(st: &ClusterState, retention: Option<&TimeValue>) -> (Map, Option<i64>) {
    let setting = st.metadata.cluster_setting("cluster.lifecycle.default.rollover").unwrap_or(DEFAULT_ROLLOVER);
    let mut m = Map::new();
    let mut max_age = None;
    for part in setting.split(',') {
        let Some((k, v)) = part.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        let v = if k == "max_age" && v == "auto" { automatic_max_age(retention) } else { v };
        if k == "max_age" {
            max_age = TimeValue::parse(v, k).ok().map(|t| t.as_millis());
        }
        let value = if k.ends_with("_docs") { v.parse::<i64>().map(Value::from).unwrap_or_else(|_| Value::from(v)) } else { Value::from(v) };
        m.insert(k.to_string(), value);
    }
    (m, max_age)
}

/// One rollover target of a data stream: its data component or its failure store.
struct Target<'a> {
    /// The `_rollover` target and the [`CHECKED`] key.
    name: String,
    write: &'a IndexMeta,
    retention: Option<TimeValue>,
}

fn targets<'a>(st: &'a ClusterState, ds: &DataStream) -> Vec<Target<'a>> {
    let (data, failures) = retentions(st, ds);
    let mut out = Vec::new();
    for (name, write, retention) in [(ds.name.clone(), ds.write_index(), data), (format!("{}::failures", ds.name), ds.failure_write_index(), failures)] {
        let Some(idx) = write.and_then(|w| st.metadata.indices.get(w)) else { continue };
        if skipped(idx) || !managed_by_dsl(ds, idx) {
            continue;
        }
        out.push(Target { name, write: idx, retention });
    }
    out
}

/// When the write index's `max_age` is reached, unless that was already found without a
/// rollover.
fn age_due(st: &ClusterState, t: &Target<'_>) -> Option<i64> {
    let (_, max_age) = rollover_conditions(st, t.retention.as_ref());
    let due = t.write.creation_date().saturating_add(max_age?);
    let checked = st.metadata.xpack.get(CHECKED, &t.name).filter(|c| c.get("index").and_then(Value::as_str) == Some(t.write.name.as_str()));
    match checked.and_then(|c| c.get("at")).and_then(Value::as_i64) {
        Some(at) if at >= due => None,
        _ => Some(due),
    }
}

/// When the index may be deleted: its generation date plus the retention, but not before a
/// time series index's end time.
fn deletion_due(ds: &DataStream, idx: &IndexMeta, retention: &TimeValue) -> Option<i64> {
    let due = generation_date(ds, idx)?.saturating_add(retention.as_millis());
    let end = idx.settings.get("index.time_series.end_time").and_then(|v| crate::meta::index_settings::parse_date_setting(v).ok());
    Some(end.map_or(due, |e| due.max(e)))
}

/// Indices of `ds` whose retention could delete them: managed, not a write index, not skipped,
/// with their retention.
fn retention_candidates<'a>(st: &'a ClusterState, ds: &'a DataStream) -> impl Iterator<Item = (&'a IndexMeta, TimeValue)> + 'a {
    let (data, failures) = retentions(st, ds);
    let backing = ds.indices.iter().filter_map(move |n| Some((n, data?)));
    let failure = ds.failure_indices.iter().filter_map(move |n| Some((n, failures?)));
    backing.chain(failure).filter_map(|(n, r)| st.metadata.indices.get(n).map(|i| (&**i, r))).filter(|(i, _)| !skipped(i) && managed_by_dsl(ds, i))
}

fn active(st: &ClusterState) -> bool {
    st.dialect == Dialect::Elasticsearch && !st.metadata.data_streams.is_empty()
}

/// When the data stream lifecycle next has time-based work (a rollover's `max_age`, a
/// retention running out): what wakes a sleeping cluster. `None` without such work.
pub fn next_due(st: &ClusterState) -> Option<i64> {
    if !active(st) {
        return None;
    }
    let mut due: Option<i64> = None;
    let mut at = |t: Option<i64>| {
        if let Some(t) = t {
            due = Some(due.map_or(t, |d| d.min(t)));
        }
    };
    for ds in st.metadata.data_streams.values() {
        for t in targets(st, ds) {
            at(age_due(st, &t));
        }
        for (idx, retention) in retention_candidates(st, ds) {
            at(deletion_due(ds, idx, &retention));
        }
    }
    due
}

/// Whether a loaded cluster polls at `data_streams.lifecycle.poll_interval`: a write could make
/// a size or document-count condition true, or an operation failed and is retried.
pub fn polls(st: &ClusterState) -> bool {
    active(st) && (st.metadata.xpack.len(ERRORS) > 0 || st.metadata.data_streams.values().any(|ds| !targets(st, ds).is_empty()))
}

/// What one pass decided for a data stream.
#[derive(Default)]
struct Work {
    rollovers: Vec<(String, String, Map)>,
    deletes: Vec<String>,
}

/// One pass over every data stream at `now` (`DataStreamLifecycleService.run`).
pub async fn run(cluster: &Cluster, now: i64) {
    crate::time::at(now, pass(cluster, now)).await
}

async fn pass(cluster: &Cluster, now: i64) {
    let st = cluster.state();
    if !active(&st) && st.metadata.xpack.len(ERRORS) == 0 {
        return;
    }
    let mut work = Work::default();
    let mut checked: Vec<(String, String)> = Vec::new();
    for ds in st.metadata.data_streams.values() {
        for t in targets(&st, ds) {
            let (conditions, max_age) = rollover_conditions(&st, t.retention.as_ref());
            let aged = max_age.is_some_and(|a| now >= t.write.creation_date().saturating_add(a));
            // Without the age condition, only writes can meet a condition: an index that took
            // none since the cluster loaded (its shards aren't open) isn't asked.
            if aged || cluster.engine.loaded(&t.write.uuid).is_some() {
                work.rollovers.push((t.name.clone(), t.write.name.clone(), conditions));
                if aged && age_due(&st, &t).is_some() {
                    checked.push((t.name.clone(), t.write.name.clone()));
                }
            }
        }
        for (idx, retention) in retention_candidates(&st, ds) {
            // An index being downsampled is deleted once that finishes.
            if idx.settings.get("index.downsample.status") == Some("started") {
                continue;
            }
            if deletion_due(ds, idx, &retention).is_some_and(|d| now >= d) {
                work.deletes.push(idx.name.clone());
            }
        }
    }
    drop(st);
    let mut errors: Vec<(String, Option<Value>)> = Vec::new();
    let mut rolled: Vec<String> = Vec::new();
    for (target, write, conditions) in &work.rollovers {
        let body = obj! { "conditions" => Value::Obj(conditions.clone()) };
        match internal::call_system(cluster, "POST", &format!("/{}/_rollover", enc(target)), Some(&body)).await {
            Ok(r) => {
                if r.get("rolled_over").and_then(Value::as_bool) == Some(true) {
                    rolled.push(target.clone());
                }
                errors.push((write.clone(), None));
            }
            // An error only counts while that index is still the write index.
            Err(e) => {
                let ds = target.trim_end_matches("::failures");
                let still = cluster.state().metadata.data_streams.get(ds).is_some_and(|d| d.write_index() == Some(write) || d.failure_write_index() == Some(write));
                if still {
                    errors.push((write.clone(), Some(e.to_value(false))));
                }
            }
        }
    }
    for name in &work.deletes {
        let r = internal::call_system(cluster, "DELETE", &format!("/{}", enc(name)), None).await;
        errors.push((name.clone(), r.err().map(|e| e.to_value(false))));
    }
    checked.retain(|(t, _)| !rolled.contains(t));
    record(cluster, errors, checked, now).await;
}

/// Writes the pass's outcomes: errors recorded or cleared (and those of indices no longer
/// managed dropped), `max_age`s found without a rollover. No write when nothing changed.
async fn record(cluster: &Cluster, errors: Vec<(String, Option<Value>)>, checked: Vec<(String, String)>, now: i64) {
    let st = cluster.state();
    let stale = |st: &ClusterState, name: &str, rec: &Value| match (st.metadata.indices.get(name), st.metadata.data_stream_of(name)) {
        (Some(idx), Some(ds)) => rec.get("uuid").and_then(Value::as_str) != Some(idx.uuid.as_str()) || !managed_by_dsl(ds, idx),
        _ => true,
    };
    let changes = !checked.is_empty()
        || errors.iter().any(|(n, e)| e.is_some() || st.metadata.xpack.get(ERRORS, n).is_some())
        || st.metadata.xpack.iter(ERRORS).any(|(n, r)| stale(&st, n, r))
        || st.metadata.xpack.iter(CHECKED).any(|(t, c)| {
            let ds = st.metadata.data_streams.get(t.trim_end_matches("::failures"));
            ds.is_none_or(|d| c.get("index").and_then(Value::as_str).is_none_or(|i| d.write_index() != Some(i) && d.failure_write_index() != Some(i)))
        });
    drop(st);
    if !changes {
        return;
    }
    let _ = cluster
        .update(|st| {
            for (name, e) in &errors {
                let Some(uuid) = st.metadata.indices.get(name).map(|i| i.uuid.clone()) else {
                    store::remove(st, ERRORS, name);
                    continue;
                };
                let Some(e) = e else {
                    store::remove(st, ERRORS, name);
                    continue;
                };
                let mut message = e.to_json();
                if message.len() > MAX_ERROR_MESSAGE_LENGTH {
                    let mut end = MAX_ERROR_MESSAGE_LENGTH;
                    while !message.is_char_boundary(end) {
                        end -= 1;
                    }
                    message.truncate(end);
                }
                let prev = st.metadata.xpack.get(ERRORS, name).filter(|p| p.get("uuid").and_then(Value::as_str) == Some(uuid.as_str()) && p.get("message").and_then(Value::as_str) == Some(message.as_str()));
                let rec = match prev {
                    Some(p) => {
                        let mut p = p.as_object().cloned().unwrap_or_default();
                        let n = p.get("retry_count").and_then(Value::as_i64).unwrap_or(0) + 1;
                        p.insert("retry_count".into(), Value::from(n));
                        p.insert("last".into(), Value::from(now));
                        Value::Obj(p)
                    }
                    None => obj! { "uuid" => uuid, "first" => now, "last" => now, "message" => message, "retry_count" => 0 },
                };
                store::put(st, ERRORS, name, rec, now)?;
            }
            let gone: Vec<String> = st.metadata.xpack.iter(ERRORS).filter(|(n, r)| stale(st, n, r)).map(|(n, _)| n.clone()).collect();
            for n in gone {
                store::remove(st, ERRORS, &n);
            }
            let old: Vec<String> = st
                .metadata
                .xpack
                .iter(CHECKED)
                .filter(|(t, c)| {
                    let ds = st.metadata.data_streams.get(t.trim_end_matches("::failures"));
                    ds.is_none_or(|d| c.get("index").and_then(Value::as_str).is_none_or(|i| d.write_index() != Some(i) && d.failure_write_index() != Some(i)))
                })
                .map(|(t, _)| t.clone())
                .collect();
            for t in old {
                store::remove(st, CHECKED, &t);
            }
            for (target, write) in &checked {
                store::put(st, CHECKED, target, obj! { "index" => write.clone(), "at" => now }, now)?;
            }
            Ok(())
        })
        .await;
}

#[cfg(test)]
mod tests {
    use crate::cluster::{Cluster, ClusterConfig, MemoryStore};
    use crate::json::Value;
    use crate::rest::EsRequest;
    use std::sync::Arc;

    fn at<'a>(v: &'a Value, path: &[&str]) -> &'a Value {
        static NULL: Value = Value::Null;
        v.pointer(path).unwrap_or(&NULL)
    }

    const DAY: i64 = 24 * 3600 * 1000;

    async fn call(c: &Cluster, method: &str, uri: &str, body: &str) -> (u16, Value) {
        let req = if body.is_empty() { EsRequest::new(method, uri) } else { EsRequest::json(method, uri, body.to_string()) };
        let r = crate::rest::handle(c, req).await;
        (r.status, crate::json::parse(&r.body).ok().flatten().unwrap_or(Value::Null))
    }

    fn backing(c: &Cluster) -> Vec<String> {
        c.state().metadata.data_streams["logs-app"].indices.clone()
    }

    /// The write index rolls over at its `max_age` (`auto`: 1d for a 7d retention) once it
    /// has a document; a backing index goes once its retention has passed since its rollover,
    /// never the write index; the wake-up time follows.
    #[tokio::test]
    async fn rolls_over_and_deletes_past_retention() {
        let c = Arc::new(Cluster::open(ClusterConfig::new("c"), Arc::new(MemoryStore::new())).await.unwrap());
        let t = r#"{"index_patterns":["logs-app*"],"data_stream":{},"template":{"lifecycle":{"data_retention":"7d"}}}"#;
        assert_eq!(call(&c, "PUT", "/_index_template/app", t).await.0, 200);
        assert_eq!(call(&c, "PUT", "/_data_stream/logs-app", "").await.0, 200);
        let first = backing(&c)[0].clone();
        let created = c.state().metadata.indices[&first].creation_date();
        // An empty write index: its max_age (1d) is when the lifecycle has work.
        assert_eq!(super::next_due(&c.state()), Some(created + DAY));
        super::run(&c, created + DAY).await;
        assert_eq!(backing(&c), [first.clone()], "min_docs=1: an empty index isn't rolled over");
        assert_eq!(super::next_due(&c.state()), None, "only a write can roll it over now");
        assert!(super::polls(&c.state()));
        let (s, v) = call(&c, "POST", "/logs-app/_doc?refresh=true", r#"{"@timestamp":"2026-01-01T00:00:00Z"}"#).await;
        assert_eq!(s, 201, "{v}");
        let rolled_at = created + DAY + 1000;
        super::run(&c, rolled_at).await;
        let after = backing(&c);
        assert_eq!(after.len(), 2, "{after:?}");
        let st = c.state();
        let old = &st.metadata.indices[&first];
        assert_eq!(old.rollover_info["logs-app"].time, rolled_at);
        assert_eq!(crate::json::Value::Obj(old.rollover_info["logs-app"].met_conditions.clone()).to_json(), r#"{"max_age":"1d","min_docs":1}"#);
        let new_created = st.metadata.indices[&after[1]].creation_date();
        assert_eq!(new_created, rolled_at, "the new write index is dated the run's time");
        drop(st);
        // Next: the old index's retention (7d after its rollover), before the new one's age.
        assert_eq!(super::next_due(&c.state()), Some((rolled_at + 7 * DAY).min(new_created + DAY)));
        let (_, v) = call(&c, "GET", &format!("/{first}/_lifecycle/explain"), "").await;
        let e = at(&v, &["indices", first.as_str()]);
        assert_eq!((at(e, &["managed_by_lifecycle"]).as_bool(), at(e, &["rollover_date_millis"]).as_i64()), (Some(true), Some(rolled_at)), "{v}");
        super::run(&c, rolled_at + 7 * DAY - 1).await;
        assert!(backing(&c).contains(&first), "not before its retention ran out");
        super::run(&c, rolled_at + 7 * DAY).await;
        let left = backing(&c);
        assert!(!left.contains(&first) && !c.state().metadata.indices.contains_key(&first), "{left:?}");
        assert!(c.state().metadata.indices.contains_key(left.last().unwrap()), "the write index stays");
    }

    /// An index an ILM policy manages (`prefer_ilm` default) and one with
    /// `index.lifecycle.skip` are left alone.
    #[tokio::test]
    async fn skips_indices_it_does_not_manage() {
        let c = Arc::new(Cluster::open(ClusterConfig::new("c"), Arc::new(MemoryStore::new())).await.unwrap());
        let t = r#"{"index_patterns":["logs-app*"],"data_stream":{},"template":{"lifecycle":{"data_retention":"1d"}}}"#;
        assert_eq!(call(&c, "PUT", "/_index_template/app", t).await.0, 200);
        assert_eq!(call(&c, "PUT", "/_data_stream/logs-app", "").await.0, 200);
        assert_eq!(call(&c, "POST", "/logs-app/_rollover", "").await.0, 200);
        assert_eq!(call(&c, "POST", "/logs-app/_rollover", "").await.0, 200);
        let [a, b, _] = <[String; 3]>::try_from(backing(&c)).unwrap();
        assert_eq!(call(&c, "PUT", &format!("/{a}/_settings"), r#"{"index.lifecycle.name":"some-policy"}"#).await.0, 200);
        assert_eq!(call(&c, "PUT", &format!("/{b}/_settings"), r#"{"index.lifecycle.skip":true}"#).await.0, 200);
        let now = crate::time::now_millis() + 30 * DAY;
        super::run(&c, now).await;
        assert_eq!(backing(&c).len(), 3, "{:?}", backing(&c));
        let (_, v) = call(&c, "GET", &format!("/{a}/_lifecycle/explain"), "").await;
        assert_eq!(at(&v, &["indices", a.as_str(), "managed_by_lifecycle"]), &Value::Bool(false), "{v}");
    }
}
