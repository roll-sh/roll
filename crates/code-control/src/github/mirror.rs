//! roll → GitHub: the default branch, pushed as one fast-forward that carries every landing
//! since the last push. `code_github::Mirror` decides when; this task does the pushing.

use super::{Hub, link_of, update_link};
use crate::Code;
use crate::db::now;
use code_git::{Credentials, Object, ObjectId, RefUpdate};
use code_github::{Mirror, MirrorAction, PushFailure};
use code_store::Oid;
use futures::StreamExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

pub(crate) enum Cmd {
    /// GitHub's default branch was seen at this sha (`None`: it's gone).
    Observed(Option<String>),
}

/// Start the repo's mirror if it isn't running.
pub(crate) fn start(code: &Arc<Code>, repo_id: &str) {
    let Some(hub) = code.github_hub().cloned() else { return };
    let rx = {
        let mut mirrors = hub.mirrors.lock();
        if mirrors.get(repo_id).is_some_and(|tx| !tx.is_closed()) {
            return;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        mirrors.insert(repo_id.to_string(), tx);
        rx
    };
    let (code, repo_id) = (code.clone(), repo_id.to_string());
    tokio::spawn(async move {
        run(code, hub, repo_id, rx).await;
    });
}

pub(crate) fn observed(code: &Code, repo_id: &str, sha: Option<String>) {
    if let Some(hub) = code.github_hub()
        && let Some(tx) = hub.mirrors.lock().get(repo_id)
    {
        let _ = tx.send(Cmd::Observed(sha));
    }
}

/// "Is `a` an ancestor of `d`?" over roll's history, by git commit id.
fn ancestry<'a>(code: &'a Code, repo: &'a str) -> impl Fn(&str, &str) -> bool + 'a {
    move |a, d| {
        let cs = |s: &str| ObjectId::from_hex(s.as_bytes()).and_then(|o| code.store.changeset_of_git(repo, &Oid(o.0)).ok().flatten());
        match (cs(a), cs(d)) {
            (Some(a), Some(d)) => code.store.is_ancestor(repo, &a, &d).unwrap_or(false),
            _ => false,
        }
    }
}

enum PushError {
    Rejected(Option<String>),
    Transient(String),
}

async fn run(code: Arc<Code>, hub: Arc<Hub>, repo_id: String, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    let Ok(r) = code.repo_by_id(&repo_id) else { return };
    let Some(link) = link_of(&r) else { return };
    let mut default_ref = code_store::full_ref(&r.default_branch);
    let mut landed = code.store.subscribe(&repo_id);
    let mut m = Mirror::new(hub.opts.mirror.clone(), link.remote_head.clone());
    let anc = ancestry(&code, &repo_id);
    // Whatever landed while nobody was pushing (during the import, or before a restart).
    if let Ok(Some(h)) = code.head(&r)
        && let Ok((commit, _)) = code.store.derive_git(&repo_id, &h)
    {
        m.landed(commit.hex(), Instant::now());
        m.resolve(&anc);
    }
    let mut status = Status::default();
    loop {
        let now_i = Instant::now();
        let wait = match m.poll(now_i) {
            MirrorAction::Idle | MirrorAction::Blocked(_) => None,
            MirrorAction::Wait(d) => Some(d),
            MirrorAction::Push { from, to } => {
                match push(&code, &hub, &repo_id, &default_ref, from.as_deref(), &to).await {
                    Ok(()) => {
                        m.push_succeeded(&to, Instant::now());
                        let t = now();
                        let _ = update_link(&code, &repo_id, |l| {
                            l.remote_head = Some(to.clone());
                            l.mirror.last_push_at = Some(t);
                        });
                    }
                    Err(PushError::Rejected(remote)) => m.push_failed(PushFailure::Rejected { remote }, Instant::now(), &anc),
                    Err(PushError::Transient(e)) => {
                        tracing::warn!(repo = %repo_id, error = %e, "mirroring to GitHub");
                        m.push_failed(PushFailure::Transient(e), Instant::now(), &anc)
                    }
                }
                // The default branch may have been renamed.
                if let Ok(r) = code.repo_by_id(&repo_id) {
                    default_ref = code_store::full_ref(&r.default_branch);
                }
                status.write(&code, &repo_id, &m, false);
                continue;
            }
        };
        status.write(&code, &repo_id, &m, false);
        tokio::select! {
            ev = landed.recv() => match ev {
                Ok(l) if l.ref_name == default_ref => {
                    m.landed(l.git_commit.hex(), Instant::now());
                    m.resolve(&anc);
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if let Ok(r) = code.repo_by_id(&repo_id)
                        && let Ok(Some(h)) = code.head(&r)
                        && let Ok((commit, _)) = code.store.derive_git(&repo_id, &h)
                    {
                        m.landed(commit.hex(), Instant::now());
                        m.resolve(&anc);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            cmd = rx.recv() => match cmd {
                Some(Cmd::Observed(sha)) => {
                    m.remote_observed(sha.as_deref(), &anc);
                    let _ = update_link(&code, &repo_id, |l| l.remote_head = sha.clone());
                }
                None => break,
            },
            _ = tokio::time::sleep(wait.unwrap_or(Duration::from_secs(3600))) => {}
        }
    }
    status.write(&code, &repo_id, &m, true);
}

/// Push `to` over `from` (GitHub's expected value) with every object GitHub doesn't have.
async fn push(code: &Code, hub: &Hub, repo: &str, ref_name: &str, from: Option<&str>, to: &str) -> Result<(), PushError> {
    let t = |e: &dyn std::fmt::Display| PushError::Transient(e.to_string());
    let r = code.repo_by_id(repo).map_err(|e| t(&e))?;
    let link = link_of(&r).ok_or_else(|| PushError::Transient("not connected".into()))?;
    if !link.connected {
        return Err(PushError::Transient("the GitHub App lost access".into()));
    }
    let creds = hub.client.git_credentials(link.installation_id, &link.owner, &link.repo).await.map_err(|e| t(&e))?;
    let auth = Credentials { username: creds.username, password: creds.password };
    let new = ObjectId::from_hex(to.as_bytes()).ok_or_else(|| PushError::Transient(format!("bad commit {to}")))?;
    let old = from.and_then(|f| ObjectId::from_hex(f.as_bytes()));
    let haves: Vec<Oid> = old.iter().map(|o| Oid(o.0)).filter(|o| code.store.changeset_of_git(repo, o).ok().flatten().is_some()).collect();
    let list = code.store.objects_for_fetch(repo, &[Oid(new.0)], &haves).map_err(|e| t(&e))?;
    let reads = futures::stream::iter(list.into_iter().map(|(oid, _)| async move {
        let obj = code.store.git_object(repo, &oid).await?;
        let (kind, data) = obj.ok_or_else(|| anyhow::anyhow!("object {oid} is missing"))?;
        anyhow::Ok(Object { id: ObjectId(oid.0), kind: git_kind(kind), data })
    }))
    .buffered(32)
    .collect::<Vec<_>>()
    .await;
    let objects: Vec<Object> = reads.into_iter().collect::<anyhow::Result<_>>().map_err(|e| t(&e))?;
    let update = RefUpdate { name: ref_name.to_string(), old: old.unwrap_or(ObjectId::ZERO), new };
    let report = hub.git.push(&creds.url, Some(&auth), &[update], &objects).await.map_err(|e| t(&e))?;
    if report.ok() {
        return Ok(());
    }
    let reason = report
        .refs
        .iter()
        .find_map(|s| s.result.as_ref().err().cloned())
        .or_else(|| report.unpack.as_ref().err().cloned())
        .unwrap_or_else(|| "push failed".into());
    let stale = ["non-fast-forward", "fetch first", "stale", "cannot lock ref", "failed to update ref", "already exists"];
    if stale.iter().any(|s| reason.contains(s)) {
        // Where GitHub really is, so the mirror can tell a lost response from a real fork.
        let remote = hub.git.ls_refs(&creds.url, Some(&auth)).await.ok().and_then(|refs| refs.get(ref_name).map(|r| r.oid.to_hex()));
        return Err(PushError::Rejected(remote));
    }
    Err(PushError::Transient(reason))
}

fn git_kind(k: code_store::Kind) -> code_git::Kind {
    match k {
        code_store::Kind::Commit => code_git::Kind::Commit,
        code_store::Kind::Tree => code_git::Kind::Tree,
        code_store::Kind::Blob => code_git::Kind::Blob,
        code_store::Kind::Tag => code_git::Kind::Tag,
    }
}

/// Writes `Repo.github.mirror` when it changes: at once for a new state or error, at most once a
/// second for the `behind` count (landings can come by the thousand).
#[derive(Default)]
struct Status {
    last: Option<super::MirrorView>,
    written_at: Option<Instant>,
}

impl Status {
    fn write(&mut self, code: &Code, repo: &str, m: &Mirror, force: bool) {
        let s = m.status(Instant::now());
        let behind = match (&s.head, &s.mirrored) {
            (Some(h), _) if s.mirrored.as_ref() == Some(h) => 0,
            (Some(h), m) => {
                let generation = |sha: &str| {
                    ObjectId::from_hex(sha.as_bytes())
                        .and_then(|o| code.store.changeset_of_git(repo, &Oid(o.0)).ok().flatten())
                        .and_then(|cs| code.store.meta(repo, &cs).ok().flatten())
                        .map(|meta| meta.generation)
                };
                let head = generation(h).unwrap_or(0);
                match m.as_deref().and_then(generation) {
                    Some(g) => head.saturating_sub(g),
                    None => head + 1,
                }
            }
            (None, _) => 0,
        };
        let (state, error) = if let Some(b) = &s.blocked {
            let why = match b.kind {
                code_github::BlockedKind::RemoteAhead => "GitHub has commits roll hasn't ingested yet".to_string(),
                code_github::BlockedKind::Diverged => "GitHub and roll diverged".to_string(),
            };
            ("paused", Some(why))
        } else if s.failures > 0 {
            ("error", s.last_error.clone())
        } else if behind > 0 {
            ("pending", None)
        } else {
            ("ok", None)
        };
        let view = super::MirrorView { state: state.into(), last_push_at: None, behind, error };
        let changed_state = self.last.as_ref().is_none_or(|l| l.state != view.state || l.error != view.error);
        let changed = self.last.as_ref() != Some(&view);
        let due = self.written_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
        if !(force || changed_state || (changed && due)) {
            return;
        }
        let _ = update_link(code, repo, |l| {
            // A disconnected repo (App uninstalled or suspended) keeps the state the disconnect
            // set; a mirror task still winding down mustn't report it as ok.
            if !l.connected {
                return;
            }
            // A block the webhook side explained (which files overlap) keeps its explanation.
            let explained = view.state == "paused" && l.mirror.state == "paused" && l.mirror.error.is_some();
            if !explained {
                l.mirror.error = view.error.clone();
            }
            l.mirror.state = view.state.clone();
            l.mirror.behind = view.behind;
        });
        self.last = Some(view);
        self.written_at = Some(Instant::now());
    }
}
