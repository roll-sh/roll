//! Settling and snoozing threads (docs/code/agents.md, "Settling"). Ported from T3 Code: the
//! settle, un-settle and snooze commands and their checks in `Orchestrator.ts`, and the
//! automatic rules of `ThreadSettlementService.ts` (a sweep every minute: inactivity, a change
//! that landed or closed). T3's clients work out when a snooze is over; here the sweep wakes it
//! and records `woke_at`, so every device agrees.

use super::{Fail, Host, Res, Rt, now};
use agent_proto::*;
use std::sync::Weak;
use std::time::Duration;

const DAY: Millis = 86_400_000;
/// The furthest a snooze may reach.
const MAX_SNOOZE: Millis = 366 * DAY;
/// How often the daemon looks for threads to settle and snoozes to wake (T3's sweep).
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// What keeps a thread in the inbox whatever the user wants, refused with T3's reasons.
fn parkable(t: &Thread, busy: bool, what: &str) -> Res<()> {
    if t.archived_at.is_some() {
        return Err(Fail::Conflict(format!("the thread is archived: unarchive it before {what}")));
    }
    if t.pending_requests > 0 {
        return Err(Fail::Conflict(format!("the thread is waiting on an approval or question: answer it before {what}")));
    }
    if !t.queued.is_empty() {
        return Err(Fail::Conflict(format!("the thread has a queued message: send it or remove it from the queue before {what}")));
    }
    if busy {
        return Err(Fail::Conflict(format!("the thread is still running: stop it before {what}")));
    }
    Ok(())
}

/// Whether the daemon may settle the thread by itself at `at` (T3's `isAutoSettleCandidate`).
fn may_auto_settle(t: &Thread, busy: bool, at: Millis) -> bool {
    t.archived_at.is_none()
        && t.inbox.settle.is_none()
        && !t.inbox.manual
        && !t.inbox.is_snoozed(at)
        && t.pending_requests == 0
        && t.queued.is_empty()
        && t.active_run.is_none()
        && !t.status.is_busy()
        && !busy
}

/// The last thing that happened in the thread (T3's latest activity): a message or a run
/// starting or ending, the thread's creation, a snooze waking. Renaming or visiting isn't.
fn last_activity(t: &Thread, runs: &[Run]) -> Millis {
    let runs = runs.iter().flat_map(|r| [Some(r.requested_at), r.started_at, r.completed_at]).flatten();
    let snooze_over = t.inbox.snooze.map(|s| s.until);
    runs.chain([Some(t.created_at), t.inbox.woke_at, snooze_over].into_iter().flatten()).max().unwrap_or(t.created_at)
}

/// A request or a failure ends a snooze early (T3's `isThreadSnoozeWokenEarly`).
pub(crate) fn wake_early(inbox: &mut Inbox, at: Millis) {
    if inbox.snooze.take().is_some() {
        inbox.woke_at = Some(at);
    }
}

pub(crate) fn spawn_sweeper(host: Weak<Host>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP_EVERY).await;
            let Some(h) = host.upgrade() else { return };
            h.settle_round(now()).await;
        }
    });
}

impl Host {
    pub(crate) async fn thread_settle(&self, id: &ThreadId) -> Res<Reply> {
        let mut rt = self.lock(id).await?;
        let busy = rt.active.is_some() || rt.thread.status.is_busy();
        parkable(&rt.thread, busy, "settling")?;
        let inbox = &mut rt.thread.inbox;
        if !inbox.is_settled() {
            inbox.settle = Some(Settle::Settled { at: now(), by: SettledBy::User });
        }
        inbox.snooze = None;
        // Like T3, a settled thread lets go of its idle provider session, and of its terminals
        // that have finished (T3's `closeIdle`; a running one, a dev server, stays).
        self.close_session(&mut rt).await;
        let finished = self.close_terminals(|x| x.thread.as_ref() == Some(id) && x.status == TerminalStatus::Exited)?;
        self.put_thread(&mut rt)?;
        self.put(finished)?;
        Ok(Reply::Ok)
    }

    pub(crate) async fn thread_unsettle(&self, id: &ThreadId, restore: Option<InboxBefore>) -> Res<Reply> {
        let mut rt = self.lock(id).await?;
        let inbox = &mut rt.thread.inbox;
        match restore {
            None => inbox.settle = Some(Settle::Active { at: now() }),
            // Undoing a settle: only the user's, still standing (new activity or another device
            // may have moved it since). A settle that found it settled changed nothing to undo.
            Some(before) => {
                let by_user = matches!(inbox.settle, Some(Settle::Settled { by: SettledBy::User, .. }));
                if !by_user || matches!(before.settle, Some(Settle::Settled { .. })) {
                    return Ok(Reply::Ok);
                }
                inbox.settle = before.settle;
                // A snooze comes back while it still has time to run.
                if inbox.snooze.is_none() {
                    inbox.snooze = before.snooze.filter(|s| s.until > now());
                }
            }
        }
        self.put_thread(&mut rt)?;
        Ok(Reply::Ok)
    }

    pub(crate) async fn thread_snooze(&self, id: &ThreadId, until: Millis) -> Res<Reply> {
        let at = now();
        if until <= at {
            return Err(Fail::Invalid("choose a time in the future to snooze until".into()));
        }
        if until - at > MAX_SNOOZE {
            return Err(Fail::Invalid("a thread can be snoozed for at most a year".into()));
        }
        let mut rt = self.lock(id).await?;
        parkable(&rt.thread, false, "snoozing")?;
        let inbox = &mut rt.thread.inbox;
        // It comes back to the inbox when it wakes, not to the settled shelf.
        if inbox.is_settled() {
            inbox.settle = None;
        }
        inbox.snooze = Some(Snooze { until, at });
        self.put_thread(&mut rt)?;
        Ok(Reply::Ok)
    }

    pub(crate) async fn thread_unsnooze(&self, id: &ThreadId) -> Res<Reply> {
        let mut rt = self.lock(id).await?;
        if rt.thread.inbox.snooze.take().is_some() {
            self.put_thread(&mut rt)?;
        }
        Ok(Reply::Ok)
    }

    /// Opening a conversation isn't activity: `updated_at` stays (T3's `thread.visit`).
    pub(crate) async fn thread_visit(&self, id: &ThreadId) -> Res<Reply> {
        let mut rt = self.lock(id).await?;
        rt.thread.inbox.visited_at = Some(now());
        self.put(vec![Entity::Thread(rt.thread.clone())])?;
        drop(rt);
        // Its files may have changed outside roll (an editor, another terminal): counted again as
        // it's opened, once, quietly (one thread: what changed since its base).
        self.touch_diff(id);
        Ok(Reply::Ok)
    }

    pub(crate) async fn thread_auto_settle(&self, id: &ThreadId, enabled: bool) -> Res<Reply> {
        let mut rt = self.lock(id).await?;
        if rt.thread.inbox.manual == enabled {
            rt.thread.inbox.manual = !enabled;
            self.put_thread(&mut rt)?;
        }
        Ok(Reply::Ok)
    }

    /// The thread's change landed or closed: settle it if auto-settle may (T3 settles on a
    /// merged PR when `sidebarAutoSettleOnMerge` is on, and on a closed one always).
    pub(crate) async fn thread_change_ended(&self, id: &ThreadId, landed: bool) -> Res<Reply> {
        let mut rt = self.lock(id).await?;
        if std::mem::take(&mut rt.private.change_open) {
            self.save_private(&rt)?;
        }
        let rules = self.project(&rt.thread.project).map(|p| p.settings.auto_settle).unwrap_or_default();
        if (landed && !rules.on_land) || !may_auto_settle(&rt.thread, rt.active.is_some(), now()) {
            return Ok(Reply::Ok);
        }
        let last = last_activity(&rt.thread, &self.runs_of(id)?);
        self.auto_settle(&mut rt, last, if landed { SettledBy::Landed } else { SettledBy::Closed })?;
        Ok(Reply::Ok)
    }

    /// T3 dates an automatic settle at the thread's last activity, so the shelf reads in the
    /// order things happened.
    fn auto_settle(&self, rt: &mut Rt, last: Millis, by: SettledBy) -> anyhow::Result<()> {
        rt.thread.inbox.settle = Some(Settle::Settled { at: last, by });
        rt.thread.inbox.snooze = None;
        self.put_thread(rt)
    }

    /// One sweep at `at` (the daemon runs one a minute; tests pass a later clock): snoozes that
    /// are due wake, and threads inactive for their project's `auto_settle.after_days` settle.
    /// After it, the cleanup round: idle sessions and old settled worktrees.
    pub async fn settle_round(&self, at: Millis) {
        if let Err(e) = self.try_settle_round(at).await {
            tracing::warn!("settling threads: {e:#}");
        }
        if let Err(e) = self.cleanup_round(at).await {
            tracing::warn!("cleaning up: {e:#}");
        }
    }

    async fn try_settle_round(&self, at: Millis) -> anyhow::Result<()> {
        let mut rules = std::collections::HashMap::new();
        for entity in self.store.list(EntityKind::Project)? {
            if let Entity::Project(p) = entity {
                rules.insert(p.id, p.settings.auto_settle);
            }
        }
        for entity in self.store.list(EntityKind::Thread)? {
            let Entity::Thread(t) = entity else { continue };
            let wakes = t.inbox.snooze.is_some_and(|s| s.until <= at);
            let rule = rules.get(&t.project).copied();
            let days = rule.and_then(|r| r.after_days);
            if !wakes && (days.is_none() || !may_auto_settle(&t, false, at)) {
                continue;
            }
            // Decide again on the live record: it may have moved since the list.
            let Ok(mut rt) = self.lock(&t.id).await else { continue };
            if let Some(s) = rt.thread.inbox.snooze.filter(|s| s.until <= at) {
                rt.thread.inbox.snooze = None;
                rt.thread.inbox.woke_at = Some(s.until);
                self.put_thread(&mut rt)?;
            }
            let Some(days) = days else { continue };
            if !may_auto_settle(&rt.thread, rt.active.is_some(), at) {
                continue;
            }
            // T3's `keepPullRequests`: an open change keeps it; landing or closing settles it.
            if rule.is_some_and(|r| r.keep_changes) && rt.private.change_open {
                continue;
            }
            let last = last_activity(&rt.thread, &self.runs_of(&t.id)?);
            if last.saturating_add(u64::from(days) * DAY) <= at {
                self.auto_settle(&mut rt, last, SettledBy::Inactivity)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(requested: Millis, completed: Option<Millis>) -> Run {
        Run {
            id: RunId::from_suffix("0000000000000001"),
            thread: ThreadId::from_suffix("0000000000000001"),
            ordinal: 1,
            provider: ProviderId::codex(),
            model: None,
            status: RunStatus::Completed,
            prompt: String::new(),
            attachments: vec![],
            steers: vec![],
            steered_at: vec![],
            idle_compaction: false,
            delivers: Vec::new(),
            requested_at: requested,
            started_at: Some(requested + 1),
            completed_at: completed,
            checkpoint: None,
            stat: None,
            files: Vec::new(),
            usage: Usage::default(),
            context: None,
            error: None,
        }
    }

    #[test]
    fn activity_is_the_newest_of_runs_creation_and_waking() {
        let t: Thread = serde_json::from_value(serde_json::json!({
            "id": "thr_0000000000000001", "project": "prj_0000000000000001", "title": "t",
            "provider": "codex", "model": null, "runtime_mode": "ask", "interaction_mode": "default",
            "cwd": "/", "status": "idle", "run_count": 1, "pending_requests": 0,
            "created_at": 10, "updated_at": 999
        }))
        .unwrap();
        assert_eq!(last_activity(&t, &[]), 10);
        assert_eq!(last_activity(&t, &[run(20, Some(50)), run(30, None)]), 50, "updated_at isn't activity");
        let woke = Thread { inbox: Box::new(Inbox { woke_at: Some(70), ..Inbox::default() }), ..t.clone() };
        assert_eq!(last_activity(&woke, &[run(20, Some(50))]), 70);
    }
}
