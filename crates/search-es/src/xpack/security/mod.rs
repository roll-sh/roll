//! Security: the native realm (users, roles, role mappings), API keys, OAuth2 tokens, service
//! accounts, user profiles, application privileges, and the authentication and authorization
//! of every request.
//!
//! **Authentication** ([`authenticate`]) runs before a request is parsed, like Elasticsearch's
//! security REST filter. A request is authenticated by, in order:
//!
//! - the node: when `search-server` already verified the cluster's own credentials it says so
//!   in [`AUTHENTICATED_HEADER`] (it strips any client-sent copy), and the request runs as the
//!   cluster's owner (`elastic`, a superuser); a request without an `Authorization` header in
//!   dev mode (no node authentication) is the owner too;
//! - `Basic` credentials of a native user ([`users`]), a file-realm user (node setting
//!   `roll.security.file_users`, `name:password:role|role,...`: the test clusters' superuser),
//!   or a reserved user (`elastic` and friends);
//! - `ApiKey` credentials ([`api_keys`]), `Bearer` access tokens and service account tokens
//!   ([`tokens`]).
//!
//! Secrets are never stored: passwords as PBKDF2-HMAC-SHA512 (`{PBKDF2}10000$salt$hash`, one of
//! Elasticsearch's hash formats), API keys and tokens (128 bits of randomness or more) as salted
//! SHA-256. A verified password is cached per cluster by the SHA-256 of the credentials and the
//! stored hash, so only the first request of a user pays for PBKDF2.
//!
//! **Authorization** ([`authorize`]) checks the request's transport action
//! ([`privileges::action_of`]) against the principal's roles: superusers (and the cluster owner)
//! skip it, so requests without credentials cost one header lookup. API keys are limited by
//! their owner's roles at creation (`limited_by`), as in Elasticsearch.

pub mod actions;
pub mod api_keys;
pub mod privileges;
pub mod query;
pub mod tokens;
pub mod users;

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::obj;
use crate::rest::Ctx;
use privileges::{Permission, Scope};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

/// The header `search-server` sets on requests it authenticated with the cluster's own
/// credentials (value: the user name, `elastic` for the owner).
pub const AUTHENTICATED_HEADER: &str = "x-roll-authenticated-user";

/// Narrows a request the node authenticated as the owner to these index names and patterns
/// (comma-separated; `-` exclusions are ignored): roll search's per-index control-plane routes
/// (`/v1/search/clusters/{id}/indices/{index}/…`) send it, so a caller scoped to an index by
/// roll's permissions reaches no other index through the request (a bulk line's `_index`, an
/// expression). It can only take privileges away, so a client sending it gains nothing.
pub const INDEX_SCOPE_HEADER: &str = "x-roll-index-scope";

/// How a principal authenticated.
#[derive(Clone, Debug, PartialEq)]
pub enum AuthKind {
    Realm,
    ApiKey(Box<ApiKeyAuth>),
    Token,
    ServiceAccount { token: String },
}

/// What an API key authentication carries.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiKeyAuth {
    pub id: String,
    pub name: Value,
    pub role_descriptors: Value,
    pub limited_by: Value,
    pub cross_cluster: bool,
}

/// An authenticated user.
#[derive(Clone, Debug, PartialEq)]
pub struct Principal {
    pub username: String,
    pub roles: Vec<String>,
    pub full_name: Value,
    pub email: Value,
    pub metadata: Value,
    pub realm_name: &'static str,
    pub realm_type: &'static str,
    pub kind: AuthKind,
}

impl Principal {
    /// The cluster's owner: the reserved `elastic` superuser.
    pub fn owner() -> Principal {
        Principal {
            username: "elastic".into(),
            roles: vec!["superuser".into()],
            full_name: Value::Null,
            email: Value::Null,
            metadata: obj! { "_reserved" => true },
            realm_name: "reserved",
            realm_type: "reserved",
            kind: AuthKind::Realm,
        }
    }

    /// The owner narrowed to `scope` ([`INDEX_SCOPE_HEADER`]): as an API key of the owner whose
    /// one role grants every index privilege on those names and nothing else.
    pub fn owner_scoped(scope: &str) -> Principal {
        let names: Vec<Value> = scope.split(',').map(str::trim).filter(|n| !n.is_empty() && !n.starts_with('-')).map(Value::from).collect();
        let limited_by = users::reserved_roles().get("superuser").cloned().map(|d| obj! { "superuser" => d }).unwrap_or(Value::Null);
        Principal {
            kind: AuthKind::ApiKey(Box::new(ApiKeyAuth {
                id: "roll-index-scope".into(),
                name: Value::from("roll-index-scope"),
                role_descriptors: obj! { "index_scope" => obj! { "cluster" => Value::Arr(Vec::new()), "indices" => Value::Arr(vec![obj! { "names" => Value::Arr(names), "privileges" => Value::Arr(vec![Value::from("all")]) }]) } },
                limited_by,
                cross_cluster: false,
            })),
            ..Principal::owner()
        }
    }

    /// Whether the principal holds every privilege (superuser role, not narrowed by an API key).
    pub fn is_superuser(&self) -> bool {
        match &self.kind {
            AuthKind::ApiKey(a) => {
                a.role_descriptors.as_object().is_none_or(|m| m.is_empty()) && self.roles.iter().any(|r| r == "superuser")
            }
            _ => self.roles.iter().any(|r| r == "superuser"),
        }
    }

    /// `GET _security/_authenticate`.
    pub fn to_value(&self) -> Value {
        let mut m = crate::json::Map::new();
        m.insert("username".into(), self.username.clone().into());
        let roles = if matches!(self.kind, AuthKind::ApiKey(_)) { Vec::new() } else { self.roles.clone() };
        m.insert("roles".into(), Value::Arr(roles.into_iter().map(Value::from).collect()));
        m.insert("full_name".into(), self.full_name.clone());
        m.insert("email".into(), self.email.clone());
        if matches!(self.kind, AuthKind::Token) {
            m.insert("token".into(), obj! { "managed_by" => "elasticsearch" });
        }
        m.insert("metadata".into(), self.metadata.clone());
        m.insert("enabled".into(), true.into());
        let (name, ty) = match &self.kind {
            AuthKind::ApiKey(_) => ("_es_api_key", "_es_api_key"),
            AuthKind::ServiceAccount { .. } => ("_service_account", "_service_account"),
            _ => (self.realm_name, self.realm_type),
        };
        m.insert("authentication_realm".into(), obj! { "name" => name, "type" => ty });
        m.insert("lookup_realm".into(), obj! { "name" => name, "type" => ty });
        let auth_type = match &self.kind {
            AuthKind::Realm => "realm",
            AuthKind::ApiKey(_) => "api_key",
            AuthKind::Token => "token",
            AuthKind::ServiceAccount { .. } => "token",
        };
        m.insert("authentication_type".into(), auth_type.into());
        match &self.kind {
            AuthKind::ApiKey(a) => {
                m.insert("api_key".into(), obj! { "managed_by" => "elasticsearch", "name" => a.name.clone(), "id" => a.id.clone() });
            }
            AuthKind::ServiceAccount { token } => {
                m.insert("token".into(), obj! { "name" => token.clone(), "type" => "_service_account_index" });
            }
            _ => {}
        }
        Value::Obj(m)
    }

    /// The `user` object Elasticsearch embeds in tokens and profiles.
    pub fn user_value(&self) -> Value {
        obj! {
            "username" => self.username.clone(),
            "roles" => Value::Arr(self.roles.iter().map(Value::from).collect()),
            "full_name" => self.full_name.clone(),
            "email" => self.email.clone(),
            "metadata" => self.metadata.clone(),
            "enabled" => true,
            "authentication_realm" => obj! { "name" => self.realm_name, "type" => self.realm_type },
            "lookup_realm" => obj! { "name" => self.realm_name, "type" => self.realm_type },
            "authentication_type" => "realm",
        }
    }
}

/// The principal of a request (authenticated by [`authenticate`]; the owner if it wasn't).
pub fn principal<'a>(ctx: &'a Ctx) -> &'a Principal {
    static OWNER: OnceLock<Principal> = OnceLock::new();
    ctx.principal.get().unwrap_or_else(|| OWNER.get_or_init(Principal::owner))
}

const WWW_AUTHENTICATE: [&str; 3] = ["Basic realm=\"security\", charset=\"UTF-8\"", "Bearer realm=\"security\"", "ApiKey"];

/// A 401 with Elasticsearch's `WWW-Authenticate` challenges.
pub fn unauthorized(reason: impl Into<String>) -> EsError {
    EsError::security(401, reason).with_header_values("WWW-Authenticate", WWW_AUTHENTICATE.iter().map(|s| s.to_string()).collect())
}

/// The 401 for credentials of an unknown kind of principal ("anonymous access is not allowed").
pub fn unauthorized_credentials(detail: impl Into<String>) -> EsError {
    EsError::security(401, "unable to authenticate with provided credentials and anonymous access is not allowed for this request")
        .with_meta("additional_unsuccessful_credentials", detail.into())
        .with_header_values("WWW-Authenticate", WWW_AUTHENTICATE.iter().map(|s| s.to_string()).collect())
}

/// Authenticates the request and records its principal.
pub fn authenticate(ctx: &Ctx) -> EsResult<()> {
    let req = ctx.request;
    if req.header(AUTHENTICATED_HEADER).is_some() {
        if let Some(scope) = req.header(INDEX_SCOPE_HEADER) {
            let _ = ctx.principal.set(Principal::owner_scoped(scope));
        }
        return Ok(());
    }
    let Some(h) = req.header("authorization") else { return Ok(()) };
    let p = authenticate_header(ctx.cluster, h, &req.uri)?;
    let _ = ctx.principal.set(p);
    Ok(())
}

/// Authenticates an `Authorization` header value (`uri`: for error messages).
pub fn authenticate_header(cluster: &Cluster, header: &str, uri: &str) -> EsResult<Principal> {
    let h = header.trim();
    let scheme_end = h.find(' ').unwrap_or(h.len());
    let (scheme, value) = (&h[..scheme_end], h[scheme_end..].trim());
    if value.is_empty() {
        return Ok(Principal::owner());
    }
    if scheme.eq_ignore_ascii_case("basic") {
        let decoded = b64_decode(value).map_err(|e| {
            unauthorized("invalid basic authentication header encoding").caused_by(EsError::illegal_argument(e))
        })?;
        let text = String::from_utf8_lossy(&decoded).into_owned();
        let Some((user, password)) = text.split_once(':') else {
            return Err(unauthorized("invalid basic authentication header value"));
        };
        return users::authenticate_password(cluster, user, password).ok_or_else(|| unauthorized(format!("unable to authenticate user [{user}] for REST request [{uri}]")));
    }
    if scheme.eq_ignore_ascii_case("apikey") {
        return api_keys::authenticate(cluster, value);
    }
    if scheme.eq_ignore_ascii_case("bearer") {
        return tokens::authenticate_bearer(cluster, value);
    }
    // Other schemes present no credentials Elasticsearch's realms understand.
    Ok(Principal::owner())
}

/// Authenticates credentials a node could not verify against the cluster's own credentials
/// (`search-server` asks before it lets a tenant's native user, API key or token in): native
/// and file-realm users with their passwords, reserved users with a password of their own, API
/// keys, access tokens and service account tokens. Unlike [`authenticate_header`] it never
/// falls back to the owner: no credentials, unknown schemes and a reserved user without a
/// password of its own (which a dev cluster accepts with any password) are `None`.
pub fn authenticate_native(cluster: &Cluster, header: &str) -> Option<Principal> {
    let h = header.trim();
    let (scheme, value) = h.split_once(' ')?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if scheme.eq_ignore_ascii_case("basic") {
        let decoded = b64_decode(value).ok()?;
        let text = String::from_utf8(decoded).ok()?;
        let (user, password) = text.split_once(':')?;
        if users::is_reserved(user) && !users::has_own_password(cluster, user) {
            return None;
        }
        return users::authenticate_password(cluster, user, password);
    }
    if scheme.eq_ignore_ascii_case("apikey") {
        return api_keys::authenticate(cluster, value).ok();
    }
    if scheme.eq_ignore_ascii_case("bearer") {
        return tokens::authenticate_bearer(cluster, value).ok();
    }
    None
}

/// The permissions a principal's roles grant (an API key's: its own descriptors, limited by
/// its owner's).
pub struct Effective {
    pub own: Vec<Permission>,
    pub limited_by: Option<Vec<Permission>>,
}

impl Effective {
    fn allows(&self, f: impl Fn(&Permission) -> bool) -> bool {
        let own = self.own.iter().any(&f);
        match &self.limited_by {
            Some(l) => own && l.iter().any(&f),
            None => own,
        }
    }

    pub fn allows_cluster(&self, action: &str) -> bool {
        self.allows(|p| p.allows_cluster(action))
    }

    pub fn allows_index(&self, action: &str, index: &str) -> bool {
        let restricted = privileges::is_restricted(index);
        self.allows(|p| p.allows_index(action, index, restricted))
    }

    pub fn has_cluster_privilege(&self, privilege: &str) -> bool {
        self.allows(|p| p.has_cluster_privilege(privilege))
    }

    pub fn has_index_privilege(&self, privilege: &str, index: &str) -> bool {
        self.allows(|p| p.has_index_privilege(privilege, index))
    }

    pub fn has_application_privilege(&self, app: &str, privilege: &str, resource: &str) -> bool {
        self.allows(|p| {
            p.applications.iter().any(|(a, privs, res)| {
                privileges::glob(a, app) && privs.iter().any(|x| x == privilege || x == "*" || privileges::glob(x, privilege)) && res.iter().any(|r| privileges::glob(r, resource))
            })
        })
    }
}

/// Role descriptors by name, as `GET _security/role` shows them: native roles, then the reserved
/// ones (file-realm roles are not listed, as in Elasticsearch).
pub fn role_descriptor(st: &crate::meta::ClusterState, name: &str) -> Option<Value> {
    if let Some(r) = st.metadata.xpack.get(users::ROLES, name) {
        return Some(r.clone());
    }
    users::reserved_roles().get(name).cloned()
}

/// The file realm's roles (`roles.yml`), from the node setting `roll.security.file_roles`: the
/// path of a JSON object of role descriptors by name (the test clusters' roles files, converted
/// from YAML). Parsed once per loaded cluster; an unreadable file means no file roles.
pub type FileRoles = std::sync::Arc<std::collections::BTreeMap<String, Value>>;

pub fn file_roles(cluster: &Cluster) -> FileRoles {
    cluster
        .xpack
        .file_roles
        .get_or_init(|| {
            let roles = cluster
                .config
                .node_setting("roll.security.file_roles")
                .and_then(|path| std::fs::read(path).ok())
                .and_then(|bytes| crate::json::parse(&bytes).ok().flatten())
                .and_then(|v| match v {
                    Value::Obj(m) => Some(m.into_iter().collect()),
                    _ => None,
                })
                .unwrap_or_default();
            std::sync::Arc::new(roles)
        })
        .clone()
}

/// The descriptor a role name resolves to when authorizing, in Elasticsearch's
/// `CompositeRolesStore` order: reserved roles, then file roles, then native roles.
pub fn resolve_role(file: &FileRoles, st: &crate::meta::ClusterState, name: &str) -> Option<Value> {
    if let Some(r) = users::reserved_roles().get(name) {
        return Some(r.clone());
    }
    if let Some(r) = file.get(name) {
        return Some(r.clone());
    }
    st.metadata.xpack.get(users::ROLES, name).cloned()
}

fn permissions_of_roles(file: &FileRoles, st: &crate::meta::ClusterState, roles: &[String]) -> Vec<Permission> {
    roles.iter().filter_map(|r| resolve_role(file, st, r)).map(|d| Permission::of_descriptor(&d)).collect()
}

fn permissions_of_descriptors(d: &Value) -> Vec<Permission> {
    d.as_object().map(|m| m.values().map(Permission::of_descriptor).collect()).unwrap_or_default()
}

/// The effective permissions of a principal.
pub fn effective(file: &FileRoles, st: &crate::meta::ClusterState, p: &Principal) -> Effective {
    match &p.kind {
        AuthKind::ApiKey(a) => {
            let (role_descriptors, limited_by) = (&a.role_descriptors, &a.limited_by);
            let limited = permissions_of_descriptors(limited_by);
            if role_descriptors.as_object().is_none_or(|m| m.is_empty()) {
                Effective { own: limited, limited_by: None }
            } else {
                Effective { own: permissions_of_descriptors(role_descriptors), limited_by: Some(limited) }
            }
        }
        AuthKind::ServiceAccount { .. } => {
            let d = tokens::service_account_descriptor(&p.username).unwrap_or(Value::Null);
            Effective { own: vec![Permission::of_descriptor(&d)], limited_by: None }
        }
        _ => Effective { own: permissions_of_roles(file, st, &p.roles), limited_by: None },
    }
}

/// Read APIs whose wildcard index expressions Elasticsearch expands to the indices the user
/// may read (`IndicesAndAliasesResolver`) instead of failing on the others.
const NARROWED: &[&str] = &["search", "count", "field_caps", "async_search.submit", "eql.search", "search_template", "open_point_in_time", "knn_search"];

/// Checks the request's action against its principal's roles, then the indices its queries
/// read besides its targets ([`check_lookups`]). `Ok(Some(index))` asks the caller to run the
/// request on `index` instead: its wildcards expanded to authorized indices.
pub fn authorize(ctx: &Ctx) -> EsResult<Option<String>> {
    let r = authorize_action(ctx)?;
    check_lookups(ctx, None)?;
    Ok(r)
}

/// Documents and indices a query reads besides its targets (a terms lookup, a stored shape or
/// percolated document, more_like_this documents, a lookup runtime field) are read as the
/// user, as Elasticsearch's lookups authorize them: those of the request's body, or of
/// `rendered` (a search template's output, checked once rendered).
pub fn check_lookups(ctx: &Ctx, rendered: Option<&Value>) -> EsResult<()> {
    let Some(p) = ctx.principal.get() else { return Ok(()) };
    if p.is_superuser() {
        return Ok(());
    }
    let reads = match rendered {
        Some(v) => {
            let mut out = Vec::new();
            lookup_walk(v, None, &mut out);
            out
        }
        None => lookup_reads(ctx),
    };
    if reads.is_empty() {
        return Ok(());
    }
    let eff = effective(&file_roles(ctx.cluster), &ctx.state(), p);
    let denied_lookups: Vec<(&'static str, String)> = reads.into_iter().filter(|(a, i)| !eff.allows_index(a, i)).collect();
    if let Some((a, _)) = denied_lookups.first() {
        let on: Vec<String> = denied_lookups.iter().filter(|(x, _)| x == a).map(|(_, i)| i.clone()).collect();
        return Err(denied(p, a, Some(&on)));
    }
    Ok(())
}

fn authorize_action(ctx: &Ctx) -> EsResult<Option<String>> {
    let Some(p) = ctx.principal.get() else { return Ok(None) };
    if p.is_superuser() {
        return Ok(None);
    }
    let (scope, action) = privileges::action_of(ctx.api.name());
    let st = ctx.state();
    let eff = effective(&file_roles(ctx.cluster), &st, p);
    match scope {
        Scope::Any => Ok(None),
        Scope::Cluster => {
            if eff.allows_cluster(&action) {
                return Ok(None);
            }
            // Users may manage their own API keys and change their own password.
            if action.starts_with("cluster:admin/xpack/security/api_key/") && eff.has_cluster_privilege("manage_own_api_key") {
                return Ok(None);
            }
            Err(denied(p, &action, None))
        }
        Scope::Index if NARROWED.contains(&ctx.api.name()) => narrow(ctx, p, &eff, &action),
        Scope::Index => {
            let targets = request_indices(ctx);
            let denied_on: Vec<String> = targets.into_iter().filter(|i| !eff.allows_index(&action, i)).collect();
            if denied_on.is_empty() { Ok(None) } else { Err(denied(p, &action, Some(&denied_on))) }
        }
    }
}

/// Wildcards (and no index at all) expand to the matching indices the principal may access;
/// names given explicitly must be authorized. `-*` (no index) when nothing is left, as
/// Elasticsearch's resolver does.
fn narrow(ctx: &Ctx, p: &Principal, eff: &Effective, action: &str) -> EsResult<Option<String>> {
    let mut exprs = crate::api::index_expressions(ctx);
    let wildcard = |e: &str| e.contains('*') || e == "_all";
    if !exprs.is_empty() && !exprs.iter().any(|e| wildcard(e)) {
        let denied_on: Vec<String> = request_indices(ctx).into_iter().filter(|i| !eff.allows_index(action, i)).collect();
        return if denied_on.is_empty() { Ok(None) } else { Err(denied(p, action, Some(&denied_on))) };
    }
    if exprs.is_empty() {
        exprs.push("*".into());
    }
    let st = ctx.state();
    let opts = ctx.indices_options(crate::meta::resolve::IndicesOptions::LENIENT_EXPAND_OPEN).unwrap_or(crate::meta::resolve::IndicesOptions::LENIENT_EXPAND_OPEN);
    let mut out: Vec<String> = Vec::new();
    let mut denied_on = Vec::new();
    for e in &exprs {
        if e.starts_with('-') {
            continue;
        }
        if wildcard(e) {
            let mut list = vec![e.clone()];
            list.extend(exprs.iter().filter(|x| x.starts_with('-')).cloned());
            for n in crate::meta::resolve::resolve(&st, &list, &opts, ctx.now_ms).unwrap_or_default() {
                if eff.allows_index(action, &n) && !out.contains(&n) {
                    out.push(n);
                }
            }
        } else if eff.allows_index(action, e) {
            out.push(e.clone());
        } else {
            denied_on.push(e.clone());
        }
    }
    if !denied_on.is_empty() {
        return Err(denied(p, action, Some(&denied_on)));
    }
    Ok(Some(if out.is_empty() { "-*".to_string() } else { out.join(",") }))
}

/// The `indices:admin/aliases` check of an alias the request creates on `indices` for its
/// principal (a search application's alias): the indices first, then the alias name.
pub fn check_alias_privileges(ctx: &Ctx, indices: &[String], alias: &str) -> EsResult<()> {
    let Some(p) = ctx.principal.get().filter(|p| !p.is_superuser()) else { return Ok(()) };
    const ACTION: &str = "indices:admin/aliases";
    let eff = effective(&file_roles(ctx.cluster), &ctx.state(), p);
    let denied_on: Vec<String> = indices.iter().filter(|i| !eff.allows_index(ACTION, i)).cloned().collect();
    if !denied_on.is_empty() {
        return Err(denied(p, ACTION, Some(&denied_on)));
    }
    if !eff.allows_index(ACTION, alias) {
        let by = privileges::granted_by(ACTION, false).join(",");
        return Err(EsError::security(403, format!("action [{ACTION}] is unauthorized for {}, this action is granted by the index privileges [{by}]", who(p))));
    }
    Ok(())
}

/// Who a 403 names: the user and its roles, or the API key.
fn who(p: &Principal) -> String {
    match &p.kind {
        AuthKind::ApiKey(a) => format!("API key id [{}] of user [{}]", a.id, p.username),
        _ => {
            let mut roles = p.roles.clone();
            roles.sort();
            format!("user [{}] with effective roles [{}]", p.username, roles.join(","))
        }
    }
}

/// Elasticsearch's 403 for an action the principal's roles don't grant.
fn denied(p: &Principal, action: &str, indices: Option<&[String]>) -> EsError {
    let who = who(p);
    let on = indices.map(|i| format!(" on indices [{}]", i.join(","))).unwrap_or_default();
    let by = privileges::granted_by(action, indices.is_none());
    let grant = if by.is_empty() {
        String::new()
    } else {
        format!(", this action is granted by the {} privileges [{}]", if indices.is_none() { "cluster" } else { "index" }, by.join(","))
    };
    EsError::security(403, format!("action [{action}] is unauthorized for {who}{on}{grant}"))
}

/// The indices a request targets: its `{index}` path part (resolved; names that don't exist
/// yet as given), `_index` of bulk/mget/msearch bodies, or every index when it names none.
fn request_indices(ctx: &Ctx) -> Vec<String> {
    let st = ctx.state();
    let mut exprs: Vec<String> = crate::api::index_expressions(ctx);
    let api = ctx.api.name();
    if matches!(api, "bulk" | "mget" | "msearch" | "msearch_template") {
        exprs.extend(body_indices(ctx));
    }
    // A search application is authorized as an index of its name.
    if api == "search_application.search" {
        exprs.extend(ctx.path_param("name").map(str::to_string));
    }
    if exprs.is_empty() {
        return st.metadata.indices.keys().filter(|k| !k.starts_with('.')).cloned().collect();
    }
    let mut out = Vec::new();
    let opts = crate::meta::resolve::IndicesOptions::LENIENT_EXPAND_OPEN;
    for e in exprs {
        if e.contains('*') || e == "_all" {
            if let Ok(names) = crate::meta::resolve::resolve(&st, std::slice::from_ref(&e), &opts, ctx.now_ms) {
                out.extend(names);
            }
        } else {
            out.push(e.trim_start_matches('-').to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// APIs whose bodies are queries (and runtime mappings), scanned by [`lookup_reads`]. Search
/// templates are checked once rendered ([`check_lookups`]).
const QUERY_APIS: &[&str] = &[
    "search", "msearch", "count", "delete_by_query", "update_by_query", "explain", "indices.validate_query", "async_search.submit", "field_caps",
    "knn_search", "open_point_in_time", "eql.search", "terms_enum", "termvectors", "mtermvectors", "reindex", "rank_eval", "esql.query",
    "esql.async_query", "sql.query", "graph.explore", "search_mvt", "fleet.search", "fleet.msearch", "transform.preview_transform",
    "rollup.rollup_search",
];

/// NDJSON bodies: every line is a header or a query.
const NDJSON_QUERY_APIS: &[&str] = &["msearch", "fleet.msearch"];

/// The indices a request's queries read besides its targets, with the action each read is
/// authorized as (see [`lookup_walk`]). The body is read as the API reads it: any content type,
/// pretty-printed or not, `?source=`; NDJSON line by line.
fn lookup_reads(ctx: &Ctx) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    // Only bodies that hold queries: documents being written may hold any keys.
    if !QUERY_APIS.contains(&ctx.api.name()) {
        return out;
    }
    if NDJSON_QUERY_APIS.contains(&ctx.api.name()) {
        for line in ctx.body.split(|b| *b == b'\n') {
            if let Ok(Some(v)) = crate::json::parse(line) {
                lookup_walk(&v, None, &mut out);
            }
        }
    } else if let Ok(Some(v)) = ctx.body() {
        lookup_walk(&v, None, &mut out);
    }
    out
}

/// Finds the reads of other indices in a query body: `indices:data/read/get` for a terms
/// lookup's document (`index` with `id` and `path`, or OpenSearch's `query`), an
/// `indexed_shape` (index `shapes` unless named), `percolate`'s stored document and
/// `more_like_this`'s `like` / `unlike` documents; `indices:data/read/search` for a `lookup`
/// runtime field's `target_index`. Documents (`document`, `documents`) and script `params` are
/// data, not queries, and aren't searched.
fn lookup_walk(v: &Value, parent: Option<&str>, out: &mut Vec<(&'static str, String)>) {
    const GET: &str = "indices:data/read/get";
    const SEARCH: &str = "indices:data/read/search";
    match v {
        Value::Obj(m) => {
            let s = |k: &str| m.get(k).and_then(Value::as_str);
            if parent == Some("indexed_shape") {
                out.push((GET, s("index").unwrap_or("shapes").to_string()));
            } else if let Some(i) = s("index") {
                let lookup = m.contains_key("path") && (m.contains_key("id") || m.contains_key("query"));
                if lookup || parent == Some("percolate") {
                    out.push((GET, i.to_string()));
                }
            }
            if matches!(parent, Some("like" | "unlike")) && let Some(i) = s("_index") {
                out.push((GET, i.to_string()));
            }
            if s("type") == Some("lookup") && let Some(i) = s("target_index") {
                out.push((SEARCH, i.to_string()));
            }
            for (k, c) in m {
                if !matches!(k.as_str(), "document" | "documents" | "params") {
                    lookup_walk(c, Some(k.as_str()), out);
                }
            }
        }
        // `like: [{...}, "text"]`: the array's documents keep their parent's key.
        Value::Arr(a) => a.iter().for_each(|c| lookup_walk(c, parent, out)),
        _ => {}
    }
}

/// `_index` values of an NDJSON or `docs` body.
fn body_indices(ctx: &Ctx) -> Vec<String> {
    let mut out = Vec::new();
    if ctx.api.name() == "mget" {
        if let Ok(Some(v)) = ctx.body() {
            for d in v.get("docs").and_then(Value::as_array).into_iter().flatten() {
                if let Some(i) = d.get("_index").and_then(Value::as_str) {
                    out.push(i.to_string());
                }
            }
        }
        return out;
    }
    for line in ctx.body.split(|b| *b == b'\n') {
        let Ok(Some(Value::Obj(m))) = crate::json::parse(line) else { continue };
        for (k, v) in &m {
            if let Some(i) = v.get("_index").and_then(Value::as_str) {
                out.push(i.to_string());
            }
            if k == "index" {
                if let Some(s) = v.as_str() {
                    out.push(s.to_string());
                } else if let Some(a) = v.as_array() {
                    out.extend(a.iter().map(Value::to_es_string));
                }
            }
        }
    }
    out
}

// --- hashing ---------------------------------------------------------------------------------

const PBKDF2_ROUNDS: u32 = 10_000;

/// Hashes a password (`{PBKDF2}rounds$salt$hash`, base64).
pub fn hash_password(password: &str) -> String {
    use base64::Engine;
    let salt: [u8; 32] = rand::random();
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha512>(password.as_bytes(), &salt, PBKDF2_ROUNDS, &mut out);
    let b64 = base64::engine::general_purpose::STANDARD;
    format!("{{PBKDF2}}{PBKDF2_ROUNDS}${}${}", b64.encode(salt), b64.encode(out))
}

/// Checks a password against [`hash_password`]'s output, in constant time.
pub fn verify_password(password: &str, stored: &str) -> bool {
    use base64::Engine;
    let Some(rest) = stored.strip_prefix("{PBKDF2}") else { return false };
    let mut parts = rest.split('$');
    let (Some(rounds), Some(salt), Some(hash)) = (parts.next(), parts.next(), parts.next()) else { return false };
    let b64 = base64::engine::general_purpose::STANDARD;
    let (Ok(rounds), Ok(salt), Ok(hash)) = (rounds.parse::<u32>(), b64.decode(salt), b64.decode(hash)) else { return false };
    if rounds == 0 || rounds > 1_000_000 || hash.len() > 64 {
        return false;
    }
    let mut out = vec![0u8; hash.len()];
    pbkdf2::pbkdf2_hmac::<sha2::Sha512>(password.as_bytes(), &salt, rounds, &mut out);
    ct_eq(&out, &hash)
}

/// Salted SHA-256 for high-entropy secrets (API keys, tokens): `{ssha256}salt$hash`.
pub fn hash_secret(secret: &str) -> String {
    use base64::Engine;
    let salt: [u8; 16] = rand::random();
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut h = Sha256::new();
    h.update(salt);
    h.update(secret.as_bytes());
    format!("{{ssha256}}{}${}", b64.encode(salt), b64.encode(h.finalize()))
}

pub fn verify_secret(secret: &str, stored: &str) -> bool {
    use base64::Engine;
    let Some((salt, hash)) = stored.strip_prefix("{ssha256}").and_then(|r| r.split_once('$')) else { return false };
    let b64 = base64::engine::general_purpose::STANDARD;
    let (Ok(salt), Ok(hash)) = (b64.decode(salt), b64.decode(hash)) else { return false };
    let mut h = Sha256::new();
    h.update(&salt);
    h.update(secret.as_bytes());
    ct_eq(&h.finalize(), &hash)
}

/// Constant-time equality (no early exit on the first difference).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b) {
        d |= x ^ y;
    }
    d == 0
}

/// SHA-256 of the parts (cache keys, token lookup keys).
pub fn digest(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    h.finalize().into()
}

/// Standard base64, padded or not (Java's decoder accepts both).
pub fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
    STANDARD.decode(s).or_else(|_| STANDARD_NO_PAD.decode(s)).map_err(|e| e.to_string())
}

/// Random URL-safe base64 (no padding) of `n` bytes.
pub fn random_b64url(n: usize) -> String {
    use base64::Engine;
    let bytes: Vec<u8> = (0..n).map(|_| rand::random::<u8>()).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hashes_verify() {
        let h = hash_password("s3krit-password");
        assert!(h.starts_with("{PBKDF2}10000$"));
        assert!(verify_password("s3krit-password", &h));
        assert!(!verify_password("s3krit-passwore", &h));
        assert!(!verify_password("x", "garbage"));
        let s = hash_secret("abc");
        assert!(verify_secret("abc", &s));
        assert!(!verify_secret("abd", &s));
        assert_ne!(hash_secret("abc"), s);
    }

    #[test]
    fn owner_renders_like_elastic() {
        let v = Principal::owner().to_value();
        assert_eq!(
            v.to_json(),
            r#"{"username":"elastic","roles":["superuser"],"full_name":null,"email":null,"metadata":{"_reserved":true},"enabled":true,"authentication_realm":{"name":"reserved","type":"reserved"},"lookup_realm":{"name":"reserved","type":"reserved"},"authentication_type":"realm"}"#
        );
    }

    #[test]
    fn denied_messages() {
        let mut p = Principal::owner();
        p.username = "u".into();
        p.roles = vec!["b".into(), "a".into()];
        assert_eq!(
            denied(&p, "cluster:monitor/health", None).reason(),
            "action [cluster:monitor/health] is unauthorized for user [u] with effective roles [a,b], this action is granted by the cluster privileges [monitor,manage,all]"
        );
        assert_eq!(
            denied(&p, "indices:data/read/search", Some(&["i".to_string()])).reason(),
            "action [indices:data/read/search] is unauthorized for user [u] with effective roles [a,b] on indices [i], this action is granted by the index privileges [read,all]"
        );
    }
}
