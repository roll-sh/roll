//! roll code as previews see it: changes, branch heads and `roll.toml` services, read from
//! `code_control::Code`; previews reported back onto their change (`Change.preview`, which roll
//! code's GitHub outbox turns into the pull request's comment, commit status and deployment).

use crate::engine::Engine;
use crate::ports::{ChangeInfo, ServiceSpec, Source};
use crate::runtime::Trees;
use crate::settings::Settings;
use crate::{Kind, Preview, State};
use async_trait::async_trait;
use code_control::model::{
    ChangeState, PreviewEnvironment, PreviewGithub, PreviewPostgres, PreviewRedis, PreviewResourceLink, PreviewServiceLink, PreviewState, RepoRow,
};
use code_control::{Code, PreviewBackend};
use code_preview::isolation::{Isolation, Job};
use code_preview::RollToml;
use code_preview::config::PreviewSpec;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock, Weak};
use tokio::sync::broadcast;

/// Services read from a revision's `roll.toml`, kept per (repo, revision).
const SPEC_CACHE: usize = 512;

pub struct CodeSource {
    code: Arc<Code>,
    nudge: broadcast::Sender<()>,
    specs: Mutex<HashMap<(String, String), Vec<ServiceSpec>>>,
}

impl CodeSource {
    /// Follow roll code: its `change` events nudge the engine (the reconciler reads the
    /// database itself, so a dropped event costs one interval at most).
    pub fn new(code: Arc<Code>) -> Arc<CodeSource> {
        let (nudge, _) = broadcast::channel(16);
        let mut events = code.subscribe();
        let tx = nudge.clone();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(e) if e.kind == "change" => {
                        let _ = tx.send(());
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let _ = tx.send(());
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Arc::new(CodeSource { code, nudge, specs: Mutex::new(HashMap::new()) })
    }

    fn repo(&self, full: &str) -> anyhow::Result<RepoRow> {
        let (owner, name) = full.split_once('/').ok_or_else(|| anyhow::anyhow!("{full:?} isn't owner/name"))?;
        Ok(self.code.find_repo(owner, name)?)
    }

    fn info(&self, repo: &RepoRow, c: &code_control::model::Change) -> ChangeInfo {
        let head = self.code.store.resolve(&repo.id, &c.head).ok().flatten().map(|cs| cs.to_string()).unwrap_or_else(|| c.head.clone());
        ChangeInfo {
            id: c.id.clone(),
            repo: repo.full_name(),
            number: c.number as i64,
            title: c.title.clone(),
            branch: c.branch.clone(),
            head,
            open: matches!(c.state, ChangeState::Open | ChangeState::Queued | ChangeState::Conflict),
            draft: c.draft,
            labels: c.labels.clone(),
            trusted: c.trusted,
            approved: c.run_approved,
            github_pr: c.github_pr.map(|n| n as i64),
        }
    }

    fn summary(&self, p: &Preview, s: &Settings) -> code_control::model::Preview {
        let state = match p.state {
            State::Ready => PreviewState::Running,
            State::Sleeping => PreviewState::Sleeping,
            State::Failed => PreviewState::Failed,
            _ => PreviewState::Creating,
        };
        let pg = p.resources.iter().find(|r| r.kind == Kind::Postgres).and_then(|r| r.r#ref.clone().map(|b| PreviewPostgres { project_id: r.parent.clone(), branch_id: b }));
        let redis = p.resources.iter().find(|r| r.kind == Kind::Redis).and_then(|r| r.r#ref.clone().map(|db_id| PreviewRedis { db_id }));
        let web = self.code.config.web_url.trim_end_matches('/');
        code_control::model::Preview {
            state,
            // Plain URLs: the change, its pull request and repo webhooks can be public, so the
            // access token never goes there (roll's own surfaces hand out `share_url`).
            url: p.url.clone(),
            postgres: pg,
            redis,
            error: p.error.clone(),
            created_at: p.created_at / 1000,
            environment: Some(PreviewEnvironment {
                id: p.id.clone(),
                slug: p.slug.clone(),
                state: p.state.as_str().to_string(),
                services: p
                    .services
                    .iter()
                    .map(|x| PreviewServiceLink { name: x.name.clone(), url: x.url.clone(), state: x.state.as_str().to_string() })
                    .collect(),
                resources: p.resources.iter().map(|x| PreviewResourceLink { kind: x.kind.as_str().to_string(), parent: x.parent.clone(), r#ref: x.r#ref.clone() }).collect(),
                // As precise as the comment shows: a usage reading that moves a cost by a hair
                // doesn't make a new event for every webhook and the pull request.
                sleeping_usd_month: (p.cost.sleeping_usd_month * 100.0).round() / 100.0,
                active_usd_hour: (p.cost.active_usd_hour * 1000.0).round() / 1000.0,
                dashboard_url: (!web.is_empty()).then(|| format!("{web}/dashboard/projects/{}/previews?preview={}", p.project, p.id)),
                github: PreviewGithub { comment: s.github.comment, status: s.github.status, deployment: s.github.deployment },
                head: Some(p.head.clone()),
                schema_changes: p.schema_changes.clone(),
                expires_at: p.goes_at,
                page_a11y: p.page_a11y.as_ref().map(|a| code_control::model::PreviewPageA11y {
                    issues: a.issues.iter().map(|i| code_control::model::PreviewA11yIssue { kind: i.kind.clone(), count: i.count, first: i.first.clone() }).collect(),
                    lang: a.lang,
                    title: a.title,
                }),
                page_health: p.page_health.as_ref().map(|h| code_control::model::PreviewPageHealth { errors: h.errors, first_error: h.first_error.clone(), failed: h.failed, first_failed: h.first_failed.clone() }),
                page_speed: p.page_speed.as_ref().map(|v| code_control::model::PreviewPageSpeed {
                    ttfb_ms: v.now.ttfb_ms,
                    fcp_ms: v.now.fcp_ms,
                    load_ms: v.now.load_ms,
                    bytes: v.now.bytes,
                    before_load_ms: v.before.as_ref().map(|b| b.load_ms),
                }),
                visual_change: p.visual_change.as_ref().map(|v| code_control::model::PreviewVisualChange { since: v.since.clone(), percent: v.percent, phone_percent: v.phone_percent }),
                earlier: p.earlier.iter().map(|x| code_control::model::PreviewEarlier { service: x.service.clone(), head: x.revision.clone(), url: x.url.clone() }).collect(),
                // Only a public preview's: the comment is readable by whoever reads the pull request.
                screenshot_url: (s.access == crate::settings::Access::Public)
                    .then(|| p.screenshot_link.clone())
                    .flatten()
                    .map(|(exp, sig)| format!("{}/v1/previews/{}/screenshot.png?exp={exp}&sig={sig}", self.code.config.api_url.trim_end_matches('/'), p.id)),
            }),
        }
    }
}

/// The service a `[preview]` section describes, named after the repo unless it says.
pub fn service_of(repo: &str, p: &PreviewSpec) -> ServiceSpec {
    let name = p.name.clone().unwrap_or_else(|| {
        let n = crate::label_part(repo.rsplit('/').next().unwrap_or(repo), 20);
        if n.is_empty() { "app".into() } else { n }
    });
    ServiceSpec {
        name,
        image: p.image.clone(),
        run: p.run.clone(),
        port: p.port,
        memory_mb: p.memory_mb,
        cpus: p.cpus,
        env: p.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        health: p.health.clone(),
    }
}

#[async_trait]
impl Source for CodeSource {
    async fn open_changes(&self, repo: &str) -> anyhow::Result<Vec<ChangeInfo>> {
        let r = self.repo(repo)?;
        let mut out = Vec::new();
        for state in [ChangeState::Open, ChangeState::Queued, ChangeState::Conflict] {
            for c in self.code.db.changes(&r.id, Some(state), 1000)? {
                out.push(self.info(&r, &c));
            }
        }
        Ok(out)
    }

    async fn change(&self, id: &str) -> anyhow::Result<Option<ChangeInfo>> {
        let Some(c) = self.code.db.change(id)? else { return Ok(None) };
        let Some(r) = self.code.db.repo_by_id(&c.repo_id)? else { return Ok(None) };
        Ok(Some(self.info(&r, &c)))
    }

    async fn head(&self, repo: &str, branch: Option<&str>) -> anyhow::Result<Option<String>> {
        let r = match self.repo(repo) {
            Ok(r) => r,
            Err(e) if e.downcast_ref::<code_control::Error>().is_some_and(|e| matches!(e, code_control::Error::NotFound(_))) => return Ok(None),
            Err(e) => return Err(e),
        };
        let branch = branch.unwrap_or(&r.default_branch);
        Ok(self.code.store.get_ref(&r.id, branch)?.map(|cs| cs.to_string()))
    }

    async fn services(&self, repo: &str, revision: &str) -> anyhow::Result<Vec<ServiceSpec>> {
        let key = (repo.to_string(), revision.to_string());
        if let Some(s) = self.specs.lock().get(&key) {
            return Ok(s.clone());
        }
        let r = self.repo(repo)?;
        let cs = self.code.store.resolve_or_404(&r.id, revision)?;
        let specs = {
            let tree = self.code.checkout(&r.id, &cs).await?;
            match RollToml::load(tree.path()).map_err(anyhow::Error::msg)? {
                Some(RollToml { preview: Some(p), .. }) => vec![service_of(repo, &p)],
                _ => Vec::new(),
            }
        };
        let mut cache = self.specs.lock();
        if cache.len() >= SPEC_CACHE {
            cache.clear();
        }
        cache.insert(key, specs.clone());
        Ok(specs)
    }

    async fn paths(&self, repo: &str, head: &str) -> anyhow::Result<Option<Vec<String>>> {
        let r = self.repo(repo)?;
        let Some(cs) = self.code.store.resolve(&r.id, head)? else { return Ok(None) };
        Ok(Some(self.code.change_paths(&r, &cs)?))
    }

    async fn paths_between(&self, repo: &str, from: &str, to: &str) -> anyhow::Result<Option<Vec<String>>> {
        let r = self.repo(repo)?;
        let (Some(a), Some(b)) = (self.code.store.resolve(&r.id, from)?, self.code.store.resolve(&r.id, to)?) else { return Ok(None) };
        Ok(Some(self.code.store.compare(&r.id, &a, &b)?.into_iter().map(|d| d.path.as_str().to_string()).collect()))
    }

    async fn feedback(&self, change: &str) -> anyhow::Result<Vec<crate::notes::Note>> {
        Ok(self
            .code
            .db
            .reviews(change)?
            .into_iter()
            .filter(|r| !r.dismissed && r.verdict == code_control::review::Verdict::Comment)
            .filter_map(|r| {
                let (path, selector, note) = crate::notes::parse(&r.body)?;
                Some(crate::notes::Note { path, selector, note, by: r.reviewer.name, at: r.created_at })
            })
            .collect())
    }

    async fn git_commit(&self, repo: &str, revision: &str) -> anyhow::Result<Option<String>> {
        let r = self.repo(repo)?;
        let cs = self.code.store.resolve_or_404(&r.id, revision)?;
        Ok(self.code.store.git_of(&r.id, &cs)?.map(|(commit, _)| commit.hex()))
    }

    async fn report(&self, p: &Preview, s: &Settings) {
        let Some(change) = &p.change else { return };
        let summary = self.summary(p, s);
        if let Err(e) = self.code.report_environment(change, Some(&summary)) {
            tracing::warn!(preview = %p.id, error = %e, "previews: reporting to the change");
        }
    }

    async fn budget(&self, repos: &[String], alert: &crate::ports::BudgetAlert) -> anyhow::Result<()> {
        let web = self.code.config.web_url.trim_end_matches('/');
        let dashboard = (!web.is_empty()).then(|| format!("{web}/dashboard/projects/{}/previews", alert.project));
        for full in repos {
            // A repo gone from roll code has nobody to tell; anything else is tried again.
            let repo = match self.repo(full) {
                Ok(r) => r,
                Err(e) if e.downcast_ref::<code_control::Error>().is_some_and(|e| matches!(e, code_control::Error::NotFound(_))) => continue,
                Err(e) => return Err(e),
            };
            self.code.emit_budget(&repo.id, serde_json::json!({ "budget": alert, "dashboard_url": dashboard }))?;
        }
        Ok(())
    }

    async fn forget(&self, p: &Preview) {
        let Some(change) = &p.change else { return };
        // Only the change's preview environment, never roll code's own preview of it.
        let ours = self.code.db.change(change).ok().flatten().and_then(|c| c.preview).and_then(|x| x.environment).is_some_and(|e| e.id == p.id);
        if ours && let Err(e) = self.code.report_environment(change, None) {
            tracing::warn!(preview = %p.id, error = %e, "previews: clearing the change's preview");
        }
    }

    fn nudges(&self) -> Option<broadcast::Receiver<()>> {
        Some(self.nudge.subscribe())
    }
}

#[async_trait]
impl Trees for CodeSource {
    async fn prepare(&self, iso: &dyn Isolation, job: &Job, repo: &str, revision: &str) -> anyhow::Result<()> {
        let r = self.repo(repo)?;
        let cs = self.code.store.resolve_or_404(&r.id, revision)?;
        // A pooled work tree, leased only while the container's copy is made.
        let tree = self.code.checkout(&r.id, &cs).await?;
        iso.prepare(job, tree.path()).await
    }

    async fn run(&self, iso: &Arc<dyn Isolation>, spec: &code_preview::config::CheckSpec, repo: &str, revision: &str, env: &[(String, String)]) -> anyhow::Result<code_preview::runner::RunOutput> {
        let r = self.repo(repo)?;
        let cs = self.code.store.resolve_or_404(&r.id, revision)?;
        let tree = self.code.checkout(&r.id, &cs).await?;
        Ok(code_preview::runner::run_check(iso, spec, tree.path(), env).await)
    }
}

/// roll code's preview backend, in front of its single-container previews: a change whose repo
/// is in a project with previews on gets the project's preview environment instead
/// (`POST /v1/code/changes/{id}/preview` makes one, `DELETE` removes it).
pub struct Backend {
    inner: Arc<dyn PreviewBackend>,
    engine: OnceLock<Weak<Engine>>,
    workspace: Arc<roll_workspace::Workspace>,
    source: Arc<CodeSource>,
}

impl Backend {
    pub fn new(inner: Arc<dyn PreviewBackend>, workspace: Arc<roll_workspace::Workspace>, source: Arc<CodeSource>) -> Arc<Backend> {
        Arc::new(Backend { inner, engine: OnceLock::new(), workspace, source })
    }

    pub fn attach(&self, engine: &Arc<Engine>) {
        let _ = self.engine.set(Arc::downgrade(engine));
    }

    /// The engine and the project of a repo, when its project has previews on.
    fn environment_for(&self, repo: &RepoRow) -> Option<(Arc<Engine>, String)> {
        let engine = self.engine.get().and_then(Weak::upgrade)?;
        let full = repo.full_name();
        let project = self.workspace.list(roll_workspace::Scope::All).ok()?.into_iter().find(|p| p.resources.iter().any(|r| r.kind == roll_workspace::Kind::Repo && r.r#ref == full))?;
        engine.store.settings(&project.id).ok().filter(|s| s.enabled)?;
        Some((engine, project.id))
    }
}

#[async_trait]
impl PreviewBackend for Backend {
    async fn create(&self, repo: &RepoRow, change: &code_control::model::Change, dir: &Path) -> anyhow::Result<code_control::model::Preview> {
        let Some((engine, project)) = self.environment_for(repo) else { return self.inner.create(repo, change, dir).await };
        let (p, _) = engine.create(&project, &repo.full_name(), None, Some(&change.id), "roll").await.map_err(|e| anyhow::anyhow!("{e}"))?;
        let s = engine.store.settings(&project)?;
        Ok(self.source.summary(&p, &s))
    }

    async fn delete(&self, repo: &RepoRow, change: &code_control::model::Change) -> anyhow::Result<()> {
        let env = change.preview.as_ref().and_then(|p| p.environment.as_ref());
        match (env, self.engine.get().and_then(Weak::upgrade)) {
            (Some(e), Some(engine)) => match engine.delete(&e.id) {
                Ok(_) | Err(crate::Error::NotFound(_)) => Ok(()),
                Err(err) => Err(anyhow::anyhow!("{err}")),
            },
            _ => self.inner.delete(repo, change).await,
        }
    }
}
