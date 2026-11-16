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
                    let (mut said, mut deployed) = (Vec::new(), Vec::new());
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
                        match crate::git_source::head(&remote, Some(&p.branch)).await {
                            Ok((now, _)) if now == sha => {}
                            Ok((now, _)) => {
                                said.push(format!("the repository has {} at {}, not {}: only its head deploys", p.branch, &now[..12], &sha[..12]));
                                continue;
                            }
                            Err(e) => {
                                said.push(format!("{} couldn't be checked against the repository ({e:#}): not deployed", p.branch));
                                continue;
                            }
                        }
                        for e in following {
                            if e.paused {
                                said.push(format!("{} is paused", e.name));
                                continue;
                            }
                            if let Some(r) = &e.held_by {
                                said.push(format!("{}'s deploys are held after the rollback {r}", e.name));
                                continue;
                            }
                            if let Some(why) = self.git_push_skip(&svc, e, &sha, &p.message).await {
                                said.push(format!("{}: {why}", e.name));
                                continue;
                            }
                            let first = p.message.lines().next().unwrap_or("").chars().take(500).collect::<String>();
                            let body = json!({ "revision": sha, "message": if first.is_empty() { format!("push to {}", p.branch) } else { first } });
                            let by = By { trigger: Trigger::Push, actor: Some("deploy hook".into()) };
                            match self.deploy(service, &e.name, body, by, None, Some(&format!("git:{}:{sha}", e.name))).await {
                                Ok(d) => deployed.push(d),
                                Err(err) => said.push(format!("{}: {err}", e.name)),
                            }
                        }
                    }
                    // The answer is this environment's deploy when it made one, else another's.
                    deployed.sort_by_key(|d| d.environment != env);
                    return Ok(match deployed.into_iter().next() {
                        Some(d) => HookOutcome::Deployed(Box::new(d)),
                        None => HookOutcome::Ignored(said.join("; ")),
                    });
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
        pushes.pushed(crate::integrations::GitPush { service: service.to_string(), url: url.to_string(), branch: branch.clone(), sha, message }).await;
        format!("a push to {branch}, handed to roll previews {what}")
    }
}

/// A branch a push moved to `sha` (none: it was deleted).
struct BranchPush {
    branch: String,
    sha: Option<String>,
    message: String,
}

enum Push {
    Ping,
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

