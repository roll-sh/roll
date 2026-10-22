//! The merge queue (`land_policy = "queue"`): nothing lands on the default branch untested.
//!
//! Changes wait in two lanes (`urgent` before `normal`, FIFO within each). Each repo has one
//! worker that takes up to `batch_size` waiting changes, rebases them one after another onto
//! the default branch's head on a scratch ref (`refs/roll/queue`, with the lander's merge
//! semantics, so a textual conflict ejects that change right away) and runs the queue's
//! checks once, on the tip. Green: the whole batch lands with one compare-and-swap
//! ([`Store::advance`](code_store::Store::advance)). Red: the batch is bisected (each prefix
//! of it is already a tested-ready tree) to the first change that breaks it; the changes
//! before it land, it's ejected with the failure, and the rest go back in line. A check only
//! counts against a batch if it passes on the head the batch is built on, so a broken default
//! branch doesn't eject everything (and its fix can still get through).
//!
//! One batch is in flight per repo (speculative depth 1). The batch size adapts: halved after
//! a failure, doubled after a success, never above `[queue] batch_size`. Dequeuing, revising
//! or closing a change in the batch makes the worker throw the batch away and rebuild it. A
//! restart re-tests whatever was in flight; a batch that was landing when the process died is
//! recognized as landed if its tip is on the default branch.

use crate::db::{EntryRow, now};
use crate::model::*;
use crate::service::short_ref;
use crate::settings::{FILE, Settings};
use crate::{Caller, Code, Error, Result, cs_id, new_id};
use code_store::{ChangesetId, Conflict, Expect, LandError};
use futures::future::join_all;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{info, warn};

/// Where batches are built: one scratch ref per repo, never listed or counted.
pub const SCRATCH: &str = "refs/roll/queue";
/// A worker with nothing to do for this long stops; enqueueing starts a new one.
const IDLE: Duration = Duration::from_secs(300);
/// Finished entries `GET …/queue` shows.
const RECENT: usize = 20;

/// The batch being tested.
#[derive(Clone, Debug, Serialize)]
pub struct BatchView {
    pub id: String,
    /// Change ids, in landing order.
    pub changes: Vec<String>,
    /// The speculative tip being tested.
    pub tip: Option<String>,
    pub started_at: i64,
    /// `testing`, `bisecting` or `landing`.
    pub state: &'static str,
    /// How many of `changes` (from the first) the test running now covers: all of them while
    /// testing, fewer while bisecting.
    pub testing_prefix: Option<usize>,
    /// The latest test's checks.
    pub checks: Vec<Check>,
}

/// A repo's queue worker (its state; the task runs [`Code::work`]).
#[derive(Default)]
pub struct Worker {
    notify: Notify,
    /// Something changed under the batch in flight (a member dequeued, revised or closed).
    dirty: AtomicBool,
    view: Mutex<Option<BatchView>>,
}

#[derive(Default)]
pub struct Queues {
    workers: Mutex<HashMap<String, Arc<Worker>>>,
    /// Workers only start once the runner is plugged in ([`Code::start_queues`]): a batch
    /// tested without one would land untested.
    ready: AtomicBool,
}

/// The checks of one test run.
#[derive(Clone, Debug, Default)]
pub struct TestResult {
    pub checks: Vec<Check>,
}

impl TestResult {
    pub fn failed(&self) -> BTreeSet<String> {
        self.checks.iter().filter(|k| k.status == CheckStatus::Failure).map(|k| k.name.clone()).collect()
    }
}

/// One change in the batch: its entry, the change, and the changeset its rebased copy ends at.
#[derive(Clone)]
struct Member {
    entry: EntryRow,
    change: Change,
    tip: ChangesetId,
}

impl Code {
    // ── settings and blockers ──

    /// The repo's landing settings, from `roll.toml` on its default branch's head.
    pub async fn settings(&self, r: &RepoRow) -> Arc<Settings> {
        let head = self.head(r).ok().flatten();
        if let Some((h, s)) = self.settings_cache.lock().get(&r.id)
            && *h == head
        {
            return s.clone();
        }
        let s = match head {
            None => Settings::default(),
            Some(h) => match self.store.read_file(&r.id, &h, FILE).await {
                Ok(None) => Settings::default(),
                Ok(Some(f)) if f.mode != code_store::Mode::Regular => {
                    Settings { error: Some(format!("{FILE} must be a regular file")), ..Settings::default() }
                }
                Ok(Some(f)) => {
                    let text = String::from_utf8_lossy(&f.data);
                    match crate::settings::parse(&text) {
                        Ok(mut s) => {
                            s.error = self.runner.read().as_ref().and_then(|r| r.validate(&text));
                            s
                        }
                        Err(e) => Settings { error: Some(e), ..Settings::default() },
                    }
                }
                Err(e) => Settings { error: Some(format!("{FILE}: {e}")), ..Settings::default() },
            },
        };
        let s = Arc::new(s);
        self.settings_cache.lock().insert(r.id.clone(), (head, s.clone()));
        s
    }

    /// The settings last read for a repo (defaults before the first read).
    pub fn settings_cached(&self, repo_id: &str) -> Arc<Settings> {
        self.settings_cache.lock().get(repo_id).map(|(_, s)| s.clone()).unwrap_or_default()
    }

    /// Why `c` can't land right now: its state and checks ([`base_blockers`](Self::base_blockers))
    /// and its reviews (from the review config and facts last read; [`mergeable`](Self::mergeable)
    /// reads them first).
    pub fn blockers(&self, r: &RepoRow, c: &Change, settings: &Settings) -> Vec<Blocker> {
        let mut out = Self::base_blockers(r, c, settings);
        out.extend(self.review_blockers(r, c));
        out
    }

    /// Why `c` can't land right now, reviews aside. `settings` are the repo's (from its default
    /// branch).
    pub fn base_blockers(r: &RepoRow, c: &Change, settings: &Settings) -> Vec<Blocker> {
        let mut out = Vec::new();
        match c.state {
            ChangeState::Landed => out.push(Blocker::new("state", "already landed")),
            ChangeState::Closed => out.push(Blocker::new("state", "closed; reopen it by revising it")),
            // A conflict is found again (or not) when it's tried: the branch may have moved on.
            ChangeState::Open | ChangeState::Queued | ChangeState::Conflict => {}
        }
        if c.draft {
            out.push(Blocker::new("draft", "draft: mark it ready first"));
        }
        if !c.trusted && !c.run_approved {
            out.push(Blocker::new("untrusted", "from someone without write access: a writer must approve running it"));
        }
        if r.land_policy == LandPolicy::Queue {
            let pending: Vec<String> = settings
                .change_checks(&r.required_checks)
                .into_iter()
                .filter_map(|name| match c.checks.iter().find(|k| k.name == name) {
                    Some(k) if matches!(k.status, CheckStatus::Success | CheckStatus::Skipped) => None,
                    Some(k) => Some(format!("{name} ({})", k.status.as_str())),
                    None => Some(format!("{name} (not run)")),
                })
                .collect();
            if !pending.is_empty() {
                out.push(Blocker::new("checks", format!("required checks haven't passed: {}", pending.join(", "))));
            }
        }
        out
    }

    /// Can `c` be queued (or land)? The extension point for landing rules: every rule adds
    /// blockers here.
    pub async fn mergeable(&self, r: &RepoRow, c: &Change) -> Result<(), Vec<Blocker>> {
        let settings = self.settings(r).await;
        self.prepare_review(r, c).await;
        let blockers = self.blockers(r, c, &settings);
        if blockers.is_empty() { Ok(()) } else { Err(blockers) }
    }

    /// A change's queue entry as the API shows it.
    pub(crate) fn queue_info(&self, e: &EntryRow) -> Result<QueueInfo> {
        let position = if e.state.active() { Some(self.db.position(e)?) } else { None };
        Ok(QueueInfo {
            id: e.id.clone(),
            position,
            lane: e.lane,
            state: e.state,
            error: e.error.clone(),
            output: e.output.clone(),
            batch: e.batch_id.clone(),
            enqueued_at: e.enqueued_at,
            enqueued_by: e.enqueued_by.clone(),
            finished_at: e.finished_at,
        })
    }

    // ── enqueue, dequeue, pause ──

    /// Queue a change. With the repo's `land_policy = "queue"` this is how it lands; on
    /// `post_merge` repos the queue still tests before landing if asked to.
    pub async fn enqueue(&self, r: &RepoRow, c: &Change, lane: Lane, caller: &Caller) -> Result<Value> {
        self.enqueue_by(r, c, lane, caller.identity().as_deref()).await
    }

    /// [`enqueue`](Self::enqueue), recording `by` as who queued it.
    pub async fn enqueue_by(&self, r: &RepoRow, c: &Change, lane: Lane, by: Option<&str>) -> Result<Value> {
        match c.state {
            ChangeState::Landed => return Err(Error::Conflict(format!("change {} already landed", c.number))),
            ChangeState::Closed => return Err(Error::Conflict(format!("change {} is closed", c.number))),
            _ => {}
        }
        if let Err(blockers) = self.mergeable(r, c).await {
            return Err(Error::Blocked(blockers));
        }
        let e = self.db.enqueue(&new_id("qe"), &r.id, &c.id, lane, by)?;
        self.db.set_change_state(&c.id, ChangeState::Queued, None, None)?;
        let change = self.change_or_404(&c.id)?;
        self.emit(&r.id, "change", serde_json::to_value(&change).unwrap_or_default());
        self.emit_queue(r);
        self.kick(&r.id);
        let position = self.db.position(&e)?;
        Ok(json!({ "status": "queued", "position": position, "queue_id": e.id, "change": change }))
    }

    /// Take a change out of the queue (it goes back to `open`).
    pub fn dequeue(&self, r: &RepoRow, c: &Change) -> Result<Change> {
        if self.db.remove_entry(&c.id)?.is_none() {
            return Err(Error::Conflict(format!("change {} isn't queued", c.number)));
        }
        // Even a waiting entry may be in a batch being built right now.
        self.unsettle(&r.id);
        if c.state == ChangeState::Queued {
            self.db.set_change_state(&c.id, ChangeState::Open, None, None)?;
        }
        let c = self.change_or_404(&c.id)?;
        self.emit(&r.id, "change", serde_json::to_value(&c).unwrap_or_default());
        self.emit_queue(r);
        Ok(c)
    }

    /// A change in the queue was revised, closed or marked draft: the batch it's in (if any)
    /// is stale, and a revision stays queued.
    pub(crate) fn queued_change_moved(&self, r: &RepoRow, c: &Change, keep: bool) -> Result<()> {
        let Some(e) = self.db.entry_for_change(&c.id)?.filter(|e| e.state.active()) else { return Ok(()) };
        self.unsettle(&r.id);
        if e.state != EntryState::Waiting {
            self.db.set_entry(&e.id, EntryState::Waiting, None, None)?;
        }
        if keep {
            self.db.set_change_state(&c.id, ChangeState::Queued, None, None)?;
            self.kick(&r.id);
        } else {
            self.db.remove_entry(&c.id)?;
        }
        self.emit_queue(r);
        Ok(())
    }

    /// `PATCH /changes/{id}/enqueue`: move a queued change to another lane. It keeps its place
    /// in time, so its position is recomputed by the lane order (urgent first, then by when it
    /// was queued). A change in the batch being tested is moved for the next batch.
    pub fn set_lane(&self, r: &RepoRow, c: &Change, lane: Lane, caller: &Caller) -> Result<Change> {
        if !self.db.set_entry_lane(&c.id, lane)? {
            return Err(Error::Conflict(format!("change {} isn't in the merge queue", c.number)));
        }
        self.db.audit(&r.id, &caller.identity().unwrap_or_default(), "queue_lane", &json!({ "change": c.id, "lane": lane }))?;
        self.emit_queue(r);
        let c = self.change_or_404(&c.id)?;
        self.emit_change(&c);
        Ok(c)
    }

    /// How many changes the batch in flight has (0 when none is).
    pub fn queue_batch_len(&self, repo_id: &str) -> u64 {
        self.queues.workers.lock().get(repo_id).and_then(|w| w.view.lock().as_ref().map(|v| v.changes.len() as u64)).unwrap_or(0)
    }

    pub fn set_queue_paused(&self, r: &RepoRow, caller: &Caller, paused: bool) -> Result<Value> {
        self.db.set_paused(&r.id, paused)?;
        self.db.audit(&r.id, &caller.identity().unwrap_or_default(), if paused { "queue_pause" } else { "queue_resume" }, &json!({}))?;
        if !paused {
            self.kick(&r.id);
        }
        self.emit_queue(r);
        self.queue_view(r)
    }

    /// Running queue workers (one per repo with recent queue activity).
    pub fn queue_workers(&self) -> usize {
        self.queues.workers.lock().len()
    }

    /// Mark a worker's batch stale and wake it.
    pub(crate) fn unsettle(&self, repo_id: &str) {
        if let Some(w) = self.queues.workers.lock().get(repo_id) {
            w.dirty.store(true, Ordering::SeqCst);
            w.notify.notify_one();
        }
    }

    // ── views ──

    pub fn queue_view(&self, r: &RepoRow) -> Result<Value> {
        let q = self.db.repo_queue(&r.id)?;
        let settings = self.settings_cached(&r.id);
        let batch = self.queues.workers.lock().get(&r.id).and_then(|w| w.view.lock().clone());
        let entry = |e: &EntryRow, position: Option<u64>| -> Result<Value> {
            let c = self.db.change(&e.change_id)?;
            Ok(json!({
                "id": e.id,
                "change": e.change_id,
                "number": c.as_ref().map(|c| c.number),
                "title": c.as_ref().map(|c| c.title.clone()),
                "author": c.as_ref().map(|c| c.author.clone()),
                "position": position,
                "lane": e.lane,
                "state": e.state,
                "error": e.error,
                "output": e.output,
                "batch": e.batch_id,
                "enqueued_at": e.enqueued_at,
                "enqueued_by": e.enqueued_by,
                "finished_at": e.finished_at,
            }))
        };
        let active = self.db.active_entries(&r.id)?;
        let entries = active.iter().enumerate().map(|(i, e)| entry(e, Some(i as u64 + 1))).collect::<Result<Vec<_>>>()?;
        let recent = self.db.finished_entries(&r.id, RECENT)?.iter().map(|e| entry(e, None)).collect::<Result<Vec<_>>>()?;
        let (landed_1h, avg_wait_s) = self.db.queue_throughput(&r.id)?;
        let max = settings.queue.batch_size;
        let state = match (&batch, q.paused) {
            (Some(b), _) => b.state,
            (None, true) => "paused",
            (None, false) => "idle",
        };
        Ok(json!({
            "repo": r.full_name(),
            "policy": r.land_policy,
            "state": state,
            "paused": q.paused,
            "batch": batch,
            "entries": entries,
            "recent": recent,
            "stats": {
                "depth": active.len(),
                "landed_1h": landed_1h,
                "avg_wait_s": avg_wait_s.round(),
                "batch_size": q.batch_size.unwrap_or(max).clamp(1, max),
                "max_batch_size": max,
            },
            "settings": {
                "checks": settings.queue.checks,
                "change_checks": settings.change_checks(&r.required_checks),
                "batch_size": max,
                "timeout_s": settings.queue.timeout_s,
                "require_change_checks": settings.queue.require_change_checks,
                "auto_revert": settings.main.auto_revert,
                "error": settings.error,
            },
            "last_green": q.last_green,
        }))
    }

    /// The `queue` event: enough for a client to know it should refetch `GET …/queue`.
    fn emit_queue(&self, r: &RepoRow) {
        let paused = self.db.repo_queue(&r.id).map(|q| q.paused).unwrap_or(false);
        let depth = self.db.active_entries(&r.id).map(|a| a.len()).unwrap_or(0);
        let batch = self.queues.workers.lock().get(&r.id).and_then(|w| w.view.lock().clone());
        let state = match (&batch, paused) {
            (Some(b), _) => b.state,
            (None, true) => "paused",
            (None, false) => "idle",
        };
        self.emit(&r.id, "queue", json!({ "state": state, "paused": paused, "depth": depth, "batch": batch }));
    }

    // ── workers ──

    /// Start processing queues: call once the runner (if any) is plugged in. Resumes what a
    /// previous process left: batches in flight are tested again, and a batch that was landing
    /// counts as landed if its tip made it onto the default branch.
    pub fn start_queues(&self) -> anyhow::Result<()> {
        self.queues.ready.store(true, Ordering::SeqCst);
        for repo_id in self.db.queued_repos()? {
            if let Err(e) = self.recover_queue(&repo_id) {
                warn!(repo = %repo_id, error = %e, "recovering a merge queue");
            }
            self.kick(&repo_id);
        }
        Ok(())
    }

    fn recover_queue(&self, repo_id: &str) -> Result<()> {
        let Some(r) = self.db.repo_by_id(repo_id)? else { return Ok(()) };
        let head = self.head(&r)?;
        for e in self.db.active_entries(repo_id)? {
            match e.state {
                EntryState::Waiting => {}
                EntryState::Landing => {
                    let landed = match (&e.landed_as, &head) {
                        (Some(tip), Some(head)) => match self.store.resolve(repo_id, tip)? {
                            Some(tip) => self.store.is_fp_ancestor(repo_id, &tip, head)?,
                            None => false,
                        },
                        _ => false,
                    };
                    if landed {
                        let tip = e.landed_as.clone().unwrap_or_default();
                        self.db.finish_entry(&e.id, EntryState::Landed, None, Some(&tip))?;
                        if let Some(c) = self.db.change(&e.change_id)? {
                            self.mark_landed_as(&c, &tip)?;
                        }
                        info!(repo = %r.full_name(), change = %e.change_id, "a batch that was landing when roll stopped did land");
                    } else {
                        self.db.set_entry(&e.id, EntryState::Waiting, None, None)?;
                    }
                }
                _ => self.db.set_entry(&e.id, EntryState::Waiting, None, None)?,
            }
        }
        Ok(())
    }

    /// Wake (or start) a repo's queue worker.
    pub(crate) fn kick(&self, repo_id: &str) {
        if !self.queues.ready.load(Ordering::SeqCst) {
            return;
        }
        let Some(code) = self.me.upgrade() else { return };
        let mut workers = self.queues.workers.lock();
        if let Some(w) = workers.get(repo_id) {
            w.notify.notify_one();
            return;
        }
        let w = Arc::new(Worker::default());
        workers.insert(repo_id.to_string(), w.clone());
        let repo_id = repo_id.to_string();
        tokio::spawn(async move { code.work(repo_id, w).await });
    }

    async fn work(self: Arc<Self>, repo_id: String, w: Arc<Worker>) {
        let mut failures = 0u32;
        loop {
            let progressed = match self.step(&repo_id, &w).await {
                Ok(p) => {
                    failures = 0;
                    p
                }
                Err(e) => {
                    failures += 1;
                    warn!(repo = %repo_id, error = %format!("{e:#}"), "merge queue step failed");
                    *w.view.lock() = None;
                    // Put the batch back in line (or finish it, if it had landed) and try again a
                    // little later.
                    if let Err(e) = self.recover_queue(&repo_id) {
                        warn!(repo = %repo_id, error = %e, "resetting a merge queue after a failure");
                    }
                    tokio::time::sleep(Duration::from_millis(250 * u64::from(failures.min(40)))).await;
                    true
                }
            };
            // The repo is gone (the step removed this worker), or another worker took over.
            if !self.queues.workers.lock().get(&repo_id).is_some_and(|x| Arc::ptr_eq(x, &w)) {
                return;
            }
            if progressed {
                continue;
            }
            tokio::select! {
                _ = w.notify.notified() => {}
                _ = tokio::time::sleep(IDLE) => {
                    let mut workers = self.queues.workers.lock();
                    let idle = self.db.waiting_entries(&repo_id, 1).map(|e| e.is_empty()).unwrap_or(false);
                    if idle && workers.get(&repo_id).is_some_and(|x| Arc::ptr_eq(x, &w)) {
                        workers.remove(&repo_id);
                        return;
                    }
                }
            }
        }
    }

    /// One batch: build, test, land or bisect. `false` when there was nothing to do.
    async fn step(&self, repo_id: &str, w: &Worker) -> anyhow::Result<bool> {
        let Some(r) = self.db.repo_by_id(repo_id)? else {
            self.queues.workers.lock().remove(repo_id);
            return Ok(false);
        };
        let q = self.db.repo_queue(repo_id)?;
        if q.paused {
            return Ok(false);
        }
        let settings = self.settings(&r).await;
        let max = settings.queue.batch_size;
        let size = q.batch_size.unwrap_or(max).clamp(1, max);
        let waiting = self.db.waiting_entries(repo_id, size)?;
        if waiting.is_empty() {
            return Ok(false);
        }
        w.dirty.store(false, Ordering::SeqCst);
        let head = self.head(&r)?;

        // Gate: whatever stopped being mergeable while it waited leaves the queue.
        let mut ready = Vec::new();
        for e in waiting {
            let Some(c) = self.db.change(&e.change_id)? else {
                self.db.finish_entry(&e.id, EntryState::Ejected, Some("the change no longer exists"), None)?;
                continue;
            };
            let c = self.change_view(c)?;
            if let Err(blockers) = self.mergeable(&r, &c).await {
                let why = blockers.iter().map(|b| b.message.as_str()).collect::<Vec<_>>().join("; ");
                self.eject(&r, &e, &c, &format!("can't land: {why}"), None)?;
                continue;
            }
            let cs = self.store.resolve_or_404(repo_id, &c.head)?;
            ready.push((e, c, cs));
        }
        if ready.is_empty() {
            self.emit_queue(&r);
            return Ok(true);
        }

        // Build the speculative chain on the scratch ref, starting at the head.
        let batch_id = new_id("qb");
        self.store.update_ref(repo_id, SCRATCH, Expect::Any, head).await?;
        for (e, _, _) in &ready {
            self.db.set_entry(&e.id, EntryState::Testing, Some(&batch_id), None)?;
        }
        *w.view.lock() = Some(BatchView {
            id: batch_id.clone(),
            changes: ready.iter().map(|(e, _, _)| e.change_id.clone()).collect(),
            tip: None,
            started_at: now(),
            state: "testing",
            testing_prefix: None,
            checks: Vec::new(),
        });
        self.emit_queue(&r);
        let mut members: Vec<Member> = Vec::new();
        for (e, c, cs) in ready {
            match self.store.land(repo_id, SCRATCH, cs).await {
                Ok(l) if l.already => {
                    // Already on the default branch (it landed some other way).
                    self.db.finish_entry(&e.id, EntryState::Landed, None, Some(&cs_id(&cs)))?;
                    self.mark_landed_as(&c, &cs_id(&cs))?;
                }
                Ok(l) => members.push(Member { entry: e, change: c, tip: l.changeset }),
                Err(LandError::Conflict(k)) => self.eject_conflict(&r, &e, &c, &k)?,
                Err(LandError::Other(err)) => self.eject(&r, &e, &c, &format!("couldn't be rebased onto {}: {err:#}", r.default_branch), None)?,
            }
        }
        if members.is_empty() {
            *w.view.lock() = None;
            self.emit_queue(&r);
            return Ok(true);
        }
        let tip = members.last().map(|m| m.tip).expect("non-empty");
        if let Some(v) = w.view.lock().as_mut() {
            v.changes = members.iter().map(|m| m.change.id.clone()).collect();
            v.tip = Some(cs_id(&tip));
            v.testing_prefix = Some(members.len());
        }
        self.emit_queue(&r);

        let result = self.test_at(&r, &settings, &tip, None, Some(w)).await?;
        if self.restart_if_dirty(&r, &members, w)? {
            return Ok(true);
        }
        let regressions = self.regressions(&r, &settings, head.as_ref(), &result).await?;
        if regressions.is_empty() {
            if self.land_members(&r, head, &members, &batch_id, w).await? {
                self.db.set_batch_size(repo_id, (size * 2).min(max))?;
            }
            *w.view.lock() = None;
            self.emit_queue(&r);
            return Ok(true);
        }

        // Bisect: prefix k of the chain is members[..k]; the head (k = 0) passes and the whole
        // batch (k = n) doesn't. Find the first k that fails.
        for m in &members {
            self.db.set_entry(&m.entry.id, EntryState::Bisecting, Some(&batch_id), None)?;
        }
        if let Some(v) = w.view.lock().as_mut() {
            v.state = "bisecting";
        }
        self.emit_queue(&r);
        let (mut lo, mut hi) = (0usize, members.len());
        let mut failure = result;
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if let Some(v) = w.view.lock().as_mut() {
                v.testing_prefix = Some(mid);
                v.tip = Some(cs_id(&members[mid - 1].tip));
            }
            self.emit_queue(&r);
            let result = self.test_at(&r, &settings, &members[mid - 1].tip, None, Some(w)).await?;
            if self.restart_if_dirty(&r, &members, w)? {
                return Ok(true);
            }
            if self.regressions(&r, &settings, head.as_ref(), &result).await?.is_empty() {
                lo = mid;
            } else {
                hi = mid;
                failure = result;
            }
        }
        // The changes before the culprit were tested together (prefix `lo` passed).
        if lo > 0 && !self.land_members(&r, head, &members[..lo], &batch_id, w).await? {
            // The branch moved: everything is rebuilt and tested again.
            for m in &members {
                self.db.set_entry(&m.entry.id, EntryState::Waiting, None, None)?;
            }
            *w.view.lock() = None;
            self.emit_queue(&r);
            return Ok(true);
        }
        let culprit = &members[hi - 1];
        let failed = self.regressions(&r, &settings, head.as_ref(), &failure).await?;
        let why = format!("failed the queue's checks: {}", failed.iter().cloned().collect::<Vec<_>>().join(", "));
        let mut output = String::new();
        for k in failure.checks.iter().filter(|k| failed.contains(&k.name)) {
            if let Some(s) = k.summary.as_deref().filter(|s| !s.is_empty()) {
                let tail: String = s.chars().rev().take(1500).collect::<Vec<_>>().into_iter().rev().collect();
                if !output.is_empty() {
                    output.push_str("\n\n");
                }
                output.push_str(&format!("{}:\n{tail}", k.name));
            }
        }
        self.eject_with(&r, &culprit.entry, &culprit.change, &why, Some(&output).filter(|o| !o.is_empty()).map(|o| o.as_str()), None)?;
        for m in &members[hi..] {
            self.db.set_entry(&m.entry.id, EntryState::Waiting, None, None)?;
        }
        self.db.set_batch_size(repo_id, (size / 2).max(1))?;
        info!(repo = %r.full_name(), change = culprit.change.number, "merge queue ejected a change that failed checks");
        *w.view.lock() = None;
        self.emit_queue(&r);
        Ok(true)
    }

    /// The batch went stale while it tested: its changes go back in line, untouched.
    fn restart_if_dirty(&self, r: &RepoRow, members: &[Member], w: &Worker) -> anyhow::Result<bool> {
        if !w.dirty.swap(false, Ordering::SeqCst) {
            return Ok(false);
        }
        for m in members {
            self.db.set_entry(&m.entry.id, EntryState::Waiting, None, None)?;
        }
        *w.view.lock() = None;
        self.emit_queue(r);
        Ok(true)
    }

    /// Land `members` (a tested prefix of the chain) with one compare-and-swap from `head`.
    /// `false` if the default branch moved meanwhile: they go back in line to be rebuilt.
    async fn land_members(&self, r: &RepoRow, head: Option<ChangesetId>, members: &[Member], batch: &str, w: &Worker) -> anyhow::Result<bool> {
        let tip = members.last().map(|m| m.tip).expect("non-empty");
        let settings = self.settings(r).await;
        // Each one must still be queued, at the head that was tested, and still mergeable (a
        // required check may have gone red, or the change been closed, while the batch tested).
        for m in members {
            let entry = self.db.entry_for_change(&m.change.id)?;
            let current = self.db.change(&m.change.id)?;
            let moved = entry.is_none_or(|e| e.id != m.entry.id || !e.state.active()) || current.as_ref().is_none_or(|c| c.head != m.change.head);
            let blocked = match current {
                Some(c) if !moved => !Self::base_blockers(r, &self.change_view(c)?, &settings).is_empty(),
                _ => false,
            };
            if moved || blocked {
                for m in members {
                    self.db.set_entry(&m.entry.id, EntryState::Waiting, None, None)?;
                }
                return Ok(false);
            }
        }
        if let Some(v) = w.view.lock().as_mut() {
            v.state = "landing";
        }
        for m in members {
            self.db.set_entry(&m.entry.id, EntryState::Landing, Some(batch), Some(&cs_id(&m.tip)))?;
            self.landing_changes.put((r.id.clone(), m.tip), m.change.id.clone());
        }
        self.emit_queue(r);
        match self.store.advance(&r.id, &r.default_branch, head, tip).await {
            Ok(_) => {}
            Err(e) => {
                info!(repo = %r.full_name(), error = %e, "the default branch moved under the merge queue; rebuilding the batch");
                for m in members {
                    self.db.set_entry(&m.entry.id, EntryState::Waiting, None, None)?;
                }
                return Ok(false);
            }
        }
        for m in members {
            let tip = cs_id(&m.tip);
            self.db.finish_entry(&m.entry.id, EntryState::Landed, None, Some(&tip))?;
            if let Some(c) = self.db.change(&m.change.id)? {
                self.mark_landed_as(&c, &tip)?;
            }
        }
        self.db.set_last_green(&r.id, &cs_id(&tip))?;
        self.db.touch_repo(&r.id)?;
        Ok(true)
    }

    fn eject_conflict(&self, r: &RepoRow, e: &EntryRow, c: &Change, k: &Conflict) -> anyhow::Result<()> {
        let paths: Vec<String> = k.paths.iter().map(|p| p.path.as_str().to_string()).collect();
        let info = ConflictInfo { paths: paths.clone(), head: k.head.as_ref().map(cs_id).unwrap_or_default() };
        let why = format!("conflicts with changes ahead of it in {}", paths.join(", "));
        self.eject(r, e, c, &why, Some(info))
    }

    /// Take a change out of the queue: `conflict` with the paths, or back to `open`.
    fn eject(&self, r: &RepoRow, e: &EntryRow, c: &Change, why: &str, conflict: Option<ConflictInfo>) -> anyhow::Result<()> {
        self.eject_with(r, e, c, why, None, conflict)
    }

    /// [`eject`](Self::eject), keeping the output (log tail) behind the reason apart.
    fn eject_with(&self, r: &RepoRow, e: &EntryRow, c: &Change, why: &str, output: Option<&str>, conflict: Option<ConflictInfo>) -> anyhow::Result<()> {
        if !self.db.finish_entry_with(&e.id, EntryState::Ejected, Some(why), output, None)? {
            return Ok(());
        }
        if c.state == ChangeState::Queued {
            match &conflict {
                Some(info) => self.db.set_change_state(&c.id, ChangeState::Conflict, None, Some(info))?,
                None => self.db.set_change_state(&c.id, ChangeState::Open, None, None)?,
            }
        }
        if let Ok(c) = self.change_or_404(&c.id) {
            self.emit(&r.id, "change", serde_json::to_value(&c).unwrap_or_default());
        }
        Ok(())
    }

    // ── testing trees ──

    /// Run checks on `cs`: the queue's required checks (or, with `only`, just those). Without
    /// a runner (or a `roll.toml` and an explicit list) there's nothing to run: a pass.
    pub async fn test_at(
        &self,
        r: &RepoRow,
        settings: &Settings,
        cs: &ChangesetId,
        only: Option<&BTreeSet<String>>,
        progress: Option<&Worker>,
    ) -> anyhow::Result<TestResult> {
        let Some(runner) = self.runner.read().clone() else { return Ok(TestResult::default()) };
        let required: Option<BTreeSet<String>> = match only {
            Some(only) => Some(only.clone()),
            None => settings.queue.checks.as_ref().map(|c| c.iter().cloned().collect()),
        };
        // Most trees have no roll.toml: don't materialize them for nothing.
        if self.store.entry(&r.id, cs, FILE)?.is_none() && required.as_ref().is_none_or(|r| r.is_empty()) {
            return Ok(TestResult::default());
        }
        let tree = self.checkout(&r.id, cs).await?;
        self.run_planned(r, settings, tree.path(), required.as_ref(), progress, &*runner).await
    }

    async fn run_planned(
        &self,
        r: &RepoRow,
        settings: &Settings,
        dir: &std::path::Path,
        required: Option<&BTreeSet<String>>,
        progress: Option<&Worker>,
        runner: &dyn crate::Runner,
    ) -> anyhow::Result<TestResult> {
        let plan = runner.plan(r, dir).await?;
        // A broken roll.toml shows up as an already failed check: it always counts.
        let mut checks: Vec<Check> =
            plan.into_iter().filter(|k| k.status != CheckStatus::Queued || required.is_none_or(|r| r.contains(&k.name))).collect();
        if let Some(required) = required {
            let missing: Vec<String> = required.iter().filter(|n| !checks.iter().any(|k| &k.name == *n)).cloned().collect();
            for name in missing {
                checks.push(Check {
                    name,
                    status: CheckStatus::Failure,
                    source: "runner".into(),
                    url: None,
                    summary: Some(format!("required by the merge queue, but {FILE} doesn't declare it")),
                    started_at: None,
                    finished_at: Some(now()),
                });
            }
        }
        let set_view = |checks: &[Check]| {
            if let Some(w) = progress
                && let Some(v) = w.view.lock().as_mut()
            {
                v.checks = checks.to_vec();
            }
        };
        set_view(&checks);
        let running = Arc::new(Mutex::new(checks.clone()));
        let set_view = &set_view;
        let runs = checks.iter().filter(|k| k.status == CheckStatus::Queued).cloned().map(|k| {
            let running = running.clone();
            async move {
                let done = match runner.run(r, None, dir, &k).await {
                    Ok(done) => done,
                    Err(e) => Check { status: CheckStatus::Failure, summary: Some(format!("the runner failed: {e:#}")), ..k.clone() },
                };
                let mut all = running.lock();
                if let Some(slot) = all.iter_mut().find(|x| x.name == done.name) {
                    *slot = done;
                }
                set_view(&all);
            }
        });
        let timeout = Duration::from_secs(settings.queue.timeout_s);
        if tokio::time::timeout(timeout, join_all(runs)).await.is_err() {
            let mut all = running.lock();
            for k in all.iter_mut().filter(|k| !k.status.finished()) {
                k.status = CheckStatus::Failure;
                k.summary = Some(format!("the merge queue's timeout ({}s) ran out", settings.queue.timeout_s));
                k.finished_at = Some(now());
            }
        }
        let checks = running.lock().clone();
        Ok(TestResult { checks })
    }

    /// Checks that fail in `result` but pass on `base` (the head a batch is built on): only
    /// those count against the batch. The base is tested only when needed, once per head.
    async fn regressions(
        &self,
        r: &RepoRow,
        settings: &Settings,
        base: Option<&ChangesetId>,
        result: &TestResult,
    ) -> anyhow::Result<BTreeSet<String>> {
        let failed = result.failed();
        if failed.is_empty() {
            return Ok(failed);
        }
        let Some(base) = base else { return Ok(failed) };
        if self.db.repo_queue(&r.id)?.last_green.as_deref() == Some(cs_id(base).as_str()) {
            return Ok(failed);
        }
        let key = (r.id.clone(), *base);
        let base_failed = match self.baselines.get(&key) {
            Some(b) => b,
            None => {
                let b = Arc::new(self.test_at(r, settings, base, Some(&failed), None).await?.failed());
                if b.is_empty() {
                    self.db.set_last_green(&r.id, &cs_id(base))?;
                }
                self.baselines.put(key, b.clone());
                b
            }
        };
        Ok(failed.difference(&base_failed).cloned().collect())
    }

    // ── landing policy ──

    /// `POST …/land` on a queue repo: the write becomes a change, and the change is queued.
    pub(crate) async fn land_via_queue(&self, r: &RepoRow, caller: &Caller, cs: ChangesetId, lane: Lane, by: Option<&str>) -> Result<Value> {
        let head = cs_id(&cs);
        let c = match self.db.change_by_head(&r.id, &head)? {
            Some(c) => self.change_view(c)?,
            None => {
                let info = self.store.info(&r.id, &cs)?.ok_or_else(|| Error::NotFound(format!("changeset {head}")))?;
                let input = json!({
                    "title": info.summary,
                    "body": info.message.split_once('\n').map(|(_, b)| b.trim()).unwrap_or(""),
                    "changeset": head,
                });
                self.create_change(r, caller, &input, None).await?
            }
        };
        let by = by.map(str::to_string).or_else(|| caller.identity());
        self.enqueue_by(r, &c, lane, by.as_deref()).await
    }

    /// An admin landing straight on a queue repo's default branch. Recorded in the audit log.
    pub(crate) fn audit_bypass(&self, r: &RepoRow, caller: &Caller, change: Option<&str>, cs: &ChangesetId, ref_name: &str) -> Result<()> {
        if *caller != Caller::Admin {
            return Err(Error::Forbidden("bypassing the merge queue needs the admin token".into()));
        }
        self.db.audit(
            &r.id,
            &caller.identity().unwrap_or_default(),
            "queue_bypass",
            &json!({ "change": change, "changeset": cs_id(cs), "ref": short_ref(ref_name) }),
        )?;
        warn!(repo = %r.full_name(), changeset = %cs_id(cs), "landed around the merge queue");
        Ok(())
    }
}
