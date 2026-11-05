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
                // A new icon may name another picture.
                if settings.icon != p.settings.icon {
                    p.logo = self.find_logo(&p.path, &settings.icon).await;
                }
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
        let pulled = matches!(outcome, PullOutcome::Pulled { .. });
        let result = self.record_pull(id, outcome).await?;
        // New commits may bring a new logo.
        if pulled && let Err(e) = self.refresh_logo(id).await {
            tracing::warn!("logo of {id}: {e:?}");
        }
        Ok(result)
    }

    /// A project's logo as its files say now (`logo.rs`), off the async threads.
    pub(crate) async fn find_logo(&self, path: &str, icon: &ProjectIcon) -> Option<ProjectLogo> {
        let logos = self.logos.clone();
        let (path, icon) = (std::path::PathBuf::from(path), icon.clone());
        tokio::task::spawn_blocking(move || logos.resolve(&path, &icon)).await.ok().flatten()
    }

    /// Looks for the project's logo again and records it only if it changed.
    pub(crate) async fn refresh_logo(&self, id: &ProjectId) -> Res<()> {
        let p = self.project(id)?;
        let logo = self.find_logo(&p.path, &p.settings.icon).await;
        if logo == p.logo {
            return Ok(());
        }
        let _guard = self.projects.lock().await;
        let mut now_p = self.project(id)?;
        // A `project.update` that changed the icon meanwhile found the logo for it.
        if now_p.settings.icon != p.settings.icon || now_p.logo == logo {
            return Ok(());
        }
        now_p.logo = logo;
        now_p.updated_at = now();
        self.put(vec![Entity::Project(now_p)])?;
        Ok(())
    }

    /// Every project's logo, again (at startup and on auto-pull's loop). Unchanged files are
    /// only looked at, never read again (`logo.rs` remembers them by fingerprint).
    async fn refresh_logos(&self) {
        let projects = match self.store.list(EntityKind::Project) {
            Ok(p) => p,
            Err(e) => return tracing::warn!("logos: listing projects: {e:#}"),
        };
        for entity in projects {
            if let Entity::Project(p) = entity
                && let Err(e) = self.refresh_logo(&p.id).await
            {
                tracing::warn!("logo of {}: {e:?}", p.id);
            }
        }
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
/// Each round looks for every project's logo again too (hourly while auto-pull is off).
pub(crate) fn spawn_puller(host: Weak<Host>) {
    tokio::spawn(async move {
        loop {
            let Some(h) = host.upgrade() else { return };
            // The machine's settings may change the pace (or turn it off) at any time.
            let every = h.pull_every();
            // Logos first, since a round of pulls may take a while. The first round is the startup's.
            h.refresh_logos().await;
            if every.is_some() {
                h.pull_round().await;
            }
            let changed = h.settings_changed.clone();
            drop(h);
            tokio::select! {
                _ = tokio::time::sleep(every.unwrap_or(PULL_OFF_LOOK)) => {}
                _ = changed.notified() => {}
            }
        }
    });
}

/// How often a puller that is turned off looks again (the settings wake it sooner).
const PULL_OFF_LOOK: Duration = Duration::from_secs(3600);

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
    agent_proto::check_options(&d.options).map_err(Fail::Invalid)?;
    if s.auto_settle.after_days.is_some_and(|d| !(1..=AutoSettle::MAX_DAYS).contains(&d)) {
        return Err(Fail::Invalid(format!("auto-settle waits 1 to {} days", AutoSettle::MAX_DAYS)));
    }
    if s.cleanup.worktree_after_days.is_some_and(|d| d > Cleanup::MAX_DAYS) {
        return Err(Fail::Invalid(format!("worktree cleanup waits 0 to {} days", Cleanup::MAX_DAYS)));
    }
    s.icon = s.icon.check().map_err(Fail::Invalid)?;
    Ok(s)
}
