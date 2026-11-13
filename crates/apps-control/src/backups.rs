//! Volume backups (docs/apps/README.md, "Volume backups"; Railway's volume backups, Render's
//! and Fly's snapshots): a volume's files as a tar in object storage,
//! `apps/volumes/<service>/<env>/<unix ms>.tar`, made on demand and once a day, the newest
//! seven kept. A backup streams from a throwaway container that mounts the volume read-only (no
//! network), so it's what's on disk at that moment: an app mid-write may leave a file half
//! written, as any snapshot of a running disk. A restore holds the environment asleep, empties
//! the volume, writes the backup's files and lets the next request wake it.

use crate::{Apps, Error, Result, volume_name};
use serde::Serialize;
use std::sync::Arc;

/// Backups kept per environment (the oldest go after a new one).
const KEEP: usize = 7;
/// A day between scheduled backups.
const EVERY_MS: i64 = 24 * 3_600_000;

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

    async fn volume_of(self: &Arc<Self>, service: &str, env: &str) -> Result<String> {
        let (sid, e) = (service.to_string(), env.to_string());
        let (svc, _) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await?;
        if svc.run.volume.is_none() {
            return Err(Error::Conflict(format!("{} has no volume", svc.name)));
        }
        Ok(volume_name(service, env))
    }

    /// `GET …/volume/backups`: newest first.
    pub async fn backups(self: &Arc<Self>, service: &str, env: &str) -> Result<Vec<Backup>> {
        self.volume_of(service, env).await?;
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

    /// `POST …/volume/backups`: back the volume up now, then keep the newest [`KEEP`].
    pub async fn backup_volume(self: &Arc<Self>, service: &str, env: &str) -> Result<Backup> {
        let volume = self.volume_of(service, env).await?;
        let objects = self.backup_store()?;
        let at = code_sql::now_ms();
        let key = format!("{}{at}.tar", prefix(service, env));
        let tar = self.deps.runtime.read_volume(&volume).await.map_err(|e| Error::Internal(format!("reading the volume: {e:#}")))?;
        let size = objects.upload(&key, tar).await.map_err(|e| Error::Internal(format!("storing the backup: {e:#}")))?;
        self.logs.sink(format!("env:{service}/{env}"), crate::logs::Stream::System).line(format!("volume backed up ({} KiB)", size / 1024));
        for old in self.backups(service, env).await?.into_iter().skip(KEEP) {
            let _ = objects.delete(&format!("{}{}.tar", prefix(service, env), old.id)).await;
        }
        Ok(Backup { id: at.to_string(), at, size })
    }

    /// `POST …/volume/backups/{id}/restore`: the volume as it was then. The environment is held
    /// asleep meanwhile (requests get a 503 a moment) and wakes on the next request.
    pub async fn restore_volume(self: &Arc<Self>, service: &str, env: &str, id: &str) -> Result<Backup> {
        let volume = self.volume_of(service, env).await?;
        let objects = self.backup_store()?.clone();
        let Some(b) = self.backups(service, env).await?.into_iter().find(|b| b.id == id) else {
            return Err(Error::NotFound(format!("no backup {id} of {service} in {env}")));
        };
        let live = self.live(service, env);
        live.set_held(true);
        live.sleep().await;
        let key = format!("{}{}.tar", prefix(service, env), b.id);
        let (reader, writer) = tokio::io::duplex(1 << 20);
        let download = tokio::spawn(async move { objects.download(&key, writer).await });
        let wrote = self.deps.runtime.write_volume(&volume, Box::new(reader)).await;
        let read = download.await.map_err(|e| Error::Internal(e.to_string()))?;
        live.set_held(false);
        read.map_err(|e| Error::Internal(format!("reading the backup: {e:#}")))?;
        wrote.map_err(|e| Error::Internal(format!("writing the volume: {e:#}")))?;
        self.logs.sink(format!("env:{service}/{env}"), crate::logs::Stream::System).line(format!("volume restored from the backup of {}", b.id));
        Ok(b)
    }

    /// Once an hour, on the owner: a backup of each volume whose newest is a day old.
    pub(crate) async fn scheduled_backups(self: &Arc<Self>) {
        if self.deps.objects.is_none() {
            return;
        }
        for l in self.lives().into_iter().filter(|l| self.owns(&l.service)) {
            let Some(tpl) = l.template() else { continue };
            if tpl.run.volume.is_none() {
                continue;
            }
            let newest = self.backups(&l.service, &l.env).await.ok().and_then(|b| b.first().map(|b| b.at)).unwrap_or(0);
            if code_sql::now_ms() - newest < EVERY_MS {
                continue;
            }
            if let Err(e) = self.backup_volume(&l.service, &l.env).await {
                tracing::warn!(service = %l.service, env = %l.env, error = %e, "apps: a scheduled volume backup failed");
            }
        }
    }
}
