//! Rollup jobs (`_rollup/job/...`), their capabilities (`_rollup/data`, `{index}/_rollup/data`)
//! and `_rollup_search`, as Elasticsearch 9.5.5 runs them (deprecated there, but working).
//!
//! **Jobs** are stored in the cluster metadata (family [`JOBS`]): the configuration, the
//! indexer state (`stopped`, `started`), the composite position the indexer resumes from, the
//! stats, and while started the time of the next cron trigger. Elasticsearch keeps them as
//! persistent tasks; like those, a job outlives its rollup index, and the rollup index's
//! mapping `_meta._rollup` also holds every job's configuration, which is what the caps APIs
//! and `_rollup_search` read (so they work on rollup indices restored or created by hand).
//!
//! `PUT` checks the configuration ([`config`]), refuses a first job in a cluster with no rollup
//! index at all (rollup is deprecated), checks the fields against `_field_caps` of the source
//! indices, then creates the rollup index (its `_meta` and dynamic templates) or adds the job
//! to an existing rollup index's `_meta`.
//!
//! **The indexer** ([`indexer`]) runs as the job's creator (kept with the job, as a watch keeps
//! its user; [`internal::as_principal`]), so it reads only the documents and fields that user
//! may read and writes only where it may write. It runs on the cluster's job scheduler
//! ([`crate::opensearch::jobs`]): a started job's next cron time (Quartz syntax, UTC) is kept
//! in its stored state, so a sleeping cluster is woken for it. Each trigger reads the source
//! indices with a paged composite aggregation from the stored position up to the current
//! interval's start (minus `delay`), writes one document per bucket (Elasticsearch's field
//! layout and `RollupIDGenerator` ids, so a re-run overwrites) and refreshes the rollup index.
//!
//! **`_rollup_search`** ([`search`]) splits the target into rollup and live indices, picks the
//! best jobs for each aggregation, rewrites aggregations and query to the rolled fields, runs
//! one search per job through the core search API and unrolls the results (doc counts from
//! the `_count` sums, averages from sums and counts), merged with the live indices' result.

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::{enc, internal, key_loc, store};

mod config;
mod indexer;
mod search;

use config::JobConfig;
pub use search::rollup_search;

/// The stored jobs.
pub const JOBS: &str = "rollup.job";

const DEPRECATION: &str = "The rollup functionality will be removed in Elasticsearch 10.0. See docs for more information.";

/// The rollups of a loaded cluster running a trigger right now, and a count of finished
/// triggers that `_stop?wait_for_completion` watches.
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
    fn is_running(&self, id: &str) -> bool {
        self.running.lock().contains(id)
    }

    fn begin(&self, id: &str) -> bool {
        self.running.lock().insert(id.to_string())
    }

    fn end(&self, id: &str) {
        self.running.lock().remove(id);
        self.finished.send_modify(|n| *n += 1);
    }

    async fn wait_idle(&self, id: &str, timeout: std::time::Duration) -> bool {
        let mut rx = self.finished.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if !self.is_running(id) {
                return true;
            }
            if !matches!(tokio::time::timeout_at(deadline, rx.changed()).await, Ok(Ok(()))) {
                return !self.is_running(id);
            }
        }
    }
}

fn job_config(job: &Value) -> Option<JobConfig> {
    job.get("config").and_then(JobConfig::from_meta)
}

fn state_of(job: &Value) -> &str {
    job.get("state").and_then(Value::as_str).unwrap_or("stopped")
}

/// The indexer state the APIs show: a started job running a trigger is `indexing`, a stopped
/// one finishing its trigger `stopping`.
fn shown_state(cluster: &Cluster, id: &str, job: &Value) -> &'static str {
    let running = cluster.xpack.rollups.is_running(id);
    match (state_of(job), running) {
        ("started", true) => "indexing",
        ("started", false) => "started",
        (_, true) => "stopping",
        _ => "stopped",
    }
}

fn next_cron(cron: &str, after: i64) -> Option<i64> {
    crate::xpack::watcher::schedule::next_cron(cron, &crate::time::Zone::UTC, after)
}

/// When the next started job's cron fires: the cluster's job scheduler sleeps until then.
pub fn next_due(st: &crate::meta::ClusterState) -> Option<i64> {
    st.metadata.xpack.iter(JOBS).filter(|(_, j)| state_of(j) == "started").filter_map(|(_, j)| j.get("next_run").and_then(Value::as_i64)).min()
}

/// Triggers every started job that is due (the job scheduler's turn). Returns how many ran.
pub async fn run_due(cluster: &Cluster, now: i64) -> usize {
    let due: Vec<String> = cluster
        .state()
        .metadata
        .xpack
        .iter(JOBS)
        .filter(|(_, j)| state_of(j) == "started" && j.get("next_run").and_then(Value::as_i64).is_some_and(|n| n <= now))
        .map(|(id, _)| id.clone())
        .collect();
    for id in &due {
        indexer::trigger(cluster, id).await;
    }
    due.len()
}

fn not_found(id: &str) -> EsError {
    EsError::resource_not_found(format!("Task for Rollup Job [{id}] not found"))
}

/// Whether any index's mapping has a `_meta._rollup` (`hasRollupIndices`).
fn has_rollup_indices(st: &crate::meta::ClusterState) -> bool {
    st.metadata.indices.values().any(|idx| idx.mapping.meta.as_ref().is_some_and(|m| m.contains_key("_rollup")))
}

/// `PUT _rollup/job/{id}`.
pub async fn put(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    let body = ctx.body_map()?.ok_or_else(|| EsError::parse("request body is required"))?;
    let loc = |k: &str| key_loc(ctx, k);
    let cfg = JobConfig::parse(&body, Some(&id), &loc)?;
    ctx.warn(DEPRECATION);
    if let Some(g) = &cfg.groups
        && let Some(modern) = config::deprecated_zone(&g.date_histogram.time_zone)
    {
        let tz = &g.date_histogram.time_zone;
        ctx.warn(format!("Creating Rollup job [{}] with timezone [{tz}], but [{tz}] has been deprecated by the IANA.  Use [{modern}] instead.", cfg.id));
    }
    let st = ctx.state();
    if st.metadata.xpack.len(JOBS) == 0 && !has_rollup_indices(&st) {
        return Err(EsError::illegal_argument("new rollup jobs are not allowed in clusters that don't have any rollup usage, since rollup has been deprecated"));
    }
    drop(st);
    let fields = cfg.all_fields();
    let uri = format!("/{}/_field_caps?fields={}", enc(&cfg.indices().join(",")), enc(&fields.join(",")));
    // As the caller: a job reads only what its creator may read (Elasticsearch runs the field
    // caps in the request's context, and the indexer with the creator's stored headers).
    let caps = internal::as_caller(internal::Caller::of(ctx), internal::call(ctx.cluster, "GET", &uri, None)).await?;
    let errors = cfg.validate_mappings(caps.get("fields").and_then(Value::as_object).unwrap_or(&Map::new()));
    if !errors.is_empty() {
        let mut e = EsError::action_request_validation(&errors);
        e.kind = "rollup_action_request_validation_exception".into();
        return Err(e);
    }
    create_or_update_index(ctx.cluster, &cfg).await?;
    let user = internal::stored_user(crate::xpack::security::principal(ctx));
    let job_id = cfg.id.clone();
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            if st.metadata.xpack.get(JOBS, &job_id).is_some() {
                return Err(EsError::new(409, "status_exception", format!("Cannot create job [{job_id}] because it has already been created (task exists)")));
            }
            let task_id = st.metadata.xpack.iter(JOBS).filter_map(|(_, j)| j.get("task_id").and_then(Value::as_i64)).max().unwrap_or(0) + 1;
            let mut job = obj! { "config" => cfg.to_value(), "task_id" => task_id, "state" => "stopped" };
            if let (Some(u), Value::Obj(m)) = (user, &mut job) {
                m.insert("user".into(), u);
            }
            store::put(st, JOBS, &job_id, job, now).map(|_| ())
        })
        .await?;
    Reply::acknowledged()
}

/// The rollup index's mappings: the job in `_meta._rollup`, strings as keywords and the date
/// histogram timestamps as dates (`TransportPutRollupJobAction.createMappings`).
fn rollup_mappings(cfg: &JobConfig) -> Value {
    let mut jobs = Map::new();
    jobs.insert(cfg.id.clone(), cfg.to_value());
    obj! {
        "_meta" => obj! { "rollup-version" => "", "_rollup" => Value::Obj(jobs) },
        "dynamic_templates" => crate::arr![
            obj! { "strings" => obj! { "match_mapping_type" => "string", "mapping" => obj! { "type" => "keyword" } } },
            obj! { "date_histograms" => obj! { "path_match" => "*.date_histogram.timestamp", "mapping" => obj! { "type" => "date" } } },
        ],
    }
}

/// Creates the rollup index, or adds the job to an existing rollup index's `_meta`.
async fn create_or_update_index(cluster: &Cluster, cfg: &JobConfig) -> EsResult<()> {
    let index = &cfg.rollup_index;
    let id = &cfg.id;
    match internal::call(cluster, "PUT", &format!("/{}", enc(index)), Some(&obj! { "mappings" => rollup_mappings(cfg) })).await {
        Ok(_) => return Ok(()),
        Err(e) if e.kind == "resource_already_exists_exception" => {}
        Err(e) => return Err(EsError::java(500, "runtime_exception", format!("Could not create index for rollup job [{id}]")).caused_by(e)),
    }
    let st = cluster.state();
    let meta = st.metadata.indices.get(index).and_then(|idx| idx.mapping.meta.clone());
    let Some(mut meta) = meta else {
        return Err(EsError::java(
            500,
            "runtime_exception",
            format!("Rollup data cannot be added to existing indices that contain non-rollup data (expected to find _meta key in mapping of rollup index [{index}] but not found)."),
        ));
    };
    let Some(Value::Obj(mut jobs)) = meta.get("_rollup").cloned() else {
        return Err(EsError::java(
            500,
            "runtime_exception",
            format!("Rollup data cannot be added to existing indices that contain non-rollup data (expected to find rollup meta key [_rollup] in mapping of rollup index [{index}] but not found)."),
        ));
    };
    if jobs.get(id).is_some_and(|v| !v.is_null()) {
        return Err(EsError::new(409, "status_exception", format!("Cannot create rollup job [{id}] because job was previously created (existing metadata).")));
    }
    jobs.insert(id.clone(), cfg.to_value());
    meta.insert("_rollup".into(), Value::Obj(jobs));
    internal::call(cluster, "PUT", &format!("/{}/_mapping", enc(index)), Some(&obj! { "_meta" => Value::Obj(meta) })).await?;
    Ok(())
}

/// The ids a get/caps request names: everything for none, `_all` or `*`.
fn is_all(id: Option<&str>) -> bool {
    matches!(id, None | Some("") | Some("_all") | Some("*"))
}

fn stats_value(job: &Value) -> Value {
    let s = job.get("stats");
    let n = |k: &str| s.and_then(|s| s.get(k)).and_then(Value::as_i64).unwrap_or(0);
    let mut m = Map::new();
    for k in [
        "pages_processed",
        "documents_processed",
        "rollups_indexed",
        "trigger_count",
        "index_time_in_ms",
        "index_total",
        "index_failures",
        "search_time_in_ms",
        "search_total",
        "search_failures",
        "processing_time_in_ms",
        "processing_total",
    ] {
        m.insert(k.into(), n(k).into());
    }
    Value::Obj(m)
}

/// `GET _rollup/job/{id}`.
pub fn get(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.warn(DEPRECATION);
    let id = ctx.path_param("id");
    let st = ctx.state();
    let mut jobs = Vec::new();
    for (job_id, job) in st.metadata.xpack.iter(JOBS) {
        if !is_all(id) && id != Some(job_id.as_str()) {
            continue;
        }
        let mut status = Map::new();
        status.insert("job_state".into(), shown_state(ctx.cluster, job_id, job).into());
        if let Some(p) = job.get("position").filter(|p| !p.is_null()) {
            status.insert("current_position".into(), p.clone());
        }
        jobs.push(obj! { "config" => job.get("config").cloned().unwrap_or_default(), "status" => Value::Obj(status), "stats" => stats_value(job) });
    }
    Reply::ok(obj! { "jobs" => Value::Arr(jobs) })
}

fn config_str(job: &Map, key: &str) -> String {
    job.get("config").and_then(|c| c.get(key)).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// `POST _rollup/job/{id}/_start`.
pub async fn start(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.warn(DEPRECATION);
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    let job = ctx.state().metadata.xpack.get(JOBS, &id).cloned().ok_or_else(|| not_found(&id))?;
    match shown_state(ctx.cluster, &id, &job) {
        "started" | "indexing" => return Reply::ok(obj! { "started" => true }),
        "stopped" => {}
        other => {
            return Err(EsError::new(500, "exception", format!("Cannot start task for Rollup Job [{id}] because state was [{}]", other.to_uppercase())));
        }
    }
    let now = ctx.now_ms;
    let key = id.clone();
    ctx.cluster
        .update(move |st| {
            let Some(Value::Obj(mut j)) = st.metadata.xpack.get(JOBS, &key).cloned() else { return Err(not_found(&key)) };
            let cron = config_str(&j, "cron");
            j.insert("state".into(), "started".into());
            match next_cron(&cron, now) {
                Some(t) => j.insert("next_run".into(), t.into()),
                None => j.shift_remove("next_run"),
            };
            store::put(st, JOBS, &key, Value::Obj(j), now).map(|_| ())
        })
        .await?;
    Reply::ok(obj! { "started" => true })
}

/// `POST _rollup/job/{id}/_stop`: a running trigger finishes its page and stops
/// (`wait_for_completion` waits for that, up to `timeout`).
pub async fn stop(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.warn(DEPRECATION);
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    let timeout = ctx.time_param("timeout", crate::units::TimeValue::seconds(30))?;
    let wait = ctx.bool_param("wait_for_completion", false)?;
    if ctx.state().metadata.xpack.get(JOBS, &id).is_none() {
        return Err(not_found(&id));
    }
    let now = ctx.now_ms;
    let key = id.clone();
    ctx.cluster
        .update(move |st| {
            let Some(Value::Obj(mut j)) = st.metadata.xpack.get(JOBS, &key).cloned() else { return Err(not_found(&key)) };
            if j.get("state").and_then(Value::as_str) == Some("stopped") && !j.contains_key("next_run") {
                return Ok(());
            }
            j.insert("state".into(), "stopped".into());
            j.shift_remove("next_run");
            store::put(st, JOBS, &key, Value::Obj(j), now).map(|_| ())
        })
        .await?;
    if wait && !ctx.cluster.xpack.rollups.wait_idle(&id, std::time::Duration::from_millis(timeout.as_millis().max(0) as u64)).await {
        return Err(EsError::timeout(format!(
            "Timed out after [{}] while waiting for rollup job [{id}] to stop. State was [STOPPING]",
            ctx.query("timeout").unwrap_or("30s")
        )));
    }
    Reply::ok(obj! { "stopped" => true })
}

/// `DELETE _rollup/job/{id}`: only a stopped job (the rollup index and its `_meta` stay).
pub async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.warn(DEPRECATION);
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    let job = ctx.state().metadata.xpack.get(JOBS, &id).cloned().ok_or_else(|| EsError::resource_not_found(format!("the task with id [{id}] doesn't exist")))?;
    let state = shown_state(ctx.cluster, &id, &job);
    if state != "stopped" {
        let failure = obj! {
            "task_id" => job.get("task_id").and_then(Value::as_i64).unwrap_or(0),
            "node_id" => ctx.cluster.node.id.clone(),
            "status" => "INTERNAL_SERVER_ERROR",
            "reason" => obj! {
                "type" => "illegal_state_exception",
                "reason" => format!("Could not delete job [{id}] because indexer state is [{}].  Job must be [STOPPED] before deletion.", state.to_uppercase()),
            },
        };
        return Ok(Reply::Body(500, obj! { "task_failures" => crate::arr![failure], "acknowledged" => false }));
    }
    let key = id.clone();
    ctx.cluster
        .update(move |st| {
            store::remove(st, JOBS, &key);
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

/// `{name: {rollup_jobs: [caps by job id]}}`.
fn caps_by(groups: std::collections::BTreeMap<String, Vec<config::JobCaps>>) -> Value {
    let mut out = Map::new();
    for (name, mut caps) in groups {
        caps.sort_by(|a, b| a.job_id.cmp(&b.job_id));
        out.insert(name, obj! { "rollup_jobs" => Value::Arr(caps.iter().map(config::JobCaps::to_value).collect()) });
    }
    Value::Obj(out)
}

/// `GET _rollup/data/{id}`: the jobs whose index pattern is `id` (every job for `_all`), by
/// index pattern.
pub fn caps(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.warn(DEPRECATION);
    let id = ctx.path_param("id");
    let all = is_all(id);
    let mut groups: std::collections::BTreeMap<String, Vec<config::JobCaps>> = Default::default();
    for idx in ctx.state().metadata.indices.values() {
        for job in config::index_jobs(idx) {
            let cap = job.caps();
            if all || Some(cap.index_pattern.as_str()) == id {
                groups.entry(cap.index_pattern.clone()).or_default().push(cap);
            }
        }
    }
    Reply::ok(caps_by(groups))
}

/// `GET {index}/_rollup/data`: the jobs of the rollup indices `index` resolves to, by rollup
/// index.
pub fn index_caps(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.warn(DEPRECATION);
    let st = ctx.state();
    let exprs: Vec<String> = ctx.path_param("index").unwrap_or("_all").split(',').map(str::to_string).collect();
    let opts = crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED.with_params(|k| ctx.query(k).map(str::to_string))?;
    let names = crate::meta::resolve::resolve(&st, &exprs, &opts, ctx.now_ms)?;
    let mut groups: std::collections::BTreeMap<String, Vec<config::JobCaps>> = Default::default();
    for name in &names {
        let Some(idx) = st.metadata.indices.get(name) else { continue };
        for job in config::index_jobs(idx) {
            let cap = job.caps();
            groups.entry(cap.rollup_index.clone()).or_default().push(cap);
        }
    }
    Reply::ok(caps_by(groups))
}
