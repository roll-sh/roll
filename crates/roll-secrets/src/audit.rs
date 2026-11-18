//! The audit log (docs/secrets.md, "Audit log"): every write and every read of values, with who,
//! where and which keys; never a value. Each project's rows form a hash chain with signed digests
//! (roll-secrets-chain, docs/secrets.md "Tamper evidence").

use crate::access::{Action, Caller, Scope};
use crate::chain::{self, Digest, Verification, Walk};
use crate::{Error, Result, Secrets, from_json, json};
use code_sql::{Conn, OptionalExtension, params};

use serde::Deserialize;

/// How long rows are kept.
pub const RETENTION_DAYS: i64 = 400;

pub use crate::chain::AuditEntry;

pub(crate) const COLUMNS: &str = "id, at, actor, action, env, path, keys, summary, ip, approval, seq, hash, digest";

pub(crate) fn row(r: &code_sql::Row<'_>) -> code_sql::Result<AuditEntry> {
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
        approval: r.get::<_, Option<String>>(9)?.and_then(|a| serde_json::from_str(&a).ok()),
        seq: r.get(10)?,
        hash: r.get(11)?,
        digest: r.get(12)?,
    })
}

pub(crate) const SCHEMA: &str = "
CREATE UNIQUE INDEX IF NOT EXISTS audit_seq ON audit (project_id, seq);
CREATE INDEX IF NOT EXISTS audit_digests ON audit (project_id, at) WHERE action = 'audit.digest';
CREATE TABLE IF NOT EXISTS audit_heads (
    project_id TEXT PRIMARY KEY,
    seq INTEGER NOT NULL,
    hash TEXT NOT NULL,
    at INTEGER NOT NULL,
    digest_seq INTEGER NOT NULL,
    digest_at INTEGER NOT NULL,
    start_seq INTEGER NOT NULL
) WITHOUT ROWID;
";

#[derive(Debug, Default, Deserialize)]
pub struct AuditFilter {
    pub env: Option<String>,
    pub key: Option<String>,
    pub actor: Option<String>,
    pub action: Option<String>,
    pub before: Option<i64>,
    /// Rows at or after this time (unix ms).
    pub since: Option<i64>,
    pub limit: Option<i64>,
    /// `csv` or `jsonl`: an export (up to 10,000 rows) instead of JSON.
    pub format: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record(c: &Conn<'_>, project: &str, caller: &Caller, action: &str, env: Option<&str>, path: Option<&str>, keys: &[String], summary: &str, now: i64) -> Result<()> {
    let e = AuditEntry {
        at: now,
        actor: caller.principal.clone(),
        action: action.into(),
        environment: env.map(Into::into),
        path: path.map(Into::into),
        keys: keys.iter().take(200).cloned().collect(),
        summary: summary.into(),
        ip: caller.ip.clone(),
        approval: caller.approval.clone(),
        ..AuditEntry::default()
    };
    append(c, project, e).map(|_| ())
}

/// Add a row to the end of `project`'s chain, in the caller's transaction: its number and the hash
/// before it come from the project's head, which this updates first. The head's row lock (Postgres
/// re-runs a serializable write that loses it) or SQLite's one writer means two writers never take
/// one number, and `audit_seq` refuses a second row at one anyway. Returns the row's `seq`.
fn append(c: &Conn<'_>, project: &str, mut e: AuditEntry) -> Result<i64> {
    let take = |c: &Conn<'_>| -> Result<Option<(i64, String)>> {
        Ok(c.query("UPDATE audit_heads SET seq = seq + 1, at = ? WHERE project_id = ? RETURNING seq, hash", params![e.at, project], |r| Ok((r.get(0)?, r.get(1)?)))?.pop())
    };
    let (seq, prev) = match take(c)? {
        Some(h) => h,
        None => {
            // The project's first chained row: older rows stay unchained.
            c.execute("INSERT OR IGNORE INTO audit_heads (project_id, seq, hash, at, digest_seq, digest_at, start_seq) VALUES (?, 0, '', ?, 0, 0, 1)", params![project, e.at])?;
            take(c)?.ok_or_else(|| Error::Internal("the audit chain's head vanished".into()))?
        }
    };
    e.seq = Some(seq);
    let hash = chain::link(&prev, project, &e);
    // The row and the head's new hash in one round trip on Postgres.
    use code_sql::ValueRef::{Int, Null, Text};
    fn opt(v: &Option<String>) -> code_sql::ValueRef<'_> {
        v.as_deref().map_or(Null, Text)
    }
    let (keys, approval) = (json(&e.keys), e.approval.as_ref().map(json));
    let row = vec![Text(project), Int(seq), Text(&hash), Int(e.at), Text(&e.actor), Text(&e.action), opt(&e.environment), opt(&e.path), Text(&keys), Text(&e.summary), opt(&e.ip), opt(&approval), opt(&e.digest)];
    c.execute_pipeline(&[
        ("INSERT INTO audit (project_id, seq, hash, at, actor, action, env, path, keys, summary, ip, approval, digest) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)", vec![row]),
        ("UPDATE audit_heads SET hash = ? WHERE project_id = ?", vec![vec![Text(&hash), Text(project)]]),
    ])?;
    Ok(seq)
}

impl Secrets {
    /// Record a read in its own transaction. A read that can't be recorded still answers (the
    /// failure is logged): the database that would hold the row is the one that's failing.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn audit_now(&self, project: &str, caller: &Caller, action: &str, env: Option<&str>, path: Option<&str>, keys: &[String], summary: &str) {
        let res = self.db.now_ms().map_err(crate::Error::from).and_then(|now| self.db.write(|c| record(c, project, caller, action, env, path, keys, summary, now)));
        if let Err(e) = res {
            tracing::error!(project, action, "recording an audit row failed: {e}");
        }
    }

    pub fn audit(&self, project: &str, f: AuditFilter, caller: &Caller) -> Result<Vec<AuditEntry>> {
        caller.need(Action::Read, &Scope::project(project))?;
        if let Some(e) = &f.env {
            caller.need(Action::Read, &Scope::env(project, e))?;
        }
        self.project(project)?;
        let limit = f.limit.unwrap_or(if f.format.is_some() { 10_000 } else { 100 }).clamp(1, 10_000);
        // Conditions only for the filters given: Postgres can't type `? IS NULL` parameters.
        let mut sql = format!("SELECT {COLUMNS} FROM audit WHERE project_id = ? AND id < ?");
        let mut args: Vec<code_sql::Value> = vec![code_sql::Value::Text(project.into()), code_sql::Value::Int(f.before.unwrap_or(i64::MAX))];
        let mut cond = |clause: &str, v: Option<String>| {
            if let Some(v) = v {
                sql.push_str(clause);
                args.push(code_sql::Value::Text(v));
            }
        };
        cond(" AND env = ?", f.env.clone());
        cond(" AND actor = ?", f.actor.clone());
        cond(" AND action LIKE ?", f.action.as_ref().map(|a| format!("{a}%")));
        cond(" AND keys LIKE ?", f.key.as_ref().map(|k| format!("%\"{k}\"%")));
        if let Some(since) = f.since {
            sql.push_str(" AND at >= ?");
            args.push(code_sql::Value::Int(since));
        }
        sql.push_str(" ORDER BY id DESC LIMIT ?");
        args.push(code_sql::Value::Int(limit));
        let rows = self.db.read(|c| c.query(&sql, &args, row))?;
        // A token limited to some environments sees only their rows.
        Ok(rows
            .into_iter()
            .filter(|r| r.environment.as_deref().is_none_or(|e| caller.allows(Action::Read, &Scope::at(project, e, r.path.as_deref().unwrap_or("/")))))
            .collect())
    }
}

/// `GET …/audit/verify?since=`.
#[derive(Debug, Default, Deserialize)]
pub struct VerifyQuery {
    /// Unix ms: walk from the last digest before it instead of the log's start.
    pub since: Option<i64>,
}

/// Rows read at once while walking.
const PAGE: i64 = 5_000;

impl Secrets {
    /// Walk `project`'s chain and check its digests: where it breaks, if it does. From the log's
    /// start, or with `since`, from the last signed digest before then.
    pub fn verify_audit(&self, project: &str, q: VerifyQuery, caller: &Caller) -> Result<Verification> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        let keys = crate::issuer::public_keys(&self.db)?;
        let from: Option<i64> = match q.since {
            Some(t) => self.db.read(|c| c.query_row("SELECT MAX(seq) FROM audit WHERE project_id = ? AND action = ? AND at <= ?", params![project, chain::DIGEST, t], |r| r.get(0)))?,
            None => None,
        };
        // The head first: rows written while walking come after it, and a row and the head move in
        // one transaction.
        let head: Option<(i64, String)> = self.db.read(|c| c.query_row("SELECT seq, hash FROM audit_heads WHERE project_id = ?", [project], |r| Ok((r.get(0)?, r.get(1)?))).optional())?;
        let upto = head.as_ref().map_or(0, |h| h.0);
        let mut walk = Walk::new(project, from.is_none());
        let mut after = from.map_or(0, |s| s - 1);
        loop {
            let rows = self.db.read(|c| c.query(&format!("SELECT {COLUMNS} FROM audit WHERE project_id = ? AND seq > ? AND seq <= ? ORDER BY seq LIMIT ?"), params![project, after, upto, PAGE], row))?;
            let n = rows.len() as i64;
            for r in &rows {
                if !walk.push(r, &mut |kid| keys.get(kid).cloned()) {
                    return Ok(walk.finish());
                }
                after = r.seq.unwrap_or(after);
            }
            if n < PAGE {
                break;
            }
        }
        // Rows deleted from the end: the head still counts them.
        if let Some((seq, hash)) = head {
            let walked = walk.v.head.unwrap_or(0);
            if walked < seq {
                walk.broken(Some(walked + 1), "truncated", format!("rows #{}–#{seq} are missing from the end", walked + 1));
            } else if walked == seq && walk.last_hash() != Some(hash.as_str()) {
                walk.broken(Some(seq), "rewritten", format!("the log through #{seq} isn't the one roll wrote"));
            }
        }
        // Rows from before the chain began have no hash; one written after it began was added
        // outside roll (or by a node still running an older roll).
        let first: Option<i64> = self.db.read(|c| c.query_row("SELECT MIN(id) FROM audit WHERE project_id = ? AND seq IS NOT NULL", [project], |r| r.get(0)))?;
        let count = |above: i64| self.db.read(|c| c.query_row("SELECT COUNT(*) FROM audit WHERE project_id = ? AND seq IS NULL AND id > ?", params![project, above], |r| r.get::<_, i64>(0)));
        walk.v.unchained = count(0)?;
        let late = match first {
            Some(id) if walk.v.unchained > 0 => count(id)?,
            _ => 0,
        };
        if late > 0 {
            walk.broken(None, "unchained", format!("{} written after the chain began outside it", crate::write::plural(late as usize, "row")));
        }
        Ok(walk.finish())
    }

    /// The sweeper: sign a digest for each project whose chain moved in the last hour, and cut what
    /// retention drops. Rows go only at a digest older than `keep_ms`, which is then where the log
    /// starts; the digest written with the cut signs that, in the cut's transaction. Returns the
    /// digests written.
    #[doc(hidden)]
    pub fn sweep_audit_chain(&self, keep_ms: i64) -> Result<usize> {
        let now = self.db.now_ms()?;
        let cutoff = now - keep_ms;
        // Rows from before the chain began go by age, as every row did.
        self.db.write(|c| c.execute("DELETE FROM audit WHERE seq IS NULL AND at < ?", [cutoff]))?;
        let due: Vec<String> = self.db.read(|c| {
            c.query(
                "SELECT h.project_id FROM audit_heads h WHERE (h.seq > h.digest_seq AND h.digest_at <= ?)
                 OR EXISTS (SELECT 1 FROM audit a WHERE a.project_id = h.project_id AND a.action = ? AND a.at < ? AND a.seq > h.start_seq) LIMIT 500",
                params![now - chain::DIGEST_EVERY_MS, chain::DIGEST, cutoff],
                |r| r.get(0),
            )
        })?;
        if due.is_empty() {
            return Ok(0);
        }
        let Ok(keys) = self.keys() else { return Ok(0) };
        let (kid, key) = crate::issuer::signer(&self.db, keys)?;
        let mut written = 0;
        for project in due {
            // The write is the claim: a node that comes second finds nothing due.
            let wrote = self.db.write(|c| {
                type Head = (i64, String, i64, i64, i64, i64);
                let (seq, hash, last_at, digest_seq, digest_at, start): Head =
                    c.query_row("SELECT seq, hash, at, digest_seq, digest_at, start_seq FROM audit_heads WHERE project_id = ?", [&project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
                let cut: Option<i64> = c.query_row("SELECT MAX(seq) FROM audit WHERE project_id = ? AND action = ? AND at < ? AND seq > ?", params![project, chain::DIGEST, cutoff, start], |r| r.get(0))?;
                if cut.is_none() && (seq <= digest_seq || digest_at > now - chain::DIGEST_EVERY_MS) {
                    return Ok(false);
                }
                let start = match cut {
                    Some(at) => {
                        c.execute("DELETE FROM audit WHERE project_id = ? AND seq < ?", params![project, at])?;
                        at
                    }
                    None => start,
                };
                let d = Digest { project: project.clone(), seq, hash, last_at, at: now, start };
                let e = AuditEntry {
                    at: now,
                    actor: chain::DIGEST_ACTOR.into(),
                    action: chain::DIGEST.into(),
                    summary: format!("signed the log through #{seq}"),
                    digest: Some(chain::sign(&kid, &key, &d).map_err(|_| Error::Internal("signing a digest failed".into()))?),
                    ..AuditEntry::default()
                };
                let own = append(c, &project, e)?;
                c.execute("UPDATE audit_heads SET digest_seq = ?, digest_at = ?, start_seq = ? WHERE project_id = ?", params![own, now, start, project])?;
                Ok::<_, Error>(true)
            })?;
            written += usize::from(wrote);
        }
        Ok(written)
    }
}
