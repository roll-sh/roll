//! The rollup indexer (`RollupIndexer`, `IndexerUtils`, `RollupIDGenerator`): one trigger of a
//! started job reads the source indices with a paged composite aggregation, from the stored
//! position up to the start of the current interval (minus `delay`), and indexes one document
//! per bucket into the rollup index.

use super::*;
use config::JobConfig;

/// The composite aggregation's name (`RollupIndexer.AGGREGATION_NAME`).
const AGG: &str = "rollup";

/// `Rollup.CURRENT_ROLLUP_VERSION`.
const VERSION: i64 = 2;

/// What one trigger did.
#[derive(Default)]
pub(super) struct Pass {
    pages: i64,
    processed: i64,
    indexed: i64,
    index_failures: i64,
    searches: i64,
    index_ops: i64,
    search_ms: i64,
    index_ms: i64,
    processing_ms: i64,
    position: Option<Value>,
}

/// One trigger of a started job, recorded with the time of its next cron trigger.
pub(super) async fn trigger(cluster: &Cluster, id: &str) {
    let runs = &cluster.xpack.rollups;
    if !runs.begin(id) {
        return;
    }
    if let Some(job) = cluster.state().metadata.xpack.get(JOBS, id).cloned()
        && state_of(&job) == "started"
    {
        let owner = job.get("user").and_then(internal::principal_of);
        let outcome = match job_config(&job) {
            Some(cfg) => internal::as_principal(owner, run(cluster, &cfg, job.get("position").filter(|p| p.as_object().is_some()).cloned())).await,
            None => Err(EsError::illegal_state(format!("the configuration of rollup job [{id}] can't be read"))),
        };
        // A store refusing the write (a cluster unloading) leaves the trigger due: the
        // scheduler runs it again.
        let _ = record(cluster, id, outcome).await;
    }
    runs.end(id);
}

/// The composite sources in Elasticsearch's order: date_histogram, histograms, terms.
fn sources(cfg: &JobConfig) -> Vec<Value> {
    let mut out = Vec::new();
    let Some(g) = &cfg.groups else { return out };
    let d = &g.date_histogram;
    let mut dh = Map::new();
    dh.insert("field".into(), d.field.clone().into());
    dh.insert(d.kind.key().into(), d.interval.clone().into());
    dh.insert("time_zone".into(), d.time_zone.clone().into());
    let mut one = Map::new();
    one.insert(format!("{}.date_histogram", d.field), obj! { "date_histogram" => Value::Obj(dh) });
    out.push(Value::Obj(one));
    for f in g.histogram.iter().flat_map(|h| h.fields.iter().map(move |f| (f, h.interval))) {
        let mut one = Map::new();
        one.insert(format!("{}.histogram", f.0), obj! { "histogram" => obj! { "field" => f.0.clone(), "interval" => f.1, "missing_bucket" => true } });
        out.push(Value::Obj(one));
    }
    for f in g.terms.iter().flatten() {
        let mut one = Map::new();
        one.insert(format!("{f}.terms"), obj! { "terms" => obj! { "field" => f.clone(), "missing_bucket" => true } });
        out.push(Value::Obj(one));
    }
    out
}

/// The metrics under each bucket (`createAggregationBuilders`): an average is a sum and a
/// value count.
fn metric_aggs(cfg: &JobConfig) -> Map {
    let mut out = Map::new();
    for m in &cfg.metrics {
        let f = &m.field;
        for metric in &m.metrics {
            match metric.as_str() {
                "avg" => {
                    out.insert(format!("{f}.avg.value"), obj! { "sum" => obj! { "field" => f.clone() } });
                    out.insert(format!("{f}.avg._count"), obj! { "value_count" => obj! { "field" => f.clone() } });
                }
                other => {
                    out.insert(format!("{f}.{other}.value"), obj! { other => obj! { "field" => f.clone() } });
                }
            }
        }
    }
    out
}

/// `RollupIDGenerator`: the bucket's key values, `$`-delimited, hashed with MurmurHash3
/// x64-128 (seed 19), as `{job}${base64url(h1 h2)}`.
#[derive(Default)]
struct IdGen(Vec<u8>);

impl IdGen {
    fn push(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
        self.0.push(b'$');
    }

    fn null(&mut self) {
        self.push(b"__NULL_PLACEHOLDER__830f1de2__");
    }

    fn value(&mut self, v: &Value) {
        match v {
            Value::Null => self.null(),
            Value::Str(s) => self.push(s.as_bytes()),
            Value::Num(n) => match n.as_i64() {
                Some(l) => self.push(&l.to_be_bytes()),
                None => self.push(&n.as_f64().to_bits().to_be_bytes()),
            },
            other => self.push(other.to_es_string().as_bytes()),
        }
    }

    fn id(&self, job: &str) -> String {
        use base64::Engine as _;
        let (h1, h2) = search_aggs::util::murmur3_x64_128(&self.0, 19);
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&h1.to_be_bytes());
        bytes[8..].copy_from_slice(&h2.to_be_bytes());
        format!("{job}${}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }
}

/// One rollup document per composite bucket (`IndexerUtils.processBuckets`), with its id.
fn bucket_doc(cfg: &JobConfig, bucket: &Value) -> (String, Value) {
    let count = bucket.get("doc_count").and_then(Value::as_i64).unwrap_or(0);
    let mut doc = Map::new();
    let mut id = IdGen::default();
    let mut keys: Vec<(&String, &Value)> = bucket.get("key").and_then(Value::as_object).map(|m| m.iter().collect()).unwrap_or_default();
    keys.sort_by(|a, b| a.0.cmp(b.0));
    for (k, v) in keys {
        doc.insert(format!("{k}._count"), count.into());
        if k.ends_with(".date_histogram") {
            let d = cfg.groups.as_ref().map(|g| &g.date_histogram);
            doc.insert(format!("{k}.timestamp"), v.clone());
            doc.insert(format!("{k}.interval"), d.map(|d| d.interval.clone()).unwrap_or_default().into());
            doc.insert(format!("{k}.time_zone"), d.map(|d| d.time_zone.clone()).unwrap_or_default().into());
            id.value(v);
        } else if k.ends_with(".histogram") {
            doc.insert(format!("{k}.value"), v.clone());
            let interval = cfg.groups.as_ref().and_then(|g| g.histogram.as_ref()).map_or(0, |h| h.interval);
            doc.insert(format!("{k}.interval"), interval.into());
            match v {
                Value::Num(n) => id.push(&n.as_f64().to_bits().to_be_bytes()),
                _ => id.null(),
            }
        } else {
            doc.insert(format!("{k}.value"), v.clone());
            id.value(v);
        }
    }
    id.push(cfg.id.as_bytes());
    let mut empty_counts = Vec::new();
    if let Some(m) = bucket.as_object() {
        for (name, agg) in m {
            if name == "key" || name == "doc_count" {
                continue;
            }
            let Some(v) = agg.get("value").and_then(Value::as_f64).filter(|v| !v.is_infinite() && !v.is_nan()) else { continue };
            if name.ends_with("._count") && v == 0.0 {
                empty_counts.push(name.clone());
            } else {
                doc.insert(name.clone(), Value::from(v));
            }
        }
    }
    for c in empty_counts {
        doc.shift_remove(&c.replace("_count", "value"));
    }
    doc.insert("_rollup.version".into(), VERSION.into());
    doc.insert("_rollup.id".into(), cfg.id.clone().into());
    (id.id(&cfg.id), Value::Obj(doc))
}

/// A trigger's work: pages of composite buckets from the stored position to the boundary,
/// each bulk-indexed into the rollup index, then a refresh.
async fn run(cluster: &Cluster, cfg: &JobConfig, mut position: Option<Value>) -> EsResult<Pass> {
    let mut pass = Pass::default();
    let now = crate::time::now_millis();
    let mut query = obj! { "match_all" => obj! {} };
    if let Some(g) = &cfg.groups {
        let d = &g.date_histogram;
        let delay = d.delay.as_deref().and_then(|x| crate::units::TimeValue::parse(x, "delay").ok()).map_or(0, |t| t.as_millis());
        let rounding = d.rounding().ok_or_else(|| EsError::illegal_argument(format!("invalid interval [{}] of rollup job [{}]", d.interval, cfg.id)))?;
        let boundary = rounding.round(now - delay);
        let lower = position.as_ref().and_then(|p| p.get(&format!("{}.date_histogram", d.field))).and_then(Value::as_i64).unwrap_or(0);
        let mut range = Map::new();
        range.insert(d.field.clone(), obj! { "gte" => lower, "lt" => boundary, "format" => "epoch_millis" });
        query = obj! { "range" => Value::Obj(range) };
    }
    let uri = format!("/{}/_search?allow_partial_search_results=false", enc(&cfg.indices().join(",")));
    let metrics = metric_aggs(cfg);
    let srcs = sources(cfg);
    loop {
        let mut composite = Map::new();
        composite.insert("size".into(), cfg.page_size.into());
        composite.insert("sources".into(), Value::Arr(srcs.clone()));
        if let Some(p) = &position {
            composite.insert("after".into(), p.clone());
        }
        let mut agg = Map::new();
        agg.insert("composite".into(), Value::Obj(composite));
        if !metrics.is_empty() {
            agg.insert("aggs".into(), Value::Obj(metrics.clone()));
        }
        let mut aggs = Map::new();
        aggs.insert(AGG.into(), Value::Obj(agg));
        let body = obj! { "size" => 0, "track_total_hits" => false, "query" => query.clone(), "aggs" => Value::Obj(aggs) };
        let s0 = std::time::Instant::now();
        let resp = internal::call(cluster, "POST", &uri, Some(&body)).await?;
        pass.search_ms += s0.elapsed().as_millis() as i64;
        pass.searches += 1;
        pass.pages += 1;
        let p0 = std::time::Instant::now();
        let buckets = resp.pointer(&["aggregations", AGG, "buckets"]).and_then(Value::as_array).cloned().unwrap_or_default();
        if buckets.is_empty() {
            pass.processing_ms += p0.elapsed().as_millis() as i64;
            break;
        }
        pass.processed += buckets.iter().filter_map(|b| b.get("doc_count").and_then(Value::as_i64)).sum::<i64>();
        let mut nd = String::new();
        for b in &buckets {
            let (id, doc) = bucket_doc(cfg, b);
            nd.push_str(&obj! { "index" => obj! { "_index" => cfg.rollup_index.clone(), "_id" => id } }.to_json());
            nd.push('\n');
            nd.push_str(&doc.to_json());
            nd.push('\n');
        }
        pass.processing_ms += p0.elapsed().as_millis() as i64;
        let i0 = std::time::Instant::now();
        let out = internal::call_ndjson(cluster, "POST", "/_bulk", nd).await?;
        pass.index_ms += i0.elapsed().as_millis() as i64;
        pass.index_ops += 1;
        for item in out.get("items").and_then(Value::as_array).into_iter().flatten() {
            if item.pointer(&["index", "status"]).and_then(Value::as_i64).is_some_and(|s| s < 300) {
                pass.indexed += 1;
            } else {
                pass.index_failures += 1;
            }
        }
        position = resp.pointer(&["aggregations", AGG, "after_key"]).cloned();
        pass.position = position.clone();
        if position.is_none() {
            break;
        }
    }
    let _ = internal::call(cluster, "POST", &format!("/{}/_refresh", enc(&cfg.rollup_index)), None).await;
    Ok(pass)
}

/// Records a trigger's outcome: the stats, the position, and the next cron trigger while the
/// job is still started (a job stopped or deleted meanwhile stays so).
async fn record(cluster: &Cluster, id: &str, outcome: EsResult<Pass>) -> EsResult<()> {
    let id = id.to_string();
    let end = crate::time::now_millis();
    cluster
        .update(move |st| {
            let Some(Value::Obj(mut j)) = st.metadata.xpack.get(JOBS, &id).cloned() else { return Ok(()) };
            let mut s = j.get("stats").and_then(Value::as_object).cloned().unwrap_or_default();
            let mut add = |k: &str, n: i64| {
                let cur = s.get(k).and_then(Value::as_i64).unwrap_or(0);
                s.insert(k.into(), (cur + n).into());
            };
            add("trigger_count", 1);
            match &outcome {
                Ok(p) => {
                    add("pages_processed", p.pages);
                    add("documents_processed", p.processed);
                    add("rollups_indexed", p.indexed);
                    add("index_failures", p.index_failures);
                    add("index_time_in_ms", p.index_ms);
                    add("index_total", p.index_ops);
                    add("search_time_in_ms", p.search_ms);
                    add("search_total", p.searches);
                    add("processing_time_in_ms", p.processing_ms);
                    add("processing_total", p.pages);
                    if let Some(pos) = &p.position {
                        j.insert("position".into(), pos.clone());
                    }
                }
                Err(_) => add("search_failures", 1),
            }
            j.insert("stats".into(), Value::Obj(s));
            if j.get("state").and_then(Value::as_str) == Some("started") {
                let cron = config_str(&j, "cron");
                match next_cron(&cron, end) {
                    Some(t) => j.insert("next_run".into(), t.into()),
                    None => j.shift_remove("next_run"),
                };
            }
            store::put(st, JOBS, &id, Value::Obj(j), end).map(|_| ())
        })
        .await
}

#[cfg(test)]
mod tests {
    use crate::cluster::{Cluster, ClusterConfig, MemoryStore};
    use crate::json::Value;
    use crate::rest::EsRequest;
    use std::sync::Arc;

    async fn call(c: &Cluster, method: &str, uri: &str, body: &str) -> (u16, Value) {
        let req = if body.is_empty() { EsRequest::new(method, uri) } else { EsRequest::json(method, uri, body.to_string()) };
        let resp = crate::rest::handle(c, req).await;
        (resp.status, crate::json::parse(&resp.body).ok().flatten().unwrap_or_default())
    }

    fn job(c: &Cluster, path: &[&str]) -> Option<Value> {
        c.state().metadata.xpack.get(super::JOBS, "j")?.pointer(path).cloned()
    }

    /// A job reads as its creator: a user's job rolls up only the documents its role's query
    /// lets it read, and a user that can't read an index can't roll it up.
    #[tokio::test]
    async fn jobs_run_as_their_creator() {
        let c = Cluster::open(ClusterConfig::new("c"), Arc::new(MemoryStore::new())).await.unwrap();
        call(&c, "PUT", "/src", r#"{"mappings":{"properties":{"t":{"type":"date"},"p":{"type":"keyword"},"v":{"type":"integer"}}}}"#).await;
        call(&c, "PUT", "/secret", r#"{"mappings":{"properties":{"t":{"type":"date"},"p":{"type":"keyword"},"v":{"type":"integer"}}}}"#).await;
        call(&c, "PUT", "/seed", r#"{"mappings":{"_meta":{"_rollup":{"x":{}}}}}"#).await;
        for (i, p) in ["a", "b", "a"].iter().enumerate() {
            for index in ["src", "secret"] {
                call(&c, "PUT", &format!("/{index}/_doc/{i}?refresh=true"), &format!(r#"{{"t":"2020-01-01T00:1{i}:00Z","p":"{p}","v":{i}}}"#)).await;
            }
        }
        let role = r#"{"cluster":["manage_rollup"],"indices":[{"names":["src"],"privileges":["read","view_index_metadata"],"query":{"term":{"p":"a"}}},{"names":["src_rollup","secret_rollup"],"privileges":["all"]}]}"#;
        assert_eq!(call(&c, "PUT", "/_security/role/roller", role).await.0, 200);
        assert_eq!(call(&c, "PUT", "/_security/user/roller", r#"{"password":"roller-pass-1","roles":["roller"]}"#).await.0, 200);
        let as_roller = |method: &str, uri: &str, body: &str| {
            use base64::Engine;
            let mut req = if body.is_empty() { EsRequest::new(method, uri) } else { EsRequest::json(method, uri, body.to_string()) };
            req.headers.push(("Authorization".into(), format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("roller:roller-pass-1"))));
            crate::rest::handle(&c, req)
        };
        let job = |index: &str| {
            format!(
                r#"{{"index_pattern":"{index}","rollup_index":"{index}_rollup","cron":"0 0 0 1 1 ? 2099","page_size":10,"groups":{{"date_histogram":{{"field":"t","fixed_interval":"1h"}},"terms":{{"fields":["p"]}}}},"metrics":[{{"field":"v","metrics":["max"]}}]}}"#
            )
        };
        assert_eq!(as_roller("PUT", "/_rollup/job/s", &job("secret")).await.status, 403, "the creator can't read secret");
        assert_eq!(as_roller("PUT", "/_rollup/job/j", &job("src")).await.status, 200);
        assert_eq!(as_roller("POST", "/_rollup/job/j/_start", "").await.status, 200);
        super::trigger(&c, "j").await;
        assert_eq!(job_of(&c, "j", &["stats", "rollups_indexed"]).and_then(|v| v.as_i64()), Some(1), "one bucket: (00:00, a)");
        let (_, hits) = call(&c, "POST", "/src_rollup/_search", r#"{"query":{"term":{"p.terms.value":"b"}}}"#).await;
        assert_eq!(hits.pointer(&["hits", "total", "value"]).and_then(Value::as_i64), Some(0), "rolled up a document its creator can't read");
        // `_rollup_search` searches as its caller too, also on live indices.
        let r = as_roller("POST", "/src/_rollup_search", r#"{"size":0,"aggs":{"p":{"terms":{"field":"p"}}}}"#).await;
        let v = crate::json::parse(&r.body).ok().flatten().unwrap_or_default();
        let keys: Vec<String> = v.pointer(&["aggregations", "p", "buckets"]).and_then(Value::as_array).into_iter().flatten().filter_map(|b| b.get("key").map(Value::to_es_string)).collect();
        assert_eq!((r.status, keys), (200, vec!["a".to_string()]), "_rollup_search read documents its caller can't");
    }

    fn job_of(c: &Cluster, id: &str, path: &[&str]) -> Option<Value> {
        c.state().metadata.xpack.get(super::JOBS, id)?.pointer(path).cloned()
    }

    /// A started job runs on the cluster's job scheduler at its cron times, keeps its next
    /// trigger in the stored state (so a sleeping cluster resumes it after loading), and what
    /// it writes answers `_rollup_search` like the source answers `_search`.
    #[tokio::test]
    async fn started_jobs_roll_up_in_the_background_and_resume_after_a_reload() {
        let store = Arc::new(MemoryStore::new());
        let c = Arc::new(Cluster::open(ClusterConfig::new("c"), store.clone()).await.unwrap());
        c.enable_background_tasks();
        call(&c, "PUT", "/src", r#"{"mappings":{"properties":{"t":{"type":"date"},"p":{"type":"keyword"},"v":{"type":"integer"}}}}"#).await;
        call(&c, "PUT", "/seed", r#"{"mappings":{"_meta":{"_rollup":{"x":{}}}}}"#).await;
        let docs = [("2020-01-01T00:10:00Z", "a", 1), ("2020-01-01T00:20:00Z", "a", 3), ("2020-01-01T01:10:00Z", "b", 10), ("2020-01-01T03:10:00Z", "a", 4)];
        for (i, (t, p, v)) in docs.iter().enumerate() {
            call(&c, "PUT", &format!("/src/_doc/{i}?refresh=true"), &format!(r#"{{"t":"{t}","p":"{p}","v":{v}}}"#)).await;
        }
        let cfg = r#"{"index_pattern":"src","rollup_index":"src_rollup","cron":"* * * * * ?","page_size":2,
            "groups":{"date_histogram":{"field":"t","fixed_interval":"1h"},"terms":{"fields":["p"]}},
            "metrics":[{"field":"v","metrics":["max","avg"]}]}"#;
        assert_eq!(call(&c, "PUT", "/_rollup/job/j", cfg).await.0, 200);
        assert_eq!(call(&c, "POST", "/_rollup/job/j/_start", "").await.0, 200);
        let indexed = |c: &Cluster| job(c, &["stats", "rollups_indexed"]).and_then(|v| v.as_i64()).unwrap_or(0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while indexed(&c) < 3 {
            assert!(std::time::Instant::now() < deadline, "the job didn't index its buckets");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let q = r#"{"size":0,"query":{"term":{"p":"a"}},"aggs":{"h":{"date_histogram":{"field":"t","fixed_interval":"1h","min_doc_count":1},"aggs":{"m":{"max":{"field":"v"}},"a":{"avg":{"field":"v"}}}}}}"#;
        let (status, rolled) = call(&c, "POST", "/src_rollup/_rollup_search", q).await;
        assert_eq!(status, 200, "{rolled:?}");
        let (_, live) = call(&c, "POST", "/src/_search", q).await;
        // Sub-aggregations come in the rolled request's order: compare with sorted keys.
        fn sorted(v: &Value) -> Value {
            match v {
                Value::Obj(m) => {
                    let mut keys: Vec<&String> = m.keys().collect();
                    keys.sort();
                    Value::Obj(keys.into_iter().map(|k| (k.clone(), sorted(&m[k]))).collect())
                }
                Value::Arr(a) => Value::Arr(a.iter().map(sorted).collect()),
                other => other.clone(),
            }
        }
        assert_eq!(rolled.get("aggregations").map(|a| sorted(a).to_json()), live.get("aggregations").map(|a| sorted(a).to_json()));

        // The cluster unloads and loads again: still started, and triggered again.
        let triggers = |c: &Cluster| job(c, &["stats", "trigger_count"]).and_then(|v| v.as_i64()).unwrap_or(0);
        drop(c);
        let c = Arc::new(Cluster::open(ClusterConfig::new("c"), store).await.unwrap());
        assert_eq!(job(&c, &["state"]), Some(Value::from("started")));
        let before = triggers(&c);
        c.enable_background_tasks();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while triggers(&c) <= before {
            assert!(std::time::Instant::now() < deadline, "no trigger after the reload");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(call(&c, "POST", "/_rollup/job/j/_stop?wait_for_completion=true", "").await.0, 200);
        assert_eq!(job(&c, &["next_run"]), None);
        assert_eq!(call(&c, "DELETE", "/_rollup/job/j", "").await.0, 200);
    }
}
