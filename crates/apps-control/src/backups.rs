//! Volume backups (docs/apps/README.md, "Volume backups"; Railway's volume backups, Render's
//! and Fly's snapshots): a volume's files as a tar in object storage,
//! `apps/volumes/<service>/<env>/<unix ms>.tar`, made on demand and on a schedule (once a day,
//! the newest seven kept, unless `volume.backups` says otherwise); a restore takes one of the
//! environment's own, or another environment's of the service (seeding staging from production). A backup streams from a throwaway container that mounts the volume read-only (no
//! network), so it's what's on disk at that moment: an app mid-write may leave a file half
//! written, as any snapshot of a running disk. A restore holds the environment asleep, empties
//! the volume, writes the backup's files and lets the next request wake it.

use crate::{Apps, Error, Result, volume_name};
use serde::Serialize;
use std::sync::Arc;

/// How long a deleted environment's (or service's) backups are kept to restore from by hand.
const ORPHAN_MS: i64 = 7 * 24 * 3_600_000;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Backup {
    /// Its time, unix ms, as text (the restore names it).
    pub id: String,
    pub at: i64,
    pub size: u64,
}

fn prefix(service: &str, env: &str) -> String {
    format!("apps/volumes/{service}/{env}/")
}

impl Apps {
    fn backup_store(&self) -> Result<&pg_storage::Objects> {
        self.deps.objects.as_ref().ok_or_else(|| Error::Conflict("volume backups need object storage on this node".into()))
    }

    /// The environment's volume, and its backups' schedule and retention (its own run's, else the
    /// service's).
    async fn volume_of(self: &Arc<Self>, service: &str, env: &str) -> Result<(String, crate::model::VolumeBackups)> {
        let (sid, e) = (service.to_string(), env.to_string());
        let (svc, row) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await?;
        let run = svc.for_env(&row).map(|s| s.run).unwrap_or_else(|_| svc.run.clone());
        let Some(v) = run.volume else {
            return Err(Error::Conflict(format!("{} has no volume", svc.name)));
        };
        Ok((volume_name(service, env), v.backups.unwrap_or(crate::model::VolumeBackups::DEFAULT)))
    }

    /// `GET …/volume/backups`: newest first.
    pub async fn backups(self: &Arc<Self>, service: &str, env: &str) -> Result<Vec<Backup>> {
        self.volume_of(service, env).await?;
        self.stored_backups(service, env).await
    }

    /// The backups kept under an environment's name, newest first (its volume or not).
    async fn stored_backups(self: &Arc<Self>, service: &str, env: &str) -> Result<Vec<Backup>> {
        let objects = self.backup_store()?;
        let p = prefix(service, env);
        let mut out: Vec<Backup> = objects
            .list(&p)
            .await
            .map_err(|e| Error::Internal(e.to_string()))?
            .into_iter()
            .filter_map(|(key, size)| {
                let id = key.strip_prefix(&p)?.strip_suffix(".tar")?.to_string();
                let at = id.parse().ok()?;
                Some(Backup { id, at, size })
            })
            .collect();
        out.sort_by_key(|b| std::cmp::Reverse(b.at));
        Ok(out)
    }

    /// `POST …/volume/backups`: back the volume up now, then keep the newest `volume.backups.keep`.
    pub async fn backup_volume(self: &Arc<Self>, service: &str, env: &str) -> Result<Backup> {
        let (volume, kept) = self.volume_of(service, env).await?;
        let objects = self.backup_store()?;
        let at = code_sql::now_ms();
        let key = format!("{}{at}.tar", prefix(service, env));
        let tar = self.deps.runtime.read_volume(&volume).await.map_err(|e| Error::Internal(format!("reading the volume: {e:#}")))?;
        let size = objects.upload(&key, tar).await.map_err(|e| Error::Internal(format!("storing the backup: {e:#}")))?;
        self.logs.sink(format!("env:{service}/{env}"), crate::logs::Stream::System).line(format!("volume backed up ({} KiB)", size / 1024));
        for old in self.stored_backups(service, env).await?.into_iter().skip(kept.keep as usize) {
            let _ = objects.delete(&format!("{}{}.tar", prefix(service, env), old.id)).await;
        }
        Ok(Backup { id: at.to_string(), at, size })
    }

    /// The volume's last backup before it goes (its environment or service deleted): taken after
    /// its instances stopped, kept [`ORPHAN_MS`] under the same prefix, best effort (a node
    /// without object storage, or a service without a volume, has none to take).
    pub(crate) async fn last_backup(self: &Arc<Self>, service: &str, env: &str, had_volume: bool) {
        let Some(objects) = self.deps.objects.as_ref().filter(|_| had_volume) else { return };
        let at = code_sql::now_ms();
        let key = format!("{}{at}.tar", prefix(service, env));
        let taken = async {
            let tar = self.deps.runtime.read_volume(&volume_name(service, env)).await?;
            objects.upload(&key, tar).await
        }
        .await;
        match taken {
            Ok(size) => tracing::info!(service, env, size, "apps: a deleted environment's volume was backed up (kept a week)"),
            Err(e) => tracing::warn!(service, env, error = %e, "apps: a deleted environment's volume couldn't be backed up"),
        }
    }

    /// `POST …/volume/backups/{id}/restore {from?}`: the volume as it was then; `from`: a backup
    /// of another environment of the service (seeding staging from production). The backup is
    /// downloaded whole and checked against its listed size before anything touches the volume;
    /// then the environment is held asleep (requests get a 503 a moment; rollouts and cron runs
    /// wait), and the runtime swaps the volume's files for the backup's only once it extracted
    /// whole. On its own task, so a client that goes away can't leave it half done or held.
    pub async fn restore_volume(self: &Arc<Self>, service: &str, env: &str, id: &str, from: Option<&str>) -> Result<Backup> {
        let (volume, _) = self.volume_of(service, env).await?;
        let objects = self.backup_store()?.clone();
        // Another environment's: one of the service's that exists (its backups, its volume or not).
        let source = from.unwrap_or(env).to_string();
        if source != env {
            let (sid, f) = (service.to_string(), source.clone());
            self.db(move |s| s.environment(&sid, &f)).await?;
        }
        let Some(b) = self.stored_backups(service, &source).await?.into_iter().find(|b| b.id == id) else {
            return Err(Error::NotFound(format!("no backup {id} of {service} in {source}")));
        };
        let (this, service, env) = (self.clone(), service.to_string(), env.to_string());
        tokio::spawn(async move {
            let key = format!("{}{}.tar", prefix(&service, &source), b.id);
            let dir = this.cfg.home.join("tmp");
            tokio::fs::create_dir_all(&dir).await.map_err(|e| Error::Internal(e.to_string()))?;
            let path = dir.join(format!("restore-{}.tar", crate::model::random(12).to_ascii_lowercase()));
            let fetched = async {
                let file = tokio::fs::File::create(&path).await?;
                let n = objects.download(&key, file).await?;
                anyhow::ensure!(n == b.size, "the backup came back with {n} of its {} bytes", b.size);
                Ok(())
            }
            .await;
            let restored = match fetched {
                Ok(()) => {
                    // What's there now, first: a restore can be undone by restoring that.
                    if let Err(e) = this.backup_volume(&service, &env).await {
                        let _ = tokio::fs::remove_file(&path).await;
                        return Err(Error::Internal(format!("backing up what's there before the restore (the volume is untouched): {e}")));
                    }
                    let _held = this.live(&service, &env).hold_asleep().await;
                    match tokio::fs::File::open(&path).await {
                        Ok(f) => this.deps.runtime.write_volume(&volume, Box::new(f)).await.map_err(|e| Error::Internal(format!("writing the volume (it's as it was): {e:#}"))),
                        Err(e) => Err(Error::Internal(e.to_string())),
                    }
                }
                Err(e) => Err(Error::Internal(format!("reading the backup (the volume is untouched): {e:#}"))),
            };
            let _ = tokio::fs::remove_file(&path).await;
            restored?;
            let whose = if source == env { String::new() } else { format!(" of {source}") };
            this.logs.sink(format!("env:{service}/{env}"), crate::logs::Stream::System).line(format!("volume restored from the backup{whose} of {}", b.id));
            Ok(b)
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
    }

    /// Once an hour, on the owner: how full each volume is (`du` in a helper container on Docker),
    /// kept for the environment's answer, and `volume.full` once it passes 90% of its size (again
    /// only after it went under 80%): a volume that fills up takes its database down.
    pub async fn measure_volumes(self: &Arc<Self>) {
        for l in self.lives().into_iter().filter(|l| self.owns(&l.service)) {
            let Some(vol) = l.template().and_then(|t| t.run.volume.clone()) else { continue };
            let Some(bytes) = self.deps.runtime.volume_used(&volume_name(&l.service, &l.env)).await else { continue };
            let size = vol.size_gb as u64 * (1 << 30);
            let pct = bytes.saturating_mul(100).checked_div(size).unwrap_or(0);
            let (used, full) = (format!("volume_used:{}/{}", l.service, l.env), format!("volume_full:{}/{}", l.service, l.env));
            let (sid, en, now) = (l.service.clone(), l.env.clone(), code_sql::now_ms());
            let alert = self
                .db(move |s| {
                    s.set_state(&used, &format!("{bytes} {now}"))?;
                    let was = s.get_state(&full)?.is_some_and(|v| !v.is_empty());
                    if pct >= 90 && !was {
                        s.set_state(&full, "1")?;
                        s.add_event("volume.full", &sid, &en, None, &serde_json::json!({ "used_bytes": bytes, "size_gb": vol.size_gb, "pct": pct }))?;
                        return Ok(true);
                    }
                    if pct < 80 && was {
                        s.set_state(&full, "")?;
                    }
                    Ok(false)
                })
                .await
                .unwrap_or(false);
            if alert {
                l.log.stream(crate::logs::Stream::System).line(format!("the volume is {pct}% full ({} of {} GB)", human_bytes(bytes), vol.size_gb));
            }
        }
    }

    /// An environment's schedules as they stand: the next due time (the run its live deployment
    /// runs with: roll.toml's included) and the job each last started.
    pub(crate) async fn schedule_states(self: &Arc<Self>, svc: &crate::store::ServiceRow, env: &crate::store::EnvRow) -> Vec<crate::model::ScheduleState> {
        let run = match self.lives.lock().get(&crate::env_key(&svc.id, &env.name)).and_then(|l| l.template()) {
            Some(t) => t.run.clone(),
            None => svc.run.clone(),
        };
        let mut out = Vec::new();
        for sc in &run.schedules {
            let next = (env.active.is_some() && !env.paused && !env.ephemeral)
                .then(|| crate::cron::Schedule::parse(&sc.schedule, sc.timezone.as_deref()).ok())
                .flatten()
                .and_then(|s| s.next_after(code_sql::now_ms()));
            let (sid, en, key) = (svc.id.clone(), env.name.clone(), format!("sched-job:{}/{}/{}", svc.id, env.name, sc.name));
            let last = self
                .db(move |s| {
                    let Some(job) = s.get_state(&key)? else { return Ok(None) };
                    Ok(s.job(&sid, &en, &job).ok())
                })
                .await
                .ok()
                .flatten()
                .map(|j| crate::model::ScheduleRun { job: j.id, status: j.status, started_at: j.started_at });
            let k = format!("sched-call:{}/{}/{}", svc.id, env.name, sc.name);
            let last_call = match &sc.path {
                Some(_) => self.db(move |s| s.get_state(&k)).await.ok().flatten().and_then(|v| serde_json::from_str(&v).ok()),
                None => None,
            };
            out.push(crate::model::ScheduleState { name: sc.name.clone(), next_run_at: next, last, last_call });
        }
        out
    }

    /// What an environment's volume was last measured at: (bytes, when).
    pub(crate) async fn volume_usage(self: &Arc<Self>, service: &str, env: &str) -> Option<(u64, i64)> {
        let key = format!("volume_used:{service}/{env}");
        let v = self.db(move |s| s.get_state(&key)).await.ok().flatten()?;
        let (b, at) = v.split_once(' ')?;
        Some((b.parse().ok()?, at.parse().ok()?))
    }

    /// Once an hour, on the owner: a backup of each volume whose newest is a day old.
    pub(crate) async fn scheduled_backups(self: &Arc<Self>) {
        if self.deps.objects.is_none() {
            return;
        }
        for l in self.lives().into_iter().filter(|l| self.owns(&l.service)) {
            let Some(tpl) = l.template() else { continue };
            let Some(v) = &tpl.run.volume else { continue };
            // Its own schedule (0: by hand only), else once a day.
            let every = v.backups.unwrap_or(crate::model::VolumeBackups::DEFAULT).every_hours;
            if every == 0 {
                continue;
            }
            let newest = self.backups(&l.service, &l.env).await.ok().and_then(|b| b.first().map(|b| b.at)).unwrap_or(0);
            if code_sql::now_ms() - newest < i64::from(every) * 3_600_000 {
                continue;
            }
            if let Err(e) = self.backup_volume(&l.service, &l.env).await {
                tracing::warn!(service = %l.service, env = %l.env, error = %e, "apps: a scheduled volume backup failed");
                // Said where the team looks: its log, and an event its targets hear by default.
                l.log.stream(crate::logs::Stream::System).line(format!("the scheduled volume backup failed: {e}"));
                let (sid, en, data) = (l.service.clone(), l.env.clone(), serde_json::json!({ "error": e.to_string() }));
                let _ = self.db(move |s| s.add_event("volume.backup_failed", &sid, &en, None, &data)).await;
            }
        }
        // Once on the fleet: a deleted environment's (or service's) backups go after a week.
        if self.deps.cluster.singleton("apps:spend") {
            self.expire_orphaned_backups().await;
        }
    }

    async fn expire_orphaned_backups(self: &Arc<Self>) {
        let Some(objects) = self.deps.objects.as_ref() else { return };
        let Ok(envs) = self.db(|s| s.all_environments()).await else { return };
        let live: std::collections::HashSet<(String, String)> = envs.into_iter().map(|e| (e.service, e.name)).collect();
        let Ok(all) = objects.list("apps/volumes/").await else { return };
        let now = code_sql::now_ms();
        for (key, _) in all {
            let mut parts = key.trim_start_matches("apps/volumes/").splitn(3, '/');
            let (Some(svc), Some(env), Some(file)) = (parts.next(), parts.next(), parts.next()) else { continue };
            let at: i64 = file.trim_end_matches(".tar").parse().unwrap_or(now);
            if !live.contains(&(svc.to_string(), env.to_string())) && now - at > ORPHAN_MS {
                let _ = objects.delete(&key).await;
            }
        }
    }
}

fn human_bytes(b: u64) -> String {
    match b {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.0} MB", b as f64 / (1u64 << 20) as f64),
        b => format!("{} KB", b / 1024),
    }
}
