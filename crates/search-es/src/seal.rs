//! Secrets a cluster keeps in its stored state, sealed at rest: a watch's HTTP passwords,
//! credential headers and URLs, connector and Fleet secrets, inference endpoints' keys.
//!
//! Elasticsearch stores them as given unless `xpack.watcher.encrypt_sensitive_data` and a
//! system key are set; roll seals them whenever the node has a key (rolld's `ROLL_SECRET_KEY`,
//! through [`Sealer::from_secret`]): AES-256-GCM under a key derived from it, a fresh nonce per
//! value and the value's kind as associated data, so a sealed password can't be moved into
//! another kind's slot. A sealed value is a string, `::roll_sealed::v1:` and the base64 of the
//! nonce and ciphertext, so it travels through cluster state, snapshots and branches unchanged.
//!
//! Every kind is bound to the cluster ([`bind`]: its id in roll's catalog), so a value doesn't
//! open in another cluster's state: a branch starts from a copy of its parent's, and the
//! parent's secrets (its provider keys, its connectors' API keys, its webhooks' tokens) are the
//! parent's, which the branch's people may not be allowed to read. A branch's copy holds
//! [`INHERITED`] in their place ([`crate::branch::mark_inherited`]): using one is an error that
//! says to give the secret again on the branch.
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

/// What a branch's copy holds in place of a secret of its parent's ([`crate::branch`]): it never
/// opens, and using it is an error that says to give the secret again on the branch.
pub const INHERITED: &str = "::roll_inherited::";

/// `kind` bound to this cluster (`<kind>:<cluster id>`, [`crate::branch::cluster_id`]): a value
/// sealed for it opens only in this cluster's state, never in a branch's copy of it.
pub fn bind(cluster: &Cluster, kind: &str) -> String {
    format!("{kind}:{}", crate::branch::cluster_id(cluster))
}

/// The error of using a secret a branch didn't get from its parent ([`INHERITED`]).
pub fn inherited(what: &str) -> EsError {
    EsError::illegal_argument(format!("this branch doesn't have the parent's {what}; set it on the branch"))
}

/// What `kind` names (bound or not), for messages: its part before the cluster.
fn label(kind: &str) -> &str {
    kind.split(':').next().unwrap_or(kind)
}

/// Seals and opens a cluster's stored secrets, and makes the web crawler's site verification
/// tokens ([`Sealer::site_token`]) under a key of their own.
pub struct Sealer {
    cipher: Aes256Gcm,
    site_key: [u8; 32],
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
        let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, secret.as_bytes());
        let (mut key, mut site_key) = ([0u8; 32], [0u8; 32]);
        hk.expand(b"roll search: sealed cluster secrets v1", &mut key).ok()?;
        hk.expand(b"roll search: crawler site verification tokens v1", &mut site_key).ok()?;
        Some(Arc::new(Sealer { cipher: Aes256Gcm::new(&key.into()), site_key }))
    }

    /// The token `identity` publishes to verify `site` for the web crawler: HMAC-SHA256 of the
    /// identity's length (8 bytes, big-endian), the identity and the site, under a key derived
    /// from the secret for this alone (its own label, apart from sealing's),
    /// base64url, its first 22 characters (132 bits). The same owner and site give the same token
    /// for as long as the secret stays, so a published record outlives every row that named it; a
    /// new secret gives new tokens.
    pub fn site_token(&self, identity: &str, site: &str) -> String {
        use hmac::Mac;
        let mut mac = <hmac::Hmac<sha2::Sha256> as hmac::Mac>::new_from_slice(&self.site_key).expect("HMAC takes any key length");
        // Length-prefixed, so no identity and site run into another pair's.
        mac.update(&(identity.len() as u64).to_be_bytes());
        mac.update(identity.as_bytes());
        mac.update(site.as_bytes());
        let mut t = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        t.truncate(22);
        t
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
        "this node has no sealing key (ROLL_SECRET_KEY), so a {} would be stored in the clear: it isn't stored. Have the operator set ROLL_SECRET_KEY",
        label(kind)
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
    seal_given_with(cluster.config.sealer.as_deref(), cluster.config.unsealed_secrets, kind, plain)
}

/// [`seal_given`] with the node's sealer and its opt-in in hand (inside a state update): the one
/// place a secret a client gives is sealed, kept as given on a node that opted in
/// ([`crate::ClusterConfig::unsealed_secrets`]), or refused ([`keyless`]).
pub fn seal_given_with(sealer: Option<&Sealer>, unsealed: bool, kind: &str, plain: &str) -> EsResult<String> {
    // Kept as it is: it never opens ([`open`]), so it can't carry anything.
    if plain == INHERITED {
        return Ok(INHERITED.to_string());
    }
    match sealer {
        Some(s) => Ok(s.seal(kind, plain)),
        None if unsealed => Ok(plain.to_string()),
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
/// node can't open (no key, another deployment's key, another cluster's, tampered with) is an
/// error that names neither the value nor the key; [`INHERITED`] is [`inherited`].
pub fn open(cluster: &Cluster, kind: &str, stored: &str) -> EsResult<String> {
    if stored == INHERITED {
        return Err(inherited(label(kind)));
    }
    let Some(body) = stored.strip_prefix(PREFIX) else { return Ok(stored.to_string()) };
    // Bound values first; the bare kind only for this load after binding them failed.
    let unbound = || cluster.unbound_secrets.load(std::sync::atomic::Ordering::Relaxed) && label(kind) != kind;
    let plain = cluster.config.sealer.as_ref().and_then(|s| s.open(kind, body).or_else(|| unbound().then(|| s.open(label(kind), body)).flatten())).ok_or_else(|| {
        EsError::new(500, "illegal_state_exception", format!("a sealed {} can't be opened here (sealed for another cluster, or under another key)", label(kind)))
    })?;
    if plain == INHERITED {
        return Err(inherited(label(kind)));
    }
    Ok(plain)
}

/// A secret store's entry for `value` (connector and Fleet secrets, `kind` bound to the cluster
/// and the entry): `{"sealed": …}` holding its JSON sealed when the node has a key, `{"value": …}`
/// as given on a node that opted in, refused otherwise ([`keyless`]).
pub fn seal_value(cluster: &Cluster, kind: &str, value: crate::json::Value) -> EsResult<crate::json::Value> {
    match &cluster.config.sealer {
        Some(s) => Ok(crate::obj! { "sealed" => s.seal(kind, &value.to_json()) }),
        None if cluster.config.unsealed_secrets => Ok(crate::obj! { "value" => value }),
        None => Err(keyless(kind)),
    }
}

/// Where a branch's state was copied from: what the host can read of it, so a branch made
/// before secrets were bound to their cluster keeps the secrets it made itself
/// ([`seal_leftovers`]). Sealing draws a fresh nonce, so a sealed value equal byte for byte to one
/// in these states was copied, never made again.
#[async_trait::async_trait]
pub trait Lineage: Send + Sync + std::fmt::Debug {
    /// Saved states (as [`crate::MetaStore`] saves them) the branch's state may have been copied
    /// from: the parent's current one and the parent's oldest kept one (the nearest to when the
    /// branch was made; the branch's own history isn't read, since once pruned it can hold
    /// secrets the branch made). Empty when either can't be read: then nothing counts as the
    /// branch's own. A value the parent had when the branch was made but replaced before its
    /// oldest kept state isn't in either, so it's kept as the branch's own (architecture.md).
    async fn sources(&self) -> Vec<bytes::Bytes>;
}

/// What [`seal_leftovers`] did.
#[derive(Debug, Default, PartialEq)]
pub struct Leftovers {
    /// Records it changed.
    pub changed: usize,
    /// `<family>/<id>` of every record whose secrets it took out as the parent's (on a branch).
    pub taken: Vec<String>,
}

/// A stored secret as the cluster keeps it from now on, sealed under `to` (`kind` bound to the
/// cluster, [`bind`]): a value in the clear (kept by a node without a key) is sealed, and one
/// sealed under the bare `kind` (before secrets were bound to their cluster) is opened and sealed
/// again. Unless it's `inherited` (in a branch's copy of its parent's state) and `copied` says
/// it came from the parent: then it becomes [`INHERITED`]. A value under another key is left as
/// it is (the node may get that key back).
pub fn settle(sealer: &Sealer, inherited: bool, copied: &dyn Fn(&str) -> bool, kind: &str, to: &str, stored: &str) -> String {
    if stored == INHERITED || sealer.open_text(to, stored).is_some() {
        return stored.to_string();
    }
    if inherited && copied(stored) {
        return INHERITED.to_string();
    }
    if !is_sealed(stored) {
        return sealer.seal_text(to, stored);
    }
    match sealer.open_text(kind, stored) {
        Some(plain) => sealer.seal_text(to, &plain),
        None => stored.to_string(),
    }
}

/// Every string, and every record's JSON, of the families [`seal_leftovers`] settles in saved
/// state `raw`: what a copied value equals.
fn secret_values(raw: &[u8], out: &mut std::collections::HashSet<String>) {
    use crate::json::Value;
    use crate::xpack::{entsearch::connectors, fleet, inference, watcher};
    fn strings(v: &Value, out: &mut std::collections::HashSet<String>) {
        match v {
            Value::Str(s) => {
                out.insert(s.clone());
            }
            Value::Obj(m) => m.values().for_each(|x| strings(x, out)),
            Value::Arr(a) => a.iter().for_each(|x| strings(x, out)),
            _ => {}
        }
    }
    let Ok(Some(v)) = crate::json::parse(raw) else { return };
    let xpack = crate::xpack::store::XpackState::from_stored(v.get("xpack").or_else(|| v.get("metadata").and_then(|m| m.get("xpack"))));
    for family in [inference::SECRETS, connectors::SECRETS, fleet::SECRETS, watcher::WATCHES] {
        for (_, rec) in xpack.iter(family) {
            out.insert(rec.to_json());
            strings(rec, out);
        }
    }
}

/// (family, id, as read, what it becomes: `None` removes it, whether that takes the parent's out)
type Change = (&'static str, String, crate::json::Value, Option<crate::json::Value>, bool);

/// The changes [`seal_leftovers`] makes to `st`, with `copied` telling a branch's copied values.
fn plan(cluster: &Cluster, st: &crate::meta::ClusterState, sealer: &Sealer, branch: bool, copied: &dyn Fn(&str) -> bool) -> Vec<Change> {
    use crate::json::Value;
    use crate::xpack::{entsearch::connectors, fleet, inference, watcher};
    let mut changes: Vec<Change> = Vec::new();
    let to = bind(cluster, inference::SECRET_KIND);
    for (id, rec) in st.metadata.xpack.iter(inference::SECRETS) {
        let Value::Obj(m) = rec else { continue };
        let settled: crate::json::Map = m
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().map_or_else(|| v.clone(), |s| Value::from(settle(sealer, branch, copied, inference::SECRET_KIND, &to, s)))))
            .collect();
        if &settled != m {
            let taken = settled.values().zip(m.values()).any(|(a, b)| a.as_str() == Some(INHERITED) && b.as_str() != Some(INHERITED));
            changes.push((inference::SECRETS, id.clone(), rec.clone(), Some(Value::Obj(settled)), taken));
        }
    }
    for (family, kind) in [(connectors::SECRETS, connectors::SECRET_KIND), (fleet::SECRETS, fleet::SECRET_KIND)] {
        for (id, rec) in st.metadata.xpack.iter(family) {
            let to = format!("{}:{id}", bind(cluster, kind));
            let next = match (rec.get("sealed").and_then(Value::as_str), rec.get("value")) {
                (Some(s), _) if sealer.open_text(&to, s).is_some() => continue,
                _ if branch && copied(&rec.to_json()) => None,
                (Some(s), _) => match sealer.open_text(kind, s) {
                    Some(plain) => Some(crate::obj! { "sealed" => sealer.seal_text(&to, &plain) }),
                    None => continue,
                },
                (None, Some(v)) => Some(crate::obj! { "sealed" => sealer.seal_text(&to, &v.to_json()) }),
                (None, None) => continue,
            };
            let taken = next.is_none();
            changes.push((family, id.clone(), rec.clone(), next, taken));
        }
    }
    for (id, rec) in st.metadata.xpack.iter(watcher::WATCHES) {
        let Some(Value::Obj(w)) = rec.get("watch") else { continue };
        let inherited = branch && crate::branch::inherited_watch(cluster, st, id);
        let sealed = watcher::parse::rebound_watch(cluster, w, sealer, inherited, copied);
        if &sealed != w {
            let taken = Value::Obj(sealed.clone()).to_json().matches(INHERITED).count() > Value::Obj(w.clone()).to_json().matches(INHERITED).count();
            let mut r = rec.clone();
            if let Value::Obj(m) = &mut r {
                m.insert("watch".into(), Value::Obj(sealed));
            }
            changes.push((watcher::WATCHES, id.clone(), rec.clone(), Some(r), taken));
        }
    }
    changes
}

/// Settles every secret a cluster keeps ([`settle`]) and persists it at once: seals what's kept in
/// the clear (stored on a node without a key) with this node's key, binds to the cluster what was
/// sealed before secrets were bound, and on a branch ([`crate::branch::is_branch`]) takes out
/// what it holds of its parent's: inference endpoints' keys and inherited watches' secrets become
/// [`INHERITED`], connector and Fleet secrets (`_connector/_secret`, `_fleet/secret`) are removed.
/// A branch made before binding can't tell its own secrets from its parent's by how they're
/// sealed: those equal byte for byte to a value in the states it was copied from
/// ([`crate::ClusterConfig::lineage`]) are the parent's, the rest its own, sealed to it. With
/// nothing to compare with, every one that isn't sealed as its own counts as the parent's.
/// Snapshot repositories' credentials are sealed too. A connector's own configuration values are
/// the connectors' to seal (`xpack::entsearch::sync::adopt_existing`, bound to the cluster and the
/// connector): this never touches them.
/// Run as the cluster opens; a record changed meanwhile is left for the next open. Should it
/// fail on a cluster that isn't a branch, values sealed under the bare kind still open until the
/// next load ([`open`]).
pub async fn seal_leftovers(cluster: &Cluster) -> EsResult<Leftovers> {
    let r = settle_leftovers(cluster).await;
    if r.is_err() && !crate::branch::is_branch(cluster, &cluster.state()) {
        cluster.unbound_secrets.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    r
}

async fn settle_leftovers(cluster: &Cluster) -> EsResult<Leftovers> {
    use crate::xpack::store;
    let Some(sealer) = cluster.config.sealer.clone() else { return Ok(Leftovers::default()) };
    let st = cluster.state();
    let branch = crate::branch::is_branch(cluster, &st);
    // On a branch, first as if everything not its own were copied, noting what that takes.
    let candidates = std::cell::Cell::new(false);
    let mut changes = plan(cluster, &st, &sealer, branch, &|_| {
        candidates.set(true);
        true
    });
    if candidates.get()
        && let Some(lineage) = cluster.config.lineage.clone()
    {
        let mut values = std::collections::HashSet::new();
        for raw in lineage.sources().await {
            secret_values(&raw, &mut values);
        }
        if !values.is_empty() {
            changes = plan(cluster, &st, &sealer, branch, &|v| values.contains(v));
        }
    }
    let repos = st.metadata.repositories.values().any(|r| r.secrets.values().any(|v| !is_sealed(v)));
    drop(st);
    if changes.is_empty() && !repos {
        return Ok(Leftovers::default());
    }
    let out = Leftovers { changed: changes.len() + usize::from(repos), taken: changes.iter().filter(|c| c.4).map(|c| format!("{}/{}", c.0, c.1)).collect() };
    let now = crate::time::now_millis();
    cluster
        .update(move |st| {
            for (family, id, read, next, _) in changes {
                if st.metadata.xpack.get(family, &id) != Some(&read) {
                    continue;
                }
                match next {
                    Some(v) => {
                        store::put(st, family, &id, v, now)?;
                    }
                    None => {
                        store::remove(st, family, &id);
                    }
                }
            }
            crate::api::snapshots::seal_stored_secrets(&mut st.metadata, Some(&sealer));
            Ok(())
        })
        .await?;
    Ok(out)
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
        // A sealed value copied from elsewhere (another cluster under the same key) is never
        // taken as the password: the watch is refused, and nothing of it stored.
        let planted = Sealer::from_secret("0123456789abcdef0123456789abcdef-k").unwrap().seal(super::super::xpack::watcher::http::PASSWORD_KIND, "victim-pw");
        let watch2 = watch.replace("pw-1", &planted);
        let r = call("PUT", "/_watcher/watch/w2?active=false", &watch2).await;
        assert!(r.status == 400 && String::from_utf8_lossy(&r.body).contains("found a sealed value"), "{}", String::from_utf8_lossy(&r.body));
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
        assert_eq!(seal_leftovers(&c).await.unwrap().changed, 3);
        let saved = String::from_utf8_lossy(&store.load().await.unwrap().unwrap()).into_owned();
        for secret in ["pw-1", "conn-3", "fleet-4"] {
            assert!(!saved.contains(secret), "{secret} still in the clear in the saved state");
        }
        let got = call(&c, "GET", &format!("/_connector/_secret/{id}"), "").await;
        assert!(String::from_utf8_lossy(&got.body).contains("conn-3"), "still opens");
        assert_eq!(seal_leftovers(&c).await.unwrap().changed, 0);
        // One sealed before secrets were bound to their cluster is bound to it as it opens.
        let bare = Sealer::from_secret("0123456789abcdef0123456789abcdef-k").unwrap().seal("connector secret", "\"bare-5\"");
        c.update(move |st| crate::xpack::store::put(st, crate::xpack::entsearch::connectors::SECRETS, "old", crate::obj! { "sealed" => bare }, 0).map(drop)).await.unwrap();
        assert_eq!(seal_leftovers(&c).await.unwrap().changed, 1);
        let got = call(&c, "GET", "/_connector/_secret/old", "").await;
        assert!(String::from_utf8_lossy(&got.body).contains("bare-5"), "{}", String::from_utf8_lossy(&got.body));
    }
}
