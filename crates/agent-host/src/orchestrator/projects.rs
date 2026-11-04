//! Project settings (docs/code/agents.md, "Project settings"): `project.update`, `project.pull`,
//! auto-pull and the worktree setup script.
//!
//! Ported from T3 Code: the settings from `packages/contracts/src/settings.ts` and
//! `apps/server/src/orchestration-v2/ProjectStore.ts` (with migration
//! `045_ProjectionProjectsAutoPull.ts`), auto-pull from `apps/server/src/vcs/VcsStatusBroadcaster.ts`
//! (`maybeAutoPull`, its remote refresh loop) and `serverRuntimeStartup.ts` (`autoPullProjects`
//! at startup), the setup script from `apps/server/src/project/ProjectSetupScriptRunner.ts`.
//!
//! - Auto-pull fast-forwards a project's checkout when its default branch moved on the remote,
//!   by [`crate::git::fast_forward`]'s rules; anything else is a reason, and nothing is touched.
//!   Unlike T3 (which only logs), the last result is kept on the project (`Project.pull`), so the
//!   canvas can say what happened. It is rewritten only when it changes, so a project that stays
//!   up to date doesn't send a record every minute.
//! - Projects of plain git repos fetch with the user's own git setup every
//!   [`Config::pull_every`](crate::config::Config::pull_every), starting at once (T3 pulls every
//!   project at startup and while a client watches). roll code projects can't fetch without the
//!   relay's grant, so the relay sends `project.pull` with one when their default branch moves.
//! - A checkout an agent is working in is never pulled under it (T3 doesn't check this).

use super::{Fail, Host, Res, now};
use crate::git;
use agent_proto::*;
use std::path::Path;
use std::sync::Weak;
use std::time::Duration;

/// Limits on what a project's settings may hold.
const MAX_SCRIPTS: usize = 32;
const MAX_NAME: usize = 120;
const MAX_COMMAND: usize = 8 << 10;
const MAX_FIELD: usize = 200;

/// The setup script's terminal size until a client resizes it.
const SETUP_COLS: u16 = 120;
const SETUP_ROWS: u16 = 32;

impl Host {
    pub(crate) async fn project_update(&self, id: &ProjectId, name: Option<String>, settings: Option<ProjectSettings>, grant: Option<CodeGrant>) -> Res<Reply> {
        let settings = settings.map(checked).transpose()?;
        let turned_on = {
            let _guard = self.projects.lock().await;
            let mut p = self.project(id)?;
            if let Some(name) = name {
                let name = name.trim();
                if name.is_empty() || name.chars().count() > MAX_NAME {
                    return Err(Fail::Invalid(format!("a project's name is 1 to {MAX_NAME} characters")));
                }
                p.name = name.to_string();
            }
            let was_on = p.settings.auto_pull;
            if let Some(settings) = settings {
                p.settings = settings;
            }
            p.updated_at = now();
            let on = p.settings.auto_pull;
            self.put(vec![Entity::Project(p)])?;
            on && !was_on
        };
        // Turning auto-pull on pulls at once (with the relay's grant for a roll code repo), after
        // the reply: the settings are stored, the pull's result follows as a record.
        if turned_on {
            let host = self.me.clone();
            let id = id.clone();
            tokio::spawn(async move {
                if let Some(host) = host.upgrade()
                    && let Err(e) = host.pull(&id, grant.as_ref()).await
                {
                    tracing::warn!("auto-pull of {id}: {e:?}");
                }
            });
        }
        Ok(Reply::Ok)
    }

    pub(crate) async fn project_pull(&self, id: &ProjectId, grant: Option<CodeGrant>) -> Res<Reply> {
        Ok(Reply::Pulled(self.pull(id, grant.as_ref()).await?))
    }

    /// Pulls a project by auto-pull's rules and records the result on it. One pull at a time.
    pub(crate) async fn pull(&self, id: &ProjectId, grant: Option<&CodeGrant>) -> Res<PullResult> {
        let _one = self.pulling.lock().await;
        let project = self.project(id)?;
        let grant = grant.filter(|g| project.repo.as_ref() == Some(&g.repo));
        let outcome = if self.busy_in(&project)? {
            PullOutcome::Skipped { reason: PullSkip::Busy, message: "an agent is working in the checkout".into() }
        } else if project.repo.is_some() && grant.is_none() {
            PullOutcome::Skipped { reason: PullSkip::NoAccess, message: "a roll code repo is fetched with roll's access: it pulls when roll asks".into() }
        } else {
            git::fast_forward(Path::new(&project.path), grant).await
        };
        self.record_pull(id, outcome).await
    }

    /// Whether a thread working in the project's own checkout (not a worktree) is busy.
    fn busy_in(&self, project: &Project) -> anyhow::Result<bool> {
        Ok(self.store.list(EntityKind::Thread)?.into_iter().any(|e| {
            matches!(e, Entity::Thread(t) if t.project == project.id && t.worktree.is_none() && t.status.is_busy())
        }))
    }

    /// Keeps a pull's result (and the checkout's git info) on the project. The same outcome again
    /// keeps its first time and writes nothing, unless the git info moved.
    async fn record_pull(&self, id: &ProjectId, outcome: PullOutcome) -> Res<PullResult> {
        let _guard = self.projects.lock().await;
        let mut p = self.project(id)?;
        let git = git::info(Path::new(&p.path)).await;
        let again = !matches!(outcome, PullOutcome::Pulled { .. }) && p.pull.as_ref().is_some_and(|r| r.outcome == outcome);
        let result = match (&p.pull, again) {
            (Some(last), true) => last.clone(),
            _ => PullResult { at: now(), outcome },
        };
        if !again || p.git != git {
            p.pull = Some(result.clone());
            p.git = git;
            p.updated_at = now();
            self.put(vec![Entity::Project(p)])?;
        }
        Ok(result)
    }

    /// One round of auto-pull over the projects that fetch on their own.
    async fn pull_round(&self) {
        let projects = match self.store.list(EntityKind::Project) {
            Ok(p) => p,
            Err(e) => return tracing::warn!("auto-pull: listing projects: {e:#}"),
        };
        for entity in projects {
            if let Entity::Project(p) = entity
                && p.settings.auto_pull
                && p.repo.is_none()
                && let Err(e) = self.pull(&p.id, None).await
            {
                tracing::warn!("auto-pull of {}: {e:?}", p.id);
            }
        }
    }

    /// Runs the project's setup script (the first with `on_worktree`) in a terminal in a new
    /// thread's worktree, beside the agent (T3's `ProjectSetupScriptRunner`, `async` scripts).
    /// A script that can't start never fails the thread.
    pub(crate) async fn setup_worktree(&self, project: &ProjectId, thread: &ThreadId) {
        let Ok(p) = self.project(project) else { return };
        let Some(script) = p.settings.scripts.into_iter().find(|s| s.on_worktree) else { return };
        if let Err(e) = self.terminal_open(None, Some(thread.clone()), None, Some(script.id), SETUP_COLS, SETUP_ROWS).await {
            tracing::warn!("setup script of {project} for {thread}: {e:?}");
        }
    }
}

/// Auto-pull for the projects that fetch on their own: a round now, then one every `every`.
pub(crate) fn spawn_puller(host: Weak<Host>, every: Option<Duration>) {
    let Some(every) = every else { return };
    tokio::spawn(async move {
        loop {
            let Some(h) = host.upgrade() else { return };
            h.pull_round().await;
            drop(h);
            tokio::time::sleep(every).await;
        }
    });
}

/// Settings as given, trimmed and checked.
fn checked(mut s: ProjectSettings) -> Res<ProjectSettings> {
    if s.scripts.len() > MAX_SCRIPTS {
        return Err(Fail::Invalid(format!("a project has at most {MAX_SCRIPTS} scripts")));
    }
    let mut ids = std::collections::HashSet::new();
    for script in &mut s.scripts {
        script.id = script.id.trim().to_string();
        script.name = script.name.trim().to_string();
        script.command = script.command.trim().to_string();
        let id_ok = !script.id.is_empty() && script.id.len() <= 64 && script.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !id_ok {
            return Err(Fail::Invalid(format!("script id {:?} must be 1 to 64 letters, digits, '-' or '_'", script.id)));
        }
        if !ids.insert(script.id.clone()) {
            return Err(Fail::Invalid(format!("script id {:?} is used twice", script.id)));
        }
        if script.name.is_empty() || script.name.chars().count() > MAX_NAME {
            return Err(Fail::Invalid(format!("a script's name is 1 to {MAX_NAME} characters")));
        }
        if script.command.is_empty() || script.command.len() > MAX_COMMAND {
            return Err(Fail::Invalid(format!("a script's command is 1 to {MAX_COMMAND} bytes")));
        }
    }
    let d = &mut s.defaults;
    for field in [&mut d.model, &mut d.effort] {
        *field = field.take().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    }
    let long = |v: &Option<String>| v.as_ref().is_some_and(|v| v.len() > MAX_FIELD);
    if long(&d.model) || long(&d.effort) || d.provider.as_ref().is_some_and(|p| p.0.len() > MAX_FIELD) {
        return Err(Fail::Invalid(format!("a default is at most {MAX_FIELD} bytes")));
    }
    Ok(s)
}
