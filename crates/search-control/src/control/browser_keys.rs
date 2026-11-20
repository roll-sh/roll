//! Keys a web page can ship (docs/search/README.md, "Search from the browser"; Algolia's
//! search-only and secured API keys, Typesense's scoped keys, Meilisearch's tenant tokens).
//!
//! A browser key is an ordinary API key of the cluster, made through its own
//! `_security/api_key`, so clients need nothing new:
//!
//! - one role descriptor with `read` on the named indices, or on one search application with
//!   `restriction.workflows: ["search_application_query"]` (it can then only run that
//!   application's search template); no cluster privilege, so no write, no `manage` and no
//!   `_security`;
//! - an optional filter as the role's document-level `query` (Algolia's secured-key filters), and
//!   an optional `expiration`;
//! - `metadata.roll_browser`: what it was made for and its rate limit, which the cluster's owner
//!   node enforces ([`search_server::browser`]).
//!
//! The encoded key is in the create answer only. Listing reads the cluster's keys back; revoking
//! invalidates one, refusing any key that isn't a browser key.

use super::{Control, ControlError, Result, connection_of};
use base64::Engine;
use search_es::EsRequest;
use search_server::browser::{MAX_PER_MINUTE, METADATA};
use serde::Deserialize;
use serde_json::{Value, json};

/// A key's rate limit when none is given: ten requests a second, from all its pages together.
pub const DEFAULT_PER_MINUTE: u64 = 600;
/// A key's limit per client address when none is given: one visitor can't use up the key.
pub const DEFAULT_PER_IP_PER_MINUTE: u64 = 60;
/// The most browser keys a cluster may have.
pub const MAX_KEYS: usize = 20;
/// The most indices one key may name.
const MAX_INDICES: usize = 64;
/// The largest filter, as JSON.
const MAX_FILTER_BYTES: usize = 16 * 1024;

/// `POST /v1/search/clusters/{id}/browser-keys`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewBrowserKey {
    pub name: String,
    /// What it may search: indices (names or patterns) …
    #[serde(default)]
    pub indices: Option<Vec<String>>,
    /// … or one search application (then only `_application/search_application/{name}/_search`).
    #[serde(default)]
    pub application: Option<String>,
    /// A query every search through the key is filtered by (document-level security).
    #[serde(default)]
    pub filter: Option<Value>,
    #[serde(default)]
    pub expires_in_s: Option<i64>,
    #[serde(default)]
    pub rate_limit: Option<RateLimit>,
    /// What its requests may cost (`search_es::xpack::security::browser::Policy`, and
    /// `max_concurrent`): the safe defaults when left out, widened by the owner.
    #[serde(default)]
    pub limits: Option<Limits>,
}

/// A browser key's request limits; every field optional.
#[derive(Clone, Debug, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_result_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_buckets: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_items: Option<u64>,
    /// Distinct texts one request may embed (4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_inference: Option<u64>,
    /// The most documents a `text_similarity_reranker` reranks (100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rerank_window: Option<u64>,
    /// Inference endpoints besides those of the indices' `semantic_text` fields (a reranker's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_endpoints: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_scripts: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_expensive_queries: Option<bool>,
}

/// What a revoke did.
pub enum Revoked {
    Now,
    /// The cluster is deleted: the revoke is kept and applied by its restore.
    AtRestore,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    #[serde(default)]
    pub per_minute: Option<u64>,
    #[serde(default)]
    pub per_ip_per_minute: Option<u64>,
}

/// What a browser key reaches, as made and as listed.
fn reach(k: &NewBrowserKey) -> Result<Value> {
    let name = k.name.trim();
    if name.is_empty() || name.len() > 1024 {
        return Err(ControlError::Invalid("name: 1 to 1024 characters".into()));
    }
    let (indices, application) = match (&k.indices, &k.application) {
        (Some(i), None) if !i.is_empty() => {
            if i.len() > MAX_INDICES {
                return Err(ControlError::Invalid(format!("indices: at most {MAX_INDICES}")));
            }
            for n in i {
                if n.is_empty() || n.len() > 255 || n.contains([',', ' ', '"', '\\', '/', '?', '#']) || n.starts_with(['-', '+', '_']) {
                    return Err(ControlError::Invalid(format!("indices: {n:?} isn't an index name or pattern")));
                }
                if n.chars().all(|c| c == '*') {
                    return Err(ControlError::Invalid(format!("indices: {n:?} would reach every index, hidden ones included: name the indices or a pattern of them")));
                }
                if n.starts_with('.') {
                    return Err(ControlError::Invalid(format!("indices: {n:?} is a hidden or system index: a browser key can't reach those")));
                }
            }
            (Some(i.clone()), None)
        }
        (None, Some(a)) if !a.is_empty() => {
            if a.len() > 255 || a.contains([',', ' ', '"', '\\', '/', '?', '#', '*']) || a.starts_with(['-', '+', '_', '.']) || a != &a.to_lowercase() {
                return Err(ControlError::Invalid(format!("application: {a:?} isn't a search application's name")));
            }
            (None, Some(a.clone()))
        }
        _ => return Err(ControlError::Invalid("give either indices (a list of names or patterns) or application (one search application)".into())),
    };
    let filter = match &k.filter {
        None | Some(Value::Null) => None,
        Some(f @ Value::Object(m)) if m.len() == 1 => {
            if serde_json::to_vec(f).map_or(usize::MAX, |b| b.len()) > MAX_FILTER_BYTES {
                return Err(ControlError::Invalid(format!("filter: at most {MAX_FILTER_BYTES} bytes of JSON")));
            }
            Some(f.clone())
        }
        Some(_) => return Err(ControlError::Invalid("filter: one query, as an object (e.g. {\"term\": {\"public\": true}})".into())),
    };
    let r = k.rate_limit.unwrap_or(RateLimit { per_minute: None, per_ip_per_minute: None });
    let per_minute = r.per_minute.unwrap_or(DEFAULT_PER_MINUTE);
    let per_ip = r.per_ip_per_minute.unwrap_or(DEFAULT_PER_IP_PER_MINUTE);
    let ok = |n: u64| (1..=MAX_PER_MINUTE).contains(&n);
    if !ok(per_minute) || !ok(per_ip) {
        return Err(ControlError::Invalid(format!("rate_limit: 1 to {MAX_PER_MINUTE} requests per minute")));
    }
    if k.expires_in_s.is_some_and(|s| s <= 0) {
        return Err(ControlError::Invalid("expires_in_s must be positive".into()));
    }
    let l = k.limits.clone().unwrap_or_default();
    let within = |v: Option<u64>, max: u64| v.is_none_or(|n| (1..=max).contains(&n));
    if !within(l.max_concurrent, 64)
        || !within(l.max_result_window, 10_000)
        || !within(l.max_buckets, 65_536)
        || !within(l.max_items, 1_000)
        || !within(l.max_inference, 64)
        || !within(l.max_rerank_window, 1_000)
    {
        return Err(ControlError::Invalid(
            "limits: max_concurrent 1-64, max_result_window 1-10000, max_buckets 1-65536, max_items 1-1000, max_inference 1-64, max_rerank_window 1-1000".into(),
        ));
    }
    let endpoints = l.inference_endpoints.as_deref().unwrap_or_default();
    if endpoints.len() > 16 || endpoints.iter().any(|e| e.is_empty() || e.len() > 255 || e.contains([',', ' ', '"', '\\', '/', '?', '#', '*'])) {
        return Err(ControlError::Invalid("limits: inference_endpoints, at most 16 inference endpoint ids".into()));
    }
    let mut out = json!({ "rate_limit": { "per_minute": per_minute, "per_ip_per_minute": per_ip } });
    if k.limits.is_some() {
        out["limits"] = json!(l);
    }
    match (indices, application) {
        (Some(i), _) => out["indices"] = json!(i),
        (_, Some(a)) => out["application"] = json!(a),
        _ => {}
    }
    if let Some(f) = filter {
        out["filter"] = f;
    }
    Ok(out)
}

/// The role descriptor of a key that reaches `reach`.
fn role(reach: &Value) -> Value {
    let names = match reach.get("application") {
        Some(a) => json!([a]),
        None => reach["indices"].clone(),
    };
    let mut indices = json!({ "names": names, "privileges": ["read"] });
    if let Some(f) = reach.get("filter") {
        indices["query"] = f.clone();
    }
    let mut role = json!({ "cluster": [], "indices": [indices] });
    if reach.get("application").is_some() {
        role["restriction"] = json!({ "workflows": ["search_application_query"] });
    }
    role
}

/// A key of the cluster's `_security/api_key` answer as the route lists it (`None`: not a
/// browser key).
fn listed(k: &Value, cluster_id: &str) -> Option<Value> {
    let meta = k.get("metadata")?.get(METADATA)?;
    // A branch holds its parent's keys, which work only on the parent.
    if meta.get("cluster")?.as_str()? != cluster_id {
        return None;
    }
    let mut out = json!({
        "id": k.get("id")?,
        "name": k.get("name").cloned().unwrap_or(Value::Null),
        "created_at": k.get("creation").and_then(Value::as_i64).map(|ms| ms / 1000),
        "expires_at": k.get("expiration").and_then(Value::as_i64).map(|ms| ms / 1000),
        "rate_limit": meta.get("rate_limit").cloned().unwrap_or(Value::Null),
    });
    for f in ["indices", "application", "filter", "limits"] {
        if let Some(v) = meta.get(f) {
            out[f] = v.clone();
        }
    }
    Some(out)
}

impl Control {
    /// A new browser key of cluster `id`: the encoded key (`base64(<key id>@<cluster>:<key>)`,
    /// sent as `Authorization: ApiKey …`) is in this answer only, with the cluster's URL.
    pub async fn create_browser_key(&self, id: &str, k: NewBrowserKey) -> Result<Value> {
        let c = self.get(id).await?;
        if c.flavor == "opensearch" {
            return Err(ControlError::Invalid("browser keys are API keys: make them on an Elasticsearch-flavor cluster (OpenSearch clients don't send API keys)".into()));
        }
        let reach = reach(&k)?;
        // Counted before the create, so two racing creates may pass one over: a bound on keys
        // left lying around, not a quota.
        if self.browser_keys(id).await?.len() >= MAX_KEYS {
            return Err(ControlError::Conflict(format!("cluster {} has {MAX_KEYS} browser keys already: revoke one first", c.name)));
        }
        // Bound to this cluster's id: a branch (or a restore of an earlier state) copies the
        // cluster's keys, and the node refuses a browser key anywhere but where it was made.
        let mut meta = reach.clone();
        meta["cluster"] = json!(c.id);
        let mut body = json!({
            "name": k.name.trim(),
            "role_descriptors": { "roll_browser": role(&reach) },
            "metadata": { METADATA: meta },
        });
        if let Some(s) = k.expires_in_s {
            body["expiration"] = json!(format!("{s}s"));
        }
        let resp = self.call(&c, EsRequest::json("POST", "/_security/api_key", serde_json::to_vec(&body).unwrap_or_default())).await;
        let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
        match resp.status {
            200 => {}
            // The cluster read the filter or the names and refused them: the caller's to fix.
            400 => return Err(ControlError::Invalid(format!("the cluster refused the key: {}", v["error"]["reason"].as_str().unwrap_or("bad request")))),
            s => return Err(ControlError::Internal(anyhow::anyhow!("creating a browser key of cluster {} failed ({s}): {v}", c.name))),
        }
        let (Some(key_id), Some(key)) = (v["id"].as_str(), v["api_key"].as_str()) else {
            return Err(ControlError::Internal(anyhow::anyhow!("the cluster answered without a key")));
        };
        let mut out = json!({
            "id": key_id,
            "name": k.name.trim(),
            "api_key": base64::engine::general_purpose::STANDARD.encode(format!("{key_id}@{}:{key}", c.name)),
            "url": connection_of(&self.cfg, &c.name)["url"],
            "created_at": super::now(),
            "expires_at": v["expiration"].as_i64().map(|ms| ms / 1000),
        });
        if let (Some(o), Some(r)) = (out.as_object_mut(), reach.as_object()) {
            o.extend(r.clone());
        }
        Ok(out)
    }

    /// Cluster `id`'s browser keys that still work (not revoked, not expired), newest first.
    pub async fn browser_keys(&self, id: &str) -> Result<Vec<Value>> {
        let c = self.get(id).await?;
        if c.flavor == "opensearch" {
            return Ok(Vec::new());
        }
        let resp = self.call(&c, EsRequest::new("GET", "/_security/api_key?active_only=true")).await;
        let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
        if resp.status != 200 {
            return Err(ControlError::Internal(anyhow::anyhow!("listing the API keys of cluster {} failed ({})", c.name, resp.status)));
        }
        let mut keys: Vec<Value> = v["api_keys"].as_array().into_iter().flatten().filter_map(|k| listed(k, &c.id)).collect();
        keys.sort_by_key(|k| std::cmp::Reverse(k["created_at"].as_i64().unwrap_or(0)));
        Ok(keys)
    }

    /// Revokes browser key `key_id` of cluster `id` (`NotFound`: no such browser key, any other
    /// key of the cluster, or a deleted cluster, whose keys can't be told apart until it is
    /// restored).
    pub async fn revoke_browser_key(&self, id: &str, key_id: &str) -> Result<Revoked> {
        let c = match self.get(id).await {
            Ok(c) => c,
            // A deleted cluster: a browser key of it (its stored security says so) is revoked by
            // the restore, as `keys/{key}` keeps a revoke.
            Err(ControlError::NotFound(m)) => {
                let Some(d) = self.store.get_deleted(id).await? else { return Err(ControlError::NotFound(m)) };
                let dialect = super::dialect(&d.cluster.flavor).unwrap_or(search_es::Dialect::Elasticsearch);
                let state = self.data.read_cluster(&d.stored_as).await.map_err(ControlError::Internal)?;
                let made_for = state.and_then(|s| search_es::xpack::security::stored_browser_key_cluster(&s.json, dialect, key_id));
                if made_for.as_deref() != Some(d.cluster.id.as_str()) || !self.keep_revoke(id, &format!("key:{key_id}")).await? {
                    return Err(ControlError::NotFound(format!("no browser key {key_id} on cluster {id}")));
                }
                return Ok(Revoked::AtRestore);
            }
            Err(e) => return Err(e),
        };
        let none = || ControlError::NotFound(format!("no browser key {key_id} on cluster {}", c.name));
        if c.flavor == "opensearch" {
            return Err(none());
        }
        let resp = self.call(&c, EsRequest::new("GET", &format!("/_security/api_key?id={}", urlencoding::encode(key_id)))).await;
        let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
        let is_browser = v["api_keys"].as_array().into_iter().flatten().any(|k| k["id"].as_str() == Some(key_id) && listed(k, &c.id).is_some());
        if !is_browser {
            return Err(none());
        }
        self.delete_key_on(&c, key_id).await.map(|()| Revoked::Now)
    }
}

