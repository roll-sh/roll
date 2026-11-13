//! Read alerts (docs/secrets.md, "Read alerts"): reads of values that look unlike the reader,
//! told to the environment's notify URL (names, addresses and counts, never a value):
//! - a token or workload identity reading from an address it hasn't read from before;
//! - one principal revealing `VOLUME` values of an environment within an hour.
//!
//! The sweep walks the audit log after a cursor. Advancing the cursor, recording what was seen
//! and queueing the notices are one write, so two nodes never both alert.

use crate::{Result, Secrets, from_json, names};
use code_sql::{OptionalExtension, params};
use serde_json::json;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS read_addresses (
    actor TEXT NOT NULL,
    ip TEXT NOT NULL,
    first_at INTEGER NOT NULL,
    PRIMARY KEY (actor, ip)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS read_bursts (
    actor TEXT NOT NULL,
    env_id TEXT NOT NULL,
    hour INTEGER NOT NULL,
    PRIMARY KEY (actor, env_id, hour)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS alert_cursor (
    name TEXT PRIMARY KEY,
    audit_id INTEGER NOT NULL
) WITHOUT ROWID;
";

/// Reveals by one principal in one environment within an hour that make an alert.
pub(crate) const VOLUME: i64 = 30;
const HOUR_MS: i64 = 3_600_000;
const BATCH: i64 = 1000;

/// The audit actions that read values.
const READS: &str = "'secrets.reveal', 'secrets.export', 'secrets.resolve'";

impl Secrets {
    /// Look at the audit rows since the last sweep and queue an alert for each odd read.
    /// Returns how many alerts were queued.
    pub(crate) fn sweep_alerts(&self) -> Result<usize> {
        let now = self.db.now_ms()?;
        // Most sweeps find nothing new: say so without a write.
        let (cursor, top): (Option<i64>, i64) = self.db.read(|c| {
            let cursor = c.query_row("SELECT audit_id FROM alert_cursor WHERE name = 'reads'", (), |r| r.get::<_, i64>(0)).optional()?;
            let top = c.query_row("SELECT COALESCE(MAX(id), 0) FROM audit", (), |r| r.get(0))?;
            Ok::<_, crate::Error>((cursor, top))
        })?;
        if cursor == Some(top) {
            return Ok(0);
        }
        self.db.write(|c| {
            c.execute("DELETE FROM read_bursts WHERE hour < ?", [now / HOUR_MS - 2])?;
            let cursor: Option<i64> = c.query_row("SELECT audit_id FROM alert_cursor WHERE name = 'reads'", (), |r| r.get(0)).optional()?;
            let Some(cursor) = cursor else {
                // The first sweep starts from now: history isn't alerted on.
                let top: i64 = c.query_row("SELECT COALESCE(MAX(id), 0) FROM audit", (), |r| r.get(0))?;
                c.execute("INSERT INTO alert_cursor (name, audit_id) VALUES ('reads', ?) ON CONFLICT (name) DO NOTHING", [top])?;
                return Ok(0);
            };
            type Read = (i64, String, i64, String, String, Option<String>, Option<String>, String);
            let rows: Vec<Read> = c.query(
                &format!("SELECT id, project_id, at, actor, action, env, ip, keys FROM audit WHERE id > ? AND action IN ({READS}) ORDER BY id LIMIT ?"),
                params![cursor, BATCH],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)),
            )?;
            let top = match rows.last() {
                Some(r) => r.0,
                // Nothing read: jump over the other rows.
                None => c.query_row("SELECT COALESCE(MAX(id), ?) FROM audit", [cursor], |r| r.get(0))?,
            };
            if c.execute("UPDATE alert_cursor SET audit_id = ? WHERE name = 'reads' AND audit_id = ?", params![top, cursor])? != 1 {
                return Ok(0);
            }
            let mut queued = 0;
            for (_, project, at, actor, action, env, ip, keys) in rows {
                let Some(env) = env else { continue };
                let mut said = vec![];
                // A machine's new address. Its first address isn't news: that's where it lives.
                if let Some(ip) = ip.filter(|_| actor.starts_with("token:") || actor.starts_with("identity:"))
                    && c.execute("INSERT INTO read_addresses (actor, ip, first_at) VALUES (?, ?, ?) ON CONFLICT (actor, ip) DO NOTHING", params![actor, ip, at])? == 1
                {
                    let known: i64 = c.query_row("SELECT COUNT(*) FROM read_addresses WHERE actor = ?", [&actor], |r| r.get(0))?;
                    if known > 1 {
                        let n = from_json::<Vec<String>>(&keys).len();
                        said.push((format!("{actor} read {} in {env} from {ip}, an address it hasn't read from before", crate::write::plural(n, "value")), json!({ "kind": "new_address", "actor": actor, "ip": ip })));
                    }
                }
                // Many values revealed one by one: someone copying an environment out.
                if action == "secrets.reveal" {
                    let n: i64 = c.query_row(
                        "SELECT COUNT(*) FROM audit WHERE project_id = ? AND env = ? AND actor = ? AND action = 'secrets.reveal' AND at > ? AND at <= ?",
                        params![project, env, actor, at - HOUR_MS, at],
                        |r| r.get(0),
                    )?;
                    if n >= VOLUME {
                        let env_id: Option<String> = c.query_row("SELECT id FROM environments WHERE project_id = ? AND slug = ?", [&project, &env], |r| r.get(0)).optional()?;
                        if let Some(env_id) = env_id
                            && c.execute("INSERT INTO read_bursts (actor, env_id, hour) VALUES (?, ?, ?) ON CONFLICT (actor, env_id, hour) DO NOTHING", params![actor, env_id, at / HOUR_MS])? == 1
                        {
                            said.push((format!("{actor} revealed {n} values in {env} within an hour"), json!({ "kind": "many_reveals", "actor": actor, "count": n })));
                        }
                    }
                }
                if said.is_empty() {
                    continue;
                }
                // Only an environment with somewhere to say it.
                let target: Option<String> = c.query_row("SELECT id FROM environments WHERE project_id = ? AND slug = ? AND notify_url IS NOT NULL", [&project, &env], |r| r.get(0)).optional()?;
                let Some(env_id) = target else { continue };
                let link = self.web_url.as_ref().map(|w| format!("{}/dashboard/projects/{project}/secrets?env={env}&tab=activity", w.trim_end_matches('/')));
                for (text, alert) in said {
                    let mut text = format!("roll secrets: {text}.");
                    if let Some(l) = &link {
                        text.push_str(&format!(" {l}"));
                    }
                    let body = json!({ "text": text, "content": text, "allowed_mentions": { "parse": [] }, "project": project, "environment": env, "alert": alert, "at": at, "link": link });
                    c.execute(
                        "INSERT INTO notices (id, project_id, env_id, body, attempts, next_at, sent_at, created_at) VALUES (?, ?, ?, ?, 0, ?, NULL, ?)",
                        params![names::new_id("ntc", 16), project, env_id, body.to_string(), now, now],
                    )?;
                    queued += 1;
                }
            }
            Ok::<_, crate::Error>(queued)
        })
    }
}
