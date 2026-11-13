//! The native realm and role management: users (`_security/user`), roles (`_security/role`),
//! role mappings, application privileges, and the privilege introspection APIs
//! (`_security/user/_privileges`, `_has_privileges`, `_security/privilege/_builtin`).
//!
//! Users and roles are objects of the `security.*` families ([`crate::xpack::store`]); the
//! reserved users and roles are Elasticsearch 9.5.5's (captured from `GET _security/user` and
//! `GET _security/role`), and only their passwords and `enabled` flags can change.

use super::query::{self, QueryRequest};
use super::{Principal, privileges};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::store;
use std::collections::BTreeMap;
use std::sync::OnceLock;

pub const USERS: &str = "security.user";
pub const ROLES: &str = "security.role";
pub const MAPPINGS: &str = "security.role_mapping";
pub const PRIVILEGES: &str = "security.privilege";
/// Password and `enabled` overrides of reserved users.
pub const RESERVED: &str = "security.reserved_user";

/// Elasticsearch 9.5.5's reserved users, with their roles and metadata.
const RESERVED_USERS: &[(&str, &[&str], &str)] = &[
    ("elastic", &["superuser"], r#"{"_reserved":true}"#),
    ("kibana", &["kibana_system"], r#"{"_deprecated":true,"_reserved":true,"_deprecated_reason":"Please use the [kibana_system] user instead."}"#),
    ("kibana_system", &["kibana_system"], r#"{"_reserved":true}"#),
    ("logstash_system", &["logstash_system"], r#"{"_reserved":true}"#),
    ("beats_system", &["beats_system"], r#"{"_reserved":true}"#),
    ("apm_system", &["apm_system"], r#"{"_reserved":true}"#),
    ("remote_monitoring_user", &["remote_monitoring_collector", "remote_monitoring_agent"], r#"{"_reserved":true}"#),
];

fn reserved_user(name: &str) -> Option<Value> {
    let (n, roles, meta) = RESERVED_USERS.iter().find(|(n, _, _)| *n == name)?;
    Some(obj! {
        "username" => *n,
        "roles" => Value::Arr(roles.iter().map(|r| Value::from(*r)).collect()),
        "full_name" => Value::Null,
        "email" => Value::Null,
        "metadata" => crate::json::parse(meta.as_bytes()).ok().flatten().unwrap_or(Value::Null),
        "enabled" => true,
    })
}

/// The reserved roles by name.
pub fn reserved_roles() -> &'static BTreeMap<String, Value> {
    static R: OnceLock<BTreeMap<String, Value>> = OnceLock::new();
    R.get_or_init(|| match crate::json::parse(include_bytes!("reserved_roles.json")) {
        Ok(Some(Value::Obj(m))) => m.into_iter().collect(),
        _ => BTreeMap::new(),
    })
}

/// Users of the file realm, from the node setting `roll.security.file_users`
/// (`name:password:role|role,...`; Elasticsearch's test clusters define their superuser this
/// way).
fn file_user(cluster: &Cluster, name: &str) -> Option<(String, Vec<String>)> {
    let spec = cluster.config.node_setting("roll.security.file_users")?;
    for entry in spec.split(',') {
        let mut it = entry.splitn(3, ':');
        let (Some(u), Some(p)) = (it.next(), it.next()) else { continue };
        if u == name {
            let roles = it.next().map(|r| r.split('|').filter(|s| !s.is_empty()).map(str::to_string).collect()).unwrap_or_default();
            return Some((p.to_string(), roles));
        }
    }
    None
}

fn strs(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Arr(a)) => a.iter().map(Value::to_es_string).collect(),
        Some(Value::Str(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn principal_of(user: &Value, realm_name: &'static str, realm_type: &'static str) -> Principal {
    Principal {
        username: user.get("username").map(Value::to_es_string).unwrap_or_default(),
        roles: strs(user.get("roles")),
        full_name: user.get("full_name").cloned().unwrap_or(Value::Null),
        email: user.get("email").cloned().unwrap_or(Value::Null),
        metadata: user.get("metadata").cloned().unwrap_or_else(Value::object),
        realm_name,
        realm_type,
        kind: super::AuthKind::Realm,
    }
}

/// Checks a password against the cache, then the stored hash.
fn check_password(cluster: &Cluster, user: &str, password: &str, stored: &str) -> bool {
    check_password_with(cluster, user, password, stored, super::verify_password)
}

/// [`check_password`] with a verifier of the stored hash's format.
pub(super) fn check_password_with(cluster: &Cluster, user: &str, password: &str, stored: &str, verify: fn(&str, &str) -> bool) -> bool {
    let key = super::digest(&[user.as_bytes(), password.as_bytes(), stored.as_bytes()]);
    if cluster.xpack.verified.lock().contains(&key) {
        return true;
    }
    if !verify(password, stored) {
        return false;
    }
    let mut cache = cluster.xpack.verified.lock();
    if cache.len() >= 10_000 {
        cache.clear();
    }
    cache.insert(key);
    true
}

/// Whether `name` is a reserved user (`elastic`, `kibana_system`, ...).
pub fn is_reserved(name: &str) -> bool {
    RESERVED_USERS.iter().any(|(n, _, _)| *n == name)
}

/// Whether user `name` has a password of its own: a file-realm user, or a reserved user whose
/// password was set with the change password API.
pub fn has_own_password(cluster: &Cluster, name: &str) -> bool {
    file_user(cluster, name).is_some() || cluster.state().metadata.xpack.get(RESERVED, name).and_then(|o| o.get("password_hash")).is_some()
}

/// Authenticates `user:password` against the file, native and reserved realms.
pub fn authenticate_password(cluster: &Cluster, user: &str, password: &str) -> Option<Principal> {
    if let Some((pw, roles)) = file_user(cluster, user) {
        return super::ct_eq(pw.as_bytes(), password.as_bytes()).then(|| {
            let u = obj! { "username" => user, "roles" => Value::Arr(roles.into_iter().map(Value::from).collect()), "metadata" => obj! {} };
            principal_of(&u, "default_file", "file")
        });
    }
    let st = cluster.state();
    if let Some(r) = reserved_user(user) {
        let over = st.metadata.xpack.get(RESERVED, user);
        if over.and_then(|o| o.get("enabled")).and_then(Value::as_bool) == Some(false) {
            return None;
        }
        // Without a password of its own, `elastic` accepts any (a dev cluster's owner; nodes
        // that authenticate tenants verify the cluster's credentials before this runs).
        let ok = match over.and_then(|o| o.get("password_hash")).and_then(Value::as_str) {
            Some(h) => check_password(cluster, user, password, h),
            None => user == "elastic",
        };
        return ok.then(|| principal_of(&r, "reserved", "reserved"));
    }
    let u = st.metadata.xpack.get(USERS, user)?;
    if u.get("enabled").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let hash = u.get("password_hash").and_then(Value::as_str)?;
    check_password(cluster, user, password, hash).then(|| principal_of(u, "default_native", "native"))
}

/// The enabled user `name` of the file, reserved or native realm, without its credentials
/// (the target of a run-as).
pub fn lookup_user(cluster: &Cluster, name: &str) -> Option<Principal> {
    if let Some((_, roles)) = file_user(cluster, name) {
        let u = obj! { "username" => name, "roles" => Value::Arr(roles.into_iter().map(Value::from).collect()), "metadata" => obj! {} };
        return Some(principal_of(&u, "default_file", "file"));
    }
    let st = cluster.state();
    if let Some(r) = reserved_user(name) {
        let enabled = st.metadata.xpack.get(RESERVED, name).and_then(|o| o.get("enabled")).and_then(Value::as_bool) != Some(false);
        return enabled.then(|| principal_of(&r, "reserved", "reserved"));
    }
    let u = st.metadata.xpack.get(USERS, name)?;
    (u.get("enabled").and_then(Value::as_bool) != Some(false)).then(|| principal_of(u, "default_native", "native"))
}

/// A user as `GET _security/user` prints it.
fn user_view(u: &Value, profile_uid: Option<Value>) -> Value {
    let mut out = Map::new();
    for k in ["username", "roles", "full_name", "email", "metadata", "enabled"] {
        out.insert(k.into(), u.get(k).cloned().unwrap_or(Value::Null));
    }
    if let Some(p) = profile_uid {
        out.insert("profile_uid".into(), p);
    }
    Value::Obj(out)
}

fn validation(errors: &[&str]) -> EsError {
    let mut s = String::from("Validation Failed: ");
    for (i, e) in errors.iter().enumerate() {
        s.push_str(&format!("{}: {e};", i + 1));
    }
    EsError::new(400, "validation_exception", s)
}

fn check_password_len(p: &str) -> EsResult<()> {
    if p.chars().count() < 6 {
        return Err(validation(&["passwords must be at least [6] characters long"]));
    }
    Ok(())
}

/// `Validation.Users.validateUsername`.
fn validate_username(name: &str) -> EsResult<()> {
    let ok = !name.is_empty()
        && name.chars().count() <= 507
        && !name.starts_with(char::is_whitespace)
        && !name.ends_with(char::is_whitespace)
        && name.chars().all(|c| (' '..='~').contains(&c) || c.is_alphanumeric());
    if !ok {
        return Err(EsError::action_request_validation(&[
            "Invalid username. Valid user names must be at least 1 characters and no longer than 507 characters. They can contain alphanumeric characters (a-z, A-Z, 0-9), spaces, punctuation, and printable symbols in the Basic Latin (ASCII) block. Leading or trailing whitespace is not allowed.".into(),
        ]));
    }
    Ok(())
}

pub async fn put_user(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("username").unwrap_or_default().to_string();
    let body = ctx.body()?.unwrap_or_else(Value::object);
    if reserved_user(&name).is_some() {
        return Err(EsError::action_request_validation(&[format!("user [{name}] is reserved and only the password can be changed")]));
    }
    validate_username(&name)?;
    let Some(m) = body.as_object() else { return Err(EsError::parse("failed to parse add user request")) };
    let mut password = None;
    let mut hash = None;
    for (k, v) in m {
        match k.as_str() {
            "password" => password = Some(v.to_es_string()),
            "password_hash" => hash = Some(v.to_es_string()),
            "username" => match v {
                Value::Str(u) if *u == name => {}
                Value::Str(_) => return Err(EsError::illegal_argument(format!("[username] in source does not match the username provided [{name}]"))),
                other => {
                    let kind = if matches!(other, Value::Num(_)) { "VALUE_NUMBER" } else { other.kind_name() };
                    return Err(EsError::parse(format!("expected field [username] to be of type string, but found [{kind}] instead")));
                }
            },
            "roles" | "full_name" | "email" | "metadata" | "enabled" => {}
            other => {
                return Err(EsError::new(400, "parse_exception", format!("failed to parse add user request. unexpected field [{other}]")));
            }
        }
    }
    if let Some(p) = &password {
        check_password_len(p)?;
    }
    if let Some(Value::Obj(meta)) = m.get("metadata")
        && meta.keys().any(|k| k.starts_with('_'))
    {
        return Err(EsError::action_request_validation(&["user metadata keys may not start with [_]".into()]));
    }
    let new_hash = match (password, hash) {
        (Some(p), _) => Some(super::hash_password(&p)),
        (None, Some(h)) => Some(accept_hash(h)?),
        (None, None) => None,
    };
    let roles = strs(m.get("roles"));
    let full_name = m.get("full_name").cloned().unwrap_or(Value::Null);
    let email = m.get("email").cloned().unwrap_or(Value::Null);
    let metadata = m.get("metadata").cloned().unwrap_or_else(Value::object);
    let enabled = m.get("enabled").and_then(Value::as_bool);
    let now = ctx.now_ms;
    let created = ctx
        .cluster
        .update(move |st| {
            let existing = st.metadata.xpack.get(USERS, &name).cloned();
            let hash = match (new_hash, existing.as_ref().and_then(|e| e.get("password_hash")).cloned()) {
                (Some(h), _) => Value::from(h),
                (None, Some(h)) => h,
                (None, None) => return Err(validation(&["password must be specified unless you are updating an existing user"])),
            };
            let enabled = enabled.or_else(|| existing.as_ref().and_then(|e| e.get("enabled")).and_then(Value::as_bool)).unwrap_or(true);
            let u = obj! {
                "username" => name.clone(),
                "roles" => Value::Arr(roles.iter().map(Value::from).collect()),
                "full_name" => full_name,
                "email" => email,
                "metadata" => metadata,
                "enabled" => enabled,
                "password_hash" => hash,
            };
            store::put(st, USERS, &name, u, now)
        })
        .await?;
    Reply::ok(obj! { "created" => created })
}

pub fn get_user(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let with_uid = ctx.bool_param("with_profile_uid", false)?;
    let uid_of = |name: &str| -> Option<Value> { with_uid.then(|| super::tokens::profile_uid_of(&st, name).map(Value::from)).flatten() };
    let mut out = Map::new();
    match ctx.list_param("username") {
        Some(names) if !names.is_empty() => {
            for n in &names {
                if let Some(u) = reserved_user(n) {
                    out.insert(n.clone(), user_view(&reserved_with_overrides(&st, n, u), uid_of(n)));
                } else if let Some(u) = st.metadata.xpack.get(USERS, n) {
                    out.insert(n.clone(), user_view(u, uid_of(n)));
                }
            }
            if out.is_empty() {
                return Ok(Reply::Body(404, Value::Obj(out)));
            }
        }
        _ => {
            for (n, _, _) in RESERVED_USERS {
                if let Some(u) = reserved_user(n) {
                    out.insert((*n).to_string(), user_view(&reserved_with_overrides(&st, n, u), uid_of(n)));
                }
            }
            for (n, u) in st.metadata.xpack.iter(USERS) {
                out.insert(n.clone(), user_view(u, uid_of(n)));
            }
        }
    }
    Reply::ok(Value::Obj(out))
}

fn reserved_with_overrides(st: &crate::meta::ClusterState, name: &str, mut u: Value) -> Value {
    if let Some(e) = st.metadata.xpack.get(RESERVED, name).and_then(|o| o.get("enabled")).cloned()
        && let Value::Obj(m) = &mut u
    {
        m.insert("enabled".into(), e);
    }
    u
}

pub async fn delete_user(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("username").unwrap_or_default().to_string();
    if reserved_user(&name).is_some() {
        return Err(EsError::action_request_validation(&[format!("user [{name}] is reserved and cannot be deleted")]));
    }
    let found = ctx.cluster.update(move |st| Ok(store::remove(st, USERS, &name).is_some())).await?;
    Ok(Reply::Body(if found { 200 } else { 404 }, obj! { "found" => found }))
}

pub async fn set_enabled(ctx: &Ctx<'_>, enabled: bool) -> EsResult<Reply> {
    let name = ctx.path_param("username").unwrap_or_default().to_string();
    let me = super::principal(ctx).username.clone();
    if !enabled && name == me {
        return Err(validation(&["users may not update the enabled status of their own account"]));
    }
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            if reserved_user(&name).is_some() {
                let mut o = st.metadata.xpack.get(RESERVED, &name).cloned().unwrap_or_else(Value::object);
                if let Value::Obj(m) = &mut o {
                    m.insert("enabled".into(), enabled.into());
                }
                return store::put(st, RESERVED, &name, o, now).map(|_| ());
            }
            let Some(mut u) = st.metadata.xpack.get(USERS, &name).cloned() else {
                return Err(validation(&[if enabled { "only existing users can be enabled" } else { "only existing users can be disabled" }]));
            };
            if let Value::Obj(m) = &mut u {
                m.insert("enabled".into(), enabled.into());
            }
            store::put(st, USERS, &name, u, now).map(|_| ())
        })
        .await?;
    Reply::ok(obj! {})
}

/// A client's `password_hash`: one of the hash formats roll verifies (`{PBKDF2}`), never
/// anything else (a clear text password sent as a hash included).
fn accept_hash(h: String) -> EsResult<String> {
    if h.starts_with("{PBKDF2}") {
        return Ok(h);
    }
    Err(EsError::illegal_argument("The provided password hash is not a hash or it could not be resolved to a supported hash algorithm. The supported password hash algorithms are [pbkdf2]"))
}

pub async fn change_password(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = match ctx.path_param("username") {
        Some(n) => n.to_string(),
        None => super::principal(ctx).username.clone(),
    };
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let hash = match (body.get("password"), body.get("password_hash")) {
        (Some(p), _) => {
            let p = p.to_es_string();
            check_password_len(&p)?;
            super::hash_password(&p)
        }
        (None, Some(h)) => accept_hash(h.to_es_string())?,
        (None, None) => return Err(validation(&["password must be specified"])),
    };
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let family = if reserved_user(&name).is_some() { RESERVED } else { USERS };
            let mut u = match st.metadata.xpack.get(family, &name).cloned() {
                Some(u) => u,
                None if family == RESERVED => Value::object(),
                None => return Err(validation(&["user must exist in order to change password"])),
            };
            if let Value::Obj(m) = &mut u {
                m.insert("password_hash".into(), hash.into());
            }
            store::put(st, family, &name, u, now).map(|_| ())
        })
        .await?;
    Reply::ok(obj! {})
}

fn user_fields(u: &Value, f: &str) -> Vec<Value> {
    match f {
        "username" | "roles" | "full_name" | "email" | "enabled" => query::path_values(u, f),
        _ if f.starts_with("metadata.") => query::path_values(u, f),
        "*" => query::path_values(u, "*"),
        _ => Vec::new(),
    }
}

pub fn query_user(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body()?;
    let req = QueryRequest::parse(body.as_ref(), "query_user_request")?;
    let st = ctx.state();
    let with_uid = ctx.bool_param("with_profile_uid", false)?;
    let objs: Vec<(String, Value)> = st.metadata.xpack.iter(USERS).map(|(k, v)| (k.clone(), v.clone())).collect();
    let (total, page) = query::run(&req, objs, &user_fields)?;
    let sorted = !req.sort.is_empty();
    let hits = page
        .into_iter()
        .map(|(u, keys)| {
            let name = u.get("username").map(Value::to_es_string).unwrap_or_default();
            let uid = with_uid.then(|| super::tokens::profile_uid_of(&st, &name).map(Value::from)).flatten();
            query::with_sort(user_view(&u, uid), keys, sorted)
        })
        .collect();
    Reply::ok(query::response(total, "users", hits))
}

// --- roles -----------------------------------------------------------------------------------

/// Parses and validates a role descriptor into its stored (and printed) form.
pub fn parse_role(name: &str, v: &Value, api_key: bool) -> EsResult<Value> {
    let Some(m) = v.as_object() else { return Err(EsError::parse(format!("failed to parse role [{name}]. expected an object but found [{}] instead", v.kind_name()))) };
    let mut errors: Vec<String> = Vec::new();
    let mut cluster = Vec::new();
    let mut indices = Vec::new();
    let mut applications = Vec::new();
    let mut run_as = Vec::new();
    let mut metadata = Value::object();
    let mut description = None;
    let mut remote_indices = None;
    let mut remote_cluster = None;
    let mut restriction = None;
    let mut global = None;
    for (k, val) in m {
        match k.as_str() {
            "cluster" => cluster = strs(Some(val)),
            "indices" | "index" => {
                for e in val.as_array().into_iter().flatten() {
                    let Some(em) = e.as_object() else { continue };
                    let mut out = Map::new();
                    for ek in em.keys() {
                        if !matches!(ek.as_str(), "names" | "privileges" | "field_security" | "query" | "allow_restricted_indices") {
                            return Err(EsError::parse(format!("failed to parse indices privileges for role [{name}]. unexpected field [{ek}]")));
                        }
                    }
                    let names = strs(em.get("names"));
                    if em.get("names").is_none() {
                        return Err(EsError::parse(format!("failed to parse indices privileges for role [{name}]. missing required [names] field")));
                    }
                    let privs = strs(em.get("privileges"));
                    for p in &privs {
                        if !privileges::is_index_privilege(p) {
                            errors.push(privileges::unknown_index_privilege(p));
                        }
                    }
                    out.insert("names".into(), Value::Arr(names.into_iter().map(Value::from).collect()));
                    out.insert("privileges".into(), Value::Arr(privs.into_iter().map(Value::from).collect()));
                    if let Some(fs) = em.get("field_security") {
                        out.insert("field_security".into(), fs.clone());
                    }
                    if let Some(q) = em.get("query") {
                        let text = match q {
                            Value::Str(s) => s.clone(),
                            other => other.to_json(),
                        };
                        check_role_query(&text).map_err(|e| {
                            let names = strs(em.get("names")).join(",");
                            EsError::parse(format!("failed to parse field 'query' for indices [{names}] at index privilege [{}] of role descriptor", indices.len())).caused_by(e)
                        })?;
                        out.insert("query".into(), Value::from(text));
                    }
                    out.insert("allow_restricted_indices".into(), em.get("allow_restricted_indices").and_then(Value::as_bool).unwrap_or(false).into());
                    indices.push(Value::Obj(out));
                }
            }
            "applications" => {
                for e in val.as_array().into_iter().flatten() {
                    applications.push(obj! {
                        "application" => e.get("application").cloned().unwrap_or(Value::Null),
                        "privileges" => Value::Arr(strs(e.get("privileges")).into_iter().map(Value::from).collect()),
                        "resources" => Value::Arr(strs(e.get("resources")).into_iter().map(Value::from).collect()),
                    });
                }
            }
            "run_as" => run_as = strs(Some(val)),
            "metadata" => {
                if let Value::Obj(mm) = val
                    && mm.keys().any(|k| k.starts_with('_'))
                {
                    errors.push("role metadata keys may not start with [_]".into());
                }
                metadata = val.clone();
            }
            "transient_metadata" => {}
            "description" => description = Some(val.clone()),
            "remote_indices" => remote_indices = Some(val.clone()),
            "remote_cluster" => remote_cluster = Some(val.clone()),
            "restriction" if api_key => restriction = Some(val.clone()),
            "global" => global = parse_global(val)?,
            other => return Err(EsError::parse(format!("failed to parse role [{name}]. unexpected field [{other}]"))),
        }
    }
    for c in &cluster {
        if !privileges::is_cluster_privilege(c) {
            errors.push(privileges::unknown_cluster_privilege(c));
        }
    }
    if !errors.is_empty() {
        return Err(EsError::action_request_validation(&errors));
    }
    let mut out = Map::new();
    out.insert("cluster".into(), Value::Arr(cluster.into_iter().map(Value::from).collect()));
    if let Some(g) = global {
        out.insert("global".into(), g);
    }
    out.insert("indices".into(), Value::Arr(indices));
    out.insert("applications".into(), Value::Arr(applications));
    out.insert("run_as".into(), Value::Arr(run_as.into_iter().map(Value::from).collect()));
    out.insert("metadata".into(), metadata);
    if let Some(r) = remote_indices {
        out.insert("remote_indices".into(), r);
    }
    if let Some(r) = remote_cluster {
        out.insert("remote_cluster".into(), r);
    }
    if let Some(r) = restriction {
        out.insert("restriction".into(), r);
    }
    out.insert("transient_metadata".into(), obj! { "enabled" => true });
    if let Some(d) = description {
        out.insert("description".into(), d);
    }
    Ok(Value::Obj(out))
}

/// `DLSRoleQueryValidator`: a role's query parses as a query (unless it is a template, which is
/// checked once rendered), and holds none of the queries that would read other documents than
/// the one being filtered: terms lookups, indexed shapes, `has_child`, `has_parent` and
/// `percolate` (in compound queries too).
fn check_role_query(text: &str) -> EsResult<()> {
    let Some(v) = crate::json::parse(text.as_bytes())? else { return Ok(()) };
    if v.get("template").is_some() {
        return Ok(());
    }
    crate::query::parse_query(&v, Some(text.as_bytes()), &[])?;
    fn walk(v: &Value) -> EsResult<()> {
        let Some((kind, body)) = v.as_object().and_then(|m| m.iter().next()) else { return Ok(()) };
        let refuse = |what: &str| Err(EsError::illegal_argument(format!("{what} query isn't supported as part of a role query")));
        let children = |keys: &[&str]| -> Vec<&Value> { keys.iter().filter_map(|k| body.get(k)).flat_map(|c| c.as_array().map(|a| a.iter().collect()).unwrap_or_else(|| vec![c])).collect() };
        let nested = match kind.as_str() {
            "terms" if body.as_object().is_some_and(|m| m.values().any(|f| f.as_object().is_some_and(|o| o.contains_key("index") || o.contains_key("id") || o.contains_key("path")))) => {
                return Err(EsError::illegal_argument("terms query with terms lookup isn't supported as part of a role query"));
            }
            "geo_shape" if body.as_object().is_some_and(|m| m.values().any(|f| f.get("indexed_shape").is_some())) => {
                return Err(EsError::illegal_argument("geoshape query referring to indexed shapes isn't supported as part of a role query"));
            }
            "has_child" | "has_parent" | "percolate" => return refuse(kind),
            "bool" => children(&["must", "should", "filter", "must_not"]),
            "dis_max" => children(&["queries"]),
            "constant_score" => children(&["filter"]),
            "function_score" => children(&["query"]),
            "boosting" => children(&["positive", "negative"]),
            _ => Vec::new(),
        };
        nested.into_iter().try_for_each(walk)
    }
    walk(&v)
}

/// A role's `global` (configurable cluster) privileges, stored as Elasticsearch prints them
/// (every category, empty ones included); `None` when it grants nothing.
fn parse_global(v: &Value) -> EsResult<Option<Value>> {
    const CATEGORIES: &str = "application,profile,role,data_source";
    let Some(m) = v.as_object() else { return Err(EsError::parse(format!("failed to parse privilege. expected [START_OBJECT] but found [{}] instead", v.kind_name()))) };
    let mut out = obj! { "application" => Value::object(), "profile" => Value::object(), "role" => Value::object(), "data_source" => Value::Arr(Vec::new()) };
    let mut any = false;
    for (k, c) in m {
        if !CATEGORIES.split(',').any(|x| x == k) {
            return Err(EsError::parse(format!("failed to parse privilege. expected one of [{CATEGORIES}] but found [{k}] instead")));
        }
        // `{"manage": {"applications": [...]}}` (application), `{"write": {"applications": [...]}}`
        // (profile): every list is an array of strings.
        if let Value::Obj(inner) = c {
            for p in inner.values() {
                for list in p.as_object().into_iter().flat_map(|o| o.values()) {
                    if !matches!(list, Value::Arr(a) if a.iter().all(|x| x.as_str().is_some())) {
                        return Err(EsError::parse(format!("failed to parse privilege. expected [START_ARRAY] but found [{}] instead", list.kind_name())));
                    }
                }
            }
            any |= !inner.is_empty();
        } else if let Value::Arr(a) = c {
            any |= !a.is_empty();
        }
        if let Value::Obj(o) = &mut out {
            o.insert(k.clone(), c.clone());
        }
    }
    Ok(any.then_some(out))
}

/// The applications an application privileges request (get, put, delete) names; none when it
/// names every application.
pub fn privilege_request_applications(ctx: &Ctx) -> Vec<String> {
    match ctx.api.name() {
        "security.put_privileges" => ctx.body().ok().flatten().and_then(|b| b.as_object().map(|m| m.keys().cloned().collect())).unwrap_or_default(),
        _ => ctx.path_param("application").map(|a| a.split(',').map(str::to_string).collect()).unwrap_or_default(),
    }
}

/// The applications whose privileges a role's `global.application.manage` lets it manage.
pub fn managed_applications(global: &Value) -> Vec<String> {
    global.get("application").and_then(|a| a.get("manage")).and_then(|m| m.get("applications")).and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default()
}

fn check_role_name(name: &str) -> EsResult<()> {
    if reserved_roles().contains_key(name) {
        return Err(EsError::action_request_validation(&[format!("Role [{name}] is reserved and may not be used.")]));
    }
    if name.is_empty() || name.chars().count() > 507 || name.starts_with(char::is_whitespace) || name.ends_with(char::is_whitespace) {
        return Err(EsError::action_request_validation(&[
            "Role names must be at least 1 and no more than 507 characters. They can contain alphanumeric characters (a-z, A-Z, 0-9), spaces, punctuation, and printable symbols in the Basic Latin (ASCII) block. Leading or trailing whitespace is not allowed.".into(),
        ]));
    }
    Ok(())
}

pub async fn put_role(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    check_role_name(&name)?;
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let role = parse_role(&name, &body, false)?;
    let now = ctx.now_ms;
    let created = ctx.cluster.update(move |st| store::put(st, ROLES, &name, role, now)).await?;
    Reply::ok(obj! { "role" => obj! { "created" => created } })
}

pub fn get_role(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let mut out = Map::new();
    match ctx.list_param("name") {
        Some(names) if !names.is_empty() => {
            for n in names {
                if let Some(r) = super::role_descriptor(&st, &n) {
                    out.insert(n, r);
                }
            }
            if out.is_empty() {
                return Ok(Reply::Body(404, Value::Obj(out)));
            }
        }
        _ => {
            let mut all: BTreeMap<String, Value> = reserved_roles().clone();
            for (k, v) in st.metadata.xpack.iter(ROLES) {
                all.insert(k.clone(), v.clone());
            }
            out.extend(all);
        }
    }
    Reply::ok(Value::Obj(out))
}

pub async fn delete_role(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    if reserved_roles().contains_key(&name) {
        return Err(EsError::illegal_argument(format!("role [{name}] is reserved and cannot be deleted")));
    }
    let found = ctx.cluster.update(move |st| Ok(store::remove(st, ROLES, &name).is_some())).await?;
    Ok(Reply::Body(if found { 200 } else { 404 }, obj! { "found" => found }))
}

pub async fn bulk_put_role(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let Some(Value::Obj(roles)) = body.get("roles") else {
        return Err(EsError::action_request_validation(&["roles is missing".into()]));
    };
    let mut parsed = Vec::new();
    let mut errors = Map::new();
    for (name, r) in roles {
        match check_role_name(name).and_then(|_| parse_role(name, r, false)) {
            Ok(v) => parsed.push((name.clone(), v)),
            Err(e) => {
                errors.insert(name.clone(), obj! { "type" => e.kind.to_string(), "reason" => e.reason().to_string() });
            }
        }
    }
    let now = ctx.now_ms;
    let (created, updated, noop) = ctx
        .cluster
        .update(move |st| {
            let (mut c, mut u, mut n) = (Vec::new(), Vec::new(), Vec::new());
            for (name, v) in parsed {
                match st.metadata.xpack.get(ROLES, &name) {
                    Some(old) if *old == v => n.push(name),
                    Some(_) => {
                        store::put(st, ROLES, &name, v, now)?;
                        u.push(name);
                    }
                    None => {
                        store::put(st, ROLES, &name, v, now)?;
                        c.push(name);
                    }
                }
            }
            Ok((c, u, n))
        })
        .await?;
    let mut out = Map::new();
    for (k, list) in [("created", created), ("updated", updated), ("noop", noop)] {
        if !list.is_empty() {
            out.insert(k.into(), Value::Arr(list.into_iter().map(Value::from).collect()));
        }
    }
    if !errors.is_empty() {
        out.insert("errors".into(), obj! { "count" => errors.len(), "details" => Value::Obj(errors) });
    }
    Reply::ok(Value::Obj(out))
}

pub async fn bulk_delete_role(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let names = strs(body.get("names"));
    let (deleted, not_found) = ctx
        .cluster
        .update(move |st| {
            let (mut d, mut nf) = (Vec::new(), Vec::new());
            for n in names {
                if store::remove(st, ROLES, &n).is_some() { d.push(n) } else { nf.push(n) }
            }
            Ok((d, nf))
        })
        .await?;
    let mut out = Map::new();
    if !deleted.is_empty() {
        out.insert("deleted".into(), Value::Arr(deleted.into_iter().map(Value::from).collect()));
    }
    if !not_found.is_empty() {
        out.insert("not_found".into(), Value::Arr(not_found.into_iter().map(Value::from).collect()));
    }
    Reply::ok(Value::Obj(out))
}

fn role_fields(r: &Value, f: &str) -> Vec<Value> {
    match f {
        "name" | "description" | "cluster" | "run_as" => query::path_values(r, f),
        "applications.application" | "applications.resources" | "applications.privileges" => query::path_values(r, f),
        _ if f.starts_with("metadata.") => query::path_values(r, f),
        "*" => query::path_values(r, "*"),
        _ => Vec::new(),
    }
}

pub fn query_role(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body()?;
    let req = QueryRequest::parse(body.as_ref(), "query_role_request")?;
    let st = ctx.state();
    let mut all: BTreeMap<String, Value> = reserved_roles().clone();
    for (k, v) in st.metadata.xpack.iter(ROLES) {
        all.insert(k.clone(), v.clone());
    }
    let objs = all
        .into_iter()
        .map(|(k, v)| {
            let mut m = Map::new();
            m.insert("name".into(), Value::from(k.clone()));
            if let Value::Obj(vm) = v {
                m.extend(vm);
            }
            (k, Value::Obj(m))
        })
        .collect();
    let (total, page) = query::run(&req, objs, &role_fields)?;
    let sorted = !req.sort.is_empty();
    Reply::ok(query::response(total, "roles", page.into_iter().map(|(r, k)| query::with_sort(r, k, sorted)).collect()))
}

// --- role mappings ---------------------------------------------------------------------------

/// Line and column just past the end of a JSON body (where a missing field is reported).
fn end_location(raw: &[u8]) -> (usize, usize) {
    let trimmed = raw.iter().rposition(|b| !b.is_ascii_whitespace()).map_or(0, |p| p + 1);
    let mut line = 1usize;
    let mut col = 1usize;
    for b in &raw[..trimmed] {
        if *b == b'\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col.saturating_sub(1).max(1))
}

pub async fn put_role_mapping(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let Some(m) = body.as_object() else { return Err(EsError::parse(format!("failed to parse role-mapping [{name}]"))) };
    for k in m.keys() {
        if !matches!(k.as_str(), "enabled" | "roles" | "role_templates" | "rules" | "metadata") {
            return Err(EsError::parse(format!("failed to parse role-mapping [{name}]. unexpected field [{k}]")));
        }
    }
    let missing = |field: &str| {
        let (line, col) = end_location(ctx.raw_json().unwrap_or(b"{}"));
        let msg = format!("failed to parse role-mapping [{name}]. missing field [{field}]");
        EsError::parsing(line, col, msg.clone()).caused_by(EsError::illegal_state(msg))
    };
    let Some(rules) = m.get("rules") else { return Err(missing("rules")) };
    if m.get("roles").is_none() && m.get("role_templates").is_none() {
        return Err(missing("roles"));
    }
    let mut out = Map::new();
    out.insert("enabled".into(), m.get("enabled").and_then(Value::as_bool).unwrap_or(true).into());
    if let Some(r) = m.get("roles") {
        out.insert("roles".into(), Value::Arr(strs(Some(r)).into_iter().map(Value::from).collect()));
    } else {
        out.insert("roles".into(), Value::Arr(Vec::new()));
    }
    if let Some(t) = m.get("role_templates") {
        out.insert("role_templates".into(), t.clone());
    }
    out.insert("rules".into(), rules.clone());
    out.insert("metadata".into(), m.get("metadata").cloned().unwrap_or_else(Value::object));
    let v = Value::Obj(out);
    let now = ctx.now_ms;
    let created = ctx.cluster.update(move |st| store::put(st, MAPPINGS, &name, v, now)).await?;
    Reply::ok(obj! { "role_mapping" => obj! { "created" => created } })
}

pub fn get_role_mapping(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let mut out = Map::new();
    match ctx.list_param("name") {
        Some(names) if !names.is_empty() => {
            for n in names {
                if let Some(r) = st.metadata.xpack.get(MAPPINGS, &n) {
                    out.insert(n, r.clone());
                }
            }
            if out.is_empty() {
                return Ok(Reply::Body(404, Value::Obj(out)));
            }
        }
        _ => {
            for (k, v) in st.metadata.xpack.iter(MAPPINGS) {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Reply::ok(Value::Obj(out))
}

pub async fn delete_role_mapping(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    let found = ctx.cluster.update(move |st| Ok(store::remove(st, MAPPINGS, &name).is_some())).await?;
    Ok(Reply::Body(if found { 200 } else { 404 }, obj! { "found" => found }))
}

// --- application privileges ------------------------------------------------------------------

pub fn privilege_key(app: &str, name: &str) -> String {
    format!("{app}:{name}")
}

fn valid_privilege_name(n: &str) -> bool {
    let mut c = n.chars();
    c.next().is_some_and(|f| f.is_ascii_lowercase()) && c.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
}

pub async fn put_privileges(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let Some(apps) = body.as_object() else { return Err(EsError::parse("failed to parse privileges")) };
    let mut errors = Vec::new();
    let mut items = Vec::new();
    for (app, privs) in apps {
        for (name, p) in privs.as_object().into_iter().flatten() {
            if let Some(a) = p.get("application").and_then(Value::as_str)
                && a != app
            {
                errors.push(format!("Application privilege [{name}] in application [{app}] has an inconsistent application [{a}]"));
            }
            if !valid_privilege_name(name) {
                errors.push(format!("Application privilege names must match the pattern ^[a-z][a-zA-Z0-9_.-]*$ (found '{name}')"));
            }
            let actions = strs(p.get("actions"));
            if actions.is_empty() {
                errors.push("Application privileges must have at least one action".into());
            }
            for a in &actions {
                if !a.contains('/') && !a.contains('*') && !a.contains(':') {
                    errors.push(format!("action [{a}] must contain one of [ '/' , '*' , ':' ]"));
                }
            }
            items.push((app.clone(), name.clone(), obj! {
                "application" => app.clone(),
                "name" => name.clone(),
                "actions" => Value::Arr(actions.into_iter().map(Value::from).collect()),
                "metadata" => p.get("metadata").cloned().unwrap_or_else(Value::object),
            }));
        }
    }
    if !errors.is_empty() {
        return Err(EsError::action_request_validation(&errors));
    }
    let now = ctx.now_ms;
    let results = ctx
        .cluster
        .update(move |st| {
            let mut out = Vec::new();
            for (app, name, v) in items {
                let created = store::put(st, PRIVILEGES, &privilege_key(&app, &name), v, now)?;
                out.push((app, name, created));
            }
            Ok(out)
        })
        .await?;
    let mut out = Map::new();
    for (app, name, created) in results {
        let e = out.entry(app).or_insert_with(Value::object);
        if let Value::Obj(m) = e {
            m.insert(name, obj! { "created" => created });
        }
    }
    Reply::ok(Value::Obj(out))
}

pub fn get_privileges(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let app = ctx.path_param("application").map(str::to_string);
    let names = ctx.list_param("name");
    let mut by_app: BTreeMap<String, Map> = BTreeMap::new();
    for (_, v) in st.metadata.xpack.iter(PRIVILEGES) {
        let a = v.get("application").map(Value::to_es_string).unwrap_or_default();
        let n = v.get("name").map(Value::to_es_string).unwrap_or_default();
        if app.as_deref().is_some_and(|want| !privileges::glob(want, &a)) {
            continue;
        }
        if let Some(ns) = &names
            && !ns.iter().any(|x| x == &n || x == "*")
        {
            continue;
        }
        by_app.entry(a).or_default().insert(n, v.clone());
    }
    let empty = by_app.is_empty();
    let out: Map = by_app.into_iter().map(|(a, m)| (a, Value::Obj(crate::json::java_hash_map(m, 16)))).collect();
    Ok(Reply::Body(if empty && (app.is_some() || names.is_some()) { 404 } else { 200 }, Value::Obj(out)))
}

pub async fn delete_privileges(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let app = ctx.path_param("application").unwrap_or_default().to_string();
    let mut names = ctx.list_param("name").unwrap_or_default();
    names.sort();
    let found = ctx
        .cluster
        .update(move |st| Ok(names.into_iter().map(|n| { let f = store::remove(st, PRIVILEGES, &privilege_key(&app, &n)).is_some(); (n, f) }).collect::<Vec<_>>()))
        .await?;
    let app = ctx.path_param("application").unwrap_or_default().to_string();
    let any = found.iter().any(|(_, f)| *f);
    let inner: Map = found.into_iter().map(|(n, f)| (n, obj! { "found" => f })).collect();
    let mut out = Map::new();
    out.insert(app, Value::Obj(inner));
    Ok(Reply::Body(if any { 200 } else { 404 }, Value::Obj(out)))
}

pub fn builtin_privileges(_ctx: &Ctx) -> EsResult<Reply> {
    Reply::ok(crate::json::parse(include_bytes!("builtin_privileges.json")).ok().flatten().unwrap_or_else(Value::object))
}

/// `GET _security/user/_privileges`.
pub fn get_user_privileges(ctx: &Ctx) -> EsResult<Reply> {
    let p = super::principal(ctx);
    if p.is_superuser() {
        return Reply::ok(crate::json::parse(SUPERUSER_PRIVILEGES.as_bytes()).ok().flatten().unwrap_or_else(Value::object));
    }
    let st = ctx.state();
    let eff = super::effective(&super::file_roles(ctx.cluster), &st, p);
    let mut cluster: Vec<String> = eff.own.iter().flat_map(|r| r.cluster.iter().cloned()).collect();
    cluster.sort();
    cluster.dedup();
    let mut run_as: Vec<String> = eff.own.iter().flat_map(|r| r.run_as.iter().cloned()).collect();
    run_as.sort();
    run_as.dedup();
    // The roles merged into one as `CompositeRolesStore` builds it: index entries by their
    // set of names (and restricted access), privileges and field and document security
    // collected; application privileges by application and privilege (resources collected),
    // then shown by application and resources.
    let strs_of = |v: &[String]| Value::Arr(v.iter().map(Value::from).collect());
    #[derive(Default)]
    struct Merged {
        names: Vec<String>,
        restricted: bool,
        privileges: Vec<String>,
        field_security: Vec<Value>,
        queries: Vec<String>,
    }
    let mut groups: Vec<Merged> = Vec::new();
    for g in eff.own.iter().flat_map(|r| r.indices.iter()) {
        let mut names = g.names.clone();
        names.sort();
        names.dedup();
        let i = match groups.iter().position(|m| m.names == names && m.restricted == g.allow_restricted) {
            Some(i) => i,
            None => {
                groups.push(Merged { names, restricted: g.allow_restricted, ..Default::default() });
                groups.len() - 1
            }
        };
        let e = &mut groups[i];
        e.privileges.extend(g.privileges.iter().cloned());
        e.field_security.extend(g.field_security.iter().cloned());
        if let Some(q) = &g.query {
            let q = q.as_str().map(str::to_string).unwrap_or_else(|| q.to_json());
            if !e.queries.contains(&q) {
                e.queries.push(q);
            }
        }
    }
    let indices: Vec<Value> = groups
        .into_iter()
        .map(|mut g| {
            g.privileges.sort();
            g.privileges.dedup();
            let mut m = Map::new();
            m.insert("names".into(), strs_of(&g.names));
            m.insert("privileges".into(), strs_of(&g.privileges));
            if !g.field_security.is_empty() {
                m.insert("field_security".into(), Value::Arr(g.field_security));
            }
            if !g.queries.is_empty() {
                m.insert("query".into(), strs_of(&g.queries));
            }
            m.insert("allow_restricted_indices".into(), g.restricted.into());
            Value::Obj(m)
        })
        .collect();
    let mut by_privilege: Vec<(String, String, Vec<String>)> = Vec::new();
    for (a, privs, res) in eff.own.iter().flat_map(|r| r.applications.iter()) {
        for pr in privs {
            match by_privilege.iter_mut().find(|(x, y, _)| x == a && y == pr) {
                Some((_, _, r)) => r.extend(res.iter().cloned()),
                None => by_privilege.push((a.clone(), pr.clone(), res.clone())),
            }
        }
    }
    let mut by_resources: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();
    for (a, pr, mut res) in by_privilege {
        res.sort();
        res.dedup();
        match by_resources.iter_mut().find(|(x, r, _)| *x == a && *r == res) {
            Some((_, _, p)) => p.push(pr),
            None => by_resources.push((a, res, vec![pr])),
        }
    }
    let applications: Vec<Value> = by_resources
        .into_iter()
        .map(|(a, res, mut privs)| {
            privs.sort();
            privs.dedup();
            obj! { "application" => a, "privileges" => strs_of(&privs), "resources" => strs_of(&res) }
        })
        .collect();
    // Each configurable privilege a role grants, as its own object.
    let global: Vec<Value> = eff
        .own
        .iter()
        .filter_map(|r| r.global.as_ref())
        .flat_map(|g| {
            ["application", "profile", "role"].into_iter().filter_map(move |k| g.get(k).filter(|c| c.as_object().is_some_and(|m| !m.is_empty())).map(|c| obj! { k => c.clone() }))
        })
        .collect();
    let mut out = obj! {
        "cluster" => Value::Arr(cluster.into_iter().map(Value::from).collect()),
        "global" => Value::Arr(global),
        "indices" => Value::Arr(indices),
        "applications" => Value::Arr(applications),
        "run_as" => Value::Arr(run_as.into_iter().map(Value::from).collect()),
    };
    let remote_indices: Vec<Value> = eff
        .own
        .iter()
        .flat_map(|r| r.remote_indices.iter())
        .map(|e| {
            let mut m = Map::new();
            for k in ["names", "privileges", "field_security", "query"] {
                if let Some(v) = e.get(k) {
                    m.insert(k.into(), v.clone());
                }
            }
            m.insert("allow_restricted_indices".into(), e.get("allow_restricted_indices").and_then(Value::as_bool).unwrap_or(false).into());
            m.insert("clusters".into(), e.get("clusters").cloned().unwrap_or_else(|| Value::Arr(Vec::new())));
            Value::Obj(m)
        })
        .collect();
    let remote_cluster: Vec<Value> = eff.own.iter().flat_map(|r| r.remote_cluster.iter().cloned()).collect();
    if let Value::Obj(m) = &mut out {
        if !remote_indices.is_empty() {
            m.insert("remote_indices".into(), Value::Arr(remote_indices));
        }
        if !remote_cluster.is_empty() {
            m.insert("remote_cluster".into(), Value::Arr(remote_cluster));
        }
    }
    Reply::ok(out)
}

const SUPERUSER_PRIVILEGES: &str = r#"{"cluster":["all"],"global":[],"indices":[{"names":["*"],"privileges":["all"],"allow_restricted_indices":false},{"names":["*"],"privileges":["monitor","read","read_cross_cluster","view_index_metadata"],"allow_restricted_indices":true}],"applications":[{"application":"*","privileges":["*"],"resources":["*"]}],"run_as":["*"],"remote_indices":[{"names":["*"],"privileges":["all"],"allow_restricted_indices":false,"clusters":["*"]},{"names":["*"],"privileges":["monitor","read","read_cross_cluster","view_index_metadata"],"allow_restricted_indices":true,"clusters":["*"]}],"remote_cluster":[{"privileges":["monitor_enrich","monitor_stats"],"clusters":["*"]}]}"#;

/// The privilege checks of `_has_privileges` (also used for user profiles): `(all granted,
/// cluster, index, application)`.
pub fn check_privileges(file: &super::FileRoles, st: &crate::meta::ClusterState, p: &Principal, body: &Value) -> EsResult<(bool, Value, Value, Value)> {
    let superuser = p.is_superuser();
    let eff = super::effective(file, st, p);
    let mut all = true;
    let mut cluster = Map::new();
    for c in strs(body.get("cluster")) {
        if !privileges::is_cluster_privilege(&c) {
            return Err(EsError::illegal_argument(privileges::unknown_cluster_privilege(&c)));
        }
        let ok = superuser || eff.has_cluster_privilege(&c);
        all &= ok;
        cluster.insert(c, ok.into());
    }
    let mut index = Map::new();
    for e in body.get("index").and_then(Value::as_array).into_iter().flatten() {
        let privs = strs(e.get("privileges"));
        for p in &privs {
            if !privileges::is_index_privilege(p) {
                return Err(EsError::illegal_argument(privileges::unknown_index_privilege(p)));
            }
        }
        for n in strs(e.get("names")) {
            let entry = index.entry(n.clone()).or_insert_with(Value::object);
            for pr in &privs {
                let ok = superuser || eff.has_index_privilege(pr, &n);
                all &= ok;
                if let Value::Obj(m) = entry {
                    m.insert(pr.clone(), ok.into());
                }
            }
        }
    }
    let mut application = Map::new();
    for e in body.get("application").and_then(Value::as_array).into_iter().flatten() {
        let app = e.get("application").map(Value::to_es_string).unwrap_or_default();
        let privs = strs(e.get("privileges"));
        let entry = application.entry(app.clone()).or_insert_with(Value::object);
        for r in strs(e.get("resources")) {
            let mut rm = Map::new();
            for pr in &privs {
                let ok = superuser || eff.has_application_privilege(st, &app, pr, &r);
                all &= ok;
                rm.insert(pr.clone(), ok.into());
            }
            if let Value::Obj(m) = entry {
                m.insert(r, Value::Obj(rm));
            }
        }
    }
    Ok((all, Value::Obj(cluster), Value::Obj(index), Value::Obj(application)))
}

pub fn has_privileges(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let p = super::principal(ctx);
    let st = ctx.state();
    let (all, cluster, index, application) = check_privileges(&super::file_roles(ctx.cluster), &st, p, &body)?;
    Reply::ok(obj! { "username" => p.username.clone(), "has_all_requested" => all, "cluster" => cluster, "index" => index, "application" => application })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_parse_and_validate() {
        let r = parse_role("r", &obj! { "indices" => crate::arr![obj! { "names" => "*", "privileges" => crate::arr!["all"] }], "metadata" => obj! { "k" => 1 } }, false).unwrap();
        assert_eq!(
            r.to_json(),
            r#"{"cluster":[],"indices":[{"names":["*"],"privileges":["all"],"allow_restricted_indices":false}],"applications":[],"run_as":[],"metadata":{"k":1},"transient_metadata":{"enabled":true}}"#
        );
        let e = parse_role("cxbad", &obj! { "foo" => 1 }, false).unwrap_err();
        assert_eq!(e.reason(), "failed to parse role [cxbad]. unexpected field [foo]");
        let e = parse_role("r", &obj! { "cluster" => crate::arr!["bogus"] }, false).unwrap_err();
        assert!(e.reason().starts_with("Validation Failed: 1: unknown cluster privilege [bogus]. a privilege must be either one of the predefined cluster privilege names [manage_own_api_key,"));
        assert_eq!(e.kind, "action_request_validation_exception");
    }

    #[test]
    fn reserved_data_loads() {
        assert!(reserved_roles().contains_key("superuser"));
        assert!(reserved_roles().contains_key("kibana_user"));
        assert_eq!(reserved_roles().len(), 29);
        assert!(reserved_user("elastic").is_some());
        assert!(valid_privilege_name("p1"));
        assert!(!valid_privilege_name("P1"));
    }

    #[test]
    fn end_location_points_past_the_body() {
        assert_eq!(end_location(br#"{"roles":["a"]}"#), (1, 15));
        assert_eq!(end_location(b"{\n}\n"), (2, 1));
    }
}
