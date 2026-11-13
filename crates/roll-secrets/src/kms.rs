//! Customer-managed keys (docs/secrets.md, "Customer-managed keys"): a project's data key, already
//! sealed by the deployment key, wrapped once more by a KMS key in the customer's own account.
//! Opening it then needs both: roll's deployment key and the customer's key, which they can revoke
//! (or disable) to make the project's values unreadable to roll.
//!
//! Stored as `kms:v1:<key, base64url>:<ciphertext, base64>`; the KMS encryption context names roll
//! secrets and the project, so a wrapped key opens for nothing else. An unwrapped key (still sealed
//! by the deployment key) is kept in memory for [`CACHE_TTL`], so a revocation takes effect within
//! that and a request doesn't wait on KMS.

use crate::{Error, Result};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long an unwrapped data key is kept before KMS is asked again.
pub const CACHE_TTL: Duration = Duration::from_secs(60);
const PREFIX: &str = "kms:v1:";

/// The customer's KMS (rolld: AWS KMS, cross-account). Calls block; errors name the KMS's reason,
/// never key material.
pub trait ProjectKms: Send + Sync {
    /// `plain` encrypted with `key` (a key ARN) for `project`.
    fn encrypt(&self, key: &str, project: &str, plain: &[u8]) -> std::result::Result<Vec<u8>, String>;
    /// What `encrypt` made, decrypted.
    fn decrypt(&self, key: &str, project: &str, ciphertext: &[u8]) -> std::result::Result<zeroize::Zeroizing<Vec<u8>>, String>;
}

/// A key ARN as roll accepts one: an AWS KMS key or alias in some account.
pub fn check_key(key: &str) -> Result<()> {
    let ok = key.len() <= 256 && key.starts_with("arn:aws") && key.split(':').count() >= 6 && (key.contains(":key/") || key.contains(":alias/")) && key.chars().all(|c| c.is_ascii_graphic());
    if ok { Ok(()) } else { Err(Error::BadRequest("a customer-managed key is a KMS key's ARN: arn:aws:kms:<region>:<account>:key/<id> (or alias/<name>)".into())) }
}

/// The key a stored data key is wrapped by, if it is.
pub fn wrapped_by(stored: &str) -> Option<String> {
    let rest = stored.strip_prefix(PREFIX)?;
    let (key, _) = rest.split_once(':')?;
    String::from_utf8(URL_SAFE_NO_PAD.decode(key).ok()?).ok()
}

/// Unwraps stored data keys, caching what it unwrapped.
#[derive(Clone, Default)]
pub(crate) struct Unwrapper {
    pub kms: Option<Arc<dyn ProjectKms>>,
    cache: Arc<Mutex<HashMap<String, (Instant, String)>>>,
}

impl Unwrapper {
    pub fn new(kms: Option<Arc<dyn ProjectKms>>) -> Unwrapper {
        Unwrapper { kms, cache: Default::default() }
    }

    fn kms(&self) -> Result<&Arc<dyn ProjectKms>> {
        self.kms.as_ref().ok_or_else(|| Error::Unavailable("this project's data key is wrapped by a customer-managed key, and this server has no KMS".into()))
    }

    /// `stored` as the key ring opens it: as is, or unwrapped by the customer's key.
    pub fn inner(&self, project: &str, stored: &str) -> Result<String> {
        let Some(rest) = stored.strip_prefix(PREFIX) else { return Ok(stored.to_string()) };
        if let Some((at, inner)) = self.cache.lock().expect("kms cache").get(stored)
            && at.elapsed() < CACHE_TTL
        {
            return Ok(inner.clone());
        }
        let (key, ct) = rest.split_once(':').ok_or_else(|| Error::Internal("a wrapped data key is damaged".into()))?;
        let key = URL_SAFE_NO_PAD.decode(key).ok().and_then(|k| String::from_utf8(k).ok()).ok_or_else(|| Error::Internal("a wrapped data key is damaged".into()))?;
        let ct = STANDARD.decode(ct).map_err(|_| Error::Internal("a wrapped data key is damaged".into()))?;
        let plain = self.kms()?.decrypt(&key, project, &ct).map_err(|why| Error::Unavailable(format!("the project's customer-managed key ({key}) didn't open its data key: {why}")))?;
        let inner = String::from_utf8(plain.to_vec()).map_err(|_| Error::Internal("a wrapped data key is damaged".into()))?;
        self.cache.lock().expect("kms cache").insert(stored.to_string(), (Instant::now(), inner.clone()));
        Ok(inner)
    }

    /// `inner` (sealed by the deployment key) wrapped by `key` for `project`, checked to open again.
    pub fn wrap(&self, key: &str, project: &str, inner: &str) -> Result<String> {
        let kms = self.kms()?;
        let ct = kms.encrypt(key, project, inner.as_bytes()).map_err(|why| Error::BadRequest(format!("roll couldn't encrypt with {key}: {why} (its key policy must let roll's role use it)")))?;
        let back = kms.decrypt(key, project, &ct).map_err(|why| Error::BadRequest(format!("roll could encrypt with {key} but not decrypt: {why}")))?;
        if back.as_slice() != inner.as_bytes() {
            return Err(Error::BadRequest(format!("{key} didn't give back what it encrypted")));
        }
        Ok(format!("{PREFIX}{}:{}", URL_SAFE_NO_PAD.encode(key), STANDARD.encode(ct)))
    }

    /// Forget a project's unwrapped key (its wrapping changed).
    pub fn forget(&self, stored: &str) {
        self.cache.lock().expect("kms cache").remove(stored);
    }
}

impl crate::Secrets {
    /// The customer-managed key wrapping `project`'s data key, if one does.
    pub fn kms_key(&self, project: &str, caller: &crate::Caller) -> Result<Option<String>> {
        caller.need(crate::Action::Admin, &crate::Scope::project(project))?;
        self.project(project)?;
        let stored = self.stored_dek(project)?;
        Ok(wrapped_by(&stored))
    }

    /// Wrap `project`'s data key with `key` (checked to encrypt and decrypt first), or, with
    /// `None`, unwrap it back to roll's deployment key alone. The values themselves don't change.
    pub fn set_kms_key(&self, project: &str, key: Option<String>, caller: &crate::Caller) -> Result<Option<String>> {
        caller.need(crate::Action::Admin, &crate::Scope::project(project))?;
        self.project(project)?;
        let stored = self.stored_dek(project)?;
        let inner = self.unwrap.inner(project, &stored)?;
        let next = match &key {
            Some(k) => {
                check_key(k)?;
                self.unwrap.wrap(k, project, &inner)?
            }
            None => inner,
        };
        let now = self.db.now_ms()?;
        let summary = match &key {
            Some(k) => format!("wrapped the project's data key with the customer-managed key {k}"),
            None => match wrapped_by(&stored) {
                Some(k) => format!("unwrapped the project's data key from {k}: roll's deployment key alone seals it"),
                None => return Ok(None),
            },
        };
        self.db.write(|c| {
            if c.execute("UPDATE projects SET dek = ? WHERE project_id = ? AND dek = ?", [next.as_str(), project, stored.as_str()])? == 0 {
                return Err(Error::Conflict("conflict", "the project's data key changed meanwhile: try again".into()));
            }
            crate::audit::record(c, project, caller, "project.kms", None, None, &[], &summary, now)
        })?;
        self.unwrap.forget(&stored);
        Ok(key)
    }

    fn stored_dek(&self, project: &str) -> Result<String> {
        Ok(self.db.read(|c| c.query_row("SELECT dek FROM projects WHERE project_id = ?", [project], |r| r.get::<_, String>(0)))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_arn_is_checked_and_read_back_from_a_wrapped_key() {
        assert!(check_key("arn:aws:kms:eu-west-1:123456789012:key/1234abcd-12ab-34cd-56ef-1234567890ab").is_ok());
        assert!(check_key("arn:aws:kms:eu-west-1:123456789012:alias/roll").is_ok());
        assert!(check_key("1234abcd").is_err() && check_key("arn:aws:s3:::bucket").is_err());
        let stored = format!("{PREFIX}{}:AAAA", URL_SAFE_NO_PAD.encode("arn:aws:kms:eu-west-1:1:key/k"));
        assert_eq!(wrapped_by(&stored).as_deref(), Some("arn:aws:kms:eu-west-1:1:key/k"));
        assert_eq!(wrapped_by("v1:abc"), None);
    }
}
