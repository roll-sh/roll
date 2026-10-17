//! Compute driver: runs unmodified Postgres containers via the Docker CLI.
//!
//! Computes are disposable. A branch's data directory is a local cache of what's durable in
//! object storage; waking an endpoint creates a fresh container over that directory, suspending
//! stops it. Everything that touches PGDATA runs inside the Postgres image, so file ownership is
//! right on any host. The same shape maps onto ECS/EC2 tasks.

use anyhow::{Context, bail};
use pg_storage::{Lsn, Objects};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{debug, warn};

pub const NETWORK: &str = "roll";
/// Same on every compute: standbys require these to be >= the primary's.
const MAX_CONNECTIONS: u32 = 200;

pub fn container_name(endpoint_id: &str) -> String {
    format!("roll-{endpoint_id}")
}

/// Where the data directory is mounted, and where PGDATA lives inside it.
///
/// PGDATA is always a subdirectory (`<major>/docker`, the Postgres 18 image layout) rather than
/// the mount root: Docker Desktop doesn't reliably apply the entrypoint's `chown` to a bind-mount
/// root, which makes the first start fail. Older versions mount over the image's `VOLUME` so no
/// anonymous volume is created. Directories from before this layout keep working.
fn mount_and_pgdata(data_dir: &Path, pg_version: u32) -> (&'static str, String) {
    let mount = if pg_version >= 18 { "/var/lib/postgresql" } else { "/var/lib/postgresql/data" };
    if legacy_layout(data_dir, pg_version) {
        return (mount, mount.into());
    }
    (mount, format!("{mount}/{pg_version}/docker"))
}

/// A Postgres <18 cluster written directly at the mount root.
fn legacy_layout(data_dir: &Path, pg_version: u32) -> bool {
    pg_version < 18 && data_dir.join("PG_VERSION").exists()
}

/// PGDATA as seen from the host.
pub fn host_pgdata(data_dir: &Path, pg_version: u32) -> PathBuf {
    if legacy_layout(data_dir, pg_version) {
        return data_dir.to_path_buf();
    }
    data_dir.join(pg_version.to_string()).join("docker")
}

/// Whether a data directory holds an initialised cluster.
pub fn has_cluster(data_dir: &Path, pg_version: u32) -> bool {
    host_pgdata(data_dir, pg_version).join("PG_VERSION").exists()
}

pub struct RunSpec<'a> {
    pub endpoint_id: &'a str,
    pub image: String,
    pub pg_version: u32,
    pub data_dir: &'a Path,
    pub password: &'a str,
    /// CPU limit right now (moved live by the autoscaler).
    pub cpus: f64,
    /// Memory limit right now (moved live with CPU).
    pub mem_mb: u64,
    /// Sized for the minimum so it always fits; the OS page cache uses the rest.
    pub shared_buffers_mb: u64,
    /// Planner hint, sized for the maximum.
    pub cache_mb: u64,
}

pub struct BackupInfo {
    pub start: Lsn,
    pub end: Lsn,
    pub timeline: u32,
    pub bytes: u64,
}

pub struct SnapshotInfo {
    pub checkpoint: Lsn,
    pub redo: Lsn,
    pub timeline: u32,
    pub bytes: u64,
}

pub struct DockerDriver {
    init_dir: PathBuf,
    /// `roll.home=<dir>` on every container, so one daemon's computes can be found (and cleaned up).
    home_label: String,
    pub host_cpus: f64,
}

/// Lets standbys and branch copies use the replication protocol over the network.
const INIT_SCRIPT: &str = r#"#!/bin/sh
echo "host replication all all scram-sha-256" >> "$PGDATA/pg_hba.conf"
"#;

async fn docker(args: &[&str]) -> anyhow::Result<String> {
    debug!(?args, "docker");
    let out = Command::new("docker").args(args).output().await.context("running docker")?;
    if !out.status.success() {
        bail!("docker {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn after<'a>(text: &'a str, label: &str) -> Option<&'a str> {
    text.lines().find_map(|l| l.split_once(label).map(|(_, v)| v.trim()))
}

impl DockerDriver {
    pub async fn new(home: &Path) -> anyhow::Result<Self> {
        let init_dir = home.join("initdb.d");
        std::fs::create_dir_all(&init_dir)?;
        let script = init_dir.join("10-roll-replication.sh");
        std::fs::write(&script, INIT_SCRIPT)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        }
        if docker(&["network", "inspect", NETWORK]).await.is_err() {
            docker(&["network", "create", NETWORK]).await?;
        }
        let host_cpus = docker(&["info", "--format", "{{.NCPU}}"]).await?.parse().unwrap_or(1.0);
        Ok(Self { init_dir, home_label: format!("roll.home={}", home.display()), host_cpus })
    }

    fn cpus(&self, cu: f64) -> String {
        format!("{:.2}", cu.clamp(0.1, self.host_cpus))
    }

    /// Start a compute and return the host port Postgres is published on, once it accepts queries.
    pub async fn run(&self, spec: &RunSpec<'_>, ready_timeout: Duration) -> anyhow::Result<u16> {
        let name = container_name(spec.endpoint_id);
        let _ = docker(&["rm", "-f", &name]).await;
        std::fs::create_dir_all(spec.data_dir)?;

        let (mount, pgdata) = mount_and_pgdata(spec.data_dir, spec.pg_version);
        let settings = [
            format!("shared_buffers={}MB", spec.shared_buffers_mb.max(16)),
            format!("effective_cache_size={}MB", spec.cache_mb.max(64)),
            format!("max_connections={MAX_CONNECTIONS}"),
            "jit=off".into(),
            "wal_level=replica".into(),
            "max_wal_senders=10".into(),
            "max_replication_slots=10".into(),
            "wal_compression=on".into(),
            // Segments are archived by the safekeeper; don't keep or recycle extra copies.
            "wal_recycle=off".into(),
            "min_wal_size=32MB".into(),
            "max_slot_wal_keep_size=4GB".into(),
            "hot_standby_feedback=on".into(),
            "random_page_cost=1.1".into(),
            "effective_io_concurrency=200".into(),
            "shared_preload_libraries=pg_stat_statements".into(),
        ];

        let volume = format!("{}:{mount}", spec.data_dir.display());
        let init = format!("{}:/docker-entrypoint-initdb.d:ro", self.init_dir.display());
        let password = format!("POSTGRES_PASSWORD={}", spec.password);
        let pgdata_env = format!("PGDATA={pgdata}");
        let memory = format!("{}m", spec.mem_mb.max(64));
        let cpus = self.cpus(spec.cpus);
        let mut args = vec![
            "run", "-d", "--name", &name, "--network", NETWORK, "--label", "roll.endpoint", "--label", &self.home_label,
            "--add-host", "host.docker.internal:host-gateway",
            "-p", "127.0.0.1::5432", "--cpus", &cpus, "--memory", &memory, "--memory-swap", &memory,
            "--shm-size", "256m", "--stop-signal", "SIGINT", "--stop-timeout", "60", "-v", &volume, "-v", &init,
            "-e", "POSTGRES_USER=roll", "-e", "POSTGRES_DB=main", "-e", &password, "-e", &pgdata_env,
            &spec.image,
        ];
        let flags: Vec<String> = settings.iter().map(|s| format!("-c{s}")).collect();
        args.extend(flags.iter().map(String::as_str));

        // A brand-new cluster can fail its first start on Docker Desktop: the entrypoint's
        // `chown` of PGDATA isn't always visible yet when the server checks ownership. Retrying
        // as-is would skip initdb *and* the init scripts, so start the fresh cluster over.
        let fresh = !has_cluster(spec.data_dir, spec.pg_version);
        let mut attempt = 1;
        loop {
            docker(&args).await?;
            match self.wait_ready(&name, ready_timeout).await {
                Ok(()) => return self.port(&name).await,
                Err(e) if fresh && attempt < 3 && e.to_string().contains("has wrong ownership") => {
                    warn!(container = %name, attempt, "fresh cluster hit the ownership race; initialising again");
                    let _ = docker(&["rm", "-f", &name]).await;
                    let pgdata = host_pgdata(spec.data_dir, spec.pg_version);
                    std::fs::remove_dir_all(&pgdata).or_else(|e| match e.kind() {
                        std::io::ErrorKind::NotFound => Ok(()),
                        _ => Err(e),
                    })?;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn wait_ready(&self, name: &str, timeout: Duration) -> anyhow::Result<()> {
        // `-h 127.0.0.1` skips the socket-only server the entrypoint runs during initdb.
        let deadline = Instant::now() + timeout;
        let mut delay = Duration::from_millis(50);
        loop {
            if docker(&["exec", name, "pg_isready", "-q", "-h", "127.0.0.1", "-U", "roll", "-d", "main"]).await.is_ok() {
                return Ok(());
            }
            if Instant::now() > deadline {
                let logs = self.logs(name).await.unwrap_or_default();
                bail!("compute {name} did not become ready:\n{logs}");
            }
            if !self.is_running(name).await {
                let logs = self.logs(name).await.unwrap_or_default();
                bail!("compute {name} exited:\n{logs}");
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_millis(500));
        }
    }

    pub async fn logs(&self, name: &str) -> anyhow::Result<String> {
        let out = Command::new("docker").args(["logs", "--tail", "40", name]).output().await?;
        Ok(format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    }

    pub async fn port(&self, name: &str) -> anyhow::Result<u16> {
        let out = docker(&["port", name, "5432/tcp"]).await?;
        let line = out.lines().next().context("no published port")?;
        Ok(line.rsplit(':').next().context("bad port")?.parse()?)
    }

    pub async fn is_running(&self, name: &str) -> bool {
        docker(&["inspect", "-f", "{{.State.Running}}", name]).await.is_ok_and(|s| s == "true")
    }

    pub async fn stop(&self, endpoint_id: &str) -> anyhow::Result<()> {
        let name = container_name(endpoint_id);
        match docker(&["stop", "-t", "60", &name]).await {
            Err(e) if e.to_string().contains("No such container") => Ok(()),
            r => r.map(drop),
        }
    }

    pub async fn remove(&self, endpoint_id: &str) -> anyhow::Result<()> {
        let _ = docker(&["rm", "-f", &container_name(endpoint_id)]).await;
        Ok(())
    }

    /// Live resize of CPU and memory, without restart.
    pub async fn set_size(&self, endpoint_id: &str, cu: f64, mem_mb: u64) -> anyhow::Result<()> {
        let memory = format!("{}m", mem_mb.max(64));
        let name = container_name(endpoint_id);
        let cpus = self.cpus(cu);
        docker(&["update", "--cpus", &cpus, "--memory", &memory, "--memory-swap", &memory, &name]).await.map(drop)
    }

    /// Non-reclaimable memory (anonymous + shared buffers) of a running compute, in bytes.
    pub async fn memory_in_use(&self, endpoint_id: &str) -> Option<u64> {
        let stat = docker(&["exec", &container_name(endpoint_id), "cat", "/sys/fs/cgroup/memory.stat"]).await.ok()?;
        let get = |k: &str| {
            stat.lines().find_map(|l| l.strip_prefix(k).and_then(|v| v.trim().parse::<u64>().ok())).unwrap_or(0)
        };
        Some(get("anon ") + get("shmem "))
    }

    /// CPU use in cores for every running roll compute, keyed by endpoint id.
    pub async fn cpu_usage(&self) -> anyhow::Result<HashMap<String, f64>> {
        let filter = format!("label={}", self.home_label);
        let ids = docker(&["ps", "-q", "--filter", &filter]).await?;
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut args = vec!["stats", "--no-stream", "--format", "{{.Name}}\t{{.CPUPerc}}"];
        args.extend(ids.lines());
        let out = docker(&args).await?;
        Ok(out
            .lines()
            .filter_map(|l| {
                let (name, pct) = l.split_once('\t')?;
                let ep = name.strip_prefix("roll-")?;
                let cores = pct.trim_end_matches('%').parse::<f64>().ok()? / 100.0;
                Some((ep.to_string(), cores))
            })
            .collect())
    }

    /// Consistent copy of a running compute's data into `dest` with `pg_basebackup` (branching).
    pub async fn basebackup(
        &self,
        source_endpoint: &str,
        image: &str,
        pg_version: u32,
        password: &str,
        dest: &Path,
    ) -> anyhow::Result<()> {
        std::fs::create_dir_all(dest)?;
        let (mount, pgdata) = mount_and_pgdata(dest, pg_version);
        let volume = format!("{}:{mount}", dest.display());
        let pw = format!("PGPASSWORD={password}");
        let host = container_name(source_endpoint);
        docker(&[
            "run", "--rm", "--network", NETWORK, "-v", &volume, "-e", &pw, image,
            "pg_basebackup", "-h", &host, "-U", "roll", "-D", &pgdata, "-X", "stream", "-c", "fast",
        ])
        .await
        .map(drop)
    }

    /// Online base image of a running compute, streamed to object storage (zstd tar of PGDATA).
    /// Recovery from it needs the WAL archived from `start` onward.
    pub async fn basebackup_upload(
        &self,
        endpoint_id: &str,
        password: &str,
        objects: &Objects,
        key: &str,
    ) -> anyhow::Result<BackupInfo> {
        let pw = format!("PGPASSWORD={password}");
        let name = container_name(endpoint_id);
        let mut child = Command::new("docker")
            .args(["exec", "-e", &pw, &name])
            .args(["pg_basebackup", "-h", "127.0.0.1", "-U", "roll", "-D", "-", "-Ft", "-X", "none", "-c", "fast", "--no-manifest"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let err_task = tokio::spawn(async move {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s).await;
            s
        });
        // backup_label is the first member of the archive; keep the head to read it.
        let mut head = Vec::with_capacity(64 * 1024);
        let uploaded = objects
            .upload_zstd(key, stdout, |chunk| {
                let room = (64 * 1024usize).saturating_sub(head.len());
                head.extend_from_slice(&chunk[..room.min(chunk.len())]);
            })
            .await;
        let status = child.wait().await?;
        let log = err_task.await.unwrap_or_default();
        if !status.success() || uploaded.is_err() {
            let _ = objects.delete(key).await;
            bail!("pg_basebackup failed: {} {:?}", log.trim(), uploaded.err());
        }
        let (_, bytes) = uploaded?;
        // "START WAL LOCATION: 0/2000028 (file 000000010000000000000002)" / "START TIMELINE: 1"
        let label = String::from_utf8_lossy(&head);
        let start = after(&label, "START WAL LOCATION:")
            .and_then(|v| v.split_whitespace().next())
            .context("no backup_label in base backup")?;
        let timeline = after(&label, "START TIMELINE:").context("no timeline in backup_label")?;
        // The backup is consistent no later than the WAL position right after it finished.
        let end = docker(&["exec", &name, "psql", "-U", "roll", "-d", "main", "-Atc", "select pg_current_wal_lsn()"])
            .await
            .unwrap_or_default();
        let start: Lsn = start.parse()?;
        let end: Lsn = end.parse().unwrap_or(start);
        Ok(BackupInfo { start, end: end.max(start), timeline: timeline.trim().parse()?, bytes })
    }

    /// Cold image of a stopped compute's data directory, streamed to object storage.
    pub async fn snapshot_upload(
        &self,
        data_dir: &Path,
        pg_version: u32,
        image: &str,
        objects: &Objects,
        key: &str,
    ) -> anyhow::Result<SnapshotInfo> {
        let (mount, pgdata) = mount_and_pgdata(data_dir, pg_version);
        let volume = format!("{}:{mount}", data_dir.display());
        let env = format!("PGDATA={pgdata}");
        let script = r#"set -eo pipefail; pg_controldata -D "$PGDATA" >&2; cd "$PGDATA"; tar -cf - . | zstd -1 -q -T0"#;
        let mut child = Command::new("docker")
            .args(["run", "--rm", "-v", &volume, "-e", &env, image, "sh", "-c", script])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let err_task = tokio::spawn(async move {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s).await;
            s
        });
        let uploaded = objects.upload(key, stdout).await;
        let status = child.wait().await?;
        let log = err_task.await.unwrap_or_default();
        if !status.success() {
            let _ = objects.delete(key).await;
            bail!("snapshot failed: {}", log.trim());
        }
        let state = after(&log, "Database cluster state:").unwrap_or_default();
        if !state.starts_with("shut down") {
            let _ = objects.delete(key).await;
            bail!("refusing to snapshot a cluster that wasn't shut down cleanly (state: {state})");
        }
        Ok(SnapshotInfo {
            checkpoint: after(&log, "Latest checkpoint location:").context("no checkpoint location")?.parse()?,
            redo: after(&log, "Latest checkpoint's REDO location:").context("no REDO location")?.parse()?,
            timeline: after(&log, "Latest checkpoint's TimeLineID:").context("no timeline")?.parse()?,
            bytes: uploaded?,
        })
    }

    /// Unpack a base image from object storage into an empty data directory. `conf` is appended to
    /// `postgresql.auto.conf`; `signal` (`recovery.signal` / `standby.signal`) is created.
    #[allow(clippy::too_many_arguments)]
    pub async fn restore_download(
        &self,
        data_dir: &Path,
        pg_version: u32,
        image: &str,
        objects: &Objects,
        key: &str,
        conf: Option<&str>,
        signal: Option<&str>,
    ) -> anyhow::Result<u64> {
        std::fs::create_dir_all(data_dir)?;
        let (mount, pgdata) = mount_and_pgdata(data_dir, pg_version);
        let volume = format!("{}:{mount}", data_dir.display());
        let env = [
            format!("PGDATA={pgdata}"),
            format!("ROLL_CONF={}", conf.unwrap_or_default()),
            format!("ROLL_SIGNAL={}", signal.unwrap_or_default()),
        ];
        let script = r#"set -e
mkdir -p "$PGDATA"; cd "$PGDATA"
find . -mindepth 1 -delete
zstd -d -q -c | tar -xf -
rm -f postmaster.pid
if [ -n "$ROLL_CONF" ]; then printf '\n# roll restore\n%s\n' "$ROLL_CONF" >> postgresql.auto.conf; fi
if [ -n "$ROLL_SIGNAL" ]; then touch "$ROLL_SIGNAL"; fi
chmod 700 "$PGDATA""#;
        let mut child = Command::new("docker")
            .args(["run", "--rm", "-i", "-v", &volume, "-e", &env[0], "-e", &env[1], "-e", &env[2], image, "sh", "-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let err_task = tokio::spawn(async move {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s).await;
            s
        });
        let downloaded = objects.download(key, stdin).await; // dropping stdin closes the pipe
        let status = child.wait().await?;
        let log = err_task.await.unwrap_or_default();
        if !status.success() {
            bail!("restore failed: {}", log.trim());
        }
        downloaded
    }
}

/// Copy-on-write clone of a stopped branch's data (APFS clonefile / Linux reflinks).
pub async fn clone_dir(src: &Path, dest: &Path) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut cmd = Command::new("cp");
    if cfg!(target_os = "macos") {
        cmd.args(["-c", "-Rp"]);
    } else {
        cmd.args(["-a", "--reflink=auto"]);
    }
    let out = cmd.arg(src).arg(dest).output().await?;
    if !out.status.success() {
        bail!("cp: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Logical size of a directory tree.
pub fn dir_size(path: &Path) -> u64 {
    fn walk(p: &Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(p) else { return 0 };
        entries
            .flatten()
            .map(|e| match e.file_type() {
                Ok(t) if t.is_dir() => walk(&e.path()),
                Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => 0,
            })
            .sum()
    }
    walk(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pgdata_is_a_subdirectory_for_every_version() {
        let dir = std::env::temp_dir().join(format!("roll-layout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(mount_and_pgdata(&dir, 18), ("/var/lib/postgresql", "/var/lib/postgresql/18/docker".into()));
        assert_eq!(mount_and_pgdata(&dir, 16), ("/var/lib/postgresql/data", "/var/lib/postgresql/data/16/docker".into()));
        assert_eq!(host_pgdata(&dir, 17), dir.join("17/docker"));
        assert!(!has_cluster(&dir, 16));

        // A cluster written at the mount root by an older roll keeps its layout.
        std::fs::write(dir.join("PG_VERSION"), "16\n").unwrap();
        assert_eq!(mount_and_pgdata(&dir, 16).1, "/var/lib/postgresql/data");
        assert_eq!(host_pgdata(&dir, 16), dir);
        assert!(has_cluster(&dir, 16));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
