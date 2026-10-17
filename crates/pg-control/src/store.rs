//! Control-plane state in SQLite. Small, synchronous queries behind a mutex.

use crate::ids::now;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use std::path::Path;
use std::sync::Mutex;

#[derive(Clone, Debug, Serialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub pg_version: u32,
    pub region: String,
    #[serde(skip)]
    pub password: String,
    pub created_at: i64,
    /// Point-in-time restore window.
    pub history_retention_s: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalState {
    /// Data is on this host's disk (and in object storage).
    Local,
    /// Data lives only in object storage; the next start restores it.
    Evicted,
}

#[derive(Clone, Debug, Serialize)]
pub struct Branch {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub created_at: i64,
    pub local_state: LocalState,
    /// The data directory holds a restore that finishes on the next start.
    pub pending_recovery: bool,
    /// Point in time this branch was forked at, when created from history.
    pub parent_at: Option<i64>,
    /// Name of the current local data directory. Every restore goes to a fresh directory that
    /// is swapped in when complete, so a path is never reused.
    #[serde(skip)]
    pub data_dir: Option<String>,
}

impl Branch {
    pub fn dir_name(&self) -> &str {
        self.data_dir.as_deref().unwrap_or(&self.id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointState {
    Starting,
    Running,
    Suspended,
}

impl EndpointState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Suspended => "suspended",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "starting" => Self::Starting,
            "running" => Self::Running,
            _ => Self::Suspended,
        }
    }
}

pub const READ_WRITE: &str = "read_write";
pub const READ_ONLY: &str = "read_only";

#[derive(Clone, Debug, Serialize)]
pub struct Endpoint {
    pub id: String,
    pub project_id: String,
    pub branch_id: String,
    /// `read_write` (the branch primary) or `read_only` (a replica).
    pub kind: String,
    pub state: EndpointState,
    pub min_cu: f64,
    pub max_cu: f64,
    /// Current size, moved between min and max by the autoscaler.
    pub cu: f64,
    pub suspend_timeout_s: i64,
    pub host_port: Option<u16>,
    pub last_active: i64,
    pub created_at: i64,
    /// Replicas: name of the current local data directory.
    #[serde(skip)]
    pub data_dir: Option<String>,
}

impl Endpoint {
    pub fn is_primary(&self) -> bool {
        self.kind == READ_WRITE
    }
    pub fn dir_name(&self) -> &str {
        self.data_dir.as_deref().unwrap_or(&self.id)
    }
}

pub struct ComputeSegment {
    pub cu: f64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

pub struct StorageSample {
    pub branch_id: String,
    pub tier: String,
    pub at: i64,
    pub bytes: i64,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct WalPosition {
    pub timeline: u32,
    pub flush_lsn: u64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct BaseImage {
    pub id: String,
    pub branch_id: String,
    /// `online` (pg_basebackup of a running compute) or `snapshot` (cold copy of a stopped one).
    pub kind: String,
    pub key: String,
    pub timeline: u32,
    /// Recovery from this image needs WAL from `start_lsn`…
    pub start_lsn: u64,
    /// …and is consistent at `end_lsn`.
    pub end_lsn: u64,
    /// The branch's durable WAL position when the image was taken.
    pub flush_lsn: u64,
    pub created_at: i64,
    pub bytes: i64,
}

pub struct Store {
    conn: Mutex<Connection>,
}

/// Applied in order; `PRAGMA user_version` records how many have run.
const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE IF NOT EXISTS projects (
  id TEXT PRIMARY KEY, name TEXT NOT NULL, pg_version INTEGER NOT NULL, region TEXT NOT NULL,
  password TEXT NOT NULL, created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS branches (
  id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL, parent_id TEXT, created_at INTEGER NOT NULL, UNIQUE (project_id, name)
);
CREATE TABLE IF NOT EXISTS endpoints (
  id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  branch_id TEXT NOT NULL REFERENCES branches(id) ON DELETE CASCADE, kind TEXT NOT NULL,
  state TEXT NOT NULL, min_cu REAL NOT NULL, max_cu REAL NOT NULL, cu REAL NOT NULL,
  suspend_timeout_s INTEGER NOT NULL, host_port INTEGER, last_active INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS compute_segments (
  id INTEGER PRIMARY KEY AUTOINCREMENT, endpoint_id TEXT NOT NULL, project_id TEXT NOT NULL,
  cu REAL NOT NULL, started_at INTEGER NOT NULL, ended_at INTEGER
);
CREATE INDEX IF NOT EXISTS compute_segments_project ON compute_segments (project_id, started_at);
CREATE TABLE IF NOT EXISTS storage_samples (
  branch_id TEXT NOT NULL, project_id TEXT NOT NULL, at INTEGER NOT NULL, bytes INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS storage_samples_project ON storage_samples (project_id, at);
"#,
    r#"
ALTER TABLE projects ADD COLUMN history_retention_s INTEGER NOT NULL DEFAULT 604800;
ALTER TABLE branches ADD COLUMN local_state TEXT NOT NULL DEFAULT 'local';
ALTER TABLE branches ADD COLUMN pending_recovery INTEGER NOT NULL DEFAULT 0;
ALTER TABLE branches ADD COLUMN parent_at INTEGER;
ALTER TABLE storage_samples ADD COLUMN tier TEXT NOT NULL DEFAULT 'local';
CREATE TABLE wal_positions (
  branch_id TEXT PRIMARY KEY, timeline INTEGER NOT NULL, flush_lsn INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE TABLE lsn_index (branch_id TEXT NOT NULL, at INTEGER NOT NULL, timeline INTEGER NOT NULL, lsn INTEGER NOT NULL);
CREATE INDEX lsn_index_branch ON lsn_index (branch_id, at);
CREATE TABLE base_images (
  id TEXT PRIMARY KEY, branch_id TEXT NOT NULL, kind TEXT NOT NULL, key TEXT NOT NULL,
  timeline INTEGER NOT NULL, start_lsn INTEGER NOT NULL, end_lsn INTEGER NOT NULL, flush_lsn INTEGER NOT NULL,
  created_at INTEGER NOT NULL, bytes INTEGER NOT NULL
);
CREATE INDEX base_images_branch ON base_images (branch_id, created_at);
"#,
    r#"
ALTER TABLE branches ADD COLUMN data_dir TEXT;
ALTER TABLE endpoints ADD COLUMN data_dir TEXT;
"#,
];

fn project_row(r: &Row) -> rusqlite::Result<Project> {
    Ok(Project {
        id: r.get(0)?,
        name: r.get(1)?,
        pg_version: r.get(2)?,
        region: r.get(3)?,
        password: r.get(4)?,
        created_at: r.get(5)?,
        history_retention_s: r.get(6)?,
    })
}

fn branch_row(r: &Row) -> rusqlite::Result<Branch> {
    Ok(Branch {
        id: r.get(0)?,
        project_id: r.get(1)?,
        name: r.get(2)?,
        parent_id: r.get(3)?,
        created_at: r.get(4)?,
        local_state: if r.get::<_, String>(5)? == "evicted" { LocalState::Evicted } else { LocalState::Local },
        pending_recovery: r.get(6)?,
        parent_at: r.get(7)?,
        data_dir: r.get(8)?,
    })
}

fn endpoint_row(r: &Row) -> rusqlite::Result<Endpoint> {
    Ok(Endpoint {
        id: r.get(0)?,
        project_id: r.get(1)?,
        branch_id: r.get(2)?,
        kind: r.get(3)?,
        state: EndpointState::parse(&r.get::<_, String>(4)?),
        min_cu: r.get(5)?,
        max_cu: r.get(6)?,
        cu: r.get(7)?,
        suspend_timeout_s: r.get(8)?,
        host_port: r.get(9)?,
        last_active: r.get(10)?,
        created_at: r.get(11)?,
        data_dir: r.get(12)?,
    })
}

fn base_row(r: &Row) -> rusqlite::Result<BaseImage> {
    Ok(BaseImage {
        id: r.get(0)?,
        branch_id: r.get(1)?,
        kind: r.get(2)?,
        key: r.get(3)?,
        timeline: r.get(4)?,
        start_lsn: r.get::<_, i64>(5)? as u64,
        end_lsn: r.get::<_, i64>(6)? as u64,
        flush_lsn: r.get::<_, i64>(7)? as u64,
        created_at: r.get(8)?,
        bytes: r.get(9)?,
    })
}

const PROJECT_COLS: &str = "id, name, pg_version, region, password, created_at, history_retention_s";
const BRANCH_COLS: &str =
    "id, project_id, name, parent_id, created_at, local_state, pending_recovery, parent_at, data_dir";
const ENDPOINT_COLS: &str = "id, project_id, branch_id, kind, state, min_cu, max_cu, cu, suspend_timeout_s, host_port, \
     last_active, created_at, data_dir";
const BASE_COLS: &str = "id, branch_id, kind, key, timeline, start_lsn, end_lsn, flush_lsn, created_at, bytes";

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
            tx.commit()?;
        }
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> anyhow::Result<T> {
        Ok(f(&self.conn.lock().unwrap())?)
    }

    // ── projects ──

    pub fn insert_project(&self, p: &Project) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                &format!("INSERT INTO projects ({PROJECT_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"),
                params![p.id, p.name, p.pg_version, p.region, p.password, p.created_at, p.history_retention_s],
            )
            .map(drop)
        })
    }

    pub fn projects(&self) -> anyhow::Result<Vec<Project>> {
        self.with(|c| {
            let mut s = c.prepare(&format!("SELECT {PROJECT_COLS} FROM projects ORDER BY created_at"))?;
            s.query_map([], project_row)?.collect()
        })
    }

    pub fn project(&self, id: &str) -> anyhow::Result<Option<Project>> {
        self.with(|c| {
            c.query_row(&format!("SELECT {PROJECT_COLS} FROM projects WHERE id = ?1"), [id], project_row)
                .optional()
        })
    }

    pub fn set_history_retention(&self, id: &str, seconds: i64) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute("UPDATE projects SET history_retention_s = ?2 WHERE id = ?1", params![id, seconds]).map(drop)
        })
    }

    pub fn delete_project(&self, id: &str) -> anyhow::Result<()> {
        self.with(|c| {
            for table in ["compute_segments", "storage_samples"] {
                c.execute(&format!("DELETE FROM {table} WHERE project_id = ?1"), [id])?;
            }
            for table in ["wal_positions", "lsn_index", "base_images"] {
                c.execute(
                    &format!("DELETE FROM {table} WHERE branch_id IN (SELECT id FROM branches WHERE project_id = ?1)"),
                    [id],
                )?;
            }
            c.execute("DELETE FROM projects WHERE id = ?1", [id]).map(drop)
        })
    }

    // ── branches ──

    pub fn insert_branch(&self, b: &Branch) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                &format!("INSERT INTO branches ({BRANCH_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"),
                params![
                    b.id,
                    b.project_id,
                    b.name,
                    b.parent_id,
                    b.created_at,
                    match b.local_state {
                        LocalState::Local => "local",
                        LocalState::Evicted => "evicted",
                    },
                    b.pending_recovery,
                    b.parent_at,
                    b.data_dir
                ],
            )
            .map(drop)
        })
    }

    pub fn branches(&self, project_id: &str) -> anyhow::Result<Vec<Branch>> {
        self.with(|c| {
            let mut s =
                c.prepare(&format!("SELECT {BRANCH_COLS} FROM branches WHERE project_id = ?1 ORDER BY created_at"))?;
            s.query_map([project_id], branch_row)?.collect()
        })
    }

    pub fn all_branches(&self) -> anyhow::Result<Vec<Branch>> {
        self.with(|c| {
            let mut s = c.prepare(&format!("SELECT {BRANCH_COLS} FROM branches"))?;
            s.query_map([], branch_row)?.collect()
        })
    }

    pub fn branch(&self, id: &str) -> anyhow::Result<Option<Branch>> {
        self.with(|c| {
            c.query_row(&format!("SELECT {BRANCH_COLS} FROM branches WHERE id = ?1"), [id], branch_row).optional()
        })
    }

    pub fn branch_by_name(&self, project_id: &str, name: &str) -> anyhow::Result<Option<Branch>> {
        self.with(|c| {
            c.query_row(
                &format!("SELECT {BRANCH_COLS} FROM branches WHERE project_id = ?1 AND name = ?2"),
                [project_id, name],
                branch_row,
            )
            .optional()
        })
    }

    pub fn set_local_state(&self, id: &str, state: LocalState) -> anyhow::Result<()> {
        let s = match state {
            LocalState::Local => "local",
            LocalState::Evicted => "evicted",
        };
        self.with(|c| c.execute("UPDATE branches SET local_state = ?2 WHERE id = ?1", params![id, s]).map(drop))
    }

    pub fn set_branch_data_dir(&self, id: &str, name: &str) -> anyhow::Result<()> {
        self.with(|c| c.execute("UPDATE branches SET data_dir = ?2 WHERE id = ?1", params![id, name]).map(drop))
    }

    pub fn set_endpoint_data_dir(&self, id: &str, name: &str) -> anyhow::Result<()> {
        self.with(|c| c.execute("UPDATE endpoints SET data_dir = ?2 WHERE id = ?1", params![id, name]).map(drop))
    }

    pub fn set_pending_recovery(&self, id: &str, pending: bool) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute("UPDATE branches SET pending_recovery = ?2 WHERE id = ?1", params![id, pending]).map(drop)
        })
    }

    pub fn delete_branch(&self, id: &str) -> anyhow::Result<()> {
        self.with(|c| {
            for table in ["storage_samples", "wal_positions", "lsn_index", "base_images"] {
                c.execute(&format!("DELETE FROM {table} WHERE branch_id = ?1"), [id])?;
            }
            c.execute("DELETE FROM branches WHERE id = ?1", [id]).map(drop)
        })
    }

    // ── endpoints ──

    pub fn insert_endpoint(&self, e: &Endpoint) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                &format!(
                    "INSERT INTO endpoints ({ENDPOINT_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
                ),
                params![
                    e.id,
                    e.project_id,
                    e.branch_id,
                    e.kind,
                    e.state.as_str(),
                    e.min_cu,
                    e.max_cu,
                    e.cu,
                    e.suspend_timeout_s,
                    e.host_port,
                    e.last_active,
                    e.created_at,
                    e.data_dir
                ],
            )
            .map(drop)
        })
    }

    pub fn endpoint(&self, id: &str) -> anyhow::Result<Option<Endpoint>> {
        self.with(|c| {
            c.query_row(&format!("SELECT {ENDPOINT_COLS} FROM endpoints WHERE id = ?1"), [id], endpoint_row)
                .optional()
        })
    }

    pub fn endpoints(&self, project_id: &str) -> anyhow::Result<Vec<Endpoint>> {
        self.with(|c| {
            let mut s = c.prepare(&format!(
                "SELECT {ENDPOINT_COLS} FROM endpoints WHERE project_id = ?1 ORDER BY created_at"
            ))?;
            s.query_map([project_id], endpoint_row)?.collect()
        })
    }

    pub fn branch_endpoints(&self, branch_id: &str) -> anyhow::Result<Vec<Endpoint>> {
        self.with(|c| {
            let mut s =
                c.prepare(&format!("SELECT {ENDPOINT_COLS} FROM endpoints WHERE branch_id = ?1 ORDER BY created_at"))?;
            s.query_map([branch_id], endpoint_row)?.collect()
        })
    }

    pub fn all_endpoints(&self) -> anyhow::Result<Vec<Endpoint>> {
        self.with(|c| {
            let mut s = c.prepare(&format!("SELECT {ENDPOINT_COLS} FROM endpoints"))?;
            s.query_map([], endpoint_row)?.collect()
        })
    }

    pub fn endpoint_for_branch(&self, branch_id: &str) -> anyhow::Result<Option<Endpoint>> {
        self.with(|c| {
            c.query_row(
                &format!("SELECT {ENDPOINT_COLS} FROM endpoints WHERE branch_id = ?1 AND kind = 'read_write'"),
                [branch_id],
                endpoint_row,
            )
            .optional()
        })
    }

    pub fn delete_endpoint(&self, id: &str) -> anyhow::Result<()> {
        self.with(|c| c.execute("DELETE FROM endpoints WHERE id = ?1", [id]).map(drop))
    }

    pub fn set_endpoint_state(&self, id: &str, state: EndpointState, host_port: Option<u16>) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE endpoints SET state = ?2, host_port = ?3 WHERE id = ?1",
                params![id, state.as_str(), host_port],
            )
            .map(drop)
        })
    }

    pub fn touch_endpoint(&self, id: &str) -> anyhow::Result<()> {
        self.with(|c| c.execute("UPDATE endpoints SET last_active = ?2 WHERE id = ?1", params![id, now()]).map(drop))
    }

    pub fn set_endpoint_cu(&self, id: &str, cu: f64) -> anyhow::Result<()> {
        self.with(|c| c.execute("UPDATE endpoints SET cu = ?2 WHERE id = ?1", params![id, cu]).map(drop))
    }

    pub fn update_endpoint_settings(
        &self,
        id: &str,
        min_cu: f64,
        max_cu: f64,
        suspend_timeout_s: i64,
    ) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE endpoints SET min_cu = ?2, max_cu = ?3, suspend_timeout_s = ?4, cu = MIN(MAX(cu, ?2), ?3) WHERE id = ?1",
                params![id, min_cu, max_cu, suspend_timeout_s],
            )
            .map(drop)
        })
    }

    // ── WAL & history ──

    pub fn wal_position(&self, branch_id: &str) -> anyhow::Result<Option<WalPosition>> {
        self.with(|c| {
            c.query_row(
                "SELECT timeline, flush_lsn, updated_at FROM wal_positions WHERE branch_id = ?1",
                [branch_id],
                |r| Ok(WalPosition { timeline: r.get(0)?, flush_lsn: r.get::<_, i64>(1)? as u64, updated_at: r.get(2)? }),
            )
            .optional()
        })
    }

    /// Record durable WAL progress and index it by wall-clock time (for restore-to-timestamp).
    pub fn record_flush(&self, branch_id: &str, timeline: u32, lsn: u64) -> anyhow::Result<()> {
        let at = now();
        self.with(|c| {
            c.execute(
                "INSERT INTO wal_positions (branch_id, timeline, flush_lsn, updated_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (branch_id) DO UPDATE SET timeline = ?2, flush_lsn = ?3, updated_at = ?4",
                params![branch_id, timeline, lsn as i64, at],
            )?;
            // One index row per second is plenty.
            c.execute("DELETE FROM lsn_index WHERE branch_id = ?1 AND at = ?2", params![branch_id, at])?;
            c.execute(
                "INSERT INTO lsn_index (branch_id, at, timeline, lsn) VALUES (?1, ?2, ?3, ?4)",
                params![branch_id, at, timeline, lsn as i64],
            )
            .map(drop)
        })
    }

    /// Latest durable position at or before `at`.
    pub fn lsn_at(&self, branch_id: &str, at: i64) -> anyhow::Result<Option<(u32, u64, i64)>> {
        self.with(|c| {
            c.query_row(
                "SELECT timeline, lsn, at FROM lsn_index WHERE branch_id = ?1 AND at <= ?2 ORDER BY at DESC, lsn DESC LIMIT 1",
                params![branch_id, at],
                |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64, r.get(2)?)),
            )
            .optional()
        })
    }

    pub fn trim_lsn_index(&self, branch_id: &str, before: i64) -> anyhow::Result<()> {
        self.with(|c| {
            // Keep the newest row before the cutoff so the window start stays resolvable.
            c.execute(
                "DELETE FROM lsn_index WHERE branch_id = ?1 AND at < (
                   SELECT COALESCE(MAX(at), ?2) FROM lsn_index WHERE branch_id = ?1 AND at < ?2)",
                params![branch_id, before],
            )
            .map(drop)
        })
    }

    pub fn insert_base(&self, b: &BaseImage) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                &format!("INSERT INTO base_images ({BASE_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"),
                params![
                    b.id,
                    b.branch_id,
                    b.kind,
                    b.key,
                    b.timeline,
                    b.start_lsn as i64,
                    b.end_lsn as i64,
                    b.flush_lsn as i64,
                    b.created_at,
                    b.bytes
                ],
            )
            .map(drop)
        })
    }

    /// Base images of a branch, newest first.
    pub fn bases(&self, branch_id: &str) -> anyhow::Result<Vec<BaseImage>> {
        self.with(|c| {
            let mut s = c.prepare(&format!(
                "SELECT {BASE_COLS} FROM base_images WHERE branch_id = ?1 ORDER BY created_at DESC, end_lsn DESC"
            ))?;
            s.query_map([branch_id], base_row)?.collect()
        })
    }

    pub fn delete_base(&self, id: &str) -> anyhow::Result<()> {
        self.with(|c| c.execute("DELETE FROM base_images WHERE id = ?1", [id]).map(drop))
    }

    // ── metering ──

    pub fn open_segment(&self, endpoint_id: &str, project_id: &str, cu: f64) -> anyhow::Result<()> {
        self.close_segment(endpoint_id)?;
        self.with(|c| {
            c.execute(
                "INSERT INTO compute_segments (endpoint_id, project_id, cu, started_at) VALUES (?1, ?2, ?3, ?4)",
                params![endpoint_id, project_id, cu, now()],
            )
            .map(drop)
        })
    }

    pub fn close_segment(&self, endpoint_id: &str) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE compute_segments SET ended_at = ?2 WHERE endpoint_id = ?1 AND ended_at IS NULL",
                params![endpoint_id, now()],
            )
            .map(drop)
        })
    }

    pub fn segments_since(&self, project_id: &str, since: i64) -> anyhow::Result<Vec<ComputeSegment>> {
        self.with(|c| {
            let mut s = c.prepare(
                "SELECT cu, started_at, ended_at FROM compute_segments
                 WHERE project_id = ?1 AND (ended_at IS NULL OR ended_at >= ?2)",
            )?;
            s.query_map(params![project_id, since], |r| {
                Ok(ComputeSegment { cu: r.get(0)?, started_at: r.get(1)?, ended_at: r.get(2)? })
            })?
            .collect()
        })
    }

    pub fn insert_storage_sample(&self, project_id: &str, branch_id: &str, tier: &str, bytes: i64) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO storage_samples (branch_id, project_id, tier, at, bytes) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![branch_id, project_id, tier, now(), bytes],
            )
            .map(drop)
        })
    }

    pub fn storage_since(&self, project_id: &str, since: i64) -> anyhow::Result<Vec<StorageSample>> {
        self.with(|c| {
            let mut s = c.prepare(
                "SELECT branch_id, tier, at, bytes FROM storage_samples WHERE project_id = ?1 AND at >= ?2 ORDER BY at",
            )?;
            s.query_map(params![project_id, since], |r| {
                Ok(StorageSample { branch_id: r.get(0)?, tier: r.get(1)?, at: r.get(2)?, bytes: r.get(3)? })
            })?
            .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("roll-store-{name}-{}", crate::ids::new_id("t")));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("state.db")
    }

    fn project(id: &str) -> Project {
        Project {
            id: id.into(),
            name: "p".into(),
            pg_version: 18,
            region: "local".into(),
            password: "pw".into(),
            created_at: 1,
            history_retention_s: 86_400,
        }
    }

    fn branch(id: &str, project: &str) -> Branch {
        Branch {
            id: id.into(),
            project_id: project.into(),
            name: id.into(),
            parent_id: None,
            created_at: 1,
            local_state: LocalState::Local,
            pending_recovery: false,
            parent_at: None,
            data_dir: None,
        }
    }

    #[test]
    fn migrates_a_version_zero_database() {
        let path = temp_db("migrate");
        {
            // The schema as first shipped, with a row in it.
            let c = Connection::open(&path).unwrap();
            c.execute_batch(MIGRATIONS[0]).unwrap();
            c.execute(
                "INSERT INTO projects (id, name, pg_version, region, password, created_at) VALUES ('prj_a', 'a', 18, 'local', 'pw', 1)",
                [],
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let p = store.project("prj_a").unwrap().unwrap();
        assert_eq!(p.history_retention_s, 604_800);
        let version: i64 = store.conn.lock().unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version as usize, MIGRATIONS.len());
        // Re-opening is a no-op.
        drop(store);
        Store::open(&path).unwrap();
    }

    #[test]
    fn branch_and_endpoint_roundtrip() {
        let store = Store::open(&temp_db("roundtrip")).unwrap();
        store.insert_project(&project("prj_a")).unwrap();
        let mut b = branch("br_a", "prj_a");
        b.parent_at = Some(42);
        store.insert_branch(&b).unwrap();
        store.set_branch_data_dir("br_a", "br_a_x").unwrap();
        store.set_local_state("br_a", LocalState::Evicted).unwrap();
        store.set_pending_recovery("br_a", true).unwrap();
        let got = store.branch("br_a").unwrap().unwrap();
        assert_eq!(got.dir_name(), "br_a_x");
        assert_eq!(got.local_state, LocalState::Evicted);
        assert!(got.pending_recovery);
        assert_eq!(got.parent_at, Some(42));
        assert_eq!(store.branch_by_name("prj_a", "br_a").unwrap().unwrap().id, "br_a");
        assert!(store.branch_by_name("prj_b", "br_a").unwrap().is_none());

        let ep = Endpoint {
            id: "ep_a".into(),
            project_id: "prj_a".into(),
            branch_id: "br_a".into(),
            kind: READ_WRITE.into(),
            state: EndpointState::Suspended,
            min_cu: 0.25,
            max_cu: 2.0,
            cu: 0.25,
            suspend_timeout_s: 300,
            host_port: None,
            last_active: 1,
            created_at: 1,
            data_dir: None,
        };
        store.insert_endpoint(&ep).unwrap();
        store.insert_endpoint(&Endpoint { id: "ep_r".into(), kind: READ_ONLY.into(), ..ep.clone() }).unwrap();
        assert_eq!(store.endpoint_for_branch("br_a").unwrap().unwrap().id, "ep_a");
        assert_eq!(store.branch_endpoints("br_a").unwrap().len(), 2);
        store.update_endpoint_settings("ep_a", 1.0, 4.0, 60).unwrap();
        let e = store.endpoint("ep_a").unwrap().unwrap();
        assert_eq!((e.min_cu, e.max_cu, e.cu, e.suspend_timeout_s), (1.0, 4.0, 1.0, 60)); // cu clamped up

        store.delete_project("prj_a").unwrap();
        assert!(store.branch("br_a").unwrap().is_none());
        assert!(store.endpoint("ep_r").unwrap().is_none());
    }

    #[test]
    fn lsn_index_resolves_times() {
        let store = Store::open(&temp_db("lsn")).unwrap();
        let rows = [(100, 1, 1000), (101, 1, 2000), (105, 1, 3000), (110, 2, 4000)];
        {
            let c = store.conn.lock().unwrap();
            for (at, tli, lsn) in rows {
                c.execute("INSERT INTO lsn_index VALUES ('br', ?1, ?2, ?3)", params![at, tli, lsn]).unwrap();
            }
        }
        assert_eq!(store.lsn_at("br", 99).unwrap(), None);
        assert_eq!(store.lsn_at("br", 100).unwrap(), Some((1, 1000, 100)));
        assert_eq!(store.lsn_at("br", 104).unwrap(), Some((1, 2000, 101)));
        assert_eq!(store.lsn_at("br", 999).unwrap(), Some((2, 4000, 110)));

        // Trimming keeps the newest row before the cutoff so the window start still resolves.
        store.trim_lsn_index("br", 104).unwrap();
        assert_eq!(store.lsn_at("br", 100).unwrap(), None);
        assert_eq!(store.lsn_at("br", 104).unwrap(), Some((1, 2000, 101)));
    }

    #[test]
    fn record_flush_tracks_position() {
        let store = Store::open(&temp_db("flush")).unwrap();
        store.record_flush("br", 1, 500).unwrap();
        store.record_flush("br", 1, 900).unwrap();
        let p = store.wal_position("br").unwrap().unwrap();
        assert_eq!((p.timeline, p.flush_lsn), (1, 900));
        // One index row per second.
        let n: i64 = store.conn.lock().unwrap().query_row("SELECT COUNT(*) FROM lsn_index", [], |r| r.get(0)).unwrap();
        assert!(n <= 2);
        assert_eq!(store.lsn_at("br", now() + 1).unwrap().unwrap().1, 900);
    }

    #[test]
    fn bases_newest_first() {
        let store = Store::open(&temp_db("bases")).unwrap();
        for (id, at) in [("b1", 10), ("b3", 30), ("b2", 20)] {
            store
                .insert_base(&BaseImage {
                    id: id.into(),
                    branch_id: "br".into(),
                    kind: "online".into(),
                    key: format!("k/{id}"),
                    timeline: 1,
                    start_lsn: 1,
                    end_lsn: 2,
                    flush_lsn: 2,
                    created_at: at,
                    bytes: 3,
                })
                .unwrap();
        }
        let ids: Vec<_> = store.bases("br").unwrap().into_iter().map(|b| b.id).collect();
        assert_eq!(ids, ["b3", "b2", "b1"]);
        store.delete_base("b2").unwrap();
        assert_eq!(store.bases("br").unwrap().len(), 2);
    }
}
