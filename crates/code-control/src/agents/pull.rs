//! Auto-pull for roll code projects (docs/code/agents.md, "Project settings"). A roll code
//! checkout's remote has no credentials in it, so its daemon can't fetch on its own: the relay
//! asks it to (`project.pull`, with a fetch-only grant for that repo) when the repo's default
//! branch is somewhere the checkout isn't. The daemon then pulls by T3's auto-pull rules (only
//! a clean fast-forward of the checked-out default branch) and records what it did.
//!
//! Level-triggered, like following changes: every [`PULL_EVERY`] and whenever the store says
//! something landed on a default branch (a hint; a dropped one loses nothing), every mirrored
//! project with `auto_pull` and a `repo`, on a machine connected here, whose `git.head` isn't the
//! repo's default-branch head is asked to pull. Once per head, and again after [`RETRY`] while it
//! still isn't there (the checkout may have had local changes).

use super::code::{mint, repo_of};
use super::store;
use crate::Code;
use agent_proto::{Command, Entity, Project, Reply};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// How often projects are looked at when nothing lands.
pub(crate) const PULL_EVERY: Duration = Duration::from_secs(30);
/// How long before a project that didn't get to a head is asked again.
const RETRY: Duration = Duration::from_secs(5 * 60);
/// Who the daemon sees the command from.
const BY: &str = "roll";

/// Start asking (idempotent; needs a Tokio runtime).
pub(crate) fn start(code: &Arc<Code>) {
    if tokio::runtime::Handle::try_current().is_err() || code.agents.pulling.swap(true, Ordering::SeqCst) {
        return;
    }
    let weak = Arc::downgrade(code);
    let mut landed = code.store.subscribe_all();
    tokio::spawn(async move {
        loop {
            let every = match weak.upgrade() {
                Some(code) => code.agents.pull_every(),
                None => return,
            };
            let hinted = async {
                loop {
                    match landed.recv().await {
                        Ok(ev) if ev.ref_name.starts_with("refs/heads/") => return true,
                        Ok(_) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => return true,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return false,
                    }
                }
            };
            if let Ok(false) = tokio::time::timeout(every, hinted).await {
                return;
            }
            let Some(code) = weak.upgrade() else { return };
            for owner in code.agents.connected_owners() {
                ask(&code, &owner);
            }
        }
    });
}

/// The owner's auto-pull roll code projects on connected machines, and their machines.
fn projects(code: &Code, owner_id: &str) -> code_sql::Result<Vec<(String, Project)>> {
    let rows: Vec<(String, String)> = code.db.sql().query(
        "SELECT machine_id, json FROM agent_entities WHERE owner_id = ? AND kind = 'project'",
        [owner_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(rows
        .into_iter()
        .filter_map(|(m, j)| match serde_json::from_str(&j) {
            Ok(Entity::Project(p)) if p.settings.auto_pull && p.repo.is_some() && code.agents.online(&m) => Some((m, p)),
            _ => None,
        })
        .collect())
}

/// Ask every project of the owner that is behind its repo's default branch to pull.
fn ask(code: &Arc<Code>, owner_id: &str) {
    let projects = match projects(code, owner_id) {
        Ok(p) => p,
        Err(e) => return tracing::warn!(error = %e, "agents: reading projects to pull"),
    };
    for (machine_id, p) in projects {
        let Some(asked) = &p.repo else { continue };
        let Ok(r) = repo_of(code, asked) else { continue };
        if r.owner_id != owner_id {
            continue;
        }
        let head = match code.head(&r) {
            Ok(Some(cs)) => match code.store.derive_git(&r.id, &cs) {
                Ok((commit, _)) => commit.hex(),
                Err(e) => {
                    tracing::warn!(repo = %r.id, error = %e, "agents: the default branch's commit");
                    continue;
                }
            },
            _ => continue,
        };
        if p.git.as_ref().and_then(|g| g.head.as_deref()) == Some(head.as_str()) {
            continue;
        }
        let key = (machine_id.clone(), p.id.0.clone());
        {
            let mut sent = code.agents.pulls.lock();
            if sent.get(&key).is_some_and(|(h, at)| *h == head && at.elapsed() < RETRY) {
                continue;
            }
            sent.insert(key, (head.clone(), Instant::now()));
        }
        let Ok(Some(m)) = store::machine(code.db.sql(), &machine_id) else { continue };
        let grant = match mint(code, &m, &r, asked, None) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(repo = %r.id, error = %e, "agents: a grant to pull with");
                continue;
            }
        };
        let id = format!("pull-{}-{}-{:x}", p.id.0, &head[..12.min(head.len())], code_sql::now_ms());
        let code = code.clone();
        let command = Command::ProjectPull { project: p.id.clone() };
        tokio::spawn(async move {
            let reply = code.agents.command(&m.id, id, BY.into(), command, Some(grant), code.agents.command_timeout()).await;
            if let Reply::Error(e) = reply {
                tracing::debug!(machine = %m.id, error = ?e, "agents: asking a project to pull");
            }
        });
    }
}
