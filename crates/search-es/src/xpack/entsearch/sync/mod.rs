//! Running connector syncs: roll is the connector service for the Elastic-managed connectors of
//! the services it implements ([`config::SERVICES`]), as Elastic Cloud is for its native
//! connectors (docs/search/architecture.md, "Connectors").
//!
//! The cluster's job scheduler ([`crate::opensearch::jobs`]) asks [`next_due`] and calls
//! [`run_due`], which marks jobs a dead node left behind, validates edited filtering drafts,
//! creates the jobs schedules make, and starts pending jobs: each as a task of its own, one per
//! connector at a time, that claims the job, reads its source a page at a time into a [`Sink`]
//! (sync rules, bulk indexing, throttling, heartbeats, cancel), deletes what a full sync didn't
//! see, and records the outcome on the job and the connector.

pub mod config;
pub mod extract;
pub mod postgres;
pub mod rules;
pub mod s3;

use super::connectors::{CONNECTORS, SYNC_JOBS};
use super::now_nanos_string;
use crate::cluster::Cluster;
use crate::json::{Map, Value};
use crate::obj;
use crate::xpack::{internal, store};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The `worker_hostname` of jobs roll claims.
pub const WORKER: &str = "roll";
/// roll's own record of a connector it runs (who configured it, when its schedules last fired),
/// kept apart so the connector document keeps Elasticsearch's shape.
pub const RECORDS: &str = "connector.roll";
/// Node settings.
pub const ALLOW_PRIVATE: &str = "roll.connectors.allow_private_hosts";
pub const MAX_DOCS_PER_SECOND: &str = "roll.connectors.max_docs_per_second";
pub const MAX_BYTES_PER_SECOND: &str = "roll.connectors.max_bytes_per_second";

const HEARTBEAT: Duration = Duration::from_secs(10);
/// A job roll claimed that hasn't checked in for this long belongs to a node that went away.
const STALE_MS: i64 = 5 * 60_000;
/// Syncs running at once on one cluster.
const MAX_RUNNING: usize = 2;
/// Syncs running at once on this node (every cluster): each holds a page and, in a full sync,
/// up to [`MAX_SEEN`] ids' hashes.
const NODE_MAX_RUNNING: usize = 4;
static NODE_SYNCS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(NODE_MAX_RUNNING);
/// How often a pending job is looked at again while the node's syncs are all taken (another
/// cluster's sync ending doesn't wake this cluster's scheduler).
const NODE_FULL_RETRY_MS: i64 = 5_000;
/// Documents a full sync remembers seeing (8 bytes each).
const MAX_SEEN: usize = 20_000_000;
/// A page of a source: rows or objects, and bytes.
pub const PAGE_DOCS: usize = 500;
pub const PAGE_BYTES: usize = 20 << 20;
/// A row or an object's text larger than this isn't indexed (Elastic's extraction limit).
pub const DOC_BYTES_MAX: usize = 10 << 20;

/// The connectors of a loaded cluster syncing right now.
#[derive(Default)]
pub struct Syncs {
    running: parking_lot::Mutex<HashSet<String>>,
}

impl Syncs {
    /// Whether a sync is running (the host keeps the cluster loaded meanwhile).
    pub fn busy(&self) -> bool {
        !self.running.lock().is_empty()
    }

    fn is_running(&self, connector: &str) -> bool {
        self.running.lock().contains(connector)
    }

    fn begin(&self, connector: &str) -> bool {
        let mut r = self.running.lock();
        r.len() < MAX_RUNNING && r.insert(connector.to_string())
    }

    /// Whether this cluster has a slot free.
    fn has_room(&self) -> bool {
        self.running.lock().len() < MAX_RUNNING
    }

    fn end(&self, connector: &str) {
        self.running.lock().remove(connector);
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn connector_of(j: &Value) -> &str {
    j.pointer(&["connector", "id"]).and_then(Value::as_str).unwrap_or_default()
}

/// An `Instant` string of these documents as Unix ms.
fn instant_ms(v: Option<&Value>) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(v?.as_str()?).ok().map(|d| d.timestamp_millis())
}

fn active(status: &str) -> bool {
    matches!(status, "in_progress" | "canceling")
}

/// Connectors with a job in progress.
fn busy_connectors(st: &crate::meta::ClusterState) -> HashSet<String> {
    st.metadata.xpack.iter(SYNC_JOBS).filter(|(_, j)| active(str_of(j, "status"))).map(|(_, j)| connector_of(j).to_string()).collect()
}

fn anchor(st: &crate::meta::ClusterState, connector: &str, kind: &str) -> Option<i64> {
    st.metadata.xpack.get(RECORDS, connector)?.pointer(&["anchors", kind]).and_then(Value::as_i64)
}

/// The schedules a connector has on (`full`, `incremental`) and their cron expressions.
fn schedules(c: &Value) -> Vec<(&'static str, String)> {
    if matches!(str_of(c, "status"), "created" | "needs_configuration") {
        return Vec::new();
    }
    ["full", "incremental"]
        .into_iter()
        .filter_map(|k| {
            let s = c.pointer(&["scheduling", k])?;
            (s.get("enabled").and_then(Value::as_bool) == Some(true)).then(|| (k, str_of(s, "interval").to_string()))
        })
        .collect()
}

fn next_fire(cron: &str, after: i64) -> Option<i64> {
    crate::xpack::watcher::schedule::next_cron(cron, &crate::time::Zone::UTC, after)
}

fn draft_edited(c: &Value) -> bool {
    c.get("filtering").and_then(Value::as_array).and_then(|f| f.first()).and_then(|f| f.pointer(&["draft", "validation", "state"])).and_then(Value::as_str) == Some("edited")
}

/// When the scheduler must run [`run_due`] next (Unix ms; `0`: now). `syncs` is the loaded
/// cluster's running syncs (`None` for an unloaded one, which runs none): a pending job isn't due
/// while every slot is taken, since a slot freeing wakes the scheduler.
pub fn next_due(st: &crate::meta::ClusterState, syncs: Option<&Syncs>) -> Option<i64> {
    let mut due: Option<i64> = None;
    let mut at = |t: i64| due = Some(due.map_or(t, |d| d.min(t)));
    let busy = busy_connectors(st);
    let mut runnable = HashSet::new();
    for (id, c) in st.metadata.xpack.iter(CONNECTORS) {
        if !config::runs(c) {
            continue;
        }
        runnable.insert(id.as_str());
        if draft_edited(c) {
            at(0);
        }
        for (kind, cron) in schedules(c) {
            match anchor(st, id, kind) {
                None => at(0),
                Some(a) => {
                    if let Some(t) = next_fire(&cron, a) {
                        at(t);
                    }
                }
            }
        }
    }
    for (_, j) in st.metadata.xpack.iter(SYNC_JOBS) {
        let cid = connector_of(j);
        match str_of(j, "status") {
            "pending" if runnable.contains(cid) && !busy.contains(cid) && syncs.is_none_or(|s| !s.is_running(cid)) => match syncs {
                Some(s) if !s.has_room() => {}
                Some(_) if NODE_SYNCS.available_permits() == 0 => at(crate::time::now_millis() + NODE_FULL_RETRY_MS),
                _ => at(0),
            },
            s if active(s) && str_of(j, "worker_hostname") == WORKER => at(instant_ms(j.get("last_seen")).unwrap_or(0) + STALE_MS),
            _ => {}
        }
    }
    due
}

/// The scheduler's turn: settles what is due and starts pending syncs.
pub async fn run_due(cluster: &Cluster, now: i64) {
    let Some(handle) = cluster.handle() else { return };
    let syncs = &cluster.xpack.connector_syncs;
    let st = cluster.state();
    // Jobs a node that went away left in progress.
    let orphans: Vec<String> = st
        .metadata
        .xpack
        .iter(SYNC_JOBS)
        .filter(|(_, j)| active(str_of(j, "status")) && str_of(j, "worker_hostname") == WORKER)
        .filter(|(_, j)| !syncs.is_running(connector_of(j)))
        .filter(|(_, j)| instant_ms(j.get("last_seen")).unwrap_or(0) + STALE_MS <= now)
        .map(|(id, _)| id.clone())
        .collect();
    // Edited drafts of connectors roll runs, and their validation.
    let drafts: Vec<(String, Value)> = st
        .metadata
        .xpack
        .iter(CONNECTORS)
        .filter(|(_, c)| config::runs(c) && draft_edited(c))
        .map(|(id, c)| {
            let draft = c.get("filtering").and_then(Value::as_array).and_then(|f| f.first()).and_then(|f| f.get("draft")).cloned().unwrap_or(Value::Null);
            (id.clone(), rules::validate(str_of(c, "service_type"), &draft))
        })
        .collect();
    // Schedules that came due (or are seen for the first time, which starts their count).
    let fired: Vec<(String, &'static str, bool)> = st
        .metadata
        .xpack
        .iter(CONNECTORS)
        .filter(|(_, c)| config::runs(c))
        .flat_map(|(id, c)| {
            schedules(c).into_iter().filter_map(|(kind, cron)| match anchor(&st, id, kind) {
                None => Some((id.clone(), kind, false)),
                Some(a) => next_fire(&cron, a).filter(|t| *t <= now).map(|_| (id.clone(), kind, true)),
            })
        })
        .collect();
    drop(st);
    // Nothing to settle: no write (a write would wake the scheduler again).
    let settle = !(orphans.is_empty() && drafts.is_empty() && fired.is_empty());
    let _ = cluster
        .update(move |st| {
            if !settle {
                return Ok(());
            }
            for id in orphans {
                let Some(j) = st.metadata.xpack.get(SYNC_JOBS, &id).cloned() else { continue };
                let cid = connector_of(&j).to_string();
                let outcome = if str_of(&j, "status") == "canceling" { Outcome::Canceled } else { Outcome::Failed("the sync stopped checking in: the node running it went away".into()) };
                let Value::Obj(mut j) = j else { continue };
                let t = now_nanos_string();
                settle_job(&mut j, &outcome, &t);
                store::put(st, SYNC_JOBS, &id, Value::Obj(j), now)?;
                settle_connector(st, &cid, &outcome, None, &t, now)?;
            }
            for (id, validation) in drafts {
                let Some(Value::Obj(mut c)) = st.metadata.xpack.get(CONNECTORS, &id).cloned() else { continue };
                if let Some(Value::Arr(f)) = c.get_mut("filtering")
                    && let Some(Value::Obj(entry)) = f.first_mut()
                    && let Some(Value::Obj(draft)) = entry.get_mut("draft")
                {
                    draft.insert("validation".into(), validation);
                }
                store::put(st, CONNECTORS, &id, Value::Obj(c), now)?;
            }
            // Schedules that came due make jobs (unless one is already waiting or running).
            let busy = busy_connectors(st);
            let pending: HashSet<String> = st.metadata.xpack.iter(SYNC_JOBS).filter(|(_, j)| str_of(j, "status") == "pending").map(|(_, j)| connector_of(j).to_string()).collect();
            for (cid, kind, run) in fired {
                let mut rec = st.metadata.xpack.get(RECORDS, &cid).cloned().unwrap_or_else(|| obj! {});
                if let Value::Obj(m) = &mut rec {
                    let mut anchors = m.get("anchors").and_then(Value::as_object).cloned().unwrap_or_default();
                    anchors.insert(kind.into(), now.into());
                    m.insert("anchors".into(), Value::Obj(anchors));
                }
                store::put(st, RECORDS, &cid, rec, now)?;
                if !run || busy.contains(&cid) || pending.contains(&cid) {
                    continue;
                }
                super::connectors::insert_job(st, &cid, kind, "scheduled", &crate::meta::names::time_based_id(), now)?;
                if let Some(Value::Obj(mut c)) = st.metadata.xpack.get(CONNECTORS, &cid).cloned() {
                    let field = if kind == "full" { "last_sync_scheduled_at" } else { "last_incremental_sync_scheduled_at" };
                    c.insert(field.into(), now_nanos_string().into());
                    store::put(st, CONNECTORS, &cid, Value::Obj(c), now)?;
                }
            }
            Ok(())
        })
        .await;
    // Pending jobs, oldest first, of connectors with nothing running.
    let st = cluster.state();
    let busy = busy_connectors(&st);
    let mut pending: Vec<(String, String, String)> = st
        .metadata
        .xpack
        .iter(SYNC_JOBS)
        .filter(|(_, j)| str_of(j, "status") == "pending")
        .filter_map(|(id, j)| {
            let cid = connector_of(j);
            let c = st.metadata.xpack.get(CONNECTORS, cid)?;
            (config::runs(c) && !busy.contains(cid)).then(|| (str_of(j, "created_at").to_string(), id.clone(), cid.to_string()))
        })
        .collect();
    pending.sort();
    drop(st);
    for (_, job, cid) in pending {
        let Ok(permit) = NODE_SYNCS.try_acquire() else { break };
        if !syncs.begin(&cid) {
            continue;
        }
        let c = handle.clone();
        let (j, id) = (job.clone(), cid.clone());
        tokio::spawn(supervise(handle.clone(), job, cid, Some(permit), async move { run(&c, &j, &id).await }));
    }
}

/// Runs a claimed slot's sync (`work`) as a task of its own and, however it ends (a panic
/// included), frees the slot and the node's permit, settles a job it left unfinished, and wakes
/// the scheduler for the next pending job.
async fn supervise(cluster: Arc<Cluster>, job: String, cid: String, permit: Option<tokio::sync::SemaphorePermit<'static>>, work: impl std::future::Future<Output = ()> + Send + 'static) {
    /// Frees the slot even if this task is dropped (the runtime shutting down).
    struct Slot(Arc<Cluster>, String);
    impl Drop for Slot {
        fn drop(&mut self) {
            self.0.xpack.connector_syncs.end(&self.1);
        }
    }
    let slot = Slot(cluster.clone(), cid.clone());
    if let Err(e) = tokio::spawn(work).await {
        let why = if e.is_panic() { "the sync stopped on an internal error" } else { "the sync was stopped" };
        let _ = cluster
            .update(move |st| {
                let Some(Value::Obj(mut j)) = st.metadata.xpack.get(SYNC_JOBS, &job).cloned() else { return Ok(()) };
                let status = j.get("status").and_then(Value::as_str).unwrap_or_default().to_string();
                if matches!(status.as_str(), "completed" | "error" | "canceled") {
                    return Ok(());
                }
                let outcome = if status == "canceling" { Outcome::Canceled } else { Outcome::Failed(why.into()) };
                let (now, t) = (crate::time::now_millis(), now_nanos_string());
                settle_job(&mut j, &outcome, &t);
                store::put(st, SYNC_JOBS, &job, Value::Obj(j), now)?;
                settle_connector(st, &cid, &outcome, None, &t, now)
            })
            .await;
    }
    drop(permit);
    drop(slot);
    cluster.jobs.kick(&cluster);
}

/// How a sync ended.
enum Outcome {
    Completed,
    Canceled,
    Failed(String),
}

/// Why a sync stopped before its end.
pub enum Stop {
    Canceled,
    Failed(String),
}

impl From<String> for Stop {
    fn from(s: String) -> Stop {
        Stop::Failed(s)
    }
}

/// The job's fields an outcome sets.
fn settle_job(j: &mut Map, outcome: &Outcome, t: &str) {
    j.insert("last_seen".into(), t.into());
    j.insert("completed_at".into(), t.into());
    match outcome {
        Outcome::Completed => {
            j.insert("status".into(), "completed".into());
        }
        Outcome::Canceled => {
            j.insert("status".into(), "canceled".into());
            j.insert("canceled_at".into(), t.into());
        }
        Outcome::Failed(e) => {
            j.insert("status".into(), "error".into());
            j.insert("error".into(), e.clone().into());
        }
    }
}

/// The connector's fields an outcome sets (`done`: the counts and, on success, the new cursor).
fn settle_connector(st: &mut crate::meta::ClusterState, cid: &str, outcome: &Outcome, done: Option<(&Counters, Option<Value>)>, t: &str, now: i64) -> crate::error::EsResult<()> {
    let Some(Value::Obj(mut c)) = st.metadata.xpack.get(CONNECTORS, cid).cloned() else { return Ok(()) };
    match outcome {
        Outcome::Completed => {
            c.insert("last_sync_status".into(), "completed".into());
            c.insert("last_sync_error".into(), Value::Null);
            c.insert("last_synced".into(), t.into());
            if let Some((n, cursor)) = done {
                c.insert("last_indexed_document_count".into(), (n.indexed as i64).into());
                c.insert("last_deleted_document_count".into(), (n.deleted as i64).into());
                if let Some(cur) = cursor {
                    c.insert("sync_cursor".into(), cur);
                }
            }
            if matches!(c.get("status").and_then(Value::as_str), Some("configured" | "error" | "connected")) {
                c.insert("status".into(), "connected".into());
                c.insert("error".into(), Value::Null);
            }
        }
        Outcome::Canceled => {
            c.insert("last_sync_status".into(), "canceled".into());
        }
        Outcome::Failed(e) => {
            c.insert("last_sync_status".into(), "error".into());
            c.insert("last_sync_error".into(), e.clone().into());
            c.insert("status".into(), "error".into());
            c.insert("error".into(), e.clone().into());
        }
    }
    store::put(st, CONNECTORS, cid, Value::Obj(c), now).map(|_| ())
}

/// One job, start to end.
async fn run(cluster: &Arc<Cluster>, job_id: &str, cid: &str) {
    // Claim it: only a job still pending is ours.
    let (jid, connector_id) = (job_id.to_string(), cid.to_string());
    let claimed = cluster
        .update(move |st| {
            let Some(Value::Obj(mut j)) = st.metadata.xpack.get(SYNC_JOBS, &jid).cloned() else { return Ok(false) };
            if j.get("status").and_then(Value::as_str) != Some("pending") {
                return Ok(false);
            }
            let t = now_nanos_string();
            j.insert("status".into(), "in_progress".into());
            j.insert("worker_hostname".into(), WORKER.into());
            j.insert("started_at".into(), t.clone().into());
            j.insert("last_seen".into(), t.into());
            let now = crate::time::now_millis();
            store::put(st, SYNC_JOBS, &jid, Value::Obj(j), now)?;
            if let Some(Value::Obj(mut c)) = st.metadata.xpack.get(CONNECTORS, &connector_id).cloned() {
                c.insert("last_sync_status".into(), "in_progress".into());
                store::put(st, CONNECTORS, &connector_id, Value::Obj(c), now)?;
            }
            Ok(true)
        })
        .await;
    if !matches!(claimed, Ok(true)) {
        return;
    }
    let principal = cluster.state().metadata.xpack.get(RECORDS, cid).and_then(|r| r.get("user")).and_then(internal::principal_of);
    let mut counters = Counters::default();
    let result = internal::as_principal(principal.clone(), sync(cluster, job_id, cid, principal.as_ref(), &mut counters)).await;
    let (outcome, cursor) = match result {
        Ok(cursor) => (Outcome::Completed, Some(cursor)),
        Err(Stop::Canceled) => (Outcome::Canceled, None),
        Err(Stop::Failed(e)) => (Outcome::Failed(e), None),
    };
    let total = match outcome {
        Outcome::Completed => count(cluster, cid).await,
        _ => None,
    };
    let (jid, cid) = (job_id.to_string(), cid.to_string());
    let _ = cluster
        .update(move |st| {
            let now = crate::time::now_millis();
            let t = now_nanos_string();
            if let Some(Value::Obj(mut j)) = st.metadata.xpack.get(SYNC_JOBS, &jid).cloned() {
                counters.write(&mut j);
                if let Some(n) = total {
                    j.insert("total_document_count".into(), n.into());
                }
                settle_job(&mut j, &outcome, &t);
                store::put(st, SYNC_JOBS, &jid, Value::Obj(j), now)?;
            }
            settle_connector(st, &cid, &outcome, Some((&counters, cursor)), &t, now)
        })
        .await;
}

/// The documents in a connector's index after its sync.
async fn count(cluster: &Cluster, cid: &str) -> Option<i64> {
    let index = cluster.state().metadata.xpack.get(CONNECTORS, cid)?.get("index_name")?.as_str()?.to_string();
    let index = enc(&index);
    internal::call(cluster, "POST", &format!("/{index}/_refresh"), None).await.ok()?;
    internal::call(cluster, "GET", &format!("/{index}/_count"), None).await.ok()?.get("count")?.as_i64()
}

/// The sync proper: the connector's source into its index. The new sync cursor on success.
async fn sync(cluster: &Arc<Cluster>, job_id: &str, cid: &str, principal: Option<&crate::xpack::security::Principal>, counters: &mut Counters) -> Result<Value, Stop> {
    let st = cluster.state();
    let job = st.metadata.xpack.get(SYNC_JOBS, job_id).cloned().ok_or(Stop::Canceled)?;
    let connector = st.metadata.xpack.get(CONNECTORS, cid).filter(|c| config::runs(c)).cloned().ok_or_else(|| Stop::Failed("the connector is gone".into()))?;
    drop(st);
    if let Some(e) = internal::principal_refusal(cluster, principal) {
        return Err(Stop::Failed(e.reason().to_string()));
    }
    let job_type = str_of(&job, "job_type").to_string();
    if job_type == "access_control" {
        return Err(Stop::Failed("access control syncs aren't supported by roll's connectors".into()));
    }
    let full = job_type == "full";
    let service = str_of(&connector, "service_type").to_string();
    let values = config::Values::open(cluster, &connector).map_err(|e| Stop::Failed(e.reason().to_string()))?;
    let index = connector.get("index_name").and_then(Value::as_str).filter(|i| !i.is_empty()).ok_or_else(|| Stop::Failed("the connector has no [index_name]".into()))?.to_string();
    let filtering = job.pointer(&["connector", "filtering"]).cloned().unwrap_or(Value::Null);
    let rules = rules::Rules::compile(&filtering).map_err(|p| Stop::Failed(format!("the active sync rules don't parse: {}", p.into_iter().map(|p| p.message).collect::<Vec<_>>().join("; "))))?;
    ensure_index(cluster, &index).await?;
    let pipeline = match connector.pointer(&["pipeline", "name"]).and_then(Value::as_str) {
        Some(p) if internal::call(cluster, "GET", &format!("/_ingest/pipeline/{}", enc(p)), None).await.is_ok() => Some(p.to_string()),
        _ => None,
    };
    let setting = |k: &str, d: u64| cluster.config.node_setting(k).and_then(|v| v.parse::<u64>().ok()).filter(|n| *n > 0).unwrap_or(d);
    let allow_private = cluster.config.node_setting(ALLOW_PRIVATE).is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let mut sink = Sink {
        cluster,
        job: job_id.to_string(),
        index,
        pipeline,
        rules,
        full,
        seen: Vec::new(),
        counters,
        docs_per_second: setting(MAX_DOCS_PER_SECOND, 1_000),
        bytes_per_second: setting(MAX_BYTES_PER_SECOND, 10 << 20),
        started: Instant::now(),
        beat: Instant::now(),
    };
    let cursor = connector.get("sync_cursor").cloned().unwrap_or(Value::Null);
    let advanced = rules::advanced(&filtering).cloned();
    let result = match service.as_str() {
        "postgresql" => postgres::sync(&values, &mut sink, full, &cursor, allow_private).await,
        _ => s3::sync(&values, &mut sink, full, &cursor, advanced.as_ref(), allow_private).await,
    };
    let cursor = result.map_err(|s| match s {
        Stop::Failed(e) => Stop::Failed(values.scrub(&e)),
        other => other,
    })?;
    if full {
        sink.delete_unseen().await?;
    }
    Ok(cursor)
}

/// A name as one segment of an internal URI's path or query.
fn enc(name: &str) -> String {
    crate::s3::uri_encode(name, true)
}

async fn ensure_index(cluster: &Cluster, index: &str) -> Result<(), Stop> {
    let path = format!("/{}", enc(index));
    match internal::call(cluster, "GET", &path, None).await {
        Ok(_) => Ok(()),
        Err(e) if e.kind == "index_not_found_exception" => match internal::call(cluster, "PUT", &path, None).await {
            Ok(_) => Ok(()),
            Err(e) if e.kind == "resource_already_exists_exception" => Ok(()),
            Err(e) => Err(Stop::Failed(format!("creating the index [{index}] failed: {}", e.reason()))),
        },
        Err(e) => Err(Stop::Failed(format!("the index [{index}] can't be read: {}", e.reason()))),
    }
}

/// A sync's counts.
#[derive(Default)]
pub struct Counters {
    pub indexed: u64,
    /// Bytes of the documents indexed.
    pub volume: u64,
    pub deleted: u64,
    pub failed: u64,
    pub processed: u64,
    pub skipped: u64,
    /// Bytes read from the source (throttled).
    pub bytes: u64,
    /// More of the job's `metadata`.
    pub metadata: Map,
}

impl Counters {
    fn write(&self, j: &mut Map) {
        j.insert("indexed_document_count".into(), (self.indexed as i64).into());
        // Elastic's connectors report the volume in MiB.
        j.insert("indexed_document_volume".into(), (((self.volume + (1 << 19)) >> 20) as i64).into());
        j.insert("deleted_document_count".into(), (self.deleted as i64).into());
        let mut meta = j.get("metadata").and_then(Value::as_object).cloned().unwrap_or_default();
        meta.insert("failed_document_count".into(), (self.failed as i64).into());
        meta.insert("skipped_document_count".into(), (self.skipped as i64).into());
        for (k, v) in &self.metadata {
            meta.insert(k.clone(), v.clone());
        }
        j.insert("metadata".into(), Value::Obj(meta));
    }
}

/// Where a source's documents go: sync rules, bulk indexing into the connector's index,
/// throttling, the job's heartbeat and cancel.
pub struct Sink<'a> {
    cluster: &'a Cluster,
    job: String,
    index: String,
    pipeline: Option<String>,
    rules: rules::Rules,
    full: bool,
    /// Hashes of the ids a full sync saw.
    seen: Vec<u64>,
    pub counters: &'a mut Counters,
    docs_per_second: u64,
    bytes_per_second: u64,
    started: Instant,
    beat: Instant,
}

fn id_hash(id: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    id.hash(&mut h);
    h.finish()
}

impl Sink<'_> {
    /// Counts bytes read from the source that aren't in a document (they're throttled too).
    pub fn read(&mut self, bytes: u64) {
        self.counters.bytes += bytes;
    }

    /// Records a document the source couldn't hand over (too large).
    pub fn skip(&mut self) {
        self.counters.skipped += 1;
    }

    /// Indexes one page of documents (each with its `_id`).
    pub async fn page(&mut self, docs: Vec<Map>) -> Result<(), Stop> {
        let mut body = String::new();
        let mut sizes = Vec::new();
        for mut doc in docs {
            let Some(id) = doc.shift_remove("_id").map(|v| v.to_es_string()) else { continue };
            self.counters.processed += 1;
            if !self.rules.includes(&doc) {
                continue;
            }
            if self.full {
                if self.seen.len() >= MAX_SEEN {
                    return Err(Stop::Failed(format!("a full sync indexes at most {MAX_SEEN} documents")));
                }
                self.seen.push(id_hash(&id));
            }
            let source = Value::Obj(doc).to_json();
            self.counters.bytes += source.len() as u64;
            sizes.push(source.len() as u64);
            body.push_str(&obj! { "index" => obj! { "_index" => self.index.clone(), "_id" => id } }.to_json());
            body.push('\n');
            body.push_str(&source);
            body.push('\n');
        }
        if !sizes.is_empty() {
            let uri = match &self.pipeline {
                Some(p) => format!("/_bulk?pipeline={}", enc(p)),
                None => "/_bulk".to_string(),
            };
            let resp = internal::call_ndjson(self.cluster, "POST", &uri, body).await.map_err(|e| Stop::Failed(format!("indexing into [{}] failed: {}", self.index, e.reason())))?;
            let items = resp.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            for (item, size) in items.iter().zip(&sizes) {
                if item.pointer(&["index", "status"]).and_then(Value::as_i64).is_some_and(|s| s < 300) {
                    self.counters.indexed += 1;
                    self.counters.volume += size;
                } else {
                    self.counters.failed += 1;
                }
            }
            if self.counters.failed > 100 && self.counters.failed * 10 > self.counters.processed {
                let reason = items.iter().find_map(|i| i.pointer(&["index", "error", "reason"]).and_then(Value::as_str)).unwrap_or("rejected").to_string();
                return Err(Stop::Failed(format!("{} documents failed to index (the last: {reason})", self.counters.failed)));
            }
        }
        self.throttle().await;
        self.check().await
    }

    /// Sleeps as long as the sync is ahead of its rates.
    async fn throttle(&self) {
        let by_docs = self.counters.processed as f64 / self.docs_per_second as f64;
        let by_bytes = self.counters.bytes as f64 / self.bytes_per_second as f64;
        let ahead = by_docs.max(by_bytes) - self.started.elapsed().as_secs_f64();
        if ahead > 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(ahead.min(60.0))).await;
        }
    }

    /// Whether the job is still running (not canceled, not deleted).
    fn running(&self) -> bool {
        self.cluster.state().metadata.xpack.get(SYNC_JOBS, &self.job).is_some_and(|j| str_of(j, "status") == "in_progress")
    }

    /// Resolves once the job was canceled (or deleted): sources race their I/O against it.
    pub async fn canceled(&self) {
        while self.running() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Stops when the job was canceled (or deleted); writes its counters every [`HEARTBEAT`].
    pub async fn check(&mut self) -> Result<(), Stop> {
        if !self.running() {
            return Err(Stop::Canceled);
        }
        if self.beat.elapsed() < HEARTBEAT {
            return Ok(());
        }
        self.beat = Instant::now();
        let id = self.job.clone();
        let mut snapshot = Map::new();
        self.counters.write(&mut snapshot);
        self.cluster
            .update(move |st| {
                let Some(Value::Obj(mut j)) = st.metadata.xpack.get(SYNC_JOBS, &id).cloned() else { return Ok(()) };
                for (k, v) in snapshot {
                    j.insert(k, v);
                }
                j.insert("last_seen".into(), now_nanos_string().into());
                store::put(st, SYNC_JOBS, &id, Value::Obj(j), crate::time::now_millis()).map(|_| ())
            })
            .await
            .map_err(|e| Stop::Failed(format!("the job can't be updated: {}", e.reason())))
    }

    /// A full sync's last step: deletes the documents of the index it didn't see.
    async fn delete_unseen(&mut self) -> Result<(), Stop> {
        self.seen.sort_unstable();
        let listing = |index: &str, e: crate::error::EsError| Stop::Failed(format!("listing [{index}] for deletes failed: {}", e.reason()));
        let first = obj! { "size" => 1000, "_source" => false, "sort" => crate::arr!["_doc"] };
        let mut page = internal::call(self.cluster, "POST", &format!("/{}/_search?scroll=2m", enc(&self.index)), Some(&first)).await.map_err(|e| listing(&self.index, e))?;
        let mut scroll_id = page.get("_scroll_id").and_then(Value::as_str).map(str::to_string);
        let result = loop {
            let hits = page.pointer(&["hits", "hits"]).and_then(Value::as_array).cloned().unwrap_or_default();
            if hits.is_empty() {
                break Ok(());
            }
            let gone: Vec<String> = hits.iter().filter_map(|h| h.get("_id").and_then(Value::as_str)).filter(|id| self.seen.binary_search(&id_hash(id)).is_err()).map(str::to_string).collect();
            if !gone.is_empty() {
                let mut body = String::new();
                for id in gone {
                    body.push_str(&obj! { "delete" => obj! { "_index" => self.index.clone(), "_id" => id } }.to_json());
                    body.push('\n');
                }
                match internal::call_ndjson(self.cluster, "POST", "/_bulk", body).await {
                    Ok(resp) => {
                        let n = resp.get("items").and_then(Value::as_array).map_or(0, |a| a.iter().filter(|i| i.pointer(&["delete", "status"]).and_then(Value::as_i64) == Some(200)).count());
                        self.counters.deleted += n as u64;
                    }
                    Err(e) => break Err(Stop::Failed(format!("deleting from [{}] failed: {}", self.index, e.reason()))),
                }
            }
            if let Err(e) = self.check().await {
                break Err(e);
            }
            let Some(sid) = scroll_id.clone() else { break Ok(()) };
            page = match internal::call(self.cluster, "POST", "/_search/scroll", Some(&obj! { "scroll" => "2m", "scroll_id" => sid })).await {
                Ok(p) => p,
                Err(e) => break Err(listing(&self.index, e)),
            };
            if let Some(s) = page.get("_scroll_id").and_then(Value::as_str) {
                scroll_id = Some(s.to_string());
            }
        };
        if let Some(sid) = scroll_id {
            let _ = internal::call(self.cluster, "DELETE", "/_search/scroll", Some(&obj! { "scroll_id" => crate::arr![sid] })).await;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{ClusterConfig, MemoryStore};

    async fn call(c: &Cluster, method: &str, uri: &str, body: &str) -> Value {
        let req = if body.is_empty() { crate::EsRequest::new(method, uri) } else { crate::EsRequest::json(method, uri, body.to_string()) };
        crate::json::parse(&crate::rest::handle(c, req).await.body).ok().flatten().unwrap_or(Value::Null)
    }

    /// A sync that panics still frees its slot (the cluster isn't pinned loaded) and leaves its
    /// job failed, not `in_progress` forever.
    #[tokio::test]
    async fn a_panicking_sync_frees_its_slot_and_fails_its_job() {
        let c = Arc::new(Cluster::open(ClusterConfig::new("c"), Arc::new(MemoryStore::new())).await.unwrap());
        call(&c, "PUT", "/_connector/x", r#"{"index_name":"content-x","is_native":true,"service_type":"s3"}"#).await;
        let job = call(&c, "POST", "/_connector/_sync_job", r#"{"id":"x"}"#).await.get("id").and_then(Value::as_str).unwrap().to_string();
        assert!(c.xpack.connector_syncs.begin("x"));
        supervise(c.clone(), job.clone(), "x".into(), None, async { panic!("a bug in a source") }).await;
        assert!(!c.xpack.connector_syncs.busy(), "the slot is free");
        let j = call(&c, "GET", &format!("/_connector/_sync_job/{job}"), "").await;
        assert_eq!(j.get("status").and_then(Value::as_str), Some("error"), "{j:?}");
        let conn = call(&c, "GET", "/_connector/x", "").await;
        assert_eq!(conn.get("last_sync_status").and_then(Value::as_str), Some("error"));
    }
}
