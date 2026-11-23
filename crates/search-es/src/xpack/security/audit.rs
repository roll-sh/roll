//! The security audit log of a cluster: Elasticsearch's audit trail (`LoggingAuditTrail`: its
//! event names and entry fields) on an Elasticsearch-flavor cluster, OpenSearch Security's audit
//! log (`AuditCategory`, the `audit_*` fields of format version 4) on an OpenSearch-flavor one.
//!
//! **What is recorded.** Failed authentications, access denials, `run_as` denials and changes of
//! the security configuration (users, roles, role mappings, API keys, privileges, service
//! tokens; OpenSearch Security's configuration documents), by default. Granted access and
//! successful authentications are recorded only when a cluster asks for them: one entry per
//! request. On Elasticsearch-flavor clusters the dynamic cluster settings
//! `xpack.security.audit.enabled` and `xpack.security.audit.logfile.events.include` / `exclude`
//! decide (node settings of the same names give the defaults); on OpenSearch-flavor ones the
//! stored audit configuration (`_plugins/_security/api/audit`: `enabled`, `enable_rest`,
//! `enable_transport`, the disabled categories, `ignore_users`, `ignore_requests`, compliance's
//! `internal_config` and its ignored users). Requests a cluster makes to itself (x-pack features,
//! background jobs) record only their denials.
//!
//! **Where entries go.** To the cluster's [`AuditLog`] (`ClusterConfig::audit`), which
//! `search-server` keeps per cluster. A cluster without one records nothing and pays nothing.
//!
//! **Never in an entry**: a password, API key, token, `Authorization` or any other header, a
//! query string or a request body. A failed authentication names the user only when Basic
//! credentials gave one. Configuration changes are built field by field from what the request
//! names (a user's `has_password`, a grant's `has_password`, never the secret), never by copying
//! the body.

use super::{AuthKind, Principal};
use crate::cluster::{Cluster, Dialect};
use crate::error::EsError;
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, EsRequest, Reply};
use crate::routes::{Api, Method};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// The header `search-server` sets to the address a client connected from (`ip:port`; the
/// PROXY protocol's source behind a load balancer). The node drops any copy a client sends.
pub const CLIENT_ADDRESS_HEADER: &str = "x-roll-client-address";

/// `xpack.security.audit.enabled`.
pub const ENABLED: &str = "xpack.security.audit.enabled";
/// `xpack.security.audit.logfile.events.include`.
pub const INCLUDE: &str = "xpack.security.audit.logfile.events.include";
/// `xpack.security.audit.logfile.events.exclude`.
pub const EXCLUDE: &str = "xpack.security.audit.logfile.events.exclude";

/// Elasticsearch's event names (`LoggingAuditTrail`), as the include and exclude settings take
/// them (in any case).
pub const EVENTS: &[&str] = &[
    "access_denied",
    "access_granted",
    "anonymous_access_denied",
    "authentication_failed",
    "authentication_success",
    "connection_denied",
    "connection_granted",
    "realm_authentication_failed",
    "run_as_denied",
    "run_as_granted",
    "security_config_change",
    "system_access_granted",
    "tampered_request",
];

/// What roll records when `include` isn't set: Elasticsearch's default list without
/// `access_granted` and `run_as_granted`, which would be an entry per request.
pub const DEFAULT_INCLUDE: &[&str] =
    &["access_denied", "anonymous_access_denied", "authentication_failed", "connection_denied", "tampered_request", "run_as_denied", "security_config_change"];

/// One entry, for the sink.
#[derive(Clone, Debug)]
pub struct Record {
    pub at_ms: i64,
    /// What the entry is: Elasticsearch's include name (`access_denied`,
    /// `security_config_change`) or OpenSearch's category (`MISSING_PRIVILEGES`).
    pub event: &'static str,
    /// The entry as the product writes it: one JSON object.
    pub entry: String,
}

/// Where a cluster's entries go.
pub trait Sink: Send + Sync {
    fn record(&self, cluster: &str, record: Record);
}

/// A cluster's audit log ([`crate::cluster::ClusterConfig::audit`]).
#[derive(Clone)]
pub struct AuditLog(pub Arc<dyn Sink>);

impl std::fmt::Debug for AuditLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuditLog")
    }
}

/// Checks the event names of an include or exclude list, as Elasticsearch does when the setting
/// is updated.
pub fn check_event_names(names: &[String]) -> Result<(), EsError> {
    match names.iter().find(|n| n.as_str() != "_all" && !EVENTS.contains(&n.to_ascii_lowercase().as_str())) {
        Some(n) => Err(EsError::illegal_argument(format!("invalid event name specified [{n}]"))),
        None => Ok(()),
    }
}

/// What happened, in either product's terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    AuthenticationFailed,
    AnonymousAccessDenied,
    AuthenticationSuccess,
    AccessDenied,
    AccessGranted,
    RunAsDenied,
    ConfigChange,
    ConfigRead,
    IndexEvent,
    /// OpenSearch Security's REST API refused to a user without its admin access.
    SecurityApiDenied,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layer {
    Rest,
    Transport,
    ComplianceRead,
    ComplianceWrite,
}

impl Kind {
    fn es(self) -> Option<&'static str> {
        Some(match self {
            Kind::AuthenticationFailed => "authentication_failed",
            Kind::AnonymousAccessDenied => "anonymous_access_denied",
            Kind::AuthenticationSuccess => "authentication_success",
            Kind::AccessDenied => "access_denied",
            Kind::AccessGranted => "access_granted",
            Kind::RunAsDenied => "run_as_denied",
            Kind::ConfigChange => "security_config_change",
            Kind::ConfigRead | Kind::IndexEvent | Kind::SecurityApiDenied => return None,
        })
    }

    fn os(self) -> Option<(&'static str, Layer)> {
        Some(match self {
            Kind::AuthenticationFailed | Kind::AnonymousAccessDenied => ("FAILED_LOGIN", Layer::Rest),
            Kind::AuthenticationSuccess => ("AUTHENTICATED", Layer::Rest),
            Kind::SecurityApiDenied => ("MISSING_PRIVILEGES", Layer::Rest),
            Kind::AccessDenied => ("MISSING_PRIVILEGES", Layer::Transport),
            Kind::AccessGranted => ("GRANTED_PRIVILEGES", Layer::Transport),
            Kind::IndexEvent => ("INDEX_EVENT", Layer::Transport),
            Kind::ConfigChange => ("COMPLIANCE_INTERNAL_CONFIG_WRITE", Layer::ComplianceWrite),
            Kind::ConfigRead => ("COMPLIANCE_INTERNAL_CONFIG_READ", Layer::ComplianceRead),
            Kind::RunAsDenied => return None,
        })
    }
}

/// Which entries a cluster records.
#[derive(Debug, Default)]
pub struct Filter {
    /// Elasticsearch: the include list less the exclude list. OpenSearch: the REST layer's
    /// categories.
    rest: Vec<&'static str>,
    /// OpenSearch: the transport layer's categories.
    transport: Vec<&'static str>,
    compliance_read: bool,
    compliance_write: bool,
    ignore_users: Vec<String>,
    read_ignore_users: Vec<String>,
    write_ignore_users: Vec<String>,
    ignore_requests: Vec<String>,
}

/// OpenSearch's `AuditCategory` values this log writes.
const OS_CATEGORIES: &[&str] = &["FAILED_LOGIN", "MISSING_PRIVILEGES", "GRANTED_PRIVILEGES", "AUTHENTICATED", "INDEX_EVENT"];

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

impl Filter {
    /// Records nothing.
    pub fn off() -> Filter {
        Filter::default()
    }

    /// A cluster's filter, from its settings (Elasticsearch) or its stored audit configuration
    /// (OpenSearch).
    fn of(cluster: &Cluster, st: &crate::meta::ClusterState) -> Filter {
        match cluster.dialect {
            Dialect::Elasticsearch => Filter::elasticsearch(cluster, st),
            Dialect::OpenSearch => Filter::opensearch(&super::opensearch::api::current_audit(st)),
        }
    }

    /// The defaults of a cluster whose state isn't at hand (a sleeping one): Elasticsearch's
    /// node settings, else roll's default; OpenSearch's stored defaults.
    pub fn defaults(dialect: Dialect) -> Filter {
        match dialect {
            Dialect::Elasticsearch => Filter::events(None, None, None),
            Dialect::OpenSearch => Filter::opensearch(&super::opensearch::api::current_audit(&crate::meta::ClusterState::default())),
        }
    }

    fn elasticsearch(cluster: &Cluster, st: &crate::meta::ClusterState) -> Filter {
        let setting = |key: &str| -> Option<Vec<String>> {
            let m = &st.metadata;
            m.transient.get_list(key).or_else(|| m.persistent.get_list(key)).or_else(|| cluster.config.node_setting(key).map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()))
        };
        let enabled = st.metadata.cluster_setting(ENABLED).or_else(|| cluster.config.node_setting(ENABLED));
        Filter::events(enabled, setting(INCLUDE), setting(EXCLUDE))
    }

    fn events(enabled: Option<&str>, include: Option<Vec<String>>, exclude: Option<Vec<String>>) -> Filter {
        if enabled.is_some_and(|v| v.eq_ignore_ascii_case("false")) {
            return Filter::off();
        }
        let has = |list: &Option<Vec<String>>, e: &str| list.as_ref().is_some_and(|l| l.iter().any(|x| x == "_all" || x.eq_ignore_ascii_case(e)));
        let rest = EVENTS
            .iter()
            .copied()
            .filter(|e| if include.is_some() { has(&include, e) } else { DEFAULT_INCLUDE.contains(e) })
            .filter(|e| !has(&exclude, e))
            .collect();
        Filter { rest, ..Filter::default() }
    }

    fn opensearch(cfg: &Value) -> Filter {
        if cfg.get("enabled").and_then(Value::as_bool) == Some(false) {
            return Filter::off();
        }
        let audit = cfg.get("audit");
        let field = |k: &str| audit.and_then(|a| a.get(k));
        let disabled = strings(field("disabled_categories"));
        let layer = |on: &str, off: &str| -> Vec<&'static str> {
            if field(on).and_then(Value::as_bool) == Some(false) {
                return Vec::new();
            }
            let off = strings(field(off));
            OS_CATEGORIES.iter().copied().filter(|c| !off.iter().chain(&disabled).any(|d| d == c)).collect()
        };
        let compliance = cfg.get("compliance");
        let c = |k: &str| compliance.and_then(|c| c.get(k));
        let internal = c("enabled").and_then(Value::as_bool) != Some(false) && c("internal_config").and_then(Value::as_bool) == Some(true);
        Filter {
            rest: layer("enable_rest", "disabled_rest_categories"),
            transport: layer("enable_transport", "disabled_transport_categories"),
            compliance_read: internal,
            compliance_write: internal,
            ignore_users: strings(field("ignore_users")),
            read_ignore_users: strings(c("read_ignore_users")),
            write_ignore_users: strings(c("write_ignore_users")),
            ignore_requests: strings(field("ignore_requests")),
        }
    }

    /// Whether anything is recorded at all.
    fn records_anything(&self) -> bool {
        !self.rest.is_empty() || !self.transport.is_empty() || self.compliance_read || self.compliance_write
    }

    /// The event name `kind` is recorded under, when this filter records it for `user` and
    /// `request` (an action or a path).
    fn wants(&self, dialect: Dialect, kind: Kind, user: Option<&str>, request: Option<&str>) -> Option<&'static str> {
        let matches = |patterns: &[String], s: Option<&str>| s.is_some_and(|s| patterns.iter().any(|p| super::privileges::glob(p, s)));
        match dialect {
            Dialect::Elasticsearch => kind.es().filter(|e| self.rest.contains(e)),
            Dialect::OpenSearch => {
                let (category, layer) = kind.os()?;
                let on = match layer {
                    Layer::Rest => self.rest.contains(&category),
                    Layer::Transport => self.transport.contains(&category),
                    Layer::ComplianceRead => return (self.compliance_read && !matches(&self.read_ignore_users, user)).then_some(category),
                    Layer::ComplianceWrite => return (self.compliance_write && !matches(&self.write_ignore_users, user)).then_some(category),
                };
                (on && !matches(&self.ignore_users, user) && !matches(&self.ignore_requests, request)).then_some(category)
            }
        }
    }

    fn wants_any(&self, dialect: Dialect, kinds: &[Kind]) -> bool {
        kinds.iter().any(|k| self.wants(dialect, *k, None, None).is_some())
    }
}

/// What a cluster's entries say about where they come from.
#[derive(Debug)]
pub struct Origin {
    pub dialect: Dialect,
    pub cluster_name: String,
    /// `None` for a sleeping cluster: Elasticsearch's `cluster.uuid` is left out.
    pub cluster_uuid: Option<String>,
    pub node_id: String,
    pub node_name: String,
}

/// A cluster's filter and origin, as of a state version.
#[derive(Debug)]
pub struct Snapshot {
    pub filter: Filter,
    pub origin: Origin,
}

impl Snapshot {
    /// The defaults for a cluster that isn't loaded ([`Filter::defaults`]).
    pub fn defaults(dialect: Dialect, cluster_name: &str, node_id: &str, node_name: &str) -> Snapshot {
        Snapshot {
            filter: Filter::defaults(dialect),
            origin: Origin { dialect, cluster_name: cluster_name.to_string(), cluster_uuid: None, node_id: node_id.to_string(), node_name: node_name.to_string() },
        }
    }
}

/// The cluster's current filter and origin (cached per state version).
pub fn snapshot(cluster: &Cluster) -> Arc<Snapshot> {
    let st = cluster.state();
    let mut cached = cluster.xpack.audit.lock();
    if let Some((v, s)) = &*cached
        && *v == st.version
    {
        return s.clone();
    }
    let s = Arc::new(Snapshot {
        filter: Filter::of(cluster, &st),
        origin: Origin {
            dialect: cluster.dialect,
            cluster_name: cluster.name.clone(),
            cluster_uuid: Some(st.metadata.cluster_uuid.clone()),
            node_id: cluster.node.id.clone(),
            node_name: cluster.node.name.clone(),
        },
    });
    *cached = Some((st.version, s.clone()));
    s
}

// ------------------------------------------------------------------------------ requests

/// The audit context of one client request: its id, method, path and address, and what the
/// cluster records.
pub struct Request {
    log: AuditLog,
    snap: Arc<Snapshot>,
    id: OnceLock<String>,
    method: String,
    path: String,
    address: Option<String>,
    /// Set by the request's first dispatch ([`enter`]): later ones are calls it makes itself.
    entered: AtomicBool,
}

tokio::task_local! {
    static REQUEST: Arc<Request>;
}

/// The audit context of a client's request to `cluster`, when the cluster records anything.
pub fn scope(cluster: &Cluster, req: &EsRequest) -> Option<Arc<Request>> {
    let log = cluster.config.audit.clone()?;
    let snap = snapshot(cluster);
    if !snap.filter.records_anything() {
        return None;
    }
    Some(Arc::new(Request {
        log,
        snap,
        id: OnceLock::new(),
        method: req.method.to_ascii_uppercase(),
        path: req.uri.split_once('?').map_or(req.uri.as_str(), |(p, _)| p).to_string(),
        address: req.header(CLIENT_ADDRESS_HEADER).map(str::to_string),
        entered: AtomicBool::new(false),
    }))
}

/// Runs `f` (a client request's dispatch) in its audit context.
pub async fn run<F: std::future::Future>(scope: Option<Arc<Request>>, f: F) -> F::Output {
    match scope {
        Some(r) => REQUEST.scope(r, f).await,
        None => f.await,
    }
}

/// The audit context when this dispatch is the client's request itself (`None` for the calls it
/// makes to the cluster, and outside a client request).
pub fn enter() -> Option<Arc<Request>> {
    REQUEST.try_with(|r| (!r.entered.swap(true, Ordering::SeqCst)).then(|| r.clone())).ok().flatten()
}

fn current() -> Option<Arc<Request>> {
    REQUEST.try_with(Arc::clone).ok()
}

impl Request {
    fn id(&self) -> &str {
        self.id.get_or_init(crate::meta::names::random_uuid)
    }

    fn emit(&self, kind: Kind, user: Option<&str>, request: Option<&str>, build: impl FnOnce(&'static str, &Entry) -> Value) {
        let d = self.snap.origin.dialect;
        let Some(event) = self.snap.filter.wants(d, kind, user, request) else { return };
        let entry = Entry { origin: &self.snap.origin, request: Some(self), at_ms: crate::time::now_millis() };
        let v = build(event, &entry);
        self.log.0.record(&self.snap.origin.cluster_name, Record { at_ms: entry.at_ms, event, entry: v.to_json() });
    }

    /// A request whose credentials failed (`Authorization` as sent: only a Basic user name is
    /// read from it).
    pub fn authentication_failed(&self, req: &EsRequest) {
        record_failed_login(&self.log, &self.snap, req, basic_user(req.header("authorization")).as_deref(), Some(self));
    }

    /// The request authenticated (recorded only when asked for).
    pub fn authenticated(&self, ctx: &Ctx) {
        if !self.snap.filter.wants_any(self.snap.origin.dialect, &[Kind::AuthenticationSuccess]) {
            return;
        }
        let p = audited_principal(ctx);
        self.emit(Kind::AuthenticationSuccess, Some(&p.username), Some(&self.path), |event, e| match e.origin.dialect {
            Dialect::Elasticsearch => {
                let mut m = e.es_head("rest", event);
                es_principal(&mut m, &p, true);
                e.es_rest_tail(&mut m);
                Value::Obj(m)
            }
            Dialect::OpenSearch => {
                let mut m = e.os_head(event, "REST");
                e.os_rest(&mut m);
                os_user(&mut m, &p);
                Value::Obj(m)
            }
        });
    }

    /// The request was authorized (recorded only when asked for).
    pub fn granted(&self, ctx: &Ctx) {
        let d = self.snap.origin.dialect;
        if !self.snap.filter.wants_any(d, &[Kind::AccessGranted, Kind::IndexEvent]) {
            return;
        }
        let p = audited_principal(ctx);
        let (_, action) = super::privileges::action_of(ctx.api.name());
        let indices = super::request_index_names(ctx);
        let indices = (!indices.is_empty()).then_some(indices.as_slice());
        self.emit(Kind::AccessGranted, Some(&p.username), Some(&action), |event, e| e.transport(event, &p, &action, indices, None));
        if d == Dialect::OpenSearch && action.starts_with("indices:admin/") {
            self.emit(Kind::IndexEvent, Some(&p.username), Some(&action), |event, e| e.transport(event, &p, &action, indices, None));
        }
    }

    /// A finished request: a change (or, on OpenSearch, a read) of the security configuration.
    pub fn finished(&self, ctx: &Ctx, reply: &Reply) {
        match self.snap.origin.dialect {
            Dialect::Elasticsearch => {
                if !self.snap.filter.wants_any(Dialect::Elasticsearch, &[Kind::ConfigChange]) {
                    return;
                }
                for (action, body) in config_changes(ctx, reply) {
                    self.emit(Kind::ConfigChange, None, None, |event, e| {
                        let mut m = e.es_head(event, action);
                        m.insert("request.id".into(), self.id().into());
                        if let Value::Obj(b) = body {
                            m.extend(b);
                        }
                        Value::Obj(m)
                    });
                }
            }
            Dialect::OpenSearch => {
                let Some(doc) = os_config_doc(ctx.api) else { return };
                if !matches!(reply, Reply::Body(s, _) | Reply::Empty(s) | Reply::Raw(s, ..) | Reply::Stream(s, ..) if *s < 300) {
                    return;
                }
                let kind = if ctx.method == Method::Get { Kind::ConfigRead } else { Kind::ConfigChange };
                let p = audited_principal(ctx);
                self.emit(kind, Some(&p.username), None, |event, e| {
                    let mut m = e.os_head(event, "");
                    m.shift_remove("audit_request_layer");
                    if kind == Kind::ConfigChange {
                        m.insert("audit_compliance_operation".into(), "UPDATE".into());
                    }
                    m.insert("audit_trace_doc_id".into(), doc.into());
                    m.insert("audit_trace_indices".into(), Value::Arr(vec![".opendistro_security".into()]));
                    m.insert("audit_trace_resolved_indices".into(), Value::Arr(vec![".opendistro_security".into()]));
                    e.os_rest(&mut m);
                    os_user(&mut m, &p);
                    Value::Obj(m)
                });
            }
        }
    }
}

/// Elasticsearch's `access_denied` or OpenSearch's `MISSING_PRIVILEGES` for `p`, in the running
/// request's audit context (nothing outside one).
pub(crate) fn access_denied(p: &Principal, action: &str, indices: Option<&[String]>) {
    let Some(r) = current() else { return };
    r.emit(Kind::AccessDenied, Some(&p.username), Some(action), |event, e| e.transport(event, p, action, indices, None));
}

/// Elasticsearch's `run_as_denied`: `authn` may not run as `target`.
pub(crate) fn run_as_denied(ctx: &Ctx, authn: &Principal, target: &str, action: &str) {
    let Some(r) = current() else { return };
    let target_realm = super::users::lookup_user(ctx.cluster, target).map(|u| u.realm_name);
    let indices = super::request_index_names(ctx);
    let indices = (!indices.is_empty()).then_some(indices.as_slice());
    r.emit(Kind::RunAsDenied, Some(&authn.username), Some(action), |event, e| e.transport(event, authn, action, indices, Some((target, target_realm))));
}

/// OpenSearch Security's REST API refused `p` (`reason`: the plugin's text).
pub(crate) fn security_api_denied(p: &Principal, reason: &str) {
    let Some(r) = current() else { return };
    r.emit(Kind::SecurityApiDenied, Some(&p.username), Some(&r.path), |event, e| {
        let mut m = e.os_head(event, "REST");
        e.os_rest(&mut m);
        m.insert("audit_request_id".into(), r.id().into());
        os_user(&mut m, p);
        m.insert("audit_request_privilege".into(), reason.into());
        Value::Obj(m)
    });
}

/// Records a failed authentication of `req` that the node refused before the cluster saw it
/// (`snap`: the cluster's, or [`Snapshot::defaults`] for a sleeping one). `user`: the Basic user
/// name given, if any.
pub fn failed_login(log: &AuditLog, snap: &Snapshot, req: &EsRequest, user: Option<&str>) {
    record_failed_login(log, snap, req, user, None);
}

fn record_failed_login(log: &AuditLog, snap: &Snapshot, req: &EsRequest, user: Option<&str>, request: Option<&Request>) {
    let d = snap.origin.dialect;
    // Elasticsearch: a Basic user that failed is `authentication_failed`; no credentials, or an
    // API key or token that failed, end as `anonymous_access_denied` (the realms find no user).
    let kind = if user.is_some() { Kind::AuthenticationFailed } else { Kind::AnonymousAccessDenied };
    let path = req.uri.split_once('?').map_or(req.uri.as_str(), |(p, _)| p);
    let Some(event) = snap.filter.wants(d, kind, user, Some(path)) else { return };
    let owned;
    let request = match request {
        Some(r) => r,
        None => {
            owned = Request {
                log: log.clone(),
                snap: Arc::new(Snapshot { filter: Filter::off(), origin: clone_origin(&snap.origin) }),
                id: OnceLock::new(),
                method: req.method.to_ascii_uppercase(),
                path: path.to_string(),
                address: req.header(CLIENT_ADDRESS_HEADER).map(str::to_string),
                entered: AtomicBool::new(true),
            };
            &owned
        }
    };
    let e = Entry { origin: &snap.origin, request: Some(request), at_ms: crate::time::now_millis() };
    let v = match d {
        Dialect::Elasticsearch => {
            let mut m = e.es_head("rest", event);
            if let Some(u) = user {
                m.insert("user.name".into(), u.into());
            }
            e.es_rest_tail(&mut m);
            Value::Obj(m)
        }
        Dialect::OpenSearch => {
            let mut m = e.os_head(event, "REST");
            e.os_rest(&mut m);
            m.insert("audit_request_effective_user".into(), user.unwrap_or("<NONE>").into());
            m.insert("audit_request_effective_user_is_admin".into(), false.into());
            Value::Obj(m)
        }
    };
    log.0.record(&snap.origin.cluster_name, Record { at_ms: e.at_ms, event, entry: v.to_json() });
}

fn clone_origin(o: &Origin) -> Origin {
    Origin { dialect: o.dialect, cluster_name: o.cluster_name.clone(), cluster_uuid: o.cluster_uuid.clone(), node_id: o.node_id.clone(), node_name: o.node_name.clone() }
}

/// The user name of a Basic `Authorization` value (never its password).
pub fn basic_user(header: Option<&str>) -> Option<String> {
    let h = header?.trim();
    let (scheme, value) = h.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = super::b64_decode(value.trim()).ok()?;
    let text = String::from_utf8_lossy(&decoded);
    text.split_once(':').map(|(u, _)| u.to_string())
}

/// The principal an entry names: the authenticated one, else the cluster's owner (`elastic`, or
/// OpenSearch's `admin`).
fn audited_principal(ctx: &Ctx) -> Principal {
    match ctx.cluster.dialect {
        Dialect::OpenSearch => super::opensearch::principal(ctx),
        Dialect::Elasticsearch => super::principal(ctx).clone(),
    }
}

// ------------------------------------------------------------------------------ entries

struct Entry<'a> {
    origin: &'a Origin,
    request: Option<&'a Request>,
    at_ms: i64,
}

impl Entry<'_> {
    /// `type`, `timestamp`, `cluster.uuid`, `node.id`, `event.type`, `event.action`.
    fn es_head(&self, event_type: &str, action: &str) -> Map {
        let mut m = Map::new();
        m.insert("type".into(), "audit".into());
        m.insert("timestamp".into(), timestamp(self.at_ms, ',', "+0000").into());
        if let Some(u) = &self.origin.cluster_uuid {
            m.insert("cluster.uuid".into(), u.as_str().into());
        }
        m.insert("node.id".into(), self.origin.node_id.as_str().into());
        m.insert("event.type".into(), event_type.into());
        m.insert("event.action".into(), action.into());
        m
    }

    /// `origin.type`, `origin.address`, `url.path`, `request.method`, `request.id`.
    fn es_rest_tail(&self, m: &mut Map) {
        m.insert("origin.type".into(), "rest".into());
        if let Some(r) = self.request {
            if let Some(a) = &r.address {
                m.insert("origin.address".into(), a.as_str().into());
            }
            m.insert("url.path".into(), r.path.as_str().into());
            m.insert("request.method".into(), r.method.as_str().into());
            m.insert("request.id".into(), r.id().into());
        }
    }

    /// The `audit_*` fields every OpenSearch entry has; `layer` empty for compliance entries.
    fn os_head(&self, category: &str, layer: &str) -> Map {
        let mut m = Map::new();
        m.insert("audit_format_version".into(), 4.into());
        m.insert("audit_category".into(), category.into());
        m.insert("@timestamp".into(), timestamp(self.at_ms, '.', "+00:00").into());
        m.insert("audit_cluster_name".into(), self.origin.cluster_name.as_str().into());
        m.insert("audit_node_id".into(), self.origin.node_id.as_str().into());
        m.insert("audit_node_name".into(), self.origin.node_name.as_str().into());
        m.insert("audit_request_layer".into(), layer.into());
        m.insert("audit_request_origin".into(), "REST".into());
        if let Some(a) = self.request.and_then(|r| r.address.as_deref()) {
            // The plugin prints the address without its port.
            let ip = a.rsplit_once(':').map_or(a, |(ip, _)| ip).trim_start_matches('[').trim_end_matches(']');
            m.insert("audit_request_remote_address".into(), ip.into());
        }
        m
    }

    /// `audit_rest_request_method` and `audit_rest_request_path`.
    fn os_rest(&self, m: &mut Map) {
        if let Some(r) = self.request {
            m.insert("audit_rest_request_method".into(), r.method.as_str().into());
            m.insert("audit_rest_request_path".into(), r.path.as_str().into());
        }
    }

    /// An action's entry (`access_granted`, `access_denied`, `run_as_denied`; OpenSearch's
    /// transport categories). `run_as`: the user `p` tried to run as, and its realm.
    fn transport(&self, event: &str, p: &Principal, action: &str, indices: Option<&[String]>, run_as: Option<(&str, Option<&str>)>) -> Value {
        let id = self.request.map(|r| r.id().to_string());
        match self.origin.dialect {
            Dialect::Elasticsearch => {
                let mut m = self.es_head("transport", event);
                match run_as {
                    Some((target, realm)) => {
                        m.insert("user.name".into(), p.username.as_str().into());
                        m.insert("user.run_as.name".into(), target.into());
                        m.insert("user.realm".into(), p.realm_name.into());
                        if let Some(r) = realm {
                            m.insert("user.run_as.realm".into(), r.into());
                        }
                        m.insert("user.roles".into(), strs(&p.roles));
                    }
                    None => es_principal(&mut m, p, true),
                }
                m.insert("origin.type".into(), "rest".into());
                if let Some(a) = self.request.and_then(|r| r.address.as_deref()) {
                    m.insert("origin.address".into(), a.into());
                }
                if let Some(id) = id {
                    m.insert("request.id".into(), id.into());
                }
                m.insert("action".into(), action.into());
                if let Some(n) = request_name(action) {
                    m.insert("request.name".into(), n.into());
                }
                if let Some(i) = indices {
                    m.insert("indices".into(), strs(i));
                }
                Value::Obj(m)
            }
            Dialect::OpenSearch => {
                let mut m = self.os_head(event, "TRANSPORT");
                if let Some(n) = request_name(action) {
                    m.insert("audit_transport_request_type".into(), n.into());
                }
                if let Some(id) = id {
                    m.insert("audit_request_id".into(), id.into());
                }
                os_user(&mut m, p);
                m.insert("audit_request_auth_method".into(), "basic".into());
                m.insert("audit_request_privilege".into(), action.into());
                if let Some(i) = indices {
                    m.insert("audit_trace_indices".into(), strs(i));
                    m.insert("audit_trace_resolved_indices".into(), strs(i));
                }
                Value::Obj(m)
            }
        }
    }
}

fn strs(v: &[String]) -> Value {
    Value::Arr(v.iter().map(|s| Value::from(s.as_str())).collect())
}

/// `authentication.type` and the `user.*` / `apikey.*` fields of Elasticsearch's entries.
fn es_principal(m: &mut Map, p: &Principal, with_type: bool) {
    if with_type {
        let ty = match &p.kind {
            AuthKind::Realm | AuthKind::OpenSearch(_) => "REALM",
            AuthKind::ApiKey(_) => "API_KEY",
            AuthKind::Token | AuthKind::ServiceAccount { .. } => "TOKEN",
        };
        m.insert("authentication.type".into(), ty.into());
    }
    m.insert("user.name".into(), p.username.as_str().into());
    if let Some(a) = &p.authenticating {
        m.insert("user.run_by.name".into(), a.username.as_str().into());
    }
    if let AuthKind::ApiKey(k) = &p.kind {
        m.insert("apikey.id".into(), k.id.as_str().into());
        if !k.name.is_null() {
            m.insert("apikey.name".into(), k.name.clone());
        }
        return;
    }
    m.insert("user.realm".into(), p.realm_name.into());
    if let Some(a) = &p.authenticating {
        m.insert("user.run_by.realm".into(), a.realm_name.into());
    }
    m.insert("user.roles".into(), strs(&p.roles));
}

/// `audit_request_effective_user` and its roles.
fn os_user(m: &mut Map, p: &Principal) {
    m.insert("audit_request_effective_user".into(), p.username.as_str().into());
    if !p.roles.is_empty() {
        m.insert("audit_request_user_roles".into(), strs(&p.roles));
    }
}

/// `2026-11-17T18:29:32,840+0000` (Elasticsearch) or `2026-11-17T18:30:39.762+00:00`
/// (OpenSearch): UTC.
fn timestamp(ms: i64, sep: char, zone: &str) -> String {
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let (h, mi, s, milli) = (rem / 3_600_000, rem / 60_000 % 60, rem / 1000 % 60, rem % 1000);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}{sep}{milli:03}{zone}")
}

/// The transport request class of an action, as both products name it.
fn request_name(action: &str) -> Option<&'static str> {
    Some(match action {
        "indices:data/read/search" => "SearchRequest",
        "indices:data/read/msearch" => "MultiSearchRequest",
        "indices:data/read/get" => "GetRequest",
        "indices:data/read/mget" => "MultiGetRequest",
        "indices:data/read/field_caps" => "FieldCapabilitiesRequest",
        "indices:data/read/scroll" => "SearchScrollRequest",
        "indices:data/read/esql" => "EsqlQueryRequest",
        "indices:data/read/sql" => "SqlQueryRequest",
        "indices:data/write/index" => "IndexRequest",
        "indices:data/write/bulk" => "BulkRequest",
        "indices:data/write/delete" => "DeleteRequest",
        "indices:data/write/update" => "UpdateRequest",
        "indices:data/write/delete/byquery" => "DeleteByQueryRequest",
        "indices:data/write/update/byquery" => "UpdateByQueryRequest",
        "indices:data/write/reindex" => "ReindexRequest",
        "indices:admin/create" => "CreateIndexRequest",
        "indices:admin/delete" => "DeleteIndexRequest",
        "indices:admin/mapping/put" => "PutMappingRequest",
        "indices:admin/refresh" => "RefreshRequest",
        "indices:admin/aliases" => "IndicesAliasesRequest",
        "indices:admin/settings/update" => "UpdateSettingsRequest",
        "cluster:monitor/main" => "MainRequest",
        "cluster:monitor/health" => "ClusterHealthRequest",
        "cluster:admin/settings/update" => "ClusterUpdateSettingsRequest",
        "cluster:admin/xpack/security/user/put" => "PutUserRequest",
        "cluster:admin/xpack/security/user/delete" => "DeleteUserRequest",
        "cluster:admin/xpack/security/user/change_password" => "ChangePasswordRequest",
        "cluster:admin/xpack/security/user/set_enabled" => "SetEnabledRequest",
        "cluster:admin/xpack/security/role/put" => "PutRoleRequest",
        "cluster:admin/xpack/security/role/delete" => "DeleteRoleRequest",
        "cluster:admin/xpack/security/role_mapping/put" => "PutRoleMappingRequest",
        "cluster:admin/xpack/security/role_mapping/delete" => "DeleteRoleMappingRequest",
        "cluster:admin/xpack/security/api_key/create" => "CreateApiKeyRequest",
        "cluster:admin/xpack/security/api_key/grant" => "GrantApiKeyRequest",
        "cluster:admin/xpack/security/api_key/invalidate" => "InvalidateApiKeyRequest",
        "cluster:admin/xpack/security/api_key/update" => "UpdateApiKeyRequest",
        _ => return None,
    })
}

// ------------------------------------------------------------------------------ config changes

/// OpenSearch Security's configuration document an API reads or writes.
fn os_config_doc(api: Api) -> Option<&'static str> {
    Some(match api {
        Api::PluginsSecurityInternalusers | Api::PluginsSecurityAccount => "internalusers",
        Api::PluginsSecurityRoles => "roles",
        Api::PluginsSecurityRolesmapping => "rolesmapping",
        Api::PluginsSecurityActiongroups => "actiongroups",
        Api::PluginsSecurityTenants => "tenants",
        Api::PluginsSecurityAudit => "audit",
        Api::PluginsSecuritySecurityconfig => "config",
        _ => return None,
    })
}

/// Copies `keys` of `from` that are present into a new object, in that order.
fn pick(from: &Value, keys: &[&str]) -> Map {
    let mut m = Map::new();
    for k in keys {
        if let Some(v) = from.get(k) {
            m.insert((*k).to_string(), v.clone());
        }
    }
    m
}

/// A role descriptor as Elasticsearch prints it in entries (its privileges, never anything else
/// from the body).
fn role_descriptor(d: &Value) -> Value {
    let mut m = Map::new();
    let list = |k: &str| d.get(k).cloned().unwrap_or_else(|| Value::Arr(Vec::new()));
    m.insert("cluster".into(), list("cluster"));
    if let Some(g) = d.get("global") {
        m.insert("global".into(), g.clone());
    }
    m.insert("indices".into(), list("indices"));
    m.insert("applications".into(), list("applications"));
    m.insert("run_as".into(), list("run_as"));
    for k in ["remote_indices", "remote_cluster", "restriction", "description"] {
        if let Some(v) = d.get(k) {
            m.insert(k.into(), v.clone());
        }
    }
    if let Some(md) = d.get("metadata").filter(|v| v.as_object().is_some_and(|o| !o.is_empty())) {
        m.insert("metadata".into(), md.clone());
    }
    Value::Obj(m)
}

fn descriptors(v: Option<&Value>) -> Value {
    Value::Arr(v.and_then(Value::as_object).map(|o| o.values().map(role_descriptor).collect()).unwrap_or_default())
}

/// Elasticsearch's `security_config_change` entries for a finished request: `(event.action,
/// {put|delete|change|create|invalidate: …})` each. Built from named fields only.
fn config_changes(ctx: &Ctx, reply: &Reply) -> Vec<(&'static str, Value)> {
    let body = ctx.body().ok().flatten().unwrap_or(Value::Null);
    let param = |n: &str| ctx.path_param(n).unwrap_or_default().to_string();
    let replied = match reply {
        Reply::Body(_, v) => v.clone(),
        _ => Value::Null,
    };
    let one = |action: &'static str, verb: &str, what: &str, v: Value| vec![(action, obj! { verb => obj! { what => v } })];
    match ctx.api {
        Api::SecurityPutUser => {
            let mut m = Map::new();
            m.insert("name".into(), param("username").into());
            m.insert("enabled".into(), body.get("enabled").cloned().unwrap_or(Value::from(true)));
            m.insert("roles".into(), body.get("roles").cloned().unwrap_or_else(|| Value::Arr(Vec::new())));
            m.extend(pick(&body, &["full_name", "email"]));
            m.insert("has_password".into(), (body.get("password").is_some() || body.get("password_hash").is_some()).into());
            m.extend(pick(&body, &["metadata"]));
            one("put_user", "put", "user", Value::Obj(m))
        }
        Api::SecurityDeleteUser => one("delete_user", "delete", "user", obj! { "name" => param("username") }),
        Api::SecurityChangePassword => {
            let name = ctx.path_param("username").map(str::to_string).unwrap_or_else(|| super::principal(ctx).username.clone());
            one("change_password", "change", "password", obj! { "user" => obj! { "name" => name } })
        }
        Api::SecurityEnableUser => vec![("change_enable_user", obj! { "change" => obj! { "enable" => obj! { "user" => obj! { "name" => param("username") } } } })],
        Api::SecurityDisableUser => vec![("change_disable_user", obj! { "change" => obj! { "disable" => obj! { "user" => obj! { "name" => param("username") } } } })],
        Api::SecurityPutRole => one("put_role", "put", "role", obj! { "name" => param("name"), "role_descriptor" => role_descriptor(&body) }),
        Api::SecurityBulkPutRole => body
            .get("roles")
            .and_then(Value::as_object)
            .map(|roles| roles.iter().map(|(n, d)| ("put_role", obj! { "put" => obj! { "role" => obj! { "name" => n.as_str(), "role_descriptor" => role_descriptor(d) } } })).collect())
            .unwrap_or_default(),
        Api::SecurityDeleteRole => one("delete_role", "delete", "role", obj! { "name" => param("name") }),
        Api::SecurityBulkDeleteRole => strings(body.get("names")).into_iter().map(|n| ("delete_role", obj! { "delete" => obj! { "role" => obj! { "name" => n } } })).collect(),
        Api::SecurityPutRoleMapping => {
            let mut m = Map::new();
            m.insert("name".into(), param("name").into());
            m.extend(pick(&body, &["roles", "role_templates", "rules", "enabled", "metadata"]));
            one("put_role_mapping", "put", "role_mapping", Value::Obj(m))
        }
        Api::SecurityDeleteRoleMapping => one("delete_role_mapping", "delete", "role_mapping", obj! { "name" => param("name") }),
        Api::SecurityCreateApiKey | Api::SecurityGrantApiKey | Api::SecurityCreateCrossClusterApiKey => {
            let key = if ctx.api == Api::SecurityGrantApiKey { body.get("api_key").cloned().unwrap_or(Value::Null) } else { body.clone() };
            let mut m = Map::new();
            m.insert("id".into(), replied.get("id").cloned().unwrap_or(Value::Null));
            m.insert("name".into(), key.get("name").cloned().unwrap_or(Value::Null));
            m.insert("type".into(), if ctx.api == Api::SecurityCreateCrossClusterApiKey { "cross_cluster" } else { "rest" }.into());
            m.insert("expiration".into(), key.get("expiration").cloned().unwrap_or(Value::Null));
            if ctx.api != Api::SecurityCreateCrossClusterApiKey {
                m.insert("role_descriptors".into(), descriptors(key.get("role_descriptors")));
            }
            m.extend(pick(&key, &["metadata"]));
            let mut create = Map::new();
            create.insert("apikey".into(), Value::Obj(m));
            if ctx.api == Api::SecurityGrantApiKey {
                // The grant's credentials are never read, only named.
                let ty = body.get("grant_type").cloned().unwrap_or(Value::Null);
                let grant = match body.get("username") {
                    Some(u) => obj! { "type" => ty, "user" => obj! { "name" => u.clone(), "has_password" => body.get("password").is_some() } },
                    None => obj! { "type" => ty, "has_access_token" => body.get("access_token").is_some() },
                };
                create.insert("grant".into(), grant);
            }
            vec![("create_apikey", obj! { "create" => Value::Obj(create) })]
        }
        Api::SecurityInvalidateApiKey => {
            let mut m = Map::new();
            let ids: Vec<Value> = body.get("ids").and_then(Value::as_array).cloned().or_else(|| body.get("id").map(|i| vec![i.clone()])).unwrap_or_default();
            if !ids.is_empty() {
                m.insert("ids".into(), Value::Arr(ids));
            }
            m.extend(pick(&body, &["name"]));
            m.insert("owned_by_authenticated_user".into(), body.get("owner").and_then(Value::as_bool).unwrap_or(false).into());
            if let Some(u) = body.get("username") {
                let mut user = Map::new();
                user.insert("name".into(), u.clone());
                if let Some(r) = body.get("realm_name") {
                    user.insert("realm".into(), r.clone());
                }
                m.insert("user".into(), Value::Obj(user));
            }
            one("invalidate_apikeys", "invalidate", "apikeys", Value::Obj(m))
        }
        Api::SecurityUpdateApiKey | Api::SecurityUpdateCrossClusterApiKey => {
            let mut m = Map::new();
            m.insert("id".into(), param("id").into());
            if ctx.api == Api::SecurityUpdateApiKey && body.get("role_descriptors").is_some() {
                m.insert("role_descriptors".into(), descriptors(body.get("role_descriptors")));
            }
            m.extend(pick(&body, &["metadata", "expiration"]));
            one("change_apikey", "change", "apikey", Value::Obj(m))
        }
        Api::SecurityBulkUpdateApiKeys => {
            let mut m = pick(&body, &["ids"]);
            if body.get("role_descriptors").is_some() {
                m.insert("role_descriptors".into(), descriptors(body.get("role_descriptors")));
            }
            m.extend(pick(&body, &["metadata", "expiration"]));
            one("change_apikeys", "change", "apikeys", Value::Obj(m))
        }
        Api::SecurityPutPrivileges => {
            let mut out = Vec::new();
            for (app, privs) in body.as_object().into_iter().flatten() {
                for (name, p) in privs.as_object().into_iter().flatten() {
                    let mut m = Map::new();
                    m.insert("application".into(), app.as_str().into());
                    m.insert("name".into(), name.as_str().into());
                    m.extend(pick(p, &["actions", "metadata"]));
                    out.push(Value::Obj(m));
                }
            }
            vec![("put_privileges", obj! { "put" => obj! { "privileges" => Value::Arr(out) } })]
        }
        Api::SecurityDeletePrivileges => one(
            "delete_privileges",
            "delete",
            "privileges",
            obj! { "application" => param("application"), "privileges" => Value::Arr(param("name").split(',').map(Value::from).collect()) },
        ),
        Api::SecurityCreateServiceToken => {
            let name = ctx.path_param("name").map(Value::from).or_else(|| replied.pointer(&["token", "name"]).cloned()).unwrap_or(Value::Null);
            one("create_service_token", "create", "service_token", obj! { "namespace" => param("namespace"), "service" => param("service"), "name" => name })
        }
        Api::SecurityDeleteServiceToken => {
            one("delete_service_token", "delete", "service_token", obj! { "namespace" => param("namespace"), "service" => param("service"), "name" => param("name") })
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_each_products_format() {
        assert_eq!(timestamp(1_795_026_574_295, ',', "+0000"), "2026-11-18T18:29:34,295+0000");
        assert_eq!(timestamp(0, '.', "+00:00"), "1970-01-01T00:00:00.000+00:00");
    }

    #[test]
    fn elasticsearch_include_and_exclude() {
        let f = Filter::events(None, None, None);
        assert_eq!(f.wants(Dialect::Elasticsearch, Kind::AccessDenied, None, None), Some("access_denied"));
        assert_eq!(f.wants(Dialect::Elasticsearch, Kind::AccessGranted, None, None), None);
        let f = Filter::events(None, Some(vec!["_all".into()]), Some(vec!["ACCESS_DENIED".into()]));
        assert!(f.wants(Dialect::Elasticsearch, Kind::AccessGranted, None, None).is_some());
        assert!(f.wants(Dialect::Elasticsearch, Kind::AccessDenied, None, None).is_none());
        assert!(!Filter::events(Some("false"), None, None).records_anything());
        assert!(check_event_names(&["Access_Granted".into(), "_all".into()]).is_ok());
        assert_eq!(check_event_names(&["bogus".into()]).unwrap_err().reason(), "invalid event name specified [bogus]");
    }

    #[test]
    fn opensearch_categories_follow_the_stored_config() {
        let f = Filter::defaults(Dialect::OpenSearch);
        let d = Dialect::OpenSearch;
        assert_eq!(f.wants(d, Kind::AuthenticationFailed, Some("bob"), None), Some("FAILED_LOGIN"));
        assert_eq!(f.wants(d, Kind::AccessDenied, Some("bob"), None), Some("MISSING_PRIVILEGES"));
        assert_eq!(f.wants(d, Kind::AccessGranted, Some("bob"), None), None);
        assert_eq!(f.wants(d, Kind::ConfigChange, Some("admin"), None), Some("COMPLIANCE_INTERNAL_CONFIG_WRITE"));
        // `ignore_users` holds kibanaserver by default.
        assert_eq!(f.wants(d, Kind::AccessDenied, Some("kibanaserver"), None), None);
        let cfg = crate::json::parse(br#"{"enabled":true,"audit":{"enable_rest":false,"disabled_transport_categories":[],"ignore_requests":["indices:data/read/*"]},"compliance":{"enabled":true,"internal_config":false}}"#).unwrap().unwrap();
        let f = Filter::opensearch(&cfg);
        assert_eq!(f.wants(d, Kind::AuthenticationFailed, None, None), None);
        assert_eq!(f.wants(d, Kind::AccessGranted, Some("bob"), Some("indices:admin/create")), Some("GRANTED_PRIVILEGES"));
        assert_eq!(f.wants(d, Kind::AccessGranted, Some("bob"), Some("indices:data/read/search")), None);
        assert_eq!(f.wants(d, Kind::ConfigChange, Some("admin"), None), None);
    }

    #[test]
    fn a_failed_basic_login_names_the_user_and_nothing_else() {
        use base64::Engine;
        let h = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("bob:hunter2-secret"));
        assert_eq!(basic_user(Some(&h)).as_deref(), Some("bob"));
        assert_eq!(basic_user(Some("ApiKey abc")), None);
        struct Keep(parking_lot::Mutex<Vec<Record>>);
        impl Sink for Keep {
            fn record(&self, _cluster: &str, r: Record) {
                self.0.lock().push(r);
            }
        }
        let keep = Arc::new(Keep(Default::default()));
        let log = AuditLog(keep.clone());
        let mut req = EsRequest::new("GET", "/secret/_search?q=hunter2-secret");
        req.headers.push(("Authorization".into(), h));
        req.headers.push((CLIENT_ADDRESS_HEADER.into(), "10.0.0.7:5544".into()));
        for d in [Dialect::Elasticsearch, Dialect::OpenSearch] {
            failed_login(&log, &Snapshot::defaults(d, "c1", "n1", "node-1"), &req, basic_user(req.header("authorization")).as_deref());
        }
        let got = keep.0.lock();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].event, "authentication_failed");
        assert_eq!(got[1].event, "FAILED_LOGIN");
        for r in got.iter() {
            assert!(r.entry.contains("bob") && r.entry.contains("/secret/_search"), "{}", r.entry);
            assert!(!r.entry.contains("hunter2") && !r.entry.contains("Basic"), "a credential in an entry: {}", r.entry);
        }
    }
}
