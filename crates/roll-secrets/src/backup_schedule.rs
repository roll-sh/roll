//! Scheduled backups (docs/secrets.md, "Backup and restore"): a project's archive, sealed to a key
//! of its admins' as `POST …/backup` seals it, written to one of the project's own roll buckets
//! every so often, read back to check it's whole, and the oldest beyond `keep` deleted. roll never
//! holds the key that opens one: only the recipient's secret key does (`roll secrets restore`).

use crate::access::{Action, Caller, Everything, Scope};
use crate::{Error, Result, Secrets, audit};
use code_sql::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS backup_schedules (
    project_id TEXT PRIMARY KEY,
    bucket TEXT NOT NULL,
    recipient TEXT NOT NULL,
    every_hours INTEGER NOT NULL,
    keep INTEGER NOT NULL,
    next_at INTEGER NOT NULL,
    last_at INTEGER,
    last_key TEXT,
    last_error TEXT,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL
) WITHOUT ROWID;
";

/// The most backups a schedule keeps: under the 1000 keys one listing returns, so pruning always
/// sees one more than it keeps.
const MAX_KEEP: u32 = 500;

/// Where a project's backups go, under this prefix in its bucket.
fn prefix(project: &str) -> String {
    format!("roll-secrets/backups/{project}/")
}

#[derive(Debug, Serialize, PartialEq)]
pub struct BackupSchedule {
    pub bucket: String,
    /// The X25519 public key every backup is sealed to (its secret key opens them).
    pub recipient: String,
    pub every_hours: u32,
    pub keep: u32,
    pub next_at: i64,
    pub last_at: Option<i64>,
    /// The last backup's object key in the bucket.
    pub last_key: Option<String>,
    pub last_error: Option<String>,
    pub created_by: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewBackupSchedule {
    /// One of the project's roll buckets (on its canvas).
    pub bucket: String,
    pub recipient: String,
    #[serde(default = "every_day")]
    pub every_hours: u32,
    #[serde(default = "a_month")]
    pub keep: u32,
}

fn every_day() -> u32 {
    24
}

fn a_month() -> u32 {
    30
}

impl Secrets {
    pub fn backup_schedule(&self, project: &str, caller: &Caller) -> Result<Option<BackupSchedule>> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        Ok(self.db.read(|c| {
            c.query_row(
                "SELECT bucket, recipient, every_hours, keep, next_at, last_at, last_key, last_error, created_by FROM backup_schedules WHERE project_id = ?",
                [project],
                |r| {
                    Ok(BackupSchedule {
                        bucket: r.get(0)?,
                        recipient: r.get(1)?,
                        every_hours: r.get::<_, i64>(2)? as u32,
                        keep: r.get::<_, i64>(3)? as u32,
                        next_at: r.get(4)?,
                        last_at: r.get(5)?,
                        last_key: r.get(6)?,
                        last_error: r.get(7)?,
                        created_by: r.get(8)?,
                    })
                },
            )
            .optional()
        })?)
    }

    /// Back the project up on a schedule (replacing any): the first backup is due now.
    pub fn set_backup_schedule(&self, project: &str, n: NewBackupSchedule, caller: &Caller) -> Result<BackupSchedule> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        if self.deliver.is_none() {
            return Err(Error::BadRequest("scheduled backups aren't on this server".into()));
        }
        if !(1..=24 * 31).contains(&n.every_hours) || !(1..=MAX_KEEP).contains(&n.keep) {
            return Err(Error::BadRequest(format!("every_hours is 1 to 744, keep 1 to {MAX_KEEP}")));
        }
        if !self.members(project)?.contains(&("bucket".to_string(), n.bucket.clone())) {
            return Err(Error::BadRequest(self.not_linkable(project, "bucket", &n.bucket)));
        }
        // A key that can't be sealed to is refused now, not at 3 a.m.
        crate::crypto::seal_to(&n.recipient, "roll-secrets:backup-check", b"").map_err(|why| Error::BadRequest(format!("recipient: {why}")))?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            c.execute(
                "INSERT INTO backup_schedules (project_id, bucket, recipient, every_hours, keep, next_at, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT (project_id) DO UPDATE SET bucket = excluded.bucket, recipient = excluded.recipient, every_hours = excluded.every_hours, keep = excluded.keep, next_at = excluded.next_at, last_error = NULL, created_at = excluded.created_at, created_by = excluded.created_by",
                params![project, n.bucket, n.recipient, n.every_hours as i64, n.keep as i64, now, now, caller.principal],
            )?;
            audit::record(c, project, caller, "backup.schedule", None, None, &[], &format!("backs the project up to bucket {} every {} h, keeping {}", n.bucket, n.every_hours, n.keep), now)
        })?;
        self.backup_schedule(project, caller)?.ok_or_else(|| Error::Internal("the schedule vanished".into()))
    }

    pub fn delete_backup_schedule(&self, project: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            if c.execute("DELETE FROM backup_schedules WHERE project_id = ?", [project])? == 0 {
                return Err(Error::NotFound("no backup schedule".into()));
            }
            audit::record(c, project, caller, "backup.unschedule", None, None, &[], "stopped scheduled backups (the backups made stay in the bucket)", now)
        })
    }

    /// One pass: each due schedule, claimed first (one node runs it), backed up, checked, pruned.
    /// Returns (made, failed).
    pub(crate) fn sweep_backups(&self) -> Result<(usize, usize)> {
        let Some(deliver) = self.deliver.clone() else { return Ok((0, 0)) };
        let now = self.db.now_ms()?;
        let due: Vec<(String, i64)> = self.db.read(|c| c.query("SELECT project_id, next_at FROM backup_schedules WHERE next_at <= ? ORDER BY next_at LIMIT 10", [now], |r| Ok((r.get(0)?, r.get(1)?))))?;
        let (mut made, mut failed) = (0, 0);
        for (project, next_at) in due {
            // Claimed by moving next_at on: another node sees it not due.
            let claimed = self.db.write(|c| c.execute("UPDATE backup_schedules SET next_at = ? + every_hours * 3600000 WHERE project_id = ? AND next_at = ?", params![now, project, next_at]))?;
            if claimed == 0 {
                continue;
            }
            let outcome = self.scheduled_backup(&project, deliver.as_ref());
            let at = self.db.now_ms()?;
            let (key, error) = match &outcome {
                Ok(key) => (Some(key.clone()), None),
                Err(e) => (None, Some(e.to_string().chars().take(300).collect::<String>())),
            };
            self.db.write(|c| {
                match &key {
                    Some(k) => c.execute("UPDATE backup_schedules SET last_at = ?, last_key = ?, last_error = NULL WHERE project_id = ?", params![at, k, project])?,
                    None => c.execute("UPDATE backup_schedules SET last_error = ? WHERE project_id = ?", params![error, project])?,
                };
                Ok::<_, Error>(())
            })?;
            match outcome {
                Ok(_) => made += 1,
                Err(e) => {
                    failed += 1;
                    tracing::warn!(%project, "a scheduled backup failed: {e}");
                }
            }
        }
        Ok((made, failed))
    }

    /// Make one backup: sealed, written, read back whole, the oldest beyond `keep` deleted.
    fn scheduled_backup(&self, project: &str, deliver: &dyn crate::Deliver) -> Result<String> {
        let (bucket, recipient, keep, by): (String, String, i64, String) =
            self.db.read(|c| c.query_row("SELECT bucket, recipient, keep, created_by FROM backup_schedules WHERE project_id = ?", [project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))))?;
        // As syncs: what a person set up runs while they can still reach the project.
        if !self.maker_reaches(&by, project)? {
            return Err(Error::Forbidden(format!("{by} set this schedule up and can't reach the project any more: set it up again")));
        }
        // The bucket may have left the project since.
        if !self.members(project)?.contains(&("bucket".to_string(), bucket.clone())) {
            return Err(Error::BadRequest(self.not_linkable(project, "bucket", &bucket)));
        }
        let caller = Caller { principal: format!("backup:{project}"), grants: Arc::new(Everything), ip: None, runtime: false, approval: None, maker: None };
        // As `POST …/backup` answers it, so `roll secrets restore` takes either.
        let sealed = serde_json::json!({ "sealed": self.backup(project, &recipient, &caller)?, "context": crate::backup::context(project) });
        let ms = self.db.now_ms()?;
        let t = time::OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000)).map_err(|e| Error::Internal(e.to_string()))?;
        // Sortable as text: oldest first, as pruning reads them.
        let at = format!("{:04}{:02}{:02}T{:02}{:02}{:02}.{:03}Z", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute(), t.second(), ms.rem_euclid(1000));
        let key = format!("{}{at}.json", prefix(project));
        deliver.put_object(&bucket, &key, &sealed)?;
        // Read back (no more than was written): a backup that didn't land whole isn't counted as made.
        if deliver.get_object(&bucket, &key, sealed.to_string().len())? != sealed {
            return Err(Error::Link(format!("{key} in {bucket} doesn't read back as written")));
        }
        // Only roll's own backups (`<time>.json` straight under the prefix) are ever pruned. The
        // listing is oldest first and up to 1000 long, more than `keep` may be: the oldest go each time.
        let mut kept: Vec<String> = deliver.list_objects(&bucket, &prefix(project))?.into_iter().filter(|k| k.strip_prefix(&prefix(project)).is_some_and(|name| !name.contains('/') && name.ends_with(".json"))).collect();
        kept.sort();
        let extra = kept.len().saturating_sub(keep.max(1) as usize);
        for old in kept.into_iter().take(extra) {
            deliver.delete_object(&bucket, &old)?;
        }
        Ok(key)
    }
}
