//! Kubernetes service-account tokens (docs/secrets.md, "Kubernetes service-account tokens"), as
//! Vault's Kubernetes secrets engine: a project registers a cluster with a token of a service
//! account roll manages it as, and links in `dynamic` mode issue a fresh token per lease, through
//! the TokenRequest API, either for an existing service account or for one made for the lease
//! (`role_rules`: a ServiceAccount, a Role or ClusterRole with those rules and its binding, all
//! named `roll-<lease>` and labelled with the lease, deleted when it ends).
//!
//! The cluster's API server is a URL the tenant chose: every call is rolld's
//! ([`Deliver::request_trusting`]): HTTPS to a public address, no redirects, a timeout, a bounded
//! answer, and the registration's own CA trusted for that cluster's calls only. The admin token
//! and the CA are sealed with the project's data key.
//!
//! A bound token can't be revoked on its own: for an existing service account, ending its lease
//! records the revocation, and the token keeps working until its expiry, which is the lease's end
//! (roll never hands out one that outlives its lease). A generated account's tokens stop working
//! when its lease ends: deleting the account ends them. Errors name the cluster's host, the step
//! and the status, never a token.

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
    /// A service account token roll manages the cluster with: it may `create` `serviceaccounts/token`
    /// (and, for `role_rules` links, create and delete ServiceAccounts, Roles and RoleBindings).
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

/// What a link asks for: a token of a service account in `namespace`, for `audiences` (the API
/// server's own when empty).
struct Want {
    namespace: String,
    account: Account,
    audiences: Vec<String>,
}

enum Account {
    /// An existing service account (`service_account`).
    Existing(String),
    /// One made per lease, bound to a Role (a ClusterRole when `cluster_wide`) of `rules`
    /// (`role_rules`).
    Generated { rules: Vec<Json>, cluster_wide: bool },
}

/// The most rules a generated role has, and the most entries in each of a rule's lists.
const RULES_MAX: usize = 32;
const LIST_MAX: usize = 32;

/// Whether one entry of a rule's list is valid.
type Valid = fn(&str) -> bool;

/// `role_rules` as Kubernetes PolicyRules (`apiGroups`, `resources`, `verbs`, `resourceNames?`),
/// checked by Kubernetes' name rules and bounded, in the API's own shape.
fn rules(v: &Json) -> Result<Vec<Json>> {
    let bad = |m: String| Error::BadRequest(m);
    let Some(list) = v.as_array().filter(|l| (1..=RULES_MAX).contains(&l.len())) else {
        return Err(bad(format!("role_rules is a list of 1 to {RULES_MAX} rules: {{apiGroups, resources, verbs, resourceNames?}}")));
    };
    let group: Valid = |s| s.is_empty() || s == "*" || subdomain(s);
    let resource: Valid = |s| s.len() <= 253 && s.split('/').count() <= 2 && s.split('/').all(|p| p == "*" || subdomain(p));
    let verb: Valid = |s| s == "*" || ((1..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_lowercase()));
    let object: Valid = |s| (1..=253).contains(&s.len()) && s != "." && s != ".." && !s.chars().any(|c| c == '/' || c == '%' || c.is_whitespace() || c.is_control());
    // (key, valid entry, required, what its entries are)
    let checks: [(&str, Valid, bool, &str); 4] = [
        ("apiGroups", group, true, "API groups (\"\" is the core group)"),
        ("resources", resource, true, "resources (pods, pods/log, *)"),
        ("verbs", verb, true, "verbs (get, list, watch, create, *)"),
        ("resourceNames", object, false, "object names"),
    ];
    list.iter()
        .enumerate()
        .map(|(i, r)| {
            let r = r.as_object().ok_or_else(|| bad(format!("role_rules[{i}] is a rule: {{apiGroups, resources, verbs, resourceNames?}}")))?;
            if let Some(k) = r.keys().find(|k| !checks.iter().any(|c| c.0 == k.as_str())) {
                return Err(bad(format!("role_rules[{i}] has {k:?}: a rule is apiGroups, resources, verbs and resourceNames")));
            }
            let mut out = serde_json::Map::new();
            for (key, ok, required, what) in &checks {
                let items = match r.get(*key) {
                    None if !required => continue,
                    Some(Json::Array(a)) if a.len() <= LIST_MAX && (!a.is_empty() || !required) => a,
                    _ => return Err(bad(format!("role_rules[{i}].{key} is a list of 1 to {LIST_MAX} {what}"))),
                };
                if !items.iter().all(|v| v.as_str().is_some_and(ok)) {
                    return Err(bad(format!("role_rules[{i}].{key} holds {what} only")));
                }
                if !items.is_empty() {
                    out.insert(key.to_string(), Json::Array(items.clone()));
                }
            }
            Ok(Json::Object(out))
        })
        .collect()
}

fn want(link: &Link) -> Result<Want> {
    let o = &link.options;
    let shape = "a kubernetes link names a namespace and either an existing service account or the rules of a role made per lease: options {namespace, service_account | role_rules, cluster_wide?, audiences?}";
    let Some(namespace) = o.get("namespace").and_then(Json::as_str).map(str::to_string) else { return Err(Error::BadRequest(shape.into())) };
    if !label(&namespace) {
        return Err(Error::BadRequest("namespace is a Kubernetes name (lowercase letters, digits and -)".into()));
    }
    let cluster_wide = match o.get("cluster_wide") {
        None => None,
        Some(Json::Bool(b)) => Some(*b),
        Some(_) => return Err(Error::BadRequest("cluster_wide is true or false".into())),
    };
    let account = match (o.get("service_account"), o.get("role_rules")) {
        (Some(_), Some(_)) => return Err(Error::BadRequest("service_account and role_rules are exclusive: an existing service account, or one made per lease with those rules".into())),
        (Some(sa), None) => {
            let sa = sa.as_str().filter(|s| subdomain(s)).ok_or_else(|| Error::BadRequest("service_account is a Kubernetes name (lowercase letters, digits, - and .)".into()))?;
            if cluster_wide.is_some() {
                return Err(Error::BadRequest("cluster_wide goes with role_rules: an existing service account keeps the roles it has".into()));
            }
            Account::Existing(sa.to_string())
        }
        (None, Some(r)) => Account::Generated { rules: rules(r)?, cluster_wide: cluster_wide.unwrap_or(false) },
        (None, None) => return Err(Error::BadRequest(shape.into())),
    };
    let audiences: Vec<String> = match o.get("audiences") {
        None | Some(Json::Null) => vec![],
        Some(Json::Array(a)) if a.len() <= 10 => a.iter().map(|v| v.as_str().filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)).map(str::to_string)).collect::<Option<_>>().ok_or_else(|| {
            Error::BadRequest("audiences are strings (256 characters at most)".into())
        })?,
        Some(_) => return Err(Error::BadRequest("audiences is a list of at most 10 strings".into())),
    };
    Ok(Want { namespace, account, audiences })
}

const RBAC: &str = "/apis/rbac.authorization.k8s.io/v1";

/// The objects made for one lease: a ServiceAccount, a Role or ClusterRole, and its binding, all
/// named `name` (`roll-<lease>`). The lease's handle.
#[derive(Serialize, Deserialize)]
struct Made {
    namespace: String,
    name: String,
    cluster_wide: bool,
}

/// Made in this order, deleted in the reverse.
#[derive(Clone, Copy)]
enum Object {
    Account,
    Role,
    Binding,
}

impl Made {
    fn kind(&self, o: Object) -> &'static str {
        match (o, self.cluster_wide) {
            (Object::Account, _) => "ServiceAccount",
            (Object::Role, false) => "Role",
            (Object::Role, true) => "ClusterRole",
            (Object::Binding, false) => "RoleBinding",
            (Object::Binding, true) => "ClusterRoleBinding",
        }
    }

    fn collection(&self, o: Object) -> String {
        let ns = &self.namespace;
        match (o, self.cluster_wide) {
            (Object::Account, _) => format!("/api/v1/namespaces/{ns}/serviceaccounts"),
            (Object::Role, false) => format!("{RBAC}/namespaces/{ns}/roles"),
            (Object::Role, true) => format!("{RBAC}/clusterroles"),
            (Object::Binding, false) => format!("{RBAC}/namespaces/{ns}/rolebindings"),
            (Object::Binding, true) => format!("{RBAC}/clusterrolebindings"),
        }
    }

    /// The step's resource, for a 403's hint (`create on rolebindings`).
    fn resource(&self, o: Object) -> String {
        let c = self.collection(o);
        c.rsplit('/').next().unwrap_or_default().to_string()
    }

    /// What an error calls the object (`the Role roll-… in ci`).
    fn what(&self, o: Object) -> String {
        match (o, self.cluster_wide) {
            (Object::Role | Object::Binding, true) => format!("the {} {}", self.kind(o), self.name),
            _ => format!("the {} {} in {}", self.kind(o), self.name, self.namespace),
        }
    }

    fn body(&self, o: Object, lease: &str, rules: &[Json]) -> Value {
        let mut metadata = json!({ "name": self.name, "labels": { "app.kubernetes.io/managed-by": "roll-secrets", "roll.sh/lease": lease } });
        if matches!(o, Object::Account) || !self.cluster_wide {
            metadata["namespace"] = json!(self.namespace);
        }
        let api = if matches!(o, Object::Account) { "v1" } else { "rbac.authorization.k8s.io/v1" };
        let mut b = json!({ "apiVersion": api, "kind": self.kind(o), "metadata": metadata });
        match o {
            // Nothing runs as it in the cluster: no pod mounts its token.
            Object::Account => b["automountServiceAccountToken"] = json!(false),
            Object::Role => b["rules"] = json!(rules),
            Object::Binding => {
                b["roleRef"] = json!({ "apiGroup": "rbac.authorization.k8s.io", "kind": self.kind(Object::Role), "name": self.name });
                b["subjects"] = json!([{ "kind": "ServiceAccount", "name": self.name, "namespace": self.namespace }]);
            }
        }
        Value::new(b.to_string())
    }
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

/// Why a status failed, in roll's words (never the server's message): `needs` is the permission
/// the step takes, `missing` what a 404 means.
fn hint(status: u16, needs: &str, missing: &str) -> String {
    match status {
        401 => ": the admin token isn't valid there (expired, or for another cluster)".into(),
        403 => format!(": the admin token may not do this (it needs {needs})"),
        404 => format!(": {missing}"),
        409 => ": it exists already".into(),
        _ => String::new(),
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

    /// A TokenRequest for `sa` in the link's namespace, expiring with the lease: the token and its
    /// expiry (unix ms).
    fn token(&self, o: &Opened, w: &Want, sa: &str, ttl: u64, generated: bool) -> Result<(Value, i64)> {
        let host = o.host();
        let mut spec = json!({ "expirationSeconds": ttl * 60 });
        if !w.audiences.is_empty() {
            spec["audiences"] = json!(w.audiences);
        }
        let body = Value::new(json!({ "apiVersion": "authentication.k8s.io/v1", "kind": "TokenRequest", "spec": spec }).to_string());
        let path = format!("/api/v1/namespaces/{}/serviceaccounts/{sa}/token", w.namespace);
        let now = self.db.now_ms()?;
        let (status, answer) = o.call(self.deliver.as_ref(), "POST", &path, Some(&body))?;
        let what = format!("{}/{sa}", w.namespace);
        if !(200..300).contains(&status) {
            let missing = if generated { "the service account roll just made is gone" } else { "no such service account in that namespace" };
            return Err(Error::Link(format!("{host} answered {status} to a token for {what}{}", hint(status, "create on serviceaccounts/token", missing))));
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
            return Err(Error::Link(format!("{host} issued {what} a token that outlives its lease ({ttl} minutes asked): roll doesn't hand it out")));
        }
        Ok((token, expires_at))
    }

    /// Create the lease's account, role and binding, in that order. When a step fails, what this
    /// issue made is deleted again (nothing here is in a database transaction), and the error names
    /// the step and the status.
    fn make(&self, o: &Opened, m: &Made, lease: &str, rules: &[Json]) -> Result<()> {
        let mut made = vec![];
        for obj in [Object::Account, Object::Role, Object::Binding] {
            let status = o.call(self.deliver.as_ref(), "POST", &m.collection(obj), Some(&m.body(obj, lease, rules))).map(|(s, _)| s);
            let failed = match status {
                Ok(s) if (200..300).contains(&s) => {
                    made.insert(0, obj);
                    continue;
                }
                Ok(s) => format!("{} answered {s} to creating {}{}", o.host(), m.what(obj), hint(s, &format!("create on {}", m.resource(obj)), &format!("no namespace {}", m.namespace))),
                Err(e) => format!("creating {}: {e}", m.what(obj)),
            };
            return Err(Error::Link(format!("{failed}{}", self.unmake(o, m, &made, lease))));
        }
        Ok(())
    }

    /// Delete `objs` after a failed issue, best effort: what an error adds about them.
    fn unmake(&self, o: &Opened, m: &Made, objs: &[Object], lease: &str) -> String {
        let left: Vec<String> = objs.iter().filter(|obj| self.delete(o, m, **obj).is_err()).map(|obj| m.kind(*obj).to_string()).collect();
        match left.as_slice() {
            [] => String::new(),
            _ => format!(" (roll couldn't delete the {} it made: they're labelled roll.sh/lease={lease})", left.join(", ")),
        }
    }

    /// Delete one object made for a lease. A 404 is already gone.
    fn delete(&self, o: &Opened, m: &Made, obj: Object) -> Result<()> {
        let path = format!("{}/{}", m.collection(obj), m.name);
        match o.call(self.deliver.as_ref(), "DELETE", &path, None)? {
            (s, _) if (200..300).contains(&s) || s == 404 => Ok(()),
            (s, _) => Err(Error::Link(format!("{} answered {s} to deleting {}{}", o.host(), m.what(obj), hint(s, &format!("delete on {}", m.resource(obj)), "")))),
        }
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

    fn maker(&self, r#ref: &str) -> Option<(String, String)> {
        self.db.read(|c| c.query_row("SELECT project_id, created_by FROM kubernetes_clusters WHERE id = ?", [r#ref], |r| Ok((r.get(0)?, r.get(1)?))).optional()).ok().flatten()
    }

    fn name(&self, r#ref: &str) -> Option<String> {
        self.db.read(|c| c.query_row("SELECT name FROM kubernetes_clusters WHERE id = ?", [r#ref], |r| r.get::<_, String>(0)).optional()).ok().flatten()
    }

    fn read(&self, _: &str, _: &str, _: &Link) -> Result<Value> {
        Err(Error::BadRequest("Kubernetes tokens are dynamic: each lease gets one of its own".into()))
    }

    /// A token that expires when the lease ends: of the link's service account, or of one made
    /// for this lease with the link's rules.
    fn issue_lease(&self, r#ref: &str, _credential: &str, link: &Link, _label: &str, ttl_minutes: u64, lease: &str) -> Result<Issued> {
        let w = want(link)?;
        let ttl = ttl_minutes.min(TTL_MAX);
        let o = self.open(r#ref)?;
        let (sa, made) = match &w.account {
            Account::Existing(sa) => (sa.clone(), None),
            Account::Generated { rules, cluster_wide } => {
                let made = Made { namespace: w.namespace.clone(), name: format!("roll-{}", lease.strip_prefix("lse_").unwrap_or(lease)), cluster_wide: *cluster_wide };
                self.make(&o, &made, lease, rules)?;
                (made.name.clone(), Some(made))
            }
        };
        let (token, expires_at) = match self.token(&o, &w, &sa, ttl, made.is_some()) {
            Ok(t) => t,
            Err(e) => return Err(match &made {
                Some(m) => Error::Link(format!("{e}{}", self.unmake(&o, m, &[Object::Binding, Object::Role, Object::Account], lease))),
                None => e,
            }),
        };
        let ca = o.ca.map(|c| c.expose().to_string()).unwrap_or_default();
        let parts = BTreeMap::from([("token".to_string(), token), ("api_url".to_string(), Value::new(o.api_url)), ("ca".to_string(), Value::new(ca))]);
        let mut handle = json!({ "namespace": w.namespace, "service_account": sa, "expires_at": expires_at });
        if let Some(m) = made {
            handle["made"] = json!(m);
        }
        Ok(Issued { parts, handle })
    }

    /// A generated account's objects are deleted (binding, role, account): its tokens end with
    /// the account. A bound token of an existing account can't be ended early: it works until its
    /// own expiry, the lease's end, and revoking a lease only records that.
    fn revoke(&self, r#ref: &str, handle: &Json) -> Result<()> {
        let Some(made) = handle.get("made").and_then(|m| serde_json::from_value::<Made>(m.clone()).ok()) else { return Ok(()) };
        let o = self.open(r#ref)?;
        for obj in [Object::Binding, Object::Role, Object::Account] {
            self.delete(&o, &made, obj)?;
        }
        Ok(())
    }

    fn revocable(&self, link: &Link) -> bool {
        link.options.contains_key("role_rules")
    }

    fn renew(&self, _: &str, _: &Json, _: i64) -> Result<()> {
        Err(Error::BadRequest("a Kubernetes token's expiry is fixed when it's issued: resolve again for a new one".into()))
    }

    fn check(&self, link: &Link) -> Result<()> {
        let w = want(link)?;
        if link.option_u64("ttl_minutes").is_some_and(|t| t > TTL_MAX) {
            let why = match w.account {
                Account::Existing(_) => "an existing service account's token can't be revoked before it expires",
                Account::Generated { .. } => "a Kubernetes token lives a day at most",
            };
            return Err(Error::BadRequest(format!("{why}: ttl_minutes is {TTL_MAX} at most (keep it short)")));
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
            let why = match status {
                401 => hint(401, "", ""),
                404 => ": is api_url the API server?".into(),
                _ => String::new(),
            };
            return Err(Error::BadRequest(format!("{host} answered {status} to the admin token{why}")));
        }
        let dek = self.project(project)?;
        let id = names::new_id("kube", 16);
        let sealed_token = dek.seal(&aad(project, &id, "token"), &token);
        let sealed_ca = ca.map(|c| dek.seal(&aad(project, &id, "ca"), &Value::new(c)));
        let now = self.db.now_ms()?;
        let k = KubernetesCluster { id: id.clone(), name: n.name, api_url, has_ca: sealed_ca.is_some(), created_at: now, created_by: caller.maker_id() };
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM kubernetes_clusters WHERE project_id = ? AND name = ?", [project, &k.name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("a Kubernetes cluster named {} exists", k.name)));
            }
            c.execute(
                "INSERT INTO kubernetes_clusters (id, project_id, name, api_url, token, ca, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, project, k.name, k.api_url, sealed_token, sealed_ca, now, k.created_by],
            )?;
            audit::record(c, self.keys()?, project, caller, "kubernetes.add", None, None, &[], &format!("added Kubernetes cluster {} ({host})", k.name), now)
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
            audit::record(c, self.keys()?, project, caller, "kubernetes.delete", None, None, &[], &format!("deleted Kubernetes cluster {name}"), now)
        })
    }

    /// The project's clusters, as link targets' (kind, ref).
    pub(crate) fn kubernetes_refs(&self, project: &str) -> Result<Vec<(String, String)>> {
        self.db.read(|c| Ok(c.query("SELECT id, name FROM kubernetes_clusters WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get(0)?, r.get(1)?)))?))
    }
}

