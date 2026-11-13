//! Running transforms in the background: the cluster's job scheduler
//! ([`crate::opensearch::jobs`]) triggers every started transform when it is due, and each
//! trigger's outcome is stored in the transform (`_roll`), so a started transform keeps its
//! schedule across the cluster sleeping and waking.

use super::*;

/// The transforms of a loaded cluster running a trigger right now (their stats say
/// `indexing`), and a count of finished triggers that requests waiting on one watch
/// (`_start` of a continuous transform, `_stop?wait_for_completion`). Everything else about a
/// run is in the transform's stored `_roll` state, so it survives the cluster unloading.
pub struct Runs {
    running: parking_lot::Mutex<std::collections::HashSet<String>>,
    finished: tokio::sync::watch::Sender<u64>,
}

impl Default for Runs {
    fn default() -> Runs {
        Runs { running: Default::default(), finished: tokio::sync::watch::channel(0).0 }
    }
}

impl Runs {
    pub(super) fn is_running(&self, id: &str) -> bool {
        self.running.lock().contains(id)
    }

    /// Claims `id` for a trigger; `false` when one is running already.
    fn begin(&self, id: &str) -> bool {
        self.running.lock().insert(id.to_string())
    }

    fn end(&self, id: &str) {
        self.running.lock().remove(id);
        self.finished.send_modify(|n| *n += 1);
    }

    /// Waits until `done` holds, checked again after every finished trigger, for at most
    /// `timeout`. Returns whether it held.
    pub(super) async fn wait_until(&self, timeout: std::time::Duration, mut done: impl FnMut() -> bool) -> bool {
        let mut rx = self.finished.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if done() {
                return true;
            }
            if !matches!(tokio::time::timeout_at(deadline, rx.changed()).await, Ok(Ok(()))) {
                return done();
            }
        }
    }
}

pub(super) fn next_run(t: &Value) -> Option<i64> {
    t.pointer(&["_roll", "next_run"]).and_then(Value::as_i64)
}

/// When the next started transform is due: the cluster's job scheduler
/// ([`crate::opensearch::jobs`]) sleeps until then.
pub fn next_due(st: &crate::meta::ClusterState) -> Option<i64> {
    st.metadata.xpack.iter(TRANSFORMS).filter(|(_, t)| state_of(t) == "started").filter_map(|(_, t)| next_run(t)).min()
}

/// Triggers every transform that is due (the job scheduler's turn). Returns how many ran.
pub async fn run_due(cluster: &Cluster, now: i64) -> usize {
    let due: Vec<String> = cluster
        .state()
        .metadata
        .xpack
        .iter(TRANSFORMS)
        .filter(|(_, t)| state_of(t) == "started" && next_run(t).is_some_and(|n| n <= now))
        .map(|(id, _)| id.clone())
        .collect();
    for id in &due {
        trigger(cluster, id).await;
    }
    due.len()
}

/// The frequency a transform is triggered at.
pub(super) fn frequency_ms(t: &Value) -> i64 {
    t.get("frequency").and_then(|f| crate::units::TimeValue::parse(&f.to_es_string(), "frequency").ok()).map_or(60_000, |v| v.as_millis())
}

/// One trigger of a started transform: a checkpoint (a batch transform's only one, after
/// which it stops; a continuous one's when its sources changed), recorded with the time of
/// the next trigger.
pub(super) async fn trigger(cluster: &Cluster, id: &str) {
    let runs = &cluster.xpack.transforms;
    if !runs.begin(id) {
        return;
    }
    if let Ok(t) = get_one(&cluster.state(), id)
        && state_of(&t) == "started"
    {
        let outcome = checkpoint(cluster, &t).await;
        // A store refusing the write (a cluster unloading) leaves the trigger due: the
        // scheduler runs it again.
        let _ = record(cluster, id, outcome).await;
    }
    runs.end(id);
}

/// What one trigger did.
#[derive(Default)]
pub(super) struct Pass {
    /// A new checkpoint was written (`false`: a continuous transform found no changes).
    checkpointed: bool,
    /// The sync time field's bound of the checkpoint (continuous transforms).
    upper_bound: Option<i64>,
    created_dest: bool,
    pages: i64,
    processed: i64,
    indexed: i64,
    deleted: i64,
    searches: i64,
    index_ops: i64,
    search_ms: i64,
    index_ms: i64,
    delete_ms: i64,
    duration_ms: i64,
}

/// The source query, limited to sync times in `[from, to)` for a continuous transform.
pub(super) fn query_between(t: &Value, from: Option<i64>, to: Option<i64>) -> Value {
    let base = t.pointer(&["source", "query"]).cloned().unwrap_or_else(|| obj! { "match_all" => obj! {} });
    let Some(field) = t.pointer(&["sync", "time", "field"]).and_then(Value::as_str) else { return base };
    if from.is_none() && to.is_none() {
        return base;
    }
    let mut range = Map::new();
    if let Some(f) = from {
        range.insert("gte".into(), f.into());
    }
    if let Some(t) = to {
        range.insert("lt".into(), t.into());
    }
    range.insert("format".into(), "epoch_millis".into());
    let mut on = Map::new();
    on.insert(field.to_string(), Value::Obj(range));
    obj! { "bool" => obj! { "filter" => crate::arr![base, obj! { "range" => Value::Obj(on) }] } }
}

/// `now - delay`, rounded down to the interval of a date_histogram group on the sync field
/// (so a checkpoint never cuts a bucket in two).
pub(super) fn upper_bound(t: &Value, now: i64) -> i64 {
    let delay = t.pointer(&["sync", "time", "delay"]).and_then(|d| crate::units::TimeValue::parse(&d.to_es_string(), "delay").ok()).map_or(60_000, |v| v.as_millis());
    let bound = now - delay;
    let field = t.pointer(&["sync", "time", "field"]).and_then(Value::as_str);
    let interval = t
        .pointer(&["pivot", "group_by"])
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|g| g.values())
        .filter_map(|g| g.get("date_histogram"))
        .find(|h| h.get("field").and_then(Value::as_str) == field)
        .and_then(|h| match (h.get("fixed_interval"), h.get("calendar_interval").and_then(Value::as_str)) {
            (Some(f), _) => crate::units::TimeValue::parse(&f.to_es_string(), "fixed_interval").ok().map(|v| v.as_millis()),
            (None, Some("1m" | "minute")) => Some(60_000),
            (None, Some("1h" | "hour")) => Some(3_600_000),
            (None, Some("1d" | "day")) => Some(86_400_000),
            _ => None,
        })
        .filter(|i| *i > 0);
    match interval {
        Some(i) => bound.div_euclid(i) * i,
        None => bound,
    }
}

/// Creates the destination index when it doesn't exist, with the mappings Elasticsearch
/// deduces (none at all without `deduce_mappings`) and the configured aliases. Returns whether
/// it created it.
pub(super) async fn ensure_dest(cluster: &Cluster, t: &Value, now: i64) -> EsResult<bool> {
    let dest = dest_index(t);
    if crate::write::write_index_name(&cluster.state(), dest).ok().flatten().is_some() {
        return Ok(false);
    }
    let deduce = deduces_mappings(t);
    let props = if deduce { deduced_mappings(cluster, t).await } else { Map::new() };
    let mut body = generated_dest(t.get("id").and_then(Value::as_str).unwrap_or_default(), now, props);
    if let Value::Obj(m) = &mut body {
        if !deduce && let Some(Value::Obj(mappings)) = m.get_mut("mappings") {
            mappings.shift_remove("properties");
        }
        if let Some(Value::Arr(aliases)) = t.pointer(&["dest", "aliases"]) {
            let names: Map = aliases.iter().filter_map(|a| a.get("alias").and_then(Value::as_str)).map(|n| (n.to_string(), obj! {})).collect();
            m.insert("aliases".into(), Value::Obj(names));
        }
    }
    internal::call(cluster, "PUT", &format!("/{}", enc(dest)), Some(&body)).await?;
    Ok(true)
}

/// One trigger's work: for a continuous transform past its first checkpoint, whether the
/// sources changed since the last checkpoint's bound; then (if so) a checkpoint, which reads
/// every bucket of the sources up to the new bound with a paged composite aggregation and
/// bulk-indexes one document per bucket (ids are stable, so a bucket is overwritten); then
/// the retention policy.
pub(super) async fn checkpoint(cluster: &Cluster, t: &Value) -> EsResult<Pass> {
    let began = std::time::Instant::now();
    let now = crate::time::now_millis();
    let r = roll_state(t);
    let mut pass = Pass::default();
    let continuous = t.get("sync").is_some();
    let last = r.get("checkpoint").and_then(Value::as_i64).unwrap_or(0);
    let from = r.get("from").and_then(Value::as_i64);
    let upper = continuous.then(|| upper_bound(t, now));
    let count_uri = format!("/{}/_count?ignore_unavailable=true&allow_no_indices=true", enc(&source_indices(t).join(",")));
    if continuous && last > 0 {
        let previous = r.get("time_upper_bound").and_then(Value::as_i64);
        let s0 = std::time::Instant::now();
        let changed = internal::call(cluster, "POST", &count_uri, Some(&obj! { "query" => query_between(t, previous, upper) })).await?;
        pass.search_ms += s0.elapsed().as_millis() as i64;
        pass.searches += 1;
        if changed.get("count").and_then(Value::as_i64).unwrap_or(0) == 0 {
            pass.upper_bound = previous;
            pass.duration_ms = began.elapsed().as_millis() as i64;
            return Ok(pass);
        }
    }
    pass.created_dest = ensure_dest(cluster, t, now).await?;
    let dest = dest_index(t).to_string();
    let types = if deduces_mappings(t) { deduced_mappings(cluster, t).await } else { Map::new() };
    let query = query_between(t, from, upper);
    let uri = search_uri(t);
    let bulk_uri = match t.pointer(&["dest", "pipeline"]).and_then(Value::as_str) {
        Some(p) => format!("/_bulk?pipeline={}", enc(p)),
        None => "/_bulk".to_string(),
    };
    let page_size = setting(t, "max_page_search_size").or_else(|| t.pointer(&["pivot", "max_page_search_size"])).and_then(Value::as_i64).unwrap_or(PAGE_SIZE);
    let mut after: Option<Value> = None;
    loop {
        let req = composite_request_over(t, query.clone(), page_size, after.as_ref());
        let s0 = std::time::Instant::now();
        let resp = internal::call(cluster, "POST", &uri, Some(&req)).await?;
        pass.search_ms += s0.elapsed().as_millis() as i64;
        pass.searches += 1;
        pass.pages += 1;
        let buckets = resp.pointer(&["aggregations", "_transform", "buckets"]).and_then(Value::as_array).cloned().unwrap_or_default();
        if buckets.is_empty() {
            break;
        }
        pass.processed += buckets.iter().filter_map(|b| b.get("doc_count").and_then(Value::as_i64)).sum::<i64>();
        let mut nd = String::new();
        for (doc_id, d) in bucket_docs(t, &buckets, &types) {
            nd.push_str(&obj! { "index" => obj! { "_index" => dest.clone(), "_id" => doc_id } }.to_json());
            nd.push('\n');
            nd.push_str(&d.to_json());
            nd.push('\n');
        }
        let i0 = std::time::Instant::now();
        let out = internal::call_ndjson(cluster, "POST", &bulk_uri, nd).await?;
        pass.index_ms += i0.elapsed().as_millis() as i64;
        pass.index_ops += 1;
        pass.indexed += out.get("items").and_then(Value::as_array).map_or(0, |a| a.iter().filter(|i| i.pointer(&["index", "status"]).and_then(Value::as_i64).is_some_and(|s| s < 300)).count() as i64);
        after = resp.pointer(&["aggregations", "_transform", "after_key"]).cloned();
        if after.is_none() || (buckets.len() as i64) < page_size {
            // One more (empty) page confirms the end, as Elasticsearch's indexer does.
            pass.searches += 1;
            pass.pages += 1;
            break;
        }
    }
    let _ = internal::call(cluster, "POST", &format!("/{}/_refresh", enc(&dest)), None).await;
    // The retention policy deletes destination documents whose time is older than max_age.
    if let (Some(field), Some(age)) = (
        t.pointer(&["retention_policy", "time", "field"]).and_then(Value::as_str),
        t.pointer(&["retention_policy", "time", "max_age"]).and_then(|a| crate::units::TimeValue::parse(&a.to_es_string(), "max_age").ok()),
    ) {
        let mut range = Map::new();
        range.insert(field.to_string(), obj! { "lt" => now - age.as_millis(), "format" => "epoch_millis" });
        let d0 = std::time::Instant::now();
        let out = internal::call(cluster, "POST", &format!("/{}/_delete_by_query?refresh=true&conflicts=proceed", enc(&dest)), Some(&obj! { "query" => obj! { "range" => Value::Obj(range) } })).await?;
        pass.delete_ms += d0.elapsed().as_millis() as i64;
        pass.deleted += out.get("deleted").and_then(Value::as_i64).unwrap_or(0);
    }
    pass.checkpointed = true;
    pass.upper_bound = upper;
    pass.duration_ms = began.elapsed().as_millis() as i64;
    Ok(pass)
}

/// `TransformIndexerStats`' exponential averages (a window of 10 checkpoints).
pub(super) fn exp_avg(prev: Option<f64>, x: f64) -> f64 {
    const ALPHA: f64 = 2.0 / 11.0;
    match prev {
        Some(p) => ALPHA * x + (1.0 - ALPHA) * p,
        None => x,
    }
}

/// Records a trigger's outcome: the stats, the checkpoint, and what happens next. A batch
/// transform stops after its checkpoint; a continuous one is due again a `frequency` later. A
/// failure is retried with a backoff up to `num_failure_retries` times (forever when
/// unattended), then the transform fails. A transform stopped or deleted meanwhile stays so.
pub(super) async fn record(cluster: &Cluster, id: &str, outcome: EsResult<Pass>) -> EsResult<()> {
    let id = id.to_string();
    let end = crate::time::now_millis();
    cluster
        .update(move |st| {
            let Some(cur) = st.metadata.xpack.get(TRANSFORMS, &id).cloned() else { return Ok(()) };
            let started = state_of(&cur) == "started";
            let continuous = cur.get("sync").is_some();
            let frequency = frequency_ms(&cur);
            let retries = if unattended(&cur) { -1 } else { setting(&cur, "num_failure_retries").and_then(Value::as_i64).unwrap_or(10) };
            let v = with_roll_state(cur, |r| {
                let mut s = r.get("stats").and_then(Value::as_object).cloned().unwrap_or_default();
                let mut add = |k: &str, n: i64| {
                    let cur = s.get(k).and_then(Value::as_i64).unwrap_or(0);
                    s.insert(k.into(), (cur + n).into());
                };
                add("trigger_count", 1);
                match &outcome {
                    Ok(p) => {
                        add("pages_processed", p.pages);
                        add("documents_processed", p.processed);
                        add("documents_indexed", p.indexed);
                        add("documents_deleted", p.deleted);
                        add("index_time_in_ms", p.index_ms);
                        add("index_total", p.index_ops);
                        add("search_time_in_ms", p.search_ms);
                        add("search_total", p.searches);
                        add("processing_total", p.pages);
                        add("delete_time_in_ms", p.delete_ms);
                        r.insert("last_search_time".into(), end.into());
                        r.shift_remove("failure_count");
                        r.shift_remove("reason");
                        if p.created_dest {
                            r.insert("created_dest".into(), true.into());
                        }
                        if p.checkpointed {
                            let first = r.get("checkpoint").and_then(Value::as_i64).unwrap_or(0) == 0;
                            let mut avg = |k: &str, x: i64| {
                                let prev = if first { None } else { s.get(k).and_then(Value::as_f64) };
                                s.insert(k.into(), Value::from(exp_avg(prev, x as f64)));
                            };
                            avg("exponential_avg_checkpoint_duration_ms", p.duration_ms);
                            avg("exponential_avg_documents_indexed", p.indexed);
                            avg("exponential_avg_documents_processed", p.processed);
                            let cp = r.get("checkpoint").and_then(Value::as_i64).unwrap_or(0) + 1;
                            r.insert("checkpoint".into(), cp.into());
                            r.insert("checkpoint_time".into(), end.into());
                            r.insert("changes_last_detected_at".into(), end.into());
                        }
                        if let Some(b) = p.upper_bound {
                            r.insert("time_upper_bound".into(), b.into());
                        }
                        if started && !continuous {
                            r.insert("state".into(), "stopped".into());
                            r.shift_remove("next_run");
                        } else if started {
                            r.insert("next_run".into(), (end + frequency).into());
                        }
                    }
                    Err(e) => {
                        add("search_failures", 1);
                        let failures = r.get("failure_count").and_then(Value::as_i64).unwrap_or(0) + 1;
                        let reason = e.reason.clone().unwrap_or_default();
                        if started && retries >= 0 && failures > retries {
                            r.insert("state".into(), "failed".into());
                            r.insert("reason".into(), format!("task encountered more than {retries} failures; latest failure: {reason}").into());
                            r.shift_remove("next_run");
                        } else if started {
                            r.insert("failure_count".into(), failures.into());
                            r.insert("reason".into(), reason.into());
                            let backoff = (1_000i64 << failures.min(12)).min(frequency.max(1_000));
                            r.insert("next_run".into(), (end + backoff).into());
                        }
                    }
                }
                r.insert("stats".into(), Value::Obj(s));
            });
            store::put(st, TRANSFORMS, &id, v, end).map(|_| ())
        })
        .await
}

#[cfg(test)]
mod tests {
    use crate::cluster::{Cluster, ClusterConfig, MemoryStore};
    use crate::json::Value;
    use crate::rest::EsRequest;
    use std::sync::Arc;

    async fn call(c: &Cluster, method: &str, uri: &str, body: &str) -> u16 {
        let req = if body.is_empty() { EsRequest::new(method, uri) } else { EsRequest::json(method, uri, body.to_string()) };
        crate::rest::handle(c, req).await.status
    }

    fn roll(c: &Cluster, id: &str, path: &[&str]) -> Option<Value> {
        let st = c.state();
        let mut full = vec!["_roll"];
        full.extend_from_slice(path);
        st.metadata.xpack.get(super::TRANSFORMS, id)?.pointer(&full).cloned()
    }

    async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !cond() {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Transforms run on the cluster's job scheduler: a batch transform's checkpoint after
    /// `_start` answers, a continuous one's every `frequency`. A started transform's schedule
    /// is in the stored state, so it resumes when the cluster loads again (a sleeping cluster).
    #[tokio::test]
    async fn started_transforms_run_in_the_background_and_resume_after_a_reload() {
        let store = Arc::new(MemoryStore::new());
        let c = Arc::new(Cluster::open(ClusterConfig::new("c"), store.clone()).await.unwrap());
        c.enable_background_tasks();
        assert_eq!(call(&c, "PUT", "/src", r#"{"mappings":{"properties":{"t":{"type":"date"},"a":{"type":"keyword"}}}}"#).await, 200);
        assert_eq!(call(&c, "PUT", "/src/_doc/1?refresh=true", r#"{"t":"2020-01-01T00:00:00Z","a":"x"}"#).await, 201);
        let pivot = r#""pivot":{"group_by":{"a":{"terms":{"field":"a"}}},"aggs":{"n":{"value_count":{"field":"a"}}}}"#;
        assert_eq!(call(&c, "PUT", "/_transform/batch", &format!(r#"{{"source":{{"index":"src"}},"dest":{{"index":"batch-dest"}},{pivot}}}"#)).await, 200);
        assert_eq!(call(&c, "POST", "/_transform/batch/_start", "").await, 200);
        eventually("the batch transform to stop", || roll(&c, "batch", &["state"]) == Some(Value::from("stopped"))).await;
        assert_eq!(roll(&c, "batch", &["checkpoint"]), Some(Value::from(1)));
        assert_eq!(roll(&c, "batch", &["stats", "documents_indexed"]), Some(Value::from(1)));

        let continuous = format!(r#"{{"source":{{"index":"src"}},"dest":{{"index":"cont-dest"}},"frequency":"1s","sync":{{"time":{{"field":"t","delay":"1s"}}}},{pivot}}}"#);
        assert_eq!(call(&c, "PUT", "/_transform/cont", &continuous).await, 200);
        // A continuous transform's `_start` answers after its first checkpoint.
        assert_eq!(call(&c, "POST", "/_transform/cont/_start", "").await, 200);
        assert_eq!(roll(&c, "cont", &["checkpoint"]), Some(Value::from(1)));
        let triggers = |c: &Cluster| roll(c, "cont", &["stats", "trigger_count"]).and_then(|v| v.as_i64()).unwrap_or(0);
        eventually("a second trigger", || triggers(&c) >= 2).await;

        // The cluster unloads and loads again: still started, and triggered again once the
        // cluster may run background work.
        drop(c);
        let c = Arc::new(Cluster::open(ClusterConfig::new("c"), store).await.unwrap());
        assert_eq!(roll(&c, "cont", &["state"]), Some(Value::from("started")));
        let before = triggers(&c);
        c.enable_background_tasks();
        eventually("a trigger after the reload", || triggers(&c) > before).await;
        assert_eq!(call(&c, "POST", "/_transform/cont/_stop?wait_for_completion=true", "").await, 200);
        assert_eq!(roll(&c, "cont", &["state"]), Some(Value::from("stopped")));
        assert_eq!(roll(&c, "cont", &["next_run"]), None);
    }
}
