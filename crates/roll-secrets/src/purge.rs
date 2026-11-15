//! Purge and retention (docs/secrets.md, "Purge and retention"): destroying old values.
//!
//! After a leak and a rotation, an old value shouldn't stay readable in history. Purging drops a
//! version's ciphertext (crypto-shredding: without it, nothing decrypts the value) and keeps the
//! row, marked purged, so history still says what changed and when, and a rollback that would need
//! it says it can't. A project's `history_days` purges what's older than that, never a secret's
//! current version.

use crate::access::{Action, Caller, Scope};
use crate::{Error, Result, Secrets, audit, envs, names};
use code_sql::{OptionalExtension, params};

impl Secrets {
    /// Purge the values of `key`'s versions older than `before_version` (default: every version
    /// but the current one). Returns how many were purged.
    pub fn purge(&self, project: &str, slug: &str, path: &str, key: &str, before_version: Option<i64>, caller: &Caller) -> Result<i64> {
        let path = names::path(path)?;
        caller.need(Action::Admin, &Scope::at(project, slug, &path))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let env = envs::by_slug(c, project, slug)?;
            // The key's history, whether or not it exists now (a deleted key's old values too).
            let found: Option<(String, i64)> = c
                .query_row(
                    "SELECT secret_id, MAX(version) FROM versions WHERE env_id = ? AND path = ? AND key = ? AND owner = '' GROUP BY secret_id ORDER BY MAX(seq) DESC LIMIT 1",
                    [&env.id, &path, key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let (id, latest) = found.ok_or_else(|| Error::NotFound(format!("no secret {key} in {slug}:{path}")))?;
            let current: Option<i64> = c.query_row("SELECT version FROM secrets WHERE id = ?", [&id], |r| r.get(0)).optional()?;
            // Never the value in use now.
            let below = before_version.unwrap_or(latest + 1).min(current.unwrap_or(latest + 1));
            let n = c.execute(
                "UPDATE versions SET value = NULL, purged_at = ? WHERE secret_id = ? AND version < ? AND value IS NOT NULL",
                params![now, id, below],
            )? as i64;
            audit::record(c, project, caller, "secrets.purge", Some(slug), Some(&path), &[key.to_string()], &format!("purged {n} old value{} of {key}", if n == 1 { "" } else { "s" }), now)?;
            Ok(n)
        })
    }

    /// Set (or clear) how long old values are kept.
    pub fn set_history_days(&self, project: &str, days: Option<i64>, caller: &Caller) -> Result<Option<i64>> {
        caller.need(Action::Admin, &Scope::project(project))?;
        if days.is_some_and(|d| !(1..=3650).contains(&d)) {
            return Err(Error::BadRequest("history_days is 1–3650".into()));
        }
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            c.execute("UPDATE projects SET history_days = ? WHERE project_id = ?", params![days, project])?;
            let summary = match days {
                Some(d) => format!("old values are purged after {d} days"),
                None => "old values are kept".into(),
            };
            audit::record(c, project, caller, "history.retention", None, None, &[], &summary, now)
        })?;
        Ok(days)
    }

    pub fn history_days(&self, project: &str) -> Result<Option<i64>> {
        Ok(self.db.read(|c| c.query_row("SELECT history_days FROM projects WHERE project_id = ?", [project], |r| r.get(0)).optional())?.flatten())
    }

    /// The sweeper: purge every project's values older than its `history_days`, never a
    /// current version. Returns how many were purged.
    pub(crate) fn sweep_retention(&self) -> Result<usize> {
        let now = self.db.now_ms()?;
        let projects: Vec<(String, i64)> = self.db.read(|c| c.query("SELECT project_id, history_days FROM projects WHERE history_days IS NOT NULL", (), |r| Ok((r.get(0)?, r.get(1)?))))?;
        let mut purged = 0;
        for (project, days) in projects {
            let cutoff = now - days * 86_400_000;
            purged += self.db.write(|c| {
                c.execute(
                    "UPDATE versions SET value = NULL, purged_at = ? WHERE project_id = ? AND at < ? AND value IS NOT NULL
                     AND NOT EXISTS (SELECT 1 FROM secrets s WHERE s.id = versions.secret_id AND s.version = versions.version)",
                    params![now, project, cutoff],
                )
            })?;
        }
        Ok(purged)
    }
}
