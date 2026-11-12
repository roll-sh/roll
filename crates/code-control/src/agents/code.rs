//! Agents in roll code (docs/code/agents.md, "Agents in roll code"): the canvas and the code in
//! the same place.
//!
//! - **Grants.** A daemon gets git access to one of its owner's repos only inside the command
//!   that needs it (`project.add` with `clone`, `thread.publish`): the relay mints a grant
//!   (`rlg_…`, stored as sha256) for that repo, that machine and that command's sender's rights
//!   (`write` only when the sender may write), for ten minutes. It opens git and nothing else,
//!   as `agent:<machine name>`, and is looked up on every request, so revoking the machine ends
//!   it at once.
//! - **Publishing.** The daemon pushes the thread's branch with its grant; roll code's git
//!   adapter opens or revises the branch's change, as for any push. The relay then reads that
//!   change, links it to the thread, and with `land` sends it to landing through the API on the
//!   sender's behalf (`POST /v1/code/changes/{id}/land`): the merge queue on a `queue` repo, the
//!   landing rules otherwise. Nothing goes around them; a blocked land is an answer.
//! - **Following.** A linked change's status is merged into the thread's mirrored record
//!   (`Thread.change`), re-read from the changes table (where checks, reviews, the queue and
//!   landings all end up, durably) for every unsettled link of the owners whose agents this node
//!   serves: every second, and sooner when the event feed hints that something changed. The feed
//!   only wakes the follower; it never says what changed, so a dropped event loses nothing.
//! - **Work items.** A thread takes a work item (claimed through the API, or one already
//!   claimed); its link records it (`Thread.work`) and who holds the claim. While the thread
//!   works (running, or idle for less than [`WORK_IDLE`], not archived, its machine connected),
//!   the follower keeps that claim's lease alive on its behalf; otherwise the lease runs out as
//!   any other. Publishing links the branch's change to the item (`PATCH
//!   /v1/code/changes/{id}` with `work_item`, as if the change had been opened with it), so the
//!   item goes to `review` and is done when the change lands.

use super::store::{self, LinkUpdate, MachineRow};
use crate::model::{Change, ChangeState, CheckStatus, LandPolicy, Owner, RepoRow, WorkState};
use crate::{Caller, Code, Error, Result, hash_token};
use agent_proto::{ChangeStatus, CodeGrant, Command, EntityKind, Landing, LandingStatus, RepoRef, Reply, StreamEvent, ThreadChange, WorkRef};
use axum::Router;
use axum::body::Body as AxumBody;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tower::ServiceExt;

/// Grant tokens start with this.
pub(crate) const GRANT_PREFIX: &str = "rlg_";
/// How long a grant works: long enough for a big clone after the command's 30 s reply wait.
const GRANT_TTL: Duration = Duration::from_secs(10 * 60);
/// How often linked changes are read again when nothing hints sooner.
pub(crate) const FOLLOW_EVERY: Duration = Duration::from_secs(1);
/// The least time between two rounds, however often the feed hints.
const MIN_GAP: Duration = Duration::from_millis(200);
/// Largest API answer read on a caller's behalf.
const MAX_ANSWER: usize = 16 << 20;
/// How long a thread may sit idle and still have its work item's claim kept alive.
pub(crate) const WORK_IDLE: Duration = Duration::from_secs(10 * 60);
/// Event kinds that may move a change.
const CHANGE_EVENTS: [&str; 5] = ["change", "check", "landed", "queue", "review"];

/// The API router, so the relay calls roll code's routes on a caller's behalf (with their token,
/// routed in a cluster) the way MCP tools do. Added as a request extension by `api::router`.
#[derive(Clone)]
pub struct ApiRouter(pub Router);

/// Call the API as the request with `headers` (its token and agent headers).
pub(super) async fn call(api: &ApiRouter, headers: &HeaderMap, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(path);
    for name in [header::AUTHORIZATION.as_str(), "x-roll-agent", "x-roll-user"] {
        if let Some(v) = headers.get(name) {
            b = b.header(name, v.clone());
        }
    }
    let req = match body {
        Some(v) => b.header(header::CONTENT_TYPE, "application/json").body(AxumBody::from(v.to_string())),
        None => b.body(AxumBody::empty()),
    };
    let Ok(req) = req else { return (StatusCode::BAD_REQUEST, json!({ "error": "bad request" })) };
    let resp = match api.0.clone().oneshot(req).await {
        Ok(r) => r,
        Err(e) => match e {},
    };
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), MAX_ANSWER).await.unwrap_or_default();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn api_of(api: Option<&ApiRouter>) -> Result<&ApiRouter> {
    api.ok_or_else(|| Error::Internal(anyhow::anyhow!("the agents relay has no API router")))
}

/// Percent-encode a path segment.
fn seg(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// An API error as this route's error, keeping its status.
fn passed(status: StatusCode, v: &Value) -> Error {
    let msg = v["error"].as_str().unwrap_or("roll code refused it").to_string();
    match status {
        StatusCode::UNAUTHORIZED => Error::Unauthorized,
        StatusCode::FORBIDDEN => Error::Forbidden(msg),
        StatusCode::NOT_FOUND => Error::NotFound(msg.trim_end_matches(" not found").to_string()),
        StatusCode::CONFLICT => Error::Conflict(msg),
        StatusCode::UNPROCESSABLE_ENTITY => Error::Unprocessable(msg),
        StatusCode::SERVICE_UNAVAILABLE => Error::Unavailable(msg),
        _ => Error::Invalid(msg),
    }
}

// ── grants ──

/// The live grant a git password is, if it is one.
pub(crate) fn grant(code: &Code, token: &str) -> Result<Option<store::GrantRow>> {
    Ok(store::grant_by_token(code.db.sql(), &hash_token(token))?)
}

/// A repo by the name a project knows it by, following a rename.
pub(super) fn repo_of(code: &Code, r: &RepoRef) -> Result<RepoRow> {
    match code.find_repo(&r.owner, &r.repo) {
        Err(Error::Moved(to)) => {
            let (o, n) = to.split_once('/').unwrap_or((&to, ""));
            code.find_repo(o, n)
        }
        found => found,
    }
}

/// The repo a command works on, checked: one of the machine's owner's, and `push` only for a
/// sender who may write it.
fn granted_repo(code: &Code, caller: &Caller, m: &MachineRow, asked: &RepoRef, push: bool) -> Result<RepoRow> {
    let r = repo_of(code, asked).map_err(|_| Error::NotFound(format!("repository {}/{}", asked.owner, asked.repo)))?;
    if r.owner_id != m.owner_id {
        return Err(Error::Invalid(format!("{}/{} isn't the machine's owner's: only their own repos open on their machines", asked.owner, asked.repo)));
    }
    if !Code::can_read(caller, &r) {
        return Err(Error::NotFound(format!("repository {}/{}", asked.owner, asked.repo)));
    }
    if push {
        code.check_write(caller, &r)?;
    }
    Ok(r)
}

/// A grant for `r`; with `push`, it may push that one branch (`refs/heads/<push>`).
pub(super) fn mint(code: &Code, m: &MachineRow, r: &RepoRow, asked: &RepoRef, push: Option<&str>) -> Result<CodeGrant> {
    let mut b = [0u8; 24];
    getrandom::fill(&mut b).expect("system randomness");
    let token = format!("{GRANT_PREFIX}{}", hex::encode(b));
    let expires_at = code_sql::now_ms() + GRANT_TTL.as_millis() as i64;
    let push_ref = push.map(code_store::full_ref);
    store::insert_grant(code.db.sql(), &hash_token(&token), &m.id, &r.id, push_ref.as_deref(), expires_at)?;
    // A connected repo's GitHub side, for a publish's fallback while roll can't be reached.
    let github = crate::github::link_of(r).filter(|l| l.connected && push.is_some()).map(|l| agent_proto::GrantGithub {
        owner: l.owner.clone(),
        repo: l.repo.clone(),
        url: l.url.trim_end_matches('/').to_string(),
        default_branch: r.default_branch.clone(),
    });
    Ok(CodeGrant { repo: asked.clone(), url: code.clone_url(r), token, write: push.is_some(), branch: push.map(str::to_string), expires_at: expires_at.max(0) as u64, github })
}

/// The repo a command works on and the grant for it, when it needs one. A clone's grant only
/// fetches, as does a pull's (`project.pull`, and `project.update` turning auto-pull on, which
/// pulls at once); a publish's pushes the thread's own branch (`refs/heads/<worktree branch>`)
/// and nothing else: never the default branch, another branch or a tag (`git.rs` checks each ref).
pub(super) async fn grant_for(code: &Code, caller: &Caller, m: &MachineRow, command: &Command) -> Result<Option<(CodeGrant, RepoRow)>> {
    let project_repo = |project: &agent_proto::ProjectId| -> Result<Option<RepoRef>> {
        Ok(match store::mirrored(code.db.sql(), &m.id, EntityKind::Project, project.as_str())? {
            Some(agent_proto::Entity::Project(agent_proto::Project { repo, .. })) => repo,
            _ => None,
        })
    };
    let (asked, branch) = match command {
        Command::ProjectAdd { clone: Some(repo), .. } => (repo.clone(), None),
        Command::ProjectPull { project } | Command::ProjectUpdate { project, settings: Some(agent_proto::ProjectSettings { auto_pull: true, .. }), .. } => {
            match project_repo(project)? {
                Some(repo) => (repo, None),
                None => return Ok(None),
            }
        }
        Command::ThreadPublish { thread, .. } => {
            let Some(agent_proto::Entity::Thread(t)) = store::mirrored(code.db.sql(), &m.id, EntityKind::Thread, thread.as_str())? else { return Ok(None) };
            let Some(agent_proto::Entity::Project(p)) = store::mirrored(code.db.sql(), &m.id, EntityKind::Project, t.project.as_str())? else { return Ok(None) };
            let Some(repo) = p.repo else { return Ok(None) };
            match t.worktree {
                Some(w) => (repo, Some(w.branch)),
                // A thread in the folder of a checkout with no commits yet publishes the repo's
                // first commit (`first_commit`): onto its default branch while it has none, or as
                // a change when nothing lands on it directly (a queue repo, required reviews).
                None if t.cwd == p.path && p.git.as_ref().is_some_and(|g| g.head.is_none()) => {
                    let r = granted_repo(code, caller, m, &repo, true)?;
                    if code.head(&r)?.is_some() {
                        return Err(Error::Invalid(format!(
                            "{} has commits now: pull the project, then fork this thread into a worktree to publish it",
                            r.full_name()
                        )));
                    }
                    let branch = if r.land_policy == LandPolicy::Queue || code.direct_lands_need_review(&r).await? {
                        FIRST_COMMIT_BRANCH.to_string()
                    } else {
                        r.default_branch.clone()
                    };
                    return Ok(Some((mint(code, m, &r, &repo, Some(&branch))?, r)));
                }
                // The daemon refuses the rest: only a thread in its own worktree publishes.
                None => return Ok(None),
            }
        }
        _ => return Ok(None),
    };
    let r = granted_repo(code, caller, m, &asked, branch.is_some())?;
    let push_ref = match branch {
        Some(b) if b.trim().is_empty() || b.starts_with("refs/") => return Err(Error::Invalid(format!("the thread's branch {b:?} can't be published"))),
        Some(b) if b == r.default_branch => {
            return Err(Error::Invalid(format!("the thread works on {}'s default branch {b}: only a branch of its own is published", r.full_name())));
        }
        b => b,
    };
    Ok(Some((mint(code, m, &r, &asked, push_ref.as_deref())?, r)))
}

// ── a thread's change ──

fn cut(s: String) -> String {
    if s.chars().count() <= 200 { s } else { format!("{}…", s.chars().take(199).collect::<String>()) }
}

/// A change as the canvas shows it on its thread.
pub(crate) fn thread_change(code: &Code, r: &RepoRow, c: &Change) -> ThreadChange {
    let named = |f: fn(CheckStatus) -> bool| c.checks.iter().filter(|k| f(k.status)).map(|k| k.name.as_str()).collect::<Vec<_>>().join(", ");
    let failing = named(|s| s == CheckStatus::Failure);
    let running = named(|s| matches!(s, CheckStatus::Queued | CheckStatus::Running));
    let (status, detail) = match c.state {
        ChangeState::Landed => (ChangeStatus::Landed, None),
        ChangeState::Closed => (ChangeStatus::Closed, None),
        ChangeState::Queued => (ChangeStatus::Queued, c.queue.as_ref().and_then(|q| q.position).map(|p| format!("position {p} in the merge queue"))),
        ChangeState::Conflict => (ChangeStatus::Conflict, c.conflict.as_ref().map(|k| k.paths.join(", "))),
        ChangeState::Open if !failing.is_empty() => (ChangeStatus::Failing, Some(failing)),
        ChangeState::Open if !running.is_empty() => (ChangeStatus::Checking, Some(running)),
        ChangeState::Open => match c.blockers.first() {
            Some(b) => (ChangeStatus::Blocked, Some(b.message.clone())),
            None => (ChangeStatus::Ready, None),
        },
    };
    ThreadChange {
        repo: r.full_name(),
        number: c.number,
        status,
        url: format!("{}/changes/{}", code.web_url(r), c.number),
        detail: detail.filter(|d| !d.is_empty()).map(cut),
    }
}

/// A change's status now; `None` if it's gone (its repo was deleted).
fn current(code: &Code, change_id: &str) -> Result<Option<ThreadChange>> {
    let Some(c) = code.db.change(change_id)? else { return Ok(None) };
    let c = code.change_view(c)?;
    let r = code.repo_by_id(&c.repo_id)?;
    Ok(Some(thread_change(code, &r, &c)))
}

/// Update a thread's link and publish its record, in cursor order.
async fn relink(code: &Code, owner_id: &str, machine_id: &str, thread: &str, update: LinkUpdate<'_>) {
    let hub = code.agents.hub(owner_id);
    let _order = hub.order.lock().await;
    match store::update_link(code.db.sql(), owner_id, machine_id, thread, update) {
        Ok(Some(m)) => {
            let _ = hub.events.send(Arc::new(StreamEvent::Entity(Box::new(m))));
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(thread, error = %e, "agents: linking a thread to roll code"),
    }
}

/// What the relay adds to the reply of a `thread.publish` in a roll code repo `r`: the branch's
/// change (linked to the thread), and with `land` what sending it to landing did.
pub(super) async fn published(code: &Code, api: Option<&ApiRouter>, headers: &HeaderMap, m: &MachineRow, command: &Command, r: &RepoRow, reply: Reply) -> Reply {
    let (Command::ThreadPublish { thread, land, .. }, Reply::Published(mut p)) = (command, reply.clone()) else { return reply };
    let (thread, land) = (thread.as_str(), *land);
    if p.branch == r.default_branch {
        first_commit(code, r, &mut p);
        return Reply::Published(p);
    }
    let change = match code.db.change_by_branch(&r.id, &p.branch) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "agents: reading a published branch's change");
            None
        }
    };
    let Some(c) = change else {
        // The daemon said already (roll's git couldn't be reached: queued on GitHub).
        if land && p.landing.is_none() {
            p.landing = Some(Landing {
                status: LandingStatus::Failed,
                message: format!("{} has no open change in {} (it may have landed already)", p.branch, r.full_name()),
                blockers: Vec::new(),
            });
        }
        return Reply::Published(p);
    };
    p.change = Some(c.number);
    p.url = Some(format!("{}/changes/{}", code.web_url(r), c.number));
    if let Some(api) = api {
        link_work(code, api, headers, m, thread, r, &c).await;
    }
    if let Ok(c) = code.change_view(c.clone()) {
        relink(code, &m.owner_id, &m.id, thread, LinkUpdate::Change { id: &c.id, view: &thread_change(code, r, &c) }).await;
    }
    if land {
        p.landing = Some(match api {
            Some(api) => send_to_land(api, headers, &c.id).await,
            None => Landing { status: LandingStatus::Failed, message: "the relay can't reach roll code's API".into(), blockers: Vec::new() },
        });
        if let Ok(Some(view)) = current(code, &c.id) {
            relink(code, &m.owner_id, &m.id, thread, LinkUpdate::Change { id: &c.id, view: &view }).await;
        }
    }
    Reply::Published(p)
}

/// The branch an empty repo's first commit goes on as a change, when nothing lands on the default
/// branch directly (`grant_for`).
const FIRST_COMMIT_BRANCH: &str = "roll/first-commit";

/// An empty repo's first commit, pushed onto its default branch (`grant_for`): it landed as it was
/// pushed (a push the landing rules turn into a change fails, so the daemon doesn't answer).
fn first_commit(code: &Code, r: &RepoRow, p: &mut agent_proto::Published) {
    let landed = || -> Result<bool> {
        let Some(head) = code.head(r)? else { return Ok(false) };
        Ok(code.store.derive_git(&r.id, &head)?.0.hex() == p.commit)
    };
    p.landing = Some(match landed() {
        Ok(true) => Landing { status: LandingStatus::Landed, message: format!("started {} with it", r.default_branch), blockers: Vec::new() },
        Ok(false) => Landing { status: LandingStatus::Failed, message: format!("the push didn't start {}", r.default_branch), blockers: Vec::new() },
        Err(e) => Landing { status: LandingStatus::Failed, message: e.to_string(), blockers: Vec::new() },
    });
}

/// The thread's work item is what its change does: linked on the sender's behalf (`PATCH
/// /v1/code/changes/{id}` with `work_item`), so landing the change completes it. Only an item
/// of the change's repo; a change already doing it is left as it is.
async fn link_work(code: &Code, api: &ApiRouter, headers: &HeaderMap, m: &MachineRow, thread: &str, r: &RepoRow, c: &Change) {
    let work = match store::mirrored(code.db.sql(), &m.id, EntityKind::Thread, thread) {
        Ok(Some(agent_proto::Entity::Thread(t))) => t.work,
        _ => None,
    };
    let Some(work) = work.filter(|w| w.repo == r.full_name()) else { return };
    if c.work_item.as_deref() == Some(work.id.as_str()) {
        return;
    }
    let (status, v) = call(api, headers, Method::PATCH, &format!("/v1/code/changes/{}", seg(&c.id)), Some(json!({ "work_item": work.id }))).await;
    if !status.is_success() {
        tracing::warn!(change = %c.id, work = %work.id, %status, error = %v["error"], "agents: linking a published change to its thread's work item");
    }
}

/// `POST /v1/code/changes/{id}/land` on the sender's behalf, as a [`Landing`].
async fn send_to_land(api: &ApiRouter, headers: &HeaderMap, change_id: &str) -> Landing {
    let (status, v) = call(api, headers, Method::POST, &format!("/v1/code/changes/{}/land", seg(change_id)), Some(json!({}))).await;
    let error = v["error"].as_str().unwrap_or("").to_string();
    let (status, message, blockers) = match status {
        StatusCode::OK => (LandingStatus::Landed, format!("landed as {}", v["changeset"].as_str().map(|s| &s[..s.len().min(15)]).unwrap_or("a changeset")), Vec::new()),
        StatusCode::ACCEPTED => (LandingStatus::Queued, format!("in the merge queue at position {}", v["position"].as_u64().unwrap_or(0)), Vec::new()),
        StatusCode::UNPROCESSABLE_ENTITY => {
            let blockers = v["blockers"].as_array().map(|b| b.iter().filter_map(|b| b["message"].as_str().map(str::to_string)).collect()).unwrap_or_default();
            (LandingStatus::Blocked, if error.is_empty() { "it can't land yet".into() } else { error }, blockers)
        }
        StatusCode::CONFLICT if v["status"] == "conflict" => (LandingStatus::Conflict, if error.is_empty() { "it conflicts with the default branch".into() } else { error }, Vec::new()),
        s => (LandingStatus::Failed, if error.is_empty() { s.to_string() } else { error }, Vec::new()),
    };
    Landing { status, message, blockers }
}

// ── following ──

/// Start following linked changes (idempotent; needs a Tokio runtime).
pub(crate) fn follow(code: &Arc<Code>) {
    if tokio::runtime::Handle::try_current().is_err() || code.agents.following.swap(true, Ordering::SeqCst) {
        return;
    }
    let weak = Arc::downgrade(code);
    let mut feed = code.subscribe();
    tokio::spawn(async move {
        loop {
            let every = match weak.upgrade() {
                Some(code) => code.agents.follow_every(),
                None => return,
            };
            // A hint, never the source: what changed is read from the changes table.
            let hinted = async {
                loop {
                    match feed.recv().await {
                        Ok(ev) if CHANGE_EVENTS.contains(&ev.kind) => return true,
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
            // Owners with something to follow whose agents this node serves: one query a round.
            let owners = match store::following_owners(code.db.sql()) {
                Ok(o) => o,
                Err(e) => {
                    tracing::warn!(error = %e, "agents: reading who has linked changes");
                    Vec::new()
                }
            };
            let hubs = code.agents.hub_owners();
            for owner in owners.iter().filter(|o| hubs.contains(o)) {
                let here = code.cluster().is_none_or(|c| c.owns(&format!("{}{owner}", crate::cluster::AGENTS_KEY)));
                if here {
                    follow_owner(&code, owner).await;
                    keep_claims(&code, owner);
                }
            }
            drop(code);
            // A busy feed hints constantly: rounds stay a little apart.
            tokio::time::sleep(every.min(MIN_GAP)).await;
        }
    });
}

/// Bring every unsettled change link of an owner up to date.
pub(super) async fn follow_owner(code: &Code, owner_id: &str) {
    let links = match store::unsettled_links(code.db.sql(), owner_id) {
        Ok(l) => l,
        Err(e) => return tracing::warn!(error = %e, "agents: reading linked changes"),
    };
    for l in links {
        let Some(id) = &l.change_id else { continue };
        let view = match current(code, id) {
            Ok(Some(v)) => v,
            // The change is gone with its repo: it won't move again.
            Ok(None) => match &l.change {
                Some(last) => ThreadChange { status: ChangeStatus::Closed, detail: None, ..last.clone() },
                None => continue,
            },
            Err(e) => {
                tracing::warn!(change = %id, error = %e, "agents: reading a linked change");
                continue;
            }
        };
        if l.change.as_ref() != Some(&view) || view.status.is_settled() {
            relink(code, owner_id, &l.machine_id, &l.thread, LinkUpdate::Change { id, view: &view }).await;
        }
        if view.status.is_settled() {
            change_ended(code, &l, id, view.status == ChangeStatus::Landed).await;
        }
    }
}

/// Tell the thread's daemon that its change landed or closed, once (the link stops being
/// followed), so the daemon can settle the thread by its project's rules (docs/code/agents.md,
/// "Settling"). Best effort: a machine that is offline misses it, and inactivity settles the
/// thread later.
async fn change_ended(code: &Code, l: &store::LinkRow, change: &str, landed: bool) {
    let command = Command::ThreadChangeEnded { thread: agent_proto::ThreadId(l.thread.clone()), landed };
    let id = format!("change-ended-{}-{change}", l.thread);
    let reply = code.agents.command(&l.machine_id, id, "roll".into(), command, None, std::time::Duration::from_secs(10)).await;
    if let Reply::Error(e) = reply {
        tracing::debug!(thread = %l.thread, error = ?e, "agents: telling a thread its change ended");
    }
}

/// Keep the claims of an owner's working threads alive; stop following a claim once it isn't
/// the claimant's any more (it lapsed, was released, taken over, or went to review or done).
pub(super) fn keep_claims(code: &Code, owner_id: &str) {
    let links = match store::claimed_links(code.db.sql(), owner_id) {
        Ok(l) => l,
        Err(e) => return tracing::warn!(error = %e, "agents: reading kept claims"),
    };
    for l in links {
        let (Some(work_id), Some(claimant)) = (&l.work_id, &l.claimant) else { continue };
        let key = (l.machine_id.clone(), l.thread.clone());
        let held = match code.db.work(work_id) {
            Ok(Some(w)) if working(code, &l) => code.keep_claim(&w, claimant),
            Ok(Some(w)) => Ok(w.state == WorkState::Claimed && w.claimed_by.as_deref() == Some(claimant.as_str()) && w.lease_until.is_some_and(|u| u >= crate::db::now())),
            Ok(None) => Ok(false),
            Err(e) => Err(e.into()),
        };
        match held {
            Ok(true) => {}
            Ok(false) => {
                code.agents.working.lock().remove(&key);
                if let Err(e) = store::forget_claimant(code.db.sql(), &l.machine_id, &l.thread, claimant) {
                    tracing::warn!(thread = %l.thread, error = %e, "agents: letting go of a kept claim");
                }
            }
            Err(e) => tracing::warn!(work = %work_id, error = %e, "agents: keeping a thread's claim alive"),
        }
    }
}

/// Whether a thread still works on its item: its machine is connected here, it isn't archived,
/// and it is busy or was last seen busy less than [`WORK_IDLE`] ago (by this node's clock).
fn working(code: &Code, l: &store::LinkRow) -> bool {
    if !code.agents.online(&l.machine_id) {
        return false;
    }
    let Ok(Some(agent_proto::Entity::Thread(t))) = store::mirrored(code.db.sql(), &l.machine_id, EntityKind::Thread, &l.thread) else { return false };
    if t.archived_at.is_some() {
        return false;
    }
    let now = std::time::Instant::now();
    let mut seen = code.agents.working.lock();
    let last = seen.entry((l.machine_id.clone(), l.thread.clone())).or_insert(now);
    if t.status.is_busy() || t.active_run.is_some() {
        *last = now;
    }
    now.duration_since(*last) < code.agents.work_idle()
}

// ── routes ──

/// `POST /v1/code/agents/{owner}/machines/{machine}/projects`: create a roll code repo (or take
/// the one of that name) and clone it onto the machine as a project, in one step.
pub(super) async fn create_project(code: &Arc<Code>, api: Option<&ApiRouter>, caller: &Caller, headers: &HeaderMap, o: &Owner, m: &MachineRow, b: &Value) -> Result<Reply> {
    let api = api_of(api)?;
    let id = b.get("id").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty() && s.len() <= 128).ok_or_else(|| Error::Invalid("id must be 1 to 128 characters".into()))?;
    let repo = b.get("repo").filter(|r| r.is_object()).ok_or_else(|| Error::Invalid("repo is required: {name, description?, visibility?}".into()))?;
    let name = repo.get("name").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| Error::Invalid("repo.name is required".into()))?;
    let mut input = json!({ "owner": o.name, "name": name });
    for k in ["description", "visibility", "default_branch"] {
        if let Some(v) = repo.get(k).filter(|v| !v.is_null()) {
            input[k] = v.clone();
        }
    }
    let (status, v) = call(api, headers, Method::POST, "/v1/code/repos", Some(input)).await;
    // A retry finds the repo it made: it's cloned like any other.
    if !status.is_success() && status != StatusCode::CONFLICT {
        return Err(passed(status, &v));
    }
    // A repo with no history gets a first commit (a README), so the clone has a branch to put
    // worktrees on and threads can publish from the start.
    let path = format!("/v1/code/repos/{}/{}", seg(&o.name), seg(name));
    let (status, r) = call(api, headers, Method::GET, &path, None).await;
    if !status.is_success() {
        return Err(passed(status, &r));
    }
    if r["head"].is_null() {
        let mut readme = format!("# {name}\n");
        if let Some(d) = r["description"].as_str().filter(|d| !d.trim().is_empty()) {
            readme.push_str(&format!("\n{}\n", d.trim()));
        }
        let first = json!({ "base": null, "message": format!("Start {name}"), "files": { "README.md": { "content": readme } } });
        let (status, v) = call(api, headers, Method::POST, &format!("{path}/land"), Some(first)).await;
        // Another request may have started it a moment ago: that's as good.
        if !status.is_success() && status != StatusCode::CONFLICT {
            return Err(passed(status, &v));
        }
    }
    let path = match b.get("path") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(p)) => p.clone(),
        Some(_) => return Err(Error::Invalid("path must be a string".into())),
    };
    let label = match b.get("name") {
        None | Some(Value::Null) => None,
        Some(Value::String(n)) => Some(n.clone()),
        Some(_) => return Err(Error::Invalid("name must be a string".into())),
    };
    let command = Command::ProjectAdd { path, name: label, clone: Some(RepoRef { owner: o.name.clone(), repo: name.to_string() }) };
    let grant = grant_for(code, caller, m, &command).await?.map(|(g, _)| g);
    Ok(code.machine_command(m, id.to_string(), super::api::sender(caller), command, grant).await)
}

/// A work item from the API (`GET` one, or `claim` the next), as the thread's [`WorkRef`].
fn work_ref(v: &Value) -> Option<WorkRef> {
    Some(WorkRef { id: v["id"].as_str()?.into(), repo: v["repo"].as_str()?.into(), number: v["number"].as_u64()?, title: v["title"].as_str().unwrap_or("").into() })
}

/// `POST /v1/code/agents/{owner}/threads/{thread}/work`: the thread takes a work item: `work`
/// (`wi_…` or `owner/repo#N`, already claimed or not) or the next one `claim`ed from `repo`.
/// Answers the work item, or 204 when there's nothing to claim.
pub(super) async fn take_work(code: &Code, api: Option<&ApiRouter>, caller: &Caller, headers: &HeaderMap, o: &Owner, thread: &str, b: &Value) -> Result<Option<Value>> {
    let api = api_of(api)?;
    let (machine_id, _) = store::thread_of(code.db.sql(), &o.id, thread)?.ok_or_else(|| Error::NotFound(format!("thread {thread}")))?;
    let machine = store::machine(code.db.sql(), &machine_id)?.filter(|m| !m.revoked).ok_or_else(|| Error::NotFound(format!("thread {thread}")))?;
    let s = |k: &str| b.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
    let item = match (s("work"), s("repo")) {
        (Some(w), _) => {
            let path = match w.rsplit_once('#') {
                Some((repo, n)) => {
                    let (ro, rn) = repo.split_once('/').ok_or_else(|| Error::Invalid("work is \"wi_…\" or \"owner/repo#N\"".into()))?;
                    format!("/v1/code/repos/{}/{}/work/{}", seg(ro), seg(rn), seg(n))
                }
                None => format!("/v1/code/work/{}", seg(w)),
            };
            let (status, v) = call(api, headers, Method::GET, &path, None).await;
            if !status.is_success() {
                return Err(passed(status, &v));
            }
            v
        }
        (None, Some(repo)) => {
            let (ro, rn) = repo.split_once('/').ok_or_else(|| Error::Invalid("repo is \"owner/name\"".into()))?;
            let agent = s("agent").map(str::to_string).or_else(|| caller.agent().map(|a| format!("agent:{a}"))).unwrap_or_else(|| format!("agent:{}", machine.name));
            let mut body = json!({ "agent": agent });
            for k in ["labels", "lease_s"] {
                if let Some(v) = b.get(k).filter(|v| !v.is_null()) {
                    body[k] = v.clone();
                }
            }
            let (status, v) = call(api, headers, Method::POST, &format!("/v1/code/repos/{}/{}/work/claim", seg(ro), seg(rn)), Some(body)).await;
            if status == StatusCode::NO_CONTENT {
                return Ok(None);
            }
            if !status.is_success() {
                return Err(passed(status, &v));
            }
            v
        }
        (None, None) => return Err(Error::Invalid("give work (\"wi_…\" or \"owner/repo#N\") or repo to claim from".into())),
    };
    let w = work_ref(&item).ok_or_else(|| Error::Internal(anyhow::anyhow!("an unreadable work item")))?;
    if w.repo.split_once('/').map(|(owner, _)| owner) != Some(o.name.as_str()) {
        return Err(Error::Invalid(format!("{} isn't {}'s: a thread takes work from its owner's repos", w.repo, o.name)));
    }
    // A claim held when the thread takes the item is kept alive while the thread works.
    let claimant = (item["state"] == "claimed").then(|| item["claimed_by"].as_str()).flatten();
    relink(code, &o.id, &machine.id, thread, LinkUpdate::Work { item: Some(&w), claimant }).await;
    Ok(Some(item))
}

/// `DELETE …/threads/{thread}/work`: the thread lets go of its work item (the claim stays the
/// claimant's to release).
pub(super) async fn drop_work(code: &Code, o: &Owner, thread: &str) -> Result<()> {
    let (machine_id, _) = store::thread_of(code.db.sql(), &o.id, thread)?.ok_or_else(|| Error::NotFound(format!("thread {thread}")))?;
    relink(code, &o.id, &machine_id, thread, LinkUpdate::Work { item: None, claimant: None }).await;
    Ok(())
}
