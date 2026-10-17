//! Control logic: projects, branches, endpoints, wake-on-connect and suspend.

use crate::compute::{DockerDriver, RunSpec, clone_dir, has_cluster};
use crate::config::Config;
use crate::history::Target;
use crate::ids::{new_id, new_password, now};
use crate::store::{Branch, Endpoint, EndpointState, LocalState, Project, READ_ONLY, READ_WRITE, Store};
use pg_proxy::{RouteError, Router};
use pg_storage::Objects;
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::task::JoinHandle;
use tracing::{info, warn};

#[derive(Debug, Error)]
pub enum ControlError {
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, ControlError>;

pub struct Control {
    pub config: Config,
    pub store: Store,
    pub driver: DockerDriver,
    pub objects: Objects,
    connections: Mutex<HashMap<String, usize>>,
    /// Serialises start/stop/branch operations per endpoint.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// WAL safekeepers of running primaries, by branch.
    pub(crate) receivers: Mutex<HashMap<String, JoinHandle<()>>>,
    /// Branches with a base backup in progress.
    pub(crate) backups: Mutex<HashSet<String>>,
}

pub const SUPPORTED_VERSIONS: [u32; 3] = [16, 17, 18];

const READY_TIMEOUT: Duration = Duration::from_secs(90);
/// Restores replay WAL before accepting connections.
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(1800);

impl Control {
    pub async fn new(mut config: Config) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(config.home.join("data"))?;
        if config.internal_token.is_empty() {
            let path = config.home.join("internal_token");
            config.internal_token = match std::fs::read_to_string(&path) {
                Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
                _ => {
                    let t = new_password();
                    std::fs::write(&path, &t)?;
                    t
                }
            };
        }
        let store = Store::open(&config.home.join("state.db"))?;
        let driver = DockerDriver::new(&config.home).await?;
        let objects = Objects::new(&config.storage)?;
        Ok(Arc::new(Self {
            config,
            store,
            driver,
            objects,
            connections: Mutex::default(),
            locks: Mutex::default(),
            receivers: Mutex::default(),
            backups: Mutex::default(),
        }))
    }

    pub(crate) fn lock(&self, endpoint_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks.lock().unwrap().entry(endpoint_id.to_string()).or_default().clone()
    }

    pub fn connections(&self, endpoint_id: &str) -> usize {
        self.connections.lock().unwrap().get(endpoint_id).copied().unwrap_or(0)
    }

    pub fn connection_uri(&self, project: &Project, endpoint_id: &str) -> String {
        format!(
            "postgres://roll:{}@{}:{}/main?options=endpoint%3D{endpoint_id}&sslmode=disable",
            project.password, self.config.public_host, self.config.public_port
        )
    }

    pub(crate) fn project_or_404(&self, id: &str) -> Result<Project> {
        self.store.project(id)?.ok_or_else(|| ControlError::NotFound(format!("project {id}")))
    }

    pub fn endpoint_or_404(&self, id: &str) -> Result<Endpoint> {
        self.store.endpoint(id)?.ok_or_else(|| ControlError::NotFound(format!("endpoint {id}")))
    }

    /// A branch of `project_id` by id or name.
    pub fn branch_in(&self, project_id: &str, key: &str) -> Result<Branch> {
        self.store
            .branch(key)?
            .filter(|b| b.project_id == project_id)
            .or(self.store.branch_by_name(project_id, key)?)
            .ok_or_else(|| ControlError::NotFound(format!("branch {key}")))
    }

    /// Current local data directory of a branch's primary.
    pub fn branch_path(&self, b: &Branch) -> std::path::PathBuf {
        self.config.data_path(b.dir_name())
    }

    /// Current local data directory of a replica.
    pub fn replica_path(&self, e: &Endpoint) -> std::path::PathBuf {
        self.config.replica_path(e.dir_name())
    }

    fn shared_buffers_mb(&self, ep: &Endpoint) -> u64 {
        self.config.mem_mb(ep.min_cu) / 4
    }

    // ── compute lifecycle ──

    /// Ensure the endpoint's compute is running and return its address.
    pub async fn ensure_running(self: &Arc<Self>, endpoint_id: &str) -> Result<SocketAddr> {
        let lock = self.lock(endpoint_id);
        let _held = lock.lock().await;
        let ep = self.endpoint_or_404(endpoint_id)?;
        if ep.state == EndpointState::Running
            && let Some(port) = ep.host_port
        {
            let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
            // Cheap liveness probe; a compute that died underneath us gets restarted below.
            let probe = tokio::time::timeout(Duration::from_millis(500), tokio::net::TcpStream::connect(addr));
            if matches!(probe.await, Ok(Ok(_))) {
                self.store.touch_endpoint(endpoint_id)?;
                return Ok(addr);
            }
            warn!(endpoint = endpoint_id, "compute marked running but unreachable; restarting");
            self.store.close_segment(endpoint_id)?;
        }
        let project = self.project_or_404(&ep.project_id)?;
        let branch = self.store.branch(&ep.branch_id)?.ok_or_else(|| ControlError::NotFound(ep.branch_id.clone()))?;
        let started = std::time::Instant::now();
        self.store.set_endpoint_state(endpoint_id, EndpointState::Starting, None)?;

        let result = async {
            let (data_dir, recover, slow) = if ep.is_primary() {
                let (dir, recover) = self.hydrate(&branch, &project).await?;
                (dir, recover, recover)
            } else {
                let mut dir = self.replica_path(&ep);
                let fresh = !has_cluster(&dir, project.pg_version);
                if fresh {
                    let primary = self
                        .store
                        .endpoint_for_branch(&branch.id)?
                        .ok_or_else(|| ControlError::NotFound("primary endpoint".into()))?;
                    let conninfo = self.primary_conninfo(&primary, &project, &ep.id);
                    let name = new_id(&ep.id);
                    let fresh_dir = self.config.replica_path(&name);
                    if let Err(e) = self.restore_into(&fresh_dir, &project, &branch.id, Target::Latest, Some(&conninfo)).await {
                        let _ = std::fs::remove_dir_all(&fresh_dir);
                        return Err(e);
                    }
                    self.store.set_endpoint_data_dir(&ep.id, &name)?;
                    let _ = std::fs::remove_dir_all(&dir);
                    dir = fresh_dir;
                }
                (dir, false, fresh)
            };
            let cu = ep.cu.clamp(ep.min_cu, ep.max_cu);
            let spec = RunSpec {
                endpoint_id,
                image: self.config.image(project.pg_version),
                pg_version: project.pg_version,
                data_dir: &data_dir,
                password: &project.password,
                cpus: cu,
                mem_mb: self.config.mem_mb(cu),
                shared_buffers_mb: self.shared_buffers_mb(&ep),
                cache_mb: self.config.mem_mb(ep.max_cu) * 3 / 4,
            };
            let port = self.driver.run(&spec, if slow { RECOVERY_TIMEOUT } else { READY_TIMEOUT }).await?;
            if recover {
                self.finish_recovery(port, &project.password).await?;
                self.store.set_pending_recovery(&branch.id, false)?;
            }
            if ep.is_primary() {
                self.store.set_local_state(&branch.id, LocalState::Local)?;
                self.start_receiver(&ep, port, &project.password);
            }
            Ok::<_, ControlError>((port, cu))
        }
        .await;

        match result {
            Ok((port, cu)) => {
                self.store.set_endpoint_state(endpoint_id, EndpointState::Running, Some(port))?;
                self.store.set_endpoint_cu(endpoint_id, cu)?;
                self.store.open_segment(endpoint_id, &ep.project_id, cu)?;
                self.store.touch_endpoint(endpoint_id)?;
                info!(endpoint = endpoint_id, ms = started.elapsed().as_millis() as u64, "compute started");
                Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            }
            Err(e) => {
                warn!(endpoint = endpoint_id, error = %e, "compute failed to start");
                let _ = self.driver.stop(endpoint_id).await;
                self.store.set_endpoint_state(endpoint_id, EndpointState::Suspended, None)?;
                Err(e)
            }
        }
    }

    pub async fn suspend(&self, endpoint_id: &str) -> Result<()> {
        let lock = self.lock(endpoint_id);
        let _held = lock.lock().await;
        let ep = self.endpoint_or_404(endpoint_id)?;
        self.driver.stop(endpoint_id).await?;
        if ep.is_primary() {
            // A clean shutdown streams its final WAL to the safekeeper before the socket closes.
            self.stop_receiver(&ep.branch_id).await;
        }
        self.store.set_endpoint_state(endpoint_id, EndpointState::Suspended, None)?;
        self.store.close_segment(endpoint_id)?;
        info!(endpoint = endpoint_id, "compute suspended");
        Ok(())
    }

    /// Live resize of a running compute (autoscaler and API): CPU and memory, no restart.
    pub async fn resize(&self, ep: &Endpoint, cu: f64) -> Result<()> {
        let lock = self.lock(&ep.id);
        let _held = lock.lock().await;
        let current = self.endpoint_or_404(&ep.id)?;
        if current.state == EndpointState::Running {
            if cu < current.cu
                && let Some(used) = self.driver.memory_in_use(&ep.id).await
                && used as f64 > self.config.mem_mb(cu) as f64 * 1024.0 * 1024.0 * 0.8
            {
                return Ok(()); // would squeeze the working set; stay at this size
            }
            self.driver.set_size(&ep.id, cu, self.config.mem_mb(cu)).await?;
            self.store.open_segment(&ep.id, &ep.project_id, cu)?;
        }
        self.store.set_endpoint_cu(&ep.id, cu)?;
        Ok(())
    }

    pub async fn update_endpoint(
        &self,
        endpoint_id: &str,
        min_cu: Option<f64>,
        max_cu: Option<f64>,
        suspend_timeout_s: Option<i64>,
    ) -> Result<Endpoint> {
        let ep = self.endpoint_or_404(endpoint_id)?;
        let min = min_cu.unwrap_or(ep.min_cu);
        let max = max_cu.unwrap_or(ep.max_cu);
        let timeout = suspend_timeout_s.unwrap_or(ep.suspend_timeout_s);
        if !(0.25..=64.0).contains(&min) || max < min || max > 64.0 {
            return Err(ControlError::Invalid("need 0.25 <= min_cu <= max_cu <= 64".into()));
        }
        if timeout < 0 {
            return Err(ControlError::Invalid("suspend_timeout_s must be >= 0 (0 = never)".into()));
        }
        self.store.update_endpoint_settings(endpoint_id, min, max, timeout)?;
        let updated = self.endpoint_or_404(endpoint_id)?;
        if (updated.cu - ep.cu).abs() > f64::EPSILON {
            self.resize(&ep, updated.cu).await?;
        }
        Ok(updated)
    }

    // ── projects & branches ──

    pub async fn create_project(self: &Arc<Self>, name: &str, pg_version: Option<u32>) -> Result<Project> {
        let name = name.trim();
        if name.is_empty() || name.len() > 64 {
            return Err(ControlError::Invalid("name must be 1-64 characters".into()));
        }
        let pg_version = pg_version.unwrap_or(self.config.default_pg_version);
        if !SUPPORTED_VERSIONS.contains(&pg_version) {
            return Err(ControlError::Invalid(format!("pg_version must be one of {SUPPORTED_VERSIONS:?}")));
        }
        let project = Project {
            id: new_id("prj"),
            name: name.into(),
            pg_version,
            region: "local".into(),
            password: new_password(),
            created_at: now(),
            history_retention_s: self.config.defaults.history_retention_s,
        };
        self.store.insert_project(&project)?;
        let branch = Branch {
            id: new_id("br"),
            project_id: project.id.clone(),
            name: "main".into(),
            parent_id: None,
            created_at: now(),
            local_state: LocalState::Local,
            pending_recovery: false,
            parent_at: None,
            data_dir: None,
        };
        self.store.insert_branch(&branch)?;
        let ep = self.insert_endpoint(&project.id, &branch.id, READ_WRITE)?;

        // Run initdb (and the first base backup) in the background so the first connection is fast.
        let this = self.clone();
        tokio::spawn(async move {
            let _ = this.ensure_running(&ep.id).await;
        });
        Ok(project)
    }

    pub async fn set_history_retention(&self, project_id: &str, seconds: i64) -> Result<Project> {
        if !(3600..=90 * 86_400).contains(&seconds) {
            return Err(ControlError::Invalid("history_retention_s must be between 1 hour and 90 days".into()));
        }
        self.project_or_404(project_id)?;
        self.store.set_history_retention(project_id, seconds)?;
        self.project_or_404(project_id)
    }

    fn insert_endpoint(&self, project_id: &str, branch_id: &str, kind: &str) -> Result<Endpoint> {
        let d = &self.config.defaults;
        let ep = Endpoint {
            id: new_id("ep"),
            project_id: project_id.into(),
            branch_id: branch_id.into(),
            kind: kind.into(),
            state: EndpointState::Suspended,
            min_cu: d.min_cu,
            max_cu: d.max_cu,
            cu: d.min_cu,
            suspend_timeout_s: d.suspend_timeout_s,
            host_port: None,
            last_active: now(),
            created_at: now(),
            data_dir: None,
        };
        self.store.insert_endpoint(&ep)?;
        Ok(ep)
    }

    /// Branch from `parent` (default `main`), either from its current state or — with `at` —
    /// from any point in its history. The parent never stops serving:
    /// - point in history, or parent evicted: restored from object storage
    /// - parent running: `pg_basebackup`
    /// - parent suspended: copy-on-write clone of its data directory
    pub async fn create_branch(
        &self,
        project_id: &str,
        name: &str,
        parent: Option<&str>,
        at: Option<Target>,
    ) -> Result<Branch> {
        let project = self.project_or_404(project_id)?;
        let name = name.trim();
        if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_/.".contains(c)) {
            return Err(ControlError::Invalid("branch name: 1-64 of [A-Za-z0-9-_/.]".into()));
        }
        if self.store.branch_by_name(project_id, name)?.is_some() {
            return Err(ControlError::Conflict(format!("branch {name} already exists")));
        }
        let parent = self.branch_in(project_id, parent.unwrap_or("main"))?;
        let parent_ep = self
            .store
            .endpoint_for_branch(&parent.id)?
            .ok_or_else(|| ControlError::NotFound("parent endpoint".into()))?;

        let mut branch = Branch {
            id: new_id("br"),
            project_id: project_id.into(),
            name: name.into(),
            parent_id: Some(parent.id.clone()),
            created_at: now(),
            local_state: LocalState::Local,
            pending_recovery: false,
            parent_at: match at {
                Some(Target::Time(t)) => Some(t),
                _ => None,
            },
            data_dir: None,
        };
        let dest = self.config.data_path(branch.dir_name());
        {
            let lock = self.lock(&parent_ep.id);
            let _held = lock.lock().await;
            let parent_ep = self.endpoint_or_404(&parent_ep.id)?;
            let parent_local = has_cluster(&self.branch_path(&parent), project.pg_version) && !parent.pending_recovery;
            let result = if at.is_some() || !parent_local {
                self.restore_into(&dest, &project, &parent.id, at.unwrap_or(Target::Latest), None)
                    .await
                    .map(|recover| branch.pending_recovery = recover)
            } else if parent_ep.state == EndpointState::Running {
                let image = self.config.image(project.pg_version);
                self.driver
                    .basebackup(&parent_ep.id, &image, project.pg_version, &project.password, &dest)
                    .await
                    .map_err(Into::into)
            } else {
                clone_dir(&self.branch_path(&parent), &dest).await.map_err(Into::into)
            };
            if let Err(e) = result {
                let _ = std::fs::remove_dir_all(&dest);
                return Err(e);
            }
        }
        self.store.insert_branch(&branch)?;
        self.insert_endpoint(project_id, &branch.id, READ_WRITE)?;
        Ok(branch)
    }

    /// Remove an endpoint's compute and local data.
    async fn teardown_endpoint(&self, ep: &Endpoint) -> Result<()> {
        self.driver.remove(&ep.id).await?;
        self.store.close_segment(&ep.id)?;
        if !ep.is_primary() {
            let _ = std::fs::remove_dir_all(self.replica_path(ep));
        }
        Ok(())
    }

    pub async fn delete_branch(&self, project_id: &str, branch_id: &str) -> Result<()> {
        let branch = self.branch_in(project_id, branch_id)?;
        if branch.parent_id.is_none() {
            return Err(ControlError::Invalid("the root branch can't be deleted; delete the project".into()));
        }
        self.purge_branch(&branch).await?;
        self.store.delete_branch(&branch.id)?;
        Ok(())
    }

    async fn purge_branch(&self, branch: &Branch) -> Result<()> {
        for ep in self.store.branch_endpoints(&branch.id)? {
            self.teardown_endpoint(&ep).await?;
        }
        self.stop_receiver(&branch.id).await;
        let _ = std::fs::remove_dir_all(self.branch_path(branch));
        let _ = std::fs::remove_dir_all(self.config.spool_dir(&branch.id));
        self.objects.delete_prefix(&pg_storage::objects::branch_prefix(&branch.id)).await?;
        Ok(())
    }

    pub async fn delete_project(&self, project_id: &str) -> Result<()> {
        self.project_or_404(project_id)?;
        for b in self.store.branches(project_id)? {
            self.purge_branch(&b).await?;
        }
        self.store.delete_project(project_id)?;
        Ok(())
    }

    // ── read replicas ──

    /// Add a read-only endpoint to a branch: a hot standby that scales (and suspends) on its own,
    /// keeps serving while the primary sleeps, and catches up from object storage.
    pub async fn create_replica(&self, project_id: &str, branch_key: &str) -> Result<Endpoint> {
        let branch = self.branch_in(project_id, branch_key)?;
        if self.store.bases(&branch.id)?.is_empty() {
            return Err(ControlError::Conflict(
                "this branch has no history in object storage yet; start its primary once and try again".into(),
            ));
        }
        self.insert_endpoint(project_id, &branch.id, READ_ONLY)
    }

    pub async fn delete_endpoint(&self, endpoint_id: &str) -> Result<()> {
        let ep = self.endpoint_or_404(endpoint_id)?;
        if ep.is_primary() {
            return Err(ControlError::Invalid("a branch's primary endpoint is deleted with the branch".into()));
        }
        let lock = self.lock(endpoint_id);
        let _held = lock.lock().await;
        self.teardown_endpoint(&ep).await?;
        self.store.delete_endpoint(endpoint_id)?;
        Ok(())
    }

    // ── SQL console ──

    /// Run SQL on an endpoint (dashboard SQL editor). Wakes the compute if needed.
    pub async fn query(self: &Arc<Self>, endpoint_id: &str, sql: &str) -> Result<Vec<QueryResult>> {
        let ep = self.endpoint_or_404(endpoint_id)?;
        let project = self.project_or_404(&ep.project_id)?;
        let addr = self.ensure_running(endpoint_id).await?;
        let conn_str = format!(
            "host={} port={} user=roll password={} dbname=main connect_timeout=10 application_name=roll-console",
            addr.ip(),
            addr.port(),
            project.password
        );
        let (client, connection) = tokio_postgres::connect(&conn_str, tokio_postgres::NoTls)
            .await
            .map_err(|e| ControlError::Internal(e.into()))?;
        self.connection_opened(endpoint_id);
        let handle = tokio::spawn(connection);
        let messages = client.simple_query(sql).await;
        drop(client);
        let _ = handle.await;
        self.connection_closed(endpoint_id);
        let messages = messages.map_err(|e| ControlError::Invalid(pg_error_text(&e)))?;

        let mut results = Vec::new();
        let mut current = QueryResult::default();
        for m in messages {
            match m {
                tokio_postgres::SimpleQueryMessage::RowDescription(cols) => {
                    current.columns = cols.iter().map(|c| c.name().to_string()).collect();
                }
                tokio_postgres::SimpleQueryMessage::Row(row) => {
                    if current.columns.is_empty() {
                        current.columns = row.columns().iter().map(|c| c.name().to_string()).collect();
                    }
                    current.rows.push((0..row.len()).map(|i| row.get(i).map(str::to_string)).collect());
                }
                tokio_postgres::SimpleQueryMessage::CommandComplete(n) => {
                    current.affected = n;
                    results.push(std::mem::take(&mut current));
                }
                _ => {}
            }
        }
        Ok(results)
    }

    pub(crate) fn connection_opened(&self, endpoint: &str) {
        *self.connections.lock().unwrap().entry(endpoint.to_string()).or_default() += 1;
        let _ = self.store.touch_endpoint(endpoint);
    }

    pub(crate) fn connection_closed(&self, endpoint: &str) {
        if let Some(n) = self.connections.lock().unwrap().get_mut(endpoint) {
            *n = n.saturating_sub(1);
        }
        let _ = self.store.touch_endpoint(endpoint);
    }
}

fn pg_error_text(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => format!("{}: {}", db.code().code(), db.message()),
        None => e.to_string(),
    }
}

#[derive(Default, serde::Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    pub affected: u64,
}

/// The proxy's view of the control plane. Wrapping the `Arc` lets `wake` start computes that
/// spawn background work (safekeepers, base backups).
pub struct ProxyRouter(pub Arc<Control>);

impl Router for ProxyRouter {
    async fn wake(&self, endpoint: &str) -> std::result::Result<SocketAddr, RouteError> {
        match self.0.ensure_running(endpoint).await {
            Ok(addr) => Ok(addr),
            Err(ControlError::NotFound(_)) => Err(RouteError::NotFound(format!("endpoint {endpoint} does not exist"))),
            Err(e) => Err(RouteError::Unavailable(format!("endpoint {endpoint} could not start: {e}"))),
        }
    }

    fn connection_opened(&self, endpoint: &str) {
        self.0.connection_opened(endpoint);
    }

    fn connection_closed(&self, endpoint: &str) {
        self.0.connection_closed(endpoint);
    }
}
