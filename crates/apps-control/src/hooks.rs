//! Deploy hooks (docs/apps/README.md, "Deploy hooks"; Render's deploy hooks): a secret URL per
//! environment that deploys it, for CI or a registry's webhook, with no API token. Only the
//! token's hash is stored; the URL deploys the service's own source, and for an image service
//! optionally another tag or digest of the same repository, never another image.

use crate::model::{By, Deployment, Source, Trigger, image_repository, random};
use crate::{Apps, Error, Result};
use serde_json::{Value, json};
use std::sync::Arc;

impl Apps {
    /// `POST …/environments/{env}/hook`: a new hook (the old one stops working); its path, once.
    pub async fn put_hook(self: &Arc<Self>, service: &str, env: &str) -> Result<Value> {
        let (sid, e) = (service.to_string(), env.to_string());
        self.db(move |s| s.environment(&sid, &e)).await?;
        let token = new_token();
        let (sid, e, hash) = (service.to_string(), env.to_string(), crate::public::token_hash(&token));
        self.db(move |s| s.set_hook(&sid, &e, Some(&hash))).await?;
        Ok(json!({ "path": format!("/v1/apps/hooks/{service}/{env}/{token}") }))
    }

    /// `DELETE …/environments/{env}/hook`.
    pub async fn delete_hook(self: &Arc<Self>, service: &str, env: &str) -> Result<()> {
        let (sid, e) = (service.to_string(), env.to_string());
        self.db(move |s| {
            s.environment(&sid, &e)?;
            s.set_hook(&sid, &e, None)
        })
        .await
    }

    /// A hook token found where it shouldn't be (roll-access's leak scanning, `Access::on_leaked`):
    /// the hook stops working at once. Says which hook it was, or `None` when no live hook has it.
    pub fn revoke_leaked_hook(&self, token: &str) -> Option<String> {
        let row = self.store.environment_by_hook(&crate::public::token_hash(token)).ok()??;
        self.store.set_hook(&row.service, &row.name, None).ok()?;
        tracing::warn!(service = %row.service, env = %row.name, "apps: a leaked deploy hook was revoked");
        Some(format!("{}'s {} deploy hook", row.service, row.name))
    }

    /// `POST /v1/apps/hooks/{service}/{env}/{token}[?image=]`: deploy, as the hook. Anything
    /// that isn't a live hook is the same 404, so the URL tells a guesser nothing. A Git URL
    /// service's hook is also its repository's push webhook (GitHub's or GitLab's `push` event
    /// as `payload`): a push deploys every environment that follows the pushed branch at once,
    /// keyed by the commit as the five-minute watch's deploy is (a redelivery, or the watch, makes
    /// no second one); a deleted branch, a `[skip ci]` commit or a ping is `Ignored`. A branch no
    /// environment follows goes to roll previews once the repository agrees ([`GitPush`]).
    ///
    /// [`GitPush`]: crate::integrations::GitPush
    pub async fn run_hook(self: &Arc<Self>, service: &str, env: &str, token: &str, image: Option<String>, payload: Option<Webhook>) -> Result<HookOutcome> {
        let none = || Error::NotFound("no such hook".into());
        let (sid, e) = (service.to_string(), env.to_string());
        let (svc, row) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await.map_err(|_| none())?;
        let Some(hash) = row.hook_hash.as_deref() else { return Err(none()) };
        if !crate::public::same(&crate::public::token_hash(token), hash) {
            return Err(none());
        }
        let mut body = json!({ "message": "deploy hook" });
        if let (Source::Git { .. }, Some(w)) = (&svc.source, payload.as_ref()) {
            match push_of(w) {
                Push::Ping => return Ok(HookOutcome::Ignored("pong".into())),
                Push::Plain => {}
                Push::Ignored(why) => return Ok(HookOutcome::Ignored(why)),
                Push::Pull(pull) => {
                    let Source::Git { url, .. } = &svc.source else { unreachable!("matched above") };
                    let Some(pushes) = self.pushes.get() else { return Ok(HookOutcome::Ignored(format!("pull request {}: roll previews isn't on this node", pull.number))) };
                    return Ok(HookOutcome::Ignored(self.hand_pull_to_previews(pushes.clone(), service, url, pull).await));
                }
                Push::Branches(moved) => {
                    let Source::Git { url, .. } = &svc.source else { unreachable!("matched above") };
                    // One webhook for every environment (Render's, Railway's): each branch the push
                    // moved deploys every environment that follows it, whichever environment's hook
                    // it was posted to; a branch none follows is roll previews' (when it's on this
                    // node). Only a branch's head per the repository's refs deploys, so this hook
                    // can do no more than the five-minute watch would, sooner.
                    let sid = service.to_string();
                    let envs = self.db(move |s| s.environments(&sid)).await?;
                    let remote = self.git_remote(service, url).await;
                    let (mut said, mut deployed, mut failed) = (Vec::new(), Vec::new(), Vec::new());
                    // The repository's refs, once for the whole delivery.
                    let mut refs = None;
                    for p in moved {
                        let following: Vec<&crate::store::EnvRow> = envs.iter().filter(|e| !e.ephemeral && e.branch.as_deref() == Some(p.branch.as_str())).collect();
                        if following.is_empty() {
                            said.push(match self.pushes.get() {
                                Some(pushes) => self.hand_to_previews(pushes.clone(), service, url, p).await,
                                None => format!("a push to {}: no environment follows it", p.branch),
                            });
                            continue;
                        }
                        let Some(sha) = p.sha.clone() else {
                            said.push(format!("{} was deleted", p.branch));
                            continue;
                        };
                        // The body alone never names a commit: a leaked hook URL can't put an old
                        // commit, or a fork's, live, and a push a newer one overtook is that one's.
                        if refs.is_none() {
                            refs = Some(remote.ls_refs().await.map_err(|e| format!("{e:#}")));
                        }
                        match refs.as_ref().expect("read above") {
                            Ok(r) => match r.get(&format!("refs/heads/{}", p.branch)).map(|r| r.oid.to_string()) {
                                Some(now) if now == sha => {}
                                Some(now) => {
                                    said.push(format!("the repository has {} at {}, not {}: only its head deploys", p.branch, &now[..12], &sha[..12]));
                                    continue;
                                }
                                None => {
                                    said.push(format!("the repository has no branch {}: not deployed", p.branch));
                                    continue;
                                }
                            },
                            Err(e) => {
                                said.push(format!("{} couldn't be checked against the repository ({e}): not deployed", p.branch));
                                continue;
                            }
                        }
                        // Its message as the repository has it (`[skip ci]` is the author's, not the
                        // body's), and roll.toml at it: once for every environment.
                        let message = crate::git_source::commit_message(&remote, &sha).await.unwrap_or_else(|_| p.message.clone());
                        let file = self.app_config_at(&svc, &sha).await;
                        for e in following {
                            if e.paused {
                                said.push(format!("{} is paused", e.name));
                                continue;
                            }
                            if let Some(r) = &e.held_by {
                                said.push(format!("{}'s deploys are held after the rollback {r}", e.name));
                                continue;
                            }
                            if let Some(why) = self.git_push_skip(&svc, e, &sha, &message, file.as_ref()).await {
                                said.push(format!("{}: {why}", e.name));
                                continue;
                            }
                            let first = message.lines().next().unwrap_or("").chars().take(500).collect::<String>();
                            let body = json!({ "revision": sha, "message": if first.is_empty() { format!("push to {}", p.branch) } else { first } });
                            let by = By { trigger: Trigger::Push, actor: Some("deploy hook".into()) };
                            match self.deploy(service, &e.name, body, by, None, Some(&format!("git:{}:{sha}", e.name))).await {
                                Ok(d) => deployed.push(d),
                                Err(err) => {
                                    said.push(format!("{}: {err}", e.name));
                                    failed.push(err);
                                }
                            }
                        }
                    }
                    // The answer is this environment's deploy when it made one, else another's; a
                    // deploy that failed with none made is the answer's error (the platform shows
                    // a failed delivery), never a quiet `ignored`.
                    deployed.sort_by_key(|d| d.environment != env);
                    return match (deployed.into_iter().next(), failed.into_iter().next()) {
                        (Some(d), _) => Ok(HookOutcome::Deployed(Box::new(d))),
                        (None, Some(err)) => Err(err),
                        (None, None) => Ok(HookOutcome::Ignored(said.join("; "))),
                    };
                }
            }
        }
        if let Some(img) = image.filter(|i| !i.trim().is_empty()) {
            let Source::Image { image: own } = &svc.source else {
                return Err(Error::BadRequest("image is for an image service; this one builds its source".into()));
            };
            if image_repository(&img) != image_repository(own) {
                return Err(Error::BadRequest(format!("a hook deploys {} only (another tag or digest of it)", image_repository(own))));
            }
            body["image"] = json!(img.trim());
        }
        // A hook is automation: after a rollback, what it would deploy waits for a person.
        if let Some(r) = self.held(service, env).await {
            return Err(crate::Error::Conflict(format!("deploys are held after the rollback {r}: resume them (or deploy) first")));
        }
        self.deploy(service, env, body, By { trigger: Trigger::Api, actor: Some("deploy hook".into()) }, None, None).await.map(|d| HookOutcome::Deployed(Box::new(d)))
    }
}

/// What a hook's call did.
pub enum HookOutcome {
    Deployed(Box<Deployment>),
    /// Nothing to deploy, and why (a push to another branch, a ping).
    Ignored(String),
}

/// A repository's webhook call: its body, and the event its platform named in a header
/// (`X-GitHub-Event`, `X-Gitlab-Event`, Bitbucket's `X-Event-Key`).
pub struct Webhook {
    pub body: Value,
    pub event: Option<String>,
}

/// A webhook's call as a push: GitHub's (`ref`, `after`, `head_commit.message`; `zen` on its
/// ping), GitLab's (`ref`, `checkout_sha`, the last of `commits`) or Bitbucket's
/// (`push.changes[].new`: a branch, its target's hash and message).
impl Apps {
    /// A push to a branch apps doesn't follow, checked against the repository's refs and handed
    /// to roll previews. What the hook answers (`{ ignored }`).
    async fn hand_to_previews(self: &Arc<Self>, pushes: Arc<dyn crate::integrations::GitPushes>, service: &str, url: &str, push: BranchPush) -> String {
        let BranchPush { branch, sha, message } = push;
        let refs = match self.git_remote(service, url).await.ls_refs().await {
            Ok(r) => r,
            Err(e) => return format!("a push to {branch}: the repository didn't answer ({e:#})"),
        };
        let now = refs.get(&format!("refs/heads/{branch}")).map(|r| r.oid.to_string());
        if now != sha {
            return format!("a push to {branch}, but the repository has it {}", now.map_or("deleted".to_string(), |s| format!("at {}", &s[..12.min(s.len())])));
        }
        let what = if sha.is_some() { "for its preview" } else { "deleted: its preview goes" };
        pushes.pushed(crate::integrations::GitPush { service: service.to_string(), url: url.to_string(), branch: branch.clone(), sha, message, pull: None }).await;
        format!("a push to {branch}, handed to roll previews {what}")
    }

    /// A pull request opened, moved or closed, for roll previews: its head checked against the
    /// repository (a same-repository one's branch, a fork's `refs/pull/N/head` or GitLab's
    /// `refs/merge-requests/N/head`), so a body can't name a commit that isn't its head, and a
    /// forged `fork: false` can't pass a fork's commit off as a branch's. A close isn't checked
    /// (the head ref stays): at worst a forged one takes a preview down, as a forged branch
    /// deletion would. What the hook answers.
    async fn hand_pull_to_previews(self: &Arc<Self>, pushes: Arc<dyn crate::integrations::GitPushes>, service: &str, url: &str, p: PullEvent) -> String {
        let PullEvent { number, title, branch, sha, fork, gitlab } = p;
        let page = format!("{}{}{number}", url.trim_end_matches('/').trim_end_matches(".git"), if gitlab { "/-/merge_requests/" } else { "/pull/" });
        let branch = if fork { format!("pull/{number}") } else { branch };
        if let Some(sha) = &sha {
            let head = if !fork {
                format!("refs/heads/{branch}")
            } else if gitlab {
                format!("refs/merge-requests/{number}/head")
            } else {
                format!("refs/pull/{number}/head")
            };
            match self.git_remote(service, url).await.ref_at(&head).await {
                Ok(Some(now)) if now == *sha => {}
                Ok(now) => return format!("pull request {number}: the repository has {head} {}, not {}", now.map_or("missing".to_string(), |n| format!("at {}", &n[..12])), &sha[..12]),
                Err(e) => return format!("pull request {number}: the repository didn't answer ({e:#})"),
            }
        }
        let what = if sha.is_some() { "for its preview" } else { "closed: its preview goes" };
        let pull = crate::integrations::GitPull { number, title: title.chars().take(200).collect(), url: page, fork };
        pushes.pushed(crate::integrations::GitPush { service: service.to_string(), url: url.to_string(), branch, sha, message: String::new(), pull: Some(pull) }).await;
        format!("pull request {number}, handed to roll previews {what}")
    }
}

/// A pull request's (GitHub) or merge request's (GitLab) event: its head now (none: closed).
struct PullEvent {
    number: u64,
    title: String,
    /// Its head's branch (in the fork, for a fork's).
    branch: String,
    sha: Option<String>,
    fork: bool,
    gitlab: bool,
}

/// A branch a push moved to `sha` (none: it was deleted).
struct BranchPush {
    branch: String,
    sha: Option<String>,
    message: String,
}

enum Push {
    Ping,
    /// A pull request opened, moved, reopened or closed.
    Pull(PullEvent),
    /// The branches it moved (one, but for Bitbucket's that move several).
    Branches(Vec<BranchPush>),
    /// An empty body (CI's POST): a plain deploy.
    Plain,
    /// Another event, a tag, or a body that isn't a push: nothing deploys.
    Ignored(String),
}

fn push_of(w: &Webhook) -> Push {
    let p = &w.body;
    if w.event.as_deref() == Some("ping") || (p.get("zen").is_some() && p.get("hook_id").is_some()) {
        return Push::Ping;
    }
    if let Some(pull) = pull_of(w) {
        return pull;
    }
    if let Some(e) = w.event.as_deref().filter(|e| !matches!(*e, "push" | "Push Hook" | "repo:push")) {
        return Push::Ignored(format!("a {e} event isn't a push"));
    }
    let hex = |s: &str| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()) && s.bytes().any(|b| b != b'0');
    // Bitbucket's: every change of the push that moved a branch (at most 100 read).
    if let Some(changes) = p["push"]["changes"].as_array().filter(|c| !c.is_empty()) {
        let mut moved = Vec::new();
        let mut kinds = Vec::new();
        for c in changes.iter().take(100) {
            let (new, old) = (&c["new"], &c["old"]);
            let kind = new["type"].as_str().or(old["type"].as_str()).unwrap_or("");
            if kind != "branch" {
                kinds.push(kind.to_string());
                continue;
            }
            let branch = new["name"].as_str().or(old["name"].as_str()).unwrap_or("").to_string();
            let sha = new["target"]["hash"].as_str().filter(|s| hex(s)).map(str::to_ascii_lowercase);
            moved.push(BranchPush { branch, sha, message: new["target"]["message"].as_str().unwrap_or("").to_string() });
        }
        return if moved.is_empty() { Push::Ignored(format!("a push of a {}, not a branch", kinds.join(", "))) } else { Push::Branches(moved) };
    }
    let Some(r) = p["ref"].as_str() else {
        return if p.as_object().is_some_and(|o| o.is_empty()) { Push::Plain } else { Push::Ignored("not a push".into()) };
    };
    let Some(branch) = r.strip_prefix("refs/heads/") else { return Push::Ignored(format!("a push of {r}, not a branch")) };
    let sha = p["after"].as_str().or(p["checkout_sha"].as_str()).filter(|s| hex(s));
    let message = p["head_commit"]["message"].as_str().or_else(|| p["commits"].as_array().and_then(|c| c.last()).and_then(|c| c["message"].as_str())).unwrap_or("");
    Push::Branches(vec![BranchPush { branch: branch.to_string(), sha: sha.map(str::to_ascii_lowercase), message: message.to_string() }])
}

/// GitHub's `pull_request` or GitLab's `Merge Request Hook` event as a [`Push::Pull`]; None
/// for any other event. Fork or not is the head's repository against the base's, by id.
fn pull_of(w: &Webhook) -> Option<Push> {
    let p = &w.body;
    let hex = |s: &str| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit());
    let (number, title, branch, sha, fork, gitlab, action) = match w.event.as_deref()? {
        "pull_request" => {
            let pr = &p["pull_request"];
            let fork = pr["head"]["repo"]["id"].as_u64().is_none() || pr["head"]["repo"]["id"] != pr["base"]["repo"]["id"];
            (pr["number"].as_u64().or(p["number"].as_u64()), pr["title"].as_str(), pr["head"]["ref"].as_str(), pr["head"]["sha"].as_str(), fork, false, p["action"].as_str().unwrap_or(""))
        }
        "Merge Request Hook" => {
            let mr = &p["object_attributes"];
            let fork = mr["source_project_id"].as_u64().is_none() || mr["source_project_id"] != mr["target_project_id"];
            (mr["iid"].as_u64(), mr["title"].as_str(), mr["source_branch"].as_str(), mr["last_commit"]["id"].as_str(), fork, true, mr["action"].as_str().unwrap_or(""))
        }
        _ => return None,
    };
    let (Some(number), Some(branch)) = (number, branch.filter(|b| !b.is_empty() && b.len() <= 255)) else { return Some(Push::Ignored("a pull request without its number or branch".into())) };
    let open = matches!(action, "opened" | "synchronize" | "reopened" | "open" | "update" | "reopen");
    let closed = matches!(action, "closed" | "close" | "merge");
    if !open && !closed {
        return Some(Push::Ignored(format!("pull request {number} {action}: nothing to deploy")));
    }
    let sha = match (open, sha.filter(|s| hex(s))) {
        (true, Some(s)) => Some(s.to_ascii_lowercase()),
        (true, None) => return Some(Push::Ignored(format!("pull request {number} names no head commit"))),
        (false, _) => None,
    };
    Some(Push::Pull(PullEvent { number, title: title.unwrap_or("").to_string(), branch: branch.to_string(), sha, fork, gitlab }))
}

/// A hook's token: `rlk_`, 34 random characters of lowercase base32 and 6 of CRC-32, as roll's
/// other secrets (`roll_access::new_secret`), so secret scanning can find a leaked hook URL
/// and tell it from look-alike text.
fn new_token() -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let body = format!("rlk_{}", random(34));
    let c = crc32fast::hash(body.as_bytes()) >> 2;
    let sum: String = (0..6).rev().map(|i| ALPHABET[((c >> (i * 5)) & 31) as usize] as char).collect();
    format!("{body}{sum}")
}

