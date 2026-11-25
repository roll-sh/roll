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
    /// Which derived key this is: the hash of its signed payload (`rdk1.<payload>`), so what one
    /// derived key stores for later (a scroll, a point in time, async results: none of them
    /// reachable through the browser policy today) is never another's of the same parent
    /// ([`crate::search::contexts::owner_of`], [`super::resource_owner`]).
    pub id: String,
    /// Role descriptors every permission must also pass: the derived key's indices (or its
    /// parent's) with `read`, and its filter as their document-level `query`.
    pub narrowing: Value,
    /// The end customer its per-user rate limit is keyed by.
    pub user: Option<String>,
    /// Its user's rate, lower than the default share ([`user_share`]).
    pub per_minute: Option<u64>,
    /// Pages it may be sent from (as written: the node normalizes and checks them).
    pub origins: Vec<String>,
    /// When it stops working (unix seconds).
    pub exp: i64,
    /// The generation of the parent's secret that signed it: a rotation after its verification
    /// stops the rest of its request too ([`verified`]).
    pub generation: i64,
}

/// A derived key the node verified on this request ([`crate::rest::Verified::Derived`]): its
/// parent's id and what it may do, never the key or anything secret.
#[derive(Clone, Debug, PartialEq)]
pub struct VerifiedKey {
    pub key: String,
    pub auth: DerivedAuth,
}

/// The principal of a derived key the node verified on this request: it isn't verified again,
/// but its parent must still be a live derivable browser key whose secret is still the one that
/// signed it, and the key unexpired, so a revoke, a rotation or an expiry between the node's
/// check and any of the request's internal calls holds.
pub fn verified(cluster: &Cluster, v: &VerifiedKey) -> EsResult<Principal> {
    let st = cluster.state();
    let Some(k) = st.metadata.xpack.get(KEYS, &v.key) else { return Err(unverified()) };
    let now_ms = crate::time::now_millis();
    let generation = k.pointer(&["metadata", METADATA, DERIVE]).filter(|d| !d.is_null()).map(|d| d.get("generation").and_then(Value::as_i64).unwrap_or(0));
    if k.get("invalidated").and_then(Value::as_bool) == Some(true)
        || k.get("expiration").and_then(Value::as_i64).is_some_and(|e| e <= now_ms)
        || generation != Some(v.auth.generation)
        || v.auth.exp.saturating_add(SKEW_S) < now_ms / 1000
    {
        return Err(unverified());
    }
    let mut p = super::api_keys::principal_of(&v.key, k);
    if let AuthKind::ApiKey(a) = &mut p.kind {
        a.derived = Some(Box::new(v.auth.clone()));
    }
    Ok(p)
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

/// A refusal once the signature is verified: only the secret's holder reads why. It names the
/// field, never a value from the key.
fn refused(why: impl std::fmt::Display) -> EsError {
    super::unauthorized_credentials(format!("API key: derived key refused: {why}"))
}

/// The one answer before the signature is verified, whatever failed (the key's form, its
/// cluster, its parent, the signature): nothing about which clusters a node holds or which keys
/// they have reaches anyone without the secret.
fn unverified() -> EsError {
    super::unauthorized_credentials("API key: derived key refused")
}

/// Splits and parses a derived key, bounded by its own bytes: one JSON object, no trailing
/// bytes, no duplicate keys.
fn split(token: &str) -> EsResult<Token<'_>> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(unverified());
    }
    let rest = token.strip_prefix(PREFIX).ok_or_else(unverified)?;
    let (payload, mac) = rest.split_once('.').ok_or_else(unverified)?;
    let signed = &token[..PREFIX.len() + payload.len()];
    let mac = URL_SAFE_NO_PAD.decode(mac).ok().filter(|m| m.len() == 32).ok_or_else(unverified)?;
    let json = URL_SAFE_NO_PAD.decode(payload).map_err(|_| unverified())?;
    // Strict on both counts: serde_json refuses trailing bytes, the engine's parser duplicates.
    if serde_json::from_slice::<serde_json::Value>(&json).is_err() {
        return Err(unverified());
    }
    let claims = match crate::json::parse(&json) {
        Ok(Some(v @ Value::Obj(_))) => v,
        _ => return Err(unverified()),
    };
    Ok(Token { signed, mac, claims })
}

/// The fields a derived key may hold.
const FIELDS: &[&str] = &["key", "cluster", "exp", "indices", "application", "filter", "user", "origins", "rate_limit"];

/// The parent key id and cluster name a derived key names, read without verifying it (to route
/// it to its cluster, which verifies it).
pub fn names(token: &str) -> Option<(String, String)> {
    let t = split(token).ok()?;
    let s = |f: &str| t.claims.get(f).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_string);
    Some((s("key")?, s("cluster")?))
}

/// A parent's signing secret in the clear: sealed under this node's key for the cluster it's
/// loaded as (`catalog_id`) and the key (`None` past any doubt: no key to open it, another
/// binding, or kept in the clear on a node that doesn't keep secrets so).
fn signing_secret(cluster: &Cluster, browser: &Value, catalog_id: &str, key_id: &str) -> Option<String> {
    let stored = browser.pointer(&[DERIVE, "secret"])?.as_str()?;
    if crate::seal::is_sealed(stored) {
        return cluster.config.sealer.as_ref()?.open_text(&seal_kind(catalog_id, key_id), stored);
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

/// Authenticates a derived key: named for this cluster, its parent a live derivable browser key
/// of this cluster (by its id in roll's catalog: a branch, a restore elsewhere or a node without
/// a catalog has none of its keys), its signature the parent's current secret's (compared in
/// constant time), then not expired and every field within its parent's. The principal is the
/// parent's API key, narrowed. Everything before the signature is refused alike ([`unverified`]).
pub fn authenticate(cluster: &Cluster, token: &str) -> EsResult<Principal> {
    let t = split(token)?;
    let c = &t.claims;
    let key_id = c.get("key").and_then(Value::as_str).ok_or_else(unverified)?;
    if c.get("cluster").and_then(Value::as_str) != Some(cluster.name.as_str()) {
        return Err(unverified());
    }
    let catalog_id = cluster.config.catalog_id.as_deref().ok_or_else(unverified)?;
    let st = cluster.state();
    let Some(k) = st.metadata.xpack.get(KEYS, key_id) else { return Err(unverified()) };
    let now_ms = crate::time::now_millis();
    if k.get("invalidated").and_then(Value::as_bool) == Some(true)
        || k.get("expiration").and_then(Value::as_i64).is_some_and(|e| e <= now_ms)
        || k.get("type").and_then(Value::as_str).is_some_and(|t| t != "rest")
    {
        return Err(unverified());
    }
    let Some(browser) = k.pointer(&["metadata", METADATA]) else { return Err(unverified()) };
    if browser.get("cluster").and_then(Value::as_str) != Some(catalog_id) {
        return Err(unverified());
    }
    let Some(secret) = signing_secret(cluster, browser, catalog_id, key_id) else { return Err(unverified()) };
    let mut mac = <Hmac<sha2::Sha256> as Mac>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key");
    mac.update(t.signed.as_bytes());
    if mac.verify_slice(&t.mac).is_err() {
        return Err(unverified());
    }
    if c.as_object().is_some_and(|m| m.keys().any(|k| !FIELDS.contains(&k.as_str()))) {
        return Err(refused(format!("a field it doesn't know: only {}", FIELDS.join(", "))));
    }
    let mut derived = narrowed(&st, c, browser, k, now_ms / 1000)?;
    derived.generation = browser.pointer(&[DERIVE, "generation"]).and_then(Value::as_i64).unwrap_or(0);
    derived.id = <sha2::Sha256 as sha2::Digest>::digest(t.signed.as_bytes())[..16].iter().map(|b| format!("{b:02x}")).collect();
    let mut p = super::api_keys::principal_of(key_id, k);
    if let AuthKind::ApiKey(a) = &mut p.kind {
        a.derived = Some(Box::new(derived));
    }
    Ok(p)
}

/// Whether filter `q` (a [`plain_filter`]) expands terms: `prefix`, `wildcard`,
/// `match_phrase_prefix`, `match_bool_prefix`, `simple_query_string` (its `*` and `~`), or a
/// `fuzziness` or `max_expansions` in a match query. A browser key's own queries do so under its
/// policy (`max_expansions` capped); a filter is built as the owner's, outside it, so only a parent
/// with `allow_expensive_queries` lets its derived keys' filters expand.
fn expands(q: &Value) -> bool {
    fn names(v: &Value, keys: &[&str]) -> bool {
        v.as_object().is_some_and(|m| m.iter().any(|(k, x)| keys.contains(&k.as_str()) || names(x, keys)))
    }
    let Some((kind, body)) = q.as_object().and_then(|m| m.iter().next()) else { return false };
    let queries = |v: &Value| match v {
        Value::Arr(a) => a.iter().any(expands),
        q => expands(q),
    };
    match kind.as_str() {
        "prefix" | "wildcard" | "match_phrase_prefix" | "match_bool_prefix" | "simple_query_string" => true,
        "match" | "match_phrase" => names(body, &["fuzziness", "max_expansions"]),
        "multi_match" => names(body, &["fuzziness", "max_expansions"]) || matches!(body.get("type").and_then(Value::as_str), Some("phrase_prefix" | "bool_prefix")),
        "bool" => ["must", "should", "filter", "must_not"].iter().filter_map(|k| body.get(k)).any(queries),
        "constant_score" => body.get("filter").is_some_and(queries),
        "nested" => body.get("query").is_some_and(queries),
        _ => false,
    }
}

/// The fields the `range` queries of filter `q` (a [`plain_filter`]) test.
fn range_fields<'a>(q: &'a Value, out: &mut Vec<&'a str>) {
    let Some((kind, body)) = q.as_object().and_then(|m| m.iter().next()) else { return };
    let children: Vec<&'a Value> = match kind.as_str() {
        "range" => {
            out.extend(body.as_object().into_iter().flat_map(|m| m.keys()).filter(|k| !matches!(k.as_str(), "boost" | "_name")).map(String::as_str));
            return;
        }
        "bool" => ["must", "should", "filter", "must_not"].iter().filter_map(|k| body.get(k)).collect(),
        "constant_score" => body.get("filter").into_iter().collect(),
        "nested" => body.get("query").into_iter().collect(),
        _ => return,
    };
    for c in children {
        match c {
            Value::Arr(a) => a.iter().for_each(|q| range_fields(q, out)),
            q => range_fields(q, out),
        }
    }
}

/// The indices a derived key reaches (`reach`: the names and patterns its narrowing grants): by
/// name or alias, and the backing indices of the data streams (and data stream aliases) it names.
fn reached<'a>(st: &'a crate::meta::ClusterState, reach: &[String]) -> Vec<&'a crate::meta::IndexMeta> {
    let hit = |n: &str| reach.iter().any(|p| super::privileges::glob(p, n));
    let mut backing: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut stream = |name: &str| {
        if let Some(d) = st.metadata.data_streams.get(name) {
            backing.extend(d.indices.iter().map(String::as_str));
        }
    };
    for d in st.metadata.data_streams.values().filter(|d| hit(&d.name)) {
        stream(&d.name);
    }
    for (alias, a) in &st.metadata.data_stream_aliases {
        if hit(alias) {
            a.data_streams.iter().for_each(|d| stream(d));
        }
    }
    st.metadata.indices.values().filter(|i| hit(&i.name) || i.aliases.keys().any(|a| hit(a)) || backing.contains(i.name.as_str())).map(|i| &**i).collect()
}

/// Whether `path` is a string in mapping `m` (text, keyword and their kin), a keyed subfield of a
/// `flattened` field (`labels.foo`) included: a range over it walks the terms between its bounds.
fn string_field(m: &crate::mapping::Mapping, path: &str) -> bool {
    const STRINGS: &[&str] = &["text", "match_only_text", "keyword", "wildcard", "version", "flattened", "alias"];
    if let Some(r) = m.lookup(path) {
        return STRINGS.contains(&r.type_name());
    }
    let mut at = path.len();
    while let Some(i) = path[..at].rfind('.') {
        at = i;
        if m.lookup(&path[..at]).is_some_and(|r| r.type_name() == "flattened") {
            return true;
        }
    }
    false
}

/// The fields the queries of filter `q` (a [`plain_filter`]) name, as written (a query's own
/// parameters may come along: only ever a reason to refuse more); `*` where one searches its
/// index's default fields.
fn filter_fields<'a>(q: &'a Value, out: &mut Vec<&'a str>) {
    const PARAMS: &[&str] = &["boost", "_name", "validation_method", "ignore_unmapped", "distance", "distance_type", "type"];
    let Some((kind, body)) = q.as_object().and_then(|m| m.iter().next()) else { return };
    let children: Vec<&'a Value> = match kind.as_str() {
        "bool" => ["must", "should", "filter", "must_not"].iter().filter_map(|k| body.get(k)).collect(),
        "constant_score" => body.get("filter").into_iter().collect(),
        "nested" => body.get("query").into_iter().collect(),
        "ids" | "match_all" | "match_none" => return,
        "exists" => {
            out.extend(body.get("field").and_then(Value::as_str));
            return;
        }
        "multi_match" | "simple_query_string" => {
            match body.get("fields").and_then(Value::as_array) {
                Some(f) => out.extend(f.iter().filter_map(Value::as_str).map(|f| f.split('^').next().unwrap_or(f))),
                None => out.push("*"),
            }
            return;
        }
        _ => {
            out.extend(body.as_object().into_iter().flat_map(|m| m.keys()).map(String::as_str).filter(|k| !PARAMS.contains(k)));
            return;
        }
    };
    for c in children {
        match c {
            Value::Arr(a) => a.iter().for_each(|q| filter_fields(q, out)),
            q => filter_fields(q, out),
        }
    }
}

/// Whether one of `fields` (names or patterns) is a runtime field of an index in `indices`: a
/// query on one runs its script for every document, and a filter is built as the owner's,
/// outside the browser policy.
fn names_runtime_field(indices: &[&crate::meta::IndexMeta], fields: &[&str]) -> bool {
    indices.iter().any(|i| i.mapping.runtime.keys().any(|r| fields.iter().any(|f| super::privileges::glob(f, r))))
}

/// Whether `q` is a filter a derived key may carry: queries on the document itself, combined
/// with `bool`, `constant_score` and `nested`. Nothing that reads another document or index
/// (terms lookups, indexed shapes, `more_like_this`, joins, `percolate`), runs Painless
/// (`script`, `script_score`, `function_score`), renders a template or wraps an encoded query
/// (`wrapper`): its filter is built as the owner's, outside the browser policy, so each of
/// those would reach past the parent's own access.
fn plain_filter(q: &Value) -> bool {
    let Some((kind, body)) = q.as_object().filter(|m| m.len() == 1).and_then(|m| m.iter().next()) else { return false };
    let queries = |v: &Value| match v {
        Value::Arr(a) => a.iter().all(plain_filter),
        q => plain_filter(q),
    };
    let keys = |allowed: &[&str], nested: &[&str]| {
        body.as_object().is_some_and(|b| b.iter().all(|(k, v)| if nested.contains(&k.as_str()) { queries(v) } else { allowed.contains(&k.as_str()) }))
    };
    match kind.as_str() {
        "term" | "range" | "exists" | "ids" | "prefix" | "wildcard" | "match" | "match_phrase" | "match_phrase_prefix" | "match_bool_prefix" | "multi_match"
        | "match_all" | "match_none" | "simple_query_string" | "geo_bounding_box" | "geo_distance" | "geo_polygon" => true,
        // Lists of values only: an object is a lookup of another document.
        "terms" => body.as_object().is_some_and(|b| b.iter().all(|(k, v)| v.as_array().is_some() || k == "boost" || k == "_name")),
        "bool" => keys(&["minimum_should_match", "boost", "_name", "adjust_pure_negative"], &["must", "should", "filter", "must_not"]),
        "constant_score" => body.get("filter").is_some() && keys(&["boost", "_name"], &["filter"]),
        "nested" => body.get("query").is_some() && keys(&["path", "score_mode", "ignore_unmapped", "boost", "_name"], &["query"]),
        _ => false,
    }
}

/// Checks a signed payload's fields against its parent (`browser`, its `metadata.roll_browser`;
/// `k`, the stored key), refusing any that would widen it.
fn narrowed(st: &crate::meta::ClusterState, c: &Value, browser: &Value, k: &Value, now_s: i64) -> EsResult<DerivedAuth> {
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
                super::browser::check_index_name(n).map_err(|why| refused(format!("[indices]: a name {why}")))?;
                // A pattern is within one of the parent's when that one matches it as written: the
                // parent's `*`s cover the derived pattern's (`logs-*` covers `logs-2026*`).
                if !parent.iter().any(|p| super::privileges::glob(p, n)) {
                    return Err(refused("[indices]: a name isn't within its parent's indices"));
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
    let reach: Vec<String> = names.iter().filter_map(Value::as_str).map(str::to_string).collect();
    let mut group = obj! { "names" => Value::Arr(names), "privileges" => crate::arr!["read"] };
    match c.get("filter") {
        None | Some(Value::Null) => {}
        Some(f @ Value::Obj(m)) if m.len() == 1 => {
            if depth(f) > MAX_FILTER_DEPTH {
                return Err(refused(format!("[filter] nests deeper than {MAX_FILTER_DEPTH}")));
            }
            if !plain_filter(f) {
                return Err(refused("[filter]: only queries on the document itself (term, terms of values, range, exists, match… in bool, constant_score or nested): no lookups, joins, scripts, templates or wrapped queries"));
            }
            // Read from the mappings as they are on each request, so a field mapped later is
            // caught then.
            let indices = reached(st, &reach);
            let mut named = Vec::new();
            filter_fields(f, &mut named);
            if names_runtime_field(&indices, &named) {
                return Err(refused("[filter]: a query on a runtime field runs a script for every document: a derived key's filter can't"));
            }
            if !super::browser::Policy::of(browser).allow_expensive_queries {
                if expands(f) {
                    return Err(refused("[filter]: expanding terms (prefix, wildcard, the prefix match queries, simple_query_string, fuzziness, max_expansions) needs its parent's limits.allow_expensive_queries"));
                }
                let mut ranged = Vec::new();
                range_fields(f, &mut ranged);
                if indices.iter().any(|i| ranged.iter().any(|f| string_field(&i.mapping, f))) {
                    return Err(refused("[filter]: a range on a text or keyword field (its terms are walked, as a prefix's are) needs its parent's limits.allow_expensive_queries"));
                }
            }
            // As a role's query is checked when it's put, then as the cluster will build it.
            if super::users::check_role_query(&f.to_json()).is_err() || crate::query::parse_stored(f).is_err() {
                return Err(refused("[filter] isn't a query the cluster can build"));
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
    Ok(DerivedAuth { id: String::new(), narrowing: obj! { "roll_derived" => descriptor }, user, per_minute, origins, exp, generation: 0 })
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

/// What an API key update's `metadata` (`k`'s new one; `stored`, the key as stored) does to a
/// browser key's `derive`, applied inside the state update:
/// - a `roll_browser` without `derive` keeps the stored one (`"derive": null` removes it);
/// - `derive.secret` sent back as [`REDACTED`] (as the APIs print it) keeps the stored one;
/// - a new secret (roll's create and rotate) must be the next `generation` of the stored one, so
///   of two rotations racing only one is stored: the other is a 409, and its secret is never
///   given out.
pub fn on_update(new: &mut Value, stored: &Value) -> EsResult<()> {
    let kept = stored.pointer(&["metadata", METADATA, DERIVE]).filter(|d| !d.is_null()).cloned();
    let Some(b) = new.as_object_mut().and_then(|m| m.get_mut(METADATA)).and_then(Value::as_object_mut) else { return Ok(()) };
    let generation = |d: &Value| d.get("generation").and_then(Value::as_i64).unwrap_or(0);
    match b.get(DERIVE) {
        None => {
            if let Some(d) = kept {
                b.insert(DERIVE.into(), d);
            }
        }
        Some(Value::Null) => {
            b.shift_remove(DERIVE);
        }
        Some(d) if d.get("secret").and_then(Value::as_str) == Some(REDACTED) => match kept {
            Some(d) => {
                b.insert(DERIVE.into(), d);
            }
            None => {
                b.shift_remove(DERIVE);
            }
        },
        Some(d) => {
            // A key's first is 1, so a stored one past `i64::MAX - 1` was never made here.
            let Some(next) = kept.as_ref().map_or(0, generation).checked_add(1) else {
                return Err(EsError::new(409, "version_conflict_engine_exception", format!("[{METADATA}.{DERIVE}]: its generation can't advance: make a new browser key")));
            };
            if generation(d) != next {
                return Err(EsError::new(409, "version_conflict_engine_exception", format!("[{METADATA}.{DERIVE}]: a new signing secret must be generation [{next}]; another took its place")));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test vector the TypeScript helper's test checks too (apps/dashboard
    /// `search-derive.test.ts`): the same secret and payload sign the same key.
    const VECTOR: &str = include_str!("../../../tests/fixtures/derived-key-vector.json");

    /// An update's `derive`: left out, kept; `null`, removed; a new secret only as the next
    /// generation, never past `i64::MAX`.
    #[test]
    fn updates_keep_derive_and_advance_its_generation_once() {
        let stored = |generation: i64| obj! { "metadata" => obj! { METADATA => obj! { DERIVE => obj! { "secret" => "s", "generation" => generation } } } };
        let update = |derive: Option<Value>| {
            let mut b = obj! { "indices" => crate::arr!["i"] };
            if let (Some(d), Some(m)) = (derive, b.as_object_mut()) {
                m.insert(DERIVE.into(), d);
            }
            obj! { METADATA => b }
        };
        let secret = |generation: i64| obj! { "secret" => "new", "generation" => generation };
        let mut kept = update(None);
        on_update(&mut kept, &stored(3)).unwrap();
        assert_eq!(kept.pointer(&[METADATA, DERIVE, "generation"]).and_then(Value::as_i64), Some(3), "left out: kept");
        let mut removed = update(Some(Value::Null));
        on_update(&mut removed, &stored(3)).unwrap();
        assert!(removed.pointer(&[METADATA, DERIVE]).is_none(), "null: removed");
        assert!(on_update(&mut update(Some(secret(4))), &stored(3)).is_ok(), "the next generation");
        for (g, at) in [(3, 3), (5, 3), (2, 3), (1, i64::MAX)] {
            let e = on_update(&mut update(Some(secret(g))), &stored(at)).unwrap_err();
            assert_eq!(e.status, 409, "generation {g} over a stored {at}");
        }
        // A new key starts at 1.
        assert!(on_update(&mut update(Some(secret(1))), &Value::Null).is_ok());
        assert_eq!(on_update(&mut update(Some(secret(i64::MAX))), &Value::Null).unwrap_err().status, 409);
    }

    async fn call(c: &Cluster, method: &str, uri: &str, body: &str) -> (u16, Value) {
        let req = if body.is_empty() { crate::rest::EsRequest::new(method, uri) } else { crate::rest::EsRequest::json(method, uri, body.to_string()) };
        let r = crate::rest::handle(c, req).await;
        (r.status, crate::json::parse(&r.body).ok().flatten().unwrap_or(Value::Null))
    }

    /// A derived key is checked against the cluster that verifies it, whatever routed it there:
    /// named for another cluster it's refused; on a node without a catalog (`search-dev`) it is
    /// never accepted. A planted generation is refused when the parent is made.
    #[tokio::test]
    async fn the_cluster_checking_a_key_must_be_the_one_it_names() {
        let make = |name: &str, catalog: Option<&str>| {
            let mut cfg = crate::ClusterConfig::new(name);
            cfg.catalog_id = catalog.map(str::to_string);
            cfg.unsealed_secrets = true;
            cfg
        };
        let key = |generation: i64| {
            format!(
                r#"{{"name":"saas","role_descriptors":{{"r":{{"indices":[{{"names":["i"],"privileges":["read"]}}]}}}},"metadata":{{"roll_browser":{{"cluster":"srch_1","indices":["i"],"rate_limit":{{"per_minute":100}},"derive":{{"secret":"rdks_t","generation":{generation}}}}}}}}}"#
            )
        };
        let exp = crate::time::now_millis() / 1000 + 600;
        for (name, catalog) in [("shop", Some("srch_1")), ("dev", None)] {
            let c = Cluster::open(make(name, catalog), std::sync::Arc::new(crate::MemoryStore::new())).await.unwrap();
            let (s, v) = call(&c, "POST", "/_security/api_key", &key(i64::MAX)).await;
            assert_eq!(s, 409, "a planted generation: {v}");
            let (s, v) = call(&c, "POST", "/_security/api_key", &key(1)).await;
            assert_eq!(s, 200, "{v}");
            let id = v.get("id").and_then(Value::as_str).unwrap();
            let token = |cluster: &str| sign("rdks_t", &format!(r#"{{"key":"{id}","cluster":"{cluster}","exp":{exp}}}"#));
            let mine = authenticate(&c, &token(name));
            if catalog.is_some() {
                let p = mine.expect("named for this cluster");
                let AuthKind::ApiKey(a) = &p.kind else { panic!("an API key") };
                let v = VerifiedKey { key: id.to_string(), auth: a.derived.as_deref().expect("narrowed").clone() };
                assert!(verified(&c, &v).is_ok(), "as the node verified it");
                let stale = VerifiedKey { auth: DerivedAuth { exp: exp - 3600, ..v.auth.clone() }, ..v.clone() };
                assert!(verified(&c, &stale).is_err(), "expired since");
                // A rotation after the node's check stops the rest of the request too.
                let body = r#"{"metadata":{"roll_browser":{"cluster":"srch_1","indices":["i"],"rate_limit":{"per_minute":100},"derive":{"secret":"rdks_u","generation":2}}}}"#;
                let (s, r) = call(&c, "PUT", &format!("/_security/api_key/{id}"), body).await;
                assert_eq!(s, 200, "{r}");
                assert!(verified(&c, &v).is_err(), "its secret rotated since");
            } else {
                assert!(mine.is_err(), "no catalog id to bind the secret to");
            }
            assert!(authenticate(&c, &token("other")).is_err(), "named for another cluster");
        }
    }

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

    /// Before the signature is checked, from the bytes alone, and all with one answer (an
    /// unknown field is refused once the signature verifies: the control tests).
    #[test]
    fn malformed_keys_are_refused_from_their_bytes() {
        let ok = sign("rdks_x", r#"{"key":"k","cluster":"c","exp":1}"#);
        assert!(split(&ok).is_ok());
        for bad in [
            sign("rdks_x", r#"{"key":"k","cluster":"c","exp":1}{"exp":9}"#),
            sign("rdks_x", r#"{"key":"k","key":"j","cluster":"c","exp":1}"#),
            sign("rdks_x", r#"["key"]"#),
            ok.replacen("rdk1.", "rdk2.", 1),
            format!("{ok}="),
            format!("{}{}", ok, "A".repeat(MAX_TOKEN_BYTES)),
        ] {
            let e = split(&bad).err().expect(&bad);
            let detail = e.meta().iter().find(|(k, _)| k == "additional_unsuccessful_credentials").and_then(|(_, v)| v.as_str()).map(str::to_string);
            assert_eq!(detail.as_deref(), Some("API key: derived key refused"), "{bad}");
        }
    }
}
