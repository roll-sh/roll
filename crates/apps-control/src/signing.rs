//! Signed images (Cloud Run's Binary Authorization, cosign's `--key`): a project's signing keys
//! (P-256 public keys in PEM, as `cosign generate-key-pair` writes them) and whether its image
//! services must meet them. With `required`, an image service's deploy pins its image, then asks
//! the registry for the cosign signature beside that digest; one no key verifies fails the deploy
//! before anything switches. roll's own builds aren't images from a registry and pass (roll built
//! them from the source); a sidecar's image isn't checked yet.

use crate::{Apps, Error, Result};
use serde_json::{Value, json};
use std::sync::Arc;

/// Keys a project names at most.
pub const MAX_KEYS: usize = 10;

impl Apps {
    pub async fn signing(self: &Arc<Self>, project: &str) -> Result<Value> {
        let p = project.to_string();
        let (keys, required) = self.db(move |s| s.project_signing(&p)).await?.unwrap_or_default();
        // roll's own key for the project's builds' provenance (`crate::provenance`), made here if
        // it has none yet, so it can be pinned before the first build.
        let p = project.to_string();
        let roll_key = self.db(move |s| s.provenance_key(&p).map(|(_, public)| public)).await?;
        Ok(json!({ "keys": keys, "required": required, "roll_key": roll_key }))
    }

    /// `PUT /v1/apps/projects/{project}/signing`: `{ keys, required? }` (required by default).
    pub async fn set_signing(self: &Arc<Self>, project: &str, body: Value) -> Result<Value> {
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| !["keys", "required"].contains(&k.as_str())) {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let keys: Vec<String> = o.get("keys").and_then(Value::as_array).filter(|k| !k.is_empty() && k.len() <= MAX_KEYS).ok_or_else(|| Error::BadRequest(format!("keys is 1–{MAX_KEYS} public keys in PEM (cosign.pub)")))?.iter().map(|k| k.as_str().unwrap_or_default().trim().to_string()).collect();
        for k in &keys {
            use p256::pkcs8::DecodePublicKey;
            if k.len() > 4096 || p256::PublicKey::from_public_key_pem(k).is_err() {
                return Err(Error::BadRequest("a key isn't a P-256 public key in PEM (cosign generate-key-pair's cosign.pub)".into()));
            }
        }
        let required = match o.get("required") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(Error::BadRequest("required is a boolean".into())),
        };
        {
            let (this, p) = (self.clone(), project.to_string());
            tokio::task::spawn_blocking(move || this.project_name(&p)).await.map_err(|e| Error::Internal(e.to_string()))??;
        }
        let (p, k) = (project.to_string(), keys.clone());
        self.db(move |s| s.set_project_signing(&p, &k, required)).await?;
        Ok(json!({ "keys": keys, "required": required }))
    }

    pub async fn delete_signing(self: &Arc<Self>, project: &str) -> Result<()> {
        let p = project.to_string();
        self.db(move |s| s.delete_project_signing(&p)).await
    }

    /// Whether the project requires signed images.
    pub(crate) async fn signing_required(self: &Arc<Self>, project: &str) -> bool {
        let p = project.to_string();
        matches!(self.db(move |s| s.project_signing(&p)).await, Ok(Some((_, true))))
    }

    /// An image service's pinned image meets its project's signing rule (nothing asked: it
    /// does), said in the deploy's log either way.
    pub(crate) async fn check_signed(self: &Arc<Self>, project: &str, pinned: &str, login: Option<&crate::runtime::RegistryLogin>, log: &crate::logs::LogSink) -> anyhow::Result<()> {
        let p = project.to_string();
        let Some((keys, true)) = self.db(move |s| s.project_signing(&p)).await? else { return Ok(()) };
        match self.deps.runtime.verify_signature(pinned, &keys, login).await {
            Ok(i) => {
                log.line(format!("signed: its cosign signature verifies with the project's key #{}", i + 1));
                Ok(())
            }
            Err(e) => anyhow::bail!("the project requires signed images, and {pinned} isn't: {e:#}"),
        }
    }
}
