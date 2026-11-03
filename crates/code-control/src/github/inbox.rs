//! GitHub deliveries, durable until handled (docs/code/cluster.md, "GitHub webhooks").
//!
//! A delivery's per-repo parts are written to `github_inbox` (with its de-duplication row, in
//! one transaction) before anything runs, and each part is handled on its repo's owner. GitHub
//! is answered 2xx only once every part is either handled or recorded here, so an
//! acknowledged delivery is never lost:
//!
//! - **Handled in time**: the part's row is deleted; GitHub gets 200.
//! - **The handler failed** on a live owner while GitHub is still waiting: the delivery's rows and
//!   its de-duplication row are deleted and GitHub gets the error, so its redelivery runs the
//!   whole delivery again (as on a single node before).
//! - **The owner couldn't be reached, or died mid-way**: the rows stay and GitHub gets 202. The
//!   owner (whoever takes the repo next) handles them: on taking the repo over, when it's woken,
//!   and every few seconds.
//! - **Too slow** (past the budget GitHub allows): GitHub gets 202 and the part finishes in the
//!   background; if it fails then, it's retried with backoff.
//!
//! Each attempt holds a claim on its row (`next_at` moved a claim's length ahead on the
//! database's clock, a compare-and-swap like deliveries'), so two nodes never handle one part at
//! once; a node that dies holding one leaves it due again when the claim lapses. Handling is
//! at-least-once: a node that dies after handling a part but before deleting its row has it run
//! again (GitHub's handlers here are idempotent: upserts keyed by GitHub's ids).

use super::Hub;
use crate::{Code, Error, Result, new_id};
use code_sql::{OptionalExtension, params};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS github_inbox (
    id TEXT PRIMARY KEY,
    receipt TEXT NOT NULL,
    repo_id TEXT NOT NULL,
    event TEXT NOT NULL,
    payload TEXT NOT NULL,
    state TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_at INTEGER NOT NULL,
    error TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS github_inbox_due ON github_inbox (state, next_at);
CREATE INDEX IF NOT EXISTS github_inbox_repo ON github_inbox (repo_id, created_at);
CREATE INDEX IF NOT EXISTS github_inbox_receipt ON github_inbox (receipt);
"#;

/// How long one attempt holds a part (tests shorten it).
pub(super) const CLAIM: Duration = Duration::from_secs(20);
/// Attempts before a part is given up (`failed`, kept for the record).
const MAX_ATTEMPTS: i64 = 8;
/// First retry delay, then ×4 per attempt (at most an hour).
const RETRY_BASE_MS: i64 = 2_000;
/// How often an owner looks for due parts of its repos.
pub(super) const POLL: Duration = Duration::from_secs(5);

/// A part, persisted: (row id, repo id).
pub(super) type Part = (String, String);

/// What became of a part handled on its owner.
#[derive(Debug)]
pub(crate) enum Outcome {
    Done,
    /// The owner couldn't be reached or answered unavailable (or died mid-way): the row stays.
    Unavailable,
    /// The handler failed on a live owner.
    Failed(Error),
}

fn claim_ms(hub: &Hub) -> i64 {
    hub.inbox_claim_ms.load(Ordering::Relaxed) as i64
}

/// Record a delivery: its de-duplication row (if GitHub sent an id) and one row per repo part,
/// claimed by the receiving node for a claim's length. `None` if GitHub sent it before.
pub(super) fn persist(code: &Code, hub: &Hub, delivery: Option<&str>, event: &str, body: &[u8], repos: &[String]) -> Result<Option<(String, Vec<Part>, i64)>> {
    let receipt = delivery.map(str::to_string).unwrap_or_else(|| new_id("ghd"));
    let payload = String::from_utf8_lossy(body).into_owned();
    let now = code.db.sql().now_ms()?;
    let until = now + claim_ms(hub);
    let parts = code.db.write(|c| {
        if let Some(d) = delivery
            && c.execute("INSERT OR IGNORE INTO github_deliveries (id, at) VALUES (?, ?)", params![d, crate::db::now()])? == 0
        {
            return Ok(None);
        }
        let mut parts = Vec::new();
        for repo in repos {
            let id = new_id("ghi");
            c.execute(
                "INSERT INTO github_inbox (id, receipt, repo_id, event, payload, state, attempts, next_at, created_at) VALUES (?, ?, ?, ?, ?, 'pending', 0, ?, ?)",
                params![id, receipt, repo, event, payload, until, now],
            )?;
            parts.push((id, repo.clone()));
        }
        Ok(Some(parts))
    })?;
    Ok(parts.map(|p| (receipt, p, until)))
}

/// A deleted repo's parts go with it.
pub(crate) fn delete_repo(code: &Code, repo_id: &str) -> Result<()> {
    if code.github_hub().is_some() {
        code.db.sql().execute("DELETE FROM github_inbox WHERE repo_id = ?", [repo_id])?;
    }
    Ok(())
}

/// Forget a delivery whose handling failed while GitHub was waiting: GitHub's redelivery runs it
/// again from the start.
pub(super) fn forget(code: &Code, receipt: &str, delivery: Option<&str>) {
    let r = code.db.write(|c| {
        c.execute("DELETE FROM github_inbox WHERE receipt = ?", [receipt])?;
        if let Some(d) = delivery {
            c.execute("DELETE FROM github_deliveries WHERE id = ?", [d])?;
        }
        Ok(())
    });
    if let Err(e) = r {
        tracing::warn!(error = %e, "forgetting a failed GitHub delivery");
    }
}

/// Parts the receiving node couldn't get handled (their owner was unreachable): due now, for the
/// owner to pick up. Only rows still under the receipt's own claim (an owner that took one may
/// still be working on it).
pub(super) fn release(code: &Code, ids: &[String], claimed_until: i64) {
    let r = code.db.write(|c| {
        for id in ids {
            c.execute("UPDATE github_inbox SET next_at = 0 WHERE id = ? AND state = 'pending' AND next_at = ?", params![id, claimed_until])?;
        }
        Ok(())
    });
    if let Err(e) = r {
        tracing::warn!(error = %e, "releasing GitHub delivery parts");
    }
}

/// On the repo's owner: handle part `id` if it's still `expected` (its claim when it was sent
/// here, or the `next_at` it was due at). `Ok` if it's done (or someone else has it, or did it).
/// A failure is retried later with backoff (and reported).
pub(crate) async fn process(code: &Arc<Code>, repo_id: &str, id: &str, expected: i64) -> Result<()> {
    let Some(hub) = code.github_hub().cloned() else { return Err(Error::NotFound("GitHub App".into())) };
    let lock = hub.inbox_locks.lock().entry(repo_id.to_string()).or_default().clone();
    let _one = lock.lock().await;
    let now = code.db.sql().now_ms()?;
    let until = now + claim_ms(&hub);
    let row: Option<(String, String, i64)> = code.db.write(|c| {
        let n = c.execute(
            "UPDATE github_inbox SET next_at = ?, attempts = attempts + 1 WHERE id = ? AND repo_id = ? AND state = 'pending' AND next_at = ?",
            params![until, id, repo_id, expected],
        )?;
        if n == 0 {
            return Ok(None);
        }
        c.query_row("SELECT event, payload, attempts FROM github_inbox WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()
    })?;
    let Some((event, payload, attempts)) = row else { return Ok(()) };
    hub.inbox_started.fetch_add(1, Ordering::SeqCst);
    // The claim is renewed while the part runs (a push can take a while), so it only lapses
    // when this node stops: then another node takes the part within a claim's length.
    let work = async {
        while hub.inbox_hang.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        super::sync::handle_for_repo(code, repo_id, &event, payload.as_bytes()).await
    };
    let mut work = std::pin::pin!(work);
    let mut until = until;
    let mut renew = tokio::time::interval(Duration::from_millis((claim_ms(&hub) / 3).max(10) as u64));
    renew.tick().await;
    let result = loop {
        tokio::select! {
            r = &mut work => break r,
            _ = renew.tick() => {
                let next = code.db.sql().now_ms().unwrap_or(until) + claim_ms(&hub);
                match code.db.sql().execute("UPDATE github_inbox SET next_at = ? WHERE id = ? AND state = 'pending' AND next_at = ?", params![next, id, until]) {
                    Ok(1) => until = next,
                    Ok(_) => tracing::warn!(repo = %repo_id, part = %id, "lost the claim on a GitHub delivery part while handling it"),
                    Err(e) => tracing::warn!(repo = %repo_id, part = %id, error = %e, "renewing the claim on a GitHub delivery part"),
                }
            }
        }
    };
    let now = code.db.sql().now_ms().unwrap_or(now);
    match result {
        Ok(()) => {
            code.db.sql().execute("DELETE FROM github_inbox WHERE id = ? AND next_at = ?", params![id, until])?;
            Ok(())
        }
        Err(e) => {
            let msg = e.to_string();
            let (state, next) = if attempts >= MAX_ATTEMPTS {
                tracing::warn!(repo = %repo_id, event = %event, attempts, error = %msg, "giving up on a GitHub delivery");
                ("failed", i64::MAX / 2)
            } else {
                let delay = RETRY_BASE_MS.saturating_mul(4i64.saturating_pow((attempts - 1).max(0) as u32)).min(3_600_000);
                ("pending", now + delay)
            };
            code.db.sql().execute(
                "UPDATE github_inbox SET state = ?, next_at = ?, error = ? WHERE id = ? AND next_at = ?",
                params![state, next, msg, id, until],
            )?;
            wake_later(code, repo_id, Duration::from_millis((next - now).clamp(0, 3_600_000) as u64));
            Err(e)
        }
    }
}

/// Handle every due part of a repo, oldest first (on its owner).
pub(crate) async fn process_due(code: &Arc<Code>, repo_id: &str) {
    if code.github_hub().is_none() || !code.owns_repo(repo_id) {
        return;
    }
    let now = match code.db.sql().now_ms() {
        Ok(n) => n,
        Err(e) => return tracing::warn!(repo = %repo_id, error = %e, "reading GitHub deliveries"),
    };
    let due: Vec<(String, i64)> = match code.db.sql().query(
        "SELECT id, next_at FROM github_inbox WHERE repo_id = ? AND state = 'pending' AND next_at <= ? ORDER BY created_at, id LIMIT 100",
        params![repo_id, now],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ) {
        Ok(d) => d,
        Err(e) => return tracing::warn!(repo = %repo_id, error = %e, "reading GitHub deliveries"),
    };
    for (id, next_at) in due {
        if !code.owns_repo(repo_id) {
            return;
        }
        if let Err(e) = process(code, repo_id, &id, next_at).await {
            tracing::warn!(repo = %repo_id, error = %e, "a GitHub delivery failed; it will be retried");
        }
    }
}

/// Look again at a repo's parts after `after` (a retry's backoff), on whichever node owns it then.
fn wake_later(code: &Arc<Code>, repo_id: &str, after: Duration) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let (me, id) = (Arc::downgrade(code), repo_id.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        if let Some(code) = me.upgrade() {
            if code.owns_repo(&id) {
                process_due(&code, &id).await;
            } else {
                code.kick_owner(&id);
            }
        }
    });
}

/// Every few seconds: the due parts of the repos this node owns (a node that died holding a
/// claim, a receiving node that couldn't reach the owner, retries after a restart).
pub(super) fn start_poll(code: &Arc<Code>) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let me = Arc::downgrade(code);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(POLL).await;
            let Some(code) = me.upgrade() else { return };
            let now = match code.db.sql().now_ms() {
                Ok(n) => n,
                Err(_) => continue,
            };
            let repos: Vec<String> = match code.db.sql().query(
                "SELECT DISTINCT repo_id FROM github_inbox WHERE state = 'pending' AND next_at <= ? LIMIT 1000",
                [now],
                |r| r.get(0),
            ) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "looking for due GitHub deliveries");
                    continue;
                }
            };
            for repo in repos {
                if code.owns_repo(&repo) {
                    process_due(&code, &repo).await;
                } else if code.cluster().is_some() {
                    // Nobody may own it (an idle repo given up): this makes its owner take it.
                    code.kick_owner(&repo);
                }
            }
        }
    });
}

impl Code {
    /// Tests: GitHub deliveries handled on this node wait (as if stuck) while set.
    #[doc(hidden)]
    pub fn github_hang_deliveries(&self, on: bool) {
        if let Some(h) = self.github_hub() {
            h.inbox_hang.store(on, Ordering::SeqCst);
        }
    }

    /// Tests: GitHub delivery parts this node started handling.
    #[doc(hidden)]
    pub fn github_deliveries_started(&self) -> usize {
        self.github_hub().map_or(0, |h| h.inbox_started.load(Ordering::SeqCst))
    }

    /// Tests: how long one attempt at a GitHub delivery part holds it.
    #[doc(hidden)]
    pub fn set_github_delivery_claim(&self, d: Duration) {
        if let Some(h) = self.github_hub() {
            h.inbox_claim_ms.store(d.as_millis() as u64, Ordering::SeqCst);
        }
    }
}
