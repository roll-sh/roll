//! Audit export and streaming (docs/secrets.md, "Audit log"): a project's audit rows to a SIEM.
//!
//! A sink is an HTTPS URL that receives the project's new audit rows in order: batches of up to
//! 200, signed like syncs and every roll webhook (`X-Roll-Signature-256: sha256=HMAC(secret, "ts.body")`, `X-Roll-Delivery`), never a value
//! (audit rows hold none). Each sink keeps a cursor, the last row id delivered; the sweeper claims a
//! sink in a write before delivering, so no two nodes deliver one batch, and a failed delivery
//! retries the same batch later. roll-secrets has no HTTP client: rolld's [`Deliver`] posts.

use crate::access::{Action, Caller, Scope};
use crate::audit::AuditEntry;
use crate::{Error, Result, Secrets, Value, audit, from_json, names};
use code_sql::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::sync::Arc;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS audit_sinks (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    url TEXT NOT NULL,
    secret BLOB NOT NULL,
    cursor INTEGER NOT NULL,
    status TEXT NOT NULL,
    error TEXT,
    delivered_at INTEGER,
    running_until INTEGER NOT NULL,
    retry_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS audit_sinks_project ON audit_sinks (project_id);
";

/// Rows per delivery.
const BATCH: i64 = 200;
/// How long a claim holds, and how long a failed delivery waits.
const HOLD_MS: i64 = 2 * 60_000;
const RETRY_MS: i64 = 5 * 60_000;
/// How old an audit row is before a sink gets it (longer than any write transaction).
const SETTLE_MS: i64 = 30_000;
/// Sinks per project.
const SINKS_MAX: i64 = 10;

/// Posts signed JSON to a public URL (rolld: `PublicOnly`, no redirects, a bounded read).
pub trait Deliver: Send + Sync {
    /// Whether `url` may be a sink (https, public).
    fn check(&self, url: &str) -> Result<()>;
    /// POST `body` to `url`, signed with `secret`, `X-Roll-Sink: <id>`.
    fn post(&self, url: &str, id: &str, secret: &Value, body: &Json) -> Result<()>;
    /// GET `url` (https, public, no redirects), at most `limit` bytes: an OIDC issuer's discovery
    /// document and keys (identities.rs).
    fn get(&self, url: &str, limit: usize) -> Result<Vec<u8>> {
        let _ = (url, limit);
        Err(crate::Error::Link("fetching isn't on this server".into()))
    }
    /// POST `body` to `url`, signed with `secret`, `X-Roll-Rotator: <id>`, and the 2xx answer (at
    /// most `limit` bytes): a rotator's (rotators.rs). The answer may hold a value.
    fn call(&self, url: &str, id: &str, secret: &Value, body: &Json, limit: usize) -> Result<zeroize::Zeroizing<Vec<u8>>> {
        let _ = (url, id, secret, body, limit);
        Err(crate::Error::Link("calling out isn't on this server".into()))
    }
}

#[derive(Debug, Serialize)]
pub struct Sink {
    pub id: String,
    pub url: String,
    pub status: String,
    pub error: Option<String>,
    /// The last audit row delivered.
    pub cursor: i64,
    pub delivered_at: Option<i64>,
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewSink {
    pub url: String,
    /// Deliver the rows from now on (default) or every row the project still has.
    #[serde(default)]
    pub from_start: bool,
}

fn sink_aad(project: &str, id: &str) -> String {
    format!("roll-secrets:sink:{project}:{id}")
}

const COLUMNS: &str = "id, url, status, error, cursor, delivered_at, created_at, created_by";

fn sink_row(r: &code_sql::Row<'_>) -> code_sql::Result<Sink> {
    Ok(Sink { id: r.get(0)?, url: r.get(1)?, status: r.get(2)?, error: r.get(3)?, cursor: r.get(4)?, delivered_at: r.get(5)?, created_at: r.get(6)?, created_by: r.get(7)? })
}

/// A sink URL naming a roll bucket instead of an endpoint: `bucket:<name>`. Each batch becomes an
/// object `roll-secrets/audit/<sink>/<delivery>.json` there.
pub const BUCKET: &str = "bucket:";
/// A sink URL naming a topic of a roll stream: `stream:<stream id>/<topic>`. Each audit event
/// becomes one record there, keyed by its audit id.
pub const STREAM: &str = "stream:";

/// The roll resource (kind, ref) a sink URL names, if it names one.
pub fn roll_target(url: &str) -> Option<(&'static str, &str)> {
    if let Some(b) = url.strip_prefix(BUCKET) {
        return Some(("bucket", b));
    }
    url.strip_prefix(STREAM).map(|s| ("stream", s.split('/').next().unwrap_or_default()))
}

impl Secrets {
    fn deliver(&self) -> Result<&Arc<dyn Deliver>> {
        self.deliver.as_ref().ok_or_else(|| Error::BadRequest("audit streaming isn't on this server".into()))
    }

    pub fn sinks(&self, project: &str, caller: &Caller) -> Result<Vec<Sink>> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| Ok(c.query(&format!("SELECT {COLUMNS} FROM audit_sinks WHERE project_id = ? ORDER BY created_at"), [project], sink_row)?))
    }

    /// A new sink; the second value is its signing secret, shown once.
    pub fn create_sink(&self, project: &str, n: NewSink, caller: &Caller) -> Result<(Sink, String)> {
        caller.need(Action::Admin, &Scope::project(project))?;
        // A roll bucket of the project's own canvas (an archive with the bucket's retention and
        // Object Lock), or an HTTPS endpoint.
        match roll_target(&n.url) {
            Some((kind, name)) => {
                if let Some(rest) = n.url.strip_prefix(STREAM) {
                    let topic = rest.split_once('/').map(|(_, t)| t).unwrap_or_default();
                    if topic.is_empty() || topic.len() > 249 || !topic.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
                        return Err(Error::BadRequest("a stream sink is stream:<stream id>/<topic>".into()));
                    }
                }
                if !self.members(project)?.contains(&(kind.to_string(), name.to_string())) {
                    return Err(Error::BadRequest(self.not_linkable(project, kind, name)));
                }
                self.deliver()?;
            }
            None => self.deliver()?.check(&n.url)?,
        }
        let dek = self.project(project)?;
        let now = self.db.now_ms()?;
        let id = names::new_id("snk", 12);
        let secret = names::new_id("whsec", 32);
        let sealed = dek.seal(&sink_aad(project, &id), &Value::new(secret.clone()));
        let sink = self.db.write(|c| {
            let n_sinks: i64 = c.query_row("SELECT COUNT(*) FROM audit_sinks WHERE project_id = ?", [project], |r| r.get(0))?;
            if n_sinks >= SINKS_MAX {
                return Err(Error::Conflict("conflict", format!("a project has at most {SINKS_MAX} audit sinks")));
            }
            let cursor: i64 = if n.from_start { 0 } else { c.query_row("SELECT COALESCE(MAX(id), 0) FROM audit WHERE project_id = ?", [project], |r| r.get(0))? };
            c.execute(
                "INSERT INTO audit_sinks (id, project_id, url, secret, cursor, status, error, delivered_at, running_until, retry_at, created_at, created_by)
                 VALUES (?, ?, ?, ?, ?, 'new', NULL, NULL, 0, 0, ?, ?)",
                params![id, project, n.url.trim(), sealed, cursor, now, caller.principal],
            )?;
            audit::record(c, project, caller, "sink.create", None, None, &[], &format!("audit stream to {}", host(&n.url)), now)?;
            Ok(c.query_row(&format!("SELECT {COLUMNS} FROM audit_sinks WHERE id = ?"), [&id], sink_row)?)
        })?;
        Ok((sink, secret))
    }

    pub fn delete_sink(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let url: Option<String> = c.query_row("SELECT url FROM audit_sinks WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?;
            let url = url.ok_or_else(|| Error::NotFound(format!("no audit sink {id}")))?;
            c.execute("DELETE FROM audit_sinks WHERE id = ?", [id])?;
            audit::record(c, project, caller, "sink.delete", None, None, &[], &format!("stopped the audit stream to {}", host(&url)), now)
        })
    }

    /// One pass (the sweeper): each sink's next batch, delivered. Returns (delivered, failed).
    pub(crate) fn sweep_sinks(&self) -> Result<(usize, usize)> {
        let Some(deliver) = self.deliver.clone() else { return Ok((0, 0)) };
        let now = self.db.now_ms()?;
        // Only rows older than SETTLE_MS: ids are given out when a row is written, so on Postgres a
        // row can commit after a higher one is already visible, and a cursor past it would skip it.
        let settled = now - SETTLE_MS;
        let due: Vec<(String, String, String, Vec<u8>, i64)> = self.db.read(|c| {
            c.query(
                "SELECT s.id, s.project_id, s.url, s.secret, s.cursor FROM audit_sinks s WHERE s.running_until <= ? AND s.retry_at <= ?
                 AND EXISTS (SELECT 1 FROM audit a WHERE a.project_id = s.project_id AND a.id > s.cursor AND a.at <= ?) LIMIT 100",
                params![now, now, settled],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
        })?;
        let (mut ok, mut failed) = (0, 0);
        for (id, project, url, sealed, cursor) in due {
            let hold = now + HOLD_MS;
            if self.db.write(|c| c.execute("UPDATE audit_sinks SET running_until = ? WHERE id = ? AND running_until <= ? AND cursor = ?", params![hold, id, now, cursor]))? != 1 {
                continue;
            }
            let mut rows = self.db.read(|c| {
                c.query(
                    "SELECT id, at, actor, action, env, path, keys, summary, ip FROM audit WHERE project_id = ? AND id > ? ORDER BY id LIMIT ?",
                    params![project, cursor, BATCH],
                    |r| {
                        Ok(AuditEntry {
                            id: r.get(0)?,
                            at: r.get(1)?,
                            actor: r.get(2)?,
                            action: r.get(3)?,
                            environment: r.get(4)?,
                            path: r.get(5)?,
                            keys: from_json(&r.get::<_, String>(6)?),
                            summary: r.get(7)?,
                            ip: r.get(8)?,
                        })
                    },
                )
            })?;
            // Up to the first row that hasn't settled (by id, so nothing before it is skipped).
            if let Some(at) = rows.iter().position(|r| r.at > settled) {
                rows.truncate(at);
            }
            if rows.is_empty() {
                self.db.write(|c| c.execute("UPDATE audit_sinks SET running_until = 0 WHERE id = ? AND running_until = ?", params![id, hold]))?;
                continue;
            }
            let last = rows.last().map(|r| r.id).unwrap_or(cursor);
            // A bucket must still be on the project's canvas: one moved to another project, or
            // removed, gets nothing more.
            let gone = roll_target(&url).is_some_and(|(kind, name)| !self.members(&project).map(|m| m.contains(&(kind.to_string(), name.to_string()))).unwrap_or(false));
            let sent = if gone { Err(Error::BadRequest(format!("{url} isn't on this project's canvas any more"))) } else { Ok(()) };
            let sent = sent.and_then(|_| self.project(&project)).and_then(|dek| dek.open(&sink_aad(&project, &id), &sealed).map_err(Error::from)).and_then(|secret| {
                let body = json!({ "sink": id, "project": project, "events": rows });
                deliver.post(&url, &id, &secret, &body)
            });
            let at = self.db.now_ms()?;
            match sent {
                Ok(()) => {
                    ok += 1;
                    self.db.write(|c| c.execute("UPDATE audit_sinks SET cursor = ?, status = 'ok', error = NULL, delivered_at = ?, running_until = 0, retry_at = 0 WHERE id = ? AND running_until = ?", params![last, at, id, hold]))?;
                }
                Err(e) => {
                    failed += 1;
                    let why: String = e.to_string().chars().take(300).collect();
                    tracing::warn!(sink = %id, "an audit delivery failed: {why}");
                    self.db.write(|c| c.execute("UPDATE audit_sinks SET status = 'error', error = ?, running_until = 0, retry_at = ? WHERE id = ? AND running_until = ?", params![why, at + RETRY_MS, id, hold]))?;
                }
            }
        }
        Ok((ok, failed))
    }
}

/// A URL's host, for audit summaries (never its path or query: they may hold a token).
fn host(url: &str) -> String {
    url.split("://").nth(1).and_then(|r| r.split(['/', '?', '#']).next()).unwrap_or_default().rsplit('@').next().unwrap_or_default().to_string()
}

/// Rows as CSV: one header line, every field quoted.
pub(crate) fn csv(rows: &[AuditEntry]) -> String {
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let mut out = String::from("id,at,actor,action,environment,path,keys,summary,ip\n");
    for r in rows {
        let fields = [r.id.to_string(), r.at.to_string(), q(&r.actor), q(&r.action), q(r.environment.as_deref().unwrap_or("")), q(r.path.as_deref().unwrap_or("")), q(&r.keys.join(" ")), q(&r.summary), q(r.ip.as_deref().unwrap_or(""))];
        out.push_str(&fields.join(","));
        out.push('\n');
    }
    out
}
