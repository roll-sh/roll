//! Approval notifications (docs/secrets.md, "Protected environments and approvals"): when a change
//! request opens on an environment whose policy has a `notify_url`, its approvers hear about it.
//!
//! The notice is written to an outbox in the request's own transaction and delivered by the
//! sweeper (never inline from a request), with retries. Its body works as a Slack or Discord
//! incoming webhook (`text`, `content`) and carries the request's fields for anything else: key
//! names, never values.
//!
//! Rotation reminders use the same outbox: once a day, an environment with a `max_age_days` limit
//! and a notify URL hears which of its keys are older than that (names only).

use crate::{Result, Secrets, Value, names};
use code_sql::{Conn, params};
use serde_json::{Value as Json, json};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS notices (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    env_id TEXT NOT NULL,
    body TEXT NOT NULL,
    attempts INTEGER NOT NULL,
    next_at INTEGER NOT NULL,
    sent_at INTEGER,
    created_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS notices_due ON notices (sent_at, next_at);
";

/// Tries before a notice is given up on, and the waits between them.
const ATTEMPTS: i64 = 6;
const BACKOFF_MS: i64 = 60_000;

/// Queue a notice (inside a write).
#[allow(clippy::too_many_arguments)]
pub(crate) fn queue(c: &Conn<'_>, project: &str, env_id: &str, env: &str, request: &str, by: &str, message: &str, keys: &[String], link: Option<String>, now: i64) -> Result<()> {
    let shown: Vec<&String> = keys.iter().take(20).collect();
    let more = keys.len().saturating_sub(shown.len());
    let list = shown.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", ") + &if more > 0 { format!(" and {more} more") } else { String::new() };
    let what = if message.is_empty() { String::new() } else { format!(": {message}") };
    let mut text = format!("roll secrets: {by} opened change request {request} on {env}{what} ({list}). It needs an approval.");
    if let Some(l) = &link {
        text.push_str(&format!(" Review: {l}"));
    }
    // A request's message and key names are the requester's words: no Slack markup (`<!channel>`,
    // links) and no Discord mentions.
    let slack = text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let body = json!({ "text": slack, "content": text, "allowed_mentions": { "parse": [] }, "project": project, "environment": env, "request": { "id": request, "by": by, "message": message, "keys": keys }, "link": link });
    c.execute(
        "INSERT INTO notices (id, project_id, env_id, body, attempts, next_at, sent_at, created_at) VALUES (?, ?, ?, ?, 0, ?, NULL, ?)",
        params![names::new_id("ntc", 16), project, env_id, body.to_string(), now, now],
    )?;
    Ok(())
}

/// A day: how often an environment is reminded of its stale keys.
const REMIND_EVERY_MS: i64 = 86_400_000;

impl Secrets {
    /// The sweeper: queue each due environment's rotation reminder (one write each: the day is
    /// claimed and the notice queued together, so two nodes never both remind). Returns how many.
    pub(crate) fn sweep_reminders(&self) -> Result<usize> {
        let now = self.db.now_ms()?;
        type Due = (String, String, String, i64, Option<i64>);
        let due: Vec<Due> = self.db.read(|c| {
            c.query(
                "SELECT id, project_id, slug, max_age_days, reminded_at FROM environments
                 WHERE max_age_days IS NOT NULL AND notify_url IS NOT NULL AND COALESCE(reminded_at, 0) <= ? LIMIT 100",
                params![now - REMIND_EVERY_MS],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
        })?;
        let mut queued = 0;
        for (env_id, project, slug, days, at) in due {
            let link = self.web_url.as_ref().map(|w| format!("{}/dashboard/projects/{project}/secrets?env={slug}", w.trim_end_matches('/')));
            queued += self.db.write(|c| {
                if c.execute("UPDATE environments SET reminded_at = ? WHERE id = ? AND COALESCE(reminded_at, 0) = ?", params![now, env_id, at.unwrap_or(0)])? != 1 {
                    return Ok::<_, crate::Error>(0);
                }
                // Values that only change when someone changes them: not live or dynamic links.
                let stale: Vec<String> = c.query(
                    "SELECT key FROM secrets WHERE env_id = ? AND owner = '' AND updated_at < ? AND (link IS NULL OR link LIKE '%\"mode\":\"managed\"%') ORDER BY key",
                    params![env_id, now - days * 86_400_000],
                    |r| r.get(0),
                )?;
                if stale.is_empty() {
                    return Ok(0);
                }
                let shown = stale.iter().take(20).cloned().collect::<Vec<_>>().join(", ");
                let more = if stale.len() > 20 { format!(" and {} more", stale.len() - 20) } else { String::new() };
                let mut text = format!("roll secrets: {} in {slug} unchanged for over {days} days ({shown}{more}). Rotate them, or raise the limit.", crate::write::plural(stale.len(), "key"));
                if let Some(l) = &link {
                    text.push_str(&format!(" {l}"));
                }
                let body = json!({ "text": text, "content": text, "allowed_mentions": { "parse": [] }, "project": project, "environment": slug, "stale": stale, "max_age_days": days, "link": link });
                c.execute(
                    "INSERT INTO notices (id, project_id, env_id, body, attempts, next_at, sent_at, created_at) VALUES (?, ?, ?, ?, 0, ?, NULL, ?)",
                    params![names::new_id("ntc", 16), project, env_id, body.to_string(), now, now],
                )?;
                Ok(1)
            })?;
        }
        Ok(queued)
    }

    /// The sweeper: deliver due notices. Returns (sent, failed).
    pub(crate) fn sweep_notices(&self) -> Result<(usize, usize)> {
        let Some(deliver) = self.deliver.clone() else { return Ok((0, 0)) };
        let now = self.db.now_ms()?;
        // The environment's URL as it is now (sealed: a chat's incoming webhook URL is a credential);
        // none any more, and the notice is dropped.
        type Due = (String, String, String, Option<Vec<u8>>, String, i64);
        let due: Vec<Due> = self.db.read(|c| {
            c.query(
                "SELECT n.id, n.project_id, n.env_id, e.notify_url, n.body, n.attempts FROM notices n LEFT JOIN environments e ON e.id = n.env_id
                 WHERE n.sent_at IS NULL AND n.next_at <= ? AND n.attempts < ? LIMIT 50",
                params![now, ATTEMPTS],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
        })?;
        let (mut sent, mut failed) = (0, 0);
        for (id, project, env_id, sealed, body, attempts) in due {
            let Some(sealed) = sealed else {
                self.db.write(|c| c.execute("DELETE FROM notices WHERE id = ?", [&id]))?;
                continue;
            };
            let url = match self.project(&project).and_then(|dek| dek.open(&notify_aad(&project, &env_id), &sealed).map_err(crate::Error::from)) {
                Ok(u) => u,
                Err(e) => {
                    tracing::warn!(notice = %id, "an approval notice's URL doesn't open: {e}");
                    continue;
                }
            };
            // The claim: push next_at out first, so another node skips it meanwhile.
            let hold = now + BACKOFF_MS * (attempts + 1);
            if self.db.write(|c| c.execute("UPDATE notices SET attempts = attempts + 1, next_at = ? WHERE id = ? AND attempts = ? AND sent_at IS NULL", params![hold, id, attempts]))? != 1 {
                continue;
            }
            let body: Json = serde_json::from_str(&body).unwrap_or(Json::Null);
            match deliver.post(url.expose(), &id, &Value::default(), &body) {
                Ok(()) => {
                    sent += 1;
                    self.db.write(|c| c.execute("UPDATE notices SET sent_at = ? WHERE id = ?", params![now, id]))?;
                }
                Err(e) => {
                    failed += 1;
                    tracing::warn!(notice = %id, attempt = attempts + 1, "an approval notice failed: {e}");
                }
            }
        }
        // Delivered or given up on a week ago: gone.
        let old = now - 7 * 86_400_000;
        self.db.write(|c| c.execute("DELETE FROM notices WHERE created_at < ? AND (sent_at IS NOT NULL OR attempts >= ?)", params![old, ATTEMPTS]))?;
        Ok((sent, failed))
    }
}

/// The associated data of an environment's sealed notify URL.
pub(crate) fn notify_aad(project: &str, env_id: &str) -> String {
    format!("roll-secrets:notify:{project}:{env_id}")
}
