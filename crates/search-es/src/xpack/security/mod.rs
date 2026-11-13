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
pub mod dls;
pub mod opensearch;
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

/// Elasticsearch's run-as header: the request runs as this user once the authenticating
/// user's `run_as` privilege allows it (`xpack.security.authc.run_as.enabled`).
pub const RUN_AS_HEADER: &str = "es-security-runas-user";

/// How a principal authenticated.
#[derive(Clone, Debug, PartialEq)]
pub enum AuthKind {
    Realm,
    ApiKey(Box<ApiKeyAuth>),
    Token,
    ServiceAccount { token: String },
    /// An OpenSearch Security user (the opensearch dialect): its backend roles and attributes.
    OpenSearch(Box<opensearch::Identity>),
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
    /// The user that authenticated, when this principal is the user it runs as
    /// ([`RUN_AS_HEADER`]): its realm is the `authentication_realm`, this one's the
    /// `lookup_realm`.
    pub authenticating: Option<Box<Principal>>,
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
            authenticating: None,
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

    /// The OpenSearch Security user this principal is (the opensearch dialect).
    pub fn os(&self) -> Option<&opensearch::Identity> {
        match &self.kind {
            AuthKind::OpenSearch(id) => Some(id),
            _ => None,
        }
    }

    /// Whether the principal holds every privilege (superuser role, not narrowed by an API key).
    pub fn is_superuser(&self) -> bool {
        match &self.kind {
            AuthKind::ApiKey(a) => {
                a.role_descriptors.as_object().is_none_or(|m| m.is_empty()) && self.roles.iter().any(|r| r == "superuser")
            }
            AuthKind::OpenSearch(id) => id.owner,
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
        let (auth_name, auth_ty) = self.authenticating.as_ref().map_or((name, ty), |a| (a.realm_name, a.realm_type));
        m.insert("authentication_realm".into(), obj! { "name" => auth_name, "type" => auth_ty });
        m.insert("lookup_realm".into(), obj! { "name" => name, "type" => ty });
        let auth_type = match &self.kind {
            AuthKind::Realm | AuthKind::OpenSearch(_) => "realm",
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
            "authentication_realm" => self.authenticating.as_ref().map_or_else(|| obj! { "name" => self.realm_name, "type" => self.realm_type }, |a| obj! { "name" => a.realm_name, "type" => a.realm_type }),
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
    let p = if req.header(AUTHENTICATED_HEADER).is_some() {
        req.header(INDEX_SCOPE_HEADER).map(Principal::owner_scoped)
    } else {
        match req.header("authorization") {
            Some(h) => Some(authenticate_header(ctx.cluster, h, &req.uri)?),
            None => None,
        }
    };
    let p = match req.header(RUN_AS_HEADER) {
        Some(target) if ctx.cluster.dialect == crate::cluster::Dialect::Elasticsearch && run_as_enabled(ctx.cluster) => {
            Some(run_as(ctx, p.unwrap_or_else(Principal::owner), target)?)
        }
        _ => p,
    };
    if let Some(p) = p {
        let _ = ctx.principal.set(p);
    }
    Ok(())
}

fn run_as_enabled(cluster: &Cluster) -> bool {
    cluster.config.node_setting("xpack.security.authc.run_as.enabled").is_none_or(|v| v != "false")
}

/// The user `authn` runs the request as (`AuthenticatorChain.lookupRunAsUser`, then
/// `AuthorizationService.authorizeRunAs`): a user the realms know, enabled, that a `run_as`
/// pattern of `authn`'s roles names. API keys and service accounts can't run as anyone; their
/// header is ignored, as Elasticsearch does.
fn run_as(ctx: &Ctx, authn: Principal, target: &str) -> EsResult<Principal> {
    if !matches!(authn.kind, AuthKind::Realm | AuthKind::Token) {
        return Ok(authn);
    }
    let allowed = || effective(&file_roles(ctx.cluster), &ctx.state(), &authn).allows_run_as(target);
    match users::lookup_user(ctx.cluster, target).filter(|_| allowed()) {
        Some(mut user) => {
            user.kind = authn.kind.clone();
            user.authenticating = Some(Box::new(authn));
            Ok(user)
        }
        None => Err(run_as_denied(&authn, &privileges::action_of(ctx.api.name()).1, target)),
    }
}

/// Elasticsearch's 403 for a user its `run_as` privilege doesn't allow (or that doesn't exist).
pub(crate) fn run_as_denied(authn: &Principal, action: &str, target: &str) -> EsError {
    EsError::security(403, format!("action [{action}] is unauthorized for {}, because user [{}] is unauthorized to run as [{target}]", who(authn), authn.username))
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
            if cluster.dialect == crate::cluster::Dialect::OpenSearch {
                return opensearch::unauthorized();
            }
            unauthorized("invalid basic authentication header encoding").caused_by(EsError::illegal_argument(e))
        })?;
        let text = String::from_utf8_lossy(&decoded).into_owned();
        let Some((user, password)) = text.split_once(':') else {
            return Err(unauthorized("invalid basic authentication header value"));
        };
        if cluster.dialect == crate::cluster::Dialect::OpenSearch {
            return opensearch::authenticate_password(cluster, user, password, false).ok_or_else(opensearch::unauthorized);
        }
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
        if cluster.dialect == crate::cluster::Dialect::OpenSearch {
            return opensearch::authenticate_password(cluster, user, password, true);
        }
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
    /// Backing indices by name, with their data streams: a backing index is authorized by
    /// what its data stream is (`IndicesPermission`'s `IndexResource`). Failure store indices
    /// are not: they need the failure store's own privileges.
    pub parents: std::collections::HashMap<String, String>,
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
        let parent = self.parents.get(index);
        self.allows(|p| p.allows_index(action, index, restricted) || parent.is_some_and(|ds| p.allows_index(action, ds, privileges::is_restricted(ds))))
    }

    /// Whether some role grants `action` on some index (`IndicesPermission.check(action)`):
    /// how Elasticsearch authorizes the request of a composite action, whose items are then
    /// authorized one by one.
    pub fn allows_index_action(&self, action: &str) -> bool {
        self.allows(|p| p.indices.iter().any(|g| g.privileges.iter().any(|x| privileges::index_grants(x, action))))
    }

    /// Whether a `run_as` pattern names `user` (`RunAsPermission`).
    pub fn allows_run_as(&self, user: &str) -> bool {
        self.allows(|p| p.run_as.iter().any(|r| privileges::glob(r, user)))
    }

    pub fn has_cluster_privilege(&self, privilege: &str) -> bool {
        self.allows(|p| p.has_cluster_privilege(privilege))
    }

    pub fn has_index_privilege(&self, privilege: &str, index: &str) -> bool {
        self.allows(|p| p.has_index_privilege(privilege, index))
    }

    /// `ApplicationPermission.grants`: a requested privilege is the actions of the application's
    /// privilege of that name (or itself, when it is an action), granted when the actions the
    /// role's privileges stand for cover them, on resources covering the requested ones.
    pub fn has_application_privilege(&self, st: &crate::meta::ClusterState, app: &str, privilege: &str, resource: &str) -> bool {
        let actions_of = |name: &str| -> Vec<String> {
            match st.metadata.xpack.get(users::PRIVILEGES, &users::privilege_key(app, name)) {
                Some(d) => d.get("actions").and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default(),
                None if name.contains('/') || name.contains(':') || name.contains('*') => vec![name.to_string()],
                None => Vec::new(),
            }
        };
        let wanted = actions_of(privilege);
        self.allows(|p| {
            p.applications.iter().any(|(a, privs, res)| {
                let by_name = privs.iter().any(|x| x == privilege || x == "*" || privileges::glob(x, privilege));
                let by_actions = !wanted.is_empty() && {
                    let granted: Vec<String> = privs.iter().flat_map(|x| if x == "*" { vec!["*".to_string()] } else { actions_of(x) }).collect();
                    wanted.iter().all(|w| granted.iter().any(|g| privileges::glob(g, w)))
                };
                privileges::glob(a, app) && (by_name || by_actions) && res.iter().any(|r| privileges::glob(r, resource))
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
    let (own, limited_by) = match &p.kind {
        AuthKind::ApiKey(a) => {
            let (role_descriptors, limited_by) = (&a.role_descriptors, &a.limited_by);
            let limited = permissions_of_descriptors(limited_by);
            if role_descriptors.as_object().is_none_or(|m| m.is_empty()) {
                (limited, None)
            } else {
                (permissions_of_descriptors(role_descriptors), Some(limited))
            }
        }
        AuthKind::ServiceAccount { .. } => {
            let d = tokens::service_account_descriptor(&p.username).unwrap_or(Value::Null);
            (vec![Permission::of_descriptor(&d)], None)
        }
        AuthKind::OpenSearch(id) => (opensearch::permissions(st, p, id), None),
        _ => (permissions_of_roles(file, st, &p.roles), None),
    };
    let parents = st.metadata.data_streams.values().flat_map(|d| d.indices.iter().map(|i| (i.clone(), d.name.clone()))).collect();
    Effective { own, limited_by, parents }
}

/// Read APIs whose wildcard index expressions Elasticsearch expands to the indices the user
/// may read (`IndicesAndAliasesResolver`) instead of failing on the others.
const NARROWED: &[&str] = &["search", "count", "field_caps", "async_search.submit", "eql.search", "search_template", "open_point_in_time", "knn_search", "indices.get", "cat.indices"];

/// The indices options [`narrow`] expands an API's wildcards with (before its parameters).
fn narrow_defaults(api: &str) -> crate::meta::resolve::IndicesOptions {
    use crate::meta::resolve::IndicesOptions;
    match api {
        "indices.get" => IndicesOptions::STRICT_EXPAND_OPEN,
        "cat.indices" => IndicesOptions::STRICT_EXPAND,
        _ => IndicesOptions::LENIENT_EXPAND_OPEN,
    }
}

/// Data stream APIs that list what their wildcards match: they list the data streams the
/// principal may see ([`retain_authorized`]); names given explicitly must be authorized.
const DATA_STREAM_LISTS: &[&str] = &["indices.get_data_stream", "indices.data_streams_stats"];

/// Keeps the names (data streams) the request's principal may act on with its action.
pub fn retain_authorized(ctx: &Ctx, names: &mut Vec<String>) {
    let visible = authorized_names(ctx);
    names.retain(|n| visible(n));
}

/// Whether the request's principal may act on a name (index, alias, data stream) with the
/// request's action: what a listing whose wildcards Elasticsearch expands to authorized names
/// shows.
pub fn authorized_names(ctx: &Ctx) -> impl Fn(&str) -> bool {
    let check = ctx.principal.get().filter(|p| !p.is_superuser()).map(|p| (effective(&file_roles(ctx.cluster), &ctx.state(), p), privileges::action_of(ctx.api.name()).1));
    move |n| check.as_ref().is_none_or(|(eff, action)| eff.allows_index(action, n))
}

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

/// A write an ingest pipeline moved to `index` (`_index`, `reroute`): authorized there as an
/// index write, as Elasticsearch authorizes each bulk item at the index it finally goes to.
pub fn check_moved_write(ctx: &Ctx, index: &str) -> EsResult<()> {
    const ACTION: &str = "indices:data/write/index";
    let Some(p) = ctx.principal.get() else { return Ok(()) };
    if p.is_superuser() {
        return Ok(());
    }
    let eff = effective(&file_roles(ctx.cluster), &ctx.state(), p);
    if eff.allows_index(ACTION, index) { Ok(()) } else { Err(denied(p, ACTION, Some(&[index.to_string()]))) }
}

fn authorize_action(ctx: &Ctx) -> EsResult<Option<String>> {
    let Some(p) = ctx.principal.get() else { return Ok(None) };
    if p.is_superuser() {
        return Ok(None);
    }
    let (scope, action) = privileges::action_of(ctx.api.name());
    let st = ctx.state();
    let eff = effective(&file_roles(ctx.cluster), &st, p);
    // A user changes its own password when a native or reserved realm authenticated it (not a
    // token or an API key: `RBACEngine.checkChangePasswordAction`); any other needs the action.
    if ctx.api.name() == "security.change_password" {
        const CHANGE: &str = "cluster:admin/xpack/security/user/change_password";
        let own = ctx.path_param("username").is_none_or(|u| u == p.username);
        if own && p.kind == AuthKind::Realm && matches!(p.realm_type, "native" | "reserved") || eff.allows_cluster(CHANGE) {
            return Ok(None);
        }
        return Err(denied(p, CHANGE, None));
    }
    if p.os().is_some() {
        // The plugin checks composite requests, scrolls and reindex as cluster actions (their
        // items are then checked on their indices), and expands no wildcards to the permitted
        // indices.
        if opensearch::is_cluster_action(&action) && !ctx.api.name().starts_with("plugins_security.") {
            if !eff.allows_cluster(&action) {
                return Err(denied(p, &action, None));
            }
            if scope != Scope::Cluster {
                return Ok(None);
            }
        }
        // A single document write: its action on its index, and `indices:admin/create` when
        // it creates the index.
        if matches!(ctx.api.name(), "index" | "create" | "delete" | "update") {
            let name = crate::meta::resolve::resolve_date_math(ctx.path_param("index").unwrap_or_default(), ctx.now_ms)?;
            let creates = ctx.api.name() != "delete" && crate::write::resolve_existing(&st, &name).is_none() && matches!(crate::write::write_index_name(&st, &name), Ok(None));
            if creates && !eff.allows_index("indices:admin/create", &name) {
                return Err(denied(p, "indices:admin/create", None));
            }
            let action = if ctx.api.name() == "create" { "indices:data/write/index" } else { action.as_str() };
            return if eff.allows_index(action, &name) { Ok(None) } else { Err(denied(p, action, None)) };
        }
        if scope == Scope::Index && NARROWED.contains(&ctx.api.name()) {
            let defaults = narrow_defaults(ctx.api.name());
            let opts = ctx.indices_options(defaults).unwrap_or(defaults);
            return check_all(ctx, p, &eff, &action, crate::api::index_expressions(ctx), &opts).map(|_| None);
        }
    }
    match scope {
        Scope::Any => Ok(None),
        Scope::Cluster => {
            if eff.allows_cluster(&action) {
                return Ok(None);
            }
            // `global.application.manage`: the privileges of the applications it names
            // (`ManageApplicationPrivileges`), every application the request names matching.
            if action.starts_with("cluster:admin/xpack/security/privilege/") && !action.contains("/builtin/") {
                let apps = users::privilege_request_applications(ctx);
                let manages = |perm: &Permission| {
                    let patterns = perm.global.as_ref().map(users::managed_applications).unwrap_or_default();
                    !apps.is_empty() && apps.iter().all(|a| patterns.iter().any(|p| privileges::glob(p, a)))
                };
                if eff.allows(manages) {
                    return Ok(None);
                }
            }
            // Users may manage their own API keys (`ManageOwnApiKeyClusterPrivilege`).
            if action.starts_with("cluster:admin/xpack/security/api_key/") && eff.has_cluster_privilege("manage_own_api_key") && api_keys::own_keys_request(ctx, p) {
                return Ok(None);
            }
            Err(denied(p, &action, None))
        }
        Scope::Index if NARROWED.contains(&ctx.api.name()) => narrow(ctx, p, &eff, &action),
        // Composite requests: their items are authorized as they run (`authorize_writes`,
        // `authorize_gets`, each search of an msearch).
        Scope::Index if COMPOSITE.contains(&ctx.api.name()) => {
            if eff.allows_index_action(&action) { Ok(None) } else { Err(denied(p, &action, None)) }
        }
        // A single document write runs as a one-item bulk request: its action by name, then
        // the item's checks.
        Scope::Index if matches!(ctx.api.name(), "index" | "create" | "delete") => {
            let name = crate::meta::resolve::resolve_date_math(ctx.path_param("index").unwrap_or_default(), ctx.now_ms)?;
            if !eff.allows_index_action(&action) {
                return Err(denied(p, &action, Some(std::slice::from_ref(&name))));
            }
            let op = match ctx.api.name() {
                "delete" => WriteOp::Delete,
                "create" => WriteOp::Create,
                _ if ctx.query("op_type") == Some("create") || (ctx.path_param("id").is_none() && ctx.query("op_type").is_none()) => WriteOp::Create,
                _ => WriteOp::Index,
            };
            let st = ctx.state();
            let item = WriteItem::new(&st, name, op, ctx.path_param("id").unwrap_or_default(), ctx.query("routing"));
            match check_writes(p, &eff, std::slice::from_ref(&item)).pop().flatten() {
                Some(e) => Err(e),
                None => Ok(None),
            }
        }
        // Its wildcards list what the principal may see (`authorized_names`); a role must grant
        // the action on some index.
        Scope::Index if ctx.api.name() == "indices.resolve_index" && !eff.allows_index_action(&action) => {
            Err(denied(p, &action, Some(&target_expressions(ctx))))
        }
        Scope::Index if DATA_STREAM_LISTS.contains(&ctx.api.name()) || ctx.api.name() == "indices.resolve_index" => {
            let explicit = target_expressions(ctx).into_iter().filter(|e| !e.contains('*') && e != "_all" && !e.starts_with('-'));
            let denied_on: Vec<String> = explicit.filter(|n| !eff.allows_index(&action, n)).collect();
            if denied_on.is_empty() { Ok(None) } else { Err(denied(p, &action, Some(&denied_on))) }
        }
        Scope::Index => {
            let targets = request_indices(ctx);
            let denied_on: Vec<String> = targets.into_iter().filter(|i| !eff.allows_index(&action, i)).collect();
            if denied_on.is_empty() { Ok(None) } else { Err(denied(p, &action, Some(&denied_on))) }
        }
    }
}

/// The `indices:admin/refresh` an OpenSearch user's `refresh=true` by-query request runs as the
/// user once its documents are written (the plugin denies it then, after the writes).
pub fn check_refresh(ctx: &Ctx) -> EsResult<()> {
    const REFRESH: &str = "indices:admin/refresh";
    let Some(p) = ctx.principal.get().filter(|p| !p.is_superuser() && p.os().is_some()) else { return Ok(()) };
    let eff = effective(&file_roles(ctx.cluster), &ctx.state(), p);
    if request_indices(ctx).iter().all(|i| eff.allows_index(REFRESH, i)) { Ok(()) } else { Err(denied(p, REFRESH, None)) }
}

/// APIs Elasticsearch authorizes by action name only (`RBACEngine.shouldAuthorizeIndexActionNameOnly`):
/// the request when any role grants its action on some index, its items one by one.
const COMPOSITE: &[&str] = &["bulk", "mget", "msearch", "msearch_template"];

/// What a bulk item (or a single document request) writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteOp {
    Index,
    Create,
    Update,
    Delete,
}

impl WriteOp {
    /// The action Elasticsearch authorizes the item as (`BulkShardRequest` items).
    fn action(self) -> &'static str {
        match self {
            WriteOp::Index => "indices:data/write/index:op_type/index",
            WriteOp::Create => "indices:data/write/index:op_type/create",
            WriteOp::Update => "indices:data/write/update",
            WriteOp::Delete => "indices:data/write/delete",
        }
    }

    /// The item's action without Elasticsearch's `:op_type/…` suffix (OpenSearch's name).
    fn base_action(self) -> &'static str {
        match self {
            WriteOp::Index | WriteOp::Create => "indices:data/write/index",
            WriteOp::Update => "indices:data/write/update",
            WriteOp::Delete => "indices:data/write/delete",
        }
    }
}

/// One write of a bulk request, as its authorization sees it.
#[derive(Clone, Debug)]
pub struct WriteItem {
    /// The item's index, alias or data stream (date math resolved).
    pub name: String,
    pub op: WriteOp,
    /// Whether nothing by that name exists, so the write would create the index.
    pub creates: bool,
    /// The shard it lands on: the writes of one shard are one shard request.
    pub shard: (String, u32),
}

impl WriteItem {
    pub fn new(st: &crate::meta::ClusterState, name: String, op: WriteOp, id: &str, routing: Option<&str>) -> WriteItem {
        let (creates, shard) = match crate::write::resolve_existing(st, &name) {
            Some(idx) => (false, (idx.name.clone(), crate::meta::routing::shard_for(&idx, id, routing))),
            None => (op != WriteOp::Delete && matches!(crate::write::write_index_name(st, &name), Ok(None)), (name.clone(), 0)),
        };
        WriteItem { name, op, creates, shard }
    }
}

/// The `indices:admin/mapping/auto_put` check of a write that updates its index's mapping
/// (`IndicesPermission`): granted by a privilege that grants the action, or, on an index that
/// isn't a data stream's, by `index`, `create`, `create_doc` or `write`, with Elasticsearch's
/// deprecation warning.
pub fn check_mapping_update(ctx: &Ctx, idx: &crate::meta::IndexMeta) -> EsResult<()> {
    const AUTO_PUT: &str = "indices:admin/mapping/auto_put";
    const BWC: &[&str] = &["index", "create", "create_doc", "write"];
    fn bwc<'a>(perm: &'a Permission, index: &str, restricted: bool) -> Option<&'a privileges::IndexGrant> {
        perm.indices.iter().find(|g| {
            (!restricted || g.allow_restricted) && g.names.iter().any(|n| privileges::index_pattern_matches(n, index)) && g.privileges.iter().any(|x| BWC.contains(&x.as_str()))
        })
    }
    // OpenSearch updates mappings for a write as the node, without checking the user.
    let Some(p) = ctx.principal.get().filter(|p| !p.is_superuser() && p.os().is_none()) else { return Ok(()) };
    let st = ctx.state();
    let eff = effective(&file_roles(ctx.cluster), &st, p);
    let in_data_stream = st.metadata.data_stream_of(&idx.name).is_some();
    // The concrete index, else an alias the write may have gone through: one that points at
    // this index only, or has it as its write index (`getPutMappingIndexOrAlias`).
    let aliases = st.aliases();
    let through: Vec<&str> = aliases
        .iter()
        .filter(|(_, members)| members.iter().any(|(i, a)| i.name == idx.name && (members.len() == 1 || a.is_write_index == Some(true))))
        .map(|(n, _)| *n)
        .collect();
    for name in std::iter::once(idx.name.as_str()).chain(through) {
        if eff.allows_index(AUTO_PUT, name) {
            return Ok(());
        }
        if in_data_stream {
            continue;
        }
        let restricted = privileges::is_restricted(name);
        if eff.allows(|perm| perm.allows_index(AUTO_PUT, name, restricted) || bwc(perm, name, restricted).is_some()) {
            let grant = eff.own.iter().chain(eff.limited_by.iter().flatten()).find_map(|perm| (!perm.allows_index(AUTO_PUT, name, restricted)).then(|| bwc(perm, name, restricted)).flatten());
            if let Some(g) = grant {
                ctx.warn(format!(
                    "the index privilege [{}] allowed the update mapping action [{AUTO_PUT}] on index [{name}], this privilege will not permit mapping updates in the next major release - users who require access to update mappings must be granted explicit privileges",
                    g.privileges.join(","),
                ));
            }
            return Ok(());
        }
    }
    Err(denied(p, AUTO_PUT, None))
}

/// Whether a composite request's items are authorized one by one (a principal that doesn't
/// skip authorization).
pub fn checks_items(ctx: &Ctx) -> bool {
    ctx.principal.get().is_some_and(|p| !p.is_superuser())
}

/// Elasticsearch's checks of the writes of a request whose own action was authorized by name
/// (`TransportBulkAction`, `AuthorizationService.authorizeBulkItems`), in its order:
/// `indices:admin/auto_create` on an index a write creates; `indices:data/write/bulk[s]` on
/// every name written to a shard, where one denied name fails all of the shard's writes; then
/// each write's own action on its name. The error of each denied item, `None` for the others.
pub fn authorize_writes(ctx: &Ctx, items: &[WriteItem]) -> Vec<Option<EsError>> {
    match ctx.principal.get().filter(|p| !p.is_superuser()) {
        Some(p) => check_writes(p, &effective(&file_roles(ctx.cluster), &ctx.state(), p), items),
        None => vec![None; items.len()],
    }
}

fn check_writes(p: &Principal, eff: &Effective, items: &[WriteItem]) -> Vec<Option<EsError>> {
    const AUTO_CREATE: &str = "indices:admin/auto_create";
    const SHARD: &str = "indices:data/write/bulk[s]";
    // OpenSearch: an index a write creates needs `indices:admin/create`; then the shard action
    // and the item's own action, both named in one 403.
    if let Some(id) = p.os() {
        return items
            .iter()
            .map(|it| {
                if it.creates && !eff.allows_index("indices:admin/create", &it.name) {
                    return Some(opensearch::denied(p, id, "indices:admin/create"));
                }
                let missing: Vec<&str> = [SHARD, it.op.base_action()].into_iter().filter(|a| !eff.allows_index(a, &it.name)).collect();
                (!missing.is_empty()).then(|| opensearch::denied(p, id, &missing.join(", ")))
            })
            .collect();
    }
    let mut out: Vec<Option<EsError>> = items
        .iter()
        .map(|it| (it.creates && !eff.allows_index(AUTO_CREATE, &it.name)).then(|| denied(p, AUTO_CREATE, Some(std::slice::from_ref(&it.name)))))
        .collect();
    for (shard, denied_on) in denied_shards(eff, SHARD, items.iter().enumerate().filter(|(i, _)| out[*i].is_none()).map(|(i, it)| (i, &it.shard, &it.name))) {
        for (i, it) in items.iter().enumerate() {
            if out[i].is_none() && it.shard == *shard {
                out[i] = Some(denied(p, SHARD, Some(&denied_on)));
            }
        }
    }
    for (i, it) in items.iter().enumerate() {
        if out[i].is_none() && !eff.allows_index(it.op.action(), &it.name) {
            out[i] = Some(denied(p, it.op.action(), Some(std::slice::from_ref(&it.name))));
        }
    }
    out
}

/// The shards one of whose names `action` is denied on, with those names (sorted): a shard
/// request names every index, alias or data stream its items were sent to.
fn denied_shards<'a>(eff: &Effective, action: &str, items: impl Iterator<Item = (usize, &'a (String, u32), &'a String)>) -> Vec<(&'a (String, u32), Vec<String>)> {
    let mut shards: Vec<(&(String, u32), Vec<String>)> = Vec::new();
    for (_, shard, name) in items {
        let names = match shards.iter_mut().find(|(s, _)| *s == shard) {
            Some((_, n)) => n,
            None => {
                shards.push((shard, Vec::new()));
                &mut shards.last_mut().expect("pushed").1
            }
        };
        if !eff.allows_index(action, name) && !names.contains(name) {
            names.push(name.clone());
        }
    }
    shards.retain(|(_, n)| !n.is_empty());
    shards.iter_mut().for_each(|(_, n)| n.sort());
    shards
}

/// The `indices:data/read/mget[shard]` checks of a multi get's documents (`(name, shard)`,
/// names as requested): one denied name fails every document of its shard.
pub fn authorize_gets(ctx: &Ctx, docs: &[(String, (String, u32))]) -> Vec<Option<EsError>> {
    const SHARD: &str = "indices:data/read/mget[shard]";
    let Some(p) = ctx.principal.get().filter(|p| !p.is_superuser()) else { return vec![None; docs.len()] };
    let eff = effective(&file_roles(ctx.cluster), &ctx.state(), p);
    let mut out = vec![None; docs.len()];
    for (shard, denied_on) in denied_shards(&eff, SHARD, docs.iter().enumerate().map(|(i, (n, s))| (i, s, n))) {
        for (i, (_, s)) in docs.iter().enumerate() {
            if s == shard {
                out[i] = Some(denied(p, SHARD, Some(&denied_on)));
            }
        }
    }
    out
}

/// One search of a multi search, authorized as a search request ([`narrow`]): its indices,
/// wildcards expanded to the ones the principal may read (`None`: unchanged).
pub fn authorize_search(ctx: &Ctx, exprs: &[String], opts: &crate::meta::resolve::IndicesOptions) -> EsResult<Option<Vec<String>>> {
    const SEARCH: &str = "indices:data/read/search";
    let Some(p) = ctx.principal.get().filter(|p| !p.is_superuser()) else { return Ok(None) };
    let eff = effective(&file_roles(ctx.cluster), &ctx.state(), p);
    if p.os().is_some() {
        return check_all(ctx, p, &eff, SEARCH, exprs.to_vec(), opts).map(|_| None);
    }
    narrow_exprs(ctx, p, &eff, SEARCH, exprs.to_vec(), opts, opts.ignore_unavailable)
}

/// OpenSearch's check of an index request (`do_not_fail_on_forbidden` off): every index its
/// expressions resolve to (or every index when it names none) must be permitted; nothing is
/// narrowed to the permitted ones.
fn check_all(ctx: &Ctx, p: &Principal, eff: &Effective, action: &str, exprs: Vec<String>, opts: &crate::meta::resolve::IndicesOptions) -> EsResult<()> {
    let st = ctx.state();
    let mut names: Vec<String> = Vec::new();
    // No index: every index the request reads, as its wildcard resolves (dot-prefixed ones too).
    if exprs.iter().all(|e| e.starts_with('-')) {
        let list: Vec<String> = std::iter::once("*".to_string()).chain(exprs.iter().cloned()).collect();
        names.extend(crate::meta::resolve::resolve(&st, &list, opts, ctx.now_ms).unwrap_or_default());
    }
    for e in exprs.iter().filter(|e| !e.starts_with('-')) {
        if e.contains('*') || e == "_all" {
            let mut list = vec![e.clone()];
            list.extend(exprs.iter().filter(|x| x.starts_with('-')).cloned());
            names.extend(crate::meta::resolve::resolve(&st, &list, opts, ctx.now_ms).unwrap_or_default());
        } else {
            names.push(e.clone());
        }
    }
    if names.iter().all(|n| eff.allows_index(action, n)) { Ok(()) } else { Err(denied(p, action, None)) }
}

/// Wildcards (and no index at all) expand to the matching indices the principal may access;
/// names given explicitly must be authorized, unless `ignore_unavailable` drops the ones that
/// aren't (`IndexAbstractionResolver`). `-*` (no index) when nothing is left, as
/// Elasticsearch's resolver does.
fn narrow(ctx: &Ctx, p: &Principal, eff: &Effective, action: &str) -> EsResult<Option<String>> {
    use crate::meta::resolve::IndicesOptions;
    let defaults = narrow_defaults(ctx.api.name());
    let opts = ctx.indices_options(defaults).unwrap_or(defaults);
    // `ignore_unavailable` as the API takes it (only EQL's default is lenient).
    let own = if ctx.api.name() == "eql.search" { IndicesOptions::LENIENT_EXPAND_OPEN } else { IndicesOptions::STRICT_EXPAND_OPEN };
    let ignore_unavailable = ctx.indices_options(own).map(|o| o.ignore_unavailable).unwrap_or(false);
    Ok(narrow_exprs(ctx, p, eff, action, crate::api::index_expressions(ctx), &opts, ignore_unavailable)?.map(|v| v.join(",")))
}

fn narrow_exprs(
    ctx: &Ctx,
    p: &Principal,
    eff: &Effective,
    action: &str,
    mut exprs: Vec<String>,
    opts: &crate::meta::resolve::IndicesOptions,
    ignore_unavailable: bool,
) -> EsResult<Option<Vec<String>>> {
    // ES|QL and SQL resolve the indices their queries name strictly (`IndexResolver`): the
    // reads they make for their client (`internal::as_caller`) skip missing indices, never
    // ones the client may not read.
    let ignore_unavailable = ignore_unavailable && crate::xpack::internal::caller().is_none();
    let wildcard = |e: &str| e.contains('*') || e == "_all";
    if !exprs.is_empty() && !exprs.iter().any(|e| wildcard(e)) {
        let mut names: Vec<String> = exprs.iter().map(|e| e.trim_start_matches('-').to_string()).collect();
        names.sort();
        names.dedup();
        let denied_on: Vec<String> = names.into_iter().filter(|i| !eff.allows_index(action, i)).collect();
        if denied_on.is_empty() {
            return Ok(None);
        }
        if !ignore_unavailable {
            return Err(denied(p, action, Some(&denied_on)));
        }
    }
    if exprs.is_empty() {
        exprs.push("*".into());
    }
    let st = ctx.state();
    let mut out: Vec<String> = Vec::new();
    let mut denied_on = Vec::new();
    for e in &exprs {
        if e.starts_with('-') {
            continue;
        }
        if wildcard(e) {
            let mut list = vec![e.clone()];
            list.extend(exprs.iter().filter(|x| x.starts_with('-')).cloned());
            for n in crate::meta::resolve::resolve(&st, &list, opts, ctx.now_ms).unwrap_or_default() {
                if eff.allows_index(action, &n) && !out.contains(&n) {
                    out.push(n);
                }
            }
        } else if eff.allows_index(action, e) {
            out.push(e.clone());
        } else if !ignore_unavailable {
            denied_on.push(e.clone());
        }
    }
    if !denied_on.is_empty() {
        return Err(denied(p, action, Some(&denied_on)));
    }
    Ok(Some(if out.is_empty() { vec!["-*".to_string()] } else { out }))
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

/// The properties `set_security_user` records.
pub const INGEST_USER_PROPERTIES: &[&str] = &["username", "roles", "full_name", "email", "metadata", "api_key", "realm", "authentication_type"];

/// The writing user as `set_security_user` records it, limited to `properties`: absent values
/// (no roles, no full name, empty metadata) are left out; an API key's user is its owner, with
/// the owner's realm and the key's name, id and metadata.
pub fn ingest_user(cluster: &Cluster, p: &Principal, properties: &[String]) -> crate::json::Map {
    let st = cluster.state();
    let key = match &p.kind {
        AuthKind::ApiKey(a) => st.metadata.xpack.get(api_keys::KEYS, &a.id).cloned().map(|k| (a.clone(), k)),
        _ => None,
    };
    let mut out = crate::json::Map::new();
    for prop in properties {
        let v = match prop.as_str() {
            "username" => Some(Value::from(p.username.clone())),
            "roles" => (!p.roles.is_empty()).then(|| Value::Arr(p.roles.iter().map(Value::from).collect())),
            "full_name" => (!p.full_name.is_null()).then(|| p.full_name.clone()),
            "email" => (!p.email.is_null()).then(|| p.email.clone()),
            "metadata" => p.metadata.as_object().filter(|m| !m.is_empty()).map(|_| p.metadata.clone()),
            "api_key" => key.as_ref().map(|(a, k)| {
                let mut m = crate::json::Map::new();
                m.insert("name".into(), a.name.clone());
                m.insert("id".into(), Value::from(a.id.clone()));
                if let Some(md) = k.get("metadata").filter(|v| v.as_object().is_some_and(|m| !m.is_empty())) {
                    m.insert("metadata".into(), md.clone());
                }
                Value::Obj(crate::json::java_hash_map(m, 16))
            }),
            "realm" => Some(match &key {
                Some((_, k)) => obj! { "name" => k.get("realm").cloned().unwrap_or(Value::Null), "type" => k.get("realm_type").cloned().unwrap_or(Value::Null) },
                None => obj! { "name" => p.realm_name, "type" => p.realm_type },
            }),
            "authentication_type" => Some(Value::from(match p.kind {
                AuthKind::Realm | AuthKind::OpenSearch(_) => "REALM",
                AuthKind::ApiKey(_) => "API_KEY",
                AuthKind::Token | AuthKind::ServiceAccount { .. } => "TOKEN",
            })),
            _ => None,
        };
        if let Some(v) = v {
            out.insert(prop.clone(), v);
        }
    }
    out
}

/// Who a 403 names: the user and its roles, or the API key.
pub(crate) fn who(p: &Principal) -> String {
    match &p.kind {
        AuthKind::ApiKey(a) => format!("API key id [{}] of user [{}]", a.id, p.username),
        AuthKind::ServiceAccount { .. } => format!("service account [{}]", p.username),
        _ => {
            let mut roles = p.roles.clone();
            roles.sort();
            let run_as = p.authenticating.as_ref().map(|_| format!(" run as [{}]", p.username)).unwrap_or_default();
            let user = p.authenticating.as_ref().map_or(p.username.as_str(), |a| a.username.as_str());
            format!("user [{user}]{run_as} with effective roles [{}]", roles.join(","))
        }
    }
}

/// Elasticsearch's 403 for an action the principal's roles don't grant.
fn denied(p: &Principal, action: &str, indices: Option<&[String]>) -> EsError {
    if let Some(id) = p.os() {
        return opensearch::denied(p, id, action);
    }
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
    let mut exprs: Vec<String> = target_expressions(ctx);
    let api = ctx.api.name();
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

/// The expressions naming a request's targets: its `{index}`, the `{name}` of the data
/// stream APIs, or the `{alias}` (or data stream) a rollover rolls over.
fn target_expressions(ctx: &Ctx) -> Vec<String> {
    match ctx.api.name() {
        api if DATA_STREAM_APIS.contains(&api) => ctx.param("name").map(crate::meta::resolve::split_expressions).unwrap_or_default(),
        "indices.rollover" => ctx.path_param("alias").map(|a| vec![a.to_string()]).unwrap_or_default(),
        "indices.resolve_index" => ctx.param("name").map(crate::meta::resolve::split_expressions).unwrap_or_default(),
        _ => crate::api::index_expressions(ctx),
    }
}

/// Data stream APIs whose `{name}` is the data streams they act on.
const DATA_STREAM_APIS: &[&str] = &[
    "indices.create_data_stream",
    "indices.delete_data_stream",
    "indices.get_data_stream",
    "indices.data_streams_stats",
    "indices.migrate_to_data_stream",
    "indices.promote_data_stream",
    "indices.get_data_lifecycle",
    "indices.put_data_lifecycle",
    "indices.delete_data_lifecycle",
    "indices.get_data_stream_options",
    "indices.put_data_stream_options",
    "indices.delete_data_stream_options",
    "indices.get_data_stream_settings",
    "indices.put_data_stream_settings",
    "indices.get_data_stream_mappings",
    "indices.put_data_stream_mappings",
];

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
