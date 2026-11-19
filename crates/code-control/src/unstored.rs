//! "Store in roll" for a `secret_unstored` notice (docs/secrets.md, "Credentials roll doesn't
//! hold"; docs/code/http-api.md): roll reads the credential from the change's own blob, server-side,
//! and stores it through roll secrets ([`crate::SecretEnv::store`]) in the project roll secrets
//! trusts the repo for, as the caller. The value is never answered, logged or audited; the answer
//! is where it went and the line to write instead. roll never rewrites the change.
//!
//! `GET /v1/code/changes/{id}/unstored/{fingerprint}` says where it would go (the suggested key,
//! the project, its environments); `POST …/store {key?, environment?, path?}` stores it. Both need
//! a writer on the repo; roll secrets then checks `write` in the folder with the caller's own grant,
//! session or operator token. An outsider's change (`trusted: false`, a fork's pull request) is
//! refused with 409: its credential is whoever sent it, not the repo's, and never goes into the
//! project the repo is trusted for.

use crate::ext::{CredentialValue, SecretsAsker, StoreCredential, StoreTarget};
use crate::model::{Change, ChangeState, RepoRow};
use crate::secret_leaks::MAX_FILE;
use crate::{Caller, Code, Error, Result};
use serde_json::{Value, json};

/// A notice's credential: what the notice says about it, and its value (never shown).
struct Found {
    kind: &'static str,
    path: String,
    line: usize,
    fingerprint: String,
    key: String,
    value: CredentialValue,
}

/// `roll.toml`'s environment for checks (`staging:/api`) as (environment, folder).
fn preferred(code_env: Option<&str>) -> (Option<String>, String) {
    match code_env {
        Some(e) => match e.split_once(':') {
            Some((env, path)) => (Some(env.to_string()), path.to_string()),
            None => (Some(e.to_string()), "/".to_string()),
        },
        None => (None, "/".to_string()),
    }
}

/// A key name roll secrets would take: a letter or `_`, then letters, digits and `_`.
fn is_key(k: &str) -> bool {
    k.len() <= 256 && k.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl Code {
    /// The credential of `c`'s `secret_unstored` notice with this fingerprint, as the change shows
    /// it now (its current head; the default branch's `.roll/secrets-allow` applied), with its
    /// value read from the change's blob at the notice's path and line.
    async fn unstored_credential(&self, r: &RepoRow, c: &Change, fingerprint: &str) -> Result<Found> {
        if matches!(c.state, ChangeState::Landed | ChangeState::Closed) {
            return Err(Error::Conflict(format!("change {} is {}: there's nothing to store from it", c.id, if c.state == ChangeState::Landed { "landed" } else { "closed" })));
        }
        // An outsider's credential (a fork's pull request) isn't the repo's to keep, whoever approved
        // its run: storing it would hand the trusted project a value someone outside chose.
        if !c.trusted {
            return Err(Error::Conflict(format!("change {} is from outside the repo: roll doesn't store its credentials", c.id)));
        }
        let gone = || Error::NotFound(format!("credential {fingerprint} in change {}", c.id));
        let head = self.store.resolve(&r.id, &c.head)?.ok_or_else(gone)?;
        self.prepare_leaks(r, c).await;
        let settings = self.settings(r).await;
        let (_, facts) = self.leak_facts.get(&(r.id.clone(), head)).ok_or_else(gone)?;
        let u = facts.unstored.iter().find(|u| u.fingerprint == fingerprint && !u.allowed(&settings.secrets_allow)).ok_or_else(gone)?;
        let f = self.store.read_file(&r.id, &head, &u.path).await?.ok_or_else(gone)?;
        if f.mode != code_store::Mode::Regular || f.data.len() as u64 > MAX_FILE {
            return Err(gone());
        }
        let text = String::from_utf8_lossy(&f.data);
        // The same reading as the notice's: the match on its line whose fingerprint it names.
        let (mut line, mut seen) = (1, 0);
        for m in roll_auth::credentials::find_all(&text) {
            line += text[seen..m.start].matches('\n').count();
            seen = m.start;
            let value = &text[m.start..m.end];
            if line == u.line && roll_auth::credentials::fingerprint(value) == fingerprint {
                let value = CredentialValue(value.to_string());
                return Ok(Found { kind: u.kind, path: u.path.clone(), line: u.line, fingerprint: u.fingerprint.clone(), key: u.key.clone(), value });
            }
        }
        Err(gone())
    }

    /// `GET /v1/code/changes/{id}/unstored/{fingerprint}`: where the notice's credential would be
    /// stored, for the dashboard's dialog. Never the value.
    pub(crate) async fn unstored_target(&self, caller: &Caller, asker: &SecretsAsker, id: &str, fingerprint: &str) -> Result<Value> {
        let (r, c) = self.writable_change(caller, id)?;
        let found = self.unstored_credential(&r, &c, fingerprint).await?;
        let secrets = self.secret_env.get().cloned().ok_or_else(|| Error::Unprocessable("roll secrets isn't on this server".into()))?;
        let (prefer, path) = preferred(self.settings(&r).await.secrets_environment.as_deref());
        let StoreTarget { project, project_name, environments, environment } = secrets.store_target(&r.full_name(), asker, prefer.as_deref()).await?;
        Ok(json!({
            "kind": found.kind, "file": found.path, "line": found.line, "fingerprint": found.fingerprint, "key": found.key,
            "project": { "id": project, "name": project_name }, "environments": environments, "environment": environment, "path": path,
        }))
    }

    /// `POST /v1/code/changes/{id}/unstored/{fingerprint}/store`: store the notice's credential as
    /// a new key (`key`, default the notice's), in `environment` (default `roll.toml`'s, else
    /// `development`) at `path` (default `roll.toml`'s folder, else `/`), as the caller. Answers
    /// where it went and the read to write instead; never the value.
    pub(crate) async fn store_unstored(&self, caller: &Caller, asker: &SecretsAsker, id: &str, fingerprint: &str, b: &Value) -> Result<Value> {
        let field = |k: &str| -> Result<Option<String>> {
            match &b[k] {
                Value::Null => Ok(None),
                Value::String(s) if !s.trim().is_empty() => Ok(Some(s.trim().to_string())),
                _ => Err(Error::Invalid(format!("{k} is a non-empty string"))),
            }
        };
        let (key, environment, path) = (field("key")?, field("environment")?, field("path")?);
        if let Some(k) = &key
            && !is_key(k)
        {
            return Err(Error::Invalid(format!("{k} isn't a key name: letters, digits and _, not starting with a digit")));
        }
        let (r, c) = self.writable_change(caller, id)?;
        let found = self.unstored_credential(&r, &c, fingerprint).await?;
        let secrets = self.secret_env.get().cloned().ok_or_else(|| Error::Unprocessable("roll secrets isn't on this server".into()))?;
        let (prefer, preferred_path) = preferred(self.settings(&r).await.secrets_environment.as_deref());
        let key = key.unwrap_or(found.key);
        let message = format!("stored {} from {}#{} {}:{}", found.kind, r.full_name(), c.number, found.path, found.line);
        let credential = StoreCredential { key: key.clone(), environment, prefer, path: path.unwrap_or(preferred_path), value: found.value, message };
        let stored = secrets.store(&r.full_name(), asker, credential).await?;
        let replacement = roll_auth::credentials::env_read(&found.path, &key);
        let detail = json!({ "change": c.id, "fingerprint": found.fingerprint, "key": key, "project": stored.project, "environment": stored.environment, "path": stored.path });
        self.db.audit(&r.id, &caller.identity().unwrap_or_else(|| "anonymous".into()), "secret_unstored_stored", &detail)?;
        Ok(json!({
            "key": key, "project": stored.project, "environment": stored.environment, "path": stored.path,
            "file": found.path, "line": found.line, "replacement": replacement,
            "change": stored.change, "request": stored.request,
        }))
    }
}
