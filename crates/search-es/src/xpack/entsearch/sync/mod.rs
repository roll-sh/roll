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
pub mod crawler;
pub mod extract;
pub mod json_scan;
pub mod postgres;
pub mod rules;
pub mod s3;
pub mod slots;

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
/// The record that a cluster's connectors were looked at once roll ran syncs ([`adopt_existing`]).
const EXISTING: (&str, &str) = ("connector.roll.existing", "v1");
/// Node settings.
pub const ALLOW_PRIVATE: &str = "roll.connectors.allow_private_hosts";
pub const MAX_DOCS_PER_SECOND: &str = "roll.connectors.max_docs_per_second";
pub const MAX_BYTES_PER_SECOND: &str = "roll.connectors.max_bytes_per_second";
/// How long one sync may run (a time value, `6h` by default): past it the job fails.
pub const MAX_SYNC_DURATION: &str = "roll.connectors.max_sync_duration";
/// How many documents one sync (full or incremental) may read (20 million by default).
pub const MAX_DOCS_PER_SYNC: &str = "roll.connectors.max_docs_per_sync";

const HEARTBEAT: Duration = Duration::from_secs(10);
/// A job roll claimed that hasn't checked in for this long belongs to a node that went away.
const STALE_MS: i64 = 5 * 60_000;
/// Syncs running at once on one cluster.
const MAX_RUNNING: usize = 2;
/// How often a pending job waiting for one of the node's slots is looked at again anyway (a
/// slot that frees wakes the next waiter's scheduler: [`slots`]).
const SLOT_RETRY_MS: i64 = 60_000;
const DEFAULT_MAX_DURATION_MS: i64 = 6 * 3_600_000;
const DEFAULT_MAX_DOCS: u64 = 20_000_000;
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
    /// The cluster's key, owner and slot pool ([`slots`]), set by its first [`run_due`].
    identity: std::sync::OnceLock<(String, String, Arc<slots::Slots>)>,
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
pub fn next_due(cluster: &Cluster, st: &crate::meta::ClusterState, syncs: Option<&Syncs>) -> Option<i64> {
    let mut due: Option<i64> = None;
    let mut at = |t: i64| due = Some(due.map_or(t, |d| d.min(t)));
    let busy = busy_connectors(st);
    // Whether it's this cluster's turn for a node slot: asked once, when a job wants one.
    let mut turn: Option<bool> = None;
    let mut runnable = HashSet::new();
    for (id, c) in st.metadata.xpack.iter(CONNECTORS) {
        if !syncs_here(cluster, st, id, c) {
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
                // Not this cluster's turn for a node slot: a slot freeing wakes it.
                Some(s) if !*turn.get_or_insert_with(|| s.identity.get().is_none_or(|(k, o, pool)| pool.admits(k, o))) => at(crate::time::now_millis() + SLOT_RETRY_MS),
                _ => at(0),
            },
            // A job left by a node that went away; one running here checks in by itself (a long
            // statement can outlast the stale time without waking the scheduler for it).
            s if active(s) && str_of(j, "worker_hostname") == WORKER && syncs.is_none_or(|x| !x.is_running(cid)) => {
                at(instant_ms(j.get("last_seen")).unwrap_or(0) + STALE_MS)
            }
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
        .filter(|(id, c)| syncs_here(cluster, &st, id, c) && draft_edited(c))
        .map(|(id, c)| {
            let draft = c.get("filtering").and_then(Value::as_array).and_then(|f| f.first()).and_then(|f| f.get("draft")).cloned().unwrap_or(Value::Null);
            (id.clone(), rules::validate(str_of(c, "service_type"), &draft, &config::listed(c, "buckets"), &crawler::ceilings(cluster)))
        })
        .collect();
    // Schedules that came due (or are seen for the first time, which starts their count).
    let fired: Vec<(String, &'static str, bool)> = st
        .metadata
        .xpack
        .iter(CONNECTORS)
        .filter(|(id, c)| syncs_here(cluster, &st, id, c))
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
    let pending = pending_jobs(cluster, &cluster.state(), syncs);
    // A slot of the node's, in turn with every other cluster ([`slots`]).
    let key = cluster.name.clone();
    let owner = cluster.config.owner.clone().unwrap_or_else(|| format!("cluster {key}"));
    let pool = cluster.config.connector_slots.clone().unwrap_or_else(|| slots::node().clone());
    let _ = syncs.identity.set((key.clone(), owner.clone(), pool.clone()));
    if pending.is_empty() {
        pool.leave(&key);
    }
    let weak = Arc::downgrade(&handle);
    // What a waiter checks it still wants a slot for: a few of its jobs, each a lookup.
    let wanted: Arc<Vec<(String, String)>> = Arc::new(pending.iter().take(16).map(|(_, j, c)| (j.clone(), c.clone())).collect());
    for (_, job, cid) in pending {
        if !syncs.has_room() {
            break;
        }
        let (alive, woken, wanted) = (weak.clone(), weak.clone(), wanted.clone());
        // A waiter counts only while its cluster is loaded and still has a job to start.
        let alive: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || alive.upgrade().is_some_and(|c| wanted.iter().any(|(j, cid)| startable(&c, j, cid))));
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let Some(c) = woken.upgrade() {
                c.jobs.kick(&c);
            }
        });
        let Some(permit) = pool.take(&key, &owner, alive, wake) else { break };
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
async fn supervise(cluster: Arc<Cluster>, job: String, cid: String, permit: Option<slots::Permit>, work: impl std::future::Future<Output = ()> + Send + 'static) {
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

/// Whether job `job` of connector `cid` could start now (a few lookups: the slot pool asks under
/// its lock).
fn startable(cluster: &Cluster, job: &str, cid: &str) -> bool {
    let st = cluster.state();
    st.metadata.xpack.get(SYNC_JOBS, job).is_some_and(|j| str_of(j, "status") == "pending")
        && st.metadata.xpack.get(CONNECTORS, cid).is_some_and(|c| syncs_here(cluster, &st, cid, c))
        && !cluster.xpack.connector_syncs.is_running(cid)
}

/// The pending jobs a cluster could start now, oldest first (`created_at`, job, connector): of
/// connectors roll runs, with nothing in progress or starting.
fn pending_jobs(cluster: &Cluster, st: &crate::meta::ClusterState, syncs: &Syncs) -> Vec<(String, String, String)> {
    let busy = busy_connectors(st);
    let mut pending: Vec<(String, String, String)> = st
        .metadata
        .xpack
        .iter(SYNC_JOBS)
        .filter(|(_, j)| str_of(j, "status") == "pending")
        .filter_map(|(id, j)| {
            let cid = connector_of(j);
            let c = st.metadata.xpack.get(CONNECTORS, cid)?;
            (syncs_here(cluster, st, cid, c) && !busy.contains(cid) && !syncs.is_running(cid)).then(|| (str_of(j, "created_at").to_string(), id.clone(), cid.to_string()))
        })
        .collect();
    pending.sort();
    pending
}

/// Whether roll runs connector `id`'s syncs on this cluster: one it runs, and not one waiting to
/// be made the cluster's own ([`waits_for_adoption`]).
fn syncs_here(cluster: &Cluster, st: &crate::meta::ClusterState, id: &str, c: &Value) -> bool {
    config::runs(c) && !waits_for_adoption(cluster, st, id)
}

/// Whether connector `id`'s syncs stay paused until the cluster makes it its own (puts it again
/// or writes its configuration: [`adopt`]): one a branch inherited from its parent
/// ([`crate::branch::inherited_connector`]), or one that was there before roll ran syncs
/// ([`adopt_existing`]).
pub fn waits_for_adoption(cluster: &Cluster, st: &crate::meta::ClusterState, id: &str) -> bool {
    crate::branch::inherited_connector(cluster, st, id) || st.metadata.xpack.get(RECORDS, id).and_then(|r| r.get("waiting")).and_then(Value::as_bool) == Some(true)
}

/// Records, once, that the cluster's connectors are made under roll's syncs (every write to a
/// connector does): [`adopt_existing`] then never pauses one.
pub fn mark_current(st: &mut crate::meta::ClusterState, now_ms: i64) -> crate::error::EsResult<()> {
    if st.metadata.xpack.get(EXISTING.0, EXISTING.1).is_some() {
        return Ok(());
    }
    store::put(st, EXISTING.0, EXISTING.1, obj! { "at" => now_ms }, now_ms).map(|_| ())
}

/// Makes connector `id` the cluster's own: its syncs run from now on.
pub fn adopt(st: &mut crate::meta::ClusterState, id: &str, now_ms: i64) -> crate::error::EsResult<()> {
    mark_current(st, now_ms)?;
    crate::branch::adopt_connector(st, id, now_ms)?;
    if let Some(Value::Obj(mut rec)) = st.metadata.xpack.get(RECORDS, id).cloned()
        && rec.shift_remove("waiting").is_some()
    {
        store::put(st, RECORDS, id, Value::Obj(rec), now_ms)?;
    }
    Ok(())
}

/// Once per cluster, before its scheduler starts: the connectors it held before roll ran syncs
/// (made when roll only stored them, credentials in the clear as given, or on a branch made
/// before branches recorded what they inherited) wait to be made its own ([`waits_for_adoption`]),
/// so nothing starts reading a source on deploy with credentials nobody gave roll to use, and
/// their credentials are sealed where they lie. Adopting one keeps only credentials given again
/// ([`config::protect`]). The record that this was done is written here when there was
/// something to pause, and otherwise by the first write to a connector ([`mark_current`]), so a
/// connector made under roll's syncs never waits (a branch has its parent's record), and a
/// cluster without connectors isn't written to on load.
///
/// Every load also seals any sensitive value a connector still holds in the clear (one an earlier
/// build let a writer mark not sensitive), or clears it with no key ([`config::seal_in_place`]);
/// it writes only when there is one.
pub async fn adopt_existing(cluster: &Cluster) -> crate::error::EsResult<()> {
    let st = cluster.state();
    let first = st.metadata.xpack.get(EXISTING.0, EXISTING.1).is_none() && st.metadata.xpack.iter(CONNECTORS).any(|(_, c)| config::runs(c));
    let (sealer, unsealed) = (cluster.config.sealer.clone(), cluster.config.unsealed_secrets);
    // A node that keeps secrets as given (local development, no key) has nothing to seal.
    let clear = (sealer.is_some() || !unsealed) && st.metadata.xpack.iter(CONNECTORS).any(|(_, c)| config::holds_clear_secret(c));
    if !first && !clear {
        return Ok(());
    }
    drop(st);
    let me = crate::branch::cluster_id(cluster);
    let now = crate::time::now_millis();
    cluster
        .update(move |st| {
            if st.metadata.xpack.get(EXISTING.0, EXISTING.1).is_some() {
                let held: Vec<(String, Value)> = st.metadata.xpack.iter(CONNECTORS).filter(|(_, c)| config::holds_clear_secret(c)).map(|(k, v)| (k.clone(), v.clone())).collect();
                for (id, c) in held {
                    let Value::Obj(mut m) = c else { continue };
                    config::seal_in_place(&mut m, &me, sealer.as_deref(), unsealed);
                    store::put(st, CONNECTORS, &id, Value::Obj(m), now)?;
                }
                return Ok(());
            }
            let existing: Vec<(String, Value)> = st.metadata.xpack.iter(CONNECTORS).filter(|(_, c)| config::runs(c)).map(|(k, v)| (k.clone(), v.clone())).collect();
            for (id, c) in existing {
                let Value::Obj(mut m) = c else { continue };
                config::seal_in_place(&mut m, &me, sealer.as_deref(), unsealed);
                store::put(st, CONNECTORS, &id, Value::Obj(m), now)?;
                let mut rec = st.metadata.xpack.get(RECORDS, &id).and_then(Value::as_object).cloned().unwrap_or_default();
                rec.insert("waiting".into(), true.into());
                store::put(st, RECORDS, &id, Value::Obj(rec), now)?;
            }
            mark_current(st, now)
        })
        .await
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

/// The most of an error's text a job or a connector keeps.
const ERROR_BYTES_MAX: usize = 4 << 10;

/// `text`, cut to [`ERROR_BYTES_MAX`] on a character boundary.
pub(crate) fn capped(mut text: String) -> String {
    if text.len() > ERROR_BYTES_MAX {
        let mut at = ERROR_BYTES_MAX - 3;
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        text.truncate(at);
        text.push_str("...");
    }
    text
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
            j.insert("error".into(), capped(e.clone()).into());
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
            let e = &capped(e.clone());
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
    let me = cluster.clone();
    let claimed = cluster
        .update(move |st| {
            let Some(Value::Obj(mut j)) = st.metadata.xpack.get(SYNC_JOBS, &jid).cloned() else { return Ok(false) };
            if j.get("status").and_then(Value::as_str) != Some("pending") {
                return Ok(false);
            }
            // A connector waiting to be the cluster's own doesn't start (it stays pending).
            if waits_for_adoption(&me, st, &connector_id) {
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
    let crawler = service == crawler::SERVICE;
    ensure_index(cluster, &index, crawler.then(crawler::mappings)).await?;
    let pipeline = match connector.pointer(&["pipeline", "name"]).and_then(Value::as_str) {
        Some(p) if internal::call(cluster, "GET", &format!("/_ingest/pipeline/{}", enc(p)), None).await.is_ok() => Some(p.to_string()),
        _ => None,
    };
    let setting = |k: &str, d: u64| cluster.config.node_setting(k).and_then(|v| v.parse::<u64>().ok()).filter(|n| *n > 0).unwrap_or(d);
    let allow_private = cluster.config.node_setting(ALLOW_PRIVATE).is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let max_ms = cluster
        .config
        .node_setting(MAX_SYNC_DURATION)
        .and_then(|v| crate::units::TimeValue::parse(v, MAX_SYNC_DURATION).ok())
        .map(|t| t.as_millis())
        .filter(|m| *m > 0)
        .unwrap_or(DEFAULT_MAX_DURATION_MS);
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
        deadline: Instant::now() + Duration::from_millis(max_ms as u64),
        max_docs: setting(MAX_DOCS_PER_SYNC, DEFAULT_MAX_DOCS),
        beat: Instant::now(),
    };
    let cursor = connector.get("sync_cursor").cloned().unwrap_or(Value::Null);
    let advanced = rules::advanced(&filtering).cloned();
    let result = match service.as_str() {
        "postgresql" => postgres::sync(&values, &mut sink, full, &cursor, allow_private).await,
        crawler::SERVICE => crawler::sync(cid, &values, &mut sink, full, &cursor, advanced.as_ref(), allow_private).await,
        _ => s3::sync(&values, &mut sink, full, &cursor, advanced.as_ref(), allow_private).await,
    };
    let cursor = result.map_err(|s| match s {
        Stop::Failed(e) => Stop::Failed(values.scrub(&e)),
        other => other,
    })?;
    // A crawl purges by its own rules (`crawler::Crawl::purge`), never everything unseen.
    if full && !crawler {
        sink.delete_unseen().await?;
    }
    Ok(cursor)
}

/// A name as one segment of an internal URI's path or query.
fn enc(name: &str) -> String {
    crate::s3::uri_encode(name, true)
}

/// Creates the connector's index when it doesn't exist (with `create`'s body), never changing an
/// existing one's mappings.
async fn ensure_index(cluster: &Cluster, index: &str, create: Option<Value>) -> Result<(), Stop> {
    let path = format!("/{}", enc(index));
    match internal::call(cluster, "GET", &path, None).await {
        Ok(_) => Ok(()),
        Err(e) if e.kind == "index_not_found_exception" => match internal::call(cluster, "PUT", &path, create.as_ref()).await {
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

/// A page of documents on its way to `_bulk`: the NDJSON body itself, each document's source
/// as the text the source gave, charged to the cluster's request breaker as it grows. A page in
/// memory is its bytes, never a tree of its documents.
pub struct Page {
    body: String,
    /// Each document's source size (the job's volume).
    sizes: Vec<u64>,
    held: crate::breaker::Reservation,
}

impl Page {
    /// The body's bytes so far.
    pub fn bytes(&self) -> usize {
        self.body.len()
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
    /// When the sync fails for running too long ([`MAX_SYNC_DURATION`]).
    deadline: Instant,
    /// Documents it may read ([`MAX_DOCS_PER_SYNC`]).
    max_docs: u64,
    beat: Instant,
}

fn id_hash(id: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    id.hash(&mut h);
    h.finish()
}

impl Sink<'_> {
    /// Counts bytes read from the source (what `max_bytes_per_second` throttles): every source
    /// reports all it reads, listings, rows its rules exclude and protocol messages included.
    pub fn read(&mut self, bytes: u64) {
        self.counters.bytes += bytes;
    }

    /// A hold on the cluster's request breaker (for what a source keeps while it builds a
    /// document).
    pub fn reserve(&self) -> crate::breaker::Reservation {
        crate::breaker::Reservation::new(self.cluster.breakers.request(self.cluster, &self.cluster.state()))
    }

    /// A new, empty page.
    pub fn new_page(&self) -> Page {
        Page { body: String::new(), sizes: Vec::new(), held: self.reserve() }
    }

    /// Records a document the source couldn't hand over (too large).
    pub fn skip(&mut self) {
        self.counters.skipped += 1;
    }

    /// Adds document `id` whose source is the text `source` a source gave: it must be exactly
    /// one JSON object ([`json_scan::one_object`], which also puts it on one line), or it's
    /// counted as a failed document and left out, so no text can add a line to the `_bulk` body.
    /// It goes into the body as text, and the sync rules read only the fields they name from it
    /// ([`rules::pick`]), so no document becomes a tree here.
    pub fn push_text(&mut self, page: &mut Page, id: &str, source: &str) -> Result<(), Stop> {
        self.count()?;
        let source = match json_scan::one_object(source) {
            Ok(s) => s,
            Err(why) => {
                self.counters.failed += 1;
                self.counters.metadata.insert("last_failed_document".into(), capped(format!("[{id}] {why}")).into());
                return Ok(());
            }
        };
        let fields = self.rules.fields();
        let picked = if fields.is_empty() { Map::new() } else { rules::pick(&source, &fields) };
        if !self.rules.includes(&picked) {
            return Ok(());
        }
        self.append(page, id, &source)
    }

    /// Adds document `id` (a source that builds its documents itself).
    pub fn push_doc(&mut self, page: &mut Page, id: &str, doc: Map) -> Result<(), Stop> {
        self.count()?;
        if !self.rules.includes(&doc) {
            return Ok(());
        }
        self.append(page, id, &Value::Obj(doc).to_json())
    }

    /// Counts a document read, failing past [`MAX_DOCS_PER_SYNC`].
    fn count(&mut self) -> Result<(), Stop> {
        if self.counters.processed >= self.max_docs {
            return Err(Stop::Failed(format!("a sync reads at most {} documents ({MAX_DOCS_PER_SYNC})", self.max_docs)));
        }
        self.counters.processed += 1;
        Ok(())
    }

    fn append(&mut self, page: &mut Page, id: &str, source: &str) -> Result<(), Stop> {
        if self.full {
            if self.seen.len() >= MAX_SEEN {
                return Err(Stop::Failed(format!("a full sync indexes at most {MAX_SEEN} documents")));
            }
            self.seen.push(id_hash(id));
        }
        // The index is the request's ([`Sink::send`]), never a line's.
        let action = obj! { "index" => obj! { "_id" => id } }.to_json();
        let bytes = action.len() + source.len() + 2;
        page.held.add(bytes as u64, "connector page").map_err(|e| Stop::Failed(format!("indexing into [{}] failed: {}", self.index, e.reason())))?;
        page.sizes.push(source.len() as u64);
        page.body.reserve(bytes);
        page.body.push_str(&action);
        page.body.push('\n');
        page.body.push_str(source);
        page.body.push('\n');
        Ok(())
    }

    /// Indexes a page into the connector's index, then throttles and checks the job. The bulk
    /// request may only index into that index ([`internal::index_only`]), whatever its body
    /// holds, and charges its body to `inflight_requests`, as any `_bulk`'s.
    pub async fn send(&mut self, page: Page) -> Result<(), Stop> {
        let Page { body, sizes, held } = page;
        if !sizes.is_empty() {
            let uri = match &self.pipeline {
                Some(p) => format!("/{}/_bulk?pipeline={}", enc(&self.index), enc(p)),
                None => format!("/{}/_bulk", enc(&self.index)),
            };
            let resp = internal::index_only(self.index.clone(), internal::call_ndjson(self.cluster, "POST", &uri, body)).await.map_err(|e| Stop::Failed(format!("indexing into [{}] failed: {}", self.index, e.reason())))?;
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
        drop(held);
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

    /// Why the sync must stop now, if it must: canceled (or deleted), or past its time limit.
    fn stop_reason(&self) -> Option<Stop> {
        if !self.running() {
            return Some(Stop::Canceled);
        }
        (Instant::now() >= self.deadline).then(|| Stop::Failed(format!("the sync ran past its time limit ({MAX_SYNC_DURATION})")))
    }

    /// Resolves once the sync must stop ([`Sink::stop_reason`]): sources race their I/O
    /// against it.
    pub async fn stopped(&self) -> Stop {
        loop {
            if let Some(s) = self.stop_reason() {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Stops when the job was canceled (or deleted) or ran too long; writes its counters every
    /// [`HEARTBEAT`].
    pub async fn check(&mut self) -> Result<(), Stop> {
        if let Some(s) = self.stop_reason() {
            return Err(s);
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

    /// The native connectors a cluster held before roll ran syncs wait to be made its own (their
    /// credentials, stored in the clear, sealed where they lie); adopting one keeps none of them,
    /// and a connector made afterwards never waits.
    #[tokio::test]
    async fn connectors_from_before_roll_ran_syncs_wait_to_be_adopted() {
        let mut cfg = ClusterConfig::new("c");
        cfg.sealer = crate::seal::Sealer::from_secret("0123456789abcdef0123456789abcdef-upgrade");
        let c = Arc::new(Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap());
        call(&c, "PUT", "/_connector/x", r#"{"index_name":"content-x","is_native":true,"service_type":"postgresql"}"#).await;
        // As stored before roll ran syncs: the password in the clear, and no record of the upgrade.
        c.update(|st| {
            store::remove(st, EXISTING.0, EXISTING.1);
            let Some(Value::Obj(mut m)) = st.metadata.xpack.get(CONNECTORS, "x").cloned() else { panic!("no connector") };
            let Some(Value::Obj(cfg)) = m.get_mut("configuration") else { panic!("no configuration") };
            let Some(Value::Obj(e)) = cfg.get_mut("password") else { panic!("no password field") };
            e.insert("value".into(), "pw-from-before".into());
            store::put(st, CONNECTORS, "x", Value::Obj(m), 0).map(|_| ())
        })
        .await
        .unwrap();
        call(&c, "POST", "/_connector/_sync_job", r#"{"id":"x"}"#).await;
        let password = |c: &Cluster| c.state().metadata.xpack.get(CONNECTORS, "x").and_then(|m| m.pointer(&["configuration", "password", "value"])).map(Value::to_es_string).unwrap_or_default();

        adopt_existing(&c).await.unwrap();
        assert!(crate::seal::is_sealed(&password(&c)), "sealed where it lies");
        assert!(waits_for_adoption(&c, &c.state(), "x"));
        assert_eq!(next_due(&c, &c.state(), Some(&c.xpack.connector_syncs)), None, "its job isn't due");
        // Once per cluster: a connector made since doesn't wait.
        call(&c, "PUT", "/_connector/y", r#"{"index_name":"content-y","is_native":true,"service_type":"postgresql"}"#).await;
        adopt_existing(&c).await.unwrap();
        assert!(!waits_for_adoption(&c, &c.state(), "y"));
        // Nor on a cluster that had none: its first connector records that.
        let fresh = Arc::new(Cluster::open(ClusterConfig::new("fresh"), Arc::new(MemoryStore::new())).await.unwrap());
        adopt_existing(&fresh).await.unwrap();
        call(&fresh, "PUT", "/_connector/z", r#"{"index_name":"content-z","is_native":true,"service_type":"postgresql"}"#).await;
        adopt_existing(&fresh).await.unwrap();
        assert!(!waits_for_adoption(&fresh, &fresh.state(), "z"));
        // That node has no sealing key: a password written to it is refused, never stored.
        let refused = call(&fresh, "PUT", "/_connector/z/_configuration", r#"{"values":{"password":"pw-in-the-clear"}}"#).await;
        assert!(refused.pointer(&["error", "reason"]).and_then(Value::as_str).is_some_and(|r| r.contains("ROLL_SECRET_KEY")), "{refused:?}");
        // Marked not sensitive by the writer: refused all the same.
        let refused = call(&fresh, "PUT", "/_connector/z/_configuration", r#"{"configuration":{"password":{"label":"p","value":"pw-in-the-clear"}}}"#).await;
        assert!(refused.pointer(&["error", "reason"]).and_then(Value::as_str).is_some_and(|r| r.contains("ROLL_SECRET_KEY")), "{refused:?}");
        assert!(!fresh.state().metadata.xpack.get(CONNECTORS, "z").unwrap().to_json().contains("pw-in-the-clear"));

        // Adopted by a configuration write that gives no password: none is kept.
        call(&c, "PUT", "/_connector/x/_configuration", r#"{"values":{}}"#).await;
        assert!(!waits_for_adoption(&c, &c.state(), "x"));
        assert_eq!(password(&c), "");
        let x = call(&c, "GET", "/_connector/x", "").await;
        assert_eq!(x.get("status").and_then(Value::as_str), Some("needs_configuration"), "{x:?}");

        // A password an earlier build stored in the clear (marked not sensitive) is sealed on the
        // next load, and on a write that gives `********` back.
        let plant = |c: Arc<Cluster>| async move {
            c.update(|st| {
                let Some(Value::Obj(mut m)) = st.metadata.xpack.get(CONNECTORS, "x").cloned() else { panic!("no connector") };
                let Some(Value::Obj(cfg)) = m.get_mut("configuration") else { panic!("no configuration") };
                cfg.insert("password".into(), obj! { "label" => "p", "sensitive" => false, "value" => "pw-left-clear" });
                store::put(st, CONNECTORS, "x", Value::Obj(m), 0).map(|_| ())
            })
            .await
            .unwrap();
        };
        plant(c.clone()).await;
        adopt_existing(&c).await.unwrap();
        assert!(crate::seal::is_sealed(&password(&c)), "sealed on load");
        plant(c.clone()).await;
        call(&c, "PUT", "/_connector/x/_configuration", r#"{"values":{"password":"********"}}"#).await;
        assert!(crate::seal::is_sealed(&password(&c)), "sealed on a write");
    }

    /// A credential sealed on one cluster doesn't open on another (a branch's copy of the
    /// state), whatever its name: the seal is bound to the catalog id.
    #[tokio::test]
    async fn a_seal_opens_only_on_its_own_cluster() {
        let open = |catalog: &str| {
            let mut cfg = ClusterConfig::new("same-name");
            cfg.catalog_id = Some(catalog.into());
            cfg.sealer = crate::seal::Sealer::from_secret("0123456789abcdef0123456789abcdef-catalog");
            cfg
        };
        let a = Arc::new(Cluster::open(open("a"), Arc::new(MemoryStore::new())).await.unwrap());
        call(&a, "PUT", "/_connector/x", r#"{"index_name":"content-x","is_native":true,"service_type":"postgresql"}"#).await;
        call(&a, "PUT", "/_connector/x/_configuration", r#"{"values":{"password":"pw-a"}}"#).await;
        let stored = a.state().metadata.xpack.get(CONNECTORS, "x").cloned().unwrap();
        assert_eq!(config::Values::open(&a, &stored).ok().and_then(|v| v.text("password")).as_deref(), Some("pw-a"));
        let b = Arc::new(Cluster::open(open("b"), Arc::new(MemoryStore::new())).await.unwrap());
        assert!(config::Values::open(&b, &stored).is_err());
        // Whoever writes the configuration doesn't decide a password isn't sensitive.
        call(&a, "PUT", "/_connector/x/_configuration", r#"{"configuration":{"password":{"label":"p","value":"pw-unflagged"}}}"#).await;
        let shown = call(&a, "GET", "/_connector/x", "").await;
        assert_eq!(shown.pointer(&["configuration", "password", "value"]).and_then(Value::as_str), Some(config::REDACTED), "{shown:?}");
        assert!(!a.state().metadata.xpack.get(CONNECTORS, "x").unwrap().to_json().contains("pw-unflagged"));
    }

    /// A source's text can't add a line to the `_bulk` body: what isn't exactly one JSON object
    /// is a failed document, a pretty-printed one is put on one line, and the sync's bulk can
    /// only index into the connector's own index whatever its body says.
    #[tokio::test]
    async fn a_document_cant_smuggle_bulk_actions() {
        let c = Arc::new(Cluster::open(ClusterConfig::new("c"), Arc::new(MemoryStore::new())).await.unwrap());
        call(&c, "PUT", "/victim/_doc/1?refresh=true", r#"{"a":1}"#).await;
        call(&c, "PUT", "/content-x", "").await;
        let mut counters = Counters::default();
        let mut sink = Sink {
            cluster: &c,
            job: "none".into(),
            index: "content-x".into(),
            pipeline: None,
            rules: rules::Rules::default(),
            full: false,
            seen: Vec::new(),
            counters: &mut counters,
            docs_per_second: u64::MAX,
            bytes_per_second: u64::MAX,
            started: Instant::now(),
            deadline: Instant::now() + Duration::from_secs(60),
            max_docs: 100,
            beat: Instant::now(),
        };
        let mut page = sink.new_page();
        assert!(sink.push_text(&mut page, "smuggler", "{\"a\":1}\n{\"delete\":{\"_index\":\"victim\",\"_id\":\"1\"}}\n{\"x\":1}").is_ok());
        assert!(sink.push_text(&mut page, "pretty", "{\"a\":\n{\"delete\":{\"_index\":\"victim\",\"_id\":\"1\"}}\n}").is_ok());
        // The job doesn't exist, so the page's check says canceled; the page was indexed first.
        let _ = sink.send(page).await;
        assert_eq!((counters.failed, counters.indexed), (1, 1));
        call(&c, "POST", "/content-x/_refresh", "").await;
        let pretty = call(&c, "GET", "/content-x/_doc/pretty", "").await;
        assert_eq!(pretty.pointer(&["_source", "a", "delete", "_id"]).and_then(Value::as_str), Some("1"), "{pretty:?}");
        // Whatever a body holds, the sync's bulk can't act elsewhere.
        let body = "{\"delete\":{\"_index\":\"victim\",\"_id\":\"1\"}}\n".to_string();
        assert!(internal::index_only("content-x".into(), internal::call_ndjson(&c, "POST", "/content-x/_bulk", body)).await.is_err());
        let body = "{\"index\":{\"_index\":\"victim\",\"_id\":\"2\"}}\n{}\n".to_string();
        assert!(internal::index_only("content-x".into(), internal::call_ndjson(&c, "POST", "/content-x/_bulk", body)).await.is_err());
        // An action line naming anything but `_id` (`pipeline: _none` skips the index's own).
        let body = "{\"index\":{\"_id\":\"3\",\"pipeline\":\"_none\"}}\n{}\n".to_string();
        assert!(internal::index_only("content-x".into(), internal::call_ndjson(&c, "POST", "/content-x/_bulk", body)).await.is_err());
        let victim = call(&c, "GET", "/victim/_doc/1", "").await;
        assert_eq!(victim.get("found").and_then(Value::as_bool), Some(true), "{victim:?}");
        assert_eq!(call(&c, "GET", "/victim/_doc/2", "").await.get("found").and_then(Value::as_bool), Some(false));

        // The connector's pipeline can't move a document either, even to an index its data names.
        call(&c, "PUT", "/_ingest/pipeline/move", r#"{"processors":[{"set":{"field":"_index","value":"{{target}}"}}]}"#).await;
        let mut counters = Counters::default();
        let mut sink = Sink {
            cluster: &c,
            job: "none".into(),
            index: "content-x".into(),
            pipeline: Some("move".into()),
            rules: rules::Rules::default(),
            full: false,
            seen: Vec::new(),
            counters: &mut counters,
            docs_per_second: u64::MAX,
            bytes_per_second: u64::MAX,
            started: Instant::now(),
            deadline: Instant::now() + Duration::from_secs(60),
            max_docs: 100,
            beat: Instant::now(),
        };
        let mut page = sink.new_page();
        assert!(sink.push_text(&mut page, "mover", "{\"target\":\"elsewhere\"}").is_ok());
        let _ = sink.send(page).await;
        assert_eq!((counters.failed, counters.indexed), (1, 0));
        let elsewhere = call(&c, "GET", "/elsewhere/_doc/mover", "").await;
        assert_ne!(elsewhere.get("found").and_then(Value::as_bool), Some(true), "{elsewhere:?}");
        let created = call(&c, "GET", "/elsewhere", "").await;
        assert_eq!(created.pointer(&["error", "type"]).and_then(Value::as_str), Some("index_not_found_exception"), "the pipeline's target was created: {created:?}");
    }

    /// A sync running here whose statement outlasts the stale time isn't due again (the
    /// scheduler would wake every few seconds); in the unloaded view it's a dead node's job.
    #[tokio::test]
    async fn a_long_statement_of_a_running_sync_isnt_due_again() {
        let c = Arc::new(Cluster::open(ClusterConfig::new("c"), Arc::new(MemoryStore::new())).await.unwrap());
        call(&c, "PUT", "/_connector/x", r#"{"index_name":"content-x","is_native":true,"service_type":"s3"}"#).await;
        let job = call(&c, "POST", "/_connector/_sync_job", r#"{"id":"x"}"#).await.get("id").and_then(Value::as_str).unwrap().to_string();
        c.update(move |st| {
            let Some(Value::Obj(mut j)) = st.metadata.xpack.get(SYNC_JOBS, &job).cloned() else { panic!("no job") };
            j.insert("status".into(), "in_progress".into());
            j.insert("worker_hostname".into(), WORKER.into());
            j.insert("last_seen".into(), "2020-01-01T00:00:00Z".into());
            store::put(st, SYNC_JOBS, &job, Value::Obj(j), 0).map(|_| ())
        })
        .await
        .unwrap();
        assert!(next_due(&c, &c.state(), None).is_some_and(|t| t <= crate::time::now_millis()), "a dead node's job");
        assert!(c.xpack.connector_syncs.begin("x"));
        assert_eq!(next_due(&c, &c.state(), Some(&c.xpack.connector_syncs)), None);
    }
}
