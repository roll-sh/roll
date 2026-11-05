//! Idle compaction (docs/code/agents.md, "Turns"): T3 Code's `beforeCacheExpires`, ported from
//! `apps/server/src/orchestration-v2/IdleCompaction.ts` and `IdleCompactionService.ts` (MIT).
//!
//! A big conversation left idle is compacted just before its provider's prompt cache expires:
//! after that, the next message pays for an uncached read of all of it. The window ends when the
//! cache of the run's last request expires (Claude: an hour on subscriptions, five minutes on API
//! keys, as its usage reports) and opens [`LEAD_MS`] before, so the session can resume and send
//! the compaction while the cache is warm. Only threads the machine's settings say so for
//! ([`IdleCompaction::BeforeCacheExpires`]); the default, on return, is the composer's "Compact and
//! send". Like T3, the timers live in memory: a restart drops them.

use super::runs::StartedBy;
use super::{Host, now};
use crate::provider::{COMPACT, TurnInput};
use agent_proto::*;
use std::time::Duration;

/// Start this long before the cache expires.
pub(crate) const LEAD_MS: u64 = 2 * 60_000;

/// When an idle thread should compact (start, end), or `None` when it shouldn't: only its newest
/// run, completed on the thread's provider, not itself a compaction, with nothing waiting on
/// anyone, over [`IDLE_COMPACTION_MIN_TOKENS`], and a cache lifetime its provider reported that
/// hasn't run out.
pub(crate) fn window(thread: &Thread, run: &Run, now: Millis) -> Option<(Millis, Millis)> {
    if thread.archived_at.is_some() || thread.inbox.is_settled() || thread.inbox.is_snoozed(now) {
        return None;
    }
    if thread.active_run.is_some() || thread.pending_requests > 0 || !thread.queued.is_empty() {
        return None;
    }
    if run.thread != thread.id || run.ordinal != thread.run_count || run.status != RunStatus::Completed || run.provider != thread.provider {
        return None;
    }
    // Never compact a compaction, so one idle spell compacts at most once.
    if run.idle_compaction || (run.attachments.is_empty() && run.prompt.trim() == COMPACT) {
        return None;
    }
    let context = thread.context?;
    if context.used_tokens < IDLE_COMPACTION_MIN_TOKENS {
        return None;
    }
    let end = run.completed_at? + context.cache_ttl_ms?;
    (now < end).then(|| (end.saturating_sub(LEAD_MS), end))
}

impl Host {
    /// After a run completes: on a thread whose provider compacts idle threads before the cache
    /// expires, a timer for its window (replacing the thread's earlier one).
    pub(crate) fn schedule_idle_compaction(&self, thread: &Thread, run: &Run) {
        if self.machine_settings().idle_compaction(&thread.provider) != IdleCompaction::BeforeCacheExpires {
            return;
        }
        let Some((start, _)) = window(thread, run, now()) else { return };
        let (me, id, run_id) = (self.me.clone(), thread.id.clone(), run.id.clone());
        let timer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(start.saturating_sub(now()))).await;
            if let Some(host) = me.upgrade() {
                host.idle_compact(&id, &run_id).await;
            }
        });
        if let Some(old) = self.idle_timers.lock().unwrap().insert(thread.id.clone(), timer.abort_handle()) {
            old.abort();
        }
    }

    /// The window opened: if nothing changed since (the same newest run, still idle, still asked
    /// for, a provider that compacts), compact as a run the daemon started.
    async fn idle_compact(&self, id: &ThreadId, run: &RunId) {
        self.idle_timers.lock().unwrap().remove(id);
        let Ok(mut rt) = self.lock(id).await else { return };
        if rt.active.is_some() || self.machine_settings().idle_compaction(&rt.thread.provider) != IdleCompaction::BeforeCacheExpires {
            return;
        }
        match self.offered(&rt.thread.provider) {
            Ok(provider) if provider.capabilities().compact => {}
            _ => return,
        }
        let newest = match self.runs_of(id) {
            Ok(runs) => runs.into_iter().max_by_key(|r| r.ordinal),
            Err(e) => {
                tracing::warn!("idle compaction of {id}: {e:#}");
                return;
            }
        };
        let Some(newest) = newest.filter(|r| &r.id == run) else { return };
        if window(&rt.thread, &newest, now()).is_none() {
            return;
        }
        let input = TurnInput { text: COMPACT.into(), attachments: Vec::new() };
        if let Err(e) = self.start_run_as(&mut rt, input, StartedBy::IdleCompaction).await {
            tracing::warn!("idle compaction of {id}: {e:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 60 * 60_000;

    fn idle(used: u64, ttl: Option<u64>) -> (Thread, Run) {
        let thread: Thread = serde_json::from_value(serde_json::json!({
            "id": "thr_1", "project": "prj_1", "title": "t", "provider": "claude", "model": null,
            "runtime_mode": "full_access", "interaction_mode": "default", "cwd": "/tmp", "status": "idle",
            "run_count": 1, "pending_requests": 0, "created_at": 0, "updated_at": 0,
            "context": { "used_tokens": used, "max_tokens": 1_000_000, "cache_ttl_ms": ttl },
        }))
        .unwrap();
        let run: Run = serde_json::from_value(serde_json::json!({
            "id": "run_1", "thread": "thr_1", "ordinal": 1, "provider": "claude", "model": null, "status": "completed",
            "prompt": "refactor it", "requested_at": 0, "completed_at": 1_000,
        }))
        .unwrap();
        (thread, run)
    }

    #[test]
    fn the_window_ends_when_the_cache_expires_and_opens_two_minutes_before() {
        let (t, r) = idle(250_000, Some(HOUR));
        assert_eq!(window(&t, &r, 2_000), Some((1_000 + HOUR - LEAD_MS, 1_000 + HOUR)));
        // Inside the lead it still fires; once the cache expired, compacting saves nothing.
        assert!(window(&t, &r, 1_000 + HOUR - 1).is_some());
        assert_eq!(window(&t, &r, 1_000 + HOUR), None);
    }

    #[test]
    fn only_big_idle_conversations_with_a_known_cache_compact() {
        let now = 2_000;
        let (t, r) = idle(IDLE_COMPACTION_MIN_TOKENS - 1, Some(HOUR));
        assert_eq!(window(&t, &r, now), None, "small: a cold read is cheap");
        let (t, r) = idle(250_000, None);
        assert_eq!(window(&t, &r, now), None, "served from the cache: the lifetime is unknown");

        let (t, r) = idle(250_000, Some(HOUR));
        let no = |t: &Thread, r: &Run| assert_eq!(window(t, r, now), None);
        no(&Thread { pending_requests: 1, ..t.clone() }, &r);
        no(&Thread { active_run: Some(RunId::from_suffix("2")), ..t.clone() }, &r);
        no(&Thread { archived_at: Some(1), ..t.clone() }, &r);
        no(&Thread { run_count: 2, ..t.clone() }, &r);
        no(&Thread { provider: ProviderId::codex(), ..t.clone() }, &r);
        no(&Thread { queued: vec![QueuedMessage { id: "q".into(), text: "x".into(), attachments: vec![], at: 0, delivers: vec![] }], ..t.clone() }, &r);
        let mut settled = t.clone();
        settled.inbox.settle = Some(Settle::Settled { at: 1, by: SettledBy::User });
        no(&settled, &r);
        let mut snoozed = t.clone();
        snoozed.inbox.snooze = Some(Snooze { until: now + 1, at: 0 });
        no(&snoozed, &r);
        no(&t, &Run { status: RunStatus::Interrupted, ..r.clone() });
        // A compaction, by hand or idle, is never compacted again.
        no(&t, &Run { prompt: "/compact".into(), ..r.clone() });
        no(&t, &Run { idle_compaction: true, ..r.clone() });
    }
}
