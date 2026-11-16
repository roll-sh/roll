//! Kubernetes service-account tokens (docs/secrets.md, "Kubernetes service-account tokens"), as
//! Vault's Kubernetes secrets engine: a project registers a cluster with a token of a service
//! account roll manages it as, and links in `dynamic` mode issue a fresh token per lease for an
//! existing service account, through the TokenRequest API.
//!
//! The cluster's API server is a URL the tenant chose: every call is rolld's
//! ([`Deliver::request_trusting`]): HTTPS to a public address, no redirects, a timeout, a bounded
//! answer, and the registration's own CA trusted for that cluster's calls only. The admin token
//! and the CA are sealed with the project's data key.
//!
//! A bound token can't be revoked on its own: ending its lease records the revocation, and the
//! token keeps working until its expiry, which is the lease's end (roll never hands out one that
//! outlives its lease). Errors name the cluster's host and the status, never a token.

use crate::access::{Action, Caller, Scope};
use crate::crypto::Keyring;
use crate::links::{Field, Issued, Link, LinkProvider, Mode};
use crate::sinks::Deliver;
use crate::{Error, Result, Secrets, Value, audit, names};
use code_sql::{Db, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS kubernetes_clusters (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    api_url TEXT NOT NULL,
    token BLOB NOT NULL,
    ca BLOB,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    UNIQUE (project_id, name)
) WITHOUT ROWID;
";

pub(crate) const KIND: &str = "kubernetes";
/// The most of an API server's answer read: a TokenRequest's is a token (a few KiB) and its spec.
const ANSWER_MAX: usize = 64 * 1024;
/// The longest a token is asked for: a bound token can't be ended early, so leases stay short.
const TTL_MAX: u64 = 24 * 60;
/// How far past its lease a token's expiry may be (the cluster's clock against roll's).
const SKEW_MS: i64 = 5 * 60_000;

#[derive(Debug, Serialize, PartialEq)]
pub struct KubernetesCluster {
    pub id: String,
    pub name: String,
    /// The API server (no credential in it).
    pub api_url: String,
    /// Whether calls trust the cluster's own CA (sealed, never shown) rather than public roots.
    pub has_ca: bool,
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewKubernetesCluster {
    pub name: String,
    /// `https://<host>[:port][/path]`: the API server, at a public address.
    pub api_url: String,
    /// The cluster's CA certificates (PEM), when its API server's certificate isn't publicly trusted.
    #[serde(default)]
    pub ca_pem: Option<String>,
    /// A service account token roll manages the cluster with: it may `create` `serviceaccounts/token`.
    pub token: Value,
}

fn aad(project: &str, id: &str, what: &str) -> String {
    format!("roll-secrets:kubernetes:{project}:{id}:{what}")
}

/// A DNS-1123 label: namespaces and service accounts (63 at most).
fn label(s: &str) -> bool {
    (1..=63).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !s.starts_with('-') && !s.ends_with('-')
}

/// A service account name: a DNS-1123 subdomain (253 at most).
fn subdomain(s: &str) -> bool {
    s.len() <= 253 && s.split('.').all(label)
}

const FIELDS: &[Field] = &[
    Field { field: "token", label: "Service account token", modes: &[Mode::Dynamic], secret: true, suggest: "KUBE_TOKEN", credential: Some("token") },
    Field { field: "api_url", label: "API server", modes: &[Mode::Dynamic], secret: false, suggest: "KUBE_API_URL", credential: Some("token") },
    Field { field: "ca", label: "Cluster CA (PEM)", modes: &[Mode::Dynamic], secret: false, suggest: "KUBE_CA", credential: Some("token") },
];

/// What a link asks for: a token of `service_account` in `namespace`, for `audiences` (the API
/// server's own when empty).
struct Want {
    namespace: String,
    service_account: String,
    audiences: Vec<String>,
}

fn want(link: &Link) -> Result<Want> {
    let o = &link.options;
    let text = |k: &str| o.get(k).and_then(Json::as_str).map(str::to_string);
    let (Some(namespace), Some(service_account)) = (text("namespace"), text("service_account")) else {
        return Err(Error::BadRequest("a kubernetes link names the service account: options {namespace, service_account, audiences?}".into()));
    };
    if !label(&namespace) || !subdomain(&service_account) {
        return Err(Error::BadRequest("namespace and service_account are Kubernetes names (lowercase letters, digits, - and .)".into()));
    }
    let audiences: Vec<String> = match o.get("audiences") {
        None | Some(Json::Null) => vec![],
        Some(Json::Array(a)) if a.len() <= 10 => a.iter().map(|v| v.as_str().filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)).map(str::to_string)).collect::<Option<_>>().ok_or_else(|| {
            Error::BadRequest("audiences are strings (256 characters at most)".into())
        })?,
        Some(_) => return Err(Error::BadRequest("audiences is a list of at most 10 strings".into())),
    };
    Ok(Want { namespace, service_account, audiences })
}

/// An API server's `metav1.Time` (`2026-11-16T12:00:00Z`), as unix ms.
fn rfc3339_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' || b[19] != b'Z' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r).filter(|d| d.bytes().all(|c| c.is_ascii_digit())).and_then(|d| d.parse::<u32>().ok());
    let month = time::Month::try_from(n(5..7)? as u8).ok()?;
    let date = time::Date::from_calendar_date(n(0..4)? as i32, month, n(8..10)? as u8).ok()?;
    let t = time::Time::from_hms(n(11..13)? as u8, n(14..16)? as u8, n(17..19)? as u8).ok()?;
    Some(time::PrimitiveDateTime::new(date, t).assume_utc().unix_timestamp() * 1000)
}

/// A cluster as its calls need it.
struct Opened {
    name: String,
    api_url: String,
    token: Value,
    ca: Option<Value>,
}

impl Opened {
    fn host(&self) -> String {
        names::url_host(&self.api_url)
    }

    /// One call with the admin token. Errors name the host, never a token or the answer.
    fn call(&self, deliver: &dyn Deliver, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, zeroize::Zeroizing<Vec<u8>>)> {
        let auth = Value::new(format!("Bearer {}", self.token.expose()));
        let accept = Value::new("application/json");
        let headers = [("authorization", &auth), ("accept", &accept)];
        deliver.request_trusting(self.ca.as_ref().map(Value::expose), method, &format!("{}{path}", self.api_url), &headers, body.map(|b| ("application/json", b)), ANSWER_MAX)
    }
}

/// Why a status failed, in roll's words (never the server's message).
fn hint(status: u16) -> &'static str {
    match status {
        401 => ": the admin token isn't valid there (expired, or for another cluster)",
        403 => ": the admin token may not do this (it needs create on serviceaccounts/token)",
        404 => ": no such service account in that namespace",
        _ => "",
    }
}

/// What TokenRequest answers (the rest ignored).
#[derive(Deserialize)]
struct Answer {
    status: AnswerStatus,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnswerStatus {
    token: Value,
    expiration_timestamp: String,
}

/// The link provider for `kubernetes`: registered by [`Secrets::new`] when rolld can call out.
pub(crate) struct Provider {
    pub db: Db,
    pub keys: Keyring,
    pub unwrap: crate::kms::Unwrapper,
    pub deliver: Arc<dyn Deliver>,
}

impl Provider {
    fn open(&self, id: &str) -> Result<Opened> {
        type Row = (String, String, String, Vec<u8>, Option<Vec<u8>>, String);
        let row: Option<Row> = self.db.read(|c| {
            Ok::<_, Error>(
                c.query_row("SELECT k.project_id, k.name, k.api_url, k.token, k.ca, p.dek FROM kubernetes_clusters k JOIN projects p ON p.project_id = k.project_id WHERE k.id = ?", [id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
                })
                .optional()?,
            )
        })?;
        let (project, name, api_url, token, ca, dek) = row.ok_or_else(|| Error::Link(format!("Kubernetes cluster {id} doesn't exist any more")))?;
        let inner = self.unwrap.inner(&project, &dek)?;
        let (dek, _) = self.keys.open_dek(&project, &inner)?;
        let ca = ca.map(|ca| dek.open(&aad(&project, id, "ca"), &ca)).transpose()?;
        Ok(Opened { name, api_url, token: dek.open(&aad(&project, id, "token"), &token)?, ca })
    }
}

impl LinkProvider for Provider {
    fn kind(&self) -> &str {
        KIND
    }

    fn fields(&self) -> &[Field] {
        FIELDS
    }

    fn suggested(&self) -> &[(&'static str, Mode)] {
        &[("token", Mode::Dynamic), ("api_url", Mode::Dynamic), ("ca", Mode::Dynamic)]
    }

    fn name(&self, r#ref: &str) -> Option<String> {
        self.db.read(|c| c.query_row("SELECT name FROM kubernetes_clusters WHERE id = ?", [r#ref], |r| r.get::<_, String>(0)).optional()).ok().flatten()
    }

    fn read(&self, _: &str, _: &str, _: &Link) -> Result<Value> {
        Err(Error::BadRequest("Kubernetes tokens are dynamic: each lease gets one of its own".into()))
    }

    /// A token of the link's service account that expires when the lease ends.
    fn issue(&self, r#ref: &str, _credential: &str, link: &Link, _label: &str, ttl_minutes: Option<u64>) -> Result<Issued> {
        let w = want(link)?;
        let ttl = ttl_minutes.unwrap_or_else(|| link.ttl_minutes()).min(TTL_MAX);
        let o = self.open(r#ref)?;
        let host = o.host();
        let mut spec = json!({ "expirationSeconds": ttl * 60 });
        if !w.audiences.is_empty() {
            spec["audiences"] = json!(w.audiences);
        }
        let body = Value::new(json!({ "apiVersion": "authentication.k8s.io/v1", "kind": "TokenRequest", "spec": spec }).to_string());
        let path = format!("/api/v1/namespaces/{}/serviceaccounts/{}/token", w.namespace, w.service_account);
        let now = self.db.now_ms()?;
        let (status, answer) = o.call(self.deliver.as_ref(), "POST", &path, Some(&body))?;
        let what = format!("{}/{}", w.namespace, w.service_account);
        if !(200..300).contains(&status) {
            return Err(Error::Link(format!("{host} answered {status} to a token for {what}{}", hint(status))));
        }
        // Never the answer in an error: it holds the token.
        let a: Answer = serde_json::from_slice(&answer).map_err(|_| Error::Link(format!("{host} didn't answer a TokenRequest for {what}")))?;
        let token = a.status.token;
        if token.expose().is_empty() || token.expose().len() > 16 * 1024 || token.expose().chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(Error::Link(format!("{host} answered a token roll can't hand out for {what}")));
        }
        let expires_at = rfc3339_ms(&a.status.expiration_timestamp).ok_or_else(|| Error::Link(format!("{host} answered no expiry for {what}'s token")))?;
        // A bound token can't be revoked: one that outlives its lease is never handed out.
        if expires_at > now + ttl as i64 * 60_000 + SKEW_MS {
            return Err(Error::Link(format!("{host} issued {what} a token that outlives its lease ({} minutes asked): roll doesn't hand it out", ttl)));
        }
        let ca = o.ca.map(|c| c.expose().to_string()).unwrap_or_default();
        let parts = BTreeMap::from([("token".to_string(), token), ("api_url".to_string(), Value::new(o.api_url)), ("ca".to_string(), Value::new(ca))]);
        Ok(Issued { parts, handle: json!({ "namespace": w.namespace, "service_account": w.service_account, "expires_at": expires_at }) })
    }

    /// A bound service-account token can't be ended early: it works until its own expiry, the
    /// lease's end. Revoking a lease only records that.
    fn revoke(&self, _: &str, _: &Json) -> Result<()> {
        Ok(())
    }

    fn revocable(&self) -> bool {
        false
    }

    fn renew(&self, _: &str, _: &Json, _: i64) -> Result<()> {
        Err(Error::BadRequest("a Kubernetes token's expiry is fixed when it's issued: resolve again for a new one".into()))
    }

    fn check(&self, link: &Link) -> Result<()> {
        want(link)?;
        if link.option_u64("ttl_minutes").is_some_and(|t| t > TTL_MAX) {
            return Err(Error::BadRequest(format!("a Kubernetes token can't be revoked before it expires: ttl_minutes is {TTL_MAX} at most (keep it short)")));
        }
        Ok(())
    }
}

impl Secrets {
    pub fn kubernetes_clusters(&self, project: &str, caller: &Caller) -> Result<Vec<KubernetesCluster>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| {
            Ok(c.query("SELECT id, name, api_url, ca IS NOT NULL, created_at, created_by FROM kubernetes_clusters WHERE project_id = ? ORDER BY name", [project], |r| {
                Ok(KubernetesCluster { id: r.get(0)?, name: r.get(1)?, api_url: r.get(2)?, has_ca: r.get::<_, i64>(3)? != 0, created_at: r.get(4)?, created_by: r.get(5)? })
            })?)
        })
    }

    /// Register a cluster, after its API server took the admin token.
    pub fn add_kubernetes_cluster(&self, project: &str, n: NewKubernetesCluster, caller: &Caller) -> Result<KubernetesCluster> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let deliver = self.deliver.as_ref().ok_or_else(|| Error::BadRequest("Kubernetes clusters aren't on this server".into()))?;
        if n.name.is_empty() || n.name.len() > 64 || !n.name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
            return Err(Error::BadRequest("a cluster's name is letters, digits, - and _ (64 at most)".into()));
        }
        let api_url = n.api_url.trim().trim_end_matches('/').to_string();
        let host = names::url_host(&api_url);
        let authority = api_url.strip_prefix("https://").map(|r| r.split('/').next().unwrap_or_default()).unwrap_or_default();
        if host.is_empty() || api_url.len() > 2048 || authority.contains('@') || api_url.contains(['?', '#']) || api_url.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(Error::BadRequest("api_url is the API server's https:// URL (no user name, query or fragment)".into()));
        }
        deliver.check(&api_url).map_err(|_| Error::BadRequest("api_url is the API server's https:// URL, at a public address".into()))?;
        let ca = n.ca_pem.map(|c| c.trim().to_string()).filter(|c| !c.is_empty());
        if ca.as_ref().is_some_and(|c| c.len() > 64 * 1024 || !c.contains("-----BEGIN CERTIFICATE-----")) {
            return Err(Error::BadRequest("ca_pem is the cluster's CA certificates, PEM (64 KiB at most)".into()));
        }
        let token = n.token.expose().trim();
        if token.is_empty() || token.len() > 16 * 1024 || token.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(Error::BadRequest("token is a service account token (16 KiB at most)".into()));
        }
        let token = Value::new(token);
        let probe = Opened { name: n.name.clone(), api_url: api_url.clone(), token: token.clone(), ca: ca.clone().map(Value::new) };
        // Discovery answers any valid token: the address, the TLS trust and the token, proven.
        let (status, _) = probe.call(deliver.as_ref(), "GET", "/api", None).map_err(|e| Error::BadRequest(format!("roll couldn't reach {}: {e}", probe.name)))?;
        if status != 200 {
            return Err(Error::BadRequest(format!("{host} answered {status} to the admin token{}", hint(status))));
        }
        let dek = self.project(project)?;
        let id = names::new_id("kube", 16);
        let sealed_token = dek.seal(&aad(project, &id, "token"), &token);
        let sealed_ca = ca.map(|c| dek.seal(&aad(project, &id, "ca"), &Value::new(c)));
        let now = self.db.now_ms()?;
        let k = KubernetesCluster { id: id.clone(), name: n.name, api_url, has_ca: sealed_ca.is_some(), created_at: now, created_by: caller.principal.clone() };
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM kubernetes_clusters WHERE project_id = ? AND name = ?", [project, &k.name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("a Kubernetes cluster named {} exists", k.name)));
            }
            c.execute(
                "INSERT INTO kubernetes_clusters (id, project_id, name, api_url, token, ca, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, project, k.name, k.api_url, sealed_token, sealed_ca, now, k.created_by],
            )?;
            audit::record(c, project, caller, "kubernetes.add", None, None, &[], &format!("added Kubernetes cluster {} ({host})", k.name), now)
        })?;
        Ok(k)
    }

    /// Delete a cluster no key links to and no live lease came from.
    pub fn delete_kubernetes_cluster(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        let like = (format!("%\"kind\":\"{KIND}\"%"), format!("%\"ref\":\"{id}\"%"));
        self.db.write(|c| {
            let name: String =
                c.query_row("SELECT name FROM kubernetes_clusters WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?.ok_or_else(|| Error::NotFound(format!("no Kubernetes cluster {id}")))?;
            let linked: Vec<String> = c.query("SELECT key FROM secrets WHERE project_id = ? AND link LIKE ? AND link LIKE ? ORDER BY key LIMIT 5", params![project, like.0, like.1], |r| r.get(0))?;
            if !linked.is_empty() {
                return Err(Error::Conflict("in_use", format!("{name} still issues {}: unlink them first", linked.join(", "))));
            }
            let live: i64 = c.query_row("SELECT COUNT(*) FROM leases WHERE project_id = ? AND revoked_at IS NULL AND link LIKE ? AND link LIKE ?", params![project, like.0, like.1], |r| r.get(0))?;
            if live > 0 {
                return Err(Error::Conflict("in_use", format!("{name} has {live} lease(s) still live: delete it once they've ended")));
            }
            c.execute("DELETE FROM kubernetes_clusters WHERE id = ?", [id])?;
            audit::record(c, project, caller, "kubernetes.delete", None, None, &[], &format!("deleted Kubernetes cluster {name}"), now)
        })
    }

    /// The project's clusters, as link targets' (kind, ref).
    pub(crate) fn kubernetes_refs(&self, project: &str) -> Result<Vec<(String, String)>> {
        self.db.read(|c| Ok(c.query("SELECT id, name FROM kubernetes_clusters WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get(0)?, r.get(1)?)))?))
    }
}

