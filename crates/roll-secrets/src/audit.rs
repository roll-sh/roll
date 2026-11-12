//! The audit log (docs/secrets.md, "Audit log"): every write and every read of values, with who,
//! where and which keys; never a value.

use crate::access::{Action, Caller, Scope};
use crate::{Result, Secrets, from_json, json};
use code_sql::{Conn, params};

use serde::{Deserialize, Serialize};

/// How long rows are kept.
pub const RETENTION_DAYS: i64 = 400;

#[derive(Debug, Serialize)]
pub struct AuditEntry {
    pub id: i64,
    pub at: i64,
    pub actor: String,
    pub action: String,
    pub environment: Option<String>,
    pub path: Option<String>,
    pub keys: Vec<String>,
    pub summary: String,
    pub ip: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct AuditFilter {
    pub env: Option<String>,
    pub key: Option<String>,
    pub actor: Option<String>,
    pub action: Option<String>,
    pub before: Option<i64>,
    pub limit: Option<i64>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record(c: &Conn<'_>, project: &str, caller: &Caller, action: &str, env: Option<&str>, path: Option<&str>, keys: &[String], summary: &str, now: i64) -> Result<()> {
    let keys: Vec<&String> = keys.iter().take(200).collect();
    c.execute(
        "INSERT INTO audit (project_id, at, actor, action, env, path, keys, summary, ip) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![project, now, caller.principal, action, env, path, json(&keys), summary, caller.ip],
    )?;
    Ok(())
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
        let limit = f.limit.unwrap_or(100).clamp(1, 1000);
        // Conditions only for the filters given: Postgres can't type `? IS NULL` parameters.
        let mut sql = String::from("SELECT id, at, actor, action, env, path, keys, summary, ip FROM audit WHERE project_id = ? AND id < ?");
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
        sql.push_str(" ORDER BY id DESC LIMIT ?");
        args.push(code_sql::Value::Int(limit));
        let rows = self.db.read(|c| {
            c.query(&sql, &args, |r| {
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
            })
        })?;
        // A token limited to some environments sees only their rows.
        Ok(rows.into_iter().filter(|r| r.environment.as_deref().is_none_or(|e| caller.allows(Action::Read, &Scope::env(project, e)))).collect())
    }
}
