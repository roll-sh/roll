//! The reconciler (docs/previews.md, "Converging"): every pass discovers which previews should
//! exist from roll code's open changes, then converges each unsettled preview to its desired
//! state. Every step looks before it makes, so a pass after a crash carries on where the last
//! one stopped.

use crate::ports::{Apps, Brancher, ChangeInfo, EnvSource, Secrets, Source, Target};
use crate::settings::{Settings, is_production};
use crate::store::{New, ResourceRow, Row, Store};
use crate::{Desired, Error, Kind, Mode, Preview, ResourceState, Result, ServiceState, State, branch_name};
use async_trait::async_trait;
use parking_lot::Mutex;
use roll_workspace::Workspace;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{info, warn};

/// The lease of the node that runs the reconciler and serves preview hosts in a cluster
/// (`Cluster::singleton("previews")`).
pub const LEADER_KEY: &str = "singleton:previews";

/// Settled previews aren't converged again until something changes.
const SETTLED: i64 = i64::MAX;
/// The first retry after a failed step, doubling up to [`MAX_BACKOFF_MS`].
const BACKOFF_MS: i64 = 15_000;
const MAX_BACKOFF_MS: i64 = 600_000;
/// Previews converged at once.
const PARALLEL: usize = 4;

#[derive(Clone, Debug)]
pub struct Config {
    /// Services answer on `<label>.<domain>`.
    pub domain: String,
    pub scheme: String,
    pub port: Option<u16>,
    /// A pass at least this often.
    pub interval: Duration,
    /// How often branches' sizes and costs are read again.
    pub usage_every: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            domain: "preview.localhost".into(),
            scheme: "http".into(),
            port: None,
            interval: Duration::from_secs(15),
            usage_every: Duration::from_secs(300),
        }
    }
}

impl Config {
    pub fn url(&self, host: &str) -> String {
        let port = match (self.scheme.as_str(), self.port) {
            ("http", Some(80)) | ("https", Some(443)) | (_, None) => String::new(),
            (_, Some(p)) => format!(":{p}"),
        };
        format!("{}://{host}.{}{port}", self.scheme, self.domain)
    }
}

pub struct Engine {
    pub cfg: Config,
    pub store: Arc<Store>,
    workspace: Arc<Workspace>,
    source: Arc<dyn Source>,
    branchers: HashMap<Kind, Arc<dyn Brancher>>,
    apps: Arc<dyn Apps>,
    secrets: Arc<dyn Secrets>,
    /// Whether this node runs the reconciler (`singleton("previews")` in a cluster).
    leader: Box<dyn Fn() -> bool + Send + Sync>,
    kick: Notify,
    usage_read: Mutex<HashMap<String, i64>>,
    /// One pass at a time.
    passing: tokio::sync::Mutex<()>,
}

/// The variables a branch of `kind` is reached by (the first of a kind takes the plain names).
fn suffixed(vars: Vec<(String, String)>, suffix: Option<&str>) -> Vec<(String, String)> {
    match suffix {
        None => vars,
        Some(s) => vars.into_iter().map(|(k, v)| (format!("{k}_{s}"), v)).collect(),
    }
}

fn set(env: &mut Vec<(String, String)>, k: &str, v: String) {
    env.retain(|(name, _)| name != k);
    env.push((k.to_string(), v));
}

fn env_suffix(parent: &str) -> String {
    let s: String = parent.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    s.trim_matches('_').to_string()
}

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Config,
        store: Arc<Store>,
        workspace: Arc<Workspace>,
        source: Arc<dyn Source>,
        branchers: Vec<Arc<dyn Brancher>>,
        apps: Arc<dyn Apps>,
        secrets: Arc<dyn Secrets>,
        leader: Box<dyn Fn() -> bool + Send + Sync>,
    ) -> Arc<Engine> {
        Arc::new(Engine {
            cfg,
            store,
            workspace,
            source,
            branchers: branchers.into_iter().map(|b| (b.kind(), b)).collect(),
            apps,
            secrets,
            leader,
            kick: Notify::new(),
            usage_read: Mutex::new(HashMap::new()),
            passing: tokio::sync::Mutex::new(()),
        })
    }

    /// Run passes until the engine is dropped: every `interval`, and as soon as something is
    /// asked of it or roll code nudges.
    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::downgrade(self);
        let mut nudges = self.source.nudges();
        tokio::spawn(async move {
            loop {
                let Some(e) = this.upgrade() else { return };
                if (e.leader)()
                    && let Err(err) = e.pass().await
                {
                    warn!(error = %err, "previews: a pass failed");
                }
                let interval = e.cfg.interval;
                let kicked = async {
                    match nudges.as_mut() {
                        Some(n) => {
                            let _ = n.recv().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    _ = e.kick.notified() => {}
                    _ = kicked => {}
                    _ = tokio::time::sleep(interval) => {}
                }
                drop(e);
            }
        })
    }

    /// Converge soon.
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    /// One pass: discover, then converge every preview that's due.
    pub async fn pass(self: &Arc<Self>) -> Result<()> {
        let _one = self.passing.lock().await;
        for (project, settings) in self.store.enabled_projects()? {
            if let Err(e) = self.discover(&project, &settings).await {
                warn!(%project, error = %e, "previews: discovering");
            }
        }
        let now = self.store.now_ms()?;
        let due: Vec<Row> = self.store.rows(None)?.into_iter().filter(|r| r.next_at <= now).collect();
        let sem = Arc::new(tokio::sync::Semaphore::new(PARALLEL));
        let mut set = tokio::task::JoinSet::new();
        for r in due {
            let this = self.clone();
            let permit = sem.clone().acquire_owned().await.map_err(|e| Error::Internal(e.to_string()))?;
            set.spawn(async move {
                let id = r.id.clone();
                this.converge_one(r).await;
                drop(permit);
                id
            });
        }
        while set.join_next().await.is_some() {}
        Ok(())
    }

    /// The previews a project should have, from its repos' open changes and its rules.
    async fn discover(&self, project_id: &str, s: &Settings) -> anyhow::Result<()> {
        let project = match self.workspace.get(project_id) {
            Ok(p) => p,
            Err(roll_workspace::Error::NotFound(_)) => {
                // The project is gone: so are its previews (their branches go with them).
                for r in self.store.rows(Some(project_id))? {
                    self.mark_deleted(&r.id, "the project was deleted")?;
                }
                self.store.delete_settings(project_id)?;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        let repos: Vec<String> = project.resources.iter().filter(|r| r.kind == roll_workspace::Kind::Repo).map(|r| r.r#ref.clone()).collect();
        let now = self.store.now_ms()?;
        let mut open: HashMap<String, ChangeInfo> = HashMap::new();
        let mut heads: HashMap<String, Option<String>> = HashMap::new();
        for repo in &repos {
            for c in self.source.open_changes(repo).await? {
                open.insert(c.id.clone(), c);
            }
            heads.insert(repo.clone(), self.source.head(repo, None).await?);
        }
        let existing = self.store.rows(Some(project_id))?;
        // Changes that match the rules and have no preview yet.
        for c in open.values() {
            if !wants(s, c) {
                continue;
            }
            if existing.iter().any(|r| r.change_id.as_deref() == Some(&c.id)) {
                continue;
            }
            let key = match c.github_pr {
                Some(n) => format!("pr{n}"),
                None => format!("c{}", c.number),
            };
            let state = if c.may_run() { State::Creating } else { State::AwaitingApproval };
            let (r, made) = self.store.create(&New {
                project_id: project_id.to_string(),
                key,
                project_name: project.name.clone(),
                repo: c.repo.clone(),
                change_id: Some(c.id.clone()),
                branch: c.branch.clone(),
                number: Some(c.number),
                title: c.title.clone(),
                github_pr: c.github_pr,
                head: c.head.clone(),
                trusted: c.trusted,
                state,
                expires_at: None,
                created_by: "roll".into(),
            })?;
            if made {
                info!(preview = %r.id, change = %c.id, "previews: new preview");
            }
        }
        // What changed for the previews this project has.
        for r in self.store.rows(Some(project_id))? {
            if r.desired == Desired::Deleted {
                continue;
            }
            let ttl_ms = s.ttl_hours as i64 * 3_600_000;
            if let Some(change) = &r.change_id {
                match open.get(change) {
                    Some(c) if wants(s, c) => {
                        if c.head != r.head || c.title != r.title || c.trusted != r.trusted || c.github_pr != r.github_pr {
                            let moved = c.head != r.head;
                            let (head, title, trusted, pr) = (c.head.clone(), c.title.clone(), c.trusted, c.github_pr);
                            self.store.update(&r.id, |row| {
                                row.head = head.clone();
                                row.title = title.clone();
                                row.trusted = trusted;
                                row.github_pr = pr;
                                if moved {
                                    row.last_active_at = now;
                                    row.next_at = 0;
                                    row.attempts = 0;
                                    if matches!(row.state, State::Ready | State::Sleeping | State::Failed) {
                                        row.state = State::Updating;
                                    }
                                }
                            })?;
                            if moved {
                                self.store.log(&r.id, &format!("pushed: {}", short(&c.head)))?;
                            }
                        }
                        // An approval (or its loss on a new head) is a change of state.
                        if c.may_run() == (r.state == State::AwaitingApproval) {
                            self.store.update(&r.id, |row| row.next_at = 0)?;
                        }
                    }
                    Some(_) => {
                        self.mark_deleted(&r.id, "the change no longer matches the project's preview rules")?;
                        continue;
                    }
                    None => {
                        let why = match self.source.change(change).await? {
                            Some(c) if c.open => None,
                            Some(_) => Some("the change landed or closed"),
                            None => Some("the change was deleted"),
                        };
                        if let Some(why) = why {
                            self.mark_deleted(&r.id, why)?;
                            continue;
                        }
                    }
                }
            } else if let Some(branch) = &r.branch {
                // A branch preview follows its branch until the branch goes.
                match self.source.head(&r.repo, Some(branch)).await? {
                    None => {
                        self.mark_deleted(&r.id, "the branch was deleted")?;
                        continue;
                    }
                    Some(h) if h != r.head => {
                        self.store.update(&r.id, |row| {
                            row.head = h.clone();
                            row.last_active_at = now;
                            row.next_at = 0;
                            if matches!(row.state, State::Ready | State::Sleeping | State::Failed) {
                                row.state = State::Updating;
                            }
                        })?;
                        self.store.log(&r.id, &format!("pushed: {}", short(&h)))?;
                    }
                    Some(_) => {}
                }
            }
            if r.expires_at.is_some_and(|t| t <= now) || r.last_active_at + ttl_ms <= now {
                self.mark_deleted(&r.id, &format!("unused for {} hours (ttl)", s.ttl_hours))?;
                continue;
            }
            // A service from another repo of the project follows that repo's default branch.
            let stale = self.store.services(&r.id)?.into_iter().any(|svc| {
                svc.repo != r.repo && heads.get(&svc.repo).is_some_and(|h| h.is_some() && h.as_deref() != svc.revision.as_deref())
            });
            // A reset that's due, or the usage reading that's due, makes it converge too.
            let reset_due = s.reset_every_hours > 0 && r.last_reset_at.unwrap_or(r.created_at) + s.reset_every_hours as i64 * 3_600_000 <= now;
            let usage_due = self.usage_read.lock().get(&r.id).is_none_or(|t| *t + self.cfg.usage_every.as_millis() as i64 <= now);
            if (stale || reset_due || usage_due) && r.next_at == SETTLED {
                self.store.update(&r.id, |row| row.next_at = 0)?;
            }
        }
        // Slots: the oldest queued previews start as others go.
        let rows = self.store.rows(Some(project_id))?;
        let busy = rows.iter().filter(|r| r.desired == Desired::Active && r.state != State::Queued).count();
        let mut free = (s.max_previews as usize).saturating_sub(busy);
        let mut queued: Vec<&Row> = rows.iter().filter(|r| r.desired == Desired::Active && r.state == State::Queued).collect();
        queued.sort_by_key(|r| r.created_at);
        for r in queued {
            if free == 0 {
                break;
            }
            free -= 1;
            self.store.update(&r.id, |row| {
                row.state = State::Creating;
                row.next_at = 0;
            })?;
        }
        // New previews beyond the limit wait.
        let rows = self.store.rows(Some(project_id))?;
        let mut active: Vec<&Row> = rows.iter().filter(|r| r.desired == Desired::Active && r.state != State::Queued).collect();
        active.sort_by_key(|r| r.created_at);
        for r in active.iter().skip(s.max_previews as usize) {
            if r.state == State::Creating && self.store.resources(&r.id)?.iter().all(|x| x.r#ref.is_none()) {
                self.store.update(&r.id, |row| row.state = State::Queued)?;
                self.store.log(&r.id, &format!("waiting: the project has {} previews (max_previews)", s.max_previews))?;
            }
        }
        Ok(())
    }

    pub fn mark_deleted(&self, id: &str, why: &str) -> Result<Row> {
        let r = self.store.update(id, |row| {
            row.desired = Desired::Deleted;
            row.next_at = 0;
            row.attempts = 0;
        })?;
        self.store.log(id, &format!("deleting: {why}"))?;
        self.kick();
        Ok(r)
    }

    async fn converge_one(&self, r: Row) {
        let id = r.id.clone();
        let deleting = r.desired == Desired::Deleted;
        let result = if deleting { self.teardown(&r).await } else { self.converge(&r).await };
        match result {
            Ok(()) => {}
            Err(e) => {
                let msg = format!("{e:#}");
                warn!(preview = %id, error = %msg, "previews: converging");
                let now = self.store.now_ms().unwrap_or_default();
                let updated = self.store.update(&id, |row| {
                    row.attempts += 1;
                    row.next_at = now + (BACKOFF_MS << (row.attempts - 1).clamp(0, 10)).min(MAX_BACKOFF_MS);
                    row.error = Some(msg.chars().take(4000).collect());
                    if !deleting {
                        row.state = State::Failed;
                    }
                });
                if let Ok(row) = updated {
                    let _ = self.store.log(&id, &format!("failed: {}", msg.lines().next().unwrap_or("")));
                    if row.attempts == 1 {
                        self.report(row).await;
                    }
                }
            }
        }
    }

    fn brancher(&self, kind: Kind) -> anyhow::Result<&Arc<dyn Brancher>> {
        self.branchers.get(&kind).ok_or_else(|| anyhow::anyhow!("roll {} isn't on this node, so it can't be branched", kind.as_str()))
    }

    /// Bring one active preview to its desired state.
    async fn converge(&self, r: &Row) -> anyhow::Result<()> {
        let s = self.store.settings(&r.project_id)?;
        // A fork's head waits for a writer's approval; nothing of it runs meanwhile.
        if let Some(change) = &r.change_id {
            let Some(c) = self.source.change(change).await? else {
                self.mark_deleted(&r.id, "the change was deleted")?;
                return Ok(());
            };
            if !c.may_run() {
                for svc in self.store.services(&r.id)? {
                    self.apps.sleep(&r.id, &svc.name).await;
                    self.store.set_service(&r.id, &svc.name, ServiceState::Sleeping, None, None)?;
                }
                let row = self.store.update(&r.id, |row| {
                    row.state = State::AwaitingApproval;
                    row.next_at = SETTLED;
                    row.attempts = 0;
                    row.error = None;
                })?;
                self.report(row).await;
                return Ok(());
            }
        }
        if r.state == State::Queued {
            self.store.update(&r.id, |row| row.next_at = SETTLED)?;
            return Ok(());
        }
        if r.state == State::AwaitingApproval {
            self.store.update(&r.id, |row| row.state = State::Creating)?;
            self.store.log(&r.id, "approved: starting")?;
        }
        let project = self.workspace.get(&r.project_id)?;
        let name = branch_name(&r.id);

        // 1. Data: a branch of every data resource of the project the rules don't skip.
        let wanted: Vec<(Kind, String, Mode)> = project
            .resources
            .iter()
            .filter_map(|res| Kind::of_workspace(res.kind).map(|k| (k, res.r#ref.clone())))
            .filter_map(|(k, parent)| s.mode_of(k, &parent).map(|m| (k, parent, m)))
            .collect();
        for gone in self.store.want_resources(&r.id, &wanted)? {
            self.delete_branch(&r.id, &gone, &name).await?;
            self.store.drop_resource(&r.id, gone.kind, &gone.parent)?;
        }
        let now = self.store.now_ms()?;
        let reset_due = s.reset_every_hours > 0 && r.last_reset_at.unwrap_or(r.created_at) + s.reset_every_hours as i64 * 3_600_000 <= now;
        let asked: Vec<String> = r.reset_requested.clone().unwrap_or_default();
        let mut reset_any = false;
        let mut problems: Vec<String> = Vec::new();
        for res in self.store.resources(&r.id)? {
            let key = format!("{}:{}", res.kind.as_str(), res.parent);
            let b = match self.brancher(res.kind) {
                Ok(b) => b.clone(),
                Err(e) => {
                    let msg = e.to_string();
                    self.store.set_resource(&r.id, res.kind, &res.parent, |x| {
                        x.state = ResourceState::Failed;
                        x.error = Some(msg.clone());
                    })?;
                    problems.push(msg);
                    continue;
                }
            };
            match &res.r#ref {
                None => {
                    let made = b.branch(&res.parent, &name, res.mode).await.map_err(|e| anyhow::anyhow!("branching {key}: {e:#}"))?;
                    self.store.set_resource(&r.id, res.kind, &res.parent, |x| {
                        x.r#ref = Some(made.r#ref.clone());
                        x.kept = made.kept.clone();
                        x.state = ResourceState::Ready;
                        x.error = None;
                    })?;
                    self.store.log(&r.id, &format!("branched {key} as {}", made.r#ref))?;
                }
                Some(branch) if reset_due || asked.iter().any(|a| a == "*" || *a == key) => {
                    self.store.set_resource(&r.id, res.kind, &res.parent, |x| x.state = ResourceState::Resetting)?;
                    let fresh = b.reset(&res.parent, branch, res.kept.as_deref(), &name, res.mode).await.map_err(|e| anyhow::anyhow!("resetting {key}: {e:#}"))?;
                    self.store.set_resource(&r.id, res.kind, &res.parent, |x| {
                        x.r#ref = Some(fresh.r#ref.clone());
                        x.kept = fresh.kept.clone();
                        x.state = ResourceState::Ready;
                        x.error = None;
                        x.bytes = 0;
                    })?;
                    self.store.log(&r.id, &format!("reset {key} from its parent"))?;
                    reset_any = true;
                }
                Some(_) if res.state != ResourceState::Ready => {
                    self.store.set_resource(&r.id, res.kind, &res.parent, |x| {
                        x.state = ResourceState::Ready;
                        x.error = None;
                    })?;
                }
                Some(_) => {}
            }
        }
        if reset_any || reset_due || !asked.is_empty() {
            self.store.update(&r.id, |row| {
                row.reset_requested = None;
                if reset_any || reset_due {
                    row.last_reset_at = Some(now);
                }
            })?;
        }

        // 2. What the branches use (every `usage_every`), and the size cap.
        let usage_due = self.usage_read.lock().get(&r.id).is_none_or(|t| *t + self.cfg.usage_every.as_millis() as i64 <= now);
        if usage_due {
            for res in self.store.resources(&r.id)? {
                let (Some(branch), Ok(b)) = (&res.r#ref, self.brancher(res.kind)) else { continue };
                match b.usage(&res.parent, branch).await {
                    Ok(u) => self.store.set_resource(&r.id, res.kind, &res.parent, |x| {
                        x.bytes = u.bytes;
                        x.usd_month = u.storage_usd_month;
                        x.usd_hour = u.active_usd_hour;
                        x.spent_usd = u.month_to_date_usd;
                    })?,
                    Err(e) => warn!(preview = %r.id, kind = res.kind.as_str(), error = %e, "previews: reading usage"),
                }
            }
            self.usage_read.lock().insert(r.id.clone(), now);
        }
        let bytes: i64 = self.store.resources(&r.id)?.iter().map(|x| x.bytes).sum();
        if bytes as u64 > s.max_bytes {
            for svc in self.store.services(&r.id)? {
                self.apps.sleep(&r.id, &svc.name).await;
            }
            let msg = format!("its branches hold {bytes} bytes, over the project's max_bytes ({}): reset it to start again", s.max_bytes);
            let row = self.store.update(&r.id, |row| {
                row.state = State::Sleeping;
                row.error = Some(msg.clone());
                row.next_at = SETTLED;
            })?;
            self.report(row).await;
            return Ok(());
        }

        // 3. The secrets environment: its base, the project's overrides, the branches' URLs.
        if is_production(&s.secrets.base) && (!s.secrets.allow_production || !r.trusted) {
            anyhow::bail!("this preview may not inherit the production environment {:?}", s.secrets.base);
        }
        let urls = self.resource_env(&r.id).await?;
        let mut vars: Vec<(String, String)> = s.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (k, v) in urls {
            set(&mut vars, &k, v);
        }
        self.secrets.ensure(&r.project_id, &env_name(&r.id), &s.secrets.base, &vars).await.map_err(|e| anyhow::anyhow!("secrets: {e:#}"))?;

        // 4. Services: the change's repo at its head, every other repo at its default head.
        let repos: Vec<String> = project.resources.iter().filter(|x| x.kind == roll_workspace::Kind::Repo).map(|x| x.r#ref.clone()).collect();
        let mut keep: Vec<String> = Vec::new();
        for repo in &repos {
            let rev = if *repo == r.repo { Some(r.head.clone()) } else { self.source.head(repo, None).await? };
            let Some(rev) = rev else { continue };
            for spec in self.source.services(repo, &rev).await.map_err(|e| anyhow::anyhow!("reading {repo}'s services: {e:#}"))? {
                keep.push(spec.name.clone());
                let svc = self.store.want_service(&r.id, &spec.name, repo, &rev, spec.memory_mb, &|h| self.cfg.url(h))?;
                // A redeploy (or a reset, whose URLs are new) restarts it on its next request.
                if svc.deployed.as_deref() != Some(&rev) || reset_any {
                    let t = Target {
                        preview: r.id.clone(),
                        service: spec.name.clone(),
                        host: svc.host.clone(),
                        repo: repo.clone(),
                        revision: rev.clone(),
                        spec: spec.clone(),
                        sleep_after: Duration::from_secs(s.sleep_after_s as u64),
                    };
                    self.store.set_service(&r.id, &spec.name, ServiceState::Deploying, None, None)?;
                    match self.apps.deploy(&t).await {
                        Ok(()) => {
                            let state = self.apps.status(&r.id, &spec.name).unwrap_or(ServiceState::Sleeping);
                            self.store.set_service(&r.id, &spec.name, state, Some(&rev), None)?;
                            self.store.log(&r.id, &format!("deployed {} at {}", spec.name, short(&rev)))?;
                        }
                        Err(e) => {
                            let msg = format!("{e:#}");
                            self.store.set_service(&r.id, &spec.name, ServiceState::Failed, None, Some(&msg))?;
                            anyhow::bail!("deploying {}: {msg}", spec.name);
                        }
                    }
                }
            }
        }
        for svc in self.store.services(&r.id)? {
            if !keep.contains(&svc.name) {
                self.apps.remove(&r.id, &svc.name).await;
                self.store.drop_service(&r.id, &svc.name)?;
                self.store.log(&r.id, &format!("removed service {}", svc.name))?;
            }
        }

        // 5. Settled.
        let running = self.store.services(&r.id)?.iter().any(|x| x.state == ServiceState::Running);
        let error = (!problems.is_empty()).then(|| problems.join("; "));
        let was = r.state;
        let row = self.store.update(&r.id, |row| {
            row.state = if running || row.state != State::Sleeping { State::Ready } else { State::Sleeping };
            row.error = error.clone();
            row.attempts = 0;
            row.next_at = SETTLED;
        })?;
        if was != row.state || was == State::Creating || was == State::Updating {
            self.store.log(&r.id, row.state.as_str())?;
        }
        self.report(row).await;
        Ok(())
    }

    /// The variables every service of the preview gets for its branches.
    async fn resource_env(&self, id: &str) -> anyhow::Result<Vec<(String, String)>> {
        let mut seen: BTreeMap<Kind, usize> = BTreeMap::new();
        let mut env = Vec::new();
        for res in self.store.resources(id)? {
            let (Some(branch), Ok(b)) = (&res.r#ref, self.brancher(res.kind)) else { continue };
            let n = seen.entry(res.kind).or_default();
            let suffix = (*n > 0).then(|| env_suffix(&res.parent));
            *n += 1;
            let vars = b.connect(&res.parent, branch, res.kept.as_deref()).await.map_err(|e| anyhow::anyhow!("connecting to the {} branch: {e:#}", res.kind.as_str()))?;
            for (k, v) in suffixed(vars, suffix.as_deref()) {
                set(&mut env, &k, v);
            }
        }
        Ok(env)
    }

    async fn delete_branch(&self, id: &str, res: &ResourceRow, name: &str) -> anyhow::Result<()> {
        let Some(branch) = &res.r#ref else { return Ok(()) };
        // A branch must never be its parent: refuse rather than delete production.
        if branch == &res.parent {
            anyhow::bail!("refusing to delete {} {branch}: it's the preview's parent", res.kind.as_str());
        }
        self.store.set_resource(id, res.kind, &res.parent, |x| x.state = ResourceState::Deleting)?;
        self.brancher(res.kind)?.delete(&res.parent, branch, name).await.map_err(|e| anyhow::anyhow!("deleting the {} branch {branch}: {e:#}", res.kind.as_str()))?;
        self.store.log(id, &format!("deleted the {} branch {branch}", res.kind.as_str()))?;
        Ok(())
    }

    /// Delete a preview: services, secrets, branches, then the row. Each step is idempotent.
    async fn teardown(&self, r: &Row) -> anyhow::Result<()> {
        if r.state != State::Deleting {
            let row = self.store.update(&r.id, |row| row.state = State::Deleting)?;
            self.report(row).await;
        }
        for svc in self.store.services(&r.id)? {
            self.apps.remove(&r.id, &svc.name).await;
            self.store.drop_service(&r.id, &svc.name)?;
        }
        self.secrets.delete(&r.project_id, &env_name(&r.id)).await.map_err(|e| anyhow::anyhow!("secrets: {e:#}"))?;
        let name = branch_name(&r.id);
        for res in self.store.resources(&r.id)? {
            self.delete_branch(&r.id, &res, &name).await?;
            self.store.drop_resource(&r.id, res.kind, &res.parent)?;
        }
        let view = self.store.preview(&r.id)?;
        self.source.forget(&view).await;
        self.store.remove(&r.id)?;
        self.usage_read.lock().remove(&r.id);
        info!(preview = %r.id, "previews: deleted");
        Ok(())
    }

    async fn report(&self, row: Row) {
        let Ok(s) = self.store.settings(&row.project_id) else { return };
        if let Ok(p) = self.store.view(row) {
            self.source.report(&p, &s).await;
        }
    }

    // ── what the API asks for ──

    /// A workspace project, or a 404.
    pub fn workspace_project(&self, id: &str) -> Result<roll_workspace::Project> {
        Ok(self.workspace.get(id)?)
    }

    /// A preview for `branch` of `repo` (or for change `change`), made by hand.
    pub async fn create(&self, project_id: &str, repo: &str, branch: Option<&str>, change: Option<&str>, by: &str) -> Result<(Preview, bool)> {
        let project = self.workspace.get(project_id)?;
        if !project.resources.iter().any(|r| r.kind == roll_workspace::Kind::Repo && r.r#ref == repo) {
            return Err(Error::NotFound(format!("{repo} isn't in project {}", project.name)));
        }
        let s = self.store.settings(project_id)?;
        let busy = self.store.rows(Some(project_id))?.iter().filter(|r| r.desired == Desired::Active && r.state != State::Queued).count();
        let n = match (change, branch) {
            (Some(id), _) => {
                let c = self.source.change(id).await.map_err(|e| Error::Internal(e.to_string()))?.ok_or_else(|| Error::NotFound(format!("no change {id}")))?;
                if c.repo != repo {
                    return Err(Error::BadRequest(format!("change {id} is in {}, not {repo}", c.repo)));
                }
                if !c.open {
                    return Err(Error::Conflict(format!("change {id} has landed or closed")));
                }
                if let Some(existing) = self.store.row_by_change(id)? {
                    return Ok((self.store.view(existing)?, false));
                }
                New {
                    project_id: project_id.to_string(),
                    key: c.github_pr.map_or_else(|| format!("c{}", c.number), |n| format!("pr{n}")),
                    project_name: project.name.clone(),
                    repo: repo.to_string(),
                    change_id: Some(c.id.clone()),
                    branch: c.branch.clone(),
                    number: Some(c.number),
                    title: c.title.clone(),
                    github_pr: c.github_pr,
                    head: c.head.clone(),
                    trusted: c.trusted,
                    state: if c.may_run() { State::Creating } else { State::AwaitingApproval },
                    expires_at: None,
                    created_by: by.to_string(),
                }
            }
            (None, Some(b)) => {
                let head = self
                    .source
                    .head(repo, Some(b))
                    .await
                    .map_err(|e| Error::Internal(e.to_string()))?
                    .ok_or_else(|| Error::NotFound(format!("{repo} has no branch {b}")))?;
                if let Some(existing) = self.store.rows(Some(project_id))?.into_iter().find(|r| r.change_id.is_none() && r.repo == repo && r.branch.as_deref() == Some(b) && r.desired == Desired::Active) {
                    return Ok((self.store.view(existing)?, false));
                }
                let now = self.store.now_ms()?;
                New {
                    project_id: project_id.to_string(),
                    key: b.to_string(),
                    project_name: project.name.clone(),
                    repo: repo.to_string(),
                    change_id: None,
                    branch: Some(b.to_string()),
                    number: None,
                    title: format!("{repo}@{b}"),
                    github_pr: None,
                    head,
                    trusted: true,
                    state: State::Creating,
                    expires_at: Some(now + s.ttl_hours as i64 * 3_600_000),
                    created_by: by.to_string(),
                }
            }
            (None, None) => return Err(Error::BadRequest("say which branch (branch) or change (change) to preview".into())),
        };
        if busy >= s.max_previews as usize {
            return Err(Error::Conflict(format!("the project has {busy} previews, its max_previews: delete one first")));
        }
        let (row, made) = self.store.create(&n)?;
        self.kick();
        Ok((self.store.view(row)?, made))
    }

    pub fn delete(&self, id: &str) -> Result<Preview> {
        let r = self.mark_deleted(id, "asked to")?;
        self.store.view(r)
    }

    /// Converge now (redeploy whatever isn't at the revision it should run).
    pub fn refresh(&self, id: &str) -> Result<Preview> {
        let r = self.store.update(id, |row| {
            row.next_at = 0;
            row.attempts = 0;
        })?;
        self.usage_read.lock().remove(id);
        self.kick();
        self.store.view(r)
    }

    /// Re-branch `which` (`"<kind>:<parent>"`, or every resource when empty) from the parents.
    pub fn reset(&self, id: &str, which: Vec<String>) -> Result<Preview> {
        let resources = self.store.resources(id)?;
        for w in &which {
            if !resources.iter().any(|r| format!("{}:{}", r.kind.as_str(), r.parent) == *w) {
                return Err(Error::BadRequest(format!("the preview has no resource {w}")));
            }
        }
        let which = if which.is_empty() { vec!["*".to_string()] } else { which };
        let r = self.store.update(id, |row| {
            if row.desired == Desired::Active {
                row.reset_requested = Some(which.clone());
                row.next_at = 0;
                row.attempts = 0;
                if row.state == State::Sleeping && row.error.is_some() {
                    row.state = State::Updating;
                }
            }
        })?;
        self.kick();
        self.store.view(r)
    }

    pub async fn sleep(&self, id: &str) -> Result<Preview> {
        let r = self.store.row(id)?;
        for svc in self.store.services(id)? {
            self.apps.sleep(id, &svc.name).await;
            self.store.set_service(id, &svc.name, ServiceState::Sleeping, None, None)?;
        }
        let r = self.store.update(&r.id, |row| {
            row.asleep = true;
            if row.state == State::Ready {
                row.state = State::Sleeping;
            }
        })?;
        self.store.log(id, "put to sleep")?;
        self.report(r).await;
        self.store.preview(id)
    }

    pub async fn wake(&self, id: &str) -> Result<Preview> {
        let r = self.store.row(id)?;
        if r.desired == Desired::Deleted || matches!(r.state, State::AwaitingApproval | State::Queued | State::Deleting) {
            return Err(Error::Conflict(format!("the preview is {}: it can't be woken", r.state.as_str())));
        }
        self.store.update(id, |row| {
            row.asleep = false;
            row.last_active_at = row.last_active_at.max(0);
        })?;
        self.store.touch(id)?;
        for svc in self.store.services(id)? {
            if let Err(e) = self.apps.wake(id, &svc.name).await {
                return Err(Error::Conflict(format!("{} didn't start: {e:#}", svc.name)));
            }
        }
        self.store.preview(id)
    }

    pub async fn logs(&self, id: &str, service: Option<&str>, lines: usize) -> Result<(String, Vec<String>)> {
        let services = self.store.services(id)?;
        let svc = match service {
            Some(name) => services.into_iter().find(|s| s.name == name).ok_or_else(|| Error::NotFound(format!("the preview has no service {name}")))?,
            None => services.into_iter().next().ok_or_else(|| Error::NotFound("the preview has no services".into()))?,
        };
        Ok((svc.name.clone(), self.apps.logs(id, &svc.name, lines).await))
    }
}

/// Whether a change gets a preview under the project's rules (approval aside).
fn wants(s: &Settings, c: &ChangeInfo) -> bool {
    c.open && (s.drafts || !c.draft) && crate::glob::any(&s.branches, c.branch.as_deref().unwrap_or("*"))
}

/// The secrets environment of a preview: its branch name (`pv-<id>`, within roll secrets'
/// 32-character slugs).
pub fn env_name(preview_id: &str) -> String {
    branch_name(preview_id)
}

fn short(rev: &str) -> &str {
    let rev = rev.strip_prefix("cs_").unwrap_or(rev);
    &rev[..rev.len().min(10)]
}

#[async_trait]
impl EnvSource for Engine {
    async fn env(&self, preview: &str, service: &str) -> anyhow::Result<(Target, Vec<(String, String)>)> {
        let r = self.store.row(preview)?;
        if r.desired == Desired::Deleted || matches!(r.state, State::AwaitingApproval | State::Queued | State::Deleting) {
            anyhow::bail!("the preview is {}", if r.desired == Desired::Deleted { "being deleted" } else { r.state.as_str() });
        }
        // A fork's head runs only once approved, checked again at every start.
        if let Some(change) = &r.change_id {
            match self.source.change(change).await? {
                Some(c) if c.may_run() && c.head == r.head => {}
                Some(c) if !c.may_run() => anyhow::bail!("this change's head isn't approved to run (approve-run)"),
                Some(_) => anyhow::bail!("the change moved on: the preview is updating"),
                None => anyhow::bail!("the change was deleted"),
            }
        }
        let s = self.store.settings(&r.project_id)?;
        let services = self.store.services(preview)?;
        let svc = services.iter().find(|x| x.name == service).ok_or_else(|| anyhow::anyhow!("no service {service}"))?;
        let rev = svc.revision.clone().ok_or_else(|| anyhow::anyhow!("{service} isn't deployed yet"))?;
        let spec = self
            .source
            .services(&svc.repo, &rev)
            .await?
            .into_iter()
            .find(|x| x.name == service)
            .ok_or_else(|| anyhow::anyhow!("{} no longer has a service {service} at {}", svc.repo, short(&rev)))?;
        // Later entries win: roll.toml, the secrets environment, the project's overrides, the
        // branches, then roll's own.
        let mut env = spec.env.clone();
        for (k, v) in self.secrets.resolve(&r.project_id, &env_name(&r.id)).await.map_err(|e| anyhow::anyhow!("secrets: {e:#}"))? {
            set(&mut env, &k, v);
        }
        for (k, v) in &s.env {
            set(&mut env, k, v.clone());
        }
        for (k, v) in self.resource_env(preview).await? {
            set(&mut env, &k, v);
        }
        set(&mut env, "ROLL_PREVIEW", r.id.clone());
        set(&mut env, "ROLL_PREVIEW_URL", svc.url.clone());
        for other in &services {
            set(&mut env, &format!("ROLL_PREVIEW_URL_{}", env_suffix(&other.name)), other.url.clone());
        }
        set(&mut env, "PORT", spec.port.to_string());
        set(&mut env, "ROLL_REPO", svc.repo.clone());
        set(&mut env, "ROLL_CHANGESET", rev.clone());
        if let Some(c) = &r.change_id {
            set(&mut env, "ROLL_CHANGE", c.clone());
        }
        let t = Target {
            preview: r.id.clone(),
            service: service.to_string(),
            host: svc.host.clone(),
            repo: svc.repo.clone(),
            revision: rev,
            spec,
            sleep_after: Duration::from_secs(s.sleep_after_s as u64),
        };
        Ok((t, env))
    }

    fn touched(&self, preview: &str) {
        let _ = self.store.touch(preview);
    }
}

impl Engine {
    /// The runtime saw a service start, stop or fail: keep the preview's state truthful.
    pub async fn observed(&self, preview: &str, service: &str, state: ServiceState, error: Option<String>, active_ms: i64) {
        let _ = self.store.set_service(preview, service, state, None, error.as_deref());
        if active_ms > 0 {
            let _ = self.store.add_active(preview, service, active_ms);
        }
        let Ok(services) = self.store.services(preview) else { return };
        let running = services.iter().any(|s| s.state == ServiceState::Running);
        let Ok(row) = self.store.update(preview, |row| {
            if matches!(row.state, State::Ready | State::Sleeping) {
                row.state = if running { State::Ready } else { State::Sleeping };
                if running {
                    row.asleep = false;
                }
            }
        }) else {
            return;
        };
        let msg = match (state, &error) {
            (ServiceState::Running, _) => format!("{service} started"),
            (ServiceState::Sleeping, _) => format!("{service} went to sleep"),
            (ServiceState::Failed, Some(e)) => format!("{service} failed: {}", e.lines().next().unwrap_or("")),
            _ => return,
        };
        let _ = self.store.log(preview, &msg);
        self.report(row).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_rules() {
        let c = Config { port: Some(7070), domain: "preview.test".into(), ..Config::default() };
        assert_eq!(c.url("web-pr7-shop"), "http://web-pr7-shop.preview.test:7070");
        let https = Config { scheme: "https".into(), port: Some(443), ..c };
        assert_eq!(https.url("x"), "https://x.preview.test");
        let s = Settings { enabled: true, branches: vec!["feat/*".into()], ..Settings::default() };
        let ch = ChangeInfo {
            id: "chg_1".into(),
            repo: "a/b".into(),
            number: 1,
            title: "t".into(),
            branch: Some("feat/x".into()),
            head: "cs_1".into(),
            open: true,
            draft: false,
            trusted: true,
            approved: false,
            github_pr: None,
        };
        assert!(wants(&s, &ch));
        assert!(!wants(&s, &ChangeInfo { branch: Some("fix/x".into()), ..ch.clone() }));
        assert!(!wants(&s, &ChangeInfo { draft: true, ..ch.clone() }));
        assert_eq!(env_suffix("prj_ab-c"), "PRJ_AB_C");
    }
}
