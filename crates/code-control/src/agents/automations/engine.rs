//! The engine (docs/code/automations.md, "Durability"): fires schedules, matches the inbox's new
//! events against automations, and works on runs, step by step, with retries and backoff.
//!
//! One loop per node, woken by new events and runs ([`Code::wake_automations`]) and every
//! [`TICK`]. It works for the owners whose agents this node serves (`agents:<owner id>`; every
//! owner on a single node); it kicks the holder of an owner with work this node doesn't serve.
//! Every decision is a compare-and-swap in one write: a schedule slot fires once (its `next_at`),
//! an event is matched once (its `state`), a run is worked on by one node at a time (its `claim`,
//! renewed while a step runs and checked by every write of the run).

use super::context::StepContext;
use super::defs::{self, AutomationRow};
use super::registry::{OwnerRef, StepOutcome};
use super::schedule::Schedule;
use super::{filter, store};
use crate::Code;
use agent_proto::StreamEvent;
use agent_proto::automation::{
    AutomationEvent, AutomationEventId, AutomationId, AutomationRun, AutomationRunId, AutomationSpec, Concurrency, EventBrief, LogLevel, LogLine, OnError, RunBrief,
    RunState, Step, StepRun, StepState, Trigger, limits,
};
use code_sql::{Conn, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// How often the engine looks when nothing wakes it.
pub const TICK: Duration = Duration::from_secs(1);
/// How long a claim on a run lasts without being renewed, and how often it's renewed.
pub const CLAIM: Duration = Duration::from_secs(60);
const RENEW: Duration = Duration::from_secs(20);
/// The longest one attempt of a step runs before it counts as a transient failure.
const ATTEMPT_MAX: Duration = Duration::from_secs(10 * 60);
/// Runs worked on at once on a node.
const MAX_TASKS: usize = 32;
/// Events matched, and schedules fired, per round.
const PAGE: i64 = 200;
/// The cluster-wide worker that kicks owners with work nobody serves.
pub const KICKER: &str = "automations";

/// What's kept of a run besides what it shows: the definition it started with, each step's
/// private state, and whether its steps still need cleaning up after it ended.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Private {
    #[serde(default)]
    pub spec: Option<AutomationSpec>,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub run_as: Option<String>,
    #[serde(default)]
    pub steps: BTreeMap<String, Value>,
    #[serde(default)]
    pub cleanup: bool,
}

/// A run's row.
#[derive(Clone, Debug)]
pub(crate) struct RunRow {
    pub owner_id: String,
    pub event_id: String,
    pub run: AutomationRun,
    pub private: Private,
    pub next_at: Option<i64>,
}

const RUN_COLS: &str = "owner_id, event_id, json, private, cursor, next_at";

fn run_row(r: &Row<'_>) -> code_sql::Result<Option<RunRow>> {
    let json: String = r.get(2)?;
    let Ok(mut run) = serde_json::from_str::<AutomationRun>(&json) else { return Ok(None) };
    run.cursor = r.get::<_, i64>(4)?.max(0) as u64;
    let private: String = r.get(3)?;
    Ok(Some(RunRow { owner_id: r.get(0)?, event_id: r.get(1)?, run, private: serde_json::from_str(&private).unwrap_or_default(), next_at: r.get(5)? }))
}

pub(crate) fn run_on(c: &Conn<'_>, id: &str) -> code_sql::Result<Option<RunRow>> {
    Ok(c.query_row(&format!("SELECT {RUN_COLS} FROM automation_runs WHERE id = ?"), [id], run_row).optional()?.flatten())
}

pub(crate) fn run_of(db: &code_sql::Db, id: &str) -> code_sql::Result<Option<RunRow>> {
    Ok(db.query_row(&format!("SELECT {RUN_COLS} FROM automation_runs WHERE id = ?"), [id], run_row).optional()?.flatten())
}

/// The owner's runs (or one automation's), newest first, made before `before`.
pub(crate) fn runs(db: &code_sql::Db, owner_id: &str, automation: Option<&str>, status: Option<RunState>, before: Option<&str>, limit: i64) -> code_sql::Result<Vec<AutomationRun>> {
    let before_at: Option<(i64, String)> = match before {
        Some(b) => db.query_row("SELECT created_at, id FROM automation_runs WHERE id = ? AND owner_id = ?", [b, owner_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?,
        None => None,
    };
    let mut sql = format!("SELECT {RUN_COLS} FROM automation_runs WHERE owner_id = ?");
    use code_sql::Value::{Int, Text};
    let mut args: Vec<code_sql::Value> = vec![Text(owner_id.to_string())];
    if let Some(a) = automation {
        sql.push_str(" AND automation_id = ?");
        args.push(Text(a.to_string()));
    }
    if let Some(s) = status {
        sql.push_str(" AND status = ?");
        args.push(Text(state_str(s).to_string()));
    }
    if let Some((at, id)) = before_at {
        sql.push_str(" AND (created_at < ? OR (created_at = ? AND id < ?))");
        args.extend([Int(at), Int(at), Text(id)]);
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    args.push(Int(limit));
    Ok(db.query(&sql, args, run_row)?.into_iter().flatten().map(|r| r.run).collect())
}

/// Runs changed after `cursor` (a stream catching up).
pub(crate) fn runs_after(db: &code_sql::Db, owner_id: &str, cursor: u64, limit: i64) -> code_sql::Result<Vec<AutomationRun>> {
    Ok(db
        .query(&format!("SELECT {RUN_COLS} FROM automation_runs WHERE owner_id = ? AND cursor > ? ORDER BY cursor LIMIT ?"), params![owner_id, cursor as i64, limit], run_row)?
        .into_iter()
        .flatten()
        .map(|r| r.run)
        .collect())
}

pub(crate) fn event_of(db: &code_sql::Db, id: &str) -> code_sql::Result<Option<AutomationEvent>> {
    db.query_row("SELECT id, source, kind, key, at, data, automation_id FROM automation_events WHERE id = ?", [id], event_row).optional()
}

fn event_row(r: &Row<'_>) -> code_sql::Result<AutomationEvent> {
    Ok(AutomationEvent {
        id: AutomationEventId(r.get(0)?),
        source: r.get(1)?,
        kind: r.get(2)?,
        key: r.get(3)?,
        at: r.get::<_, i64>(4)?.max(0) as u64,
        data: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(Value::Null),
        automation: r.get::<_, Option<String>>(6)?.map(AutomationId),
    })
}

pub(crate) fn state_str(s: RunState) -> &'static str {
    match s {
        RunState::Queued => "queued",
        RunState::Running => "running",
        RunState::Waiting => "waiting",
        RunState::Succeeded => "succeeded",
        RunState::Failed => "failed",
        RunState::Cancelled => "cancelled",
        RunState::Skipped => "skipped",
    }
}

const ACTIVE: &str = "('queued', 'running', 'waiting')";

/// The template context of an event (before its run exists).
pub(crate) fn event_ctx(e: &AutomationEvent, summary: &str, automation: &AutomationId, name: &str, owner: &str) -> Value {
    json!({
        "event": { "id": e.id, "source": e.source, "kind": e.kind, "key": e.key, "at": e.at, "data": e.data, "summary": summary },
        "automation": { "id": automation, "name": name },
        "owner": owner,
        "steps": {},
        "run": {},
    })
}

fn ms_now() -> i64 {
    code_sql::now_ms()
}

fn bounded(v: Value) -> Value {
    let n = serde_json::to_string(&v).map_or(0, |s| s.len());
    if n > limits::OUTPUT { json!({ "truncated": true, "bytes": n }) } else { v }
}

fn line(level: LogLevel, text: impl Into<String>) -> LogLine {
    LogLine { at: ms_now().max(0) as u64, level, text: text.into().chars().take(limits::LOG_LINE).collect() }
}

fn push_log(s: &mut StepRun, lines: impl IntoIterator<Item = LogLine>) {
    s.log.extend(lines);
    let n = s.log.len();
    if n > limits::LOG_LINES {
        s.log.drain(..n - limits::LOG_LINES);
    }
}

/// End a run: its unfinished steps are cancelled, and its steps' cleanup is due.
fn finish(r: &mut RunRow, status: RunState, reason: Option<String>) {
    let now = ms_now().max(0) as u64;
    r.run.status = status;
    r.run.finished_at = Some(now);
    r.run.reason = reason;
    for s in r.run.steps.iter_mut().filter(|s| !s.status.is_final()) {
        s.status = StepState::Cancelled;
        s.next_at = None;
        s.finished_at = Some(now);
    }
    r.private.cleanup = !r.private.steps.is_empty();
    r.next_at = r.private.cleanup.then(ms_now);
}

/// Write a run back (one write, checking `claim` when given, taking a cursor); when it ends, the
/// next queued run of its automation is let go. The runs to publish.
fn save_on(c: &Conn<'_>, r: &RunRow, claim: Option<&str>, keep_claim: bool) -> code_sql::Result<Option<Vec<AutomationRun>>> {
    let now = ms_now();
    let cursor = crate::agents::store::take_cursors(c, &r.owner_id, 1)?;
    let mut run = r.run.clone();
    run.cursor = cursor;
    let json = serde_json::to_string(&run).unwrap_or_default();
    let private = serde_json::to_string(&r.private).unwrap_or_default();
    let status = state_str(run.status);
    let n = match claim {
        Some(cl) => c.execute(
            "UPDATE automation_runs SET status = ?, json = ?, private = ?, cursor = ?, updated_at = ?, next_at = ?, \
             claim = CASE WHEN ? THEN claim ELSE NULL END, claimed_until = CASE WHEN ? THEN claimed_until ELSE NULL END \
             WHERE id = ? AND claim = ?",
            params![status, json, private, cursor as i64, now, r.next_at, keep_claim, keep_claim, run.id.as_str(), cl],
        )?,
        None => c.execute(
            "UPDATE automation_runs SET status = ?, json = ?, private = ?, cursor = ?, updated_at = ?, next_at = ?, claim = NULL, claimed_until = NULL WHERE id = ?",
            params![status, json, private, cursor as i64, now, r.next_at, run.id.as_str()],
        )?,
    };
    if n == 0 {
        return Ok(None);
    }
    for t in &run.threads {
        c.execute(
            "INSERT INTO automation_threads (machine_id, thread, owner_id, run_id, automation_id) VALUES (?, ?, ?, ?, ?) ON CONFLICT (machine_id, thread, run_id) DO NOTHING",
            params![t.machine.as_str(), t.thread.as_str(), &r.owner_id, run.id.as_str(), run.automation.as_str()],
        )?;
    }
    let brief = RunBrief { id: run.id.clone(), status: run.status, at: run.created_at };
    c.execute(
        "UPDATE automations SET last_run = ? WHERE id = ? AND (last_run IS NULL OR last_run LIKE ?)",
        params![serde_json::to_string(&brief).unwrap_or_default(), run.automation.as_str(), format!("%\"id\":\"{}\"%", run.id)],
    )?;
    let mut out = vec![run.clone()];
    if run.status.is_final() {
        out.extend(promote(c, run.automation.as_str())?);
    }
    Ok(Some(out))
}

/// Let the oldest queued run of an automation go, if its concurrency allows one now.
fn promote(c: &Conn<'_>, automation: &str) -> code_sql::Result<Vec<AutomationRun>> {
    let active: i64 = c.query_row("SELECT COUNT(*) FROM automation_runs WHERE automation_id = ? AND status IN ('running', 'waiting')", [automation], |r| r.get(0))?;
    let spec: Option<String> = c.query_row("SELECT spec FROM automations WHERE id = ?", [automation], |r| r.get(0)).optional()?;
    let parallel = spec.and_then(|s| serde_json::from_str::<AutomationSpec>(&s).ok()).is_some_and(|s| s.concurrency == Concurrency::Parallel);
    let room = if parallel { limits::PARALLEL as i64 - active } else { (active == 0) as i64 };
    if room <= 0 {
        return Ok(vec![]);
    }
    let ids: Vec<String> =
        c.query("SELECT id FROM automation_runs WHERE automation_id = ? AND status = 'queued' ORDER BY created_at, id LIMIT ?", params![automation, room], |r| r.get(0))?;
    let mut out = Vec::new();
    for id in ids {
        let Some(mut q) = run_on(c, &id)? else { continue };
        q.run.status = RunState::Running;
        q.next_at = Some(ms_now());
        if let Some(v) = save_on(c, &q, None, false)? {
            out.extend(v.into_iter().take(1));
        }
    }
    Ok(out)
}

/// Cancel an automation's runs going on (it was deleted, or a new run replaces them under
/// `cancel_previous`). Inside a write; the runs to publish.
pub(crate) fn cancel_runs_of(c: &Conn<'_>, _owner_id: &str, automation: &str, why: &str) -> code_sql::Result<Vec<AutomationRun>> {
    let ids: Vec<String> = c.query(&format!("SELECT id FROM automation_runs WHERE automation_id = ? AND status IN {ACTIVE}"), [automation], |r| r.get(0))?;
    let mut out = Vec::new();
    for id in ids {
        if let Some(mut r) = run_on(c, &id)? {
            finish(&mut r, RunState::Cancelled, Some(why.to_string()));
            if let Some(v) = save_on(c, &r, None, false)? {
                out.push(v[0].clone());
            }
        }
    }
    Ok(out)
}

/// What deciding a new run's fate found.
fn first_state(c: &Conn<'_>, a: &AutomationRow, now: i64) -> code_sql::Result<(RunState, Option<String>)> {
    let rate = a.spec.rate_limit.unwrap_or(limits::DEFAULT_RATE);
    let recent: i64 = c.query_row(
        "SELECT COUNT(*) FROM automation_runs WHERE automation_id = ? AND created_at > ? AND status <> 'skipped'",
        params![&a.id, now - rate.per_s as i64 * 1000],
        |r| r.get(0),
    )?;
    if recent >= rate.max as i64 {
        return Ok((RunState::Skipped, Some(format!("over the rate limit ({} runs per {} s)", rate.max, rate.per_s))));
    }
    let active: i64 = c.query_row(&format!("SELECT COUNT(*) FROM automation_runs WHERE automation_id = ? AND status IN {ACTIVE}"), [&a.id], |r| r.get(0))?;
    Ok(match a.spec.concurrency {
        _ if active == 0 => (RunState::Running, None),
        Concurrency::Skip => (RunState::Skipped, Some("another run was going".into())),
        Concurrency::Queue => (RunState::Queued, None),
        Concurrency::Parallel if active as usize >= limits::PARALLEL => (RunState::Queued, None),
        Concurrency::Parallel | Concurrency::CancelPrevious => (RunState::Running, None),
    })
}

/// A new run of `a` for `e` (inside the write that marks `e` done). `None` if it has one.
fn new_run(c: &Conn<'_>, a: &AutomationRow, e: &AutomationEvent, summary: &str, owner: &str, status: RunState, reason: Option<String>) -> code_sql::Result<Option<Vec<AutomationRun>>> {
    let now = ms_now();
    let id = AutomationRunId(crate::new_id("arn"));
    let mut cancelled = Vec::new();
    if status == RunState::Running && a.spec.concurrency == Concurrency::CancelPrevious {
        cancelled = cancel_runs_of(c, &a.owner_id, &a.id, "a newer run replaced it")?;
    }
    let steps = a
        .spec
        .steps
        .iter()
        .map(|s| StepRun {
            id: s.id.clone(),
            kind: s.kind.clone(),
            status: if status == RunState::Skipped { StepState::Skipped } else { StepState::Pending },
            attempt: 0,
            started_at: None,
            finished_at: None,
            next_at: None,
            output: Value::Null,
            error: None,
            log: vec![],
        })
        .collect();
    let run = AutomationRun {
        id: id.clone(),
        automation: AutomationId(a.id.clone()),
        name: a.spec.name.clone(),
        status,
        event: EventBrief { id: e.id.clone(), source: e.source.clone(), kind: e.kind.clone(), summary: summary.to_string(), at: e.at },
        steps,
        threads: vec![],
        cursor: 0,
        created_at: now.max(0) as u64,
        started_at: None,
        finished_at: status.is_final().then_some(now.max(0) as u64),
        reason,
        retried_at: None,
    };
    let private = Private { spec: Some(a.spec.clone()), owner: owner.to_string(), run_as: a.spec.run_as.clone(), ..Private::default() };
    let next_at = (status == RunState::Running).then_some(now);
    let n = c.execute(
        "INSERT INTO automation_runs (id, owner_id, automation_id, event_id, status, json, private, cursor, created_at, updated_at, next_at) \
         VALUES (?, ?, ?, ?, ?, '{}', '{}', 0, ?, ?, ?) ON CONFLICT (automation_id, event_id) DO NOTHING",
        params![id.as_str(), &a.owner_id, &a.id, e.id.as_str(), state_str(status), now, now, next_at],
    )?;
    if n == 0 {
        return Ok(None);
    }
    let row = RunRow { owner_id: a.owner_id.clone(), event_id: e.id.0.clone(), run, private, next_at };
    let mut out = save_on(c, &row, None, false)?.unwrap_or_default();
    // The new run's last-run brief replaces whatever was there.
    let brief = RunBrief { id: id.clone(), status, at: now.max(0) as u64 };
    c.execute("UPDATE automations SET last_run = ? WHERE id = ?", params![serde_json::to_string(&brief).unwrap_or_default(), &a.id])?;
    out.extend(cancelled);
    Ok(Some(out))
}

/// The automations an event is for, read in the matching write.
fn candidates(c: &Conn<'_>, owner_id: &str, e: &AutomationEvent) -> code_sql::Result<Vec<AutomationRow>> {
    if let Some(a) = &e.automation {
        return Ok(defs::get_on(c, a.as_str())?.filter(|r| r.owner_id == owner_id && r.deleted_at.is_none()).into_iter().collect());
    }
    let repo = e.data.get("repo_id").and_then(Value::as_str).unwrap_or("*");
    let ids: Vec<String> = c.query(
        "SELECT DISTINCT automation_id FROM automation_listens WHERE owner_id = ? AND source = ? AND (kind = ? OR kind = '*') AND (repo_id = ? OR repo_id = '*')",
        params![owner_id, &e.source, &e.kind, repo],
        |r| r.get(0),
    )?;
    let mut out = Vec::new();
    for id in ids {
        if let Some(r) = defs::get_on(c, &id)?.filter(|r| r.enabled && r.deleted_at.is_none()) {
            out.push(r);
        }
    }
    Ok(out)
}

impl Code {
    /// Whether this node works on `owner_id`'s automations (it holds the owner's agents lease).
    pub(crate) fn serves_automations(&self, owner_id: &str) -> bool {
        self.cluster().is_none_or(|c| c.owns(&format!("{}{owner_id}", crate::cluster::AGENTS_KEY)))
    }

    /// Start the engine on this node (idempotent; needs a Tokio runtime). Runs, schedules and
    /// events left by a previous process are picked up at its first round.
    pub fn start_automations(&self) {
        if tokio::runtime::Handle::try_current().is_err() || self.automations.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(me) = self.me.upgrade() else {
            self.automations.started.store(false, Ordering::SeqCst);
            return;
        };
        super::steps::register_built_ins(&me);
        super::register_integrations(&me);
        let weak = Arc::downgrade(&me);
        drop(me);
        tokio::spawn(async move {
            loop {
                let Some(code) = weak.upgrade() else { return };
                let wake = code.automations.wake.clone();
                let notified = wake.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                code.automation_round().await;
                let tick = code.automations.tick();
                drop(code);
                let _ = tokio::time::timeout(tick, notified).await;
            }
        });
    }

    /// One round: due schedules, new events, due runs.
    pub async fn automation_round(self: &Arc<Self>) {
        let owners = match self.automation_owners() {
            Ok(o) => o,
            Err(e) => return tracing::warn!(error = %e, "automations: reading what's due"),
        };
        let mut elsewhere = Vec::new();
        for o in owners {
            if !self.serves_automations(&o) {
                elsewhere.push(o);
                continue;
            }
            if let Err(e) = self.fire_schedules(&o).await {
                tracing::warn!(owner = %o, error = %e, "automations: firing schedules");
            }
            if let Err(e) = self.match_events(&o).await {
                tracing::warn!(owner = %o, error = %e, "automations: matching events");
            }
            if let Err(e) = self.start_due_runs(&o) {
                tracing::warn!(owner = %o, error = %e, "automations: starting runs");
            }
        }
        if !elsewhere.is_empty() && self.singleton(KICKER) {
            for o in elsewhere {
                if self.automations.kick_due(&o) {
                    self.kick_owner(&format!("{}{o}", crate::cluster::AGENTS_KEY));
                }
            }
        }
    }

    /// Owners with something due: a schedule, a new event, a run.
    fn automation_owners(&self) -> crate::Result<Vec<String>> {
        let now = ms_now();
        Ok(self.db.sql().query(
            "SELECT owner_id FROM automations WHERE next_at IS NOT NULL AND next_at <= ? AND enabled = 1 AND deleted_at IS NULL \
             UNION SELECT owner_id FROM automation_events WHERE state = 'new' \
             UNION SELECT owner_id FROM automation_runs WHERE next_at IS NOT NULL AND next_at <= ? AND (claimed_until IS NULL OR claimed_until < ?)",
            params![now, now, now],
            |r| r.get(0),
        )?)
    }

    /// Whether the owner has automation work a node must stay for: an enabled schedule, an
    /// event not matched yet, a run going on (`Code::repo_busy` keeps the agents lease then).
    pub(crate) fn automations_busy(&self, owner_id: &str) -> bool {
        self.db
            .sql()
            .query_row(
                &format!(
                    "SELECT (EXISTS (SELECT 1 FROM automations WHERE owner_id = ?1 AND next_at IS NOT NULL AND enabled = 1 AND deleted_at IS NULL)) \
                     OR (EXISTS (SELECT 1 FROM automation_events WHERE owner_id = ?1 AND state = 'new')) \
                     OR (EXISTS (SELECT 1 FROM automation_runs WHERE owner_id = ?1 AND (status IN {ACTIVE} OR next_at IS NOT NULL)))"
                ),
                [owner_id],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(true)
    }

    /// Fire the owner's due schedules: one write per slot, a compare-and-swap on `next_at` with
    /// the tick recorded in it. A slot missed while no node ran fires once.
    async fn fire_schedules(&self, owner_id: &str) -> crate::Result<()> {
        let now = ms_now();
        let due: Vec<AutomationRow> = self
            .db
            .sql()
            .query(
                &format!("SELECT {} FROM automations WHERE owner_id = ? AND next_at IS NOT NULL AND next_at <= ? AND enabled = 1 AND deleted_at IS NULL LIMIT ?", defs::COLS),
                params![owner_id, now, PAGE],
                defs::row,
            )?
            .into_iter()
            .flatten()
            .collect();
        let mut fired = false;
        for a in due {
            let Some(at) = a.next_at else { continue };
            let next = Schedule::of(&a.spec.trigger).ok().flatten().and_then(|s| s.next_after(now.max(at), a.created_at));
            let e = super::NewEvent {
                source: a.spec.trigger.source().to_string(),
                kind: "tick".into(),
                id: format!("{}:{at}", a.id),
                data: json!({ "at": now, "scheduled": at }),
                summary: format!("Scheduled for {}", chrono::DateTime::from_timestamp_millis(at).map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default()),
                automation: Some(AutomationId(a.id.clone())),
            };
            let ok = self.db.sql().write(|c| -> code_sql::Result<_> {
                if c.execute("UPDATE automations SET next_at = ? WHERE id = ? AND next_at = ?", params![next, &a.id, at])? == 0 {
                    return Ok(false);
                }
                store::insert_event(c, owner_id, &e)?;
                Ok(true)
            })?;
            fired |= ok;
        }
        if fired {
            self.automations.wake.notify_one();
        }
        Ok(())
    }

    /// Match the owner's new events, oldest first: each in one write that marks it done and
    /// makes its runs.
    pub(crate) async fn match_events(&self, owner_id: &str) -> crate::Result<()> {
        let events: Vec<(AutomationEvent, String)> = self.db.sql().query(
            "SELECT id, source, kind, key, at, data, automation_id, summary FROM automation_events WHERE owner_id = ? AND state = 'new' ORDER BY at, id LIMIT ?",
            params![owner_id, PAGE],
            |r| Ok((event_row(r)?, r.get(7)?)),
        )?;
        if events.is_empty() {
            return Ok(());
        }
        let owner = crate::agents::store::owner_name(self.db.sql(), owner_id)?.unwrap_or_default();
        let hub = self.agents.hub(owner_id);
        let reg = &self.automations.registry;
        for (e, summary) in events {
            let _order = hub.order.lock().await;
            let made = self.db.sql().write(|c| -> code_sql::Result<_> {
                if c.execute("UPDATE automation_events SET state = 'done' WHERE id = ? AND state = 'new'", [e.id.as_str()])? == 0 {
                    return Ok(vec![]);
                }
                let mut out = Vec::new();
                for a in candidates(c, owner_id, &e)? {
                    // The integration's own say first, then the filters.
                    if let Trigger::Integration { source, config, events } = &a.spec.trigger
                        && (e.automation.is_none() && (!events.is_empty() && !events.contains(&e.kind) || reg.trigger(source).is_none_or(|t| !t.matches(config, &e))))
                    {
                        continue;
                    }
                    let ctx = event_ctx(&e, &summary, &AutomationId(a.id.clone()), &a.spec.name, &owner);
                    let holds = filter::all(&a.spec.filters, &ctx);
                    let (status, reason) = match (holds, &e.automation) {
                        (true, _) => first_state(c, &a, ms_now())?,
                        // A run by hand the filters refuse is recorded, so the owner sees why.
                        (false, Some(_)) => (RunState::Skipped, Some("the filters don't hold for this event".into())),
                        (false, None) => continue,
                    };
                    if let Some(mut runs) = new_run(c, &a, &e, &summary, &owner, status, reason)? {
                        out.append(&mut runs);
                    }
                }
                Ok(out)
            })?;
            for r in made {
                let _ = hub.events.send(Arc::new(StreamEvent::AutomationRun { cursor: r.cursor, run: Box::new(r) }));
            }
        }
        Ok(())
    }

    /// Start a task for each due run of the owner (claimed in the task).
    fn start_due_runs(self: &Arc<Self>, owner_id: &str) -> crate::Result<()> {
        let now = ms_now();
        let ids: Vec<String> = self.db.sql().query(
            "SELECT id FROM automation_runs WHERE owner_id = ? AND next_at IS NOT NULL AND next_at <= ? AND (claimed_until IS NULL OR claimed_until < ?) ORDER BY next_at LIMIT ?",
            params![owner_id, now, now, MAX_TASKS as i64],
            |r| r.get(0),
        )?;
        for id in ids {
            if !self.automations.working.lock().insert(id.clone()) {
                continue;
            }
            if self.automations.working.lock().len() > MAX_TASKS {
                self.automations.working.lock().remove(&id);
                break;
            }
            let me = self.clone();
            tokio::spawn(async move {
                me.work_on(&id).await;
                me.automations.working.lock().remove(&id);
            });
        }
        Ok(())
    }

    /// Claim a run and take it as far as it goes now.
    async fn work_on(self: &Arc<Self>, id: &str) {
        let claim = crate::agents::api::random_hex(16);
        let now = ms_now();
        let until = now + CLAIM.as_millis() as i64;
        let claimed = self.db.sql().execute(
            "UPDATE automation_runs SET claim = ?, claimed_until = ? WHERE id = ? AND next_at IS NOT NULL AND next_at <= ? AND (claimed_until IS NULL OR claimed_until < ?)",
            params![&claim, until, id, now, now],
        );
        match claimed {
            Ok(1) => {}
            Ok(_) => return,
            Err(e) => return tracing::warn!(run = %id, error = %e, "automations: claiming a run"),
        }
        let r = match run_of(self.db.sql(), id) {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(e) => return tracing::warn!(run = %id, error = %e, "automations: reading a run"),
        };
        if let Err(e) = self.advance(r, &claim).await {
            tracing::warn!(run = %id, error = %e, "automations: working on a run");
            // Let it go: the next round tries again.
            let _ = self.db.sql().execute("UPDATE automation_runs SET claim = NULL, claimed_until = NULL, next_at = ? WHERE id = ? AND claim = ?", params![ms_now() + 5000, id, &claim]);
        }
    }

    /// Save a run under its claim and publish it (and any run its end let go). `false`: the
    /// claim was lost (cancelled, or another node took over): stop.
    async fn save_run(&self, r: &RunRow, claim: &str, keep_claim: bool) -> crate::Result<bool> {
        let hub = self.agents.hub(&r.owner_id);
        let _order = hub.order.lock().await;
        let Some(runs) = self.db.sql().write(|c| save_on(c, r, Some(claim), keep_claim))? else { return Ok(false) };
        for run in runs {
            let _ = hub.events.send(Arc::new(StreamEvent::AutomationRun { cursor: run.cursor, run: Box::new(run) }));
        }
        Ok(true)
    }

    fn renew(&self, id: &str, claim: &str) -> bool {
        let until = ms_now() + CLAIM.as_millis() as i64;
        matches!(self.db.sql().execute("UPDATE automation_runs SET claimed_until = ? WHERE id = ? AND claim = ?", params![until, id, claim]), Ok(1))
    }

    /// The run's template context: the event, the run, the steps that finished.
    fn run_ctx(&self, r: &RunRow) -> crate::Result<Value> {
        let e = event_of(self.db.sql(), &r.event_id)?.ok_or_else(|| crate::Error::NotFound(format!("event {}", r.event_id)))?;
        let mut ctx = event_ctx(&e, &r.run.event.summary, &r.run.automation, &r.run.name, &r.private.owner);
        ctx["run"] = json!({ "id": r.run.id, "automation": r.run.automation, "created_at": r.run.created_at, "threads": r.run.threads });
        if let Some(spec) = &r.private.spec {
            ctx["automation"]["defaults"] = json!(spec.defaults);
        }
        let mut steps = serde_json::Map::new();
        for s in &r.run.steps {
            let v = match s.status {
                StepState::Succeeded => s.output.clone(),
                StepState::Failed => json!({ "error": s.error }),
                StepState::Skipped => json!({ "skipped": true }),
                _ => continue,
            };
            steps.insert(s.id.clone(), v);
        }
        ctx["steps"] = Value::Object(steps);
        Ok(ctx)
    }

    /// Take a claimed run as far as it goes: step by step until one waits, retries later,
    /// fails the run, or the run ends.
    async fn advance(self: &Arc<Self>, mut r: RunRow, claim: &str) -> crate::Result<()> {
        let id = r.run.id.0.clone();
        let spec = r.private.spec.clone().ok_or_else(|| crate::Error::Invalid("the run has no definition".into()))?;
        if r.run.status.is_final() {
            // Ended: what its steps made only for it is cleaned up once.
            self.clean_up(&mut r).await;
            r.private.cleanup = false;
            r.next_at = None;
            self.save_run(&r, claim, false).await?;
            return Ok(());
        }
        if r.run.started_at.is_none() {
            r.run.started_at = Some(ms_now().max(0) as u64);
        }
        let owner = OwnerRef { id: r.owner_id.clone(), name: r.private.owner.clone() };
        loop {
            let Some(i) = r.run.steps.iter().position(|s| !s.status.is_final()) else {
                finish(&mut r, RunState::Succeeded, None);
                self.save_run(&r, claim, true).await?;
                // Clean up at once if there's anything to (the claim is still ours).
                if r.private.cleanup {
                    self.clean_up(&mut r).await;
                    r.private.cleanup = false;
                    r.next_at = None;
                }
                self.save_run(&r, claim, false).await?;
                return Ok(());
            };
            let def = spec.steps.get(i).cloned().ok_or_else(|| crate::Error::Invalid("the run's steps don't match its definition".into()))?;
            let ctx = self.run_ctx(&r)?;
            let now = ms_now();
            // `when` is looked at once, before the first attempt.
            if r.run.steps[i].status == StepState::Pending && !filter::all(&def.when, &ctx) {
                let s = &mut r.run.steps[i];
                s.status = StepState::Skipped;
                s.finished_at = Some(now.max(0) as u64);
                continue;
            }
            let Some(kind) = self.automations.registry.step(&def.kind) else {
                self.fail_step(&mut r, i, format!("this server has no step kind {:?}", def.kind), &def);
                if r.run.status.is_final() {
                    return self.end(r, claim).await;
                }
                continue;
            };
            let retry = def.retry.unwrap_or_else(|| kind.default_retry());
            let timeout = def.timeout_s.map(Duration::from_secs).unwrap_or_else(|| kind.default_timeout());
            let s = &mut r.run.steps[i];
            let started = s.started_at.map(|t| t as i64).unwrap_or(now);
            if s.started_at.is_some() && now - started > timeout.as_millis() as i64 {
                let msg = format!("timed out after {} s", timeout.as_secs());
                self.fail_step(&mut r, i, msg, &def);
                if r.run.status.is_final() {
                    return self.end(r, claim).await;
                }
                continue;
            }
            // A new attempt, unless it's a wait being looked at again.
            let waiting = s.status == StepState::Waiting;
            if !waiting {
                s.attempt += 1;
            }
            if s.started_at.is_none() {
                s.started_at = Some(now.max(0) as u64);
            }
            if !waiting && s.status != StepState::Running {
                s.status = StepState::Running;
                s.next_at = None;
                r.run.status = RunState::Running;
                if !self.save_run(&r, claim, true).await? {
                    return Ok(());
                }
            }
            let state = r.private.steps.get(&def.id).cloned().unwrap_or(Value::Null);
            let mut sc = StepContext::new(
                self.clone(),
                owner.clone(),
                r.run.automation.clone(),
                r.run.name.clone(),
                r.private.run_as.clone(),
                r.run.id.clone(),
                def.clone(),
                r.run.steps[i].attempt,
                ctx,
                state,
                started,
            );
            let deadline = Duration::from_millis((started + timeout.as_millis() as i64 - now).max(1000) as u64).min(ATTEMPT_MAX);
            let outcome = {
                let fut = kind.run(&mut sc);
                tokio::pin!(fut);
                let t = tokio::time::sleep(deadline);
                tokio::pin!(t);
                loop {
                    tokio::select! {
                        out = &mut fut => break out,
                        _ = &mut t => break StepOutcome::Retry(format!("the attempt took over {} s", deadline.as_secs())),
                        _ = tokio::time::sleep(RENEW) => {
                            if !self.renew(&id, claim) {
                                return Ok(());
                            }
                        }
                    }
                }
            };
            // What the attempt left: its log, its state, threads it started.
            let StepContext { log, threads, state, .. } = sc;
            push_log(&mut r.run.steps[i], log);
            if state.is_null() {
                r.private.steps.remove(&def.id);
            } else {
                r.private.steps.insert(def.id.clone(), state);
            }
            for t in threads {
                if !r.run.threads.iter().any(|x| x.thread == t.thread) {
                    r.run.threads.push(t);
                }
            }
            let now = ms_now();
            match outcome {
                StepOutcome::Done(out) => {
                    let s = &mut r.run.steps[i];
                    s.status = StepState::Succeeded;
                    s.output = bounded(out);
                    s.error = None;
                    s.next_at = None;
                    s.finished_at = Some(now.max(0) as u64);
                }
                StepOutcome::Wait { after } => {
                    let wake = now + after.as_millis().max(200) as i64;
                    let wake = wake.min(started + timeout.as_millis() as i64 + 1000);
                    let s = &mut r.run.steps[i];
                    s.status = StepState::Waiting;
                    s.next_at = Some(wake.max(0) as u64);
                    r.run.status = RunState::Waiting;
                    r.next_at = Some(wake);
                    self.save_run(&r, claim, false).await?;
                    return Ok(());
                }
                StepOutcome::Retry(why) => {
                    let s = &mut r.run.steps[i];
                    if s.attempt < retry.attempts {
                        let backoff = (retry.backoff_s.max(1) * 2u64.saturating_pow(s.attempt.saturating_sub(1))).min(3600);
                        let wake = now + backoff as i64 * 1000;
                        push_log(s, [line(LogLevel::Warn, format!("attempt {} failed: {why}; trying again in {backoff} s", s.attempt))]);
                        s.status = StepState::Retrying;
                        s.error = Some(why);
                        s.next_at = Some(wake as u64);
                        r.run.status = RunState::Waiting;
                        r.next_at = Some(wake);
                        self.save_run(&r, claim, false).await?;
                        return Ok(());
                    }
                    let msg = format!("{why} (after {} attempts)", s.attempt);
                    self.fail_step(&mut r, i, msg, &def);
                }
                StepOutcome::Fail(why) => self.fail_step(&mut r, i, why, &def),
            }
            if r.run.status.is_final() {
                return self.end(r, claim).await;
            }
            if !self.save_run(&r, claim, true).await? {
                return Ok(());
            }
        }
    }

    /// A step failed for good: the run fails, unless the step goes on on error.
    fn fail_step(&self, r: &mut RunRow, i: usize, why: String, def: &Step) {
        let now = ms_now().max(0) as u64;
        let s = &mut r.run.steps[i];
        push_log(s, [line(LogLevel::Error, why.clone())]);
        s.status = StepState::Failed;
        s.error = Some(why.clone());
        s.next_at = None;
        s.finished_at = Some(now);
        // The step as the builder names it.
        let step = if def.name.trim().is_empty() { def.id.clone() } else { format!("“{}”", def.name.trim()) };
        if def.on_error == OnError::Fail {
            finish(r, RunState::Failed, Some(format!("step {step} failed: {why}")));
        }
    }

    /// The run ended: save it, clean up after its steps, save again.
    async fn end(self: &Arc<Self>, mut r: RunRow, claim: &str) -> crate::Result<()> {
        if !self.save_run(&r, claim, true).await? {
            return Ok(());
        }
        if r.private.cleanup {
            self.clean_up(&mut r).await;
            r.private.cleanup = false;
            r.next_at = None;
            self.save_run(&r, claim, false).await?;
        }
        Ok(())
    }

    /// Every step with state gets its `cleanup`, best effort.
    async fn clean_up(self: &Arc<Self>, r: &mut RunRow) {
        let Some(spec) = r.private.spec.clone() else { return };
        let Ok(ctx) = self.run_ctx(r) else { return };
        let owner = OwnerRef { id: r.owner_id.clone(), name: r.private.owner.clone() };
        for (i, def) in spec.steps.iter().enumerate() {
            let Some(state) = r.private.steps.get(&def.id).cloned() else { continue };
            let Some(kind) = self.automations.registry.step(&def.kind) else { continue };
            let started = r.run.steps.get(i).and_then(|s| s.started_at).unwrap_or_default() as i64;
            let mut sc = StepContext::new(self.clone(), owner.clone(), r.run.automation.clone(), r.run.name.clone(), r.private.run_as.clone(), r.run.id.clone(), def.clone(), 0, ctx.clone(), state, started);
            if tokio::time::timeout(Duration::from_secs(60), kind.cleanup(&mut sc, r.run.status)).await.is_err() {
                sc.log(LogLevel::Warn, "cleaning up took over a minute; left as it is");
            }
            if let Some(s) = r.run.steps.get_mut(i) {
                push_log(s, sc.log);
            }
        }
    }

    /// Cancel a run (the API). The run as it is now; `None` if it isn't the owner's.
    pub(crate) async fn cancel_automation_run(&self, owner_id: &str, id: &str) -> crate::Result<Option<AutomationRun>> {
        let hub = self.agents.hub(owner_id);
        let _order = hub.order.lock().await;
        let runs = self.db.sql().write(|c| -> code_sql::Result<_> {
            let Some(mut r) = run_on(c, id)?.filter(|r| r.owner_id == owner_id) else { return Ok(None) };
            if r.run.status.is_final() {
                return Ok(Some(vec![r.run]));
            }
            finish(&mut r, RunState::Cancelled, Some("cancelled".into()));
            save_on(c, &r, None, false)
        })?;
        let Some(runs) = runs else { return Ok(None) };
        let first = runs.first().cloned();
        for run in runs {
            let _ = hub.events.send(Arc::new(StreamEvent::AutomationRun { cursor: run.cursor, run: Box::new(run) }));
        }
        self.automations.wake.notify_one();
        Ok(first)
    }

    /// Go on with a failed or cancelled run from the step that stopped it. `Err(Conflict)` for a
    /// run that isn't.
    pub(crate) async fn retry_automation_run(&self, owner_id: &str, id: &str) -> crate::Result<Option<AutomationRun>> {
        let hub = self.agents.hub(owner_id);
        let _order = hub.order.lock().await;
        let runs = self.db.sql().write(|c| -> code_sql::Result<_> {
            let Some(mut r) = run_on(c, id)?.filter(|r| r.owner_id == owner_id) else { return Ok(Ok(None)) };
            if !matches!(r.run.status, RunState::Failed | RunState::Cancelled) {
                return Ok(Err(()));
            }
            let now = ms_now().max(0) as u64;
            // What stopped it stays readable: the step's error and the run's reason go into the
            // step's log before they're cleared.
            let why = match (&r.run.status, &r.run.reason) {
                (RunState::Cancelled, _) => "Retried after the run was cancelled".to_string(),
                (_, Some(reason)) => format!("Retried after the run failed: {reason}"),
                _ => "Retried after the run failed".to_string(),
            };
            for s in r.run.steps.iter_mut().filter(|s| matches!(s.status, StepState::Failed | StepState::Cancelled)) {
                let mut lines = Vec::new();
                if let Some(e) = s.error.take().filter(|e| s.log.last().is_none_or(|l| &l.text != e)) {
                    lines.push(LogLine { at: s.finished_at.unwrap_or(now), level: LogLevel::Error, text: e.chars().take(limits::LOG_LINE).collect() });
                }
                lines.push(line(LogLevel::Info, why.as_str()));
                push_log(s, lines);
                s.status = StepState::Pending;
                s.attempt = 0;
                s.started_at = None;
                s.finished_at = None;
                s.next_at = None;
                s.error = None;
                s.output = Value::Null;
            }
            r.run.status = RunState::Running;
            r.run.finished_at = None;
            r.run.reason = None;
            r.run.retried_at = Some(now);
            r.private.cleanup = false;
            r.next_at = Some(ms_now());
            Ok(Ok(Some(save_on(c, &r, None, false)?)))
        })?;
        let runs = match runs {
            Ok(Some(Some(r))) => r,
            Ok(_) => return Ok(None),
            Err(()) => return Err(crate::Error::Conflict("only a failed or cancelled run can be retried".into())),
        };
        let first = runs.first().cloned();
        for run in runs {
            let _ = hub.events.send(Arc::new(StreamEvent::AutomationRun { cursor: run.cursor, run: Box::new(run) }));
        }
        self.automations.wake.notify_one();
        Ok(first)
    }

    /// A thread a run started changed: look at the run now (inside the mirror's write).
    pub(crate) fn nudge_runs_of_thread(c: &Conn<'_>, machine_id: &str, thread: &str) -> code_sql::Result<usize> {
        c.execute(
            "UPDATE automation_runs SET next_at = ? WHERE status = 'waiting' AND (next_at IS NULL OR next_at > ?) \
             AND id IN (SELECT run_id FROM automation_threads WHERE machine_id = ? AND thread = ?)",
            params![ms_now(), ms_now(), machine_id, thread],
        )
    }
}

/// Owners kicked recently (so a busy owner nobody serves isn't kicked every round).
#[derive(Default)]
pub(crate) struct Kicks(parking_lot::Mutex<HashMap<String, std::time::Instant>>);

impl Kicks {
    pub(crate) fn due(&self, owner: &str) -> bool {
        let mut m = self.0.lock();
        if m.len() > 4096 {
            m.retain(|_, t| t.elapsed() < Duration::from_secs(60));
        }
        match m.get(owner) {
            Some(t) if t.elapsed() < Duration::from_secs(10) => false,
            _ => {
                m.insert(owner.to_string(), std::time::Instant::now());
                true
            }
        }
    }
}
