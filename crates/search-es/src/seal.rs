//! Secrets a cluster keeps in its stored state, sealed at rest: a watch's HTTP passwords and
//! `Authorization` headers, connector and Fleet secrets.
//!
//! Elasticsearch stores them as given unless `xpack.watcher.encrypt_sensitive_data` and a
//! system key are set; roll seals them whenever the node has a key (rolld's `ROLL_SECRET_KEY`,
//! through [`Sealer::from_secret`]): AES-256-GCM under a key derived from it, a fresh nonce per
//! value and the value's kind as associated data, so a sealed password can't be moved into
//! another kind's slot. A sealed value is a string, `::roll_sealed::v1:` and the base64 of the
//! nonce and ciphertext, so it travels through cluster state, snapshots and branches unchanged.
//!
//! Without a key (the dev server, tests) values are stored as given, as on Elasticsearch.
//! Values are opened only where they are used (a request sent, a secret read back through its
//! own API); everything that prints them redacts them as before.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use std::sync::Arc;

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};

const PREFIX: &str = "::roll_sealed::v1:";

/// Seals and opens a cluster's stored secrets.
pub struct Sealer {
    cipher: Aes256Gcm,
}

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sealer(..)")
    }
}

impl Sealer {
    /// The sealer of a deployment secret (at least 32 bytes of it).
    pub fn from_secret(secret: &str) -> Option<Arc<Sealer>> {
        if secret.trim().len() < 32 {
            return None;
        }
        let mut key = [0u8; 32];
        hkdf::Hkdf::<sha2::Sha256>::new(None, secret.as_bytes()).expand(b"roll search: sealed cluster secrets v1", &mut key).ok()?;
        Some(Arc::new(Sealer { cipher: Aes256Gcm::new(&key.into()) }))
    }

    fn seal(&self, kind: &str, plain: &str) -> String {
        let nonce: [u8; 12] = rand::random();
        let ct = self.cipher.encrypt(Nonce::from_slice(&nonce), Payload { msg: plain.as_bytes(), aad: kind.as_bytes() }).expect("AES-GCM sealing doesn't fail");
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ct);
        format!("{PREFIX}{}", STANDARD.encode(out))
    }

    fn open(&self, kind: &str, sealed: &str) -> Option<String> {
        let raw = STANDARD.decode(sealed).ok()?;
        if raw.len() < 12 {
            return None;
        }
        let (nonce, ct) = raw.split_at(12);
        let plain = self.cipher.decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: kind.as_bytes() }).ok()?;
        String::from_utf8(plain).ok()
    }
}

/// Whether `v` is a sealed value.
pub fn is_sealed(v: &str) -> bool {
    v.starts_with(PREFIX)
}

/// `plain` as the cluster stores it: sealed when the node has a key (a value sealed already is
/// kept as it is).
pub fn seal(cluster: &Cluster, kind: &str, plain: &str) -> String {
    match &cluster.config.sealer {
        Some(s) if !is_sealed(plain) => s.seal(kind, plain),
        _ => plain.to_string(),
    }
}

/// A stored value in the clear (`stored` as it is when it isn't sealed). A sealed value this
/// node can't open (no key, another deployment's key, tampered with) is an error that names
/// neither the value nor the key.
pub fn open(cluster: &Cluster, kind: &str, stored: &str) -> EsResult<String> {
    let Some(body) = stored.strip_prefix(PREFIX) else { return Ok(stored.to_string()) };
    cluster
        .config
        .sealer
        .as_ref()
        .and_then(|s| s.open(kind, body))
        .ok_or_else(|| EsError::new(500, "illegal_state_exception", format!("a sealed {kind} can't be opened on this node (its sealing key differs)")))
}

/// A secret store's entry for `value` (connector and Fleet secrets): `{"value": …}` as given,
/// or `{"sealed": …}` holding its JSON sealed when the node has a key.
pub fn seal_value(cluster: &Cluster, kind: &str, value: crate::json::Value) -> crate::json::Value {
    match &cluster.config.sealer {
        Some(s) => crate::obj! { "sealed" => s.seal(kind, &value.to_json()) },
        None => crate::obj! { "value" => value },
    }
}

/// The value of an entry [`seal_value`] made.
pub fn open_value(cluster: &Cluster, kind: &str, entry: &crate::json::Value) -> EsResult<crate::json::Value> {
    let Some(sealed) = entry.get("sealed").and_then(crate::json::Value::as_str) else {
        return Ok(entry.get("value").cloned().unwrap_or(crate::json::Value::Null));
    };
    let text = open(cluster, kind, sealed)?;
    crate::json::parse(text.as_bytes()).ok().flatten().ok_or_else(|| EsError::new(500, "illegal_state_exception", format!("a sealed {kind} holds no value")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_values_open_only_under_their_key_and_kind() {
        let a = Sealer::from_secret("0123456789abcdef0123456789abcdef-a").unwrap();
        let b = Sealer::from_secret("0123456789abcdef0123456789abcdef-b").unwrap();
        let s = a.seal("watch password", "hunter2");
        assert!(is_sealed(&s) && !s.contains("hunter2"));
        assert_ne!(s, a.seal("watch password", "hunter2"), "a fresh nonce per value");
        let body = s.strip_prefix(PREFIX).unwrap();
        assert_eq!(a.open("watch password", body).as_deref(), Some("hunter2"));
        assert_eq!(a.open("connector secret", body), None);
        assert_eq!(b.open("watch password", body), None);
        assert!(Sealer::from_secret("short").is_none());
    }
}
