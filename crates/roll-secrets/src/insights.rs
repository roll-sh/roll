//! Insights (docs/secrets.md, "Insights"): when each key of a folder was last changed and last
//! read, so unused and stale credentials stand out. Reads come from the audit log (every read of
//! values is in it), over the last 90 days; nothing is decrypted.

use crate::access::{Action, Caller, Scope};
use crate::envs;
use crate::{Result, Secrets, from_json, names};
use code_sql::params;
use serde::Serialize;
use std::collections::HashMap;

/// How far back reads are looked for, and the most audit rows read for it.
const WINDOW_DAYS: i64 = 90;
const ROWS_MAX: i64 = 50_000;

#[derive(Debug, Serialize)]
pub struct KeyInsight {
    pub key: String,
    pub version: i64,
    pub updated_at: i64,
    pub updated_by: String,
    /// The last read of its value (reveal, export, resolve, sync) in the last 90 days.
    pub last_read_at: Option<i64>,
    pub last_read_by: Option<String>,
    pub reads_30d: i64,
    /// Unchanged for longer than the environment's `max_age_days`.
    pub stale: bool,
}

#[derive(Debug, Serialize)]
pub struct Insights {
    pub path: String,
    pub max_age_days: Option<i64>,
    pub keys: Vec<KeyInsight>,
    /// Since when reads are known (rows older than the window, or than the audit's retention,
    /// aren't counted).
    pub reads_since: i64,
}

impl Secrets {
    pub fn insights(&self, project: &str, slug: &str, path: &str, caller: &Caller) -> Result<Insights> {
        let path = names::path(path)?;
        caller.need(Action::Read, &Scope::at(project, slug, &path))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        let since = now - WINDOW_DAYS * 86_400_000;
        let max_age_days = self.db.read(|c| envs::by_slug(c, project, slug))?.max_age_days;
        let entries = self.view(project, slug, &path, caller, false, Action::Read)?;
        // Newest first: the first row naming a key is its last read.
        let rows: Vec<(i64, String, String)> = self.db.read(|c| {
            c.query(
                "SELECT at, actor, keys FROM audit WHERE project_id = ? AND env = ? AND path = ? AND at >= ?
                 AND action IN ('secrets.reveal', 'secrets.export', 'secrets.resolve', 'sync.push') ORDER BY id DESC LIMIT ?",
                params![project, slug, path, since, ROWS_MAX],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
        })?;
        let mut last: HashMap<String, (i64, String)> = HashMap::new();
        let mut month: HashMap<String, i64> = HashMap::new();
        for (at, actor, keys) in rows {
            for k in from_json::<Vec<String>>(&keys) {
                if at >= now - 30 * 86_400_000 {
                    *month.entry(k.clone()).or_default() += 1;
                }
                last.entry(k).or_insert_with(|| (at, actor.clone()));
            }
        }
        let keys = entries
            .into_iter()
            .map(|e| {
                let read = last.get(&e.row.key);
                KeyInsight {
                    stale: max_age_days.is_some_and(|d| now - e.row.updated_at > d * 86_400_000),
                    last_read_at: read.map(|r| r.0),
                    last_read_by: read.map(|r| r.1.clone()),
                    reads_30d: month.get(&e.row.key).copied().unwrap_or(0),
                    key: e.row.key,
                    version: e.row.version,
                    updated_at: e.row.updated_at,
                    updated_by: e.row.updated_by,
                }
            })
            .collect();
        Ok(Insights { path, max_age_days, keys, reads_since: since })
    }
}
