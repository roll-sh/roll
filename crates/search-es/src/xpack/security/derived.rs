//! Browser keys derived without a call to roll (docs/search/README.md, "Search from the browser";
//! Algolia's `generateSecuredApiKey`, Typesense's `generateScopedSearchKey`, Meilisearch's tenant
//! tokens). A SaaS's backend signs a short-lived key per end customer from one derivable browser
//! key (the parent), locally, and the page sends it as any key: `Authorization: ApiKey <derived>`.
//!
//! **The token** is `rdk1.<payload>.<mac>`: the payload is base64url (no padding) of one JSON
//! object, the mac base64url of HMAC-SHA256 over `rdk1.<payload>` (the version prefix included)
//! keyed with the parent's signing secret as written (`rdks_…`). One version, one algorithm,
//! nothing in the token chooses either. The payload's fields, unknown ones refused:
//!
//! - `key` (the parent's id) and `cluster` (its cluster's name, for routing), required;
//! - `exp`, required: unix seconds, at most [`MAX_AHEAD_S`] ahead and never past the parent's
//!   own expiry;
//! - `indices` (each covered by one of the parent's names or patterns) or `application` (the
//!   parent's own); left out, the parent's;
//! - `filter`: one query, ANDed with the parent's filter;
//! - `user`: the end customer the per-user rate limit is keyed by (Algolia's `userToken`), with
//!   `rate_limit.per_minute` at most its default share (a quarter of the parent's rate);
//! - `origins`: pages it may be sent from, each one the cluster's CORS allows (the node checks
//!   them: [`DerivedAuth::origins`]).
//!
//! **The signing secret** is a parent's `metadata.roll_browser.derive.secret`, sealed under the
//! node's key with the cluster's id and the key's id as associated data ([`seal_kind`]): it opens
//! only for that key of that cluster (a branch's copy is refused by the node as its browser keys
//! are; one rebound to the branch's id, or moved to another key, doesn't open). It is shown once, by roll's
//! create and rotate routes; the API key APIs print it redacted ([`REDACTED`]). Revoking the
//! parent or rotating its secret refuses every key derived from it with the next request: a
//! derived key is checked against its parent's stored state on every request, nothing cached.
//!
//! **What a derived key may do** is its parent's, narrowed: the principal is the parent's API key
//! with one more set of role descriptors every permission must also pass
//! ([`DerivedAuth::narrowing`], in [`super::Effective::also`]): its indices intersect the parent's,
//! and its filter is a second document-level query a document must also match. It is a browser
//! key, under the parent's policy and rate limits, and never reaches `_security`.

use super::api_keys::KEYS;
use super::browser::METADATA;
use super::{AuthKind, Principal};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::obj;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hkdf::hmac::{Hmac, Mac};

/// A derived key's version prefix (`rdk1.<payload>.<mac>`).
pub const PREFIX: &str = "rdk1.";
/// A signing secret's prefix.
pub const SECRET_PREFIX: &str = "rdks_";
/// The longest derived key, in bytes: a header's worth, and every field is bounded by it.
pub const MAX_TOKEN_BYTES: usize = 4096;
/// How far ahead a derived key may expire.
pub const MAX_AHEAD_S: i64 = 24 * 3600;
/// The clocks' difference a derived key's expiry allows for, both ways.
pub const SKEW_S: i64 = 30;
/// The deepest a derived key's filter may nest (objects and arrays).
pub const MAX_FILTER_DEPTH: usize = 24;
/// The longest `user`, in bytes.
pub const MAX_USER_BYTES: usize = 128;
/// The most `origins` a derived key may name.
pub const MAX_ORIGINS: usize = 32;
/// The most `indices` a derived key may name.
pub const MAX_INDICES: usize = 64;
/// `metadata.roll_browser.derive`: a derivable parent's signing secret and when it was made.
pub const DERIVE: &str = "derive";
/// What the API key APIs print in place of a signing secret; an update that sends it back keeps
/// the stored one.
pub const REDACTED: &str = "::es_redacted::";

/// What a derived key adds to its parent's API key authentication.
#[derive(Clone, Debug, PartialEq)]
pub struct DerivedAuth {
    /// Role descriptors every permission must also pass: the derived key's indices (or its
    /// parent's) with `read`, and its filter as their document-level `query`.
    pub narrowing: Value,
    /// The end customer its per-user rate limit is keyed by.
    pub user: Option<String>,
    /// Its user's rate, lower than the default share ([`user_share`]).
    pub per_minute: Option<u64>,
    /// Pages it may be sent from (as written: the node normalizes and checks them).
    pub origins: Vec<String>,
}

/// The kind a parent's signing secret is sealed as: bound to its cluster's id and its key's id.
pub fn seal_kind(cluster_id: &str, key_id: &str) -> String {
    format!("browser key signing secret {cluster_id}/{key_id}")
}

/// A new signing secret: 32 random bytes.
pub fn new_secret() -> String {
    let bytes: [u8; 32] = rand::random();
    format!("{SECRET_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// The derived key `secret` signs for `payload` (one JSON object, as the helpers write it).
pub fn sign(secret: &str, payload: &str) -> String {
    let signed = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(payload));
    let mut mac = <Hmac<sha2::Sha256> as Mac>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key");
    mac.update(signed.as_bytes());
    format!("{signed}.{}", URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

/// Whether an `ApiKey` header value is a derived key (by its prefix alone).
pub fn is_derived(value: &str) -> bool {
    value.starts_with(PREFIX)
}

/// A user's default (and largest) share of its parent's rate: a quarter, so one end customer
/// can't use up the key; lower when the derived key asks for less.
pub fn user_share(parent_per_minute: u64, asked: Option<u64>) -> u64 {
    let share = (parent_per_minute / 4).max(1);
    asked.map_or(share, |n| n.min(share))
}

/// A derived key, split and its payload parsed (not verified).
struct Token<'a> {
    /// `rdk1.<payload>`: what the mac signs.
    signed: &'a str,
    mac: Vec<u8>,
    claims: Value,
}

fn refused(why: impl std::fmt::Display) -> EsError {
    super::unauthorized_credentials(format!("API key: derived key refused: {why}"))
}

/// Splits and parses a derived key, bounded by its own bytes: one JSON object, no trailing
/// bytes, no duplicate keys.
fn split(token: &str) -> EsResult<Token<'_>> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(refused(format!("longer than {MAX_TOKEN_BYTES} bytes")));
    }
    let rest = token.strip_prefix(PREFIX).ok_or_else(|| refused("not an rdk1 key"))?;
    let (payload, mac) = rest.split_once('.').ok_or_else(|| refused("malformed"))?;
    let signed = &token[..PREFIX.len() + payload.len()];
    let mac = URL_SAFE_NO_PAD.decode(mac).ok().filter(|m| m.len() == 32).ok_or_else(|| refused("malformed signature"))?;
    let json = URL_SAFE_NO_PAD.decode(payload).map_err(|_| refused("malformed payload"))?;
    // Strict on both counts: serde_json refuses trailing bytes, the engine's parser duplicates.
    if serde_json::from_slice::<serde_json::Value>(&json).is_err() {
        return Err(refused("its payload isn't one JSON object"));
    }
    let claims = match crate::json::parse(&json) {
        Ok(Some(v @ Value::Obj(_))) => v,
        _ => return Err(refused("its payload isn't one JSON object")),
    };
    const FIELDS: &[&str] = &["key", "cluster", "exp", "indices", "application", "filter", "user", "origins", "rate_limit"];
    if let Some(k) = claims.as_object().into_iter().flat_map(|m| m.keys()).find(|k| !FIELDS.contains(&k.as_str())) {
        return Err(refused(format!("unknown field [{k}]")));
    }
    Ok(Token { signed, mac, claims })
}

/// The parent key id and cluster name a derived key names, read without verifying it (to route
/// it to its cluster, which verifies it).
pub fn names(token: &str) -> Option<(String, String)> {
    let t = split(token).ok()?;
    let s = |f: &str| t.claims.get(f).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_string);
    Some((s("key")?, s("cluster")?))
}

/// A parent's signing secret in the clear: sealed under this node's key for its cluster and key
/// (`None` past any doubt: no key to open it, another binding, or kept in the clear on a node
/// that doesn't keep secrets so).
fn signing_secret(cluster: &Cluster, browser: &Value, key_id: &str) -> Option<String> {
    let stored = browser.pointer(&[DERIVE, "secret"])?.as_str()?;
    let made_for = browser.get("cluster")?.as_str()?;
    if crate::seal::is_sealed(stored) {
        return cluster.config.sealer.as_ref()?.open_text(&seal_kind(made_for, key_id), stored);
    }
    (cluster.config.unsealed_secrets && stored.starts_with(SECRET_PREFIX)).then(|| stored.to_string())
}

fn depth(v: &Value) -> usize {
    match v {
        Value::Obj(m) => 1 + m.values().map(depth).max().unwrap_or(0),
        Value::Arr(a) => 1 + a.iter().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

/// Authenticates a derived key: its parent a live derivable browser key of this cluster, its
/// signature the parent's current secret's (compared in constant time), not expired, and every
/// field within its parent's. The principal is the parent's API key, narrowed.
pub fn authenticate(cluster: &Cluster, token: &str) -> EsResult<Principal> {
    let t = split(token)?;
    let c = &t.claims;
    let key_id = c.get("key").and_then(Value::as_str).ok_or_else(|| refused("[key] is required"))?;
    let st = cluster.state();
    let no_parent = || refused(format!("[{key_id}] isn't a derivable browser key of this cluster"));
    let Some(k) = st.metadata.xpack.get(KEYS, key_id) else { return Err(no_parent()) };
    let now_ms = crate::time::now_millis();
    if k.get("invalidated").and_then(Value::as_bool) == Some(true)
        || k.get("expiration").and_then(Value::as_i64).is_some_and(|e| e <= now_ms)
        || k.get("type").and_then(Value::as_str).is_some_and(|t| t != "rest")
    {
        return Err(no_parent());
    }
    let Some(browser) = k.pointer(&["metadata", METADATA]) else { return Err(no_parent()) };
    let Some(secret) = signing_secret(cluster, browser, key_id) else { return Err(no_parent()) };
    let mut mac = <Hmac<sha2::Sha256> as Mac>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key");
    mac.update(t.signed.as_bytes());
    if mac.verify_slice(&t.mac).is_err() {
        return Err(refused("its signature isn't its parent's (a rotated secret signs anew)"));
    }
    let derived = narrowed(c, browser, k, now_ms / 1000)?;
    let mut p = super::api_keys::principal_of(key_id, k);
    if let AuthKind::ApiKey(a) = &mut p.kind {
        a.derived = Some(Box::new(derived));
    }
    Ok(p)
}

/// Checks a signed payload's fields against its parent (`browser`, its `metadata.roll_browser`;
/// `k`, the stored key), refusing any that would widen it.
fn narrowed(c: &Value, browser: &Value, k: &Value, now_s: i64) -> EsResult<DerivedAuth> {
    let exp = c.get("exp").and_then(Value::as_i64).ok_or_else(|| refused("[exp] (unix seconds) is required"))?;
    if exp.saturating_add(SKEW_S) < now_s {
        return Err(refused("expired"));
    }
    if exp > now_s + MAX_AHEAD_S + SKEW_S {
        return Err(refused(format!("[exp] is more than {MAX_AHEAD_S} seconds ahead")));
    }
    if k.get("expiration").and_then(Value::as_i64).is_some_and(|parent| exp.saturating_mul(1000) > parent) {
        return Err(refused("[exp] is past its parent's expiry"));
    }
    let parent_indices: Option<Vec<&str>> = browser.get("indices").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect());
    let parent_app = browser.get("application").and_then(Value::as_str);
    let names: Vec<Value> = match (c.get("indices"), c.get("application")) {
        (Some(_), Some(_)) => return Err(refused("give [indices] or [application], not both")),
        (Some(i), None) => {
            let Some(parent) = &parent_indices else { return Err(refused("[indices] on a key of a search application")) };
            let list = i.as_array().filter(|a| !a.is_empty() && a.len() <= MAX_INDICES).ok_or_else(|| refused(format!("[indices]: 1 to {MAX_INDICES} names or patterns")))?;
            let mut out = Vec::with_capacity(list.len());
            for n in list {
                let n = n.as_str().ok_or_else(|| refused("[indices]: names or patterns"))?;
                super::browser::check_index_name(n).map_err(refused)?;
                // A pattern is within one of the parent's when that one matches it as written: the
                // parent's `*`s cover the derived pattern's (`logs-*` covers `logs-2026*`).
                if !parent.iter().any(|p| super::privileges::glob(p, n)) {
                    return Err(refused(format!("[indices]: [{n}] isn't within its parent's indices")));
                }
                out.push(Value::from(n));
            }
            out
        }
        (None, Some(a)) => {
            if a.as_str().is_none() || a.as_str() != parent_app {
                return Err(refused("[application] isn't its parent's"));
            }
            vec![a.clone()]
        }
        (None, None) => match (&parent_indices, parent_app) {
            (Some(p), _) => p.iter().map(|n| Value::from(*n)).collect(),
            (None, Some(a)) => vec![Value::from(a)],
            (None, None) => return Err(refused("its parent reaches nothing")),
        },
    };
    let mut group = obj! { "names" => Value::Arr(names), "privileges" => crate::arr!["read"] };
    match c.get("filter") {
        None | Some(Value::Null) => {}
        Some(f @ Value::Obj(m)) if m.len() == 1 => {
            if depth(f) > MAX_FILTER_DEPTH {
                return Err(refused(format!("[filter] nests deeper than {MAX_FILTER_DEPTH}")));
            }
            if let Some(g) = group.as_object_mut() {
                g.insert("query".into(), f.clone());
            }
        }
        Some(_) => return Err(refused("[filter]: one query, as an object")),
    }
    let mut descriptor = obj! { "cluster" => Value::Arr(Vec::new()), "indices" => Value::Arr(vec![group]) };
    if parent_app.is_some()
        && let Some(d) = descriptor.as_object_mut()
    {
        d.insert("restriction".into(), obj! { "workflows" => crate::arr!["search_application_query"] });
    }
    let user = match c.get("user") {
        None | Some(Value::Null) => None,
        Some(Value::Str(u)) if !u.is_empty() && u.len() <= MAX_USER_BYTES && !u.chars().any(char::is_control) => Some(u.to_string()),
        Some(_) => return Err(refused(format!("[user]: 1 to {MAX_USER_BYTES} bytes of text"))),
    };
    let per_minute = match c.get("rate_limit") {
        None | Some(Value::Null) => None,
        Some(Value::Obj(r)) if r.len() == 1 => {
            let n = r.get("per_minute").and_then(Value::as_i64).and_then(|n| u64::try_from(n).ok()).filter(|n| *n >= 1).ok_or_else(|| refused("[rate_limit]: {\"per_minute\": n}"))?;
            if user.is_none() {
                return Err(refused("[rate_limit] is its user's: give [user]"));
            }
            let parent = browser.pointer(&["rate_limit", "per_minute"]).and_then(Value::as_i64).and_then(|n| u64::try_from(n).ok()).unwrap_or(u64::MAX / 4);
            if n > user_share(parent, None) {
                return Err(refused(format!("[rate_limit.per_minute] is more than a user's share of its parent ({})", user_share(parent, None))));
            }
            Some(n)
        }
        Some(_) => return Err(refused("[rate_limit]: {\"per_minute\": n}")),
    };
    let origins = match c.get("origins") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Arr(a)) if !a.is_empty() && a.len() <= MAX_ORIGINS => {
            let o: Vec<String> = a.iter().filter_map(Value::as_str).filter(|o| o.len() <= 255).map(str::to_string).collect();
            if o.len() != a.len() {
                return Err(refused("[origins]: origins as strings"));
            }
            o
        }
        Some(_) => return Err(refused(format!("[origins]: 1 to {MAX_ORIGINS} origins"))),
    };
    Ok(DerivedAuth { narrowing: obj! { "roll_derived" => descriptor }, user, per_minute, origins })
}

/// The stored key `k` with its signing secret redacted ([`REDACTED`]), as the API key APIs
/// print and query it (`None`: it has none).
pub fn redacted(k: &Value) -> Option<Value> {
    let secret = k.pointer(&["metadata", METADATA, DERIVE, "secret"])?;
    if secret.as_str() == Some(REDACTED) {
        return None;
    }
    let mut k = k.clone();
    redact_metadata(k.as_object_mut()?.get_mut("metadata")?);
    Some(k)
}

/// An API key's `metadata` with its signing secret, if any, replaced by [`REDACTED`] (as the
/// APIs print it, and the audit log's copy of a create or update).
pub fn redact_metadata(md: &mut Value) {
    let slot = md.as_object_mut().and_then(|m| m.get_mut(METADATA)).and_then(Value::as_object_mut).and_then(|b| b.get_mut(DERIVE)).and_then(Value::as_object_mut);
    if let Some(d) = slot
        && d.contains_key("secret")
    {
        d.insert("secret".into(), Value::from(REDACTED));
    }
}

/// An update's `metadata` that sends [`REDACTED`] back keeps the stored key's signing secret.
pub fn keep_secret(new: &mut Value, stored: &Value) {
    let Some(kept) = stored.pointer(&["metadata", METADATA, DERIVE, "secret"]).cloned() else { return };
    let slot = new.as_object_mut().and_then(|m| m.get_mut(METADATA)).and_then(Value::as_object_mut).and_then(|b| b.get_mut(DERIVE)).and_then(Value::as_object_mut);
    if let Some(d) = slot
        && d.get("secret").and_then(Value::as_str) == Some(REDACTED)
    {
        d.insert("secret".into(), kept);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test vector the TypeScript helper's test checks too (apps/dashboard
    /// `search-derive.test.ts`): the same secret and payload sign the same key.
    const VECTOR: &str = include_str!("../../../tests/fixtures/derived-key-vector.json");

    #[test]
    fn the_shared_vector_signs_and_splits() {
        let v: serde_json::Value = serde_json::from_str(VECTOR).unwrap();
        let (secret, payload, key) = (v["secret"].as_str().unwrap(), v["payload"].as_str().unwrap(), v["key"].as_str().unwrap());
        assert_eq!(sign(secret, payload), key);
        let t = split(key).unwrap();
        let mut mac = <Hmac<sha2::Sha256> as Mac>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(t.signed.as_bytes());
        assert!(mac.verify_slice(&t.mac).is_ok());
        assert_eq!(names(key), Some(("Jx2kq3Z0RkKp1M7m0nQv".into(), "shop".into())));
    }

    #[test]
    fn malformed_keys_are_refused_from_their_bytes() {
        let ok = sign("rdks_x", r#"{"key":"k","cluster":"c","exp":1}"#);
        assert!(split(&ok).is_ok());
        for bad in [
            sign("rdks_x", r#"{"key":"k","cluster":"c","exp":1}{"exp":9}"#),
            sign("rdks_x", r#"{"key":"k","key":"j","cluster":"c","exp":1}"#),
            sign("rdks_x", r#"{"key":"k","cluster":"c","exp":1,"alg":"none"}"#),
            sign("rdks_x", r#"["key"]"#),
            ok.replacen("rdk1.", "rdk2.", 1),
            format!("{ok}="),
            format!("{}{}", ok, "A".repeat(MAX_TOKEN_BYTES)),
        ] {
            assert!(split(&bad).is_err(), "{bad}");
        }
    }
}
