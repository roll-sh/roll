//! The audit log (docs/secrets.md, "Audit log"): every write and every read of values, with who,
//! where and which keys; never a value. Each project's rows form a hash chain with signed digests
//! (roll-secrets-chain, docs/secrets.md "Tamper evidence").

use crate::access::{Action, Caller, Scope};
use crate::chain::{self, Digest, Gap, Verification, Walk};
use crate::crypto::Keyring;
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

/// What the head's MAC covers ([`Keyring::audit_mac`]): the project, the chain's last row and its
/// hash, and where the log starts. A database writer can recompute every hash, but without the
/// deployment key can't move the head back after cutting rows from the end, or forward over rows it
/// rewrote, or move where the log starts. Its own label, so a MAC of anything else never passes.
fn head_msg(project: &str, seq: i64, hash: &str, start: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + project.len() + hash.len());
    for part in ["roll-secrets:audit-head:v1", project, hash] {
        out.extend_from_slice(&(part.len() as u64).to_be_bytes());
        out.extend_from_slice(part.as_bytes());
    }
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&start.to_be_bytes());
    out
}

/// How old the newest digest may be before `verify` calls the log `stale`: the sweeper signs every
/// chain hourly, moved or not, so an older one means rows after it can't be vouched for (cut from
/// the end and the head put back from a copy) or the signer stopped.
pub const STALE_MS: i64 = 2 * chain::DIGEST_EVERY_MS;

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
    /// `false` leaves out roll's signed digests (`audit.digest`, hourly for every chain).
    pub digests: Option<bool>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record(c: &Conn<'_>, mac: &Keyring, project: &str, caller: &Caller, action: &str, env: Option<&str>, path: Option<&str>, keys: &[String], summary: &str, now: i64) -> Result<()> {
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
    append(c, mac, project, e, None).map(|_| ())
}

/// Add a row to the end of `project`'s chain, in the caller's transaction: its number and the hash
/// before it come from the project's head, which this updates first. The head's row lock (Postgres
/// re-runs a serializable write that loses it) or SQLite's one writer means two writers never take
/// one number, and `audit_seq` refuses a second row at one anyway. The head carries a MAC
/// ([`head_msg`]), checked before it moves: a head changed outside roll isn't MACed again, so it
/// stays visibly broken and the sweeper won't sign it. `start` moves where the log starts
/// (retention's cut). Returns the row's `seq`.
fn append(c: &Conn<'_>, mac: &Keyring, project: &str, mut e: AuditEntry, start: Option<i64>) -> Result<i64> {
    type Head = (i64, String, i64, Option<String>);
    let take = |c: &Conn<'_>| -> Result<Option<Head>> {
        Ok(c.query("UPDATE audit_heads SET seq = seq + 1, at = ? WHERE project_id = ? RETURNING seq, hash, start_seq, mac", params![e.at, project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.pop())
    };
    let (seq, prev, was_start, tag) = match take(c)? {
        Some(h) => h,
        None => {
            // The project's first chained row: older rows stay unchained.
            let tag = mac.audit_mac(&head_msg(project, 0, "", 1));
            c.execute("INSERT OR IGNORE INTO audit_heads (project_id, seq, hash, at, digest_seq, digest_at, start_seq, mac) VALUES (?, 0, '', ?, 0, 0, 1, ?)", params![project, e.at, tag])?;
            take(c)?.ok_or_else(|| Error::Internal("the audit chain's head vanished".into()))?
        }
    };
    let intact = tag.is_some_and(|t| mac.audit_mac_ok(&head_msg(project, seq - 1, &prev, was_start), &t));
    if !intact {
        tracing::warn!(project, "the audit chain's head isn't one roll wrote (changed in the database): it stays broken and unsigned");
    }
    let start = start.unwrap_or(was_start);
    e.seq = Some(seq);
    let hash = chain::link(&prev, project, &e);
    // The row and the head's new hash in one round trip on Postgres.
    use code_sql::ValueRef::{Int, Null, Text};
    fn opt(v: &Option<String>) -> code_sql::ValueRef<'_> {
        v.as_deref().map_or(Null, Text)
    }
    let (keys, approval) = (json(&e.keys), e.approval.as_ref().map(json));
    let tag = intact.then(|| mac.audit_mac(&head_msg(project, seq, &hash, start)));
    let row = vec![Text(project), Int(seq), Text(&hash), Int(e.at), Text(&e.actor), Text(&e.action), opt(&e.environment), opt(&e.path), Text(&keys), Text(&e.summary), opt(&e.ip), opt(&approval), opt(&e.digest)];
    c.execute_pipeline(&[
        ("INSERT INTO audit (project_id, seq, hash, at, actor, action, env, path, keys, summary, ip, approval, digest) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)", vec![row]),
        ("UPDATE audit_heads SET hash = ?, start_seq = ?, mac = ? WHERE project_id = ?", vec![vec![Text(&hash), Int(start), opt(&tag), Text(project)]]),
    ])?;
    Ok(seq)
}

impl Secrets {
    /// Record a read in its own transaction. A read that can't be recorded still answers (the
    /// failure is logged): the database that would hold the row is the one that's failing.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn audit_now(&self, project: &str, caller: &Caller, action: &str, env: Option<&str>, path: Option<&str>, keys: &[String], summary: &str) {
        let res = self.db.now_ms().map_err(crate::Error::from).and_then(|now| self.keys().and_then(|mac| self.db.write(|c| record(c, mac, project, caller, action, env, path, keys, summary, now))));
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
        cond(" AND action <> ?", (f.digests == Some(false)).then(|| chain::DIGEST.to_string()));
        if let Some(since) = f.since {
            sql.push_str(" AND at >= ?");
            args.push(code_sql::Value::Int(since));
        }
        sql.push_str(" ORDER BY id DESC LIMIT ?");
        args.push(code_sql::Value::Int(limit));
        let rows = self.db.read(|c| c.query(&sql, &args, row))?;
        // Which keys are decoys is the admins' to know.
        let decoys = self.sees_decoys(project, caller);
        // A token limited to some environments sees only their rows.
        Ok(rows
            .into_iter()
            .filter(|r| decoys || !crate::decoys::ADMIN_ACTIONS.contains(&r.action.as_str()))
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
        let mac = self.keys()?;
        // Only keys whose private half opens with the deployment key: one a database writer added
        // doesn't vouch for anything (issuer.rs).
        let keys = crate::issuer::public_keys(&self.db, mac, &self.issuer_keys)?;
        let from: Option<i64> = match q.since {
            Some(t) => self.db.read(|c| c.query_row("SELECT MAX(seq) FROM audit WHERE project_id = ? AND action = ? AND at <= ?", params![project, chain::DIGEST, t], |r| r.get(0)))?,
            None => None,
        };
        // The head first: rows written while walking come after it, and a row and the head move in
        // one transaction.
        type Head = (i64, String, i64, Option<String>);
        let head: Option<Head> = self.db.read(|c| c.query_row("SELECT seq, hash, start_seq, mac FROM audit_heads WHERE project_id = ?", [project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional())?;
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
        // Rows deleted from the end: the head still counts them, and a head put back over them, or
        // forward over rows rewritten after the last digest, doesn't carry roll's MAC.
        if let Some((seq, hash, start, tag)) = head {
            if !tag.is_some_and(|t| mac.audit_mac_ok(&head_msg(project, seq, &hash, start), &t)) {
                walk.broken(Some(seq), "head", "the chain's head isn't one roll wrote: rows were cut from the end or rewritten, and the head moved to match".into());
            }
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
        // The signer writes a digest hourly: an older newest one leaves the end unvouched for.
        let signed = match walk.v.signed_at {
            Some(at) => Some(at),
            None => self.db.read(|c| c.query_row("SELECT MIN(at) FROM audit WHERE project_id = ? AND seq IS NOT NULL", [project], |r| r.get::<_, Option<i64>>(0)))?,
        };
        let now = self.db.now_ms()?;
        if let Some(at) = signed.filter(|at| now - at > STALE_MS) {
            let hours = (now - at) / 3_600_000;
            walk.broken(None, "stale", format!("roll last signed the log {hours} hours ago (it signs hourly): what's after #{} isn't vouched for", walk.v.through.unwrap_or(0)));
        }
        Ok(walk.finish())
    }

    /// The sweeper: sign a digest of each project's chain hourly, moved or not (so `verify` can tell
    /// a stale one), and cut what retention drops. Rows go only at a digest older than `keep_ms`,
    /// which is then where the log starts; the digest written with the cut signs that, in the cut's
    /// transaction. Returns the digests written.
    #[doc(hidden)]
    pub fn sweep_audit_chain(&self, keep_ms: i64) -> Result<usize> {
        self.sweep_audit_chain_at(keep_ms, self.db.now_ms()?)
    }

    /// [`Secrets::sweep_audit_chain`] at `now` (tests move the clock).
    #[doc(hidden)]
    pub fn sweep_audit_chain_at(&self, keep_ms: i64, now: i64) -> Result<usize> {
        let cutoff = now - keep_ms;
        // Rows from before the chain began go by age, as every row did.
        self.db.write(|c| c.execute("DELETE FROM audit WHERE seq IS NULL AND at < ?", [cutoff]))?;
        let due: Vec<String> = self.db.read(|c| {
            c.query(
                "SELECT h.project_id FROM audit_heads h WHERE h.digest_at <= ?
                 OR EXISTS (SELECT 1 FROM audit a WHERE a.project_id = h.project_id AND a.action = ? AND a.at < ? AND a.seq > h.start_seq) LIMIT 500",
                params![now - chain::DIGEST_EVERY_MS, chain::DIGEST, cutoff],
                |r| r.get(0),
            )
        })?;
        if due.is_empty() {
            return Ok(0);
        }
        let Ok(mac) = self.keys() else { return Ok(0) };
        let (kid, key) = crate::issuer::signer(&self.db, mac)?;
        let mut written = 0;
        for project in due {
            // The write is the claim: a node that comes second finds nothing due.
            let wrote = self.db.write(|c| {
                type Head = (i64, String, i64, i64, i64, i64, Option<String>);
                let (seq, hash, last_at, digest_seq, digest_at, start, tag): Head =
                    c.query_row("SELECT seq, hash, at, digest_seq, digest_at, start_seq, mac FROM audit_heads WHERE project_id = ?", [&project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?;
                let cut: Option<i64> = c.query_row("SELECT MAX(seq) FROM audit WHERE project_id = ? AND action = ? AND at < ? AND seq > ?", params![project, chain::DIGEST, cutoff, start], |r| r.get(0))?;
                if cut.is_none() && digest_at > now - chain::DIGEST_EVERY_MS {
                    return Ok(false);
                }
                // Never sign a head changed outside roll: a sink would get roll's word for it. Not
                // asked again for an hour.
                if !tag.is_some_and(|t| mac.audit_mac_ok(&head_msg(&project, seq, &hash, start), &t)) {
                    tracing::warn!(project, "not signing the audit log: its head isn't one roll wrote");
                    c.execute("UPDATE audit_heads SET digest_at = ? WHERE project_id = ?", params![now, project])?;
                    return Ok(false);
                }
                // The newest digest the chain itself holds (signed, so a database writer can't make
                // one up), or with none the chain's first row. Older than `verify` allows, or not
                // the one the head says roll last wrote, and the rows after it can't be vouched
                // for: an old head put back from a copy passes its MAC. This digest says so, and
                // `verify` reports the gap for as long as it's in the log.
                let last: Option<(i64, i64)> = c.query("SELECT seq, at FROM audit WHERE project_id = ? AND action = ? AND seq <= ? ORDER BY seq DESC LIMIT 1", params![project, chain::DIGEST, seq], |r| Ok((r.get(0)?, r.get(1)?)))?.pop();
                let (after, since) = match last {
                    Some(d) => d,
                    None => (0, c.query_row("SELECT MIN(at) FROM audit WHERE project_id = ? AND seq IS NOT NULL", [&project], |r| r.get::<_, Option<i64>>(0))?.unwrap_or(0)),
                };
                let gap = (now - since > STALE_MS || after != digest_seq).then_some(Gap { after, since });
                if gap.is_some() {
                    tracing::warn!(project, after, "the audit log went unsigned past the stale limit, or its last digest is gone: the next digest records the gap");
                }
                let start = match cut {
                    Some(at) => {
                        c.execute("DELETE FROM audit WHERE project_id = ? AND seq < ?", params![project, at])?;
                        at
                    }
                    None => start,
                };
                let summary = match &gap {
                    Some(_) => format!("signed the log through #{seq} after a gap"),
                    None => format!("signed the log through #{seq}"),
                };
                let d = Digest { project: project.clone(), seq, hash, last_at, at: now, start, gap };
                let e = AuditEntry {
                    at: now,
                    actor: chain::DIGEST_ACTOR.into(),
                    action: chain::DIGEST.into(),
                    summary,
                    digest: Some(chain::sign(&kid, &key, &d).map_err(|_| Error::Internal("signing a digest failed".into()))?),
                    ..AuditEntry::default()
                };
                let own = append(c, mac, &project, e, Some(start))?;
                c.execute("UPDATE audit_heads SET digest_seq = ?, digest_at = ? WHERE project_id = ?", params![own, now, project])?;
                Ok::<_, Error>(true)
            })?;
            written += usize::from(wrote);
        }
        Ok(written)
    }
}
