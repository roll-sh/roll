//! roll code on many nodes (docs/code/cluster.md): every repo's requests run on the repo's
//! owner, a repo's background work starts and stops as its ownership moves, events and GitHub
//! deliveries reach the owner from any node, and the health check.
//!
//! On a single node (no [`code_cluster::Cluster`]) none of this does anything: every repo is
//! this node's and every request runs where it arrives.
//!
//! - **Routing** ([`route`], a layer on the API and git routers): a request naming a repo
//!   (`/v1/code/repos/{owner}/{repo}/…`, `/git/{owner}/{repo}.git/…`) or something of one
//!   (`/v1/code/changes/{id}/…`, `/v1/code/threads/{id}/…`, `/v1/code/work/{id}/…`) is
//!   dispatched by the repo's id, so a rename keeps its owner. The guard of a local request
//!   lives as long as its response body (git packs, event streams); an event stream ends when
//!   the repo starts moving, and its client reconnects to the new owner.
//! - **Duties**: on `Acquired`, the node forgets what it cached about the repo and resumes its
//!   work from the database (merge queue, reviewer jobs, GitHub import or mirror, the code index;
//!   code-preview's checks and previews through [`Code::subscribe_ownership`]). On `Lost` it
//!   stops them. Work in flight also stops on its own once the lease is gone
//!   ([`Code::while_owner`]), since `Lost` may come late (or never, for a killed process).
//! - **Internal routes** (`/internal/code/repos/{id}/…`, signed): events emitted on a node that
//!   doesn't own the repo, "resume this repo" kicks, and one repo's part of a GitHub delivery.
//!   Each is itself dispatched by the repo id, so it follows the repo if it moves meanwhile.

use crate::model::Event;
use crate::{Code, Config, Error, Result};
use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use code_cluster::{Cluster, ClusterConfig, Guard, Ownership};
use code_sql::OptionalExtension;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

/// A repo's ownership changed on this node: for whoever runs per-repo work outside this crate
/// (code-preview's checks and previews).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoOwnership {
    /// This node owns the repo now: resume its work.
    Acquired(String),
    /// It doesn't any more: stop.
    Lost(String),
}

/// What this node does for the repos it owns.
#[derive(Default)]
pub(crate) struct Duties {
    /// Tasks doing a repo's work (reviewer jobs, GitHub imports and mirrors, resuming), aborted
    /// when the repo moves away.
    tasks: Mutex<HashMap<String, Vec<AbortHandle>>>,
    watchers: Mutex<Vec<mpsc::UnboundedSender<RepoOwnership>>>,
    /// Events for repos another node owns, on their way there.
    events: std::sync::OnceLock<mpsc::UnboundedSender<Event>>,
    /// The node is shutting down: the health check fails so load balancers stop sending to it.
    draining: std::sync::atomic::AtomicBool,
    /// Repo-scoped requests this node ran itself (as the repo's owner).
    served: std::sync::atomic::AtomicU64,
}

/// Events sent to an owner in one request at most.
const EVENT_BATCH: usize = 256;
/// Tries a kick gets (backing off up to 5 s between them: about two minutes in all).
const KICK_ATTEMPTS: u32 = 30;
/// How often work in flight checks that the lease it started under is still held.
const OWNER_CHECK: Duration = Duration::from_millis(100);

/// A cluster needs a database and object storage every node shares.
pub(crate) fn check_config(config: &Config, objects: &pg_storage::Objects, control: &code_sql::Db) -> anyhow::Result<()> {
    if config.cluster_single_host {
        return Ok(());
    }
    if control.backend() != code_sql::Backend::Postgres {
        anyhow::bail!("a roll code cluster needs Postgres (--code-database): every node must use the same database");
    }
    if objects.description.starts_with("file://") {
        anyhow::bail!(
            "a roll code cluster needs S3 (--s3-bucket): {} is a local directory, which the other nodes can't read",
            objects.description
        );
    }
    Ok(())
}

/// Join the cluster and start following ownership.
pub(crate) fn join(code: &Arc<Code>, db: code_sql::Db, cfg: ClusterConfig) -> anyhow::Result<()> {
    let rt = tokio::runtime::Handle::try_current().map_err(|_| anyhow::anyhow!("a roll code cluster node needs a Tokio runtime"))?;
    let cluster = match rt.runtime_flavor() {
        tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(|| Cluster::join(db, cfg))?,
        _ => Cluster::join(db, cfg)?,
    };
    let ownership = cluster.subscribe();
    code.cluster.set(cluster).map_err(|_| anyhow::anyhow!("already part of a cluster"))?;
    let (tx, rx) = mpsc::unbounded_channel();
    let _ = code.duties.events.set(tx);
    rt.spawn(follow_ownership(Arc::downgrade(code), ownership));
    rt.spawn(forward_events(Arc::downgrade(code), rx));
    rt.spawn(release_idle_repos(Arc::downgrade(code)));
    Ok(())
}

/// Give up repos nobody has used for `Config::repo_idle_release` and that have no work waiting
/// (their leases would otherwise be renewed forever). The next request takes them again.
async fn release_idle_repos(code: Weak<Code>) {
    let idle = match code.upgrade() {
        Some(c) => c.config.repo_idle_release,
        None => return,
    };
    let every = (idle / 4).clamp(Duration::from_millis(50), Duration::from_secs(60));
    loop {
        tokio::time::sleep(every).await;
        let Some(code) = code.upgrade() else { return };
        let Some(cluster) = code.cluster().cloned() else { return };
        for key in cluster.idle(idle).into_iter().filter(|k| !k.starts_with("singleton:")) {
            let (c, code) = (cluster.clone(), code.clone());
            let released = tokio::task::spawn_blocking(move || c.release_if_idle(&key, idle, || !code.repo_busy(&key))).await;
            if let Err(e) = released {
                tracing::warn!(error = %e, "releasing an idle repo");
            }
        }
    }
}

async fn follow_ownership(code: Weak<Code>, mut rx: mpsc::UnboundedReceiver<Ownership>) {
    while let Some(ev) = rx.recv().await {
        let Some(code) = code.upgrade() else { return };
        match ev {
            Ownership::Acquired { key, epoch } if !key.starts_with("singleton:") => {
                tracing::debug!(repo = %key, epoch, "took a repo");
                code.acquired(&key);
            }
            Ownership::Lost { key } if !key.starts_with("singleton:") => {
                tracing::debug!(repo = %key, "gave a repo up");
                code.lost(&key);
            }
            _ => {}
        }
    }
}

impl Code {
    // ── ownership ──

    fn acquired(self: &Arc<Self>, repo_id: &str) {
        self.forget_repo(repo_id);
        self.tell_watchers(RepoOwnership::Acquired(repo_id.into()));
        let me = self.clone();
        let id = repo_id.to_string();
        let t = tokio::spawn(async move { me.resume_repo(&id).await });
        self.track(repo_id, t.abort_handle());
    }

    fn lost(&self, repo_id: &str) {
        self.tell_watchers(RepoOwnership::Lost(repo_id.into()));
        self.stop_queue(repo_id);
        let tasks = self.duties.tasks.lock().remove(repo_id).unwrap_or_default();
        for t in tasks {
            t.abort();
        }
        crate::github::stop(self, repo_id);
        self.forget_repo(repo_id);
        // A node keeps no index of a repo it doesn't own: the next owner builds its own, and a
        // copy left here would go stale (or outlive the repo). Not at once: a node that fenced
        // itself through a slow database often gets the repo straight back.
        if self.ai.index.is_some()
            && let (Some(me), Some(c), Ok(rt)) = (self.me.upgrade(), self.cluster(), tokio::runtime::Handle::try_current())
        {
            let (id, grace) = (repo_id.to_string(), c.config().lease_ttl * 2);
            let me = Arc::downgrade(&me);
            rt.spawn(async move {
                tokio::time::sleep(grace).await;
                if let Some(code) = me.upgrade()
                    && !code.owns_repo(&id)
                    && let Some(i) = &code.ai.index
                {
                    i.remove(&id);
                }
            });
        }
    }

    /// Whether a repo has work its owner must not walk away from: merge-queue entries, reviewer
    /// jobs, checks queued or running, previews running, promised webhook events, or a GitHub
    /// connection (its mirror). On any doubt (an error), busy.
    pub(crate) fn repo_busy(&self, repo_id: &str) -> bool {
        let r = match self.db.repo_by_id(repo_id) {
            Ok(Some(r)) => r,
            // Deleted: nothing to keep.
            Ok(None) => return false,
            Err(_) => return true,
        };
        if crate::github::link_of(&r).is_some_and(|l| l.connected) {
            return true;
        }
        // GitHub delivery parts waiting for it (the table exists once the App is on).
        if self.github_hub().is_some() {
            match self.db.sql().query_row("SELECT EXISTS (SELECT 1 FROM github_inbox WHERE repo_id = ? AND state = 'pending')", [repo_id], |r| r.get::<_, bool>(0)) {
                Ok(false) => {}
                _ => return true,
            }
        }
        let busy = self.db.sql().query_row(
            "SELECT (EXISTS (SELECT 1 FROM queue_entries WHERE repo_id = ?1 AND state NOT IN ('ejected', 'landed'))) \
             OR (EXISTS (SELECT 1 FROM ai_jobs WHERE repo_id = ?1 AND state IN ('queued', 'running'))) \
             OR (EXISTS (SELECT 1 FROM checks k JOIN changes c ON c.id = k.change_id \
                         WHERE c.repo_id = ?1 AND c.state IN ('open', 'queued', 'conflict') AND k.status IN ('queued', 'running'))) \
             OR (EXISTS (SELECT 1 FROM changes WHERE repo_id = ?1 AND preview LIKE '%\"state\":\"running\"%')) \
             OR (EXISTS (SELECT 1 FROM webhook_outbox WHERE repo_id = ?1))",
            [repo_id],
            |r| r.get::<_, bool>(0),
        );
        busy.unwrap_or(true)
    }

    /// A deleted repo: its owner gives the lease up for good once the request deleting it is
    /// done (its work stops with `Lost`); the row is deleted rather than left for a sweep.
    pub(crate) fn retire_repo(&self, repo_id: &str) {
        if let Some(c) = self.cluster() {
            c.retire(repo_id);
        }
    }

    /// Indexes on disk for repos this node doesn't own: on a single node, those of repos that no
    /// longer exist; in a cluster, all of them (a node starts owning nothing, and a node that
    /// was killed never got to drop the ones it lost).
    pub(crate) fn clean_indexes(&self) {
        let Some(index) = &self.ai.index else { return };
        for repo in index.on_disk() {
            let stale = self.config.cluster.is_some() || matches!(self.db.repo_by_id(&repo), Ok(None));
            if stale {
                tracing::info!(repo = %repo, "removing a code index this node doesn't own");
                index.remove(&repo);
            }
        }
    }

    fn tell_watchers(&self, ev: RepoOwnership) {
        self.duties.watchers.lock().retain(|w| w.send(ev.clone()).is_ok());
    }

    /// Ownership changes of repos on this node (nothing on a single node, which owns them all).
    /// Starts with `Acquired` for every repo owned now. A watcher may see a repo acquired twice,
    /// or lost without having seen it acquired: handling must be idempotent.
    pub fn subscribe_ownership(&self) -> mpsc::UnboundedReceiver<RepoOwnership> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut watchers = self.duties.watchers.lock();
        if let Some(c) = self.cluster() {
            for (k, _) in c.owned() {
                if !k.starts_with("singleton:") {
                    let _ = tx.send(RepoOwnership::Acquired(k));
                }
            }
            watchers.push(tx);
        }
        rx
    }

    /// Abort `task` if the repo moves to another node (a no-op on a single node).
    pub(crate) fn track(&self, repo_id: &str, task: AbortHandle) {
        if self.cluster().is_none() {
            return;
        }
        let mut tasks = self.duties.tasks.lock();
        let v = tasks.entry(repo_id.to_string()).or_default();
        v.retain(|t| !t.is_finished());
        v.push(task);
    }

    /// Resume a repo's work from the database on its owner: the merge queue (its batch in flight
    /// is recovered by the new worker), reviewer jobs (those that were running on a node that's
    /// gone start over), the GitHub import or mirror, and the code index. Idempotent.
    pub(crate) async fn resume_repo(self: &Arc<Self>, repo_id: &str) {
        if !self.owns_repo(repo_id) {
            return;
        }
        let r = match self.db.repo_by_id(repo_id) {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(e) => return tracing::warn!(repo = %repo_id, error = %e, "resuming a repo's work"),
        };
        match self.db.active_entries(repo_id) {
            Ok(e) if !e.is_empty() => self.kick(repo_id),
            Ok(_) => {}
            Err(e) => tracing::warn!(repo = %repo_id, error = %e, "resuming a merge queue"),
        }
        // GitHub delivery parts recorded for it (github::resume starts them for connected
        // repos; a part can outlive the connection, and is then dropped by its handler).
        if crate::github::link_of(&r).is_none_or(|l| !l.connected) {
            crate::github::inbox::process_due(self, repo_id).await;
        }
        match self.requeue_running_ai_jobs_of(repo_id) {
            Ok(0) => {}
            Ok(n) => tracing::info!(repo = %r.full_name(), jobs = n, "restarting reviews that were running on another node"),
            Err(e) => tracing::warn!(repo = %repo_id, error = %e, "requeueing reviews"),
        }
        self.ai.wake.notify_one();
        crate::github::resume(self, &r);
        if let Some(index) = &self.ai.index {
            // An index this node built before catches up; a repo whose reviewer needs one
            // (deep reviews) builds it in the background; others build on first use.
            let built = index.repo(&r.id).ok().and_then(|x| x.head().ok().flatten()).is_some();
            if (built || self.reviewer_settings(&r).await.0.enabled)
                && let Err(e) = self.index_ready(&r, Duration::ZERO).await
            {
                tracing::warn!(repo = %r.full_name(), error = %e, "resuming the codebase index");
            }
        }
    }

    /// Repo-scoped requests this node ran as the repo's owner (diagnostics and tests).
    pub fn repo_requests_served(&self) -> u64 {
        self.duties.served.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Drop everything this node cached about a repo (it was taken over: another node may have
    /// changed it meanwhile), including whatever webhooks.rs keeps (`Code::forget_repo_webhooks`).
    pub fn forget_repo(&self, repo_id: &str) {
        self.settings_cache.lock().remove(repo_id);
        self.review_configs.lock().remove(repo_id);
        self.landing_changes.retain(|k| k.0 != repo_id);
        self.languages.retain(|k| k.0 != repo_id);
        self.contributors.retain(|k| k.0 != repo_id);
        self.last_changes.retain(|k| k.0 != repo_id);
        self.readmes.retain(|k| k.0 != repo_id);
        self.blames.retain(|k| k.0 != repo_id);
        self.baselines.retain(|k| k.0 != repo_id);
        self.review_facts.retain(|k| k.0 != repo_id);
        self.forget_repo_webhooks(repo_id);
    }

    /// Run `fut` while this node owns the repo, holding a guard so a handoff waits for it (up to
    /// the cluster's `handoff_timeout`). `None` if the repo isn't ours, or stopped being ours
    /// (the work is dropped at its next await: its writes must be safe to leave half done, as
    /// for a crash). On a single node, just `fut`.
    pub(crate) async fn while_owner<T>(&self, repo_id: &str, fut: impl Future<Output = T>) -> Option<T> {
        match self.cluster() {
            None => Some(fut.await),
            Some(c) => {
                let g = c.guard(repo_id)?;
                self.while_owner_guarded(repo_id, &g, fut).await
            }
        }
    }

    /// [`Code::while_owner`] under a guard already held.
    pub(crate) async fn while_owner_guarded<T>(&self, repo_id: &str, g: &Guard, fut: impl Future<Output = T>) -> Option<T> {
        let Some(c) = self.cluster() else { return Some(fut.await) };
        let epoch = g.epoch();
        let mut fut = std::pin::pin!(fut);
        let mut tick = tokio::time::interval(OWNER_CHECK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                v = &mut fut => return Some(v),
                _ = tick.tick() => {
                    // Held through a handoff (which waits for this guard), gone once it's lost,
                    // fenced, or the process stopped taking part.
                    if c.epoch(repo_id) != Some(epoch) {
                        return None;
                    }
                }
            }
        }
    }

    /// Reviewer jobs that may run now, of the repos this node owns.
    pub(crate) fn due_ai_jobs_here(&self, n: usize) -> Result<Vec<crate::ai::store::Job>> {
        let Some(c) = self.cluster() else { return Ok(self.db.due_ai_jobs(n)?) };
        let repos: Vec<String> = c.owned().into_iter().map(|(k, _)| k).filter(|k| !k.starts_with("singleton:")).collect();
        let mut out = Vec::new();
        for chunk in repos.chunks(200) {
            let marks = vec!["?"; chunk.len()].join(", ");
            let mut params: Vec<code_sql::Value> = vec![code_sql::Value::Int(crate::db::now_ms())];
            params.extend(chunk.iter().map(|r| code_sql::Value::Text(r.clone())));
            params.push(code_sql::Value::Int(crate::db::limit(n)));
            out.extend(self.db.sql().query(
                &format!(
                    "SELECT {} FROM ai_jobs WHERE state = 'queued' AND run_at <= ? AND repo_id IN ({marks}) \
                     AND NOT EXISTS (SELECT 1 FROM ai_jobs o WHERE o.change_id = ai_jobs.change_id AND o.state = 'running') ORDER BY created_at, rowid LIMIT ?",
                    crate::ai::store::JOB_COLS
                ),
                params,
                crate::ai::store::job_row,
            )?);
        }
        out.sort_by_key(|j| j.created_at);
        out.truncate(n);
        Ok(out)
    }

    /// A repo's reviewer jobs that were running when it moved here start over (unless they're
    /// this node's own).
    fn requeue_running_ai_jobs_of(&self, repo_id: &str) -> Result<usize> {
        Ok(self.db.requeue_ai_jobs_of(repo_id, &self.node_name())?)
    }

    /// Jobs of the repos this node owns whose claim lapsed (their node stopped renewing it).
    pub(crate) fn requeue_lapsed_ai_jobs_here(&self) -> Result<usize> {
        let Some(c) = self.cluster() else { return Ok(self.db.requeue_lapsed_ai_jobs(None)?) };
        let repos: Vec<String> = c.owned().into_iter().map(|(k, _)| k).filter(|k| !k.starts_with("singleton:")).collect();
        if repos.is_empty() {
            return Ok(0);
        }
        Ok(self.db.requeue_lapsed_ai_jobs(Some(&repos))?)
    }

    /// Who this node is in claims it writes: its cluster node id, or `local` on a single node.
    pub(crate) fn node_name(&self) -> String {
        self.cluster().map_or_else(|| crate::ai::store::LOCAL.to_string(), |c| c.node_id().to_string())
    }

    // ── to the owner ──

    /// Leave the cluster gracefully (SIGTERM): fail the health check, hand every repo to the
    /// other nodes (each handoff waits for the requests and batches in flight), then leave. The
    /// API should keep serving meanwhile: requests for repos already handed off are forwarded.
    pub async fn drain(&self, timeout: Duration) {
        let Some(c) = self.cluster() else { return };
        self.duties.draining.store(true, std::sync::atomic::Ordering::SeqCst);
        c.drain(timeout).await;
    }
    /// Pass an event to the repo's owner, whose feed (`/events`) its subscribers read. In order
    /// per repo; dropped (with a log line) if no node can take it for a few seconds, like an
    /// event sent while nobody listens.
    pub(crate) fn forward_event(&self, ev: Event) {
        if let Some(tx) = self.duties.events.get() {
            let _ = tx.send(ev);
        }
    }

    /// Have the repo's owner resume its work (a node that doesn't own it noticed something the
    /// owner must act on: a queue to run, a GitHub repo connected, delivery parts recorded for
    /// it). In the background, retried until some node takes it (taking the repo over resumes
    /// its work too, so a kick only has to reach whoever owns it next): the work itself is in the
    /// database, the kick only makes it start now.
    pub(crate) fn kick_owner(&self, repo_id: &str) {
        let (Some(me), Ok(rt)) = (self.me.upgrade(), tokio::runtime::Handle::try_current()) else { return };
        let id = repo_id.to_string();
        let weak = Arc::downgrade(&me);
        drop(me);
        rt.spawn(async move {
            let mut wait = Duration::from_millis(100);
            for attempt in 0..KICK_ATTEMPTS {
                let Some(me) = weak.upgrade() else { return };
                let resp = me.to_owner(&id, "kick", Bytes::new()).await;
                if resp.status().is_success() {
                    return;
                }
                if attempt + 1 == KICK_ATTEMPTS {
                    tracing::warn!(repo = %id, status = %resp.status(), "waking a repo's owner failed; its next owner resumes the work");
                    return;
                }
                drop(me);
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(5));
            }
        });
    }

    /// One repo's part of a GitHub delivery (recorded as `part`, claimed until `claimed_until`),
    /// handled on the repo's owner.
    pub(crate) async fn github_on_owner(&self, repo_id: &str, part: &str, claimed_until: i64) -> crate::github::inbox::Outcome {
        use crate::github::inbox::Outcome;
        let body = Bytes::from(json!({ "part": part, "claimed_until": claimed_until }).to_string());
        let resp = self.to_owner(repo_id, "github", body).await;
        let status = resp.status();
        if status.is_success() {
            return Outcome::Done;
        }
        // Nothing ran, or the owner went away mid-way: the part stays recorded.
        if matches!(status, StatusCode::SERVICE_UNAVAILABLE | StatusCode::BAD_GATEWAY | StatusCode::GATEWAY_TIMEOUT | StatusCode::LOOP_DETECTED) {
            return Outcome::Unavailable;
        }
        let text = axum::body::to_bytes(resp.into_body(), 64 << 10).await.map(|b| String::from_utf8_lossy(&b).to_string()).unwrap_or_default();
        Outcome::Failed(match status {
            StatusCode::NOT_FOUND => Error::NotFound(text.trim().to_string()),
            StatusCode::BAD_REQUEST => Error::Invalid(text.trim().to_string()),
            _ => Error::Internal(anyhow::anyhow!("the repo's owner answered {status}: {}", text.trim())),
        })
    }

    /// `POST /internal/code/repos/{id}/{what}`, run where the repo lives: here, or on its owner.
    async fn to_owner(&self, repo_id: &str, what: &'static str, body: Bytes) -> Response {
        let (Some(cluster), Some(me)) = (self.cluster().cloned(), self.me.upgrade()) else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        let b = Request::builder().method(Method::POST).uri(format!("/internal/code/repos/{repo_id}/{what}")).header(header::CONTENT_TYPE, "application/json");
        let Ok(req) = b.body(Body::from(body)) else { return StatusCode::BAD_REQUEST.into_response() };
        let id = repo_id.to_string();
        cluster.dispatch(repo_id, req, move |req, guard| internal_local(me, id, what, req, guard)).await
    }

    // ── routing ──

    /// The repo a request is for, by id (renames followed); `None` for global routes and for
    /// names and ids that don't exist (they answer 404 wherever they land).
    fn route_key(&self, t: &Target) -> Option<String> {
        let one = |sql: &str, id: &str| -> Option<String> { self.db.sql().query_row(sql, [id], |r| r.get(0)).optional().ok().flatten() };
        match t {
            Target::Repo(owner, name) => match self.find_repo(owner, name) {
                Ok(r) => Some(r.id),
                Err(Error::Moved(to)) => {
                    let (o, n) = to.split_once('/')?;
                    self.find_repo(o, n).ok().map(|r| r.id)
                }
                Err(_) => None,
            },
            Target::Change(id) => one("SELECT repo_id FROM changes WHERE id = ?", id),
            Target::Thread(id) => one("SELECT repo_id FROM review_threads WHERE id = ?", id),
            Target::Work(id) => one("SELECT repo_id FROM work_items WHERE id = ?", id),
        }
    }

    /// `GET /v1/code/health`: whether this node can serve (the database answers; in a cluster,
    /// the node is a member holding valid leases).
    pub(crate) async fn health(self: &Arc<Self>) -> Response {
        let me = self.clone();
        let db = tokio::time::timeout(Duration::from_secs(5), tokio::task::spawn_blocking(move || me.db.sql().now_ms())).await;
        let database = matches!(db, Ok(Ok(Ok(_))));
        let draining = self.duties.draining.load(std::sync::atomic::Ordering::SeqCst);
        let cluster = self.cluster().map(|c| {
            let repos = c.owned().iter().filter(|(k, _)| !k.starts_with("singleton:")).count();
            json!({ "node": c.node_id(), "healthy": c.is_healthy(), "draining": draining, "nodes": c.nodes().len(), "repos": repos })
        });
        let ok = database && !draining && self.cluster().is_none_or(|c| c.is_healthy());
        let status = if ok { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
        let body = json!({ "status": if ok { "ok" } else { "unavailable" }, "database": database, "cluster": cluster });
        let mut r = (status, Json(body)).into_response();
        if !ok {
            r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        r
    }
}

/// What a repo-scoped route names.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    Repo(String, String),
    Change(String),
    Thread(String),
    Work(String),
}

fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// Which repo-scoped thing a path names, if any.
fn classify(path: &str) -> Option<Target> {
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let t = match segs.as_slice() {
        ["v1", "code", "repos", owner, repo, ..] => Target::Repo(decode(owner), decode(repo)),
        ["v1", "code", "changes", id, ..] => Target::Change(decode(id)),
        ["v1", "code", "threads", id, ..] => Target::Thread(decode(id)),
        ["v1", "code", "work", id, ..] => Target::Work(decode(id)),
        ["git", owner, repo, ..] => {
            let repo = decode(repo);
            Target::Repo(decode(owner), repo.strip_suffix(".git").unwrap_or(&repo).to_string())
        }
        _ => return None,
    };
    match &t {
        Target::Repo(o, r) if o.is_empty() || r.is_empty() => None,
        Target::Change(id) | Target::Thread(id) | Target::Work(id) if id.is_empty() => None,
        _ => Some(t),
    }
}

/// Middleware on the API and git routers (in a cluster): run each repo-scoped request on the
/// repo's owner. Global routes run where they arrive.
///
/// The node a client reached keeps trying while the repo has no owner it can reach (being
/// handed off, or its owner died and the lease hasn't expired), for about a lease's length, so
/// moves and failovers don't fail requests whose body it could keep (up to 4 MiB; a streamed git
/// push gets one try). An answer of "unavailable" means nothing ran, so trying again is safe;
/// so does a 508, which only comes from nodes that disagree for a moment about where a repo that
/// is moving belongs (views of the live nodes converge within a heartbeat).
pub(crate) async fn route(State(code): State<Arc<Code>>, req: Request, next: Next) -> Response {
    let Some(cluster) = code.cluster().cloned() else { return next.run(req).await };
    let Some(key) = classify(req.uri().path()).and_then(|t| code.route_key(&t)) else { return next.run(req).await };
    if let Some(from) = req.extensions().get::<code_cluster::Forwarded>().map(|f| f.node.clone()) {
        // Never send a request back where it came from: the two nodes disagree about the repo
        // right now. Unavailable makes the first node try again shortly.
        if let Ok(code_cluster::Route::Remote(n)) = cluster.route_async(&key).await
            && n.id == from
        {
            cluster.forget(&key);
            return code_cluster::unavailable(Duration::from_millis(50));
        }
    }
    let first_node = code_cluster::hops(&req) == 0;
    let small = http_body::Body::size_hint(req.body()).exact().is_some_and(|n| n <= RETRY_BODY);
    let local = move |code: Arc<Code>, next: Next| {
        move |req: Request, guard: Guard| async move {
            code.duties.served.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            hold(next.run(req).await, guard)
        }
    };
    if !first_node || !small {
        return cluster.dispatch(&key, req, local(code.clone(), next)).await;
    }
    let (parts, body) = req.into_parts();
    let body = match axum::body::to_bytes(body, RETRY_BODY as usize).await {
        Ok(b) => b,
        Err(e) => return Error::Invalid(format!("reading the request body: {e}")).into_response(),
    };
    let cfg = cluster.config();
    let deadline = tokio::time::Instant::now() + cfg.lease_ttl + cfg.sweep_every * 2 + Duration::from_secs(1);
    loop {
        let req = Request::from_parts(parts.clone(), Body::from(body.clone()));
        let resp = cluster.dispatch(&key, req, local(code.clone(), next.clone())).await;
        let wait = match unavailable_for(&resp) {
            Some(w) => w,
            None if resp.status() == StatusCode::LOOP_DETECTED => {
                cluster.forget(&key);
                Duration::from_millis(50)
            }
            None => return resp,
        };
        if tokio::time::Instant::now() + wait >= deadline {
            return resp;
        }
        tokio::time::sleep(wait.min(cfg.max_retry_wait).max(Duration::from_millis(20))).await;
    }
}

/// Request bodies up to this size are kept so the request can be routed again.
const RETRY_BODY: u64 = 4 << 20;

/// How long to wait before routing again, if `resp` says the repo can't be served right now.
fn unavailable_for(resp: &Response) -> Option<Duration> {
    if resp.status() != StatusCode::SERVICE_UNAVAILABLE {
        return None;
    }
    let ms = resp.headers().get("x-roll-code-unavailable")?.to_str().ok()?.parse::<u64>().ok()?;
    Some(Duration::from_millis(ms))
}

/// Keep `guard` for as long as the response body is being sent. An event stream also ends when
/// the repo starts moving to another node (its client reconnects there).
fn hold(resp: Response, guard: Guard) -> Response {
    let stream = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("text/event-stream"));
    let (parts, inner) = resp.into_parts();
    let end: Option<Pin<Box<dyn Future<Output = ()> + Send>>> = if stream { Some(Box::pin(guard.ending())) } else { None };
    Response::from_parts(parts, Body::new(Held { inner, end, _guard: guard }))
}

struct Held {
    inner: Body,
    end: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    _guard: Guard,
}

impl http_body::Body for Held {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<std::result::Result<http_body::Frame<Bytes>, axum::Error>>> {
        if let Some(end) = self.end.as_mut()
            && end.as_mut().poll(cx).is_ready()
        {
            return Poll::Ready(None);
        }
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

// ── internal routes ──

/// `/internal/code/repos/{id}/{events,kick,github}`: signed requests from other nodes.
pub(crate) fn internal_routes(code: Arc<Code>, cluster: Arc<Cluster>) -> Router {
    Router::new()
        .route("/internal/code/repos/{id}/events", post(internal_events))
        .route("/internal/code/repos/{id}/kick", post(internal_kick))
        .route("/internal/code/repos/{id}/github", post(internal_github))
        .layer(axum::extract::DefaultBodyLimit::max(32 << 20))
        .route_layer(axum::middleware::from_fn_with_state(cluster, code_cluster::require_signed))
        .with_state(code)
}

async fn internal(code: Arc<Code>, id: String, what: &'static str, req: Request) -> Response {
    let Some(cluster) = code.cluster().cloned() else { return StatusCode::NOT_FOUND.into_response() };
    let key = id.clone();
    cluster.dispatch(&key, req, move |req, guard| internal_local(code, id, what, req, guard)).await
}

async fn internal_events(State(code): State<Arc<Code>>, Path(id): Path<String>, req: Request) -> Response {
    internal(code, id, "events", req).await
}

async fn internal_kick(State(code): State<Arc<Code>>, Path(id): Path<String>, req: Request) -> Response {
    internal(code, id, "kick", req).await
}

async fn internal_github(State(code): State<Arc<Code>>, Path(id): Path<String>, req: Request) -> Response {
    internal(code, id, "github", req).await
}

/// An internal request, on the repo's owner.
async fn internal_local(code: Arc<Code>, repo_id: String, what: &'static str, req: Request, guard: Guard) -> Response {
    let body = match axum::body::to_bytes(req.into_body(), 32 << 20).await {
        Ok(b) => b,
        Err(e) => return Error::Invalid(format!("reading the body: {e}")).into_response(),
    };
    let resp = match what {
        "events" => {
            let evs: Vec<Value> = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => return Error::Invalid(format!("invalid JSON: {e}")).into_response(),
            };
            for v in evs {
                let Some(kind) = v["kind"].as_str().and_then(|k| crate::webhooks::EVENTS.iter().find(|e| **e == k)) else { continue };
                code.emit_local(Event { repo_id: repo_id.clone(), kind, data: v["data"].clone() });
            }
            StatusCode::NO_CONTENT.into_response()
        }
        "kick" => {
            code.resume_repo(&repo_id).await;
            StatusCode::NO_CONTENT.into_response()
        }
        "github" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let (Some(part), Some(claimed_until)) = (v["part"].as_str(), v["claimed_until"].as_i64()) else {
                return Error::Invalid("a GitHub delivery part needs part and claimed_until".into()).into_response();
            };
            match crate::github::inbox::process(&code, &repo_id, part, claimed_until).await {
                Ok(()) => StatusCode::NO_CONTENT.into_response(),
                Err(e) => e.into_response(),
            }
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    };
    drop(guard);
    resp
}

/// Send events for repos this node doesn't own to their owners: one request per repo per batch,
/// in order.
async fn forward_events(code: Weak<Code>, mut rx: mpsc::UnboundedReceiver<Event>) {
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        while batch.len() < EVENT_BATCH {
            match rx.try_recv() {
                Ok(e) => batch.push(e),
                Err(_) => break,
            }
        }
        let Some(code) = code.upgrade() else { return };
        let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
        for e in batch {
            let v = json!({ "kind": e.kind, "data": e.data });
            match groups.iter_mut().find(|(r, _)| *r == e.repo_id) {
                Some((_, g)) => g.push(v),
                None => groups.push((e.repo_id, vec![v])),
            }
        }
        for (repo, evs) in groups {
            let body = Bytes::from(serde_json::to_vec(&evs).unwrap_or_default());
            for attempt in 0..4u64 {
                let resp = code.to_owner(&repo, "events", body.clone()).await;
                if resp.status().is_success() {
                    break;
                }
                if attempt == 3 {
                    tracing::warn!(repo = %repo, events = evs.len(), status = %resp.status(), "no node took a repo's events; dropping them");
                } else {
                    tokio::time::sleep(Duration::from_millis(250 * (attempt + 1))).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_scoped_paths_are_classified() {
        let repo = |o: &str, r: &str| Some(Target::Repo(o.into(), r.into()));
        assert_eq!(classify("/v1/code/repos/acme/api"), repo("acme", "api"));
        assert_eq!(classify("/v1/code/repos/acme/api/tree/main/src"), repo("acme", "api"));
        assert_eq!(classify("/v1/code/repos/acme/a%2Eb/refs"), repo("acme", "a.b"));
        assert_eq!(classify("/git/acme/api.git/info/refs"), repo("acme", "api"));
        assert_eq!(classify("/git/acme/api/git-upload-pack"), repo("acme", "api"));
        assert_eq!(classify("/v1/code/changes/chg_1/land"), Some(Target::Change("chg_1".into())));
        assert_eq!(classify("/v1/code/changes/chg_1"), Some(Target::Change("chg_1".into())));
        assert_eq!(classify("/v1/code/threads/th_1/resolve"), Some(Target::Thread("th_1".into())));
        assert_eq!(classify("/v1/code/work/wi_1/heartbeat"), Some(Target::Work("wi_1".into())));
        for global in [
            "/v1/code",
            "/v1/code/repos",
            "/v1/code/repos/acme",
            "/v1/code/repos//x",
            "/v1/code/owners/acme",
            "/v1/code/names/acme/api",
            "/v1/code/mcp",
            "/v1/code/pricing",
            "/v1/code/review-apps/app_1",
            "/v1/code/github/webhook",
            "/v1/code/health",
            "/v1/code/changes/",
            "/git/acme",
        ] {
            assert_eq!(classify(global), None, "{global}");
        }
    }
}
