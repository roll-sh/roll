//! Control logic: projects, branches, endpoints, wake-on-connect and suspend.

use crate::compute::{DockerDriver, RunSpec, clone_dir, container_name, has_cluster};
use crate::config::Config;
use crate::history::Target;
use crate::ids::{new_id, new_password, now};
use crate::store::{Branch, ComputeAnswers, Endpoint, EndpointState, LocalState, Project, READ_ONLY, READ_WRITE, Store};
use pg_proxy::decoy::{Answers, Decoy};
use pg_proxy::{RouteError, Router};
use pg_storage::Objects;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
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
    /// How long each endpoint's compute took to start last from local data (a decoy's refusal
    /// waits as long).
    starts: Mutex<HashMap<String, Duration>>,
    /// Who hears of a decoy role's logins.
    decoy_listeners: Mutex<Vec<Arc<DecoyListener>>>,
    /// What each compute image answers before a password (decoy roles), by image reference.
    known: Mutex<HashMap<String, Known>>,
    /// Held while an image's answers are learned, by image reference.
    learning: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// A compute image's answers as learned from `image_id`: `None` when the decoy can't match them.
struct Known {
    image_id: String,
    answers: Option<Arc<Answers>>,
}

type DecoyListener = dyn Fn(&DecoyLogin) + Send + Sync;

/// A decoy role's login (roll secrets' Postgres decoys), refused by the proxy. Names and the
/// address, never what the client sent.
#[derive(Clone, Debug)]
pub struct DecoyLogin {
    pub project_id: String,
    pub endpoint_id: String,
    pub role: String,
    pub peer: Option<IpAddr>,
    /// Unix ms.
    pub at: i64,
}

/// A decoy role as made.
#[derive(Clone, Debug, serde::Serialize)]
pub struct DecoyRole {
    pub role: String,
    /// The project's root branch's primary, where the role was made.
    pub endpoint_id: String,
}

/// A compute's start when none was measured on this node yet (a decoy's refusal waits as long).
const START_DEFAULT: Duration = Duration::from_millis(1500);

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
        let mut known = HashMap::new();
        for a in store.compute_answers()? {
            // A row this version can't read is learned again.
            let answers = match a.answers.map(|j| serde_json::from_str::<Answers>(&j)) {
                Some(Ok(answers)) if answers.complete() => Some(Arc::new(answers)),
                Some(_) => continue,
                None => None,
            };
            known.insert(a.image, Known { image_id: a.image_id, answers });
        }
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
            starts: Mutex::default(),
            decoy_listeners: Mutex::default(),
            known: Mutex::new(known),
            learning: Mutex::default(),
        }))
    }

    pub(crate) fn lock(&self, endpoint_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks.lock().unwrap().entry(endpoint_id.to_string()).or_default().clone()
    }

    pub fn connections(&self, endpoint_id: &str) -> usize {
        self.connections.lock().unwrap().get(endpoint_id).copied().unwrap_or(0)
    }

    pub fn connection_uri(&self, project: &Project, endpoint_id: &str) -> String {
        match &self.config.public_domain {
            Some(domain) => format!("postgres://roll:{}@{endpoint_id}.{domain}:{}/main?sslmode=require", project.password, self.config.public_port),
            None => format!(
                "postgres://roll:{}@{}:{}/main?options=endpoint%3D{endpoint_id}&sslmode=disable",
                project.password, self.config.public_host, self.config.public_port
            ),
        }
    }

    /// The connection string of a branch's own role, when it has one (`roles.rs`).
    pub fn app_connection_uri(&self, branch: &Branch, endpoint_id: &str) -> Option<String> {
        let password = branch.app_password.as_ref()?;
        Some(match &self.config.public_domain {
            Some(domain) => format!("postgres://{}:{password}@{endpoint_id}.{domain}:{}/main?sslmode=require", branch.id, self.config.public_port),
            None => format!(
                "postgres://{}:{password}@{}:{}/main?options=endpoint%3D{endpoint_id}&sslmode=disable",
                branch.id, self.config.public_host, self.config.public_port
            ),
        })
    }

    /// Give a branch a role of its own (idempotent: the same password once it has one), made on
    /// the branch's compute; again after anything that replaces its data (a reset).
    pub async fn ensure_app_role(self: &Arc<Self>, project_id: &str, branch_id: &str) -> Result<Branch> {
        let branch = self.branch_in(project_id, branch_id)?;
        if branch.parent_id.is_none() {
            return Err(ControlError::Invalid("the root branch keeps the project's role; give a branch one".into()));
        }
        let password = match &branch.app_password {
            Some(p) => p.clone(),
            None => {
                let p = crate::ids::new_password();
                self.store.set_app_password(&branch.id, &p)?;
                p
            }
        };
        let ep = self.store.endpoint_for_branch(&branch.id)?.ok_or_else(|| ControlError::NotFound("primary endpoint".into()))?;
        self.query(&ep.id, &crate::roles::role_sql(&branch.id, &password), false).await?;
        self.branch_in(project_id, &branch.id)
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
        // A start from local data, as most are: not initdb's, a restore's or a new replica's.
        let ordinary = match ep.is_primary() {
            true => has_cluster(&self.branch_path(&branch), project.pg_version) && !branch.pending_recovery,
            false => has_cluster(&self.replica_path(&ep), project.pg_version),
        };
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
                if ordinary {
                    self.starts.lock().unwrap().insert(endpoint_id.to_string(), started.elapsed());
                }
                tokio::spawn(self.clone().check_answers(endpoint_id.to_string(), self.config.image(project.pg_version), project.pg_version, port));
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
            app_password: None,
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
        self.create_branch_with(project_id, name, parent, at, false).await
    }

    /// [`Control::create_branch`], and with `schema_only` emptied of every row before it exists
    /// ([`DockerDriver::empty_offline`]): nothing ever serves or records the parent's rows.
    pub async fn create_branch_with(
        &self,
        project_id: &str,
        name: &str,
        parent: Option<&str>,
        at: Option<Target>,
        schema_only: bool,
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
            app_password: None,
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
                self.online_copy(&project, &parent, &parent_ep.id, &dest).await
            } else {
                clone_dir(&self.branch_path(&parent), &dest).await.map_err(Into::into)
            };
            if let Err(e) = result {
                let _ = std::fs::remove_dir_all(&dest);
                return Err(e);
            }
        }
        if schema_only {
            if branch.pending_recovery {
                let _ = std::fs::remove_dir_all(&dest);
                return Err(ControlError::Invalid("a structure-only branch can't be taken from a point in time".into()));
            }
            if let Err(e) = self.driver.empty_offline(&dest, project.pg_version, &self.config.image(project.pg_version)).await {
                let _ = std::fs::remove_dir_all(&dest);
                return Err(e.into());
            }
        }
        self.store.insert_branch(&branch)?;
        self.insert_endpoint(project_id, &branch.id, READ_WRITE)?;
        Ok(branch)
    }

    /// A running parent's data into `dest`: a file-system copy under the backup API (shares
    /// blocks on XFS and btrfs), else `pg_basebackup`.
    pub(crate) async fn online_copy(&self, project: &Project, parent: &Branch, parent_ep: &str, dest: &std::path::Path) -> Result<()> {
        match self.driver.online_clone(parent_ep, &project.password, &self.branch_path(parent), dest, project.pg_version).await {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, endpoint = parent_ep, "file-system branch copy failed; using pg_basebackup");
                let _ = std::fs::remove_dir_all(dest);
                let image = self.config.image(project.pg_version);
                self.driver.basebackup(parent_ep, &image, project.pg_version, &project.password, dest).await.map_err(Into::into)
            }
        }
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

    /// Run SQL on an endpoint (dashboard SQL editor, MCP). Wakes the compute if needed.
    ///
    /// `read_only`: the SQL runs as a login role made for this one query, holding nothing but
    /// `pg_read_all_data` (with `default_transaction_read_only` and a statement timeout), dropped
    /// with anything it made when the query ends, even if the caller goes away. Postgres itself
    /// refuses its writes, whatever the SQL does (`SET`, `COMMIT; …`, procedures). A standby can't
    /// make the role, and its only user is the superuser: read-only SQL runs on the primary.
    pub async fn query(self: &Arc<Self>, endpoint_id: &str, sql: &str, read_only: bool) -> Result<Vec<QueryResult>> {
        let ep = self.endpoint_or_404(endpoint_id)?;
        let project = self.project_or_404(&ep.project_id)?;
        let addr = self.ensure_running(endpoint_id).await?;
        self.connection_opened(endpoint_id);
        // Detached: the role made for a read-only query is dropped even if this request isn't
        // awaited to the end.
        let (sql, password) = (sql.to_string(), project.password.clone());
        let out = tokio::spawn(async move { run_sql(addr, &password, &sql, read_only).await }).await;
        self.connection_closed(endpoint_id);
        let out = out.map_err(|e| ControlError::Internal(anyhow::anyhow!("the query task failed: {e}")))?;
        let messages = out?;

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

    // ── decoy roles ──

    /// Make `role` a decoy of the project (roll secrets' Postgres decoys): from here on the proxy
    /// refuses its logins on every endpoint of the project, as Postgres refuses a wrong password,
    /// without waking a compute, and tells [`Control::on_decoy_login`]'s listeners. It's also
    /// made `NOLOGIN` (with no password) on the root branch, for any way in around the proxy.
    /// The name is the caller's, so it looks like the roles it makes; one that exists is refused,
    /// and so is any on an image whose answers the decoy can't match.
    pub async fn create_decoy_role(self: &Arc<Self>, project_id: &str, role: &str) -> Result<DecoyRole> {
        let pg_version = self.project_or_404(project_id)?.pg_version;
        if !decoy_name_ok(role) {
            return Err(ControlError::Invalid("a decoy role's name is 1-62 of [a-z0-9_], not starting with a digit or pg_".into()));
        }
        if self.store.is_decoy_role(project_id, role)? {
            return Err(ControlError::Conflict(format!("{role} is a decoy already")));
        }
        let ep = self.root_endpoint(project_id)?;
        // Its compute is up for the role anyway: what its image answers is known before any login.
        let port = self.ensure_running(&ep.id).await?.port();
        if self.answers(&self.config.image(pg_version), pg_version, Some((&ep.id, port))).await.is_none() {
            return Err(ControlError::Invalid("roll can't answer a decoy role's logins as this database's Postgres image does, so one here could be told from a missing role".into()));
        }
        // First in Postgres: this fails for a role that exists, which must never become a decoy.
        self.query(&ep.id, &format!("CREATE ROLE {role} NOLOGIN"), false).await?;
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt).map_err(|e| anyhow::anyhow!("os randomness: {e}"))?;
        use base64::Engine as _;
        if let Err(e) = self.store.insert_decoy_role(project_id, role, &base64::engine::general_purpose::STANDARD.encode(salt)) {
            let _ = self.query(&ep.id, &format!("DROP ROLE IF EXISTS {role}"), false).await;
            return Err(e.into());
        }
        Ok(DecoyRole { role: role.to_string(), endpoint_id: ep.id })
    }

    /// Drop a decoy role: from Postgres (with anything granted to it), then from the proxy's
    /// table. Done already (or never a decoy) is fine.
    pub async fn drop_decoy_role(self: &Arc<Self>, project_id: &str, role: &str) -> Result<()> {
        self.project_or_404(project_id)?;
        if !self.store.is_decoy_role(project_id, role)? {
            return Ok(());
        }
        // Only names `create_decoy_role` took are in the table.
        let ep = self.root_endpoint(project_id)?;
        let sql = format!("DO $$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role}') THEN EXECUTE 'DROP OWNED BY {role}'; EXECUTE 'DROP ROLE {role}'; END IF; END $$;");
        self.query(&ep.id, &sql, false).await?;
        self.store.delete_decoy_role(project_id, role)?;
        Ok(())
    }

    /// Hear of every decoy role's login this node's proxy refuses (called on the login's task:
    /// hand anything slow off).
    pub fn on_decoy_login(&self, f: impl Fn(&DecoyLogin) + Send + Sync + 'static) {
        self.decoy_listeners.lock().unwrap().push(Arc::new(f));
    }

    fn tell_decoy_login(&self, login: &DecoyLogin) {
        let listeners = self.decoy_listeners.lock().unwrap().clone();
        for f in listeners {
            f(login);
        }
    }

    // ── what computes answer before a password (decoy roles) ──

    /// What `image` answers before a password is checked, learned first when it isn't known:
    /// from the endpoint's compute when it's up (`running`: its id and host port), else from a
    /// compute started for that alone (a decoy's login never wakes an endpoint). `None`: the
    /// decoy can't answer as it does (or it couldn't be asked).
    async fn answers(&self, image: &str, pg_version: u32, running: Option<(&str, u16)>) -> Option<Arc<Answers>> {
        if let Some(k) = self.known.lock().unwrap().get(image) {
            return k.answers.clone();
        }
        let lock = self.learning.lock().unwrap().entry(image.to_string()).or_default().clone();
        let _held = lock.lock().await;
        if let Some(k) = self.known.lock().unwrap().get(image) {
            return k.answers.clone();
        }
        let learned = match running {
            Some((endpoint, port)) => match self.driver.image_id(&container_name(endpoint)).await {
                Ok(id) => self.learn_at(image, &id, pg_version, port).await,
                Err(e) => Err(e),
            },
            None => self.learn_alone(image, pg_version).await,
        };
        learned.unwrap_or_else(|e| {
            warn!(image, error = %e, "learning what a compute image answers failed");
            None
        })
    }

    /// Forget every image's answers: each is learned again when it's next needed (as after an
    /// upgrade that moves the pinned images).
    pub fn forget_compute_answers(&self) -> anyhow::Result<()> {
        self.store.delete_compute_answers()?;
        self.known.lock().unwrap().clear();
        Ok(())
    }

    /// After `endpoint`'s compute started on `image`: learn what it answers, unless that's known
    /// for the image it runs (a reference by digest can't move; a tag can).
    async fn check_answers(self: Arc<Self>, endpoint: String, image: String, pg_version: u32, port: u16) {
        if image.contains("@sha256:") && self.known.lock().unwrap().contains_key(&image) {
            return;
        }
        let lock = self.learning.lock().unwrap().entry(image.clone()).or_default().clone();
        let _held = lock.lock().await;
        let learned = async {
            let id = self.driver.image_id(&container_name(&endpoint)).await?;
            if self.known.lock().unwrap().get(&image).is_some_and(|k| k.image_id == id) {
                return Ok(None);
            }
            self.learn_at(&image, &id, pg_version, port).await
        };
        if let Err(e) = learned.await {
            warn!(endpoint, image, error = %e, "learning what a compute image answers failed");
        }
    }

    /// Ask the compute at `port` (running `image`, whose id is `image_id`) and keep its answers.
    async fn learn_at(&self, image: &str, image_id: &str, pg_version: u32, port: u16) -> anyhow::Result<Option<Arc<Answers>>> {
        let answers = pg_proxy::decoy::learn::learn(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await?;
        let json = answers.as_ref().map(serde_json::to_string).transpose()?;
        self.store.put_compute_answers(&ComputeAnswers { image: image.into(), image_id: image_id.into(), answers: json }, pg_version)?;
        match answers {
            Some(_) => info!(image, "learned what a compute image answers before a password"),
            None => warn!(image, "a decoy can't answer as this compute image does: decoy roles' logins on it go to Postgres"),
        }
        let answers = answers.map(Arc::new);
        self.known.lock().unwrap().insert(image.into(), Known { image_id: image_id.into(), answers: answers.clone() });
        Ok(answers)
    }

    /// Learn `image`'s answers from a compute of an empty cluster, started for that alone.
    async fn learn_alone(&self, image: &str, pg_version: u32) -> anyhow::Result<Option<Arc<Answers>>> {
        let id = new_id("learn");
        let dir = self.config.home.join("learn").join(&id);
        let password = new_password();
        let spec = RunSpec { endpoint_id: &id, image: image.into(), pg_version, data_dir: &dir, password: &password, cpus: 0.5, mem_mb: 256, shared_buffers_mb: 32, cache_mb: 64 };
        let learned = async {
            let port = self.driver.run(&spec, READY_TIMEOUT).await?;
            let image_id = self.driver.image_id(&container_name(&id)).await?;
            self.learn_at(image, &image_id, pg_version, port).await
        }
        .await;
        let _ = self.driver.remove(&id).await;
        let _ = std::fs::remove_dir_all(&dir);
        learned
    }

    /// The primary endpoint of the project's root branch.
    fn root_endpoint(&self, project_id: &str) -> Result<Endpoint> {
        let root = self.store.branches(project_id)?.into_iter().find(|b| b.parent_id.is_none()).ok_or_else(|| ControlError::NotFound("root branch".into()))?;
        self.store.endpoint_for_branch(&root.id)?.ok_or_else(|| ControlError::NotFound("primary endpoint".into()))
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

/// A name `CREATE ROLE` takes unquoted, that Postgres won't cut (63 bytes) and doesn't reserve.
fn decoy_name_ok(role: &str) -> bool {
    (1..=62).contains(&role.len())
        && role.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && !role.as_bytes()[0].is_ascii_digit()
        && !role.starts_with("pg_")
}

/// One query's messages: as the owner, or (`read_only`) as a role made for it and dropped after.
async fn run_sql(addr: SocketAddr, password: &str, sql: &str, read_only: bool) -> Result<Vec<tokio_postgres::SimpleQueryMessage>> {
    let conn = |user: &str, password: &str| {
        format!("host={} port={} user={user} password={password} dbname=main connect_timeout=10 application_name=roll-console", addr.ip(), addr.port())
    };
    let connect = |info: String| async move {
        let (client, connection) = tokio_postgres::connect(&info, tokio_postgres::NoTls).await.map_err(|e| ControlError::Internal(e.into()))?;
        Ok::<_, ControlError>((client, tokio::spawn(connection)))
    };
    let (admin, admin_conn) = connect(conn("roll", password)).await?;
    if !read_only {
        let messages = admin.simple_query(sql).await;
        drop(admin);
        let _ = admin_conn.await;
        return messages.map_err(|e| ControlError::Invalid(pg_error_text(&e)));
    }
    if admin.query_one("SELECT pg_is_in_recovery()", &[]).await.map(|r| r.get::<_, bool>(0)).unwrap_or(true) {
        drop(admin);
        let _ = admin_conn.await;
        return Err(ControlError::Invalid("read-only SQL runs on a branch's primary endpoint, not a read replica".into()));
    }
    let role = format!("roll_ro_{}", crate::ids::random_string(16));
    let pw = crate::ids::random_string(32);
    admin
        .batch_execute(&format!(
            "CREATE ROLE {role} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS CONNECTION LIMIT 1 \
             PASSWORD '{pw}' VALID UNTIL '{until}' IN ROLE pg_read_all_data; \
             ALTER ROLE {role} SET default_transaction_read_only = on; \
             ALTER ROLE {role} SET statement_timeout = '60s'",
            until = chrono_like_in(600)
        ))
        .await
        .map_err(|e| ControlError::Internal(anyhow::anyhow!("making the read-only role: {}", pg_error_text(&e))))?;
    let messages = match connect(conn(&role, &pw)).await {
        Ok((reader, reader_conn)) => {
            let m = reader.simple_query(sql).await;
            drop(reader);
            let _ = reader_conn.await;
            m.map_err(|e| ControlError::Invalid(pg_error_text(&e)))
        }
        Err(e) => Err(e),
    };
    // Whatever the role made (a temporary table goes with its session) goes with it.
    if let Err(e) = admin.batch_execute(&format!("DROP OWNED BY {role}; DROP ROLE {role}")).await {
        tracing::warn!(role, error = %pg_error_text(&e), "dropping a read-only query role");
    }
    drop(admin);
    let _ = admin_conn.await;
    messages
}

/// `secs` from now as a Postgres timestamp literal in UTC (`2026-11-12 19:05:30+00`).
fn chrono_like_in(secs: u64) -> String {
    let unix = (std::time::SystemTime::now() + std::time::Duration::from_secs(secs)).duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (days, rem) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}+00", rem / 3600, rem % 3600 / 60, rem % 60)
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

    async fn decoy(&self, endpoint: &str, user: &str, peer: Option<SocketAddr>) -> Option<Decoy> {
        let found = match self.0.store.decoy_at(endpoint, user) {
            Ok(found) => found?,
            Err(e) => {
                // Postgres refuses it anyway (`NOLOGIN`, no password): only the alert is lost.
                warn!(endpoint, error = %e, "looking up a decoy role failed");
                return None;
            }
        };
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        self.0.tell_decoy_login(&DecoyLogin { project_id: found.project_id, endpoint_id: endpoint.to_string(), role: user.to_string(), peer: peer.map(|p| p.ip()), at });
        let asked = std::time::Instant::now();
        let answers = self.0.answers(&self.0.config.image(found.pg_version), found.pg_version, found.running.map(|port| (endpoint, port))).await;
        let start = match found.running {
            Some(_) => Duration::ZERO,
            None => self.0.starts.lock().unwrap().get(endpoint).copied().unwrap_or(START_DEFAULT),
        };
        // Learning them (once per image) counts toward the wait.
        Some(Decoy { salt: found.salt, start: start.saturating_sub(asked.elapsed()), answers })
    }

    fn connection_opened(&self, endpoint: &str) {
        self.0.connection_opened(endpoint);
    }

    fn connection_closed(&self, endpoint: &str) {
        self.0.connection_closed(endpoint);
    }
}
