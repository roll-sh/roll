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
//! A node without a key refuses to store a secret a client gives (a 400 naming
//! `ROLL_SECRET_KEY`) rather than keep it in the clear, unless it opts in for local development
//! ([`crate::ClusterConfig::unsealed_secrets`]: the dev server, tests), when values are stored as
//! given, as on Elasticsearch. A node that gains a key seals what was kept in the clear as each
//! cluster opens ([`seal_leftovers`]), and persists it at once.
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

    /// `plain` sealed for `kind` (a secret kept outside a cluster's state: roll's control
    /// plane binds it to its row through `kind`).
    pub fn seal_text(&self, kind: &str, plain: &str) -> String {
        self.seal(kind, plain)
    }

    /// What [`Sealer::seal_text`] sealed for `kind`; `None` for anything else.
    pub fn open_text(&self, kind: &str, stored: &str) -> Option<String> {
        self.open(kind, stored.strip_prefix(PREFIX)?)
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

/// The refusal of a node without a key to store a `kind` in the clear.
pub fn keyless(kind: &str) -> EsError {
    EsError::illegal_argument(format!(
        "this node has no sealing key (ROLL_SECRET_KEY), so a {kind} would be stored in the clear: it isn't stored. Have the operator set ROLL_SECRET_KEY"
    ))
}

/// Whether the cluster may keep a secret: sealed under a key, or in the clear on a node that
/// opted in ([`crate::ClusterConfig::unsealed_secrets`]).
pub fn may_store(cluster: &Cluster) -> bool {
    cluster.config.sealer.is_some() || cluster.config.unsealed_secrets
}

/// A secret a client just gave, as the cluster stores it: sealed when the node has a key, even
/// when it looks sealed already. Unlike [`seal`], nothing given is kept as a sealed value: a
/// client must not plant one (copied from a snapshot, or another cluster on the node) for the
/// node to open and send where the client chooses. Refused on a node without a key ([`keyless`]).
pub fn seal_given(cluster: &Cluster, kind: &str, plain: &str) -> EsResult<String> {
    match &cluster.config.sealer {
        Some(s) => Ok(s.seal(kind, plain)),
        None if cluster.config.unsealed_secrets => Ok(plain.to_string()),
        None => Err(keyless(kind)),
    }
}

/// Whether `v` is a sealed value.
pub fn is_sealed(v: &str) -> bool {
    v.starts_with(PREFIX)
}

/// `plain` as the cluster stores it: sealed when the node has a key (a value sealed already is
/// kept as it is); in the clear refused on a node without a key ([`keyless`]).
pub fn seal(cluster: &Cluster, kind: &str, plain: &str) -> EsResult<String> {
    match &cluster.config.sealer {
        Some(s) if !is_sealed(plain) => Ok(s.seal(kind, plain)),
        None if !is_sealed(plain) && !cluster.config.unsealed_secrets => Err(keyless(kind)),
        _ => Ok(plain.to_string()),
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

/// A secret store's entry for `value` (connector and Fleet secrets): `{"sealed": …}` holding its
/// JSON sealed when the node has a key, `{"value": …}` as given on a node that opted in, refused
/// otherwise ([`keyless`]).
pub fn seal_value(cluster: &Cluster, kind: &str, value: crate::json::Value) -> EsResult<crate::json::Value> {
    match &cluster.config.sealer {
        Some(s) => Ok(crate::obj! { "sealed" => s.seal(kind, &value.to_json()) }),
        None if cluster.config.unsealed_secrets => Ok(crate::obj! { "value" => value }),
        None => Err(keyless(kind)),
    }
}

/// Seals every secret a cluster keeps in the clear (stored on a node without a key), with this
/// node's key, and persists it at once: inference endpoints' keys, watches' passwords and
/// `Authorization` headers, connector and Fleet secrets, snapshot repositories' credentials.
/// Run as the cluster opens; a record changed meanwhile is left for the next open. Returns how
/// many records it sealed.
pub async fn seal_leftovers(cluster: &Cluster) -> EsResult<usize> {
    use crate::json::Value;
    use crate::xpack::store;
    let Some(sealer) = cluster.config.sealer.clone() else { return Ok(0) };
    let st = cluster.state();
    // (family, id, as read, sealed)
    let mut changes: Vec<(&'static str, String, Value, Value)> = Vec::new();
    for (id, rec) in st.metadata.xpack.iter(crate::xpack::inference::SECRETS) {
        let Value::Obj(m) = rec else { continue };
        if m.values().all(|v| v.as_str().is_none_or(is_sealed)) {
            continue;
        }
        let sealed = m.iter().map(|(k, v)| (k.clone(), match v.as_str() {
            Some(s) if !is_sealed(s) => Value::from(sealer.seal(crate::xpack::inference::SECRET_KIND, s)),
            _ => v.clone(),
        }));
        changes.push((crate::xpack::inference::SECRETS, id.clone(), rec.clone(), Value::Obj(sealed.collect())));
    }
    for (family, kind) in [(crate::xpack::entsearch::connectors::SECRETS, crate::xpack::entsearch::connectors::SECRET_KIND), (crate::xpack::fleet::SECRETS, crate::xpack::fleet::SECRET_KIND)] {
        for (id, rec) in st.metadata.xpack.iter(family) {
            if let Some(v) = rec.get("value") {
                changes.push((family, id.clone(), rec.clone(), crate::obj! { "sealed" => sealer.seal(kind, &v.to_json()) }));
            }
        }
    }
    for (id, rec) in st.metadata.xpack.iter(crate::xpack::watcher::WATCHES) {
        let Some(Value::Obj(w)) = rec.get("watch") else { continue };
        // Already in the cluster's state: its sealed values are its own.
        let sealed = crate::xpack::watcher::parse::sealed_watch(cluster, w, &|_| true)?;
        if &sealed != w {
            let mut r = rec.clone();
            if let Value::Obj(m) = &mut r {
                m.insert("watch".into(), Value::Obj(sealed));
            }
            changes.push((crate::xpack::watcher::WATCHES, id.clone(), rec.clone(), r));
        }
    }
    let repos = st.metadata.repositories.values().any(|r| r.secrets.values().any(|v| !is_sealed(v)));
    drop(st);
    if changes.is_empty() && !repos {
        return Ok(0);
    }
    let n = changes.len() + usize::from(repos);
    let now = crate::time::now_millis();
    cluster
        .update(move |st| {
            for (family, id, read, sealed) in changes {
                if st.metadata.xpack.get(family, &id) == Some(&read) {
                    store::put(st, family, &id, sealed, now)?;
                }
            }
            crate::api::snapshots::seal_stored_secrets(&mut st.metadata, Some(&sealer));
            Ok(())
        })
        .await?;
    Ok(n)
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

    /// What a cluster with a key stores is sealed, and still works where it's used: a watch's
    /// password and `Authorization` header, a connector secret.
    #[tokio::test]
    async fn stored_secrets_are_sealed() {
        use crate::cluster::{ClusterConfig, MemoryStore};
        let mut cfg = ClusterConfig::new("c");
        cfg.sealer = Sealer::from_secret("0123456789abcdef0123456789abcdef-k");
        let c = Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap();
        let call = |method: &str, uri: &str, body: &str| {
            let req = if body.is_empty() { crate::EsRequest::new(method, uri) } else { crate::EsRequest::json(method, uri, body.to_string()) };
            crate::rest::handle(&c, req)
        };
        let watch = r#"{"trigger":{"schedule":{"interval":"1h"}},"input":{"http":{"request":{"url":"http://h.example/x","auth":{"basic":{"username":"u","password":"pw-1"}},"headers":{"Authorization":"Bearer tok-2"}}}},"actions":{}}"#;
        assert_eq!(call("PUT", "/_watcher/watch/w?active=false", watch).await.status, 201);
        let r = call("POST", "/_connector/_secret", r#"{"value":"conn-3"}"#).await;
        let id = crate::json::parse(&r.body).unwrap().unwrap().get("id").and_then(crate::json::Value::as_str).unwrap().to_string();
        let state = format!("{:?}", c.state().metadata.xpack);
        for secret in ["pw-1", "tok-2", "conn-3"] {
            assert!(!state.contains(secret), "{secret} stored in the clear");
        }
        assert!(state.contains(PREFIX));
        let got = call("GET", &format!("/_connector/_secret/{id}"), "").await;
        assert!(String::from_utf8_lossy(&got.body).contains("conn-3"));
        let shown = call("GET", "/_watcher/watch/w", "").await;
        assert!(!String::from_utf8_lossy(&shown.body).contains(PREFIX), "redacted, not sealed text");
        // A sealed value copied from elsewhere (another cluster under the same key) isn't kept
        // as the password: it's sealed again as given, so the node never opens the other's.
        let planted = Sealer::from_secret("0123456789abcdef0123456789abcdef-k").unwrap().seal(super::super::xpack::watcher::http::PASSWORD_KIND, "victim-pw");
        let watch2 = watch.replace("pw-1", &planted);
        assert_eq!(call("PUT", "/_watcher/watch/w2?active=false", &watch2).await.status, 201);
        let state = format!("{:?}", c.state().metadata.xpack);
        assert!(!state.contains(&planted), "the planted sealed value is not stored as it was");
    }

    /// A node without a key refuses the secrets clients give; one that gains a key seals, as the
    /// cluster opens, what a keyless node that opted in kept in the clear, and saves it at once.
    #[tokio::test]
    async fn keyless_nodes_refuse_and_keyed_ones_seal_leftovers() {
        use crate::cluster::{ClusterConfig, MemoryStore, MetaStore};
        let store = Arc::new(MemoryStore::new());
        async fn call(c: &Cluster, method: &str, uri: &str, body: &str) -> crate::EsResponse {
            crate::rest::handle(c, crate::EsRequest::json(method, uri, body.to_string())).await
        }
        let watch = r#"{"trigger":{"schedule":{"interval":"1h"}},"input":{"http":{"request":{"url":"http://h.example/x","auth":{"basic":{"username":"u","password":"pw-1"}}}}},"actions":{}}"#;
        let c = Cluster::open(ClusterConfig::new("c"), store.clone()).await.unwrap();
        for (uri, body) in [("/_connector/_secret", r#"{"value":"conn-3"}"#), ("/_watcher/watch/w?active=false", watch)] {
            let r = call(&c, if uri.contains("watch") { "PUT" } else { "POST" }, uri, body).await;
            assert!(r.status == 400 && String::from_utf8_lossy(&r.body).contains("ROLL_SECRET_KEY"), "{uri}: {}", String::from_utf8_lossy(&r.body));
        }
        drop(c);
        let c = Cluster::open(ClusterConfig { unsealed_secrets: true, ..ClusterConfig::new("c") }, store.clone()).await.unwrap();
        assert_eq!(call(&c, "PUT", "/_watcher/watch/w?active=false", watch).await.status, 201);
        let r = call(&c, "POST", "/_connector/_secret", r#"{"value":"conn-3"}"#).await;
        let id = crate::json::parse(&r.body).unwrap().unwrap().get("id").and_then(crate::json::Value::as_str).unwrap().to_string();
        assert_eq!(call(&c, "POST", "/_fleet/secret", r#"{"value":"fleet-4"}"#).await.status, 200);
        drop(c);
        let mut cfg = ClusterConfig::new("c");
        cfg.sealer = Sealer::from_secret("0123456789abcdef0123456789abcdef-k");
        let c = Cluster::open(cfg, store.clone()).await.unwrap();
        assert_eq!(seal_leftovers(&c).await.unwrap(), 3);
        let saved = String::from_utf8_lossy(&store.load().await.unwrap().unwrap()).into_owned();
        for secret in ["pw-1", "conn-3", "fleet-4"] {
            assert!(!saved.contains(secret), "{secret} still in the clear in the saved state");
        }
        let got = call(&c, "GET", &format!("/_connector/_secret/{id}"), "").await;
        assert!(String::from_utf8_lossy(&got.body).contains("conn-3"), "still opens");
        assert_eq!(seal_leftovers(&c).await.unwrap(), 0);
    }
}
