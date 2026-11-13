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

    /// `POST /v1/apps/hooks/{service}/{env}/{token}[?image=]`: deploy, as the hook. Anything
    /// that isn't a live hook is the same 404, so the URL tells a guesser nothing.
    pub async fn run_hook(self: &Arc<Self>, service: &str, env: &str, token: &str, image: Option<String>) -> Result<Deployment> {
        let none = || Error::NotFound("no such hook".into());
        let (sid, e) = (service.to_string(), env.to_string());
        let (svc, row) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await.map_err(|_| none())?;
        let Some(hash) = row.hook_hash.as_deref() else { return Err(none()) };
        if !crate::public::same(&crate::public::token_hash(token), hash) {
            return Err(none());
        }
        let mut body = json!({ "message": "deploy hook" });
        if let Some(img) = image.filter(|i| !i.trim().is_empty()) {
            let Source::Image { image: own } = &svc.source else {
                return Err(Error::BadRequest("image is for an image service; this one builds its source".into()));
            };
            if image_repository(&img) != image_repository(own) {
                return Err(Error::BadRequest(format!("a hook deploys {} only (another tag or digest of it)", image_repository(own))));
            }
            body["image"] = json!(img.trim());
        }
        self.deploy(service, env, body, By { trigger: Trigger::Api, actor: Some("deploy hook".into()) }, None, None).await
    }
}

/// A hook's token: `rlh_`, 34 random characters of lowercase base32 and 6 of CRC-32, as roll's
/// other secrets (`roll_access::new_secret`), so secret scanning can find a leaked hook URL
/// and tell it from look-alike text.
fn new_token() -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let body = format!("rlh_{}", random(34));
    let c = crc32fast::hash(body.as_bytes()) >> 2;
    let sum: String = (0..6).rev().map(|i| ALPHABET[((c >> (i * 5)) & 31) as usize] as char).collect();
    format!("{body}{sum}")
}

