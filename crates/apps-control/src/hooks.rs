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
    /// as `payload`): a push to the environment's branch deploys that commit at once, keyed by
    /// it as the five-minute watch's deploy is (a redelivery, or the watch, makes no second one);
    /// another branch, a deleted branch, a `[skip ci]` commit or a ping is `Ignored`.
    pub async fn run_hook(self: &Arc<Self>, service: &str, env: &str, token: &str, image: Option<String>, payload: Option<Value>) -> Result<HookOutcome> {
        let none = || Error::NotFound("no such hook".into());
        let (sid, e) = (service.to_string(), env.to_string());
        let (svc, row) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await.map_err(|_| none())?;
        let Some(hash) = row.hook_hash.as_deref() else { return Err(none()) };
        if !crate::public::same(&crate::public::token_hash(token), hash) {
            return Err(none());
        }
        let mut body = json!({ "message": "deploy hook" });
        let mut key = None;
        if let (Source::Git { .. }, Some(p)) = (&svc.source, payload.as_ref()) {
            match push_of(p) {
                Push::Ping => return Ok(HookOutcome::Ignored("pong".into())),
                Push::Other => {}
                Push::Branch { branch, sha, message } => {
                    let follows = row.branch.as_deref().unwrap_or(branch.as_str());
                    if branch != follows {
                        return Ok(HookOutcome::Ignored(format!("a push to {branch}; {env} follows {follows}")));
                    }
                    let Some(sha) = sha else { return Ok(HookOutcome::Ignored(format!("{branch} was deleted"))) };
                    if let Some(tag) = crate::deploy::skip_tag(&message) {
                        return Ok(HookOutcome::Ignored(format!("its message says {tag}")));
                    }
                    let first = message.lines().next().unwrap_or("").chars().take(500).collect::<String>();
                    body = json!({ "revision": sha, "message": if first.is_empty() { format!("push to {branch}") } else { first } });
                    key = Some(format!("git:{env}:{sha}"));
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
        let trigger = if key.is_some() { Trigger::Push } else { Trigger::Api };
        self.deploy(service, env, body, By { trigger, actor: Some("deploy hook".into()) }, None, key.as_deref()).await.map(|d| HookOutcome::Deployed(Box::new(d)))
    }
}

/// What a hook's call did.
pub enum HookOutcome {
    Deployed(Box<Deployment>),
    /// Nothing to deploy, and why (a push to another branch, a ping).
    Ignored(String),
}

/// A webhook's body as a push: GitHub's (`ref`, `after`, `head_commit.message`; `zen` on its
/// ping) or GitLab's (`ref`, `checkout_sha`, the last of `commits`).
enum Push {
    Ping,
    /// A branch moved to `sha` (none: it was deleted).
    Branch { branch: String, sha: Option<String>, message: String },
    /// Not a push (a registry's webhook, CI's empty POST): a plain deploy.
    Other,
}

fn push_of(p: &Value) -> Push {
    if p.get("zen").is_some() && p.get("hook_id").is_some() {
        return Push::Ping;
    }
    let Some(branch) = p["ref"].as_str().and_then(|r| r.strip_prefix("refs/heads/")) else { return Push::Other };
    let sha = p["after"].as_str().or(p["checkout_sha"].as_str()).filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()) && s.bytes().any(|b| b != b'0'));
    let message = p["head_commit"]["message"].as_str().or_else(|| p["commits"].as_array().and_then(|c| c.last()).and_then(|c| c["message"].as_str())).unwrap_or("");
    Push::Branch { branch: branch.to_string(), sha: sha.map(str::to_ascii_lowercase), message: message.to_string() }
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

