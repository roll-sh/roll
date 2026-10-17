//! Durable history in object storage: WAL safekeeping, base images, point-in-time restore,
//! eviction of idle branches and retention.

use crate::compute::{container_name, has_cluster};
use crate::control::{Control, ControlError, Result};
use crate::ids::{new_id, now};
use crate::store::{BaseImage, Branch, Endpoint, LocalState, Project};
use bytes::Bytes;
use pg_storage::lsn::{Lsn, parse_segment_file};
use pg_storage::objects::{base_key, branch_prefix, wal_key};
use pg_storage::receiver::{self, Event, ReceiverArgs};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// What state to restore.
#[derive(Clone, Copy, Debug)]
pub enum Target {
    /// Everything durable.
    Latest,
    /// The state as of a wall-clock time (unix seconds).
    Time(i64),
    /// The state at a WAL position.
    Lsn { timeline: Option<u32>, lsn: Lsn },
}

#[derive(Serialize)]
pub struct History {
    /// Earliest restorable time (unix seconds), if any history exists yet.
    pub start: Option<i64>,
    /// Latest restorable time.
    pub end: Option<i64>,
    pub timeline: Option<u32>,
    pub flush_lsn: Option<String>,
    pub retention_s: i64,
    pub bases: Vec<BaseImage>,
}

impl Control {
    // ── safekeeper ──

    /// Stream this primary's WAL into object storage for as long as it runs.
    pub(crate) fn start_receiver(self: &Arc<Self>, ep: &Endpoint, port: u16, password: &str) {
        let mut receivers = self.receivers.lock().unwrap();
        if receivers.get(&ep.branch_id).is_some_and(|h| !h.is_finished()) {
            return;
        }
        let branch = ep.branch_id.clone();
        let args = ReceiverArgs {
            branch: branch.clone(),
            addr: ([127, 0, 0, 1], port).into(),
            user: "roll".into(),
            password: password.into(),
            spool: self.config.spool_dir(&branch),
            objects: self.objects.clone(),
            resume: self.store.wal_position(&branch).ok().flatten().map(|p| (p.timeline, Lsn(p.flush_lsn))),
        };
        let this = self.clone();
        let handle = tokio::spawn(async move {
            let cb_this = this.clone();
            let cb_branch = branch.clone();
            let on_event = move |ev: Event| match ev {
                Event::Streaming { timeline, fresh, .. } => {
                    let has_base = cb_this
                        .store
                        .bases(&cb_branch)
                        .map(|bs| bs.iter().any(|b| b.timeline == timeline))
                        .unwrap_or(false);
                    if fresh || !has_base {
                        cb_this.clone().spawn_base_backup(cb_branch.clone());
                    }
                }
                Event::Flushed { timeline, lsn } => {
                    if let Err(e) = cb_this.store.record_flush(&cb_branch, timeline, lsn.0) {
                        warn!(branch = %cb_branch, error = %e, "recording WAL position failed");
                    }
                }
            };
            if let Err(e) = receiver::run(args, on_event).await {
                warn!(branch = %branch, error = %e, "safekeeper stopped with error");
            }
        });
        receivers.insert(ep.branch_id.clone(), handle);
    }

    /// Wait for a branch's safekeeper to drain (after its compute stopped).
    pub(crate) async fn stop_receiver(&self, branch_id: &str) {
        let handle = self.receivers.lock().unwrap().remove(branch_id);
        if let Some(mut h) = handle
            && tokio::time::timeout(Duration::from_secs(120), &mut h).await.is_err()
        {
            warn!(branch = branch_id, "safekeeper didn't drain in time; aborting");
            h.abort();
        }
    }

    // ── base images ──

    fn spawn_base_backup(self: Arc<Self>, branch_id: String) {
        tokio::spawn(async move {
            if let Err(e) = self.take_base_backup(&branch_id).await {
                warn!(branch = %branch_id, error = %e, "base backup failed");
            }
        });
    }

    /// Online base image of a branch's running primary.
    pub async fn take_base_backup(&self, branch_id: &str) -> Result<BaseImage> {
        if !self.backups.lock().unwrap().insert(branch_id.to_string()) {
            return Err(ControlError::Conflict("a base backup is already running".into()));
        }
        let result = async {
            let ep = self
                .store
                .endpoint_for_branch(branch_id)?
                .ok_or_else(|| ControlError::NotFound(format!("primary of {branch_id}")))?;
            let project = self.project_or_404(&ep.project_id)?;
            let name = format!("{}-online", ids_ms());
            let key = base_key(branch_id, &format!("{name}.tar.zst"));
            let started = std::time::Instant::now();
            let info = self.driver.basebackup_upload(&ep.id, &project.password, &self.objects, &key).await?;
            let base = BaseImage {
                id: new_id("base"),
                branch_id: branch_id.into(),
                kind: "online".into(),
                key,
                timeline: info.timeline,
                start_lsn: info.start.0,
                end_lsn: info.end.0,
                flush_lsn: info.end.0,
                created_at: now(),
                bytes: info.bytes as i64,
            };
            self.save_base(&base, &name).await?;
            info!(branch = branch_id, start = %info.start, end = %info.end, bytes = info.bytes,
                  ms = started.elapsed().as_millis() as u64, "base backup stored");
            Ok(base)
        }
        .await;
        self.backups.lock().unwrap().remove(branch_id);
        result
    }

    async fn save_base(&self, base: &BaseImage, name: &str) -> Result<()> {
        self.store.insert_base(base)?;
        let meta = serde_json::to_vec_pretty(base).map_err(anyhow::Error::from)?;
        self.objects.put(&base_key(&base.branch_id, &format!("{name}.json")), Bytes::from(meta)).await?;
        Ok(())
    }

    /// Cold image of a branch whose primary is stopped.
    async fn snapshot_branch(&self, branch: &Branch, project: &Project) -> Result<BaseImage> {
        let name = format!("{}-snapshot", ids_ms());
        let key = base_key(&branch.id, &format!("{name}.tar.zst"));
        let info = self
            .driver
            .snapshot_upload(
                &self.branch_path(branch),
                project.pg_version,
                &self.config.image(project.pg_version),
                &self.objects,
                &key,
            )
            .await?;
        let flush = self.store.wal_position(&branch.id)?.map(|p| p.flush_lsn).unwrap_or(info.checkpoint.0);
        let base = BaseImage {
            id: new_id("base"),
            branch_id: branch.id.clone(),
            kind: "snapshot".into(),
            key,
            timeline: info.timeline,
            start_lsn: info.redo.0,
            end_lsn: info.checkpoint.0,
            flush_lsn: flush.max(info.checkpoint.0),
            created_at: now(),
            bytes: info.bytes as i64,
        };
        self.save_base(&base, &name).await?;
        Ok(base)
    }

    // ── restore ──

    fn restore_command(&self, branch_id: &str) -> String {
        let url = format!("{}/internal/wal/{branch_id}/%f?token={}", self.config.internal_url, self.config.internal_token);
        format!(r#"restore_command = 'wget -q -O "%p.part" "{url}" && mv "%p.part" "%p"'"#)
    }

    /// Resolve a target to a WAL position on a branch's history.
    fn resolve(&self, branch_id: &str, target: Target) -> Result<Option<(u32, Lsn)>> {
        match target {
            Target::Latest => Ok(None),
            Target::Lsn { timeline, lsn } => {
                let tli = match timeline {
                    Some(t) => t,
                    None => self
                        .store
                        .wal_position(branch_id)?
                        .map(|p| p.timeline)
                        .ok_or_else(|| ControlError::Invalid("branch has no history yet".into()))?,
                };
                Ok(Some((tli, lsn)))
            }
            Target::Time(at) => {
                if at > now() {
                    return Err(ControlError::Invalid("that time is in the future".into()));
                }
                let (tli, lsn, _) = self
                    .store
                    .lsn_at(branch_id, at)?
                    .ok_or_else(|| ControlError::Invalid("no history at that time (before the restore window)".into()))?;
                Ok(Some((tli, Lsn(lsn))))
            }
        }
    }

    /// `(parent timeline, switch point)` for every ancestor of `timeline`, from its history file.
    async fn timeline_switches(&self, branch_id: &str, timeline: u32) -> Result<Vec<(u32, Lsn)>> {
        if timeline <= 1 {
            return Ok(Vec::new());
        }
        let Some(data) = self.objects.get(&wal_key(branch_id, &pg_storage::lsn::history_file(timeline))).await? else {
            return Ok(Vec::new());
        };
        Ok(String::from_utf8_lossy(&data)
            .lines()
            .filter_map(|l| {
                let mut parts = l.split_whitespace();
                let tli = parts.next()?.parse().ok()?;
                let lsn = parts.next()?.parse().ok()?;
                Some((tli, lsn))
            })
            .collect())
    }

    /// Fill `dest` with `source`'s data as of `target`, from object storage.
    /// Returns whether WAL recovery runs on the next start. With `standby`, the result is a hot
    /// standby following the source's primary (a read replica).
    pub(crate) async fn restore_into(
        &self,
        dest: &Path,
        project: &Project,
        source: &str,
        target: Target,
        standby: Option<&str>,
    ) -> Result<bool> {
        let point = self.resolve(source, target)?;
        let bases = self.store.bases(source)?;
        let position = self.store.wal_position(source)?;
        // Restore along the timeline being targeted (for "latest": the current one).
        let timeline = point.map(|(t, _)| t).or(position.map(|p| p.timeline));
        let switches = match timeline {
            Some(t) => self.timeline_switches(source, t).await?,
            None => Vec::new(),
        };
        let base = choose_base(&bases, timeline, point.map(|(_, l)| l), &switches)
            .ok_or_else(|| ControlError::Invalid("no base image covers that point (outside the restore window)".into()))?;

        let up_to_date = match point {
            None => position.is_none_or(|p| p.flush_lsn <= base.flush_lsn),
            Some((_, lsn)) => lsn.0 == base.flush_lsn,
        };
        let recover = standby.is_some() || base.kind != "snapshot" || !up_to_date;

        let mut conf = Vec::new();
        let signal = if let Some(conninfo) = standby {
            conf.push(self.restore_command(source));
            conf.push(format!("primary_conninfo = '{conninfo}'"));
            conf.push("recovery_target_timeline = 'latest'".into());
            Some("standby.signal")
        } else if recover {
            conf.push(self.restore_command(source));
            conf.push("recovery_target_action = 'promote'".into());
            // No connections until recovery is done: readiness means "promoted".
            conf.push("hot_standby = off".into());
            match point {
                Some((tli, lsn)) => {
                    conf.push(format!("recovery_target_timeline = '{tli}'"));
                    conf.push(format!("recovery_target_lsn = '{lsn}'"));
                }
                None => conf.push("recovery_target_timeline = 'latest'".into()),
            }
            Some("recovery.signal")
        } else {
            None
        };
        let conf = conf.join("\n");
        let started = std::time::Instant::now();
        let bytes = self
            .driver
            .restore_download(
                dest,
                project.pg_version,
                &self.config.image(project.pg_version),
                &self.objects,
                &base.key,
                (!conf.is_empty()).then_some(conf.as_str()),
                signal,
            )
            .await?;
        info!(source, base = %base.id, kind = %base.kind, ?point, recover, bytes,
              ms = started.elapsed().as_millis() as u64, "restored base image");
        Ok(recover && standby.is_none())
    }

    /// Make sure a branch's data is on local disk. Returns the data directory and whether
    /// recovery runs on the next start.
    pub(crate) async fn hydrate(&self, branch: &Branch, project: &Project) -> Result<(PathBuf, bool)> {
        let dir = self.branch_path(branch);
        if has_cluster(&dir, project.pg_version) {
            return Ok((dir, branch.pending_recovery));
        }
        if self.store.bases(&branch.id)?.is_empty() {
            if self.store.wal_position(&branch.id)?.is_none() {
                return Ok((dir, false)); // brand new: initdb on start
            }
            return Err(ControlError::Internal(anyhow::anyhow!(
                "branch {} has no local data and no base image in object storage",
                branch.id
            )));
        }
        info!(branch = %branch.id, "no local data; restoring from object storage");
        let (fresh, recover) = self.restore_fresh(branch, project, Target::Latest).await?;
        let _ = std::fs::remove_dir_all(&dir);
        Ok((fresh, recover))
    }

    /// Restore a branch's history into a new directory and swap it in. The old directory is
    /// left for the caller to remove.
    async fn restore_fresh(&self, branch: &Branch, project: &Project, target: Target) -> Result<(PathBuf, bool)> {
        let name = new_id(&branch.id);
        let dir = self.config.data_path(&name);
        let recover = match self.restore_into(&dir, project, &branch.id, target, None).await {
            Ok(r) => r,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
        self.store.set_branch_data_dir(&branch.id, &name)?;
        self.store.set_local_state(&branch.id, LocalState::Local)?;
        self.store.set_pending_recovery(&branch.id, recover)?;
        Ok((dir, recover))
    }

    /// After a restore finishes recovery, drop the restore-only settings.
    pub(crate) async fn finish_recovery(&self, port: u16, password: &str) -> Result<()> {
        let conn_str = format!("host=127.0.0.1 port={port} user=roll password={password} dbname=main connect_timeout=10");
        let (client, connection) =
            tokio_postgres::connect(&conn_str, tokio_postgres::NoTls).await.map_err(|e| ControlError::Internal(e.into()))?;
        let handle = tokio::spawn(connection);
        for setting in [
            "restore_command",
            "recovery_target_lsn",
            "recovery_target_timeline",
            "recovery_target_action",
            "hot_standby",
            "primary_conninfo",
        ] {
            client
                .simple_query(&format!("ALTER SYSTEM RESET {setting}"))
                .await
                .map_err(|e| ControlError::Internal(e.into()))?;
        }
        client.simple_query("SELECT pg_reload_conf()").await.map_err(|e| ControlError::Internal(e.into()))?;
        drop(client);
        let _ = handle.await;
        Ok(())
    }

    /// Restore a branch in place to an earlier point. The current state is kept as a new branch
    /// (`<name>_old_<unix time>`), and the endpoint — and connection string — stay the same.
    pub async fn restore_branch(self: &Arc<Self>, project_id: &str, branch_id: &str, target: Target) -> Result<Branch> {
        let project = self.project_or_404(project_id)?;
        let branch = self.branch_in(project_id, branch_id)?;
        let ep = self
            .store
            .endpoint_for_branch(&branch.id)?
            .ok_or_else(|| ControlError::NotFound("primary endpoint".into()))?;
        // Resolve before touching anything so a bad target changes nothing.
        let point = self.resolve(&branch.id, target)?;
        let target = match point {
            Some((tli, lsn)) => Target::Lsn { timeline: Some(tli), lsn },
            None => Target::Latest,
        };

        self.suspend(&ep.id).await?;
        let backup_name = format!("{}_old_{}", branch.name, now());
        let backup = self.create_branch(project_id, &backup_name, Some(&branch.id), None).await?;

        let lock = self.lock(&ep.id);
        let held = lock.lock().await;
        let old = self.branch_path(&branch);
        // On failure nothing changed: the branch still points at its old data.
        self.restore_fresh(&branch, &project, target).await?;
        let _ = std::fs::remove_dir_all(&old);
        let _ = std::fs::remove_dir_all(self.config.spool_dir(&branch.id));
        drop(held);
        // Finish recovery now rather than on the first connection.
        self.ensure_running(&ep.id).await?;
        Ok(backup)
    }

    // ── eviction & retention ──

    /// Drop a suspended branch's local data; it stays restorable from object storage.
    pub async fn evict_branch(&self, branch_id: &str) -> Result<()> {
        let branch = self.store.branch(branch_id)?.ok_or_else(|| ControlError::NotFound(branch_id.into()))?;
        let project = self.project_or_404(&branch.project_id)?;
        let ep = self
            .store
            .endpoint_for_branch(branch_id)?
            .ok_or_else(|| ControlError::NotFound("primary endpoint".into()))?;
        let lock = self.lock(&ep.id);
        let _held = lock.lock().await;
        let ep = self.endpoint_or_404(&ep.id)?;
        if ep.state != crate::store::EndpointState::Suspended {
            return Err(ControlError::Conflict("suspend the compute before evicting".into()));
        }
        if branch.local_state == LocalState::Evicted {
            return Ok(());
        }
        self.stop_receiver(branch_id).await;
        let dir = self.branch_path(&branch);
        if has_cluster(&dir, project.pg_version) {
            if branch.pending_recovery {
                return Err(ControlError::Conflict("branch has an unfinished restore; start it first".into()));
            }
            let base = self.snapshot_branch(&branch, &project).await?;
            info!(branch = branch_id, bytes = base.bytes, "evicted to object storage");
        } else if self.store.bases(branch_id)?.is_empty() {
            return Err(ControlError::Conflict("branch has no data to evict".into()));
        }
        let _ = self.driver.remove(&ep.id).await;
        std::fs::remove_dir_all(&dir).or_else(ignore_missing).map_err(anyhow::Error::from)?;
        std::fs::remove_dir_all(self.config.spool_dir(branch_id)).or_else(ignore_missing).map_err(anyhow::Error::from)?;
        self.store.set_local_state(branch_id, LocalState::Evicted)?;
        Ok(())
    }

    /// Delete base images and WAL older than the restore window needs.
    pub async fn gc_branch(&self, branch: &Branch, retention_s: i64) -> Result<usize> {
        let cutoff = now() - retention_s;
        let bases = self.store.bases(&branch.id)?; // newest first
        let keep = match bases.iter().position(|b| b.created_at < cutoff) {
            Some(i) => i + 1, // everything inside the window, plus the newest one before it
            None => bases.len(),
        };
        let mut deleted = 0;
        for b in &bases[keep..] {
            self.objects.delete(&b.key).await?;
            self.objects.delete(&b.key.replace(".tar.zst", ".json")).await?;
            self.store.delete_base(&b.id)?;
            deleted += 1;
        }
        if let Some(oldest) = bases[..keep].last() {
            let min_seg = Lsn(oldest.start_lsn).segno();
            for (key, _) in self.objects.list(&wal_key(&branch.id, "")).await? {
                let file = key.rsplit('/').next().unwrap_or_default();
                let stem = file.split('.').next().unwrap_or_default();
                if let Some((_, seg)) = parse_segment_file(stem)
                    && seg < min_seg
                {
                    self.objects.delete(&key).await?;
                    deleted += 1;
                }
            }
            self.store.trim_lsn_index(&branch.id, cutoff.max(oldest.created_at))?;
        }
        Ok(deleted)
    }

    pub async fn object_bytes(&self, branch_id: &str) -> Result<u64> {
        Ok(self.objects.list(&branch_prefix(branch_id)).await?.iter().map(|(_, s)| s).sum())
    }

    pub async fn history(&self, branch_id: &str) -> Result<History> {
        let branch = self.store.branch(branch_id)?.ok_or_else(|| ControlError::NotFound(branch_id.into()))?;
        let project = self.project_or_404(&branch.project_id)?;
        let bases = self.store.bases(branch_id)?;
        let position = self.store.wal_position(branch_id)?;
        let running = self
            .store
            .endpoint_for_branch(branch_id)?
            .is_some_and(|e| e.state == crate::store::EndpointState::Running);
        let start = bases.last().map(|b| b.created_at.max(now() - project.history_retention_s));
        let end = position.map(|p| if running { now() } else { p.updated_at });
        Ok(History {
            start,
            end: end.or(start),
            timeline: position.map(|p| p.timeline),
            flush_lsn: position.map(|p| Lsn(p.flush_lsn).to_string()),
            retention_s: project.history_retention_s,
            bases,
        })
    }

    /// WAL for `restore_command` on restoring computes and replicas.
    pub async fn wal_file(&self, branch_id: &str, file: &str) -> Result<Option<Bytes>> {
        Ok(receiver::fetch_wal(&self.objects, &self.config.spool_dir(branch_id), branch_id, file).await?)
    }

    /// `primary_conninfo` for replicas of a branch.
    pub(crate) fn primary_conninfo(&self, primary: &Endpoint, project: &Project, replica_id: &str) -> String {
        format!(
            "host={} port=5432 user=roll password={} application_name={replica_id}",
            container_name(&primary.id),
            project.password
        )
    }
}

/// Newest base image (of `bases`, newest first) usable to reach `lsn` on `timeline`. A base on
/// an ancestor timeline only qualifies if it ends before that timeline forked off — later bases
/// there describe an abandoned future.
pub(crate) fn choose_base<'a>(
    bases: &'a [BaseImage],
    timeline: Option<u32>,
    lsn: Option<Lsn>,
    switches: &[(u32, Lsn)],
) -> Option<&'a BaseImage> {
    bases.iter().find(|b| {
        let on_path = match timeline {
            None => true,
            Some(t) => {
                b.timeline == t
                    || (b.timeline < t && switches.iter().any(|(st, at)| *st == b.timeline && b.end_lsn <= at.0))
            }
        };
        on_path && lsn.is_none_or(|l| b.end_lsn <= l.0)
    })
}

fn ignore_missing(e: std::io::Error) -> std::io::Result<()> {
    if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) }
}

fn ids_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(id: &str, timeline: u32, end: u64) -> BaseImage {
        BaseImage {
            id: id.into(),
            branch_id: "br".into(),
            kind: "online".into(),
            key: String::new(),
            timeline,
            start_lsn: end.saturating_sub(10),
            end_lsn: end,
            flush_lsn: end,
            created_at: end as i64,
            bytes: 0,
        }
    }

    #[test]
    fn latest_takes_newest_on_current_timeline() {
        let bases = [base("b3", 1, 300), base("b2", 1, 200), base("b1", 1, 100)];
        assert_eq!(choose_base(&bases, Some(1), None, &[]).unwrap().id, "b3");
        assert_eq!(choose_base(&bases, None, None, &[]).unwrap().id, "b3");
    }

    #[test]
    fn point_needs_base_that_ends_before_it() {
        let bases = [base("b3", 1, 300), base("b2", 1, 200), base("b1", 1, 100)];
        assert_eq!(choose_base(&bases, Some(1), Some(Lsn(250)), &[]).unwrap().id, "b2");
        assert_eq!(choose_base(&bases, Some(1), Some(Lsn(200)), &[]).unwrap().id, "b2");
        assert!(choose_base(&bases, Some(1), Some(Lsn(50)), &[]).is_none());
    }

    #[test]
    fn skips_bases_from_an_abandoned_future() {
        // Timeline 2 forked from timeline 1 at 150: b3 and b2 (tl 1, after 150) are not ancestors.
        let bases = [base("b4", 2, 400), base("b3", 1, 300), base("b2", 1, 200), base("b1", 1, 100)];
        let switches = [(1, Lsn(150))];
        assert_eq!(choose_base(&bases, Some(2), None, &switches).unwrap().id, "b4");
        assert_eq!(choose_base(&bases, Some(2), Some(Lsn(350)), &switches).unwrap().id, "b1");
        // On timeline 1 itself, its later bases are fine.
        assert_eq!(choose_base(&bases, Some(1), Some(Lsn(350)), &switches).unwrap().id, "b3");
    }

    #[test]
    fn no_history_file_means_no_ancestors() {
        let bases = [base("b2", 1, 200)];
        assert!(choose_base(&bases, Some(2), Some(Lsn(500)), &[]).is_none());
    }
}
