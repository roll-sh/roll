//! Remote clusters between a person's own search clusters (cross-cluster search; Elastic
//! Cloud's remote clusters between deployments, Elasticsearch 9's API key based remote cluster
//! security). docs/search/README.md, "Cross-cluster search".
//!
//! - **Who may link**: a cluster may name another as a remote only when both are one person's,
//!   or both the operator's ([`may_link`]). Names are global, so naming another person's cluster
//!   is refused exactly as naming one that doesn't exist. Checked when the remote is set up
//!   (`PUT _cluster/settings`, [`Control::link_remote`]) and again on every request, since a
//!   deleted cluster's name can be taken by someone else.
//! - **Credentials**: the remote issues a `cross_cluster` API key (`search` access patterns);
//!   the local cluster keeps it as `cluster.remote.<alias>.credentials`, a secure setting, here
//!   in the catalog (`search_remotes`), sealed under the deployment's key and bound to its row
//!   (cluster, alias, target), never returned. Revoking the key on the remote cuts the link.
//! - **Execution**: a remote's part runs on the remote cluster's owner through [`Control`]'s
//!   own in-process call or signed fleet forward, never over the public endpoint, so IP
//!   allowlists don't apply to it. A sleeping remote wakes for it.

use crate::control::{Control, ControlError, Result};
use crate::store::{RemoteLink, SearchCluster};
use async_trait::async_trait;
use base64::Engine;
use search_es::remote::links::{Links, REMOTE_ACCESS_HEADER};
use search_es::{EsError, EsRequest, EsResponse};
use serde_json::{Value, json};
use std::sync::{OnceLock, Weak};

/// The control plane as search-es's [`Links`].
pub struct ControlLinks {
    control: OnceLock<Weak<Control>>,
    domain: Option<String>,
}

impl ControlLinks {
    pub fn new(domain: Option<String>) -> Self {
        Self { control: OnceLock::new(), domain }
    }

    pub(crate) fn set(&self, c: Weak<Control>) {
        let _ = self.control.set(c);
    }

    fn control(&self) -> Option<std::sync::Arc<Control>> {
        self.control.get().and_then(Weak::upgrade)
    }
}

/// The roll search cluster an address names: `name`, `name:port`, or the cluster's endpoint
/// under the node's domain (`name.<domain>[:port]`, with or without `https://`).
pub fn target(address: &str, domain: Option<&str>) -> Option<String> {
    let a = address.trim();
    let a = a.strip_prefix("https://").or_else(|| a.strip_prefix("http://")).unwrap_or(a);
    let a = a.split('/').next().unwrap_or_default().trim_end_matches('.');
    let host = a.rsplit_once(':').filter(|(_, p)| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())).map_or(a, |(h, _)| h);
    if host.parse::<std::net::IpAddr>().is_ok() || host.starts_with('[') {
        return None;
    }
    let host = host.to_ascii_lowercase();
    let label = if host.contains('.') { search_server::auth::host_label(&host, domain)?.to_string() } else { host };
    crate::control::valid_name(&label).then_some(label)
}

/// Whether `local` may name `target` as a remote: two Elasticsearch-flavor clusters with the
/// same owner (one person's, or both the operator's), not the same cluster.
pub fn may_link(local: &SearchCluster, target: &SearchCluster) -> bool {
    local.id != target.id && local.owner == target.owner && local.flavor == "elasticsearch" && target.flavor == "elasticsearch"
}

/// The answer for a target the caller can't link to: the same as for one that doesn't exist.
fn no_target(name: &str) -> ControlError {
    ControlError::NotFound(format!("no search cluster {name}"))
}

/// The most remote clusters one cluster holds credentials for: each search may fan out to all.
pub const MAX_REMOTES: usize = 20;

/// What a remote's credentials are sealed for: their row.
fn kind(cluster_id: &str, alias: &str, target_id: &str) -> String {
    format!("search remote credentials {cluster_id} {alias} {target_id}")
}

/// A remote alias as a setting name takes it: letters, digits, `-` and `_`.
pub fn valid_alias(a: &str) -> bool {
    (1..=64).contains(&a.len()) && a.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The `(id, secret)` of an encoded API key (`base64(id:secret)`).
fn decode_key(encoded: &str) -> Option<(String, String)> {
    let raw = base64::engine::general_purpose::STANDARD.decode(encoded.trim()).ok()?;
    let text = String::from_utf8(raw).ok()?;
    let (id, secret) = text.split_once(':')?;
    let id = id.split_once('@').map_or(id, |(i, _)| i);
    (!id.is_empty() && !secret.is_empty()).then(|| (id.to_string(), secret.to_string()))
}

#[async_trait]
impl Links for ControlLinks {
    fn target(&self, address: &str) -> Option<String> {
        target(address, self.domain.as_deref())
    }

    async fn may_link(&self, local: &str, target: &str) -> bool {
        let Some(c) = self.control() else { return false };
        match (c.get(local).await, c.get(target).await) {
            (Ok(l), Ok(t)) => may_link(&l, &t),
            _ => false,
        }
    }

    async fn has_credentials(&self, local: &str, alias: &str) -> bool {
        let Some(c) = self.control() else { return false };
        match c.get(local).await {
            Ok(l) => c.store.remote(&l.id, alias).await.ok().flatten().is_some(),
            Err(_) => false,
        }
    }

    async fn send(&self, local: &str, alias: &str, target: &str, req: EsRequest) -> std::result::Result<EsResponse, EsError> {
        // Elasticsearch's answer for a remote it can't reach; why, when it's the setup's.
        let unreachable = |why: Option<&str>| EsError::new(500, "connect_transport_exception", format!("Unable to connect to [{alias}]{}", why.map(|w| format!(": {w}")).unwrap_or_default()));
        let Some(c) = self.control() else { return Err(unreachable(None)) };
        let (Ok(l), Ok(t)) = (c.get(local).await, c.get(target).await) else { return Err(unreachable(None)) };
        if !may_link(&l, &t) {
            return Err(unreachable(None));
        }
        let Ok(Some(row)) = c.store.remote(&l.id, alias).await else {
            return Err(unreachable(Some("the remote has no credentials (cluster.remote.<alias>.credentials): link it through roll's API")));
        };
        // The name may have been deleted and taken since: the key is the cluster it was made for.
        if row.target_id != t.id {
            return Err(unreachable(None));
        }
        let Some(encoded) = c.open_remote_credentials(&row) else { return Err(unreachable(Some("its credentials can't be opened on this node"))) };
        let Some((key_id, secret)) = decode_key(&encoded) else { return Err(unreachable(Some("its credentials aren't an API key"))) };
        Ok(c.send_link(&t, &key_id, &secret, req).await)
    }
}

impl Control {
    /// Opens a remote's sealed credentials (`None`: sealed under another key, or tampered with).
    fn open_remote_credentials(&self, row: &RemoteLink) -> Option<String> {
        match &self.cfg.sealer {
            Some(s) if search_es::seal::is_sealed(&row.credentials) => s.open_text(&kind(&row.cluster_id, &row.alias, &row.target_id), &row.credentials),
            Some(_) => None,
            None if search_es::seal::is_sealed(&row.credentials) => None,
            None => Some(row.credentials.clone()),
        }
    }

    /// Sends a linked remote's request to cluster `t` as cross-cluster key `key_id`, on `t`'s
    /// owner (here, or forwarded signed): roll's own call, which the target's IP allowlist
    /// doesn't filter and which wakes it.
    async fn send_link(&self, t: &SearchCluster, key_id: &str, secret: &str, mut req: EsRequest) -> EsResponse {
        // A repository's maker is said only by `Control::put_repository`.
        req.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("authorization") && !k.eq_ignore_ascii_case("host") && !k.eq_ignore_ascii_case(search_es::secret_refs::MAKER_HEADER));
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{key_id}@{}:{secret}", t.name));
        req.headers.push(("Authorization".into(), format!("ApiKey {token}")));
        self.on_owner(&t.name, req).await
    }

    /// The remotes cluster `id` holds credentials for, with their targets' names.
    pub async fn remotes(&self, id: &str) -> Result<Vec<Value>> {
        let c = self.get(id).await?;
        let mut out = Vec::new();
        for r in self.store.remotes(&c.id).await? {
            let target = self.get(&r.target_id).await.ok();
            out.push(json!({
                "alias": r.alias,
                "cluster": target.as_ref().map(|t| t.name.clone()),
                "cluster_id": r.target_id,
                "key_id": r.key_id,
                "minted": r.minted,
                "created_at": r.created_at,
                "connected": target.as_ref().is_some_and(|t| may_link(&c, t)),
            }));
        }
        Ok(out)
    }

    /// Links cluster `id` to cluster `target` as its remote `alias` (`PUT …/remotes/{alias}`):
    /// stores the credentials sealed and sets `cluster.remote.<alias>.*` (proxy mode, the
    /// target's name). `api_key`: a `cross_cluster` key the person made on the target
    /// (`POST _security/cross_cluster/api_key`), checked there; else roll makes one with
    /// `access` (Elasticsearch's shape; default: search every index). `visible`: whether the
    /// caller may see a cluster (anyone else's answers as missing).
    #[allow(clippy::too_many_arguments)]
    pub async fn link_remote(
        &self,
        id: &str,
        alias: &str,
        target: &str,
        skip_unavailable: Option<bool>,
        access: Option<Value>,
        api_key: Option<&str>,
        visible: impl Fn(&SearchCluster) -> bool,
    ) -> Result<Value> {
        if !valid_alias(alias) {
            return Err(ControlError::Invalid("alias: 1 to 64 letters, digits, - and _".into()));
        }
        let c = self.get(id).await?;
        let held = self.store.remotes(&c.id).await?;
        if held.len() >= MAX_REMOTES && !held.iter().any(|r| r.alias == alias) {
            return Err(ControlError::Conflict(format!("cluster {} has {MAX_REMOTES} remote clusters already, the most one may have: unlink one first", c.name)));
        }
        let t = match self.get(target).await {
            Ok(t) if t.id == c.id => return Err(ControlError::Invalid("a cluster can't be its own remote".into())),
            Ok(t) if visible(&t) && may_link(&c, &t) => t,
            _ => return Err(no_target(target)),
        };
        let (key_id, encoded, minted) = match api_key {
            Some(k) => {
                let Some((key_id, secret)) = decode_key(k) else { return Err(ControlError::Invalid("api_key: the encoded cross-cluster API key (base64 of id:secret)".into())) };
                // It must authenticate on the target as one of its cross-cluster keys.
                let mut probe = EsRequest::new("GET", "/_security/_authenticate");
                probe.headers.push((REMOTE_ACCESS_HEADER.into(), json!({ "cluster": c.name, "roles": [] }).to_string()));
                let r = self.send_link(&t, &key_id, &secret, probe).await;
                let v: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
                if r.status != 200 || v["api_key"]["id"].as_str() != Some(key_id.as_str()) {
                    return Err(ControlError::Invalid(format!("api_key isn't a valid cross-cluster API key of cluster {}", t.name)));
                }
                let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{key_id}:{secret}"));
                (key_id, encoded, false)
            }
            None => {
                let access = access.unwrap_or_else(|| json!({ "search": [{ "names": ["*"] }] }));
                let body = json!({ "name": format!("roll remote {} as {alias}", c.name), "access": access, "metadata": { "roll_remote": { "cluster": c.id, "alias": alias } } });
                let r = self.call(&t, EsRequest::json("POST", "/_security/cross_cluster/api_key", serde_json::to_vec(&body).unwrap_or_default())).await;
                let v: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
                match (r.status, v["id"].as_str(), v["encoded"].as_str()) {
                    (200, Some(i), Some(e)) => (i.to_string(), e.to_string(), true),
                    (400, _, _) => return Err(ControlError::Invalid(format!("access: {}", v["error"]["reason"].as_str().unwrap_or("invalid")))),
                    (s, _, _) => return Err(ControlError::Internal(anyhow::anyhow!("making a cross-cluster API key of cluster {} failed ({s})", t.name))),
                }
            }
        };
        let k = kind(&c.id, alias, &t.id);
        // Sealed under the deployment's key; a node without one (`ROLL_SECRET_KEY` unset: local
        // development only, as for every secret search keeps) stores it as given.
        let credentials = match &self.cfg.sealer {
            Some(s) => s.seal_text(&k, &encoded),
            None => encoded,
        };
        let row = RemoteLink { cluster_id: c.id.clone(), alias: alias.to_string(), target_id: t.id.clone(), key_id: key_id.clone(), credentials, minted, created_at: crate::control::now() };
        let replaced = self.store.put_remote(&row).await?;
        let mut settings = serde_json::Map::new();
        let prefix = format!("cluster.remote.{alias}");
        settings.insert(format!("{prefix}.mode"), json!("proxy"));
        settings.insert(format!("{prefix}.proxy_address"), json!(t.name));
        settings.insert(format!("{prefix}.seeds"), Value::Null);
        if let Some(s) = skip_unavailable {
            settings.insert(format!("{prefix}.skip_unavailable"), json!(s.to_string()));
        }
        let r = self.call(&c, EsRequest::json("PUT", "/_cluster/settings", serde_json::to_vec(&json!({ "persistent": settings })).unwrap_or_default())).await;
        if r.status != 200 {
            // Leave nothing behind: the old credentials back, the new key gone.
            match &replaced {
                Some(old) => drop(self.store.put_remote(old).await),
                None => drop(self.store.delete_remote(&c.id, alias).await),
            }
            if minted {
                let _ = self.revoke_remote_key(&t.id, &key_id).await;
            }
            let v: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
            return Err(ControlError::Internal(anyhow::anyhow!("setting remote {alias} of cluster {} failed ({}): {}", c.name, r.status, v["error"]["reason"])));
        }
        if let Some(old) = replaced.filter(|o| o.minted && o.key_id != key_id) {
            let _ = self.revoke_remote_key(&old.target_id, &old.key_id).await;
        }
        let mut out = json!({ "alias": alias, "cluster": t.name, "cluster_id": t.id, "mode": "proxy", "proxy_address": t.name, "key_id": key_id, "minted": minted });
        if let Some(s) = skip_unavailable {
            out["skip_unavailable"] = json!(s);
        }
        Ok(out)
    }

    /// Unlinks remote `alias` of cluster `id`: its `cluster.remote.<alias>.*` settings and
    /// credentials go, and a key roll made is revoked on the target.
    pub async fn unlink_remote(&self, id: &str, alias: &str) -> Result<()> {
        let c = self.get(id).await?;
        let Some(row) = self.store.remote(&c.id, alias).await? else { return Err(ControlError::NotFound(format!("cluster {} has no remote {alias}", c.name))) };
        let mut settings = serde_json::Map::new();
        for k in ["mode", "proxy_address", "seeds", "skip_unavailable", "server_name", "proxy_socket_connections", "node_connections", "transport.ping_schedule", "transport.compress"] {
            settings.insert(format!("cluster.remote.{alias}.{k}"), Value::Null);
        }
        let r = self.call(&c, EsRequest::json("PUT", "/_cluster/settings", serde_json::to_vec(&json!({ "persistent": settings, "transient": settings })).unwrap_or_default())).await;
        if r.status != 200 {
            return Err(ControlError::Internal(anyhow::anyhow!("removing remote {alias} of cluster {} failed ({})", c.name, r.status)));
        }
        self.store.delete_remote(&c.id, alias).await?;
        if row.minted {
            self.revoke_remote_key(&row.target_id, &row.key_id).await?;
        }
        Ok(())
    }

    /// Revokes the cross-cluster keys roll made for cluster `id`'s remotes, on their targets
    /// (before its rows go, at a hard delete or a purge).
    pub(crate) async fn revoke_remote_keys_of(&self, id: &str) -> Result<()> {
        for r in self.store.remotes(id).await?.into_iter().filter(|r| r.minted) {
            self.revoke_remote_key(&r.target_id, &r.key_id).await?;
        }
        Ok(())
    }

    /// Revokes cross-cluster key `key_id` on cluster `target_id` (gone already counts).
    async fn revoke_remote_key(&self, target_id: &str, key_id: &str) -> Result<()> {
        let t = match self.get(target_id).await {
            Ok(t) => t,
            Err(ControlError::NotFound(_)) => return Ok(()),
            Err(e) => return Err(e),
        };
        let body = serde_json::to_vec(&json!({ "ids": [key_id] })).unwrap_or_default();
        let r = self.call(&t, EsRequest::json("DELETE", "/_security/api_key", body)).await;
        if r.status < 300 || r.status == 404 { Ok(()) } else { Err(ControlError::Internal(anyhow::anyhow!("revoking remote key {key_id} of cluster {} failed ({})", t.name, r.status))) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        let d = Some("search.example.com");
        assert_eq!(target("logs", d), Some("logs".into()));
        assert_eq!(target("logs:9300", d), Some("logs".into()));
        assert_eq!(target("https://logs.search.example.com:443/", d), Some("logs".into()));
        assert_eq!(target("logs.search.example.com", d), Some("logs".into()));
        assert_eq!(target("logs.elsewhere.com:9300", d), None);
        assert_eq!(target("127.0.0.1:9300", d), None);
        assert_eq!(target("[::1]:9300", d), None);
        assert_eq!(target("Logs", None), Some("logs".into()));
        assert_eq!(target("bad_name", None), None);
    }
}
